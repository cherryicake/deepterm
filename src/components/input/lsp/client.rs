//! Minimal LSP client: stdio JSON-RPC transport with request/response
//! correlation and the document lifecycle needed for semantic highlighting.
//!
//! The transport is synchronous over an owned async reader/writer so it can
//! run on any executor (deepterm uses the gpui background executor).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use async_channel::{Sender, bounded};
use async_process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use futures::{AsyncReadExt, AsyncWriteExt};
use parking_lot::Mutex;
use serde_json::{Value, json};

/// Frame a JSON-RPC message with the LSP `Content-Length` header.
pub fn encode_message(value: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).expect("serialize json-rpc message");
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Parse one JSON-RPC message from `buf`, returning the parsed value and the
/// number of consumed bytes. Returns `Ok(None)` when the buffer holds only a
/// partial message.
pub fn decode_message(buf: &[u8]) -> Result<Option<(Value, usize)>> {
    // Headers end with \r\n\r\n.
    let header_end = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return Ok(None), // partial header — wait for more data
    };

    let header = std::str::from_utf8(&buf[..header_end])?;
    let mut content_length = None;
    for line in header.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
    }
    let Some(content_length) = content_length else {
        return Err(anyhow!("missing Content-Length header"));
    };

    let total = header_end + 4 + content_length;
    if buf.len() < total {
        return Ok(None);
    }

    let body = &buf[header_end + 4..total];
    let value = serde_json::from_slice(body)?;
    Ok(Some((value, total)))
}

