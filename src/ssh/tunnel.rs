//! 端口转发隧道：同一主机的所有终端标签**共享一条专职隧道连接**。
//!
//! # 为什么需要它
//!
//! 反向监听端口是**远端资源**：`<bind_host>:<bind_port>` 在同一台远端上只能被
//! 一个 SSH 会话持有。若每个终端标签各自建连、各自发一次 `tcpip-forward`，
//! 第二个标签起必然被远端拒绝（`RequestDenied`，远端日志里是「port already
//! in use」）——而占住端口的恰恰是本进程上一个标签自己。用户看到的现象就是
//! 「每开一个标签弹一次端口被占用」。
//!
//! 这里把隧道从「某条终端连接」上摘出来，按主机（`user@host:port`）在进程内
//! 共享：
//!
//! - 首个需要转发的标签建立隧道（一条不承载 shell、只做转发的 SSH 连接）；
//! - 后续标签复用同一条隧道，**不再重复请求监听**，也就不会再撞端口；
//! - 隧道连接断开（网络故障、远端重启）时下一次使用自动重建；
//! - 规则变更时**增量对齐**：新增的补建、移除的撤销，已有的一条都不动。
//!
//! # 生命周期
//!
//! 隧道按主机常驻进程（取舍与 OpenSSH `ControlPersist` 一致），不随某个标签
//! 关闭而消失：否则「关掉所有标签再打开」会在旧监听的释放与新请求之间赛跑，
//! 重新撞上同一个端口——那正是本模块要消灭的现象。进程退出时 TCP 断开，
//! 远端监听随之撤销，不留残余。
//!
//! 隧道与承载 shell 的连接是两条独立连接：关掉任何一个标签都不影响其它标签
//! 正在使用的转发。
//!
//! # 与系统 ssh 路径的关系
//!
//! 系统 ssh（FIDO2 / Touch ID 密钥）路径没有 russh 连接可用，反向隧道只能由
//! 它自己的 `-R` 承担，本模块不参与建立；但若该主机已有活着的共享隧道（另一个
//! russh 标签建的），[`rules_for_openssh`] 会把已覆盖的规则摘掉——再绑一次只会
//! 拿到同样的「端口被占用」警告。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use russh::client;

use crate::ssh::config::SshConnectionConfig;
use crate::ssh::port_forward::{
    self, EstablishedForward, PortForwardRule, RemoteForwardTable, wire_host,
};
use crate::ssh::russh_terminal::SshClientHandler;

/// 隧道的主机键：`user@host:port`。
///
/// 端口转发是「远端监听」语义，与本地无关；同一台主机的不同凭据共用一条隧道
/// 不会跨越安全边界（隧道本身就是用该主机的配置建立的）。
pub fn host_key(config: &SshConnectionConfig) -> String {
    format!("{}@{}:{}", config.username, config.host, config.port)
}

/// 一条已装上的转发：原始规则 + 远端实际绑定。
///
/// 规则（而不是「绑定地址 + 端口」）才是复用的判据：`bind_port == 0` 的规则
/// 每次请求都会拿到不同的随机端口，按端口比对会把同一条规则认成新的，于是
/// 每开一个标签就多绑一个监听——泄漏。按规则比对，同一条规则只绑一次。
#[derive(Clone, Debug, PartialEq, Eq)]
struct InstalledForward {
    rule: PortForwardRule,
    established: EstablishedForward,
}

/// 两条规则是否**在转发语义上**等价。
///
/// `description` 是纯展示字段（见 [`PortForwardRule::description`]）：改个备注
/// 不该让监听重建——重建期间端口会短暂空出来，可能被别的进程抢走。比对时
/// 忽略它，只比真正决定隧道的四个字段。
fn same_forward(a: &PortForwardRule, b: &PortForwardRule) -> bool {
    a.bind_host == b.bind_host
        && a.bind_port == b.bind_port
        && a.target_host == b.target_host
        && a.target_port == b.target_port
}

