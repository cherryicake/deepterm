//! 客户端：连接守护进程，必要时自动拉起。
//!
//! UI 进程只通过这里与守护进程打交道。所有"发现没连上 → 拉起 → 重试"
//! 的逻辑都收在这一个文件里，调用方拿到的永远是"连上了"或一个明确的错误。

use std::io::{BufReader, BufWriter, Read, Write};
use std::process::Child;
#[cfg(not(test))]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub use super::bootstrap::{
    CONNECT_RETRY, DAEMON_BIN, DAEMON_ENV, STARTUP_TIMEOUT, daemon_binary_path,
};
use super::protocol::{CONTROL, Frame, Kind, read_frame};

/// 一条与守护进程的连接。
///
/// **整个应用共用一条**：每条连接在守护进程侧要占两个线程，而 UI 上可能有
/// 几十个终端。共用一条连接后，各路终端的数据由 reader 线程按 channel
/// 分发给各自的接收端。
pub struct Client {
    /// 往守护进程写帧。
    /// 待写帧的队列。
    ///
    /// **不直接在调用方线程写 socket**：`send` 会在 UI 线程上被调用
    /// （按键、滚轮、resize），而 socket 写是阻塞的。守护进程一旦读得慢
    /// （它那边可能在给一堆客户端推输出），send 就卡在内核缓冲区上，
    /// UI 线程跟着冻住——表现为"滚动一下就卡死"，而且完全看不出跟网络
    /// 有关。
    ///
    /// 改成投递到队列，由专门的写线程负责真正的 write。
    outbox: async_channel::Sender<Frame>,
    /// 控制面回复的分发表：请求 id → 该请求的等待者。
    ///
    /// # 为什么不能用一个共享的 `Receiver`
    ///
    /// 旧实现是「每次 `call` 新起一条线程去抢同一个 `replies`」，抢到之后
    /// **如果不是自己等的 id 就把这条回复丢掉**。多个 `call` 并发时（`attach`
    /// 与 `list`/`get` 在打开窗口时确实会并发），B 会把 A 的回复吃掉并丢弃，
    /// A 于是等到超时——而 `attach` 超时的后果不是"报个错"：它拿不到
    /// `channel`，`pump_into` 就**不会调 `attach_channel` 登记路由**。
    /// 之后 reader 线程收到该 channel 的数据时找不到路由，全丢进 `pending`，
    /// 接收端永远空着 —— 界面就冻在最后一屏。
    ///
    /// 现在只保留一个分发器（reader 线程本身），按 id 把回复投进对应的
    /// 一次性通道；谁的回复归谁，谁也不会吃别人的。
    pending_replies:
        Arc<std::sync::Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<Frame>>>>,
    /// 各终端 channel 的输出接收端。
    ///
    /// 挂接（attach）时在这里登记一个新 channel；reader 线程看到属于它的
    /// 数据帧就投进对应的 `Sender`。没有登记的输出直接丢弃（那是别的
    /// 连接留下的，或已经 detach 的）。
    routes: Arc<std::sync::Mutex<std::collections::HashMap<u32, async_channel::Sender<Frame>>>>,
    /// reader 线程是否已退出（守护进程没了）。
    closed: Arc<AtomicBool>,
    /// 建立连接的时刻，与 `last_rx_ms` 一起计算这条 line 已经静默多久。
    read_since: Instant,
    /// reader 最后一次读到任意帧的时间（相对 `read_since` 的毫秒数）。
    ///
    /// SSH 半开时 `is_closed()` 可能永远为 false，控制请求只会各自超时。
    /// Herdr 在 wire 层记录“最后一次听到真实字节”，这里用同样的判据：
    /// 只要还有终端输出或 status 回复在流动，line 就不算死；完全没有任何
    /// 字节超过上限，supervisor 才主动重拨。
    last_rx_ms: Arc<AtomicU64>,
    /// 路由尚未登记时提前到达的帧（见 `attach_channel`）。
    pending: Arc<std::sync::Mutex<std::collections::HashMap<u32, Pending>>>,
    next_id: AtomicU64,
    /// 由本进程启动的 `deepcore connect` 子进程。
    ///
    /// 直接连 socket（测试与显式兼容路径）时为 `None`。最后一条 `Client`
    /// 释放时 guard 会结束桥进程；core/holder 是独立会话，不受影响。
    _bridge: Option<BridgeProcess>,
}

