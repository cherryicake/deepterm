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

use super::utils::shell_quote_single;

use anyhow::{Context as _, anyhow, bail};

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::{self, BlockingSftpSession};
use zeroize::Zeroizing;

// `LeaseKey` 仅含 `config.id()` 派生的非敏感键与本地路径，不含口令/私钥，
// 故直接派生 `Debug` 即可（输出与手写实现一致，避免多余样板）。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LeaseKey {
    source_key: String,
    dest_key: String,
    local_key_path: PathBuf,
}

fn config_key(config: &SshConnectionConfig) -> String {
    // 关键安全修复：此前用 `serde_json::to_string(config)` 做键，
    // 会把明文 password/passphrase 序列化进内存 HashMap 键并长期驻留，
    // 且 `Debug` 打印即泄漏。现改用 `config.id()`（仅含 username/host/port/nickname），
    // 与 `sftp_fs::cache_key` 对密码只存内容哈希的原则一致。
    // 注意：同一主机不同凭据会复用同一租约目录，但密钥文件本身按需上传覆盖，
    // 权限均为 0600，复用不跨越安全边界。
    config.id()
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
    // 1. 源端 rm -rf 临时目录（用带引号版本，避免忘记包裹导致注入）。
    let cmd = format!("rm -rf {}", shell_quote_single(&lease.remote_temp_dir));
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

    // 2) 未命中：建租约。
    //    create_lease（网络操作）在锁外执行；插入前在注册表锁内复查一次，
    //    若其它 worker 已在竞态窗口中插入同 key 的租约，则复用对方的
    //    并回收自己刚建的重复目录——否则后插者会覆盖先插者的表项，
    //    先插者 Drop 时会按 key 查到别人的租约并 rm -rf 掉对方正在使用的
    //    远端密钥目录（并发多文件 scp 直连的 4 worker 同 key 场景真实可触发）。
    let new_lease = create_lease(source_session, source_config, key_path, cancelled)?;
    let remote_key_path;

    {
        let mut leases = registry().leases.lock();
        if let Some(existing_arc) = leases.get(&lease_key).cloned() {
            // 竞态：另一个 worker 抢先登记了同 key 租约。复用它，回收重复资源。
            let existing_remote_key_path = existing_arc.lock().remote_key_path.clone();
            leases.remove(&lease_key);
            drop(leases);
            let mut existing = existing_arc.lock();
            existing.ref_count += 1;
            log::info!(
                "SCP 密钥租约竞态复用: refs={} src={}@{} dst={}@{} key={}",
                existing.ref_count,
                source_config.username,
                source_config.host,
                destination_config.username,
                destination_config.host,
                key_path.display()
            );
            let dup = ScpKeyLease {
                local_temp_path: new_lease.local_temp_path.clone(),
                remote_temp_dir: new_lease.remote_temp_dir.clone(),
                remote_key_path: new_lease.remote_key_path.clone(),
                key_size: new_lease.key_size,
                ref_count: 0,
                source_config: new_lease.source_config.clone(),
            };
            // 回收自己刚建的目录/本地临时密钥（cleanup_lease 内部走后台线程模式，
            // 但这里调用方已不在 with_cached_session 的源会话锁内持有路径——
            // 实际在锁内，因此显式起后台线程，与 LeaseGuard::drop 同一模式）。
            let _ = std::thread::Builder::new()
                .name("scp-key-cleanup".into())
                .spawn(move || cleanup_lease(&dup));
            remote_key_path = existing_remote_key_path;
        } else {
            remote_key_path = new_lease.remote_key_path.clone();
            log::info!(
                "SCP 密钥租约登记: src={}@{} dst={}@{} key={} remote_dir={}",
                source_config.username,
                source_config.host,
                destination_config.username,
                destination_config.host,
                key_path.display(),
                new_lease.remote_temp_dir
            );
            leases.insert(lease_key.clone(), Arc::new(Mutex::new(new_lease)));
        }
    }

    Ok(LeaseGuard {
        key: lease_key,
        remote_key_path,
    })
}

