use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, App, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, Pixels, Render, ScrollHandle, SharedString, Task,
    Window, anchored, canvas, deferred, div, linear, point, px, svg,
};

use crate::assets::Icon;
use crate::components::animation::easing;
use crate::theme::ActiveTheme;

/// 下拉进入动画时长（毫秒）。
const SELECT_ENTER_MS: u64 = 200;
/// 下拉退出动画时长（毫秒）。
const SELECT_EXIT_MS: u64 = 130;
/// 进入 / 退出动画的垂直位移，像素。
const SELECT_SLIDE_PX: f32 = 10.0;
/// 在触发器正下方弹出时，触发器底边到弹窗边框的间距（像素）需要 > 6px 后才是视觉上真实的间距。
const POPUP_GAP_BELOW_TRIGGER: f32 = 6.0;

/// 进入动画：返回 (位置进度, 不透明度)。位置全程平滑减速，不透明度提前收尾。
fn select_enter_progress(t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let pos = easing::macos_enter()(t);
    let opacity = easing::standard()((t / 0.6).clamp(0.0, 1.0));
    (pos, opacity)
}

/// 退出动画：返回 (位置进度, 不透明度)。位置加速收拢，不透明度更快淡出。
fn select_exit_progress(t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let pos = easing::macos_exit()(t);
    let opacity = 1.0 - (t / 0.7).clamp(0.0, 1.0);
    (pos, opacity)
}

/// 下拉弹窗相对于触发器的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectPopupPosition {
    /// 将高亮项与触发器对齐（默认行为）。
    #[default]
    AlignSelected,
    /// 在触发器正下方弹出。
    BelowTrigger,
}

/// A macOS-style dropdown select component.
///
/// Visual states (matching the provided screenshots):
///
/// 1. Normal state (截图1):
///    - Displays the currently selected text label on the left
///    - Right side shows a circular button (20×20px) with a chevron up/down icon
///    - The icon is rendered inside a rounded-full container with tab_hover_background color
///
/// 2. Hover state (截图2):
///    - The entire trigger area gets a light gray rounded background (rounded-lg, 8px radius)
///    - Background color: theme.tab_hover_background
///    - Cursor changes to pointer
///
/// 3. Open/Expanded state (截图3):
///    - A dropdown menu appears below the trigger with a small margin (mt-1)
///    - Menu container: panel_background with border, rounded-xl (10px), shadow-md
///    - Each menu item is a rounded row (rounded-md, 6px) with height 28px
///    - Selected item: blue accent background (theme.accent) with white checkmark (✓)
///    - Unselected items: muted text color, hover shows tab_hover_background
///    - Keyboard navigation supported: ↑/↓ to move, Enter to confirm, Escape to close
///
/// Usage:
/// ```rust
/// let items = vec!["始终显示".into(), "启用时显示".into(), "从不显示".into()];
/// let select = MacosSelect::new(window, cx, items, "启用时显示");
/// select.update(cx, |select, cx| {
///     select.on_select(|label, index, _window, cx| {
///         println!("Selected: {}", label);
///     });
///     select.set_popup_position(SelectPopupPosition::BelowTrigger, cx);
/// });
/// ```
pub struct MacosSelect {
    focus_handle: FocusHandle,
    is_open: bool,
    is_closing: bool,
    all_items: Vec<SharedString>,
    selected_item: SharedString,
    highlighted_index: usize,
    scroll_handle: ScrollHandle,
    on_select: Option<Arc<dyn Fn(SharedString, usize, &mut Window, &mut App) + Send + Sync>>,
    trigger_bounds: Rc<Cell<gpui::Bounds<Pixels>>>,
    /// 动画代次，确保每次开/关从 delta=0 重新播放。
    anim_epoch: u32,
    /// 退出动画结束后清除 `is_closing` 的延迟任务。
    _close_task: Option<Task<()>>,
    /// 下拉弹窗的显示位置。
    popup_position: SelectPopupPosition,
}

