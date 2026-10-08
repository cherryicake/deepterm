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
///
/// `pub(crate)`：共享转发隧道（[`crate::ssh::tunnel`]）也要用它建连接——隧道
/// 连接与终端连接共用同一套主机密钥校验与转发认领逻辑，两边行为必须一致。
pub(crate) struct SshClientHandler {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) host_key_policy: crate::ssh::host_key::HostKeyPolicy,
    /// 端口转发的认领表：有人连到远端监听端口时，服务器会发起
    /// `forwarded-tcpip` 通道，由这里查到本机目标并桥接过去。
    ///
    /// 终端连接传**空表**：转发全部挂在专职隧道连接上（见
    /// [`crate::ssh::tunnel`]），终端连接不请求监听，也就不会收到
    /// `forwarded-tcpip`；真收到了说明不是我们的转发，按未登记忽略。
    pub(crate) port_forwards: crate::ssh::port_forward::RemoteForwardTable,
}

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // 统一走 host_key::verify_server_key，与 SFTP 通道共用逻辑（防一侧漏检）。
        crate::ssh::host_key::verify_server_key(
            &self.host,
            self.port,
            server_public_key,
            self.host_key_policy,
        )
        .await
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let Some((target_host, target_port)) = self
            .port_forwards
            .lookup(connected_address, connected_port as u16)
        else {
            // 认领不到就不是我们登记的转发：不桥接，直接丢弃（通道 drop 即关闭）。
            log::warn!("russh: 收到未登记的转发连接 {connected_address}:{connected_port}，已忽略");
            return Ok(());
        };
        log::info!(
            "russh: 转发连接 {connected_address}:{connected_port} → 本机 {target_host}:{target_port}"
        );
        tokio::spawn(async move {
            if let Err(e) = crate::ssh::port_forward::serve_forwarded_channel(
                channel,
                target_host.clone(),
                target_port,
            )
            .await
            {
                log::warn!("russh: 转发到本机 {target_host}:{target_port} 失败: {e}");
            }
        });
        Ok(())
    }
}

/// 连接期间的输入缓冲上限（字节）：连接期通常只有键盘节奏的输入，
/// 但大段粘贴会一次写入大块；超限后从最旧的开始丢弃，防止内存无限增长。
const PENDING_INPUT_MAX_BYTES: usize = 256 * 1024;

