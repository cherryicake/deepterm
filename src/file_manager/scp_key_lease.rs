//! 远程 scp 直连传输的临时密钥租约（refcount-managed lifecycle）。
//!
//! 同一组（源服务器 / 目标服务器 / 本地密钥）在并发或连续的传输任务中复用同一份
//! 上传到源端 `/tmp/.deepterm-scp-*` 的密钥；引用计数归零（最后一个传输任务结束）
//! 时才清理 —— 把"密钥在源端的驻留时间"缩到刚好覆盖活跃任务，
//! 避免每次传输都重复 mkdir/upload/chmod/rm 的固定开销。
//!
//! ## 安全性边界
//!
//! - 远端目录权限 `0700`，密钥文件权限 `0600`；其他用户不可读。
//! - 进程内每个源服务器在第一次 acquire 时跑一次 `find /tmp -maxdepth 1
//!   -name '.deepterm-scp-*' -mmin +60 -exec rm -rf` 兜底清扫历史残留
//!   （针对 DeepTerm 上次崩溃 / kill -9 留下的目录）。
//! - 缓存命中时跑一次 `test -f` 廉价存活检查，被外部清扫（tmpreaper 等）则
//!   作废条目重建。
//! - DeepTerm 正常退出时所有 guard 被 Drop，归零触发同步清理。
//!   崩溃 / kill -9 路径走 60 分钟兜底。

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::utils::shell_single_quote;

use anyhow::{Context as _, anyhow, bail};

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::{self, BlockingSftpSession};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LeaseKey {
    source_key: String,
    dest_key: String,
    local_key_path: PathBuf,
}

fn config_key(config: &SshConnectionConfig) -> String {
    serde_json::to_string(config)
        .unwrap_or_else(|_| format!("{}@{}:{}", config.username, config.host, config.port))
}

struct ScpKeyLease {
    local_temp_path: PathBuf,
    remote_temp_dir: String,
    remote_key_path: String,
    key_size: usize,
    ref_count: usize,
    source_config: SshConnectionConfig,
}

struct Registry {
    leases: Mutex<HashMap<LeaseKey, Arc<Mutex<ScpKeyLease>>>>,
    /// 进程级"已经做过历史残留清扫"的源服务器集合（去重用）。
    cleaned_sources: Mutex<HashSet<String>>,
}

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Registry {
        leases: Mutex::new(HashMap::new()),
        cleaned_sources: Mutex::new(HashSet::new()),
    })
}

/// 由 [`acquire_lease`] 返回；调用方持有 guard 期间，密钥租约的 ref_count 增 1。
/// guard 被 Drop 时自动 ref_count - 1；归零时同步执行清理（源端 `rm -rf` +
/// 本地零覆盖删除）。
pub struct LeaseGuard {
    key: LeaseKey,
    remote_key_path: String,
}

impl LeaseGuard {
    pub fn remote_key_path(&self) -> &str {
        &self.remote_key_path
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let registry = registry();
        let lease_arc = {
            let leases = registry.leases.lock();
            leases.get(&self.key).cloned()
        };
        let Some(lease_arc) = lease_arc else {
            // 已被其他路径（test_f 失败时的失活摘除）从 registry 删掉。
            return;
        };

        // 决定是否要清理：refcount > 1 仅自减；归零时摘出 lease 并交后台线程清理。
        let to_cleanup = {
            let mut lease = lease_arc.lock();
            if lease.ref_count > 1 {
                lease.ref_count -= 1;
                log::info!(
                    "SCP 密钥租约释放: refs={} key={}",
                    lease.ref_count,
                    self.key.local_key_path.display()
                );
                None
            } else {
                let mut leases = registry.leases.lock();
                leases.remove(&self.key);
                Some(ScpKeyLease {
                    local_temp_path: lease.local_temp_path.clone(),
                    remote_temp_dir: lease.remote_temp_dir.clone(),
                    remote_key_path: lease.remote_key_path.clone(),
                    key_size: lease.key_size,
                    ref_count: 0,
                    source_config: lease.source_config.clone(),
                })
            }
        };

        if let Some(lease) = to_cleanup {
            // 关键：清理必须放后台线程跑。
            //
            // Drop 经常发生在 `copy_via_remote_scp` 返回时，而那个函数本身嵌套在
            // `sftp_fs::with_cached_session(source_config, ...)` 的闭包里 —— 此时
            // 源服务器对应的 `Mutex<BlockingSftpSession>` 还由外层闭包持有。
            // 若同步调 `with_cached_session` 做 `rm -rf`，会立即在同一把 Mutex 上
            // 自我死锁，整条传输任务、所有后续 SFTP 操作（包括文件浏览器）都会卡住。
            //
            // 把清理移到独立线程：等外层闭包返回、Mutex 释放后，线程自然拿到锁
            // 完成 `rm -rf`。延迟通常是毫秒级。代价是进程在清理执行前被
            // `kill -9` 时密钥可能短期残留，由 60 分钟兜底清扫负责回收。
            let _ = std::thread::Builder::new()
                .name("scp-key-cleanup".into())
                .spawn(move || cleanup_lease(&lease));
        }
    }
}

