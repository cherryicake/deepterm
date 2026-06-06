use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use anyhow::{Context as _, anyhow};

use crate::file_transfer::utils::{
    TransferSummary, check_cancelled, remote_file_name, resolve_local_conflict,
    resolve_remote_conflict, temporary_local_path, temporary_remote_path,
};
use crate::file_transfer::{ConflictPolicy, ConflictResolver};
use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::{self, RemoteFileKind};

pub fn upload_local_to_remote(
    source_path: &Path,
    ssh_config: &SshConnectionConfig,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    rename_to: Option<&str>,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;
    let metadata = fs::metadata(source_path)
        .with_context(|| format!("无法检查源文件 {}", source_path.display()))?;
    let file_name = super::utils::local_file_name(source_path)?;
    let effective_name = rename_to.unwrap_or(&file_name);

    sftp_fs::with_cached_session(ssh_config, |session| {
        session.create_dir_all(destination_dir)?;

        if metadata.is_file() {
            let desired_path = sftp_fs::join_remote_path(destination_dir, effective_name);
            let destination_path = match resolve_remote_conflict(
                session,
                &desired_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => path,
                None => return Ok(TransferSummary::one_skipped()),
            };
            return upload_local_file_to_remote_path(
                session,
                source_path,
                &destination_path,
                cancelled,
                progress,
            );
        }

        if metadata.is_dir() {
            let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);
            let destination_root = match resolve_remote_conflict(
                session,
                &desired_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => path,
                None => return Ok(TransferSummary::one_skipped()),
            };
            session.create_dir_all(&destination_root)?;
            return upload_local_dir_to_remote_path(
                session,
                source_path,
                &destination_root,
                conflict_policy,
                cancelled,
                progress,
                resolver,
            );
        }

        // 符号链接 / 特殊文件：计为跳过，避免 Move 误删源。
        log::warn!("跳过不支持的本地文件类型: {}", source_path.display());
        Ok(TransferSummary::one_skipped())
    })
}

pub fn upload_local_dir_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    source_dir: &Path,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    session.create_dir_all(destination_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(source_dir) {
            let mode = metadata.permissions().mode();
            let _ = session.set_permissions(destination_dir, mode);
        }
    }

    let mut summary = TransferSummary::default();

    for entry in fs::read_dir(source_dir)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        let source_path = entry.path();
        let metadata = entry.metadata()?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        let destination_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        if metadata.is_file() {
            match resolve_remote_conflict(
                session,
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => summary.merge(upload_local_file_to_remote_path(
                    session,
                    &source_path,
                    &path,
                    cancelled,
                    progress,
                )?),
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else if metadata.is_dir() {
            match resolve_remote_conflict(
                session,
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => {
                    session.create_dir_all(&path)?;
                    summary.merge(upload_local_dir_to_remote_path(
                        session,
                        &source_path,
                        &path,
                        conflict_policy,
                        cancelled,
                        progress,
                        resolver,
                    )?);
                }
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else {
            // 符号链接 / 特殊文件：计为跳过，避免 Move 误删源。
            log::warn!("跳过不支持的本地条目: {}", source_path.display());
            summary.merge(TransferSummary::one_skipped());
        }
    }

    Ok(summary)
}

pub fn upload_local_file_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    source_path: &Path,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    #[cfg(unix)]
    let source_permissions = fs::metadata(source_path)
        .ok()
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()));

    let temporary_path = temporary_remote_path(destination_path);
    if session.exists(&temporary_path)? {
        let _ = session.remove_file(&temporary_path);
    }
    let result = session.upload_file_from_local_with_cancel_and_progress(
        source_path,
        &temporary_path,
        cancelled,
        progress,
    );
    if let Err(error) = &result {
        let _ = session.remove_file(&temporary_path);
        return Err(anyhow!("上传失败: {}", error));
    }

    let transferred = result?;
    if let Err(error) = session.rename(&temporary_path, destination_path) {
        let _ = session.remove_file(&temporary_path);
        return Err(error);
    }
    #[cfg(unix)]
    if let Some(mode) = source_permissions {
        let _ = session.set_permissions(destination_path, mode);
    }
    Ok(TransferSummary::one_file(transferred))
}

