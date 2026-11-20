//! 远端机器表与每台机器一条的 core line。
//!
//! 目标结构与 herdr 相同：本地 core 只持有连接和镜像，终端进程与历史都在
//! 远端 core（它的 holder 里）。一条 line 的字节流是：
//!
//! ```text
//! 本地 core ── russh exec ──▶ 远端 `deepcore connect` ──socket──▶ 远端 core
//! ```
//
//! 这里刻意不复用系统的 `ssh` 命令：认证必须继续走 DeepTerm 已有的
//! Keychain / 密码 / 主机密钥校验路径，而那条路径在 `crate::ssh` 的 russh
//! 实现里。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ssh::SshConnectionConfig;
use crate::ssh::russh_terminal::{SshClientHandler, connect_and_authenticate};

use super::client::Client;
use super::deploy::{self, InstallMode, InstallOutcome};
use super::protocol::{Frame, Kind};

const RECONNECT_MIN: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const LOGIN_RECONNECT_STEPS: [Duration; 3] = [
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(600),
];
/// 一条已经建立的 line 完成协议握手的时间。
const REMOTE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// 首次连接还要 SSH 握手、认证、安装并启动远端 core。20 秒会把正常的慢认证
/// 误判成失败；失败页再切旧 SSH 就会静默丢掉保活语义。
pub(crate) const MACHINE_START_TIMEOUT: Duration = Duration::from_secs(60);
/// app → 本地 core 的整次 spawn 请求必须比 supervisor 的等待更长，否则 UI 会
/// 在本地 core 仍等待远端时先超时。
pub(crate) const MACHINE_SPAWN_TIMEOUT: Duration = Duration::from_secs(70);
/// 已连接 line 上一次普通调用的上限。
///
/// 10 秒只够低延迟网络。实测跨 Wi-Fi 丢包时，远端 core 已经回复、但
/// 11 KiB 的首帧穿过 SSH 就可能超过 10 秒；此时若继续复用这条 line，
/// 每次 attach 都会超时，界面永远停在失败页。真正无响应的 line 仍由
/// [`MachineRuntime::invalidate_line`] 主动关闭并重连。
const REMOTE_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// 一条 line 多久没有任何真实字节就算死。
///
/// herdr 用 `machine_timeout(30s) + heartbeat_every(15s)`；这里没有单独的
/// 事件心跳，但 supervisor 每 3 秒发一次 `server.status`，健康 line 不会
/// 静默这么久。按真实到达字节计时而不是按请求成败计时，慢链路上的大帧
/// 只要仍在流动就不会被误杀。
const REMOTE_SILENCE_TIMEOUT: Duration = Duration::from_secs(45);
const REMOTE_LIST_TIMEOUT: Duration = Duration::from_millis(1200);
const STATUS_REFRESH: Duration = Duration::from_secs(3);
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const MISSING_MARKER: &str = "deepcore is not installed here";

/// 一台远端机器。配置里只存 Keychain 引用，不存明文密码或私钥。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Machine {
    /// 本地稳定 id；远端 core 重启、IP 变化都不变。
    pub id: String,
    /// 地址前缀：`<name>/t3`。
    pub name: String,
    /// 复用 DeepTerm 现有 SSH 配置、认证与主机密钥策略。
    pub ssh: SshConnectionConfig,
    /// 远端独立运行时目录（测试/并行安装）；`None` 使用远端默认目录。
    #[serde(default)]
    pub session: Option<String>,
    /// 上一次握手确认的远端 core 永久 id。
    #[serde(default)]
    pub remote_core_id: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub keeps_running_after_logout: Option<bool>,
    #[serde(default)]
    pub logout_linger_user: Option<String>,
}

impl Machine {
    pub(crate) fn view(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "host": self.ssh.host,
            "port": self.ssh.port,
            "username": self.ssh.username,
            "nickname": self.ssh.nickname,
            "session": self.session,
            "remote_core_id": self.remote_core_id,
            "hostname": self.hostname,
            "version": self.version,
            "keeps_running_after_logout": self.keeps_running_after_logout,
            "logout_linger_user": self.logout_linger_user,
        })
    }
}

/// 机器配置的落盘部分与运行时监督分开。
pub struct MachineBook {
    path: std::path::PathBuf,
    machines: Mutex<Vec<Machine>>,
}

impl MachineBook {
    pub fn default_path() -> std::path::PathBuf {
        crate::workspace::persistence::PersistenceManager::get_config_dir().join("machines.json")
    }

    pub fn load_default() -> Arc<Self> {
        match Self::load(Self::default_path()) {
            Ok(book) => Arc::new(book),
            Err(e) => {
                log::warn!("无法读取 machines.json，使用空机器表: {e}");
                Arc::new(Self::empty(Self::default_path()))
            }
        }
    }

