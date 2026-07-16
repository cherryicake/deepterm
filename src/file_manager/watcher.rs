use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
            let entry = refs.entry(path.clone()).or_insert(WatchRefs {
                non_recursive: 0,
                recursive: 0,
            });
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
    watched: Arc<Mutex<HashMap<PathBuf, RemoteWatchState>>>,
    _task: gpui::Task<()>,
}

struct RemoteWatchState {
    fs: Arc<dyn FileSystem>,
    last_meta: Option<FileMetadata>,
    last_dir_entries: Option<Vec<FileEntry>>,
    last_content_hash: Option<u64>,
    /// 同一路径被多个订阅者监视时的引用计数。
    ref_count: usize,
    /// Set to true after we emit a Remove event due to the path becoming unreachable. When the
    /// path reappears, we force an event even if the metadata matches the previous baseline, so
    /// subscribers can recover from transient errors.
    reported_missing: bool,
}

impl EventEmitter<FsChangeEvent> for RemoteFsWatcher {}

fn watch_increment(
    watched: &mut HashMap<PathBuf, RemoteWatchState>,
    path: &Path,
    fs: Arc<dyn FileSystem>,
) {
    watched
        .entry(path.to_path_buf())
        .and_modify(|state| state.ref_count += 1)
        .or_insert(RemoteWatchState {
            fs,
            last_meta: None,
            last_dir_entries: None,
            last_content_hash: None,
            ref_count: 1,
            reported_missing: false,
        });
}

fn watch_decrement(watched: &mut HashMap<PathBuf, RemoteWatchState>, path: &Path) -> bool {
    if let Some(state) = watched.get_mut(path) {
        state.ref_count = state.ref_count.saturating_sub(1);
        if state.ref_count == 0 {
            watched.remove(path);
            return true;
        }
    }
    false
}

impl RemoteFsWatcher {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let watched = Arc::new(Mutex::new(HashMap::<PathBuf, RemoteWatchState>::new()));
        let watched_clone = watched.clone();
        let weak = cx.entity().downgrade();

        let _task = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(REMOTE_POLL_INTERVAL).await;

                let to_check: Vec<(PathBuf, Arc<dyn FileSystem>)> = {
                    let lock = watched_clone.lock();
                    lock.iter()
                        .map(|(p, s)| (p.clone(), s.fs.clone()))
                        .collect()
                };

                for (path, fs) in to_check {
                    let (meta, entries, content_hash) = cx
                        .background_executor()
                        .spawn({
                            let path = path.clone();
                            let fs = fs.clone();
                            async move {
                                let meta = fs.metadata(&path);
                                let mut entries = None;
                                let mut content_hash = None;
                                if let Ok(ref m) = meta {
                                    if m.is_dir {
                                        entries = fs.read_dir(&path).ok();
                                    } else {
                                        content_hash = fs
                                            .read_file_bytes(&path)
                                            .ok()
                                            .flatten()
                                            .map(|bytes| hash_bytes(&bytes));
                                    }
                                }
                                (meta, entries, content_hash)
                            }
                        })
                        .await;

                    let meta = match meta {
                        Ok(m) => m,
                        Err(e) => {
                            log::warn!("Remote metadata failed for {}: {}", path.display(), e);
                            let should_emit = {
                                let mut lock = watched_clone.lock();
                                if let Some(state) = lock.get_mut(&path) {
                                    if state.last_meta.is_some() && !state.reported_missing {
                                        state.reported_missing = true;
                                        state.last_meta = None;
                                        state.last_dir_entries = None;
                                        state.last_content_hash = None;
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
                            let prev_hash = state.last_content_hash.take();

                            let (emit, kind) = detect_remote_change(
                                prev_meta.as_ref(),
                                prev_entries.as_deref(),
                                prev_hash,
                                &meta,
                                entries.as_deref(),
                                content_hash,
                                reported_missing,
                            );

                            state.last_meta = Some(meta);
                            state.last_dir_entries = entries.clone();
                            state.last_content_hash = content_hash;
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
                                FsChangeEvent::with_entries(path.clone(), kind, entries)
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
        let mut lock = self.watched.lock();
        watch_increment(&mut lock, path, fs);
        cx.notify();
    }

    pub fn unwatch(&mut self, path: &Path, cx: &mut Context<Self>) {
        let mut lock = self.watched.lock();
        if !watch_decrement(&mut lock, path) {
            log::warn!(
                "RemoteFsWatcher unwatch called for {} which is not watched",
                path.display()
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_manager::filesystem::LocalFileSystem;
    use crate::file_manager::types::FileEntryKind;
    use std::path::PathBuf;

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
    fn detect_remote_change_first_poll_no_emit() {
        let new = meta(false, 5, None);
        let hash = Some(hash_bytes(b"hello"));

        let (emit, _) = detect_remote_change(None, None, None, &new, None, hash, false);
        assert!(!emit);
    }

    #[test]
    fn watch_ref_counting() {
        let mut watched = HashMap::new();
        let fs = Arc::new(LocalFileSystem) as Arc<dyn FileSystem>;
        let path = Path::new("/tmp/test.txt");

        watch_increment(&mut watched, path, fs.clone());
        assert_eq!(watched.get(path).map(|s| s.ref_count), Some(1));

        watch_increment(&mut watched, path, fs.clone());
        assert_eq!(watched.get(path).map(|s| s.ref_count), Some(2));

        assert!(!watch_decrement(&mut watched, path));
        assert_eq!(watched.get(path).map(|s| s.ref_count), Some(1));

        assert!(watch_decrement(&mut watched, path));
        assert!(watched.get(path).is_none());

        assert!(!watch_decrement(&mut watched, path));
    }
}
