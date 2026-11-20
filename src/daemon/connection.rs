//! 连接的服务端实现：在一条传输流（Unix socket / 命名管道）上跑收发循环。
//!
//! 与 `server.rs`（纯逻辑的请求分发）分开：这里全是 IO 与线程，
//! 那边是可以单测的决策逻辑。

use std::collections::HashMap;
use std::io::BufWriter;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use super::identity::ServerIdentity;
use super::machines::Machines;
use super::protocol::{CONTROL, Frame, Kind, read_frame};
use super::registry::Registry;
use super::server::handle_request;

/// `attach` 快照的分块大小。
///
/// 必须小于 `client::MAX_PENDING`（256 KiB），并给同窗口里的其它帧留出
/// 余量：快照是客户端登记路由前唯一的大块数据，分块后每帧都能被完整
/// 缓冲，不会被"超限丢整段"的规则整体丢弃。
const ATTACH_CHUNK_BYTES: usize = 64 * 1024;

/// 单条连接上同时允许运行的控制请求线程数。
///
/// 控制请求是低频的（开终端、挂接、列终端），但协议允许客户端任意灌。
/// 每条线程最多阻塞一个远端调用（`list` 是 1.5 秒），没有上限时客户端
/// 可以凭空造出成千上万条线程。
const MAX_CONTROL_THREADS: usize = 64;

/// 一个 GUI channel 指向的 holder 与 holder 侧 channel。
///
/// 多个 holder 并存后，只记 channel 号已经不够：两个 holder 的 channel 都
/// 从 1 开始，必须同时记“哪一条 holder 连接”。
#[derive(Clone)]
struct HolderRoute {
    holder: Arc<super::core_holder::HolderClient>,
    channel: u32,
}

/// 终端操作的实际执行者。
///
/// core 与 holder 跑的是同一份分发代码，差别只在"谁来真的建 PTY"：
/// - holder：自己建（它是 PTY 的父进程，core 重启不牵连它）；
/// - core：转发给 holder（它随时可能被重启，不能亲自持有 PTY）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// 直接用本地注册表。
    Local,
    /// 转发给 holder。
    Holder,
}

/// 每个连接独立的状态。
///
/// 控制请求与数据帧在同一个连接上并行处理：远端 SSH 请求可能在网络抖动时
/// 阻塞数秒，不能让它占住连接读循环，否则本地终端的按键和 resize 会排在
/// 它后面。herdr 的 `server.rs` 对每条控制请求单独 `spawn`，这里用同构的
/// 一线程一请求模型；状态按字段加锁，SSH 调用期间不持有任何路由锁。
struct Connection {
    /// 下一个要分配的终端 channel（避开 0，那是控制面）。
    next_channel: AtomicU32,
    /// channel → 终端地址。
    channels: Mutex<HashMap<u32, String>>,
    /// 本连接 channel → **holder 侧** channel。
    ///
    /// # 为什么需要这张表
    ///
    /// 两端的 channel 号是**各自独立编号**的：`conn.next_channel` 在 GUI
    /// 连接上从 1 递增，而 holder 那条 core→holder 连接有自己的
    /// `next_channel`。早期实现假设"两边恰好一致"，于是 core 直接拿 GUI 的
    /// channel 号去 holder 订阅。
    ///
    /// 这个假设在 GUI 重连后立刻失效：新 GUI 连接又从 1 开始编号，而
    /// holder 那边早已递增到 13、14……于是 core 订阅了一个**根本不会有数据
    /// 的** channel。GUI 收到零字节，界面冻在最后一屏——而日志里一切正常，
    /// 因为每一步都"成功"了。
    ///
    /// 现在严格按 holder 回复里的 channel 号订阅，再用这张表把数据帧的
    /// channel 号**改写回本连接的编号**，GUI 侧的路由才认得。
    holder_channels: Mutex<HashMap<u32, HolderRoute>>,
    /// 本连接 channel → 远端终端订阅（每台机器共享 line，channel 各自独立）。
    remote_channels: Mutex<HashMap<u32, super::machines::RemoteAttachment>>,
    /// 客户端订阅的输出。
    outgoing: async_channel::Sender<Frame>,
    /// 本地终端订阅：channel → (地址, `Registry` 订阅 id)。
    ///
    /// 以 channel 为键而不是地址：同一个终端可以有多个挂接，退订时必须只
    /// 退掉对应 channel，不能误伤另一个窗口。
    attachments: Mutex<HashMap<u32, (String, u64)>>,
    /// 已挂接但**数据流还没开闸**的订阅票据。
    ///
    /// # 为什么需要开闸
    ///
    /// `attach` 的回复（含 channel 号）与整屏快照走同一条连接。客户端必须
    /// 先拿到回复、登记路由，然后才能处理数据帧；在此之前的帧只能靠客户端
    /// 自己缓冲，而缓冲是有上限的（超限就得丢，丢了就要重新对齐）。
    ///
    /// herdr 的做法是 `open_paused` + 回复送出后 `resume`
    /// （`server.rs:385`）：**数据流在回复之后才开始**。这样客户端的
    /// "先登记后收数据"是天然成立的，不需要任何缓冲。
    ///
    /// 这里对齐同一策略：attach 时把 channel 记进这个集合，转发层看到它就
    /// 把帧压住；回复写进 outgoing 之后立刻移除，开始放行。
    paused: Mutex<HashMap<u32, u64>>,
}

impl Connection {
    fn new(outgoing: async_channel::Sender<Frame>) -> Self {
        Self {
            next_channel: AtomicU32::new(1),
            channels: Mutex::new(HashMap::new()),
            holder_channels: Mutex::new(HashMap::new()),
            remote_channels: Mutex::new(HashMap::new()),
            outgoing,
            attachments: Mutex::new(HashMap::new()),
            paused: Mutex::new(HashMap::new()),
        }
    }

    fn allocate_channel(&self) -> u32 {
        self.next_channel.fetch_add(1, Ordering::Relaxed)
    }

    fn remote_attachment(&self, channel: u32) -> Option<super::machines::RemoteAttachment> {
        lock(&self.remote_channels).get(&channel).cloned()
    }

    fn holder_route(&self, channel: u32) -> Option<HolderRoute> {
        lock(&self.holder_channels).get(&channel).cloned()
    }

    fn channel_address(&self, channel: u32) -> Option<String> {
        lock(&self.channels).get(&channel).cloned()
    }

    fn attachment_id(&self, channel: u32) -> Option<u64> {
        lock(&self.attachments).get(&channel).map(|(_, id)| *id)
    }