impl MacosSelect {
    pub fn new(
        _window: &mut Window,
        cx: &mut App,
        items: Vec<SharedString>,
        selected: impl Into<SharedString>,
    ) -> Entity<Self> {
        let selected = selected.into();
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let highlighted_index = items.iter().position(|item| item == &selected).unwrap_or(0);

            Self {
                focus_handle,
                is_open: false,
                is_closing: false,
                all_items: items,
                selected_item: selected,
                highlighted_index,
                scroll_handle: ScrollHandle::new(),
                on_select: None,
                trigger_bounds: Rc::new(Cell::new(gpui::Bounds::default())),
                anim_epoch: 0,
                _close_task: None,
                popup_position: SelectPopupPosition::default(),
            }
        })
    }

    pub fn on_select<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, usize, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_select = Some(Arc::new(callback));
    }

    pub fn set_items(&mut self, items: Vec<SharedString>, cx: &mut Context<Self>) {
        self.all_items = items;
        self.highlighted_index = self
            .all_items
            .iter()
            .position(|item| item == &self.selected_item)
            .unwrap_or(0);
        cx.notify();
    }

    pub fn set_selected(&mut self, selected: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.selected_item = selected.into();
        self.highlighted_index = self
            .all_items
            .iter()
            .position(|item| item == &self.selected_item)
            .unwrap_or(0);
        cx.notify();
    }

    pub fn set_popup_position(&mut self, position: SelectPopupPosition, cx: &mut Context<Self>) {
        self.popup_position = position;
        cx.notify();
    }

    fn toggle(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_closing {
            return;
        }
        if self.is_open {
            self.close(cx);
        } else {
            self.is_open = true;
            self.is_closing = false;
            self._close_task = None;
            self.anim_epoch = self.anim_epoch.wrapping_add(1);
            self.highlighted_index = self
                .all_items
                .iter()
                .position(|item| item == &self.selected_item)
                .unwrap_or(0);
            cx.notify();
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.is_open {
            return;
        }
        self.is_open = false;
        self.is_closing = true;
        self.anim_epoch = self.anim_epoch.wrapping_add(1);
        cx.notify();
        // 退出动画结束后卸载下拉。略多留几毫秒以容忍帧调度抖动。
        self._close_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SELECT_EXIT_MS + 40))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.is_closing = false;
                this._close_task = None;
                cx.notify();
            });
        }));
    }

    fn confirm(
        &mut self,
        item: SharedString,
        item_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_item = item.clone();
        self.close(cx);
        if let Some(ref on_select) = self.on_select {
            on_select(item, item_idx, window, cx);
        }
    }

    fn confirm_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.all_items.get(self.highlighted_index).cloned() {
            self.confirm(item, self.highlighted_index, window, cx);
        }
    }

    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.all_items.len();
        if count == 0 {
            self.highlighted_index = 0;
            return;
        }
        let new_index = if delta >= 0 {
            (self.highlighted_index + delta as usize) % count
        } else {
            let delta_abs = (-delta) as usize;
            if self.highlighted_index >= delta_abs {
                self.highlighted_index - delta_abs
            } else {
                count - (delta_abs - self.highlighted_index) % count
            }
        };
        self.highlighted_index = new_index;
        self.scroll_handle.scroll_to_item(self.highlighted_index);
        cx.notify();
    }
}

impl Focusable for MacosSelect {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MacosSelect {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_rendered = self.is_open || self.is_closing;
        let is_exit = self.is_closing;
        let anim_epoch = self.anim_epoch;
        let selected = self.selected_item.clone();
        let trigger_bounds = self.trigger_bounds.clone();
        let _bounds = trigger_bounds.get();

        let theme_accent = theme.accent;
        let theme_text = theme.text;
        let theme_text_muted = theme.text_muted;
        let is_dark = cx.theme().appearance.is_dark();

