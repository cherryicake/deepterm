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
pub struct Client {
    /// 往守护进程写帧。
    writer: std::sync::Mutex<BufWriter<std::os::unix::net::UnixStream>>,
    /// 收到的帧。控制回复与终端输出都在这个流里，由调用方按 channel 分发。
    incoming: async_channel::Receiver<Frame>,
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
        let (tx, rx) = async_channel::unbounded::<Frame>();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_flag = closed.clone();

        std::thread::Builder::new()
            .name("daemon-client-read".into())
            .spawn(move || {
                let mut reader = read_stream;
                loop {
                    match read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            if tx.try_send(frame).is_err() {
                                break;
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
            incoming: rx,
            closed,
            next_id: AtomicU64::new(1),
        })
    }

    /// 连接是否已经断了。
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 收到的帧流。
    pub fn incoming(&self) -> &async_channel::Receiver<Frame> {
        &self.incoming
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

        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "守护进程没有回复",
                ));
            }
            match self.incoming.recv_blocking() {
                Ok(frame) => {
                    if frame.channel != CONTROL || frame.kind != Kind::Json {
                        // 不是控制回复（可能是终端输出），丢弃。
                        // 真正的分发由调用方自己的循环负责。
                        continue;
                    }
                    let reply: Value =
                        serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                    if reply.get("id").and_then(Value::as_u64) == Some(id) {
                        return Ok(reply);
                    }
                }
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "守护进程连接已断开",
                    ));
                }
            }
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
fn spawn_daemon() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    log::info!("拉起守护进程: {}", exe.display());
    std::process::Command::new(exe)
        .arg("__daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

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

    /// 启动超时必须是个合理的值：太短会让慢机器上的守护进程起不来。
    #[test]
    fn the_startup_timeout_is_generous_enough_for_a_cold_start() {
        assert!(
            STARTUP_TIMEOUT >= Duration::from_secs(2),
            "冷启动超时太短: {STARTUP_TIMEOUT:?}"
        );
    }
}
