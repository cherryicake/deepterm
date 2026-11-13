//! SSH 会话管理 - Zellij 方式（使用 PTY）
//!
//! 通过创建本地 PTY，在其中运行 ssh 命令，实现真正的终端体验。

use std::sync::Arc;

use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty::{self, Options};

use crate::ssh::config::SshConnectionConfig;
use crate::ssh::keychain::{ResolvedAuthMethod, resolve_auth_method};
use crate::ssh::remote_shell_tracking_command;
use crate::terminal::TerminalBounds;
use crate::terminal::terminal::ZedListener;

/// 系统 ssh 路径的代理落点。
///
/// 这条路径只有 `ProxyCommand` 一种表达方式，而它在部分情况下不可用（非 macOS
/// 的 nc、IPv6 代理地址）。不可用时**只能**直连（russh 处理不了的密钥类型才会
/// 走到这里，没有可回退的通道），但必须让用户知道——所以把结论返回给调用方去
/// 报到 UI，而不是只写日志。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyCommandOutcome {
    /// 本次没有选择代理，或目标命中例外/本机地址，直连是预期行为。
    NotRequested,
    /// 已把 `ProxyCommand` 装进 ssh 参数。
    Applied { proxy: String },
    /// 用户选了代理，但这条路径无法表达，已退回直连。
    Unavailable { reason: String },
}

/// 按系统代理设置给 `ssh_args` 追加 `ProxyCommand`，返回落点供调用方报到 UI。
fn apply_proxy_command(
    config: &SshConnectionConfig,
    ssh_args: &mut Vec<String>,
) -> ProxyCommandOutcome {
    if !config.use_system_proxy {
        return ProxyCommandOutcome::NotRequested;
    }
    let proxy = match crate::ssh::system_proxy::route(config) {
        crate::ssh::system_proxy::ProxyRoute::Via(proxy) => proxy,
        crate::ssh::system_proxy::ProxyRoute::Direct => {
            log::info!("SSH: 本次目标直连（本机地址或命中代理例外）");
            return ProxyCommandOutcome::NotRequested;
        }
        crate::ssh::system_proxy::ProxyRoute::Unavailable => {
            return ProxyCommandOutcome::Unavailable {
                reason: "未检测到系统代理（请在系统设置或 ALL_PROXY 中配置）".to_string(),
            };
        }
    };
    match proxy.openssh_proxy_command() {
        Some(command) => {
            log::info!(
                "SSH: 经系统代理 {} 连接（ProxyCommand: {command}）",
                proxy.display()
            );
            ssh_args.push("-o".to_string());
            ssh_args.push(format!("ProxyCommand={command}"));
            ProxyCommandOutcome::Applied {
                proxy: proxy.display(),
            }
        }
        None => ProxyCommandOutcome::Unavailable {
            reason: format!(
                "系统代理 {} 无法用 OpenSSH 的 ProxyCommand 表达（需要支持 -X 的 nc，且不支持 IPv6 代理地址）",
                proxy.display()
            ),
        },
    }
}

/// SSH 终端 - 使用 Zellij 方式（PTY）
#[derive(Clone)]
pub struct SshPtyTerminal {
    /// PTY 事件循环发送器
    sender: EventLoopSender,
}

