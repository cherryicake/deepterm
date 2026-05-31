use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, App, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, Pixels, Render, ScrollHandle, SharedString, Task,
    Window, anchored, canvas, deferred, div, point, px, svg,
};

use crate::assets::Icon;
use crate::theme::ActiveTheme;

/// 下拉进入动画时长（毫秒）。
const SELECT_ENTER_MS: u64 = 160;
/// 下拉退出动画时长（毫秒）。
const SELECT_EXIT_MS: u64 = 110;
/// 进入 / 退出动画的垂直位移，像素。
const SELECT_SLIDE_PX: f32 = 6.0;

/// 进入缓动：ease-out-quint（快入慢停）。
fn select_enter_easing(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(5)
}

/// 退出缓动：ease-in-cubic（慢起快收）。
fn select_exit_easing(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
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
/// select.update(cx, |select, _cx| {
///     select.on_select(|label, index, cx| {
///         println!("Selected: {}", label);
///     });
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
    on_select: Option<Arc<dyn Fn(SharedString, usize, &mut App) + Send + Sync>>,
    trigger_bounds: Rc<Cell<gpui::Bounds<Pixels>>>,
    /// 动画代次，确保每次开/关从 delta=0 重新播放。
    anim_epoch: u32,
    /// 退出动画结束后清除 `is_closing` 的延迟任务。
    _close_task: Option<Task<()>>,
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
            }
        })
    }

    pub fn on_select<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, usize, &mut App) + Send + Sync + 'static,
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

    fn confirm(&mut self, item: SharedString, item_idx: usize, cx: &mut Context<Self>) {
        self.selected_item = item.clone();
        self.close(cx);
        if let Some(ref on_select) = self.on_select {
            on_select(item, item_idx, cx);
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

    fn confirm_highlighted(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.all_items.get(self.highlighted_index).cloned() {
            self.confirm(item, self.highlighted_index, cx);
        }
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

        let highlighted = self.highlighted_index;
        let focus_handle = self.focus_handle.clone();
        let scroll_handle = self.scroll_handle.clone();

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
                        cx.listener(move |this, _, _, cx| {
                            this.confirm(item_clone.clone(), item_idx, cx);
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
            .flex_shrink()
            .min_w_0()
            .max_w_full()
            .items_start()
            .track_focus(&focus_handle)
            .child(
                div()
                    .id("macos-select-trigger")
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
                                    .size(px(12.0))
                                    .path(Icon::ChevronUpDown.path())
                                    .text_color(theme.icon),
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
                let duration_ms = if is_exit { SELECT_EXIT_MS } else { SELECT_ENTER_MS };
                let anim_id: gpui::ElementId = ("macos-select-anim", anim_epoch).into();
                this.child(
                    deferred(
                        anchored()
                            .anchor(gpui::Corner::TopLeft)
                            .offset(point(
                                px(0.0),
                                -(px(30.0) + px(highlighted as f32 * 26.0)),
                            ))
                            .snap_to_window_with_margin(px(8.))
                            .child(
                                div()
                                    .id("macos-select-dropdown")
                                    .occlude()
                                    .child(
                                        div()
                                            .occlude()
                                            .bg(theme.panel_background)
                                            .border_1()
                                            .border_color(theme.border)
                                            .rounded(px(10.0))
                                            .shadow_md()
                                            .p(px(6.0))
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.0))
                                            .key_context("MacosSelect")
                                            .on_key_down(cx.listener(
                                                |this, event: &KeyDownEvent, _, cx| {
                                                    match event.keystroke.key.as_str() {
                                                        "down" => this.move_highlight(1, cx),
                                                        "up" => this.move_highlight(-1, cx),
                                                        "enter" | "return" => {
                                                            this.confirm_highlighted(cx)
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
                                                        window
                                                            .window_bounds()
                                                            .get_bounds()
                                                            .size
                                                            .height
                                                            - px(32.0),
                                                    )
                                                    .overflow_y_scroll()
                                                    .track_scroll(&scroll_handle)
                                                    .flex()
                                                    .flex_col()
                                                    .gap(px(2.0))
                                                    .children(item_elements),
                                            )
                                            .with_animation(
                                                anim_id,
                                                Animation::new(Duration::from_millis(duration_ms))
                                                    .with_easing(move |t| {
                                                        if is_exit {
                                                            select_exit_easing(t)
                                                        } else {
                                                            select_enter_easing(t)
                                                        }
                                                    }),
                                                move |this, delta| {
                                                    let (offset_y, opacity) = if is_exit {
                                                        (-SELECT_SLIDE_PX * delta, 1.0 - delta)
                                                    } else {
                                                        (
                                                            -SELECT_SLIDE_PX * (1.0 - delta),
                                                            delta,
                                                        )
                                                    };
                                                    this.top(px(offset_y))
                                                        .opacity(opacity.clamp(0.0, 1.0))
                                                },
                                            ),
                                    )
                                    .on_mouse_down_out(
                                        cx.listener(|this, _, _, cx| this.close(cx)),
                                    ),
                            ),
                    )
                    .with_priority(101),
                )
            })
    }
}
