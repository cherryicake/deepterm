//! The independently identified DeepTerm Core.app bundle.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const APP_NAME: &str = "DeepTerm Core.app";
pub const EXECUTABLE: &str = "Contents/MacOS/deepcore";
pub const IDENTIFIER: &str = "dev.deepterm.core";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

pub fn bundle_root(path: &Path) -> Option<&Path> {
    let root = path.parent()?.parent()?.parent()?;
    (path.strip_prefix(root).ok()? == Path::new(EXECUTABLE) && root.file_name()? == APP_NAME)
        .then_some(root)
}

pub fn source_is_bundle(path: &Path) -> bool {
    bundle_root(path).is_some()
}

pub fn register(bundle: &Path) {
    if !bundle.is_dir() {
        return;
    }
    if let Err(error) = Command::new(LSREGISTER).arg("-f").arg(bundle).output()
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("无法向 LaunchServices 注册 {}: {error}", bundle.display());
    } else {
        log::debug!(
            "已向 LaunchServices 注册 {IDENTIFIER}: {}",
            bundle.display()
        );
    }
}

/// Keep a symlinked/core path inside the same physical bundle.
pub fn core_executable(exe: &Path) -> PathBuf {
    let canonical = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    if source_is_bundle(&canonical) {
        canonical
    } else {
        exe.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_bundle_executable_has_a_bundle_root() {
        assert_eq!(
            bundle_root(Path::new(
                "/share/DeepTerm Core.app/Contents/MacOS/deepcore"
            )),
            Some(Path::new("/share/DeepTerm Core.app"))
        );
        for path in [
            "/bin/deepcore",
            "/x.app/Contents/MacOS/deepcore",
            "/DeepTerm Core.app/Contents/MacOS/core",
        ] {
            assert!(bundle_root(Path::new(path)).is_none(), "{path}");
        }
    }
}
