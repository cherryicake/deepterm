use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use russh::client::{self, AuthResult};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ssh::TRANSFER_CANCELLED_MSG;
use crate::file_manager::file_tree_cache::RemoteFileTreeCache;
use crate::ssh::auth::try_agent_auth;
use crate::ssh::config::SshConnectionConfig;
use crate::ssh::keychain::{
    ResolvedAuthMethod, load_certificate, load_key_material, resolve_auth_method,
};

/// 在共享 tokio runtime 上同步执行一个 SFTP 异步块。
///
/// sftp_fs 的同步 API 大量采用「`block_on(async { … })`」包装；此宏把
/// runtime 获取 + block_on 的样板收敛到一处，每个调用点只留业务逻辑。
macro_rules! sftp_block_on {
    ($body:block) => {
        crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async $body)
    };
}

/// 命令输出的实时进度回调：每收到一块 stderr 数据调用一次。
type ProgressCallback<'a> = Option<&'a mut dyn FnMut(&[u8])>;
/// 分片读取任务：成功时返回 `(分片起始偏移, 数据)`。
type ChunkReadFuture<'a> = futures::future::BoxFuture<'a, anyhow::Result<(u64, Vec<u8>)>>;

/// `execute_command*` 家族的 stdout/stderr 累积上限（64MB）。
/// 该 API 是通用入口（file_manager 用它跑 scp/du/find 等），无上限时一条
/// 失控命令可在超时（默认上限 24h）前耗尽本机内存。
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

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

/// SFTP 通道的 russh handler。
///
/// 必须做主机密钥校验：SFTP 承载全部远端文件读写，若像早期版本那样无条件
/// 接受任何密钥，这条通道对中间人攻击就是完全敞开的。
struct SftpClientHandler {
    host: String,
    port: u16,
    host_key_policy: crate::ssh::host_key::HostKeyPolicy,
}

