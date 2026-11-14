//! core 侧的 holder 连接：拉起 holder、把 spawn 转发给它、把输出接回来。
//!
//! # 数据流
//!
//! ```text
//! UI ──socket──▶ core ──holder socket──▶ holder
//!                 │                        │
//!                 └──── 输出原样转发 ◀───────┘
//! ```
//!
//! core 在这里是个**代理**：它对 UI 保持原来的协议不变，只是把 `spawn`
//! 和字节流中转给 holder。这样 UI 侧一行都不用改，而 PTY 的父进程从
//! core 变成了 holder——core 重启不再带走终端。
//!
//! # 为什么代理而不是"UI 直连 holder"
//!
//! UI 直连的话，`terminals.json`、`keep`、地址分配这些状态就得由 holder
//! 持有，而 holder 是个应当尽量无脑的进程（它的职责是"别死"）。让 core
//! 这个可以随便重启的进程做协调、holder 只负责持 PTY，职责更清楚，也和
//! herdr 的分工一致。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::protocol::{CONTROL, Frame, Kind};
use super::transport;

/// 等 holder 起来的最长时间。
const HOLDER_STARTUP: Duration = Duration::from_secs(3);

/// 拉起后重试连接的间隔。
const CONNECT_RETRY: Duration = Duration::from_millis(50);

/// 与 holder 的连接。
///
/// 全局单例：core 只有一个 holder，所有终端都走这一条连接（holder 侧
/// 每条连接要占两个线程，按终端各连一条会白白多出一堆）。
pub struct HolderClient {
    writer: Mutex<std::io::BufWriter<transport::Stream>>,
    /// holder 推来的**控制回复**（channel 0 上的 JSON）。
    ///
    /// 与终端输出分开两条通道：两者共用一个接收端时，`call` 会把终端
    /// 输出的帧当控制回复吞掉，而泵任务又会把控制回复当输出吃掉——表现
    /// 是"handler 说处理了、实际什么都没发生"，极难查。
    incoming: async_channel::Receiver<Frame>,
    /// 与 holder 的连接是否已断。
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// channel → 输出接收端。由 `attach_channel` 登记，reader 线程按号投递。
    routes: Arc<Mutex<std::collections::HashMap<u32, async_channel::Sender<Frame>>>>,
    /// 路由尚未登记时提前到达的帧（按 channel 攒着）。
    ///
    /// 见 reader 线程里的说明：`attach` 的快照总在回复之前到达，这段缓冲
    /// 就是为了不让它丢掉。
    pending: Arc<Mutex<std::collections::HashMap<u32, Vec<u8>>>>,
}

/// 单个 channel 最多攒多少字节。
///
/// 正常情况这些数据毫秒内就会被认领，这个上限只防"holder 推了一个没人
/// 要的 channel"把内存吃光。
const MAX_PENDING: usize = 8 * 1024 * 1024;

static HOLDER: Mutex<Option<Arc<HolderClient>>> = Mutex::new(None);

/// 取（必要时拉起）holder 连接。
pub fn holder() -> Option<Arc<HolderClient>> {
    let guard = || {
        HOLDER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };

    if let Some(client) = guard().as_ref()
        && !client.is_closed()
    {
        return Some(client.clone());
    }

    match connect_or_start() {
        Ok(client) => {
            let client = Arc::new(client);
            let mut slot = guard();
            match slot.as_ref().filter(|existing| !existing.is_closed()) {
                Some(existing) => Some(existing.clone()),
                None => {
                    *slot = Some(client.clone());
                    log::info!("已（重新）连接 holder");
                    Some(client)
                }
            }
        }
        Err(e) => {
            log::error!("无法连接 holder: {e}");
            None
        }
    }
}

/// 连接 holder；没在跑就拉起来。
fn connect_or_start() -> std::io::Result<HolderClient> {
    let path = super::holder_socket_path();
    if let Ok(stream) = transport::connect(&path) {
        return HolderClient::from_stream(stream);
    }

    spawn_holder()?;

    let deadline = Instant::now() + HOLDER_STARTUP;
    let mut last = None;
    while Instant::now() < deadline {
        match transport::connect(&path) {
            Ok(stream) => return HolderClient::from_stream(stream),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(CONNECT_RETRY);
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "holder 未能在预期时间内启动")
    }))
}