impl SshPtyTerminal {
    /// 创建新的 SSH PTY 终端
    ///
    /// `scale` 为窗口所在显示的缩放：`bounds` 是逻辑点，而 PTY 的
    /// `ws_xpixel` 需要物理像素（ghostty 也是按 DPI 换算后下发）。
    ///
    /// 返回终端句柄与代理落点：后者由调用方报到 UI（见 [`ProxyCommandOutcome`]）。
    /// 构造一条可以直接交给 PTY 执行的 SSH 启动命令。
    ///
    /// 抽出来是为了让**守护进程**也能起 SSH 终端：持久化把 PTY 搬到了
    /// `deepcore`，而 `ssh` 只是那条 PTY 里跑的程序。两边必须共用同一套
    /// 参数构造（StrictHostKeyChecking 策略、超时、保活、代理、端口转发），
    /// 否则同一主机在"进程内"和"持久化"下表现会不一样。
    ///
    /// 返回的 `env` 里可能带 `SSHPASS`（密码认证走 `sshpass -e`）。调用方
    /// 要清楚这是**凭据**：只交给子进程环境，不落盘、不写日志、不回报给 UI。
    pub fn build_ssh_command(
        config: &SshConnectionConfig,
        initial_cwd: Option<&str>,
    ) -> anyhow::Result<(
        String,
        Vec<String>,
        std::collections::HashMap<String, String>,
        ProxyCommandOutcome,
    )> {
        let resolved_auth = resolve_auth_method(&config.auth_method)?;

        // StrictHostKeyChecking 与 russh 路径共用同一策略，避免同一开关在
        // 两条传输路径上行为不一致。
        let host_key_policy = crate::ssh::host_key::policy_from_global_config();
        let mut ssh_program = "ssh";
        let mut ssh_args = vec![
            "-o".to_string(),
            format!("StrictHostKeyChecking={}", host_key_policy.openssh_str()),
            "-o".to_string(),
            "ServerAliveInterval=60".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
            // 避免不可达主机等 OS 级 SYN 重试（约 75-130s）。
            "-o".to_string(),
            format!("ConnectTimeout={}", config.timeout.max(1)),
            "-p".to_string(),
            config.port.to_string(),
            "-l".to_string(),
            config.username.clone(),
            "-t".to_string(),
            "-t".to_string(), // 强制分配 TTY
        ];

        let proxy_outcome = apply_proxy_command(config, &mut ssh_args);

        let mut use_sshpass_env = false;
        match &resolved_auth {
            ResolvedAuthMethod::PrivateKey {
                path,
                certificate_path,
                ..
            } => {
                ssh_args.push("-i".to_string());
                ssh_args.push(path.to_string_lossy().to_string());
                if let Some(certificate_path) = certificate_path {
                    ssh_args.push("-o".to_string());
                    ssh_args.push(format!(
                        "CertificateFile={}",
                        certificate_path.to_string_lossy()
                    ));
                }
            }
            ResolvedAuthMethod::Password { .. } => {
                // 密码经 `SSHPASS` 环境变量传递，不出现在 argv（`ps` 可见）。
                let mut sshpass_args = vec!["-e".to_string(), "ssh".to_string()];
                sshpass_args.append(&mut ssh_args);
                ssh_args = sshpass_args;
                ssh_program = "sshpass";
                use_sshpass_env = true;
            }
            ResolvedAuthMethod::Agent | ResolvedAuthMethod::Auto => {}
        }

        // 系统 ssh 路径没有 russh handle，反向隧道用 `-R` 表达。
        let pending_forwards = crate::ssh::tunnel::rules_for_openssh(config, &config.port_forwards);
        let forward_args = crate::ssh::port_forward::openssh_args(&pending_forwards);
        if !forward_args.is_empty() {
            ssh_args.extend(forward_args);
        }

        ssh_args.push(format!("{}@{}", config.username, config.host));
        ssh_args.push(remote_shell_tracking_command(initial_cwd));

        let mut env = crate::terminal::mappings::default_terminal_env();
        // SSH 远程主机可能没有 deepterm terminfo，回退 xterm-256color
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        if use_sshpass_env && let ResolvedAuthMethod::Password { password } = &resolved_auth {
            env.insert("SSHPASS".to_string(), password.to_string());
        }

        // 记一条脱敏后的命令行：排查 SSH 连不上时，`ssh` 到底带了哪些参数
        // 是最关键的线索（StrictHostKeyChecking、超时、证书、转发规则）。
        // 口令经环境变量传递、不在 argv 里，这里再脱敏一次做兜底。
        log::info!(
            "SSH 命令: {} {:?}",
            ssh_program,
            redact_sensitive_args(&ssh_args)
        );

        Ok((ssh_program.to_string(), ssh_args, env, proxy_outcome))
    }

    pub fn new(
        config: &SshConnectionConfig,
        bounds: TerminalBounds,
        scale: f32,
        term: Arc<FairMutex<Term<ZedListener>>>,
        listener: ZedListener,
        initial_cwd: Option<String>,
    ) -> anyhow::Result<(Self, ProxyCommandOutcome)> {
        log::info!("=== 创建 SSH PTY 终端 ===");
        log::info!("目标: {}@{}:{}", config.username, config.host, config.port);

        // 命令构造与守护进程路径共用同一份实现。
        let (program, args, env, proxy_outcome) =
            Self::build_ssh_command(config, initial_cwd.as_deref())?;
        let shell = alacritty_terminal::tty::Shell::new(program, args);

        let pty_options = Options {
            shell: Some(shell),
            working_directory: None,
            drain_on_exit: true,
            env,
            #[cfg(target_os = "windows")]
            escape_args: true,
        };

        // 转换窗口大小
        let window_size = bounds.window_size(scale);

        // 创建 PTY
        log::debug!("创建 PTY...");
        let pty = tty::new(&pty_options, window_size, 0)
            .map_err(|e| anyhow::anyhow!("创建 PTY 失败: {}", e))?;

        let event_loop = EventLoop::new(
            term, listener, pty, true,  // hold
            false, // ref_test
        )
        .map_err(|e| anyhow::anyhow!("创建事件循环失败: {}", e))?;

        // 获取发送器
        let sender = event_loop.channel();

        // 启动事件循环
        log::debug!("启动事件循环...");
        let _io_thread = event_loop.spawn();

        log::info!("SSH PTY 终端创建成功");

        Ok((Self { sender }, proxy_outcome))
    }

