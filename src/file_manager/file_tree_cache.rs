use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::editor::encoding::FileEncoding;
use crate::file_manager::types::FileEntry;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind};
use crate::ssh::SshConnectionConfig;

/// 引用归零后的“保温”时长：释放后保留一段时间，便于切回时复用缓存
const ORPHAN_TTL: Duration = Duration::from_secs(10 * 60);
/// 最多保留的无引用服务器数量（超出时按最旧优先逐出）
const MAX_ORPHAN_SERVERS: usize = 3;

/// 单个目录的缓存条目
struct DirNode {
    entries: Vec<FileEntry>,
    #[allow(dead_code)]
    loaded_at: Instant,
}

/// 远程文件内容缓存条目
struct FileContentEntry {
    content: String,
    /// 文件磁盘编码（缓存命中时无需重新检测）。
    encoding: FileEncoding,
    loaded_at: Instant,
}

/// 单台远程服务器的文件内容缓存
struct ServerFileCache {
    /// path -> FileContentEntry，按文件路径索引
    files: HashMap<PathBuf, FileContentEntry>,
}

/// 单台远程服务器的目录树（文件系统已浏览子集）
struct ServerTree {
    /// path -> DirNode，扁平映射，隐式形成树结构
    nodes: HashMap<PathBuf, DirNode>,
    /// 保持展开状态的文件夹路径
    expanded: HashSet<PathBuf>,
    /// 正在使用这棵树的终端数量（引用计数）
    ref_count: usize,
    /// 引用归零的时间；`None` 表示仍有活跃引用。
    /// 归零后不立即清除，保温 `ORPHAN_TTL` 后由 sweep 逐出。
    orphaned_since: Option<Instant>,
}

/// 全局远程目录树缓存。
///
/// 每台远程服务器维护一棵目录树，保存该服务器上所有已浏览过的目录结构。
/// 当没有终端引用某台服务器时，整棵树从内存中清除。
pub struct RemoteFileTreeCache {
    servers: HashMap<String, ServerTree>,
    /// 远程文件内容缓存：server_key -> ServerFileCache
    file_caches: HashMap<String, ServerFileCache>,
}

static GLOBAL_CACHE: OnceLock<Mutex<RemoteFileTreeCache>> = OnceLock::new();

impl RemoteFileTreeCache {
    fn new() -> Self {
        Self {
            servers: HashMap::new(),
            file_caches: HashMap::new(),
        }
    }

