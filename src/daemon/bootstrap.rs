//! 定位、拉起并等待 `deepcore`。
//!
//! 这些逻辑原本住在 `client.rs` 里；`deepcore connect` 也需要同一套行为，
//! 所以抽到这里。GUI 不再直接连 socket，而是先启动 `deepcore connect`，
//! 由它负责让 core 就位并在 stdin/stdout 上转发协议字节。

use std::time::{Duration, Instant};

use super::transport::{self, Stream};

/// 拉起守护进程后最多等多久。
///
/// macOS launchd 的一次性交接包含 launchctl、fork、环境恢复与 socket 通知；
/// 繁忙机器上可能超过 3 秒。15 秒与 herdr 的启动等待保持一致。
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// 拉起后重试连接的间隔。
pub const CONNECT_RETRY: Duration = Duration::from_millis(50);

/// 守护进程的可执行文件名。
pub const DAEMON_BIN: &str = "deepcore";

/// 指定 `deepcore` 路径的环境变量（测试与自定义安装）。
pub const DAEMON_ENV: &str = "DEEPTERM_DEEPCORE";

/// 连接 core；没在跑就拉起来。
pub fn connect_to_core() -> std::io::Result<Stream> {
    let path = super::socket_path();
    if let Ok(stream) = transport::connect(&path) {
        return Ok(stream);
    }

    spawn_daemon()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match transport::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last_error = Some(e);
                std::thread::sleep(CONNECT_RETRY);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "守护进程未能在预期时间内启动")
    }))
}

/// 拉起守护进程。
///
/// # 怎么找到 `deepcore`
///
/// 1. `$DEEPTERM_DEEPCORE`（测试与自定义安装）；
/// 2. macOS：GUI 内的 `Resources/DeepTerm Core.app/Contents/MacOS/deepcore`；
/// 3. 与当前可执行文件**同目录**（开发构建与兼容安装）；
/// 4. `PATH`。
fn spawn_daemon() -> std::io::Result<()> {
    // 测试进程绝不能拉起守护进程。判断依据是测试二进制的固有属性，不依赖
    // 环境变量，因此没有竞态。
    #[cfg(test)]
    {
        log::warn!("测试进程不拉起守护进程");
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "测试进程禁止拉起守护进程",
        ))
    }

    #[cfg(not(test))]
    {
        let exe = daemon_binary_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "找不到 {DAEMON_BIN}（应与 {} 在同一目录，或在 PATH 里）",
                    DAEMON_BIN
                ),
            )
        })?;
        log::info!("拉起守护进程: {}", exe.display());
        #[cfg(target_os = "macos")]
        {
            if super::platform::spawn_core(&exe)? {
                return Ok(());
            }
            log::warn!("launchd 启动不可用，回退直接拉起 core");
        }
        #[cfg(windows)]
        {
            let mut command = std::process::Command::new(&exe);
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            return Ok(());
        }
        // 这一行不是死代码：Windows 上上面那个块已经 `return` 了，而
        // `#[cfg(unix)]` 在 Windows 构建里被整体删除。两种配置下函数都有
        // 一条可达的返回路径，不需要再补兜底分支。
        #[cfg(unix)]
        return super::launcher::spawn_detached(&exe, &[], "core");
    }
}

/// 定位 `deepcore` 可执行文件。
///
/// 抽成独立函数是为了可测：真正拉起进程的路径没法在测试里安全调用，而
/// "找不找得到"恰恰是最容易因为打包布局变化而坏掉的地方。
pub fn daemon_binary_path() -> Option<std::path::PathBuf> {
    if let Some(configured) = std::env::var_os(DAEMON_ENV).filter(|v| !v.is_empty()) {
        let path = std::path::PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }

    let name = format!("{DAEMON_BIN}{}", std::env::consts::EXE_SUFFIX);
    if let Ok(current) = std::env::current_exe() {
        #[cfg(target_os = "macos")]
        if let Some(bundled) = super::platform::gui_core_executable(&current) {
            return Some(bundled);
        }
        // 与当前可执行文件同目录（开发构建、旧安装与预期中的兼容路径）。
        let dir = current.parent()?;
        let sibling = dir.join(&name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    // PATH 兜底。
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配置了 `$DEEPTERM_DEEPCORE` 就必须用它。
    #[test]
    fn the_configured_daemon_binary_wins() {
        let dir = std::env::temp_dir().join(format!("deepcore-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let fake = dir.join(format!("{DAEMON_BIN}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&fake, b"#!/bin/sh\n").expect("write");

        let _guard = super::super::EnvGuard::set(DAEMON_ENV, &fake);
        assert_eq!(daemon_binary_path().as_deref(), Some(fake.as_path()));
        drop(_guard);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 配置指向不存在的文件时要忽略它，而不是返回一个坏路径。
    #[test]
    fn a_bogus_configured_path_is_ignored() {
        let _guard = super::super::EnvGuard::set(DAEMON_ENV, "/nonexistent/deepcore-xyz");
        let resolved = daemon_binary_path();
        assert_ne!(
            resolved.as_deref(),
            Some(std::path::Path::new("/nonexistent/deepcore-xyz")),
            "不存在的路径不该被采用"
        );
    }
}
