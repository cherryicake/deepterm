use crate::components::callbacks::OptionalWindowCallback;
use crate::components::animation::easing;
use crate::theme::ActiveTheme;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, BoxShadow, Context, ElementId,
    Entity, InteractiveElement, IntoElement, KeyBinding, ParentElement, Pixels, RenderOnce,
    StatefulInteractiveElement, StyleRefinement, Styled, Task, Window, actions, deferred, div,
    linear, point, px,
};
use std::rc::Rc;
use std::time::Duration;

const CONTEXT: &str = "Modal";

/// 弹窗进入动画时长（毫秒）。macOS sheet 由顶部轻柔落下、末段减速。
const MODAL_ENTER_MS: u64 = 340;
/// 弹窗退出动画时长（毫秒）。向上快速收拢消失。
const MODAL_EXIT_MS: u64 = 200;
/// 进入 / 退出动画的垂直位移（从上方滑入），像素。
const MODAL_SLIDE_PX: f32 = 22.0;

actions!(modal, [Cancel]);

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Cancel, Some(CONTEXT))]);
}

/// 弹窗动画状态。封装「打开 / 关闭中 / 动画代次」与延迟卸载任务，
/// 供调用方（如 HostView、KeychainView）以 `Entity<ModalState>` 形式持有，
/// 复用 macOS 风格的「关闭时保持挂载直到退出动画结束」逻辑。
///
/// 作为独立实体，`close` 内部的延迟清理任务通过自身弱引用回到本状态，
/// 调用方无需提供「从宿主视图导航到本字段」的函数指针，避免误指向
/// 其他字段导致退出动画状态卡死。宿主视图需
/// `cx.observe(&modal_state, |_, _, cx| cx.notify())` 订阅刷新。
#[derive(Default)]
pub struct ModalState {
    open: bool,
    closing: bool,
    epoch: u32,
    /// 退出动画结束后清除 `closing` 的延迟任务。
    _hide_task: Option<Task<()>>,
}

