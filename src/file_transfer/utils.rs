use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail};

use crate::file_transfer::{
    wait_for_resolution, ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver,
};
use crate::ssh::sftp_fs::{self, RemoteFileKind};

pub const TRANSFER_CANCELLED_MSG: &str = "transfer cancelled";

#[derive(Clone, Debug, Default)]
pub struct TransferSummary {
    pub files_transferred: usize,
    pub bytes_transferred: u64,
}

impl TransferSummary {
    pub fn one_file(bytes_transferred: u64) -> Self {
        Self {
            files_transferred: 1,
            bytes_transferred,
        }
    }

    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        if metadata.is_file() {
            Self::one_file(metadata.len())
        } else {
            Self::default()
        }
    }

    pub fn from_remote_metadata(metadata: &crate::ssh::sftp_fs::RemoteFileMetadata) -> Self {
        if metadata.kind == RemoteFileKind::File {
            Self::one_file(metadata.size)
        } else {
            Self::default()
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.files_transferred = self.files_transferred.saturating_add(other.files_transferred);
        self.bytes_transferred = self.bytes_transferred.saturating_add(other.bytes_transferred);
    }
}

pub fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

pub fn local_file_name(path: &Path) -> anyhow::Result<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| anyhow!("{} 没有文件名", path.display()))
}

pub fn remote_file_name(path: &str) -> anyhow::Result<String> {
    let trimmed = path.trim_end_matches('/');
    let file_name = trimmed
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("{} 没有文件名", path))?;
    Ok(file_name.to_string())
}

pub fn is_local_noop_move(source_path: &Path, destination_dir: &Path) -> bool {
    if source_path == destination_dir {
        return true;
    }
    let source_components: Vec<_> = source_path.components().collect();
    let dest_components: Vec<_> = destination_dir.components().collect();
    if source_components == dest_components {
        return true;
    }
    if source_components.len() >= 2 {
        let parent = &source_components[..source_components.len() - 1];
        if parent == dest_components.as_slice() {
            return true;
        }
    }
    false
}

pub fn is_remote_noop_move(remote_path: &str, destination_dir: &str) -> bool {
    let source = normalize_remote_dir(remote_path);
    let destination = normalize_remote_dir(destination_dir);
    if source == destination {
        return true;
    }
    sftp_fs::remote_parent(remote_path)
        .map(|parent| normalize_remote_dir(&parent) == destination)
        .unwrap_or(false)
}

pub fn normalize_remote_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn temporary_local_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "transfer".to_string());
    path.with_file_name(format!(".{}.deepterm-part", file_name))
}

pub fn temporary_remote_path(path: &str) -> String {
    let parent = sftp_fs::remote_parent(path).unwrap_or_else(|| "/".to_string());
    let file_name = remote_file_name(path).unwrap_or_else(|_| "transfer".to_string());
    sftp_fs::join_remote_path(&parent, &format!(".{}.deepterm-part", file_name))
}

pub fn prevent_local_recursive_copy(
    source_dir: &Path,
    destination_dir: &Path,
) -> anyhow::Result<()> {
    let source_dir = source_dir.canonicalize()?;
    let destination_parent = destination_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let destination_parent = destination_parent.canonicalize()?;
    if destination_parent.starts_with(&source_dir) {
        bail!("不能将文件夹复制到自身内部: {}", source_dir.display());
    }
    Ok(())
}

pub fn prevent_remote_recursive_copy(
    source_dir: &str,
    destination_dir: &str,
) -> anyhow::Result<()> {
    let source = normalize_remote_dir(source_dir);
    let destination = normalize_remote_dir(destination_dir);
    if destination.starts_with(&format!("{}/", source.trim_end_matches('/')))
        || destination == source
    {
        bail!("不能将远程文件夹复制到自身内部: {}", source_dir);
    }
    Ok(())
}

pub fn check_cancelled(cancelled: &Arc<AtomicBool>) -> anyhow::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("{}", TRANSFER_CANCELLED_MSG);
    }
    Ok(())
}