        // 和 popup_menu 一致的颜色
        let menu_bg = if is_dark {
            gpui::Hsla::from(gpui::rgb(0x2C2C2E))
        } else {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF))
        };
        let menu_border =
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(if is_dark { 0.1 } else { 0.6 });

        let highlighted = self.highlighted_index;
        let popup_position = self.popup_position;
        let focus_handle = self.focus_handle.clone();
        let scroll_handle = self.scroll_handle.clone();

        let trigger_bounds_value = trigger_bounds.get();
        let dropdown_offset_y = match popup_position {
            SelectPopupPosition::BelowTrigger => px(POPUP_GAP_BELOW_TRIGGER),
            SelectPopupPosition::AlignSelected => -(px(30.0) + px(highlighted as f32 * 26.0)),
        };
        let dropdown_width = match popup_position {
            SelectPopupPosition::AlignSelected => None,
            SelectPopupPosition::BelowTrigger => Some(trigger_bounds_value.size.width),
        };

        let item_elements: Vec<gpui::AnyElement> = self
            .all_items
            .iter()
            .enumerate()
            .map(|(item_idx, item)| {
                let is_selected = *item == selected;
                let is_highlighted = item_idx == highlighted;
                let item_clone = item.clone();
                div()
                    .id(("macos-select-item", item_idx))
                    .h(px(24.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .rounded(px(6.0))
                    .when(is_highlighted, |d| {
                        d.bg(theme_accent).text_color(theme.accent_foreground)
                    })
                    .when(!is_highlighted, |d| {
                        d.when(is_selected, |d| {
                            d.bg(theme_accent.opacity(if cx.theme().appearance.is_dark() {
                                0.85
                            } else {
                                0.12
                            }))
                        })
                        .when(!is_selected, |d| {
                            d.hover(|s| s.bg(theme.tab_hover_background))
                        })
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.confirm(item_clone.clone(), item_idx, window, cx);
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .w(px(16.0))
                                    .h(px(16.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(if is_highlighted {
                                                theme.accent_foreground
                                            } else if is_selected {
                                                theme_accent
                                            } else {
                                                gpui::transparent_black()
                                            })
                                            .child("✓"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if is_highlighted {
                                        theme.accent_foreground
                                    } else if is_selected {
                                        theme_text
                                    } else {
                                        theme_text_muted
                                    })
                                    .child(item.to_string()),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .relative()
            .flex_shrink(1.0)
            .min_w_0()
            .max_w_full()
            .items_start()
            .track_focus(&focus_handle)
            .child(
                div()
                    .id("macos-select-trigger")
                    .relative()
                    .h(px(24.0))
                    .max_w_full()
                    .px(px(6.0))
                    .flex()
                    .min_w_0()
                    .items_center()
                    .justify_end()
                    .gap(px(12.0))
                    .rounded(px(8.0))
                    .hover(|s| s.bg(theme.tab_hover_background))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.toggle(window, cx)),
                    )
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if !this.is_open {
                            match event.keystroke.key.as_str() {
                                "enter" | "return" | "space" => this.toggle(window, cx),
                                _ => {}
                            }
                        }
                    }))
                    .child(
                        div()
                            .id("title")
                            .ml_2()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .truncate()
                            .text_sm()
                            .text_color(theme.text)
                            .child(selected.to_string()),
                    )
                    .child(
                        div()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(theme.tab_hover_background)
                            .child(
                                svg()
                                    .size(px(6.0))
                                    .path(Icon::ChevronUpDown.path())
                                    .text_color(theme.text),
                            ),
                    )
                    .child({
                        let trigger_bounds = trigger_bounds.clone();
                        canvas(
                            move |bounds, _, _| {
                                trigger_bounds.set(bounds);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    }),
            )
            .when(is_rendered, move |this| {
                let duration_ms = if is_exit {
                    SELECT_EXIT_MS
                } else {
                    SELECT_ENTER_MS
                };
                let anim_id: gpui::ElementId = ("macos-select-anim", anim_epoch).into();
                this.child(
                    deferred(
                        anchored()
                            .anchor(gpui::Anchor::TopLeft)
                            .offset(point(px(0.0), dropdown_offset_y))
                            .snap_to_window_with_margin(px(8.))
                            .child(
                                div()
                                    .id("macos-select-dropdown")
                                    .occlude()
                                    .bg(menu_bg)
                                    .border(px(0.5))
                                    .border_color(menu_border)
                                    .rounded(px(10.0))
                                    .shadow_lg()
                                    .when(dropdown_width.is_some(), |d| {
                                        d.w(dropdown_width.unwrap())
                                            .px(px(6.0))
                                            .pb(px(6.0))
                                            .pt(px(1.0))
                                    })
                                    .when(dropdown_width.is_none(), |d| d.p(px(6.0)))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .key_context("MacosSelect")
                                    .on_key_down(cx.listener(
                                        |this, event: &KeyDownEvent, window, cx| {
                                            match event.keystroke.key.as_str() {
                                                "down" => this.move_highlight(1, cx),
                                                "up" => this.move_highlight(-1, cx),
                                                "enter" | "return" => {
                                                    this.confirm_highlighted(window, cx)
                                                }
                                                "escape" => this.close(cx),
                                                _ => {}
                                            }
                                        },
                                    ))
                                    .track_focus(&focus_handle)
                                    .child(
                                        div()
                                            .id("macos-select-list")
                                            .flex_1()
                                            .min_h_0()
                                            .max_h(
                                                window.window_bounds().get_bounds().size.height
                                                    - px(32.0),
                                            )
                                            .overflow_y_scroll()
                                            .track_scroll(&scroll_handle)
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.0))
                                            .children(item_elements),
                                    )
                                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
                                    .with_animation(
                                        anim_id,
                                        Animation::new(Duration::from_millis(duration_ms))
                                            .with_easing(linear),
                                        move |this, t| {
                                            let (pos, opacity) = if is_exit {
                                                select_exit_progress(t)
                                            } else {
                                                select_enter_progress(t)
                                            };
                                            let offset_y = if is_exit {
                                                -SELECT_SLIDE_PX * pos
                                            } else {
                                                -SELECT_SLIDE_PX * (1.0 - pos)
                                            };
                                            this.top(px(offset_y)).opacity(opacity.clamp(0.0, 1.0))
                                        },
                                    ),
                            ),
                    )
                    .with_priority(101),
                )
            })
    }
}
