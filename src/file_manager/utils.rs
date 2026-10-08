use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{anyhow, bail};

use crate::file_manager::{
    ConflictPolicy, ConflictPrompt, ConflictResolution, ConflictResolver, wait_for_resolution,
};
use crate::ssh::sftp_fs::{self, RemoteFileKind};

pub use crate::ssh::TRANSFER_CANCELLED_MSG;
pub use crate::utils::shell::{shell_quote_single, shell_single_quote};

#[derive(Clone, Debug, Default)]
pub struct TransferSummary {
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    /// 因冲突选择"跳过"、或遇到不支持的条目（符号链接 / 特殊文件）而未传输的条目数。
    /// Move 操作只有在 `skipped == 0` 时才允许删除源，避免把未传输的内容连源一起删掉。
    pub skipped: usize,
}

impl TransferSummary {
    pub fn one_file(bytes_transferred: u64) -> Self {
        Self {
            files_transferred: 1,
            bytes_transferred,
            skipped: 0,
        }
    }

    /// 记录一个被跳过的条目（冲突跳过 / 不支持的类型）。
    pub fn one_skipped() -> Self {
        Self {
            files_transferred: 0,
            bytes_transferred: 0,
            skipped: 1,
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
        self.files_transferred = self
            .files_transferred
            .saturating_add(other.files_transferred);
        self.bytes_transferred = self
            .bytes_transferred
            .saturating_add(other.bytes_transferred);
        self.skipped = self.skipped.saturating_add(other.skipped);
    }
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

/// 校验用户输入的重命名文件名，防止路径遍历逃出目标目录。
///
/// 拒绝：空字符串、含 `/` `\` `\0`、为 `.`/`..`、或经 `components` 解析后不止一个
/// 正常组件（含 `../` 前缀）。调用方应在 `destination_dir.join(rename_to)` 之前调用。
pub fn validate_rename_to(rename_to: &str) -> anyhow::Result<()> {
    if rename_to.is_empty() {
        bail!("重命名文件名不能为空");
    }
    if rename_to.contains('\0') {
        bail!("重命名文件名不能包含空字符");
    }
    if rename_to.contains('/') || rename_to.contains('\\') {
        bail!("重命名文件名不能包含路径分隔符: {rename_to:?}");
    }
    if rename_to == "." || rename_to == ".." {
        bail!("重命名文件名不能为 {rename_to:?}");
    }
    // 防御编码变体：按 Path 组件解析，必须恰好一个正常文件组件。
    // `..` 会被解析为 ParentDir，此处连同未来改动一并收敛到单次匹配中确认。
    let mut components = Path::new(rename_to).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(()),
        _ => bail!("重命名文件名不合法: {rename_to:?}"),
    }
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
    // 用 symlink_metadata 不跟随符号链接：若 path 是指向目录的符号链接，
    // 应当只删链接本身（remove_file），绝不能递归删除链接目标的内容。
    let metadata = fs::symlink_metadata(path)?;
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
    // 用 symlink_metadata 不跟随符号链接：若 path 是指向目录的符号链接，
    // 应当只删链接本身（remove_file），绝不能递归删除链接目标的内容。
    let metadata = session.symlink_metadata(path)?;
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

/// 重命名候选的核心算法：`stem + _副本N + .ext`（index 从 1 起）。
/// local / remote 两版候选生成共享同一段规则，避免两处各自演化。
fn renamed_file_name(file_name: &str, index: usize) -> String {
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (file_name, None),
    };
    let suffix = if index == 1 {
        "_副本".to_string()
    } else {
        format!("_副本{}", index - 1)
    };
    match extension {
        Some(extension) => format!("{}{}.{}", stem, suffix, extension),
        None => format!("{}{}", stem, suffix),
    }
}

/// 弹出冲突询问对话框并等待用户响应；`Ask` 策略的公共实现。
fn prompt_conflict(
    file_name: String,
    destination_path: String,
    resolver: &dyn ConflictResolver,
    cancelled: &AtomicBool,
) -> anyhow::Result<ConflictResolution> {
    let (tx, rx) = async_channel::bounded(1);
    resolver.prompt(ConflictPrompt {
        file_name,
        destination_path,
        response: tx,
    });
    wait_for_resolution(&rx, cancelled)
}

/// AutoRename 的可用名探测循环（本地版）：逐个候选检查磁盘存在性，
/// 1..10_000 内找不到空位即失败。
fn first_available_local_rename(
    desired_path: &Path,
    cancelled: &AtomicBool,
) -> anyhow::Result<Option<PathBuf>> {
    for index in 1..10_000 {
        if cancelled.load(Ordering::Relaxed) {
            bail!("{}", TRANSFER_CANCELLED_MSG);
        }
        let candidate = renamed_local_candidate(desired_path, index);
        if !candidate.exists() {
            return Ok(Some(candidate));
        }
    }
    bail!("无法为 {} 找到可用的文件名", desired_path.display())
}

pub fn resolve_local_conflict(
    desired_path: &Path,
    conflict_policy: ConflictPolicy,
    resolver: Option<&dyn ConflictResolver>,
    cancelled: &AtomicBool,
) -> anyhow::Result<Option<PathBuf>> {
    if !desired_path.exists() {
        return Ok(Some(desired_path.to_path_buf()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_path_buf())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => {
            let resolver = resolver.ok_or_else(|| anyhow!("Ask 策略需要提供冲突解决器"))?;
            let file_name = desired_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let resolution = prompt_conflict(
                file_name,
                desired_path.to_string_lossy().to_string(),
                resolver,
                cancelled,
            )?;
            match resolution {
                ConflictResolution::Skip => Ok(None),
                ConflictResolution::Overwrite => Ok(Some(desired_path.to_path_buf())),
                ConflictResolution::AutoRename => {
                    first_available_local_rename(desired_path, cancelled)
                }
                ConflictResolution::Stop => bail!("{}", TRANSFER_CANCELLED_MSG),
            }
        }
        ConflictPolicy::AutoRename => first_available_local_rename(desired_path, cancelled),
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
    let original_name = match extension {
        Some(ref extension) => format!("{}.{}", stem, extension),
        None => stem,
    };
    parent.join(renamed_file_name(&original_name, index))
}

pub fn resolve_remote_conflict(
    session: &mut sftp_fs::BlockingSftpSession,
    desired_path: &str,
    conflict_policy: ConflictPolicy,
    resolver: Option<&dyn ConflictResolver>,
    cancelled: &AtomicBool,
) -> anyhow::Result<Option<String>> {
    if !session.exists(desired_path)? {
        return Ok(Some(desired_path.to_string()));
    }

    match conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(desired_path.to_string())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Ask => {
            let resolver = resolver.ok_or_else(|| anyhow!("Ask 策略需要提供冲突解决器"))?;
            let file_name = remote_file_name(desired_path).unwrap_or_else(|_| "file".to_string());
            let resolution =
                prompt_conflict(file_name, desired_path.to_string(), resolver, cancelled)?;
            match resolution {
                ConflictResolution::Skip => Ok(None),
                ConflictResolution::Overwrite => Ok(Some(desired_path.to_string())),
                ConflictResolution::AutoRename => Ok(first_available_remote_rename(
                    session,
                    desired_path,
                    cancelled,
                )?),
                ConflictResolution::Stop => bail!("{}", TRANSFER_CANCELLED_MSG),
            }
        }
        ConflictPolicy::AutoRename => {
            first_available_remote_rename(session, desired_path, cancelled)
        }
    }
}

/// AutoRename 的可用名探测。
///
/// 此前对 1..10_000 逐个候选发一次 `session.exists`（每次一个 SFTP RTT）——
/// 目录里已有上千个 `xxx_副本N` 时一次改名要付出 O(N) 次网络往返（50ms/RTT
/// 下 1000 个副本 ≈ 50 秒）。改为一次 `read_dir` 把父目录现有名字收进
/// HashSet，本地直接跳到第一个空位；read_dir 失败时退回逐个探测。
fn first_available_remote_rename(
    session: &mut sftp_fs::BlockingSftpSession,
    desired_path: &str,
    cancelled: &AtomicBool,
) -> anyhow::Result<Option<String>> {
    let parent = sftp_fs::remote_parent(desired_path)
        .ok_or_else(|| anyhow!("无法解析远程父目录: {desired_path}"))?;
    let existing: Option<std::collections::HashSet<String>> = session
        .read_dir(&parent)
        .ok()
        .map(|entries| entries.into_iter().map(|e| e.name).collect());

    for index in 1..10_000 {
        if cancelled.load(Ordering::Relaxed) {
            bail!("{}", TRANSFER_CANCELLED_MSG);
        }
        let candidate = renamed_remote_candidate(desired_path, index)?;
        let taken = match &existing {
            Some(names) => names.contains(&remote_file_name(&candidate).unwrap_or_default()),
            None => session.exists(&candidate)?,
        };
        if !taken {
            return Ok(Some(candidate));
        }
    }
    bail!("无法为 {} 找到可用的远程文件名", desired_path)
}

pub fn renamed_remote_candidate(path: &str, index: usize) -> anyhow::Result<String> {
    let parent = sftp_fs::remote_parent(path).unwrap_or_else(|| "/".to_string());
    let file_name = remote_file_name(path)?;
    let candidate_name = renamed_file_name(&file_name, index);
    Ok(sftp_fs::join_remote_path(&parent, &candidate_name))
}

/// 远程目录递归传输的骨架抽象。
///
/// 同会话复制、远程→本地下载、跨会话复制三个场景共享「遍历条目 → 冲突
/// 解决 → 文件/目录分派 → 汇总 TransferSummary」的循环结构；差异只在
/// 目标路径类型、冲突探测和单条目传输方式。各场景实现本 trait 后交给
/// [`walk_remote_dir`] 统一驱动，消除三份逐行雷同的循环体。
///
/// `D` 为目标路径类型（`String` 远程 / `PathBuf` 本地）。
pub trait DirTransferDriver {
    type Destination;

    /// 读取远程源目录的条目列表。
    fn read_entries(&mut self, remote_dir: &str) -> anyhow::Result<Vec<sftp_fs::RemoteFileEntry>>;

    /// 源目录 entry 名 → 目标目录下的完整路径。
    fn child_destination(
        &self,
        destination_dir: &Self::Destination,
        name: &str,
    ) -> Self::Destination;

    /// 冲突解决：`None` 表示跳过。
    fn resolve_conflict(
        &mut self,
        desired: &Self::Destination,
        policy: ConflictPolicy,
        resolver: Option<&dyn ConflictResolver>,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<Option<Self::Destination>>;

    /// 进入一个目录层前的准备：创建目标目录并把源目录权限同步过去。
    ///
    /// 每层递归都会调用（与原三份内联实现一致），保证子目录的
    /// mode 位在复制后与源保持一致。
    fn prepare_destination_dir(
        &mut self,
        remote_dir: &str,
        destination_dir: &Self::Destination,
    ) -> anyhow::Result<()>;

    /// 传输单个文件，返回增量统计。
    fn transfer_file(
        &mut self,
        source_path: &str,
        destination: &Self::Destination,
        cancelled: &Arc<AtomicBool>,
        progress: Option<&AtomicU64>,
    ) -> anyhow::Result<TransferSummary>;
}

/// 远程目录递归传输的统一 walker。三个传输场景（同会话复制 /
/// 下载到本地 / 跨会话复制）的公共循环：
/// 条目分派 → 冲突 resolve → `Some` 递归或传输 / `None` skip /
/// 不支持条目 warn + skip。
pub fn walk_remote_dir<D>(
    driver: &mut dyn DirTransferDriver<Destination = D>,
    remote_dir: &str,
    destination_dir: &D,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    // 每层递归先建目录并同步源目录权限（含最顶层）。
    driver.prepare_destination_dir(remote_dir, destination_dir)?;

    let mut summary = TransferSummary::default();

    for entry in driver.read_entries(remote_dir)? {
        check_cancelled(cancelled)?;
        let destination_path = driver.child_destination(destination_dir, &entry.name);

        if entry.kind == RemoteFileKind::File {
            match driver.resolve_conflict(
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => {
                    summary.merge(driver.transfer_file(&entry.path, &path, cancelled, progress)?)
                }
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else if entry.kind == RemoteFileKind::Directory {
            match driver.resolve_conflict(
                &destination_path,
                conflict_policy,
                resolver,
                cancelled,
            )? {
                Some(path) => {
                    // prepare_destination_dir 在递归的 walk_remote_dir 里执行，
                    // 这里无需再建目录。
                    summary.merge(walk_remote_dir(
                        driver,
                        &entry.path,
                        &path,
                        conflict_policy,
                        cancelled,
                        progress,
                        resolver,
                    )?);
                }
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else {
            // 符号链接 / 特殊文件：计为跳过，避免 Move 在"看似全部成功"时误删源。
            log::warn!("跳过不支持的远程条目 ({:?}): {}", entry.kind, entry.path);
            summary.merge(TransferSummary::one_skipped());
        }
    }

    Ok(summary)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_single_quote_plain() {
        assert_eq!(shell_single_quote("hello"), "hello");
    }

    #[test]
    fn shell_single_quote_with_apostrophe() {
        // foo'bar → 在单引号上下文中: 'foo'\''bar' → shell 得到 foo'bar
        assert_eq!(shell_single_quote("foo'bar"), "foo'\\''bar");
    }

    #[test]
    fn shell_single_quote_with_backslash() {
        // 反斜杠在单引号内原样保留，不做转义
        assert_eq!(shell_single_quote("foo\\bar"), "foo\\bar");
    }

    #[test]
    fn shell_single_quote_with_backslash_and_apostrophe() {
        // foo\'bar → 在单引号上下文中: 'foo\'bar' → shell 原样得到 foo\'bar
        assert_eq!(shell_single_quote("foo\\'bar"), "foo\\'\\''bar");
    }

    #[test]
    fn validate_rename_to_accepts_normal_names() {
        assert!(validate_rename_to("file.txt").is_ok());
        assert!(validate_rename_to("报告-2024_final.tar.gz").is_ok());
        assert!(validate_rename_to("a").is_ok());
    }

    #[test]
    fn validate_rename_to_rejects_traversal() {
        // 路径分隔符与父目录引用必须拒绝，防止逃出目标目录。
        assert!(validate_rename_to("").is_err());
        assert!(validate_rename_to(".").is_err());
        assert!(validate_rename_to("..").is_err());
        assert!(validate_rename_to("../evil").is_err());
        assert!(validate_rename_to("a/b").is_err());
        assert!(validate_rename_to("a\\b").is_err());
        assert!(validate_rename_to("a\0b").is_err());
        assert!(validate_rename_to("/abs/path").is_err());
    }

    #[test]
    fn renamed_file_name_suffix_rules() {
        // 共享核心：_副本 / _副本N 的编号规则（N 从第 2 个候选起）。
        assert_eq!(renamed_file_name("file.txt", 1), "file_副本.txt");
        assert_eq!(renamed_file_name("file.txt", 2), "file_副本1.txt");
        assert_eq!(renamed_file_name("file.txt", 3), "file_副本2.txt");
        assert_eq!(renamed_file_name("plain", 1), "plain_副本");
        // 多级扩展名只在最后一段后插入后缀（与 Path::file_stem/extension 语义一致）
        assert_eq!(
            renamed_file_name("archive.tar.gz", 1),
            "archive.tar_副本.gz"
        );
        // 隐藏文件不把首点当扩展名分隔
        assert_eq!(renamed_file_name(".bashrc", 1), ".bashrc_副本");
    }

    #[test]
    fn renamed_local_candidate_keeps_parent_dir() {
        let candidate = renamed_local_candidate(Path::new("/tmp/report.pdf"), 1);
        assert_eq!(candidate, Path::new("/tmp/report_副本.pdf"));
        let candidate = renamed_local_candidate(Path::new("/tmp/report.pdf"), 2);
        assert_eq!(candidate, Path::new("/tmp/report_副本1.pdf"));
        // 隐藏文件 / 无扩展名也保持与旧实现一致
        assert_eq!(
            renamed_local_candidate(Path::new("/tmp/.bashrc"), 1),
            Path::new("/tmp/.bashrc_副本")
        );
        assert_eq!(
            renamed_local_candidate(Path::new("/tmp/Makefile"), 1),
            Path::new("/tmp/Makefile_副本")
        );
    }

    #[test]
    fn renamed_remote_candidate_keeps_parent_dir() {
        let candidate = renamed_remote_candidate("/srv/data/report.pdf", 1).unwrap();
        assert_eq!(candidate, "/srv/data/report_副本.pdf");
        let candidate = renamed_remote_candidate("/srv/data/report.pdf", 2).unwrap();
        assert_eq!(candidate, "/srv/data/report_副本1.pdf");
        // 根目录下的文件不能拼出 "//"
        assert_eq!(
            renamed_remote_candidate("/report.pdf", 1).unwrap(),
            "/report_副本.pdf"
        );
    }
}
