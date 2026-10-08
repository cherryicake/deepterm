//! mermaid 图实时渲染：` ```mermaid ` 代码卡片默认画成真实图形（flowchart /
//! sequenceDiagram / gantt / pie … 全类型），卡片左上角「预览 | 源码」切换。
//!
//! 管线参考 zed（`crates/markdown/src/mermaid.rs` + `crates/mermaid_render`）：
//! `merman`（Latias94/merman，zed 同源上游）解析 + 布局 + 出 SVG → 主题化
//! → accent 美化 → usvg 量尺寸 → `gpui::Image`（SVG 位图由 gpui 缓存栅格化）。
//! 与 zed 的差异（也是刻意为之）：
//! - zed 的 `mermaid_render` postprocess 是 GPL-3.0 代码，不能复制；本模块的
//!   accent 美化（`accent_postprocess`）为自研实现，只参考其思路。
//! - zed 是只读 markdown 视图；这里是 live 编辑视图，图模式下源码行不渲染，
//!   光标不可进入块内（见 `LiveMarkdownView::mermaid_image_ranges`），编辑必须
//!   经「源码」tab —— 绝无隐藏编辑。
//!
//! 实测约束（踩过的坑，改代码前先读）：
//! - mermaid 输出把节点填充写在**共享 CSS 规则**里
//!   （`#mermaid .node rect,…{fill:#…}`），不是内联属性；且会**丢弃 alpha**
//!   （8 位 hex 截断成 6 位）。所以主题色一律预混成不透明色（见 `blend`），
//!   美化层靠注入内联 `style="fill:…"` 覆盖规则（usvg 实测内联优先）。
//! - 根 `<svg>` 默认带 `background-color:white`；必须让 host profile 的
//!   `root_background = Canvas` 把它改写成画布色，否则暗色主题下整块白底。
//!
//! 缓存键 = (块源码, 主题指纹)：切主题 → 指纹变 → 自动重渲染，无需订阅。
//! 渲染走后台任务 + 200ms 防抖（编辑中不逐键重算），渲染期间优先复用上一帧
//! 同位置块的成品图（防闪烁），无旧图则回退源码 + 「渲染中…」。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gpui::{AppContext, Context, Hsla, Image, ImageFormat, Task};
use regex::Regex;

use crate::editor::markdown_live::view::LiveMarkdownView;
use crate::theme::AppTheme;

/// 编辑防抖：源码变化后延迟渲染，打字不会每键触发一次全量 mermaid 渲染。
/// 缓存 entry 被 prune 时其 Task 一并 drop（取消），连续编辑只留下最后一次。
const RENDER_DEBOUNCE: Duration = Duration::from_millis(200);

/// 图片展示最大宽度（`image_card` 同款上限；卡片 `overflow_hidden` 兜底裁剪）。
pub(crate) const MAX_DISPLAY_W: f32 = 640.0;

/// 栅格化尺寸上限（与 `image_viewer` 同款防 viewBox 膨胀炸弹）。
const MAX_DIM: f32 = 8000.0;
const MAX_PIXELS: u64 = 8000 * 8000;

/// mermaid `fontFamily`：系统 UI 栈 + CJK 回退。末尾带 `sans-serif` 让
/// merman 文本测量在缺字体时走更保守（更宽）的回退，避免栅格化后文字溢出
/// （zed 遗留同款问题，见其 `mermaid_font_family` 注释）。
const FONT_STACK: &str = concat!(
    "system-ui, -apple-system, \"Segoe UI\", \"PingFang SC\", ",
    "\"Hiragino Sans GB\", \"Microsoft YaHei\", sans-serif"
);

// ---------------------------------------------------------------------------
// 主题色
// ---------------------------------------------------------------------------

/// 参与 mermaid 主题化的 deepterm 颜色集合；同时是缓存指纹的输入。
///
/// 所有字段都是**不透明**色：mermaid 会丢弃 themeVariables 里的 alpha，
/// 主题层一律预混（`blend`），避免节点变成实心 accent 原色。
pub(crate) struct MermaidColors {
    pub(crate) is_dark: bool,
    /// 画布 / 边标签底 / 根背景（`HostThemeRoles::canvas`）。
    canvas: Hsla,
    /// 节点填充（`primaryColor`，accent 低比例预混画布）。
    surface: Hsla,
    /// 次级节点填充（`secondaryColor`）。
    surface_alt: Hsla,
    /// 三级节点填充（`tertiaryColor`）。
    surface_muted: Hsla,
    text: Hsla,
    subtle_text: Hsla,
    border: Hsla,
    line: Hsla,
    cluster_bg: Hsla,
    cluster_border: Hsla,
    success: Hsla,
    warning: Hsla,
    error: Hsla,
    /// accent 轮换色板（postprocess 第 1 色）。
    accent: Hsla,
    git_modified: Hsla,
}

impl MermaidColors {
    /// 从当前主题解析（每帧 render 时调用；纯读取，无副作用）。
    pub(crate) fn resolve(theme: &AppTheme) -> Self {
        let ui = &theme.ui;
        let canvas = theme.editor_background();
        Self {
            is_dark: theme.appearance.is_dark(),
            canvas,
            surface: blend(ui.accent, canvas, 0.16),
            surface_alt: blend(ui.accent, canvas, 0.30),
            surface_muted: blend(ui.text, canvas, 0.08),
            text: ui.text,
            subtle_text: ui.text_muted,
            border: ui.border_strong,
            line: ui.text_muted,
            cluster_bg: blend(ui.text, canvas, 0.05),
            cluster_border: ui.border,
            success: ui.success,
            warning: ui.warning,
            error: ui.error,
            accent: ui.accent,
            git_modified: ui.git_modified,
        }
    }

