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
pub type NewTabCallback = Rc<dyn Fn(&mut Window, &mut App) + Send + Sync>;
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

#[derive(Clone, Debug, PartialEq)]
struct PanelDropPreview {
    insert_index: usize,
    tab_label: SharedString,
    /// 被拖入的面板在标签里会显示成什么程序方块。
    ///
    /// 插入预览必须和落成后的那个标签长得一样，否则拖到一半看到的宽度、
    /// 图标都不是最终结果。
    program: TileProgram,
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

// ── herdr Terminals LAYOUT 的托盘尺寸 ──────────────────────────────────────
/// 托盘高度（标签自身高度 = 托盘高度 - 上下各 3px 内边距 = 30）。
const TRAY_HEIGHT_PX: f32 = 36.0;
/// 标签在托盘里的实际高度 = `TRAY_HEIGHT_PX - TRAY_PADDING_PX * 2`。
///
/// 单独列出来是因为拖拽预览要按同一个高度画：预览没有托盘可依靠，
/// 必须自己把这个高度写死，否则两处会各算各的。
const TRAY_TAB_HEIGHT_PX: f32 = TRAY_HEIGHT_PX - TRAY_PADDING_PX * 2.0;
/// 托盘内边距。
const TRAY_PADDING_PX: f32 = 3.0;
/// 托盘圆角。
const TRAY_RADIUS_PX: f32 = 16.0;
/// 标签间距。
const TRAY_GAP_PX: f32 = 3.0;
/// 标签宽度：等宽 + `flex_shrink(1)`，标签条不会随内容抖动。
pub const TAB_WIDTH_PX: f32 = 260.0;
/// 标签最小宽度：可被压缩，但不能压到看不见（再多标签也保证图标 + 名字）。
const TAB_MIN_WIDTH_PX: f32 = 96.0;
/// 标签圆角（比托盘小一档，形成「槽里嵌胶囊」的层次）。
const TAB_RADIUS_TRAY_PX: f32 = 13.0;
/// 标签文字大小。
const TAB_TEXT_PX: f32 = 13.0;
/// 标签名字段的最大宽度：超过就省略，保证目录段总有位置。
const TAB_TITLE_MAX_PX: f32 = 150.0;
/// 程序方块边长。
const TILE_SIZE_PX: f32 = 26.0;
/// 叠放最下层相对单方块的比例。
///
/// 层次靠**逐层递进的缩放**表达（见 [`TILE_STACK_SCALE_STEP`]），不靠描边：
/// 给每层描边会在深色主题上叠出一圈黑边，看着像贴纸，也不扁平。
const TILE_STACK_SCALE: f32 = 0.84;
/// 每往上一层放大的比例。最上层接近满尺寸，读作「被拿到最前面」。
const TILE_STACK_SCALE_STEP: f32 = 0.07;
/// 叠放错位：每多一层向右偏这么多。约等于方块的四分之一，露出的是**图形
/// 边角**而不是半个方块——再多就认不出底下那个程序了。
const TILE_STACK_STEP_PX: f32 = 5.0;
/// 叠放最多显示几个。再多只留最上（选中）的那几层：一个标签里有 8 个分屏
/// 时画 8 个既看不清又挤掉标题。
const TILE_STACK_MAX: usize = 3;
/// 叠放层在 panel 切换时的过渡时长。短于标签重排（155ms）——
/// 切 panel 是"点一下"的动作，动画只用来交代"哪一层上来了"，
/// 拖久了会觉得卡。
const TILE_STACK_ANIM_MS: u64 = 130;
/// 方块内图形的边长。
const TILE_GLYPH_PX: f32 = 15.0;

/// 托盘标签的**视觉快照**：把「一个标签长什么样」从交互里剥出来。
///
/// 存在的理由：拖拽时跟随光标的预览必须和原标签**逐项一致**（宽度、高度、
/// 圆角、程序方块、两段文字、状态点）。两处各写一遍必然漂移——改了托盘样式
/// 忘了改预览，拖动中就露出一个旧样式的幽灵标签。所以两边都从这份快照出发，
/// 走同一个 [`render_tray_tab_body`]。
#[derive(Clone, Debug, PartialEq)]
pub struct TrayTabVisual {
    /// 左侧程序方块的内容（多个 panel 时这是**最上层**那个，也是状态点
    /// 与扫光所代表的那个）。
    pub program: TileProgram,
    /// 横向叠放的全部程序方块（从下到上）。只有一个时与 `program` 相同。
    ///
    /// 叠放是 herdr 没有的：它的标签对应单个终端；deepterm 的标签可以是
    /// 多个分屏 panel，把各自的程序方块错开叠起来，一眼能看出「这个标签
    /// 里有几个东西在跑」，选中的那个在最上层。
    pub tiles: Vec<TileProgram>,
    /// 名字段。
    pub title: SharedString,
    /// 目录段（空串表示不画）。
    pub cwd: SharedString,
    /// 有未保存改动（灰点）。
    pub is_modified: bool,
    /// 需要看一眼（红点）。
    pub needs_attention: bool,
    /// 方块右上角的 agent 回合状态点；`None` 不画。
    pub dot: Option<AgentStatus>,
    /// 是否正在输出（文字扫光 + 转圈）。
    pub streaming: bool,
    /// 扫光用的品牌色（`Icon::AgentIcon::accent_rgb`）。
    pub agent_accent_rgb: Option<u32>,
    /// 选中态：选中用「抬起底色 + 描边」，未选中用托盘底。
    pub is_active: bool,
    /// 标签宽度（拖拽预览按同一个值画）。
    pub width: Pixels,
    /// 动画元素 id 前缀：同一个标签的扫光 / 转圈在托盘与预览里不能共用 id。
    pub animation_key: SharedString,
}

impl TrayTabVisual {
    /// 从终端视图构造：面板标题栏拖拽时用，与顶部标签栏同一份视觉来源。
    ///
    /// 顶部标签栏的 `TabBar::tray_tab_visual` 是从 `TabItem` 取的，而面板没有
    /// `TabItem`——它手里只有 `TerminalView`。两者都从 `terminal_view` 的同一组
    /// 方法（`tab_title_parts` / `foreground_program` / `agent_tab_state`）取值，
    /// 所以同一条终端无论从顶部还是从面板标题栏拖起来，画面都一致。
    pub fn from_terminal_view(
        view: &crate::terminal_view::terminal_view::TerminalView,
        is_active: bool,
        width: Pixels,
        animation_key: impl Into<SharedString>,
        cx: &App,
    ) -> Self {
        let (title, cwd) = view.tab_title_parts(cx);
        let program = view.foreground_program(cx);
        let (agent, status) = view.agent_tab_state(cx);
        // 与 `TabItem::tile_program` 同构：程序表给什么就用什么，没有就退回
        // agent 自己的图标（面板在这里没有 OS 图标可用）。
        let tile = TileProgram {
            icon: program
                .icon
                .clone()
                .or_else(|| agent.map(|agent| agent.icon_path().to_string())),
            name: program.name.clone(),
            tile: program
                .tile
                .or_else(|| agent.map(|agent| agent.accent_rgb())),
            color: program
                .color
                .or_else(|| agent.map(|agent| agent.icon_rgb())),
            tint: program.is_tinted() || agent.is_some(),
        };
        Self {
            program: tile.clone(),
            tiles: vec![tile],
            title: title.into(),
            cwd: cwd.into(),
            is_modified: false,
            needs_attention: false,
            dot: agent.map(|_| status),
            streaming: view.agent_output_streaming(cx),
            agent_accent_rgb: agent.map(|agent| agent.accent_rgb()),
            is_active,
            width,
            animation_key: animation_key.into(),
        }
    }
}

/// 拖拽预览视图。
pub struct DragPreview {
    pub tab_label: SharedString,
    pub drag_offset: Point<Pixels>,
    pub preview_width: Pixels,
    /// 终端标签的拖拽预览：带完整视觉，画出来与原标签一致。
    ///
    /// `None` 是 panel 标题栏的拖拽（分屏面板），那里没有程序方块，
    /// 沿用旧的一行文字胶囊。
    tray: Option<TrayTabVisual>,
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
            tray: None,
        }
    }

    /// 终端标签的拖拽预览：视觉与托盘里那一枚完全一致。
    ///
    /// 只多两样东西——略微的透明度与一层阴影，用来区分「被拖着的那个」
    /// 和「还留在托盘里的那个」。
    ///
    /// 宽度取标签在托盘里**实际**占的宽度（可能已被压缩），不是名义上的
    /// 260：否则窄窗口里拖动，落点判定按 260 算、预览按 260 画，而原标签
    /// 只有 120，视觉上对不上。
    pub fn tab(visual: TrayTabVisual, width: Pixels, drag_offset: Point<Pixels>) -> Self {
        Self {
            preview_width: width,
            tab_label: visual.title.clone(),
            drag_offset,
            tray: Some(visual),
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
        let theme = cx.theme().ui;

        // 终端标签：与托盘里那一枚走同一个 body 渲染器，只有外层定位不同。
        if let Some(mut visual) = self.tray.clone() {
            // body 自己按 `visual.width` 定宽；外层再兜一层同样的宽度，
            // 保证「预览宽度 = 标签宽度」这件事只有一处真值。
            visual.width = self.preview_width;
            let tray = TabBar::tray_colors(theme);
            return div()
                .pl(self.drag_offset.x - self.preview_width / 2.0)
                .pt(self.drag_offset.y - px(TRAY_TAB_HEIGHT_PX) / 2.0)
                .child(
                    div()
                        .w(self.preview_width)
                        .h(px(TRAY_TAB_HEIGHT_PX))
                        // **必须补上托盘底色**：未选中的标签自身是透明底，
                        // 在托盘里靠托盘的颜色托着；脱离托盘以后不加这层，
                        // 就只剩图标和文字浮在桌面上，正是"文字在飘"的来源。
                        // 选中标签会再叠自己的抬起底色，所以两种状态都与原样一致。
                        .rounded(px(TAB_RADIUS_TRAY_PX))
                        .bg(tray.tray)
                        // 阴影给"被拿起来"的那一层浮起感；透明度只轻微下调，
                        // 过低的 opacity 会把同色系的方块和文字一起洗淡。
                        .opacity(0.98)
                        .shadow_md()
                        .child(render_tray_tab_body(&visual, tray, theme, None, false)),
                );
        }

        let preview_width = self.preview_width;
        let preview_height = px(30.0);

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
/// 画一个托盘标签的**全部视觉**：底色 / 描边 / 圆角 / 程序方块 / 两段文字 /
/// 状态点 / 转圈 / 关闭槽占位。
///
/// 托盘里的真标签与拖拽时跟随光标的预览都调用它——这是两者视觉一致的唯一
/// 保证。`interactive` 只影响 hover 高亮（预览不需要，它跟着光标走）。
///
/// 返回 `Div`（不是 `AnyElement`）：托盘那边还要往上挂 id / 交互回调，
/// 包成 `AnyElement` 就接不回去了。
fn render_tray_tab_body(
    visual: &TrayTabVisual,
    tray: TrayColors,
    theme: UiTheme,
    close: Option<AnyElement>,
    interactive: bool,
) -> Div {
    let is_active = visual.is_active;
    let title_color = if is_active { tray.text } else { tray.muted };
    let cwd_color = tray.dim;
    // 扫光文字要在它上面可见：取标签自己的底色（选中 = 抬起的那块，
    // 未选中 = 托盘底）。
    let surface = if is_active { tray.selected } else { tray.tray };

    let label = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_w_0()
        .gap(px(6.0))
        .items_center()
        // 两个语义圆点：灰点 = 未保存改动，红点 = 「需要你看一眼」
        // （后台铃 / agent 跑完一回合）。
        .when(visual.is_modified, |this| {
            this.child(
                div()
                    .size(px(6.0))
                    .flex_none()
                    .rounded_full()
                    .bg(if is_active {
                        tray.muted
                    } else {
                        tray.muted.opacity(0.7)
                    }),
            )
        })
        .when(visual.needs_attention, |this| {
            this.child(
                div()
                    .size(px(7.0))
                    .flex_none()
                    .rounded_full()
                    .bg(theme.error),
            )
        })
        // herdr 把名字设为 `flex_none`、目录设 `truncate`：先保证「这是什么」
        // 完整，再让「在哪儿」在剩余宽度里省略。名字本身也留一个最大宽度，
        // 否则一个超长进程名会把目录段整段挤没。
        //
        // 文字宽度用「幽灵 sizer」撑住（见 `render_tab_label` 的文档）：
        // 扫光每帧改变高亮分段，gpui 会按分段重新 shaping，行宽在整像素间
        // 振荡；让普通文本（宽度恒定）占位、扫光以覆盖层叠上去，标签
        // 才不会逐帧微抖。
        .child(
            div()
                .relative()
                .flex_none()
                .max_w(px(TAB_TITLE_MAX_PX))
                .min_w_0()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(TAB_TEXT_PX))
                        .text_color(title_color)
                        .when(visual.streaming, |this| this.opacity(0.0))
                        .child(visual.title.clone()),
                )
                .when(visual.streaming, |this| {
                    this.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .child(render_working_label(
                                &visual.animation_key,
                                &visual.title,
                                title_color,
                                TabBar::agent_sweep_color(
                                    visual.agent_accent_rgb.unwrap_or(0),
                                    title_color,
                                    surface,
                                    theme.accent,
                                ),
                            )),
                    )
                }),
        )
        .when(!visual.cwd.is_empty(), |this| {
            this.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(TAB_TEXT_PX))
                    .text_color(cwd_color)
                    .child(visual.cwd.clone()),
            )
        })
        // herdr 的「正在干活」指示：转圈。它由「真的在输出」驱动，而不是
        // 回合状态——一个静默的 agent 不该永远转。
        .when(visual.streaming, |this| {
            this.child(
                svg()
                    .flex_none()
                    .size(px(12.0))
                    .text_color(theme.warning)
                    .path(Icon::Loader.path())
                    .with_animation(
                        format!("tab-working-{}", visual.animation_key),
                        Animation::new(Duration::from_millis(900))
                            .repeat()
                            .with_easing(ease_in_out),
                        |this, delta| {
                            this.with_transformation(Transformation::rotate(percentage(delta)))
                        },
                    ),
            )
        });

    let mut body = div()
        .h_full()
        .w(visual.width)
        .flex_shrink_1()
        .min_w(px(TAB_MIN_WIDTH_PX))
        .pl(px(5.0))
        .pr(px(12.0))
        .gap(px(9.0))
        .flex()
        .flex_row()
        .items_center()
        .rounded(px(TAB_RADIUS_TRAY_PX))
        .border_1()
        .when(is_active, |this| {
            this.bg(tray.selected).border_color(tray.selected_edge)
        })
        .when(!is_active, |this| {
            let this = this.border_color(gpui::transparent_black());
            if interactive {
                this.hover(move |style| style.bg(tray.hover))
            } else {
                this
            }
        })
        .child(TabBar::render_program_tiles(
            &visual.tiles,
            visual.dot,
            tray,
            theme,
        ))
        .child(label);

    body = match close {
        Some(close) => body.child(close),
        // 预览里没有关闭按钮，但**留同样的宽度**：两边文字能截断到同样长度，
        // 拖动时标签不会突然变长。
        None => body.child(div().flex_none().w(px(16.0)).h(px(16.0))),
    };
    body
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