/// 规则占用的远端监听位（wire 形态地址 + 端口）。
///
/// `bind_port == 0` 的规则这里恒为 0，不会与任何已装上的实际端口相同——
/// 「让远端挑端口」本来就不占固定端口，谈不上冲突。
fn bind_key(rule: &PortForwardRule) -> (String, u16) {
    (wire_host(&rule.bind_host).to_string(), rule.bind_port)
}

/// 一条专职隧道连接。
struct TunnelConnection {
    handle: client::Handle<SshClientHandler>,
    /// 与 handler 里那份是同一个表（`Arc` 克隆）：`forwarded-tcpip` 通道到达时
    /// 由 handler 查它认领目标，这里负责登记/撤销。
    forwards: RemoteForwardTable,
    /// 已装上的转发；对齐与复用判断都读它。
    installed: Mutex<Vec<InstalledForward>>,
}

impl TunnelConnection {
    /// 连接是否还活着。russh 在会话循环结束时关闭 handle 的发送端，
    /// 因此这个判断能反映真实断连（网络故障、远端重启）。
    fn is_alive(&self) -> bool {
        !self.handle.is_closed()
    }

    fn installed_snapshot(&self) -> Vec<InstalledForward> {
        self.installed.lock().clone()
    }
}

/// 单个主机的隧道状态。
struct HostTunnel {
    /// 已建立的隧道；`None` = 尚未建立或已断开。
    ///
    /// `parking_lot::Mutex` 而非 tokio 的：这里只做「读一眼、换一个指针」，
    /// 不进 await，同步锁让 [`covers`] 这类同步查询也能用。
    installed: Mutex<Option<Arc<TunnelConnection>>>,
    /// 建立流程串行化：同一主机同时只允许一个标签在建立隧道，否则并发开标签
    /// 会各建一条、两条又互相撞端口。建立过程要 await（连接 + 认证 + 逐条
    /// 请求监听），所以必须是 `tokio::sync::Mutex`。
    establishing: tokio::sync::Mutex<()>,
}

fn registry() -> &'static Mutex<HashMap<String, Arc<HostTunnel>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<HostTunnel>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn host_tunnel(key: &str) -> Arc<HostTunnel> {
    let mut map = registry().lock();
    map.entry(key.to_string())
        .or_insert_with(|| {
            Arc::new(HostTunnel {
                installed: Mutex::new(None),
                establishing: tokio::sync::Mutex::new(()),
            })
        })
        .clone()
}

/// 该主机当前是否有活着的隧道（**不**新建注册表条目）。
///
/// 供调用方判断「这次虽然没配转发，但是不是该去撤销上次装上的」——用户把
/// 规则全删了以后，远端监听不该留在那里。
pub fn has_live_tunnel(config: &SshConnectionConfig) -> bool {
    let host = registry().lock().get(&host_key(config)).cloned();
    host.and_then(|h| alive_connection(&h)).is_some()
}

/// 取活着的隧道；已断开的顺手摘掉（下一次 [`acquire`] 会重建）。
fn alive_connection(host: &HostTunnel) -> Option<Arc<TunnelConnection>> {
    let mut installed = host.installed.lock();
    match installed.as_ref() {
        Some(conn) if conn.is_alive() => Some(conn.clone()),
        Some(_) => {
            *installed = None;
            None
        }
        None => None,
    }
}

/// 一次「确保该主机的转发已就绪」的结果。
#[derive(Debug, Default)]
pub struct TunnelOutcome {
    /// 本次调用新建的转发。
    pub established: Vec<EstablishedForward>,
    /// 本次调用复用的转发（隧道里早就装好了，本标签没有再请求一次监听）。
    pub reused: Vec<EstablishedForward>,
    /// 失败原因（逐条独立：一条失败不影响其它条）。
    pub errors: Vec<String>,
}

impl TunnelOutcome {
    /// 隧道本身建不起来（连接/认证失败）时的结果。
    fn tunnel_error(message: String) -> Self {
        Self {
            errors: vec![message],
            ..Default::default()
        }
    }
}

