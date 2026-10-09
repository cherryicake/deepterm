use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, anyhow};

use crate::file_manager::utils::{
    DirTransferDriver, TransferSummary, check_cancelled, remote_file_name, resolve_local_conflict,
    resolve_remote_conflict, resolve_remote_conflict_with_names, snapshot_remote_dir_names,
    temporary_local_path, temporary_remote_path, walk_remote_dir,
};
use crate::file_manager::{ConflictPolicy, ConflictResolver};
use crate::ssh::SshConnectionConfig;
use crate::ssh::TRANSFER_CANCELLED_MSG;
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

    // 整树共享的并行上传池：目录树编排（枚举/冲突/建目录）与 worker
    // 线程同 scope 运行。编排串行推进（快照冲突判定 + 建目录），文件
    // 实体投进有界队列由 worker 并行上传；队列满时编排被背压拖住，
    // 枚举与传输全程重叠，连接全树只建一次。
    let pool = ParallelUploadPool::new(ssh_config, cancelled, progress);
    pool.run(|sink| {
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
                sink.submit(source_path.to_path_buf(), destination_path);
                return Ok(TransferSummary::default());
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
                return upload_local_dir_orchestrating(
                    session,
                    source_path,
                    &destination_root,
                    conflict_policy,
                    cancelled,
                    progress,
                    resolver,
                    sink,
                );
            }

            // 符号链接 / 特殊文件：计为跳过，避免 Move 误删源。
            log::warn!("跳过不支持的本地文件类型: {}", source_path.display());
            Ok(TransferSummary::one_skipped())
        })
    })
}