    /// 主题指纹：进缓存键，切主题即失效重渲染。
    pub(crate) fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.is_dark.hash(&mut h);
        FONT_STACK.hash(&mut h);
        let mut chan = |name: &str, c: Hsla| {
            name.hash(&mut h);
            c.h.to_bits().hash(&mut h);
            c.s.to_bits().hash(&mut h);
            c.l.to_bits().hash(&mut h);
            c.a.to_bits().hash(&mut h);
        };
        chan("canvas", self.canvas);
        chan("surface", self.surface);
        chan("surface_alt", self.surface_alt);
        chan("surface_muted", self.surface_muted);
        chan("text", self.text);
        chan("subtle", self.subtle_text);
        chan("border", self.border);
        chan("line", self.line);
        chan("cluster_bg", self.cluster_bg);
        chan("cluster_border", self.cluster_border);
        chan("success", self.success);
        chan("warning", self.warning);
        chan("error", self.error);
        chan("accent", self.accent);
        chan("git_modified", self.git_modified);
        h.finish()
    }

    /// 节点填充轮换色板（accent / success / warning / git 蓝，恒定顺序）。
    /// 各色与画布预混（不透明），保证节点上的文字对比度稳定。
    fn palette(&self) -> [String; 4] {
        [
            css_hex(blend(self.accent, self.canvas, 0.22)),
            css_hex(blend(self.success, self.canvas, 0.20)),
            css_hex(blend(self.warning, self.canvas, 0.22)),
            css_hex(blend(self.git_modified, self.canvas, 0.20)),
        ]
    }
}

/// `#rrggbb` / `#rrggbbaa`（按 alpha 决定位数）。
fn css_hex(c: Hsla) -> String {
    let rgba = gpui::Rgba::from(c);
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = b(rgba.a);
    if a == 0xff {
        format!("#{:02x}{:02x}{:02x}", b(rgba.r), b(rgba.g), b(rgba.b))
    } else {
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            b(rgba.r),
            b(rgba.g),
            b(rgba.b),
            a
        )
    }
}

/// 以 `bg` 为底、按 `t` 比例混入 `fg`，返回**不透明**色。
///
/// mermaid 丢弃 themeVariables 的 alpha（8 位 hex 截断为 6 位），直接传
/// 半透明色会渲染成实心原色；主题层统一走预混。
fn blend(fg: Hsla, bg: Hsla, t: f32) -> Hsla {
    let f = gpui::Rgba::from(fg);
    let b = gpui::Rgba::from(bg);
    let mix = |x: f32, y: f32| x * t + y * (1.0 - t);
    gpui::Rgba {
        r: mix(f.r, b.r),
        g: mix(f.g, b.g),
        b: mix(f.b, b.b),
        a: 1.0,
    }
    .into()
}

// ---------------------------------------------------------------------------
// 渲染管线
// ---------------------------------------------------------------------------

/// 一块渲染完成的 mermaid 图（SVG 字节 + 逻辑尺寸）。
pub(crate) struct RenderedMermaid {
    image: Arc<Image>,
    width: f32,
    height: f32,
}

impl RenderedMermaid {
    /// 阅读态展示尺寸：按 `MAX_DISPLAY_W` 等比缩放，**只缩不放**（保底 1×1
    /// 防零尺寸）。图本身比 640 窄时保持原尺寸。
    pub(crate) fn display_size(&self) -> (f32, f32) {
        let w = self.width.max(1.0);
        let h = self.height.max(1.0);
        let s = (MAX_DISPLAY_W / w).min(1.0);
        (w * s, h * s)
    }

    /// 不缩放的自然尺寸（宽 + 高）：缩放百分比与「还原」都以此为 100%。
    pub(crate) fn natural_size(&self) -> (f32, f32) {
        (self.width.max(1.0), self.height.max(1.0))
    }

    /// 适配给定视口宽度的展示尺寸（保持长宽比，宽高恰好等于视口宽）。
    ///
    /// 全宽模式用它：图**占据整个宽度** —— 比视口宽就缩小，比视口窄就放大
    /// 到铺满（放大上限 [`ZOOM_MAX`]，避免小图被撑成巨幅）。这与「放大到自然
    /// 尺寸」是两件事：自然尺寸会让宽图溢出到需要横向滚动，而这里永不溢出。
    pub(crate) fn fit_width(&self, viewport_w: f32) -> (f32, f32) {
        let (w, h) = self.natural_size();
        let vp = viewport_w.max(1.0);
        let s = (vp / w).clamp(1.0 / ZOOM_MAX, ZOOM_MAX);
        ((w * s).max(1.0), (h * s).max(1.0))
    }

    pub(crate) fn image(&self) -> &Arc<Image> {
        &self.image
    }
}

// ---------------------------------------------------------------------------
// 缩放
// ---------------------------------------------------------------------------

/// 缩放百分比下限 / 上限。
///
/// 范围给得很宽，是为了让「适配宽度」的等效倍率（小图铺满视口时可能 > 1）
/// 落不到边界上 —— 否则 `+` 会变成按不动的死按钮。真实结构图的自然宽度都在
/// 数百像素量级，`0.1`/`16` 实际不可达，但仍然是有限值（极端尺寸的图不会
/// 被撑成无限大）。
pub(crate) const ZOOM_MIN: f32 = 0.1;
pub(crate) const ZOOM_MAX: f32 = 16.0;
/// `+` / `-` 的档位步进（乘除，保证「放大再缩小」回到同一尺寸）。
pub(crate) const ZOOM_STEP: f32 = 1.25;

/// 缩放百分比显示：`100%` / `125%`，去掉多余小数。
pub(crate) fn zoom_label(zoom: f32) -> String {
    format!("{}%", (zoom * 100.0).round() as i32)
}