/// 确保该主机的端口转发已就绪，返回本次「新建 / 复用 / 失败」的明细。
///
/// 已有活着的隧道时只做增量对齐（补建缺的、撤销多的），不重建连接；没有隧道
/// （首次使用，或上一条已断开）时才建立。失败**不阻断登录**：转发是登录的附加
/// 能力，结果由调用方报到 UI。
pub async fn acquire(
    config: &SshConnectionConfig,
    rules: &[PortForwardRule],
    timeout: Duration,
) -> TunnelOutcome {
    let desired: Vec<PortForwardRule> = rules.iter().filter(|r| r.is_complete()).cloned().collect();
    // 没有规则、也没有活着的隧道：无事可做，也不该为此在注册表里留一个空条目。
    if desired.is_empty() && !has_live_tunnel(config) {
        return TunnelOutcome::default();
    }

    let host = host_tunnel(&host_key(config));
    // 串行化建立：并发开标签时后来者在这里等，等到的是同一条隧道。
    let _establishing = host.establishing.lock().await;

    if let Some(conn) = alive_connection(&host) {
        // 走对齐而不是直接返回：`desired` 可能比隧道上装着的多（用户新加了
        // 规则）或少（用户删了规则，此时 `desired` 为空 = 全部撤销）。
        log::info!(
            "russh: 复用端口转发隧道（{}），对齐到 {} 条规则",
            host_key(config),
            desired.len()
        );
        return reconcile(&conn, &desired, timeout).await;
    }

    let forwards = RemoteForwardTable::default();
    let handle = match crate::ssh::russh_terminal::connect_and_authenticate(
        config,
        SshClientHandler {
            host: config.host.clone(),
            port: config.port,
            host_key_policy: crate::ssh::host_key::policy_from_global_config(),
            port_forwards: forwards.clone(),
        },
    )
    .await
    {
        Ok(handle) => handle,
        Err(e) => {
            log::warn!("russh: 端口转发隧道建立失败（{}）: {e:#}", host_key(config));
            return TunnelOutcome::tunnel_error(format!("端口转发隧道建立失败：{e:#}"));
        }
    };
    log::info!("russh: 端口转发隧道已建立（{}）", host_key(config));

    let conn = Arc::new(TunnelConnection {
        handle,
        forwards,
        installed: Mutex::new(Vec::new()),
    });
    let outcome = reconcile(&conn, &desired, timeout).await;
    // 装完才发布：中途让别的标签看到一条「还没有转发」的隧道，它会白等一次。
    *host.installed.lock() = Some(conn);
    outcome
}

/// 一次对齐要做的事：先撤什么、再建什么、最后撤什么、哪些直接复用。
///
/// 拆成纯函数是为了能单测：这里的判定（谁该复用、谁该让位）正是
/// 「每开一个标签撞一次端口」那类 bug 的藏身处，不该只能靠真连一台机器来验。
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// 同 bind 位、但规则已改（例如目标端口变了）：**先**撤掉，把监听位让出来。
    supersede: Vec<InstalledForward>,
    /// 需要新建的规则。
    establish: Vec<PortForwardRule>,
    /// 其余不再需要的（用户删了规则）：新建完成后撤。
    cancel: Vec<InstalledForward>,
    /// 规则未变、隧道上已装好：直接复用，不再请求监听。
    reused: Vec<EstablishedForward>,
}

fn plan(before: &[InstalledForward], desired: &[PortForwardRule]) -> Plan {
    // 没填完的规则（缺目标端口/目标地址）从不建立：`is_complete` 是唯一的
    // 判据，在这里过滤而不是交给调用方，避免漏一处就把半截规则发出去。
    let desired: Vec<&PortForwardRule> = desired.iter().filter(|r| r.is_complete()).collect();

    let mut plan = Plan::default();
    for f in before {
        if desired.iter().any(|d| same_forward(d, &f.rule)) {
            plan.reused.push(f.established.clone());
        }
    }

    for rule in &desired {
        if before.iter().any(|f| same_forward(rule, &f.rule)) {
            continue;
        }
        // 目标变了但绑定位没变：旧监听还占着端口，必须先撤再建，否则新建
        // 会被远端以「端口被占用」拒绝——占住它的正是我们自己。
        let key = bind_key(rule);
        for f in before {
            if !desired.iter().any(|d| same_forward(d, &f.rule))
                && bind_key(&f.rule) == key
                && !plan.supersede.contains(f)
            {
                plan.supersede.push(f.clone());
            }
        }
        plan.establish.push((*rule).clone());
    }

    for f in before {
        if !desired.iter().any(|d| same_forward(d, &f.rule)) && !plan.supersede.contains(f) {
            plan.cancel.push(f.clone());
        }
    }
    plan
}

