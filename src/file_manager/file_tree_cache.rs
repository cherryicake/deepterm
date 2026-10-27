use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
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
/// 单台服务器**内存侧**文件内容条数上限（与磁盘持久化口径一致）。
///
/// `MAX_PERSISTED_FILES_PER_HOST` 只约束落盘/读盘的那两条路径；内存
/// `HashMap` 此前无任何上限——编辑器每打开一个文件就插入全文（上限
/// 20MB/个）、hover 每划过一个文件插入最多 128KB，且关闭标签不回收，
/// 长会话下 RSS 单调增长。这里补上内存侧同口径上限 + FIFO 淘汰。
const MAX_CACHED_FILES_PER_SERVER: usize = MAX_PERSISTED_FILES_PER_HOST;
/// 单台服务器内存侧文件内容的**字节**上限。
///
/// 条数上限不设大小约束：用户显式打开的文件可达编辑器上限 20MB/个
/// （大小守卫只作用于 hover 这类 transient 副本），200 个大文件就是 GB 级。
/// 这里按字节再收一次，超限时同样走 FIFO 淘汰。
const MAX_CACHED_BYTES_PER_SERVER: usize = 32 * 1024 * 1024;

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
    loaded_at: Instant,
}

/// 远程文件内容缓存条目
struct FileContentEntry {
    content: String,
    /// 文件磁盘编码（缓存命中时无需重新检测）。
    encoding: FileEncoding,
    loaded_at: Instant,
    /// 是否随 sftp_cache.json 落盘。
    ///
    /// 仅"用户显式打开过的文件"才允许持久化：鼠标悬停预取 / 目录首批文件
    /// 的内容也走本缓存（进程内秒开），但 hover 一划即把服务器上 `.env` /
    /// 私钥等明文写进磁盘缓存属隐私越界，且磁盘无过期机制。
    persist: bool,
}

