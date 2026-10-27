use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext, Context, Entity, EventEmitter, Global};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::file_manager::filesystem::{FileMetadata, FileSystem};
use crate::file_manager::types::FileEntry;
use crate::ssh::SshConnectionConfig;
use futures::StreamExt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsChangeKind {
    Create,
    Modify,
    Remove,
    Other,
}

impl From<&EventKind> for FsChangeKind {
    fn from(kind: &EventKind) -> Self {
        match kind {
            EventKind::Create(_) => Self::Create,
            EventKind::Modify(_) => Self::Modify,
            EventKind::Remove(_) => Self::Remove,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FsChangeEvent {
    pub path: PathBuf,
    pub kind: FsChangeKind,
    /// For remote directory polling: the new directory entries already fetched by the watcher.
    /// When present, the subscriber can update its cache directly without an extra async load.
    pub entries: Option<Vec<FileEntry>>,
    /// 事件来源的远程服务器标识（`username@host:port`）。
    ///
    /// 多台服务器可能 watch 同一路径（如两个终端 tab 分别连不同主机、cwd 都是 `/root`），
    /// 轮询器会为每台服务器独立发事件。订阅方（文件树 / Git 面板 / 编辑器）
    /// 必须用它过滤，只应用与自己后端匹配的事件（见 [`FsChangeEvent::is_from_backend`]），
    /// 否则 A 服务器的事件会污染 B 服务器的文件树缓存。
    /// `None` 表示本地 notify 事件（不区分后端）。
    pub server: Option<String>,
}

impl FsChangeEvent {
    pub fn new(path: PathBuf, kind: FsChangeKind) -> Self {
        Self {
            path,
            kind,
            entries: None,
            server: None,
        }
    }

    /// 远程事件：附上服务器标识。
    pub fn remote(path: PathBuf, kind: FsChangeKind, server: String) -> Self {
        Self {
            server: Some(server),
            ..Self::new(path, kind)
        }
    }

    /// 远程目录轮询事件：附上新目录项与服务器标识。
    pub fn remote_with_entries(
        path: PathBuf,
        kind: FsChangeKind,
        server: String,
        entries: Vec<FileEntry>,
    ) -> Self {
        Self {
            entries: Some(entries),
            ..Self::remote(path, kind, server)
        }
    }

    /// 该事件是否来自 `backend_ssh_config` 对应的文件系统。
    ///
    /// 远程事件只匹配同一台服务器；本地事件（`server` 为 `None`）只匹配
    /// 无 SSH 配置的本地后端。
    pub fn is_from_backend(&self, backend_ssh_config: Option<&SshConnectionConfig>) -> bool {
        match (backend_ssh_config, self.server.as_deref()) {
            (Some(config), Some(server)) => remote_server_key(config) == server,
            (None, None) => true,
            _ => false,
        }
    }
}

/// 从 SSH 配置派生服务器标识（`username@host:port`）：远程 watch 状态、
/// 事件过滤与 `RemoteFileTreeCache` 共享缓存都以它为隔离单元，单一实现
/// 防止三处口径漂移。
///
/// 同一主机同一账号的多个 `RemoteFileSystem` 实例（每次建树都会 new 一个）
/// 会得到相同 key，watch 状态因此共享；不同主机同路径（两台服务器的 `/root`）
/// 则彼此独立。
pub fn remote_server_key(config: &SshConnectionConfig) -> String {
    format!("{}@{}:{}", config.username, config.host, config.port)
}

/// `FileSystem` 后端对应的 watch 服务器标识。
/// 返回 `None` 表示无法识别（不会进入远程轮询器，仅用于防御性调用）。
fn fs_server_key(fs: &Arc<dyn FileSystem>) -> Option<String> {
    fs.ssh_config().as_ref().map(remote_server_key)
}

pub fn init(cx: &mut App) {
    FsWatcherGlobal::init(cx);
    RemoteFsWatcherGlobal::init(cx);
}

/// Global handle to the file watcher entity.
///
/// Components can subscribe to the watcher entity directly to receive events, or use
/// [`FsWatcherGlobal::watch`] / [`FsWatcherGlobal::unwatch`] to manage watched paths.
pub struct FsWatcherGlobal {
    watcher: Entity<FsWatcher>,
}

impl Global for FsWatcherGlobal {}

impl FsWatcherGlobal {
    pub fn init(cx: &mut App) {
        let watcher = cx.new(FsWatcher::new);
        cx.set_global(Self { watcher });
    }

    pub fn watcher(cx: &App) -> Entity<FsWatcher> {
        cx.global::<Self>().watcher.clone()
    }

    pub fn watch(path: &Path, cx: &mut App) {
        let watcher = Self::watcher(cx);
        watcher.update(cx, |w, cx| w.watch(path, cx));
    }

    pub fn unwatch(path: &Path, cx: &mut App) {
        let watcher = Self::watcher(cx);
        watcher.update(cx, |w, cx| w.unwatch(path, cx));
    }
}

/// Global handle to the remote file polling watcher.
pub struct RemoteFsWatcherGlobal {
    watcher: Entity<RemoteFsWatcher>,
}

impl Global for RemoteFsWatcherGlobal {}

impl RemoteFsWatcherGlobal {
    pub fn init(cx: &mut App) {
        let watcher = cx.new(RemoteFsWatcher::new);
        cx.set_global(Self { watcher });
    }

    pub fn watcher(cx: &App) -> Entity<RemoteFsWatcher> {
        cx.global::<Self>().watcher.clone()
    }

    pub fn watch(path: &Path, fs: Arc<dyn FileSystem>, cx: &mut App) {
        let watcher = Self::watcher(cx);
        watcher.update(cx, |w, cx| w.watch(path, fs, cx));
    }

    pub fn unwatch(path: &Path, fs: &Arc<dyn FileSystem>, cx: &mut App) {
        let watcher = Self::watcher(cx);
        watcher.update(cx, |w, cx| w.unwatch(path, fs, cx));
    }
}

/// Reference counts for a watched path, tracked separately for recursive and non-recursive
/// subscribers so the underlying OS watch mode matches the most permissive active request.
#[derive(Clone, Copy, Debug, Default)]
struct WatchRefs {
    non_recursive: usize,
    recursive: usize,
}

/// Cross-platform file system watcher that bridges `notify` events into GPUI's entity event system.
///
/// Watch/unwatch calls are reference counted. Recursive and non-recursive watches are tracked
/// independently; if any subscriber requests recursive mode, the OS watch is configured
/// recursively, otherwise non-recursively.
pub struct FsWatcher {
    watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
    watched_paths: Arc<Mutex<HashMap<PathBuf, WatchRefs>>>,
    _task: gpui::Task<()>,
}

impl EventEmitter<FsChangeEvent> for FsWatcher {}

/// 单元测试下是否启动真实 OS 文件监视。
///
/// gpui 的 `TestScheduler` 会把测试期间仍在活动的外部线程判为「非确定性」
/// 并 abort 整个测试二进制。`notify` 的 inotify 线程恰恰是这样一条线程：
/// 只要期间有一个文件事件被投递，`cargo test` 就会随机炸掉（与 LSP 的
/// `smol::spawn` 同一类问题）。测试只覆盖引用计数与事件分发逻辑，
/// 不需要真的落到 inotify，因此这里直接不创建底层 watcher（`None` 分支
/// 在 `watch`/`unwatch` 里已有完整处理）。
#[cfg(not(test))]
fn spawn_os_watcher(tx: async_channel::Sender<Event>) -> Option<RecommendedWatcher> {
    match RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.try_send(event);
            }
        },
        notify::Config::default().with_poll_interval(Duration::from_millis(500)),
    ) {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            log::error!("Failed to create file watcher: {e} — file watching disabled");
            None
        }
    }
}

