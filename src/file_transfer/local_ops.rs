use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context as _;

use crate::file_transfer::utils::{
    TransferSummary, check_cancelled, copy_local_file_interruptible, is_local_noop_move,
    prevent_local_recursive_copy, remove_local_source, resolve_local_conflict,
    temporary_local_path,
};
use crate::file_transfer::{ConflictPolicy, ConflictResolver};

pub fn copy_local_to_local(
    source_path: &Path,
    destination_dir: &Path,
    conflict_policy: ConflictPolicy,
    rename_to: Option<&str>,
    cancelled: &Arc<AtomicBool>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;
    let metadata = fs::metadata(source_path)
        .with_context(|| format!("无法检查源文件 {}", source_path.display()))?;
    fs::create_dir_all(destination_dir)?;
    let file_name = super::utils::local_file_name(source_path)?;
    let effective_name = rename_to.unwrap_or(&file_name);
    let desired_path = destination_dir.join(effective_name);

    if metadata.is_file() {
        let destination_path =
            match resolve_local_conflict(&desired_path, conflict_policy, resolver, cancelled)? {
                Some(path) => path,
                None => return Ok(TransferSummary::one_skipped()),
            };
        if source_path == destination_path {
            // 复制到自身：无意义，按跳过处理（避免把文件截断成空）。
            return Ok(TransferSummary::one_skipped());
        }
        return copy_local_file_to_path(source_path, &destination_path, cancelled);
    }

    if metadata.is_dir() {
        let desired_path = destination_dir.join(&file_name);
        let destination_root =
            match resolve_local_conflict(&desired_path, conflict_policy, resolver, cancelled)? {
                Some(path) => path,
                None => return Ok(TransferSummary::one_skipped()),
            };
        if source_path == destination_root {
            return Ok(TransferSummary::one_skipped());
        }
        prevent_local_recursive_copy(source_path, &destination_root)?;
        return copy_local_dir_to_path(
            source_path,
            &destination_root,
            conflict_policy,
            cancelled,
            resolver,
        );
    }

    // 符号链接 / 特殊文件：计为跳过。
    log::warn!("跳过不支持的本地文件类型: {}", source_path.display());
    Ok(TransferSummary::one_skipped())
}

pub fn move_local_to_local(
    source_path: &Path,
    destination_dir: &Path,
    conflict_policy: ConflictPolicy,
    rename_to: Option<&str>,
    cancelled: &Arc<AtomicBool>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    check_cancelled(cancelled)?;
    let metadata = fs::metadata(source_path)
        .with_context(|| format!("无法检查源文件 {}", source_path.display()))?;
    fs::create_dir_all(destination_dir)?;
    if is_local_noop_move(source_path, destination_dir) {
        return Ok(TransferSummary::from_metadata(&metadata));
    }

    let file_name = super::utils::local_file_name(source_path)?;
    let effective_name = if metadata.is_file() {
        rename_to.unwrap_or(&file_name)
    } else {
        file_name.as_str()
    };
    let destination_path = match resolve_local_conflict(
        &destination_dir.join(effective_name),
        conflict_policy,
        resolver,
        cancelled,
    )? {
        Some(path) => path,
        // 顶层冲突选择跳过：不移动、不删源。
        None => return Ok(TransferSummary::one_skipped()),
    };

    if source_path == destination_path {
        return Ok(TransferSummary::from_metadata(&metadata));
    }

    if metadata.is_dir() {
        prevent_local_recursive_copy(source_path, &destination_path)?;
    }

    match fs::rename(source_path, &destination_path) {
        Ok(()) => Ok(TransferSummary::from_metadata(&metadata)),
        Err(_) => {
            // rename 失败（通常跨设备）⇒ 退化为复制 + 删源。
            // 只有在没有任何条目被跳过时才删源，避免把未复制成功的内容连源一起删掉。
            let summary = copy_local_to_local(
                source_path,
                destination_dir,
                conflict_policy,
                rename_to,
                cancelled,
                resolver,
            )?;
            if summary.skipped == 0 {
                remove_local_source(source_path)?;
            } else {
                log::warn!(
                    "本地移动：有 {} 个条目被跳过，保留源 {} 不删除",
                    summary.skipped,
                    source_path.display()
                );
            }
            Ok(summary)
        }
    }
}

