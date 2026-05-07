use std::path::PathBuf;

use crate::file_browser::FileBrowserTarget;
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

impl PathResolver {
    /// 解析本地路径
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

        if expanded.is_absolute() {
            expanded
        } else {
            ctx.process_cwd
                .clone()
                .or_else(|| ctx.working_directory.clone())
                .or_else(dirs::home_dir)
                .map(|cwd| cwd.join(&expanded))
                .unwrap_or(expanded)
        }
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

    /// 构建文件浏览器目标
    pub fn build_file_browser_target(
        argument: Option<&str>,
        is_remote: bool,
        local_ctx: Option<LocalPathContext>,
        remote_ctx: Option<RemotePathContext>,
    ) -> FileBrowserTarget {
        if is_remote {
            let ctx = remote_ctx.expect("remote context required for remote terminal");
            let requested_path = argument.and_then(|raw| {
                Self::resolve_remote(raw, ctx.current_remote_working_directory.as_deref())
            });

            FileBrowserTarget::Remote {
                ssh_config: ctx.ssh_config,
                requested_path: requested_path.or(ctx.current_remote_working_directory),
            }
        } else {
            let ctx = local_ctx.expect("local context required for local terminal");
            let requested_path = argument.map(|raw| Self::resolve_local(raw, &ctx));

            FileBrowserTarget::Local {
                requested_path: requested_path
                    .or_else(|| ctx.process_cwd.clone())
                    .or_else(|| ctx.working_directory.clone())
                    .or_else(dirs::home_dir)
                    .or(Some(PathBuf::from("/"))),
            }
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
        let output = std::process::Command::new("lsof")
            .args(["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.strip_prefix('n').map(PathBuf::from))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