/// 一次「前台状态」快照：agent 身份 + 回合状态 + 程序外观。
///
/// 三者都来自同一次前台探测，必须一起写入标签：分开传（`set_tab_agent` 再
/// 跟一个 `modify_tab` 改程序）会在两帧之间出现「图标是 vim、状态点还是
/// agent 的」这种撕裂组合。参数超过 7 个也会被 clippy 拦，聚成结构体后
/// 调用点读起来也清楚。
#[derive(Clone, Debug, Default)]
pub struct TabAgentSnapshot {
    /// 前台 CLI agent；`None` = 前台不是 agent（或 agent 已退出）。
    pub agent: Option<CLIAgent>,
    /// agent 的回合状态（只在 `agent` 为 `Some` 时有意义）。
    pub status: AgentStatus,
    /// 此刻是否真的在输出（见 [`TabItem::agent_streaming`]）。
    pub streaming: bool,
    /// agent 退出后图标回落成什么（由调用方决定）。
    pub fallback: Option<Icon>,
    /// 前台程序快照；`None` = 这一帧没有新信息，保持原值。
    pub program: Option<crate::programs::Program>,
}

/// 标签左侧方块的内容（herdr terminals-bar 的 `tile()`）。
///
/// 与 [`crate::programs::Program`] 分开：这里是**渲染视图**——图标可能来自
/// 程序表，也可能回落成标签自带的 `Icon`（恢复会话时还没有程序快照）。
#[derive(Clone, Debug, PartialEq)]
pub struct TileProgram {
    /// 图标路径（`icons/programs/vim.svg` 或 `icons/macos.svg`）。
    pub icon: Option<String>,
    /// 显示名，目前用于无障碍与拖拽预览。
    pub name: String,
    /// 方块底色（程序表里的品牌色）。
    pub tile: Option<u32>,
    /// 单色图形的颜色。
    pub color: Option<u32>,
    /// 是否按 `color` 着色（单色 mask）；否则当彩色图片画。
    pub tint: bool,
}

