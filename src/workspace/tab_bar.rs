use std::rc::Rc;
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;

use gpui::prelude::*;
use gpui::{
    Action, Animation, AnimationExt as _, AnyElement, App, Bounds, Context, Div, ElementId, Entity,
    FocusHandle, HighlightStyle, Hsla, IntoElement, MouseButton, Pixels, Point, Render,
    ScrollHandle, SharedString, Stateful, StyledText, TextRun, TextStyle, Transformation, Window,
    actions, div, ease_in_out, ease_out_quint, percentage, px, rgb, svg,
};

use crate::agent::{AgentStatus, CLIAgent};
use crate::assets::Icon;
use crate::components::menu::{ContextMenuExt, PopupMenu};
use crate::theme::{ActiveTheme, UiTheme};
use crate::workspace::panel::DraggedPanel;

actions!(
    tab_context_menu,
    [CopyTabAddress, DuplicateTab, ReconnectTab]
);

/// 以 tab id 为参数的回调（点击、关闭、复制地址、复制标签页、重连等）。
type TabCallback = Rc<dyn Fn(SharedString, &mut Window, &mut App) + Send + Sync>;
/// 可选 [`TabCallback`]。
type OptionalTabCallback = Option<TabCallback>;
/// 无参数回调（如新建标签页）。
type NewTabCallback = Rc<dyn Fn(&mut Window, &mut App) + Send + Sync>;
/// Tab 重排回调：参数为被拖拽的 tab 与插入位（before 语义）。
type TabReorderCallback = Rc<dyn Fn(DraggedTab, usize, &mut App) + Send + Sync>;
/// Panel 拖放到 TabBar 的回调：参数为被拖放的 panel 与可选的插入位。
type PanelDropCallback =
    Rc<dyn Fn(DraggedPanel, Option<usize>, &mut Window, &mut App) + Send + Sync>;
/// 右键菜单构建回调：接收一个空 `PopupMenu`，返回填充好菜单项的 `PopupMenu`。
type ContextMenuCallback =
    Rc<dyn Fn(SharedString, PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// 拖拽的 Tab 数据
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedTab {
    pub tab_id: SharedString,
    pub from_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabReorderPreview {
    dragged_tab: DraggedTab,
    /// 插入位（before 语义）：拖拽结果为「插入到重排前的第 insert_slot 个位置」，
    /// 取值范围 [0, tabs.len()]（等于 len 表示追加到末尾）。
    insert_slot: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelDropPreview {
    insert_index: usize,
    tab_label: SharedString,
}

/// sRGB 分量 → 线性光（0.04045 是 sRGB 传递函数的分段点）。
fn linearize(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB 相对亮度（WCAG 定义）：0 = 黑，1 = 白。
fn relative_luminance(color: gpui::Rgba) -> f32 {
    0.2126 * linearize(color.r) + 0.7152 * linearize(color.g) + 0.0722 * linearize(color.b)
}

/// WCAG 对比度：1.0 = 完全同色，21 = 纯黑对纯白。
fn contrast_ratio(a: gpui::Rgba, b: gpui::Rgba) -> f32 {
    let (x, y) = (relative_luminance(a), relative_luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// tty7 的圆盘边界规则：sRGB 相对亮度对比小于 1.25 时描边。
fn agent_disc_needs_edge(fill: u32, surface: gpui::Hsla) -> bool {
    contrast_ratio(rgb(fill), surface.into()) < 1.25
}

/// 两个颜色在 OKLab 空间的欧氏距离（≈ ΔE）；用于判断"能不能看出区别"。
///
/// 为什么不用亮度对比度（[`agent_disc_needs_edge`] 用的那套）：那个衡量的是
/// "前景在背景上够不够清楚"，对**色相**差异不敏感。两个等亮度但色相相反的颜色
/// 亮度比是 1.0，却一眼能分开——文字扫光正是这种情形（高亮色与基色同为前景，
/// 差异主要来自色相）。OKLab 是感知均匀空间，距离与人眼判断一致。
///
/// 经验刻度：0.02 刚可察觉，0.05 明显不同，0.15+ 差异强烈。
fn oklab_distance(a: gpui::Rgba, b: gpui::Rgba) -> f32 {
    let (l1, a1, b1) = to_oklab(a);
    let (l2, a2, b2) = to_oklab(b);
    ((l1 - l2).powi(2) + (a1 - a2).powi(2) + (b1 - b2).powi(2)).sqrt()
}

/// sRGB → OKLab（Björn Ottosson 的系数）。
fn to_oklab(c: gpui::Rgba) -> (f32, f32, f32) {
    let (r, g, b) = (linearize(c.r), linearize(c.g), linearize(c.b));
    let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    (
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    )
}

const TAB_REORDER_ANIMATION_DURATION_MS: u64 = 155;
const TAB_REORDER_GAP_PX: f32 = 4.0;
const TAB_REORDER_FALLBACK_WIDTH_PX: f32 = 160.0;
const TAB_REORDER_DRAGGED_TAB_OPACITY: f32 = 0.0;
const TAB_DRAG_PREVIEW_MIN_WIDTH_PX: f32 = 48.0;
const TAB_DRAG_PREVIEW_PADDING_X_PX: f32 = 12.0;
const TAB_MAX_WIDTH_PX: f32 = 180.0;
const TAB_HEIGHT_PX: f32 = 30.0;
const TAB_PADDING_X_PX: f32 = 12.0;
const TAB_RADIUS_PX: f32 = 8.0;
const ICON_SIZE_PX: f32 = 16.0;
const FILE_ICON_SIZE_PX: f32 = 22.0;
const AGENT_DOT_SIZE_PX: f32 = 6.0;

/// Working 状态标签文字的往返扫光：一个完整「去 + 回」的周期。
const AGENT_WORK_SHIMMER_PERIOD_MS: u64 = 2400;
/// 限制逐帧重绘频率；文字扫光对帧率不敏感，60→30 fps 省一半重排。
const AGENT_WORK_SHIMMER_MAX_FPS: f32 = 30.0;
const NEW_TAB_BUTTON_SIZE_PX: f32 = 32.0;

/// 拖拽预览视图
pub struct DragPreview {
    pub tab_label: SharedString,
    pub drag_offset: Point<Pixels>,
    pub preview_width: Pixels,
}

impl DragPreview {
    pub fn new(
        tab_label: SharedString,
        drag_offset: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self {
            preview_width: Self::measure_width(window, &tab_label, cx),
            tab_label,
            drag_offset,
        }
    }

    fn measure_width(window: &mut Window, label: &SharedString, cx: &mut App) -> Pixels {
        let font_size = px(14.0);

        let shaped_line = window.text_system().shape_line(
            label.clone(),
            font_size,
            &[TextRun {
                len: label.len(),
                font: TextStyle::default().font(),
                color: cx.theme().ui.text,
                ..Default::default()
            }],
            None,
        );

        px(
            (shaped_line.width.as_f32() + TAB_DRAG_PREVIEW_PADDING_X_PX * 2.0)
                .max(TAB_DRAG_PREVIEW_MIN_WIDTH_PX),
        )
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview_width = self.preview_width;
        let preview_height = px(30.0);
        let theme = cx.theme().ui;

        div()
            // 将预览中心对齐到鼠标位置
            .pl(self.drag_offset.x - preview_width / 2.0)
            .pt(self.drag_offset.y - preview_height / 2.0)
            .child(
                div()
                    .w(preview_width)
                    .h(preview_height)
                    .px(px(TAB_DRAG_PREVIEW_PADDING_X_PX))
                    .flex()
                    .items_center()
                    .rounded(px(8.0))
                    .bg(theme.elevated_background)
                    .border_1()
                    .border_color(theme.border_strong)
                    .opacity(0.96)
                    .shadow_md()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text)
                            .child(self.tab_label.clone()),
                    ),
            )
    }
}
/// Cosine envelope: strongest at the center, zero with a flat slope at both edges.
fn shimmer_intensity(distance: f32, radius: f32) -> f32 {
    let position = (distance.abs() / radius).min(1.0);
    (1.0 + (position * std::f32::consts::PI).cos()) * 0.5
}

/// 为**所有**字形生成高亮，而非只覆盖扫光带内的字形。
///
/// # 为什么不能只高亮带内字形
/// gpui 的 `shape_text` 只把「颜色等 decoration 完全相同」的相邻 run 合并进同一个
/// FontRun，而 FontRun 边界决定 CoreText 的分段 shaping（跨边界 kerning 丢失、
/// 相邻 run 用微差字号防连字）。若带外字形不带高亮，它们会合并成一段基色 run，
/// 扫光带每移动一帧段边界就跟着移动 → 每帧以不同边界重排 → 字形位置逐帧微漂，
/// 视觉上就是「标签宽度稳了，但里面的字还在动」。
///
/// 所以这里给每个字形都发一段高亮（带外 strength=0，颜色即基色），并给颜色加上
/// 交替的 ±ε 偏移，保证**相邻**字形颜色永不相等 → 每个字形恒为独立 run，分段
/// 只取决于字形边界、与帧无关 → 整个动画期间 shaping 结果恒定（LineLayoutCache
/// 键相同直接命中），文字纹丝不动。ε 比渲染时 8-bit 颜色的最小可辨差小约 40 倍，
/// 肉眼不可见。
fn shimmer_highlights<'a>(
    glyphs: &'a [(std::ops::Range<usize>, f32)],
    width: f32,
    phase: f32,
    base: gpui::Rgba,
    accent: gpui::Rgba,
) -> impl Iterator<Item = (std::ops::Range<usize>, HighlightStyle)> + 'a {
    // 亮核半径随标签宽度成比例——**不能**加上限。
    //
    // 相位是余弦往返，带中心的峰值速度 = π·width / period，即每帧位移
    // 随宽度线性增长；而亮核宽度就是 radius。一旦帧位移接近或超过亮核宽度，
    // 带就会「跳过」字形：某个字形从进入带内到离开，期间没有任何一帧采样到
    // 它的峰值，视觉上表现为部分字**根本不亮**。
    //
    // 实测（30fps、周期 2.4s，width=120px 时每帧位移 5.24px）：旧的
    // `.clamp(2.5, 6.0)` 把 radius 卡在 6px，帧位移/亮核 = 0.87，60% 的字
    // 达不到峰值；宽度 180px 时 47% 的字不亮。按 0.18·width 放开上限后，
    // 该比值恒定在 0.242（< 0.35 的临界值），任意宽度下都没有字被跳过。
    //
    // 下限 2.5px 仍然保留：极窄标签（<14px）若亮核小于 2.5px，扫光会退化成
    // 一根细线扫过，看不出「一道光」的形状。
    let radius = (width * 0.18).max(2.5);
    let center = phase * width;
    glyphs
        .iter()
        .enumerate()
        .map(move |(index, (range, position))| {
            let strength = shimmer_intensity(*position - center, radius);
            // 交替微调红色通道：仅用于阻止相邻同色字形被合并进同一个 FontRun，
            // 不是视觉效果的一部分。
            let nudge = if index % 2 == 0 { 1e-4 } else { -1e-4 };
            let color = gpui::Rgba {
                r: (base.r + (accent.r - base.r) * strength + nudge).clamp(0.0, 1.0),
                g: base.g + (accent.g - base.g) * strength,
                b: base.b + (accent.b - base.b) * strength,
                a: base.a,
            };
            (
                range.clone(),
                HighlightStyle {
                    color: Some(color.into()),
                    ..Default::default()
                },
            )
        })
}

/// 往返相位：0 → 1 → 0，两端由余弦自然减速。
fn shimmer_phase(t01: f32) -> f32 {
    (1.0 - (t01 * std::f32::consts::TAU).cos()) / 2.0
}

/// Only glyph foreground colors animate. Grapheme boundaries keep combining
/// marks and emoji together; display-cell positions account for wide CJK text.
/// The font, layout and background remain unchanged; colors outside the band
/// stay visually at the base color (see `shimmer_highlights` for why they
/// still carry an imperceptible per-glyph offset).
fn render_working_label(
    tab_id: &str,
    label: &SharedString,
    base_color: Hsla,
    highlight_color: Hsla,
) -> AnyElement {
    let id = format!("tab-shimmer-{tab_id}");
    let mut width = 0.0;
    let glyphs: Vec<_> = label
        .grapheme_indices(true)
        .map(|(start, glyph)| {
            let advance = unicode_width::UnicodeWidthStr::width(glyph) as f32;
            let center = width + advance * 0.5;
            width += advance;
            (start..start + glyph.len(), center)
        })
        .collect();
    let base = gpui::Rgba::from(base_color);
    let accent = gpui::Rgba::from(highlight_color);
    StyledText::new(label.clone())
        .with_animation(
            ElementId::Name(id.into()),
            Animation::new(Duration::from_millis(AGENT_WORK_SHIMMER_PERIOD_MS))
                .repeat()
                .with_max_fps(AGENT_WORK_SHIMMER_MAX_FPS),
            move |styled, delta| {
                styled.with_highlights(shimmer_highlights(
                    &glyphs,
                    width,
                    shimmer_phase(delta),
                    base,
                    accent,
                ))
            },
        )
        .into_any_element()
}

/// 标签页项
#[derive(Clone)]
pub struct TabItem {
    pub id: SharedString,
    pub label: SharedString,
    pub icon: Option<Icon>,
    pub is_modified: bool,
    pub is_remote: bool,
    pub is_loading: bool,
    /// 需要关注（后台 tab 的终端响了铃 / 发了通知）：显示小红点，
    /// tab 激活时自动清除（见 [`TabBar::set_active_tab`]）。
    pub needs_attention: bool,
    /// 前台 CLI agent 的回合状态（`Working` / `Waiting` / `Done`）。
    ///
    /// 只有 [`Self::icon`] 是 `Icon::AgentIcon` 时才有意义，但有意不做成
    /// 二选一的枚举：状态本身会频繁变化（`ToolComplete` 一个回合能触发几十次）
    /// 而 agent 身份通常不变，解耦之后 `set_tab_agent` 可以在身份不动时
    /// 只更新状态点，不必重建整个图标。
    ///
    /// `None` 表示「没有 agent」，此时不画状态点。
    pub agent_status: Option<AgentStatus>,
}

impl TabItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            is_modified: false,
            is_remote: false,
            is_loading: false,
            needs_attention: false,
            agent_status: None,
        }
    }

    pub fn with_loading(mut self, loading: bool) -> Self {
        self.is_loading = loading;
        self
    }

    pub fn with_modified(mut self, modified: bool) -> Self {
        self.is_modified = modified;
        self
    }

    pub fn with_icon(mut self, icon: Option<Icon>) -> Self {
        self.icon = icon;
        self
    }

    pub fn with_is_remote(mut self, is_remote: bool) -> Self {
        self.is_remote = is_remote;
        self
    }

    /// 当前 agent 的品牌色；不是 agent 图标时为 `None`。
    ///
    /// 品牌色已经存在 [`Icon::AgentIcon::accent_rgb`] 里（圆盘底色用的就是它），
    /// 这里只是把它取出来给文字扫光复用——**不**在渲染层重新查一遍
    /// `CLIAgent`，那样会多一个"图标是 A、颜色按 B 画"的错配来源。
    fn agent_accent_rgb(&self) -> Option<u32> {
        match self.icon {
            Some(Icon::AgentIcon { accent_rgb, .. }) => Some(accent_rgb),
            _ => None,
        }
    }
}