#[cfg(test)]
fn spawn_os_watcher(_tx: async_channel::Sender<Event>) -> Option<RecommendedWatcher> {
    log::debug!("FsWatcher: 单元测试下不启动 OS 文件监视线程");
    None
}

impl FsWatcher {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();

        let watcher = Arc::new(Mutex::new(spawn_os_watcher(tx)));

        let watched_paths = Arc::new(Mutex::new(HashMap::new()));
        let weak = cx.entity().downgrade();

        let _task = cx.spawn(async move |_, cx| {
            while let Ok(event) = rx.recv().await {
                let kind = FsChangeKind::from(&event.kind);
                if let Some(watcher) = weak.upgrade() {
                    watcher.update(cx, |_, cx| {
                        for path in &event.paths {
                            cx.emit(FsChangeEvent::new(path.clone(), kind.clone()));
                        }
                    });
                }
            }
        });

        Self {
            watcher,
            watched_paths,
            _task,
        }
    }

    pub fn watch_recursive(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let (need_start, need_upgrade) = {
            let mut refs = self.watched_paths.lock();
            let entry = refs.entry(path.clone()).or_insert(WatchRefs {
                non_recursive: 0,
                recursive: 0,
            });
            let was_recursive = entry.recursive > 0;
            entry.recursive += 1;
            if !was_recursive {
                if entry.non_recursive == 0 {
                    (true, false)
                } else {
                    (false, true)
                }
            } else {
                (false, false)
            }
        };
        if need_start || need_upgrade {
            let mut guard = self.watcher.lock();
            if let Some(watcher) = guard.as_mut() {
                if need_upgrade && let Err(e) = watcher.unwatch(&path) {
                    log::warn!("Failed to reconfigure watch for {}: {e}", path.display());
                }
                if let Err(e) = watcher.watch(&path, RecursiveMode::Recursive) {
                    log::warn!("Failed to watch {}: {e}", path.display());
                }
            } else {
                // 创建失败时 `new` 已经 error 过一次；这里逐路径降为 debug，
                // 避免监视不可用的机器上每个文件都刷一条 warning。
                log::debug!(
                    "File watcher disabled, ignoring watch for {}",
                    path.display()
                );
            }
        }
        cx.notify();
    }

    /// Start watching a path non-recursively. Callers that need recursive monitoring (e.g. the
    /// file tree) should call [`Self::watch_recursive`].
    pub fn watch(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let need_start = {
            let mut refs = self.watched_paths.lock();
            let entry = refs.entry(path.clone()).or_insert(WatchRefs {
                non_recursive: 0,
                recursive: 0,
            });
            entry.non_recursive += 1;
            entry.non_recursive == 1 && entry.recursive == 0
        };
        if need_start {
            let mut guard = self.watcher.lock();
            if let Some(watcher) = guard.as_mut() {
                if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                    log::warn!("Failed to watch {}: {e}", path.display());
                }
            } else {
                // 创建失败时 `new` 已经 error 过一次；这里逐路径降为 debug，
                // 避免监视不可用的机器上每个文件都刷一条 warning。
                log::debug!(
                    "File watcher disabled, ignoring watch for {}",
                    path.display()
                );
            }
        }
        cx.notify();
    }

    /// Stop a non-recursive watch. The underlying OS watch is removed or downgraded when the last
    /// reference is dropped.
    pub fn unwatch(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let (need_stop, need_downgrade) = {
            let mut refs = self.watched_paths.lock();
            if let Some(entry) = refs.get_mut(&path) {
                if entry.non_recursive > 0 {
                    entry.non_recursive -= 1;
                    let need_stop = entry.non_recursive == 0 && entry.recursive == 0;
                    let need_downgrade = entry.non_recursive == 0 && entry.recursive > 0;
                    if need_stop {
                        refs.remove(&path);
                    }
                    (need_stop, need_downgrade)
                } else {
                    log::warn!(
                        "unwatch called for {} without a matching non-recursive watch",
                        path.display()
                    );
                    (false, false)
                }
            } else {
                log::warn!("unwatch called for {} which is not watched", path.display());
                (false, false)
            }
        };
        if need_stop || need_downgrade {
            let mut guard = self.watcher.lock();
            if let Some(watcher) = guard.as_mut() {
                if let Err(e) = watcher.unwatch(&path) {
                    log::warn!("Failed to unwatch {}: {e}", path.display());
                }
                if need_downgrade && let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive)
                {
                    log::warn!("Failed to reconfigure watch for {}: {e}", path.display());
                }
            }
        }
        cx.notify();
    }

    /// Stop a recursive watch. The underlying OS watch is removed or downgraded when the last
    /// reference is dropped.
    pub fn unwatch_recursive(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let (need_stop, need_downgrade) = {
            let mut refs = self.watched_paths.lock();
            if let Some(entry) = refs.get_mut(&path) {
                if entry.recursive > 0 {
                    entry.recursive -= 1;
                    let need_stop = entry.non_recursive == 0 && entry.recursive == 0;
                    let need_downgrade = entry.recursive == 0 && entry.non_recursive > 0;
                    if need_stop {
                        refs.remove(&path);
                    }
                    (need_stop, need_downgrade)
                } else {
                    log::warn!(
                        "unwatch_recursive called for {} without a matching recursive watch",
                        path.display()
                    );
                    (false, false)
                }
            } else {
                log::warn!(
                    "unwatch_recursive called for {} which is not watched",
                    path.display()
                );
                (false, false)
            }
        };
        if need_stop || need_downgrade {
            let mut guard = self.watcher.lock();
            if let Some(watcher) = guard.as_mut() {
                if let Err(e) = watcher.unwatch(&path) {
                    log::warn!("Failed to unwatch {}: {e}", path.display());
                }
                if need_downgrade && let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive)
                {
                    log::warn!("Failed to reconfigure watch for {}: {e}", path.display());
                }
            }
        }
        cx.notify();
    }
}

