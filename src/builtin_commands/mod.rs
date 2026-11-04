use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::ScrollHandle;
use unicode_segmentation::UnicodeSegmentation;

pub mod argument;
pub mod command_model;
pub mod executor;
pub mod history;
pub mod overlay_ui;
pub mod path_completion;
pub mod path_resolver;
pub mod registry;
pub mod scp_completion;
pub mod suggestion_engine;

// 浮层 UI 已拆至 overlay_ui.rs；re-export 保持
// `builtin_commands::render_overlay` 等旧路径（terminal_view）不变。
pub use argument::{ArgumentError, ArgumentParser, ParsedArgument};
pub use command_model::{BuiltinCommandModel, CommandEntryState, DetectedCommand};
pub use executor::{CommandContext, CommandExecutor, CommandHandler, CommandResult};
pub use history::{CommandHistory, HistoryEntry};
pub use overlay_ui::{OverlayAnchor, render_overlay, render_path_overlay};
pub use path_completion::{CompletionKind, PathCompletionEngine, PathCompletionItem};
pub use path_resolver::{LocalPathContext, PathResolver, RemotePathContext};
pub use registry::{
    ArgumentType, Availability, BuiltinCommand, CommandHandlerId, CommandRegistry, global_registry,
};
pub use scp_completion::{ScpCompletionPhase, ScpCompletionQuery};
pub use suggestion_engine::{ScoredCommand, SuggestionEngine};

/// 输入阶段：区分命令名输入和参数输入
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryPhase {
    /// 正在输入命令名（如 "/ope"）
    CommandName,
    /// 命令已确定，正在输入参数（如 "/open-file src/"）
    ArgumentInput,
}

/// 补全浮层的可见/可用状态机。
///
/// 之前用两个布尔（`dismissed` / `overlay_hidden`）表达这三种状态会相互撞车
/// （例如补全完想"只收起浮层"但被误判为"ESC 取消走 shell"）。改成独立枚举后语义清晰，
/// 状态转移集中到 [`BuiltinCommandManager`] 的方法里。
///
/// 参考：Warp 的 `SlashCommandEntryState::DisabledUntilEmptyBuffer`
/// （app/src/terminal/input/slash_command_model.rs）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayState {
    /// 浮层可见、正常展示补全建议
    Visible,
    /// 浮层临时隐藏（补全成功后收起），再次输入字符会回到 `Visible`；
    /// 回车仍执行内建命令语义
    HiddenAfterPick,
    /// 用户显式 ESC 取消：浮层隐藏，且回车把缓冲原样下发 shell。
    /// 只有缓冲被清空、或首字符重新变成 `/` 时才会复位。
    DismissedUntilReset,
}

impl OverlayState {
    /// 浮层是否应当隐藏
    pub fn is_hidden(self) -> bool {
        !matches!(self, Self::Visible)
    }
}

/// 内建命令管理器，封装命令状态和建议逻辑
pub struct BuiltinCommandManager {
    /// 命令输入缓冲区
    state: Option<BuiltinCommandState>,
    /// 当前查询对应的命令建议缓存
    suggestions: Rc<[Arc<BuiltinCommand>]>,
    /// 命令名建议的选中索引；`None` ⇒ 浮层弹出但无高亮
    selected_index: Option<usize>,
    /// 浮层可见 / 隐藏 / 已取消的状态机。
    overlay: OverlayState,
    /// 当前输入阶段
    entry_phase: EntryPhase,
    /// 路径补全候选列表
    pub path_completions: Vec<PathCompletionItem>,
    /// 路径补全选中索引；`None` ⇒ 无高亮
    pub path_selected_index: Option<usize>,
    /// 当前已确定的命令（用于参数补全阶段）
    pub resolved_command: Option<Arc<BuiltinCommand>>,
    /// 当前工作目录缓存
    cwd: Option<PathBuf>,
    /// `/scp` 时用到的 SSH 连接列表（由外部刷新注入）
    ssh_saved_configs: Arc<Vec<crate::ssh::SshConnectionConfig>>,
    /// 当前终端若是远程 SSH 终端，这里保存其连接配置；本地终端为 `None`。
    /// `/scp` 在没有显式 host 的 token 上会以这里为基准走 SFTP，
    /// 使得"裸路径补全跟随当前终端所在主机"。
    terminal_ssh: Option<crate::ssh::SshConnectionConfig>,
    /// 远程终端通过 OSC 7 报上来的实时 cwd（绝对路径）。
    /// 用于把"裸相对路径"在补全侧拼成绝对 SFTP 路径。本地终端为 `None`。
    terminal_remote_cwd: Option<String>,
    /// 远端目录读取缓存：key 为 `(host id, 绝对目录路径)`，
    /// value 是条目列表或错误字符串
    remote_dir_cache: HashMap<(String, String), RemoteDirCacheEntry>,
    /// 远端目录缓存的访问顺序（LRU），用于限制缓存大小
    remote_dir_cache_lru: Vec<(String, String)>,
    /// 当前本地目录补全请求。本地 `read_dir` 也走后台线程，避免大目录
    /// （`node_modules` / `/usr/lib`）在每次参数击键时卡住 UI。
    current_local_request: Option<PendingLocalLoad>,
    /// 本地补全请求代次；回写时用它丢弃过期结果。
    next_local_request_id: u64,
    /// 已向外部发起、等待结果的远端目录请求集合。
    /// key 为 `(host id, 绝对目录路径)`；value 携带连接配置快照与派发标记。
    /// BTreeMap：派发顺序确定（避免 HashMap 哈希序的不确定性）。
    pending_remote_keys: std::collections::BTreeMap<(String, String), PendingRemoteLoad>,
    /// 上次读取失败、尚未向用户展示过错误的远端目录 key → 错误信息。
    /// cache-miss 分支会消费该标记显示一次错误占位（且不重新注册请求，避免
    /// 失败→刷新→重新请求→再失败的无限循环）；下一次用户输入触发的刷新由于
    /// 标记已被消费，会重新注册请求自动重试。
    failed_remote_keys: HashMap<(String, String), String>,
    /// `/scp` 第二段是否处于"等待远端结果"状态
    pub is_loading_remote: bool,
    /// 本地目录是否正在后台读取（用于占位/测试）。
    pub is_loading_local: bool,
    /// 路径补全浮层的滚动句柄：键盘 / Tab 切换选中项时调用
    /// `scroll_to_item` 把选中行带回可视区域，避免被列表区裁掉。
    path_scroll_handle: ScrollHandle,
    /// 注入的命令历史快照。外部在进入命令模式时刷新一次即可。
    history: CommandHistory,
    /// 当前输入对应的历史候选（refresh_state 时计算）。
    /// 在两个阶段都置于补全列表顶部，索引区间 `[0..len)`。
    history_matches: Vec<HistoryEntry>,
    /// 历史候选的选中下标；`None` ⇒ 未选中历史项。
    /// 与 `selected_index` / `path_selected_index` 互斥：选中历史时后两者为 `None`。
    history_selected_index: Option<usize>,
}

/// 远端目录在补全缓存中的状态
#[derive(Clone, Debug)]
pub struct RemoteDirCacheEntry {
    items: Vec<RemoteEntrySnapshot>,
}

/// 从外部异步加载后写入的远端条目快照
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteEntrySnapshot {
    pub name: String,
    pub is_directory: bool,
    pub full_path: String,
}

/// 外部需要加载的远端目录请求
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteCompletionRequest {
    /// 完整连接配置快照：请求携带配置本身，加载方无需再读磁盘配置文件。
    /// （当前终端的 SSH 连接可能未保存，或已校验后被改名/删除；只传 id 的话
    /// 这些场景下补全会永远失败。）
    pub config: crate::ssh::SshConnectionConfig,
    pub absolute_dir: String,
}

/// 本地目录补全的上下文：决定后台结果如何拼回补全列表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalCompletionContext {
    /// 普通参数：直接使用 `PathCompletionEngine` 的条目。
    Plain,
    /// `/scp` HostOrLocal：主机候选 + 本机文件。
    ScpHostOrLocal,
    /// `/scp` 的 `Local:` 路径段：每个候选拼回 `<host_token>:path`。
    ScpLocalPath { host_token: String },
}

/// 外部需要加载的本地目录请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCompletionRequest {
    pub id: u64,
    pub prefix: String,
    pub cwd: Option<PathBuf>,
    pub context: LocalCompletionContext,
}

/// 一个已注册、等待后台读取的本地目录条目。
#[derive(Clone, Debug)]
struct PendingLocalLoad {
    request: LocalCompletionRequest,
    started: bool,
}

/// 一个已注册、等待外部加载的远端目录条目。
#[derive(Clone, Debug)]
struct PendingRemoteLoad {
    config: crate::ssh::SshConnectionConfig,
    /// 是否已向外部发起（避免同一请求重复派发）
    started: bool,
}

impl BuiltinCommandManager {
    /// 列表长度 × 原始下标 → clamp 后的有效下标（列表收缩后选中项回退到末尾）。
    fn clamp_index(list_len: usize, idx: Option<usize>) -> Option<usize> {
        if list_len == 0 {
            return None;
        }
        idx.map(|i| i.min(list_len - 1))
    }

