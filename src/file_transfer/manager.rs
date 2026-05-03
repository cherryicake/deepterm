use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context as _, anyhow, bail};

use crate::file_transfer::{ConflictPolicy, FileEndpoint, TransferOperation, TransferRequest};
use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::{self, RemoteFileKind};

pub struct TransferManager;

impl TransferManager {
    pub fn execute_blocking(request: TransferRequest) -> anyhow::Result<TransferSummary> {
        Self::execute_blocking_with_cancel(request, Arc::new(AtomicBool::new(false)))
    }

    pub fn execute_blocking_with_cancel(
        request: TransferRequest,
        cancelled: Arc<AtomicBool>,
    ) -> anyhow::Result<TransferSummary> {
        Self::execute_blocking_with_cancel_and_progress(request, cancelled, None)
    }

    pub fn execute_blocking_with_cancel_and_progress(
        request: TransferRequest,
        cancelled: Arc<AtomicBool>,
        progress: Option<Arc<AtomicU64>>,
    ) -> anyhow::Result<TransferSummary> {
        let mut summary = TransferSummary::default();
        for source in request.sources {
            check_cancelled(&cancelled)?;
            summary.merge(Self::transfer_one(
                source,
                &request.destination,
                request.operation,
                request.conflict_policy,
                &cancelled,
                progress.as_deref(),
            )?);
        }

        Ok(summary)
    }

    fn transfer_one(
        source: FileEndpoint,
        destination: &FileEndpoint,
        operation: TransferOperation,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        match (source, destination) {
            (
                FileEndpoint::Local { path },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) if operation == TransferOperation::Move => {
                Self::move_local_to_local(&path, destination_dir, conflict_policy, cancelled)
            }
            (
                FileEndpoint::Local { path },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) => Self::copy_local_to_local(&path, destination_dir, conflict_policy, cancelled),
            (
                FileEndpoint::Local { path },
                FileEndpoint::Remote {
                    ssh_config,
                    path: destination_dir,
                },
            ) => {
                let summary = Self::upload_local_to_remote(
                    &path,
                    ssh_config,
                    destination_dir,
                    conflict_policy,
                    cancelled,
                    progress,
                )?;
                if operation == TransferOperation::Move {
                    remove_local_source(&path)?;
                }
                Ok(summary)
            }
            (
                FileEndpoint::Remote {
                    ssh_config,
                    path: remote_path,
                },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) => {
                let summary = Self::download_remote_to_local(
                    &ssh_config,
                    &remote_path,
                    destination_dir,
                    conflict_policy,
                    cancelled,
                    progress,
                )?;
                if operation == TransferOperation::Move {
                    sftp_fs::with_cached_session(&ssh_config, |session| {
                        remove_remote_source(session, &remote_path)
                    })?;
                }
                Ok(summary)
            }
            (
                FileEndpoint::Remote {
                    ssh_config,
                    path: remote_path,
                },
                FileEndpoint::Remote {
                    ssh_config: destination_config,
                    path: destination_dir,
                },
            ) if ssh_config == *destination_config && operation == TransferOperation::Move => {
                Self::move_remote_to_same_remote(
                    &ssh_config,
                    &remote_path,
                    destination_dir,
                    conflict_policy,
                    cancelled,
                    progress,
                )
            }
            (
                FileEndpoint::Remote {
                    ssh_config,
                    path: remote_path,
                },
                FileEndpoint::Remote {
                    ssh_config: destination_config,
                    path: destination_dir,
                },
            ) if ssh_config == *destination_config => Self::copy_remote_to_same_remote(
                &ssh_config,
                &remote_path,
                destination_dir,
                conflict_policy,
                cancelled,
                progress,
            ),
            (
                FileEndpoint::Remote {
                    ssh_config,
                    path: remote_path,
                },
                FileEndpoint::Remote {
                    ssh_config: destination_config,
                    path: destination_dir,
                },
            ) => {
                let summary = Self::copy_remote_to_different_remote(
                    &ssh_config,
                    &remote_path,
                    destination_config,
                    destination_dir,
                    conflict_policy,
                    cancelled,
                    progress,
                )?;
                if operation == TransferOperation::Move {
                    sftp_fs::with_cached_session(&ssh_config, |session| {
                        remove_remote_source(session, &remote_path)
                    })?;
                }
                Ok(summary)
            }
        }
    }

