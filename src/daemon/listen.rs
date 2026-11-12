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

use std::os::unix::net::{UnixListener, UnixStream};
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
/// `UnixListener` 没有超时的 `accept`，用非阻塞 + `sleep` 实现。
/// 50ms 的响应延迟对"连上守护进程"这件事完全够用，又不至于忙等。
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// 启动守护进程的监听循环（阻塞）。
pub fn run() -> std::io::Result<()> {
    let dir = super::runtime_dir();
    std::fs::create_dir_all(&dir)?;

    // 权限收紧：socket 拿到就等于拿到用户的所有终端。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let path = super::socket_path();

    // 先探一下：已经有守护进程在跑就别抢 socket。
    if path.exists() && UnixStream::connect(&path).is_ok() {
        log::info!("守护进程已在运行（{}），本实例退出", path.display());
        return Ok(());
    }
    // 残留的 socket 文件（上次没清理干净）。
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;

    // pid 文件：便于排障与"这个 socket 是谁的"。
    let _ = std::fs::write(dir.join("daemon.pid"), std::process::id().to_string());

    log::info!("守护进程已启动，socket: {}", path.display());

    let registry = Arc::new(Registry::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let idle_since = std::sync::Mutex::new(std::time::Instant::now());

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                let _ = stream.set_nonblocking(false);
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
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
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
