use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use crate::file_transfer::{
    ConflictPolicy, ConflictResolver, FileEndpoint, TransferOperation, TransferRequest,
};
use crate::file_transfer::cross_remote::{self, TransferStatusReporter};
use crate::file_transfer::utils::TransferSummary;

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
        Self::execute_blocking_with_cancel_and_progress_and_resolver(
            request, cancelled, progress, None,
        )
    }

    pub fn execute_blocking_with_cancel_and_progress_and_resolver(
        request: TransferRequest,
        cancelled: Arc<AtomicBool>,
        progress: Option<Arc<AtomicU64>>,
        resolver: Option<&dyn ConflictResolver>,
    ) -> anyhow::Result<TransferSummary> {
        Self::execute_blocking_with_cancel_and_progress_and_resolver_and_reporter(
            request, cancelled, progress, resolver, None,
        )
    }

    pub fn execute_blocking_with_cancel_and_progress_and_resolver_and_reporter(
        request: TransferRequest,
        cancelled: Arc<AtomicBool>,
        progress: Option<Arc<AtomicU64>>,
        resolver: Option<&dyn ConflictResolver>,
        reporter: Option<&dyn TransferStatusReporter>,
    ) -> anyhow::Result<TransferSummary> {
        let mut summary = TransferSummary::default();
        let rename_to = if request.sources.len() == 1 {
            request.rename_to.clone()
        } else {
            None
        };
        for source in request.sources {
            crate::file_transfer::utils::check_cancelled(&cancelled)?;
            summary.merge(Self::transfer_one(
                source,
                &request.destination,
                request.operation,
                request.conflict_policy,
                rename_to.as_deref(),
                &cancelled,
                progress.as_deref(),
                resolver,
                reporter,
            )?);
        }

        Ok(summary)
    }

    /// Move 操作的删源守卫：仅当本次传输没有任何条目被跳过（`skipped == 0`）时，
    /// 才执行删源闭包。否则保留源，避免把"用户选择跳过 / 不支持的符号链接"等
    /// **没有真正复制过去**的内容连同源一起删掉，造成数据丢失。
    fn remove_source_if_complete(
        summary: &TransferSummary,
        remove: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if summary.skipped == 0 {
            remove()
        } else {
            log::warn!(
                "移动操作有 {} 个条目被跳过，保留源不删除以防数据丢失",
                summary.skipped
            );
            Ok(())
        }
    }

    fn transfer_one(
        source: FileEndpoint,
        destination: &FileEndpoint,
        operation: TransferOperation,
        conflict_policy: ConflictPolicy,
        rename_to: Option<&str>,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
        resolver: Option<&dyn ConflictResolver>,
        reporter: Option<&dyn TransferStatusReporter>,
    ) -> anyhow::Result<TransferSummary> {
        match (source, destination) {
            (
                FileEndpoint::Local { path },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) if operation == TransferOperation::Move => {
                crate::file_transfer::local_ops::move_local_to_local(
                    &path,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    resolver,
                )
            }
            (
                FileEndpoint::Local { path },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) => {
                crate::file_transfer::local_ops::copy_local_to_local(
                    &path,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    resolver,
                )
            }
            (
                FileEndpoint::Local { path },
                FileEndpoint::Remote {
                    ssh_config,
                    path: destination_dir,
                },
            ) => {
                let summary = crate::file_transfer::upload_download::upload_local_to_remote(
                    &path,
                    ssh_config,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    progress,
                    resolver,
                )?;
                if operation == TransferOperation::Move {
                    Self::remove_source_if_complete(&summary, || {
                        crate::file_transfer::utils::remove_local_source(&path)
                    })?;
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
                let summary = crate::file_transfer::upload_download::download_remote_to_local(
                    &ssh_config,
                    &remote_path,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    progress,
                    resolver,
                )?;
                if operation == TransferOperation::Move {
                    Self::remove_source_if_complete(&summary, || {
                        crate::ssh::sftp_fs::with_cached_session(&ssh_config, |session| {
                            crate::file_transfer::utils::remove_remote_source(session, &remote_path)
                        })
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
                crate::file_transfer::remote_ops::move_remote_to_same_remote(
                    &ssh_config,
                    &remote_path,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    progress,
                    resolver,
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
            ) if ssh_config == *destination_config => {
                crate::file_transfer::remote_ops::copy_remote_to_same_remote(
                    &ssh_config,
                    &remote_path,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    progress,
                    resolver,
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
            ) => {
                let summary = cross_remote::copy_remote_to_different_remote(
                    &ssh_config,
                    &remote_path,
                    destination_config,
                    destination_dir,
                    conflict_policy,
                    rename_to,
                    cancelled,
                    progress,
                    resolver,
                    reporter,
                )?;
                if operation == TransferOperation::Move {
                    Self::remove_source_if_complete(&summary, || {
                        crate::ssh::sftp_fs::with_cached_session(&ssh_config, |session| {
                            crate::file_transfer::utils::remove_remote_source(session, &remote_path)
                        })
                    })?;
                }
                Ok(summary)
            }
        }
    }
}
