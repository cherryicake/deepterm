//! LSP integration framework for the code editor.
//!
//! Ported from gpui-component's `input/lsp` module. The built-in session
//! (`session.rs`) drives a real language server; the provider traits are also
//! exposed for hosts that want to plug in their own.

use std::rc::Rc;

use anyhow::Result;
use async_channel::Sender;
use gpui::{Context, SharedString, Task, Window};
use ropey::Rope;

use crate::components::input::{InputState, RopeExt as _};

pub mod client;
pub mod completions;
pub mod definitions;
pub mod hover;
pub mod semantic_tokens;
pub mod session;

pub use client::*;
pub use completions::*;
pub use definitions::*;
pub use hover::*;
pub use semantic_tokens::*;
pub use session::*;

/// Convert a byte offset to an LSP position (UTF-16 characters).
pub(crate) fn lsp_types_position_from_offset(
    text: &Rope,
    offset: usize,
) -> lsp_types::Position {
    let point = text.offset_to_point(offset);
    let line = text.slice_line(point.row);
    let character = line
        .slice(..point.column)
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum();
    lsp_types::Position::new(point.row as u32, character)
}

/// A message sent from the editor (main thread) to the LSP session task.
pub enum LspChange {
    /// Full-document content sync after an edit.
    Text(String),
    /// A hover request at a byte offset; the reply carries the hover or
    /// `None`.
    Hover {
        offset: usize,
        reply: async_channel::Sender<Result<Option<lsp_types::Hover>>>,
    },
    /// A definition request at a byte offset.
    Definition {
        offset: usize,
        reply: async_channel::Sender<Result<Vec<lsp_types::LocationLink>>>,
    },
    /// A completion request at a byte offset.
    Completion {
        offset: usize,
        reply: async_channel::Sender<Result<lsp_types::CompletionResponse>>,
    },
    /// The document was closed; the session should shut down.
    Close,
}

/// LSP-provided editor features.
pub struct Lsp {
    /// The range semantic tokens provider (semantic highlighting layered on
    /// top of tree-sitter).
    pub semantic_tokens_provider: Option<Rc<dyn DocumentRangeSemanticTokensProvider>>,
    /// The hover provider.
    pub hover_provider: Option<Rc<dyn HoverProvider>>,
    /// The definition provider.
    pub definition_provider: Option<Rc<dyn DefinitionProvider>>,
    /// The completion provider.
    pub completion_provider: Option<Rc<dyn CompletionProvider>>,
    /// Cached semantic tokens as absolute position ranges + theme token-type
    /// names. Color is resolved from the name at paint time so theme switches
    /// take effect without a refetch.
    semantic_tokens: Vec<(lsp_types::Range, SharedString)>,
    _semantic_tokens_task: Task<()>,
    _hover_task: Task<Result<()>>,
    _completion_task: Task<()>,

    /// The change channel the editor pushes document edits into; the LSP
    /// session task consumes it to sync content and refetch tokens.
    pub(crate) change_tx: Option<Sender<LspChange>>,
    /// The `file://` URI of the attached document.
    pub(crate) document_uri: String,
}

impl Default for Lsp {
    fn default() -> Self {
        Self {
            semantic_tokens_provider: None,
            hover_provider: None,
            definition_provider: None,
            completion_provider: None,
            semantic_tokens: vec![],
            _semantic_tokens_task: Task::ready(()),
            _hover_task: Task::ready(Ok(())),
            _completion_task: Task::ready(()),
            change_tx: None,
            document_uri: String::new(),
        }
    }
}

impl Lsp {
    /// Update LSP state when the text changes.
    pub(crate) fn update(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        self.update_semantic_tokens(text, window, cx);
    }

    /// Reset all LSP state.
    #[allow(dead_code)]
    pub(crate) fn reset(&mut self) {
        self.semantic_tokens.clear();
        self._semantic_tokens_task = Task::ready(());
        self._hover_task = Task::ready(Ok(()));
    }

    /// Replace the cached semantic tokens (called by the LSP session when a
    /// server response arrives) and notify on change.
    pub(crate) fn set_semantic_tokens(
        &mut self,
        tokens: Vec<(lsp_types::Range, SharedString)>,
        cx: &mut Context<InputState>,
    ) {
        if tokens != self.semantic_tokens {
            self.semantic_tokens = tokens;
            cx.notify();
        }
    }

    /// Drop cached semantic tokens when the document changes. Without this,
    /// stale tokens (byte ranges from the pre-edit text) are composed onto
    /// the new text and flicker as the cursor moves / text shifts; the fresh
    /// tokens arrive asynchronously right after.
    pub(crate) fn invalidate_semantic_tokens(&mut self, cx: &mut Context<InputState>) {
        if !self.semantic_tokens.is_empty() {
            self.semantic_tokens.clear();
            cx.notify();
        }
    }

    pub(crate) fn document_uri(&self) -> &str {
        &self.document_uri
    }
}
