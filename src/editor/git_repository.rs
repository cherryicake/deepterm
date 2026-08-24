use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use parking_lot::Mutex;
use std::sync::OnceLock;

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs;

use super::git_status::{DiffStat, GitStatus, StatusEntry, parse_numstat};

const REMOTE_GIT_TIMEOUT: u64 = 60;

/// 远程 diff 内容缓存条目：以 (HEAD commit, 文件 size, mtime) 为有效性指纹。
struct CachedRemoteDiff {
    head_commit: String,
    size: u64,
    mtime: Option<std::time::SystemTime>,
    contents: (String, String),
}

/// 键 = "repo_root\u{0}path"。进程级缓存，容量上限防膨胀。
static REMOTE_DIFF_CACHE: OnceLock<Mutex<HashMap<String, CachedRemoteDiff>>> = OnceLock::new();
const REMOTE_DIFF_CACHE_CAP: usize = 64;

fn remote_diff_cache() -> &'static Mutex<HashMap<String, CachedRemoteDiff>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedRemoteDiff>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remote_diff_cache_get(
    key: &str,
    head_commit: &str,
    size: u64,
    mtime: Option<std::time::SystemTime>,
) -> Option<(String, String)> {
    let cache = remote_diff_cache().lock();
    let entry = cache.get(key)?;
    (entry.head_commit == head_commit && entry.size == size && entry.mtime == mtime)
        .then(|| entry.contents.clone())
}

fn remote_diff_cache_insert(
    key: String,
    head_commit: String,
    size: u64,
    mtime: Option<std::time::SystemTime>,
    contents: (String, String),
) {
    let mut cache = remote_diff_cache().lock();
    // 简单容量控制：满时清空（diff 场景键数有限，粗暴策略足够）
    if cache.len() >= REMOTE_DIFF_CACHE_CAP && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(
        key,
        CachedRemoteDiff {
            head_commit,
            size,
            mtime,
            contents,
        },
    );
}

