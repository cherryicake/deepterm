//! LSP 文档符号大纲面板。
//!
//! `cmd-shift-o` 打开：列出当前文件的全部符号（函数/类/变量…），
//! 选中后跳到定义处。符号是请求时刻的快照——文件改动后列表会过期，
//! 所以面板关闭即销毁，不做增量刷新。

use gpui::prelude::*;
use gpui::{App, Context, Entity, IntoElement, Subscription, Window, div, px};

use crate::components::input::InputState;
use crate::lsp::features::{FlatSymbol, symbol_kind_marker};
use crate::theme::ActiveTheme as _;

use super::editor::DocumentSymbols;

/// 符号大纲面板。
pub struct SymbolsPanel {
    /// 目标编辑器。
    editor: Entity<InputState>,
    symbols: Vec<FlatSymbol>,
    selected: usize,
    /// 窗口句柄：确认后把焦点还给编辑器。
    window_handle: gpui::AnyWindowHandle,
    _subscription: Subscription,
}

impl SymbolsPanel {
    pub fn new(
        editor: Entity<InputState>,
        symbols: Vec<FlatSymbol>,
        window_handle: gpui::AnyWindowHandle,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_cx| Self {
            editor,
            symbols,
            selected: 0,
            window_handle,
            _subscription: Subscription::new(|| {}),
        })
    }

    pub fn set_symbols(&mut self, symbols: Vec<FlatSymbol>) {
        self.symbols = symbols;
        self.selected = 0;
    }

    /// 跳到选中的符号（键盘确认入口）。
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(symbol) = self.symbols.get(self.selected) else {
            return;
        };
        let (start, end) = (symbol.range.start, symbol.range.end);
        self.editor.update(cx, |state, cx| {
            state.selected_range = (start..end.max(start)).into();
            state.reveal_offset(start, cx);
            cx.notify();
        });
        // 焦点还给编辑器：否则键盘还在面板上，用户看不到跳转结果也没法继续输入。
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

    /// 移动选中项（键盘上下键入口）。
    pub fn move_selection(&mut self, delta: isize) {
        if self.symbols.is_empty() {
            return;
        }
        let len = self.symbols.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
    }
}

impl gpui::Render for SymbolsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let mut panel = div()
            .id("lsp-symbols-panel")
            .flex()
            .flex_col()
            .max_h(px(360.0))
            .min_w(px(280.0))
            .overflow_hidden()
            .py(px(4.0))
            .rounded(px(10.0))
            .bg(theme.panel_background.opacity(0.96))
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .text_size(px(12.0));

        if self.symbols.is_empty() {
            panel = panel.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_color(theme.text_muted)
                    .child("无符号"),
            );
            return panel.into_any_element();
        }

        for (i, symbol) in self.symbols.iter().enumerate() {
            let is_selected = i == self.selected;
            let indent = px(8.0 + symbol.depth as f32 * 12.0);
            let (start, end) = (symbol.range.start, symbol.range.end);
            let editor = self.editor.clone();

            let row = div()
                .id(("lsp-symbol", i))
                .flex()
                .items_center()
                .gap(px(6.0))
                .pl(indent)
                .pr(px(10.0))
                .py(px(2.0))
                .whitespace_nowrap()
                .cursor_pointer()
                .child(
                    div()
                        .w(px(14.0))
                        .text_color(theme.accent)
                        .child(symbol_kind_marker(symbol.kind)),
                )
                .child(
                    div()
                        .text_color(if is_selected { theme.text } else { theme.text_muted })
                        .child(symbol.name.clone()),
                )
                .when(is_selected, |this| this.bg(theme.accent.opacity(0.18)))
                .on_click(move |_, window, cx| {
                    editor.update(cx, |state, cx| {
                        state.selected_range = (start..end.max(start)).into();
                        state.reveal_offset(start, cx);
                        cx.notify();
                    });
                    // 跳完就把焦点还给编辑器并关闭面板（与回车确认同效果）。
                    let focus = editor.read(cx).focus_handle.clone();
                    focus.focus(window, cx);
                    window.dispatch_action(Box::new(DocumentSymbols), cx);
                });

            panel = panel.child(row);
        }

        panel.into_any_element()
    }
}