    /// 获取全局单例
    pub fn global() -> &'static Mutex<RemoteFileTreeCache> {
        GLOBAL_CACHE.get_or_init(|| Mutex::new(RemoteFileTreeCache::new()))
    }

    /// 生成服务器唯一标识（基于用户名、主机、端口）
    fn server_key(config: &SshConnectionConfig) -> String {
        format!("{}@{}:{}", config.username, config.host, config.port)
    }

    /// 查询目录缓存，返回该目录的子项列表
    pub fn get(&self, config: &SshConnectionConfig, path: &Path) -> Option<Vec<FileEntry>> {
        let key = Self::server_key(config);
        let tree = self.servers.get(&key)?;
        let node = tree.nodes.get(path)?;
        Some(node.entries.clone())
    }

    /// 写入目录缓存
    pub fn insert(&mut self, config: &SshConnectionConfig, path: &Path, entries: Vec<FileEntry>) {
        let key = Self::server_key(config);
        let tree = self.servers.entry(key).or_insert_with(ServerTree::new);
        tree.nodes.insert(
            path.to_path_buf(),
            DirNode {
                entries,
                loaded_at: Instant::now(),
            },
        );
    }

    /// 移除单个目录及其所有子目录缓存
    pub fn remove_dir(&mut self, config: &SshConnectionConfig, path: &Path) {
        if let Some(tree) = self.servers.get_mut(&Self::server_key(config)) {
            tree.nodes.retain(|p, _| !p.starts_with(path));
        }
    }

    /// 终端打开时调用：引用计数 +1。如服务器尚无缓存则创建。
    pub fn acquire(&mut self, config: &SshConnectionConfig) {
        self.sweep_orphans();
        let key = Self::server_key(config);
        let tree = self
            .servers
            .entry(key.clone())
            .or_insert_with(ServerTree::new);
        tree.ref_count += 1;
        tree.orphaned_since = None;
        log::debug!(
            "FileTreeCache acquire {}: ref_count={}",
            key,
            tree.ref_count
        );
    }

    /// 终端关闭时调用：引用计数 -1。归零后不立即清除，进入保温期
    /// （`ORPHAN_TTL` 内切回可直接复用），逾期或超量时由 sweep 逐出。
    pub fn release(&mut self, config: &SshConnectionConfig) {
        let key = Self::server_key(config);
        if let Some(tree) = self.servers.get_mut(&key) {
            tree.ref_count = tree.ref_count.saturating_sub(1);
            log::debug!(
                "FileTreeCache release {}: ref_count={}",
                key,
                tree.ref_count
            );
            if tree.ref_count == 0 {
                tree.orphaned_since = Some(Instant::now());
                log::debug!(
                    "FileTreeCache orphaned {}: kept for reuse, TTL {:?}",
                    key,
                    ORPHAN_TTL
                );
            }
        }
        self.sweep_orphans();
    }

    /// 清理过期的无引用服务器树；无引用树数量超限时按“最旧优先”逐出。
    fn sweep_orphans(&mut self) {
        let now = Instant::now();
        let mut orphans: Vec<(String, Instant)> = self
            .servers
            .iter()
            .filter_map(|(key, tree)| tree.orphaned_since.map(|at| (key.clone(), at)))
            .collect();

        // TTL 过期的直接逐出
        let mut evicted: Vec<String> = orphans
            .iter()
            .filter(|(_, at)| now.duration_since(*at) >= ORPHAN_TTL)
            .map(|(key, _)| key.clone())
            .collect();
        orphans.retain(|(_, at)| now.duration_since(*at) < ORPHAN_TTL);

        // 超出数量上限时，按“最旧优先”逐出
        if orphans.len() > MAX_ORPHAN_SERVERS {
            orphans.sort_by_key(|(_, at)| *at);
            let excess = orphans.len() - MAX_ORPHAN_SERVERS;
            evicted.extend(orphans.into_iter().take(excess).map(|(key, _)| key));
        }

        for key in evicted {
            self.servers.remove(&key);
            self.file_caches.remove(&key);
            log::debug!("FileTreeCache evicted server tree and file cache: {}", key);
        }
    }

    /// 保存展开状态到共享缓存
    pub fn save_expanded(&mut self, config: &SshConnectionConfig, expanded: HashSet<PathBuf>) {
        let key = Self::server_key(config);
        if let Some(tree) = self.servers.get_mut(&key) {
            tree.expanded = expanded;
        }
    }

    /// 从共享缓存加载展开状态
    pub fn load_expanded(&self, config: &SshConnectionConfig) -> HashSet<PathBuf> {
        let key = Self::server_key(config);
        self.servers
            .get(&key)
            .map(|tree| tree.expanded.clone())
            .unwrap_or_default()
    }

    /// 缓存远程文件内容
    pub fn insert_file_content(
        &mut self,
        config: &SshConnectionConfig,
        path: &Path,
        content: String,
        encoding: FileEncoding,
    ) {
        let key = Self::server_key(config);
        let file_cache = self
            .file_caches
            .entry(key.clone())
            .or_insert_with(ServerFileCache::new);
        log::debug!(
            "FileTreeCache insert file content: {}{}",
            key,
            path.display()
        );
        file_cache.files.insert(
            path.to_path_buf(),
            FileContentEntry {
                content,
                encoding,
                loaded_at: Instant::now(),
            },
        );
    }

    /// 获取缓存的远程文件内容及其磁盘编码
    pub fn get_file_content(
        &self,
        config: &SshConnectionConfig,
        path: &Path,
    ) -> Option<(String, FileEncoding)> {
        let key = Self::server_key(config);
        let file_cache = self.file_caches.get(&key)?;
        let entry = file_cache.files.get(path)?;
        log::debug!("FileTreeCache get file content: {}{}", key, path.display());
        Some((entry.content.clone(), entry.encoding))
    }

    /// 移除单个文件的内容缓存
    pub fn remove_file(&mut self, config: &SshConnectionConfig, path: &Path) {
        let key = Self::server_key(config);
        if let Some(file_cache) = self.file_caches.get_mut(&key) {
            if file_cache.files.remove(path).is_some() {
                log::debug!("FileTreeCache remove file: {}{}", key, path.display());
            }
        }
    }

    /// 处理文件系统事件：更新或移除对应的缓存条目
    pub fn on_fs_event(&mut self, config: &SshConnectionConfig, event: &FsChangeEvent) {
        let key = Self::server_key(config);
        if let Some(tree) = self.servers.get_mut(&key) {
            match event.kind {
                FsChangeKind::Remove => {
                    tree.nodes.retain(|p, _| !p.starts_with(&event.path));
                }
                _ => {
                    if let Some(ref entries) = event.entries {
                        if tree.nodes.contains_key(&event.path) {
                            tree.nodes.insert(
                                event.path.clone(),
                                DirNode {
                                    entries: entries.clone(),
                                    loaded_at: Instant::now(),
                                },
                            );
                        }
                    }
                }
            }
        }
        // 文件内容缓存失效，避免重新打开文件时看到旧内容
        if matches!(event.kind, FsChangeKind::Remove | FsChangeKind::Modify) {
            if let Some(file_cache) = self.file_caches.get_mut(&key) {
                file_cache.files.remove(&event.path);
            }
        }
    }
}

