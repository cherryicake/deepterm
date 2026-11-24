//! mermaid 图实时渲染：` ```mermaid ` 代码卡片默认画成真实图形（flowchart /
//! sequenceDiagram / gantt / pie … 全类型），卡片左上角「预览 | 源码」切换。
//!
//! 管线：`merman` 解析 + 布局 + 出 SVG（自带 resvg-safe 管线）→ usvg 量尺寸
//! → `gpui::Image`（SVG 位图由 gpui 缓存栅格化）。
//!
//! # 为什么用 merman 0.11 而不是 0.7
//!
//! 0.7 的 flowchart 只有 dagre 布局，它把同一 rank 的节点横向排开。跨组连线
//! 密集的架构图（两个 subgraph、十几条边）会被摊成两三千像素宽——用户看到
//! 的是「本该上下堆叠的两部分被拉成左右错开，连线绕成蜘蛛网」，调 `nodeSpacing`
//! / `rankSpacing` / `viewport_width` 都无法根治（都实测过）。
//!
//! 0.11 换成分层折线布局（直角连线、分组纵向堆叠），同一张 19 节点 / 13 条
//! 跨组边的图从 3115×967（宽高比 3.22）变成 1455×1701（0.86）。
//! 回归用例：`dense_architecture_graph_lays_out_vertically`。
//!
//! # 主题化：走 Mermaid 原生 `themeVariables`
//!
//! 0.11 把宿主主题换成了基于 recipe 的新架构（`DiagramTheme`），迁移面很大；
//! 而 `themeVariables` 是 Mermaid 原生、跨版本稳定的接口。`MermaidColors` 本来
//! 就按 Mermaid 的语义槽位组织，直接映射即可（见 `theme_config`）。
//!
//! 顺带的好处：0.11 完整支持 `classDef`，所以不再需要手工注入节点填充
//! （0.7 时代那套 `accent_postprocess` 已删除）。
//!
//! # 与 zed 的差异
//!
//! - zed 的 `mermaid_render` postprocess 是 GPL-3.0 代码，不能复制。
//! - zed 是只读 markdown 视图；这里是 live 编辑视图，图模式下源码行不渲染，
//!   光标不可进入块内（见 `LiveMarkdownView::mermaid_image_ranges`），编辑必须
//!   经「源码」tab —— 绝无隐藏编辑。
//!
//! 实测约束（踩过的坑，改代码前先读）：
//! - mermaid 会**丢弃 themeVariables 里的 alpha**（8 位 hex 截断成 6 位）。
//!   所以 `MermaidColors` 的颜色一律预混成不透明色（见 `blend`）——直接传
//!   accent 原色会让节点变成实心高饱和块。
//! - 根 `<svg>` 默认带 `background-color:white`。必须显式给
//!   `themeVariables.background`，否则暗色主题下整张图是一块白板
//!   （回归用例 `real_output_uses_dark_theme_background`）。
//! - `themeVariables` 只能通过 `SemanticParser::with_site_config` 注入；
//!   `Renderer` 本身没有 `with_site_config`（0.11 的 API 分层）。
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