pub fn remove_local_source(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn remove_remote_source(
    session: &mut sftp_fs::BlockingSftpSession,
    path: &str,
) -> anyhow::Result<()> {
    let metadata = session.metadata(path)?;
    match metadata.kind {
        RemoteFileKind::File | RemoteFileKind::Symlink | RemoteFileKind::Other => {
            session.remove_file(path)?;
        }
        RemoteFileKind::Directory => {
            remove_remote_dir_recursive(session, path)?;
        }
    }
    Ok(())
}

pub fn remove_remote_dir_recursive(
    session: &mut sftp_fs::BlockingSftpSession,
    path: &str,
) -> anyhow::Result<()> {
    for entry in session.read_dir(path)? {
        match entry.kind {
            RemoteFileKind::Directory => remove_remote_dir_recursive(session, &entry.path)?,
            RemoteFileKind::File | RemoteFileKind::Symlink | RemoteFileKind::Other => {
                session.remove_file(&entry.path)?;
            }
        }
    }
    session.remove_dir(path)?;
    Ok(())
}

pub fn resolve_local_conflict(
    desired_path: &Path,
    conflict_policy: ConflictPolicy,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<Option<PathBuf>> {
    if !desired_path.exists() {
        return Ok(Some(desired_path.to_path_buf()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_path_buf())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => {
            let resolver = resolver
                .ok_or_else(|| anyhow!("Ask 策略需要提供冲突解决器"))?;
            let (tx, rx) = async_channel::bounded(1);
            let file_name = desired_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            resolver.prompt(ConflictPrompt {
                file_name,
                destination_path: desired_path.to_string_lossy().to_string(),
                response: tx,
            });

            match wait_for_resolution(&rx)? {
                ConflictResolution::Skip => Ok(None),
                ConflictResolution::Overwrite => Ok(Some(desired_path.to_path_buf())),
                ConflictResolution::AutoRename => {
                    for index in 1..10_000 {
                        let candidate = renamed_local_candidate(desired_path, index);
                        if !candidate.exists() {
                            return Ok(Some(candidate));
                        }
                    }
                    bail!(
                        "无法为 {} 找到可用的文件名",
                        desired_path.display()
                    )
                }
                ConflictResolution::Stop => bail!("{}", TRANSFER_CANCELLED_MSG),
            }
        }
        ConflictPolicy::AutoRename => {
            for index in 1..10_000 {
                let candidate = renamed_local_candidate(desired_path, index);
                if !candidate.exists() {
                    return Ok(Some(candidate));
                }
            }
            bail!(
                "无法为 {} 找到可用的文件名",
                desired_path.display()
            )
        }
    }
}

pub fn renamed_local_candidate(path: &Path, index: usize) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy());
    let suffix = if index == 1 {
        "_副本".to_string()
    } else {
        format!("_副本{}", index - 1)
    };
    let file_name = match extension {
        Some(extension) => format!("{}{}.{}", stem, suffix, extension),
        None => format!("{}{}", stem, suffix),
    };
    parent.join(file_name)
}

pub fn resolve_remote_conflict(
    session: &mut sftp_fs::BlockingSftpSession,
    desired_path: &str,
    conflict_policy: ConflictPolicy,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<Option<String>> {
    if !session.exists(desired_path)? {
        return Ok(Some(desired_path.to_string()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_string())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => {
            let resolver = resolver
                .ok_or_else(|| anyhow!("Ask 策略需要提供冲突解决器"))?;
            let (tx, rx) = async_channel::bounded(1);
            let file_name = remote_file_name(desired_path).unwrap_or_else(|_| "file".to_string());
            resolver.prompt(ConflictPrompt {
                file_name,
                destination_path: desired_path.to_string(),
                response: tx,
            });

            match wait_for_resolution(&rx)? {
                ConflictResolution::Skip => Ok(None),
                ConflictResolution::Overwrite => Ok(Some(desired_path.to_string())),
                ConflictResolution::AutoRename => {
                    for index in 1..10_000 {
                        let candidate = renamed_remote_candidate(desired_path, index)?;
                        if !session.exists(&candidate)? {
                            return Ok(Some(candidate));
                        }
                    }
                    bail!(
                        "无法为 {} 找到可用的远程文件名",
                        desired_path
                    )
                }
                ConflictResolution::Stop => bail!("{}", TRANSFER_CANCELLED_MSG),
            }
        }
        ConflictPolicy::AutoRename => {
            for index in 1..10_000 {
                let candidate = renamed_remote_candidate(desired_path, index)?;
                if !session.exists(&candidate)? {
                    return Ok(Some(candidate));
                }
            }
            bail!(
                "无法为 {} 找到可用的远程文件名",
                desired_path
            )
        }
    }
}

pub fn renamed_remote_candidate(path: &str, index: usize) -> anyhow::Result<String> {
    let parent = sftp_fs::remote_parent(path).unwrap_or_else(|| "/".to_string());
    let file_name = remote_file_name(path)?;
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            (stem.to_string(), Some(extension.to_string()))
        }
        _ => (file_name, None),
    };
    let suffix = if index == 1 {
        "_副本".to_string()
    } else {
        format!("_副本{}", index - 1)
    };
    let candidate_name = match extension {
        Some(extension) => format!("{}{}.{}", stem, suffix, extension),
        None => format!("{}{}", stem, suffix),
    };
    Ok(sftp_fs::join_remote_path(&parent, &candidate_name))
}

pub fn copy_local_file_interruptible(
    source_path: &Path,
    destination_path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<u64> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let mut source = fs::File::open(source_path)?;
    let mut destination = fs::File::create(destination_path)?;
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut transferred = 0;

    loop {
        check_cancelled(cancelled)?;
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        destination.write_all(&buffer[..read])?;
        transferred += read as u64;
    }

    check_cancelled(cancelled)?;
    destination.flush()?;
    Ok(transferred)
}