/// 按百分比缩放自然尺寸（长宽比不变，尺寸保底 1px）。
pub(crate) fn zoomed_size(natural: (f32, f32), zoom: f32) -> (f32, f32) {
    let z = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
    ((natural.0 * z).max(1.0), (natural.1 * z).max(1.0))
}

/// 构建 deepterm 主题的 merman host profile：
/// 第 1 层主题化（roles → themeVariables）+ resvg-safe 输出管线。
fn build_profile(c: &MermaidColors) -> merman::render::HostThemeProfile {
    use merman::render::{
        HostThemeAppearance, HostThemeOutput, HostThemeProfile, HostThemeRoles,
        HostThemeRootBackground,
    };
    let h = css_hex;
    let roles = HostThemeRoles {
        canvas: Some(h(c.canvas)),
        surface: Some(h(c.surface)),
        surface_alt: Some(h(c.surface_alt)),
        surface_muted: Some(h(c.surface_muted)),
        text: Some(h(c.text)),
        subtle_text: Some(h(c.subtle_text)),
        border: Some(h(c.border)),
        line: Some(h(c.line)),
        // 边标签底用画布色（融入背景），且不在节点轮换集合内。
        edge_label_background: Some(h(c.canvas)),
        cluster_background: Some(h(c.cluster_bg)),
        cluster_border: Some(h(c.cluster_border)),
        note_background: Some(h(c.surface_alt)),
        note_border: Some(h(c.border)),
        note_text: Some(h(c.text)),
        actor_background: Some(h(c.surface_alt)),
        actor_border: Some(h(c.border)),
        actor_text: Some(h(c.text)),
        activation_background: Some(h(c.surface_muted)),
        activation_border: Some(h(c.border)),
        error: Some(h(c.error)),
        warning: Some(h(c.warning)),
        success: Some(h(c.success)),
    };
    let mut output = HostThemeOutput::resvg_safe_editor();
    // 根背景必须设成画布色：mermaid 默认在根 `<svg style>` 写
    // `background-color:white`，usvg 会照实渲染成白底（暗色主题下刺眼）。
    // `Canvas` 让管线把它改写成 `editor_background`，与代码卡片底色一致。
    output.root_background = HostThemeRootBackground::Canvas;
    HostThemeProfile::builder()
        .appearance(if c.is_dark {
            HostThemeAppearance::Dark
        } else {
            HostThemeAppearance::Light
        })
        .font_family(FONT_STACK)
        .roles(roles)
        .series_palette([c.accent, c.success, c.warning, c.git_modified].map(css_hex))
        .output(output)
        .build()
}

/// 源码 → 展示图。全类型放开（flowchart/sequence/gantt/pie/…）：
/// 先做类型探测（与 merman 渲染同一套检测器），探测不出 → 明确回「不是
/// Mermaid 图」；探测出但解析/布局/后处理失败 → 回「渲染失败 + 原因」。
/// 两种都返回 `Err`，上层回退源码卡片 —— 不存在「半渲染」状态。
pub(crate) fn render_to_image(
    source: &str,
    colors: &MermaidColors,
) -> Result<Arc<RenderedMermaid>, String> {
    use merman::render::HeadlessRenderer;
    let profile = build_profile(colors);
    let renderer = HeadlessRenderer::new().with_host_theme(&profile);
    // resvg-safe 管线由 profile 输出 preset（ResvgSafe）提供：剥
    // `<foreignObject>`、可读文本回退、清栅格化隐患。
    //
    // 非图文本：merman 在渲染路径上回 `Err(Parse(DetectType))`，错误文案对
    // 用户没意义（"No diagram type detected…"）。用同一份探测注册表先判，
    // 让 footer 说人话；探测通过后的失败才是真·渲染失败（保留原因）。
    if renderer
        .engine
        .registry()
        .detect_type(source, &mut merman::MermaidConfig::empty_object())
        .is_err()
    {
        return Err("未识别为 Mermaid 图，以源码展示".to_string());
    }
    let svg = renderer
        .render_svg_sync(source)
        .map_err(|e| format!("Mermaid 渲染失败：{e}"))?
        .ok_or_else(|| "未识别为 Mermaid 图，以源码展示".to_string())?;

    // 第 2 层主题美化：节点填充轮换 accent 色板（自研，见模块文档）。
    let svg = accent_postprocess(&svg, colors);

    // usvg 校验 + 量尺寸（gpui 栅格化走同一 usvg 解析，这里等价于预检）。
    let bytes = svg.as_bytes();
    let opt = usvg::Options {
        resources_dir: None,
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &opt).map_err(|e| format!("SVG 解析失败：{e}"))?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return Err("SVG 尺寸非法".to_string());
    }
    if w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err("Mermaid 图过大，拒绝渲染".to_string());
    }

    let image = Image::from_bytes(ImageFormat::Svg, bytes.to_vec());
    Ok(Arc::new(RenderedMermaid {
        image: Arc::new(image),
        width: w,
        height: h,
    }))
}

// ---------------------------------------------------------------------------
// accent 美化（自研 postprocess）
// ---------------------------------------------------------------------------

