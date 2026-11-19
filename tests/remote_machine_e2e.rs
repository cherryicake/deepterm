//! 真实远端机器端到端测试：本地 core → SSH → 远端 `deepcore connect`。
//!
//! 默认忽略，因为它会建立真实 SSH 连接，并会通过本机 HTTP 发布夹具把
//! Linux `deepcore` 自动安装到远端的隔离 session 目录：
//!
//! ```text
//! DEEPTERM_TEST_SSH_HOST=192.168.194.4 \
//! DEEPTERM_TEST_SSH_USER=zhousong \
//! DEEPTERM_TEST_CORE_ARTIFACT=/tmp/deepcore-linux-x86_64.tar.gz \
//! cargo test --test remote_machine_e2e -- --ignored --nocapture
//! ```
//!
//! 经本机 `ssh -L` 隧道运行时，`DEEPTERM_TEST_SSH_HOST/PORT` 指向隧道入口，
//! 另用 `DEEPTERM_TEST_SSH_REMOTE_HOST/PORT` 指向真实 SSH 服务器供测试
//! 校验进程与文件。

#![cfg(unix)]

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deepterm::daemon::client::Client;
use deepterm::ssh::SshConnectionConfig;
use deepterm::ssh::config::AuthMethod;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[test]
#[ignore = "requires a reachable SSH host and a Linux deepcore tar.gz artifact"]
fn a_remote_terminal_survives_local_core_restart() {
    let host = std::env::var("DEEPTERM_TEST_SSH_HOST").unwrap_or_else(|_| "192.168.194.4".into());
    let user = std::env::var("DEEPTERM_TEST_SSH_USER").unwrap_or_else(|_| "zhousong".into());
    let port = std::env::var("DEEPTERM_TEST_SSH_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(22);
    let remote_host =
        std::env::var("DEEPTERM_TEST_SSH_REMOTE_HOST").unwrap_or_else(|_| host.clone());
    let remote_port = std::env::var("DEEPTERM_TEST_SSH_REMOTE_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(port);
    let artifact = std::env::var("DEEPTERM_TEST_CORE_ARTIFACT")
        .map(std::path::PathBuf::from)
        .expect("set DEEPTERM_TEST_CORE_ARTIFACT to a Linux deepcore tar.gz");
    assert!(
        artifact.is_file(),
        "test core artifact not found: {}",
        artifact.display()
    );
    let release_server = ReleaseServer::start(&artifact);

    // 用测试自己管理的本地 TCP 中继冒充 SSH 入口，这样才能在链路中途
    // 主动切断连接、验证 supervisor 重拨。中继的上游通常是指向真实服务器的
    // 系统 `ssh -L` 隧道（macOS 对未签名测试二进制的直连有本地网络限制）。
    let relay = Relay::start(&host, port);
    let relay_host = "127.0.0.1".to_string();
    let relay_port = relay.port;

    let original_home = std::env::var("HOME").unwrap_or_default();
    let key = std::env::var("DEEPTERM_TEST_SSH_KEY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(&original_home).join(".ssh/id_rsa"));
    assert!(key.is_file(), "test SSH key not found: {}", key.display());

    let runtime = std::path::PathBuf::from(format!(
        "/tmp/dt-remote-e2e-{}-{}",
        std::process::id(),
        nonce()
    ));
    let home = std::env::temp_dir().join(format!(
        "deepterm-remote-home-{}-{}",
        std::process::id(),
        nonce()
    ));
    std::fs::create_dir_all(&runtime).expect("create isolated runtime dir");
    std::fs::create_dir_all(&home).expect("create isolated home");
    #[cfg(target_os = "macos")]
    std::fs::create_dir_all(home.join("Library/Application Support/deepterm"))
        .expect("create isolated config dir");
    #[cfg(all(unix, not(target_os = "macos")))]
    std::fs::create_dir_all(home.join(".config/deepterm")).expect("create isolated config dir");
    seed_known_hosts(&home, &relay_host, relay_port);

    // SAFETY：本集成测试进程只运行这一个用例，设置环境变量时没有并发读写。
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var(deepterm::daemon::RUNTIME_DIR_ENV, &runtime);
        std::env::set_var(
            deepterm::daemon::deploy::RELEASES_URL_ENV,
            release_server.url(),
        );
        std::env::set_var(
            deepterm::daemon::client::DAEMON_ENV,
            env!("CARGO_BIN_EXE_deepcore"),
        );
    }

    let mut ssh = SshConnectionConfig::new(
        &relay_host,
        &user,
        AuthMethod::PrivateKey {
            path: key,
            passphrase: None,
        },
    );
    ssh.port = relay_port;
    ssh.nickname = Some("lingsi-e2e".into());
    let session = format!("e2e-{}", nonce());
    let mut cleanup = TestCleanup {
        runtime: runtime.clone(),
        home: home.clone(),
        remote_host: remote_host.clone(),
        remote_port,
        user: user.clone(),
        session: session.clone(),
        done: false,
    };

    let client = Client::connect_or_start().expect("connect local core");
    let added = call(
        &client,
        json!({
            "op": "machine.add",
            "name": "lingsi-e2e",
            "session": session,
            "ssh": ssh,
        }),
    );
    assert_eq!(added["ok"], true, "machine.add failed: {added}");
    let machine = added["machine"]["name"]
        .as_str()
        .expect("machine name")
        .to_string();
    wait_until_connected(&client, &machine);
    wait_until_linger(&client, &machine, &user);

    assert_eq!(
        remote_core_version(remote_host.as_str(), remote_port, &user, &session)
            .expect("remote core was not installed into the isolated session"),
        env!("CARGO_PKG_VERSION"),
        "automatic install produced an unexpected core version"
    );

    let spawn = call(
        &client,
        json!({
            "op": "spawn",
            "machine": machine,
            "shell": "/bin/sh",
            "args": ["-c", "i=0; while :; do printf 'REMOTE_E2E:%s\\n' \"$i\"; i=$((i+1)); sleep 1; done"],
            "cols": 80,
            "rows": 24,
        }),
    );
    assert_eq!(spawn["ok"], true, "remote spawn failed: {spawn}");
    let address = spawn["address"]
        .as_str()
        .expect("remote terminal address")
        .to_string();
    assert!(address.starts_with(&format!("{machine}/")), "{address}");

    let first = attach_and_read(&client, &address, 0);
    assert!(
        first
            .windows(b"REMOTE_E2E:".len())
            .any(|w| w == b"REMOTE_E2E:"),
        "no remote output: {:?}",
        String::from_utf8_lossy(&first)
    );

    // 发布更新：强制重装同一版本，走完整的 manifest → 下载 → SHA-256 →
    // 上传 → 探针 → 原子替换 → 停旧 core 路径。远端 core PID 必须变化，
    // holder 与同一个 machine/tN 终端必须保留。
    let generation = machine_generation(&client, &machine);
    let core_before_update = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "daemon.pid",
    )
    .expect("remote core pid before update");
    let holder_before_update = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "holder.pid",
    )
    .expect("remote holder pid before update");
    let update = call(&client, json!({"op": "machine.update", "name": machine}));
    assert_eq!(update["ok"], true, "machine.update failed: {update}");
    wait_until_generation(&client, &machine, generation);

    let core_after_update = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "daemon.pid",
    )
    .expect("remote core pid after update");
    let holder_after_update = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "holder.pid",
    )
    .expect("remote holder pid after update");
    assert_ne!(
        core_after_update, core_before_update,
        "core update must restart the remote core"
    );
    assert_eq!(
        holder_after_update, holder_before_update,
        "core update must not replace the holder"
    );
    let after_update = attach_and_read(&client, &address, 0);
    assert!(
        after_update
            .windows(b"REMOTE_E2E:".len())
            .any(|w| w == b"REMOTE_E2E:"),
        "terminal disappeared after remote core update: {:?}",
        String::from_utf8_lossy(&after_update)
    );

    // 校验失败必须在覆盖二进制和停旧 core 之前终止，现有终端不受影响。
    let generation = machine_generation(&client, &machine);
    let checksum_before = remote_core_sha256(remote_host.as_str(), remote_port, &user, &session)
        .expect("sha256 of installed remote core");
    release_server.set_corrupt_sha(true);
    let rejected = call(&client, json!({"op": "machine.update", "name": machine}));
    assert_eq!(
        rejected["ok"], true,
        "machine.update request failed: {rejected}"
    );
    wait_until_state(
        &client,
        &machine,
        "install_failed",
        Some("SHA-256"),
        Duration::from_secs(20),
    );
    release_server.set_corrupt_sha(false);
    let checksum_after = remote_core_sha256(remote_host.as_str(), remote_port, &user, &session)
        .expect("sha256 after rejected update");
    assert_eq!(
        checksum_after, checksum_before,
        "a checksum failure must not replace the installed core"
    );
    wait_until_generation(&client, &machine, generation);
    let after_bad_checksum = attach_and_read(&client, &address, 0);
    assert!(
        after_bad_checksum
            .windows(b"REMOTE_E2E:".len())
            .any(|w| w == b"REMOTE_E2E:"),
        "terminal disappeared after rejected update: {:?}",
        String::from_utf8_lossy(&after_bad_checksum)
    );

    // 远端 core 重启不能杀 holder：终端进程仍要活着，新 core 起来后
    // 同一 `tN` 地址必须还能 attach，且本机 supervisor 要自动重拨。
    let generation = machine_generation(&client, &machine);
    let old_core_pid = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "daemon.pid",
    )
    .expect("remote core pid before restart");
    let old_holder_pid = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "holder.pid",
    )
    .expect("remote holder pid before restart");
    kill_remote_core(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        old_core_pid,
    );
    wait_until_generation(&client, &machine, generation);

    let new_core_pid = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "daemon.pid",
    )
    .expect("remote core pid after restart");
    let new_holder_pid = remote_pid(
        remote_host.as_str(),
        remote_port,
        &user,
        &session,
        "holder.pid",
    )
    .expect("remote holder pid after restart");
    assert_ne!(new_core_pid, old_core_pid, "remote core did not restart");
    assert_eq!(
        new_holder_pid, old_holder_pid,
        "remote core restart must not replace the holder"
    );
    let after_core_restart = attach_and_read(&client, &address, 200);
    assert!(
        after_core_restart
            .windows(b"REMOTE_E2E:".len())
            .any(|w| w == b"REMOTE_E2E:"),
        "the same remote terminal was not available after remote core restart: {:?}",
        String::from_utf8_lossy(&after_core_restart)
    );

    // 整条 SSH 传输断开：关闭已建立连接并在短时间内拒绝新连接。远端
    // holder/终端不受影响；中继恢复后本地 core 应自动重拨并重挂同一地址。
    let generation = machine_generation(&client, &machine);
    relay.drop_connections(Duration::from_millis(1200));
    wait_until_generation(&client, &machine, generation);
    let after_transport_drop = attach_and_read(&client, &address, 200);
    assert!(
        after_transport_drop
            .windows(b"REMOTE_E2E:".len())
            .any(|w| w == b"REMOTE_E2E:"),
        "the same remote terminal was not restored after SSH transport drop: {:?}",
        String::from_utf8_lossy(&after_transport_drop)
    );

    // 模拟本地 core 崩溃/升级：远端 core 与 holder 不应受影响。
    drop(client);
    terminate_local_daemons(&runtime);
    std::thread::sleep(Duration::from_millis(400));

    let client = Client::connect_or_start().expect("restart local core");
    let list = call(&client, json!({"op": "machine.list"}));
    assert_eq!(list["ok"], true, "machine.list after restart: {list}");
    assert!(
        list["machines"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["name"] == machine)),
        "machine table was not persisted: {list}"
    );
    wait_until_connected(&client, &machine);

    let second = attach_and_read(&client, &address, 200);
    assert!(
        second
            .windows(b"REMOTE_E2E:".len())
            .any(|w| w == b"REMOTE_E2E:"),
        "the same remote terminal was not restored: {:?}",
        String::from_utf8_lossy(&second)
    );

    let closed = call(&client, json!({"op": "close", "address": address}));
    assert_eq!(closed["ok"], true, "remote close failed: {closed}");

    drop(client);
    cleanup.run();
}

