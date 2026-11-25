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

/// 重挂退避：与 `daemon::machines` 的拨号退避一致。连接暂时失败时快速
/// 重试，长时间断网后降到低频，避免每个终端各烧一条重连风暴。
const RECONNECT_MIN: std::time::Duration = std::time::Duration::from_millis(300);
const RECONNECT_MAX: std::time::Duration = std::time::Duration::from_secs(30);

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

/// 一个终端视图当前绑定的 daemon 会话。
///
/// 视图先建立、连接后建立时，输入和尺寸回调不能捕获某一次的 `Session`：
/// 断线重挂会换 channel，旧 channel 上的键入会静默丢失。这里与 herdr 的
/// `Grid { channel: Option<u32> }` 等价——终端持有“当前会话”，attach/重挂成功
/// 后原位替换，回调永远写到最新 channel。
#[derive(Clone, Default)]
pub struct SessionHandle {
    state: Arc<std::sync::Mutex<SessionHandleState>>,
}

#[derive(Default)]
struct SessionHandleState {
    current: Option<Session>,
    /// 最后一次客户端尺寸。视图在 attach 完成前也会收到布局 resize；
    /// 那些通知没有 channel 可写，不能直接丢掉，否则恢复后会一直按旧网格
    /// 显示，直到用户再次拖动窗口。绑定新会话时补发最新值。
    size: Option<(u16, u16, u16, u16)>,
}

impl SessionHandle {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionHandleState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 装上新会话；同一视图内的重挂也走这里。
    pub fn bind(&self, session: Session) {
        let pending_size = {
            let mut state = self.lock();
            state.current = Some(session.clone());
            state.size
        };
        let Some((cols, rows, cell_width, cell_height)) = pending_size else {
            return;
        };
        // 网格已经一致时**不要**再发一次 resize。
        //
        // 刚 attach 回来的会话本来就是这个尺寸，补发一次会让守护进程做一次
        // 多余的 reflow，并触发一整屏重绘；那份重绘与客户端本地 VT 的网格
        // 之间还可能存在时序竞态（重绘先到、本地布局后到），正是"恢复瞬间
        // 画面花一下"的来源。只有视图在 attach 期间真的被改过尺寸才需要补。
        if (cols, rows) == (session.cols, session.rows) {
            return;
        }
        session.resize(cols, rows, cell_width, cell_height);
    }

    /// 连接彻底不可用时停止把输入发往已经不存在的 channel。
    pub fn clear(&self) {
        self.lock().current = None;
    }

    fn clone_current(&self) -> Option<Session> {
        self.lock().current.clone()
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Some(session) = self.clone_current() {
            session.write(bytes);
        }
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        let session = {
            let mut state = self.lock();
            state.size = Some((cols, rows, cell_width, cell_height));
            state.current.clone()
        };
        if let Some(session) = session {
            session.resize(cols, rows, cell_width, cell_height);
        }
    }

    /// 本视图最后一次向守护进程声明的网格。
    ///
    /// 供会话持久化使用：保存路径（每次自动保存、复制标签页、分屏）不该为了
    /// 一个尺寸去做同步 RPC——`terminal_grid` 是 150ms 超时的阻塞调用，而
    /// 这些路径都在 UI 线程上。这里返回的值正是 daemon 被 resize 成的那一份，
    /// 与远端真实网格一致。
    pub fn grid(&self) -> Option<(u16, u16)> {
        self.lock().size.map(|(cols, rows, _, _)| (cols, rows))
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

/// 把一台 SSH 主机加入机器表，并返回包含运行时状态的机器视图。
///
/// 认证配置只是交给本地 core；core 仍走 DeepTerm 原有的 russh / Keychain /
/// 主机密钥校验路径，不会改用系统 `ssh` 命令。
pub fn add_machine(
    config: &crate::ssh::SshConnectionConfig,
    name: Option<String>,
    session: Option<String>,
) -> Result<serde_json::Value, String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let mut request = serde_json::json!({
        "op": "machine.add",
        "ssh": config,
        // `SshConnectionConfig::use_system_proxy` 是 `serde(skip)` 的（它只是
        // 这一次连接的选择，不该写进 `ssh.json`），所以上面那份序列化出来的
        // 配置里恒为 false。不单独带一个字段过去，core 就永远按直连处理
        // ——「使用系统代理连接」在远端 core 路径上会静默失效。
        "use_system_proxy": config.use_system_proxy,
    });
    if let Some(name) = name {
        request["name"] = serde_json::json!(name);
    }
    if let Some(session) = session {
        request["session"] = serde_json::json!(session);
    }
    let reply = client
        .call(request)
        .map_err(|e| format!("添加机器失败: {e}"))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("守护进程拒绝添加机器")
            .to_string());
    }
    Ok(reply["machine"].clone())
}

