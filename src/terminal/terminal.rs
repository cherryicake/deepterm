//! Terminal module - simplified but functional

use std::{borrow::Cow, cell::Cell, path::PathBuf, rc::Rc, sync::Arc, time::Instant};

use alacritty_terminal::{
    Term,
    event::{Event as AlacTermEvent, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll as AlacScroll},
    index::{Column, Line, Point as AlacPoint, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, TermMode},
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

use crate::ssh::{SshConnectionConfig, SshPtyTerminal};
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
    pub cursor: AlacPoint,
    pub cursor_char: char,
    /// 光标形状 - 用于光标委托机制
    /// 当光标形状为Hidden时，终端不渲染光标，由应用程序（如Claude Code）自己处理
    pub cursor_shape: AlacCursorShape,
    /// 光标所在单元格的背景色 - 用于块光标模式下的字符颜色
    pub cursor_cell_bg: alacritty_terminal::vte::ansi::Color,
    pub display_offset: usize,
    pub mode: alacritty_terminal::term::TermMode,
    pub cols: usize,
    pub rows: usize,
}

impl Default for TerminalContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            cursor: AlacPoint::new(Line(0), Column(0)),
            cursor_char: ' ',
            cursor_shape: AlacCursorShape::Block,
            cursor_cell_bg: alacritty_terminal::vte::ansi::Color::Named(
                alacritty_terminal::vte::ansi::NamedColor::Background,
            ),
            display_offset: 0,
            mode: alacritty_terminal::term::TermMode::default(),
            cols: 80,
            rows: 24,
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
    /// 本地终端的事件循环发送器
    sender: Option<EventLoopSender>,
    last_content: TerminalContent,
    content_generation: u64,
    content_dirty: bool,
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
}