    pub fn load(path: std::path::PathBuf) -> io::Result<Self> {
        let machines = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Vec<Machine>>(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} 不是有效的机器表: {e}", path.display()),
                )
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        // **落盘值也必须校验。**
        //
        // `session` 会拼进远端 shell 命令与安装路径。只在 `ensure` 校验
        // 挡不住这条路径：`machines.json` 是普通文件，回滚、手改、从别处
        // 拷一份过来都会绕过 `ensure`。这里拒绝整份文件，而不是丢掉坏行
        // ——静默丢行会让用户的机器列表凭空少一台，比明确报错更难查。
        for machine in &machines {
            if let Some(session) = machine.session.as_deref() {
                super::validate_session(session).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} 中机器 {} 的 {e}", path.display(), machine.name),
                    )
                })?;
            }
        }
        Ok(Self {
            path,
            machines: Mutex::new(machines),
        })
    }

    pub fn empty(path: std::path::PathBuf) -> Self {
        Self {
            path,
            machines: Mutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Vec<Machine> {
        self.lock().clone()
    }

    pub fn get(&self, id: &str) -> Option<Machine> {
        self.lock().iter().find(|m| m.id == id).cloned()
    }

    pub fn by_name(&self, name: &str) -> Option<Machine> {
        self.lock()
            .iter()
            .find(|m| m.name == name || m.id == name)
            .cloned()
    }

    /// 同一 SSH 目标 + session 只保留一条记录；名称冲突时自动加后缀。
    pub fn ensure(
        &self,
        ssh: SshConnectionConfig,
        requested_name: Option<String>,
        session: Option<String>,
    ) -> Result<Machine, String> {
        if let Some(session) = session.as_deref() {
            super::validate_session(session)?;
        }
        let mut machines = self.lock();
        if let Some(existing) = machines
            .iter_mut()
            .find(|m| m.ssh.id() == ssh.id() && m.session == session)
        {
            // 认证引用/超时/代理等可能在上次连接后被用户修改，更新配置但
            // 保留稳定 id 与地址名。
            //
            // 以前这里写 `ssh.use_system_proxy = existing.ssh.use_system_proxy;`
            // 想"保留代理选择"，但那个值恒为 false——字段是 `serde(skip)`，
            // `existing` 来自 `machines.json`。结果是从 GUI 传过来的选择每次
            // 都被抹掉。现在代理标记随请求显式送达（见 `add_machine`），
            // 语义就是"以本次连接的选择为准"，直接采用即可。
            existing.ssh = ssh;
            let machine = existing.clone();
            self.save_locked(&machines)?;
            return Ok(machine);
        }

        let name = unique_name(
            requested_name
                .or_else(|| ssh.nickname.clone())
                .unwrap_or_else(|| default_name(&ssh)),
            &machines,
        );
        let machine = Machine {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            ssh,
            session,
            remote_core_id: None,
            hostname: None,
            version: None,
            keeps_running_after_logout: None,
            logout_linger_user: None,
        };
        machines.push(machine.clone());
        self.save_locked(&machines)?;
        Ok(machine)
    }

    pub fn remove(&self, name_or_id: &str) -> Option<Machine> {
        let mut machines = self.lock();
        let pos = machines
            .iter()
            .position(|m| m.name == name_or_id || m.id == name_or_id)?;
        let machine = machines.remove(pos);
        if let Err(e) = self.save_locked(&machines) {
            log::warn!("无法保存删除后的机器表: {e}");
        }
        Some(machine)
    }

    pub fn update_identity(&self, id: &str, hello: &Value) {
        let mut machines = self.lock();
        let Some(machine) = machines.iter_mut().find(|m| m.id == id) else {
            return;
        };
        let remote_core_id = hello["core_id"].as_str().map(str::to_owned);
        let hostname = hello["hostname"].as_str().map(str::to_owned);
        let version = hello["version"].as_str().map(str::to_owned);
        let keeps_running_after_logout = hello["keeps_running_after_logout"].as_bool();
        let logout_linger_user = hello["logout_linger_user"].as_str().map(str::to_owned);
        let changed = machine.remote_core_id != remote_core_id
            || machine.hostname != hostname
            || machine.version != version
            || machine.keeps_running_after_logout != keeps_running_after_logout
            || machine.logout_linger_user != logout_linger_user;
        if !changed {
            return;
        }
        machine.remote_core_id = remote_core_id;
        machine.hostname = hostname;
        machine.version = version;
        machine.keeps_running_after_logout = keeps_running_after_logout;
        machine.logout_linger_user = logout_linger_user;
        if let Err(e) = self.save_locked(&machines) {
            log::warn!("无法保存远端 core 身份: {e}");
        }
    }

    fn save_locked(&self, machines: &[Machine]) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(machines).map_err(|e| e.to_string())?;
        crate::utils::fs_atomic::atomic_write(&self.path, &bytes).map_err(|e| e.to_string())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Machine>> {
        self.machines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn default_name(ssh: &SshConnectionConfig) -> String {
    let host = ssh.host.trim();
    let host = host.split(':').next().unwrap_or(host);
    let candidate: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let candidate = candidate.trim_matches(['-', '.']);
    if candidate.is_empty() {
        "machine".to_string()
    } else {
        candidate.to_string()
    }
}

fn clean_name(name: &str) -> String {
    let clean: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let clean = clean.trim_matches(['-', '.']);
    if clean.is_empty() {
        "machine".to_string()
    } else {
        clean.to_string()
    }
}

fn unique_name(requested: String, machines: &[Machine]) -> String {
    let base = clean_name(&requested);
    if !machines.iter().any(|m| m.name == base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !machines.iter().any(|m| m.name == candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// line 的连接状态。与“终端程序退出”严格分开：机器掉线时终端记录仍保留。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Connecting,
    Installing,
    Updating,
    Connected,
    NeedsLogin,
    CoreMissing,
    Incompatible,
    InstallFailed,
    Unreachable,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Installing => "installing",
            Self::Updating => "updating",
            Self::Connected => "connected",
            Self::NeedsLogin => "needs_login",
            Self::CoreMissing => "core_missing",
            Self::Incompatible => "incompatible",
            Self::InstallFailed => "install_failed",
            Self::Unreachable => "unreachable",
        }
    }
}

#[derive(Clone, Debug)]
struct LinkStatus {
    kind: LinkKind,
    since_ms: u64,
    why: Option<String>,
}

#[derive(Clone)]
struct LiveLine {
    generation: u64,
    client: Arc<Client>,
    /// 建立这条 line 时用的 SSH 配置。
    ///
    /// 用来判断"用户改了 host/port/认证之后，这条 line 还在对着旧目标
    /// 说话"。只存与本机身份相关的那部分（`PartialEq` 已经把代理标记与
    /// 时间戳排除在外）。
    config: SshConnectionConfig,
}

struct RuntimeState {
    status: LinkStatus,
    live: Option<LiveLine>,
    generation: u64,
}

/// 一个进程内的远端 line，负责连接、握手、重连与远端调用。
pub struct MachineRuntime {
    book: Arc<MachineBook>,
    id: String,
    state: Mutex<RuntimeState>,
    changed: Condvar,
    stop: AtomicBool,
    install_requested: AtomicBool,
    install_force: AtomicBool,
}

impl MachineRuntime {
    pub fn start(book: Arc<MachineBook>, id: String) -> Arc<Self> {
        let runtime = Arc::new(Self {
            book,
            id: id.clone(),
            state: Mutex::new(RuntimeState {
                status: LinkStatus {
                    kind: LinkKind::Connecting,
                    since_ms: now_ms(),
                    why: None,
                },
                live: None,
                generation: 0,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            install_requested: AtomicBool::new(false),
            install_force: AtomicBool::new(false),
        });
        let supervisor = runtime.clone();
        std::thread::Builder::new()
            .name(format!("machine-{id}"))
            .spawn(move || supervisor.supervise())
            .map(|_| runtime.clone())
            .unwrap_or_else(|e| {
                log::error!("无法启动机器监督线程: {e}");
                runtime.set_down(LinkKind::Unreachable, format!("无法启动监督线程: {e}"));
                runtime
            })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn error_kind(&self) -> &'static str {
        self.status().kind.as_str()
    }

    pub fn machine(&self) -> Option<Machine> {
        self.book.get(&self.id)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let line = {
            let mut state = self.lock_state();
            state.live.take().map(|live| live.client)
        };
        if let Some(client) = line {
            client.close();
        }
        self.changed.notify_all();
    }

    /// 这台机器是否已被移除（监督线程与所有中继线程的退出条件）。
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// 配置（host/port/认证/端口转发）变了就作废当前 line，让监督线程按新
    /// 配置重拨。
    ///
    /// 只在"确实变了"时动手：`ensure` 每次连接都会被调用，无条件重拨会让
    /// 每条 GUI 请求都踢掉一条健康的 SSH 连接。
    pub fn invalidate_if_config_changed(&self, current: &SshConnectionConfig) {
        let stale = {
            let state = self.lock_state();
            state
                .live
                .as_ref()
                .is_some_and(|line| line.config != *current)
        };
        if !stale {
            return;
        }
        let generation = self.lock_state().live.as_ref().map(|line| line.generation);
        if let Some(generation) = generation {
            log::info!("机器 {} 的连接配置已变更，作废当前 line", self.id);
            self.invalidate_line(generation, "连接配置已变更");
        }
    }

    /// 请求监督线程安装/更新 core。`force` 为 true 时即使版本相同也重新
    /// 下载并替换（用于修复损坏的远端二进制）。
    pub fn request_install(&self, force: bool) {
        self.install_force.fetch_or(force, Ordering::SeqCst);
        self.install_requested.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    pub fn view(&self) -> Value {
        let machine = self.machine().unwrap_or_else(|| Machine {
            id: self.id.clone(),
            name: self.id.clone(),
            ssh: SshConnectionConfig::new(
                "unknown",
                "unknown",
                crate::ssh::config::AuthMethod::Auto,
            ),
            session: None,
            remote_core_id: None,
            hostname: None,
            version: None,
            keeps_running_after_logout: None,
            logout_linger_user: None,
        });
        let state = self.lock_state();
        let mut view = machine.view();
        let object = view.as_object_mut().expect("machine view is an object");
        object.insert("state".into(), json!(state.status.kind.as_str()));
        object.insert(
            "since_ms".into(),
            if state.status.kind == LinkKind::Connected {
                Value::Null
            } else {
                json!(state.status.since_ms)
            },
        );
        object.insert(
            "why".into(),
            state
                .status
                .why
                .as_ref()
                .map(|why| json!(why))
                .unwrap_or(Value::Null),
        );
        object.insert("generation".into(), json!(state.generation));
        view
    }

    fn status(&self) -> LinkStatus {
        self.lock_state().status.clone()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_down(&self, kind: LinkKind, why: impl Into<String>) {
        let mut state = self.lock_state();
        let since_ms = if state.status.kind == LinkKind::Connected {
            now_ms()
        } else {
            state.status.since_ms
        };
        state.live = None;
        state.status = LinkStatus {
            kind,
            since_ms,
            why: Some(why.into()),
        };
        self.changed.notify_all();
    }

    fn set_connecting(&self) {
        let mut state = self.lock_state();
        if state.live.is_none() {
            state.status = LinkStatus {
                kind: LinkKind::Connecting,
                since_ms: now_ms(),
                why: None,
            };
            self.changed.notify_all();
        }
    }

    fn wait_for_line(&self, timeout: Duration) -> Result<LiveLine, String> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Err("机器已被移除".into());
            }
            if let Some(line) = &state.live {
                return Ok(line.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let status = &state.status;
                return Err(status
                    .why
                    .clone()
                    .unwrap_or_else(|| status.kind.as_str().to_string()));
            }
            let (guard, _) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = guard;
        }
    }

    fn wait_for_line_after(&self, generation: u64, timeout: Duration) -> Option<LiveLine> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(line) = &state.live
                && line.generation > generation
            {
                return Some(line.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (guard, _) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = guard;
        }
    }

    fn current_line(&self) -> Option<LiveLine> {
        self.lock_state().live.clone()
    }

    /// 监督线程手里这条 line 是否仍是运行时当前使用的 line。
    ///
    /// `invalidate_line` 会把 `state.live` 清空，但不会让半开 SSH 的
    /// `Client::is_closed()` 立刻变 true。监督线程只看 is_closed 时会永远
    /// 留在 connected 循环里：状态已经是 unreachable，generation 却停在 1，
    /// GUI 的所有恢复请求都只能反复超时。
    fn line_is_current(&self, generation: u64) -> bool {
        self.lock_state()
            .live
            .as_ref()
            .is_some_and(|line| line.generation == generation)
    }

    fn call_now(&self, mut request: Value, timeout: Duration) -> Result<Value, String> {
        // **一个截止时间管两段。** 先等 line、再发请求；两段各自用满
        // `timeout` 会让整次调用翻倍（spawn 最长 120 秒，而 app 侧在
        // `MACHINE_SPAWN_TIMEOUT`（70 秒）就放弃了）。用户重试后远端
        // 会多出一个 shell，而它的地址从未返回——`close_terminal` 从此
        // 够不到它。
        let deadline = Instant::now() + timeout;
        let line = self.wait_for_line(timeout)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        match line.client.call_timeout(request.take(), remaining) {
            Ok(reply) => Ok(reply),
            Err(e) => {
                self.invalidate_line(line.generation, format!("远端调用失败: {e}"));
                // 带上机器名：多机器日志里"远端 core 没有响应"没法定位是谁。
                Err(format!(
                    "{}: 远端 core 没有响应: {e}",
                    self.machine()
                        .map(|m| m.name)
                        .unwrap_or_else(|| self.id.clone())
                ))
            }
        }
    }

    fn call_connected(&self, request: Value, timeout: Duration) -> Result<Value, String> {
        let Some(line) = self.current_line() else {
            return Err(self.status().why.unwrap_or_else(|| "机器未连接".into()));
        };
        // 这里不能因一次短超时就作废 line：列表/快照探测故意用 1~5 秒
        // 预算，慢线路上的正常拥塞会被误判成断线。恢复路径的 attach 和
        // spawn 各自使用长超时，并在那里负责作废真正的死 line。
        line.client
            .call_timeout(request, timeout)
            .map_err(|e| format!("远端 core 没有响应: {e}"))
    }

    /// 等一条仍在收字节的 line。
    ///
    /// SSH 半开连接不会让 `is_closed()` 立即变真。若调用方拿到一条已经
    /// 静默超过上限的 line 再发 attach，只会白等满 30 秒。这里先按 Herdr
    /// 的静默判据丢弃它，supervisor 会在同一轮立刻重拨。
    fn wait_for_usable_line(&self, timeout: Duration) -> Result<LiveLine, String> {
        self.wait_for_usable_line_for(timeout, REMOTE_SILENCE_TIMEOUT)
    }

    fn wait_for_usable_line_for(
        &self,
        timeout: Duration,
        silence_timeout: Duration,
    ) -> Result<LiveLine, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self
                    .status()
                    .why
                    .unwrap_or_else(|| "等待远端 line 超时".into()));
            }
            let line = self.wait_for_line(remaining)?;
            if !line.client.silent_for(silence_timeout) {
                return Ok(line);
            }
            let why = format!("远端 line 已静默 {} 秒", silence_timeout.as_secs());
            self.invalidate_line(line.generation, why.clone());
            if Instant::now() >= deadline {
                return Err(why);
            }
        }
    }

    fn attach_once(
        &self,
        remote_address: &str,
        mut request: Value,
    ) -> Result<(Value, LiveLine, u32), String> {
        let line = self.wait_for_usable_line(REMOTE_CALL_TIMEOUT)?;
        request["op"] = json!("attach");
        request["address"] = json!(remote_address);
        let reply = match line.client.call_timeout(request, REMOTE_CALL_TIMEOUT) {
            Ok(reply) => reply,
            Err(e) => {
                // attach 是恢复界面的关键路径。超时说明这条 line 已经不足以
                // 承载首帧（半开 TCP、远端 core 重启、丢包打满窗口），必须
                // 丢弃它并让监督线程拨一条新 line，不能把错误页永久留在前台。
                self.invalidate_line(line.generation, format!("远端 attach 超时: {e}"));
                return Err(format!("远端 attach 失败: {e}"));
            }
        };
        if reply["ok"] != true {
            return Err(reply["error"]
                .as_str()
                .unwrap_or("远端拒绝 attach")
                .to_string());
        }
        let channel = reply["channel"]
            .as_u64()
            .ok_or_else(|| "远端 attach 回复缺少 channel".to_string())?
            as u32;
        Ok((reply, line, channel))
    }

    /// 丢弃指定 generation 的 line，让监督线程重新拨号。
    ///
    /// `Client::is_closed()` 只能发现 socket 已经关闭。SSH 半开（对端 core
    /// 已退出、链路恢复后旧 TCP 仍在）时它永远为 false，于是所有后续调用都
    /// 超时，而状态还显示 `connected`。这正是“重启 GUI 后远程终端一直卡在
    /// 恢复中”的根因。调用方在请求超时后显式作废它。
    ///
    /// 只作废 `generation` 仍然匹配的那一条：并发的多个调用可能同时超时，
    /// 旧调用不能误杀监督线程刚建立的新 line。
    fn invalidate_line(&self, generation: u64, why: impl Into<String>) {
        // **取 client 与写状态必须在同一个临界区。**
        //
        // 旧写法先在锁里 `take` 出 client、放锁、再交给 `set_down` 重新
        // 加锁写 `live = None`。中间这段窗口里监督线程可能刚好发布一条新
        // line，`set_down` 会把那条健康的 `Connected` 一起抹成 `Unreachable`
        // 并触发重拨。
        let why = why.into();
        let client = {
            let mut state = self.lock_state();
            let stale = state
                .live
                .as_ref()
                .is_some_and(|line| line.generation == generation);
            if !stale {
                return;
            }
            let since_ms = if state.status.kind == LinkKind::Connected {
                now_ms()
            } else {
                state.status.since_ms
            };
            let client = state.live.take().map(|live| live.client);
            state.status = LinkStatus {
                kind: LinkKind::Unreachable,
                since_ms,
                why: Some(format!("line 失活: {why}")),
            };
            client
        };
        log::warn!(
            "机器 {} 的 line generation {generation} 失活，关闭并重连: {why}",
            self.id
        );
        self.changed.notify_all();
        if let Some(client) = client {
            client.close();
        }
    }

    fn supervise(self: Arc<Self>) {
        let mut backoff = RECONNECT_MIN;
        let mut login_step = 0usize;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let Some(machine) = self.machine() else {
                return;
            };
            if self.install_requested.swap(false, Ordering::SeqCst) {
                let force = self.install_force.swap(false, Ordering::SeqCst);
                let line = { self.lock_state().live.take().map(|live| live.client) };
                if let Some(client) = line {
                    client.close();
                }
                let active_kind = if force {
                    LinkKind::Updating
                } else {
                    LinkKind::Installing
                };
                match self.install_core(&machine, force, active_kind) {
                    Ok(()) => {
                        backoff = RECONNECT_MIN;
                        login_step = 0;
                        self.sleep_or_stop(RECONNECT_MIN);
                        continue;
                    }
                    Err(e) => {
                        self.set_down(LinkKind::InstallFailed, e.clone());
                        log::warn!("机器 {} 手动安装失败: {e}", machine.name);
                        self.sleep_or_stop(backoff);
                        backoff = (backoff * 2).min(RECONNECT_MAX);
                        continue;
                    }
                }
            }
            self.set_connecting();
            let transport = match RemoteTransport::open(&machine.ssh, machine.session.as_deref()) {
                Ok(transport) => transport,
                Err(e) => {
                    let text = e.to_string();
                    let kind = classify_connect_error(&text);
                    self.set_down(kind, text.clone());
                    let wait = if kind == LinkKind::NeedsLogin {
                        let wait =
                            LOGIN_RECONNECT_STEPS[login_step.min(LOGIN_RECONNECT_STEPS.len() - 1)];
                        login_step = (login_step + 1).min(LOGIN_RECONNECT_STEPS.len() - 1);
                        wait
                    } else {
                        backoff
                    };
                    log::info!(
                        "机器 {} 连接失败（{}）: {text}；{:?} 后重试",
                        machine.name,
                        kind.as_str(),
                        wait
                    );
                    self.sleep_or_stop(wait);
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue;
                }
            };
            let diagnostics = transport.diagnostics();
            let (reader, writer) = transport.into_io();
            let client = match Client::from_transport(reader, writer) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    self.set_down(LinkKind::Unreachable, format!("无法建立远端协议连接: {e}"));
                    self.sleep_or_stop(backoff);
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue;
                }
            };

            let hello = match client.hello(None, REMOTE_HANDSHAKE_TIMEOUT) {
                Ok(hello) => hello,
                Err(e) => {
                    let stderr = diagnostics.snapshot();
                    let text = if stderr.contains(MISSING_MARKER) {
                        MISSING_MARKER.to_string()
                    } else if stderr.trim().is_empty() {
                        e.to_string()
                    } else {
                        format!("{e}: {}", stderr.trim())
                    };
                    let kind = classify_handshake_error(&e, &text);
                    client.close();
                    if kind == LinkKind::CoreMissing {
                        log::info!("机器 {} 没有 deepcore，尝试自动安装", machine.name);
                        match self.install_core(&machine, false, LinkKind::Installing) {
                            Ok(()) => {
                                backoff = RECONNECT_MIN;
                                login_step = 0;
                                self.sleep_or_stop(RECONNECT_MIN);
                                continue;
                            }
                            Err(install_error) => {
                                let message =
                                    format!("远端没有 deepcore，自动安装失败: {install_error}");
                                self.set_down(LinkKind::InstallFailed, message.clone());
                                log::warn!("机器 {} {message}", machine.name);
                                self.sleep_or_stop(backoff);
                                backoff = (backoff * 2).min(RECONNECT_MAX);
                                continue;
                            }
                        }
                    }
                    self.set_down(kind, text.clone());
                    let wait = if kind == LinkKind::NeedsLogin {
                        let wait =
                            LOGIN_RECONNECT_STEPS[login_step.min(LOGIN_RECONNECT_STEPS.len() - 1)];
                        login_step = (login_step + 1).min(LOGIN_RECONNECT_STEPS.len() - 1);
                        wait
                    } else {
                        backoff
                    };
                    log::info!(
                        "机器 {} 握手失败（{}）: {text}",
                        machine.name,
                        kind.as_str()
                    );
                    self.sleep_or_stop(wait);
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue;
                }
            };

            let remote_version = hello["version"].as_str().unwrap_or_default();
            if remote_version != deploy::core_version() {
                client.close();
                log::info!(
                    "机器 {} 的 core 版本 {} 与本机 {} 不一致，尝试更新",
                    machine.name,
                    remote_version,
                    deploy::core_version()
                );
                match self.install_core(&machine, false, LinkKind::Updating) {
                    Ok(()) => {
                        backoff = RECONNECT_MIN;
                        login_step = 0;
                        self.sleep_or_stop(RECONNECT_MIN);
                        continue;
                    }
                    Err(install_error) => {
                        let message = format!(
                            "远端 core 版本为 {remote_version}，需要 {}；自动更新失败: {install_error}",
                            deploy::core_version()
                        );
                        self.set_down(LinkKind::Incompatible, message.clone());
                        log::warn!("机器 {} {message}", machine.name);
                        self.sleep_or_stop(backoff);
                        backoff = (backoff * 2).min(RECONNECT_MAX);
                        continue;
                    }
                }
            }

            let remote_core_id = hello["core_id"].as_str().unwrap_or_default();
            if let Some(expected) = machine.remote_core_id.as_deref()
                && !expected.is_empty()
                && expected != remote_core_id
            {
                self.set_down(
                    LinkKind::Incompatible,
                    format!(
                        "同一目标上出现了另一个 core（期望 {expected}，实际 {remote_core_id}）"
                    ),
                );
                self.sleep_or_stop(backoff);
                backoff = (backoff * 2).min(RECONNECT_MAX);
                continue;
            }

            self.book.update_identity(&self.id, &hello);
            let generation = {
                let mut state = self.lock_state();
                state.generation += 1;
                let generation = state.generation;
                state.status = LinkStatus {
                    kind: LinkKind::Connected,
                    since_ms: now_ms(),
                    why: None,
                };
                state.live = Some(LiveLine {
                    generation,
                    client: client.clone(),
                    config: machine.ssh.clone(),
                });
                self.changed.notify_all();
                generation
            };
            log::info!(
                "机器 {} 已连接（{}，generation {generation}）",
                machine.name,
                hello["hostname"].as_str().unwrap_or("unknown")
            );
            backoff = RECONNECT_MIN;
            login_step = 0;
            let mut next_status = Instant::now() + STATUS_REFRESH;

            while !self.stop.load(Ordering::SeqCst)
                && !client.is_closed()
                && !self.install_requested.load(Ordering::SeqCst)
            {
                if client.silent_for(REMOTE_SILENCE_TIMEOUT) {
                    self.invalidate_line(
                        generation,
                        format!("远端 line 已静默 {} 秒", REMOTE_SILENCE_TIMEOUT.as_secs()),
                    );
                    break;
                }
                // 调用方在 attach/call 超时后会用 invalidate_line 替换掉
                // `state.live`。半开 TCP 不会触发 is_closed，因此这里必须
                // 主动发现“本 generation 已被作废”，退出旧连接并重新拨号。
                if !self.line_is_current(generation) {
                    break;
                }
                if Instant::now() >= next_status {
                    next_status = Instant::now() + STATUS_REFRESH;
                    match client.call_timeout(json!({"op": "server.status"}), STATUS_TIMEOUT) {
                        Ok(status) if status["ok"] == true => {
                            self.book.update_identity(&self.id, &status);
                        }
                        Ok(status) => {
                            log::debug!(
                                "机器 {} 的状态刷新被拒绝: {}",
                                machine.name,
                                status["error"].as_str().unwrap_or("unknown")
                            );
                        }
                        Err(_) if client.is_closed() => break,
                        Err(e) => {
                            log::debug!("机器 {} 的状态刷新失败: {e}", machine.name);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            if self.install_requested.load(Ordering::SeqCst) {
                client.close();
                continue;
            }
            if !self.line_is_current(generation) {
                // invalidate_line 已经写好更具体的失败原因，不能再用笼统的
                // “line 已断开”覆盖。关闭旧 client 后立即开始新一次拨号。
                client.close();
                continue;
            }
            self.set_down(LinkKind::Unreachable, "line 已断开");
            client.close();
            // 远端 core 重启后 holder 仍在，立刻重拨就能接回同一批终端。
            self.sleep_or_stop(RECONNECT_MIN);
        }
    }

    fn sleep_or_stop(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !self.stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            if self.install_requested.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100).min(duration));
        }
    }

    fn install_core(
        &self,
        machine: &Machine,
        force: bool,
        active_kind: LinkKind,
    ) -> Result<(), String> {
        self.set_down(
            active_kind,
            if force {
                "正在重新安装远端 core"
            } else if active_kind == LinkKind::Updating {
                "正在更新远端 core"
            } else {
                "正在安装远端 core"
            },
        );
        let mode = if force {
            InstallMode::Force
        } else {
            InstallMode::Ensure
        };
        let outcome = deploy::ensure_remote_core(&machine.ssh, machine.session.as_deref(), mode)?;
        match outcome {
            InstallOutcome::Installed {
                version,
                previous_version,
                ..
            } => {
                log::info!(
                    "机器 {} 的 deepcore 已安装为 {version}（原版本 {}），holder 保持不变",
                    machine.name,
                    previous_version.as_deref().unwrap_or("无")
                );
                Ok(())
            }
            InstallOutcome::AlreadyInstalled { version, .. } => {
                if active_kind == LinkKind::Updating && force {
                    return Ok(());
                }
                Err(format!(
                    "远端已安装 deepcore {version}，但仍无法完成握手；请检查远端运行时目录或手动修复"
                ))
            }
        }
    }
}

