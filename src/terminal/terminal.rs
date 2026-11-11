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
    time::{Duration, Instant},
};

use alacritty_terminal::{
    Term,
    event::{Event as AlacTermEvent, EventListener, WindowSize},
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
// 本地终端（含 Windows 的 conpty）统一走自建 `local_pty::LocalReader`，
// 不再构造 alacritty 的 EventLoop——理由见 `local_pty` 模块文档。
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, Context, Entity, EventEmitter, Pixels, Point,
    SharedString, Size, Task, Window, px,
};

use crate::agent::{AgentEvent, AgentSessionState, CLIAgent};
use crate::ssh::{RusshTerminal, SshConnectionConfig, SshPtyTerminal, platform::detect_local_os};
use crate::ssh::{
    keychain::{PreferredSshTransport, preferred_ssh_transport},
    platform::RemoteOS,
};
use crate::terminal::file_link_detector::FileHit;
use crate::terminal::kitty::scanner::ReplySink;
use crate::terminal::kitty::storage::Storage as KittyStorage;
use crate::terminal::mappings::mouse::{
    alt_scroll, grid_point, grid_point_and_side, mouse_button_report, mouse_moved_report,
    scroll_report,
};

/// 最大滚动缓冲区行数限制
const MAX_SCROLLBACK_LINES: usize = 100000;

/// 未配置时默认的 scrollback 行数（与旧行为一致）。
const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// 单终端 scrollback 的物理字节预算。
///
/// alacritty 按“行”限额，宽窗口（250 列）会线性放大到 60 MB+；这里再按
/// `32 + 24*cols` 折算行数，把每个终端的回滚内存压到约 32 MB 以内。
/// 只缩不涨：窗口变窄时不重新分配，避免一次性申请几千行造成 RSS 尖峰。
const SCROLLBACK_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// 按列数把“请求行数”折算到字节预算内。
fn scrollback_lines_for_cols(requested: usize, cols: usize) -> usize {
    let per_row = 32usize.saturating_add(cols.saturating_mul(24));
    let by_budget = SCROLLBACK_BUDGET_BYTES / per_row.max(1);
    requested.min(by_budget).min(MAX_SCROLLBACK_LINES)
}

/// 从设置读取请求的 scrollback 行数（设置未初始化时用默认值）。
fn settings_scrollback_lines(cx: &App) -> usize {
    cx.try_global::<crate::settings::SettingsStore>()
        .and_then(|store| store.get::<crate::settings::content::TerminalContent>())
        .map(|content| content.scrollback_lines)
        .unwrap_or(DEFAULT_SCROLLBACK_LINES)
        .min(MAX_SCROLLBACK_LINES)
}

/// 渲染侧连续跳过多少次网格锁之后，退回公平锁（见 `update_last_content`）。
///
/// 取 4：足够吸收「读线程正在喂一个 chunk」的短暂窗口（一跳就是一帧），
/// 又不至于让持续大流量把画面永久饿住——超过这个次数说明锁确实被长期占用，
/// 此时排在 lease 后面的有界等待，比继续丢帧更划算。
pub(crate) const MAX_SNAPSHOT_LOCK_SKIPS: u32 = 4;

/// 跳过快照刷新后，隔多久主动补一帧（见 `sync_inner`）。
///
/// 跳过本身只在「恰好还有下一帧」时才安全：`sync` 的唯一调用点是元素
/// prepaint，所以**必须有人再排一帧**，否则最后一次唤醒（读线程退出前的
/// drain、SSH 断连、`ChildExit`）若撞上被占住的锁，`content_dirty` 会永久
/// 置位、画面停在旧内容上，直到用户碰鼠标。窗口内重绘不可靠（`notify` 与
/// `refresh` 在 prepaint 期间都不排队新帧），所以用一个短定时器兜底。
const SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(8);

/// resize 合并间隔（1:1 移植 ghostty `termio/Thread.zig` 的 `Coalesce.min_ms = 25`）。
///
/// ghostty 语义：定时器活动期间只更新最新值、**不重置定时器**（节流而非防抖），
/// 持续拖动期间以稳定 ~40Hz 应用；应用时 pty、网格、渲染尺寸三者一次到位，
/// 渲染层在两次应用之间始终使用「已应用」的尺寸。
const RESIZE_COALESCE_DELAY: Duration = Duration::from_millis(25);

/// 用户按下打断键后，等多久才允许推断「回合已结束」。
///
/// 多数 agent 在 <kbd>Esc</kbd> / <kbd>Ctrl+C</kbd> 取消回合时不发 hook
/// （Claude 的 `Stop` 明确跳过用户中断），但少数会——Kimi 的 `Interrupt`、
/// 与按键赛跑的 `Stop`——而且都远在这个窗内。没有这个宽限，那些 agent 的
/// 回合会被过早判成结束，然后被自己随后的事件翻回去，图标抖一下。
pub const AGENT_INTERRUPT_SETTLE: Duration = Duration::from_millis(1000);

/// 一个回合可以多久不出任何 hook 事件，之后放弃它、状态落回 `Idle`。
///
/// 取 30 分钟：远大于任何真实回合的安静段（Claude 最长的单次工具调用上限
/// 十分钟），而每个工具跑完、每次权限询问都会把计时重置。它的作用是兜住
/// 「`Stop` 丢了」——否则状态点会永远停在蓝色。
pub const AGENT_STALE_AFTER: Duration = Duration::from_secs(30 * 60);

/// 连续这么久没有 PTY 输出，就不再算「正在输出」。
///
/// 它与 [`crate::agent::AgentStatus::Working`] **不是一回事**：`Working` 是
/// agent 上报的回合状态（一次工具调用可以安静跑几分钟），而这里描述的是
/// 「屏幕上正在滚字」。tab 的扫光动画只该代表后者——它对应用户真正想知道的
/// 「它还在干活吗」，也让 `Stop` 丢失时（旧版 opencode 桥接就没映射回合结束
/// 事件）动画不至于一直转到 30 分钟的静默兜底才停。
///
/// 取 800ms：TUI 的流式输出远密于此；而思考/等工具时通常整秒没有字节。
pub const AGENT_STREAM_WINDOW: Duration = Duration::from_millis(800);

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
    /// 端口转发的建立结果：`message` 是给用户看的一行说明（逐条摘要，或失败原因），
    /// `ok` 表示没有失败项。逐条独立——一条失败不影响其它条。
    ///
    /// 注意：系统 ssh 路径（`-R`）拿不到远端确认，那里的 `ok: true` 只表示
    /// 「已随连接请求转发」，不代表绑定成功——文案里已如实说明。
    PortForward {
        message: String,
        ok: bool,
    },
}

/// 一次本地前台 agent 检测的结果。
///
/// 存在的意义是把「查不了」和「查了、但不是 agent」分开——这两件事都会让
/// `detect_agent_for_local` 拿不到 `CLIAgent`，但对界面意味着相反的动作。
/// 折叠成 `Option` 会逼调用方猜，猜错的代价是图标乱闪或该消失时不消失。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAgentProbe {
    /// 前台进程的 argv（探测到的话）。`None` = 没探到，调用方保持现有快照。
    ///
    /// 与 `agent` 分开：`agent` 是「argv 里认出了 agent」，而标签上的程序方块
    /// 需要的是完整 argv——`vim`、`node .../vite`、未知脚本都得能显示。
    pub argv: Option<Vec<String>>,
    /// 探测结论：[`LocalAgentProbe::Agent`] 认出了 agent，[`LocalAgentProbe::NoAgent`]
    /// 是有效结论（前台确实不是 agent），[`LocalAgentProbe::Unavailable`] 不是结论。
    pub outcome: LocalAgentOutcome,
}

/// [`LocalAgentProbe`] 的三态结论（历史名字保留，见 [`LocalAgentProbe`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalAgentOutcome {
    /// 认出来了：前台跑的就是这个 agent。
    Agent(CLIAgent),
    /// **有效结论**：拿到了前台进程的 argv，但没有任何 token 命中已知 agent
    /// （shell 提示符、`vim`、`grep`…）。agent 已经不在前台了。
    NoAgent,
    /// **不是结论**：缺 fd、`tcgetpgrp` 失败、取不到 argv。调用方应当保持现状。
    Unavailable,
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
    /// 前台 CLI agent 的身份或回合状态发生变化（见 [`Terminal::foreground_agent`]
    /// 与 [`Terminal::agent_session`]）。
    ///
    /// 有意不带载荷：订阅方通过 `term.read(cx)` 现场取状态（`tab_label` 读
    /// `remote_process_name` 就是这个模式），把状态塞进变体会复制一份并迫使
    /// 每次 emit 都 `Clone`。与既有的 `TitleChanged` 等保持一致。
    AgentChanged,
    /// 前台进程组可能换了人：shell 刚画完提示符（OSC 133;A/B）或刚收回前台。
    ///
    /// 这是给**本地** agent 探测用的触发信号，载荷无关，订阅方收到即重查
    /// （见 `TerminalView::refresh_foreground_agent`）。它替代的是纯 TTL 轮询
    /// 等满 2 秒的那段等待：agent 画完首屏就坐下等按键时不再有输出，TTL
    /// 轮询永远等不到下一个 Wakeup。
    ///
    /// 只由 OSC 133 分支发：那是 shell 集成标记「命令结束、提示符重绘」的
    /// 唯一时刻，与「agent 刚退出、shell 回到前台」严格对应。远程路径同样
    /// 会收到它（远程集成 shell 也发 133;A/B），但远程检测走
    /// `1337;Proc=`，早退。
    ForegroundGroupChanged,
    /// 端口转发的建立结果到达（见 [`Terminal::port_forward_status`]）。
    PortForwardChanged,
}

/// 端口转发的建立结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForwardStatus {
    /// 给用户看的一行说明（逐条摘要，或失败原因）。
    pub message: String,
    /// 是否没有失败项（系统 ssh 路径拿不到远端确认，`true` 仅表示「已随
    /// 连接请求转发」，见 [`DriverEvent::PortForward`]）。
    pub ok: bool,
}

#[derive(Clone)]
pub struct ZedListener(pub UnboundedSender<AlacTermEvent>);

/// 远程数据驱动事件通道。
///
/// 有界（async_channel）：SSH 读循环在 UI 消费不过来时通过 `send().await`
/// 挂起，背压经 SSH 窗口传导到远端，避免无界队列在 UI 卡顿时无限膨胀。
#[derive(Clone)]
pub struct DriverListener(pub async_channel::Sender<DriverEvent>);

/// Kitty 图形协议可占用的图像内存上限（128 MB，十进制）。
///
/// ghostty 的 `image-storage-limit` 默认值是 `320 * 1000 * 1000`，这里**故意
/// 取更小**：ghostty 上传 GPU 后即释放 CPU 像素（`renderer/image.zig` 的
/// `upload` 会 `deinit` 掉 pending 数据），常驻只有 1 份 CPU + 可见图的 GPU
/// 纹理；而 deepterm 同一张图常驻约 2 份 CPU 拷贝（`Storage` 的 RGBA +
/// `ImageCache` 里 `RenderImage` 的 BGRA）再加 GPU 纹理。按 ~2.5 倍系数折算，
/// 128 MB 的实际效果约等于 ghostty 默认 320 MB 的内存占用。
///
/// 单张图上限另有 [`crate::terminal::kitty::image`] 的 `MAX_IMAGE_BYTES`
/// 把关（64 MB，不会单张吃掉整个预算）；这里是**全部图像合计**，超限按
/// 优先级淘汰。
const KITTY_IMAGE_LIMIT: usize = 128 * 1000 * 1000;

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

/// PowerShell 的 cwd 同步脚本（注入背景见 [`powershell_cwd_sync_args`]）。
///
/// 只用单引号、不含双引号：本地终端 `escape_args == false`，参数会被
/// alacritty 的 `cmdline()` 原样拼进 `CreateProcess` 命令行，外层双引号
/// 由注入函数自己补，内层也就没有可转义的对象。
const POWERSHELL_CWD_SYNC_SCRIPT: &str = "$global:__dt_op = $function:prompt; function global:prompt { try { if ($PWD -and $PWD.Provider.Name -eq 'FileSystem') { [System.IO.Directory]::SetCurrentDirectory($PWD.Path) } } catch {}; if ($global:__dt_op) { & $global:__dt_op } else { 'PS ' + $PWD.Path + '> ' } }";

/// Windows 下给 PowerShell 注入 cwd 同步：命中返回注入后的参数，否则返回
/// [`None`]（调用方保持原样）。
///
/// 背景：windows 上本地终端的 cwd 唯一来源是进程 PEB 轮询
/// （见 `crate::utils::win_process_cwd`）——OSC 7 走不通：alacritty 的
/// 事件循环独占字节流、没有 `set_current_dir` 处理，PowerShell 自己也不
/// 发 OSC 7。而 PowerShell 的 `Set-Location` 只改 `$PWD`、**不回写进程
/// 当前目录**（cmd 的 `cd` 会），于是 PEB 永远停在启动目录：cd 之后 tab
/// 标签与侧边栏文件树都停在旧路径。修复方式是补 `-NoExit -Command
/// <脚本>`，把 `prompt` 包一层，每次渲染提示符时用 `$PWD` 回写进程当前
/// 目录（`[System.IO.Directory]::SetCurrentDirectory` 即 Win32
/// `SetCurrentDirectoryW`，同步更新 PEB 的 `CurrentDirectory.DosPath`，
/// 已在真实子进程上实测）。
///
/// 包 `prompt` 而不是 `Set-Location`：cd / Push-Location / Pop-Location /
/// 脚本内切目录，最终都会走到下一次提示符渲染，一处覆盖全部；包 cmdlet
/// 只能覆盖用户直接调用的那几条。
///
/// 已实测的两个顺序前提：`powershell -Command` 时 profile 先加载（我们的
/// 包装随后覆盖 `prompt`，并把用户的原提示符函数存进
/// `$global:__dt_op` 继续调用）；非文件系统 provider（`Cert:` 等）与已
/// 删除目录被 try/catch 吞掉，不会打断提示符。
fn powershell_cwd_sync_args(program: &str, args: &[String]) -> Option<Vec<String>> {
    // 仅 windows：unix 的 cwd 由 OSC 7 主动上报，不存在本 bug；不引入
    // 超出「Windows 兼容」范围的既有行为变更。
    if !cfg!(windows) {
        return None;
    }
    let name = local_shell_name(program)?;
    if !matches!(name, "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe") {
        return None;
    }
    // 用户已经给了 `-Command` / `-EncodedCommand` / `-File` 时不打扰：
    // 再追加一份 `-Command` 不是报错就是改变用户意图。
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-Command" | "-EncodedCommand" | "-File"))
    {
        return None;
    }
    let mut args = args.to_vec();
    args.push("-NoExit".to_string());
    args.push("-Command".to_string());
    args.push(format!("\"{POWERSHELL_CWD_SYNC_SCRIPT}\""));
    Some(args)
}

fn configure_local_shell_integration(
    program: String,
    mut args: Vec<String>,
    env: &mut HashMap<String, String>,
) -> tty::Shell {
    let Some(shell_name) = local_shell_name(&program) else {
        return tty::Shell::new(program, args);
    };

    // PowerShell 的 cwd 同步只依赖启动参数、不依赖下面要写的资源文件，
    // 所以放在资源文件写入之前：写入失败时 zsh/bash 退回默认行为可以
    // 接受，PowerShell 若跟着退回就又回到「cd 不跟随」的本 bug 了。
    if let Some(synced) = powershell_cwd_sync_args(&program, &args) {
        return tty::Shell::new(program, synced);
    }

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
        // 行尾归一化：Windows（core.autocrlf=true）下 `include_str!` 嵌入的
        // 脚本可能带 CRLF，本地 git-bash 等 Un*x shell 会报
        // `$'\r': command not found`（与远端 bootstrap 同一问题）。
        let content = crate::utils::shell::normalize_line_endings(content);
        // 脚本只允许属主读写（0600），防止被同机其他用户篡改
        #[cfg(unix)]
        {
            use std::io::Write as _;
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
        fs::write(&path, content.as_bytes()).ok()?;
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
/// 把本地路径编码成 `file://` URI（供 DnD `text/uri-list` 使用）。
fn file_uri(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for byte in text.bytes() {
        let keep =
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

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
    /// Current selection range, resolved while the term lock is already held
    /// in `update_last_content`. Read by the render hot path so it does not
    /// need to acquire the `FairMutex<Term>` a second time per frame.
    pub selection_range: Option<alacritty_terminal::selection::SelectionRange>,
    /// 当前生效调色板快照（主题基色 + 应用 OSC 4/10/11/12 改色叠加）。
    /// 见 [`crate::terminal::mappings::colors::TerminalPalette`]。
    pub palette: crate::terminal::mappings::colors::TerminalPalette,
    /// 调色板内容指纹；行缓存复用必须同时比对它，否则 OSC 改色后
    /// 会复用旧颜色。
    pub palette_hash: u64,
    /// 每个可见行的内容指纹（不含行号/坐标），供渲染层在全量 damage 时
    /// 复用未变化的行缓存。长度恒等于 `row_ranges`。
    pub row_hashes: Vec<u64>,
    /// 本帧内容相对上一帧的滚动位移：`new[i] == old[i + scroll_shift]`。
    /// 0 表示没有可识别的整体滚动（清屏/重绘/resize 等）。
    pub scroll_shift: isize,
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
            selection_range: None,
            palette: Default::default(),
            palette_hash: 0,
            row_hashes: Vec::new(),
            scroll_shift: 0,
        }
    }
}

/// FNV-1a 混合一个 u64。
#[inline]
fn hash_mix(hash: &mut u64, value: u64) {
    *hash ^= value;
    *hash = hash.wrapping_mul(0x100_0000_01b3);
}

/// 把 `Color` 的判别式与负载混进指纹（vte 的 Color 没有实现 Hash）。
#[inline]
fn hash_color(hash: &mut u64, color: alacritty_terminal::vte::ansi::Color) {
    use alacritty_terminal::vte::ansi::Color;
    match color {
        Color::Named(named) => {
            hash_mix(hash, 0);
            hash_mix(hash, named as u32 as u64);
        }
        Color::Spec(rgb) => {
            hash_mix(hash, 1);
            hash_mix(
                hash,
                u64::from(u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)),
            );
        }
        Color::Indexed(index) => {
            hash_mix(hash, 2);
            hash_mix(hash, u64::from(index));
        }
    }
}

/// 一行可见内容的指纹：只覆盖影响排版/配色的字段，不含行号与坐标。
///
/// `hyperlink` 用 Arc 指针做身份：滚动的行是同一批 `IndexedCell` 的拷贝，
/// Arc 不变；真正换了链接时指针会变，正是我们要的“内容变了”。
fn row_content_hash(cells: &[IndexedCell]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for cell in cells {
        let packed = &cell.cell;
        hash_mix(&mut hash, packed.c as u64);
        hash_color(&mut hash, packed.fg);
        hash_color(&mut hash, packed.bg);
        hash_mix(&mut hash, u64::from(packed.flags.bits()));
        match packed.underline_color {
            Some(color) => {
                hash_mix(&mut hash, 1);
                hash_color(&mut hash, color);
            }
            None => hash_mix(&mut hash, 0),
        }
        match cell.zerowidth {
            Some(chars) => {
                hash_mix(&mut hash, 1);
                for ch in chars {
                    hash_mix(&mut hash, ch as u64);
                }
            }
            None => hash_mix(&mut hash, 0),
        }
        match &cell.hyperlink {
            Some(link) => hash_mix(&mut hash, link.uri().as_ptr() as usize as u64),
            None => hash_mix(&mut hash, 0),
        }
    }
    hash
}

/// 在旧/新两帧的行指纹里找整体滚动位移。
///
/// 命中条件：至少 3/4 的行满足 `new[i] == old[i + shift]`。滚动 1 行时
/// 59 行里通常有 58 行命中；清屏/全量重绘则接近 0，退回 0（不复用）。
fn detect_scroll_shift(old: &[u64], new: &[u64]) -> isize {
    let rows = old.len();
    if rows < 4 || new.len() != rows {
        return 0;
    }
    let threshold = (rows * 3).div_ceil(4);
    let matches_at = |shift: isize| {
        let mut matches = 0usize;
        for (index, new_hash) in new.iter().enumerate() {
            let source = index as isize + shift;
            if source >= 0 && (source as usize) < rows && *new_hash == old[source as usize] {
                matches += 1;
            }
        }
        matches
    };
    // 常见场景是滚动 1~2 行，按 |shift| 递增搜索并在首次达到 3/4 阈值时
    // 返回即可。旧的 O(rows²) 全量扫描每次 Full damage（滚动/批量输出）
    // 都要把 2×rows−1 个候选全部算一遍。
    if matches_at(0) >= threshold {
        return 0;
    }
    for distance in 1..rows {
        let shift = distance as isize;
        if matches_at(shift) >= threshold {
            return shift;
        }
        if matches_at(-shift) >= threshold {
            return -shift;
        }
    }
    0
}

/// 软换行链一次最多拼多少行（见 [`Terminal::link_row_chain`]）。
///
/// 上限由「向上回溯」决定：`cat 巨大单行文件` 会让几百上千行都带 `WRAPLINE`，
/// 无上限回溯等于每次 hover 都重扫整段输出。URL/路径长到这个量级也不像人手输
/// 的，撞到上限就当没有链接 —— 只会「不认」，不会「认错」。
const MAX_LINK_CHAIN_ROWS: usize = 64;

/// 一次命中测试的完整结果：三种链接各自在**被点行**上的命中。
///
/// hover 的优先级（OSC8 > URL > 文件路径）由消费方决定；一次性给出全部
/// 候选，好让一次鼠标移动只重建一次软换行链（见
/// [`Terminal::link_hits_at_viewport_position`]）。
#[derive(Clone, Debug, Default)]
pub struct LinkHits {
    /// OSC8 显式链接：`(uri, id, start_col, end_col)`。
    pub hyperlink: Option<(String, String, usize, usize)>,
    /// 正则识别出的 URL：`(url, start_col, end_col)`。
    pub url: Option<(SharedString, usize, usize)>,
    /// 推断出的文件路径候选（含 `:line:col` 拆分结果）。
    pub file: Option<FileHit>,
}

/// 被点行所在的软换行链：`clicked_row` 是被点行在 `rows` 里的下标。
///
/// 行按文本顺序排列：视口内的行借用 `last_content` 的切片，链首越过视口
/// 顶端时补进来的 scrollback 行只能自持（见
/// [`Terminal::scrollback_rows_above_viewport`]）。
struct LinkChain<'a> {
    clicked_row: usize,
    rows: Vec<Cow<'a, [IndexedCell]>>,
}

impl LinkChain<'_> {
    /// `detect_*_in_rows` 消费的行切片迭代器。
    fn row_slices(&self) -> impl Iterator<Item = &[IndexedCell]> + '_ {
        self.rows.iter().map(|row| &row[..])
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