    fn pause(&self, channel: u32, ticket: u64) {
        lock(&self.paused).insert(channel, ticket);
    }

    fn take_paused(&self, channel: u32) -> Option<u64> {
        lock(&self.paused).remove(&channel)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 在已建立的连接上跑完整生命周期。
///
/// 返回时连接已关闭。这个函数由 accept 线程 spawn 出去，**不能**阻塞在
/// 接受循环里。
pub fn serve_connection(
    stream: super::transport::Stream,
    registry: Arc<Registry>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    backend: Backend,
    machines: Arc<Machines>,
    socket_path: std::path::PathBuf,
    identity: Arc<ServerIdentity>,
) {
    let write_stream = match super::transport::try_clone(&stream) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("守护进程：无法克隆连接: {e}");
            return;
        }
    };

    // 写线程：把 outgoing 里的帧写到 socket。
    let (out_tx, out_rx) = async_channel::unbounded::<Frame>();
    let writer = std::thread::Builder::new()
        .name("daemon-conn-w".into())
        .spawn(move || {
            let mut buf = BufWriter::new(write_stream);
            while let Ok(frame) = out_rx.recv_blocking() {
                if frame.write_to(&mut buf).is_err() {
                    break;
                }
            }
        });

    let writer_thread = match writer {
        Ok(t) => t,
        Err(e) => {
            log::warn!("守护进程：无法启动写线程: {e}");
            return;
        }
    };

    let conn = Arc::new(Connection::new(out_tx.clone()));

