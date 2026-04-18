use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, Pixels,
    Render, ScrollHandle, SharedString, Window, anchored, canvas, deferred, div, px, svg,
};

use crate::assets::Icon;
use crate::components::button::Size;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::theme::ActiveTheme;

/// A searchable select component with a trigger and an anchored popup list.
pub struct Select {
    focus_handle: FocusHandle,
    is_open: bool,
    is_closing: bool,
    searchable: bool,
    query_input: Entity<InputState>,
    all_items: Vec<SharedString>,
    item_subtitles: Vec<SharedString>,
    selected_item: SharedString,
    highlighted_index: usize,
    scroll_handle: ScrollHandle,
    on_select: Option<Arc<dyn Fn(SharedString, usize, &mut App) + Send + Sync>>,
    trigger_bounds: Rc<Cell<gpui::Bounds<Pixels>>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl Select {
    pub fn new(
        window: &mut Window,
        cx: &mut App,
        items: Vec<SharedString>,
        selected: impl Into<SharedString>,
    ) -> Entity<Self> {
        let selected = selected.into();
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let query_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Search...")
                    .placeholder_font_size(px(12.0))
            });

            let mut view = Self {
                focus_handle,
                is_open: false,
                is_closing: false,
                searchable: true,
                query_input,
                all_items: items,
                item_subtitles: Vec::new(),
                selected_item: selected,
                highlighted_index: 0,
                scroll_handle: ScrollHandle::new(),
                on_select: None,
                trigger_bounds: Rc::new(Cell::new(gpui::Bounds::default())),
                _subscriptions: Vec::new(),
            };

            view.subscribe_to_input(cx);
            view
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
        cx.notify();
    }

    pub fn set_subtitles(&mut self, subtitles: Vec<SharedString>, cx: &mut Context<Self>) {
        self.item_subtitles = subtitles;
        cx.notify();
    }

    pub fn set_selected(&mut self, selected: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.selected_item = selected.into();
        cx.notify();
    }

    pub fn searchable(&mut self, searchable: bool) {
        self.searchable = searchable;
    }

    fn subscribe_to_input(&mut self, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(
            &self.query_input.clone(),
            |_this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        );
        self._subscriptions.push(subscription);
    }

    /// Returns indices of items matching the current search query.
    pub(crate) fn filtered_item_indices(&self, cx: &App) -> Vec<usize> {
        let query = self.query_input.read(cx).value().to_string().to_lowercase();
        if query.is_empty() {
            (0..self.all_items.len()).collect()
        } else {
            self.all_items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect()
        }
    }

    fn toggle(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_closing {
            return;
        }
        if self.is_open {
            self.close(cx);
        } else {
            self.is_open = true;
            self.query_input
                .update(cx, |input, cx| input.set_text("", cx));
            let indices = self.filtered_item_indices(cx);
            self.highlighted_index = indices
                .iter()
                .map(|&i| &self.all_items[i])
                .position(|item| item == &self.selected_item)
                .unwrap_or(0);
            cx.notify();
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.is_open = false;
        self.is_closing = true;
        cx.notify();
        let entity = cx.entity();
        cx.defer(move |cx| {
            _ = entity.update(cx, |this, _cx| {
                this.is_closing = false;
            });
        });
    }

    fn confirm(&mut self, item: SharedString, item_idx: usize, cx: &mut Context<Self>) {
        self.selected_item = item.clone();
        self.close(cx);
        if let Some(ref on_select) = self.on_select {
            on_select(item, item_idx, cx);
        }
    }

    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let indices = self.filtered_item_indices(cx);
        let count = indices.len();
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
        let indices = self.filtered_item_indices(cx);
        if let Some(&item_idx) = indices.get(self.highlighted_index) {
            let item = self.all_items[item_idx].clone();
            self.confirm(item, item_idx, cx);
        }
    }
}

impl Focusable for Select {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Select {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_open = self.is_open;
        let indices = self.filtered_item_indices(cx);
        let selected = self.selected_item.clone();
        let trigger_bounds = self.trigger_bounds.clone();
        let bounds = trigger_bounds.get();

        let theme_accent = theme.accent;
        let theme_text = theme.text;
        let theme_text_muted = theme.text_muted;

