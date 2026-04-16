//! Terminal view component - Full featured

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EntityInputHandler, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString, Subscription, Task,
    UTF16Selection, Window, actions, div, point, px,
};
use gpui::{KeyBinding, prelude::*};

use crate::file_browser::FileBrowserTarget;
use crate::ssh_view::connect::Event::Reconnect;
/// Events emitted by TerminalView
#[derive(Clone, Debug)]
pub enum Event {
    /// Terminal process exited - container should close this view
    CloseTerminal,
    /// Open the file browser view
    OpenFileBrowser { target: FileBrowserTarget },
    /// Open a file in the editor
    OpenFile { path: String },
    /// SSH 重新连接请求
    SshReconnect { config: SshConnectionConfig },
}

impl EventEmitter<Event> for TerminalView {}

/// ASCII control character for Ctrl+V (SYN - Synchronous Idle)
pub const CTRL_V: u8 = 0x16;

/// IME state for tracking marked (pre-edit) text during composition
pub struct ImeState {
    pub marked_text: String,
}

use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::TermMode;

use crate::ShowSshPicker;
use crate::builtin_commands::{
    BuiltinCommandHandler, BuiltinCommandManager, find_builtin_command, resolve_command_line,
};
use crate::ssh::{SshConnectionConfig, sftp_fs};
use crate::ssh_view::SshConnectView;
use crate::terminal::mappings::keys::to_esc_str;
use crate::terminal::terminal::{Event as TerminalEvent, Terminal};
use crate::terminal_view::terminal_element::{
    FlattenedRenderCache, RowRenderCache, TerminalElement,
};
use crate::terminal_view::terminal_scrollbar::TerminalScrollbar;
use crate::theme::ActiveTheme;

// Define terminal actions
actions!(terminal, [Copy, Paste, Clear, SelectAll]);
const CONTEXT: &str = "Terminal";
const SCROLLBAR_VISIBLE_AFTER_SCROLL: Duration = Duration::from_secs(2);
pub(crate) fn init(cx: &mut App) {
    // Bind keyboard shortcuts
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("cmd-k", ShowSshPicker, Some(CONTEXT)),
    ]);
}

/// 广播输入回调类型 - 用于将输入广播给其他终端
pub type BroadcastInputCallback = Box<dyn Fn(&[u8])>;

pub struct TerminalView {
    terminal: Entity<Terminal>,
    terminal_subscription: Option<Subscription>,
    working_directory: Option<PathBuf>,
    ssh_config: Option<SshConnectionConfig>,
    focus_handle: FocusHandle,
    cursor_visible: bool,
    title: SharedString,
    selecting: bool,
    /// IME composition state for CJK input methods
    ime_state: Option<ImeState>,
    /// DeepTerm 内置命令管理器
    builtin_command_manager: BuiltinCommandManager,
    /// 当前输入行是否仍允许由 `/` 启动内置命令
    can_start_builtin_command: bool,
    /// 广播输入回调 - 当终端输入需要广播给其他终端时使用
    broadcast_callback: Option<BroadcastInputCallback>,
    /// Pending scrollbar hide task - prevents spawning a new timer every render frame
    scrollbar_hide_task: Option<Task<()>>,
    /// Scroll timestamp used for the currently scheduled hide task.
    scrollbar_hide_scheduled_at: Option<Instant>,
    /// SSH 连接进度视图 - 在连接建立期间覆盖终端内容
    ssh_connect_view: Option<Entity<SshConnectView>>,
    /// SSH 连接视图订阅 - 用于取消订阅重新连接事件
    ssh_connect_subscription: Option<Subscription>,
    /// SSH 连接视图隐藏任务 - 用于取消延迟隐藏任务
    ssh_connect_hide_task: Option<Task<()>>,
    /// Per-row render cache, indexed by viewport row (0 = top).
    pub row_render_cache: Vec<RowRenderCache>,
    /// Flattened render data reused across frames when nothing changed.
    pub flattened_render_cache: FlattenedRenderCache,
    /// Last theme id used to build row cache.
    pub cached_theme_id: Option<String>,
    /// Last selection range used to build row cache.
    pub cached_selection_range: Option<SelectionRange>,
    /// Last mouse mode state used to build row cache.
    pub cached_selection_mouse_mode: bool,
}

/// SSH 连接状态检查结果
enum SshCheckOutcome {
    Connected,
    Failed(String),
    Pending,
}

impl SshCheckOutcome {
    fn check(terminal: &Entity<Terminal>, cx: &App) -> Self {
        let term = terminal.read(cx);
        if term.is_ssh_connected() {
            SshCheckOutcome::Connected
        } else if term.is_ssh_failed() {
            SshCheckOutcome::Failed(
                term.ssh_error_message()
                    .unwrap_or_else(|| "Unknown error".to_string()),
            )
        } else {
            SshCheckOutcome::Pending
        }
    }
}

impl TerminalView {
    /// 处理 SSH 连接异常断开事件
    fn handle_ssh_abnormal_disconnect(&self, cx: &mut Context<Self>) {
        if let Some(config) = self.ssh_config.as_ref() {
            log::info!("SSH 连接异常断开，触发重连: {}", config.host_label());
            cx.emit(Event::SshReconnect {
                config: config.clone(),
            });
        } else {
            // 这不应该发生：SSH 异常断开事件应该只由 SSH 终端触发
            log::warn!("收到 SSH 异常断开事件，但 ssh_config 为 None");
        }
    }

