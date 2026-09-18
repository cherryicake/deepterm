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
//! # 边界
//!
//! - 默认只绑远端回环：只有那台机器自己能用这条隧道。要暴露给别的机器，
//!   由用户在 `bind_host` 里显式写 `0.0.0.0`——这是用户的决定，不是默认值。
//! - 转发是**附加**能力：远端拒绝（端口被占、sshd 禁了 AllowTcpForwarding）
//!   只报警告，不阻断登录。

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use parking_lot::Mutex;
use russh::Channel;
use russh::client;
use serde::{Deserialize, Serialize};

/// 一条反向端口转发规则。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    /// 把 0 解释为「让远端自己挑一个端口」，是合法请求（[`establish`] 会用服务器
    /// 回报的端口 rekey）。缺目标端口/目标地址才是真的没填完。
    pub fn is_complete(&self) -> bool {
        self.target_port > 0 && !self.target_host.trim().is_empty()
    }
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

    /// 服务器为「端口 0」请求分配了具体端口后，把登记挪到实际端口上。
    pub fn rekey(&self, bind_host: &str, from_port: u16, to_port: u16) {
        let mut table = self.inner.lock();
        if let Some(target) = table.remove(&(bind_host.to_string(), from_port)) {
            table.insert((bind_host.to_string(), to_port), target);
        }
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

/// 建立一条反向转发：请求远端监听，并把认领登记进 `forwards`。
///
/// 调用方负责在连接断开时让 `forwards` 随之失效——表本身按连接持有，
/// 连接没了表也就没了，无需显式清理。
pub async fn establish<H: client::Handler>(
    handle: &client::Handle<H>,
    rule: &PortForwardRule,
    forwards: &RemoteForwardTable,
) -> anyhow::Result<EstablishedForward> {
    if !forwards.register(
        &rule.bind_host,
        rule.bind_port,
        &rule.target_host,
        rule.target_port,
    ) {
        return Err(anyhow::anyhow!(
            "远端 {}:{} 在本次连接里已登记过",
            rule.bind_host,
            rule.bind_port
        ));
    }

    let assigned = match handle
        .tcpip_forward(rule.bind_host.clone(), u32::from(rule.bind_port))
        .await
    {
        Ok(assigned) => assigned,
        Err(e) => {
            forwards.unregister(&rule.bind_host, rule.bind_port);
            return Err(anyhow::anyhow!(
                "远端拒绝监听 {}:{}（{e}）——端口可能已被占用，或 sshd 关闭了转发",
                rule.bind_host,
                rule.bind_port
            ));
        }
    };

    // 请求端口 0 时服务器在回复里给出实际端口；请求了具体端口则回复为空。
    let bind_port = if rule.bind_port == 0 {
        assigned as u16
    } else {
        rule.bind_port
    };
    if bind_port != rule.bind_port {
        forwards.rekey(&rule.bind_host, rule.bind_port, bind_port);
    }

    Ok(EstablishedForward {
        bind_host: rule.bind_host.clone(),
        bind_port,
        target_host: rule.target_host.clone(),
        target_port: rule.target_port,
    })
}

/// 为 russh 连接装上全部启用的规则。
///
/// 返回 `(成功建立的转发, 失败原因)`：**逐条独立**，一条失败不影响其它条，
/// 也不影响登录本身。
pub async fn install_all<H: client::Handler>(
    handle: &client::Handle<H>,
    rules: &[PortForwardRule],
    forwards: &RemoteForwardTable,
) -> (Vec<EstablishedForward>, Vec<String>) {
    let mut established = Vec::new();
    let mut errors = Vec::new();
    for rule in rules.iter().filter(|r| r.is_complete()) {
        match establish(handle, rule, forwards).await {
            Ok(done) => established.push(done),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    (established, errors)
}

/// 结果文案（toast 用）。`None` 表示没有启用任何规则。
pub fn outcome_message(
    established: &[EstablishedForward],
    errors: &[String],
) -> Option<(bool, String)> {
    match (established.is_empty(), errors.is_empty()) {
        (true, true) => None,
        (false, true) => Some((
            true,
            format!(
                "端口转发已就绪：{}",
                established
                    .iter()
                    .map(EstablishedForward::summary)
                    .collect::<Vec<_>>()
                    .join("；")
            ),
        )),
        (true, false) => Some((false, errors.join("；"))),
        (false, false) => Some((
            false,
            format!(
                "已转发 {}，失败 {}：{}",
                established
                    .iter()
                    .map(EstablishedForward::summary)
                    .collect::<Vec<_>>()
                    .join("；"),
                errors.len(),
                errors.join("；")
            ),
        )),
    }
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

        table.rekey("127.0.0.1", 3801, 4000);
        assert_eq!(table.lookup("127.0.0.1", 3801), None);
        assert_eq!(
            table.lookup("127.0.0.1", 4000),
            Some(("127.0.0.1".to_string(), 3801))
        );

        table.unregister("127.0.0.1", 4000);
        assert_eq!(table.lookup("127.0.0.1", 4000), None);
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
    fn outcome_message_distinguishes_none_success_and_failure() {
        assert_eq!(outcome_message(&[], &[]), None, "没启用规则时不该弹提示");

        let ok = EstablishedForward {
            bind_host: "127.0.0.1".to_string(),
            bind_port: 3801,
            target_host: "127.0.0.1".to_string(),
            target_port: 3801,
        };
        let (ready, message) = outcome_message(std::slice::from_ref(&ok), &[]).unwrap();
        assert!(ready);
        assert!(message.contains("3801"), "{message}");

        let (ready, message) = outcome_message(&[], &["端口被占用".to_string()]).unwrap();
        assert!(!ready);
        assert!(message.contains("端口被占用"), "{message}");

        // 部分成功必须两边都说，不能只报喜或只报忧
        let (ready, message) =
            outcome_message(std::slice::from_ref(&ok), &["另一条失败".to_string()]).unwrap();
        assert!(!ready);
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