struct TestCleanup {
    runtime: std::path::PathBuf,
    home: std::path::PathBuf,
    remote_host: String,
    remote_port: u16,
    user: String,
    session: String,
    done: bool,
}

impl TestCleanup {
    fn run(&mut self) {
        if self.done {
            return;
        }
        terminate_local_daemons(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.home);
        cleanup_remote_session(
            &self.remote_host,
            self.remote_port,
            &self.user,
            &self.session,
        );
        self.done = true;
    }
}

impl Drop for TestCleanup {
    fn drop(&mut self) {
        self.run();
    }
}

struct Relay {
    port: u16,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<TcpStream>>>,
    accept_thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    fn start(upstream_host: &str, upstream_port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test relay");
        listener
            .set_nonblocking(true)
            .expect("make test relay nonblocking");
        let port = listener.local_addr().expect("relay address").port();
        let upstream = format!("{upstream_host}:{upstream_port}");
        let paused = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));

        let accept_thread = {
            let paused = paused.clone();
            let stop = stop.clone();
            let connections = connections.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut client, _)) => {
                            // macOS 会让 accepted socket 继承 listener 的非阻塞
                            // 标志；不关掉就会让 io::copy 读到 WouldBlock 后
                            // 立刻把 SSH 连接关掉。
                            client
                                .set_nonblocking(false)
                                .expect("make relay connection blocking");
                            if paused.load(Ordering::SeqCst) {
                                let _ = client.shutdown(Shutdown::Both);
                                continue;
                            }
                            let Ok(mut server) = TcpStream::connect(&upstream) else {
                                let _ = client.shutdown(Shutdown::Both);
                                continue;
                            };
                            match (client.try_clone(), server.try_clone()) {
                                (Ok(client_handle), Ok(server_handle)) => {
                                    let mut live = connections
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    if paused.load(Ordering::SeqCst) {
                                        let _ = client_handle.shutdown(Shutdown::Both);
                                        let _ = server_handle.shutdown(Shutdown::Both);
                                    } else {
                                        live.push(client_handle);
                                        live.push(server_handle);
                                    }
                                }
                                _ => {
                                    let _ = client.shutdown(Shutdown::Both);
                                    let _ = server.shutdown(Shutdown::Both);
                                    continue;
                                }
                            }
                            std::thread::spawn(move || {
                                match (client.try_clone(), server.try_clone()) {
                                    (Ok(mut upload_from), Ok(mut upload_to)) => {
                                        std::thread::spawn(move || {
                                            let _ = io::copy(&mut upload_from, &mut upload_to);
                                            let _ = upload_to.shutdown(Shutdown::Write);
                                        });
                                        let _ = io::copy(&mut server, &mut client);
                                    }
                                    _ => {}
                                }
                                let _ = client.shutdown(Shutdown::Both);
                                let _ = server.shutdown(Shutdown::Both);
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            port,
            paused,
            stop,
            connections,
            accept_thread: Some(accept_thread),
        }
    }

    fn drop_connections(&self, outage: Duration) {
        self.paused.store(true, Ordering::SeqCst);
        self.close_live_connections();
        std::thread::sleep(outage);
        // 竞态窗口内刚 accept 的连接再关一次。
        self.close_live_connections();
        self.paused.store(false, Ordering::SeqCst);
    }

    fn close_live_connections(&self) {
        let mut live = self
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for stream in live.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.close_live_connections();
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

/// 本地 Release 发布夹具：提供 `deepcore-manifest.json` 与一个 tar.gz。
///
/// 故意走真实 HTTP + 真实 tar 包，而不是在测试里直接调用安装函数；这样可以
/// 覆盖 manifest 下载、SHA-256、归档解包、SFTP 上传与远端探针整条链路。
struct ReleaseServer {
    port: u16,
    sha: Arc<Mutex<(String, bool)>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ReleaseServer {
    fn start(artifact_path: &std::path::Path) -> Self {
        let artifact = Arc::new(std::fs::read(artifact_path).expect("read core artifact"));
        let sha = Arc::new(Mutex::new((
            format!("{:x}", Sha256::digest(artifact.as_slice())),
            false,
        )));
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind release server");
        listener
            .set_nonblocking(true)
            .expect("make release server nonblocking");
        let port = listener
            .local_addr()
            .expect("release server address")
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let artifact = artifact.clone();
            let sha = sha.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_nonblocking(false)
                                .expect("make release connection blocking");
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let mut request = Vec::new();
                            let mut chunk = [0u8; 1024];
                            while request.len() < 8192 {
                                match stream.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        request.extend_from_slice(&chunk[..n]);
                                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            let path = String::from_utf8_lossy(&request)
                                .lines()
                                .next()
                                .and_then(|line| line.split_whitespace().nth(1))
                                .unwrap_or("/")
                                .to_string();
                            let (status, content_type, body) = match path.as_str() {
                                "/deepcore-manifest.json" => {
                                    let (sha, corrupt) = {
                                        let state = sha
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                                        state.clone()
                                    };
                                    let sha = if corrupt { "0".repeat(64) } else { sha };
                                    let manifest = json!({
                                        "version": env!("CARGO_PKG_VERSION"),
                                        "targets": {
                                            "linux-x86_64": {
                                                "url": format!("http://127.0.0.1:{port}/deepcore-linux-x86_64.tar.gz"),
                                                "sha256": sha,
                                            }
                                        }
                                    });
                                    (
                                        "200 OK",
                                        "application/json",
                                        serde_json::to_vec(&manifest).unwrap(),
                                    )
                                }
                                "/deepcore-linux-x86_64.tar.gz" => {
                                    ("200 OK", "application/gzip", artifact.as_ref().clone())
                                }
                                _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
                            };
                            let header = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(&body);
                            let _ = stream.flush();
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            port,
            sha,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn set_corrupt_sha(&self, corrupt: bool) {
        self.sha
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = corrupt;
    }
}

impl Drop for ReleaseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn call(client: &Client, request: Value) -> Value {
    client
        .call_timeout(request, Duration::from_secs(25))
        .expect("daemon call")
}

fn wait_until_connected(client: &Client, machine: &str) {
    // 首次连接可能要上传十几 MB 的 core；慢速 SSH/SFTP 上 30 秒会误判。
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = call(client, json!({"op": "machine.list"}));
        if last["machines"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["name"].as_str() == Some(machine)
                    && item["state"].as_str() == Some("connected")
            })
        }) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("machine {machine} did not connect: {last}");
}

fn wait_until_linger(client: &Client, machine: &str, user: &str) {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let list = call(client, json!({"op": "machine.list"}));
        if list["ok"] == true
            && let Some(row) = list["machines"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["name"] == machine))
        {
            let keeps_running = row.get("keeps_running_after_logout");
            let linger_user = row.get("logout_linger_user");
            if keeps_running.is_some_and(|value| !value.is_null())
                && linger_user.as_ref().is_some_and(|value| !value.is_null())
            {
                assert_eq!(
                    keeps_running,
                    Some(&Value::Bool(true)),
                    "远端用户必须能活过 SSH 登出（loginctl enable-linger）: {row}"
                );
                assert_eq!(linger_user.and_then(Value::as_str), Some(user), "{row}");
                return;
            }
            last = row.clone();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("machine {machine} never reported a confirmed linger state: {last}");
}

fn machine_generation(client: &Client, machine: &str) -> u64 {
    let list = call(client, json!({"op": "machine.list"}));
    list["machines"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["name"] == machine))
        .and_then(|item| item["generation"].as_u64())
        .unwrap_or_else(|| panic!("machine {machine} has no generation: {list}"))
}

fn wait_until_generation(client: &Client, machine: &str, previous: u64) {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = call(client, json!({"op": "machine.list"}));
        if last["machines"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["name"].as_str() == Some(machine)
                    && item["state"].as_str() == Some("connected")
                    && item["generation"].as_u64().is_some_and(|g| g > previous)
            })
        }) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("machine {machine} did not reconnect at a newer generation: {last}");
}