impl Client {
    /// 连接守护进程；没在跑就拉起来。
    ///
    /// 版本不匹配时会**重启 core**（见 [`restart_stale_core`]）再连一次：core
    /// 随时可以重启，PTY 在 holder 手里，终端毫发无损——这正是两进程模型存在
    /// 的意义。反过来"直接拒绝"会让用户升级后完全失去持久化能力（旧 core 还
    /// 在跑、新 GUI 连不上，正在运行的程序既看不见也接不回来）。
    pub fn connect_or_start() -> std::io::Result<Self> {
        // 单元测试绝不能拉起生产进程：测试直接走 socket 探测路径，通常
        // 立即失败；需要真实协议的测试自行提供隔离的 socket。
        #[cfg(test)]
        {
            return Self::connect_existing();
        }

        #[cfg(not(test))]
        {
            match Self::connect_via_bridge() {
                Ok(client) => Ok(client),
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    log::warn!("守护进程协议过旧，重启 core（keep-alive 由 holder 保证）");
                    let path = super::socket_path();
                    Self::restart_stale_core(&path)?;
                    Self::connect_via_bridge().map_err(|e| {
                        std::io::Error::new(e.kind(), format!("重启后的守护进程仍无法握手: {e}"))
                    })
                }
                Err(e) => Err(e),
            }
        }
    }

    /// 通过 `deepcore connect` 的 stdin/stdout 建立连接。
    #[cfg(not(test))]
    fn connect_via_bridge() -> std::io::Result<Self> {
        let exe = daemon_binary_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "找不到 {DAEMON_BIN}（应与 {} 在同一目录，或在 PATH 里）",
                    DAEMON_BIN
                ),
            )
        })?;

        log::info!("启动 stdio 桥: {} connect", exe.display());
        let mut command = Command::new(exe);
        command.arg(super::connect::CONNECT_COMMAND);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn()?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "deepcore connect 没有 stdin",
            ));
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "deepcore connect 没有 stdout",
            ));
        };
        let client = Self::from_io(stdout, stdin, Some(BridgeProcess::new(child)))?;
        client.handshake()?;
        Ok(client)
    }

    /// 让旧 core 退出，给新版本腾出 socket。
    ///
    /// 只对 **core** 这么做，绝不动 holder：holder 是终端进程的父进程，结束它
    /// 等于把用户正在跑的编译、编辑器一起带走。core 没有 PTY，重启它只影响
    /// "谁在转发"，这正是 `docs/DESIGN_TERMINAL_PERSISTENCE.md` 里"版本对不上
    /// 就重拉"的实现。
    ///
    /// 用 pid 文件而不是按进程名 `pkill`：按名字杀会同时命中 holder（两者是
    /// 同一个二进制），那正是这里最不能发生的事。
    #[cfg(not(test))]
    fn restart_stale_core(socket: &std::path::Path) -> std::io::Result<()> {
        let pid_file = socket.with_extension("pid");
        let pid = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());

        // 先删 socket：即使旧 core 的 SIGTERM 处理慢，新 core 也能立刻 bind
        // （`listen::run_at` 本来就会清理残留，这里提前做是为了少等一轮）。
        let _ = std::fs::remove_file(socket);

        let Some(pid) = pid else {
            log::warn!(
                "没有可用的 pid 文件（{}），直接重新拉起",
                pid_file.display()
            );
            let _ = std::fs::remove_file(&pid_file);
            return Ok(());
        };

        #[cfg(unix)]
        {
            // SAFETY：`kill` 只是向指定 pid 发信号；pid 来自我们自己写的 pid
            // 文件，且这是本地单用户场景。
            unsafe {
                if libc::kill(pid, libc::SIGTERM) != 0 {
                    let err = std::io::Error::last_os_error();
                    log::warn!("结束旧守护进程 {pid} 失败: {err}");
                    return Ok(());
                }
            }
            // 等它真的退出：socket 文件已删，这里看的是进程是否还活着，避免
            // 新旧两个 core 同时往同一个 socket 上写。
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                // SAFETY：信号 0 只做存在性检查，不投递。
                let alive = unsafe { libc::kill(pid, 0) == 0 };
                if !alive {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            log::info!("旧守护进程 {pid} 已退出，准备用新版本重启");
        }
        #[cfg(windows)]
        {
            // Windows 没有 SIGTERM；`taskkill` 是系统自带的可靠方式。
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            log::info!("旧守护进程 {pid} 已结束");
        }

        let _ = std::fs::remove_file(&pid_file);
        Ok(())
    }

    /// 只连已有的守护进程，不拉起。
    ///
    /// 给"关窗口时顺带检查一下"这类场景用：不该因为一次探测就起一个守护进程。
    pub fn connect_existing() -> std::io::Result<Self> {
        let stream = super::transport::connect(&super::socket_path())?;
        Self::from_stream(stream)
    }

    fn from_stream(stream: super::transport::Stream) -> std::io::Result<Self> {
        let read_stream = super::transport::try_clone(&stream)?;
        let client = Self::from_io(read_stream, stream, None)?;
        client.handshake()?;
        Ok(client)
    }

    /// 从一对独立的读/写端建立协议连接。
    ///
    /// socket 路径把同一对象克隆成两半；stdio 路径天然就是两根管道。把
    /// “读写如何取得”留在这里，上面的分帧、路由、握手逻辑完全复用。
    fn from_io(
        read_stream: impl Read + Send + 'static,
        write_stream: impl Write + Send + 'static,
        bridge: Option<BridgeProcess>,
    ) -> std::io::Result<Self> {
        let routes: Arc<
            std::sync::Mutex<std::collections::HashMap<u32, async_channel::Sender<Frame>>>,
        > = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_flag = closed.clone();
        let reader_routes = routes.clone();
        let pending: Arc<std::sync::Mutex<std::collections::HashMap<u32, Pending>>> =
            Arc::default();
        let pending_for_client = pending.clone();
        let pending_replies: Arc<
            std::sync::Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<Frame>>>,
        > = Arc::default();
        let pending_replies_for_client = pending_replies.clone();
        let reader_replies = pending_replies.clone();
        let read_since = Instant::now();
        let last_rx_ms = Arc::new(AtomicU64::new(0));
        let reader_last_rx_ms = last_rx_ms.clone();

        std::thread::Builder::new()
            .name("daemon-client-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(read_stream);
                loop {
                    match read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            reader_last_rx_ms.store(
                                read_since.elapsed().as_millis() as u64,
                                Ordering::Relaxed,
                            );
                            if frame.channel == CONTROL {
                                // 按请求 id 分发：**只喂给等它的那一个 call**。
                                // 见 `pending_replies` 的说明——共享队列会让
                                // 并发的 call 互相吞掉对方的回复，而 attach
                                // 丢掉回复就等于终端再也收不到数据。
                                let id = serde_json::from_slice::<Value>(&frame.payload)
                                    .ok()
                                    .and_then(|v| v.get("id").and_then(Value::as_u64));
                                let waiter = {
                                    let mut replies =
                                        reader_replies.lock().expect("pending replies");
                                    match id {
                                        Some(id) => replies.remove(&id),
                                        // **兼容旧守护进程**：老版本可能不回
                                        // 填 id（例如 11 月 16 日之前的 hello
                                        // 回复）。没有 id 就交给唯一/最早等待
                                        // 者，否则升级后新 GUI 会认不出握手回
                                        // 复，把一条活着的连接判成断线。
                                        None => {
                                            let key = replies.keys().min().copied();
                                            key.and_then(|k| replies.remove(&k))
                                        }
                                    }
                                };
                                if let Some(tx) = waiter {
                                    let _ = tx.send(frame);
                                } else {
                                    // 没有人在等（超时后才到的回复）：丢掉即可，
                                    // 不能因此退出读线程——那会让整条连接作废。
                                    log::debug!("收到无人等待的控制回复，丢弃");
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
                                None => {
                                    // 路由还没登记：**不能丢**。
                                    //
                                    // `attach` 的回复和数据帧走同一条连接，
                                    // 服务端会先把整屏快照推出来、再回 JSON。
                                    // 客户端要等回复解析出 channel 号才调
                                    // `attach_channel` 登记路由，中间这段窗口
                                    // 里的帧（正是那份快照）到这里时查不到。
                                    //
                                    // 与 `core_holder` 侧是同一个问题、同一个
                                    // 修法：先攒着，登记时补发。
                                    let mut p = pending.lock().expect("client pending");
                                    let buf = p.entry(channel).or_default();
                                    if buf.bytes + frame.payload.len() <= MAX_PENDING {
                                        buf.bytes += frame.payload.len();
                                        buf.frames.push(frame);
                                    } else {
                                        // 超限：丢掉**整段**而不是截断。
                                        //
                                        // 截断会留下半截转义序列，后续数据
                                        // 全部错位；而完整丢掉之后，路由一登记
                                        // 就会收到守护进程补发的整屏快照（它
                                        // 本来就会在客户端跟不上时这么做）。
                                        log::warn!(
                                            "channel {channel} 登记前的数据超过 {MAX_PENDING} 字节，丢弃"
                                        );
                                        buf.frames.clear();
                                        buf.bytes = 0;
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log::debug!("与守护进程的连接断开: {e}");
                            break;
                        }
                    }
                }
                // 关键：把各 channel 的发送端清掉，让挂接方的 `recv()`
                // 返回 `Err` 而不是永远阻塞。
                //
                // 只置 `closed` 标志是不够的：泵任务阻塞在 `recv()` 上，
                // 它不会被那个标志唤醒，于是终端静默冻结在最后一屏——
                // 用户看到的是"程序卡住了"，而实际是连接早就断了。
                reader_routes.lock().expect("client routes").clear();
                closed_flag.store(true, Ordering::SeqCst);
            })?;

        // 写线程：独占 socket 的写端，负责把队列里的帧真正发出去。
        //
        // 队列无界：调用方（UI 线程）绝不能因为守护进程慢而被阻塞。真正的
        // 背压由流控（credit）承担——终端数据本来就是按信用发的。
        //
        // **写线程退出必须置 `closed`**。否则连接的读端还活着（socket 半关）
        // 时，`is_closed()` 永远是 false，`session::client()` 会一直复用这条
        // 死连接：所有输入 `try_send` 都失败并打出"写线程已退出"，终端却
        // 既不发重连、也不给用户任何明确信号，只能停在最后一屏。这个 bug
        // 在 core 重启后持续了几分钟，日志里是成百上千条同样的写入失败。
        let (out_tx, out_rx) = async_channel::unbounded::<Frame>();
        {
            let mut buf = BufWriter::new(write_stream);
            let closed_flag = closed.clone();
            let writer_routes = routes.clone();
            std::thread::Builder::new()
                .name("daemon-client-w".into())
                .spawn(move || {
                    while let Ok(frame) = out_rx.recv_blocking() {
                        if frame.write_to(&mut buf).is_err() {
                            break;
                        }
                        // 立刻刷出去：这些是按键/滚轮，延迟会直接影响手感。
                        if buf.flush().is_err() {
                            break;
                        }
                    }
                    let _ = buf.flush();
                    // 与读端同一套收尾：清路由，让挂接方的 `recv()` 立刻
                    // 返回 `Err` 而不是永久阻塞，再置 closed 让上层重连。
                    writer_routes.lock().expect("client routes").clear();
                    closed_flag.store(true, Ordering::SeqCst);
                })
                .ok();
        }
        let client = Self {
            pending: pending_for_client,
            outbox: out_tx,
            pending_replies: pending_replies_for_client,
            routes,
            closed,
            read_since,
            last_rx_ms,
            next_id: AtomicU64::new(1),
            _bridge: bridge,
        };
        Ok(client)
    }

    /// 从任意同步读写端建立协议连接，不自动握手。
    ///
    /// 远端 line 用这个入口：Transport 的另一端是 russh channel，而握手
    /// 由 [`MachineRuntime`](super::machines::MachineRuntime) 按协议检查。
    pub(crate) fn from_transport(
        read_stream: impl Read + Send + 'static,
        write_stream: impl Write + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::from_io(read_stream, write_stream, None)
    }

    /// 建立连接后的鉴权握手。
    ///
    /// 服务端要求第一条帧是带正确 token 的 `hello`，否则直接关连接。
    /// token 读不到时给出明确错误——比"连上了但所有请求都超时"好查得多。
    fn handshake(&self) -> std::io::Result<()> {
        // 经 `deepcore connect` 建链时，桥进程要先拉起 core；app 拿到管道
        // 时 token 文件可能还没出现。这里等一个启动窗口，而不是把正常的
        // 冷启动竞态报成 "读不到 token"。桥若先退出则立刻失败。
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let token = loop {
            match super::read_token() {
                Ok(token) => break token,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if self.is_closed() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            format!("stdio 桥在 core 就绪前退出: {e}"),
                        ));
                    }
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("读不到守护进程 token: {e}"),
                        ));
                    }
                    std::thread::sleep(CONNECT_RETRY);
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("读不到守护进程 token: {e}"),
                    ));
                }
            }
        };
        self.hello(Some(&token), STARTUP_TIMEOUT).map(|_| ())
    }

    /// 发 `hello` 并校验核心身份与能力。
    ///
    /// 本地 app 传 token；远端 line 不传——`deepcore connect` 会用远端 core
    /// 自己的 token 补上。两种情况都必须经过同一套协议/能力校验。
    pub(crate) fn hello(&self, token: Option<&str>, timeout: Duration) -> std::io::Result<Value> {
        let mut request = json!({
            "op": "hello",
            "protocol": super::protocol::PROTOCOL_VERSION,
            "methods": super::protocol::SUPPORTED_METHODS,
            "capabilities": super::protocol::CAPABILITIES,
        });
        if let Some(token) = token {
            request["token"] = json!(token);
        }
        let reply = self.call_timeout(request, timeout)?;
        if reply["ok"] != true {
            // 版本不匹配要给出可操作的错误：只说"被拒绝"会让用户以为
            // 是 token 问题，然后开始翻权限。
            if reply["kind"].as_str() == Some("version_mismatch") {
                let daemon = reply["protocol"].as_u64().unwrap_or(0) as u32;
                return Err(version_mismatch_error(daemon));
            }
            let detail = reply["error"]
                .as_str()
                .filter(|error| !error.is_empty())
                .unwrap_or("守护进程拒绝了鉴权");
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                detail.to_string(),
            ));
        }

        // **必须核对回复里的版本。**
        //
        // 旧守护进程不认识 `protocol` 字段，会照常回
        // `{"ok": true, "op": "hello"}`——它不会拒绝，也不知道要报版本。
        // 只看 `ok` 的话新 GUI 会静默连上旧 core，然后 `attach` 的 history
        // 与图像回放全被忽略：用户升级后看到"重连没有回滚、图片没了"，而
        // 日志里一条错误都没有。回归测试见
        // `a_daemon_that_omits_its_version_is_rejected`。
        protocol_from_hello_reply(&reply)?;
        Ok(reply)
    }

    /// 连接是否已经断了。
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 这条连接是否已经连续 `quiet` 没收到任何字节。
    ///
    /// 只统计真实到达的帧，不统计“请求还在等回复”：慢链路上一个大帧只要
    /// 还在流动就不算静默，与 Herdr 的 `wire::Heard` 判据一致。
    pub(crate) fn silent_for(&self, quiet: Duration) -> bool {
        let last_rx_ms = self.last_rx_ms.load(Ordering::Relaxed);
        self.read_since
            .elapsed()
            .saturating_sub(Duration::from_millis(last_rx_ms))
            >= quiet
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
            .insert(channel, tx.clone());

        // 补发登记之前就到的数据（attach 的整屏快照通常在这时到达）。
        // 必须在返回接收端之前：晚了这些字节会被排到新输出后面，顺序反掉。
        let buffered = self
            .pending
            .lock()
            .expect("client pending")
            .remove(&channel);
        if let Some(pending) = buffered
            && !pending.frames.is_empty()
        {
            log::debug!("补发 {channel} 的 {} 字节（登记前到达）", pending.bytes);
            // **整帧补发**：`Meta` 帧与终端数据共用 channel，还原成
            // `Frame::data` 会把它的 JSON 直接写进 VT。
            for frame in pending.frames {
                let _ = tx.try_send(frame);
            }
        }
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
        // 先登记再发送：回复可能在 `send` 返回后、`await_reply` 登记前
        // 就到达。高延迟的远端 line 上这个窗口比本地 socket 更容易命中。
        let (tx, rx) = std::sync::mpsc::channel::<Frame>();
        self.pending_replies
            .lock()
            .expect("pending replies")
            .insert(id, tx);
        // 登记之后**每条退出路径**都要摘掉：正常回复由 reader 摘、超时由
        // `PendingReply` 摘、序列化/发送失败由它的析构摘掉。
        //
        // 以前只在 `send` 失败时摘，超时那条会永久留下一个 `Sender`。半开
        // SSH（本仓库专门要活下来的场景）下每次 30 秒超时都留一份；更要命
        // 的是 reader 对**没带 id** 的旧版回复取 `keys().min()`，一个早就
        // 超时的僵尸登记会顶替真正在等握手的那一个，把 hello 回复吃掉。
        let _pending = PendingReply {
            map: self.pending_replies.clone(),
            id,
        };
        let bytes = serde_json::to_vec(&request)?;
        self.send(Frame::json(bytes))?;
        self.await_reply(rx, timeout)
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
    fn await_reply(
        &self,
        rx: std::sync::mpsc::Receiver<Frame>,
        timeout: Duration,
    ) -> std::io::Result<Value> {
        // 登记"我在等 id"，然后带超时地等 —— **不再起临时线程抢共享队列**。
        //
        // 旧实现每次 call 都新起一条线程去 `recv_blocking()` 同一个共享接收端，
        // 抢到的回复若 id 不匹配就**直接丢弃**。并发调用时（打开窗口会同时发
        // `get`/`list`/`attach`）B 会把 A 的回复吃掉，A 等到超时。`attach`
        // 超时的后果不是"报个错"：拿不到 `channel` 就不会登记路由，之后该
        // 终端的数据全被丢进 `pending`，界面永久冻在最后一屏。
        match rx.recv_timeout(timeout) {
            Ok(frame) => {
                let reply: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                Ok(reply)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "守护进程没有回复",
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "守护进程连接已断开",
            )),
        }
    }

    /// 发一帧。
    pub fn send(&self, frame: Frame) -> std::io::Result<()> {
        // 投递即可，不等写线程。写线程没了说明连接已断。
        self.outbox
            .try_send(frame)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "写线程已退出"))
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