    /// 处理 SSH 连接进度视图状态更新
    fn handle_ssh_connect_status(
        &mut self,
        connect_view: &Entity<SshConnectView>,
        outcome: SshCheckOutcome,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            SshCheckOutcome::Connected => {
                connect_view.update(cx, |view, cx| view.set_connected(cx));
                // 取消之前的隐藏任务（如果有）
                self.ssh_connect_hide_task.take();
                // 延迟移除连接视图，让用户看到完成状态
                self.ssh_connect_hide_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    this.update(cx, |this, cx| {
                        this.ssh_connect_view = None;
                        this.ssh_connect_hide_task = None;
                        cx.notify();
                    })
                    .ok();
                }));
            }
            SshCheckOutcome::Failed(msg) => {
                // 直接传递 msg，避免 &str -> String 的克隆
                connect_view.update(cx, |view, cx| view.set_failed(msg, cx));
            }
            SshCheckOutcome::Pending => {}
        }
    }

    /// 创建本地终端视图的通用方法
    fn create_terminal_view(
        cx: &mut App,
        terminal: Entity<Terminal>,
        working_directory: Option<PathBuf>,
        ssh_config: Option<SshConnectionConfig>,
        initial_title: SharedString,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        cx.new(|cx| {
            let terminal_clone = terminal.clone();

            // 本地终端只需要处理基本事件，不需要处理 SSH 相关事件
            let subscription = cx.subscribe(
                &terminal_clone,
                |this: &mut TerminalView, _terminal, event: &TerminalEvent, cx| match event {
                    TerminalEvent::TitleChanged => {
                        this.update_title(cx);
                    }
                    TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                    // 本地终端不会触发 SshAbnormalDisconnect，不需要处理
                    TerminalEvent::SshAbnormalDisconnect => {}
                },
            );

            let mut this = Self {
                terminal: terminal_clone,
                terminal_subscription: Some(subscription),
                working_directory,
                ssh_config,
                focus_handle,
                cursor_visible: true,
                title: initial_title,
                selecting: false,
                ime_state: None,
                builtin_command_manager: BuiltinCommandManager::new(),
                can_start_builtin_command: true,
                broadcast_callback: None,
                scrollbar_hide_task: None,
                scrollbar_hide_scheduled_at: None,
                ssh_connect_view: None,
                ssh_connect_subscription: None,
                ssh_connect_hide_task: None,
                row_render_cache: Vec::new(),
                flattened_render_cache: FlattenedRenderCache::default(),
                cached_theme_id: None,
                cached_selection_range: None,
                cached_selection_mouse_mode: false,
            };
            this.update_title(cx);
            this
        })
    }

    /// 创建新的本地终端
    pub fn new(cx: &mut App) -> Entity<Self> {
        let working_directory = dirs::home_dir();
        let terminal = Terminal::new(cx, working_directory.clone());
        Self::create_terminal_view(cx, terminal, working_directory, None, "Terminal".into())
    }

    /// 使用指定命令创建新的本地终端
    ///
    /// # 参数
    ///
    /// * `cx` - GPUI 上下文
    /// * `command` - 要执行的命令（如 "powershell.exe", "cmd.exe", "wsl.exe"）
    /// * `args` - 命令参数
    pub fn new_with_command(cx: &mut App, command: &str, args: &[String]) -> Entity<Self> {
        let command_string = command.to_string();
        let args_vec: Vec<String> = args.to_vec();
        // WSL 不传递工作目录，让它默认进入 home 目录
        let working_dir = if command.contains("wsl") {
            None
        } else {
            dirs::home_dir()
        };
        let terminal =
            Terminal::new_with_command(cx, working_dir.clone(), &command_string, &args_vec);
        Self::create_terminal_view(
            cx,
            terminal,
            working_dir,
            None,
            command_string.into(),
        )
    }

    /// 创建新的 SSH 远程终端
    ///
    /// 优先使用 russh 连接，如果失败则回退到 PTY + ssh 命令方式
    ///
    /// # 参数
    ///
    /// * `cx` - GPUI 上下文
    /// * `ssh_config` - SSH 连接配置
    pub fn new_ssh(cx: &mut App, ssh_config: &SshConnectionConfig) -> Entity<Self> {
        log::info!("=== 创建 SSH 远程终端（优先 russh，失败回退到 ssh 命令）===");
        log::info!(
            "目标: {}@{}:{}",
            ssh_config.username,
            ssh_config.host,
            ssh_config.port
        );

        let focus_handle = cx.focus_handle();
        let ssh_config_clone = ssh_config.clone();

        cx.new(|cx| {
            // 首先尝试 russh 方式创建终端
            let terminal = Terminal::new_ssh(cx, &ssh_config_clone);
            let terminal_clone = terminal.clone();

            // 创建 SSH 连接进度视图（在连接成功前覆盖在终端上方）
            let connect_view = cx.new(|cx| SshConnectView::new(ssh_config_clone.clone(), cx));

            let terminal_subscription = cx.subscribe(
                &terminal_clone,
                |this: &mut TerminalView, terminal, event: &TerminalEvent, cx| match event {
                    TerminalEvent::TitleChanged => {
                        this.update_title(cx);
                    }
                    TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                        if let Some(connect_view) = this.ssh_connect_view.clone() {
                            this.handle_ssh_connect_status(
                                &connect_view,
                                SshCheckOutcome::check(&terminal, cx),
                                cx,
                            );
                        }
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                    TerminalEvent::SshAbnormalDisconnect => {
                        this.handle_ssh_abnormal_disconnect(cx);
                    }
                },
            );

            let mut this = Self {
                terminal: terminal_clone,
                terminal_subscription: Some(terminal_subscription),
                working_directory: None,
                ssh_config: Some(ssh_config_clone.clone()),
                focus_handle,
                cursor_visible: true,
                title: ssh_config_clone.host_label().into(),
                selecting: false,
                ime_state: None,
                builtin_command_manager: BuiltinCommandManager::new(),
                can_start_builtin_command: true,
                broadcast_callback: None,
                scrollbar_hide_task: None,
                scrollbar_hide_scheduled_at: None,
                ssh_connect_view: Some(connect_view.clone()),
                ssh_connect_subscription: None,
                ssh_connect_hide_task: None,
                row_render_cache: Vec::new(),
                flattened_render_cache: FlattenedRenderCache::default(),
                cached_theme_id: None,
                cached_selection_range: None,
                cached_selection_mouse_mode: false,
            };

            // 订阅 SshConnectView 的重新连接事件并存储订阅
            this.ssh_connect_subscription =
                Some(this.subscribe_ssh_connect_view(&connect_view, cx));

            this.update_title(cx);
            this
        })
    }

    /// 更新终端标题
    fn update_title(&mut self, cx: &mut Context<Self>) {
        self.title = self.terminal.read(cx).remote_info().map_or_else(
            || "Terminal".into(),
            |(host, username, port)| {
                if port == 22 {
                    format!("{}@{}", username, host)
                } else {
                    format!("{}@{}:{}", username, host, port)
                }
                .into()
            },
        );
    }

    /// 订阅终端事件（用于重新连接时）
    fn subscribe_terminal_events(
        &mut self,
        terminal: &Entity<Terminal>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(
            terminal,
            |this: &mut TerminalView, term, event: &TerminalEvent, cx| match event {
                TerminalEvent::TitleChanged => {
                    this.update_title(cx);
                }
                TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                    if let Some(connect_view) = this.ssh_connect_view.clone() {
                        this.handle_ssh_connect_status(
                            &connect_view,
                            SshCheckOutcome::check(&term, cx),
                            cx,
                        );
                    }
                    cx.notify();
                }
                TerminalEvent::Bell => {}
                TerminalEvent::CloseTerminal => {
                    // 正常退出（用户执行 exit 命令），关闭 panel
                    cx.emit(Event::CloseTerminal);
                }
                TerminalEvent::SshAbnormalDisconnect => {
                    this.handle_ssh_abnormal_disconnect(cx);
                }
            },
        )
    }

    /// 订阅 SshConnectView 的重新连接事件
    fn subscribe_ssh_connect_view(
        &mut self,
        connect_view: &Entity<SshConnectView>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(
            connect_view,
            move |this: &mut TerminalView, _view, event: &crate::ssh_view::connect::Event, cx| {
                let Reconnect { config } = event;
                log::info!("重新连接 SSH: {}", config.host_label());

                // 清理旧订阅
                this.terminal_subscription.take();
                this.ssh_connect_subscription.take();

                // 创建新的连接视图
                let new_connect_view =
                    cx.new(|cx| crate::ssh_view::SshConnectView::new(config.clone(), cx));

                // 订阅新视图的重新连接事件并存储订阅
                this.ssh_connect_subscription =
                    Some(this.subscribe_ssh_connect_view(&new_connect_view, cx));

                this.ssh_connect_view = Some(new_connect_view);

                // 创建新的终端
                let new_terminal = Terminal::new_ssh(cx, config);
                this.terminal_subscription =
                    Some(this.subscribe_terminal_events(&new_terminal, cx));
                this.terminal = new_terminal;
                cx.notify();
            },
        )
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// 设置广播输入回调
    /// 当终端有输入时，会调用此回调将输入数据广播给其他终端
    pub fn set_broadcast_callback(&mut self, callback: BroadcastInputCallback) {
        self.broadcast_callback = Some(callback);
    }

    /// 清除广播输入回调
    pub fn clear_broadcast_callback(&mut self) {
        self.broadcast_callback = None;
    }

    /// 检查是否是远程终端
    pub fn is_remote(&self, cx: &App) -> bool {
        self.terminal.read(cx).is_remote()
    }

    /// Check if the scrollbar should be visible
    /// Scrollbar is visible when:
    /// 1. There's scrollable content (total_lines > viewport_lines)
    /// 2. User is dragging the scrollbar, OR
    /// 3. Less than 2 seconds have passed since the last scroll
    pub fn should_show_scrollbar(&self, cx: &App) -> bool {
        let term = self.terminal.read(cx);
        let content = term.last_content();
        let total_lines = content.total_lines;
        let viewport_lines = content.viewport_lines;

        // No scrollbar if no scrollable content
        if total_lines <= viewport_lines {
            return false;
        }

        // Always show scrollbar while dragging
        if term.scrollbar_drag_offset.get().is_some() {
            return true;
        }

        // Show scrollbar if scrolled within the last 2 seconds
        let last_scroll = term.last_scroll_time.get();
        let elapsed = last_scroll.elapsed();
        elapsed < SCROLLBAR_VISIBLE_AFTER_SCROLL
    }

    // IME handling methods for CJK input methods
    /// Sets the marked (pre-edit) text from the IME.
    /// During composition, this is called repeatedly to update the candidate text.
    /// The text is NOT sent to the terminal - it's only for display purposes.
    pub fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    /// Gets the current marked range (UTF-16).
    pub fn marked_text_range(&self) -> Option<std::ops::Range<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    /// Returns true if IME composition is currently active.
    pub fn has_ime_composition(&self) -> bool {
        self.ime_state
            .as_ref()
            .map_or(false, |s| !s.marked_text.is_empty())
    }

    /// 是否存在本地覆盖输入。
    ///
    /// 这里包括 IME 预编辑文本和内建命令缓冲；只要存在其一，就由覆盖层负责显示输入。
    pub fn has_local_overlay_input(&self) -> bool {
        self.builtin_command_manager.is_active() || self.has_ime_composition()
    }

    /// Returns a reference to the current IME state, if any.
    pub fn ime_state(&self) -> Option<&ImeState> {
        self.ime_state.as_ref()
    }

    /// Returns the active builtin command buffer, if any.
    pub fn builtin_command_text(&self) -> &str {
        self.builtin_command_manager.query().unwrap_or_default()
    }

    /// Returns the current IME pre-edit text, if any.
    pub fn ime_text(&self) -> &str {
        self.ime_state
            .as_ref()
            .map(|state| state.marked_text.as_str())
            .unwrap_or_default()
    }

    /// 组合覆盖层要显示的文本。
    ///
    /// 内建命令缓冲显示在前，IME 预编辑文本追加在后，保持用户当前看到的输入顺序。
    pub fn local_overlay_text(&self) -> Option<String> {
        let builtin_text = self.builtin_command_text();
        let ime_text = self.ime_text();

        if builtin_text.is_empty() && ime_text.is_empty() {
            None
        } else {
            Some(format!("{builtin_text}{ime_text}"))
        }
    }

    /// 生成这次提交应该使用的命令行文本。
    ///
    /// 当存在选中项时，会按 `force_selected` 规则决定是否用选中命令替换当前前缀。
    fn builtin_command_submission_text(&self, force_selected: bool) -> Option<String> {
        let query = self.builtin_command_manager.query()?;
        let selected = self.builtin_command_manager.selected_command();
        Some(resolve_command_line(query, selected, force_selected))
    }

    /// 计算建议浮层在终端内的绘制位置。
    ///
    /// 浮层锚定在光标下一行，和命令输入所在的提示位置保持邻近。
    fn builtin_command_overlay_position(&self, cx: &App) -> Pixels {
        let term = self.terminal.read(cx);
        let content = term.last_content();
        let bounds = term.terminal_bounds();
        let cursor_line_in_viewport = content.cursor.line.0 + content.display_offset as i32;
        let overlay_row = (cursor_line_in_viewport + 1).max(0) as f32;

        let top = px((overlay_row * bounds.line_height.as_f32()).floor()) + px(6.0);

        top
    }

    /// 渲染内建命令建议浮层，并把鼠标选择转成一次显式补全。
    fn render_builtin_command_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement + 'static {
        use crate::builtin_commands::render_overlay;

        let suggestions = self.builtin_command_manager.suggestions();
        let top = self.builtin_command_overlay_position(cx);
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();
        let selected_index = self.builtin_command_manager.selected_index();
        let view = cx.entity().clone();

        render_overlay(
            suggestions,
            selected_index,
            top,
            theme,
            is_dark,
            move |index, _window, cx| {
                view.update(cx, |this, cx| {
                    this.mutate_builtin_command(cx, |manager| manager.pick(index));
                });
            },
        )
    }

    /// Clears the marked (pre-edit) text state.
    /// Called when composition is cancelled or completed.
    pub fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    /// 只有 本地/远程 shell 仍处于可接管输入的前台交互状态时，才允许 DeepTerm 拦截 `/`。
    fn can_trigger_builtin_command_here(&self, cx: &App) -> bool {
        self.terminal.read(cx).is_shell_in_foreground()
    }

    /// 判断一段新文本是否应该开启内建命令模式。
    ///
    /// 只有当前行仍允许 `/` 起始命令、当前处于 shell 提示符场景、且输入内容不是换行时才会进入。
    fn should_start_builtin_command(&self, text: &str, cx: &App) -> bool {
        !self.builtin_command_manager.is_active()
            && self.can_start_builtin_command
            && self.can_trigger_builtin_command_here(cx)
            && text.starts_with('/')
            && !text.contains(['\n', '\r'])
    }

    /// 把文本写入内建命令缓冲，并刷新浮层。
    fn update_builtin_command(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }

        self.mutate_builtin_command(cx, |manager| manager.update(text));
    }

    /// 清空内建命令状态。
    ///
    /// 只有原先处于激活状态时才触发一次通知，避免无意义刷新。
    fn clear_builtin_command(&mut self, cx: &mut Context<Self>) {
        let was_active = self.builtin_command_manager.is_active();
        self.builtin_command_manager.clear();

        if was_active {
            cx.notify();
        }
    }

    /// 从内建命令缓冲末尾删除一个字符。
    fn pop_builtin_command(&mut self, cx: &mut Context<Self>) {
        self.mutate_builtin_command(cx, BuiltinCommandManager::pop);
    }

    /// 尝试把文本路由到内建命令缓冲。
    ///
    /// 已经在内建命令模式下时会继续追加；否则只有满足起始条件时才会新建缓冲。
    fn route_text_to_builtin_command(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if text.is_empty() {
            return false;
        }

        if self.builtin_command_manager.is_active() || self.should_start_builtin_command(text, cx) {
            self.update_builtin_command(text, cx);
            return true;
        }

        false
    }

    /// 执行已经解析完成的内建命令。
    ///
    /// 返回 `true` 表示该命令由应用接管，不应再回落到 shell。
    fn execute_builtin_command(&mut self, command_line: &str, _cx: &mut Context<Self>) -> bool {
        match find_builtin_command(command_line) {
            Some(spec) => {
                log::info!("builtin command {}: {}", spec.name, spec.description);
                if spec.handler == BuiltinCommandHandler::OpenFileBrowser {
                    _cx.emit(Event::OpenFileBrowser {
                        target: self.parse_open_file_browser_target(command_line, _cx),
                    });
                } else if spec.handler == BuiltinCommandHandler::OpenFile {
                    let raw_path = command_line
                        .strip_prefix(spec.name)
                        .unwrap_or_default()
                        .trim();
                    let path = if self.is_remote(_cx) {
                        self.resolve_remote_open_target(raw_path, _cx)
                            .unwrap_or_else(|| raw_path.to_string())
                    } else {
                        self.resolve_open_target(raw_path, _cx)
                            .to_string_lossy()
                            .into_owned()
                    };
                    _cx.emit(Event::OpenFile { path });
                }
                true
            }
            None => false,
        }
    }

    fn parse_open_file_browser_target(&self, command_line: &str, cx: &App) -> FileBrowserTarget {
        let command = find_builtin_command(command_line)
            .expect("parse_open_file_browser_target should receive a builtin /open command");

        let raw_path = command_line
            .strip_prefix(command.name)
            .unwrap_or_default()
            .trim();
        if self.is_remote(cx) {
            return FileBrowserTarget::Remote {
                ssh_config: self
                    .ssh_config
                    .clone()
                    .expect("remote terminal should carry ssh config"),
                requested_path: if raw_path.is_empty() {
                    self.current_remote_working_directory(cx)
                } else {
                    self.resolve_remote_open_target(raw_path, cx)
                },
            };
        }

        let requested_path = if raw_path.is_empty() {
            self.current_working_directory(cx)
                .or_else(dirs::home_dir)
                .or(Some(PathBuf::from("/")))
        } else {
            Some(self.resolve_open_target(raw_path, cx))
        };

        FileBrowserTarget::Local { requested_path }
    }

    fn current_working_directory(&self, cx: &App) -> Option<PathBuf> {
        let process_cwd = self
            .terminal
            .read(cx)
            .child_process_id()
            .and_then(Self::query_process_working_directory);

        process_cwd.or_else(|| self.working_directory.clone())
    }

    fn current_remote_working_directory(&self, cx: &App) -> Option<String> {
        let terminal = self.terminal.read(cx);
        terminal
            .remote_working_directory()
            .map(str::to_string)
            .or_else(|| {
                terminal
                    .window_title()
                    .and_then(Self::parse_remote_working_directory_from_title)
            })
    }

    fn parse_remote_working_directory_from_title(title: &str) -> Option<String> {
        let title = title.trim();
        if title.is_empty() {
            return None;
        }

        let mut candidates = vec![title];
        if let Some((_, suffix)) = title.rsplit_once(": ") {
            candidates.push(suffix.trim());
        }
        if let Some((_, suffix)) = title.rsplit_once(':') {
            candidates.push(suffix.trim());
        }
        if let Some(index) = title.rfind(" /") {
            candidates.push(title[index + 1..].trim());
        }
        if let Some(index) = title.rfind("~/") {
            candidates.push(title[index..].trim());
        }
        if title.ends_with('~') {
            candidates.push("~");
        }

        for candidate in candidates {
            if let Some(path) = Self::normalize_remote_title_path(candidate) {
                return Some(path);
            }
        }

        title.find('/').and_then(|index| {
            let candidate = title[index..].trim();
            Self::normalize_remote_title_path(candidate)
        })
    }

    fn normalize_remote_title_path(candidate: &str) -> Option<String> {
        let candidate = candidate.trim().trim_matches(|c| matches!(c, '"' | '\''));
        if candidate.is_empty() {
            return None;
        }

        if candidate.starts_with('/') {
            return Some(candidate.to_string());
        }

        if candidate == "~" {
            return None;
        }

        candidate
            .strip_prefix("~/")
            .map(|stripped| format!("./{}", stripped))
    }

    #[cfg(target_os = "linux")]
    fn query_process_working_directory(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    #[cfg(target_os = "macos")]
    fn query_process_working_directory(pid: u32) -> Option<PathBuf> {
        let output = Command::new("lsof")
            .args(["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.strip_prefix('n').map(PathBuf::from))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn query_process_working_directory(_pid: u32) -> Option<PathBuf> {
        None
    }

    fn resolve_open_target(&self, raw_path: &str, cx: &App) -> PathBuf {
        let trimmed = raw_path.trim().trim_matches(|c| matches!(c, '"' | '\''));
        let expanded = if trimmed == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from(trimmed))
        } else if let Some(stripped) = trimmed.strip_prefix("~/") {
            dirs::home_dir()
                .map(|home| home.join(stripped))
                .unwrap_or_else(|| PathBuf::from(trimmed))
        } else {
            PathBuf::from(trimmed)
        };

        if expanded.is_absolute() {
            expanded
        } else {
            self.current_working_directory(cx)
                .or_else(|| self.working_directory.clone())
                .or_else(dirs::home_dir)
                .map(|cwd| cwd.join(&expanded))
                .unwrap_or(expanded)
        }
    }

    fn resolve_remote_open_target(&self, raw_path: &str, cx: &App) -> Option<String> {
        let trimmed = raw_path.trim().trim_matches(|c| matches!(c, '"' | '\''));
        if trimmed.is_empty() || trimmed == "~" {
            None
        } else if let Some(stripped) = trimmed.strip_prefix("~/") {
            Some(format!("./{}", stripped))
        } else if trimmed.starts_with('/') {
            Some(trimmed.to_string())
        } else {
            self.current_remote_working_directory(cx)
                .map(|cwd| sftp_fs::join_remote_path(&cwd, trimmed))
                .or_else(|| Some(trimmed.to_string()))
        }
    }

    /// 提交一条已经确定好的命令行。
    ///
    /// 如果不是应用内建命令，就把它按普通 shell 输入发回终端。
    fn submit_resolved_builtin_command(&mut self, command_line: String, cx: &mut Context<Self>) {
        self.finish_builtin_command_input(cx);

        if !self.execute_builtin_command(&command_line, cx) {
            self.send_command_line_to_shell(&command_line, cx);
        }
    }

    /// 解析并提交当前内建命令缓冲。
    fn submit_builtin_command(&mut self, force_selected: bool, cx: &mut Context<Self>) {
        let Some(command_line) = self.builtin_command_submission_text(force_selected) else {
            return;
        };

        self.submit_resolved_builtin_command(command_line, cx);
    }

    /// 将内置命令缓冲区的内容直接发送给 shell（不执行内置命令处理）
    fn send_builtin_command_to_shell(&mut self, cx: &mut Context<Self>) {
        let Some(command_line) = self.builtin_command_manager.query().map(|s| s.to_string()) else {
            return;
        };

        self.finish_builtin_command_input(cx);

        // 直接发送给 shell，不尝试执行内置命令
        self.send_command_line_to_shell(&command_line, cx);
    }

    /// 把一整行命令作为用户输入发回 shell，并自动补一个换行。
    fn send_command_line_to_shell(&mut self, command_line: &str, cx: &mut Context<Self>) {
        let mut data = command_line.as_bytes().to_vec();
        data.push(b'\n');
        self.write_with_broadcast(&data, cx);
    }

    /// 修改内建命令管理器状态，并统一触发一次刷新。
    fn mutate_builtin_command(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut BuiltinCommandManager),
    ) {
        f(&mut self.builtin_command_manager);
        cx.notify();
    }

    /// 结束当前内建命令输入，恢复普通终端输入状态。
    fn finish_builtin_command_input(&mut self, cx: &mut Context<Self>) {
        self.clear_marked_text(cx);
        self.clear_builtin_command(cx);
        self.can_start_builtin_command = true;
    }

    /// 判断某个按键在内建命令模式下是否应该被本地吞掉。
    ///
    /// 只要这个按键本来会被编码成终端输入，就不应该再透传到 PTY。
    fn should_swallow_builtin_command_key(
        &self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let mode = self.terminal.read(cx).last_content().mode;
        to_esc_str(&event.keystroke, &mode, false).is_some()
    }

    /// 处理内建命令模式下的按键。
    ///
    /// 这里优先消费导航、补全、取消、提交等按键，避免它们落到真实 shell。
    fn handle_builtin_command_key_down(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.builtin_command_manager.is_active() {
            return false;
        }

        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;

        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            if modifiers.control && key.eq_ignore_ascii_case("c") {
                self.finish_builtin_command_input(cx);
                return true;
            }

            // Ctrl+P / Ctrl+N 用于在列表中导航
            if modifiers.control && key.eq_ignore_ascii_case("p") {
                self.mutate_builtin_command(cx, BuiltinCommandManager::select_previous);
                return true;
            }
            if modifiers.control && key.eq_ignore_ascii_case("n") {
                self.mutate_builtin_command(cx, BuiltinCommandManager::select_next);
                return true;
            }

            if self.should_swallow_builtin_command_key(event, cx) {
                return true;
            }

            return false;
        }

        match key {
            "up" => {
                self.mutate_builtin_command(cx, BuiltinCommandManager::select_previous);
                true
            }
            "down" => {
                self.mutate_builtin_command(cx, BuiltinCommandManager::select_next);
                true
            }
            "tab" => {
                if modifiers.shift {
                    self.mutate_builtin_command(cx, BuiltinCommandManager::select_previous);
                } else {
                    // Tab 补全：将当前选中的命令替换到输入中
                    self.mutate_builtin_command(cx, BuiltinCommandManager::complete);
                }
                true
            }
            "backspace" => {
                self.pop_builtin_command(cx);
                self.can_start_builtin_command = !self.builtin_command_manager.is_active();
                true
            }
            "escape" => {
                self.clear_marked_text(cx);
                // ESC 关闭命令列表 UI，标记 dismiss 状态，但保留输入
                self.mutate_builtin_command(cx, BuiltinCommandManager::dismiss);
                true
            }
            "enter" | "return" => {
                if self.builtin_command_manager.is_dismissed() {
                    // 用户已取消，直接将命令发送给 shell
                    self.send_builtin_command_to_shell(cx);
                } else {
                    self.submit_builtin_command(false, cx);
                }
                true
            }
            "space" => {
                self.update_builtin_command(" ", cx);
                true
            }
            _ => self.should_swallow_builtin_command_key(event, cx),
        }
    }

    /// Commits (sends) the given text to the PTY.
    /// This is called when the user confirms the input (e.g., pressing Space to select).
    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.can_start_builtin_command = false;
            self.write_with_broadcast(text.as_bytes(), cx);
        }
    }

    /// 写入数据到终端，并可选地广播给其他终端
    fn write_with_broadcast(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _| {
            terminal.write(data);
        });
        // 如果有广播回调，调用它来广播输入
        if let Some(ref callback) = self.broadcast_callback {
            callback(data);
        }
    }

    /// 将拖拽的文件路径写入终端
    /// 参考 alacritty 的实现，将路径格式化后粘贴到终端
    fn add_paths_to_terminal(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        use itertools::Itertools;

        // 构建路径字符串，参考 alacritty 的实现：path + " "
        let text = paths
            .iter()
            .map(|path| {
                let path_str = path.to_string_lossy().into_owned();
                format!("{path_str} ")
            })
            .join("");

        // 过滤掉 \x1b 和 \x03，防止恶意输入破坏终端转义序列
        let filtered = text.replace(['\x1b', '\x03'], "");

        self.terminal.update(cx, |terminal, _| {
            let bracketed = terminal
                .last_content()
                .mode
                .contains(TermMode::BRACKETED_PASTE);

            if bracketed {
                // 使用 bracketed paste 模式
                terminal.write(b"\x1b[200~");
                terminal.write(filtered.as_bytes());
                terminal.write(b"\x1b[201~");
            } else {
                // 普通模式
                terminal.write(filtered.as_bytes());
            }
        });
    }

    // Actions
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            terminal.copy(cx);
        });
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        // Check if clipboard contains an image
        // If so, forward Ctrl+V to let the terminal application handle it
        if clipboard.entries.iter().any(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => !image.bytes.is_empty(),
            _ => false,
        }) {
            self.forward_ctrl_v(cx);
            return;
        }

        // Otherwise, paste text content with bracketed paste mode support
        if let Some(text) = clipboard.text() {
            if self.route_text_to_builtin_command(&text, cx) {
                return;
            }

            let bracketed = self.terminal.update(cx, |terminal, _| {
                terminal
                    .last_content()
                    .mode
                    .contains(TermMode::BRACKETED_PASTE)
            });

            if bracketed {
                // Use bracketed paste mode to preserve whitespace
                let mut data = Vec::new();
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(text.as_bytes());
                data.extend_from_slice(b"\x1b[201~");
                self.write_with_broadcast(&data, cx);
            } else {
                // Normal mode
                self.write_with_broadcast(text.as_bytes(), cx);
            }
        }
    }

    /// Forward Ctrl+V to the terminal as raw input
    /// This lets TUI applications handle clipboard images themselves
    fn forward_ctrl_v(&mut self, cx: &mut Context<Self>) {
        self.write_with_broadcast(&[CTRL_V], cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.clear();
        });
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.select_all();
        });
        cx.notify();
    }

    // Scrolling
    pub fn scroll_line_up(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_line_up();
        });
        cx.notify();
    }

    pub fn scroll_line_down(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_line_down();
        });
        cx.notify();
    }

    pub fn scroll_page_up(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_page_up();
        });
        cx.notify();
    }

    pub fn scroll_page_down(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_page_down();
        });
        cx.notify();
    }

    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_to_top();
        });
        cx.notify();
    }

    pub fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_to_bottom();
        });
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If user has scrolled up, pressing any key should scroll back to bottom
        let was_scrolled = self.terminal.update(cx, |terminal, _cx| {
            let display_offset = terminal.last_content().display_offset;
            if display_offset > 0 {
                terminal.scroll_to_bottom();
                true
            } else {
                false
            }
        });

        if was_scrolled {
            // Scroll back to bottom, then process the key
            cx.notify();
        }

        if self.handle_builtin_command_key_down(event, cx) {
            cx.stop_propagation();
            return;
        }

        let mut input_bytes: Option<Vec<u8>> = None;
        let handled = self.terminal.update(cx, |terminal, _cx| {
            let mode = terminal.last_content().mode;
            if let Some(bytes) = to_esc_str(&event.keystroke, &mode, false) {
                input_bytes = Some(bytes.to_vec());
                terminal.write(&bytes);
                true
            } else {
                false
            }
        });

        // 如果有输入数据且广播回调已设置，调用广播回调
        if let Some(ref bytes) = input_bytes {
            if let Some(ref callback) = self.broadcast_callback {
                callback(bytes);
            }
        }

        if handled {
            match event.keystroke.key.as_str() {
                "enter" | "return" => {
                    self.can_start_builtin_command = true;
                }
                "backspace" => {
                    if !self.builtin_command_manager.is_active() {
                        self.can_start_builtin_command = true;
                    }
                }
                _ if input_bytes.is_some() => {
                    // Only set can_start_builtin_command to false if the input is not '/'
                    // This allows starting a builtin command with '/' even after other characters
                    if let Some(ref bytes) = input_bytes {
                        if bytes != b"/" {
                            self.can_start_builtin_command = false;
                        }
                    }
                }
                _ => {}
            }

            // Stop propagation only if we handled the key
            cx.stop_propagation();
        }
        // Otherwise, let the event propagate to EntityInputHandler
        // which will handle regular character input (including uppercase letters)
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TerminalView {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.marked_text_range()
    }

    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    /// Called when text is committed (e.g., user pressed Space to confirm CJK input).
    /// This sends the text to the terminal and clears any marked text state.
    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_marked_text(cx);
        if self.route_text_to_builtin_command(text, cx) {
            return;
        }
        self.commit_text(text, cx);
    }

    /// Called during IME composition to update the pre-edit text.
    /// This does NOT send text to the terminal - it only updates the marked text state
    /// for display purposes (showing the candidate text while typing pinyin).
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_marked_text(new_text.to_string(), cx);
    }

    /// Called when IME composition is cancelled (e.g., user pressed Escape).
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.clear_marked_text(cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Only return bounds during active IME composition
        if self.ime_state.is_none() {
            return None;
        }
        self.terminal.update(cx, |terminal, _cx| {
            let content = terminal.last_content();
            let bounds = terminal.terminal_bounds();

            let cursor_line_in_viewport = content.cursor.line.0 + content.display_offset as i32;

            let viewport_lines = bounds.num_lines() as i32;
            if cursor_line_in_viewport < 0 || cursor_line_in_viewport >= viewport_lines {
                return None;
            }

            let cursor_x = bounds.bounds.left()
                + px((content.cursor.column.0 as f32 * bounds.cell_width.as_f32()).floor());
            let cursor_y = bounds.bounds.top()
                + px(cursor_line_in_viewport as f32 * bounds.line_height.as_f32());

            Some(Bounds {
                origin: point(cursor_x, cursor_y),
                size: gpui::Size {
                    width: bounds.cell_width,
                    height: bounds.line_height,
                },
            })
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self.terminal.clone();
        let focus_handle = self.focus_handle.clone();
        let view_entity = cx.entity().clone();
        let is_focused = focus_handle.is_focused(window);
        let show_scrollbar = self.should_show_scrollbar(cx);

        // Schedule a single refresh when the scrollbar should actually hide.
        // Polling every 100ms keeps repainting even when terminal content is unchanged.
        if show_scrollbar {
            let last_scroll_time = self.terminal.read(cx).last_scroll_time.get();
            let should_reschedule = self.scrollbar_hide_scheduled_at != Some(last_scroll_time);
            if should_reschedule {
                self.scrollbar_hide_task = None;
                self.scrollbar_hide_scheduled_at = Some(last_scroll_time);
            }
            if should_reschedule || self.scrollbar_hide_task.is_none() {
                let elapsed_since_scroll = last_scroll_time.elapsed();
                let hide_delay = SCROLLBAR_VISIBLE_AFTER_SCROLL
                    .checked_sub(elapsed_since_scroll)
                    .unwrap_or_else(|| Duration::from_millis(1));
                self.scrollbar_hide_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(hide_delay).await;
                    this.update(cx, |this, cx| {
                        if this.scrollbar_hide_scheduled_at == Some(last_scroll_time) {
                            this.scrollbar_hide_task = None;
                            this.scrollbar_hide_scheduled_at = None;
                            cx.notify();
                        }
                    })
                    .ok();
                }));
            }
        } else {
            self.scrollbar_hide_task = None;
            self.scrollbar_hide_scheduled_at = None;
        }

        div()
            .id("terminal-view")
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(CONTEXT)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(|this, _: &Copy, _window, cx| {
                this.copy(cx);
            }))
            .on_action(cx.listener(|this, _: &Paste, _window, cx| {
                this.paste(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.selecting = true;
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_down(event, cx);
                    });
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.selecting = false;
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_up(event, cx);
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                // Ignore mouse moves while a global drag is in progress (e.g. dragging a tab or panel)
                // to prevent terminal applications like tmux from receiving spurious drag events.
                if cx.has_active_drag() {
                    return;
                }

                this.terminal.update(cx, |terminal, cx| {
                    // Always send mouse move events in mouse mode (e.g., neovim)
                    // so the application can handle selection in real-time
                    if terminal.mouse_mode(event.modifiers.shift) {
                        terminal.mouse_move(event, cx);
                    }
                    // Also handle terminal selection if we're dragging
                    if this.selecting {
                        terminal.mouse_drag(event, cx);
                    }
                });
                cx.notify();
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                this.terminal.update(cx, |terminal, _cx| {
                    terminal.scroll_wheel(event, 3.0);
                });
                cx.stop_propagation();
                cx.notify();
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.add_paths_to_terminal(paths.paths(), cx);
            }))
            .child(
                // Use relative positioning: terminal fills container, scrollbar floats on top
                div()
                    .relative()
                    .size_full()
                    .child(
                        // Terminal content fills entire container
                        TerminalElement::new(
                            terminal.clone(),
                            focus_handle,
                            is_focused,
                            self.cursor_visible,
                            view_entity,
                        ),
                    )
                    .when(show_scrollbar, |this| {
                        // Scrollbar positioned absolutely on the right
                        // No background color - only the thumb is visible (Zed style)
                        this.child(
                            div()
                                .absolute()
                                .right(px(2.0))
                                .top_0()
                                .bottom_0()
                                .w(px(8.0))
                                .child(TerminalScrollbar::new(terminal, cx)),
                        )
                    })
                    .when(
                        self.builtin_command_manager.is_active()
                            && !self.builtin_command_manager.is_dismissed(),
                        |this| this.child(self.render_builtin_command_overlay(cx)),
                    )
                    // SSH 连接进度视图覆盖层 - 在连接建立期间覆盖终端内容
                    .when_some(self.ssh_connect_view.clone(), |this, connect_view| {
                        this.child(div().absolute().inset_0().child(connect_view))
                    }),
            )
    }
}
