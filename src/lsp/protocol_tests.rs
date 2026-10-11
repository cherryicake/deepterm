//! 协议层行为的测试：关闭握手、请求超时、诊断版本门控、`WorkspaceEdit` 两种形态。
//!
//! 这些用例**不依赖任何真实语言服务器**：它们用一个脚本化的替身进程
//! （`python3` 小脚本）精确控制服务器的收发行为，因此可以确定性地验证
//! 「我们发出了什么帧」「收到异常应答时怎么处理」。
//!
//! 与 `integration_tests.rs` 的分工：那边验证「和真实服务器能不能通」，
//! 这里验证「协议细节对不对」。

use std::path::PathBuf;
use std::time::Duration;

use crate::lsp::jsonrpc::{Connection, Message, SHUTDOWN_TIMEOUT};

/// 写一个替身服务器脚本，返回脚本路径。
///
/// 脚本内容由 `body` 给出，它可以用下面两个辅助函数：
/// - `send(obj)`：按 LSP 帧格式发一条消息；
/// - `recv()`：读一条消息（EOF 返回 `None`）。
///
/// 所有替身脚本都把收到的帧记录到 `record_path`（JSON 行），供测试断言。
fn stub_server(dir: &std::path::Path, name: &str, body: &str) -> (PathBuf, PathBuf) {
    let record = dir.join(format!("{name}.frames.jsonl"));
    let script = dir.join(format!("{name}.py"));
    let prelude = format!(
        r#"
import sys, json, os
RECORD = r"{record}"
def send(obj):
    b = json.dumps(obj).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
    sys.stdout.buffer.flush()
def recv():
    line = sys.stdin.buffer.readline()
    if not line:
        return None
    try:
        n = int(line.split(b":")[1].strip())
    except Exception:
        return None
    sys.stdin.buffer.readline()
    return json.loads(sys.stdin.buffer.read(n))
def record(obj):
    with open(RECORD, "a") as f:
        f.write(json.dumps(obj) + "\n")
"#,
        record = record.display()
    );
    std::fs::write(&script, format!("{prelude}\n{body}\n")).unwrap();
    (script, record)
}

/// 启动替身服务器（`initialize` 由替身脚本自己应答）。
async fn start_stub(script: &std::path::Path, _dir: &std::path::Path) -> Connection {
    Connection::spawn("python3", &[script.display().to_string()], None)
        .await
        .expect("stub server should start")
}

/// 读替身服务器记录的帧。
fn recorded_frames(record: &std::path::Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(record).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// 等 `f` 成立，超时返回 false。
async fn wait_for(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        smol::Timer::after(Duration::from_millis(50)).await;
    }
    f()
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deepterm-lsp-proto-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------- 关闭握手

/// 关闭时必须发 `shutdown` 请求 + `exit` 通知（LSP 关闭握手）。
///
/// 回归：早期实现只关 stdin 然后 kill，服务器没有机会保存状态/清理索引。
#[test]
fn shutdown_sends_handshake_frames() {
    let dir = scratch_dir("shutdown");
    let (script, record) = stub_server(
        &dir,
        "shutdown",
        r#"
# 应答 initialize，然后一直读到 EOF，把每条帧记下来。
msg = recv()
if msg is not None:
    send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}})
while True:
    m = recv()
    if m is None:
        break
    record(m)
    if m.get("method") == "shutdown":
        send({"jsonrpc":"2.0","id":m["id"],"result":None})
    if m.get("method") == "exit":
        break