fn classify_connect_error(error: &str) -> LinkKind {
    let lower = error.to_ascii_lowercase();
    if lower.contains("permission denied")
        || lower.contains("authentication")
        || lower.contains("认证被拒绝")
        || lower.contains("所有默认密钥")
        || lower.contains("no auth method")
    {
        LinkKind::NeedsLogin
    } else {
        LinkKind::Unreachable
    }
}

fn classify_handshake_error(error: &io::Error, text: &str) -> LinkKind {
    if text.contains(MISSING_MARKER) {
        LinkKind::CoreMissing
    } else if error.kind() == io::ErrorKind::Unsupported {
        LinkKind::Incompatible
    } else if error.kind() == io::ErrorKind::PermissionDenied
        || classify_connect_error(text) == LinkKind::NeedsLogin
        || classify_connect_error(&error.to_string()) == LinkKind::NeedsLogin
    {
        LinkKind::NeedsLogin
    } else {
        LinkKind::Unreachable
    }
}

/// 机器集合：配置 + 每台机器的运行时。
pub struct Machines {
    book: Arc<MachineBook>,
    runtimes: Mutex<HashMap<String, Arc<MachineRuntime>>>,
}

impl Machines {
    pub fn load_default() -> Arc<Self> {
        Self::from_book(MachineBook::load_default())
    }

