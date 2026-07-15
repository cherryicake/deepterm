use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::file_manager::types::FileEntry;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind};
use crate::ssh::SshConnectionConfig;

/// 单个目录的缓存条目
struct DirNode {
    entries: Vec<FileEntry>,
    #[allow(dead_code)]
    loaded_at: Instant,
}

/// 远程文件内容缓存条目
struct FileContentEntry {
    content: String,
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
        let key = Self::server_key(config);
        let tree = self
            .servers
            .entry(key.clone())
            .or_insert_with(ServerTree::new);
        tree.ref_count += 1;
        log::debug!(
            "FileTreeCache acquire {}: ref_count={}",
            key,
            tree.ref_count
        );
    }

    /// 终端关闭时调用：引用计数 -1，归零则清理整棵树。
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
                self.servers.remove(&key);
                self.file_caches.remove(&key);
                log::debug!("FileTreeCache evicted server tree and file cache: {}", key);
            }
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
                loaded_at: Instant::now(),
            },
        );
    }

    /// 获取缓存的远程文件内容
    pub fn get_file_content(&self, config: &SshConnectionConfig, path: &Path) -> Option<String> {
        let key = Self::server_key(config);
        let file_cache = self.file_caches.get(&key)?;
        let entry = file_cache.files.get(path)?;
        log::debug!("FileTreeCache get file content: {}{}", key, path.display());
        Some(entry.content.clone())
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
    }
}

impl ServerTree {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            expanded: HashSet::new(),
            ref_count: 0,
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
