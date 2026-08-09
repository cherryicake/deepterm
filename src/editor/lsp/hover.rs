//! LSP hover support: requests the hover for a symbol under the cursor and
//! records it for the hover popover / definition underline.
//!
//! Ported from gpui-component's `input/lsp/hover.rs`; the round-trip runs in
//! the session task via the change channel.

use anyhow::Result;
use gpui::{App, Context, Task, Window};
use ropey::Rope;

use super::{LspChange, lsp_position_to_offset};
use crate::components::input::{InputState, RopeExt as _};

/// Hover provider.
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
pub trait HoverProvider {
    /// textDocument/hover
    fn hover(
        &self,
        _text: &Rope,
        _offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>>;
}

/// The hover popover content currently shown, if any.
#[derive(Clone, Default)]
pub(crate) struct HoverPopoverData {
    /// Byte range of the hovered symbol in the document.
    pub(crate) symbol_range: std::ops::Range<usize>,
    /// Markdown-ish text of the hover contents (rendered by the popover UI).
    #[allow(dead_code)]
    pub(crate) contents: String,
}

impl InputState {
    /// Request hover content for `offset` (debounced 150ms) and record it.
    pub(crate) fn handle_hover_popover(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting {
            return;
        }

        let Some(tx) = self.lsp.change_tx.clone() else {
            return;
        };

        if let Some(popover) = self.hover_popover.as_ref() {
            if popover.symbol_range.contains(&offset) {
                return;
            }
        }

        let (reply_tx, reply_rx) = async_channel::bounded(1);
        if tx
            .try_send(LspChange::Hover {
                offset,
                reply: reply_tx,
            })
            .is_err()
        {
            return;
        }

        let mut symbol_range = self.text.word_range(offset).unwrap_or(offset..offset);
        let editor = cx.entity();
        let should_delay = self.hover_popover.is_none();
        self.lsp._hover_task = cx.spawn_in(window, async move |_, cx| {
            if should_delay {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
            }

            let hover = match reply_rx.recv().await {
                Ok(Ok(hover)) => hover,
                _ => None,
            };

            let _ = editor.update(cx, |editor, cx| match hover {
                Some(hover) => {
                    if let Some(range) = hover.range {
                        let start = lsp_position_to_offset(&editor.text, &range.start);
                        let end = lsp_position_to_offset(&editor.text, &range.end);
                        symbol_range = start..end;
                    }
                    let contents = hover_contents_to_plain_text(&hover.contents);
                    let had_popover = editor.hover_popover.is_some();
                    editor.hover_popover = Some(super::HoverPopoverData {
                        symbol_range: symbol_range.clone(),
                        contents,
                    });
                    if !had_popover {
                        // Must repaint so the popup appears (mouse-move
                        // handlers already ran with the old state).
                        cx.notify();
                    }
                }
                None => {
                    if editor.hover_popover.take().is_some() {
                        cx.notify();
                    }
                }
            });

            Ok(())
        });
    }
}

/// Flatten LSP hover contents (MarkupContent / MarkedString / array) into a
/// plain-text summary.
fn hover_contents_to_plain_text(contents: &lsp_types::HoverContents) -> String {
    match contents {
        lsp_types::HoverContents::Markup(content) => {
            let value = content.value.trim().to_string();
            // Keep the first few lines; strip common markdown fences.
            let mut lines: Vec<&str> = value.lines().collect();
            while lines.first().map(|l| l.trim().is_empty() || l.trim().starts_with("```"))
                == Some(true)
            {
                lines.remove(0);
            }
            while lines
                .last()
                .map(|l| l.trim().is_empty() || l.trim() == "```")
                == Some(true)
            {
                lines.pop();
            }
            lines.into_iter().collect::<Vec<_>>().join("\n")
        }
        lsp_types::HoverContents::Scalar(marked) => match marked {
            lsp_types::MarkedString::String(s) => s.clone(),
            lsp_types::MarkedString::LanguageString(lang) => {
                format!("```{}\n{}\n```", lang.language, lang.value)
            }
        },
        lsp_types::HoverContents::Array(items) => items
            .iter()
            .map(|item| match item {
                lsp_types::MarkedString::String(s) => s.clone(),
                lsp_types::MarkedString::LanguageString(lang) => {
                    format!("```{}\n{}\n```", lang.language, lang.value)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hover_contents_extraction_markdown() {
        // rust-analyzer 风格：MarkupContent（markdown），代码块包裹签名
        let contents = lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "```rust\npub fn open_file(&mut self, path: PathBuf) {\n```\n\nOpens a file.".to_string(),
        });
        let text = hover_contents_to_plain_text(&contents);
        assert!(!text.is_empty(), "extracted hover text must not be empty");
        assert!(text.contains("pub fn"), "signature preserved: {text}");
    }

    #[test]
    fn test_hover_contents_extraction_plain() {
        let contents = lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(
            "fn open_file".to_string(),
        ));
        let text = hover_contents_to_plain_text(&contents);
        assert_eq!(text, "fn open_file");
    }
}
