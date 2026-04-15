use gpui::prelude::*;
use gpui::{App, Entity, IntoElement, MouseButton, RenderOnce, Window, div, px};

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
}

impl EditorInput {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self {
            state: state.clone(),
            disabled: false,
            transparent: false,
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
}

impl RenderOnce for EditorInput {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.state.update(cx, |state, _cx| {
            state.disabled = self.disabled;
        });

        let state = self.state.read(cx);
        let is_focused = state.focus_handle.is_focused(window) && !state.disabled;
        let is_multi_line = state.mode.is_multi_line();
        let theme = cx.theme().ui;

        let actions = div()
            .key_context(super::CONTEXT)
            .track_focus(&state.focus_handle)
            .when(!state.disabled, |this| {
                this.on_action(window.listener_for(&self.state, InputState::backspace))
                    .on_action(window.listener_for(&self.state, InputState::delete))
                    .on_action(window.listener_for(&self.state, InputState::delete_to_beginning_of_line))
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
                    .on_action(window.listener_for(&self.state, InputState::select_to_start_of_line))
                    .on_action(window.listener_for(&self.state, InputState::select_to_end_of_line))
                    .on_action(window.listener_for(&self.state, InputState::select_to_previous_word))
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
            .on_scroll_wheel(window.listener_for(&self.state, InputState::on_scroll_wheel));

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
            // Single-line mode: chrome wrapper with border and fixed height
            actions
                .w_full()
                .flex()
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.0))
                        .when(!self.transparent, |this| {
                            this.h(px(36.0))
                                .px(px(10.0))
                                .gap(px(8.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(if is_focused {
                                    theme.border_strong
                                } else {
                                    theme.border
                                })
                                .bg(theme.app_background)
                        })
                        .when(self.transparent, |this| this.h_full())
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .child(TextElement::new(self.state.clone()).placeholder(state.placeholder.clone()))
                )
                .into_any_element()
        }
    }
}