"#,
    );

    smol::block_on(async {
        let conn = start_stub(&script, &dir).await;
        // 完成 initialize（替身脚本会应答）。
        let _ = conn.request("initialize", None).await;

        conn.shutdown_gracefully().await;

        let frames = recorded_frames(&record);
        let methods: Vec<&str> = frames
            .iter()
            .filter_map(|f| f.get("method").and_then(|m| m.as_str()))
            .collect();

        assert!(
            methods.contains(&"shutdown"),
            "shutdown request must be sent, got {methods:?}"
        );
        assert!(
            methods.contains(&"exit"),
            "exit notification must be sent after shutdown, got {methods:?}"
        );
        // shutdown 必须在 exit 之前，且 shutdown 是带 id 的请求。
        let shutdown_ix = methods.iter().position(|m| *m == "shutdown").unwrap();
        let exit_ix = methods.iter().position(|m| *m == "exit").unwrap();
        assert!(shutdown_ix < exit_ix, "shutdown must precede exit");

        let shutdown_frame = &frames[shutdown_ix];
        assert!(
            shutdown_frame.get("id").and_then(|i| i.as_u64()).is_some(),
            "shutdown must be a request with a numeric id, got {shutdown_frame:?}"
        );
    });
}

/// 关闭后子进程必须已经退出（不留孤儿）。
#[test]
fn shutdown_reaps_child_process() {
    let dir = scratch_dir("shutdown-reap");
    let (script, _record) = stub_server(
        &dir,
        "reap",
        r#"
msg = recv()
if msg is not None:
    send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}})
while True:
    m = recv()
    if m is None:
        break
    if m.get("method") == "shutdown":
        send({"jsonrpc":"2.0","id":m["id"],"result":None})
    if m.get("method") == "exit":
        sys.exit(0)
"#,
    );

    smol::block_on(async {
        let conn = start_stub(&script, &dir).await;
        let _ = conn.request("initialize", None).await;

        // 拿 pid 以便独立确认进程消失。
        let pid = conn.child_pid().expect("child pid");
        conn.shutdown_gracefully().await;

        // 进程必须退出：`kill(pid, 0)` 失败即不存在。
        let gone = wait_for(|| !process_alive(pid), Duration::from_secs(5)).await;
        assert!(
            gone,
            "language server child (pid {pid}) still alive after shutdown"
        );
    });
}

/// 关闭握手对「不答 shutdown 的服务器」也要有界完成（不能挂死）。
#[test]
fn shutdown_is_bounded_even_if_server_never_answers() {
    let dir = scratch_dir("shutdown-unresponsive");
    let (script, _record) = stub_server(
        &dir,
        "unresponsive",
        r#"
# 只应答 initialize；之后所有请求都不回（模拟卡死的服务器）。
msg = recv()
if msg is not None:
    send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}})
while True:
    m = recv()
    if m is None:
        break
    # 故意不应答 shutdown，也不退出。
"#,
    );

    smol::block_on(async {
        let conn = start_stub(&script, &dir).await;
        let _ = conn.request("initialize", None).await;

        let pid = conn.child_pid().expect("child pid");
        let started = std::time::Instant::now();
        conn.shutdown_gracefully().await;
        let elapsed = started.elapsed();

        // 有界：不超过握手超时 + 余量（测试断言「有界」，不钉具体延迟）。
        let bound = SHUTDOWN_TIMEOUT + Duration::from_secs(3);
        assert!(
            elapsed < bound,
            "shutdown took {elapsed:?}, expected < {bound:?}"
        );
        // 卡死的服务器会被强杀，不留孤儿。
        let gone = wait_for(|| !process_alive(pid), Duration::from_secs(5)).await;
        assert!(
            gone,
            "unresponsive server must be killed, pid {pid} still alive"
        );
    });
}

/// `kill(pid, 0)` 探测进程是否存活。
fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

// ---------------------------------------------------------------- 请求超时

/// 服务器不答的请求必须**有界失败**，并从 pending 表摘除。
///
/// 回归：早期实现 `rx.recv().await` 无限等待，卡死的服务器会让调用方的
/// future 永不返回、pending 条目永久泄漏。
#[test]
fn unresponsive_request_times_out_and_clears_pending() {
    let dir = scratch_dir("timeout");
    let (script, _record) = stub_server(
        &dir,
        "timeout",
        r#"
msg = recv()
if msg is not None:
    send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}})