fn wait_until_state(
    client: &Client,
    machine: &str,
    state: &str,
    why_contains: Option<&str>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = call(client, json!({"op": "machine.list"}));
        if last["machines"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["name"].as_str() == Some(machine)
                    && item["state"].as_str() == Some(state)
                    && why_contains.is_none_or(|needle| {
                        item["why"].as_str().is_some_and(|why| why.contains(needle))
                    })
            })
        }) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("machine {machine} never reached state {state}: {last}");
}

fn attach_and_read(client: &Client, address: &str, history: usize) -> Vec<u8> {
    let attached = call(
        client,
        json!({
            "op": "attach",
            "address": address,
            "history": history,
            "cols": 80,
            "rows": 24,
        }),
    );
    assert_eq!(attached["ok"], true, "remote attach failed: {attached}");
    let channel = attached["channel"].as_u64().expect("attach channel") as u32;
    let rx = client.attach_channel(channel);
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(frame) => output.extend_from_slice(&frame.payload),
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(async_channel::TryRecvError::Closed) => break,
        }
        if output
            .windows(b"REMOTE_E2E:".len())
            .any(|w| w == b"REMOTE_E2E:")
        {
            break;
        }
    }
    let _ = client.call_timeout(
        json!({"op": "detach", "channel": channel}),
        Duration::from_secs(5),
    );
    client.detach_channel(channel);
    output
}

