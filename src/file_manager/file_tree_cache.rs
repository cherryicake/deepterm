use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::editor::encoding::FileEncoding;
use crate::file_manager::types::FileEntry;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind};
use crate::ssh::SshConnectionConfig;
use serde::{Deserialize, Serialize};

/// Disk persistence types (Instant is reset to now on load).
#[derive(Serialize, Deserialize)]
struct PersistedCache {
    version: u32,
    servers: HashMap<String, PersistedServer>,
    file_caches: HashMap<String, HashMap<String, PersistedFileContent>>,
}

#[derive(Serialize, Deserialize)]
struct PersistedServer {
    nodes: HashMap<String, Vec<FileEntry>>,
    expanded: HashSet<String>,
}

#[derive(Serialize, Deserialize)]
struct PersistedFileContent {
    content: String,
    encoding: FileEncoding,
}

const PERSISTED_VERSION: u32 = 1;
const MAX_PERSISTED_FILES_PER_HOST: usize = 200;
const MAX_PERSISTED_DIRS_PER_HOST: usize = 500;
const MAX_CACHED_FILE_SIZE: usize = 512 * 1024;

fn cache_file_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("deepterm/sftp_cache.json")
}

/// 引用归零后的“保温”时长：释放后保留一段时间，便于切回时复用缓存
const ORPHAN_TTL: Duration = Duration::from_secs(10 * 60);
/// 最多保留的无引用服务器数量（超出时按最旧优先逐出）
const MAX_ORPHAN_SERVERS: usize = 3;

/// P1 优化：目录与文件缓存的 TTL，用于减少不必要的后台刷新
/// 目录 10s 内视为新鲜，命中后不触发后台 readdir
const DIR_TTL: Duration = Duration::from_secs(10);
/// 文件内容 30s 内新鲜，超时后仍秒开但后台会重新校验
const FILE_TTL: Duration = Duration::from_secs(30);

/// 单个目录的缓存条目
struct DirNode {
    // 关键性能：用 `Arc<[]>` 存储目录项，`get_shared()` 可零拷贝返回，
    // 避免千级文件目录每次读取即全量 `clone()`。写时仍为独占，无额外成本。
    entries: std::sync::Arc<[FileEntry]>,
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