/// 目录树上传编排（单目录层）：枚举本地条目、串行解决冲突、投递文件、
/// 递归子目录。文件传输本身由 [`ParallelUploadPool`] 的 worker 并行完成。
#[allow(clippy::too_many_arguments)]
fn upload_local_dir_orchestrating(
    session: &mut sftp_fs::BlockingSftpSession,
    source_dir: &Path,
    destination_dir: &str,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
    sink: &ParallelUploadJobSink,
) -> anyhow::Result<TransferSummary> {
    // 调用方（upload_local_to_remote / 上层递归）已 resolve 过冲突并建好
    // destination_dir；这里不再重复 create_dir_all，避免每层递归全路径
    // 逐组件探测。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(source_dir) {
            let mode = metadata.permissions().mode();
            let _ = session.set_permissions(destination_dir, mode);
        }
    }

    let mut summary = TransferSummary::default();
    // 目标目录现有名字快照：本目录下所有条目的冲突/改名探测走内存，
    // 不再逐条目一次 exists 往返。不可读（权限/竞态删除）时回退逐次探测。
    let mut dir_names = snapshot_remote_dir_names(session, destination_dir);

    for entry in fs::read_dir(source_dir)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        let source_path = entry.path();
        let metadata = entry.metadata()?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        let destination_path = sftp_fs::join_remote_path(destination_dir, &file_name);

        if metadata.is_file() {
            match resolve_remote_conflict_with_names(
                session,
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
                &mut dir_names,
            )? {
                Some(path) => sink.submit(source_path, path),
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else if metadata.is_dir() {
            match resolve_remote_conflict_with_names(
                session,
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
                &mut dir_names,
            )? {
                Some(path) => {
                    session.create_dir_all(&path)?;
                    summary.merge(upload_local_dir_orchestrating(
                        session,
                        &source_path,
                        &path,
                        conflict_policy,
                        cancelled,
                        progress,
                        resolver,
                        sink,
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

/// 目录内文件级并行的上传 worker 数。每个 worker 独占一条 SFTP 连接；
/// 受会话池（每主机保留 8 条空闲）与服务器连接数限制，4 是经验折中，
/// 可用 DEEPTERM_UPLOAD_PARALLEL 覆盖。
fn upload_parallel_workers() -> usize {
    std::env::var("DEEPTERM_UPLOAD_PARALLEL")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 16))
        .unwrap_or(4)
}

/// 整树共享的上传 worker 池。
///
/// 编排闭包与 worker 线程同 `thread::scope` 运行：worker 借用编排线程栈上的
/// `cancelled`/`progress` 引用（字节进度实时写进 UI 共享计数器），编排闭包
/// 同步枚举/解决冲突/建目录/投递，返回即关队列并在 scope 内收割。
/// 连接全树只建一次（每 worker 一条），层级深的树不按目录反复握手。
struct UploadJob {
    source_path: PathBuf,
    destination_path: String,
}

struct WorkerOutcome {
    summary: TransferSummary,
    error: Option<anyhow::Error>,
}

struct ParallelUploadPool<'a> {
    config: SshConnectionConfig,
    cancelled: Arc<AtomicBool>,
    /// 调用方（UI 共享）的进度计数器。worker 是 thread::scope 作用域线程，
    /// 可直接借用该引用——每个数据块写入实时进 UI（0.5s 轮询可见）。
    progress: Option<&'a AtomicU64>,
    worker_count: usize,
}

impl<'a> ParallelUploadPool<'a> {
    fn new(
        config: &SshConnectionConfig,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&'a AtomicU64>,
    ) -> Self {
        Self {
            config: config.clone(),
            cancelled: cancelled.clone(),
            progress,
            worker_count: upload_parallel_workers(),
        }
    }

    /// 运行一次池化上传：worker + 编排闭包同 scope。
    fn run(
        self,
        orchestrate: impl FnOnce(&ParallelUploadJobSink) -> anyhow::Result<TransferSummary>,
    ) -> anyhow::Result<TransferSummary> {
        let worker_count = self.worker_count;
        // 有界队列：编排领先 worker 一个窗口即阻塞，内存有上限，
        // 容量足够大以让「枚举与传输重叠」生效。
        let (queue_tx, queue_rx) = std::sync::mpsc::sync_channel::<UploadJob>(64);
        let queue_rx = Arc::new(std::sync::Mutex::new(queue_rx));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<WorkerOutcome>();

        let scoped = std::thread::scope(|scope| {
            for index in 0..worker_count {
                let config = self.config.clone();
                let cancelled = self.cancelled.clone();
                let progress = self.progress;
                let queue_rx = queue_rx.clone();
                let done_tx = done_tx.clone();
                scope.spawn(move || {
                    let outcome = upload_worker_loop(
                        index,
                        &config,
                        &cancelled,
                        progress,
                        &queue_rx,
                    );
                    let _ = done_tx.send(outcome);
                });
            }

            // 编排在 scope 内同步执行（枚举/冲突/建目录/投递）。
            let sink = ParallelUploadJobSink { queue: queue_tx };
            let orchestrate_outcome = orchestrate(&sink);
            drop(sink); // 关闭队列：worker 清空队列后退出

            // 收割全部 worker 结果（在 scope 内，worker 借用尚有效）。
            let mut worker_summary = TransferSummary::default();
            let mut first_error: Option<anyhow::Error> = None;
            for _ in 0..worker_count {
                let outcome = match done_rx.recv() {
                    Ok(outcome) => outcome,
                    Err(_) => continue, // spawner 意外退出：按 0 统计
                };
                worker_summary.merge(outcome.summary);
                first_error = first_error.or(outcome.error);
            }
            (orchestrate_outcome, worker_summary, first_error)
        });
        let (orchestrate_outcome, worker_summary, first_error) = scoped;
        // 编排错误必须传播：取消 / Ask 冲突选 Stop / 建目录失败都会让编排
        // 提前返回 Err 而 worker 干净退出——吞掉它会让 Move 误判「全部成功」
        // 而删源（见 manager.rs 的 remove_source_if_complete）。
        let (orchestrate_summary, orchestrate_error) = match orchestrate_outcome {
            Ok(part) => (Some(part), None),
            Err(error) => (None, Some(error)),
        };
        // 编排错误优先：它是树遍历中止的根因，比某个文件的传输错误更能
        // 解释结果；worker 侧统计仍保留在 summary 里。
        let first_error = orchestrate_error.or(first_error);
        match first_error {
            Some(error) => {
                // 部分失败：附上已成功的统计（与目录传输语义一致）。
                let mut summary = worker_summary;
                if let Some(part) = orchestrate_summary {
                    summary.merge(part);
                }
                let done = summary.files_transferred;
                let err = if done > 0 {
                    error.context(format!(
                        "（注意：{} 个条目已成功传输、{} 个被跳过，其余未完成）",
                        done, summary.skipped
                    ))
                } else {
                    error
                };
                Err(err)
            }
            None => {
                let mut summary = worker_summary;
                if let Some(part) = orchestrate_summary {
                    summary.merge(part);
                }
                Ok(summary)
            }
        }
    }
}

/// 编排侧向池投递任务的句柄（隐藏队列细节）。
struct ParallelUploadJobSink {
    queue: std::sync::mpsc::SyncSender<UploadJob>,
}

impl ParallelUploadJobSink {
    /// 投递一个文件传输任务（阻塞到队列有空位：天然对编排限流，
    /// 内存不会随树的大小无界增长）。
    fn submit(&self, source_path: PathBuf, destination_path: String) {
        // 编排结束（sink 被 drop）后 send 报错：吞掉并记日志，不能 panic
        // 中断遍历——错误在收割阶段汇总。
        if self
            .queue
            .send(UploadJob {
                source_path,
                destination_path,
            })
            .is_err()
        {
            log::warn!("上传 worker 池已关闭，任务被丢弃");
        }
    }
}

/// 单个 worker 的主循环：经共享接收端竞争取任务，持有一条独立 SSH 连接
/// 逐个上传（单文件写路径自带 8 路并发写确认），直到取消或队列耗尽。
/// progress 直接借用编排侧计数器（scope 线程允许），字节实时上报。
fn upload_worker_loop(
    index: usize,
    config: &SshConnectionConfig,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    queue: &Arc<std::sync::Mutex<std::sync::mpsc::Receiver<UploadJob>>>,
) -> WorkerOutcome {
    // panic 防御（与 manager.rs::transfer_many_parallel 的关键防御对齐）：
    // worker panic 时 send 不会执行，收割端 recv 会永久等待这个缺失的
    // outcome——原始 done_tx 在 run() 栈上活到 scope 结束，channel 永不
    // 断开，Err 分支是死路。捕获 panic 转成错误结果，收割才可能完成。
    let inner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        upload_worker_inner(index, config, cancelled, progress, queue)
    }));
    match inner {
        Ok(outcome) => outcome,
        Err(panic_payload) => {
            let msg = panic_payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知 panic".to_string());
            log::error!("上传 worker #{index} panic，已降级为错误: {msg}");
            WorkerOutcome {
                summary: TransferSummary::default(),
                error: Some(anyhow!("上传工作线程异常: {msg}")),
            }
        }
    }
}

fn upload_worker_inner(
    _index: usize,
    config: &SshConnectionConfig,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    queue: &Arc<std::sync::Mutex<std::sync::mpsc::Receiver<UploadJob>>>,
) -> WorkerOutcome {
    let mut local = TransferSummary::default();
    let mut first_error: Option<anyhow::Error> = None;

    // 惰性建连：第一个任务到来才连（条目少的树部分 worker 全程空转不付费）。
    let mut session: Option<sftp_fs::BlockingSftpSession> = None;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let job = {
            let rx = queue.lock().unwrap_or_else(|e| e.into_inner());
            rx.recv()
        };
        let Ok(job) = job else {
            break; // 队列关闭且已空
        };
        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        // 惰性建连：第一个任务到来才连（条目少的树部分 worker 全程空转不付费）。
        // 建连失败：本 worker 退出，剩余任务由其他 worker 接手（它们可能
        // 各自建连成功）；全失败时收割阶段汇总报错。
        if session.is_none() {
            match sftp_fs::BlockingSftpSession::connect(config) {
                Ok(s) => session = Some(s),
                Err(e) => {
                    first_error.get_or_insert(anyhow!("建立上传连接失败: {e}"));
                    break;
                }
            }
        }
        let current = session.as_mut().expect("上一分支已建连");

        match upload_local_file_to_remote_path(
            current,
            &job.source_path,
            &job.destination_path,
            cancelled,
            progress,
        ) {
            Ok(part) => local.merge(part),
            Err(error) => {
                // 取消引发的错误不算传输错误（外层统一报取消）：
                // 先查标志位（结构化判定），字符串比较只作兜底。
                if cancelled.load(Ordering::Relaxed) || error.to_string() == *TRANSFER_CANCELLED_MSG {
                    break;
                }
                first_error.get_or_insert(error);
            }
        }
    }
    // session 在此 drop（干净断开）；若中途建连失败，剩余任务留在队列里
    // 由其他 worker 处理（它们可能各自建连成功）。
    WorkerOutcome {
        summary: local,
        error: first_error,
    }
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

    // create 带 TRUNCATE：上次传输残留的同名 .deepterm-part 直接被
    // 覆盖，无需先 exists 一次（省一次每文件往返）。
    let temporary_path = temporary_remote_path(destination_path);
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

/// 远程→本地目录下载的 driver：见 [`walk_remote_dir`]。
struct RemoteToLocalDirDriver<'a> {
    session: &'a mut sftp_fs::BlockingSftpSession,
    /// 当前 transfer_file 对应的源条目 size（来自 read_dir，免再 stat）。
    current_entry_size: Option<u64>,
}

