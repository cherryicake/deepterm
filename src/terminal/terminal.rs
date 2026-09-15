//! Terminal module - simplified but functional

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    borrow::Cow,
    cell::Cell,
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    sync::OnceLock,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    Term,
    event::{Event as AlacTermEvent, EventListener, WindowSize},
    event_loop::{EventLoopSender, Msg},
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
// unix 本地终端走自建 `local_pty::LocalReader`，不再构造 alacritty 的 EventLoop。
#[cfg(not(unix))]
use alacritty_terminal::event_loop::EventLoop;
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
use crate::terminal::file_link_detector::{FileHit, detect_files_in_joined_text};
use crate::terminal::kitty::scanner::ReplySink;
use crate::terminal::kitty::storage::Storage as KittyStorage;
use crate::terminal::link_detector::{HyperlinkRange, UrlRange};
use crate::terminal::mappings::mouse::{
    alt_scroll, grid_point, grid_point_and_side, mouse_button_report, mouse_moved_report,
    scroll_report,
};

/// 最大滚动缓冲区行数限制
const MAX_SCROLLBACK_LINES: usize = 100000;

/// resize 合并间隔（1:1 移植 ghostty `termio/Thread.zig` 的 `Coalesce.min_ms = 25`）。
///
/// ghostty 语义：定时器活动期间只更新最新值、**不重置定时器**（节流而非防抖），
/// 持续拖动期间以稳定 ~40Hz 应用；应用时 pty、网格、渲染尺寸三者一次到位，
/// 渲染层在两次应用之间始终使用「已应用」的尺寸。
const RESIZE_COALESCE_DELAY: Duration = Duration::from_millis(25);

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
    /// 终端应用请求的桌面通知（OSC 9 / OSC 777）。
    NotificationReceived {
        title: String,
        body: String,
    },
    /// 终端应用通过 DEC 私有模式 ?12 或 CSI `Ps SP q` 改变了光标闪烁策略。
    /// `bool` 为 true 表示应用希望光标闪烁。
    BlinkChanged(bool),
}

#[derive(Clone)]
pub struct ZedListener(pub UnboundedSender<AlacTermEvent>);

/// 远程数据驱动事件通道。
///
/// 有界（async_channel）：SSH 读循环在 UI 消费不过来时通过 `send().await`
/// 挂起，背压经 SSH 窗口传导到远端，避免无界队列在 UI 卡顿时无限膨胀。
#[derive(Clone)]
pub struct DriverListener(pub async_channel::Sender<DriverEvent>);

/// Kitty 图形协议可占用的图像内存上限（320 MB）。
///
/// 与 ghostty 的 `image-storage-limit` 默认值**逐字节一致**
/// （`config/Config.zig`: `320 * 1000 * 1000`，即十进制 320 MB；不是
/// `320 * 1024 * 1024`）。ghostty 在 `termio/Termio.zig` 用该配置调用
/// `setKittyGraphicsSizeLimit`，这里是同一语义的编译期常量。
///
/// 单张图上限另有 [`crate::terminal::kitty::image`] 的 `MAX_IMAGE_BYTES`
/// 把关；这里是**全部图像合计**，超限按优先级淘汰。
const KITTY_IMAGE_LIMIT: usize = 320 * 1000 * 1000;

/// Kitty 协议应答的投递通道。
///
/// 有两个消费者，靠 `mem::take` 保证应答只被回写一次：
/// - **本地读线程**：`take_pending()` 取走后直接 `write_all` 给 PTY。这是
///   常规路径——终端应用的能力探测只等数百毫秒，必须即时回写。
/// - **UI 线程**：`flush_kitty_replies()` 兜底（如应答产生于 UI 侧的扫描）。
struct KittyReplySink {
    /// 待回写的应答字节（kitty 规定应答必须按请求顺序回写）。
    replies: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// 图像集合已变化标志，UI 据此决定是否需要重绘。
    changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ReplySink for KittyReplySink {
    fn reply(&mut self, bytes: &[u8]) {
        if let Ok(mut buf) = self.replies.lock() {
            buf.extend_from_slice(bytes);
        }
    }

    fn images_changed(&mut self) {
        self.changed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 把已排队的应答取走，供读线程直接回写 PTY。
    ///
    /// 与 UI 线程的 `flush_kitty_replies` 是**互斥**的两条回写路径：谁先
    /// 取到谁写，另一方看到空缓冲就跳过（`mem::take` 保证了不重复）。
    /// 之所以允许读线程直接写，是因为终端应用的能力探测只等 500ms，而
    /// UI 侧的重绘泵唤醒条件基于 vte 字节吞吐，探测负载（纯 APC）不满足。
    fn take_pending(&mut self) -> Vec<u8> {
        match self.replies.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        }
    }
}

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

/// shell integration 脚本目录：按 uid 隔离，避免多用户共享固定路径。
/// 共享固定路径存在两类风险：多实例写竞态；同机其他用户可预建目录/
/// 替换脚本，让本应用以当前用户身份执行任意 shell 代码。
fn shell_integration_root() -> PathBuf {
    #[cfg(unix)]
    {
        // temp_dir 本身在 macOS/Linux 上就是按用户隔离的，再叠加 uid
        // 兜底（如 TMPDIR 被指向共享目录时）
        // SAFETY（中文）：`getuid` 为 async-signal-safe 的纯查询调用，
        // 无指针参数、无全局状态写入，多线程并发调用安全。
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("deepterm-shell-integration-{uid}"))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("deepterm-shell-integration")
    }
}

/// 校验 integration 根目录可信：必须存在、是目录、属主为当前用户、
/// 且对 group/other 不可写（防符号链接/替换攻击）。
#[cfg(unix)]
fn integration_dir_is_trusted(root: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match fs::metadata(root) {
        Ok(md) => {
            // SAFETY（中文）：`getuid` 同上，为只读查询，并发安全；
            // `metadata` 结果与 `getuid` 比较用于防同机预建目录攻击。
            md.is_dir()
                && md.uid() == unsafe { libc::getuid() }
                && md.permissions().mode() & 0o022 == 0
        }
        Err(_) => false,
    }
}

/// 创建目录并把权限收紧为 0700（unix）。
fn ensure_secure_dir(path: &std::path::Path) -> Option<()> {
    fs::create_dir_all(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Some(())
}

/// 当前主机名（OSC 7 本地主机校验用）。
///
/// unix：gethostname(2) 纯 syscall（此前为一次 `/bin/hostname` 的
/// fork+exec+waitpid，且发生在数据处理路径上）。Windows：
/// COMPUTERNAME 环境变量足够本校验用途。
fn hostname_via_syscall() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY（中文）：gethostname 为只读 syscall，缓冲区指针/长度均有效；
        // 名字超长时 Linux 截断并补 NUL、macOS 返回 ERANGE（此时退回空串）。
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if ok != 0 {
            return String::new();
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..nul]).trim().to_string()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

fn write_local_shell_integration_files() -> Option<PathBuf> {
    let root = shell_integration_root();

    // 已存在的根目录若不可信（非本人所有 / group·other 可写），整个清除重建
    #[cfg(unix)]
    if root.exists() && !integration_dir_is_trusted(&root) {
        fs::remove_dir_all(&root).ok()?;
    }

    let bash_dir = root.join("shell-integration/bash");
    let zsh_dir = root.join("shell-integration/zsh");
    ensure_secure_dir(&root)?;
    ensure_secure_dir(&bash_dir)?;
    ensure_secure_dir(&zsh_dir)?;

    // 关键点：这些文件直接来自 Ghostty 的 shell integration 资源结构，
    // 只做 DeepTerm 命名适配。运行时复制资源文件，而不是手写/拼接 shell
    // wrapper，避免 zsh/bash 启动语义和 Ghostty 分叉。
    let integration_files = [
        (
            bash_dir.join("deepterm.bash"),
            include_str!("../../assets/shell-integration/bash/deepterm.bash"),
        ),
        (
            bash_dir.join("bash-preexec.sh"),
            include_str!("../../assets/shell-integration/bash/bash-preexec.sh"),
        ),
        (
            zsh_dir.join(".zshenv"),
            include_str!("../../assets/shell-integration/zsh/.zshenv"),
        ),
        (
            zsh_dir.join("deepterm-integration"),
            include_str!("../../assets/shell-integration/zsh/deepterm-integration"),
        ),
    ];

    for (path, content) in integration_files {
        // 脚本只允许属主读写（0600），防止被同机其他用户篡改
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .ok()?
                .write_all(content.as_bytes())
                .ok()?;
        }
        #[cfg(not(unix))]
        fs::write(&path, content).ok()?;
    }

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

    /// 上报用的几何：cell 尺寸乘显示缩放，行列数不变。
    ///
    /// `self` 存的是**逻辑点**（布局与命中测试依赖它），而 ghostty 的
    /// `size_report` 用的是**物理设备像素**——`renderer/size.zig:24-27`
    /// 明确要求：「any pixel values should already be scaled to the current
    /// DPI of the screen」。ghostty 从字号就按 DPI 换算
    /// （`font/face.zig:55-58` 的 `points * ydpi / 72`，其中
    /// `ydpi = content_scale * 72`），所以 cell 尺寸天生就是物理像素。
    ///
    /// 取整用 `round` 对齐 `font/Metrics.zig:265` 的 `@round`：截断每列会
    /// 稳定少 1px，几十列累积后比例就歪了。
    pub fn term_geometry(&self, scale: f32) -> crate::terminal::escape_scan::TermGeometry {
        let scale = normalize_scale(scale);
        crate::terminal::escape_scan::TermGeometry {
            rows: self.num_lines() as u16,
            cols: self.num_columns() as u16,
            cell_width: (self.cell_width.as_f32() * scale).round().max(1.0) as u16,
            cell_height: (self.line_height.as_f32() * scale).round().max(1.0) as u16,
        }
    }

    /// 上报用的 `WindowSize`：只缩放 cell 像素尺寸，行列数保持原值。
    ///
    /// 两个消费方：
    /// - PTY 的 `TIOCSWINSZ`：alacritty 自己算 `ws_xpixel = ws_col *
    ///   cell_width`（`tty/unix.rs:433`），所以这里给物理像素即可。
    /// - `CSI 14 t`：`term/mod.rs:2261` 算 `num_lines * cell_height`。
    ///
    /// 与 ghostty 的有意偏差：ghostty 直接把 surface 的原始像素宽高塞进
    /// `ws_xpixel`（`termio/Exec.zig:912-913`），我们只能透传 cell 尺寸，
    /// 于是得到的是 **grid 范围**（`floor(W/cw) * cw * scale`），最多比真实
    /// surface 小一个缩放后的 cell。**不要为了补这个差去加字段**——差一个
    /// cell 远好过差 2×，而且 `WindowSize` 没有 xpixel 字段可填。
    ///
    /// `num_lines`/`num_cols` **绝不能缩放**：它们是格子数，与 DPI 无关。
    pub fn window_size(&self, scale: f32) -> WindowSize {
        let scale = normalize_scale(scale);
        WindowSize {
            num_lines: self.num_lines() as u16,
            num_cols: self.num_columns() as u16,
            cell_width: (self.cell_width.as_f32() * scale).round().max(1.0) as u16,
            cell_height: (self.line_height.as_f32() * scale).round().max(1.0) as u16,
        }
    }
}

/// 把外部传入的缩放归一为「可用的正数」。
///
/// `scale_factor()` 正常返回 1.0/2.0 这类值，但 headless 测试或平台初始化
/// 早期可能拿到 0 或 NaN——那种情况下 `cell * scale` 会算出 0 或垃圾值，
/// 让终端应用按 0×0 建 surface。退化为 1.0 至少保持上一轮修复前的行为。
fn normalize_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
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

/// 几何的第二种投影，供尺寸上报（`CSI 16 t` / in-band size reports）使用。
///
/// 与上面的 `WindowSize` 投影**必须保持同口径**：两者都由终端应用用来推算
/// 像素尺寸，出现分歧会导致画面比例错误。`from_bounds_matches_window_size`
/// 测试锁定这一点。
///
/// 这是按 `scale = 1.0`（即**逻辑点**）的便捷投影，只应在「还没有窗口、
/// 拿不到 DPI」的场合使用（如构造期的 PTY 初始尺寸）。真正上报给终端应用
/// 时必须走 [`TerminalBounds::term_geometry`]，否则 Retina 上会低报 2×。
impl From<TerminalBounds> for crate::terminal::escape_scan::TermGeometry {
    fn from(val: TerminalBounds) -> Self {
        Self {
            rows: val.num_lines() as u16,
            cols: val.num_columns() as u16,
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
    /// Per-row OSC8 hyperlink ranges (preserved from `CellExtra::hyperlink`).
    /// Same lazy lifecycle as `url_ranges`: only maintained while the
    /// link-hover modifier is held. See `set_link_hover_active`.
    pub hyperlink_ranges: Vec<Vec<HyperlinkRange>>,
    /// Current selection range, resolved while the term lock is already held
    /// in `update_last_content`. Read by the render hot path so it does not
    /// need to acquire the `FairMutex<Term>` a second time per frame.
    pub selection_range: Option<alacritty_terminal::selection::SelectionRange>,
    /// 当前生效调色板快照（主题基色 + 应用 OSC 4/10/11/12 改色叠加）。
    /// 见 [`crate::terminal::mappings::colors::TerminalPalette`]。
    pub palette: crate::terminal::mappings::colors::TerminalPalette,
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
            hyperlink_ranges: Vec::new(),
            selection_range: None,
            palette: Default::default(),
        }
    }
}

/// Compact snapshot of the fields from alacritty's `Cell` that the render
/// pipeline actually reads.
///
/// `IndexedCell.cell` stores this instead of a full `alacritty_terminal::Cell`
/// so that the per-frame clone of every visible cell (see `update_last_content`)
/// copies only `c`/`fg`/`bg`/`flags`/`underline_color`. OSC8 hyperlinks are
/// preserved separately as an interned `IndexedCell.hyperlink` (`None` for the
/// vast majority of cells) so hover hit-testing can run without the term lock.
/// See `link_detector::detect_hyperlinks_in_line`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedCell {
    pub c: char,
    pub fg: alacritty_terminal::vte::ansi::Color,
    pub bg: alacritty_terminal::vte::ansi::Color,
    pub flags: alacritty_terminal::term::cell::Flags,
    /// SGR 58 下划线专用色（None = 未设置，渲染时回退前景色）。
    pub underline_color: Option<alacritty_terminal::vte::ansi::Color>,
}

