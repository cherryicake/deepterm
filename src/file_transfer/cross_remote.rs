use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context as _, anyhow, bail};

use crate::file_transfer::utils::{
    TRANSFER_CANCELLED_MSG, TransferSummary, check_cancelled, remote_file_name,
    remove_remote_source, resolve_remote_conflict, shell_single_quote, temporary_remote_path,
};
use crate::file_transfer::{ConflictPolicy, ConflictResolver};
use crate::ssh::SshConnectionConfig;
use crate::ssh::keychain::{ResolvedAuthMethod, resolve_auth_method};
use crate::ssh::sftp_fs::{self, BlockingSftpSession, RemoteFileKind};

/// 状态报告器：用于将 scp 执行过程中的状态信息实时回传到 UI
pub trait TransferStatusReporter: Send + Sync {
    fn report(&self, message: String);
    fn set_total_bytes(&self, _bytes: u64) {}
}

/// 空实现，用于不需要状态报告的场景
pub struct NoopReporter;
impl TransferStatusReporter for NoopReporter {
    fn report(&self, _message: String) {}
}

fn report_status(reporter: Option<&dyn TransferStatusReporter>, message: impl Into<String>) {
    if let Some(r) = reporter {
        r.report(message.into());
    }
}

/// 解析并精简 scp stderr 输出，提取有用的状态/错误信息
fn report_scp_stderr(reporter: Option<&dyn TransferStatusReporter>, data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("debug1:") || trimmed.starts_with("debug2:") {
            continue;
        }
        // 过滤掉过于内部的 OpenSSH 信息，保留用户关心的内容
        if trimmed.starts_with("OpenSSH")
            || trimmed.starts_with("Authenticated")
            || trimmed.starts_with("Connection")
            || trimmed.starts_with("Transferred")
            || trimmed.starts_with("Sending file")
            || trimmed.starts_with("Sink")
            || trimmed.starts_with("scp:")
            || trimmed.starts_with("ssh:")
        {
            report_status(reporter, trimmed);
        }
    }
}

/// 远程到远程传输策略
#[derive(Clone, Debug, PartialEq)]
enum RemoteToRemoteStrategy {
    /// 在源服务器上执行 scp，通过 SSH agent forwarding 鉴权目标端
    AgentForwardScp,
    /// 把一把本地私钥临时上传到源端，在源端执行 scp 直接传输
    TempKeyScp { key_path: PathBuf },
    /// 流式传输（数据经过 DeepTerm 内存中转）
    StreamViaLocal,
}

#[derive(Clone, Debug)]
struct RemoteOverwriteBackup {
    original_path: String,
    backup_path: String,
}

fn prepare_remote_overwrite_backup(
    destination_config: &SshConnectionConfig,
    destination_path: &str,
) -> anyhow::Result<Option<RemoteOverwriteBackup>> {
    sftp_fs::with_cached_session(destination_config, |session| {
        if !session.exists(destination_path)? {
            return Ok(None);
        }

        let backup_path = unique_remote_backup_path(session, destination_path)?;
        session
            .rename(destination_path, &backup_path)
            .with_context(|| {
                format!(
                    "目标文件已存在，但无法安全备份以便直连替换: {}",
                    destination_path
                )
            })?;
        log::info!(
            "目标端替换前备份完成: {} -> {}",
            destination_path,
            backup_path
        );
        Ok(Some(RemoteOverwriteBackup {
            original_path: destination_path.to_string(),
            backup_path,
        }))
    })
}

fn cleanup_remote_overwrite_backup(
    destination_config: &SshConnectionConfig,
    backup: Option<&RemoteOverwriteBackup>,
) {
    let Some(backup) = backup else {
        return;
    };
    if let Err(e) = sftp_fs::with_cached_session(destination_config, |session| {
        remove_remote_source(session, &backup.backup_path)
    }) {
        log::warn!("目标端替换备份清理失败: {} ({})", backup.backup_path, e);
    }
}

