use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileEntryKind,
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
        Some(Self { name, path, kind })
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
