//! SSH 终端 - 使用 russh 库实现原生 SSH 连接
//!
//! 优先使用 russh 进行真正的 SSH 协议连接，避免依赖系统 ssh 命令和 sshpass。
//! 如果连接或认证失败，调用方应回退到 SshPtyTerminal（ssh_pty.rs）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use alacritty_terminal::event::{Event as AlacTermEvent, EventListener};
use russh::client::AuthResult;
use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelMsg, Disconnect, client};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::ssh::auth::try_agent_auth;
use crate::ssh::config::SshConnectionConfig;
use crate::ssh::keychain::{
    ResolvedAuthMethod, load_certificate, load_key_material, resolve_auth_method,
};
use crate::ssh::remote_shell_tracking_command;
use crate::terminal::terminal::{DriverEvent, DriverListener, ZedListener};

/// 发送到 SSH 通道的消息
enum InputMsg {
    Data(Vec<u8>),
    Resize(u32, u32),
    Close,
}

/// russh 客户端事件处理器
struct SshClientHandler {
    host: String,
    port: u16,
}

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        match russh::keys::known_hosts::check_known_hosts(&self.host, self.port, server_public_key)
        {
            Ok(true) => {
                log::debug!("russh: 主机密钥已知且匹配: {}:{}", self.host, self.port);
                Ok(true)
            }
            Ok(false) => {
                // 主机未知，自动添加到 known_hosts（accept-new 行为）
                log::info!(
                    "russh: 新主机 {}:{}，自动添加密钥到 known_hosts",
                    self.host,
                    self.port
                );
                if let Err(e) = russh::keys::known_hosts::learn_known_hosts(
                    &self.host,
                    self.port,
                    server_public_key,
                ) {
                    log::warn!("russh: 写入 known_hosts 失败: {}", e);
                }
                Ok(true)
            }
            Err(russh::keys::Error::KeyChanged { line }) => {
                log::error!(
                    "russh: 主机密钥已变更！{}:{} known_hosts 第 {} 行不匹配",
                    self.host,
                    self.port,
                    line
                );
                Err(russh::Error::Inconsistent)
            }
            Err(e) => {
                log::warn!("russh: known_hosts 检查出错: {}", e);
                // 检查出错时接受密钥（兼容性）
                Ok(true)
            }
        }
    }
}

/// 连接状态的内部共享状态
struct ConnectionState {
    /// 连接成功后的实际终端（连接前为 None）
    terminal: Option<RusshTerminalInner>,
    /// 连接期间的输入缓冲队列
    pending_input: VecDeque<Vec<u8>>,
    /// 连接期间的大小调整缓冲
    pending_resize: Option<(u16, u16)>,
    /// 连接是否已失败
    failed: bool,
    /// 失败原因
    error_message: Option<String>,
    /// 是否已调用过 close（保证 close 幂等；连接中调用则在连接完成后立即断开）
    closed: bool,
}

/// russh SSH 终端句柄
///
/// 支持异步初始化：创建后立即返回，在后台建立连接。
/// 连接期间缓存输入数据，连接成功后自动转发。
#[derive(Clone)]
pub struct RusshTerminal {
    state: Arc<Mutex<ConnectionState>>,
}