/// Read and dispatch `Content-Length` framed messages from the server until
/// EOF. Runs on the caller's executor (deepterm drives it on the gpui
/// background executor); do not `spawn` this on an executor that is not
/// running.
pub(crate) async fn read_loop(
    mut reader: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, Sender<Result<Value>>>>>,
) {
    let mut buf = Vec::with_capacity(4096);
    let mut scratch = [0u8; 8192];

    loop {
        // Accumulate until at least one full message is available.
        loop {
            match decode_message(&buf) {
                Ok(Some((value, consumed))) => {
                    buf.drain(..consumed);
                    eprintln!("LSP read: {}", serde_json::to_string(&value).unwrap_or_default().chars().take(120).collect::<String>());
                    handle_server_message(value, &pending);
                    break;
                }
                Ok(None) => {
                    let n = match reader.read(&mut scratch).await {
                        Ok(0) => return, // EOF
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    buf.extend_from_slice(&scratch[..n]);
                }
                Err(e) => {
                    eprintln!("LSP protocol err: {e}; buf={:?}", String::from_utf8_lossy(&buf).chars().take(80).collect::<String>());
                    return;
                }
            }
        }
    }
}

fn handle_server_message(value: Value, pending: &Arc<Mutex<HashMap<u64, Sender<Result<Value>>>>>) {
    if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
        let mut map = pending.lock();
        if let Some(tx) = map.remove(&id) {
            // Response (possibly an error response).
            // `send` is async; this runs on the read-loop task without
            // awaiting, so use the synchronous `try_send` (the channel is
            // bounded(1) and the receiver is always waiting).
            if let Some(error) = value.get("error") {
                let _ = tx.try_send(Err(anyhow!("LSP error: {}", error)));
            } else {
                let _ = tx.try_send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
    }
    // Notifications (e.g. window/logMessage) are ignored for now.
}

/// An LSP client speaking stdio JSON-RPC.
pub struct LspClient {
    _child: Child,
    stdin: ChildStdin,
    stdout: Option<ChildStdout>,
    next_id: u64,
    pending: Arc<Mutex<HashMap<u64, Sender<Result<Value>>>>>,
}

impl LspClient {
    /// Take the stdout reader so the caller can drive [`read_loop`] on its
    /// own executor.
    pub fn take_reader(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Clone the pending-response registry for the read loop.
    pub fn pending(&self) -> Arc<Mutex<HashMap<u64, Sender<Result<Value>>>>> {
        self.pending.clone()
    }
}

impl LspClient {
    /// Spawn `command` with stdio pipes. The caller must take the stdout
    /// reader and drive [`read_loop`], then call [`initialize`] /
    /// [`initialized`](Self::initialized) to complete the handshake.
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("failed to spawn LSP server")?;
        let stdin = child.stdin.take().context("server stdin unavailable")?;
        let stdout = child.stdout.take().context("server stdout unavailable")?;

        let pending: Arc<Mutex<HashMap<u64, Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let client = Self {
            _child: child,
            stdin,
            stdout: Some(stdout),
            next_id: 0,
            pending,
        };

        Ok(client)
    }

    /// Send the `initialize` request (must be the first message) and return
    /// the server capabilities.
    pub async fn initialize(&mut self, root_uri: &str) -> Result<Value> {
        let capabilities = self
            .send_request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "capabilities": {},
                    "clientInfo": { "name": "deepterm", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        self.send_notification("initialized", json!({})).await?;
        Ok(capabilities)
    }

    /// Send a request and await its response.
    pub async fn send_request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = bounded::<Result<Value>>(1);
        self.pending.lock().insert(id, tx);

        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&message).await?;

        rx.recv().await.context("LSP server closed while awaiting response")?
    }

    /// Fire-and-forget notification.
    pub async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&message).await
    }

    async fn write_message(&mut self, message: &Value) -> Result<()> {
        let frame = encode_message(message);
        self.stdin
            .write_all(&frame)
            .await
            .context("failed to write to LSP server")?;
        self.stdin.flush().await.ok();
        Ok(())
    }

    /// Register an open document.
    pub async fn did_open(
        &mut self,
        uri: &str,
        language_id: &str,
        version: i64,
        text: &str,
    ) -> Result<()> {
        self.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": text,
                }
            }),
        )
        .await
    }

    /// Full-document content sync (simple and always correct).
    pub async fn did_change(&mut self, uri: &str, version: i64, text: &str) -> Result<()> {
        self.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
        .await
    }

    pub async fn did_close(&mut self, uri: &str) -> Result<()> {
        self.send_notification(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await
    }

    /// textDocument/semanticTokens/range — returns the raw delta-encoded
    /// tokens (decode with [`crate::components::input::lsp::decode_semantic_tokens`]).
    pub async fn semantic_tokens_range(
        &mut self,
        uri: &str,
        start: (u32, u32),
        end: (u32, u32),
    ) -> Result<lsp_types::SemanticTokens> {
        let resp = self
            .send_request(
                "textDocument/semanticTokens/range",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 },
                    }
                }),
            )
            .await?;
        serde_json::from_value(resp).context("invalid semanticTokens response")
    }

    /// textDocument/hover.
    pub async fn hover(
        &mut self,
        uri: &str,
        position: (u32, u32),
    ) -> Result<Option<lsp_types::Hover>> {
        let resp = self
            .send_request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.0, "character": position.1 },
                }),
            )
            .await?;
        if resp.is_null() {
            return Ok(None);
        }
        serde_json::from_value(resp).context("invalid hover response")
    }

    /// textDocument/definition — returns location links.
    pub async fn definition(
        &mut self,
        uri: &str,
        position: (u32, u32),
    ) -> Result<Vec<lsp_types::LocationLink>> {
        let resp = self
            .send_request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.0, "character": position.1 },
                }),
            )
            .await?;
        // Servers may return `Location[]` or `LocationLink[]`.
        if let Some(links) =
            serde_json::from_value::<Vec<lsp_types::LocationLink>>(resp.clone()).ok()
        {
            return Ok(links);
        }
        if let Ok(locations) = serde_json::from_value::<Vec<lsp_types::Location>>(resp) {
            return Ok(locations
                .into_iter()
                .map(|loc| lsp_types::LocationLink {
                    origin_selection_range: None,
                    target_uri: loc.uri,
                    target_range: loc.range,
                    target_selection_range: loc.range,
                })
                .collect());
        }
        Ok(Vec::new())
    }

    /// textDocument/completion.
    pub async fn completion(
        &mut self,
        uri: &str,
        position: (u32, u32),
    ) -> Result<lsp_types::CompletionResponse> {
        let resp = self
            .send_request(
                "textDocument/completion",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.0, "character": position.1 },
                    "context": {
                        "triggerKind": 1, // Invoked
                    },
                }),
            )
            .await?;
        if resp.is_null() {
            return Ok(lsp_types::CompletionResponse::Array(vec![]));
        }
        serde_json::from_value(resp).context("invalid completion response")
    }

    /// Graceful shutdown: `shutdown` request, `exit` notification.
    pub async fn shutdown(mut self) {
        let _ = self.send_request("shutdown", json!(null)).await;
        let _ = self.send_notification("exit", json!(null)).await;
        let _ = self._child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "processId": 123 },
        });
        let frame = encode_message(&value);
        let prefix = format!("Content-Length: {}\r\n\r\n", serde_json::to_vec(&value).unwrap().len());
        assert!(frame.starts_with(prefix.as_bytes()));
        let (decoded, consumed) = decode_message(&frame).unwrap().unwrap();
        assert_eq!(decoded, value);
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn test_decode_partial_returns_none() {
        let value = json!({ "jsonrpc": "2.0", "id": 2, "method": "x" });
        let frame = encode_message(&value);
        // Only part of the body.
        let partial = &frame[..frame.len() - 3];
        assert!(decode_message(partial).unwrap().is_none());
        // Empty buffer.
        assert!(decode_message(b"").unwrap().is_none());
    }

    #[test]
    fn test_decode_two_messages() {
        let a = json!({ "jsonrpc": "2.0", "id": 1 });
        let b = json!({ "jsonrpc": "2.0", "id": 2 });
        let mut buf = encode_message(&a);
        buf.extend_from_slice(&encode_message(&b));

        let (va, consumed) = decode_message(&buf).unwrap().unwrap();
        assert_eq!(va, a);
        let (vb, _) = decode_message(&buf[consumed..]).unwrap().unwrap();
        assert_eq!(vb, b);
    }

    #[test]
    fn test_decode_missing_content_length_errors() {
        let buf = b"Foo: 1\r\n\r\n{}";
        assert!(decode_message(buf).is_err());
    }

    /// End-to-end against a real rust-analyzer (requires it on `PATH`). Run
    /// with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires rust-analyzer on PATH"]
    fn test_rust_analyzer_semantic_tokens_e2e() {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async {
            let mut client =
                LspClient::spawn("rustup", &["run", "stable", "rust-analyzer"]).await.expect("spawn rust-analyzer");
            let pending = client.pending();
            let reader = client.take_reader().expect("stdout reader");

            let read_task = tokio::spawn(async move {
                read_loop(reader, pending).await;
            });

            let _ = client
                .initialize("file:///tmp")
                .await
                .expect("initialize");
            client
                .did_open(
                    "file:///tmp/main.rs",
                    "rust",
                    1,
                    "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n",
                )
                .await
                .expect("didOpen");

            let tokens = client
                .semantic_tokens_range(
                    "file:///tmp/main.rs",
                    (0, 0),
                    (3, 1),
                )
                .await
                .expect("semanticTokens/range");
            assert!(
                !tokens.data.is_empty(),
                "rust-analyzer returned no semantic tokens"
            );

            // Completion: the request must succeed; rust-analyzer may return
            // empty items for a bare temp file without a crate context.
            let completions = client
                .completion("file:///tmp/main.rs", (0, 5))
                .await
                .expect("completion request failed");
            let items = match completions {
                lsp_types::CompletionResponse::Array(items) => items,
                lsp_types::CompletionResponse::List(list) => list.items,
            };
            eprintln!("E2E: completion items = {}", items.len());

            // Hover over the `x` binding (line 1, col 8). A bare temp file
            // may have no crate context, so only the request must succeed.
            let hover = client
                .hover("file:///tmp/main.rs", (1, 8))
                .await
                .expect("hover request failed");
            eprintln!("E2E: hover = {}", hover.is_some());

            // Definition of `main` (line 0, col 3); same context caveat.
            let defs = client
                .definition("file:///tmp/main.rs", (0, 3))
                .await
                .expect("definition request failed");
            eprintln!("E2E: definitions = {}", defs.len());

            client.shutdown().await;
            read_task.abort();
        });
    }
}

