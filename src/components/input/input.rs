use gpui::prelude::*;
use gpui::{
    App, Entity, Focusable, IntoElement, MouseButton, RenderOnce, SharedString, Window, div, px,
    relative,
};

use crate::components::button::{Button, ButtonCustomVariant, ButtonVariants, Icon, Sizable, Size};
use crate::theme::ActiveTheme;

use super::{InputState, element::TextElement};

/// A text input UI wrapper bound to an [`InputState`].
///
/// This component handles action bindings, mouse/scroll events, and chrome styling
/// (borders, backgrounds), while delegating actual text rendering to [`TextElement`].
#[derive(IntoElement)]
pub struct EditorInput {
    state: Entity<InputState>,
    disabled: bool,
    transparent: bool,
    size: Size,
    mask_toggle: bool,
}

impl EditorInput {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self {
            state: state.clone(),
            disabled: false,
            transparent: false,
            size: Size::default(),
            mask_toggle: false,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn transparent(mut self) -> Self {
        self.transparent = true;
        self
    }

    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn small(self) -> Self {
        self.size(Size::Small)
    }

    pub fn mask_toggle(mut self) -> Self {
        self.mask_toggle = true;
        self
    }

    fn render_toggle_mask_button(state: &Entity<InputState>, cx: &App) -> impl IntoElement {
        let masked = state.read(cx).masked;
        let icon_path: &'static str = if masked {
            "icons/eye_off.svg"
        } else {
            "icons/eye.svg"
        };
        let style = ButtonCustomVariant::new(cx)
            .color(gpui::transparent_black())
            .hover(gpui::transparent_black())
            .active(gpui::transparent_black());
        Button::new("toggle-mask")
            .icon(Icon::new(icon_path).xsmall())
            .custom(style)
            .tab_stop(false)
            .on_click({
                let state = state.clone();
                move |_, window, cx| {
                    state.update(cx, |state, cx| {
                        state.set_masked(!state.masked, window, cx);
                    });
                    let focus_handle = state.read(cx).focus_handle(cx);
                    window.focus(&focus_handle, cx);
                }
            })
    }

    /// Floating completion popup (anchored at the bottom of the editor).
    /// Returns `None` when there is nothing to show so the element tree stays
    /// stable when the popup closes.
    fn completion_menu_element(state: &InputState, cx: &App) -> Option<gpui::AnyElement> {
        let menu = state.completion_menu.as_ref()?;
        if !menu.visible || menu.items.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let el = div()
            .id("completion-menu")
            .absolute()
            .bottom_0()
            .left_0()
            .w_full()
            .max_h(px(240.0))
            .overflow_y_scroll()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .children(
                menu.items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let selected = i == menu.selected;
                        div()
                            .id(SharedString::from(format!("completion-item-{i}")))
                            .px_2()
                            .py_1()
                            .text_color(if selected {
                                theme.accent_foreground
                            } else {
                                theme.text
                            })
                            .bg(if selected {
                                theme.accent
                            } else {
                                gpui::transparent_black()
                            })
                            .child(item.label.clone())
                    })
                    .collect::<Vec<_>>(),
            );
        Some(el.into_any_element())
    }

    /// Floating hover popover (anchored at the top of the editor).
    fn hover_popover_element(state: &InputState, cx: &App) -> Option<gpui::AnyElement> {
        let popover = state.hover_popover.as_ref()?;
        if popover.contents.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let el = div()
            .id("hover-popover")
            .absolute()
            .top_0()
            .left_0()
            .max_w(px(480.0))
            .max_h(px(200.0))
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .text_color(theme.text)
            .child(popover.contents.clone());
        Some(el.into_any_element())
    }
}

impl RenderOnce for EditorInput {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.state.update(cx, |state, _cx| {
            if state.disabled != self.disabled {
                state.disabled = self.disabled;
            }
            state.size = self.size;
        });

        let state = self.state.read(cx);
        let is_multi_line = state.mode.is_multi_line();
        let theme = cx.theme().ui;
        const LINE_HEIGHT: gpui::DefiniteLength = relative(1.618);

        let input_py = match self.size {
            Size::Large => px(10.0),
            Size::Medium => px(8.0),
            Size::Small => px(2.0),
            Size::XSmall => px(0.0),
            Size::Size(_) => px(8.0),
        };

