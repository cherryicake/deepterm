//! LSP 重命名浮层：VSCode 风格的顶部居中输入框。
//!
//! 与 [`super::goto_line::GotoLinePanel`] 同一套视觉与结构：
//! `Editor` 持有 `Option<Entity<..>>`，作为 `relative()` 父容器的
//! `absolute()` 兄弟节点渲染。
//!
//! 提交后向服务器发 `textDocument/rename`，拿到 `WorkspaceEdit` 后只应用
//! **当前文件**的那部分——跨文件的重命名需要额外的文件写入与磁盘同步，
//! 首版不做（服务器给的其他文件编辑会被跳过并记日志）。

use gpui::prelude::*;
use gpui::{App, Context, Entity, IntoElement, Subscription, Window, div, px};

use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::editor::editor::Editor;
use crate::theme::ActiveTheme as _;

use super::editor::RenameSymbol;

pub struct RenamePanel {
    pub(crate) input: Entity<InputState>,
    /// 目标编辑器。
    editor: Entity<InputState>,
    /// 宿主 `Editor`：rename 请求需要它身上的会话（uri + client）。
    host: gpui::WeakEntity<Editor>,
    /// 光标处的原符号名，作为输入框初始值。
    original: String,
    window_handle: gpui::AnyWindowHandle,
    _subscription: Subscription,
}

impl RenamePanel {
    pub fn new(
        host: gpui::WeakEntity<Editor>,
        editor: Entity<InputState>,
        window_handle: gpui::AnyWindowHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        // 初始值取光标下的词：用户通常是在改这个名字。
        let original = editor.read(cx).word_at_cursor();
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .placeholder("新名称")
                .submit_on_enter(true);
            state.set_value(&original, window, cx);
            state
        });

        let panel = cx.new(|_cx| Self {
            input: input.clone(),
            editor,
            host,
            original,
            window_handle,
            _subscription: Subscription::new(|| {}),
        });

        let subscription = panel.update(cx, |_, cx| {
            cx.subscribe(&input, |this: &mut Self, _, event, cx| {
                if let InputEvent::Submit = event {
                    this.confirm(cx);
                }
            })
        });
        panel.update(cx, |this, _| this._subscription = subscription);

        panel
    }

    /// 打开时聚焦输入框并全选（改名通常整名替换）。
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.read(cx).focus_handle.clone().focus(window, cx);
        self.input.update(cx, |state, cx| {
            let len = state.text().len();
            state.selected_range = (0..len).into();
            cx.notify();
        });
    }

    /// 提交新名称并发起 rename 请求。
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let new_name = self.input.read(cx).value().trim().to_string();
        if new_name.is_empty() || new_name == self.original {
            // 空或没改：直接关闭，不发请求（服务器会对空名报错）。
            self.close(cx);
            return;
        }

        let editor = self.editor.clone();
        let position = editor.read(cx).lsp_cursor_position();
        let handle = self.window_handle;

        // 请求走 `Editor`：会话（uri + 客户端）在它身上，面板拿不到。
        if let Some(host) = self.host.upgrade() {
            host.update(cx, |host, cx| {
                host.apply_rename(&editor, position, new_name, cx);
            });
        }

        cx.spawn(async move |_, cx| {
            let _ = cx.update_window(handle, |_root, window, cx| {
                let focus = editor.read(cx).focus_handle.clone();
                focus.focus(window, cx);
            });
        })
        .detach();

        self.close(cx);
    }

    /// 关闭：再次派发 RenameSymbol 让 Editor 收回面板。
    fn close(&mut self, cx: &mut Context<Self>) {
        let handle = self.window_handle;
        cx.spawn(async move |_, cx| {
            let _ = cx.update_window(handle, |_root, window, cx| {
                window.dispatch_action(Box::new(RenameSymbol), cx);
            });
        })
        .detach();
    }
}

impl gpui::Render for RenamePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        div()
            .id("lsp-rename-panel")
            .on_key_down(|event: &gpui::KeyDownEvent, window, cx| {
                // Escape：派发 RenameSymbol 动作 → Editor 侧切换关闭面板
                if event.keystroke.key == "escape" {
                    window.dispatch_action(Box::new(RenameSymbol), cx);
                }
            })
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(5.0))
            .rounded(px(14.0))
            .bg(theme.panel_background.opacity(0.88))
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child("重命名"),
            )
            .child(
                div()
                    .w(px(180.0))
                    .child(EditorInput::new(&self.input).transparent().small()),
            )
            .into_any_element()
    }
}