impl ServerTree {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            expanded: HashSet::new(),
            ref_count: 0,
            orphaned_since: None,
        }
    }
}

impl ServerFileCache {
    fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(tag: &str) -> SshConnectionConfig {
        let mut config = SshConnectionConfig::local();
        config.host = format!("cache-test-{tag}");
        config.username = format!("cache-user-{tag}");
        config
    }

    /// 引用归零后进入保温期：数据保留，切回时可复用
    #[test]
    fn released_tree_is_retained_for_reuse() {
        let config = test_config("retain");
        let mut cache = RemoteFileTreeCache::global().lock();
        cache.acquire(&config);
        cache.insert(&config, Path::new("/root"), vec![]);
        cache.release(&config);

        // 归零后仍在保温期内，数据可读
        assert!(cache.get(&config, Path::new("/root")).is_some());

        // 再次 acquire 不会重复计数（复用而非重建导致 ref_count 异常）
        cache.acquire(&config);
        cache.release(&config);
    }

    /// 多次 release 不会下溢 panic，也不会影响其他服务器
    #[test]
    fn double_release_does_not_panic_or_underflow() {
        let config = test_config("underflow");
        let mut cache = RemoteFileTreeCache::global().lock();
        cache.release(&config);
        cache.release(&config);
    }

    /// 展开状态在 release 后仍可恢复（保温期内）
    #[test]
    fn load_expanded_survives_release_within_ttl() {
        let config = test_config("expanded");
        let mut cache = RemoteFileTreeCache::global().lock();
        cache.acquire(&config);
        let mut expanded = HashSet::new();
        expanded.insert(PathBuf::from("/root/sub"));
        cache.save_expanded(&config, expanded);
        cache.release(&config);

        let restored = cache.load_expanded(&config);
        assert!(restored.contains(Path::new("/root/sub")));
    }

    /// 超过孤儿数量上限时按最旧优先逐出
    #[test]
    fn sweep_evicts_oldest_orphan_beyond_limit() {
        let mut cache = RemoteFileTreeCache::global().lock();
        // 制造 MAX_ORPHAN_SERVERS + 1 个孤儿（即时逐出，无需等 TTL）
        let configs: Vec<SshConnectionConfig> = (0..=MAX_ORPHAN_SERVERS)
            .map(|i| test_config(&format!("sweep-{i}")))
            .collect();
        for config in &configs {
            cache.acquire(config);
            cache.insert(config, Path::new("/root"), vec![]);
        }
        for config in &configs {
            cache.release(config);
        }

        // 逐出最旧的（sweep-0），其余保留
        assert!(cache.get(&configs[0], Path::new("/root")).is_none());
        for config in &configs[1..] {
            assert!(cache.get(config, Path::new("/root")).is_some());
        }
    }
}