    /// 写入数据到 SSH（键盘输入）
    pub fn write(&self, data: &[u8]) {
        log::debug!("写入 SSH: {} 字节", data.len());
        let msg = Msg::Input(std::borrow::Cow::Owned(data.to_vec()));
        let _ = self.sender.send(msg);
    }

    /// 调整窗口大小。`scale` 为窗口所在显示的缩放（见 [`Self::new`]）。
    pub fn resize(&self, bounds: TerminalBounds, scale: f32) {
        let window_size = bounds.window_size(scale);
        log::debug!(
            "调整 SSH 窗口大小: {}x{}",
            window_size.num_cols,
            window_size.num_lines
        );
        let _ = self.sender.send(Msg::Resize(window_size));
    }

    /// 关闭 SSH 连接
    pub fn close(&self) {
        log::info!("关闭 SSH 连接");
        let _ = self.sender.send(Msg::Shutdown);
    }
}

/// 日志脱敏：把 argv 中紧随敏感选项（-p/-P/--password）之后的参数替换为掩码。
/// 兜底防御——当前密码经 SSHPASS 环境变量传递，argv 中不应再出现密码，
/// 但若未来有人把密码重新放回 argv，日志也不会泄漏。
pub(crate) fn redact_sensitive_args(args: &[String]) -> Vec<String> {
    const SENSITIVE_FLAGS: &[&str] = &["-p", "-P", "--password", "--pass", "-p="];
    let mut redacted = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for arg in args {
        if mask_next {
            redacted.push("********".to_string());
            mask_next = false;
            continue;
        }
        if SENSITIVE_FLAGS.contains(&arg.as_str()) {
            mask_next = true;
        } else if arg.starts_with("--password=") {
            redacted.push("--password=********".to_string());
            continue;
        } else if arg.starts_with("-p=") {
            redacted.push("-p=********".to_string());
            continue;
        }
        redacted.push(arg.clone());
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::config::AuthMethod;

    /// 让测试读起来短一些，同时把"这是被测的构造函数"点出来。
    struct SshCommandBuilder;

    impl SshCommandBuilder {
        fn build(
            config: &SshConnectionConfig,
            cwd: Option<&str>,
        ) -> anyhow::Result<(
            String,
            Vec<String>,
            std::collections::HashMap<String, String>,
            ProxyCommandOutcome,
        )> {
            SshPtyTerminal::build_ssh_command(config, cwd)
        }
    }

    #[test]
    fn redact_masks_value_after_sensitive_flag() {
        let args = vec![
            "-e".to_string(),
            "ssh".to_string(),
            "-p".to_string(),
            "secret123".to_string(),
            "-l".to_string(),
            "root".to_string(),
        ];
        let out = redact_sensitive_args(&args);
        assert_eq!(out[3], "********");
        assert!(!out.iter().any(|a| a.contains("secret123")));
        // 非敏感参数保持不变
        assert_eq!(out[5], "root");
    }

    #[test]
    fn redact_masks_inline_password_flag() {
        let args = vec!["--password=hunter2".to_string(), "ssh".to_string()];
        let out = redact_sensitive_args(&args);
        assert_eq!(out[0], "--password=********");
        assert_eq!(out[1], "ssh");
    }

    /// 未选代理时不动 ssh 参数，也不报告任何落点。
    #[test]
    fn proxy_command_is_absent_when_not_requested() {
        let config = SshConnectionConfig::new("example.com", "root", AuthMethod::Agent);
        let mut args: Vec<String> = Vec::new();
        assert_eq!(
            apply_proxy_command(&config, &mut args),
            ProxyCommandOutcome::NotRequested
        );
        assert!(args.is_empty(), "没选代理不该往 ssh 参数里塞东西");
    }

    /// 本机地址直连：选了代理也不生成 ProxyCommand，且不算「不可用」——
    /// 那是预期行为，不该弹提示。
    #[test]
    fn loopback_target_reports_not_requested_rather_than_unavailable() {
        let mut config = SshConnectionConfig::new("127.0.0.1", "root", AuthMethod::Agent);
        config.use_system_proxy = true;
        let mut args: Vec<String> = Vec::new();
        assert_eq!(
            apply_proxy_command(&config, &mut args),
            ProxyCommandOutcome::NotRequested,
            "本机地址直连是预期行为，不该报「代理不可用」"
        );
        assert!(args.is_empty());
    }

    /// 选了代理但本机检测不到代理时，必须报告「不可用」（而不是静默直连）——
    /// 调用方据此弹提示，这是这条路径唯一能告知用户的方式。
    #[test]
    fn missing_proxy_is_reported_as_unavailable() {
        let mut config =
            SshConnectionConfig::new("overseas.example.com", "root", AuthMethod::Agent);
        config.use_system_proxy = true;
        let mut args: Vec<String> = Vec::new();
        // 本机可能真配了代理：两种结果都可接受，但 Direct 式的静默不在此列。
        match apply_proxy_command(&config, &mut args) {
            ProxyCommandOutcome::Unavailable { reason } => {
                assert!(!reason.is_empty(), "不可用必须带上可读的原因");
                assert!(args.is_empty(), "退回直连时不该留下半个 ProxyCommand");
            }
            ProxyCommandOutcome::Applied { .. } => {
                assert!(
                    args.iter().any(|a| a.starts_with("ProxyCommand=")),
                    "报告 Applied 就必须真的写进 ssh 参数"
                );
            }
            ProxyCommandOutcome::NotRequested => {
                panic!("选了代理却报 NotRequested 等于静默直连");
            }
        }
    }

    /// `build_ssh_command` 必须产出能直接交给 PTY 的 ssh 调用。
    ///
    /// 这条函数是两个路径共用的（进程内 SSH 与守护进程持久化），所以它
    /// 本身值得单独验：一旦它漏了 `-t -t`，远端不会分配 TTY，`htop` 之类
    /// 直接把终端弄成全屏乱码；漏了端口就是连到 22 而不是用户配的端口。
    #[test]
    fn the_ssh_command_allocates_a_tty_and_honours_the_port() {
        let config = SshConnectionConfig::new("example.com", "root", AuthMethod::Agent);
        let mut config = config;
        config.port = 2222;

        let (program, args, _env, _proxy) =
            SshCommandBuilder::build(&config, None).expect("命令应能构造");

        assert_eq!(program, "ssh", "Agent 认证直接用 ssh");
        // `-t -t` 是"强制分配 TTY"，两次才强制（OpenSSH 的约定）。
        let t_count = args.iter().filter(|a| *a == "-t").count();
        assert_eq!(t_count, 2, "必须带两遍 -t 才会强制分配 TTY: {args:?}");
        assert!(
            args.windows(2).any(|w| w[0] == "-p" && w[1] == "2222"),
            "端口要如实带上: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "root@example.com"),
            "目标主机要在参数里: {args:?}"
        );
    }

    /// 密码认证必须走 `sshpass -e` + 环境变量，口令**不能**出现在 argv。
    ///
    /// `ps` 对这些参数全局可见。这条钉住的是"口令不进 argv"这个安全属性，
    /// 不是实现细节——一旦有人图省事改成 `sshpass -p <密码>`，同机任何
    /// 用户都能读到用户的口令。
    #[test]
    fn passwords_go_through_the_environment_never_argv() {
        let secret = "hunter2-must-not-leak";
        let config = SshConnectionConfig::new(
            "example.com",
            "root",
            AuthMethod::Password {
                password: secret.to_string(),
            },
        );

        let (program, args, env, _proxy) =
            SshCommandBuilder::build(&config, None).expect("命令应能构造");

        assert_eq!(program, "sshpass", "密码认证要走 sshpass");
        assert!(
            !args.iter().any(|a| a.contains(secret)),
            "口令绝不能出现在 argv 里（ps 可见）: {args:?}"
        );
        assert!(
            args.contains(&"-e".to_string()),
            "要用 -e 让 sshpass 从环境变量取口令: {args:?}"
        );
        assert_eq!(
            env.get("SSHPASS").map(String::as_str),
            Some(secret),
            "口令应经环境变量传给子进程"
        );
    }

    /// 远程 shell integration 的引导命令要带上初始 cwd。
    ///
    /// 恢复会话时用户期望回到断开前那个目录；丢了它，重开就落在远端家目录。
    #[test]
    fn the_tracking_bootstrap_carries_the_initial_directory() {
        let config = SshConnectionConfig::new("example.com", "root", AuthMethod::Agent);

        let (_p, args, _e, _proxy) =
            SshCommandBuilder::build(&config, Some("/srv/app")).expect("命令应能构造");

        // 最后一个是发往远端执行的引导命令。
        let bootstrap = args.last().expect("应有引导命令");
        assert!(
            bootstrap.contains("/srv/app"),
            "引导命令应带初始目录: {bootstrap}"
        );
    }
}