/// 远程轮询间隔。
///
/// 旧值 3s 会让每个 watch 的文件每 3 秒整读一次；远程文件通常只需要
/// “是否变化”的语义，10s 的延迟对文件树/编辑器足够，同时显著降低
/// SFTP 小 RPC 数量。
const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// 文件内容哈希的最短间隔。
///
/// 正常情况下用 metadata（size/mtime）判断文件是否变化；只有 metadata
/// 变化、或距上次内容校验超过该间隔时才整读文件。这样既能把常规轮询的
/// 读取量降下来，又能兜底“内容变了但 mtime/size 不变”的编辑器写入。
const REMOTE_CONTENT_HASH_INTERVAL: Duration = Duration::from_secs(30);

/// 允许做内容哈希兜底的最大文件大小。
///
/// 超过该大小的文件只依赖 metadata 判断变化：整读成本随文件大小线性增长，
/// 对大型远程日志/源码反复读取会抵消轮询优化，编辑器自身也会拒绝打开
/// 超过 20MB 的文本文件。
const REMOTE_CONTENT_HASH_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 远程 metadata/read_dir/read_file_bytes 都是同步 SFTP RPC。生产环境把它们
/// 放到 smol 阻塞线程池，避免占满 GPUI 共享后台 worker（见下方轮询处注释）。
///
/// GPUI 的 `TestScheduler` 不允许工作线程唤醒测试线程，所以单测里直接同步执行。
#[cfg(not(test))]
async fn run_blocking_io<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    smol::unblock(f).await
}

#[cfg(test)]
async fn run_blocking_io<T>(f: impl FnOnce() -> T) -> T {
    f()
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn detect_remote_change(
    prev_meta: Option<&FileMetadata>,
    prev_entries: Option<&[FileEntry]>,
    prev_hash: Option<u64>,
    new_meta: &FileMetadata,
    new_entries: Option<&[FileEntry]>,
    new_hash: Option<u64>,
    reported_missing: bool,
) -> (bool, FsChangeKind) {
    let prev_is_dir = prev_meta.map(|m| m.is_dir);
    let kind = if prev_is_dir == Some(true) && !new_meta.is_dir {
        FsChangeKind::Remove
    } else {
        FsChangeKind::Modify
    };

    if reported_missing {
        return (true, FsChangeKind::Create);
    }

    let emit = if let Some(prev_meta) = prev_meta {
        if prev_meta.is_dir != new_meta.is_dir {
            true
        } else if new_meta.is_dir {
            if let Some(new_entries) = new_entries {
                prev_entries != Some(new_entries)
            } else {
                prev_entries.is_some()
            }
        } else {
            prev_meta != new_meta || prev_hash != new_hash
        }
    } else if new_meta.is_dir {
        // watch 刚建立后的首轮目录轮询：主动同步一次真实状态。
        //
        // 不emit的首轮基线存在空窗：注册 watch 到首次轮询之间发生的变化
        // （如切换终端 tab 后立刻 `rm` 文件）会被基线吞掉 —— 基线本身就是
        // 删除后的状态，后续轮询永无差异，已删文件将无限期留在文件树上。
        // 改为携带真实 entries 发一次 Modify，订阅方（文件树 loader 对比
        // 缓存）幂等吸收：相同无事发生，不同则刷新。
        true
    } else {
        false
    };

    (emit, kind)
}

/// Remote file system watcher that polls `metadata` for watched paths because remote SFTP
/// directories do not support native OS-level filesystem notifications.
///
/// Emits the same `FsChangeEvent` events as the local watcher so that subscribers do not need to
/// distinguish between the two backends.
pub struct RemoteFsWatcher {
    watched: Arc<Mutex<HashMap<WatchKey, RemoteWatchState>>>,
    _task: gpui::Task<()>,
}

/// watch 状态的键：路径 + 服务器标识。
///
/// 服务器标识不可或缺：两台服务器（两个终端 tab 连不同主机）的 cwd 常常
/// 相同（如都在 `/root`）；若只用路径做键，后注册的 fs 会被 `and_modify`
/// 静默丢弃，轮询永远只读第一台服务器 —— 另一台终端里删除文件后，它的文件树
/// 永远收不到事件（表现为删除的文件一直显示）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WatchKey {
    path: PathBuf,
    server: String,
}

