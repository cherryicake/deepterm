//! app 侧连接层经 `deepcore connect` 的端到端验证。
//!
//! 默认忽略：它会启动真实的 core/holder 进程。只在 Unix 上运行，并且把
//! 运行时目录指到临时目录，避免碰到用户正在使用的 core：
//!
//! ```text
//! cargo test --test deepcore_bridge_e2e -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::time::{Duration, Instant};

use deepterm::daemon::client::Client;
use serde_json::json;

#[test]
#[ignore = "spawns isolated deepcore / holder processes"]
fn the_app_client_connects_through_the_stdio_bridge() {
    // 必须短：`DEEPTERM_RUNTIME_DIR` 覆盖本身不做过长路径兜底，而 macOS 的
    // `$TMPDIR` 常常已经超过 Unix socket 的 `sun_path` 上限。
    let runtime = std::path::PathBuf::from(format!(
        "/tmp/dt-bridge-e2e-{}-{}",
        std::process::id(),
        uuid_like_nonce()
    ));
    std::fs::create_dir_all(&runtime).expect("create isolated runtime dir");
    let home = std::env::temp_dir().join(format!(
        "deepterm-bridge-home-{}-{}",
        std::process::id(),
        uuid_like_nonce()
    ));
    std::fs::create_dir_all(&home).expect("create isolated home");
    #[cfg(target_os = "macos")]
    std::fs::create_dir_all(home.join("Library/Application Support/deepterm"))
        .expect("create isolated config dir");
    #[cfg(all(unix, not(target_os = "macos")))]
    std::fs::create_dir_all(home.join(".config/deepterm")).expect("create isolated config dir");

    // 集成测试是独立测试进程，此时还没有创建连接线程；在启动任何 core 之前
    // 设置环境变量，保证 GUI 客户端、connect 子进程与 core 看见同一套路径。
    // SAFETY：本测试进程此刻尚无并发环境变量读写。
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var(deepterm::daemon::RUNTIME_DIR_ENV, &runtime);
        std::env::set_var(
            deepterm::daemon::client::DAEMON_ENV,
            env!("CARGO_BIN_EXE_deepcore"),
        );
    }

    let client = Client::connect_or_start().expect("app client should connect through bridge");
    let ping = client
        .call_timeout(json!({"op": "ping"}), Duration::from_secs(5))
        .expect("ping through bridge");
    assert_eq!(ping["ok"], true, "ping failed: {ping}");
    assert_eq!(ping["version"], env!("CARGO_PKG_VERSION"));

    let list = client
        .call_timeout(json!({"op": "list"}), Duration::from_secs(5))
        .expect("list through bridge");
    assert_eq!(list["ok"], true, "list failed: {list}");
    assert_eq!(list["terminals"], json!([]));

    let marker = "DEEPTERM_BRIDGE_READY";
    let spawn = client
        .call_timeout(
            json!({
                "op": "spawn",
                "shell": "/bin/sh",
                "args": ["-c", format!("printf '{marker}\\n'; sleep 30")],
                "remote": true,
                "cols": 80,
                "rows": 24,
            }),
            Duration::from_secs(5),
        )
        .expect("spawn through bridge");
    assert_eq!(spawn["ok"], true, "spawn failed: {spawn}");
    let address = spawn["address"]
        .as_str()
        .expect("terminal address")
        .to_string();

    let attached = client
        .call_timeout(
            json!({"op": "attach", "address": address, "history": 0, "cols": 80, "rows": 24}),
            Duration::from_secs(5),
        )
        .expect("attach through bridge");
    assert_eq!(attached["ok"], true, "attach failed: {attached}");
    let channel = attached["channel"].as_u64().expect("attach channel") as u32;
    let rx = client.attach_channel(channel);
    let mut screen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !contains(&screen, marker.as_bytes()) {
        match rx.try_recv() {
            Ok(frame) => screen.extend_from_slice(&frame.payload),
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(async_channel::TryRecvError::Closed) => break,
        }
    }
    assert!(
        contains(&screen, marker.as_bytes()),
        "terminal output did not contain {marker}: {:?}",
        String::from_utf8_lossy(&screen)
    );

    // 模拟关窗口：桥进程随 Client 一起结束，但 core/holder 与终端必须留下。
    drop(client);
    std::thread::sleep(Duration::from_millis(300));
    let client = Client::connect_or_start().expect("client should reconnect after bridge exit");
    let list = client
        .call_timeout(json!({"op": "list"}), Duration::from_secs(5))
        .expect("list after reconnect");
    assert_eq!(list["ok"], true, "list after reconnect failed: {list}");
    let terminal = list["terminals"]
        .as_array()
        .expect("terminals array")
        .iter()
        .find(|terminal| terminal["address"].as_str() == Some(address.as_str()))
        .expect("the same terminal must survive a bridge disconnect");
    assert_eq!(
        terminal["state"], "live",
        "terminal exited unexpectedly: {terminal}"
    );
    let close = client
        .call_timeout(
            json!({"op": "close", "address": address}),
            Duration::from_secs(5),
        )
        .expect("close terminal");
    assert_eq!(close["ok"], true, "close failed: {close}");

    drop(client);
    terminate_isolated_daemons(&runtime);
    let _ = std::fs::remove_dir_all(runtime);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn uuid_like_nonce() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn terminate_isolated_daemons(runtime: &std::path::Path) {
    // 新版 holder 使用 `holder.<id>.pid`，不能只清理旧的 `holder.pid`；
    // 否则测试断言失败时会把 holder 留在隔离目录里。
    let pid_files: Vec<_> = std::fs::read_dir(runtime)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "pid"))
        .collect();
    let pids: Vec<libc::pid_t> = pid_files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| text.trim().parse::<libc::pid_t>().ok())
        .collect();
    for &pid in &pids {
        // SAFETY：只向本测试刚在隔离运行时目录里启动、并由 pid 文件指明的
        // 进程发送 SIGTERM。单独发送信号，不改变信号处理方式。
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let alive = pids.iter().any(|&pid| {
            // SAFETY：信号 0 只检查进程是否存在，不投递信号。
            unsafe { libc::kill(pid, 0) == 0 }
        });
        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
