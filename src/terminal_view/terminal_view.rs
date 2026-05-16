//! Terminal view component - Full featured

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EntityInputHandler, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString, Subscription, Task,
    UTF16Selection, Window, actions, div, point, px,
};
use gpui::{KeyBinding, prelude::*};
use unicode_width::UnicodeWidthStr;

use crate::file_browser::FileBrowserTarget;
use crate::file_transfer::TransferRequest;
use crate::ssh::platform::RemoteOS;
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
    /// 发起一次文件传输（由 /scp 等内建命令触发）
    StartTransfer { request: TransferRequest },
    /// SSH 重新连接请求
    SshReconnect { config: SshConnectionConfig },
    /// 远程操作系统已检测到
    RemoteOsDetected { os: RemoteOS },
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

use crate::builtin_commands::{BuiltinCommandManager, resolve_command_line};
use crate::ssh::SshConnectionConfig;
use crate::ssh_view::SshConnectView;
use crate::ssh_view::picker::ShowSshPicker;
use crate::terminal::mappings::keys::to_esc_str;
use crate::terminal::terminal::{Event as TerminalEvent, Terminal};
use crate::terminal_view::blink_cursor::BlinkCursor;
use crate::terminal_view::terminal_element::{
    FlattenedRenderCache, RowRenderCache, TerminalElement,
};
use crate::terminal_view::terminal_scrollbar::TerminalScrollbar;
use crate::theme::ActiveTheme;

// Define terminal actions
actions!(terminal, [Copy, Paste, Clear, SelectAll]);
const CONTEXT: &str = "Terminal";

/// Cursor blink policy. Mirrors Zed's `TerminalBlink` setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TerminalBlink {
    /// Never blink the cursor, regardless of what the application asks for.
    Off,
    /// Always blink the cursor.
    On,
    /// Default. Do not blink unless the running application explicitly
    /// enables blinking via DEC private mode `?12h` or CSI `Ps SP q`.
    #[default]
    TerminalControlled,
}
const SCROLLBAR_VISIBLE_AFTER_SCROLL: Duration = Duration::from_secs(2);

// Scroll velocity tracking constants
const SCROLL_VELOCITY_WINDOW_SIZE: usize = 3;
const SCROLL_MULTIPLIER_BASE: f32 = 3.0;
const SCROLL_MULTIPLIER_MAX: f32 = 8.0;
const SCROLL_VELOCITY_SCALE: f32 = 2000.0;

// Scrollbar thumb animation duration (seconds)
const SCROLLBAR_ANIMATION_DURATION: f32 = 0.15;
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

/// 当前鼠标悬停的可点击链接信息。
#[derive(Clone, Debug, PartialEq)]
pub struct HoveredLink {
    pub url: SharedString,
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

pub struct TerminalView {
    terminal: Entity<Terminal>,
    terminal_subscription: Option<Subscription>,
    working_directory: Option<PathBuf>,
    ssh_config: Option<SshConnectionConfig>,
    focus_handle: FocusHandle,
    /// Drives cursor blink on/off. `visible()` is the final signal
    /// consumed by the element.
    blink_manager: Entity<BlinkCursor>,
    /// Subscriptions that must stay alive for the blink manager to fire
    /// repaints and the focus in/out hooks to run.
    _blink_subscriptions: Vec<Subscription>,
    /// Current blink policy. See `TerminalBlink`.
    blink_setting: TerminalBlink,
    /// Whether the running application currently wants the cursor to blink
    /// (set via DEC private mode `?12` or CSI `Ps SP q`).
    app_requests_blink: bool,
    /// Track previous focus state so we can only react on edges.
    was_focused: bool,
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
    /// 检测到的远程操作系统（仅对SSH终端有意义）
    remote_os: Option<RemoteOS>,
    /// 当前鼠标悬停的可点击链接（仅在按住 Cmd/Ctrl 时有效）
    pub hovered_link: Option<HoveredLink>,
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
    // ── Scrollbar thumb animation ───────────────────────────────────────────
    /// Current animated thumb Y offset (relative to track top).
    pub scrollbar_thumb_y: Pixels,
    /// Target thumb Y offset for the ongoing animation.
    scrollbar_target_y: Pixels,
    /// Thumb Y offset at animation start.
    scrollbar_anim_from_y: Pixels,
    /// Animation start instant.
    scrollbar_anim_start: Instant,
    /// Whether a thumb-position animation is currently running.
    scrollbar_animating: bool,
    // ── Scroll velocity tracking ────────────────────────────────────────────
    /// Instant of the last scroll-wheel event (for velocity calculation).
    last_scroll_instant: Instant,
    /// Sliding window of recent scroll velocities (pixels / second).
    recent_scroll_velocities: VecDeque<f32>,
    /// Last scroll direction: -1 = up, 1 = down, 0 = none.
    last_scroll_direction: i32,
    // ── Toast notification ─────────────────────────────────────────────────
    /// 当前显示的 Toast 消息（错误提示等）
    toast_message: Option<String>,
    /// Toast 自动隐藏任务
    toast_hide_task: Option<Task<()>>,
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

/// 浮窗垂直锚点：贴住光标的哪一边。
///
/// 当光标在窗口顶部、下方有空间时用 `Top`，浮窗顶端贴近光标，列表向下展开；
/// 当光标在窗口底部、上方有空间时用 `Bottom`，浮窗底端贴近光标，列表从下往上增长。
/// 这两种锚点都保证「靠近光标的一边」始终贴合光标行。
use crate::builtin_commands::OverlayAnchor;

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