impl RusshTerminal {
    /// 创建新的异步 russh 终端
    ///
    /// 立即返回句柄，在后台启动连接任务。连接期间的数据会被缓存，
    /// 连接成功后自动转发到实际的 SSH 通道。
    pub fn new(
        config: &SshConnectionConfig,
        initial_cols: u32,
        initial_rows: u32,
        listener: ZedListener,
        driver_listener: DriverListener,
        initial_cwd: Option<String>,
    ) -> Self {
        let state = Arc::new(Mutex::new(ConnectionState {
            terminal: None,
            pending_input: VecDeque::new(),
            pending_resize: None,
            failed: false,
            error_message: None,
            closed: false,
        }));

        let state_for_task = state.clone();
        let config_clone = config.clone();
        let driver_listener_for_task = driver_listener.clone();
        let initial_cwd_for_task = initial_cwd.clone();

        // 在独立线程中使用共享 tokio runtime 进行连接
        std::thread::Builder::new()
            .name(format!("russh-{}", config.host))
            .spawn(move || {
                let rt = crate::ssh::shared_runtime::shared_ssh_runtime();
                rt.block_on(async move {
                    match Self::connect_and_setup(
                        &config_clone,
                        initial_cols,
                        initial_rows,
                        initial_cwd_for_task.as_deref(),
                    )
                    .await
                    {
                        Ok((handle, channel)) => {
                            log::info!("russh: 连接成功，启动 I/O 循环");

                            // 创建内部终端
                            let (input_tx, input_rx) = unbounded_channel::<InputMsg>();

                            // 连接期间被 close：立即丢弃连接，不进入 I/O 循环
                            let was_closed = {
                                let mut state = state_for_task.lock();
                                if state.closed {
                                    true
                                } else {
                                    // 先发送待处理的输入
                                    for data in std::mem::take(&mut state.pending_input) {
                                        let _ = input_tx.send(InputMsg::Data(data));
                                    }
                                    if let Some((cols, rows)) = state.pending_resize.take() {
                                        let _ =
                                            input_tx.send(InputMsg::Resize(cols as u32, rows as u32));
                                    }
                                    state.terminal = Some(RusshTerminalInner { input_tx });
                                    false
                                }
                            };
                            if was_closed {
                                log::info!("russh: 连接建立前已被关闭，立即断开");
                                drop(handle);
                                drop(channel);
                                return;
                            }

                            // 启动 I/O 循环
                            Self::io_loop(
                                handle,
                                channel,
                                input_rx,
                                listener,
                                driver_listener_for_task,
                            )
                            .await;
                        }
                        Err(e) => {
                            log::warn!("russh: 连接失败: {}", e);
                            {
                                let mut state = state_for_task.lock();
                                state.failed = true;
                                state.error_message = Some(e.to_string());
                            }
                            // 通知 GPUI 连接状态已变更
                            listener.send_event(AlacTermEvent::Wakeup);
                        }
                    }
                });
            })
            .ok();

        Self { state }
    }

    /// 检查连接是否已失败
    pub fn is_failed(&self) -> bool {
        self.state.lock().failed
    }

    /// 获取失败原因（如果连接失败）
    pub fn error_message(&self) -> Option<String> {
        self.state.lock().error_message.clone()
    }

    /// 检查连接是否已就绪
    pub fn is_ready(&self) -> bool {
        self.state.lock().terminal.is_some()
    }

    /// 向 SSH 通道发送数据（键盘输入等）
    ///
    /// 如果连接尚未就绪，数据会被缓存，连接成功后自动发送。
    pub fn write(&self, data: &[u8]) {
        let mut state = self.state.lock();

        if let Some(ref terminal) = state.terminal {
            // 连接已就绪，直接发送
            terminal.input_tx.send(InputMsg::Data(data.to_vec())).ok();
        } else if !state.failed && !state.closed {
            // 连接中，缓存数据
            state.pending_input.push_back(data.to_vec());
            // 限制缓冲区大小，防止内存无限增长
            if state.pending_input.len() > 1000 {
                state.pending_input.pop_front();
            }
        }
    }

    /// 通知远程服务器调整终端窗口大小
    ///
    /// 如果连接尚未就绪，大小调整会被缓存，连接成功后自动发送。
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut state = self.state.lock();

