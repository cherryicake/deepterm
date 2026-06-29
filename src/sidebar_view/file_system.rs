use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::ssh::sftp_fs::{self, RemoteFileKind};
use crate::ssh::SshConnectionConfig;
use crate::sidebar_view::file_tree::{FileEntry, FileEntryKind};

pub trait FileSystem: Send + Sync + 'static {
    fn read_dir(&self, path: &Path) -> Result<Vec<FileEntry>, String>;
    fn home_dir(&self) -> Option<PathBuf>;
    fn ssh_config(&self) -> Option<SshConnectionConfig>;
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

    fn home_dir(&self) -> Option<PathBuf> {
        dirs::home_dir()
    }

    fn ssh_config(&self) -> Option<SshConnectionConfig> {
        None
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
        let entries = sftp_fs::with_cached_session(&self.config, |session| {
            session.read_dir(&path_str)
        })
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

    fn home_dir(&self) -> Option<PathBuf> {
        self.home
            .get_or_init(|| {
                sftp_fs::with_cached_session(&self.config, |session| {
                    session.canonicalize("~").or_else(|_| {
                        session.canonicalize("/home").or_else(|_| {
                            Ok::<_, anyhow::Error>("/".to_string())
                        })
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
}