impl TileProgram {
    /// 从程序表解析结果构造方块。
    pub fn from_program(program: &crate::programs::Program) -> Self {
        Self {
            icon: program.icon.clone(),
            name: program.name.clone(),
            tile: program.tile,
            color: program.color,
            tint: program.is_tinted(),
        }
    }
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
    /// 标签的目录段（herdr terminals-bar 的 `where`）：弱化色画在名字之后。
    ///
    /// 空串表示没有可显示的目录。与 [`Self::label`] 同源（都来自
    /// `TerminalView::tab_title_parts`），只是拆成了两段。
    pub cwd_label: SharedString,
    /// 前台程序快照：标签左侧方块的图标、底色与名字都来自它。
    ///
    /// `None` 表示「还不知道」（恢复会话、刚建标签、编辑器标签）——此时方块
    /// 退回主题色 + 既有 `icon`，不要凭空编一个程序名。
    pub program: Option<crate::programs::Program>,
    /// 这个标签里**每个 panel** 正在跑的程序（按面板顺序），供叠放渲染。
    ///
    /// herdr 的标签永远对应一个终端，所以它的 `tile()` 只画一个方块；
    /// deepterm 的标签可以容纳多个分屏 panel，于是把它们的程序方块横向
    /// 叠起来——一眼能看出「这个标签里有几个东西在跑」。
    ///
    /// 空表示没有面板程序信息（编辑器标签、恢复会话的旧状态），此时退回
    /// 单个 `program` 方块。顺序即绘制顺序，最后一个在最上层。
    pub tiles: Vec<TileProgram>,
    /// 这个 agent 此刻是否**真的在输出**（`Working` + 最近有 PTY 输出）。
    ///
    /// 与 [`Self::agent_status`] 分开：`Working` 是回合状态，会被迟到的
    /// `Stop`（或不发 `Stop` 的 agent）拖住；扫光动画只在真的滚字时才转，
    /// 所以它看的是这个字段，而不是 `agent_status == Working`。
    pub agent_streaming: bool,
}