fn terminate_local_daemons(runtime: &std::path::Path) {
    for name in ["daemon.pid", "holder.pid"] {
        let Ok(text) = std::fs::read_to_string(runtime.join(name)) else {
            continue;
        };
        let Ok(pid) = text.trim().parse::<libc::pid_t>() else {
            continue;
        };
        // SAFETY：只结束本测试在隔离运行时目录中启动的进程。
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    std::thread::sleep(Duration::from_millis(200));
}

fn nonce() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

/// 用系统 `ssh-keyscan` 为隔离 HOME 准备 host key。
///
/// 这是测试夹具，不是产品连接路径；实际 SSH 握手仍由 `russh` 完成并通过
/// 产品自己的 known_hosts 校验。macOS 对未签名 CLI 有本地网络限制，真实
/// 运行时可把 `DEEPTERM_TEST_SSH_HOST/PORT` 指向一条系统 `ssh -L` 隧道。
fn seed_known_hosts(home: &std::path::Path, host: &str, port: u16) {
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir).expect("create .ssh");
    let output = std::process::Command::new("ssh-keyscan")
        .args(["-p", &port.to_string(), host])
        .output()
        .expect("run ssh-keyscan");
    assert!(
        !output.stdout.is_empty(),
        "ssh-keyscan produced no keys (status {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(ssh_dir.join("known_hosts"), output.stdout).expect("write known_hosts");
}

fn cleanup_remote_session(host: &str, port: u16, user: &str, session: &str) {
    let directory = format!("$HOME/.local/share/deepterm/sessions/{session}");
    let script = format!(
        "for f in {directory}/daemon.pid {directory}/holder.pid; do \
         [ -r \"$f\" ] && kill \"$(cat \"$f\")\" 2>/dev/null || true; done; \
         sleep 0.2; rm -rf \"{directory}\""
    );
    let _ = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-p",
            &port.to_string(),
            &format!("{user}@{host}"),
            &script,
        ])
        .status();
}

