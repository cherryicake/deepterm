use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;

use gpui::{App, AppContext, Context, Entity, EventEmitter, Global};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::file_manager::filesystem::{FileMetadata, FileSystem};
use crate::file_manager::types::FileEntry;

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
}

impl FsChangeEvent {
    pub fn new(path: PathBuf, kind: FsChangeKind) -> Self {
        Self {
            path,
            kind,
            entries: None,
        }
    }

    pub fn with_entries(path: PathBuf, kind: FsChangeKind, entries: Vec<FileEntry>) -> Self {
        Self {
            path,
            kind,
            entries: Some(entries),
        }
    }
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

    pub fn unwatch(path: &Path, cx: &mut App) {
        let watcher = Self::watcher(cx);
        watcher.update(cx, |w, cx| w.unwatch(path, cx));
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
    watcher: Arc<Mutex<RecommendedWatcher>>,
    watched_paths: Arc<Mutex<HashMap<PathBuf, WatchRefs>>>,
    _task: gpui::Task<()>,
}

impl EventEmitter<FsChangeEvent> for FsWatcher {}

impl FsWatcher {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();

        let watcher = Arc::new(Mutex::new(
            match RecommendedWatcher::new(
                move |res: Result<Event, notify::Error>| {
                    if let Ok(event) = res {
                        let _ = tx.try_send(event);
                    }
                },
                notify::Config::default().with_poll_interval(Duration::from_millis(500)),
            ) {
                Ok(w) => w,
                Err(e) => {
                    log::error!("Failed to create file watcher: {e}");
                    panic!("Failed to create file watcher: {e}");
                }
            },
        ));

        let watched_paths = Arc::new(Mutex::new(HashMap::new()));
        let weak = cx.entity().downgrade();

        let _task = cx
            .spawn(async move |_, cx| {
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
            let entry = refs
                .entry(path.clone())
                .or_insert(WatchRefs { non_recursive: 0, recursive: 0 });
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
            let mut watcher = self.watcher.lock();
            if need_upgrade {
                if let Err(e) = watcher.unwatch(&path) {
                    log::warn!("Failed to reconfigure watch for {}: {e}", path.display());
                }
            }
            if let Err(e) = watcher.watch(&path, RecursiveMode::Recursive) {
                log::warn!("Failed to watch {}: {e}", path.display());
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
            let entry = refs
                .entry(path.clone())
                .or_insert(WatchRefs { non_recursive: 0, recursive: 0 });
            entry.non_recursive += 1;
            entry.non_recursive == 1 && entry.recursive == 0
        };
        if need_start {
            let mut watcher = self.watcher.lock();
            if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                log::warn!("Failed to watch {}: {e}", path.display());
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
            let mut watcher = self.watcher.lock();
            if let Err(e) = watcher.unwatch(&path) {
                log::warn!("Failed to unwatch {}: {e}", path.display());
            }
            if need_downgrade {
                if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
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
            let mut watcher = self.watcher.lock();
            if let Err(e) = watcher.unwatch(&path) {
                log::warn!("Failed to unwatch {}: {e}", path.display());
            }
            if need_downgrade {
                if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                    log::warn!("Failed to reconfigure watch for {}: {e}", path.display());
                }
            }
        }
        cx.notify();
    }
}

const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Remote file system watcher that polls `metadata` for watched paths because remote SFTP
/// directories do not support native OS-level filesystem notifications.
///
/// Emits the same `FsChangeEvent` events as the local watcher so that subscribers do not need to
/// distinguish between the two backends.
pub struct RemoteFsWatcher {
    watched: Arc<Mutex<HashMap<PathBuf, RemoteWatchState>>>,
    _task: gpui::Task<()>,
}

struct RemoteWatchState {
    fs: Arc<dyn FileSystem>,
    last_meta: Option<FileMetadata>,
    last_dir_entries: Option<Vec<FileEntry>>,
    /// Set to true after we emit a Remove event due to the path becoming unreachable. When the
    /// path reappears, we force an event even if the metadata matches the previous baseline, so
    /// subscribers can recover from transient errors.
    reported_missing: bool,
}

impl EventEmitter<FsChangeEvent> for RemoteFsWatcher {}

impl RemoteFsWatcher {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let watched = Arc::new(Mutex::new(HashMap::<PathBuf, RemoteWatchState>::new()));
        let watched_clone = watched.clone();
        let weak = cx.entity().downgrade();

        let _task = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(REMOTE_POLL_INTERVAL)
                    .await;

                let to_check: Vec<(PathBuf, Arc<dyn FileSystem>)> = {
                    let lock = watched_clone.lock();
                    lock.iter()
                        .map(|(p, s)| (p.clone(), s.fs.clone()))
                        .collect()
                };

