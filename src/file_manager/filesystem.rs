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
    fn home_dir(&self) -> Option<PathBuf>;
    fn ssh_config(&self) -> Option<SshConnectionConfig>;
    fn is_local(&self) -> bool;

    fn matches(&self, is_local: bool, ssh_config: Option<&SshConnectionConfig>) -> bool {
        self.is_local() == is_local && self.ssh_config().as_ref() == ssh_config
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
