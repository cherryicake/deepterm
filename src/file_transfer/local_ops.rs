use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};

use crate::file_transfer::utils::{
    check_cancelled, copy_local_file_interruptible, is_local_noop_move,
    prevent_local_recursive_copy, remove_local_source, resolve_local_conflict, temporary_local_path,
    TransferSummary,
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
    if is_local_noop_move(source_path, destination_dir) {
        return Ok(TransferSummary::default());
    }
    let metadata = fs::metadata(source_path)
        .with_context(|| format!("无法检查源文件 {}", source_path.display()))?;
    fs::create_dir_all(destination_dir)?;
    let file_name = super::utils::local_file_name(source_path)?;
    let effective_name = rename_to.unwrap_or(&file_name);
    let desired_path = destination_dir.join(effective_name);

    if metadata.is_file() {
        let destination_path =
            resolve_local_conflict(&desired_path, conflict_policy, resolver)?
                .ok_or_else(|| anyhow!("传输已跳过"))?;
        if source_path == destination_path {
            return Ok(TransferSummary::from_metadata(&metadata));
        }
        return copy_local_file_to_path(source_path, &destination_path, cancelled);
    }

    if metadata.is_dir() {
        let desired_path = destination_dir.join(&file_name);
        let destination_root =
            resolve_local_conflict(&desired_path, conflict_policy, resolver)?
                .ok_or_else(|| anyhow!("传输已跳过"))?;
        if source_path == destination_root {
            return Ok(TransferSummary::from_metadata(&metadata));
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

    bail!("不支持的本地文件类型: {}", source_path.display())
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
    let destination_path = resolve_local_conflict(
        &destination_dir.join(effective_name),
        conflict_policy,
        resolver,
    )?
    .ok_or_else(|| anyhow!("传输已跳过"))?;

    if source_path == destination_path {
        return Ok(TransferSummary::from_metadata(&metadata));
    }

    if metadata.is_dir() {
        prevent_local_recursive_copy(source_path, &destination_path)?;
    }

    match fs::rename(source_path, &destination_path) {
        Ok(()) => Ok(TransferSummary::from_metadata(&metadata)),
        Err(_) => {
            let summary = copy_local_to_local(
                source_path,
                destination_dir,
                conflict_policy,
                rename_to,
                cancelled,
                resolver,
            )?;
            remove_local_source(source_path)?;
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
            let destination_path =
                resolve_local_conflict(&destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("传输已跳过"))?;
            summary.merge(copy_local_file_to_path(
                &source_path,
                &destination_path,
                cancelled,
            )?);
        } else if metadata.is_dir() {
            let destination_path =
                resolve_local_conflict(&destination_path, conflict_policy, resolver)?
                    .ok_or_else(|| anyhow!("传输已跳过"))?;
            summary.merge(copy_local_dir_to_path(
                &source_path,
                &destination_path,
                conflict_policy,
                cancelled,
                resolver,
            )?);
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