        let actions = div()
            .key_context(super::CONTEXT)
            .track_focus(&state.focus_handle)
            .line_height(LINE_HEIGHT)
            .when(!state.disabled, |this| {
                this.on_action(window.listener_for(&self.state, InputState::backspace))
                    .on_action(window.listener_for(&self.state, InputState::delete))
                    .on_action(
                        window.listener_for(&self.state, InputState::delete_to_beginning_of_line),
                    )
                    .on_action(window.listener_for(&self.state, InputState::delete_to_end_of_line))
                    .on_action(window.listener_for(&self.state, InputState::delete_previous_word))
                    .on_action(window.listener_for(&self.state, InputState::delete_next_word))
                    .on_action(window.listener_for(&self.state, InputState::enter))
                    .on_action(window.listener_for(&self.state, InputState::escape))
                    .on_action(window.listener_for(&self.state, InputState::paste))
                    .on_action(window.listener_for(&self.state, InputState::cut))
                    .on_action(window.listener_for(&self.state, InputState::undo))
                    .on_action(window.listener_for(&self.state, InputState::redo))
                    .on_action(window.listener_for(&self.state, InputState::left))
                    .on_action(window.listener_for(&self.state, InputState::right))
                    .on_action(window.listener_for(&self.state, InputState::select_left))
                    .on_action(window.listener_for(&self.state, InputState::select_right))
                    .when(is_multi_line, |this| {
                        this.on_action(window.listener_for(&self.state, InputState::up))
                            .on_action(window.listener_for(&self.state, InputState::down))
                            .on_action(window.listener_for(&self.state, InputState::select_up))
                            .on_action(window.listener_for(&self.state, InputState::select_down))
                            .on_action(window.listener_for(&self.state, InputState::page_up))
                            .on_action(window.listener_for(&self.state, InputState::page_down))
                    })
                    .on_action(window.listener_for(&self.state, InputState::select_all))
                    .on_action(
                        window.listener_for(&self.state, InputState::select_to_start_of_line),
                    )
                    .on_action(window.listener_for(&self.state, InputState::select_to_end_of_line))
                    .on_action(
                        window.listener_for(&self.state, InputState::select_to_previous_word),
                    )
                    .on_action(window.listener_for(&self.state, InputState::select_to_next_word))
                    .on_action(window.listener_for(&self.state, InputState::home))
                    .on_action(window.listener_for(&self.state, InputState::end))
                    .on_action(window.listener_for(&self.state, InputState::move_to_start))
                    .on_action(window.listener_for(&self.state, InputState::move_to_end))
                    .on_action(window.listener_for(&self.state, InputState::move_to_previous_word))
                    .on_action(window.listener_for(&self.state, InputState::move_to_next_word))
                    .on_action(window.listener_for(&self.state, InputState::select_to_start))
                    .on_action(window.listener_for(&self.state, InputState::select_to_end))
                    .on_action(window.listener_for(&self.state, InputState::show_character_palette))
                    .on_action(window.listener_for(&self.state, InputState::copy))
                    .when(is_multi_line, |this| {
                        this.on_action(window.listener_for(&self.state, InputState::on_action_search))
                            .on_action(window.listener_for(
                                &self.state,
                                InputState::on_action_go_to_definition,
                            ))
                    })
                    .on_key_down(window.listener_for(&self.state, InputState::on_key_down))
            })
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&self.state, InputState::on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                window.listener_for(&self.state, InputState::on_mouse_down),
            )
            .on_mouse_up(
                MouseButton::Left,
                window.listener_for(&self.state, InputState::on_mouse_up),
            )
            .on_mouse_up(
                MouseButton::Right,
                window.listener_for(&self.state, InputState::on_mouse_up),
            )
            .on_mouse_move(window.listener_for(&self.state, InputState::on_mouse_move))
            .on_scroll_wheel(window.listener_for(&self.state, InputState::on_scroll_wheel))
            .when(self.transparent, |this| this.flex_1());

        if is_multi_line {
            // Multi-line mode: match the original InputState::render layout.
            // TextElement stays a direct flex child (wrapping it in an extra
            // div broke layout → blank editor); popups are absolutely
            // positioned overlays on the editor container.
            // Popups are conditionally-rendered overlays; when none is open
            // the children list stays identical to the plain editor (single
            // TextElement), which keeps the element tree stable.
            let completion_menu = Self::completion_menu_element(&state, cx);
            let hover_popover = Self::hover_popover_element(&state, cx);
            actions
                .id("editor-input")
                .flex_1()
                .h_full()
                .flex_grow(1.0)
                .overflow_x_hidden()
                .relative()
                .when_some(state.search_panel.clone(), |this, panel| this.child(panel))
                .child(
                    TextElement::new(self.state.clone())
                        .placeholder(state.placeholder.clone()),
                )
                .when_some(completion_menu, |this, el| this.child(el))
                .when_some(hover_popover, |this, el| this.child(el))
                .into_any_element()
        } else {
            let mask_toggle_el = if self.mask_toggle {
                Some(Self::render_toggle_mask_button(&self.state, cx).into_any_element())
            } else {
                None
            };
            let input_pr = if mask_toggle_el.is_some() {
                px(4.0)
            } else {
                px(10.0)
            };

            actions
                .w_full()
                .flex()
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.0))
                        .line_height(LINE_HEIGHT)
                        .when(!self.transparent, |this| {
                            this.h(px(28.0))
                                .pl(px(10.0))
                                .pr(input_pr)
                                .py(input_py)
                                .gap(px(8.0))
                                .rounded(px(12.0))
                                .bg(theme.tab_hover_background)
                                .when(state.focus_handle.is_focused(window), |this| {
                                    this.border(px(3.0)).border_color(theme.accent.opacity(0.5))
                                })
                        })
                        .when(self.transparent, |this| this.h_full())
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .child(
                            TextElement::new(self.state.clone())
                                .placeholder(state.placeholder.clone()),
                        )
                        .when_some(mask_toggle_el, |this, el| this.child(el)),
                )
                .into_any_element()
        }
    }
}
