//! 端口转发：把**本机服务**通过 SSH 暴露到远端（`ssh -R` 语义）。
//!
//! 场景：本机跑着某个只监听回环的服务（开发服务器、代理、数据库…），
//! 想让远端服务器上的程序也能调用它。远端进程连它自己的
//! `127.0.0.1:<bind_port>`，字节经 SSH 送回本机，由本模块桥接到
//! `target_host:target_port`。
//!
//! # 两条传输路径
//!
//! - russh（默认）：连接建立后发 `tcpip-forward` 全局请求，远端开始监听；
//!   有人连上来时服务器发起 `forwarded-tcpip` 通道，由 [`RemoteForwardTable`]
//!   认领并桥接到本机目标。
//! - 系统 ssh（FIDO2 / Touch ID 密钥走这条）：等价于给 ssh 加
//!   `-R bind:target`，隧道由 ssh 进程自己维护，本模块不参与。
//!
//! # 隧道按主机共享（进程级单例）
//!
//! 反向监听端口是**远端资源**，一个 `<bind_host>:<bind_port>` 在同一台远端
//! 上只能被一个 sshd 会话持有。若每个终端标签各自建连、各自发一次
//! `tcpip-forward`，那么第二个标签起必然被远端拒绝（`RequestDenied`，
//! 远端日志里是「port already in use」）——而占住端口的恰恰是本进程自己。
//! 用户看到的就是「每开一个标签弹一次端口被占用」。
//!
//! 因此隧道不再绑定在「某一条终端连接」上，而是按主机（`user@host:port`）
//! 在进程内共享（见 [`crate::ssh::tunnel`]）：那里保存一条**专职隧道连接**，
//! 它不承载 shell，只负责监听与桥接；同一主机的所有终端标签共用它。首个
//! 标签建隧道，后续标签直接复用（不再重发 `tcpip-forward`，也就不会再撞
//! 端口）。
//!
//! 隧道连接与承载 shell 的终端连接是两条独立连接：终端标签关掉不影响隧道
//! （其它标签还在用）。隧道按主机常驻进程，不随最后一个标签关闭而消失——
//! 否则「关掉再打开」会在旧监听的释放与新请求之间赛跑，重新撞上同一个端口。
//! 隧道连接断开（网络故障、远端重启）时下一次使用自动重建，重连后远端端口
//! 已空出，不会残留。
//! # 边界
//!
//! - 默认只绑远端回环：只有那台机器自己能用这条隧道。要暴露给别的机器，
//!   由用户在 `bind_host` 里显式写 `0.0.0.0`——这是用户的决定，不是默认值。
//! - 转发是**附加**能力：远端拒绝（端口被占、sshd 禁了 AllowTcpForwarding）
//!   只报警告，不阻断登录。**但「被拒」不等于「不可用」**：端口上已经有人在
//!   听时（同一进程早先的连接、用户自己的 `ssh -R`），效果已经成立，
//!   此时接手该监听、静默复用，而不是弹一条「端口可能已被占用」的假警报。
//! - 隧道**不跨主机复用**：`0.0.0.0:3801` 在 A 机与 B 机互不影响，各自的
//!   隧道各自绑定。同一主机上重复配置同一条规则（同一 bind 端口）会被
//!   认领表拦下，按该条失败处理（不会静默丢一条）。

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use parking_lot::Mutex;
use russh::Channel;
use russh::client;
use serde::{Deserialize, Serialize};

/// 一条反向端口转发规则。
///
/// `Hash` 与 `PartialEq`/`Eq` 一起 derive：`SshConnectionConfig` 的手写
/// `Hash` 要哈希这个 `Vec`（服务器表按主机复用 LSP 进程），纯数据结构
/// 没有自定义相等语义，derive 即可。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PortForwardRule {
    /// 远端监听地址。默认 `127.0.0.1`（只本机可用）；写 `0.0.0.0` 即对外暴露。
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    /// 远端监听端口。
    pub bind_port: u16,
    /// 本机目标地址。
    #[serde(default = "default_bind_host")]
    pub target_host: String,
    /// 本机目标端口。
    pub target_port: u16,
    /// 备注（仅用于显示）。
    #[serde(default)]
    pub description: String,
}

fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}

impl PortForwardRule {
    /// 一行摘要：`127.0.0.1:3801 → 本机 127.0.0.1:3801`。
    pub fn summary(&self) -> String {
        format!(
            "远端 {}:{} → 本机 {}:{}",
            self.bind_host, self.bind_port, self.target_host, self.target_port
        )
    }

    /// 系统 ssh 的 `-R` 取值：`bind_host:bind_port:target_host:target_port`。
    pub fn openssh_arg(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.bind_host, self.bind_port, self.target_host, self.target_port
        )
    }

    /// 规则是否填全（表单里用来决定「添加」是否可点）。
    ///
    /// `bind_port == 0` **算填全**：`ssh -R 0:...` 与 russh 的 `tcpip-forward` 都
    /// 把 0 解释为「让远端自己挑一个端口」，是合法请求（[`establish`] 会按服务器
    /// 回报的端口登记认领）。缺目标端口/目标地址才是真的没填完。
    pub fn is_complete(&self) -> bool {
        self.target_port > 0 && !self.target_host.trim().is_empty()
    }
}

/// 去掉 IPv6 字面量的方括号：`[::1]` → `::1`。
///
/// 表单输入与 `ssh -R` 参数用带方括号的形态（OpenSSH 语法要求），但
/// `tcpip-forward` 全局请求里的地址是裸字面量，服务器回报的也是裸的；
/// 认领表必须按 wire 形态存取，否则永远查不到。
pub(crate) fn wire_host(bind_host: &str) -> &str {
    bind_host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(bind_host)
}

/// 认领表的键：远端绑定地址与端口。
type BindKey = (String, u16);
/// 认领表的值：本机目标主机与端口。
type Target = (String, u16);

/// 远端 `forwarded-tcpip` 通道的认领表：(远端绑定地址, 端口) → (本机目标主机, 端口)。
///
/// 服务器回报的 `connected_address` 不保证与请求时的字面量一致（`127.0.0.1` 可能
/// 回成 `localhost`），所以精确匹配失败后还允许「端口唯一即认领」。
#[derive(Clone, Default)]
pub struct RemoteForwardTable {
    inner: Arc<Mutex<HashMap<BindKey, Target>>>,
}

impl RemoteForwardTable {
    /// 登记一条转发；同一绑定地址+端口已存在时返回 false。
    pub fn register(
        &self,
        bind_host: &str,
        bind_port: u16,
        target_host: &str,
        target_port: u16,
    ) -> bool {
        use std::collections::hash_map::Entry;
        match self.inner.lock().entry((bind_host.to_string(), bind_port)) {
            Entry::Occupied(_) => false,
            Entry::Vacant(slot) => {
                slot.insert((target_host.to_string(), target_port));
                true
            }
        }
    }

    pub fn unregister(&self, bind_host: &str, bind_port: u16) {
        self.inner
            .lock()
            .remove(&(bind_host.to_string(), bind_port));
    }

    /// 认领一条服务器发起的转发连接。
    pub fn lookup(&self, connected_address: &str, connected_port: u16) -> Option<Target> {
        let table = self.inner.lock();
        if let Some(target) = table.get(&(connected_address.to_string(), connected_port)) {
            return Some(target.clone());
        }
        // 地址字面量可能被服务器改写；同一端口只有一条转发时按端口认领。
        let mut same_port = table
            .iter()
            .filter(|((_, port), _)| *port == connected_port)
            .map(|(_, target)| target);
        match (same_port.next(), same_port.next()) {
            (Some(target), None) => Some(target.clone()),
            _ => None,
        }
    }
}

/// 把一条服务器发起的转发通道桥接到本机目标。
pub async fn serve_forwarded_channel(
    channel: Channel<client::Msg>,
    target_host: String,
    target_port: u16,
) -> io::Result<()> {
    let stream = tokio::net::TcpStream::connect((target_host.as_str(), target_port)).await?;
    bridge(stream, channel.into_stream()).await
}