/// 从机器表里删掉一台机器。
///
/// 对端 core 换了身份（`kind: rebuilt`）时，唯一正确的恢复路径是
/// “删掉旧条目 + 用同一份 SSH 配置重新添加”：新条目的 `remote_core_id` 从
/// 空开始，第一次握手就会记下对端当前的身份。保留旧 id 硬改是不行的——
/// 那等于把两个不同的 core 当成同一个，它名下的终端地址会指向不存在的东西。
pub fn remove_machine(name: &str) -> Result<(), String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let reply = client
        .call(serde_json::json!({"op": "machine.remove", "name": name}))
        .map_err(|e| format!("删除机器失败: {e}"))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("守护进程拒绝删除机器")
            .to_string());
    }
    Ok(())
}

/// 当前机器表（每项含 `name` / `ssh` / `remote_core_id` / `state` 等）。
pub fn list_machines() -> Result<Vec<serde_json::Value>, String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let reply = client
        .call(serde_json::json!({"op": "machine.list"}))
        .map_err(|e| format!("读取机器表失败: {e}"))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("守护进程拒绝列出机器")
            .to_string());
    }
    Ok(reply["machines"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// 一条机器视图是否指向给定的 SSH 目标。
///
/// 逐字段比较，不用 `SshConnectionConfig::id()`：那个 id 把昵称也算进去，
/// 而这里要找的正是“同一个目标的所有条目”，昵称恰恰是不该参与的部分。
/// 字段在机器视图里是平铺的（见 `Machine::view`），不是嵌在 `ssh` 下面。
///
/// 抽成纯函数是为了可测：真正调用会走 RPC 删条目，单测里碰不到。
fn targets_same_host(machine: &serde_json::Value, config: &crate::ssh::SshConnectionConfig) -> bool {
    machine.get("host").and_then(serde_json::Value::as_str) == Some(config.host.as_str())
        && machine
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(22)
            == config.port as u64
        && machine.get("username").and_then(serde_json::Value::as_str)
            == Some(config.username.as_str())
}

/// 删掉指向同一个 SSH 目标的机器条目，返回删掉的条目数。
///
/// 按 **SSH 目标**而不是名字查找，因为 `MachineBook::ensure` 在名字冲突时
/// 会自动加后缀（`host` → `host-2`）。用户给的昵称对不上实际条目名时，
/// 按名删会静默失败，而 `ensure` 随后又会把新身份并进那条旧记录里——
/// 结果是 rebuilt 永远自愈不了。
fn forget_target_machine(config: &crate::ssh::SshConnectionConfig) -> usize {
    let Ok(machines) = list_machines() else {
        return 0;
    };
    let mut removed = 0;
    for machine in machines {
        if !targets_same_host(&machine, config) {
            continue;
        }
        let Some(name) = machine["name"].as_str() else {
            continue;
        };
        match remove_machine(name) {
            Ok(()) => {
                removed += 1;
                log::info!("远端 core 身份已变化，已清掉旧机器条目 {name}");
            }
            // 条目本来就不在不是失败：下面的添加会重建它。
            Err(error) => log::info!("清掉旧机器条目 {name} 未成功: {error}"),
        }
    }
    removed
}

/// 在远端 core 里创建终端，返回 `machine/tN` 地址。
///
/// 首次连接可能要建立 SSH、拉起远端 core，因此超时比普通控制请求长；
/// 不能让 app 在本地 core 仍等待远端握手时先超时。
pub fn spawn_machine_terminal(
    machine: &str,
    cwd: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let mut request = serde_json::json!({
        "op": "spawn",
        "machine": machine,
        "cols": cols,
        "rows": rows,
    });
    if let Some(cwd) = cwd.filter(|cwd| !cwd.trim().is_empty()) {
        request["cwd"] = serde_json::json!(cwd);
    }
    let reply = client
        .call_timeout(request, crate::daemon::machines::MACHINE_SPAWN_TIMEOUT)
        .map_err(|e| format!("远端终端没有响应: {e}"))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("远端 core 拒绝创建终端")
            .to_string());
    }
    reply["address"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "远端 core 没有返回终端地址".to_string())
}

