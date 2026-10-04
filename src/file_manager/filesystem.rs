use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::file_manager::types::{FileEntry, FileEntryKind};
use crate::file_manager::utils;
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

    /// 创建目录（非递归语义：父目录必须已存在，已存在时报错）。
    /// 用于文件树「新建文件夹」——目标目录内已存在同名项时返回 `AlreadyExists`
    /// 而不是静默成功。本地与远端必须保持同一语义（都不能偷偷补父链）。
    /// 默认实现报「不支持」：测试 Mock / 只读后端无需关心写操作。
    fn create_dir(&self, _path: &Path) -> Result<(), FileOpError> {
        Err(FileOpError::unsupported("创建目录"))
    }

    /// 创建空文件；目标已存在时返回 `AlreadyExists`（不覆盖既有内容）。
    fn create_file(&self, _path: &Path) -> Result<(), FileOpError> {
        Err(FileOpError::unsupported("创建文件"))
    }

    /// 重命名/移动到同目录下的新名字（仅文件名变化，不改父目录）。
    /// 目标已存在时返回 `AlreadyExists`，不静默覆盖。
    fn rename_path(&self, _from: &Path, _to: &Path) -> Result<(), FileOpError> {
        Err(FileOpError::unsupported("重命名"))
    }

    /// 删除文件或目录（目录递归删除；符号链接只删链接本身不逃逸目标内容）。
    fn remove_path(&self, _path: &Path) -> Result<(), FileOpError> {
        Err(FileOpError::unsupported("删除"))
    }
}

/// 文件树写操作（新建 / 重命名 / 删除）的失败原因。
///
/// 单独拆出 `AlreadyExists` 是为了让调用方能区分「同名冲突」与「真的出错」：
/// 新建时占位名撞车可以自动换名重试，而不是给用户弹一个看不懂的 IO 错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOpError {
    /// 目标路径已存在（新建 / 重命名的同名冲突）。
    AlreadyExists(PathBuf),
    /// 其它错误：权限不足、父目录缺失、网络中断、后端不支持……
    Other(String),
}

impl FileOpError {
    fn unsupported(op: &str) -> Self {
        Self::Other(format!("此后端不支持{op}"))
    }
}

impl std::fmt::Display for FileOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists(path) => write!(f, "目标已存在: {}", path.display()),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for FileOpError {}

/// 远端 anyhow 错误 → `FileOpError`（远端没有 errno 可查，无法细分冲突类型）。
fn remote_err(error: anyhow::Error) -> FileOpError {
    FileOpError::Other(error.to_string())
}

/// 收口 `with_cached_session` 的双层结果：外层是「会话没拿到 / 连不上」，
/// 内层是操作本身的结果。两者都折成 `FileOpError`。
fn flatten_remote<T>(result: anyhow::Result<Result<T, FileOpError>>) -> Result<T, FileOpError> {
    match result {
        Ok(inner) => inner,
        Err(error) => Err(remote_err(error)),
    }
}

/// 把 `std::io::Error` 归到 `FileOpError`，`AlreadyExists` 单独拎出来。
fn map_io_error(error: std::io::Error, path: &Path) -> FileOpError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        FileOpError::AlreadyExists(path.to_path_buf())
    } else {
        FileOpError::Other(error.to_string())
    }
}