fn remote_pid(host: &str, port: u16, user: &str, session: &str, file: &str) -> Option<u32> {
    let directory = format!("$HOME/.local/share/deepterm/sessions/{session}");
    let output = ssh_remote(host, port, user, &format!("cat \"{directory}/{file}\""));
    output
        .stdout
        .split(|byte| byte.is_ascii_whitespace())
        .find(|part| !part.is_empty())
        .and_then(|part| std::str::from_utf8(part).ok())
        .and_then(|text| text.parse().ok())
}

fn remote_core_version(host: &str, port: u16, user: &str, session: &str) -> Option<String> {
    let directory = format!("$HOME/.local/share/deepterm/sessions/{session}");
    let output = ssh_remote(
        host,
        port,
        user,
        &format!("test -x \"{directory}/deepcore\" && \"{directory}/deepcore\" --version"),
    );
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .strip_prefix("deepcore ")
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
}

fn remote_core_sha256(host: &str, port: u16, user: &str, session: &str) -> Option<String> {
    let directory = format!("$HOME/.local/share/deepterm/sessions/{session}");
    let output = ssh_remote(
        host,
        port,
        user,
        &format!("sha256sum \"{directory}/deepcore\""),
    );
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

fn kill_remote_core(host: &str, port: u16, user: &str, session: &str, pid: u32) {
    let directory = format!("$HOME/.local/share/deepterm/sessions/{session}");
    let output = ssh_remote(
        host,
        port,
        user,
        &format!("test \"$(cat \"{directory}/daemon.pid\")\" = \"{pid}\" && kill \"{pid}\""),
    );
    assert!(
        output.status.success(),
        "failed to stop remote core {pid}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn ssh_remote(host: &str, port: u16, user: &str, script: &str) -> std::process::Output {
    std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-p",
            &port.to_string(),
            &format!("{user}@{host}"),
            script,
        ])
        .output()
        .expect("run system ssh")
}
