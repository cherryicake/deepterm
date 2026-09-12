use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs;

use super::encoding;
use super::git_status::{DiffStat, GitStatus, StatusEntry, parse_numstat};

const REMOTE_GIT_TIMEOUT: u64 = 60;
const HEAD_TTL: Duration = Duration::from_secs(10);
const STATUS_TTL: Duration = Duration::from_secs(5);

type HeadCache = HashMap<String, (Instant, Option<String>)>;
type StatusCache = HashMap<String, (Instant, Vec<StatusEntry>, Vec<(String, DiffStat)>)>;

static HEAD_CACHE: OnceLock<Mutex<HeadCache>> = OnceLock::new();
fn head_cache() -> &'static Mutex<HeadCache> {
    HEAD_CACHE.get_or_init(Mutex::default)
}
static STATUS_CACHE: OnceLock<Mutex<StatusCache>> = OnceLock::new();
fn status_cache() -> &'static Mutex<StatusCache> {
    STATUS_CACHE.get_or_init(Mutex::default)
}

/// 远程 diff 内容缓存条目：以 (HEAD commit, 文件 size, mtime) 为有效性指纹。
struct CachedRemoteDiff {
    head_commit: String,
    size: u64,
    mtime: Option<std::time::SystemTime>,
    contents: (String, String),
}

/// 键 = "repo_root\u{0}path"。进程级缓存，容量上限防膨胀。
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
    /// `SshConnectionConfig` 有 184 字节。内嵌它会让 `GitBackend` 膨胀到 184 字节，
    /// 连带把 `GitRepository`（208 字节）和所有携带它的事件枚举一起撑大。
    /// 装箱后 `GitBackend` 降到 8 字节，`GitRepository` 降到 32 字节。
    Remote {
        config: Box<SshConnectionConfig>,
    },
}

impl GitBackend {
    fn execute(&self, args: &[&str], cwd: &str) -> Result<String, String> {
        match self {
            GitBackend::Local => execute_local_git(args, cwd),
            GitBackend::Remote { config } => execute_remote_git(config, args, cwd),
        }
    }

    /// 拿到命令的**原始字节**。
    ///
    /// 读取文件内容（如 `git show HEAD:path`）必须走这条路径：`execute` 会先
    /// `from_utf8_lossy` 成 `String`，BOM 被保留成开头的 U+FEFF、非 UTF-8 字节
    /// 被替换成 U+FFFD，之后就没法再还原出原始编码了。
    fn execute_bytes(&self, args: &[&str], cwd: &str) -> Result<Vec<u8>, String> {
        match self {
            GitBackend::Local => execute_local_git_bytes(args, cwd),
            // 远端会话已经把 stdout 解成了 `String`（有损），重新编码回字节
            // 对合法 UTF-8 是无损的，BOM 也得以保留下来等待剥离。
            GitBackend::Remote { .. } => self.execute(args, cwd).map(|stdout| stdout.into_bytes()),
        }
    }
}

/// Git repository operations
#[derive(Clone)]
pub struct GitRepository {
    root: String,
    backend: GitBackend,
}

/// 工作区侧内容的读取结果。
pub enum DiffSideContent {
    /// 可用的文本内容
    Text(String),
    /// 文件已删除或不存在（diff 中应显示为空）
    Missing,
    /// 二进制或无法解码：给出原因，由调用方决定如何展示
    Unsupported(String),
}

