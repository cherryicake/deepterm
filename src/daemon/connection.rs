//! 连接的服务端实现：在一条 `UnixStream` 上跑收发循环。
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
    stream: std::os::unix::net::UnixStream,
    registry: Arc<Registry>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) {
    let write_stream = match stream.try_clone() {
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
                let response = dispatch(&registry, &request, &mut conn, &mut attachments);
                let bytes = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
                if conn.outgoing.try_send(Frame::json(bytes)).is_err() {
                    break;
                }
            }

            // 终端输入。
            Kind::Data => {
                if let Some(address) = conn.channels.get(&frame.channel)
                    && let Some(handle) = registry.get(address)
                {
                    handle.write_input(frame.payload);
                }
            }

            Kind::Resize => {
                if let Some(address) = conn.channels.get(&frame.channel)
                    && let Some(handle) = registry.get(address)
                    && let Some((cols, rows, w, h)) = Frame::decode_resize(&frame.payload)
                {
                    handle.resize(alacritty_terminal::event::WindowSize {
                        num_lines: rows,
                        num_cols: cols,
                        cell_width: w,
                        cell_height: h,
                    });
                }
            }

            Kind::Credit => {
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
fn dispatch(
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
