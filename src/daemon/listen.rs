//! 守护进程入口：监听 socket、接受连接。
//!
//! # 生命周期
//!
//! 守护进程**不因为客户端断开而退出**——那正是它的全部意义。它只在两种
//! 情况下结束：
//!
//! 1. 显式的 `shutdown` 请求（留给用户手动清理）；
//! 2. 一段时间内**一个连接都没有，且一个终端都不剩**（避免空转）。
//!
//! 第 2 条不能只看"没有连接"：用户可能关掉窗口、终端还在跑，这时候
//! 守护进程必须活着。

use super::transport::{self, Listener, Stream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::connection::serve_connection;
use super::machines::Machines;
use super::registry::Registry;

/// 没有连接时，守护进程空转多久后自行退出。
///
/// 取 5 分钟：足够覆盖"关掉窗口再马上打开"的情况，又不至于让一个完全
/// 没人用的守护进程挂一整天。只要有终端活着就永远不退出。
const IDLE_EXIT: Duration = Duration::from_secs(300);

/// 接受循环的轮询间隔。
///
/// 监听器没有带超时的 `accept`，用非阻塞 + `sleep` 实现（Unix）；
/// Windows 的命名管道是阻塞 accept，靠关停信号与进程退出结束。
/// 50ms 的响应延迟对"连上守护进程"这件事完全够用，又不至于忙等。
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// 多久清扫一次已结束的终端记录。
///
/// 用户关标签后终端会变成 `finished`，但记录不会自己消失——不清的话，
/// 一个跑了几天的守护进程会攒下成百上千条死记录，`list` 越来越慢，
/// 客户端每次也要多解一遍没人用的 JSON。
///
/// 取 30 秒：足够让客户端在这段时间里读到 `finished` 状态并给出提示
/// （"程序已退出"），又不至于让记录长期滞留。
const REAP_INTERVAL: Duration = Duration::from_secs(30);

/// 跨平台的 `accept`。
///
/// Unix 的 `accept` 返回 `(stream, addr)`，Windows 的命名管道只返回
/// `stream`——差异收在这里，接受循环本身两边共用。
fn listener_accept(listener: &Listener) -> std::io::Result<Stream> {
    #[cfg(unix)]
    {
        listener.accept().map(|(stream, _addr)| stream)
    }
    #[cfg(windows)]
    {
        listener.accept()
    }
}

/// 收到 SIGTERM / SIGINT 时置位，让监听循环走正常的退出路径。
///
/// 为什么需要：不处理信号时进程被直接杀掉，socket 与 pid 文件留在原地。
/// 下次启动虽然能清理（`bind` 前会删残留），但用户 `kill` 掉一个守护进程
/// 后发现 `daemon.sock` 还在，会以为它没死透。
///
/// 用 `libc::signal` 而不是引一个信号库：这里只需要"收到就置个标志"，
/// 处理函数里只做一次原子写，是 async-signal-safe 的。
#[cfg(unix)]
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn request_shutdown(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// 装好退出信号的处理器。
#[cfg(unix)]
fn install_signal_handlers() {
    // `sighandler_t` 在 unix 上是 `usize`，不能直接从函数项转——先取
    // 函数指针再转整数，跨平台（含 macOS 的 `Option<fn>` 表示）都正确。
    let handler = request_shutdown as extern "C" fn(libc::c_int) as usize;
    // SAFETY: 处理函数只做一次原子写，是 async-signal-safe 的；`signal` 的
    // 旧值不需要保存（进程退出即弃）。
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

/// 启动守护进程的监听循环（阻塞）。
pub fn run() -> std::io::Result<()> {
    // core 不亲自持有 PTY：它随时可能被重启（升级、崩溃恢复），
    // 亲自建 PTY 会让终端跟着一起死。转给 holder。
    run_at(
        super::socket_path(),
        true,
        super::connection::Backend::Holder,
        Machines::load_default(),
    )
}

/// 在指定 socket 上跑监听循环。
///
/// 抽出来是为了让 holder 复用同一套接受/分发逻辑（它只是换了个地址）。
/// `idle_exit` 控制"没终端时是否自行退出"：core 该退（用户关掉 deepterm
/// 就别留空进程），holder 在有终端时要一直活着。
pub fn run_at(
    socket: std::path::PathBuf,
    idle_exit: bool,
    backend: super::connection::Backend,
    machines: Arc<Machines>,
) -> std::io::Result<()> {
    let dir = super::runtime_dir();

    // 权限：socket 拿到就等于拿到用户的所有终端，所以目录必须 0700。
    //
    // **创建时就用 0700**，不能"先按 umask 建、再 chmod"：那样在多用户
    // 机器上留了一个窗口，别的用户可以在 chmod 之前把 socket 抢过来或
    // 塞一个假的进去（`/tmp` 下尤其现实——兜底路径就在那里）。
    //
    // `create_dir_all` 没有"指定权限"的参数，所以逐级建：已存在的目录
    // 不再碰（可能属于父级，动它会破坏别的东西）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if !dir.exists() {
            // 先按 0700 建目录本身，再逐级补父目录：`create_dir_all` 没有
            // "指定权限"参数，先建后 chmod 会留下一个按 umask 可写的窗口。
            let created = std::fs::create_dir(&dir).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    // 父目录还不存在：退化成 create_dir_all，然后收紧自己这层。
                    std::fs::create_dir_all(&dir).and_then(|()| {
                        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    })
                } else {
                    Err(e)
                }
            });
            created?;
        }
        // 已存在也校验：目录可能被别的进程按更宽的权限建过，或者是别人
        // 预置的软链。校验失败必须中止——带着可读的 token 继续跑，等于
        // 把该用户的所有终端交给同机其他用户。
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        super::verify_private_dir(&dir)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir)?;
    }

    let path = socket;

    // 先探一下：已经有守护进程在跑就别抢同一个地址。
    //
    // Unix 上还要清掉残留的 socket 文件（上次没清理干净时 bind 会失败）；
    // Windows 的命名管道没有对应的文件，`exists()` 恒为 false，清也没意义。
    let already_running = if cfg!(windows) {
        transport::connect(&path).is_ok()
    } else {
        path.exists() && transport::connect(&path).is_ok()
    };
    if already_running {
        log::info!("守护进程已在运行（{}），本实例退出", path.display());
        return Ok(());
    }

    // 抢占 pid 文件，**必须在删 socket / bind 之前**。
    //
    // 上面那次 `connect` 探测有竞态：两个实例同时启动时都能探测失败，然后
    // 先后 `remove_file` + `bind`，后者把前者的 socket 删掉，两个都活着、
    // 客户端却在两边之间漂移。`create_new` 是原子的，谁先建到谁就是合法
    // 监听者。
    let pid_file = path.with_extension("pid");
    let pid = std::process::id().to_string();
    if !claim_pid_file(&pid_file, &pid)? {
        log::warn!("{} 已被仍在运行的进程占用，本实例退出", pid_file.display());
        return Ok(());
    }

    #[cfg(unix)]
    let _ = std::fs::remove_file(&path);

    // 生成并落盘鉴权 token（0600）。
    //
    // **必须在 bind 之前**：客户端一旦能连上 socket 就会立即读 token 并握手。
    // 先 bind 后写 token 会留下一个很短的窗口，桥进程恰好在这时连上就会得到
    // “读不到守护进程 token”，把一次正常冷启动判成失败。
    //
    // 每次启动换一个：它是"本次守护进程的会话凭据"，不是长期密码，
    // 不需要也不应该持久化到用户配置里。
    let token = super::generate_token();
    // 写不进去就必须中止：`authenticate` 在读不到 token 时会拒绝所有连接
    // （见 `connection::authenticate`），此时进程只会空转 5 分钟然后退出，
    // 用户看到的是"鉴权失败"而不是"守护进程坏了"。
    super::write_token_for(&path, &token)?;

    let listener = transport::bind(&path)?;
    transport::set_nonblocking(&listener, true)?;

    log::info!("守护进程已启动，socket: {}", path.display());

    install_signal_handlers();

    // 身份在**进程启动时**固定：同一 core 上的所有连接看到同一个 generation，
    // core 重启后才变化。app 用它判断“是重连还是换了一个 core”。
    let identity = Arc::new(super::identity::ServerIdentity::detect());
    identity.start_linger_refresh();
    let registry = Arc::new(Registry::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let idle_since = std::sync::Mutex::new(std::time::Instant::now());
    let mut last_reap = std::time::Instant::now();

    // core 负责协调所有 holder。必须在 accept 之前接管：GUI 恢复会话时会
    // 立刻 attach，地址到 holder 的映射晚一步就会出现“终端明明在跑却
    // not_found”。旧 holder 也在这里被接上，它手里的终端继续可用。
    if backend == super::connection::Backend::Holder
        && let Err(error) = super::core_holder::adopt_all()
    {
        log::error!("接管现有 holder 失败: {error}");
    }

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // 收到 SIGTERM/SIGINT：走正常退出路径，把 socket 与 pid 文件清掉。
        #[cfg(unix)]
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            log::info!("收到退出信号，守护进程准备退出");
            break;
        }

        // Windows 的命名管道是阻塞 accept：没有客户端时会一直停在这里，
        // 上面的空闲退出判断根本轮不到执行。守护进程因此永不自行退出——
        // 对一个"活着才有意义"的常驻进程来说这可以接受，但要有明确说明，
        // 免得以后有人把"空闲退出"当成跨平台行为来依赖。
        //
        // Unix 侧靠下面的 `WouldBlock` 分支做空闲退出。
        match listener_accept(&listener) {
            Ok(stream) => {
                let _ = transport::set_blocking(&stream, true);
                connections.fetch_add(1, Ordering::SeqCst);
                *idle_since.lock().expect("idle") = std::time::Instant::now();

                let registry = registry.clone();
                let shutdown = shutdown.clone();
                let socket_for_conn = path.clone();
                let identity = identity.clone();
                let machines = machines.clone();
                let connections = connections.clone();
                let counter = connections.clone();
                std::thread::Builder::new()
                    .name("daemon-conn".into())
                    .spawn(move || {
                        serve_connection(
                            stream,
                            registry,
                            shutdown,
                            backend,
                            machines,
                            socket_for_conn.clone(),
                            identity,
                        );
                        counter.fetch_sub(1, Ordering::SeqCst);
                    })
                    .map_err(|e| {
                        connections.fetch_sub(1, Ordering::SeqCst);
                        log::warn!("无法启动连接线程: {e}");
                        e
                    })
                    .ok();
            }
            // Unix：非阻塞 accept 没连接时返回 `WouldBlock`，顺便检查空闲退出。
            //
            // Windows 的命名管道是**阻塞** accept，这条分支在那里不会命中；
            // 它的空闲退出由下面 `Err(e)` 之前的平台分支处理。
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // 周期性清扫已结束的终端记录（见 `REAP_INTERVAL`）。
                if last_reap.elapsed() >= REAP_INTERVAL {
                    let removed = registry.reap();
                    if !removed.is_empty() {
                        log::info!("清扫了 {} 个已结束的终端: {:?}", removed.len(), removed);
                    }
                    last_reap = std::time::Instant::now();
                }

                // 空闲退出：没有连接，也没有终端。
                // drain 的 holder 是“只服务旧终端”的：终端一空就退出，不再
                // 占着一个永远等新任务的进程。core 连接仍在也没关系——它的
                // 出现只是为了让旧终端还能被 attach。
                if super::holder::draining() && registry.is_empty() {
                    log::info!("holder 已 drain 且没有终端，退出");
                    break;
                }
                let idle = *idle_since.lock().expect("idle");
                if idle_exit
                    && connections.load(Ordering::SeqCst) == 0
                    && registry.is_empty()
                    && idle.elapsed() >= IDLE_EXIT
                {
                    log::info!("守护进程空闲超时，退出");
                    break;
                }
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(e) => {
                log::warn!("accept 失败: {e}");
                // 文件描述符耗尽（`EMFILE`/`ENFILE`）在"一连接一线程"的
                // 模型下很现实，而这条分支会以 20 Hz 无限刷日志、什么也不
                // 恢复。按平台区分：资源类错误退避，其余继续轮询。
                #[cfg(unix)]
                {
                    use std::io::ErrorKind;
                    if matches!(
                        e.kind(),
                        ErrorKind::OutOfMemory | ErrorKind::Other | ErrorKind::PermissionDenied
                    ) {
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                }
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    }

    // 只删自己登记的文件：core 被 SIGTERM 后可能已经有新实例 bind 成功，
    // 无条件 `remove_file` 会把新守护进程的 socket/pid/token 一起删掉，
    // 让它活着却永远不可达。
    let owns_pid_file = std::fs::read_to_string(&pid_file)
        .ok()
        .is_some_and(|text| text.trim() == std::process::id().to_string());
    if owns_pid_file {
        #[cfg(unix)]
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&pid_file);
        let _ = std::fs::remove_file(super::token_path_for(&path));
    } else {
        log::info!("pid 文件已属于新实例，保留其 socket 与 token");
    }
    log::info!("守护进程已退出");
    Ok(())
}

