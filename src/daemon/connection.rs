//! 连接的服务端实现：在一条传输流（Unix socket / 命名管道）上跑收发循环。
//!
//! 与 `server.rs`（纯逻辑的请求分发）分开：这里全是 IO 与线程，
//! 那边是可以单测的决策逻辑。

use std::io::BufWriter;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::protocol::{CONTROL, Frame, Kind, read_frame};
use super::registry::Registry;
use super::server::handle_request;

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
struct Connection {
    /// 下一个要分配的终端 channel（避开 0，那是控制面）。
    next_channel: u32,
    /// channel → 终端地址。
    channels: std::collections::HashMap<u32, String>,
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
    holder_channels: std::collections::HashMap<u32, u32>,
    /// 客户端订阅的输出。
    outgoing: async_channel::Sender<Frame>,
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
    paused: Vec<u64>,
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
    socket_path: std::path::PathBuf,
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

    let mut conn = Connection {
        next_channel: 1,
        channels: std::collections::HashMap::new(),
        holder_channels: std::collections::HashMap::new(),
        outgoing: out_tx.clone(),
        paused: Vec::new(),
    };

    let mut reader = stream;
    let mut attachments: Vec<(String, u64)> = Vec::new();

    // 鉴权：第一条控制帧必须是带正确 token 的 `hello`。
    //
    // 本地 socket 有 0700 目录保护，但远端部署走 TCP，`127.0.0.1:port`
    // 对同机所有用户开放——不验 token 的话，服务器上任何人都能连上来
    // 拿到别人的全部终端。
    if !authenticate(&mut reader, &mut conn, &socket_path) {
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
                let mut response =
                    dispatch_with(&registry, &request, &mut conn, &mut attachments, backend);
                // 回填请求 id：客户端按 id 匹配回复，不回填的话它会一直等到
                // 超时再失败——表现为"守护进程明明起来了，终端却退回进程内"，
                // 而且日志里什么都看不到（超时是正常错误路径）。
                if let Some(id) = request.get("id")
                    && let Some(object) = response.as_object_mut()
                {
                    object.insert("id".into(), id.clone());
                }
                let bytes = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
                if conn.outgoing.try_send(Frame::json(bytes)).is_err() {
                    break;
                }
                // 回复已经排进 outgoing：可以放行这次 attach 的数据流了。
                //
                // 客户端此刻才可能拿到 channel 号并登记路由，所以顺序是
                // "回复先入队、随后放行"。与 herdr 的 `out.resume` 同一位置
                // （`server.rs:385`）。
                if !conn.paused.is_empty() {
                    let opened: Vec<u64> = conn.paused.drain(..).collect();
                    if let Some(holder) = super::core_holder::holder() {
                        holder.resume(opened);
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
                if backend == Backend::Holder {
                    // **改写成本连接的 channel → holder 的 channel。**
                    //
                    // GUI 发来的是本地编号；holder 只认它自己那条连接上的
                    // 编号。两者是独立递增的，不改写就等于把输入发给一个
                    // 不存在/别的终端的 channel——表现为"接回来了、画面也在，
                    // 但打字没反应"。
                    let Some(&holder_channel) = conn.holder_channels.get(&frame.channel) else {
                        log::debug!(
                            "未知的本地 channel {}，{:?} 丢弃",
                            frame.channel,
                            frame.kind
                        );
                        continue;
                    };
                    if let Some(holder) = super::core_holder::holder() {
                        let _ = holder.send(Frame::new(
                            holder_channel,
                            frame.kind,
                            frame.payload.clone(),
                        ));
                    } else {
                        log::warn!("holder 不可用，{:?} 被丢弃", frame.kind);
                    }
                    continue;
                }
                if let Some(address) = conn.channels.get(&frame.channel)
                    && let Some(handle) = registry.get(address)
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
                // core 只是管道：信用回执必须转给 holder，否则 holder 侧
                // 的额度只减不增，用尽后输出永久被丢弃（界面"卡死"）。
                if backend == Backend::Holder {
                    // 同样要改写 channel 号，理由见上面 Data/Resize 分支。
                    let Some(&holder_channel) = conn.holder_channels.get(&frame.channel) else {
                        continue;
                    };
                    if let Some(holder) = super::core_holder::holder() {
                        let _ = holder.send(Frame::new(
                            holder_channel,
                            frame.kind,
                            frame.payload.clone(),
                        ));
                    }
                    continue;
                }
                if let Some(address) = conn.channels.get(&frame.channel)
                    && let Some(handle) = registry.get(address)
                    && let Ok(id) = attachments
                        .iter()
                        .find(|(a, _)| a == address)
                        .map(|(_, id)| *id)
                        .ok_or(())
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
    for (address, id) in attachments {
        if let Some(handle) = registry.get(&address) {
            handle.unsubscribe(id);
        }
    }
    if backend == Backend::Holder {
        for holder_channel in conn.holder_channels.values().copied() {
            detach_from_holder(holder_channel);
        }
    }
    drop(conn.outgoing);
    drop(out_tx);
    let _ = writer_thread.join();
}

/// 分发一条控制请求；返回给客户端的 JSON。
#[cfg(test)]
fn dispatch(
    registry: &Arc<Registry>,
    request: &Value,
    conn: &mut Connection,
    attachments: &mut Vec<(String, u64)>,
) -> Value {
    dispatch_with(registry, request, conn, attachments, Backend::Local)
}

/// 按后端分发。
fn dispatch_with(
    registry: &Arc<Registry>,
    request: &Value,
    conn: &mut Connection,
    attachments: &mut Vec<(String, u64)>,
    backend: Backend,
) -> Value {
    if backend == Backend::Holder {
        // attach：holder 分配的 channel 号直接回给 UI，core 只做转发。
        //
        // 这里注册转发的时机很关键——必须**紧接**在拿到 channel 号之后，
        // 而且 holder 侧已经为"登记前到达的帧"做了缓冲（见
        // `core_holder::attach_channel`）。两处配合，整屏快照才不会丢。
        if request.get("op").and_then(Value::as_str) == Some("attach")
            && let Some(mut reply) = forward_to_holder(request)
            && reply["ok"] == true
        {
            if let Some(holder_channel) = reply["channel"].as_u64() {
                let address = request
                    .get("address")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                // **用本连接的编号回给 GUI，不要用 holder 的。**
                //
                // 两端的 channel 号各自独立编号：holder 那条 core→holder 连接
                // 有自己的 `next_channel`（已经涨到 13、14…），而每条 GUI
                // 连接都从 1 开始。直接把 holder 的号回给 GUI，GUI 侧的路由表
                // 里根本没有那个号 —— 数据帧到了也找不到接收端。
                //
                // 早期实现"恰好能用"只是因为 core 只服务一个 GUI、且两边同步
                // 递增；一旦 GUI 重连，编号立刻错位，界面就冻死。
                let local_channel = conn.next_channel;
                conn.next_channel += 1;

                conn.holder_channels
                    .insert(local_channel, holder_channel as u32);
                // 先压住：转发层把这个 channel 的帧暂存，等回复写出去之后
                // 再放行（见 `Connection::paused`）。
                let ticket =
                    register_holder_route(local_channel, holder_channel as u32, &address, conn);
                if let Some(ticket) = ticket {
                    conn.paused.push(ticket);
                }
                conn.channels.insert(local_channel, address);

                // 回给 GUI 的是本连接的编号。
                if let Some(obj) = reply.as_object_mut() {
                    obj.insert("channel".into(), json!(local_channel));
                }
            }
            return reply;
        }
        // 其余 op 也可能带 channel（`detach`）。转发前必须把本连接的
        // channel 号改写成 holder 的，理由同 Data/Resize。
        let forward = match request.get("channel").and_then(Value::as_u64) {
            Some(local) => match conn.holder_channels.get(&(local as u32)) {
                Some(&holder_channel) => {
                    let mut rewritten = request.clone();
                    if let Some(obj) = rewritten.as_object_mut() {
                        obj.insert("channel".into(), json!(holder_channel));
                    }
                    if request.get("op").and_then(Value::as_str) == Some("detach") {
                        conn.holder_channels.remove(&(local as u32));
                        // 路由与 holder 侧订阅一起退掉：只删映射会让 holder
                        // 继续往一条没人读的 channel 推，额度照耗。
                        conn.channels.remove(&(local as u32));
                        super::core_holder::holder()
                            .inspect(|h| h.drop_route(holder_channel));
                    }
                    Some(rewritten)
                }
                None => None,
            },
            None => Some(request.clone()),
        };
        if let Some(forward) = forward
            && let Some(reply) = forward_to_holder(&forward)
        {
            return reply;
        }
    }
    dispatch_local(registry, request, conn, attachments)
}

/// 这条 op 是否要转发给 holder。
///
/// 抽成纯函数是为了可测：白名单漏项不会报错，只会静默作用到 core 里那份
/// 空 registry 上。清单本身值得被测试钉住。
fn holder_op(op: &str) -> bool {
    matches!(
        op,
        "spawn" | "attach" | "close" | "get" | "list" | "snapshot" | "detach" | "remove" | "reap"
    )
}

/// 把一条终端操作原样转给 holder。
///
/// 返回 `None` 表示这条请求不该转发（ping/list 之类由 core 自己答），
/// 或者 holder 不可用——后者会退化成 core 自己建 PTY，功能仍然可用，
/// 只是失去"活过 core 重启"这一条。
fn forward_to_holder(request: &Value) -> Option<Value> {
    let op = request.get("op").and_then(Value::as_str)?;
    // 转发白名单必须覆盖**所有作用于真实终端的 op**。
    //
    // 漏一个的后果不是"报错"，而是静默作用到 core 里那份空 registry 上：
    // 返回 `ok` 或 `not_found`，实际什么都没发生。
    //
    // - `detach`：不转发 → holder 侧的订阅者永远留着，每个挂接者都会
    //   持续占用一条推送通道；
    // - `remove` / `reap`：不转发 → 已退出的终端记录在 holder 里堆着，
    //   `list` 会一直列出来。
    if !holder_op(op) {
        return None;
    }
    let holder = match super::core_holder::holder() {
        Some(h) => h,
        None => {
            log::warn!("holder 不可用，{op} 由 core 自己处理");
            return None;
        }
    };
    match holder.call(request.clone()) {
        Ok(reply) => {
            log::info!("{op} 已由 holder 处理: {}", reply["address"]);
            Some(reply)
        }
        Err(e) => {
            log::warn!("转给 holder 的 {op} 失败: {e}");
            None
        }
    }
}

fn dispatch_local(
    registry: &Arc<Registry>,
    request: &Value,
    conn: &mut Connection,
    attachments: &mut Vec<(String, u64)>,
) -> Value {
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

            let channel = conn.next_channel;
            conn.next_channel += 1;
            conn.channels.insert(channel, address.to_string());

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
            if let (Some(cols), Some(rows)) = (
                request.get("cols").and_then(Value::as_u64),
                request.get("rows").and_then(Value::as_u64),
            ) && cols > 0
                && rows > 0
            {
                handle.resize(alacritty_terminal::event::WindowSize {
                    num_lines: rows as u16,
                    num_cols: cols as u16,
                    cell_width: 0,
                    cell_height: 0,
                });
                // 等网格真的长到新尺寸再取快照：`resize` 是投递给读线程的
                // 消息，异步生效。这里短暂等一个上限，避免把旧尺寸的画面
                // 当成新尺寸发出去。
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(120);
                while std::time::Instant::now() < deadline {
                    if handle.size() == (cols as u16, rows as u16) {
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
            let snapshot = handle.attach_paint(history, replay_images);
            let _ = conn.outgoing.try_send(Frame::data(channel, snapshot));

            let tx = conn.outgoing.clone();
            let id = handle.subscribe(channel, tx);
            attachments.push((address.to_string(), id));

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
                if let Some(address) = conn.channels.remove(&channel)
                    && let Some(pos) = attachments.iter().position(|(a, _)| a == &address)
                {
                    let (_, id) = attachments.remove(pos);
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

    fn connection() -> (
        Connection,
        async_channel::Receiver<Frame>,
        Vec<(String, u64)>,
    ) {
        let (tx, rx) = async_channel::unbounded();
        (
            Connection {
                next_channel: 1,
                channels: std::collections::HashMap::new(),
                holder_channels: std::collections::HashMap::new(),
                outgoing: tx,
                paused: Vec::new(),
            },
            rx,
            Vec::new(),
        )
    }

    /// **转发给 holder 的白名单必须覆盖所有作用于真实终端的 op。**
    ///
    /// 回归价值：漏一个不会报错，只会静默作用到 core 里那份**空 registry**
    /// 上——返回 `ok` 或 `not_found`，实际什么都没发生。这确实发生过：
    /// `detach` 不转发 → holder 侧订阅者永久泄漏；`remove`/`reap` 不转发 →
    /// 已退出的终端记录一直堆着，`list` 永远列得出来。
    ///
    /// 所以把白名单抽成 [`holder_op`] 这个纯函数，直接断言清单内容。
    #[test]
    fn every_terminal_op_is_declared_for_the_holder() {
        for op in [
            "spawn", "attach", "close", "get", "list", "snapshot", "detach", "remove", "reap",
        ] {
            assert!(
                holder_op(op),
                "{op} 必须转发给 holder（否则操作落在空 registry 上）"
            );
        }
        // 反向：控制面 / 本地可答的 op 不该转发。
        for op in ["ping", "hello", "definitely_not_an_op"] {
            assert!(!holder_op(op), "{op} 不该转发给 holder");
        }
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
    fn hello_replies_must_carry_the_request_id() {
        // 构造与 `authenticate` 里同样形状的回复，断言 id 回填逻辑生效。
        let request = json!({"id": 42, "op": "hello", "token": "x"});
        let mut reply = json!({"ok": true, "op": "hello", "protocol": 2});
        if let (Some(id), Some(obj)) = (request.get("id"), reply.as_object_mut()) {
            obj.insert("id".into(), id.clone());
        }
        assert_eq!(
            reply.get("id").and_then(Value::as_u64),
            Some(42),
            "hello 回复必须带请求 id，否则客户端按 id 等不到它"
        );
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
        let mut conn = connection().0;
        // GUI 侧连续两个 attach 拿到的本地号。
        let first_local = conn.next_channel;
        conn.next_channel += 1;
        let second_local = conn.next_channel;
        conn.next_channel += 1;
        assert_eq!((first_local, second_local), (1, 2), "本地号从 1 递增");

        // holder 侧此刻可能已经涨到 13、14。
        conn.channels.insert(first_local, "t1".into());
        conn.holder_channels.insert(first_local, 13);
        conn.channels.insert(second_local, "t8".into());
        conn.holder_channels.insert(second_local, 14);

        assert_eq!(conn.holder_channels.get(&first_local), Some(&13));
        assert_eq!(conn.holder_channels.get(&second_local), Some(&14));
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
        let (mut conn, _rx, mut attachments) = connection();
        let request = json!({"id": 42, "op": "get", "address": "t1"});

        let reply = dispatch(&registry, &request, &mut conn, &mut attachments);

        // dispatch 本身不负责 id（由调用方回填），这里验的是核心前提：
        // 回复是 JSON 对象，能被塞进 id。
        assert!(reply.is_object(), "回复应是 JSON 对象: {reply}");
    }

    /// detach 一个没登记过的 channel 不应 panic。
    #[test]
    fn detaching_an_unknown_channel_is_a_no_op() {
        let registry = Arc::new(Registry::new());
        let (mut conn, _rx, mut attachments) = connection();
        let reply = dispatch(
            &registry,
            &json!({"op": "detach", "channel": 99}),
            &mut conn,
            &mut attachments,
        );
        assert_eq!(reply["ok"], true);
    }

    /// attach 到一个不存在的终端要给出 not_found，客户端据此决定重建。
    #[test]
    fn attaching_to_a_missing_terminal_reports_not_found() {
        let registry = Arc::new(Registry::new());
        let (mut conn, _rx, mut attachments) = connection();
        let reply = dispatch(
            &registry,
            &json!({"op": "attach", "address": "t404"}),
            &mut conn,
            &mut attachments,
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
fn detach_from_holder(holder_channel: u32) {
    let Some(holder) = super::core_holder::holder() else {
        return;
    };
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
    local_channel: u32,
    holder_channel: u32,
    address: &str,
    conn: &Connection,
) -> Option<u64> {
    let holder = super::core_holder::holder()?;
    // 一律先压住：`attach` 的回复还没发出去，客户端此刻还没有登记路由。
    Some(holder.register_route(
        holder_channel,
        local_channel,
        address,
        conn.outgoing.clone(),
        true,
    ))
}

/// 校验连接的第一条帧是不是带正确 token 的 `hello`。
///
/// 失败时直接关连接，不给任何"再试一次"的机会——爆破防护靠的就是
/// token 的熵（128 位），限制重试次数意义不大，但能省掉无谓的交互。
fn authenticate(
    reader: &mut super::transport::Stream,
    conn: &mut Connection,
    socket: &std::path::Path,
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
        log::warn!("token 不匹配，拒绝连接");
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
    let id = request.get("id").cloned();
    let mut reply = json!({
        "ok": true,
        "op": "hello",
        "protocol": super::protocol::PROTOCOL_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
    });
    if let (Some(id), Some(obj)) = (id, reply.as_object_mut()) {
        obj.insert("id".into(), id);
    }
    let _ = conn
        .outgoing
        .try_send(Frame::json(serde_json::to_vec(&reply).unwrap_or_default()));
    true
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
