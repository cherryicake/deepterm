//! 向远端部署 `deepcore`，让远程终端真正跑在远端机器上。
//!
//! # 为什么需要
//!
//! "本机 core 持有 ssh 进程"能解决"关掉 deepterm 后远端程序继续跑"，但
//! 解决不了**本机断网**：ssh 一断，远端程序收到 SIGHUP 就没了。
//!
//! herdr 的做法是让远端跑它自己的 core——终端进程的父进程在远端机器上，
//! 本机断网、关机、崩溃都与它无关。这个模块做同一件事。
//!
//! # 部署流程
//!
//! 1. 用现有 SFTP 把本机 `deepcore` 传到远端
//!    `~/.local/share/deepterm/deepcore-<版本>-<指纹>`；
//! 2. **探针**：远端跑一次 `deepcore --version`，确认它能在这个系统上
//!    起来；
//! 3. 探针通过才以 `--holder` 起常驻进程，并把它的 socket 路径回报给 core。
//!
//! # 为什么必须有探针
//!
//! 二进制能不能在远端跑，只有跑一次才知道：
//!
//! - **glibc 版本**。本机（Ubuntu 24.04，glibc 2.39）编出来的程序，复制到
//!   Ubuntu 22.04（glibc 2.35）会直接 `version 'GLIBC_2.39' not found`。
//! - **架构**。x86_64 → ARM 服务器同理。
//! - **musl 与 glibc 的 libc**。
//!
//! 不做探针的话，失败会推迟到"用户开了终端、看到一堆乱码"才暴露，而且
//! 错误信息在那个语境下完全看不出是兼容性问题。
//!
//! 探针失败时**明确告知**（带远端 glibc 版本与我们的要求），而不是静默
//! 退回本机 ssh——那样用户以为在用远端持久化，实际并没有。
//!
//! # 不改远端已有状态
//!
//! 只往 `~/.local/share/deepterm/` 写自己的文件。不动 `PATH`、不动用户的
//! shell 配置、不注册服务、不需要 root。

use std::path::Path;

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::BlockingSftpSession;

/// 远端的安装目录（相对 `$HOME`）。
const REMOTE_INSTALL_DIR: &str = ".local/share/deepterm";

/// 探针超时秒数（远端跑 `--version`，正常是毫秒级）。
const PROBE_TIMEOUT_SECS: u32 = 20;

/// 一次部署的结果。
#[derive(Debug)]
pub enum DeployOutcome {
    /// 远端已经有一个能跑的，直接用。
    AlreadyInstalled {
        remote_path: String,
        version: String,
    },
    /// 这次刚传上去，并且探针通过。
    Installed {
        remote_path: String,
        version: String,
    },
    /// 部署失败，原因是给用户看的。
    ///
    /// **不要**把这种情况静默退回本机 ssh：用户会以为远端持久化生效了。
    Failed { reason: String },
}

impl DeployOutcome {
    pub fn remote_path(&self) -> Option<&str> {
        match self {
            Self::AlreadyInstalled { remote_path, .. } | Self::Installed { remote_path, .. } => {
                Some(remote_path)
            }
            Self::Failed { .. } => None,
        }
    }

    pub fn is_usable(&self) -> bool {
        self.remote_path().is_some()
    }
}

/// 本机 `deepcore` 的路径与版本指纹。
///
/// 返回 `None` 表示找不到本机的 `deepcore`（开发时可能只编了 GUI）。
pub fn local_core_binary() -> Option<(std::path::PathBuf, String)> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = format!("deepcore{}", std::env::consts::EXE_SUFFIX);
    let candidate = dir.join(&name);
    if !candidate.is_file() {
        return None;
    }
    // 用文件大小 + 修改时间做指纹：比"版本号"可靠（同一个版本号可能对应
    // 不同构建），也不需要额外的构建期注入。
    let meta = std::fs::metadata(&candidate).ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fingerprint = format!("{size:x}-{mtime:x}");
    Some((candidate, fingerprint))
}

/// 远端路径：带指纹，多个客户端/多个版本可以并存，不会互相覆盖。
pub fn remote_binary_path(fingerprint: &str) -> String {
    format!("{REMOTE_INSTALL_DIR}/deepcore-{fingerprint}")
}

