use std::path::{Path, PathBuf};

use crate::ssh::SshConnectionConfig;

/// 本地路径解析上下文
pub struct LocalPathContext {
    pub working_directory: Option<PathBuf>,
    pub process_cwd: Option<PathBuf>,
}

/// 远程路径解析上下文
pub struct RemotePathContext {
    pub ssh_config: SshConnectionConfig,
    pub current_remote_working_directory: Option<String>,
}

/// 路径解析器
///
/// 将用户输入的原始路径解析为绝对路径或远程路径。
pub struct PathResolver;

/// 词法规范化：去掉 `.` 组件、重复分隔符与尾部斜杠，保留 `..`。
/// 输入为空（如路径仅剩 `./`）时退化为 `.`，避免产出空路径。
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                // push 绝对组件会重置路径；根目录只可能是第一个组件，行为正确
                out.push(comp);
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

impl PathResolver {
    /// 解析本地路径
    ///
    /// 结果会做词法规范化：去掉 `.` 组件与重复分隔符（`/a/./b` → `/a/b`），
    /// 并去掉尾部斜杠。这样下游的相等性判断（如 `TransferRequest::is_noop`）
    /// 不会被同一目录的多种写法骗过。
    /// `..` 不做词法折叠：遇到符号链接时词法折叠会指向错误位置，
    /// 交给文件系统在打开时解析。
    pub fn resolve_local(raw_path: &str, ctx: &LocalPathContext) -> PathBuf {
        let trimmed = raw_path.trim().trim_matches(|c| matches!(c, '"' | '\''));
        let expanded = if trimmed == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from(trimmed))
        } else if let Some(stripped) = trimmed.strip_prefix("~/") {
            dirs::home_dir()
                .map(|home| home.join(stripped))
                .unwrap_or_else(|| PathBuf::from(trimmed))
        } else {
            PathBuf::from(trimmed)
        };

        let resolved = if expanded.is_absolute() {
            expanded
        } else {
            ctx.process_cwd
                .clone()
                .or_else(|| ctx.working_directory.clone())
                .or_else(dirs::home_dir)
                .map(|cwd| cwd.join(&expanded))
                .unwrap_or(expanded)
        };
        normalize_lexical(&resolved)
    }

    /// 解析远程路径
    pub fn resolve_remote(raw_path: &str, cwd: Option<&str>) -> Option<String> {
        let trimmed = raw_path.trim().trim_matches(|c| matches!(c, '"' | '\''));
        if trimmed.is_empty() || trimmed == "~" {
            None
        } else if let Some(stripped) = trimmed.strip_prefix("~/") {
            Some(format!("./{}", stripped))
        } else if trimmed.starts_with('/') {
            Some(trimmed.to_string())
        } else {
            cwd.map(|cwd| crate::ssh::sftp_fs::join_remote_path(cwd, trimmed))
                .or_else(|| Some(trimmed.to_string()))
        }
    }

    /// 解析远程工作目录（从窗口标题）
    pub fn parse_remote_working_directory_from_title(title: &str) -> Option<String> {
        let title = title.trim();
        if title.is_empty() {
            return None;
        }

        let mut candidates = vec![title];
        if let Some((_, suffix)) = title.rsplit_once(": ") {
            candidates.push(suffix.trim());
        }
        if let Some((_, suffix)) = title.rsplit_once(':') {
            candidates.push(suffix.trim());
        }
        if let Some(index) = title.rfind(" /") {
            candidates.push(title[index + 1..].trim());
        }
        if let Some(index) = title.rfind("~/") {
            candidates.push(title[index..].trim());
        }
        if title.ends_with('~') {
            candidates.push("~");
        }

        for candidate in candidates {
            if let Some(path) = Self::normalize_remote_title_path(candidate) {
                return Some(path);
            }
        }

        title.find('/').and_then(|index| {
            let candidate = title[index..].trim();
            Self::normalize_remote_title_path(candidate)
        })
    }

    fn normalize_remote_title_path(candidate: &str) -> Option<String> {
        let candidate = candidate.trim().trim_matches(|c| matches!(c, '"' | '\''));
        if candidate.is_empty() {
            return None;
        }

        if candidate.starts_with('/') {
            return Some(candidate.to_string());
        }

        if candidate == "~" {
            return None;
        }

        candidate
            .strip_prefix("~/")
            .map(|stripped| format!("./{}", stripped))
    }

    /// 查询进程工作目录
    #[cfg(target_os = "linux")]
    pub fn query_process_working_directory(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    #[cfg(target_os = "macos")]
    pub fn query_process_working_directory(pid: u32) -> Option<PathBuf> {
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"])
            .output()
            .map_err(|error| {
                log::warn!("Failed to run lsof for cwd query: {error}");
                error
            })
            .ok()?;
        if !output.status.success() {
            log::warn!(
                "lsof cwd query failed for pid {pid}: status {}",
                output.status
            );
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.strip_prefix('n').map(PathBuf::from))
    }

    /// Windows：读目标进程的 PEB 取 `CurrentDirectory`。
    ///
    /// Windows 没有 lsof 也没有 `/proc`，唯一途径是读 PEB（见
    /// [`crate::utils::win_process_cwd`]）。查不到（进程已退出 / 提权 / 跨用户）
    /// 返回 `None`，与 unix 分支的失败语义一致。
    #[cfg(windows)]
    pub fn query_process_working_directory(pid: u32) -> Option<PathBuf> {
        crate::utils::win_process_cwd::query_process_working_directory(pid)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    pub fn query_process_working_directory(_pid: u32) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_local_absolute() {
        let ctx = LocalPathContext {
            working_directory: Some(PathBuf::from("/home/user")),
            process_cwd: None,
        };
        assert_eq!(
            PathResolver::resolve_local("/etc/hosts", &ctx),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn test_resolve_local_relative() {
        let ctx = LocalPathContext {
            working_directory: Some(PathBuf::from("/home/user")),
            process_cwd: None,
        };
        assert_eq!(
            PathResolver::resolve_local("src/main.rs", &ctx),
            PathBuf::from("/home/user/src/main.rs")
        );
    }

    #[test]
    fn test_resolve_local_home() {
        let home = dirs::home_dir().unwrap();
        let ctx = LocalPathContext {
            working_directory: None,
            process_cwd: None,
        };
        assert_eq!(
            PathResolver::resolve_local("~/.bashrc", &ctx),
            home.join(".bashrc")
        );
    }

    #[test]
    fn test_resolve_remote_absolute() {
        assert_eq!(
            PathResolver::resolve_remote("/var/log", None),
            Some("/var/log".to_string())
        );
    }

    #[test]
    fn test_resolve_remote_relative() {
        assert_eq!(
            PathResolver::resolve_remote("src/main.rs", Some("/home/user")),
            Some("/home/user/src/main.rs".to_string())
        );
    }

    #[test]
    fn test_parse_remote_title_path() {
        assert_eq!(
            PathResolver::parse_remote_working_directory_from_title("user@host: /home/user"),
            Some("/home/user".to_string())
        );
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    fn ctx(cwd: &str) -> LocalPathContext {
        LocalPathContext {
            working_directory: Some(PathBuf::from(cwd)),
            process_cwd: None,
        }
    }

    #[test]
    fn resolves_and_normalizes_dot_components() {
        assert_eq!(
            PathResolver::resolve_local("./Cargo.toml", &ctx("/home/user/project")),
            PathBuf::from("/home/user/project/Cargo.toml")
        );
    }

    #[test]
    fn normalizes_trailing_slash_and_double_slashes() {
        assert_eq!(
            PathResolver::resolve_local("/tmp//a.txt", &ctx("/cwd")),
            PathBuf::from("/tmp/a.txt")
        );
        assert_eq!(
            PathResolver::resolve_local("/tmp/dir/", &ctx("/cwd")),
            PathBuf::from("/tmp/dir")
        );
    }

    #[test]
    fn keeps_parent_dir_components() {
        // `..` 不做词法折叠（符号链接语义下折叠会指错位置）
        assert_eq!(
            PathResolver::resolve_local("../a.txt", &ctx("/home/user/project")),
            PathBuf::from("/home/user/project/../a.txt")
        );
    }

    #[test]
    fn lone_dot_falls_back_to_cwd() {
        assert_eq!(
            PathResolver::resolve_local(".", &ctx("/home/user/project")),
            PathBuf::from("/home/user/project")
        );
    }
}
