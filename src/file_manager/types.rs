use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileEntryKind,
    /// Size in bytes; used to choose pipelined reads for large files.
    pub size: u64,
    /// 符号链接目标是否为目录；普通条目恒为 false。
    #[serde(default)]
    pub symlink_target_is_dir: bool,
}

impl FileEntry {
    pub fn from_dir_entry(entry: std::fs::DirEntry) -> Option<Self> {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type().ok()?;
        let kind = if file_type.is_dir() {
            FileEntryKind::Directory
        } else if file_type.is_file() {
            FileEntryKind::File
        } else if file_type.is_symlink() {
            FileEntryKind::Symlink
        } else {
            FileEntryKind::Other
        };
        let size = entry.metadata().map_or(0, |m| m.len());
        let symlink_target_is_dir = file_type.is_symlink()
            && std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
        Some(Self {
            name,
            path,
            kind,
            size,
            symlink_target_is_dir,
        })
    }

    pub fn is_dir(&self) -> bool {
        self.kind == FileEntryKind::Directory
            || (self.kind == FileEntryKind::Symlink && self.symlink_target_is_dir)
    }

    pub fn sort_key(&self) -> (u8, String) {
        let rank = match self.kind {
            FileEntryKind::Directory => 0,
            FileEntryKind::Symlink => 1,
            FileEntryKind::File => 2,
            FileEntryKind::Other => 3,
        };
        (rank, self.name.to_lowercase())
    }
}