/// 纯重排核心：把 `dragged_id` 对应标签移动到「重排前列表的第 `insert_slot` 个插入位」。
/// 返回拖拽标签的重排前索引；无实际移动或标签不存在时返回 None。
fn apply_reorder(tabs: &mut Vec<TabItem>, dragged_id: &str, insert_slot: usize) -> Option<usize> {
    let from_index = tabs.iter().position(|t| t.id == dragged_id)?;
    let final_index = TabBar::final_index_for_slot(from_index, insert_slot);

    if from_index == final_index || final_index >= tabs.len() {
        return None;
    }

    let tab = tabs.remove(from_index);
    tabs.insert(final_index, tab);
    Some(from_index)
}

/// 中点判定（纯函数）：`content_x` 位于哪个 tab 中点左侧，就插入到哪个索引之前。
fn insert_slot_for_content_x(content_x: Pixels, tab_bounds: &[Bounds<Pixels>]) -> usize {
    for (index, bounds) in tab_bounds.iter().enumerate() {
        let midpoint = bounds.left() + bounds.size.width / 2.0;
        if content_x < midpoint {
            return index;
        }
    }

    tab_bounds.len()
}

/// 纯函数：重排预览期间第 `index` 个标签的水平位移。
/// 拖拽标签右移时，[from+1, to] 区间的标签左移让位；左移时 [to, from-1] 区间右移。
fn reorder_offset(from_index: usize, to_index: usize, index: usize, shift_width: f32) -> f32 {
    if to_index > from_index {
        if index > from_index && index <= to_index {
            -shift_width
        } else {
            0.0
        }
    } else if to_index < from_index {
        if index >= to_index && index < from_index {
            shift_width
        } else {
            0.0
        }
    } else {
        0.0
    }
}

/// agent 状态点的填充方式。
///
/// `Waiting` 必须与 `Done` 有**形状**差异，而不只是颜色差异：两者的颜色是
/// 琥珀与绿，正是红绿色觉最难分辨的一对，而这一对恰好是「要不要过去看一眼」
/// 的决定性问题。填充圆点与空心圆环在小尺寸下也能一眼分开。
/// 约束来源见 `AgentStatus::dot_rgb` 的注释（agent/cli_agent.rs）。
#[derive(Debug, PartialEq, Eq)]
enum AgentDot {
    Filled,
    /// 空心圆环：只描边，中心透出图标底色。
    Hollow,
}

/// 纯函数：`AgentStatus` → `(颜色, 填充方式)`；`None` 表示不画点。
///
/// 颜色从 `dot_rgb()` 取而不是写死在这里，保证与 agent 模块对状态颜色的定义
/// 不会各说各话。`Idle` 天然落到 `None`——agent 开着但没干活时图标本身
/// 已经说明了它的存在，再点一个点只是噪声。
fn agent_dot(status: AgentStatus) -> Option<(u32, AgentDot)> {
    let (rgb, dot) = match status {
        AgentStatus::Idle => return None,
        AgentStatus::Working => (status.dot_rgb()?, AgentDot::Filled),
        AgentStatus::Waiting => (status.dot_rgb()?, AgentDot::Hollow),
        AgentStatus::Done => (status.dot_rgb()?, AgentDot::Filled),
    };
    Some((rgb, dot))
}

/// 纯函数：标签行里要不要画「需关注」红点。
///
/// 抽出来是为了能直接测这个二选一逻辑——它此前写成内联的
/// `needs_attention && !is_modified`，加上 agent 那一维后条件变三条，
/// 内联形式只能靠读代码确认。
///
/// 两条规则：
/// - 没在等你看（`needs_attention == false`）→ 不画；
/// - 有未保存改动 → 让位给「未保存」灰点，两个点挤在一起会都看不清。
///
/// # 为什么 agent 有状态点时**不**让位
///
/// 曾经这里还有第三条：agent 状态点已画出时红点让位，理由是"两个点讲同一件事"。
/// 那条规则让本函数与 `workspace::should_mark_unread` 互相抵消——后者只在
/// `Working → Done` 边沿点亮红点，而那一刻 `agent_status` 必然是 `Done`
/// （状态点可见），于是红点**永远画不出来**，`needs_attention` 成了死标志。
///
/// 两者其实不重复，是两个维度：
/// - 图标上的状态点回答「agent 现在/最近在干什么」，**不会**因为用户看过而消失；
/// - 文字边的红点回答「你有没有看过这次完成」，**激活该 tab 即清除**
///   （见 [`TabBar::set_active_tab`]）。
///
/// 后台 tab 上"跑完了、你还没看"这件事正需要后者：图标很小、非激活时还是
/// 弱化的，状态点本身不足以把人叫回来。后台铃（`PanelEvent::BellRang`）用的是
/// 同一个标志、同一个红点，语义一致。
fn should_show_attention_dot(tab: &TabItem) -> bool {
    tab.needs_attention && !tab.is_modified
}

/// 标签栏组件
pub struct TabBar {
    id: ElementId,
    pub tabs: Vec<TabItem>,
    pub active_tab_id: Option<SharedString>,
    transparent_bg: bool,
    compact: bool,
    /// 本栏是否接受 agent 状态（图标 / 状态点 / 文字扫光）。
    ///
    /// 默认 `true`：终端栏是 agent 状态唯一的归属。编辑器栏由
    /// [`TabBar::set_accepts_agent_state`] 显式关掉，见
    /// [`TabBar::set_tab_agent`] 的「只作用于终端标签」一节。
    ///
    /// 有意**不**从 `compact` 推断：紧凑模式是视觉属性（编辑器栏恰好也用它
    /// 对齐高度），拿它当语义开关会让「以后给终端栏开紧凑模式」这种纯样式
    /// 改动悄悄关掉 agent 状态。
    accepts_agent_state: bool,
    on_tab_click: OptionalTabCallback,
    on_tab_close: OptionalTabCallback,
    on_new_tab: Option<NewTabCallback>,
    /// 重排回调：`insert_slot` 为重排前列表中的插入位（before 语义）。
    /// 回调触发时 TabBar 自身的 tabs 已完成重排，消费方只需同步自己的平行结构
    /// （如 editor 的 open_files），不要再对 tab_bar.tabs 重复应用。
    on_tab_reorder: Option<TabReorderCallback>,
    /// Panel 拖拽到 TabBar 的回调
    on_panel_drop: Option<PanelDropCallback>,
    focus_handle: FocusHandle,
    /// 滚动句柄，用于控制水平滚动；子元素 bounds（内容坐标）与视口
    /// （窗口坐标）均从此句柄读取
    scroll_handle: ScrollHandle,
    /// 当前 Tab 重排预览
    reorder_preview: Option<TabReorderPreview>,
    /// 上一次 Tab 重排预览，用于做补间动画
    previous_reorder_preview: Option<TabReorderPreview>,
    /// 重排动画序号
    reorder_animation_epoch: usize,
    /// 当前 Panel 拖入 TabBar 的插入预览
    panel_drop_preview: Option<PanelDropPreview>,
    /// 当前悬停的标签 ID
    hovered_tab_id: Option<SharedString>,
    /// 右键菜单的目标标签 ID
    context_menu_tab_id: Option<SharedString>,
    /// 拷贝地址回调
    on_tab_copy_address: OptionalTabCallback,
    /// 复制标签页回调
    on_tab_duplicate: OptionalTabCallback,
    /// 重新连接回调
    on_tab_reconnect: OptionalTabCallback,
    /// 右键菜单构建回调（由调用方设置，tab_id 为右键点击的标签 ID）
    on_context_menu: Option<ContextMenuCallback>,
}

impl TabBar {
    fn build(id: ElementId, focus_handle: FocusHandle, tabs: Vec<TabItem>) -> Self {
        let active_tab_id = tabs.first().map(|tab| tab.id.clone());
        Self {
            id,
            tabs,
            active_tab_id,
            transparent_bg: false,
            compact: false,
            // 默认接受：终端栏不显式设置，编辑器栏主动关掉。
            accepts_agent_state: true,
            on_tab_click: None,
            on_tab_close: None,
            on_new_tab: None,
            on_tab_reorder: None,
            on_panel_drop: None,
            hovered_tab_id: None,
            focus_handle,
            scroll_handle: ScrollHandle::new(),
            reorder_preview: None,
            previous_reorder_preview: None,
            reorder_animation_epoch: 0,
            panel_drop_preview: None,
            context_menu_tab_id: None,
            on_tab_copy_address: None,
            on_tab_duplicate: None,
            on_tab_reconnect: None,
            on_context_menu: None,
        }
    }

