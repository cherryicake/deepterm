use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::file_manager::FileSystem;
use crate::file_manager::file_tree_cache::RemoteFileTreeCache;
use crate::file_manager::types::FileEntry;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind};
use crate::ssh::SshConnectionConfig;

/// 文件树数据加载器 —— 纯数据层，不依赖 GPUI 渲染。
///
/// 负责目录缓存管理、加载状态跟踪、共享缓存交互。
/// View 层（`SidebarFileTree`）持有此结构体，将文件操作委托给它。
///
/// 构造时对共享缓存 `acquire` 一份引用，`Drop` 时归还，
/// 保证引用计数严格配对（本地树与远程树走同一路径）。
pub struct FileTreeLoader {
    /// path -> entries，本地工作集缓存
    directory_cache: HashMap<PathBuf, Vec<FileEntry>>,
    /// 正在加载的路径
    loading_paths: HashSet<PathBuf>,
    /// 等待刷新的路径（加载期间收到的刷新请求）
    pending_refresh: HashSet<PathBuf>,
    /// 后端文件系统
    backend: Arc<dyn FileSystem>,
    /// SSH 配置（远程时有值；本地树使用 local() 占位配置作为共享缓存 key）
    ssh_config: Option<SshConnectionConfig>,
    /// 是否本地
    is_local: bool,
    /// 是否仍持有共享缓存引用
    cache_ref_held: bool,
}

impl Drop for FileTreeLoader {
    fn drop(&mut self) {
        self.release_shared_cache();
    }
}

impl FileTreeLoader {
    pub fn new(backend: Arc<dyn FileSystem>) -> Self {
        let is_local = backend.is_local();
        let ssh_config = backend.ssh_config().or_else(|| {
            if is_local {
                Some(SshConnectionConfig::local())
            } else {
                None
            }
        });
        // 构造即持有共享缓存引用，Drop 时归还
        if let Some(config) = &ssh_config {
            RemoteFileTreeCache::global().lock().acquire(config);
        }
        Self {
            directory_cache: HashMap::new(),
            loading_paths: HashSet::new(),
            pending_refresh: HashSet::new(),
            backend,
            ssh_config,
            is_local,
            cache_ref_held: true,
        }
    }

    // ── Backend 访问 ──

    pub fn backend(&self) -> &Arc<dyn FileSystem> {
        &self.backend
    }

    pub fn is_local(&self) -> bool {
        self.is_local
    }

    pub fn ssh_config(&self) -> Option<&SshConnectionConfig> {
        self.ssh_config.as_ref()
    }

    // ── 本地缓存查询 ──

    /// 查询本地缓存
    pub fn get_cached(&self, path: &Path) -> Option<&Vec<FileEntry>> {
        self.directory_cache.get(path)
    }

    /// 本地缓存是否包含某路径
    pub fn has_cached(&self, path: &Path) -> bool {
        self.directory_cache.contains_key(path)
    }

    /// 是否正在加载
    pub fn is_loading(&self, path: &Path) -> bool {
        self.loading_paths.contains(path)
    }

    // ── 加载状态管理 ──

    /// 标记开始加载。返回 false 表示已在加载中，应跳过。
    pub fn begin_load(&mut self, path: &Path) -> bool {
        if self.loading_paths.contains(path) {
            return false;
        }
        self.loading_paths.insert(path.to_path_buf());
        true
    }

    /// 完成加载：写入本地缓存，清除 loading 状态。
    pub fn finish_load(&mut self, path: &Path, entries: Vec<FileEntry>) {
        if self.loading_paths.remove(path) {
            self.directory_cache.insert(path.to_path_buf(), entries);
        }
    }

    /// 加载失败：清除 loading 状态。
    pub fn fail_load(&mut self, path: &Path) {
        self.loading_paths.remove(path);
    }