impl client::Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // 统一走 host_key::verify_server_key，与终端通道共用逻辑（防一侧漏检）。
        crate::ssh::host_key::verify_server_key(
            &self.host,
            self.port,
            server_public_key,
            self.host_key_policy,
        )
        .await
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
        let session = crate::ssh::shared_runtime::shared_ssh_runtime()
            .block_on(open_raw_sftp_session(&self.handle))?;
        let arc = Arc::new(session);
        self.bulk = Some(arc.clone());
        Ok(arc)
    }

    pub fn canonicalize(&mut self, path: &str) -> anyhow::Result<String> {
        sftp_block_on!({
            self.sftp
                .canonicalize(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn metadata(&mut self, path: &str) -> anyhow::Result<RemoteFileMetadata> {
        sftp_block_on!({
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
        sftp_block_on!({
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
        sftp_block_on!({
            let metadata = self.sftp.metadata(path).await?;
            Ok(metadata.permissions)
        })
    }

    pub fn set_permissions(&mut self, path: &str, mode: u32) -> anyhow::Result<()> {
        sftp_block_on!({
            let mut attrs = FileAttributes::empty();
            attrs.permissions = Some(mode);
            self.sftp.set_metadata(path, attrs).await?;
            Ok(())
        })
    }

    pub fn exists(&mut self, path: &str) -> anyhow::Result<bool> {
        sftp_block_on!({
            self.sftp
                .try_exists(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    /// 创建单层目录（非递归）：父目录必须已存在，目标已存在时由服务器返回错误。
    /// 与 `std::fs::create_dir` 语义对齐，供「新建文件夹」这类不应隐式补父链的场景使用。
    pub fn create_dir(&mut self, path: &str) -> anyhow::Result<()> {
        sftp_block_on!({
            self.sftp
                .create_dir(path)
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

            sftp_block_on!({ self.sftp.create_dir(current.clone()).await })?;
        }

        Ok(())
    }

    pub fn rename(&mut self, from: &str, to: &str) -> anyhow::Result<()> {
        sftp_block_on!({
            self.sftp
                .rename(from, to)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn remove_file(&mut self, path: &str) -> anyhow::Result<()> {
        sftp_block_on!({
            self.sftp
                .remove_file(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn remove_dir(&mut self, path: &str) -> anyhow::Result<()> {
        sftp_block_on!({
            self.sftp
                .remove_dir(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn upload_file_from_local_with_cancel_and_progress(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        cancelled: &AtomicBool,
        progress: Option<&std::sync::atomic::AtomicU64>,
    ) -> anyhow::Result<u64> {
        let mut local_file = File::open(local_path)?;
        sftp_block_on!({
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

    /// 上传文件并在**创建时**即设定远程权限（mode）。
    ///
    /// 默认上传路径是 "create → 写内容 → chmod"，中间存在一个按远端 umask
    /// 创建的窗口期，私钥等敏感文件在该窗口内可能对同机其他用户可读。
    /// SFTP 的 SSH_FXP_OPEN 支持携带初始属性，直接给出 0600 即可消除窗口。
    pub fn upload_file_from_local_with_permissions(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        mode: u32,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<u64> {
        let mut local_file = File::open(local_path)?;
        sftp_block_on!({
            let attrs = FileAttributes {
                permissions: Some(mode),
                ..FileAttributes::empty()
            };
            let mut remote_file = self
                .sftp
                .open_with_flags_and_attributes(
                    remote_path,
                    OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
                    attrs,
                )
                .await?;
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

    pub fn download_file_to_local_with_cancel_and_progress(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        cancelled: &AtomicBool,
        progress: Option<&std::sync::atomic::AtomicU64>,
    ) -> anyhow::Result<u64> {
        let mut local_file = File::create(local_path)?;
        sftp_block_on!({
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
        // 临时文件名带 pid+纳秒：多个并发任务复制到同一目标时不会
        // 互相删除/覆盖对方的 .deepterm-part 文件。
        let temporary_path = format!(
            "{destination_path}.deepterm-part.{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        if self.exists(&temporary_path)? {
            let _ = self.remove_file(&temporary_path);
        }

        let bulk = self.bulk_session()?;
        let config = ParallelTransferConfig::from_env();

        let transferred =
            crate::ssh::shared_runtime::shared_ssh_runtime().block_on(pipelined_copy_between(
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
        sftp_block_on!({
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

    /// 读取完整远程文件内容，若检测到二进制内容则返回 None。
    pub fn read_file_to_string(&mut self, path: &str) -> anyhow::Result<Option<String>> {
        sftp_block_on!({
            let mut file = self.sftp.open(path).await?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).await?;

            if bytes.is_empty() {
                return Ok(Some(String::new()));
            }

            if is_binary_bytes(&bytes) {
                return Ok(None);
            }

            Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
        })
    }

    /// 读取完整远程文件原始字节（小文件快路径）。
    pub fn read_file_bytes(&mut self, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        sftp_block_on!({
            let mut file = self.sftp.open(path).await?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).await?;
            Ok(Some(bytes))
        })
    }

    /// 有界预览读取（最多 `limit` 字节），用于文件树悬停预取等场景。
    /// 关键正确性（中文）：必须用 `take(limit)` 有界读，而非“先 stat 预检 +
    /// 再全量读”。远端文件可能在两次 RPC 之间增大（持续写入的日志），
    /// 全量读 + 事后 `truncate` 仍会把 GB 级内容载入内存（TOCTOU）。
    /// `take` 保证内存占用恒 `<= limit`。
    pub fn read_preview_bytes(
        &mut self,
        path: &str,
        limit: usize,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        sftp_block_on!({
            let file = self.sftp.open(path).await?;
            let mut bytes = Vec::new();
            file.take(limit as u64).read_to_end(&mut bytes).await?;
            Ok((!bytes.is_empty()).then_some(bytes))
        })
    }

    /// 大文件流水线读取（通过 bulk RawSftpSession 并行 16×248K）
    pub fn read_file_bytes_pipelined(
        &mut self,
        path: &str,
        size: u64,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        if size == 0 {
            return Ok(Some(Vec::new()));
        }
        let raw = self.bulk_session()?;
        let config = ParallelTransferConfig::from_env();
        let bytes = crate::ssh::shared_runtime::shared_ssh_runtime()
            .block_on(pipelined_read_to_vec(raw, path, size, config))?;
        Ok(Some(bytes))
    }

    /// 发送轻量级 SFTP ping（stat "."），供会话缓存健康检查使用。
    pub fn ping(&mut self) -> anyhow::Result<()> {
        sftp_block_on!({
            self.sftp.metadata(".").await?;
            Ok(())
        })
    }

    /// 将字符串内容整体写入远程文件（覆盖已存在的文件）。
    pub fn write_file_string(&mut self, path: &str, content: &str) -> anyhow::Result<()> {
        sftp_block_on!({
            let mut file = self.sftp.create(path).await?;
            file.write_all(content.as_bytes()).await?;
            file.flush().await?;
            file.sync_all().await?;
            file.shutdown().await?;
            Ok(())
        })
    }

    /// 将原始字节整体写入远程文件（覆盖已存在的文件）。
    /// 用于按非 UTF-8 编码（GBK / UTF-16 等）保存编辑器内容。
    pub fn write_file_bytes(&mut self, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        sftp_block_on!({
            let mut file = self.sftp.create(path).await?;
            file.write_all(bytes).await?;
            file.flush().await?;
            file.sync_all().await?;
            file.shutdown().await?;
            Ok(())
        })
    }

    /// 原子写远程文件：先写同目录临时文件再 rename，避免崩溃产生半截文件。
    pub fn write_file_bytes_atomic(&mut self, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let tmp = {
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!("{path}.tmp.{pid}-{nanos}")
        };

        let write_ok = sftp_block_on!({
            let mut file = self.sftp.create(tmp.clone()).await?;
            file.write_all(bytes).await?;
            file.flush().await?;
            file.sync_all().await?;
            file.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        });

        if let Err(e) = write_ok {
            let _ = self.remove_file(&tmp);
            return Err(e);
        }

        let rename_result = self.rename(&tmp, path);
        if rename_result.is_ok() {
            return Ok(());
        }
        let first_err = rename_result.unwrap_err();

        // rename 失败有两类原因，处理策略不同：
        // 1) 会话失活（连接被断）：立即放弃，保留 tmp —— 此时删除目标文件只会
        //    造成数据丢失（删除和重试大概率也失败）。
        // 2) 应用级失败：部分服务端（SFTPv3 语义，如 OpenSSH 默认配置）在目标
        //    已存在时拒绝 rename 覆盖 —— 需要走「备份原文件 → 删目标 → 重命名」。
        //    任何一步失败都保留备份 / tmp，保证至少存在一份可恢复的数据。
        if self.ping().is_err() {
            anyhow::bail!(
                "写入 {path} 失败（会话已断开）: {first_err}；新内容已保留在临时文件 {tmp}"
            );
        }
        if !self.exists(path)? {
            // 目标不存在却 rename 失败：可能是瞬时错误，直接重试一次。
            if self.rename(&tmp, path).is_ok() {
                return Ok(());
            }
            anyhow::bail!(
                "写入 {path} 失败: {first_err}；新内容已保留在临时文件 {tmp}"
            );
        }

        // 目标已存在（覆盖保存）。先把原文件复制为同目录备份，再删目标、重命名；
        // 重命名失败时用备份恢复原文件。绝不进入「原文件已删且新内容也丢」的状态。
        let bak = format!(
            "{path}.deepterm-bak.{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let copied = self.copy_remote_file_streamed(path, &bak);
        if let Err(e) = copied {
            anyhow::bail!(
                "写入 {path} 失败: {first_err}；覆盖前备份原文件也失败: {e}；已中止写入，新内容保留在 {tmp}"
            );
        }

        // 备份就绪：删除旧目标并落位新内容。
        let remove_result = self.remove_file(path);
        if let Err(e) = remove_result {
            let _ = self.remove_file(&bak);
            anyhow::bail!(
                "写入 {path} 失败: {first_err}；删除旧文件失败: {e}；新内容保留在 {tmp}"
            );
        }
        match self.rename(&tmp, path) {
            Ok(()) => {
                let _ = self.remove_file(&bak);
                Ok(())
            }
            Err(e) => {
                // 落位失败：恢复原文件；tmp（新内容）与 bak（旧内容）都保留，
                // 用户可手动恢复任意一份。
                let restore = self.rename(&bak, path);
                if let Err(restore_err) = restore {
                    anyhow::bail!(
                        "写入 {path} 失败: {first_err}; retry: {e}；恢复原文件也失败: {restore_err}；\
                         旧内容保留在 {bak}，新内容保留在 {tmp}"
                    );
                }
                anyhow::bail!(
                    "写入 {path} 失败: {first_err}; retry: {e}；已恢复原文件，新内容保留在 {tmp}"
                );
            }
        }
    }

    /// 同目录内的流式复制（256KB 分块），供原子写的覆盖前备份使用。
    /// 全程不把文件完整读入内存，大文件也只占固定缓冲。
    fn copy_remote_file_streamed(&mut self, src: &str, dst: &str) -> anyhow::Result<u64> {
        sftp_block_on!({
            let mut input = self.sftp.open(src).await?;
            let mut output = self.sftp.create(dst.to_string()).await?;
            let mut buffer = vec![0u8; Self::TRANSFER_BUFFER_SIZE];
            let mut total: u64 = 0;
            loop {
                let n = input.read(&mut buffer).await?;
                if n == 0 {
                    break;
                }
                output.write_all(&buffer[..n]).await?;
                total += n as u64;
            }
            output.flush().await?;
            output.sync_all().await?;
            output.shutdown().await?;
            Ok::<u64, anyhow::Error>(total)
        })
    }

    /// 在远程服务器上执行 shell 命令，返回 stdout/stderr 和退出码。
    /// 用于 Phase 3 服务器直连场景，在源服务器执行 scp/rsync 命令。
    ///
    /// stdout 按有损 UTF-8 解码。需要原始字节的调用方（git 输出等）请用
    /// [`Self::execute_command_bytes`]。
    pub fn execute_command(
        &mut self,
        command: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<RemoteCommandResult> {
        let raw = self.execute_command_inner(
            command,
            timeout_secs,
            None,
            None::<&mut dyn FnMut(&[u8])>,
            false,
        )?;
        Ok(raw.into_lossy())
    }

    /// 带取消检查的远程命令执行。每 200ms 检查一次 `cancelled`，
    /// 为 true 时关闭 SSH channel 并返回错误。
    pub fn execute_command_with_cancel(
        &mut self,
        command: &str,
        timeout_secs: u64,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<RemoteCommandResult> {
        let raw = self.execute_command_inner(
            command,
            timeout_secs,
            Some(cancelled),
            None::<&mut dyn FnMut(&[u8])>,
            false,
        )?;
        Ok(raw.into_lossy())
    }

    /// 带取消检查的 agent forwarding 远程命令执行。
    pub fn execute_command_with_agent_forward_and_cancel(
        &mut self,
        command: &str,
        timeout_secs: u64,
        cancelled: &AtomicBool,
        mut progress_cb: impl FnMut(&[u8]),
    ) -> anyhow::Result<RemoteCommandResult> {
        let raw = self.execute_command_inner(
            command,
            timeout_secs,
            Some(cancelled),
            Some(&mut progress_cb as &mut dyn FnMut(&[u8])),
            true,
        )?;
        Ok(raw.into_lossy())
    }

    /// stdout 保留原始字节的远程命令执行。
    ///
    /// 供需要按内容编码探测解码 stdout 的调用方使用（git show 的仓库
    /// blob、`git status -z` 的非 UTF-8 文件名等），避免 `from_utf8_lossy`
    /// 把 GBK 内容破坏成 U+FFFD 后无法还原。stderr 仅作错误展示，
    /// 固定有损解码。
    pub fn execute_command_bytes(
        &mut self,
        command: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<RemoteCommandRawResult> {
        self.execute_command_inner(command, timeout_secs, None, None, false)
    }

    /// 统一的远程命令执行核心方法。
    ///
    /// - `cancelled`: 可选的取消标志，每 200ms 检查一次
    /// - `progress_cb`: 可选的 stderr 实时回调（用于 scp 进度解析）
    /// - `agent_forward`: 是否启用 SSH agent forwarding
    ///
    /// 返回**原始字节**：stdout 的解码策略由调用方决定（git 输出需要按
    /// 文本编码探测走 `decode_bytes`，而不是一刀切 `from_utf8_lossy`）。
    fn execute_command_inner(
        &mut self,
        command: &str,
        timeout_secs: u64,
        cancelled: Option<&AtomicBool>,
        mut progress_cb: ProgressCallback<'_>,
        agent_forward: bool,
    ) -> anyhow::Result<RemoteCommandRawResult> {
        sftp_block_on!({
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
                            // 输出上限：execute_command 是通用入口（file_manager
                            // 用它跑 scp/du/find 等），一条失控命令（误把
                            // `cat /dev/zero` 之类当作命令）不能在超时前耗尽内存。
                            if stdout.len() + data.len() > MAX_COMMAND_OUTPUT_BYTES {
                                let _ = channel.eof().await;
                                let _ = channel.close().await;
                                return Err(anyhow!(
                                    "命令输出超过上限 {} 字节（可能不是预期命令）",
                                    MAX_COMMAND_OUTPUT_BYTES
                                ));
                            }
                            stdout.extend_from_slice(&data);
                        }
                        // `ext == 1` 即 stderr，直接写成字面量模式，无需 guard。
                        Some(russh::ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            if let Some(cb) = progress_cb.as_mut() {
                                cb(&data);
                            }
                            if stderr.len() + data.len() > MAX_COMMAND_OUTPUT_BYTES {
                                let _ = channel.eof().await;
                                let _ = channel.close().await;
                                return Err(anyhow!(
                                    "命令 stderr 超过上限 {} 字节",
                                    MAX_COMMAND_OUTPUT_BYTES
                                ));
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
                Ok(Ok(())) => Ok(RemoteCommandRawResult {
                    stdout,
                    // stderr 仅作错误展示，固定有损解码。
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                    exit_status,
                }),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow!("远程命令执行超时 ({}s)", timeout_secs)),
            }
        })
    }
}

/// 远程命令执行结果（原始字节）。
///
/// stderr 固定有损解码为 String（仅作错误展示）；stdout 保留字节。
#[derive(Clone, Debug)]
pub struct RemoteCommandRawResult {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub exit_status: Option<u32>,
}

impl RemoteCommandRawResult {
    pub fn success(&self) -> bool {
        self.exit_status == Some(0)
    }

    /// stdout 有损解码为文本（既有通用行为）。
    pub fn into_lossy(self) -> RemoteCommandResult {
        RemoteCommandResult {
            stdout: String::from_utf8_lossy(&self.stdout).into_owned(),
            stderr: self.stderr,
            exit_status: self.exit_status,
        }
    }
}

/// 远程命令执行结果（文本）。
///
/// stdout 按有损 UTF-8 解码。**注意**：需要按内容编码探测解码的调用方
/// （如 git show 的仓库 blob）应使用 `execute_command_bytes`，否则 GBK
/// 等非 UTF-8 内容会整篇变成 U+FFFD。
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

/// SFTP 会话缓存条目：空闲会话池 + 最后访问时间。
///
/// 会话按「取用-归还」周转：正在使用的会话不在池中（并行传输各自独占一条连接），
/// 归还时若池未满则放回复用，否则直接丢弃（`Drop` 时干净断开）。
struct CacheEntry {
    idle: Vec<SharedBlockingSftpSession>,
    last_access: std::time::Instant,
}

/// 每个主机最多保留的空闲会话数。并行传输期间会临时创建更多连接，用完即弃，
/// 避免池无限增长；UI 的读目录 / 统计等高频小操作始终能复用池中会话。
/// P0 优化：2 -> 8，允许目录并行展开 / 轮询 / 文件读取并发执行，避免单会话排队。
const MAX_IDLE_SESSIONS_PER_HOST: usize = 8;

/// 空闲超时时间（秒）：超过此时间未使用的 SFTP 会话将被清理
const SFTP_SESSION_IDLE_TIMEOUT: u64 = 1800; // 30 分钟（原 5 分钟，通过心跳减少断连）

/// 空闲超过此时间后，在清理前先尝试 ping 保活
const SFTP_SESSION_PING_THRESHOLD: u64 = 600; // 10 分钟

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
        .filter(|(_, entry)| {
            now.duration_since(entry.last_access).as_secs() > SFTP_SESSION_IDLE_TIMEOUT
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in stale_keys {
        log::debug!("Evicting stale SFTP session: {}", key);
        cache.remove(&key);
    }
}

/// 判断会话空闲状态：是否需要 ping、是否已过期。
enum IdleCheck {
    Fresh,
    NeedsPing,
    Expired,
}

fn idle_check(entry: &CacheEntry) -> IdleCheck {
    let idle = std::time::Instant::now()
        .duration_since(entry.last_access)
        .as_secs();
    if idle < SFTP_SESSION_PING_THRESHOLD {
        IdleCheck::Fresh
    } else if idle >= SFTP_SESSION_IDLE_TIMEOUT {
        IdleCheck::Expired
    } else {
        IdleCheck::NeedsPing
    }
}

/// FNV-1a 64 位哈希：用于 cache_key 中区分密码内容而不驻留明文。
/// 非加密用途（仅需区分不同输入），无需加密强度。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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
            // 用内容哈希而非明文或长度：长度无法区分同长度不同密码（改密后命中旧
            // 会话）；明文会随 key 长期驻留 HashMap。FNV-1a 单向、无碰撞性对本
            // 用途（进程内缓存区分）足够。
            use std::fmt::Write;
            let _ = write!(key, ":pw:{:016x}", fnv1a64(password.as_bytes()));
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

/// 取出一条空闲会话供独占使用；无空闲时新建连接。
///
/// 注意：新连接**不会**放入缓存 —— 调用方正在使用它；使用完毕后必须调用
/// [`checkin_session`] 归还，否则该连接会被直接丢弃（`Drop` 时干净断开）。
fn get_or_connect_session(
    config: &SshConnectionConfig,
    key: &str,
) -> anyhow::Result<SharedBlockingSftpSession> {
    // 每次获取会话时清理空闲超时的会话
    evict_stale_sessions();

    // 在持锁期间只做轻量判断；网络 ping 放到锁外，避免阻塞其他 SFTP 访问。
    // 有空闲会话直接取走（独占）。
    {
        let mut cache = session_cache().lock();
        if let Some(entry) = cache.get_mut(key) {
            if let Some(session) = entry.idle.pop() {
                entry.last_access = std::time::Instant::now();
                return Ok(session);
            }
        }
    }

    // 没有空闲会话：锁外 ping 保活检查（决定保留还是重建缓存条目）。
    let session_to_ping: Option<SharedBlockingSftpSession> = {
        let mut cache = session_cache().lock();
        match cache.get_mut(key) {
            Some(entry) => match idle_check(entry) {
                IdleCheck::Fresh => None,
                IdleCheck::NeedsPing => entry.idle.last().cloned(),
                IdleCheck::Expired => {
                    cache.remove(key);
                    None
                }
            },
            None => None,
        }
    };

    if let Some(session) = session_to_ping {
        let ping_ok = session.lock().ping().is_ok();
        let mut cache = session_cache().lock();
        if let Some(entry) = cache.get_mut(key) {
            if ping_ok {
                entry.last_access = std::time::Instant::now();
                log::debug!("Ping kept session alive: {}", key);
            } else {
                log::debug!("Ping failed, session stale: {}", key);
                entry.idle.retain(|s| !Arc::ptr_eq(s, &session));
                if entry.idle.is_empty() {
                    cache.remove(key);
                }
            }
        }
    }

    // 新建连接：直接交给调用方独占使用（不入池）。
    Ok(Arc::new(Mutex::new(BlockingSftpSession::connect(config)?)))
}

/// 归还使用完毕的会话：池未满则放回复用，否则直接丢弃（`Drop` 时干净断开）。
///
/// 条目不存在时（首次归还、或此前被过期清理过）必须先建立条目，否则会话永远
/// 无法入池 —— 每次操作都会退化成新建一条 SSH 连接。
fn checkin_session(key: &str, session: SharedBlockingSftpSession) {
    let mut cache = session_cache().lock();
    let now = std::time::Instant::now();
    match cache.get_mut(key) {
        Some(entry) => {
            entry.last_access = now;
            if entry.idle.len() < MAX_IDLE_SESSIONS_PER_HOST {
                entry.idle.push(session);
            }
            // 池已满：session 在此 drop 并干净断开。
        }
        None => {
            cache.insert(
                key.to_string(),
                CacheEntry {
                    idle: vec![session],
                    last_access: now,
                },
            );
        }
    }
}

/// 移除指定会话：用于传输失败后剔除疑似损坏的连接（`Drop` 时干净断开）。
/// 若条目已空则删除整条缓存。
fn evict_session(key: &str, session: &SharedBlockingSftpSession) {
    let mut cache = session_cache().lock();
    if let Some(entry) = cache.get_mut(key) {
        entry.idle.retain(|s| !Arc::ptr_eq(s, session));
        if entry.idle.is_empty() {
            cache.remove(key);
        }
    }
}

pub fn with_cached_session<T>(
    config: &SshConnectionConfig,
    mut f: impl FnMut(&mut BlockingSftpSession) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let key = cache_key(config);
    let mut session = get_or_connect_session(config, &key)?;

    let first = f(&mut session.lock());
    let result = match first {
        Ok(result) => Ok(result),
        Err(err) => {
            // 仅当会话确实失活（连接被服务端/网络中断）时才重连重跑一次；
            // 会话仍然存活说明是应用级错误（用户取消、路径校验失败、远端
            // 返回的业务错误等），直接返回，避免非幂等操作（rm/scp/覆盖写）
            // 被执行两次，也避免取消传输触发一次无意义的重连。
            if session.lock().ping().is_ok() {
                log::debug!(
                    "with_cached_session: 会话存活，按应用错误返回（不重试）: {err:#}"
                );
                checkin_session(&key, session);
                return Err(err);
            }
            // 会话失活：剔除并重连一次。注意必须复用同一个绑定，最终归还的
            // 是重连后的会话，而不是这个已被剔除的坏会话。
            log::debug!("with_cached_session: 会话失活，重连重试: {err:#}");
            evict_session(&key, &session);
            session = get_or_connect_session(config, &key)?;
            f(&mut session.lock())
        }
    };
    checkin_session(&key, session);
    result
}

pub fn with_cached_session_once<T>(
    config: &SshConnectionConfig,
    f: impl FnOnce(&mut BlockingSftpSession) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let key = cache_key(config);
    let session = get_or_connect_session(config, &key)?;
    let result = f(&mut session.lock());
    checkin_session(&key, session);
    result
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

/// 将字符串内容整体写入远程文件（覆盖已存在的文件）。
pub fn write_file_string(
    config: &SshConnectionConfig,
    path: &str,
    content: &str,
) -> anyhow::Result<()> {
    with_cached_session(config, |session| session.write_file_string(path, content))
}

/// 原子写远程文件（见 `BlockingSftpSession::write_file_bytes_atomic`）
pub fn write_file_bytes_atomic(
    config: &SshConnectionConfig,
    path: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    with_cached_session(config, |session| {
        session.write_file_bytes_atomic(path, bytes)
    })
}

const LARGE_FILE_THRESHOLD: u64 = 512 * 1024;

fn cached_file_size(config: &SshConnectionConfig, path: &str) -> Option<u64> {
    let parent = Path::new(path).parent()?;
    let entries = RemoteFileTreeCache::global().lock().get(config, parent)?;
    let file_name = Path::new(path).file_name()?.to_string_lossy();
    entries
        .into_iter()
        .find(|e| e.name == file_name)
        .map(|e| e.size)
}

pub fn read_file_bytes(
    config: &SshConnectionConfig,
    path: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    if let Some(size) = cached_file_size(config, path).filter(|s| *s > LARGE_FILE_THRESHOLD) {
        return with_cached_session(config, |session| {
            session.read_file_bytes_pipelined(path, size)
        });
    }
    with_cached_session(config, |session| session.read_file_bytes(path))
}

/// 有界预览读取（最多 `limit` 字节），供文件树悬停预取等场景使用。
/// 底层走 `take(limit)`，内存占用恒有界，不受文件后续增大影响（无 TOCTOU）。
pub fn read_preview_bytes(
    config: &SshConnectionConfig,
    path: &str,
    limit: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    with_cached_session(config, |session| session.read_preview_bytes(path, limit))
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

    // config.timeout 生效：TCP+SSH 握手整体限时（此前从未被使用）。
    let mut handle = tokio::time::timeout(
        std::time::Duration::from_secs(config.timeout.max(1)),
        client::connect(
            russh_config,
            (config.host.as_str(), config.port),
            SftpClientHandler {
                host: config.host.clone(),
                port: config.port,
                host_key_policy: crate::ssh::host_key::policy_from_global_config(),
            },
        ),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "连接超时（{} 秒内未完成与 {}:{} 的 TCP/SSH 握手）",
            config.timeout,
            config.host,
            config.port
        )
    })?
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
            .authenticate_password(&config.username, password.as_str())
            .await
            .map_err(|e| anyhow!("密码认证失败: {}", e)),
        ResolvedAuthMethod::PrivateKey {
            path,
            passphrase,
            certificate_path,
            ..
        } => {
            let key = load_key_material(path, passphrase.as_deref().map(|p| p.as_str()))?;
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
        // Auto 语义与终端通道一致：agent → 默认密钥（此前只试默认密钥）。
        ResolvedAuthMethod::Auto => try_agent_auth(handle, config).await,
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

async fn pipelined_read_to_vec(
    raw: Arc<RawSftpSession>,
    path: &str,
    size: u64,
    config: ParallelTransferConfig,
) -> anyhow::Result<Vec<u8>> {
    if size == 0 {
        return Ok(Vec::new());
    }
    let handle = open_handle(&raw, path, OpenFlags::READ).await?;
    let mut buffer = vec![0u8; size as usize];
    let chunk = config.chunk.max(1024) as u32;
    let parallel = config.parallel.max(1);
    use futures::future::FutureExt;
    use futures::stream::{FuturesOrdered, StreamExt};

    let mut reads: FuturesOrdered<ChunkReadFuture<'_>> = FuturesOrdered::new();
    let mut next_offset = 0u64;

    let make_read = |raw: Arc<RawSftpSession>, handle: String, offset: u64, len: u32| {
        async move {
            raw.read(handle, offset, len)
                .await
                .map(|d| (offset, d.data))
                .map_err(|e| anyhow!("SFTP read({offset}) 失败: {e}"))
        }
        .boxed()
    };

    while reads.len() < parallel && next_offset < size {
        let len = (size - next_offset).min(chunk as u64) as u32;
        reads.push_back(make_read(raw.clone(), handle.clone(), next_offset, len));
        next_offset += len as u64;
    }

    while let Some((offset, data)) = reads.next().await.transpose()? {
        if data.is_empty() && (offset as usize) < buffer.len() {
            return Err(anyhow!(
                "远程文件在 offset={offset} 提前结束 (预期大小 {size})"
            ));
        }
        let end = offset as usize + data.len();
        if end > buffer.len() {
            buffer.resize(end, 0);
        }
        buffer[offset as usize..end].copy_from_slice(&data);
        if next_offset < size {
            let len = (size - next_offset).min(chunk as u64) as u32;
            reads.push_back(make_read(raw.clone(), handle.clone(), next_offset, len));
            next_offset += len as u64;
        }
    }

    let _ = raw.close(handle).await;
    Ok(buffer)
}

const PREVIEW_ANALYSIS_BYTES: usize = 1024;

/// 判断字节内容是否为二进制（供编辑器打开文件前的拦截）。
pub fn is_binary_bytes(bytes: &[u8]) -> bool {
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