/// 节点填充轮换：给每个 `.node` 组的首个形状注入内联 `style="fill:…"`。
///
/// 为什么不改 CSS/属性：mermaid 把节点填充写在**共享规则**里
/// （`#mermaid .node rect,…{fill:#…}`），且属性值不含 alpha——只能覆盖。
/// 实测 usvg 解析时**内联 style 优先于 CSS 规则**（resvg-safe 管线也不会剥
/// 它），所以在形状标签上注入 style 最稳。识别不到 `.node` 组（sequence /
/// pie 等）就原样返回：主题层已给出正确配色，美化层只做点缀。
///
/// 安全边界：只认「节点开标签之后、遇到嵌套 `<g` 或 `</g>` 之前」的第一个
/// 形状标签；带 `style=` 的跳过。结构不符就静默不轮换，绝不动标签文字。
fn accent_postprocess(svg: &str, colors: &MermaidColors) -> String {
    static NODE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static SHAPE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    if !svg.contains("<g class=\"node") {
        return svg.to_string();
    }
    let node_re =
        NODE_RE.get_or_init(|| Regex::new(r#"<g class="node[^"]*"[^>]*>"#).expect("node regex"));
    let shape_re = SHAPE_RE.get_or_init(|| {
        Regex::new(r#"<(?:rect|circle|ellipse|polygon|path)\b[^>]*?/?>"#).expect("shape regex")
    });
    let palette = colors.palette();
    let mut out = String::with_capacity(svg.len() + 64 * 8);
    let mut last = 0usize;
    let mut ix = 0usize;
    for m in node_re.find_iter(svg) {
        let search_start = m.end();
        let limit = (search_start + 800).min(svg.len());
        let window = &svg[search_start..limit];
        // 形状必须在嵌套子组/节点结束之前出现（否则是别的结构，放弃）。
        let mut stop = window.find("<g").unwrap_or(window.len());
        if let Some(end) = window.find("</g>") {
            stop = stop.min(end);
        }
        let Some(sm) = shape_re.find(&window[..stop]) else {
            continue;
        };
        let tag = sm.as_str();
        if tag.contains("style=") {
            continue;
        }
        let fill = &palette[ix % palette.len()];
        ix += 1;
        let new_tag = inject_inline_fill(tag, fill);
        out.push_str(&svg[last..search_start + sm.start()]);
        out.push_str(&new_tag);
        last = search_start + sm.end();
    }
    out.push_str(&svg[last..]);
    out
}

/// 在形状标签名后插入 `style="fill:…"`（保留原有属性与自闭合形态）。
fn inject_inline_fill(tag: &str, fill: &str) -> String {
    let name_end = tag
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(tag.len());
    let (head, tail) = tag.split_at(name_end);
    format!("{head} style=\"fill:{fill}\"{tail}")
}

// ---------------------------------------------------------------------------
// 缓存 / 异步 / tab 状态
// ---------------------------------------------------------------------------

/// Mermaid cache key: the block source (shared, not copied) + theme fingerprint.
/// `Arc<str>` lets `body()`/`ensure()` probe the cache and record frame slots
/// without allocating a fresh `String` on every call — the render path calls
/// `body()` per visible mermaid card, and the old `source.to_string()` per call
/// was pure garbage on the hot path.
type MermaidKey = (Arc<str>, u64);

/// 一块 mermaid 的渲染体（渲染分支据此决定画图还是回退源码）。
#[derive(Clone)]
pub(crate) enum MermaidBody {
    /// 显示图；`pending = true` 表示正在重渲染（展示旧图 + 呼吸提示）。
    Image {
        image: Arc<RenderedMermaid>,
        pending: bool,
    },
    /// 回退源码：`error = None` 渲染中（无旧图），`Some(msg)` 为失败原因
    /// （已本地化，直接做卡片 footer）。
    Source { error: Option<String> },
}

struct Entry {
    result: Arc<OnceLock<Result<Arc<RenderedMermaid>, String>>>,
    /// 插入时从上一帧同位置块拷来的成品图：编辑/切主题重渲染期间继续显示
    /// （呼吸脉冲），不闪回源码（zed `fallback_image` 同款，按文档序位置对应）。
    fallback: Option<Arc<RenderedMermaid>>,
    /// 后台渲染任务；entry 被 prune 时随 drop 取消。
    _task: Task<()>,
}

/// 每帧生命周期：`begin_frame` → 对每个 mermaid 块 `ensure`（全量、含未在
/// 视口内的组，保证位置回退跨滚动稳定）→ `end_frame`（prune + 帧推进）。
pub(crate) struct MermaidState {
    entries: HashMap<MermaidKey, Entry>,
    /// 本帧（pass1）遇到的 mermaid 块键，按文档序。
    frame_keys: Vec<MermaidKey>,
    frame_ids: Vec<usize>,
    /// 上一帧的 `frame_keys`（位置回退的取图处）。
    prev_keys: Vec<MermaidKey>,
    /// 「源码」tab 状态：围栏首行 → 是否显示源码。
    showing_code: HashSet<usize>,
    /// 缩放百分比：围栏首行 → 倍率（1.0 = 100% = 自然尺寸）。
    ///
    /// 只在用户按过 `+` / `-` 后才有条目（`None` 时按「适配宽度」展示）。
    /// 与 `showing_code` 同款：按块首行记账，`end_frame` 时裁到仍存在的块。
    zoom: HashMap<usize, f32>,
    /// 本帧主题指纹（`begin_frame` 写入；帧间查询用上一帧值）。
    theme_fp: u64,
}

impl MermaidState {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            frame_keys: Vec::new(),
            frame_ids: Vec::new(),
            prev_keys: Vec::new(),
            showing_code: HashSet::new(),
            zoom: HashMap::new(),
            theme_fp: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 丢弃所有「按块首行记账」的 UI 状态（换文档时用）。
    ///
    /// 行号只在一份文档内有意义：重载后同一行号可能落在别的块上，留着旧
    /// 缩放/源码 tab 会把它们套到无关的块上。
    pub(crate) fn clear_block_state(&mut self) {
        self.showing_code.clear();
        self.zoom.clear();
    }

    /// 开帧：写主题指纹、清空本帧键槽（`prev_keys` 保留上一帧，供 ensure 取回退图）。
    pub(crate) fn begin_frame(&mut self, theme_fp: u64) {
        self.theme_fp = theme_fp;
        self.frame_keys.clear();
        self.frame_ids.clear();
    }

    /// 保证 `block_id` 块有渲染 entry（没有则防抖后台渲染一张）。
    ///
    /// 幂等：同键已存在直接记帧位后返回。新 entry 的回退图取「上一帧同位置」
    /// 块的成品图（按文档序对齐，滚动/虚拟化不影响 —— pass1 遍历全部块）。
    pub(crate) fn ensure(
        &mut self,
        block_id: usize,
        source: &str,
        colors: &MermaidColors,
        cx: &mut Context<LiveMarkdownView>,
    ) {
        // 指纹用 `begin_frame` 存下的那份：每块重算 15 字段哈希纯属浪费。
        let key: MermaidKey = (Arc::from(source), self.theme_fp);
        let pos = self.frame_keys.len();
        if self.entries.contains_key(&key) {
            // Already rendered (or in flight): record the frame slot with the
            // *cached* key so pruning compares shared pointers, not contents.
            self.frame_keys.push(key.clone());
            self.frame_ids.push(block_id);
            return;
        }
        self.frame_keys.push(key.clone());
        self.frame_ids.push(block_id);
        let fallback = self
            .prev_keys
            .get(pos)
            .and_then(|k| self.entries.get(k))
            .and_then(|e| e.result.get().and_then(|r| r.as_ref().ok().cloned()));

        let result = Arc::new(OnceLock::new());
        let result_task = result.clone();
        // gpui `Hsla` 未实现 `PartialEq`/`Send` 绑定无碍，但颜色需值拷贝进任务
        // （`MermaidColors` 全字段 Copy）。
        let source_task = source.to_string();
        let colors_owned = clone_colors(colors);
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RENDER_DEBOUNCE).await;
            let out = cx
                .background_spawn(async move { render_to_image(&source_task, &colors_owned) })
                .await;
            let _ = result_task.set(out);
            if let Some(this) = this.upgrade() {
                this.update(cx, |_, cx| cx.notify());
            }
        });
        self.entries.insert(
            key,
            Entry {
                result,
                fallback,
                _task: task,
            },
        );
    }

    /// 收帧：prune 不再出现的键（保留本帧 + 上一帧，上一帧是回退图来源），
    /// 推进帧位、清理越界的 tab / 缩放状态。
    pub(crate) fn end_frame(&mut self) {
        let old_prev = std::mem::take(&mut self.prev_keys);
        // 本帧 → `prev_keys`（下帧 ensure 的回退取图处），再对新旧两帧 prune。
        self.prev_keys = std::mem::take(&mut self.frame_keys);
        let cur = &self.prev_keys;
        let old = &old_prev;
        self.entries
            .retain(|k, _| cur.contains(k) || old.contains(k));
        let ids = std::mem::take(&mut self.frame_ids);
        self.showing_code.retain(|id| ids.contains(id));
        self.zoom.retain(|id, _| ids.contains(id));
    }

    // ---- 缩放 -----------------------------------------------------------

    /// 某块的**显式**缩放倍率（`None` = 没按过 `+`/`-`，按适配宽度展示）。
    pub(crate) fn zoom(&self, block_id: usize) -> Option<f32> {
        self.zoom.get(&block_id).copied()
    }

    /// 设为显式倍率（`+` / `-` 的落点，已夹在 [`ZOOM_MIN`]/[`ZOOM_MAX`] 内）。
    pub(crate) fn set_zoom(&mut self, block_id: usize, zoom: f32) {
        self.zoom.insert(block_id, zoom.clamp(ZOOM_MIN, ZOOM_MAX));
    }

    /// 清除显式倍率：回到「适配宽度 / 阅读尺寸」的默认展示。
    pub(crate) fn clear_zoom(&mut self, block_id: usize) {
        self.zoom.remove(&block_id);
    }

    /// 查块渲染体（渲染分支与光标禁区判定共用，保证两者永远一致）。
    pub(crate) fn body(&self, source: &str) -> MermaidBody {
        // Probe without allocating: the map's `Hash`/`Eq` for `Arc<str>` works
        // on the pointed-to string, so a borrowed `&str` key suffices.
        let Some(entry) = self.entries.get(&(Arc::from(source), self.theme_fp)) else {
            // pass1 尚未跑过（首帧/异常路径）：按渲染中回退源码。
            return MermaidBody::Source { error: None };
        };
        match entry.result.get() {
            Some(Ok(img)) => MermaidBody::Image {
                image: img.clone(),
                pending: false,
            },
            Some(Err(msg)) => MermaidBody::Source {
                error: Some(msg.clone()),
            },
            None => match &entry.fallback {
                Some(img) => MermaidBody::Image {
                    image: img.clone(),
                    pending: true,
                },
                None => MermaidBody::Source { error: None },
            },
        }
    }

    pub(crate) fn is_showing_code(&self, block_id: usize) -> bool {
        self.showing_code.contains(&block_id)
    }

    /// tab 切换（唯一的状态改变入口；预览模式 ↔ 源码模式）。
    ///
    /// 只改状态：切到**预览**时还必须把光标夹出块外（否则光标会滞留在
    /// 下一帧就不可见的源码区里），这一步依赖 view 的光标逻辑，由
    /// `LiveMarkdownView::set_mermaid_showing_code` 负责编排。
    pub(crate) fn set_showing_code(&mut self, block_id: usize, showing: bool) -> bool {
        if showing {
            self.showing_code.insert(block_id)
        } else {
            self.showing_code.remove(&block_id)
        }
    }
}

