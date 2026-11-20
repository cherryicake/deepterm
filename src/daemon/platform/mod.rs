//! 与 herdr-core 对齐的 macOS 进程身份处理。
//!
//! GUI 的本地网络授权不会自动转给 core。core 必须是独立 app bundle，并由
//! launchd 放进自己的 coalition；core 与 holder 再通过
//! `responsibility_spawnattrs_setdisclaim` 建立自己的隐私责任身份。

#[cfg(target_os = "macos")]
mod macos_appkit;
#[cfg(target_os = "macos")]
mod macos_bundle;
#[cfg(target_os = "macos")]
mod macos_launchd;
#[cfg(target_os = "macos")]
mod macos_responsibility;
#[cfg(target_os = "macos")]
mod macos_session;

use std::path::{Path, PathBuf};

/// Run before logging, threads or the async runtime.
///
/// Returns true when this process was launched through the private launchd
/// handoff and should continue as the core.
pub fn daemon_startup() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_responsibility::holder_startup();
        macos_launchd::startup()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Move a directly launched GUI core into launchd before it serves.
pub fn prepare_core() {
    #[cfg(target_os = "macos")]
    macos_launchd::prepare_core(&core_log_path());
}

/// Complete holder registration before creating PTYs or threads.
pub fn prepare_holder() {
    #[cfg(target_os = "macos")]
    if let Err(error) = macos_appkit::check_in() {
        log::warn!("holder 的后台应用注册不可用: {error}");
    }
}

/// Start a core under launchd.
///
/// Returns false when the caller should use the direct detached fallback. The
/// command carries the `DEEPTERM_LAUNCHD_DIRECT` marker so the fallback core
/// does not retry launchd in a loop.
pub fn spawn_core(exe: &Path) -> std::io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        if macos_launchd::must_skip() {
            return Ok(false);
        }
        if let Some(bundle) = macos_bundle::bundle_root(exe) {
            macos_bundle::register(bundle);
        }
        let mut cmd = std::process::Command::new(exe);
        if macos_launchd::spawn(&mut cmd, &core_log_path()) {
            return Ok(true);
        }
        // Herdr passes the same `Command` to its direct fallback, preserving
        // the bypass marker. Our detached launcher constructs a new command,
        // so copy that part explicitly.
        crate::daemon::launcher::spawn_detached_with_env(
            exe,
            &[],
            &[(macos_launchd::DIRECT, std::process::id().to_string().into())],
        )?;
        Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = exe;
        Ok(false)
    }
}

/// Resolve the independently bundled core executable for this process.
pub fn core_executable(exe: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        macos_bundle::core_executable(exe)
    }
    #[cfg(not(target_os = "macos"))]
    {
        exe.to_path_buf()
    }
}

/// Preferred independently bundled core for a GUI executable.
pub fn gui_core_executable(gui: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let contents = gui.parent()?.parent()?;
        let core = contents
            .join("Resources")
            .join(macos_bundle::APP_NAME)
            .join(macos_bundle::EXECUTABLE);
        core.is_file().then_some(core)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = gui;
        None
    }
}

/// GUI 内嵌的 holder 可执行文件（`DeepHolder.app`）。
///
/// `core_holder::spawn_holder` 用它拉起 holder。找不到时返回 `None`，调用方
/// 退回 **core 自己的路径** —— 那是同一份二进制，功能上仍然正确（只是活动
/// 监视器里显示成 `DeepTerm Core`），因此这里失败不该阻止终端启动。
pub fn gui_holder_executable(core: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // 只对“确实跑在 DeepTerm Core.app 里”的 core 推导兄弟 bundle：
        // 开发构建的 `target/debug/deepcore` 往上三级只是 target 目录，
        // 按它拼出来的路径必然不存在，白白制造一次 stat。
        let canonical = core.canonicalize().unwrap_or_else(|_| core.to_path_buf());
        let root = macos_bundle::bundle_root(&canonical)?;
        let resources = root.parent()?;
        let holder = resources
            .join(macos_bundle::HOLDER_APP_NAME)
            .join(macos_bundle::HOLDER_EXECUTABLE);
        holder.is_file().then_some(holder)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = core;
        None
    }
}

pub fn core_log_path() -> PathBuf {
    crate::utils::logging::core_log_path()
        .unwrap_or_else(|| crate::daemon::runtime_dir().join("deepcore.log"))
}