/// 连接状态的内部共享状态
struct ConnectionState {
    /// 连接成功后的实际终端（连接前为 None）
    terminal: Option<RusshTerminalInner>,
    /// 连接期间的输入缓冲队列
    pending_input: VecDeque<Vec<u8>>,
    /// 输入缓冲的字节总量（与 pending_input 同步维护）
    pending_input_bytes: usize,
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
            pending_input_bytes: 0,
            pending_resize: None,
            failed: false,
            error_message: None,
            closed: false,
        }));

        let state_for_task = state.clone();
        // spawn 失败分支（inspect_err）专用句柄：spawn 闭包按 move 捕获
        // state_for_task，若共用同一变量会在编译期与之冲突。
        let state_for_spawn_err = state.clone();
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
                        &driver_listener_for_task,
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
                                    let pending = std::mem::take(&mut state.pending_input);
                                    state.pending_input_bytes = 0;
                                    for data in pending {
                                        let _ = input_tx.send(InputMsg::Data(data));
                                    }
                                    if let Some((cols, rows)) = state.pending_resize.take() {
                                        let _ = input_tx
                                            .send(InputMsg::Resize(cols as u32, rows as u32));
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
                                state_for_task.clone(),
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
            .inspect_err(|e| {
                // 线程资源耗尽等 spawn 失败：此前被 `.ok()` 静默吞掉，
                // ConnectionState 永远停在"连接中"，UI 卡死在转圈。
                // 必须立即置失败并通知监听方。
                log::error!("russh: 连接线程启动失败: {}", e);
                let mut state = state_for_spawn_err.lock();
                state.failed = true;
                state.error_message = Some(format!("无法启动连接线程: {}", e));
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
            // 连接中，缓存数据；按字节限流（条数限制对大块粘贴无效）
            state.pending_input_bytes += data.len();
            state.pending_input.push_back(data.to_vec());
            while state.pending_input_bytes > PENDING_INPUT_MAX_BYTES {
                match state.pending_input.pop_front() {
                    Some(d) => state.pending_input_bytes -= d.len(),
                    None => break,
                }
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
        driver_listener_for_setup: &DriverListener,
    ) -> anyhow::Result<(
        client::Handle<SshClientHandler>,
        russh::Channel<client::Msg>,
    )> {
        let handle = connect_and_authenticate(
            config,
            SshClientHandler {
                host: config.host.clone(),
                port: config.port,
                host_key_policy: crate::ssh::host_key::policy_from_global_config(),
                // 终端连接不承担转发（见 `tunnel` 模块）：空表意味着任何
                // `forwarded-tcpip` 都是未登记的，按忽略处理。
                port_forwards: crate::ssh::port_forward::RemoteForwardTable::default(),
            },
        )
        .await?;

        // 转发挂在**按主机共享的专职隧道**上，不在本连接上：反向监听端口是
        // 远端资源，每个标签各请求一次的话，第二个标签起必然被远端拒绝
        // （端口被本进程上一个标签占着）。
        //
        // 规则为空但该主机已有隧道时也要走一遍：用户可能刚把规则全删了，
        // 那些远端监听需要随之撤销，不该留到进程退出。
        if !config.port_forwards.is_empty() || crate::ssh::tunnel::has_live_tunnel(config) {
            // 后台进行、不 await：隧道是一条独立的 SSH 连接，首建要握手+认证，
            // 挡在这里会让每个新标签的 shell 都多等一次连接建立。结果经
            // `DriverEvent::PortForward` 异步报到 UI，失败不阻断登录。
            let config_for_tunnel = config.clone();
            let driver_for_tunnel = driver_listener_for_setup.clone();
            tokio::spawn(async move {
                let outcome = crate::ssh::tunnel::acquire(
                    &config_for_tunnel,
                    &config_for_tunnel.port_forwards,
                    // 单条 tcpip-forward 请求的等待上限：这一段不在握手超时里，
                    // 不回应的服务器不能把登录一起拖死。
                    Duration::from_secs(config_for_tunnel.timeout.max(1)),
                )
                .await;
                for e in &outcome.errors {
                    log::warn!("russh: 端口转发失败: {e}");
                }
                if let Some((ok, message)) = crate::ssh::port_forward::tunnel_outcome_message(
                    &outcome.established,
                    &outcome.reused,
                    &outcome.adopted,
                    &outcome.errors,
                ) {
                    driver_for_tunnel
                        .0
                        .send(DriverEvent::PortForward { message, ok })
                        .await
                        .ok();
                }
            });
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

    /// SSH I/O 循环（在 tokio 任务中持续运行直到连接断开）
    ///
    /// 双向数据转发：
    /// - SSH 服务器 → VTE 处理器 → alacritty Term → 终端显示
    /// - 用户键盘输入 → SSH 通道 → 远程服务器
    async fn io_loop(
        self_state: Arc<Mutex<ConnectionState>>,
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
                            // 有界通道：UI 消费不过来时在此挂起，背压传导到远端
                            driver_listener.0.send(DriverEvent::RemoteData(data)).await.ok();
                        }
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            driver_listener.0.send(DriverEvent::RemoteData(data)).await.ok();
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            log::info!("russh: 远程进程退出，代码: {}", exit_status);
                            listener.send_event(AlacTermEvent::Exit);
                            break;
                        }
                        Some(ChannelMsg::Eof) | None => {
                            log::info!("russh: 连接关闭 (EOF)");
                            driver_listener.0
                                .send(DriverEvent::AbnormalDisconnect {
                                    reason: "连接意外关闭 (EOF)".to_string(),
                                })
                                .await
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
                                driver_listener.0
                                    .send(DriverEvent::AbnormalDisconnect {
                                        reason: format!("发送数据失败: {}", e),
                                    })
                                    .await
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
        // 循环退出即连接终结：清理 terminal 句柄并置 failed，让
        // is_ready() 反映真实状态。此前 terminal 残留为 Some，上层
        // 查询恒为"就绪"，write() 的输入被静默送进已关闭的 channel 丢失。
        {
            let mut state = self_state.lock();
            let user_closed = state.closed;
            state.terminal = None;
            if !user_closed {
                state.failed = true;
                if state.error_message.is_none() {
                    state.error_message = Some("连接已断开".to_string());
                }
            }
        }
        listener.send_event(AlacTermEvent::Wakeup);
    }
}

/// 内部终端结构（连接成功后创建）
struct RusshTerminalInner {
    input_tx: UnboundedSender<InputMsg>,
}

/// 建立 SSH 连接并完成认证（终端连接与共享转发隧道共用）。
///
/// 抽出来的意义：两条连接的**握手与认证必须完全一致**（同样的超时、keepalive、
/// 主机密钥校验、认证回退链）。此前隧道若各自实现一遍，任何一侧漏改都会让隧道
/// 与终端在同一个主机上表现不一致。
pub(crate) async fn connect_and_authenticate<H: client::Handler + Send + 'static>(
    config: &SshConnectionConfig,
    handler: H,
) -> anyhow::Result<client::Handle<H>> {
    let russh_config = Arc::new(client::Config {
        // keepalive 30s × 3 次无响应 = 90s 判定断连。inactivity 若设为
        // 60s 会先于 keepalive 兜底触发（keepalive_max 形同虚设），故置
        // None，让断连判定完全由 keepalive 负责；对正常空闲会话（keepalive
        // 有响应）无影响。
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });

    log::info!("russh: 连接到 {}:{}", config.host, config.port);
    // config.timeout 生效：TCP+SSH 握手整体限时。此前该配置从未被使用，
    // 不可达主机要等 OS 级 SYN 重试（约 75-130s）UI 才能报错。
    let mut handle = tokio::time::timeout(Duration::from_secs(config.timeout.max(1)), async {
        // 经系统代理时先完成代理握手，再把已连通的流交给 russh；直连
        // 交给 `client::connect`（它顺带处理 nodelay 等套接字选项）。
        match crate::ssh::system_proxy::dial(config).await? {
            Some(stream) => client::connect_stream(russh_config, stream, handler)
                .await
                // H::Error 只保证 Debug（russh 的约束），用 {:?} 而不是 {}。
                .map_err(|e| anyhow::anyhow!("TCP/SSH 握手失败: {e:?}")),
            None => client::connect(russh_config, (config.host.as_str(), config.port), handler)
                .await
                .map_err(|e| anyhow::anyhow!("TCP/SSH 握手失败: {e:?}")),
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "连接超时（{} 秒内未完成与 {}:{} 的 TCP/SSH 握手）",
            config.timeout,
            config.host,
            config.port
        )
    })??;

    log::info!("russh: 开始认证 (用户: {})", config.username);
    let auth_result = authenticate(&mut handle, config).await?;
    if !auth_result.success() {
        return Err(anyhow::anyhow!(
            "SSH 认证被拒绝（用户: {}）",
            config.username
        ));
    }
    Ok(handle)
}

/// 执行 SSH 认证（支持密码、密钥和自动模式）
async fn authenticate<H: client::Handler>(
    handle: &mut client::Handle<H>,
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
            let key = load_key_material(path, passphrase.as_deref().map(|p| p.as_str()))?;
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
            // "尝试多种方法"语义：agent → 默认密钥。auth.rs 的 try_agent_auth
            // 已实现完整回退链（agent 不可达/无密钥时自动尝试 ~/.ssh 默认密钥），
            // 此前这里只试默认密钥，agent 有密钥但 ~/.ssh 无默认密钥的用户
            // 在 Auto 模式下认证失败（与 SFTP 通道行为一致地错）。
            log::info!("russh: 自动认证模式（agent → 默认密钥）");
            try_agent_auth(handle, config).await
        }

        ResolvedAuthMethod::Agent => {
            log::info!("russh: SSH Agent 认证 (SSH_AUTH_SOCK)");
            try_agent_auth(handle, config).await
        }
    }
}
