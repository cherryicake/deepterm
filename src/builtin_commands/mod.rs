use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{IntoElement, ParentElement, StatefulInteractiveElement, Styled};
use gpui::{MouseButton, Pixels, div, px, svg};

use crate::theme::UiTheme;

pub mod argument;
pub mod command_model;
pub mod executor;
pub mod path_completion;
pub mod path_resolver;
pub mod registry;
pub mod scp_completion;
pub mod suggestion_engine;
pub use argument::{ArgumentError, ArgumentParser, ParsedArgument};
pub use command_model::{BuiltinCommandModel, CommandEntryState, DetectedCommand};
pub use executor::{CommandContext, CommandExecutor, CommandHandler, CommandResult};
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
    /// `/scp` 第二段是否处于"等待远端结果"状态
    pub is_loading_remote: bool,
}

/// 远端目录在补全缓存中的状态
#[derive(Clone, Debug)]
pub enum RemoteDirCacheEntry {
    Entries(Vec<RemoteEntrySnapshot>),
    Failed(String),
}

/// 从外部异步加载后写入的远端条目快照
#[derive(Clone, Debug)]
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
            is_loading_remote: false,
        }
    }

    /// 注入 SSH 保存连接列表。外部应在 `/scp` 模式启动前刷新一次即可。
    pub fn set_ssh_configs(&mut self, configs: Vec<crate::ssh::SshConnectionConfig>) {
        self.ssh_saved_configs = Arc::new(configs);
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
        let value = match entries {
            Ok(list) => RemoteDirCacheEntry::Entries(list),
            Err(err) => RemoteDirCacheEntry::Failed(err),
        };
        // 更新 LRU：先移除旧位置，再插入到末尾（最新）
        self.remote_dir_cache_lru.retain(|k| k != &key);
        self.remote_dir_cache_lru.push(key.clone());
        self.remote_dir_cache.insert(key.clone(), value);
        // LRU 淘汰：限制缓存最多 128 个条目
        while self.remote_dir_cache_lru.len() > 128 {
            let oldest = self.remote_dir_cache_lru.remove(0);
            self.remote_dir_cache.remove(&oldest);
        }
        self.pending_remote_keys.remove(&key);
        self.refresh_path_completions();
    }

    /// 如果 `/scp` 当前 token 需要远端目录数据但缓存中没有，返回需要加载的请求。
    /// 调用方负责在后台线程读取 SFTP，完成后回传到 `apply_remote_dir_result`。
    pub fn pending_remote_request(&mut self) -> Option<RemoteCompletionRequest> {
        if !self.is_loading_remote {
            return None;
        }
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
    /// - 命令模式启动；浮层 visible；当前阶段有可见候选项（loading/错误占位也算可见）
    pub fn should_render_overlay(&self) -> bool {
        if !self.is_active() || self.overlay.is_hidden() {
            return false;
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
            state.push_str(text);
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
        self.refresh_state();
    }

    /// 删除最后一个字符。
    pub fn pop(&mut self) {
        let was_dismissed = self.is_dismissed();
        let mut should_clear = false;
        if let Some(state) = self.state.as_mut() {
            state.pop();
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
        self.refresh_state();
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
    }

    /// 标记为已取消（ESC）：隐藏浮层，且回车把缓冲原样下发 shell。
    /// 仅在缓冲清空或首字符重新变成 `/` 时才会自动复位。
    pub fn dismiss(&mut self) {
        self.overlay = OverlayState::DismissedUntilReset;
        self.selected_index = None;
        self.path_selected_index = None;
    }

    /// 仅隐藏浮层，不改变"是否走内建命令"的语义。
    ///
    /// 用于补全完成后临时收起补全列表 —— 再次回车仍会执行内建命令，
    /// 用户继续输入字符 / 退格会自动重开浮层。
    pub fn hide_overlay(&mut self) {
        self.overlay = OverlayState::HiddenAfterPick;
        self.selected_index = None;
        self.path_selected_index = None;
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

    /// 在建议列表中循环选择下一项。
    ///
    /// 当前未选中（`None`）时，第一次调用进入第 0 项，与"按下 ↓ 才开始选"的语义匹配。
    pub fn select_next(&mut self) {
        match self.entry_phase {
            EntryPhase::CommandName => {
                let total = self.suggestions.len();
                if total > 0 {
                    self.selected_index = Some(match self.selected_index {
                        None => 0,
                        Some(i) => (i + 1) % total,
                    });
                }
            }
            EntryPhase::ArgumentInput => {
                let total = self.path_completions.len();
                if total > 0 {
                    self.path_selected_index = Some(match self.path_selected_index {
                        None => 0,
                        Some(i) => (i + 1) % total,
                    });
                }
            }
        }
    }

    /// 在建议列表中循环选择上一项。
    pub fn select_previous(&mut self) {
        match self.entry_phase {
            EntryPhase::CommandName => {
                let total = self.suggestions.len();
                if total > 0 {
                    self.selected_index = Some(match self.selected_index {
                        None => total - 1,
                        Some(0) => total - 1,
                        Some(i) => i - 1,
                    });
                }
            }
            EntryPhase::ArgumentInput => {
                let total = self.path_completions.len();
                if total > 0 {
                    self.path_selected_index = Some(match self.path_selected_index {
                        None => total - 1,
                        Some(0) => total - 1,
                        Some(i) => i - 1,
                    });
                }
            }
        }
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
        self.refresh_state();
    }

    /// 应用当前选中的路径补全。
    ///
    /// 将当前路径参数替换为选中的完整路径。无选中时为 no-op。
    /// 若选中的是目录，替换后自动刷新该目录下的补全列表（保持浮层展开）。
    pub fn complete_path(&mut self) {
        let Some(selected) = self.selected_path_completion().cloned() else {
            return;
        };
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let current_input = state.buffer().to_string();

        // `/scp` 走独立分支：补全项自带 `replacement`（可能含 host 前缀）
        if is_scp_command(&current_input) {
            if let Some(replacement) = selected.replacement.clone() {
                if let Some(query) = scp_completion::analyse(&current_input) {
                    let mut next = String::with_capacity(current_input.len() + replacement.len());
                    next.push_str(&current_input[..query.token_start]);
                    next.push_str(&replacement);
                    next.push_str(&current_input[query.token_end..]);
                    state.set_buffer(next);
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

        // 分离命令名和参数
        let mut parts = current_input.splitn(2, |c: char| c.is_whitespace());
        let command_name = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("");

        // 解析参数的目录前缀和文件名前缀
        let (dir_part, _file_prefix) = PathCompletionEngine::split_path_prefix(args);

        // 构建新的参数：目录前缀 + 选中的名称
        let new_arg = if selected.is_directory {
            format!("{}{}/", dir_part, selected.name)
        } else {
            format!("{}{}", dir_part, selected.name)
        };

        let new_input = if new_arg.is_empty() {
            command_name.to_string()
        } else {
            format!("{} {}", command_name, new_arg)
        };

        state.set_buffer(new_input);
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
            Some(index.min(total - 1))
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

        // `/scp` 专用分支：两个参数、可能是主机、远端目录
        if is_scp_command(&query) {
            self.refresh_scp_completions(&query);
            return;
        }

        // 通用单参数补全：在远程终端时跟随当前 SSH 主机走 SFTP
        // —— 与 `/scp` 裸路径分支一致，使补全起点始终是终端实际所在的 cwd。
        let args = query
            .trim_start()
            .splitn(2, |c: char| c.is_whitespace())
            .nth(1)
            .unwrap_or("")
            .trim_start();
        let effective_prefix = if args.is_empty() { "" } else { args };

        if let Some(terminal_cfg) = self.terminal_ssh.clone() {
            self.refresh_bare_remote_completions(effective_prefix, terminal_cfg, None);
            return;
        }

        // 本地终端：列出本机文件
        self.is_loading_remote = false;
        self.path_completions =
            PathCompletionEngine::complete(effective_prefix, self.cwd.as_deref());
        self.path_selected_index = None;
    }

    /// `/scp` 补全主入口：按光标所在 token 的阶段分发。
    fn refresh_scp_completions(&mut self, buffer: &str) {
        self.is_loading_remote = false;
        let Some(query) = scp_completion::analyse(buffer) else {
            self.path_completions.clear();
            return;
        };

        match &query.phase {
            ScpCompletionPhase::HostOrLocal { prefix } => {
                // 远程终端：host 候选 + 当前终端的远程文件；
                // 本地终端：host 候选 + 本机文件。
                if let Some(terminal_cfg) = self.terminal_ssh.clone() {
                    let mut items = build_host_only_completions(
                        prefix,
                        &self.ssh_saved_configs,
                    );
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
                    Some(RemoteDirCacheEntry::Entries(entries)) => {
                        // 更新 LRU：命中后移到末尾（最新）
                        self.remote_dir_cache_lru.retain(|k| k != &key);
                        self.remote_dir_cache_lru.push(key.clone());
                        self.path_completions = build_remote_completions(
                            &entries,
                            &file_prefix,
                            Some(canonical_host.as_str()),
                            prefix,
                        );
                    }
                    Some(RemoteDirCacheEntry::Failed(err)) => {
                        self.path_completions = vec![PathCompletionItem {
                            name: "无法读取远端目录".to_string(),
                            path: PathBuf::new(),
                            is_directory: false,
                            kind: CompletionKind::File,
                            description: Some(err),
                            replacement: None,
                        }];
                    }
                    None => {
                        self.is_loading_remote = true;
                        self.pending_remote_keys.entry(key).or_insert(false);
                        self.path_completions = vec![PathCompletionItem {
                            name: "读取远端目录中…".to_string(),
                            path: PathBuf::new(),
                            is_directory: false,
                            kind: CompletionKind::File,
                            description: Some(absolute_dir),
                            replacement: None,
                        }];
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
            Some(RemoteDirCacheEntry::Entries(entries)) => {
                self.remote_dir_cache_lru.retain(|k| k != &key);
                self.remote_dir_cache_lru.push(key.clone());
                build_remote_completions(&entries, &file_prefix, extra_host_quoted, prefix)
            }
            Some(RemoteDirCacheEntry::Failed(err)) => vec![PathCompletionItem {
                name: "无法读取远端目录".to_string(),
                path: PathBuf::new(),
                is_directory: false,
                kind: CompletionKind::File,
                description: Some(err),
                replacement: None,
            }],
            None => {
                self.is_loading_remote = true;
                self.pending_remote_keys.entry(key).or_insert(false);
                vec![PathCompletionItem {
                    name: "读取远端目录中…".to_string(),
                    path: PathBuf::new(),
                    is_directory: false,
                    kind: CompletionKind::File,
                    description: Some(absolute_dir),
                    replacement: None,
                }]
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
            let replacement = format!("{}:", quoted_host);
            items.push(PathCompletionItem {
                // 显示统一 user@host[:port]，让命令行预览与最终结果一致
                name: suggestion.replacement.clone(),
                path: PathBuf::new(),
                is_directory: false,
                kind: CompletionKind::Host,
                description,
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
}

impl BuiltinCommandState {
    /// 用初始文本创建命令缓冲区。
    pub fn new(buffer: String) -> Self {
        Self { buffer }
    }

    /// 在缓冲区末尾追加文本。
    pub fn push_str(&mut self, text: &str) {
        self.buffer.push_str(text);
    }

    /// 删除缓冲区末尾的一个字符。
    pub fn pop(&mut self) {
        self.buffer.pop();
    }

    /// 判断缓冲区是否为空。
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 返回当前缓冲区文本。
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// 替换整个缓冲内容
    pub fn set_buffer(&mut self, buffer: String) {
        self.buffer = buffer;
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
            this.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(label.to_string()),
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

/// 渲染内建命令建议浮层。
///
/// `selected_index` 为 `None` 时浮层弹出但所有项无高亮（用户尚未按 ↓/Tab）。
/// `on_pick` 由外层提供，用来响应鼠标选中某一条建议。
pub fn render_overlay<F>(
    suggestions: Rc<[Arc<BuiltinCommand>]>,
    selected_index: Option<usize>,
    anchor: OverlayAnchor,
    theme: UiTheme,
    is_dark: bool,
    on_pick: F,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);
    let anchored_at_bottom = matches!(anchor, OverlayAnchor::Bottom(_));

    let items_list = div().max_h(px(420.0)).children(
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
pub fn render_path_overlay<F>(
    completions: &[PathCompletionItem],
    selected_index: Option<usize>,
    anchor: OverlayAnchor,
    left: Pixels,
    theme: UiTheme,
    is_dark: bool,
    on_pick: F,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);

    // 紧贴命令行光标的圆角弹层。当 `anchor` 为 `Bottom` 时浮窗底端贴近光标，
    // 列表从下往上贴着光标增长。
    apply_overlay_anchor(div().absolute().left(left), anchor)
        .min_w(px(320.0))
        .max_w(px(640.0))
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
                .px(px(6.0))
                .flex()
                .flex_col()
                .children(
                    completions
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(move |(index, item)| {
                            let on_pick = on_pick.clone();
                            let is_selected = Some(index) == selected_index;
                            let icon_path = match item.kind {
                                CompletionKind::Directory => crate::assets::Icon::Folder.path(),
                                CompletionKind::Host => crate::assets::Icon::Host.path(),
                                CompletionKind::File => crate::assets::Icon::File.path(),
                            };
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
                                .child(
                                    div()
                                        .w(px(16.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            svg()
                                                .size(px(14.0))
                                                .text_color(theme.text_muted)
                                                .path(icon_path),
                                        ),
                                )
                                .child(
                                    div()
                                        .font_family("monospace")
                                        .text_size(px(13.5))
                                        .text_color(theme.text)
                                        .child(item.name.clone()),
                                )
                        }),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::{BuiltinCommandManager, find_builtin_command, resolve_command_line};

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
            "",                  // file_prefix
            Some("my host"),     // host_token（含空格）
            "/home/me/",         // current_path_prefix（光标处字面量）
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
        assert_eq!(
            manager.overlay_state(),
            super::OverlayState::Visible
        );
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
}
