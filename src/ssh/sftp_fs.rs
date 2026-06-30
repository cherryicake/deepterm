use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use parking_lot::Mutex;
use std::time::Duration;

use anyhow::anyhow;
use russh::client::{self, AuthResult};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
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
    handle: client::Handle<SftpClientHandler>,
    sftp: SftpSession,
    /// 懒启的 bulk 通道：为管道化批量 read/write 使用独立 SFTP subsystem，
    /// 避免与常规 `sftp` 元数据操作竞争同一个 request id 空间。
    bulk: Option<Arc<RawSftpSession>>,
}

impl BlockingSftpSession {
    const TRANSFER_BUFFER_SIZE: usize = 1024 * 1024;

    pub fn connect(config: &SshConnectionConfig) -> anyhow::Result<Self> {
        let rt = crate::ssh::shared_runtime::shared_ssh_runtime();
        let (handle, sftp) = rt.block_on(connect_sftp(config))?;
        Ok(Self {
            handle,
            sftp,
            bulk: None,
        })
    }

    /// 返回当前 session 的 bulk `RawSftpSession`，首次调用时惰性建立独立 channel。
    ///
    /// 该通道与 `self.sftp` 使用不同的 SSH channel，因此可以在 bulk 传输
    /// 进行时与元数据操作 / 取消检查并发，不会相互阻塞。
    pub(crate) fn bulk_session(&mut self) -> anyhow::Result<Arc<RawSftpSession>> {
        if let Some(existing) = &self.bulk {
            return Ok(existing.clone());
        }
        let session = crate::ssh::shared_runtime::shared_ssh_runtime().block_on(open_raw_sftp_session(&self.handle))?;
        let arc = Arc::new(session);
        self.bulk = Some(arc.clone());
        Ok(arc)
    }