    /// 请求刷新。如果正在加载则标记 pending 并返回 false。
    /// 如果可以立即刷新，则将路径标记为加载中并返回 true。
    ///
    /// 返回 true 后调用方必须在完成时调用 `finish_refresh` / `fail_refresh`
    /// 清除加载标记，否则该路径会卡在加载状态。
    pub fn request_refresh(&mut self, path: &Path) -> bool {
        if self.loading_paths.contains(path) {
            self.pending_refresh.insert(path.to_path_buf());
            false
        } else {
            self.loading_paths.insert(path.to_path_buf());
            true
        }
    }

    /// 检查并清除 pending_refresh 标记
    pub fn take_pending_refresh(&mut self, path: &Path) -> bool {
        self.pending_refresh.remove(path)
    }

    /// 完成刷新：有变化返回 true，调用方应通知 UI 更新。
    pub fn finish_refresh(&mut self, path: &Path, entries: Vec<FileEntry>) -> bool {
        if self.loading_paths.remove(path) && self.directory_cache.get(path) != Some(&entries) {
            self.directory_cache.insert(path.to_path_buf(), entries);
            true
        } else {
            false
        }
    }

    /// 刷新失败：只清除加载标记，保留上一次成功列出的内容。
    ///
    /// 刷新失败通常是瞬时网络/SFTP 错误，并不代表目录已被删除。此前这里会
    /// 顺手删掉缓存，根目录刷新一旦失败，文件树就会立刻变空；而随后 watcher
    /// 可能因为基线未变化不再发事件，空树便永久卡住。真正的删除由父目录的
    /// 刷新结果负责。返回 true 表示仍有一份可展示的旧缓存。
    pub fn fail_refresh(&mut self, path: &Path) -> bool {
        self.loading_paths.remove(path);
        self.directory_cache.contains_key(path)
    }

    /// 无条件更新本地缓存（用于后台静默刷新）。
    /// 返回 true 表示确实发生了变更。
    pub fn update_cached(&mut self, path: &Path, entries: Vec<FileEntry>) -> bool {
        if self.directory_cache.get(path) != Some(&entries) {
            self.directory_cache.insert(path.to_path_buf(), entries);
            true
        } else {
            false
        }
    }

    // ── 缓存清除 ──

    /// 清除以 path 为前缀的所有本地缓存
    pub fn remove_cached_recursive(&mut self, path: &Path) {
        self.directory_cache.retain(|p, _| !p.starts_with(path));
    }

    /// 保留根路径，清除其所有子目录缓存
    pub fn keep_root_clear_descendants(&mut self, root: &Path) {
        self.directory_cache.retain(|p, _| p == root);
    }

    /// 清空所有 pending refresh
    pub fn clear_pending_refresh(&mut self) {
        self.pending_refresh.clear();
    }

    /// 清空指定路径及其子路径的 pending refresh
    pub fn clear_pending_refresh_for(&mut self, path: &Path) {
        self.pending_refresh.retain(|p| !p.starts_with(path));
    }

    // ── 文件系统事件处理 ──