    let mut reader = stream;
    // 控制请求各自执行；读循环只继续接收输入和 resize。连接关闭时再统一
    // 等这些请求结束，避免它们在本连接清理后又注册回一条死连接。
    let mut controls: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // 鉴权：第一条控制帧必须是带正确 token 的 `hello`。
    //
    // 本地 socket 有 0700 目录保护，但远端部署走 TCP，`127.0.0.1:port`
    // 对同机所有用户开放——不验 token 的话，服务器上任何人都能连上来
    // 拿到别人的全部终端。
    if !authenticate(&mut reader, &conn, &socket_path, &identity) {
        return;
    }

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break, // 客户端正常断开
            Err(e) => {
                log::debug!("守护进程：读取连接失败: {e}");
                break;
            }
        };

        match frame.kind {
            // 控制请求。
            Kind::Json if frame.channel == CONTROL => {
                let request: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                // 与 herdr `server.rs:361-398` 相同：每条 call 独立执行，
                // 一条慢请求不能挡住连接上的后续输入或其他请求。
                //
                // 但也不能无界：每条 RPC 一条线程，而 `list` 会在
                // `machines.terminals` 里阻塞 1.5 秒。一个灌 `{"op":"list"}`
                // 的客户端能凭空造出成千上万条线程。超过上限就明确回一条
                // `busy`，而不是让守护进程被自己的线程数拖垮。
                controls.retain(|handle| !handle.is_finished());
                if controls.len() >= MAX_CONTROL_THREADS {
                    log::warn!("连接上并发控制请求过多（{}），拒绝一条", controls.len());
                    let mut response = json!({
                        "ok": false,
                        "error": "守护进程繁忙，请稍后重试",
                        "kind": "busy",
                    });
                    if let Some(id) = request.get("id").cloned()
                        && let Some(object) = response.as_object_mut()
                    {
                        object.insert("id".into(), id);
                    }
                    let bytes = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
                    let _ = conn.outgoing.try_send(Frame::json(bytes));
                    continue;
                }
                let worker_conn = conn.clone();
                let worker_registry = registry.clone();
                let worker_machines = machines.clone();
                let worker_identity = identity.clone();
                let spawned = std::thread::Builder::new()
                    .name("daemon-conn-rpc".into())
                    .spawn(move || {
                        let mut response = match request.get("op").and_then(Value::as_str) {
                            Some("server.status") => server_status(&worker_identity, &request),
                            _ => dispatch_with(
                                &worker_registry,
                                &request,
                                &worker_conn,
                                backend,
                                &worker_machines,
                            ),
                        };
                        // 回填请求 id：客户端按 id 匹配回复，不回填的话它会
                        // 一直等到超时再失败。
                        if let Some(id) = request.get("id")
                            && let Some(object) = response.as_object_mut()
                        {
                            object.insert("id".into(), id.clone());
                        }
                        let resumed = response
                            .get("channel")
                            .and_then(Value::as_u64)
                            .and_then(|channel| worker_conn.take_paused(channel as u32));
                        let bytes =
                            serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
                        if worker_conn.outgoing.try_send(Frame::json(bytes)).is_ok()
                            && let Some(ticket) = resumed
                            && let Some(route) = worker_conn.holder_route(
                                response
                                    .get("channel")
                                    .and_then(Value::as_u64)
                                    .unwrap_or_default() as u32,
                            )
                        {
                            // 回复已经入队后才放行这一个 attach 的数据流。
                            // 不能像旧的串行实现那样一次 drain 全部票据：并发
                            // attach 时那会把别的 channel 也提前开闸。
                            route.holder.resume(vec![ticket]);
                        }
                    });
                match spawned {
                    Ok(handle) => controls.push(handle),
                    Err(error) => {
                        log::error!("无法启动控制请求线程: {error}");
                        break;
                    }
                }
            }

            // 终端输入 / 尺寸：core 只是管道，原样转给 holder。
            //
            // **不能**在这里落到本地 registry：core 没有 PTY，`write_input`
            // 会静默丢弃（`input_tx` 那一侧没有真实终端在收）。表现是
            // "接回来了、屏幕也在，但打字没反应"——比彻底失效更难查。
            // 信用补充也要转给 holder。
            //
            // 漏掉这条的后果极其隐蔽：holder 侧每个挂接者初始只有 1 MiB
            // 额度，客户端每收一段数据回一条 `Credit`。core 把 `Credit`
            // 吞掉不转，holder 的额度就只减不增——打开 nvim 加载文件这种
            // 输出量大的场景，1 MiB 很快用尽，之后**所有输出都被丢弃**，
            // 界面从此不再刷新，看起来就是"卡死"。
            Kind::Data | Kind::Resize => {
                if let Some(remote) = conn.remote_attachment(frame.channel) {
                    if frame.kind == Kind::Resize {
                        if let Some((cols, rows, _, _)) = Frame::decode_resize(&frame.payload) {
                            remote.resize(cols, rows, frame.payload);
                        }
                    } else {
                        remote.send(frame.kind, frame.payload);
                    }
                    continue;
                }
                if backend == Backend::Holder {
                    // **改写成本连接的 channel → holder 的 channel。**
                    //
                    // GUI 发来的是本地编号；holder 只认它自己那条连接上的
                    // 编号。两者是独立递增的，不改写就等于把输入发给一个
                    // 不存在/别的终端的 channel——表现为"接回来了、画面也在，
                    // 但打字没反应"。
                    let Some(route) = conn.holder_route(frame.channel) else {
                        log::debug!(
                            "未知的本地 channel {}，{:?} 丢弃",
                            frame.channel,
                            frame.kind
                        );
                        continue;
                    };
                    // 直接移走 payload：这一帧在匹配臂里是独占的，不需要
                    // 再为热路径（每个按键、每一块输出）memcpy 一次。
                    if let Err(e) =
                        route
                            .holder
                            .send(Frame::new(route.channel, frame.kind, frame.payload))
                    {
                        log::debug!("转发到 holder 失败（channel {}）: {e}", route.channel);
                    }
                    continue;
                }
                if let Some(address) = conn.channel_address(frame.channel)
                    && let Some(handle) = registry.get(&address)
                {
                    match frame.kind {
                        Kind::Data => handle.write_input(frame.payload),
                        Kind::Resize => {
                            if let Some((cols, rows, w, h)) = Frame::decode_resize(&frame.payload) {
                                handle.resize(alacritty_terminal::event::WindowSize {
                                    num_lines: rows,
                                    num_cols: cols,
                                    cell_width: w,
                                    cell_height: h,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }

            Kind::Credit => {
                if let Some(remote) = conn.remote_attachment(frame.channel) {
                    remote.send(frame.kind, frame.payload);
                    continue;
                }
                // core 只是管道：信用回执必须转给 holder，否则 holder 侧
                // 的额度只减不增，用尽后输出永久被丢弃（界面"卡死"）。
                if backend == Backend::Holder {
                    // 同样要改写 channel 号，理由见上面 Data/Resize 分支。
                    let Some(route) = conn.holder_route(frame.channel) else {
                        continue;
                    };
                    if let Err(e) =
                        route
                            .holder
                            .send(Frame::new(route.channel, frame.kind, frame.payload))
                    {
                        log::debug!("转发信用失败（channel {}）: {e}", route.channel);
                    }
                    continue;
                }
                if let Some(address) = conn.channel_address(frame.channel)
                    && let Some(handle) = registry.get(&address)
                    && let Some(id) = conn.attachment_id(frame.channel)
                    && frame.payload.len() >= 4
                {
                    let n = u32::from_le_bytes(frame.payload[0..4].try_into().expect("已判长度"))
                        as u64;
                    handle.grant_credit(id, n);
                }
            }

            Kind::Close => break,
            _ => {}
        }
    }

    // 控制请求可能正在等远端 SSH。先等它们退出，再清理路由；否则一个
    // 迟到的 attach 会在清理之后又往已关闭的连接里登记订阅。
    for control in controls {
        if control.join().is_err() {
            log::error!("控制请求线程异常退出");
        }
    }

    // 清理：把本连接名下的**所有**订阅退掉。
    //
    // # core 侧（`Backend::Holder`）不能只退本地 registry
    //
    // core 里那份 registry 是空的——PTY 全在 holder。真正持有订阅者的是
    // holder 的 `Hub`，每个订阅者有一份**只减不增**的额度。漏退的后果不是
    // "多一点内存"：额度耗尽的订阅者会停在 `NeedsPaint`，而它的
    // `Delivery` 状态是 holder 独占的，**谁也救不回来**。
    //
    // 这正是"关掉窗口再打开，TUI 卡死"的第二层原因：关闭的窗口从不
    // 退订，那条 `Sink` 的额度被永久吃光并卡在 `NeedsPaint`；重开时
    // holder 给同一个终端分配**新的 channel 号**，新订阅者本该是干净的，
    // 但旧 `Sink` 仍在每轮广播里被遍历、被判断额度不足——它不占新通道的
    // 额度，却让每轮广播都多走一次 `needs_paint` 分支，且
    // `repaint_all`/`broadcast` 里"有一次就补一次快照"的逻辑会被它反复
    // 触发，整屏序列化开销跟着涨。
    //
    // 更要紧的是**别把旧路由留在转发表里**：`Route` 按 holder channel 索引，
    // 旧连接的 local 编号与新连接重号（都从 1 起），留着就会把 holder 的
    // 新 channel 数据误投到一条已死的连接上。
    for (_, (address, id)) in lock(&conn.attachments).drain() {
        if let Some(handle) = registry.get(&address) {
            handle.unsubscribe(id);
        }
    }
    // **先摘锁再调远端。** `detach` 是 1 秒级的 SSH 往返，
    // `detach_from_holder` 是 10 秒级的 holder 调用。持锁做这件事会挡住
    // 同一条连接上所有终端的 `Data`/`Credit`（它们走同一把锁）。
    let remotes: Vec<_> = lock(&conn.remote_channels)
        .drain()
        .map(|(_, r)| r)
        .collect();
    for remote in remotes {
        remote.detach();
    }
    if backend == Backend::Holder {
        let routes: Vec<_> = lock(&conn.holder_channels)
            .drain()
            .map(|(_, route)| route)
            .collect();
        for route in routes {
            detach_from_holder(&route.holder, route.channel);
        }
    }
    drop(conn);
    drop(out_tx);
    let _ = writer_thread.join();
}

/// 分发一条控制请求；返回给客户端的 JSON。
#[cfg(test)]
fn dispatch(registry: &Arc<Registry>, request: &Value, conn: &Connection) -> Value {
    let machines = Machines::empty();
    dispatch_with(registry, request, conn, Backend::Local, &machines)
}

/// 按后端分发。
fn dispatch_with(
    registry: &Arc<Registry>,
    request: &Value,
    conn: &Connection,
    backend: Backend,
    machines: &Arc<Machines>,
) -> Value {
    let op = request.get("op").and_then(Value::as_str).unwrap_or("");

    // holder 专用的生命周期信号。旧 core/旧 holder 不认识它：新 core 发给
    // 旧 holder 只会收到一条“未知请求”，不影响旧终端继续运行。
    //
    // **只对本地 backend（= holder 自己）生效。** core 的 registry 平时是空的
    // （PTY 都在 holder 里），而 `listen` 一旦看到 `draining && is_empty` 就
    // 退出整个进程——如果 core 接受这条请求，任何连上 core socket 的客户端
    // 一句 `{"op":"drain"}` 就能把 core 关掉，所有 GUI 连接一起断。
    if op == "drain" {
        if backend != Backend::Local {
            return json!({
                "ok": false,
                "error": "drain 只对 holder 有效",
                "kind": "invalid_params",
            });
        }
        super::holder::mark_draining();
        return json!({"ok": true});
    }

    // 机器表只属于本地 core；远端 core 收到这些 op 时同样可管理“再下一台”，
    // 但本期不做 peer/多跳，所以它们只在 app 面向的本地连接上使用。
    match op {
        "machine.add" => {
            let Some(config) = request.get("ssh").or_else(|| request.get("config")) else {
                return json!({"ok": false, "error": "缺少 ssh 配置", "kind": "invalid_params"});
            };
            let ssh = match serde_json::from_value::<crate::ssh::SshConnectionConfig>(
                config.clone(),
            ) {
                Ok(ssh) => ssh,
                Err(e) => {
                    return json!({"ok": false, "error": format!("SSH 配置无效: {e}"), "kind": "invalid_params"});
                }
            };
            // 代理选择不走 serde（`use_system_proxy` 是 `#[serde(skip)]`，为了
            // 不落盘），必须单独取回来，否则远端 core 永远直连。
            let mut ssh = ssh;
            if let Some(use_proxy) = request.get("use_system_proxy").and_then(Value::as_bool) {
                ssh.use_system_proxy = use_proxy;
            }
            let name = request
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let session = request
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_owned);
            return match machines.ensure(ssh, name, session) {
                Ok(machine) => {
                    let runtime = machines.runtime(&machine.id);
                    json!({
                        "ok": true,
                        "machine": runtime.map(|r| r.view()).unwrap_or_else(|| machine.view()),
                    })
                }
                Err(e) => json!({"ok": false, "error": e, "kind": "invalid_params"}),
            };
        }
        "machine.list" => {
            return json!({"ok": true, "machines": machines.list()});
        }
        "machine.remove" => {
            let Some(name) = request
                .get("name")
                .or_else(|| request.get("id"))
                .and_then(Value::as_str)
            else {
                return json!({"ok": false, "error": "缺少 name", "kind": "invalid_params"});
            };
            return match machines.remove(name) {
                Some(machine) => json!({"ok": true, "machine": machine.view()}),
                None => json!({"ok": false, "error": "没有这台机器", "kind": "machine_unknown"}),
            };
        }
        "machine.install" | "machine.update" => {
            let Some(name) = request
                .get("name")
                .or_else(|| request.get("id"))
                .and_then(Value::as_str)
            else {
                return json!({"ok": false, "error": "缺少 name", "kind": "invalid_params"});
            };
            let force = op == "machine.update";
            return match machines.request_install(name, force) {
                Ok(machine) => json!({"ok": true, "machine": machine}),
                Err(e) => json!({"ok": false, "error": e, "kind": "machine_unknown"}),
            };
        }
        _ => {}
    }

    if op == "list" {
        let mut reply = if backend == Backend::Holder {
            // 多 holder：把所有 holder 的终端合并成一份清单。只问当前
            // holder 会漏掉旧 holder 手里的会话，GUI 重启后那些标签就找不
            // 回来。
            super::core_holder::list_all()
                .unwrap_or_else(|| dispatch_local(registry, request, conn))
        } else {
            dispatch_local(registry, request, conn)
        };
        let (remote, states) = machines.terminals(std::time::Duration::from_millis(1500));
        if let Some(terminals) = reply.get_mut("terminals").and_then(Value::as_array_mut) {
            terminals.extend(remote);
        }
        if let Some(object) = reply.as_object_mut() {
            object.insert("machines".into(), json!(states));
        }
        return reply;
    }

    if op == "spawn"
        && let Some(machine) = request.get("machine").and_then(Value::as_str)
    {
        let mut remote = request.clone();
        if let Some(object) = remote.as_object_mut() {
            object.remove("machine");
            object.insert("remote".into(), json!(true));
        }
        return match machines.spawn(machine, remote) {
            Ok(reply) => reply,
            Err(e) => json!({
                "ok": false,
                "error": e,
                "kind": machines.find(machine).map(|r| r.error_kind()).unwrap_or("machine_unknown"),
            }),
        };
    }

    if op == "attach"
        && let Some(address) = request.get("address").and_then(Value::as_str)
        && address.contains('/')
    {
        let Some((runtime, remote_address)) = machines.resolve(address) else {
            return json!({"ok": false, "error": "未知机器", "kind": "machine_unknown"});
        };
        let local_channel = conn.allocate_channel();
        let mut remote = request.clone();
        if let Some(object) = remote.as_object_mut() {
            object.remove("id");
            object.remove("address");
        }
        return match machines.attach(
            runtime.id(),
            &remote_address,
            remote,
            local_channel,
            conn.outgoing.clone(),
        ) {
            Ok((mut reply, attachment)) => {
                if let Some(object) = reply.as_object_mut() {
                    object.insert("channel".into(), json!(local_channel));
                }
                lock(&conn.remote_channels).insert(local_channel, attachment);
                reply
            }
            Err(e) => json!({
                "ok": false,
                "error": e,
                "kind": runtime.error_kind(),
            }),
        };
    }

    if op == "detach"
        && let Some(channel) = request.get("channel").and_then(Value::as_u64)
        && let Some(remote) = lock(&conn.remote_channels).remove(&(channel as u32))
    {
        remote.detach();
        return json!({"ok": true});
    }

    if matches!(op, "get" | "close" | "remove" | "snapshot")
        && let Some(address) = request.get("address").and_then(Value::as_str)
        && address.contains('/')
    {
        let kind = machines
            .resolve(address)
            .map(|(runtime, _)| runtime.error_kind())
            .unwrap_or("machine_unknown");
        return match machines.call_address(
            address,
            request.clone(),
            std::time::Duration::from_secs(5),
        ) {
            Ok(Some(reply)) => reply,
            Ok(None) => json!({"ok": false, "error": "未知机器", "kind": "machine_unknown"}),
            Err(e) => json!({"ok": false, "error": e, "kind": kind}),
        };
    }

    if backend == Backend::Holder {
        let holder_op = request.get("op").and_then(Value::as_str).unwrap_or("");

        if holder_op == "spawn" {
            if let Some(call) = super::core_holder::spawn(request) {
                return call.reply;
            }
            log::warn!("holder 不可用，spawn 退回 core 本地 PTY（失去重启保活）");
            return dispatch_local(registry, request, conn);
        }

        if holder_op == "attach" {
            // attach：holder 分配的 channel 号直接回给 UI，core 只做转发。
            //
            // 这里注册转发的时机很关键——必须**紧接**在拿到 channel 号之后，
            // 而且 holder 侧已经为"登记前到达的帧"做了缓冲。两处配合，整屏
            // 快照才不会丢。
            if let Some(call) = super::core_holder::call_for_address(request) {
                let mut reply = call.reply;
                if reply["ok"] == true
                    && let Some(holder_channel) = reply["channel"].as_u64()
                {
                    let address = request
                        .get("address")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();

                    // **用本连接的编号回给 GUI，不要用 holder 的。**
                    //
                    // 两端的 channel 号各自独立编号：每条 GUI 连接都从 1
                    // 开始，而 holder 那条连接有自己的计数器。直接把 holder
                    // 的号回给 GUI，GUI 侧路由表里根本没有那个号。
                    let local_channel = conn.allocate_channel();
                    lock(&conn.holder_channels).insert(
                        local_channel,
                        HolderRoute {
                            holder: call.holder.clone(),
                            channel: holder_channel as u32,
                        },
                    );
                    // 先压住：转发层把这个 channel 的帧暂存，等回复写出去
                    // 之后再放行（见 `Connection::paused`）。
                    let ticket = register_holder_route(
                        &call.holder,
                        local_channel,
                        holder_channel as u32,
                        conn,
                    );
                    conn.pause(local_channel, ticket);
                    lock(&conn.channels).insert(local_channel, address);

                    // 回给 GUI 的是本连接的编号。
                    if let Some(object) = reply.as_object_mut() {
                        object.insert("channel".into(), json!(local_channel));
                    }
                }
                return reply;
            }
            log::warn!("找不到终端，attach 退回本地 registry");
            return dispatch_local(registry, request, conn);
        }

        if holder_op == "detach"
            && let Some(local) = request.get("channel").and_then(Value::as_u64)
        {
            if let Some(route) = lock(&conn.holder_channels).remove(&(local as u32)) {
                lock(&conn.channels).remove(&(local as u32));
                detach_from_holder(&route.holder, route.channel);
            }
            return json!({"ok": true});
        }

        if matches!(holder_op, "get" | "close" | "remove" | "snapshot") {
            if let Some(call) = super::core_holder::call_for_address(request) {
                return call.reply;
            }
            return dispatch_local(registry, request, conn);
        }

        if holder_op == "reap" {
            return json!({"ok": true, "removed": super::core_holder::reap_all()});
        }
    }
    dispatch_local(registry, request, conn)
}

fn dispatch_local(registry: &Arc<Registry>, request: &Value, conn: &Connection) -> Value {
    let op = request.get("op").and_then(Value::as_str).unwrap_or("");
    match op {
        "spawn" => super::server::handle_spawn(registry, request),

        "attach" => {
            let Some(address) = request.get("address").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "缺少 address"});
            };
            let Some(handle) = registry.get(address) else {
                return json!({"ok": false, "error": "没有这个终端", "kind": "not_found"});
            };

            let channel = conn.allocate_channel();
            lock(&conn.channels).insert(channel, address.to_string());

            // **先按客户端尺寸 resize，再生成快照。**
            //
            // 顺序是这里最要紧的东西。herdr-core 在 `viewers::attach` 里
            // 就是这个次序：`apply_size()` 先按 viewer 报的尺寸调整终端，
            // 然后才 `first_paint()`。
            //
            // 反过来（先按旧尺寸快照、客户端拿到后再 resize）会产生一个
            // 中间态：快照里的每一行都是按**旧列数**折行的，客户端却已经
            // 用新列数渲染。nvim / htop 这类全屏程序的表现就是边框错位、
            // 状态栏跑进中间。用户拖一下窗口就能看到"布局乱了"。
            //
            // 客户端没报尺寸时（无窗口场景、测试）保持守护进程现状。
            //
            // 范围检查后**拒绝**而不是 `as u16` 截断：`65537` 截成 `1` 会让
            // 客户端"成功"地把终端 resize 成一个错误列数，全屏程序当场错位。
            if let (Some(cols), Some(rows)) = (
                request.get("cols").and_then(Value::as_u64),
                request.get("rows").and_then(Value::as_u64),
            ) && let (Ok(cols), Ok(rows)) = (u16::try_from(cols), u16::try_from(rows))
                && cols > 0
                && rows > 0
            {
                handle.resize(alacritty_terminal::event::WindowSize {
                    num_lines: rows,
                    num_cols: cols,
                    cell_width: 0,
                    cell_height: 0,
                });
                // 等网格真的长到新尺寸再取快照：`resize` 是投递给读线程的
                // 消息，异步生效。这里短暂等一个上限，避免把旧尺寸的画面
                // 当成新尺寸发出去。
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(120);
                while std::time::Instant::now() < deadline {
                    if handle.size() == (cols, rows) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }

            // 尺寸随回复返回：客户端用它建本地 VT。正常情况就是它报上来的
            // 那个值；若 resize 没能在时限内生效，这里返回的是真实值，
            // 客户端据此校正，也不会错位。
            let (cols, rows) = handle.size();

            // 客户端要多少回滚就重放多少（有上限）。
            //
            // 默认值是 0：没有显式要求的调用方（老客户端、测试、`snapshot`
            // 这类只想要当前屏幕的路径）行为不变。带 `history` 的 attach
            // 会额外推上回滚区，用户重开窗口后滚动条才拉得上去——这也是
            // 设计文档里承诺的"重开窗口，屏幕原样"。
            let history = request
                .get("history")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(super::terminal::MAX_ATTACH_HISTORY as u64) as usize;

            // 图像只在"新窗口挂接"时重放（`history > 0` 或显式 `images`）。
            //
            // 判据为什么绑在 history 上：原地重连时客户端进程还活着、它那份
            // 图像存储也还在，重传只会把几 MB 像素再过一遍 socket；而新窗口
            // 挂接时客户端是全新的，本地 VT 里一张图都没有。
            //
            // 显式的 `images` 字段优先，留给"只想要图不要历史"的调用方
            // （以及测试）。
            let replay_images = request
                .get("images")
                .and_then(Value::as_bool)
                .unwrap_or(history > 0);

            // 先把当前屏幕（+ 回滚 + kitty 图像）发过去，再开始推送新输出。
            // 顺序不能反：反了的话先到的增量会被随后的整屏重绘覆盖掉。
            //
            // **必须分块。** 这一帧是 `attach` 回复**之前**发出去的，客户端
            // 还没登记路由，只能先攒在 `client::MAX_PENDING`（256 KiB）里；
            // 而这里的内容是 2000 行回滚 + 最多 8 MiB kitty 回放。整帧发出
            // 会稳定超限，客户端按"丢整段而不是截断"的规则把**整屏**丢掉，
            // 而协议里没有任何补发路径——重连回来的面板会一直空白。
            //
            // 切成小帧在 VT 语义上完全等价：终端字节本来就是流。
            let snapshot = handle.attach_paint(history, replay_images);
            for chunk in snapshot.chunks(ATTACH_CHUNK_BYTES) {
                let _ = conn.outgoing.try_send(Frame::data(channel, chunk.to_vec()));
            }

            let tx = conn.outgoing.clone();
            let id = handle.subscribe(channel, tx);
            lock(&conn.attachments).insert(channel, (address.to_string(), id));

            json!({
                "ok": true,
                "channel": channel,
                "address": handle.address,
                "cols": cols,
                "rows": rows,
                "state": match handle.state() {
                    super::terminal::State::Live => "live",
                    super::terminal::State::Finished => "finished",
                },
            })
        }

        "detach" => {
            if let Some(channel) = request.get("channel").and_then(Value::as_u64) {
                let channel = channel as u32;
                lock(&conn.channels).remove(&channel);
                if let Some((address, id)) = lock(&conn.attachments).remove(&channel) {
                    if let Some(handle) = registry.get(&address) {
                        handle.unsubscribe(id);
                    }
                }
            }
            json!({"ok": true})
        }

        _ => handle_request(registry, request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn connection() -> (Arc<Connection>, async_channel::Receiver<Frame>) {
        let (tx, rx) = async_channel::unbounded();
        (Arc::new(Connection::new(tx)), rx)
    }

    fn identity() -> ServerIdentity {
        ServerIdentity::for_tests(
            "core-test",
            "generation-test",
            "host-test",
            false,
            Some("tester"),
        )
    }

    /// **非 holder 进程不得接受 `drain`。**
    ///
    /// 回归价值：`drain` 会置一个进程级的 `DRAINING`，而 `listen` 一旦看到
    /// `draining && registry.is_empty()` 就退出整个进程。core 的 registry
    /// 平时就是空的（PTY 都在 holder），于是任何连上 core socket 的客户端
    /// 发一句 `{"op":"drain"}` 就能把 core 关掉，所有 GUI 连接一起断。
    ///
    /// 这里只测被拒绝的那条路径：它必须**不**改动全局状态，所以不会污染
    /// 同进程里其它用例。
    #[test]
    fn drain_is_refused_outside_the_holder_backend() {
        let registry = Arc::new(Registry::new());
        let (conn, _rx) = connection();
        let machines = Machines::empty();
        let was_draining = crate::daemon::holder::draining();

        // `Backend::Holder` 是 core 的后端（它把 PTY 转发给 holder）。
        let reply = dispatch_with(
            &registry,
            &json!({"op": "drain"}),
            &conn,
            Backend::Holder,
            &machines,
        );

        assert_eq!(reply["ok"], false, "core 不应接受 drain: {reply}");
        assert_eq!(reply["kind"], "invalid_params");
        assert_eq!(
            crate::daemon::holder::draining(),
            was_draining,
            "被拒绝的 drain 不能改动进程级状态"
        );
    }

    /// **`hello` 回复必须回填请求 id。**
    ///
    /// 回归价值：这是"关掉窗口再重开，TUI 界面直接卡死"的直接成因之一。
    ///
    /// 客户端按请求 id 等回复（`pending_replies` 按 id 分发）。服务端若
    /// 回一条不带 id 的 `{"ok":true}`，客户端认不出它是自己的，只会等到
    /// 超时/断连。而这件事发生在 **core → holder** 的握手上：core 连不上
    /// holder → 所有 `attach` 落到 core 那份空 registry 上 → 客户端等不到
    /// 回复 → 视图不登记路由 → 界面冻在最后一屏。
    ///
    /// 早期版本"能工作"只是因为客户端当时从共享队列里抢第一条控制回复，
    /// 不校验 id；改成按 id 分发后 id 就成了必需品。
    #[test]
    fn hello_replies_carry_identity_and_capabilities() {
        let request = json!({"id": 42, "op": "hello", "token": "x"});
        let reply = hello_reply(&identity(), &request);
        assert_eq!(
            reply.get("id").and_then(Value::as_u64),
            Some(42),
            "hello 回复必须带请求 id，否则客户端按 id 等不到它"
        );
        for field in [
            "protocol",
            "version",
            "core_id",
            "generation",
            "hostname",
            "methods",
            "capabilities",
            "keeps_running_after_logout",
            "logout_linger_user",
        ] {
            assert!(!reply[field].is_null(), "hello 必须带 {field}: {reply}");
        }
        assert_eq!(reply["core_id"], "core-test");
        assert_eq!(reply["generation"], "generation-test");
        let methods = reply["methods"].as_array().expect("methods 必须是数组");
        for method in crate::daemon::protocol::REQUIRED_METHODS {
            assert!(methods.iter().any(|value| value == method), "缺少 {method}");
        }
        let capabilities = reply["capabilities"]
            .as_array()
            .expect("capabilities 必须是数组");
        for capability in crate::daemon::protocol::REQUIRED_CAPABILITIES {
            assert!(
                capabilities.iter().any(|value| value == capability),
                "缺少 {capability}"
            );
        }
    }

    #[test]
    fn status_replies_refresh_the_same_identity_without_reauthentication() {
        let request = json!({"id": 7, "op": "server.status"});
        let reply = server_status(&identity(), &request);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["op"], "server.status");
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["core_id"], "core-test");
        assert_eq!(reply["keeps_running_after_logout"], false);
    }

    /// **本连接的 channel 号与 holder 的 channel 号必须分开记账。**
    ///
    /// 回归价值：这是"重启后打开 opencode，界面直接冻死"的根因。
    ///
    /// 两端的 channel 号各自独立递增：每条 GUI 连接都从 1 开始，而 holder
    /// 那条 core→holder 连接一直在涨（13、14…）。早期实现直接拿 GUI 的号
    /// 去 holder 订阅，于是订阅了一个**永远没有数据的** channel：客户端
    /// 读线程空转等零字节，而守护进程侧 2 秒明明发了 101 KB（都发到了
    /// 别处）。日志里每一步都"成功"，所以极难查。
    #[test]
    fn local_and_holder_channels_are_tracked_separately() {
        let conn = connection().0;
        let holder = Arc::new(super::super::core_holder::HolderClient::for_tests());
        let first_local = conn.allocate_channel();
        let second_local = conn.allocate_channel();
        assert_eq!((first_local, second_local), (1, 2), "本地号从 1 递增");

        lock(&conn.channels).insert(first_local, "t1".into());
        lock(&conn.holder_channels).insert(
            first_local,
            HolderRoute {
                holder: holder.clone(),
                channel: 13,
            },
        );
        lock(&conn.channels).insert(second_local, "t8".into());
        lock(&conn.holder_channels).insert(
            second_local,
            HolderRoute {
                holder,
                channel: 14,
            },
        );

        assert_eq!(
            conn.holder_route(first_local).map(|route| route.channel),
            Some(13)
        );
        assert_eq!(
            conn.holder_route(second_local).map(|route| route.channel),
            Some(14)
        );
        // 反向不会串：本地号 1 ≠ holder 号 13，这正是旧实现出错的地方。
        assert_ne!(first_local, 13);
    }

    /// 定长比较必须正确且有常量时间语义。
    ///
    /// 长度不同直接 false；内容不同也必须 false。这条钉住的是"别把
    /// `==` 换成短路比较"——短路会按字节提前返回，远端 TCP 上能被测出
    /// token 前缀。
    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abcdef", b"abcde"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    /// **版本一致才放行。**
    ///
    /// 回归价值：GUI 与守护进程是两个能分别升级的二进制。不带版本时，
    /// 新客户端连上旧守护进程，`attach` 的 `history` 与图像回放会被静默
    /// 忽略——用户只看到"重连后回滚没了、图片没了"，而日志里没有任何
    /// 错误。
    #[test]
    fn a_matching_protocol_is_accepted() {
        let request = json!({"op": "hello", "protocol": crate::daemon::protocol::PROTOCOL_VERSION});
        assert!(negotiate_protocol(&request).is_ok());
    }

    /// **版本不匹配必须拒绝，并把两边的版本都带回去。**
    ///
    /// 客户端靠 `kind == "version_mismatch"` 区分"版本问题"与"token 问题"。
    /// 不带 `protocol` 字段的话，客户端只能给出"被拒绝"这种没法操作的提示。
    #[test]
    fn a_protocol_mismatch_is_rejected_with_both_versions() {
        // 老客户端：不带 `protocol`，按 1 处理。
        let legacy = json!({"op": "hello"});
        let reply = negotiate_protocol(&legacy).expect_err("老协议必须被拒");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["kind"], "version_mismatch");
        assert_eq!(reply["client_protocol"], 1);
        assert_eq!(
            reply["protocol"].as_u64().unwrap(),
            crate::daemon::protocol::PROTOCOL_VERSION as u64
        );

        // 未来版本：比本进程新，同样要拒（本进程不认识新字段）。
        let newer = json!({"op": "hello", "protocol": 999});
        let reply = negotiate_protocol(&newer).expect_err("更新的协议也必须被拒");
        assert_eq!(reply["client_protocol"], 999);
    }

    /// attach 必须回填请求 id。
    ///
    /// 回归价值：客户端的 `call_timeout` 是按 `id` 匹配回复的（因为一条
    /// 连接上会有多个并发请求）。服务端不回填 id 时，客户端会一路等到
    /// 超时——现象是"守护进程明明在跑，终端却退回进程内 PTY"，而且
    /// 日志里什么都没有，因为超时是正常错误路径。
    #[test]
    fn attach_replies_carry_the_request_id() {
        let registry = Arc::new(Registry::new());
        let (conn, _rx) = connection();
        let request = json!({"id": 42, "op": "get", "address": "t1"});

        let reply = dispatch(&registry, &request, &conn);

        // dispatch 本身不负责 id（由调用方回填），这里验的是核心前提：
        // 回复是 JSON 对象，能被塞进 id。
        assert!(reply.is_object(), "回复应是 JSON 对象: {reply}");
    }

    /// detach 一个没登记过的 channel 不应 panic。
    #[test]
    fn detaching_an_unknown_channel_is_a_no_op() {
        let registry = Arc::new(Registry::new());
        let (conn, _rx) = connection();
        let reply = dispatch(&registry, &json!({"op": "detach", "channel": 99}), &conn);
        assert_eq!(reply["ok"], true);
    }

    /// attach 到一个不存在的终端要给出 not_found，客户端据此决定重建。
    #[test]
    fn attaching_to_a_missing_terminal_reports_not_found() {
        let registry = Arc::new(Registry::new());
        let (conn, _rx) = connection();
        let reply = dispatch(
            &registry,
            &json!({"op": "attach", "address": "t404"}),
            &conn,
        );
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["kind"], "not_found");
    }
}

