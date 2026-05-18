use gpui::prelude::*;
use gpui::{App, Entity, Focusable, IntoElement, MouseButton, RenderOnce, Window, div, px};

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
    macos26: bool,
    size: Size,
    mask_toggle: bool,
}

impl EditorInput {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self {
            state: state.clone(),
            disabled: false,
            transparent: false,
            macos26: false,
            size: Size::default(),
            mask_toggle: false,
        }
    }

    pub fn macos26(mut self) -> Self {
        self.macos26 = true;
        self
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
        let is_focused = state.focus_handle.is_focused(window) && !state.disabled;
        let is_multi_line = state.mode.is_multi_line();
        let theme = cx.theme().ui;
        const LINE_HEIGHT: gpui::Rems = gpui::Rems(1.25);

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
            // Multi-line mode: match the original InputState::render layout
            actions
                .id("editor-input")
                .flex_1()
                .h_full()
                .flex_grow()
                .overflow_x_hidden()
                .child(TextElement::new(self.state.clone()).placeholder(state.placeholder.clone()))
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
                            if self.macos26 {
                                this.h(px(32.0))
                                    .pl(px(10.0))
                                    .pr(input_pr)
                                    .py(input_py)
                                    .gap(px(8.0))
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(if is_focused {
                                        theme.accent
                                    } else {
                                        theme.border
                                    })
                                    .bg(theme.elevated_background)
                            } else {
                                this.h(px(36.0))
                                    .pl(px(10.0))
                                    .pr(input_pr)
                                    .py(input_py)
                                    .gap(px(8.0))
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(if is_focused {
                                        theme.border_strong
                                    } else {
                                        theme.border
                                    })
                                    .bg(theme.app_background)
                            }
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