/// 视口快照里的一个单元格。
///
/// `PartialEq` 是给平价测试用的：增量路径与整体重建必须逐格完全一致，
/// 包括 `zerowidth`（Kitty `U=1` 占位符的行/列编码）与 `hyperlink`。
/// 手写比对容易漏掉后来新增的字段，整格比较则不会。
#[derive(Clone, Debug, PartialEq)]
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
        let guard = crate::terminal::kitty::storage::lock_unpoisoned(&self.storage);
        f(&guard)
    }

    /// 可变访问存储。只给 UI 线程上的操作使用（如推进动画帧）——读线程
    /// 永远只读，因此这里的可变借用不会与渲染路径冲突。同样勿在其中取
    /// `term` 锁。
    pub fn with_storage_mut<R>(&self, f: impl FnOnce(&mut KittyStorage) -> R) -> R {
        let mut guard = crate::terminal::kitty::storage::lock_unpoisoned(&self.storage);
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
    /// 事件通道发送端，留在 UI 侧。
    ///
    /// `Term` 本身也持有一份，但读线程/解析线程退出时会 drop 它手里的
    /// 最后一份（如果 UI 不持），`futures::mpsc` 关闭通道会唤醒 GPUI 任务，
    /// 而这个唤醒发生在工作线程上——`#[gpui::test]` 会判成非确定性线程活动，
    /// 真实运行时也会把终端生命周期绑在读线程退出顺序上。UI 持有发送端后，
    /// 通道只在 `Terminal` 析构时关闭。
    _listener: ZedListener,
    /// 本地 PTY 启动出来的首个子进程 pid，通常就是 shell 进程。
    pub child_process_id: Option<u32>,
    /// shell 所在的进程组 id，用来判断当前前台程序是否仍然是 shell。
    #[cfg(unix)]
    shell_process_group_id: Option<i32>,
    /// 本地 PTY master fd，用来读取当前 TTY 的前台进程组。
    #[cfg(unix)]
    pty_master_fd: Option<i32>,
    /// 启动时使用的 shell 程序路径（如 "/bin/zsh"），用于生成 tab 标签。
    shell_program: Option<String>,
    /// 本会话注入发行版的 cwd 标记值（仅 WSL 标签页，见
    /// [`crate::utils::wsl::inject_cwd_token`]）。
    ///
    /// spawn 时随 `WSLENV` 送进发行版，之后轮询用同一个值去 `/proc` 里认出
    /// 这个标签页的 shell——没有它，`cd` 之后的 Linux 侧 cwd 就没人能问到。
    wsl_cwd_token: Option<String>,
    /// 远程 shell 提示符是否已准备好。
    remote_shell_ready: bool,
    /// 终端通过 OSC 7 / CurrentDir 等序列主动上报的工作目录。
    ///
    /// 两条管线都在这里汇合：
    /// - 本地 PTY：读线程的扫描器摘出 OSC，经 `LocalNotice::Osc` 回到 UI 线程
    ///   后由 `handle_local_osc` 喂进 `try_handle_osc_sequence`（host 须为本机）。
    /// - SSH：字节在 UI 线程解析，`process_pty_data` 就地判定。host 属于远端，
    ///   不做本机校验。
    ///
    /// 统一保存"终端自己声明的 cwd"，可以避免再用 `lsof`/进程 cwd 猜测 shell
    /// 状态；后者无法稳定追踪 shell 内建 `cd` 后的实时目录。
    ///
    /// 历史注记：这里曾写着"本地 PTY 走 vendored vte/alacritty 的 OSC 7 事件，
    /// 由 `PwdChanged` 写入"——**那是错的**。alacritty 0.26 与 vte 0.15 都没有
    /// `PwdChanged`，vte 的 `osc_dispatch` 对 OSC 7 直接落到 `unhandled()` 丢弃，
    /// 因此在补上本地拦截之前，本地 shell 的 cwd 上报一直是石沉大海。
    reported_working_directory: Option<String>,
    /// 检测到的远程操作系统
    pub remote_os: Option<RemoteOS>,
    /// 远程前台进程名（由远程 shell 的 preexec hook 通过 OSC 上报）
    remote_process_name: Option<String>,
    /// 端口转发的建立结果（`None` = 该连接未配置转发，或尚未回报）。
    /// 与 `remote_os` 同属「连接期一次性结果」，由 `DriverEvent::PortForward` 写入。
    port_forward_status: Option<PortForwardStatus>,
    /// 进程检测发现的前台 CLI agent。`None` = 前台不是 agent（含 shell 回到前台）。
    ///
    /// 只回答"是哪个 agent"，**不代表它在忙**——回合状态一律以
    /// `agent_session` 为准（见 [`Self::agent_session`]）。
    foreground_agent: Option<CLIAgent>,
    /// 前台进程的完整 argv（末次探测结果）。
    ///
    /// 与 `foreground_agent` 同源、同一次探测产出：agent 识别只是 argv 的一种
    /// 解读，而标签上的程序方块（`crate::programs`）需要完整命令行才能认出
    /// `node .../vite` 这类被解释器包着的程序。拆成两个字段而不是一个结构体，
    /// 是因为 `foreground_agent` 还有 hook 声明这条独立来源。
    foreground_argv: Option<Vec<String>>,
    /// hook 事件驱动的 agent 回合状态。
    ///
    /// 与 `foreground_agent` 的两种解耦情况：
    /// - 装了 hook 但进程检测认不出（agent 被包装器裹着）：靠事件里的 agent 兜底建 session。
    /// - 没装 hook：检测出身份、session 为 `None`，界面显示品牌图标 + Idle，绝不误报 Working。
    agent_session: Option<AgentSessionState>,
    /// 用户按下的「打断」键是否还在等宽限窗（见 [`Self::note_interrupt_key`]）。
    ///
    /// 存在的理由：多数 agent 在用户打断回合时不发 hook，但少数会（Kimi 的
    /// `Interrupt`、与按键赛跑的 `Stop`）。宽限窗让后者先说话，避免把还在
    /// 跑的回合过早判成结束。
    agent_interrupt_pending: Option<std::time::Instant>,
    /// 最近一次 agent 事件（或 session 重置）的时刻，用于放弃静默过久的回合。
    agent_last_event: Option<std::time::Instant>,
    window_title: Option<String>,
    /// 本地终端读线程的发送端（输入/尺寸/关闭的唯一通道）。
    ///
    /// 本地与远程互斥：本地终端只有这一条输入通道，远程走
    /// `remote_input_callback` / `remote_resize_callback`。
    local_sender: Option<crate::terminal::local_pty::LocalSender>,
    /// 本地读线程句柄。持有它以保证读线程存活至终端析构。
    /// 字段本身不被读取，故以下划线前缀标记。
    _local_reader: Option<crate::terminal::local_pty::LocalReader>,
    /// PTY 模式 SSH 的句柄。`kill()` 必须显式 `close()`：只 drop 回调闭包
    /// 会让事件循环的 sender 断开，alacritty 在收到 `Disconnected` 时
    /// `panic!("event loop channel closed")`，而 `ssh` 子进程也可能挂着。
    ssh_pty_terminal: Option<SshPtyTerminal>,
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
    /// Kitty clipboard（OSC 5522）的写入事务状态。
    kitty_clipboard: crate::terminal::kitty::clipboard::ClipboardState,
    /// Kitty DnD（OSC 72）注册与 drop-target 状态。
    kitty_dnd: crate::terminal::kitty::dnd::DndState,
    last_content: TerminalContent,
    /// Reusable buffer for the full-viewport row fingerprints. A Full damage
    /// frame used to allocate a fresh `Vec<u64>` every time; alternating with
    /// `last_content.row_hashes` keeps two buffers alive and allocation-free.
    row_hash_scratch: Vec<u64>,
    pub content_generation: u64,
    content_dirty: bool,
    /// 最近一次「有字节真的被解析进网格」的时刻（见 [`AGENT_STREAM_WINDOW`]）。
    ///
    /// 与 `content_generation` 同步更新：两者都是「输出发生了」的证据，只是
    /// 一个用于缓存失效，一个用于时间窗判定（扫光动画 / agent 计时器）。
    last_output_at: Option<std::time::Instant>,
    /// 连续多少次因为拿不到网格锁而跳过了快照刷新（见 `update_last_content`）。
    /// 超过 [`MAX_SNAPSHOT_LOCK_SKIPS`] 后退回公平锁，保证画面一定跟得上。
    snapshot_lock_skips: u32,
    /// 测试专用：增量快照分支的命中次数（见 `incremental_snapshot_count_for_test`）。
    #[cfg(test)]
    incremental_snapshot_count: u64,
    /// 测试专用：兜底全量重建分支的命中次数（见 `row_ranges` 不一致那条路径）。
    #[cfg(test)]
    snapshot_fallback_count: u64,
    /// 测试专用：累计重建了多少个 `IndexedCell`（增量快照收益的直接度量）。
    #[cfg(test)]
    snapshot_cells_built: u64,
    /// 测试专用：部分帧累计重算了多少行指纹（应只覆盖脏行）。
    #[cfg(test)]
    snapshot_rows_hashed: u64,
    /// 每次 update_last_content 后，标记哪些 viewport 行（0 = 顶行）发生了变化
    dirty_rows: Vec<bool>,
    /// 每次 update_last_content 后，记录 viewport 行的脏区列范围 `(left, right)`。
    /// 若该行无变更则为 `None`；全量刷新时所有行为 `Some((0, cols-1))`。
    dirty_row_bounds: Vec<Option<(usize, usize)>>,
    /// 事件循环任务（保持任务存活，防止被 drop；字段不被读取故加下划线前缀）
    _event_loop_task: Option<Task<()>>,
    /// 远程驱动任务（保持任务存活，防止被 drop；字段不被读取故加下划线前缀）
    _driver_task: Option<Task<()>>,
    /// 最新待应用的窗口尺寸（合并器只保留最新值；网格与 PTY/SSH 一起原子应用）。
    pending_resize: Option<TerminalBounds>,
    /// resize 合并任务句柄（ghostty 式 25ms 非重置节流）；drop 即取消。
    resize_task: Option<Task<()>>,
    /// 跳过快照刷新后的补帧任务句柄（见 [`SNAPSHOT_RETRY_DELAY`]）；drop 即取消。
    ///
    /// 只保证「一定会再有一帧」，不保证那一帧就能拿到锁——拿不到就再排一个，
    /// 直到 `MAX_SNAPSHOT_LOCK_SKIPS` 触发的公平锁兜底把它刷完为止。
    snapshot_retry_task: Option<Task<()>>,
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
    /// - 本地未上报 133;B 的 shell（如未注入集成脚本）：退化为
    ///   [`Terminal::write`] 时捕获的“首个输入前的光标位置”（即提示符末尾）。
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
    /// 用户/默认请求的 scrollback 行数；实际行数还会按
    /// [`SCROLLBACK_BUDGET_BYTES`] 随列数折算。
    scrollback_requested: usize,
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
    /// Wakeup 时抢不到网格锁，焦点模式检查被推迟到下一次快照。
    /// 这样 Wakeup 路径完全不阻塞在 `FairMutex` 上（9/25 采样主线程
    /// 46.5% 的 `__psynch_cvwait` 就来自这里）。
    focus_check_pending: bool,
}