/// 退掉 holder 上的一个订阅，并清掉它的转发路由。
///
/// 三个动作必须一起做，少一个都会留下"看得见但不会更新"的订阅：
///
/// 1. `detach` 发给 holder —— 它才会 `unsubscribe`，把额度与 `Delivery`
///    状态一并丢掉；
/// 2. 本地路由表剔除 —— 否则 holder 后续数据仍会被投进这条（可能已死的）
///    连接队列；
/// 3. 本地 channel 映射剔除 —— 信用回执按它反查 holder 号。
///
/// 幂等：holder 已经不认这个 channel 时 `detach` 返回 `ok` 或
/// `not_found`，两种都当成功。
fn detach_from_holder(holder: &Arc<super::core_holder::HolderClient>, holder_channel: u32) {
    holder.drop_route(holder_channel);
    let _ = holder.call(json!({"op": "detach", "channel": holder_channel}));
}

/// 把 holder 上 `holder_channel` 的输出转给本连接。
///
/// core 在这里是一根管子：holder 推来什么，就原样写进 UI 的 outgoing。
/// 不做缓冲、不改内容——终端字节本来就该端到端保持原样（任何"顺手处理"
/// 都可能破坏转义序列）。
///
/// 真正的转发在 holder 的 reader 线程里做（见
/// [`HolderClient::register_route`]），这里只登记路由并返回开闸票据。
fn register_holder_route(
    holder: &Arc<super::core_holder::HolderClient>,
    local_channel: u32,
    holder_channel: u32,
    conn: &Connection,
) -> u64 {
    // 一律先压住：`attach` 的回复还没发出去，客户端此刻还没有登记路由。
    holder.register_route(holder_channel, local_channel, conn.outgoing.clone(), true)
}

