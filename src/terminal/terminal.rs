//! Terminal module - simplified but functional

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    borrow::Cow,
    cell::Cell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    sync::OnceLock,
    time::Instant,
};

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
    vte::ansi::{
        CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle, Handler, Processor,
        StdSyncHandler,
    },
};
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, Context, Entity, EventEmitter, Pixels, Point,
    SharedString, Size, Task, Window, px,
};

use crate::ssh::{RusshTerminal, SshConnectionConfig, SshPtyTerminal, platform::detect_local_os};
use crate::ssh::{
    keychain::{PreferredSshTransport, preferred_ssh_transport},
    platform::RemoteOS,
};
use crate::terminal::link_detector::UrlRange;
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

#[derive(Clone, Debug)]
pub enum DriverEvent {
    RemoteData(bytes::Bytes),
    /// SSH 连接异常断开（网络问题或连接中断）
    AbnormalDisconnect {
        reason: String,
    },
}

/// Terminal events
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    TitleChanged,
    CloseTerminal,
    /// 子进程退出（带退出状态）
    ProcessExited(i32),
    /// SSH 连接异常断开（网络问题）
    SshAbnormalDisconnect,
    Bell,
    Wakeup,
    SelectionsChanged,
    WorkingDirectoryChanged(String),
    /// 终端应用通过 DEC 私有模式 ?12 或 CSI `Ps SP q` 改变了光标闪烁策略。
    /// `bool` 为 true 表示应用希望光标闪烁。
    BlinkChanged(bool),
}

#[derive(Clone)]
pub struct ZedListener(pub UnboundedSender<AlacTermEvent>);

#[derive(Clone)]
pub struct DriverListener(pub UnboundedSender<DriverEvent>);

impl EventListener for ZedListener {
    fn send_event(&self, event: AlacTermEvent) {
        self.0.unbounded_send(event).ok();
    }
}

fn env_value(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).cloned().or_else(|| std::env::var(key).ok())
}

fn local_shell_name(program: &str) -> Option<&str> {
    Path::new(program).file_name()?.to_str()
}

fn configure_local_shell_integration(
    program: String,
    mut args: Vec<String>,
    env: &mut HashMap<String, String>,
) -> tty::Shell {
    let Some(shell_name) = local_shell_name(&program) else {
        return tty::Shell::new(program, args);
    };

    let Some(root) = write_local_shell_integration_files() else {
        return tty::Shell::new(program, args);
    };

    env.insert(
        "DEEPTERM_RESOURCES_DIR".to_string(),
        root.to_string_lossy().to_string(),
    );

    match shell_name {
        "zsh" if args.is_empty() => {
            // 对齐 Ghostty 的 zsh 自动注入模型：把 ZDOTDIR 指到 DeepTerm
            // 资源目录，并把用户原来的 ZDOTDIR 保存在环境变量里。zsh 启动
            // 时会自然读取该目录下的启动文件；这些 wrapper 再 source 用户
            // 原本配置并加载 OSC 7 integration，全程不会向 PTY 写入脚本文本。
            if let Some(old_zdotdir) = env_value(env, "ZDOTDIR") {
                env.insert("DEEPTERM_ZSH_ZDOTDIR".to_string(), old_zdotdir);
            }
            env.insert(
                "ZDOTDIR".to_string(),
                root.join("shell-integration/zsh")
                    .to_string_lossy()
                    .to_string(),
            );
        }
        "bash" if args.is_empty() => {
            // 对齐 Ghostty 的 bash 自动注入模型：bash 先以 POSIX 模式启动，
            // POSIX shell 会读取 ENV 指向的 integration 脚本。脚本随后关闭
            // POSIX 模式并手动恢复 bash 原本的 profile/bashrc 加载流程。
            // macOS 系统自带 /bin/bash 与 Ghostty 一样跳过自动注入，因为它的
            // Apple patch 不可靠地支持这条 ENV/POSIX 启动路径。
            if cfg!(target_os = "macos") && program == "/bin/bash" {
                return tty::Shell::new(program, args);
            }
            if let Some(old_env) = env_value(env, "ENV") {
                env.insert("DEEPTERM_BASH_ENV".to_string(), old_env);
            }
            env.insert(
                "ENV".to_string(),
                root.join("shell-integration/bash/deepterm.bash")
                    .to_string_lossy()
                    .to_string(),
            );
            env.insert("DEEPTERM_BASH_INJECT".to_string(), "1".to_string());
            if env_value(env, "HISTFILE").is_none()
                && let Some(home) = env_value(env, "HOME")
            {
                env.insert("HISTFILE".to_string(), format!("{home}/.bash_history"));
                env.insert(
                    "DEEPTERM_BASH_UNEXPORT_HISTFILE".to_string(),
                    "1".to_string(),
                );
            }
            args.push("--posix".to_string());
        }
        _ => {}
    }

    tty::Shell::new(program, args)
}

fn write_local_shell_integration_files() -> Option<PathBuf> {
    let root = std::env::temp_dir().join("deepterm-shell-integration");
    let bash_dir = root.join("shell-integration/bash");
    let zsh_dir = root.join("shell-integration/zsh");
    fs::create_dir_all(&bash_dir).ok()?;
    fs::create_dir_all(&zsh_dir).ok()?;

    // 关键点：这些文件直接来自 Ghostty 的 shell integration 资源结构，
    // 只做 DeepTerm 命名适配。运行时复制资源文件，而不是手写/拼接 shell
    // wrapper，避免 zsh/bash 启动语义和 Ghostty 分叉。
    fs::write(
        bash_dir.join("deepterm.bash"),
        include_str!("../../assets/shell-integration/bash/deepterm.bash"),
    )
    .ok()?;
    fs::write(
        bash_dir.join("bash-preexec.sh"),
        include_str!("../../assets/shell-integration/bash/bash-preexec.sh"),
    )
    .ok()?;
    fs::write(
        zsh_dir.join(".zshenv"),
        include_str!("../../assets/shell-integration/zsh/.zshenv"),
    )
    .ok()?;
    fs::write(
        zsh_dir.join("deepterm-integration"),
        include_str!("../../assets/shell-integration/zsh/deepterm-integration"),
    )
    .ok()?;

    Some(root)
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

/// 构造覆盖整个 grid 的全选选区：从 `topmost_line()`（scrollback 顶部）
/// 的第 0 列到 `bottommost_line()`（屏幕底行）的最后一列。
/// 用 `SelectionType::Simple`（而非 `Lines`）：Simple 在两个端点都取
/// `Side::Left`/`Side::Right` 时不会裁剪行尾空格，能覆盖完整矩形区域。
fn build_select_all_selection<T: EventListener>(term: &Term<T>) -> Selection {
    let start = AlacPoint::new(term.topmost_line(), Column(0));
    let end = AlacPoint::new(term.bottommost_line(), term.last_column());
    let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
    selection.update(end, Side::Right);
    selection
}

/// Terminal content
#[derive(Clone, Debug)]
pub struct TerminalContent {
    pub cells: Vec<IndexedCell>,
    /// Contiguous row ranges into `cells`, in visible row order.
    /// Each tuple is `(start, end)` where `cells[start..end]` belongs to one visible row.
    pub row_ranges: Vec<(usize, usize)>,
    pub cursor: AlacPoint,
    pub cursor_cell: PackedCell,
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
    /// Per-row URL ranges detected in the current content.
    pub url_ranges: Vec<Vec<UrlRange>>,
    /// Current selection range, resolved while the term lock is already held
    /// in `update_last_content`. Read by the render hot path so it does not
    /// need to acquire the `FairMutex<Term>` a second time per frame.
    pub selection_range: Option<alacritty_terminal::selection::SelectionRange>,
}

impl Default for TerminalContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            row_ranges: Vec::new(),
            cursor: AlacPoint::new(Line(0), Column(0)),
            cursor_cell: PackedCell::from(&alacritty_terminal::term::cell::Cell::default()),
            cursor_shape: AlacCursorShape::Block,
            display_offset: 0,
            mode: alacritty_terminal::term::TermMode::default(),
            cols: 80,
            rows: 24,
            total_lines: 24,
            viewport_lines: 24,
            url_ranges: Vec::new(),
            selection_range: None,
        }
    }
}

