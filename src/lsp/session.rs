//! 每个编辑器标签的 LSP 会话。
//!
//! 一个 `LspSession` 对应一个打开的本地文件：它负责文档同步
//! （didOpen/didChange/didSave/didClose）、把服务器推来的诊断写进
//! `DiagnosticSet`、以及发起 hover / 补全 / 跳转等请求。
//!
//! 挂在 `Editor` 上而不是 `InputState` 上：请求的发起与结果的落地都需要
//! `Window`（聚焦、滚动、弹层），而 `InputState` 的编辑路径只有
//! `Context<InputState>`。`Editor` 侧订阅 `InputEvent::Change` 驱动同步。
//!
//! **只服务本地文件**：远程（SSH）标签不创建会话。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Entity, Task, WeakEntity};
use lsp_types::{Position, Url};
use ropey::Rope;

use crate::components::input::InputState;
use crate::lsp::client::LspClient;
use crate::lsp::features;
use crate::lsp::jsonrpc::Message;
use crate::lsp::project::LspManager;

/// didChange 的防抖窗口。
///
/// 与 Zed 同量级：太短会让服务器在打字过程中反复重解析（rust-analyzer
/// 尤其昂贵），太长则诊断滞后明显。
const CHANGE_DEBOUNCE_MS: u64 = 150;

/// 一个标签的 LSP 会话。
pub struct LspSession {
    /// 文件绝对路径。None 表示不是本地文件（不参与 LSP）。
    path: PathBuf,
    uri: Url,
    language_id: String,
    /// 文档版本号，每次 didChange 递增。
    /// 已**送达**服务器的文档版本。
    ///
    /// 用原子是因为防抖任务在后台跑，发送成功后要回写这个值
    /// （对照 Zed：版本号随实际发出的 `didChangeTextDocument` 递增）。
    version: std::sync::Arc<std::sync::atomic::AtomicI32>,
    /// 已连接的服务器；`None` 表示尚未就绪或启动失败。
    client: Option<Arc<LspClient>>,
    /// 发送 didChange 的防抖任务。新编辑到来时替换旧任务即取消它。
    change_task: Option<Task<()>>,
    /// 监听服务器通知（诊断）的任务。
    notify_task: Option<Task<()>>,
    /// 目标编辑器句柄。诊断监听用 `spawn_diagnostic_listener` 传入的实体，
    /// 这里保留一份供将来的重连用（服务器崩溃后重建监听）。
    editor: WeakEntity<InputState>,
    /// 该会话是否已发送过 didOpen。
    opened: bool,
}