    pub fn empty() -> Arc<Self> {
        Self::from_book(Arc::new(MachineBook::empty(MachineBook::default_path())))
    }

    fn from_book(book: Arc<MachineBook>) -> Arc<Self> {
        let machines = Arc::new(Self {
            book: book.clone(),
            runtimes: Mutex::new(HashMap::new()),
        });
        for machine in book.snapshot() {
            machines.start_runtime(machine.id);
        }
        machines
    }

    pub fn ensure(
        &self,
        ssh: SshConnectionConfig,
        name: Option<String>,
        session: Option<String>,
    ) -> Result<Machine, String> {
        let machine = self.book.ensure(ssh, name, session)?;
        let mut runtimes = self
            .runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match runtimes.get(&machine.id) {
            Some(runtime) => {
                // **配置变了就必须作废现有 line。**
                //
                // `MachineBook::ensure` 只更新 `machines.json` 里的配置，而
                // 活着的 line 用的是建连时的 host/port/认证。用户改了端口
                // 或换了主机后，旧实现会继续对着**旧目标**说话，直到 45 秒
                // 静默超时把它作废——这期间的所有操作都打到了错误的机器上。
                runtime.invalidate_if_config_changed(&machine.ssh);
            }
            None => {
                let runtime = MachineRuntime::start(self.book.clone(), machine.id.clone());
                runtimes.insert(machine.id.clone(), runtime);
            }
        }
        Ok(machine)
    }