    pub fn new(id: impl Into<ElementId>, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self::build(id.into(), focus_handle, Vec::new()))
    }

    pub fn new_with_tabs(
        id: impl Into<ElementId>,
        cx: &mut App,
        tabs: Vec<TabItem>,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self::build(id.into(), focus_handle, tabs))
    }

    /// 编辑器悬浮面板使用透明背景以透出玻璃材质
    pub fn set_transparent_bg(&mut self, transparent: bool, cx: &mut Context<Self>) {
        if self.transparent_bg != transparent {
            self.transparent_bg = transparent;
            cx.notify();
        }
    }

    /// 编辑器紧凑模式：降低标签高度并微调内边距，使文字与终端标签对齐
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact != compact {
            self.compact = compact;
            cx.notify();
        }
    }

    /// 关掉/打开本栏的 agent 状态接收。
    ///
    /// 编辑器栏必须调用 `set_accepts_agent_state(false)`：它显示的是文件，
    /// 不是跑着 agent 的终端。见 [`TabBar::set_tab_agent`] 的「只作用于终端标签」。
    pub fn set_accepts_agent_state(&mut self, accepts: bool, cx: &mut Context<Self>) {
        if self.accepts_agent_state != accepts {
            self.accepts_agent_state = accepts;
            cx.notify();
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub fn clear_tabs(&mut self, cx: &mut Context<Self>) {
        self.tabs.clear();
        self.active_tab_id = None;
        self.context_menu_tab_id = None;
        cx.notify();
    }

    pub fn on_tab_click<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_click = Some(Rc::new(callback));
    }

    pub fn on_tab_close<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_close = Some(Rc::new(callback));
    }

    pub fn on_new_tab<F>(&mut self, callback: F)
    where
        F: Fn(&mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_new_tab = Some(Rc::new(callback));
    }

    /// 设置重排回调。`insert_slot` 为重排前列表中的插入位（before 语义），
    /// 取值范围 [0, tabs.len()]；TabBar 自身已同步完成重排。
    pub fn on_tab_reorder<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, usize, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_reorder = Some(Rc::new(callback));
    }

    /// 设置 Panel 拖拽到 TabBar 的回调
    pub fn on_panel_drop<F>(&mut self, callback: F)
    where
        F: Fn(DraggedPanel, Option<usize>, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_panel_drop = Some(Rc::new(callback));
    }

    /// 拷贝地址回调
    pub fn on_tab_copy_address<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_copy_address = Some(Rc::new(callback));
    }

    /// 复制标签页回调
    pub fn on_tab_duplicate<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_duplicate = Some(Rc::new(callback));
    }

    /// 重新连接回调
    pub fn on_tab_reconnect<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_reconnect = Some(Rc::new(callback));
    }

    /// 右键菜单构建回调（由调用方设置）
    pub fn on_context_menu<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    {
        self.on_context_menu = Some(Rc::new(callback));
    }

    /// 获取右键菜单的目标标签 ID
    pub fn context_menu_tab_id(&self) -> Option<SharedString> {
        self.context_menu_tab_id.clone()
    }

    /// 记录本次右键菜单归属的 tab（由菜单构建闭包调用，见 render_tab）。
    pub(crate) fn set_context_menu_tab(&mut self, tab_id: SharedString) {
        self.context_menu_tab_id = Some(tab_id);
    }

    pub fn add_tab(&mut self, tab: TabItem, cx: &mut Context<Self>) {
        let is_first = self.tabs.is_empty();
        self.tabs.push(tab);
        cx.notify();
        // 滚动到最新添加的标签（最右边）
        // 首个标签不要调用 scroll_to_item(0)，避免 ScrollHandle 未初始化时触发默认偏移
        if !is_first {
            let last_index = self.tabs.len().saturating_sub(1);
            self.scroll_handle.scroll_to_item(last_index);
        }
    }

    pub fn remove_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|t| t.id.as_ref() == id) {
            self.tabs.remove(index);

            // 更新活动标签
            if self.active_tab_id.as_ref().map(|s| s.as_ref()) == Some(id) {
                self.active_tab_id = if index < self.tabs.len() {
                    Some(self.tabs[index].id.clone())
                } else if !self.tabs.is_empty() {
                    Some(self.tabs[self.tabs.len() - 1].id.clone())
                } else {
                    None
                };
            }
        }

        // 右键菜单目标已不存在，避免后续 action 作用到过期 id
        if self.context_menu_tab_id.as_deref() == Some(id) {
            self.context_menu_tab_id = None;
        }
        cx.notify();
    }

    pub fn set_active_tab(&mut self, id: impl Into<SharedString>, cx: &mut Context<Self>) {
        let new_id: SharedString = id.into();
        let changed = self.active_tab_id.as_ref() != Some(&new_id);
        if changed {
            self.active_tab_id = Some(new_id);
            // 激活即清铃：后台响铃 / 通知产生的小红点，切过来后视为已读。
            if let Some(tab) = self
                .tabs
                .iter_mut()
                .find(|t| t.id == *self.active_tab_id.as_ref().unwrap())
            {
                tab.needs_attention = false;
            }
            // 溢出时把新激活的标签滚动到可见区域（最小滚动策略）
            if let Some(index) = self
                .active_tab_id
                .as_ref()
                .and_then(|id| self.tabs.iter().position(|t| t.id == *id))
            {
                self.scroll_handle.scroll_to_item(index);
            }
            cx.notify();
        }
    }

    /// 原地修改单个标签，避免重建整个 tabs Vec
    pub fn modify_tab(
        &mut self,
        tab_id: &str,
        f: impl FnOnce(&mut TabItem),
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            f(tab);
            cx.notify();
        }
    }

    /// 更新标签的 agent 图标与回合状态点，返回该标签**原先**的状态。
    ///
    /// 返回值是改之前的状态，而不是 `bool`：调用方需要判断
    /// `Working → Done` 这个**边沿**来决定要不要点亮「需关注」红点。只看
    /// 「现在是不是 Done」会在每次 `Stop` 重复点亮，也会在切回 tab 后
    /// 因为残留的 `Done` 反复点亮。
    ///
    /// `agent` 为 `None`（agent 已退出）时图标回落到 `fallback`，由调用方
    /// 决定「这台 tab 本来该显示什么」——`TabBar` 无从知道该 tab 是
    /// Workspace 还是连了哪台远端机器。
    ///
    /// 状态与图标都没变时直接返回、不 `cx.notify()`：`ToolComplete` 会在
    /// 一个回合里触发几十次，而它多数时候只是把 `Working` 写成 `Working`。
    ///
    /// `tab_id` 不存在时返回 `None`。
    ///
    /// # 只作用于终端标签
    ///
    /// agent 状态只属于**跑着 agent 的终端**。编辑器标签（同一窗口里的
    /// 另一个 `TabBar`）不该出现 agent 图标、状态点或扫光——那里显示的是
    /// 文件名与保存状态，混进 agent 身份只会让人误以为「这个文件在跑 agent」。
    ///
    /// 这条边界由 [`TabBar::accepts_agent_state`] 保证，而不是靠调用方自觉：
    /// 编辑器栏显式关掉接收后，本方法直接返回、不改动任何标签。有意**不**
    /// 断言失败——「编辑器不显示 agent 状态」正是期望行为，不是需要崩溃的错误。
    pub fn set_tab_agent(
        &mut self,
        tab_id: &str,
        agent: Option<CLIAgent>,
        status: AgentStatus,
        fallback: Option<Icon>,
        cx: &mut Context<Self>,
    ) -> Option<AgentStatus> {
        if !self.accepts_agent_state {
            return None;
        }

        let tab = self.tabs.iter_mut().find(|t| t.id == tab_id)?;

        // 没有 agent 就没有状态点，也不必记住状态：留着会让「agent 退出后
        // 又启动另一个 agent」这一串事件里出现一个陈旧的 Done。
        let status = agent.is_some().then_some(status);
        let icon = match agent {
            Some(agent) => Some(Icon::AgentIcon {
                path: agent.icon_path(),
                rgb: agent.icon_rgb(),
                accent_rgb: agent.accent_rgb(),
            }),
            None => fallback,
        };

        let previous = tab.agent_status;
        if tab.agent_status == status && tab.icon == icon {
            return previous;
        }

        tab.agent_status = status;
        tab.icon = icon;

        cx.notify();
        previous
    }

    pub fn active_tab_id(&self) -> Option<&SharedString> {
        self.active_tab_id.as_ref()
    }

    fn handle_tab_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        if !self.tab_list_viewport().contains(&position) {
            self.clear_tab_reorder_preview(cx);
            return;
        }

        let dragged = event.drag(cx).clone();
        // 拖拽源可能来自另一个 TabBar（同窗口内终端栏与编辑器栏并存），
        // 只对本栏存在的标签生成重排预览
        if !self.tabs.iter().any(|t| t.id == dragged.tab_id) {
            self.clear_tab_reorder_preview(cx);
            return;
        }

        let insert_slot = self.tab_insert_slot_for_position(position);
        self.set_tab_reorder_preview(&dragged, insert_slot, cx);
    }

    fn handle_panel_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedPanel>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        if !self.tab_list_viewport().contains(&position) {
            self.clear_panel_drop_preview(cx);
            return;
        }

        let dragged: &DraggedPanel = event.drag(cx);
        let insert_index = self.tab_insert_slot_for_position(position);
        self.set_panel_drop_preview(insert_index, dragged.tab_label.clone(), cx);
    }

    /// 滚动容器视口（窗口坐标），拖拽命中判定用
    fn tab_list_viewport(&self) -> Bounds<Pixels> {
        self.scroll_handle.bounds()
    }

    /// 各 tab 的布局边界（内容坐标，不含滚动偏移），
    /// 直接读取 ScrollHandle 记录的子元素 bounds。
    /// 滚动容器的子元素 = 各 tab（+ 可选的 panel 插入预览，需跳过）。
    fn current_tab_bounds(&self) -> Vec<Bounds<Pixels>> {
        let preview_index = self.panel_drop_preview.as_ref().map(|p| p.insert_index);
        let child_count = self.tabs.len() + usize::from(preview_index.is_some());
        let mut bounds = Vec::with_capacity(self.tabs.len());
        for child_index in 0..child_count {
            if Some(child_index) == preview_index {
                continue;
            }
            if let Some(tab_bounds) = self.scroll_handle.bounds_for_item(child_index) {
                bounds.push(tab_bounds);
            }
        }
        bounds
    }

    fn set_tab_reorder_preview(
        &mut self,
        dragged_tab: &DraggedTab,
        insert_slot: usize,
        cx: &mut Context<Self>,
    ) {
        let next_preview = TabReorderPreview {
            dragged_tab: dragged_tab.clone(),
            insert_slot: insert_slot.min(self.tabs.len()),
        };

        if self.reorder_preview.as_ref() == Some(&next_preview) {
            return;
        }

        self.panel_drop_preview = None;
        self.previous_reorder_preview = self.reorder_preview.clone();
        self.reorder_preview = Some(next_preview);
        self.reorder_animation_epoch += 1;
        cx.notify();
    }

    fn clear_tab_reorder_preview(&mut self, cx: &mut Context<Self>) {
        if self.reorder_preview.is_none() {
            return;
        }

        self.previous_reorder_preview = self.reorder_preview.take();
        self.reorder_animation_epoch += 1;
        cx.notify();
    }

    fn set_panel_drop_preview(
        &mut self,
        insert_index: usize,
        tab_label: SharedString,
        cx: &mut Context<Self>,
    ) {
        let next_preview = PanelDropPreview {
            insert_index: insert_index.min(self.tabs.len()),
            tab_label,
        };

        if self.panel_drop_preview.as_ref() == Some(&next_preview) {
            return;
        }

        self.reorder_preview = None;
        self.previous_reorder_preview = None;
        self.panel_drop_preview = Some(next_preview);
        cx.notify();
    }

    fn clear_panel_drop_preview(&mut self, cx: &mut Context<Self>) {
        if self.panel_drop_preview.is_none() {
            return;
        }

        self.panel_drop_preview = None;
        cx.notify();
    }

    fn reset_tab_reorder_preview(&mut self, cx: &mut Context<Self>) {
        if self.reorder_preview.is_none() && self.previous_reorder_preview.is_none() {
            return;
        }

        self.reorder_preview = None;
        self.previous_reorder_preview = None;
        self.reorder_animation_epoch += 1;
        cx.notify();
    }

    fn reset_panel_drop_preview(&mut self, cx: &mut Context<Self>) {
        if self.panel_drop_preview.is_none() {
            return;
        }

        self.panel_drop_preview = None;
        cx.notify();
    }

    /// 将「原列表插入位」换算为 remove+insert 后的最终索引。
    /// insert_slot > from 时插入位在拖拽标签之后，移除后目标元素左移一位。
    fn final_index_for_slot(from_index: usize, insert_slot: usize) -> usize {
        if insert_slot > from_index {
            insert_slot - 1
        } else {
            insert_slot
        }
    }

    /// 就地应用重排：`insert_slot` 为重排前列表中的插入位（before 语义）。
    /// 返回拖拽标签的重排前索引；无实际移动或标签不存在时返回 None。
    fn apply_tab_reorder(&mut self, dragged_tab: &DraggedTab, insert_slot: usize) -> Option<usize> {
        apply_reorder(&mut self.tabs, &dragged_tab.tab_id, insert_slot)
    }

    /// 中点判定：指针位于哪个 tab 中点左侧，就插入到哪个索引之前。
    /// 位置为窗口坐标，需换算到内容坐标后与布局 bounds 比较。
    fn tab_insert_slot_for_position(&self, position: Point<Pixels>) -> usize {
        let offset = self.scroll_handle.offset();
        let content_x = position.x - offset.x;
        insert_slot_for_content_x(content_x, &self.current_tab_bounds())
    }

    fn reorder_preview_has_effect(preview: &TabReorderPreview, tab_count: usize) -> bool {
        let from_index = preview.dragged_tab.from_index;
        if from_index >= tab_count {
            return false;
        }

        let final_index = Self::final_index_for_slot(from_index, preview.insert_slot);
        final_index < tab_count && final_index != from_index
    }

    fn dragged_tab_shift_width(&self, preview: &TabReorderPreview) -> f32 {
        self.current_tab_bounds()
            .get(preview.dragged_tab.from_index)
            .map(|bounds| {
                let width: f32 = bounds.size.width.into();
                width + TAB_REORDER_GAP_PX
            })
            .unwrap_or(TAB_REORDER_FALLBACK_WIDTH_PX)
    }

    fn tab_offset_for_preview(&self, preview: Option<&TabReorderPreview>, index: usize) -> f32 {
        let Some(preview) = preview else {
            return 0.0;
        };

        if !Self::reorder_preview_has_effect(preview, self.tabs.len()) {
            return 0.0;
        }

        let from_index = preview.dragged_tab.from_index;
        let to_index = Self::final_index_for_slot(from_index, preview.insert_slot);
        let shift = self.dragged_tab_shift_width(preview);
        reorder_offset(from_index, to_index, index, shift)
    }

    fn tab_opacity_for_preview(&self, preview: Option<&TabReorderPreview>, index: usize) -> f32 {
        let Some(preview) = preview else {
            return 1.0;
        };

        if Self::reorder_preview_has_effect(preview, self.tabs.len())
            && index == preview.dragged_tab.from_index
        {
            TAB_REORDER_DRAGGED_TAB_OPACITY
        } else {
            1.0
        }
    }
}