/// 守护进程对「远端 core 换了身份」给出的 `kind`。
///
/// 这是 [`restore_machine_terminal`] 需要单独处理的第三条路径：不是“终端
/// 没了”，也不是“临时连不上”，而是对端已是另一个 core、旧身份整体作废。
pub const REBUILT_MACHINE_MARKER: &str = "kind: rebuilt";

pub(crate) fn is_rebuilt_machine_error(error: &str) -> bool {
    error.contains(REBUILT_MACHINE_MARKER)
}

/// 对端 core 换了身份之后清掉旧条目。
///
/// 只删不建：调用方随后会走“添加机器 + 新建终端”的正常路径。新条目的
/// `remote_core_id` 是 `None`，第一次握手就会记下对端当前身份，不需要客户端
/// 猜。必须删掉而不能只改 `remote_core_id`——机器名由 `MachineBook::ensure`
/// 按 SSH 目标去重，旧条目留着会让新身份被并进那条旧记录里。
fn forget_rebuilt_machine(config: &crate::ssh::SshConnectionConfig) {
    if forget_target_machine(config) == 0 {
        // 没有条目可删同样是正常情况：本地 core 刚重启、或用户已经删过。
        log::info!(
            "远端 core 身份已变化，没有可清理的旧条目；直接重新添加 {}",
            config.host_label()
        );
    }
}

