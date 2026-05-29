use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use anyhow::{anyhow, bail};

use crate::file_transfer::utils::{
    check_cancelled, is_remote_noop_move, prevent_remote_recursive_copy,
    remove_remote_source, remote_file_name, resolve_remote_conflict, TransferSummary,
};
use crate::file_transfer::{ConflictPolicy, ConflictResolver};
use crate::ssh::sftp_fs::{self, RemoteFileKind};
use crate::ssh::SshConnectionConfig;

pub fn copy_remote_to_same_remote(
    ssh_config: &SshConnectionConfig,
    remote_path: &str,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    rename_to: Option<&str>,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;
    let file_name = remote_file_name(remote_path)?;
    let effective_name = rename_to.unwrap_or(&file_name);
    if is_remote_noop_move(remote_path, destination_dir) {
        return Ok(TransferSummary::default());
    }

    sftp_fs::with_cached_session(ssh_config, |session| {
        let metadata = session.metadata(remote_path)?;
        session.create_dir_all(destination_dir)?;

        if metadata.kind == RemoteFileKind::File {
            let desired_path = sftp_fs::join_remote_path(destination_dir, effective_name);
            let destination_path =
                resolve_remote_conflict(session, &desired_path, conflict_policy, resolver, cancelled)?
                    .ok_or_else(|| anyhow!("传输已跳过"))?;
            if remote_path == destination_path {
                return Ok(TransferSummary::from_remote_metadata(&metadata));
            }
            return copy_remote_file_to_remote_path(
                session,
                remote_path,
                &destination_path,
                cancelled,
                progress,
            );
        }

        if metadata.kind == RemoteFileKind::Directory {
            let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);
            let destination_root =
                resolve_remote_conflict(session, &desired_path, conflict_policy, resolver, cancelled)?
                    .ok_or_else(|| anyhow!("传输已跳过"))?;
            if remote_path == destination_root {
                return Ok(TransferSummary::from_remote_metadata(&metadata));
            }
            prevent_remote_recursive_copy(remote_path, &destination_root)?;
            session.create_dir_all(&destination_root)?;
            return copy_remote_dir_to_remote_path(
                session,
                remote_path,
                &destination_root,
                conflict_policy,
                cancelled,
                progress,
                resolver,
            );
        }

        bail!("不支持的远程文件类型: {}", remote_path)
    })
}

pub fn move_remote_to_same_remote(
    ssh_config: &SshConnectionConfig,
    remote_path: &str,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    rename_to: Option<&str>,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;
    let file_name = remote_file_name(remote_path)?;

    sftp_fs::with_cached_session(ssh_config, |session| {
        let metadata = session.metadata(remote_path)?;
        session.create_dir_all(destination_dir)?;
        if is_remote_noop_move(remote_path, destination_dir) {
            return Ok(TransferSummary::from_remote_metadata(&metadata));
        }
        let effective_name = if metadata.kind == RemoteFileKind::File {
            rename_to.unwrap_or(&file_name)
        } else {
            file_name.as_str()
        };
        let desired_path = sftp_fs::join_remote_path(destination_dir, effective_name);
        let destination_path =
            resolve_remote_conflict(session, &desired_path, conflict_policy, resolver, cancelled)?
                .ok_or_else(|| anyhow!("传输已跳过"))?;

        if remote_path == destination_path {
            return Ok(TransferSummary::from_remote_metadata(&metadata));
        }

        if metadata.kind == RemoteFileKind::Directory {
            prevent_remote_recursive_copy(remote_path, &destination_path)?;
        }

        match session.rename(remote_path, &destination_path) {
            Ok(()) => Ok(TransferSummary::from_remote_metadata(&metadata)),
            Err(_) => {
                let summary = copy_remote_to_same_remote(
                    ssh_config,
                    remote_path,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    progress,
                    resolver,
                )?;
                remove_remote_source(session, remote_path)?;
                Ok(summary)
            }
        }
    })
}

pub fn copy_remote_dir_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_dir: &str,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    session.create_dir_all(destination_dir)?;

    if let Ok(Some(mode)) = session.permissions(remote_dir) {
        let _ = session.set_permissions(destination_dir, mode);
    }

    let mut summary = TransferSummary::default();

    for entry in session.read_dir(remote_dir)? {
        check_cancelled(cancelled)?;
        let destination_path = sftp_fs::join_remote_path(destination_dir, &entry.name);

        if entry.kind == RemoteFileKind::File {
            let destination_path =
                resolve_remote_conflict(session, &destination_path, conflict_policy, resolver, cancelled)?
                    .ok_or_else(|| anyhow!("传输已跳过"))?;
            summary.merge(copy_remote_file_to_remote_path(
                session,
                &entry.path,
                &destination_path,
                cancelled,
                progress,
            )?);
        } else if entry.kind == RemoteFileKind::Directory {
            let destination_path =
                resolve_remote_conflict(session, &destination_path, conflict_policy, resolver, cancelled)?
                    .ok_or_else(|| anyhow!("传输已跳过"))?;
            session.create_dir_all(&destination_path)?;
            summary.merge(copy_remote_dir_to_remote_path(
                session,
                &entry.path,
                &destination_path,
                conflict_policy,
                cancelled,
                progress,
                resolver,
            )?);
        }
    }

    Ok(summary)
}

/// 同服务器远程文件复制：直接调用 session 的内存流式复制，不写本地磁盘。
pub fn copy_remote_file_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    let transferred =
        session.copy_remote_file(remote_path, destination_path, cancelled, progress)?;
    Ok(TransferSummary::one_file(transferred))
}