fn restore_remote_overwrite_backup(
    destination_config: &SshConnectionConfig,
    backup: Option<&RemoteOverwriteBackup>,
) -> anyhow::Result<()> {
    let Some(backup) = backup else {
        return Ok(());
    };

    sftp_fs::with_cached_session(destination_config, |session| {
        if session.exists(&backup.original_path)? {
            remove_remote_source(session, &backup.original_path)?;
        }
        if session.exists(&backup.backup_path)? {
            session.rename(&backup.backup_path, &backup.original_path)?;
            log::info!(
                "目标端替换失败后已恢复备份: {} -> {}",
                backup.backup_path,
                backup.original_path
            );
        }
        Ok(())
    })
}

fn unique_remote_backup_path(
    session: &mut BlockingSftpSession,
    destination_path: &str,
) -> anyhow::Result<String> {
    let parent = sftp_fs::remote_parent(destination_path).unwrap_or_else(|| "/".to_string());
    let file_name = remote_file_name(destination_path).unwrap_or_else(|_| "transfer".to_string());
    for index in 0..10_000 {
        let suffix = if index == 0 {
            ".deepterm-backup".to_string()
        } else {
            format!(".deepterm-backup{}", index)
        };
        let candidate = sftp_fs::join_remote_path(&parent, &format!(".{}{}", file_name, suffix));
        if !session.exists(&candidate)? {
            return Ok(candidate);
        }
    }
    bail!(
        "unable to find available remote backup name for {}",
        destination_path
    )
}

pub fn is_safe_for_direct_scp(path: &str) -> bool {
    !path
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | b'\0' | b'\t' | b'\x08' | b'\x1b'))
}

/// SCP 直连传输失败后，尝试清理目标端可能残留的不完整文件。
///
/// SCP 协议直接写入目标路径（不经过临时文件），失败后目标端可能留下
/// 部分写入的文件。此函数连接目标端并尝试删除该文件，仅在文件存在时
/// 执行清理，失败时仅记录警告不传播错误（清理失败不影响主流程）。
fn cleanup_partial_scp_destination(
    destination_config: &SshConnectionConfig,
    destination_path: &str,
) {
    let _ = sftp_fs::with_cached_session(destination_config, |session| {
        if session.exists(destination_path)? {
            match crate::file_transfer::utils::remove_remote_source(session, destination_path) {
                Ok(()) => {
                    log::info!(
                        "已清理 SCP 目标端残留文件: {}@{}:{}",
                        destination_config.username,
                        destination_config.host,
                        destination_path,
                    );
                }
                Err(e) => {
                    log::warn!(
                        "清理 SCP 目标端残留文件失败: {}@{}:{} ({})",
                        destination_config.username,
                        destination_config.host,
                        destination_path,
                        e,
                    );
                }
            }
        }
        Ok(())
    });
}