/// 恢复一个远端终端：优先接回会话里保存的 `machine/tN`，接不上再新建。
///
/// 这个函数会做阻塞的 daemon RPC（包括首次部署远端 core 时的 SSH 握手），
/// 因此**只能从后台线程调用**。UI 线程负责在结果返回后装配 `TerminalView`。
///
/// 保存的地址仍然存在时必须优先复用：那正是“关掉 GUI 再重开，远端的
/// `opencode` / 编译任务还在原来的屏幕上”这一能力的核心。只有守护进程
/// 明确回答 `not_found` 才新建；超时、连接中断等错误直接上报，避免把一次
/// 临时拥塞误判成“终端已消失”，从而额外拉起一个重复的 shell。
///
/// 对端 core 换了身份（[`REBUILT_MACHINE_MARKER`]）是第三条路径：那不是
/// “终端没了”，也不是“临时连不上”，而是旧身份整体作废。此时清掉同目标的
/// 机器条目，让底部的“添加机器 + 新建终端”用对端当前身份重来一次。
pub fn restore_machine_terminal(
    config: &crate::ssh::SshConnectionConfig,
    name: Option<String>,
    cwd: Option<&str>,
    address: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<Session, String> {
    if let Some(address) = address {
        match attach_with_history(address, Some((cols, rows)), DEFAULT_ATTACH_HISTORY) {
            Ok(session) => {
                log::info!("已接回远端终端 {address}");
                return Ok(session);
            }
            Err(error) if is_missing_terminal_error(&error) => {
                log::info!("远端终端 {address} 已不存在，将新建终端: {error}");
            }
            Err(error) if is_unknown_machine_error(&error) => {
                // 本地 core 可能刚重启，机器表还没装进运行时。先按保存的
                // SSH 配置重新登记，再挂**原地址**；地址本身仍是终端身份，
                // 不能因为一次 core 重启就新建一个 shell。
                log::info!("远端机器尚未恢复，重新登记后接回 {address}: {error}");
                add_machine(config, name.clone(), None)?;
                match attach_with_history(address, Some((cols, rows)), DEFAULT_ATTACH_HISTORY) {
                    Ok(session) => {
                        log::info!("已接回远端终端 {address}");
                        return Ok(session);
                    }
                    Err(error) if is_missing_terminal_error(&error) => {
                        log::info!("远端终端 {address} 已不存在，将新建终端: {error}");
                    }
                    Err(error) => {
                        return Err(format!("接回远端终端 {address} 失败: {error}"));
                    }
                }
            }
            Err(error) if is_rebuilt_machine_error(&error) => {
                // 对端换了一个 core：旧地址属于**那个** core，接不回来了。
                // 清掉旧条目后落到底部的“添加+新建”路径，不要拿旧地址再试。
                log::warn!("{error}");
                forget_rebuilt_machine(config);
            }
            Err(error) => {
                return Err(format!("接回远端终端 {address} 失败: {error}"));
            }
        }
    }

    let machine = add_machine(config, name, None)?;
    let machine_name = machine["name"]
        .as_str()
        .ok_or_else(|| "守护进程没有返回机器名".to_string())?;
    let new_address = spawn_machine_terminal(machine_name, cwd, cols, rows)?;
    match attach_with_history(&new_address, Some((cols, rows)), DEFAULT_ATTACH_HISTORY) {
        Ok(session) => Ok(session),
        Err(error) => {
            // 新终端已经建出来却挂不上时，不能把它留在远端占着 PTY；
            // 用户看不到它，也没有任何入口再关闭它。
            close_terminal(&new_address);
            Err(format!(
                "远端终端 {new_address} 已创建，但挂接失败: {error}"
            ))
        }
    }
}

/// 守护进程对已消失终端给出的稳定错误文案。
///
/// `attach_with_history` 为了兼容旧 core 目前只保留文本错误；这里做一次
/// 窄匹配，**不能**把所有 attach 错误都当成“不存在”，否则一次临时超时
/// 就会多开一个远端终端。
/// 守护进程对已消失终端给出的稳定错误标记。
///
/// 与人类可读文案分开：`attach_with_history` 会把守护进程的 `kind` 原样拼
/// 进错误串（见 [`describe_attach_failure`]），下游据此判定"真的不存在"
/// 还是"临时连不上"。只认 `kind` 字段，不再匹配中文文案。
pub const MISSING_TERMINAL_MARKER: &str = "kind: not_found";

/// 把 `attach` 的失败回复压成一个错误串，保留 `kind` 供下游判别。
fn describe_attach_failure(reply: &serde_json::Value) -> String {
    let error = reply["error"].as_str().unwrap_or("守护进程拒绝挂接");
    match reply["kind"].as_str() {
        Some(kind) => format!("{error}（kind: {kind}）"),
        None => error.to_string(),
    }
}

pub(crate) fn is_missing_terminal_error(error: &str) -> bool {
    error.contains(MISSING_TERMINAL_MARKER)
        || error.contains("没有这个终端")
        || error.contains("not_found")
}

fn is_unknown_machine_error(error: &str) -> bool {
    error.contains("未知机器") || error.contains("unknown machine")
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

/// 挂接一个终端。
///
/// `size` 是**客户端当前的网格尺寸**。它必须随 attach 一起发出去：守护
/// 进程会先按这个尺寸 resize 终端，**再**生成快照。反过来（先按旧尺寸
/// 快照、客户端再 resize）会直接导致 TUI 布局错乱——见 herdr-core
/// `viewers.rs::apply_size` 与 `first_paint` 的先后关系。
///
/// 传 `None` 时退回守护进程当前尺寸（用于测试/无窗口场景）。
///
/// 默认**不要**回滚历史。这个入口同时服务于"连接断了重新挂接"：那时
/// 客户端本地 VT 里已经有断开前的回滚，再要一份会把它翻倍。只有"新窗口
/// 挂接"（本地 VT 是空的）才该用 [`attach_with_history`]。
pub fn attach(address: &str, size: Option<(u16, u16)>) -> Result<Session, String> {
    attach_with_history(address, size, 0)
}

/// 要回放多少行回滚。与守护进程的上限一致（`MAX_ATTACH_HISTORY`）。
///
/// 2000 行是设计文档承诺的量级：足够覆盖"关掉窗口前在看的那一屏 pager /
/// 编译输出"，又不至于让 attach 变成一次几 MB 的传输。
pub const DEFAULT_ATTACH_HISTORY: usize = 2000;

/// 远端 attach 要允许本地 core 先重拨 SSH、再等首帧穿过慢链路。
///
/// 本地 core 的最坏路径是等待新 line（30 秒）加远端 attach（30 秒），
/// 所以 app 侧必须比它更长；否则 app 先超时重试，会同时留下多条未完成的
/// attach，反而制造更多快照和在途数据。本机 attach 仍用默认的 10 秒。
const REMOTE_ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(70);

/// 带显式回滚行数的挂接。
///
/// `history = 0` 表示只要当前屏幕（测试与"重连时不想看见旧内容"的场景）。
pub fn attach_with_history(
    address: &str,
    size: Option<(u16, u16)>,
    history: usize,
) -> Result<Session, String> {
    let client = client().ok_or_else(|| "守护进程不可用".to_string())?;
    let mut request = serde_json::json!({
        "op": "attach",
        "address": address,
        "history": history,
    });
    if let Some((cols, rows)) = size {
        request["cols"] = serde_json::json!(cols);
        request["rows"] = serde_json::json!(rows);
    }
    let timeout = if address.contains('/') {
        REMOTE_ATTACH_TIMEOUT
    } else {
        std::time::Duration::from_secs(10)
    };
    let reply = client
        .call_timeout(request, timeout)
        .map_err(|e| format!("挂接失败: {e}"))?;
    if reply["ok"] != true {
        // **把守护进程给的 `kind` 一起带回去。**
        //
        // 旧实现只保留 `error` 文案，调用方（`restore_machine_terminal`、
        // `pump_into`）只能靠 `contains("没有这个终端")` 猜——改一句措辞就
        // 会把"终端已消失"判成"临时故障"，于是多开一个远端 shell，或者
        // 让重连循环永远转下去。
        return Err(describe_attach_failure(&reply));
    }
    // 守护进程回来的数字都是不可信输入：`as u16` 会把 65537 截成 1，
    // 于是客户端"成功"地按 1 列布局整屏。范围外一律当缺失处理（走下面的
    // 保守默认值），而不是截断。
    let channel = u32::try_from(
        reply["channel"]
            .as_u64()
            .ok_or_else(|| "挂接回复缺少 channel".to_string())?,
    )
    .map_err(|_| "挂接回复的 channel 超出 u32".to_string())?;
    // 尺寸缺失时退回一个保守值而不是报错：老版本守护进程不会带这两个字段，
    // 直接失败会让"升级 GUI 但没升级 core"变成打不开终端。
    let cols = reply["cols"]
        .as_u64()
        .and_then(|cols| u16::try_from(cols).ok())
        .unwrap_or(80)
        .max(1);
    let rows = reply["rows"]
        .as_u64()
        .and_then(|rows| u16::try_from(rows).ok())
        .unwrap_or(24)
        .max(1);
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
    // 同上：不可信数字不做截断转换，越界直接当"查不到"。
    let cols = term
        .get("cols")?
        .as_u64()
        .and_then(|cols| u16::try_from(cols).ok())?;
    let rows = term
        .get("rows")?
        .as_u64()
        .and_then(|rows| u16::try_from(rows).ok())?;
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
pub fn pump_into(
    session: Session,
    terminal: Entity<Terminal>,
    handle: SessionHandle,
    cx: &mut App,
) {
    let address = session.address.clone();
    let terminal = terminal.downgrade();
    handle.bind(session.clone());

    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        // 当前挂接的 channel 与它的输出流，每轮重连后更新。
        let mut incoming = session.client.attach_channel(session.channel);
        let mut current = session;
        let mut reconnect_delay = RECONNECT_MIN;

        'outer: loop {
            // 等第一帧（真正阻塞，没数据时任务睡眠，不占 CPU）。
            //
            // 流断了要走到下面的重连分支，**不能**静默退出：终端会永久
            // 冻结在最后一屏，用户以为程序卡死了。
            let mut stream_lost = false;

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
            match incoming.recv().await {
                Ok(first) => {
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
                                    // 队列空 ≠ 断线：只是这一轮读完了。
                                    Err(async_channel::TryRecvError::Empty) => break,
                                    // 接收端真的没了：守护进程崩溃/被更新，
                                    // 必须走重连，而不是当成"时间片结束"。
                                    Err(async_channel::TryRecvError::Closed) => {
                                        stream_lost = true;
                                        break;
                                    }
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
                            Kind::Meta => {
                                // 守护进程报来的前台进程信息。挂接式终端没有
                                // PTY fd，客户端自己算不出这个，只能靠它。
                                if let Ok(meta) = serde_json::from_slice::<
                                    crate::daemon::terminal::TerminalMeta,
                                >(&frame.payload)
                                {
                                    let _ = terminal.update(cx, |terminal, cx| {
                                        terminal.apply_daemon_meta(meta, cx);
                                    });
                                }
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
                }
                Err(_) => stream_lost = true,
            }

            if !stream_lost {
                // 时间片用完 / 队列已空：交还控制权，然后回到外层继续等数据。
                //
                // 正常的时间片结束**不能**落进下面的重连分支：那会让每个
                // 终端每 300ms 无谓重连一次，三个终端把 CPU 空转到 120%。
                // 这个 bug 是加时间片时引入的，因此用 `stream_lost` 把
                // 「队列空了」与「接收端没了」区分开。
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(0))
                    .await;
                continue 'outer;
            }

            // 走到这里说明流断了——通常是与守护进程的连接没了。
            //
            // **不能**就这样静默停下：终端会永远冻结在最后一屏，用户以为
            // 程序卡死了。要试着挂接回去；守护进程重启过的话原来的地址
            // 已经不存在，`attach` 会返回 not_found，那时才真的放弃。
            log::warn!("终端 {address} 的挂接流断开，尝试重新挂接");
            // Herdr 在流丢失后保持终端视图，由窗口轮询对**同一地址**持续
            // reattach；这里在泵任务内做等价退避重挂。网络、机器运行时和
            // 本地 core 都可能按不同节奏恢复，只尝试一次仍会让终端永久停在
            // 最后一屏。
            handle.clear();
            loop {
                // 视图已经关掉就没必要继续拨号；也避免任务在标签关闭后永久
                // 持有重试循环。
                if terminal.update(cx, |_, _| {}).is_err() {
                    break 'outer;
                }
                // 给守护进程/机器运行时一点时间恢复。`attach` 是阻塞 RPC，
                // 首次重拨还可能等 SSH 握手，必须放在后台 executor。
                cx.background_executor().timer(reconnect_delay).await;
                let attach_address = address.clone();
                let attached = cx
                    .background_executor()
                    .spawn(async move { attach(&attach_address, None) })
                    .await;
                match attached {
                    Ok(fresh) => {
                        log::info!("已重新挂接终端 {address}");
                        // 先登记新 channel，**再**替换 `incoming`。顺序反了会
                        // 让旧接收端立刻关闭并再次进入重连分支，形成风暴。
                        //
                        // 用 `attach` 而不是 `attach_with_history`：本地 VT
                        // 已保留断开前的回滚，再次回放会让历史成倍增长。
                        let next = fresh.client.attach_channel(fresh.channel);
                        if fresh.channel != current.channel {
                            current.client.detach_channel(current.channel);
                        }
                        handle.bind(fresh.clone());
                        incoming = next;
                        current = fresh;
                        reconnect_delay = RECONNECT_MIN;
                        break;
                    }
                    Err(e) if is_missing_terminal_error(&e) => {
                        log::warn!("终端 {address} 已不存在，停止重挂: {e}");
                        // 程序确实不在了，不能把“连接失败”伪装成仍在重连。
                        let _ = terminal.update(cx, |terminal, cx| {
                            terminal.handle_connection_lost(cx);
                        });
                        handle.clear();
                        break 'outer;
                    }
                    Err(e) => {
                        log::warn!(
                            "终端 {address} 暂时无法重新挂接: {e}；{}ms 后重试",
                            reconnect_delay.as_millis()
                        );
                        reconnect_delay = (reconnect_delay * 2).min(RECONNECT_MAX);
                    }
                }
            }
        }

        // 只 detach 最后一次握着的那个 channel；之前的都随连接一起没了。
        // detach 会走控制面 RPC，不能在 UI 线程等一个可能已经失联的 core。
        let last = current.clone();
        cx.background_executor()
            .spawn(async move { last.detach() })
            .await;
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
        let _env = super::super::IsolatedTestEnv::new("/tmp/deepterm-none-2");
        let result = attach("t404", None);
        assert!(result.is_err(), "没有守护进程时应报错");
    }

    /// 拿不到守护进程时必须返回错误而不是 panic。
    ///
    /// 这条保护的是降级路径：守护进程起不来（比如只读的 HOME、受限沙箱）
    /// 时，终端仍然应该能退回普通本地 PTY 打开，而不是整个应用崩掉。
    ///
    /// 必须同时禁掉"自动拉起"：查找 `deepcore` 的兜底是 PATH，而装了正式版
    /// 的机器 PATH 里就有 `/Applications/DeepTerm.app/Contents/MacOS`。只隔离
    /// 运行时目录的话，这条用例会真的起一个**生产守护进程**，然后在断言处
    /// 反向失败，还留下一个跑着真实 shell 的进程。
    #[test]
    fn spawning_without_a_daemon_reports_an_error() {
        let _env = super::super::IsolatedTestEnv::new("/tmp/deepterm-none");
        let result = spawn_terminal(None, None, &[], 80, 24);
        assert!(result.is_err(), "没有守护进程时应报错");
    }

    /// attach 完成前收到的布局 resize 不能丢：绑定会话后必须还能拿到
    /// 最后一次真实尺寸并补发。否则连接期间用户改过窗口，远端会一直停在
    /// attach 时的旧网格，直到下一次 resize。
    #[test]
    fn session_handle_keeps_the_latest_resize_before_binding() {
        let handle = SessionHandle::new();
        handle.resize(126, 50, 0, 0);
        handle.resize(126, 43, 2, 3);
        assert_eq!(handle.lock().size, Some((126, 43, 2, 3)));

        handle.clear();
        assert_eq!(
            handle.lock().size,
            Some((126, 43, 2, 3)),
            "clear 只断开当前会话，最新尺寸要留给下一次 bind"
        );
    }

    /// 重建机器时按**目标**找条目，不看昵称。
    ///
    /// `ensure` 在名字冲突时会加后缀（`host` → `host-2`），所以用户给的昵称
    /// 未必等于条目名。按名删会静默漏掉那条旧记录，然后 `ensure` 又把新身份
    /// 并进旧记录里——`rebuilt` 就永远自愈不了。
    #[test]
    fn rebuilding_matches_entries_by_target_not_by_nickname() {
        let config = crate::ssh::SshConnectionConfig::new("10.0.0.9", "me", crate::ssh::config::AuthMethod::Agent);

        let entry = |host: &str, port: u64, user: &str| {
            serde_json::json!({
                "name": "whatever-the-book-called-it",
                "host": host,
                "port": port,
                "username": user,
                "nickname": "a-name-the-user-typed",
            })
        };

        assert!(targets_same_host(&entry("10.0.0.9", 22, "me"), &config));
        // 昵称与条目名不一致不影响判定。
        assert!(targets_same_host(&entry("10.0.0.9", 22, "me"), &config));

        // 主机、端口、用户任一不同都不是同一目标；误删会毁掉用户的其它机器。
        assert!(!targets_same_host(&entry("10.0.0.10", 22, "me"), &config));
        assert!(!targets_same_host(&entry("10.0.0.9", 2222, "me"), &config));
        assert!(!targets_same_host(&entry("10.0.0.9", 22, "someone"), &config));
    }

    /// 缺字段的旧条目按 22 端口处理，不能因为一次 serde 变化就删不掉。
    #[test]
    fn a_machine_without_a_port_is_port_22() {
        let config = crate::ssh::SshConnectionConfig::new("h", "u", crate::ssh::config::AuthMethod::Agent);
        let machine = serde_json::json!({"name": "h", "host": "h", "username": "u"});
        assert!(targets_same_host(&machine, &config));
    }
}