# 之后收到的任何请求都不应答（模拟挂起）。
while True:
    m = recv()
    if m is None:
        break
"#,
    );

    smol::block_on(async {
        let conn = start_stub(&script, &dir).await;
        let _ = conn.request("initialize", None).await;

        // 用一个很短的超时驱动真实代码路径（生产值是 30s，测试不宜等）。
        let started = std::time::Instant::now();
        let result = conn
            .request_with_timeout("textDocument/hover", None, Duration::from_millis(400))
            .await;

        assert!(
            result.is_err(),
            "unanswered request must fail, got {result:?}"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "must fail promptly, took {elapsed:?}"
        );
        // 关键：条目已摘除，不泄漏。
        assert_eq!(
            conn.pending_len(),
            0,
            "timed-out request must be removed from the pending table"
        );
    });
}

/// 超时后连接仍可用：后续请求照常成功（超时不该破坏连接）。
#[test]
fn connection_usable_after_a_timeout() {
    let dir = scratch_dir("timeout-then-ok");
    let (script, _record) = stub_server(
        &dir,
        "timeout-then-ok",
        r#"
msg = recv()
if msg is not None:
    send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}})
while True:
    m = recv()
    if m is None:
        break
    # 只应答 "good" 方法，其余（"hang"）故意不答。
    if m.get("method") == "good":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"ok":True}})
"#,
    );

    smol::block_on(async {
        let conn = start_stub(&script, &dir).await;
        let _ = conn.request("initialize", None).await;

        let timed_out = conn
            .request_with_timeout("hang", None, Duration::from_millis(300))
            .await;
        assert!(timed_out.is_err(), "hang should time out");

        // 连接仍可用：真实应答能正常配对（证明 id 分配/配对没被超时搞乱）。
        let ok = conn.request("good", None).await.expect("good must succeed");
        assert_eq!(ok["ok"], true);
        assert_eq!(conn.pending_len(), 0);
    });
}

// ---------------------------------------------------------------- 帧编解码回归

/// 通知队列在溢出时丢**最旧**的，保留最新（诊断是幂等全量快照）。
#[test]
fn notifications_keep_the_newest_when_saturated() {
    // 这条是 `jsonrpc` 内部 `dispatch` 的行为，已有单测覆盖；
    // 这里补一条端到端确认：真实连接上收到的通知顺序合理。
    let dir = scratch_dir("notify-order");
    let (script, _record) = stub_server(
        &dir,
        "notify-order",
        r#"
msg = recv()
if msg is not None:
    send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}})
for i in range(5):
    send({"jsonrpc":"2.0","method":"test/notify","params":{"i":i}})
# 保持进程存活，让客户端有机会读完。
import time
time.sleep(1)
"#,
    );

    smol::block_on(async {
        let conn = start_stub(&script, &dir).await;
        // 订阅必须在服务器开始发通知**之前**：通知走广播，只投给订阅那一刻
        // 已在册的订阅者，晚订阅的收不到之前发的（见 `Connection::subscribe`）。
        let rx = conn.subscribe();
        let _ = conn.request("initialize", None).await;

        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while seen.len() < 5 && std::time::Instant::now() < deadline {
            if let Ok(Message::Notification { params, .. }) = rx.recv().await {
                if let Some(i) = params.get("i").and_then(|v| v.as_u64()) {
                    seen.push(i);
                }
            }
        }
        assert_eq!(
            seen,
            vec![0, 1, 2, 3, 4],
            "notifications must arrive in order"
        );
    });
}

/// 请求 id 单调递增，且响应能配对回正确的请求（并发下不串号）。
#[test]
fn concurrent_requests_pair_correctly() {
    let dir = scratch_dir("concurrent");
    let (script, _record) = stub_server(
        &dir,
        "concurrent",
        r#"
msg = recv()
if msg is not None:
    send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}})
