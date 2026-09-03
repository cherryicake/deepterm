use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::file_manager::types::{FileEntry, FileEntryKind};
use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::{self, RemoteFileKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMetadata {
    pub is_dir: bool,
    pub size: u64,
    pub modified_at: Option<SystemTime>,
}

pub trait FileSystem: Send + Sync + 'static {
    fn read_dir(&self, path: &Path) -> Result<Vec<FileEntry>, String>;
    fn metadata(&self, path: &Path) -> Result<FileMetadata, String>;
    fn read_file_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, String>;
    fn home_dir(&self) -> Option<PathBuf>;
    fn ssh_config(&self) -> Option<SshConnectionConfig>;
    fn is_local(&self) -> bool;

    fn matches(&self, is_local: bool, ssh_config: Option<&SshConnectionConfig>) -> bool {
        self.is_local() == is_local && self.ssh_config().as_ref() == ssh_config
    }

    /// 预览读取（最多 `limit` 字节），用于文件树悬停预取等场景。
    /// 默认实现先经 `metadata` 预检，超限直接跳过；小文件才读并截断。
    /// 注意（中文）：默认实现依赖“stat 与读之间文件不变”，存在理论 TOCTOU，
    /// 因此所有生产实现（本地/远端）均已覆盖为 `take(limit)` 有界读。
    /// 新增实现者必须同样覆盖本方法并保证内存占用不超过 `limit`，
    /// 不得直接复用默认实现读取不可信来源。
    fn read_preview_bytes(&self, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
        let meta = self.metadata(path)?;
        if meta.is_dir || meta.size > limit as u64 {
            return Ok(None);
        }
        Ok(self.read_file_bytes(path)?.map(|mut bytes| {
            bytes.truncate(limit);
            bytes
        }))
    }
}

pub struct LocalFileSystem;

impl FileSystem for LocalFileSystem {
    fn read_dir(&self, path: &Path) -> Result<Vec<FileEntry>, String> {
        let entries = std::fs::read_dir(path).map_err(|e| e.to_string())?;
        let mut result: Vec<FileEntry> = entries
            .filter_map(|e| e.ok().and_then(FileEntry::from_dir_entry))
            .collect();
        result.sort_by_key(|a| a.sort_key());
        Ok(result)
    }

    fn metadata(&self, path: &Path) -> Result<FileMetadata, String> {
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        Ok(FileMetadata {
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified_at: meta.modified().ok(),
        })
    }

    fn read_file_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// 本地高效预览：先 `metadata` 预检，超限直接跳过；
    /// 小文件用 `File::take(limit)` 仅读前 `limit` 字节，避免全量读。
    fn read_preview_bytes(&self, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
        use std::io::Read;
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.len() > limit as u64 {
            return Ok(None);
        }
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        file.take(limit as u64)
            .read_to_end(&mut buf)
            .map_err(|e| e.to_string())?;
        Ok((!buf.is_empty()).then_some(buf))
    }

    fn home_dir(&self) -> Option<PathBuf> {
        dirs::home_dir()
    }

    fn ssh_config(&self) -> Option<SshConnectionConfig> {
        None
    }

    fn is_local(&self) -> bool {
        true
    }
}

pub struct RemoteFileSystem {
    config: SshConnectionConfig,
    home: OnceLock<Option<PathBuf>>,
}

impl RemoteFileSystem {
    pub fn new(config: SshConnectionConfig) -> Self {
        Self {
            config,
            home: OnceLock::new(),
        }
    }
}

impl FileSystem for RemoteFileSystem {
    fn read_dir(&self, path: &Path) -> Result<Vec<FileEntry>, String> {
        let path_str = path.to_string_lossy();
        let entries =
            sftp_fs::with_cached_session(&self.config, |session| session.read_dir(&path_str))
                .map_err(|e| e.to_string())?;

        let mut result: Vec<FileEntry> = entries
            .into_iter()
            .map(|e| FileEntry {
                name: e.name,
                path: PathBuf::from(e.path),
                kind: match e.kind {
                    RemoteFileKind::Directory => FileEntryKind::Directory,
                    RemoteFileKind::File => FileEntryKind::File,
                    RemoteFileKind::Symlink => FileEntryKind::Symlink,
                    RemoteFileKind::Other => FileEntryKind::Other,
                },
                size: e.size,
            })
            .collect();
        result.sort_by_key(|a| a.sort_key());
        Ok(result)
    }

    fn metadata(&self, path: &Path) -> Result<FileMetadata, String> {
        let path_str = path.to_string_lossy();
        let meta =
            sftp_fs::with_cached_session(&self.config, |session| session.metadata(&path_str))
                .map_err(|e| e.to_string())?;
        Ok(FileMetadata {
            is_dir: matches!(meta.kind, RemoteFileKind::Directory),
            size: meta.size,
            modified_at: meta.modified_at,
        })
    }

    fn read_file_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, String> {
        let path_str = path.to_string_lossy();
        sftp_fs::read_file_bytes(&self.config, &path_str).map_err(|e| e.to_string())
    }

    /// 远端高效预览：底层走 `take(limit)` 有界读，内存占用恒不超过 `limit`。
    /// 关键正确性（中文）：不得用“先 `metadata` 预检 + 再全量读 + 事后截断”，
    /// 远端文件可能在两次 RPC 之间增大（持续写入的日志），全量读仍会 OOM。
    /// `metadata` 仅用于快速跳过非文件，安全边界由 `take` 有界读保证。
    fn read_preview_bytes(&self, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
        let path_str = path.to_string_lossy();
        // 快速跳过非文件，避免一次多余的 open RPC。
        // 注意：即使 stat 与 open 之间文件增大，下方的有界读依然保证内存安全。
        let meta =
            sftp_fs::with_cached_session(&self.config, |session| session.metadata(&path_str))
                .map_err(|e| e.to_string())?;
        if !matches!(meta.kind, sftp_fs::RemoteFileKind::File) {
            return Ok(None);
        }
        sftp_fs::read_preview_bytes(&self.config, &path_str, limit).map_err(|e| e.to_string())
    }

    fn home_dir(&self) -> Option<PathBuf> {
        self.home
            .get_or_init(|| {
                sftp_fs::with_cached_session(&self.config, |session| {
                    session.canonicalize("~").or_else(|_| {
                        session
                            .canonicalize("/home")
                            .or_else(|_| Ok::<_, anyhow::Error>("/".to_string()))
                    })
                })
                .ok()
                .map(PathBuf::from)
            })
            .clone()
    }

    fn ssh_config(&self) -> Option<SshConnectionConfig> {
        Some(self.config.clone())
    }

    fn is_local(&self) -> bool {
        false
    }
}
