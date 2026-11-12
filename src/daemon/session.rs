//! UI 侧：与守护进程的会话。
//!
//! 全应用共用一条连接（见 [`super::client::Client`] 的说明）。本模块负责：
//!
//! 1. 惰性连接（第一次需要时才拉起守护进程，冷启动不拖慢开窗）；
//! 2. 起终端 / 挂接终端，并把字节泵进对应的 [`Terminal`](crate::terminal::Terminal)；
//! 3. **区分"终端结束"与"连接丢失"**——这是整个功能的关键语义。
//!
//! # 为什么"断开"不等于"结束"
//!
//! 挂接式终端里，守护进程发来的 `Close` 帧表示**程序退出了**；而 socket
//! 本身断掉只表示**我们看不到它了**，程序还在跑。把两者混为一谈，用户会
//! 看到终端莫名其妙变成"已退出"，然后在守护进程还活着的时候又被判成
//! "重连失败"。herdr 用 `Stream::Closed` / `Stream::Lost` 区分这两件事，
//! 这里照做。

use std::sync::{Arc, OnceLock};

use gpui::{App, Entity};

use super::client::Client;
use super::protocol::Kind;
use crate::terminal::Terminal;

/// 全局连接。
///
/// `OnceLock` 而不是每次新建：守护进程侧每条连接要占两个线程，几十个
/// 终端各连一次就是上百个线程。
static CLIENT: OnceLock<Arc<Client>> = OnceLock::new();

/// 取（必要时建立）全局连接。失败时返回 `None` 并记日志——终端不该因为
/// 守护进程起不来就打不开，调用方应退回普通本地终端。
pub fn client() -> Option<Arc<Client>> {
    if let Some(client) = CLIENT.get() {
        // 连接断过就重建：守护进程可能重启过（更新、崩溃）。
        if !client.is_closed() {
            return Some(client.clone());
        }
        log::warn!("与守护进程的连接已断开，重新建立");
    }
    match Client::connect_or_start() {
        Ok(client) => {
            let client = Arc::new(client);
            // 竞态：另一个线程可能先塞进去了。`OnceLock` 不支持替换，
            // 所以两个都能用——多出来的那条连接在被 drop 时自然关闭。
            let stored = CLIENT.get_or_init(|| client.clone());
            Some(stored.clone())
        }
        Err(e) => {
            log::error!("无法连接终端守护进程: {e}");
            None
        }
    }
}

/// 一个已挂接的终端会话。
///
/// 可 `Clone`：输入/尺寸回调各持一份，这样它们不需要回头查"我是哪个
/// channel"——查表那种写法在 detach 之后会静默写错终端。
#[derive(Clone)]
pub struct Session {
    pub address: String,
    pub channel: u32,
    client: Arc<Client>,
}

impl Session {
    /// 往终端写键入。
    pub fn write(&self, bytes: &[u8]) {
        if let Err(e) = self.client.input(self.channel, bytes.to_vec()) {
            log::warn!("向终端 {} 写入失败: {e}", self.address);
        }
    }

    /// 通知尺寸。
    pub fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        if let Err(e) = self
            .client
            .resize(self.channel, cols, rows, cell_width, cell_height)
        {
            log::debug!("终端 {} 尺寸通知失败: {e}", self.address);
        }
    }

    /// 停止观看（**不**结束终端）。
    pub fn detach(&self) {
        let _ = self.client.call(serde_json::json!({
            "op": "detach",
            "channel": self.channel,
        }));
        self.client.detach_channel(self.channel);
    }
}

/// 在守护进程里起一个新终端并返回它的地址。
pub fn spawn_terminal(
    cwd: Option<&std::path::Path>,
    command: Option<&str>,
    args: &[String],
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let mut request = serde_json::json!({
        "op": "spawn",
        "cols": cols,
        "rows": rows,
    });
    if let Some(cwd) = cwd {
        request["cwd"] = serde_json::json!(cwd.to_string_lossy());
    }
    if let Some(command) = command {
        request["shell"] = serde_json::json!(command);
        request["args"] = serde_json::json!(args);
    }
    let reply = client
        .call(request)
        .map_err(|e| format!("守护进程没有响应: {e}"))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("守护进程拒绝创建终端")
            .to_string());
    }
    reply["address"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "守护进程没有返回终端地址".to_string())
}

/// 挂接一个已存在的终端。
pub fn attach(address: &str) -> Result<Session, String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let reply = client
        .call(serde_json::json!({"op": "attach", "address": address}))
        .map_err(|e| format!("挂接失败: {e}"))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("守护进程拒绝挂接")
            .to_string());
    }
    let channel = reply["channel"]
        .as_u64()
        .ok_or_else(|| "挂接回复缺少 channel".to_string())? as u32;
    Ok(Session {
        address: address.to_string(),
        channel,
        client,
    })
}

/// 把一个已挂接的会话接到终端实体上：开一条泵任务，把字节喂给 `Terminal`。
///
/// 返回的句柄不需要保留——任务随实体一起被丢弃。但**调用方要负责在实体
/// 销毁时 `detach`**，否则守护进程会一直往一个没人读的 channel 发数据
/// （虽然它会因发送失败自行清理，但那要等下一次输出）。
pub fn pump_into(session: Session, terminal: Entity<Terminal>, cx: &mut App) {
    let channel = session.channel;
    let incoming = session.client.attach_channel(channel);
    let address = session.address.clone();
    let terminal = terminal.downgrade();

    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        while let Ok(frame) = incoming.recv().await {
            let alive = match frame.kind {
                Kind::Data => {
                    let data = frame.payload;
                    terminal
                        .update(cx, |terminal, cx| {
                            terminal.handle_remote_data(&data, cx);
                        })
                        .is_ok()
                }
                Kind::Close => {
                    // 程序真的退出了（不是我们看不到）。
                    log::info!("终端 {address} 的程序已退出");
                    let _ = terminal.update(cx, |terminal, cx| {
                        terminal.handle_program_exit(cx);
                    });
                    break;
                }
                _ => true,
            };
            if !alive {
                // 终端实体已经没了：停止观看，但**不要**结束终端。
                log::debug!("终端 {address} 的视图已销毁，停止挂接");
                break;
            }
        }
        log::debug!("终端 {address} 的挂接流已结束");
    })
    .detach();

    // 会话本身不参与后续流程，挂接已由上面的任务接管；显式丢弃即可，
    // `Session` 没有 `Drop`，不会去 detach 掉刚建立的挂接。
    drop(session);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拿不到守护进程时必须返回错误而不是 panic。
    ///
    /// 这条保护的是降级路径：守护进程起不来（比如只读的 HOME、受限沙箱）
    /// 时，终端仍然应该能退回普通本地 PTY 打开，而不是整个应用崩掉。
    #[test]
    fn spawning_without_a_daemon_reports_an_error() {
        // SAFETY：本用例独占该变量，指向一个不存在的运行时目录。
        unsafe { std::env::set_var(super::super::RUNTIME_DIR_ENV, "/tmp/deepterm-none") };
        let result = spawn_terminal(None, None, &[], 80, 24);
        assert!(result.is_err(), "没有守护进程时应报错");
        unsafe { std::env::remove_var(super::super::RUNTIME_DIR_ENV) };
    }
}