impl From<&alacritty_terminal::term::cell::Cell> for PackedCell {
    #[inline]
    fn from(cell: &alacritty_terminal::term::cell::Cell) -> Self {
        Self {
            c: cell.c,
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
            underline_color: cell.underline_color(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexedCell {
    pub point: AlacPoint,
    pub cell: PackedCell,
    /// 该单元格的组合变音符（zero-width），最多前 3 个。
    ///
    /// 只有 Kitty 图形协议的 Unicode 占位符（U+10EEEE）会读它——行/列下标
    /// 编码在第 1/2 个变音符、图像 id 的高 8 位在第 3 个（见
    /// [`crate::terminal::kitty::unicode`]）。绝大多数单元格是 `None`，所以
    /// 放在 `IndexedCell` 而不是 `PackedCell` 上：后者要维持 `Copy`，而
    /// alacritty 的 `zerowidth()` 返回的是借用切片。
    pub zerowidth: Option<[char; 3]>,
    /// OSC8 hyperlink for this cell (`None` = no link).
    /// alacritty's `Hyperlink` is `Arc`-backed, so snapshotting it per cell
    /// costs one refcount bump and no string allocation; `id`/`uri` strings
    /// are only materialized when grouping ranges for hover hit-testing.
    pub hyperlink: Option<alacritty_terminal::term::cell::Hyperlink>,
}

/// 取一个单元格的前 3 个组合变音符，没有则返回 `None`。
///
/// 位置不足 3 个时后面的位置填 `'\0'`；这些位置永远不会被查到
/// （占位符解码按 `diacritics[0]`/`[1]`/`[2]` 顺序读取，读不到就算"没写"，
/// 所以需要能区分"没有第 3 个变音符"与"第 3 个变音符是某个值"）。
fn packed_zerowidth(cell: &alacritty_terminal::term::cell::Cell) -> Option<[char; 3]> {
    let chars = cell.zerowidth()?;
    let mut out = ['\0'; 3];
    for (slot, ch) in out.iter_mut().zip(chars.iter()) {
        *slot = *ch;
    }
    Some(out)
}

/// 滚动区搜索命中：起点 / 终点为 grid 绝对坐标（同一行内，`end` 为开区间列）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: AlacPoint,
    pub end: AlacPoint,
}

/// 远程终端输入回调类型
pub type RemoteInputCallback = Box<dyn Fn(&[u8]) + Send>;

/// 远程终端调整大小回调类型
/// 远程窗口尺寸变更回调：`(cols, rows, scale)`。
///
/// `scale` 是窗口所在显示的缩放，用于把逻辑点的网格尺寸换算成物理像素后再
/// 交给 PTY（`ws_xpixel`）。放在参数里而不是让实现方自己捕获，是因为捕获
/// 会在终端创建时就固化一个值，跨显示器拖动后就是过期数据。
pub type RemoteResizeCallback = Box<dyn Fn(u16, u16, f32) + Send>;

/// Kitty 图形协议的图像集合。
///
/// 两条字节管线（本地读线程 / 远程 UI 线程）都写入同一份存储，因为本地
/// 终端的读线程与 UI 线程需要共享它：读线程解析并落盘图像，UI 线程渲染。
/// 用 `Arc<Mutex<..>>` 而非 `FairMutex`——读写都是短临界区（哈希表插入/
/// 快照遍历），不涉及 vte 解析，无需 alacritty 那套让锁语义。
///
/// `generation` 独立于 `Terminal::content_generation`：图像变化不改变网格内容，
/// 但需要触发重绘，用单独的代际号可以精确地区分「只有图像变了」。
///
/// 内部只有两个 `Arc`，`Clone` 即共享同一份存储与代际号（浅拷贝），这正是
/// 扫描器与渲染层看到同一份图像的语义。
#[derive(Clone)]
pub struct KittyImages {
    storage: std::sync::Arc<std::sync::Mutex<KittyStorage>>,
    /// 存储变更代际（每次插入/删除/放置时自增）。
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl KittyImages {
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            storage: std::sync::Arc::new(std::sync::Mutex::new(KittyStorage::new(limit_bytes))),
            generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// 存储句柄（供扫描器构造 `ImageStore`）。
    pub fn storage(&self) -> std::sync::Arc<std::sync::Mutex<KittyStorage>> {
        self.storage.clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 在当前代际上叠加 `delta`（由渲染层读到时使用，避免额外加锁读两次）。
    pub fn bump_generation(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 只读访问存储（渲染/测试用）。持锁期间执行，勿在其中再取 `term` 锁。
    pub fn with_storage<R>(&self, f: impl FnOnce(&KittyStorage) -> R) -> R {
        let guard = self.storage.lock().expect("kitty 图像存储锁被投毒");
        f(&guard)
    }

    /// 可变访问存储。只给 UI 线程上的操作使用（如推进动画帧）——读线程
    /// 永远只读，因此这里的可变借用不会与渲染路径冲突。同样勿在其中取
    /// `term` 锁。
    pub fn with_storage_mut<R>(&self, f: impl FnOnce(&mut KittyStorage) -> R) -> R {
        let mut guard = self.storage.lock().expect("kitty 图像存储锁被投毒");
        f(&mut guard)
    }
}

impl Default for KittyImages {
    fn default() -> Self {
        Self::new(KITTY_IMAGE_LIMIT)
    }
}

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
    /// 本地终端的事件循环发送器（仅 Windows：conpty 无裸 fd，沿用 alacritty EventLoop）
    sender: Option<EventLoopSender>,
    /// 本地终端读线程的发送端（仅 unix）。与 `sender` 互斥，两者只会有一个为 `Some`。
    #[cfg(unix)]
    local_sender: Option<crate::terminal::local_pty::LocalSender>,
    /// 本地读线程句柄（仅 unix）。持有它以保证读线程存活至终端析构。
    #[cfg(unix)]
    #[allow(dead_code)]
    local_reader: Option<crate::terminal::local_pty::LocalReader>,
    /// Kitty 图形协议已存储的图像（本地路径：读线程写入，UI 线程只读渲染）。
    ///
    /// 远程路径下扫描发生在 UI 线程，直接由这里的扫描器更新。
    images: KittyImages,
    /// 待回写给 PTY 的协议应答（由读线程/扫描器投递，UI 线程消费）。
    kitty_replies: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// 图像集合自上次重绘以来是否变化（读线程置位，UI 线程清除）。
    kitty_images_dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 图像变更代际：每次重绘时若与快照不同则触发一次全量重绘。
    kitty_generation: u64,
    last_kitty_generation: u64,
    /// 远程路径的协议扫描器。
    ///
    /// 本地路径的扫描发生在读线程内，UI 线程只读结果；远程路径的字节本来
    /// 就在 UI 线程解析，因此扫描器也放在这里（`None` = 本地/headless）。
    kitty_scanner: Option<crate::terminal::kitty::scanner::KittyGraphicsScanner>,
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
    /// 最新待应用的窗口尺寸（合并器只保留最新值；网格与 PTY/SSH 一起原子应用）。
    pending_resize: Option<TerminalBounds>,
    /// resize 合并任务句柄（ghostty 式 25ms 非重置节流）；drop 即取消。
    resize_task: Option<Task<()>>,
    /// 是否已完成首次 resize。deepterm 特有：PTY 以默认 55×18 启动，首帧需立即
    /// 校正（ghostty spawn 时就用 surface 实际尺寸创建 PTY，无此问题）。
    resize_primed: bool,
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
    /// 远程路径的 in-band size reports（DECSET 2048）开关状态。
    ///
    /// 本地路径的状态在扫描器里（`KittyGraphicsScanner::mode_2048`）；远程
    /// 路径没有读线程扫描器参与尺寸嗅探，因此在这里单独跟踪。
    remote_mode_2048: bool,
    /// SGR-Pixels 鼠标编码（DECSET 1016）开关状态。
    ///
    /// 对齐 ghostty `terminal/modes.zig:318` 的 `mouse_format_sgr_pixels`。
    /// 与 2048 同理：模式 1016 不在 vte/alacritty 的枚举里，会被当作
    /// `PrivateMode::Unknown`，因此无法从 `last_content.mode` 读出来——由
    /// deepterm 自己在字节流里拦截 `DECSET ?1016` 并跟踪。
    ///
    /// 本地路径的状态在扫描器里（`KittyGraphicsScanner::mode_1016`），这里
    /// 单独保存远程路径的状态。
    remote_mode_1016: bool,
    /// 本地路径的 1016 状态句柄（读线程写、UI 线程读）。
    ///
    /// 本地终端的字节由读线程里的扫描器消费，模式开关状态留在那边；鼠标报文
    /// 却在 UI 线程编码，因此需要这个共享原子量把状态送过来。与
    /// `kitty_images_dirty` 是同一套跨线程做法。
    local_mode_1016: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 当前窗口所在的显示缩放（`Window::scale_factor()`，Retina 上为 2.0）。
    ///
    /// `terminal_bounds` 存逻辑点，而尺寸上报必须给物理像素
    /// （ghostty `renderer/size.zig:24-27`），两者相差正好这个系数。
    /// 只用于 [`TerminalBounds::term_geometry`] / `window_size` 的投影，
    /// 绝不参与布局或命中测试。
    display_scale: f32,
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
    /// MOUSE_MOTION 模式下最近一次已上报的网格坐标（行,列）。
    /// 移动事件可达数百 Hz，同一 cell 内的重复移动不再向 PTY 发报文
    /// （alacritty 同样按 cell 去重），避免远端重绘回报放大成全帧重排版。
    last_reported_mouse_cell: Option<(Line, Column)>,
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
    /// 上一次 Wakeup 时观察到的焦点上报模式（DECSET ?1004）状态。
    /// 用于检测「应用刚开启焦点上报」的跳变，补发初始 `CSI I`。
    focus_mode_reported: bool,
}

impl Terminal {
    /// scrollback 固定 10000 行（`MAX_SCROLLBACK_LINES` 内）。内存测量
    /// （见 docs/PERF_*.md）表明这是占用与可用性的默认折中；测试与基准
    /// 通过 [`Terminal::new_headless`] 参数化覆盖此值。
    fn default_terminal_config() -> Config {
        Self::terminal_config_with_scrollback(10_000)
    }

    fn terminal_config_with_scrollback(scrollback_lines: usize) -> Config {
        Config {
            scrolling_history: scrollback_lines.min(MAX_SCROLLBACK_LINES),
            default_cursor_style: AlacCursorStyle {
                shape: AlacCursorShape::Block,
                blinking: false,
            },
            osc52: alacritty_terminal::term::Osc52::CopyPaste,
            // Kitty 键盘协议：vte 负责解析 CSI u 模式栈并应答 CSI ? u 查询，
            // 键编码由 mappings/keys.rs 的 kitty 分支负责。
            kitty_keyboard: true,
            ..Config::default()
        }
    }

    /// 测试 / 基准专用：构造无 PTY、无驱动任务的终端实体。
    ///
    /// 内容通过 [`Terminal::handle_remote_data`]（直接驱动 vte 解析器）注入，
    /// 不产生子进程、不启动事件循环，保证基准与内存探针的输入完全确定。
    /// 正式代码路径请勿使用：`sender` 为空意味着所有 PTY 相关操作都是 no-op。
    #[doc(hidden)]
    pub fn new_headless(cx: &mut App, scrollback_lines: usize) -> Entity<Self> {
        let (events_tx, _events_rx) = unbounded();
        let listener = ZedListener(events_tx);
        let default_bounds = TerminalBounds::default();
        let config = Self::terminal_config_with_scrollback(scrollback_lines);
        let term = Arc::new(FairMutex::new(Term::new(config, &default_bounds, listener)));

        cx.new(|_cx| Self {
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
            #[cfg(unix)]
            local_sender: None,
            #[cfg(unix)]
            local_reader: None,
            images: KittyImages::default(),
            kitty_replies: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            kitty_images_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kitty_generation: 0,
            last_kitty_generation: 0,
            kitty_scanner: None,
            last_content: TerminalContent::default(),
            content_generation: 0,
            content_dirty: false,
            dirty_rows: Vec::new(),
            dirty_row_bounds: Vec::new(),
            event_loop_task: None,
            driver_task: None,
            pending_resize: None,
            resize_task: None,
            resize_primed: false,
            vte_processor: Processor::<StdSyncHandler>::new(),
            input_area_start: None,
            shell_fg_observed: None,
            pending_osc: Vec::new(),
            remote_mode_2048: false,
            remote_mode_1016: false,
            local_mode_1016: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            display_scale: 1.0,
            terminal_bounds: default_bounds,
            selection_phase: SelectionPhase::Ended,
            mouse_report_active: false,
            link_hover_active: false,
            last_mouse_position: None,
            last_reported_mouse_cell: None,
            scroll_px: px(0.0),
            scrollbar_drag_offset: Rc::new(Cell::new(None)),
            last_scroll_time: Cell::new(Instant::now()),
            terminal_type: TerminalType::Local,
            remote_input_callback: None,
            remote_resize_callback: None,
            russh_handle: None,
            focused: true,
            focus_mode_reported: false,
        })
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
            // SAFETY（中文）：`getpgid` 传入刚创建的子进程 pid（有效正整数），
            // 无指针参数，失败时返回 -1 并设置 errno，已判 `>=0` 处理；
            // fd 复用竞态理论存在但窗口极小，失败仅降级为 None，不影响终端创建。
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

        // Kitty 图形协议：读线程/扫描器写入，UI 线程渲染。存储在这两条
        // 管线之间共享，因此这里先建好再分别交给扫描器与 `Terminal`。
        let images = KittyImages::default();
        let kitty_replies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kitty_images_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // 模式 1016（SGR-Pixels 鼠标）的本地状态句柄：读线程里的扫描器写，
        // 鼠标报文编码（UI 线程）读。
        let local_mode_1016 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // 本地终端字节管线：
        // - unix：自建读线程（`LocalReader`），字节经应用层扫描后再喂 vte，
        //   这样 APC（Kitty 图形协议）才能被拦截。
        // - windows：conpty 无裸 fd，沿用 alacritty 的 EventLoop。
        #[cfg(unix)]
        let (local_reader, local_notices): (
            Option<crate::terminal::local_pty::LocalReader>,
            Option<async_channel::Receiver<crate::terminal::local_pty::LocalNotice>>,
        ) = {
            // 初始几何由 `LocalReader::spawn` 统一注入（见其
            // `initial_geometry` 参数），这里只需构造扫描器并交出 1016 的
            // 共享状态句柄（鼠标报文在 UI 线程按该状态编码）。
            let scanner = crate::terminal::kitty::scanner::KittyGraphicsScanner::with_storage(
                images.storage(),
                KITTY_IMAGE_LIMIT > 0,
                Box::new(KittyReplySink {
                    replies: kitty_replies.clone(),
                    changed: kitty_images_dirty.clone(),
                }),
            )
            .with_mode_1016_shared(local_mode_1016.clone());
            match crate::terminal::local_pty::LocalReader::spawn(
                pty,
                term.clone(),
                listener,
                Box::new(scanner),
                default_bounds.into(),
            ) {
                Ok(parts) => (Some(parts.reader), Some(parts.notices)),
                Err(e) => {
                    log::error!("Failed to spawn local PTY reader: {e}");
                    return cx.new(|_cx| Self::create_error_terminal());
                }
            }
        };
        #[cfg(not(unix))]
        let (local_reader, local_notices) = {
            let event_loop = match EventLoop::new(term.clone(), listener, pty, true, false) {
                Ok(event_loop) => event_loop,
                Err(e) => {
                    log::error!("Failed to create terminal EventLoop: {e}");
                    return cx.new(|_cx| Self::create_error_terminal());
                }
            };
            let _ = event_loop.spawn();
            (None, None)
        };

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

            // unix：把读线程的通知（重绘 / 子进程退出）泵到 UI 线程。
            // 读线程自身只做 vte 解析，凡需要 `Context` 的副作用都在这里完成。
            let reader_notice_task = local_notices.map(|notices| {
                cx.spawn(async move |this, cx| {
                    while let Ok(notice) = notices.recv().await {
                        let handled = this.update(cx, |terminal: &mut Self, cx| {
                            terminal.handle_local_notice(notice, cx);
                        });
                        if handled.is_err() {
                            break;
                        }
                    }
                })
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
                sender: None,
                local_sender: local_reader.as_ref().map(|r| r.sender()),
                local_reader,
                images,
                kitty_replies,
                kitty_images_dirty,
                kitty_generation: 0,
                last_kitty_generation: 0,
                kitty_scanner: None,
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                driver_task: reader_notice_task,
                pending_resize: None,
                resize_task: None,
                resize_primed: false,
                vte_processor: Processor::<StdSyncHandler>::new(),
                input_area_start: None,
                shell_fg_observed: None,
                pending_osc: Vec::new(),
                remote_mode_2048: false,
                remote_mode_1016: false,
                // 复用上面交给扫描器的那一份：读线程写、UI 线程读，必须是同一个
                // `Arc`，否则鼠标报文永远看不到 1016 被置位。
                local_mode_1016: local_mode_1016.clone(),
                display_scale: 1.0,
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                mouse_report_active: false,
                link_hover_active: false,
                last_mouse_position: None,
                last_reported_mouse_cell: None,
                scroll_px: px(0.0),
                scrollbar_drag_offset: Rc::new(Cell::new(None)),
                last_scroll_time: Cell::new(Instant::now()),
                terminal_type: TerminalType::Local,
                remote_input_callback: None,
                remote_resize_callback: None,
                russh_handle: None,
                focused: true,
                focus_mode_reported: false,
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
            #[cfg(unix)]
            local_sender: None,
            #[cfg(unix)]
            local_reader: None,
            images: KittyImages::default(),
            kitty_replies: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            kitty_images_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kitty_generation: 0,
            last_kitty_generation: 0,
            kitty_scanner: None,
            last_content: TerminalContent::default(),
            content_generation: 0,
            content_dirty: false,
            dirty_rows: Vec::new(),
            dirty_row_bounds: Vec::new(),
            event_loop_task: None,
            driver_task: None,
            pending_resize: None,
            resize_task: None,
            resize_primed: false,
            vte_processor: Processor::<StdSyncHandler>::new(),
            input_area_start: None,
            shell_fg_observed: None,
            pending_osc: Vec::new(),
            remote_mode_2048: false,
            remote_mode_1016: false,
            local_mode_1016: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            display_scale: 1.0,
            terminal_bounds: TerminalBounds::default(),
            selection_phase: SelectionPhase::Ended,
            mouse_report_active: false,
            link_hover_active: false,
            last_mouse_position: None,
            last_reported_mouse_cell: None,
            scroll_px: px(0.0),
            scrollbar_drag_offset: Rc::new(Cell::new(None)),
            last_scroll_time: Cell::new(Instant::now()),
            terminal_type: TerminalType::Local,
            remote_input_callback: None,
            remote_resize_callback: None,
            russh_handle: None,
            focused: true,
            focus_mode_reported: false,
        }
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
        // 有界通道（256 ≈ 8MB 在途上限）：见 DriverListener 文档。
        let (driver_tx, driver_rx) = async_channel::bounded(256);
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
            move |cols, rows, _scale| russh_for_resize.resize(cols, rows),
            Some(russh_for_status),
        )
    }

    fn new_ssh_pty(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        remote_cwd: Option<String>,
    ) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let (_driver_tx, driver_rx) = async_channel::bounded(1);
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
            // 构造期没有窗口、拿不到 DPI，先用 1.0；首帧 `resize` 会带上真实
            // 缩放纠正（与 PTY 初始尺寸走同一条路径）。
            1.0,
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
            move |cols, rows, scale| {
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
                pty_resize.resize(bounds, scale);
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
        driver_rx: async_channel::Receiver<DriverEvent>,
        input_handler: impl Fn(&[u8]) + Send + Sync + 'static,
        resize_handler: impl Fn(u16, u16, f32) + Send + Sync + 'static,
        russh_handle: Option<RusshTerminal>,
    ) -> Entity<Self> {
        let default_bounds = TerminalBounds::default();
        let ssh_config_clone = ssh_config.clone();

        // 远程路径的字节在 UI 线程解析，因此扫描器/共享状态都在这里建立。
        let images = KittyImages::default();
        let kitty_replies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kitty_images_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

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
                let driver_rx = driver_rx;
                // async_channel::Receiver 为 !Unpin，用其原生 recv()（返回 Err 即通道关闭）
                while let Ok(event) = driver_rx.recv().await {
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
                #[cfg(unix)]
                local_sender: None,
                #[cfg(unix)]
                local_reader: None,
                // 复用上面交给扫描器的那三份共享句柄：扫描器（写）与渲染/
                // 应答路径（读）必须是**同一个** `Arc`，否则 SSH 下图像永远
                // 渲染不出来、应答也发不出去。它们在外层创建，此处克隆
                // （`kitty_scanner` 也在本字面量里用它们，故必须 clone）。
                images: images.clone(),
                kitty_replies: kitty_replies.clone(),
                kitty_images_dirty: kitty_images_dirty.clone(),
                kitty_generation: 0,
                last_kitty_generation: 0,
                kitty_scanner: Some(
                    crate::terminal::kitty::scanner::KittyGraphicsScanner::with_storage_and_geometry(
                        images.storage(),
                        KITTY_IMAGE_LIMIT > 0,
                        Box::new(KittyReplySink {
                            replies: kitty_replies.clone(),
                            changed: kitty_images_dirty.clone(),
                        }),
                        default_bounds.into(),
                    ),
                ),
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                event_loop_task: Some(event_loop_task),
                driver_task: Some(driver_task),
                pending_resize: None,
                resize_task: None,
                resize_primed: false,
                vte_processor: Processor::<StdSyncHandler>::new(),
                input_area_start: None,
                shell_fg_observed: None,
                pending_osc: Vec::new(),
                remote_mode_2048: false,
                remote_mode_1016: false,
                local_mode_1016: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                display_scale: 1.0,
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                mouse_report_active: false,
                link_hover_active: false,
                last_mouse_position: None,
                last_reported_mouse_cell: None,
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
                focus_mode_reported: false,
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
                // SAFETY（中文）：`tcgetpgrp` 传入存活的 pty master fd，
                // 失败返回 -1，已判 `>=0`；fd 关闭竞态下仅降级为 false，不扩散。
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
            // SAFETY（中文）：同上，`tcgetpgrp` 只读查询，失败判 `<0` 后返回 None。
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

    /// 按索引解析主题色（用于颜色查询应答的回退）。
    ///
    /// 索引含义与 `Term.colors` 一致：0..=15 基础色，16..=255 色立方 / 灰阶，
    /// 256..=268 命名扩展色（前景 / 背景 / 光标 / 暗 / 亮前景）。
    /// 越界索引返回黑色（与旧行为一致，防御异常应用）。
    fn theme_color_for_index(
        theme: &crate::theme::TerminalTheme,
        index: usize,
    ) -> alacritty_terminal::vte::ansi::Rgb {
        use alacritty_terminal::vte::ansi::NamedColor;
        // vte 未提供 usize -> NamedColor 的转换；按枚举判别值手工映射。
        // 判别值序列：0..=15 基础色，256 前景，257 背景，258 光标，
        // 259..=266 暗色，267 亮前景，268 暗前景。
        const NAMED_ORDER: [NamedColor; 13] = [
            NamedColor::Foreground,
            NamedColor::Background,
            NamedColor::Cursor,
            NamedColor::DimBlack,
            NamedColor::DimRed,
            NamedColor::DimGreen,
            NamedColor::DimYellow,
            NamedColor::DimBlue,
            NamedColor::DimMagenta,
            NamedColor::DimCyan,
            NamedColor::DimWhite,
            NamedColor::BrightForeground,
            NamedColor::DimForeground,
        ];
        let hsla = if index <= 255 {
            theme.indexed_color(index as u8)
        } else {
            let named = NAMED_ORDER
                .get(index - 256)
                .copied()
                .unwrap_or(NamedColor::Foreground);
            theme.named_color(named)
        };
        crate::terminal::mappings::colors::hsla_to_rgb888(hsla)
    }

    /// 把当前主题色灌入 alacritty `Term.colors`（初始化与主题切换时调用）。
    ///
    /// 这是「颜色查询 / 改色闭环」的基座：
    /// - OSC 10/11/12/4 的查询应答读 `Term.colors`，灌入后返回主题色而非黑色；
    /// - 应用改色（OSC 4/10/11/12 SET）写入 `Term.colors`，渲染层通过
    ///   [`TerminalContent::palette`] 快照读到并立即生效；
    /// - 重置（OSC 104/110/111）把槽位清空，渲染回退主题色
    ///   （正是重置语义：回到终端默认色）。
    ///
    /// `set_color` 对非光标槽位自动标记全量 damage；光标色变化由末尾的
    /// 强制刷新兑付。
    pub fn apply_theme_colors(&mut self, theme: &crate::theme::TerminalTheme) {
        let overrides = crate::terminal::mappings::colors::theme_color_overrides(theme);
        {
            let mut term = self.term.lock();
            for (index, rgb) in overrides {
                // vte Handler trait：写入 Term.colors 并标记 damage。
                term.set_color(index, rgb);
            }
        }
        // 光标槽位不自动 damage，这里强制刷新一次内容快照，
        // 保证 palette 立刻反映新主题。
        self.update_last_content(true);
    }

    /// 更新终端焦点状态。
    ///
    /// 应用通过 DECSET `?1004h` 开启焦点上报后，聚焦变化会向 PTY 发送
    /// `CSI I` / `CSI O`，让 vim/helix 等 TUI 在失焦时正确暂停或刷新。
    /// 模式关闭或远端通道不可用时只更新内部标志，不产生任何输出。
    pub fn set_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.report_focus_state();
    }

    /// 若应用已开启焦点上报，按当前聚焦状态发送一次 `CSI I` / `CSI O`。
    fn report_focus_state(&mut self) {
        let mode = {
            let term = self.term.lock();
            term.mode().clone()
        };
        if mode.contains(TermMode::FOCUS_IN_OUT) {
            // write_raw：应用应答通道不应维护输入区锚点。
            let bytes: &[u8] = if self.focused { b"\x1b[I" } else { b"\x1b[O" };
            self.write_raw(bytes);
        }
    }

    /// 判断关闭该终端是否需要用户确认：是否有非 shell 的前台进程仍在运行。
    ///
    /// - 本地（unix）：子进程存活且 TTY 前台进程组不是 shell → 有 TUI/长任务
    ///   在跑，关闭会杀掉它；shell 在提示符时直接关，不阻塞。
    /// - 远程：SSH 仍连接且远端不在 shell 提示符（OSC 133 标记位）时需要确认。
    /// - Windows：暂无前台进程组检测手段，不阻塞（后续可补 Job Object）。
    pub fn needs_close_confirmation(&self) -> bool {
        if self.is_remote() {
            self.is_ssh_connected() && !self.is_shell_in_foreground()
        } else {
            #[cfg(unix)]
            {
                self.pty_master_fd.is_some()
                    && self.shell_process_group_id.is_some()
                    && self.local_child_alive()
                    && !self.is_shell_in_foreground()
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }

    /// 终端承载的进程是否仍在运行。
    ///
    /// - 本地：看 PTY 子进程是否还活着；
    /// - 远程：看 SSH 通道是否还连着；
    /// - Windows：无进程存活探测手段，退化为 true（保守视为「可能还活着」）。
    ///
    /// 用于区分「用户主动关闭一个还在跑的终端」与「shell 已正常 exit 退出」——
    /// 后者不该进 undo 关闭栈，否则 `cmd-shift-t` 会恢复出一个死终端。
    pub fn process_alive(&self) -> bool {
        if self.is_remote() {
            self.is_ssh_connected()
        } else {
            #[cfg(unix)]
            {
                self.local_child_alive()
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    }

    /// 本地 PTY 子进程是否仍存活（`kill(pid, 0)` 探测，不发送信号）。
    #[cfg(unix)]
    fn local_child_alive(&self) -> bool {
        match self.child_process_id {
            Some(pid) => {
                // SAFETY（中文）：`kill(pid, 0)` 仅做存在性探测，信号量为 0
                // 不产生任何实际信号；pid 为创建时捕获的有效子进程 pid。
                unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
            }
            None => false,
        }
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
        // 序列（如 133;B、cwd 更新）在此被拦截，OSC 52 等则透传给 vte；
        // Kitty 图形 APC 同样在此被摘出交给扫描器。
        self.process_pty_data(data, require_local_osc7_host, cx);

        // 扫描器刚产生的协议应答（如 `OK`、隐式 id 回报）回写给远端。
        self.flush_kitty_replies();
        // 远程路径的扫描发生在本线程，图像变更同步是即时的。
        self.sync_kitty_state();

        // 标记内容已更新
        self.content_dirty = true;
        self.content_generation += 1;

        // 远程路径没有 alacritty EventLoop，也不发 AlacTermEvent::Wakeup，
        // 在这里比对焦点上报模式位（本地终端的同类检查在 process_event）。
        self.check_focus_mode_enabled();
    }

    /// 比较焦点上报模式位与上次快照，捕获应用 DECSET ?1004h 的开启跳变，
    /// 按需补发当前焦点状态（xterm 约定：开启时立即上报一次基线）。
    fn check_focus_mode_enabled(&mut self) {
        let focus_mode_now = {
            let term = self.term.lock();
            term.mode().contains(TermMode::FOCUS_IN_OUT)
        };
        if focus_mode_now != self.focus_mode_reported {
            self.focus_mode_reported = focus_mode_now;
            if focus_mode_now {
                self.report_focus_state();
            }
        }
    }

    /// 处理从 SSH 接收的数据
    pub fn process_ssh_data(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.handle_pty_data(data, false, cx);
    }

    pub fn handle_remote_data(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.process_ssh_data(data, cx);
        cx.emit(Event::Wakeup);
    }

    /// 处理本地读线程的通知。
    ///
    /// 读线程只做 vte 解析（不需要 `Context`），凡涉及事件派发/重绘的
    /// 副作用统一回到 UI 线程在这里完成。
    #[cfg(unix)]
    fn handle_local_notice(
        &mut self,
        notice: crate::terminal::local_pty::LocalNotice,
        cx: &mut Context<Self>,
    ) {
        use crate::terminal::local_pty::LocalNotice;
        match notice {
            LocalNotice::Wakeup => {
                // 字节已被读线程解析进 `term`，这里只负责触发重绘。
                // 但协议应答必须由 UI 线程回写（读线程不能写 PTY）。
                self.flush_kitty_replies();
                self.sync_kitty_state();
                self.content_generation += 1;
                self.content_dirty = true;
                cx.emit(Event::Wakeup);
                self.check_focus_mode_enabled();
            }
            LocalNotice::ChildExit(status) => {
                // 退出事件已由读线程通过 `ZedListener` 发出（process_event
                // 会处理 CloseTerminal / ProcessExited）；这里只兜底触发一次重绘，
                // 保证 drain 出来的最后一段输出能上屏。
                self.flush_kitty_replies();
                self.sync_kitty_state();
                self.content_generation += 1;
                self.content_dirty = true;
                cx.emit(Event::Wakeup);
                let _ = status;
            }
            LocalNotice::Error(reason) => {
                log::error!("本地 PTY 读线程错误: {reason}");
            }
        }
    }

    /// 把读线程置位的「图像已变化」标志同步为 `Terminal` 自己的代际号。
    ///
    /// 读线程只 `store(true)`；真正读取存储、决定重绘的时机在 UI 线程，
    /// 这样渲染层不会在读线程刚写入一半时读到中间状态。
    fn sync_kitty_state(&mut self) {
        let changed = self
            .kitty_images_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        if changed {
            self.images.bump_generation();
        }
        self.kitty_generation = self.images.generation();
    }

    /// Windows / headless 占位：本地通知任务仅 unix 存在。
    #[cfg(not(unix))]
    #[allow(dead_code)]
    fn handle_local_notice(&mut self, _cx: &mut Context<Self>) {}

    /// 把一段字节喂给 vte 解析器（作用于 term）。空段直接跳过。
    fn advance_vte(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut term = self.term.lock();
        self.vte_processor.advance(&mut *term, bytes);
    }

    /// 喂给 vte 之前，先按 `removals`（相对**整块** `data` 的偏移）把尺寸序列摘下。
    ///
    /// `slice` 是整块中 `[base, base + slice.len())` 这一段；`removals` 沿用
    /// 整块的坐标系，因此这里用 `base` 换算。
    fn advance_vte_without_size_sequences(
        &mut self,
        slice: &[u8],
        base: usize,
        removals: &[(usize, usize)],
    ) {
        if slice.is_empty() {
            return;
        }
        if removals.is_empty() {
            self.advance_vte(slice);
            return;
        }
        let end = base + slice.len();
        let mut cursor = base;
        for &(start, len) in removals {
            if start >= end {
                break;
            }
            if start + len <= base {
                continue;
            }
            let cut_start = start.max(base);
            let cut_end = (start + len).min(end);
            if cut_start > cursor {
                self.advance_vte(&slice[cursor - base..cut_start - base]);
            }
            cursor = cursor.max(cut_end);
        }
        if cursor < end {
            self.advance_vte(&slice[cursor - base..]);
        }
    }

    /// 摘除并应答远程路径上的尺寸上报类序列。
    ///
    /// 返回需要从字节流中摘除的区间（相对 `data` 的偏移，升序）。
    /// 应答经 `write_raw` 立刻回写——与 `TextAreaSizeRequest` 走同一通道。
    ///
    /// 只处理 vte/alacritty 都不支持的两种：
    /// - `CSI 16 t`：cell 像素尺寸 → `\x1b[6;{cell_height};{cell_width}t`
    /// - `DECSET/DECRST ?2048`：in-band size reports → 置位时立即上报几何
    ///
    /// `CSI 14 t` / `CSI 18 t` **不在此列**：vte 已能正确应答（格式与 ghostty
    /// 一致），在这里再拦一次会造成双重应答。
    fn handle_size_sequences(
        &mut self,
        data: &[u8],
        _cx: &mut Context<Self>,
    ) -> Vec<(usize, usize)> {
        use crate::terminal::escape_scan::{self, SizeReport};

        let mut removals = Vec::new();
        // 上报必须用物理像素：`CSI 16 t` 的 cell 尺寸是终端应用推算宽高比的
        // 唯一依据，给逻辑点会让 Retina 上的应用把 surface 建小一半。
        let geometry = self.terminal_bounds.term_geometry(self.display_scale);

        let mut search_from = 0;
        while let Some((offset, len)) = escape_scan::find_csi_16_t(&data[search_from..]) {
            let start = search_from + offset;
            let report = escape_scan::encode_size_report(SizeReport::CellSize, geometry);
            self.write_raw(report.as_bytes());
            removals.push((start, len));
            search_from = start + len;
        }

        let mut search_from = 0;
        while let Some(hit) = escape_scan::find_private_mode_2048(&data[search_from..]) {
            self.remote_mode_2048 = hit.enabled;
            // 对齐 ghostty：每次置位都重发；复位静默。
            if hit.enabled {
                let report = escape_scan::encode_size_report(SizeReport::Mode2048, geometry);
                self.write_raw(report.as_bytes());
            }
            // 仅单参数序列可整条摘除；多参数时保留原文让 vte 处理其余模式
            // （见 `find_private_mode` 文档）。
            if let Some(r) = hit.removable {
                removals.push(r);
            }
            search_from = search_from + hit.start + hit.len;
        }

        // `DECSET/DECRST ?1016`：SGR-Pixels 鼠标编码。不产生任何应答
        // （ghostty 的鼠标模式同样是纯状态开关），只需摘除以免 vte 把它当
        // 未知模式、进而让后续 `DECRQM` 答 0。
        let mut search_from = 0;
        while let Some(hit) = escape_scan::find_private_mode_1016(&data[search_from..]) {
            self.remote_mode_1016 = hit.enabled;
            if let Some(r) = hit.removable {
                removals.push(r);
            }
            search_from = search_from + hit.start + hit.len;
        }

        // `DECRQM ?1016$p`：状态查询。vte/alacritty 对未知模式答
        // `NotSupported = 0`（`term/mod.rs:2087`），而 ghostty 答
        // 1（复位）/2（置位）。terminal-browser 正是靠这个探测像素级鼠标
        // 是否可用，答 0 会让它回退到单元格命中测试——所以必须自己应答。
        let mut search_from = 0;
        while let Some((offset, len)) =
            escape_scan::find_dec_rqm_private(&data[search_from..], b"1016")
        {
            let start = search_from + offset;
            let state = if self.remote_mode_1016 { 2 } else { 1 };
            self.write_raw(format!("\x1b[?1016;{state}$y").as_bytes());
            removals.push((start, len));
            search_from = start + len;
        }

        removals.sort_unstable();
        removals
    }

    /// 逐段扫描数据块：普通文本段直接喂给 vte，完整 OSC 交由应用层
    /// 判定（消费则丢弃，否则透传给 vte），完整 APC（Kitty 图形协议）
    /// 交给扫描器消费。跨 chunk 未完成的序列存入 `pending_osc`，与下一个
    /// 数据块拼接后重新扫描。
    ///
    /// 快速路径：两种起始符都不含时一次性喂入，避免分段锁开销。
    fn process_pty_data(
        &mut self,
        data: &[u8],
        require_local_osc7_host: bool,
        cx: &mut Context<Self>,
    ) {
        use crate::terminal::escape_scan::{self, Extract};

        // 未完成 OSC 的缓存上限；超过则放弃拼接直接透传，防止异常输出
        // （无终止符的 ESC ]）无限占用内存。
        const MAX_PENDING_OSC: usize = 8 * 1024;
        // 未完成 APC（Kitty 图形）的上限与单条命令负载上限一致：
        // 图形分片单条可达数 MB（terminal-browser 的整帧传输），8 KiB 会
        // 把合法序列透传给 vte 而丢失图像。
        const MAX_PENDING_APC: usize = crate::terminal::kitty::command::MAX_COMMAND_BYTES;

        // 有跨 chunk 残尾时先拼接（拷贝一次，仅在有残尾时发生）。
        let pending = std::mem::take(&mut self.pending_osc);
        let joined: Vec<u8>;
        let data: &[u8] = if pending.is_empty() {
            data
        } else {
            joined = [pending.as_slice(), data].concat();
            joined.as_slice()
        };

        let next_osc = memchr::memmem::find(data, escape_scan::OSC_START);
        let next_apc = memchr::memmem::find(data, escape_scan::APC_START);
        let next_csi = memchr::memmem::find(data, escape_scan::CSI_START);
        if next_osc.is_none() && next_apc.is_none() && next_csi.is_none() {
            self.advance_vte(data);
            return;
        }

        // 尺寸上报类序列（`CSI 16 t` / DECSET 2048）就地摘除并应答。
        //
        // vte 对 `CSI 16 t` 是 `unhandled!()`、alacritty 把 2048 视为未知模式，
        // 因此必须在这一层处理。几何直接取 `self.terminal_bounds`（UI 线程，
        // 恒为最新），无须像本地路径那样经过扫描器缓存。
        let size_removals = self.handle_size_sequences(data, cx);
        if next_osc.is_none() && next_apc.is_none() {
            // 只有尺寸序列：喂入剩余文本即可。
            self.advance_vte_without_size_sequences(data, 0, &size_removals);
            return;
        }

        let mut pos = 0;
        loop {
            // 下一个起始符（OSC / APC 中较早者）。
            let osc_offset = memchr::memmem::find(&data[pos..], escape_scan::OSC_START);
            let apc_offset = memchr::memmem::find(&data[pos..], escape_scan::APC_START);
            let (offset, is_apc) = match (osc_offset, apc_offset) {
                (Some(o), Some(a)) => {
                    if a < o {
                        (a, true)
                    } else {
                        (o, false)
                    }
                }
                (Some(o), None) => (o, false),
                (None, Some(a)) => (a, true),
                (None, None) => break,
            };
            let seq_pos = pos + offset;

            // 先喂入序列之前的普通文本，使光标反映这些内容。
            // 尺寸序列只出现在这些"普通文本"里，逐段摘下。
            self.advance_vte_without_size_sequences(&data[pos..seq_pos], pos, &size_removals);

            if is_apc {
                match escape_scan::extract_apc(&data[seq_pos..]) {
                    Extract::Complete { len, content } => {
                        // 图形 APC 由扫描器消费，不进入 vte（vte 会丢弃内容，
                        // 但序列本身仍走一遍状态机——提前摘除更干净）。
                        self.handle_kitty_apc(content);
                        pos = seq_pos + len;
                    }
                    Extract::Incomplete => {
                        if data.len() - seq_pos > MAX_PENDING_APC {
                            self.advance_vte(&data[seq_pos..]);
                        } else {
                            self.pending_osc = data[seq_pos..].to_vec();
                        }
                        pos = data.len();
                    }
                    Extract::NotStart => pos = seq_pos + escape_scan::APC_START.len(),
                }
                continue;
            }

            match escape_scan::extract_osc(&data[seq_pos..]) {
                Extract::Complete { len, content } => {
                    if !self.try_handle_osc_sequence(cx, content, require_local_osc7_host) {
                        // 不是应用层关心的 OSC，保留给 vte 自己处理。
                        self.advance_vte(&data[seq_pos..seq_pos + len]);
                    }
                    pos = seq_pos + len;
                }
                Extract::Incomplete => {
                    // 序列不完整：若残尾过大则直接透传，否则暂存等下一个 chunk。
                    if data.len() - seq_pos > MAX_PENDING_OSC {
                        self.advance_vte(&data[seq_pos..]);
                    } else {
                        self.pending_osc = data[seq_pos..].to_vec();
                    }
                    pos = data.len();
                }
                Extract::NotStart => pos = seq_pos + escape_scan::OSC_START.len(),
            }
        }

        if pos < data.len() {
            self.advance_vte_without_size_sequences(&data[pos..], pos, &size_removals);
        }
    }

    /// 处理一条完整的 APC 内容（远程路径：扫描在 UI 线程完成）。
    ///
    /// 先同步光标位置——`a=T`/`a=p`/`d=c` 以光标为锚点，而扫描器不与 `term`
    /// 争锁，只能由调用方在解析字节的间隙告知。
    fn handle_kitty_apc(&mut self, content: &[u8]) {
        use crate::terminal::escape_scan;
        if !escape_scan::is_kitty_graphics(content) {
            return;
        }
        let cursor = {
            let term = self.term.lock();
            let point = term.grid().cursor.point;
            (point.line.0, point.column.0 as u32)
        };
        let Some(scanner) = self.kitty_scanner.as_mut() else {
            return;
        };
        scanner.set_cursor(cursor.0, cursor.1);
        scanner.scan_apc(content);
    }

    /// 把扫描器攒下的协议应答回写给 PTY/SSH。
    fn flush_kitty_replies(&mut self) {
        let replies = match self.kitty_replies.lock() {
            Ok(mut buf) if !buf.is_empty() => std::mem::take(&mut *buf),
            _ => return,
        };
        self.write_raw(&replies);
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

        // 桌面通知（应用请求）：
        // - iTerm2 OSC 9;message（单段文本，标题用消息首行或固定为“通知”）
        // - iTerm2/rxvt 扩展 OSC 9;notify;Title;Body
        // - rxvt OSC 777;notify;Title;Body
        if let Some((title, body)) = Self::parse_notification_osc(osc_content) {
            cx.emit(Event::NotificationReceived { title, body });
            return true;
        }

        false
    }

    /// 解析通知类 OSC（9 / 777），返回 (标题, 正文)。
    /// 空内容返回 None，不消费。
    fn parse_notification_osc(osc_content: &[u8]) -> Option<(String, String)> {
        let content = std::str::from_utf8(osc_content).ok()?.trim();

        let (title, body) = if let Some(rest) = content.strip_prefix("9;notify;") {
            // OSC 9;notify;Title;Body
            let mut parts = rest.splitn(2, ';');
            let title = parts.next().unwrap_or("").trim().to_string();
            let body = parts.next().unwrap_or("").trim().to_string();
            (title, body)
        } else if let Some(rest) = content.strip_prefix("777;notify;") {
            // OSC 777;notify;Title;Body
            let mut parts = rest.splitn(2, ';');
            let title = parts.next().unwrap_or("").trim().to_string();
            let body = parts.next().unwrap_or("").trim().to_string();
            (title, body)
        } else if let Some(message) = content.strip_prefix("9;") {
            // OSC 9;message（iTerm2 兼容形态）：首行作标题，其余作正文。
            // 排除 OSC 9;4（进度上报，暂不支持，透传给 vte 忽略），
            // 避免把进度参数误当通知文本。
            if message.trim_start().starts_with("4;") {
                return None;
            }
            let message = message.trim();
            if message.is_empty() {
                return None;
            }
            let mut lines = message.splitn(2, '\n');
            let title = lines.next().unwrap_or("").trim().to_string();
            let body = lines.next().unwrap_or("").trim().to_string();
            (title, body)
        } else {
            return None;
        };

        if title.is_empty() && body.is_empty() {
            return None;
        }
        Some((title, body))
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
            HOSTNAME.get_or_init(hostname_via_syscall)
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
                // 数据刚被 vte 处理：捕获应用 DECSET ?1004h 的开启时机
                // （本地终端的解析发生在 alacritty EventLoop 线程，远程在
                // handle_pty_data；两种路径处理完都会到达这里或那里）。
                self.check_focus_mode_enabled();
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
                self.write_raw(response.as_bytes());
            }
            AlacTermEvent::TextAreaSizeRequest(format) => {
                // 响应窗口大小查询 (CSI t) —— 这里是 `CSI 14 t` 的主路径
                // （alacritty 在 `term/mod.rs:2258` 用闭包算 `num_lines *
                // cell_height` / `num_cols * cell_width`，即 grid 的像素范围）。
                // cell 尺寸同样必须是物理像素，理由同 `handle_size_sequences`。
                let window_size = self.terminal_bounds.window_size(self.display_scale);
                let response = format(window_size);
                self.write_raw(response.as_bytes());
            }
            AlacTermEvent::PtyWrite(text) => {
                self.write_raw(text.as_bytes());
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
                // 响应颜色查询（OSC 10/11/12;? 与 OSC 4;idx;?）：
                // 优先读 `Term.colors`（主题基色已由 apply_theme_colors 灌入，
                // 应用改色写入其中），未覆盖的槽位（如 16..=255 色立方）
                // 回退到当前主题的静态色板。
                let color = {
                    let term = self.term.lock();
                    term.colors()[index].unwrap_or_else(|| {
                        let theme = crate::theme::ThemeState::global(cx).active_theme().terminal;
                        Self::theme_color_for_index(&theme, index)
                    })
                };
                let response = format(color);
                self.write_raw(response.as_bytes());
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

        cells.extend(content.display_iter.map(|ic| {
            // `hyperlink()` is an `Arc` clone (no string allocation); the
            // `id`/`uri` strings are only materialized when grouping ranges
            // for hover hit-testing while the link modifier is held.
            IndexedCell {
                point: ic.point,
                cell: PackedCell::from(ic.cell),
                zerowidth: packed_zerowidth(ic.cell),
                hyperlink: ic.cell.hyperlink(),
            }
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
        // 调色板快照：OSC 4/10/11/12 改色 / 重置与主题切换都在此同步到渲染路径。
        self.last_content.palette =
            crate::terminal::mappings::colors::TerminalPalette::from_colors(term.colors());

        // URL + OSC8 ranges are only consumed by link hover hit-testing.
        // Skip the per-row scans while the hover modifier is not held.
        let rebuild_all_links = self.link_hover_active
            && (force_full_damage
                || self.last_content.url_ranges.len() != row_ranges.len()
                || self.last_content.hyperlink_ranges.len() != row_ranges.len());
        if !self.link_hover_active {
            self.last_content.url_ranges.clear();
            self.last_content.hyperlink_ranges.clear();
        } else if rebuild_all_links {
            self.last_content.url_ranges.clear();
            self.last_content.hyperlink_ranges.clear();
            self.last_content.url_ranges.reserve(row_ranges.len());
            self.last_content.hyperlink_ranges.reserve(row_ranges.len());
            for (start, end) in row_ranges.iter() {
                let row_cells = &cells[*start..*end];
                self.last_content.url_ranges.push(
                    crate::terminal::link_detector::detect_urls_in_line(row_cells),
                );
                self.last_content.hyperlink_ranges.push(
                    crate::terminal::link_detector::detect_hyperlinks_in_line(row_cells),
                );
            }
        } else {
            for (row_idx, dirty) in self.dirty_rows.iter().copied().enumerate() {
                if !dirty {
                    continue;
                }
                let Some((start, end)) = row_ranges.get(row_idx).copied() else {
                    continue;
                };
                if row_idx < self.last_content.url_ranges.len() {
                    self.last_content.url_ranges[row_idx] =
                        crate::terminal::link_detector::detect_urls_in_line(&cells[start..end]);
                }
                if row_idx < self.last_content.hyperlink_ranges.len() {
                    self.last_content.hyperlink_ranges[row_idx] =
                        crate::terminal::link_detector::detect_hyperlinks_in_line(
                            &cells[start..end],
                        );
                }
            }
        }
    }

    pub fn sync(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.sync_inner(_cx);
    }

    /// 测试 / 基准专用：无 Window 的 sync。
    ///
    /// [`Terminal::sync`] 不使用 Window 参数，本入口让内存测量测试
    /// （TestAppContext 无元素绘制）也能驱动完整快照路径。
    #[doc(hidden)]
    pub fn sync_for_test(&mut self, cx: &mut Context<Self>) {
        self.sync_inner(cx);
    }

    fn sync_inner(&mut self, _cx: &mut Context<Self>) {
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

    /// 返回视图行 damage 上界与「图像是否变化」。
    ///
    /// 图像渲染必须知道自上一帧以来图像集合是否变过：网格 damage 只覆盖
    /// 文字，而 Kitty 图像的插入/删除往往不伴随任何文字变化（例：`a=d` 删除
    /// 一条不占格的图像），此时若只看 `dirty_rows` 就不会重绘，屏幕上会残留
    /// 已删除的图像。
    pub fn take_kitty_damage(&mut self) -> bool {
        let changed = self.kitty_generation != self.last_kitty_generation;
        self.last_kitty_generation = self.kitty_generation;
        changed
    }

    /// 只读访问 Kitty 图像存储（渲染层用）。
    ///
    /// 回调形式：持锁期间不要再去取 `term` 锁——本地终端的读线程会长时间
    /// 持有 `term` 并最终申请图像存储锁，反向加锁会形成死锁环路。
    pub fn with_kitty_storage<R>(&self, f: impl FnOnce(&KittyStorage) -> R) -> R {
        self.images.with_storage(f)
    }

    /// 图像存储的共享句柄。
    ///
    /// 渲染层需要「先释放对 `Terminal` 实体的借用，再自行加锁」——因为
    /// 纹理创建要走 GPUI 的 `App`，而带着 `Entity` 借用去调 GPUI 会造成
    /// 借位冲突。拿到 `Arc` 后调用方可以脱开实体自行 `lock`。
    pub fn images_handle(&self) -> std::sync::Arc<std::sync::Mutex<KittyStorage>> {
        self.images.storage()
    }

    /// 当前图像集合代际（测试/诊断用）。
    pub fn kitty_generation(&self) -> u64 {
        self.kitty_generation
    }

    /// 推进所有正在播放的 Kitty 动画帧，返回距下一次换帧的毫秒数。
    ///
    /// 渲染层每构建一帧调用一次（`now_ms` 用与其它调用同一单调时钟），并按
    /// 返回值安排一次唤醒——这就是动画唯一的驱动来源，不存在后台线程。
    /// 换帧会 bump 图像代际，因此这里同步 `kitty_generation`，让
    /// [`Terminal::take_kitty_damage`] 在下次 prepaint 认出"需要重绘"。
    pub fn animation_tick(&mut self, now_ms: u64) -> Option<u64> {
        let delay = self.images.with_storage_mut(|storage| {
            crate::terminal::kitty::render::animation_tick(storage, now_ms)
        });
        // 换帧等价于"图像集合变了"：代际一变，渲染层就会失效纹理并全量重绘。
        self.sync_kitty_state();
        delay
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

        #[cfg(unix)]
        if let Some(ref sender) = self.local_sender {
            sender.input(data.to_vec());
            return;
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

    /// 应用程序回写的原始通道（终端查询应答 / 程序注入），**不维护输入区锚点**。
    ///
    /// `write` 是用户输入路径：含 CRLF 时失效锚点（提示符重绘）是其语义的一部分。
    /// 但 ClipboardLoad / PtyWrite / TextAreaSizeRequest / ColorRequest 这类
    /// 应答如果也走 `write`，应答文本中恰含 `\r`（如剪贴板回写任意文本）就会
    /// 误清锚点并把锚点重捕到程序输出末尾，使 `input_line_blank` 在非空行上
    /// 误报 true，导致 `/` 前缀被错误拦截进内建命令。
    pub fn write_raw(&mut self, data: &[u8]) {
        #[cfg(unix)]
        if let Some(ref sender) = self.local_sender {
            sender.input(data.to_vec());
            return;
        }
        if let Some(ref sender) = self.sender {
            let msg = Msg::Input(Cow::Owned(data.to_vec()));
            let _ = sender.send(msg);
            return;
        }
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
        // 本地终端（unix）：通知读线程退出，释放 PTY / 回收 shell 子进程。
        #[cfg(unix)]
        if let Some(sender) = &self.local_sender {
            sender.shutdown();
        }
        // 本地终端（windows）：通知底层事件循环关闭（会回收 shell 子进程）
        if let Some(sender) = &self.sender {
            let _ = sender.send(Msg::Shutdown);
        }
        // russh 远程：显式关闭 SSH 连接
        if let Some(russh) = &self.russh_handle {
            russh.close();
        }
    }

    /// `scale` 是窗口所在显示的缩放（`Window::scale_factor()`）。它只影响
    /// 尺寸上报与 PTY 的 `ws_xpixel`，不影响网格行列数或布局——`bounds`
    /// 自身恒为逻辑点。
    pub fn resize(&mut self, bounds: TerminalBounds, scale: f32, cx: &mut Context<Self>) {
        let cols = bounds.num_columns();
        let rows = bounds.num_lines();

        // Guard against invalid resize dimensions
        if cols == 0 || rows == 0 {
            log::warn!("Skipping resize with invalid dimensions: {}x{}", cols, rows);
            return;
        }

        // 对应 ghostty `Surface.resize`：主线程立即更新像素几何（原点），
        // 网格与 PTY 走合并通道，渲染层在两次应用之间始终用「已应用」尺寸。
        self.terminal_bounds.bounds.origin = bounds.bounds.origin;

        // 必须在下面的早退之前更新：跨显示器拖动窗口时网格行列数可能一点
        // 没变、只有 DPI 变了，早退会把这个更新吞掉，读线程就会一直拿着旧
        // 几何上报（Retina ↔ 外接屏来回拖就会出现分辨率跳错）。
        let scale = normalize_scale(scale);
        let scale_changed = scale != self.display_scale;
        self.display_scale = scale;

        // 对应 ghostty `Terminal.resize` 的 cols/rows 未变早退。
        let applied_cols = self.terminal_bounds.num_columns();
        let applied_rows = self.terminal_bounds.num_lines();
        if cols == applied_cols && rows == applied_rows && !scale_changed {
            // 尺寸已与应用一致（可能拖回了旧尺寸）：撤销待应用的中间值。
            self.pending_resize = None;
            self.resize_task = None;
            return;
        }
        if cols == applied_cols && rows == applied_rows {
            // 只有 DPI 变化：没有网格 reflow 可合并，但 PTY 与扫描器必须
            // 立刻拿到新几何，所以绕过 25ms 合并定时器直接应用。
            self.pending_resize = Some(bounds);
            self.apply_pending_resize(false, cx);
            return;
        }

        // 对应 ghostty `Surface.resize` → IO mailbox `.resize`（最新值覆盖）。
        self.pending_resize = Some(bounds);

        // deepterm 特有：PTY 以默认 55×18 启动，首帧需立即校正
        // （ghostty spawn 时就用 surface 实际尺寸创建 PTY，无此问题）。
        if !self.resize_primed {
            self.resize_primed = true;
            self.apply_pending_resize(false, cx);
            return;
        }

        // deepterm 特有：headless / 错误终端没有子进程可同步，立即应用以保持
        // 测试与基准确定性（ghostty 无此形态）。
        // 本地终端：读线程 / EventLoop 都没有子进程可同步时（headless、错误终端），
        // 立即应用以保持测试与基准确定性。
        #[cfg(unix)]
        let has_local_sink = self.local_sender.is_some() || self.remote_resize_callback.is_some();
        #[cfg(not(unix))]
        let has_local_sink = self.sender.is_some() || self.remote_resize_callback.is_some();
        if !has_local_sink {
            self.apply_pending_resize(false, cx);
            return;
        }

        // 1:1 对应 ghostty `termio/Thread.handleResize`：合并定时器活动期间
        // 只更新最新值、不重置定时器 —— 25ms 节流，持续拖动期间稳定 ~40Hz。
        if self.resize_task.is_some() {
            return;
        }
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_COALESCE_DELAY).await;
            let _ = this.update(cx, |terminal, cx| {
                terminal.resize_task = None;
                terminal.apply_pending_resize(true, cx);
            });
        }));
    }

    /// 1:1 对应 ghostty `Termio.resize`：pty、网格、渲染尺寸三者一次到位。
    ///
    /// 顺序与 ghostty 相反（先网格后 PTY），原因：ghostty 的 pty resize 与
    /// 网格 reflow 在同一线程顺序执行，子进程的重绘输出必然落在 reflow 之后；
    /// deepterm 的网格在主线程、输出解析在 IO 线程，若先发 SIGWINCH，子进程
    /// 的快速响应可能在网格 reflow 前被解析进旧网格。先 reflow 可保证新尺寸
    /// 的输出永远落在新网格上。
    fn apply_pending_resize(&mut self, wake: bool, cx: &mut Context<Self>) {
        self.resize_task = None;

        let Some(bounds) = self.pending_resize.take() else {
            return;
        };
        let cols = bounds.num_columns();
        let rows = bounds.num_lines();
        if cols == 0 || rows == 0 {
            return;
        }
        let applied_cols = self.terminal_bounds.num_columns();
        let applied_rows = self.terminal_bounds.num_lines();

        // 全量应用新 bounds（尺寸 + 度量 + 原点）—— 渲染层由此拿到新尺寸。
        self.terminal_bounds = bounds;
        let grid_changed = cols != applied_cols || rows != applied_rows;
        if grid_changed {
            {
                let mut term = self.term.lock();
                term.resize(alacritty_terminal::term::test::TermSize::new(cols, rows));
            }
            // 对应 ghostty `Terminal.resize` 尾部的 `flags.dirty.clear = true`：
            // resize 后强制渲染层全量重绘，行缓存不留任何旧尺寸的残留。
            self.input_area_start = None;
            self.update_last_content(true);
        }

        // 注意：不能因 cols/rows 未变就跳过下面的通知。跨显示器拖动只改 DPI、
        // 不改网格，此时 PTY 的 `ws_xpixel` 与读线程扫描器的几何都必须刷新。
        //
        // 本地终端：通过 sender 发送 resize 消息（cell 尺寸已按显示缩放
        // 换算成物理像素，读线程据此填扫描器几何）。
        #[cfg(unix)]
        if let Some(ref sender) = self.local_sender {
            sender.resize(bounds.window_size(self.display_scale));
        }
        if let Some(ref sender) = self.sender {
            let _ = sender.send(Msg::Resize(bounds.window_size(self.display_scale)));
        }
        // 远程终端：通过回调通知 SSH 通道调整窗口大小（带上缩放，理由见回调类型）
        if let Some(ref callback) = self.remote_resize_callback {
            callback(cols as u16, rows as u16, self.display_scale);
        }
        // 对应 ghostty `renderer_mailbox.push(.resize)` + `renderer_wakeup`。
        if wake {
            cx.emit(Event::Wakeup);
        }
    }

    /// Enable or disable lazy link-range maintenance (URL regex + OSC8).
    ///
    /// While enabled (link-hover modifier held), `update_last_content` keeps
    /// `last_content.url_ranges` / `hyperlink_ranges` fresh for dirty rows.
    /// While disabled, all link work is skipped entirely: the ranges are only
    /// consumed by hover hit-testing, so maintaining them on every output
    /// frame is wasted scanning and allocation churn.
    ///
    /// Turning the flag on rebuilds all rows immediately from the cached
    /// content snapshot — no term lock is needed because `last_content` is
    /// owned by `Self` (hyperlinks are preserved per-cell, see `IndexedCell`).
    pub fn set_link_hover_active(&mut self, active: bool) {
        if self.link_hover_active == active {
            return;
        }
        self.link_hover_active = active;
        if active {
            let TerminalContent {
                cells,
                row_ranges,
                url_ranges,
                hyperlink_ranges,
                ..
            } = &mut self.last_content;
            url_ranges.clear();
            hyperlink_ranges.clear();
            url_ranges.reserve(row_ranges.len());
            hyperlink_ranges.reserve(row_ranges.len());
            for (start, end) in row_ranges.iter() {
                let row_cells = &cells[*start..*end];
                url_ranges.push(crate::terminal::link_detector::detect_urls_in_line(
                    row_cells,
                ));
                hyperlink_ranges.push(crate::terminal::link_detector::detect_hyperlinks_in_line(
                    row_cells,
                ));
            }
        } else {
            self.last_content.url_ranges.clear();
            self.last_content.hyperlink_ranges.clear();
        }
    }

    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    /// 收集视口里的 Unicode 占位符单元格（Kitty 图形协议 `U=1`）。
    ///
    /// 只挑 `U+10EEEE` 的单元格——绝大多数帧一个都没有，所以这里的成本就是
    /// 一遍字符比较。结果按 `(行, 列)` 升序：`last_content.cells` 本身就是按
    /// 可见行、行内列序排列的，顺序扫一遍即可。
    ///
    /// 为什么不直接让渲染层读 `last_content()`：那里需要同时持有终端实体的
    /// 借用和 kitty 存储锁，而两者都要参与 `plan_placeholders`。这里先拍平成
    /// 一份不借用终端的数据，渲染层拿去用就行。
    pub fn kitty_placeholder_cells(&self) -> Vec<crate::terminal::kitty::render::PlaceholderCell> {
        use crate::terminal::kitty::unicode::PLACEHOLDER;

        let mut out = Vec::new();
        for indexed in &self.last_content.cells {
            if indexed.cell.c != PLACEHOLDER {
                continue;
            }
            out.push(crate::terminal::kitty::render::PlaceholderCell {
                row: indexed.point.line.0,
                col: indexed.point.column.0 as u32,
                fg: indexed.cell.fg,
                underline_color: indexed.cell.underline_color,
                zerowidth: indexed.zerowidth,
            });
        }
        out
    }

    /// Find a URL at the given viewport position.
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

    /// Find an OSC8 hyperlink at the given viewport position.
    /// Returns (uri, id, start_col, end_col) if found. Highest hit-test
    /// priority: explicit links beat regex-guessed URLs/paths.
    pub fn hyperlink_range_at_viewport_position(
        &self,
        viewport_row: usize,
        col: usize,
    ) -> Option<(String, String, usize, usize)> {
        let content = &self.last_content;
        if viewport_row >= content.hyperlink_ranges.len() {
            return None;
        }
        for range in &content.hyperlink_ranges[viewport_row] {
            if col >= range.start_col && col < range.end_col {
                return Some((
                    range.uri.clone(),
                    range.id.clone(),
                    range.start_col,
                    range.end_col,
                ));
            }
        }
        None
    }

    /// Whether the viewport row is soft-wrapped onto the next row.
    pub fn viewport_row_wrapped(&self, viewport_row: usize) -> bool {
        let content = &self.last_content;
        let Some((start, end)) = content.row_ranges.get(viewport_row).copied() else {
            return false;
        };
        if start >= end {
            return false;
        }
        content.cells[start..end].iter().any(|c| {
            c.cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WRAPLINE)
        })
    }

    /// File-path candidate under a viewport position (for `Cmd+Click`).
    /// Handles soft-wrapped rows by joining the continuation row, but only
    /// returns hits overlapping the *clicked* row so hover never highlights
    /// the next row's text.
    pub fn file_hit_at_viewport_position(
        &self,
        viewport_row: usize,
        col: usize,
    ) -> Option<FileHit> {
        use crate::terminal::file_link_detector::{detect_files_in_line, file_hit_at_col};
        let content = &self.last_content;
        let (start, end) = content.row_ranges.get(viewport_row).copied()?;
        let row_cells = &content.cells[start..end];
        // Fast path: no wrap continuation.
        if !self.viewport_row_wrapped(viewport_row) || viewport_row + 1 >= content.row_ranges.len()
        {
            let hits = detect_files_in_line(row_cells);
            return file_hit_at_col(&hits, col).cloned();
        }
        // Wrapped: join current + next row text; column mapping only covers
        // the first row (detect_files_in_joined_text filters by byte len).
        let (nstart, nend) = content.row_ranges.get(viewport_row + 1).copied()?;
        let mut joined = String::new();
        for cell in row_cells.iter().chain(content.cells[nstart..nend].iter()) {
            let ch = cell.cell.c;
            if ch == '\0' {
                continue;
            }
            joined.push(ch);
        }
        let mut first_text = String::new();
        let mut first_map: Vec<usize> = Vec::new();
        for cell in row_cells {
            let ch = cell.cell.c;
            if ch == '\0' {
                continue;
            }
            let c = cell.point.column.0;
            for _ in 0..ch.len_utf8() {
                first_map.push(c);
            }
            first_text.push(ch);
        }
        let first_row_bytes = first_text.len();
        let hits = detect_files_in_joined_text(&joined, &first_map, first_row_bytes);
        file_hit_at_col(&hits, col).cloned()
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

    // Mouse handling
    pub fn mouse_mode(&self, shift: bool) -> bool {
        if shift {
            return false;
        }
        let term = self.term.lock();
        let mode = term.mode();
        mode.intersects(TermMode::MOUSE_MODE) && !mode.contains(TermMode::VI)
    }

    /// SGR-Pixels 鼠标编码（DECSET 1016）当前是否生效。
    ///
    /// 该模式的开关状态无法从 `TermMode` 读出（vte/alacritty 不认识 1016），
    /// 因此按路径分别取：
    /// - 本地终端：读线程里的扫描器写入 `local_mode_1016` 共享原子量；
    /// - 远程终端：UI 线程在 `process_pty_data` 里写入 `remote_mode_1016`。
    ///
    /// 两条路径只会有一条是活跃的，因此本地标志为假时回退到远程字段即可，
    /// 无需额外的分支判断。
    fn sgr_pixels_active(&self) -> bool {
        self.local_mode_1016
            .load(std::sync::atomic::Ordering::Relaxed)
            || self.remote_mode_1016
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
            if let Some(bytes) = mouse_button_report(
                point,
                position,
                e.button,
                e.modifiers,
                true,
                self.last_content.mode,
                self.sgr_pixels_active(),
                self.display_scale,
            ) {
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
            if let Some(bytes) = mouse_button_report(
                point,
                position,
                e.button,
                e.modifiers,
                false,
                self.last_content.mode,
                self.sgr_pixels_active(),
                self.display_scale,
            ) {
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

            // 按网格坐标去重：MOUSE_MOTION 模式下移动事件可达数百 Hz，
            // 每个（行,列）只发一次报文（alacritty 同样按 cell 去重），
            // 否则远端应用的重绘回报会成倍放大为全帧重排版。
            //
            // **SGR-Pixels 例外**：该模式下坐标精度是像素，同一单元格内的
            // 移动也必须上报，否则应用只能拿到单元格粒度的位置。ghostty
            // 在 `mouse_encode.zig:108` 显式豁免了 `.sgr_pixels`。因此这种
            // 情况下跳过去重、也不更新 `last_reported_mouse_cell`。
            if !self.sgr_pixels_active() {
                if self.last_reported_mouse_cell == Some((point.line, point.column)) {
                    return;
                }
                self.last_reported_mouse_cell = Some((point.line, point.column));
            }

            if let Some(bytes) = mouse_moved_report(
                point,
                position,
                e.pressed_button,
                e.modifiers,
                self.last_content.mode,
                self.sgr_pixels_active(),
                self.display_scale,
            ) {
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
                let position = e.position - self.terminal_bounds.bounds.origin;
                let point = grid_point(
                    position,
                    self.terminal_bounds,
                    self.last_content.display_offset,
                );

                if let Some(scrolls) = scroll_report(
                    point,
                    position,
                    scroll_lines,
                    e,
                    self.last_content.mode,
                    self.sgr_pixels_active(),
                    self.display_scale,
                ) {
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
    /// 所有滚动入口的公共尾部：滚动 + 置脏 + 记录时间。
    fn scroll_by(&mut self, scroll: AlacScroll) {
        let mut term = self.term.lock();
        term.scroll_display(scroll);
        drop(term);
        self.content_dirty = true;
        self.last_scroll_time.set(Instant::now());
    }

    pub fn scroll_line_up(&mut self) {
        self.scroll_by(AlacScroll::Delta(1));
    }

    pub fn scroll_line_down(&mut self) {
        self.scroll_by(AlacScroll::Delta(-1));
    }

    pub fn scroll_page_up(&mut self) {
        let lines = self.term.lock().screen_lines() as i32;
        self.scroll_by(AlacScroll::Delta(lines));
    }

    pub fn scroll_page_down(&mut self) {
        let lines = self.term.lock().screen_lines() as i32;
        self.scroll_by(AlacScroll::Delta(-lines));
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_by(AlacScroll::Top);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_by(AlacScroll::Bottom);
    }

    /// Set the display offset directly (for scrollbar dragging)
    pub fn set_display_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        let current_offset = self.last_content.display_offset;
        let delta = offset as i32 - current_offset as i32;
        if delta != 0 {
            self.scroll_by(AlacScroll::Delta(delta));
            cx.emit(Event::Wakeup);
        }
    }

    // ── Scrollback search ─────────────────────────────────────────────────

    /// 在全部滚动区（scrollback + 屏幕）内搜索 needle（**同步，阻塞调用线程**）。
    ///
    /// 大滚动区（数千行）下的全量扫描较重，UI 路径请改用
    /// [`Self::search_snapshot`] + [`search_matches_in_snapshot`] 把比对放到后台线程。
    /// 本方法保留给测试与一次性同步查询。
    ///
    /// 点坐标为 grid 绝对坐标（负 Line 表示 scrollback），空 / 纯空白 needle 返回空集。
    pub fn search_matches(&self, needle: &str) -> Vec<SearchMatch> {
        let snapshot = self.search_snapshot();
        search_matches_in_snapshot(&snapshot, needle)
    }

    /// 抓取当前 grid 的搜索快照（持 term 锁，成本为 O(可见字符数) 的一次拷贝）。
    ///
    /// 快照与 grid 解耦后可安全 move 到后台线程做比对，避免逐键全量扫描卡住 UI。
    pub fn search_snapshot(&self) -> SearchSnapshot {
        let term = self.term.lock();
        grid_search_snapshot(&term)
    }

    /// 把滚动位置调整到使 grid 行 `line` 尽量居中显示。
    ///
    /// `line` 为 grid 绝对坐标（屏幕顶行 = Line(0)，scrollback 为负）。
    /// 返回调整是否发生（供调用方决定是否重绘）。
    pub fn scroll_line_to_center(&mut self, line: Line) -> bool {
        let (viewport_rows, current_offset, history_size) = {
            let term = self.term.lock();
            let grid = term.grid();
            (
                grid.screen_lines(),
                grid.display_offset(),
                grid.history_size(),
            )
        };
        let target = scroll_offset_to_center(viewport_rows, history_size, line);
        if target == current_offset {
            return false;
        }
        self.scroll_by(AlacScroll::Delta(target as i32 - current_offset as i32));
        true
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
            self.write(&[crate::terminal::CTRL_V]);
            return;
        }

        // Otherwise, paste text content with bracketed paste mode support
        if let Some(text) = clipboard.text() {
            let term = self.term.lock();
            let bracketed = term.mode().contains(TermMode::BRACKETED_PASTE);
            drop(term);

            let sanitized = sanitize_pasted_text(&text, bracketed);

            if bracketed {
                // bracketed paste：单次写入，避免 start/payload/end 被拆成
                // 多个 PTY 写入块（中间可能被其他输出插入、破坏粘贴语义）
                let mut data = Vec::with_capacity(sanitized.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(sanitized.as_bytes());
                data.extend_from_slice(b"\x1b[201~");
                self.write(&data);
            } else {
                // Normal mode
                self.write(sanitized.as_bytes());
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

/// 粘贴内容清洗，防止"复制网页上的恶意文本 → 粘贴即执行"类攻击。
///
/// - 非 bracketed 模式：剪贴板内容会被 shell/编辑器当成键盘输入处理，
///   ESC 与控制字符可注入转义序列、Ctrl-C 等，因此除 `\n` `\t` 外的
///   C0/C1/DEL 全部剔除，并把 CR 归一为 LF（避免多行文本被逐行立即执行）。
/// - bracketed 模式：应用已知这是粘贴内容，保留其余控制字符；
///   但仍要剔除粘贴定界符 `ESC[200~` / `ESC[201~`，否则内容里的
///   `ESC[201~` 会提前结束粘贴模式，后续文本重新变回键盘输入被执行。
pub fn sanitize_pasted_text(text: &str, bracketed: bool) -> String {
    if bracketed {
        // 只剔除定界符序列，其余内容原样保留
        return text.replace("\u{1b}[200~", "").replace("\u{1b}[201~", "");
    }

    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // CRLF 只保留一个 LF，否则每行之间会多出一个空行
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.push('\n'),
            '\n' | '\t' => out.push(ch),
            // C0 控制字符（含 ESC）与 DEL 全部丢弃
            c if c < ' ' || c == '\u{7f}' => {}
            // C1 控制字符丢弃
            c if ('\u{80}'..='\u{9f}').contains(&c) => {}
            c => out.push(c),
        }
    }
    out
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

/// 单行的搜索快照：紧凑字符流 + 每个字符的列号与占宽。
///
/// 与 grid 解耦后可整体 move 到后台线程，比对过程无需再持 `FairMutex<Term>`。
#[derive(Clone, Debug, Default)]
pub struct LineSnapshot {
    /// 剔除宽字符占位格与行尾 null 后的字符流。
    pub text: String,
    /// `text` 中每个字符对应的 grid 列号（下标与字符一一对应）。
    pub cols: Vec<u32>,
    /// 每个字符占的 cell 宽（1 或 2）。
    pub widths: Vec<u8>,
}

/// 整个滚动区（scrollback + 屏幕）的搜索快照。
#[derive(Clone, Debug, Default)]
pub struct SearchSnapshot {
    /// `lines[0]` 对应的 grid 行号（最旧一行，通常为负）。
    pub topmost_line: i32,
    pub lines: Vec<LineSnapshot>,
}

/// 从 grid 抓取搜索快照（调用方需已持有 term 锁）。
///
/// 两类格子跳过：
/// - 行尾 null cell（稀疏行）：不算内容
/// - 宽字符占位格（WIDE_CHAR_SPACER / LEADING_WIDE_CHAR_SPACER）：
///   alacritty 把 CJK 的第二格存成空格占位，检索必须剔除，
///   否则"你好"会被拆成 你/空格/好 而永远无法命中。
///
/// 占宽用于命中尾部 end_col 的计算（CJK 宽字符占 2 格）。
pub fn grid_search_snapshot<T: EventListener>(term: &Term<T>) -> SearchSnapshot {
    let grid = term.grid();
    let total = grid.total_lines();
    let cols = grid.columns();
    let topmost = grid.topmost_line();

    let mut lines = Vec::with_capacity(total);
    for i in 0..total {
        let line = Line(topmost.0 + i as i32);
        let row = &grid[line];

        let mut text = String::with_capacity(cols);
        let mut cols_of = Vec::with_capacity(cols);
        let mut widths_of = Vec::with_capacity(cols);
        for col in 0..cols {
            let cell = &row[Column(col)];
            let spacer_flags = alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER
                | alacritty_terminal::term::cell::Flags::LEADING_WIDE_CHAR_SPACER;
            if cell.c == '\0' || cell.flags.intersects(spacer_flags) {
                continue;
            }
            let wide = if cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR)
            {
                2
            } else {
                1
            };
            text.push(cell.c);
            cols_of.push(col as u32);
            widths_of.push(wide);
        }
        lines.push(LineSnapshot {
            text,
            cols: cols_of,
            widths: widths_of,
        });
    }

    SearchSnapshot {
        topmost_line: topmost.0,
        lines,
    }
}

/// 在快照中搜索 `needle`（大小写折叠），返回所有命中的行内区间。
///
/// 纯CPU、无锁、可在后台线程执行。坐标规则与 grid 一致：屏幕顶行 = Line(0)，
/// scrollback 为负；`end` 列为开区间（下一字符列号或尾字符列+占宽）。
/// 每行内重叠命中只取首个（不重复计数同一子串）。
pub fn search_matches_in_snapshot(snapshot: &SearchSnapshot, needle: &str) -> Vec<SearchMatch> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Vec::new();
    }
    // 大小写折叠：每个字符取首个小写字母。
    // （对多字符折叠的个别 Unicode 情形不完美，可接受。）
    let needle_folded: Vec<char> = fold_chars(needle.chars());
    let window = needle_folded.len();
    if window == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for (row_index, line) in snapshot.lines.iter().enumerate() {
        // 大小写折叠是逐字符 1:1 映射，折叠后长度与原文一致，直接比窗口即可。
        let folded = fold_chars(line.text.chars());
        if folded.len() < window {
            continue;
        }

        let grid_line = Line(snapshot.topmost_line + row_index as i32);
        let mut start = 0usize;
        while start + window <= folded.len() {
            if folded[start..start + window] == needle_folded[..] {
                let start_col = line.cols[start] as usize;
                // 结束列：优先用下一个字符的列号（稀疏行会自然跨过空档），
                // 行尾则用命中尾字符的列 + 占宽。
                let last = start + window - 1;
                let end_col = line
                    .cols
                    .get(last + 1)
                    .map(|c| *c as usize)
                    .unwrap_or(line.cols[last] as usize + line.widths[last] as usize);
                out.push(SearchMatch {
                    start: AlacPoint::new(grid_line, Column(start_col)),
                    end: AlacPoint::new(grid_line, Column(end_col)),
                });
                // 重叠匹配跳过，避免同一位置重复命中。
                start += window;
            } else {
                start += 1;
            }
        }
    }
    out
}

/// 逐字符大小写折叠（取首个小写字母）。
#[inline]
fn fold_chars(iter: impl Iterator<Item = char>) -> Vec<char> {
    iter.map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

/// 在 grid 中搜索 `needle`（大小写折叠），返回所有命中的行内区间。
///
/// 供测试与同步路径使用；UI 路径请用 [`grid_search_snapshot`] +
/// [`search_matches_in_snapshot`] 以避开主线程的全量扫描。
pub fn search_matches_in_grid<T: EventListener>(term: &Term<T>, needle: &str) -> Vec<SearchMatch> {
    let snapshot = grid_search_snapshot(term);
    search_matches_in_snapshot(&snapshot, needle)
}

/// 计算「使 grid 行 `line` 居中显示」所需的 display_offset。
///
/// 纯函数（供 [`Terminal::scroll_line_to_center`] 与测试复用）：
/// 屏幕行索引 = line.0 + display_offset，居中即让该索引落在 rows/2。
/// 结果 clamp 到 [0, history]，0 = 正常视图、history = 滚到最底 scrollback。
pub fn scroll_offset_to_center(viewport_rows: usize, history_size: usize, line: Line) -> usize {
    if viewport_rows == 0 {
        return 0;
    }
    let target = (viewport_rows / 2) as i32 - line.0;
    target.clamp(0, history_size as i32) as usize
}

#[cfg(test)]
mod tests {
    use super::{
        IndexedCell, PackedCell, RESIZE_COALESCE_DELAY, Terminal, TerminalBounds, TerminalContent,
        TerminalType, packed_zerowidth,
    };
    use alacritty_terminal::term::test::mock_term;
    use alacritty_terminal::vte::ansi::Handler;
    use gpui::{AppContext as _, Bounds, Entity, TestAppContext, point, px, size};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn search_finds_matches_across_scrollback_case_folded() {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::test::TermSize;

        // 4 行屏、10k 回滚：写入两行后 linefeed 两次，制造 scrollback。
        let size = TermSize::new(20, 4);
        let mut term = alacritty_terminal::term::Term::new(
            alacritty_terminal::term::Config::default(),
            &size,
            alacritty_terminal::event::VoidListener,
        );
        term.goto(0, 0);
        for c in "hello".chars() {
            term.input(c);
        }
        term.linefeed();
        for c in "World".chars() {
            term.input(c);
        }
        // 光标移到屏底再 linefeed：两行内容整体进 scrollback（hello → Line(-1)）
        term.goto(3, 0);
        term.linefeed();

        let matches = super::search_matches_in_grid(&term, "HELLO");
        assert_eq!(matches.len(), 1, "大小写折叠应命中 hello");
        let m = &matches[0];
        assert_eq!(m.start.line, Line(-1), "应命中 scrollback 里的 hello");
        assert_eq!(m.start.column, Column(0));
        // end 为开区间：命中尾字符列 + 1
        assert_eq!(m.end.column, Column(5));

        // 多行命中："l" 在两行都有
        let l_matches = super::search_matches_in_grid(&term, "l");
        assert_eq!(l_matches.len(), 3, "hello 两个 l + World 一个 l");
    }

    #[test]
    fn search_finds_cjk_text() {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::test::TermSize;

        // 4 行屏：写入中文 + 英文混合行，验证宽字符检索。
        let size = TermSize::new(20, 4);
        let mut term = alacritty_terminal::term::Term::new(
            alacritty_terminal::term::Config::default(),
            &size,
            alacritty_terminal::event::VoidListener,
        );
        term.goto(0, 0);
        for c in "你好 deepterm 世界".chars() {
            term.input(c);
        }

        let matches = super::search_matches_in_grid(&term, "你好");
        assert_eq!(matches.len(), 1, "应命中中文词");
        let m = &matches[0];
        assert_eq!(m.start.line, Line(0));
        assert_eq!(m.start.column, Column(0));
        // 宽字符占 2 格：end 应为列 4（开区间，尾字符列 0+占宽 2、第二个字列 2+占宽 2）
        assert_eq!(m.end.column, Column(4));

        // 单个中文字 + 英文词
        assert_eq!(super::search_matches_in_grid(&term, "世界").len(), 1);
        assert_eq!(super::search_matches_in_grid(&term, "deepterm").len(), 1);
    }

    #[test]
    fn search_skips_empty_and_overlapping_hits() {
        let term = mock_term("aaaa");
        // "aa" 应命中 2 次（重叠跳过），而不是 3 次（滑动窗口）
        let matches = super::search_matches_in_grid(&term, "aa");
        assert_eq!(matches.len(), 2);
        assert_eq!(
            matches[0].start.column,
            alacritty_terminal::index::Column(0)
        );
        assert_eq!(
            matches[1].start.column,
            alacritty_terminal::index::Column(2)
        );
        // 空串 / 纯空白无命中
        assert!(super::search_matches_in_grid(&term, "").is_empty());
        assert!(super::search_matches_in_grid(&term, "   ").is_empty());
    }

    #[test]
    fn search_snapshot_matches_sync_scan_and_follows_grid_drift() {
        use super::AlacPoint;
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::test::TermSize;

        let size = TermSize::new(20, 4);
        let mut term = alacritty_terminal::term::Term::new(
            alacritty_terminal::term::Config::default(),
            &size,
            alacritty_terminal::event::VoidListener,
        );
        term.goto(0, 0);
        for c in "hello".chars() {
            term.input(c);
        }
        term.linefeed();
        for c in "world".chars() {
            term.input(c);
        }

        // 后台搜索走快照路径，必须与同步扫描结果完全一致。
        let snapshot = super::grid_search_snapshot(&term);
        assert_eq!(snapshot.lines.len(), term.grid().total_lines());
        let via_snapshot = super::search_matches_in_snapshot(&snapshot, "HELLO");
        let via_grid = super::search_matches_in_grid(&term, "HELLO");
        assert_eq!(via_snapshot, via_grid, "快照路径与同步路径必须一致");
        assert_eq!(via_snapshot[0].start, AlacPoint::new(Line(0), Column(0)));

        // 继续输出 3 行：旧内容整体上移，grid 绝对坐标随之漂移。
        // 这正是命中集必须跟随内容代次重算的原因——沿用旧坐标会高亮到错行。
        for _ in 0..3 {
            term.goto(3, 0);
            term.linefeed();
        }
        let shifted = super::search_matches_in_grid(&term, "hello");
        assert_eq!(shifted.len(), 1);
        assert_eq!(
            shifted[0].start.line,
            Line(-3),
            "重算后应指向内容当前所在行"
        );
        assert_ne!(shifted[0].start.line, via_snapshot[0].start.line);
    }

    #[test]
    fn scroll_center_clamps_offset_to_history() {
        // 视口 10 行、scrollback 100 行：
        // - 屏内行 Line(0)：居中需要 offset = 5 - 0 = 5（屏顶行落在视口第 5 行）
        assert_eq!(
            super::scroll_offset_to_center(10, 100, alacritty_terminal::index::Line(0)),
            5
        );
        // - scrollback 行 Line(-50)：offset = 5 + 50 = 55
        assert_eq!(
            super::scroll_offset_to_center(10, 100, alacritty_terminal::index::Line(-50)),
            55
        );
        // - 无 scrollback 时屏内行也居不了中：clamp 到 0（正常视图）
        assert_eq!(
            super::scroll_offset_to_center(10, 0, alacritty_terminal::index::Line(0)),
            0
        );
        // - 越过历史的行（无 scrollback 时）：clamp 到 history=0
        assert_eq!(
            super::scroll_offset_to_center(10, 0, alacritty_terminal::index::Line(-50)),
            0
        );
        // - 零行视口（防御）：0
        assert_eq!(
            super::scroll_offset_to_center(0, 100, alacritty_terminal::index::Line(-50)),
            0
        );
    }

    #[test]
    fn parses_osc_notification_forms() {
        // iTerm2 OSC 9;notify;Title;Body
        let (title, body) =
            Terminal::parse_notification_osc("9;notify;构建失败;请检查日志".as_bytes()).unwrap();
        assert_eq!(title, "构建失败");
        assert_eq!(body, "请检查日志");
        // urxvt OSC 777;notify;Title;Body
        let (title, body) = Terminal::parse_notification_osc(b"777;notify;Deploy;done").unwrap();
        assert_eq!(title, "Deploy");
        assert_eq!(body, "done");
        // 简化 OSC 9;message：全文作标题，body 为空
        let (title, body) = Terminal::parse_notification_osc(b"9;hello world").unwrap();
        assert_eq!(title, "hello world");
        assert_eq!(body, "");
        // OSC 9;4 进度上报（ConEmu）不是通知
        assert!(Terminal::parse_notification_osc(b"9;4;50").is_none());
        assert!(Terminal::parse_notification_osc(b"9;4;3;50").is_none());
        // 无效形态
        assert!(Terminal::parse_notification_osc(b"9;").is_none());
        assert!(Terminal::parse_notification_osc(b"8;").is_none());
    }

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

    // ===== resize 通知合并器测试 =====

    /// 构造 `cols×rows` 的 `TerminalBounds`（line_height=16、cell_width=9）。
    fn bounds_for(cols: usize, rows: usize) -> TerminalBounds {
        TerminalBounds::new(
            px(16.0),
            px(9.0),
            Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(9.0 * cols as f32), px(16.0 * rows as f32)),
            ),
        )
    }

    /// 构造一个无 PTY、但带记录型 `remote_resize_callback` 的终端，
    /// 以便观察「窗口尺寸通知」的下发次数与内容（含缩放）。
    fn resize_test_terminal(
        cx: &mut TestAppContext,
    ) -> (Entity<Terminal>, Arc<Mutex<Vec<(u16, u16, f32)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sink = calls.clone();
        let terminal = cx.new(move |_| {
            let mut t = Terminal::create_error_terminal();
            t.remote_resize_callback = Some(Box::new(move |cols, rows, scale| {
                sink.lock().unwrap().push((cols, rows, scale));
            }));
            t
        });
        (terminal, calls)
    }

    fn grid_size(t: &Terminal) -> (usize, usize) {
        let c = t.last_content();
        (c.cols, c.rows)
    }

    /// ghostty 语义：25ms 非重置节流（定时器活动期间只更新最新值），
    /// 到点一次性原子应用「网格 reflow + PTY/SSH 通知」，只应用最新尺寸。
    #[gpui::test]
    fn resize_throttles_like_ghostty(cx: &mut TestAppContext) {
        let (terminal, calls) = resize_test_terminal(cx);

        // 首帧立即校正（deepterm 特有：PTY 以默认 55×18 启动）。
        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(30, 10), 1.0, cx);
            assert_eq!(grid_size(t), (30, 10));
        });
        assert_eq!(calls.lock().unwrap().as_slice(), &[(30, 10, 1.0)]);

        // 合并窗口内网格保持旧尺寸，只保留最新 pending。
        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(40, 14), 1.0, cx);
            assert_eq!(grid_size(t), (30, 10), "合并窗口内网格不应变化");
        });
        assert_eq!(calls.lock().unwrap().len(), 1, "合并窗口内不应通知");

        // 定时器活动期间后续事件不重置定时器（ghostty handleResize 语义）。
        cx.background_executor
            .advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        terminal.update(cx, |t, cx| t.resize(bounds_for(50, 15), 1.0, cx));
        cx.background_executor
            .advance_clock(Duration::from_millis(15));
        cx.run_until_parked();
        terminal.read_with(cx, |t, _| {
            assert_eq!(grid_size(t), (50, 15), "25ms 到点应应用最新尺寸");
        });
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(30, 10, 1.0), (50, 15, 1.0)],
            "只通知最新尺寸"
        );
    }

    /// 零尺寸（cols=0）不应用、不通知；恢复后能再次应用。
    #[gpui::test]
    fn resize_zero_bounds_does_not_apply(cx: &mut TestAppContext) {
        let (terminal, calls) = resize_test_terminal(cx);
        terminal.update(cx, |t, cx| t.resize(bounds_for(40, 10), 1.0, cx));
        let before = calls.lock().unwrap().len();

        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(0, 10), 1.0, cx);
            assert_eq!(grid_size(t), (40, 10), "零尺寸不应改变网格");
        });
        assert_eq!(calls.lock().unwrap().len(), before, "零尺寸不应通知");

        // 恢复有效尺寸：走合并窗口后应用。
        terminal.update(cx, |t, cx| t.resize(bounds_for(41, 10), 1.0, cx));
        cx.background_executor.advance_clock(RESIZE_COALESCE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().last().copied(),
            Some((41, 10, 1.0)),
            "恢复尺寸后应应用最新尺寸"
        );
    }

    /// 尺寸拖回已应用值时撤销 pending 并取消合并任务，不多发一次通知。
    #[gpui::test]
    fn resize_dedupes_when_returning_to_applied_size(cx: &mut TestAppContext) {
        let (terminal, calls) = resize_test_terminal(cx);
        terminal.update(cx, |t, cx| t.resize(bounds_for(30, 10), 1.0, cx)); // prime
        assert_eq!(calls.lock().unwrap().as_slice(), &[(30, 10, 1.0)]);

        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(40, 14), 1.0, cx); // 武装合并定时器
            t.resize(bounds_for(30, 10), 1.0, cx); // 拖回已应用尺寸
        });
        cx.background_executor.advance_clock(RESIZE_COALESCE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(30, 10, 1.0)],
            "拖回原尺寸不应产生多余通知"
        );
    }