    pub fn remove(&self, name_or_id: &str) -> Option<Machine> {
        let machine = self.book.remove(name_or_id)?;
        let runtime = self
            .runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&machine.id);
        if let Some(runtime) = runtime {
            runtime.stop();
        }
        Some(machine)
    }

    /// 请求安装/更新远端 core。真正工作由该机器的监督线程完成，避免
    /// 一条 app 控制请求阻塞整个本地 core 的连接线程。
    pub fn request_install(&self, name_or_id: &str, force: bool) -> Result<Value, String> {
        let runtime = self
            .find(name_or_id)
            .ok_or_else(|| "没有这台机器".to_string())?;
        runtime.request_install(force);
        Ok(runtime.view())
    }

    pub fn list(&self) -> Vec<Value> {
        self.book
            .snapshot()
            .into_iter()
            .map(|machine| {
                self.runtime(&machine.id)
                    .map(|runtime| runtime.view())
                    .unwrap_or_else(|| machine.view())
            })
            .collect()
    }

    pub fn runtime(&self, id: &str) -> Option<Arc<MachineRuntime>> {
        self.runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub fn find(&self, name_or_id: &str) -> Option<Arc<MachineRuntime>> {
        let machine = self.book.by_name(name_or_id)?;
        self.runtime(&machine.id)
    }

    /// 把 `name/t3` 或 `id/t3` 拆成运行时与远端裸地址。
    pub fn resolve(&self, address: &str) -> Option<(Arc<MachineRuntime>, String)> {
        let (prefix, rest) = address.split_once('/')?;
        if prefix.is_empty() || rest.is_empty() || rest.contains('/') {
            return None;
        }
        Some((self.find(prefix)?, rest.to_string()))
    }

    pub fn spawn(&self, machine: &str, mut request: Value) -> Result<Value, String> {
        let runtime = self
            .find(machine)
            .ok_or_else(|| format!("未知机器 {machine}"))?;
        request.as_object_mut().map(|o| o.remove("machine"));
        request["op"] = json!("spawn");
        let mut reply = runtime.call_now(request, MACHINE_START_TIMEOUT)?;
        if reply["ok"] == true
            && let Some(address) = reply["address"].as_str()
        {
            let name = runtime
                .machine()
                .map(|m| m.name)
                .unwrap_or_else(|| machine.to_string());
            reply["address"] = json!(format!("{name}/{address}"));
        }
        Ok(reply)
    }

    pub fn call_address(
        &self,
        address: &str,
        mut request: Value,
        timeout: Duration,
    ) -> Result<Option<Value>, String> {
        let Some((runtime, remote_address)) = self.resolve(address) else {
            return Ok(None);
        };
        request["address"] = json!(remote_address);
        let mut reply = runtime.call_connected(request, timeout)?;
        localize_reply(&runtime, &mut reply);
        Ok(Some(reply))
    }

    /// 合并本地调用：远端列表失败不阻塞本地列表，只把机器状态带回去。
    pub fn terminals(&self, timeout: Duration) -> (Vec<Value>, Vec<Value>) {
        let runtimes: Vec<Arc<MachineRuntime>> = self
            .runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        // 每台机器一次 `list`，彼此独立 → fork-join。
        //
        // 用 `thread::scope` 而不是若干 `thread::spawn` + `mpsc`：作用域保证
        // 所有线程在返回前 join，既不会留下没人管的游离线程（旧写法在超时
        // 后直接返回，那些线程还在跑并各自持着一个 `Arc<MachineRuntime>`），
        // 也省掉手工 fan-in。
        //
        // 单次调用本身有上界（`call_connected` 内部按 per-call 超时收），
        // 所以 join 不会无限等；这里再把 per-call 预算钳到调用方给的整体
        // 预算之内，保证整个函数不超过 `timeout`。
        let per_call = timeout.min(REMOTE_LIST_TIMEOUT);
        let results: Vec<(Arc<MachineRuntime>, Result<Value, String>)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = runtimes
                    .into_iter()
                    .map(|runtime| {
                        let runtime_for_call = runtime.clone();
                        let handle = scope.spawn(move || {
                            runtime_for_call.call_connected(json!({"op": "list"}), per_call)
                        });
                        (runtime, handle)
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|(runtime, handle)| {
                        let result = handle
                            .join()
                            .unwrap_or_else(|_| Err("远端 list 线程异常退出".to_string()));
                        (runtime, result)
                    })
                    .collect()
            });

        let mut terminals = Vec::new();
        let mut states = Vec::new();
        for (runtime, result) in results {
            match result {
                Ok(mut reply) if reply["ok"] == true => {
                    localize_reply(&runtime, &mut reply);
                    if let Some(items) = reply["terminals"].as_array() {
                        terminals.extend(items.iter().cloned());
                    }
                }
                Ok(_) | Err(_) => {}
            }
            states.push(runtime.view());
        }
        // 超时没回的机器也出现在状态里，用户能看到它在连接而不是“凭空消失”。
        for machine in self.list() {
            if !states.iter().any(|state| state["id"] == machine["id"]) {
                states.push(machine);
            }
        }
        (terminals, states)
    }

    pub fn attach(
        &self,
        machine: &str,
        remote_address: &str,
        request: Value,
        local_channel: u32,
        sink: async_channel::Sender<Frame>,
    ) -> Result<(Value, RemoteAttachment), String> {
        let runtime = self
            .find(machine)
            .ok_or_else(|| format!("未知机器 {machine}"))?;
        let size = request
            .get("cols")
            .and_then(Value::as_u64)
            .zip(request.get("rows").and_then(Value::as_u64))
            .map(|(cols, rows)| {
                (
                    cols.min(u16::MAX as u64) as u16,
                    rows.min(u16::MAX as u64) as u16,
                )
            });
        let (mut reply, line, remote_channel) = runtime.attach_once(remote_address, request)?;
        let receiver = line.client.attach_channel(remote_channel);
        let attachment = RemoteAttachment {
            runtime: runtime.clone(),
            remote_address: remote_address.to_string(),
            remote_channel: Arc::new(Mutex::new(remote_channel)),
            size: Arc::new(Mutex::new(size)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        localize_reply(&runtime, &mut reply);
        let relay = attachment.clone();
        let generation = line.generation;
        // 中继线程创建失败时必须先退订：否则远端仍为我们推数据、额度只减
        // 不增，最后停在 `NeedsPaint` 谁也救不回来。
        if let Err(e) = std::thread::Builder::new()
            .name("machine-attach".into())
            .spawn(move || relay.run(receiver, generation, local_channel, sink))
        {
            attachment.detach();
            return Err(format!("无法启动远端终端中继: {e}"));
        }
        Ok((reply, attachment))
    }

    fn start_runtime(&self, id: String) {
        let runtime = MachineRuntime::start(self.book.clone(), id.clone());
        self.runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, runtime);
    }
}