/// 重命名且不覆盖已存在的目标。
///
/// POSIX `rename(2)` 会**静默覆盖**同名目标，用户重命名时等于无警示地丢文件。
/// 这里在支持的平台上走原子的 no-replace 变体（不存在「先检查再改名」的
/// TOCTOU 窗口）：macOS 用 `renameatx_np(RENAME_EXCL)`，Linux/gnu 用
/// `renameat2(RENAME_NOREPLACE)`；其余平台降级为「先探测再改名」。
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // 目标存在时返回 EEXIST，而不是静默覆盖。
        let ret = unsafe {
            libc::renameatx_np(
                libc::AT_FDCWD,
                from_c.as_ptr(),
                libc::AT_FDCWD,
                to_c.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if ret == 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let ret = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from_c.as_ptr(),
                libc::AT_FDCWD,
                to_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if ret == 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }

    #[cfg(not(any(target_vendor = "apple", all(target_os = "linux", target_env = "gnu"))))]
    {
        // 降级路径存在 TOCTOU 窗口，但该平台没有可用的原子接口。
        // 用 symlink_metadata 而非 exists()：后者对指向不存在目标的符号链接
        // 返回 false，会让 rename 静默覆盖掉这个链接。
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("目标已存在: {}", to.display()),
            ));
        }
        std::fs::rename(from, to)
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

    fn create_dir(&self, path: &Path) -> Result<(), FileOpError> {
        // create_dir（非 create_dir_all）：已存在时返回 AlreadyExists，
        // 正好作为「同名冲突」的信号；父目录不存在时也会报错。
        std::fs::create_dir(path).map_err(|e| map_io_error(e, path))
    }

    fn create_file(&self, path: &Path) -> Result<(), FileOpError> {
        // create_new：原子拒绝覆盖，避免「检查存在 → 创建」的 TOCTOU。
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|_| ())
            .map_err(|e| map_io_error(e, path))
    }

    fn rename_path(&self, from: &Path, to: &Path) -> Result<(), FileOpError> {
        // 原子的 no-replace 重命名：不会静默覆盖同名目标（详见函数注释）。
        rename_no_replace(from, to).map_err(|e| map_io_error(e, to))
    }

    fn remove_path(&self, path: &Path) -> Result<(), FileOpError> {
        utils::remove_local_source(path).map_err(|e| FileOpError::Other(e.to_string()))
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

    fn create_dir(&self, path: &Path) -> Result<(), FileOpError> {
        let path_str = path.to_string_lossy().into_owned();
        flatten_remote(sftp_fs::with_cached_session(&self.config, |session| {
            // 与本地 `create_dir` 对齐的非递归语义：目标已存在 → 冲突错误；
            // 父目录不存在 → 报错，绝不逐级补齐父链。用 create_dir_all 会让
            // 远端「新建文件夹」悄悄把缺失的父目录一起建出来，与本地行为分裂。
            if session.exists(&path_str)? {
                return Ok(Err(FileOpError::AlreadyExists(path.to_path_buf())));
            }
            // 远端路径以 `/` 分隔，`Path::parent()` 在 Windows 上同样认 `/`。
            if let Some(parent) = path.parent() {
                let parent_str = parent.to_string_lossy();
                if !parent_str.is_empty() && !session.exists(&parent_str)? {
                    return Ok(Err(FileOpError::Other(format!(
                        "父目录不存在: {}",
                        parent_str
                    ))));
                }
            }
            Ok(session.create_dir(&path_str).map_err(remote_err))
        }))
    }

    fn create_file(&self, path: &Path) -> Result<(), FileOpError> {
        let path_str = path.to_string_lossy().into_owned();
        flatten_remote(sftp_fs::with_cached_session(&self.config, |session| {
            if session.exists(&path_str)? {
                return Ok(Err(FileOpError::AlreadyExists(path.to_path_buf())));
            }
            // 写入空内容即创建空文件；已存在时上面已拒绝。
            Ok(session.write_file_bytes(&path_str, b"").map_err(remote_err))
        }))
    }

    fn rename_path(&self, from: &Path, to: &Path) -> Result<(), FileOpError> {
        let from_str = from.to_string_lossy().into_owned();
        let to_str = to.to_string_lossy().into_owned();
        flatten_remote(sftp_fs::with_cached_session(&self.config, |session| {
            // 部分 SSH 服务器（posix-rename 扩展）rename 会覆盖目标：先拒绝。
            if session.exists(&to_str)? {
                return Ok(Err(FileOpError::AlreadyExists(to.to_path_buf())));
            }
            Ok(session.rename(&from_str, &to_str).map_err(remote_err))
        }))
    }

    fn remove_path(&self, path: &Path) -> Result<(), FileOpError> {
        let path_str = path.to_string_lossy().into_owned();
        flatten_remote(sftp_fs::with_cached_session(&self.config, |session| {
            Ok(utils::remove_remote_source(session, &path_str).map_err(remote_err))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deepterm-fs-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn local_create_file_conflicts_with_existing() {
        let dir = temp_dir("create-file");
        let path = dir.join("a.txt");
        assert!(LocalFileSystem.create_file(&path).is_ok());
        // 已存在：报错而不是清空既有内容
        assert!(LocalFileSystem.create_file(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_create_dir_conflicts_with_existing() {
        let dir = temp_dir("create-dir");
        let child = dir.join("sub");
        assert!(LocalFileSystem.create_dir(&child).is_ok());
        assert!(LocalFileSystem.create_dir(&child).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_rename_refuses_existing_target() {
        let dir = temp_dir("rename");
        let from = dir.join("a.txt");
        let to = dir.join("b.txt");
        LocalFileSystem.create_file(&from).unwrap();
        LocalFileSystem.create_file(&to).unwrap();
        // 目标已存在：拒绝（不静默覆盖）
        assert!(LocalFileSystem.rename_path(&from, &to).is_err());
        // 目标不存在：成功
        let to2 = dir.join("c.txt");
        assert!(LocalFileSystem.rename_path(&from, &to2).is_ok());
        assert!(to2.exists());
        assert!(!from.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_remove_recursive_and_symlink_safe() {
        let dir = temp_dir("remove");
        let nested = dir.join("d/e");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("f.txt"), "x").unwrap();
        assert!(LocalFileSystem.remove_path(&dir.join("d")).is_ok());
        assert!(!dir.join("d").exists());

        // 符号链接：只删链接本身，不递归删链接目标
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        // windows 创建符号链接需要开发者模式或 SeCreateSymbolicLink 权限，
        // 拿不到就跳过这一段（上面的递归删除已经覆盖主路径）。
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&outside, dir.join("link")).is_ok();
        #[cfg(unix)]
        let linked = {
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
            true
        };
        if linked {
            assert!(LocalFileSystem.remove_path(&dir.join("link")).is_ok());
            assert!(outside.exists(), "链接目标必须保留");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_rename_reports_already_exists_without_clobbering() {
        let dir = temp_dir("rename-conflict");
        let from = dir.join("a.txt");
        let to = dir.join("b.txt");
        LocalFileSystem.create_file(&from).unwrap();
        std::fs::write(&to, "keep me").unwrap();

        // 冲突要能被识别出来（调用方据此决定提示或自动换名），且不能覆盖目标。
        assert_eq!(
            LocalFileSystem.rename_path(&from, &to),
            Err(FileOpError::AlreadyExists(to.clone()))
        );
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "keep me");
        assert!(from.exists(), "源不能被移走");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_create_reports_already_exists() {
        let dir = temp_dir("create-conflict");
        let path = dir.join("a.txt");
        LocalFileSystem.create_file(&path).unwrap();
        assert_eq!(
            LocalFileSystem.create_file(&path),
            Err(FileOpError::AlreadyExists(path.clone()))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
