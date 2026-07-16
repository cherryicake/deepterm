use std::path::Path;
use std::process::Command;

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs;

use super::git_status::{DiffStat, GitStatus, StatusEntry, parse_numstat};

const REMOTE_GIT_TIMEOUT: u64 = 60;

#[derive(Clone)]
pub enum GitBackend {
    Local,
    Remote { config: SshConnectionConfig },
}

impl GitBackend {
    fn execute(&self, args: &[&str], cwd: &str) -> Result<String, String> {
        match self {
            GitBackend::Local => execute_local_git(args, cwd),
            GitBackend::Remote { config } => execute_remote_git(config, args, cwd),
        }
    }
}

/// Git repository operations
#[derive(Clone)]
pub struct GitRepository {
    root: String,
    backend: GitBackend,
}

impl GitRepository {
    /// Find the git repository root starting from the given path
    pub fn find(
        from: &Path,
        is_local: bool,
        ssh_config: Option<SshConnectionConfig>,
    ) -> Option<Self> {
        let from_str = from.to_string_lossy();
        if is_local {
            Self::find_local(&from_str)
        } else {
            let config = ssh_config?;
            Self::find_remote(&from_str, &config)
        }
    }

    fn find_local(from: &str) -> Option<Self> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(from)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(Self {
            root,
            backend: GitBackend::Local,
        })
    }

    fn find_remote(from: &str, config: &SshConnectionConfig) -> Option<Self> {
        let command = format!("cd {} && git rev-parse --show-toplevel", shell_quote(from));
        let result = sftp_fs::with_cached_session(config, |session| {
            session.execute_command(&command, REMOTE_GIT_TIMEOUT)
        })
        .ok()?;
        if !result.success() {
            log::warn!(
                "Remote path {} is not a git repository: {}",
                from,
                result.stderr.trim()
            );
            return None;
        }
        let root = result.stdout.trim().to_string();
        Some(Self {
            root,
            backend: GitBackend::Remote {
                config: config.clone(),
            },
        })
    }

    /// Get the repository root path
    pub fn root(&self) -> &str {
        &self.root
    }

    fn run_git(&self, args: &[&str]) -> Result<String, String> {
        self.backend.execute(args, &self.root)
    }

    /// Get all file statuses using `git status --porcelain=v1`
    pub fn status(&self) -> Result<Vec<StatusEntry>, String> {
        let stdout = self.run_git(&[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--no-renames",
            "-z",
            "--",
            ".",
        ])?;
        let git_status = GitStatus::parse(&stdout);
        Ok(git_status.entries.iter().cloned().collect())
    }

    /// Get diff stat for all files using `git diff --numstat`
    pub fn diff_stat(&self) -> Result<Vec<(String, DiffStat)>, String> {
        match self.run_git(&["diff", "--numstat", "--no-renames", "HEAD"]) {
            Ok(stdout) => Ok(parse_numstat(&stdout)),
            Err(_) => Ok(Vec::new()), // Might be initial commit with no HEAD
        }
    }

    /// Get the diff output for a specific file
    pub fn diff_file(&self, path: &str) -> Result<String, String> {
        let mut diff = self.run_git(&["diff", "--", path])?;

        let staged = self.run_git(&["diff", "--staged", "--", path]).unwrap_or_default();
        if !staged.is_empty() {
            if !diff.is_empty() {
                diff.push_str("\n");
            }
            diff.push_str(&staged);
        }

        // For untracked files, show the full file content as added
        if diff.is_empty() {
            if let Ok(content) = self.read_file(path) {
                diff = format!(
                    "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n"
                );
                for line in content.lines() {
                    diff.push_str(&format!("+{line}\n"));
                }
            }
        }

        Ok(diff)
    }

    fn read_file(&self, path: &str) -> Result<String, String> {
        match &self.backend {
            GitBackend::Local => {
                let file_path = Path::new(&self.root).join(path);
                std::fs::read_to_string(&file_path)
                    .map_err(|e| format!("Failed to read file: {e}"))
            }
            GitBackend::Remote { config } => {
                let remote_path = join_remote_path(&self.root, path);
                sftp_fs::with_cached_session(config, |session| {
                    session
                        .read_text_preview(&remote_path)
                        .map(|opt| opt.unwrap_or_default())
                })
                .map_err(|e| format!("Failed to read remote file: {e}"))
            }
        }
    }

    /// Stage files
    pub fn stage(&self, paths: &[&str]) -> Result<(), String> {
        let mut args = Vec::with_capacity(1 + paths.len());
        args.push("add");
        args.extend(paths.iter().copied());
        self.run_git(&args)?;
        Ok(())
    }

    /// Unstage files
    pub fn unstage(&self, paths: &[&str]) -> Result<(), String> {
        let mut args = Vec::with_capacity(2 + paths.len());
        args.push("reset");
        args.push("HEAD");
        args.push("--");
        args.extend(paths.iter().copied());
        self.run_git(&args)?;
        Ok(())
    }
}

fn execute_local_git(args: &[&str], cwd: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("Failed to run git {}: {e}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {stderr}", args.join(" ")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn execute_remote_git(
    config: &SshConnectionConfig,
    args: &[&str],
    cwd: &str,
) -> Result<String, String> {
    let mut command = format!("cd {} && git", shell_quote(cwd));
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }

    let result = sftp_fs::with_cached_session(config, |session| {
        session.execute_command(&command, REMOTE_GIT_TIMEOUT)
    })
    .map_err(|e| format!("Failed to run remote git: {e}"))?;

    if !result.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            result.stderr.trim()
        ));
    }

    Ok(result.stdout)
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let mut result = String::with_capacity(s.len() + 2);
    result.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            result.push_str("'\\''");
        } else {
            result.push(ch);
        }
    }
    result.push('\'');
    result
}

fn join_remote_path(root: &str, path: &str) -> String {
    let root = root.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if root.is_empty() {
        format!("/{}", path)
    } else {
        format!("{}/{}", root, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_join_remote_path() {
        assert_eq!(
            join_remote_path("/home/user/project", "src/main.rs"),
            "/home/user/project/src/main.rs"
        );
        assert_eq!(
            join_remote_path("/home/user/project/", "src/main.rs"),
            "/home/user/project/src/main.rs"
        );
        assert_eq!(
            join_remote_path("/home/user/project", "/src/main.rs"),
            "/home/user/project/src/main.rs"
        );
        assert_eq!(join_remote_path("/", "src/main.rs"), "/src/main.rs");
    }
}