impl Terminal {
    pub fn new(cx: &mut App, working_directory: Option<PathBuf>) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());

        // Choose a sensible default shell per platform. On Windows prefer `COMSPEC` or `cmd.exe`;
        // on Unix-like systems prefer the `SHELL` env var or `/bin/sh`.
        let default_shell = if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        };

        // 设置终端环境变量 - 使用共享函数
        let env = crate::terminal::mappings::default_terminal_env();

        let pty_options = tty::Options {
            shell: Some(tty::Shell::new(default_shell, vec![])),
            working_directory,
            env,
            ..Default::default()
        };

        let default_bounds = TerminalBounds::default();
        let pty = match tty::new(&pty_options, default_bounds.into(), 0) {
            Ok(pty) => pty,
            Err(e) => {
                eprintln!("Failed to create PTY: {}", e);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };

        let mut config = Config::default();
        config.scrolling_history = 10000.min(MAX_SCROLLBACK_LINES);
        // 设置默认光标样式为Block（块光标）
        // 这是alacritty终端的默认光标形状，大多数终端模拟器都使用这种样式
        config.default_cursor_style = AlacCursorStyle {
            shape: AlacCursorShape::Block,
            blinking: false,
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &default_bounds,
            listener.clone(),
        )));

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
                sender: Some(notifier),
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
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
            }
        })
    }

    fn create_error_terminal() -> Self {
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TerminalBounds::default(),
            ZedListener(unbounded().0),
        )));

        Self {
            term,
            sender: None,
            last_content: TerminalContent::default(),
            content_generation: 0,
            content_dirty: false,
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

        let mut config = Config::default();
        config.scrolling_history = 10000.min(MAX_SCROLLBACK_LINES);
        config.default_cursor_style = AlacCursorStyle {
            shape: AlacCursorShape::Block,
            blinking: false,
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config,
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
                sender: None, // 远程终端不使用本地 sender
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
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
            }
        })
    }

    /// 创建新的 SSH 远程终端
    ///
    /// 使用 PTY + 系统 ssh 命令方式创建连接。
    /// 这是性能和兼容性最佳的方案：
    /// - 数据路径短：PTY → EventLoop → Terminal
    /// - 自动支持所有 SSH 功能（密钥、代理、配置等）
    /// - 使用 alacritty 优化的事件循环处理刷新
    pub fn new_ssh(cx: &mut App, ssh_config: &SshConnectionConfig) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());

        let default_bounds = TerminalBounds::default();

        let mut config = Config::default();
        config.scrolling_history = 10000.min(MAX_SCROLLBACK_LINES);
        config.default_cursor_style = AlacCursorStyle {
            shape: AlacCursorShape::Block,
            blinking: false,
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config.clone(),
            &default_bounds,
            listener.clone(),
        )));

        // 使用 PTY + 系统 ssh 命令方式创建 SSH 终端
        // 这是性能和兼容性最佳的方案
        Self::create_ssh_pty_terminal(cx, ssh_config, term, listener, events_rx)
    }

    /// 创建 SSH PTY 终端（内部方法）
    fn create_ssh_pty_terminal(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        term: Arc<FairMutex<Term<ZedListener>>>,
        listener: ZedListener,
        events_rx: UnboundedReceiver<AlacTermEvent>,
    ) -> Entity<Self> {
        let default_bounds = TerminalBounds::default();

        // 创建 SshPtyTerminal - 使用传入的 listener，确保 EventLoop 的 Wakeup 事件能被正确处理
        let ssh_pty = match SshPtyTerminal::new(ssh_config, default_bounds, term.clone(), listener)
        {
            Ok(pty) => Arc::new(pty),
            Err(e) => {
                eprintln!("Failed to create SSH PTY: {}", e);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };

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

            // 克隆 Arc 指针用于闭包
            let ssh_pty_for_input = ssh_pty.clone();
            let ssh_pty_for_resize = ssh_pty.clone();

            Self {
                term: term_clone,
                sender: None,
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
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
                remote_input_callback: Some(Box::new(move |data: &[u8]| {
                    ssh_pty_for_input.write(data);
                })),
                remote_resize_callback: Some(Box::new(move |cols: u16, rows: u16| {
                    let cell_width = px(9.0);
                    let line_height = px(16.0);
                    let new_bounds = TerminalBounds {
                        cell_width,
                        line_height,
                        bounds: Bounds {
                            origin: Point::default(),
                            size: Size {
                                width: px(cols as f32 * cell_width.as_f32()),
                                height: px(rows as f32 * line_height.as_f32()),
                            },
                        },
                    };
                    ssh_pty_for_resize.resize(new_bounds);
                })),
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

    /// 处理从 SSH 接收的数据
    pub fn process_ssh_data(&mut self, data: &[u8], cx: &mut Context<Self>) {
        log::debug!("Terminal 处理 SSH 数据: {} 字节", data.len());

        // 首先提取和处理 OSC 52 序列
        let filtered_data = self.extract_and_handle_osc52(data, cx);

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

    /// 提取 OSC 52 序列并处理，返回过滤后的数据
    /// 使用 Cow 实现零拷贝快速路径 - 当没有 OSC 序列时直接返回借用数据
    fn extract_and_handle_osc52<'a>(
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
                    // 尝试解析为 OSC 52
                    if Terminal::try_handle_osc52(cx, osc_content) {
                        // 成功处理 OSC 52，跳过这个序列
                        i += osc_end;
                        continue;
                    }
                    // 不是 OSC 52，保留序列
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
            AlacTermEvent::Title(_) => {
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
            _ => {}
        }
    }

    fn update_last_content(&mut self) {
        let term = self.term.lock();
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

        self.last_content.cursor = content.cursor.point;
        self.last_content.cursor_char = term.grid()[content.cursor.point].c;
        // 获取光标形状 - 用于光标委托机制
        // 当光标形状为Hidden时，终端不渲染光标，由应用程序自己处理
        self.last_content.cursor_shape = content.cursor.shape;
        // 获取光标所在单元格的背景色 - 用于块光标模式下的字符颜色
        self.last_content.cursor_cell_bg = term.grid()[content.cursor.point].bg;
        self.last_content.display_offset = content.display_offset;
        self.last_content.mode = content.mode;
        self.last_content.cols = self.terminal_bounds.num_columns();
        self.last_content.rows = self.terminal_bounds.num_lines();
    }

    pub fn sync(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.content_dirty {
            self.update_last_content();
            self.content_dirty = false;
        }
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
        self.terminal_bounds = bounds;

        let cols = bounds.num_columns() as u16;
        let rows = bounds.num_lines() as u16;

        // 本地终端：通过 sender 发送 resize 消息
        if let Some(ref sender) = self.sender {
            let window_size: WindowSize = bounds.into();
            let _ = sender.send(Msg::Resize(window_size));
        }

        // 远程终端：通过回调通知 SSH 通道调整窗口大小
        if let Some(ref callback) = self.remote_resize_callback {
            callback(cols, rows);
        }

        let mut term = self.term.lock();
        term.resize(alacritty_terminal::term::test::TermSize::new(
            bounds.num_columns(),
            bounds.num_lines(),
        ));

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
            match e.button {
                gpui::MouseButton::Left => {
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
                _ => {}
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

        // Otherwise, paste text content
        if let Some(text) = clipboard.text() {
            self.write(text.as_bytes());
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