    /// 处理文件系统事件，返回需要刷新的父目录路径（如有）。
    ///
    /// - Remove 事件：清除该路径及子路径缓存，返回父目录
    /// - Create/Modify 事件：返回最近的已缓存祖先路径
    /// - 带 entries 的事件：直接更新缓存，返回 None
    pub fn handle_fs_event(&mut self, root_path: &Path, event: &FsChangeEvent) -> Option<PathBuf> {
        if !event.path.starts_with(root_path) {
            return None;
        }

        // 同步更新共享缓存
        if let Some(config) = &self.ssh_config {
            RemoteFileTreeCache::global()
                .lock()
                .on_fs_event(config, event);
        }

        // Remote watcher 直接提供了新 entries，直接更新本地缓存。
        //
        // 这里**不能**要求该路径此前已缓存或正在加载：初次加载失败时
        // `fail_load` 已经把 root 从 loading_paths 里移除，而 watcher 的首次
        // 轮询仍会带着真实 entries 发来 Modify。旧逻辑会把这份唯一可用的数据
        // 直接丢掉；此后 watcher 基线已经建立，内容不变就再也不会发事件，
        // 文件树于是永久停在「只有根行、没有子项」的空状态。
        if let Some(ref entries) = event.entries {
            if self.directory_cache.get(&event.path) != Some(entries) {
                self.loading_paths.remove(&event.path);
                self.directory_cache
                    .insert(event.path.clone(), entries.clone());
            }
            return None; // 已直接更新，无需额外刷新
        }

        // Remove 事件：清除该路径及子路径
        if event.kind == FsChangeKind::Remove {
            let removed_path = &event.path;
            let is_root = removed_path == root_path;
            if is_root {
                self.keep_root_clear_descendants(removed_path);
                // 共享缓存对任意 Remove 都会连节点本身一起删除；根目录只是
                // 暂时不可达（网络抖动），旧条目仍可展示，也是恢复期间唯一的
                // 内容来源，因此把它写回去，避免切回该树时又变成空目录。
                if let Some(entries) = self.directory_cache.get(removed_path).cloned() {
                    self.set_shared_cache(removed_path, entries);
                }
            } else {
                self.remove_cached_recursive(removed_path);
            }
            self.loading_paths.remove(removed_path);
            // 找到最近的已缓存祖先，返回它以便刷新
            let mut ancestor = removed_path.parent();
            while let Some(p) = ancestor {
                if !p.starts_with(root_path) {
                    break;
                }
                if self.directory_cache.contains_key(p) || self.loading_paths.contains(p) {
                    return Some(p.to_path_buf());
                }
                ancestor = p.parent();
            }
            return Some(root_path.to_path_buf());
        }

        // Create/Modify 事件：找最近的已缓存祖先；找不到时退回根目录。
        // 事件可能指向一个尚未成功列出的深层目录，此时唯一能恢复显示的
        // 做法就是重新读取根目录（刷新是防抖的，不会造成事件风暴）。
        let mut ancestor = Some(event.path.as_path());
        while let Some(p) = ancestor {
            if !p.starts_with(root_path) {
                break;
            }
            if self.directory_cache.contains_key(p) || self.loading_paths.contains(p) {
                return Some(p.to_path_buf());
            }
            ancestor = p.parent();
        }
        Some(root_path.to_path_buf())
    }

    // ── 共享缓存交互 ──

    /// 从共享缓存获取目录条目
    pub fn get_shared_cache(&self, path: &Path) -> Option<Vec<FileEntry>> {
        let config = self.ssh_config.as_ref()?;
        RemoteFileTreeCache::global().lock().get(config, path)
    }

    /// 写入共享缓存
    pub fn set_shared_cache(&self, path: &Path, entries: Vec<FileEntry>) {
        if let Some(config) = &self.ssh_config {
            RemoteFileTreeCache::global()
                .lock()
                .insert(config, path, entries);
        }
    }

    /// 从共享缓存移除目录
    pub fn remove_shared_cache(&self, path: &Path) {
        if let Some(config) = &self.ssh_config {
            RemoteFileTreeCache::global()
                .lock()
                .remove_dir(config, path);
        }
    }

    /// Returns true if the shared directory cache is still fresh (within TTL).
    pub fn is_shared_cache_fresh(&self, path: &Path) -> bool {
        self.ssh_config.as_ref().is_some_and(|config| {
            RemoteFileTreeCache::global()
                .lock()
                .is_dir_fresh(config, path)
        })
    }