        if let Some(ref terminal) = state.terminal {
            // 连接已就绪，直接发送
            terminal
                .input_tx
                .send(InputMsg::Resize(cols as u32, rows as u32))
                .ok();
        } else if !state.failed && !state.closed {
            // 连接中，缓存最后的大小
            state.pending_resize = Some((cols, rows));
        }
    }

    /// 关闭 SSH 连接
    ///
    /// 幂等：重复调用安全。若连接尚未就绪，置位 `closed`，
    /// 连接任务在建立完成后会立即断开，避免泄漏仍在握手的连接。
    pub fn close(&self) {
        let mut state = self.state.lock();

        state.closed = true;
        if let Some(ref terminal) = state.terminal {
            terminal.input_tx.send(InputMsg::Close).ok();
        }
    }

    /// 建立 SSH 连接并准备 PTY 通道（内部异步方法）
    async fn connect_and_setup(
        config: &SshConnectionConfig,
        cols: u32,
        rows: u32,
        initial_cwd: Option<&str>,
    ) -> anyhow::Result<(
        client::Handle<SshClientHandler>,
        russh::Channel<client::Msg>,
    )> {
        let russh_config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            ..Default::default()
        });

        log::info!("russh: 连接到 {}:{}", config.host, config.port);
        let mut handle = client::connect(
            russh_config,
            (config.host.as_str(), config.port),
            SshClientHandler {
                host: config.host.clone(),
                port: config.port,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("TCP/SSH 握手失败: {}", e))?;

        log::info!("russh: 开始认证 (用户: {})", config.username);
        let auth_result = Self::authenticate(&mut handle, config).await?;
        if !auth_result.success() {
            return Err(anyhow::anyhow!(
                "SSH 认证被拒绝（用户: {}）",
                config.username
            ));
        }

        log::info!("russh: 认证成功，打开会话通道");
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| anyhow::anyhow!("打开 SSH 通道失败: {}", e))?;

        channel
            .request_pty(
                false,
                "xterm-256color",
                cols,
                rows,
                0,
                0,
                &[], // 使用默认终端模式
            )
            .await
            .map_err(|e| anyhow::anyhow!("请求 PTY 失败: {}", e))?;

        let shell_command = remote_shell_tracking_command(initial_cwd);
        channel
            .exec(true, shell_command.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("启动带跟踪的远程 shell 失败: {}", e))?;

        log::info!("russh: PTY + Shell 就绪");
        Ok((handle, channel))
    }

    /// 执行 SSH 认证（支持密码、密钥和自动模式）
    async fn authenticate(
        handle: &mut client::Handle<SshClientHandler>,
        config: &SshConnectionConfig,
    ) -> anyhow::Result<AuthResult> {
        let resolved_auth = resolve_auth_method(&config.auth_method)?;

        match &resolved_auth {
            ResolvedAuthMethod::Password { password } => {
                log::info!("russh: 密码认证");
                handle
                    .authenticate_password(&config.username, password.as_str())
                    .await
                    .map_err(|e| anyhow::anyhow!("密码认证失败: {}", e))
            }

            ResolvedAuthMethod::PrivateKey {
                path,
                passphrase,
                certificate_path,
                ..
            } => {
                log::info!("russh: 私钥认证 ({:?})", path);
                let key = load_key_material(path, passphrase.as_deref())?;
                if let Some(certificate_path) = certificate_path {
                    let cert = load_certificate(certificate_path)?;
                    handle
                        .authenticate_openssh_cert(&config.username, key, cert)
                        .await
                        .map_err(|e| anyhow::anyhow!("OpenSSH 证书认证失败: {}", e))
                } else {
                    let hash_alg = handle
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();
                    handle
                        .authenticate_publickey(
                            &config.username,
                            PrivateKeyWithHashAlg::new(key, hash_alg),
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("公钥认证失败: {}", e))
                }
            }

            ResolvedAuthMethod::Auto => {
                log::info!("russh: 自动认证模式（依次尝试默认密钥）");
                let default_keys = crate::ssh::find_default_ssh_keys();

                if default_keys.is_empty() {
                    return Err(anyhow::anyhow!("自动认证失败：~/.ssh 中未找到密钥文件"));
                }

                // 服务器 RSA hash 偏好在整个连接中不变，只需协商一次
                let hash_alg = handle
                    .best_supported_rsa_hash()
                    .await
                    .ok()
                    .flatten()
                    .flatten();

                for key_path in &default_keys {
                    log::info!("russh: 尝试密钥 {:?}", key_path);
                    let key = match russh::keys::load_secret_key(key_path, None) {
                        Ok(k) => k,
                        Err(e) => {
                            log::debug!("russh: 跳过密钥 {:?}: {}", key_path, e);
                            continue;
                        }
                    };
                    match handle
                        .authenticate_publickey(
                            &config.username,
                            PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                        )
                        .await
                    {
                        Ok(result) if result.success() => {
                            log::info!("russh: 密钥认证成功 ({:?})", key_path);
                            return Ok(result);
                        }
                        Ok(_) => {
                            log::debug!("russh: 密钥 {:?} 认证未成功，继续尝试", key_path);
                        }
                        Err(e) => {
                            log::debug!("russh: 密钥 {:?} 认证错误: {}", key_path, e);
                        }
                    }
                }

                Err(anyhow::anyhow!(
                    "自动认证失败：所有默认密钥均无效（用户: {}）",
                    config.username
                ))
            }

            ResolvedAuthMethod::Agent => {
                log::info!("russh: SSH Agent 认证 (SSH_AUTH_SOCK)");
                try_agent_auth(handle, config).await
            }
        }
    }

    /// SSH I/O 循环（在 tokio 任务中持续运行直到连接断开）
    ///
    /// 双向数据转发：
    /// - SSH 服务器 → VTE 处理器 → alacritty Term → 终端显示
    /// - 用户键盘输入 → SSH 通道 → 远程服务器
    async fn io_loop(
        handle: client::Handle<SshClientHandler>,
        mut channel: russh::Channel<client::Msg>,
        mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputMsg>,
        listener: ZedListener,
        driver_listener: DriverListener,
    ) {
        loop {
            tokio::select! {
                // 接收来自 SSH 服务器的数据
                msg_opt = channel.wait() => {
                    match msg_opt {
                        Some(ChannelMsg::Data { data }) => {
                            driver_listener
                                .0
                                .unbounded_send(DriverEvent::RemoteData(data))
                                .ok();
                        }
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            driver_listener
                                .0
                                .unbounded_send(DriverEvent::RemoteData(data))
                                .ok();
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            log::info!("russh: 远程进程退出，代码: {}", exit_status);
                            listener.send_event(AlacTermEvent::Exit);
                            break;
                        }
                        Some(ChannelMsg::Eof) | None => {
                            log::info!("russh: 连接关闭 (EOF)");
                            driver_listener
                                .0
                                .unbounded_send(DriverEvent::AbnormalDisconnect {
                                    reason: "连接意外关闭 (EOF)".to_string(),
                                })
                                .ok();
                            break;
                        }
                        _ => {}
                    }
                }

                // 接收用户输入并转发到 SSH 服务器
                msg_opt = input_rx.recv() => {
                    match msg_opt {
                        Some(InputMsg::Data(data)) => {
                            if let Err(e) = channel.data(data.as_ref()).await {
                                log::error!("russh: 发送数据到服务器失败: {}", e);
                                driver_listener
                                    .0
                                    .unbounded_send(DriverEvent::AbnormalDisconnect {
                                        reason: format!("发送数据失败: {}", e),
                                    })
                                    .ok();
                                break;
                            }
                        }
                        Some(InputMsg::Resize(cols, rows)) => {
                            if let Err(e) = channel.window_change(cols, rows, 0, 0).await {
                                log::warn!("russh: 调整窗口大小失败: {}", e);
                            }
                        }
                        Some(InputMsg::Close) | None => {
                            log::info!("russh: 收到关闭指令，断开连接");
                            channel.eof().await.ok();
                            handle
                                .disconnect(Disconnect::ByApplication, "", "English")
                                .await
                                .ok();
                            break;
                        }
                    }
                }
            }
        }

        log::info!("russh: I/O 循环结束");
    }
}

/// 内部终端结构（连接成功后创建）
struct RusshTerminalInner {
    input_tx: UnboundedSender<InputMsg>,
}