struct RemoteWatchState {
    fs: Arc<dyn FileSystem>,
    last_meta: Option<FileMetadata>,
    last_dir_entries: Option<Vec<FileEntry>>,
    last_content_hash: Option<u64>,
    /// 上次对文件内容做整读/哈希的时间；用于限制整文件读取频率。
    last_content_check: Option<Instant>,
    /// 同一路径被多个订阅者监视时的引用计数。
    ref_count: usize,
    /// Set to true after we emit a Remove event due to the path becoming unreachable. When the
    /// path reappears, we force an event even if the metadata matches the previous baseline, so
    /// subscribers can recover from transient errors.
    reported_missing: bool,
}

impl EventEmitter<FsChangeEvent> for RemoteFsWatcher {}

fn watch_increment(
    watched: &mut HashMap<WatchKey, RemoteWatchState>,
    path: &Path,
    fs: Arc<dyn FileSystem>,
) {
    let Some(server) = fs_server_key(&fs) else {
        log::warn!(
            "RemoteFsWatcher watch called with a backend without ssh_config: {}",
            path.display()
        );
        return;
    };
    watched
        .entry(WatchKey {
            path: path.to_path_buf(),
            server: server.clone(),
        })
        .and_modify(|state| state.ref_count += 1)
        .or_insert(RemoteWatchState {
            fs,
            last_meta: None,
            last_dir_entries: None,
            last_content_hash: None,
            last_content_check: None,
            ref_count: 1,
            reported_missing: false,
        });
}

fn watch_decrement(
    watched: &mut HashMap<WatchKey, RemoteWatchState>,
    path: &Path,
    fs: &Arc<dyn FileSystem>,
) -> bool {
    let Some(server) = fs_server_key(fs) else {
        return false;
    };
    let key = WatchKey {
        path: path.to_path_buf(),
        server,
    };
    let Some(state) = watched.get_mut(&key) else {
        return false;
    };
    state.ref_count = state.ref_count.saturating_sub(1);
    if state.ref_count == 0 {
        watched.remove(&key);
        return true;
    }
    false
}

impl RemoteFsWatcher {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let watched = Arc::new(Mutex::new(HashMap::<WatchKey, RemoteWatchState>::new()));
        let watched_clone = watched.clone();
        let weak = cx.entity().downgrade();

        let _task = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(REMOTE_POLL_INTERVAL).await;

                // 快照本轮要轮询的 (路径, 服务器, fs, 上次 metadata, 上次内容校验时间)；
                // 键必须含服务器标识，否则两台服务器同路径时只会轮询到先注册的那台。
                let to_check: Vec<(
                    PathBuf,
                    String,
                    Arc<dyn FileSystem>,
                    Option<FileMetadata>,
                    Option<Instant>,
                )> = {
                    let lock = watched_clone.lock();
                    lock.iter()
                        .map(|(key, s)| {
                            (
                                key.path.clone(),
                                key.server.clone(),
                                s.fs.clone(),
                                s.last_meta.clone(),
                                s.last_content_check,
                            )
                        })
                        .collect()
                };

                if to_check.is_empty() {
                    continue;
                }

                let bg = cx.background_executor().clone();
                let all_results = futures::stream::iter(to_check)
                    .map(|(path, server, fs, prev_meta, last_content_check)| {
                        let bg = bg.clone();
                        async move {
                            bg.spawn(async move {
                                // 远程后端的 metadata/read_dir 是同步 SFTP RPC。
                                // 每轮最多 8 路并发，若直接跑在 GPUI 共享后台
                                // worker 上，网络稍慢就会占满线程池，把文件树
                                // 的根目录读取饿死。交给阻塞线程池，worker 只
                                // 负责等待结果。
                                run_blocking_io(move || {
                                    let meta = fs.metadata(&path);
                                    let (entries, content_hash, content_checked) = match &meta {
                                        Ok(m) if m.is_dir => (fs.read_dir(&path).ok(), None, false),
                                        Ok(m) => {
                                            let hash_candidate =
                                                m.size <= REMOTE_CONTENT_HASH_MAX_BYTES;
                                            let meta_changed = prev_meta.as_ref() != Some(m);
                                            let hash_due = hash_candidate
                                                && last_content_check.map_or(true, |at| {
                                                    at.elapsed() >= REMOTE_CONTENT_HASH_INTERVAL
                                                });
                                            if hash_candidate && (meta_changed || hash_due) {
                                                (
                                                    None,
                                                    fs.read_file_bytes(&path)
                                                        .ok()
                                                        .flatten()
                                                        .map(|bytes| hash_bytes(&bytes)),
                                                    true,
                                                )
                                            } else {
                                                // 大文件或还没到内容校验时间：本轮不读文件，
                                                // 状态更新时沿用旧 hash。
                                                (None, None, false)
                                            }
                                        }
                                        Err(_) => (None, None, false),
                                    };
                                    (path, server, meta, entries, content_hash, content_checked)
                                })
                                .await
                            })
                            .await
                        }
                    })
                    .buffer_unordered(8)
                    .collect::<Vec<_>>()
                    .await;

