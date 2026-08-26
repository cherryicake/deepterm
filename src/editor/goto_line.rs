//! 跳转到行面板：VSCode 风格的顶部居中悬浮卡片（macOS Liquid Glass 视觉）。

use gpui::prelude::*;
use gpui::{App, Context, Entity, IntoElement, Subscription, Window, div, px};

use crate::components::input::{EditorInput, InputEvent, InputState, RopeExt};
use crate::theme::ActiveTheme;

use super::editor::GotoLine;

pub struct GotoLinePanel {
    pub(crate) input: Entity<InputState>,
    /// 目标编辑器（当前激活的文本编辑器）。
    editor: Entity<InputState>,
    total_lines: usize,
    /// 最近一次解析错误提示（如超出范围）。
    error: Option<&'static str>,
    window_handle: gpui::AnyWindowHandle,
    _subscription: Subscription,
}

impl GotoLinePanel {
    pub fn new(
        editor: Entity<InputState>,
        window_handle: gpui::AnyWindowHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let total_lines = editor.read(cx).text().lines_len().max(1);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(format!("行号 (1-{total_lines})，支持 行:列"))
                .submit_on_enter(true)
        });

        let panel = cx.new(|_cx| Self {
            input: input.clone(),
            editor,
            total_lines,
            error: None,
            window_handle,
            _subscription: Subscription::new(|| {}),
        });

        let subscription = panel.update(cx, |_, cx| {
            cx.subscribe(&input, |this: &mut Self, _, event, cx| {
                match event {
                    InputEvent::Submit => this.confirm(cx),
                    InputEvent::Change => {
                        if this.error.is_some() {
                            this.error = None;
                            cx.notify();
                        }
                    }
                    _ => {}
                }
            })
        });
        panel.update(cx, |this, _| this._subscription = subscription);

        panel
    }

    /// 打开时聚焦输入框。
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input
            .read(cx)
            .focus_handle
            .clone()
            .focus(window, cx);
    }

    /// 解析输入（"42" 或 "42:7"）并跳转。
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let raw = self.input.read(cx).value().to_string();
        let total = self.total_lines.max(1);
        let parse = |s: &str| -> Option<(usize, usize)> {
            let mut parts = s.trim().splitn(2, ':');
            let line = parts.next()?.trim().parse::<usize>().ok()?;
            let col = match parts.next().map(|c| c.trim().parse::<usize>()) {
                Some(Ok(col)) => col,
                Some(Err(_)) => return None,
                None => 1,
            };
            if line == 0 {
                return None;
            }
            Some((line.min(total), col.max(1)))
        };
        let Some((line, col)) = parse(&raw) else {
            self.error = Some("格式无效，示例：42 或 42:7");
            cx.notify();
            return;
        };

        let editor = self.editor.clone();
        let mut jumped = false;
        editor.update(cx, |state, cx| {
            let row = line - 1;
            let line_start = state.text().line_start_offset(row);
            let line_end = state.text().line_end_offset(row);
            // 列按字符计（1 起）：跳过本行前 col-1 个字符
            let mut offset = line_start;
            let mut remaining = col - 1;
            for ch in state.text().slice(line_start..line_end).chars() {
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                offset += ch.len_utf8();
            }
            state.selected_range = (offset..offset).into();
            state.scroll_to_center(offset, cx);
            jumped = true;
            cx.notify();
        });

        // 跳转成功：关闭面板并把焦点还给编辑器
        if jumped {
            let editor = self.editor.clone();
            let handle = self.window_handle;
            cx.spawn(async move |_, cx| {
                let _ = cx.update_window(handle, |_root, window, cx| {
                    let focus = editor.read(cx).focus_handle.clone();
                    focus.focus(window, cx);
                });
            })
            .detach();
        }
    }
}

impl gpui::Render for GotoLinePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        div()
            .id("goto-line-panel")
            .on_key_down(|event: &gpui::KeyDownEvent, window, cx| {
                // Escape：再次派发 GotoLine 动作 → Editor 侧切换关闭面板
                if event.keystroke.key == "escape" {
                    window.dispatch_action(Box::new(GotoLine), cx);
                }
            })
            // 液态玻璃卡片
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
            .when_some(self.error, |this, _| {
                this.child(
                    div()
                        .text_color(theme.error_foreground)
                        .text_size(px(12.0))
                        .child("格式无效"),
                )
            })
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child("转到行"),
            )
            .child(
                div()
                    .w(px(150.0))
                    .child(EditorInput::new(&self.input).transparent().small()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(format!("共 {} 行", self.total_lines)),
            )
            .into_any_element()
    }
}
