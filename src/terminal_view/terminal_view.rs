//! Terminal view component - Full featured

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EntityInputHandler, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString, Subscription, Task,
    UTF16Selection, Window, actions, div, point, px,
};
use gpui::{KeyBinding, prelude::*};
use unicode_width::UnicodeWidthStr;

use crate::builtin_commands::executor::CommandResult;
use crate::file_manager::TransferRequest;
use crate::ssh::platform::RemoteOS;
use crate::ssh_view::connect::Event::Reconnect;
/// Events emitted by TerminalView
#[derive(Clone, Debug)]
pub enum Event {
    /// Terminal process exited - container should close this view
    CloseTerminal,
    /// 子进程退出（带退出状态）
    ProcessExited(i32),
    /// Open a file or directory in the editor or file tree.
    /// `terminal_cwd` is this terminal's working directory: the file-tree
    /// root always equals it, clicked subdirectories only reveal inside it.
    OpenPath {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
        ssh_config: Option<SshConnectionConfig>,
        terminal_cwd: Option<String>,
    },
    /// 发起一次文件传输（由 /scp 等内建命令触发）
    StartTransfer {
        request: TransferRequest,
    },
    /// SSH 重新连接请求
    SshReconnect {
        config: SshConnectionConfig,
    },
    /// 远程操作系统已检测到
    RemoteOsDetected {
        os: RemoteOS,
    },
    WorkingDirectoryChanged,
    /// 终端标题变化（可能由前台进程设置）
    TitleChanged,
    /// 后台铃：终端 BEL / 通知到达且本视图未聚焦，
    /// 面板链路据此点亮 tab 的需关注红点。
    BellRangWhileUnfocused,
}

impl EventEmitter<Event> for TerminalView {}

/// ASCII control character for Ctrl+V (SYN - Synchronous Idle)
///
/// 定义在 `terminal` 模块（终端域常量）；此处 re-export 保持
/// `terminal_view::CTRL_V` 旧路径。此前定义在本模块，使 `terminal`
/// 反向 use `terminal_view`，形成 terminal ↔ terminal_view 模块环。
pub use crate::terminal::CTRL_V;

/// IME state for tracking marked (pre-edit) text during composition
pub struct ImeState {
    pub marked_text: String,
}

use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::TermMode;

use crate::assets::Icon;
use crate::builtin_commands::{BuiltinCommandManager, resolve_command_line};
use crate::components::input::{EditorInput, InputEvent, InputState};
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
use crate::workspace::session_state::TerminalSessionState;

// Define terminal actions
actions!(
    terminal,
    [Copy, Paste, Clear, SelectAll, StartSearch, DismissSearch]
);
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
const TAB_LABEL_DEBOUNCE_DELAY: Duration = Duration::from_millis(100);

/// 拖拽选择边缘自动滚动：指针距视口上下边缘小于此行数时激活。
const SELECTION_AUTOSCROLL_EDGE_LINES: f32 = 1.5;
/// 边缘自动滚动周期。过快会在高频终端上产生过多重绘，过慢则滚动不连贯。
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(50);

// Scroll velocity tracking constants
const SCROLL_VELOCITY_WINDOW_SIZE: usize = 3;
const SCROLL_MULTIPLIER_BASE: f32 = 3.0;
const SCROLL_MULTIPLIER_MAX: f32 = 8.0;
const SCROLL_VELOCITY_SCALE: f32 = 2000.0;

// Scrollbar thumb animation duration (seconds)
const SCROLLBAR_ANIMATION_DURATION: f32 = 0.15;

/// BEL 视觉闪烁总时长（毫秒）：视区边缘呼吸式高亮，随后自然衰减。
const BELL_FLASH_MS: u64 = 350;

/// 搜索防抖窗口（毫秒）：输入停顿 / 终端停止输出后才重算命中集。
const SEARCH_DEBOUNCE_MS: u64 = 150;

/// 内容漂移触发重算的节流窗口（毫秒）。持续输出时最坏每此间隔重算一次，
/// 避免每帧调度堆积出几十个后台扫描任务（Task drop 不会取消已排队的任务）。
const SEARCH_DRIFT_THROTTLE_MS: u64 = 400;

pub fn init(cx: &mut App) {
    // Bind keyboard shortcuts
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        // 全选：macOS 用 cmd-a；其他平台用 ctrl-shift-a（ctrl-a 是 shell
        // 的行首快捷键，不能占用）。
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-a", SelectAll, Some(CONTEXT)),
        #[cfg(target_os = "windows")]
        KeyBinding::new("ctrl-k", ShowSshPicker, Some(CONTEXT)),
        #[cfg(not(target_os = "windows"))]
        KeyBinding::new("cmd-k", ShowSshPicker, Some(CONTEXT)),
        // 回滚区搜索：macOS 用 cmd-f，其他平台 ctrl-shift-f
        //（ctrl-f 是 shell 的 forward-char，不能占用）。
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-f", StartSearch, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-f", StartSearch, Some(CONTEXT)),
        // Escape 关闭搜索浮层（仅在搜索激活时消费，其余时候传播给 shell）
        KeyBinding::new("escape", DismissSearch, Some(CONTEXT)),
    ]);
}

/// 广播消息 - 广播组内同步的内容。
///
/// - `Raw`：原始终端输入字节（普通 shell 输入），接收方直接写入 PTY。
/// - `BuiltinSync`：内建命令缓冲的最新状态（文本 + 光标 + 浮层折叠），接收方据此镜像显示。
/// - `BuiltinEnd`：内建命令输入结束（Ctrl+C 取消 / 提交后清理），接收方同步清空本地镜像。
/// - `BuiltinCommand`：内建命令整行（如 `/scp ...`），接收方在各自上下文静默执行。
#[derive(Clone, Debug)]
pub enum BroadcastMsg {
    Raw(Vec<u8>),
    BuiltinSync {
        text: String,
        cursor: usize,
        dismissed: bool,
    },
    BuiltinEnd,
    BuiltinCommand(String),
}

/// 广播输入回调类型 - 用于将输入广播给其他终端
pub type BroadcastInputCallback = Box<dyn Fn(BroadcastMsg)>;

/// 广播回环抑制作用域守卫。
///
/// 构造时把标志置为 `true`，在作用域结束时（**包括 panic unwind**）复位为 `false`。
/// 用于广播接收逻辑（静默执行内建命令 / 镜像内建输入）期间抑制再次广播，避免回环。
///
/// 之所以不用「手动置 true、结尾置 false」而改用守卫：后者若中间 panic，
/// `broadcast_suppressed` 会永久停留在 `true`，导致该面板后续所有广播被静默吞掉且无任何报错。
///
/// 持有 `Rc<Cell<bool>>` 而非 `&mut bool`：守卫需要与 `self` 上的其它 `&mut self`
/// 方法调用共存，直接借用字段会产生别名冲突；共享所有权的内部可变性容器是安全做法。
struct BroadcastSuppressionGuard {
    flag: Rc<Cell<bool>>,
}

impl BroadcastSuppressionGuard {
    fn new(flag: &Rc<Cell<bool>>) -> Self {
        flag.set(true);
        Self {
            flag: Rc::clone(flag),
        }
    }
}

impl Drop for BroadcastSuppressionGuard {
    fn drop(&mut self) {
        self.flag.set(false);
    }
}

/// 内建命令执行模式。
///
/// - `Interactive`：正常交互提交，发射 UI 事件、弹 Toast。
/// - `Silent`：广播接收方（目标面板），静默执行：不弹 Toast、冲突策略降级
///   （避免弹冲突对话框），其余行为与 Interactive 一致。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BuiltinExecMode {
    Interactive,
    Silent,
}

/// 当前鼠标悬停的可点击链接信息。
#[derive(Clone, Debug, PartialEq)]
pub struct HoveredLink {
    pub kind: HoveredLinkKind,
    pub url: SharedString,
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
    /// Resolved absolute path for file links (local path or remote path).
    pub resolved_path: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub ssh_config: Option<SshConnectionConfig>,
}

/// Hovered link kind. Hit-test priority: `OscHyperlink > Url > FilePath`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoveredLinkKind {
    /// Regex-detected `http(s)` URL.
    Url,
    /// Explicit OSC8 hyperlink (`file://` or `http(s)`).
    OscHyperlink,
    /// Inferred file path (`ls` output, `file:line:col`).
    FilePath,
}

fn decode_pct_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
                && let Ok(v) = u8::from_str_radix(hex, 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract a local absolute path from an OSC8 `file://` URI.
/// `file:///etc/hosts` / `file://localhost/etc/hosts` → `/etc/hosts`.
fn parse_file_uri_local_path(uri: &str) -> Option<std::path::PathBuf> {
    let trimmed = uri.trim();
    // `get(..7)` 而非 `&trimmed[..7]`：OSC8 内容完全由终端输出控制，
    // 首部含多字节字符时字节切片会 panic。
    if !trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("file://"))
    {
        return None;
    }
    let rest = &trimmed[7..];
    // Split `host/path`; empty/localhost host means local path.
    let path = match rest.find('/') {
        Some(idx) => &rest[idx..],
        None => return None,
    };
    let decoded = decode_pct_path(path);
    if !decoded.starts_with('/') {
        return None;
    }
    Some(std::path::PathBuf::from(decoded))
}

/// Extract a remote absolute path from an OSC8 `file://` URI.
/// The host part is the *remote* host by construction (bytes came from SSH).
fn parse_file_uri_remote_path(uri: &str) -> Option<String> {
    parse_file_uri_local_path(uri).map(|p| p.to_string_lossy().to_string())
}

/// 终端滚动区搜索状态（由 Cmd-F / Ctrl-Shift-F 激活）。
///
/// - `matches` 为 grid 绝对坐标命中集（后台线程比对产出）；
/// - 元素在 prepaint 阶段按当前 display_offset 映射到视口行并绘制高亮，
///   因此滚动无需重算（视口行映射每帧重算）；
/// - 命中集是**快照**：终端继续输出会让 grid 绝对坐标整体漂移，
///   由 `TerminalView::search_generation` 比对内容代次后触发重算。
#[derive(Default)]
pub struct TerminalSearchState {
    /// 搜索浮层是否展开（激活）
    pub active: bool,
    /// 搜索词（已去除首尾空白）
    pub query: String,
    /// 全部命中（行序从旧到新）
    pub matches: Vec<crate::terminal::terminal::SearchMatch>,
    /// 当前命中索引（matches 下标）
    pub active_index: usize,
}

impl TerminalSearchState {
    /// 当前命中的 grid 坐标（无命中返回 None）。
    pub fn active_match(&self) -> Option<&crate::terminal::terminal::SearchMatch> {
        self.matches.get(self.active_index)
    }
}

/// 本地 shell 进程 cwd 的后台刷新缓存。
///
/// 查询走 lsof / /proc 子进程（单次几十至几百毫秒），不能在 UI 线程执行：
/// 按键（内建命令补全）、标签刷新、拖拽等路径只读缓存，由
/// [`TerminalView::local_process_cwd`] 在后台线程刷新。
#[derive(Default)]
struct ProcessCwdCache {
    /// 最近一次后台查询结果
    cwd: Option<PathBuf>,
    /// 最近一次查询时间（含无 pid / 失败，避免重复 spawn）
    checked_at: Option<Instant>,
    /// 后台查询进行中（同时只允许一个）
    refresh_in_flight: bool,
}