pub fn download_remote_to_local(
    ssh_config: &SshConnectionConfig,
    remote_path: &str,
    destination_dir: &Path,
    conflict_policy: ConflictPolicy,
    rename_to: Option<&str>,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;
    fs::create_dir_all(destination_dir)?;
    let file_name = remote_file_name(remote_path)?;
    let effective_name = rename_to.unwrap_or(&file_name);

    sftp_fs::with_cached_session(ssh_config, |session| {
        let metadata = session.metadata(remote_path)?;

        if metadata.kind == RemoteFileKind::File {
            let desired_path = destination_dir.join(effective_name);
            let destination_path = match resolve_local_conflict(
                &desired_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => path,
                None => return Ok(TransferSummary::one_skipped()),
            };
            return download_remote_file_to_local_path(
                session,
                remote_path,
                &destination_path,
                cancelled,
                progress,
            );
        }

        if metadata.kind == RemoteFileKind::Directory {
            let desired_path = destination_dir.join(&file_name);
            let destination_root = match resolve_local_conflict(
                &desired_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => path,
                None => return Ok(TransferSummary::one_skipped()),
            };
            fs::create_dir_all(&destination_root)?;
            return download_remote_dir_to_local_path(
                session,
                remote_path,
                &destination_root,
                conflict_policy,
                cancelled,
                progress,
                resolver,
            );
        }

        // 符号链接 / 设备文件等不支持的类型：计为跳过而非静默丢弃，避免 Move 误删源。
        log::warn!(
            "跳过不支持的远程文件类型 ({:?}): {}",
            metadata.kind,
            remote_path
        );
        Ok(TransferSummary::one_skipped())
    })
}

pub fn download_remote_dir_to_local_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_dir: &str,
    destination_dir: &Path,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    fs::create_dir_all(destination_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(Some(mode)) = session.permissions(remote_dir) {
            let _ = fs::set_permissions(destination_dir, fs::Permissions::from_mode(mode));
        }
    }

    let mut summary = TransferSummary::default();

    for entry in session.read_dir(remote_dir)? {
        check_cancelled(cancelled)?;
        let destination_path = destination_dir.join(&entry.name);

        if entry.kind == RemoteFileKind::File {
            match resolve_local_conflict(&destination_path, conflict_policy, resolver, cancelled)? {
                Some(path) => summary.merge(download_remote_file_to_local_path(
                    session,
                    &entry.path,
                    &path,
                    cancelled,
                    progress,
                )?),
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else if entry.kind == RemoteFileKind::Directory {
            match resolve_local_conflict(&destination_path, conflict_policy, resolver, cancelled)? {
                Some(path) => {
                    fs::create_dir_all(&path)?;
                    summary.merge(download_remote_dir_to_local_path(
                        session,
                        &entry.path,
                        &path,
                        conflict_policy,
                        cancelled,
                        progress,
                        resolver,
                    )?);
                }
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else {
            // 符号链接 / 设备文件等：计为跳过，保证 Move 不会因"看似全部成功"而删源。
            log::warn!("跳过不支持的远程条目 ({:?}): {}", entry.kind, entry.path);
            summary.merge(TransferSummary::one_skipped());
        }
    }

    Ok(summary)
}

pub fn download_remote_file_to_local_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_path: &Path,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    let remote_permissions = session.permissions(remote_path).ok().flatten();

    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary_path = temporary_local_path(destination_path);
    if temporary_path.exists() {
        let _ = fs::remove_file(&temporary_path);
    }
    let result = session.download_file_to_local_with_cancel_and_progress(
        remote_path,
        &temporary_path,
        cancelled,
        progress,
    );
    if let Err(error) = &result {
        let _ = fs::remove_file(&temporary_path);
        return Err(anyhow!("下载失败: {}", error));
    }

    let transferred = result?;
    if let Err(error) = fs::rename(&temporary_path, destination_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = remote_permissions {
            let _ = fs::set_permissions(destination_path, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(TransferSummary::one_file(transferred))
}
