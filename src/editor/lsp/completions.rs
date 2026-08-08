//! Code completion: LSP `textDocument/completion`, a popup list, keyboard
//! navigation and insertion.
//!
//! Ported from gpui-component's `input/lsp/completions.rs`, adapted to
//! deepterm: the LSP round-trip runs in the session task via the change
//! channel and the popup is rendered by the editor element (deepterm has no
//! dedicated popover component).

use anyhow::Result;
use gpui::{App, Context, Task, Window};
use lsp_types::{CompletionContext, CompletionItem, CompletionResponse};
use ropey::Rope;

use super::LspChange;
use crate::components::input::{InputState, RopeExt as _};

/// A provider of code completions.
pub trait CompletionProvider {
    /// textDocument/completion
    fn completions(
        &self,
        _text: &Rope,
        _offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<CompletionResponse>>;
}

/// The completion popup state rendered by the editor element.
#[derive(Clone, Default)]
pub(crate) struct CompletionMenu {
    pub(crate) items: Vec<CompletionItem>,
    pub(crate) selected: usize,
    /// Byte offset where the current word started (completion replaces from
    /// here to the cursor).
    pub(crate) trigger_start: usize,
    pub(crate) visible: bool,
}

impl CompletionMenu {
    pub(crate) fn selected_item(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }
}

impl InputState {
    /// Whether the completion popup is open.
    pub(crate) fn is_completion_open(&self) -> bool {
        self.completion_menu
            .as_ref()
            .map(|m| m.visible && !m.items.is_empty())
            .unwrap_or(false)
    }

    /// Request completions for the current cursor position (debounced).
    pub(crate) fn schedule_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tx) = self.lsp.change_tx.clone() else {
            return;
        };
        if self.completion_inserting {
            return;
        }

        let offset = self.cursor();
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        if tx
            .try_send(LspChange::Completion {
                offset,
                reply: reply_tx,
            })
            .is_err()
        {
            return;
        }

        let editor = cx.entity();
        self.lsp._completion_task = cx.spawn_in(window, async move |_, cx| {
            // Small debounce so typing bursts don't spam the server.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            let response = match reply_rx.recv().await {
                Ok(Ok(response)) => response,
                _ => CompletionResponse::Array(vec![]),
            };

            let items = match response {
                CompletionResponse::Array(items) => items,
                CompletionResponse::List(list) => list.items,
            };

            let _ = editor.update(cx, |editor, cx| {
                // Cursor may have moved while awaiting; only show if still
                // relevant (cursor >= trigger word start).
                let trigger_start = editor
                    .text
                    .word_range(editor.cursor())
                    .map(|r| r.start)
                    .unwrap_or(editor.cursor());
                if items.is_empty() {
                    if let Some(menu) = editor.completion_menu.as_mut() {
                        menu.visible = false;
                        menu.items.clear();
                    }
                    cx.notify();
                    return;
                }
                editor.completion_menu = Some(super::CompletionMenu {
                    items,
                    selected: 0,
                    trigger_start,
                    visible: true,
                });
                cx.notify();
            });
        });
    }

    /// Navigate the completion list. Returns true when handled (menu open).
    pub(crate) fn completion_move(&mut self, down: bool, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.completion_menu.as_mut() else {
            return false;
        };
        if !menu.visible || menu.items.is_empty() {
            return false;
        }
        let len = menu.items.len();
        menu.selected = if down {
            (menu.selected + 1) % len
        } else {
            (menu.selected + len - 1) % len
        };
        cx.notify();
        true
    }

    /// Accept the selected completion, replacing the current word.
    /// Returns true when handled (menu open).
    pub(crate) fn accept_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.completion_menu.as_ref() else {
            return false;
        };
        let Some(item) = menu.selected_item().cloned() else {
            return false;
        };
        if !menu.visible {
            return false;
        }

        let trigger_start = menu.trigger_start;
        let cursor = self.cursor();
        self.completion_inserting = true;
        self.completion_menu = None;

        let insert = item
            .insert_text
            .clone()
            .unwrap_or_else(|| item.label.clone());
        if cursor > trigger_start {
            let range_utf16 = self.range_to_utf16(&(trigger_start..cursor));
            self.replace_text_in_range_silent(Some(range_utf16), &insert, window, cx);
        } else {
            let range_utf16 = self.range_to_utf16(&(cursor..cursor));
            self.replace_text_in_range_silent(Some(range_utf16), &insert, window, cx);
        }
        self.completion_inserting = false;
        cx.notify();
        true
    }

    pub(crate) fn close_completion(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = self.completion_menu.as_mut() {
            menu.visible = false;
            menu.items.clear();
        }
        cx.notify();
    }
}