impl Terminal {
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
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &default_bounds,
            listener.clone(),
        )));

        cx.new(|_cx| Self {
            term,
            _listener: listener,
            child_process_id: None,
            #[cfg(unix)]
            shell_process_group_id: None,
            #[cfg(unix)]
            pty_master_fd: None,
            shell_program: None,
            wsl_cwd_token: None,
            remote_shell_ready: false,
            reported_working_directory: None,
            remote_os: None,
            remote_process_name: None,
            port_forward_status: None,
            foreground_agent: None,
            foreground_argv: None,
            agent_session: None,
            agent_interrupt_pending: None,
            agent_last_event: None,
            window_title: None,
            local_sender: None,
            _local_reader: None,
            ssh_pty_terminal: None,
            images: KittyImages::default(),
            kitty_replies: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            kitty_images_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kitty_generation: 0,
            last_kitty_generation: 0,
            kitty_scanner: None,
            kitty_clipboard: Default::default(),
            kitty_dnd: Default::default(),
            last_content: TerminalContent::default(),
            row_hash_scratch: Vec::new(),
            content_generation: 0,
            content_dirty: false,
            last_output_at: None,
            snapshot_lock_skips: 0,
            #[cfg(test)]
            incremental_snapshot_count: 0,
            #[cfg(test)]
            snapshot_fallback_count: 0,
            #[cfg(test)]
            snapshot_cells_built: 0,
            #[cfg(test)]
            snapshot_rows_hashed: 0,
            dirty_rows: Vec::new(),
            dirty_row_bounds: Vec::new(),
            _event_loop_task: None,
            _driver_task: None,
            pending_resize: None,
            resize_task: None,
            snapshot_retry_task: None,
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
            last_mouse_position: None,
            last_reported_mouse_cell: None,
            scroll_px: px(0.0),
            scrollbar_drag_offset: Rc::new(Cell::new(None)),
            last_scroll_time: Cell::new(Instant::now()),
            scrollback_requested: scrollback_lines.min(MAX_SCROLLBACK_LINES),
            terminal_type: TerminalType::Local,
            remote_input_callback: None,
            remote_resize_callback: None,
            russh_handle: None,
            focused: true,
            focus_mode_reported: false,
            focus_check_pending: false,
        })
    }

    fn build_local_terminal(
        cx: &mut App,
        mut pty_options: tty::Options,
        pty_error_message: String,
        remote_os: Option<RemoteOS>,
        shell_program: Option<String>,
    ) -> Entity<Self> {
        // 把当前二进制所在目录前置进子 shell 的 PATH：Finder 启动与
        // `cargo run` 都不保证 deepterm 已在 PATH 里，而用户在终端里可能想
        // 直接调它。仅注入本地 PTY，不能把本机路径传给 SSH。
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent()
        {
            let path = pty_options.env.get("PATH").cloned().unwrap_or_default();
            if let Ok(path) = std::env::join_paths(
                std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&path)),
            ) {
                pty_options
                    .env
                    .insert("PATH".into(), path.to_string_lossy().into_owned());
            }
        }
        // WSL 标签页：给本会话的 shell 打一个唯一 cwd 标记，并把它点名进
        // WSLENV——WSL 只透传 WSLENV 里列出的变量，不加就没法把标记送进发行版。
        //
        // 这是「cd 之后文件树根跟着走」的**唯一**实时来源：WSL 标签页没有
        // shell integration（拿不到 OSC 7），Windows 侧的进程 cwd 查询又只看得到
        // `wsl.exe` 自己。后续轮询用这个值进 `/proc` 里认出本标签页的 shell。
        //
        // `cfg(windows)`：WSL 标签页只在 Windows 上存在。unix 上不能进这个分支
        // ——那里若有个恰好叫 `wsl` 的程序，给它注入 `WSLENV`/`DEEPTERM_CWD_TOKEN`
        // 就是凭空改变用户 shell 的环境。
        #[cfg(windows)]
        let wsl_cwd_token = shell_program
            .as_deref()
            .filter(|program| crate::utils::wsl::is_wsl_command(program))
            .map(|_| crate::utils::wsl::inject_cwd_token(&mut pty_options.env));
        #[cfg(not(windows))]
        let wsl_cwd_token: Option<String> = None;
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
        let scrollback_requested = settings_scrollback_lines(cx);
        let scrollback_lines =
            scrollback_lines_for_cols(scrollback_requested, default_bounds.num_columns());
        let term = Arc::new(FairMutex::new(Term::new(
            Self::terminal_config_with_scrollback(scrollback_lines),
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
        let child_process_id = pty.child_watcher().pid().map(|p| p.get());

        // Kitty 图形协议：读线程/扫描器写入，UI 线程渲染。存储在这两条
        // 管线之间共享，因此这里先建好再分别交给扫描器与 `Terminal`。
        let images = KittyImages::default();
        let kitty_replies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kitty_images_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // 模式 1016（SGR-Pixels 鼠标）的本地状态句柄：读线程里的扫描器写，
        // 鼠标报文编码（UI 线程）读。
        let local_mode_1016 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // 本地终端字节管线：自建读线程（`LocalReader`），字节经应用层扫描后
        // 再喂 vte，这样 APC（Kitty 图形协议）才能被拦截。Windows 的 conpty
        // 同样走这条管线（`Pty` 抽象已覆盖平台差异），扫描器不再是 unix 独占。
        // 系统 conhost 的 ConPTY 会丢 APC，Windows 上必须同时分发 WT 的
        // `conpty.dll`（见 `local_pty` 模块文档）。
        //
        // 初始几何由 `LocalReader::spawn` 统一注入（见其 `initial_geometry`
        // 参数），这里只需构造扫描器并交出 1016 的共享状态句柄（鼠标报文在
        // UI 线程按该状态编码）。
        let scanner = crate::terminal::kitty::scanner::KittyGraphicsScanner::with_storage(
            images.storage(),
            KITTY_IMAGE_LIMIT > 0,
            Box::new(KittyReplySink {
                replies: kitty_replies.clone(),
                changed: kitty_images_dirty.clone(),
            }),
        )
        .with_mode_1016_shared(local_mode_1016.clone());
        let terminal_listener = listener.clone();
        let (local_reader, local_notices) = match crate::terminal::local_pty::LocalReader::spawn(
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

            // 把读线程的通知（重绘 / 子进程退出 / OSC 旁路）泵到 UI 线程。
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
                _listener: terminal_listener,
                child_process_id,
                #[cfg(unix)]
                shell_process_group_id,
                #[cfg(unix)]
                pty_master_fd,
                shell_program,
                wsl_cwd_token,
                remote_shell_ready: false,
                reported_working_directory: None,
                remote_os,
                remote_process_name: None,
                port_forward_status: None,
                foreground_agent: None,
                foreground_argv: None,
                agent_session: None,
                agent_interrupt_pending: None,
                agent_last_event: None,
                window_title: None,
                local_sender: local_reader.as_ref().map(|r| r.sender()),
                _local_reader: local_reader,
                ssh_pty_terminal: None,
                images,
                kitty_replies,
                kitty_images_dirty,
                kitty_generation: 0,
                last_kitty_generation: 0,
                kitty_scanner: None,
                kitty_clipboard: Default::default(),
                kitty_dnd: Default::default(),
                last_content: TerminalContent::default(),
                row_hash_scratch: Vec::new(),
                content_generation: 0,
                content_dirty: false,
                last_output_at: None,
                snapshot_lock_skips: 0,
                #[cfg(test)]
                incremental_snapshot_count: 0,
                #[cfg(test)]
                snapshot_fallback_count: 0,
                #[cfg(test)]
                snapshot_cells_built: 0,
                #[cfg(test)]
                snapshot_rows_hashed: 0,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                _event_loop_task: Some(event_loop_task),
                _driver_task: reader_notice_task,
                pending_resize: None,
                resize_task: None,
                snapshot_retry_task: None,
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
                last_mouse_position: None,
                last_reported_mouse_cell: None,
                scroll_px: px(0.0),
                scrollbar_drag_offset: Rc::new(Cell::new(None)),
                last_scroll_time: Cell::new(Instant::now()),
                scrollback_requested,
                terminal_type: TerminalType::Local,
                remote_input_callback: None,
                remote_resize_callback: None,
                russh_handle: None,
                focused: true,
                focus_mode_reported: false,
                focus_check_pending: false,
            }
        })
    }

    pub fn new(cx: &mut App, working_directory: Option<PathBuf>) -> Entity<Self> {
        // Windows 上「挑默认 shell」是靠**真的跑一次**探测进程完成的，而
        // powershell/pwsh 都是控制台程序：从 GUI 子系统进程里用
        // `Command::output()` 启动它们，Windows 会新分配控制台并显示窗口，
        // 用户看到的是「标签页还没出来，先闪一个黑框」。
        // `hidden_command` 加上 `CREATE_NO_WINDOW` 消掉这个闪烁。
        #[cfg(windows)]
        let default_shell = {
            let probe_runs = |program: &str| {
                crate::utils::console::hidden_command(program)
                    .arg("-NoProfile")
                    .arg("-Command")
                    .arg("exit 0")
                    .output()
                    .is_ok()
            };
            if probe_runs("powershell.exe") {
                "powershell.exe".to_string()
            } else if probe_runs("pwsh.exe") {
                "pwsh.exe".to_string()
            } else {
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
            }
        };
        #[cfg(not(windows))]
        let default_shell: String =
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

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

        // 与默认 shell 路径（configure_local_shell_integration）相同的
        // PowerShell cwd 同步注入（见 powershell_cwd_sync_args）。注入放在
        // Terminal 层而不是 TerminalView 层：session_state 存的是这里的
        // 原始 command/args，重连重放时会重新注入一份，不会双重叠加。
        let args = powershell_cwd_sync_args(command, args).unwrap_or_else(|| args.to_vec());

        Self::build_local_terminal(
            cx,
            tty::Options {
                shell: Some(tty::Shell::new(command.to_string(), args)),
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
        let listener = ZedListener(unbounded().0);
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TerminalBounds::default(),
            listener.clone(),
        )));

        Self {
            term,
            _listener: listener,
            child_process_id: None,
            #[cfg(unix)]
            shell_process_group_id: None,
            #[cfg(unix)]
            pty_master_fd: None,
            shell_program: None,
            wsl_cwd_token: None,
            remote_shell_ready: false,
            reported_working_directory: None,
            remote_os: None,
            remote_process_name: None,
            port_forward_status: None,
            foreground_agent: None,
            foreground_argv: None,
            agent_session: None,
            agent_interrupt_pending: None,
            agent_last_event: None,
            window_title: None,
            local_sender: None,
            _local_reader: None,
            ssh_pty_terminal: None,
            images: KittyImages::default(),
            kitty_replies: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            kitty_images_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kitty_generation: 0,
            last_kitty_generation: 0,
            kitty_scanner: None,
            kitty_clipboard: Default::default(),
            kitty_dnd: Default::default(),
            last_content: TerminalContent::default(),
            row_hash_scratch: Vec::new(),
            content_generation: 0,
            content_dirty: false,
            last_output_at: None,
            snapshot_lock_skips: 0,
            #[cfg(test)]
            incremental_snapshot_count: 0,
            #[cfg(test)]
            snapshot_fallback_count: 0,
            #[cfg(test)]
            snapshot_cells_built: 0,
            #[cfg(test)]
            snapshot_rows_hashed: 0,
            dirty_rows: Vec::new(),
            dirty_row_bounds: Vec::new(),
            _event_loop_task: None,
            _driver_task: None,
            pending_resize: None,
            resize_task: None,
            snapshot_retry_task: None,
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
            last_mouse_position: None,
            last_reported_mouse_cell: None,
            scroll_px: px(0.0),
            scrollbar_drag_offset: Rc::new(Cell::new(None)),
            last_scroll_time: Cell::new(Instant::now()),
            scrollback_requested: DEFAULT_SCROLLBACK_LINES,
            terminal_type: TerminalType::Local,
            remote_input_callback: None,
            remote_resize_callback: None,
            russh_handle: None,
            focused: true,
            focus_mode_reported: false,
            focus_check_pending: false,
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
        let scrollback_requested = settings_scrollback_lines(cx);
        let scrollback_lines =
            scrollback_lines_for_cols(scrollback_requested, default_bounds.num_columns());

        let term = Arc::new(FairMutex::new(Term::new(
            Self::terminal_config_with_scrollback(scrollback_lines),
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
            listener,
            events_rx,
            driver_rx,
            move |data| russh_for_input.write(data),
            move |cols, rows, _scale| russh_for_resize.resize(cols, rows),
            scrollback_requested,
            Some(russh_for_status),
        )
    }

    fn new_ssh_pty(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        remote_cwd: Option<String>,
    ) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        // 系统 ssh 路径原本没有驱动事件（`-R` 由 ssh 进程自己维护），这条通道
        // 现在回报两类「连接期一次性结果」：代理落点与端口转发。容量取 2——
        // 两类各一条，都发生在建连前，满了会 `try_send` 失败丢掉其中一条。
        let (driver_tx, driver_rx) = async_channel::bounded(2);
        let listener = ZedListener(events_tx.clone());
        let default_bounds = TerminalBounds::default();
        let scrollback_requested = settings_scrollback_lines(cx);
        let scrollback_lines =
            scrollback_lines_for_cols(scrollback_requested, default_bounds.num_columns());

        let term = Arc::new(FairMutex::new(Term::new(
            Self::terminal_config_with_scrollback(scrollback_lines),
            &default_bounds,
            listener.clone(),
        )));

        let terminal_listener = listener.clone();
        let (pty_terminal, proxy_outcome) = match SshPtyTerminal::new(
            ssh_config,
            default_bounds,
            // 构造期没有窗口、拿不到 DPI，先用 1.0；首帧 `resize` 会带上真实
            // 缩放纠正（与 PTY 初始尺寸走同一条路径）。
            1.0,
            term.clone(),
            listener,
            remote_cwd,
        ) {
            Ok(created) => created,
            Err(err) => {
                log::error!("SSH PTY 初始化失败: {}", err);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };

        let pty_input = pty_terminal.clone();
        let pty_resize = pty_terminal.clone();

        // 系统 ssh 路径的隧道由 ssh 进程自己维护（`-R` 已在 ssh 参数里），
        // 这里只把「本次带了哪些规则」报到 UI。
        //
        // 共享隧道已覆盖的规则不再计入：那些端口已经被本进程的专职隧道监听着，
        // 再加 `-R` 只会让 ssh 报「端口被占用」（占住的是我们自己），报到 UI
        // 就成了一条自己制造的假警报。
        //
        // 代理落点复用同一条通道：用户点了「使用系统代理连接」而这条路径表达
        // 不了时，必须弹出来而不是静默直连。
        let proxy_notice = match &proxy_outcome {
            crate::ssh::ssh_pty::ProxyCommandOutcome::Unavailable { reason } => {
                Some((format!("无法经系统代理连接：{reason}。本次为直连。"), false))
            }
            crate::ssh::ssh_pty::ProxyCommandOutcome::Applied { proxy } => {
                Some((format!("已随连接请求经系统代理 {proxy} 建立"), true))
            }
            crate::ssh::ssh_pty::ProxyCommandOutcome::NotRequested => None,
        };
        if let Some((message, ok)) = proxy_notice {
            driver_tx
                .try_send(DriverEvent::PortForward { message, ok })
                .ok();
        }
        let pending = crate::ssh::tunnel::rules_for_openssh(ssh_config, &ssh_config.port_forwards);
        if !pending.is_empty() {
            // 这条路径拿不到远端的确认（`-R` 的结果只体现在 ssh 的 stderr 上，
            // 直接落在终端里）。所以只陈述「请求了什么」，不谎称已绑定成功。
            let summary = pending
                .iter()
                .map(|r| r.summary())
                .collect::<Vec<_>>()
                .join("；");
            driver_tx
                .try_send(DriverEvent::PortForward {
                    message: format!(
                        "已随连接请求转发：{summary}（远端端口被占用时 ssh 会在终端里提示）"
                    ),
                    ok: true,
                })
                .ok();
        }
        // 全部由共享隧道覆盖时不报：与 russh 路径一致，纯复用是静默的
        // （每开一个标签弹一次「已就绪」就是噪音）。
        // 未配置：丢掉发送端，driver_rx 立即结束（与改动前行为一致）。
        drop(driver_tx);

        let terminal = Self::create_remote_terminal(
            cx,
            ssh_config,
            term,
            terminal_listener,
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
            scrollback_requested,
            None,
        );
        // 保存一份句柄供 `kill()` 显式 close；输入/尺寸闭包里的克隆仍然
        // 只是借用同一个 sender。
        terminal.update(cx, |terminal, _| {
            terminal.ssh_pty_terminal = Some(pty_terminal);
        });
        terminal
    }

    /// 通用远程终端实体创建（抽取 create_russh_terminal 和 create_ssh_pty_terminal 的公共部分）
    fn create_remote_terminal(
        cx: &mut App,
        ssh_config: &SshConnectionConfig,
        term: Arc<FairMutex<Term<ZedListener>>>,
        listener: ZedListener,
        events_rx: UnboundedReceiver<AlacTermEvent>,
        driver_rx: async_channel::Receiver<DriverEvent>,
        input_handler: impl Fn(&[u8]) + Send + Sync + 'static,
        resize_handler: impl Fn(u16, u16, f32) + Send + Sync + 'static,
        scrollback_requested: usize,
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
                        DriverEvent::PortForward { message, ok } => {
                            terminal.port_forward_status =
                                Some(PortForwardStatus { message, ok });
                            cx.emit(Event::PortForwardChanged);
                        }
                    });
                }
            });

            Self {
                term: term_clone,
                _listener: listener,
                child_process_id: None,
                #[cfg(unix)]
                shell_process_group_id: None,
                #[cfg(unix)]
                pty_master_fd: None,
                shell_program: None,
                wsl_cwd_token: None,
                remote_shell_ready: false,
                reported_working_directory: None,
                remote_os: None,
                remote_process_name: None,
                port_forward_status: None,
                foreground_agent: None,
                foreground_argv: None,
                agent_session: None,
                agent_interrupt_pending: None,
                agent_last_event: None,
                window_title: None,
                local_sender: None,
                _local_reader: None,
                ssh_pty_terminal: None,
                // 复用上面交给扫描器的那三份共享句柄：扫描器（写）与渲染/
                // 应答路径（读）必须是**同一个** `Arc`，否则 SSH 下图像永远
                // 渲染不出来、应答也发不出去。它们在外层创建，此处克隆
                // （`kitty_scanner` 也在本字面量里用它们，故必须 clone）。
                images: images.clone(),
                kitty_replies: kitty_replies.clone(),
                kitty_images_dirty: kitty_images_dirty.clone(),
                kitty_generation: 0,
                last_kitty_generation: 0,
                kitty_clipboard: Default::default(),
                kitty_dnd: Default::default(),
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
                row_hash_scratch: Vec::new(),
                content_generation: 0,
                content_dirty: false,
                last_output_at: None,
                snapshot_lock_skips: 0,
                #[cfg(test)]
                incremental_snapshot_count: 0,
                #[cfg(test)]
                snapshot_fallback_count: 0,
                #[cfg(test)]
                snapshot_cells_built: 0,
            #[cfg(test)]
            snapshot_rows_hashed: 0,
                dirty_rows: Vec::new(),
                dirty_row_bounds: Vec::new(),
                _event_loop_task: Some(event_loop_task),
                _driver_task: Some(driver_task),
                pending_resize: None,
                resize_task: None,
                snapshot_retry_task: None,
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
                last_mouse_position: None,
                last_reported_mouse_cell: None,
                scroll_px: px(0.0),
                scrollbar_drag_offset: Rc::new(Cell::new(None)),
                last_scroll_time: Cell::new(Instant::now()),
                scrollback_requested,
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
            focus_check_pending: false,
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

    /// 前台 CLI agent 的身份（进程检测或 hook 声明）。
    ///
    /// `None` 表示前台不是 agent——包括"shell 回到了前台"和"检测认不出来"。
    /// 注意 `Some` 不代表它在忙：回合状态在 [`Self::agent_session`]。
    pub fn foreground_agent(&self) -> Option<CLIAgent> {
        self.foreground_agent
    }

    /// 前台进程的 argv（末次探测结果）。`None` = 前台是 shell，或还没探过。
    pub fn foreground_argv(&self) -> Option<&[String]> {
        self.foreground_argv.as_deref()
    }

    /// agent 的回合状态。`None` = 没有 agent 在前台，或没装 hook 所以没有状态来源。
    pub fn agent_session(&self) -> Option<&AgentSessionState> {
        self.agent_session.as_ref()
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
        // 各平台分支显式 `return`：这样即使分析器没有应用 `cfg`（把三个块
        // 都当普通语句看），也不会把非尾块判成「应为 ()」。编译器的 cfg
        // 结果与原来完全一致。
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(format!("/proc/{}/comm", pid))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        #[cfg(target_os = "macos")]
        {
            return std::process::Command::new("/bin/ps")
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
                });
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    pub fn reported_working_directory(&self) -> Option<&str> {
        self.reported_working_directory.as_deref()
    }

    /// 本会话注入发行版的 cwd 标记（仅 WSL 标签页；见 [`Self::wsl_cwd_token`]）。
    ///
    /// 没有它就进 `/proc` 认不出哪个进程是这个标签页的 shell，也就问不到它
    /// `cd` 之后的实时目录。
    pub fn wsl_cwd_token(&self) -> Option<&str> {
        self.wsl_cwd_token.as_deref()
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

    /// 端口转发的建立结果（未配置或尚未回报时为 `None`）。
    pub fn port_forward_status(&self) -> Option<&PortForwardStatus> {
        self.port_forward_status.as_ref()
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
        // 注意：`force_full_damage` 只是「拿不到锁时也退回公平锁」，首选仍是
        // 非阻塞的 `try_lock_unfair`，所以本调用**仍可能返回 false**（快照跳过
        // 本帧）。这不影响正确性：主题色已写进 `Term.colors`，下一帧快照自然
        // 会带上；调用方也无需重试。
        let _ = self.update_last_content(true);
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
    ///
    /// 用最后一次快照里的 mode，而不是再取一次 `term.lock()`：焦点切换
    /// 可能发生在读线程持 lease 期间，这里排队等锁会把整个 UI 卡住。
    fn report_focus_state(&mut self) {
        if self.last_content.mode.contains(TermMode::FOCUS_IN_OUT) {
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
        self.note_output();

        // 远程路径不走本地读线程（`local_pty`），也不发 AlacTermEvent::Wakeup，
        // 在这里比对焦点上报模式位（本地终端的同类检查在 process_event）。
        self.check_focus_mode_enabled();
    }

    /// 比较焦点上报模式位与上次快照，捕获应用 DECSET ?1004h 的开启跳变，
    /// 按需补发当前焦点状态（xterm 约定：开启时立即上报一次基线）。
    ///
    /// Wakeup 路径不能阻塞在网格锁上：`try_lock_unfair` 拿不到就置
    /// `focus_check_pending`，等下一次 `update_last_content` 持锁时补检。
    /// 这样既不会漏掉「应用刚开启 1004」的跳变沿，也不会让读线程的 lease
    /// 把 UI 卡在 `__psynch_cvwait`（9/25 采样 46.5%）。
    fn check_focus_mode_enabled(&mut self) {
        let mode = match self.term.try_lock_unfair() {
            Some(term) => *term.mode(),
            None => {
                self.focus_check_pending = true;
                return;
            }
        };
        self.apply_focus_mode(mode);
    }

    /// 根据一份已读到的 mode 更新焦点上报状态。
    fn apply_focus_mode(&mut self, mode: TermMode) {
        self.focus_check_pending = false;
        let focus_mode_now = mode.contains(TermMode::FOCUS_IN_OUT);
        if focus_mode_now != self.focus_mode_reported {
            self.focus_mode_reported = focus_mode_now;
            if focus_mode_now {
                // `report_focus_state` 只读 last_content.mode，不会再加锁；
                // 这里把刚读到的 mode 先写进快照，保证基线报得出去。
                self.last_content.mode = mode;
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

    /// 记录一次「输出进网格」：快照失效 + 输出时刻 + 代际自增。
    ///
    /// 三件事永远一起发生——代际用于缓存/搜索失效，时刻用于扫光动画与
    /// agent 计时器的时间窗。分开写迟早会有某个入口漏掉其中一件。
    fn note_output(&mut self) {
        self.content_dirty = true;
        self.content_generation += 1;
        self.last_output_at = Some(std::time::Instant::now());
    }

    /// 目前是否「正在持续输出」（见 [`AGENT_STREAM_WINDOW`]）。
    pub fn output_streaming(&self, window: Duration) -> bool {
        self.last_output_at.is_some_and(|at| at.elapsed() < window)
    }

    /// 距离「不再算正在输出」还有多久；已经停了（或从未输出）时返回 `None`。
    ///
    /// 给 agent 计时器用：正在输出时它要把唤醒点排在这个窗口的末尾，
    /// 否则扫光的关闭要等到下一次事件或 30 分钟的静默兜底。
    fn output_stream_remaining(&self, window: Duration) -> Option<Duration> {
        let at = self.last_output_at?;
        let elapsed = at.elapsed();
        (elapsed < window).then(|| window - elapsed)
    }

    /// 处理本地读线程的通知。
    ///
    /// 读线程只做 vte 解析（不需要 `Context`），凡涉及事件派发/重绘的
    /// 副作用统一回到 UI 线程在这里完成。
    /// `pub(crate)` 而非私有：测试要能灌一条 OSC 走**本地**这条真实入口
    /// （远程那条是 `handle_remote_data`，两者在 OSC 处理上并不等价）。
    pub(crate) fn handle_local_notice(
        &mut self,
        notice: crate::terminal::local_pty::LocalNotice,
        cx: &mut Context<Self>,
    ) {
        use crate::terminal::local_pty::LocalNotice;
        match notice {
            LocalNotice::Wakeup => {
                // 字节已被读线程解析进 `term`，这里只负责触发重绘。
                // 协议应答的常规回写由读线程取走并直写 PTY；这里的
                // `flush_kitty_replies` 只兜底 UI 侧扫描产生的应答
                // （两条路径靠 `mem::take` 互斥，见 `KittyReplySink`）。
                self.flush_kitty_replies();
                self.sync_kitty_state();
                self.note_output();
                cx.emit(Event::Wakeup);
                self.check_focus_mode_enabled();
            }
            LocalNotice::ChildExit(status) => {
                // 退出事件已由读线程通过 `ZedListener` 发出（process_event
                // 会处理 CloseTerminal / ProcessExited）；这里只兜底触发一次重绘，
                // 保证 drain 出来的最后一段输出能上屏。
                self.flush_kitty_replies();
                self.sync_kitty_state();
                self.note_output();
                cx.emit(Event::Wakeup);
                let _ = status;
            }
            LocalNotice::Osc { content, cursor } => {
                // agent 事件与桌面通知共用 OSC 777 信道，必须**先**试 agent 解析：
                // 否则会落进 handle_local_osc → try_handle_osc_sequence →
                // parse_notification_osc，弹出一条虚假桌面通知（同远程侧的排序理由）。
                //
                // 顺序必须在这个分支里，不能塞进 `handle_local_osc` 内部靠后的位置
                // ——那时通知已经发出去了。
                if let Some(event) = crate::agent::parse_agent_event(&content) {
                    self.apply_agent_events(&[event], cx);
                } else {
                    self.handle_local_osc(&content, Some(cursor), cx);
                }
            }
            LocalNotice::Error(reason) => {
                log::error!("本地 PTY 读线程错误: {reason}");
            }
        }
    }

    /// 判定读线程报来的一条 OSC 是否由应用层消费。
    ///
    /// 与远程路径的关系：远程路径在 `process_pty_data` 里就地判定；本地路径的
    /// 判定发生在序列被读线程**旁路通知**之后（判定要 `Context` 与 `Terminal`
    /// 字段，读线程都没有）。字节本身不必在这里补喂——扫描器已按原位置喂给
    /// vte 了（见 `NoticeOscSink`），这里只做「是否消费」的判定，避免与 vte
    /// 争抢同一序列的处理权。
    ///
    /// `cursor_at_sequence`：见 [`Self::try_handle_osc_sequence`]。本地路径必须
    /// 由读线程带上（同 chunk 的后续字节已经解析完），不能现读光标。
    ///
    /// `require_local_osc7_host = true`：这条序列来自本机 PTY，OSC 7 的 host
    /// 必须解析为**本机**才采信。远程路径传 `false`（host 属于远端，不该拿
    /// 本机 hostname 比对）。这正是两条管线共用 `try_handle_osc_sequence` 时
    /// 唯一需要区分的语义。
    fn handle_local_osc(
        &mut self,
        content: &[u8],
        cursor_at_sequence: Option<(i32, u32)>,
        cx: &mut Context<Self>,
    ) {
        self.try_handle_osc_sequence(cx, content, true, cursor_at_sequence);
    }

    /// 把一批 agent 事件并入状态。
    ///
    /// 与 tty7 的 `apply_agent_signals` 的差异（均为刻意）：
    /// - 无 daemon 通知通道，改用 `Event::AgentChanged` 让订阅方主动取状态；
    /// - 不接收 `notification` 参数——deepterm 的桌面通知与 agent 状态**不关联**
    ///   （已确认：普通通知完全忽略，状态只由 hook 事件驱动）；
    /// - 无 `rich` / `launch_argv`——那两个服务于 daemon 跨重启保留 pane 状态，
    ///   本仓库没有 daemon（见 `cli_agent.rs` 对 `AgentSessionState` 的说明）。
    pub fn apply_agent_events(&mut self, events: &[AgentEvent], cx: &mut Context<Self>) {
        if events.is_empty() {
            return;
        }
        let before_agent = self.foreground_agent;
        let before_session = self.agent_session.clone();

        for event in events {
            // hook 自己声明的 agent 是**兜底**：进程检测认不出（包装器、临时进程名）
            // 时才采信；已经认出来的不覆盖——检测结果来自真实进程，比事件里的自称可靠。
            if self.foreground_agent.is_none() && event.agent.is_some() {
                self.foreground_agent = event.agent;
            }
            // 门控建 session：仅当"确实有 agent"时才建，避免杂散 sentinel 输出
            // 凭空造出一个没有 agent 的孤儿 session（那会让界面显示状态点却无图标）。
            if self.agent_session.is_none()
                && (event.agent.is_some() || self.foreground_agent.is_some())
            {
                self.agent_session = Some(AgentSessionState::default());
            }
            if let Some(session) = self.agent_session.as_mut() {
                session.apply_event(event);
            }
        }
        // 事件到了：静默计时重置，宽限窗里等的那个中断也不需要再猜了——
        // 这条事件本身就是 agent 对那个按键的回应（或与它无关的新活动）。
        self.agent_last_event = Some(std::time::Instant::now());
        self.agent_interrupt_pending = None;

        // 两个判定**不可合并**：agent 身份可能单独变化而 session 全等。
        // 例如 session_id 已经设好、status 已经是 Idle 时再收一条 `SessionStart`，
        // session 前后完全相同，但若这次事件顺带把 `foreground_agent` 从 None 补成了
        // `Some(Claude)`（兜底路径），界面必须刷新才显示得出图标。
        // tty7 的片段只判 session，照抄会静默丢掉这类"纯身份"变更。
        //
        // 这里能直接比整份 session，前提是它**没有**「每个事件都变」的字段：
        // 曾经有个 `activity`（工具调用计数）在 `ToolComplete` 里自增，于是
        // 一个回合里几十次 `ToolComplete` 每次都判为"变了"，每次都发
        // `AgentChanged`，订阅方白重绘、白转发。那个字段已删除（它当时也没有
        // 任何渲染消费方）。**新增字段时请确认它不会在状态未变时自增。**
        if self.foreground_agent != before_agent || self.agent_session != before_session {
            cx.emit(Event::AgentChanged);
        }
    }

    /// 前台 agent 身份的**权威**写入：`Some` 建立身份，`None` 表示确认退出了。
    ///
    /// `None` 会一并清掉 `agent_session`——所以调用方必须确认「真的没有 agent」，
    /// 而不是「这一次没查出来」。
    ///
    /// 两条管线的「回到 None」机制并不对称，各自都在这条路径上落地：
    /// - 本地：`tcgetpgrp(fd) == shell_process_group_id` 时 `detect_agent_for_local`
    ///   直接返回 `None`，连 argv 都不取。但检测是**轮询**，它的 `None` 不可靠，
    ///   所以走 [`Self::apply_detected_agent`] 而不是这里。
    /// - 远程：没有 pgrp 概念，壳集成回到提示符时会**重新上报 shell 自己的 basename**
    ///   （`assets/shell-integration/zsh/deepterm-integration:126` 发 `OSC 1337;Proc=zsh`），
    ///   所以 `remote_process_name` 永远不会变回 `None`，只会被覆盖成 shell 名；
    ///   这是**事件驱动**、结论明确，所以直接调这里。
    ///
    /// 值不变时不发事件（检测按 TTL 周期性跑，重复结果很多）。
    pub fn set_foreground_agent(&mut self, agent: Option<CLIAgent>, cx: &mut Context<Self>) {
        if self.foreground_agent == agent {
            return;
        }
        if agent.is_none() && self.agent_session.is_some() {
            self.agent_session = None;
        }
        self.foreground_agent = agent;
        cx.emit(Event::AgentChanged);
    }

    /// 进程检测结果的写入路径。
    ///
    /// 与 [`Self::set_foreground_agent`] 的区别在于**`NoAgent` 与 `Unavailable`
    /// 被区别对待**：检测是每 500 毫秒一次的轮询，它「查不了」和「查了、前台不是
    /// agent」是两件事，混为一谈会让图标乱闪。
    ///
    /// - [`LocalAgentProbe::Agent`]：认出来了，更新身份。
    /// - [`LocalAgentProbe::NoAgent`]：前台确实是个我们认识的程序序列里
    ///   没有的东西（shell 提示符、`vim`、`grep`…）。**这是有效结论**：
    ///   agent 已经退到后台或退出了，图标该落回去。
    /// - [`LocalAgentProbe::Unavailable`]：没有 fd、`tcgetpgrp` 失败、
    ///   取不到 argv。**这不是结论**，保持现状。
    ///
    /// 为什么 `Unavailable` 必须保持现状：agent 启动的瞬间、或它 fork 出的
    /// 短命子进程短暂占据前台时，检测会连续几轮拿不到有效结果。如果那时把
    /// 身份清掉，tab 上的图标就会闪一下再回来，而 hook 报的状态点更是会
    /// 直接消失且**不会自己恢复**——hook 要等下一个回合事件才再报一次。
    ///
    /// 取舍是「宁可多留一会儿」而不是「宁可闪一下」：图标多留一会儿是轻微
    /// 的信息滞后，闪一下则是明显的视觉噪声。
    pub fn apply_detected_agent(&mut self, probe: LocalAgentProbe, cx: &mut Context<Self>) {
        let LocalAgentProbe { argv, outcome } = probe;
        // 「查不了」：身份与 argv 都保持现状（见 [`LocalAgentOutcome`]）——
        // 这一支必须在写 argv **之前**判掉，否则一次短暂的读失败会把
        // 前台程序抹成「什么都没有」。
        let agent = match outcome {
            LocalAgentOutcome::Agent(agent) => Some(agent),
            LocalAgentOutcome::NoAgent => None,
            LocalAgentOutcome::Unavailable => return,
        };

        // argv 是「终端里跑的是什么」的唯一原始输入：标签方块的程序识别
        // （`crate::programs`）全靠它，而不只是 agent 身份。
        // `NoAgent` 且 argv 为 `None` 表示 shell 回到了前台，此时清空是**结论**，
        // 不是「查不了」——两者在上面已经分流。
        let argv_changed = argv.as_ref() != self.foreground_argv.as_ref();
        self.foreground_argv = argv;

        let agent_changed = self.foreground_agent != agent;
        self.set_foreground_agent(agent, cx);
        // 身份没变但程序变了（`vim` → `nvim`、shell → `htop`）也要让上层
        // 重画，否则标签会一直举着旧图标。
        if argv_changed && !agent_changed {
            cx.emit(Event::TitleChanged);
        }
    }

    /// 本地前台 agent 检测。
    ///
    /// **必须在后台线程调用**：macOS 走 `sysctl`，Linux 读 `/proc`。之所以做成
    /// 自由函数而不是 `Terminal` 方法，就是因为后台线程不能碰 `Terminal`——
    /// 这里只接受两个 `Copy` 值。
    #[cfg(unix)]
    pub fn detect_agent_for_local(
        pty_master_fd: Option<i32>,
        shell_process_group_id: Option<i32>,
    ) -> LocalAgentProbe {
        // 「查不了」和「查了，不是 agent」必须区分开——见 [`LocalAgentOutcome`]。
        let unavailable = LocalAgentProbe {
            argv: None,
            outcome: LocalAgentOutcome::Unavailable,
        };
        let Some(fd) = pty_master_fd else {
            return unavailable;
        };
        // SAFETY（中文）：`tcgetpgrp` 只读查询，传入存活的 pty master fd；
        // 失败返回 -1，由下面判掉。
        let pgid = unsafe { libc::tcgetpgrp(fd) };
        if pgid < 0 {
            return unavailable;
        }
        // shell 在前台：这是**确定**的「前台不是 agent」，属于有效结论。
        // argv 交给上层按 shell 名补（`shell_program` 有更准的那一份）。
        if shell_process_group_id == Some(pgid) {
            return LocalAgentProbe {
                argv: None,
                outcome: LocalAgentOutcome::NoAgent,
            };
        }
        // 取不到 argv（进程已退出、权限不足）：退回「查不了」，
        // 不能据此断言前台不是 agent。
        let Some(argv) = crate::terminal::foreground_argv::foreground_argv(pgid as u32) else {
            return unavailable;
        };
        let outcome = match CLIAgent::detect_from_argv_with(&argv, &HashMap::new()) {
            Some(agent) => LocalAgentOutcome::Agent(agent),
            // 拿到了 argv 但没有一个 token 命中已知 agent：
            // 前台确实跑着某个程序，只是我们不认识它。
            None => LocalAgentOutcome::NoAgent,
        };
        LocalAgentProbe {
            argv: Some(argv),
            outcome,
        }
    }

    /// 供后台线程取 pty master fd（[`Self::detect_agent_for_local`] 的入参）。
    #[cfg(unix)]
    pub(crate) fn pty_master_fd(&self) -> Option<i32> {
        self.pty_master_fd
    }

    /// 供后台线程取 shell 进程组 id（[`Self::detect_agent_for_local`] 的入参）。
    #[cfg(unix)]
    pub(crate) fn shell_process_group_id(&self) -> Option<i32> {
        self.shell_process_group_id
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

    /// 把一段字节喂给 vte 解析器（作用于 term）。空段直接跳过。
    fn advance_vte(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut term = self.term.lock();
        self.vte_processor.advance(&mut *term, bytes);
    }

    /// 喂给 vte 之前，先按 `removals`（相对**整块** `data` 的偏移）把探测类序列摘下。
    ///
    /// `slice` 是整块中 `[base, base + slice.len())` 这一段；`removals` 沿用
    /// 整块的坐标系，因此这里用 `base` 换算。
    fn advance_vte_without_removals(
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
    /// 处理 vte/alacritty 要么不应答、要么答错的探测序列：
    /// - `CSI 16 t`：cell 像素尺寸 → `\x1b[6;{cell_height};{cell_width}t`
    /// - `DECSET/DECRST ?2048`：in-band size reports → 置位时立即上报几何
    /// - `CSI > {Ps} q`（XTVERSION）：终端自报 → `\x1bP>|deepterm {版本}\x1b\\`；
    ///   vte 只认 DECSCUSR，对它**无应答**
    /// - `CSI > {Ps} c`（DA2）：→ `\x1b[>0;{Pv};1c`；不摘的话 alacritty 会
    ///   代答它自己的库版本号，把 deepterm 报成 alacritty
    ///
    /// 与本地读线程路径的分工：本地字节在 `KittyGraphicsScanner` 里拦截，
    /// 远程（SSH）字节在 UI 线程这里就地判定；两边用同一组
    /// `escape_scan` 查找器与应答构造函数，保证行为一致。
    ///
    /// `CSI 14 t` / `CSI 18 t` **不在此列**：vte 已能正确应答（格式与 ghostty
    /// 一致），在这里再拦一次会造成双重应答。
    fn handle_probe_sequences(
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
            // 只记状态。去重缓存（`last_reported_mouse_cell`）的失效由
            // `mouse_move` 用 `last_sgr_pixels_seen` 统一处理——本地路径的
            // 置位发生在读线程，UI 侧只有在移动事件里才能发现翻转。
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

        // `CSI > {Ps} q`（XTVERSION）：终端自报名称/版本。
        //
        // 必须摘除：vte-0.15 只认 DECSCUSR（`('q', [b' '])`），这条会落进
        // `unhandled!()` 且不产生任何应答，探测方只能等到超时。
        let mut search_from = 0;
        while let Some((offset, len)) = escape_scan::find_xtversion(&data[search_from..]) {
            let start = search_from + offset;
            self.write_raw(escape_scan::xtversion_reply().as_bytes());
            removals.push((start, len));
            search_from = start + len;
        }

        // `CSI > {Ps} c`（DA2）：回 deepterm 自己的版本号。
        //
        // 必须摘除：不摘的话 alacritty 的 `identify_terminal` 会代答它自己的
        // 库版本（`\x1b[>0;2600;1c`），在客户端把 deepterm 报成 alacritty。
        let mut search_from = 0;
        while let Some((offset, len)) = escape_scan::find_da2(&data[search_from..]) {
            let start = search_from + offset;
            self.write_raw(escape_scan::da2_reply().as_bytes());
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

        // 纯文本快速路径：没有 ESC 且没有残尾时，不可能出现 OSC/APC/CSI，
        // 直接喂 vte。旧代码仍会跑 3 次 memmem 查找，且每次都重建 searcher。
        if self.pending_osc.is_empty() && memchr::memchr(0x1b, data).is_none() {
            self.advance_vte(data);
            return;
        }

        // 有跨 chunk 残尾时先拼接（拷贝一次，仅在有残尾时发生）。
        let pending = std::mem::take(&mut self.pending_osc);
        let joined: Vec<u8>;
        let data: &[u8] = if pending.is_empty() {
            data
        } else {
            joined = [pending.as_slice(), data].concat();
            joined.as_slice()
        };

        let next_osc = escape_scan::OSC_FINDER.find(data);
        let next_apc = escape_scan::APC_FINDER.find(data);
        let next_csi = escape_scan::CSI_FINDER.find(data);
        if next_osc.is_none() && next_apc.is_none() && next_csi.is_none() {
            self.advance_vte(data);
            return;
        }

        // 探测类序列（`CSI 16 t` / DECSET 2048 / XTVERSION / DA2）就地摘除
        // 并应答。
        //
        // vte 对 `CSI 16 t` 与 XTVERSION 是 `unhandled!()`、alacritty 把 2048
        // 视为未知模式、DA2 又会被它代答成 alacritty 的库版本，因此必须在
        // 这一层处理。几何直接取 `self.terminal_bounds`（UI 线程，恒为最新），
        // 无须像本地路径那样经过扫描器缓存。
        let probe_removals = self.handle_probe_sequences(data, cx);
        if next_osc.is_none() && next_apc.is_none() {
            // 只有探测序列：喂入剩余文本即可。
            self.advance_vte_without_removals(data, 0, &probe_removals);
            return;
        }

        let mut pos = 0;
        loop {
            // 下一个起始符（OSC / APC 中较早者）。
            let osc_offset = escape_scan::OSC_FINDER.find(&data[pos..]);
            let apc_offset = escape_scan::APC_FINDER.find(&data[pos..]);
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
            self.advance_vte_without_removals(&data[pos..seq_pos], pos, &probe_removals);

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
                    // 远程路径按序列**分段**喂入：处理到这里时其前置文本已经过
                    // vte 解析，所以 133;B 的光标现读即正确（`None`）。
                    if !self.try_handle_osc_sequence(cx, content, require_local_osc7_host, None) {
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
            self.advance_vte_without_removals(&data[pos..], pos, &probe_removals);
        }
    }

    /// 处理一条完整的 APC 内容（远程路径：扫描在 UI 线程完成）。
    ///
    /// 先同步光标位置——`a=T`/`a=p`/`d=c` 以光标为锚点，而扫描器不与 `term`
    /// 争锁，只能由调用方在解析字节的间隙告知。可见行窗口同理：`d=a` 只删
    /// 屏幕内可见的放置，而窗口随滚动变化。
    fn handle_kitty_apc(&mut self, content: &[u8]) {
        use crate::terminal::escape_scan;
        if !escape_scan::is_kitty_graphics(content) {
            return;
        }
        // 同步更新（`CSI ?2026h`）期间 vte 把字节攒在 processor 的同步缓冲
        // 里不解析，grid 停在块开始前——锚点（CSI 定位）会取到旧光标。
        // 取光标前先回灌，语义与本地路径的 `ScanCtx::flush_sync_update` 一致。
        if self.vte_processor.sync_bytes_count() > 0 {
            let mut term = self.term.lock();
            self.vte_processor.stop_sync(&mut *term);
        }
        let (cursor, visible) = {
            let term = self.term.lock();
            let grid = term.grid();
            let point = grid.cursor.point;
            (
                (point.line.0, point.column.0 as u32),
                crate::terminal::kitty::storage::VisibleRows::new(
                    grid.display_offset(),
                    grid.screen_lines(),
                ),
            )
        };
        let Some(scanner) = self.kitty_scanner.as_mut() else {
            return;
        };
        scanner.set_cursor(cursor.0, cursor.1);
        scanner.set_visible_rows(visible);
        // 与本地路径相同：光标推进在 APC 处理时立即完成，后续文本
        // 才会从正确的位置继续。
        let term = &self.term;
        scanner.scan_apc(content, &mut |rows, target_col| {
            let mut term = term.lock();
            for _ in 0..rows {
                term.linefeed();
            }
            term.goto_col(target_col as usize);
        });
    }

    /// 把扫描器攒下的协议应答回写给 PTY/SSH。
    fn flush_kitty_replies(&mut self) {
        let replies = match self.kitty_replies.lock() {
            Ok(mut buf) if !buf.is_empty() => std::mem::take(&mut *buf),
            _ => return,
        };
        self.write_raw(&replies);
    }

    /// 处理 Kitty DnD（OSC 72）命令。
    fn handle_kitty_dnd(&mut self, content: &[u8]) {
        let (reply, _action) = self.kitty_dnd.handle(content);
        if !reply.is_empty() {
            self.write_raw(&reply);
        }
    }

    /// 客户端是否已注册 Kitty DnD drop target。
    pub fn kitty_dnd_registered(&self) -> bool {
        self.kitty_dnd.is_registered()
    }

    /// 把原生文件拖入转成 Kitty `t=M` drop 事件；未注册时返回 false。
    ///
    /// GPUI 的 `on_drop` 不提供指针坐标，这里用 (0,0) 作为网格/像素位置；
    /// 客户端仍可从 `text/uri-list` 读取文件。hover/accept 阶段尚未接入。
    pub fn kitty_dnd_begin_file_drop(
        &mut self,
        paths: &[std::path::PathBuf],
        cell: (i32, i32),
        pixel: (i32, i32),
    ) -> bool {
        use crate::terminal::kitty::dnd::Item;

        if paths.is_empty() {
            return false;
        }
        let mut uris = String::new();
        let mut plain = String::new();
        for path in paths {
            uris.push_str(&file_uri(path));
            uris.push_str("\r\n");
            plain.push_str(&path.to_string_lossy());
            plain.push('\n');
        }
        let items = vec![
            Item {
                mime: "text/uri-list".to_string(),
                data: uris.into_bytes(),
            },
            Item {
                mime: "text/plain".to_string(),
                data: plain.into_bytes(),
            },
        ];
        let Some(reply) = self.kitty_dnd.begin_drop(items, cell, pixel) else {
            return false;
        };
        self.write_raw(&reply);
        true
    }

    /// 处理 Kitty clipboard（OSC 5522）请求。
    ///
    /// 写入/读取都走当前 GPUI 剪贴板；读取沿用 OSC 52 的既有信任模型
    /// （应用运行在终端里即可读），不额外弹权限窗。协议错误由解析层编码
    /// 成 `EINVAL`/`EFBIG` 回包。
    fn handle_kitty_clipboard(&mut self, cx: &mut Context<Self>, content: &[u8]) {
        use crate::terminal::kitty::clipboard::{self, Content, ReadData, Request};

        let request = self.kitty_clipboard.handle(content);
        if let Some(reply) = self.kitty_clipboard.take_reply() {
            self.write_raw(&reply);
        }
        match request {
            Request::None => {}
            Request::Write { content, .. } => match content {
                Content::Text(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                Content::Image { mime, data } => {
                    let format = match mime.as_str() {
                        "image/jpeg" => gpui::ImageFormat::Jpeg,
                        "image/webp" => gpui::ImageFormat::Webp,
                        "image/gif" => gpui::ImageFormat::Gif,
                        _ => gpui::ImageFormat::Png,
                    };
                    let image = gpui::Image::from_bytes(format, data);
                    cx.write_to_clipboard(ClipboardItem::new_image(&image));
                }
            },
            Request::Read {
                id,
                mimes,
                list,
                primary,
            } => {
                let item = cx.read_from_clipboard();
                let text = item.as_ref().and_then(|item| item.text());
                let image = item.as_ref().and_then(|item| {
                    item.entries.iter().find_map(|entry| match entry {
                        ClipboardEntry::Image(image) => {
                            Some((image.format.mime_type().to_string(), image.bytes.clone()))
                        }
                        _ => None,
                    })
                });
                let data = ReadData { text, image };
                let reply = clipboard::encode_read_response(&id, &mimes, list, primary, &data);
                self.write_raw(&reply);
            }
        }
    }

    /// 判定一条 OSC 是否由应用层消费。
    ///
    /// `cursor_at_sequence`：序列出现时刻的网格光标，由**读线程**在字节流里
    /// 就地取样后随通知带上来。只有本地路径能给（那条管线的字节在读线程解析，
    /// 等通知到 UI 线程时同一 chunk 的后续字节早已喂完）；远程路径传 `None`，
    /// 因为它在 UI 线程内**分段**喂入，处理到这里时光标恰好就是序列位置。
    ///
    /// 只有 `133;B`（输入区锚点）需要它，其余分支与光标无关。
    fn try_handle_osc_sequence(
        &mut self,
        cx: &mut Context<Self>,
        osc_content: &[u8],
        require_local_osc7_host: bool,
        cursor_at_sequence: Option<(i32, u32)>,
    ) -> bool {
        // Kitty DnD（OSC 72）：注册/查询/drop-target 数据请求。
        if let Some(rest) = osc_content.strip_prefix(b"72;") {
            self.handle_kitty_dnd(rest);
            return true;
        }
        // Kitty clipboard（OSC 5522）：解析/事务/应答编码在
        // `kitty::clipboard`，这里只负责系统剪贴板读写与回写 PTY。
        if let Some(rest) = osc_content.strip_prefix(b"5522;") {
            self.handle_kitty_clipboard(cx, rest);
            return true;
        }

        // agent 事件与桌面通知**共用** OSC 777 信道，所以这一判定必须排在
        // `parse_notification_osc` 之前。理由（已核实）：后者在 `777;notify;`
        // 分支里会把 sentinel 负载切成 `title="deepterm://cli-agent"`、
        // `body="<json>"`，于是弹出一条**虚假桌面通知**——hook 每报一次事件
        // 用户就收到一条垃圾通知。
        //
        // 为什么不需要 tty7 那句 `if title != Some(AGENT_EVENT_SENTINEL)` 显式检查：
        // tty7 的 `osc::parse_notification` 是通用解析器，必须回查 title 才能排除
        // sentinel；而本仓库的 `parse_agent_event`（`cli_agent.rs:550`）**硬剥**
        // `777;notify;deepterm://cli-agent;` 字面前缀，非此一律 `None`。
        // 因此"先试 agent 解析、`None` 再落通知解析"这个结构本身就已完备。
        //
        // 那句检查还**有害**：一条 title 恰好是字面量 `deepterm://cli-agent` 的
        // 真实用户通知会被静默吞掉，而这里会正确送达。**请勿**为了"对齐 tty7"补回来。
        if let Some(event) = crate::agent::parse_agent_event(osc_content) {
            self.apply_agent_events(&[event], cx);
            return true;
        }

        if Self::matches_osc_prefix(osc_content, "133;A") {
            self.remote_shell_ready = true;
            // shell 接管前台（刚画完提示符）→ 前台组可能刚从 agent 变回 shell。
            cx.emit(Event::ForegroundGroupChanged);
            return true;
        }

        if Self::matches_osc_prefix(osc_content, "133;B") {
            // 输入区起点标记，作为空行判定的锚点。
            // （兼容 `133;B;aid=...` 等带后缀的变体。）
            //
            // 光标必须取**序列出现时**的位置，不能现读 `Term`：
            // - 本地路径：同一 chunk 里 `133;B` 之后的输入回显早已被读线程
            //   解析完，现读拿到的是行尾，`input_line_blank` 于是把非空行
            //   误判为空行（`/` 前缀会被错误拦截成内建命令）。
            // - 远程路径：`process_pty_data` 按序列分段喂入，处理到这里时
            //   光标恰好就是锚点，`None` 分支现读即正确。
            let point = match cursor_at_sequence {
                Some((line, column)) => AlacPoint::new(Line(line), Column(column as usize)),
                None => {
                    let term = self.term.lock();
                    term.grid().cursor.point
                }
            };
            self.input_area_start = Some(point);
            // 提示符画完 = 上一条命令（含 agent）已结束。此时重查前台的代价
            // 最低、收益最高：agent 若已退出，图标此刻就该落回去。
            cx.emit(Event::ForegroundGroupChanged);
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
                // 远程检测是**纯字符串比较、无 fork**，所以就地同步执行，不必
                // 走 `TerminalView` 那套异步缓存（那套存在的理由是本地检测要
                // sysctl / 读 /proc）。顺带也不用等 2 秒 TTL。
                let agent =
                    CLIAgent::detect_from_argv_with(std::slice::from_ref(&proc), &HashMap::new());
                self.remote_process_name = Some(proc);
                self.set_foreground_agent(agent, cx);
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
        } else {
            let message = content.strip_prefix("9;")?;
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
        // - 本地 PTY 通过 `handle_local_osc` 进入，要求 host 是本机。
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
                self.note_output();
                cx.emit(Event::Wakeup);
                // 数据刚被 vte 处理：捕获应用 DECSET ?1004h 的开启时机
                // （本地终端的解析发生在 `local_pty` 读线程，远程在
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
                // cell 尺寸同样必须是物理像素，理由同 `handle_probe_sequences`。
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

    /// 对整个视口重建快照：铺满 `cells` 并据此重算 `row_ranges`。
    ///
    /// `Full` damage（滚动 / resize / 清屏…）与几何变化都走这里；增量路径
    /// 只覆盖脏行、不碰这两个缓冲，所以这段逻辑必须保持「全量且自洽」。
    fn rebuild_viewport_snapshot<'a>(
        cells: &mut Vec<IndexedCell>,
        row_ranges: &mut Vec<(usize, usize)>,
        display_iter: impl Iterator<
            Item = alacritty_terminal::grid::Indexed<&'a alacritty_terminal::term::cell::Cell>,
        >,
        num_rows: usize,
    ) {
        let estimated_size = display_iter.size_hint().0;
        cells.clear();
        if cells.capacity() < estimated_size {
            cells.reserve(estimated_size);
        }

        cells.extend(display_iter.map(|ic| {
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
    }

    /// 刷新 `last_content` 快照；返回 false 表示没拿到网格锁、快照未更新
    /// （调用方应保持 `content_dirty`，下一帧再试）。
    fn update_last_content(&mut self, force_full_damage: bool) -> bool {
        // 渲染侧不排队等网格锁。
        //
        // `Term` 由 `FairMutex` 保护，读线程解析一个 chunk 时会先拿 `lease()`
        // 占住 `next`，再持 `data` 直到 `MAX_LOCKED_READ`（64 KiB）喂完。UI
        // 线程若用公平的 `lock()`，就得排在那个 lease 后面——一个 pane 的写
        // 入速度会直接绑住整个窗口的帧率（同一 UI 线程要画所有 pane）。
        //
        // 所以先 `try_lock_unfair()` 抄近路（不排队、不阻塞）。拿不到说明读
        // 线程正在写网格，此时**本轮跳过**快照刷新：`content_dirty` 保持置位，
        // 下一个事件循环的 `sync()` 会再试一次，屏幕上最坏只是少画一帧——
        // 与 tty7 在 `TerminalElement::build_grid` 里的取舍同源。
        //
        // 连续跳过的次数有上限（[`MAX_SNAPSHOT_LOCK_SKIPS`]）：持续大流量下
        // 读线程喂完一个 chunk 就立刻回来重取锁，`try_lock_unfair` 可能一次次
        // 落空，屏幕就再也不更新了。超限后退回公平锁——它确实会排在 lease
        // 后面，但 lease 只在喂一个 chunk（最多 64 KiB）期间持有，代价有界，
        // 换来的是「画面一定跟得上」。
        //
        // 例外是 `force_full_damage`：resize / 主题换色这类路径要求快照与网格
        // 立刻一致（否则会拿旧尺寸的内容去配新网格），所以拿不到锁时它**不跳过**，
        // 而是退回公平锁排队。注意它并非「总是排队」——首选仍是上面的非阻塞
        // `try_lock_unfair`，只有在抢不到时才走到 `self.term.lock()`。
        let mut term = match self.term.try_lock_unfair() {
            Some(term) => {
                self.snapshot_lock_skips = 0;
                term
            }
            None if force_full_damage => self.term.lock(),
            None if self.snapshot_lock_skips >= MAX_SNAPSHOT_LOCK_SKIPS => self.term.lock(),
            None => {
                self.snapshot_lock_skips += 1;
                return false;
            }
        };

        // Collect damage info while we hold the lock, before resetting.
        let num_rows = self.terminal_bounds.num_lines();
        let num_cols = self.terminal_bounds.num_columns();
        // `Full` damage（滚动、resize、模式切换、清屏…）意味着视口里没有一行
        // 可以信任，必须整体重建快照；`Partial` 则只列出真正变过的行。
        let full_damage;
        if force_full_damage {
            // 在重置前先消费掉待处理的 damage，这样即使我们将强制进行
            // 全量失效，内部的 damage 追踪器状态也能保持一致。
            let _ = term.damage();
            full_damage = true;
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
                    full_damage = true;
                    Self::mark_viewport_fully_dirty(
                        &mut self.dirty_rows,
                        &mut self.dirty_row_bounds,
                        num_rows,
                        num_cols,
                    );
                }
                TermDamage::Partial(iter) => {
                    full_damage = false;
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
        let display_offset = content.display_offset;
        // 先拷贝 mode：快照结束、锁释放后还要用它补检焦点上报跳变。
        let mode = content.mode;

        // ── 视口快照：只有 `Partial` damage 时按行增量刷新 ──────────────────
        //
        // 旧实现每帧对**整个视口**（默认 80×24 = 1920 格，大窗口更多）重新
        // 构造 `IndexedCell`（每格 56 字节，含一次 `Arc` 克隆），即使只有一行
        // 变过。打字 / 回显这种稳态场景下，每帧的绝大部分工作都是白做的。
        //
        // 增量成立的前提由 alacritty 的 damage 语义保证：任何会让**视口整体
        // 错位**的操作（`scroll_display`、resize、`scroll_up_relative` /
        // `scroll_down_relative`、清屏、模式切换）都会 `mark_fully_damaged()`，
        // 于是我们只会看到 `TermDamage::Full`，走整体重建。`Partial` 因此
        // 意味着「只有这些行变了，且它们仍在原来的视口行号上」。
        //
        // 另外必须守住的两条：
        // - `display_offset` 变了必然是全量（同上，`scroll_display` 已置 Full），
        //   这里仍显式比对一次，作为不依赖上游实现的自身防线；
        // - 视口行列数变了（resize 的 reflow 可能早于 damage 被观察）同样是
        //   全量，否则行切片长度会对不上。
        let size_changed = self.last_content.cols != num_cols || self.last_content.rows != num_rows;
        let offset_changed = self.last_content.display_offset != display_offset;
        let geometry_changed = size_changed || offset_changed;
        let rebuild_all = full_damage || geometry_changed;
        // 部分帧若走了兜底全量重建，行切片也可能整体变了；行指纹需全量重算。
        let mut snapshot_rebuilt_all = rebuild_all;

        // 全量重建前留下上一帧的行指纹，用于识别“滚动位移”；部分更新
        // 只更新脏行，未变行沿用上一帧指纹。
        let previous_row_hashes = std::mem::take(&mut self.last_content.row_hashes);

        let cells = &mut self.last_content.cells;
        let row_ranges = &mut self.last_content.row_ranges;

        if rebuild_all {
            Self::rebuild_viewport_snapshot(cells, row_ranges, content.display_iter, num_rows);
            #[cfg(test)]
            {
                self.snapshot_cells_built += cells.len() as u64;
            }
        } else if row_ranges.len() == num_rows {
            // 行切片长度与视口行数一致 → 原地覆盖变过的行，其余保持不动。
            // 每行的 grid 行号由 display_offset 与行下标唯一确定，与整体重建
            // 得到的 `IndexedCell::point.line` 完全一致。
            #[cfg(test)]
            {
                self.incremental_snapshot_count += 1;
            }
            for (row_idx, (start, end)) in row_ranges.iter_mut().enumerate() {
                if !self.dirty_rows.get(row_idx).copied().unwrap_or(false) {
                    continue;
                }
                let line = alacritty_terminal::index::Line(row_idx as i32 - display_offset as i32);
                let cols = (*end - *start).min(num_cols);
                #[cfg(test)]
                {
                    self.snapshot_cells_built += cols as u64;
                }
                for col in 0..cols {
                    let point = AlacPoint::new(line, Column(col));
                    let cell = &term.grid()[point];
                    cells[*start + col] = IndexedCell {
                        point,
                        cell: PackedCell::from(cell),
                        zerowidth: packed_zerowidth(cell),
                        hyperlink: cell.hyperlink(),
                    };
                }
            }
        } else {
            // 兜底：行切片长度与视口行数不一致。按上面的论证这条路径**不可达**
            // （几何变化必然伴随 `Full` damage，已被 `rebuild_all` 拦下），
            // 全量测试也确实走不到它。保留它是为了让「不变量万一被上游改动
            // 打破」时退化成正确但慢的全量重建，而不是切片越界 panic。
            // 测试靠 `corrupt_row_ranges_for_test` 人为破坏不变量进来——防御性
            // 代码若从不执行，就无法证明它真能兜住。
            #[cfg(test)]
            {
                self.snapshot_fallback_count += 1;
            }
            Self::rebuild_viewport_snapshot(cells, row_ranges, content.display_iter, num_rows);
            snapshot_rebuilt_all = true;
            #[cfg(test)]
            {
                self.snapshot_cells_built += cells.len() as u64;
            }
        }

        // 行指纹：全量帧用于识别滚动位移（把整行缓存平移复用）；部分帧只
        // 更新真正重建过的脏行，未变行沿用上一帧指纹（内容没变，指纹不变）。
        let (row_hashes, scroll_shift) = if rebuild_all {
            // 复用 scratch 缓冲区：全量 damage（滚动/批量输出）每帧都会走到
            // 这里，旧实现每次都 `Vec::with_capacity` 新建一份。
            let mut hashes = std::mem::take(&mut self.row_hash_scratch);
            hashes.clear();
            hashes.reserve(row_ranges.len());
            for (start, end) in row_ranges.iter() {
                hashes.push(row_content_hash(&cells[*start..*end]));
            }
            // 只有视口行列变化时不能复用；display_offset 变化（用户滚动）
            // 正是行缓存平移最该生效的场景。
            let shift = if size_changed {
                0
            } else {
                detect_scroll_shift(&previous_row_hashes, &hashes)
            };
            // 上一帧的 Vec 归还给 scratch，与 `last_content.row_hashes`
            // 交替使用，稳态下不再分配。
            self.row_hash_scratch = previous_row_hashes;
            (hashes, shift)
        } else {
            // 部分更新复用上一帧的 Vec，保证「无变化帧零分配」仍然成立。
            //
            // 只有 `dirty_rows` 标记的行内容变过（alacritty 的 Partial
            // damage 语义保证其余行未移动），所以只需重算这些行的指纹；
            // 旧实现每次输出都重算整个视口，大窗口 / 高频输出下是纯浪费。
            let mut hashes = previous_row_hashes;
            let recompute_all = snapshot_rebuilt_all || hashes.len() != row_ranges.len();
            if recompute_all {
                hashes.clear();
                hashes.reserve(row_ranges.len());
            }
            for (row_idx, (start, end)) in row_ranges.iter().enumerate() {
                if recompute_all || self.dirty_rows.get(row_idx).copied().unwrap_or(false) {
                    let hash = row_content_hash(&cells[*start..*end]);
                    if recompute_all {
                        hashes.push(hash);
                    } else {
                        hashes[row_idx] = hash;
                    }
                    #[cfg(test)]
                    {
                        self.snapshot_rows_hashed += 1;
                    }
                }
            }
            (hashes, 0)
        };
        self.last_content.row_hashes = row_hashes;
        self.last_content.scroll_shift = scroll_shift;

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
        // 绝大多数帧颜色没变，先在栈上比较再决定是否重算指纹；`from_colors`
        // 本身只是一次 269 项拷贝，直接比较比每帧重新哈希便宜。
        let palette =
            crate::terminal::mappings::colors::TerminalPalette::from_colors(term.colors());
        if palette != self.last_content.palette {
            self.last_content.palette_hash = palette.hash_u64();
            self.last_content.palette = palette;
        }

        // 链接（URL / OSC8 / 文件路径）不再随每帧输出维护缓存：
        // 消费方只有 hover 命中测试，按需用 `last_content.cells` 现算，
        // 既不会因为「缓存由过期快照重建」而认错，也省掉了每帧扫描。

        // Wakeup 时抢不到锁而推迟的焦点模式检查，在这里补上（此时锁在手，
        // 但报告只读快照，不会二次加锁）。
        drop(term);
        if self.focus_check_pending {
            self.apply_focus_mode(mode);
        }

        // 快照已与本帧的网格对齐（调用方据此清除 `content_dirty`）。
        true
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

    /// 测试专用：强制走一次**整体重建**的视口快照。
    ///
    /// 供增量快照的平价测试当参考实现——两条路径是同一函数里的两个分支，
    /// 所以比对的是真实代码的等价性。
    #[doc(hidden)]
    pub fn full_snapshot_for_test(&mut self) {
        let _ = self.update_last_content(true);
    }

    /// 测试专用：增量快照分支被真正走过的次数。
    ///
    /// 平价测试必须证明它比的是「增量 vs 全量」，而不是「全量 vs 全量」——
    /// 否则测试会在增量路径彻底失效时依然全绿。这个计数器让那种失效可见。
    #[cfg(test)]
    pub fn incremental_snapshot_count_for_test(&self) -> u64 {
        self.incremental_snapshot_count
    }

    /// 测试专用：网格锁句柄，供饥饿测试人为占住锁。
    #[cfg(test)]
    pub fn term_lock_for_test(&self) -> Arc<FairMutex<Term<ZedListener>>> {
        self.term.clone()
    }

    /// 测试专用：置位内容脏标记，模拟「读线程送来了新输出」。
    #[cfg(test)]
    pub fn mark_content_dirty_for_test(&mut self) {
        self.content_dirty = true;
    }

    /// 测试专用：快照是否仍待刷新。
    #[cfg(test)]
    pub fn content_dirty_for_test(&self) -> bool {
        self.content_dirty
    }

    /// 测试专用：累计重建了多少个 `IndexedCell`。
    ///
    /// 这是增量快照收益的**确定性**度量——不受机器负载影响，比 wall-clock
    /// 更能说明「每帧少做多少工作」。
    #[cfg(test)]
    pub fn snapshot_cells_built_for_test(&self) -> u64 {
        self.snapshot_cells_built
    }

    /// 测试专用：累计重算了多少行指纹。
    #[cfg(test)]
    pub fn snapshot_rows_hashed_for_test(&self) -> u64 {
        self.snapshot_rows_hashed
    }

    /// 测试专用：立刻应用挂起的 resize（绕过 25ms 合并定时器）。
    #[cfg(test)]
    pub fn apply_pending_resize_for_test(&mut self, cx: &mut Context<Self>) {
        self.apply_pending_resize(false, cx);
    }

    /// 测试专用：人为破坏「`row_ranges.len() == 视口行数`」这个不变量，
    /// 逼 [`Terminal::update_last_content`] 走兜底的全量重建分支。
    ///
    /// 该分支在生产路径上不可达（几何变化必然伴随 `Full` damage，已被
    /// `rebuild_all` 拦下），但它存在的意义正是「万一上游改动打破了不变量，
    /// 退化成正确但慢的全量重建，而不是切片越界 panic」。防御性代码若从不
    /// 被执行，就无法证明它真的能兜住——这个钩子让测试能真的走进去。
    #[cfg(test)]
    pub fn corrupt_row_ranges_for_test(&mut self) {
        // 丢掉最后一行切片：`len()` 与视口行数不再相等，但剩余切片仍然
        // 自洽（不会越界），这样兜底分支能顺利重建而不是先 panic。
        self.last_content.row_ranges.pop();
    }

    /// 测试专用：当前行切片数量（配合 `corrupt_row_ranges_for_test` 断言）。
    #[cfg(test)]
    pub fn row_range_count_for_test(&self) -> usize {
        self.last_content.row_ranges.len()
    }

    /// 测试专用：兜底全量重建分支的命中次数。
    #[cfg(test)]
    pub fn snapshot_fallback_count_for_test(&self) -> u64 {
        self.snapshot_fallback_count
    }

    fn sync_inner(&mut self, cx: &mut Context<Self>) {
        if self.content_dirty {
            // 拿不到网格锁时快照**保持旧值**、`content_dirty` 保持置位：这一帧
            // 用上一帧的内容画，稍后再试。屏幕最多晚一帧，但整个窗口不会被
            // 某个 pane 的读线程拖住。
            if self.update_last_content(false) {
                self.content_dirty = false;
                self.snapshot_retry_task = None;
            } else {
                self.schedule_snapshot_retry(cx);
            }
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

    /// 排一个补帧任务，确保跳过快照刷新后**一定**还会再 `sync` 一次。
    ///
    /// 为什么不能只依赖「下一个事件循环」：`sync` 的唯一调用点是元素
    /// `prepaint`，而 GPUI 不按 vsync 空转。若这次跳过之后不再有新输出，
    /// 就再没有任何东西会驱动一帧——`content_dirty` 永久置位、画面停在旧内容
    /// 上。触发条件并不罕见：读线程退出前的 drain、SSH 断连、`ChildExit`
    /// 这类「最后一次唤醒」只要撞上被占住的锁就会中招。
    ///
    /// 也不能靠 `cx.notify()` / `Window::refresh()`：两者在 prepaint 期间都
    /// 不排队新帧（`Window::refresh` 只在 `draw_phase == None` 时生效，
    /// `notify` 在绘制中只标脏当前帧）。所以用一个短定时器绕开绘制阶段，
    /// 代价是跳帧时多一次 8ms 的定时器。
    ///
    /// 已有任务在跑时不重复排：持续跳过只需一个在途任务，它每次唤醒都会再排，
    /// 直到某一帧真的把快照刷完。
    fn schedule_snapshot_retry(&mut self, cx: &mut Context<Self>) {
        if self.snapshot_retry_task.is_some() {
            return;
        }
        self.snapshot_retry_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SNAPSHOT_RETRY_DELAY).await;
            let _ = this.update(cx, |terminal, cx| {
                terminal.snapshot_retry_task = None;
                if terminal.content_dirty {
                    // 直接重试一次快照；仍然拿不到锁就再排下一个补帧。
                    if terminal.update_last_content(false) {
                        terminal.content_dirty = false;
                    } else {
                        terminal.schedule_snapshot_retry(cx);
                    }
                    // 让渲染层重新跑一遍 prepaint，把新快照画出来。
                    cx.notify();
                }
            });
        }));
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

    /// 一次输入写入是否是用户在要求 agent 停下：裸 <kbd>Esc</kbd> 或
    /// <kbd>Ctrl+C</kbd>，含 kitty 键盘协议与 xterm `modifyOtherKeys` 编码
    /// （TUI 常把终端切到那两种模式）。
    ///
    /// 只有**整段写入**都恰好是这个键才算——按键总是单独到达，而一次粘贴
    /// 里恰好含 `0x03` 不是按键。
    ///
    /// Grok Build 是例外：它的 Esc 会让回合继续跑，把它当中断会把还在工作的
    /// 回合判成结束。
    fn is_interrupt_write(&self, data: &[u8]) -> bool {
        let Some(agent) = self.foreground_agent else {
            return false;
        };
        match data {
            // 裸 Esc。
            b"\x1b" => return agent != CLIAgent::Grok,
            // 裸 Ctrl+C：legacy 编码或 kitty 的 Ctrl+C（CSI 99;5u 的简写形式）。
            b"\x03" | b"\x1b[27;5;99~" => return true,
            _ => {}
        }
        // kitty 键盘协议 / modifyOtherKeys：CSI <code>;<mods>[:<event>]u。
        let Some(body) = data
            .strip_prefix(b"\x1b[")
            .and_then(|b| b.strip_suffix(b"u"))
            .and_then(|b| std::str::from_utf8(b).ok())
        else {
            return false;
        };
        let mut fields = body.split(';');
        let code = fields
            .next()
            .and_then(|f| f.split(':').next())
            .and_then(|c| c.parse::<u32>().ok());
        let mut modifiers = fields.next().unwrap_or("1").split(':');
        let mods = modifiers
            .next()
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or(1);
        // 只认按下（event 1）；缺失视为按下。
        let event = modifiers.next().map_or(Some(1), |e| e.parse::<u32>().ok());
        if event != Some(1) {
            return false;
        }
        // Caps Lock / Num Lock 搭在掩码里但不改变按键本身。
        const LOCKS: u32 = 64 | 128;
        match (code, mods.saturating_sub(1) & !LOCKS) {
            (Some(27), 0) => agent != CLIAgent::Grok,
            (Some(99), 4) => true,
            _ => false,
        }
    }

    /// 记下用户按了打断键，等待一个宽限窗再下结论。
    ///
    /// 真报了中断的 agent（Kimi 的 `Interrupt`、与按键赛跑的 `Stop`）会在窗内
    /// 发事件，那时 [`Self::apply_agent_events`] 会清掉这个 pending；窗过了还
    /// 没事件，才由 [`Self::settle_agent_interrupt`] 推断回合结束。
    fn note_interrupt_key(&mut self) {
        self.agent_interrupt_pending = Some(std::time::Instant::now());
    }

    /// 宽限窗过后仍无事件：把回合判成已结束。
    ///
    /// 由 `TerminalView` 的定时器驱动（见 [`AGENT_INTERRUPT_SETTLE`]）。
    pub fn settle_agent_interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.agent_interrupt_pending else {
            return;
        };
        if pending.elapsed() < AGENT_INTERRUPT_SETTLE {
            return;
        }
        self.agent_interrupt_pending = None;
        let before = self.agent_session.clone();
        if let Some(session) = self.agent_session.as_mut() {
            session.assume_interrupted();
        }
        if self.agent_session != before {
            cx.emit(Event::AgentChanged);
        }
    }

    /// 放弃一个静默过久的回合（见 [`AgentSessionState::assume_stale`]）。
    ///
    /// 同样由 `TerminalView` 的定时器驱动。阈值远大于任何真实回合的安静
    /// 段：每个工具跑完、每次权限询问都会重置计时。
    pub fn expire_stale_agent(&mut self, cx: &mut Context<Self>) {
        let working = self
            .agent_session
            .as_ref()
            .is_some_and(|s| s.status == crate::agent::AgentStatus::Working);
        if !working {
            return;
        }
        let Some(since) = self.agent_last_event else {
            // 从未收到过事件（跨视图带入的 session）：从现在起算，给一次机会。
            self.agent_last_event = Some(std::time::Instant::now());
            return;
        };
        if since.elapsed() < AGENT_STALE_AFTER {
            return;
        }
        let before = self.agent_session.clone();
        if let Some(session) = self.agent_session.as_mut() {
            session.assume_stale();
        }
        if self.agent_session != before {
            cx.emit(Event::AgentChanged);
        }
    }

    /// 是否有待处理的 agent 计时器工作（宽限窗或静默过期）。
    ///
    /// 供 `TerminalView` 决定要不要起定时器：两者都没有时不空转。
    pub fn has_pending_agent_timers(&self) -> bool {
        self.agent_interrupt_pending.is_some()
            || self
                .agent_session
                .as_ref()
                .is_some_and(|s| s.status == crate::agent::AgentStatus::Working)
    }

    /// 下一次 agent 计时器该在多久后醒来：离最近的待办还有一半时间。
    ///
    /// 宽限窗未过时是亚秒级；只剩静默过期时是分钟级。下限 `min` 防止 0 间隔
    /// busy-loop，上限是静默过期本身的 30 分钟天花板。
    ///
    /// 正在输出时有第三个期限：输出窗口的末尾（用来把「正在输出」翻回 false，
    /// 关掉 tab 扫光）。这个期限**不减半**——它是准确时刻而不是预估提前量，
    /// 减半只会让它连续醒十几次才走到。
    pub fn agent_timer_delay(&self, min: Duration) -> Duration {
        let mut wait = AGENT_STALE_AFTER;
        if let Some(pending) = self.agent_interrupt_pending {
            wait = wait.min(AGENT_INTERRUPT_SETTLE.saturating_sub(pending.elapsed()));
        }
        if let Some(since) = self.agent_last_event {
            wait = wait.min(AGENT_STALE_AFTER.saturating_sub(since.elapsed()));
        } else if self
            .agent_session
            .as_ref()
            .is_some_and(|s| s.status == crate::agent::AgentStatus::Working)
        {
            // 从没收到过事件：先醒一次，把静默计时的起点定下来。
            wait = Duration::ZERO;
        }
        // 留一半余量：事件可能比预期早到，早醒一次比晚醒一次好。
        let delay = (wait / 2).max(min).min(AGENT_STALE_AFTER);
        match self.output_stream_remaining(AGENT_STREAM_WINDOW) {
            Some(remaining) => delay.min(remaining.max(min)),
            None => delay,
        }
    }

    pub fn write(&mut self, data: &[u8]) {
        // 用户输入即将进入 PTY：维护输入区锚点（见 input_area_start 文档）。
        // - shell 前台且尚无锚点 → 以当前光标（提示符末尾）为锚点；
        // - 写入回车 → 提示符即将重绘，锚点失效，等待重新捕获。
        //
        // 若 shell 没有上报 OSC 133;B（未注入集成脚本），这条回显锚点路径
        // 就是本地空行判定的唯一依据。
        self.observe_input_state();
        if memchr::memchr2(b'\r', b'\n', data).is_some() {
            self.input_area_start = None;
        }

        // 打断键：起宽限窗，让 agent 有机会先自己报中断。
        if self.is_interrupt_write(data) {
            self.note_interrupt_key();
        }

        if let Some(ref sender) = self.local_sender {
            sender.input(data.to_vec());
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
        if let Some(ref sender) = self.local_sender {
            sender.input(data.to_vec());
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
    /// - 本地 PTY：向读线程发送 `Shutdown`，读线程退出并释放 `Pty`（回收 shell 子进程）。
    /// - russh 远程：发送 `InputMsg::Close`（连接任务仅在收到 Close 或发送端断开时退出；
    ///   仅 drop `RusshTerminal` 无法终止连接，因为 `state` 被任务自身持有）。
    /// - PTY 模式远程：显式 `close()` 发 `Shutdown`；只靠 drop 回调闭包会让
    ///   alacritty 事件循环在 channel 断开时 panic，且 ssh 子进程可能挂着。
    pub fn kill(&mut self) {
        // 本地终端：通知读线程退出，释放 PTY / 回收 shell 子进程。
        if let Some(sender) = &self.local_sender {
            sender.shutdown();
        }
        // PTY 模式 SSH：显式关闭事件循环与 ssh 子进程。
        if let Some(pty) = &self.ssh_pty_terminal {
            pty.close();
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
        let has_local_sink = self.local_sender.is_some() || self.remote_resize_callback.is_some();
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
                // 宽窗口会把“按行”的历史线性放大（250 列 × 10k 行 ≈ 60 MB）。
                // 必须在 reflow **之前**按新列数缩历史：否则 resize 会先把
                // 10k 行全部扩到 250 列，峰值仍然顶到 60 MB，缩完才降下来。
                // 只缩不涨，避免窗口变窄时重新申请几千行。
                let desired = scrollback_lines_for_cols(self.scrollback_requested, cols);
                if term.grid().history_size() > desired {
                    term.set_options(Self::terminal_config_with_scrollback(desired));
                }
                term.resize(alacritty_terminal::term::test::TermSize::new(cols, rows));
            }
            // 对应 ghostty `Terminal.resize` 尾部的 `flags.dirty.clear = true`：
            // resize 后强制渲染层全量重绘，行缓存不留任何旧尺寸的残留。
            self.input_area_start = None;
            // resize 后必须拿到与新几何一致的内容：`force_full_damage` 保证
            // 抢不到锁时会退回公平锁排队（而不是跳过本帧）。
            let _ = self.update_last_content(true);
        }

        // 注意：不能因 cols/rows 未变就跳过下面的通知。跨显示器拖动只改 DPI、
        // 不改网格，此时 PTY 的 `ws_xpixel` 与读线程扫描器的几何都必须刷新。
        //
        // 本地终端：通过读线程发送 resize 消息（cell 尺寸已按显示缩放换算成
        // 物理像素，读线程据此填扫描器几何并转给 PTY/ConPTY）。
        if let Some(ref sender) = self.local_sender {
            sender.resize(bounds.window_size(self.display_scale));
        }
        // 远程终端：通过回调通知 SSH 通道调整窗口大小（带上缩放，理由见回调类型）
        if let Some(ref callback) = self.remote_resize_callback {
            callback(cols as u16, rows as u16, self.display_scale);
        }
        // 远程终端的字节在 UI 线程解析，没有读线程替扫描器维护几何，只能在这里
        // 推给它（本地路径由 LocalReader 负责）。
        self.sync_remote_scanner_geometry();

        // in-band size reports（`?2048`）在远程路径上由本层应答并**摘除**，
        // 序列进不到扫描器（见 `handle_probe_sequences`），所以扫描器的
        // `set_geometry` 重报不会发生——必须用本层自己的 `remote_mode_2048`
        // 补一次。ghostty `stream_terminal.zig` 的 `resize()` 同样是在模式
        // 启用期间每次尺寸变化都主动上报。
        if self.remote_mode_2048 {
            let report = crate::terminal::escape_scan::encode_size_report(
                crate::terminal::escape_scan::SizeReport::Mode2048,
                self.terminal_bounds.term_geometry(self.display_scale),
            );
            self.write_raw(report.as_bytes());
        }
        // 对应 ghostty `renderer_mailbox.push(.resize)` + `renderer_wakeup`。
        if wake {
            cx.emit(Event::Wakeup);
        }
    }

    /// 把当前几何告知远程扫描器。
    ///
    /// 尺寸上报（`CSI 16 t`/2048）由扫描器直接应答，删除类命令（`d=c` 等）
    /// 还要拿单元格尺寸把放置的像素尺寸换算成网格矩形——两者都需要几何。
    /// 本地路径在读线程里自己维护，远程路径没有读线程，只能在这里推。
    fn sync_remote_scanner_geometry(&mut self) {
        let Some(scanner) = self.kitty_scanner.as_mut() else {
            return;
        };
        scanner.set_geometry(self.terminal_bounds.term_geometry(self.display_scale));
    }

    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    /// 每个可见行的内容指纹（与 `last_content().row_ranges` 等长）。
    pub fn row_hashes(&self) -> &[u64] {
        &self.last_content.row_hashes
    }

    /// 本帧相对上一帧的滚动位移；0 = 无整体滚动。
    pub fn scroll_shift(&self) -> isize {
        self.last_content.scroll_shift
    }

    /// 释放可重建的视口快照缓冲（隐藏标签用）。
    ///
    /// 只丢弃 `cells/row_ranges/row_hashes` 的堆容量，不碰网格；下一帧
    /// `content_dirty` 会走全量重建。
    pub fn release_snapshot_buffers(&mut self) {
        self.last_content.cells = Vec::new();
        self.last_content.row_ranges = Vec::new();
        self.last_content.row_hashes = Vec::new();
        self.last_content.scroll_shift = 0;
        self.content_dirty = true;
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

    /// 取出 `viewport_row` 所在的软换行链（见 [`LinkChain`]）。
    ///
    /// 软换行在网格里是多行、在文本上是一个 token。长链接/长路径常被折断，
    /// 只看被点的那一行会把它腰斩成假链接（`https://exa` + `mple.com`，
    /// 或者后半截 `le.com/x` 被当成独立域名），所以识别必须按整条链拼起来
    /// 算，命中区再按行切开。
    ///
    /// **向前回溯是必须的**：续行的前半截在上一行，不回溯就等于拿残缺的一
    /// 半去做词法分析 —— 那正是「识别错误」的主要来源。链首贴着视口顶端
    /// 时，前半截还可能落在视口上方的 scrollback 里，能补则补；补不到就
    /// 保持「不认」，绝不「认错」。
    fn link_row_chain(&self, viewport_row: usize) -> LinkChain<'_> {
        let content = &self.last_content;
        if content.row_ranges.get(viewport_row).is_none() {
            return LinkChain {
                clicked_row: 0,
                rows: Vec::new(),
            };
        }
        // 向上回溯：上一行带 WRAPLINE ⟹ 它的内容是被点这行接续的。
        let mut first = viewport_row;
        while first > 0
            && viewport_row - first < MAX_LINK_CHAIN_ROWS
            && self.viewport_row_wrapped(first - 1)
        {
            first -= 1;
        }
        // 视口内的行已经占掉多少链预算，剩下的留给 scrollback 前缀。
        let prefix = if first == 0 {
            let budget = MAX_LINK_CHAIN_ROWS.saturating_sub(viewport_row - first + 1);
            self.scrollback_rows_above_viewport(budget)
        } else {
            Vec::new()
        };
        let prefix_len = prefix.len();
        let mut rows: Vec<Cow<'_, [IndexedCell]>> = prefix.into_iter().map(Cow::Owned).collect();
        let mut cursor = first;
        while let Some((start, end)) = content.row_ranges.get(cursor).copied() {
            rows.push(Cow::Borrowed(&content.cells[start..end]));
            // 只有「已越过被点的行」之后，链的终点才由 WRAPLINE 决定。
            if cursor >= viewport_row && !self.viewport_row_wrapped(cursor) {
                break;
            }
            if rows.len() >= MAX_LINK_CHAIN_ROWS || cursor + 1 >= content.row_ranges.len() {
                break;
            }
            cursor += 1;
        }
        LinkChain {
            clicked_row: prefix_len + (viewport_row - first),
            rows,
        }
    }

    /// 从视口上方最近的一行开始向上收集连续的 `WRAPLINE` 行（最多 `limit`
    /// 行），按文本顺序（旧→新）返回。
    ///
    /// 只有 hover 命中测试才走这里，而且只在链首贴着视口顶端（`first == 0`）
    /// 时；拿不到 `term` 锁就直接放弃 —— 宁可少认，也不能让鼠标移动被锁卡住。
    /// 到历史顶或遇到不带 `WRAPLINE` 的行即停：越界会读到压缩缓冲区的垃圾。
    fn scrollback_rows_above_viewport(&self, limit: usize) -> Vec<Vec<IndexedCell>> {
        if limit == 0 {
            return Vec::new();
        }
        let Some(term) = self.term.try_lock_unfair() else {
            return Vec::new();
        };
        let grid = term.grid();
        let display_offset = grid.display_offset();
        if display_offset == 0 {
            return Vec::new();
        }
        // 视口第 0 行是 `Line(-display_offset)`，它上面一行再减一。
        let mut line = Line(-(display_offset as i32) - 1);
        let oldest = Line(-(grid.history_size() as i32));
        let mut out: Vec<Vec<IndexedCell>> = Vec::new();
        while out.len() < limit && line >= oldest {
            let row = &grid[line];
            let wrapped = row.into_iter().any(|c| {
                c.flags
                    .contains(alacritty_terminal::term::cell::Flags::WRAPLINE)
            });
            if !wrapped {
                break;
            }
            let mut cells = Vec::with_capacity(row.len());
            for (col, cell) in row.into_iter().enumerate() {
                cells.push(IndexedCell {
                    point: AlacPoint::new(line, Column(col)),
                    cell: PackedCell::from(cell),
                    zerowidth: packed_zerowidth(cell),
                    hyperlink: cell.hyperlink(),
                });
            }
            out.push(cells);
            line -= 1;
        }
        out.reverse();
        out
    }

    /// 命中测试：被点 viewport 位置在软换行链上的三种链接候选。
    ///
    /// 按需从 `last_content.cells` 现算（不缓存）：hover 是唯一消费方，
    /// 每帧为没人看的行做扫描纯属浪费，而缓存又会因为「重建时快照已过期」
    /// 而认错链接。三种识别共用同一条链；消费方（`TerminalView`）负责按
    /// `(行, 列, 内容代次, …)` 记住上一格的结果。
    ///
    /// 识别范围是被点行所在的整条软换行链（前后都算），但每种命中都只返回
    /// 被点那一行上的区间：hover 高亮不该跑到相邻行去。
    pub fn link_hits_at_viewport_position(&self, viewport_row: usize, col: usize) -> LinkHits {
        use crate::terminal::file_link_detector::{detect_files_in_rows, file_hit_at_col};
        let chain = self.link_row_chain(viewport_row);
        if chain.rows.is_empty() {
            return LinkHits::default();
        }
        let fits = |row: usize, start_col: usize, end_col: usize| {
            row == chain.clicked_row && col >= start_col && col < end_col
        };
        let hyperlink =
            crate::terminal::link_detector::detect_hyperlinks_in_rows(chain.row_slices())
                .into_iter()
                .find(|(row, range)| fits(*row, range.start_col, range.end_col))
                .map(|(_, range)| (range.uri, range.id, range.start_col, range.end_col));
        let url = crate::terminal::link_detector::detect_urls_in_rows(chain.row_slices())
            .into_iter()
            .find(|(row, range)| fits(*row, range.start_col, range.end_col))
            .map(|(_, range)| (range.url.into(), range.start_col, range.end_col));
        let file = {
            let hits = detect_files_in_rows(chain.row_slices());
            file_hit_at_col(&hits, chain.clicked_row, col).cloned()
        };
        LinkHits {
            hyperlink,
            url,
            file,
        }
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
        // - 本地 PTY：仅 drop `local_sender` 只关闭通道，读线程还阻塞在
        //   `Poller::wait` 上；必须显式发 `Shutdown` 让它退出并释放 `Pty`
        //   （触发 SIGHUP / `ClosePseudoConsole`，回收 shell 子进程）。
        // - russh 远程：连接任务仅在收到 `InputMsg::Close`（或发送端断开）时退出，
        //   仅 drop `RusshTerminal` 无法终止连接，必须显式 `close()`。
        // - PTY 模式远程：显式 `close()`，否则事件循环在 sender 断开时 panic。
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
    /// `text` 中每个字符对应的 grid 列号；`None` 表示列号就是从 0 开始的
    /// 连续序列（绝大多数 terminal 行都是这种形态，省掉一条 `Vec<u32>`）。
    pub cols: Option<Vec<u32>>,
    /// 每个字符占的 cell 宽（1 或 2）；`None` 表示全为 1（常见路径）。
    pub widths: Option<Vec<u8>>,
}

impl LineSnapshot {
    #[inline]
    fn col_at(&self, index: usize) -> usize {
        self.cols
            .as_ref()
            .map_or(index, |cols| cols[index] as usize)
    }

    #[inline]
    fn width_at(&self, index: usize) -> usize {
        self.widths
            .as_ref()
            .map_or(1, |widths| widths[index] as usize)
    }
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
        // Most rows are a contiguous run of single-width chars; keep the
        // per-char column/width tables lazy so the common case allocates only
        // the text `String` instead of three allocations per line.
        let mut cols_of: Option<Vec<u32>> = None;
        let mut widths_of: Option<Vec<u8>> = None;
        let mut char_count = 0usize;
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
            if col != char_count {
                cols_of.get_or_insert_with(|| (0..char_count as u32).collect());
            }
            if wide == 2 {
                widths_of.get_or_insert_with(|| vec![1u8; char_count]);
            }
            text.push(cell.c);
            if let Some(cols_of) = cols_of.as_mut() {
                cols_of.push(col as u32);
            }
            if let Some(widths_of) = widths_of.as_mut() {
                widths_of.push(wide);
            }
            char_count += 1;
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
                let start_col = line.col_at(start);
                // 结束列：优先用下一个字符的列号（稀疏行会自然跨过空档），
                // 行尾则用命中尾字符的列 + 占宽。
                let last = start + window - 1;
                let end_col = if last + 1 < folded.len() {
                    line.col_at(last + 1)
                } else {
                    line.col_at(last) + line.width_at(last)
                };
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
    /// 本地路径的 OSC 通知类型。
    use crate::terminal::local_pty::LocalNotice;
    use alacritty_terminal::term::test::mock_term;
    use alacritty_terminal::vte::ansi::Handler;
    use gpui::{AppContext as _, Bounds, Entity, TestAppContext, point, px, size};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[gpui::test]
    fn focus_check_never_blocks_on_grid_lock(cx: &mut TestAppContext) {
        let terminal = cx.update(|cx| Terminal::new_headless(cx, 1_000));
        terminal.update(cx, |terminal, _cx| {
            // 模拟读线程持 lease/网格锁：Wakeup 里的检查必须立刻返回并置
            // pending，而不是排队等锁。克隆 Arc 只是为了让 guard 不借用
            // `terminal` 本身，避免与下面的 `&mut` 调用冲突。
            let term = terminal.term.clone();
            let guard = term.lock();
            let started = std::time::Instant::now();
            terminal.check_focus_mode_enabled();
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "拿不到网格锁时不应阻塞"
            );
            assert!(terminal.focus_check_pending);
            drop(guard);

            // 锁释放后的下一轮检查应清掉 pending，并正常应用模式。
            terminal.check_focus_mode_enabled();
            assert!(!terminal.focus_check_pending);
        });
    }

    #[test]
    fn scrollback_budget_shrinks_wide_windows_only() {
        // 31 列默认几何下 10k 行仍在 32 MB 预算内。
        assert_eq!(super::scrollback_lines_for_cols(10_000, 31), 10_000);
        // 250 列：每行 32 + 24*250 = 6032 B，32 MB / 6032 ≈ 5563 行。
        let wide = super::scrollback_lines_for_cols(10_000, 250);
        assert!((5_000..=5_800).contains(&wide), "wide={wide}");
        // 用户显式配置更小时不放大。
        assert_eq!(super::scrollback_lines_for_cols(2_000, 31), 2_000);
        // 不允许超过全局上限。
        assert!(super::scrollback_lines_for_cols(usize::MAX, 31) <= super::MAX_SCROLLBACK_LINES);
    }

    #[gpui::test]
    fn user_scroll_exposes_positive_shift(cx: &mut TestAppContext) {
        let terminal = cx.update(|cx| Terminal::new_headless(cx, 1_000));
        terminal.update(cx, |t, cx| {
            let mut bytes = Vec::new();
            for i in 0..80 {
                bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
            }
            t.handle_remote_data(&bytes, cx);
            t.sync_for_test(cx);
            assert!(t.scroll_shift() == 0, "首次快照不应有位移");

            t.scroll_line_up();
            t.sync_for_test(cx);
            // 视口向历史方向移动：新第 i 行 = 旧第 i-1 行，位移是 -1。
            assert_eq!(t.scroll_shift(), -1, "上滚一行应识别为 -1");
        });
    }

    #[test]
    fn detect_scroll_shift_recognizes_one_line_and_clear() {
        let old = [1u64, 2, 3, 4, 5, 6];
        // 上滚一行：新行 = 旧行 i+1，最后一行是全新内容。
        let new = [2u64, 3, 4, 5, 6, 99];
        assert_eq!(super::detect_scroll_shift(&old, &new), 1);
        // 下滚一行：新行 = 旧行 i-1。
        let new = [99u64, 1, 2, 3, 4, 5];
        assert_eq!(super::detect_scroll_shift(&old, &new), -1);
        // 清屏/无关重绘：不应误判成滚动。
        let new = [99u64, 98, 97, 96, 95, 94];
        assert_eq!(super::detect_scroll_shift(&old, &new), 0);
    }

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

    /// 回归：新建的本地终端必须持有输入通道（`local_sender`），否则用户打的
    /// 每个字符都被 `Terminal::write` 静默丢弃。
    ///
    /// 症状极具迷惑性：终端能打开、提示符照常出现（输出侧正常，因为解析
    /// PTY 字节的读线程在跑），但**打不进任何字**。
    ///
    /// 历史上这是平台接线漏了一条的 bug：unix 走 `local_sender`、windows 走
    /// alacritty `EventLoop` 的 `sender`，而 `EventLoop::spawn` 会消费 self，
    /// channel 必须在 spawn **之前**取走。现在两条平台统一走 `LocalReader`，
    /// 这条不变量照着钉：本地终端必有 `local_sender`。
    #[gpui::test]
    fn local_terminal_keeps_an_input_channel(cx: &mut TestAppContext) {
        let terminal = cx.update(|cx| Terminal::new(cx, None));
        terminal.update(cx, |t, _| {
            assert!(
                t.local_sender.is_some(),
                "本地终端必须持有 LocalReader 的 sender，否则输入被静默丢弃"
            );
        });
        // 显式 drop：LocalReader 会 join gather/parse，确保线程在 GPUI 测试
        // 调度器检查前退出；否则 parser 线程退出时 drop listener 唤醒 UI
        // 任务，会被 `#[gpui::test]` 判成「非确定性线程活动」。
        drop(terminal);
    }

    /// 计时器的等待时间必须按「离待办还有多久」算。
    ///
    /// 以前写成 `elapsed - deadline`：到期前恒为 0，agent 干活时每 250ms 醒
    /// 一次，最长持续 30 分钟——正是文档里承诺「分钟级」的反面。
    #[test]
    fn agent_timer_delay_counts_down_to_the_deadline() {
        use crate::agent::{AgentSessionState, AgentStatus};
        use std::time::Instant;

        let min = Duration::from_millis(250);
        let mut term = Terminal::create_error_terminal();

        // 打断宽限窗：1 秒后结算，醒来必须在这一秒之内。
        term.agent_interrupt_pending = Some(Instant::now());
        let delay = term.agent_timer_delay(min);
        assert!(
            delay <= super::AGENT_INTERRUPT_SETTLE && delay >= min,
            "打断宽限窗的等待时间不对：{delay:?}"
        );

        // 静默过期：离到期还有 2 秒 → 一半时间醒来。
        term.agent_interrupt_pending = None;
        term.agent_session = Some(AgentSessionState {
            status: AgentStatus::Working,
            ..Default::default()
        });
        term.agent_last_event =
            Some(Instant::now() - (super::AGENT_STALE_AFTER - Duration::from_secs(2)));
        let delay = term.agent_timer_delay(min);
        assert!(
            delay <= Duration::from_secs(2) && delay >= min,
            "静默过期的等待时间不对：{delay:?}"
        );

        // 刚收到事件：离到期还早，不该被 250ms 的下限拖着空转。
        term.agent_last_event = Some(Instant::now());
        let delay = term.agent_timer_delay(min);
        assert!(
            delay > Duration::from_secs(60),
            "回合刚开始时不该频繁醒来：{delay:?}"
        );
    }

    /// 输出停下后，计时器必须排在输出窗口末尾醒来。
    ///
    /// 否则「不再输出」要等到下一个 hook 事件或 30 分钟的静默兜底才被看见，
    /// tab 的扫光就会在 agent 安静下来以后继续转几十分钟。
    #[test]
    fn agent_timer_wakes_at_the_end_of_the_output_window() {
        use crate::agent::{AgentSessionState, AgentStatus};
        use std::time::Instant;

        let min = Duration::from_millis(250);
        let mut term = Terminal::create_error_terminal();
        term.agent_session = Some(AgentSessionState {
            status: AgentStatus::Working,
            ..Default::default()
        });
        term.agent_last_event = Some(Instant::now());

        // 刚有输出：窗口还剩 ~800ms，计时器必须在窗口末尾醒来。
        term.last_output_at = Some(Instant::now());
        assert!(
            term.output_streaming(super::AGENT_STREAM_WINDOW),
            "刚写入的终端必须算「正在输出」"
        );
        let delay = term.agent_timer_delay(min);
        assert!(
            delay <= super::AGENT_STREAM_WINDOW && delay >= Duration::from_millis(700),
            "输出窗口的唤醒点不对：{delay:?}"
        );

        // 窗口已过：不再算正在输出，计时器回到长时间的静默等待。
        term.last_output_at = Some(Instant::now() - super::AGENT_STREAM_WINDOW * 2);
        assert!(
            !term.output_streaming(super::AGENT_STREAM_WINDOW),
            "超过窗口仍算「正在输出」，扫光会停不下来"
        );
        let delay = term.agent_timer_delay(min);
        assert!(
            delay > Duration::from_secs(60),
            "输出已停时不该频繁醒来：{delay:?}"
        );
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

    /// 端到端：windows 本地终端里写入的字节必须真的到达子进程。
    ///
    /// `local_terminal_keeps_an_input_channel` 只钉住「有通道」这个不变量；
    /// 这条钉住「按键确实穿过 conpty 到了对端」。做法是让子进程自己把读到的
    /// 那一行落盘（`set /p` 读一行 → 重定向到文件），再等文件出现——
    /// 不依赖渲染层的单元格内容，因此不受排版/光标动画影响。
    #[cfg(windows)]
    #[gpui::test]
    fn windows_local_terminal_delivers_typed_bytes_to_the_child(cx: &mut TestAppContext) {
        let out = std::env::temp_dir().join(format!(
            "deepterm-e2e-input-{}-{}.txt",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_file(&out);
        // `!VAR!` + `/v:on`（延迟展开）：`%VAR%` 会在整行解析期就展开，
        // 那时 `set /p` 还没跑，读到的永远是空串。
        let script = format!(
            "set /p DEEPTERM_PROBE=&& echo !DEEPTERM_PROBE!>\"{}\"",
            out.display()
        );

        let terminal = cx.update(|cx| {
            Terminal::new_with_command(
                cx,
                None,
                "cmd.exe",
                &["/v:on".to_string(), "/c".to_string(), script],
            )
        });
        terminal.update(cx, |t, _| t.write(b"deepterm-e2e-ok\r\n"));

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut written = None;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            if let Ok(text) = std::fs::read_to_string(&out) {
                if !text.trim().is_empty() {
                    written = Some(text);
                    break;
                }
            }
        }
        let _ = std::fs::remove_file(&out);
        assert_eq!(
            written.as_deref().map(str::trim),
            Some("deepterm-e2e-ok"),
            "写入终端的字节必须到达子进程（cmd.exe 未读到该行）"
        );
    }

    // ===== 本地路径 OSC 拦截测试 =====
    //
    // 与上面几条的分工：那些走 `handle_pty_data`（远程/UI 线程内的就地判定），
    // 这些走 `handle_local_osc`（读线程旁路通知后由 UI 线程判定，本地路径）。
    // 两条路径的处理函数相同，差异只在入口与 `require_local_osc7_host`。
    // 不摘除 `LocalNotice` 的构造：读线程通知这一跳正是被测对象。

    /// 本地路径的 OSC 133;B 必须被消费并设置输入区锚点。
    ///
    /// 这条锁住的是"读线程旁路通知 → LocalNotice::Osc → UI 线程判定"整条链：
    /// 修补前本地 shell 的提示符标记全部石沉大海（vte 没有 133 分支）。
    #[gpui::test]
    fn local_osc133_b_sets_input_area_anchor(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.advance_vte(b"$ ");
            // 读线程在序列位置取样的光标是 (0,2)；UI 线程必须采信它，
            // 而不是现读 `Term`（那时同 chunk 的后续字节已解析完）。
            t.handle_local_notice(
                LocalNotice::Osc {
                    content: b"133;B".to_vec(),
                    cursor: (0, 2),
                },
                cx,
            );
            let anchor = t.input_area_start.expect("本地 133;B 应被消费并捕获锚点");
            assert_eq!(anchor.column.0, 2, "锚点应取自通知里的序列时刻光标");
        });
    }

    /// 回归护栏：`133;B` 之后的输入回显**不得**污染锚点。
    ///
    /// 这是"本地路径空行判定"最容易错的形态：读线程把整个 chunk 解析完后才
    /// 发通知，若 UI 线程现读 `Term` 的光标，拿到的是行尾（回显之后）而不是
    /// 锚点，`input_line_blank` 就会把有内容的行误判为空行——`/` 前缀会被
    /// 错误拦截成内建命令。
    #[gpui::test]
    fn local_osc133_b_anchor_ignores_trailing_echo(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            // 模拟读线程：`$ ` + 锚点 + 用户输入 `abc` 全部已解析进 term。
            t.advance_vte(b"$ \x1b]133;B\x07abc");
            // 通知稍后才到，带的是序列时刻的光标 (0,2)。
            t.handle_local_notice(
                LocalNotice::Osc {
                    content: b"133;B".to_vec(),
                    cursor: (0, 2),
                },
                cx,
            );

            assert_eq!(
                t.input_area_start.map(|p| p.column.0),
                Some(2),
                "锚点必须是提示符末尾，不能被后续回显推到行尾"
            );
            assert!(!t.input_line_blank(), "行内已有 'abc'，不该被判定为空行");
        });
    }

    /// 本地路径的 OSC 777 通知应被消费。
    ///
    /// 断言方式说明：`Event::NotificationReceived` 的订阅需要 `Entity`
    /// 回调机制，在 `TestAppContext` 里写起来脆；这里退一步验证"消费判定
    /// 本身"——它决定应用层是否派发通知。通知事件的派发是既有代码，
    /// 远程路径的同类测试已覆盖。
    #[gpui::test]
    fn local_osc_notification_is_consumed(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            let consumed = t.try_handle_osc_sequence(cx, b"777;notify;Build;done", true, None);
            assert!(consumed, "本地 OSC 777 通知应由应用层消费");
        });
    }

    /// Kitty clipboard 的写入事务经 OSC 5522 提交后，系统剪贴板必须更新。
    #[gpui::test]
    fn kitty_clipboard_write_updates_system_clipboard(cx: &mut TestAppContext) {
        use crate::terminal::kitty::command::encode_base64;

        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            let mime = encode_base64(b"text/plain");
            let payload = encode_base64(b"hello kitty");
            let write = format!("5522;type=write:mime={mime};{payload}");
            assert!(t.try_handle_osc_sequence(cx, write.as_bytes(), true, None));
            assert!(t.try_handle_osc_sequence(cx, b"5522;type=wdata;", true, None));
        });
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("剪贴板应有文本");
        assert_eq!(text, "hello kitty");
    }

    /// 应用层不认识的 OSC（调色板、标题、超链接…）必须被判为"未消费"。
    ///
    /// 这条守住两条管线的分工：这类序列由 **vte** 处理，而扫描器已把它们按
    /// 原位置喂进 vte（见 `scanner::KittyGraphicsScanner::scan`）。若这里误判
    /// 成"消费"，应用层就会以为自己处理过了——虽然字节仍在流里，但语义上
    /// 把 vte 的职责划到了应用层，后续任何"消费了就不喂"的改动都会让超链接
    /// 与调色板静默失效。
    ///
    /// 用 OSC 4（调色板）做探针：vte 认它，应用层不认，两边界限清晰。
    #[gpui::test]
    fn unclaimed_local_osc_is_left_to_vte(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            assert!(
                !t.try_handle_osc_sequence(cx, b"4;1;#123456", true, None),
                "OSC 4（调色板）不应由应用层消费，它归 vte 处理"
            );
            // 同理：OSC 8（超链接）是 vte 的职责，应用层不得认领。
            assert!(
                !t.try_handle_osc_sequence(cx, b"8;;https://example.com", true, None),
                "OSC 8（超链接）不应由应用层消费，它归 vte 处理"
            );
        });
    }

    // ===== 阶段 3：agent 状态测试 =====
    //
    // 这些测试集中的不变式：
    // - agent 事件走 OSC 777 与桌面通知**共用**的信道，但绝不能被当成通知；
    // - 状态只由 hook 事件驱动，进程检测只管"是哪个 agent"；
    // - 只有真正变化才发 `AgentChanged`。

    /// 判定一条 OSC 应落到哪条路由。
    ///
    /// 把 `try_handle_osc_sequence` 顶部的两个分支抽成纯函数穷举测试，比订阅
    /// 式断言稳（订阅需要 `Entity` 回调机制，`TestAppContext` 里写起来脆）。
    /// 返回 `(是否会被 agent 路由拿走, 是否会产生桌面通知)`。
    ///
    /// 注意第二个分量不是"`parse_notification_osc` 能解析"——那只说明格式像通知。
    /// **真正会不会弹通知取决于顺序**：agent 判定在前，一旦被它拿走，
    /// `parse_notification_osc` 在 `try_handle_osc_sequence` 里根本不会被调用。
    /// 所以这里显式表达顺序（与实现同构），否则测试无法暴露排序写反。
    #[cfg(test)]
    fn route_osc(content: &[u8]) -> (bool, bool) {
        if crate::agent::parse_agent_event(content).is_some() {
            return (true, false);
        }
        (false, Terminal::parse_notification_osc(content).is_some())
    }

    /// agent sentinel 与三种真实通知/无关序列的路由互不串台。
    ///
    /// 关键是第一行：sentinel 必须**只**命中 agent，绝不能同时被判成通知
    /// ——否则每报一次 hook 事件用户就多收一条垃圾桌面通知。
    #[test]
    fn agent_sentinel_does_not_route_to_notification() {
        assert_eq!(
            route_osc(b"777;notify;deepterm://cli-agent;{\"event\":\"stop\"}"),
            (true, false),
            "sentinel 负载应只走 agent 路由"
        );
        assert_eq!(
            route_osc(b"777;notify;Build;done"),
            (false, true),
            "普通 777 通知不受影响"
        );
        assert_eq!(route_osc(b"9;hello"), (false, true), "OSC 9 通知不受影响");
        assert_eq!(
            route_osc(b"4;1;#123456"),
            (false, false),
            "调色板既非 agent 也非通知，应透传"
        );
    }

    /// 一条 title 恰为字面量 `deepterm://cli-agent` 的**真实**通知必须送达。
    ///
    /// 这条钉住"为什么不需要 tty7 那句显式 sentinel 检查"：tty7 是回查解析出的
    /// title 来排除 sentinel，因此会把这种命名撞车的真实通知静默吞掉。deepterm 的
    /// `parse_agent_event` 要求完整前缀 `777;notify;deepterm://cli-agent;`（带分号
    /// 与 JSON），title 只有 sentinel 且 body 不是 JSON 时它返回 `None`，于是正确
    /// 落入通知路由。若有人为了"对齐 tty7"补回检查，这条会失败。
    #[test]
    fn user_notification_titled_like_sentinel_still_delivered() {
        assert_eq!(
            route_osc(b"777;notify;deepterm://cli-agent;hello world"),
            (false, true),
            "title 撞车的真实通知不应被 agent 路由吞掉"
        );
    }

    /// 远程路径：sentinel 经 `try_handle_osc_sequence` 后建立 session，
    /// 且**不**产生 `NotificationReceived`。
    #[gpui::test]
    fn remote_agent_sentinel_builds_session_without_notification(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        let events = Arc::new(Mutex::new(Vec::<crate::terminal::Event>::new()));
        let sink = events.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, ev: &crate::terminal::Event, _| {
                sink.lock().unwrap().push(ev.clone());
            })
        });

        let payload =
            br#"777;notify;deepterm://cli-agent;{"v":1,"agent":"claude","event":"prompt-submit","session_id":"s-1"}"#;
        terminal.update(cx, |t, cx| {
            assert!(
                t.try_handle_osc_sequence(cx, payload, false, None),
                "远程 sentinel 应被消费"
            );
            let session = t.agent_session().expect("hook 事件应建立 session");
            assert_eq!(session.status, crate::agent::AgentStatus::Working);
            assert_eq!(session.session_id.as_deref(), Some("s-1"));
            assert_eq!(t.foreground_agent(), Some(crate::agent::CLIAgent::Claude));
        });

        // 关键：绝不能弹出一条虚假桌面通知（订阅回调在 update 返回后才跑）。
        let seen = events.lock().unwrap();
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, crate::terminal::Event::NotificationReceived { .. })),
            "sentinel 不得被当成桌面通知，实际事件 {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|e| *e == crate::terminal::Event::AgentChanged),
            "应发出 AgentChanged，实际事件 {seen:?}"
        );
    }

    /// 本地路径：同一负载经 `handle_local_notice(LocalNotice::Osc(..))`。
    ///
    /// 这条专门锁住 `handle_local_notice` 里 agent 判定**先于** `handle_local_osc`
    /// 的排序——写反的话事件会落进 `parse_notification_osc`，session 建不起来。
    #[gpui::test]
    fn local_agent_sentinel_builds_session_without_notification(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        let events = Arc::new(Mutex::new(Vec::<crate::terminal::Event>::new()));
        let sink = events.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, ev: &crate::terminal::Event, _| {
                sink.lock().unwrap().push(ev.clone());
            })
        });

        let payload =
            br#"777;notify;deepterm://cli-agent;{"v":1,"agent":"claude","event":"prompt-submit","session_id":"s-2"}"#;
        terminal.update(cx, |t, cx| {
            t.handle_local_notice(
                LocalNotice::Osc {
                    content: payload.to_vec(),
                    cursor: (0, 0),
                },
                cx,
            );
            let session = t
                .agent_session()
                .expect("本地 sentinel 应建立 session（排序写反就会失败）");
            assert_eq!(session.status, crate::agent::AgentStatus::Working);
            assert_eq!(t.foreground_agent(), Some(crate::agent::CLIAgent::Claude));
        });

        let seen = events.lock().unwrap();
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, crate::terminal::Event::NotificationReceived { .. })),
            "本地 sentinel 不得被当成桌面通知，实际事件 {seen:?}"
        );
    }

    /// 本地路径的普通通知仍然照常派发，别被 agent 判定顺手吃掉。
    ///
    /// 这条是上一条的对照：agent 判定只该拿走 sentinel，其余一切照旧。
    #[gpui::test]
    fn local_plain_notification_still_delivered(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        let events = Arc::new(Mutex::new(Vec::<crate::terminal::Event>::new()));
        let sink = events.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, ev: &crate::terminal::Event, _| {
                sink.lock().unwrap().push(ev.clone());
            })
        });

        terminal.update(cx, |t, cx| {
            t.handle_local_notice(
                LocalNotice::Osc {
                    content: b"777;notify;Build;done".to_vec(),
                    cursor: (0, 0),
                },
                cx,
            );
            assert!(
                t.agent_session().is_none(),
                "普通通知不应建出 agent session"
            );
            assert!(t.foreground_agent().is_none());
        });

        let seen = events.lock().unwrap();
        assert!(
            seen.iter().any(|e| matches!(
                e,
                crate::terminal::Event::NotificationReceived { title, body }
                    if title == "Build" && body == "done"
            )),
            "普通 777 通知必须照常派发，实际事件 {seen:?}"
        );
    }

    /// 远程路径的普通通知也应照常派发（对照上一条的远程版本）。
    #[gpui::test]
    fn remote_plain_notification_still_delivered(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        let events = Arc::new(Mutex::new(Vec::<crate::terminal::Event>::new()));
        let sink = events.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, ev: &crate::terminal::Event, _| {
                sink.lock().unwrap().push(ev.clone());
            })
        });

        terminal.update(cx, |t, cx| {
            assert!(t.try_handle_osc_sequence(cx, b"777;notify;Build;done", false, None));
            assert!(t.agent_session().is_none());
        });

        let seen = events.lock().unwrap();
        assert!(
            seen.iter().any(|e| matches!(
                e,
                crate::terminal::Event::NotificationReceived { title, body }
                    if title == "Build" && body == "done"
            )),
            "远程普通 777 通知必须照常派发，实际事件 {seen:?}"
        );
    }

    /// hook 声明的 agent 在进程检测失败时兜底，但**不覆盖**已检测出的身份。
    ///
    /// 检测结果来自真实进程 argv，比事件里的自称可靠：包装器脚本可能报错、也可能
    /// 一个 pane 里套了两层 agent，都不该让身份来回翻。
    #[gpui::test]
    fn hook_agent_fills_in_only_when_detection_failed(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            let ev = |agent: Option<crate::agent::CLIAgent>, kind| crate::agent::AgentEvent {
                agent,
                kind,
                session_id: None,
                message: None,
                cwd: None,
                prompt: None,
            };

            assert!(t.foreground_agent().is_none());
            t.apply_agent_events(
                &[ev(
                    Some(crate::agent::CLIAgent::Claude),
                    crate::agent::AgentEventKind::SessionStart,
                )],
                cx,
            );
            assert_eq!(
                t.foreground_agent(),
                Some(crate::agent::CLIAgent::Claude),
                "检测失败时应采信 hook 声明的 agent"
            );

            // 再来一个自称别的 agent 的事件：身份必须保持不变。
            t.apply_agent_events(
                &[ev(
                    Some(crate::agent::CLIAgent::Aider),
                    crate::agent::AgentEventKind::Notification,
                )],
                cx,
            );
            assert_eq!(
                t.foreground_agent(),
                Some(crate::agent::CLIAgent::Claude),
                "已确定的身份不应被后续事件覆盖"
            );
        });
    }

    /// 门控：没有任何 agent 声明的事件不得凭空建出孤儿 session。
    ///
    /// 场景是"杂散 sentinel 输出"——比如用户 `cat` 了一个含 hook 日志的文件。
    /// 建出 session 会让界面显示状态点却没有品牌图标。
    #[gpui::test]
    fn agentless_event_does_not_create_orphan_session(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.apply_agent_events(
                &[crate::agent::AgentEvent {
                    agent: None,
                    kind: crate::agent::AgentEventKind::Stop,
                    session_id: None,
                    message: None,
                    cwd: None,
                    prompt: None,
                }],
                cx,
            );
            assert!(t.agent_session().is_none(), "无 agent 不应建 session");
            assert!(t.foreground_agent().is_none());
        });
    }

    /// `AgentChanged` 只在真正变化时发一次。
    ///
    /// 检测按 TTL 周期跑、hook 事件也可能重复投递，若每次都 emit 会让订阅方
    /// 无谓重绘。
    ///
    /// 注意 gpui 的派发时机：`cx.emit` 的订阅回调在**发射方 `update` 闭包返回之后**
    /// 才跑。所以断言必须放在闭包外——在闭包内断言 `emissions` 恒为 0（这不是本实现
    /// 的缺陷，是 `Context` 借用期的语义）。曾经在闭包内断言，白白排查了很久。
    #[gpui::test]
    fn agent_changed_emitted_only_on_real_change(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        let emissions = Arc::new(Mutex::new(Vec::<crate::terminal::Event>::new()));
        let sink = emissions.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, ev: &crate::terminal::Event, _| {
                sink.lock().unwrap().push(ev.clone());
            })
        });
        assert_eq!(emissions.lock().unwrap().len(), 0, "订阅应已挂上（无事件）");

        let session_start = || crate::agent::AgentEvent {
            agent: Some(crate::agent::CLIAgent::Claude),
            kind: crate::agent::AgentEventKind::SessionStart,
            session_id: None,
            message: None,
            cwd: None,
            prompt: None,
        };

        // 第一次：建立 session，必须发一次。
        terminal.update(cx, |t, cx| t.apply_agent_events(&[session_start()], cx));
        assert_eq!(
            emissions.lock().unwrap().len(),
            1,
            "第一次事件就该发 AgentChanged"
        );

        // 第二次：同一事件，session 前后全等，不该再发。
        terminal.update(cx, |t, cx| t.apply_agent_events(&[session_start()], cx));
        assert_eq!(
            emissions.lock().unwrap().len(),
            1,
            "重复的同一事件只应发一次"
        );

        // 第三次：状态从 Idle 变 Done，要发。
        terminal.update(cx, |t, cx| {
            t.apply_agent_events(
                &[crate::agent::AgentEvent {
                    agent: Some(crate::agent::CLIAgent::Claude),
                    kind: crate::agent::AgentEventKind::Stop,
                    session_id: None,
                    message: None,
                    cwd: None,
                    prompt: None,
                }],
                cx,
            )
        });
        assert_eq!(emissions.lock().unwrap().len(), 2, "状态真的变了要再发一次");
    }

    /// **仅身份变化**（session 全等）也必须发 `AgentChanged`。
    ///
    /// 这是最容易踩的坑：tty7 的片段末尾只判 `agent_session != before`。
    /// 这里构造 session_id 已设、status 已是 Idle 的情形——此时再来一条
    /// `SessionStart`（同样把 status 设成 Idle、带同一个 session_id）会让 session
    /// 前后逐字段相等，但它顺带把 `foreground_agent` 从 `None` 兜底成 `Some(Claude)`。
    /// 界面必须收到事件才显示得出图标，只判 session 的实现会静默丢掉。
    #[gpui::test]
    fn agent_changed_emitted_for_identity_only_change(cx: &mut TestAppContext) {
        use crate::agent::{AgentEvent, AgentEventKind, AgentSessionState, AgentStatus, CLIAgent};

        let terminal = cx.new(|_| anchor_test_terminal());
        let emissions = Arc::new(Mutex::new(0usize));
        let sink = emissions.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, ev: &crate::terminal::Event, _| {
                if *ev == crate::terminal::Event::AgentChanged {
                    *sink.lock().unwrap() += 1;
                }
            })
        });

        // 起点：身份已确定（模拟进程检测结果），session 由一条事件建立。
        terminal.update(cx, |t, cx| {
            t.foreground_agent = Some(CLIAgent::Claude);
            t.apply_agent_events(
                &[AgentEvent {
                    agent: None,
                    kind: AgentEventKind::SessionStart,
                    session_id: Some("fixed".to_string()),
                    message: None,
                    cwd: None,
                    prompt: None,
                }],
                cx,
            )
        });

        let expected = AgentSessionState {
            status: AgentStatus::Idle,
            session_id: Some("fixed".to_string()),
            ..Default::default()
        };
        terminal.update(cx, |t, _cx| {
            assert_eq!(
                t.agent_session(),
                Some(&expected),
                "起点 session 应为 Idle + 固定 id"
            );
        });
        let before = *emissions.lock().unwrap();

        // 关键一幕：身份被清掉（模拟检测短暂失败），再收一条事件把它兜底回
        // `Some(Claude)`。session 前后逐字段相等，只有身份变了。
        terminal.update(cx, |t, cx| {
            t.foreground_agent = None;
            t.apply_agent_events(
                &[AgentEvent {
                    agent: Some(CLIAgent::Claude),
                    kind: AgentEventKind::SessionStart,
                    session_id: Some("fixed".to_string()),
                    message: None,
                    cwd: None,
                    prompt: None,
                }],
                cx,
            )
        });

        terminal.update(cx, |t, _cx| {
            assert_eq!(
                t.agent_session(),
                Some(&expected),
                "前提：session 在这次事件前后必须逐字段相等（否则测的不是纯身份变更）"
            );
            assert_eq!(t.foreground_agent(), Some(CLIAgent::Claude));
        });
        assert_eq!(
            *emissions.lock().unwrap(),
            before + 1,
            "仅身份变化（session 全等）也必须发 AgentChanged"
        );
    }

    /// 一个回合里的几十次 `ToolComplete` **不得**各发一次 `AgentChanged`。
    ///
    /// 这是上面那条「只在真正变化时发」最容易失守的形态：判断用的是整份
    /// session 相等，只要 `ToolComplete` 里塞了任何自增字段，状态明明停在
    /// `Working`，每次工具调用却都判为"变了"。订阅方（`TerminalView` → panel →
    /// workspace → tab 栏）于是被每 100ms 级的工具调用拖着反复重绘、反复同步。
    ///
    /// 曾经的 `AgentSessionState::activity` 正是这样一个字段，且没有任何渲染
    /// 消费方。这条测试锁住「状态不变 ⇒ 不发事件」。
    #[gpui::test]
    fn tool_completions_do_not_storm_agent_changed(cx: &mut TestAppContext) {
        use crate::agent::{AgentEvent, AgentEventKind, CLIAgent};

        let terminal = cx.new(|_| anchor_test_terminal());
        let emissions = Arc::new(Mutex::new(0usize));
        let sink = emissions.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, ev: &crate::terminal::Event, _| {
                if *ev == crate::terminal::Event::AgentChanged {
                    *sink.lock().unwrap() += 1;
                }
            })
        });

        let ev = |kind| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: None,
            cwd: None,
            prompt: None,
        };

        // 起手：一个回合开始（建立 session + 进 Working）。
        terminal.update(cx, |t, cx| {
            t.apply_agent_events(&[ev(AgentEventKind::PromptSubmit)], cx)
        });
        let after_prompt = *emissions.lock().unwrap();
        assert_eq!(after_prompt, 1, "建立 session 并进 Working 应发一次");

        // 二十次工具完成：状态一直是 Working，不该再发。
        for _ in 0..20 {
            terminal.update(cx, |t, cx| {
                t.apply_agent_events(&[ev(AgentEventKind::ToolComplete)], cx)
            });
        }
        assert_eq!(
            *emissions.lock().unwrap(),
            after_prompt,
            "Working 状态下的 ToolComplete 不该再发 AgentChanged（状态没变）"
        );

        // 收尾仍然要发——别把真实变化一起静默掉。
        terminal.update(cx, |t, cx| {
            t.apply_agent_events(&[ev(AgentEventKind::Stop)], cx)
        });
        assert_eq!(
            *emissions.lock().unwrap(),
            after_prompt + 1,
            "Working → Done 是真实变化，必须发"
        );
    }

    /// agent 退出、shell 回到前台时清空身份与 session。
    ///
    /// 远程的清除机制靠"检测对 shell 名返回 `None`"，所以这里直接喂壳集成在
    /// 回到提示符时上报的值（`OSC 1337;Proc=zsh`，见
    /// `assets/shell-integration/zsh/deepterm-integration:126`）。
    #[gpui::test]
    fn returning_to_shell_clears_agent_and_session(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            // 先进 agent。
            t.handle_pty_data(b"\x1b]1337;Proc=claude\x1b\\", false, cx);
            assert_eq!(t.foreground_agent(), Some(crate::agent::CLIAgent::Claude));
            t.apply_agent_events(
                &[crate::agent::AgentEvent {
                    agent: None,
                    kind: crate::agent::AgentEventKind::PromptSubmit,
                    session_id: None,
                    message: None,
                    cwd: None,
                    prompt: None,
                }],
                cx,
            );
            assert!(t.agent_session().is_some());

            // 回提示符：壳集成重新上报 shell 自己的 basename。
            t.handle_pty_data(b"\x1b]1337;Proc=zsh\x1b\\", false, cx);
            assert!(
                t.foreground_agent().is_none(),
                "回到 shell 后身份应清空，实际 {:?}",
                t.foreground_agent()
            );
            assert!(t.agent_session().is_none(), "身份清空必须一并清掉 session");
        });
    }

    /// 钉住上一条测试的前提：**没有任何 agent alias 与 shell 名冲突**。
    ///
    /// 这条前提是"远程靠检测返回 `None` 来清空"能成立的**唯一**依据。若将来
    /// 给某个 agent 加了 `sh`/`bash` 之类的 alias，`OSC 1337;Proc=zsh` 会被误判
    /// 成 agent，回到提示符也清不掉——那时这条测试会先失败，比线上怪异行为好。
    #[test]
    fn no_agent_alias_collides_with_shell_names() {
        use std::collections::HashMap;
        for shell in ["zsh", "bash", "fish", "sh", "dash", "tcsh", "ksh"] {
            let argv = vec![shell.to_string()];
            assert_eq!(
                crate::agent::CLIAgent::detect_from_argv_with(&argv, &HashMap::new()),
                None,
                "shell 名 {shell:?} 不应被识别成任何 agent"
            );
        }
    }

    /// `set_foreground_agent` 值不变时不发事件；清空时顺带清 session。
    #[gpui::test]
    fn set_foreground_agent_is_idempotent(cx: &mut TestAppContext) {
        use crate::agent::{AgentEvent, AgentEventKind, CLIAgent};

        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.set_foreground_agent(Some(CLIAgent::Claude), cx);
            assert_eq!(t.foreground_agent(), Some(CLIAgent::Claude));
            t.apply_agent_events(
                &[AgentEvent {
                    agent: None,
                    kind: AgentEventKind::PromptSubmit,
                    session_id: None,
                    message: None,
                    cwd: None,
                    prompt: None,
                }],
                cx,
            );
            assert!(t.agent_session().is_some());

            // 同一个值：不得清 session（否则周期检测会把状态洗掉）。
            t.set_foreground_agent(Some(CLIAgent::Claude), cx);
            assert!(t.agent_session().is_some(), "值未变化时不应动 session");

            t.set_foreground_agent(None, cx);
            assert!(t.foreground_agent().is_none());
            assert!(t.agent_session().is_none());
        });
    }

    /// 本地 OSC 7 的 host 必须是本机才采信（`require_local_osc7_host = true`）。
    ///
    /// 远程路径传 `false`（host 属于远端，拿本机 hostname 比对必然失败）。
    /// 若本地误传 `false`，伪造的 host 会被当成合法 cwd 写进来。
    #[gpui::test]
    fn local_osc7_rejects_foreign_host(cx: &mut TestAppContext) {
        let terminal = cx.new(|_| anchor_test_terminal());
        terminal.update(cx, |t, cx| {
            t.handle_local_notice(
                LocalNotice::Osc {
                    content: b"7;file://definitely-not-this-machine.local/tmp".to_vec(),
                    cursor: (0, 0),
                },
                cx,
            );
            assert!(
                t.reported_working_directory.is_none(),
                "非本机 host 的本地 OSC 7 不应被采信，实际 {:?}",
                t.reported_working_directory
            );
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

    /// 构造一个 `cols×rows` 的终端，把 `text` 当作 PTY 输出喂进去并刷新快照，
    /// 返回可以直接做命中测试的实体。
    ///
    /// 走真实的 `vte` 解析 + `update_last_content` 快照，因此宽字符占位格、
    /// `WRAPLINE`、行尾空格这些「网格事实」都在里面 —— 单元测试里手工拼
    /// `IndexedCell` 是拼不出这些的。
    fn link_probe_terminal(
        cx: &mut TestAppContext,
        cols: usize,
        rows: usize,
        text: &str,
    ) -> Entity<Terminal> {
        let bounds = bounds_for(cols, rows);
        let text = text.to_string();
        let terminal = cx.new(|_| Terminal::create_error_terminal());
        terminal.update(cx, |t, cx| {
            t.resize(bounds, 1.0, cx);
            t.process_pty_data(text.as_bytes(), false, cx);
            t.update_last_content(true);
            assert_eq!(grid_size(t), (cols, rows), "网格尺寸应等于测试请求的尺寸");
        });
        terminal
    }

    /// 命中测试的简写：`(url, start_col, end_col)`。
    fn url_at(t: &Terminal, row: usize, col: usize) -> Option<(String, usize, usize)> {
        t.link_hits_at_viewport_position(row, col)
            .url
            .map(|(u, s, e)| (u.to_string(), s, e))
    }

    /// 整行逐格命中测试：哪些列上有链接。
    fn url_cols(t: &Terminal, row: usize, cols: usize) -> Vec<usize> {
        (0..cols).filter(|c| url_at(t, row, *c).is_some()).collect()
    }

    #[gpui::test]
    fn hit_url_after_wide_chars(cx: &mut TestAppContext) {
        // 「见」占两格：URL 起点必须落在第 3 列，而不是被占位格顶偏一列。
        let terminal = link_probe_terminal(cx, 40, 3, "见 https://example.com/a");
        terminal.update(cx, |t, _| {
            assert_eq!(grid_size(t), (40, 3));
            assert_eq!(
                url_at(t, 0, 3),
                Some(("https://example.com/a".into(), 3, 24)),
                "URL 命中区必须是 3..24（21 个字符）"
            );
            // 「见」的右半格（第 1 列）与它前面的空格都不是链接
            assert!(url_at(t, 0, 0).is_none());
            assert!(url_at(t, 0, 1).is_none());
            assert!(url_at(t, 0, 2).is_none());
            // 末列之后是行尾填充空格
            assert!(url_at(t, 0, 24).is_none());
            assert_eq!(url_cols(t, 0, 40), (3..24).collect::<Vec<_>>());
        });
    }

    #[gpui::test]
    fn hit_url_split_by_soft_wrap(cx: &mut TestAppContext) {
        // 20 列窄终端：链接被软换行折成两半，两半都必须指向**同一个**完整
        // 地址。旧实现两半都不认（前半截 `https://example.` 被截断、后半截
        // `com/long/path` 没有锚点）。
        let terminal = link_probe_terminal(cx, 20, 4, "see https://example.com/long/path");
        terminal.update(cx, |t, _| {
            let url = "https://example.com/long/path".to_string();
            // 第 0 行：`see ` 之后是 URL 的前 16 个字符，占满 4..20
            assert_eq!(url_at(t, 0, 4), Some((url.clone(), 4, 20)));
            assert_eq!(url_cols(t, 0, 20), (4..20).collect::<Vec<_>>());
            // 第 1 行：URL 剩下的 `com/long/path`（13 列）
            assert_eq!(url_at(t, 1, 0), Some((url.clone(), 0, 13)));
            assert_eq!(url_at(t, 1, 12), Some((url, 0, 13)));
            assert_eq!(url_cols(t, 1, 20), (0..13).collect::<Vec<_>>());
            // 「see 」不带链接
            assert!(url_at(t, 0, 0).is_none());
            assert!(url_at(t, 0, 3).is_none());
        });
    }

    #[gpui::test]
    fn hit_wrapped_osc8_link(cx: &mut TestAppContext) {
        // OSC8 显式链接被折行后，后半截仍必须解析到**同一个** uri（而不是
        // 掉到正则分支被当成裸域名）。
        let uri = "https://example.com/very/long/target";
        let text = format!("\x1b]8;id=link1;{uri}\x07{uri}\x1b]8;;\x07");
        let terminal = link_probe_terminal(cx, 20, 4, &text);
        terminal.update(cx, |t, _| {
            assert_eq!(
                t.link_hits_at_viewport_position(0, 0).hyperlink,
                Some((uri.to_string(), "link1".into(), 0, 20))
            );
            assert_eq!(
                t.link_hits_at_viewport_position(1, 15).hyperlink,
                Some((uri.to_string(), "link1".into(), 0, 16)),
                "续行仍应命中同一个 OSC8 链接"
            );
            // 第 1 行第 16 列之后是空白填充，不该命中
            assert!(t.link_hits_at_viewport_position(1, 16).hyperlink.is_none());
            // 正则分支此时也能认出同一条完整 URL（优先 OSC8 还是 URL 是
            // `terminal_view::hit_test_link` 的事，两条路径都得给全地址）。
            assert_eq!(url_at(t, 1, 0), Some((uri.to_string(), 0, 16)));
        });
    }

    #[gpui::test]
    fn hit_url_not_truncated_at_viewport_top(cx: &mut TestAppContext) {
        // 19 列：`see https://a.exam` 折行后滚出视口，视口第 0 行只剩
        // `ple.com/long/path`。旧实现只看视口内的行，会把后半截（前半截在
        // scrollback 里）当成完整的裸域名；新实现从 grid 补回前缀行。
        let terminal = link_probe_terminal(
            cx,
            19,
            3,
            "see https://a.example.com/long/path\r\n1\r\n2\r\n3\r\n",
        );
        terminal.update(cx, |t, cx| {
            // 滚到 offset=2：视口第 0 行 = 内容第 1 行（URL 的后半截）。
            t.set_display_offset(2, cx);
            t.update_last_content(true);
            assert_eq!(
                t.link_hits_at_viewport_position(0, 0).url,
                Some(("https://a.example.com/long/path".into(), 0, 16)),
                "scrollback 里的前半截必须接回来"
            );
            // 视口第 1 行是内容行 `1`，不带链接。
            assert!(t.link_hits_at_viewport_position(1, 0).url.is_none());
        });
    }

    #[gpui::test]
    fn hit_path_with_wide_chars_is_one_token(cx: &mut TestAppContext) {
        // 旧实现把 `~/项目/src/main.rs` 按宽字符占位格切成 `~/项`、`目`、
        // `/src/main.rs` 三个 token（真实路径永远命不中），尾部还会退化成
        // 绝对路径 `/src/main.rs`。
        let terminal = link_probe_terminal(cx, 40, 3, "~/项目/src/main.rs");
        terminal.update(cx, |t, _| {
            assert_eq!(
                t.link_hits_at_viewport_position(0, 0).file.unwrap().path,
                "~/项目/src/main.rs"
            );
            let hits: Vec<_> = (0..40)
                .filter_map(|c| {
                    t.link_hits_at_viewport_position(0, c)
                        .file
                        .map(|h| (c, h.path.clone()))
                })
                .collect();
            assert_eq!(
                hits,
                (0..18)
                    .map(|c| (c, "~/项目/src/main.rs".to_string()))
                    .collect::<Vec<_>>(),
                "整条路径应当是一个 token，且每列（含宽字符右半格）都命中它"
            );
        });
    }

    #[gpui::test]
    fn hit_path_range_excludes_wrapping_punct(cx: &mut TestAppContext) {
        let terminal = link_probe_terminal(cx, 40, 3, "(src/main.rs)");
        terminal.update(cx, |t, _| {
            let hit = t
                .link_hits_at_viewport_position(0, 1)
                .file
                .expect("路径应可命中");
            assert_eq!(hit.path, "src/main.rs");
            // `(` 在第 0 列，路径占 1..=11，`)` 在第 12 列
            assert_eq!((hit.start_col, hit.end_col), (1, 12));
            assert!(
                t.link_hits_at_viewport_position(0, 0).file.is_none(),
                "`(` 不该可点"
            );
            assert!(
                t.link_hits_at_viewport_position(0, 12).file.is_none(),
                "`)` 不该可点"
            );
        });
    }

    #[gpui::test]
    fn hit_prose_words_are_not_paths(cx: &mut TestAppContext) {
        // 正文高频词 + 版本号 + 时间戳不再变成链接
        let terminal = link_probe_terminal(
            cx,
            60,
            3,
            "error in src target index main v1.2.3 12:30:45\r\nmain.rs",
        );
        terminal.update(cx, |t, _| {
            for col in 0..60 {
                let hit = t.link_hits_at_viewport_position(0, col).file;
                assert!(
                    hit.is_none(),
                    "第 {col} 列不该是可点击路径：{:?}",
                    hit.map(|h| h.raw.clone())
                );
            }
            // 下一行里真正的文件名仍然可点
            assert_eq!(
                t.link_hits_at_viewport_position(1, 0)
                    .file
                    .map(|h| h.path.clone()),
                Some("main.rs".to_string())
            );
        });
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

    /// 远程式终端 + Kitty 扫描器，供 resize 相关的协议用例使用。
    ///
    /// 与 [`remote_size_test_terminal`] 的差别：带上扫描器（`apply_pending_resize`
    /// 会把几何推给它），并置 `resize_primed` 跳过首帧校正分支，让用例里的
    /// resize 直接走「应用」路径而不是「首次校正」路径。
    fn remote_kitty_terminal(cx: &mut TestAppContext) -> (Entity<Terminal>, Arc<Mutex<Vec<u8>>>) {
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = written.clone();
        let terminal = cx.new(move |_| {
            let mut t = Terminal::create_error_terminal();
            t.terminal_bounds = bounds_for(80, 24);
            t.remote_input_callback = Some(Box::new(move |bytes| {
                sink.lock().unwrap().extend_from_slice(bytes);
            }));
            t.kitty_scanner = Some(KittyGraphicsScanner::with_storage(
                t.images.storage(),
                true,
                Box::new(crate::terminal::kitty::scanner::CollectingSink::default()),
            ));
            t.resize_primed = true;
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

    /// 远程终端在 resize 后必须把几何推给扫描器。
    ///
    /// 本地路径由读线程维护扫描器几何，远程路径没有读线程——只有在
    /// `apply_pending_resize` 里补上，否则删除类命令（`d=c`）拿不到单元格
    /// 尺寸，未显式给出 `c=`/`r=` 的放置会被当成 1×1 格，跨格的图像删不掉。
    ///
    /// 观察点是共享存储里的放置数量：先用 `a=T` 把 30x60 像素的图放在光标处
    /// （`bounds_for` 的格子在 1x 下是 9x16 像素 → 占 4 列 4 行），再用 `d=c`
    /// 打它内部的**非锚点**格。几何若没送达，矩形退化成 1x1，这次删除就匹配
    /// 不到任何东西。
    ///
    /// 不用尺寸上报做观察点：远程路径的 `CSI 16 t` 与 `?2048` 都由
    /// `handle_probe_sequences` 在 `terminal.rs` 里直接应答，且序列在进扫描器前
    /// 就被摘除，看不到扫描器自己的状态。
    #[gpui::test]
    fn remote_resize_pushes_geometry_to_scanner(cx: &mut TestAppContext) {
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let terminal = cx.new(|_| {
            let mut t = Terminal::create_error_terminal();
            // 初始尺寸必须与下面 resize 的目标**不同**：行列与缩放都没变时
            // `resize` 会早退（对应 ghostty 的同名早退），几何就不会被推送，
            // 测的就不是本用例要覆盖的那条路径了。
            t.terminal_bounds = bounds_for(80, 24);
            t.remote_input_callback = Some(Box::new(|_bytes| {}));
            t.kitty_scanner = Some(KittyGraphicsScanner::with_storage(
                t.images.storage(),
                true,
                Box::new(crate::terminal::kitty::scanner::CollectingSink::default()),
            ));
            // 跳过首帧校正分支，让下面的 resize 直接走应用路径。
            t.resize_primed = true;
            t
        });

        // resize 走 apply_pending_resize，应把几何（1x，cell 9x16）同步给扫描器。
        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(60, 20), 1.0, cx);
        });

        let storage = terminal.read_with(cx, |t, _| t.images_handle());
        type Store = std::sync::Arc<std::sync::Mutex<crate::terminal::kitty::storage::Storage>>;
        // 观测共享存储里 id=1 的放置数：几何送达与否直接决定这次删除命不命中。
        fn placements(store: &Store) -> usize {
            store
                .lock()
                .unwrap()
                .get(1)
                .map_or(0, |s| s.placements.len())
        }

        // 30x60 像素 → 除以 9x16 向上取整 = 4 列 4 行。
        let mut raw = Vec::with_capacity(30 * 60 * 4);
        for _ in 0..30 * 60 {
            raw.extend_from_slice(&[255, 0, 0, 255]);
        }
        let b64 = crate::terminal::kitty::command::encode_base64(&raw);

        // `a=T` 才是"传输并显示"；`a=t` 只存不显示，不会有放置。
        let seq = format!("\x1b[3;3H\x1b_Ga=T,i=1,f=32,s=30,v=60;{b64}\x1b\\");
        terminal.update(cx, |t, cx| {
            t.process_pty_data(seq.as_bytes(), false, cx);
        });

        assert_eq!(placements(&storage), 1, "a=T 应在光标 (2, 2) 处建立放置");

        // 锚点在 (2, 2)，矩形覆盖 2..=5 行 x 2..=5 列；打内部的 (4, 4)。
        // CUP 与 APC 分两次投递：APC 的锚点取自 vte 当前光标，同一批字节里
        // 光标尚未推进，会把删除算到旧位置上。
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[5;5H", false, cx);
            t.process_pty_data(b"\x1b_Ga=d,d=c\x1b\\", false, cx);
        });

        assert_eq!(
            placements(&storage),
            0,
            "d=c 打在矩形内部格应删除放置；未删说明几何没送达、矩形退化成 1x1"
        );
    }

    /// resize 时若 in-band size reports（DECSET ?2048）已启用，必须**重报一次**
    /// 新尺寸，且这次重报要真的写回 SSH 通道。
    ///
    /// 对齐 ghostty `stream_terminal.zig` 的 `resize()`：模式启用期间每次尺寸
    /// 变化都要主动告知客户端，否则它只能靠 `CSI 14 t` 轮询。
    ///
    /// 远程路径的 2048 开关由 `Terminal` 自己在 `handle_probe_sequences` 里观察，
    /// 序列随即被摘除——进不到扫描器，所以扫描器的 `set_geometry` 重报不会发生，
    /// 必须由 `apply_pending_resize` 用 `remote_mode_2048` 自己补报一次。
    #[gpui::test]
    fn remote_resize_re_reports_2048_to_channel(cx: &mut TestAppContext) {
        let (terminal, written) = remote_kitty_terminal(cx);

        // 开启 2048：此时会立即上报一次当前几何，先清掉，只观察 resize 那一次。
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?2048h", false, cx);
        });
        assert!(
            !written.lock().unwrap().is_empty(),
            "置位 2048 时应立即上报一次当前几何"
        );
        written.lock().unwrap().clear();

        // 行列都变 → 走 apply_pending_resize 的应用路径。
        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(60, 20), 1.0, cx);
        });

        let out = written.lock().unwrap().clone();
        assert!(
            !out.is_empty(),
            "resize 后必须立刻重报 2048 尺寸，实际没有写出任何字节"
        );
        let text = String::from_utf8_lossy(&out);
        // 格式 `\x1b[48;{rows};{cols};{height_px};{width_px}t`；
        // 60x20 网格、9x16 的 cell → 540 高 320 宽。
        assert!(
            text.contains("\x1b[48;20;60;320;540t"),
            "应报新网格 20 行 60 列与 320x540 像素，实际 {text:?}"
        );
    }

    /// `?2048l` 复位后 resize 不得再上报（对齐 ghostty：只有置位才发）。
    #[gpui::test]
    fn remote_resize_does_not_report_2048_after_reset(cx: &mut TestAppContext) {
        let (terminal, written) = remote_kitty_terminal(cx);

        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?2048h", false, cx);
            t.process_pty_data(b"\x1b[?2048l", false, cx);
        });
        written.lock().unwrap().clear();

        terminal.update(cx, |t, cx| {
            t.resize(bounds_for(60, 20), 1.0, cx);
        });

        assert!(
            written.lock().unwrap().is_empty(),
            "2048 已复位，resize 不应再上报，实际 {:?}",
            String::from_utf8_lossy(&written.lock().unwrap())
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

    /// 鼠标移动去重的完整语义（含 1016 开关后的行为）。
    ///
    /// 去重缓存（`last_reported_mouse_cell`）只按**网格坐标**拦阻，且
    /// **不随 1016 开关重置**——这是对齐 ghostty 的有意行为，不是缺陷：
    /// `input/mouse_encode.zig:37` 的 `last_cell` 同样持久存在，
    /// `Surface.zig:3713` 只在移动时写入、从不在模式开关时清空。
    /// ghostty 唯一相关的拦阻条件是
    /// `if (event.action == .motion and opts.format != .sgr_pixels)`（`:108-113`），
    /// 与这里的 `if !self.sgr_pixels_active()` 等价。
    ///
    /// 之所以不会「吞掉新模式后的第一次移动」：像素格式下整段去重被跳过，
    /// 报文照发；切回单元格格式后，只要指针换了格子缓存自然失效。
    ///
    /// 本用例钉住三条：同格重复 → 去重；换格 → 必发；1016 开关一轮后换格
    /// → 仍必发（缓存按格子失效，不因模式切换而误判）。
    #[gpui::test]
    fn mouse_move_dedup_is_per_cell_and_survives_1016_toggle(cx: &mut TestAppContext) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = written.clone();
        let terminal = cx.new(move |_| {
            let mut t = Terminal::create_error_terminal();
            t.terminal_bounds = bounds_for(80, 24);
            t.remote_input_callback = Some(Box::new(move |bytes| {
                sink.lock().unwrap().extend_from_slice(bytes);
            }));
            t
        });

        // `mouse_mode()` 读的是 alacritty 的实时模式位，所以必须真的把
        // `?1002h`（按钮拖动）+ `?1003h`（任意移动）喂进 vte。
        //
        // 但报文编码读的是 `last_content.mode` **快照**（`mouse_move` 尾部），
        // 它只在 `update_last_content` 里从 `Term` 同步（真实路径由每帧负责）。
        // 只置位 Term 而不刷新快照，`mouse_mode()` 会通过、报文却因快照里没有
        // MOUSE_MOTION 而被 `mouse_moved_report` 判为 None —— 表现为一次都不写。
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?1002h\x1b[?1003h", false, cx);
            let _ = t.update_last_content(false);
            assert!(t.mouse_mode(false), "1002/1003 置位后应进入鼠标模式");
        });
        written.lock().unwrap().clear();

        // bounds_for 是 9x16 的格子、原点 (0,0)：x∈[9,18) 是列 1，
        // x∈[18,27) 是列 2；y=24 恒定落在行 1 中部，纵向不参与断言。
        let ev = |x: f32| gpui::MouseMoveEvent {
            position: point(px(x), px(24.0)),
            pressed_button: Some(gpui::MouseButton::Left),
            modifiers: gpui::Modifiers::default(),
        };

        // 1) 列 1 内首次移动 → 上报一次。
        terminal.update(cx, |t, cx| {
            t.mouse_move(&ev(13.0), cx);
        });
        let after_first = written.lock().unwrap().len();
        assert!(after_first > 0, "首次移动应上报");

        // 2) 同一格内再移动 → 被去重。
        terminal.update(cx, |t, cx| {
            t.mouse_move(&ev(14.0), cx);
        });
        assert_eq!(
            written.lock().unwrap().len(),
            after_first,
            "同格重复移动应被去重"
        );

        // 3) 换到列 2 → 新格子必发。
        terminal.update(cx, |t, cx| {
            t.mouse_move(&ev(22.0), cx);
        });
        let after_cross = written.lock().unwrap().len();
        assert!(after_cross > after_first, "跨格移动必须上报");

        // 4) 开关 1016 一轮（指针不动）。
        terminal.update(cx, |t, cx| {
            t.process_pty_data(b"\x1b[?1016h", false, cx);
            t.process_pty_data(b"\x1b[?1016l", false, cx);
        });
        assert!(
            !terminal.read_with(cx, |t, _| t.sgr_pixels_active()),
            "一轮开关后 1016 应回到复位状态"
        );

        // 5) 回到列 1（缓存里此刻是列 2）→ 属于换格，必发。开关本身不清缓存，
        //    但换格令缓存失效，所以这次不会被陈旧坐标吞掉。
        terminal.update(cx, |t, cx| {
            t.mouse_move(&ev(13.0), cx);
        });
        assert!(
            written.lock().unwrap().len() > after_cross,
            "1016 开关后换格移动必须上报（缓存按格子失效，不因模式切换而误判）"
        );
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
            selection_range: None,
            palette: Default::default(),
            palette_hash: 0,
            row_hashes: Vec::new(),
            scroll_shift: 0,
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