/// 双向拷贝，任一侧结束时关闭另一侧的写方向。
async fn bridge<A, B>(a: A, b: B) -> io::Result<()>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;

    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    let a_to_b = async {
        tokio::io::copy(&mut ar, &mut bw).await?;
        bw.shutdown().await
    };
    let b_to_a = async {
        tokio::io::copy(&mut br, &mut aw).await?;
        aw.shutdown().await
    };

    tokio::try_join!(a_to_b, b_to_a)?;
    Ok(())
}

/// 单条规则建立成功后的结果（含远端实际端口，`bind_port == 0` 时由服务器分配）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EstablishedForward {
    pub bind_host: String,
    pub bind_port: u16,
    pub target_host: String,
    pub target_port: u16,
    /// 这条监听是**别人**已经在远端开着、我们只是接手使用的（见
    /// [`probe_existing_listener`]）。
    ///
    /// 撤销时必须区分：`cancel_tcpip_forward` 只能撤我们自己请求成功的监听；
    /// 对别人的监听发撤销请求不但无效，还可能把用户另一个会话的转发弄掉。
    pub adopted: bool,
}

impl EstablishedForward {
    /// 一行摘要：`远端 127.0.0.1:3801 → 本机 127.0.0.1:3801`。
    pub fn summary(&self) -> String {
        format!(
            "远端 {}:{} → 本机 {}:{}",
            self.bind_host, self.bind_port, self.target_host, self.target_port
        )
    }
}

/// 探测「远端该端口是否已经有人在听」的等待上限。
///
/// 刻意与 `tcpip_forward` 的请求超时分开：探测发生在**建立锁**内（同一主机
/// 同时只能有一个标签在建隧道），用 30s 去等一个连不上的端口会把后来所有标签
/// 一起堵住。而探测的目的只是确认「端口上有没有服务」——本机回环上一个无人
/// 监听的端口会立即回 RST，根本不需要等。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 请求监听被拒后，探一下远端该端口是否已经有人在听。
///
/// 用 `direct-tcpip` 通道去连 `bind_host:bind_port`：连得上就说明监听存在
/// （sshd 会替我们完成这次 TCP 连接，因此探测本身不依赖远端有别的工具）。
///
/// 典型来源是同一个 DeepTerm 进程里早先建立的监听（旧版本没有隧道共享，每个
/// 标签各自 `tcpip-forward`；或本次进程上一次连接留下的会话），也可能是用户
/// 自己跑的 `ssh -R`。无论哪种，用户想要的「远端能连上这个端口」这个**效果**
/// 已经成立，把它报成失败是假警报。
///
/// 只对**可连接的具体地址**做探测：`0.0.0.0` / `::` 是通配地址，不是可连接的
/// 目的地址，「已有人在听」这个判断在它们身上本就无从区分（可能是别的接口），
/// 因此直接判为未探明，按原来的失败处理——宁可多报一次，也不要谎称转发可用。
///
/// 探测失败（连不上、超时）一律当作「没有人在听」，也就是原来的失败语义。
///
/// `pub(crate)`：`tunnel` 也要用它复查接手的监听是否还在（见
/// `tunnel::prune_stale_adopted`）。
pub(crate) async fn probe_existing_listener<H: client::Handler>(
    handle: &client::Handle<H>,
    bind_host: &str,
    bind_port: u16,
) -> bool {
    if bind_port == 0 || bind_host == "0.0.0.0" || bind_host == "::" {
        return false;
    }
    let probe = handle.channel_open_direct_tcpip(
        bind_host.to_string(),
        u32::from(bind_port),
        "127.0.0.1".to_string(),
        0,
    );
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(channel)) => {
            // 连上就立刻关掉：我们要的只是「有人监听」这个事实，不产生业务流量。
            let _ = channel.close().await;
            true
        }
        Ok(Err(_)) | Err(_) => false,
    }
}