pub struct TerminalView {
    terminal: Entity<Terminal>,
    terminal_subscription: Option<Subscription>,
    working_directory: Option<PathBuf>,
    ssh_config: Option<SshConnectionConfig>,
    /// 本地终端以指定命令启动时保存的命令（如 wsl.exe / powershell.exe），用于会话重连
    command: Option<String>,
    /// 与 `command` 配套的启动参数，用于会话重连
    command_args: Vec<String>,
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
    /// 拖拽选择时的边缘自动滚动方向：+1 = 向上滚入 scrollback，
    /// -1 = 向下滚回最新内容，None = 未激活。
    /// 由鼠标移动事件更新，由 `selection_autoscroll_task` 周期性消费。
    selection_autoscroll_direction: Option<i32>,
    /// 边缘自动滚动周期任务（约 50ms/次）。拖拽结束或鼠标离开
    /// 边缘区域时置 None 取消。
    selection_autoscroll_task: Option<Task<()>>,
    /// IME composition state for CJK input methods
    ime_state: Option<ImeState>,
    /// DeepTerm 内置命令管理器
    builtin_command_manager: BuiltinCommandManager,
    /// `/scp` 主机候选的磁盘配置缓存（加载时刻 + 快照）。
    /// 避免在 `/scp` 输入期间每个按键都读盘解析 ssh.json；短 TTL 兼顾
    /// 「新建/修改连接后尽快可见」。
    scp_hosts_cache: Option<(std::time::Instant, Vec<crate::ssh::SshConnectionConfig>)>,
    /// 广播输入回调 - 当终端输入需要广播给其他终端时使用
    broadcast_callback: Option<BroadcastInputCallback>,
    /// 应用远端同步的内建命令缓冲期间置位，阻止 mutate_builtin_command 再次广播（防回环）。
    /// 使用 `Rc<Cell<bool>>` 以便与 `&mut self` 方法共存（见 BroadcastSuppressionGuard）。
    broadcast_suppressed: Rc<Cell<bool>>,
    /// Pending scrollbar hide task - prevents spawning a new timer every render frame
    scrollbar_hide_task: Option<Task<()>>,
    /// Scroll timestamp used for the currently scheduled hide task.
    scrollbar_hide_scheduled_at: Option<Instant>,
    /// 等待"该换动画帧了"的定时任务；到点唤醒一次重绘。
    ///
    /// 复用任务而不是每帧 spawn：动画活跃期间每帧都会走到调度点，不缓存任务
    /// 会以帧率泄漏定时任务。
    kitty_animation_task: Option<Task<()>>,
    /// SSH 连接进度视图 - 在连接建立期间覆盖终端内容
    ssh_connect_view: Option<Entity<SshConnectView>>,
    /// SSH 连接视图订阅 - 用于取消订阅重新连接事件
    ssh_connect_subscription: Option<Subscription>,
    /// SSH 连接视图隐藏任务 - 用于取消延迟隐藏任务
    ssh_connect_hide_task: Option<Task<()>>,
    /// 检测到的远程操作系统（仅对SSH终端有意义）
    remote_os: Option<RemoteOS>,
    cached_working_directory: Option<String>,
    /// 本地 shell 进程 cwd（后台线程刷新，UI 路径只读）
    process_cwd: ProcessCwdCache,
    pending_remote_cwd_restore: Option<String>,
    /// 当前鼠标悬停的可点击链接（仅在按住 Cmd/Ctrl 时有效）
    pub hovered_link: Option<HoveredLink>,
    /// 滚动区搜索状态（Cmd-F 激活）
    pub search: TerminalSearchState,
    /// 搜索输入框实体（打开搜索时创建；None = 从未打开）
    search_input: Option<Entity<InputState>>,
    /// 搜索输入 Change 订阅（重算命中集）
    _search_input_subscription: Option<Subscription>,
    /// 搜索任务：150ms 防抖 → 主线程抓快照 → 后台线程比对 → 回主线程落地。
    /// 只保留最新一个（旧任务的回写会因 request id 过期被丢弃）。
    search_task: Option<Task<()>>,
    /// 后台搜索请求序号：每次调度 +1，回写时校验，丢弃过期结果
    /// （GPUI 的 Task 被 drop 只是 detach，旧任务仍会跑完）。
    search_request_id: u64,
    /// 上次重算命中集时的终端内容代次。内容变化（终端有输出 / scrollback
    /// 滚动）会让命中集的 grid 绝对坐标整体漂移，据此触发重算。
    search_generation: u64,
    /// 上次因「内容变化」而调度重算的时刻。
    ///
    /// GPUI 的 `Task` 被 drop 只是 detach（旧任务仍会跑完），持续输出时若每帧
    /// 都调度，会堆积出几十个全量扫描任务。这里对内容变化路径做节流：
    /// 用户改词则立即调度（见 `schedule_search_recompute` 的调用处重置）。
    search_last_schedule: Option<Instant>,
    /// Whether the link-hover modifier (Cmd/Ctrl) was held at the last mouse
    /// move. Transitions are forwarded to `Terminal::set_link_hover_active`
    /// so URL detection runs only while links are actually hoverable.
    link_modifier_held: bool,
    /// 文件悬停的存在性校验缓存：`Cmd` 按住划过同一文件时避免每个
    /// mousemove 都做一次 `metadata` syscall。短 TTL（新建/删除文件后
    /// 最多延迟片刻即刷新），有界防止无限增长。
    file_hover_stat_cache: std::collections::HashMap<PathBuf, (bool, Instant)>,
    /// Per-row render cache, indexed by viewport row (0 = top).
    pub row_render_cache: Vec<RowRenderCache>,
    /// Flattened render data reused across frames when nothing changed.
    pub flattened_render_cache: FlattenedRenderCache,
    /// Last theme id used to build row cache.
    ///
    /// 字号 / 行高 / 列数等度量变化不在此列：`TerminalElement::prepaint` 另有
    /// `RenderMetricsSignature` 比对，度量一变就整体清空行缓存。
    pub cached_theme_id: Option<String>,
    /// 最近一次灌入 Terminal.colors 的主题 id（OSC 颜色查询闭环）。
    applied_theme_id: Option<String>,
    /// Last selection range used to build row cache.
    pub cached_selection_range: Option<SelectionRange>,
    /// Last mouse mode state used to build row cache.
    pub cached_selection_mouse_mode: bool,
    // ── Render scratch buffers ──────────────────────────────────────────────
    // Reused across frames in `prepaint` to avoid per-frame allocations when
    // copying out the terminal's dirty-row / dirty-row-bound snapshots. These
    // are only read (never mutated) after being filled each frame.
    pub scratch_dirty_rows: Vec<bool>,
    pub scratch_dirty_row_bounds: Vec<Option<(usize, usize)>>,
    /// Kitty 图像纹理缓存（跨帧复用）。
    ///
    /// 放在视图而非元素上：元素是每次 `render` 新建的临时对象，缓存必须
    /// 跨帧存活才有意义。内容代际不变时复用同一张 `RenderImage`，避免每帧
    /// 对大图做 RGBA→BGRA 的全量重写。见 [`crate::terminal::kitty::render`]。
    pub kitty_image_cache: crate::terminal::kitty::render::ImageCache,
    /// Kitty 动画帧的时钟与唤醒调度。
    ///
    /// GPUI 只在有变化时重绘，不像专用终端渲染器那样每 vsync 都驱动一遍
    /// 动画，所以这里记住"下次该换帧的时刻"，只有到点才请求重绘——否则
    /// 只要有动画在跑，`render` 就会自己把自己无限地重新调度下去。
    pub kitty_animation_clock: crate::terminal::kitty::AnimationClock,
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
    /// 信息型 Toast（OSC 9/777 通知）：(标题, 正文)，与错误 Toast 分离。
    info_toast: Option<(String, String)>,
    /// 信息 Toast 自动隐藏任务
    info_toast_hide_task: Option<Task<()>>,
    // ── Bell visual flash ─────────────────────────────────────────────────
    /// 最近一次 BEL 到达时刻：驱动视区边缘的短促视觉闪烁。
    /// `None` 或距今超过 [`BELL_FLASH_MS`] 时不绘制。
    bell_flash_at: Option<Instant>,
    /// 闪烁自动结束任务（到点清 bell_flash_at 并重绘）
    bell_flash_task: Option<Task<()>>,
    /// 上一次 BEL 时刻（限频，防止快速连环铃把闪烁刷成频闪）。
    last_bell_at: Option<Instant>,
    // ── Tab label tracking ─────────────────────────────────────────────────
    /// 上一次计算出的 tab 标签，用于检测是否需要更新
    last_tab_label: Option<String>,
    /// Tab 标签防抖：延迟计算任务
    tab_label_debounce_task: Option<Task<()>>,
    /// Tab 标签是否需要在防抖触发后重新计算
    tab_label_pending: bool,
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
        self.terminal.update(cx, |terminal, _| {
            terminal.set_focused(focused);
        });
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

    fn on_working_directory_changed(&mut self, cwd: &str, cx: &mut Context<Self>) {
        self.cached_working_directory = Some(cwd.to_string());
        cx.emit(Event::WorkingDirectoryChanged);
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
        command: Option<String>,
        command_args: Vec<String>,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        cx.new(|cx| {
            let mut this = Self::build_with_terminal(
                cx,
                terminal,
                working_directory,
                ssh_config,
                initial_title,
                command,
                command_args,
                focus_handle,
            );
            this.update_title(cx);
            this.last_tab_label = Some(this.tab_label(cx));
            this
        })
    }

    /// 从既有 Terminal 实体构建视图值。
    ///
    /// [`Self::create_terminal_view`] 的实体构建体抽出的纯构造部分，
    /// 供 `replace_root` 式的基准窗口直接调用。
    ///
    /// 参数沿用 [`Self::create_terminal_view`] 的原始参数集（clippy 的
    /// 7 参上限在此放宽：这是纯机械拆分，合并参数会改变公开构造签名）。
    #[allow(clippy::too_many_arguments)]
    fn build_with_terminal(
        cx: &mut Context<Self>,
        terminal: Entity<Terminal>,
        working_directory: Option<PathBuf>,
        ssh_config: Option<SshConnectionConfig>,
        initial_title: SharedString,
        command: Option<String>,
        command_args: Vec<String>,
        focus_handle: FocusHandle,
    ) -> Self {
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
                    this.check_tab_label_changed(cx);
                    cx.notify();
                }
                TerminalEvent::WorkingDirectoryChanged(cwd) => {
                    this.on_working_directory_changed(cwd, cx);
                }
                TerminalEvent::Bell => {
                    this.on_bell(cx);
                }
                TerminalEvent::NotificationReceived { title, body } => {
                    this.on_notification(title.clone(), body.clone(), cx);
                }
                TerminalEvent::CloseTerminal => {
                    cx.emit(Event::CloseTerminal);
                }
                TerminalEvent::ProcessExited(status) => {
                    cx.emit(Event::ProcessExited(*status));
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

        Self {
            terminal: terminal_clone,
            terminal_subscription: Some(subscription),
            working_directory,
            ssh_config,
            command,
            command_args,
            focus_handle,
            blink_manager,
            _blink_subscriptions: blink_subscriptions,
            blink_setting: TerminalBlink::default(),
            app_requests_blink: false,
            was_focused: false,
            title: initial_title,
            selecting: false,
            selection_autoscroll_direction: None,
            selection_autoscroll_task: None,
            ime_state: None,
            builtin_command_manager: BuiltinCommandManager::new(),
            scp_hosts_cache: None,
            broadcast_callback: None,
            broadcast_suppressed: Rc::new(Cell::new(false)),
            scrollbar_hide_task: None,
            scrollbar_hide_scheduled_at: None,
            kitty_animation_task: None,
            ssh_connect_view: None,
            ssh_connect_subscription: None,
            ssh_connect_hide_task: None,
            remote_os: None,
            cached_working_directory: None,
            process_cwd: ProcessCwdCache::default(),
            pending_remote_cwd_restore: None,
            hovered_link: None,
            search: TerminalSearchState::default(),
            search_input: None,
            _search_input_subscription: None,
            search_task: None,
            search_request_id: 0,
            search_generation: 0,
            search_last_schedule: None,
            link_modifier_held: false,
            file_hover_stat_cache: std::collections::HashMap::new(),
            row_render_cache: Vec::new(),
            flattened_render_cache: FlattenedRenderCache::default(),
            cached_theme_id: None,
            applied_theme_id: None,
            cached_selection_range: None,
            cached_selection_mouse_mode: false,
            scratch_dirty_rows: Vec::new(),
            scratch_dirty_row_bounds: Vec::new(),
            kitty_image_cache: crate::terminal::kitty::render::ImageCache::new(),
            kitty_animation_clock: crate::terminal::kitty::AnimationClock::new(),
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
            info_toast: None,
            info_toast_hide_task: None,
            bell_flash_at: None,
            bell_flash_task: None,
            last_bell_at: None,
            last_tab_label: None,
            tab_label_debounce_task: None,
            tab_label_pending: false,
        }
    }

    /// 测试 / 基准专用：从无驱动 Terminal 构建视图（配合 `replace_root` 使用）。
    #[doc(hidden)]
    pub fn build_headless(cx: &mut Context<Self>, terminal: Entity<Terminal>) -> Self {
        let focus_handle = cx.focus_handle();
        let mut this = Self::build_with_terminal(
            cx,
            terminal,
            None,
            None,
            "Terminal".into(),
            None,
            Vec::new(),
            focus_handle,
        );
        this.update_title(cx);
        this.last_tab_label = Some(this.tab_label(cx));
        this
    }