/// 拉起 `deepcore --holder`。
fn spawn_holder() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    log::info!("拉起 holder: {}", exe.display());

    let mut command = std::process::Command::new(exe);
    command.arg(super::holder::HOLDER_FLAG);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // 关键：让 holder 脱离 core 的会话。
    //
    // 不脱离的话，core 退出时（用户关掉 deepterm、`kill` 掉 core、或
    // 终端会话结束）holder 会跟着收到 SIGHUP/SIGTERM——而"活过 core 重启"
    // 正是它存在的全部意义。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY：`setsid` 在 fork 后的子进程里执行，失败返回 -1；
        // 调用的是 async-signal-safe 的系统调用。
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `DETACHED_PROCESS` + `CREATE_NEW_PROCESS_GROUP`：不附着到 core 的
        // 控制台，也不接收 core 的控制台事件（Ctrl+C 之类）。
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    command.spawn()?;
    Ok(())
}

impl HolderClient {
    fn from_stream(stream: transport::Stream) -> std::io::Result<Self> {
        let read_stream = transport::try_clone(&stream)?;
        let (tx, rx) = async_channel::unbounded::<Frame>();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = closed.clone();
        // 各终端 channel 的输出接收端（core 侧订阅 holder 的输出）。
        let routes: Arc<Mutex<std::collections::HashMap<u32, async_channel::Sender<Frame>>>> =
            Arc::default();
        let reader_routes = routes.clone();
        let pending: Arc<Mutex<std::collections::HashMap<u32, Vec<u8>>>> = Arc::default();
        let pending_rx = pending.clone();

        std::thread::Builder::new()
            .name("holder-reader".into())
            .spawn(move || {
                let mut reader = read_stream;
                loop {
                    match super::protocol::read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            if frame.channel == CONTROL {
                                let _ = tx.try_send(frame);
                                continue;
                            }
                            let channel = frame.channel;
                            let sender = reader_routes
                                .lock()
                                .expect("holder routes")
                                .get(&channel)
                                .cloned();
                            match sender {
                                Some(sender) => {
                                    if sender.try_send(frame).is_err() {
                                        reader_routes
                                            .lock()
                                            .expect("holder routes")
                                            .remove(&channel);
                                    }
                                }
                                None => {
                                    // 路由还没登记：**不能丢**。
                                    //
                                    // `attach` 的流程是 holder 先把整屏快照
                                    // 推过来、再回复 channel 号，而转发层拿到
                                    // channel 号之后才 `attach_channel`。中间
                                    // 这段窗口里的帧（正是那份快照）到这里时
                                    // 路由还不存在。
                                    //
                                    // 丢掉它的后果非常具体：客户端收不到
                                    // `?1049h`，本地 VT 停在主屏，画面被当成
                                    // 普通输出追加、滚轮能整体滚走——也就是
                                    // "nvim 不再像全屏程序"。
                                    //
                                    // 先攒着，登记路由时补发。
                                    let mut p = pending_rx.lock().expect("pending");
                                    let buf = p.entry(channel).or_default();
                                    // 上限保护：没人来认领的 channel 不该
                                    // 无限吃内存（正常会在毫秒内被取走）。
                                    if buf.len() + frame.payload.len() <= MAX_PENDING {
                                        buf.extend_from_slice(&frame.payload);
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log::debug!("与 holder 的连接断开: {e}");
                            break;
                        }
                    }
                }
                reader_routes.lock().expect("holder routes").clear();
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })?;

        let client = Self {
            writer: Mutex::new(std::io::BufWriter::new(stream)),
            incoming: rx,
            closed,
            routes,
            pending,
        };
        client.handshake()?;
        Ok(client)
    }

    /// 与 holder 完成鉴权握手。
    ///
    /// holder 与 core 各有一份 token（见 `token_path_for`）；这里读 holder
    /// 的那份。漏掉这一步时，后续每条请求都会被服务端拒掉，而表面上
    /// 连接是"成功"的——最初的表现就是"spawn 说成功，holder 里却没有"。
    fn handshake(&self) -> std::io::Result<()> {
        let token = super::read_token_for(&super::holder_socket_path()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("读不到 holder token: {e}"),
            )
        })?;
        let request = json!({"id": 0, "op": "hello", "token": token});
        self.send(Frame::json(serde_json::to_vec(&request)?))?;

