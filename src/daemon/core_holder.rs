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

use super::protocol::{CONTROL, Frame};
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
    /// 待写帧的队列（由专用写线程消费）。
    ///
    /// # 为什么不在调用方线程直接写
    ///
    /// 旧实现是 `Mutex<BufWriter>` + 调用方持锁 `write_to`，而 `write_to`
    /// 结尾会 `flush()`。这带来两个问题：
    ///
    /// 1. **调用方是 core 的连接读线程**——它一边等着读 UI 发来的滚轮/按键，
    ///    一边同步往 holder 写。socket 一卡（holder 正在忙、内核缓冲满），
    ///    这条线程就停住，**后续所有输入**都排在它后面。
    /// 2. 每帧一次 `flush` 就是一次系统调用；滚轮是高频事件，一次滚动几十次
    ///    写，光 syscall 就够掉帧。
    ///
    /// 现在与 GUI→core 用同一套分工：投递进无界队列，写线程负责真正写出去。
    /// 投递永不阻塞调用方。
    outbox: async_channel::Sender<Frame>,
    /// holder 控制回复的分发表：请求 id → 该请求的等待者。
    ///
    /// # 为什么不能用共享队列
    ///
    /// 旧实现是一个共享的 `incoming` 接收端，`call` 从里面抢回复、**id 不
    /// 匹配就直接丢弃**，而且用的是**没有超时**的 `recv_blocking()`。于是：
    ///
    /// - 并发两个 call（打开窗口时 `get`/`list`/`attach` 确实是并发的）时，
    ///   B 把 A 的回复吃掉并丢掉，A 永远等不到；
    /// - `recv_blocking()` 没有超时，A 会**永久阻塞在 core 的连接线程上**。
    ///
    /// 后果正是用户报的那个现象：关掉窗口再重开，终端视图拿不到 `attach`
    /// 回复 → 不登记路由 → 数据全进 `pending` → 界面冻在最后一屏。
    ///
    /// 现在按 id 分发，并给每个等待者一个带超时的等待，谁也吃不掉谁的。
    pending_replies: Arc<Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<Frame>>>>,
    /// 与 holder 的连接是否已断。
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// holder 的 channel → 转发目标。
    ///
    /// reader 线程收到数据帧后**就地改写 channel 号并转发**，不再为每个
    /// 挂接起一条 relay 线程。
    ///
    /// # 为什么要这么改
    ///
    /// 旧实现是"每个 attach 一条 relay 线程"，core 里常驻十几条这样的线程，
    /// 每条都在 `recv_blocking` 上等。它们还各自持有一份 `outgoing` 克隆，
    /// 于是连接断开时根本无法靠 sender 计数判断"该退出了没有"——线程必须
    /// 等下一次 `try_send` 失败才退，而如果那个 channel 再没数据，它就永久
    /// 挂着。
    ///
    /// 现在转发在已有的 reader 线程里做（它本来就在读 socket），零额外线程，
    /// 断开时路由直接随 map 一起清掉。
    routes: Arc<Mutex<std::collections::HashMap<u32, Route>>>,
    /// 路由尚未登记时提前到达的帧（按 channel 攒着）。
    ///
    /// 见 reader 线程里的说明：`attach` 的快照总在回复之前到达，这段缓冲
    /// 就是为了不让它丢掉。
    pending: Arc<Mutex<std::collections::HashMap<u32, Pending>>>,
}

/// 一条 holder channel 的转发目标。
struct Route {
    /// 目标连接的出帧队列。
    tx: async_channel::Sender<Frame>,
    /// 目标连接的本地 channel 号（改写后用它）。
    ///
    /// 两端的 channel 号各自独立编号：holder 那条连接从它自己的
    /// `next_channel` 取值，而 GUI 连接从 1 递增。转发时必须改写，
    /// 否则 GUI 的路由表里找不到这个号。
    local_channel: u32,
    /// 终端地址（holder 重连后按它重新挂接）。
    address: String,
    /// 还没开闸时攒下的帧（见 [`HolderClient::resume`]）。
    ///
    /// `attach` 时先压住数据，等回复写出去之后再放行：客户端必须先拿到
    /// channel 号才能登记路由，提前收到数据只能靠它自己缓冲（有上限，
    /// 超限就得丢，丢了就要重新对齐）。
    held: Vec<Frame>,
    /// 是否还在压着。
    paused: bool,
    /// 开闸用的全局唯一票据。
    ///
    /// **不能用 `local_channel` 代替**：它是每条 GUI 连接**各自**从 1 开始
    /// 编号的。两个窗口同时挂着各自的 t1 时，两条连接的 local channel 都是
    /// 1，按它匹配就会把别人的数据流一起开闸（或者反过来，自己的没开）。
    /// 这类错配的结果是"某个窗口的终端永久收不到数据"。
    ticket: u64,
}