    /// 创建一个空闲的内建命令管理器。
    pub fn new() -> Self {
        Self {
            state: None,
            suggestions: Rc::from(Vec::new()),
            selected_index: None,
            overlay: OverlayState::Visible,
            entry_phase: EntryPhase::CommandName,
            path_completions: Vec::new(),
            path_selected_index: None,
            resolved_command: None,
            cwd: std::env::current_dir().ok(),
            ssh_saved_configs: Arc::new(Vec::new()),
            terminal_ssh: None,
            terminal_remote_cwd: None,
            current_local_request: None,
            next_local_request_id: 0,
            remote_dir_cache: HashMap::new(),
            remote_dir_cache_lru: Vec::new(),
            pending_remote_keys: std::collections::BTreeMap::new(),
            failed_remote_keys: HashMap::new(),
            is_loading_remote: false,
            is_loading_local: false,
            path_scroll_handle: ScrollHandle::new(),
            history: CommandHistory::default(),
            history_matches: Vec::new(),
            history_selected_index: None,
        }
    }

    /// 注入 SSH 保存连接列表。外部应在 `/scp` 模式启动前刷新一次即可。
    pub fn set_ssh_configs(&mut self, configs: Vec<crate::ssh::SshConnectionConfig>) {
        self.ssh_saved_configs = Arc::new(configs);
    }

    /// 注入命令历史快照。外部应在进入命令模式时刷新一次（来自 `history::snapshot()`）。
    pub fn set_history(&mut self, history: CommandHistory) {
        self.history = history;
        // 历史变了 ⇒ 当前匹配可能失效，按现有缓冲重算
        self.refresh_history_matches();
    }

    /// 当前输入对应的历史候选（置于补全列表顶部）。
    pub fn history_matches(&self) -> &[HistoryEntry] {
        &self.history_matches
    }

    /// 历史候选段是否非空（用于决定浮层是否渲染）。
    pub fn has_history_matches(&self) -> bool {
        !self.history_matches.is_empty()
    }

    /// 历史候选的选中下标（指向 `history_matches`）。
    pub fn history_selected_index(&self) -> Option<usize> {
        Self::clamp_index(self.history_matches.len(), self.history_selected_index)
    }

    /// 按当前缓冲重算历史候选；缓冲为空时清空。
    fn refresh_history_matches(&mut self) {
        self.history_matches = match self.query() {
            Some(q) if !q.trim().is_empty() => self.history.matches(q),
            _ => Vec::new(),
        };
        // 列表变化后 clamp 选中项
        self.history_selected_index = self.history_selected_index();
    }

    /// 注入"当前终端"的连接信息。
    ///
    /// - 本地终端：`ssh = None`、`remote_cwd = None`
    /// - 远程终端：`ssh = Some(...)`，`remote_cwd` 取 OSC 7 报上来的实时 cwd
    ///
    /// 设置后，`/scp` 在没有显式 host 的 token 上会跟随当前终端所在主机：
    /// 远程终端走 SFTP 列出远端目录，本地终端走本机文件系统。
    pub fn set_terminal_context(
        &mut self,
        ssh: Option<crate::ssh::SshConnectionConfig>,
        remote_cwd: Option<String>,
    ) {
        self.terminal_ssh = ssh;
        self.terminal_remote_cwd = remote_cwd;
    }

    /// 注入"本机端"的工作目录基准。
    ///
    /// 路径补全的相对路径（如 `/scp `、`Local:./xx`）以这里为基准。
    /// 调用方应当传入当前终端 shell 的实时 cwd（本地终端 → 由 `lsof` / `/proc` 查到；
    /// 远程终端 → DeepTerm 进程的工作目录），否则回退到 `std::env::current_dir()`。
    pub fn set_local_cwd(&mut self, cwd: Option<PathBuf>) {
        self.cwd = cwd.or_else(|| std::env::current_dir().ok());
    }

    /// 写入/更新远端目录结果，并根据需要刷新当前补全列表。
    pub fn apply_remote_dir_result(
        &mut self,
        config_id: String,
        absolute_dir: String,
        entries: Result<Vec<RemoteEntrySnapshot>, String>,
    ) {
        let key = (config_id, absolute_dir);
        let should_refresh = match entries {
            Ok(new_items) => {
                // 成功 ⇒ 清除可能残留的失败标记
                self.failed_remote_keys.remove(&key);
                // 对比缓存：有变化才更新
                let changed = match self.remote_dir_cache.get(&key) {
                    Some(cached) => cached.items != new_items,
                    None => true,
                };
                if changed {
                    self.remote_dir_cache
                        .insert(key.clone(), RemoteDirCacheEntry { items: new_items });
                    // LRU 淘汰（同步清理对应失败标记，避免孤儿条目无限堆积）
                    self.remote_dir_cache_lru.retain(|k| k != &key);
                    self.remote_dir_cache_lru.push(key.clone());
                    while self.remote_dir_cache_lru.len() > 128 {
                        let oldest = self.remote_dir_cache_lru.remove(0);
                        self.remote_dir_cache.remove(&oldest);
                        self.failed_remote_keys.remove(&oldest);
                    }
                }
                changed
            }
            Err(err) => {
                // 失败不写正向缓存，但记录一次错误标记，让 cache-miss 分支把
                // "正在加载" 收敛成错误占位（否则 UI 会永远停在加载中）。
                // 仅当之前没有可用缓存时才清空已有缓存——避免一次后台刷新失败
                // 就把先前成功列出的内容也抹掉。
                if !self.remote_dir_cache.contains_key(&key) {
                    // 失败标记有上限：超出时整体清空（这些标记只影响 UX 提示，
                    // 且正常会话远达不到此规模；防长会话无限增长）
                    if self.failed_remote_keys.len() >= 128 {
                        self.failed_remote_keys.clear();
                    }
                    self.failed_remote_keys.insert(key.clone(), err);
                }
                self.pending_remote_keys.remove(&key);
                self.refresh_path_completions();
                return;
            }
        };
        self.pending_remote_keys.remove(&key);
        if should_refresh {
            self.refresh_path_completions();
        }
    }

    /// 如果 `/scp` 当前 token 有未完成的远端目录请求，返回需要加载的请求。
    /// 调用方负责在后台线程读取 SFTP，完成后回传到 `apply_remote_dir_result`。
    /// 请求携带完整连接配置快照，调用方不再依赖磁盘上的保存配置。
    /// 用 BTreeMap 保证多个挂起请求时派发顺序确定（按 key 字典序）。
    pub fn pending_remote_request(&mut self) -> Option<RemoteCompletionRequest> {
        for ((_cfg_id, dir), entry) in self.pending_remote_keys.iter_mut() {
            if entry.started {
                continue;
            }
            entry.started = true;
            return Some(RemoteCompletionRequest {
                config: entry.config.clone(),
                absolute_dir: dir.clone(),
            });
        }
        None
    }

    /// 注册一次远端目录后台加载（key 已存在时保持原样，不重复派发）。
    fn register_remote_dir_load(
        &mut self,
        key: (String, String),
        config: crate::ssh::SshConnectionConfig,
    ) {
        self.pending_remote_keys
            .entry(key)
            .or_insert_with(|| PendingRemoteLoad {
                config,
                started: false,
            });
    }

    /// 放弃当前本地补全请求（分支切换/新输入时调用），并推进代次让在途
    /// 后台结果失效。
    fn cancel_local_completion(&mut self) {
        self.next_local_request_id = self.next_local_request_id.wrapping_add(1);
        self.current_local_request = None;
        self.is_loading_local = false;
    }

    /// 注册一次本地目录后台读取。前缀与 cwd 一起快照，避免读取期间
    /// 用户输入变化导致结果拼到错误目录。
    fn request_local_completions(&mut self, prefix: &str, context: LocalCompletionContext) {
        self.next_local_request_id = self.next_local_request_id.wrapping_add(1);
        self.current_local_request = Some(PendingLocalLoad {
            request: LocalCompletionRequest {
                id: self.next_local_request_id,
                prefix: prefix.to_string(),
                cwd: self.cwd.clone(),
                context,
            },
            started: false,
        });
        self.is_loading_local = true;
    }

    /// 取出一个尚未派发的本地目录请求。
    pub fn pending_local_request(&mut self) -> Option<LocalCompletionRequest> {
        let pending = self.current_local_request.as_mut()?;
        if pending.started {
            return None;
        }
        pending.started = true;
        Some(pending.request.clone())
    }

    /// 该代次是否仍是当前请求（后台任务唤醒后先查一次，避免无谓读盘）。
    pub fn is_local_request_current(&self, id: u64) -> bool {
        self.current_local_request
            .as_ref()
            .is_some_and(|pending| pending.request.id == id)
    }

