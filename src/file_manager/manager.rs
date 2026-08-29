use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::file_manager::cross_remote::{self, TransferStatusReporter};
use crate::file_manager::utils::TransferSummary;
use crate::file_manager::{
    ConflictPolicy, ConflictResolver, FileEndpoint, TransferOperation, TransferRequest,
};

pub struct TransferManager;

/// 多源传输的并行并发上限：同时最多跑多少个独立传输（每个源一条线程）。
/// 受 SFTP 会话池 / 共享 SSH runtime（4 worker）约束，4 是网络 IO 为主的合理折中。
const MAX_PARALLEL_TRANSFERS: usize = 4;

/// 按「目的 + 源文件名」分条的全局互斥条数。
///
/// 冲突策略（AutoRename / Ask）的“检查目标是否存在 → 创建/改名”不是原子操作：
/// 两个**同名**源并行传到同一目录时（如 `dir1/a.txt dir2/a.txt → host:/dst/`），
/// 会同时判定“a.txt 空闲”并同时写入，后者静默覆盖前者。
/// 用目的 + 源文件名的哈希分条锁把同名同目标的传输串行化；
/// 不同文件名落在不同条上，互不阻塞，也不引入全局串行化。
const TRANSFER_LOCK_STRIPES: usize = 64;

static TRANSFER_LOCKS: [std::sync::Mutex<()>; TRANSFER_LOCK_STRIPES] =
    [const { std::sync::Mutex::new(()) }; TRANSFER_LOCK_STRIPES];