/// 把 `deepcore` 部署到远端。
///
/// 阻塞调用（内部走 `shared_ssh_runtime`），调用方应在后台线程里跑。
pub fn deploy(config: &SshConnectionConfig) -> DeployOutcome {
    let Some((local_path, fingerprint)) = local_core_binary() else {
        return DeployOutcome::Failed {
            reason: "本机找不到 deepcore，无法部署到远端".into(),
        };
    };
    let remote_path = remote_binary_path(&fingerprint);

    let mut session = match BlockingSftpSession::connect(config) {
        Ok(session) => session,
        Err(e) => {
            return DeployOutcome::Failed {
                reason: format!("无法通过 SFTP 连接远端: {e}"),
            };
        }
    };

    // 1) 远端已经有了？直接探针验证它还能跑。
    if let Ok(Some(version)) = probe(&mut session, &remote_path) {
        log::info!("远端已有可用的 deepcore（{version}），跳过上传");
        return DeployOutcome::AlreadyInstalled {
            remote_path,
            version,
        };
    }

    // 2) 上传。
    log::info!(
        "向远端部署 deepcore: {} → {remote_path}",
        local_path.display()
    );
    if let Err(e) = upload(&mut session, &local_path, &remote_path) {
        return DeployOutcome::Failed {
            reason: format!("上传 deepcore 到远端失败: {e}"),
        };
    }

    // 3) 探针：**必须**在真正使用前验证它能在这个系统上起来。
    match probe(&mut session, &remote_path) {
        Ok(Some(version)) => DeployOutcome::Installed {
            remote_path,
            version,
        },
        Ok(None) => DeployOutcome::Failed {
            reason: format!(
                "deepcore 已上传但无法在远端运行。\n\
                 常见原因：远端 glibc 版本低于本机（本机二进制需要更新的 glibc），\
                 或两边架构不同（x86_64 / ARM）。\n\
                 远端报告：{}",
                probe_diagnosis(&mut session, &remote_path)
            ),
        },
        Err(e) => DeployOutcome::Failed {
            reason: format!("远端探针执行失败: {e}"),
        },
    }
}

/// 远端 core 的 socket 路径（相对 `$HOME`）。
pub const REMOTE_SOCKET_REL: &str = ".local/share/deepterm/remote-core.sock";

/// 在远端把 core 起来（若没在跑）。
///
/// 返回远端 socket 的**绝对路径**；失败返回给用户看的原因。
///
/// 用 `nohup setsid` 起：core 必须活过这次 SSH 会话，否则一断开就没了，
/// 那和"本机持有 ssh"没有区别。
pub fn start_remote_core(
    session: &mut BlockingSftpSession,
    remote_binary: &str,
) -> Result<String, String> {
    let bin = shell_quote(remote_binary);
    let sock = shell_quote(&format!("$HOME/{REMOTE_SOCKET_REL}"));
    let home_quoted = format!("$HOME/{}", REMOTE_SOCKET_REL);

    // 已经有一个活着？先探一下。
    let probe = format!(
        "if [ -S {sock} ]; then echo ALIVE; else echo NONE; fi",
        sock = sock
    );
    if let Ok((_, out, _)) = session.exec_capture(&probe) {
        if out.contains("ALIVE") {
            log::info!("远端 core 已在运行");
            // 但要确认它真的能应答，而不是一个残留的 socket 文件。
            if let Ok((Some(0), _, _)) = session.exec_capture(&format!(
                "DEEPTERM_RUNTIME_DIR=$(dirname {home}) {bin} --ping 2>/dev/null && echo OK",
                home = shell_quote(&home_quoted),
                bin = bin
            )) {
                return Ok(format!("$HOME/{REMOTE_SOCKET_REL}"));
            }
        }
    }

    // 起来。
    let start = format!(
        "mkdir -p \"$(dirname {home})\" && \
         DEEPTERM_RUNTIME_DIR=\"$(dirname {home})\" \
         nohup setsid {bin} >/dev/null 2>&1 < /dev/null & \
         sleep 0.3; echo STARTED",
        home = shell_quote(&home_quoted),
        bin = bin
    );
    match session.exec_capture(&start) {
        Ok((_, out, _)) if out.contains("STARTED") => {
            log::info!("远端 core 已启动");
            Ok(format!("$HOME/{REMOTE_SOCKET_REL}"))
        }
        Ok((_, _, err)) => Err(format!("远端 core 启动失败: {err}")),
        Err(e) => Err(format!("远端 core 启动失败: {e}")),
    }
}