    /// 写入后台读回的本地目录结果；过期代次直接丢弃。
    pub fn apply_local_completion_result(&mut self, id: u64, local_items: Vec<PathCompletionItem>) {
        if !self.is_local_request_current(id) {
            return;
        }
        let Some(pending) = self.current_local_request.take() else {
            return;
        };
        let request = pending.request;
        let items = match request.context {
            LocalCompletionContext::Plain => local_items,
            LocalCompletionContext::ScpHostOrLocal => {
                let mut items =
                    build_host_only_completions(&request.prefix, &self.ssh_saved_configs);
                items.extend(map_host_or_local_items(&request.prefix, local_items));
                items
            }
            LocalCompletionContext::ScpLocalPath { host_token } => {
                map_local_path_items(&request.prefix, &host_token, local_items)
            }
        };
        self.is_loading_local = false;
        self.path_completions = items;
        self.path_selected_index = None;
    }

    /// 获取当前缓冲区内容
    pub fn query(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.buffer())
    }

    /// 检查是否有活跃的命令输入
    pub fn is_active(&self) -> bool {
        self.state.is_some()
    }

    /// 检查是否处于 ESC 取消状态（回车会把缓冲交给 shell）。
    pub fn is_dismissed(&self) -> bool {
        matches!(self.overlay, OverlayState::DismissedUntilReset)
    }

    /// 检查是否处于参数输入阶段
    pub fn is_argument_input(&self) -> bool {
        self.entry_phase == EntryPhase::ArgumentInput
    }

    /// 是否有可被回车确认的补全条目（命令名候选 / 路径候选）。
    ///
    /// 必须满足三点：
    /// 1. 浮层 visible
    /// 2. 用户已通过 ↑/↓/Tab 真正高亮某一项（`Some(i)`）
    /// 3. 该项不是 loading / 错误占位
    pub fn has_active_completion(&self) -> bool {
        if self.overlay.is_hidden() {
            return false;
        }
        // 选中了历史项 ⇒ 始终是可确认的有效补全
        if self.history_selected_index().is_some() {
            return true;
        }
        match self.entry_phase {
            EntryPhase::CommandName => {
                self.selected_index.is_some() && !self.suggestions.is_empty()
            }
            EntryPhase::ArgumentInput => {
                if self.path_selected_index.is_none() {
                    return false;
                }
                self.selected_path_completion()
                    .map(|item| {
                        item.replacement.is_some()
                            || matches!(
                                item.kind,
                                CompletionKind::Directory
                                    | CompletionKind::File
                                    | CompletionKind::Host
                            ) && !item.name.is_empty()
                                && !self.is_loading_remote
                    })
                    .unwrap_or(false)
            }
        }
    }

    /// 浮层是否应当渲染：
    /// - 命令模式启动；浮层 visible；当前阶段有可见候选项（含历史段；loading/错误占位也算可见）
    pub fn should_render_overlay(&self) -> bool {
        if !self.is_active() || self.overlay.is_hidden() {
            return false;
        }
        if self.has_history_matches() {
            return true;
        }
        match self.entry_phase {
            EntryPhase::CommandName => !self.suggestions.is_empty(),
            EntryPhase::ArgumentInput => !self.path_completions.is_empty(),
        }
    }

    /// 获取命令建议列表缓存的共享引用。
    pub fn suggestions(&self) -> Rc<[Arc<BuiltinCommand>]> {
        self.suggestions.clone()
    }

    /// 获取当前选中的命令
    pub fn selected_command(&self) -> Option<Arc<BuiltinCommand>> {
        let index = self.selected_index?;
        self.suggestions.get(index).cloned()
    }

    /// 获取当前选中的路径补全项
    pub fn selected_path_completion(&self) -> Option<&PathCompletionItem> {
        let index = self.path_selected_index?;
        self.path_completions.get(index)
    }

    /// 追加输入内容。
    pub fn update(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let was_dismissed = self.is_dismissed();
        let old_first_char = self.state.as_ref().and_then(|s| s.buffer().chars().next());
        if let Some(state) = self.state.as_mut() {
            state.insert(text);
        } else {
            self.state = Some(BuiltinCommandState::new(text.to_string()));
        }
        let new_first_char = self.state.as_ref().and_then(|s| s.buffer().chars().next());

        // 取消状态的复位规则（参考 Warp 的 DisabledUntilEmptyBuffer + did_add_slash）：
        // - 普通 HiddenAfterPick → 每次输入都视作继续编辑，恢复 Visible
        // - DismissedUntilReset → 只有首字符从非 `/` 变成 `/` 时才复活（例如用户把命令
        //   从前头粘一个 `/`）；否则保持取消，直到缓冲清空/整体 reset
        self.overlay = if was_dismissed {
            let did_add_slash = new_first_char == Some('/') && old_first_char != Some('/');
            if did_add_slash {
                OverlayState::Visible
            } else {
                OverlayState::DismissedUntilReset
            }
        } else {
            OverlayState::Visible
        };
        // 列表会因输入变化而刷新，重置高亮 —— 让用户按 ↓/Tab 重新选
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
        self.refresh_state();
    }

    /// 以远端广播的整段缓冲替换当前缓冲（文本 + 光标）。供广播接收方镜像输入使用。
    ///
    /// 与 `update` / `pop` 不同，这里直接整体替换而不做增量编辑；会重置所有高亮、
    /// 恢复浮层可见，并按新缓冲重算当前阶段与补全列表。
    pub fn set_buffer(&mut self, text: &str, cursor: usize) {
        if text.is_empty() {
            self.state = None;
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = None;
            self.path_completions.clear();
        } else {
            self.state = Some(BuiltinCommandState::new(text.to_string()));
            if let Some(state) = self.state.as_mut() {
                state.set_cursor(cursor.min(text.len()));
            }
        }
        self.overlay = OverlayState::Visible;
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
        self.refresh_state();
    }

    /// 删除光标前一个字符（退格）。
    pub fn pop(&mut self) {
        let was_dismissed = self.is_dismissed();
        let mut should_clear = false;
        if let Some(state) = self.state.as_mut() {
            state.delete_backward();
            should_clear = state.is_empty();
        }
        if should_clear {
            self.state = None;
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = None;
            self.path_completions.clear();
        }
        // 缓冲清空 → 回到 Visible（任何取消/隐藏状态都被重置）；
        // 否则保持原有取消语义，避免用户 ESC 后一路删着字还莫名其妙又弹起来
        self.overlay = if should_clear || !was_dismissed {
            OverlayState::Visible
        } else {
            OverlayState::DismissedUntilReset
        };
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
        self.refresh_state();
    }

    /// 删除光标后一个字符（Delete 键）。
    pub fn delete_forward(&mut self) {
        let mut should_clear = false;
        let mut changed = false;
        if let Some(state) = self.state.as_mut() {
            changed = state.delete_forward();
            should_clear = state.is_empty();
        }
        if !changed {
            return;
        }
        if should_clear {
            self.state = None;
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = None;
            self.path_completions.clear();
            self.overlay = OverlayState::Visible;
        }
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
        self.refresh_state();
    }

    /// 删除光标前一个词（Opt+Backspace）。
    pub fn delete_word_backward(&mut self) {
        let was_dismissed = self.is_dismissed();
        let mut should_clear = false;
        let mut changed = false;
        if let Some(state) = self.state.as_mut() {
            changed = state.delete_word_backward();
            should_clear = state.is_empty();
        }
        if !changed {
            return;
        }
        if should_clear {
            self.state = None;
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = None;
            self.path_completions.clear();
        }
        self.overlay = if should_clear || !was_dismissed {
            OverlayState::Visible
        } else {
            OverlayState::DismissedUntilReset
        };
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
        self.refresh_state();
    }

    /// 光标左移一个字符。
    pub fn move_cursor_left(&mut self) {
        self.move_cursor_with(BuiltinCommandState::move_left);
    }

    /// 光标右移一个字符。
    pub fn move_cursor_right(&mut self) {
        self.move_cursor_with(BuiltinCommandState::move_right);
    }

    /// 光标按词左移（Opt+←）。
    pub fn move_cursor_word_left(&mut self) {
        self.move_cursor_with(BuiltinCommandState::move_word_left);
    }

    /// 光标按词右移（Opt+→）。
    pub fn move_cursor_word_right(&mut self) {
        self.move_cursor_with(BuiltinCommandState::move_word_right);
    }

    /// 光标移到行首（Home / Cmd+←）。
    pub fn move_cursor_home(&mut self) {
        self.move_cursor_with(BuiltinCommandState::move_home);
    }

    /// 光标移到行尾（End / Cmd+→）。
    pub fn move_cursor_end(&mut self) {
        self.move_cursor_with(BuiltinCommandState::move_end);
    }

    /// 移动光标后，按新光标所在 token 重算补全列表（仅参数阶段需要）。
    ///
    /// 光标只是位置变化、不改写缓冲，所以不动命令名建议 / 历史匹配 / overlay 状态，
    /// 只在参数阶段重刷路径补全，使弹窗跟随光标落到对应参数（如本地源 / 远端目的）。
    fn move_cursor_with(&mut self, mv: impl FnOnce(&mut BuiltinCommandState) -> bool) {
        let moved = self.state.as_mut().map(mv).unwrap_or(false);
        if !moved {
            return;
        }
        if self.entry_phase == EntryPhase::ArgumentInput {
            self.path_selected_index = None;
            self.refresh_path_completions();
        }
    }

    /// 当前光标在缓冲中的字节偏移；无活跃输入时返回 `None`。
    pub fn cursor(&self) -> Option<usize> {
        self.state.as_ref().map(|s| s.cursor())
    }

    /// 清空命令状态，完全退出内建命令模式。
    pub fn clear(&mut self) {
        self.state = None;
        self.suggestions = Rc::from(Vec::new());
        self.selected_index = None;
        self.overlay = OverlayState::Visible;
        self.entry_phase = EntryPhase::CommandName;
        self.path_completions.clear();
        self.path_selected_index = None;
        self.resolved_command = None;
        self.history_matches.clear();
        self.history_selected_index = None;
    }

    /// 标记为已取消（ESC）：隐藏浮层，且回车把缓冲原样下发 shell。
    /// 仅在缓冲清空或首字符重新变成 `/` 时才会自动复位。
    pub fn dismiss(&mut self) {
        self.overlay = OverlayState::DismissedUntilReset;
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
    }

    /// 仅隐藏浮层，不改变"是否走内建命令"的语义。
    ///
    /// 用于补全完成后临时收起补全列表 —— 再次回车仍会执行内建命令，
    /// 用户继续输入字符 / 退格会自动重开浮层。
    pub fn hide_overlay(&mut self) {
        self.overlay = OverlayState::HiddenAfterPick;
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
    }

    /// 补全成功后由外层调用：根据当前缓冲是否还需更多参数决定下一步：
    /// - 还需更多参数（且选中项不是目录） ⇒ 追加空格 → 下一阶段浮层自动重弹
    /// - 已经满了 / 选中目录（用户还在该 token 内继续展开）⇒ 保持浮层可见
    pub fn advance_after_pick(&mut self) {
        if self.should_advance_to_next_arg() {
            // `update(" ")` 同时把 overlay 切回 Visible 并触发 refresh_state
            self.update(" ");
        } else {
            // 目录选中后保持浮层展开，让用户继续选择子目录
            let is_dir_expanding = self.query().is_some_and(|b| b.ends_with('/'));
            if !is_dir_expanding {
                self.hide_overlay();
            }
        }
    }

    /// 判断当前缓冲在补全后是否应当 auto-advance 到下一个参数 token。
    ///
    /// 规则（通用化，支持任意多参数命令）：
    /// 1. 当前命令必须存在且需要参数
    /// 2. 缓冲已存在的 token 数 < 命令期望参数个数（还有空位）
    /// 3. 末尾不是空白（紧跟着已经是一个完整 token；否则 update(" ") 会把空白叠倍）
    /// 4. 末尾不是目录尾斜杠 `/`（用户还在该 token 内展开子目录）
    /// 5. 末尾不是 host 分隔符 `:`（host 刚补全完成，path 部分还没开始输入）
    fn should_advance_to_next_arg(&self) -> bool {
        let Some(buffer) = self.query() else {
            return false;
        };
        let Some(cmd) = self.resolved_command.as_ref() else {
            return false;
        };
        // 命令不需要参数时不 advance
        if cmd.argument == ArgumentType::None {
            return false;
        }
        // 末尾空白意味着此前已经处于"下一参数空位"阶段，不需要再加
        if buffer.chars().last().is_some_and(char::is_whitespace) {
            return false;
        }
        // 用户选中目录后 path 末尾会是 `/` —— 留在当前 token 继续展开
        if buffer.ends_with('/') {
            return false;
        }
        // host 补全刚完成时缓冲以 `:` 结尾（如 `/scp root@1.2.3.4:`），
        // 表示用户接下来要在同一 token 内输入远端 path —— 不要追加空格，
        // 否则 `:` 后会被切成第二个参数，导致 scp 解析失败。
        if buffer.ends_with(':') {
            return false;
        }
        // 命令名 + 后续 token 总数；< 期望参数个数 ⇒ 还需要 advance
        // /scp 至少 2 个参数（多源时可在最后一个 token 前手动继续追加源），
        // 因此参数 token ≥ 2 后不再自动前移。
        let total_tokens = scp_completion::shell_split_tokens(
            buffer
                .split_once(|c: char| c.is_whitespace())
                .map(|x| x.1)
                .unwrap_or(""),
            0,
        )
        .len();
        total_tokens < 2
    }

    /// 浮层是否应当隐藏（DismissedUntilReset 或 HiddenAfterPick）
    pub fn is_overlay_hidden(&self) -> bool {
        self.overlay.is_hidden()
    }

    /// 当前浮层状态（用于测试 / 精确分支）
    pub fn overlay_state(&self) -> OverlayState {
        self.overlay
    }

    /// 当前阶段「原候选段」（命令名 / 路径）的条目数，不含历史段。
    fn base_completion_len(&self) -> usize {
        match self.entry_phase {
            EntryPhase::CommandName => self.suggestions.len(),
            EntryPhase::ArgumentInput => self.path_completions.len(),
        }
    }

    /// 组合列表的总条目数 = 历史段 + 原候选段。
    fn combined_total(&self) -> usize {
        self.history_matches.len() + self.base_completion_len()
    }

    /// 组合空间中的当前选中下标：`[0..H)` 为历史，`[H..)` 为原候选。
    fn combined_selected(&self) -> Option<usize> {
        let h = self.history_matches.len();
        if let Some(i) = self.history_selected_index {
            return Some(i.min(h.saturating_sub(1)));
        }
        let base = match self.entry_phase {
            EntryPhase::CommandName => self.selected_index,
            EntryPhase::ArgumentInput => self.path_selected_index,
        }?;
        Some(h + base)
    }

    /// 把组合空间下标拆分回「历史 / 原候选」两段并写入对应字段，必要时滚动。
    fn set_combined_selected(&mut self, combined: usize) {
        let h = self.history_matches.len();
        if combined < h {
            self.history_selected_index = Some(combined);
            self.selected_index = None;
            self.path_selected_index = None;
        } else {
            let base = combined - h;
            self.history_selected_index = None;
            match self.entry_phase {
                EntryPhase::CommandName => {
                    self.selected_index = Some(base);
                }
                EntryPhase::ArgumentInput => {
                    self.path_selected_index = Some(base);
                    self.path_scroll_handle.scroll_to_item(base);
                }
            }
        }
    }

    /// 在组合列表（历史段 + 原候选段）中循环选择下一项。
    ///
    /// 当前未选中（`None`）时，第一次调用进入第 0 项，与"按下 ↓ 才开始选"的语义匹配。
    pub fn select_next(&mut self) {
        let total = self.combined_total();
        if total == 0 {
            return;
        }
        let next = match self.combined_selected() {
            None => 0,
            Some(i) => (i + 1) % total,
        };
        self.set_combined_selected(next);
    }

    /// 在组合列表中循环选择上一项。
    pub fn select_previous(&mut self) {
        let total = self.combined_total();
        if total == 0 {
            return;
        }
        let prev = match self.combined_selected() {
            None => total - 1,
            Some(0) => total - 1,
            Some(i) => i - 1,
        };
        self.set_combined_selected(prev);
    }

    /// 当前选中的历史候选（指向 `history_matches`）。
    pub fn selected_history_entry(&self) -> Option<&HistoryEntry> {
        let index = self.history_selected_index()?;
        self.history_matches.get(index)
    }

    /// 应用当前选中的历史项：用其整行覆盖缓冲（光标落末尾），收起浮层等待回车提交。
    /// 无选中历史项时为 no-op。
    pub fn complete_history(&mut self) {
        let Some(entry) = self.selected_history_entry().cloned() else {
            return;
        };
        if let Some(state) = self.state.as_mut() {
            state.set_buffer(entry.command_line.clone());
        } else {
            self.state = Some(BuiltinCommandState::new(entry.command_line.clone()));
        }
        // 回填整行后只收起浮层（HiddenAfterPick）：再次回车即提交，继续输入会重开浮层。
        self.refresh_state();
        self.hide_overlay();
    }

    /// 用当前选中项替换命令名前缀，同时保留已经输入的参数部分。
    pub fn complete(&mut self) {
        let Some(selected) = self.selected_command() else {
            return;
        };
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let current_input = state.buffer().to_string();
        let args = current_input
            .split_once(|c: char| c.is_whitespace())
            .map(|x| x.1)
            .unwrap_or("");

        let new_input = if args.is_empty() {
            selected.name.to_string()
        } else {
            format!("{} {}", selected.name, args)
        };

        state.set_buffer(new_input);
        // 命令名补全完成 ⇒ 立刻视为 Visible（用户继续编辑就还要看建议）
        self.overlay = OverlayState::Visible;
        self.selected_index = None;
        self.path_selected_index = None;
        self.history_selected_index = None;
        self.refresh_state();
    }

    /// 应用当前选中的路径补全。
    ///
    /// 将**光标所在**的路径参数替换为选中的完整路径。无选中时为 no-op。
    /// 替换后光标落到替换串末尾。若选中的是目录，刷新该目录下的补全列表（保持浮层展开）。
    pub fn complete_path(&mut self) {
        let Some(selected) = self.selected_path_completion().cloned() else {
            return;
        };
        // 占位项（读取中 / 读取失败）不可应用：既无替换文本也无真实路径，
        // 落到通用分支会把占位文案写进命令行（如 `/scp /srv/读取远端目录中…`）。
        if selected.is_placeholder() {
            return;
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let current_input = state.buffer().to_string();
        let cursor = state.cursor();

        // `/scp` 走独立分支：补全项自带 `replacement`（可能含 host 前缀）
        if is_scp_command(&current_input)
            && let Some(replacement) = selected.replacement.clone()
            && let Some(query) = scp_completion::analyse_at(&current_input, cursor)
        {
            let mut next = String::with_capacity(current_input.len() + replacement.len());
            next.push_str(&current_input[..query.token_start]);
            next.push_str(&replacement);
            next.push_str(&current_input[query.token_end..]);
            // 光标落到替换串末尾（保持在被补全的 token 上，便于继续展开目录）
            let new_cursor = query.token_start + replacement.len();
            state.set_buffer_with_cursor(next, new_cursor);
            self.path_selected_index = None;
            self.refresh_path_completions();
            // 选中目录后保持浮层可见，但不预选 —— 用户需主动按 ↑/↓/Tab 才能选中
            if selected.is_directory {
                self.overlay = OverlayState::Visible;
            }
            return;
        }

        // 通用命令：替换光标所在的参数 token（而非整行末尾）。
        let (token_start, token_end) = generic_arg_token_range_at(&current_input, cursor);
        let token = &current_input[token_start..token_end];
        let (dir_part, _file_prefix) = PathCompletionEngine::split_path_prefix(token);

        // 构建新 token：目录前缀 + 选中的名称
        let new_token = if selected.is_directory {
            format!("{}{}/", dir_part, selected.name)
        } else {
            format!("{}{}", dir_part, selected.name)
        };

        let mut next = String::with_capacity(current_input.len() + new_token.len());
        next.push_str(&current_input[..token_start]);
        next.push_str(&new_token);
        next.push_str(&current_input[token_end..]);
        let new_cursor = token_start + new_token.len();

        state.set_buffer_with_cursor(next, new_cursor);
        self.path_selected_index = None;
        self.refresh_path_completions();

        // 选中目录后保持浮层可见，但不预选 —— 用户需主动按 ↑/↓/Tab 才能选中
        if selected.is_directory {
            self.overlay = OverlayState::Visible;
        }
    }

    /// 按外部交互（如鼠标点击）直接设置选中项。
    pub fn set_selected_index(&mut self, index: usize) {
        let total = self.suggestions.len();
        self.selected_index = if total == 0 {
            None
        } else {
            Some(index.min(total - 1))
        };
    }

    /// 设置路径补全选中项。
    pub fn set_path_selected_index(&mut self, index: usize) {
        let total = self.path_completions.len();
        self.path_selected_index = if total == 0 {
            None
        } else {
            let clamped = index.min(total - 1);
            self.path_scroll_handle.scroll_to_item(clamped);
            Some(clamped)
        };
    }

    /// 处理一次显式选中（鼠标点击）。
    pub fn pick(&mut self, index: usize) {
        self.set_selected_index(index);
        self.complete();
    }

    /// 处理路径补全的显式选中（鼠标点击）。
    pub fn pick_path(&mut self, index: usize) {
        self.set_path_selected_index(index);
        self.complete_path();
    }

    /// 处理历史候选的显式选中（鼠标点击）：选中并立即回填整行。
    pub fn pick_history(&mut self, index: usize) {
        let total = self.history_matches.len();
        if total == 0 {
            return;
        }
        self.history_selected_index = Some(index.min(total - 1));
        self.selected_index = None;
        self.path_selected_index = None;
        self.complete_history();
    }

    /// 获取当前命令名建议的高亮索引（`None` ⇒ 未高亮）。
    pub fn selected_index(&self) -> Option<usize> {
        Self::clamp_index(self.suggestions.len(), self.selected_index)
    }

    /// 获取当前路径建议的高亮索引（`None` ⇒ 未高亮）。
    pub fn selected_path_index(&self) -> Option<usize> {
        Self::clamp_index(self.path_completions.len(), self.path_selected_index)
    }

    /// 路径补全浮层共享的滚动句柄，供 [`render_path_overlay`] `.track_scroll()` 使用。
    pub fn path_scroll_handle(&self) -> &ScrollHandle {
        &self.path_scroll_handle
    }

    /// 根据当前缓冲区文本重算所有状态。
    fn refresh_state(&mut self) {
        self.refresh_suggestions();
        self.detect_entry_phase();
        if self.entry_phase == EntryPhase::ArgumentInput {
            self.refresh_path_completions();
        } else {
            self.path_completions.clear();
            self.path_selected_index = None;
        }
        self.refresh_history_matches();
    }

    /// 检测当前输入阶段。
    fn detect_entry_phase(&mut self) {
        let Some(query) = self.query() else {
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = None;
            return;
        };

        let trailing_whitespace = query
            .chars()
            .last()
            .map(char::is_whitespace)
            .unwrap_or(false);
        let command_name = query.split_whitespace().next().unwrap_or("");
        let has_second_token = query.split_whitespace().nth(1).is_some();

        // 命令已解析成功（精确匹配命令名），并且满足以下任一：
        // - 已有第二个 token（参数本体）
        // - 命令名后有空白（用户按了空格，准备输参数）
        // 且命令的 argument 类型不是 None
        if let Some(cmd) = global_registry().find(command_name) {
            let needs_arg = cmd.argument != ArgumentType::None;
            if needs_arg && (has_second_token || trailing_whitespace) {
                self.entry_phase = EntryPhase::ArgumentInput;
                self.resolved_command = Some(cmd);
                return;
            }
            // 命令名匹配但还没到参数阶段
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = Some(cmd);
            return;
        }

        self.entry_phase = EntryPhase::CommandName;
        self.resolved_command = None;
    }

    /// 刷新命令建议列表。
    fn refresh_suggestions(&mut self) {
        self.suggestions = self
            .query()
            .map(|query| {
                let all_commands = global_registry().all_commands();
                let scored = SuggestionEngine::suggest(&all_commands, query);
                scored.into_iter().map(|s| s.command).collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into();
        // 刷新后用 selected_index() clamp 现有索引（若先前 Some(i) 越界则回到合法范围；
        // 若已 None 则保持 None —— "无选中" 的语义不会因列表变化被无中生有地带入）。
        self.selected_index = self.selected_index();
    }

    /// 刷新路径补全列表。
    fn refresh_path_completions(&mut self) {
        // 输入变了：在途的本地读取结果作废（远端请求按 key 复用，不受影响）。
        self.cancel_local_completion();
        let Some(query) = self.query().map(str::to_string) else {
            self.path_completions.clear();
            self.is_loading_remote = false;
            return;
        };
        // 补全焦点跟随光标所在 token；无活跃 state 时退化为末尾。
        let cursor = self.cursor().unwrap_or(query.len());

        // `/scp` 专用分支：多参数（多源 + 目的）、可能是主机、远端目录
        if is_scp_command(&query) {
            self.refresh_scp_completions(&query, cursor);
            return;
        }

        // 通用单参数补全：取光标所在的参数 token（而非整行末尾），
        // 这样光标移回前面也能列出对应目录。
        let effective_prefix = generic_arg_prefix_at(&query, cursor);

        if let Some(terminal_cfg) = self.terminal_ssh.clone() {
            self.refresh_bare_remote_completions(&effective_prefix, terminal_cfg, None);
            return;
        }

        // 本地终端：后台列出本机文件；先用占位项保持浮层打开。
        self.is_loading_remote = false;
        self.path_completions = vec![local_loading_item()];
        self.path_selected_index = None;
        self.request_local_completions(&effective_prefix, LocalCompletionContext::Plain);
    }

    /// `/scp` 补全主入口：按光标所在 token 的阶段分发。
    fn refresh_scp_completions(&mut self, buffer: &str, cursor: usize) {
        self.is_loading_remote = false;
        let Some(query) = scp_completion::analyse_at(buffer, cursor) else {
            self.path_completions.clear();
            return;
        };

        match &query.phase {
            ScpCompletionPhase::HostOrLocal { prefix } => {
                // 远程终端：host 候选 + 当前终端的远程文件；
                // 本地终端：host 候选 + 本机文件。
                if let Some(terminal_cfg) = self.terminal_ssh.clone() {
                    let mut items = build_host_only_completions(prefix, &self.ssh_saved_configs);
                    items.extend(self.compute_bare_remote_completion_items(
                        prefix,
                        terminal_cfg,
                        None,
                    ));
                    self.path_completions = items;
                    self.path_selected_index = None;
                } else {
                    // 主机候选无 IO，先显示；本机文件后台读取后追加。
                    let hosts = build_host_only_completions(prefix, &self.ssh_saved_configs);
                    self.path_completions = if hosts.is_empty() {
                        vec![local_loading_item()]
                    } else {
                        hosts
                    };
                    self.path_selected_index = None;
                    self.request_local_completions(prefix, LocalCompletionContext::ScpHostOrLocal);
                }
            }
            ScpCompletionPhase::RemotePath { host_token, prefix } => {
                // 端口语法 `user@host:port:path`：analyse_at 按第一个冒号切分，
                // host_token 丢失端口、prefix 以端口开头。若 prefix 以 `<数字>:` 开头，
                // 先把端口并回 host 再匹配；匹配不上则不展示候选
                // （避免按裸 host 误补全，也避免 `user@localhost:2222:...` 被当成
                // 本机路径补全）。
                let (host_token, prefix) = match scp_completion::split_port_prefix(prefix) {
                    Some((port, rest)) => (format!("{}:{}", host_token, port), rest.to_string()),
                    None => (host_token.to_string(), prefix.to_string()),
                };
                let host_token = host_token.as_str();
                let prefix = prefix.as_str();

                // 关键字 `Local` —— 始终指向"运行 DeepTerm 的本机"
                // （端口重映射之后才检查：`user@localhost:2222` 不是本机）
                if scp_completion::is_local_keyword_str(host_token) {
                    self.path_completions = vec![local_loading_item()];
                    self.path_selected_index = None;
                    self.request_local_completions(
                        prefix,
                        LocalCompletionContext::ScpLocalPath {
                            host_token: host_token.to_string(),
                        },
                    );
                    return;
                }

                let matched =
                    scp_completion::match_hosts_by_prefix(host_token, &self.ssh_saved_configs);
                let exact = matched.iter().find(|h| {
                    let cfg = match h.config {
                        Some(c) => c,
                        None => return false,
                    };
                    h.display.eq_ignore_ascii_case(host_token)
                        || cfg.host_label().eq_ignore_ascii_case(host_token)
                        || cfg.connection_string().eq_ignore_ascii_case(host_token)
                        || cfg
                            .nickname
                            .as_deref()
                            .map(|n| n.eq_ignore_ascii_case(host_token))
                            .unwrap_or(false)
                        || cfg.host.eq_ignore_ascii_case(host_token)
                });
                let Some(host) = exact else {
                    self.path_completions.clear();
                    return;
                };
                let host_cfg = match host.config {
                    Some(c) => c,
                    None => {
                        self.path_completions.clear();
                        return;
                    }
                };
                let (absolute_dir, file_prefix) = split_remote_dir_and_prefix(prefix);
                let key = (host_cfg.id(), absolute_dir.clone());

                // 用统一的 `user@host[:port]` 形式作为补全 replacement 的 host 部分，
                // 即便用户当前输入的是 nickname / host，命中后仍以标准格式回写。
                let canonical_host = host_cfg.host_label();

                match self.remote_dir_cache.get(&key).cloned() {
                    Some(cached) => {
                        // 有缓存：立即显示，同时注册后台刷新请求
                        self.remote_dir_cache_lru.retain(|k| k != &key);
                        self.remote_dir_cache_lru.push(key.clone());
                        self.path_completions = build_remote_completions(
                            &cached.items,
                            &file_prefix,
                            Some(canonical_host.as_str()),
                            prefix,
                        );
                        // 注册后台刷新（不设置 is_loading_remote，避免 UI 显示加载中）
                        self.register_remote_dir_load(key, host_cfg.clone());
                    }
                    None => {
                        if let Some(err) = self.failed_remote_keys.remove(&key) {
                            // 上次读取失败：显示一次错误占位，不重新注册请求（断开重试循环）。
                            // 下一次用户输入触发刷新时标记已消费，会重新注册请求自动重试。
                            self.path_completions = vec![remote_dir_failed_item(err)];
                        } else {
                            // 无缓存：显示加载中，注册请求
                            self.is_loading_remote = true;
                            self.register_remote_dir_load(key, host_cfg.clone());
                            self.path_completions = vec![remote_dir_loading_item(absolute_dir)];
                        }
                    }
                }
            }
        }
        self.path_selected_index = None;
    }

    /// `/scp` 在"远程终端 + 没有显式 host"时的补全：直接走当前终端 SSH 的 SFTP，
    /// replacement 写裸路径（不加 `host:` 前缀），保持"裸路径跟随当前终端所在主机"。
    ///
    /// `extra_host_quoted` 是供未来扩展用的可选 host 前缀；当前调用方传 `None`。
    fn refresh_bare_remote_completions(
        &mut self,
        prefix: &str,
        terminal_cfg: crate::ssh::SshConnectionConfig,
        extra_host_quoted: Option<&str>,
    ) {
        self.path_completions =
            self.compute_bare_remote_completion_items(prefix, terminal_cfg, extra_host_quoted);
        self.path_selected_index = None;
    }

    /// 与 `refresh_bare_remote_completions` 同语义，但只返回 items，由调用方负责合并 / 写回。
    /// 用于 HostOrLocal 阶段把 host 候选与当前终端远程文件拼到一起。
    fn compute_bare_remote_completion_items(
        &mut self,
        prefix: &str,
        terminal_cfg: crate::ssh::SshConnectionConfig,
        extra_host_quoted: Option<&str>,
    ) -> Vec<PathCompletionItem> {
        let base_cwd = self.terminal_remote_cwd.as_deref();
        let (absolute_dir, file_prefix) = split_remote_dir_for_bare(prefix, base_cwd);
        let key = (terminal_cfg.id(), absolute_dir.clone());

        match self.remote_dir_cache.get(&key).cloned() {
            Some(cached) => {
                // 有缓存：立即显示
                self.remote_dir_cache_lru.retain(|k| k != &key);
                self.remote_dir_cache_lru.push(key.clone());
                // 注册后台刷新
                self.register_remote_dir_load(key, terminal_cfg);
                build_remote_completions(&cached.items, &file_prefix, extra_host_quoted, prefix)
            }
            None => {
                if let Some(err) = self.failed_remote_keys.remove(&key) {
                    // 上次读取失败：一次性错误占位，不重新注册请求（断开重试循环）
                    vec![remote_dir_failed_item(err)]
                } else {
                    // 无缓存：显示加载中
                    self.is_loading_remote = true;
                    self.register_remote_dir_load(key, terminal_cfg);
                    vec![remote_dir_loading_item(absolute_dir)]
                }
            }
        }
    }
}

impl Default for BuiltinCommandManager {
    fn default() -> Self {
        Self::new()
    }
}

fn is_scp_command(buffer: &str) -> bool {
    buffer
        .split_whitespace()
        .next()
        .map(|name| name == "/scp")
        .unwrap_or(false)
}

/// 通用命令（`/open` 等单参数命令）下，光标所在参数 token 的字节范围。
///
/// 把 `command_name` 之后的参数区按空白切分，返回包含 `cursor` 的那个 token 的
/// `(start, end)`（绝对字节偏移）。光标落在空白 / 空位时返回 `(cursor, cursor)`，
/// 表示一个空 token，从而补全当前目录。
pub fn generic_arg_token_range_at(buffer: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(buffer.len());
    // 命令名结束位置（第一个空白）
    let after_cmd = buffer
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8());
    let Some(after_cmd) = after_cmd else {
        // 还没有空格：整段都是命令名，没有参数 token
        return (buffer.len(), buffer.len());
    };
    if cursor < after_cmd {
        // 光标还在命令名里
        return (after_cmd, after_cmd);
    }
    let args_area = &buffer[after_cmd..];
    let tokens = scp_completion::shell_split_tokens(args_area, after_cmd);
    for t in &tokens {
        if cursor >= t.start && cursor <= t.end {
            return (t.start, t.end);
        }
    }
    // 落在 token 之间的空白：空 token
    (cursor, cursor)
}

/// 通用命令下，光标所在参数 token 去引号后的文本（作为补全前缀）。
fn generic_arg_prefix_at(buffer: &str, cursor: usize) -> String {
    let (start, end) = generic_arg_token_range_at(buffer, cursor);
    scp_completion::unquote_raw(&buffer[start..end])
}

/// 远端目录尚未加载时的占位补全项。
fn remote_dir_loading_item(absolute_dir: String) -> PathCompletionItem {
    PathCompletionItem {
        name: "读取远端目录中…".to_string(),
        path: PathBuf::new(),
        is_directory: false,
        kind: CompletionKind::File,
        description: Some(absolute_dir),
        icon: Some(crate::assets::Icon::ProgressIndicator),
        replacement: None,
    }
}

/// 远端目录读取失败时的一次性占位补全项（不写缓存、不重新注册请求）。
fn remote_dir_failed_item(err: String) -> PathCompletionItem {
    PathCompletionItem {
        name: "无法读取远端目录".to_string(),
        path: PathBuf::new(),
        is_directory: false,
        kind: CompletionKind::File,
        description: Some(err),
        icon: None,
        replacement: None,
    }
}

/// 把本机文件候选拼进 `HostOrLocal` 列表（不读盘，供后台结果回写复用）。
fn map_host_or_local_items(
    prefix: &str,
    local_items: Vec<PathCompletionItem>,
) -> Vec<PathCompletionItem> {
    local_items
        .into_iter()
        .map(|item| {
            let replacement = compose_local_replacement(prefix, &item);
            PathCompletionItem {
                replacement: Some(replacement),
                description: Some(if item.is_directory {
                    "Directory".to_string()
                } else {
                    item.path.to_string_lossy().into_owned()
                }),
                icon: None,
                ..item
            }
        })
        .collect()
}

/// 本地目录读取中的非交互占位项。
fn local_loading_item() -> PathCompletionItem {
    PathCompletionItem {
        name: "读取目录中…".to_string(),
        path: PathBuf::new(),
        is_directory: false,
        kind: CompletionKind::Directory,
        description: None,
        icon: None,
        replacement: None,
    }
}

/// 仅生成 host 候选（含 `Local` 关键字 + 已保存连接），不附本机/远端文件。
/// 远程终端的 HostOrLocal 阶段会用它先列 host，再追加 SFTP 远程文件。
fn build_host_only_completions(
    prefix: &str,
    saved: &[crate::ssh::SshConnectionConfig],
) -> Vec<PathCompletionItem> {
    let mut items: Vec<PathCompletionItem> = Vec::new();

    // 主机候选：prefix 呈路径形态（以 `/`、`~`、`.` 开头或包含 `/`）时跳过
    // —— 主机名 / label 里不可能出现这些字符
    let looks_like_path = prefix.starts_with('/')
        || prefix.starts_with('~')
        || prefix.starts_with('.')
        || prefix.contains('/');
    if !looks_like_path {
        for suggestion in scp_completion::match_hosts_by_prefix(prefix, saved) {
            // description：保留 nickname 等可读名让用户区分；Local 标注 "This machine"
            let description = match suggestion.kind {
                scp_completion::HostSuggestionKind::Local => Some("This machine".to_string()),
                _ => suggestion
                    .config
                    .and_then(|cfg| cfg.nickname.clone())
                    .filter(|nick| nick != &suggestion.replacement),
            };
            // host token 含 `:`（带端口）或空格 → 必须整体加引号，否则提交时
            // first_unquoted_colon 会在 `host:port` 之间切
            let quoted_host = quote_host_token(&suggestion.replacement);
            let replacement = format!("{}:~/", quoted_host);
            items.push(PathCompletionItem {
                // 显示统一 user@host[:port]，让命令行预览与最终结果一致
                name: suggestion.replacement.clone(),
                path: PathBuf::new(),
                is_directory: false,
                kind: CompletionKind::Host,
                description,
                icon: suggestion.icon,
                replacement: Some(replacement),
            });
        }
    }

    items
}

fn compose_local_replacement(prefix: &str, item: &PathCompletionItem) -> String {
    let (dir_part, _) = PathCompletionEngine::split_path_prefix(prefix);
    let raw = if item.is_directory {
        format!("{}{}/", dir_part, item.name)
    } else {
        format!("{}{}", dir_part, item.name)
    };
    // 路径含空格/特殊字符时自动加引号
    scp_completion::shell_quote(&raw)
}

/// 把本机文件候选拼回 `<host_token>:path`（不读盘，供后台结果回写复用）。
fn map_local_path_items(
    prefix: &str,
    host_token: &str,
    local_items: Vec<PathCompletionItem>,
) -> Vec<PathCompletionItem> {
    let host_quoted = quote_host_token(host_token);
    local_items
        .into_iter()
        .map(|item| {
            let path_segment = compose_local_replacement(prefix, &item);
            // path_segment 已经按需要包好引号；与 host 部分独立量化，`:` 永远在外
            let replacement = format!("{}:{}", host_quoted, path_segment);
            PathCompletionItem {
                name: item.name.clone(),
                path: item.path.clone(),
                is_directory: item.is_directory,
                kind: if item.is_directory {
                    CompletionKind::Directory
                } else {
                    CompletionKind::File
                },
                description: Some(if item.is_directory {
                    "Directory".to_string()
                } else {
                    item.path.to_string_lossy().into_owned()
                }),
                icon: None,
                replacement: Some(replacement),
            }
        })
        .collect()
}

/// 把远端 path 前缀拆成 "用于查 read_dir 的目录" 和 "过滤用的文件名前缀"。
///
/// 例：
/// - `/var/lo` → (`/var`, `lo`)
/// - `/var/log/` → (`/var/log`, "")
/// - `~/` → (`~`, "")（外部解析 `~` 为远端家目录绝对路径）
/// - 空串 → (`~`, "")
fn split_remote_dir_and_prefix(prefix: &str) -> (String, String) {
    if prefix.is_empty() {
        return ("~".to_string(), String::new());
    }
    if let Some(pos) = prefix.rfind('/') {
        let dir = &prefix[..=pos]; // 包含 '/'
        let file = &prefix[pos + 1..];
        let dir = dir.trim_end_matches('/');
        let dir = if dir.is_empty() {
            "/".to_string()
        } else {
            dir.to_string()
        };
        (dir, file.to_string())
    } else if prefix.starts_with('~') {
        // `~abc` —— 视作相对家目录
        ("~".to_string(), prefix.to_string())
    } else {
        // 没有分隔符：路径相对登录家目录，如 "log"
        ("~".to_string(), prefix.to_string())
    }
}

/// `split_remote_dir_and_prefix` 的"裸路径"版本：当远程终端没有显式 host 时，
/// 用户在打的相对路径（如 `log`、`./logs/foo`）应当以**当前 shell 的 cwd** 为基准，
/// 而不是登录家目录。绝对路径与 `~/...` 维持原有语义。
fn split_remote_dir_for_bare(prefix: &str, base_cwd: Option<&str>) -> (String, String) {
    let base = base_cwd.unwrap_or("~");
    if prefix.is_empty() {
        return (base.to_string(), String::new());
    }
    if let Some(stripped) = prefix.strip_prefix("~/") {
        // `~/...` 显式指家目录，下钻：dir/file 拆分以 home 为根
        let (dir, file) = split_relative(stripped);
        let dir_full = if dir.is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", dir.trim_end_matches('/'))
        };
        return (dir_full, file);
    }
    if prefix == "~" {
        return ("~".to_string(), String::new());
    }
    if prefix.starts_with('/') {
        // 绝对路径：跟原来同样规则
        return split_remote_dir_and_prefix(prefix);
    }
    // 相对路径（含 `./xx`）：以 base_cwd 为根拼出绝对目录前缀
    let stripped = prefix.strip_prefix("./").unwrap_or(prefix);
    let (rel_dir, file) = split_relative(stripped);
    let dir_full = if rel_dir.is_empty() {
        base.to_string()
    } else {
        crate::ssh::sftp_fs::join_remote_path(base, rel_dir.trim_end_matches('/'))
    };
    (dir_full, file)
}