    fn copy_local_to_local(
        source_path: &Path,
        destination_dir: &Path,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let metadata = fs::metadata(source_path)
            .with_context(|| format!("unable to inspect {}", source_path.display()))?;
        fs::create_dir_all(destination_dir)?;
        let file_name = local_file_name(source_path)?;
        let desired_path = destination_dir.join(file_name);

        if metadata.is_file() {
            let destination_path = resolve_local_conflict(&desired_path, conflict_policy)?
                .ok_or_else(|| anyhow!("transfer skipped"))?;
            return copy_local_file_to_path(source_path, &destination_path, cancelled);
        }

        if metadata.is_dir() {
            let destination_root = resolve_local_conflict(&desired_path, conflict_policy)?
                .ok_or_else(|| anyhow!("transfer skipped"))?;
            prevent_local_recursive_copy(source_path, &destination_root)?;
            return copy_local_dir_to_path(
                source_path,
                &destination_root,
                conflict_policy,
                cancelled,
            );
        }

        bail!("unsupported local file type: {}", source_path.display())
    }

    fn move_local_to_local(
        source_path: &Path,
        destination_dir: &Path,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let metadata = fs::metadata(source_path)
            .with_context(|| format!("unable to inspect {}", source_path.display()))?;
        fs::create_dir_all(destination_dir)?;
        if is_local_noop_move(source_path, destination_dir) {
            return Ok(TransferSummary::default());
        }

        let destination_path = resolve_local_conflict(
            &destination_dir.join(local_file_name(source_path)?),
            conflict_policy,
        )?
        .ok_or_else(|| anyhow!("transfer skipped"))?;

        if metadata.is_dir() {
            prevent_local_recursive_copy(source_path, &destination_path)?;
        }

        match fs::rename(source_path, &destination_path) {
            Ok(()) => Ok(TransferSummary::from_metadata(&metadata)),
            Err(_) => {
                let summary = Self::copy_local_to_local(
                    source_path,
                    destination_dir,
                    conflict_policy,
                    cancelled,
                )?;
                remove_local_source(source_path)?;
                Ok(summary)
            }
        }
    }

    fn upload_local_to_remote(
        source_path: &Path,
        ssh_config: &SshConnectionConfig,
        destination_dir: &str,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let metadata = fs::metadata(source_path)
            .with_context(|| format!("unable to inspect {}", source_path.display()))?;
        let file_name = local_file_name(source_path)?;
        let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        sftp_fs::with_cached_session(ssh_config, |session| {
            session.create_dir_all(destination_dir)?;

            if metadata.is_file() {
                let destination_path =
                    resolve_remote_conflict(session, &desired_path, conflict_policy)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
                return upload_local_file_to_remote_path(
                    session,
                    source_path,
                    &destination_path,
                    cancelled,
                    progress,
                );
            }

            if metadata.is_dir() {
                let destination_root =
                    resolve_remote_conflict(session, &desired_path, conflict_policy)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
                session.create_dir_all(&destination_root)?;
                return upload_local_dir_to_remote_path(
                    session,
                    source_path,
                    &destination_root,
                    conflict_policy,
                    cancelled,
                    progress,
                );
            }

            bail!("unsupported local file type: {}", source_path.display())
        })
    }

