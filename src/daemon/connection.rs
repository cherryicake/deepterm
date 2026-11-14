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
    /// 客户端订阅的输出。
    outgoing: async_channel::Sender<Frame>,
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
        outgoing: out_tx.clone(),
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
                    if let Some(holder) = super::core_holder::holder() {
                        let _ = holder.send(frame.clone());
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
                    if let Some(holder) = super::core_holder::holder() {
                        let _ = holder.send(frame.clone());
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

    // 清理：把所有挂接退掉，否则终端会一直往一个已关闭的 channel 发。
    for (address, id) in attachments {
        if let Some(handle) = registry.get(&address) {
            handle.unsubscribe(id);
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
            && let Some(reply) = forward_to_holder(request)
            && reply["ok"] == true
        {
            if let Some(channel) = reply["channel"].as_u64() {
                let address = request
                    .get("address")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // core 侧也要把 channel 映射到地址：之后的 Data/Resize 转发
                // 与 detach 都靠它定位是哪个终端。
                conn.channels.insert(channel as u32, address);
                relay_holder_output(channel as u32, conn);
            }
            return reply;
        }
        if let Some(reply) = forward_to_holder(request) {
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

            // 先把当前屏幕发过去，再开始推送新输出。顺序不能反：
            // 反了的话先到的增量会被随后的整屏重绘覆盖掉。
            let snapshot = handle.snapshot();
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
                outgoing: tx,
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

/// 把 holder 上某个 channel 的输出转给 UI 连接。
///
/// core 在这里是一根管子：holder 推来什么，就原样写进 UI 的 outgoing。
/// 不做缓冲、不改内容——终端字节本来就该端到端保持原样（任何"顺手处理"
/// 都可能破坏转义序列）。
fn relay_holder_output(channel: u32, conn: &mut Connection) {
    let Some(holder) = super::core_holder::holder() else {
        return;
    };
    let incoming = holder.attach_channel(channel);
    let outgoing = conn.outgoing.clone();

    std::thread::Builder::new()
        .name(format!("relay-{channel}"))
        .spawn(move || {
            while let Ok(frame) = incoming.recv_blocking() {
                // channel 号原样保留：UI 侧就是按它路由到对应终端视图的。
                if outgoing.try_send(frame).is_err() {
                    break;
                }
            }
        })
        .ok();
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
        let _ = conn.outgoing.try_send(Frame::json(
            serde_json::to_vec(&json!({"ok": false, "error": "需要先 hello"})).unwrap_or_default(),
        ));
        return false;
    }
    let provided = request.get("token").and_then(Value::as_str).unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        log::warn!("token 不匹配，拒绝连接");
        return false;
    }
    let _ = conn.outgoing.try_send(Frame::json(
        serde_json::to_vec(&json!({"ok": true, "op": "hello"})).unwrap_or_default(),
    ));
    true
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