/// 建立一条反向转发：请求远端监听，并把认领登记进 `forwards`。
///
/// `timeout` 限制单条请求的等待：`tcpip_forward` 是全局请求，要等服务器回复，
/// 而这一段不在连接握手超时（`config.timeout` 只包 `client::connect`）范围内。
/// 不回或慢回的服务器不能把登录一起拖死——超时按该条失败处理。
///
/// `allow_adopt`：请求被拒时是否允许「探到端口上已经有人在听就接手」。
/// 调用方在「同绑定位上占着的是别人的监听、而规则又变了」时传 `false`——
/// 那种情况下接手会登记一个永远收不到流量的目标（详见 `tunnel::supersedes_adopted`）。
///
/// 调用方负责在连接断开时让 `forwards` 随之失效——表本身按连接持有，
/// 连接没了表也就没了，无需显式清理。
pub async fn establish<H: client::Handler>(
    handle: &client::Handle<H>,
    rule: &PortForwardRule,
    forwards: &RemoteForwardTable,
    timeout: std::time::Duration,
    allow_adopt: bool,
) -> anyhow::Result<EstablishedForward> {
    let wire = wire_host(&rule.bind_host);
    // 固定端口的规则先登记再请求：服务器可能在 tcpip_forward 回复送达前就
    // 放连接进来，认领必须已经在表里。端口 0 的规则最终端口未知、无从预登记，
    // 只能在服务器分配后登记——分配与登记之间到达的连接会被当「未登记」丢弃，
    // 这个窗口无法消除（服务器分配了什么端口只有回复才知道）。
    let pre_registered = rule.bind_port != 0;
    if pre_registered
        && !forwards.register(wire, rule.bind_port, &rule.target_host, rule.target_port)
    {
        return Err(anyhow::anyhow!(
            "远端 {}:{} 在本次连接里已登记过",
            rule.bind_host,
            rule.bind_port
        ));
    }

    let assigned = match tokio::time::timeout(
        timeout,
        handle.tcpip_forward(wire.to_string(), u32::from(rule.bind_port)),
    )
    .await
    {
        Err(_) => {
            if pre_registered {
                forwards.unregister(wire, rule.bind_port);
            }
            return Err(anyhow::anyhow!(
                "远端 {} 秒内未回应监听 {}:{} 的请求，已跳过该条",
                timeout.as_secs(),
                rule.bind_host,
                rule.bind_port
            ));
        }
        Ok(Err(e)) => {
            // 被拒 ≠ 转发不可用。先探一下端口上是不是已经有人听着：本进程
            // 早先的连接（旧版本每个标签各自建监听）、用户自己的 `ssh -R`
            // 都会占住端口，而那种情况下「远端能连上这个端口」这个用户真正
            // 要的效果已经成立。此时把监听认下来、静默复用，而不是弹一条
            // 「端口可能已被占用」的假警报（用户看到的是「明明好用却报错」）。
            if allow_adopt && probe_existing_listener(handle, wire, rule.bind_port).await {
                log::info!(
                    "russh: 远端 {}:{} 已有监听在服务，复用该监听而不重复请求（{e}）",
                    rule.bind_host,
                    rule.bind_port
                );
                return Ok(EstablishedForward {
                    bind_host: rule.bind_host.clone(),
                    bind_port: rule.bind_port,
                    target_host: rule.target_host.clone(),
                    target_port: rule.target_port,
                    adopted: true,
                });
            }
            if pre_registered {
                forwards.unregister(wire, rule.bind_port);
            }
            return Err(anyhow::anyhow!(
                "远端拒绝监听 {}:{}（{e}）——端口可能已被占用，或 sshd 关闭了转发",
                rule.bind_host,
                rule.bind_port
            ));
        }
        Ok(Ok(assigned)) => assigned,
    };

    // 请求端口 0 时服务器在回复里给出实际端口；请求了具体端口则回复为空。
    let bind_port = if rule.bind_port == 0 {
        let assigned = assigned as u16;
        if !forwards.register(wire, assigned, &rule.target_host, rule.target_port) {
            // 分配到的端口撞上本次连接里另一条规则：服务器保证端口唯一，
            // 走到这里说明配置有重叠，撤掉监听、按失败处理。
            let _ = handle
                .cancel_tcpip_forward(wire.to_string(), u32::from(assigned))
                .await;
            return Err(anyhow::anyhow!(
                "远端分配的端口 {} 与本次连接里另一条规则冲突",
                assigned
            ));
        }
        assigned
    } else {
        rule.bind_port
    };

    Ok(EstablishedForward {
        bind_host: rule.bind_host.clone(),
        bind_port,
        target_host: rule.target_host.clone(),
        target_port: rule.target_port,
        adopted: false,
    })
}