/// Compact snapshot of the fields from alacritty's `Cell` that the render
/// pipeline actually reads.
///
/// `IndexedCell.cell` stores this instead of a full `alacritty_terminal::Cell`
/// so that the per-frame clone of every visible cell (see `update_last_content`)
/// copies only `c`/`fg`/`bg`/`flags` and skips the `Option<Arc<CellExtra>>` field
/// (`zerowidth` / `underline_color` / `hyperlink`), which is unused on the render
/// path. All four fields are cheap `Copy` types, making the clone effectively
/// a small stack copy with no heap traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedCell {
    pub c: char,
    pub fg: alacritty_terminal::vte::ansi::Color,
    pub bg: alacritty_terminal::vte::ansi::Color,
    pub flags: alacritty_terminal::term::cell::Flags,
}

impl From<&alacritty_terminal::term::cell::Cell> for PackedCell {
    #[inline]
    fn from(cell: &alacritty_terminal::term::cell::Cell) -> Self {
        Self {
            c: cell.c,
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexedCell {
    pub point: AlacPoint,
    pub cell: PackedCell,
}

/// 远程终端输入回调类型
pub type RemoteInputCallback = Box<dyn Fn(&[u8]) + Send>;

/// 远程终端调整大小回调类型
pub type RemoteResizeCallback = Box<dyn Fn(u16, u16) + Send>;

pub struct Terminal {
    term: Arc<FairMutex<Term<ZedListener>>>,
    /// 本地 PTY 启动出来的首个子进程 pid，通常就是 shell 进程。
    pub child_process_id: Option<u32>,
    /// shell 所在的进程组 id，用来判断当前前台程序是否仍然是 shell。
    shell_process_group_id: Option<i32>,
    /// 本地 PTY master fd，用来读取当前 TTY 的前台进程组。
    pty_master_fd: Option<i32>,
    /// 启动时使用的 shell 程序路径（如 "/bin/zsh"），用于生成 tab 标签。
    shell_program: Option<String>,
    /// 远程 shell 提示符是否已准备好。
    remote_shell_ready: bool,
    /// 终端通过 OSC 7 / CurrentDir 等序列主动上报的工作目录。
    ///
    /// 这里同时服务本地 PTY 与 SSH 终端：
    /// - 本地 PTY 走 vendored vte/alacritty 的 OSC 7 事件，由 `PwdChanged` 写入。
    /// - SSH 数据流先在 DeepTerm 侧截获 OSC，再复用同一套解析和保存逻辑。
    ///   统一保存“终端自己声明的 cwd”，可以避免再用 `lsof`/进程 cwd 猜测 shell
    ///   状态；后者无法稳定追踪 shell 内建 `cd` 后的实时目录。
    reported_working_directory: Option<String>,
    /// 检测到的远程操作系统
    pub remote_os: Option<RemoteOS>,
    /// 远程前台进程名（由远程 shell 的 preexec hook 通过 OSC 上报）
    remote_process_name: Option<String>,
    window_title: Option<String>,
    /// 本地终端的事件循环发送器
    sender: Option<EventLoopSender>,
    last_content: TerminalContent,
    pub content_generation: u64,
    content_dirty: bool,
    /// 每次 update_last_content 后，标记哪些 viewport 行（0 = 顶行）发生了变化
    dirty_rows: Vec<bool>,
    /// 每次 update_last_content 后，记录 viewport 行的脏区列范围 `(left, right)`。
    /// 若该行无变更则为 `None`；全量刷新时所有行为 `Some((0, cols-1))`。
    dirty_row_bounds: Vec<Option<(usize, usize)>>,
    /// 事件循环任务（保持任务存活，防止被drop）
    #[allow(dead_code)]
    event_loop_task: Option<Task<()>>,
    /// 远程驱动任务（保持任务存活，防止被drop）
    #[allow(dead_code)]
    driver_task: Option<Task<()>>,
    /// VTE 解析器状态（跨数据块保持，避免转义序列在 chunk 边界处被截断导致乱码）
    vte_processor: Processor<StdSyncHandler>,
    /// 输入区起点锚点（OSC 133;B 标记处 / 无集成 shell 时的回显锚点）。
    ///
    /// shell 回显会把光标留在当前行内容的末尾，因此“光标 == 锚点”即输入行为空。
    /// 判定逻辑见 [`Terminal::input_line_blank`]。
    /// - 远程集成 shell：提示符绘制完成时 OSC 133;B 精确设置锚点；
    /// - 本地 / 无集成 shell：alacritty EventLoop 直接消费 PTY 字节，OSC 133 不可见，
    ///   退化为 [`Terminal::write`] 时捕获的“首个输入前的光标位置”（即提示符末尾）。
    ///
    /// 锚点失效时机：resize / clear（reflow 改变行列）、写入回车（提示符即将重绘）、
    /// shell 重回前台（旧提示符位置作废，如退出编辑器后）。
    input_area_start: Option<AlacPoint>,
    /// 上一次观察到的前台状态，用于检测“程序退出 → shell 回到前台”的转换；
    /// 转换发生时旧锚点必然过期，交由下一次输入重新捕获。
    shell_fg_observed: Option<bool>,
    /// 跨 chunk 未完成的 OSC 序列暂存。
    ///
    /// 若 OSC 终止符落在下一个 PTY 读取块中，直接把残尾透传给 vte 会导致
    /// 应用层（133;A/B/C、OSC 7 等）永远看不到这个序列；这里把残尾缓存
    /// 起来与下一个 chunk 拼接后再扫描。超过上限则放弃缓存直接透传，防止
    /// 异常输出（无终止符的 ESC ]）无限占用内存。
    pending_osc: Vec<u8>,
    pub terminal_bounds: TerminalBounds,
    selection_phase: SelectionPhase,
    /// Whether a mouse press was reported to the terminal application (mouse
    /// mode active at `mouse_down` time). The release report must be paired
    /// with the press report even if the shift modifier changes mid-drag,
    /// otherwise applications receive an unpaired release (or never see the
    /// button go up and treat all later motion as a drag).
    mouse_report_active: bool,
    /// Whether the link-hover modifier (Cmd on macOS / Ctrl elsewhere) is
    /// currently held. URL detection for the viewport costs one regex scan
    /// plus two allocations per dirty row on every frame, but its only
    /// consumer is hover hit-testing — so it is maintained lazily only while
    /// this flag is set. See `set_link_hover_active`.
    link_hover_active: bool,
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
    /// 终端视图是否获得焦点（用于剪辑板操作守卫）
    focused: bool,
}

impl Terminal {
    fn default_terminal_config() -> Config {
        Config {
            scrolling_history: 10000.min(MAX_SCROLLBACK_LINES),
            default_cursor_style: AlacCursorStyle {
                shape: AlacCursorShape::Block,
                blinking: false,
            },
            osc52: alacritty_terminal::term::Osc52::CopyPaste,
            ..Config::default()
        }
    }

    fn build_local_terminal(
        cx: &mut App,
        pty_options: tty::Options,
        pty_error_message: String,
        remote_os: Option<RemoteOS>,
        shell_program: Option<String>,
    ) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());
        let default_bounds = TerminalBounds::default();
        let pty = match tty::new(&pty_options, default_bounds.into(), 0) {
            Ok(pty) => pty,
            Err(err) => {
                log::error!("{}: {}", pty_error_message, err);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };
        let term = Arc::new(FairMutex::new(Term::new(
            Self::default_terminal_config(),
            &default_bounds,
            listener.clone(),
        )));
        #[cfg(unix)]
        let (child_process_id, shell_process_group_id, pty_master_fd) = {
            let pid = pty.child().id();
            let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
            (
                Some(pid),
                if pgid >= 0 { Some(pgid) } else { None },
                Some(pty.file().as_raw_fd()),
            )
        };
        #[cfg(not(unix))]
        let (child_process_id, shell_process_group_id, pty_master_fd) =
            (pty.child_watcher().pid().map(|p| p.get()), None, None);
        let event_loop = match EventLoop::new(term.clone(), listener, pty, true, false) {
            Ok(event_loop) => event_loop,
            Err(e) => {
                log::error!("Failed to create terminal EventLoop: {e}");
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
                shell_program,
                remote_shell_ready: false,
                reported_working_directory: None,
                remote_os,
                remote_process_name: None,
                window_title: None,
                sender: Some(notifier),
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                driver_task: None,
                vte_processor: Processor::<StdSyncHandler>::new(),
                input_area_start: None,
                shell_fg_observed: None,
                pending_osc: Vec::new(),
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                mouse_report_active: false,
                link_hover_active: false,
                last_mouse_position: None,
                scroll_px: px(0.0),
                scrollbar_drag_offset: Rc::new(Cell::new(None)),
                last_scroll_time: Cell::new(Instant::now()),
                terminal_type: TerminalType::Local,
                remote_input_callback: None,
                remote_resize_callback: None,
                russh_handle: None,
                focused: true,
            }
        })
    }

    pub fn new(cx: &mut App, working_directory: Option<PathBuf>) -> Entity<Self> {
        let default_shell = if cfg!(windows) {
            if std::process::Command::new("powershell.exe")
                .arg("-NoProfile")
                .arg("-Command")
                .arg("exit 0")
                .output()
                .is_ok()
            {
                "powershell.exe".to_string()
            } else if std::process::Command::new("pwsh.exe")
                .arg("-NoProfile")
                .arg("-Command")
                .arg("exit 0")
                .output()
                .is_ok()
            {
                "pwsh.exe".to_string()
            } else {
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
            }
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        };

        let remote_os = Some(detect_local_os());

        let shell_program = default_shell.clone();
        let mut env = crate::terminal::mappings::default_terminal_env();
        let shell = configure_local_shell_integration(default_shell, Vec::new(), &mut env);

        Self::build_local_terminal(
            cx,
            tty::Options {
                shell: Some(shell),
                working_directory,
                env,
                ..Default::default()
            },
            "Failed to create PTY".to_string(),
            remote_os,
            Some(shell_program),
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
        let remote_os = {
            let cmd_lower = command.to_lowercase();
            let is_wsl = cmd_lower.ends_with("wsl") || cmd_lower.ends_with("wsl.exe");
            if is_wsl {
                Some(crate::ssh::platform::detect_wsl_os(command, args))
            } else {
                Some(detect_local_os())
            }
        };

        Self::build_local_terminal(
            cx,
            tty::Options {
                shell: Some(tty::Shell::new(command.to_string(), args.to_vec())),
                working_directory,
                env: crate::terminal::mappings::default_terminal_env(),
                ..Default::default()
            },
            format!("Failed to create PTY for command '{command}'"),
            remote_os,
            Some(command.to_string()),
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
            shell_program: None,
            remote_shell_ready: false,
            reported_working_directory: None,
            remote_os: None,
            remote_process_name: None,
            window_title: None,
            sender: None,
            last_content: TerminalContent::default(),
            content_generation: 0,
            content_dirty: false,
            dirty_rows: Vec::new(),
            dirty_row_bounds: Vec::new(),
            event_loop_task: None,
            driver_task: None,
            vte_processor: Processor::<StdSyncHandler>::new(),
            input_area_start: None,
            shell_fg_observed: None,
            pending_osc: Vec::new(),
            terminal_bounds: TerminalBounds::default(),
            selection_phase: SelectionPhase::Ended,
            mouse_report_active: false,
            link_hover_active: false,
            last_mouse_position: None,
            scroll_px: px(0.0),
            scrollbar_drag_offset: Rc::new(Cell::new(None)),
            last_scroll_time: Cell::new(Instant::now()),
            terminal_type: TerminalType::Local,
            remote_input_callback: None,
            remote_resize_callback: None,
            russh_handle: None,
            focused: true,
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
                shell_program: None,
                remote_shell_ready: false,
                reported_working_directory: None,
                remote_os: None,
                remote_process_name: None,
                window_title: None,
                sender: None, // 远程终端不使用本地 sender
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                driver_task: None,
                vte_processor: Processor::<StdSyncHandler>::new(),
                input_area_start: None,
                shell_fg_observed: None,
                pending_osc: Vec::new(),
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                mouse_report_active: false,
                link_hover_active: false,
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
                focused: true,
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
        Self::new_ssh_with_remote_cwd(cx, ssh_config, None)
    }

    pub fn new_ssh_with_remote_cwd(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        remote_cwd: Option<String>,
    ) -> Entity<Self> {
        match preferred_ssh_transport(&ssh_config.auth_method) {
            Ok(PreferredSshTransport::Pty) => {
                log::info!("SSH: 该凭据类型使用系统 ssh PTY 模式");
                Self::new_ssh_pty(cx, ssh_config, remote_cwd)
            }
            Ok(PreferredSshTransport::Russh) | Err(_) => {
                Self::new_ssh_russh(cx, ssh_config, remote_cwd)
            }
        }
    }

    fn new_ssh_russh(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        remote_cwd: Option<String>,
    ) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let (driver_tx, driver_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());
        let driver_listener = DriverListener(driver_tx);

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
            listener.clone(),
            driver_listener,
            remote_cwd,
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
            driver_rx,
            move |data| russh_for_input.write(data),
            move |cols, rows| russh_for_resize.resize(cols, rows),
            Some(russh_for_status),
        )
    }

    fn new_ssh_pty(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        remote_cwd: Option<String>,
    ) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let (_driver_tx, driver_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());
        let default_bounds = TerminalBounds::default();

        let term = Arc::new(FairMutex::new(Term::new(
            Self::default_terminal_config(),
            &default_bounds,
            listener.clone(),
        )));

        let pty_terminal = match SshPtyTerminal::new(
            ssh_config,
            default_bounds,
            term.clone(),
            listener,
            remote_cwd,
        ) {
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
            driver_rx,
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
        driver_rx: UnboundedReceiver<DriverEvent>,
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
                }
            });

            let driver_task = cx.spawn(async move |this, cx| {
                let mut driver_rx = driver_rx;
                while let Some(event) = driver_rx.next().await {
                    let _ = this.update(cx, |terminal: &mut Self, cx| match event {
                        DriverEvent::RemoteData(data) => terminal.handle_remote_data(&data, cx),
                        DriverEvent::AbnormalDisconnect { reason } => {
                            log::warn!("SSH 连接异常断开: {}", reason);
                            cx.emit(Event::SshAbnormalDisconnect);
                        }
                    });
                }
            });

            Self {
                term: term_clone,
                child_process_id: None,
                shell_process_group_id: None,
                pty_master_fd: None,
                shell_program: None,
                remote_shell_ready: false,
                reported_working_directory: None,
                remote_os: None,
                remote_process_name: None,
                window_title: None,
                sender: None,
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                driver_task: Some(driver_task),
                vte_processor: Processor::<StdSyncHandler>::new(),
                input_area_start: None,
                shell_fg_observed: None,
                pending_osc: Vec::new(),
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                mouse_report_active: false,
                link_hover_active: false,
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
                focused: true,
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

    /// 判断当前交互上下文是否仍然是 shell 提示符。
    /// 本地终端通过前台进程组判断，远程终端通过远程 shell 发回的提示符标记判断。
    pub fn is_shell_in_foreground(&self) -> bool {
        if self.is_remote() {
            return self.remote_shell_ready;
        }

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

    pub fn shell_name(&self) -> Option<&str> {
        self.shell_program
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .and_then(|n| n.to_str())
    }

    /// 获取远程前台进程名（由远程 shell 的 preexec hook 通过 OSC 1337;Proc= 上报）。
    pub fn remote_process_name(&self) -> Option<&str> {
        self.remote_process_name.as_deref()
    }

    /// 获取当前 TTY 前台进程的名称（本地终端）。
    /// 如果前台仍然是 shell 或无法获取，返回 None。
    pub fn foreground_process_name(&self) -> Option<String> {
        if self.is_remote() {
            return None;
        }

        #[cfg(unix)]
        {
            let fd = self.pty_master_fd?;
            let pgid = unsafe { libc::tcgetpgrp(fd) };
            if pgid < 0 {
                return None;
            }
            // 前台进程组就是 shell 自己，说明没有前台应用
            if self.shell_process_group_id == Some(pgid) {
                return None;
            }
            // 获取前台进程组的 leader 进程名
            Self::process_name_by_pid(pgid as u32)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    #[cfg(unix)]
    fn process_name_by_pid(pid: u32) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(format!("/proc/{}/comm", pid))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("/bin/ps")
                .args(["-p", &pid.to_string(), "-o", "comm="])
                .output()
                .ok()
                .and_then(|out| {
                    if out.status.success() {
                        String::from_utf8(out.stdout)
                            .ok()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .map(|s| {
                                std::path::Path::new(&s)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or(s)
                            })
                    } else {
                        None
                    }
                })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    pub fn reported_working_directory(&self) -> Option<&str> {
        self.reported_working_directory.as_deref()
    }

    /// 兼容旧调用点：历史上这个值只来自 SSH OSC，所以 API 名称包含 remote。
    /// 现在本地 PTY 也会通过 OSC 7 上报 cwd，新代码应优先使用
    /// `reported_working_directory` 表达更准确的语义。
    pub fn remote_working_directory(&self) -> Option<&str> {
        self.reported_working_directory()
    }

    /// 检查 SSH 连接是否已就绪（russh 已连接）
    pub fn is_ssh_connected(&self) -> bool {
        self.russh_handle.as_ref().is_some_and(|h| h.is_ready())
    }

    /// 检查 SSH 连接是否已失败
    pub fn is_ssh_failed(&self) -> bool {
        self.russh_handle.as_ref().is_some_and(|h| h.is_failed())
    }

    /// 获取 SSH 连接错误信息（如果连接失败）
    pub fn ssh_error_message(&self) -> Option<String> {
        self.russh_handle.as_ref()?.error_message()
    }

    /// 更新终端焦点状态
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// 处理 PTY/SSH 原始输出。
    ///
    /// 所有原始 bytes 都先经过这里旁路识别 OSC 7，再继续喂给 alacritty 的 VTE
    /// parser。这样不需要改 vte/alacritty 上游源码，也不会改变终端渲染数据流。
    fn handle_pty_data(
        &mut self,
        data: &[u8],
        require_local_osc7_host: bool,
        cx: &mut Context<Self>,
    ) {
        // 分段喂入：处理某个 OSC 时，其前置文本已经过 vte 解析，
        // 这样 OSC 133;B 捕获的光标位置精确反映提示符末尾，
        // 不受同 chunk 后续回显的影响。需要由应用层消费的 OSC
        // 序列（如 133;B、cwd 更新）在此被拦截，OSC 52 等则透传给 vte。
        self.process_pty_data(data, require_local_osc7_host, cx);

        // 标记内容已更新
        self.content_dirty = true;
        self.content_generation += 1;
    }

    /// 处理从 SSH 接收的数据
    pub fn process_ssh_data(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.handle_pty_data(data, false, cx);
    }

    pub fn handle_remote_data(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.process_ssh_data(data, cx);
        cx.emit(Event::Wakeup);
    }

    /// 把一段字节喂给 vte 解析器（作用于 term）。空段直接跳过。
    fn advance_vte(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut term = self.term.lock();
        self.vte_processor.advance(&mut *term, bytes);
    }

    /// 逐段扫描数据块：普通文本段直接喂给 vte，完整 OSC 交由应用层
    /// 判定（消费则丢弃，否则透传给 vte）。跨 chunk 未完成的 OSC
    /// 存入 `pending_osc`，与下一个数据块拼接后重新扫描。
    ///
    /// 快速路径：不含 OSC 起始符时一次性喂入，避免分段锁开销。
    fn process_pty_data(
        &mut self,
        data: &[u8],
        require_local_osc7_host: bool,
        cx: &mut Context<Self>,
    ) {
        const OSC_START: &[u8] = b"\x1b]";
        // 未完成 OSC 的缓存上限；超过则放弃拼接直接透传，防止异常
        // 输出（无终止符的 ESC ]）无限占用内存。
        const MAX_PENDING_OSC: usize = 8 * 1024;

        // 有跨 chunk 残尾时先拼接（拷贝一次，仅在有残尾时发生）。
        let pending = std::mem::take(&mut self.pending_osc);
        let joined: Vec<u8>;
        let data: &[u8] = if pending.is_empty() {
            data
        } else {
            joined = [pending.as_slice(), data].concat();
            joined.as_slice()
        };

        if memchr::memmem::find(data, OSC_START).is_none() {
            self.advance_vte(data);
            return;
        }

        let mut pos = 0;
        while let Some(osc_offset) = memchr::memmem::find(&data[pos..], OSC_START) {
            let osc_pos = pos + osc_offset;

            // 先喂入 OSC 之前的普通文本，使光标反映这些内容
            self.advance_vte(&data[pos..osc_pos]);

            match Terminal::extract_osc_sequence(&data[osc_pos..]) {
                Some((osc_end, osc_content)) => {
                    if self.try_handle_osc_sequence(cx, osc_content, require_local_osc7_host) {
                        // 已由应用层处理，跳过这个序列
                        pos = osc_pos + osc_end;
                    } else {
                        // 不是应用层关心的 OSC，保留给 vte 自己处理
                        self.advance_vte(&data[osc_pos..osc_pos + osc_end]);
                        pos = osc_pos + osc_end;
                    }
                }
                None => {
                    // 序列不完整：若残尾过大则直接透传，否则暂存等下一个 chunk
                    if data.len() - osc_pos > MAX_PENDING_OSC {
                        self.advance_vte(&data[osc_pos..]);
                    } else {
                        self.pending_osc = data[osc_pos..].to_vec();
                    }
                    pos = data.len();
                }
            }
        }

        if pos < data.len() {
            self.advance_vte(&data[pos..]);
        }
    }

    fn try_handle_osc_sequence(
        &mut self,
        cx: &mut Context<Self>,
        osc_content: &[u8],
        require_local_osc7_host: bool,
    ) -> bool {
        if Self::matches_osc_prefix(osc_content, "133;A") {
            self.remote_shell_ready = true;
            return true;
        }

        if Self::matches_osc_prefix(osc_content, "133;B") {
            // 输入区起点标记：提示符文本已经过 process_pty_data 分段喂入，
            // 此时光标即输入区起点，作为空行判定的锚点。
            // （兼容 `133;B;aid=...` 等带后缀的变体。）
            let point = {
                let term = self.term.lock();
                term.grid().cursor.point
            };
            self.input_area_start = Some(point);
            return true;
        }

        if Self::matches_osc_prefix(osc_content, "133;C") {
            self.remote_shell_ready = false;
            return true;
        }

        if let Some(path) = Self::parse_working_directory_osc(osc_content, require_local_osc7_host)
        {
            if self.reported_working_directory.as_deref() != Some(path.as_str()) {
                log::info!(
                    "OSC reported cwd changed: {:?} -> {}",
                    self.reported_working_directory.as_deref(),
                    path
                );
                self.reported_working_directory = Some(path.clone());
                cx.emit(Event::WorkingDirectoryChanged(path));
            }
            return true;
        }

        if let Some(proc) = Self::parse_remote_process_osc(osc_content) {
            if self.remote_process_name.as_deref() != Some(proc.as_str()) {
                self.remote_process_name = Some(proc);
            }
            return true;
        }

        if let Some(os) = Self::parse_remote_os_osc(osc_content) {
            self.remote_os = Some(os);
            return true;
        }

        false
    }

    fn matches_osc_prefix(osc_content: &[u8], prefix: &str) -> bool {
        std::str::from_utf8(osc_content)
            .ok()
            .is_some_and(|s| s.trim().starts_with(prefix))
    }

    fn parse_working_directory_osc(
        osc_content: &[u8],
        require_local_osc7_host: bool,
    ) -> Option<String> {
        let content = std::str::from_utf8(osc_content).ok()?.trim();

        if let Some(uri) = content.strip_prefix("7;") {
            return Self::parse_osc7_working_directory_uri(uri.trim(), require_local_osc7_host);
        }

        if let Some(path) = content.strip_prefix("1337;CurrentDir=") {
            return Self::normalize_remote_working_directory(path.trim());
        }

        if let Some(path) = content.strip_prefix("633;P;Cwd=") {
            return Self::normalize_remote_working_directory(path.trim());
        }

        None
    }

    fn parse_remote_process_osc(osc_content: &[u8]) -> Option<String> {
        let content = std::str::from_utf8(osc_content).ok()?.trim();
        let proc = content.strip_prefix("1337;Proc=")?;
        let proc = proc.trim();
        if proc.is_empty() {
            return None;
        }
        Some(proc.to_string())
    }

    fn parse_remote_os_osc(osc_content: &[u8]) -> Option<RemoteOS> {
        let content = std::str::from_utf8(osc_content).ok()?.trim();
        let id = content.strip_prefix("1338;")?;
        let os = RemoteOS::from_id(id.trim());
        if os == RemoteOS::Unknown {
            return None;
        }
        Some(os)
    }

    fn parse_osc7_working_directory_uri(uri: &str, require_local_host: bool) -> Option<String> {
        // OSC 7 常见有两种形态：
        // - file://host/path：标准/通用格式，path 是 URL percent-encoding。
        // - kitty-shell-cwd://host/path：Kitty/Ghostty 使用的格式，语义同样是 cwd。
        //
        // 和 Ghostty 一样，解析 URL 时保持“本地/远程上下文”分层：
        // - 本地 PTY 通过 alacritty/vte 的 PwdChanged 进入，要求 host 是本机。
        // - SSH 数据流通过 try_handle_osc_sequence 进入，host 属于远端，不做本机校验。
        let (host, path) = if let Some(rest) = uri.strip_prefix("file://") {
            Self::split_osc7_host_path(rest)?
        } else if let Some(rest) = uri.strip_prefix("kitty-shell-cwd://") {
            Self::split_osc7_host_path(rest)?
        } else {
            ("", uri)
        };

        if require_local_host && !Self::is_local_osc7_host(host) {
            log::warn!("Ignoring OSC 7 cwd from non-local host: {host}");
            return None;
        }

        Self::normalize_remote_working_directory(path)
    }

    fn split_osc7_host_path(value: &str) -> Option<(&str, &str)> {
        let slash_index = value.find('/')?;
        let (host, path) = value.split_at(slash_index);
        Some((host, path))
    }

    fn is_local_osc7_host(host: &str) -> bool {
        let host = host.trim();
        if host.is_empty() {
            return false;
        }

        if matches!(host, "localhost" | "127.0.0.1" | "::1") {
            return true;
        }

        fn cached_hostname() -> &'static str {
            static HOSTNAME: OnceLock<String> = OnceLock::new();
            HOSTNAME.get_or_init(|| {
                std::process::Command::new("/bin/hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default()
            })
        }

        let hostname = cached_hostname();
        host == hostname
            || hostname
                .split_once('.')
                .map(|(short, _)| host == short)
                .unwrap_or(false)
            || host
                .split_once('.')
                .map(|(short, _)| short == hostname)
                .unwrap_or(false)
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

        let body = &data[2..];

        // 查找 BEL 结束符 (0x07)
        if let Some(pos) = memchr::memchr(0x07, body) {
            return Some((pos + 3, &body[..pos]));
        }

        // 查找 ESC \ 结束符 (0x1b 0x5c)
        if let Some(pos) = memchr::memmem::find(body, b"\x1b\\") {
            return Some((pos + 4, &body[..pos]));
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
            AlacTermEvent::Exit => {
                cx.emit(Event::CloseTerminal);
            }
            AlacTermEvent::ChildExit(status) => {
                let exit_code = status.code().unwrap_or(-1);
                if exit_code == 0 {
                    cx.emit(Event::CloseTerminal);
                } else {
                    cx.emit(Event::ProcessExited(exit_code));
                }
            }
            AlacTermEvent::ClipboardStore(_, data) => {
                if self.focused {
                    cx.write_to_clipboard(ClipboardItem::new_string(data));
                }
            }
            AlacTermEvent::ClipboardLoad(_clipboard_type, format) => {
                if !self.focused {
                    return;
                }
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
                let color = term.colors()[index].unwrap_or(alacritty_terminal::vte::ansi::Rgb {
                    r: 0,
                    g: 0,
                    b: 0,
                });
                drop(term);
                let response = format(color);
                self.write(response.as_bytes());
            }
            AlacTermEvent::CursorBlinkingChange => {
                // Application requested a cursor blink state change via DEC
                // private mode `?12h` / `?12l` or CSI `Ps SP q`. Forward the
                // new flag so the view layer can run (or stop) its blink
                // state machine.
                let blinking = self.term.lock().cursor_style().blinking;
                cx.emit(Event::BlinkChanged(blinking));
            }
        }
    }

    fn mark_viewport_fully_dirty(
        dirty_rows: &mut Vec<bool>,
        dirty_row_bounds: &mut Vec<Option<(usize, usize)>>,
        num_rows: usize,
        num_cols: usize,
    ) {
        dirty_rows.clear();
        dirty_rows.resize(num_rows, true);
        dirty_row_bounds.clear();
        if num_cols == 0 {
            dirty_row_bounds.resize(num_rows, None);
        } else {
            dirty_row_bounds.resize(num_rows, Some((0, num_cols.saturating_sub(1))));
        }
    }

    fn update_last_content(&mut self, force_full_damage: bool) {
        let mut term = self.term.lock();

        // Collect damage info while we hold the lock, before resetting.
        let num_rows = self.terminal_bounds.num_lines();
        let num_cols = self.terminal_bounds.num_columns();
        if force_full_damage {
            // 在重置前先消费掉待处理的 damage，这样即使我们将强制进行
            // 全量失效，内部的 damage 追踪器状态也能保持一致。
            let _ = term.damage();
            Self::mark_viewport_fully_dirty(
                &mut self.dirty_rows,
                &mut self.dirty_row_bounds,
                num_rows,
                num_cols,
            );
        } else {
            let damage = term.damage();
            self.dirty_rows.clear();
            self.dirty_row_bounds.clear();
            match damage {
                TermDamage::Full => {
                    Self::mark_viewport_fully_dirty(
                        &mut self.dirty_rows,
                        &mut self.dirty_row_bounds,
                        num_rows,
                        num_cols,
                    );
                }
                TermDamage::Partial(iter) => {
                    self.dirty_rows.resize(num_rows, false);
                    self.dirty_row_bounds.resize(num_rows, None);
                    for line_damage in iter {
                        let row = line_damage.line;
                        if row < num_rows {
                            self.dirty_rows[row] = true;
                            self.dirty_row_bounds[row] =
                                Some((line_damage.left, line_damage.right));
                        }
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
            cell: PackedCell::from(ic.cell),
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
        let cursor_grid_cell: &alacritty_terminal::term::cell::Cell =
            &term.grid()[content.cursor.point];
        self.last_content.cursor_cell = PackedCell::from(cursor_grid_cell);
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
        // Resolve the current selection range now that the lock is held, so the
        // render path can read it from `last_content` instead of acquiring the
        // term lock a second time per frame.
        self.last_content.selection_range = term.selection.as_ref().and_then(|s| s.to_range(&term));

        // URL ranges are only consumed by link hover hit-testing. Skip the
        // per-row regex scans while the hover modifier is not held.
        let rebuild_all_urls = self.link_hover_active
            && (force_full_damage || self.last_content.url_ranges.len() != row_ranges.len());
        if !self.link_hover_active {
            self.last_content.url_ranges.clear();
        } else if rebuild_all_urls {
            self.last_content.url_ranges.clear();
            self.last_content.url_ranges.reserve(row_ranges.len());
            for (start, end) in row_ranges.iter() {
                let row_cells = &cells[*start..*end];
                let ranges = crate::terminal::link_detector::detect_urls_in_line(row_cells);
                self.last_content.url_ranges.push(ranges);
            }
        } else {
            for (row_idx, dirty) in self.dirty_rows.iter().copied().enumerate() {
                if !dirty {
                    continue;
                }
                let Some((start, end)) = row_ranges.get(row_idx).copied() else {
                    continue;
                };
                self.last_content.url_ranges[row_idx] =
                    crate::terminal::link_detector::detect_urls_in_line(&cells[start..end]);
            }
        }
    }

    pub fn sync(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.content_dirty {
            self.update_last_content(false);
            self.content_dirty = false;
        } else {
            // Nothing changed this frame – tell the element all rows are clean.
            self.dirty_rows.fill(false);
            self.dirty_row_bounds.fill(None);
            // Content is unchanged, but the selection may still have moved
            // (e.g. a drag via `update_selection` does not set `content_dirty`).
            // Refresh the cached selection range so the render path's read from
            // `last_content.selection_range` stays accurate without an extra
            // lock grab there.
            if self.selection_phase != SelectionPhase::Ended {
                let term = self.term.lock();
                self.last_content.selection_range =
                    term.selection.as_ref().and_then(|s| s.to_range(&term));
            }
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
        // 用户输入即将进入 PTY：维护输入区锚点（见 input_area_start 文档）。
        // - shell 前台且尚无锚点 → 以当前光标（提示符末尾）为锚点；
        // - 写入回车 → 提示符即将重绘，锚点失效，等待重新捕获。
        //
        // 本地终端看不到 OSC 133;B（alacritty EventLoop 直接消费 PTY 字节），
        // 这条回显锚点路径就是本地空行判定的唯一依据。
        self.observe_input_state();
        if memchr::memchr2(b'\r', b'\n', data).is_some() {
            self.input_area_start = None;
        }

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

    /// 当前光标在 grid 中的绝对坐标（与 [`Terminal::input_area_start`] 同一坐标系）。
    pub fn current_cursor_point(&self) -> AlacPoint {
        let term = self.term.lock();
        term.grid().cursor.point
    }

    /// 观察前台状态并维护输入区锚点，返回 shell 是否处于前台。
    ///
    /// - 检测到“程序退出 → shell 回到前台”的转换时，旧锚点指向的提示符
    ///   已经作废（如退出编辑器后的新提示符），需要失效重捕；
    /// - shell 前台且尚无锚点（刚重置 / 首次输入前）时，以当前光标为锚点。
    ///   回显发生前捕获，因此锚点即提示符末尾。
    fn observe_input_state(&mut self) -> bool {
        let fg = self.is_shell_in_foreground();
        if self.shell_fg_observed == Some(false) && fg {
            self.input_area_start = None;
        }
        self.shell_fg_observed = Some(fg);
        if fg && self.input_area_start.is_none() {
            self.input_area_start = Some(self.current_cursor_point());
        }
        fg
    }

    /// 判断 shell 输入行当前是否为空（用于内建命令 `/` 触发判定）。
    ///
    /// 原理：shell 会回显每一次行编辑，回显后光标总停在当前行内容的末尾，
    /// 因此“光标 == 输入区起点锚点” ⇔ 行内没有已输入的内容。锚点来源：
    /// - OSC 133;B（远程集成 shell，精确）；
    /// - 最近一次用户输入前捕获的回显锚点（本地 / 无集成 shell）。
    ///
    /// 程序在前台时始终返回 false（`/` 不应被拦截）。
    pub fn input_line_blank(&mut self) -> bool {
        let fg = self.observe_input_state();
        if !fg {
            return false;
        }
        match self.input_area_start {
            Some(anchor) => self.current_cursor_point() == anchor,
            // 理论不可达：observe_input_state 在 shell 前台时总会补上锚点。
            // 保守视为空行（与“新提示符”语义一致）。
            None => true,
        }
    }

    /// 显式终止底层进程/连接，避免终端实体被替换或关闭后造成资源泄漏。
    ///
    /// - 本地 PTY：向 alacritty 事件循环发送 `Msg::Shutdown`，触发 PTY / 子 shell 回收。
    /// - russh 远程：发送 `InputMsg::Close`（连接任务仅在收到 Close 或发送端断开时退出；
    ///   仅 drop `RusshTerminal` 无法终止连接，因为 `state` 被任务自身持有）。
    /// - PTY 模式远程：`SshPtyTerminal` 由 remote 回调闭包持有，随 `Terminal` 析构自动释放。
    pub fn kill(&mut self) {
        // 本地终端：通知底层事件循环关闭（会回收 shell 子进程）
        if let Some(sender) = &self.sender {
            let _ = sender.send(Msg::Shutdown);
        }
        // russh 远程：显式关闭 SSH 连接
        if let Some(russh) = &self.russh_handle {
            russh.close();
        }
    }

    pub fn resize(&mut self, bounds: TerminalBounds, _cx: &mut Context<Self>) {
        let previous_cols = self.terminal_bounds.num_columns();
        let previous_rows = self.terminal_bounds.num_lines();
        let cols = bounds.num_columns();
        let rows = bounds.num_lines();

        // Guard against invalid resize dimensions
        if cols == 0 || rows == 0 {
            log::warn!("Skipping resize with invalid dimensions: {}x{}", cols, rows);
            return;
        }

        self.terminal_bounds = bounds;

        if cols == previous_cols && rows == previous_rows {
            return;
        }

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
        // reflow 会改变行的折行与坐标，旧锚点必然过期；
        // shell 收到 SIGWINCH 后重绘提示符时会重新发 133;B，
        // 本地终端则由下一次输入重新捕获。
        self.input_area_start = None;
        // Alacritty does not mark resize as full damage. Treat the new grid as
        // a full renderer invalidation so stale row caches never survive a wrap
        // or viewport-size change.
        self.update_last_content(true);
    }

    /// Enable or disable lazy URL-range maintenance.
    ///
    /// While enabled (link-hover modifier held), `update_last_content` keeps
    /// `last_content.url_ranges` fresh for dirty rows. While disabled, all
    /// URL work is skipped entirely: the ranges are only consumed by hover
    /// hit-testing (`url_range_at_viewport_position`), so maintaining them on
    /// every output frame is wasted regex scanning and allocation churn.
    ///
    /// Turning the flag on rebuilds all rows immediately from the cached
    /// content snapshot — no term lock is needed because `last_content` is
    /// owned by `Self`.
    pub fn set_link_hover_active(&mut self, active: bool) {
        if self.link_hover_active == active {
            return;
        }
        self.link_hover_active = active;
        if active {
            let TerminalContent { cells, row_ranges, url_ranges, .. } = &mut self.last_content;
            url_ranges.clear();
            url_ranges.reserve(row_ranges.len());
            for (start, end) in row_ranges.iter() {
                let ranges = crate::terminal::link_detector::detect_urls_in_line(&cells[*start..*end]);
                url_ranges.push(ranges);
            }
        } else {
            self.last_content.url_ranges.clear();
        }
    }

    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    /// Find a URL at the given viewport position.
    /// Find a URL range at the given viewport position.
    /// Returns (url, start_col, end_col) if found.
    pub fn url_range_at_viewport_position(
        &self,
        viewport_row: usize,
        col: usize,
    ) -> Option<(SharedString, usize, usize)> {
        let content = &self.last_content;
        if viewport_row >= content.url_ranges.len() {
            return None;
        }
        for range in &content.url_ranges[viewport_row] {
            if col >= range.start_col && col < range.end_col {
                return Some((range.url.clone().into(), range.start_col, range.end_col));
            }
        }
        None
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
        mode.intersects(TermMode::MOUSE_MODE) && !mode.contains(TermMode::VI)
    }

    /// 清空旧选区并开始新选区（单次持锁，避免连续两次 term 锁）。
    fn restart_selection(&mut self, ty: SelectionType, point: AlacPoint, side: Side) {
        let mut term = self.term.lock();
        term.selection = Some(Selection::new(ty, point, side));
        self.selection_phase = SelectionPhase::Started;
    }

    fn update_selection(&mut self, mut point: AlacPoint, side: Side) -> bool {
        let mut term = self.term.lock();
        let mut selection = match term.selection.take() {
            Some(selection) => selection,
            None => return false,
        };

        point.line = std::cmp::min(point.line, term.bottommost_line());
        selection.update(point, side);
        term.selection = Some(selection);
        true
    }

    /// 将 gpui 的 click_count 归一化为 1..=3 的循环：1=字符、2=词、3=行。
    /// macOS 原生 clickCount 会随连续点击无限递增，第 4 击应回到字符选择
    /// （与 alacritty 的 1→2→3→1 循环一致），而不是落入未定义分支。
    fn normalized_click_count(click_count: usize) -> usize {
        if click_count == 0 {
            1
        } else {
            (click_count - 1) % 3 + 1
        }
    }

    pub fn mouse_down(&mut self, e: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        let position = e.position - self.terminal_bounds.bounds.origin;

        if self.mouse_mode(e.modifiers.shift) {
            self.mouse_report_active = true;
            let point = grid_point(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );
            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, true, self.last_content.mode)
            {
                self.write(&bytes);
            }
            return;
        }

        match e.button {
            gpui::MouseButton::Left => {
                let (point, side) = grid_point_and_side(
                    position,
                    self.terminal_bounds,
                    self.last_content.display_offset,
                );

                // Ctrl 按住时统一为矩形选区；否则按点击次数循环：
                // 1=字符 2=词 3=行（归一化后 4 击重新回到字符选择）。
                let ty = if e.modifiers.control {
                    SelectionType::Block
                } else {
                    match Self::normalized_click_count(e.click_count) {
                        1 => SelectionType::Simple,
                        2 => SelectionType::Semantic,
                        _ => SelectionType::Lines,
                    }
                };
                self.restart_selection(ty, point, side);
                cx.emit(Event::SelectionsChanged);
            }
            // 中键：本地模式下粘贴剪贴板（终端惯例；mouse mode 下已在上方转发给应用）
            gpui::MouseButton::Middle => {
                self.paste(cx);
            }
            // 右键：本地模式下无默认行为（mouse mode 下已在上方转发给应用）
            gpui::MouseButton::Right => {}
            gpui::MouseButton::Navigate(_) => {}
        }
    }

    pub fn mouse_up(&mut self, e: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        // 释放报文必须与按下报文配对：只有 mouse_down 时确实向应用上报了
        // press（mouse_report_active），才上报 release。若中途按住/松开
        // shift 导致 mouse_mode 判定翻转，也不会发出幽灵报文或丢失配对。
        if self.mouse_report_active {
            self.mouse_report_active = false;
            let position = e.position - self.terminal_bounds.bounds.origin;
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
            return;
        }

        // 本地模式：左键释放时若存在非空选区则 copy-on-select。
        // （右键不再复制：避免与未来的上下文菜单冲突，且原实现从未被触发过。）
        if e.button == gpui::MouseButton::Left {
            self.copy(cx);
        }

        self.selection_phase = SelectionPhase::Ended;
    }

    pub fn mouse_drag(&mut self, e: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        if self.selection_phase == SelectionPhase::Ended {
            return;
        }

        // 若左键已不再按下，说明拖拽在终端视图外结束（GPUI 元素级
        // `on_mouse_up` 仅在指针位于元素内时触发，视图外的释放不会送达）。
        // 此时收尾选择而不是继续延伸，否则后续悬停移动会无按键地
        // 继续扩展选区（幽灵拖拽）。
        if e.pressed_button != Some(gpui::MouseButton::Left) {
            self.selection_phase = SelectionPhase::Ended;
            return;
        }

        if self.mouse_mode(e.modifiers.shift) {
            return;
        }

        self.selection_phase = SelectionPhase::Selecting;
        let position = e.position - self.terminal_bounds.bounds.origin;
        self.last_mouse_position = Some(position);
        self.update_selection_at_position(position, cx);
    }

    /// 用相对于终端内容区域的像素坐标更新选区头部。
    /// display_offset 从 term 实时读取（拖拽期间自动滚动会改变它，
    /// `last_content.display_offset` 快照会滞后）。
    fn update_selection_at_position(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let display_offset = {
            let term = self.term.lock();
            term.grid().display_offset()
        };
        let (point, side) = grid_point_and_side(position, self.terminal_bounds, display_offset);
        if self.update_selection(point, side) {
            cx.emit(Event::SelectionsChanged);
        }
    }

    /// 拖拽选择时的边缘自动滚动：每次滚动 `lines` 行（正=向上进入
    /// scrollback，负=向下回到最新内容），然后用最后已知的鼠标位置
    /// 延伸选区头部到当前视口边缘。
    /// 由视图层的周期任务驱动（见 `TerminalView` 的 auto-scroll task）。
    pub fn autoscroll_selection(&mut self, lines: i32, cx: &mut Context<Self>) {
        if self.selection_phase == SelectionPhase::Ended {
            return;
        }

        let display_offset_changed = {
            let mut term = self.term.lock();
            let before = term.grid().display_offset();
            term.scroll_display(AlacScroll::Delta(lines));
            let after = term.grid().display_offset();
            before != after
        };

        if display_offset_changed {
            self.content_dirty = true;
            self.last_scroll_time.set(Instant::now());
        }

        if let Some(position) = self.last_mouse_position {
            self.update_selection_at_position(position, cx);
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
            /* Reset scroll state when the touch is cancelled */
            gpui::TouchPhase::Cancelled => {
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
            gpui::TouchPhase::Ended => {
                let remainder = (self.scroll_px / line_height).abs();
                let direction = if remainder > 0.5 {
                    Some(if self.scroll_px >= px(0.) { 1 } else { -1 })
                } else {
                    None
                };
                self.scroll_px = px(0.);
                direction
            }
        }
    }

    /// Reset the scroll-pixel accumulator (called on direction change).
    pub fn reset_scroll_px(&mut self) {
        self.scroll_px = px(0.);
    }

    // Selection
    pub fn selection_text(&self) -> Option<String> {
        let term = self.term.lock();
        term.selection_to_string()
    }

    pub fn clear_selection(&mut self) {
        let mut term = self.term.lock();
        term.selection = None;
        self.selection_phase = SelectionPhase::Ended;
        // `is_cell_selected` / the render path read `last_content.selection_range`
        // instead of re-locking the term. Keep the cached value in sync here:
        // because `selection_phase` is now `Ended`, `sync()`'s else-branch will
        // NOT refresh it, so without this it would linger stale until the next
        // `content_dirty` frame.
        self.last_content.selection_range = None;
    }

    /// 全选：从 scrollback 顶行到屏幕底行（含最后一列）。
    /// 此前实现用 `Selection::new(Lines, Line(0)..Line(0))`，两个锚点相同，
    /// `to_range` 只会把范围扩展到首个逻辑行的边界——实际只选中了第一行。
    /// 正确范围应从 `topmost_line()`（scrollback 最顶部，负值）到
    /// `bottommost_line()`（屏幕底行）。
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let mut term = self.term.lock();
        term.selection = Some(build_select_all_selection(&term));
        // Resolve the new selection immediately so the first frame after a
        // "select all" action shows the highlight without waiting for a
        // content-dirty cycle.
        self.last_content.selection_range = term.selection.as_ref().and_then(|s| s.to_range(&term));
        self.selection_phase = SelectionPhase::Ended;
        cx.emit(Event::SelectionsChanged);
    }

    // Scrolling
    // 注意：scroll_display 只更新 term 内部的 display_offset，视口快照
    // （last_content）依赖 sync() 的 content_dirty 分支刷新。这里统一置位
    // content_dirty，否则键盘滚动后视口/选区高亮会停留在旧数据。
    pub fn scroll_line_up(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Delta(1));
        drop(term);
        self.content_dirty = true;
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_line_down(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Delta(-1));
        drop(term);
        self.content_dirty = true;
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_page_up(&mut self) {
        let mut term = self.term.lock();
        let lines = term.screen_lines() as i32;
        term.scroll_display(AlacScroll::Delta(lines));
        drop(term);
        self.content_dirty = true;
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_page_down(&mut self) {
        let mut term = self.term.lock();
        let lines = term.screen_lines() as i32;
        term.scroll_display(AlacScroll::Delta(-lines));
        drop(term);
        self.content_dirty = true;
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_to_top(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Top);
        drop(term);
        self.content_dirty = true;
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_to_bottom(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Bottom);
        drop(term);
        self.content_dirty = true;
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
        if let Some(text) = term.selection_to_string().filter(|text| !text.is_empty()) {
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
        // 清屏后光标归位到屏幕左上角，旧锚点坐标必然过期。
        self.input_area_start = None;
        let mut term = self.term.lock();
        term.clear_screen(ClearMode::All);
    }
}

impl EventEmitter<Event> for Terminal {}

impl Drop for Terminal {
    fn drop(&mut self) {
        // 终端被丢弃时显式回收底层资源：
        // - 本地 PTY：`sender` 断开会让 alacritty 事件循环退出并回收 shell；此处再补发
        //   `Msg::Shutdown` 以确保即使仍被短暂引用也能及时关闭。
        // - russh 远程：连接任务仅在收到 `InputMsg::Close`（或发送端断开）时退出，
        //   仅 drop `RusshTerminal` 无法终止连接，必须显式 `close()`。
        // - PTY 模式远程：`SshPtyTerminal` 由 remote 回调闭包持有，会随 `Terminal` 析构释放。
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::{Terminal, TerminalType};
    use alacritty_terminal::term::test::mock_term;
    use alacritty_terminal::vte::ansi::Handler;
    use gpui::{AppContext as _, TestAppContext};

    #[test]
    fn click_count_normalizes_in_cycles_of_three() {
        // macOS 的 clickCount 随连续点击无限递增：1/2/3 击后第 4 击
        // 应回到字符选择，而不是落入未定义分支。
        assert_eq!(Terminal::normalized_click_count(1), 1);
        assert_eq!(Terminal::normalized_click_count(2), 2);
        assert_eq!(Terminal::normalized_click_count(3), 3);
        assert_eq!(Terminal::normalized_click_count(4), 1);
        assert_eq!(Terminal::normalized_click_count(5), 2);
        assert_eq!(Terminal::normalized_click_count(6), 3);
        assert_eq!(Terminal::normalized_click_count(7), 1);
        // 异常输入退化到单击。
        assert_eq!(Terminal::normalized_click_count(0), 1);
    }

    #[test]
    fn select_all_covers_scrollback_to_bottom() {
        // 2 行屏幕，写入额外换行制造 scrollback 历史。
        let mut term = mock_term("one\ntwo");
        for _ in 0..3 {
            term.input('\n');
        }

        let selection = super::build_select_all_selection(&term);
        let range = selection
            .to_range(&term)
            .expect("select-all selection should not be empty");

        use alacritty_terminal::grid::Dimensions;
        assert_eq!(
            range.start.line,
            term.topmost_line(),
            "应从 scrollback 顶部开始"
        );
        assert_eq!(range.start.column, alacritty_terminal::index::Column(0));
        assert_eq!(
            range.end.line,
            term.bottommost_line(),
            "应覆盖到屏幕底行（而不仅是第一行）"
        );
        assert_eq!(range.end.column, term.last_column());
    }

    #[test]
    fn select_all_without_scrollback_covers_screen() {
        let term = mock_term("abc\ndef");
        let selection = super::build_select_all_selection(&term);
        let range = selection.to_range(&term).expect("range should exist");

        use alacritty_terminal::grid::Dimensions;
        assert_eq!(range.start.line, term.topmost_line());
        assert_eq!(range.end.line, term.bottommost_line());
    }

    #[test]
    fn parses_file_scheme_osc7_cwd() {
        assert_eq!(
            Terminal::parse_osc7_working_directory_uri(
                "file://remote-host/home/alice/project",
                false
            ),
            Some("/home/alice/project".to_string())
        );
    }

    #[test]
    fn parses_kitty_shell_cwd_scheme() {
        assert_eq!(
            Terminal::parse_osc7_working_directory_uri(
                "kitty-shell-cwd://remote-host/tmp/build",
                false
            ),
            Some("/tmp/build".to_string())
        );
    }

    #[test]
    fn decodes_percent_encoded_path_segments() {
        assert_eq!(
            Terminal::parse_osc7_working_directory_uri(
                "file://remote-host/Users/alice/My%20Project/%E4%B8%AD%E6%96%87",
                false
            ),
            Some("/Users/alice/My Project/中文".to_string())
        );
    }

    #[test]
    fn rejects_relative_osc7_paths() {
        assert_eq!(
            Terminal::parse_osc7_working_directory_uri("file://remote-host/../relative", false),
            Some("/../relative".to_string())
        );
        assert_eq!(
            Terminal::parse_osc7_working_directory_uri("relative/path", false),
            None
        );
    }

    #[test]
    fn local_context_requires_local_host() {
        assert_eq!(
            Terminal::parse_osc7_working_directory_uri(
                "file://definitely-not-this-machine.local/tmp",
                true
            ),
            None
        );
    }

    #[test]
    fn parses_remote_working_directory_osc_variants() {
        assert_eq!(
            Terminal::parse_working_directory_osc(b"7;file://host/home/alice", false),
            Some("/home/alice".to_string())
        );
        assert_eq!(
            Terminal::parse_working_directory_osc(b"1337;CurrentDir=/var/www", false),
            Some("/var/www".to_string())
        );
        assert_eq!(
            Terminal::parse_working_directory_osc(b"633;P;Cwd=/opt/app", false),
            Some("/opt/app".to_string())
        );
    }

    // ===== 输入区锚点（空行判定）测试 =====

    /// 构造无 PTY 的终端，并伪装成"就绪的远程 shell"，
    /// 使 `is_shell_in_foreground` 走 `remote_shell_ready` 分支返回 true。
    fn anchor_test_terminal() -> Terminal {
        let mut t = Terminal::create_error_terminal();
        t.terminal_type = TerminalType::Remote {
            host: "test".into(),
            username: "user".into(),
            port: 22,
        };
        t.remote_shell_ready = true;
        t
    }

    /// 远程集成 shell 的提示符数据在同一 chunk 内：
    /// 133;A + 提示符文本 + 133;B + 用户输入回显。
    /// 锚点必须落在提示符文本之后、回显之前（验证分段喂入）。
    #[gpui::test]
    fn osc133_b_anchor_reflects_preceding_prompt_text(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.handle_pty_data(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\abc", false, cx);
            let anchor = t.input_area_start.expect("133;B 应捕获锚点");
            let cursor = t.current_cursor_point();
            assert_eq!(anchor.column.0, 2, "锚点应在 '$ ' 之后（列 2）");
            assert_eq!(cursor.column.0, 5, "回显 'abc' 后光标应在列 5");
            assert_eq!(anchor.line, cursor.line);
        });
    }

    /// OSC 终止符落在下一个 chunk 时应被拼接重扫，而不是永久丢失。
    #[gpui::test]
    fn osc133_b_split_across_chunks_is_reassembled(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.handle_pty_data(b"$ \x1b]133", false, cx);
            assert!(t.input_area_start.is_none(), "不完整 OSC 不应误判");
            t.handle_pty_data(b";B\x1b\\", false, cx);
            let anchor = t.input_area_start.expect("跨 chunk 的 133;B 也应被捕获");
            assert_eq!(anchor.column.0, 2);
            assert!(t.pending_osc.is_empty(), "处理后残尾应清空");
        });
    }

    /// 无终止符的超长 ESC ] 输出不应无限累积在 pending 缓冲中。
    #[gpui::test]
    fn oversized_unterminated_osc_does_not_accumulate(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        let mut data = vec![0x1b, b']'];
        data.extend(std::iter::repeat(b'x').take(9 * 1024));
        terminal.update(cx, |t, cx| {
            t.handle_pty_data(&data, false, cx);
            assert!(t.pending_osc.is_empty());
        });
    }

    /// 空行判定随回显内容变化：空 → 输入后非空 → 退格删空后恢复可触发。
    /// 对应用户报告的两个症状。
    #[gpui::test]
    fn input_line_blank_tracks_echoed_content(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.handle_pty_data(b"$ \x1b]133;B\x1b\\", false, cx);
            assert!(t.input_line_blank(), "新提示符处应为空行");

            t.handle_pty_data(b"abc", false, cx);
            assert!(!t.input_line_blank(), "行内已有内容时不应触发（症状二）");

            // BS + 空格覆盖 + BS：退格删除一个字符（光标净减一）
            t.handle_pty_data(b"\x08 \x08\x08 \x08\x08 \x08", false, cx);
            assert!(t.input_line_blank(), "退格删空后应恢复可触发");
        });
    }

    /// Ctrl+C 后 shell 重绘提示符并重新发 133;B：空行判定恢复（症状一）。
    #[gpui::test]
    fn input_line_blank_recovered_after_prompt_redraw(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.handle_pty_data(b"$ \x1b]133;B\x1b\\abc", false, cx);
            assert!(!t.input_line_blank());

            // 光标回到提示符末尾并重发 133;B
            t.handle_pty_data(b"\r  \x1b]133;B\x1b\\", false, cx);
            assert!(t.input_line_blank());
        });
    }

    /// 本地路径（无 133;B）：write 捕获回显锚点，回车写入使锚点失效。
    #[gpui::test]
    fn write_captures_and_resets_echo_anchor(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.handle_pty_data(b"$ ", false, cx);
            assert!(t.input_area_start.is_none(), "无 133;B 时锚点待捕获");

            // 首次输入前捕获：锚点 = 提示符末尾（回显尚未发生）
            t.write(b"ab");
            assert_eq!(t.input_area_start.unwrap().column.0, 2);

            // 回车写入：提示符将重绘，锚点失效等待重捕
            t.write(b"\r");
            assert!(t.input_area_start.is_none());
        });
    }

    /// 前台程序退出后（fg 转换），旧锚点失效，首次输入重新捕获。
    #[gpui::test]
    fn foreground_return_recaptures_anchor(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.handle_pty_data(b"$ \x1b]133;B\x1b\\", false, cx);
            assert!(t.input_line_blank());

            // 133;C：程序进入前台 → 不允许触发
            t.handle_pty_data(b"\x1b]133;C\x1b\\vim running...", false, cx);
            assert!(!t.input_line_blank());

            // 程序退出后 shell 回到前台（133;A）；
            // 模拟本地 shell（无 133;B）：首次输入触发 fg 转换重捕
            t.remote_shell_ready = true;
            t.write(b"x");
            let anchor = t.input_area_start.expect("fg 转换后应重新捕获锚点");
            assert_eq!(anchor, t.current_cursor_point());
        });
    }
}