impl ModalState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建独立的弹窗状态实体，并在宿主视图上订阅其变化以便重新渲染。
    /// 宿主视图构造时调用一次即可。
    pub fn entity<T: 'static>(cx: &mut Context<T>) -> Entity<Self> {
        let entity = cx.new(|_| Self::new());
        cx.observe(&entity, |_, _, cx| cx.notify()).detach();
        entity
    }

    /// 弹窗是否需要渲染（打开中或正在播放退出动画）。
    pub fn is_rendered(&self) -> bool {
        self.open || self.closing
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// 动画代次，作为动画元素 id 的一部分，确保动画从 delta=0 重新播放。
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// 打开弹窗，触发进入动画。
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.closing = false;
        self._hide_task = None;
        self.open = true;
        self.epoch = self.epoch.wrapping_add(1);
        cx.notify();
    }

    /// 开始关闭：进入退出动画阶段并保持渲染，待动画结束后卸载。
    /// 延迟任务通过自身弱引用回到本实体，无需调用方提供字段导航。
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.closing = true;
        self.epoch = self.epoch.wrapping_add(1);
        self._hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(MODAL_EXIT_MS + 40))
                .await;
            let _ = this.update(cx, |state, cx| {
                state.closing = false;
                state._hide_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
}

/// 进入时不透明度的「提前完成」比例：内容在动画走到 65% 时就已完全不透明，
/// 余下时间只做位置滑动，避免「淡入与位移同步收尾」的拖沓感。macOS 的弹窗
/// 也是内容很快变清晰、外形再轻微稳定到位。
const MODAL_OPACITY_LEAD: f32 = 0.65;

/// 进入动画下，给定线性时间进度 `t`，返回 (位置进度, 不透明度) ∈ [0,1]。
/// 位置走 macOS 进入贝塞尔（全程平滑减速到位），不透明度提前收尾。
fn modal_enter_progress(t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let pos = easing::macos_enter()(t);
    let opacity = (t / MODAL_OPACITY_LEAD).clamp(0.0, 1.0);
    // 不透明度本身也做一次平滑，避免线性淡入的生硬感。
    let opacity = easing::standard()(opacity);
    (pos, opacity)
}

/// 退出动画下，返回 (位置进度, 不透明度)。位置走 ease-in 加速收拢，
/// 不透明度更快淡出（在 75% 处归零），让面板「先消失、形随其后」。
fn modal_exit_progress(t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let pos = easing::macos_exit()(t);
    let opacity = 1.0 - (t / 0.75).clamp(0.0, 1.0);
    (pos, opacity)
}

/// A modal dialog component that renders a centered panel over a backdrop.
///
/// The modal provides a customizable container with a backdrop overlay.
/// It contains no default elements — all content is provided by the caller.
///
/// # Example
///
/// ```text
/// // In your Render implementation:
/// .when(self.show_settings, |this| {
///     this.child(
///         Modal::new("settings-modal")
///             .width(px(480.))
///             .max_height(px(600.))
///             .on_close(cx.listener(|this, _window, cx| {
///                 this.show_settings = false;
///                 cx.notify();
///             }))
///             .child(my_custom_content),
///     )
/// })
/// ```
#[derive(IntoElement)]
pub struct Modal {
    id: ElementId,
    style: StyleRefinement,
    width: Option<Pixels>,
    height: Option<Pixels>,
    max_width: Option<Pixels>,
    max_height: Option<Pixels>,
    min_width: Option<Pixels>,
    min_height: Option<Pixels>,
    padding: Option<Pixels>,
    rounded: Option<Pixels>,
    top_padding: Option<Pixels>,
    children: Vec<AnyElement>,
    close_on_backdrop: bool,
    close_on_escape: bool,
    on_close: OptionalWindowCallback,
    /// 是否正在播放退出动画。
    closing: bool,
    /// 动画代次，确保每次开/关从 delta=0 重新播放。
    epoch: u32,
}

impl Modal {
    /// Create a new modal with the given element id.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            width: None,
            height: None,
            max_width: None,
            max_height: None,
            min_width: None,
            min_height: None,
            padding: Some(px(24.0)),
            rounded: Some(px(16.0)),
            top_padding: None,
            children: Vec::new(),
            close_on_backdrop: true,
            close_on_escape: true,
            on_close: None,
            closing: false,
            epoch: 0,
        }
    }

    /// Set the width of the modal panel.
    pub fn width(mut self, width: impl Into<Pixels>) -> Self {
        self.width = Some(width.into());
        self
    }

    /// Set the height of the modal panel.
    pub fn height(mut self, height: impl Into<Pixels>) -> Self {
        self.height = Some(height.into());
        self
    }

    /// Set the max width of the modal panel.
    pub fn max_width(mut self, max_width: impl Into<Pixels>) -> Self {
        self.max_width = Some(max_width.into());
        self
    }

    /// Set the max height of the modal panel.
    pub fn max_height(mut self, max_height: impl Into<Pixels>) -> Self {
        self.max_height = Some(max_height.into());
        self
    }

    /// Set the min width of the modal panel.
    pub fn min_width(mut self, min_width: impl Into<Pixels>) -> Self {
        self.min_width = Some(min_width.into());
        self
    }

    /// Set the min height of the modal panel.
    pub fn min_height(mut self, min_height: impl Into<Pixels>) -> Self {
        self.min_height = Some(min_height.into());
        self
    }

    /// Set the padding of the modal panel. Default is `px(24.0)`.
    pub fn padding(mut self, padding: impl Into<Pixels>) -> Self {
        self.padding = Some(padding.into());
        self
    }

    /// Set the border radius of the modal panel. Default is `px(16.0)`.
    pub fn rounded(mut self, rounded: impl Into<Pixels>) -> Self {
        self.rounded = Some(rounded.into());
        self
    }

    /// Set the top padding of the modal panel to position it lower from the top.
    /// When set, the modal will be aligned to the top with this padding instead of centered.
    pub fn top_padding(mut self, top: impl Into<Pixels>) -> Self {
        self.top_padding = Some(top.into());
        self
    }

    /// Set whether clicking the backdrop closes the modal. Default is `true`.
    pub fn close_on_backdrop(mut self, close: bool) -> Self {
        self.close_on_backdrop = close;
        self
    }

    /// Set whether pressing Escape closes the modal. Default is `true`.
    pub fn close_on_escape(mut self, close: bool) -> Self {
        self.close_on_escape = close;
        self
    }

    /// Set a callback that is invoked when the modal is closed
    /// (either by backdrop click, Escape, or programmatically).
    pub fn on_close(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(handler));
        self
    }

    /// 标记弹窗正在播放退出动画。由调用方根据 [`ModalState::is_closing`] 传入。
    pub fn closing(mut self, closing: bool) -> Self {
        self.closing = closing;
        self
    }

    /// 设置动画代次，确保每次开/关从 delta=0 重新播放。
    /// 由调用方根据 [`ModalState::epoch`] 传入。
    pub fn epoch(mut self, epoch: u32) -> Self {
        self.epoch = epoch;
        self
    }
}

impl ParentElement for Modal {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for Modal {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Modal {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().ui;
        let on_close = self.on_close.clone();
        let close_on_backdrop = self.close_on_backdrop;
        let close_on_escape = self.close_on_escape;