    fn download_remote_to_local(
        ssh_config: &SshConnectionConfig,
        remote_path: &str,
        destination_dir: &Path,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        fs::create_dir_all(destination_dir)?;
        let file_name = remote_file_name(remote_path)?;
        let desired_path = destination_dir.join(file_name);

        sftp_fs::with_cached_session(ssh_config, |session| {
            let metadata = session.metadata(remote_path)?;

            if metadata.kind == RemoteFileKind::File {
                let destination_path = resolve_local_conflict(&desired_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
                return download_remote_file_to_local_path(
                    session,
                    remote_path,
                    &destination_path,
                    cancelled,
                    progress,
                );
            }

            if metadata.kind == RemoteFileKind::Directory {
                let destination_root = resolve_local_conflict(&desired_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
                fs::create_dir_all(&destination_root)?;
                return download_remote_dir_to_local_path(
                    session,
                    remote_path,
                    &destination_root,
                    conflict_policy,
                    cancelled,
                    progress,
                );
            }

            bail!("unsupported remote file type: {}", remote_path)
        })
    }

    fn copy_remote_to_same_remote(
        ssh_config: &SshConnectionConfig,
        remote_path: &str,
        destination_dir: &str,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let file_name = remote_file_name(remote_path)?;
        let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);
        if is_remote_noop_move(remote_path, destination_dir) {
            return Ok(TransferSummary::default());
        }

        sftp_fs::with_cached_session(ssh_config, |session| {
            let metadata = session.metadata(remote_path)?;
            session.create_dir_all(destination_dir)?;

            if metadata.kind == RemoteFileKind::File {
                let destination_path =
                    resolve_remote_conflict(session, &desired_path, conflict_policy)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
                return copy_remote_file_to_remote_path(
                    session,
                    remote_path,
                    &destination_path,
                    cancelled,
                    progress,
                );
            }

            if metadata.kind == RemoteFileKind::Directory {
                let destination_root =
                    resolve_remote_conflict(session, &desired_path, conflict_policy)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
                prevent_remote_recursive_copy(remote_path, &destination_root)?;
                session.create_dir_all(&destination_root)?;
                return copy_remote_dir_to_remote_path(
                    session,
                    remote_path,
                    &destination_root,
                    conflict_policy,
                    cancelled,
                    progress,
                );
            }

            bail!("unsupported remote file type: {}", remote_path)
        })
    }

    fn move_remote_to_same_remote(
        ssh_config: &SshConnectionConfig,
        remote_path: &str,
        destination_dir: &str,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let file_name = remote_file_name(remote_path)?;
        let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        sftp_fs::with_cached_session(ssh_config, |session| {
            let metadata = session.metadata(remote_path)?;
            session.create_dir_all(destination_dir)?;
            let destination_path =
                resolve_remote_conflict(session, &desired_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;

            if metadata.kind == RemoteFileKind::Directory {
                prevent_remote_recursive_copy(remote_path, &destination_path)?;
            }

            match session.rename(remote_path, &destination_path) {
                Ok(()) => Ok(TransferSummary::from_remote_metadata(&metadata)),
                Err(_) => {
                    let summary = Self::copy_remote_to_same_remote(
                        ssh_config,
                        remote_path,
                        destination_dir,
                        conflict_policy,
                        cancelled,
                        progress,
                    )?;
                    remove_remote_source(session, remote_path)?;
                    Ok(summary)
                }
            }
        })
    }

    fn copy_remote_to_different_remote(
        source_config: &SshConnectionConfig,
        remote_path: &str,
        destination_config: &SshConnectionConfig,
        destination_dir: &str,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let file_name = remote_file_name(remote_path)?;
        let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        sftp_fs::with_cached_session(source_config, |source_session| {
            let metadata = source_session.metadata(remote_path)?;
            sftp_fs::with_cached_session(destination_config, |destination_session| {
                destination_session.create_dir_all(destination_dir)?;

                if metadata.kind == RemoteFileKind::File {
                    let destination_path = resolve_remote_conflict(
                        destination_session,
                        &desired_path,
                        conflict_policy,
                    )?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
                    return copy_remote_file_between_sessions(
                        source_session,
                        remote_path,
                        destination_session,
                        &destination_path,
                        cancelled,
                        progress,
                    );
                }

                if metadata.kind == RemoteFileKind::Directory {
                    let destination_root = resolve_remote_conflict(
                        destination_session,
                        &desired_path,
                        conflict_policy,
                    )?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
                    destination_session.create_dir_all(&destination_root)?;
                    return copy_remote_dir_between_sessions(
                        source_session,
                        remote_path,
                        destination_session,
                        &destination_root,
                        conflict_policy,
                        cancelled,
                        progress,
                    );
                }

                bail!("unsupported remote file type: {}", remote_path)
            })
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct TransferSummary {
    pub files_transferred: usize,
    pub bytes_transferred: u64,
}

impl TransferSummary {
    fn one_file(bytes_transferred: u64) -> Self {
        Self {
            files_transferred: 1,
            bytes_transferred,
        }
    }

    fn from_metadata(metadata: &fs::Metadata) -> Self {
        if metadata.is_file() {
            Self::one_file(metadata.len())
        } else {
            Self::default()
        }
    }

    fn from_remote_metadata(metadata: &crate::ssh::sftp_fs::RemoteFileMetadata) -> Self {
        if metadata.kind == RemoteFileKind::File {
            Self::one_file(metadata.size)
        } else {
            Self::default()
        }
    }

    fn merge(&mut self, other: Self) {
        self.files_transferred += other.files_transferred;
        self.bytes_transferred += other.bytes_transferred;
    }
}

fn copy_local_dir_to_path(
    source_dir: &Path,
    destination_dir: &Path,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<TransferSummary> {
    fs::create_dir_all(destination_dir)?;
    let mut summary = TransferSummary::default();

    for entry in fs::read_dir(source_dir)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        let source_path = entry.path();
        let metadata = entry.metadata()?;
        let destination_path = destination_dir.join(entry.file_name());

        if metadata.is_file() {
            let destination_path = resolve_local_conflict(&destination_path, conflict_policy)?
                .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(copy_local_file_to_path(
                &source_path,
                &destination_path,
                cancelled,
            )?);
        } else if metadata.is_dir() {
            let destination_path = resolve_local_conflict(&destination_path, conflict_policy)?
                .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(copy_local_dir_to_path(
                &source_path,
                &destination_path,
                conflict_policy,
                cancelled,
            )?);
        }
    }

    Ok(summary)
}

fn copy_local_file_to_path(
    source_path: &Path,
    destination_path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<TransferSummary> {
    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary_path = temporary_local_path(destination_path);
    if temporary_path.exists() {
        let _ = fs::remove_file(&temporary_path);
    }
    let result = copy_local_file_interruptible(source_path, &temporary_path, cancelled);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    let transferred = result?;
    if let Err(error) = fs::rename(&temporary_path, destination_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    Ok(TransferSummary::one_file(transferred))
}

fn upload_local_dir_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    source_dir: &Path,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    session.create_dir_all(destination_dir)?;
    let mut summary = TransferSummary::default();

    for entry in fs::read_dir(source_dir)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        let source_path = entry.path();
        let metadata = entry.metadata()?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        let destination_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        if metadata.is_file() {
            let destination_path =
                resolve_remote_conflict(session, &destination_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(upload_local_file_to_remote_path(
                session,
                &source_path,
                &destination_path,
                cancelled,
                progress,
            )?);
        } else if metadata.is_dir() {
            let destination_path =
                resolve_remote_conflict(session, &destination_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            session.create_dir_all(&destination_path)?;
            summary.merge(upload_local_dir_to_remote_path(
                session,
                &source_path,
                &destination_path,
                conflict_policy,
                cancelled,
                progress,
            )?);
        }
    }

    Ok(summary)
}

fn upload_local_file_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    source_path: &Path,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
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
    if let Err(error) = result {
        let _ = session.remove_file(&temporary_path);
        return Err(error);
    }

    let transferred = result?;
    if let Err(error) = session.rename(&temporary_path, destination_path) {
        let _ = session.remove_file(&temporary_path);
        return Err(error);
    }
    Ok(TransferSummary::one_file(transferred))
}

fn download_remote_dir_to_local_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_dir: &str,
    destination_dir: &Path,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    fs::create_dir_all(destination_dir)?;
    let mut summary = TransferSummary::default();

    for entry in session.read_dir(remote_dir)? {
        check_cancelled(cancelled)?;
        let destination_path = destination_dir.join(&entry.name);

        if entry.kind == RemoteFileKind::File {
            let destination_path = resolve_local_conflict(&destination_path, conflict_policy)?
                .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(download_remote_file_to_local_path(
                session,
                &entry.path,
                &destination_path,
                cancelled,
                progress,
            )?);
        } else if entry.kind == RemoteFileKind::Directory {
            let destination_path = resolve_local_conflict(&destination_path, conflict_policy)?
                .ok_or_else(|| anyhow!("transfer skipped"))?;
            fs::create_dir_all(&destination_path)?;
            summary.merge(download_remote_dir_to_local_path(
                session,
                &entry.path,
                &destination_path,
                conflict_policy,
                cancelled,
                progress,
            )?);
        }
    }

    Ok(summary)
}

fn download_remote_file_to_local_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_path: &Path,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
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
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    let transferred = result?;
    if let Err(error) = fs::rename(&temporary_path, destination_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    Ok(TransferSummary::one_file(transferred))
}

fn copy_remote_dir_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_dir: &str,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    session.create_dir_all(destination_dir)?;
    let mut summary = TransferSummary::default();

    for entry in session.read_dir(remote_dir)? {
        check_cancelled(cancelled)?;
        let destination_path = sftp_fs::join_remote_path(destination_dir, &entry.name);

        if entry.kind == RemoteFileKind::File {
            let destination_path =
                resolve_remote_conflict(session, &destination_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(copy_remote_file_to_remote_path(
                session,
                &entry.path,
                &destination_path,
                cancelled,
                progress,
            )?);
        } else if entry.kind == RemoteFileKind::Directory {
            let destination_path =
                resolve_remote_conflict(session, &destination_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            session.create_dir_all(&destination_path)?;
            summary.merge(copy_remote_dir_to_remote_path(
                session,
                &entry.path,
                &destination_path,
                conflict_policy,
                cancelled,
                progress,
            )?);
        }
    }

    Ok(summary)
}

fn copy_remote_file_to_remote_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    let temporary_path = temporary_remote_path(destination_path);
    if session.exists(&temporary_path)? {
        let _ = session.remove_file(&temporary_path);
    }

    let local_temporary =
        std::env::temp_dir().join(format!("deepterm-remote-copy-{}", uuid::Uuid::new_v4()));
    let download_result = session
        .download_file_to_local_with_cancel_and_progress(
            remote_path,
            &local_temporary,
            cancelled,
            progress,
        );
    if let Err(error) = download_result {
        let _ = fs::remove_file(&local_temporary);
        let _ = session.remove_file(&temporary_path);
        return Err(error);
    }

    let transferred = download_result?;
    let upload_result = session.upload_file_from_local_with_cancel_and_progress(
        &local_temporary,
        &temporary_path,
        cancelled,
        progress,
    );
    let _ = fs::remove_file(&local_temporary);
    if let Err(error) = upload_result {
        let _ = session.remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = session.rename(&temporary_path, destination_path) {
        let _ = session.remove_file(&temporary_path);
        return Err(error);
    }
    Ok(TransferSummary::one_file(transferred))
}

fn copy_remote_dir_between_sessions(
    source_session: &mut sftp_fs::BlockingSftpSession,
    remote_dir: &str,
    destination_session: &mut sftp_fs::BlockingSftpSession,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    destination_session.create_dir_all(destination_dir)?;
    let mut summary = TransferSummary::default();

    for entry in source_session.read_dir(remote_dir)? {
        check_cancelled(cancelled)?;
        let destination_path = sftp_fs::join_remote_path(destination_dir, &entry.name);

        if entry.kind == RemoteFileKind::File {
            let destination_path =
                resolve_remote_conflict(destination_session, &destination_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(copy_remote_file_between_sessions(
                source_session,
                &entry.path,
                destination_session,
                &destination_path,
                cancelled,
                progress,
            )?);
        } else if entry.kind == RemoteFileKind::Directory {
            let destination_path =
                resolve_remote_conflict(destination_session, &destination_path, conflict_policy)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            destination_session.create_dir_all(&destination_path)?;
            summary.merge(copy_remote_dir_between_sessions(
                source_session,
                &entry.path,
                destination_session,
                &destination_path,
                conflict_policy,
                cancelled,
                progress,
            )?);
        }
    }

    Ok(summary)
}

fn copy_remote_file_between_sessions(
    source_session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_session: &mut sftp_fs::BlockingSftpSession,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    let temporary_path = temporary_remote_path(destination_path);
    if destination_session.exists(&temporary_path)? {
        let _ = destination_session.remove_file(&temporary_path);
    }

    let local_temporary =
        std::env::temp_dir().join(format!("deepterm-remote-bridge-{}", uuid::Uuid::new_v4()));
    let download_result = source_session
        .download_file_to_local_with_cancel_and_progress(
            remote_path,
            &local_temporary,
            cancelled,
            progress,
        );
    if let Err(error) = download_result {
        let _ = fs::remove_file(&local_temporary);
        let _ = destination_session.remove_file(&temporary_path);
        return Err(error);
    }

    let transferred = download_result?;
    let upload_result = destination_session.upload_file_from_local_with_cancel_and_progress(
        &local_temporary,
        &temporary_path,
        cancelled,
        progress,
    );
    let _ = fs::remove_file(&local_temporary);
    if let Err(error) = upload_result {
        let _ = destination_session.remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = destination_session.rename(&temporary_path, destination_path) {
        let _ = destination_session.remove_file(&temporary_path);
        return Err(error);
    }
    Ok(TransferSummary::one_file(transferred))
}

fn local_file_name(path: &Path) -> anyhow::Result<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))
}

fn remote_file_name(path: &str) -> anyhow::Result<String> {
    let trimmed = path.trim_end_matches('/');
    let file_name = trimmed
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("{} has no file name", path))?;
    Ok(file_name.to_string())
}

fn is_local_noop_move(source_path: &Path, destination_dir: &Path) -> bool {
    // 快速路径：直接相等（把目录拖到它自己上）
    if source_path == destination_dir {
        return true;
    }

    // 用 components 比较，忽略尾部斜杠差异
    let source_components: Vec<_> = source_path.components().collect();
    let dest_components: Vec<_> = destination_dir.components().collect();

    if source_components == dest_components {
        return true;
    }

    // source 的父目录是否等于 destination
    if source_components.len() >= 2 {
        let parent = &source_components[..source_components.len() - 1];
        if parent == dest_components.as_slice() {
            return true;
        }
    }

    false
}

fn is_remote_noop_move(remote_path: &str, destination_dir: &str) -> bool {
    let source = normalize_remote_dir(remote_path);
    let destination = normalize_remote_dir(destination_dir);

    if source == destination {
        return true;
    }

    sftp_fs::remote_parent(remote_path)
        .map(|parent| normalize_remote_dir(&parent) == destination)
        .unwrap_or(false)
}

fn copy_local_file_interruptible(
    source_path: &Path,
    destination_path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<u64> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let mut source = fs::File::open(source_path)?;
    let mut destination = fs::File::create(destination_path)?;
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut transferred = 0;

    loop {
        check_cancelled(cancelled)?;
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        destination.write_all(&buffer[..read])?;
        transferred += read as u64;
    }

    check_cancelled(cancelled)?;
    destination.flush()?;
    Ok(transferred)
}

fn remove_local_source(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_remote_source(
    session: &mut sftp_fs::BlockingSftpSession,
    path: &str,
) -> anyhow::Result<()> {
    let metadata = session.metadata(path)?;
    match metadata.kind {
        RemoteFileKind::File | RemoteFileKind::Symlink | RemoteFileKind::Other => {
            session.remove_file(path)?;
        }
        RemoteFileKind::Directory => {
            remove_remote_dir_recursive(session, path)?;
        }
    }
    Ok(())
}

fn remove_remote_dir_recursive(
    session: &mut sftp_fs::BlockingSftpSession,
    path: &str,
) -> anyhow::Result<()> {
    for entry in session.read_dir(path)? {
        match entry.kind {
            RemoteFileKind::Directory => remove_remote_dir_recursive(session, &entry.path)?,
            RemoteFileKind::File | RemoteFileKind::Symlink | RemoteFileKind::Other => {
                session.remove_file(&entry.path)?;
            }
        }
    }
    session.remove_dir(path)?;
    Ok(())
}

fn resolve_local_conflict(
    desired_path: &Path,
    conflict_policy: ConflictPolicy,
) -> anyhow::Result<Option<PathBuf>> {
    if !desired_path.exists() {
        return Ok(Some(desired_path.to_path_buf()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_path_buf())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => bail!("conflict policy Ask is not implemented yet"),
        ConflictPolicy::AutoRename => {
            for index in 1..10_000 {
                let candidate = renamed_local_candidate(desired_path, index);
                if !candidate.exists() {
                    return Ok(Some(candidate));
                }
            }
            bail!(
                "unable to find available file name for {}",
                desired_path.display()
            )
        }
    }
}

fn renamed_local_candidate(path: &Path, index: usize) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy());
    let file_name = match extension {
        Some(extension) => format!("{} copy {}.{}", stem, index, extension),
        None => format!("{} copy {}", stem, index),
    };
    parent.join(file_name)
}

fn resolve_remote_conflict(
    session: &mut sftp_fs::BlockingSftpSession,
    desired_path: &str,
    conflict_policy: ConflictPolicy,
) -> anyhow::Result<Option<String>> {
    if !session.exists(desired_path)? {
        return Ok(Some(desired_path.to_string()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_string())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => bail!("conflict policy Ask is not implemented yet"),
        ConflictPolicy::AutoRename => {
            for index in 1..10_000 {
                let candidate = renamed_remote_candidate(desired_path, index)?;
                if !session.exists(&candidate)? {
                    return Ok(Some(candidate));
                }
            }
            bail!(
                "unable to find available remote file name for {}",
                desired_path
            )
        }
    }
}

fn renamed_remote_candidate(path: &str, index: usize) -> anyhow::Result<String> {
    let parent = sftp_fs::remote_parent(path).unwrap_or_else(|| "/".to_string());
    let file_name = remote_file_name(path)?;
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            (stem.to_string(), Some(extension.to_string()))
        }
        _ => (file_name, None),
    };
    let candidate_name = match extension {
        Some(extension) => format!("{} copy {}.{}", stem, index, extension),
        None => format!("{} copy {}", stem, index),
    };
    Ok(sftp_fs::join_remote_path(&parent, &candidate_name))
}

fn temporary_local_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "transfer".to_string());
    path.with_file_name(format!(".{}.deepterm-part", file_name))
}

fn temporary_remote_path(path: &str) -> String {
    let parent = sftp_fs::remote_parent(path).unwrap_or_else(|| "/".to_string());
    let file_name = remote_file_name(path).unwrap_or_else(|_| "transfer".to_string());
    sftp_fs::join_remote_path(&parent, &format!(".{}.deepterm-part", file_name))
}

fn prevent_local_recursive_copy(source_dir: &Path, destination_dir: &Path) -> anyhow::Result<()> {
    let source_dir = source_dir.canonicalize()?;
    let destination_parent = destination_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let destination_parent = destination_parent.canonicalize()?;
    if destination_parent.starts_with(&source_dir) {
        bail!("cannot copy a folder into itself: {}", source_dir.display());
    }
    Ok(())
}

fn prevent_remote_recursive_copy(source_dir: &str, destination_dir: &str) -> anyhow::Result<()> {
    let source = normalize_remote_dir(source_dir);
    let destination = normalize_remote_dir(destination_dir);
    if destination.starts_with(&format!("{}/", source.trim_end_matches('/')))
        || destination == source
    {
        bail!("cannot copy a remote folder into itself: {}", source_dir);
    }
    Ok(())
}

fn normalize_remote_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn check_cancelled(cancelled: &Arc<AtomicBool>) -> anyhow::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("transfer cancelled");
    }
    Ok(())
}