impl LspSession {
    /// 为一个本地文件创建会话；远程文件返回 `None`。
    pub fn new(
        path: &Path,
        language_id: &str,
        editor: WeakEntity<InputState>,
    ) -> Option<Self> {
        let uri = crate::lsp::client::path_to_uri(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            uri,
            language_id: language_id.to_string(),
            version: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)),
            client: None,
            change_task: None,
            notify_task: None,
            editor,
            opened: false,
        })
    }

    /// 已送达服务器的文档版本（测试观测点）。
    ///
    /// 对照 Zed：这个值应等于「实际发出过多少次 `didChangeTextDocument`」，
    /// 而不是「用户敲了多少次键」。
    pub fn sent_version(&self) -> i32 {
        self.version.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 是否已连上服务器。
    pub fn is_active(&self) -> bool {
        self.client.as_ref().is_some_and(|c| !c.is_closed())
    }

    /// 服务器句柄（供发起请求）。
    pub fn client(&self) -> Option<&Arc<LspClient>> {
        self.client.as_ref().filter(|c| !c.is_closed())
    }

    /// 文档高亮：光标处符号的所有出现位置（字节区间）。
    ///
    /// 需要当前文本来把 LSP 的 (line, character) 换算成字节偏移，所以由
    /// 调用方传入——会话自己不持有文本（编辑器才是文本的唯一真相源）。
    pub async fn document_highlight(
        &self,
        text: &Rope,
        position: lsp_types::Position,
    ) -> Result<Vec<std::ops::Range<usize>>> {
        match self.client() {
            Some(client) => {
                let highlights = client.document_highlight(&self.uri, position).await?;
                Ok(features::to_highlight_ranges(text, highlights))
            }
            None => Ok(Vec::new()),
        }
    }

    /// 折叠范围。
    pub async fn folding_ranges(&self) -> Result<Vec<features::FoldRange>> {
        match self.client() {
            Some(client) => Ok(features::to_fold_ranges(
                client.folding_ranges(&self.uri).await?,
            )),
            None => Ok(Vec::new()),
        }
    }

    /// Inlay Hint（可见区间的）。
    pub async fn inlay_hints(&self, range: lsp_types::Range) -> Result<Vec<lsp_types::InlayHint>> {
        match self.client() {
            Some(client) => client.inlay_hints(&self.uri, range).await,
            None => Ok(Vec::new()),
        }
    }

    /// 签名帮助。
    pub async fn signature_help(
        &self,
        position: lsp_types::Position,
    ) -> Result<Option<lsp_types::SignatureHelp>> {
        match self.client() {
            Some(client) => client.signature_help(&self.uri, position).await,
            None => Ok(None),
        }
    }

    /// Code Action。
    pub async fn code_actions(
        &self,
        range: lsp_types::Range,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> Result<lsp_types::CodeActionResponse> {
        match self.client() {
            Some(client) => client.code_actions(&self.uri, range, diagnostics).await,
            None => Ok(Vec::new()),
        }
    }

    /// 语义 token（原始扁平数组）。
    pub async fn semantic_tokens(&self) -> Result<lsp_types::SemanticTokens> {
        match self.client() {
            Some(client) => client.semantic_tokens(&self.uri).await,
            None => Ok(lsp_types::SemanticTokens {
                result_id: None,
                data: Vec::new(),
            }),
        }
    }

    pub fn uri(&self) -> &Url {
        &self.uri
    }

    /// 目标编辑器句柄。
    ///
    /// 诊断监听建立时用它取回实体；服务器崩溃后重连也走它。
    pub fn editor(&self) -> &WeakEntity<InputState> {
        &self.editor
    }

    /// 服务器进程是否已死（连上过但后来崩了）。
    ///
    /// 用于决定是否重连：`client` 为 `None` 表示从未连上（或已禁用），
    /// 与本方法区分开，避免对「不支持的语言」反复重试。
    pub fn is_dead(&self) -> bool {
        self.client.as_ref().is_some_and(|c| c.is_closed())
    }

    pub fn language_id(&self) -> &str {
        &self.language_id
    }

    /// 连接服务器：必要时启动进程，然后 didOpen 并订阅诊断。
    ///
    /// 可以重复调用（例如服务器崩溃后重连）；已连接时是空操作。
    pub fn connect(
        &mut self,
        cx: &mut App,
    ) -> Option<async_channel::Receiver<Option<Arc<LspClient>>>> {
        if self.is_active() {
            return None;
        }
        let rx = LspManager::server_for_file(cx, &self.path, &self.language_id)?;
        Some(rx)
    }

    /// 服务器就绪后的收尾：didOpen + 订阅诊断通知。
    ///
    /// 状态由调用方置为 `Ready`（`InputState` 是状态的唯一真相源：状态栏
    /// 渲染与进度监听都从它读写，两处存会漂移）。
    pub fn on_connected(&mut self, client: Arc<LspClient>, text: &Rope) {
        self.client = Some(client.clone());
        self.opened = true;
        self.version.store(0, std::sync::atomic::Ordering::SeqCst);

        let uri = self.uri.clone();
        let language_id = self.language_id.clone();
        let text_str = text.to_string();
        // didOpen 必须在后台发：它在 await 期间不能占住 UI 线程。
        smol::spawn(async move {
            if let Err(e) = client
                .did_open(&uri, &language_id, 0, &text_str)
                .await
            {
                log::warn!("lsp didOpen failed for {uri}: {e}");
            }
        })
        .detach();
    }

    /// 文本变化：防抖后发 didChange。
    ///
    /// 只在已连接时生效；未连接时静默丢弃（用户仍然能正常编辑）。
    pub fn on_text_changed(&mut self, text: &Rope, cx: &mut App) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if client.is_closed() {
            return;
        }
        let uri = self.uri.clone();
        let text_str = text.to_string();

        // **版本号在真正发送时递增，而不是每次击键**。
        //
        // 对照 Zed 的 `lsp_store.rs`：它在发出 `didChangeTextDocument` 的那一
        // 刻算 `next_version = previous_snapshot.version + 1`，让版本号与
        // 「服务器实际收到过多少版本文档」一一对应。
        //
        // 此前每次击键都 `+= 1`，而防抖会取消绝大多数任务——于是版本号远
        // 远跑在服务器已收到的版本前面。而 `publishDiagnostics` 的版本门控
        // 拿它当基准比较，服务器的诊断版本永远追不上，**所有诊断都会被当成
        // 「过期」丢弃**（用户看到的就是「没有诊断」）。
        let version_slot = self.version.clone();
        let base_version = version_slot.load(std::sync::atomic::Ordering::SeqCst);
        self.change_task.take();
        self.change_task = Some(cx.spawn(async move |_cx| {
            // 用 smol 的定时器：gpui 的 background_executor 在这里拿不到
            // （`cx` 是 `WeakEntity` 上下文，没有 executor 句柄）。
            smol::Timer::after(std::time::Duration::from_millis(CHANGE_DEBOUNCE_MS)).await;
            // 发送那一刻才定版本：`base + 1`。取消掉的任务不会推进版本。
            let version = base_version + 1;
            // 全量同步：不构造增量 TextEdit，避免与服务器文本漂移。
            if let Err(e) = client.did_change(&uri, version, &text_str, &[]).await {
                log::debug!("lsp didChange failed for {uri}: {e}");
            } else {
                // 记录已送达的版本，供下一次递增取基准。
                version_slot.fetch_max(version, std::sync::atomic::Ordering::SeqCst);
            }
        }));
    }

    /// 保存后通知服务器。
    pub fn on_saved(&self, text: &Rope) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if client.is_closed() {
            return;
        }
        let uri = self.uri.clone();
        let text_str = text.to_string();
        smol::spawn(async move {
            if let Err(e) = client.did_save(&uri, &text_str).await {
                log::debug!("lsp didSave failed for {uri}: {e}");
            }
        })
        .detach();
    }

    /// 关闭标签：发 didClose 并停掉所有任务。
    pub fn close(&mut self) {
        self.change_task.take();
        self.notify_task.take();
        if let Some(client) = self.client.take() {
            let uri = self.uri.clone();
            smol::spawn(async move {
                let _ = client.did_close(&uri).await;
            })
            .detach();
        }
        self.opened = false;
    }

    /// 订阅服务器的 `publishDiagnostics`，写进编辑器的 `DiagnosticSet`。
    ///
    /// 只在**本文件的 uri** 上生效：一个服务器进程服务整个项目，它会推
    /// 所有打开文件的诊断，必须按 uri 过滤，否则 A 文件的诊断会画到 B 上。
    pub fn spawn_diagnostic_listener(
        &mut self,
        editor: &Entity<InputState>,
        cx: &mut App,
    ) {
        let Some(client) = self.client.clone() else {
            log::warn!("lsp: no client, diagnostic listener not started");
            return;
        };
        log::debug!("lsp: starting diagnostic listener for {}", self.uri);
        let weak = editor.downgrade();
        let uri = self.uri.clone();
        let source_name = self.language_id.clone();

        self.notify_task.take();
        self.notify_task = Some(cx.spawn(async move |cx| {
            let rx = client.notifications();
            while let Ok(msg) = rx.recv().await {
                let Message::Notification { method, params } = msg else {
                    continue;
                };

                // `$/progress`：服务器的工作进度（索引、扫描…）→ 状态栏。
                if method == "$/progress" {
                    let parsed = serde_json::from_value::<lsp_types::ProgressParams>(params);
                    let Ok(progress) = parsed else {
                        continue;
                    };
                    let lsp_types::ProgressParamsValue::WorkDone(value) = progress.value;
                    let alive = weak
                        .update(cx, |state: &mut InputState, cx| {
                            state.apply_lsp_progress(&progress.token, &value);
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !alive {
                        break;
                    }
                    continue;
                }

                if method != "textDocument/publishDiagnostics" {
                    continue;
                }
                let Some(publish) = crate::lsp::client::as_publish_diagnostics(&params) else {
                    continue;
                };
                if publish.uri != uri {
                    continue;
                }
                log::debug!(
                    "lsp: publishDiagnostics {} items (version {:?}) for {}",
                    publish.diagnostics.len(),
                    publish.version,
                    uri
                );
                let diagnostics = publish.diagnostics;
                // 版本门控：服务器可能乱序推送，过期的那条不能覆盖新的。
                let version = publish.version;
                let source = source_name.clone();
                // 编辑器已被关闭时 `update` 返回 Err：停止监听，避免任务泄漏。
                let alive = weak
                    .update(cx, |state: &mut InputState, cx| {
                        // 换算与写入都在 `set_diagnostics_versioned` 里完成：
                        // 调用方只有 `&mut self`，无法同时持有 `text()` 的借用。
                        let items = features::to_diagnostics(state.text(), diagnostics);
                        // LSP 诊断是**全量快照**：直接覆盖，不做合并。
                        // 上一轮的 stale 诊断会随本次快照一起消失。
                        let _ = &source;
                        let applied = state.set_diagnostics_versioned(items, version);
                        // 状态栏的「实时状态」：按严重级别统计当前文件。
                        let counts = state
                            .diagnostics()
                            .map(|set| {
                                set.items().iter().fold((0usize, 0usize), |acc, d| {
                                    match d.severity {
                                        crate::editor::syntax::DiagnosticSeverity::Error => {
                                            (acc.0 + 1, acc.1)
                                        }
                                        crate::editor::syntax::DiagnosticSeverity::Warning => {
                                            (acc.0, acc.1 + 1)
                                        }
                                        _ => acc,
                                    }
                                })
                            })
                            .unwrap_or((0, 0));
                        state.set_lsp_diagnostic_counts(counts.0, counts.1);
                        cx.notify();
                        applied
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        // 标签关闭但未显式 `close()`（例如切标签页组被丢弃）：
        // 也要发 didClose，否则服务器一直持有已关闭的文档。
        self.close();
    }
}

// ---------------------------------------------------------------- 请求入口

/// 编辑器里某个偏移对应的 LSP `Position`。
pub fn offset_to_lsp_position(state: &InputState, offset: usize) -> Position {
    features::offset_to_position(state.text(), offset)
}

/// LSP `Position` 对应的编辑器字节偏移。
pub fn lsp_position_to_offset(state: &InputState, position: Position) -> usize {
    features::position_to_offset(state.text(), position)
}

/// 当前光标处的 `Position`。
pub fn cursor_lsp_position(state: &InputState) -> Position {
    offset_to_lsp_position(state, state.cursor())
}

/// 应用一组 LSP `TextEdit`（格式化 / Code Action / 重命名）。
///
/// 只处理**当前文件**的部分：重命名可能跨多个文件，其余文件由调用方决定。
pub fn apply_text_edits(
    state: &mut InputState,
    edits: Vec<lsp_types::TextEdit>,
    cx: &mut gpui::Context<InputState>,
) {
    if edits.is_empty() {
        return;
    }
    let byte_edits = features::to_byte_edits(state.text(), edits);
    // 从后往前应用，避免前面的编辑让后面的偏移失效。
    for edit in byte_edits {
        // 复用了补全的区间替换原语：它已经做了边界校验与撤销记录。
        if !state.apply_completion(edit.range, &edit.new_text, cx) {
            // 区间失效（文本被并发改短）：停止应用后续编辑，
            // 半套格式化比写坏缓冲区好。
            log::warn!("lsp: stale edit range, aborting remaining edits");
            break;
        }
    }
}

/// 文档高亮（`textDocument/documentHighlight`）→ 字节区间列表。
///
/// 用于把光标处符号的所有出现位置高亮出来，与编辑器已有的「同词高亮」
/// 是同一个视觉通道，但来源更准（服务器知道作用域）。
pub fn document_highlight_ranges(
    state: &InputState,
    highlights: Vec<lsp_types::DocumentHighlight>,
) -> Vec<std::ops::Range<usize>> {
    features::to_highlight_ranges(state.text(), highlights)
}

/// 折叠范围（`textDocument/foldingRange`）→ (起始行, 结束行)。
pub fn fold_ranges(ranges: Vec<lsp_types::FoldingRange>) -> Vec<features::FoldRange> {
    features::to_fold_ranges(ranges)
}

/// Inlay Hint（`textDocument/inlayHint`）→ 编辑器用的 hint 列表。
pub fn inlay_hints(
    state: &InputState,
    hints: Vec<lsp_types::InlayHint>,
) -> Vec<features::InlayHintData> {
    features::to_inlay_hints(state.text(), hints)
}

/// 签名帮助的展示文本。
pub fn signature_help_text(help: &lsp_types::SignatureHelp) -> String {
    features::signature_text(help)
}

/// Code Action 的展示标题。
pub fn code_action_title(action: &lsp_types::CodeActionOrCommand) -> Option<String> {
    features::code_action_label(action).map(|(title, _)| title)
}

/// 从会话发起一个请求并等待结果。
///
/// 统一的「拿 client → 发请求 → 后台 await」骨架，避免每个功能各写一遍
/// 错误降级逻辑。
pub fn with_client<F, T, Fut>(
    session: &LspSession,
    cx: &mut App,
    f: F,
) -> Option<async_channel::Receiver<Result<T>>>
where
    F: FnOnce(Arc<LspClient>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let client = session.client()?.clone();
    let (tx, rx) = async_channel::bounded(1);
    smol::spawn(async move {
        let result = f(client).await;
        let _ = tx.send(result).await;
    })
    .detach();
    let _ = cx;
    Some(rx)
}