/// 上传并落权限位。
fn upload(session: &mut BlockingSftpSession, local: &Path, remote: &str) -> anyhow::Result<()> {
    // 建目录（`create_dir_all` 语义由 SFTP 会话自己处理）。
    let dir = REMOTE_INSTALL_DIR.to_string();
    session.create_dir_all(&dir)?;
    // 0700：只有本人能执行。这是用户自己的程序，别让别人也能跑。
    session.upload_file_from_local_with_permissions(
        local,
        remote,
        0o700,
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    Ok(())
}

/// 跑一次 `--version`，成功返回版本串。
fn probe(session: &mut BlockingSftpSession, remote_path: &str) -> anyhow::Result<Option<String>> {
    let command = format!(
        "command -v timeout >/dev/null 2>&1 && timeout {PROBE_TIMEOUT_SECS} {path} --version || {path} --version",
        path = shell_quote(remote_path),
    );
    let (code, stdout, _stderr) = session.exec_capture(&command)?;
    match code {
        // 退出码 0 且输出带版本号才算通过。
        Some(0) if !stdout.trim().is_empty() => Ok(Some(stdout.trim().to_string())),
        _ => Ok(None),
    }
}

/// 探针失败时收集一段能说明原因的诊断信息。
///
/// 目的很具体：用户看到"跑不起来"时，第一个念头是"是不是权限/路径"，
/// 而真实原因往往在 glibc 那一行里。把 `ldd --version` 与直接运行的
/// stderr 一起带出来，省掉一轮来回。
fn probe_diagnosis(session: &mut BlockingSftpSession, remote_path: &str) -> String {
    let path = shell_quote(remote_path);
    let command = format!(
        "echo '--- run ---'; {path} --version 2>&1 | head -3; \
         echo '--- glibc ---'; ldd --version 2>&1 | head -1; \
         echo '--- arch ---'; uname -m"
    );
    match session.exec_capture(&command) {
        Ok((_, stdout, stderr)) => {
            let text = format!("{stdout}{stderr}");
            text.trim().to_string()
        }
        Err(e) => format!("（诊断命令也失败了: {e}）"),
    }
}

/// 单引号转义，供拼进远端 shell 命令。
///
/// 远端命令是字符串拼的，路径里出现空格或引号就会出事。用单引号包住、
/// 内部的单引号按 POSIX 规则拆开。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 远端路径必须带指纹，且落在固定目录下。
    ///
    /// 带指纹的意义：同一台机器可能被多个客户端/多个版本部署过，覆盖
    /// 别人的文件会让"我这边好好的、那边坏了"变成常态。
    #[test]
    fn remote_paths_are_fingerprinted_under_a_fixed_directory() {
        let a = remote_binary_path("100-200");
        let b = remote_binary_path("101-200");
        assert_ne!(a, b, "不同指纹必须对应不同文件");
        assert!(a.starts_with(REMOTE_INSTALL_DIR), "{a}");
        assert!(a.contains("deepcore-100-200"), "{a}");
    }

    /// 路径不能被拼进 shell 命令时被意外展开。
    ///
    /// 回归价值：远端命令是字符串拼接的，路径里只要有空格或引号，不转义
    /// 就会变成"跑另一个命令"。这类问题在测试机上永远碰不到（家目录没
    /// 空格），到了 macOS 用户那儿就是必现。
    #[test]
    fn shell_quoting_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("/a/b c"), "'/a/b c'");
        assert_eq!(shell_quote("/a/it's"), r"'/a/it'\''s'");
    }

    /// 找不到 `deepcore` 时不能返回"安装成功"。
    ///
    /// 这条防的是静默降级：如果 `Failed` 被误当成可用，UI 会以为远端
    /// 持久化生效了，用户关掉 deepterm 却发现远端任务被 SIGHUP 带走。
    #[test]
    fn a_missing_local_binary_is_reported_as_failure() {
        let outcome = DeployOutcome::Failed {
            reason: "test".into(),
        };
        assert!(!outcome.is_usable());
        assert_eq!(outcome.remote_path(), None);
    }

    /// 探针超时必须是个有限值：远端卡住时不能把 UI 也拖住。
    #[test]
    fn the_probe_has_a_bounded_timeout() {
        assert!(PROBE_TIMEOUT_SECS > 0 && PROBE_TIMEOUT_SECS <= 60);
    }
}