fn cleanup_lease(lease: &ScpKeyLease) {
    // 1. 源端 rm -rf 临时目录
    let cmd = format!("rm -rf '{}'", shell_single_quote(&lease.remote_temp_dir));
    match sftp_fs::with_cached_session(&lease.source_config, |session| {
        session.execute_command(&cmd, 5)
    }) {
        Ok(result) if result.success() => {
            log::info!(
                "SCP 密钥租约清理完成: src={}@{} remote_dir={}",
                lease.source_config.username,
                lease.source_config.host,
                lease.remote_temp_dir
            );
        }
        Ok(result) => {
            log::warn!(
                "SCP 密钥租约清理失败: rm -rf {} exit={:?} stderr={}; 源端 /tmp 下可能遗留密钥",
                lease.remote_temp_dir,
                result.exit_status,
                result.stderr.trim()
            );
        }
        Err(e) => {
            log::warn!(
                "SCP 密钥租约清理失败 ({}@{}): {}; 源端 {} 可能遗留密钥",
                lease.source_config.username,
                lease.source_config.host,
                e,
                lease.remote_temp_dir
            );
        }
    }

    // 2. 本地零覆盖 + 删除
    let _ = std::fs::write(&lease.local_temp_path, vec![0u8; lease.key_size]);
    let _ = std::fs::remove_file(&lease.local_temp_path);
}

/// 申请一份临时密钥租约。命中现有租约则 ref_count + 1；否则创建（mkdir + upload +
/// chmod）并登记。返回的 [`LeaseGuard`] 在 Drop 时自动释放。
pub fn acquire_lease(
    source_session: &mut BlockingSftpSession,
    source_config: &SshConnectionConfig,
    destination_config: &SshConnectionConfig,
    key_path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<LeaseGuard> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("transfer cancelled");
    }

    // 进程级一次性历史残留清扫：仅清理 mtime > 60 分钟的目录，避免误删并发实例。
    sweep_stale_temp_dirs_once(source_session, source_config);

    let lease_key = LeaseKey {
        source_key: config_key(source_config),
        dest_key: config_key(destination_config),
        local_key_path: key_path.to_path_buf(),
    };

    // 1) 命中：存活检查 + ref_count + 1
    let cached = registry().leases.lock().get(&lease_key).cloned();
    if let Some(lease_arc) = cached {
        let remote_key_path = lease_arc.lock().remote_key_path.clone();
        if remote_key_alive(source_session, &remote_key_path) {
            let mut lease = lease_arc.lock();
            lease.ref_count += 1;
            log::info!(
                "SCP 密钥租约复用: refs={} src={}@{} dst={}@{} key={}",
                lease.ref_count,
                lease.source_config.username,
                lease.source_config.host,
                destination_config.username,
                destination_config.host,
                key_path.display()
            );
            return Ok(LeaseGuard {
                key: lease_key,
                remote_key_path,
            });
        }
        // 失活：摘除，进入未命中路径重建
        log::warn!(
            "SCP 密钥租约失效（远端密钥已不存在），重新建立: {}",
            remote_key_path
        );
        registry().leases.lock().remove(&lease_key);
    }

    // 2) 未命中：建租约
    let new_lease = create_lease(source_session, source_config, key_path, cancelled)?;
    let remote_key_path = new_lease.remote_key_path.clone();
    log::info!(
        "SCP 密钥租约登记: src={}@{} dst={}@{} key={} remote_dir={}",
        source_config.username,
        source_config.host,
        destination_config.username,
        destination_config.host,
        key_path.display(),
        new_lease.remote_temp_dir
    );
    registry()
        .leases
        .lock()
        .insert(lease_key.clone(), Arc::new(Mutex::new(new_lease)));

    Ok(LeaseGuard {
        key: lease_key,
        remote_key_path,
    })
}