fn localize_reply(runtime: &MachineRuntime, reply: &mut Value) {
    let Some(name) = runtime.machine().map(|m| m.name) else {
        return;
    };
    if let Some(address) = reply.get_mut("address")
        && let Some(raw) = address.as_str().map(str::to_owned)
        && !raw.contains('/')
    {
        *address = json!(format!("{name}/{raw}"));
    }
    if let Some(terminals) = reply.get_mut("terminals").and_then(Value::as_array_mut) {
        for terminal in terminals {
            if let Some(address) = terminal.get_mut("address")
                && let Some(raw) = address.as_str().map(str::to_owned)
                && !raw.contains('/')
            {
                *address = json!(format!("{name}/{raw}"));
            }
        }
    }
}

/// 一个 app 连接对远端终端的订阅。可克隆给中继线程共享停止标志。
#[derive(Clone)]
pub struct RemoteAttachment {
    runtime: Arc<MachineRuntime>,
    remote_address: String,
    remote_channel: Arc<Mutex<u32>>,
    size: Arc<Mutex<Option<(u16, u16)>>>,
    stop: Arc<AtomicBool>,
}

impl RemoteAttachment {
    pub fn send(&self, kind: Kind, payload: Vec<u8>) {
        let channel = *self
            .remote_channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(line) = self.runtime.current_line() {
            let _ = line.client.send(Frame::new(channel, kind, payload));
        }
    }

    pub fn resize(&self, cols: u16, rows: u16, payload: Vec<u8>) {
        *self
            .size
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((cols, rows));
        self.send(Kind::Resize, payload);
    }

    pub fn detach(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let channel = *self
            .remote_channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(line) = self.runtime.current_line() {
            let _ = line.client.call_timeout(
                json!({"op": "detach", "channel": channel}),
                Duration::from_secs(1),
            );
            // **必须注销本地的接收端。**
            //
            // `run` 阻塞在 `receiver.recv_blocking()`：置 `stop` 唤不醒它，
            // 而 detach 之后远端不再产帧，于是它永远等下去——每关一个远端
            // 标签就漏一条线程加一个 `Arc<RemoteAttachment>`，直到整台机器
            // 被移除、`client.close()` 把所有 route sender 一起丢弃。
            line.client.detach_channel(channel);
        }
    }

    fn run(
        self,
        mut receiver: async_channel::Receiver<Frame>,
        mut generation: u64,
        local_channel: u32,
        sink: async_channel::Sender<Frame>,
    ) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            while let Ok(frame) = receiver.recv_blocking() {
                if self.stop.load(Ordering::SeqCst) {
                    return;
                }
                let close = frame.kind == Kind::Close;
                if sink
                    .try_send(Frame::new(local_channel, frame.kind, frame.payload))
                    .is_err()
                {
                    self.detach();
                    return;
                }
                if close {
                    return;
                }
            }

            // 流断开不是终端结束：写一行提示，然后等同一台机器的新 line。
            let name = self
                .runtime
                .machine()
                .map(|m| m.name)
                .unwrap_or_else(|| "remote".into());
            let notice = format!("\x18\r\n[line to {name} lost; reconnecting...]\r\n");
            if sink
                .try_send(Frame::data(local_channel, notice.into_bytes()))
                .is_err()
            {
                self.detach();
                return;
            }

            // 流已经断了：先作废我们挂着的这一条 line。
            //
            // 不能只等 supervisor 的静默判据（45 秒）：半开 SSH 上它仍是
            // `is_current`，`wait_for_line_after` 会永远等不到更大的
            // generation——这正是"终端永久冻在最后一屏"的成因。作废之后
            // supervisor 立刻重拨，新 generation 由它递增。
            self.runtime
                .invalidate_line(generation, "attach 流中断，重建 line");

            // 恢复：一直等到重新挂上，或者确认终端已不存在。
            //
            // `generation` 只从成功的 attach 取（`line.generation`），绝不
            // 本地自增——它必须始终是运行时真的发布过的值，否则一定超前。
            loop {
                // **两个退出条件都要检查。** `wait_for_line_after` 在运行时
                // 停止时会**立刻**返回 `None`，而下面 `continue` 会不带任何
                // 睡眠地空转——100% CPU 直到进程退出。`MachineRuntime::stop`
                // 也不会去置 attachment 自己的 `stop`（那只由 `detach` 置），
                // 所以这里必须直接看运行时。
                if self.stop.load(Ordering::SeqCst) || self.runtime.is_stopped() {
                    return;
                }
                if self
                    .runtime
                    .wait_for_line_after(generation, Duration::from_millis(500))
                    .is_none()
                {
                    continue;
                }
                let mut request = json!({"op": "attach", "history": 0});
                if let Some((cols, rows)) = *self
                    .size
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                {
                    request["cols"] = json!(cols);
                    request["rows"] = json!(rows);
                }
                match self.runtime.attach_once(&self.remote_address, request) {
                    // `attach_once` 对 `ok != true` 一律返回 `Err`，所以拿到
                    // `Ok` 就一定是成功，不需要再判 `reply["ok"]`。
                    Ok((_, line, channel)) => {
                        // **必须是比我们手里更新的一条 line。** `attach_once`
                        // 内部只保证"拿到一条不静默的 line"，不保证它比传入的
                        // generation 大；若在旧 line 上反复 attach 成功，这里
                        // 会拿着同一个 generation 无限循环 attach 同一条线。
                        if line.generation <= generation {
                            log::debug!(
                                "attach 命中了旧 line（generation {} <= {generation}），等新的",
                                line.generation
                            );
                            self.runtime.sleep_or_stop(RECONNECT_MIN);
                            continue;
                        }
                        generation = line.generation;
                        *self
                            .remote_channel
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = channel;
                        receiver = line.client.attach_channel(channel);
                        break;
                    }
                    // "终端已不存在"走的是错误分支——原来的
                    // `Ok(..) if kind == "not_found"` 永远不可达。
                    Err(error) if crate::daemon::session::is_missing_terminal_error(&error) => {
                        let _ = sink.try_send(Frame::new(
                            local_channel,
                            Kind::Close,
                            b"remote terminal no longer exists".to_vec(),
                        ));
                        return;
                    }
                    Err(error) => {
                        // 远端还没恢复（core 重启中 / 机器掉线）：等下一轮。
                        // 这里**不能**本地 generation += 1，那会让等待条件
                        // 永远不成立；靠 sleep 限速，靠 supervisor 出新 line。
                        log::debug!("远端 attach 失败，稍后重试: {error}");
                        self.runtime.sleep_or_stop(RECONNECT_MIN);
                    }
                }
            }
        }
    }
}

