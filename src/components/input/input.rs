use gpui::prelude::*;
use gpui::{
    App, CursorStyle, Entity, Focusable, IntoElement, MouseButton, RenderOnce, Window, div, px,
    relative,
};

use crate::components::button::{Button, ButtonCustomVariant, ButtonVariants, Icon, Sizable, Size};
use crate::sidebar_view::font_settings::FontState;
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
}

impl RenderOnce for EditorInput {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // 仅在值真正变化时写入并通知：render 每帧都会走到这里，
        // 无条件写入会让 InputState 每帧都被判定为已更新。
        self.state.update(cx, |state, cx| {
            if state.disabled != self.disabled {
                state.disabled = self.disabled;
                cx.notify();
            }
            if state.size != self.size {
                state.size = self.size;
                cx.notify();
            }
        });

        let state = self.state.read(cx);
        let is_multi_line = state.mode.is_multi_line();
        let theme = cx.theme().ui;
        const LINE_HEIGHT: gpui::DefiniteLength = relative(1.618);

        // 代码编辑器用独立的「编辑器字体」（等宽字体 + 独立字号），
        // 与 UI 字体解耦：缩进参考线、列对齐都以等宽字体为前提。
        // 单行输入框（搜索框、设置项等）继续用 UI 字体。
        let editor_font = if state.mode.is_code_editor() {
            cx.try_global::<FontState>().map(|font_state| {
                (
                    font_state.settings.editor_font_family.clone(),
                    font_state.settings.editor_font_size,
                )
            })
        } else {
            None
        };

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
            // 整个输入面默认是文本光标（VSCode `.view-lines{cursor:text}`，
            // 迁移前本仓也在根 div 上设 IBeam）：内边距、末行以下的空白都算
            // 编辑器的一部分。行号栏与滚动条条带由 `TextElement` 用更靠后的
            // hitbox 覆盖成箭头，子元素的光标请求优先于本容器。
            .cursor(CursorStyle::IBeam)
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
                            .on_action(window.listener_for(&self.state, InputState::indent_inline))
                            .on_action(window.listener_for(&self.state, InputState::outdent_inline))
                            .on_action(window.listener_for(&self.state, InputState::indent))
                            .on_action(window.listener_for(&self.state, InputState::outdent))
                            .on_action(window.listener_for(&self.state, InputState::toggle_comment))
                            .on_action(window.listener_for(&self.state, InputState::toggle_soft_wrap))
                            .on_action(window.listener_for(&self.state, InputState::move_line_up))
                            .on_action(window.listener_for(&self.state, InputState::move_line_down))
                            .on_action(window.listener_for(&self.state, InputState::duplicate_line))
                            .on_action(window.listener_for(&self.state, InputState::delete_line))
                            .on_action(window.listener_for(&self.state, InputState::join_lines))
                            .on_action(window.listener_for(&self.state, InputState::toggle_fold))
                            .on_action(
                                window.listener_for(&self.state, InputState::toggle_fold_all),
                            )
                            .on_action(window.listener_for(&self.state, InputState::fold))
                            .on_action(window.listener_for(&self.state, InputState::unfold))
                            .on_action(window.listener_for(&self.state, InputState::fold_all))
                            .on_action(window.listener_for(&self.state, InputState::unfold_all))
                            .on_action(
                                window.listener_for(&self.state, InputState::fold_selected_ranges),
                            )
                            .on_action(window.listener_for(&self.state, InputState::fold_level))
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
                        this.on_action(
                            window.listener_for(&self.state, InputState::on_action_search),
                        )
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
            .on_scroll_wheel(window.listener_for(&self.state, InputState::on_scroll_wheel))
            .when_some(editor_font, |this, (family, size)| {
                this.font_family(family).text_size(px(size))
            })
            .when(self.transparent, |this| this.flex_1());

        if is_multi_line {
            // Multi-line mode: match the original InputState::render layout.
            // TextElement stays a direct flex child (wrapping it in an extra
            // div broke layout → blank editor).
            actions
                .id("editor-input")
                .relative()
                .flex_1()
                .h_full()
                .flex_grow(1.0)
                .overflow_x_hidden()
                .child(TextElement::new(self.state.clone()).placeholder(state.placeholder.clone()))
                // 搜索面板：VSCode 式悬浮卡片，绝对定位右上角，不占文档流。
                .when_some(state.search_panel.clone(), |this, panel| {
                    this.child(
                        div()
                            .absolute()
                            .top(px(6.0))
                            .right(px(12.0))
                            .max_w(px(560.0))
                            .child(panel),
                    )
                })
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