/// 结果文案（toast 用）：**只在状态变化时报**——新建了转发、或失败了。
///
/// 纯复用（隧道早就装好、这次什么都没变）返回 `None`：转发是按主机共享的，
/// 每开一个标签都弹一次「已就绪」就是噪音，与旧实现「每开一个标签撞一次
/// 端口」是同一个毛病的一体两面。复用只写日志。
///
/// **纯接手**同样返回 `None`：接手是「端口上已经有东西在听、效果已经成立」
/// 的情形（典型来源：同一个 DeepTerm 进程里早先建立的监听）。用户要的是
/// 「远端能连上」，那件事本来就成立，弹一条提示只是把以前的假警报换个说法。
/// 与新建/失败混在一起时才顺带说一句，让用户知道哪些不是本次建的。
///
/// `reused` 仅在**部分**复用（新加了规则、其余沿用）时出现在文案里，让用户
/// 知道哪些是这次新建的、哪些本来就在。
pub fn tunnel_outcome_message(
    established: &[EstablishedForward],
    reused: &[EstablishedForward],
    adopted: &[EstablishedForward],
    errors: &[String],
) -> Option<(bool, String)> {
    // 只有「本次新建了东西」或「有失败」才值得打扰用户。接手（adopted）单独
    // 出现时不算：那意味着端口上早就有东西在听、用户要的效果本来就成立。
    if established.is_empty() && errors.is_empty() {
        return None;
    }
    let summarize = |items: &[EstablishedForward]| {
        items
            .iter()
            .map(EstablishedForward::summary)
            .collect::<Vec<_>>()
            .join("；")
    };
    let ok = errors.is_empty();
    let mut parts = Vec::new();
    if !established.is_empty() {
        parts.push(format!("已就绪：{}", summarize(established)));
    }
    if !adopted.is_empty() {
        // 接手的监听不是本次建的，也不能由我们撤销；文案要说清「能用」
        // 与「不是我们建的」这两件事。
        parts.push(format!("已沿用远端已有监听：{}", summarize(adopted)));
    }
    if !reused.is_empty() {
        parts.push(format!(
            "沿用已有隧道：{}（同一主机只监听一次）",
            summarize(reused)
        ));
    }
    if !errors.is_empty() {
        parts.push(format!("失败 {}：{}", errors.len(), errors.join("；")));
    }
    Some((ok, parts.join("；")))
}

