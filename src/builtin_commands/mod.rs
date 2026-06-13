use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    Transformation,
};
use gpui::{MouseButton, Pixels, ScrollHandle, div, ease_in_out, percentage, px, svg};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::UiTheme;

pub mod argument;
pub mod command_model;
pub mod executor;
pub mod history;
pub mod path_completion;
pub mod path_resolver;
pub mod registry;
pub mod scp_completion;
pub mod suggestion_engine;
pub use argument::{ArgumentError, ArgumentParser, ParsedArgument};
pub use command_model::{BuiltinCommandModel, CommandEntryState, DetectedCommand};
pub use executor::{CommandContext, CommandExecutor, CommandHandler, CommandResult};
pub use history::{CommandHistory, HistoryEntry};
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

    /// 回车时是否应把缓冲交回 shell（放弃内建语义）
    pub fn should_fallthrough_to_shell(self) -> bool {
        matches!(self, Self::DismissedUntilReset)
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
    /// 已经向外部发起、等待结果的远端目录请求集合
    pending_remote_keys: HashMap<(String, String), bool>,
    /// 上次读取失败、尚未向用户展示过错误的远端目录 key → 错误信息。
    /// cache-miss 分支会消费该标记显示一次错误占位（且不重新注册请求，避免
    /// 失败→刷新→重新请求→再失败的无限循环）；下一次用户输入触发的刷新由于
    /// 标记已被消费，会重新注册请求自动重试。
    failed_remote_keys: HashMap<(String, String), String>,
    /// `/scp` 第二段是否处于"等待远端结果"状态
    pub is_loading_remote: bool,
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

/// 统一选中项：落在「历史段」还是「原候选段」。
///
/// 两个阶段的选中索引都按「历史在前、原候选在后」的组合空间编码：
/// `[0..H)` 为历史候选，`[H..)` 为原候选（命令名 / 路径）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectedKind {
    /// 历史候选下标（指向 `history_matches`）
    History(usize),
    /// 原候选下标（指向 `suggestions` 或 `path_completions`，已减去历史段偏移）
    Base(usize),
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteCompletionRequest {
    pub config_id: String,
    pub absolute_dir: String,
}

impl BuiltinCommandManager {
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
            remote_dir_cache: HashMap::new(),
            remote_dir_cache_lru: Vec::new(),
            pending_remote_keys: HashMap::new(),
            failed_remote_keys: HashMap::new(),
            is_loading_remote: false,
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
        let total = self.history_matches.len();
        if total == 0 {
            return None;
        }
        self.history_selected_index.map(|i| i.min(total - 1))
    }

    /// 按当前缓冲重算历史候选；缓冲为空时清空。
    fn refresh_history_matches(&mut self) {
        let now = history::current_unix_secs();
        self.history_matches = match self.query() {
            Some(q) if !q.trim().is_empty() => self.history.matches(q, now),
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
                    // LRU 淘汰
                    self.remote_dir_cache_lru.retain(|k| k != &key);
                    self.remote_dir_cache_lru.push(key.clone());
                    while self.remote_dir_cache_lru.len() > 128 {
                        let oldest = self.remote_dir_cache_lru.remove(0);
                        self.remote_dir_cache.remove(&oldest);
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
    pub fn pending_remote_request(&mut self) -> Option<RemoteCompletionRequest> {
        for ((cfg, dir), pending) in self.pending_remote_keys.iter_mut() {
            if !*pending {
                *pending = true;
                return Some(RemoteCompletionRequest {
                    config_id: cfg.clone(),
                    absolute_dir: dir.clone(),
                });
            }
        }
        None
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
            let is_dir_expanding = self.query().map_or(false, |b| b.ends_with('/'));
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
        if buffer.chars().last().map_or(false, char::is_whitespace) {
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
        // /scp 期望 2 个参数 ⇒ 总 token 数 3 时已满
        let total_tokens = scp_completion::shell_split_tokens(
            buffer
                .splitn(2, |c: char| c.is_whitespace())
                .nth(1)
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
            .splitn(2, |c: char| c.is_whitespace())
            .nth(1)
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
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let current_input = state.buffer().to_string();
        let cursor = state.cursor();

        // `/scp` 走独立分支：补全项自带 `replacement`（可能含 host 前缀）
        if is_scp_command(&current_input) {
            if let Some(replacement) = selected.replacement.clone() {
                if let Some(query) = scp_completion::analyse_at(&current_input, cursor) {
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
            }
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
        let total = self.suggestions.len();
        if total == 0 {
            return None;
        }
        self.selected_index.map(|i| i.min(total - 1))
    }

    /// 获取当前路径建议的高亮索引（`None` ⇒ 未高亮）。
    pub fn selected_path_index(&self) -> Option<usize> {
        let total = self.path_completions.len();
        if total == 0 {
            return None;
        }
        self.path_selected_index.map(|i| i.min(total - 1))
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
        let Some(query) = self.query().map(str::to_string) else {
            self.path_completions.clear();
            self.is_loading_remote = false;
            return;
        };
        // 补全焦点跟随光标所在 token；无活跃 state 时退化为末尾。
        let cursor = self.cursor().unwrap_or(query.len());

        // `/scp` 专用分支：两个参数、可能是主机、远端目录
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

        // 本地终端：列出本机文件
        self.is_loading_remote = false;
        self.path_completions =
            PathCompletionEngine::complete(&effective_prefix, self.cwd.as_deref());
        self.path_selected_index = None;
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
                    self.path_completions = build_host_or_local_completions(
                        prefix,
                        self.cwd.as_deref(),
                        &self.ssh_saved_configs,
                    );
                }
            }
            ScpCompletionPhase::RemotePath { host_token, prefix } => {
                // 关键字 `Local` —— 始终指向"运行 DeepTerm 的本机"
                if scp_completion::is_local_keyword_str(host_token) {
                    self.path_completions =
                        build_local_path_completions(prefix, host_token, self.cwd.as_deref());
                    self.path_selected_index = None;
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
                        self.pending_remote_keys.entry(key).or_insert(false);
                    }
                    None => {
                        if let Some(err) = self.failed_remote_keys.remove(&key) {
                            // 上次读取失败：显示一次错误占位，不重新注册请求（断开重试循环）。
                            // 下一次用户输入触发刷新时标记已消费，会重新注册请求自动重试。
                            self.path_completions = vec![remote_dir_failed_item(err)];
                        } else {
                            // 无缓存：显示加载中，注册请求
                            self.is_loading_remote = true;
                            self.pending_remote_keys.entry(key).or_insert(false);
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
                self.pending_remote_keys.entry(key).or_insert(false);
                build_remote_completions(&cached.items, &file_prefix, extra_host_quoted, prefix)
            }
            None => {
                if let Some(err) = self.failed_remote_keys.remove(&key) {
                    // 上次读取失败：一次性错误占位，不重新注册请求（断开重试循环）
                    vec![remote_dir_failed_item(err)]
                } else {
                    // 无缓存：显示加载中
                    self.is_loading_remote = true;
                    self.pending_remote_keys.entry(key).or_insert(false);
                    vec![remote_dir_loading_item(absolute_dir)]
                }
            }
        }
    }

    /// 获取参数提示文本（如 "file_path"）。
    pub fn argument_hint(&self) -> Option<&'static str> {
        self.resolved_command
            .as_ref()
            .and_then(|cmd| match cmd.argument {
                ArgumentType::Optional { hint } => hint,
                ArgumentType::Required { hint } => Some(hint),
                ArgumentType::None => None,
            })
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

/// Host 条目展示在 IP 右侧的次要文字（nickname / `Local` 标签）；
/// 文件 / 目录条目返回 `None`，让 UI 只渲染文件名。
fn host_description(item: &PathCompletionItem) -> Option<String> {
    if item.kind != CompletionKind::Host {
        return None;
    }
    item.description.clone()
}

/// 估算路径补全浮层应当占用的宽度，让最长的"name + 描述"在 UI 边界以内完整展示。
///
/// 以字符宽度粗算（monospace 13.5px ≈ 8.5px/char、proportional 12.5px ≈ 7px/char），
/// 再加上 padding/icon/gap 的固定开销。算出的值会被 [`OVERLAY_MIN_WIDTH`] /
/// [`OVERLAY_MAX_WIDTH`] 夹紧，避免极端情况撑爆窗口。
fn estimate_overlay_width(items: &[PathCompletionItem]) -> f32 {
    // 横向布局：padding(10*2) + icon(16) + icon→name gap(10) + name→desc gap(10) + 余量(12)
    const FIXED_CHROME: f32 = 10.0 + 10.0 + 16.0 + 10.0 + 12.0;
    const NAME_CHAR: f32 = 8.5;
    const DESC_CHAR: f32 = 7.0;

    let needed = items
        .iter()
        .map(|item| {
            let name_w = item.name.chars().count() as f32 * NAME_CHAR;
            let desc_w = host_description(item)
                .map(|d| 10.0 + d.chars().count() as f32 * DESC_CHAR)
                .unwrap_or(0.0);
            FIXED_CHROME + name_w + desc_w
        })
        .fold(OVERLAY_MIN_WIDTH, f32::max);

    needed.min(OVERLAY_MAX_WIDTH)
}

const OVERLAY_MIN_WIDTH: f32 = 320.0;
const OVERLAY_MAX_WIDTH: f32 = 900.0;

/// `HostOrLocal` 阶段：合并"匹配的主机"与"本地路径补全"为一份列表。
fn build_host_or_local_completions(
    prefix: &str,
    cwd: Option<&std::path::Path>,
    saved: &[crate::ssh::SshConnectionConfig],
) -> Vec<PathCompletionItem> {
    let mut items: Vec<PathCompletionItem> = build_host_only_completions(prefix, saved);

    // 本地路径补全（空 prefix 时列出当前目录，方便用户直接选择本地文件）
    for item in PathCompletionEngine::complete(prefix, cwd) {
        let replacement = compose_local_replacement(prefix, &item);
        items.push(PathCompletionItem {
            replacement: Some(replacement),
            description: Some(if item.is_directory {
                "Directory".to_string()
            } else {
                item.path.to_string_lossy().into_owned()
            }),
            icon: None,
            ..item
        });
    }

    items
}

/// 仅生成 host 候选（含 `Local` 关键字 + 已保存连接），不附本机/远端文件。
/// 远程终端的 HostOrLocal 阶段会用它先列 host，再追加 SFTP 远程文件。
fn build_host_only_completions(
    prefix: &str,
    saved: &[crate::ssh::SshConnectionConfig],
) -> Vec<PathCompletionItem> {
    let mut items: Vec<PathCompletionItem> = Vec::new();

    // 主机：按 prefix 匹配；prefix 以 `/` 或 `~` 开头则跳过主机匹配（明显是路径）
    if !prefix.starts_with('/') && !prefix.starts_with('~') && !prefix.starts_with('.') {
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

/// `Local:` 前缀阶段：列出本机文件，并把每个候选 token 重新拼成 `<host_token>:path`。
///
/// `host_token` 是用户当前在缓冲中输入的 host 部分（可能是 `Local`、
/// `user@localhost` 等），保持原字面量以避免每次按键都改写光标处文本。
fn build_local_path_completions(
    prefix: &str,
    host_token: &str,
    cwd: Option<&std::path::Path>,
) -> Vec<PathCompletionItem> {
    let host_quoted = quote_host_token(host_token);
    let mut out: Vec<PathCompletionItem> = Vec::new();
    for item in PathCompletionEngine::complete(prefix, cwd) {
        let path_segment = compose_local_replacement(prefix, &item);
        // path_segment 已经按需要包好引号；与 host 部分独立量化，`:` 永远在外
        let replacement = format!("{}:{}", host_quoted, path_segment);
        out.push(PathCompletionItem {
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
        });
    }
    out
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
            if !entry.name.to_lowercase().starts_with(&prefix_lower) {
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
/// 优先级依次是：精确匹配、前缀匹配、名称子串匹配、描述子串匹配。
pub fn command_suggestions(query: &str) -> Vec<Arc<BuiltinCommand>> {
    let all_commands = global_registry().all_commands();
    let scored = SuggestionEngine::suggest(&all_commands, query);
    scored.into_iter().map(|s| s.command).collect()
}

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

fn render_key_hint(key: &str, label: Option<&str>, theme: UiTheme) -> impl IntoElement {
    let key = key.to_string();
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .px(px(7.0))
                .py(px(2.0))
                .bg(theme.elevated_background)
                .rounded(px(6.0))
                .text_size(px(11.0))
                .text_color(theme.text)
                .child(key),
        )
        .when_some(label, |this, label| {
            let label = label.to_string();
            this.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(label),
            )
        })
}

/// 浮窗垂直锚点：贴住光标的哪一边。
///
/// 终端覆盖光标在父容器顶部 → 用 `Top` 让浮窗紧贴光标行下方，列表向下展开；
/// 终端覆盖光标在父容器底部 → 用 `Bottom` 让浮窗底端紧贴光标行上方，
/// 列表从下往上增长。两种锚点都保证「靠近光标的一边」始终贴合光标。
#[derive(Clone, Copy, Debug)]
pub enum OverlayAnchor {
    /// 距父容器顶端的偏移（`.top(px)`）
    Top(Pixels),
    /// 距父容器底端的偏移（`.bottom(px)`）
    Bottom(Pixels),
}

/// 把锚点应用到一个绝对定位的 `div`。
fn apply_overlay_anchor<E: gpui::Styled>(element: E, anchor: OverlayAnchor) -> E {
    match anchor {
        OverlayAnchor::Top(top) => element.top(top),
        OverlayAnchor::Bottom(bottom) => element.bottom(bottom),
    }
}

/// 把 `last_used`（unix 秒）转成人类可读的相对时间，用于历史条目右侧次要文字。
fn relative_time_label(last_used: i64, now: i64) -> String {
    let diff = (now - last_used).max(0);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86_400)
    }
}

/// 渲染「最近使用」历史分组的行集合，置于补全列表顶部。
///
/// `history_selected` 是历史段内的高亮下标（`None` ⇒ 历史段无高亮）。
/// `on_pick_history(i)` 由外层提供，响应鼠标点击历史项第 `i` 条。
fn render_history_rows<F>(
    entries: &[HistoryEntry],
    history_selected: Option<usize>,
    theme: UiTheme,
    is_dark: bool,
    on_pick_history: Rc<F>,
) -> Vec<gpui::AnyElement>
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    if entries.is_empty() {
        return Vec::new();
    }
    let now = history::current_unix_secs();
    let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(entries.len() + 1);

    // 分组标题
    rows.push(
        div()
            .px(px(8.0))
            .py(px(2.0))
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child("最近使用")
            .into_any_element(),
    );

    for (index, entry) in entries.iter().enumerate() {
        let on_pick = on_pick_history.clone();
        let is_selected = Some(index) == history_selected;
        let time_label = relative_time_label(entry.last_used, now);
        rows.push(
            div()
                .w_full()
                .h(px(26.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .bg(if is_selected {
                    theme.accent.opacity(if is_dark { 0.18 } else { 0.12 })
                } else {
                    gpui::transparent_black()
                })
                .when(!is_selected, |this| {
                    this.hover(move |style| {
                        style.bg(theme.accent.opacity(if is_dark { 0.10 } else { 0.06 }))
                    })
                })
                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    on_pick(index, window, cx);
                })
                .child(
                    div()
                        .w(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            svg()
                                .size(px(12.0))
                                .text_color(theme.icon_muted)
                                .path(crate::assets::Icon::Clock.path()),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .truncate()
                        .font_family("monospace")
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child(entry.command_line.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(time_label),
                )
                .into_any_element(),
        );
    }

    // 分隔线
    rows.push(
        div()
            .h(px(1.0))
            .my(px(2.0))
            .bg(theme.border)
            .into_any_element(),
    );
    rows
}

/// 渲染内建命令建议浮层。
///
/// `selected_index` 为 `None` 时浮层弹出但所有项无高亮（用户尚未按 ↓/Tab）。
/// `on_pick` 由外层提供，用来响应鼠标选中某一条建议。
#[allow(clippy::too_many_arguments)]
pub fn render_overlay<F, H>(
    suggestions: Rc<[Arc<BuiltinCommand>]>,
    selected_index: Option<usize>,
    history: &[HistoryEntry],
    history_selected: Option<usize>,
    anchor: OverlayAnchor,
    theme: UiTheme,
    is_dark: bool,
    on_pick: F,
    on_pick_history: H,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
    H: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);
    let anchored_at_bottom = matches!(anchor, OverlayAnchor::Bottom(_));

    let history_rows = render_history_rows(
        history,
        history_selected,
        theme,
        is_dark,
        Rc::new(on_pick_history),
    );

    let items_list = div().max_h(px(420.0)).children(history_rows).children(
        suggestions
            .iter()
            .cloned()
            .enumerate()
            .map(move |(index, cmd)| {
                let on_pick = on_pick.clone();
                let is_selected = Some(index) == selected_index;
                div()
                    .w_full()
                    .h_6()
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .bg(if is_selected {
                        theme.accent.opacity(if is_dark { 0.16 } else { 0.09 })
                    } else {
                        theme.panel_background
                    })
                    .when(!is_selected, |this| {
                        this.hover(move |style| style.bg(theme.tab_hover_background))
                    })
                    .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        on_pick(index, window, cx);
                    })
                    .child(
                        div()
                            .w(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                svg()
                                    .size(px(10.0))
                                    .text_color(theme.icon_muted)
                                    .path(cmd.icon.path()),
                            ),
                    )
                    .child(
                        div()
                            .w(px(390.0))
                            .text_size(px(13.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(cmd.name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(cmd.description),
                    )
            }),
    );

    let key_hints_row = div()
        .px(px(6.0))
        .flex()
        .items_center()
        .gap_2()
        .child(render_key_hint("↑↓", Some("navigate"), theme))
        .child(render_key_hint("esc", Some("dismiss"), theme));

    let container = apply_overlay_anchor(div().absolute().left(px(0.0)).right(px(0.0)), anchor)
        .flex()
        .flex_col()
        .gap_0()
        // 浮窗整体使用不透明面板背景，避免终端文本透过来。
        .bg(theme.panel_background)
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        });

    if anchored_at_bottom {
        // 浮窗从光标向上展开 ⇒ 按键提示行挪到浮窗底部，紧贴光标行
        container
            .child(items_list)
            .child(div().h(px(1.0)).bg(theme.border))
            .child(key_hints_row)
            .child(div().h(px(1.0)).bg(theme.border))
    } else {
        container
            .child(div().h(px(1.0)).bg(theme.border))
            .child(key_hints_row)
            .child(div().h(px(1.0)).bg(theme.border))
            .child(items_list)
    }
}

/// 渲染路径补全浮层。
///
/// `selected_index` 为 `None` 时浮层弹出但所有项无高亮。
/// `on_pick` 由外层提供，用来响应鼠标选中某一条路径建议。
#[allow(clippy::too_many_arguments)]
pub fn render_path_overlay<F, H>(
    completions: &[PathCompletionItem],
    selected_index: Option<usize>,
    history: &[HistoryEntry],
    history_selected: Option<usize>,
    anchor: OverlayAnchor,
    left: Pixels,
    theme: UiTheme,
    is_dark: bool,
    scroll_handle: &ScrollHandle,
    on_pick: F,
    on_pick_history: H,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
    H: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);
    let overlay_width = estimate_overlay_width(completions);

    let history_rows = render_history_rows(
        history,
        history_selected,
        theme,
        is_dark,
        Rc::new(on_pick_history),
    );

    // 紧贴命令行光标的圆角弹层。当 `anchor` 为 `Bottom` 时浮窗底端贴近光标，
    // 列表从下往上贴着光标增长。宽度按最长建议项估算（夹紧到 320–900px）。
    apply_overlay_anchor(div().absolute().left(left), anchor)
        .w(px(overlay_width))
        .rounded(px(12.0))
        .bg(theme.notification_background)
        .shadow_lg()
        .flex()
        .flex_col()
        .py(px(4.0))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .id("path-completion-list")
                .max_h(px(272.0))
                .overflow_y_scroll()
                .track_scroll(scroll_handle)
                .px(px(6.0))
                .flex()
                .flex_col()
                .children(history_rows)
                .children(
                    completions
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(move |(index, item)| {
                            let on_pick = on_pick.clone();
                            let is_selected = Some(index) == selected_index;
                            let icon_path =
                                item.icon
                                    .map(|i| i.path())
                                    .unwrap_or_else(|| match item.kind {
                                        CompletionKind::Directory => {
                                            crate::assets::Icon::Folder.path()
                                        }
                                        CompletionKind::Host => crate::assets::Icon::Host.path(),
                                        CompletionKind::File => crate::assets::Icon::File.path(),
                                    });
                            div()
                                .w_full()
                                .h(px(28.0))
                                .px(px(10.0))
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .bg(if is_selected {
                                    theme.accent.opacity(if is_dark { 0.18 } else { 0.12 })
                                } else {
                                    gpui::transparent_black()
                                })
                                .when(!is_selected, |this| {
                                    this.hover(move |style| {
                                        style.bg(theme.accent.opacity(if is_dark {
                                            0.10
                                        } else {
                                            0.06
                                        }))
                                    })
                                })
                                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                                    cx.stop_propagation();
                                    on_pick(index, window, cx);
                                })
                                .child({
                                    let svg_element = svg()
                                        .size(px(14.0))
                                        .text_color(theme.text_muted)
                                        .path(icon_path);
                                    if item.icon == Some(crate::assets::Icon::ProgressIndicator) {
                                        div()
                                            .w(px(16.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                svg_element.with_animation(
                                                    "progress-rotate",
                                                    Animation::new(
                                                        std::time::Duration::from_millis(1000),
                                                    )
                                                    .repeat()
                                                    .with_easing(ease_in_out),
                                                    |this, delta| {
                                                        this.with_transformation(
                                                            Transformation::rotate(percentage(
                                                                delta,
                                                            )),
                                                        )
                                                    },
                                                ),
                                            )
                                    } else {
                                        div()
                                            .w(px(16.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(svg_element)
                                    }
                                })
                                .child(
                                    div()
                                        .font_family("monospace")
                                        .text_size(px(13.5))
                                        .text_color(theme.text)
                                        .child(item.name.clone()),
                                )
                                // 仅 Host 条目附带 nickname / 描述（"This machine"、保存的别名等）；
                                // 文件 / 目录条目只显示文件名
                                .when_some(host_description(&item), |this, desc| {
                                    this.child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .truncate()
                                            .text_size(px(12.5))
                                            .text_color(theme.text_muted)
                                            .child(desc),
                                    )
                                })
                        }),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::{
        BuiltinCommandManager, CommandHistory, find_builtin_command, resolve_command_line,
    };

    #[test]
    fn parses_builtin_open_command() {
        assert_eq!(
            find_builtin_command("/open").map(|cmd| cmd.name),
            Some("/open")
        );
        assert_eq!(
            find_builtin_command("/open my-target").map(|cmd| cmd.name),
            Some("/open")
        );
    }

    #[test]
    fn leaves_unknown_slash_command_to_shell() {
        assert_eq!(find_builtin_command("/unknown"), None);
        assert_eq!(find_builtin_command("ls"), None);
    }

    #[test]
    fn fake_builtin_command_specs_are_available_for_prompt() {
        let registry = super::global_registry();
        assert!(registry.find("/open-file").is_some());
        assert!(registry.find("/open").is_some());
        assert!(registry.find("/scp").is_some());
    }

    #[test]
    fn resolves_selected_command_when_query_is_a_prefix() {
        let registry = super::global_registry();
        let selected = registry.find("/open");
        assert_eq!(
            resolve_command_line("/op target", selected.as_deref(), false),
            "/open target"
        );
    }

    #[test]
    fn keeps_unknown_query_when_selection_only_comes_from_fallback() {
        let registry = super::global_registry();
        let selected = registry.find("/open");
        assert_eq!(
            resolve_command_line("/unknown target", selected.as_deref(), false),
            "/unknown target"
        );
    }

    #[test]
    fn forces_selected_command_for_explicit_pick() {
        let registry = super::global_registry();
        let selected = registry.find("/open-file");
        assert_eq!(
            resolve_command_line("/unknown target", selected.as_deref(), true),
            "/open-file target"
        );
    }

    #[test]
    fn pick_completes_selected_command_and_keeps_args() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/op target");
        manager.pick(0);

        // 缓冲被替换为完整命令
        assert_eq!(manager.query(), Some("/open target"));
        // 选择已应用 ⇒ 高亮重置为 None（"无选中" 是新的默认）
        assert_eq!(manager.selected_index(), None);
        assert!(!manager.is_dismissed());
    }

    #[test]
    fn pick_uses_clicked_index() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/op target");
        manager.pick(1);

        assert_eq!(manager.query(), Some("/open-file target"));
        // 选择已应用 ⇒ 高亮重置为 None
        assert_eq!(manager.selected_index(), None);
    }

    // ── overlay 状态机行为 ─────────────────────────────────────────────────────

    #[test]
    fn hide_overlay_only_hides_overlay_but_keeps_builtin_semantics() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp alice:/a /tmp/");
        manager.hide_overlay();
        assert_eq!(
            manager.overlay_state(),
            super::OverlayState::HiddenAfterPick
        );
        assert!(manager.is_overlay_hidden());
        // HiddenAfterPick 不应被解读为 "ESC 取消"
        assert!(!manager.is_dismissed());
    }

    #[test]
    fn dismiss_persists_until_buffer_cleared() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp something");
        manager.dismiss();
        assert_eq!(
            manager.overlay_state(),
            super::OverlayState::DismissedUntilReset
        );
        // 继续输入普通字符不应让浮层复活
        manager.update("x");
        assert!(manager.is_dismissed());
        // 删光字符后应复位
        while manager.query().is_some() {
            manager.pop();
        }
        assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
    }

    #[test]
    fn dismiss_revives_when_slash_prepended_at_front() {
        // Warp 语义：从没有 `/` 的缓冲插一个 `/` 回前头，视作重新进入内建命令
        let mut manager = BuiltinCommandManager::new();
        manager.update("abc");
        manager.dismiss();
        assert!(manager.is_dismissed());
        // 当前 update 只支持追加 —— 我们用"清空 + 重打 `/`" 模拟"首字符变 `/`"
        while manager.query().is_some() {
            manager.pop();
        }
        manager.update("/open");
        assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
    }

    #[test]
    fn space_after_scp_enters_argument_phase() {
        // `/scp ` 末尾空格 + 精确命令匹配 ⇒ 立即进入参数补全阶段
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp");
        assert!(!manager.is_argument_input());
        manager.update(" ");
        assert!(manager.is_argument_input());
    }

    #[test]
    fn remote_completion_replacement_keeps_colon_outside_quotes() {
        // host 含空格时，补全输出必须是 `"host":path`，否则提交时 `:` 会被当成
        // 引号内部字符，导致 parse_scp_token 把整段识别成本地路径。
        let entries = vec![super::RemoteEntrySnapshot {
            name: "app.tar".to_string(),
            is_directory: false,
            full_path: "/home/me/app.tar".to_string(),
        }];
        let items = super::build_remote_completions(
            &entries,
            "",              // file_prefix
            Some("my host"), // host_token（含空格）
            "/home/me/",     // current_path_prefix（光标处字面量）
        );
        assert_eq!(items.len(), 1);
        let replacement = items[0].replacement.clone().unwrap();
        // `:` 必须在 `my host` 的闭合引号之后
        let colon_pos = replacement.find(':').expect(": must exist");
        let close_quote_pos = replacement.find("\" ").or_else(|| {
            // 若后续没有空格，直接取第一个闭合引号
            replacement.match_indices('"').nth(1).map(|(i, _)| i)
        });
        assert!(
            close_quote_pos.map_or(false, |p| p < colon_pos),
            "expected closing quote before colon, got {}",
            replacement
        );
        // 完整格式
        assert_eq!(replacement, "\"my host\":/home/me/app.tar");
    }

    #[test]
    fn remote_completion_replacement_quotes_user_at_host_with_port() {
        // user@host:port 含 `:`，必须整体引号化，否则 first_unquoted_colon
        // 会切到端口前的 `:`。
        let entries = vec![super::RemoteEntrySnapshot {
            name: "x.txt".to_string(),
            is_directory: false,
            full_path: "/tmp/x.txt".to_string(),
        }];
        let items = super::build_remote_completions(
            &entries,
            "",
            Some("deploy@1.2.3.4:2222"), // user@host:port
            "/tmp/",
        );
        assert_eq!(items.len(), 1);
        let r = items[0].replacement.clone().unwrap();
        assert_eq!(r, "\"deploy@1.2.3.4:2222\":/tmp/x.txt");
    }

    #[test]
    fn match_hosts_returns_user_at_host_replacement() {
        use crate::ssh::config::{AuthMethod, SshConnectionConfig};
        let mut cfg =
            SshConnectionConfig::new("1.2.3.4".to_string(), "root".to_string(), AuthMethod::Agent);
        cfg.nickname = Some("alice".to_string());
        let saved = vec![cfg];
        let suggestions = super::scp_completion::match_hosts_by_prefix("ali", &saved);
        let nickname_hit = suggestions
            .iter()
            .find(|s| s.kind == super::scp_completion::HostSuggestionKind::Nickname)
            .expect("nickname hit");
        // display 仍是匹配源（保留 nickname 用于 UI 描述）
        assert_eq!(nickname_hit.display, "alice");
        // replacement 统一为 user@host
        assert_eq!(nickname_hit.replacement, "root@1.2.3.4");
    }

    #[test]
    fn bare_remote_completion_uses_terminal_cwd_and_writes_naked_path() {
        // 远程终端 + 没有显式 host 的 token：补全应当走 SFTP 列出当前 shell 的 cwd，
        // replacement 为裸路径（不带 host:），与"裸路径跟随当前终端"一致。
        use crate::ssh::config::{AuthMethod, SshConnectionConfig};
        let cfg =
            SshConnectionConfig::new("1.2.3.4".to_string(), "root".to_string(), AuthMethod::Agent);
        let cfg_id = cfg.id();

        let mut manager = BuiltinCommandManager::new();
        manager.set_terminal_context(Some(cfg.clone()), Some("/srv/app".to_string()));
        manager.update("/scp lo");

        // 第一次刷新会标记 loading，并把 (cfg_id, "/srv/app") 这把 key 注入待加载队列
        assert!(manager.is_loading_remote);
        let req = manager
            .pending_remote_request()
            .expect("pending remote request must be issued");
        assert_eq!(req.config_id, cfg_id);
        assert_eq!(req.absolute_dir, "/srv/app");

        // 模拟 SFTP 加载完成
        manager.apply_remote_dir_result(
            cfg_id.clone(),
            "/srv/app".to_string(),
            Ok(vec![
                super::RemoteEntrySnapshot {
                    name: "logs".to_string(),
                    is_directory: true,
                    full_path: "/srv/app/logs".to_string(),
                },
                super::RemoteEntrySnapshot {
                    name: "log.txt".to_string(),
                    is_directory: false,
                    full_path: "/srv/app/log.txt".to_string(),
                },
            ]),
        );

        // 远程终端 HostOrLocal：host 候选（含 Local 关键字）排在前，
        // 后面才是 SFTP 列出的远程文件。
        let names: Vec<&str> = manager
            .path_completions
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert!(
            names.contains(&"logs") && names.contains(&"log.txt"),
            "expected remote files in completion list, got {:?}",
            names
        );

        // 找出 logs/ 与 log.txt 的条目，验证 replacement 是裸路径（不带 host: 前缀）
        let logs = manager
            .path_completions
            .iter()
            .find(|i| i.name == "logs" && i.is_directory)
            .expect("logs/ entry");
        assert_eq!(logs.replacement.as_deref(), Some("logs/"));
        let log_txt = manager
            .path_completions
            .iter()
            .find(|i| i.name == "log.txt")
            .expect("log.txt entry");
        assert_eq!(log_txt.replacement.as_deref(), Some("log.txt"));
    }

    #[test]
    fn remote_dir_failure_shows_error_then_retries_without_loop() {
        // 远端目录读取失败时：① 收敛成错误占位（不再永远卡在"加载中"），
        // ② 不重新注册请求（断开失败→刷新→重试的无限循环），
        // ③ 下一次用户输入触发刷新时重新注册请求，自动重试。
        use crate::ssh::config::{AuthMethod, SshConnectionConfig};
        let cfg =
            SshConnectionConfig::new("1.2.3.4".to_string(), "root".to_string(), AuthMethod::Agent);
        let cfg_id = cfg.id();

        let mut manager = BuiltinCommandManager::new();
        manager.set_terminal_context(Some(cfg.clone()), Some("/srv/app".to_string()));
        manager.update("/scp lo");

        // 首次：标记 loading 并注册请求
        assert!(manager.is_loading_remote);
        let req = manager
            .pending_remote_request()
            .expect("first request issued");
        assert_eq!(req.absolute_dir, "/srv/app");

        // 模拟读取失败
        manager.apply_remote_dir_result(
            cfg_id.clone(),
            "/srv/app".to_string(),
            Err("权限不足".into()),
        );

        // ① 错误占位应出现在补全列表中（HostOrLocal 阶段它排在 host 候选之后）
        let failed_item = manager
            .path_completions
            .iter()
            .find(|i| i.name == "无法读取远端目录")
            .expect("failure placeholder must be shown instead of staying on loading");
        assert_eq!(failed_item.description.as_deref(), Some("权限不足"));
        // ② 失败后不得再有挂起请求（否则就是无限重试）
        assert!(
            manager.pending_remote_request().is_none(),
            "failure must not re-register a pending request"
        );

        // ③ 用户继续输入触发刷新 ⇒ 失败标记已消费，重新注册请求重试
        manager.update("g");
        let retry = manager
            .pending_remote_request()
            .expect("next input should retry");
        assert_eq!(retry.absolute_dir, "/srv/app");
    }

    #[test]
    fn bare_local_completion_in_local_terminal_keeps_host_listing() {
        // 本地终端不应改变行为：HostOrLocal 仍然能列出本机文件 + 已保存主机
        let mut manager = BuiltinCommandManager::new();
        // 没有 set_terminal_context ⇒ ssh = None ⇒ 走原有 build_host_or_local_completions
        manager.update("/scp ");
        // 至少应有本地条目；不强求具体文件名（依赖 cwd）
        // —— 关键是不能进入 loading_remote 分支
        assert!(!manager.is_loading_remote);
    }

    #[test]
    fn path_overlay_starts_without_highlight() {
        // 路径建议浮层弹出时不应预选任何项 —— 用户必须主动按 ↑/↓/Tab 才能选中。
        let mut manager = BuiltinCommandManager::new();
        manager.update("/open ");
        assert!(manager.is_argument_input());
        assert_eq!(manager.selected_path_index(), None);
    }

    #[test]
    fn dir_pick_keeps_overlay_but_does_not_preselect() {
        // /open 选中目录后浮层保持可见，但不应自动选中第一项。
        // 用 Local 关键字模拟"本机目录补全 + 选中目录"路径，规避对真实 fs 的依赖。
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp Local:/");
        // 保证 path_completions 非空再走 complete_path：直接构造一个 Directory 项
        manager.path_completions = vec![super::PathCompletionItem {
            name: "etc".to_string(),
            path: std::path::PathBuf::from("/etc"),
            is_directory: true,
            kind: super::CompletionKind::Directory,
            description: None,
            icon: None,
            replacement: Some("Local:/etc/".to_string()),
        }];
        manager.path_selected_index = Some(0); // 用户已主动选中
        manager.complete_path();
        // 选中目录后浮层应当 Visible，但不应再自动预选
        assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
        assert_eq!(manager.selected_path_index(), None);
    }

    // ── 渐进补全流程 ────────────────────────────────────────────────────────

    #[test]
    fn overlay_starts_without_highlight() {
        // /sc 弹出命令候选，默认无高亮
        let mut manager = BuiltinCommandManager::new();
        manager.update("/sc");
        assert!(!manager.suggestions().is_empty());
        assert_eq!(manager.selected_index(), None);
        assert!(!manager.has_active_completion());
    }

    #[test]
    fn arrow_down_first_press_lands_on_first_item() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/sc");
        manager.select_next();
        assert_eq!(manager.selected_index(), Some(0));
        assert!(manager.has_active_completion());
    }

    #[test]
    fn arrow_up_first_press_lands_on_last_item() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/sc");
        let total = manager.suggestions().len();
        manager.select_previous();
        assert_eq!(manager.selected_index(), Some(total - 1));
    }

    #[test]
    fn typing_after_highlight_resets_to_no_selection() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/sc");
        manager.select_next();
        assert_eq!(manager.selected_index(), Some(0));
        manager.update("p");
        assert_eq!(manager.selected_index(), None);
    }

    #[test]
    fn enter_with_no_selection_leaves_buffer_unchanged() {
        // 模拟 TerminalView 的 Enter 分支 B：浮层 visible 但未高亮 ⇒ 仅关浮层
        let mut manager = BuiltinCommandManager::new();
        manager.update("/sc");
        let buffer_before = manager.query().map(str::to_string);
        // 没按 ↓/Tab，has_active_completion 为 false
        assert!(!manager.has_active_completion());
        manager.hide_overlay();
        assert_eq!(manager.query().map(str::to_string), buffer_before);
        assert_eq!(
            manager.overlay_state(),
            super::OverlayState::HiddenAfterPick
        );
    }

    #[test]
    fn advance_after_pick_appends_space_when_more_args_needed() {
        // /scp 第一段补全完后，应自动追加空格触发第二段浮层
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp Local:/tmp/a.txt");
        manager.advance_after_pick();
        // 末尾应已加空格，进入第 2 个 token 阶段
        assert!(manager.query().unwrap().ends_with(' '));
        // 第二段 host 浮层应当 visible 且无高亮
        assert!(manager.is_argument_input());
        assert_eq!(manager.path_selected_index, None);
    }

    #[test]
    fn advance_after_pick_does_not_append_when_dir_selected() {
        // 选中目录后 path 末尾是 `/`，用户继续在该 token 内展开 ⇒ 不 advance
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp Local:/tmp/");
        manager.advance_after_pick();
        // 不应加空格
        assert_eq!(manager.query(), Some("/scp Local:/tmp/"));
        // 浮层保持 Visible —— 让用户继续选择子目录
        assert_eq!(manager.overlay_state(), super::OverlayState::Visible);
    }

    #[test]
    fn advance_after_pick_hides_overlay_when_args_complete() {
        // 两个 token 都填了 ⇒ 不再 advance，浮层进入 HiddenAfterPick 等回车提交
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp Local:/tmp/a.txt Local:/var/");
        // 注意：尾斜杠会让 advance 提前返回（dir 分支），先去掉它来构造"两段都已填具体文件"
        manager.update("b.txt");
        manager.advance_after_pick();
        assert_eq!(
            manager.query(),
            Some("/scp Local:/tmp/a.txt Local:/var/b.txt")
        );
        assert_eq!(
            manager.overlay_state(),
            super::OverlayState::HiddenAfterPick
        );
    }

    #[test]
    fn advance_after_pick_does_not_append_after_host_colon() {
        // 选中 host 后缓冲变成 `/scp <host>:`，path 还没输入。
        // 不应追加空格，否则 `:` 会被切成 token 边界，scp 参数解析失败。
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp root@1.2.3.4:");
        manager.advance_after_pick();
        assert_eq!(manager.query(), Some("/scp root@1.2.3.4:"));
    }

    // ── 光标编辑（cursor-aware buffer）─────────────────────────────────────────

    #[test]
    fn insert_at_cursor_after_moving_left() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp ab");
        // 光标默认在末尾（7）
        assert_eq!(manager.cursor(), Some(7));
        manager.move_cursor_left(); // 光标到 6（"a" 与 "b" 之间）
        assert_eq!(manager.cursor(), Some(6));
        manager.update("X"); // 在光标处插入
        assert_eq!(manager.query(), Some("/scp aXb"));
        // 插入后光标落在插入串之后
        assert_eq!(manager.cursor(), Some(7));
    }

    #[test]
    fn delete_forward_removes_char_after_cursor() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp ab");
        manager.move_cursor_left(); // 光标在 "a" 与 "b" 之间
        manager.delete_forward(); // 删 "b"
        assert_eq!(manager.query(), Some("/scp a"));
    }

    #[test]
    fn backspace_deletes_before_cursor_in_middle() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp ab");
        manager.move_cursor_left(); // 光标在 6
        manager.pop(); // 删光标前的 "a"
        assert_eq!(manager.query(), Some("/scp b"));
        assert_eq!(manager.cursor(), Some(5));
    }

    #[test]
    fn home_and_end_jump_to_edges() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp ab");
        manager.move_cursor_home();
        assert_eq!(manager.cursor(), Some(0));
        manager.move_cursor_end();
        assert_eq!(manager.cursor(), Some(7));
    }

    #[test]
    fn word_motion_and_word_delete() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp foo bar");
        // 末尾按词左移：跳过 "bar" 到其词首（位置 9）
        manager.move_cursor_word_left();
        assert_eq!(manager.cursor(), Some(9));
        // 再按词左移：跳过 "foo " 到 "foo" 词首（位置 5）
        manager.move_cursor_word_left();
        assert_eq!(manager.cursor(), Some(5));
        // 删词：删掉 "foo "（光标前一个词含尾随空格的处理由 prev_word_boundary 决定）
        manager.move_cursor_end();
        manager.delete_word_backward();
        assert_eq!(manager.query(), Some("/scp foo "));
    }

    #[test]
    fn cursor_moves_respect_cjk_grapheme_boundary() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp 中文");
        // 末尾光标在 "/scp ".len()(5) + 6 = 11
        assert_eq!(manager.cursor(), Some(11));
        manager.move_cursor_left(); // 跨过 "文"（3 字节）
        assert_eq!(manager.cursor(), Some(8));
        manager.move_cursor_left(); // 跨过 "中"（3 字节）
        assert_eq!(manager.cursor(), Some(5));
    }

    #[test]
    fn completion_follows_cursor_to_local_source_token() {
        // 复现截图场景：`/scp Downloads/ root@1.2.3.4:~/`，光标在本地源 token。
        // 弹窗应列本地文件（HostOrLocal），而不是末尾远端目的 token 的远端目录。
        let mut manager = BuiltinCommandManager::new();
        manager.update("/scp Downloads/ root@1.2.3.4:~/");
        // 把光标移回到 "Downloads/" 之后（本地源 token 末尾）
        // 末尾长度 - "root@1.2.3.4:~/".len() - 1(空格) = "/scp Downloads/".len()
        let target = "/scp Downloads/".len();
        while manager.cursor().unwrap() > target {
            manager.move_cursor_left();
        }
        assert_eq!(manager.cursor(), Some(target));
        // 移动光标已触发参数阶段补全重算（move_cursor_with）。
        // 不应进入远端加载（说明没把焦点落在 host:path 远端 token 上）
        assert!(
            !manager.is_loading_remote,
            "cursor on local source must not trigger remote dir load"
        );
    }

    // ── 历史候选选择与回填 ─────────────────────────────────────────────────────

    fn manager_with_history(lines: &[&str]) -> BuiltinCommandManager {
        let mut history = CommandHistory::default();
        for (i, l) in lines.iter().enumerate() {
            history.record(l, 100 + i as i64);
        }
        let mut manager = BuiltinCommandManager::new();
        manager.set_history(history);
        manager
    }

    #[test]
    fn history_matches_surface_in_command_name_phase() {
        let mut manager = manager_with_history(&["/scp alice:/etc/hosts ./"]);
        manager.update("/sc");
        // 输入 /sc 时应召回历史 /scp 整行
        assert_eq!(manager.history_matches().len(), 1);
        assert!(manager.should_render_overlay());
    }

    #[test]
    fn combined_selection_traverses_history_then_base() {
        let mut manager = manager_with_history(&["/scp alice:/x ./"]);
        manager.update("/sc");
        // 第一次 ↓ 落在历史段第 0 项
        manager.select_next();
        assert_eq!(manager.history_selected_index(), Some(0));
        assert_eq!(manager.selected_index(), None);
        // 再 ↓ 进入原候选段（命令名建议）第 0 项
        manager.select_next();
        assert_eq!(manager.history_selected_index(), None);
        assert_eq!(manager.selected_index(), Some(0));
    }

    #[test]
    fn complete_history_fills_whole_line() {
        let mut manager = manager_with_history(&["/scp alice:/etc/hosts ./"]);
        manager.update("/sc");
        manager.select_next(); // 选中历史段第 0 项
        assert!(manager.has_active_completion());
        manager.complete_history();
        // 整行回填，光标落末尾，浮层收起
        assert_eq!(manager.query(), Some("/scp alice:/etc/hosts ./"));
        assert_eq!(manager.cursor(), Some("/scp alice:/etc/hosts ./".len()));
        assert_eq!(
            manager.overlay_state(),
            super::OverlayState::HiddenAfterPick
        );
    }

    #[test]
    fn history_matches_surface_in_argument_phase() {
        let mut manager = manager_with_history(&["/scp alice:/etc/hosts ./"]);
        manager.update("/scp ali");
        assert!(manager.is_argument_input());
        // 整行子串匹配：输入 /scp ali 召回含 alice 的历史
        assert_eq!(manager.history_matches().len(), 1);
    }
}
