//! 两个独立 app bundle：`DeepTerm Core.app` 与 `DeepHolder.app`。
//!
//! # 为什么要拆成两个 bundle
//!
//! `deepcore` 这**一个**可执行文件按参数分成三个角色：core、holder、
//! `connect` 桥。holder 会调 `NSApplication.finishLaunching` 注册成 accessory
//! app（见 `macos_appkit`），注册之后活动监视器显示的是 **bundle 的
//! `CFBundleDisplayName`**，而不是可执行文件名。两者共用一个 bundle 时，
//! core 和 holder 会显示成同一个名字，排查“谁在持 PTY”只能靠 `ps` 参数。
//!
//! 拆成两个 bundle 后：
//!
//! - core 走 launchd 交接、**不**注册 AppKit → 显示可执行名 `deepcore`；
//! - holder 在 `DeepHolder.app` 里注册 → 显示 `DeepHolder`。
//!
//! # 两份二进制必须来自同一次构建
//!
//! `identity::build_id()` 在 macOS 上取的是 `LC_UUID`（签名不改变它），
//! 所以两份副本只要来自同一次构建就天然一致；但打包时仍要断言，否则一旦
//! 有人只替换其中一份，新 holder 会被判成旧构建并立刻 drain。

use std::path::{Path, PathBuf};
use std::process::Command;

/// core 的 bundle（GUI 内嵌，也是远端部署的落点）。
pub const APP_NAME: &str = "DeepTerm Core.app";
/// core 的可执行文件（相对 bundle 根）。
pub const EXECUTABLE: &str = "Contents/MacOS/deepcore";
/// core 的 bundle identifier，TCC 授权按它持久化。
pub const IDENTIFIER: &str = "dev.deepterm.core";

/// holder 的 bundle。与 core 并排放在 GUI 的 `Contents/Resources/` 下。
pub const HOLDER_APP_NAME: &str = "DeepHolder.app";
/// holder 的可执行文件（相对 bundle 根）。
pub const HOLDER_EXECUTABLE: &str = "Contents/MacOS/deepholder";
/// holder 的 bundle identifier，与 core 分开以便各自独立签名。
pub const HOLDER_IDENTIFIER: &str = "dev.deepterm.holder";

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// 一组 bundle 的识别规则。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleSpec {
    pub app: &'static str,
    pub executable: &'static str,
    pub identifier: &'static str,
}

pub const CORE_BUNDLE: BundleSpec = BundleSpec {
    app: APP_NAME,
    executable: EXECUTABLE,
    identifier: IDENTIFIER,
};

pub const HOLDER_BUNDLE: BundleSpec = BundleSpec {
    app: HOLDER_APP_NAME,
    executable: HOLDER_EXECUTABLE,
    identifier: HOLDER_IDENTIFIER,
};

pub const BUNDLES: [BundleSpec; 2] = [CORE_BUNDLE, HOLDER_BUNDLE];

/// 路径属于哪个 bundle，返回该 bundle 的根目录。
///
/// 只认**精确**映射：`<root>/Contents/MacOS/<spec.executable>` 且
/// `root` 的名字恰为 `spec.app`。放行任意 `*.app` 会让伪装的 bundle 继承
/// 我们的 TCC 授权。
pub fn bundle_root(path: &Path) -> Option<&Path> {
    let root = path.parent()?.parent()?.parent()?;
    let relative = path.strip_prefix(root).ok()?;
    let name = root.file_name()?;
    BUNDLES
        .iter()
        .find(|spec| relative == Path::new(spec.executable) && name == spec.app)
        .map(|_| root)
}

pub fn source_is_bundle(path: &Path) -> bool {
    bundle_root(path).is_some()
}

/// holder 是否运行在自己的 bundle 里。
///
/// 打包自检用：分发后 holder 必须能在活动监视器里显示 `DeepHolder`，
/// 若它还跑在 core 的 bundle 里，说明打包漏了一份二进制。
pub fn holder_has_own_bundle(path: &Path) -> bool {
    bundle_root(path).is_some_and(|root| root.file_name() == Some(HOLDER_APP_NAME.as_ref()))
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
            "已向 LaunchServices 注册 {}",
            bundle.file_name().unwrap_or_default().to_string_lossy()
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
        assert_eq!(
            bundle_root(Path::new("/share/DeepHolder.app/Contents/MacOS/deepholder")),
            Some(Path::new("/share/DeepHolder.app"))
        );
        for path in [
            "/bin/deepcore",
            "/x.app/Contents/MacOS/deepcore",
            "/DeepTerm Core.app/Contents/MacOS/core",
            // 交叉：core 的名字放进 holder bundle、holder 的名字放进 core bundle
            "/DeepHolder.app/Contents/MacOS/deepcore",
            "/DeepTerm Core.app/Contents/MacOS/deepholder",
            // 名字像但不是的 bundle
            "/NotDeepHolder.app/Contents/MacOS/deepholder",
        ] {
            assert!(bundle_root(Path::new(path)).is_none(), "{path}");
        }
    }

    /// 两个 bundle 的 identifier 必须不同：相同则无法分别签名，
    /// TCC 授权也会互相覆盖。
    #[test]
    fn the_two_bundles_have_distinct_identifiers() {
        assert_ne!(CORE_BUNDLE.identifier, HOLDER_BUNDLE.identifier);
        assert_ne!(CORE_BUNDLE.app, HOLDER_BUNDLE.app);
    }

    #[test]
    fn holder_only_counts_its_own_bundle() {
        assert!(holder_has_own_bundle(Path::new(
            "/share/DeepHolder.app/Contents/MacOS/deepholder"
        )));
        assert!(!holder_has_own_bundle(Path::new(
            "/share/DeepTerm Core.app/Contents/MacOS/deepcore"
        )));
    }
}
