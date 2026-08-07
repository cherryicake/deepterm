//! LSP integration framework for the code editor.
//!
//! Ported from gpui-component's `input/lsp` module. Providers are trait
//! objects registered by the host (a real language server, a local
//! analyzer, …); `Lsp` coordinates async fetching, caching and composition
//! into the render pipeline. The actual LSP client / server management is
//! intentionally left to the host.

use std::rc::Rc;

use async_channel::Sender;
use gpui::{Context, SharedString, Task, Window};
use ropey::Rope;

use crate::components::input::InputState;

pub mod client;
pub mod semantic_tokens;
pub mod session;

pub use client::*;
pub use semantic_tokens::*;
pub use session::*;

/// A message sent from the editor (main thread) to the LSP session task.
pub enum LspChange {
    /// Full-document content sync after an edit.
    Text(String),
    /// The document was closed; the session should shut down.
    Close,
}

/// LSP-provided editor features.
pub struct Lsp {
    /// The range semantic tokens provider (semantic highlighting layered on
    /// top of tree-sitter).
    pub semantic_tokens_provider: Option<Rc<dyn DocumentRangeSemanticTokensProvider>>,

    /// Cached semantic tokens as absolute position ranges + theme token-type
    /// names. Color is resolved from the name at paint time so theme switches
    /// take effect without a refetch.
    semantic_tokens: Vec<(lsp_types::Range, SharedString)>,
    _semantic_tokens_task: Task<()>,

    /// The change channel the editor pushes document edits into; the LSP
    /// session task consumes it to sync content and refetch tokens.
    pub(crate) change_tx: Option<Sender<LspChange>>,
}

impl Default for Lsp {
    fn default() -> Self {
        Self {
            semantic_tokens_provider: None,
            semantic_tokens: vec![],
            _semantic_tokens_task: Task::ready(()),
            change_tx: None,
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

}