fn remote_key_alive(session: &mut BlockingSftpSession, remote_key_path: &str) -> bool {
    let cmd = format!("test -f {} && echo ok", shell_quote_single(remote_key_path));
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
    // 关键安全：私钥字节用 `Zeroizing` 包裹，作用域结束自动清零，
    // 防堆内存残留与 core dump 泄露；显式 `fill(0)` 保留作双保险。
    let mut key_bytes = Zeroizing::new(
        std::fs::read(key_path)
            .with_context(|| format!("读取本地私钥失败: {}", key_path.display()))?,
    );

    super::utils::check_cancelled(cancelled)?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let pid = std::process::id();
    // nonce 由 uuid v4 的前 4 字节派生（uuid 已依赖且内含随机源），
    // 省掉 rand 整棵依赖树（全仓仅此一处使用）。
    let nonce = uuid::Uuid::new_v4().as_u128() as u32;
    // 目录名包含 pid + 时间戳 + 随机数：防同进程内冲突，便于排障定位。
    let dir_name = format!(".deepterm-scp-{}-{}-{}", pid, timestamp, nonce);
    let remote_temp_dir = format!("/tmp/{}", dir_name);

    source_session
        .execute_command(
            &format!("mkdir -m 700 {}", shell_quote_single(&remote_temp_dir)),
            5,
        )
        .map_err(|e| anyhow!("源端创建临时目录失败: {}", e))?;

    super::utils::check_cancelled(cancelled)?;

    let remote_key_path = format!("{}/id_key", remote_temp_dir);
    let local_temp_path = std::env::temp_dir().join(&dir_name);

    // 关键安全修复：本地临时私钥必须 0600 创建，避免 umask 022 下同机可读。
    // 复用 `keychain::write_secret_file`（O_CREAT|O_TRUNC + mode 0600 + 写后收紧），
    // 与远端 `upload_..._with_permissions(0o600)` 对称。
    // 另：`dir_name` 含 pid+时间戳+rand随机数，本地 `temp_dir/<dir_name>` 无预建竞态；
    // 若文件已存在（极小概率碰撞），`write_secret_file` 会截断复用，权限仍收紧为 0600。
    if let Err(e) = crate::ssh::keychain::write_secret_file(&local_temp_path, &key_bytes) {
        let _ = source_session.execute_command(
            &format!("rm -rf {}", shell_quote_single(&remote_temp_dir)),
            5,
        );
        key_bytes.fill(0);
        return Err(anyhow!("写入本地临时密钥文件失败: {}", e));
    }

    super::utils::check_cancelled(cancelled)?;

    // 关键：创建时就带 0600，避免"按远端 umask 创建 → 稍后 chmod"的
    // 窗口期内其他用户可读私钥；下面的 chmod 仅作兼容兜底（部分服务端
    // 会忽略 open 携带的初始属性）。
    if let Err(e) = source_session.upload_file_from_local_with_permissions(
        &local_temp_path,
        &remote_key_path,
        0o600,
        cancelled,
    ) {
        let _ = std::fs::write(&local_temp_path, vec![0u8; key_bytes.len()]);
        let _ = std::fs::remove_file(&local_temp_path);
        let _ = source_session.execute_command(
            &format!("rm -rf {}", shell_quote_single(&remote_temp_dir)),
            5,
        );
        key_bytes.fill(0);
        return Err(anyhow!("上传临时密钥到源服务器失败: {}", e));
    }

    super::utils::check_cancelled(cancelled)?;

    if let Err(e) = source_session.execute_command(
        &format!("chmod 600 {}", shell_quote_single(&remote_key_path)),
        5,
    ) {
        let _ = std::fs::write(&local_temp_path, vec![0u8; key_bytes.len()]);
        let _ = std::fs::remove_file(&local_temp_path);
        let _ = source_session.execute_command(
            &format!("rm -rf {}", shell_quote_single(&remote_temp_dir)),
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