crate::impl_focusable!(TabBar);

impl TabBar {
    /// 右键菜单动作的统一监听器：取用并清除 `context_menu_tab_id`，
    /// 避免过期 id 被后续 action 误用。
    fn context_menu_action_listener<A: Action>(
        cx: &Context<Self>,
        callback: OptionalTabCallback,
    ) -> impl Fn(&A, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _: &A, window, cx| {
            let tab_id = this.context_menu_tab_id.take();
            if let (Some(callback), Some(tab_id)) = (callback.as_ref(), tab_id) {
                callback(tab_id, window, cx);
            }
        })
    }

    /// 标签列表滚动容器：拖拽/落点处理 + 标签与插入预览子元素
    fn render_tab_list(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let on_tab_reorder = self.on_tab_reorder.clone();
        let on_panel_drop = self.on_panel_drop.clone();
        let panel_drop_preview = self.panel_drop_preview.clone();
        let theme = cx.theme().ui;

        let mut children: Vec<AnyElement> =
            Vec::with_capacity(self.tabs.len() + usize::from(panel_drop_preview.is_some()));
        for index in 0..=self.tabs.len() {
            if let Some(preview) = panel_drop_preview
                .as_ref()
                .filter(|preview| preview.insert_index == index)
            {
                children.push(Self::render_panel_drop_preview(theme, preview));
            }
            if let Some(tab) = self.tabs.get(index) {
                children.push(self.render_tab(tab, index, cx));
            }
        }

        div()
            .id("tab-list-container")
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h_full()
            .on_drag_move::<DraggedPanel>(cx.listener(|this, event, _window, cx| {
                this.handle_panel_drag_move(event, cx);
            }))
            .on_drag_move::<DraggedTab>(cx.listener(|this, event, _window, cx| {
                this.handle_tab_drag_move(event, cx);
            }))
            // 拖拽重排：由 drag_move 维护的中点插入位直接应用。
            // 抖动兜底：GPUI 的拖拽启动阈值只有 2px（逻辑像素），在 1x 外接屏上
            // 等于 2 个物理像素，手部微抖就会把一次点击变成拖拽——on_click 被
            // 取消，而落点仍在原位、重排又是 no-op，看起来就是“点了没反应”
            // （还会短暂闪出跟随鼠标的拖拽预览）。此时落点无实际位移，等价于
            // 一次点击，直接补发 on_tab_click。
            .on_drop(cx.listener({
                let on_tab_reorder = on_tab_reorder.clone();
                let on_tab_click = self.on_tab_click.clone();
                move |this, dragged: &DraggedTab, window, cx| {
                    cx.stop_propagation();

                    let insert_slot = this
                        .reorder_preview
                        .as_ref()
                        .filter(|preview| preview.dragged_tab.tab_id == dragged.tab_id)
                        .map(|preview| preview.insert_slot);

                    // 只对本栏仍存在的标签补点击：外来 TabBar 的拖拽、已关闭的
                    // 标签一律放行，避免把未知 id 喂给 on_tab_click。
                    let is_own_tab = this.tabs.iter().any(|t| t.id == dragged.tab_id);
                    let fire_click_as_fallback = |window: &mut Window, cx: &mut Context<TabBar>| {
                        if is_own_tab && let Some(ref on_click) = on_tab_click {
                            on_click(dragged.tab_id.clone(), window, cx);
                        }
                    };

                    if let Some(insert_slot) = insert_slot {
                        let did_reorder = this.apply_tab_reorder(dragged, insert_slot);
                        this.reset_tab_reorder_preview(cx);

                        if let Some(ref callback) = on_tab_reorder
                            && did_reorder.is_some()
                        {
                            callback(dragged.clone(), insert_slot, cx);
                        } else if did_reorder.is_none() {
                            fire_click_as_fallback(window, cx);
                        }
                    } else {
                        this.clear_tab_reorder_preview(cx);
                        // 拖拽启动后没有任何 drag_move（抖动极小、按下后原地释放）：
                        // 同样视为点击。
                        fire_click_as_fallback(window, cx);
                    }
                }
            }))
            .on_drop(cx.listener({
                let on_panel_drop = on_panel_drop.clone();
                move |this, dragged: &DraggedPanel, window, cx| {
                    let insert_index = this
                        .panel_drop_preview
                        .as_ref()
                        .map(|preview| preview.insert_index);

                    this.reset_panel_drop_preview(cx);
                    cx.stop_propagation();

                    if let Some(ref callback) = on_panel_drop {
                        callback(dragged.clone(), insert_index, window, cx);
                    }
                }
            }))
            .flex_1()
            .overflow_x_scroll()
            .track_scroll(&self.scroll_handle)
            .children(children)
    }

    /// 渲染单个标签（含重排补间动画包装）
    fn render_tab(&self, tab: &TabItem, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().ui;
        let tab_id = tab.id.clone();
        let is_active = self.active_tab_id.as_ref() == Some(&tab.id);
        let label = tab.label.clone();

        let start_offset =
            self.tab_offset_for_preview(self.previous_reorder_preview.as_ref(), index);
        let end_offset = self.tab_offset_for_preview(self.reorder_preview.as_ref(), index);
        let start_opacity =
            self.tab_opacity_for_preview(self.previous_reorder_preview.as_ref(), index);
        let end_opacity = self.tab_opacity_for_preview(self.reorder_preview.as_ref(), index);

        let tab_height = if self.compact {
            px(24.0)
        } else {
            px(TAB_HEIGHT_PX)
        };
        let (border, bg, hover_bg) = if self.compact {
            (theme.transparent(), gpui::transparent_black(), None)
        } else if is_active {
            (theme.tab_active_border, theme.tab_active_background, None)
        } else {
            (
                theme.transparent(),
                theme.tab_inactive_background,
                Some(theme.tab_hover_background),
            )
        };
        // 标签底色：扫光文字要在它上面可见（见 `agent_sweep_color`）。compact
        // 模式下标签自身透明，文字实际压在标签栏背景上，所以按标签栏取。
        let surface = if self.compact {
            theme.tab_bar_background
        } else if is_active {
            theme.tab_active_background
        } else {
            theme.tab_inactive_background
        };
        let tab_inner = div()
            .id(tab_id.clone())
            .h(tab_height)
            .px(px(TAB_PADDING_X_PX))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.0))
            .rounded(px(TAB_RADIUS_PX))
            .border_1()
            .border_color(border)
            .bg(bg)
            .hover(move |style| match hover_bg {
                Some(hover) => style.bg(hover),
                None => style,
            })
            .on_hover(cx.listener({
                let tab_id = tab_id.clone();
                move |this, hovered: &bool, _window, cx| {
                    this.hovered_tab_id = hovered.then(|| tab_id.clone());
                    cx.notify();
                }
            }))
            .on_drag(
                DraggedTab {
                    tab_id: tab_id.clone(),
                    from_index: index,
                },
                {
                    let label = label.clone();
                    move |_drag_data, position, window, cx| {
                        cx.stop_propagation();
                        let preview = DragPreview::new(label.clone(), position, window, cx);
                        cx.new(move |_cx| preview)
                    }
                },
            )
            .child(self.render_tab_icon_slot(tab, &tab_id, is_active, cx))
            .child(Self::render_tab_label(tab, is_active, theme, surface))
            .on_click(cx.listener({
                let on_tab_click = self.on_tab_click.clone();
                let tab_id = tab_id.clone();
                move |_this, _event: &gpui::ClickEvent, window, cx| {
                    if let Some(ref callback) = on_tab_click {
                        callback(tab_id.clone(), window, cx);
                    }
                }
            }))
            // 左键：阻止冒泡到 TitleBar 触发窗口拖拽
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            // 中键关闭标签（浏览器/终端通用习惯）
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener({
                    let on_tab_close = self.on_tab_close.clone();
                    let tab_id = tab_id.clone();
                    move |_this, _event, window, cx| {
                        if let Some(ref callback) = on_tab_close {
                            callback(tab_id.clone(), window, cx);
                        }
                    }
                }),
            )
            // 注意：这里曾经有个手动 Right 监听器负责记录被右键的 tab，
            // 但 ContextMenu 包装器在 window 层先收到右键并 stop_propagation，
            // 行内监听器永远收不到——记录点已搬进下方构建闭包（自带 tab_id）。
            .context_menu(format!("tab-menu-{}", tab_id), {
                let tab_id = tab_id.clone();
                let on_context_menu = self.on_context_menu.clone();
                // 注意：ContextMenu 包装器在 window 层先收到右键并 stop_propagation，
                // 上面的行内 Right 监听器收不到事件——这里是唯一可靠的记录点：
                // 构建闭包自带本次右键的 tab_id（defer 一帧后执行，无竞态）。
                let tabs = cx.entity();
                move |menu: PopupMenu, window, cx| {
                    tabs.update(cx, |bar, _| {
                        bar.context_menu_tab_id = Some(tab_id.clone());
                    });
                    if let Some(ref cb) = on_context_menu {
                        cb(tab_id.clone(), menu, window, cx)
                    } else {
                        menu
                    }
                }
            });

        let needs_animation = start_offset != end_offset || start_opacity != end_opacity;
        if needs_animation {
            let animation_id = format!("tab-reorder-{}-{}", tab_id, self.reorder_animation_epoch);

            div()
                .child(tab_inner)
                .flex_shrink_0()
                .with_animation(
                    animation_id,
                    Animation::new(Duration::from_millis(TAB_REORDER_ANIMATION_DURATION_MS))
                        .with_easing(ease_out_quint()),
                    move |this, delta| {
                        let offset = start_offset + (end_offset - start_offset) * delta;
                        let opacity = start_opacity + (end_opacity - start_opacity) * delta;
                        this.left(px(offset)).opacity(opacity)
                    },
                )
                .into_any_element()
        } else {
            tab_inner.into_any_element()
        }
    }

    /// 标签的图标/关闭按钮/loading 槽位。
    /// 关闭按钮不依赖 icon 存在：hover 时统一显示关闭按钮，
    /// 其优先于 loading 动画，慢速远程连接也能从标签上直接关闭。
    fn render_tab_icon_slot(
        &self,
        tab: &TabItem,
        tab_id: &SharedString,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().ui;
        let icon = tab.icon;
        let is_hovered = self.hovered_tab_id.as_ref() == Some(tab_id);

        let icon_slot_width = if icon.is_some_and(|i| matches!(i, Icon::FileIcon(_))) {
            px(FILE_ICON_SIZE_PX)
        } else {
            px(ICON_SIZE_PX)
        };

        let content = if is_hovered {
            let on_tab_close = self.on_tab_close.clone();
            let tab_id_close = tab_id.clone();
            svg()
                .size(px(ICON_SIZE_PX))
                .cursor_pointer()
                .text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted
                })
                .path(Icon::XmarkCircleFill.path())
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |_this, _event, window, cx| {
                        if let Some(ref callback) = on_tab_close {
                            callback(tab_id_close.clone(), window, cx);
                        }
                    }),
                )
                .into_any_element()
        } else if tab.is_loading {
            svg()
                .size(px(ICON_SIZE_PX))
                .text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted
                })
                .path(Icon::Loader.path())
                .with_animation(
                    format!("tab-loading-{}", tab_id),
                    Animation::new(Duration::from_millis(800))
                        .repeat()
                        .with_easing(ease_in_out),
                    |this, delta| {
                        this.with_transformation(Transformation::rotate(percentage(delta)))
                    },
                )
                .into_any_element()
        } else if let Some(Icon::AgentIcon {
            path,
            rgb,
            accent_rgb,
        }) = icon
        {
            // agent 图标压过一切：跑着 agent 的 tab，「这是哪个 agent、它现在
            // 在干嘛」永远比「什么类型的 panel」更重要。这个分支必须排在下面
            // 的通用 icon 分支之前——`AgentIcon` 也是一个 `Icon`。
            let surface = if self.compact {
                theme.tab_bar_background
            } else if is_active {
                theme.tab_active_background
            } else {
                theme.tab_inactive_background
            };
            Self::render_agent_icon_slot(
                path,
                rgb,
                accent_rgb,
                tab.agent_status,
                surface,
                theme.border,
            )
        } else if let Some(icon) = icon {
            let icon_size = if matches!(icon, Icon::FileIcon(_)) {
                px(FILE_ICON_SIZE_PX)
            } else {
                px(ICON_SIZE_PX)
            };
            svg()
                .size(icon_size)
                .text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted
                })
                .path(icon.path())
                .into_any_element()
        } else {
            div().into_any_element()
        };

        div()
            .w(icon_slot_width)
            .flex()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }

    /// agent 图标 + 右下角状态点。
    ///
    /// 与 tty7 一致：品牌色圆盘承载单色剪影，不能把圆盘色当成图形色。
    ///
    /// 状态点用 `absolute()` 压在图标右下角：不占布局空间，所以图标仍然
    /// 居中在自己的 16px 槽位里，有没有状态点都不会让标签左右抖动。
    fn render_agent_icon_slot(
        path: &'static str,
        rgb_value: u32,
        accent_rgb: u32,
        status: Option<AgentStatus>,
        surface: gpui::Hsla,
        border: gpui::Hsla,
    ) -> AnyElement {
        let dot = status.and_then(agent_dot);

        div()
            .relative()
            .size(px(ICON_SIZE_PX))
            .child(
                div()
                    .size(px(ICON_SIZE_PX))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(accent_rgb))
                    .when(agent_disc_needs_edge(accent_rgb, surface), |disc| {
                        disc.border_1().border_color(border)
                    })
                    .child(
                        svg()
                            .size(px(ICON_SIZE_PX * 0.54))
                            .text_color(rgb(rgb_value))
                            .path(path),
                    ),
            )
            .when_some(dot, |this, (dot_rgb_value, shape)| {
                this.child(
                    div()
                        .absolute()
                        .right_0()
                        .bottom_0()
                        .size(px(AGENT_DOT_SIZE_PX))
                        .rounded_full()
                        // Hollow：只描边、中心透明，与实心点形成形状差异。
                        // 描边宽度取 1px——6px 的环再粗就糊成实心了。
                        .when(shape == AgentDot::Filled, |this| {
                            this.bg(rgb(dot_rgb_value))
                        })
                        .when(shape == AgentDot::Hollow, |this| {
                            this.border_1().border_color(rgb(dot_rgb_value))
                        }),
                )
            })
            .into_any_element()
    }

    /// 标签文字扫光的高亮色：当前 agent 的**品牌色**。
    ///
    /// # 为什么要做可见性兜底
    ///
    /// 品牌色里有一批是近黑或近白（Codex / Grok / Kimi / OpenCode / Auggie 是黑，
    /// Cursor 是浅灰）。它们与文字**基色**撞色时，扫光扫过去什么都看不见。
    ///
    /// # 为什么用感知距离而不是亮度对比
    ///
    /// 亮度对比度是给"前景 vs 背景"判断可读性的，用在"两个前景色能否区分"
    /// 上会严重误判：solarized-dark 的文字 `#839496` 与 Claude 橙 `#D97757`
    /// 亮度比只有 1.01（看起来"隐形"），但两者色相迥异，感知距离 0.151，
    /// **肉眼一眼可辨**。若按亮度判，21/23 个 agent 都会被无谓地回落掉。
    ///
    /// 所以这里用 OKLab 的欧氏距离（≈ ΔE）：它对色相差异敏感，与人眼判断一致。
    /// 阈值 0.05 取自"明显不同"的下限（刚可察觉约 0.02）。
    ///
    /// # 为什么还要查一次背景
    ///
    /// "与基色分得开"只是必要条件：扫光是把**文字**染成品牌色，所以品牌色还得
    /// 在标签底色上看得见。只看基色会放过一整类反例——浅色主题的标签底色接近
    /// 纯白（github-light 就是 `#FFFFFF`），黑色品牌色与正文分得很开，扫光扫过去
    /// 却直接压进底色里，**文字整段消失**。
    ///
    /// 两条判据都过才用品牌色，否则回落到主题 accent。实测 23 个 agent ×
    /// 31 个主题 × 2 种激活态共 1426 个组合：8 个 agent 会在部分主题下回落
    /// （品牌色越接近黑白，回落越多），其余组合全部保留品牌色。回落目标本身
    /// 始终安全：accent 对正文的最小感知距离 0.051、对底色最小对比度 1.63。
    fn agent_sweep_color(
        brand_rgb: u32,
        base: gpui::Hsla,
        surface: gpui::Hsla,
        accent: gpui::Hsla,
    ) -> gpui::Hsla {
        let brand = rgb(brand_rgb);
        let distinguishable = oklab_distance(brand, base.into()) >= 0.05;
        let visible_on_surface = contrast_ratio(brand, surface.into()) >= 1.25;
        if distinguishable && visible_on_surface {
            brand.into()
        } else {
            accent
        }
    }

    /// 标签文本（含未保存 / 需关注圆点），非激活时限宽截断
    ///
    /// 两个圆点是**两个维度**，可以同时出现：
    /// - 灰点 = 文件有未保存改动（编辑器专属语义）；
    /// - 红点 = 「需关注」——后台 tab 响了铃，或 agent 跑完了一个回合而你还没
    ///   看过它（激活即清除）。判定见 [`should_show_attention_dot`]。
    ///
    /// 红点**不**因为图标上有 agent 状态点而让位：那个点说的是"agent 在干什么"，
    /// 不会因为你切过去看过就消失；红点说的是"这次完成你还没看"。前者是状态，
    /// 后者是未读，两者不重复。详见 [`should_show_attention_dot`] 的文档。
    fn render_tab_label(
        tab: &TabItem,
        is_active: bool,
        theme: UiTheme,
        surface: gpui::Hsla,
    ) -> Div {
        let show_attention = should_show_attention_dot(tab);

        div()
            .relative()
            .overflow_hidden()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.0))
            .when(!is_active, |this| this.max_w(px(TAB_MAX_WIDTH_PX)))
            .when(tab.is_modified, |this| {
                this.child(
                    div()
                        .size(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(if is_active {
                            theme.text_muted
                        } else {
                            theme.text_muted.opacity(0.7)
                        }),
                )
            })
            // 需关注（后台铃 / 通知）：更醒目的强调色圆点，激活后清除。
            .when(show_attention, |this| {
                this.child(
                    div()
                        .size(px(7.0))
                        .flex_none()
                        .rounded_full()
                        .bg(theme.error),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .whitespace_nowrap()
                    .font_weight(if is_active {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if is_active {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    // 宽度基准（ghost sizer）：这里必须永远渲染一份**普通文本**。
                    // 扫光动画每帧都会改变高亮分段，gpui 会按分段重新 shaping，
                    // 行宽在两个整像素值之间逐帧振荡；若让扫光文本直接待在文档流里，
                    // tab（flex_shrink_0 自适应内容）就会跟着一帧一抖。
                    // 普通文本是单 run 排版，宽度逐帧恒定，由它撑住 tab 宽度；
                    // Working 时只把它隐形，扫光以覆盖层形式叠在上面，不参与布局。
                    // text_ellipsis 挂在 sizer 上而不是外层容器：避免覆盖层里的
                    // 扫光文本继承截断样式，被逐帧重排出的亚像素宽度差误触发截断。
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .when(tab.agent_status == Some(AgentStatus::Working), |this| {
                                this.opacity(0.)
                            })
                            .child(tab.label.clone()),
                    )
                    .when(tab.agent_status == Some(AgentStatus::Working), |this| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .child(render_working_label(
                                    &tab.id,
                                    &tab.label,
                                    if is_active {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    },
                                    // 扫光高亮用当前 agent 的品牌色，而不是主题
                                    // 强调色：文字在动的时候顺带说明"是哪个 agent
                                    // 在干活"。品牌色与正文或标签底色撞色时由
                                    // `agent_sweep_color` 回落到主题 accent。
                                    Self::agent_sweep_color(
                                        tab.agent_accent_rgb().unwrap_or(0),
                                        if is_active {
                                            theme.text
                                        } else {
                                            theme.text_muted
                                        },
                                        surface,
                                        theme.accent,
                                    ),
                                )),
                        )
                    }),
            )
    }

    /// Panel 拖入时的插入位置预览
    fn render_panel_drop_preview(theme: UiTheme, preview: &PanelDropPreview) -> AnyElement {
        div()
            .id(format!("panel-drop-preview-{}", preview.insert_index))
            .h(px(TAB_HEIGHT_PX))
            .px(px(TAB_PADDING_X_PX))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.0))
            .rounded(px(TAB_RADIUS_PX))
            .border_1()
            .border_color(theme.border_strong.opacity(0.55))
            .bg(theme.tab_hover_background.opacity(0.6))
            .opacity(0.75)
            .child(
                div()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child(preview.tab_label.clone()),
            )
            .into_any_element()
    }

    /// 新建标签按钮 - 放在滚动区域外面，始终可见
    fn render_new_tab_button(theme: UiTheme, on_new_tab: NewTabCallback) -> Stateful<Div> {
        div()
            .id("new-tab-btn")
            .w(px(NEW_TAB_BUTTON_SIZE_PX))
            .h(px(NEW_TAB_BUTTON_SIZE_PX))
            .flex()
            .flex_shrink_0()
            .ml_1()
            .mr_3()
            .items_center()
            .justify_center()
            .rounded(px(999.0))
            .cursor_pointer()
            .hover(move |style| style.bg(theme.tab_hover_background))
            .child(
                svg()
                    .size(px(ICON_SIZE_PX))
                    .text_color(theme.icon_muted)
                    .path(Icon::Plus.path()),
            )
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                on_new_tab(window, cx);
            })
    }
}

