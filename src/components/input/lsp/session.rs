//! A long-running LSP session bound to one open document: spawns the server,
//! registers the document, syncs edits, and continuously refreshes semantic
//! tokens into the editor's `Lsp` cache.
//!
//! The session task runs as a gpui async task (main thread, but never
//! blocking); the server process and the stdout read loop live on the
//! background executor. The editor pushes [`LspChange`] messages through a
//! channel on every edit.

use anyhow::{Context as _, Result};
use async_channel::{Receiver, Sender, bounded};
use gpui::{App, AsyncApp, WeakEntity};
use ropey::Rope;

use super::{LspChange, LspClient};
use super::semantic_tokens::decode_semantic_tokens;
use crate::components::input::InputState;

/// Server configuration for a language.
pub struct LspServerSpec {
    pub language_id: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// `file://` URI of the workspace root (or the file's directory).
    pub root_uri: String,
}

/// Start an LSP session for `editor`'s document at `uri`.
///
/// Returns a channel the editor pushes [`LspChange`]s into, or `None` when
/// no session was started.
pub fn start_lsp_session(
    editor: WeakEntity<InputState>,
    uri: String,
    spec: LspServerSpec,
    cx: &mut App,
) -> Option<Sender<LspChange>> {
    let (tx, rx) = bounded::<LspChange>(16);

    cx.spawn(async move |cx| {
        run_session(editor, uri, spec, rx, cx).await;
    })
    .detach();

    Some(tx)
}

/// Spawn the LSP server and complete the initialize handshake, trying a
/// fallback command when the primary one fails or stalls.
///
/// rust-analyzer is often installed through rustup, where `rust-analyzer`
/// on the shim resolves to the active toolchain's component — which may be
/// missing (e.g. nightly without the rust-analyzer component). The shim then
/// exits immediately and the initialize request never gets a response, so we
/// race it against a timeout and retry with `rustup run stable`.
async fn spawn_and_initialize(
    spec: &LspServerSpec,
    cx: &AsyncApp,
) -> Option<LspClient> {
    let mut commands: Vec<(&str, Vec<&'static str>)> = vec![(spec.command, spec.args.to_vec())];
    if spec.command == "rust-analyzer" {
        commands.push(("rustup", vec!["run", "stable", "rust-analyzer"]));
    }

    for (cmd, args) in commands {
        let spawned = cx
            .background_executor()
            .spawn({
                let cmd = cmd.to_string();
                let args = args.clone();
                async move { LspClient::spawn(&cmd, &args).await }
            })
            .await;
        let Ok(mut client) = spawned else {
            continue;
        };

        // Drive the read loop so responses are dispatched while awaiting.
        if let Some(reader) = client.take_reader() {
            let pending = client.pending();
            cx.background_executor()
                .spawn(async move {
                    super::client::read_loop(reader, pending).await;
                })
                .detach();
        }

        let root = spec.root_uri.clone();
        let init = async { client.initialize(&root).await };
        let timeout = async {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(8))
                .await;
            Err::<serde_json::Value, anyhow::Error>(anyhow::anyhow!("initialize timeout"))
        };
        match smol::future::race(init, timeout).await {
            Ok(_) => return Some(client),
            Err(e) => {
                log::warn!("LSP initialize failed via {cmd}: {e}");
                let _ = client.shutdown().await;
            }
        }
    }
    None
}

async fn run_session(
    editor: WeakEntity<InputState>,
    uri: String,
    spec: LspServerSpec,
    rx: Receiver<LspChange>,
    cx: &AsyncApp,
) {
    let mut client = match spawn_and_initialize(&spec, cx).await {
        Some(client) => client,
        None => {
            log::warn!("LSP session could not be established for {}", spec.command);
            return;
        }
    };

    // Extract the semantic-tokens legend advertised by the server.
    let capabilities = client.take_initialize_capabilities().unwrap_or_default();

    let legend = capabilities
        .get("capabilities")
        .and_then(|c| c.get("semanticTokensProvider"))
        .and_then(|p| p.get("legend"))
        .and_then(|v| serde_json::from_value::<lsp_types::SemanticTokensLegend>(v.clone()).ok())
        .unwrap_or_else(default_legend);

    // Wait for the first content push from the editor.
    let mut current_text = match rx.recv().await {
        Ok(LspChange::Text(text)) => text,
        _ => return,
    };

    let mut version = 1i64;
    let _ = cx.update(|cx| {
        editor.update(cx, |state, _| {
            state.lsp.document_uri = uri.clone();
        })
    });
    let _ = client
        .did_open(&uri, spec.language_id, version, &current_text)
        .await;

    let _ = fetch_and_apply(&mut client, &editor, &uri, &current_text, &legend, cx).await;

    loop {
        match rx.recv().await {
            Ok(LspChange::Text(text)) => {
                current_text = text;
                version += 1;
                let _ = client.did_change(&uri, version, &current_text).await;
                let _ =
                    fetch_and_apply(&mut client, &editor, &uri, &current_text, &legend, cx).await;
            }
            Ok(LspChange::Hover { offset, reply }) => {
                let position = super::lsp_types_position_from_offset(&current_rope(&current_text), offset);
                let result = client.hover(&uri, (position.line, position.character)).await;
                let _ = reply.try_send(result);
            }
            Ok(LspChange::Definition { offset, reply }) => {
                let position = super::lsp_types_position_from_offset(&current_rope(&current_text), offset);
                let result = client.definition(&uri, (position.line, position.character)).await;
                let _ = reply.try_send(result);
            }
            Ok(LspChange::Completion { offset, reply }) => {
                let position = super::lsp_types_position_from_offset(&current_rope(&current_text), offset);
                let result = client.completion(&uri, (position.line, position.character)).await;
                let _ = reply.try_send(result);
            }
            Ok(LspChange::Close) | Err(_) => break,
        }
    }

    let _ = client.shutdown().await;
}

fn current_rope(text: &str) -> Rope {
    Rope::from_str(text)
}

async fn fetch_and_apply(
    client: &mut LspClient,
    editor: &WeakEntity<InputState>,
    uri: &str,
    text: &str,
    legend: &lsp_types::SemanticTokensLegend,
    cx: &AsyncApp,
) -> Result<()> {
    let rope = Rope::from_str(text);
    let lines = rope.len_lines(ropey::LineType::LF);
    let last_line = lines.saturating_sub(1) as u32;
    let last_len = rope
        .line(lines.saturating_sub(1), ropey::LineType::LF)
        .chars()
        .count() as u32;

    let tokens = client
        .semantic_tokens_range(uri, (0, 0), (last_line, last_len))
        .await
        .context("semanticTokens/range failed")?;
    let decoded = decode_semantic_tokens(&tokens, legend);

    let _ = cx.update(|cx| editor.update(cx, |state, cx| {
        state.lsp.set_semantic_tokens(decoded, cx);
    }));
    Ok(())
}

/// A conservative fallback legend matching the common token-type names the
/// highlight theme understands, when the server does not advertise one.
fn default_legend() -> lsp_types::SemanticTokensLegend {
    let types = [
        "namespace", "type", "class", "enum", "interface", "struct", "typeParameter", "parameter",
        "variable", "property", "enumMember", "event", "function", "method", "macro", "keyword",
        "modifier", "comment", "string", "number", "regexp", "operator",
    ];
    lsp_types::SemanticTokensLegend {
        token_types: types
            .iter()
            .map(|t| lsp_types::SemanticTokenType::from(*t))
            .collect(),
        token_modifiers: vec![],
    }
}