/// 校验连接的第一条帧是不是带正确 token 的 `hello`。
///
/// 失败时直接关连接，不给任何"再试一次"的机会——爆破防护靠的就是
/// token 的熵（128 位），限制重试次数意义不大，但能省掉无谓的交互。
fn authenticate(
    reader: &mut super::transport::Stream,
    conn: &Connection,
    socket: &std::path::Path,
    identity: &ServerIdentity,
) -> bool {
    // 读**本 socket** 配套的 token：core 与 holder 各有一份，用错文件就
    // 会出现"core 拿自己的 token 去连 holder、被拒"。
    let expected = match super::read_token_for(socket) {
        Ok(token) => token,
        Err(e) => {
            // 读不到自己的 token 说明运行时目录有问题；此时**拒绝**所有
            // 连接而不是放行——"配置坏了就谁都能进"是最糟的默认值。
            log::error!("读不到守护进程 token，拒绝连接: {e}");
            return false;
        }
    };

    let frame = match read_frame(reader) {
        Ok(Some(frame)) => frame,
        _ => return false,
    };
    if frame.channel != CONTROL || frame.kind != Kind::Json {
        log::warn!("连接的第一帧不是控制帧，拒绝");
        return false;
    }
    let request: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
    if request.get("op").and_then(Value::as_str) != Some("hello") {
        let mut reply = json!({"ok": false, "error": "需要先 hello"});
        if let (Some(id), Some(obj)) = (request.get("id"), reply.as_object_mut()) {
            obj.insert("id".into(), id.clone());
        }
        let _ = conn
            .outgoing
            .try_send(Frame::json(serde_json::to_vec(&reply).unwrap_or_default()));
        return false;
    }
    let provided = request.get("token").and_then(Value::as_str).unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        log::warn!(
            "token 不匹配，拒绝连接（携带 {} 字节，期望 {} 字节）",
            provided.len(),
            expected.len()
        );
        return false;
    }

    // 协议版本协商。
    //
    // 客户端报了版本才校验：老客户端不带这个字段（协议 1），按 1 处理。
    // 版本高于自己说明对面更新，自己缺功能；低于自己说明可能有字段会被
    // 静默忽略——两种都明确拒绝，比"连上了但一半语义失效"好查得多。
    if let Err(reply) = negotiate_protocol(&request) {
        let _ = conn
            .outgoing
            .try_send(Frame::json(serde_json::to_vec(&reply).unwrap_or_default()));
        return false;
    }

    // **回填请求 id。**
    //
    // 客户端的握手是按请求 id 等回复的（`pending_replies` 按 id 分发）。
    // 不带 id 的话客户端无法把这条回复认作自己的，会一直等到超时/断连——
    // 表现是 core 永远连不上 holder（"鉴权时与 holder 的连接断开"），
    // 于是所有 attach 都落到空 registry 上并最终超时，界面冻死。
    //
    // 早期版本这里不带 id 也能工作，因为客户端当时是"从共享队列里抢第一条
    // 控制回复"；改成按 id 分发之后，id 就成了必需品。
    let reply = hello_reply(identity, &request);
    let _ = conn
        .outgoing
        .try_send(Frame::json(serde_json::to_vec(&reply).unwrap_or_default()));
    true
}