impl DirTransferDriver for RemoteToLocalDirDriver<'_> {
    type Destination = PathBuf;

    fn read_entries(&mut self, remote_dir: &str) -> anyhow::Result<Vec<sftp_fs::RemoteFileEntry>> {
        self.session.read_dir(remote_dir)
    }

    fn child_destination(&self, destination_dir: &PathBuf, name: &str) -> PathBuf {
        destination_dir.join(name)
    }

    fn resolve_conflict(
        &mut self,
        desired: &PathBuf,
        policy: ConflictPolicy,
        resolver: Option<&dyn ConflictResolver>,
        cancelled: &AtomicBool,
        _dir_names: &mut crate::file_manager::utils::RemoteDirNames,
    ) -> anyhow::Result<Option<PathBuf>> {
        resolve_local_conflict(desired, policy, resolver, cancelled)
    }

    fn prepare_destination_dir(
        &mut self,
        // 仅 unix 用它落地远端权限（chmod）；其他平台没有对应语义。
        #[cfg_attr(not(unix), allow(unused_variables))] remote_dir: &str,
        destination_dir: &PathBuf,
    ) -> anyhow::Result<()> {
        fs::create_dir_all(destination_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(Some(mode)) = self.session.permissions(remote_dir) {
                let _ = fs::set_permissions(destination_dir, fs::Permissions::from_mode(mode));
            }
        }
        Ok(())
    }

    fn will_transfer_entry(&mut self, entry: &sftp_fs::RemoteFileEntry) {
        self.current_entry_size = Some(entry.size);
    }

    fn transfer_file(
        &mut self,
        source_path: &str,
        destination: &PathBuf,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary> {
        // walker 场景：read_dir 条目已带 size，传入后下载函数不再为阈值
        // 判定单独 stat 一次（目录树每文件省一个串行 RTT）。
        let known = self.current_entry_size.map(|size| sftp_fs::RemoteFileMetadata {
            kind: RemoteFileKind::File,
            size,
            modified_at: None,
            permissions: None,
        });
        download_remote_file_with_known_metadata(
            self.session,
            source_path,
            destination,
            known,
            cancelled,
            progress,
        )
    }
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
    walk_remote_dir(
        &mut RemoteToLocalDirDriver {
            session,
            current_entry_size: None,
        },
        remote_dir,
        &destination_dir.to_path_buf(),
        conflict_policy,
        cancelled,
        progress,
        resolver,
    )
}