/// 把"无前导斜杠的相对路径"拆成 `(dir_part_without_trailing_slash, file_prefix)`。
fn split_relative(rel: &str) -> (String, String) {
    if let Some(pos) = rel.rfind('/') {
        (rel[..pos].to_string(), rel[pos + 1..].to_string())
    } else {
        (String::new(), rel.to_string())
    }
}

/// 用加载到的远端条目构造 PathCompletionItem 列表。
///
/// `host_token = Some(...)`：作为 `host:path` 的 host 前缀写入 replacement
/// （用于已经输入了 `host:` 的 RemotePath 阶段）。
/// `host_token = None`：replacement 为裸路径，不带 `host:` 前缀
/// （远程终端的 HostOrLocal 阶段：用户直接打路径，跟随当前终端所在主机）。
fn build_remote_completions(
    entries: &[RemoteEntrySnapshot],
    file_prefix: &str,
    host_token: Option<&str>,
    current_path_prefix: &str,
) -> Vec<PathCompletionItem> {
    let prefix_lower = file_prefix.to_lowercase();
    // 传给 split_path_prefix 的"目录部分"应当与用户已经输入的内容保持字面一致
    // —— 否则补全后的路径会跟光标处前缀不一致
    let (dir_part_literal, _) = PathCompletionEngine::split_path_prefix(current_path_prefix);
    let host_quoted = host_token.map(quote_host_token);
    let mut items: Vec<(PathCompletionItem, bool)> = entries
        .iter()
        .filter_map(|entry| {
            // 过滤隐藏文件，除非用户显式输入 '.'
            if entry.name.starts_with('.') && !prefix_lower.starts_with('.') {
                return None;
            }
            // Use case-insensitive prefix check without allocating
            if !entry.name.starts_with(&prefix_lower)
                && !entry.name.to_lowercase().starts_with(&prefix_lower)
            {
                return None;
            }
            let new_path_segment = if entry.is_directory {
                format!("{}{}/", dir_part_literal, entry.name)
            } else {
                format!("{}{}", dir_part_literal, entry.name)
            };
            // host 和 path 独立量化，`:` 必须留在引号外，否则 `first_unquoted_colon`
            // 会把整段识别成本地路径。裸路径模式下若整体含空格仍需引号包裹。
            let path_quoted = if needs_shell_quote(&new_path_segment) {
                scp_completion::shell_quote(&new_path_segment)
            } else {
                new_path_segment.clone()
            };
            let replacement = match host_quoted.as_deref() {
                Some(h) => format!("{}:{}", h, path_quoted),
                None => path_quoted,
            };
            let description = if entry.is_directory {
                Some("Directory".to_string())
            } else {
                Some(entry.full_path.clone())
            };
            Some((
                PathCompletionItem {
                    name: entry.name.clone(),
                    path: PathBuf::from(&entry.full_path),
                    is_directory: entry.is_directory,
                    kind: if entry.is_directory {
                        CompletionKind::Directory
                    } else {
                        CompletionKind::File
                    },
                    description,
                    icon: None,
                    replacement: Some(replacement),
                },
                entry.is_directory,
            ))
        })
        .collect();

    // 目录优先，同类按字母序
    items.sort_by(|a, b| match (b.1, a.1) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => a.0.name.cmp(&b.0.name),
    });
    items.into_iter().map(|(item, _)| item).collect()
}

