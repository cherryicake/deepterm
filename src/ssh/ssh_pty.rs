//! SSH 会话管理 - Zellij 方式（使用 PTY）
//!
//! 通过创建本地 PTY，在其中运行 ssh 命令，实现真正的终端体验。

use std::sync::Arc;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty::{self, Options};

use crate::ssh::config::SshConnectionConfig;
use crate::ssh::keychain::{ResolvedAuthMethod, resolve_auth_method};
use crate::ssh::remote_shell_tracking_command;
use crate::terminal::TerminalBounds;
use crate::terminal::terminal::ZedListener;

/// SSH 终端 - 使用 Zellij 方式（PTY）
#[derive(Clone)]
pub struct SshPtyTerminal {
    /// PTY 事件循环发送器
    sender: EventLoopSender,
}

impl SshPtyTerminal {
    /// 创建新的 SSH PTY 终端
    pub fn new(
        config: &SshConnectionConfig,
        bounds: TerminalBounds,
        term: Arc<FairMutex<Term<ZedListener>>>,
        listener: ZedListener,
        initial_cwd: Option<String>,
    ) -> anyhow::Result<Self> {
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
            "-p".to_string(),
            config.port.to_string(),
            "-l".to_string(),
            config.username.clone(),
            "-t".to_string(),
            "-t".to_string(), // 强制分配 TTY
        ];

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
                env.insert("SSHPASS".to_string(), password.clone());
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
        };

        // 转换窗口大小
        let window_size = bounds_to_window_size(bounds);

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

        Ok(Self { sender })
    }

    /// 写入数据到 SSH（键盘输入）
    pub fn write(&self, data: &[u8]) {
        log::debug!("写入 SSH: {} 字节", data.len());
        let msg = Msg::Input(std::borrow::Cow::Owned(data.to_vec()));
        let _ = self.sender.send(msg);
    }

    /// 调整窗口大小
    pub fn resize(&self, bounds: TerminalBounds) {
        let window_size = bounds_to_window_size(bounds);
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

/// 将 TerminalBounds 转换为 WindowSize
fn bounds_to_window_size(bounds: TerminalBounds) -> WindowSize {
    WindowSize {
        num_cols: bounds.num_columns() as u16,
        num_lines: bounds.num_lines() as u16,
        cell_width: bounds.cell_width.as_f32() as u16,
        cell_height: bounds.line_height.as_f32() as u16,
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
}