    pub fn canonicalize(&mut self, path: &str) -> anyhow::Result<String> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            self.sftp
                .canonicalize(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn metadata(&mut self, path: &str) -> anyhow::Result<RemoteFileMetadata> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            let metadata = self.sftp.metadata(path).await?;
            Ok(convert_metadata(
                metadata.file_type(),
                metadata.len(),
                metadata.mtime,
                metadata.permissions,
            ))
        })
    }

    /// 与 [`metadata`](Self::metadata) 相同，但**不跟随符号链接**：返回链接自身的属性。
    /// 用于删源等需要区分"链接本身"与"链接目标"的场景，避免误删/误递归到目标。
    pub fn symlink_metadata(&mut self, path: &str) -> anyhow::Result<RemoteFileMetadata> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            let metadata = self.sftp.symlink_metadata(path).await?;
            Ok(convert_metadata(
                metadata.file_type(),
                metadata.len(),
                metadata.mtime,
                metadata.permissions,
            ))
        })
    }

    pub fn permissions(&mut self, path: &str) -> anyhow::Result<Option<u32>> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            let metadata = self.sftp.metadata(path).await?;
            Ok(metadata.permissions)
        })
    }

    pub fn set_permissions(&mut self, path: &str, mode: u32) -> anyhow::Result<()> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            let mut attrs = FileAttributes::empty();
            attrs.permissions = Some(mode);
            self.sftp.set_metadata(path, attrs).await?;
            Ok(())
        })
    }

    pub fn exists(&mut self, path: &str) -> anyhow::Result<bool> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
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

            crate::ssh::shared_runtime::shared_ssh_runtime()
                .block_on(async { self.sftp.create_dir(current.clone()).await })?;
        }

        Ok(())
    }

    pub fn rename(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            self.sftp
                .rename(from, to)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn remove_file(&mut self, path: &str) -> anyhow::Result<()> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            self.sftp
                .remove_file(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn remove_dir(&mut self, path: &str) -> anyhow::Result<()> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
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
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
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

            remote_file.flush().await?;
            remote_file.sync_all().await?;
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
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
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
            local_file.sync_all()?;
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
        let size = self.metadata(source_path)?.size;
        // 使用临时文件避免传输中断时污染目标路径
        let temporary_path = format!("{}.deepterm-part", destination_path);

        if self.exists(&temporary_path)? {
            let _ = self.remove_file(&temporary_path);
        }

        let bulk = self.bulk_session()?;
        let config = ParallelTransferConfig::from_env();

        let transferred = crate::ssh::shared_runtime::shared_ssh_runtime().block_on(pipelined_copy_between(
            bulk.clone(),
            source_path,
            bulk,
            &temporary_path,
            size,
            config,
            cancelled,
            progress,
        ));

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
    /// # 并发策略
    ///
    /// 底层使用两条独立的 bulk SFTP channel（分别从源/目的 session 懒启）。
    /// 同时保持多个 in-flight `read`/`write` 请求，显著提升高 RTT 链路的吞吐量。
    /// 具体并发度由 `ParallelTransferConfig::from_env` 从环境变量读取。
    pub fn stream_transfer_to(
        &mut self,
        source_path: &str,
        destination_session: &mut BlockingSftpSession,
        destination_path: &str,
        cancelled: &AtomicBool,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<u64> {
        let size = self.metadata(source_path)?.size;
        let source_raw = self.bulk_session()?;
        let dest_raw = destination_session.bulk_session()?;
        let config = ParallelTransferConfig::from_env();

        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(pipelined_copy_between(
            source_raw,
            source_path,
            dest_raw,
            destination_path,
            size,
            config,
            cancelled,
            progress,
        ))
    }

    pub fn read_dir(&mut self, path: &str) -> anyhow::Result<Vec<RemoteFileEntry>> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
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
        const PREVIEW_BYTES: usize = 64 * 1024;

        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
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
        self.execute_command_inner(
            command,
            timeout_secs,
            None,
            None::<&mut dyn FnMut(&[u8])>,
            false,
        )
    }

    /// 带取消检查的远程命令执行。每 200ms 检查一次 `cancelled`，
    /// 为 true 时关闭 SSH channel 并返回错误。
    pub fn execute_command_with_cancel(
        &mut self,
        command: &str,
        timeout_secs: u64,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<RemoteCommandResult> {
        self.execute_command_inner(
            command,
            timeout_secs,
            Some(cancelled),
            None::<&mut dyn FnMut(&[u8])>,
            false,
        )
    }

    /// 在远程服务器上执行命令，并为该通道启用 **SSH agent forwarding**。
    pub fn execute_command_with_agent_forward(
        &mut self,
        command: &str,
        timeout_secs: u64,
        mut progress_cb: impl FnMut(&[u8]),
    ) -> anyhow::Result<RemoteCommandResult> {
        self.execute_command_inner(
            command,
            timeout_secs,
            None,
            Some(&mut progress_cb as &mut dyn FnMut(&[u8])),
            true,
        )
    }

    /// 带取消检查的 agent forwarding 远程命令执行。
    pub fn execute_command_with_agent_forward_and_cancel(
        &mut self,
        command: &str,
        timeout_secs: u64,
        cancelled: &AtomicBool,
        mut progress_cb: impl FnMut(&[u8]),
    ) -> anyhow::Result<RemoteCommandResult> {
        self.execute_command_inner(
            command,
            timeout_secs,
            Some(cancelled),
            Some(&mut progress_cb as &mut dyn FnMut(&[u8])),
            true,
        )
    }

    /// 统一的远程命令执行核心方法。
    ///
    /// - `cancelled`: 可选的取消标志，每 200ms 检查一次
    /// - `progress_cb`: 可选的 stderr 实时回调（用于 scp 进度解析）
    /// - `agent_forward`: 是否启用 SSH agent forwarding
    fn execute_command_inner(
        &mut self,
        command: &str,
        timeout_secs: u64,
        cancelled: Option<&AtomicBool>,
        mut progress_cb: Option<&mut dyn FnMut(&[u8])>,
        agent_forward: bool,
    ) -> anyhow::Result<RemoteCommandResult> {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
            let mut channel = self
                .handle
                .channel_open_session()
                .await
                .map_err(|e| anyhow!("打开 SSH 会话通道失败: {}", e))?;

            if agent_forward {
                channel
                    .agent_forward(true)
                    .await
                    .map_err(|e| anyhow!("请求 agent forwarding 失败: {}", e))?;
            }

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
                    let msg = match tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        channel.wait(),
                    )
                    .await
                    {
                        Ok(msg) => msg,
                        Err(_elapsed) => {
                            // 200ms 超时：检查取消标志
                            if let Some(c) = cancelled {
                                if c.load(Ordering::Relaxed) {
                                    let _ = channel.eof().await;
                                    let _ = channel.close().await;
                                    return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
                                }
                            }
                            continue;
                        }
                    };
                    match msg {
                        Some(russh::ChannelMsg::Data { data }) => {
                            stdout.extend_from_slice(&data);
                        }
                        Some(russh::ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                            if let Some(cb) = progress_cb.as_mut() {
                                cb(&data);
                            }
                            stderr.extend_from_slice(&data);
                        }
                        Some(russh::ChannelMsg::ExitStatus {
                            exit_status: status,
                        }) => {
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

            // 取消时已在 timeout 内关闭 channel，避免重复 close
            if !matches!(&result, Ok(Err(e)) if e.to_string().contains(TRANSFER_CANCELLED_MSG)) {
                let _ = channel.close().await;
            }

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

/// SFTP 会话缓存条目：会话 + 最后访问时间
struct CacheEntry {
    session: SharedBlockingSftpSession,
    last_access: std::time::Instant,
}

/// 空闲超时时间（秒）：超过此时间未使用的 SFTP 会话将被清理
const SFTP_SESSION_IDLE_TIMEOUT: u64 = 300; // 5 分钟

fn session_cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 清理空闲超时的 SFTP 会话
fn evict_stale_sessions() {
    let mut cache = session_cache().lock();
    let now = std::time::Instant::now();
    let stale_keys: Vec<String> = cache
        .iter()
        .filter(|(_, entry)| now.duration_since(entry.last_access).as_secs() > SFTP_SESSION_IDLE_TIMEOUT)
        .map(|(key, _)| key.clone())
        .collect();
    for key in stale_keys {
        log::debug!("Evicting stale SFTP session: {}", key);
        cache.remove(&key);
    }
}

fn cache_key(config: &SshConnectionConfig) -> String {
    // v2: inactivity_timeout disabled for long file transfers
    // Build a compact key from identifying fields instead of full JSON serialization
    let port_str = config.port.to_string();
    let capacity = 3 + config.username.len() + 1 + config.host.len() + 1 + port_str.len() + 16;
    let mut key = String::with_capacity(capacity);
    key.push_str("v2:");
    key.push_str(&config.username);
    key.push('@');
    key.push_str(&config.host);
    key.push(':');
    key.push_str(&port_str);

    // Include auth_method discriminant for cache correctness
    use crate::ssh::config::AuthMethod;
    match &config.auth_method {
        AuthMethod::Password { password } => {
            use std::fmt::Write;
            let _ = write!(key, ":pw:{}", password.len());
        }
        AuthMethod::PasswordRef { keychain_id } => {
            use std::fmt::Write;
            let _ = write!(key, ":pwr:{}", keychain_id);
        }
        AuthMethod::PrivateKey { path, .. } => {
            use std::fmt::Write;
            let _ = write!(key, ":pk:{}", path.display());
        }
        AuthMethod::PrivateKeyRef { keychain_id } => {
            use std::fmt::Write;
            let _ = write!(key, ":pkr:{}", keychain_id);
        }
        AuthMethod::Agent => {
            key.push_str(":agent");
        }
        AuthMethod::Auto => {
            key.push_str(":auto");
        }
    }
    key
}

fn get_or_connect_session(
    config: &SshConnectionConfig,
    key: &str,
) -> anyhow::Result<SharedBlockingSftpSession> {
    // 每次获取会话时清理空闲超时的会话
    evict_stale_sessions();

    if session_cache().lock().contains_key(key) {
        // 更新最后访问时间
        let mut cache = session_cache().lock();
        if let Some(entry) = cache.get_mut(key) {
            entry.last_access = std::time::Instant::now();
        }
        return Ok(cache.get(key).unwrap().session.clone());
    }

    let session = Arc::new(Mutex::new(BlockingSftpSession::connect(config)?));
    let mut cache = session_cache().lock();
    Ok(cache
        .entry(key.to_string())
        .or_insert_with(|| CacheEntry {
            session: session.clone(),
            last_access: std::time::Instant::now(),
        })
        .session
        .clone())
}

fn evict_session(key: &str, session: &SharedBlockingSftpSession) {
    let mut cache = session_cache().lock();
    if cache
        .get(key)
        .is_some_and(|entry| Arc::ptr_eq(&entry.session, session))
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

    match f(&mut session.lock()) {
        Ok(result) => Ok(result),
        Err(_) => {
            evict_session(&key, &session);
            let session = get_or_connect_session(config, &key)?;
            f(&mut session.lock())
        }
    }
}

pub fn with_cached_session_once<T>(
    config: &SshConnectionConfig,
    f: impl FnOnce(&mut BlockingSftpSession) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let key = cache_key(config);
    let session = get_or_connect_session(config, &key)?;
    f(&mut session.lock())
}

impl Drop for BlockingSftpSession {
    fn drop(&mut self) {
        let rt = crate::ssh::shared_runtime::shared_ssh_runtime();
        rt.block_on(async {
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
    // 高级 `SftpSession` 默认 response timeout 是 10 秒，长文件传输过程中偶发的
    // TCP 停顿 / 服务端忙会直接把 in-flight 请求从表里清掉，随后服务端响应
    // 到达就报 `Packet X for unknown recipient` 并把整条传输拖崩。与 bulk 通道
    // 保持一致放到 300s —— 上层 TransferManager 自己用 `cancelled: AtomicBool`
    // 做取消，不依赖这个死等时长。
    sftp.set_timeout(300);
    Ok((handle, sftp))
}

/// 在已有 SSH 连接上开一条新的 SFTP subsystem，返回底层 `RawSftpSession`。
///
/// 独立 channel 使得 pipelined bulk 传输不会占用主 `SftpSession` 的 request
/// id 空间；同时保持了 keepalive / 鉴权等连接级共享状态。
async fn open_raw_sftp_session(
    handle: &client::Handle<SftpClientHandler>,
) -> anyhow::Result<RawSftpSession> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| anyhow!("打开 bulk SFTP 通道失败: {}", e))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| anyhow!("启动 bulk SFTP 子系统失败: {}", e))?;

    let raw = RawSftpSession::new(channel.into_stream());
    // bulk 传输耗时远超默认 10s，放宽到 5 分钟；任意单个请求都不该超过这个值。
    raw.set_timeout(300);
    raw.init()
        .await
        .map_err(|e| anyhow!("初始化 bulk SFTP 会话失败: {}", e))?;
    Ok(raw)
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
        let child_trimmed = child.trim_start_matches('/');
        let mut result = String::with_capacity(1 + child_trimmed.len());
        result.push('/');
        result.push_str(child_trimmed);
        result
    } else if base.is_empty() {
        child.to_string()
    } else {
        let base_trimmed = base.trim_end_matches('/');
        let child_trimmed = child.trim_start_matches('/');
        let mut result = String::with_capacity(base_trimmed.len() + 1 + child_trimmed.len());
        result.push_str(base_trimmed);
        result.push('/');
        result.push_str(child_trimmed);
        result
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

// ============================================================================
// 方案 1/2：SFTP 请求级管道化传输
// ============================================================================

/// 管道化传输的运行期参数。
///
/// - `parallel`: 同时保持的 in-flight 请求数，越高越能掩盖 RTT，但占内存越多。
/// - `chunk`: 单次 SFTP read/write 的字节数，必须 ≤ 服务器协商的 `max_read_len`
///   / `max_write_len`（通常 256 KiB）。这里默认取 248 KiB 留少量余量。
/// - `io_timeout_secs`: 单个 SFTP 请求的响应超时。默认比 `RawSftpSession` 的
///   10s 更宽松，避免大块写入时误判。
#[derive(Clone, Copy, Debug)]
pub struct ParallelTransferConfig {
    pub parallel: usize,
    pub chunk: usize,
    pub io_timeout_secs: u64,
}

impl Default for ParallelTransferConfig {
    fn default() -> Self {
        Self {
            parallel: 16,
            chunk: 248 * 1024,
            io_timeout_secs: 300,
        }
    }
}

impl ParallelTransferConfig {
    /// 从环境变量读取覆盖值，便于无需重编译进行调试。
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("DEEPTERM_SFTP_PARALLEL") {
            if let Ok(parsed) = v.parse::<usize>() {
                if parsed >= 1 {
                    cfg.parallel = parsed.min(64);
                }
            }
        }
        if let Ok(v) = std::env::var("DEEPTERM_SFTP_CHUNK") {
            if let Ok(parsed) = v.parse::<usize>() {
                // 上限贴近 russh-sftp 的 261120，避免触发服务器限流。
                if (1024..=256 * 1024).contains(&parsed) {
                    cfg.chunk = parsed;
                }
            }
        }
        cfg
    }
}

/// 管道化跨 session 传输单文件：源读 + 目标写全部走各自 session 的 bulk channel，
/// 通过 K 个并发 read / K 个并发 write 掩盖 RTT。
///
/// `size` 为源文件的大小字节数；`progress` 累加已成功写入字节；`cancelled`
/// 被设置时立刻 bail 出 `TRANSFER_CANCELLED_MSG`（可能留下半传输的临时文件，
/// 由调用方清理）。
pub(crate) async fn pipelined_copy_between(
    source_raw: Arc<RawSftpSession>,
    source_path: &str,
    dest_raw: Arc<RawSftpSession>,
    dest_path: &str,
    size: u64,
    config: ParallelTransferConfig,
    cancelled: &AtomicBool,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<u64> {
    let src_handle = open_handle(&source_raw, source_path, OpenFlags::READ).await?;
    let dst_handle = open_handle(
        &dest_raw,
        dest_path,
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
    )
    .await?;

    let result = pipelined_copy(
        &source_raw,
        &src_handle,
        &dest_raw,
        &dst_handle,
        size,
        config,
        cancelled,
        progress,
    )
    .await;

    // 无论成功失败都尝试关闭句柄，避免泄漏
    let _ = source_raw.close(src_handle).await;
    let _ = dest_raw.close(dst_handle).await;

    result
}

async fn open_handle(raw: &RawSftpSession, path: &str, flags: OpenFlags) -> anyhow::Result<String> {
    let handle = raw
        .open(path.to_string(), flags, FileAttributes::default())
        .await
        .map_err(|e| anyhow!("打开远程文件 {} 失败: {}", path, e))?;
    Ok(handle.handle)
}

/// 管道化拷贝：读端保留最多 `parallel` 个 in-flight `read`，
/// 每读到一块数据即提交对应 offset 的 `write`。读与写在同一个 `select!`
/// 上调度，不等对方完成即可继续。
///
/// 正确性要点：每块数据携带显式 offset，服务器端即使乱序到达也按 offset 写。
/// 管道化拷贝：把读端与写端拆分为两个独立的 async 任务，
/// 通过 bounded channel 做背压。这样源端最多可保持 `parallel` 个
/// in-flight `read`、目标端最多可保持 `parallel` 个 in-flight `write`，
/// 二者并发掩盖各自链路的 RTT。
///
/// 正确性要点：每块数据携带显式 offset，写入端按 offset 调用 `write`，
/// 服务器即使乱序处理也会落到正确位置。
async fn pipelined_copy(
    src_raw: &RawSftpSession,
    src_handle: &str,
    dst_raw: &RawSftpSession,
    dst_handle: &str,
    size: u64,
    config: ParallelTransferConfig,
    cancelled: &AtomicBool,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<u64> {
    if size == 0 {
        return Ok(0);
    }

    let parallel = config.parallel.max(1);
    // channel 容量 = parallel：reader 最多领先 writer 一个窗口，
    // 避免内存峰值无上限。
    let (tx, rx) = tokio::sync::mpsc::channel::<(u64, Vec<u8>)>(parallel);

    let reader = pipelined_reader(src_raw, src_handle, size, config, cancelled, tx);
    let writer = pipelined_writer(dst_raw, dst_handle, config, cancelled, progress, rx);

    // try_join!：任一端出错立刻中止另一端（channel 被 drop 时对方感知到 EOF/Closed）。
    let (_, bytes_written) = tokio::try_join!(reader, writer)?;
    Ok(bytes_written)
}

async fn pipelined_reader(
    raw: &RawSftpSession,
    handle: &str,
    size: u64,
    config: ParallelTransferConfig,
    cancelled: &AtomicBool,
    tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
) -> anyhow::Result<()> {
    use futures::future::FutureExt;
    use futures::stream::{FuturesOrdered, StreamExt};

    let chunk = config.chunk.max(1024) as u32;
    let parallel = config.parallel.max(1);
    let mut reads: FuturesOrdered<_> = FuturesOrdered::new();
    let mut next_offset: u64 = 0;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
        }

        while reads.len() < parallel && next_offset < size {
            let remaining = size - next_offset;
            let len = (chunk as u64).min(remaining) as u32;
            let offset = next_offset;
            let h = handle.to_string();
            reads.push_back(
                async move {
                    raw.read(h, offset, len)
                        .await
                        .map(|d| (offset, d.data))
                        .map_err(|e| anyhow!("SFTP read({}) 失败: {}", offset, e))
                }
                .boxed(),
            );
            next_offset += len as u64;
        }

        match reads.next().await {
            Some(Ok((offset, data))) => {
                if data.is_empty() {
                    if offset >= size {
                        break;
                    }
                    return Err(anyhow!(
                        "源文件在 offset={} 早于预期结束 (声明大小 {})",
                        offset,
                        size
                    ));
                }
                tx.send((offset, data))
                    .await
                    .map_err(|_| anyhow!("写入端已关闭，传输终止"))?;
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }

    Ok(())
}

async fn pipelined_writer(
    raw: &RawSftpSession,
    handle: &str,
    config: ParallelTransferConfig,
    cancelled: &AtomicBool,
    progress: Option<&AtomicU64>,
    mut rx: tokio::sync::mpsc::Receiver<(u64, Vec<u8>)>,
) -> anyhow::Result<u64> {
    use futures::future::FutureExt;
    use futures::stream::{FuturesUnordered, StreamExt};

    let parallel = config.parallel.max(1);
    let mut writes: FuturesUnordered<_> = FuturesUnordered::new();
    let mut bytes_written: u64 = 0;
    let mut rx_closed = false;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
        }

        if rx_closed && writes.is_empty() {
            break;
        }

        let can_recv = !rx_closed && writes.len() < parallel;
        let has_writes = !writes.is_empty();

        if can_recv && has_writes {
            tokio::select! {
                biased;
                Some(res) = writes.next() => {
                    let n = res?;
                    bytes_written += n as u64;
                    if let Some(p) = progress {
                        p.fetch_add(n as u64, Ordering::Relaxed);
                    }
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some((offset, data)) => {
                            let n = data.len();
                            let h = handle.to_string();
                            writes.push(
                                async move {
                                    raw.write(h, offset, data)
                                        .await
                                        .map(|_| n)
                                        .map_err(|e| anyhow!("SFTP write(offset={}) 失败: {}", offset, e))
                                }
                                .boxed(),
                            );
                        }
                        None => {
                            rx_closed = true;
                        }
                    }
                }
            }
        } else if can_recv {
            // 当前没有 in-flight 写，只等待读端产出
            match rx.recv().await {
                Some((offset, data)) => {
                    let n = data.len();
                    let h = handle.to_string();
                    writes.push(
                        async move {
                            raw.write(h, offset, data)
                                .await
                                .map(|_| n)
                                .map_err(|e| anyhow!("SFTP write(offset={}) 失败: {}", offset, e))
                        }
                        .boxed(),
                    );
                }
                None => {
                    rx_closed = true;
                }
            }
        } else if has_writes {
            // 写窗口已满（或 rx 已关闭）：等一个写完成腾出空位
            if let Some(res) = writes.next().await {
                let n = res?;
                bytes_written += n as u64;
                if let Some(p) = progress {
                    p.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        } else {
            break;
        }
    }

    Ok(bytes_written)
}

const PREVIEW_ANALYSIS_BYTES: usize = 1024;

/// Detect whether a byte sequence is likely binary content.
/// Based on zed's heuristic in worktree/src/worktree.rs.
fn is_binary_content(bytes: &[u8]) -> bool {
    if bytes.len() < 2 {
        return false;
    }

    if bytes.starts_with(b"%PDF-")
        || bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xFF\xD8\xFF")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"RIFF")
        || bytes.starts_with(b"OggS")
        || bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"ID3")
        || bytes.starts_with(b"\xFF\xFB")
        || bytes.starts_with(b"\xFF\xFA")
        || bytes.starts_with(b"\xFF\xF3")
        || bytes.starts_with(b"\xFF\xF2")
    {
        return true;
    }

    let limit = bytes.len().min(PREVIEW_ANALYSIS_BYTES);
    let mut null_count = 0usize;
    let mut non_text_like_count = 0usize;

    for &byte in &bytes[..limit] {
        if byte == 0 {
            null_count += 1;
            non_text_like_count += 1;
            continue;
        }

        let is_text_like = matches!(
            byte,
            b'\t' | b'\n' | b'\r' | 0x0C
            | 0x20..=0x7E
            | 0x80..=0xBF | 0xC2..=0xF4
        );

        if !is_text_like {
            non_text_like_count += 1;
        }
    }

    if null_count > 0 && null_count * 16 >= limit {
        return true;
    }

    non_text_like_count * 100 >= limit * 8
}
