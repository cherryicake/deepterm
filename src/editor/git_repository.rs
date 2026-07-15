use std::path::{Path, PathBuf};
use std::process::Command;

use super::git_status::{DiffStat, GitStatus, StatusEntry, parse_numstat};

/// Git repository operations
pub struct GitRepository {
    pub root: PathBuf,
}

impl GitRepository {
    /// Find the git repository root starting from the given path
    pub fn find(from: &Path) -> Option<Self> {
        let mut current = from.to_path_buf();
        loop {
            if current.join(".git").exists() {
                return Some(Self { root: current });
            }
            if !current.pop() {
                return None;
            }
        }
    }

    /// Get the repository root path
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get all file statuses using `git status --porcelain=v1`
    pub fn status(&self) -> Result<Vec<StatusEntry>, String> {
        let output = Command::new("git")
            .arg("status")
            .arg("--porcelain=v1")
            .arg("--untracked-files=all")
            .arg("--no-renames")
            .arg("-z")
            .arg("--")
            .arg(".")
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("Failed to run git status: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git status failed: {stderr}"));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let git_status = GitStatus::parse(&stdout);
        Ok(git_status.entries.iter().cloned().collect())
    }

    /// Get diff stat for all files using `git diff --numstat`
    pub fn diff_stat(&self) -> Result<Vec<(String, DiffStat)>, String> {
        let output = Command::new("git")
            .arg("diff")
            .arg("--numstat")
            .arg("--no-renames")
            .arg("HEAD")
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("Failed to run git diff --numstat: {e}"))?;

        if !output.status.success() {
            // Might be initial commit with no HEAD
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_numstat(&stdout))
    }

    /// Get the diff output for a specific file
    pub fn diff_file(&self, path: &str) -> Result<String, String> {
        // Try unstaged changes first
        let output = Command::new("git")
            .arg("diff")
            .arg("--")
            .arg(path)
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("Failed to run git diff: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git diff failed: {stderr}"));
        }

        let mut diff = String::from_utf8_lossy(&output.stdout).to_string();

        // Also get staged changes
        let staged_output = Command::new("git")
            .arg("diff")
            .arg("--staged")
            .arg("--")
            .arg(path)
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("Failed to run git diff --staged: {e}"))?;

        if staged_output.status.success() {
            let staged_diff = String::from_utf8_lossy(&staged_output.stdout);
            if !staged_diff.is_empty() {
                if !diff.is_empty() {
                    diff.push_str("\n");
                }
                diff.push_str(&staged_diff);
            }
        }

        // For untracked files, show the full file content as added
        if diff.is_empty() {
            let file_path = self.root.join(path);
            if let Ok(content) = std::fs::read_to_string(&file_path) {
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

    /// Stage files
    pub fn stage(&self, paths: &[&str]) -> Result<(), String> {
        let output = Command::new("git")
            .arg("add")
            .args(paths)
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("Failed to run git add: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git add failed: {stderr}"));
        }
        Ok(())
    }

    /// Unstage files
    pub fn unstage(&self, paths: &[&str]) -> Result<(), String> {
        let output = Command::new("git")
            .arg("reset")
            .arg("HEAD")
            .arg("--")
            .args(paths)
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("Failed to run git reset: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git reset failed: {stderr}"));
        }
        Ok(())
    }
}
