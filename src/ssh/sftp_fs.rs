use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use russh::client::{self, AuthResult};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::file_transfer::TRANSFER_CANCELLED_MSG;
use crate::ssh::auth::{try_agent_auth, try_default_keys};
use crate::ssh::config::SshConnectionConfig;
use crate::ssh::keychain::{
    ResolvedAuthMethod, load_certificate, load_key_material, resolve_auth_method,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteFileKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug)]
pub struct RemoteFileEntry {
    pub name: String,
    pub path: String,
    pub kind: RemoteFileKind,
    pub size: u64,
    pub modified_at: Option<std::time::SystemTime>,
}

#[derive(Clone, Debug)]
pub struct RemoteFileMetadata {
    pub kind: RemoteFileKind,
    pub size: u64,
    pub modified_at: Option<std::time::SystemTime>,
    pub permissions: Option<u32>,
}

struct SftpClientHandler;

impl client::Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct BlockingSftpSession {
    runtime: tokio::runtime::Runtime,
    handle: client::Handle<SftpClientHandler>,
    sftp: SftpSession,
}

impl BlockingSftpSession {
    const TRANSFER_BUFFER_SIZE: usize = 1024 * 1024;

    pub fn connect(config: &SshConnectionConfig) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Runtime::new()?;
        let (handle, sftp) = runtime.block_on(connect_sftp(config))?;
        Ok(Self {
            runtime,
            handle,
            sftp,
        })
    }

    pub fn canonicalize(&mut self, path: &str) -> anyhow::Result<String> {
        self.runtime.block_on(async {
            self.sftp
                .canonicalize(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn metadata(&mut self, path: &str) -> anyhow::Result<RemoteFileMetadata> {
        self.runtime.block_on(async {
            let metadata = self.sftp.metadata(path).await?;
            Ok(convert_metadata(
                metadata.file_type(),
                metadata.len(),
                metadata.mtime,
                metadata.permissions,
            ))
        })
    }

    pub fn permissions(&mut self, path: &str) -> anyhow::Result<Option<u32>> {
        self.runtime.block_on(async {
            let metadata = self.sftp.metadata(path).await?;
            Ok(metadata.permissions)
        })
    }

    pub fn set_permissions(&mut self, path: &str, mode: u32) -> anyhow::Result<()> {
        self.runtime.block_on(async {
            let mut attrs = FileAttributes::default();
            attrs.permissions = Some(mode);
            self.sftp.set_metadata(path, attrs).await?;
            Ok(())
        })
    }

    pub fn exists(&mut self, path: &str) -> anyhow::Result<bool> {
        self.runtime.block_on(async {
            self.sftp
                .try_exists(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn create_dir_all(&mut self, path: &str) -> anyhow::Result<()> {
        if path.is_empty() || path == "/" {
            return Ok(());
        }

        let mut current = if path.starts_with('/') {
            "/".to_string()
        } else {
            String::new()
        };

        for component in remote_components(path) {
            current = join_remote_path(&current, component);
            if self.exists(&current)? {
                continue;
            }

            self.runtime
                .block_on(async { self.sftp.create_dir(current.clone()).await })?;
        }

        Ok(())
    }

    pub fn rename(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
        self.runtime.block_on(async {
            self.sftp
                .rename(from, to)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn remove_file(&mut self, path: &str) -> anyhow::Result<()> {
        self.runtime.block_on(async {
            self.sftp
                .remove_file(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn remove_dir(&mut self, path: &str) -> anyhow::Result<()> {
        self.runtime.block_on(async {
            self.sftp
                .remove_dir(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn upload_file_from_local(
        &mut self,
        local_path: &Path,
        remote_path: &str,
    ) -> anyhow::Result<u64> {
        self.upload_file_from_local_with_cancel(local_path, remote_path, &AtomicBool::new(false))
    }

    pub fn upload_file_from_local_with_cancel(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<u64> {
        self.upload_file_from_local_with_cancel_and_progress(
            local_path,
            remote_path,
            cancelled,
            None,
        )
    }

    pub fn upload_file_from_local_with_cancel_and_progress(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        cancelled: &AtomicBool,
        progress: Option<&std::sync::atomic::AtomicU64>,
    ) -> anyhow::Result<u64> {
        let mut local_file = File::open(local_path)?;
        self.runtime.block_on(async {
            let mut remote_file = self.sftp.create(remote_path).await?;
            let mut transferred = 0;
            let mut buffer = vec![0; Self::TRANSFER_BUFFER_SIZE];

            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
                }

                let read = local_file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }

                remote_file.write_all(&buffer[..read]).await?;
                transferred += read as u64;

                if let Some(p) = progress {
                    p.fetch_add(read as u64, Ordering::Relaxed);
                }
            }

            if cancelled.load(Ordering::Relaxed) {
                return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
            }

            remote_file.shutdown().await?;
            Ok(transferred)
        })
    }

    pub fn download_file_to_local(
        &mut self,
        remote_path: &str,
        local_path: &Path,
    ) -> anyhow::Result<u64> {
        self.download_file_to_local_with_cancel(remote_path, local_path, &AtomicBool::new(false))
    }

    pub fn download_file_to_local_with_cancel(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<u64> {
        self.download_file_to_local_with_cancel_and_progress(
            remote_path,
            local_path,
            cancelled,
            None,
        )
    }

    pub fn download_file_to_local_with_cancel_and_progress(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        cancelled: &AtomicBool,
        progress: Option<&std::sync::atomic::AtomicU64>,
    ) -> anyhow::Result<u64> {
        let mut local_file = File::create(local_path)?;
        self.runtime.block_on(async {
            let mut remote_file = self.sftp.open(remote_path).await?;
            let mut transferred = 0;
            let mut buffer = vec![0; Self::TRANSFER_BUFFER_SIZE];

            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
                }

                let read = remote_file.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }

                local_file.write_all(&buffer[..read])?;
                transferred += read as u64;

                if let Some(p) = progress {
                    p.fetch_add(read as u64, Ordering::Relaxed);
                }
            }

            if cancelled.load(Ordering::Relaxed) {
                return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
            }

            local_file.flush()?;
            Ok(transferred)
        })
    }

    /// 同服务器远程文件流式复制：源路径 → 目标路径，不写本地磁盘。
    /// 数据在同一个 SFTP session 内直接流转，避免磁盘中转。
    pub fn copy_remote_file(
        &mut self,
        source_path: &str,
        destination_path: &str,
        cancelled: &AtomicBool,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<u64> {
        let source_permissions = self.permissions(source_path).ok().flatten();
        // 使用临时文件避免传输中断时污染目标路径
        let temporary_path = format!("{}.deepterm-part", destination_path);

        if self.exists(&temporary_path)? {
            let _ = self.remove_file(&temporary_path);
        }

        // 核心：在单个 block_on 内交替读取源文件并写入目标文件，全程内存缓冲
        let transferred = self.runtime.block_on(async {
            let mut source_file = self.sftp.open(source_path).await?;
            let mut dest_file = self.sftp.create(&temporary_path).await?;
            let mut buffer = vec![0; Self::TRANSFER_BUFFER_SIZE];
            let mut transferred = 0u64;

            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
                }

                let read = source_file.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }

                dest_file.write_all(&buffer[..read]).await?;
                transferred += read as u64;

                if let Some(p) = progress {
                    p.fetch_add(read as u64, Ordering::Relaxed);
                }
            }

            if cancelled.load(Ordering::Relaxed) {
                return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
            }

            dest_file.shutdown().await?;
            Ok(transferred)
        });

        // 传输完成后重命名为最终文件名，失败时清理临时文件
        match transferred {
            Ok(bytes) => {
                self.rename(&temporary_path, destination_path)?;
                if let Some(mode) = source_permissions {
                    let _ = self.set_permissions(destination_path, mode);
                }
                Ok(bytes)
            }
            Err(e) => {
                let _ = self.remove_file(&temporary_path);
                Err(e)
            }
        }
    }

    /// 跨服务器流式传输：从当前 session 读取源文件，写入目标 session 的目标路径，不写本地磁盘。
    /// 数据仅在内存 buffer 中中转，源端读取和目的端写入在同一个异步上下文交替执行。
    ///
    /// # 运行时假设
    ///
    /// 本方法在 `self.runtime` 的 `block_on` 中同时访问 `destination_session.sftp`。
    /// 这要求 `destination_session` 的 `SftpSession` 可以跨 tokio runtime 使用。
    /// `russh-sftp` 内部通过 SSH channel buffer 与事件循环通信，不直接依赖 reactor，
    /// 因此该模式在实践中可行。如果未来 `russh-sftp` 版本引入 runtime 亲和性检查，
    /// 需要改为分两步执行：在各自 runtime 上分别完成读取和写入。
    pub fn stream_transfer_to(
        &mut self,
        source_path: &str,
        destination_session: &mut BlockingSftpSession,
        destination_path: &str,
        cancelled: &AtomicBool,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<u64> {
        self.runtime.block_on(async {
            let mut source_file = self.sftp.open(source_path).await?;
            let mut dest_file = destination_session.sftp.create(destination_path).await?;
            let mut buffer = vec![0; Self::TRANSFER_BUFFER_SIZE];
            let mut transferred = 0u64;

            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
                }

                let read = source_file.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }

                dest_file.write_all(&buffer[..read]).await?;
                transferred += read as u64;

                if let Some(p) = progress {
                    p.fetch_add(read as u64, Ordering::Relaxed);
                }
            }

            if cancelled.load(Ordering::Relaxed) {
                return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
            }

            dest_file.shutdown().await?;
            Ok(transferred)
        })
    }

    pub fn read_dir(&mut self, path: &str) -> anyhow::Result<Vec<RemoteFileEntry>> {
        self.runtime.block_on(async {
            let entries = self
                .sftp
                .read_dir(path)
                .await?
                .map(|entry| {
                    let metadata = entry.metadata();
                    RemoteFileEntry {
                        name: entry.file_name(),
                        path: join_remote_path(path, &entry.file_name()),
                        kind: convert_file_type(entry.file_type()),
                        size: metadata.len(),
                        modified_at: metadata.mtime.map(|secs| {
                            std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
                        }),
                    }
                })
                .collect();
            Ok(entries)
        })
    }

    pub fn read_text_preview(&mut self, path: &str) -> anyhow::Result<Option<String>> {
        use crate::file_browser::shared::is_binary_content;
        const PREVIEW_BYTES: usize = 64 * 1024;

        self.runtime.block_on(async {
            let file = self.sftp.open(path).await?;
            let mut bytes = Vec::new();
            file.take(PREVIEW_BYTES as u64)
                .read_to_end(&mut bytes)
                .await?;

            if bytes.is_empty() {
                return Ok(Some("Empty file".to_string()));
            }

            if is_binary_content(&bytes) {
                return Ok(None);
            }

            let text = String::from_utf8_lossy(&bytes).into_owned();
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(text))
            }
        })
    }

    /// 在远程服务器上执行 shell 命令，返回 stdout/stderr 和退出码。
    /// 用于 Phase 3 服务器直连场景，在源服务器执行 scp/rsync 命令。
    pub fn execute_command(
        &mut self,
        command: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<RemoteCommandResult> {
        self.runtime.block_on(async {
            let mut channel = self
                .handle
                .channel_open_session()
                .await
                .map_err(|e| anyhow!("打开 SSH 会话通道失败: {}", e))?;

            // 不分配 PTY：避免终端转义字符干扰命令输出
            channel
                .exec(false, command)
                .await
                .map_err(|e| anyhow!("执行远程命令失败: {}", e))?;

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut exit_status = None;

            let timeout = tokio::time::Duration::from_secs(timeout_secs);

            let result = tokio::time::timeout(timeout, async {
                loop {
                    match channel.wait().await {
                        Some(russh::ChannelMsg::Data { data }) => {
                            stdout.extend_from_slice(&data);
                        }
                        Some(russh::ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                            stderr.extend_from_slice(&data);
                        }
                        Some(russh::ChannelMsg::ExitStatus { exit_status: status }) => {
                            exit_status = Some(status);
                        }
                        Some(russh::ChannelMsg::Eof) => {
                            // 标准 SSH 协议顺序为：
                            //   ExitStatus → Eof → Close
                            // 此处不 break，继续等待 Close，
                            // 以防 ExitStatus 在 Eof 之后到达
                        }
                        Some(russh::ChannelMsg::Close) => {
                            break;
                        }
                        None => {
                            break;
                        }
                        _ => {}
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;

            // 确保通道关闭
            let _ = channel.close().await;

            match result {
                Ok(Ok(())) => Ok(RemoteCommandResult {
                    stdout: String::from_utf8_lossy(&stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                    exit_status,
                }),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow!("远程命令执行超时 ({}s)", timeout_secs)),
            }
        })
    }
}

/// 远程命令执行结果
#[derive(Clone, Debug)]
pub struct RemoteCommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_status: Option<u32>,
}

impl RemoteCommandResult {
    pub fn success(&self) -> bool {
        self.exit_status == Some(0)
    }
}

type SharedBlockingSftpSession = Arc<Mutex<BlockingSftpSession>>;

fn session_cache() -> &'static Mutex<HashMap<String, SharedBlockingSftpSession>> {
    static CACHE: OnceLock<Mutex<HashMap<String, SharedBlockingSftpSession>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(config: &SshConnectionConfig) -> String {
    // v2: inactivity_timeout disabled for long file transfers
    format!(
        "v2:{}",
        serde_json::to_string(config)
            .unwrap_or_else(|_| format!("{}@{}:{}", config.username, config.host, config.port))
    )
}

fn get_or_connect_session(
    config: &SshConnectionConfig,
    key: &str,
) -> anyhow::Result<SharedBlockingSftpSession> {
    if let Some(session) = session_cache().lock().unwrap().get(key).cloned() {
        return Ok(session);
    }

    let session = Arc::new(Mutex::new(BlockingSftpSession::connect(config)?));
    let mut cache = session_cache().lock().unwrap();
    Ok(cache
        .entry(key.to_string())
        .or_insert_with(|| session.clone())
        .clone())
}

fn evict_session(key: &str, session: &SharedBlockingSftpSession) {
    let mut cache = session_cache().lock().unwrap();
    if cache
        .get(key)
        .is_some_and(|cached| Arc::ptr_eq(cached, session))
    {
        cache.remove(key);
    }
}

pub fn with_cached_session<T>(
    config: &SshConnectionConfig,
    mut f: impl FnMut(&mut BlockingSftpSession) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let key = cache_key(config);
    let session = get_or_connect_session(config, &key)?;

    match f(&mut session.lock().unwrap()) {
        Ok(result) => Ok(result),
        Err(_) => {
            evict_session(&key, &session);
            let session = get_or_connect_session(config, &key)?;
            f(&mut session.lock().unwrap())
        }
    }
}

impl Drop for BlockingSftpSession {
    fn drop(&mut self) {
        self.runtime.block_on(async {
            let _ = self.sftp.close().await;
            let _ = self
                .handle
                .disconnect(russh::Disconnect::ByApplication, "", "English")
                .await;
        });
    }
}

pub fn canonicalize(config: &SshConnectionConfig, path: &str) -> anyhow::Result<String> {
    with_cached_session(config, |session| session.canonicalize(path))
}

pub fn metadata(config: &SshConnectionConfig, path: &str) -> anyhow::Result<RemoteFileMetadata> {
    with_cached_session(config, |session| session.metadata(path))
}

pub fn read_dir(config: &SshConnectionConfig, path: &str) -> anyhow::Result<Vec<RemoteFileEntry>> {
    with_cached_session(config, |session| session.read_dir(path))
}

pub fn read_text_preview(
    config: &SshConnectionConfig,
    path: &str,
) -> anyhow::Result<Option<String>> {
    with_cached_session(config, |session| session.read_text_preview(path))
}

async fn connect_sftp(
    config: &SshConnectionConfig,
) -> anyhow::Result<(client::Handle<SftpClientHandler>, SftpSession)> {
    let russh_config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });

    let mut handle = client::connect(
        russh_config,
        (config.host.as_str(), config.port),
        SftpClientHandler,
    )
    .await
    .map_err(|e| anyhow!("TCP/SSH 握手失败: {}", e))?;

    let auth_result = authenticate(&mut handle, config).await?;
    if !auth_result.success() {
        return Err(anyhow!("SSH 认证被拒绝（用户: {}）", config.username));
    }

    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| anyhow!("打开 SSH 通道失败: {}", e))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| anyhow!("启动 SFTP 子系统失败: {}", e))?;

    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| anyhow!("初始化 SFTP 会话失败: {}", e))?;
    Ok((handle, sftp))
}

async fn authenticate(
    handle: &mut client::Handle<SftpClientHandler>,
    config: &SshConnectionConfig,
) -> anyhow::Result<AuthResult> {
    let resolved_auth = resolve_auth_method(&config.auth_method)?;

    match &resolved_auth {
        ResolvedAuthMethod::Password { password } => handle
            .authenticate_password(&config.username, password)
            .await
            .map_err(|e| anyhow!("密码认证失败: {}", e)),
        ResolvedAuthMethod::PrivateKey {
            path,
            passphrase,
            certificate_path,
            ..
        } => {
            let key = load_key_material(path, passphrase.as_deref())?;
            if let Some(certificate_path) = certificate_path {
                let cert = load_certificate(certificate_path)?;
                handle
                    .authenticate_openssh_cert(&config.username, key, cert)
                    .await
                    .map_err(|e| anyhow!("OpenSSH 证书认证失败: {}", e))
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
                    .map_err(|e| anyhow!("公钥认证失败: {}", e))
            }
        }
        ResolvedAuthMethod::Auto => try_default_keys(handle, config).await,
        ResolvedAuthMethod::Agent => try_agent_auth(handle, config).await,
    }
}

fn convert_file_type(file_type: FileType) -> RemoteFileKind {
    match file_type {
        FileType::Dir => RemoteFileKind::Directory,
        FileType::File => RemoteFileKind::File,
        FileType::Symlink => RemoteFileKind::Symlink,
        FileType::Other => RemoteFileKind::Other,
    }
}

fn convert_metadata(
    file_type: FileType,
    size: u64,
    mtime: Option<u32>,
    permissions: Option<u32>,
) -> RemoteFileMetadata {
    RemoteFileMetadata {
        kind: convert_file_type(file_type),
        size,
        modified_at: mtime
            .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)),
        permissions,
    }
}

pub fn join_remote_path(base: &str, child: &str) -> String {
    if base == "/" {
        format!("/{}", child.trim_start_matches('/'))
    } else if base.is_empty() {
        child.to_string()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            child.trim_start_matches('/')
        )
    }
}

pub fn remote_parent(path: &str) -> Option<String> {
    if path.is_empty() || path == "/" {
        return None;
    }

    let trimmed = path.trim_end_matches('/');
    if let Some(index) = trimmed.rfind('/') {
        if index == 0 {
            Some("/".to_string())
        } else {
            Some(trimmed[..index].to_string())
        }
    } else {
        None
    }
}

pub fn remote_components(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}