                for (path, server, meta, entries, content_hash, content_checked) in all_results {
                    let watch_key = WatchKey {
                        path: path.clone(),
                        server: server.clone(),
                    };
                    let meta = match meta {
                        Ok(m) => m,
                        Err(e) => {
                            let should_emit = {
                                let mut lock = watched_clone.lock();
                                if let Some(state) = lock.get_mut(&watch_key) {
                                    if state.last_meta.is_some() && !state.reported_missing {
                                        state.reported_missing = true;
                                        state.last_meta = None;
                                        state.last_dir_entries = None;
                                        state.last_content_hash = None;
                                        state.last_content_check = None;
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };
                            // 只在状态变迁（首次失败，伴随 Remove 事件）时 WARN；
                            // 之后每轮重复失败不再携带新信息，降为 debug，避免日志刷屏。
                            if should_emit {
                                log::warn!(
                                    "Remote metadata failed for {} ({}): {}",
                                    path.display(),
                                    server,
                                    e
                                );
                                if let Some(watcher) = weak.upgrade() {
                                    watcher.update(cx, |_, cx| {
                                        cx.emit(FsChangeEvent::remote(
                                            path.clone(),
                                            FsChangeKind::Remove,
                                            server.clone(),
                                        ));
                                    });
                                }
                            } else {
                                log::debug!(
                                    "Remote metadata failed for {} ({}): {}",
                                    path.display(),
                                    server,
                                    e
                                );
                            }
                            continue;
                        }
                    };

                    let (should_emit, kind, reported_missing) = {
                        let mut lock = watched_clone.lock();
                        if let Some(state) = lock.get_mut(&watch_key) {
                            let reported_missing = state.reported_missing;
                            state.reported_missing = false;
                            let prev_meta = state.last_meta.take();
                            let prev_entries = state.last_dir_entries.take();
                            let prev_hash = state.last_content_hash;

                            // 本轮可能为了省 SFTP 读取而跳过内容哈希；此时沿用
                            // 旧 hash，避免把 `None` 误判成“内容变了”而每轮发
                            // Modify 事件。
                            let effective_hash = content_hash.or(prev_hash);

                            let (emit, kind) = detect_remote_change(
                                prev_meta.as_ref(),
                                prev_entries.as_deref(),
                                prev_hash,
                                &meta,
                                entries.as_deref(),
                                effective_hash,
                                reported_missing,
                            );

                            state.last_meta = Some(meta);
                            state.last_dir_entries = entries.clone();
                            state.last_content_hash = effective_hash;
                            if content_checked {
                                state.last_content_check = Some(Instant::now());
                            }
                            (emit || reported_missing, kind, reported_missing)
                        } else {
                            (false, FsChangeKind::Modify, false)
                        }
                    };

                    if should_emit && let Some(watcher) = weak.upgrade() {
                        // On recovery from a previously reported missing state, force a Create
                        // event so subscribers reload the path even if its metadata matches
                        // the old baseline.
                        let event = if reported_missing {
                            FsChangeEvent::remote(
                                path.clone(),
                                FsChangeKind::Create,
                                server.clone(),
                            )
                        } else if let Some(entries) = entries {
                            FsChangeEvent::remote_with_entries(
                                path.clone(),
                                kind,
                                server.clone(),
                                entries,
                            )
                        } else {
                            FsChangeEvent::remote(path.clone(), kind, server.clone())
                        };
                        watcher.update(cx, |_, cx| {
                            cx.emit(event);
                        });
                    }
                }
            }
        });

        Self { watched, _task }
    }

    pub fn watch(&mut self, path: &Path, fs: Arc<dyn FileSystem>, cx: &mut Context<Self>) {
        let mut lock = self.watched.lock();
        watch_increment(&mut lock, path, fs);
        cx.notify();
    }

    /// 停止一次 watch。`fs` 必须与当初 `watch` 时是同一台服务器（ssh_config 派生 key），
    /// 否则无法定位到正确的引用计数。
    pub fn unwatch(&mut self, path: &Path, fs: &Arc<dyn FileSystem>, cx: &mut Context<Self>) {
        let mut lock = self.watched.lock();
        if !watch_decrement(&mut lock, path, fs) {
            log::warn!(
                "RemoteFsWatcher unwatch called for {} which is not watched",
                path.display()
            );
        }
        cx.notify();
    }
}

/// Helper trait to subscribe to the global watcher from a component context.
///
/// 只有 `fs_watcher` / `remote_fs_watcher` 是每个实现的真实差异点，
/// 其余 6 个方法均为默认实现（`let watcher = self.fs_watcher();
/// watcher.update(self, …)`），App / Context 两个 impl 不再各自复制一份。
pub trait WatcherContext: AppContext + Sized {
    fn fs_watcher(&self) -> Entity<FsWatcher>;
    fn remote_fs_watcher(&self) -> Entity<RemoteFsWatcher>;

    fn watch_path(&mut self, path: &Path) {
        let watcher = self.fs_watcher();
        watcher.update(self, |w, cx| w.watch(path, cx));
    }

    fn watch_recursive_path(&mut self, path: &Path) {
        let watcher = self.fs_watcher();
        watcher.update(self, |w, cx| w.watch_recursive(path, cx));
    }

    fn unwatch_path(&mut self, path: &Path) {
        let watcher = self.fs_watcher();
        watcher.update(self, |w, cx| w.unwatch(path, cx));
    }

    fn unwatch_recursive_path(&mut self, path: &Path) {
        let watcher = self.fs_watcher();
        watcher.update(self, |w, cx| w.unwatch_recursive(path, cx));
    }

    fn watch_remote_path(&mut self, path: &Path, fs: Arc<dyn FileSystem>) {
        let watcher = self.remote_fs_watcher();
        watcher.update(self, |w, cx| w.watch(path, fs, cx));
    }

    /// 停止远程 watch。`fs` 用于定位到正确的服务器 watch 状态
    /// （两台服务器可能 watch 同一路径，见 [`WatchKey`]）。
    fn unwatch_remote_path(&mut self, path: &Path, fs: &Arc<dyn FileSystem>) {
        let watcher = self.remote_fs_watcher();
        watcher.update(self, |w, cx| w.unwatch(path, fs, cx));
    }
}

impl WatcherContext for App {
    fn fs_watcher(&self) -> Entity<FsWatcher> {
        FsWatcherGlobal::watcher(self)
    }

    fn remote_fs_watcher(&self) -> Entity<RemoteFsWatcher> {
        RemoteFsWatcherGlobal::watcher(self)
    }
}