    /// Apply the current blink policy to the `BlinkCursor` entity.
    ///
    /// Called from `on_blink_changed` and on focus/policy transitions.
    fn sync_blink_manager(&self, focused: bool, cx: &mut Context<Self>) {
        let should_blink = focused
            && match self.blink_setting {
                TerminalBlink::Off => false,
                TerminalBlink::On => true,
                TerminalBlink::TerminalControlled => self.app_requests_blink,
            };

        self.blink_manager.update(cx, |blink, cx| {
            if should_blink {
                blink.start(cx);
            } else {
                blink.stop(cx);
            }
        });
    }

    /// React to a `BlinkChanged` event from the terminal (DEC ?12 or
    /// CSI `Ps SP q`). Only toggles the manager when the current policy
    /// actually consults the application's preference.
    fn on_blink_changed(&mut self, blinking: bool, cx: &mut Context<Self>) {
        self.app_requests_blink = blinking;
        if matches!(self.blink_setting, TerminalBlink::TerminalControlled) {
            self.sync_blink_manager(self.was_focused, cx);
        }
    }

    /// Override the blink policy. Exposed so that settings/UI can flip the
    /// behaviour at runtime without recreating the view.
    #[allow(dead_code)]
    pub fn set_blink_setting(&mut self, setting: TerminalBlink, cx: &mut Context<Self>) {
        if self.blink_setting == setting {
            return;
        }
        self.blink_setting = setting;
        self.sync_blink_manager(self.was_focused, cx);
    }

    /// Called on every key event that reaches the terminal input path.
    /// Keeps the caret solid for `PAUSE_DELAY` so typing never produces
    /// a mid-stroke blank frame.
    fn pause_blink(&self, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, |blink, cx| blink.pause(cx));
    }

    /// Returns whether the cursor should currently be painted. Failing the
    /// focused check renders the hollow/outline form of the cursor; the
    /// blink manager only runs while focused.
    fn should_show_cursor(&self, focused: bool, cx: &App) -> bool {
        if !focused {
            // When unfocused we always draw the hollow outline; element
            // maps Block->Hollow unconditionally.
            return true;
        }
        self.blink_manager.read(cx).visible()
    }

