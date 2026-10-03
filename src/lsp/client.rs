//! LSP 客户端：`initialize` 握手 + 文档同步 + `textDocument/*` 请求。
//!
//! 每个服务器进程一个实例，由 [`crate::lsp::project::LspManager`] 按
//! 「项目根 + 语言」复用。客户端本身不含 UI 状态，可以被多个标签共享。
//!
//! 返回值统一是 `lsp_types` 的类型；`params` 直接传 `&Value`，
//! 新增一个方法不需要动 [`crate::lsp::jsonrpc`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use async_channel::Receiver;
use lsp_types::{
    ClientCapabilities, CodeActionClientCapabilities, CodeActionContext, CodeActionKind,
    CodeActionKindLiteralSupport, CodeActionLiteralSupport, CodeActionParams, CodeActionResponse,
    CompletionItem, CompletionParams, DocumentHighlight, DocumentHighlightParams,
    DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeParams, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, InlayHint, InlayHintParams, InitializeParams,
    InitializeResult, Location, PartialResultParams, Position, PublishDiagnosticsParams, Range,
    ReferenceContext, ReferenceParams, RenameParams, DocumentFormattingParams, SelectionRangeParams,
    SemanticTokensParams, SignatureHelp, SignatureHelpParams, TextDocumentIdentifier,
    TextDocumentPositionParams, TextEdit, Url, WorkDoneProgressParams, WorkspaceEdit,
};

use crate::lsp::jsonrpc::{Connection, Message};

/// 一次 `initialize` 握手的结果。
pub struct LspClient {
    conn: Arc<Connection>,
    /// 服务器能力快照。请求前先查这里，避免对不支持的方法发请求
    /// （有些服务器会对不支持的方法返回错误而不是忽略）。
    capabilities: Arc<InitializeResult>,
    /// 项目根，用于 `rootUri`。
    root: PathBuf,
    /// 服务器声明的文档同步方式：全量或增量。
    sync_kind: lsp_types::TextDocumentSyncKind,
}

impl LspClient {
    /// 启动服务器并完成 `initialize` / `initialized` 握手。
    pub async fn start(
        program: &str,
        args: &[String],
        root: PathBuf,
    ) -> Result<Self> {
        let conn = Connection::spawn(program, args).await?;
        let conn = Arc::new(conn);

        let root_uri = path_to_uri(&root)?;
        #[allow(deprecated)]
        // `root_uri` 被标为 deprecated（推荐 `workspace_folders`），但大量
        // 现役服务器（gopls 的旧版本、clangd、jdtls）仍只读它——
        // 不给的话它们会退化成「无工作区」而拒绝分析。两个都给。
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri.clone()),
            // 部分旧服务器只读 `rootPath`。
            #[allow(deprecated)]
            root_path: Some(root.to_string_lossy().to_string()),
            workspace_folders: None,
            capabilities: client_capabilities(),
            initialization_options: None,
            client_info: None,
            locale: None,
            trace: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let params = serde_json::to_value(&params).context("serialize InitializeParams")?;

        let result: InitializeResult = serde_json::from_value(
            conn.request("initialize", Some(&params)).await?
        )
        .context("parse InitializeResult")?;

        // `initialized` 之后服务器才会开始发诊断等通知。
        conn.notify("initialized", Some(&serde_json::json!({}))).await?;

        let sync_kind = result
            .capabilities
            .text_document_sync
            .as_ref()
            .map(|s| match s {
                lsp_types::TextDocumentSyncCapability::Kind(k) => *k,
                lsp_types::TextDocumentSyncCapability::Options(o) => o
                    .change
                    .unwrap_or(lsp_types::TextDocumentSyncKind::FULL),
            })
            .unwrap_or(lsp_types::TextDocumentSyncKind::FULL);