impl<'a, T> WatcherContext for Context<'a, T> {
    fn fs_watcher(&self) -> Entity<FsWatcher> {
        FsWatcherGlobal::watcher(self)
    }

    fn remote_fs_watcher(&self) -> Entity<RemoteFsWatcher> {
        RemoteFsWatcherGlobal::watcher(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_manager::filesystem::LocalFileSystem;
    use crate::file_manager::types::FileEntryKind;
    use crate::ssh::SshConnectionConfig;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    fn meta(is_dir: bool, size: u64, mtime: Option<std::time::SystemTime>) -> FileMetadata {
        FileMetadata {
            is_dir,
            size,
            modified_at: mtime,
        }
    }

    fn entry(name: &str, kind: FileEntryKind) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: PathBuf::from(name),
            kind,
            size: 0,
        }
    }

    #[test]
    fn hash_bytes_detects_content_difference() {
        let a = hash_bytes(b"hello");
        let b = hash_bytes(b"world");
        let c = hash_bytes(b"hello");
        assert_ne!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn detect_remote_change_file_content_changed_same_metadata() {
        let old = meta(false, 5, None);
        let new = meta(false, 5, None);
        let old_hash = Some(hash_bytes(b"hello"));
        let new_hash = Some(hash_bytes(b"world"));

        let (emit, kind) =
            detect_remote_change(Some(&old), None, old_hash, &new, None, new_hash, false);
        assert!(emit);
        assert_eq!(kind, FsChangeKind::Modify);
    }

    #[test]
    fn detect_remote_change_file_content_same() {
        let old = meta(false, 5, None);
        let new = meta(false, 5, None);
        let hash = Some(hash_bytes(b"hello"));

        let (emit, _) = detect_remote_change(Some(&old), None, hash, &new, None, hash, false);
        assert!(!emit);
    }

    #[test]
    fn detect_remote_change_file_metadata_changed() {
        let old = meta(false, 5, None);
        let new = meta(false, 6, None);
        let hash = Some(hash_bytes(b"hello"));

        let (emit, kind) = detect_remote_change(Some(&old), None, hash, &new, None, hash, false);
        assert!(emit);
        assert_eq!(kind, FsChangeKind::Modify);
    }

    #[test]
    fn detect_remote_change_file_removed_and_recreated() {
        let old = meta(false, 5, None);
        let new = meta(false, 5, None);
        let hash = Some(hash_bytes(b"hello"));

        let (emit, kind) = detect_remote_change(Some(&old), None, hash, &new, None, hash, true);
        assert!(emit);
        assert_eq!(kind, FsChangeKind::Create);
    }

    #[test]
    fn detect_remote_change_dir_entries_changed() {
        let old_meta = meta(true, 0, None);
        let new_meta = meta(true, 0, None);
        let old_entries = vec![entry("a.txt", FileEntryKind::File)];
        let new_entries = vec![
            entry("a.txt", FileEntryKind::File),
            entry("b.txt", FileEntryKind::File),
        ];

        let (emit, kind) = detect_remote_change(
            Some(&old_meta),
            Some(&old_entries),
            None,
            &new_meta,
            Some(&new_entries),
            None,
            false,
        );
        assert!(emit);
        assert_eq!(kind, FsChangeKind::Modify);
    }

    #[test]
    fn detect_remote_change_dir_entries_same() {
        let old_meta = meta(true, 0, None);
        let new_meta = meta(true, 0, None);
        let entries = vec![entry("a.txt", FileEntryKind::File)];

        let (emit, _) = detect_remote_change(
            Some(&old_meta),
            Some(&entries),
            None,
            &new_meta,
            Some(&entries),
            None,
            false,
        );
        assert!(!emit);
    }

    #[test]
    fn detect_remote_change_file_to_dir() {
        let old = meta(false, 5, None);
        let new = meta(true, 0, None);
        let hash = Some(hash_bytes(b"hello"));

        let (emit, kind) = detect_remote_change(Some(&old), None, hash, &new, None, None, false);
        assert!(emit);
        // 沿用原有逻辑：只有 dir -> file 才返回 Remove，file -> dir 返回 Modify
        assert_eq!(kind, FsChangeKind::Modify);
    }

    #[test]
    fn detect_remote_change_file_first_poll_no_emit() {
        let new = meta(false, 5, None);
        let hash = Some(hash_bytes(b"hello"));

        let (emit, _) = detect_remote_change(None, None, None, &new, None, hash, false);
        assert!(!emit, "文件首轮轮询不主动同步（编辑器自行校验内容）");
    }

    /// 目录首轮轮询必须 emit（携带真实 entries 的 Modify）：
    /// 否则 watch 注册后 ~ 一个轮询周期内的变更会被基线吞掉，
    /// 已删除的文件将无限期留在文件树上。
    #[test]
    fn detect_remote_change_dir_first_poll_emits_sync() {
        let new_meta = meta(true, 0, None);
        let entries = vec![entry("a.txt", FileEntryKind::File)];

        let (emit, kind) =
            detect_remote_change(None, None, None, &new_meta, Some(&entries), None, false);
        assert!(emit, "目录首轮轮询必须发同步事件");
        assert_eq!(kind, FsChangeKind::Modify);
    }

    #[test]
    fn watch_ref_counting() {
        let mut watched = HashMap::new();
        let path = Path::new("/tmp/test.txt");
        let mock = RemoteMockFs::new("ref-count-host", &[]);
        let key = WatchKey {
            path: path.to_path_buf(),
            server: remote_server_key(&mock.config),
        };
        let fs: Arc<dyn FileSystem> = Arc::new(mock);

        watch_increment(&mut watched, path, fs.clone());
        assert_eq!(watched.get(&key).map(|s| s.ref_count), Some(1));

        watch_increment(&mut watched, path, fs.clone());
        assert_eq!(watched.get(&key).map(|s| s.ref_count), Some(2));

        assert!(!watch_decrement(&mut watched, path, &fs));
        assert_eq!(watched.get(&key).map(|s| s.ref_count), Some(1));

        assert!(watch_decrement(&mut watched, path, &fs));
        assert!(watched.is_empty());

        assert!(!watch_decrement(&mut watched, path, &fs));
    }

    /// 无 ssh_config 的后端（本地 fs 误入远程轮询器）：不注册、不 panic
    #[test]
    fn watch_increment_rejects_backend_without_ssh_config() {
        let mut watched = HashMap::new();
        let fs = Arc::new(LocalFileSystem) as Arc<dyn FileSystem>;
        watch_increment(&mut watched, Path::new("/tmp"), fs);
        assert!(watched.is_empty());
    }

    /// 同路径不同服务器：各自独立计数，互不干扰（回归：多主机同 cwd 切换）
    #[test]
    fn watch_ref_counting_isolates_servers_on_same_path() {
        let mut watched = HashMap::new();
        let mock_a = RemoteMockFs::new("srv-a", &[]);
        let mock_b = RemoteMockFs::new("srv-b", &[]);
        let server_b = remote_server_key(&mock_b.config);
        let fs_a: Arc<dyn FileSystem> = Arc::new(mock_a);
        let fs_b: Arc<dyn FileSystem> = Arc::new(mock_b);
        let path = Path::new("/root");

        // 服务器 A 的树 watch /root，随后服务器 B 的树也 watch /root
        watch_increment(&mut watched, path, fs_a.clone());
        watch_increment(&mut watched, path, fs_b.clone());

        // 两条独立状态（若 key 冲突则只会剩一条 ref=2）
        assert_eq!(
            watched.len(),
            2,
            "同路径的两台服务器必须是独立的 watch 状态"
        );

        // 各自减到 0 各自移除；A unwatch 不应影响 B 的状态
        assert!(watch_decrement(&mut watched, path, &fs_a));
        assert_eq!(
            watched
                .iter()
                .find(|(k, _)| k.path == path && k.server == server_b)
                .map(|(_, s)| s.ref_count),
            Some(1),
            "A 的 unwatch 不应移除 B 的 watch 状态"
        );
        assert!(watch_decrement(&mut watched, path, &fs_b));
        assert!(watched.is_empty());
    }

    /// 订阅方服务器过滤的四种匹配分支
    #[test]
    fn fs_change_event_is_from_backend_filters_by_server() {
        let mut config = SshConnectionConfig::local();
        config.host = "srv-a".to_string();
        let mut other = SshConnectionConfig::local();
        other.host = "srv-b".to_string();

        let remote_event = FsChangeEvent::remote(
            PathBuf::from("/root"),
            FsChangeKind::Modify,
            remote_server_key(&config),
        );
        let local_event = FsChangeEvent::new(PathBuf::from("/root"), FsChangeKind::Modify);

        // 远程事件：只匹配同一台服务器
        assert!(remote_event.is_from_backend(Some(&config)));
        assert!(!remote_event.is_from_backend(Some(&other)));
        assert!(
            !remote_event.is_from_backend(None),
            "远程事件不匹配本地后端"
        );

        // 本地事件：只匹配本地后端
        assert!(local_event.is_from_backend(None));
        assert!(!local_event.is_from_backend(Some(&config)));
    }

    // ── 轮询链路端到端测试（TestAppContext 确定性驱动，无需真实网络）──

    /// 可动态变更目录项的远程 mock 文件系统（模拟一台 SFTP 服务器）。
    struct RemoteMockFs {
        entries: StdMutex<Vec<FileEntry>>,
        config: SshConnectionConfig,
    }

    impl RemoteMockFs {
        fn new(host: &str, names: &[&str]) -> Self {
            let mut config = SshConnectionConfig::local();
            config.host = host.to_string();
            config.username = format!("user-{host}");
            Self {
                entries: StdMutex::new(
                    names
                        .iter()
                        .map(|n| FileEntry {
                            name: (*n).to_string(),
                            path: PathBuf::from(format!("/root/{n}")),
                            kind: FileEntryKind::File,
                            size: 0,
                        })
                        .collect(),
                ),
                config,
            }
        }
        fn remove(&self, name: &str) {
            self.entries.lock().unwrap().retain(|e| e.name != name);
        }
    }

    impl FileSystem for RemoteMockFs {
        fn read_dir(&self, _path: &Path) -> Result<Vec<FileEntry>, String> {
            Ok(self.entries.lock().unwrap().clone())
        }

        fn metadata(&self, _path: &Path) -> Result<FileMetadata, String> {
            Ok(FileMetadata {
                is_dir: true,
                size: 0,
                modified_at: None,
            })
        }

        fn read_file_bytes(&self, _path: &Path) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }

        fn home_dir(&self) -> Option<PathBuf> {
            None
        }

        fn ssh_config(&self) -> Option<SshConnectionConfig> {
            Some(self.config.clone())
        }

        fn is_local(&self) -> bool {
            false
        }
    }