/// 把 deepterm 主题写进 Mermaid 的 `themeVariables`。
///
/// 为什么用 `themeVariables` 而不是 merman 的宿主主题 API：
/// 0.11 把宿主主题换成了基于 recipe 的新架构（`DiagramTheme`），迁移面很大；
/// 而 `themeVariables` 是 Mermaid 原生、跨版本稳定的接口，`%%{init}%%`
/// 指令和 site config 都能生效。配色集合本来就是按 Mermaid 的语义槽位
/// 组织的（`primaryColor` / `clusterBkg` …），直接映射即可。
///
/// 注意 mermaid 会丢弃 alpha（8 位 hex 截断成 6 位），所以 `MermaidColors`
/// 里的颜色都已经预混成不透明色。
fn theme_config(colors: &MermaidColors) -> merman::MermaidConfig {
    let h = css_hex;
    let vars = serde_json::json!({
        "theme": "base",
        "themeVariables": {
            // 根背景：不给的话 mermaid 会用白色，暗色主题下刺眼。
            "background": h(colors.canvas),
            "fontFamily": FONT_STACK,
            "fontSize": "14px",
            // 节点头部（primary）与正文。
            "primaryColor": h(colors.surface),
            "primaryTextColor": h(colors.text),
            "primaryBorderColor": h(colors.border),
            // 次级/三级（部分图类型用）。
            "secondaryColor": h(colors.surface_alt),
            "tertiaryColor": h(colors.surface_muted),
            "secondaryTextColor": h(colors.text),
            "tertiaryTextColor": h(colors.text),
            // 连线与箭头。
            "lineColor": h(colors.line),
            // 分组（subgraph）：0.11 的分层布局会把分组画成实心框，
            // 底色必须比节点更淡，否则节点会糊在框里分辨不出。
            "clusterBkg": h(colors.cluster_bg),
            "clusterBorder": h(colors.cluster_border),
            // 边标签。
            "edgeLabelBackground": h(colors.canvas),
            // 状态/序列图等用的语义色。
            "errorBkgColor": h(colors.error),
            "errorTextColor": h(colors.text),
            "noteBkgColor": h(colors.surface_alt),
            "noteTextColor": h(colors.text),
            "noteBorderColor": h(colors.border),
            "actorBkg": h(colors.surface_alt),
            "actorBorder": h(colors.border),
            "actorTextColor": h(colors.text),
            "signalColor": h(colors.line),
            "signalTextColor": h(colors.text),
            "labelBoxBkgColor": h(colors.surface_muted),
            "labelBoxBorderColor": h(colors.border),
            "labelTextColor": h(colors.text),
            "loopTextColor": h(colors.text),
            "activationBkgColor": h(colors.surface_muted),
            "activationBorderColor": h(colors.border),
        },
        // series：多系列图的轮换色板。
        "themeCSS": null,
    });
    merman::MermaidConfig::from_value(vars)
}

