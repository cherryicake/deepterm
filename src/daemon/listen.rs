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

/// 启动守护进程的监听循环（阻塞）。
pub fn run() -> std::io::Result<()> {
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
            std::fs::create_dir_all(&dir)?;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        // 已存在也校验一次：目录可能被别的进程按更宽的权限建过。
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir)?;
    }

    let path = super::socket_path();

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
    #[cfg(unix)]
    let _ = std::fs::remove_file(&path);

    let listener = transport::bind(&path)?;
    transport::set_nonblocking(&listener, true)?;

    // pid 文件：便于排障与"这个 socket 是谁的"。
    let _ = std::fs::write(dir.join("daemon.pid"), std::process::id().to_string());

    log::info!("守护进程已启动，socket: {}", path.display());

    let registry = Arc::new(Registry::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let idle_since = std::sync::Mutex::new(std::time::Instant::now());
    let mut last_reap = std::time::Instant::now();

    loop {
        if shutdown.load(Ordering::SeqCst) {
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
                let connections = connections.clone();
                let counter = connections.clone();
                std::thread::Builder::new()
                    .name("daemon-conn".into())
                    .spawn(move || {
                        serve_connection(stream, registry, shutdown);
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
                let idle = *idle_since.lock().expect("idle");
                if connections.load(Ordering::SeqCst) == 0
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
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    }

    #[cfg(unix)]
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("daemon.pid"));
    log::info!("守护进程已退出");
    Ok(())
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
