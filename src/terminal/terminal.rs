//! Terminal module - simplified but functional

use std::{borrow::Cow, cell::Cell, os::fd::AsRawFd, path::PathBuf, rc::Rc, sync::Arc, time::Instant};

use alacritty_terminal::{
    Term,
    event::{Event as AlacTermEvent, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll as AlacScroll},
    index::{Column, Line, Point as AlacPoint, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, TermDamage, TermMode},
    tty::{self},
    vte::ansi::{CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle, Handler},
};
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, Context, Entity, EventEmitter, Pixels, Point, Size,
    Task, Window, px,
};

use crate::ssh::keychain::{PreferredSshTransport, preferred_ssh_transport};
use crate::ssh::{RusshTerminal, SshConnectionConfig, SshPtyTerminal};
use crate::terminal::clipboard::{handle_osc52, parse_osc52};
use crate::terminal::mappings::mouse::{
    alt_scroll, grid_point, grid_point_and_side, mouse_button_report, mouse_moved_report,
    scroll_report,
};
use crate::terminal_view::terminal_view::CTRL_V;

/// 最大滚动缓冲区行数限制
const MAX_SCROLLBACK_LINES: usize = 100000;

/// 终端类型 - 区分本地和远程终端
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalType {
    /// 本地终端（通过PTY）
    Local,
    /// 远程SSH终端
    Remote {
        /// 主机地址
        host: String,
        /// 用户名
        username: String,
        /// 端口
        port: u16,
    },
}

/// Terminal events
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    TitleChanged,
    CloseTerminal,
    Bell,
    Wakeup,
    SelectionsChanged,
}

#[derive(Clone)]
pub struct ZedListener(pub UnboundedSender<AlacTermEvent>);

impl EventListener for ZedListener {
    fn send_event(&self, event: AlacTermEvent) {
        self.0.unbounded_send(event).ok();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalBounds {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub bounds: Bounds<Pixels>,
}

impl TerminalBounds {
    pub fn new(line_height: Pixels, cell_width: Pixels, bounds: Bounds<Pixels>) -> Self {
        TerminalBounds {
            cell_width,
            line_height,
            bounds,
        }
    }

    pub fn num_lines(&self) -> usize {
        (self.bounds.size.height.as_f32() / self.line_height.as_f32()).floor() as usize
    }

    pub fn num_columns(&self) -> usize {
        (self.bounds.size.width.as_f32() / self.cell_width.as_f32()).floor() as usize
    }

    pub fn last_column(&self) -> Column {
        Column(self.num_columns().saturating_sub(1))
    }

    pub fn bottommost_line(&self) -> Line {
        Line(self.num_lines().saturating_sub(1) as i32)
    }

    pub fn width(&self) -> Pixels {
        self.bounds.size.width
    }

    pub fn height(&self) -> Pixels {
        self.bounds.size.height
    }
}

impl Default for TerminalBounds {
    fn default() -> Self {
        TerminalBounds::new(
            px(16.0),
            px(9.0),
            Bounds {
                origin: Point::default(),
                size: Size {
                    width: px(500.0),
                    height: px(300.0),
                },
            },
        )
    }
}

impl From<TerminalBounds> for WindowSize {
    fn from(val: TerminalBounds) -> Self {
        WindowSize {
            num_lines: val.num_lines() as u16,
            num_cols: val.num_columns() as u16,
            cell_width: val.cell_width.as_f32() as u16,
            cell_height: val.line_height.as_f32() as u16,
        }
    }
}

impl Dimensions for TerminalBounds {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines()
    }

    fn columns(&self) -> usize {
        self.num_columns()
    }
}

/// Selection phase
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionPhase {
    Ended,
    Selecting,
    Started,
}

/// Terminal content
#[derive(Clone, Debug)]
pub struct TerminalContent {
    pub cells: Vec<IndexedCell>,
    /// Contiguous row ranges into `cells`, in visible row order.
    /// Each tuple is `(start, end)` where `cells[start..end]` belongs to one visible row.
    pub row_ranges: Vec<(usize, usize)>,
    pub cursor: AlacPoint,
    pub cursor_cell: alacritty_terminal::term::cell::Cell,
    /// 光标形状 - 用于光标委托机制
    /// 当光标形状为Hidden时，终端不渲染光标，由应用程序（如Claude Code）自己处理
    pub cursor_shape: AlacCursorShape,
    pub display_offset: usize,
    pub mode: alacritty_terminal::term::TermMode,
    pub cols: usize,
    pub rows: usize,
    /// Total lines including scrollback history (cached to avoid extra mutex lock)
    pub total_lines: usize,
    /// Visible viewport lines (cached to avoid extra mutex lock)
    pub viewport_lines: usize,
}

impl Default for TerminalContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            row_ranges: Vec::new(),
            cursor: AlacPoint::new(Line(0), Column(0)),
            cursor_cell: alacritty_terminal::term::cell::Cell::default(),
            cursor_shape: AlacCursorShape::Block,
            display_offset: 0,
            mode: alacritty_terminal::term::TermMode::default(),
            cols: 80,
            rows: 24,
            total_lines: 24,
            viewport_lines: 24,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexedCell {
    pub point: AlacPoint,
    pub cell: alacritty_terminal::term::cell::Cell,
}

/// 远程终端输入回调类型
pub type RemoteInputCallback = Box<dyn Fn(&[u8]) + Send>;

/// 远程终端调整大小回调类型
pub type RemoteResizeCallback = Box<dyn Fn(u16, u16) + Send>;

pub struct Terminal {
    term: Arc<FairMutex<Term<ZedListener>>>,
    /// 本地 PTY 启动出来的首个子进程 pid，通常就是 shell 进程。
    child_process_id: Option<u32>,
    /// shell 所在的进程组 id，用来判断当前前台程序是否仍然是 shell。
    shell_process_group_id: Option<i32>,
    /// 本地 PTY master fd，用来读取当前 TTY 的前台进程组。
    pty_master_fd: Option<i32>,
    remote_working_directory: Option<String>,
    window_title: Option<String>,
    /// 本地终端的事件循环发送器
    sender: Option<EventLoopSender>,
    last_content: TerminalContent,
    content_generation: u64,
    content_dirty: bool,
    /// 每次 update_last_content 后，标记哪些 viewport 行（0 = 顶行）发生了变化
    dirty_rows: Vec<bool>,
    /// 每次 update_last_content 后，记录 viewport 行的脏区列范围 `(left, right)`。
    /// 若该行无变更则为 `None`；全量刷新时所有行为 `Some((0, cols-1))`。
    dirty_row_bounds: Vec<Option<(usize, usize)>>,
    /// 事件循环任务（保持任务存活，防止被drop）
    #[allow(dead_code)]
    event_loop_task: Option<Task<()>>,
    terminal_bounds: TerminalBounds,
    selection_phase: SelectionPhase,
    last_mouse_position: Option<Point<Pixels>>,
    /// Scroll accumulator for smooth scrolling (trackpad support)
    /// Accumulates partial scroll amounts and applies them when they reach a full line
    scroll_px: Pixels,
    /// Scrollbar drag state - stores the y-offset within the thumb where dragging started
    /// This is stored in Terminal to persist across render cycles
    pub scrollbar_drag_offset: Rc<Cell<Option<Pixels>>>,
    /// Last time the terminal was scrolled (for scrollbar auto-hide)
    pub last_scroll_time: Cell<Instant>,
    /// 终端类型（本地或远程）
    terminal_type: TerminalType,
    /// 远程终端输入回调（用于发送数据到SSH）
    remote_input_callback: Option<RemoteInputCallback>,
    /// 远程终端调整大小回调
    remote_resize_callback: Option<RemoteResizeCallback>,
    /// russh 句柄（用于查询 SSH 连接状态）
    russh_handle: Option<RusshTerminal>,
}

impl Terminal {
    fn default_terminal_config() -> Config {
        let mut config = Config::default();
        config.scrolling_history = 10000.min(MAX_SCROLLBACK_LINES);
        config.default_cursor_style = AlacCursorStyle {
            shape: AlacCursorShape::Block,
            blinking: false,
        };
        config
    }

