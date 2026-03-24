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
use crate::terminal::TerminalBounds;
use crate::terminal::terminal::ZedListener;

/// SSH 终端 - 使用 Zellij 方式（PTY）
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
    ) -> anyhow::Result<Self> {
        log::info!("=== 创建 SSH PTY 终端 ===");
        log::info!("目标: {}@{}:{}", config.username, config.host, config.port);

        // 构建 SSH 命令
        let mut ssh_program = "ssh";
        let mut ssh_args = vec![
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
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
        match &config.auth_method {
            crate::ssh::config::AuthMethod::PrivateKey { path, .. } => {
                log::info!("使用密钥: {:?}", path);
                ssh_args.push("-i".to_string());
                ssh_args.push(path.to_string_lossy().to_string());
            }
            crate::ssh::config::AuthMethod::Password { password } => {
                log::info!("使用密码认证 (通过 sshpass)");
                // 使用 sshpass 自动输入密码
                // 命令格式: sshpass -p 'password' ssh [options] user@host
                let mut sshpass_args = vec!["-p".to_string(), password.clone(), "ssh".to_string()];
                sshpass_args.append(&mut ssh_args);
                ssh_args = sshpass_args;
                ssh_program = "sshpass";
            }
            _ => {}
        }

        // 目标主机
        ssh_args.push(format!("{}@{}", config.username, config.host));

        log::info!("SSH 命令: {} {:?}", ssh_program, ssh_args);

        // 创建 PTY 选项 - 使用共享的环境变量设置
        let env = crate::terminal::mappings::default_terminal_env();

        let pty_options = Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                ssh_program.to_string(),
                ssh_args,
            )),
            working_directory: None,
            hold: true,
            env,
        };

        // 转换窗口大小
        let window_size = bounds_to_window_size(bounds);

        // 创建 PTY
        log::debug!("创建 PTY...");
        let pty = tty::new(&pty_options, window_size, 0)
            .map_err(|e| anyhow::anyhow!("创建 PTY 失败: {}", e))?;

        // 创建事件循环
        log::debug!("创建事件循环...");
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