impl TabItem {
    /// 是否该画「正在输出」的扫光：`Working` 是回合状态，`agent_streaming`
    /// 才是「屏幕真的在动」。两者都为真才画——只认回合状态会让一个静默的
    /// agent（或丢了 `Stop` 的 agent）把扫光一直转下去。
    pub fn is_agent_streaming(&self) -> bool {
        self.agent_streaming && self.agent_status == Some(AgentStatus::Working)
    }

    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            is_modified: false,
            is_remote: false,
            is_loading: false,
            needs_attention: false,
            cwd_label: SharedString::default(),
            program: None,
            tiles: Vec::new(),
            agent_status: None,
            agent_streaming: false,
        }
    }

    /// 标签上要画的全部程序方块：多个 panel 时横向叠放，**选中的那个排在最后**
    /// （绘制顺序的最后 = 视觉最上层）。
    ///
    /// 没有 `tiles` 时退回单个 `program`（与 herdr 的单终端标签一致）。
    pub fn stacked_tiles(&self, selected_index: Option<usize>) -> Vec<TileProgram> {
        if self.tiles.is_empty() {
            return vec![self.tile_program()];
        }
        let mut tiles = self.tiles.clone();
        // 把选中的那个挪到末尾：重叠渲染时后面的盖住前面的。
        if let Some(index) = selected_index.filter(|index| *index < tiles.len()) {
            let selected = tiles.remove(index);
            tiles.push(selected);
        }
        tiles
    }

    /// 标签左侧方块上要画的程序（herdr 的「program tile」）。
    ///
    /// 没有程序快照时回落成「用现有 `icon` + 主题色」的假程序，
    /// 保证所有调用点都能无分支地画方块。
    pub fn tile_program(&self) -> TileProgram {
        match &self.program {
            Some(program) => TileProgram {
                icon: program
                    .icon
                    .clone()
                    .or_else(|| self.icon.map(|icon| icon.path().to_string())),
                name: program.name.clone(),
                tile: program.tile,
                color: program.color,
                tint: program.is_tinted(),
            },
            None => TileProgram {
                icon: self.icon.map(|icon| icon.path().to_string()),
                name: self.label.to_string(),
                tile: None,
                color: None,
                tint: true,
            },
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
    /// 顶部「空间胶囊」的文案（herdr 的 space picker）：标签页组名。
    ///
    /// `None` = 不显示胶囊（编辑器栏、或调用方没接线）。
    workspace_chip_label: Option<SharedString>,
    /// 点击胶囊的回调（打开标签页组选择器）。
    on_workspace_chip: Option<NewTabCallback>,
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
            workspace_chip_label: None,
            on_workspace_chip: None,
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

    /// 设置顶部「空间胶囊」：文案 + 点击回调；`None` 表示不显示。
    pub fn set_workspace_chip(
        &mut self,
        label: Option<SharedString>,
        on_click: Option<NewTabCallback>,
        cx: &mut Context<Self>,
    ) {
        let changed = self.workspace_chip_label != label
            || self.on_workspace_chip.is_some() != on_click.is_some();
        self.workspace_chip_label = label;
        self.on_workspace_chip = on_click;
        if changed {
            cx.notify();
        }
    }

    /// 当前胶囊文案（接线方切换 workspace 时用它保留/改写入口）。
    pub fn workspace_chip_label(&self) -> Option<&SharedString> {
        self.workspace_chip_label.as_ref()
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
        snapshot: TabAgentSnapshot,
        cx: &mut Context<Self>,
    ) -> Option<AgentStatus> {
        if !self.accepts_agent_state {
            return None;
        }

        let TabAgentSnapshot {
            agent,
            status,
            streaming,
            fallback,
            program,
        } = snapshot;
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

        // 图标与程序方块现在各司其职：图标保留给**列表/侧栏**等旧调用点，
        // 标签本体画的是 `program`（herdr 的程序表，含 shell/vim/htop 等
        // 非 agent 程序）。两者都由同一次前台探测产出，一起更新才不会错位。
        let program_changed = program.as_ref() != tab.program.as_ref();

        let previous = tab.agent_status;
        if tab.agent_status == status
            && tab.icon == icon
            && tab.agent_streaming == streaming
            && !program_changed
        {
            return previous;
        }

        tab.agent_status = status;
        tab.icon = icon;
        tab.agent_streaming = streaming;
        if let Some(program) = program {
            tab.program = Some(program);
        }

        cx.notify();
        previous
    }

    /// 用「每个 panel 正在跑的程序」刷新一个标签的方块叠列与标题。
    ///
    /// `panels` 是**已经按面板顺序排好的程序**（选中项在末尾，见调用方），
    /// `title`/`cwd` 取自选中那个 panel。这是「多终端合成一个标签」的显示
    /// 规则：herdr 一个标签对应一个终端，deepterm 一个标签可以是分屏，
    /// 所以用叠放图标 + 选中 panel 的标题来表达「这一格是什么」。
    pub fn set_tab_panels(
        &mut self,
        tab_id: &str,
        panels: Vec<TileProgram>,
        title: SharedString,
        cwd: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else {
            return;
        };
        let changed = tab.tiles != panels || tab.label != title || tab.cwd_label != cwd;
        if !changed {
            return;
        }
        tab.tiles = panels;
        tab.label = title;
        tab.cwd_label = cwd;
        // 注意：不动 `tab.program`。它由现有的 `TabLabelChanged` /
        // `AgentStateChanged` 事件维护，代表"这台标签的前台程序"，
        // 还带着 `shell` / `agent` / `process` 这些叠放列表里放不下的语义。
        // 渲染时"最上层是哪个"由 `stacked_tiles` 的排序决定，不靠它。
        cx.notify();
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
        self.set_panel_drop_preview(
            insert_index,
            dragged.tab_label.clone(),
            dragged.tile_program(),
            cx,
        );
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
        program: TileProgram,
        cx: &mut Context<Self>,
    ) {
        let next_preview = PanelDropPreview {
            insert_index: insert_index.min(self.tabs.len()),
            tab_label,
            program,
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
                // 用托盘的真实间距：`TAB_REORDER_GAP_PX`(4) 是旧版标签栏的
                // 间距，托盘是 `TRAY_GAP_PX`(3)。差这 1px，重排时让位的标签
                // 会与落点差一点点，累积在多个标签上就是肉眼可见的错位。
                width + TRAY_GAP_PX
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
                children.push(Self::render_panel_drop_preview(
                    theme,
                    Self::tray_colors(theme),
                    preview,
                    px(TAB_WIDTH_PX),
                ));
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
            .gap_1()
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
                            .when(tab.is_agent_streaming(), |this| this.opacity(0.))
                            .child(tab.label.clone()),
                    )
                    .when(tab.is_agent_streaming(), |this| {
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
    fn render_panel_drop_preview(
        theme: UiTheme,
        tray: TrayColors,
        preview: &PanelDropPreview,
        tab_width: Pixels,
    ) -> AnyElement {
        // 插入预览就是"落成后的那个标签"：同一个 body、同宽同高同圆角，
        // 只把底色换成淡淡的强调色、整体降一点透明度表示"还没落地"。
        //
        // 这张预览原来是一个自制的窄胶囊（高 30、宽随文字、圆角 8），
        // 与托盘标签完全不是一个东西——拖到标签中间时会看到一条明显的
        // 窄条，落点宽度也按它算，所以插进去以后标签宽度全乱。
        let visual = TrayTabVisual {
            program: preview.program.clone(),
            tiles: vec![preview.program.clone()],
            title: preview.tab_label.clone(),
            cwd: SharedString::default(),
            is_modified: false,
            needs_attention: false,
            dot: None,
            streaming: false,
            agent_accent_rgb: None,
            // 关键：与普通标签一样**不是选中态**，宽度取**托盘里标签的
            // 实际宽度**而不是名义 260——多标签时标签会被压缩到 96px 下限，
            // 预览却按 260 画，插进去之前看到的就是一条比谁都宽的黑洞。
            is_active: false,
            width: tab_width,
            animation_key: SharedString::from(format!("panel-drop-{}", preview.insert_index)),
        };
        div()
            .id(format!("panel-drop-preview-{}", preview.insert_index))
            .opacity(0.65)
            .child(render_tray_tab_body(&visual, tray, theme, None, false))
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

impl TabBar {
    /// 紧凑模式（编辑器标签栏）的渲染：沿用改造前的样式。
    ///
    /// 终端标签栏换成 herdr 的托盘样式（见 [`Self::render_tray`]）之后，
    /// 编辑器栏保持原样：它显示的是文件，不是「终端里跑着什么程序」，
    /// 套用程序方块与 260px 宽标签只会让人误读。
    fn render_compact(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
            .into_any_element()
    }

    /// herdr Terminals LAYOUT 的顶栏标签：一条可横向滚动的「托盘」，
    /// 里面是间距 3px、圆角 13px 的标签，选中的那块抬起来。
    ///
    /// 与旧版标签栏的区别（都是 herdr 的取舍，不是随手改的尺寸）：
    /// - 标签**等宽**（`w(260)` + `flex_shrink(1)` + `min_w(0)`）：外壳不随内容
    ///   抖动，切换标签时标签条不会左右蹦；这也是 herdr 敢把 cwd 画在标签里的前提。
    /// - 选中态靠**底色 + 1px 描边**，未选中只有 hover 时的一点高光，两者不会
    ///   看成同一种状态。
    /// - 左侧方块是「程序」（vim / node / claude），不是「操作系统」：图标、
    ///   底色、名字全部来自 `crate::programs`，与 herdr 的 `tile()` 一致。
    /// - 关闭按钮**常驻占位、hover 才显形**，且与程序方块分处两端：herdr 是
    ///   让方块在被悬停时变成 X，但那会让「点标签左侧」变成「关掉这个标签」，
    ///   手一抖就关掉正在看的东西，这里有意不跟。
    fn render_tray(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().ui;
        let tray = Self::tray_colors(theme);
        let on_panel_drop = self.on_panel_drop.clone();

        // 插入预览要占「落成后那个标签」的位置与宽度。标签在托盘里会被
        // 压缩（多标签时贴着 `TAB_MIN_WIDTH_PX` 下限），所以取已有标签的
        // 实际布局宽度；一个标签都没有时退回名义宽度。
        let tab_width = self
            .current_tab_bounds()
            .first()
            .map(|bounds| bounds.size.width)
            .filter(|width| *width > px(0.0))
            .unwrap_or(px(TAB_WIDTH_PX));

        let mut tabs: Vec<AnyElement> =
            Vec::with_capacity(self.tabs.len() + usize::from(self.panel_drop_preview.is_some()));
        // `0..=len`：插入位可以等于 `len`（追加到末尾）。少遍历这一格，
        // 拖到最右边时预览根本不画，用户会以为"拖不进去"。
        for index in 0..=self.tabs.len() {
            if let Some(preview) = self
                .panel_drop_preview
                .as_ref()
                .filter(|preview| preview.insert_index == index)
            {
                tabs.push(Self::render_panel_drop_preview(
                    theme, tray, preview, tab_width,
                ));
            }
            if let Some(tab) = self.tabs.get(index) {
                tabs.push(self.render_tray_tab(tab, index, tray, cx));
            }
        }

        div()
            .id(self.id.clone())
            .h_full()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            // 左键不冒泡到标题栏：否则点标签会先被标题栏当成「按在拖拽区」，
            // 触发窗口移动（macOS 上还会抢走双击）。
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
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
            // 左侧「空间胶囊」：herdr 用一个胶囊显示当前 space 并点开选择器。
            // deepterm 的对应概念是**标签页组**，胶囊显示组名、点开分组选择器。
            // 没接回调时整块不出现（例如单组场景），保持标签条贴左。
            .when_some(
                self.on_workspace_chip
                    .clone()
                    .zip(self.workspace_chip_label.clone()),
                |this, (callback, label)| {
                    this.child(
                        div()
                            .id("workspace-chip")
                            .flex_none()
                            .h(px(30.0))
                            .pl(px(10.0))
                            .pr(px(12.0))
                            .gap(px(8.0))
                            .flex()
                            .items_center()
                            .rounded_full()
                            .bg(tray.chip)
                            .cursor_pointer()
                            .hover(move |style| style.bg(tray.hover))
                            .child(
                                svg()
                                    .size(px(15.0))
                                    .text_color(tray.dim)
                                    .path(Icon::Stack.path()),
                            )
                            .child(
                                div()
                                    .max_w(px(160.0))
                                    .truncate()
                                    .text_size(px(13.0))
                                    .text_color(tray.text)
                                    .child(label),
                            )
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(move |_event, window, cx| {
                                cx.stop_propagation();
                                callback(window, cx);
                            }),
                    )
                },
            )
            .child(
                // 托盘：标签列表的底，`p(3)` + `gap(3)` + 圆角 16 —— herdr 的
                // 「一条胶囊槽里嵌着胶囊标签」。
                div()
                    .id("tab-tray")
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_shrink_1()
                    .min_w_0()
                    // 高度取「36 与父高度」的较小者：标题栏标准高 44，托盘 36；
                    // 但若被塞进更矮的容器（测试窗口、紧凑布局）也不能溢出，
                    // 否则垂直居中后内容被裁掉、标签点不中。
                    .h(px(TRAY_HEIGHT_PX))
                    .max_h_full()
                    .p(px(TRAY_PADDING_PX))
                    .gap(px(TRAY_GAP_PX))
                    .flex_nowrap()
                    .rounded(px(TRAY_RADIUS_PX))
                    .bg(tray.tray)
                    // 溢出滚动**挂在托盘自身**：它宽度是内容推出来的（`flex_shrink_1`
                    // + `min_w_0` 才是上限），套一层 `flex_1` 的内层容器会让托盘在
                    // 「内容宽度 ↔ 弹性子项宽度」之间循环求解，实测解出来是 0 宽，
                    // 标签永远点不到。
                    .overflow_x_scroll()
                    .track_scroll(&self.scroll_handle)
                    .on_drag_move::<DraggedPanel>(cx.listener(|this, event, _window, cx| {
                        this.handle_panel_drag_move(event, cx);
                    }))
                    .on_drag_move::<DraggedTab>(cx.listener(|this, event, _window, cx| {
                        this.handle_tab_drag_move(event, cx);
                    }))
                    .on_drop(cx.listener({
                        let on_tab_reorder = self.on_tab_reorder.clone();
                        let on_tab_click = self.on_tab_click.clone();
                        move |this, dragged: &DraggedTab, window, cx| {
                            cx.stop_propagation();
                            let insert_slot = this
                                .reorder_preview
                                .as_ref()
                                .filter(|preview| preview.dragged_tab.tab_id == dragged.tab_id)
                                .map(|preview| preview.insert_slot);
                            let is_own_tab = this.tabs.iter().any(|t| t.id == dragged.tab_id);
                            let fire_click_as_fallback =
                                |window: &mut Window, cx: &mut Context<TabBar>| {
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
                                fire_click_as_fallback(window, cx);
                            }
                        }
                    }))
                    .children(tabs),
            )
            // 托盘右侧的弹性留白：标签条不铺满整行，空隙留给窗口拖拽
            // （TitleBar 的拖拽区在 TabBar 之后，这里只是不让标签条吃掉它）。
            // `min_w_0` 是必须的：否则这块留白会把托盘挤到最小内容宽度以下。
            .child(div().flex_1().min_w_0().h_full())
            // 右侧「新建标签」图标按钮（herdr 的 iconButton）。
            //
            // herdr 在这个位置还有第二个按钮（命令面板 `app.palette`）。
            // deepterm 没有命令面板，硬凑一个按钮只会多出一个来路不明的
            // 图标——设置入口本来就在侧边栏，这里不复制一份。
            .when_some(self.on_new_tab.clone(), |this, callback| {
                this.child(Self::render_tray_icon_button(
                    "tm-new",
                    Icon::Plus,
                    tray,
                    callback,
                ))
            })
            .into_any_element()
    }

    /// 托盘右侧的图标按钮（herdr 的 `iconButton`）：17px 图形、7px 内边距、
    /// 8px 圆角，hover 时浮出一块底色。
    fn render_tray_icon_button(
        id: &'static str,
        icon: Icon,
        tray: TrayColors,
        on_click: NewTabCallback,
    ) -> AnyElement {
        div()
            .id(id)
            .flex_none()
            .p(px(7.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .hover(move |style| style.bg(tray.chip))
            .child(svg().size(px(17.0)).text_color(tray.dim).path(icon.path()))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(move |_event, window, cx| {
                cx.stop_propagation();
                on_click(window, cx);
            })
            .into_any_element()
    }

    /// 托盘里的单个标签（herdr `terminals-bar` 的 `tab()`）。
    ///
    /// 布局：`[程序方块] [名字 (flex_none)] [目录 (truncate)]`，右侧 hover 时
    /// 让方块变成关闭按钮——与 herdr「图标位置显示关闭」的取舍一致：不额外
    /// 占宽，标签等宽也不抖。
    fn render_tray_tab(
        &self,
        tab: &TabItem,
        index: usize,
        tray: TrayColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().ui;
        let tab_id = tab.id.clone();
        let is_active = self.active_tab_id.as_ref() == Some(&tab.id);
        let is_hovered = self.hovered_tab_id.as_ref() == Some(&tab_id);

        let start_offset =
            self.tab_offset_for_preview(self.previous_reorder_preview.as_ref(), index);
        let end_offset = self.tab_offset_for_preview(self.reorder_preview.as_ref(), index);
        let start_opacity =
            self.tab_opacity_for_preview(self.previous_reorder_preview.as_ref(), index);
        let end_opacity = self.tab_opacity_for_preview(self.reorder_preview.as_ref(), index);

        // 标签本体**永远按名义宽度参与布局**（`TAB_WIDTH_PX` + `flex_shrink`），
        // 由托盘去压缩它。绝不能拿"上一帧量到的宽度"回填给自己：
        // 那是个自我固化的反馈环——一旦某帧被压到 96px，之后每帧都按
        // 96px 起算，`flex_shrink` 再也压不回 260，标签就永远窄着。
        // （曾经这么写过，表现是插入/删除标签后整排宽度再也回不去。）
        let visual = self.tray_tab_visual(tab, px(TAB_WIDTH_PX));

        // 只有**拖拽预览**需要实际宽度：它是脱离托盘的独立视图，没有
        // flex 容器帮它收缩，必须自己拿准上一帧的布局宽度，才不会比原标签宽。
        let actual_width = self
            .current_tab_bounds()
            .get(index)
            .map(|bounds| bounds.size.width)
            .filter(|width| *width > px(0.0))
            .unwrap_or(px(TAB_WIDTH_PX));
        let mut drag_visual = visual.clone();
        // 预览里的动画键加后缀：同一个标签在托盘与预览里同时存在，扫光 / 转圈
        // 的动画 id 必须唯一，否则两处争同一份动画状态。
        drag_visual.animation_key = SharedString::from(format!("drag-{}", tab_id));

        // 关闭槽是唯一带交互的部分：交给共用 body；预览里传 None，
        // 但 body 会留同样宽的空位，保证两边文字截断长度一致。
        let close = Self::render_tray_close_slot(
            tab_id.clone(),
            is_active,
            is_hovered,
            tray,
            self.on_tab_close.clone(),
            cx,
        );

        let tab_inner = render_tray_tab_body(&visual, tray, theme, Some(close), true)
            .id(tab_id.clone())
            .cursor_pointer()
            .on_hover(cx.listener({
                let tab_id = tab_id.clone();
                move |this, hovered: &bool, _window, cx| {
                    let next = hovered.then(|| tab_id.clone());
                    if this.hovered_tab_id != next {
                        this.hovered_tab_id = next;
                        cx.notify();
                    }
                }
            }))
            .on_drag(
                DraggedTab {
                    tab_id: tab_id.clone(),
                    from_index: index,
                },
                move |_drag_data, position, _window, cx| {
                    cx.stop_propagation();
                    let visual = drag_visual.clone();
                    let width = actual_width;
                    cx.new(move |_cx| DragPreview::tab(visual, width, position))
                },
            )
            .on_click(cx.listener({
                let on_tab_click = self.on_tab_click.clone();
                let tab_id = tab_id.clone();
                move |_this, _event: &gpui::ClickEvent, window, cx| {
                    if let Some(ref callback) = on_tab_click {
                        callback(tab_id.clone(), window, cx);
                    }
                }
            }))
            // 左键：阻止冒泡到标题栏触发窗口拖拽
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            // 中键关闭（浏览器/终端通用习惯）
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
            .context_menu(format!("tab-menu-{}", tab_id), {
                let tab_id = tab_id.clone();
                let on_context_menu = self.on_context_menu.clone();
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
            return div()
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
                .into_any_element();
        }
        tab_inner.into_any_element()
    }

    /// 把一个标签的「长什么样」抽成快照：托盘渲染与拖拽预览共用。
    ///
    /// `width` 由调用方给：标签在托盘里允许被压缩，预览要按同一个宽度画，
    /// 否则一松手就「变宽了」。
    fn tray_tab_visual(&self, tab: &TabItem, width: Pixels) -> TrayTabVisual {
        // agent 回合状态点压在程序方块上（herdr 同款）。只在真有程序方块
        // 时画：恢复会话等场景下 program/icon 都可能为空，空方块上点一个点
        // 既没信息量，也会让一次状态变化改到与它无关的几何。
        let dot = (tab.program.is_some() || matches!(tab.icon, Some(Icon::AgentIcon { .. })))
            .then_some(tab.agent_status)
            .flatten();
        // 叠放顺序由 `stacked_tiles` 定：选中的 panel 在最后 = 最上层。
        // 这里没有"面板选中索引"的信息（`TabBar` 只认识标签），所以按
        // `tiles` 的原序叠放；调用方（workspace）在写入 `tiles` 时已经把
        // 选中的那个排到了末尾。
        let tiles = tab.stacked_tiles(None);
        TrayTabVisual {
            program: tiles.last().cloned().unwrap_or_else(|| tab.tile_program()),
            tiles,
            title: tab.label.clone(),
            cwd: tab.cwd_label.clone(),
            is_modified: tab.is_modified,
            needs_attention: should_show_attention_dot(tab),
            dot,
            streaming: tab.is_agent_streaming(),
            agent_accent_rgb: tab.agent_accent_rgb(),
            is_active: self.active_tab_id.as_ref() == Some(&tab.id),
            width,
            animation_key: SharedString::from(format!("tray-{}", tab.id)),
        }
    }

    /// hover 时方块位置显示关闭按钮（herdr / 旧版 deepterm 的共同取舍：
    /// 不额外占宽，标签等宽不抖）。
    fn render_tray_close_slot(
        tab_id: SharedString,
        is_active: bool,
        is_hovered: bool,
        tray: TrayColors,
        on_tab_close: OptionalTabCallback,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(format!("tab-close-{}", tab_id))
            .flex_none()
            .w(px(16.0))
            .h(px(16.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            // 常驻占位、hover 才显形：占位恒定，标签不会因为「鼠标移上来」
            // 改变宽度（那会让整排标签左右跳）。
            .opacity(if is_hovered { 1.0 } else { 0.0 })
            .hover(move |style| style.bg(tray.hover))
            .child(
                svg()
                    .size(px(12.0))
                    .text_color(if is_active { tray.text } else { tray.muted })
                    .path(Icon::XmarkCircleFill.path()),
            )
            // 用 click 而不是 mouse_down：mouse_down 里 stop_propagation 会把
            // 父标签的 click 候选一起掐掉，正中点击反而选不中这个标签。
            .on_click(
                cx.listener(move |_this, _event: &gpui::ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if let Some(ref callback) = on_tab_close {
                        callback(tab_id.clone(), window, cx);
                    }
                }),
            )
            .into_any_element()
    }

    /// herdr terminals-bar 的托盘颜色：标签条自己是一块比背景略深的底，
    /// 选中的标签从这块底上「浮」出来。
    ///
    /// 五档颜色全部从主题令牌推导，不写死 herdr 的 `#1d1f22` / `#34373c`
    /// 那套 hex：deepterm 有 31 个明暗主题，写死会在一半主题上糊掉。
    fn tray_colors(theme: UiTheme) -> TrayColors {
        let dark = theme.text.l > 0.5;
        // 托盘底：比应用背景深一档（暗色主题）或深半档（亮色主题）。
        let tray = if dark {
            theme.app_background.blend(gpui::rgba(0x00000026).into())
        } else {
            theme.app_background.blend(gpui::rgba(0x00000012).into())
        };
        // 选中标签：抬起一档，与托盘拉开距离；描边再亮一点，形成 herdr 的
        // 1px「边缘高光」。
        let selected = theme.elevated_background;
        let selected_edge = if dark {
            theme.border.blend(gpui::rgba(0xffffff14).into())
        } else {
            theme.border.blend(gpui::rgba(0x00000014).into())
        };
        TrayColors {
            tray,
            selected,
            selected_edge,
            // 未选中标签 hover：极轻的白色（暗色）/ 黑色（亮色）覆盖。
            hover: if dark {
                gpui::rgba(0xffffff0d).into()
            } else {
                gpui::rgba(0x0000000a).into()
            },
            // 胶囊（左侧空间按钮）与右侧图标按钮的底。
            chip: theme.panel_background.blend(if dark {
                gpui::rgba(0xffffff0a).into()
            } else {
                gpui::rgba(0x00000008).into()
            }),
            text: theme.text,
            muted: theme.text_muted,
            dim: theme.icon_muted,
        }
    }

    /// 小尺寸版本，给侧边栏这类行高 28px 的位置用。
    ///
    /// 托盘里的方块是 26px；侧边栏的图标位只有 16px，直接塞进去会被裁掉
    /// 或者把行撑高。这里按比例缩到 18px、错位 4px，多 panel 时同样叠放，
    /// 只是更紧凑。
    pub fn render_program_tiles_small(tiles: &[TileProgram], theme: UiTheme) -> AnyElement {
        let tray = Self::tray_colors(theme);
        let size = 18.0;
        let step_px = 4.0;
        let visible = tiles.len().min(TILE_STACK_MAX);
        let shown = &tiles[tiles.len() - visible..];

        if visible <= 1 {
            let Some(tile) = shown.first() else {
                return div().size(px(size)).into_any_element();
            };
            return Self::render_tile_at(tile, size, 0.62, None, false, tray, theme);
        }

        let width = px(size) + px(step_px) * (visible as f32 - 1.0);
        let mut stack = div().relative().flex_none().w(width).h(px(size));
        for (index, tile) in shown.iter().enumerate() {
            stack = stack.child(
                div()
                    .absolute()
                    .left(px(step_px) * index as f32)
                    .top_0()
                    // 小尺寸下不做动画：侧边栏会随切换重绘，短寿命的动画
                    // 只是无谓的开销，也会让列表看起来在抖。
                    .child(Self::render_tile_at(
                        tile, size, 0.62, None, false, tray, theme,
                    )),
            );
        }
        stack.into_any_element()
    }

    fn render_program_tiles(
        tiles: &[TileProgram],
        status: Option<AgentStatus>,
        tray: TrayColors,
        theme: UiTheme,
    ) -> AnyElement {
        let visible = tiles.len().min(TILE_STACK_MAX);
        // 只保留最后 `visible` 个：最后那个是选中项，必须完整可见。
        let shown = &tiles[tiles.len() - visible..];

        let single = shown.first().cloned().unwrap_or(TileProgram {
            icon: None,
            name: String::new(),
            tile: None,
            color: None,
            tint: true,
        });
        if visible <= 1 {
            return Self::render_program_tile(&single, status, tray, theme);
        }

        let step = px(TILE_STACK_STEP_PX);
        // 最高的一层（最上层）最大。
        let top_px =
            TILE_SIZE_PX * (TILE_STACK_SCALE + TILE_STACK_SCALE_STEP * (visible as f32 - 1.0));
        // 整叠宽度：最后一层完整、前面每层各让出 `step`。
        let width = px(top_px) + step * (visible as f32 - 1.0);
        // 高度取满尺寸的槽位：叠放的方块比单个略小，但**占位高度不变**，
        // 否则标签里的文字基线会随 panel 数量上下跳。
        let slot = px(TILE_SIZE_PX);

        let mut stack = div()
            .relative()
            .flex_none()
            .w(width)
            .h(slot)
            .flex()
            .items_center();

        for (index, tile) in shown.iter().enumerate() {
            let is_top = index + 1 == visible;
            // 从下到上逐层放大：这是唯一的层次线索，不加任何描边。
            let layer_px = TILE_SIZE_PX * (TILE_STACK_SCALE + TILE_STACK_SCALE_STEP * index as f32);
            // 过渡：新上来的那层从左侧小幅滑入并淡入，其余层同时把位置
            // 让到位。动画 id 带上程序名，切 panel 时新旧两层拿到不同的
            // id，才会各自播放一次而不是直接跳变。
            let anim_id = format!("stack-{}", tile.name);
            let target_left = step * index as f32;
            stack = stack.child(
                // 定位交给动画闭包统一设置：闭包里的 `left` 会覆盖外层的，
                // 两处都写等于让「起点」和「终点」各说各话。
                div()
                    .absolute()
                    // 竖向居中：叠放整体比槽位矮一点，居中才不会看着像浮在上面。
                    .top(px((TILE_SIZE_PX - layer_px) / 2.0))
                    .size(px(layer_px))
                    .with_animation(
                        anim_id,
                        Animation::new(Duration::from_millis(TILE_STACK_ANIM_MS))
                            .with_easing(ease_out_quint()),
                        move |this, delta| {
                            // 从左侧 4px 处滑入 + 淡入，落到目标位置。
                            let dx = px(4.0) * (1.0 - delta);
                            this.left(target_left + dx).opacity(0.55 + 0.45 * delta)
                        },
                    )
                    // 后画的盖住先画的：越靠后越上层。
                    .child(Self::render_tile_at(
                        tile,
                        layer_px,
                        TILE_GLYPH_PX / TILE_SIZE_PX,
                        is_top.then_some(status).flatten(),
                        // 不描边：层次全靠缩放递进与位移，保持扁平。
                        false,
                        tray,
                        theme,
                    )),
            );
        }
        stack.into_any_element()
    }

    /// 叠放中的一层：缩小的程序方块。
    ///
    /// `is_top` 为真时多一圈与托盘同色的细边，把最上层从下层方块里「摘」
    /// 出来。下层不描边：每层都描边就会显得边框很厚、外面还多一圈黑边。
    /// 单个程序方块（herdr 的 `tile()`）：26px 圆角方块，品牌底色 + 图形，
    /// agent 有回合状态时在右上角压一个状态点。
    fn render_program_tile(
        program: &TileProgram,
        status: Option<AgentStatus>,
        tray: TrayColors,
        theme: UiTheme,
    ) -> AnyElement {
        Self::render_tile_at(
            program,
            TILE_SIZE_PX,
            TILE_GLYPH_PX / TILE_SIZE_PX,
            status,
            false,
            tray,
            theme,
        )
    }

    /// 在任意尺寸下画一个程序方块——**所有**方块渲染的唯一实现。
    ///
    /// 托盘单方块、托盘叠放层、侧边栏/标题栏小方块都走它：三种尺寸下
    /// 圆角、描边、图形着色、状态点必须长得一样，各写一份必然漂移。
    ///
    /// - `glyph_ratio`：图形占方块的比例（26px 方块里是 15/26）。
    /// - `ring`：是否描一圈托盘色的边。只有叠放的最上层需要——它把方块
    ///   从下层里「摘」出来；下层与单方块都不描边，否则会显厚、像贴纸。
    fn render_tile_at(
        tile: &TileProgram,
        size: f32,
        glyph_ratio: f32,
        status: Option<AgentStatus>,
        ring: bool,
        tray: TrayColors,
        theme: UiTheme,
    ) -> AnyElement {
        let fill: Hsla = tile
            .tile
            .map(|value| rgb(value).into())
            .unwrap_or(tray.chip);
        let glyph_px = size * glyph_ratio;

        let glyph: AnyElement = match tile.icon.as_ref() {
            Some(path) if tile.tint => svg()
                .size(px(glyph_px))
                .text_color(tile.color.map(|v| rgb(v).into()).unwrap_or(theme.text))
                .path(SharedString::from(path.clone()))
                .into_any_element(),
            Some(path) => gpui::img(SharedString::from(path.clone()))
                .w(px(glyph_px))
                .h(px(glyph_px))
                .into_any_element(),
            None => div().into_any_element(),
        };

        // 圆角随尺寸收缩：26px 用 7，18px 用 5——大圆角在小方块上会显得
        // 像药丸，反而模糊了"这是个程序图标"。
        let radius = if size >= 24.0 {
            7.0
        } else if size >= 20.0 {
            6.0
        } else {
            5.0
        };
        let dot_size = if size >= 24.0 { 11.0 } else { 9.0 };

        div()
            .relative()
            .flex_none()
            .size(px(size))
            .rounded(px(radius))
            .when(ring, |this| this.border_2().border_color(tray.tray))
            .bg(fill)
            .flex()
            .items_center()
            .justify_center()
            .child(glyph)
            .when_some(status.and_then(agent_dot), |this, (dot_rgb, shape)| {
                this.child(
                    div()
                        .absolute()
                        // herdr 是 `top(-4).right(-4)` + 2px 描边，把点从方块上
                        // 「摘」出来；小尺寸下收一点，避免压到相邻层。
                        .top(px(if size >= 24.0 { -4.0 } else { -3.0 }))
                        .right(px(if size >= 24.0 { -4.0 } else { -3.0 }))
                        .size(px(dot_size))
                        .rounded_full()
                        .border_2()
                        .border_color(tray.tray)
                        .when(shape == AgentDot::Filled, |this| this.bg(rgb(dot_rgb)))
                        .when(shape == AgentDot::Hollow, |this| {
                            this.border_1().border_color(rgb(dot_rgb))
                        }),
                )
            })
            .into_any_element()
    }
}

/// 托盘样式的颜色集合（见 [`TabBar::tray_colors`]）。
#[derive(Clone, Copy)]
struct TrayColors {
    tray: Hsla,
    selected: Hsla,
    selected_edge: Hsla,
    hover: Hsla,
    chip: Hsla,
    text: Hsla,
    muted: Hsla,
    dim: Hsla,
}

impl Render for TabBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 紧凑模式就是编辑器标签栏：那边保持旧样式，终端栏走新的托盘样式。
        if self.compact {
            return self.render_compact(window, cx).into_any_element();
        }
        self.render_tray(cx).into_any_element()
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
        cx.update(crate::theme::init);

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
        cx.update(crate::theme::init);

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

    /// 托盘标签的方块取自程序表（herdr 的 `tile()`）；没有程序快照时
    /// 退回标签自带的 `Icon`，不能画出一个空方块。
    #[test]
    fn tile_program_prefers_program_table_and_falls_back_to_icon() {
        let mut tab = TabItem::new("t", "vim").with_icon(Some(Icon::Terminal));
        // 没有程序快照：用标签自带图标，底色留给主题。
        let fallback = tab.tile_program();
        assert_eq!(
            fallback.icon.as_deref(),
            Some(Icon::Terminal.path().as_ref())
        );
        assert_eq!(fallback.tile, None);
        assert!(fallback.tint, "回落时按单色 mask 着色，跟随主题文字色");

        // 有程序快照：图标/底色/名字全部来自程序表。
        tab.program = Some(crate::programs::of(None, &["vim".to_string()]));
        let from_table = tab.tile_program();
        assert_eq!(
            from_table.icon.as_deref(),
            Some("icons/programs/vim.svg"),
            "程序表里的 vim 必须有它自己的图标"
        );
        assert_eq!(from_table.tile, Some(0x019733), "Vim 的品牌绿");
        assert_eq!(from_table.name, "Vim");
    }

    /// 单色程序（`tint`）与彩色程序（Claude / Codex 这类自带配色的 logo）
    /// 必须分别走 SVG 着色与图片两条渲染路径。
    #[test]
    fn tile_program_distinguishes_tinted_and_picture_icons() {
        let tinted = crate::programs::of(None, &["python".to_string()]);
        assert!(tinted.is_tinted(), "python 的图标是单色 mask");
        assert!(!tinted.icon.clone().unwrap().is_empty());

        let picture = crate::programs::of(Some("claude-code"), &[]);
        assert!(!picture.is_tinted(), "Claude 的 logo 自带彩色，不能当 mask");
        assert_eq!(picture.tile, Some(0x1C1D21));
    }

    /// 两段式标签：名字与目录分开存，空目录不占位（herdr 的 `where`）。
    #[test]
    fn cwd_label_stays_empty_when_unknown() {
        let mut tab = TabItem::new("t", "zsh");
        assert!(tab.cwd_label.is_empty());
        tab.cwd_label = "~/Documents/deepterm".into();
        assert_eq!(tab.cwd_label.as_ref(), "~/Documents/deepterm");
    }

    /// 拖拽跟随的预览必须与托盘里的标签**同宽同高同圆角**——用户看到的是
    /// 「把这个标签拿起来」，不是「弹出一个别的框」。
    ///
    /// 这里钉的是可测的那部分：预览视图的宽度字段直接取标签实际宽度，
    /// 且视觉快照（方块 / 标题 / 目录 / 选中态）原样带过去。
    #[gpui::test]
    fn drag_preview_keeps_tab_geometry_and_visual(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let bar = cx.update(|cx| {
            let mut tab = TabItem::new("t1", "vim");
            tab.program = Some(crate::programs::of(None, &["vim".to_string()]));
            tab.cwd_label = "~/Documents/deepterm".into();
            let bar = TabBar::new_with_tabs("preview-bar", cx, vec![tab]);
            bar.update(cx, |bar, _| {
                bar.active_tab_id = Some("t1".into());
            });
            bar
        });

        // 视觉快照与托盘里那一枚同源。
        let visual = bar.read_with(cx, |bar, _| bar.tray_tab_visual(&bar.tabs[0], px(180.0)));
        assert_eq!(visual.title.as_ref(), "vim");
        assert_eq!(visual.cwd.as_ref(), "~/Documents/deepterm");
        assert!(visual.is_active, "被拖的那个通常就是当前标签");
        assert_eq!(visual.program.tile, Some(0x019733));

        // 预览按标签实际宽度构造，不是名义 260。
        let preview = DragPreview::tab(visual, px(180.0), point(px(40.0), px(30.0)));
        assert_eq!(preview.preview_width, px(180.0));
        assert!(
            preview.tray.is_some(),
            "终端标签的预览必须走托盘视觉，不能退回旧的文字胶囊"
        );
    }

    /// 标签本体必须按**名义宽度**参与布局，不能把"上一帧量到的宽度"回填给自己。
    ///
    /// 那是个自我固化的反馈环：某帧被 `flex_shrink` 压到 96px 以后，下一帧
    /// 以 96px 为基准重新布局，`flex_shrink` 再没有余量把它撑回 260——标签
    /// 就永远窄着，插入/删除标签后整排宽度再也回不去。
    ///
    /// 拖拽预览是另一回事：它脱离托盘、没有 flex 容器帮它收缩，必须自己
    /// 拿准实际宽度。两者不能共用同一个取值。
    #[gpui::test]
    fn tab_layout_width_is_nominal_not_measured(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let tabs: Vec<TabItem> = (0..3)
            .map(|i| TabItem::new(format!("t{i}"), format!("tab {i}")))
            .collect();
        let bar = cx.update(|cx| TabBar::new_with_tabs("width-bar", cx, tabs));

        // `tray_tab_visual` 收到的宽度就是 body 的 `w(..)`；托盘渲染走名义宽度。
        let visual = bar.read_with(cx, |bar, _| {
            bar.tray_tab_visual(&bar.tabs[0], px(TAB_WIDTH_PX))
        });
        assert_eq!(
            visual.width,
            px(TAB_WIDTH_PX),
            "标签本体必须按名义宽度布局，否则 flex_shrink 会把它永久压窄"
        );

        // 下限必须小于名义宽度，否则"可压缩"这件事本身就不成立。
        assert!(
            px(TAB_MIN_WIDTH_PX) < px(TAB_WIDTH_PX),
            "TAB_MIN_WIDTH_PX 必须小于 TAB_WIDTH_PX，否则 flex_shrink 无从生效"
        );
    }

    /// 插入预览必须和落成后的标签**同宽同高同圆角**，且宽度跟随托盘的
    /// 实际标签宽度——多标签时标签会被压缩到 96px，预览却按 260 画的话，
    /// 插进去之前看到的就是一条比谁都宽的黑洞。
    ///
    /// 这里钉住三条不变量（都是上一版真实踩过的）：
    /// 1. 预览与标签共用同一个 body（圆角/高度/方块都来自 `TrayTabVisual`）；
    /// 2. 预览宽度由调用方传入，不是内部写死；
    /// 3. 预览不是选中态（不会画出「抬起」的底色，以免看起来像已存在）。
    #[test]
    fn panel_drop_preview_matches_tab_geometry() {
        let program = TabItem::new("probe", "vim").tile_program();
        let preview = PanelDropPreview {
            insert_index: 1,
            tab_label: "vim".into(),
            program: program.clone(),
        };

        // body 的高度与圆角来自常量，预览与标签共用同一套。
        // 注意高度**恰好**等于旧胶囊的 30px（36 - 3×2 = 30）——这是巧合，
        // 不是"没有改动"：真正变的是圆角、方块、两段文字与底色。
        assert_eq!(TRAY_TAB_HEIGHT_PX, TRAY_HEIGHT_PX - TRAY_PADDING_PX * 2.0);
        assert_ne!(
            TAB_RADIUS_TRAY_PX, TAB_RADIUS_PX,
            "托盘标签圆角不再复用旧胶囊的 8px"
        );

        // 预览保留程序方块：插进标签栏后显示什么，拖动中就显示什么。
        assert_eq!(preview.program.name, program.name);
        assert_eq!(preview.tab_label.as_ref(), "vim");
    }

    /// 两条拖拽入口必须画出**同一个**标签：顶部标签栏拖的是 `TabItem`，
    /// 面板标题栏拖的是 `TerminalView`，但落到预览上必须是同一套视觉。
    ///
    /// 这条钉住的是「视觉来源」而不是像素：两边的方块、标题、目录、状态点
    /// 都必须来自 `TerminalView` 的同一组方法。历史 bug 正是面板那条走了
    /// 另一条渲染分支（单行文字胶囊），同一个终端从两处拖起来长得不一样。
    #[gpui::test]
    fn panel_and_tab_drag_share_one_visual(cx: &mut gpui::TestAppContext) {
        use crate::terminal::Terminal;
        use crate::terminal_view::terminal_view::TerminalView;

        cx.update(crate::theme::init);
        cx.update(crate::sidebar_view::font_settings::FontState::init);

        let view = cx.new(|cx| {
            let terminal = Terminal::new_headless(cx, 200);
            TerminalView::build_headless(cx, terminal)
        });
        cx.run_until_parked();

        // 面板那条：从 TerminalView 直接构造视觉。
        let from_panel = view.read_with(cx, |view, cx| {
            TrayTabVisual::from_terminal_view(view, true, px(TAB_WIDTH_PX), "panel-probe", cx)
        });

        // 顶部那条的等价输入：把同一个视图的标题/程序塞进一个 TabItem，
        // 走 TabBar 的视觉提取。
        let (title, cwd) = view.read_with(cx, |view, cx| view.tab_title_parts(cx));
        let program = view.read_with(cx, |view, cx| view.foreground_program(cx));
        let mut tab = TabItem::new("probe", title.clone());
        tab.cwd_label = cwd.clone().into();
        tab.program = Some(program.clone());
        let bar = cx.update(|cx| TabBar::new_with_tabs("probe-bar", cx, vec![tab]));
        let from_tab = bar.read_with(cx, |bar, _| {
            bar.tray_tab_visual(&bar.tabs[0], px(TAB_WIDTH_PX))
        });

        // 同一条终端：名字 / 目录 / 程序方块必须逐项相等。
        assert_eq!(from_panel.title, from_tab.title, "两处拖拽的标题必须一致");
        assert_eq!(from_panel.cwd, from_tab.cwd, "两处拖拽的目录必须一致");
        assert_eq!(
            from_panel.program.icon, from_tab.program.icon,
            "两处拖拽的程序图标必须一致"
        );
        assert_eq!(
            from_panel.program.tile, from_tab.program.tile,
            "两处拖拽的方块底色必须一致"
        );
        assert_eq!(from_panel.dot, from_tab.dot, "两处拖拽的状态点必须一致");
        assert_eq!(
            from_panel.streaming, from_tab.streaming,
            "两处拖拽的扫光状态必须一致"
        );

        // 高度：预览必须按**标签在托盘里的实际高度**画（托盘高减去上下
        // 内边距），不是托盘外框高度——直接用 36 会让预览比原标签高 6px。
        assert_eq!(TRAY_TAB_HEIGHT_PX, TRAY_HEIGHT_PX - TRAY_PADDING_PX * 2.0);
    }

    /// 预览不能只有一个"浮动"的方块和文字：未选中标签本身是透明底，在托盘里
    /// 靠托盘托着，脱离托盘后必须自己补一层底色，否则看起来就是文字在飘。
    ///
    /// 这里钉住的是「补底色这件事必须发生」这个不变量——托盘底色与选中标签
    /// 底色必须不同，否则补上去也看不出来。
    #[gpui::test]
    fn preview_has_a_surface_not_just_text(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let theme = cx.update(|cx| cx.theme().ui);
        let tray = TabBar::tray_colors(theme);
        assert_ne!(
            tray.tray, tray.selected,
            "托盘底与选中标签底必须不同：预览靠补托盘底来避免「文字浮空」"
        );
        assert_ne!(
            tray.tray, tray.hover,
            "hover 高光不能与托盘底同色，否则悬停提示看不见"
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
                TabAgentSnapshot {
                    agent: Some(CLIAgent::Claude),
                    status: AgentStatus::Working,
                    streaming: true,
                    ..Default::default()
                },
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
                TabAgentSnapshot {
                    agent: Some(CLIAgent::Claude),
                    status: AgentStatus::Working,
                    streaming: true,
                    ..Default::default()
                },
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

    /// 扫光只看「真的在输出」，不看回合状态。
    ///
    /// `Working` 是 agent 上报的回合状态，可能被迟到的 `Stop`（旧版 opencode
    /// 桥接甚至没有映射回合结束事件）拖住几十分钟；扫光若只看它，agent
    /// 安静下来后还会继续转。
    #[test]
    fn shimmer_requires_streaming_not_just_working() {
        use crate::agent::AgentStatus;

        let working = TabItem {
            agent_status: Some(AgentStatus::Working),
            agent_streaming: false,
            ..TabItem::new("t", "terminal")
        };
        assert!(
            !working.is_agent_streaming(),
            "Working 但已停止输出时不该扫光"
        );

        let streaming = TabItem {
            agent_streaming: true,
            ..working
        };
        assert!(
            streaming.is_agent_streaming(),
            "Working 且正在输出时应当扫光"
        );

        let done = TabItem {
            agent_status: Some(AgentStatus::Done),
            agent_streaming: true,
            ..TabItem::new("t", "terminal")
        };
        assert!(
            !done.is_agent_streaming(),
            "回合已结束时即便有残余输出也不该扫光"
        );
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

    /// 叠放必须「扁平」：每层缩放后**小于**单个方块，层间只靠位移分开，
    /// 且顶层缩放不能小于下层的字号可辨下限。
    ///
    /// 用户反馈过两版问题：一是每层都描边 → 边框很厚、外面还有一圈黑边；
    /// 二是错位太大 → 叠得又宽又散。这条把几何与「只有顶层描边」钉住。
    #[test]
    fn stacked_tiles_stay_flat_and_compact() {
        // 每层都比单方块小，否则叠起来比单个还大，"折叠"感就没了。
        assert!(TILE_STACK_SCALE < 1.0, "叠放层必须缩小，不能与单方块同尺寸");
        // 缩得不能太狠：18px 以下图形就糊了。
        let layer = TILE_SIZE_PX * TILE_STACK_SCALE;
        assert!(layer >= 18.0, "叠放层 {layer}px 太小，图形会糊成一团");
        // 最上层必须比下层大，层次才看得出来——叠放层一律不描边，
        // 缩放递进是唯一的层次线索。
        assert!(
            TILE_STACK_SCALE_STEP > 0.0,
            "逐层缩放必须有递进，否则叠起来像一块"
        );
        let top = TILE_SIZE_PX
            * (TILE_STACK_SCALE + TILE_STACK_SCALE_STEP * (TILE_STACK_MAX as f32 - 1.0));
        assert!(top <= TILE_SIZE_PX + 0.5, "最上层 {top}px 不应超过单个方块");
        // 错位要明显小于层宽，否则叠出来是一串而不是一叠。
        assert!(
            TILE_STACK_STEP_PX < layer / 2.0,
            "错位 {}px 相对 {layer}px 的层宽太大，会显得又宽又散",
            TILE_STACK_STEP_PX
        );
        // 整叠宽度仍要明显窄于两个并排方块，才值得用"叠"。
        let width = layer + TILE_STACK_STEP_PX * (TILE_STACK_MAX as f32 - 1.0);
        assert!(
            width < TILE_SIZE_PX * 2.0,
            "{width}px 的整叠已超过两个方块宽，叠放失去意义"
        );
        // 层数上限不能是 1：那就等于没做叠放。
        assert!(TILE_STACK_MAX >= 2);
    }

    /// 一个标签里有多个 panel 时，程序方块按「叠放」渲染：选中的那个在
    /// **最后**（绘制顺序的最后 = 视觉最上层），其余保持面板顺序。
    ///
    /// 这是"多终端合成一格"的核心显示规则，坏掉的表现是：切了 panel 但
    /// 标签上的图标不跟着换到最前面，或者干脆只显示一个方块。
    #[test]
    fn stacked_tiles_put_the_selected_panel_on_top() {
        let vim = crate::programs::of(None, &["vim".to_string()]);
        let node = crate::programs::of(None, &["node".to_string()]);
        let claude = crate::programs::of(Some("claude-code"), &[]);

        let mut tab = TabItem::new("t", "vim");
        tab.tiles = vec![
            TileProgram::from_program(&vim),
            TileProgram::from_program(&node),
            TileProgram::from_program(&claude),
        ];

        // 没有选中信息：原序。
        let plain = tab.stacked_tiles(None);
        assert_eq!(plain.len(), 3);
        assert_eq!(plain[0].name, vim.name);
        assert_eq!(plain[2].name, claude.name);

        // 选中中间那个（node）：它被挪到末尾 = 最上层。
        let stacked = tab.stacked_tiles(Some(1));
        assert_eq!(stacked.len(), 3, "叠放不能丢方块");
        assert_eq!(
            stacked.last().map(|t| t.name.as_str()),
            Some(node.name.as_str()),
            "选中的 panel 必须排在最上层"
        );
        assert_eq!(stacked[0].name, vim.name, "其余保持相对顺序");
        assert_eq!(stacked[1].name, claude.name);

        // 越界的选中索引不该 panic，也不该丢东西。
        let out_of_range = tab.stacked_tiles(Some(99));
        assert_eq!(out_of_range.len(), 3);

        // 一个 panel 都没有 `tiles` 时退回单个 program 方块。
        let single = TabItem::new("t2", "zsh");
        assert_eq!(single.stacked_tiles(None).len(), 1);
    }

    /// 发行版 OS 图标已整体移除：`RemoteOS::icon()` 只给通用图标，
    /// 且 `assets/icons/` 下不再有那些发行版 logo。
    #[test]
    fn os_logos_are_gone_from_the_bundle() {
        use gpui::AssetSource as _;
        for stale in [
            "icons/macos.svg",
            "icons/ubuntu.svg",
            "icons/debian.svg",
            "icons/centos.svg",
            "icons/arch.svg",
            "icons/windows.svg",
            "icons/linuxmint.svg",
            "icons/fedora.svg",
            "icons/rhel.svg",
            "icons/opensuse.svg",
            "icons/manjaro.svg",
            "icons/linux.svg",
            "icons/workspace.svg",
        ] {
            assert!(
                crate::assets::Assets.load(stale).is_err()
                    || matches!(crate::assets::Assets.load(stale), Ok(None)),
                "{stale} 应当已从资源里删除"
            );
        }
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