/// 抢占 pid 文件，返回自己是否成为合法监听者。
///
/// 用 `create_new` 而不是 `write`：两个实例同时启动时，这是唯一的原子
/// 仲裁点。已存在时只有在**持有者已经不在**的情况下才接管，否则让位。
fn claim_pid_file(pid_file: &std::path::Path, pid: &str) -> std::io::Result<bool> {
    use std::io::Write as _;

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pid_file)
    {
        Ok(mut file) => {
            file.write_all(pid.as_bytes())?;
            file.sync_all()?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let holder = std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            let stale = holder.is_none_or(|holder| !process_is_alive(holder));
            if !stale {
                return Ok(false);
            }
            std::fs::write(pid_file, pid)?;
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

/// 进程是否存在（用于判断残留的 pid 文件是否还能代表一个活着的守护进程）。
///
/// 信号 0 只做存在性检查，不投递信号。
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY:`kill` 只做存在性检查，不投递信号。
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Windows 上无法用信号探测：按"已残留"处理，交由 `create_new` 之外的
    // 命名管道探测兜底。
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空闲退出阈值必须足够长，否则"关窗口再打开"会丢掉终端。
    ///
    /// 这条钉住的是一个很容易被"优化"掉的常量：把它调小到几十秒，
    /// 功能测试全过，但用户中午关窗口吃饭回来，终端就没了。
    #[test]
    fn the_idle_timeout_leaves_room_for_a_break() {
        assert!(
            IDLE_EXIT >= Duration::from_secs(120),
            "空闲退出不能短于 2 分钟，实际 {IDLE_EXIT:?}"
        );
    }

    /// 接受循环必须能及时看到关停信号，不能阻塞在 accept 上。
    #[test]
    fn the_accept_poll_is_short_enough_to_notice_shutdown() {
        assert!(
            ACCEPT_POLL <= Duration::from_millis(500),
            "轮询间隔太长，关停响应会迟滞: {ACCEPT_POLL:?}"
        );
    }
}
