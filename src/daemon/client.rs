//! 客户端：连接守护进程，必要时自动拉起。
//!
//! UI 进程只通过这里与守护进程打交道。所有"发现没连上 → 拉起 → 重试"
//! 的逻辑都收在这一个文件里，调用方拿到的永远是"连上了"或一个明确的错误。

use std::io::BufWriter;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::protocol::{CONTROL, Frame, Kind, read_frame};

/// 拉起守护进程后最多等多久。
///
/// 首次启动要创建 PTY、加载 shell 集成脚本；3 秒足够，超时说明真的出问题了。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

/// 拉起后重试连接的间隔。
const CONNECT_RETRY: Duration = Duration::from_millis(50);

/// 一条与守护进程的连接。
///
/// **整个应用共用一条**：每条连接在守护进程侧要占两个线程，而 UI 上可能有
/// 几十个终端。共用一条连接后，各路终端的数据由 reader 线程按 channel
/// 分发给各自的接收端。
pub struct Client {
    /// 往守护进程写帧。
    writer: std::sync::Mutex<BufWriter<std::os::unix::net::UnixStream>>,
    /// 控制面的 JSON 回复。
    replies: async_channel::Receiver<Frame>,
    /// 各终端 channel 的输出接收端。
    ///
    /// 挂接（attach）时在这里登记一个新 channel；reader 线程看到属于它的
    /// 数据帧就投进对应的 `Sender`。没有登记的输出直接丢弃（那是别的
    /// 连接留下的，或已经 detach 的）。
    routes: Arc<std::sync::Mutex<std::collections::HashMap<u32, async_channel::Sender<Frame>>>>,
    /// reader 线程是否已退出（守护进程没了）。
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
}