        let highlighted = self.highlighted_index;
        let focus_handle = self.focus_handle.clone();
        let scroll_handle = self.scroll_handle.clone();
        let query_input = self.query_input.clone();

        let item_elements: Vec<gpui::AnyElement> = indices
            .into_iter()
            .enumerate()
            .map(|(ix, item_idx)| {
                let item = &self.all_items[item_idx];
                let is_selected = *item == selected;
                let is_highlighted = ix == highlighted;
                let item_clone = item.clone();
                let subtitle = self.item_subtitles.get(item_idx).cloned();
                div()
                    .id(("select-item", ix))
                    .w_full()
                    .py(px(6.0))
                    .pl(px(6.0))
                    .pr(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .cursor_pointer()
                    .rounded(px(4.0))
                    .when(is_highlighted, |d| {
                        d.bg(theme_accent.opacity(
                            if cx.theme().appearance.is_dark() {
                                0.18
                            } else {
                                0.10
                            },
                        ))
                    })
                    .when(!is_highlighted && !is_selected, |d| {
                        d.hover(|s| s.bg(theme.tab_hover_background))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.confirm(item_clone.clone(), item_idx, cx);
                        }),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .w(px(2.0))
                                    .h(px(12.0))
                                    .rounded_full()
                                    .bg(theme_accent)
                                    .when(!is_selected, |d| d.opacity(0.0)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if is_selected {
                                        theme_text
                                    } else {
                                        theme_text_muted
                                    })
                                    .child(item.to_string()),
                            ),
                    )
                    .when(subtitle.as_ref().map_or(false, |s| !s.is_empty()), |d| {
                        d.child(
                            div()
                                .pl(px(8.0))
                                .text_xs()
                                .text_color(theme_text_muted)
                                .child(subtitle.unwrap().to_string()),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        div()
            .w_full()
            .relative()
            .child(
                div()
                    .w_full()
                    .h(px(32.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.app_background)
                    .cursor_pointer()
                    .track_focus(&focus_handle)
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
                            .w_full()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .truncate()
                            .text_sm()
                            .text_color(theme.text)
                            .child(selected.to_string()),
                    )
                    .child(
                        svg()
                            .size(px(12.0))
                            .path(Icon::ChevronDown.path())
                            .text_color(theme.icon),
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
            .when(is_open, move |this| {
                this.child(
                    deferred(
                        anchored().snap_to_window_with_margin(px(8.)).child(
                            div()
                                .occlude()
                                .w(bounds.size.width + px(2.))
                                .child(
                                    div()
                                        .occlude()
                                        .mt(px(4.0))
                                        .bg(theme.panel_background)
                                        .border_1()
                                        .border_color(theme.border)
                                        .rounded(px(6.0))
                                        .shadow_md()
                                        .p(px(4.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(4.0))
                                        .key_context("Select")
                                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                            match event.keystroke.key.as_str() {
                                                "down" => this.move_highlight(1, cx),
                                                "up" => this.move_highlight(-1, cx),
                                                "enter" | "return" => this.confirm_highlighted(cx),
                                                "escape" => this.close(cx),
                                                _ => {}
                                            }
                                        }))
                                        .track_focus(&focus_handle)
                                        .when(self.searchable, |d| {
                                            d.child(
                                                div()
                                                    .w_full()
                                                    .h(px(28.0))
                                                    .px(px(6.0))
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(6.0))
                                                    .rounded(px(4.0))
                                                    .border_1()
                                                    .border_color(theme.border)
                                                    .bg(theme.app_background)
                                                    .child(
                                                        svg()
                                                            .size(px(12.0))
                                                            .path(Icon::MagnifyingGlass.path())
                                                            .text_color(theme.icon),
                                                    )
                                                    .child(
                                                        EditorInput::new(&query_input)
                                                            .size(Size::Small)
                                                            .transparent(),
                                                    ),
                                            )
                                        })
                                        .child(
                                            div()
                                                .id("select-list")
                                                .flex_1()
                                                .min_h_0()
                                                .max_h(px(240.0))
                                                .overflow_y_scroll()
                                                .track_scroll(&scroll_handle)
                                                .flex()
                                                .flex_col()
                                                .gap(px(1.0))
                                                .children(item_elements),
                                        ),
                                )
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx))),
                        ),
                    )
                    .with_priority(1),
                )
            })
    }
}