        Ok(Self {
            conn,
            capabilities: Arc::new(result),
            root,
            sync_kind,
        })
    }

    /// 服务器能力快照（`textDocument/*` 的开关查询入口）。
    pub fn capabilities(&self) -> &InitializeResult {
        &self.capabilities
    }

    /// 服务器声明的补全触发字符（`completionProvider.triggerCharacters`）。
    ///
    /// 对照 Zed 的 `lsp_store.rs`：它从
    /// `server.capabilities().completion_provider.trigger_characters` 取，
    /// 存进 `completion_triggers_per_language_server` 用于触发补全。
    ///
    /// 典型值：Rust 的 `:` / `.`、CSS 的 `:` / `@`、HTML 的 `<`。
    /// 之前完全不读这个字段，只按「标识符字符」触发，于是 `:`、`@`、`<`
    /// 这些**服务器明确要求的**触发点永远不会请求补全。
    pub fn completion_trigger_characters(&self) -> Vec<String> {
        trigger_characters_of(&self.capabilities)
    }

    /// 项目根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 是否已断开。
    pub fn is_closed(&self) -> bool {
        self.conn.is_closed()
    }

    /// 服务器主动通知的接收端（诊断等）。
    pub fn notifications(&self) -> Receiver<Message> {
        self.conn.notifications.clone()
    }

    // ---------------------------------------------------------------- 文档同步

    /// `textDocument/didOpen`。
    pub async fn did_open(
        &self,
        uri: &Url,
        language_id: &str,
        version: i32,
        text: &str,
    ) -> Result<()> {
        let params = lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.to_string(),
                version,
                text: text.to_string(),
            },
        };
        let value = serde_json::to_value(&params)?;
        self.conn.notify("textDocument/didOpen", Some(&value)).await
    }

    /// `textDocument/didChange`。
    ///
    /// 服务器声明增量同步时发增量数组；否则发全量快照。全量更稳妥
    /// （不依赖旧文本的一致性），所以未声明时按全量走。
    pub async fn did_change(
        &self,
        uri: &Url,
        version: i32,
        full_text: &str,
        changes: &[TextEdit],
    ) -> Result<()> {
        let content_changes = if self.sync_kind == lsp_types::TextDocumentSyncKind::INCREMENTAL
            && !changes.is_empty()
        {
            changes
                .iter()
                .map(|e| lsp_types::TextDocumentContentChangeEvent {
                    range: Some(e.range),
                    range_length: None,
                    text: e.new_text.clone(),
                })
                .collect()
        } else {
            // 全量：`range` 为 `None` 表示替换整篇文本。
            vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: full_text.to_string(),
            }]
        };

        let params = lsp_types::DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version,
            },
            content_changes,
        };
        let value = serde_json::to_value(&params)?;
        self.conn
            .notify("textDocument/didChange", Some(&value))
            .await
    }

    /// `textDocument/didSave`。
    pub async fn did_save(&self, uri: &Url, text: &str) -> Result<()> {
        let params = lsp_types::DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            text: Some(text.to_string()),
        };
        let value = serde_json::to_value(&params)?;
        self.conn.notify("textDocument/didSave", Some(&value)).await
    }

    /// `textDocument/didClose`。
    pub async fn did_close(&self, uri: &Url) -> Result<()> {
        let params = lsp_types::DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
        };
        let value = serde_json::to_value(&params)?;
        self.conn.notify("textDocument/didClose", Some(&value)).await
    }

    // ---------------------------------------------------------------- 查询请求

    /// `textDocument/hover`。
    pub async fn hover(&self, uri: &Url, position: Position) -> Result<Option<Hover>> {
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self.conn.request("textDocument/hover", Some(&value)).await?;
        // 无 hover 时服务器返回 `null`，而不是空对象。
        if result.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(result)?)
    }

    /// `textDocument/completion`。
    ///
    /// 返回 `CompletionItem[]` 或 `CompletionList`；`isIncomplete` 为 true 时
    /// 调用方应在用户继续输入后重新请求。
    pub async fn completion(
        &self,
        uri: &Url,
        position: Position,
        trigger_character: Option<String>,
    ) -> Result<(Vec<CompletionItem>, bool)> {
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            context: Some(lsp_types::CompletionContext {
                trigger_kind: if trigger_character.is_some() {
                    lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER
                } else {
                    lsp_types::CompletionTriggerKind::INVOKED
                },
                trigger_character,
            }),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/completion", Some(&value))
            .await?;

        if result.is_null() {
            return Ok((Vec::new(), false));
        }
        // 两种合法形态：数组，或 `{isIncomplete, items}`。
        if let Ok(list) = serde_json::from_value::<lsp_types::CompletionList>(result.clone()) {
            // 0.95 的 `is_incomplete` 是 `bool`（不是 `Option<bool>`）。
            return Ok((list.items, list.is_incomplete));
        }
        let items: Vec<CompletionItem> = serde_json::from_value(result)?;
        Ok((items, false))
    }

    /// `completionItem/resolve`：补全选中后再拉取详情（文档、额外文本编辑）。
    pub async fn resolve_completion(&self, mut item: CompletionItem) -> Result<CompletionItem> {
        let value = serde_json::to_value(&item)?;
        let result = self
            .conn
            .request("completionItem/resolve", Some(&value))
            .await?;
        item = serde_json::from_value(result)?;
        Ok(item)
    }

    /// `textDocument/definition`。
    pub async fn goto_definition(
        &self,
        uri: &Url,
        position: Position,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/definition", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(result)?)
    }

    /// `textDocument/references`。
    pub async fn references(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            context: ReferenceContext {
                include_declaration,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/references", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// `textDocument/documentSymbol`。
    pub async fn document_symbols(&self, uri: &Url) -> Result<DocumentSymbolResponse> {
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/documentSymbol", Some(&value))
            .await?;
        Ok(serde_json::from_value(result).unwrap_or(DocumentSymbolResponse::Flat(Vec::new())))
    }

    /// `textDocument/rename`：返回跨文件的 `WorkspaceEdit`。
    pub async fn rename(
        &self,
        uri: &Url,
        position: Position,
        new_name: &str,
    ) -> Result<Option<WorkspaceEdit>> {
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            new_name: new_name.to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self.conn.request("textDocument/rename", Some(&value)).await?;
        if result.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(result)?)
    }

    /// `textDocument/formatting`。
    pub async fn formatting(&self, uri: &Url, tab_size: u32, insert_spaces: bool) -> Result<Vec<TextEdit>> {
        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            options: lsp_types::FormattingOptions {
                tab_size,
                insert_spaces,
                insert_final_newline: None,
                trim_final_newlines: None,
                trim_trailing_whitespace: None,
                properties: Default::default(),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/formatting", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// `textDocument/signatureHelp`。
    pub async fn signature_help(
        &self,
        uri: &Url,
        position: Position,
    ) -> Result<Option<SignatureHelp>> {
        let params = SignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            context: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/signatureHelp", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(result)?)
    }

    /// `textDocument/codeAction`。
    pub async fn code_actions(
        &self,
        uri: &Url,
        range: Range,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> Result<CodeActionResponse> {
        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range,
            context: CodeActionContext {
                diagnostics,
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/codeAction", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// `textDocument/documentHighlight`：光标处符号的所有出现位置。
    pub async fn document_highlight(
        &self,
        uri: &Url,
        position: Position,
    ) -> Result<Vec<DocumentHighlight>> {
        let params = DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/documentHighlight", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// `textDocument/foldingRange`。
    pub async fn folding_ranges(&self, uri: &Url) -> Result<Vec<FoldingRange>> {
        let params = FoldingRangeParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/foldingRange", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// `textDocument/inlayHint`。
    pub async fn inlay_hints(&self, uri: &Url, range: Range) -> Result<Vec<InlayHint>> {
        let params = InlayHintParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/inlayHint", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// `textDocument/selectionRange`：扩展/收缩选区的层级。
    pub async fn selection_ranges(
        &self,
        uri: &Url,
        positions: Vec<Position>,
    ) -> Result<Vec<lsp_types::SelectionRange>> {
        let params = SelectionRangeParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            positions,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/selectionRange", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// `textDocument/semanticTokens/full`：语义高亮 token 的扁平整数数组。
    ///
    /// 返回原始数组（每 5 个整数一个 token），由
    /// [`crate::lsp::features`] 解码成高亮区间。
    pub async fn semantic_tokens(&self, uri: &Url) -> Result<lsp_types::SemanticTokens> {
        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(&params)?;
        let result = self
            .conn
            .request("textDocument/semanticTokens/full", Some(&value))
            .await?;
        if result.is_null() {
            return Ok(lsp_types::SemanticTokens {
                result_id: None,
                data: Vec::new(),
            });
        }
        Ok(serde_json::from_value(result)?)
    }

    /// 关闭连接：执行 LSP 关闭握手（`shutdown` → `exit`）并回收子进程。
    ///
    /// 这是应用退出（`LspManager::shutdown_all`）与标签关闭时走的入口。
    pub fn shutdown(&self) {
        self.conn.shutdown();
    }

    /// 服务器子进程 pid（测试用来确认退出后没有孤儿进程）。
    pub fn connection_pid(&self) -> Option<u32> {
        self.conn.child_pid()
    }
}

/// 我们声明的客户端能力。
///
/// 只声明真正实现的，避免服务器发来我们不会处理的请求。
/// `text_document` 里声明了后续要用的全部方法。
/// 从 `InitializeResult` 取服务器的补全触发字符。
///
/// 抽成自由函数便于单测：这是唯一读取该能力字段的地方，测试可以直接喂
/// 真实的 `InitializeResult`（由 JSON 反序列化而来）验证取值路径。
fn trigger_characters_of(result: &InitializeResult) -> Vec<String> {
    result
        .capabilities
        .completion_provider
        .as_ref()
        .and_then(|provider| provider.trigger_characters.as_ref())
        .map(|chars| chars.iter().cloned().collect())
        .unwrap_or_default()
}

fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(lsp_types::WorkspaceClientCapabilities {
            // 接受 `workspace/applyEdit`：重命名/格式化跨文件时服务器可能
            // 直接下发编辑而不走响应体。
            apply_edit: Some(true),
            workspace_folders: Some(true),
            ..Default::default()
        }),
        text_document: Some(lsp_types::TextDocumentClientCapabilities {
            synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(true),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                did_save: Some(true),
            }),
            completion: Some(lsp_types::CompletionClientCapabilities {
                completion_item: Some(lsp_types::CompletionItemCapability {
                    snippet_support: Some(false),
                    documentation_format: Some(vec![
                        lsp_types::MarkupKind::Markdown,
                        lsp_types::MarkupKind::PlainText,
                    ]),
                    resolve_support: Some(lsp_types::CompletionItemCapabilityResolveSupport {
                        properties: vec![
                            "documentation".to_string(),
                            "detail".to_string(),
                            "additionalTextEdits".to_string(),
                        ],
                    }),
                    ..Default::default()
                }),
                completion_list: None,
                context_support: Some(true),
                ..Default::default()
            }),
            hover: Some(lsp_types::HoverClientCapabilities {
                content_format: Some(vec![
                    lsp_types::MarkupKind::Markdown,
                    lsp_types::MarkupKind::PlainText,
                ]),
                dynamic_registration: Some(true),
            }),
            signature_help: Some(lsp_types::SignatureHelpClientCapabilities {
                signature_information: Some(lsp_types::SignatureInformationSettings {
                    documentation_format: Some(vec![
                        lsp_types::MarkupKind::Markdown,
                        lsp_types::MarkupKind::PlainText,
                    ]),
                    parameter_information: Some(lsp_types::ParameterInformationSettings {
                        label_offset_support: Some(true),
                    }),
                    active_parameter_support: Some(true),
                }),
                ..Default::default()
            }),
            definition: Some(lsp_types::GotoCapability {
                dynamic_registration: Some(true),
                link_support: Some(true),
            }),
            references: Some(lsp_types::ReferenceClientCapabilities {
                dynamic_registration: Some(true),
            }),
            document_highlight: Some(lsp_types::DocumentHighlightClientCapabilities {
                dynamic_registration: Some(true),
            }),
            document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            code_action: Some(CodeActionClientCapabilities {
                code_action_literal_support: Some(CodeActionLiteralSupport {
                    code_action_kind: CodeActionKindLiteralSupport {
                        value_set: vec![
                            CodeActionKind::EMPTY.as_str().to_string(),
                            CodeActionKind::QUICKFIX.as_str().to_string(),
                            CodeActionKind::REFACTOR.as_str().to_string(),
                            CodeActionKind::REFACTOR_EXTRACT.as_str().to_string(),
                            CodeActionKind::REFACTOR_INLINE.as_str().to_string(),
                            CodeActionKind::REFACTOR_REWRITE.as_str().to_string(),
                            CodeActionKind::SOURCE.as_str().to_string(),
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS
                                .as_str()
                                .to_string(),
                        ],
                    },
                }),
                ..Default::default()
            }),
            formatting: Some(lsp_types::DocumentFormattingClientCapabilities {
                dynamic_registration: Some(true),
            }),
            rename: Some(lsp_types::RenameClientCapabilities {
                prepare_support: Some(true),
                ..Default::default()
            }),
            publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                // 相关诊断信息（如 note 级补充说明）我们渲染进 hover。
                related_information: Some(true),
                version_support: Some(true),
                ..Default::default()
            }),
            folding_range: Some(lsp_types::FoldingRangeClientCapabilities {
                dynamic_registration: Some(true),
                line_folding_only: Some(false),
                ..Default::default()
            }),
            inlay_hint: Some(lsp_types::InlayHintClientCapabilities {
                resolve_support: None,
                dynamic_registration: Some(true),
            }),
            semantic_tokens: Some(lsp_types::SemanticTokensClientCapabilities {
                dynamic_registration: Some(true),
                requests: lsp_types::SemanticTokensClientCapabilitiesRequests {
                    range: Some(false),
                    full: Some(lsp_types::SemanticTokensFullOptions::Bool(true)),
                },
                // 声明 token 类型与修饰符，服务器才能返回有意义的索引。
                token_types: vec![
                    lsp_types::SemanticTokenType::NAMESPACE,
                    lsp_types::SemanticTokenType::TYPE,
                    lsp_types::SemanticTokenType::CLASS,
                    lsp_types::SemanticTokenType::ENUM,
                    lsp_types::SemanticTokenType::INTERFACE,
                    lsp_types::SemanticTokenType::STRUCT,
                    lsp_types::SemanticTokenType::TYPE_PARAMETER,
                    lsp_types::SemanticTokenType::PARAMETER,
                    lsp_types::SemanticTokenType::VARIABLE,
                    lsp_types::SemanticTokenType::PROPERTY,
                    lsp_types::SemanticTokenType::FUNCTION,
                    lsp_types::SemanticTokenType::METHOD,
                    lsp_types::SemanticTokenType::MACRO,
                    lsp_types::SemanticTokenType::KEYWORD,
                    lsp_types::SemanticTokenType::COMMENT,
                    lsp_types::SemanticTokenType::STRING,
                    lsp_types::SemanticTokenType::NUMBER,
                    lsp_types::SemanticTokenType::OPERATOR,
                ],
                token_modifiers: vec![
                    lsp_types::SemanticTokenModifier::DECLARATION,
                    lsp_types::SemanticTokenModifier::DEFINITION,
                    lsp_types::SemanticTokenModifier::READONLY,
                    lsp_types::SemanticTokenModifier::STATIC,
                    lsp_types::SemanticTokenModifier::DEPRECATED,
                    lsp_types::SemanticTokenModifier::ABSTRACT,
                    lsp_types::SemanticTokenModifier::ASYNC,
                    lsp_types::SemanticTokenModifier::MODIFICATION,
                    lsp_types::SemanticTokenModifier::DOCUMENTATION,
                    lsp_types::SemanticTokenModifier::DEFAULT_LIBRARY,
                ],
                formats: vec![lsp_types::TokenFormat::RELATIVE],
                overlapping_token_support: Some(false),
                multiline_token_support: Some(false),
                server_cancel_support: Some(false),
                augments_syntax_tokens: Some(true),
            }),
            selection_range: Some(lsp_types::SelectionRangeClientCapabilities {
                dynamic_registration: Some(true),
            }),
            ..Default::default()
        }),
        window: Some(lsp_types::WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        general: Some(lsp_types::GeneralClientCapabilities {
            // **只声明 UTF-16**（对照 Zed 的 `lsp.rs`，它同样只列
            // `PositionEncodingKind::UTF16`）。
            //
            // 这里不能顺手把 UTF-8 也列上：LSP 规范规定服务器从客户端给的
            // 列表里挑一个（优先第一个它支持的），而我们的位置换算全部按
            // UTF-16 code unit 实现（`RopeExt::offset_to_position`）。声明了
            // UTF-8 就等于允许服务器按**字节列**回位置，那会让含多字节字符的
            // 文件里所有位置整体错位，且错得很隐蔽（BMP 内看不出来）。
            position_encodings: Some(vec![lsp_types::PositionEncodingKind::UTF16]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// 路径 → `file://` URI。
///
/// 语言服务器只认 URI。相对路径先转绝对，否则 `Url::from_file_path`
/// 会失败。
pub fn path_to_uri(path: &Path) -> Result<Url> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("cannot resolve relative path without cwd")?
            .join(path)
    };
    Url::from_file_path(&abs).map_err(|_| anyhow::anyhow!("invalid path for URI: {}", abs.display()))
}

/// `file://` URI → 路径。
pub fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

/// 从通知里取出 `textDocument/publishDiagnostics` 参数。
pub fn as_publish_diagnostics(params: &serde_json::Value) -> Option<PublishDiagnosticsParams> {
    serde_json::from_value(params.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只声明 UTF-16 位置编码。
    ///
    /// 回归：曾同时声明 UTF-8。LSP 规范允许服务器从客户端给的列表里挑一个，
    /// 而我们的位置换算全部按 UTF-16 code unit 实现——服务器挑了 UTF-8 就会
    /// 让含多字节字符的文件里所有位置整体错位，且 BMP 内看不出来。
    /// 对照 Zed 的 `lsp.rs`：它同样只列 `PositionEncodingKind::UTF16`。
    #[test]
    fn advertises_only_utf16_position_encoding() {
        let caps = client_capabilities();
        let encodings = caps
            .general
            .as_ref()
            .and_then(|g| g.position_encodings.clone())
            .expect("must declare position encodings");
        assert_eq!(
            encodings,
            vec![lsp_types::PositionEncodingKind::UTF16],
            "must advertise exactly UTF-16; adding UTF-8 would let the server \
             pick byte-column semantics our conversions do not implement"
        );
    }

    /// 补全能力里声明 `resolveSupport`（选中后再拉详情）与文档格式。
    #[test]
    fn declares_completion_item_resolve_support() {
        let caps = client_capabilities();
        let item = caps
            .text_document
            .as_ref()
            .and_then(|t| t.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .expect("completion_item capabilities");
        assert!(
            item.resolve_support.is_some(),
            "we call completionItem/resolve, so it must be advertised"
        );
    }

    /// 触发字符取自服务器的 `completionProvider.triggerCharacters`。
    ///
    /// 用真实 `InitializeResult` 反序列化构造（不是手搓字段），确保读的就是
    /// 服务器实际返回的那个位置——对照 Zed 的
    /// `server.capabilities().completion_provider.trigger_characters`。
    #[test]
    fn reads_trigger_characters_from_server_capabilities() {
        let result: InitializeResult = serde_json::from_value(serde_json::json!({
            "capabilities": {
                "completionProvider": {
                    "triggerCharacters": [":", ".", "@"]
                }
            }
        }))
        .expect("InitializeResult parses");

        let chars = trigger_characters_of(&result);
        assert_eq!(chars, vec![":".to_string(), ".".to_string(), "@".to_string()]);
    }

    /// 服务器没声明触发字符时返回空（不能因此 panic 或瞎触发）。
    #[test]
    fn missing_trigger_characters_yields_empty() {
        let result: InitializeResult = serde_json::from_value(serde_json::json!({
            "capabilities": {}
        }))
        .expect("InitializeResult parses");
        assert!(trigger_characters_of(&result).is_empty());
    }
}