/// 单台远程服务器的文件内容缓存
struct ServerFileCache {
    /// path -> FileContentEntry，按文件路径索引
    files: HashMap<PathBuf, FileContentEntry>,
    /// 插入序（FIFO 淘汰用）。与 `files` 同步维护：删除必须走
    /// [`Self::forget`]，否则会堆积已移除路径的陈旧键，把淘汰队列堵死。
    order: VecDeque<PathBuf>,
    /// 当前驻留字节数（`files` 各条目 `content.len()` 之和），随增删同步。
    /// 条数上限之外的第二道闸：用户显式打开的文件不受大小守卫约束
    /// （见 [`RemoteFileTreeCache::insert_file_content`），200 个大文件
    /// 足以把 RSS 顶到 GB 级，这里按字节再收一次。
    bytes: usize,
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
                // 走有界插入：磁盘恢复的条目也记入 FIFO 序，参与内存侧淘汰
                // （否则它们永远不进 order，超限时淘汰不动它们）。
                fc.insert_bounded(
                    PathBuf::from(p_str),
                    FileContentEntry {
                        content: persisted_file.content,
                        encoding: persisted_file.encoding,
                        loaded_at: Instant::now(),
                        // 磁盘恢复的条目本来就是持久化的，保持原语义
                        persist: true,
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
                    // 仅持久化用户显式打开过的文件（persist=false 为 hover/目录预取）
                    .filter(|(_, entry)| entry.persist)
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
        // 与 watcher 的 watch 状态 key 同源，两处必须永远一致：
        // 共享缓存与轮询器都以服务器为隔离单元，格式漂移会导致错位。
        crate::file_manager::watcher::remote_server_key(config)
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

    /// 缓存远程文件内容（默认持久化；hover / 目录预取请传 `persist: false`）
    ///
    /// 有界保护：
    /// - **大小守卫**（仅 `persist: false`）：transient 副本（hover / 目录
    ///   预取）超过 `MAX_CACHED_FILE_SIZE` 不入缓存——它本就是「省一次 SFTP
    ///   往返」的顺手优化，为大文件永久留一份副本只会把 RSS 顶高。用户显式
    ///   打开的文件不走守卫：编辑器自己允许开到 20MB，这里卡一道会让
    ///   512KB~20MB 的文件每次重开都走完整异步加载（加载态 + 一次 SFTP
    ///   往返），把「秒开」变成可感知的等待；而条数上限已把总量约束住。
    /// - **条数上限**：`MAX_CACHED_FILES_PER_SERVER`，FIFO 淘汰（非持久化
    ///   的 hover 副本优先），见 [`ServerFileCache::insert_bounded`]。
    pub fn insert_file_content(
        &mut self,
        config: &SshConnectionConfig,
        path: &Path,
        content: String,
        encoding: FileEncoding,
        persist: bool,
    ) {
        let key = Self::server_key(config);
        if !persist && content.len() > MAX_CACHED_FILE_SIZE {
            log::debug!(
                "FileTreeCache skip oversized file content ({} bytes): {}:{}",
                content.len(),
                key,
                path.display()
            );
            return;
        }
        let file_cache = self
            .file_caches
            .entry(key.clone())
            .or_insert_with(ServerFileCache::new);
        log::debug!(
            "FileTreeCache insert file content (persist={}): {}:{}",
            persist,
            key,
            path.display()
        );
        file_cache.insert_bounded(
            path.to_path_buf(),
            FileContentEntry {
                content,
                encoding,
                loaded_at: Instant::now(),
                persist,
            },
        );
        if persist {
            self.schedule_save();
        }
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
        if let Some(tree) = self.servers.get(&key)
            && let Some(node) = tree.nodes.get(path)
        {
            return node.loaded_at.elapsed() < DIR_TTL;
        }
        false
    }

    /// 判断文件内容缓存是否新鲜
    pub fn is_file_fresh(&self, config: &SshConnectionConfig, path: &Path) -> bool {
        let key = Self::server_key(config);
        if let Some(fc) = self.file_caches.get(&key)
            && let Some(entry) = fc.files.get(path)
        {
            return entry.loaded_at.elapsed() < FILE_TTL;
        }
        false
    }

    /// 移除单个文件的内容缓存
    pub fn remove_file(&mut self, config: &SshConnectionConfig, path: &Path) {
        let key = Self::server_key(config);
        if let Some(file_cache) = self.file_caches.get_mut(&key)
            && file_cache.forget(path)
        {
            log::debug!("FileTreeCache remove file: {}:{}", key, path.display());
            self.schedule_save();
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
                    // watcher 送来的就是刚刚列出的真实目录内容。即使这个节点
                    // 此前不在共享缓存里（例如首次加载失败），也应该写入：
                    // 否则下一次挂载同一棵文件树时仍只能重新发起一次可能失败
                    // 的 SFTP 读取。
                    if let Some(ref entries) = event.entries {
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
        // 文件内容缓存失效，避免重新打开文件时看到旧内容
        if matches!(event.kind, FsChangeKind::Remove | FsChangeKind::Modify)
            && let Some(file_cache) = self.file_caches.get_mut(&key)
        {
            file_cache.forget(&event.path);
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
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    /// 有界插入：新键记入 FIFO 序；超出 `MAX_CACHED_FILES_PER_SERVER`（条数）
    /// 或 `MAX_CACHED_BYTES_PER_SERVER`（字节）时，**先淘汰最旧的非持久化条目**
    /// （hover / 目录预取产生的 transient 副本），没有非持久化条目才淘汰最旧
    /// 的持久化条目（用户显式打开过的文件）。
    ///
    /// 同键覆盖视作「刚用过」：移到队尾，而不是保留原来的位置。
    ///
    /// 刚插入的条目不参与淘汰：缓存已满且全是持久化条目时，新来的
    /// `persist: false` 副本是唯一的非持久化条目，会被自己这一步选走并删掉，
    /// 于是 hover/目录预取静默失效（插进去就没了）。超出预算时改为淘汰最旧
    /// 的持久化条目——那也只是一个「秒开」副本，服务器仍是内容权威。
    ///
    /// 淘汰只损失「秒开」优化：被淘汰的文件下次打开从服务器重新拉取，
    /// 不影响正确性（内容的唯一权威在服务器与编辑器实体）。
    fn insert_bounded(&mut self, path: PathBuf, entry: FileContentEntry) {
        self.bytes += entry.content.len();
        if let Some(previous) = self.files.insert(path.clone(), entry) {
            // 同键覆盖：旧条目的字节先扣掉，否则 `bytes` 会只增不减地虚高。
            self.bytes -= previous.content.len();
            // 覆盖也要刷新 FIFO 位置：刚重新加载的条目最可能马上再被用到，
            // 停在队首会让它成为下一个淘汰目标（缓存刚写好就丢）。
            self.order.retain(|p| p != &path);
            self.order.push_back(path.clone());
        } else {
            self.order.push_back(path.clone());
        }
        // 超限才淘汰。单条就可能顶满字节预算：此时新条目自身也必须可被淘汰
        // （缓存装不下就是装不下），否则循环永远收敛不了。
        let single_too_big = self.bytes > MAX_CACHED_BYTES_PER_SERVER;
        let skip = (!single_too_big).then_some(path.as_path());
        while self.files.len() > MAX_CACHED_FILES_PER_SERVER
            || self.bytes > MAX_CACHED_BYTES_PER_SERVER
        {
            let Some(victim) = self.take_victim(skip) else {
                break;
            };
            if let Some(removed) = self.files.remove(&victim) {
                self.bytes -= removed.content.len();
            }
        }
    }

    /// 取下一条淘汰候选：最旧的非持久化条目优先，否则最旧条目。
    ///
    /// `skip` 是本次插入的键，永不被选中（见 [`Self::insert_bounded`]）。
    ///
    /// `order` 里可能含有已被 `forget` 之外的路径移除的键（防御性）：
    /// 这些键不在 `files` 里，`position` 会跳过，`pop_front` 会消耗掉，
    /// 循环必然收敛。
    fn take_victim(&mut self, skip: Option<&Path>) -> Option<PathBuf> {
        let is_candidate = |p: &PathBuf| {
            Some(p.as_path()) != skip && self.files.get(p).is_some_and(|e| !e.persist)
        };
        if let Some(pos) = self.order.iter().position(is_candidate) {
            return self.order.remove(pos);
        }
        // 全是持久化条目：从队首跳过 `skip` 与已失效的陈旧键。
        let pos = self
            .order
            .iter()
            .position(|p| Some(p.as_path()) != skip && self.files.contains_key(p))?;
        self.order.remove(pos)
    }

    /// 删除条目并同步 FIFO 序（所有移除出口都走这里）。
    fn forget(&mut self, path: &Path) -> bool {
        match self.files.remove(path) {
            Some(removed) => {
                self.bytes -= removed.content.len();
                self.order.retain(|p| p != path);
                true
            }
            None => false,
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

    fn file_entry(content: &str, persist: bool) -> FileContentEntry {
        FileContentEntry {
            content: content.to_string(),
            encoding: FileEncoding::Utf8,
            loaded_at: Instant::now(),
            persist,
        }
    }

    /// 同键覆盖要刷新 FIFO 位置：刚重新加载的条目最可能马上再被用到，
    /// 停在队首会让它成为下一个淘汰目标（缓存刚写好就丢）。
    #[test]
    fn overwriting_a_key_moves_it_to_the_back() {
        let mut cache = ServerFileCache::new();
        cache.insert_bounded(PathBuf::from("/a"), file_entry("a", true));
        cache.insert_bounded(PathBuf::from("/b"), file_entry("b", true));
        // 重新加载 /a：字节数按新旧差值更新，顺序移到队尾。
        cache.insert_bounded(PathBuf::from("/a"), file_entry("aa", true));

        let order: Vec<&Path> = cache.order.iter().map(PathBuf::as_path).collect();
        assert_eq!(order, vec![Path::new("/b"), Path::new("/a")]);
        assert_eq!(cache.bytes, "aa".len() + "b".len());
    }
}