    /// 护栏：任何时刻 `last_content` 的网格尺寸都必须与 `terminal_bounds` 一致
    /// （两者只在 apply 时一起变化），防止渲染/命中错位。
    #[gpui::test]
    fn resize_snapshot_stays_in_sync_with_grid(cx: &mut TestAppContext) {
        let (terminal, _calls) = resize_test_terminal(cx);
        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(30, 10), 1.0, cx); // prime

            // 合并窗口内两者都不变，不变量成立
            t.resize(bounds_for(80, 35), 1.0, cx);
            let c = t.last_content();
            assert_eq!(c.cols, t.terminal_bounds.num_columns());
            assert_eq!(c.rows, t.terminal_bounds.num_lines());
            assert_eq!((c.cols, c.rows), (30, 10));
        });
        cx.background_executor.advance_clock(RESIZE_COALESCE_DELAY);
        cx.run_until_parked();
        terminal.read_with(cx, |t, _| {
            let c = t.last_content();
            assert_eq!(c.cols, t.terminal_bounds.num_columns());
            assert_eq!(c.rows, t.terminal_bounds.num_lines());
            assert_eq!((c.cols, c.rows), (80, 35));
        });
    }

    /// 两个几何投影必须同口径。
    ///
    /// `TermGeometry`（尺寸上报用）与 `WindowSize`（PTY/SIGWINCH 用）都是
    /// 从同一份 `TerminalBounds` 投影出来的，任何一处漂移都会让终端应用算出
    /// 错误的像素尺寸（画面比例错乱）。这里把两者钉在一起。
    #[test]
    fn from_bounds_matches_window_size_conversion() {
        use alacritty_terminal::event::WindowSize;

        let bounds = bounds_for(80, 24);
        let geometry: crate::terminal::escape_scan::TermGeometry = bounds.into();
        let window: WindowSize = bounds.into();

        assert_eq!(geometry.rows, window.num_lines);
        assert_eq!(geometry.cols, window.num_cols);
        assert_eq!(geometry.cell_width, window.cell_width);
        assert_eq!(geometry.cell_height, window.cell_height);
    }

    /// 几何投影的像素结果必须自洽：`cell_width` 是宽度、`cell_height` 是高度。
    ///
    /// `bounds_for` 用 line_height=16、cell_width=9，因此 80x24 的文本区
    /// 应是 720x384 像素。若两处字段被互换（`csi_16_t` 的高在前极易踩到
    /// 这个坑），这条会失败。
    #[test]
    fn from_bounds_keeps_cell_axes_straight() {
        let bounds = bounds_for(80, 24);
        let geometry: crate::terminal::escape_scan::TermGeometry = bounds.into();

        assert_eq!(geometry.cell_width, 9);
        assert_eq!(geometry.cell_height, 16);
        assert_eq!(geometry.width_pixels(), 720);
        assert_eq!(geometry.height_pixels(), 384);
    }

    /// 缩放投影必须按 DPI 放大 cell 像素尺寸（Retina 上就是 2×）。
    ///
    /// 终端应用（如 `terminal-browser`）拿这个尺寸决定渲染 surface 的像素
    /// 规模；少报一半就会渲染出低分辨率画面再放大。ghostty 从字号起就按
    /// `content_scale` 换算（`font/face.zig:55-58`），这里锁定同一行为。
    #[test]
    fn scaled_geometry_doubles_extents() {
        let bounds = bounds_for(80, 24);
        let scaled = bounds.term_geometry(2.0);

        assert_eq!(scaled.cell_width, 18, "cell 宽应随缩放翻倍");
        assert_eq!(scaled.cell_height, 32, "cell 高应随缩放翻倍");
        assert_eq!(scaled.width_pixels(), 1440);
        assert_eq!(scaled.height_pixels(), 768);
        // 行列数是格子数，与 DPI 无关。
        assert_eq!((scaled.cols, scaled.rows), (80, 24));
    }

    /// 缩放后的取整必须四舍五入，与 ghostty `font/Metrics.zig:265` 的
    /// `@round(face_width)` 一致。
    ///
    /// 截断（`as u16`）在 1.5× 这种分数缩放下每列稳定少 1px，几十列累积
    /// 后宽高比就歪了。
    #[test]
    fn scaled_geometry_rounds_like_ghostty() {
        let bounds = bounds_for(80, 24);
        let scaled = bounds.term_geometry(1.5);

        // 9 * 1.5 = 13.5 → 14（截断会得 13）
        assert_eq!(scaled.cell_width, 14);
        // 16 * 1.5 = 24，正好整除
        assert_eq!(scaled.cell_height, 24);
    }

    /// `WindowSize` 随缩放只改 cell 像素尺寸，**绝不能**改行列数。
    ///
    /// `num_lines`/`num_cols` 是 PTY 的 `TIOCSWINSZ` 行列数，被缩放会让
    /// 子进程的网格尺寸直接错乱。
    #[test]
    fn window_size_keeps_grid_counts_unscaled() {
        let bounds = bounds_for(80, 24);
        let base = bounds.window_size(1.0);
        let scaled = bounds.window_size(2.0);

        assert_eq!((base.num_lines, base.num_cols), (24, 80));
        assert_eq!((scaled.num_lines, scaled.num_cols), (24, 80));
        assert_eq!(scaled.cell_width, base.cell_width * 2);
        assert_eq!(scaled.cell_height, base.cell_height * 2);
    }

    /// 非法的缩放值（0 / 负数 / NaN）退化为 1.0，而不是算出 0 尺寸。
    ///
    /// 平台初始化早期或 headless 测试可能拿到 0；若直接相乘，终端应用会
    /// 按 0×0 建 surface，比低分辨率更糟。
    #[test]
    fn degenerate_scale_falls_back_to_one() {
        let bounds = bounds_for(80, 24);

        for bad in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            let geometry = bounds.term_geometry(bad);
            assert_eq!(geometry.cell_width, 9, "scale={bad} 应退化为 1.0");
            assert_eq!(geometry.cell_height, 16, "scale={bad} 应退化为 1.0");
        }
    }

    // ===== 远程路径的尺寸上报 =====

    /// 构造一个远程式终端：无 PTY，应答经 `remote_input_callback` 收集。
    fn remote_size_test_terminal(
        cx: &mut TestAppContext,
        cols: usize,
        rows: usize,
    ) -> (Entity<Terminal>, Arc<Mutex<Vec<u8>>>) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = written.clone();
        let terminal = cx.new(move |_| {
            let mut t = Terminal::create_error_terminal();
            t.terminal_bounds = bounds_for(cols, rows);
            t.remote_input_callback = Some(Box::new(move |bytes| {
                sink.lock().unwrap().extend_from_slice(bytes);
            }));
            t
        });
        (terminal, written)
    }

    /// `CSI 16 t` 在远程路径上被应答，且序列不进 vte。
    ///
    /// 格式必须是 ghostty 的 `\x1b[6;{cell_height};{cell_width}t`——**高在前**。
    /// `bounds_for` 给的是 cell_width=9、line_height=16。
    #[gpui::test]
    fn remote_csi_16_t_answered_with_cell_height_then_width(cx: &mut TestAppContext) {
        let (terminal, written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[16t", false, cx);
        });

        let out = written.lock().unwrap().clone();
        assert_eq!(
            out,
            b"\x1b[6;16;9t".to_vec(),
            "应回应 `cell_height;cell_width`（高在前），实际 {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// `CSI 14 t` / `CSI 18 t` 不得被拦截——vte 已能正确应答，重复拦截会
    /// 造成双重应答（客户端收到两条尺寸）。
    #[gpui::test]
    fn remote_csi_14_and_18_not_intercepted(cx: &mut TestAppContext) {
        let (terminal, written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[14t", false, cx);
            t.process_pty_data(b"\x1b[18t", false, cx);
        });

        assert!(
            written.lock().unwrap().is_empty(),
            "14/18 的应答由 vte 负责，本层不应写入任何字节"
        );
    }

    /// `CSI 16 t` 的应答随显示缩放放大——这是「分辨率低一半」的主修复点。
    ///
    /// Retina 上 `display_scale` 是 2.0，因此 9x16 的 cell 必须报成 18x32。
    #[gpui::test]
    fn remote_csi_16_t_scales_with_display_scale(cx: &mut TestAppContext) {
        let (terminal, written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.display_scale = 2.0;
            t.process_pty_data(b"\x1b[16t", false, cx);
        });

        let out = written.lock().unwrap().clone();
        assert_eq!(
            out,
            b"\x1b[6;32;18t".to_vec(),
            "Retina 上应报物理像素 32x18，实际 {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// `CSI 14 t` 的应答同样必须按物理像素给出。
    ///
    /// 走的是 `TextAreaSizeRequest`（alacritty 在闭包里算
    /// `num_lines * cell_height`，即 `term/mod.rs:2258`），与 `CSI 16 t`
    /// 是**两条独立的投影路径**，必须分别覆盖。80x24 的文本区在 2× 下是
    /// 1440x768。
    ///
    /// 这里直接投递事件而不喂字节：vte 的 `EventProxy` 是异步的，字节路径
    /// 需要事件循环泵才能观察到 `PtyWrite`。闭包按 alacritty 原样构造，因此
    /// 测到的仍是真实格式化逻辑。
    #[gpui::test]
    fn remote_csi_14_t_reports_scaled_grid_extent(cx: &mut TestAppContext) {
        use alacritty_terminal::event::WindowSize;

        let (terminal, written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.display_scale = 2.0;
            // 与 `Term::text_area_size_pixels` 完全同构的格式化闭包。
            t.process_event(
                alacritty_terminal::event::Event::TextAreaSizeRequest(std::sync::Arc::new(
                    move |window_size: WindowSize| {
                        format!(
                            "\x1b[4;{};{}t",
                            window_size.num_lines * window_size.cell_height,
                            window_size.num_cols * window_size.cell_width
                        )
                    },
                )),
                cx,
            );
        });

        let out = written.lock().unwrap().clone();
        assert_eq!(
            out,
            b"\x1b[4;768;1440t".to_vec(),
            "CSI 14 t 应报物理像素 768x1440（高在前），实际 {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// DPI 变化而网格不变时，几何也必须被重新下发。
    ///
    /// 跨显示器拖动窗口时 cols/rows 往往一点没变、只有缩放变了。若
    /// `resize` 的早退吞掉这次更新，PTY 与读线程就会一直用旧几何上报。
    /// 这里两次 resize 尺寸相同、只换缩放，第二次必须仍产生通知。
    #[gpui::test]
    fn dpi_change_without_grid_change_updates_geometry(cx: &mut TestAppContext) {
        let (terminal, calls) = resize_test_terminal(cx);

        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(80, 24), 1.0, cx);
            t.resize(bounds_for(80, 24), 2.0, cx);
        });

        let calls = calls.lock().unwrap().clone();
        let scales: Vec<f32> = calls.iter().map(|(_, _, s)| *s).collect();
        assert!(
            scales.contains(&2.0),
            "仅缩放变化也必须重新下发几何（否则 SSH/PTY 侧分辨率退回半分辨率），\
             实际通知序列 {calls:?}"
        );
    }

    /// 普通文本与尺寸序列混合时，只有尺寸序列被摘除，文本完整进入网格。
    #[gpui::test]
    fn remote_text_around_size_sequence_still_reaches_grid(cx: &mut TestAppContext) {
        let (terminal, written) = remote_size_test_terminal(cx, 40, 10);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"ab\x1b[16tcd", false, cx);
        });

        assert_eq!(written.lock().unwrap().clone(), b"\x1b[6;16;9t".to_vec());

        // 文本 `abcd` 应完整落在网格上（尺寸序列不产生任何单元格）。
        let text: String = terminal.read_with(cx, |t, _| {
            let term = t.term.lock();
            term.grid().display_iter().map(|c| c.cell.c).collect()
        });
        assert!(
            text.starts_with("abcd"),
            "`ab` + `cd` 应拼成 abcd（序列被摘除后无残留），实际 {text:?}"
        );
    }

    /// 与 2048 合写的其它模式必须照常生效（回归：曾整条摘除导致粘贴保护消失）。
    ///
    /// `ESC[?2004;2048h` 里 `2004` 是 bracketed paste，由 vte 处理。若把整条
    /// 序列摘掉，应用的粘贴保护就没了——zsh/fish 这类会插入裸文本而没有
    /// `\x1b[200~`/`\x1b[201~` 包裹。
    #[gpui::test]
    fn remote_multi_param_decset_keeps_other_modes(cx: &mut TestAppContext) {
        let (terminal, _written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?2004;2048h", false, cx);
        });

        let bracketed = terminal.read_with(cx, |t, _| {
            t.term
                .lock()
                .mode()
                .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
        });
        assert!(
            bracketed,
            "`?2004;2048h` 不得吞掉 2004（bracketed paste），否则粘贴保护失效"
        );

        // 2048 本身也必须被识别（我们额外跟踪的那一份）。
        let mode_2048 = terminal.read_with(cx, |t, _| t.remote_mode_2048);
        assert!(mode_2048, "同一序列里的 2048 也必须被识别并置位");
    }

    /// DECSET 2048 置位时立即上报几何；复位静默。
    #[gpui::test]
    fn remote_mode_2048_enable_reports_and_disable_is_silent(cx: &mut TestAppContext) {
        let (terminal, written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?2048h", false, cx);
        });
        // 80x24, cell 9x16 → 720x384 像素。
        assert_eq!(
            written.lock().unwrap().clone(),
            b"\x1b[48;24;80;384;720t".to_vec(),
            "置位应立即上报（rows;cols;height_px;width_px）"
        );

        written.lock().unwrap().clear();
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?2048l", false, cx);
        });
        assert!(written.lock().unwrap().is_empty(), "复位不应产生上报");
    }

    /// 模式 1016（SGR-Pixels 鼠标）的完整往返。
    ///
    /// 这是 terminal-browser 实际发出的序列组合：
    /// `\x1b[?1016h` 置位、`\x1b[?1016$p` 探测。ghostty 对已置位的模式答
    /// `2`，客户端据此启用像素级命中测试。修复前这里答的是 `?1016;0$y`
    /// （vte/alacritty 的「不识别」），会让客户端退回单元格粒度。
    #[gpui::test]
    fn remote_mode_1016_probe_reports_recognized(cx: &mut TestAppContext) {
        let (terminal, written) = remote_size_test_terminal(cx, 80, 24);

        // 未置位时应答「已识别但复位」= 1。
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?1016$p", false, cx);
        });
        assert_eq!(
            written.lock().unwrap().clone(),
            b"\x1b[?1016;1$y".to_vec(),
            "置位前的探测应答 `1`（已识别/复位），绝不能是 `0`（不识别）"
        );

        // 置位本身不产生应答（纯状态开关）。
        written.lock().unwrap().clear();
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?1016h", false, cx);
        });
        assert!(
            written.lock().unwrap().is_empty(),
            "DECSET 1016 是纯状态开关，不应产生任何应答"
        );

        // 置位后探测应答 `2`。
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?1016$p", false, cx);
        });
        assert_eq!(
            written.lock().unwrap().clone(),
            b"\x1b[?1016;2$y".to_vec(),
            "置位后探测应答 `2`（已识别/置位）"
        );
    }

    /// 1016 的状态必须真的影响鼠标报文的坐标格式。
    #[gpui::test]
    fn remote_mode_1016_switches_mouse_report_to_pixels(cx: &mut TestAppContext) {
        let (terminal, _written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?1016h", false, cx);
            assert!(
                t.sgr_pixels_active(),
                "置位后 sgr_pixels_active 应为真（否则报文仍是单元格坐标）"
            );
        });

        // 复位后必须回到假，否则像素模式会粘住。
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?1016l", false, cx);
            assert!(!t.sgr_pixels_active(), "复位后 sgr_pixels_active 应为假");
        });
    }

    /// `CSI 16 t` 整条落在同一个 chunk 里时必然被应答。
    ///
    /// 注：跨 chunk 被切断的形态（如 `\x1b[1` + `6t`）会交给 vte 状态机，
    /// 本轮不保证应答——终端应用的探测都是整条发送，且真发生也只是退化为
    /// 「拿不到尺寸」（与修复前一致），不会更糟。这里只钉住整条到达的情形。
    #[gpui::test]
    fn remote_csi_16_t_answered_when_whole(cx: &mut TestAppContext) {
        let (terminal, written) = remote_size_test_terminal(cx, 80, 24);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[16t", false, cx);
        });
        assert_eq!(written.lock().unwrap().clone(), b"\x1b[6;16;9t".to_vec());
    }

    /// 占位符单元格能被拍平出来，且行/列/颜色/变音符齐全。
    ///
    /// 走一遍真实路径：用 vte 处理器往 alacritty 网格里打
    /// `U+10EEEE` + 组合变音符，再让 `Terminal` 的快照把它捞出来。
    #[test]
    fn kitty_placeholder_cells_extracts_placeholder_with_diacritics() {
        use alacritty_terminal::term::test::TermSize;
        use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

        let size = TermSize::new(20, 4);
        let mut term = alacritty_terminal::term::Term::new(
            alacritty_terminal::term::Config::default(),
            &size,
            alacritty_terminal::event::VoidListener,
        );

        // 必须走真正的 vte 解析器：`Handler::input` 只处理可打印/零宽字符，
        // 不解析转义序列，直接喂字节会把 `[38;5;42m` 当正文打出来。
        // `CSI 2;4H` 是 1-based 的行/列，落到位图第 1 行第 3 列。
        let mut processor: Processor = Processor::new();
        processor.advance(
            &mut term,
            "\x1b[2;4H\x1b[38;5;42m\u{10EEEE}\u{0305}\u{030D}".as_bytes(),
        );

        // 把这一行快照成 TerminalContent，再走 Terminal 的拍平函数。
        let mut content = TerminalContent {
            cells: Vec::new(),
            row_ranges: Vec::new(),
            cursor: alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(0),
                alacritty_terminal::index::Column(0),
            ),
            cursor_cell: PackedCell::from(&alacritty_terminal::term::cell::Cell::default()),
            cursor_shape: alacritty_terminal::vte::ansi::CursorShape::Block,
            display_offset: 0,
            mode: alacritty_terminal::term::TermMode::empty(),
            cols: 20,
            rows: 4,
            total_lines: 4,
            viewport_lines: 4,
            url_ranges: Vec::new(),
            hyperlink_ranges: Vec::new(),
            selection_range: None,
            palette: Default::default(),
        };
        let renderable = term.renderable_content();
        content
            .cells
            .extend(renderable.display_iter.map(|ic| IndexedCell {
                point: ic.point,
                cell: PackedCell::from(ic.cell),
                zerowidth: packed_zerowidth(ic.cell),
                hyperlink: ic.cell.hyperlink(),
            }));
        assert_eq!(
            content.cells.len(),
            80,
            "display_iter 会给出整个 20x4 视口的单元格（含空白格）"
        );

        // 只有占位符那一个格子有内容，其余都是空白。
        let non_blank: Vec<_> = content.cells.iter().filter(|c| c.cell.c != ' ').collect();
        assert_eq!(non_blank.len(), 1, "非空单元格只有一个");

        // 直接验证拍平后的字段，而不是构造整个 Terminal（那需要 GPUI 上下文）。
        let cell = non_blank[0];
        assert_eq!(cell.cell.c, crate::terminal::kitty::unicode::PLACEHOLDER);
        assert_eq!(cell.point.line.0, 1, "行号 = 网格绝对行");
        assert_eq!(cell.point.column.0, 3, "列号");
        assert_eq!(cell.cell.fg, Color::Indexed(42));
        assert_eq!(
            cell.zerowidth,
            Some(['\u{0305}', '\u{030D}', '\0']),
            "变音符应被带上，且不足 3 个时补 0"
        );
        // 颜色与变音符能解码出正确的图像 id 与行列。
        let decoded = crate::terminal::kitty::unicode::IncompletePlacement::decode(
            crate::terminal::kitty::unicode::color_to_id(&cell.cell.fg),
            cell.cell
                .underline_color
                .as_ref()
                .map_or(0, crate::terminal::kitty::unicode::color_to_id),
            &cell.zerowidth.unwrap(),
        );
        assert_eq!(decoded.image_id_low, 42);
        assert_eq!(decoded.row, Some(0));
        assert_eq!(decoded.col, Some(1));

        // 命名色的对照：默认前景色解出来是 0 号命名色，不会误判成图像。
        assert_eq!(
            crate::terminal::kitty::unicode::color_to_id(&Color::Named(NamedColor::Foreground)),
            NamedColor::Foreground as u32
        );
    }
}