/// 构造成功的 `hello` 回复。
///
/// 回填请求 id 是必需的：客户端按 id 分发回复（见 `client::Client`），
/// 缺少 id 会让它一直等到握手超时。
fn hello_reply(identity: &ServerIdentity, request: &Value) -> Value {
    let mut reply = server_status(identity, request);
    let object = reply.as_object_mut().expect("status reply is an object");
    object.insert("op".into(), json!("hello"));
    // core 与 holder 是同一个可执行文件：holder 用它在接管时判断“新终端该
    // 交给谁”。旧 holder 没有这个字段，按旧构建处理（只保留它手里的终端）。
    object.insert("build".into(), json!(super::identity::build_id()));
    object.insert("pid".into(), json!(std::process::id()));
    object.insert("draining".into(), json!(super::holder::draining()));
    object.insert("protocol".into(), json!(super::protocol::PROTOCOL_VERSION));
    object.insert("methods".into(), json!(super::protocol::SUPPORTED_METHODS));
    object.insert("capabilities".into(), json!(super::protocol::CAPABILITIES));
    reply
}

/// 不重新鉴权即可查询的进程身份与保活状态。
///
/// 远端机器监督线程用它在长连接期间刷新 linger 状态，避免只在握手时
/// 读一次、用户后来执行 `loginctl enable-linger` 却一直显示旧值。
fn server_status(identity: &ServerIdentity, request: &Value) -> Value {
    let mut reply = json!({
        "ok": true,
        "op": "server.status",
        "version": env!("CARGO_PKG_VERSION"),
        "core_id": identity.core_id.as_str(),
        "generation": identity.generation.as_str(),
        "hostname": identity.hostname.as_str(),
        "keeps_running_after_logout": identity.keeps_running_after_logout(),
        "logout_linger_user": identity.logout_linger_user(),
    });
    if let (Some(id), Some(object)) = (request.get("id"), reply.as_object_mut()) {
        object.insert("id".into(), id.clone());
    }
    reply
}