pub fn remote_scp_timeout_secs() -> u64 {
    std::env::var("DEEPTERM_REMOTE_SCP_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.clamp(60, 86_400))
        .unwrap_or(86_400)
}

pub fn copy_remote_to_different_remote(
    source_config: &SshConnectionConfig,
    remote_path: &str,
    destination_config: &SshConnectionConfig,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    rename_to: Option<&str>,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
    reporter: Option<&dyn TransferStatusReporter>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;
    let file_name = remote_file_name(remote_path)?;

    sftp_fs::with_cached_session(source_config, |source_session| {
        let metadata = source_session.metadata(remote_path)?;
        if metadata.kind == RemoteFileKind::File {
            if let Some(r) = reporter {
                r.set_total_bytes(metadata.size);
            }
        }
        let is_directory = metadata.kind == RemoteFileKind::Directory;
        let effective_name = if is_directory {
            file_name.as_str()
        } else {
            rename_to.unwrap_or(&file_name)
        };
        let desired_path = sftp_fs::join_remote_path(destination_dir, effective_name);

        sftp_fs::with_cached_session(destination_config, |destination_session| {
            destination_session.create_dir_all(destination_dir)
        })?;

        let resolved_destination =
            sftp_fs::with_cached_session(destination_config, |destination_session| {
                resolve_remote_conflict(
                    destination_session,
                    &desired_path,
                    conflict_policy,
                    resolver,
                    cancelled,
                )
            })?;

        let destination_path = match resolved_destination {
            Some(path) => path,
            None => return Ok(TransferSummary::default()),
        };

        let overwrite_backup =
            prepare_remote_overwrite_backup(destination_config, &destination_path)?;
        let direct_paths_are_safe =
            is_safe_for_direct_scp(remote_path) && is_safe_for_direct_scp(&destination_path);

        const SMALL_FILE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB
        let force_stream = metadata.kind == RemoteFileKind::File && metadata.size < SMALL_FILE_THRESHOLD;
        if force_stream {
            log::info!(
                "文件大小 {:.2} MiB 小于 10MB，跳过直连尝试，直接使用本地中转传输",
                metadata.size as f64 / (1024.0 * 1024.0)
            );
        }

        let strategies = if force_stream {
            vec![RemoteToRemoteStrategy::StreamViaLocal]
        } else {
            detect_transfer_strategies(source_session, destination_config)
        };

        let mut tried_labels: Vec<&str> = Vec::new();
        let mut failure_reasons: Vec<String> = Vec::new();

        for strategy in &strategies {
            if cancelled.load(Ordering::Relaxed) {
                restore_remote_overwrite_backup(destination_config, overwrite_backup.as_ref())?;
                bail!("{}", TRANSFER_CANCELLED_MSG);
            }

            let label = match strategy {
                RemoteToRemoteStrategy::AgentForwardScp => "直连(agent forwarding)",
                RemoteToRemoteStrategy::TempKeyScp { .. } => "直连(密钥)",
                RemoteToRemoteStrategy::StreamViaLocal => "本地中转传输",
            };

            // 构造当前状态文本：首次显示 "XXX..."，失败后追加 "XXX 失败 尝试 YYY..."
            let status_text = if tried_labels.is_empty() {
                format!("{}...", label)
            } else {
                format!("{} 失败 尝试 {}...", tried_labels.last().unwrap(), label)
            };
            report_status(reporter, status_text);

            let result = match strategy {
                RemoteToRemoteStrategy::AgentForwardScp if direct_paths_are_safe => {
                    copy_via_agent_forward_scp(
                        source_session,
                        remote_path,
                        destination_config,
                        &destination_path,
                        is_directory,
                        cancelled,
                        progress,
                        reporter,
                    )
                }
                RemoteToRemoteStrategy::AgentForwardScp => {
                    Err(anyhow!("路径包含不适合远程 scp 直连的控制字符"))
                }
                RemoteToRemoteStrategy::TempKeyScp { key_path } if direct_paths_are_safe => {
                    copy_via_remote_scp(
                        source_session,
                        source_config,
                        remote_path,
                        destination_config,
                        &destination_path,
                        key_path,
                        is_directory,
                        cancelled,
                        progress,
                        reporter,
                    )
                }
                RemoteToRemoteStrategy::TempKeyScp { .. } => {
                    Err(anyhow!("路径包含不适合远程 scp 直连的控制字符"))
                }
                RemoteToRemoteStrategy::StreamViaLocal => {
                    sftp_fs::with_cached_session(destination_config, |destination_session| {
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
                            bail!("不支持的远程文件类型: {}", remote_path)
                        }
                    })
                }
            };

            match result {
                Ok(summary) => {
                    cleanup_remote_overwrite_backup(destination_config, overwrite_backup.as_ref());
                    return Ok(summary);
                }
                Err(e) => {
                    failure_reasons.push(format!("{}: {}", label, e));
                    tried_labels.push(label);
                    match strategy {
                        RemoteToRemoteStrategy::AgentForwardScp => {
                            log::warn!("方案 3a (agent forwarding scp) 失败，尝试下一策略: {}", e);
                        }
                        RemoteToRemoteStrategy::TempKeyScp { key_path } => {
                            log::warn!(
                                "方案 3b (临时密钥 scp, key={}) 失败，尝试下一策略: {}",
                                key_path.display(),
                                e
                            );
                        }
                        RemoteToRemoteStrategy::StreamViaLocal => {
                            log::warn!("流式传输失败，准备恢复目标端备份: {}", e);
                        }
                    }
                }
            }
        }

        restore_remote_overwrite_backup(destination_config, overwrite_backup.as_ref())?;
        let combined = if failure_reasons.is_empty() {
            "所有传输策略均失败".to_string()
        } else {
            failure_reasons.join(" | ")
        };
        report_status(reporter, format!("全部失败: {}", combined));
        Err(anyhow!(combined))
    })
}