impl Client {
    /// 连接守护进程；没在跑就拉起来。
    pub fn connect_or_start() -> std::io::Result<Self> {
        let path = super::socket_path();
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) {
            return Self::from_stream(stream);
        }
        spawn_daemon()?;

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let mut last_error = None;
        while Instant::now() < deadline {
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(stream) => return Self::from_stream(stream),
                Err(e) => {
                    last_error = Some(e);
                    std::thread::sleep(CONNECT_RETRY);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "守护进程未能在预期时间内启动")
        }))
    }

    /// 只连已有的守护进程，不拉起。
    ///
    /// 给"关窗口时顺带检查一下"这类场景用：不该因为一次探测就起一个守护进程。
    pub fn connect_existing() -> std::io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(super::socket_path())?;
        Self::from_stream(stream)
    }

    fn from_stream(stream: std::os::unix::net::UnixStream) -> std::io::Result<Self> {
        let read_stream = stream.try_clone()?;
        let (reply_tx, reply_rx) = async_channel::unbounded::<Frame>();
        let routes: Arc<
            std::sync::Mutex<std::collections::HashMap<u32, async_channel::Sender<Frame>>>,
        > = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_flag = closed.clone();
        let reader_routes = routes.clone();

        std::thread::Builder::new()
            .name("daemon-client-read".into())
            .spawn(move || {
                let mut reader = read_stream;
                loop {
                    match read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            if frame.channel == CONTROL {
                                if reply_tx.try_send(frame).is_err() {
                                    break;
                                }
                                continue;
                            }
                            // 终端输出：交给登记过这个 channel 的接收端。
                            let channel = frame.channel;
                            let sender = reader_routes
                                .lock()
                                .expect("client routes")
                                .get(&channel)
                                .cloned();
                            match sender {
                                Some(tx) => {
                                    if tx.try_send(frame).is_err() {
                                        // 接收端没了：删掉登记，免得一直投递。
                                        reader_routes
                                            .lock()
                                            .expect("client routes")
                                            .remove(&channel);
                                    }
                                }
                                // 没有登记的 channel：这是已经 detach 的终端
                                // 残留的输出，丢掉即可。
                                None => {}
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log::debug!("与守护进程的连接断开: {e}");
                            break;
                        }
                    }
                }
                closed_flag.store(true, Ordering::SeqCst);
            })?;

        Ok(Self {
            writer: std::sync::Mutex::new(BufWriter::new(stream)),
            replies: reply_rx,
            routes,
            closed,
            next_id: AtomicU64::new(1),
        })
    }

    /// 连接是否已经断了。
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 登记一个终端 channel，返回它的输出接收端。
    ///
    /// 每个 `attach` 拿到的 channel 对应一个接收端。接收端被丢弃时
    /// reader 线程会自动注销该 channel。
    pub fn attach_channel(&self, channel: u32) -> async_channel::Receiver<Frame> {
        let (tx, rx) = async_channel::unbounded();
        self.routes
            .lock()
            .expect("client routes")
            .insert(channel, tx);
        rx
    }

    /// 注销一个 channel。
    pub fn detach_channel(&self, channel: u32) {
        self.routes.lock().expect("client routes").remove(&channel);
    }

    /// 发一个控制请求并等回复。
    ///
    /// 超时是必需的：守护进程如果卡住（比如某个终端在 uninterruptible sleep
    /// 里），UI 线程不能跟着一起卡死。
    pub fn call(&self, request: Value) -> std::io::Result<Value> {
        self.call_timeout(request, Duration::from_secs(10))
    }

    pub fn call_timeout(&self, request: Value, timeout: Duration) -> std::io::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = request;
        if let Some(obj) = request.as_object_mut() {
            obj.insert("id".into(), json!(id));
        }
        let bytes = serde_json::to_vec(&request)?;
        self.send(Frame::json(bytes))?;
        self.await_reply(id, timeout)
    }

    /// 等一条指定 id 的控制回复。
    ///
    /// # 为什么不能直接用 `recv_blocking`
    ///
    /// 它会一直阻塞到有消息为止，于是"超时"这条分支永远走不到——守护
    /// 进程一旦不回，调用方（常常是 UI 线程）就永久卡死。而这里又必须
    /// 用阻塞式的收，因为 `call_timeout` 是同步接口（GPUI 的很多初始化
    /// 路径拿不到 async 上下文）。
    ///
    /// 折中办法：另起一条线程做阻塞收，主线程带超时地等它。多一条线程
    /// 的代价可以接受——只有 `call` 期间才存在，且控制面调用是低频的
    /// （开终端、挂接、列终端）。
    fn await_reply(&self, id: u64, timeout: Duration) -> std::io::Result<Value> {
        let (tx, rx) = std::sync::mpsc::channel::<Option<Value>>();
        let replies = self.replies.clone();
        std::thread::Builder::new()
            .name("daemon-call".into())
            .spawn(move || {
                loop {
                    match replies.recv_blocking() {
                        Ok(frame) => {
                            if frame.channel != CONTROL || frame.kind != Kind::Json {
                                continue;
                            }
                            let reply: Value =
                                serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                            if reply.get("id").and_then(Value::as_u64) == Some(id) {
                                let _ = tx.send(Some(reply));
                                return;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(None);
                            return;
                        }
                    }
                }
            })?;

        match rx.recv_timeout(timeout) {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "守护进程连接已断开",
            )),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "守护进程没有回复",
            )),
        }
    }

    /// 发一帧。
    pub fn send(&self, frame: Frame) -> std::io::Result<()> {
        let mut writer = self.writer.lock().expect("client writer");
        frame.write_to(&mut *writer)
    }

    /// 向某个终端的 channel 写输入。
    pub fn input(&self, channel: u32, bytes: Vec<u8>) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.send(Frame::data(channel, bytes))
    }

    /// 通知某个终端的尺寸。
    pub fn resize(
        &self,
        channel: u32,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> std::io::Result<()> {
        self.send(Frame::new(
            channel,
            Kind::Resize,
            Frame::encode_resize(cols, rows, cell_width, cell_height),
        ))
    }

    /// 补发流控信用。
    pub fn credit(&self, channel: u32, bytes: u32) -> std::io::Result<()> {
        self.send(Frame::new(
            channel,
            Kind::Credit,
            bytes.to_le_bytes().to_vec(),
        ))
    }

    /// 让出这条连接（关闭 socket，reader 线程随之退出）。
    pub fn close(&self) {
        let _ = self.send(Frame::new(CONTROL, Kind::Close, Vec::new()));
    }
}

/// 后台拉起守护进程。
/// 守护进程的可执行文件名。
pub const DAEMON_BIN: &str = "deepcore";