        match self.incoming.recv_blocking() {
            Ok(frame) => {
                let reply: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                if reply["ok"] == true {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "holder 拒绝了鉴权",
                    ))
                }
            }
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "鉴权时与 holder 的连接断开",
            )),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 登记一个终端 channel，返回它的输出流。
    ///
    /// core 与 holder 之间**沿用同一个 channel 号**：UI 请求 attach 时拿到
    /// 的编号会原样告诉 holder，holder 推数据也用这个号。这样 core 只需要
    /// 按号转发，不必维护一张额外的映射表（映射表还有个隐患：holder 重启
    /// 后旧编号可能撞上新终端）。
    pub fn attach_channel(&self, channel: u32) -> async_channel::Receiver<Frame> {
        let (tx, rx) = async_channel::unbounded();
        self.routes()
            .lock()
            .expect("holder routes")
            .insert(channel, tx.clone());

        // 补发登记之前就到的数据（`attach` 的整屏快照几乎总在这时到达）。
        //
        // 补发必须发生在**返回接收端之前**：调用方拿到 `rx` 就立刻开始读，
        // 晚一步这些字节就会被当成"新输出"排到后面，画面顺序就反了。
        let buffered = self.pending.lock().expect("pending").remove(&channel);
        if let Some(bytes) = buffered
            && !bytes.is_empty()
        {
            log::debug!("补发 {channel} 的 {} 字节（登记前到达）", bytes.len());
            let _ = tx.try_send(Frame::data(channel, bytes));
        }
        rx
    }

    fn routes(&self) -> &Arc<Mutex<std::collections::HashMap<u32, async_channel::Sender<Frame>>>> {
        &self.routes
    }

    /// 注销一个 channel。
    pub fn detach_channel(&self, channel: u32) {
        self.routes()
            .lock()
            .expect("holder routes")
            .remove(&channel);
    }

    /// 发一帧给 holder。
    pub fn send(&self, frame: Frame) -> std::io::Result<()> {
        let mut writer = self.writer.lock().expect("holder writer");
        frame.write_to(&mut *writer)
    }

    /// 起一个终端，返回 holder 分配的地址。
    pub fn spawn(&self, request: Value) -> Result<String, String> {
        let mut request = request;
        request["op"] = json!("spawn");
        let reply = self
            .call(request)
            .map_err(|e| format!("holder 没有响应: {e}"))?;
        if reply["ok"] != true {
            return Err(reply["error"]
                .as_str()
                .unwrap_or("holder 拒绝创建终端")
                .to_string());
        }
        reply["address"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "holder 没有返回地址".to_string())
    }

    /// 控制面调用。
    pub fn call(&self, request: Value) -> std::io::Result<Value> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut request = request;
        if let Some(obj) = request.as_object_mut() {
            obj.insert("id".into(), json!(id));
        }
        self.send(Frame::json(serde_json::to_vec(&request)?))?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "holder 没有回复",
                ));
            }
            match self.incoming.recv_blocking() {
                Ok(frame) => {
                    if frame.kind != Kind::Json {
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
                        "与 holder 的连接已断开",
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// holder 与 core 必须用不同的 socket。
    #[test]
    fn the_holder_socket_differs_from_the_core_socket() {
        assert_ne!(
            super::super::socket_path(),
            super::super::holder_socket_path()
        );
    }

    /// 拉起 holder 时用的参数必须能被 `holder::is_holder_invocation` 认出来。
    ///
    /// 这条防的是"两边各写一个常量"：`spawn_holder` 拼的参数如果和识别
    /// 逻辑对不上，holder 会以 core 身份启动，然后和自己抢同一个 socket。
    #[test]
    fn the_spawn_flag_matches_the_detection() {
        assert!(super::super::holder::is_holder_invocation(&[
            "deepcore".into(),
            super::super::holder::HOLDER_FLAG.into()
        ]));
    }
}