fn detect_transfer_strategies(
    source_session: &mut BlockingSftpSession,
    destination_config: &SshConnectionConfig,
) -> Vec<RemoteToRemoteStrategy> {
    let mut strategies = Vec::new();

    if let Ok(forced) = std::env::var("DEEPTERM_REMOTE_TRANSFER_STRATEGY") {
        log::info!("检测到 DEEPTERM_REMOTE_TRANSFER_STRATEGY={}", forced);
        match forced.as_str() {
            "3a" | "agent" => {
                return vec![
                    RemoteToRemoteStrategy::AgentForwardScp,
                    RemoteToRemoteStrategy::StreamViaLocal,
                ];
            }
            "3b" | "tempkey" => {
                let mut forced_strategies = Vec::new();
                if let Some(cached) = crate::ssh::auth::lookup_authenticated_key(destination_config)
                {
                    forced_strategies.push(RemoteToRemoteStrategy::TempKeyScp { key_path: cached });
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
                forced_strategies.push(RemoteToRemoteStrategy::StreamViaLocal);
                return forced_strategies;
            }
            "pipeline" | "stream" => return vec![RemoteToRemoteStrategy::StreamViaLocal],
            "auto" | _ => {}
        }
    }

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
            check_result
                .as_ref()
                .map(|r| r.stdout.trim())
                .unwrap_or("<err>")
        );
        strategies.push(RemoteToRemoteStrategy::StreamViaLocal);
        return strategies;
    }
    log::debug!("源服务器具备 scp/ssh 命令");

    let resolved = resolve_auth_method(&destination_config.auth_method);
    log::info!(
        "目标服务器({}@{}:{}) 认证方式解析结果: {}",
        destination_config.username,
        destination_config.host,
        destination_config.port,
        match &resolved {
            Ok(ResolvedAuthMethod::PrivateKey {
                path, passphrase, ..
            }) => format!(
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
            if let Some(cached) = crate::ssh::auth::lookup_authenticated_key(destination_config) {
                log::info!(
                    "候选策略: 临时密钥 + scp 直连，复用连接时认过的密钥 {}（数据不经 DeepTerm）",
                    cached.display()
                );
                strategies.push(RemoteToRemoteStrategy::TempKeyScp { key_path: cached });
            } else {
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
    log::info!("最终远程到远程传输策略顺序: {:?}", strategies);
    strategies
}

fn copy_via_agent_forward_scp(
    source_session: &mut BlockingSftpSession,
    remote_path: &str,
    destination_config: &SshConnectionConfig,
    destination_path: &str,
    is_directory: bool,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    reporter: Option<&dyn TransferStatusReporter>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;

    let dest_host = format!(
        "{}@{}",
        destination_config.username, destination_config.host
    );

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
         -o ConnectTimeout=10 \
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

    // 后台进度监控：scp 执行期间每秒轮询目标大小，直接写入 UI 共享的 progress。
    // 用 thread::scope 让监控线程借用真实的 &AtomicU64，无需中转 Arc（之前写到
    // 临时 Arc 再在结束后对账，导致进度条整个传输期间不动、完成瞬间才跳变）。
    let started = std::time::Instant::now();
    let stop_monitor = AtomicBool::new(false);
    let cmd_result = std::thread::scope(|scope| {
        if let Some(p) = progress {
            let stop_ref = &stop_monitor;
            let cancelled_ref: &AtomicBool = cancelled;
            scope.spawn(move || {
                monitor_dest_progress(
                    destination_config,
                    destination_path,
                    is_directory,
                    p,
                    stop_ref,
                    cancelled_ref,
                );
            });
        }
        let r = source_session.execute_command_with_agent_forward_and_cancel(
            &scp_cmd,
            remote_scp_timeout_secs(),
            cancelled,
            |data: &[u8]| {
                report_scp_stderr(reporter, data);
                if !data.is_empty() && log::log_enabled!(log::Level::Trace) {
                    log::trace!("scp stderr: {}", String::from_utf8_lossy(data).trim_end());
                }
            },
        );
        // 通知监控线程退出；scope 在闭包结束时自动 join
        stop_monitor.store(true, Ordering::Relaxed);
        r
    });

    let cmd_result = cmd_result?;

    if cmd_result.success() {
        let elapsed = started.elapsed();
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
        report_status(
            reporter,
            format!(
                "完成 {:.2} MiB/s",
                (total as f64) / secs / (1024.0 * 1024.0)
            ),
        );
        if let Some(p) = progress {
            // 修正最终进度（避免轮询偏差）
            let current = p.load(Ordering::Relaxed);
            if current < total {
                p.fetch_add(total - current, Ordering::Relaxed);
            }
        }
        if is_directory {
            Ok(TransferSummary {
                files_transferred: remote_total_files(source_session, remote_path),
                bytes_transferred: total,
                skipped: 0,
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
        report_status(
            reporter,
            format!(
                "失败: {}",
                if stderr.is_empty() {
                    "无错误输出".to_string()
                } else {
                    stderr.to_string()
                }
            ),
        );
        // SCP 直连失败：目标端可能残留不完整文件，尝试清理新建的文件
        if !is_directory {
            cleanup_partial_scp_destination(destination_config, destination_path);
        }
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

/// 后台进度监控：scp 执行期间，在一条独立 SSH 连接上每秒轮询目标文件/目录的
/// 当前大小，并把增量写入 `progress`（UI 共享的同一个 atomic，从而实时更新进度条）。
///
/// - 文件：直接 `metadata().size`
/// - 目录：`du -sb`（GNU coreutils 扩展，BSD/macOS/busybox 不支持 `-b`；不可用时
///   `|| echo 0` 兜底，目录进度会停在 0，但不影响传输本身与最终对账）
///
/// `stop` 置位（scp 已返回）或 `cancelled` 置位时退出。
fn monitor_dest_progress(
    destination_config: &SshConnectionConfig,
    destination_path: &str,
    is_directory: bool,
    progress: &AtomicU64,
    stop: &AtomicBool,
    cancelled: &AtomicBool,
) {
    let Ok(mut session) = sftp_fs::BlockingSftpSession::connect(destination_config) else {
        return;
    };
    let mut last_size = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) || cancelled.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        let size = if is_directory {
            let cmd = format!(
                "du -sb '{}' 2>/dev/null || echo 0",
                shell_single_quote(destination_path)
            );
            match session.execute_command(&cmd, 5) {
                Ok(result) if result.success() => result.stdout.trim().parse::<u64>().ok(),
                _ => None,
            }
        } else {
            session
                .metadata(destination_path)
                .ok()
                .map(|meta| meta.size)
        };
        if let Some(size) = size {
            if size > last_size {
                progress.fetch_add(size - last_size, Ordering::Relaxed);
                last_size = size;
            }
        }
    }
}

fn remote_total_size(session: &mut BlockingSftpSession, path: &str) -> u64 {
    fn walk(session: &mut BlockingSftpSession, path: &str) -> anyhow::Result<u64> {
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

/// 统计远端路径下的普通文件数（用于 scp 直连传输后填充 summary 的 files_transferred，
/// 否则目录传输 UI 永远显示 "0 个文件"）。符号链接 / 特殊文件不计入。
fn remote_total_files(session: &mut BlockingSftpSession, path: &str) -> usize {
    fn walk(session: &mut BlockingSftpSession, path: &str) -> anyhow::Result<usize> {
        let meta = session.metadata(path)?;
        match meta.kind {
            RemoteFileKind::File => Ok(1),
            RemoteFileKind::Directory => {
                let mut total = 0usize;
                for entry in session.read_dir(path)? {
                    total = total.saturating_add(match entry.kind {
                        RemoteFileKind::File => 1,
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

fn copy_via_remote_scp(
    source_session: &mut BlockingSftpSession,
    source_config: &SshConnectionConfig,
    remote_path: &str,
    destination_config: &SshConnectionConfig,
    destination_path: &str,
    key_path: &std::path::Path,
    is_directory: bool,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    reporter: Option<&dyn TransferStatusReporter>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;

    log::debug!("远程 scp 使用本地密钥: {}", key_path.display());

    let lease = crate::file_transfer::scp_key_lease::acquire_lease(
        source_session,
        source_config,
        destination_config,
        key_path,
        cancelled,
    )?;
    let remote_key_path = lease.remote_key_path().to_string();

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
         -o ConnectTimeout=10 \
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

    // 后台进度监控：同 copy_via_agent_forward_scp，借用真实 progress 直接写。
    let started = std::time::Instant::now();
    let stop_monitor = AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        if let Some(p) = progress {
            let stop_ref = &stop_monitor;
            let cancelled_ref: &AtomicBool = cancelled;
            scope.spawn(move || {
                monitor_dest_progress(
                    destination_config,
                    destination_path,
                    is_directory,
                    p,
                    stop_ref,
                    cancelled_ref,
                );
            });
        }
        let r = source_session.execute_command_with_cancel(
            &scp_cmd,
            remote_scp_timeout_secs(),
            cancelled,
        );
        stop_monitor.store(true, Ordering::Relaxed);
        r
    });

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
                report_status(
                    reporter,
                    format!(
                        "完成 {:.2} MiB/s",
                        (total as f64) / secs / (1024.0 * 1024.0)
                    ),
                );
                if let Some(p) = progress {
                    // 修正最终进度（避免轮询偏差）
                    let current = p.load(Ordering::Relaxed);
                    if current < total {
                        p.fetch_add(total - current, Ordering::Relaxed);
                    }
                }
                if is_directory {
                    Ok(TransferSummary {
                        files_transferred: remote_total_files(source_session, remote_path),
                        bytes_transferred: total,
                        skipped: 0,
                    })
                } else {
                    Ok(TransferSummary::one_file(total))
                }
            } else {
                let stderr = cmd_result.stderr.trim();
                log::warn!(
                    "远程 scp 失败 (exit={:?}): stdout={}, stderr={}",
                    cmd_result.exit_status,
                    cmd_result.stdout,
                    stderr
                );
                report_status(reporter, format!("失败: {}", stderr));
                // SCP 直连失败：目标端可能残留不完整文件，尝试清理新建的文件
                if !is_directory {
                    cleanup_partial_scp_destination(destination_config, destination_path);
                }
                bail!("远程 scp 失败: {}", stderr)
            }
        }
        Err(e) => {
            log::warn!("远程 scp 执行失败: {}", e);
            report_status(reporter, format!("失败: {}", e));
            Err(e)
        }
    }
}

pub fn copy_remote_dir_between_sessions(
    source_session: &mut BlockingSftpSession,
    remote_dir: &str,
    destination_session: &mut BlockingSftpSession,
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
            match resolve_remote_conflict(
                destination_session,
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => summary.merge(copy_remote_file_between_sessions(
                    source_session,
                    &entry.path,
                    destination_session,
                    &path,
                    cancelled,
                    progress,
                )?),
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else if entry.kind == RemoteFileKind::Directory {
            match resolve_remote_conflict(
                destination_session,
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => {
                    destination_session.create_dir_all(&path)?;
                    summary.merge(copy_remote_dir_between_sessions(
                        source_session,
                        &entry.path,
                        destination_session,
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
            log::warn!("跳过不支持的远程条目 ({:?}): {}", entry.kind, entry.path);
            summary.merge(TransferSummary::one_skipped());
        }
    }

    Ok(summary)
}

pub fn copy_remote_file_between_sessions(
    source_session: &mut BlockingSftpSession,
    remote_path: &str,
    destination_session: &mut BlockingSftpSession,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    let source_permissions = source_session.permissions(remote_path).ok().flatten();

    let temporary_path = temporary_remote_path(destination_path);
    if destination_session.exists(&temporary_path)? {
        let _ = destination_session.remove_file(&temporary_path);
    }

    let transferred = source_session.stream_transfer_to(
        remote_path,
        destination_session,
        &temporary_path,
        cancelled,
        progress,
    );

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