/// 一条 SSH 上的 framed 字节管道。
struct RemoteTransport {
    reader: ChannelReader,
    writer: ChannelWriter,
    diagnostics: RemoteDiagnostics,
}

impl RemoteTransport {
    fn open(config: &SshConnectionConfig, session: Option<&str>) -> io::Result<Self> {
        let config = config.clone();
        let command = remote_connect_command(session);
        let (tx_in, rx_in) = async_channel::bounded::<Vec<u8>>(64);
        let (tx_out, rx_out) = async_channel::bounded::<WireOut>(64);
        let diagnostics = RemoteDiagnostics::default();
        let task_diagnostics = diagnostics.clone();

        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async move {
            let handler = SshClientHandler {
                host: config.host.clone(),
                port: config.port,
                host_key_policy: crate::ssh::host_key::policy_from_global_config(),
                port_forwards: crate::ssh::port_forward::RemoteForwardTable::default(),
            };
            let handle = connect_and_authenticate(&config, handler)
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            let channel = handle
                .channel_open_session()
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            channel
                .exec(true, command)
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            let (mut read_half, write_half) = channel.split();

            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        message = read_half.wait() => match message {
                            Some(russh::ChannelMsg::Data { data }) => {
                                if tx_in.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                            Some(russh::ChannelMsg::ExtendedData { data, ext: 1 }) => {
                                task_diagnostics.push(&data);
                            }
                            Some(russh::ChannelMsg::Eof)
                            | Some(russh::ChannelMsg::Close)
                            | None => break,
                            _ => {}
                        },
                        outbound = rx_out.recv() => match outbound {
                            Ok(WireOut::Data(data)) => {
                                if write_half.data_bytes(data).await.is_err() {
                                    break;
                                }
                            }
                            Ok(WireOut::Shutdown) | Err(_) => {
                                let _ = write_half.eof().await;
                                break;
                            }
                        }
                    }
                }
                drop(tx_in);
                let _ = write_half.close().await;
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "English")
                    .await;
            });

            Ok(Self {
                reader: ChannelReader {
                    rx: rx_in,
                    buffer: Vec::new(),
                    offset: 0,
                },
                writer: ChannelWriter { tx: tx_out },
                diagnostics,
            })
        })
    }

    fn diagnostics(&self) -> RemoteDiagnostics {
        self.diagnostics.clone()
    }

    fn into_io(self) -> (ChannelReader, ChannelWriter) {
        (self.reader, self.writer)
    }
}

enum WireOut {
    Data(Vec<u8>),
    Shutdown,
}

/// 把 SSH 读任务产出的块变成同步 `Read`，供现有 `Client` 分帧线程使用。
struct ChannelReader {
    rx: async_channel::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    offset: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.offset < self.buffer.len() {
                let n = (self.buffer.len() - self.offset).min(out.len());
                out[..n].copy_from_slice(&self.buffer[self.offset..self.offset + n]);
                self.offset += n;
                return Ok(n);
            }
            match self.rx.recv_blocking() {
                Ok(bytes) => {
                    self.buffer = bytes;
                    self.offset = 0;
                    if self.buffer.is_empty() {
                        continue;
                    }
                }
                Err(_) => return Ok(0),
            }
        }
    }
}