/// 全局唯一的开闸票据。
static NEXT_TICKET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_ticket() -> u64 {
    NEXT_TICKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 路由登记之前到达的一批帧。
///
/// **必须整帧保留，不能只留 payload**：`Kind::Meta` 与 `Kind::Data` 走
/// 同一条 channel，语义完全不同。只留字节的话，一条早到的 `Meta` 会
/// 被当成终端输出补发，VT 里于是直接画出 `{"foreground":...}` 这段 JSON。
#[derive(Default)]
struct Pending {
    frames: Vec<Frame>,
    /// 已攒的 payload 字节数（`MAX_PENDING` 只按它算）。
    bytes: usize,
}

/// 单个 channel 最多攒多少字节。
///
/// 正常情况这些数据毫秒内就会被认领，这个上限只防"holder 推了一个没人
/// 要的 channel"把内存吃光。
///
/// **与 `client::MAX_PENDING`（256 KiB）不同值是有意的**，别"顺手统一"：
/// 这里是 core→holder 的中转上限（一份快照动辄 200 KB，且 core 侧本来就
/// 只是过手），那边是 GUI 侧上限，GUI 的内存压力与这里无关。
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
                    let previous = slot.replace(client.clone());
                    log::info!("已（重新）连接 holder");
                    drop(slot);
                    // 换了新连接：把已有的挂接全部重新登记一遍。
                    //
                    // 新连接上 holder 的 channel 号从头开始，旧号一律失效。
                    // 不重建的话所有 viewer 会永久静默（数据在跑、画面不动）。
                    if previous.is_some() {
                        client.recover_routes();
                    }
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
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = closed.clone();
        let pending_replies: Arc<
            Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<Frame>>>,
        > = Arc::default();
        let reader_replies = pending_replies.clone();
        // 各终端 channel 的输出接收端（core 侧订阅 holder 的输出）。
        let routes: Arc<Mutex<std::collections::HashMap<u32, Route>>> = Arc::default();
        let reader_routes = routes.clone();
        let pending: Arc<Mutex<std::collections::HashMap<u32, Pending>>> = Arc::default();
        let pending_rx = pending.clone();

        std::thread::Builder::new()
            .name("holder-reader".into())
            .spawn(move || {
                let mut reader = read_stream;
                loop {
                    match super::protocol::read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            if frame.channel == CONTROL {
                                // 按 id 只喂给等它的那一个 call。
                                //
                                // **兼容旧 holder**：早期版本（以及 11 月 16 日
                                // 之前构建的所有版本）的 hello 回复不带 `id`。
                                // 新 core 若严格要求 id，就会在握手时把手上一份
                                // 正在跑的旧 holder 判成"连接断开"——而它持有
                                // 用户的终端，绝不能因此被抛弃。
                                //
                                // 兜底策略：回复没有 id 时交给唯一等待者（或最早
                                // 登记的那个）。控制面是低频请求，同一时刻通常只有
                                // 一个在等；即使并发，最坏结果是某次请求拿到别家的
                                // 回复并被上层判为无效字段，而不是整条连接不可用。
                                let parsed = serde_json::from_slice::<Value>(&frame.payload).ok();
                                let id = parsed
                                    .as_ref()
                                    .and_then(|v| v.get("id").and_then(Value::as_u64));
                                let waiter = {
                                    let mut replies =
                                        reader_replies.lock().expect("pending replies");
                                    match id {
                                        Some(id) => replies.remove(&id),
                                        None => {
                                            // 取最早登记的一个（HashMap 无序，
                                            // 用最小 id 保证确定性）。
                                            let key = replies.keys().min().copied();
                                            key.and_then(|k| replies.remove(&k))
                                        }
                                    }
                                };
                                if let Some(tx) = waiter {
                                    let _ = tx.send(frame);
                                } else {
                                    log::debug!("收到无人等待的 holder 控制回复，丢弃");
                                }
                                continue;
                            }
                            let channel = frame.channel;
                            // 就地改写 channel 号并转发。
                            //
                            // 这一步以前在每条 attach 各自的 relay 线程里做；
                            // 现在直接在这里做，见 `HolderClient::routes` 的说明。
                            // 就地改写 channel 号并转发；**压着的时候先攒起来**。
                            enum Action {
                                Send(async_channel::Sender<Frame>, Frame),
                                Hold,
                                Buffer(Frame),
                            }
                            let kind = frame.kind;
                            let payload = frame.payload;
                            let action = {
                                let mut routes = reader_routes.lock().expect("holder routes");

                                match routes.get_mut(&channel) {
                                    Some(route) if route.paused => {
                                        // 还没开闸（attach 的回复还在路上）：
                                        // 攒着，等 `resume` 按序放行。
                                        route.held.push(Frame::new(
                                            route.local_channel,
                                            kind,
                                            payload,
                                        ));
                                        Action::Hold
                                    }
                                    Some(route) => Action::Send(
                                        route.tx.clone(),
                                        Frame::new(route.local_channel, kind, payload),
                                    ),
                                    None => Action::Buffer(Frame::new(channel, kind, payload)),
                                }
                            };
                            match action {
                                Action::Send(tx, f) => {
                                    if tx.try_send(f).is_err() {
                                        reader_routes
                                            .lock()
                                            .expect("holder routes")
                                            .remove(&channel);
                                    }
                                }
                                Action::Hold => {}
                                Action::Buffer(frame) => {
                                    // 路由还没登记：**不能丢**。
                                    //
                                    // `attach` 的流程是 holder 先把整屏快照
                                    // 推过来、再回复 channel 号，而转发层拿到
                                    // channel 号之后才登记路由。中间这段窗口里
                                    // 的帧（正是那份快照）到这里时路由还不存在。
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
                                    if buf.bytes + frame.payload.len() <= MAX_PENDING {
                                        buf.bytes += frame.payload.len();
                                        buf.frames.push(frame);
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

        // 写线程：独占 socket 写端。见 `outbox` 字段的说明。
        let (out_tx, out_rx) = async_channel::unbounded::<Frame>();
        {
            let mut buf = std::io::BufWriter::new(stream);
            let closed_flag = closed.clone();
            std::thread::Builder::new()
                .name("holder-writer".into())
                .spawn(move || {
                    while let Ok(frame) = out_rx.recv_blocking() {
                        if frame.write_to(&mut buf).is_err() {
                            break;
                        }
                    }
                    let _ = std::io::Write::flush(&mut buf);
                    closed_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .ok();
        }

        let client = Self {
            outbox: out_tx,
            pending_replies,
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
        let request = json!({
            "id": 0,
            "op": "hello",
            "token": token,
            "protocol": super::protocol::PROTOCOL_VERSION,
        });
        // 先登记 id=0 的等待者，再发请求（回复可能来得比登记更早）。
        let (tx, rx) = std::sync::mpsc::channel::<Frame>();
        self.pending_replies
            .lock()
            .expect("pending replies")
            .insert(0, tx);
        self.send(Frame::json(serde_json::to_vec(&request)?))?;

        match rx.recv_timeout(HOLDER_STARTUP) {
            Ok(frame) => {
                let reply: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                if reply["ok"] != true {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "holder 拒绝了鉴权",
                    ));
                }
                // **与 GUI↔core 不同：holder 的版本不匹配只警告，不拒绝。**
                //
                // holder 是 PTY 的父进程，它手上的终端进程全靠它活着。为了
                // 一次版本差异把它踢掉，等于把用户正在跑的编译、编辑器一起
                // 杀掉——而版本差异只影响 `attach` 的可选字段（history /
                // 图像回放），不影响帧格式与终端存活。
                //
                // 代价说清楚：旧 holder 会静默忽略 `history` 与图像回放，
                // 即"重连后没有回滚/图片"。这条警告就是排查该现象的唯一
                // 线索，因此必须写明。
                let holder_protocol = reply
                    .get("protocol")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .unwrap_or(1);
                if holder_protocol != super::protocol::PROTOCOL_VERSION {
                    log::warn!(
                        "holder 协议版本为 {holder_protocol}、本进程为 {}：\
                         终端仍按旧 holder 继续运行（它持有 PTY，不能为了升级\
                         把它结束掉），但重连时不会恢复回滚与 kitty 图像。\
                         等所有终端退出后重启 deepterm 即可恢复正常。",
                        super::protocol::PROTOCOL_VERSION
                    );
                }
                Ok(())
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
    /// 换连接之后，把所有挂接重新 attach 一遍。
    ///
    /// 新连接上 holder 的 channel 号从 1 重新开始，旧号全部失效。这里按
    /// 地址重新 attach，拿到新号后**就地更新路由**——所以对上层（GUI）来说
    /// channel 号完全不变，只是底层换了条通道。
    ///
    /// 整个过程在后台线程做：`call` 要等 holder 回复，不能阻塞调用方
    /// （可能是某条连接的读线程）。
    fn recover_routes(self: &Arc<Self>) {
        let this = self.clone();
        std::thread::Builder::new()
            .name("holder-recover".into())
            .spawn(move || {
                // 给 holder 一点时间就位。
                std::thread::sleep(std::time::Duration::from_millis(100));
                let routes: Vec<(u32, String, async_channel::Sender<Frame>, u32)> = {
                    let guard = this.routes.lock().expect("holder routes");
                    guard
                        .iter()
                        .map(|(holder_ch, r)| {
                            (*holder_ch, r.address.clone(), r.tx.clone(), r.local_channel)
                        })
                        .collect()
                };
                if routes.is_empty() {
                    return;
                }
                let mut recovered = Vec::with_capacity(routes.len());
                for (old_holder_ch, address, tx, local_channel) in routes {
                    let reply = this.call(serde_json::json!({
                        "op": "attach",
                        "address": address,
                        "history": 0,
                    }));
                    let Ok(reply) = reply else { continue };
                    let Some(new_ch) = reply["channel"].as_u64() else {
                        continue;
                    };
                    recovered.push((old_holder_ch, new_ch as u32, address, tx, local_channel));
                }
                if recovered.is_empty() {
                    return;
                }
                {
                    let mut guard = this.routes.lock().expect("holder routes");
                    for (old_ch, new_ch, address, tx, local_channel) in &recovered {
                        guard.remove(old_ch);
                        guard.insert(
                            *new_ch,
                            Route {
                                tx: tx.clone(),
                                local_channel: *local_channel,
                                address: address.clone(),
                                // 恢复时通道已经是通的（GUI 那侧早就登记过路由）。
                                held: Vec::new(),
                                paused: false,
                                ticket: next_ticket(),
                            },
                        );
                    }
                }
                log::info!("holder 重连后恢复了 {} 个挂接", recovered.len());
            })
            .ok();
    }

    /// 登记一条 holder channel 的转发目标。
    ///
    /// holder 会在回复 `attach` 之前就把整屏快照推过来，所以登记之前到达
    /// 的帧必须补发（见 `pending`）。补发必须在返回前完成：调用方拿到回执
    /// 就立刻开始收数据，晚一步那份快照会被排到新输出后面，画面顺序就反了。
    pub fn register_route(
        &self,
        holder_channel: u32,
        local_channel: u32,
        address: &str,
        tx: async_channel::Sender<Frame>,
        paused: bool,
    ) -> u64 {
        let ticket = next_ticket();
        // 登记路由与取 pending 分两次加锁，不嵌套（与 reader 的加锁顺序一致，
        // 避免 AB/BA 死锁形态）。
        {
            let mut routes = self.routes.lock().expect("holder routes");
            routes.insert(
                holder_channel,
                Route {
                    tx: tx.clone(),
                    local_channel,
                    address: address.to_string(),
                    held: Vec::new(),
                    paused,
                    ticket,
                },
            );
        }
        let buffered = self
            .pending
            .lock()
            .expect("pending")
            .remove(&holder_channel);
        if let Some(pending) = buffered
            && !pending.frames.is_empty()
        {
            if paused {
                // 还没开闸：先攒在路由里，等 `resume` 时一并发出。
                let mut routes = self.routes.lock().expect("holder routes");
                if let Some(route) = routes.get_mut(&holder_channel) {
                    route.held.extend(
                        pending
                            .frames
                            .into_iter()
                            .map(|f| Frame::new(local_channel, f.kind, f.payload)),
                    );
                }
            } else {
                log::debug!(
                    "补发 {holder_channel} 的 {} 字节（登记前到达）",
                    pending.bytes
                );
                for f in pending.frames {
                    let _ = tx.try_send(Frame::new(local_channel, f.kind, f.payload));
                }
            }
        }
        ticket
    }

    /// 剔除一条转发路由（不发请求）。
    ///
    /// 与 [`Self::resume`] 一样按 **holder 的** channel 号索引——那是路由表
    /// 的键，不是对端的本地编号。
    pub fn drop_route(&self, holder_channel: u32) {
        self.routes
            .lock()
            .expect("holder routes")
            .remove(&holder_channel);
        self.pending
            .lock()
            .expect("pending")
            .remove(&holder_channel);
    }

    /// 开闸：把压住的帧按序发出去。
    ///
    /// 参数是 [`Self::register_route`] 返回的**票据**，不是 channel 号——
    /// 后者在每条连接内独立编号，按它匹配会误伤别的连接。
    pub fn resume(&self, tickets: Vec<u64>) {
        if tickets.is_empty() {
            return;
        }
        let drained: Vec<(async_channel::Sender<Frame>, u32, Vec<Frame>)> = {
            let mut routes = self.routes.lock().expect("holder routes");
            routes
                .values_mut()
                .filter(|r| tickets.contains(&r.ticket) && r.paused)
                .map(|r| {
                    r.paused = false;
                    let held = std::mem::take(&mut r.held);
                    (r.tx.clone(), r.local_channel, held)
                })
                .collect()
        };
        for (tx, local, held) in drained {
            if held.is_empty() {
                continue;
            }
            let total: usize = held.iter().map(|f| f.payload.len()).sum();
            log::info!("开闸 channel {local}：放行 {total} 字节");
            for frame in held {
                if tx.try_send(frame).is_err() {
                    break;
                }
            }
        }
    }

    /// 发一帧给 holder。
    pub fn send(&self, frame: Frame) -> std::io::Result<()> {
        // 只投递，不等写线程：调用方是连接读线程，绝不能阻塞在 socket 上。
        self.outbox
            .try_send(frame)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "holder 写线程已退出"))
    }

    /// 控制面调用。
    pub fn call(&self, request: Value) -> std::io::Result<Value> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut request = request;
        if let Some(obj) = request.as_object_mut() {
            obj.insert("id".into(), json!(id));
        }
        // 登记 id → 等待者，然后带超时地等。见 `pending_replies` 的说明：
        // 共享队列 + id 不匹配就丢弃的旧写法会让并发 call 互相吞回复，
        // 而且 `recv_blocking` 没有超时会**永久卡住 core 的连接线程**。
        let (tx, rx) = std::sync::mpsc::channel::<Frame>();
        self.pending_replies
            .lock()
            .expect("pending replies")
            .insert(id, tx);
        self.send(Frame::json(serde_json::to_vec(&request)?))?;

        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(frame) => {
                let reply: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                Ok(reply)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "holder 没有回复",
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "与 holder 的连接已断开",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::protocol::Kind;

    /// **登记前到达的帧必须保留帧类型。**
    ///
    /// 回归价值：`attach` 的整屏快照总在回复之前到达，所以需要一个登记前的
    /// 缓冲。缓冲若只留 payload 字节，`Kind::Meta` 就会被当成终端输出补发
    /// ——客户端 VT 里会直接画出 `{"foreground":...}` 这段 JSON，还会把真正
    /// 的画面挤出屏幕。
    #[test]
    fn buffered_frames_keep_their_kind() {
        let mut pending = Pending::default();
        pending.frames.push(Frame::new(1, Kind::Meta, b"{\"foreground\":null}".to_vec()));
        pending.bytes += pending.frames.last().expect("刚推入").payload.len();
        pending.frames.push(Frame::data(1, b"screen".to_vec()));
        pending.bytes += pending.frames.last().expect("刚推入").payload.len();

        // 补发路径把每条帧原样发出（这里直接验证保留的 kind 序列）。
        let kinds: Vec<Kind> = pending.frames.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::Meta, Kind::Data],
            "缓冲不能把 Meta 压成 Data"
        );
        assert!(pending.bytes > 0, "字节计数要跟上，否则 MAX_PENDING 不起作用");
    }

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