/// 系统 ssh 路径要加的 `-R` 参数（逐条规则一个）。
pub fn openssh_args(rules: &[PortForwardRule]) -> Vec<String> {
    let mut args = Vec::new();
    for rule in rules.iter().filter(|r| r.is_complete()) {
        args.push("-R".to_string());
        args.push(rule.openssh_arg());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(bind_port: u16, target_port: u16) -> PortForwardRule {
        PortForwardRule {
            bind_host: "127.0.0.1".to_string(),
            bind_port,
            target_host: "127.0.0.1".to_string(),
            target_port,
            description: String::new(),
        }
    }

    #[test]
    fn openssh_arg_matches_ssh_r_grammar() {
        let r = rule(3801, 3801);
        assert_eq!(r.openssh_arg(), "127.0.0.1:3801:127.0.0.1:3801");

        let mut wildcard = rule(9000, 3000);
        wildcard.bind_host = "0.0.0.0".to_string();
        assert_eq!(wildcard.openssh_arg(), "0.0.0.0:9000:127.0.0.1:3000");

        // 端口 0 表示「让远端自己挑」，是合法请求，不应被过滤掉
        assert!(rule(0, 3000).is_complete());
        // 目标端口/主机缺失才是没填完
        assert!(!rule(9000, 0).is_complete());
        let mut no_host = rule(9000, 3000);
        no_host.target_host = "  ".to_string();
        assert!(!no_host.is_complete());
    }

    #[test]
    fn openssh_args_skips_incomplete_rules() {
        let args = openssh_args(&[rule(3801, 3801), rule(9000, 0)]);
        assert_eq!(
            args,
            vec![
                "-R".to_string(),
                "127.0.0.1:3801:127.0.0.1:3801".to_string()
            ]
        );
        assert!(openssh_args(&[]).is_empty());
    }

    #[test]
    fn forward_table_claims_by_exact_then_unique_port() {
        let table = RemoteForwardTable::default();
        assert!(table.register("127.0.0.1", 3801, "127.0.0.1", 3801));
        assert!(
            !table.register("127.0.0.1", 3801, "127.0.0.1", 9999),
            "同一绑定端口不能被登记两次"
        );

        assert_eq!(
            table.lookup("127.0.0.1", 3801),
            Some(("127.0.0.1".to_string(), 3801))
        );
        // 服务器把地址回报成 localhost 时按端口认领
        assert_eq!(
            table.lookup("localhost", 3801),
            Some(("127.0.0.1".to_string(), 3801))
        );
        assert_eq!(table.lookup("127.0.0.1", 22), None);

        table.unregister("127.0.0.1", 3801);
        assert_eq!(table.lookup("127.0.0.1", 3801), None);
    }

    #[test]
    fn wire_host_strips_ipv6_brackets() {
        assert_eq!(wire_host("[::1]"), "::1");
        assert_eq!(wire_host("[2001:db8::9]"), "2001:db8::9");
        // 非 IPv6 原样返回
        assert_eq!(wire_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(wire_host("0.0.0.0"), "0.0.0.0");
        // 只有成对的括号才剥：半截括号是非法输入，不该被「修好」
        assert_eq!(wire_host("[::1"), "[::1");
        assert_eq!(wire_host("::1]"), "::1]");
    }

    #[test]
    fn assigned_ports_can_coexist_for_multiple_port_zero_rules() {
        // 端口 0 的规则在服务器分配后才登记，因此多条「让远端挑端口」的规则
        // 可以共存（各自拿到不同的实际端口），不像预登记那样互相顶掉。
        let table = RemoteForwardTable::default();
        assert!(table.register("127.0.0.1", 40001, "127.0.0.1", 3000));
        assert!(table.register("127.0.0.1", 40002, "127.0.0.1", 3001));
        assert_eq!(
            table.lookup("127.0.0.1", 40001),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(
            table.lookup("127.0.0.1", 40002),
            Some(("127.0.0.1".to_string(), 3001))
        );
    }

    #[test]
    fn ambiguous_port_is_not_claimed() {
        let table = RemoteForwardTable::default();
        // 两条不同绑定地址、同一端口：地址对不上时不能瞎认一条
        table.register("127.0.0.1", 5000, "127.0.0.1", 1);
        table.register("0.0.0.0", 5000, "127.0.0.1", 2);
        assert_eq!(table.lookup("elsewhere", 5000), None);
        assert_eq!(
            table.lookup("127.0.0.1", 5000),
            Some(("127.0.0.1".to_string(), 1))
        );
        assert_eq!(
            table.lookup("0.0.0.0", 5000),
            Some(("127.0.0.1".to_string(), 2))
        );
    }

    #[test]
    fn tunnel_outcome_message_distinguishes_none_new_reused_and_failure() {
        let done = EstablishedForward {
            bind_host: "127.0.0.1".to_string(),
            bind_port: 3801,
            target_host: "127.0.0.1".to_string(),
            target_port: 3801,
            adopted: false,
        };

        assert_eq!(
            tunnel_outcome_message(&[], &[], &[], &[]),
            None,
            "没启用规则时不该弹提示"
        );

        let (ok, message) =
            tunnel_outcome_message(std::slice::from_ref(&done), &[], &[], &[]).unwrap();
        assert!(ok);
        assert!(message.contains("3801"), "{message}");
        assert!(message.contains("已就绪"), "{message}");

        // 纯复用要**静默**：转发按主机共享，每开一个标签弹一次就是噪音。
        assert_eq!(
            tunnel_outcome_message(&[], std::slice::from_ref(&done), &[], &[]),
            None,
            "隧道早装好了、这次什么都没变，不该弹提示"
        );

        // 纯接手也要**静默**：端口上已经有人在听、效果已经成立，弹提示只是
        // 把以前的假警报换个说法。这是本次修复的核心断言。
        let mut adopted = done.clone();
        adopted.adopted = true;
        assert_eq!(
            tunnel_outcome_message(&[], &[], std::slice::from_ref(&adopted), &[]),
            None,
            "远端端口已在服务且转发可用时，不该弹任何提示"
        );

        // 但接手与失败同时出现时要说出来（用户得知道哪条没成）。
        let (ok, message) = tunnel_outcome_message(
            &[],
            &[],
            std::slice::from_ref(&adopted),
            &["另一条失败".to_string()],
        )
        .unwrap();
        assert!(!ok);
        assert!(
            message.contains("沿用远端已有监听") && message.contains("另一条失败"),
            "{message}"
        );

        // 部分复用（新加了规则）要把「新建的」和「沿用的」分开说。
        let added = EstablishedForward {
            bind_host: "127.0.0.1".to_string(),
            bind_port: 3802,
            target_host: "127.0.0.1".to_string(),
            target_port: 3802,
            adopted: false,
        };
        let (ok, message) = tunnel_outcome_message(
            std::slice::from_ref(&added),
            std::slice::from_ref(&done),
            &[],
            &[],
        )
        .unwrap();
        assert!(ok);
        assert!(
            message.contains("已就绪") && message.contains("3802"),
            "{message}"
        );
        assert!(
            message.contains("沿用已有隧道") && message.contains("3801"),
            "{message}"
        );

        let (ok, message) =
            tunnel_outcome_message(&[], &[], &[], &["端口被占用".to_string()]).unwrap();
        assert!(!ok);
        assert!(message.contains("端口被占用"), "{message}");

        // 部分成功必须两边都说，不能只报喜或只报忧
        let (ok, message) = tunnel_outcome_message(
            std::slice::from_ref(&done),
            &[],
            &[],
            &["另一条失败".to_string()],
        )
        .unwrap();
        assert!(!ok);
        assert!(
            message.contains("3801") && message.contains("另一条失败"),
            "{message}"
        );
    }

    #[test]
    fn rule_summary_is_readable() {
        assert_eq!(
            rule(3801, 8080).summary(),
            "远端 127.0.0.1:3801 → 本机 127.0.0.1:8080"
        );
    }

    #[tokio::test]
    async fn bridge_propagates_data_and_eof_both_directions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut client_a, a) = tokio::io::duplex(64);
        let (b, mut server_b) = tokio::io::duplex(64);
        let bridged = tokio::spawn(async move { bridge(a, b).await });

        client_a.write_all(b"ping").await.unwrap();
        client_a.shutdown().await.unwrap();

        let mut got = Vec::new();
        server_b.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"ping", "A→B 数据送达，A 侧 EOF 关闭 B 的读方向");

        server_b.write_all(b"pong").await.unwrap();
        server_b.shutdown().await.unwrap();
        let mut back = Vec::new();
        client_a.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"pong", "B→A 回复送达，B 侧 EOF 关闭 A 的读方向");

        bridged.await.unwrap().unwrap();
    }
}