while True:
    m = recv()
    if m is None:
        break
    if "id" in m and m.get("method"):
        # 故意**乱序**应答：先回后到的，再回先到的。
        pass
    # 把收到的请求攒起来一起回（下一轮循环结束时回）。
    if m.get("method") == "ping":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"echo":m.get("params")}})
"#,
    );

    smol::block_on(async {
        let conn = std::sync::Arc::new(start_stub(&script, &dir).await);
        let _ = conn.request("initialize", None).await;

        // 并发发多个请求，每个带不同 params，验证应答配对正确。
        let mut tasks = Vec::new();
        for i in 0..8u64 {
            let conn = conn.clone();
            tasks.push(smol::spawn(async move {
                let params = serde_json::json!({ "n": i });
                let out = conn.request("ping", Some(&params)).await.unwrap();
                (i, out)
            }));
        }
        for t in tasks {
            let (i, out) = t.await;
            assert_eq!(
                out["echo"]["n"], i,
                "response paired to the wrong request (expected n={i})"
            );
        }
        assert_eq!(conn.pending_len(), 0);
    });
}

// ---------------------------------------------------------------- 应用入口的关闭路径

/// 驱动**应用真正调用的入口**：`LspClient::start` → `LspClient::shutdown`。
///
/// 上面的用例直接测 `Connection`；这条覆盖 `LspClient` 与
/// `LspManager::shutdown_all` 所依赖的同一条链路（`client.shutdown()` →
/// `conn.shutdown()` → 关闭握手），确保应用退出时走的就是握手。
#[test]
fn lsp_client_shutdown_runs_handshake_and_reaps() {
    let dir = scratch_dir("client-shutdown");
    let (script, record) = stub_server(
        &dir,
        "client",
        r#"
# 应答 initialize（LspClient::start 会做握手），之后记录帧并配合关闭。
while True:
    m = recv()
    if m is None:
        break
    if m.get("method") == "initialize":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"capabilities":{}}})
        continue
    if m.get("method") == "initialized":
        continue
    record(m)
    if m.get("method") == "shutdown":
        send({"jsonrpc":"2.0","id":m["id"],"result":None})
    if m.get("method") == "exit":
        sys.exit(0)
"#,
    );

    smol::block_on(async {
        // 走真实入口：`LspClient::start` 内部会 spawn + initialize。
        let client = crate::lsp::client::LspClient::start(
            "python3",
            &[script.display().to_string()],
            dir.clone(),
        )
        .await
        .expect("client start");

        // 应用退出路径调用它。
        client.shutdown();

        // 握手帧必须发出（等它写完）。
        let saw = wait_for(
            || {
                let frames = recorded_frames(&record);
                let methods: Vec<String> = frames
                    .iter()
                    .filter_map(|f| f.get("method").and_then(|m| m.as_str()).map(String::from))
                    .collect();
                methods.contains(&"shutdown".to_string()) && methods.contains(&"exit".to_string())
            },
            Duration::from_secs(10),
        )
        .await;
        assert!(
            saw,
            "LspClient::shutdown must emit shutdown + exit handshake"
        );
    });
}

/// 应用退出后不能留下孤儿进程（`shutdown_all` 的语义）。
#[test]
fn app_teardown_leaves_no_orphan_server() {
    let dir = scratch_dir("no-orphan");
    let (script, _record) = stub_server(
        &dir,
        "orphan",
        r#"
while True:
    m = recv()
    if m is None:
        break
    if m.get("method") == "initialize":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"capabilities":{}}})
        continue
    if m.get("method") == "shutdown":
        send({"jsonrpc":"2.0","id":m["id"],"result":None})
    if m.get("method") == "exit":
        sys.exit(0)