/// 拉起守护进程。
///
/// # 怎么找到 `deepcore`
///
/// 1. `$DEEPTERM_DEEPCORE`（测试与自定义安装）；
/// 2. 与当前可执行文件**同目录**——发布包里两个二进制并排放，
///    这一条覆盖绝大多数情况；
/// 3. `PATH`。
///
/// 不硬编码安装路径：开发时是 `target/debug/`，打包后可能是
/// `/usr/lib/deepterm/` 或 `DeepTerm.app/Contents/MacOS/`。
fn spawn_daemon() -> std::io::Result<()> {
    let exe = daemon_binary_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "找不到 {DAEMON_BIN}（应与 {} 在同一目录，或在 PATH 里）",
                DAEMON_BIN
            ),
        )
    })?;
    log::info!("拉起守护进程: {}", exe.display());
    let mut command = std::process::Command::new(&exe);
    // Windows 上别弹控制台窗口。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// 定位 `deepcore` 可执行文件。
///
/// 抽成独立函数是为了可测：`spawn_daemon` 没法在测试里安全调用
/// （真会起进程），而"找不找得到"恰恰是最容易因为打包布局变化而坏掉的地方。
pub fn daemon_binary_path() -> Option<std::path::PathBuf> {
    if let Some(configured) = std::env::var_os(DAEMON_ENV).filter(|v| !v.is_empty()) {
        let path = std::path::PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }

    let name = format!("{DAEMON_BIN}{}", std::env::consts::EXE_SUFFIX);
    // 与 GUI 同目录（发布包的常态）。
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join(&name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    // PATH 兜底。
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or(None)
}

/// 指定 `deepcore` 路径的环境变量（测试与自定义安装）。
pub const DAEMON_ENV: &str = "DEEPTERM_DEEPCORE";

#[cfg(test)]
mod tests {
    use super::*;

    /// 只连不拉起：没有守护进程时必须立刻返回错误，而不是卡住或起一个。
    ///
    /// 这条同时保护测试隔离——如果 `connect_existing` 会拉起守护进程，
    /// 并行跑的测试会互相抢 socket。
    #[test]
    fn connecting_without_a_daemon_fails_fast() {
        // SAFETY：本用例独占该变量（测试隔离目录）。
        unsafe { std::env::set_var(super::super::RUNTIME_DIR_ENV, "/tmp/deepterm-no-daemon") };
        let start = Instant::now();
        let result = Client::connect_existing();
        assert!(result.is_err(), "没有守护进程时应失败");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "必须快速失败，实际耗时 {:?}",
            start.elapsed()
        );
        unsafe { std::env::remove_var(super::super::RUNTIME_DIR_ENV) };
    }

    /// 配置了 `$DEEPTERM_DEEPCORE` 就必须用它。
    ///
    /// 这条保护测试隔离与自定义安装：没有它，`daemon_binary_path` 会退到
    /// "同目录找 deepcore"，而 `cargo test` 的产物目录里确实有那个文件，
    /// 于是一次误调用就真的把用户的守护进程拉起来了。
    #[test]
    fn the_configured_daemon_binary_wins() {
        let dir = std::env::temp_dir().join(format!("deepcore-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let fake = dir.join(format!("{DAEMON_BIN}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(
            &fake,
            b"#!/bin/sh
",
        )
        .expect("write");

        // SAFETY：本用例独占该变量。
        unsafe { std::env::set_var(DAEMON_ENV, &fake) };
        assert_eq!(daemon_binary_path().as_deref(), Some(fake.as_path()));
        unsafe { std::env::remove_var(DAEMON_ENV) };

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 配置指向不存在的文件时要忽略它，而不是返回一个坏路径。
    #[test]
    fn a_bogus_configured_path_is_ignored() {
        // SAFETY：本用例独占该变量。
        unsafe { std::env::set_var(DAEMON_ENV, "/nonexistent/deepcore-xyz") };
        let resolved = daemon_binary_path();
        assert_ne!(
            resolved.as_deref(),
            Some(std::path::Path::new("/nonexistent/deepcore-xyz")),
            "不存在的路径不该被采用"
        );
        unsafe { std::env::remove_var(DAEMON_ENV) };
    }

    /// 启动超时必须是个合理的值：太短会让慢机器上的守护进程起不来。
    #[test]
    fn the_startup_timeout_is_generous_enough_for_a_cold_start() {
        assert!(
            STARTUP_TIMEOUT >= Duration::from_secs(2),
            "冷启动超时太短: {STARTUP_TIMEOUT:?}"
        );
    }
}