    /// 释放共享缓存引用（幂等；正常情况下由 `Drop` 调用）
    fn release_shared_cache(&mut self) {
        if !self.cache_ref_held {
            return;
        }
        self.cache_ref_held = false;
        if let Some(config) = &self.ssh_config {
            RemoteFileTreeCache::global().lock().release(config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_manager::filesystem::FileMetadata;
    use crate::file_manager::types::FileEntryKind;
    use std::sync::Mutex as StdMutex;

    /// 可注入读取结果的 mock 后端；ssh_config 返回唯一 key 以隔离全局共享缓存
    struct MockFs {
        entries: StdMutex<Result<Vec<FileEntry>, String>>,
        config: SshConnectionConfig,
    }

    impl FileSystem for MockFs {
        fn read_dir(&self, _path: &Path) -> Result<Vec<FileEntry>, String> {
            self.entries.lock().unwrap().clone()
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

    fn test_config(tag: &str) -> SshConnectionConfig {
        let mut config = SshConnectionConfig::local();
        config.host = format!("test-host-{tag}");
        config.username = format!("test-user-{tag}");
        config
    }

    fn entries(names: &[&str]) -> Vec<FileEntry> {
        names
            .iter()
            .map(|n| FileEntry {
                name: (*n).to_string(),
                path: PathBuf::from(format!("/root/{n}")),
                kind: FileEntryKind::File,
                size: 0,
            })
            .collect()
    }

    /// 回归守卫（Bug 1）：request_refresh 返回 true 时必须标记 loading，
    /// 否则完成回调里的 is_loading 守卫会丢弃刷新结果，本地 fs 变更完全不刷新
    #[test]
    fn request_refresh_marks_loading_and_finish_writes_cache() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(entries(&["a", "b"]))),
            config: test_config("refresh-finish"),
        });
        let mut loader = FileTreeLoader::new(backend);

        assert!(loader.request_refresh(Path::new("/root")));
        assert!(loader.is_loading(Path::new("/root")));

        // 内容有变化 → 返回 true 且清除 loading
        assert!(loader.finish_refresh(Path::new("/root"), entries(&["a", "b"])));
        assert!(!loader.is_loading(Path::new("/root")));
        assert_eq!(
            loader.get_cached(Path::new("/root")),
            Some(&entries(&["a", "b"]))
        );
    }

    /// 加载中的 request_refresh → pending 标记，加载完成后补一次刷新
    #[test]
    fn request_refresh_during_load_defers_via_pending() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(entries(&["a"]))),
            config: test_config("refresh-pending"),
        });
        let mut loader = FileTreeLoader::new(backend);

        assert!(loader.begin_load(Path::new("/root")));
        assert!(!loader.request_refresh(Path::new("/root")));
        assert!(loader.take_pending_refresh(Path::new("/root")));
        assert!(!loader.take_pending_refresh(Path::new("/root")));
    }

    /// 刷新结果与缓存相同 → finish_refresh 返回 false，但 loading 必须被清除
    #[test]
    fn finish_refresh_with_unchanged_entries_clears_loading() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(entries(&["a"]))),
            config: test_config("refresh-unchanged"),
        });
        let mut loader = FileTreeLoader::new(backend);
        loader.begin_load(Path::new("/root"));
        loader.finish_load(Path::new("/root"), entries(&["a"]));

        assert!(loader.request_refresh(Path::new("/root")));
        assert!(!loader.finish_refresh(Path::new("/root"), entries(&["a"])));
        assert!(!loader.is_loading(Path::new("/root")));
        assert!(loader.has_cached(Path::new("/root")));
    }

    /// 刷新失败只清除 loading，保留旧内容（瞬时网络错误不应清空文件树）
    #[test]
    fn fail_refresh_keeps_last_known_entries() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(entries(&["a"]))),
            config: test_config("refresh-fail"),
        });
        let mut loader = FileTreeLoader::new(backend);
        loader.begin_load(Path::new("/root"));
        loader.finish_load(Path::new("/root"), entries(&["a"]));

        assert!(loader.request_refresh(Path::new("/root")));
        assert!(loader.fail_refresh(Path::new("/root")));
        assert!(!loader.is_loading(Path::new("/root")));
        assert_eq!(
            loader.get_cached(Path::new("/root")),
            Some(&entries(&["a"]))
        );
    }

    /// 回归：初次 root read_dir 失败后，watcher 首轮带的 entries 必须能救回空树。
    /// 旧逻辑只在 cached/loading 时才接收 entries，失败后两者都不成立，
    /// 唯一一份可用目录数据被丢弃，且 watcher 基线建立后不再发事件。
    #[test]
    fn watcher_entries_recover_root_after_initial_load_failure() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Err("connection reset".to_string())),
            config: test_config("watcher-recover"),
        });
        let mut loader = FileTreeLoader::new(backend);

        assert!(loader.begin_load(Path::new("/root")));
        loader.fail_load(Path::new("/root"));
        assert!(!loader.has_cached(Path::new("/root")));
        assert!(!loader.is_loading(Path::new("/root")));

        let event = FsChangeEvent {
            path: PathBuf::from("/root"),
            kind: FsChangeKind::Modify,
            entries: Some(entries(&["fresh"])),
            server: None,
        };
        assert_eq!(loader.handle_fs_event(Path::new("/root"), &event), None);
        assert_eq!(
            loader.get_cached(Path::new("/root")),
            Some(&entries(&["fresh"]))
        );
    }

    /// Create/Modify 在整棵树都没有缓存时也必须返回 root 触发重载，
    /// 不能一路向上走到树外后返回 None。
    #[test]
    fn event_in_uncached_tree_requests_root_reload() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(Vec::new())),
            config: test_config("uncached-recover"),
        });
        let mut loader = FileTreeLoader::new(backend);

        let event = FsChangeEvent {
            path: PathBuf::from("/root/sub/file"),
            kind: FsChangeKind::Create,
            entries: None,
            server: None,
        };
        assert_eq!(
            loader.handle_fs_event(Path::new("/root"), &event),
            Some(PathBuf::from("/root"))
        );
    }

    /// 根目录暂时不可达（watcher 发 Remove）时保留最后一份目录内容，
    /// 否则恢复前文件树会先被清空。
    #[test]
    fn remove_root_keeps_last_known_entries() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(entries(&["a"]))),
            config: test_config("root-remove"),
        });
        let mut loader = FileTreeLoader::new(backend);
        loader.begin_load(Path::new("/root"));
        loader.finish_load(Path::new("/root"), entries(&["a"]));

        let event = FsChangeEvent {
            path: PathBuf::from("/root"),
            kind: FsChangeKind::Remove,
            entries: None,
            server: None,
        };
        assert_eq!(
            loader.handle_fs_event(Path::new("/root"), &event),
            Some(PathBuf::from("/root"))
        );
        assert!(loader.has_cached(Path::new("/root")));
    }

    /// watcher 带 entries 的事件直写缓存并清除 loading，
    /// 后续完成的 in-flight 读取因 is_loading 为 false 被丢弃（更新数据获胜）
    #[test]
    fn watcher_direct_update_wins_over_in_flight_load() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(entries(&["stale"]))),
            config: test_config("watcher-wins"),
        });
        let mut loader = FileTreeLoader::new(backend);

        assert!(loader.begin_load(Path::new("/root")));
        // watcher 事件带 entries：loader 直写缓存
        let event = FsChangeEvent {
            path: PathBuf::from("/root"),
            kind: FsChangeKind::Modify,
            entries: Some(entries(&["fresh"])),
            server: None,
        };
        loader.handle_fs_event(Path::new("/root"), &event);

        assert!(!loader.is_loading(Path::new("/root")));
        assert_eq!(
            loader.get_cached(Path::new("/root")),
            Some(&entries(&["fresh"]))
        );
    }

    /// Remove 事件：删除路径及子路径缓存，返回最近已缓存祖先供刷新
    #[test]
    fn remove_event_clears_subtree_and_finds_cached_ancestor() {
        let backend = Arc::new(MockFs {
            entries: StdMutex::new(Ok(entries(&["a"]))),
            config: test_config("remove"),
        });
        let mut loader = FileTreeLoader::new(backend);
        loader.begin_load(Path::new("/root"));
        loader.finish_load(Path::new("/root"), entries(&["sub"]));
        loader.begin_load(Path::new("/root/sub"));
        loader.finish_load(Path::new("/root/sub"), entries(&["x"]));

        let event = FsChangeEvent {
            path: PathBuf::from("/root/sub"),
            kind: FsChangeKind::Remove,
            entries: None,
            server: None,
        };
        let ancestor = loader.handle_fs_event(Path::new("/root"), &event);

        assert_eq!(ancestor, Some(PathBuf::from("/root")));
        assert!(!loader.has_cached(Path::new("/root/sub")));
        assert!(loader.has_cached(Path::new("/root")));
    }
}
