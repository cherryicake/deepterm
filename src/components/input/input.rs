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

        // 上下文以 `KeyContext` 类型构造，而不是字符串：字符串会被 gpui 当
        // 上下文谓词解析（`KeyContext::parse`），`&&`、`==` 这类字符会被
        // 拆成条目，导致上下文错乱。`Input` 是主标识，vim 状态在其上追加
        // `VimControl` / `vim_mode` / `vim_operator` 供键位表匹配。
        let mut key_context = gpui::KeyContext::new_with_defaults();
        key_context.add(super::CONTEXT);
        if let Some(vim) = state.vim.as_ref() {
            vim.extend_key_context(&mut key_context);
        } else {
            // 不接受 vim 的输入框：显式标记，让 vim 键位整体退出。
            //
            // 只靠「不追加 vim 状态」不够：gpui 的谓词从最深一层往上找，
            // 嵌套在编辑器里的查找框会命中**祖先**的 `VimControl`。带上
            // 这个标记后，`vim.json` 里每段谓词的 `!NoVim` 在整条栈上求值，
            // 于是祖先再深也匹配不上。
            key_context.add(crate::vim::NO_VIM_CONTEXT);
        }
        // 补全菜单可见时把状态写进键位上下文。
        //
        // 内置补全键现在无条件绑在 `Input` 上、由 handler 判断可见性；
        // 这个上下文仍保留给用户键位表 / 将来的条件绑定使用，不能删。
        if state
            .completion_menu()
            .is_some_and(|menu| !menu.candidates.is_empty())
        {
            key_context.add("showing_completions");
        }

        let actions = div()
            .key_context(key_context)
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
                    // vim 键位。仅在 `vim_mode` 上下文匹配时才会派发
                    // （见 assets/keymaps/vim.json），所以 vim 关闭时这些
                    // 监听器不会被触发。
                    .on_action(window.listener_for(&self.state, InputState::vim_normal_mode))
                    .on_action(window.listener_for(&self.state, InputState::vim_insert_before))
                    .on_action(window.listener_for(&self.state, InputState::vim_insert_after))
                    .on_action(
                        window
                            .listener_for(&self.state, InputState::vim_insert_first_non_whitespace),
                    )
                    .on_action(window.listener_for(&self.state, InputState::vim_insert_end_of_line))
                    .on_action(window.listener_for(&self.state, InputState::vim_left))
                    .on_action(window.listener_for(&self.state, InputState::vim_right))
                    .on_action(window.listener_for(&self.state, InputState::vim_down))
                    .on_action(window.listener_for(&self.state, InputState::vim_up))
                    .on_action(window.listener_for(&self.state, InputState::vim_next_word_start))
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_previous_word_start),
                    )
                    .on_action(window.listener_for(&self.state, InputState::vim_next_word_end))
                    .on_action(window.listener_for(&self.state, InputState::vim_start_of_line))
                    .on_action(window.listener_for(&self.state, InputState::vim_end_of_line))
                    .on_action(window.listener_for(&self.state, InputState::vim_start_of_document))
                    .on_action(window.listener_for(&self.state, InputState::vim_end_of_document))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_delete))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_yank))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_change))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_object_inner))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_object_around))
                    .on_action(window.listener_for(&self.state, InputState::vim_object_word))
                    .on_action(window.listener_for(&self.state, InputState::vim_object_key_action))
                    .on_action(window.listener_for(&self.state, InputState::vim_delete_char))
                    .on_action(window.listener_for(&self.state, InputState::vim_paste_after))
                    .on_action(window.listener_for(&self.state, InputState::vim_paste_before))
                    .on_action(window.listener_for(&self.state, InputState::vim_join_lines))
                    .on_action(window.listener_for(&self.state, InputState::vim_indent))
                    .on_action(window.listener_for(&self.state, InputState::vim_outdent))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_1))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_2))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_3))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_4))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_5))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_6))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_7))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_8))
                    .on_action(window.listener_for(&self.state, InputState::vim_count_9))
                    // 撤销 / 重做与行内快捷命令
                    .on_action(window.listener_for(&self.state, InputState::vim_undo))
                    .on_action(window.listener_for(&self.state, InputState::vim_redo))
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_delete_to_end_of_line),
                    )
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_change_to_end_of_line),
                    )
                    .on_action(window.listener_for(&self.state, InputState::vim_yank_line))
                    .on_action(window.listener_for(&self.state, InputState::vim_substitute_char))
                    .on_action(window.listener_for(&self.state, InputState::vim_substitute_line))
                    .on_action(window.listener_for(&self.state, InputState::vim_open_line_below))
                    .on_action(window.listener_for(&self.state, InputState::vim_open_line_above))
                    .on_action(window.listener_for(&self.state, InputState::vim_toggle_case))
                    .on_action(window.listener_for(&self.state, InputState::vim_matching_bracket))
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_first_non_whitespace),
                    )
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_first_non_whitespace_down),
                    )
                    .on_action(window.listener_for(&self.state, InputState::vim_half_page_down))
                    .on_action(window.listener_for(&self.state, InputState::vim_half_page_up))
                    .on_action(window.listener_for(&self.state, InputState::vim_page_down))
                    .on_action(window.listener_for(&self.state, InputState::vim_page_up))
                    .on_action(window.listener_for(&self.state, InputState::vim_window_top))
                    .on_action(window.listener_for(&self.state, InputState::vim_window_middle))
                    .on_action(window.listener_for(&self.state, InputState::vim_window_bottom))
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_search_word_forward),
                    )
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_search_word_backward),
                    )
                    .on_action(window.listener_for(&self.state, InputState::vim_search_next))
                    .on_action(window.listener_for(&self.state, InputState::vim_search_prev))
                    // 等待一个字符的命令与注释
                    .on_action(window.listener_for(&self.state, InputState::vim_push_find_forward))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_find_before))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_find_backward))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_find_after))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_replace))
                    .on_action(window.listener_for(&self.state, InputState::vim_repeat_find))
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_repeat_find_reversed),
                    )
                    .on_action(window.listener_for(&self.state, InputState::vim_toggle_comment))
                    // Visual 模式
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_char))
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_line))
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_swap_ends))
                    .on_action(window.listener_for(&self.state, InputState::vim_reselect_visual))
                    .on_action(window.listener_for(&self.state, InputState::vim_repeat_change))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_register))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_set_mark))
                    .on_action(window.listener_for(&self.state, InputState::vim_push_jump_mark))
                    .on_action(
                        window.listener_for(&self.state, InputState::vim_push_jump_mark_line),
                    )
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_delete))
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_yank))
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_change))
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_indent))
                    .on_action(window.listener_for(&self.state, InputState::vim_visual_outdent))
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
                            .on_action(
                                window.listener_for(&self.state, InputState::toggle_soft_wrap),
                            )
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