    /// Reacts to focus edges. Enables blinking on focus-in (subject to
    /// policy) and disables it on focus-out.
    fn on_focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.was_focused == focused {
            return;
        }
        self.was_focused = focused;
        self.sync_blink_manager(focused, cx);
    }

    /// 检查终端是否检测到新的远程操作系统，如果是则发出事件
    fn check_remote_os(&mut self, terminal: Entity<Terminal>, cx: &mut Context<Self>) {
        if let Some(os) = terminal.read(cx).remote_os {
            if self.remote_os != Some(os) {
                self.remote_os = Some(os);
                cx.emit(Event::RemoteOsDetected { os });
            }
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
                |this: &mut TerminalView, terminal, event: &TerminalEvent, cx| match event {
                    TerminalEvent::TitleChanged => {
                        this.update_title(cx);
                    }
                    TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                        this.check_remote_os(terminal.clone(), cx);
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                    // 本地终端不会触发 SshAbnormalDisconnect，不需要处理
                    TerminalEvent::SshAbnormalDisconnect => {}
                    TerminalEvent::BlinkChanged(blinking) => {
                        this.on_blink_changed(*blinking, cx);
                    }
                },
            );

            let blink_manager = cx.new(|_| BlinkCursor::new());
            let blink_subscriptions = vec![cx.observe(&blink_manager, |_, _, cx| cx.notify())];

            let mut this = Self {
                terminal: terminal_clone,
                terminal_subscription: Some(subscription),
                working_directory,
                ssh_config,
                focus_handle,
                blink_manager,
                _blink_subscriptions: blink_subscriptions,
                blink_setting: TerminalBlink::default(),
                app_requests_blink: false,
                was_focused: false,
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
                remote_os: None,
                hovered_link: None,
                row_render_cache: Vec::new(),
                flattened_render_cache: FlattenedRenderCache::default(),
                cached_theme_id: None,
                cached_selection_range: None,
                cached_selection_mouse_mode: false,
                scrollbar_thumb_y: px(0.0),
                scrollbar_target_y: px(0.0),
                scrollbar_anim_from_y: px(0.0),
                scrollbar_anim_start: Instant::now(),
                scrollbar_animating: false,
                last_scroll_instant: Instant::now(),
                recent_scroll_velocities: VecDeque::with_capacity(SCROLL_VELOCITY_WINDOW_SIZE),
                last_scroll_direction: 0,
                toast_message: None,
                toast_hide_task: None,
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
        Self::create_terminal_view(cx, terminal, working_dir, None, command_string.into())
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
                        this.check_remote_os(terminal.clone(), cx);
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                    TerminalEvent::SshAbnormalDisconnect => {
                        this.handle_ssh_abnormal_disconnect(cx);
                    }
                    TerminalEvent::BlinkChanged(blinking) => {
                        this.on_blink_changed(*blinking, cx);
                    }
                },
            );

            let blink_manager = cx.new(|_| BlinkCursor::new());
            let blink_subscriptions = vec![cx.observe(&blink_manager, |_, _, cx| cx.notify())];

            let mut this = Self {
                terminal: terminal_clone,
                terminal_subscription: Some(terminal_subscription),
                working_directory: None,
                ssh_config: Some(ssh_config_clone.clone()),
                focus_handle,
                blink_manager,
                _blink_subscriptions: blink_subscriptions,
                blink_setting: TerminalBlink::default(),
                app_requests_blink: false,
                was_focused: false,
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
                remote_os: None,
                hovered_link: None,
                row_render_cache: Vec::new(),
                flattened_render_cache: FlattenedRenderCache::default(),
                cached_theme_id: None,
                cached_selection_range: None,
                cached_selection_mouse_mode: false,
                scrollbar_thumb_y: px(0.0),
                scrollbar_target_y: px(0.0),
                scrollbar_anim_from_y: px(0.0),
                scrollbar_anim_start: Instant::now(),
                scrollbar_animating: false,
                last_scroll_instant: Instant::now(),
                recent_scroll_velocities: VecDeque::with_capacity(SCROLL_VELOCITY_WINDOW_SIZE),
                last_scroll_direction: 0,
                toast_message: None,
                toast_hide_task: None,
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
                TerminalEvent::BlinkChanged(blinking) => {
                    this.on_blink_changed(*blinking, cx);
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

    pub fn working_directory(&self) -> Option<&PathBuf> {
        self.working_directory.as_ref()
    }

    pub fn ssh_config(&self) -> Option<&SshConnectionConfig> {
        self.ssh_config.as_ref()
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

    /// Calculate the scrollbar thumb Y offset (relative to track top) from
    /// the current terminal `display_offset`.
    fn calculate_scrollbar_thumb_y(&self, cx: &App) -> Pixels {
        let term = self.terminal.read(cx);
        let content = term.last_content();
        let line_height = term.terminal_bounds.line_height;

        crate::terminal_view::terminal_scrollbar::calculate_thumb_metrics(
            content.total_lines,
            content.viewport_lines,
            content.display_offset,
            line_height,
            term.terminal_bounds.bounds.size.height,
        )
        .map(|(y, _)| y)
        .unwrap_or(px(0.0))
    }

    /// Kick off a short ease-out animation of the scrollbar thumb to `target_y`.
    fn animate_scrollbar(&mut self, target_y: Pixels) {
        self.scrollbar_anim_from_y = self.scrollbar_thumb_y;
        self.scrollbar_target_y = target_y;
        self.scrollbar_anim_start = Instant::now();
        self.scrollbar_animating = true;
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
        Some(resolve_command_line(
            query,
            selected.as_deref(),
            force_selected,
        ))
    }

    /// 计算建议浮层在终端内的绘制位置。
    ///
    /// 浮层是 `div().relative().size_full()` 的子元素，使用 `.absolute().top().left()`
    /// 定位，因此返回的坐标必须是**相对父级**的偏移，绝不能掺入 `terminal_bounds.bounds.origin`
    /// 那一份窗口绝对坐标，否则会把面板偏移加两遍。
    ///
    /// 终端内容只在水平方向有 `TERMINAL_HORIZONTAL_PADDING` 的 padding（见
    /// `terminal_element::terminal_content_bounds`），垂直方向无内边距。
    ///
    /// 返回 `(anchor, left)`：当 `anchor` 为 [`OverlayAnchor::Top`] 时浮窗用 `.top()` 锚定、
    /// 列表向下展开；当为 [`OverlayAnchor::Bottom`] 时浮窗用 `.bottom()` 锚定、
    /// 列表从下往上贴着光标增长（光标在窗口底部时使用，避免被裁切）。
    ///
    /// `visual_extra_rows` 与 `visual_col` 表示由内建命令缓冲文本换行后导致的
    /// 视觉光标偏移：当文本超过终端列宽自动折行时，浮窗应贴着用户**视觉上**看到的
    /// 光标行，而不是 PTY 光标行（否则会盖在用户正在输入的最后一行上）。
    fn builtin_command_overlay_position(
        &self,
        cx: &App,
        anchor_cells: f32,
        overlay_width: f32,
        overlay_height: f32,
        visual_extra_rows: usize,
        visual_col: usize,
    ) -> (OverlayAnchor, Pixels) {
        const TERMINAL_HORIZONTAL_PADDING: f32 = 4.0;

        let term = self.terminal.read(cx);
        let content = term.last_content();
        let bounds = term.terminal_bounds;
        let cursor_row_in_viewport =
            content.cursor.line.0.max(0) as f32 + visual_extra_rows as f32;
        // 当文本未换行时延用原有的 cursor_col + anchor_cells 偏移；换行后视觉光标列
        // 从 0 重新开始，anchor 也基于视觉列。
        let cursor_col = if visual_extra_rows == 0 {
            content.cursor.column.0 as f32 + anchor_cells
        } else {
            visual_col as f32
        };

        let cell_w = bounds.cell_width.as_f32();
        let line_h = bounds.line_height.as_f32();

        // 相对父级 (relative div = 终端元素整体) 的光标坐标
        let cursor_left = TERMINAL_HORIZONTAL_PADDING + (cursor_col * cell_w).floor();
        let cursor_top = (cursor_row_in_viewport * line_h).floor();

        // 父级宽高：terminal_bounds 是去掉左右 padding 后的内容区，加回两侧 padding 即父级尺寸
        let parent_width = bounds.bounds.size.width.as_f32() + 2.0 * TERMINAL_HORIZONTAL_PADDING;
        let parent_height = bounds.bounds.size.height.as_f32();

        const GAP: f32 = 2.0;
        const VIEWPORT_GUTTER: f32 = 6.0;
        const RIGHT_GUTTER: f32 = 16.0;

        let cursor_bottom = cursor_top + line_h;
        let space_below = parent_height - cursor_bottom;
        let space_above = cursor_top;

        // 优先在光标下方紧贴一行；下方放不下且上方更宽裕 ⇒ 切到上方锚点（列表从下往上增长）。
        // 这样光标贴近窗口底部时，浮窗顶端不会被裁，靠近光标的一边始终贴合。
        let anchor = if space_below >= overlay_height + GAP + VIEWPORT_GUTTER {
            // 下方足够 ⇒ 用 top
            let top = cursor_bottom + GAP;
            OverlayAnchor::Top(px(top))
        } else if space_above >= overlay_height + GAP + VIEWPORT_GUTTER {
            // 上方足够 ⇒ 用 bottom，距离父容器底部的偏移 = 父高度 - 光标行顶端 + GAP
            let bottom = (parent_height - cursor_top) + GAP;
            OverlayAnchor::Bottom(px(bottom))
        } else if space_below >= space_above {
            // 都放不下，下方略宽：top 贴底，列表内部 max_h 处理裁切
            let top = (parent_height - overlay_height - VIEWPORT_GUTTER).max(cursor_bottom + GAP);
            OverlayAnchor::Top(px(top.max(VIEWPORT_GUTTER)))
        } else {
            // 都放不下，上方略宽：bottom 贴顶
            let bottom = (parent_height - cursor_top + GAP).max(VIEWPORT_GUTTER);
            OverlayAnchor::Bottom(px(bottom))
        };

        let max_left = (parent_width - overlay_width - RIGHT_GUTTER).max(0.0);
        let left = cursor_left.max(0.0).min(max_left);

        (anchor, px(left))
    }

    fn builtin_command_path_anchor_cells(&self) -> f32 {
        let Some(buffer) = self.builtin_command_manager.query() else {
            return 0.0;
        };
        if !self.builtin_command_manager.is_argument_input() {
            return 0.0;
        }

        let anchor_byte = self
            .scp_path_anchor_byte(buffer)
            .or_else(|| self.generic_path_anchor_byte(buffer))
            .unwrap_or(0);
        UnicodeWidthStr::width(&buffer[..anchor_byte.min(buffer.len())]) as f32
    }

    fn generic_path_anchor_byte(&self, buffer: &str) -> Option<usize> {
        let command_end = buffer.find(char::is_whitespace)?;
        let args_start = buffer[command_end..]
            .char_indices()
            .find_map(|(offset, c)| (!c.is_whitespace()).then_some(command_end + offset))
            .unwrap_or(buffer.len());
        let args = &buffer[args_start..];
        let (dir_part, _) = crate::builtin_commands::PathCompletionEngine::split_path_prefix(args);
        Some(args_start + dir_part.len())
    }

    fn scp_path_anchor_byte(&self, buffer: &str) -> Option<usize> {
        if buffer.split_whitespace().next()? != "/scp" {
            return None;
        }

        let query = crate::builtin_commands::scp_completion::analyse(buffer)?;
        let raw_token = &buffer[query.token_start..query.token_end];
        match query.phase {
            crate::builtin_commands::ScpCompletionPhase::HostOrLocal { prefix } => {
                if prefix.starts_with('/')
                    || prefix.starts_with('.')
                    || prefix.starts_with('~')
                    || prefix.contains('/')
                {
                    let (dir_part, _) =
                        crate::builtin_commands::PathCompletionEngine::split_path_prefix(&prefix);
                    Some(query.token_start + dir_part.len().min(raw_token.len()))
                } else {
                    Some(query.token_start)
                }
            }
            crate::builtin_commands::ScpCompletionPhase::RemotePath { prefix, .. } => {
                let colon =
                    crate::builtin_commands::scp_completion::first_unquoted_colon(raw_token)?;
                let path_start = query.token_start + colon + 1;
                let (dir_part, _) =
                    crate::builtin_commands::PathCompletionEngine::split_path_prefix(&prefix);
                Some(path_start + dir_part.len().min(buffer.len().saturating_sub(path_start)))
            }
        }
    }

    /// 渲染内建命令建议浮层，并把鼠标选择转成一次显式补全。
    fn render_builtin_command_overlay(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();
        let view = cx.entity().clone();

        // 内建命令缓冲在面板内可能因列宽不足自动换行，浮窗需要贴着视觉光标行而非
        // PTY 光标行，否则会覆盖用户正在输入的最后一行。
        let (visual_extra_rows, visual_col) = self.builtin_command_visual_cursor(cx);

        if self.builtin_command_manager.is_argument_input() {
            let anchor_cells = self.builtin_command_path_anchor_cells();
            let (anchor, left) = self.builtin_command_overlay_position(
                cx,
                anchor_cells,
                320.0,
                288.0,
                visual_extra_rows,
                visual_col,
            );
            // 参数输入阶段：渲染路径补全浮层
            let path_completions = self.builtin_command_manager.path_completions.clone();
            let selected_index = self.builtin_command_manager.path_selected_index;
            let scroll_handle = self.builtin_command_manager.path_scroll_handle().clone();

            crate::builtin_commands::render_path_overlay(
                &path_completions,
                selected_index,
                anchor,
                left,
                theme,
                is_dark,
                &scroll_handle,
                move |index, _window, cx| {
                    view.update(cx, |this, cx| {
                        this.mutate_builtin_command(cx, |manager| manager.pick_path(index));
                    });
                },
            )
            .into_any_element()
        } else {
            let (anchor, _) = self.builtin_command_overlay_position(
                cx,
                0.0,
                640.0,
                460.0,
                visual_extra_rows,
                visual_col,
            );
            // 命令名输入阶段：渲染命令建议浮层
            let suggestions = self.builtin_command_manager.suggestions();
            let selected_index = self.builtin_command_manager.selected_index();

            crate::builtin_commands::render_overlay(
                suggestions,
                selected_index,
                anchor,
                theme,
                is_dark,
                move |index, _window, cx| {
                    view.update(cx, |this, cx| {
                        this.mutate_builtin_command(cx, |manager| manager.pick(index));
                    });
                },
            )
            .into_any_element()
        }
    }

    /// 推算内建命令文本换行后的视觉光标偏移。
    ///
    /// 返回 `(extra_rows, visual_col)`：
    /// - `extra_rows`：相对 PTY 光标行向下偏移的视觉行数（未换行时为 0）；
    /// - `visual_col`：换行后视觉光标所在列；未换行时调用方应继续使用 PTY 列。
    fn builtin_command_visual_cursor(&self, cx: &App) -> (usize, usize) {
        let builtin = self.builtin_command_text();
        let ime = self.ime_text();
        if builtin.is_empty() && ime.is_empty() {
            return (0, 0);
        }
        let term = self.terminal.read(cx);
        let bounds = term.terminal_bounds;
        let cols = bounds.num_columns();
        let cursor_col = term.last_content().cursor.column.0;
        let wrapped =
            crate::terminal_view::overlay_wrap::wrap_overlay_text(builtin, ime, cursor_col, cols);
        (wrapped.cursor_row, wrapped.cursor_col)
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
    fn execute_builtin_command(&mut self, command_line: &str, cx: &mut Context<Self>) -> bool {
        use crate::builtin_commands::executor::CommandResult;
        use crate::builtin_commands::executor::global_executor;
        use crate::builtin_commands::{
            ArgumentParser, CommandContext, PathResolver, global_registry,
        };

        // 提取命令名和参数
        let command_name = command_line
            .split_whitespace()
            .next()
            .unwrap_or(command_line);
        let argument = ArgumentParser::extract_argument(command_line, command_name);

        // 在注册表中查找命令
        let Some(command) = global_registry().find(command_name) else {
            return false;
        };

        log::info!("builtin command {}: {}", command.name, command.description);

        // 查询 shell 进程的实时工作目录
        let (process_cwd, remote_cwd) = if self.ssh_config.is_some() {
            // 远程终端：通过 OSC 序列动态追踪
            let remote_cwd = self
                .terminal
                .read(cx)
                .remote_working_directory()
                .map(String::from);
            (None, remote_cwd)
        } else {
            // 本地终端：通过操作系统查询 shell 进程实际 CWD
            let pid = self.terminal.read(cx).child_process_id;
            let process_cwd = pid
                .and_then(PathResolver::query_process_working_directory)
                .or_else(|| self.working_directory.clone());
            (process_cwd, None)
        };

        // 构建命令上下文
        let ctx = CommandContext {
            terminal_view: Some(cx.entity()),
            working_directory: self.working_directory.clone(),
            process_cwd,
            remote_cwd,
            is_remote: self.ssh_config.is_some(),
            ssh_config: self.ssh_config.clone(),
        };

        // 执行命令
        let result = {
            let executor = global_executor().lock().unwrap();
            executor.execute(&command, argument, ctx)
        };

        match result {
            Some(CommandResult::OpenFileBrowser { target }) => {
                cx.emit(Event::OpenFileBrowser { target });
                true
            }
            Some(CommandResult::OpenFile { path }) => {
                cx.emit(Event::OpenFile { path });
                true
            }
            Some(CommandResult::StartTransfer { request }) => {
                cx.emit(Event::StartTransfer { request });
                true
            }
            Some(CommandResult::Log { .. }) => {
                // LogOnly 命令不需要发射事件
                true
            }
            Some(CommandResult::None) => true,
            Some(CommandResult::Error { message }) => {
                log::warn!("Command error: {}", message);
                self.show_toast(message, cx);
                true
            }
            Some(CommandResult::FallThrough { shell_command }) => {
                // Handler 做完副作用 / 预处理，把（原始或改写过的）文本交给 shell
                let to_send = shell_command.unwrap_or_else(|| command_line.to_string());
                self.send_command_line_to_shell(&to_send, cx);
                true
            }
            None => false,
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
        // 同步"本机端 cwd"：本地终端用 shell 进程实时 cwd（lsof / /proc），
        // 远程终端用 DeepTerm 进程的初始工作目录。补全引擎拿到的 cwd 必须是
        // 当前终端 shell 的实际位置，而不是 DeepTerm 启动时所在的项目目录。
        let local_cwd = if self.ssh_config.is_some() {
            self.working_directory.clone()
        } else {
            let pid = self.terminal.read(cx).child_process_id;
            pid.and_then(crate::builtin_commands::PathResolver::query_process_working_directory)
                .or_else(|| self.working_directory.clone())
        };
        self.builtin_command_manager.set_local_cwd(local_cwd);

        // 注入"当前终端"的连接上下文：
        // - 本地终端 → ssh = None
        // - 远程终端 → ssh = Some(...)，remote_cwd 取 OSC 7 上报的实时路径
        // 这样 `/scp` 没有显式 host 的 token 会跟随当前终端所在主机
        // （远程终端走 SFTP 列远端目录，本地终端列本机文件）。
        let terminal_remote_cwd = if self.ssh_config.is_some() {
            self.terminal
                .read(cx)
                .remote_working_directory()
                .map(String::from)
        } else {
            None
        };
        self.builtin_command_manager
            .set_terminal_context(self.ssh_config.clone(), terminal_remote_cwd);

        // 进入 /scp 之前先把保存的 SSH 连接注入管理器，供 host / 远端目录补全使用。
        // 调用点可能很多（每次输入字符），所以仅在当前缓冲以 `/scp` 开头时才加载。
        let should_refresh_hosts = self
            .builtin_command_manager
            .query()
            .map(|q| q.trim_start().starts_with("/scp"))
            .unwrap_or(false);
        if should_refresh_hosts {
            let configs = crate::ssh::config::SshGlobalConfig::load().saved_connections;
            self.builtin_command_manager.set_ssh_configs(configs);
        }

        f(&mut self.builtin_command_manager);

        // 远端目录按需加载：如果管理器标记为"loading"，取出请求并后台读取
        while let Some(request) = self.builtin_command_manager.pending_remote_request() {
            self.spawn_remote_dir_load(request, cx);
        }

        cx.notify();
    }

    /// 后台读取远端目录并把结果回写到 `BuiltinCommandManager`。
    fn spawn_remote_dir_load(
        &self,
        request: crate::builtin_commands::RemoteCompletionRequest,
        cx: &mut Context<Self>,
    ) {
        let saved = crate::ssh::config::SshGlobalConfig::load().saved_connections;
        let Some(config) = saved.iter().find(|c| c.id() == request.config_id).cloned() else {
            // 配置已删：直接回传空列表避免死等，并提示用户刷新配置
            let config_id = request.config_id.clone();
            let dir = request.absolute_dir.clone();
            cx.spawn(async move |this, cx| {
                this.update(cx, |this, cx| {
                    this.builtin_command_manager.apply_remote_dir_result(
                        config_id,
                        dir,
                        Err("SSH 配置已被删除或修改，请刷新连接列表后重试".into()),
                    );
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        };
        let abs_dir = request.absolute_dir.clone();
        let cfg_id = request.config_id.clone();

        cx.spawn(async move |this, cx| {
            let abs_dir_task = abs_dir.clone();
            let config_task = config.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let effective = if abs_dir_task == "~" {
                        crate::ssh::sftp_fs::with_cached_session(&config_task, |session| {
                            session
                                .canonicalize(".")
                                .or_else(|_| session.canonicalize("~"))
                        })
                        .unwrap_or_else(|_| "/".to_string())
                    } else {
                        abs_dir_task
                    };
                    let entries = crate::ssh::sftp_fs::read_dir(&config_task, &effective);
                    (effective, entries)
                })
                .await;

            let (canonical_dir, read) = result;
            let payload = match read {
                Ok(entries) => {
                    let list: Vec<crate::builtin_commands::RemoteEntrySnapshot> = entries
                        .into_iter()
                        .map(|e| crate::builtin_commands::RemoteEntrySnapshot {
                            is_directory: matches!(
                                e.kind,
                                crate::ssh::sftp_fs::RemoteFileKind::Directory
                            ),
                            full_path: e.path,
                            name: e.name,
                        })
                        .collect();
                    Ok(list)
                }
                Err(err) => Err(format!("{}", err)),
            };
            this.update(cx, |this, cx| {
                // canonical 与 original 都写一次，避免 prefix 用 "~" 查不到
                let original_dir = abs_dir.clone();
                let duplicated = canonical_dir != original_dir;
                this.builtin_command_manager.apply_remote_dir_result(
                    cfg_id.clone(),
                    original_dir,
                    payload.clone(),
                );
                if duplicated {
                    this.builtin_command_manager.apply_remote_dir_result(
                        cfg_id,
                        canonical_dir,
                        payload,
                    );
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 显示 Toast 错误通知
    fn show_toast(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.toast_message = Some(message);

        // 取消之前的隐藏任务
        self.toast_hide_task.take();

        // 4秒后自动隐藏
        self.toast_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            this.update(cx, |this, cx| {
                this.toast_message = None;
                this.toast_hide_task = None;
                cx.notify();
            })
            .ok();
        }));

        cx.notify();
    }

    /// 渲染 Toast 通知条
    fn render_toast(&self, cx: &mut Context<Self>) -> impl IntoElement + 'static {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();

        div()
            .absolute()
            .top(px(8.0))
            .left(px(0.0))
            .right(px(0.0))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(theme.error.opacity(if is_dark { 0.15 } else { 0.1 }))
                    .border_1()
                    .border_color(theme.error.opacity(0.3))
                    .child(
                        gpui::svg()
                            .size(px(16.0))
                            .text_color(theme.error)
                            .path(crate::assets::Icon::WindowClose.path()),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .child(self.toast_message.clone().unwrap_or_default()),
                    ),
            )
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

        // 当前输入阶段（为未来可能的分支保留；Tab 目前统一用于切换高亮）
        let _is_argument_phase = self.builtin_command_manager.is_argument_input();

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
                // Tab / Shift+Tab 仅在补全列表中切换高亮，不直接插入。
                if modifiers.shift {
                    self.mutate_builtin_command(cx, BuiltinCommandManager::select_previous);
                } else {
                    self.mutate_builtin_command(cx, BuiltinCommandManager::select_next);
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
                // 回车的三段语义（参考 Warp 的渐进补全流程）：
                //  A. 浮层 visible 且**有高亮项** ⇒ 应用补全；之后判断该命令是否还需更多
                //     参数，是则追加空格触发下一阶段浮层（auto-advance），否则 hide 浮层。
                //  B. 浮层 visible 但**未高亮**（用户没按 ↓/Tab） ⇒ 仅关浮层，不补全也不
                //     提交；用户接着输入字符会自动重开浮层。
                //  C. 浮层 hidden（HiddenAfterPick / DismissedUntilReset） ⇒ 提交命令。
                let mgr = &self.builtin_command_manager;
                let overlay_visible_with_results =
                    !mgr.is_overlay_hidden() && mgr.should_render_overlay();
                let has_highlight = mgr.has_active_completion();

                if has_highlight {
                    let is_arg = mgr.is_argument_input();
                    self.mutate_builtin_command(cx, |manager| {
                        if is_arg {
                            manager.complete_path();
                        } else {
                            manager.complete();
                        }
                        manager.advance_after_pick();
                    });
                } else if overlay_visible_with_results {
                    // B：用户没选任何项，只是想关掉浮层
                    self.mutate_builtin_command(cx, BuiltinCommandManager::hide_overlay);
                } else if self.builtin_command_manager.is_dismissed() {
                    // 用户已显式 ESC 取消，直接把命令发给 shell
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
        // Keep the cursor solid while the user is actively typing.
        self.pause_blink(cx);

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
                    // 输入 '/' 时重置标志，确保内建命令模式能正常启动；
                    // 输入其他字符时设为 false，避免行中误触发
                    if let Some(ref bytes) = input_bytes {
                        if bytes == b"/" {
                            self.can_start_builtin_command = true;
                        } else {
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
        self.pause_blink(cx);
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
            let bounds = terminal.terminal_bounds;

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
        // React to focus edges so we can start/stop blinking.
        self.on_focus_changed(is_focused, cx);
        let cursor_visible = self.should_show_cursor(is_focused, cx);
        let show_scrollbar = self.should_show_scrollbar(cx);

        // Pre-read terminal state needed below to avoid redundant entity locks.
        let (scrollbar_drag_offset, last_scroll_time) = {
            let term = self.terminal.read(cx);
            (
                term.scrollbar_drag_offset.get(),
                term.last_scroll_time.get(),
            )
        };

        // ── Scrollbar thumb position ─────────────────────────────────────────
        // While dragging, snap to the exact position and kill any animation.
        let scrollbar_thumb_y = if scrollbar_drag_offset.is_some() {
            self.scrollbar_animating = false;
            self.scrollbar_thumb_y = self.calculate_scrollbar_thumb_y(cx);
            self.scrollbar_thumb_y
        } else if self.scrollbar_animating {
            let elapsed = self.scrollbar_anim_start.elapsed().as_secs_f32();
            let duration = SCROLLBAR_ANIMATION_DURATION;
            let t = (elapsed / duration).min(1.0);
            let ease = 1.0 - (1.0 - t).powi(3);
            self.scrollbar_thumb_y = px(self.scrollbar_anim_from_y.as_f32()
                + (self.scrollbar_target_y.as_f32() - self.scrollbar_anim_from_y.as_f32()) * ease);
            if t < 1.0 {
                cx.notify();
            } else {
                self.scrollbar_animating = false;
            }
            self.scrollbar_thumb_y
        } else {
            self.scrollbar_thumb_y
        };

        // Schedule a single refresh when the scrollbar should actually hide.
        // Polling every 100ms keeps repainting even when terminal content is unchanged.
        if show_scrollbar {
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
            .cursor(
                if self.hovered_link.is_some() {
                    CursorStyle::PointingHand
                } else {
                    CursorStyle::IBeam
                },
            )
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
                    #[cfg(target_os = "macos")]
                    let is_cmd = event.modifiers.platform;
                    #[cfg(not(target_os = "macos"))]
                    let is_cmd = event.modifiers.control;

                    if is_cmd {
                        if let Some(ref link) = this.hovered_link {
                            if let Err(e) = open::that(link.url.as_ref()) {
                                log::warn!("Failed to open URL '{}': {}", link.url, e);
                            }
                            return;
                        }
                    }

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

                // Detect link modifier key state (Cmd on macOS, Ctrl elsewhere)
                #[cfg(target_os = "macos")]
                let modifiers_pressed = event.modifiers.platform;
                #[cfg(not(target_os = "macos"))]
                let modifiers_pressed = event.modifiers.control;

                // URL hover detection (only when modifier is held)
                let link = if modifiers_pressed {
                    this.terminal.update(cx, |terminal, _cx| {
                        let position = event.position - terminal.terminal_bounds.bounds.origin;
                        let cell_width = terminal.terminal_bounds.cell_width;
                        let line_height = terminal.terminal_bounds.line_height;
                        let content = terminal.last_content();

                        if position.x < px(0.0) || position.y < px(0.0) {
                            return None;
                        }
                        let col = (position.x.as_f32() / cell_width.as_f32()).floor() as usize;
                        let row = (position.y.as_f32() / line_height.as_f32()).floor() as usize;
                        if col >= content.cols || row >= content.rows {
                            return None;
                        }
                        terminal
                            .url_range_at_viewport_position(row, col)
                            .map(|(url, start_col, end_col)| HoveredLink {
                                url: url.into(),
                                row,
                                start_col,
                                end_col,
                            })
                    })
                } else {
                    None
                };

                let link_changed = this.hovered_link != link;
                this.hovered_link = link;
                if link_changed {
                    cx.notify();
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
                let now = Instant::now();
                let elapsed = this.last_scroll_instant.elapsed().as_secs_f32();
                this.last_scroll_instant = now;

                let line_height = this.terminal.read(cx).terminal_bounds.line_height;

                // Compute instantaneous velocity (pixels / second).
                let delta_px = event.delta.pixel_delta(line_height).y.as_f32().abs();
                let velocity = if elapsed > 0.001 {
                    delta_px / elapsed
                } else {
                    0.0
                };

                // Maintain a sliding window of recent velocities.
                this.recent_scroll_velocities.push_back(velocity);
                if this.recent_scroll_velocities.len() > SCROLL_VELOCITY_WINDOW_SIZE {
                    this.recent_scroll_velocities.pop_front();
                }
                let avg_velocity: f32 = this.recent_scroll_velocities.iter().sum::<f32>()
                    / this.recent_scroll_velocities.len().max(1) as f32;

                // Dynamic multiplier: base + velocity bonus (capped at max).
                let dynamic_multiplier = (SCROLL_MULTIPLIER_BASE
                    + avg_velocity / SCROLL_VELOCITY_SCALE)
                    .min(SCROLL_MULTIPLIER_MAX);

                // Detect direction change and tell Terminal to reset accumulator.
                let current_direction = event.delta.pixel_delta(line_height).y.signum() as i32;
                let direction_changed = current_direction != 0
                    && this.last_scroll_direction != 0
                    && current_direction != this.last_scroll_direction;
                if direction_changed {
                    this.terminal.update(cx, |t, _cx| {
                        t.reset_scroll_px();
                    });
                }
                this.last_scroll_direction = current_direction;

                this.terminal.update(cx, |terminal, _cx| {
                    terminal.scroll_wheel(event, dynamic_multiplier);
                });
                let target_y = this.calculate_scrollbar_thumb_y(cx);
                this.animate_scrollbar(target_y);
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
                            cursor_visible,
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
                                .child(TerminalScrollbar::new(
                                    terminal,
                                    Some(scrollbar_thumb_y),
                                    cx,
                                )),
                        )
                    })
                    .when(
                        self.builtin_command_manager.should_render_overlay(),
                        |this| this.child(self.render_builtin_command_overlay(cx)),
                    )
                    .when(self.toast_message.is_some(), |this| {
                        this.child(self.render_toast(cx))
                    })
                    // SSH 连接进度视图覆盖层 - 在连接建立期间覆盖终端内容
                    .when_some(self.ssh_connect_view.clone(), |this, connect_view| {
                        this.child(div().absolute().inset_0().child(connect_view))
                    }),
            )
    }
}