#[cfg(test)]
mod paste_sanitize_tests {
    use super::sanitize_pasted_text;

    #[test]
    fn strips_escape_and_control_chars_in_normal_mode() {
        // 经典粘贴攻击：ESC 序列 + 换行执行。
        // ESC 剔除后残留的 "[2J" 只是普通文本，不再构成转义序列
        let malicious = "echo hi\n\u{1b}[2Jrm -rf /\u{7}";
        let out = sanitize_pasted_text(malicious, false);
        assert_eq!(out, "echo hi\n[2Jrm -rf /");
        assert!(!out.contains('\u{1b}'));
        assert!(!out.contains('\u{7}'));
    }

    #[test]
    fn normalizes_cr_and_crlf_to_lf_in_normal_mode() {
        // CRLF 不能变成两个 LF（否则每行之间多一个空行）
        assert_eq!(sanitize_pasted_text("a\r\nb\rc", false), "a\nb\nc");
        assert_eq!(sanitize_pasted_text("a\r\n\r\nb", false), "a\n\nb");
    }

    #[test]
    fn keeps_tabs_and_multiline_in_normal_mode() {
        assert_eq!(sanitize_pasted_text("a\tb\nc", false), "a\tb\nc");
    }

    #[test]
    fn strips_c1_controls_in_normal_mode() {
        assert_eq!(sanitize_pasted_text("a\u{9b}b", false), "ab");
    }

    #[test]
    fn bracketed_mode_preserves_content_but_strips_delimiters() {
        // 内容中的 ESC[201~ 会提前结束粘贴模式，必须剔除
        let text = "ls\u{1b}[201~; rm -rf /\u{1b}[200~";
        let out = sanitize_pasted_text(text, true);
        assert_eq!(out, "ls; rm -rf /");
    }

    #[test]
    fn non_ascii_text_is_preserved() {
        assert_eq!(sanitize_pasted_text("中文 ok ✅", false), "中文 ok ✅");
    }
}