    /// 获取全局单例（首次初始化时尝试从磁盘恢复）
    pub fn global() -> &'static Mutex<RemoteFileTreeCache> {
        GLOBAL_CACHE.get_or_init(|| {
            let mut cache = RemoteFileTreeCache::new();
            cache.load_from_disk();
            Mutex::new(cache)
        })
    }

    /// Loads persisted cache from disk; failures are ignored.
    fn load_from_disk(&mut self) {
        let path = cache_file_path();
        let Ok(data) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(persisted): Result<PersistedCache, _> = serde_json::from_str(&data) else {
            return;
        };
        if persisted.version != PERSISTED_VERSION {
            return;
        }
        for (key, persisted_server) in persisted.servers {
            let mut tree = ServerTree::new();
            for (p_str, entries) in persisted_server
                .nodes
                .into_iter()
                .take(MAX_PERSISTED_DIRS_PER_HOST)
            {
                tree.nodes.insert(
                    PathBuf::from(p_str),
                    DirNode {
                        entries: std::sync::Arc::from(entries),
                        loaded_at: Instant::now(),
                    },
                );
            }
            for p_str in persisted_server.expanded {
                tree.expanded.insert(PathBuf::from(p_str));
            }
            tree.orphaned_since = Some(Instant::now());
            self.servers.insert(key, tree);
        }
        for (key, files) in persisted.file_caches {
            let mut fc = ServerFileCache::new();
            for (p_str, persisted_file) in files.into_iter().take(MAX_PERSISTED_FILES_PER_HOST) {
                if persisted_file.content.len() > MAX_CACHED_FILE_SIZE {
                    continue;
                }
                fc.files.insert(
                    PathBuf::from(p_str),
                    FileContentEntry {
                        content: persisted_file.content,
                        encoding: persisted_file.encoding,
                        loaded_at: Instant::now(),
                    },
                );
            }
            self.file_caches.insert(key, fc);
        }
        log::debug!(
            "FileTreeCache loaded from disk: {} servers",
            self.servers.len()
        );
    }

    fn to_persisted(&self) -> PersistedCache {
        let servers = self
            .servers
            .iter()
            .map(|(key, tree)| {
                let nodes = tree
                    .nodes
                    .iter()
                    .take(MAX_PERSISTED_DIRS_PER_HOST)
                    .map(|(path, node)| (path.to_string_lossy().to_string(), node.entries.to_vec()))
                    .collect();
                let expanded = tree
                    .expanded
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                (key.clone(), PersistedServer { nodes, expanded })
            })
            .collect();
        let file_caches = self
            .file_caches
            .iter()
            .filter_map(|(key, fc)| {
                let files = fc
                    .files
                    .iter()
                    .take(MAX_PERSISTED_FILES_PER_HOST)
                    .filter(|(_, entry)| entry.content.len() <= MAX_CACHED_FILE_SIZE)
                    .map(|(path, entry)| {
                        (
                            path.to_string_lossy().to_string(),
                            PersistedFileContent {
                                content: entry.content.clone(),
                                encoding: entry.encoding,
                            },
                        )
                    })
                    .collect::<HashMap<_, _>>();
                if files.is_empty() {
                    None
                } else {
                    Some((key.clone(), files))
                }
            })
            .collect();
        PersistedCache {
            version: PERSISTED_VERSION,
            servers,
            file_caches,
        }
    }

    /// P2: 异步落盘（后台线程，不阻塞 UI）
    /// 关键性能修复（中文）：
    ///此前每次 `insert/save_expanded/on_fs_event` 都在 UI 线程全量 `clone()` 目录树+文件内容
    /// 并 `thread::spawn` 落盘，浏览目录即 `O(全缓存)` 拷贝 + 线程爆炸。
    /// 现改为 500ms 防抖合并 + 克隆移入后台线程：
    /// UI 线程仅更新内存并记录时间戳，后台线程持全局锁做 `to_persisted()` 克隆与序列化，
    /// 高频浏览时多次写合并为一次落盘。
    fn schedule_save(&self) {
        const DEBOUNCE_MS: u64 = 500;
        static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);
        static PENDING: AtomicU64 = AtomicU64::new(0);
        fn wall_ms(fallback: u64) -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(fallback)
        }

        LAST_REQ_MS.store(wall_ms(0), Ordering::Relaxed);
        // 已有待执行的防抖任务则直接返回，由它合并本次写。
        if PENDING.swap(1, Ordering::AcqRel) == 1 {
            return;
        }
        std::thread::Builder::new()
            .name("sftp-cache-save".into())
            .spawn(move || {
                loop {
                    let last = LAST_REQ_MS.load(Ordering::Relaxed);
                    let elapsed = wall_ms(last).saturating_sub(last);
                    if elapsed < DEBOUNCE_MS {
                        std::thread::sleep(Duration::from_millis(DEBOUNCE_MS - elapsed));
                        continue;
                    }
                    // 防抖窗口已过：后台持锁克隆并落盘（UI 线程早已释放锁）。
                    // 关键正确性：必须用阻塞锁，后台线程可等待，不影响 UI。
                    // 若用 try_lock，恰逢 UI 持锁时将静默丢一次持久化，
                    // 其后无新写则脏缓存永久不落盘，退出即丢浏览记录。
                    let persisted = Self::global().lock().to_persisted();
                    let path = cache_file_path();
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(json) = serde_json::to_string(&persisted) {
                        // 缓存里含远端文件的实际内容（可能是 .env、凭据类文件），
                        // 必须 0600，与 ssh.json / keychain.json 的口径一致
                        let _ = crate::ssh::keychain::write_secret_file(&path, json.as_bytes());
                    }
                    PENDING.store(0, Ordering::Release);
                    // 竞态兜底：保存期间若有新写到达（LAST_REQ 更新），再跑一轮。
                    let last_after = LAST_REQ_MS.load(Ordering::Relaxed);
                    if wall_ms(last_after).saturating_sub(last_after) < DEBOUNCE_MS
                        && PENDING.swap(1, Ordering::AcqRel) == 0
                    {
                        continue;
                    }
                    break;
                }
            })
            .ok();
    }

    /// 生成服务器唯一标识（基于用户名、主机、端口）
    fn server_key(config: &SshConnectionConfig) -> String {
        format!("{}@{}:{}", config.username, config.host, config.port)
    }

    /// 查询目录缓存，返回该目录的子项列表
    /// 兼容旧接口（返回 `Vec` 拷贝）；高频路径请用 [`Self::get_shared`] 零拷贝版本。
    pub fn get(&self, config: &SshConnectionConfig, path: &Path) -> Option<Vec<FileEntry>> {
        self.get_shared(config, path).map(|arc| arc.to_vec())
    }

    /// 查询目录缓存的零拷贝版本，返回 `Arc<[FileEntry]>`。
    /// 千级文件目录下避免每次读取即全量 `clone()`，调用方仅读不写时优先使用。
    pub fn get_shared(
        &self,
        config: &SshConnectionConfig,
        path: &Path,
    ) -> Option<std::sync::Arc<[FileEntry]>> {
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
                entries: std::sync::Arc::from(entries),
                loaded_at: Instant::now(),
            },
        );
        self.schedule_save();
    }

    /// 移除单个目录及其所有子目录缓存
    pub fn remove_dir(&mut self, config: &SshConnectionConfig, path: &Path) {
        if let Some(tree) = self.servers.get_mut(&Self::server_key(config)) {
            tree.nodes.retain(|p, _| !p.starts_with(path));
        }
        self.schedule_save();
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
        self.schedule_save();
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
        self.schedule_save();
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

    /// 判断目录缓存是否新鲜（在 TTL 内）
    pub fn is_dir_fresh(&self, config: &SshConnectionConfig, path: &Path) -> bool {
        let key = Self::server_key(config);
        if let Some(tree) = self.servers.get(&key) {
            if let Some(node) = tree.nodes.get(path) {
                return node.loaded_at.elapsed() < DIR_TTL;
            }
        }
        false
    }

    /// 判断文件内容缓存是否新鲜
    pub fn is_file_fresh(&self, config: &SshConnectionConfig, path: &Path) -> bool {
        let key = Self::server_key(config);
        if let Some(fc) = self.file_caches.get(&key) {
            if let Some(entry) = fc.files.get(path) {
                return entry.loaded_at.elapsed() < FILE_TTL;
            }
        }
        false
    }

    /// 移除单个文件的内容缓存
    pub fn remove_file(&mut self, config: &SshConnectionConfig, path: &Path) {
        let key = Self::server_key(config);
        if let Some(file_cache) = self.file_caches.get_mut(&key) {
            if file_cache.files.remove(path).is_some() {
                log::debug!("FileTreeCache remove file: {}{}", key, path.display());
                self.schedule_save();
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
                                    entries: std::sync::Arc::from(entries.clone()),
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
        self.schedule_save();
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