/// 使某远程仓库的全部 diff 缓存失效（写入文件后调用，避免 stat 未变化
/// 但内容已变时读到旧缓存——SFTP mtime 精度可能不足一秒）。
pub fn invalidate_remote_diff_cache(repo_root: &str, path: &str) {
    let mut cache = remote_diff_cache().lock();
    cache.remove(&format!("{}\u{0}{}", repo_root, path));
}

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
    /// Find the git repository root starting from the given path.
    /// Returns `Ok(Some(repo))` if a git repository was found, `Ok(None)` if the path
    /// is not inside a git repository, and `Err` if git itself is unavailable or the
    /// remote connection failed.
    pub fn find(
        from: &Path,
        is_local: bool,
        ssh_config: Option<SshConnectionConfig>,
    ) -> Result<Option<Self>, String> {
        let from_str = from.to_string_lossy();
        if is_local {
            Self::find_local(&from_str)
        } else {
            let config = ssh_config.ok_or("Missing SSH config for remote git")?;
            Self::find_remote(&from_str, &config)
        }
    }

    fn find_local(from: &str) -> Result<Option<Self>, String> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(from)
            .output()
            .map_err(|e| format!("Git command failed: {e}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Some(Self {
            root,
            backend: GitBackend::Local,
        }))
    }

    fn find_remote(from: &str, config: &SshConnectionConfig) -> Result<Option<Self>, String> {
        let version_result = sftp_fs::with_cached_session(config, |session| {
            session.execute_command("git --version", REMOTE_GIT_TIMEOUT)
        })
        .map_err(|e| format!("Failed to connect to remote: {e}"))?;

        if !version_result.success() {
            return Err("Git is not installed on the remote server".to_string());
        }

        let command = format!("cd {} && git rev-parse --show-toplevel", shell_quote(from));
        let result = sftp_fs::with_cached_session(config, |session| {
            session.execute_command(&command, REMOTE_GIT_TIMEOUT)
        })
        .map_err(|e| format!("Failed to run remote git: {e}"))?;

        if !result.success() {
            return Ok(None);
        }

        let root = result.stdout.trim().to_string();
        Ok(Some(Self {
            root,
            backend: GitBackend::Remote {
                config: config.clone(),
            },
        }))
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

    /// Get the old (HEAD) and new (working tree) contents of a file for the
    /// structured diff view.
    ///
    /// - Old content comes from `git show HEAD:<path>`; empty for untracked
    ///   files or repositories without a HEAD commit yet.
    /// - New content is the working-tree file; empty if the file was deleted.
    ///
    /// Diffing the working tree against HEAD matches the previous behavior of
    /// `diff_file` (staged + unstaged changes combined).
    ///
    /// 远程仓库启用 stat 缓存：先用 `rev-parse HEAD` + SFTP stat（两次轻量
    /// 往返）判断内容是否变化，未变化则直接复用上次全文，避免每次刷新
    /// 都全量拉取两端内容（大文件经 SFTP 明显慢）。本地仓库文件读取廉价，
    /// 不走缓存。
    pub fn diff_file_contents(&self, path: &str) -> Result<(String, String), String> {
        match &self.backend {
            GitBackend::Local => {
                let head_ref = format!("HEAD:{path}");
                let old = self.run_git(&["show", &head_ref]).unwrap_or_default();
                let new = self.read_file(path).unwrap_or_default();
                Ok((old, new))
            }
            GitBackend::Remote { config } => {
                let head_commit = self.head_commit().unwrap_or_else(|| "no-head".to_string());
                let remote_path = join_remote_path(&self.root, path);
                // 工作区文件可能已被删除：stat 失败视为删除态，用哨兵值入键
                let (size, mtime) = match sftp_fs::metadata(config, &remote_path) {
                    Ok(meta) => (meta.size, meta.modified_at),
                    Err(_) => (u64::MAX, None),
                };
                if let Some(cached) = remote_diff_cache_get(
                    &format!("{}\u{0}{}", self.root, path),
                    &head_commit,
                    size,
                    mtime,
                ) {
                    return Ok(cached);
                }
                let head_ref = format!("HEAD:{path}");
                let old = self.run_git(&["show", &head_ref]).unwrap_or_default();
                let new = self.read_file(path).unwrap_or_default();
                remote_diff_cache_insert(
                    format!("{}\u{0}{}", self.root, path),
                    head_commit,
                    size,
                    mtime,
                    (old.clone(), new.clone()),
                );
                Ok((old, new))
            }
        }
    }

    /// 当前 HEAD 提交 id；无提交历史时返回 None。
    fn head_commit(&self) -> Option<String> {
        match &self.backend {
            GitBackend::Local => self
                .run_git(&["rev-parse", "HEAD"])
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            GitBackend::Remote { config } => {
                let command = format!(
                    "cd {} && git rev-parse HEAD",
                    shell_quote(&self.root)
                );
                crate::ssh::sftp_fs::with_cached_session(config, |session| {
                    session.execute_command(&command, REMOTE_GIT_TIMEOUT)
                })
                .ok()
                .filter(|r| r.success())
                .map(|r| r.stdout.trim().to_string())
                .filter(|s| !s.is_empty())
            }
        }
    }

    /// Write content to a working-tree file (local fs or remote via SFTP).
    pub fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        match &self.backend {
            GitBackend::Local => {
                let file_path = Path::new(&self.root).join(path);
                std::fs::write(&file_path, content)
                    .map_err(|e| format!("Failed to write file: {e}"))
            }
            GitBackend::Remote { config } => {
                let remote_path = join_remote_path(&self.root, path);
                sftp_fs::write_file_string(config, &remote_path, content)
                    .map_err(|e| format!("Failed to write remote file: {e}"))
                    .inspect(|_| {
                        // SFTP mtime 精度可能秒级：同秒内写完后 stat 指纹不变，
                        // 必须显式失效，否则 diff 会读到旧内容
                        invalidate_remote_diff_cache(&self.root, path);
                    })
            }
        }
    }

    /// The SSH config when this is a remote repository.
    pub fn ssh_config(&self) -> Option<SshConnectionConfig> {
        match &self.backend {
            GitBackend::Local => None,
            GitBackend::Remote { config } => Some(config.clone()),
        }
    }

    /// Resolve a repo-relative path to a full filesystem path (local or remote).
    pub fn resolve_path(&self, path: &str) -> String {
        match &self.backend {
            GitBackend::Local => Path::new(&self.root)
                .join(path)
                .to_string_lossy()
                .to_string(),
            GitBackend::Remote { .. } => join_remote_path(&self.root, path),
        }
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
                sftp_fs::read_file_to_string(config, &remote_path)
                    .map_err(|e| format!("Failed to read remote file: {e}"))
                    .map(|opt| opt.unwrap_or_default())
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
    use std::path::PathBuf;
    use std::process::Command;

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{}-{}", prefix, uuid::Uuid::new_v4()))
    }

    fn init_repo(path: &PathBuf) {
        let _ = std::fs::create_dir_all(path);
        let _ = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .output()
            .expect("git should be installed");
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(path)
            .output();
        let _ = Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output();
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_dir_all(path);
    }

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

    #[test]
    fn test_find_local_repo() {
        let temp = temp_dir("deepterm-git-find");
        init_repo(&temp);

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        assert_eq!(
            std::fs::canonicalize(repo.root()).unwrap(),
            std::fs::canonicalize(&temp).unwrap()
        );

        cleanup(&temp);
    }

    #[test]
    fn test_find_not_a_repo() {
        let temp = temp_dir("deepterm-git-not-repo");
        let _ = std::fs::create_dir_all(&temp);
        let repo = GitRepository::find(&temp, true, None).unwrap();
        assert!(repo.is_none());
        cleanup(&temp);
    }

    #[test]
    fn test_status_and_stage_local() {
        let temp = temp_dir("deepterm-git-status");
        init_repo(&temp);

        // Initial commit
        std::fs::write(temp.join("file.txt"), "hello").unwrap();
        let _ = Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");

        // Modify the file
        std::fs::write(temp.join("file.txt"), "hello world").unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].path, "file.txt");
        assert!(status[0].status.is_modified());

        // Stage the change
        repo.stage(&["file.txt"]).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.len(), 1);
        assert!(
            status[0].status.is_modified(),
            "staged tracked file should still appear as modified"
        );

        // Diff should contain the change
        let diff = repo.diff_file("file.txt").unwrap();
        assert!(diff.contains("hello world"));

        // Unstage
        repo.unstage(&["file.txt"]).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.len(), 1);
        assert!(status[0].status.is_modified());

        cleanup(&temp);
    }

    #[test]
    fn test_diff_untracked_local() {
        let temp = temp_dir("deepterm-git-untracked");
        init_repo(&temp);

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");

        std::fs::write(temp.join("new.txt"), "new content\nsecond line").unwrap();
        let diff = repo.diff_file("new.txt").unwrap();
        assert!(diff.contains("new content"));
        assert!(diff.contains("second line"));
        assert!(diff.contains("new file mode"));

        cleanup(&temp);
    }
}