/// 判断字符串是否需要 shell 引号包裹（含空格、制表符或引号字符）。
fn needs_shell_quote(s: &str) -> bool {
    s.chars().any(|c| matches!(c, ' ' | '\t' | '"' | '\''))
}

/// host token 在拼到 `host:path` 时若自身含 `:`（带端口）或空格 / 引号，
/// 必须整体引号化；否则 `first_unquoted_colon` 会切错。
///
/// 复用 `shell_quote`：当 host 含 `:` 时强制加引号（即使不含空格），
/// 因为 `first_unquoted_colon` 会把 `user@host:port` 中的 `:` 识别为分隔符。
fn quote_host_token(host: &str) -> String {
    if host.contains(':') {
        // 强制引号包裹（shell_quote 对纯 ASCII 无空格字符串不会加引号）
        let mut out = String::with_capacity(host.len() + 2);
        out.push('"');
        for c in host.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        scp_completion::shell_quote(host)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinCommandState {
    buffer: String,
    /// 光标字节偏移，恒落在字符（grapheme）边界。新建 / set_buffer 后等于 buffer.len()。
    cursor: usize,
}

impl BuiltinCommandState {
    /// 用初始文本创建命令缓冲区，光标置于末尾。
    pub fn new(buffer: String) -> Self {
        let cursor = buffer.len();
        Self { buffer, cursor }
    }

    /// 设置光标字节偏移。
    ///
    /// 会 clamp 到缓冲长度内，并回退到最近的 UTF-8 字符边界，
    /// 避免远端同步的非法偏移导致后续 `insert_str` / 删除操作 panic。
    pub fn set_cursor(&mut self, cursor: usize) {
        let mut pos = cursor.min(self.buffer.len());
        while pos > 0 && !self.buffer.is_char_boundary(pos) {
            pos -= 1;
        }
        self.cursor = pos;
    }

    /// 在光标处插入文本，光标右移到插入串末尾。
    pub fn insert(&mut self, text: &str) {
        self.buffer.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// 删除光标前一个 grapheme（退格），光标左移。返回是否删除了内容。
    pub fn delete_backward(&mut self) -> bool {
        let Some(prev) = self.prev_boundary(self.cursor) else {
            return false;
        };
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// 删除光标后一个 grapheme（Delete 键）。返回是否删除了内容。
    pub fn delete_forward(&mut self) -> bool {
        let Some(next) = self.next_boundary(self.cursor) else {
            return false;
        };
        self.buffer.replace_range(self.cursor..next, "");
        true
    }

    /// 删除光标前的一个「词」（Opt+Backspace）。返回是否删除了内容。
    pub fn delete_word_backward(&mut self) -> bool {
        let target = self.prev_word_boundary(self.cursor);
        if target == self.cursor {
            return false;
        }
        self.buffer.replace_range(target..self.cursor, "");
        self.cursor = target;
        true
    }

    /// 光标左移一个 grapheme。返回是否发生移动。
    pub fn move_left(&mut self) -> bool {
        match self.prev_boundary(self.cursor) {
            Some(prev) => {
                self.cursor = prev;
                true
            }
            None => false,
        }
    }

    /// 光标右移一个 grapheme。返回是否发生移动。
    pub fn move_right(&mut self) -> bool {
        match self.next_boundary(self.cursor) {
            Some(next) => {
                self.cursor = next;
                true
            }
            None => false,
        }
    }

    /// 光标按词左移（Opt+←）。返回是否发生移动。
    pub fn move_word_left(&mut self) -> bool {
        let target = self.prev_word_boundary(self.cursor);
        if target == self.cursor {
            return false;
        }
        self.cursor = target;
        true
    }

    /// 光标按词右移（Opt+→）。返回是否发生移动。
    pub fn move_word_right(&mut self) -> bool {
        let target = self.next_word_boundary(self.cursor);
        if target == self.cursor {
            return false;
        }
        self.cursor = target;
        true
    }

    /// 光标移到行首（Home / Cmd+←）。返回是否发生移动。
    pub fn move_home(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = 0;
        true
    }

    /// 光标移到行尾（End / Cmd+→）。返回是否发生移动。
    pub fn move_end(&mut self) -> bool {
        if self.cursor == self.buffer.len() {
            return false;
        }
        self.cursor = self.buffer.len();
        true
    }

    /// 判断缓冲区是否为空。
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 返回当前缓冲区文本。
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// 当前光标字节偏移。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 替换整个缓冲内容，光标钉到末尾。
    pub fn set_buffer(&mut self, buffer: String) {
        self.cursor = buffer.len();
        self.buffer = buffer;
    }

    /// 替换整个缓冲内容，并把光标显式置于 `cursor`（夹紧到合法字符边界范围内）。
    pub fn set_buffer_with_cursor(&mut self, buffer: String, cursor: usize) {
        let cursor = cursor.min(buffer.len());
        // 夹紧到字符边界，避免落在 UTF-8 多字节序列中间。
        let cursor = if buffer.is_char_boundary(cursor) {
            cursor
        } else {
            buffer[..cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
        self.cursor = cursor;
        self.buffer = buffer;
    }

    /// 光标左侧的一个 grapheme 边界（字节偏移），已在行首返回 `None`。
    fn prev_boundary(&self, offset: usize) -> Option<usize> {
        if offset == 0 {
            return None;
        }
        self.buffer[..offset]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
    }

    /// 光标右侧的一个 grapheme 边界（字节偏移），已在行尾返回 `None`。
    fn next_boundary(&self, offset: usize) -> Option<usize> {
        if offset >= self.buffer.len() {
            return None;
        }
        self.buffer[offset..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(i, _)| offset + i)
            .or(Some(self.buffer.len()))
    }

    /// 向左跳过空白后再跳过一段非空白，得到「词首」字节偏移。
    fn prev_word_boundary(&self, offset: usize) -> usize {
        let mut pos = offset;
        // 先吃掉光标左侧的空白
        while pos > 0 {
            let prev = self.prev_boundary(pos).unwrap_or(0);
            if self.buffer[prev..pos].chars().all(char::is_whitespace) {
                pos = prev;
            } else {
                break;
            }
        }
        // 再吃掉一段非空白
        while pos > 0 {
            let prev = self.prev_boundary(pos).unwrap_or(0);
            if self.buffer[prev..pos].chars().any(char::is_whitespace) {
                break;
            }
            pos = prev;
        }
        pos
    }

    /// 向右跳过非空白后再跳过一段空白，得到下一个「词尾」字节偏移。
    fn next_word_boundary(&self, offset: usize) -> usize {
        let len = self.buffer.len();
        let mut pos = offset;
        // 先吃掉一段非空白
        while pos < len {
            let next = self.next_boundary(pos).unwrap_or(len);
            if self.buffer[pos..next].chars().any(char::is_whitespace) {
                break;
            }
            pos = next;
        }
        // 再吃掉光标右侧的空白
        while pos < len {
            let next = self.next_boundary(pos).unwrap_or(len);
            if self.buffer[pos..next].chars().all(char::is_whitespace) {
                pos = next;
            } else {
                break;
            }
        }
        pos
    }
}

/// 根据命令行首个 token 查找内建命令定义。
///
/// 只匹配命令名本身，后续参数不会参与查找。
pub fn find_builtin_command(input: &str) -> Option<Arc<BuiltinCommand>> {
    let command_name = input.split_whitespace().next()?;
    global_registry().find(command_name)
}

/// 根据当前查询生成建议列表，并按匹配分数从高到低排序。
///
/// 根据当前输入和选中项，生成最终要提交的命令行。
///
/// `force_selected = true` 用于显式选择场景（如鼠标点击），允许直接用选中项覆盖当前前缀。
pub fn resolve_command_line(
    query: &str,
    selected: Option<&BuiltinCommand>,
    force_selected: bool,
) -> String {
    let Some(command_name) = query.split_whitespace().next() else {
        return query.to_string();
    };
    let Some(selected) = selected else {
        return query.to_string();
    };

    if !force_selected && !selected.name.starts_with(command_name) {
        return query.to_string();
    }

    let suffix = query.strip_prefix(command_name).unwrap_or_default();
    format!("{}{}", selected.name, suffix)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
