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

use std::sync::Arc;

use gpui::{App, Entity};

use super::client::Client;
use super::protocol::Kind;
use crate::terminal::Terminal;

/// 全局连接。
///
/// `OnceLock` 而不是每次新建：守护进程侧每条连接要占两个线程，几十个
/// 终端各连一次就是上百个线程。
///
/// 用 `Mutex<Option<Arc<Client>>>` 而不是 `OnceLock`：连接会**断**（守护
/// 进程崩溃、被更新、被用户结束），断了必须能换一条新的。`OnceLock`
/// 只写一次，第一次的连接一旦关闭，后面所有调用都会拿到那条死连接，
/// 表现为"守护进程明明重启了，终端却再也接不回来"。
static CLIENT: std::sync::Mutex<Option<Arc<Client>>> = std::sync::Mutex::new(None);

/// 取（必要时建立）全局连接。失败时返回 `None` 并记日志——终端不该因为
/// 守护进程起不来就打不开，调用方应退回普通本地终端。
pub fn client() -> Option<Arc<Client>> {
    let guard = || {
        CLIENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };

    // 已有一条活着的连接就复用（守护进程侧每条连接要占两个线程）。
    if let Some(client) = guard().as_ref()
        && !client.is_closed()
    {
        return Some(client.clone());
    }

    // 连接断了（或从没连过）：重建。`connect_or_start` 会顺带把守护进程
    // 拉起来——它可能刚崩溃或被更新。
    match Client::connect_or_start() {
        Ok(client) => {
            let client = Arc::new(client);
            // 竞态下另一线程可能已经先建好了一条：用它的，我们这条随
            // `Arc` 的最后一个引用一起关闭，不会泄漏。
            let mut slot = guard();
            match slot.as_ref().filter(|existing| !existing.is_closed()) {
                Some(existing) => Some(existing.clone()),
                None => {
                    *slot = Some(client.clone());
                    log::info!("已（重新）建立与终端守护进程的连接");
                    Some(client)
                }
            }
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
    /// 终端在守护进程里的网格尺寸 `(cols, rows)`。
    ///
    /// 客户端**必须**用它建本地 VT。尺寸不一致时，重放的整屏画面会按
    /// 错误的列数折行——nvim / htop 这类全屏程序重连后布局全乱就是这个。
    pub cols: u16,
    pub rows: u16,
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
    spawn_with(cwd, command, args, &[], false, cols, rows)
}

/// 起一个远程（SSH）终端。
///
/// `env` 里可能带 `SSHPASS` 之类的**凭据**：它只随这一次 spawn 请求发出，
/// 守护进程把它交给子进程环境后就不再保留，也不会写进终端记录。
pub fn spawn_remote_terminal(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    spawn_with(None, Some(command), args, env, true, cols, rows)
}

/// 起终端请求的统一实现。
///
/// `remote` 为 true 时守护进程**不套**本地 shell integration：远程主机上没有
/// deepterm 的集成脚本，注入 `ZDOTDIR`/`ENV` 只会把对方的 shell 弄坏。
fn spawn_with(
    cwd: Option<&std::path::Path>,
    command: Option<&str>,
    args: &[String],
    env: &[(String, String)],
    remote: bool,
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
    if !env.is_empty() {
        let map: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v)))
            .collect();
        request["env"] = serde_json::Value::Object(map);
    }
    if remote {
        request["remote"] = serde_json::json!(true);
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

/// 结束守护进程里某个终端里的程序（用户显式关闭标签/面板）。
///
/// 与"断开挂接"是两件事：断开只是不再观看，程序继续跑。
pub fn close_terminal(address: &str) {
    let Some(client) = client() else { return };
    match client.call(serde_json::json!({"op": "close", "address": address})) {
        Ok(reply) if reply["ok"] == true => {
            log::info!("已结束守护进程终端 {address}");
        }
        Ok(reply) => {
            log::warn!("结束终端 {address} 失败: {}", reply["error"]);
        }
        Err(e) => log::warn!("结束终端 {address} 时无法联系守护进程: {e}"),
    }
}

/// 挂接一个已存在的终端。
/// 挂接一个终端。
///
/// `size` 是**客户端当前的网格尺寸**。它必须随 attach 一起发出去：守护
/// 进程会先按这个尺寸 resize 终端，**再**生成快照。反过来（先按旧尺寸
/// 快照、客户端再 resize）会直接导致 TUI 布局错乱——见 herdr-core
/// `viewers.rs::apply_size` 与 `first_paint` 的先后关系。
///
/// 传 `None` 时退回守护进程当前尺寸（用于测试/无窗口场景）。
pub fn attach(address: &str, size: Option<(u16, u16)>) -> Result<Session, String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let mut request = serde_json::json!({"op": "attach", "address": address});
    if let Some((cols, rows)) = size {
        request["cols"] = serde_json::json!(cols);
        request["rows"] = serde_json::json!(rows);
    }
    let reply = client.call(request).map_err(|e| format!("挂接失败: {e}"))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("守护进程拒绝挂接")
            .to_string());
    }
    let channel = reply["channel"]
        .as_u64()
        .ok_or_else(|| "挂接回复缺少 channel".to_string())? as u32;
    // 尺寸缺失时退回一个保守值而不是报错：老版本守护进程不会带这两个字段，
    // 直接失败会让"升级 GUI 但没升级 core"变成打不开终端。
    let cols = reply["cols"].as_u64().unwrap_or(80) as u16;
    let rows = reply["rows"].as_u64().unwrap_or(24) as u16;
    Ok(Session {
        address: address.to_string(),
        channel,
        cols,
        rows,
        client,
    })
}