/// `MermaidColors` 无 `PartialEq`（gpui `Hsla` 未实现），任务侧要独立拥有
/// 一份 —— 直接逐字段拷贝（Hsla: Copy）。
fn clone_colors(c: &MermaidColors) -> MermaidColors {
    MermaidColors {
        is_dark: c.is_dark,
        canvas: c.canvas,
        surface: c.surface,
        surface_alt: c.surface_alt,
        surface_muted: c.surface_muted,
        text: c.text,
        subtle_text: c.subtle_text,
        border: c.border,
        line: c.line,
        cluster_bg: c.cluster_bg,
        cluster_border: c.cluster_border,
        success: c.success,
        warning: c.warning,
        error: c.error,
        accent: c.accent,
        git_modified: c.git_modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;

    fn colors() -> MermaidColors {
        MermaidColors {
            is_dark: true,
            canvas: rgb(0x1e1e2e).into(),
            surface: rgb(0x89b4fa).into(),
            surface_alt: rgb(0x94e2d5).into(),
            surface_muted: rgb(0x313244).into(),
            text: rgb(0xcdd6f4).into(),
            subtle_text: rgb(0xa6adc8).into(),
            border: rgb(0x585b70).into(),
            line: rgb(0xa6adc8).into(),
            cluster_bg: rgb(0x313244).into(),
            cluster_border: rgb(0x45475a).into(),
            success: rgb(0xa6e3a1).into(),
            warning: rgb(0xf9e2af).into(),
            error: rgb(0xf38ba8).into(),
            accent: rgb(0x89b4fa).into(),
            git_modified: rgb(0x74c7ec).into(),
        }
    }

    /// 会话验收样例：改策略 → 探活 → 分支 → 构建 → 部署 → 评测 → 回环。
    const SAMPLE: &str = r#"flowchart TD
    A["改 my_policy/policy.py<br/>或 v1-SKILL.md"] --> B
    B["探活端点<br/>probe_endpoint.py"] --> C{模型可用?}
    C -->|"否"| B1["换端点 / 等配额<br/>改 .env.local"]
    B1 --> B
    C -->|"是"| D
    D["离线自检<br/>test_my_policy.py<br/>无 Docker · 无 key"] --> E{通过?}
    E -->|"否"| A
    E -->|"是"| F
    F["构建 pack<br/>trajectoryrl-miner build"] --> G
    G["同步到服务器<br/>scp → 192.168.194.4"] --> H
    H["真场景评测<br/>eval_local.py --scenarios a,b,c"] --> I
    I["读产物<br/>results.json / policy.log / meter.json"] --> J{分数提升?}
    J -->|"否"| K["看 testee_transcript.txt<br/>定位 agent 死因"]
    J -->|"是"| L["保留 · 更新 README"]
    K --> A"#;

    #[test]
    fn sample_flowchart_renders() {
        let out = render_to_image(SAMPLE, &colors()).expect("sample 应渲染成功");
        assert!(out.width > 100.0 && out.height > 100.0, "尺寸异常");
    }

    #[test]
    fn all_diagram_types_render() {
        // 「全开」：无白名单，merman 检测到就渲染。
        let cases = [
            "sequenceDiagram\n    Alice->>Bob: Hello\n    Bob-->>Alice: Hi",
            "classDiagram\n    class Animal {\n      +String name\n      +eat()\n    }",
            "pie title Pets\n    \"Cats\": 30\n    \"Dogs\": 60",
            "gantt\n    title 持续集成\n    dateFormat YYYY-MM-DD\n    section 构建\n    编译 :a1, 2026-01-01, 3d",
            "stateDiagram-v2\n    [*] --> Running\n    Running --> [*]",
            "erDiagram\n    CUSTOMER ||--o{ ORDER : places",
            "xychart-beta\n    title \"发布量\"\n    x-axis [1, 2, 3]\n    bar [10, 20, 30]",
            "gitGraph\n    commit\n    branch dev\n    checkout dev\n    commit",
        ];
        for src in cases {
            render_to_image(src, &colors())
                .unwrap_or_else(|e| panic!("{src:?} 应渲染成功，实际：{e}"));
        }
    }

    #[test]
    fn non_mermaid_is_unsupported() {
        // `expect_err` 要求 Ok 型实现 Debug，这里手动 match。
        let err = match render_to_image("just some plain text", &colors()) {
            Err(e) => e,
            Ok(_) => panic!("非 mermaid 文本必须失败回退"),
        };
        assert!(err.contains("未识别"), "错误信息应可直接做 footer：{err}");
    }

    #[test]
    fn broken_syntax_does_not_panic() {
        // 语法坏掉：返回 Err（回退源码）即可，绝不 panic。
        for src in [
            "flowchart TD\nA -->>>>",
            "flowchart TD\n  A[未闭合",
            "sequenceDiagram\n->>:",
        ] {
            let _ = render_to_image(src, &colors());
        }
    }

    #[test]
    fn cjk_and_combining_marks_do_not_panic() {
        let zalgo = "Ne\u{0301}\u{0302}\u{0303}d";
        let src = format!("flowchart TD\n  A[\"**{zalgo}** 中文节点 {zalgo}\"] --> B");
        let _ = render_to_image(&src, &colors());
    }

    #[test]
    fn accent_rotation_injects_inline_fill_on_node_shapes() {
        let c = colors();
        let palette = c.palette();
        // 模拟真实结构：节点填充在共享 CSS 规则里，形状标签无 fill/style。
        let svg = concat!(
            r#"<svg><style>#m .node rect{fill:#89b4fa;}</style><g class="nodes">"#,
            r#"<g class="node default" id="n0" transform="translate(1,2)"><rect class="basic label-container" x="0" y="0" width="10" height="10"/><g class="label"></g></g>"#,
            r#"<g class="node default" id="n1" transform="translate(3,4)"><path class="basic label-container" d="M0 0h5v5z"/></g>"#,
            r#"<g class="node default" id="n2"><rect class="basic label-container" width="8" height="8"/></g>"#,
            r#"<g class="cluster"><rect width="99" height="99"/></g>"#,
            r#"</g></svg>"#,
        );
        let out = accent_postprocess(svg, &c);
        assert!(
            out.contains(&format!("style=\"fill:{}\"", palette[0])),
            "n0 应注入第 1 色：{out}"
        );
        assert!(
            out.contains(&format!("style=\"fill:{}\"", palette[1])),
            "n1 应注入第 2 色：{out}"
        );
        assert!(
            out.contains(&format!("style=\"fill:{}\"", palette[2])),
            "n2 应注入第 3 色：{out}"
        );
        // cluster 不是 node：不能被动。
        assert_eq!(
            out.matches("style=\"fill:").count(),
            3,
            "只应注入 3 个节点：{out}"
        );
        // 原始属性必须原样保留（只在标签名后插入了 style）。
        assert!(
            out.contains(r#"class="basic label-container" x="0" y="0" width="10" height="10"/>"#),
            "原有属性与自闭合形态必须保留：{out}"
        );
    }

    #[test]
    fn accent_skips_shapes_that_already_have_style() {
        let c = colors();
        let svg = concat!(
            r#"<svg><g class="node default" id="n0">"#,
            r#"<rect style="opacity:0.5" class="basic label-container" width="4" height="4"/></g></svg>"#,
        );
        let out = accent_postprocess(svg, &c);
        assert_eq!(out, svg, "已有 style 的形状不能重复注入");
    }

    #[test]
    fn accent_noop_without_node_groups() {
        // sequence / pie 等无 `.node` 组：原样返回（第 1 层主题已配色）。
        let c = colors();
        let svg = r#"<svg><g class="actor"><rect width="4" height="4"/></g></svg>"#;
        assert_eq!(accent_postprocess(svg, &c), svg);
    }

    #[test]
    fn accent_palette_cycles() {
        let c = colors();
        let palette = c.palette();
        let nodes: String = (0..5)
            .map(|i| {
                format!(r#"<g class="node default" id="n{i}"><rect width="4" height="4"/></g>"#)
            })
            .collect();
        let out = accent_postprocess(&format!(r#"<svg>{nodes}</svg>"#), &c);
        let count: usize = palette
            .iter()
            .map(|p| out.matches(p.as_str()).count())
            .sum();
        assert_eq!(count, 5, "5 个节点都应被轮换：{out}");
        // 第 5 个回绕到第 1 色。
        assert_eq!(
            out.matches(&format!("style=\"fill:{}\"", palette[0]))
                .count(),
            2
        );
    }

    #[test]
    fn fingerprint_tracks_theme_changes() {
        let a = colors();
        let fp = a.fingerprint();
        assert_eq!(fp, a.fingerprint(), "同色应同指纹");
        let mut b = clone_colors(&a);
        b.is_dark = !b.is_dark;
        assert_ne!(fp, b.fingerprint(), "明暗切换必须换指纹（触发重渲染）");
        let mut c = clone_colors(&a);
        c.accent = rgb(0xff0000).into();
        assert_ne!(fp, c.fingerprint(), "accent 变化必须换指纹");
    }

    /// 端到端：真实 merman 输出上，美化层必须真的改到填充（不是空转），
    /// 且渲染结果里不含白色根背景（暗色主题白底是最刺眼的回归）。
    #[test]
    fn real_output_is_accented_and_has_no_white_root_background() {
        let c = colors();
        let profile = build_profile(&c);
        let renderer = merman::render::HeadlessRenderer::new().with_host_theme(&profile);
        let raw = renderer.render_svg_sync(SAMPLE).unwrap().unwrap();
        let out = accent_postprocess(&raw, &c);
        let palette = c.palette();
        let hits: usize = palette
            .iter()
            .map(|p| out.matches(p.as_str()).count())
            .sum();
        assert!(
            hits >= 4,
            "真实 flowchart 上应至少轮换 4 个节点，实际 {hits}"
        );
        assert!(out.contains("style=\"fill:"), "应注入内联 style");
        // 根背景：不得残留 mermaid 默认白底。
        assert!(
            !raw.contains("background-color:white"),
            "根 svg 白底必须被改写：{raw:.400}"
        );
        assert!(
            !raw.contains("background-color: white"),
            "根 svg 白底必须被改写"
        );
    }

    /// usvg 解析后的实际填充必须命中轮换色板（验证内联 style 真的生效，
    /// 而不是写进了被 CSS 覆盖的无效位置）。
    #[test]
    fn inline_fill_survives_usvg_resolution() {
        let c = colors();
        let profile = build_profile(&c);
        let renderer = merman::render::HeadlessRenderer::new().with_host_theme(&profile);
        let raw = renderer.render_svg_sync(SAMPLE).unwrap().unwrap();
        let out = accent_postprocess(&raw, &c);
        let opt = usvg::Options {
            resources_dir: None,
            ..usvg::Options::default()
        };
        let tree = usvg::Tree::from_data(out.as_bytes(), &opt).expect("美化后的 SVG 必须仍可解析");
        fn collect(n: &usvg::Node, out: &mut Vec<(u8, u8, u8)>) {
            match n {
                usvg::Node::Group(g) => {
                    for ch in g.children() {
                        collect(ch, out);
                    }
                }
                usvg::Node::Path(p) => {
                    if let Some(usvg::Paint::Color(col)) = p.fill().map(|f| f.paint()) {
                        out.push((col.red, col.green, col.blue));
                    }
                }
                _ => {}
            }
        }
        let mut fills = Vec::new();
        collect(
            &usvg::Node::Group(Box::new(tree.root().clone())),
            &mut fills,
        );
        // 轮换色板的 RGB 必须在解析结果里出现。
        let palette_rgb: Vec<(u8, u8, u8)> = c
            .palette()
            .iter()
            .map(|hex| {
                let b = |i: usize| u8::from_str_radix(&hex[1 + i * 2..3 + i * 2], 16).unwrap();
                (b(0), b(1), b(2))
            })
            .collect();
        let hit = palette_rgb.iter().filter(|rgb| fills.contains(rgb)).count();
        assert!(
            hit >= 3,
            "usvg 解析后应至少命中 3 个轮换色，实际 {hit}（fills={fills:?}）"
        );
        // 暗色主题：解析结果里不得有纯白填充（白底回归检测）。
        assert!(!fills.contains(&(255, 255, 255)), "不得残留纯白填充");
    }

    #[test]
    fn display_size_caps_width() {
        let big = RenderedMermaid {
            image: Arc::new(Image::from_bytes(ImageFormat::Png, vec![0])),
            width: 1280.0,
            height: 640.0,
        };
        let (w, h) = big.display_size();
        assert!((w - MAX_DISPLAY_W).abs() < 0.01);
        assert!((h - MAX_DISPLAY_W / 2.0).abs() < 0.01);

        let small = RenderedMermaid {
            image: big.image.clone(),
            width: 300.0,
            height: 200.0,
        };
        assert_eq!(small.display_size(), (300.0, 200.0));
    }

    /// 适配宽度：图宽恰好等于视口宽（宽图缩小、小图放大到铺满），长宽比不变。
    ///
    /// 「全宽只占据整个宽度、不放大到自然尺寸」这条要求的落点就在这里。
    #[test]
    fn fit_width_fills_viewport_without_changing_ratio() {
        let img = |w: f32, h: f32| RenderedMermaid {
            image: Arc::new(Image::from_bytes(ImageFormat::Png, vec![0])),
            width: w,
            height: h,
        };
        // 宽图：缩到视口宽。
        let (w, h) = img(2000.0, 1000.0).fit_width(800.0);
        assert!((w - 800.0).abs() < 0.01, "宽图应缩到视口宽，实际 {w}");
        assert!((h - 400.0).abs() < 0.01, "高度按比例，实际 {h}");
        // 小图：放大到铺满视口（不是保持原尺寸）。
        let (w, h) = img(200.0, 100.0).fit_width(800.0);
        assert!((w - 800.0).abs() < 0.01, "小图应放大到视口宽，实际 {w}");
        assert!((h - 400.0).abs() < 0.01, "高度按比例，实际 {h}");
        // 长宽比在所有档位都保持不变。
        for (nw, nh) in [(2000.0, 1000.0), (300.0, 900.0), (50.0, 25.0)] {
            let (w, h) = img(nw, nh).fit_width(640.0);
            assert!(
                (w / h - nw / nh).abs() < 0.01,
                "({nw}, {nh}) 适配后长宽比变了：({w}, {h})"
            );
        }
        // 极端小图：放大倍数夹在 ZOOM_MAX 内（不被撑成巨幅）。
        let (w, _) = img(10.0, 10.0).fit_width(100_000.0);
        assert!(
            w <= 10.0 * ZOOM_MAX + 0.01,
            "放大倍数应夹在 {ZOOM_MAX} 内，实际 {w}"
        );
        // 零尺寸视口不炸（首帧 bounds 为 0 的路径）。
        let (w, h) = img(500.0, 250.0).fit_width(0.0);
        assert!(w >= 1.0 && h >= 1.0, "尺寸不得归零：({w}, {h})");
    }

    /// 缩放的纯函数：档位乘除可逆、夹紧、百分比文案无多余小数。
    #[test]
    fn zoom_math_is_reversible_and_clamped() {
        // 乘除可逆（夹紧前）：放大再缩小回到原值。
        let step_in = |z: f32| z * ZOOM_STEP;
        let step_out = |z: f32| z / ZOOM_STEP;
        assert!((step_out(step_in(1.0)) - 1.0).abs() < 1e-6);
        assert!((step_out(step_in(0.5)) - 0.5).abs() < 1e-6);
        // 夹紧（`set_zoom` 的落点）。
        let clamp = |z: f32| z.clamp(ZOOM_MIN, ZOOM_MAX);
        assert_eq!(clamp(100.0), ZOOM_MAX);
        assert_eq!(clamp(0.001), ZOOM_MIN);
        let mut z = 1.0;
        for _ in 0..40 {
            z = clamp(step_in(z));
        }
        assert_eq!(z, ZOOM_MAX, "反复放大应停在上限");
        let mut z = 1.0;
        for _ in 0..40 {
            z = clamp(step_out(z));
        }
        assert_eq!(z, ZOOM_MIN, "反复缩小应停在下限");
        // 文案。
        assert_eq!(zoom_label(1.0), "100%");
        assert_eq!(zoom_label(1.25), "125%");
        assert_eq!(zoom_label(0.8), "80%");
        // 尺寸：长宽比不变 + 保底 1px。
        assert_eq!(zoomed_size((1000.0, 500.0), 2.0), (2000.0, 1000.0));
        let (w, h) = zoomed_size((1.0, 1.0), ZOOM_MIN);
        assert!(w >= 1.0 && h >= 1.0, "尺寸不得归零：({w}, {h})");
    }

    /// 状态机：`+`/`−` 记显式倍率，「还原」清掉它回到适配宽度。
    #[test]
    fn zoom_state_records_and_clears() {
        let mut st = MermaidState::new();
        assert_eq!(st.zoom(0), None, "初始无显式倍率");
        st.set_zoom(0, 2.0);
        assert_eq!(st.zoom(0), Some(2.0));
        st.clear_zoom(0);
        assert_eq!(st.zoom(0), None, "还原应回到适配宽度");
        // 夹紧。
        st.set_zoom(0, 999.0);
        assert_eq!(st.zoom(0), Some(ZOOM_MAX));
        st.set_zoom(0, 0.0);
        assert_eq!(st.zoom(0), Some(ZOOM_MIN));
    }
}
