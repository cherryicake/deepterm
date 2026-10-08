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

        let resolved_auth = resolve_auth_method(&config.auth_method)?;

        // 构建 SSH 命令。StrictHostKeyChecking 与 russh 路径共用同一策略，
        // 避免同一开关在两条传输路径上行为不一致。
        let host_key_policy = crate::ssh::host_key::policy_from_global_config();
        let mut ssh_program = "ssh";
        let mut ssh_args = vec![
            "-o".to_string(),
            format!("StrictHostKeyChecking={}", host_key_policy.openssh_str()),
            "-o".to_string(),
            "ServerAliveInterval=60".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
            // config.timeout 在系统 ssh 路径同样生效（与 russh 路径一致），
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

        // 系统代理：OpenSSH 不读系统代理设置，只能经 `ProxyCommand` 表达。
        // 路由判定与 russh 路径共用（`system_proxy::route`），两条传输对
        // 「这个目标走不走代理」的结论一致。
        //
        // 结果由调用方报到 UI（见 [`ProxyCommandOutcome`]）：用户点了「使用系统
        // 代理连接」却没走成代理时，必须说出来——只写日志等于静默直连。
        let proxy_outcome = apply_proxy_command(config, &mut ssh_args);

        // 添加认证方式
        // 密码认证使用 sshpass -e：密码通过 SSHPASS 环境变量传递，
        // 避免明文密码出现在命令行 argv 中（会被 ps 等工具全局可见）。
        let mut use_sshpass_env = false;
        match &resolved_auth {
            ResolvedAuthMethod::PrivateKey {
                path,
                certificate_path,
                ..
            } => {
                log::info!("使用密钥: {:?}", path);
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
                log::info!("使用密码认证 (通过 sshpass -e)");
                // 命令格式: SSHPASS=<password> sshpass -e ssh [options] user@host
                let mut sshpass_args = vec!["-e".to_string(), "ssh".to_string()];
                sshpass_args.append(&mut ssh_args);
                ssh_args = sshpass_args;
                ssh_program = "sshpass";
                use_sshpass_env = true;
            }
            ResolvedAuthMethod::Agent | ResolvedAuthMethod::Auto => {}
        }

        // 端口转发：系统 ssh 路径没有 russh handle，反向隧道用 `-R` 表达，
        // 隧道由 ssh 进程自己维护；「请求了什么」由 terminal.rs 直接按 config 报给 UI。
        //
        // 但若该主机已有活着的**共享隧道**（另一个 russh 标签建的，见
        // `crate::ssh::tunnel`），这些规则已经装在上面了：再加 `-R` 只会让 ssh
        // 拿到「端口被占用」的警告——占住端口的正是本进程自己。此时省掉。
        let pending_forwards = crate::ssh::tunnel::rules_for_openssh(config, &config.port_forwards);
        let forward_args = crate::ssh::port_forward::openssh_args(&pending_forwards);
        if !forward_args.is_empty() {
            log::info!("SSH: 端口转发参数 {:?}", forward_args);
            ssh_args.extend(forward_args);
        } else if !config.port_forwards.is_empty() {
            log::info!("SSH: 端口转发已由共享隧道覆盖，本次连接不再重复请求监听");
        }

        // 目标主机
        ssh_args.push(format!("{}@{}", config.username, config.host));
        // 和 russh 路径保持一致：远程 shell integration 在远程 shell 启动前
        // 通过 ssh command bootstrap 完成，不再登录后向远程 PTY 输入脚本。
        ssh_args.push(remote_shell_tracking_command(initial_cwd.as_deref()));

        // 日志脱敏：密码等敏感参数不得写入日志（SSHPASS 经环境变量传递，
        // argv 中不再包含密码，此处仍做兜底过滤以防未来引入含密码的参数）。
        log::info!(
            "SSH 命令: {} {:?}",
            ssh_program,
            redact_sensitive_args(&ssh_args)
        );

        // 创建 PTY 选项 - 使用共享的环境变量设置
        let mut env = crate::terminal::mappings::default_terminal_env();
        // SSH 远程主机可能没有 deepterm terminfo，回退 xterm-256color
        env.insert("TERM".to_string(), "xterm-256color".to_string());

        // sshpass -e 从 SSHPASS 环境变量读取密码（仅子进程环境可见，
        // 不会出现在 ps 输出中）
        if use_sshpass_env {
            if let ResolvedAuthMethod::Password { password } = &resolved_auth {
                env.insert("SSHPASS".to_string(), password.to_string());
            }
        }

        let pty_options = Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                ssh_program.to_string(),
                ssh_args,
            )),
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
fn redact_sensitive_args(args: &[String]) -> Vec<String> {
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
}
