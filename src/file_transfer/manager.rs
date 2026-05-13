use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context as _, anyhow, bail};

use crate::file_transfer::{
    ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver, FileEndpoint,
    TransferOperation, TransferRequest, wait_for_resolution,
};
use crate::ssh::SshConnectionConfig;
use crate::ssh::keychain::{ResolvedAuthMethod, resolve_auth_method};
use crate::ssh::sftp_fs::{self, RemoteFileKind};

pub const TRANSFER_CANCELLED_MSG: &str = "transfer cancelled";

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
                resolver,
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
        resolver: Option<&dyn ConflictResolver>,
    ) -> anyhow::Result<TransferSummary> {
        match (source, destination) {
            (
                FileEndpoint::Local { path },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) if operation == TransferOperation::Move => Self::move_local_to_local(
                &path,
                destination_dir,
                conflict_policy,
                cancelled,
                resolver,
            ),
            (
                FileEndpoint::Local { path },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) => Self::copy_local_to_local(
                &path,
                destination_dir,
                conflict_policy,
                cancelled,
                resolver,
            ),
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
                    resolver,
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
                    resolver,
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
            ) if ssh_config == *destination_config => Self::copy_remote_to_same_remote(
                &ssh_config,
                &remote_path,
                destination_dir,
                conflict_policy,
                cancelled,
                progress,
                resolver,
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
                    resolver,
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
        resolver: Option<&dyn ConflictResolver>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        if is_local_noop_move(source_path, destination_dir) {
            return Ok(TransferSummary::default());
        }
        let metadata = fs::metadata(source_path)
            .with_context(|| format!("unable to inspect {}", source_path.display()))?;
        fs::create_dir_all(destination_dir)?;
        let file_name = local_file_name(source_path)?;
        let desired_path = destination_dir.join(file_name);

        if metadata.is_file() {
            let destination_path =
                resolve_local_conflict(&desired_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            if source_path == destination_path {
                return Ok(TransferSummary::from_metadata(&metadata));
            }
            return copy_local_file_to_path(source_path, &destination_path, cancelled);
        }

        if metadata.is_dir() {
            let destination_root =
                resolve_local_conflict(&desired_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            if source_path == destination_root {
                return Ok(TransferSummary::from_metadata(&metadata));
            }
            prevent_local_recursive_copy(source_path, &destination_root)?;
            return copy_local_dir_to_path(
                source_path,
                &destination_root,
                conflict_policy,
                cancelled,
                resolver,
            );
        }

        bail!("unsupported local file type: {}", source_path.display())
    }

    fn move_local_to_local(
        source_path: &Path,
        destination_dir: &Path,
        conflict_policy: ConflictPolicy,
        cancelled: &Arc<AtomicBool>,
        resolver: Option<&dyn ConflictResolver>,
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
            resolver,
        )?
        .ok_or_else(|| anyhow!("transfer skipped"))?;

        if source_path == destination_path {
            return Ok(TransferSummary::from_metadata(&metadata));
        }

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
                    resolver,
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
        resolver: Option<&dyn ConflictResolver>,
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
                    resolve_remote_conflict(session, &desired_path, conflict_policy, resolver)?
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
                    resolve_remote_conflict(session, &desired_path, conflict_policy, resolver)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
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
        resolver: Option<&dyn ConflictResolver>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        fs::create_dir_all(destination_dir)?;
        let file_name = remote_file_name(remote_path)?;
        let desired_path = destination_dir.join(file_name);

        sftp_fs::with_cached_session(ssh_config, |session| {
            let metadata = session.metadata(remote_path)?;

            if metadata.kind == RemoteFileKind::File {
                let destination_path =
                    resolve_local_conflict(&desired_path, conflict_policy, resolver)?
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
                let destination_root =
                    resolve_local_conflict(&desired_path, conflict_policy, resolver)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
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
        resolver: Option<&dyn ConflictResolver>,
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
                    resolve_remote_conflict(session, &desired_path, conflict_policy, resolver)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
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
                let destination_root =
                    resolve_remote_conflict(session, &desired_path, conflict_policy, resolver)?
                        .ok_or_else(|| anyhow!("transfer skipped"))?;
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
        resolver: Option<&dyn ConflictResolver>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let file_name = remote_file_name(remote_path)?;
        let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        sftp_fs::with_cached_session(ssh_config, |session| {
            let metadata = session.metadata(remote_path)?;
            session.create_dir_all(destination_dir)?;
            if is_remote_noop_move(remote_path, destination_dir) {
                return Ok(TransferSummary::from_remote_metadata(&metadata));
            }
            let destination_path =
                resolve_remote_conflict(session, &desired_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;

            if remote_path == destination_path {
                return Ok(TransferSummary::from_remote_metadata(&metadata));
            }

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
                        resolver,
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
        resolver: Option<&dyn ConflictResolver>,
    ) -> anyhow::Result<TransferSummary> {
        check_cancelled(cancelled)?;
        let file_name = remote_file_name(remote_path)?;
        let desired_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        sftp_fs::with_cached_session(source_config, |source_session| {
            let metadata = source_session.metadata(remote_path)?;
            let is_directory = metadata.kind == RemoteFileKind::Directory;

            // 先确保目标父目录存在（scp 要求父目录存在；流式路径也需要）
            sftp_fs::with_cached_session(destination_config, |destination_session| {
                destination_session.create_dir_all(destination_dir)
            })?;

            // 预解析冲突：`Ask` / `AutoRename` 在调用 scp 前先定好最终路径，
            // scp 对这两种策略无法做到精准语义（只能覆盖），所以必要时会降级。
            let resolved_destination =
                sftp_fs::with_cached_session(destination_config, |destination_session| {
                    resolve_remote_conflict(
                        destination_session,
                        &desired_path,
                        conflict_policy,
                        resolver,
                    )
                })?;

            let destination_path = match resolved_destination {
                Some(path) => path,
                None => return Ok(TransferSummary::default()),
            };

            // 只有 `Overwrite` / `Skip`（转换为已存在预检过的路径）/ `AutoRename`
            // 后得到新文件名的三种情况能安全走 scp；`Ask` 如果选了覆盖等同 Overwrite。
            // 这里统一把"新路径"交给 scp，由它写入目标即可。

            let strategies = detect_transfer_strategies(source_session, destination_config);

            let mut last_error: Option<anyhow::Error> = None;

            for strategy in &strategies {
                if cancelled.load(Ordering::Relaxed) {
                    bail!("{}", TRANSFER_CANCELLED_MSG);
                }
                match strategy {
                    RemoteToRemoteStrategy::AgentForwardScp => {
                        match copy_via_agent_forward_scp(
                            source_session,
                            remote_path,
                            destination_config,
                            &destination_path,
                            is_directory,
                            cancelled,
                            progress,
                        ) {
                            Ok(summary) => return Ok(summary),
                            Err(e) => {
                                log::warn!(
                                    "方案 3a (agent forwarding scp) 失败，尝试下一策略: {}",
                                    e
                                );
                                last_error = Some(e);
                            }
                        }
                    }
                    RemoteToRemoteStrategy::TempKeyScp { key_path } => {
                        match copy_via_remote_scp(
                            source_session,
                            source_config,
                            remote_path,
                            destination_config,
                            &destination_path,
                            key_path,
                            is_directory,
                            cancelled,
                            progress,
                        ) {
                            Ok(summary) => return Ok(summary),
                            Err(e) => {
                                log::warn!(
                                    "方案 3b (临时密钥 scp, key={}) 失败，尝试下一策略: {}",
                                    key_path.display(),
                                    e
                                );
                                last_error = Some(e);
                            }
                        }
                    }
                    RemoteToRemoteStrategy::StreamViaLocal => {
                        log::warn!(
                            "降级到流式传输：源 {} -> 目标 {}@{}:{}，数据将经过 DeepTerm 内存中转（可能较慢）",
                            remote_path,
                            destination_config.username,
                            destination_config.host,
                            destination_path
                        );
                        return sftp_fs::with_cached_session(
                            destination_config,
                            |destination_session| {
                                if metadata.kind == RemoteFileKind::File {
                                    copy_remote_file_between_sessions(
                                        source_session,
                                        remote_path,
                                        destination_session,
                                        &destination_path,
                                        cancelled,
                                        progress,
                                    )
                                } else if metadata.kind == RemoteFileKind::Directory {
                                    destination_session.create_dir_all(&destination_path)?;
                                    copy_remote_dir_between_sessions(
                                        source_session,
                                        remote_path,
                                        destination_session,
                                        &destination_path,
                                        conflict_policy,
                                        cancelled,
                                        progress,
                                        resolver,
                                    )
                                } else {
                                    bail!("unsupported remote file type: {}", remote_path)
                                }
                            },
                        );
                    }
                }
            }

            Err(last_error.unwrap_or_else(|| anyhow!("所有远程到远程传输策略均失败")))
        })
    }
}

// ============================================================================
// Phase 3: 服务器直连绕过（Remote-to-Remote Direct Transfer）
// ============================================================================

/// 远程到远程传输策略
#[derive(Clone, Debug, PartialEq)]
enum RemoteToRemoteStrategy {
    /// 在源服务器上执行 scp，通过 **SSH agent forwarding** 鉴权目标端
    /// （数据不经过 DeepTerm，密钥也不落源端）
    AgentForwardScp,
    /// 把一把本地私钥临时上传到源端，在源端执行 scp 直接传输
    /// （数据不经过 DeepTerm；携带具体密钥路径，支持多把 default key 逐个尝试）
    TempKeyScp { key_path: PathBuf },
    /// Phase 1/2 流式传输（数据经过 DeepTerm 内存中转）
    StreamViaLocal,
}

/// 检测可用的远程到远程传输策略，返回按优先级排序的候选列表。
///
/// 调用方应依次尝试每个策略，任一失败时自动降级到下一个。返回序列
/// 最后一定包含 `StreamViaLocal` 作为兜底。
fn detect_transfer_strategies(
    source_session: &mut sftp_fs::BlockingSftpSession,
    destination_config: &SshConnectionConfig,
) -> Vec<RemoteToRemoteStrategy> {
    let mut strategies = Vec::new();

    // 允许通过环境变量强制指定策略，便于排障
    if let Ok(forced) = std::env::var("DEEPTERM_REMOTE_TRANSFER_STRATEGY") {
        log::info!("检测到 DEEPTERM_REMOTE_TRANSFER_STRATEGY={}", forced);
        match forced.as_str() {
            "3a" | "agent" => return vec![RemoteToRemoteStrategy::AgentForwardScp],
            "3b" | "tempkey" => {
                // 强制 3b 时优先查缓存，其次看目标配置，最后兜底枚举默认密钥
                let mut forced_strategies = Vec::new();
                if let Some(cached) =
                    crate::ssh::auth::lookup_authenticated_key(destination_config)
                {
                    forced_strategies
                        .push(RemoteToRemoteStrategy::TempKeyScp { key_path: cached });
                }
                if let Ok(ResolvedAuthMethod::PrivateKey {
                    path,
                    passphrase: None,
                    ..
                }) = resolve_auth_method(&destination_config.auth_method)
                {
                    let candidate = RemoteToRemoteStrategy::TempKeyScp { key_path: path };
                    if !forced_strategies.contains(&candidate) {
                        forced_strategies.push(candidate);
                    }
                }
                if forced_strategies.is_empty() {
                    for key_path in usable_default_keys() {
                        let candidate = RemoteToRemoteStrategy::TempKeyScp {
                            key_path: key_path.clone(),
                        };
                        if !forced_strategies.contains(&candidate) {
                            forced_strategies.push(candidate);
                        }
                    }
                }
                if forced_strategies.is_empty() {
                    log::warn!("强制 3b 但找不到可用的本地无 passphrase 私钥，回退到流式");
                    return vec![RemoteToRemoteStrategy::StreamViaLocal];
                }
                return forced_strategies;
            }
            "pipeline" | "stream" => return vec![RemoteToRemoteStrategy::StreamViaLocal],
            "auto" | _ => {}
        }
    }

    // 1. 检查源服务器是否具备 scp/ssh 能力
    let check_result = source_session.execute_command(
        "command -v scp >/dev/null 2>&1 && command -v ssh >/dev/null 2>&1 && echo 'ok'",
        10,
    );
    let has_scp_ssh = matches!(
        check_result,
        Ok(ref result) if result.success() && result.stdout.trim() == "ok"
    );

    if !has_scp_ssh {
        log::warn!(
            "源服务器缺少 scp/ssh 命令（检测输出: {:?}），只能走流式传输，数据将经过 DeepTerm 中转",
            check_result.as_ref().map(|r| r.stdout.trim()).unwrap_or("<err>")
        );
        strategies.push(RemoteToRemoteStrategy::StreamViaLocal);
        return strategies;
    }
    log::debug!("源服务器具备 scp/ssh 命令");

    // 2. 解析目标认证方式，决定可走哪条直连路径
    let resolved = resolve_auth_method(&destination_config.auth_method);
    log::info!(
        "目标服务器({}@{}:{}) 认证方式解析结果: {}",
        destination_config.username,
        destination_config.host,
        destination_config.port,
        match &resolved {
            Ok(ResolvedAuthMethod::PrivateKey { path, passphrase, .. }) => format!(
                "PrivateKey(path={}, has_passphrase={})",
                path.display(),
                passphrase.is_some()
            ),
            Ok(ResolvedAuthMethod::Password { .. }) => "Password".to_string(),
            Ok(ResolvedAuthMethod::Agent) => "Agent".to_string(),
            Ok(ResolvedAuthMethod::Auto) => "Auto".to_string(),
            Err(e) => format!("Err({})", e),
        }
    );

    match resolved {
        Ok(ResolvedAuthMethod::PrivateKey {
            path, passphrase, ..
        }) => {
            // 3a 首选：如果本机 agent 已加载该密钥，则启用 agent forwarding
            if passphrase.is_none() {
                match crate::ssh::agent::agent_has_key(&path) {
                    Ok(true) => {
                        log::info!(
                            "ssh-agent 已加载目标密钥 {}，优先使用 agent forwarding + scp（数据不经 DeepTerm）",
                            path.display()
                        );
                        strategies.push(RemoteToRemoteStrategy::AgentForwardScp);
                    }
                    Ok(false) => {
                        log::info!(
                            "ssh-agent 可达但未加载目标密钥 {}，跳过 agent forwarding",
                            path.display()
                        );
                    }
                    Err(e) => {
                        log::info!("ssh-agent 不可用 ({}), 跳过 agent forwarding", e);
                    }
                }
            } else {
                log::info!("目标私钥带 passphrase，跳过 agent forwarding（agent 内密钥应已解锁）");
            }

            // 3b 次选：临时上传目标已配置的私钥
            if passphrase.is_none() {
                log::info!(
                    "候选策略: 临时密钥 + scp 直连，使用目标配置密钥 {}（数据不经 DeepTerm）",
                    path.display()
                );
                strategies.push(RemoteToRemoteStrategy::TempKeyScp {
                    key_path: path.clone(),
                });
            } else {
                log::info!("目标私钥含 passphrase，不能用于远程 scp 直连");
            }
        }
        Ok(ResolvedAuthMethod::Agent) | Ok(ResolvedAuthMethod::Auto) => {
            // 1) 若本机 agent 可达且至少有一把身份，agent forwarding 让源端 scp 自动尝试。
            match crate::ssh::agent::agent_is_available() {
                Ok(true) => {
                    log::info!(
                        "目标使用 Agent/Auto 认证，本机 ssh-agent 可用且含身份，优先使用 agent forwarding + scp（数据不经 DeepTerm）"
                    );
                    strategies.push(RemoteToRemoteStrategy::AgentForwardScp);
                }
                Ok(false) => {
                    log::info!(
                        "目标使用 Agent/Auto 认证，但本机 ssh-agent 未加载任何密钥，跳过 agent forwarding"
                    );
                }
                Err(e) => {
                    log::info!(
                        "目标使用 Agent/Auto 认证，但本机 ssh-agent 不可达 ({}), 跳过 agent forwarding",
                        e
                    );
                }
            }
            // 2) 优先复用连接时已认过的那把密钥：DeepTerm 连接目标时（src/ssh/auth.rs
            //    ::try_default_keys）若落在 ~/.ssh 默认密钥的某一把上，会把该 (config->key_path)
            //    记入进程内缓存。此时它就是源端 scp 唯一需要的，不再枚举其它 default key。
            if let Some(cached) = crate::ssh::auth::lookup_authenticated_key(destination_config) {
                log::info!(
                    "候选策略: 临时密钥 + scp 直连，复用连接时认过的密钥 {}（数据不经 DeepTerm）",
                    cached.display()
                );
                strategies.push(RemoteToRemoteStrategy::TempKeyScp { key_path: cached });
            } else {
                // 兜底：尚未连过目标，或连接时用的是 agent（不走磁盘密钥）—— 只能枚举
                // 本机默认密钥，按 try_default_keys 顺序。第一把不行自动降到下一把。
                let default_keys = usable_default_keys();
                if default_keys.is_empty() {
                    log::info!(
                        "本机 ~/.ssh 中没有可用的默认密钥（无 passphrase 且可解析），无法走临时密钥 scp"
                    );
                } else {
                    for key_path in default_keys {
                        log::info!(
                            "候选策略: 临时密钥 + scp 直连，使用本机默认密钥 {}（数据不经 DeepTerm）",
                            key_path.display()
                        );
                        strategies.push(RemoteToRemoteStrategy::TempKeyScp { key_path });
                    }
                }
            }
        }
        Ok(ResolvedAuthMethod::Password { .. }) => {
            log::info!("目标服务器使用密码认证，远程 scp 不安全，降级到流式传输");
        }
        Err(e) => {
            log::warn!("解析目标服务器认证方式失败: {}", e);
        }
    }

    // 兜底：流式传输永远可用
    if !strategies.contains(&RemoteToRemoteStrategy::StreamViaLocal) {
        if strategies.is_empty() {
            log::warn!(
                "没有可用的直连策略，只能走流式传输（数据将经 DeepTerm 内存中转，速度受限）"
            );
        } else {
            log::debug!("追加流式传输作为兜底策略");
        }
    }
    strategies.push(RemoteToRemoteStrategy::StreamViaLocal);
    log::info!(
        "最终远程到远程传输策略顺序: {:?}",
        strategies
    );
    strategies
}

/// 通过 **agent forwarding + scp** 实现远程直连传输（方案 3a）。
///
/// 本机 ssh-agent 已加载目标服务器的密钥时，源端 scp 可以通过转发回来的
/// agent socket 鉴权目标，不需要把私钥搬到源服务器。相对于 [`copy_via_remote_scp`]
/// 更安全：
///
/// - 私钥始终留在本地 agent 内，源端看不到明文；
/// - 不在源端 `/tmp` 生成文件；
/// - 源端命令退出后 forwarded agent socket 立即失效。
fn copy_via_agent_forward_scp(
    source_session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_config: &SshConnectionConfig,
    destination_path: &str,
    is_directory: bool,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;

    let dest_host = format!(
        "{}@{}",
        destination_config.username, destination_config.host
    );

    // scp 的 `-v` 会把详细过程输出到 stderr；目前只拿 exit status，
    // progress_cb 先保留作为将来解析 rsync --info=progress2 的入口。
    let recurse = if is_directory { " -r" } else { "" };
    let port_arg = if destination_config.port == 22 {
        String::new()
    } else {
        format!(" -P {}", destination_config.port)
    };

    let scp_cmd = format!(
        "scp{recurse}{port_arg} -p -B \
         -o StrictHostKeyChecking=accept-new \
         -o UserKnownHostsFile=/dev/null \
         -o LogLevel=ERROR \
         '{src}' '{dst_host}:{dst_path}'",
        recurse = recurse,
        port_arg = port_arg,
        src = shell_single_quote(remote_path),
        dst_host = dest_host,
        dst_path = shell_single_quote(destination_path),
    );

    log::info!(
        "执行 agent forwarding + scp: src={} dst={}@{}:{} recurse={} port={}",
        remote_path,
        destination_config.username,
        destination_config.host,
        destination_path,
        is_directory,
        destination_config.port
    );

    let started = std::time::Instant::now();
    let cmd_result =
        source_session.execute_command_with_agent_forward(&scp_cmd, 3600, |data: &[u8]| {
            // scp 本身不直接报字节进度；此处预留给未来替换为 `rsync --info=progress2`。
            if !data.is_empty() && log::log_enabled!(log::Level::Trace) {
                log::trace!("scp stderr: {}", String::from_utf8_lossy(data).trim_end());
            }
        })?;

    if cmd_result.success() {
        let elapsed = started.elapsed();
        // scp 没有报告传输字节数；在成功后用源端 metadata 补齐 summary/progress。
        let total = if is_directory {
            remote_total_size(source_session, remote_path)
        } else {
            source_session.metadata(remote_path)?.size
        };
        let secs = elapsed.as_secs_f64().max(0.001);
        log::info!(
            "agent forwarding + scp 传输成功: {} 字节 / {:.2}s ≈ {:.2} MiB/s",
            total,
            secs,
            (total as f64) / secs / (1024.0 * 1024.0)
        );
        if let Some(p) = progress {
            p.fetch_add(total, Ordering::Relaxed);
        }
        if is_directory {
            Ok(TransferSummary {
                files_transferred: 0,
                bytes_transferred: total,
            })
        } else {
            Ok(TransferSummary::one_file(total))
        }
    } else {
        let stderr = cmd_result.stderr.trim();
        log::warn!(
            "agent forwarding + scp 失败 (exit={:?}): {}",
            cmd_result.exit_status,
            stderr
        );
        bail!(
            "scp failed: {}",
            if stderr.is_empty() {
                "no stderr"
            } else {
                stderr
            }
        )
    }
}

/// 转义路径作为 shell 单引号字符串的参数：`a'b` → `'a'\''b'`
fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

/// 返回本机 `~/.ssh` 下可直接用于 `scp -i` 的默认密钥列表（按算法偏好排序）。
///
/// 筛选条件：
/// - 文件存在且能读。
/// - 能通过 `russh::keys::load_secret_key(path, None)` 解析 —— 这相当于"没有
///   passphrase"或"OpenSSH 格式允许无密码解密"；否则 scp 会在远程端阻塞等输入。
///
/// 顺序与 [`crate::ssh::find_default_ssh_keys`] 保持一致（ed25519 > rsa > ecdsa > dsa），
/// 这样 scp 的尝试顺序与 DeepTerm 自己连接时的 `try_default_keys` 顺序一致。
fn usable_default_keys() -> Vec<PathBuf> {
    let mut result: Vec<PathBuf> = Vec::new();
    for key_path in crate::ssh::find_default_ssh_keys() {
        match russh::keys::load_secret_key(&key_path, None) {
            Ok(_) => result.push(key_path),
            Err(e) => {
                log::debug!(
                    "默认密钥 {} 不能无 passphrase 加载，跳过 scp 直连候选: {}",
                    key_path.display(),
                    e
                );
            }
        }
    }
    result
}

/// 递归统计远程路径（文件或目录）的总字节数。
///
/// 仅用于 scp 直连路径事后补齐进度，失败时返回 0 以免阻塞 summary 生成——
/// 反正 UI 会退回到"传输完成"的状态。
fn remote_total_size(session: &mut sftp_fs::BlockingSftpSession, path: &str) -> u64 {
    fn walk(session: &mut sftp_fs::BlockingSftpSession, path: &str) -> anyhow::Result<u64> {
        let meta = session.metadata(path)?;
        match meta.kind {
            RemoteFileKind::File => Ok(meta.size),
            RemoteFileKind::Directory => {
                let mut total = 0u64;
                for entry in session.read_dir(path)? {
                    total = total.saturating_add(match entry.kind {
                        RemoteFileKind::File => entry.size,
                        RemoteFileKind::Directory => walk(session, &entry.path).unwrap_or(0),
                        _ => 0,
                    });
                }
                Ok(total)
            }
            _ => Ok(0),
        }
    }
    walk(session, path).unwrap_or(0)
}

/// 通过 **临时密钥 + scp** 实现服务器直连（方案 3b）。
/// 将一把本地可读的私钥临时上传到源服务器，在源服务器执行 scp 命令直接传输到目标。
///
/// 调用方负责选择 `key_path`：可以是目标连接配置里的密钥，也可以是
/// `~/.ssh` 下的默认密钥（当目标使用 Agent/Auto 认证、且 ssh-agent 空时，DeepTerm
/// 自己连接时也是用同一把默认密钥认过的 —— 同一把密钥就能让 scp 直接认过）。
///
/// # 密钥生命周期
///
/// 由 [`scp_key_lease`] 模块管理：同一组 `(源, 目标, 本地 key)` 的并发 / 连续
/// 传输任务复用同一份远端临时密钥。引用计数归零（最后一个任务结束）时才
/// 同步清理源端目录和本地临时文件。详见 `scp_key_lease.rs` 的文档注释。
///
/// # 安全措施
///
/// - 仅支持无 passphrase 的私钥（远程端无 TTY 输入密码）；调用方应预筛。
/// - 在源服务器创建权限受限的临时目录（`chmod 700`），文件 `chmod 600`。
/// - 所有活跃租约引用归零后 `rm -rf` 临时目录；本地临时文件同时用零覆盖再删。
/// - 进程启动后每个源端首次使用时兜底清扫 60 分钟前的 `.deepterm-scp-*` 残留。
/// - 使用 `StrictHostKeyChecking=accept-new` 替代 `no`，拒绝已变更的主机密钥。
fn copy_via_remote_scp(
    source_session: &mut sftp_fs::BlockingSftpSession,
    source_config: &SshConnectionConfig,
    remote_path: &str,
    destination_config: &SshConnectionConfig,
    destination_path: &str,
    key_path: &Path,
    is_directory: bool,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;

    // 调用方（`detect_transfer_strategies` 内部）负责筛出可解析的无 passphrase 密钥；
    // 这里不再重新解析 auth_method —— Agent/Auto 认证下也无 auth 可解析。
    log::debug!("远程 scp 使用本地密钥: {}", key_path.display());

    // 申请租约：命中则 ref_count + 1 并直接复用；未命中则上传 + 登记。
    let lease = crate::file_transfer::scp_key_lease::acquire_lease(
        source_session,
        source_config,
        destination_config,
        key_path,
        cancelled,
    )?;
    let remote_key_path = lease.remote_key_path().to_string();

    // 构建 scp 命令
    //   accept-new：接受新主机密钥但拒绝已变更的密钥（比 no 更安全）
    //   UserKnownHostsFile=/dev/null：不在源服务器上持久化主机密钥
    //   -p：保留权限与时间戳（目录场景下尤其重要）
    let dest_host = format!(
        "{}@{}",
        destination_config.username, destination_config.host
    );

    log::warn!(
        "远程 scp 直连使用 trust-on-first-use 主机密钥验证 \
        （StrictHostKeyChecking=accept-new），第一次连接无法防范 MITM 攻击"
    );

    let recurse = if is_directory { "-r " } else { "" };
    let port_flag = if destination_config.port == 22 {
        String::new()
    } else {
        format!("-P {} ", destination_config.port)
    };

    let scp_cmd = format!(
        "scp {recurse}{port_flag}-p -B \
         -o StrictHostKeyChecking=accept-new \
         -o UserKnownHostsFile=/dev/null \
         -o LogLevel=ERROR \
         -i '{key}' '{src}' '{dst_host}:{dst_path}'",
        recurse = recurse,
        port_flag = port_flag,
        key = shell_single_quote(&remote_key_path),
        src = shell_single_quote(remote_path),
        dst_host = dest_host,
        dst_path = shell_single_quote(destination_path),
    );

    log::info!(
        "执行远程 scp{}（临时密钥）: src={} dst={}@{}:{} port={}",
        if is_directory { " -r" } else { "" },
        remote_path,
        destination_config.username,
        destination_config.host,
        destination_path,
        destination_config.port
    );

    let started = std::time::Instant::now();
    let result = source_session.execute_command(&scp_cmd, 3600);
    // 注意：lease 此时仍在作用域内，Drop 会在本函数返回后触发 ref_count - 1。
    // 归零时同步执行 `rm -rf` + 本地清理；否则由最后一个引用者负责。

    match result {
        Ok(cmd_result) => {
            if cmd_result.success() {
                let elapsed = started.elapsed();
                let total = if is_directory {
                    remote_total_size(source_session, remote_path)
                } else {
                    source_session.metadata(remote_path)?.size
                };
                let secs = elapsed.as_secs_f64().max(0.001);
                log::info!(
                    "远程 scp 传输成功（临时密钥）: {} 字节 / {:.2}s ≈ {:.2} MiB/s",
                    total,
                    secs,
                    (total as f64) / secs / (1024.0 * 1024.0)
                );
                if let Some(p) = progress {
                    p.fetch_add(total, Ordering::Relaxed);
                }
                if is_directory {
                    Ok(TransferSummary {
                        files_transferred: 0,
                        bytes_transferred: total,
                    })
                } else {
                    Ok(TransferSummary::one_file(total))
                }
            } else {
                log::warn!(
                    "远程 scp 失败 (exit={:?}): stdout={}, stderr={}",
                    cmd_result.exit_status,
                    cmd_result.stdout,
                    cmd_result.stderr
                );
                bail!("远程 scp 失败: {}", cmd_result.stderr.trim())
            }
        }
        Err(e) => {
            log::warn!("远程 scp 执行失败: {}", e);
            Err(e)
        }
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
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    fs::create_dir_all(destination_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(source_dir) {
            let mode = metadata.permissions().mode();
            let _ = fs::set_permissions(destination_dir, fs::Permissions::from_mode(mode));
        }
    }

    let mut summary = TransferSummary::default();

    for entry in fs::read_dir(source_dir)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        let source_path = entry.path();
        let metadata = entry.metadata()?;
        let destination_path = destination_dir.join(entry.file_name());

        if metadata.is_file() {
            let destination_path =
                resolve_local_conflict(&destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(copy_local_file_to_path(
                &source_path,
                &destination_path,
                cancelled,
            )?);
        } else if metadata.is_dir() {
            let destination_path =
                resolve_local_conflict(&destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(copy_local_dir_to_path(
                &source_path,
                &destination_path,
                conflict_policy,
                cancelled,
                resolver,
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
    #[cfg(unix)]
    let source_permissions = fs::metadata(source_path)
        .ok()
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()));

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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = source_permissions {
            let _ = fs::set_permissions(destination_path, std::fs::Permissions::from_mode(mode));
        }
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
            let destination_path =
                resolve_remote_conflict(session, &destination_path, conflict_policy, resolver)?
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
                resolve_remote_conflict(session, &destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            session.create_dir_all(&destination_path)?;
            summary.merge(upload_local_dir_to_remote_path(
                session,
                &source_path,
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

fn upload_local_file_to_remote_path(
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
        return Err(anyhow!("upload failed: {}", error));
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

fn download_remote_dir_to_local_path(
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
            let destination_path =
                resolve_local_conflict(&destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            summary.merge(download_remote_file_to_local_path(
                session,
                &entry.path,
                &destination_path,
                cancelled,
                progress,
            )?);
        } else if entry.kind == RemoteFileKind::Directory {
            let destination_path =
                resolve_local_conflict(&destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
            fs::create_dir_all(&destination_path)?;
            summary.merge(download_remote_dir_to_local_path(
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

fn download_remote_file_to_local_path(
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
        return Err(anyhow!("download failed: {}", error));
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

fn copy_remote_dir_to_remote_path(
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
                resolve_remote_conflict(session, &destination_path, conflict_policy, resolver)?
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
                resolve_remote_conflict(session, &destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("transfer skipped"))?;
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
fn copy_remote_file_to_remote_path(
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

fn copy_remote_dir_between_sessions(
    source_session: &mut sftp_fs::BlockingSftpSession,
    remote_dir: &str,
    destination_session: &mut sftp_fs::BlockingSftpSession,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    destination_session.create_dir_all(destination_dir)?;

    if let Ok(Some(mode)) = source_session.permissions(remote_dir) {
        let _ = destination_session.set_permissions(destination_dir, mode);
    }

    let mut summary = TransferSummary::default();

    for entry in source_session.read_dir(remote_dir)? {
        check_cancelled(cancelled)?;
        let destination_path = sftp_fs::join_remote_path(destination_dir, &entry.name);

        if entry.kind == RemoteFileKind::File {
            let destination_path = resolve_remote_conflict(
                destination_session,
                &destination_path,
                conflict_policy,
                resolver,
            )?
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
            let destination_path = resolve_remote_conflict(
                destination_session,
                &destination_path,
                conflict_policy,
                resolver,
            )?
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
                resolver,
            )?);
        }
    }

    Ok(summary)
}

/// 跨服务器远程文件复制：从源 session 流式传输到目标 session，不写本地磁盘。
/// 通过 `stream_transfer_to` 在内存中直接中转数据，源端读取与目的端写入交替执行。
fn copy_remote_file_between_sessions(
    source_session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_session: &mut sftp_fs::BlockingSftpSession,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    let source_permissions = source_session.permissions(remote_path).ok().flatten();

    // 使用临时文件避免传输中断时污染目标路径
    let temporary_path = temporary_remote_path(destination_path);
    if destination_session.exists(&temporary_path)? {
        let _ = destination_session.remove_file(&temporary_path);
    }

    // 核心：内存流式传输，不走本地磁盘
    let transferred = source_session.stream_transfer_to(
        remote_path,
        destination_session,
        &temporary_path,
        cancelled,
        progress,
    );

    // 传输完成后重命名为最终文件名，失败时清理临时文件
    match transferred {
        Ok(bytes) => {
            destination_session.rename(&temporary_path, destination_path)?;
            if let Some(mode) = source_permissions {
                let _ = destination_session.set_permissions(destination_path, mode);
            }
            Ok(TransferSummary::one_file(bytes))
        }
        Err(e) => {
            let _ = destination_session.remove_file(&temporary_path);
            Err(e)
        }
    }
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
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<Option<PathBuf>> {
    if !desired_path.exists() {
        return Ok(Some(desired_path.to_path_buf()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_path_buf())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => {
            let resolver = resolver
                .ok_or_else(|| anyhow::anyhow!("conflict resolver required for Ask policy"))?;
            let (tx, rx) = async_channel::bounded(1);
            let file_name = desired_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            resolver.prompt(ConflictPrompt {
                file_name,
                destination_path: desired_path.to_string_lossy().to_string(),
                response: tx,
            });

            match wait_for_resolution(&rx)? {
                ConflictResolution::Skip => Ok(None),
                ConflictResolution::Overwrite => Ok(Some(desired_path.to_path_buf())),
                ConflictResolution::AutoRename => {
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
                ConflictResolution::Stop => bail!("{}", TRANSFER_CANCELLED_MSG),
            }
        }
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
    let suffix = if index == 1 {
        "_副本".to_string()
    } else {
        format!("_副本{}", index - 1)
    };
    let file_name = match extension {
        Some(extension) => format!("{}{}.{}", stem, suffix, extension),
        None => format!("{}{}", stem, suffix),
    };
    parent.join(file_name)
}

fn resolve_remote_conflict(
    session: &mut sftp_fs::BlockingSftpSession,
    desired_path: &str,
    conflict_policy: ConflictPolicy,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<Option<String>> {
    if !session.exists(desired_path)? {
        return Ok(Some(desired_path.to_string()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_string())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => {
            let resolver = resolver
                .ok_or_else(|| anyhow::anyhow!("conflict resolver required for Ask policy"))?;
            let (tx, rx) = async_channel::bounded(1);
            let file_name = remote_file_name(desired_path).unwrap_or_else(|_| "file".to_string());
            resolver.prompt(ConflictPrompt {
                file_name,
                destination_path: desired_path.to_string(),
                response: tx,
            });

            match wait_for_resolution(&rx)? {
                ConflictResolution::Skip => Ok(None),
                ConflictResolution::Overwrite => Ok(Some(desired_path.to_string())),
                ConflictResolution::AutoRename => {
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
                ConflictResolution::Stop => bail!("{}", TRANSFER_CANCELLED_MSG),
            }
        }
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
    let suffix = if index == 1 {
        "_副本".to_string()
    } else {
        format!("_副本{}", index - 1)
    };
    let candidate_name = match extension {
        Some(extension) => format!("{}{}.{}", stem, suffix, extension),
        None => format!("{}{}", stem, suffix),
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
        bail!("{}", TRANSFER_CANCELLED_MSG);
    }
    Ok(())
}