    /// 事件收集探针：持有订阅，把收到的事件存入共享 Vec。
    struct EventProbe {
        _sub: Option<gpui::Subscription>,
    }

    fn make_probe(
        cx: &mut gpui::TestAppContext,
        watcher: &gpui::Entity<RemoteFsWatcher>,
        events: Arc<StdMutex<Vec<FsChangeEvent>>>,
    ) -> gpui::Entity<EventProbe> {
        cx.new(|cx| {
            let sink = events;
            let sub = cx.subscribe(watcher, move |_, _, event: &FsChangeEvent, _| {
                sink.lock().unwrap().push(event.clone());
            });
            EventProbe { _sub: Some(sub) }
        })
    }

    /// 断言失败时的诊断摘要：只取路径与事件类型，避免打印整个 entries。
    fn brief(events: &[FsChangeEvent]) -> Vec<(PathBuf, FsChangeKind)> {
        events
            .iter()
            .map(|e| (e.path.clone(), e.kind.clone()))
            .collect()
    }

    /// 测试时钟推进后，轮询任务仍需被 executor 实际跑完才发事件。
    fn wait_for_events(
        cx: &mut gpui::TestAppContext,
        events: &Arc<StdMutex<Vec<FsChangeEvent>>>,
        predicate: impl Fn(&[FsChangeEvent]) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            if predicate(&events.lock().unwrap()) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 轮询链路端到端测试：被 watch 目录内删除文件 → 下一轮轮询应发出携带新 entries 的 Modify 事件
    #[gpui::test]
    fn remote_watcher_emits_modify_with_entries_on_delete(cx: &mut gpui::TestAppContext) {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let watcher = cx.new(|cx| RemoteFsWatcher::new(cx));
        let _probe = make_probe(cx, &watcher, events.clone());

        let fs = Arc::new(RemoteMockFs::new("srv-a", &["a.txt", "b.txt"]));
        watcher.update(cx, |w, cx| w.watch(Path::new("/root"), fs.clone(), cx));

        // 首轮轮询：目录首次轮询会发一次携带真实 entries 的同步事件（幂等，
        // 订阅方对比后无变化则无事发生）
        cx.executor().advance_clock(REMOTE_POLL_INTERVAL);
        wait_for_events(cx, &events, |received| {
            received
                .iter()
                .any(|e| e.path == Path::new("/root") && e.kind == FsChangeKind::Modify)
        });
        {
            let received = events.lock().unwrap();
            let sync = received
                .iter()
                .find(|e| e.path == Path::new("/root") && e.kind == FsChangeKind::Modify);
            assert!(
                sync.is_some(),
                "目录首轮轮询应发一次同步事件，实际收到: {:?}",
                brief(&received)
            );
            let entries = sync
                .unwrap()
                .entries
                .as_ref()
                .expect("同步事件应携带真实 entries");
            assert!(
                entries.iter().any(|e| e.name == "b.txt"),
                "同步事件应反映删除前的真实状态"
            );
        }
        events.lock().unwrap().clear();

        // 模拟终端里 `rm b.txt`
        fs.remove("b.txt");
        cx.executor().advance_clock(REMOTE_POLL_INTERVAL);
        wait_for_events(cx, &events, |received| {
            received
                .iter()
                .any(|e| e.path == Path::new("/root") && e.kind == FsChangeKind::Modify)
        });

        let received = events.lock().unwrap();
        let modify = received
            .iter()
            .find(|e| e.path == Path::new("/root") && e.kind == FsChangeKind::Modify);
        assert!(
            modify.is_some(),
            "删除文件后应收到 /root 的 Modify 事件，实际收到: {:?}",
            brief(&received)
        );
        let modify = modify.unwrap();
        assert!(
            modify.server.is_some(),
            "远程事件必须携带服务器标识，供订阅方过滤"
        );
        let entries = modify.entries.as_ref().expect("事件应携带新 entries");
        assert!(
            entries.iter().all(|e| e.name != "b.txt"),
            "事件中的 entries 不应再包含 b.txt"
        );
    }

    /// 复现用户报告的 Bug：两台服务器（两个终端 tab）watch 同一路径时，
    /// 轮询只看第一台服务器，第二台终端里删除文件 → 文件树永远不刷新。
    ///
    /// 场景对应 `set_terminal_file_tree` 的真实切换顺序：
    /// 新树的 watch 先发生（旧状态仍在），旧树 cleanup 的 unwatch 后发生。
    #[gpui::test]
    fn remote_watcher_distinguishes_servers_watching_same_path(cx: &mut gpui::TestAppContext) {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let watcher = cx.new(|cx| RemoteFsWatcher::new(cx));
        let _probe = make_probe(cx, &watcher, events.clone());

        // 服务器 A 的树先 watch /root
        let fs_a = Arc::new(RemoteMockFs::new("srv-a", &["a.txt", "b.txt"]));
        watcher.update(cx, |w, cx| w.watch(Path::new("/root"), fs_a.clone(), cx));
        cx.executor().advance_clock(REMOTE_POLL_INTERVAL);
        wait_for_events(cx, &events, |received| {
            received
                .iter()
                .any(|e| e.path == Path::new("/root") && e.kind == FsChangeKind::Modify)
        });
        events.lock().unwrap().clear();

        // 切换到服务器 B 的终端：新树 watch /root（真实顺序：旧状态还在）
        let fs_b = Arc::new(RemoteMockFs::new("srv-b", &["a.txt", "b.txt"]));
        watcher.update(cx, |w, cx| w.watch(Path::new("/root"), fs_b.clone(), cx));
        // 旧树 cleanup：unwatch
        watcher.update(cx, |w, cx| {
            w.unwatch(
                Path::new("/root"),
                &(fs_a.clone() as Arc<dyn FileSystem>),
                cx,
            )
        });

        // 在服务器 B 的终端里 `rm b.txt`
        fs_b.remove("b.txt");
        cx.executor().advance_clock(REMOTE_POLL_INTERVAL * 3);
        wait_for_events(cx, &events, |received| {
            received
                .iter()
                .any(|e| e.path == Path::new("/root") && e.kind == FsChangeKind::Modify)
        });

        let received = events.lock().unwrap();
        let modify = received
            .iter()
            .find(|e| e.path == Path::new("/root") && e.kind == FsChangeKind::Modify);
        assert!(
            modify.is_some(),
            "服务器 B 上删除文件后必须产生事件（否则 B 的文件树永久停留旧内容），\
             实际收到: {:?}",
            brief(&received)
        );
        let modify = modify.unwrap();
        assert_eq!(
            modify.server.as_deref(),
            Some(remote_server_key(&fs_b.config).as_str()),
            "事件必须标记来源服务器，订阅方才能过滤其他主机的事件"
        );
        let entries = modify.entries.as_ref().expect("事件应携带新 entries");
        assert!(
            entries.iter().all(|e| e.name != "b.txt"),
            "事件中的 entries 应反映服务器 B 的新状态（不含 b.txt）"
        );
    }
}
