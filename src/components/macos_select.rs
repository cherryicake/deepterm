use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, App, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, Pixels, Render, ScrollHandle, SharedString, Task,
    Window, anchored, canvas, deferred, div, linear, point, px, svg,
};

use crate::assets::Icon;
use crate::components::animation::easing;
use crate::theme::ActiveTheme;

/// 选项选中回调，参数为选中的文本与其索引。
type SelectCallback = Rc<dyn Fn(SharedString, usize, &mut Window, &mut App)>;

/// 下拉进入动画时长（毫秒）。
const SELECT_ENTER_MS: u64 = 200;
/// 下拉退出动画时长（毫秒）。
const SELECT_EXIT_MS: u64 = 130;
/// 进入 / 退出动画的垂直位移，像素。
const SELECT_SLIDE_PX: f32 = 10.0;
/// 在触发器正下方弹出时，触发器底边到弹窗边框的间距（像素）需要 > 6px 后才是视觉上真实的间距。
const POPUP_GAP_BELOW_TRIGGER: f32 = 6.0;
/// 触发器高度（像素）。
const SELECT_TRIGGER_H: f32 = 24.0;
/// 下拉项高度（像素）。
const SELECT_ITEM_H: f32 = 24.0;
/// 下拉项之间的间距（像素）。
const SELECT_ITEM_GAP: f32 = 2.0;
/// 下拉弹窗内容区内边距（像素）。
const SELECT_POPUP_PAD: f32 = 6.0;

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
    on_select: Option<SelectCallback>,
    trigger_bounds: Rc<Cell<gpui::Bounds<Pixels>>>,
    /// 动画代次，确保每次开/关从 delta=0 重新播放。
    anim_epoch: u32,
    /// 退出动画结束后清除 `is_closing` 的延迟任务。
    _close_task: Option<Task<()>>,
    /// 下拉弹窗的显示位置。
    popup_position: SelectPopupPosition,
    /// 在该索引之后绘制分割线（用于将标签页组与一级菜单隔开）
    separator_after: Option<usize>,
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
                separator_after: None,
            }
        })
    }

    pub fn on_select<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, usize, &mut Window, &mut App) + 'static,
    {
        self.on_select = Some(Rc::new(callback));
    }

    pub fn set_items(&mut self, items: Vec<SharedString>, cx: &mut Context<Self>) {
        self.set_items_with_separator(items, None, cx);
    }

    /// 设置选项，并在 `separator_after` 指定的索引之后绘制一条分割线（分割线不占选项索引）。
    pub fn set_items_with_separator(
        &mut self,
        items: Vec<SharedString>,
        separator_after: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.all_items = items;
        self.separator_after = separator_after;
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

crate::impl_focusable!(MacosSelect);

impl Render for MacosSelect {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_rendered = self.is_open || self.is_closing;
        let is_exit = self.is_closing;
        let anim_epoch = self.anim_epoch;
        let selected = self.selected_item.clone();
        let trigger_bounds = self.trigger_bounds.clone();

        let theme_accent = theme.accent;
        let theme_text = theme.text;
        let theme_text_muted = theme.text_muted;

        // 与 PopupMenu 一致的浮层颜色（来自主题，可在主题文件中通过
        // menu_background / menu_border 覆盖）。
        let menu_bg = theme.menu_background;
        let menu_border = theme.menu_border;

        let highlighted = self.highlighted_index;
        let popup_position = self.popup_position;
        let focus_handle = self.focus_handle.clone();
        let scroll_handle = self.scroll_handle.clone();

        let trigger_bounds_value = trigger_bounds.get();
        let dropdown_offset_y = match popup_position {
            SelectPopupPosition::BelowTrigger => px(POPUP_GAP_BELOW_TRIGGER),
            SelectPopupPosition::AlignSelected => {
                // 将高亮项对齐到触发器行：基准偏移 = 触发器高度 + 触发器间距，
                // 再为高亮项之前的每一项上移一行（项高 + 行间距）。
                let item_stride = SELECT_ITEM_H + SELECT_ITEM_GAP;
                let base = SELECT_TRIGGER_H + POPUP_GAP_BELOW_TRIGGER;
                -(px(base) + px(highlighted as f32 * item_stride))
            }
        };
        let dropdown_width = match popup_position {
            SelectPopupPosition::AlignSelected => None,
            SelectPopupPosition::BelowTrigger => Some(trigger_bounds_value.size.width),
        };

        let separator_after = self.separator_after;
        let mut item_elements: Vec<gpui::AnyElement> = Vec::with_capacity(
            self.all_items.len() + if separator_after.is_some() { 1 } else { 0 },
        );
        for (item_idx, item) in self.all_items.iter().enumerate() {
            let is_selected = *item == selected;
            let is_highlighted = item_idx == highlighted;
            let item_clone = item.clone();
            let el = div()
                .id(("macos-select-item", item_idx))
                .h(px(SELECT_ITEM_H))
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
                        )
                        .child(div().w(px(16.0)).h(px(16.0))),
                )
                .into_any_element();
            item_elements.push(el);
            if separator_after == Some(item_idx) {
                item_elements.push(
                    div()
                        .h(px(9.))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .h(px(1.))
                                .w_full()
                                .mx(px(8.))
                                .bg(theme.menu_separator.opacity(0.6)),
                        )
                        .into_any_element(),
                );
            }
        }

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
                    .h(px(SELECT_TRIGGER_H))
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
                            // 标签名左侧留白稍大于触发器内边距（6+8=14px），
                            // 与右侧 chevron 图标中心（6+16/2=14px）对称，视觉更平衡。
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
                                    // 四周内边距统一：首个选项到弹窗顶部与末个选项
                                    // 到弹窗底部的间距保持一致。
                                    .p(px(SELECT_POPUP_PAD))
                                    .when(dropdown_width.is_some(), |d| {
                                        // 以触发器宽度为下限，弹窗可随最宽选项自然变宽，
                                        // 避免触发器收紧后长选项文字被裁剪。
                                        d.min_w(dropdown_width.unwrap())
                                    })
                                    .flex()
                                    .flex_col()
                                    .gap(px(SELECT_ITEM_GAP))
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
                                            .gap(px(SELECT_ITEM_GAP))
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