/// 把文件字节分为"文本 / 二进制 / 空文件"三类。
fn classify_bytes(bytes: &[u8]) -> DiffSideContent {
    if bytes.is_empty() {
        return DiffSideContent::Text(String::new());
    }
    // 与 SSH 侧一致：二进制文件不参与文本 diff
    if sftp_fs::is_binary_bytes(bytes) {
        return DiffSideContent::Unsupported("二进制文件".to_string());
    }
    // GBK/GB18030/UTF-16/BOM 等由统一解码层处理
    DiffSideContent::Text(crate::editor::encoding::decode_bytes(bytes).text)
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
                config: Box::new(config.clone()),
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

    fn status_uncached(&self) -> Result<Vec<StatusEntry>, String> {
        let stdout = self.run_git(&[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--no-renames",
            "-z",
            "--",
            ".",
        ])?;
        Ok(GitStatus::parse(&stdout).entries.iter().cloned().collect())
    }

    fn diff_stat_uncached(&self) -> Result<Vec<(String, DiffStat)>, String> {
        self.run_git(&["diff", "--numstat", "--no-renames", "HEAD"])
            .map(|s| parse_numstat(&s))
            .or(Ok(Vec::new()))
    }

    fn invalidate_status_cache(&self) {
        status_cache().lock().remove(&self.root);
        head_cache().lock().remove(&self.root);
    }

    /// Get all file statuses using `git status --porcelain=v1` (remote has 5s cache).
    pub fn status(&self) -> Result<Vec<StatusEntry>, String> {
        if !matches!(&self.backend, GitBackend::Remote { .. }) {
            return self.status_uncached();
        }
        let key = self.root.clone();
        if let Some((at, cached, _)) = status_cache().lock().get(&key).cloned() {
            if at.elapsed() < STATUS_TTL {
                return Ok(cached);
            }
        }
        let result = self.status_uncached()?;
        let mut cache = status_cache().lock();
        let diff_stat = cache
            .get(&key)
            .map(|(_, _, d)| d.clone())
            .unwrap_or_default();
        cache.insert(key, (Instant::now(), result.clone(), diff_stat));
        Ok(result)
    }

    /// Get diff stat for all files using `git diff --numstat` (remote has 5s cache).
    pub fn diff_stat(&self) -> Result<Vec<(String, DiffStat)>, String> {
        if !matches!(&self.backend, GitBackend::Remote { .. }) {
            return self.diff_stat_uncached();
        }
        let key = self.root.clone();
        if let Some((at, _, cached)) = status_cache().lock().get(&key).cloned() {
            if at.elapsed() < STATUS_TTL && !cached.is_empty() {
                return Ok(cached);
            }
        }
        let result = self.diff_stat_uncached()?;
        let mut cache = status_cache().lock();
        let status = cache
            .get(&key)
            .map(|(_, s, _)| s.clone())
            .unwrap_or_default();
        cache.insert(key, (Instant::now(), status, result.clone()));
        Ok(result)
    }

    /// Get the diff output for a specific file
    pub fn diff_file(&self, path: &str) -> Result<String, String> {
        let mut diff = self.run_git(&["diff", "--", path])?;

        let staged = self
            .run_git(&["diff", "--staged", "--", path])
            .unwrap_or_default();
        if !staged.is_empty() {
            if !diff.is_empty() {
                diff.push('\n');
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
    /// Remote fetch optimized to feel local (mirrors VSCode `git` model).
    ///
    /// 1. `HEAD` (10s) + `stat` in parallel — saves one RTT.
    /// 2. Cache hit (`head` + `size/mtime`) → 0 RTT.
    /// 3. Miss → reuse editor file cache if fresh (30s) — saves one RTT.
    /// 4. Otherwise fetch `HEAD` and worktree in parallel; pipelined read for >512KB.
    ///
    /// With `DiffView::new_loading` the tab opens instantly and resolves in ~1 RTT.
    pub fn diff_file_contents(&self, path: &str) -> Result<(String, String), String> {
        match &self.backend {
            GitBackend::Local => {
                let head_ref = format!("HEAD:{path}");
                let old = self.head_text(&head_ref);
                let new = match self.read_file_content(path)? {
                    DiffSideContent::Text(text) => text,
                    DiffSideContent::Missing => String::new(),
                    // 二进制/无法解码：给占位文本，避免被当成"整个文件被删除"
                    DiffSideContent::Unsupported(reason) => {
                        log::debug!("git diff: {path} 无法按文本展示: {reason}");
                        format!("<{reason}，无法显示文本差异>")
                    }
                };
                Ok((old, new))
            }
            GitBackend::Remote { config } => {
                // Fetch HEAD and stat in parallel.
                let remote_path = sftp_fs::join_remote_path(&self.root, path);
                let (head_commit, meta_res) = std::thread::scope(|scope| {
                    let head =
                        scope.spawn(|| self.head_commit().unwrap_or_else(|| "no-head".into()));
                    let stat = scope.spawn(|| sftp_fs::metadata(config, &remote_path));
                    // 关键防御：线程 join 失败不得 unwrap 拖垮 UI，
                    // 降级为默认值，保证远端 diff 永不因后台 panic 而崩溃。
                    let head_commit = head.join().unwrap_or_else(|_| {
                        log::warn!("git远端HEAD查询线程异常，降级为no-head");
                        "no-head".to_string()
                    });
                    let meta_res = stat.join().unwrap_or_else(|_| {
                        log::warn!("git远端stat线程异常，降级为空元数据");
                        Err(anyhow::anyhow!("stat线程异常"))
                    });
                    (head_commit, meta_res)
                });
                let (size, mtime) = meta_res
                    .map(|meta| (meta.size, meta.modified_at))
                    .unwrap_or((u64::MAX, None));
                let cache_key = format!("{}\u{0}{}", self.root, path);
                if let Some(cached) = remote_diff_cache_get(&cache_key, &head_commit, size, mtime) {
                    return Ok(cached);
                }
                // Reuse editor cache only if fresh to avoid stale content.
                let file_cache_hit = {
                    let cache =
                        crate::file_manager::file_tree_cache::RemoteFileTreeCache::global().lock();
                    let path_ref = std::path::Path::new(&remote_path);
                    if cache.is_file_fresh(config, path_ref) {
                        cache
                            .get_file_content(config, path_ref)
                            .map(|(content, _)| content)
                    } else {
                        None
                    }
                };
                let head_ref = format!("HEAD:{path}");
                let (old, new) = if let Some(cached_new) = file_cache_hit {
                    // Only need HEAD via git — saves one RTT + SFTP read.
                    let old = self.head_text(&head_ref);
                    (old, cached_new)
                } else {
                    // 3. Parallel fetch of both sides; SFTP read picks pipelined
                    // path for large files based on the stat size already known.
                    std::thread::scope(|s| {
                        let h_old = s.spawn(|| self.head_text(&head_ref));
                        let h_new = s.spawn(|| {
                            let is_large = size != u64::MAX && size > 512 * 1024;
                            let content = if is_large {
                                sftp_fs::read_file_bytes(config, &remote_path)
                                    .ok()
                                    .flatten()
                                    .map(|bytes| classify_bytes(&bytes))
                                    .unwrap_or_else(|| {
                                        self.read_file_content(path)
                                            .unwrap_or(DiffSideContent::Missing)
                                    })
                            } else {
                                self.read_file_content(path)
                                    .unwrap_or(DiffSideContent::Missing)
                            };
                            match content {
                                DiffSideContent::Text(text) => text,
                                DiffSideContent::Missing => String::new(),
                                DiffSideContent::Unsupported(reason) => {
                                    format!("<{reason}，无法显示文本差异>")
                                }
                            }
                        });
                        (
                            h_old.join().unwrap_or_default(),
                            h_new.join().unwrap_or_default(),
                        )
                    })
                };
                remote_diff_cache_insert(
                    cache_key,
                    head_commit,
                    size,
                    mtime,
                    (old.clone(), new.clone()),
                );
                Ok((old, new))
            }
        }
    }

    /// Current HEAD commit id (remote is cached for 10s).
    fn head_commit(&self) -> Option<String> {
        let GitBackend::Remote { config } = &self.backend else {
            return self
                .run_git(&["rev-parse", "HEAD"])
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        };
        if let Some((at, cached)) = head_cache().lock().get(self.root.as_str()).cloned() {
            if at.elapsed() < HEAD_TTL {
                return cached;
            }
        }
        let command = format!("cd {} && git rev-parse HEAD", shell_quote(&self.root));
        let result = crate::ssh::sftp_fs::with_cached_session(config, |session| {
            session.execute_command(&command, REMOTE_GIT_TIMEOUT)
        })
        .ok()
        .filter(|r| r.success())
        .map(|r| r.stdout.trim().to_string())
        .filter(|s| !s.is_empty());
        head_cache()
            .lock()
            .insert(self.root.clone(), (Instant::now(), result.clone()));
        result
    }
}

/// Diff 三元内容：HEAD、暂存区、工作区。
/// VSCode 语义：工作区 diff 的左侧基线是暂存区（部分暂存时）否则是 HEAD；
/// 已暂存 diff 是 HEAD↔暂存区。
#[derive(Clone, Debug)]
pub struct DiffBaseContents {
    pub head: String,
    pub staged: String,
    pub worktree: String,
    pub has_staged: bool,
}

impl DiffBaseContents {
    /// 工作区 diff 的左侧基线（暂存区优先，否则 HEAD）。
    pub fn worktree_base(&self) -> &str {
        if self.has_staged {
            &self.staged
        } else {
            &self.head
        }
    }
}

impl GitRepository {
    /// 探测工作区文件的磁盘编码。
    ///
    /// 保存时必须按原编码写回：读取侧会剥离 BOM、并把 GBK / UTF-16 统一解码
    /// 成 UTF-8，若写回时不做逆变换，BOM 会被永久删除、非 UTF-8 文件会被
    /// 整体改写为 UTF-8（用户无感知的内容损坏）。
    ///
    /// 探测失败（文件不存在、无读权限等）时回退 UTF-8。
    pub fn detect_encoding(&self, path: &str) -> encoding::FileEncoding {
        let detect = |bytes: &[u8]| encoding::decode_bytes(bytes).encoding;

        match &self.backend {
            GitBackend::Local => {
                let file_path = Path::new(&self.root).join(path);
                std::fs::read(&file_path)
                    .map(|bytes| detect(&bytes))
                    .unwrap_or(encoding::FileEncoding::Utf8)
            }
            GitBackend::Remote { config } => {
                let remote_path = sftp_fs::join_remote_path(&self.root, path);
                sftp_fs::read_file_bytes(config, &remote_path)
                    .ok()
                    .flatten()
                    .map(|bytes| detect(&bytes))
                    .unwrap_or(encoding::FileEncoding::Utf8)
            }
        }
    }

    /// Write content to a working-tree file (local fs or remote via SFTP).
    ///
    /// 仅用于写入已经是 UTF-8、且无需保留 BOM / 行尾风格的内容（例如补丁
    /// 导出）。写回用户正在编辑的文件请走 `detect_encoding` +
    /// `crate::editor::editor::save_text_to_file`，否则会破坏编码。
    pub fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        match &self.backend {
            GitBackend::Local => {
                let file_path = Path::new(&self.root).join(path);
                std::fs::write(&file_path, content)
                    .map_err(|e| format!("Failed to write file: {e}"))
            }
            GitBackend::Remote { config } => {
                let remote_path = sftp_fs::join_remote_path(&self.root, path);
                sftp_fs::write_file_string(config, &remote_path, content)
                    .map_err(|e| format!("Failed to write remote file: {e}"))
                    .inspect(|_| {
                        invalidate_remote_diff_cache(&self.root, path);
                        self.invalidate_status_cache();
                        // Remove stale file cache; next open uses fresh size/mtime.
                        crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                            .lock()
                            .remove_file(config, Path::new(&remote_path));
                    })
            }
        }
    }

    /// The SSH config when this is a remote repository.
    pub fn ssh_config(&self) -> Option<SshConnectionConfig> {
        match &self.backend {
            GitBackend::Local => None,
            GitBackend::Remote { config } => Some((**config).clone()),
        }
    }

    /// Resolve a repo-relative path to a full filesystem path (local or remote).
    pub fn resolve_path(&self, path: &str) -> String {
        match &self.backend {
            GitBackend::Local => Path::new(&self.root)
                .join(path)
                .to_string_lossy()
                .to_string(),
            GitBackend::Remote { .. } => sftp_fs::join_remote_path(&self.root, path),
        }
    }

    fn read_file(&self, path: &str) -> Result<String, String> {
        match self.read_file_content(path)? {
            DiffSideContent::Text(text) => Ok(text),
            DiffSideContent::Missing => Err("文件不存在".to_string()),
            DiffSideContent::Unsupported(reason) => Err(reason),
        }
    }

    /// 读取工作区侧内容，并区分"文件不存在"与"无法作为文本展示"。
    ///
    /// 过去 `read_file` 对 GBK/GB18030 等编码与二进制文件统一返回 Err，
    /// 调用方 `diff_file_contents` 再 `unwrap_or_default()` 变成空串，
    /// 于是 diff 视图把这类文件显示成"整个文件被删除"。
    fn read_file_content(&self, path: &str) -> Result<DiffSideContent, String> {
        match &self.backend {
            GitBackend::Local => {
                let file_path = Path::new(&self.root).join(path);
                let bytes = match std::fs::read(&file_path) {
                    Ok(bytes) => bytes,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(DiffSideContent::Missing);
                    }
                    Err(e) => return Err(format!("Failed to read file: {e}")),
                };
                Ok(classify_bytes(&bytes))
            }
            GitBackend::Remote { config } => {
                let remote_path = sftp_fs::join_remote_path(&self.root, path);
                // Handles encoding and pipelined read for large files.
                let bytes_opt = sftp_fs::read_file_bytes(config, &remote_path)
                    .map_err(|e| format!("Failed to read remote file: {e}"))?;
                let Some(bytes) = bytes_opt else {
                    return Ok(DiffSideContent::Missing);
                };
                Ok(classify_bytes(&bytes))
            }
        }
    }

    /// HEAD 侧内容，按仓库里存的字节解码（含 BOM 剥离）。
    ///
    /// 工作区侧走的是 [`classify_bytes`] → `encoding::decode_bytes`：剥掉 BOM、
    /// 把 GBK / UTF-16 解成 UTF-8。HEAD 侧先前用的是 `run_git`，它内部
    /// `from_utf8_lossy`，于是同一个文件得到
    /// `old = "\u{FEFF}line1\n…"` 与 `new = "line1\n…"`，首行被判为永久修改，
    /// 非 UTF-8 文件更是整篇对不上。
    ///
    /// 未跟踪文件 / 尚无 HEAD 提交时 `git show` 会失败，此时返回空串（与旧
    /// 行为一致）。
    fn head_text(&self, head_ref: &str) -> String {
        let Ok(bytes) = self.backend.execute_bytes(&["show", head_ref], &self.root) else {
            return String::new();
        };
        if bytes.is_empty() || sftp_fs::is_binary_bytes(&bytes) {
            return String::new();
        }
        encoding::decode_bytes(&bytes).text
    }

    /// Stage files
    pub fn stage(&self, paths: &[&str]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = Vec::with_capacity(1 + paths.len());
        args.push("add");
        args.extend(paths.iter().copied());
        self.run_git(&args)?;
        self.invalidate_status_cache();
        for p in paths {
            invalidate_remote_diff_cache(&self.root, p);
        }
        Ok(())
    }

    /// Unstage files
    pub fn unstage(&self, paths: &[&str]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = Vec::with_capacity(2 + paths.len());
        args.push("reset");
        args.push("HEAD");
        args.push("--");
        args.extend(paths.iter().copied());
        self.run_git(&args)?;
        self.invalidate_status_cache();
        for p in paths {
            invalidate_remote_diff_cache(&self.root, p);
        }
        Ok(())
    }

    /// 放弃工作区改动：跟踪文件回到 HEAD，未跟踪文件保持不动（由调用方经文件系统删除并二次确认）。
    pub fn discard(&self, paths: &[&str]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        // `git restore` 在旧版 git 不存在，优先 restore、失败回退到 checkout。
        let mut args = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
        args.extend(paths.iter().copied());
        if self.run_git(&args).is_err() {
            let mut fallback = vec!["checkout", "HEAD", "--"];
            fallback.extend(paths.iter().copied());
            self.run_git(&fallback)?;
        }
        self.invalidate_status_cache();
        for p in paths {
            invalidate_remote_diff_cache(&self.root, p);
        }
        Ok(())
    }

    /// 暂存全部（含未跟踪与删除，即 `git add -A`）。
    /// VSCode 智能提交在无已暂存内容时走此路径。
    pub fn stage_all(&self) -> Result<(), String> {
        self.run_git(&["add", "-A"])?;
        self.invalidate_status_cache();
        Ok(())
    }

    /// 提交已暂存改动。空消息直接返回错误，避免产生空提交。
    pub fn commit(&self, message: &str) -> Result<(), String> {
        let message = message.trim();
        if message.is_empty() {
            return Err("提交信息不能为空".to_string());
        }
        self.run_git(&["commit", "-m", message])?;
        self.invalidate_status_cache();
        // HEAD 变了，所有远端 diff 缓存都可能过期；状态缓存已清，diff 缓存按需失效
        //（此处不清全量，仅靠 head_commit 指纹在下次读取时自然失效）。
        Ok(())
    }

    /// 修正上次提交（`--amend`），保留原提交的作者与时间戳语义。
    pub fn commit_amend(&self, message: &str) -> Result<(), String> {
        let message = message.trim();
        if message.is_empty() {
            return Err("提交信息不能为空".to_string());
        }
        self.run_git(&["commit", "--amend", "-m", message])?;
        self.invalidate_status_cache();
        Ok(())
    }

    /// 在指定目录初始化仓库（VSCode 空态“初始化仓库”按钮）。
    pub fn init(path: &str, ssh_config: Option<&SshConnectionConfig>) -> Result<(), String> {
        match ssh_config {
            None => execute_local_git(&["init"], path).map(|_| ()),
            Some(config) => {
                let command = format!("cd {} && git init", shell_quote(path));
                let result = sftp_fs::with_cached_session(config, |session| {
                    session.execute_command(&command, REMOTE_GIT_TIMEOUT)
                })
                .map_err(|e| format!("初始化仓库失败：{e}"))?;
                if result.success() {
                    Ok(())
                } else {
                    Err(format!("初始化仓库失败：{}", result.stderr.trim()))
                }
            }
        }
    }

    /// 当前分支名。detached HEAD 时回退到短 commit id；无 HEAD 时返回 None。
    pub fn branch_name(&self) -> Option<String> {
        if let Ok(name) = self.run_git(&["branch", "--show-current"]) {
            let name = name.trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
        // 旧版 git / detached：用 abbrev-ref，仍为 HEAD 则取短 id。
        if let Ok(name) = self.run_git(&["rev-parse", "--abbrev-ref", "HEAD"]) {
            let name = name.trim().to_string();
            if !name.is_empty() && name != "HEAD" {
                return Some(name);
            }
        }
        self.run_git(&["rev-parse", "--short", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// 暂存区（index）中的文件内容。用于“已暂存” diff 视图与 hunk 暂存。
    /// 未暂存的新文件 / 无 HEAD 时返回空串。
    pub fn staged_text(&self, path: &str) -> String {
        let staged_ref = format!(":{path}");
        let Ok(bytes) = self.backend.execute_bytes(&["show", &staged_ref], &self.root) else {
            return String::new();
        };
        if bytes.is_empty() || sftp_fs::is_binary_bytes(&bytes) {
            return String::new();
        }
        encoding::decode_bytes(&bytes).text
    }

    /// 单个路径是否有已暂存改动（`git status --porcelain` 限定路径）。
    /// 用于判断工作区 diff 的左侧基线：部分暂存时基线是暂存区（VSCode 行为），
    /// 否则是 HEAD。注意已暂存删除的路径：index 无 blob，`staged_text` 为空
    /// 串但此时空串是有意义的基线，不能回退到 HEAD。
    fn path_has_staged(&self, path: &str) -> bool {
        let output = self
            .run_git(&["status", "--porcelain=v1", "--no-renames", "-z", "--", path])
            .unwrap_or_default();
        GitStatus::parse(&output)
            .entries
            .iter()
            .any(|e| e.status.has_staged_changes())
    }

    /// 一次取齐 diff 三元组（HEAD、暂存区、工作区 + 是否有已暂存改动）。
    ///
    /// VSCode 语义由调用方经 [`DiffBaseContents::worktree_base`] 落实。
    /// 远端四路并行（HEAD / 暂存区 / 工作区 / 单路径状态），(HEAD, 工作区)
    /// 命中缓存时只补拉暂存区与状态。
    pub fn diff_base_contents(&self, path: &str) -> Result<DiffBaseContents, String> {
        fn worktree_string(content: DiffSideContent, path: &str) -> String {
            match content {
                DiffSideContent::Text(text) => text,
                DiffSideContent::Missing => String::new(),
                DiffSideContent::Unsupported(reason) => {
                    log::debug!("git diff: {path} 无法按文本展示: {reason}");
                    format!("<{reason}，无法显示文本差异>")
                }
            }
        }

        match &self.backend {
            GitBackend::Local => {
                let head_ref = format!("HEAD:{path}");
                let head = self.head_text(&head_ref);
                let staged = self.staged_text(path);
                let has_staged = self.path_has_staged(path);
                let worktree = worktree_string(self.read_file_content(path)?, path);
                Ok(DiffBaseContents {
                    head,
                    staged,
                    worktree,
                    has_staged,
                })
            }
            GitBackend::Remote { config } => {
                let remote_path = sftp_fs::join_remote_path(&self.root, path);
                let (head_commit, meta_res) = std::thread::scope(|scope| {
                    let head =
                        scope.spawn(|| self.head_commit().unwrap_or_else(|| "no-head".into()));
                    let stat = scope.spawn(|| sftp_fs::metadata(config, &remote_path));
                    let head_commit = head.join().unwrap_or_else(|_| {
                        log::warn!("git远端HEAD查询线程异常，降级为no-head");
                        "no-head".to_string()
                    });
                    let meta_res = stat.join().unwrap_or_else(|_| {
                        log::warn!("git远端stat线程异常，降级为空元数据");
                        Err(anyhow::anyhow!("stat线程异常"))
                    });
                    (head_commit, meta_res)
                });
                let (size, mtime) = meta_res
                    .map(|meta| (meta.size, meta.modified_at))
                    .unwrap_or((u64::MAX, None));
                let cache_key = format!("{}\u{0}{}", self.root, path);

                // 编辑器文件缓存（30s 新鲜度）命中则省一次 SFTP 读。
                let file_cache_hit = {
                    let cache =
                        crate::file_manager::file_tree_cache::RemoteFileTreeCache::global().lock();
                    let path_ref = std::path::Path::new(&remote_path);
                    if cache.is_file_fresh(config, path_ref) {
                        cache
                            .get_file_content(config, path_ref)
                            .map(|(content, _)| content)
                    } else {
                        None
                    }
                };

                if let Some(cached) = remote_diff_cache_get(&cache_key, &head_commit, size, mtime)
                {
                    // 缓存命中：并行补拉暂存区与单路径状态即可。
                    let (staged, has_staged) = std::thread::scope(|s| {
                        let h_staged = s.spawn(|| self.staged_text(path));
                        let h_status = s.spawn(|| self.path_has_staged(path));
                        (
                            h_staged.join().unwrap_or_default(),
                            h_status.join().unwrap_or(false),
                        )
                    });
                    return Ok(DiffBaseContents {
                        head: cached.0,
                        staged,
                        worktree: cached.1,
                        has_staged,
                    });
                }

                let head_ref = format!("HEAD:{path}");
                let (head, staged, has_staged, worktree) = std::thread::scope(|s| {
                    let h_head = s.spawn(|| self.head_text(&head_ref));
                    let h_staged = s.spawn(|| self.staged_text(path));
                    let h_status = s.spawn(|| self.path_has_staged(path));
                    let h_new = s.spawn(|| {
                        if let Some(cached_new) = file_cache_hit {
                            return cached_new;
                        }
                        let is_large = size != u64::MAX && size > 512 * 1024;
                        let content = if is_large {
                            sftp_fs::read_file_bytes(config, &remote_path)
                                .ok()
                                .flatten()
                                .map(|bytes| classify_bytes(&bytes))
                                .unwrap_or(DiffSideContent::Missing)
                        } else {
                            self.read_file_content(path)
                                .unwrap_or(DiffSideContent::Missing)
                        };
                        worktree_string(content, path)
                    });
                    (
                        h_head.join().unwrap_or_default(),
                        h_staged.join().unwrap_or_default(),
                        h_status.join().unwrap_or(false),
                        h_new.join().unwrap_or_default(),
                    )
                });
                remote_diff_cache_insert(
                    cache_key,
                    head_commit,
                    size,
                    mtime,
                    (head.clone(), worktree.clone()),
                );
                Ok(DiffBaseContents {
                    head,
                    staged,
                    worktree,
                    has_staged,
                })
            }
        }
    }

    /// 暂存一个 hunk：将单 hunk 补丁经 `git apply --cached` 写入 index。
    /// 调用方需保证 `patch` 为合法 unified diff（含 `---/+++` 头）。
    pub fn stage_hunk_patch(&self, patch: &str) -> Result<(), String> {
        use std::io::Write;
        match &self.backend {
            GitBackend::Local => {
                let mut child = Command::new("git")
                    .args(["apply", "--cached", "--unidiff-zero", "-"])
                    .current_dir(&self.root)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("暂存失败：{e}"))?;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin
                        .write_all(patch.as_bytes())
                        .map_err(|e| format!("暂存失败：{e}"))?;
                }
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("暂存失败：{e}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("暂存该块失败：{}", stderr.trim()));
                }
                self.invalidate_status_cache();
                Ok(())
            }
            GitBackend::Remote { config } => {
                // 远端会话不支持 stdin：补丁先写 /tmp 临时文件，再让 git apply 读取。
                let tmp = format!(
                    "/tmp/deepterm-hunk-{}-{}.patch",
                    std::process::id(),
                    patch.len()
                );
                sftp_fs::write_file_string(config, &tmp, patch)
                    .map_err(|e| format!("暂存失败：{e}"))?;
                let command = format!(
                    "cd {} && git apply --cached --unidiff-zero {} && rm -f {}",
                    shell_quote(&self.root),
                    shell_quote(&tmp),
                    shell_quote(&tmp)
                );
                let result = sftp_fs::with_cached_session(config, |session| {
                    session.execute_command(&command, REMOTE_GIT_TIMEOUT)
                });
                // 失败也尽力清理临时文件。
                if result.is_err() {
                    let _ = sftp_fs::with_cached_session(config, |session| {
                        session.remove_file(&tmp)
                    });
                }
                result
                    .map_err(|e| format!("暂存失败：{e}"))
                    .and_then(|r| {
                        if r.success() {
                            self.invalidate_status_cache();
                            Ok(())
                        } else {
                            Err(format!("暂存该块失败：{}", r.stderr.trim()))
                        }
                    })
            }
        }
    }
}

fn execute_local_git(args: &[&str], cwd: &str) -> Result<String, String> {
    let stdout = execute_local_git_bytes(args, cwd)?;
    Ok(String::from_utf8_lossy(&stdout).to_string())
}

/// 与 [`execute_local_git`] 相同，但返回未解码的原始字节（供编码探测使用）。
fn execute_local_git_bytes(args: &[&str], cwd: &str) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("Failed to run git {}: {e}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {stderr}", args.join(" ")));
    }

    Ok(output.stdout)
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
    crate::utils::shell::shell_quote_single(s)
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
            sftp_fs::join_remote_path("/home/user/project", "src/main.rs"),
            "/home/user/project/src/main.rs"
        );
        assert_eq!(
            sftp_fs::join_remote_path("/home/user/project/", "src/main.rs"),
            "/home/user/project/src/main.rs"
        );
        assert_eq!(
            sftp_fs::join_remote_path("/home/user/project", "/src/main.rs"),
            "/home/user/project/src/main.rs"
        );
        assert_eq!(sftp_fs::join_remote_path("/", "src/main.rs"), "/src/main.rs");
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

    /// 回归测试：HEAD 侧必须与工作区侧走同一套解码。
    ///
    /// 曾经 HEAD 侧用 `run_git`（内部 `from_utf8_lossy`），BOM 被保留成开头的
    /// U+FEFF，而工作区侧的 `decode_bytes` 会剥掉它 —— 于是未做任何修改的
    /// BOM 文件首行永远被判为"已修改"。GBK 文件同理（整篇对不上）。
    #[test]
    fn test_head_side_strips_bom_and_decodes_like_worktree() {
        let temp = temp_dir("deepterm-git-bom");
        init_repo(&temp);

        let content = "line1\nline2\n";
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(content.as_bytes());
        std::fs::write(temp.join("bom.txt"), &bytes).unwrap();

        let _ = Command::new("git")
            .args(["add", "bom.txt"])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");

        let (old, new) = repo.diff_file_contents("bom.txt").unwrap();
        assert!(!old.starts_with('\u{feff}'), "HEAD 侧必须剥掉 BOM: {old:?}");
        assert!(
            !new.starts_with('\u{feff}'),
            "工作区侧必须剥掉 BOM: {new:?}"
        );
        assert_eq!(old, new, "未修改的 BOM 文件不应产生任何差异");
        assert_eq!(new, content);

        cleanup(&temp);
    }

    /// 真正改了内容的 BOM 文件仍要正常出差异（确认上面的相等不是"两头都空"）。
    #[test]
    fn test_bom_file_still_reports_real_changes() {
        let temp = temp_dir("deepterm-git-bom-mod");
        init_repo(&temp);

        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("line1\nline2\n".as_bytes());
        std::fs::write(temp.join("bom.txt"), &bytes).unwrap();

        let _ = Command::new("git")
            .args(["add", "bom.txt"])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        // 只改第二行，第一行的 BOM 必须不再制造伪差异。
        let mut modified = vec![0xEF, 0xBB, 0xBF];
        modified.extend_from_slice("line1\nline2 changed\n".as_bytes());
        std::fs::write(temp.join("bom.txt"), &modified).unwrap();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        let (old, new) = repo.diff_file_contents("bom.txt").unwrap();
        assert_eq!(old, "line1\nline2\n");
        assert_eq!(new, "line1\nline2 changed\n");

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

    /// hunk 补丁回环：改两处，只暂存第一块，暂存区与工作区应各自正确。
    #[test]
    fn test_stage_hunk_patch_roundtrip() {
        let temp = temp_dir("deepterm-git-hunk");
        init_repo(&temp);

        std::fs::write(temp.join("a.txt"), "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        std::fs::write(temp.join("a.txt"), "ONE\ntwo\nthree\nfour\nfive\nSIX\n").unwrap();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        let (old, new) = repo.diff_file_contents("a.txt").unwrap();
        let hunks = crate::editor::diff_model::compute_hunks(&old, &new);
        assert_eq!(hunks.len(), 2);

        let patch = crate::editor::diff_model::build_hunk_patch("a.txt", &old, &new, &hunks[0]);
        repo.stage_hunk_patch(&patch).unwrap();

        // 暂存区只有第一块，工作区两块都在。
        let staged = repo.staged_text("a.txt");
        assert!(staged.contains("ONE\n"), "staged should contain first hunk: {staged:?}");
        assert!(
            staged.contains("five\nsix\n"),
            "staged should keep old tail: {staged:?}"
        );
        let status = repo.status().unwrap();
        let entry = status.iter().find(|e| e.path == "a.txt").expect("a.txt in status");
        // 部分暂存：既有已暂存也有未暂存。
        assert!(entry.status.has_staged_changes());
        assert!(entry.status.has_unstaged_changes());

        cleanup(&temp);
    }

    #[test]
    fn test_commit_and_branch_and_discard() {
        let temp = temp_dir("deepterm-git-commit");
        init_repo(&temp);

        std::fs::write(temp.join("f.txt"), "v1\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        // 空消息拒绝提交。
        assert!(repo.commit("   ").is_err());

        std::fs::write(temp.join("f.txt"), "v2\n").unwrap();
        repo.stage(&["f.txt"]).unwrap();
        repo.commit("second").unwrap();
        let (old, new) = repo.diff_file_contents("f.txt").unwrap();
        assert_eq!(old, new, "提交后 HEAD 与工作区应一致");

        // 分支名非空（新仓库默认分支）。
        assert!(repo.branch_name().is_some());

        // 放弃更改回到 HEAD。
        std::fs::write(temp.join("f.txt"), "v3-dirty\n").unwrap();
        repo.discard(&["f.txt"]).unwrap();
        let content = std::fs::read_to_string(temp.join("f.txt")).unwrap();
        assert_eq!(content, "v2\n");

        cleanup(&temp);
    }

    /// 智能提交（stage_all）+ amend 回环。
    #[test]
    fn test_stage_all_and_amend() {
        let temp = temp_dir("deepterm-git-smart");
        init_repo(&temp);

        std::fs::write(temp.join("a.txt"), "a1\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");

        // 未跟踪 + 修改混合：stage_all 应全部收进暂存区。
        std::fs::write(temp.join("a.txt"), "a2\n").unwrap();
        std::fs::write(temp.join("b.txt"), "b1\n").unwrap();
        repo.stage_all().unwrap();
        let status = repo.status().unwrap();
        assert!(
            status
                .iter()
                .all(|e| e.status.has_staged_changes()),
            "stage_all 后所有条目都应有已暂存改动"
        );

        repo.commit("smart").unwrap();
        assert!(repo.status().unwrap().is_empty());

        // amend：空消息拒绝；正常 amend 改写 HEAD 内容。
        assert!(repo.commit_amend("  ").is_err());
        std::fs::write(temp.join("a.txt"), "a3\n").unwrap();
        repo.stage(&["a.txt"]).unwrap();
        repo.commit_amend("amended").unwrap();
        let (old, new) = repo.diff_file_contents("a.txt").unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("a3\n", "a3\n"));

        cleanup(&temp);
    }

    /// 三元组基线语义：部分暂存时工作区基线是暂存区；无暂存时是 HEAD；
    /// 已暂存删除的路径 has_staged 为 true（空基线有意义）。
    #[test]
    fn test_diff_base_contents() {
        let temp = temp_dir("deepterm-git-base");
        init_repo(&temp);

        std::fs::write(temp.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(temp.join("g.txt"), "g1\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");

        // 纯未暂存修改：基线 = HEAD。
        std::fs::write(temp.join("g.txt"), "g2\n").unwrap();
        let base = repo.diff_base_contents("g.txt").unwrap();
        assert!(!base.has_staged);
        assert_eq!(base.worktree_base(), "g1\n");
        assert_eq!(base.worktree, "g2\n");

        // 部分暂存：第一行改动暂存，第二行改动留在工作区。
        std::fs::write(temp.join("f.txt"), "ONE\ntwo\nthree\n").unwrap();
        repo.stage(&["f.txt"]).unwrap();
        std::fs::write(temp.join("f.txt"), "ONE\nTWO\nthree\n").unwrap();
        let base = repo.diff_base_contents("f.txt").unwrap();
        assert!(base.has_staged);
        assert_eq!(base.worktree_base(), "ONE\ntwo\nthree\n");
        assert_eq!(base.worktree, "ONE\nTWO\nthree\n");
        assert_eq!(base.head, "one\ntwo\nthree\n");

        // 已暂存删除：has_staged 为 true，基线为空。
        //（先还原 g.txt 的未暂存修改，否则 git rm 会拒绝执行。）
        repo.discard(&["g.txt"]).unwrap();
        let _ = Command::new("git")
            .args(["rm", "--quiet", "g.txt"])
            .current_dir(&temp)
            .output();
        let base = repo.diff_base_contents("g.txt").unwrap();
        assert!(base.has_staged);
        assert_eq!(base.worktree_base(), "");

        cleanup(&temp);
    }
}