/// 一次控制调用登记的清理 guard。
///
/// `call_timeout` 登记等待者之后，无论走哪条退出路径都必须把它摘掉。正常
/// 收到回复时 reader 线程已经 `remove` 过，这里再摘一次是幂等的。
struct PendingReply {
    map: Arc<std::sync::Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<Frame>>>>,
    id: u64,
}

impl Drop for PendingReply {
    fn drop(&mut self) {
        if let Ok(mut replies) = self.map.lock() {
            replies.remove(&self.id);
        }
    }
}

/// `deepcore connect` 子进程的清理 guard。
///
/// 最后一条 `Client` 释放时结束桥进程。core 与 holder 都在独立会话里，
/// 不会跟着结束；下一次连接会重新接回同一批终端。
struct BridgeProcess {
    child: std::sync::Mutex<Option<Child>>,
}

impl BridgeProcess {
    #[cfg(not(test))]
    fn new(child: Child) -> Self {
        Self {
            child: std::sync::Mutex::new(Some(child)),
        }
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        let Ok(slot) = self.child.get_mut() else {
            return;
        };
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 路由登记之前到达的一批帧。
///
/// 与 `core_holder::Pending` 同一个理由：必须整帧保留。`Kind::Meta` 与
/// `Kind::Data` 共用一条 channel，只留 payload 会把标签用的 JSON 当成
/// 终端输出补发，VT 里直接画出 `{"foreground":...}`。
#[derive(Default)]
struct Pending {
    frames: Vec<Frame>,
    /// 已攒的 payload 字节数（`MAX_PENDING` 只按它算）。
    bytes: usize,
}

/// 单个 channel 在路由登记之前最多先攒多少字节。
///
/// 这个缓冲只服务于**一个极短的窗口**：`attach` 的回复还在路上、而 holder
/// 已经把整屏快照推过来了。正常几十毫秒内就会被认领。
///
/// 取 256 KiB（与初始流控额度同量级）而不是 8 MiB：实测一屏 180×50 的完整
/// 重绘约 200 KB，256 KiB 刚好容得下；而万一某个 channel 永远没人认领
/// （客户端 bug、或对端发了没人要的号），最多也只占这么多，不会把 GUI 的
/// 内存吃掉。
///
/// 值**故意**小于 `core_holder::MAX_PENDING`（8 MiB）：那个是 core 中转
/// 上限，这里是 GUI 自己的上限，两者压力模型不同，别"顺手统一"。
///
/// 超限时**丢掉整段缓冲**而不是截断——截断会把半截转义序列留在客户端，
/// 反而让后续数据全部错位（这正是 `render_screen` 要发 `CAN` 的原因）。
const MAX_PENDING: usize = 256 * 1024;

/// 从 `hello` 回复里取对端协议版本并校验，返回校验通过的版本号。
///
/// 协议号相同还不够：旧 core 也会报 `protocol: 2`，但缺少能力清单与身份
/// 字段。那些 core 对 `attach` 的 history、kitty 回放等能力只能靠猜；这里
/// 要求字段齐全，缺任何一项都用 `Unsupported` 拒绝，让调用方走“重启 core、
/// 不碰 holder”的升级路径。
///
/// 抽成纯函数是为了可测：`handshake` 要真实 socket 与 token 文件。
fn protocol_from_hello_reply(reply: &Value) -> std::io::Result<u32> {
    let version = reply
        .get("protocol")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .unwrap_or(1);
    if version != super::protocol::PROTOCOL_VERSION {
        return Err(version_mismatch_error(version));
    }

    for field in [
        "version",
        "core_id",
        "generation",
        "hostname",
        "methods",
        "capabilities",
        "keeps_running_after_logout",
        "logout_linger_user",
    ] {
        if reply.get(field).is_none() {
            return Err(missing_hello_field(field));
        }
    }
    for field in ["version", "core_id", "generation", "hostname"] {
        if reply[field]
            .as_str()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            return Err(missing_hello_field(field));
        }
    }
    if !reply["keeps_running_after_logout"].is_boolean()
        && !reply["keeps_running_after_logout"].is_null()
    {
        return Err(missing_hello_field("keeps_running_after_logout"));
    }

    for (field, required) in [
        ("methods", super::protocol::REQUIRED_METHODS),
        ("capabilities", super::protocol::REQUIRED_CAPABILITIES),
    ] {
        let Some(advertised) = reply[field].as_array() else {
            return Err(missing_hello_field(field));
        };
        for item in required {
            if !advertised.iter().any(|value| value.as_str() == Some(item)) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!("守护进程没有声明必需能力 {item}（{field} 不完整）"),
                ));
            }
        }
    }
    Ok(version)
}