    fn build_local_terminal(
        cx: &mut App,
        pty_options: tty::Options,
        pty_error_message: String,
    ) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());
        let default_bounds = TerminalBounds::default();
        let pty = match tty::new(&pty_options, default_bounds.into(), 0) {
            Ok(pty) => pty,
            Err(err) => {
                eprintln!("{}: {}", pty_error_message, err);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };
        let term = Arc::new(FairMutex::new(Term::new(
            Self::default_terminal_config(),
            &default_bounds,
            listener.clone(),
        )));
        let child_process_id = Some(pty.child().id());
        #[cfg(unix)]
        let shell_process_group_id = match unsafe { libc::getpgid(pty.child().id() as libc::pid_t) } {
            pgid if pgid >= 0 => Some(pgid),
            _ => None,
        };
        #[cfg(not(unix))]
        let shell_process_group_id = None;
        #[cfg(unix)]
        let pty_master_fd = Some(pty.file().as_raw_fd());
        #[cfg(not(unix))]
        let pty_master_fd = None;
        let event_loop = match EventLoop::new(term.clone(), listener, pty, false, false) {
            Ok(event_loop) => event_loop,
            Err(e) => {
                eprintln!("Failed to create EventLoop: {}", e);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };
        let notifier = event_loop.channel();
        let _ = event_loop.spawn();

        cx.new(|cx| {
            let term_clone = term.clone();
            let event_loop_task = cx.spawn(async move |this, cx| {
                let mut events_rx = events_rx;
                while let Some(event) = events_rx.next().await {
                    let _ = this.update(cx, |terminal: &mut Self, cx| {
                        terminal.process_event(event, cx);
                    });
                    smol::future::yield_now().await;
                }
            });

            Self {
                term: term_clone,
                child_process_id,
                shell_process_group_id,
                pty_master_fd,
                remote_working_directory: None,
                window_title: None,
                sender: Some(notifier),
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                last_mouse_position: None,
                scroll_px: px(0.0),
                scrollbar_drag_offset: Rc::new(Cell::new(None)),
                last_scroll_time: Cell::new(Instant::now()),
                terminal_type: TerminalType::Local,
                remote_input_callback: None,
                remote_resize_callback: None,
                russh_handle: None,
            }
        })
    }

    pub fn new(cx: &mut App, working_directory: Option<PathBuf>) -> Entity<Self> {
        // Choose a sensible default shell per platform. On Windows prefer `COMSPEC` or `cmd.exe`;
        // on Unix-like systems prefer the `SHELL` env var or `/bin/sh`.
        let default_shell = if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        };

        Self::build_local_terminal(
            cx,
            tty::Options {
                shell: Some(tty::Shell::new(default_shell, vec![])),
                working_directory,
                env: crate::terminal::mappings::default_terminal_env(),
                ..Default::default()
            },
            "Failed to create PTY".to_string(),
        )
    }