/// 每轮输出处理的时间片预算。
///
/// 与 herdr 的 `OUTPUT_TIME_PER_TURN`（6ms）同量级：足够把一批 16~64 KiB
/// 的帧吞下，又不会让渲染等太久。
const OUTPUT_TURN_BUDGET: std::time::Duration = std::time::Duration::from_millis(6);

/// 查询某个终端在守护进程里的真实网格尺寸。
///
/// 供会话持久化用：客户端本地 VT 的尺寸对**非活动标签**毫无意义（那些
/// 终端从未布局，`terminal_bounds` 还是构造时的默认值 55×18）。要记住
/// "上次关掉时它是多大"，只能问真正持有它的守护进程。
///
/// 用最短超时：这是保存路径上的一次附加查询，绝不能因为守护进程慢就
/// 把 UI 卡住。查不到就返回 `None`——保存一个缺失值，比存一个错的强。
pub fn terminal_grid(address: &str) -> Option<(u16, u16)> {
    let client = client()?;
    let reply = client
        .call_timeout(
            serde_json::json!({"op": "get", "address": address}),
            std::time::Duration::from_millis(150),
        )
        .ok()?;
    let term = reply.get("terminal")?;
    let cols = term.get("cols")?.as_u64()? as u16;
    let rows = term.get("rows")?.as_u64()? as u16;
    if cols == 0 || rows == 0 {
        return None;
    }
    Some((cols, rows))
}