/// 把隧道上的转发对齐到 `desired`：已有的复用、缺的补建、多的撤销。
async fn reconcile(
    conn: &TunnelConnection,
    desired: &[PortForwardRule],
    timeout: Duration,
) -> TunnelOutcome {
    let plan = plan(&conn.installed_snapshot(), desired);
    let mut outcome = TunnelOutcome::default();

    for f in &plan.supersede {
        cancel_forward(conn, f).await;
    }
    for rule in &plan.establish {
        match port_forward::establish(&conn.handle, rule, &conn.forwards, timeout).await {
            Ok(done) => {
                log::info!("russh: 端口转发已建立 {}", done.summary());
                conn.installed.lock().push(InstalledForward {
                    rule: rule.clone(),
                    established: done.clone(),
                });
                outcome.established.push(done);
            }
            Err(e) => {
                log::warn!("russh: 端口转发失败: {e:#}");
                outcome.errors.push(format!("{e:#}"));
            }
        }
    }
    for f in &plan.cancel {
        cancel_forward(conn, f).await;
    }

    outcome.reused = plan.reused;
    outcome
}

/// 撤销一条转发：先让远端停掉监听，再摘掉认领表条目。
async fn cancel_forward(conn: &TunnelConnection, f: &InstalledForward) {
    let wire = wire_host(&f.rule.bind_host);
    let port = f.established.bind_port;
    match conn
        .handle
        .cancel_tcpip_forward(wire.to_string(), u32::from(port))
        .await
    {
        Ok(()) => log::info!("russh: 端口转发已撤销 {}", f.established.summary()),
        Err(e) => log::warn!("russh: 撤销端口转发 {} 失败: {e}", f.established.summary()),
    }
    conn.forwards.unregister(wire, port);
    conn.installed.lock().retain(|x| x.rule != f.rule);
}

/// 该主机活着的隧道上已装好的规则（没有隧道时为**空**）。
///
/// 同步查询（不 await）：系统 ssh 路径要在构造 ssh 参数前判断哪些规则不必再
/// 请求一次监听，那时还在同步上下文里。
pub fn installed_rules(config: &SshConnectionConfig) -> Vec<PortForwardRule> {
    let host = registry().lock().get(&host_key(config)).cloned();
    let Some(host) = host else {
        return Vec::new();
    };
    let Some(conn) = alive_connection(&host) else {
        return Vec::new();
    };
    conn.installed_snapshot()
        .into_iter()
        .map(|f| f.rule)
        .collect()
}

