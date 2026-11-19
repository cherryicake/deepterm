//! 让 core / holder 脱离调用方会话启动。
//!
//! Linux 上有 systemd user manager 时优先 `systemd-run --user`：进程归用户
//! manager 托管，配合 linger 可以活到 SSH 登出之后。没有 user manager 时
//! 退回 `setsid`，仍然能活过拉起它的 core 或 SSH 会话结束。

use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
use std::ffi::OsString;

#[cfg(target_os = "linux")]
const ENV_NAMES: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "DISPLAY",
    "WAYLAND_DISPLAY",
];

/// 启动一个不需要 stdin/stdout 的长生命周期进程。
pub fn spawn_detached(exe: &Path, args: &[&str], role: &str) -> std::io::Result<()> {
    #[cfg(not(target_os = "linux"))]
    let _ = role;
    #[cfg(target_os = "linux")]
    if user_manager_runtime().is_some() {
        match spawn_with_systemd(exe, args, role) {
            Ok(()) => return Ok(()),
            Err(e) => log::warn!("systemd-run 启动 {role} 失败，回退 setsid: {e}"),
        }
    }
    spawn_with_setsid(exe, args)
}

/// macOS only: direct fallback carrying the launchd bypass marker.
///
/// The marker is scoped to the core's immediate parent. Without it a failed
/// launchd job causes the fallback core to enter launchd again, recursively.
#[cfg(target_os = "macos")]
pub fn spawn_detached_with_env(
    exe: &Path,
    args: &[&str],
    env: &[(&str, std::ffi::OsString)],
) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    // SAFETY: setsid runs in the forked child before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map(|_| ())
}

fn spawn_with_setsid(exe: &Path, args: &[&str]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid 在 fork 后的子进程中调用，失败时返回 -1。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map(|_| ())
}

#[cfg(target_os = "linux")]
fn user_manager_runtime() -> Option<std::path::PathBuf> {
    // SAFETY: `getuid` 只返回当前进程的真实 uid，无参数、不访问内存，不会失败。
    let uid = unsafe { libc::getuid() };
    let expected = std::path::PathBuf::from(format!("/run/user/{uid}"));
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| expected.clone());
    if runtime != expected || !runtime.join("systemd/private").exists() {
        return None;
    }
    Some(runtime)
}

#[cfg(target_os = "linux")]
fn spawn_with_systemd(exe: &Path, args: &[&str], role: &str) -> std::io::Result<()> {
    let runtime = user_manager_runtime().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "没有 systemd user manager")
    })?;
    let unit = format!(
        "deepterm-{role}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );

    let mut command = Command::new("systemd-run");
    command.args(systemd_run_args(
        &unit,
        exe,
        args,
        &propagated_env(),
        &runtime,
    ));

    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(std::io::Error::other(format!(
        "systemd-run 退出码 {:?}: {}",
        output.status.code(),
        stderr.trim()
    )))
}

#[cfg(target_os = "linux")]
fn propagated_env() -> Vec<(OsString, OsString)> {
    let mut values = Vec::new();
    for (name, value) in std::env::vars_os() {
        let Some(name_text) = name.to_str() else {
            continue;
        };
        if ENV_NAMES.contains(&name_text) || name_text.starts_with("DEEPTERM_") {
            values.push((name, value));
        }
    }
    values
}

#[cfg(target_os = "linux")]
fn systemd_run_args(
    unit: &str,
    exe: &Path,
    args: &[&str],
    env: &[(OsString, OsString)],
    runtime: &Path,
) -> Vec<OsString> {
    let mut command_args = vec![
        OsString::from("--user"),
        OsString::from("--quiet"),
        OsString::from("--collect"),
        OsString::from("--no-ask-password"),
        OsString::from("--property=KillMode=process"),
        OsString::from("--property=StandardInput=null"),
        OsString::from("--property=StandardOutput=null"),
        OsString::from("--property=StandardError=null"),
        OsString::from(format!("--unit={unit}")),
    ];
    for (name, value) in env {
        let mut assignment = OsString::from("--setenv=");
        assignment.push(name);
        assignment.push("=");
        assignment.push(value);
        command_args.push(assignment);
    }
    if !env.iter().any(|(name, _)| name == "XDG_RUNTIME_DIR") {
        let mut assignment = OsString::from("--setenv=XDG_RUNTIME_DIR=");
        assignment.push(runtime.as_os_str());
        command_args.push(assignment);
    }
    command_args.push(OsString::from("--"));
    command_args.push(exe.as_os_str().to_owned());
    command_args.extend(args.iter().map(OsString::from));
    command_args
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::*;
    #[cfg(target_os = "linux")]
    use std::ffi::OsStr;

    #[test]
    #[cfg(target_os = "linux")]
    fn systemd_run_args_keep_the_service_process_alive_without_dbus_assets() {
        let args = systemd_run_args(
            "deepterm-holder-1",
            Path::new("/opt/deepcore"),
            &[super::super::holder::HOLDER_FLAG],
            &[(OsString::from("HOME"), OsString::from("/home/tester"))],
            Path::new("/run/user/1000"),
        );
        assert!(args.contains(&OsString::from("--user")));
        assert!(args.contains(&OsString::from("--property=KillMode=process")));
        assert!(args.contains(&OsString::from("--setenv=HOME=/home/tester")));
        assert!(args.contains(&OsString::from("--setenv=XDG_RUNTIME_DIR=/run/user/1000")));
        let separator = args
            .iter()
            .position(|arg| arg == OsStr::new("--"))
            .expect("systemd-run 需要 -- 分隔符");
        assert_eq!(
            &args[separator + 1..],
            &[
                OsString::from("/opt/deepcore"),
                OsString::from(super::super::holder::HOLDER_FLAG),
            ]
        );
    }
}