fn remote_key_alive(session: &mut BlockingSftpSession, remote_key_path: &str) -> bool {
    let cmd = format!(
        "test -f '{}' && echo ok",
        shell_single_quote(remote_key_path)
    );
    matches!(
        session.execute_command(&cmd, 5),
        Ok(r) if r.success() && r.stdout.trim() == "ok"
    )
}

fn sweep_stale_temp_dirs_once(
    session: &mut BlockingSftpSession,
    source_config: &SshConnectionConfig,
) {
    let key = config_key(source_config);
    {
        let cleaned = registry().cleaned_sources.lock();
        if cleaned.contains(&key) {
            return;
        }
    }

    // 仅清理 mtime > 60 分钟的目录：兜底崩溃/kill -9 残留，同时避开
    // 其他并发 DeepTerm 实例当前正在使用的目录。
    let cmd = "find /tmp -maxdepth 1 -name '.deepterm-scp-*' -mmin +60 \
               -exec rm -rf {} + 2>/dev/null; echo done";
    match session.execute_command(cmd, 10) {
        Ok(_) => {
            log::info!(
                "源端历史 .deepterm-scp-* 残留清扫完成: {}@{}",
                source_config.username,
                source_config.host
            );
        }
        Err(e) => {
            log::warn!(
                "源端历史残留清扫失败 ({}@{}): {}",
                source_config.username,
                source_config.host,
                e
            );
        }
    }

    registry().cleaned_sources.lock().insert(key);
}

fn create_lease(
    source_session: &mut BlockingSftpSession,
    source_config: &SshConnectionConfig,
    key_path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<ScpKeyLease> {
    let mut key_bytes = std::fs::read(key_path)
        .with_context(|| format!("读取本地私钥失败: {}", key_path.display()))?;

    super::utils::check_cancelled(cancelled)?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let pid = std::process::id();
    let nonce = rand::random::<u32>();
    // 目录名包含 pid + 时间戳 + 随机数：防同进程内冲突，便于排障定位。
    let dir_name = format!(".deepterm-scp-{}-{}-{}", pid, timestamp, nonce);
    let remote_temp_dir = format!("/tmp/{}", dir_name);

    source_session
        .execute_command(
            &format!("mkdir -m 700 '{}'", shell_single_quote(&remote_temp_dir)),
            5,
        )
        .map_err(|e| anyhow!("源端创建临时目录失败: {}", e))?;

    super::utils::check_cancelled(cancelled)?;

    let remote_key_path = format!("{}/id_key", remote_temp_dir);
    let local_temp_path = std::env::temp_dir().join(&dir_name);

    if let Err(e) = std::fs::write(&local_temp_path, &key_bytes) {
        let _ = source_session.execute_command(
            &format!("rm -rf '{}'", shell_single_quote(&remote_temp_dir)),
            5,
        );
        key_bytes.fill(0);
        return Err(anyhow!("写入本地临时密钥文件失败: {}", e));
    }

    super::utils::check_cancelled(cancelled)?;

    if let Err(e) = source_session.upload_file_from_local_with_cancel(
        &local_temp_path,
        &remote_key_path,
        cancelled,
    ) {
        let _ = std::fs::write(&local_temp_path, vec![0u8; key_bytes.len()]);
        let _ = std::fs::remove_file(&local_temp_path);
        let _ = source_session.execute_command(
            &format!("rm -rf '{}'", shell_single_quote(&remote_temp_dir)),
            5,
        );
        key_bytes.fill(0);
        return Err(anyhow!("上传临时密钥到源服务器失败: {}", e));
    }

    super::utils::check_cancelled(cancelled)?;

    if let Err(e) = source_session.execute_command(
        &format!("chmod 600 '{}'", shell_single_quote(&remote_key_path)),
        5,
    ) {
        let _ = std::fs::write(&local_temp_path, vec![0u8; key_bytes.len()]);
        let _ = std::fs::remove_file(&local_temp_path);
        let _ = source_session.execute_command(
            &format!("rm -rf '{}'", shell_single_quote(&remote_temp_dir)),
            5,
        );
        key_bytes.fill(0);
        return Err(anyhow!("收紧源端密钥权限失败: {}", e));
    }

    let key_size = key_bytes.len();
    key_bytes.fill(0);

    Ok(ScpKeyLease {
        local_temp_path,
        remote_temp_dir,
        remote_key_path,
        key_size,
        ref_count: 1,
        source_config: source_config.clone(),
    })
}