pub fn copy_local_dir_to_path(
    source_dir: &Path,
    destination_dir: &Path,
    conflict_policy: ConflictPolicy,
    cancelled: &Arc<AtomicBool>,
    resolver: Option<&dyn ConflictResolver>,
) -> anyhow::Result<TransferSummary> {
    fs::create_dir_all(destination_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(source_dir) {
            let mode = metadata.permissions().mode();
            let _ = fs::set_permissions(destination_dir, fs::Permissions::from_mode(mode));
        }
    }

    let mut summary = TransferSummary::default();

    for entry in fs::read_dir(source_dir)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        let source_path = entry.path();
        let metadata = entry.metadata()?;
        let destination_path = destination_dir.join(entry.file_name());

        if metadata.is_file() {
            match resolve_local_conflict(&destination_path, conflict_policy, resolver, cancelled)? {
                Some(path) => {
                    summary.merge(copy_local_file_to_path(&source_path, &path, cancelled)?)
                }
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else if metadata.is_dir() {
            match resolve_local_conflict(&destination_path, conflict_policy, resolver, cancelled)? {
                Some(path) => summary.merge(copy_local_dir_to_path(
                    &source_path,
                    &path,
                    conflict_policy,
                    cancelled,
                    resolver,
                )?),
                None => summary.merge(TransferSummary::one_skipped()),
            }
        } else {
            // 符号链接 / 特殊文件：计为跳过，避免 Move 误删源。
            log::warn!("跳过不支持的本地条目: {}", source_path.display());
            summary.merge(TransferSummary::one_skipped());
        }
    }

    Ok(summary)
}

pub fn copy_local_file_to_path(
    source_path: &Path,
    destination_path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<TransferSummary> {
    #[cfg(unix)]
    let source_permissions = fs::metadata(source_path)
        .ok()
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()));

    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary_path = temporary_local_path(destination_path);
    if temporary_path.exists() {
        let _ = fs::remove_file(&temporary_path);
    }
    let result = copy_local_file_interruptible(source_path, &temporary_path, cancelled);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    let transferred = result?;
    if let Err(error) = fs::rename(&temporary_path, destination_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = source_permissions {
            let _ = fs::set_permissions(destination_path, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(TransferSummary::one_file(transferred))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transfer::{ConflictPrompt, ConflictResolution};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 测试用冲突解决器：对每个冲突回复固定决策，并记录被询问的次数。
    struct StubResolver {
        resolution: ConflictResolution,
        prompts: AtomicUsize,
    }

    impl StubResolver {
        fn new(resolution: ConflictResolution) -> Self {
            Self {
                resolution,
                prompts: AtomicUsize::new(0),
            }
        }
    }

    impl ConflictResolver for StubResolver {
        fn prompt(&self, prompt: ConflictPrompt) {
            self.prompts.fetch_add(1, Ordering::Relaxed);
            let _ = prompt.response.try_send(self.resolution.clone());
        }
    }

    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("deepterm-ft-test-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn move_dir_with_skipped_conflict_keeps_source() {
        // 复现数据安全 bug：目录 Move，目标已存在同名文件，用户选"跳过" ⇒
        // 源里那个被跳过的文件绝不能因为"目录移动完成"被删掉。
        let root = unique_temp_dir("move-skip");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        // 源目录 src/data 下有 a.txt、b.txt
        let src_data = src.join("data");
        fs::create_dir_all(&src_data).unwrap();
        fs::write(src_data.join("a.txt"), b"AAA").unwrap();
        fs::write(src_data.join("b.txt"), b"BBB").unwrap();
        // 目标已存在 dst/data/a.txt（制造冲突）
        let dst_data = dst.join("data");
        fs::create_dir_all(&dst_data).unwrap();
        fs::write(dst_data.join("a.txt"), b"OLD").unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let resolver = StubResolver::new(ConflictResolution::Skip);

        // 跨设备 rename 在同一临时目录不会失败，所以这里直接调用目录复制路径
        // 来精确验证"跳过 ⇒ skipped 计数 ⇒ 不删源"的语义。
        let summary = copy_local_dir_to_path(
            &src_data,
            &dst_data,
            ConflictPolicy::Ask,
            &cancelled,
            Some(&resolver),
        )
        .unwrap();

        // a.txt 冲突被跳过，b.txt 正常复制
        assert_eq!(summary.skipped, 1, "a.txt 应计为跳过");
        assert_eq!(summary.files_transferred, 1, "b.txt 应被复制");
        // 源文件必须原样保留
        assert!(src_data.join("a.txt").exists(), "被跳过的源文件必须保留");
        assert_eq!(
            fs::read(dst_data.join("a.txt")).unwrap(),
            b"OLD",
            "目标未被覆盖"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_local_does_not_delete_source_when_skipped() {
        // 端到端：move_local_to_local 顶层冲突选跳过 ⇒ 返回 skipped、不删源。
        let root = unique_temp_dir("move-top-skip");
        let dst = root.join("dst");
        fs::create_dir_all(&dst).unwrap();
        let src_file = root.join("file.txt");
        fs::write(&src_file, b"hello").unwrap();
        // 目标已存在同名文件
        fs::write(dst.join("file.txt"), b"existing").unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let resolver = StubResolver::new(ConflictResolution::Skip);

        let summary = move_local_to_local(
            &src_file,
            &dst,
            ConflictPolicy::Ask,
            None,
            &cancelled,
            Some(&resolver),
        )
        .unwrap();

        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.files_transferred, 0);
        // 源文件必须还在（没有被移动也没有被删）
        assert!(src_file.exists(), "跳过时源文件必须保留");
        assert_eq!(fs::read(dst.join("file.txt")).unwrap(), b"existing");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_to_same_dir_conflict_is_not_silently_swallowed() {
        // 修复 #5：把文件复制到它所在的同一目录，应走冲突解决（这里选覆盖），
        // 而不是被 is_local_noop_move 当成空操作直接吞掉。
        let root = unique_temp_dir("copy-same-dir");
        let src_file = root.join("file.txt");
        fs::write(&src_file, b"data").unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let resolver = StubResolver::new(ConflictResolution::Skip);

        // 复制 root/file.txt 到 root/（同目录）⇒ 命中自身冲突
        let summary = copy_local_to_local(
            &src_file,
            &root,
            ConflictPolicy::Ask,
            None,
            &cancelled,
            Some(&resolver),
        )
        .unwrap();

        // 关键：必须真的进入了冲突判定（被询问或识别为自身），而不是静默返回空 default。
        // 选 Skip ⇒ skipped 计数为 1，文件保持原样。
        assert_eq!(summary.skipped, 1, "同目录复制应识别为冲突并按跳过处理");
        assert!(src_file.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_dir_autorename_keeps_both() {
        // AutoRename（保留两者）：目标冲突时复制到 _副本，源与目标都保留。
        let root = unique_temp_dir("copy-autorename");
        let src = root.join("data");
        let dst = root.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("x.txt"), b"NEW").unwrap();
        fs::create_dir_all(dst.join("data")).unwrap();
        fs::write(dst.join("data").join("x.txt"), b"OLD").unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let resolver = StubResolver::new(ConflictResolution::AutoRename);

        let summary = copy_local_to_local(
            &src,
            &dst,
            ConflictPolicy::Ask,
            None,
            &cancelled,
            Some(&resolver),
        )
        .unwrap();

        assert_eq!(summary.skipped, 0);
        // 原 dst/data/x.txt 保留，新内容进入 dst/data_副本/
        assert_eq!(fs::read(dst.join("data").join("x.txt")).unwrap(), b"OLD");
        assert!(
            dst.join("data_副本").join("x.txt").exists(),
            "保留两者：应生成 _副本"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