    /// 测试 / 基准专用：focus 句柄（构造 TerminalElement 需要）。
    #[doc(hidden)]
    pub fn focus_handle_for_test(&self) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }

    /// 创建新的本地终端
    pub fn new(cx: &mut App) -> Entity<Self> {
        let working_directory = dirs::home_dir();
        Self::new_with_working_directory(cx, working_directory)
    }

    pub fn new_with_working_directory(
        cx: &mut App,
        working_directory: Option<PathBuf>,
    ) -> Entity<Self> {
        let terminal = Terminal::new(cx, working_directory.clone());
        Self::create_terminal_view(
            cx,
            terminal,
            working_directory,
            None,
            "Terminal".into(),
            None,
            Vec::new(),
        )
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
            command_string.clone().into(),
            Some(command_string),
            args_vec,
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
        Self::new_ssh_with_initial_remote_cwd(cx, ssh_config, None)
    }

    fn new_ssh_with_initial_remote_cwd(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        initial_remote_cwd: Option<String>,
    ) -> Entity<Self> {
        log::info!("=== 创建 SSH 远程终端（优先 russh，失败回退到 ssh 命令）===");
        log::info!(
            "目标: {}@{}:{}",
            ssh_config.username,
            ssh_config.host,
            ssh_config.port
        );

        let focus_handle = cx.focus_handle();
        let ssh_config_clone = ssh_config.clone();
        let initial_remote_cwd = initial_remote_cwd.filter(|cwd| !cwd.trim().is_empty());

        cx.new(|cx| {
            // 首先尝试 russh 方式创建终端
            let terminal = Terminal::new_ssh_with_remote_cwd(
                cx,
                &ssh_config_clone,
                initial_remote_cwd.clone(),
            );
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
                        this.check_tab_label_changed(cx);
                        cx.notify();
                    }
                    TerminalEvent::WorkingDirectoryChanged(cwd) => {
                        this.on_working_directory_changed(cwd, cx);
                    }
                    TerminalEvent::Bell => {
                        this.on_bell(cx);
                    }
                    TerminalEvent::NotificationReceived { title, body } => {
                        this.on_notification(title.clone(), body.clone(), cx);
                    }
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                    TerminalEvent::ProcessExited(status) => {
                        cx.emit(Event::ProcessExited(*status));
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
                command: None,
                command_args: Vec::new(),
                focus_handle,
                blink_manager,
                _blink_subscriptions: blink_subscriptions,
                blink_setting: TerminalBlink::default(),
                app_requests_blink: false,
                was_focused: false,
                title: ssh_config_clone.host_label().into(),
                selecting: false,
                selection_autoscroll_direction: None,
                selection_autoscroll_task: None,
                ime_state: None,
                builtin_command_manager: BuiltinCommandManager::new(),
                scp_hosts_cache: None,
                broadcast_callback: None,
                broadcast_suppressed: Rc::new(Cell::new(false)),
                scrollbar_hide_task: None,
                scrollbar_hide_scheduled_at: None,
                kitty_animation_task: None,
                ssh_connect_view: Some(connect_view.clone()),
                ssh_connect_subscription: None,
                ssh_connect_hide_task: None,
                remote_os: None,
                cached_working_directory: None,
                process_cwd: ProcessCwdCache::default(),
                pending_remote_cwd_restore: None,
                hovered_link: None,
                search: TerminalSearchState::default(),
                search_input: None,
                _search_input_subscription: None,
                search_task: None,
                search_request_id: 0,
                search_generation: 0,
                search_last_schedule: None,
                link_modifier_held: false,
                file_hover_stat_cache: std::collections::HashMap::new(),
                row_render_cache: Vec::new(),
                flattened_render_cache: FlattenedRenderCache::default(),
                cached_theme_id: None,
                applied_theme_id: None,
                cached_selection_range: None,
                cached_selection_mouse_mode: false,
                scratch_dirty_rows: Vec::new(),
                scratch_dirty_row_bounds: Vec::new(),
                kitty_image_cache: crate::terminal::kitty::render::ImageCache::new(),
                kitty_animation_clock: crate::terminal::kitty::AnimationClock::new(),
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
                info_toast: None,
                info_toast_hide_task: None,
                bell_flash_at: None,
                bell_flash_task: None,
                last_bell_at: None,
                last_tab_label: None,
                tab_label_debounce_task: None,
                tab_label_pending: false,
            };

            // 订阅 SshConnectView 的重新连接事件并存储订阅
            this.ssh_connect_subscription =
                Some(this.subscribe_ssh_connect_view(&connect_view, cx));

            this.update_title(cx);
            this.last_tab_label = Some(this.tab_label(cx));
            this
        })
    }

    pub fn new_ssh_with_remote_cwd(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        remote_cwd: Option<String>,
    ) -> Entity<Self> {
        let remote_cwd = remote_cwd.filter(|cwd| !cwd.trim().is_empty());
        let terminal_view =
            Self::new_ssh_with_initial_remote_cwd(cx, ssh_config, remote_cwd.clone());
        terminal_view.update(cx, |this, _cx| {
            this.pending_remote_cwd_restore = remote_cwd;
        });
        terminal_view
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
        cx.emit(Event::TitleChanged);
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
                    this.check_tab_label_changed(cx);
                    cx.notify();
                }
                TerminalEvent::WorkingDirectoryChanged(cwd) => {
                    this.on_working_directory_changed(cwd, cx);
                }
                TerminalEvent::Bell => {
                    this.on_bell(cx);
                }
                TerminalEvent::NotificationReceived { title, body } => {
                    this.on_notification(title.clone(), body.clone(), cx);
                }
                TerminalEvent::CloseTerminal => {
                    // 正常退出（用户执行 exit 命令），关闭 panel
                    cx.emit(Event::CloseTerminal);
                }
                TerminalEvent::ProcessExited(status) => {
                    cx.emit(Event::ProcessExited(*status));
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
                let remote_cwd = this.current_remote_working_directory(cx);
                let old_terminal = this.terminal.clone();
                let new_terminal = Terminal::new_ssh_with_remote_cwd(cx, config, remote_cwd);
                // 旧终端显式关闭：避免依赖引用计数回收时的 Drop 兜底，
                // 也确保旧 SSH 会话立即断开而非等到下次回收。
                old_terminal.update(cx, |t, _| t.kill());
                // 旧终端的行渲染缓存/扁平化缓存内容属于旧 terminal 实体。
                // 新实体 dirty_rows 初始为空，若保留旧缓存，连接完成的首帧
                // （尚无 Wakeup）可能复用旧终端的文本内容闪现一帧。
                this.row_render_cache.clear();
                this.flattened_render_cache = Default::default();
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

    /// 取出全部已缓存的 Kitty 纹理（tab/panel 关闭时释放 GPU 图集用）。
    ///
    /// 调用方必须把返回的纹理逐个 `Window::drop_image`。缓存清空后若视图
    /// 复活（如 undo 恢复），重绘会按需重建纹理，无副作用。
    pub fn take_kitty_textures(&mut self) -> Vec<std::sync::Arc<gpui::RenderImage>> {
        self.kitty_image_cache.take_all()
    }

    pub fn working_directory(&self) -> Option<&PathBuf> {
        self.working_directory.as_ref()
    }

    pub fn ssh_config(&self) -> Option<&SshConnectionConfig> {
        self.ssh_config.as_ref()
    }

    pub fn session_state(&self, cx: &App) -> TerminalSessionState {
        if let Some(config) = self.ssh_config.clone() {
            return TerminalSessionState::Ssh {
                config,
                remote_cwd: self.current_remote_working_directory(cx),
            };
        }

        let cwd = self.current_local_working_directory(cx);
        TerminalSessionState::Local {
            cwd: cwd.map(|path| path.to_string_lossy().to_string()),
            command: self.command.clone(),
            args: self.command_args.clone(),
        }
    }

    /// 终止该视图背后的终端（本地 shell 进程 / 远程 SSH 连接），用于替换或关闭前主动回收资源。
    pub fn kill(&self, cx: &mut App) {
        self.terminal.update(cx, |terminal, _| {
            terminal.kill();
        });
    }

    fn current_local_working_directory(&self, cx: &App) -> Option<PathBuf> {
        if let Some(cwd) = self.cached_working_directory.as_deref() {
            return Some(PathBuf::from(cwd));
        }

        let terminal = self.terminal.read(cx);
        terminal
            .reported_working_directory()
            .map(PathBuf::from)
            // 进程 cwd 只读后台缓存（local_process_cwd 刷新），不在 UI 线程起 lsof
            .or_else(|| self.process_cwd.cwd.clone())
            .or_else(|| self.working_directory.clone())
    }

    /// 读取并按需后台刷新本地 shell 进程的实时 cwd（lsof / /proc）。
    ///
    /// 查询移到后台线程：此前按键、标签刷新、拖拽等高频路径在 UI 线程
    /// 同步起 lsof 子进程，单次可达几十到几百毫秒，造成明显卡顿。
    /// 缓存 2 秒内直接复用；过期且有 pid 时后台刷新一次（in-flight 防抖），
    /// 期间返回旧值。无 pid（远程终端/尚未启动）也记录时间戳，避免重复进入。
    fn local_process_cwd(&mut self, cx: &mut Context<Self>) -> Option<PathBuf> {
        const TTL: Duration = Duration::from_secs(2);

        let cache_fresh = self
            .process_cwd
            .checked_at
            .is_some_and(|at| at.elapsed() < TTL);
        if cache_fresh || self.process_cwd.refresh_in_flight {
            return self.process_cwd.cwd.clone();
        }

        let Some(pid) = self.terminal.read(cx).child_process_id else {
            // 无 pid：记录检查时间，避免每次调用都重走到这里
            self.process_cwd.checked_at = Some(Instant::now());
            return self.process_cwd.cwd.clone();
        };

        self.process_cwd.refresh_in_flight = true;
        cx.spawn(async move |this, cx| {
            let cwd = cx
                .background_executor()
                .spawn(async move {
                    crate::builtin_commands::PathResolver::query_process_working_directory(pid)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.process_cwd = ProcessCwdCache {
                    cwd,
                    checked_at: Some(Instant::now()),
                    refresh_in_flight: false,
                };
                // cwd 变化会影响 tab 标签与会话保存
                cx.notify();
            });
        })
        .detach();
        self.process_cwd.cwd.clone()
    }

    fn current_remote_working_directory(&self, cx: &App) -> Option<String> {
        self.cached_working_directory
            .clone()
            .or_else(|| {
                self.terminal
                    .read(cx)
                    .reported_working_directory()
                    .map(String::from)
            })
            .or_else(|| self.pending_remote_cwd_restore.clone())
    }

    /// Hover at a view position: ensure link ranges are maintained, map the
    /// pixels to a grid cell, hit-test, and refresh `hovered_link`.
    /// Shared by mouse-move hover and mouse-down click (click must not rely
    /// on a possibly stale/missing hover cache).
    fn hover_at_position(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<HoveredLink> {
        if !self.link_modifier_held {
            self.link_modifier_held = true;
            self.terminal.update(cx, |terminal, _cx| {
                terminal.set_link_hover_active(true);
            });
        }
        let (row, col) = {
            let terminal = self.terminal.read(cx);
            let rel = position - terminal.terminal_bounds.bounds.origin;
            if rel.x < px(0.0) || rel.y < px(0.0) {
                return None;
            }
            let cell_width = terminal.terminal_bounds.cell_width;
            let line_height = terminal.terminal_bounds.line_height;
            let content = terminal.last_content();
            let col = (rel.x.as_f32() / cell_width.as_f32()).floor() as usize;
            let row = (rel.y.as_f32() / line_height.as_f32()).floor() as usize;
            if col >= content.cols || row >= content.rows {
                return None;
            }
            (row, col)
        };
        let link = self.hit_test_link(row, col, cx);
        if self.hovered_link != link {
            self.hovered_link = link.clone();
            cx.notify();
        }
        link
    }

    /// Activate a hovered link. Returns true when the click is consumed
    /// (opened or deliberately swallowed) and must not start a selection.
    fn activate_hovered_link(
        &self,
        link: &HoveredLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match link.kind {
            HoveredLinkKind::Url => {
                // 关键安全：仅 http(s) 经白名单二次校验后外部打开。
                crate::utils::safe_open::open_url_safely(link.url.as_ref());
                true
            }
            HoveredLinkKind::OscHyperlink => {
                let lower = link.url.as_ref().to_ascii_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    crate::utils::safe_open::open_url_safely(link.url.as_ref());
                    return true;
                }
                // `file://` OSC8: fall through to OpenPath.
                if let Some(ref resolved) = link.resolved_path {
                    self.emit_open_path_for_link(
                        resolved.clone(),
                        link.line,
                        link.col,
                        link.ssh_config.clone(),
                        window,
                        cx,
                    );
                    return true;
                }
                // Unknown scheme: swallow the click so it
                // never starts a text selection.
                true
            }
            HoveredLinkKind::FilePath => {
                if let Some(ref resolved) = link.resolved_path {
                    self.emit_open_path_for_link(
                        resolved.clone(),
                        link.line,
                        link.col,
                        link.ssh_config.clone(),
                        window,
                        cx,
                    );
                }
                true
            }
        }
    }

    /// Hit-test a grid position while the link modifier is held.
    /// Priority: OSC8 hyperlink > URL regex > inferred file path.
    /// Each terminal read is scoped so the `&mut self` file-hit below never
    /// aliases a live borrow.
    fn hit_test_link(&mut self, row: usize, col: usize, cx: &App) -> Option<HoveredLink> {
        // 1. Explicit OSC8 hyperlink (highest priority).
        let osc = self
            .terminal
            .read(cx)
            .hyperlink_range_at_viewport_position(row, col);
        if let Some((uri, _id, start_col, end_col)) = osc
            && let Some(link) = self.hovered_for_osc_uri(&uri, row, col, start_col, end_col, cx)
        {
            return Some(link);
        }
        // Unknown scheme: fall through to URL/file detection on the
        // display text (the visible filename may still be clickable).
        // 2. Regex URL.
        let url = self
            .terminal
            .read(cx)
            .url_range_at_viewport_position(row, col);
        if let Some((url, start_col, end_col)) = url {
            return Some(HoveredLink {
                kind: HoveredLinkKind::Url,
                url,
                row,
                start_col,
                end_col,
                resolved_path: None,
                line: None,
                col: None,
                ssh_config: None,
            });
        }
        // 3. Inferred file path.
        let hit = self
            .terminal
            .read(cx)
            .file_hit_at_viewport_position(row, col)?;
        self.hovered_for_file_hit(&hit, row, cx)
    }

    fn hovered_for_osc_uri(
        &mut self,
        uri: &str,
        row: usize,
        col: usize,
        start_col: usize,
        end_col: usize,
        cx: &App,
    ) -> Option<HoveredLink> {
        let trimmed = uri.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Some(HoveredLink {
                kind: HoveredLinkKind::OscHyperlink,
                url: trimmed.to_string().into(),
                row,
                start_col,
                end_col,
                resolved_path: None,
                line: None,
                col: None,
                ssh_config: None,
            });
        }
        if lower.starts_with("file://") {
            // Display text may carry `:line:col` (e.g. `foo.rs:12` linked to
            // `file:///abs/foo.rs`); reuse the file detector for the suffix.
            let (line, col_num) = self
                .terminal
                .read(cx)
                .file_hit_at_viewport_position(row, col)
                .map(|h| (h.line, h.col))
                .unwrap_or((None, None));
            let is_remote = self.ssh_config.is_some();
            if is_remote {
                let remote_path = parse_file_uri_remote_path(trimmed)?;
                return Some(HoveredLink {
                    kind: HoveredLinkKind::OscHyperlink,
                    url: trimmed.to_string().into(),
                    row,
                    start_col,
                    end_col,
                    resolved_path: Some(remote_path),
                    line,
                    col: col_num,
                    ssh_config: self.ssh_config.clone(),
                });
            }
            let local_path = parse_file_uri_local_path(trimmed)?;
            // Local OSC8 file links point at absolute paths; still verify
            // existence so stale links never navigate (cached, see hover).
            if !self.cached_path_exists(&local_path) {
                return None;
            }
            return Some(HoveredLink {
                kind: HoveredLinkKind::OscHyperlink,
                url: trimmed.to_string().into(),
                row,
                start_col,
                end_col,
                resolved_path: Some(local_path.to_string_lossy().to_string()),
                line,
                col: col_num,
                ssh_config: None,
            });
        }
        None
    }

    fn hovered_for_file_hit(
        &mut self,
        hit: &crate::terminal::file_link_detector::FileHit,
        row: usize,
        cx: &App,
    ) -> Option<HoveredLink> {
        if let Some(ssh_config) = self.ssh_config.clone() {
            let cwd = self.current_remote_working_directory(cx);
            let resolved = crate::builtin_commands::PathResolver::resolve_remote(
                &hit.path,
                cwd.as_deref(),
            )?;
            // Hover shows immediately; existence is validated asynchronously
            // on click (SFTP round-trip must not block mouse-move).
            return Some(HoveredLink {
                kind: HoveredLinkKind::FilePath,
                url: hit.raw.clone().into(),
                row,
                start_col: hit.start_col,
                end_col: hit.end_col,
                resolved_path: Some(resolved),
                line: hit.line,
                col: hit.col,
                ssh_config: Some(ssh_config),
            });
        }
        let cwd = self.current_local_working_directory(cx);
        let ctx = crate::builtin_commands::LocalPathContext {
            working_directory: cwd.clone(),
            process_cwd: cwd,
        };
        let resolved =
            crate::builtin_commands::PathResolver::resolve_local(&hit.path, &ctx);
        // Existence check kills false positives (`ls` prose vs real files).
        // Both files and directories are clickable. Cached with a short TTL
        // so sweeping the mouse across one file doesn't stat per mousemove.
        if !self.cached_path_exists(&resolved) {
            return None;
        }
        Some(HoveredLink {
            kind: HoveredLinkKind::FilePath,
            url: hit.raw.clone().into(),
            row,
            start_col: hit.start_col,
            end_col: hit.end_col,
            resolved_path: Some(resolved.to_string_lossy().to_string()),
            line: hit.line,
            col: hit.col,
            ssh_config: None,
        })
    }

    /// Cached `metadata` existence check for link hover (short TTL, bounded).
    fn cached_path_exists(&mut self, path: &std::path::Path) -> bool {
        const TTL: Duration = Duration::from_secs(2);
        const MAX_ENTRIES: usize = 256;
        if let Some((exists, at)) = self.file_hover_stat_cache.get(path) {
            if at.elapsed() < TTL {
                return *exists;
            }
        }
        let exists = std::fs::metadata(path).is_ok();
        if self.file_hover_stat_cache.len() >= MAX_ENTRIES {
            self.file_hover_stat_cache.clear();
        }
        self.file_hover_stat_cache
            .insert(path.to_path_buf(), (exists, Instant::now()));
        exists
    }

    /// Emit `Event::OpenPath` for a clicked link. Remote paths are validated
    /// asynchronously (SFTP `metadata`) so a stale hover never opens an
    /// error tab; local paths were already validated on hover.
    fn emit_open_path_for_link(
        &self,
        resolved: String,
        line: Option<u32>,
        col: Option<u32>,
        ssh_config: Option<SshConnectionConfig>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ssh_config) = ssh_config else {
            let terminal_cwd = self
                .current_local_working_directory(cx)
                .map(|p| p.to_string_lossy().to_string());
            cx.emit(Event::OpenPath {
                path: resolved,
                line,
                col,
                ssh_config: None,
                terminal_cwd,
            });
            return;
        };
        // Remote: validate existence in the background, then emit.
        // NOTE: terminal_cwd is read inside the update below (at validation
        // completion time) so a `cd` during the SFTP round-trip still lands
        // the tree on the fresh cwd.
        cx.spawn(async move |this, cx| {
            let exists = cx
                .background_executor()
                .spawn({
                    let ssh_config = ssh_config.clone();
                    let resolved = resolved.clone();
                    async move {
                        use crate::file_manager::filesystem::FileSystem;
                        let backend = crate::file_manager::filesystem::RemoteFileSystem::new(
                            ssh_config,
                        );
                        backend
                            .metadata(&std::path::PathBuf::from(&resolved))
                            .is_ok()
                    }
                })
                .await;
            if !exists {
                log::warn!("Cmd+Click: remote path does not exist: {resolved:?}");
                return;
            }
            let _ = this.update(cx, |this, cx| {
                cx.emit(Event::OpenPath {
                    path: resolved,
                    line,
                    col,
                    ssh_config: Some(ssh_config),
                    terminal_cwd: this.current_remote_working_directory(cx),
                });
            });
        })
        .detach();
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// 生成用于 TabBar 的标签文本。
    /// 格式：
    /// - shell 在前台时：`shell_name: ~/shortened/cwd`
    /// - 有前台应用时：`process_name: ~/shortened/cwd`（若 cwd 获取不到则只显示进程名）
    pub fn tab_label(&self, cx: &App) -> String {
        let term = self.terminal.read(cx);

        // 远程终端：HostName - process: ~/cwd
        if term.is_remote() {
            let host = self
                .ssh_config
                .as_ref()
                .map(|c| c.nickname.clone().unwrap_or_else(|| c.host.clone()))
                .unwrap_or_else(|| self.title().to_string());
            let process = term.remote_process_name();
            let username = self.ssh_config.as_ref().map(|c| c.username.as_str());
            let cwd = self
                .current_remote_working_directory(cx)
                .map(|p| {
                    if let Some(user) = username {
                        let home_path = format!("/home/{}", user);
                        if p == home_path {
                            return "~".to_string();
                        }
                        let home_prefix = format!("{}/", home_path);
                        if let Some(rest) = p.strip_prefix(&home_prefix) {
                            return format!("~/{}", rest);
                        }
                    }
                    if p == "/home" { "/home".to_string() } else { p }
                })
                .unwrap_or_default();
            let mut parts = Vec::with_capacity(2);
            if let Some(proc) = process {
                parts.push(proc.to_string());
            }
            if !cwd.is_empty() {
                parts.push(cwd);
            }
            return if parts.is_empty() {
                host
            } else {
                format!("{} - {}", host, parts.join(": "))
            };
        }

        let process_name = term
            .foreground_process_name()
            .or_else(|| term.shell_name().map(|s| s.to_string()));

        // 优先使用缓存的 cwd（OSC 7 / 远程上报），fallback 到后台刷新的进程 cwd 缓存。
        // 不在标签/拖拽路径同步起 lsof（由 do_check_tab_label_changed 后台预热）。
        let cwd = self
            .cached_working_directory
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| term.reported_working_directory().map(PathBuf::from))
            .or_else(|| self.process_cwd.cwd.clone())
            .or_else(|| self.working_directory.clone());

        let cwd_display = cwd.as_ref().and_then(|p| {
            let path = p.to_string_lossy();
            let home = dirs::home_dir()?;
            let home_str = home.to_string_lossy();
            if path.starts_with(home_str.as_ref()) {
                Some(format!("~{}", &path[home_str.len()..]))
            } else {
                Some(path.to_string())
            }
        });

        match (process_name, cwd_display) {
            (Some(proc), Some(cwd)) => format!("{}: {}", proc, cwd),
            (Some(proc), None) => proc,
            (None, Some(cwd)) => cwd,
            (None, None) => "Local Terminal".to_string(),
        }
    }

    /// 检查 tab 标签是否发生变化，带防抖：首次立即计算，后续快速调用在间隔内只触发一次。
    fn check_tab_label_changed(&mut self, cx: &mut Context<Self>) {
        if self.tab_label_debounce_task.is_none() {
            self.do_check_tab_label_changed(cx);
            self.tab_label_debounce_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(TAB_LABEL_DEBOUNCE_DELAY)
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.tab_label_debounce_task = None;
                    if this.tab_label_pending {
                        this.tab_label_pending = false;
                        this.do_check_tab_label_changed(cx);
                    }
                });
            }));
            return;
        }
        self.tab_label_pending = true;
    }

    fn do_check_tab_label_changed(&mut self, cx: &mut Context<Self>) {
        // 先后台刷新进程 cwd 缓存（防抖后低频），标签计算只读缓存
        self.local_process_cwd(cx);
        let current = self.tab_label(cx);
        if self.last_tab_label.as_deref() != Some(&current) {
            self.last_tab_label = Some(current);
            cx.emit(Event::TitleChanged);
        }
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

    /// 推进 Kitty 动画帧，并在下一次该换帧时安排一次重绘。
    ///
    /// 每帧 `render` 调一次。约定：
    /// - 到点了（或还没有在途计划）才推进时钟；否则只等定时任务唤醒，
    ///   避免同一帧内对同一批动画反复 tick；
    /// - tick 后如果仍有动画在跑，用一次 `cx.spawn` 定时到调度器给的延迟上，
    ///   到点 `cx.notify()` 触发下一帧——这就是动画唯一的驱动力；
    /// - 没有动画在跑就什么都不做，渲染循环保持静止（不会空转）。
    fn tick_kitty_animations(&mut self, cx: &mut Context<Self>) {
        if self.kitty_animation_clock.is_pending() && !self.kitty_animation_clock.due() {
            return;
        }

        // 推进必须在终端实体上做：存储在里面，且 tick 会 bump 代际。
        let now_ms = self.kitty_animation_clock.now_ms();
        let delay = {
            let terminal = self.terminal.clone();
            terminal.update(cx, |terminal, _cx| terminal.animation_tick(now_ms))
        };
        self.kitty_animation_clock.schedule(delay);

        // 没有动画在跑就彻底停下：否则每帧都会重新调度一次空转。
        if delay.is_none() {
            self.kitty_animation_task = None;
            return;
        }

        // 已有一个在途任务就不重复 spawn（动画活跃时每帧都会走到这里）。
        if self.kitty_animation_task.is_some() {
            return;
        }

        let delay = self.kitty_animation_clock.time_until_due();
        self.kitty_animation_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                this.kitty_animation_task = None;
                cx.notify();
            })
            .ok();
        }));
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

    /// 停止拖拽选择的边缘自动滚动（取消周期任务并清除方向）。
    fn stop_selection_autoscroll(&mut self) {
        self.selection_autoscroll_task = None;
        self.selection_autoscroll_direction = None;
    }

    /// 结束左键拖拽选择（若正在进行）：清除拖拽状态、停止边缘自动滚动，
    /// 并把释放事件转发给终端（copy-on-select / mouse mode release 报文）。
    /// 返回是否确有进行中的拖拽被结束。
    ///
    /// 供两个释放路径共用：视图元素级 `on_mouse_up`（指针在视图内）与
    /// `TerminalElement::paint` 注册的窗口级监听器（指针在视图外时元素
    /// 级 handler 不会触发）。后者先触发并清除 `selecting`，前者随后
    /// 因守卫早退，不会重复调用 `mouse_up`。
    pub(crate) fn finish_mouse_selection(
        &mut self,
        event: &MouseUpEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.selecting {
            return false;
        }
        self.selecting = false;
        self.stop_selection_autoscroll();
        self.terminal.update(cx, |terminal, cx| {
            terminal.mouse_up(event, cx);
        });
        cx.notify();
        true
    }

    /// 拖拽选择期间由鼠标移动事件调用：根据指针 `position_y` 是否位于
    /// 视口上下边缘区域（约 1.5 行内），启动/停止边缘自动滚动任务。
    /// 鼠标静止在边缘时 mouse_move 不再触发，因此需要一个周期任务
    /// （而非仅在事件中滚动）才能持续滚动并延伸选区。
    fn update_selection_autoscroll(
        &mut self,
        position_y: Pixels,
        line_height: Pixels,
        viewport_height: Pixels,
        cx: &mut Context<Self>,
    ) {
        // +1 = 向上滚入 scrollback，-1 = 向下滚回最新内容，None = 不滚动。
        // （边界裁剪由 `Terminal::autoscroll_selection` 内部处理，滚到顶/底
        // 后 display_offset 不再变化。）
        let edge = line_height * SELECTION_AUTOSCROLL_EDGE_LINES;
        let direction = if position_y < edge {
            Some(1)
        } else if position_y > viewport_height - edge {
            Some(-1)
        } else {
            None
        };

        if direction == self.selection_autoscroll_direction {
            return;
        }
        self.selection_autoscroll_direction = direction;
        match direction {
            Some(_) => self.start_selection_autoscroll(cx),
            None => self.selection_autoscroll_task = None,
        }
    }

    /// 启动边缘自动滚动周期任务（先取消旧任务）。
    /// 方向由 `selection_autoscroll_direction` 字段提供，方向变化时
    /// `update_selection_autoscroll` 会重启本任务。
    fn start_selection_autoscroll(&mut self, cx: &mut Context<Self>) {
        self.selection_autoscroll_task = None;
        self.selection_autoscroll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(SELECTION_AUTOSCROLL_INTERVAL)
                    .await;
                // 返回值表示任务是否应继续运行：拖拽结束或方向被清除时停。
                match this.update(cx, |view, cx| {
                    let Some(direction) = view.selection_autoscroll_direction else {
                        return false;
                    };
                    if !view.selecting {
                        return false;
                    }
                    view.terminal.update(cx, |terminal, cx| {
                        terminal.autoscroll_selection(direction, cx);
                    });
                    cx.notify();
                    true
                }) {
                    Ok(true) => continue,
                    _ => break,
                }
            }
        }));
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
            .is_some_and(|s| !s.marked_text.is_empty())
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

    /// 内建命令缓冲内的光标字节偏移；无活跃输入时为 0（等同末尾）。
    pub fn builtin_command_cursor(&self) -> usize {
        self.builtin_command_manager.cursor().unwrap_or(0)
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
        let cursor_row_in_viewport = content.cursor.line.0.max(0) as f32 + visual_extra_rows as f32;
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
        let cursor = self.builtin_command_cursor();

        let anchor_byte = self
            .scp_path_anchor_byte(buffer, cursor)
            .or_else(|| self.generic_path_anchor_byte(buffer, cursor))
            .unwrap_or(0);
        // 防御：锚点必须落在字符边界上，否则切片 panic（应用崩溃）。
        // 正常路径下各 anchor 计算已保证边界，这里兜底避免未来回归。
        let mut anchor = anchor_byte.min(buffer.len());
        while anchor > 0 && !buffer.is_char_boundary(anchor) {
            anchor -= 1;
        }
        UnicodeWidthStr::width(&buffer[..anchor]) as f32
    }

    fn generic_path_anchor_byte(&self, buffer: &str, cursor: usize) -> Option<usize> {
        // 锚到光标所在参数 token 的目录前缀之后（与补全替换的起点一致）。
        let (token_start, token_end) =
            crate::builtin_commands::generic_arg_token_range_at(buffer, cursor);
        let token = &buffer[token_start..token_end];
        let (dir_part, _) = crate::builtin_commands::PathCompletionEngine::split_path_prefix(token);
        Some(token_start + dir_part.len().min(token.len()))
    }

    fn scp_path_anchor_byte(&self, buffer: &str, cursor: usize) -> Option<usize> {
        if buffer.split_whitespace().next()? != "/scp" {
            return None;
        }
        crate::builtin_commands::scp_completion::path_anchor_byte(buffer, cursor)
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
            let history = self.builtin_command_manager.history_matches().to_vec();
            let history_selected = self.builtin_command_manager.history_selected_index();
            let view_history = view.clone();

            crate::builtin_commands::render_path_overlay(
                &path_completions,
                selected_index,
                &history,
                history_selected,
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
                move |index, _window, cx| {
                    view_history.update(cx, |this, cx| {
                        this.mutate_builtin_command(cx, |manager| manager.pick_history(index));
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
            let history = self.builtin_command_manager.history_matches().to_vec();
            let history_selected = self.builtin_command_manager.history_selected_index();
            let view_history = view.clone();

            crate::builtin_commands::render_overlay(
                suggestions,
                selected_index,
                &history,
                history_selected,
                anchor,
                theme,
                is_dark,
                move |index, _window, cx| {
                    view.update(cx, |this, cx| {
                        this.mutate_builtin_command(cx, |manager| manager.pick(index));
                    });
                },
                move |index, _window, cx| {
                    view_history.update(cx, |this, cx| {
                        this.mutate_builtin_command(cx, |manager| manager.pick_history(index));
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
        let cursor_byte = self.builtin_command_cursor();
        let term = self.terminal.read(cx);
        let bounds = term.terminal_bounds;
        let cols = bounds.num_columns();
        let cursor_col = term.last_content().cursor.column.0;
        let wrapped = crate::terminal_view::overlay_wrap::wrap_overlay_text(
            builtin,
            ime,
            cursor_byte,
            cursor_col,
            cols,
        );
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
    /// 只有输入行仍为空（光标停在输入区起点）、shell 处于前台可接管状态、
    /// 且输入内容以 `/` 开头且不含换行时才会进入。空行判定由
    /// [`crate::terminal::Terminal::input_line_blank`] 完成（OSC 133;B 锚点 /
    /// 回显锚点），不再依赖按键历史启发式 flag。
    fn should_start_builtin_command(&self, text: &str, cx: &mut Context<Self>) -> bool {
        !self.builtin_command_manager.is_active()
            && text.starts_with('/')
            && !text.contains(['\n', '\r'])
            && self
                .terminal
                .update(cx, |terminal, _| terminal.input_line_blank())
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
    /// 清空意味着本面板的内建输入结束（Ctrl+C 取消 / 提交后清理等），
    /// 广播开启时同步发送 `BuiltinEnd`，让目标面板一并清空镜像输入。
    fn clear_builtin_command(&mut self, cx: &mut Context<Self>) {
        let was_active = self.builtin_command_manager.is_active();
        self.builtin_command_manager.clear();

        if was_active {
            if !self.broadcast_suppressed.get() {
                if let Some(ref callback) = self.broadcast_callback {
                    callback(BroadcastMsg::BuiltinEnd);
                }
            }
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
    /// 返回 `Some(result)` 表示该命令由应用接管（不应再回落到 shell）；
    /// 返回 `None` 表示未命中注册表，调用方应把原文交给 shell。
    fn execute_builtin_command(
        &mut self,
        command_line: &str,
        cx: &mut Context<Self>,
        mode: BuiltinExecMode,
    ) -> Option<CommandResult> {
        use crate::builtin_commands::executor::global_executor;
        use crate::builtin_commands::{ArgumentParser, CommandContext, global_registry};

        // 提取命令名和参数
        let command_name = command_line
            .split_whitespace()
            .next()
            .unwrap_or(command_line);
        let argument = ArgumentParser::extract_argument(command_line, command_name);

        // 在注册表中查找命令
        let command = global_registry().find(command_name)?;

        log::info!("builtin command {}: {}", command.name, command.description);

        // 查询 shell 进程的实时工作目录
        let (process_cwd, remote_cwd) = if self.ssh_config.is_some() {
            // 远程终端：通过 OSC 序列动态追踪
            let remote_cwd = self.current_remote_working_directory(cx);
            (None, remote_cwd)
        } else {
            // 本地终端：优先使用 OSC 7 上报的 cwd。OSC 7 来自 shell prompt hook，
            // 能实时反映 shell 内建 `cd` 后的目录；进程 cwd 查询只作为兼容兜底，
            // 因为某些平台/场景下拿到的可能是启动 shell 或前台程序的旧目录。
            self.local_process_cwd(cx); // 顺带后台刷新缓存
            let process_cwd = self.current_local_working_directory(cx);
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
            let executor = global_executor().lock();
            executor.execute(&command, argument, ctx)
        };

        match &result {
            Some(CommandResult::OpenPath {
                path,
                line,
                col,
                ssh_config,
                terminal_cwd,
            }) => {
                // 打开编辑器 / 文件树不是弹窗，广播接收方也按各自上下文打开
                cx.emit(Event::OpenPath {
                    path: path.clone(),
                    line: *line,
                    col: *col,
                    ssh_config: ssh_config.clone(),
                    terminal_cwd: terminal_cwd.clone(),
                });
            }
            Some(CommandResult::StartTransfer { request }) => {
                if mode == BuiltinExecMode::Interactive {
                    cx.emit(Event::StartTransfer {
                        request: request.clone(),
                    });
                } else {
                    // 静默（广播接收）模式：文件传输是**全局副作用**（工作区级任务），
                    // 而不是面板局部行为。若每个接收面板都各自启动一次，同一份文件
                    // 会被并发传 N 次；并发写同一目标路径还可能损坏文件。
                    // 因此传输只由发起面板执行一次，接收面板不重复执行。
                    log::info!(
                        "Broadcast /scp suppressed on receiving panel (transfer executes once on the origin panel)"
                    );
                }
            }
            Some(CommandResult::Log { .. }) => {
                // LogOnly 命令不需要发射事件
            }
            Some(CommandResult::None) => {}
            Some(CommandResult::Error { message }) => {
                if mode == BuiltinExecMode::Interactive {
                    log::warn!("Command error: {}", message);
                    self.show_toast(message, cx);
                } else {
                    // 静默模式：仅记录日志，不弹 Toast
                    log::info!("Broadcast builtin command error (suppressed): {}", message);
                }
            }
            Some(CommandResult::FallThrough { shell_command }) => {
                // Handler 做完副作用 / 预处理，把（原始或改写过的）文本交给 shell
                let to_send = shell_command
                    .clone()
                    .unwrap_or_else(|| command_line.to_string());
                if mode == BuiltinExecMode::Interactive {
                    self.send_command_line_to_shell(&to_send, cx);
                } else if self.can_trigger_builtin_command_here(cx) {
                    // 静默模式：直接写本机 PTY，不走 write_with_broadcast，防止再次广播形成回环；
                    // 仅在 shell 处于前台时写入，避免注入正在运行的全屏程序
                    let mut data = to_send.as_bytes().to_vec();
                    data.push(b'\n');
                    self.terminal.update(cx, |terminal, _| {
                        terminal.write(&data);
                    });
                }
            }
            None => {}
        }

        result
    }

    /// 提交一条已经确定好的命令行。
    ///
    /// 如果不是应用内建命令，就把它按普通 shell 输入发回终端。
    fn submit_resolved_builtin_command(&mut self, command_line: String, cx: &mut Context<Self>) {
        self.finish_builtin_command_input(cx);

        let result = self.execute_builtin_command(&command_line, cx, BuiltinExecMode::Interactive);

        // 记录到命令历史（仅首 token 命中注册表的内建命令），供下次召回整行。
        // 只在执行成功时记录：失败的命令（如未知主机）召回出来只会再次失败。
        if crate::builtin_commands::find_builtin_command(&command_line).is_some()
            && !matches!(
                result,
                Some(crate::builtin_commands::CommandResult::Error { .. })
            )
        {
            crate::builtin_commands::history::record_command(&command_line);
        }

        match result {
            // 未命中注册表：整行回落到 shell（内部经 write_with_broadcast 广播原始输入）
            None => {
                self.send_command_line_to_shell(&command_line, cx);
            }
            // FallThrough 已在 execute_builtin_command 内部回落到 shell 并完成原始广播
            Some(CommandResult::FallThrough { .. }) => {}
            // 应用层命令已接管：把整行命令广播给组内其他面板，让面板局部命令（如 /open）
            // 在各自上下文静默执行。注意：全局副作用命令（如 /scp 的文件传输）在接收面板
            // 会被抑制（见 execute_builtin_command 的 StartTransfer 分支），只由本面板执行一次。
            // 另外不能走 Raw 广播 —— 内建命令文本从未进入 PTY，其他 shell 拿到它毫无意义。
            Some(_) => {
                if let Some(ref callback) = self.broadcast_callback {
                    callback(BroadcastMsg::BuiltinCommand(command_line));
                }
            }
        }
    }

    /// 解析并提交当前内建命令缓冲。
    fn submit_builtin_command(&mut self, force_selected: bool, cx: &mut Context<Self>) {
        let Some(command_line) = self.builtin_command_submission_text(force_selected) else {
            return;
        };

        self.submit_resolved_builtin_command(command_line, cx);
    }

    /// 广播接收方入口：以静默模式执行一条内建命令。
    ///
    /// 各面板用自己的 cwd / ssh 上下文解析并执行（命令历史、Toast、冲突弹窗等
    /// UI 副作用只在发起面板发生）；**全局副作用（/scp 的文件传输）会被抑制**，
    /// 只由发起面板执行一次。若本面板正处于内建命令输入中，先清空其缓冲。
    pub fn execute_broadcast_command(&mut self, command_line: &str, cx: &mut Context<Self>) {
        // 接收方执行不产生任何广播：清缓冲的 BuiltinEnd 已由发起面板发出，
        // 这里再发会造成冗余消息甚至回环。用守卫确保即使在 panic 时也复位标志。
        let _guard = BroadcastSuppressionGuard::new(&self.broadcast_suppressed);
        if self.builtin_command_manager.is_active() {
            self.finish_builtin_command_input(cx);
        }
        self.execute_builtin_command(command_line, cx, BuiltinExecMode::Silent);
    }

    /// 广播接收方入口：把发起面板的内建命令缓冲（文本 + 光标 + 浮层折叠）镜像到本面板。
    ///
    /// 仅在 shell 处于前台可接管输入时应用；应用期间抑制再次广播，避免回环。
    /// 补全列表由本面板按自己的 cwd / ssh 上下文独立计算（remote 目录按需异步加载）。
    pub fn apply_broadcast_builtin_input(
        &mut self,
        text: &str,
        cursor: usize,
        dismissed: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.can_trigger_builtin_command_here(cx) {
            return;
        }
        // 作用域守卫：镜像期间抑制再次广播避免回环，panic 时自动复位。
        let _guard = BroadcastSuppressionGuard::new(&self.broadcast_suppressed);
        self.mutate_builtin_command(cx, |manager| {
            manager.set_buffer(text, cursor);
            if dismissed && manager.is_active() {
                // 镜像发起面板的 ESC 折叠状态，保持浮层可见性一致
                manager.dismiss();
            }
        });
    }

    /// 广播接收方入口：同步清空本地镜像的内建输入。
    ///
    /// 对应发起面板的 Ctrl+C 取消 / 提交后清理；应用期间抑制广播避免回环。
    pub fn apply_broadcast_builtin_end(&mut self, cx: &mut Context<Self>) {
        if !self.builtin_command_manager.is_active() {
            return;
        }
        // 作用域守卫：清理镜像期间抑制广播避免回环，panic 时自动复位。
        let _guard = BroadcastSuppressionGuard::new(&self.broadcast_suppressed);
        self.finish_builtin_command_input(cx);
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
        // 同步"本机端 cwd"：本地终端用 shell 进程实时 cwd（后台刷新的缓存，
        // 不在按键路径同步起 lsof），远程终端用 DeepTerm 进程的初始工作目录。
        // 补全引擎拿到的 cwd 必须是当前终端 shell 的实际位置。
        let local_cwd = if self.ssh_config.is_some() {
            self.working_directory.clone()
        } else {
            self.local_process_cwd(cx)
                .or_else(|| self.working_directory.clone())
        };
        self.builtin_command_manager.set_local_cwd(local_cwd);

        // 注入"当前终端"的连接上下文：
        // - 本地终端 → ssh = None
        // - 远程终端 → ssh = Some(...)，remote_cwd 取 OSC 7 上报的实时路径
        // 这样 `/scp` 没有显式 host 的 token 会跟随当前终端所在主机
        // （远程终端走 SFTP 列远端目录，本地终端列本机文件）。
        let terminal_remote_cwd = if self.ssh_config.is_some() {
            self.current_remote_working_directory(cx)
        } else {
            None
        };
        self.builtin_command_manager
            .set_terminal_context(self.ssh_config.clone(), terminal_remote_cwd);

        // 进入 /scp 之前先把保存的 SSH 连接注入管理器，供 host / 远端目录补全使用。
        // 调用点可能很多（每次输入字符），读盘解析 ssh.json 的开销不可接受；
        // 用短 TTL 缓存：连续输入期间不重复读盘，新建/修改的连接至多 5 秒后可见。
        let should_refresh_hosts = self
            .builtin_command_manager
            .query()
            .map(|q| q.trim_start().starts_with("/scp"))
            .unwrap_or(false);
        if should_refresh_hosts {
            const SCP_HOSTS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);
            // 需要重读：从未加载过，或缓存已过 TTL
            let needs_reload = self
                .scp_hosts_cache
                .as_ref()
                .map(|(loaded_at, _)| loaded_at.elapsed() >= SCP_HOSTS_CACHE_TTL)
                .unwrap_or(true);
            if needs_reload {
                let configs = crate::ssh::config::SshGlobalConfig::load().saved_connections;
                self.builtin_command_manager
                    .set_ssh_configs(configs.clone());
                self.scp_hosts_cache = Some((std::time::Instant::now(), configs));
            }
        }

        // 注入命令历史快照（内存克隆，不读盘），供补全弹窗召回历史整行。
        // `mutate_builtin_command` 只在内建命令上下文被调用，故无条件注入即可；
        // 注入须早于 `f()`，这样 `f()` 内部的 refresh_state 能用最新历史算 matches。
        self.builtin_command_manager
            .set_history(crate::builtin_commands::history::snapshot());

        f(&mut self.builtin_command_manager);

        // 远端目录按需加载：如果管理器标记为"loading"，取出请求并后台读取
        while let Some(request) = self.builtin_command_manager.pending_remote_request() {
            self.spawn_remote_dir_load(request, cx);
        }

        // 广播缓冲最新状态（文本 / 光标 / 浮层折叠），让广播组内其他面板镜像显示
        // 同一段输入。应用远端同步（broadcast_suppressed）时不广播，防止回环。
        if !self.broadcast_suppressed.get() {
            if let Some(ref callback) = self.broadcast_callback {
                callback(BroadcastMsg::BuiltinSync {
                    text: self.builtin_command_text().to_string(),
                    cursor: self.builtin_command_cursor(),
                    dismissed: self.builtin_command_manager.is_dismissed(),
                });
            }
        }

        cx.notify();
    }

    /// 后台读取远端目录并把结果回写到 `BuiltinCommandManager`。
    ///
    /// 请求自带连接配置快照（见 `RemoteCompletionRequest`），不再回读磁盘上的
    /// 保存配置：当前终端的连接可能未保存/已被改名，只靠 id 查表会永远查不到。
    fn spawn_remote_dir_load(
        &self,
        request: crate::builtin_commands::RemoteCompletionRequest,
        cx: &mut Context<Self>,
    ) {
        let config = request.config;
        let abs_dir = request.absolute_dir;
        let cfg_id = config.id();

        cx.spawn(async move |this, cx| {
            let abs_dir_task = abs_dir.clone();
            let config_task = config.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::ssh::sftp_fs::with_cached_session_once(&config_task, |session| {
                        let effective = if abs_dir_task == "~" {
                            session
                                .canonicalize(".")
                                .or_else(|_| session.canonicalize("~"))
                                .unwrap_or_else(|_| "/".to_string())
                        } else if let Some(rest) = abs_dir_task.strip_prefix("~/") {
                            let home = session
                                .canonicalize(".")
                                .or_else(|_| session.canonicalize("~"))
                                .unwrap_or_else(|_| "/".to_string());
                            crate::ssh::sftp_fs::join_remote_path(&home, rest)
                        } else {
                            abs_dir_task.clone()
                        };
                        let entries = session.read_dir(&effective);
                        Ok((effective, entries))
                    })
                    .unwrap_or_else(|err| (abs_dir_task, Err(err)))
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
                Err(err) => Err(err.to_string()),
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

    // ── Scrollback search ─────────────────────────────────────────────────

    /// 打开搜索浮层：创建（或复用）输入框并聚焦。
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.active {
            // 已激活：仅重新聚焦输入框
            if let Some(input) = &self.search_input {
                input.update(cx, |input, cx| input.focus(window, cx));
            }
            return;
        }

        self.search.active = true;
        if self.search_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索终端内容...")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
                    .clean_on_escape()
            });
            let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        this.schedule_search_recompute(cx);
                    }
                    InputEvent::Focus | InputEvent::Blur => {
                        // 聚焦态变化需要重绘（边框高亮在 render 时直接向 window 查询）
                        cx.notify();
                    }
                    _ => {}
                }
            });
            self._search_input_subscription = Some(subscription);
            self.search_input = Some(input);
        }
        if let Some(input) = &self.search_input {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        // 再次打开：输入框里还留着上次的词，立即重算命中。
        let has_text = self
            .search_input
            .as_ref()
            .is_some_and(|input| !input.read(cx).value().is_empty());
        if has_text {
            self.start_search_from_scratch(cx);
        }
        cx.notify();
    }

    /// 关闭搜索浮层：清高亮、还焦点、保留输入词（再次打开时可继续）。
    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.active {
            return;
        }
        self.search.active = false;
        self.search.matches.clear();
        self.search.active_index = 0;
        // 作废在飞的后台任务：Task drop 只是 detach，回写靠 request id 失效。
        self.invalidate_search_request();
        self.search_task = None;
        // 恢复终端焦点（搜索框抢占了 focus handle）
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// 终端内容变了就重算命中集（render 每帧调用一次）。
    ///
    /// 命中存的是 grid 绝对坐标，新输出 / scrollback 环形滚动会让它们整体漂移，
    /// 不重算则高亮画在错行、跳转也错行。持续输出时这里会被节流到
    /// [`SEARCH_DRIFT_THROTTLE_MS`]，效果等于「停手 150ms（防抖）后才算一次」。
    fn maybe_schedule_search_for_content_change(&mut self, cx: &mut Context<Self>) {
        if !self.search.active || self.search.query.is_empty() {
            return;
        }
        let generation = self.terminal.read(cx).content_generation;
        let throttled = self
            .search_last_schedule
            .is_some_and(|t| t.elapsed() < Duration::from_millis(SEARCH_DRIFT_THROTTLE_MS));
        if generation != self.search_generation && !throttled {
            self.search_last_schedule = Some(Instant::now());
            self.schedule_search_recompute(cx);
        }
    }

    /// 自增搜索请求序号，使已发出的后台任务在回写时被判定为过期。
    fn invalidate_search_request(&mut self) {
        self.search_request_id = self.search_request_id.wrapping_add(1);
    }

    /// 调度一次命中重算（150ms 防抖）：
    /// 主线程抓 grid 快照 → **后台线程**比对 → 回主线程落地。
    ///
    /// 两个触发点：
    /// - 输入框内容变化（`InputEvent::Change`）；
    /// - 终端内容代次变化：新输出 / scrollback 环形滚动会让命中集里的
    ///   grid 绝对坐标整体漂移，不重算就会高亮错行、跳转错行。
    ///
    /// 持续输出时每帧都会重调度，等效于「停手 150ms 后才算一次」。
    fn schedule_search_recompute(&mut self, cx: &mut Context<Self>) {
        let query = self
            .search_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();

        // 空词：直接清空，无需惊动后台线程
        if query.trim().is_empty() {
            self.invalidate_search_request();
            self.search_task = None;
            self.search.query.clear();
            self.search.matches.clear();
            self.search.active_index = 0;
            self.search_generation = self.terminal.read(cx).content_generation;
            cx.notify();
            return;
        }

        self.invalidate_search_request();
        let request_id = self.search_request_id;
        let query_for_apply = query.clone();
        let terminal = self.terminal.clone();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SEARCH_DEBOUNCE_MS))
                .await;

            // 让位给更新的请求：旧任务被 drop 只是 detach，仍会跑到这里，
            // 先确认自己还是「最新一个」再干活，避免重复全量扫描。
            let still_current = this
                .update(cx, |this, _| this.search_request_id == request_id)
                .unwrap_or(false);
            if !still_current {
                return;
            }

            // 快照必须在主线程抓（要持 term 锁），代价是一次 O(字符数) 拷贝；
            // 代次也要在抓快照的同一时刻读，才能准确反映「这批命中对应哪一版内容」。
            let Ok((snapshot, snapshot_generation)) = this.update(cx, |_, cx| {
                let terminal = terminal.read(cx);
                (terminal.search_snapshot(), terminal.content_generation)
            }) else {
                return;
            };

            // 折叠 + 滑动窗口比对是整段扫描里最贵的部分，放后台线程。
            let matches = cx
                .background_spawn(async move {
                    crate::terminal::terminal::search_matches_in_snapshot(&snapshot, &query)
                })
                .await;

            this.update(cx, |this, cx| {
                // 过期结果（期间又有新请求 / 搜索已关闭）直接丢弃。
                if this.search_request_id != request_id {
                    return;
                }
                this.apply_search_matches(query_for_apply, matches, snapshot_generation, cx);
            })
            .ok();
        }));
    }

    /// 落地后台算出的命中集：尽量保持当前命中的相对位置，并把它滚到视口中央。
    fn apply_search_matches(
        &mut self,
        query: String,
        matches: Vec<crate::terminal::terminal::SearchMatch>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.search.query = query.trim().to_string();
        let previous = self.search.active_match().copied();
        self.search.matches = matches;
        self.search.active_index = match previous {
            // 重算（终端输出导致坐标漂移）：按「同列优先、行号最近」找回旧命中，
            // 否则用户正在看的命中会被拽到别处。
            Some(prev) => self
                .search
                .matches
                .iter()
                .enumerate()
                .min_by_key(|(_, m)| {
                    (
                        m.start.column.0.abs_diff(prev.start.column.0),
                        (m.start.line.0 - prev.start.line.0).abs(),
                    )
                })
                .map(|(idx, _)| idx)
                .unwrap_or(0),
            // 首次搜索：从当前视口附近开始
            None => self.pick_initial_match_index(cx),
        };
        self.search_generation = generation;

        // 命中变化后把当前命中滚到视口中央
        if let Some(m) = self.search.active_match() {
            let line = m.start.line;
            self.terminal.update(cx, |terminal, _| {
                terminal.scroll_line_to_center(line);
            });
        }
        cx.notify();
    }

    /// 首次命中的选择：视口内第一个 → 视口下方最近 → （全在上方时）上方最近。
    ///
    /// 早先实现在没有「视口下方命中」时退回 index 0（最旧的一条），会把视图
    /// 一路滚回几千行之前；这里改为就近优先。
    fn pick_initial_match_index(&self, cx: &App) -> usize {
        let matches = &self.search.matches;
        if matches.is_empty() {
            return 0;
        }
        let content = self.terminal.read(cx).last_content();
        let top = -(content.display_offset as i32);
        let bottom = top + content.rows as i32 - 1;
        matches
            .iter()
            .position(|m| (top..=bottom).contains(&m.start.line.0))
            .or_else(|| matches.iter().position(|m| m.start.line.0 > bottom))
            .unwrap_or(matches.len() - 1)
    }

    /// 跳到下一个 / 上一个命中（wrap-around），并把该命中滚到视口中央。
    fn search_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.search.matches.is_empty() {
            return;
        }
        let len = self.search.matches.len();
        self.search.active_index = if forward {
            (self.search.active_index + 1) % len
        } else {
            (self.search.active_index + len - 1) % len
        };
        if let Some(m) = self.search.active_match() {
            let line = m.start.line;
            self.terminal.update(cx, |terminal, _| {
                terminal.scroll_line_to_center(line);
            });
        }
        cx.notify();
    }

    /// 打开搜索浮层时，用输入框里的既有词算一次（走同一条防抖 + 后台链路）。
    pub fn start_search_from_scratch(&mut self, cx: &mut Context<Self>) {
        self.schedule_search_recompute(cx);
    }

    /// 渲染搜索 UI：右上角搜索框（无外层浮层背景）。
    ///
    /// 样式对齐侧边栏 Host 面板的搜索框：32px 高、圆角 12、聚焦 3px accent 发光描边。
    /// 布局：搜索图标 → 输入区（transparent EditorInput）→ 命中计数 → 关闭钮。
    ///
    /// 按键：Enter / Down = 下一个命中，Shift-Enter / Up = 上一个，
    /// Escape 关闭（经 DismissSearch action，仅搜索激活时消费）。
    /// 这些按键在容器层拦截（on_key_down + stop_propagation），
    /// 不会下漏到终端输入。
    fn render_search_bar(&self, is_focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let match_count = self.search.matches.len();
        let counter: SharedString = if match_count == 0 {
            "0".into()
        } else {
            format!("{}/{}", self.search.active_index + 1, match_count).into()
        };
        let input = self
            .search_input
            .clone()
            .expect("search input must exist while search is active");
        div()
            .absolute()
            .top(px(6.0))
            .right(px(10.0))
            .id("terminal-search")
            .flex()
            .items_center()
            .gap(px(6.0))
            // 样式对齐侧边栏 Host 面板搜索框：32px 高、圆角 12、
            // 未聚焦无描边，聚焦时 3px accent 描边（发光式外圈高亮）。
            .h(px(32.0))
            .w(px(260.0))
            .px(px(10.0))
            .rounded(px(12.0))
            .bg(theme.tab_hover_background)
            .when(is_focused, |this| {
                this.border(px(3.0)).border_color(theme.accent.opacity(0.5))
            })
            .on_key_down(cx.listener(
                |this, event: &gpui::KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    let mods = &event.keystroke.modifiers;
                    let plain = !mods.control && !mods.platform && !mods.alt;
                    match key {
                        // Enter / Down（无修饰）：下一个命中
                        "enter" | "return" if plain && !mods.shift => {
                            this.search_step(true, cx);
                            cx.stop_propagation();
                        }
                        // Shift-Enter / Up：上一个
                        "enter" | "return" if plain && mods.shift => {
                            this.search_step(false, cx);
                            cx.stop_propagation();
                        }
                        "down" if plain => {
                            this.search_step(true, cx);
                            cx.stop_propagation();
                        }
                        "up" if plain => {
                            this.search_step(false, cx);
                            cx.stop_propagation();
                        }
                        "escape" => {
                            this.close_search(window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                },
            ))
            // 搜索图标（灰，仅装饰）
            .child(
                gpui::svg()
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted)
                    .path(Icon::MagnifyingGlass.path()),
            )
            // 输入区：transparent EditorInput 占满剩余宽度，无内建背景/高度
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .items_center()
                    .child(EditorInput::new(&input).small().transparent()),
            )
            // 命中计数：紧凑灰字，宽度固定避免输入时跳动
            .child(
                div()
                    .flex_none()
                    .min_w(px(28.0))
                    .text_size(px(11.0))
                    .text_color(if match_count == 0 {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .text_align(gpui::TextAlign::Right)
                    .child(counter),
            )
            // 关闭按钮：终端 tab 同款 XmarkCircleFill 图标
            .child(
                gpui::svg()
                    .size(px(14.0))
                    .flex_none()
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .path(Icon::XmarkCircleFill.path())
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _event, window, cx| {
                            this.close_search(window, cx);
                        }),
                    ),
            )
            .into_any_element()
    }

    /// BEL 到达：
    /// - 视区短促视觉闪烁（限频，防频闪）；
    /// - 视图未聚焦时点亮所属 tab 的红点（BellRangWhileUnfocused）。
    fn on_bell(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        // 限频：150ms 内的连环铃只闪一次。
        if self
            .last_bell_at
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(150))
        {
            self.last_bell_at = Some(now);
            return;
        }
        self.last_bell_at = Some(now);

        self.bell_flash_at = Some(now);
        self.bell_flash_task = None;
        self.bell_flash_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(BELL_FLASH_MS))
                .await;
            this.update(cx, |this, cx| {
                this.bell_flash_at = None;
                this.bell_flash_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();

        if !self.was_focused {
            cx.emit(Event::BellRangWhileUnfocused);
        }
    }

    /// OSC 9 / 777 通知到达：信息型 Toast + 未聚焦时点亮 tab 红点。
    /// 通知不触发 BEL 闪烁（Toast 本身已是视觉反馈）。
    fn on_notification(&mut self, title: String, body: String, cx: &mut Context<Self>) {
        self.info_toast = Some((title, body));
        self.info_toast_hide_task = None;
        self.info_toast_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(5))
                .await;
            this.update(cx, |this, cx| {
                this.info_toast = None;
                this.info_toast_hide_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();

        if !self.was_focused {
            cx.emit(Event::BellRangWhileUnfocused);
        }
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

    /// 渲染信息型 Toast（OSC 9/777 终端通知）：顶部居中的通知卡片。
    fn render_info_toast(&self, title: &str, body: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();
        let title_shared: SharedString = title.to_string().into();
        let body_shared: SharedString = body.to_string().into();
        let body_element = if body.is_empty() {
            None
        } else {
            Some(div().text_size(px(12.0)).text_color(theme.text_muted).child(body_shared))
        };

        div()
            .absolute()
            .top(px(8.0))
            .left(px(0.0))
            .right(px(0.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_w(px(400.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(theme.elevated_background.opacity(if is_dark { 0.92 } else { 0.97 }))
                    .border_1()
                    .border_color(theme.border_strong)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                gpui::svg()
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(theme.text_muted)
                                    .path(crate::assets::Icon::Bell.path()),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .text_ellipsis()
                                    .child(title_shared),
                            ),
                    )
                    .when_some(body_element, |this, body| this.child(body)),
            )
            .into_any_element()
    }

    /// 结束当前内建命令输入，恢复普通终端输入状态。
    fn finish_builtin_command_input(&mut self, cx: &mut Context<Self>) {
        self.clear_marked_text(cx);
        self.clear_builtin_command(cx);
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

            // ── 光标编辑组合键 ───────────────────────────────────────────────
            // macOS 习惯：Opt+←/→ 按词移动、Opt+Backspace 删词、Cmd+←/→ 跳行首尾。
            // 仅在「无 Ctrl」时启用，避免与上面的 Ctrl 导航冲突。
            if !modifiers.control && !modifiers.function {
                // Cmd（platform）+ ←/→：跳到行首 / 行尾
                if modifiers.platform {
                    match key {
                        "left" => {
                            self.mutate_builtin_command(
                                cx,
                                BuiltinCommandManager::move_cursor_home,
                            );
                            return true;
                        }
                        "right" => {
                            self.mutate_builtin_command(cx, BuiltinCommandManager::move_cursor_end);
                            return true;
                        }
                        _ => {}
                    }
                }
                // Opt（alt）+ ←/→：按词移动；Opt+Backspace：删词
                if modifiers.alt {
                    match key {
                        "left" => {
                            self.mutate_builtin_command(
                                cx,
                                BuiltinCommandManager::move_cursor_word_left,
                            );
                            return true;
                        }
                        "right" => {
                            self.mutate_builtin_command(
                                cx,
                                BuiltinCommandManager::move_cursor_word_right,
                            );
                            return true;
                        }
                        "backspace" => {
                            self.mutate_builtin_command(
                                cx,
                                BuiltinCommandManager::delete_word_backward,
                            );
                            return true;
                        }
                        _ => {}
                    }
                }
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
                true
            }
            "delete" => {
                self.mutate_builtin_command(cx, BuiltinCommandManager::delete_forward);
                true
            }
            "left" => {
                self.mutate_builtin_command(cx, BuiltinCommandManager::move_cursor_left);
                true
            }
            "right" => {
                self.mutate_builtin_command(cx, BuiltinCommandManager::move_cursor_right);
                true
            }
            "home" => {
                self.mutate_builtin_command(cx, BuiltinCommandManager::move_cursor_home);
                true
            }
            "end" => {
                self.mutate_builtin_command(cx, BuiltinCommandManager::move_cursor_end);
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
                    // 选中历史项 ⇒ 回填整行后收起浮层，等待用户再次回车提交（不 auto-advance）。
                    if self
                        .builtin_command_manager
                        .history_selected_index()
                        .is_some()
                    {
                        self.mutate_builtin_command(cx, BuiltinCommandManager::complete_history);
                    } else {
                        let is_arg = mgr.is_argument_input();
                        self.mutate_builtin_command(cx, |manager| {
                            if is_arg {
                                manager.complete_path();
                            } else {
                                manager.complete();
                            }
                            manager.advance_after_pick();
                        });
                    }
                } else if overlay_visible_with_results {
                    // B：用户没选任何项，只是想关掉浮层
                    // 但如果缓冲区第一个 token 精确匹配了某个注册命令，应该提交执行
                    let first_token = self
                        .builtin_command_manager
                        .query()
                        .and_then(|q| q.split_whitespace().next());
                    let is_exact_command = first_token
                        .and_then(|name| crate::builtin_commands::global_registry().find(name))
                        .is_some();
                    if is_exact_command {
                        self.submit_builtin_command(false, cx);
                    } else {
                        self.mutate_builtin_command(cx, BuiltinCommandManager::hide_overlay);
                    }
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
            callback(BroadcastMsg::Raw(data.to_vec()));
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

        self.terminal.update(cx, |terminal, _| {
            let bracketed = terminal
                .last_content()
                .mode
                .contains(TermMode::BRACKETED_PASTE);

            // 统一走粘贴清洗：过滤 ESC 等控制字符，防止拖拽文件名/恶意
            // 文本中的转义序列被终端当作输入执行
            let filtered = crate::terminal::terminal::sanitize_pasted_text(&text, bracketed);

            if bracketed {
                // 使用 bracketed paste 模式（单次写入，避免定界符与内容被拆开）
                let mut data = Vec::with_capacity(filtered.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(filtered.as_bytes());
                data.extend_from_slice(b"\x1b[201~");
                terminal.write(&data);
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

            // 粘贴前清洗：非 bracketed 模式剔除 ESC/控制字符，
            // bracketed 模式剔除粘贴定界符（防止提前结束粘贴模式而执行后续文本）
            let sanitized = crate::terminal::terminal::sanitize_pasted_text(&text, bracketed);

            if bracketed {
                // Use bracketed paste mode to preserve whitespace
                let mut data = Vec::with_capacity(sanitized.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(sanitized.as_bytes());
                data.extend_from_slice(b"\x1b[201~");
                self.write_with_broadcast(&data, cx);
            } else {
                // Normal mode
                self.write_with_broadcast(sanitized.as_bytes(), cx);
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
        self.terminal.update(cx, |terminal, cx| {
            terminal.select_all(cx);
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
                callback(BroadcastMsg::Raw(bytes.clone()));
            }
        }

        if handled {
            // 空行 / 前台判定已由 Terminal::write 内的输入区锚点统一维护：
            // 回车写入后锚点自动失效（提示符将重绘），其余输入的回显会推动
            // 光标离开锚点。这里不再需要按按键历史猜测 shell 行是否为空。
            cx.stop_propagation();
        }
        // Otherwise, let the event propagate to EntityInputHandler
        // which will handle regular character input (including uppercase letters)
    }
}

crate::impl_focusable!(TerminalView);

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
        self.ime_state.as_ref()?;
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

/// BEL 视觉闪烁层：视区四周一层强调色描边，透明度随剩余时间线性衰减。
///
/// 颜色用终端前景色（跟随主题），不额外引入语义色，深浅主题下均可见。
fn render_bell_flash(started: Instant, cx: &App) -> gpui::AnyElement {
    let theme = cx.theme();
    let terminal_theme = theme.terminal;
    let progress = (started.elapsed().as_secs_f32()
        / Duration::from_millis(BELL_FLASH_MS).as_secs_f32())
        .clamp(0.0, 1.0);
    let alpha = (1.0 - progress) * 0.55;
    if alpha <= 0.01 {
        return div().into_any_element();
    }

    let color = terminal_theme.foreground.opacity(alpha);
    div()
        .absolute()
        .inset_0()
        .border_2()
        .border_color(color)
        .rounded(px(4.0))
        .into_any_element()
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self.terminal.clone();
        let focus_handle = self.focus_handle.clone();
        let view_entity = cx.entity().clone();
        let is_focused = focus_handle.is_focused(window);

        // ── 主题色同步（OSC 10/11/12/4 查询闭环）───────────────────────────
        // 首帧与主题切换时把当前主题灌入 Term.colors：查询应答与渲染回退
        // 均以此为准（应用改色写入 Term.colors 的实时生效也经由渲染层快照）。
        {
            let active_theme_id = crate::theme::ThemeState::global(cx)
                .active_theme()
                .id
                .clone();
            if self.applied_theme_id.as_ref() != Some(&active_theme_id) {
                let theme = crate::theme::ThemeState::global(cx).active_theme().terminal;
                self.terminal.update(cx, |terminal, _| {
                    terminal.apply_theme_colors(&theme);
                });
                self.applied_theme_id = Some(active_theme_id);
            }
        }

        // ── 搜索命中集跟随内容变化 ──────────────────────────────────────────
        // 命中存的是 grid 绝对坐标，终端一输出（新行进入 scrollback）就会整体
        // 漂移，不重算则高亮画在错行、跳转也错行。持续输出时这里会反复重调度，
        // 效果等于「停手 150ms 后才真正算一次」。
        self.maybe_schedule_search_for_content_change(cx);

        // React to focus edges so we can start/stop blinking.
        self.on_focus_changed(is_focused, cx);
        let cursor_visible = self.should_show_cursor(is_focused, cx);
        let show_scrollbar = self.should_show_scrollbar(cx);

        // ── Kitty 动画帧推进 ──────────────────────────────────────────────────
        // 唯一驱动源：GPUI 不每 vsync 重绘，所以这里既推进动画、也安排下一次
        // 唤醒。只有"确实到换帧点"才重新调度 `cx.notify()`，静止的动画不会
        // 让渲染循环空转。换帧会 bump 图像代际，元素 prepaint 的
        // `take_kitty_damage()` 随后认出来并全量重绘。
        self.tick_kitty_animations(cx);

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

        let terminal_mouse_mode = self.terminal.read(cx).mouse_mode(false);

        // 终端进入 mouse mode 时，鼠标事件已经由 ncurses、vim、tmux、lazygit 等 TUI 应用接管，
        // 此时用户在屏幕上的拖拽并不是 DeepTerm 本地文本选择，而是需要原样转发给远端应用的终端鼠标协议。
        // 参考 alacritty 的策略，这种场景下应显示普通箭头光标，而不是文本输入区域常用的 IBeam：
        // 1. 箭头能更准确表达“当前由 TUI 应用处理鼠标交互”，避免误导用户以为是在选择宿主应用文本；
        // 2. macOS 上一些全局划词/翻译工具会把 IBeam + 拖选识别为普通文本选择，并监听或改写系统剪切板；
        // 3. OSC52 复制本身由远端 TUI 通过转义序列完成，若外部工具在拖选期间介入，会造成 DeepTerm 写入成功但 Cmd+V 得到旧内容。
        // 因此只有在非 mouse mode、由 DeepTerm 自己负责本地终端选区时，才继续显示 IBeam。
        let cursor_style = if self.hovered_link.is_some() {
            CursorStyle::PointingHand
        } else if terminal_mouse_mode {
            CursorStyle::Arrow
        } else {
            CursorStyle::IBeam
        };

        div()
            .id("terminal-view")
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(CONTEXT)
            .cursor(cursor_style)
            .on_action(cx.listener(|this, _: &Copy, _window, cx| {
                this.copy(cx);
            }))
            .on_action(cx.listener(|this, _: &Paste, _window, cx| {
                this.paste(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _window, cx| {
                this.select_all(cx);
            }))
            .on_action(cx.listener(|this, _: &StartSearch, window, cx| {
                this.open_search(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DismissSearch, window, cx| {
                // 仅在搜索激活时消费 Escape；否则显式 propagate，
                // 让按键继续走 on_key_down → to_esc_str → PTY。
                // GPUI 的 action 在 bubble 阶段默认不再传播，
                // 不调 propagate() 会吞掉 ESC，TUI 应用永远收不到 \x1b。
                if this.search.active {
                    this.close_search(window, cx);
                    cx.stop_propagation();
                } else {
                    cx.propagate();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    #[cfg(target_os = "macos")]
                    let is_cmd = event.modifiers.platform;
                    #[cfg(not(target_os = "macos"))]
                    let is_cmd = event.modifiers.control;

                    if is_cmd {
                        // 按点击坐标实时重算，不依赖 hover 缓存：
                        // 按住修饰键后不移动鼠标直接点击、或点击前终端有新输出
                        // 滚动时，缓存的行列可能为空或已指向别的文本。
                        if let Some(link) = this.hover_at_position(event.position, cx) {
                            if this.activate_hovered_link(&link, window, cx) {
                                return;
                            }
                        } else if this.hovered_link.take().is_some() {
                            cx.notify();
                        }
                    }

                    this.selecting = true;
                    // 新一次按下：重置上一次拖拽残留的自动滚动状态，
                    // 避免旧任务在新选区开始时继续滚动。
                    this.stop_selection_autoscroll();
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_down(event, cx);
                    });
                    cx.notify();
                }),
            )
            // 中键/右键：转发给终端（mouse mode 下应用程序需要收到这些事件，
            // 例如 vim/tmux 的中键粘贴、less 的右键菜单）。本地模式下中键
            // 粘贴剪贴板、右键无默认行为。不参与文本选择，不动 `selecting`。
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_down(event, cx);
                    });
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_down(event, cx);
                    });
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    // 视图内释放时，TerminalElement::paint 注册的全局释放
                    // 监听器（bubble 阶段先于本 handler 触发）通常已处理过
                    // 并清掉了 `selecting`；这里退化为兜底路径。
                    this.finish_mouse_selection(event, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_up(event, cx);
                    });
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_up(event, cx);
                    });
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

                // Link hover detection (only when modifier is held).
                // Priority: OSC8 hyperlink > URL regex > inferred file path.
                // `hover_at_position` 负责修饰键按下沿的 ranges 激活与 hover 刷新。
                if modifiers_pressed {
                    this.hover_at_position(event.position, cx);
                } else if this.hovered_link.take().is_some() {
                    // 修饰键松开：清除悬停（hover_at_position 只负责置位）。
                    this.terminal.update(cx, |terminal, _cx| {
                        terminal.set_link_hover_active(false);
                    });
                    cx.notify();
                }

                // 元素级 `on_mouse_up` 仅在指针位于本元素内时触发：若左键
                // 在视图外释放（例如拖到侧边栏/标签栏后松开），释放事件不会
                // 送达，拖拽状态会泄漏——后续悬停移动会无按键地继续扩展
                // 选区（幽灵拖拽）。此处用 `pressed_button` 检测并就地收尾。
                if this.selecting && event.pressed_button != Some(MouseButton::Left) {
                    this.selecting = false;
                    this.stop_selection_autoscroll();
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
                // 拖拽时检测指针是否在视口上下边缘区域，启动/停止边缘
                // 自动滚动任务（鼠标静止在边缘时也能持续滚动并延伸选区）。
                if this.selecting {
                    let (position_y, line_height, viewport_height) = {
                        let term = this.terminal.read(cx);
                        let bounds = term.terminal_bounds;
                        let position = event.position - bounds.bounds.origin;
                        (position.y, bounds.line_height, bounds.bounds.size.height)
                    };
                    this.update_selection_autoscroll(position_y, line_height, viewport_height, cx);
                }
                // Only request a repaint when something visible changes.
                // - Link hover changes are already notified above (`link_changed`).
                // - Mouse-mode moves forward bytes to the PTY; the application's own
                //   redraw output drives the subsequent repaint, so no notify is needed.
                // - Active drag selection needs an immediate repaint to update the
                //   selection highlight as the cursor moves.
                if this.selecting {
                    cx.notify();
                }
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
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                // Clear the hovered-link highlight when the pointer leaves the
                // terminal. `on_mouse_move` only fires inside this element, so
                // without this the underline would stay stuck until the next
                // in-view mouse move (e.g. after moving to the sidebar while
                // holding Cmd).
                if !*hovered && this.hovered_link.take().is_some() {
                    this.link_modifier_held = false;
                    this.terminal.update(cx, |terminal, _cx| {
                        terminal.set_link_hover_active(false);
                    });
                    cx.notify();
                }
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
                    // OSC 9/777 信息型 Toast（与错误 Toast 分层：通知场景优先展示）
                    .when_some(self.info_toast.clone(), |this, (title, body)| {
                        this.child(self.render_info_toast(&title, &body, cx))
                    })
                    // 回滚区搜索浮层（右上角）
                    .when(self.search.active, |this| {
                        // 直接向 window 查询输入框聚焦态（事件链可能丢帧，这里每次渲染都为准）
                        let is_focused = self
                            .search_input
                            .as_ref()
                            .is_some_and(|input| {
                                input.read(cx).focus_handle(cx).is_focused(window)
                            });
                        this.child(self.render_search_bar(is_focused, cx))
                    })
                    // BEL 视觉闪烁：视区边缘一层随时间衰减的强调色描边。
                    .when_some(
                        self.bell_flash_at
                            .filter(|t| t.elapsed() < Duration::from_millis(BELL_FLASH_MS)),
                        |this, started| this.child(render_bell_flash(started, cx)),
                    )
                    // SSH 连接进度视图覆盖层 - 在连接建立期间覆盖终端内容
                    .when_some(self.ssh_connect_view.clone(), |this, connect_view| {
                        this.child(div().absolute().inset_0().child(connect_view))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;
    use crate::terminal::terminal::TerminalBounds;
    use gpui::{TestAppContext, size};

    /// 搜索全链路（输入 → 后台比对 → 回写 → 内容漂移后重算）。
    ///
    /// 覆盖点：
    /// - 命中集在**后台线程**算出并正确回写（而不是卡在主线程同步扫描）；
    /// - 终端继续输出导致 grid 坐标漂移后，重算能把命中带到新的行号上
    ///   （这是命中集必须跟随内容代次重算的原因）。
    #[gpui::test]
    fn search_runs_in_background_and_follows_content_drift(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (view, cx) = cx.add_window_view(|_window, cx| {
            let terminal = Terminal::new_headless(cx, 1_000);
            TerminalView::build_headless(cx, terminal)
        });

        // headless 默认 bounds 行数列数不直观，显式指定 40×10 便于推算漂移。
        let bounds = TerminalBounds::new(
            px(16.0),
            px(9.0),
            Bounds::new(point(px(0.0), px(0.0)), size(px(360.0), px(160.0))),
        );
        // 灌入内容：先填满一屏（10 行）把 hello 挤进 scrollback，
        // 否则内容不滚动，坐标也就无从漂移。
        let mut boot = String::new();
        for i in 0..60 {
            boot.push_str(&format!("filler line {i}\r\n"));
        }
        boot.push_str("hello world\r\n");
        let boot = boot.into_bytes();

        // 灌入内容 + 打开搜索浮层 + 填入查询词
        view.downgrade()
            .update_in(cx, |view, window, cx| {
                view.terminal.update(cx, |terminal, cx| {
                    terminal.resize(bounds, 1.0, cx);
                    terminal.handle_remote_data(&boot, cx);
                });
                view.open_search(window, cx);
                let input = view.search_input.clone().expect("搜索输入框应已创建");
                input.update(cx, |input, cx| input.set_value("hello", window, cx));
                view.start_search_from_scratch(cx);
            })
            .unwrap();

        // 防抖 + 后台比对 + 回写：推进时钟直到任务全部落地
        cx.background_executor
            .advance_clock(Duration::from_millis(SEARCH_DEBOUNCE_MS * 2));
        cx.run_until_parked();

        let first = view.read_with(cx, |view, _| {
            (view.search.query.clone(), view.search.matches.clone())
        });
        assert_eq!(first.0, "hello");
        assert_eq!(first.1.len(), 1, "后台比对应命中 1 处");
        let initial_line = first.1[0].start.line;

        // 追加 3 行输出：旧内容整体上移，坐标必须跟着漂移
        view.downgrade()
            .update_in(cx, |view, _window, cx| {
                view.terminal.update(cx, |terminal, cx| {
                    for _ in 0..3 {
                        terminal.handle_remote_data(b"more output\r\n", cx);
                    }
                });
                // 模拟 render 每帧的代次检查（清掉节流标记以便立即调度）
                view.search_last_schedule = None;
                view.maybe_schedule_search_for_content_change(cx);
            })
            .unwrap();
        cx.background_executor
            .advance_clock(Duration::from_millis(SEARCH_DEBOUNCE_MS * 2));
        cx.run_until_parked();

        let recomputed = view.read_with(cx, |view, _| view.search.matches.clone());
        assert_eq!(recomputed.len(), 1, "重算后仍应命中 1 处");
        assert_ne!(
            recomputed[0].start.line, initial_line,
            "内容漂移后命中必须落到新的 grid 行，否则高亮会画错行"
        );
    }
}