/// 源码 → 展示图。全类型放开（flowchart/sequence/gantt/pie/…）：
/// 先做类型探测（与 merman 渲染同一套检测器），探测不出 → 明确回「不是
/// Mermaid 图」；探测出但解析/布局/后处理失败 → 回「渲染失败 + 原因」。
/// 两种都返回 `Err`，上层回退源码卡片 —— 不存在「半渲染」状态。
pub(crate) fn render_to_image(
    source: &str,
    colors: &MermaidColors,
) -> Result<Arc<RenderedMermaid>, String> {
    use merman::{OperationControl, RenderOutput, RenderRequest, Renderer, SemanticParser};

    // 非图文本：merman 在解析路径上会失败，错误文案对用户没意义
    // （"No diagram type detected…"）。先探测让 footer 说人话；
    // 探测通过后的失败才是真·渲染失败（保留原因）。
    //
    // 探测用**不带主题**的默认 parser：主题只影响出图样式，不影响图类型判定。
    if merman::DetectorRegistry::pinned_mermaid_baseline()
        .detect_type(source, &mut merman::SourcePresentationConfig::default())
        .is_err()
    {
        return Err("未识别为 Mermaid 图，以源码展示".to_string());
    }

    let parser = SemanticParser::new().with_site_config(theme_config(colors));
    let renderer = Renderer::new().with_semantic_parser(parser);
    // **必须显式选 resvg-safe 管线。**
    //
    // `SvgRequest::default()` 的管线是 `Parity`：它保留 mermaid 原生的
    // `<foreignObject>` HTML 标签，那是给浏览器/WebView 用的。而 gpui 用
    // usvg 栅格化 SVG，usvg **不支持 foreignObject** —— 结果是所有标签文字
    // 静默消失，只剩空框（布局、连线、配色都正常，看起来像「文字被吃了」）。
    //
    // `resvg_safe` 会把 HTML 标签降级成 `<text>`/`<tspan>` 并清掉其它
    // usvg 不认的结构（非法数值属性、CSS 动画等）。
    // 回归用例：`svg_has_real_text_not_foreign_object`。
    let svg_request = merman::SvgRequest {
        pipeline: Some(merman::svg::SvgPipeline::resvg_safe()),
        ..Default::default()
    };
    let output = renderer
        .render(RenderRequest::svg(
            source,
            OperationControl::new(),
            svg_request,
        ))
        .map_err(|e| format!("Mermaid 渲染失败：{e}"))?;
    let RenderOutput::Svg(Some(svg)) = output else {
        return Err("未识别为 Mermaid 图，以源码展示".to_string());
    };
    let svg = svg.svg().to_string();

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
        source: &Arc<str>,
        colors: &MermaidColors,
        cx: &mut Context<LiveMarkdownView>,
    ) {
        // 指纹用 `begin_frame` 存下的那份：每块重算 15 字段哈希纯属浪费。
        // 查询键直接克隆调用方（geometry）缓存的 `Arc<str>`：只加引用计数，
        // 不再每帧把整块源码 `Arc::from` 拷贝一次。
        let key: MermaidKey = (source.clone(), self.theme_fp);
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
        // 源码直接共享 `Arc<str>`（引用计数），任务内 `&source_task` 退化为
        // `&str` 传给 `render_to_image`，也免掉一次整块拷贝。
        let source_task = source.clone();
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
    pub(crate) fn body(&self, source: &Arc<str>) -> MermaidBody {
        // 查表键克隆调用方的 `Arc<str>`（仅引用计数，不再 `Arc::from(source)`
        // 拷贝整块源码）；`Hash`/`Eq` 仍按指向的字符串内容比较，与插入时的
        // key 等价。
        let Some(entry) = self.entries.get(&(source.clone(), self.theme_fp)) else {
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

    /// 与 [`render_to_image`] 走同一套主题配置，只返回未美化的原始 SVG。
    ///
    /// 测试要断言「主题化之后长什么样」，所以不能直接用 `render_to_image`
    /// （它已经做了 accent 注入）；这里复刻渲染入口的主题参数。
    fn render_svg_for_test(source: &str, colors: &MermaidColors) -> String {
        let parser = merman::SemanticParser::new().with_site_config(theme_config(colors));
        let renderer = merman::Renderer::new().with_semantic_parser(parser);
        let out = renderer
            .render(merman::RenderRequest::svg(
                source,
                merman::OperationControl::new(),
                merman::SvgRequest {
                    pipeline: Some(merman::svg::SvgPipeline::resvg_safe()),
                    ..Default::default()
                },
            ))
            .expect("render ok");
        match out {
            merman::RenderOutput::Svg(Some(svg)) => svg.svg().to_string(),
            other => panic!("expected svg, got {other:?}"),
        }
    }

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

    /// 端到端：真实 merman 输出必须带上 deepterm 的暗色主题。
    ///
    /// 回归价值：mermaid 默认根背景是白色，暗色主题下刺眼；`themeVariables`
    /// 一旦没生效（拼错键名、被 `%%{init}%%` 覆盖），整张图会在深色卡片上
    /// 变成一块白板。
    #[test]
    fn real_output_uses_dark_theme_background() {
        let c = colors();
        let raw = render_svg_for_test(SAMPLE, &c);
        let canvas = css_hex(c.canvas);
        assert!(
            raw.contains(&canvas),
            "画布色 {canvas} 必须出现在输出里（themeVariables.background）"
        );
        // 不得残留 mermaid 默认白底。
        assert!(
            !raw.contains("background-color:white") && !raw.contains("background-color: white"),
            "根 svg 白底必须被主题覆盖：{:.400}",
            raw
        );
    }

    /// 回归：跨组连线密集的架构图必须**纵向为主**。
    ///
    /// 用户报告的是「本该上下堆叠的两个部分被拉成左右错开、连线绕成蜘蛛网」。
    /// 根因在 0.7 的 dagre 布局：它把同 rank 的节点横向排开，19 节点 / 13 条
    /// 跨组边的图会摊成 3115×967（宽高比 3.22）。0.11 换成分层折线布局后
    /// 同一张图是 1455×1701（0.86）。
    ///
    /// 判据钉在宽高比上而不是绝对尺寸：布局器调参不该让这条用例变红，
    /// 但**换回横向铺开的算法**必须变红。
    #[test]
    fn dense_architecture_graph_lays_out_vertically() {
        // 两个分组 + 密集跨组边（含反向），与用户那张图同构。
        let mut src = String::from("flowchart TB\n    subgraph host[\"宿主机\"]\n");
        for i in 0..10 {
            src.push_str(&format!("        H{i}[\"host 服务 {i}<br/>:8000\"]\n"));
        }
        src.push_str("    end\n    subgraph ext[\"外部依赖\"]\n");
        for i in 0..7 {
            src.push_str(&format!("        E{i}[\"外部 {i}\"]\n"));
        }
        src.push_str("    end\n");
        for i in 1..10 {
            src.push_str(&format!("    H{} --> H{i}\n", i - 1));
        }
        for i in 0..7 {
            src.push_str(&format!("    H{} --> E{i}\n", i % 10));
            src.push_str(&format!("    E{i} -.-> H{}\n", (i + 3) % 10));
        }

        let out = render_to_image(&src, &colors()).expect("应渲染成功");
        let ratio = out.width / out.height;
        assert!(
            ratio < 1.5,
            "架构图应纵向为主（宽高比 < 1.5），实际 {ratio:.2}（{}x{}）",
            out.width,
            out.height
        );
    }

    /// **标签文字必须真的出现在 SVG 里。**
    ///
    /// 回归：`SvgRequest::default()` 的管线是 `Parity`，它保留 mermaid 原生的
    /// `<foreignObject>` HTML 标签。gpui 用 usvg 栅格化，而 **usvg 不支持
    /// foreignObject** —— 图上所有文字静默消失，只剩空框（布局/连线/配色都对，
    /// 唯独没有字，极易被误判成「字体加载失败」）。
    ///
    /// 必须显式选 `ResvgSafe` 管线，它会把 HTML 标签降级成 `<text>`。
    #[test]
    fn svg_has_real_text_not_foreign_object() {
        let src = "flowchart TD\n    A[甲乙丙] --> B[丁戊己]";
        let c = colors();
        let svg = render_svg_for_test(src, &c);
        assert!(
            !svg.contains("<foreignObject"),
            "不得残留 foreignObject（usvg 不渲染它，文字会整段消失）"
        );
        assert!(svg.contains("<text"), "标签必须降级成 <text>：{:.400}", svg);
        assert!(svg.contains("甲乙丙"), "节点文案必须出现在 SVG 文本里");
    }

    /// `classDef` 必须真的生效：0.11 的布局/绘制支持样式类，旧版 0.7 不支持
    /// （所以当年才用 `accent_postprocess` 手工注入 `style="fill:…"`）。
    ///
    /// 现在改为断言 SVG 形状上带 `style` 且含 `stroke-width`——用户那张架构图
    /// 里 `public` / `external` / `state` 几个类的粗细与虚线全靠它。
    #[test]
    fn class_defs_survive_into_shape_styles() {
        let src = "flowchart TD\n    A[甲] --> B[乙]\n    classDef thick stroke-width:4px\n    class A thick";
        let c = colors();
        let raw = render_svg_for_test(src, &c);
        assert!(
            raw.contains("stroke-width"),
            "classDef 的 stroke-width 必须落到形状上：{:.400}",
            raw
        );
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