/// 把一个已挂接的会话接到终端实体上：开一条泵任务，把字节喂给 `Terminal`。
///
/// 返回的句柄不需要保留——任务随实体一起被丢弃。但**调用方要负责在实体
/// 销毁时 `detach`**，否则守护进程会一直往一个没人读的 channel 发数据
/// （虽然它会因发送失败自行清理，但那要等下一次输出）。
pub fn pump_into(session: Session, terminal: Entity<Terminal>, cx: &mut App) {
    let address = session.address.clone();
    let terminal = terminal.downgrade();

    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        // 当前挂接的 channel 与它的输出流，每轮重连后更新。
        let mut incoming = session.client.attach_channel(session.channel);
        let mut current = session;

        'outer: loop {
            // 等第一帧（真正阻塞，没数据时任务睡眠，不占 CPU）。
            // 真正阻塞：没数据时任务睡眠，不占 CPU。
            let Ok(first) = incoming.recv().await else {
                break 'outer;
            };

            // **每轮只吃一个时间片**，然后让出一次事件循环。
            //
            // 这条是防"UI 被输出饿死"的关键。原来写的是
            // `while let Ok(frame) = incoming.recv().await { ... }` —— 只要
            // 还有数据就一路转下去，中间**从不 yield**。nvim 滚动/重绘时
            // 输出是连续的，这个循环于是永不返回：同一条任务链上的重绘、
            // 光标动画、滚动条推进全都拿不到执行机会，用户看到的就是
            // "界面卡住不动"。
            //
            // herdr 用 `output_turn`（`terminal.rs:24`）做同一件事：攒够
            // 一个 6ms 的时间片就把控制权交还出去。
            let deadline = std::time::Instant::now() + OUTPUT_TURN_BUDGET;
            let mut pending = Some(first);
            loop {
                let frame = match pending.take() {
                    Some(frame) => frame,
                    None => {
                        // 时间片用完就停手，剩下的留给下一轮。
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        match incoming.try_recv() {
                            Ok(frame) => frame,
                            Err(_) => break,
                        }
                    }
                };

                match frame.kind {
                    Kind::Data => {
                        let data = frame.payload;
                        let len = data.len();
                        if terminal
                            .update(cx, |terminal, cx| {
                                terminal.handle_remote_data(&data, cx);
                            })
                            .is_err()
                        {
                            // 视图没了：停止观看，但**不要**结束终端。
                            log::debug!("终端 {address} 的视图已销毁，停止挂接");
                            break 'outer;
                        }
                        // 归还流控信用。
                        //
                        // 守护进程侧每个挂接者初始只有 1 MiB 额度，用尽之后
                        // 新的输出会被**丢弃**（它不能让内存无限增长）。不发
                        // 这条回执，额度只减不增——打开 nvim 加载文件这种
                        // 输出量大的场景几秒就把额度耗光，之后界面再也不
                        // 刷新，看起来就是"卡死"。
                        let _ = current
                            .client
                            .credit(current.channel, len.min(u32::MAX as usize) as u32);
                    }
                    Kind::Close => {
                        // 程序真的退出了（不是我们看不到）。
                        log::info!("终端 {address} 的程序已退出");
                        let _ = terminal.update(cx, |terminal, cx| {
                            terminal.handle_program_exit(cx);
                        });
                        break 'outer;
                    }
                    _ => {}
                }
            }

            // 时间片用完 / 队列已空：交还控制权，然后回到外层继续等数据。
            //
            // **必须 `continue`**。这里的 `break` 是内层 loop 的退出，若
            // 直接往下走，就会落进"挂接流断开 → 重新挂接"那条分支——正常
            // 的时间片结束被当成断线，于是每隔 300ms 无谓重连一次，三个
            // 终端把 CPU 空转到 120%。这个 bug 是加时间片时引入的。
            cx.background_executor()
                .timer(std::time::Duration::from_millis(0))
                .await;
            continue 'outer;

            // 走到这里说明流断了——通常是与守护进程的连接没了。
            //
            // **不能**就这样静默停下：终端会永远冻结在最后一屏，用户以为
            // 程序卡死了。要试着挂接回去；守护进程重启过的话原来的地址
            // 已经不存在，`attach` 会返回 not_found，那时才真的放弃。
            log::warn!("终端 {address} 的挂接流断开，尝试重新挂接");

            // 给守护进程一点时间起来（它可能正在被重新拉起）。
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;

            match attach(&address, None) {
                Ok(fresh) => {
                    log::info!("已重新挂接终端 {address}");
                    // 先登记新 channel，**再**替换 `incoming`。
                    //
                    // 顺序反了会形成重连风暴：`attach_channel` 往 routes 里
                    // 插入同名 channel 会**替换**掉旧的发送端，旧接收端的
                    // 所有 sender 随即被丢弃，它的 `recv()` 立刻返回 `Err`
                    // ——正好落回本循环的重连分支，于是 300ms 一次无限重连，
                    // 每个终端每秒烧掉几十次重连请求，CPU 空转到 120%+。
                    let next = fresh.client.attach_channel(fresh.channel);
                    // 旧 channel 一并注销，避免 routes 里留下没人读的条目。
                    if fresh.channel != current.channel {
                        current.client.detach_channel(current.channel);
                    }
                    incoming = next;
                    current = fresh;
                }
                Err(e) => {
                    log::warn!("终端 {address} 无法重新挂接: {e}");
                    // 真接不回来了：告诉用户，别让画面静默冻住。
                    let _ = terminal.update(cx, |terminal, cx| {
                        terminal.handle_connection_lost(cx);
                    });
                    break 'outer;
                }
            }
        }

        // 只 detach 最后一次握着的那个 channel；之前的都随连接一起没了。
        current.detach();
        log::debug!("终端 {address} 的挂接任务结束");
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 挂接不存在的终端必须返回错误，而不是给一个空会话。
    ///
    /// 这条保护重连路径：守护进程重启后所有地址都不存在了，`attach`
    /// 若在这里"成功"返回，泵任务就会挂在一个永远没有数据的 channel 上，
    /// 终端静默冻住——比明确报"接不回来"糟糕得多。
    #[test]
    fn attaching_to_an_unknown_terminal_fails() {
        // SAFETY：本用例独占该变量，指向一个不存在的运行时目录。
        unsafe { std::env::set_var(super::super::RUNTIME_DIR_ENV, "/tmp/deepterm-none-2") };
        let result = attach("t404", None);
        assert!(result.is_err(), "没有守护进程时应报错");
        unsafe { std::env::remove_var(super::super::RUNTIME_DIR_ENV) };
    }

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
