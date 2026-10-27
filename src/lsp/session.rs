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
//! **本地与远端都服务**：远程（SSH）标签的服务器跑在那台主机上，
//! stdio 经 SSH channel 桥接（见 [`crate::ssh::lsp_channel`]）。
//! 协议与文档同步逻辑两边完全共用。

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
use crate::lsp::project::{LspManager, ServerKey};

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
    /// 远端主机配置；`None` 表示本地文件。
    ///
    /// 决定两件事：服务器在哪台机器上跑（`LspManager` 据此选本地子进程还是
    /// SSH channel），以及跨文件跳转时目标文件该用哪条打开路径
    /// （`Editor::open_lsp_location`）。
    ssh_config: Option<crate::ssh::SshConnectionConfig>,
    /// 该会话挂的项目根，也就是它用的 `ServerKey.root`。
    ///
    /// `connect` 时定下来并缓存：跳转打开的目标文件（典型是 `.pyi` 存根、
    /// 标准库、第三方包）往往不在当前文件的项目树里，若每个文件都按自己的
    /// 路径重算根，就会在存根目录另起一个昂贵的服务器。`Editor` 把发起跳转
    /// 的会话的根传给目标会话，二者于是共用一个进程。
    server_root: Option<PathBuf>,
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
    /// 为一个文件创建会话。
    ///
    /// `ssh_config` 为 `None` 时是本地文件；`Some` 时是远端主机上的文件，
    /// 服务器将跑在那台机器上（uri 仍是 `file:///remote/path`——
    /// 服务器视角与本地视角在这个 uri 上是一致的）。
    ///
    /// 返回 `None` 表示路径无法构成合法 uri（不该发生，防御性返回）。
    pub fn new(
        path: &Path,
        language_id: &str,
        editor: WeakEntity<InputState>,
        ssh_config: Option<crate::ssh::SshConnectionConfig>,
    ) -> Option<Self> {
        let uri = crate::lsp::client::path_to_uri(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            uri,
            language_id: language_id.to_string(),
            ssh_config,
            server_root: None,
            version: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)),
            client: None,
            change_task: None,
            notify_task: None,
            editor,
            opened: false,
        })
    }

    /// 测试用：替换会话的 uri（模拟「目标就是当前文件」）。
    #[cfg(test)]
    pub fn set_uri_for_test(&mut self, uri: lsp_types::Url) {
        self.uri = uri;
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

    /// 该会话是否服务远端主机上的文件。
    ///
    /// 调用方据此决定：跨文件跳转时目标文件走本地打开还是远端打开，
    /// 以及 `didChangeWatchedFiles` 该匹配哪个后端的事件源。
    pub fn ssh_config(&self) -> Option<&crate::ssh::SshConnectionConfig> {
        self.ssh_config.as_ref()
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

    /// 这个会话挂在**哪个项目根**上（= 它的 `ServerKey.root`）。
    ///
    /// 跨文件跳转要用：服务器回的 `Location` 指的是它自己工作区里的文件，
    /// 所以那个文件属于**这台服务器**，而不属于「目标路径自己最近的带标记
    /// 祖先目录」。少了这个值，从 `List[int]` 跳进 typeshed 的 `typing.pyi`
    /// 会按存根所在目录另起一个服务器（见 `crate::editor::Editor::lsp_jump_roots`）。
    ///
    /// `None` = 还没连过（`connect` 时才知道根）。
    pub fn server_root(&self) -> Option<&std::path::Path> {
        self.server_root.as_deref()
    }

    /// 显式指定这个会话该挂到哪个项目根上（**必须在 `connect` 之前**调）。
    ///
    /// 只有一条路会用到：文件是由 LSP 跳转打开的，发起跳转的会话已经知道自己
    /// 的根。不指定时 `connect` 按文件路径自己算（那是打开项目内文件的常规路径）。
    pub fn set_server_root(&mut self, root: std::path::PathBuf) {
        self.server_root = Some(root);
    }

    /// 连接服务器：必要时启动进程，然后 didOpen 并订阅诊断。
    ///
    /// 可以重复调用（例如服务器崩溃后重连）；已连接时是空操作。
    ///
    /// 远端会话把 `ssh_config` 一起交给管理器：服务器要跑在那台主机上，
    /// 且「同路径不同主机」必须是两个独立的键（否则两个进程互相顶掉）。
    pub fn connect(
        &mut self,
        cx: &mut App,
    ) -> Option<async_channel::Receiver<Option<Arc<LspClient>>>> {
        if self.is_active() {
            return None;
        }
        // 根要么已被调用方指定（跳转打开的文件），要么按这个文件自己算。
        // 两条都在这里定下来并**存进会话**：菜单与跳转之后的归属判定都读它。
        let key = server_key_for(
            self.server_root.as_deref(),
            &self.path,
            &self.language_id,
            self.ssh_config.clone(),
        );
        self.server_root = Some(key.root.clone());
        let rx = LspManager::server_for_key(cx, &key)?;
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
            if let Err(e) = client.did_open(&uri, &language_id, 0, &text_str).await {
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

        // **版本号在真正发送时原子递增，而不是每次击键**。
        //
        // 对照 Zed 的 `lsp_store.rs`：它在发出 `didChangeTextDocument` 的那一
        // 刻算 `next_version = previous_snapshot.version + 1`，让版本号与
        // 「服务器实际收到过多少版本文档」一一对应。
        //
        // 此前每次击键都 `+= 1`，而防抖会取消绝大多数任务——于是版本号远
        // 远跑在服务器已收到的版本前面。而 `publishDiagnostics` 的版本门控
        // 拿它当基准比较，服务器的诊断版本永远追不上，**所有诊断都会被当成
        // 「过期」丢弃**（用户看到的就是「没有诊断」）。
        //
        // 仍不能在调度时读取 base：前一次 didChange 可能正在发送、尚未回写，
        // 两个任务会拿到同一个 base，产生重复版本。改为发送前 `fetch_add`，
        // 由原子变量唯一分配。失败时留下版本空洞是允许的（版本只需递增）。
        let version_slot = self.version.clone();
        self.change_task.take();
        self.change_task = Some(cx.spawn(async move |_cx| {
            // 用 smol 的定时器：gpui 的 background_executor 在这里拿不到
            // （`cx` 是 `WeakEntity` 上下文，没有 executor 句柄）。
            smol::Timer::after(std::time::Duration::from_millis(CHANGE_DEBOUNCE_MS)).await;
            // 发送那一刻才分配版本：取消掉的任务不会推进版本。
            let version = version_slot.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            // 全量同步：不构造增量 TextEdit，避免与服务器文本漂移。
            if let Err(e) = client.did_change(&uri, version, &text_str, &[]).await {
                log::debug!("lsp didChange failed for {uri}: {e}");
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

    /// 用户主动停止服务器：断开当前会话，但保留 uri / language_id / 版本号。
    ///
    /// 与 [`Self::close`] 的两点差别：
    /// - **发 `didClose` 后还要收进程**：`close` 只通知服务器「文档关了」，
    ///   进程交给 `Drop` 兜底；而用户按「停止」要的就是进程消失，必须显式
    ///   `shutdown()`（握手 → 等退出 → 超时强杀）。
    /// - **不清会话**：`connect` 之后还能原样重启，`uri` / `version` 都在。
    pub fn stop(&mut self) {
        self.change_task.take();
        self.notify_task.take();
        if let Some(client) = self.client.take() {
            let uri = self.uri.clone();
            smol::spawn(async move {
                let _ = client.did_close(&uri).await;
                client.shutdown();
            })
            .detach();
            log::info!("lsp: stopped server for {}", self.uri);
        }
        self.opened = false;
    }

    /// 订阅服务器的 `publishDiagnostics`，写进编辑器的 `DiagnosticSet`。
    ///
    /// **每个标签一份独立订阅**（`client.notifications()` 现在是广播订阅，
    /// 不是共享队列）：一个服务器进程服务整个项目，多个标签同时打开时，
    /// 每个标签都要收到**全部**诊断通知，再按自己的 uri 过滤。
    ///
    /// 共享队列是错的：`async_channel` 是 MPMC（每条消息只到一个接收者），
    /// A 文件的诊断会被 B 标签的监听器消费掉，而后者只认自己的 uri，于是
    /// `continue` 掉——**没有任何人处理这条诊断**。实测三个标签里有一个的
    /// 诊断被静默丢弃。对照 VSCode：它的通知走 `Emitter.fire()` 广播，
    /// 过滤 uri 的是订阅者自己。
    pub fn spawn_diagnostic_listener(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        let Some(client) = self.client.clone() else {
            log::warn!("lsp: no client, diagnostic listener not started");
            return;
        };
        log::debug!("lsp: starting diagnostic listener for {}", self.uri);
        let weak = editor.downgrade();
        let uri = self.uri.clone();

        self.notify_task.take();
        self.notify_task = Some(cx.spawn(async move |cx| {
            let rx = client.notifications();
            while let Ok(msg) = rx.recv().await {
                let Message::Notification { method, params } = msg else {
                    continue;
                };

                // `$/progress`：服务器的工作进度（索引、扫描…）→ 状态栏。
                if method == "$/progress" {
                    let parsed =
                        serde_json::from_value::<lsp_types::ProgressParams>(params.clone());
                    let Ok(progress) = parsed else {
                        // 解析失败**必须留日志**：这里原本是静默 `continue`，
                        // 于是「`end` 解析不上 → 令牌永不删除 → 状态栏永远卡在
                        // Indexing 0%」这类故障完全没有线索可查。
                        log::debug!("lsp: $/progress unparsable, dropped: {params}");
                        continue;
                    };
                    let lsp_types::ProgressParamsValue::WorkDone(value) = progress.value;
                    // 闭包的返回值是「状态是否变了」，**不能**用它表示实体存活：
                    // 一条不改变状态的进度通知会让 `unwrap_or(false)` 把「没变化」
                    // 读成「编辑器已关」，从而永久终止监听——此后这个文件的诊断
                    // 与进度再也不会更新（chip 停在最后一次变化的样子）。
                    let changed = weak.update(cx, |state: &mut InputState, cx| {
                        let changed = state.apply_lsp_progress(&progress.token, &value);
                        if changed {
                            cx.notify();
                        }
                        changed
                    });
                    if changed.is_err() {
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
                // 必须走 `uri_eq`：windows 上服务器回的盘符大小写可能与本地不一致，
                // 精确比较会让这份诊断被当成「别人的」丢掉（于是文件改了、
                // 诊断不刷新）。
                if !crate::lsp::client::uri_eq(&publish.uri, &uri) {
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
                // 编辑器已被关闭时 `update` 返回 Err：停止监听，避免任务泄漏。
                //
                // 闭包返回的是「有没有变化」，与存活判断**分开**（`is_err`）：
                // 用同一个布尔表示两件事，会让一条被版本门控丢弃的诊断
                // （`applied == false`）被当成「编辑器关了」而终止监听——
                // 此后这个文件的诊断与进度再也不更新，chip 停在旧值。
                let changed = weak.update(cx, |state: &mut InputState, cx| {
                    // 换算与写入都在 `set_diagnostics_versioned` 里完成：
                    // 调用方只有 `&mut self`，无法同时持有 `text()` 的借用。
                    let items = features::to_diagnostics(state.text(), diagnostics);
                    // LSP 诊断是**全量快照**：直接覆盖，不做合并。
                    // 上一轮的 stale 诊断会随本次快照一起消失。
                    let applied = state.set_diagnostics_versioned(items, version);
                    // 状态栏的「实时状态」：按严重级别统计当前文件。
                    let counts = state
                        .diagnostics()
                        .map(|set| {
                            set.items()
                                .iter()
                                .fold((0usize, 0usize), |acc, d| match d.severity {
                                    crate::editor::syntax::DiagnosticSeverity::Error => {
                                        (acc.0 + 1, acc.1)
                                    }
                                    crate::editor::syntax::DiagnosticSeverity::Warning => {
                                        (acc.0, acc.1 + 1)
                                    }
                                    _ => acc,
                                })
                        })
                        .unwrap_or((0, 0));
                    let changed = state.set_lsp_diagnostic_counts(counts.0, counts.1) || applied;
                    // 只在**内容或状态真的变了**时重绘：`notify` 的代价是整棵
                    // 视图树重建，而服务器每次改动都会重发同一份诊断。
                    if changed {
                        cx.notify();
                    }
                    changed
                });
                if changed.is_err() {
                    break;
                }
            }
            // 通道关闭 = 服务器进程退出/连接断开。最后一份诊断已经没有
            // 来源可以更新，留着会变成永久的旧错误。
            let _ = weak.update(cx, |state, _| state.reset_lsp_diagnostics());
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

/// 这个会话该挂到哪个服务器键上。
///
/// **显式根优先**，没有才按文件路径算（[`LspManager::key_for_file`]）。
/// 抽成纯函数是因为 `connect` 要 gpui 的 `App` 且会真的拉起进程，而这条
/// 「谁决定归属」的分支正是跨文件跳转的要点：
///
/// 从项目里点 `List[int]` 跳到 typeshed 的 `typing.pyi`，落点**不在项目树内**。
/// 若按落点自己的路径算根，就在存根目录旁边另起了一个 Python 服务器
/// （pyright 冷启动要索引整个 typeshed，用户体感是「跳一次卡一次」）。
/// 显式把发起跳转的会话的根传进来，两个文件于是共用同一个进程。
///
/// 远端一侧还有个副作用：显式根**不需要**去那台机器上探项目根，
/// 所以跳转打开的文件不会走编辑器的「等根探测先挂起」那条路。
fn server_key_for(
    session_root: Option<&Path>,
    path: &Path,
    language: &str,
    ssh_config: Option<crate::ssh::SshConnectionConfig>,
) -> ServerKey {
    match session_root {
        Some(root) => ServerKey {
            root: root.to_path_buf(),
            language: language.to_string(),
            ssh_config,
        },
        None => LspManager::key_for_file(path, language, ssh_config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跳转打开的文件挂回**发起跳转的服务器**，而不是落点目录。
    #[test]
    fn explicit_root_wins_over_the_path_derived_one() {
        let project = std::env::temp_dir().join("deepterm-stub-project");
        let stub = project.join("../typeshed/typing.pyi");
        let key = server_key_for(Some(&project), &stub, "python", None);
        assert_eq!(key.root, project, "跳转过来的文件必须挂回原根");
        assert_eq!(key.language, "python");
        assert!(key.ssh_config.is_none());
    }

    /// 没指定根时按文件自己算（普通打开的那条路）。
    #[test]
    fn without_an_explicit_root_the_path_decides() {
        let dir = std::env::temp_dir().join("deepterm-stub-free");
        std::fs::create_dir_all(&dir).ok();
        let key = server_key_for(None, &dir.join("main.py"), "python", None);
        // 兜底口径：探不到项目标记就退到文件所在目录（见 `fallback_root`）。
        assert_eq!(key.root, crate::lsp::project::fallback_root(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 「同路径不同主机」仍是两个键：根一样也不能合并。
    #[test]
    fn explicit_root_keeps_the_host_in_the_key() {
        let ssh =
            crate::ssh::SshConnectionConfig::new("10.0.0.1", "bob", crate::ssh::AuthMethod::Agent);
        let key = server_key_for(
            Some(Path::new("/srv/app")),
            Path::new("/srv/app/main.rs"),
            "rust",
            Some(ssh),
        );
        assert_eq!(key.root, Path::new("/srv/app"));
        assert_eq!(
            key.ssh_config.as_ref().map(|c| c.host.as_str()),
            Some("10.0.0.1")
        );
    }
}