        // 动画方向与时长：退出阶段反向播放且更快收拢。
        let is_exit = self.closing;
        let duration_ms = if is_exit {
            MODAL_EXIT_MS
        } else {
            MODAL_ENTER_MS
        };
        let epoch = self.epoch;
        let backdrop_anim_id: ElementId = ("modal-backdrop-anim", epoch).into();
        let panel_anim_id: ElementId = ("modal-panel-anim", epoch).into();

        let has_max_h = self.max_height.is_some();
        let panel = div()
            .id("modal-panel")
            .flex()
            .flex_col()
            .items_center()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .text_color(theme.text)
            .when_some(self.rounded, |this, r| this.rounded(r))
            .when_some(self.width, |this, w| this.w(w))
            .when_some(self.height, |this, h| this.h(h))
            .when_some(self.max_width, |this, w| this.max_w(w))
            .when_some(self.max_height, |this, h| this.max_h(h))
            .when_some(self.min_width, |this, w| this.min_w(w))
            .when_some(self.min_height, |this, h| this.min_h(h))
            .when_some(self.padding, |this, p| this.p(p))
            .when(has_max_h, |this| this.overflow_y_scroll())
            .shadow(vec![BoxShadow {
                color: theme.shadow.opacity(if cx.theme().appearance.is_dark() {
                    0.48
                } else {
                    0.24
                }),
                offset: point(px(0.), px(8.)),
                blur_radius: px(32.),
                spread_radius: px(-8.),
                inset: false,
            }])
            .occlude()
            .children(self.children)
            // 面板：从上方 MODAL_SLIDE_PX 处淡入滑下（进入），或向上收拢淡出（退出）。
            // 用线性时间驱动动画，曲线在 animator 内部应用，从而把「位置」与
            // 「不透明度」解耦：不透明度提前收尾、位置全程平滑减速到位。
            .with_animation(
                panel_anim_id,
                Animation::new(Duration::from_millis(duration_ms)).with_easing(linear),
                move |this, t| {
                    let (pos, opacity) = if is_exit {
                        modal_exit_progress(t)
                    } else {
                        modal_enter_progress(t)
                    };
                    // pos: 进入时 0→1 表示「从上方滑到原位」；退出时 0→1 表示「从原位滑回上方」。
                    let offset_y = if is_exit {
                        -MODAL_SLIDE_PX * pos
                    } else {
                        -MODAL_SLIDE_PX * (1.0 - pos)
                    };
                    this.top(px(offset_y)).opacity(opacity.clamp(0.0, 1.0))
                },
            );

        let backdrop_opacity = theme.overlay_background.opacity(0.5);
        let has_top_padding = self.top_padding.is_some();
        // 调用方通过 `impl Styled` 设置的 refinement 应用到最外层全屏容器，
        // 作为基准后再叠加组件的关键布局样式（absolute / inset_0 / flex 等）。
        let mut container = div().id(self.id);
        *container.style() = self.style;
        deferred(
            container
                .absolute()
                .inset_0()
                .size_full()
                .flex()
                .justify_center()
                .when(has_top_padding, |this| {
                    this.items_start()
                        .when_some(self.top_padding, |this, t| this.pt(t))
                })
                .when(!has_top_padding, |this| this.items_center())
                .child(
                    // Backdrop layer — 纯透明度淡入 / 淡出。
                    div()
                        .id("modal-backdrop")
                        .absolute()
                        .inset_0()
                        .size_full()
                        .bg(backdrop_opacity)
                        .when(close_on_backdrop, |this| {
                            this.on_click(move |_, window, cx| {
                                if let Some(on_close) = &on_close {
                                    (on_close)(window, cx);
                                }
                            })
                        })
                        .with_animation(
                            backdrop_anim_id,
                            Animation::new(Duration::from_millis(duration_ms)).with_easing(linear),
                            move |this, t| {
                                // 遮罩用标准平滑曲线，并比面板更早建立 / 更晚消散，
                                // 营造「先暗下来、内容随后浮现」的层次感。
                                let opacity = if is_exit {
                                    1.0 - easing::macos_exit()(t)
                                } else {
                                    easing::standard()(t)
                                };
                                this.opacity(opacity.clamp(0.0, 1.0))
                            },
                        ),
                )
                .child(
                    // Content layer - centered panel
                    div()
                        .id("modal-content")
                        .relative()
                        .flex()
                        .items_center()
                        .justify_center()
                        .max_w(window.window_bounds().get_bounds().size.width * 0.9)
                        .max_h(window.window_bounds().get_bounds().size.height * 0.9)
                        .child(panel),
                )
                .when(close_on_escape, |this| {
                    this.key_context(CONTEXT).on_action({
                        let on_close = self.on_close.clone();
                        move |_: &Cancel, window: &mut Window, cx: &mut App| {
                            if let Some(on_close) = &on_close {
                                (on_close)(window, cx);
                            }
                        }
                    })
                }),
        )
        .with_priority(100)
    }
}