impl Render for TabBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let on_panel_drop = self.on_panel_drop.clone();

        let new_tab_button = self
            .on_new_tab
            .clone()
            .map(|on_new_tab| Self::render_new_tab_button(theme, on_new_tab));

        div()
            .id(self.id.clone())
            .h_full()
            .w_full()
            .flex()
            .items_center()
            // 标签栏背景：编辑器悬浮面板内需透明以透出玻璃材质，其余场景保持 app 背景避免脱节
            .bg(if self.transparent_bg {
                gpui::transparent_black()
            } else {
                theme.app_background
            })
            .overflow_hidden()
            .pt(if self.compact {
                px(2.5)
            } else if self.transparent_bg {
                px(4.)
            } else {
                px(5.)
            })
            .pb(if self.compact {
                px(3.5)
            } else if self.transparent_bg {
                px(4.)
            } else {
                px(3.)
            })
            // 阻止 TabBar 区域的鼠标事件冒泡到 TitleBar，防止触发窗口拖拽
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.stop_propagation();
                }),
            )
            // TabBar 整体接受 DraggedPanel drop（落点在标签列表之外时的兜底）
            .on_drop(cx.listener({
                let on_panel_drop = on_panel_drop.clone();
                move |this, dragged: &DraggedPanel, window, cx| {
                    let insert_index = this
                        .panel_drop_preview
                        .as_ref()
                        .map(|preview| preview.insert_index);
                    this.reset_panel_drop_preview(cx);
                    if let Some(ref callback) = on_panel_drop {
                        callback(dragged.clone(), insert_index, window, cx);
                    }
                }
            }))
            .on_action(Self::context_menu_action_listener::<CopyTabAddress>(
                cx,
                self.on_tab_copy_address.clone(),
            ))
            .on_action(Self::context_menu_action_listener::<DuplicateTab>(
                cx,
                self.on_tab_duplicate.clone(),
            ))
            .on_action(Self::context_menu_action_listener::<ReconnectTab>(
                cx,
                self.on_tab_reconnect.clone(),
            ))
            .child(
                // 外层容器：横向布局滚动区 + 新建按钮
                div()
                    .id("tab-bar-outer")
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .w_full()
                    .child(self.render_tab_list(cx))
                    .children(new_tab_button),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_disc_edge_preserves_low_contrast_brands() {
        assert!(agent_disc_needs_edge(0x000000, rgb(0x101010).into()));
        assert!(agent_disc_needs_edge(0xFFFFFF, rgb(0xF8F8F8).into()));
        assert!(!agent_disc_needs_edge(0x000000, rgb(0xFFFFFF).into()));
        assert!(!agent_disc_needs_edge(0xFFFFFF, rgb(0x000000).into()));
        assert!(agent_disc_needs_edge(0x000000, rgb(0x1D1D1D).into()));
        assert!(!agent_disc_needs_edge(0x000000, rgb(0x1E1E1E).into()));
    }

    #[test]
    fn sweep_center_is_strongest_with_symmetric_soft_edges() {
        let base = gpui::Rgba::from(rgb(0x808080));
        let accent = gpui::Rgba::from(rgb(0x2080ff));
        let glyphs: Vec<_> = (0..11).map(|i| (i..i + 1, i as f32)).collect();
        let frame: Vec<_> = shimmer_highlights(&glyphs, 10.0, 0.5, base, accent).collect();
        let color = |index| {
            frame
                .iter()
                .find(|(range, _)| range.start == index)
                .map(|(_, style)| gpui::Rgba::from(style.color.unwrap()))
                .unwrap_or(base)
        };
        // 带外字形必须**视觉上**等于基色：ε 微调远小于 8-bit 最小可辨差（≈4e-3），
        // 但不要求逐位相等（ε 的存在正是为了拆开相邻字形，见 shimmer_highlights）。
        const VISUALLY_SAME: f32 = 5e-4;
        assert!((color(0).r - base.r).abs() < VISUALLY_SAME);
        assert!((color(10).r - base.r).abs() < VISUALLY_SAME);
        assert!((color(5).b - accent.b).abs() < 1e-5);
        assert!(color(5).b > color(4).b && color(4).b > color(3).b && color(3).b > base.b);
        assert!((color(4).b - color(6).b).abs() < 1e-5);
        assert!((color(3).b - color(7).b).abs() < 1e-5);
        // 防抖不变量：每个字形都有高亮，且相邻字形颜色永不相等——
        // 否则 gpui 会把它们合并进同一个 FontRun，段边界随帧移动导致文字微漂。
        assert_eq!(frame.len(), glyphs.len());
        for pair in frame.windows(2) {
            assert_ne!(pair[0].1.color, pair[1].1.color);
        }
        // A sub-character movement must change intensity, not jump a solid band.
        let next: Vec<_> = shimmer_highlights(&glyphs, 10.0, 0.51, base, accent).collect();
        assert_ne!(frame, next);
    }

    /// 扫光带不能「跳过」字形：任何宽度的标签上，每个字都要有机会亮到峰值。
    ///
    /// 相位是余弦往返，带中心的峰值速度 = π·width / period，即每帧位移随宽度
    /// **线性增长**；而亮核宽度就是 radius。一旦帧位移逼近亮核宽度，字形从进入
    /// 带到离开之间没有任何一帧采样到它的峰值——部分字**根本不亮**。
    ///
    /// 这条守的是 radius 的上限回归：旧代码 `.clamp(2.5, 6.0)` 把亮核卡在 6px，
    /// 30fps 下 width=120px 时帧位移 5.24px（比值 0.87），60% 的字达不到峰值。
    /// 按 0.18·width 放开上限后该比值恒为 0.242，任意宽度都无字形被跳过。
    #[test]
    fn every_glyph_reaches_peak_across_label_widths() {
        // base 纯黑、accent 纯白：红通道即强度（仅差 ±1e-4 的防合并微调）。
        let base = rgb(0x000000);
        let accent = rgb(0xFFFFFF);
        let frames =
            (AGENT_WORK_SHIMMER_PERIOD_MS as f32 / 1000.0 * AGENT_WORK_SHIMMER_MAX_FPS) as usize;

        for width in [30.0, 60.0, 120.0, 180.0, 240.0] {
            let count = (width / 8.0) as usize;
            let glyphs: Vec<_> = (0..count)
                .map(|i| (i..i + 1, (i as f32 + 0.5) * width / count as f32))
                .collect();
            let mut peak = vec![0.0f32; count];
            // 按真实帧率逐帧跑满一个周期，记录每个字形见过的最强亮度。
            for frame in 0..frames {
                let phase = shimmer_phase(frame as f32 / frames as f32);
                for (index, (_, style)) in
                    shimmer_highlights(&glyphs, width, phase, base, accent).enumerate()
                {
                    let r = gpui::Rgba::from(style.color.unwrap()).r;
                    peak[index] = peak[index].max(r);
                }
            }
            let dim = peak.iter().filter(|p| **p < 0.9).count();
            assert_eq!(
                dim, 0,
                "width={width}: {dim}/{count} 个字形整周期都没亮到峰值——\
                 扫光带跳过了它们（radius 相对每帧位移太小）"
            );
        }
    }

    /// 相位必须是 0→1→0 的往返，且两端（t=0 / t=1）速度为零。
    #[test]
    fn shimmer_phase_round_trips_with_soft_ends() {
        assert_eq!(shimmer_phase(0.0), 0.0);
        assert_eq!(shimmer_phase(1.0), 0.0);
        assert!((shimmer_phase(0.5) - 1.0).abs() < 1e-6);
        // 对称：t 与 1-t 的相位互补（往返一致）。
        assert!((shimmer_phase(0.25) + shimmer_phase(0.75) - 1.0).abs() < 1e-4);
        // 中点附近相位单调上升，两端平缓（余弦包络）。
        assert!(shimmer_phase(0.5) > shimmer_phase(0.4));
    }

    use gpui::{point, size};

    fn make_tabs(ids: &[&str]) -> Vec<TabItem> {
        ids.iter().map(|id| TabItem::new(*id, *id)).collect()
    }

    fn ids(tabs: &[TabItem]) -> Vec<String> {
        tabs.iter().map(|t| t.id.to_string()).collect()
    }

    fn preview(from_index: usize, insert_slot: usize) -> TabReorderPreview {
        TabReorderPreview {
            dragged_tab: DraggedTab {
                tab_id: format!("tab-{from_index}").into(),
                from_index,
            },
            insert_slot,
        }
    }

    #[test]
    fn final_index_for_slot_uses_before_semantics() {
        // slot == from 或 from+1：插入位仍在自身位置，等价无移动
        assert_eq!(TabBar::final_index_for_slot(0, 0), 0);
        assert_eq!(TabBar::final_index_for_slot(0, 1), 0);
        // slot > from+1：移除自身后目标位左移一位
        assert_eq!(TabBar::final_index_for_slot(0, 2), 1);
        assert_eq!(TabBar::final_index_for_slot(0, 3), 2);
        assert_eq!(TabBar::final_index_for_slot(1, 3), 2);
        // slot < from：插入位即最终索引
        assert_eq!(TabBar::final_index_for_slot(2, 0), 0);
        assert_eq!(TabBar::final_index_for_slot(2, 3), 2);
    }

    #[test]
    fn apply_reorder_moves_tabs_to_insert_slot() {
        // 右移：A 拖到末尾 [A,B,C] -> [B,C,A]
        let mut tabs = make_tabs(&["a", "b", "c"]);
        assert_eq!(apply_reorder(&mut tabs, "a", 3), Some(0));
        assert_eq!(ids(&tabs), ["b", "c", "a"]);

        // 左移：C 拖到开头 [A,B,C] -> [C,A,B]
        let mut tabs = make_tabs(&["a", "b", "c"]);
        assert_eq!(apply_reorder(&mut tabs, "c", 0), Some(2));
        assert_eq!(ids(&tabs), ["c", "a", "b"]);

        // 跨一位右移：B 拖到 C 之后 [A,B,C] -> [A,C,B]
        let mut tabs = make_tabs(&["a", "b", "c"]);
        assert_eq!(apply_reorder(&mut tabs, "b", 3), Some(1));
        assert_eq!(ids(&tabs), ["a", "c", "b"]);
    }

    #[test]
    fn apply_reorder_no_op_cases() {
        let mut tabs = make_tabs(&["a", "b", "c"]);

        // slot == from：无移动
        assert_eq!(apply_reorder(&mut tabs, "b", 1), None);
        // slot == from + 1：插入位在自身之后，等价无移动
        assert_eq!(apply_reorder(&mut tabs, "b", 2), None);
        assert_eq!(apply_reorder(&mut tabs, "a", 1), None);
        assert_eq!(ids(&tabs), ["a", "b", "c"]);

        // 标签不存在
        assert_eq!(apply_reorder(&mut tabs, "zzz", 0), None);
        // 空列表
        let mut empty: Vec<TabItem> = Vec::new();
        assert_eq!(apply_reorder(&mut empty, "a", 0), None);
    }

    #[test]
    fn reorder_preview_has_effect_matches_final_position_change() {
        assert!(!TabBar::reorder_preview_has_effect(&preview(1, 1), 3));
        assert!(!TabBar::reorder_preview_has_effect(&preview(1, 2), 3));
        assert!(TabBar::reorder_preview_has_effect(&preview(1, 3), 3));
        assert!(TabBar::reorder_preview_has_effect(&preview(1, 0), 3));
        // from 越界
        assert!(!TabBar::reorder_preview_has_effect(&preview(3, 0), 3));
        // 最终索引越界
        assert!(!TabBar::reorder_preview_has_effect(&preview(0, 5), 3));
    }

    #[test]
    fn insert_slot_uses_tab_midpoints() {
        let bounds: Vec<Bounds<Pixels>> = (0..3)
            .map(|i| Bounds {
                origin: point(px(i as f32 * 100.0), px(0.0)),
                size: size(px(100.0), px(30.0)),
            })
            .collect();

        assert_eq!(insert_slot_for_content_x(px(-10.0), &bounds), 0);
        assert_eq!(insert_slot_for_content_x(px(49.0), &bounds), 0);
        // 恰在中点上：视为越过中点，插入到下一个位置
        assert_eq!(insert_slot_for_content_x(px(50.0), &bounds), 1);
        assert_eq!(insert_slot_for_content_x(px(149.0), &bounds), 1);
        assert_eq!(insert_slot_for_content_x(px(150.0), &bounds), 2);
        assert_eq!(insert_slot_for_content_x(px(249.0), &bounds), 2);
        // 最后一个中点之后：追加到末尾
        assert_eq!(insert_slot_for_content_x(px(250.0), &bounds), 3);
        assert_eq!(insert_slot_for_content_x(px(1000.0), &bounds), 3);
        // 无标签：只能插入到 0
        assert_eq!(insert_slot_for_content_x(px(10.0), &[]), 0);
    }

    #[test]
    fn reorder_offset_shifts_only_moved_interval() {
        // 右移 A(0)->2：区间 (0, 2] 左移让位，拖拽标签自身不动
        assert_eq!(reorder_offset(0, 2, 0, 10.0), 0.0);
        assert_eq!(reorder_offset(0, 2, 1, 10.0), -10.0);
        assert_eq!(reorder_offset(0, 2, 2, 10.0), -10.0);
        assert_eq!(reorder_offset(0, 2, 5, 10.0), 0.0);

        // 左移 C(2)->0：区间 [0, 2) 右移让位
        assert_eq!(reorder_offset(2, 0, 0, 10.0), 10.0);
        assert_eq!(reorder_offset(2, 0, 1, 10.0), 10.0);
        assert_eq!(reorder_offset(2, 0, 2, 10.0), 0.0);

        // 无移动：全部为 0
        assert_eq!(reorder_offset(1, 1, 0, 10.0), 0.0);
        assert_eq!(reorder_offset(1, 1, 1, 10.0), 0.0);
    }

    /// 右键 tab → 构建出右键菜单；菜单项 Action 派发必须到达 TabBar 的
    /// on_action 处理器（终端标签：拷贝地址/复制标签页/重新连接）。
    ///
    /// 回归测试：右键链路涉及三段接力（手动 Right 记录 tab id → ContextMenu
    /// 浮层开菜单 → Action 派发到 TabBar 根），任一段断裂菜单即“不起作用”。
    struct MenuProbeHost {
        bar: Entity<TabBar>,
    }

    impl Render for MenuProbeHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            // 固定 400x30 顶左布局：首个 tab 落在 (0..~150, 0..30)。
            div().w(px(400.)).h(px(30.)).child(self.bar.clone())
        }
    }

    #[gpui::test]
    fn working_sweep_keeps_tab_background_stable(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let mut tab = TabItem::new("sweep-probe", "opencode: ~/Documents/工作区/deepterm");
        tab.agent_status = Some(AgentStatus::Working);
        let bar = cx.update(|cx| TabBar::new_with_tabs("sweep-bar", cx, vec![tab]));
        let window = cx.open_window(size(px(600.), px(40.)), {
            let bar = bar.clone();
            move |_, _| MenuProbeHost { bar: bar.clone() }
        });
        cx.run_until_parked();
        let capture = |cx: &mut gpui::TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window
                    .painted_quads()
                    .into_iter()
                    .map(|quad| {
                        // Ignore submission order; compare the actual visible rectangle,
                        // clipping, background and border submitted to the renderer.
                        format!(
                            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                            quad.bounds,
                            quad.content_mask,
                            quad.background,
                            quad.border_color,
                            quad.corner_radii,
                            quad.border_widths
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap()
        };
        let baseline = capture(cx);
        assert!(
            !baseline.is_empty(),
            "probe must paint a real tab background"
        );
        for frame in 1..=16 {
            cx.executor().advance_clock(Duration::from_millis(150));
            cx.run_until_parked();
            assert_eq!(
                capture(cx),
                baseline,
                "background changed on sweep frame {frame}"
            );
        }
        bar.update(cx, |bar, cx| {
            bar.tabs[0].agent_status = Some(AgentStatus::Done);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            capture(cx),
            baseline,
            "leaving Working changed tab geometry or background"
        );
    }

    #[gpui::test]
    fn tab_right_click_opens_menu_and_action_reaches_handler(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::theme::init(cx));

        let menu_built = std::sync::Arc::new(std::sync::Mutex::new(false));
        let action_tab = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));

        let bar = cx.update(|cx| {
            TabBar::new_with_tabs(
                "test-tab-bar",
                cx,
                vec![
                    TabItem::new("t1", "Terminal 1"),
                    TabItem::new("t2", "Terminal 2"),
                ],
            )
        });
        bar.update(cx, |bar, _| {
            let built = menu_built.clone();
            bar.on_context_menu(move |tab_id, menu, _window, _cx| {
                *built.lock().unwrap() = true;
                menu.menu(format!("probe-{tab_id}"), Box::new(CopyTabAddress))
            });
            let fired = action_tab.clone();
            bar.on_tab_copy_address(move |tab_id, _window, _cx| {
                *fired.lock().unwrap() = Some(tab_id.to_string());
            });
        });

        let window = cx.open_window(gpui::size(px(400.), px(30.)), {
            let bar = bar.clone();
            move |_, _| MenuProbeHost { bar: bar.clone() }
        });
        let handle: gpui::AnyWindowHandle = window.into();
        let cx = &mut gpui::VisualTestContext::from_window(handle, cx);
        cx.run_until_parked();

        // 首个 tab 内右键单击。
        let pos = point(px(30.), px(15.));
        cx.simulate_mouse_down(pos, MouseButton::Right, gpui::Modifiers::none());
        cx.simulate_mouse_up(pos, MouseButton::Right, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            *menu_built.lock().unwrap(),
            "tab 右键必须构建出右键菜单（开菜单链路断裂）"
        );
        // 右键必须同时记下 tab id，供菜单动作回调使用。
        let recorded = bar.update(cx, |bar, _| bar.context_menu_tab_id());
        assert_eq!(recorded.as_deref(), Some("t1"), "右键必须记下被点的 tab id");

        // 菜单项派发 Action：必须到达 TabBar 根的 on_action 处理器。
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(CopyTabAddress), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            action_tab.lock().unwrap().as_deref(),
            Some("t1"),
            "菜单 Action 派发必须到达 TabBar 处理器"
        );

        // 收尾：点空白处关闭菜单，否则 PopupMenu 实体泄漏（测试会报 leaked handles）。
        let outside = point(px(350.), px(15.));
        cx.simulate_mouse_down(outside, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_up(outside, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// 回归：1x 外接屏上 GPUI 的 2px 拖拽阈值等于 2 个物理像素，物理鼠标按压
    /// 微抖就会把一次点击误启动成拖拽——`on_click` 被取消，而落点仍在原位、
    /// 重排又是 no-op，看起来就是“点了没反应”（还会短暂闪出跟随鼠标的拖拽
    /// 预览）。此时原位 drop 必须把这次落点补发为一次点击。
    #[gpui::test]
    fn jittered_press_still_fires_tab_click(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::theme::init(cx));

        let clicked = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let reordered = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let bar = cx.update(|cx| {
            TabBar::new_with_tabs(
                "test-tab-bar-jitter",
                cx,
                vec![
                    TabItem::new("t1", "Terminal 1"),
                    TabItem::new("t2", "Terminal 2"),
                ],
            )
        });
        bar.update(cx, |bar, _| {
            let fired = clicked.clone();
            bar.on_tab_click(move |tab_id, _window, _cx| {
                *fired.lock().unwrap() = Some(tab_id.to_string());
            });
            let moved = reordered.clone();
            bar.on_tab_reorder(move |dragged, _slot, _cx| {
                moved.lock().unwrap().push(dragged.tab_id.to_string());
            });
        });

        let window = cx.open_window(gpui::size(px(400.), px(30.)), {
            let bar = bar.clone();
            move |_, _| MenuProbeHost { bar: bar.clone() }
        });
        let handle: gpui::AnyWindowHandle = window.into();
        let cx = &mut gpui::VisualTestContext::from_window(handle, cx);
        cx.run_until_parked();

        // 对照：干净点击必须触发切换（既有行为不断裂）。
        let pos = point(px(30.), px(15.));
        cx.simulate_mouse_move(pos, None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_click(pos, gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(clicked.lock().unwrap().as_deref(), Some("t1"));
        *clicked.lock().unwrap() = None;

        // 抖动点击：按下后移动 5px（超过 GPUI 2px 拖拽阈值）再释放。
        // 修复前：误启动拖拽 → click 被取消 → drop 又是原位 no-op → 无任何回调。
        let down = point(px(30.), px(15.));
        let up = point(px(34.), px(18.));
        cx.simulate_mouse_move(down, None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_down(down, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(up, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(up, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            clicked.lock().unwrap().as_deref(),
            Some("t1"),
            "抖动导致的原位 drop 必须补发 tab 点击"
        );
        assert!(reordered.lock().unwrap().is_empty(), "原位落点不得触发重排");
        let order: Vec<String> = bar.update(cx, |bar, _| {
            bar.tabs.iter().map(|t| t.id.to_string()).collect()
        });
        assert_eq!(order, ["t1", "t2"]);
    }

    // ===== agent 状态点（阶段 4）=====

    /// `Waiting` 必须与 `Done` 有形状差异，而不只是颜色差异。
    ///
    /// 这是一个**无障碍回归护栏**：两者的颜色是琥珀与绿，正是红绿色觉最难
    /// 分辨的一对，而这一对恰好决定「要不要过去看一眼」。如果哪天有人为了
    /// 「看起来更统一」把 Waiting 也改成实心，这个测试必须挡住。
    #[test]
    fn agent_dot_shapes_distinguish_waiting_from_done() {
        use AgentStatus::*;

        let (waiting_rgb, waiting_shape) = agent_dot(Waiting).expect("Waiting 必须有状态点");
        let (done_rgb, done_shape) = agent_dot(Done).expect("Done 必须有状态点");
        let (_, working_shape) = agent_dot(Working).expect("Working 必须有状态点");

        assert_eq!(waiting_shape, AgentDot::Hollow, "Waiting 必须是空心圆环");
        assert_eq!(done_shape, AgentDot::Filled);
        assert_eq!(working_shape, AgentDot::Filled);

        // 形状与颜色两个维度都不能重合，否则红绿色觉用户只剩颜色可依赖。
        assert_ne!(waiting_shape, done_shape);
        assert_ne!(waiting_rgb, done_rgb);

        // Idle 不画点：agent 开着但没干活，图标本身已经说明它的存在。
        assert_eq!(agent_dot(Idle), None);
    }

    /// 状态点颜色必须来自 `AgentStatus::dot_rgb()`，不能在本模块另起一套。
    #[test]
    fn agent_dot_color_comes_from_status() {
        use AgentStatus::*;

        for status in [Working, Waiting, Done] {
            let expected = status.dot_rgb().expect("这三个状态都该有颜色");
            let (rgb_value, _) = agent_dot(status).expect("都该有状态点");
            assert_eq!(rgb_value, expected, "{status:?} 的颜色与 dot_rgb 不一致");
        }
    }

    /// 扫光高亮色优先用 agent 品牌色；与正文或标签底色撞色时才回落。
    ///
    /// 守两类失败：
    /// 1. 高亮色退化成主题 accent（改动前的行为）——文字在动，但看不出是哪个 agent；
    /// 2. 品牌色撞色时直接用它——扫光扫过去**什么都看不见**。
    ///
    /// 撞色有两种，必须都判：
    /// - 与**正文基色**重合（品牌色与正文同色时扫光不可见）；
    /// - 在**标签底色**上不可见（浅色主题的标签底色接近纯白，黑色品牌色
    ///   扫过去直接压进底色里）。只看正文会放过后者。
    ///
    /// 用例取自 `assets/themes/builtin.json` 的真实值，不手编：手编极易把
    /// 「看起来该重合」当成重合（纯黑配 #1A1A1A 实际感知距离 0.218，完全可辨）。
    #[test]
    fn agent_sweep_color_uses_brand_unless_indistinguishable() {
        let accent: gpui::Hsla = rgb(0x3B82F6).into();
        // `Hsla`/`Rgba` 没有精确 `PartialEq`（浮点分量），逐通道比较。
        let same = |a: gpui::Hsla, b: gpui::Hsla| {
            let (x, y) = (gpui::Rgba::from(a), gpui::Rgba::from(b));
            (x.r - y.r).abs() < 1e-4 && (x.g - y.g).abs() < 1e-4 && (x.b - y.b).abs() < 1e-4
        };
        // 中性的安全底色（对纯黑/纯白都有足够对比度），用于隔离「与正文撞色」。
        let neutral: gpui::Hsla = rgb(0x808080).into();

        // solarized-dark 正文（#839496）与 Claude 橙：亮度比 1.01 但色相迥异，
        // 感知距离 0.151 → 必须保留品牌色。这是亮度判据会误杀的典型形态。
        let solarized_dark_text: gpui::Hsla = rgb(0x839496).into();
        assert!(
            same(
                TabBar::agent_sweep_color(
                    0xD97757,
                    solarized_dark_text,
                    rgb(0x002b36).into(),
                    accent
                ),
                rgb(0xD97757).into()
            ),
            "色相迥异时必须保留品牌色，不能被亮度判据误杀"
        );

        // 与正文重合：macos26-dark 的正文就是 #FFFFFF，纯白品牌与它距离 0
        // → 回落（底色中性，排除背景因素）。
        let white_text: gpui::Hsla = rgb(0xFFFFFF).into();
        assert!(
            same(
                TabBar::agent_sweep_color(0xFFFFFF, white_text, neutral, accent),
                accent
            ),
            "品牌色与正文完全重合时必须回落，否则扫光不可见"
        );

        // 在底色上不可见：github-light 的标签底色是 #FFFFFF，纯白品牌
        // 与正文 #24292F 分得很开（dE 0.722），但白字压白底对比度 1.00
        // → 必须回落。只看正文的判据会在这里放行。
        let github_light_text: gpui::Hsla = rgb(0x24292F).into();
        let github_light_bg: gpui::Hsla = rgb(0xFFFFFF).into();
        assert!(
            same(
                TabBar::agent_sweep_color(0xFFFFFF, github_light_text, github_light_bg, accent),
                accent
            ),
            "品牌色在标签底色上不可见时必须回落（白字压白底）"
        );

        // 同一个纯白品牌配 macos26-light 的底色（#DEDEE4，对比度 1.29）→ 保留。
        assert!(
            same(
                TabBar::agent_sweep_color(
                    0xFFFFFF,
                    rgb(0x1D1D1F).into(),
                    rgb(0xDEDEE4).into(),
                    accent
                ),
                rgb(0xFFFFFF).into()
            ),
            "品牌色在底色上可见、与正文也分得开时必须保留"
        );

        // 纯黑品牌（Codex / Grok / Kimi / OpenCode / Auggie）配 ayu-dark 的
        // 深色底色（#0a0e14，对比度 1.09）→ 回落；同一个黑色配浅色正文和浅底
        // → 保留。
        assert!(
            same(
                TabBar::agent_sweep_color(
                    0x000000,
                    rgb(0xb3b1ad).into(),
                    rgb(0x0a0e14).into(),
                    accent
                ),
                accent
            ),
            "纯黑品牌在深色底色上不可见时必须回落"
        );
        assert!(
            same(
                TabBar::agent_sweep_color(
                    0x000000,
                    rgb(0x1D1D1F).into(),
                    rgb(0xF5F5F7).into(),
                    accent
                ),
                rgb(0x000000).into()
            ),
            "纯黑品牌配浅色正文和浅底时清晰可见"
        );
    }

    /// 编辑器标签栏必须拒绝 agent 状态——agent 只属于跑着它的终端标签。
    ///
    /// 同一窗口里有**两个** `TabBar`：终端栏（`tab-bar`）与编辑器栏
    /// （`editor-tab-bar`）。两者共用同一套渲染代码，所以「agent 不波及编辑器」
    /// 不能靠「编辑器不调用 `set_tab_agent`」这个约定——那只在没人改动时才成立。
    /// 这条钉住真正的边界：编辑器栏显式关掉接收后，即使把 agent 状态喂给它，
    /// 也必须原样拒绝（不改图标、不设状态点）。
    #[gpui::test]
    fn editor_tab_bar_rejects_agent_state(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let bar = cx.update(|cx| {
            TabBar::new_with_tabs(
                "editor-tab-bar",
                cx,
                vec![TabItem::new("file-1", "main.rs")],
            )
        });
        bar.update(cx, |bar, cx| bar.set_accepts_agent_state(false, cx));

        let outcome = bar.update(cx, |bar, cx| {
            bar.set_tab_agent(
                "file-1",
                Some(CLIAgent::Claude),
                AgentStatus::Working,
                None,
                cx,
            )
        });

        assert_eq!(outcome, None, "编辑器栏必须拒绝 agent 状态");
        bar.read_with(cx, |bar, _| {
            let tab = &bar.tabs[0];
            assert_eq!(tab.agent_status, None, "编辑器标签不该有 agent 状态点");
            assert!(
                !matches!(tab.icon, Some(Icon::AgentIcon { .. })),
                "编辑器标签不该被换成 agent 图标，实际是 {:?}",
                tab.icon
            );
        });
    }

    /// 终端标签栏必须接受 agent 状态——上一条的对照，防止守卫写成「一律拒绝」。
    #[gpui::test]
    fn terminal_tab_bar_accepts_agent_state(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let bar = cx.update(|cx| {
            TabBar::new_with_tabs("tab-bar", cx, vec![TabItem::new("tab-1", "claude")])
        });
        assert!(
            bar.read_with(cx, |bar, _| bar.accepts_agent_state),
            "终端栏默认必须接受 agent 状态"
        );

        bar.update(cx, |bar, cx| {
            bar.set_tab_agent(
                "tab-1",
                Some(CLIAgent::Claude),
                AgentStatus::Working,
                None,
                cx,
            );
        });

        bar.read_with(cx, |bar, _| {
            let tab = &bar.tabs[0];
            assert_eq!(tab.agent_status, Some(AgentStatus::Working));
            assert!(matches!(tab.icon, Some(Icon::AgentIcon { .. })));
        });
    }

    /// `TabItem` 的品牌色必须来自图标本身携带的 `accent_rgb`。
    ///
    /// 渲染层若自己再查一次 `CLIAgent`，就多了一个「图标画的是 A、扫光按 B 取色」
    /// 的错配来源。这里锁住两者同源。
    #[test]
    fn tab_item_brand_color_comes_from_its_icon() {
        use crate::agent::CLIAgent;

        let tab = TabItem {
            icon: Some(Icon::AgentIcon {
                path: CLIAgent::Claude.icon_path(),
                rgb: CLIAgent::Claude.icon_rgb(),
                accent_rgb: CLIAgent::Claude.accent_rgb(),
            }),
            ..TabItem::new("t", "terminal")
        };
        assert_eq!(
            tab.agent_accent_rgb(),
            Some(CLIAgent::Claude.accent_rgb()),
            "品牌色必须与图标携带的 accent_rgb 一致"
        );

        // 非 agent 图标没有品牌色（普通终端 / 编辑器 tab）。
        let plain = TabItem::new("t2", "terminal");
        assert_eq!(plain.agent_accent_rgb(), None);
    }

    /// 「需关注」红点的让位规则。
    #[test]
    fn attention_dot_yields_to_modified_only() {
        let base = |needs_attention, is_modified| TabItem {
            needs_attention,
            is_modified,
            ..TabItem::new("t", "terminal")
        };

        // 没在等你看 → 不画。
        assert!(!should_show_attention_dot(&base(false, false)));

        // 普通的「后台响了铃」→ 画。
        assert!(should_show_attention_dot(&base(true, false)));

        // 有未保存改动 → 让位给灰点。
        assert!(!should_show_attention_dot(&base(true, true)));
    }

    /// **组合断言**：agent 跑完后，红点标志必须真的能画出来。
    ///
    /// 这条是本模块最容易「两半都对、合起来错」的地方：`workspace::should_mark_unread`
    /// 只认 `Working → Done` 边沿，而那一刻 `agent_status` 必然是 `Done`。
    /// 若 `should_show_attention_dot` 在"有 agent 状态点"时让位（曾经如此），
    /// 标志被点亮后**永远画不出来**——两个纯函数各自都有测试，却没有任何一条
    /// 覆盖它们的组合。这里显式把两边接起来断言。
    #[test]
    fn done_edge_flag_actually_renders_a_dot() {
        use crate::workspace::workspace::should_mark_unread;
        use AgentStatus::*;

        // 后台 tab：agent 从 Working 走到 Done。
        let previous = Some(Working);
        let next = Done;
        let is_active = false;
        assert!(
            should_mark_unread(previous, next, is_active),
            "前提：后台 tab 的 Working → Done 边沿应点亮标志"
        );

        // 标志点亮后的 tab 状态：Done 让 `agent_dot` 返回 `Some`（绿实心点可见）。
        let tab = TabItem {
            needs_attention: true,
            is_modified: false,
            agent_status: Some(next),
            ..TabItem::new("t", "terminal")
        };
        assert!(
            agent_dot(next).is_some(),
            "前提：Done 确实有图标状态点，所以旧的让位规则会吞掉红点"
        );
        assert!(
            should_show_attention_dot(&tab),
            "agent 跑完点亮的红点必须真的画出来，不能被图标状态点吞掉"
        );

        // 用户切过去看过 → 标志被清（`set_active_tab` 的语义），红点随之消失。
        let viewed = TabItem {
            needs_attention: false,
            ..tab
        };
        assert!(
            !should_show_attention_dot(&viewed),
            "看过之后红点必须消失，否则「未读」语义就没了"
        );
    }

    /// 每个 `CLIAgent` 的图标都必须在嵌入资源里真实存在。
    ///
    /// 这是最容易悄无声息失效的一环：`icon_path()` 返回一个不存在的路径时
    /// gpui 只是画不出东西，不报错、不 panic，tab 上就是空一块。所以直接拿
    /// `Assets::get` 查一遍，而不是只断言路径字符串长得对。
    #[test]
    fn every_agent_icon_resolves_to_an_embedded_asset() {
        for agent in CLIAgent::ALL {
            let icon = Icon::AgentIcon {
                path: agent.icon_path(),
                rgb: agent.icon_rgb(),
                accent_rgb: agent.accent_rgb(),
            };
            let path = icon.path();
            assert!(
                crate::assets::Assets::get(path.as_ref()).is_some(),
                "{agent:?} 的图标 {path} 不在嵌入资源里"
            );
        }
    }
}