fn missing_hello_field(field: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("守护进程 hello 回复缺少必需字段 {field}"),
    )
}

/// 协议版本不匹配时的统一错误。
///
/// `kind` 用 `Unsupported`：调用方（`connect_or_start`）据此决定"重启旧
/// core"而不是把错误抛给用户——core 可以随便重启，PTY 在 holder 手里。
fn version_mismatch_error(daemon_protocol: u32) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "守护进程协议版本为 {daemon_protocol}，本程序需要 {}。",
            super::protocol::PROTOCOL_VERSION
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_hello() -> Value {
        json!({
            "ok": true,
            "op": "hello",
            "protocol": crate::daemon::protocol::PROTOCOL_VERSION,
            "version": env!("CARGO_PKG_VERSION"),
            "core_id": "core-test",
            "generation": "generation-test",
            "hostname": "host-test",
            "methods": crate::daemon::protocol::SUPPORTED_METHODS,
            "capabilities": crate::daemon::protocol::CAPABILITIES,
            "keeps_running_after_logout": false,
            "logout_linger_user": Value::Null,
        })
    }

    /// **登记前到达的帧必须保留帧类型。**
    ///
    /// 回归价值：`attach` 的整屏快照总在回复之前到达，客户端要等回复解析出
    /// channel 号才登记路由，中间这段窗口里的帧靠 `pending` 兜着。如果缓冲
    /// 只留 payload 字节，一条早到的 `Kind::Meta` 会在登记后被当成终端输出
    /// 补发——VT 里直接画出 `{"foreground":...}` 这段 JSON。
    #[test]
    fn buffered_frames_keep_their_kind() {
        let mut pending = Pending::default();
        pending
            .frames
            .push(Frame::new(1, Kind::Meta, b"{}".to_vec()));
        pending.bytes += pending.frames.last().expect("刚推入").payload.len();
        pending.frames.push(Frame::data(1, b"screen".to_vec()));
        pending.bytes += pending.frames.last().expect("刚推入").payload.len();

        let kinds: Vec<Kind> = pending.frames.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::Meta, Kind::Data],
            "缓冲不能把 Meta 压成 Data"
        );
    }

    /// 静默判据必须看“真实到达的字节”，并在新帧到达后复位。
    ///
    /// 回归价值：SSH 半开时 reader 不会收到 EOF，只有这个时间戳能让
    /// supervisor 判断 line 已经不可用；若只看控制请求是否成功，一次
    /// 慢请求就可能误杀仍在正常推流的终端。
    #[cfg(unix)]
    #[test]
    fn incoming_bytes_reset_the_silence_clock() {
        use std::os::unix::net::UnixStream;

        let (read_stream, mut peer_write) = UnixStream::pair().expect("read pair");
        let (write_stream, _peer_keepalive) = UnixStream::pair().expect("write pair");
        let client = Client::from_transport(read_stream, write_stream).expect("in-memory client");
        std::thread::sleep(Duration::from_millis(30));
        assert!(client.silent_for(Duration::from_millis(20)));

        Frame::json(br#"{"id":1,"ok":true}"#.to_vec())
            .write_to(&mut peer_write)
            .expect("write frame");
        let deadline = Instant::now() + Duration::from_secs(1);
        while client.silent_for(Duration::from_millis(20)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !client.silent_for(Duration::from_millis(20)),
            "收到帧后静默计时必须复位"
        );
    }

    /// 只连不拉起：没有守护进程时必须立刻返回错误，而不是卡住或起一个。
    ///
    /// 这条同时保护测试隔离——如果 `connect_existing` 会拉起守护进程，
    /// 并行跑的测试会互相抢 socket。
    #[test]
    fn connecting_without_a_daemon_fails_fast() {
        let _dir = super::super::RuntimeDirGuard::new("/tmp/deepterm-no-daemon");
        let start = Instant::now();
        let result = Client::connect_existing();
        assert!(result.is_err(), "没有守护进程时应失败");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "必须快速失败，实际耗时 {:?}",
            start.elapsed()
        );
    }

    /// **并发的 `call` 不能互相吞掉对方的回复。**
    ///
    /// 回归价值：这是"关掉窗口再重开，TUI 界面直接卡死"的根因。
    ///
    /// 旧实现每次 `call` 新起一条线程去抢同一个共享接收端，抢到的回复若
    /// id 不匹配就**直接丢弃**。打开窗口时 `get`/`list`/`attach` 是并发发
    /// 的，B 把 A 的回复吃掉后 A 等到超时——而 `attach` 超时的后果不是
    /// "报个错"：拿不到 `channel` 就不会调 `attach_channel` 登记路由，
    /// 之后该终端的数据全被丢进 `pending`，接收端永远空着，界面冻死。
    ///
    /// 这里直接验证分发语义：两个 id 的回复按 id 各自送达，谁也不会吃谁的。
    #[test]
    fn concurrent_calls_never_steal_each_others_replies() {
        use std::sync::mpsc::Sender;
        use std::sync::{Arc, Mutex};

        let pending: Arc<Mutex<std::collections::HashMap<u64, Sender<Frame>>>> = Arc::default();

        // 两条"等待者"，模拟两个并发 call。
        let (tx1, rx1) = std::sync::mpsc::channel();
        let (tx2, rx2) = std::sync::mpsc::channel();
        pending.lock().unwrap().insert(1, tx1);
        pending.lock().unwrap().insert(2, tx2);

        // 模拟 reader 线程分发：**先到的是 id=2 的回复**（顺序与登记顺序相反，
        // 这正是旧实现会出错的场景）。
        for id in [2u64, 1u64] {
            let waiter = pending.lock().unwrap().remove(&id);
            let frame = Frame::json(serde_json::to_vec(&json!({"id": id, "ok": true})).unwrap());
            waiter.expect("每个 id 都应有等待者").send(frame).unwrap();
        }

        let r1: Value =
            serde_json::from_slice(&rx1.try_recv().expect("id=1 必须收到").payload).unwrap();
        let r2: Value =
            serde_json::from_slice(&rx2.try_recv().expect("id=2 必须收到").payload).unwrap();
        assert_eq!(r1["id"], 1, "id=1 的等待者只能收到自己的回复");
        assert_eq!(r2["id"], 2, "id=2 的等待者只能收到自己的回复");
        assert!(pending.lock().unwrap().is_empty(), "分发后不应残留等待者");
    }

    /// 超时的调用必须把自己的登记摘掉，不能留下僵尸等待者。
    ///
    /// 回归价值：以前只在 `send` 失败时 `remove`，超时那条会永久留在
    /// `pending_replies` 里。半开 SSH 下每次超时都留一份，而且 reader 对
    /// 没带 id 的旧版回复取 `keys().min()`——一个早就超时的僵尸会顶替真正
    /// 在等握手的那一个，把 hello 回复吃掉，于是"连接明明活着却握手失败"。
    #[test]
    fn a_timed_out_call_leaves_no_waiter_behind() {
        // reader 端永不回话：用一对空管道模拟"对端不响应"。
        let client =
            Client::from_transport(std::io::Cursor::new(Vec::<u8>::new()), Vec::<u8>::new())
                .expect("in-memory client");

        let result = client.call_timeout(json!({"op": "hello"}), Duration::from_millis(30));
        assert!(result.is_err(), "没有回复就应该是超时错误");
        assert_eq!(
            client
                .pending_replies
                .lock()
                .expect("pending replies")
                .len(),
            0,
            "超时后不得残留等待者"
        );
        // 再调一次也不该累加。
        let _ = client.call_timeout(json!({"op": "hello"}), Duration::from_millis(10));
        assert_eq!(
            client
                .pending_replies
                .lock()
                .expect("pending replies")
                .len(),
            0,
            "连续超时也不得累积等待者"
        );
    }

    /// 启动超时必须是个合理的值：太短会让慢机器上的守护进程起不来。
    #[test]
    fn the_startup_timeout_is_generous_enough_for_a_cold_start() {
        assert!(
            STARTUP_TIMEOUT >= Duration::from_secs(2),
            "冷启动超时太短: {STARTUP_TIMEOUT:?}"
        );
    }

    /// **不报版本的守护进程必须被拒绝。**
    ///
    /// 回归价值：这是"升级后回滚与图片静默消失"的直接原因。旧 core 不认
    /// 识 `protocol` 字段，会照常回 `{"ok": true}`；只看 `ok` 的话新 GUI
    /// 会"连上"旧 core，之后 `attach` 的 `history` 与图像回放全被忽略，
    /// 而日志里没有任何错误。
    ///
    /// 这里不起真实进程：`handshake` 的判据抽在
    /// `protocol_from_hello_reply` 里，直接断言它的结论。
    #[test]
    fn a_daemon_that_omits_its_version_is_rejected() {
        let legacy = json!({"ok": true, "op": "hello"});
        let err = protocol_from_hello_reply(&legacy).expect_err("不报版本必须被拒");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");

        let current = current_hello();
        assert_eq!(
            protocol_from_hello_reply(&current).unwrap(),
            crate::daemon::protocol::PROTOCOL_VERSION
        );

        let newer = json!({"ok": true, "protocol": 999});
        assert!(
            protocol_from_hello_reply(&newer).is_err(),
            "更新的协议也必须拒绝：本进程不认识它的新字段"
        );
    }

    /// **协议号相同但能力字段缺失的旧 core 也必须拒绝。**
    ///
    /// 这是协议 2 扩展字段的兼容边界：旧 core 报同样的 `protocol: 2`，
    /// 但不知道自己在 hello 里要提供能力清单。只看版本号会让 app 静默
    /// 接上一个“看起来兼容、实际少一半语义”的 core。
    #[test]
    fn a_daemon_without_hello_capabilities_is_rejected() {
        for field in [
            "generation",
            "core_id",
            "hostname",
            "methods",
            "capabilities",
            "keeps_running_after_logout",
            "logout_linger_user",
        ] {
            let mut hello = current_hello();
            hello.as_object_mut().expect("object").remove(field);
            let err =
                protocol_from_hello_reply(&hello).expect_err(&format!("缺少 {field} 时必须拒绝"));
            assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
            assert!(err.to_string().contains(field), "错误应点名 {field}: {err}");
        }
    }

    #[test]
    fn an_unconfirmed_linger_state_is_valid_and_stays_unknown() {
        let mut hello = current_hello();
        hello["keeps_running_after_logout"] = Value::Null;
        assert_eq!(
            protocol_from_hello_reply(&hello).expect("null 表示无法证实，仍是合法握手"),
            crate::daemon::protocol::PROTOCOL_VERSION
        );
    }

    /// 清单结构对但少一项必需能力时也要拒绝，并点名缺的是哪项。
    #[test]
    fn a_daemon_missing_a_required_capability_is_rejected() {
        let mut hello = current_hello();
        let capabilities: Vec<&str> = crate::daemon::protocol::REQUIRED_CAPABILITIES
            .iter()
            .copied()
            .filter(|capability| *capability != "terminal.attach")
            .collect();
        hello["capabilities"] = json!(capabilities);
        let err = protocol_from_hello_reply(&hello).expect_err("缺能力必须被拒");
        assert!(err.to_string().contains("terminal.attach"), "{err}");
    }

    /// 旧 holder 没有机器 API，但仍是合法的终端后端。
    ///
    /// 回归价值：core 升级后第一次重连必须接回旧 holder 里正在跑的 PTY；
    /// 若 `machines.v1` 被误列为必需能力，用户会看到所有终端突然消失。
    #[test]
    fn a_holder_without_machine_support_is_still_compatible() {
        let mut hello = current_hello();
        let methods: Vec<&str> = crate::daemon::protocol::SUPPORTED_METHODS
            .iter()
            .copied()
            .filter(|method| !crate::daemon::protocol::MACHINE_METHODS.contains(method))
            .collect();
        let capabilities: Vec<&str> = crate::daemon::protocol::CAPABILITIES
            .iter()
            .copied()
            .filter(|capability| *capability != "machines.v1")
            .collect();
        hello["methods"] = json!(methods);
        hello["capabilities"] = json!(capabilities);
        assert_eq!(
            protocol_from_hello_reply(&hello).expect("hello 里带着协议版本"),
            crate::daemon::protocol::PROTOCOL_VERSION
        );
    }
}