/// 计算某个源→目的传输对应的分条锁。
fn transfer_stripe(
    source: &FileEndpoint,
    destination: &FileEndpoint,
    rename_to: Option<&str>,
) -> &'static std::sync::Mutex<()> {
    let name = rename_to
        .map(str::to_string)
        .or_else(|| match source {
            FileEndpoint::Local { path } => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            FileEndpoint::Remote { path, .. } => path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .map(str::to_string),
        })
        .unwrap_or_default();
    let dest = match destination {
        FileEndpoint::Local { path } => path.display().to_string(),
        FileEndpoint::Remote { ssh_config, path } => {
            format!("{}@{}:{}{}", ssh_config.username, ssh_config.host, ssh_config.port, path)
        }
    };
    let key = format!("{}|{}", dest, name);
    let mut hash = 0u64;
    for byte in key.bytes() {
        hash = hash.wrapping_mul(131).wrapping_add(byte as u64);
    }
    &TRANSFER_LOCKS[(hash as usize) % TRANSFER_LOCK_STRIPES]
}

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
        let TransferRequest {
            sources,
            destination,
            operation,
            conflict_policy,
            rename_to,
        } = request;
        let rename_to = if sources.len() == 1 {
            rename_to
        } else {
            None
        };

        // scp CLI 语义：`/scp src dst`（dst 无尾斜杠）在 dst 是**已存在目录**时
        // 表示“拷入该目录”，仅当 dst 不存在/是文件名时才是重命名。
        // `/scp` 处理器只做字符串拆分（dir + 期望文件名），这里在实际传输前
        // 做一次存在性判定，把“dir+名 已存在且为目录”的目的地收敛回目录语义。
        // 例子：`/scp a.txt ~` / `/scp a.txt /tmp` / `/scp a.txt bob:/tmp`。
        let (destination, rename_to) =
            Self::resolve_existing_dir_destination(destination, rename_to);

        if sources.len() <= 1 {
            // 单源（或空）：保持原串行路径。
            let mut summary = TransferSummary::default();
            for source in sources {
                crate::file_manager::utils::check_cancelled(&cancelled)?;
                summary.merge(Self::transfer_one(
                    source,
                    &destination,
                    operation,
                    conflict_policy,
                    rename_to.as_deref(),
                    &cancelled,
                    progress.as_deref(),
                    resolver,
                    reporter,
                )?);
            }
            return Ok(summary);
        }

        // 多源：并行传输。每个源独占一条工作线程（以及各自独立的 SFTP 连接，
        // 见 sftp_fs 的会话池），线程间共享取消标志 / 进度计数 / 冲突解决器。
        Self::transfer_many_parallel(
            sources,
            &destination,
            operation,
            conflict_policy,
            rename_to.as_deref(),
            &cancelled,
            progress.as_deref(),
            resolver,
            reporter,
        )
    }

    /// 多源并行传输：最多 [`MAX_PARALLEL_TRANSFERS`] 个工作线程轮流取源，
    /// 任意时刻至多 N 条传输在跑；取消后不再取新源，其余源继续尝试传完，
    /// 汇总后返回第一个错误（与文件管理器“其余文件照常”的语义一致）。
    fn transfer_many_parallel(
        sources: Vec<FileEndpoint>,
        destination: &FileEndpoint,
        operation: TransferOperation,
        conflict_policy: ConflictPolicy,
        rename_to: Option<&str>,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
        resolver: Option<&dyn ConflictResolver>,
        reporter: Option<&dyn TransferStatusReporter>,
    ) -> anyhow::Result<TransferSummary> {
        if sources.is_empty() {
            return Ok(TransferSummary::default());
        }
        // 与串行路径一致：尚未开始就已被取消 ⇒ 直接返回取消错误。
        crate::file_manager::utils::check_cancelled(cancelled)?;

        let worker_count = sources.len().min(MAX_PARALLEL_TRANSFERS);
        let next = AtomicUsize::new(0);

        let outcomes: Vec<(TransferSummary, Option<anyhow::Error>)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..worker_count)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut local = TransferSummary::default();
                            let mut first_error: Option<anyhow::Error> = None;
                            loop {
                                // 已取消：不再取新源（正在传的源内部也会自行退出）。
                                if crate::file_manager::utils::check_cancelled(cancelled).is_err()
                                {
                                    break;
                                }
                                let index = next.fetch_add(1, Ordering::Relaxed);
                                let Some(source) = sources.get(index) else {
                                    break;
                                };
                                match Self::transfer_one(
                                    source.clone(),
                                    destination,
                                    operation,
                                    conflict_policy,
                                    rename_to,
                                    cancelled,
                                    progress,
                                    resolver,
                                    reporter,
                                ) {
                                    Ok(part) => local.merge(part),
                                    Err(error) => {
                                        if first_error.is_none() {
                                            first_error = Some(error);
                                        }
                                    }
                                }
                            }
                            (local, first_error)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

        let mut summary = TransferSummary::default();
        let mut first_error: Option<anyhow::Error> = None;
        for (part, error) in outcomes {
            summary.merge(part);
            if first_error.is_none() {
                first_error = error;
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(summary),
        }
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

    /// 把“无尾斜杠目的 token”拆出的 `(目录, 期望文件名)` 按 scp CLI 语义收敛：
    /// 若 `目录/文件名` 实际指向一个已存在的目录，则该名字不是重命名目标，
    /// 而是目的地目录本身（拷入其中）。本地用 `is_dir` 同步判定；远端走
    /// SFTP 会话池的 `metadata`（会话有缓存，代价一次往返）。
    /// 判定失败（网络错误 / 不存在 / 是文件）时保持原语义不变，交由后续
    /// 冲突解决流程处理。
    fn resolve_existing_dir_destination(
        destination: FileEndpoint,
        rename_to: Option<String>,
    ) -> (FileEndpoint, Option<String>) {
        let Some(name) = rename_to else {
            return (destination, None);
        };
        match &destination {
            FileEndpoint::Local { path } => {
                let full = path.join(&name);
                if full.is_dir() {
                    log::info!(
                        "/scp 目的地 {} 是已存在目录，改为拷入其中",
                        full.display()
                    );
                    return (FileEndpoint::Local { path: full }, None);
                }
                (destination, Some(name))
            }
            FileEndpoint::Remote { ssh_config, path } => {
                let full = crate::ssh::sftp_fs::join_remote_path(path, &name);
                let is_dir = crate::ssh::sftp_fs::with_cached_session(ssh_config, |session| {
                    Ok(session.metadata(&full).is_ok_and(|m| {
                        m.kind == crate::ssh::sftp_fs::RemoteFileKind::Directory
                    }))
                })
                .unwrap_or(false);
                if is_dir {
                    log::info!(
                        "/scp 目的地 {}:{} 是已存在目录，改为拷入其中",
                        ssh_config.host_label(),
                        full
                    );
                    return (
                        FileEndpoint::Remote {
                            ssh_config: ssh_config.clone(),
                            path: full,
                        },
                        None,
                    );
                }
                (destination, Some(name))
            }
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
        // 同名同目标的并发保护（见 TRANSFER_LOCK_STRIPES 注释）：
        // 串行路径与并行路径都经过这里，跨任务并发同样受保护。
        let _stripe = transfer_stripe(&source, destination, rename_to)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match (source, destination) {
            (
                FileEndpoint::Local { path },
                FileEndpoint::Local {
                    path: destination_dir,
                },
            ) if operation == TransferOperation::Move => {
                crate::file_manager::local_ops::move_local_to_local(
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
            ) => crate::file_manager::local_ops::copy_local_to_local(
                &path,
                destination_dir,
                conflict_policy,
                rename_to,
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
                let summary = crate::file_manager::upload_download::upload_local_to_remote(
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
                        crate::file_manager::utils::remove_local_source(&path)
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
                let summary = crate::file_manager::upload_download::download_remote_to_local(
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
                            crate::file_manager::utils::remove_remote_source(session, &remote_path)
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
                crate::file_manager::remote_ops::move_remote_to_same_remote(
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
                crate::file_manager::remote_ops::copy_remote_to_same_remote(
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
                            crate::file_manager::utils::remove_remote_source(session, &remote_path)
                        })
                    })?;
                }
                Ok(summary)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn local(path: PathBuf) -> FileEndpoint {
        FileEndpoint::Local { path }
    }

    #[test]
    fn scp_semantics_existing_dir_destination_copies_into_it() {
        // /scp a.txt <已存在目录>（无尾斜杠）⇒ 拷入该目录，而不是重命名成目录名
        let dir = std::env::temp_dir().join(format!(
            "deepterm_scpdir_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("existing_dir")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();

        let request = TransferRequest {
            sources: vec![local(dir.join("a.txt"))],
            // 模拟 /scp 处理器对 `.../existing_dir`（无尾斜杠）的拆分结果
            destination: local(dir.clone()),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::Ask,
            rename_to: Some("existing_dir".to_string()),
        };
        let summary = TransferManager::execute_blocking(request).unwrap();
        assert_eq!(summary.files_transferred, 1);
        assert!(
            dir.join("existing_dir/a.txt").exists(),
            "应拷入已存在目录，而不是试图把文件写成目录本身"
        );
        assert!(!dir.join("existing_dir/existing_dir").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scp_semantics_nonexistent_name_still_renames() {
        // /scp a.txt newname.txt（不存在）⇒ 保持重命名语义
        let dir = std::env::temp_dir().join(format!(
            "deepterm_scpren_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();

        let request = TransferRequest {
            sources: vec![local(dir.join("a.txt"))],
            destination: local(dir.clone()),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::Ask,
            rename_to: Some("newname.txt".to_string()),
        };
        let summary = TransferManager::execute_blocking(request).unwrap();
        assert_eq!(summary.files_transferred, 1);
        assert!(dir.join("newname.txt").exists(), "不存在的名字应保持重命名");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_multi_source_local_copy() {
        let dir = std::env::temp_dir().join(format!(
            "deepterm_parallel_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        let dst_dir = dir.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        for (name, content) in [("a.txt", "aaa"), ("b.txt", "bbb"), ("c.txt", "ccc")] {
            std::fs::write(src_dir.join(name), content).unwrap();
        }

        let request = TransferRequest {
            sources: vec![
                local(src_dir.join("a.txt")),
                local(src_dir.join("b.txt")),
                local(src_dir.join("c.txt")),
            ],
            destination: local(dst_dir.clone()),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::AutoRename,
            rename_to: None,
        };
        let summary = TransferManager::execute_blocking(request).unwrap();

        assert_eq!(summary.files_transferred, 3, "三个文件都应传完");
        assert_eq!(summary.skipped, 0);
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert!(
                dst_dir.join(name).exists(),
                "目标文件 {} 应存在",
                dst_dir.join(name).display()
            );
            assert_eq!(
                std::fs::read_to_string(dst_dir.join(name)).unwrap(),
                std::fs::read_to_string(src_dir.join(name)).unwrap()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_same_name_sources_autorename_not_overwrite() {
        // 两个目录下同名文件并行传同一目标：分条锁将其串行化，
        // 第二个应自动改名（a.txt_副本）而不是静默覆盖第一个。
        let dir = std::env::temp_dir().join(format!(
            "deepterm_samename_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d1")).unwrap();
        std::fs::create_dir_all(dir.join("d2")).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("d1/a.txt"), "first").unwrap();
        std::fs::write(dir.join("d2/a.txt"), "second").unwrap();

        let request = TransferRequest {
            sources: vec![
                local(dir.join("d1/a.txt")),
                local(dir.join("d2/a.txt")),
            ],
            destination: local(dir.join("dst")),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::AutoRename,
            rename_to: None,
        };
        let summary = TransferManager::execute_blocking(request).unwrap();

        assert_eq!(summary.files_transferred, 2);
        // 两个文件都在，且内容互不覆盖（顺序取决于哪个 worker 先取到源）。
        // 自动改名格式：a.txt → a_副本.txt（后缀插入 stem 与扩展名之间）。
        let mut contents: Vec<String> = ["a.txt", "a_副本.txt"]
            .iter()
            .map(|name| std::fs::read_to_string(dir.join("dst").join(name)).unwrap())
            .collect();
        contents.sort();
        assert_eq!(contents, vec!["first".to_string(), "second".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_multi_source_skips_cancel_before_start() {
        // 并发上限不会因取消而卡死：取消后工作线程应立即退出且不报错。
        let dir = std::env::temp_dir().join(format!(
            "deepterm_cancel_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "a").unwrap();
        std::fs::write(dir.join("src/b.txt"), "b").unwrap();

        let cancelled = Arc::new(AtomicBool::new(true));
        let request = TransferRequest {
            sources: vec![
                local(dir.join("src/a.txt")),
                local(dir.join("src/b.txt")),
            ],
            destination: local(dir.join("dst")),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::AutoRename,
            rename_to: None,
        };
        let result = TransferManager::execute_blocking_with_cancel(request, cancelled);
        assert!(result.is_err(), "已取消时应返回取消错误");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