                for (path, fs) in to_check {
                    let (meta, entries) = cx
                        .background_executor()
                        .spawn({
                            let path = path.clone();
                            let fs = fs.clone();
                            async move {
                                let meta = fs.metadata(&path);
                                let entries = if let Ok(ref m) = meta {
                                    if m.is_dir {
                                        fs.read_dir(&path).ok()
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };
                                (meta, entries)
                            }
                        })
                        .await;

                    let meta = match meta {
                        Ok(m) => m,
                        Err(e) => {
                            log::warn!(
                                "Remote metadata failed for {}: {}",
                                path.display(),
                                e
                            );
                            let should_emit = {
                                let mut lock = watched_clone.lock();
                                if let Some(state) = lock.get_mut(&path) {
                                    if state.last_meta.is_some() && !state.reported_missing {
                                        state.reported_missing = true;
                                        state.last_meta = None;
                                        state.last_dir_entries = None;
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };
                            if should_emit {
                                if let Some(watcher) = weak.upgrade() {
                                    watcher.update(cx, |_, cx| {
                                        cx.emit(FsChangeEvent::new(
                                            path.clone(),
                                            FsChangeKind::Remove,
                                        ));
                                    });
                                }
                            }
                            continue;
                        }
                    };

                    let (should_emit, kind, reported_missing) = {
                        let mut lock = watched_clone.lock();
                        if let Some(state) = lock.get_mut(&path) {
                            let reported_missing = state.reported_missing;
                            state.reported_missing = false;
                            let prev_meta = state.last_meta.take();
                            let prev_entries = state.last_dir_entries.take();
                            let prev_is_dir = prev_meta.as_ref().map(|m| m.is_dir);

                            let emit = if let Some(prev_meta) = prev_meta {
                                if prev_meta.is_dir != meta.is_dir {
                                    true
                                } else if meta.is_dir {
                                    if let Some(ref new_entries) = entries {
                                        prev_entries.as_ref() != Some(new_entries)
                                    } else {
                                        prev_entries.is_some()
                                    }
                                } else {
                                    prev_meta != meta
                                }
                            } else {
                                false
                            };

                            let kind = if prev_is_dir == Some(true) && !meta.is_dir {
                                FsChangeKind::Remove
                            } else {
                                FsChangeKind::Modify
                            };

                            state.last_meta = Some(meta);
                            state.last_dir_entries = entries.clone();
                            (emit || reported_missing, kind, reported_missing)
                        } else {
                            (false, FsChangeKind::Modify, false)
                        }
                    };

                    if should_emit {
                        if let Some(watcher) = weak.upgrade() {
                            // On recovery from a previously reported missing state, force a Create
                            // event so subscribers reload the path even if its metadata matches
                            // the old baseline.
                            let event = if reported_missing {
                                FsChangeEvent::new(path.clone(), FsChangeKind::Create)
                            } else if let Some(entries) = entries {
                                FsChangeEvent::with_entries(
                                    path.clone(),
                                    kind,
                                    entries,
                                )
                            } else {
                                FsChangeEvent::new(path.clone(), kind)
                            };
                            watcher.update(cx, |_, cx| {
                                cx.emit(event);
                            });
                        }
                    }
                }
            }
        });

        Self { watched, _task }
    }

    pub fn watch(&mut self, path: &Path, fs: Arc<dyn FileSystem>, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let mut lock = self.watched.lock();
        lock.entry(path).or_insert(RemoteWatchState {
            fs,
            last_meta: None,
            last_dir_entries: None,
            reported_missing: false,
        });
        cx.notify();
    }

    pub fn unwatch(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let mut lock = self.watched.lock();
        lock.remove(&path);
        cx.notify();
    }
}

/// Helper trait to subscribe to the global watcher from a component context.
pub trait WatcherContext {
    fn fs_watcher(&self) -> Entity<FsWatcher>;
    fn remote_fs_watcher(&self) -> Entity<RemoteFsWatcher>;
    fn watch_path(&mut self, path: &Path);
    fn watch_recursive_path(&mut self, path: &Path);
    fn unwatch_path(&mut self, path: &Path);
    fn unwatch_recursive_path(&mut self, path: &Path);
    fn watch_remote_path(&mut self, path: &Path, fs: Arc<dyn FileSystem>);
    fn unwatch_remote_path(&mut self, path: &Path);
}

impl WatcherContext for App {
    fn fs_watcher(&self) -> Entity<FsWatcher> {
        FsWatcherGlobal::watcher(self)
    }

    fn remote_fs_watcher(&self) -> Entity<RemoteFsWatcher> {
        RemoteFsWatcherGlobal::watcher(self)
    }

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

    fn unwatch_remote_path(&mut self, path: &Path) {
        let watcher = self.remote_fs_watcher();
        watcher.update(self, |w, cx| w.unwatch(path, cx));
    }
}

impl<'a, T> WatcherContext for Context<'a, T> {
    fn fs_watcher(&self) -> Entity<FsWatcher> {
        FsWatcherGlobal::watcher(self)
    }

    fn remote_fs_watcher(&self) -> Entity<RemoteFsWatcher> {
        RemoteFsWatcherGlobal::watcher(self)
    }

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

    fn unwatch_remote_path(&mut self, path: &Path) {
        let watcher = self.remote_fs_watcher();
        watcher.update(self, |w, cx| w.unwatch(path, cx));
    }
}