/// 系统 ssh 路径还需要自己用 `-R` 请求的规则：共享隧道已覆盖的摘掉。
///
/// 逐条判断而不是「要么全给要么全不给」：部分覆盖时（隧道是另一个标签建的，
/// 规则集不完全相同）只省掉已覆盖的那些，其余照旧请求——全给会让已覆盖的
/// 端口报一次「被占用」（占住的是我们自己），全不给则会漏掉真正需要的转发。
pub fn rules_for_openssh(
    config: &SshConnectionConfig,
    rules: &[PortForwardRule],
) -> Vec<PortForwardRule> {
    let installed = installed_rules(config);
    if installed.is_empty() {
        return rules.to_vec();
    }
    rules
        .iter()
        .filter(|r| !installed.iter().any(|i| same_forward(r, i)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::config::{AuthMethod, SshConnectionConfig};

    fn rule(bind_port: u16, target_port: u16) -> PortForwardRule {
        PortForwardRule {
            bind_host: "127.0.0.1".to_string(),
            bind_port,
            target_host: "127.0.0.1".to_string(),
            target_port,
            description: String::new(),
        }
    }

    fn installed(rule: PortForwardRule, actual_bind_port: u16) -> InstalledForward {
        InstalledForward {
            established: EstablishedForward {
                bind_host: rule.bind_host.clone(),
                bind_port: actual_bind_port,
                target_host: rule.target_host.clone(),
                target_port: rule.target_port,
            },
            rule,
        }
    }

    fn config(host: &str) -> SshConnectionConfig {
        let mut c = SshConnectionConfig::new(host, "root", AuthMethod::Auto);
        c.port = 22;
        c
    }

    #[test]
    fn host_key_is_per_user_host_port() {
        let mut a = config("10.0.0.1");
        let b = config("10.0.0.2");
        assert_ne!(host_key(&a), host_key(&b));

        // 同一主机不同端口是不同隧道（两个 sshd 各自的监听空间）
        let mut other_port = config("10.0.0.1");
        other_port.port = 2222;
        assert_ne!(host_key(&a), host_key(&other_port));

        // 用户不同也是不同隧道（远端监听属于那个登录会话）
        let mut other_user = config("10.0.0.1");
        other_user.username = "deploy".to_string();
        assert_ne!(host_key(&a), host_key(&other_user));

        a.nickname = Some("别名".to_string());
        assert_eq!(
            host_key(&a),
            host_key(&config("10.0.0.1")),
            "昵称只是显示用，不该分裂隧道"
        );
        assert_eq!(host_key(&b), "root@10.0.0.2:22");
    }

    #[test]
    fn plan_reuses_unchanged_rules_without_rebinding() {
        // 这是「每开一个标签撞一次端口」的直接回归测试：同一条规则第二次来
        // 必须复用，而不是再 establish 一遍。
        let r = rule(3801, 3801);
        let before = vec![installed(r.clone(), 3801)];

        let p = plan(&before, std::slice::from_ref(&r));
        assert!(p.establish.is_empty(), "已装好的规则不该重复请求监听");
        assert!(p.supersede.is_empty());
        assert!(p.cancel.is_empty());
        assert_eq!(p.reused.len(), 1);
        assert_eq!(p.reused[0].bind_port, 3801);
    }

    #[test]
    fn plan_establishes_only_missing_rules() {
        let existing = rule(3801, 3801);
        let added = rule(3800, 3800);
        let before = vec![installed(existing.clone(), 3801)];

        let p = plan(&before, &[existing.clone(), added.clone()]);
        assert_eq!(p.establish, vec![added]);
        assert_eq!(p.reused.len(), 1, "已有那条仍是复用");
        assert!(p.cancel.is_empty());
        assert!(p.supersede.is_empty());
    }

    #[test]
    fn plan_cancels_rules_removed_from_config() {
        let kept = rule(3801, 3801);
        let dropped = rule(3800, 3800);
        let before = vec![installed(kept.clone(), 3801), installed(dropped, 3800)];

        let p = plan(&before, std::slice::from_ref(&kept));
        assert!(p.establish.is_empty());
        assert_eq!(p.cancel.len(), 1, "配置里删掉的规则要撤销监听");
        assert_eq!(p.cancel[0].rule.bind_port, 3800);
    }

    #[test]
    fn plan_supersedes_same_bind_port_before_rebinding() {
        // 目标改了、绑定端口没变：旧监听还占着端口，必须先撤再建，
        // 否则新建会被远端以「端口被占用」拒绝（占住的是我们自己）。
        let old = rule(3801, 3801);
        let new = rule(3801, 9090);
        let before = vec![installed(old.clone(), 3801)];

        let p = plan(&before, std::slice::from_ref(&new));
        assert_eq!(p.supersede.len(), 1);
        assert_eq!(p.supersede[0].rule, old);
        assert_eq!(p.establish, vec![new]);
        assert!(
            p.cancel.is_empty(),
            "被顶替的那条已进 supersede，不该再撤一次"
        );
        assert!(p.reused.is_empty(), "规则变了就不是复用");
    }

    #[test]
    fn plan_does_not_supersede_across_different_bind_ports() {
        // 换端口不该牵连旧监听：那是两条互不相干的规则，旧的那条按「移除」撤。
        let old = rule(3801, 3801);
        let new = rule(3802, 3802);
        let before = vec![installed(old.clone(), 3801)];

        let p = plan(&before, std::slice::from_ref(&new));
        assert!(p.supersede.is_empty());
        assert_eq!(p.establish, vec![new]);
        assert_eq!(p.cancel.len(), 1);
        assert_eq!(p.cancel[0].rule, old);
    }

    #[test]
    fn plan_matches_port_zero_rules_by_rule_not_by_assigned_port() {
        // 「让远端挑端口」的规则实际绑定端口每次都不同；按端口比对会把它
        // 认成新规则、每开一个标签多绑一个监听。按规则比对才是对的。
        let r = rule(0, 3000);
        let before = vec![installed(r.clone(), 40001)];

        let p = plan(&before, std::slice::from_ref(&r));
        assert!(p.establish.is_empty(), "端口 0 的规则也不该重复绑定");
        assert_eq!(p.reused.len(), 1);
        assert_eq!(p.reused[0].bind_port, 40001, "复用要回报服务器分配的端口");
    }

    #[test]
    fn plan_ignores_incomplete_rules() {
        let mut incomplete = rule(3801, 0);
        incomplete.target_port = 0;
        let p = plan(&[], &[incomplete]);
        assert!(p.establish.is_empty(), "半截规则不该被发出去");
        assert!(p.cancel.is_empty());
    }

    #[test]
    fn plan_reuses_rule_when_only_description_changed() {
        // 备注是纯展示字段：改它不该重建监听（重建期间端口会短暂空出来，
        // 可能被别的进程抢走）。
        let mut old = rule(3801, 3801);
        old.description = "改前的备注".to_string();
        let mut new = rule(3801, 3801);
        new.description = "改后的备注".to_string();

        let p = plan(&[installed(old, 3801)], std::slice::from_ref(&new));
        assert!(p.establish.is_empty(), "只改备注不该重新绑定");
        assert!(p.cancel.is_empty());
        assert!(p.supersede.is_empty());
        assert_eq!(p.reused.len(), 1);
    }

    #[test]
    fn plan_treats_target_change_as_a_rebind() {
        // 目标变了是真正的变更：必须重建（不是复用）。
        let old = rule(3801, 3801);
        let new = rule(3801, 9090);
        let p = plan(&[installed(old, 3801)], std::slice::from_ref(&new));
        assert_eq!(p.establish, vec![new]);
        assert_eq!(p.supersede.len(), 1);
        assert!(p.reused.is_empty());
    }

    #[test]
    fn bind_key_uses_wire_form_of_ipv6() {
        let mut v6 = rule(3801, 3801);
        v6.bind_host = "[::1]".to_string();
        assert_eq!(bind_key(&v6), ("::1".to_string(), 3801));
    }

    #[test]
    fn empty_rules_do_not_create_registry_entries() {
        // 没配转发的主机不该因为「连过一次」就在注册表里留条目：那会让
        // 每台连过的机器都占一份状态，且 `has_live_tunnel` 会误报。
        let c = config("198.51.100.9");
        assert!(!has_live_tunnel(&c));
        assert!(installed_rules(&c).is_empty());
        assert!(!has_live_tunnel(&c), "查询本身不该留下痕迹");
    }

    #[test]
    fn rules_for_openssh_is_passthrough_when_no_tunnel_is_installed() {
        // 没有共享隧道（纯 PTY 用户）：规则原样交给 `-R`，行为与改动前一致。
        let c = config("198.51.100.10");
        let rules = vec![rule(3801, 3801), rule(3800, 3800)];
        assert_eq!(rules_for_openssh(&c, &rules), rules);
        assert!(installed_rules(&c).is_empty());

        // 单条规则同理（`from_ref` 避免为一条规则建 Vec）。
        let single = rule(3801, 3801);
        assert_eq!(
            rules_for_openssh(&c, std::slice::from_ref(&single)),
            vec![single]
        );
    }

    #[test]
    fn has_live_tunnel_is_false_before_any_tunnel_exists() {
        // 未建立过隧道的主机：既没有活隧道，也没有已装规则。
        let c = config("198.51.100.8");
        assert!(!has_live_tunnel(&c));
        assert!(installed_rules(&c).is_empty());
    }
}