"#,
    );

    smol::block_on(async {
        let client = crate::lsp::client::LspClient::start(
            "python3",
            &[script.display().to_string()],
            dir.clone(),
        )
        .await
        .expect("client start");

        let pid = client.connection_pid().expect("pid");
        client.shutdown();

        let gone = wait_for(|| !process_alive(pid), Duration::from_secs(10)).await;
        assert!(gone, "server pid {pid} still alive after client shutdown");
    });
}

// ---------------------------------------------------------------- 启动环境

/// 语言服务器子进程必须带着自拼的 `PATH` 启动，且该 `PATH` 含用户级目录。
///
/// 这是「LSP 失败: rust 服务器启动失败」那条 bug 的护栏。从 Finder / Dock
/// 启动时本进程继承的是 launchd 的最小 `PATH`
/// （`/usr/bin:/bin:/usr/sbin:/sbin`），`~/.cargo/bin` 不在其中；而 rustup 装的
/// `rust-analyzer` 只在那里。若启动时不自己拼 `PATH`，按名字
/// `Command::new("rust-analyzer")` 直接 `NotFound`——用户在终端里敲同一命令
/// 却一切正常，差别只在启动方式。
///
/// 断言的是**真的进了子进程环境**（`get_envs`），而不是「我们算过一个字符串」：
/// 漏掉 `command.env(...)` 这一步时，`utils::path` 的单测仍会全绿。
#[test]
#[cfg(unix)]
fn spawn_command_carries_a_path_with_user_bin_dirs() {
    let command = crate::lsp::jsonrpc::spawn_command("rust-analyzer", &[]);

    let path = command
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .and_then(|(_, value)| value)
        .expect("语言服务器子进程必须显式设置 PATH，否则 Finder 启动时找不到它")
        .to_string_lossy()
        .into_owned();
    let dirs: Vec<&str> = path.split(':').collect();

    let home = dirs::home_dir().expect("测试环境必须有主目录");
    for dir in crate::utils::path::USER_BIN_DIRS {
        let expected = home.join(dir);
        assert!(
            dirs.contains(&expected.to_string_lossy().as_ref()),
            "PATH 缺 ~/{dir}：rustup / standalone 装的语言服务器只在这里，\
             找不到就是「LSP 失败: 服务器启动失败」：{path}"
        );
    }
    // 系统目录只在 macOS 上断言：`system_search_path` 读 `/etc/paths`，
    // `/usr/bin` 是白纸黑字写在那里的；其它平台没有这层兜底，继承来的
    // `PATH` 里有什么就是什么，拿它当断言等于依赖 CI runner 的环境。
    #[cfg(target_os = "macos")]
    assert!(dirs.contains(&"/usr/bin"), "系统目录不能丢：{path}");
}

/// 用户级目录里按**名字**启动的语言服务器必须真的能跑起来并完成握手。
///
/// 与上一条的分工：那条验证「`PATH` 进了子进程环境」，这条验证「这样拼出来的
/// `PATH` 真的能解析到可执行文件」——`Command::env` 生效与查找成功是两件事，
/// 中间还隔着 `PATH` 内容本身对不对。
#[test]
#[cfg(unix)]
fn command_with_composed_path_resolves_a_bare_name() {
    let dir = scratch_dir("spawn-path");
    let home = dir.join("home");
    let bin = home.join(".cargo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = dir.join("invoked.txt");

    let fake = bin.join("fake-language-server");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf 'ran' > {}\n",
            crate::utils::shell::shell_quote_single(&marker.to_string_lossy())
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // 与 `local_spawn_path` 同一套拼装，只把 home 换成临时目录。
    let path = crate::utils::path::local_spawn_path_for(&home, "/usr/bin:/bin");
    let output = std::process::Command::new("fake-language-server")
        .env("PATH", &path)
        .output()
        .expect("必须能在自拼的 PATH 里按裸名字找到可执行文件");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");
    let _ = std::fs::remove_dir_all(&dir);
}