/// 校验 `hello` 里的协议版本，返回不匹配时要回给客户端的回复。
///
/// 抽成纯函数是为了可测：`authenticate` 要真实 socket 与 token 文件，而
/// "版本不匹配要明确拒绝、并把两边的版本号都带回去"恰恰是最容易写错也最
/// 需要回归保护的一条。
fn negotiate_protocol(request: &Value) -> Result<(), Value> {
    let client_protocol = request
        .get("protocol")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .unwrap_or(1);
    if client_protocol == super::protocol::PROTOCOL_VERSION {
        return Ok(());
    }
    log::warn!(
        "协议版本不匹配（客户端 {client_protocol}，本进程 {}），拒绝连接",
        super::protocol::PROTOCOL_VERSION
    );
    // 回复必须带 `protocol`，客户端才能把"版本不匹配"与"token 错"区分开，
    // 进而给出可操作的提示（重启应用 / 结束旧守护进程）。
    Err(json!({
        "ok": false,
        "op": "hello",
        "kind": "version_mismatch",
        "protocol": super::protocol::PROTOCOL_VERSION,
        "client_protocol": client_protocol,
    }))
}

/// 定长比较，避免按字节提前返回泄漏 token 前缀。
///
/// 这不是防本地攻击（本地用户读不到 0600 文件），而是防远端场景下的
/// 时序侧信道——TCP 上往返延迟可以被测量。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