#[cfg(test)]
mod powershell_cwd_sync_tests {
    use super::powershell_cwd_sync_args;

    #[cfg(windows)]
    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(windows)]
    #[test]
    fn injects_noexit_command_for_powershell_variants() {
        for program in [
            "powershell.exe",
            "powershell",
            "pwsh.exe",
            "pwsh",
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
        ] {
            let out = powershell_cwd_sync_args(program, &[]).expect("PowerShell 应注入 cwd 同步");
            assert_eq!(out[0], "-NoExit");
            assert_eq!(out[1], "-Command");
            // 外层引号必须自带：本地终端 escape_args == false，alacritty 会把
            // 参数原样拼进 CreateProcess 命令行
            assert!(
                out[2].starts_with('"') && out[2].ends_with('"') && out[2].len() > 2,
                "脚本参数应自带外层双引号"
            );
            let script = &out[2][1..out[2].len() - 1];
            // 脚本内出现双引号会破坏命令行引号结构（无法转义）
            assert!(!script.contains('"'), "脚本内不能含双引号");
            assert!(script.contains("SetCurrentDirectory"));
            assert!(script.contains("function global:prompt"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn leaves_other_shells_and_user_command_args_alone() {
        assert!(powershell_cwd_sync_args("cmd.exe", &[]).is_none());
        assert!(powershell_cwd_sync_args("wsl.exe", &[]).is_none());
        assert!(powershell_cwd_sync_args("bash", &[]).is_none());
        // 用户自带 -Command / -File 时不打扰
        assert!(
            powershell_cwd_sync_args(
                "powershell.exe",
                &args(&["-NoProfile", "-Command", "Get-Date"])
            )
            .is_none()
        );
        assert!(powershell_cwd_sync_args("pwsh.exe", &args(&["-File", "x.ps1"])).is_none());
        // 无害的用户参数（-NoProfile）仍应注入
        let out = powershell_cwd_sync_args("powershell.exe", &args(&["-NoProfile"]))
            .expect("-NoProfile 不该阻止注入");
        assert_eq!(out[0], "-NoProfile");
        assert_eq!(out[1], "-NoExit");
        assert_eq!(out[2], "-Command");
    }

    #[cfg(not(windows))]
    #[test]
    fn noop_on_non_windows() {
        // unix 的 cwd 由 OSC 7 主动上报，注入属于超范围行为变更
        assert!(powershell_cwd_sync_args("powershell.exe", &[]).is_none());
        assert!(powershell_cwd_sync_args("pwsh", &[]).is_none());
    }
}