/// 把 `Client` 写线程的同步 `Write` 变成 SSH 写任务的有界队列。
struct ChannelWriter {
    tx: async_channel::Sender<WireOut>,
}

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.tx
            .send_blocking(WireOut::Data(bytes.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "远端 line 已断开"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ChannelWriter {
    fn drop(&mut self) {
        let _ = self.tx.try_send(WireOut::Shutdown);
    }
}

#[derive(Clone, Default)]
struct RemoteDiagnostics {
    text: Arc<Mutex<Vec<u8>>>,
}

impl RemoteDiagnostics {
    fn push(&self, bytes: &[u8]) {
        let mut text = self
            .text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if text.len() < 64 * 1024 {
            let room = 64 * 1024 - text.len();
            text.extend_from_slice(&bytes[..bytes.len().min(room)]);
        }
    }

    fn snapshot(&self) -> String {
        String::from_utf8_lossy(
            &self
                .text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }
}

fn remote_connect_command(session: Option<&str>) -> String {
    match session {
        // `session` 已由 `validate_session` 收口，这里仍然统一加引号：
        // 这是拼进 `sh -c` 的最后一道边，不该依赖上游一定校验过。
        Some(session) => {
            let dir = shell_quote(&format!("$HOME/.local/share/deepterm/sessions/{session}"));
            format!(
                "export DEEPTERM_RUNTIME_DIR={dir}; \
                 if [ -x {dir}/deepcore ]; then \
                 exec {dir}/deepcore connect; \
                 else echo \"{MISSING_MARKER}\" >&2; exit 127; fi"
            )
        }
        None => format!(
            "if [ -x \"$HOME/.local/share/deepterm/deepcore\" ]; then \
             exec \"$HOME/.local/share/deepterm/deepcore\" connect; \
             elif command -v deepcore >/dev/null 2>&1; then \
             exec deepcore connect; \
             else echo \"{MISSING_MARKER}\" >&2; exit 127; fi"
        ),
    }
}

/// POSIX 单引号转义。
///
/// 与 `deploy::shell_quote` 同一语义：单引号里的一切都是字面量，唯一的
/// 特殊字符是单引号本身（闭合后接 `\'` 再重开）。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::config::AuthMethod;

    fn book() -> (Arc<MachineBook>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "deepterm-machines-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        (Arc::new(MachineBook::empty(path.clone())), path)
    }

    /// 代理选择必须按「本次连接」生效，而不是被上一次的值盖掉。
    ///
    /// 回归价值：`use_system_proxy` 是 `serde(skip)` 的，所以 `machines.json`
    /// 里那份永远是 `false`。旧代码在更新已有记录时写
    /// `ssh.use_system_proxy = existing.ssh.use_system_proxy;`——等于每次都把
    /// GUI 传进来的选择抹成 `false`，于是「使用系统代理连接」连海外主机时
    /// 静默走直连（通常直接超时）。
    #[test]
    fn the_proxy_choice_follows_the_latest_connection() {
        let (book, path) = book();
        let mut ssh = SshConnectionConfig::new("overseas.example", "root", AuthMethod::Agent);

        ssh.use_system_proxy = true;
        let machine = book.ensure(ssh.clone(), Some("box".into()), None).unwrap();
        assert!(
            machine.ssh.use_system_proxy,
            "第一次连接选了代理，记录里就该是 true"
        );

        // 用户这次选择直连：必须立刻生效，不能被旧值粘住。
        ssh.use_system_proxy = false;
        let machine = book.ensure(ssh.clone(), Some("box".into()), None).unwrap();
        assert!(!machine.ssh.use_system_proxy, "用户改回直连后仍被旧值粘住");

        // 再选回代理，同样要跟上。
        ssh.use_system_proxy = true;
        let machine = book.ensure(ssh, Some("box".into()), None).unwrap();
        assert!(machine.ssh.use_system_proxy);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn names_are_safe_unique_and_address_prefixes_round_trip() {
        let (book, path) = book();
        let ssh = SshConnectionConfig::new("192.168.1.8", "root", AuthMethod::Agent);
        let a = book
            .ensure(ssh.clone(), Some("box name".into()), None)
            .unwrap();
        let b = book.ensure(ssh, Some("box name".into()), None).unwrap();
        assert_eq!(a.name, "box-name");
        assert_eq!(b.id, a.id, "同一 SSH 目标必须复用机器记录");
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("json"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn different_name_is_suffixed_and_persisted() {
        let (book, path) = book();
        let a = book
            .ensure(
                SshConnectionConfig::new("a.example", "u", AuthMethod::Agent),
                Some("box".into()),
                None,
            )
            .unwrap();
        let b = book
            .ensure(
                SshConnectionConfig::new("b.example", "u", AuthMethod::Agent),
                Some("box".into()),
                None,
            )
            .unwrap();
        assert_eq!(a.name, "box");
        assert_eq!(b.name, "box-2");
        let reloaded = MachineBook::load(path.clone()).unwrap();
        assert_eq!(reloaded.snapshot().len(), 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn session_is_restricted_before_it_reaches_a_shell() {
        let (book, path) = book();
        let ssh = SshConnectionConfig::new("host", "u", AuthMethod::Agent);
        assert!(
            book.ensure(ssh.clone(), None, Some("bad;rm -rf /".into()))
                .is_err()
        );
        assert!(
            book.ensure(ssh.clone(), None, Some("ok.session-1".into()))
                .is_ok()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn replies_and_nested_lists_are_prefixed_once() {
        let (book, path) = book();
        let machine = book
            .ensure(
                SshConnectionConfig::new("host", "u", AuthMethod::Agent),
                Some("box".into()),
                None,
            )
            .unwrap();
        let runtime = MachineRuntime::start(book.clone(), machine.id.clone());
        let mut reply = json!({"ok": true, "address": "t3", "terminals": [{"address": "t1"}]});
        localize_reply(&runtime, &mut reply);
        assert_eq!(reply["address"], "box/t3");
        assert_eq!(reply["terminals"][0]["address"], "box/t1");
        localize_reply(&runtime, &mut reply);
        assert_eq!(reply["address"], "box/t3", "已有前缀不能再加一次");
        runtime.stop();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn machine_addresses_require_a_nonempty_prefix_and_terminal() {
        let machines = Machines::empty();
        assert!(machines.resolve("box/t1").is_none());
        assert!(machines.resolve("/t1").is_none());
        assert!(machines.resolve("box/").is_none());
        assert!(machines.resolve("box/t1/extra").is_none());
        assert!(machines.resolve("t1").is_none());
    }

    #[test]
    fn remote_command_checks_install_before_anything_else() {
        let command = remote_connect_command(Some("ok"));
        assert!(command.contains("DEEPTERM_RUNTIME_DIR"));
        assert!(!command.contains("%s"), "命令里不能留下未替换的格式占位符");
        assert!(command.contains(MISSING_MARKER));
        assert!(command.contains("connect"));
    }

    #[test]
    fn app_spawn_timeout_outlives_the_supervisor_start_wait() {
        assert!(MACHINE_SPAWN_TIMEOUT > MACHINE_START_TIMEOUT);
    }

    /// **配置变了才作废旧 line，配置没变不能重拨。**
    ///
    /// 回归价值：`Machines::ensure` 每次连接都会被调用，而旧实现只更新
    /// `machines.json`、从不碰活着的 line——用户改了端口后，监督线程会继续
    /// 对着**旧目标**说话，直到 45 秒静默超时。反过来，如果无条件作废，
    /// 每条 GUI 请求都会踢掉一条健康的 SSH 连接。
    #[test]
    fn a_config_change_invalidates_the_line_but_an_identical_one_does_not() {
        let (book, path) = book();
        let client = Arc::new(
            Client::from_transport(std::io::Cursor::new(Vec::<u8>::new()), Vec::<u8>::new())
                .expect("in-memory client"),
        );
        let original = SshConnectionConfig::new("old.example", "root", AuthMethod::Agent);
        let runtime = MachineRuntime {
            book,
            id: "box".into(),
            state: Mutex::new(RuntimeState {
                status: LinkStatus {
                    kind: LinkKind::Connected,
                    since_ms: now_ms(),
                    why: None,
                },
                live: Some(LiveLine {
                    generation: 3,
                    client,
                    config: original.clone(),
                }),
                generation: 3,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            install_requested: AtomicBool::new(false),
            install_force: AtomicBool::new(false),
        };

        // 同样的配置（含代理标记这种"本次连接的选择"）：保持不动。
        let mut same = original.clone();
        same.use_system_proxy = true;
        runtime.invalidate_if_config_changed(&same);
        assert!(
            runtime.line_is_current(3),
            "配置没变不该重拨，否则每次请求都踢掉健康的 SSH 连接"
        );

        // 端口变了：必须作废。
        let mut changed = original.clone();
        changed.port = 2222;
        runtime.invalidate_if_config_changed(&changed);
        assert!(!runtime.line_is_current(3), "端口变了必须作废旧 line");
        assert_eq!(runtime.status().kind, LinkKind::Unreachable);

        let _ = std::fs::remove_file(path);
    }

    /// 半开 SSH 上 `Client::is_closed()` 可能永远为 false；只要运行时把
    /// `live` 清掉，监督线程就必须能从 generation 判断自己已被作废。
    #[test]
    fn an_invalidated_line_is_no_longer_current_and_keeps_its_reason() {
        let (book, path) = book();
        let client = Arc::new(
            Client::from_transport(std::io::Cursor::new(Vec::<u8>::new()), Vec::<u8>::new())
                .expect("in-memory client"),
        );
        let runtime = MachineRuntime {
            book,
            id: "box".into(),
            state: Mutex::new(RuntimeState {
                status: LinkStatus {
                    kind: LinkKind::Connected,
                    since_ms: now_ms(),
                    why: None,
                },
                live: Some(LiveLine {
                    generation: 7,
                    client,
                    config: SshConnectionConfig::new("host", "u", AuthMethod::Agent),
                }),
                generation: 7,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            install_requested: AtomicBool::new(false),
            install_force: AtomicBool::new(false),
        };

        assert!(runtime.line_is_current(7));
        runtime.invalidate_line(7, "远端 attach 超时");
        assert!(!runtime.line_is_current(7));
        assert_eq!(runtime.status().kind, LinkKind::Unreachable);
        assert_eq!(
            runtime.status().why.as_deref(),
            Some("line 失活: 远端 attach 超时")
        );
        let _ = std::fs::remove_file(path);
    }

    /// 静默 line 不能继续承载恢复。
    ///
    /// 回归价值：GUI 重启接回远端时，本地 core 可能还留着一条 TCP 尚未
    /// 关闭、对端其实已经不可达的 SSH line。此时 `is_closed()` 为 false，
    /// 直接 attach 会白等 30 秒后才失败；必须按最后收到字节的时间先丢弃。
    #[test]
    fn attach_does_not_reuse_a_silent_line() {
        let (book, path) = book();
        let client = Arc::new(
            Client::from_transport(std::io::Cursor::new(Vec::<u8>::new()), Vec::<u8>::new())
                .expect("in-memory client"),
        );
        let runtime = MachineRuntime {
            book,
            id: "box".into(),
            state: Mutex::new(RuntimeState {
                status: LinkStatus {
                    kind: LinkKind::Connected,
                    since_ms: now_ms(),
                    why: None,
                },
                live: Some(LiveLine {
                    generation: 9,
                    client,
                    config: SshConnectionConfig::new("host", "u", AuthMethod::Agent),
                }),
                generation: 9,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            install_requested: AtomicBool::new(false),
            install_force: AtomicBool::new(false),
        };

        std::thread::sleep(Duration::from_millis(30));
        let error = match runtime
            .wait_for_usable_line_for(Duration::from_millis(50), Duration::from_millis(10))
        {
            Ok(_) => panic!("静默 line 不能被复用"),
            Err(error) => error,
        };
        assert!(error.contains("静默"), "{error}");
        assert!(!runtime.line_is_current(9));
        assert_eq!(runtime.status().kind, LinkKind::Unreachable);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn identity_refresh_preserves_an_unknown_linger_state() {
        let (book, path) = book();
        let machine = book
            .ensure(
                SshConnectionConfig::new("host", "u", AuthMethod::Agent),
                None,
                None,
            )
            .unwrap();
        book.update_identity(
            &machine.id,
            &json!({
                "version": "0.1.0",
                "core_id": "core-1",
                "hostname": "box",
                "keeps_running_after_logout": null,
                "logout_linger_user": "u",
            }),
        );
        let refreshed = book.get(&machine.id).expect("machine");
        assert_eq!(refreshed.keeps_running_after_logout, None);
        assert_eq!(refreshed.logout_linger_user.as_deref(), Some("u"));
        let _ = std::fs::remove_file(path);
    }
}
