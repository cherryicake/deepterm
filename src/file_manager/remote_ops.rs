use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use crate::file_manager::utils::{
    DirTransferDriver, TransferSummary, check_cancelled, is_remote_noop_move,
    prevent_remote_recursive_copy, remote_file_name, remove_remote_source, resolve_remote_conflict,
    walk_remote_dir,
};
use crate::file_manager::{ConflictPolicy, ConflictResolver};
use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::{self, RemoteFileKind};

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

        // 符号链接 / 特殊文件：计为跳过。
        log::warn!(
            "跳过不支持的远程文件类型 ({:?}): {}",
            metadata.kind,
            remote_path
        );
        Ok(TransferSummary::one_skipped())
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
        let destination_path = match resolve_remote_conflict(
            session,
            &desired_path,
            conflict_policy,
            resolver,
            cancelled,
        )? {
            Some(path) => path,
            // 顶层冲突选择跳过：不移动、不删源。
            None => return Ok(TransferSummary::one_skipped()),
        };

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
                // 只有在没有任何条目被跳过时才删源，避免把未复制成功的内容连源一起删掉。
                if summary.skipped == 0 {
                    remove_remote_source(session, remote_path)?;
                } else {
                    log::warn!(
                        "远程移动：有 {} 个条目被跳过，保留源 {} 不删除",
                        summary.skipped,
                        remote_path
                    );
                }
                Ok(summary)
            }
        }
    })
}

/// 同服务器目录复制的 driver：见 [`walk_remote_dir`]。
struct SameSessionDirDriver<'a> {
    session: &'a mut sftp_fs::BlockingSftpSession,
}

impl DirTransferDriver for SameSessionDirDriver<'_> {
    type Destination = String;

    fn read_entries(&mut self, remote_dir: &str) -> anyhow::Result<Vec<sftp_fs::RemoteFileEntry>> {
        self.session.read_dir(remote_dir)
    }

    fn child_destination(&self, destination_dir: &String, name: &str) -> String {
        sftp_fs::join_remote_path(destination_dir, name)
    }

    fn resolve_conflict(
        &mut self,
        desired: &String,
        policy: ConflictPolicy,
        resolver: Option<&dyn ConflictResolver>,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<Option<String>> {
        resolve_remote_conflict(self.session, desired, policy, resolver, cancelled)
    }

    fn prepare_destination_dir(
        &mut self,
        remote_dir: &str,
        destination_dir: &String,
    ) -> anyhow::Result<()> {
        self.session.create_dir_all(destination_dir)?;
        if let Ok(Some(mode)) = self.session.permissions(remote_dir) {
            let _ = self.session.set_permissions(destination_dir, mode);
        }
        Ok(())
    }

    fn transfer_file(
        &mut self,
        source_path: &str,
        destination: &String,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        copy_remote_file_to_remote_path(self.session, source_path, destination, cancelled, progress)
    }
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
    walk_remote_dir(
        &mut SameSessionDirDriver { session },
        remote_dir,
        &destination_dir.to_string(),
        conflict_policy,
        cancelled,
        progress,
        resolver,
    )
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