pub fn download_remote_file_to_local_path(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_path: &Path,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    download_remote_file_with_known_metadata(
        session,
        remote_path,
        destination_path,
        None,
        cancelled,
        progress,
    )
}

/// 下载单文件（可带已知 metadata）：`known` 提供时跳过阈值判定的 stat
/// 与 chmod 查询（目录树 walker 从 read_dir 已拿到 size；permissions
/// 仍需单独查——它不在 read_dir 条目里，仅 unix 落地 chmod 用）。
fn download_remote_file_with_known_metadata(
    session: &mut sftp_fs::BlockingSftpSession,
    remote_path: &str,
    destination_path: &Path,
    known: Option<sftp_fs::RemoteFileMetadata>,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<TransferSummary> {
    // 远端权限只用于 unix 侧的落地 chmod；其他平台没有对应语义，
    // 但仍保留这次查询（它同时验证远端路径可读，避免白跑一次下载）。
    #[cfg_attr(not(unix), allow(unused_variables))]
    let remote_permissions = session.permissions(remote_path).ok().flatten();

    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary_path = temporary_local_path(destination_path);
    if temporary_path.exists() {
        let _ = fs::remove_file(&temporary_path);
    }
    let result = session.download_file_with_known_size(
        remote_path,
        &temporary_path,
        known,
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