    /// 使用指定命令创建新的本地终端
    ///
    /// # 参数
    ///
    /// * `cx` - GPUI 上下文
    /// * `working_directory` - 工作目录
    /// * `command` - 要执行的命令（如 "powershell.exe", "cmd.exe", "wsl.exe"）
    /// * `args` - 命令参数
    pub fn new_with_command(
        cx: &mut App,
        working_directory: Option<PathBuf>,
        command: &str,
        args: &[String],
    ) -> Entity<Self> {
        Self::build_local_terminal(
            cx,
            tty::Options {
                shell: Some(tty::Shell::new(command.to_string(), args.to_vec())),
                working_directory,
                env: crate::terminal::mappings::default_terminal_env(),
                ..Default::default()
            },
            format!("Failed to create PTY for command '{command}'"),
        )
    }

    fn create_error_terminal() -> Self {
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TerminalBounds::default(),
            ZedListener(unbounded().0),
        )));

        Self {
            term,
            child_process_id: None,
            shell_process_group_id: None,
            pty_master_fd: None,
            remote_working_directory: None,
            window_title: None,
            sender: None,
            last_content: TerminalContent::default(),
            content_generation: 0,
            content_dirty: false,
            dirty_rows: Vec::new(),
            dirty_row_bounds: Vec::new(),
            event_loop_task: None,
            terminal_bounds: TerminalBounds::default(),
            selection_phase: SelectionPhase::Ended,
            last_mouse_position: None,
            scroll_px: px(0.0),
            scrollbar_drag_offset: Rc::new(Cell::new(None)),
            last_scroll_time: Cell::new(Instant::now()),
            terminal_type: TerminalType::Local,
            remote_input_callback: None,
            remote_resize_callback: None,
            russh_handle: None,
        }
    }

    /// 创建新的 SSH 远程终端
    ///
    /// # 参数
    ///
    /// * `cx` - GPUI 上下文
    /// * `ssh_config` - SSH 连接配置
    /// * `on_data` - 数据发送回调，用于发送数据到 SSH 通道
    /// * `on_resize` - 调整大小回调，用于通知远程主机窗口大小变化
    pub fn new_remote<F, R>(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        on_data: F,
        on_resize: R,
    ) -> Entity<Self>
    where
        F: Fn(&[u8]) + Send + 'static,
        R: Fn(u16, u16) + Send + 'static,
    {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());

        let default_bounds = TerminalBounds::default();

        let term = Arc::new(FairMutex::new(Term::new(
            Self::default_terminal_config(),
            &default_bounds,
            listener.clone(),
        )));

        // 创建远程终端
        cx.new(|cx| {
            let term_clone = term.clone();

            let event_loop_task = cx.spawn(async move |this, cx| {
                let mut events_rx = events_rx;
                while let Some(event) = events_rx.next().await {
                    let _ = this.update(cx, |terminal: &mut Self, cx| {
                        terminal.process_event(event, cx);
                    });
                    smol::future::yield_now().await;
                }
            });

            Self {
                term: term_clone,
                child_process_id: None,
                shell_process_group_id: None,
                pty_master_fd: None,
                remote_working_directory: None,
                window_title: None,
                sender: None, // 远程终端不使用本地 sender
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                last_mouse_position: None,
                scroll_px: px(0.0),
                scrollbar_drag_offset: Rc::new(Cell::new(None)),
                last_scroll_time: Cell::new(Instant::now()),
                terminal_type: TerminalType::Remote {
                    host: ssh_config.host.clone(),
                    username: ssh_config.username.clone(),
                    port: ssh_config.port,
                },
                remote_input_callback: Some(Box::new(on_data)),
                remote_resize_callback: Some(Box::new(on_resize)),
                russh_handle: None,
            }
        })
    }

    /// 创建新的 SSH 远程终端
    ///
    /// 优先使用 russh 库进行原生 SSH 连接，采用异步初始化：
    /// - 立即返回终端面板，不阻塞 GPUI 渲染线程
    /// - 在后台建立 SSH 连接
    /// - 连接期间缓存输入数据，连接成功后自动转发
    /// - 若 russh 连接失败，自动回退到 PTY + 系统 ssh 命令方式
    pub fn new_ssh(cx: &mut App, ssh_config: &SshConnectionConfig) -> Entity<Self> {
        match preferred_ssh_transport(&ssh_config.auth_method) {
            Ok(PreferredSshTransport::Pty) => {
                log::info!("SSH: 该凭据类型使用系统 ssh PTY 模式");
                Self::new_ssh_pty(cx, ssh_config)
            }
            Ok(PreferredSshTransport::Russh) | Err(_) => Self::new_ssh_russh(cx, ssh_config),
        }
    }

    fn new_ssh_russh(cx: &mut App, ssh_config: &SshConnectionConfig) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());

        let default_bounds = TerminalBounds::default();

        let term = Arc::new(FairMutex::new(Term::new(
            Self::default_terminal_config(),
            &default_bounds,
            listener.clone(),
        )));

        // 优先尝试 russh 原生连接（使用与 Term 初始化的 default_bounds 一致的大小）
        log::info!("SSH: 启动 russh 异步连接...");
        let initial_cols = default_bounds.num_columns() as u32;
        let initial_rows = default_bounds.num_lines() as u32;

        // 创建异步 russh 终端（立即返回，后台连接）
        let russh_term = RusshTerminal::new(
            ssh_config,
            initial_cols,
            initial_rows,
            term.clone(),
            listener.clone(),
        );

        // 创建终端实体（立即返回，不阻塞 GPUI）
        let russh_for_input = russh_term.clone();
        let russh_for_resize = russh_term.clone();
        let russh_for_status = russh_term.clone();
        Self::create_remote_terminal(
            cx,
            ssh_config,
            term.clone(),
            events_rx,
            move |data| russh_for_input.write(data),
            move |cols, rows| russh_for_resize.resize(cols, rows),
            Some(russh_for_status),
        )
    }

    fn new_ssh_pty(cx: &mut App, ssh_config: &SshConnectionConfig) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());
        let default_bounds = TerminalBounds::default();

        let term = Arc::new(FairMutex::new(Term::new(
            Self::default_terminal_config(),
            &default_bounds,
            listener.clone(),
        )));

        let pty_terminal =
            match SshPtyTerminal::new(ssh_config, default_bounds, term.clone(), listener) {
                Ok(pty_terminal) => pty_terminal,
                Err(err) => {
                    log::error!("SSH PTY 初始化失败: {}", err);
                    return cx.new(|_cx| Self::create_error_terminal());
                }
            };

        let pty_input = pty_terminal.clone();
        let pty_resize = pty_terminal.clone();

        Self::create_remote_terminal(
            cx,
            ssh_config,
            term,
            events_rx,
            move |data| pty_input.write(data),
            move |cols, rows| {
                let bounds = TerminalBounds::new(
                    default_bounds.line_height,
                    default_bounds.cell_width,
                    Bounds {
                        origin: Point::default(),
                        size: Size {
                            width: px(cols as f32 * default_bounds.cell_width.as_f32()),
                            height: px(rows as f32 * default_bounds.line_height.as_f32()),
                        },
                    },
                );
                pty_resize.resize(bounds);
            },
            None,
        )
    }

    /// 通用远程终端实体创建（抽取 create_russh_terminal 和 create_ssh_pty_terminal 的公共部分）
    fn create_remote_terminal(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        term: Arc<FairMutex<Term<ZedListener>>>,
        events_rx: UnboundedReceiver<AlacTermEvent>,
        input_handler: impl Fn(&[u8]) + Send + Sync + 'static,
        resize_handler: impl Fn(u16, u16) + Send + Sync + 'static,
        russh_handle: Option<RusshTerminal>,
    ) -> Entity<Self> {
        let default_bounds = TerminalBounds::default();
        let ssh_config_clone = ssh_config.clone();

        cx.new(|cx| {
            let term_clone = term.clone();

            let event_loop_task = cx.spawn(async move |this, cx| {
                let mut events_rx = events_rx;
                while let Some(event) = events_rx.next().await {
                    let _ = this.update(cx, |terminal: &mut Self, cx| {
                        terminal.process_event(event, cx);
                    });
                    smol::future::yield_now().await;
                }
            });

            Self {
                term: term_clone,
                child_process_id: None,
                shell_process_group_id: None,
                pty_master_fd: None,
                remote_working_directory: None,
                window_title: None,
                sender: None,
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                last_mouse_position: None,
                scroll_px: px(0.0),
                scrollbar_drag_offset: Rc::new(Cell::new(None)),
                last_scroll_time: Cell::new(Instant::now()),
                terminal_type: TerminalType::Remote {
                    host: ssh_config_clone.host.clone(),
                    username: ssh_config_clone.username.clone(),
                    port: ssh_config_clone.port,
                },
                remote_input_callback: Some(Box::new(input_handler)),
                remote_resize_callback: Some(Box::new(resize_handler)),
                russh_handle,
            }
        })
    }

    /// 获取终端类型
    pub fn terminal_type(&self) -> &TerminalType {
        &self.terminal_type
    }

    /// 检查是否是远程终端
    pub fn is_remote(&self) -> bool {
        matches!(self.terminal_type, TerminalType::Remote { .. })
    }

    /// 获取远程连接信息（如果是远程终端）
    pub fn remote_info(&self) -> Option<(&str, &str, u16)> {
        match &self.terminal_type {
            TerminalType::Remote {
                host,
                username,
                port,
            } => Some((host.as_str(), username.as_str(), *port)),
            _ => None,
        }
    }

    pub fn child_process_id(&self) -> Option<u32> {
        self.child_process_id
    }

    /// 判断当前 TTY 的前台进程组是否仍然是 shell。
    /// 若不是，说明前台已有其他交互式程序接管输入。
    pub fn is_shell_in_foreground(&self) -> bool {
        #[cfg(unix)]
        {
            match self.pty_master_fd {
                Some(fd) => match unsafe { libc::tcgetpgrp(fd) } {
                    pgid if pgid >= 0 => self.shell_process_group_id == Some(pgid),
                    _ => false,
                },
                None => false,
            }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn window_title(&self) -> Option<&str> {
        self.window_title.as_deref()
    }

    pub fn remote_working_directory(&self) -> Option<&str> {
        self.remote_working_directory.as_deref()
    }

    /// 检查 SSH 连接是否已就绪（russh 已连接）
    pub fn is_ssh_connected(&self) -> bool {
        self.russh_handle.as_ref().map_or(true, |h| h.is_ready())
    }

    /// 检查 SSH 连接是否已失败
    pub fn is_ssh_failed(&self) -> bool {
        self.russh_handle.as_ref().is_some_and(|h| h.is_failed())
    }

    /// 获取 SSH 连接错误信息（如果连接失败）
    pub fn ssh_error_message(&self) -> Option<String> {
        self.russh_handle.as_ref()?.error_message()
    }

    /// 处理从 SSH 接收的数据
    pub fn process_ssh_data(&mut self, data: &[u8], cx: &mut Context<Self>) {
        log::debug!("Terminal 处理 SSH 数据: {} 字节", data.len());

        // 先拦截需要由应用层消费的 OSC 序列，比如 OSC 52 与 cwd 更新。
        let filtered_data = self.extract_and_handle_osc_sequences(data, cx);

        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        // 创建处理器 (使用默认的 StdSyncHandler 作为超时处理器)
        let mut processor = Processor::<StdSyncHandler>::new();

        // 获取终端锁并处理数据
        let mut term = self.term.lock();

        // 处理过滤后的数据（逐字节处理）
        for byte in filtered_data.as_ref() {
            processor.advance(&mut *term, *byte);
        }

        // 标记内容已更新
        self.content_dirty = true;
        self.content_generation += 1;

        log::debug!("SSH 数据处理完成");
    }

    /// 提取需要由应用层处理的 OSC 序列，返回过滤后的数据。
    /// 使用 Cow 实现零拷贝快速路径 - 当没有 OSC 序列时直接返回借用数据。
    fn extract_and_handle_osc_sequences<'a>(
        &mut self,
        data: &'a [u8],
        cx: &mut Context<Self>,
    ) -> Cow<'a, [u8]> {
        // 快速路径: 检查是否包含 OSC 开始序列
        if !data.windows(2).any(|w| w == &[0x1b, b']']) {
            return Cow::Borrowed(data); // 没有 OSC 序列，零拷贝返回
        }

        // 慢速路径: 需要过滤 OSC 序列
        let mut result = Vec::with_capacity(data.len());
        let mut i = 0;

        while i < data.len() {
            // 查找 OSC 序列开始: ESC ] 或 \x1b ]
            if i + 1 < data.len() && data[i] == 0x1b && data[i + 1] == b']' {
                // 找到了 OSC 开始，尝试提取完整序列
                if let Some((osc_end, osc_content)) = Terminal::extract_osc_sequence(&data[i..]) {
                    if self.try_handle_osc_sequence(cx, osc_content) {
                        // 已由应用层处理，跳过这个序列
                        i += osc_end;
                        continue;
                    }
                    // 不是应用层关心的 OSC，保留给 VTE 自己处理
                    result.extend_from_slice(&data[i..i + osc_end]);
                    i += osc_end;
                    continue;
                }
            }
            // 普通字节，保留
            result.push(data[i]);
            i += 1;
        }

        Cow::Owned(result)
    }

    fn try_handle_osc_sequence(&mut self, cx: &mut Context<Self>, osc_content: &[u8]) -> bool {
        if Self::try_handle_osc52(cx, osc_content) {
            return true;
        }

        if let Some(path) = Self::parse_remote_working_directory_osc(osc_content) {
            self.remote_working_directory = Some(path);
            return true;
        }

        false
    }

    /// 尝试处理 OSC 52 序列
    /// 返回 true 如果成功处理，false 如果不是 OSC 52
    fn try_handle_osc52(cx: &mut Context<Self>, osc_content: &[u8]) -> bool {
        // 快速检查 OSC 52 前缀 "52;" (避免不必要的 UTF-8 转换)
        if osc_content.len() < 3
            || osc_content[0] != b'5'
            || osc_content[1] != b'2'
            || osc_content[2] != b';'
        {
            return false;
        }

        // OSC 52 内容是 ASCII (base64)，可以安全地使用 from_utf8_unchecked
        let content_str = unsafe { std::str::from_utf8_unchecked(osc_content) };

        if let Some(operation) = parse_osc52(content_str) {
            handle_osc52(cx, &operation);
            true
        } else {
            false
        }
    }

    fn parse_remote_working_directory_osc(osc_content: &[u8]) -> Option<String> {
        let content = std::str::from_utf8(osc_content).ok()?.trim();

        if let Some(uri) = content.strip_prefix("7;") {
            return Self::parse_osc7_file_uri(uri.trim());
        }

        if let Some(path) = content.strip_prefix("1337;CurrentDir=") {
            return Self::normalize_remote_working_directory(path.trim());
        }

        if let Some(path) = content.strip_prefix("633;P;Cwd=") {
            return Self::normalize_remote_working_directory(path.trim());
        }

        None
    }

    fn parse_osc7_file_uri(uri: &str) -> Option<String> {
        let path = if let Some(path) = uri.strip_prefix("file://") {
            let slash_index = path.find('/')?;
            &path[slash_index..]
        } else {
            uri
        };

        Self::normalize_remote_working_directory(path)
    }

    fn normalize_remote_working_directory(path: &str) -> Option<String> {
        let path = path.trim();
        if !path.starts_with('/') {
            return None;
        }

        Some(Self::decode_pct_path(path))
    }

    fn decode_pct_path(path: &str) -> String {
        let bytes = path.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;

        while index < bytes.len() {
            if bytes[index] == b'%' && index + 2 < bytes.len() {
                let hex = [bytes[index + 1], bytes[index + 2]];
                if let Ok(hex) = std::str::from_utf8(&hex)
                    && let Ok(value) = u8::from_str_radix(hex, 16)
                {
                    decoded.push(value);
                    index += 3;
                    continue;
                }
            }

            decoded.push(bytes[index]);
            index += 1;
        }

        String::from_utf8(decoded).unwrap_or_else(|_| path.to_string())
    }

    /// 提取完整的 OSC 序列
    /// 返回 (序列长度, 序列内容不含 OSC 前缀)
    fn extract_osc_sequence(data: &[u8]) -> Option<(usize, &[u8])> {
        // OSC 序列格式: ESC ] <content> BEL
        //           或: ESC ] <content> ESC \
        if data.len() < 2 || data[0] != 0x1b || data[1] != b']' {
            return None;
        }

        // 查找 BEL 结束符 (0x07)
        if let Some(pos) = data[2..].iter().position(|&b| b == 0x07) {
            return Some((pos + 3, &data[2..pos + 2]));
        }

        // 查找 ESC \ 结束符 (0x1b 0x5c)
        if let Some(pos) = data[2..].windows(2).position(|w| w == &[0x1b, b'\\']) {
            return Some((pos + 4, &data[2..pos + 2]));
        }

        // 未找到结束符，序列不完整
        None
    }

    fn process_event(&mut self, event: AlacTermEvent, cx: &mut Context<Self>) {
        match event {
            AlacTermEvent::Title(title) => {
                self.window_title = Some(title);
                cx.emit(Event::TitleChanged);
            }
            AlacTermEvent::Wakeup => {
                self.content_generation += 1;
                self.content_dirty = true;
                cx.emit(Event::Wakeup);
            }
            AlacTermEvent::Bell => {
                cx.emit(Event::Bell);
            }
            AlacTermEvent::Exit | AlacTermEvent::ChildExit(_) => {
                cx.emit(Event::CloseTerminal);
            }
            AlacTermEvent::ClipboardStore(_, data) => {
                // 处理 OSC 52 剪贴板存储请求
                cx.write_to_clipboard(ClipboardItem::new_string(data));
            }
            AlacTermEvent::ClipboardLoad(_clipboard_type, format) => {
                // 响应剪贴板查询请求 (XTGETTCAP Ms)
                let clipboard_content = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text().map(|t| t.to_string()))
                    .unwrap_or_default();
                let response = format(&clipboard_content);
                self.write(response.as_bytes());
            }
            AlacTermEvent::TextAreaSizeRequest(format) => {
                // 响应窗口大小查询 (CSI t)
                let window_size = alacritty_terminal::event::WindowSize {
                    num_lines: self.terminal_bounds.num_lines() as u16,
                    num_cols: self.terminal_bounds.num_columns() as u16,
                    cell_width: self.terminal_bounds.cell_width.as_f32() as u16,
                    cell_height: self.terminal_bounds.line_height.as_f32() as u16,
                };
                let response = format(window_size);
                self.write(response.as_bytes());
            }
            AlacTermEvent::PtyWrite(text) => {
                // 直接写入 PTY
                self.write(text.as_bytes());
            }
            AlacTermEvent::MouseCursorDirty => {
                // 鼠标光标形状可能需要变化，触发刷新
                cx.emit(Event::Wakeup);
            }
            AlacTermEvent::ResetTitle => {
                self.window_title = None;
                cx.emit(Event::TitleChanged);
            }
            AlacTermEvent::ColorRequest(index, format) => {
                // 响应颜色查询请求
                let term = self.term.lock();
                let color = term.colors()[index]
                    .map(|rgb| rgb)
                    .unwrap_or_else(|| alacritty_terminal::vte::ansi::Rgb { r: 0, g: 0, b: 0 });
                drop(term);
                let response = format(color);
                self.write(response.as_bytes());
            }
            AlacTermEvent::CursorBlinkingChange => {
                // Ignore cursor blinking changes for now.
                // They can trigger frequent whole-terminal repaints with no content damage,
                // which is disproportionately expensive in the current GPUI renderer.
            }
        }
    }

    fn update_last_content(&mut self) {
        let mut term = self.term.lock();

        // Collect damage info while we hold the lock, before resetting.
        let num_rows = self.terminal_bounds.num_lines();
        let num_cols = self.terminal_bounds.num_columns();
        let damage = term.damage();
        self.dirty_rows.clear();
        self.dirty_row_bounds.clear();
        match damage {
            TermDamage::Full => {
                self.dirty_rows.resize(num_rows, true);
                if num_cols == 0 {
                    self.dirty_row_bounds.resize(num_rows, None);
                } else {
                    self.dirty_row_bounds
                        .resize(num_rows, Some((0, num_cols.saturating_sub(1))));
                }
            }
            TermDamage::Partial(iter) => {
                self.dirty_rows.resize(num_rows, false);
                self.dirty_row_bounds.resize(num_rows, None);
                for line_damage in iter {
                    let row = line_damage.line;
                    if row < num_rows {
                        self.dirty_rows[row] = true;
                        self.dirty_row_bounds[row] = Some((line_damage.left, line_damage.right));
                    }
                }
            }
        }
        term.reset_damage();

        let content = term.renderable_content();

        let estimated_size = content.display_iter.size_hint().0;
        let cells = &mut self.last_content.cells;
        cells.clear();
        if cells.capacity() < estimated_size {
            cells.reserve(estimated_size);
        }

        cells.extend(content.display_iter.map(|ic| IndexedCell {
            point: ic.point,
            cell: ic.cell.clone(),
        }));

        let row_ranges = &mut self.last_content.row_ranges;
        row_ranges.clear();
        row_ranges.reserve(num_rows.max(cells.len() / 4));
        let mut start = 0usize;
        while start < cells.len() {
            let line = cells[start].point.line;
            let mut end = start + 1;
            while end < cells.len() && cells[end].point.line == line {
                end += 1;
            }
            row_ranges.push((start, end));
            start = end;
        }

        self.last_content.cursor = content.cursor.point;
        self.last_content.cursor_cell = term.grid()[content.cursor.point].clone();
        // 获取光标形状 - 用于光标委托机制
        // 当光标形状为Hidden时，终端不渲染光标，由应用程序自己处理
        self.last_content.cursor_shape = content.cursor.shape;
        self.last_content.display_offset = content.display_offset;
        self.last_content.mode = content.mode;
        self.last_content.cols = self.terminal_bounds.num_columns();
        self.last_content.rows = self.terminal_bounds.num_lines();
        // Cache line counts while we already hold the lock (avoids extra mutex grabs per frame)
        self.last_content.total_lines = term.total_lines();
        self.last_content.viewport_lines = term.screen_lines();
    }

    pub fn sync(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.content_dirty {
            self.update_last_content();
            self.content_dirty = false;
        } else {
            // Nothing changed this frame – tell the element all rows are clean.
            self.dirty_rows.fill(false);
            self.dirty_row_bounds.fill(None);
        }
    }

    /// Returns which viewport rows (0 = top) changed since the last `sync()`.
    pub fn dirty_rows(&self) -> &[bool] {
        &self.dirty_rows
    }

    /// Returns viewport-row damage bounds `(left, right)` for the last `sync()`.
    pub fn dirty_row_bounds(&self) -> &[Option<(usize, usize)>] {
        &self.dirty_row_bounds
    }

    pub fn write(&mut self, data: &[u8]) {
        // 本地终端：通过 sender 发送到 PTY
        if let Some(ref sender) = self.sender {
            let msg = Msg::Input(Cow::Owned(data.to_vec()));
            let _ = sender.send(msg);
            return;
        }

        // 远程终端：通过回调发送到 SSH 通道
        if let Some(ref callback) = self.remote_input_callback {
            callback(data);
        }
    }

    pub fn resize(&mut self, bounds: TerminalBounds, cx: &mut Context<Self>) {
        let cols = bounds.num_columns();
        let rows = bounds.num_lines();

        // Guard against invalid resize dimensions
        if cols == 0 || rows == 0 {
            log::warn!("Skipping resize with invalid dimensions: {}x{}", cols, rows);
            return;
        }

        self.terminal_bounds = bounds;

        let cols_u16 = cols as u16;
        let rows_u16 = rows as u16;

        // 本地终端：通过 sender 发送 resize 消息
        if let Some(ref sender) = self.sender {
            let window_size: WindowSize = bounds.into();
            let _ = sender.send(Msg::Resize(window_size));
        }

        // 远程终端：通过回调通知 SSH 通道调整窗口大小
        if let Some(ref callback) = self.remote_resize_callback {
            callback(cols_u16, rows_u16);
        }

        let mut term = self.term.lock();
        term.resize(alacritty_terminal::term::test::TermSize::new(cols, rows));

        drop(term);
        self.update_last_content();
        cx.emit(Event::Wakeup);
    }

    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    /// Get the total number of lines in the terminal including scrollback history
    pub fn total_lines(&self) -> usize {
        let term = self.term.lock();
        term.total_lines()
    }

    /// Get the number of visible lines in the viewport (screen lines)
    pub fn viewport_lines(&self) -> usize {
        let term = self.term.lock();
        term.screen_lines()
    }

    pub fn terminal_bounds(&self) -> TerminalBounds {
        self.terminal_bounds
    }

    pub fn set_terminal_bounds(&mut self, bounds: TerminalBounds) {
        self.terminal_bounds = bounds;
    }

    // Mouse handling
    pub fn mouse_mode(&self, shift: bool) -> bool {
        if shift {
            return false;
        }
        let term = self.term.lock();
        let mode = term.mode();
        mode.intersects(TermMode::MOUSE_MODE)
    }

    pub fn mouse_down(&mut self, e: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        // Convert to local coordinates relative to terminal bounds
        let position = e.position - self.terminal_bounds.bounds.origin;
        let point = grid_point(
            position,
            self.terminal_bounds,
            self.last_content.display_offset,
        );

        if self.mouse_mode(e.modifiers.shift) {
            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, true, self.last_content.mode)
            {
                self.write(&bytes);
            }
        } else {
            if let gpui::MouseButton::Left = e.button {
                let (point, side) = grid_point_and_side(
                    position,
                    self.terminal_bounds,
                    self.last_content.display_offset,
                );

                let selection_type = match e.click_count {
                    0 => return,
                    1 => Some(SelectionType::Simple),
                    2 => Some(SelectionType::Semantic),
                    3 => Some(SelectionType::Lines),
                    _ => None,
                };

                if let Some(sel_type) = selection_type {
                    let mut term = self.term.lock();
                    let selection = Selection::new(sel_type, point, side);
                    term.selection = Some(selection);
                    self.selection_phase = SelectionPhase::Started;
                    cx.emit(Event::SelectionsChanged);
                }
            }
        }
    }

    pub fn mouse_up(&mut self, e: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        // Convert to local coordinates relative to terminal bounds
        let position = e.position - self.terminal_bounds.bounds.origin;

        if self.mouse_mode(e.modifiers.shift) {
            let point = grid_point(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );

            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, false, self.last_content.mode)
            {
                self.write(&bytes);
            }
        } else {
            if e.button == gpui::MouseButton::Left {
                let term = self.term.lock();
                if let Some(text) = term.selection_to_string() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
        }

        self.selection_phase = SelectionPhase::Ended;
    }

    pub fn mouse_drag(&mut self, e: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.mouse_mode(e.modifiers.shift) && self.selection_phase != SelectionPhase::Ended {
            self.selection_phase = SelectionPhase::Selecting;
            // Convert to local coordinates relative to terminal bounds
            let position = e.position - self.terminal_bounds.bounds.origin;
            let (point, side) = grid_point_and_side(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );

            let mut term = self.term.lock();
            if let Some(mut selection) = term.selection.take() {
                selection.update(point, side);
                term.selection = Some(selection);
                cx.emit(Event::SelectionsChanged);
            }
        }
    }

    pub fn mouse_move(&mut self, e: &gpui::MouseMoveEvent, _cx: &mut Context<Self>) {
        // Convert to local coordinates relative to terminal bounds
        let position = e.position - self.terminal_bounds.bounds.origin;
        self.last_mouse_position = Some(position);

        if self.mouse_mode(e.modifiers.shift) {
            let (point, _side) = grid_point_and_side(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );

            if let Some(bytes) =
                mouse_moved_report(point, e.pressed_button, e.modifiers, self.last_content.mode)
            {
                self.write(&bytes);
            }
        }
    }

    /// Scroll the terminal
    pub fn scroll_wheel(&mut self, e: &gpui::ScrollWheelEvent, scroll_multiplier: f32) {
        let mouse_mode = self.mouse_mode(e.shift);
        let scroll_multiplier = if mouse_mode { 1. } else { scroll_multiplier };

        if let Some(scroll_lines) = self.determine_scroll_lines(e, scroll_multiplier)
            && scroll_lines != 0
        {
            if mouse_mode {
                let point = grid_point(
                    e.position - self.terminal_bounds.bounds.origin,
                    self.terminal_bounds,
                    self.last_content.display_offset,
                );

                if let Some(scrolls) = scroll_report(point, scroll_lines, e, self.last_content.mode)
                {
                    for scroll in scrolls {
                        self.write(&scroll);
                    }
                };
            } else if self
                .last_content
                .mode
                .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
                && !e.shift
            {
                self.write(&alt_scroll(scroll_lines));
            } else {
                let scroll = AlacScroll::Delta(scroll_lines);
                let mut term = self.term.lock();
                term.scroll_display(scroll);
                self.content_dirty = true;
                self.last_scroll_time.set(Instant::now());
            }
        }
    }

    fn determine_scroll_lines(
        &mut self,
        e: &gpui::ScrollWheelEvent,
        scroll_multiplier: f32,
    ) -> Option<i32> {
        let line_height = self.terminal_bounds.line_height;
        match e.touch_phase {
            /* Reset scroll state on started */
            gpui::TouchPhase::Started => {
                self.scroll_px = px(0.);
                None
            }
            /* Calculate the appropriate scroll lines */
            gpui::TouchPhase::Moved => {
                let old_offset = (self.scroll_px / line_height) as i32;

                self.scroll_px += e.delta.pixel_delta(line_height).y * scroll_multiplier;

                let new_offset = (self.scroll_px / line_height) as i32;

                // Whenever we hit the edges, reset our stored scroll to 0
                // so we can respond to changes in direction quickly
                self.scroll_px %= self.terminal_bounds.height();

                Some(new_offset - old_offset)
            }
            gpui::TouchPhase::Ended => None,
        }
    }

    // Selection
    pub fn selection_text(&self) -> Option<String> {
        let term = self.term.lock();
        term.selection_to_string()
    }

    pub fn selection_range(&self) -> Option<alacritty_terminal::selection::SelectionRange> {
        let term = self.term.lock();
        term.selection.as_ref().and_then(|s| s.to_range(&term))
    }

    pub fn is_cell_selected(&self, point: &AlacPoint) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        let point_line = point.line.0;
        let point_col = point.column.0;

        // 确定选择范围的正常顺序
        let (start, end) = if range.start.line <= range.end.line {
            (range.start, range.end)
        } else {
            (range.end, range.start)
        };

        // 检查行范围
        if point_line < start.line.0 || point_line > end.line.0 {
            return false;
        }

        // 同一行的特殊情况
        if start.line == end.line {
            return point_col >= start.column.0 && point_col <= end.column.0;
        }

        // 多行选择的边界检查
        match point_line.cmp(&start.line.0) {
            std::cmp::Ordering::Equal => point_col >= start.column.0,
            std::cmp::Ordering::Greater if point_line == end.line.0 => point_col <= end.column.0,
            _ => true,
        }
    }

    pub fn clear_selection(&mut self) {
        let mut term = self.term.lock();
        term.selection = None;
        self.selection_phase = SelectionPhase::Ended;
    }

    pub fn select_all(&mut self) {
        let mut term = self.term.lock();
        let start = AlacPoint::new(Line(0), Column(0));
        let selection = Selection::new(SelectionType::Lines, start, Side::Left);
        term.selection = Some(selection);
    }

    // Scrolling
    pub fn scroll_line_up(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Delta(1));
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_line_down(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Delta(-1));
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_page_up(&mut self) {
        let mut term = self.term.lock();
        let lines = term.screen_lines() as i32;
        term.scroll_display(AlacScroll::Delta(lines));
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_page_down(&mut self) {
        let mut term = self.term.lock();
        let lines = term.screen_lines() as i32;
        term.scroll_display(AlacScroll::Delta(-lines));
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_to_top(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Top);
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_to_bottom(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Bottom);
        self.last_scroll_time.set(Instant::now());
    }

    /// Set the display offset directly (for scrollbar dragging)
    pub fn set_display_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        let current_offset = self.last_content.display_offset;
        let delta = offset as i32 - current_offset as i32;
        if delta != 0 {
            let mut term = self.term.lock();
            term.scroll_display(AlacScroll::Delta(delta));
            self.content_dirty = true;
            self.last_scroll_time.set(Instant::now());
            cx.emit(Event::Wakeup);
        }
    }

    // Copy/Paste
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let term = self.term.lock();
        if let Some(text) = term.selection_to_string() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        // Check if clipboard contains an image
        // If so, forward Ctrl+V to let the terminal application handle it
        if clipboard.entries.iter().any(|entry| match entry {
            ClipboardEntry::Image(image) => !image.bytes.is_empty(),
            _ => false,
        }) {
            self.write(&[CTRL_V]);
            return;
        }

        // Otherwise, paste text content with bracketed paste mode support
        if let Some(text) = clipboard.text() {
            let term = self.term.lock();
            let bracketed = term.mode().contains(TermMode::BRACKETED_PASTE);
            drop(term);

            if bracketed {
                // Use bracketed paste mode to preserve whitespace
                self.write(b"\x1b[200~");
                self.write(text.as_bytes());
                self.write(b"\x1b[201~");
            } else {
                // Normal mode
                self.write(text.as_bytes());
            }
        }
    }

    // Clear
    pub fn clear(&mut self) {
        use alacritty_terminal::vte::ansi::ClearMode;
        let mut term = self.term.lock();
        term.clear_screen(ClearMode::All);
    }
}

impl EventEmitter<Event> for Terminal {}
