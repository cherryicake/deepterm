use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs;
use crate::ssh::sftp_fs::RemoteFileKind;

use super::encoding;
use super::git_status::{DiffStat, FileStatus, GitStatus, StatusEntry, parse_numstat};

const REMOTE_GIT_TIMEOUT: u64 = 60;
const HEAD_TTL: Duration = Duration::from_secs(10);
const STATUS_TTL: Duration = Duration::from_secs(5);
/// 远端 `GitRepository::find` 结果缓存 TTL。
const FIND_TTL: Duration = Duration::from_secs(30);

/// status+numstat 合并命令两段输出之间的分隔标记（含前导 NUL）。
const STATUS_NUMSTAT_MARKER: &[u8] = b"\0__DEEPTERM_NUMSTAT__\n";

/// 构造一次 `git` 调用，**不弹控制台窗口**（仅 windows 生效）。
///
/// `git.exe` 是控制台程序，而 deepterm 是 GUI 子系统进程；用
/// `Command::output()` / `spawn()` 启动它时 stdio 走管道，子进程无法继承
/// 我们的控制台，Windows 就会新分配一个并显示出来。git 调用点很多
/// （`rev-parse` 找仓、status 轮询、`merge-file`…），逐个加标志迟早会漏，
/// 所以统一走这里——新增调用点默认就是对的。
///
/// 也统一补上 `PATH`：git 与语言服务器一样是**按名字**启动的，从 Finder /
/// Dock 启动时本进程只有 launchd 那份最小 `PATH`。`/usr/bin/git` 通常在，
/// 但用户若只装了 Homebrew 版 git（或 git 只在 `~/.local/bin`），不补就会
/// 静默 `NotFound`——git 面板表现为「仓库列表空着」，没有任何报错。
/// 见 [`crate::utils::path`]。
pub(crate) fn git_command() -> Command {
    let mut command = crate::utils::console::hidden_command("git");
    // 取不到主目录时保持继承行为：设一份残缺的 `PATH` 比不设更糟。
    if let Some(path) = crate::utils::path::local_spawn_path() {
        command.env("PATH", path);
    }
    command
}

/// 在字节序列中查找子序列（`[u8]::windows` 对 1 字节 needle 的特化不通用，
/// 简单实现足够：仅在合并命令输出中找一次固定短标记）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// git 文本输出按内容编码解码：合法 UTF-8 直接用（零拷贝路径由调用方决定），
/// 否则走 `decode_bytes` 兜底（GBK 等非 UTF-8 文件名/blob）。
fn decode_bytes_to_string(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    encoding::decode_bytes(bytes).text
}

type HeadCache = HashMap<String, (Instant, Option<String>)>;
type StatusCache = HashMap<String, (Instant, Vec<StatusEntry>, Vec<(String, DiffStat)>)>;
/// 键 = 远端会话标识 + 起始路径；值为 find 结果。
type FindCache = HashMap<String, (Instant, Result<Option<GitRepository>, String>)>;

/// HEAD / STATUS 缓存容量上限（键 = 会话中查询过的远端仓库 root）。
///
/// 读取侧只有 TTL 门控、**过期条目不淘汰**：只有同 key 再次查询时才被
/// 覆盖。SSH 到过多主机/目录后，每条目（全仓库 status + numstat，大仓库
/// 几十至上百 KB）会一直驻留。这里在插入侧收敛：先淘汰过期条目，仍超限
/// 则整表清空——对齐 `REMOTE_DIFF_CACHE_CAP` 的既有做法，代价是 TTL 内
/// 按需重建（一次远端 git 调用）。
const HEAD_CACHE_CAP: usize = 32;
const STATUS_CACHE_CAP: usize = 32;

/// 有界插入 HEAD 缓存：淘汰过期 → 超限清空 → 插入。
///
/// `!contains_key` 守卫与 `REMOTE_DIFF_CACHE_CAP` 一致：刷新一个已知 key
/// 不该把表里其他仍在 TTL 内的条目一起清掉。
fn bounded_insert_head(cache: &mut HeadCache, key: String, value: (Instant, Option<String>)) {
    cache.retain(|_, (at, _)| at.elapsed() < HEAD_TTL);
    if cache.len() >= HEAD_CACHE_CAP && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, value);
}

/// 有界插入 STATUS 缓存（语义同 [`bounded_insert_head`]）。
fn bounded_insert_status(
    cache: &mut StatusCache,
    key: String,
    value: (Instant, Vec<StatusEntry>, Vec<(String, DiffStat)>),
) {
    cache.retain(|_, (at, ..)| at.elapsed() < STATUS_TTL);
    if cache.len() >= STATUS_CACHE_CAP && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, value);
}

static HEAD_CACHE: OnceLock<Mutex<HeadCache>> = OnceLock::new();
fn head_cache() -> &'static Mutex<HeadCache> {
    HEAD_CACHE.get_or_init(Mutex::default)
}
static STATUS_CACHE: OnceLock<Mutex<StatusCache>> = OnceLock::new();
fn status_cache() -> &'static Mutex<StatusCache> {
    STATUS_CACHE.get_or_init(Mutex::default)
}
static FIND_CACHE: OnceLock<Mutex<FindCache>> = OnceLock::new();
fn find_cache() -> &'static Mutex<FindCache> {
    FIND_CACHE.get_or_init(Mutex::default)
}

/// 远端 find 缓存键：连接标识（用户:主机:端口）+ 起始路径。
/// 键里不含任何凭据。容量上限防膨胀（多服务器多目录场景）。
const FIND_CACHE_CAP: usize = 128;

fn remote_repo_cache_key(config: &SshConnectionConfig, from: &str) -> String {
    format!(
        "{}:{}:{}@{}",
        config.username, config.host, config.port, from
    )
}

/// 使远端 find 缓存失效（初始化仓库后调用，保证 find 重新探测）。
pub fn invalidate_find_cache(ssh_config: Option<&SshConnectionConfig>) {
    let Some(config) = ssh_config else {
        return;
    };
    let prefix = format!("{}:{}:{}@", config.username, config.host, config.port);
    find_cache().lock().retain(|k, _| !k.starts_with(&prefix));
}

/// 远程 diff 内容缓存条目。
///
/// 有效性指纹分两层：
/// - `(HEAD commit, 文件 size, mtime)`：决定 (head, worktree) 二元组可否复用；
/// - `.git/index` 的 `(size, mtime)`：决定暂存区内容与单路径状态
///   （has_staged / is_new）可否复用。index stat 失败（`.git` 为文件的
///   worktree 布局等）时暂存层不缓存，退回逐次拉取。
#[derive(Clone)]
struct CachedRemoteDiff {
    head_commit: String,
    size: u64,
    mtime: Option<std::time::SystemTime>,
    /// `.git/index` 指纹；None = 暂存层未缓存。
    index: Option<(u64, Option<std::time::SystemTime>)>,
    /// 暂存区内容 + 单路径状态 + 是否不可按文本展示。
    staged: Option<StagedSide>,
    /// (head, worktree) 文本。
    contents: (String, String),
    /// 工作区侧不可按文本展示（二进制等）。
    worktree_unsupported: bool,
}

/// 缓存的暂存区侧信息。
#[derive(Clone)]
struct StagedSide {
    text: String,
    unsupported: bool,
    status: Option<super::git_status::FileStatus>,
}

impl CachedRemoteDiff {
    fn head_worktree_matches(
        &self,
        head_commit: &str,
        size: u64,
        mtime: Option<std::time::SystemTime>,
    ) -> bool {
        self.head_commit == head_commit && self.size == size && self.mtime == mtime
    }
}

/// 键 = "repo_root\u{0}path"。进程级缓存，容量上限防膨胀。
const REMOTE_DIFF_CACHE_CAP: usize = 64;

fn remote_diff_cache() -> &'static Mutex<HashMap<String, CachedRemoteDiff>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedRemoteDiff>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// (head, worktree) 二元组命中（忽略暂存层）。
fn remote_diff_cache_get_pair(
    key: &str,
    head_commit: &str,
    size: u64,
    mtime: Option<std::time::SystemTime>,
) -> Option<(String, String)> {
    let cache = remote_diff_cache().lock();
    let entry = cache.get(key)?;
    entry
        .head_worktree_matches(head_commit, size, mtime)
        .then(|| entry.contents.clone())
}

/// 全量命中（含暂存区与单路径状态）。
fn remote_diff_cache_get_full(
    key: &str,
    head_commit: &str,
    size: u64,
    mtime: Option<std::time::SystemTime>,
    index: Option<(u64, Option<std::time::SystemTime>)>,
) -> Option<CachedRemoteDiff> {
    let cache = remote_diff_cache().lock();
    let entry = cache.get(key)?;
    // 指纹缺失时不能宣称全量命中——无法证明暂存层仍然新鲜。
    let (index_size, index_mtime) = index?;
    if entry.index != Some((index_size, index_mtime)) {
        return None;
    }
    entry
        .head_worktree_matches(head_commit, size, mtime)
        .then(|| entry.clone())
}

/// 写入 (head, worktree) 二元组；暂存层信息缺失时保留既有条目里的暂存层
/// （同指纹下既有 staged 仍然新鲜，不能被 pair-only 写入抹掉）。
#[allow(clippy::too_many_arguments)]
fn remote_diff_cache_insert_pair(
    key: String,
    head_commit: String,
    size: u64,
    mtime: Option<std::time::SystemTime>,
    contents: (String, String),
    worktree_unsupported: bool,
) {
    let mut cache = remote_diff_cache().lock();
    if cache.len() >= REMOTE_DIFF_CACHE_CAP && !cache.contains_key(&key) {
        cache.clear();
    }
    let (index, staged) = match cache.get(&key) {
        Some(entry) if entry.head_worktree_matches(&head_commit, size, mtime) => {
            (entry.index, entry.staged.clone())
        }
        _ => (None, None),
    };
    cache.insert(
        key,
        CachedRemoteDiff {
            head_commit,
            size,
            mtime,
            index,
            staged,
            contents,
            worktree_unsupported,
        },
    );
}

/// 全量写入（含暂存层；index 指纹必须有效，否则退化为 pair-only）。
#[allow(clippy::too_many_arguments)]
fn remote_diff_cache_insert_full(
    key: String,
    head_commit: String,
    size: u64,
    mtime: Option<std::time::SystemTime>,
    index: Option<(u64, Option<std::time::SystemTime>)>,
    staged: StagedSide,
    contents: (String, String),
    worktree_unsupported: bool,
) {
    let mut cache = remote_diff_cache().lock();
    if cache.len() >= REMOTE_DIFF_CACHE_CAP && !cache.contains_key(&key) {
        cache.clear();
    }
    let staged = index.map(|_| staged);
    cache.insert(
        key,
        CachedRemoteDiff {
            head_commit,
            size,
            mtime,
            index,
            staged,
            contents,
            worktree_unsupported,
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
    /// 读取文件内容（如 `git show HEAD:path`）必须走这条路径：
    /// `execute` 会先解码成 `String`（本地 `from_utf8_lossy`、远端
    /// `execute_command` 同样有损），BOM 被保留成开头的 U+FEFF、非 UTF-8
    /// 字节被替换成 U+FFFD，之后就没法再还原出原始编码了。
    fn execute_bytes(&self, args: &[&str], cwd: &str) -> Result<Vec<u8>, String> {
        match self {
            GitBackend::Local => execute_local_git_bytes(args, cwd),
            // 远端 exec 通道经 `execute_command_bytes` 保留原始字节，
            // GBK / UTF-16 blob 不再有损。
            GitBackend::Remote { config } => execute_remote_git_bytes(config, args, cwd),
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

/// 冲突解决采用的侧。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictSide {
    /// 当前分支的版本
    Ours,
    /// 传入分支的版本
    Theirs,
    /// 两侧全文并置（union 合并）
    Both,
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

/// status + numstat 合并拉取的结果。
type StatusAndNumstat = (Vec<StatusEntry>, Vec<(String, DiffStat)>);

impl GitRepository {
    /// Find the git repository root starting from the given path.
    /// Returns `Ok(Some(repo))` if a git repository was found, `Ok(None)` if the path
    /// is not inside a git repository, and `Err` if git itself is unavailable or the
    /// remote connection failed.
    ///
    /// 带进程级缓存：远端一次 find 是两条 SSH 往返（git --version +
    /// rev-parse），编辑器每打开/保存一个文件都会为行标记重找一次仓库，
    /// 不缓存的话多文件场景开销成倍放大。TTL 内直接复用；本地探测很便宜，
    /// 只在 `find` 语义需要刷新时由 `invalidate_find_cache` 显式清除。
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
            let cache_key = remote_repo_cache_key(&config, &from_str);
            if let Some((at, cached)) = find_cache().lock().get(&cache_key).cloned()
                && at.elapsed() < FIND_TTL
            {
                return cached;
            }
            let result = Self::find_remote(&from_str, &config);
            // 连接失败（Err）不缓存：下次重试；Ok 结果（含 None）缓存。
            if result.is_ok() {
                let mut cache = find_cache().lock();
                // 容量控制：满时清空（键数有限，粗暴策略足够）。
                if cache.len() >= FIND_CACHE_CAP && !cache.contains_key(&cache_key) {
                    cache.clear();
                }
                cache.insert(cache_key, (Instant::now(), result.clone()));
            }
            result
        }
    }

    fn find_local(from: &str) -> Result<Option<Self>, String> {
        let output = git_command()
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
            session.execute_command_bytes("git --version", REMOTE_GIT_TIMEOUT)
        })
        .map_err(|e| format!("Failed to connect to remote: {e}"))?;

        if !version_result.success() {
            return Err("Git is not installed on the remote server".to_string());
        }

        let command = format!("cd {} && git rev-parse --show-toplevel", shell_quote(from));
        let result = sftp_fs::with_cached_session(config, |session| {
            session.execute_command_bytes(&command, REMOTE_GIT_TIMEOUT)
        })
        .map_err(|e| format!("Failed to run remote git: {e}"))?;

        if !result.success() {
            return Ok(None);
        }

        let root = decode_bytes_to_string(&result.stdout).trim().to_string();
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

    /// 远端专用：一条命令同时取 status 与 numstat，结果进共享缓存。
    /// status/numstat 两个入口都从这份缓存取，保证一次 SSH 往返。
    ///
    /// 合并形式：`git status -z ... && printf '\0MARKER\n' && git diff --numstat HEAD`。
    /// marker 自带前导 NUL，porcelain 输出本身以 NUL 结尾，切分稳定；
    /// numstat 输出是行分隔文本。
    fn status_and_numstat(&self) -> Result<StatusAndNumstat, String> {
        let key = self.root.clone();
        if let Some((at, cached_status, cached_stat)) = status_cache().lock().get(&key).cloned()
            && at.elapsed() < STATUS_TTL
        {
            return Ok((cached_status, cached_stat));
        }
        let GitBackend::Remote { config } = &self.backend else {
            // 本地退化为两条独立命令（本地进程开销可忽略）。
            let status = self.status_uncached()?;
            let numstat = self.diff_stat_uncached()?;
            return Ok((status, numstat));
        };
        let command = format!(
            "cd {} && git status --porcelain=v1 --untracked-files=all --no-renames -z -- . \
             && printf '\\0__DEEPTERM_NUMSTAT__\\n' && git diff --numstat --no-renames HEAD",
            shell_quote(&self.root),
        );
        let raw = sftp_fs::with_cached_session(config, |session| {
            session.execute_command_bytes(&command, REMOTE_GIT_TIMEOUT)
        })
        .map_err(|e| format!("Failed to run remote git: {e}"))?;
        if !raw.success() {
            return Err(format!("git status failed: {}", raw.stderr.trim()));
        }
        let bytes = raw.stdout;
        let Some(pos) = find_subslice(&bytes, STATUS_NUMSTAT_MARKER) else {
            return Err("git status 合并输出缺少分隔标记".to_string());
        };
        let status_text = decode_bytes_to_string(&bytes[..pos]);
        let numstat_text = decode_bytes_to_string(&bytes[pos + STATUS_NUMSTAT_MARKER.len()..]);
        let entries: Vec<StatusEntry> = GitStatus::parse(&status_text)
            .entries
            .iter()
            .cloned()
            .collect();
        let numstat = parse_numstat(&numstat_text);
        {
            let mut cache = status_cache().lock();
            bounded_insert_status(
                &mut cache,
                key,
                (Instant::now(), entries.clone(), numstat.clone()),
            );
        }
        Ok((entries, numstat))
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
        Ok(self.status_and_numstat()?.0)
    }

    /// Get diff stat for all files using `git diff --numstat` (remote has 5s cache).
    ///
    /// 注意缓存命中条件不能加 `!cached.is_empty()`：只有未跟踪改动的仓库
    /// numstat 恒为空，那是合法结果而非未缓存——加了条件会让这类仓库
    /// 每次轮询都真跑一次远端命令。
    pub fn diff_stat(&self) -> Result<Vec<(String, DiffStat)>, String> {
        if !matches!(&self.backend, GitBackend::Remote { .. }) {
            return self.diff_stat_uncached();
        }
        Ok(self.status_and_numstat()?.1)
    }

    /// 单个路径的 git 状态（`has_staged` 判定用；远端随 status 缓存）。
    ///
    /// `--untracked-files=all`：默认模式下未跟踪**目录**只报目录本身，
    /// 路径限定查不到里面的 `??` 条目，`is_new` 会误判为 false。
    pub fn path_status(&self, path: &str) -> Option<FileStatus> {
        let output = self
            .run_git(&[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--no-renames",
                "-z",
                "--",
                path,
            ])
            .unwrap_or_default();
        let entries = &GitStatus::parse(&output).entries;
        // 同一路径理论只有一条（untracked 与 tracked-deleted 已在解析层合并），
        // 但防御性取第一条。
        entries.first().map(|e| e.status)
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
                if let Some(cached) =
                    remote_diff_cache_get_pair(&cache_key, &head_commit, size, mtime)
                {
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
                let (old, new, unsupported) = if let Some(cached_new) = file_cache_hit {
                    // Only need HEAD via git — saves one RTT + SFTP read.
                    let old = self.head_text(&head_ref);
                    (old, cached_new, false)
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
                                DiffSideContent::Text(text) => (text, false),
                                DiffSideContent::Missing => (String::new(), false),
                                DiffSideContent::Unsupported(reason) => {
                                    (format!("<{reason}，无法显示文本差异>"), true)
                                }
                            }
                        });
                        let (old, (new, unsupported)) = (
                            h_old.join().unwrap_or_default(),
                            h_new.join().unwrap_or_default(),
                        );
                        (old, new, unsupported)
                    })
                };
                remote_diff_cache_insert_pair(
                    cache_key,
                    head_commit,
                    size,
                    mtime,
                    (old.clone(), new.clone()),
                    unsupported,
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
            session.execute_command_bytes(&command, REMOTE_GIT_TIMEOUT)
        })
        .ok()
        .filter(|r| r.success())
        .map(|r| decode_bytes_to_string(&r.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
        {
            let mut cache = head_cache().lock();
            bounded_insert_head(
                &mut cache,
                self.root.clone(),
                (Instant::now(), result.clone()),
            );
        }
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
    /// 工作区或暂存区任一侧不可按文本展示（二进制等）。
    /// diff 视图应整体只读，避免占位文本被编辑后保存覆盖二进制文件。
    pub is_binary: bool,
    /// 路径不在 index 中（未跟踪文件）。hunk 暂存补丁需 `/dev/null` 头，
    /// 且工作区侧全部内容都是“新增”。
    pub is_new: bool,
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
    ///
    /// 远端走**有界预读**（前 1MB）：探测只需前缀特征，避免为了一次探测
    /// 整体下载大文件（保存时还要再上传一次，流量翻倍）。唯一不确定的情形
    /// 是「前缀纯 ASCII 且文件被截断」——此时无法区分 UTF-8 与 GBK，回退
    /// 一次全量读保证判定与旧行为完全一致。
    pub fn detect_encoding(&self, path: &str) -> encoding::FileEncoding {
        const REMOTE_DETECT_LIMIT: usize = 1024 * 1024;
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
                let bytes = sftp_fs::read_preview_bytes(config, &remote_path, REMOTE_DETECT_LIMIT)
                    .ok()
                    .flatten();
                let Some(bytes) = bytes else {
                    return encoding::FileEncoding::Utf8;
                };
                let truncated_pure_ascii =
                    bytes.len() == REMOTE_DETECT_LIMIT && bytes.iter().all(|b| *b < 0x80);
                if truncated_pure_ascii {
                    // 前缀纯 ASCII 且被截断：编码仍不确定，全量读一次求准确。
                    sftp_fs::read_file_bytes(config, &remote_path)
                        .ok()
                        .flatten()
                        .map(|bytes| detect(&bytes))
                        .unwrap_or(encoding::FileEncoding::Utf8)
                } else {
                    detect(&bytes)
                }
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

    /// 读取工作区侧内容，并区分"文件不存在"与"无法作为文本展示"。
    fn read_file_content(&self, path: &str) -> Result<DiffSideContent, String> {
        match &self.backend {
            GitBackend::Local => {
                let file_path = Path::new(&self.root).join(path);
                let bytes = match std::fs::read(&file_path) {
                    Ok(bytes) => bytes,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(DiffSideContent::Missing);
                    }
                    // 子模块/嵌套仓库是目录（status 里已剥掉 `/` 后缀），
                    // 与二进制文件同样按"无法文本展示"处理。
                    Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => {
                        return Ok(DiffSideContent::Unsupported("子模块/嵌套仓库".to_string()));
                    }
                    Err(e) => return Err(format!("Failed to read file: {e}")),
                };
                Ok(classify_bytes(&bytes))
            }
            GitBackend::Remote { config } => {
                let remote_path = sftp_fs::join_remote_path(&self.root, path);
                // 子模块/嵌套仓库是目录（status 里已剥掉 `/` 后缀），
                // 与二进制文件同样按"无法文本展示"处理；SFTP open 目录
                // 只会报错，提前按 metadata 分流。
                if let Ok(meta) = sftp_fs::metadata(config, &remote_path)
                    && meta.kind == RemoteFileKind::Directory
                {
                    return Ok(DiffSideContent::Unsupported("子模块/嵌套仓库".to_string()));
                }
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
        self.run_git(&["commit", "--cleanup=whitespace", "-m", message])?;
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
        // 与 commit 一致的 cleanup：amend 默认同样剥 # 行。
        self.run_git(&["commit", "--amend", "--cleanup=whitespace", "-m", message])?;
        self.invalidate_status_cache();
        Ok(())
    }

    /// 上次提交的完整信息（amend 预填：用户只想改个错字时不该被迫重写全文）。
    /// 无 HEAD 时返回 None。
    pub fn last_commit_message(&self) -> Option<String> {
        let raw = self.run_git(&["log", "-1", "--pretty=%B"]).ok()?;
        let text = raw.trim_end_matches('\n').to_string();
        (!text.trim().is_empty()).then_some(text)
    }

    /// 分支同步状态：`(ahead, behind, upstream)`。
    ///
    /// ahead = 本地领先上游的提交数，behind = 落后数；无上游分支时
    /// upstream 为 None（ahead/behind 视为 0）——没有 fetch 过、纯本地
    /// 仓库都是这种正常形态，不是错误。
    pub fn branch_status(&self) -> Result<(u32, u32, Option<String>), String> {
        let upstream = self
            .run_git(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let Some(upstream_name) = upstream.as_deref().map(str::to_string) else {
            return Ok((0, 0, None));
        };

        let counts = self
            .run_git(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
            .map_err(|e| format!("Failed to get ahead/behind: {e}"))?;
        let mut parts = counts.split_whitespace();
        let ahead = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let behind = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        Ok((ahead, behind, Some(upstream_name)))
    }

    /// 贮藏当前改动（`git stash push`，可选消息）。
    /// 只贮藏跟踪文件（VSCode 默认，避免把无关未跟踪文件一起收走）。
    ///
    /// 空改动检测有两条线：status 预检是硬保证（locale 无关）；
    /// 输出匹配兜底（git 无改动时**退出码为 0**、只往 stdout 打
    /// "No local changes to save"，且该消息在非英文 locale 下有翻译）。
    pub fn stash_push(&self, message: Option<&str>) -> Result<(), String> {
        if let Ok(status) = self.status()
            && !status.iter().any(|e| !e.status.is_untracked())
        {
            return Err("没有可贮藏的更改".to_string());
        }
        let result = match message {
            Some(msg) => self.run_git(&["stash", "push", "-m", msg]),
            None => self.run_git(&["stash", "push"]),
        }
        .map_err(|e| format!("贮藏失败：{e}"))?;
        if result.contains("No local changes to save") {
            return Err("没有可贮藏的更改".to_string());
        }
        self.invalidate_status_cache();
        Ok(())
    }

    /// 弹出最近贮藏（`git stash pop`；有冲突时 git 保留贮藏并报错）。
    pub fn stash_pop(&self) -> Result<(), String> {
        self.run_git(&["stash", "pop"])
            .map_err(|e| format!("弹出贮藏失败：{e}"))?;
        self.invalidate_status_cache();
        Ok(())
    }

    /// 解决冲突：以指定侧内容覆盖工作区文件并暂存（VSCode 三选一动作）。
    ///
    /// `Both` 不能用 `git checkout`（它没有 union 选项，`--merge` 会重新
    /// 生成冲突标记），需从 index 的三个 stage（:1 基线 / :2 ours / :3
    /// theirs）经 `git merge-file --union` 做逐行并置后覆盖工作区。
    pub fn resolve_conflict(&self, path: &str, side: ConflictSide) -> Result<(), String> {
        if side == ConflictSide::Both {
            return match &self.backend {
                GitBackend::Local => self.resolve_conflict_union_local(path),
                GitBackend::Remote { config } => self.resolve_conflict_union_remote(config, path),
            };
        }
        let side_flag = match side {
            ConflictSide::Ours => "--ours",
            ConflictSide::Theirs => "--theirs",
            ConflictSide::Both => unreachable!(),
        };
        self.run_git(&["checkout", side_flag, "--", path])?;
        self.stage(&[path])?;
        Ok(())
    }

    /// 本地 union 合并：blob 直接内存传递，仅 merge-file 与结果写入落盘。
    fn resolve_conflict_union_local(&self, path: &str) -> Result<(), String> {
        // add/add 冲突无 stage 1：空基线即两侧全文并置，与 union 语义一致。
        let base = execute_local_git_bytes(&["show", &format!(":1:{path}")], &self.root)
            .unwrap_or_default();
        let ours = execute_local_git_bytes(&["show", &format!(":2:{path}")], &self.root)
            .map_err(|e| format!("无法合并两侧：{e}（可能是删除/修改冲突）"))?;
        let theirs = execute_local_git_bytes(&["show", &format!(":3:{path}")], &self.root)
            .map_err(|e| format!("无法合并两侧：{e}（可能是删除/修改冲突）"))?;

        let tag = format!(
            "deepterm-conflict-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let dir = std::env::temp_dir();
        let t_base = dir.join(format!("{tag}-base"));
        let t_ours = dir.join(format!("{tag}-ours"));
        let t_theirs = dir.join(format!("{tag}-theirs"));
        let io_err = |e: std::io::Error| format!("无法合并两侧：{e}");
        std::fs::write(&t_base, base).map_err(io_err)?;
        std::fs::write(&t_ours, ours).map_err(io_err)?;
        std::fs::write(&t_theirs, theirs).map_err(io_err)?;
        let output = git_command()
            .args(["merge-file", "-p", "--union"])
            .arg(&t_ours)
            .arg(&t_base)
            .arg(&t_theirs)
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("无法合并两侧：{e}"))?;
        let _ = std::fs::remove_file(&t_base);
        let _ = std::fs::remove_file(&t_ours);
        let _ = std::fs::remove_file(&t_theirs);
        // union 合并不产生冲突标记，正常应退出 0；个别版本把“有过冲突”
        // 报成 1，此时 stdout 仍是合并结果，一并接受。
        let merged_ok = output.status.success() || output.status.code() == Some(1);
        if !merged_ok {
            return Err(format!(
                "无法合并两侧：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let target = std::path::Path::new(&self.root).join(path);
        std::fs::write(&target, &output.stdout).map_err(io_err)?;
        self.stage(&[path])?;
        Ok(())
    }

    /// 远端 union 合并：三条 stage blob 的取出、合并、覆盖、暂存合并为
    /// 单条 shell 命令（零 SFTP 文件往返）；临时文件名带 uuid 防预建
    /// 符号链接攻击，命令结束即清理。
    fn resolve_conflict_union_remote(
        &self,
        config: &SshConnectionConfig,
        path: &str,
    ) -> Result<(), String> {
        let tag = format!(
            "deepterm-conflict-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let t_base = format!("/tmp/{tag}-base");
        let t_ours = format!("/tmp/{tag}-ours");
        let t_theirs = format!("/tmp/{tag}-theirs");
        let qpath = shell_quote(path);
        // `:2:{qpath}` 依赖 shell 相邻引号片段拼接为同一个词，路径含空格/引号都安全。
        let command = format!(
            "cd {root} && \
             (git show :1:{qpath} > {t_base} 2>/dev/null || : > {t_base}) && \
             git show :2:{qpath} > {t_ours} && \
             git show :3:{qpath} > {t_theirs} && \
             git merge-file --union {t_ours} {t_base} {t_theirs} && \
             cp {t_ours} {qpath} && \
             git add -- {qpath} && \
             rm -f {t_base} {t_ours} {t_theirs}",
            root = shell_quote(&self.root),
            qpath = qpath,
            t_base = shell_quote(&t_base),
            t_ours = shell_quote(&t_ours),
            t_theirs = shell_quote(&t_theirs),
        );
        let result = sftp_fs::with_cached_session(config, |session| {
            session.execute_command(&command, REMOTE_GIT_TIMEOUT)
        })
        .map_err(|e| format!("无法合并两侧：{e}"))?;
        if !result.success() {
            // 命令已尽力清理（未走到 rm 时残留文件名不可预测，不构成风险）。
            return Err(format!("无法合并两侧：{}", result.stderr.trim()));
        }
        self.invalidate_status_cache();
        invalidate_remote_diff_cache(&self.root, path);
        Ok(())
    }

    /// 在指定目录初始化仓库（VSCode 空态“初始化仓库”按钮）。
    pub fn init(path: &str, ssh_config: Option<&SshConnectionConfig>) -> Result<(), String> {
        invalidate_find_cache(ssh_config);
        match ssh_config {
            None => execute_local_git(&["init"], path).map(|_| ()),
            Some(config) => {
                let command = format!("cd {} && git init", shell_quote(path));
                let result = sftp_fs::with_cached_session(config, |session| {
                    session.execute_command_bytes(&command, REMOTE_GIT_TIMEOUT)
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
        self.staged_content(path).0
    }

    /// 暂存区侧内容，区分文本与不可展示（二进制）。
    /// 返回 `(text, unsupported)`。
    fn staged_content(&self, path: &str) -> (String, bool) {
        let staged_ref = format!(":{path}");
        let Ok(bytes) = self
            .backend
            .execute_bytes(&["show", &staged_ref], &self.root)
        else {
            return (String::new(), false);
        };
        if bytes.is_empty() {
            return (String::new(), false);
        }
        if sftp_fs::is_binary_bytes(&bytes) {
            return ("<二进制文件，无法显示文本差异>".to_string(), true);
        }
        (encoding::decode_bytes(&bytes).text, false)
    }

    /// 一次取齐 diff 三元组（HEAD、暂存区、工作区）与派生标志。
    ///
    /// VSCode 语义由调用方经 [`DiffBaseContents::worktree_base`] 落实。
    /// 远端五路并行（HEAD commit / 工作区 stat / index stat 预取在
    /// 一批，随后按缓存命中情况补拉）；`.git/index` 的 (size, mtime)
    /// 作为暂存层指纹，全量命中时 0 额外往返返回。
    pub fn diff_base_contents(&self, path: &str) -> Result<DiffBaseContents, String> {
        fn worktree_string(content: DiffSideContent, path: &str) -> (String, bool) {
            match content {
                DiffSideContent::Text(text) => (text, false),
                DiffSideContent::Missing => (String::new(), false),
                DiffSideContent::Unsupported(reason) => {
                    log::debug!("git diff: {path} 无法按文本展示: {reason}");
                    (format!("<{reason}，无法显示文本差异>"), true)
                }
            }
        }

        match &self.backend {
            GitBackend::Local => {
                let head_ref = format!("HEAD:{path}");
                let head = self.head_text(&head_ref);
                let (staged, staged_unsupported) = self.staged_content(path);
                let status = self.path_status(path);
                let (worktree, worktree_unsupported) =
                    worktree_string(self.read_file_content(path)?, path);
                Ok(DiffBaseContents {
                    has_staged: status.map(|s| s.has_staged_changes()).unwrap_or(false),
                    is_new: status.map(|s| s.is_untracked()).unwrap_or(false),
                    is_binary: staged_unsupported || worktree_unsupported,
                    head,
                    staged,
                    worktree,
                })
            }
            GitBackend::Remote { config } => {
                let remote_path = sftp_fs::join_remote_path(&self.root, path);
                let index_path = sftp_fs::join_remote_path(&self.root, ".git/index");
                // 第一批并行：HEAD commit、工作区 stat、index stat。
                let (head_commit, meta_res, index_meta_res) = std::thread::scope(|scope| {
                    let head =
                        scope.spawn(|| self.head_commit().unwrap_or_else(|| "no-head".into()));
                    let stat = scope.spawn(|| sftp_fs::metadata(config, &remote_path));
                    let index_stat = scope.spawn(|| sftp_fs::metadata(config, &index_path));
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
                    let index_meta_res = index_stat.join().unwrap_or_else(|_| {
                        log::warn!("git远端index stat线程异常，降级为无指纹");
                        Err(anyhow::anyhow!("index stat线程异常"))
                    });
                    (head_commit, meta_res, index_meta_res)
                });
                let (size, mtime) = meta_res
                    .map(|meta| (meta.size, meta.modified_at))
                    .unwrap_or((u64::MAX, None));
                let index_fp = index_meta_res
                    .ok()
                    .map(|meta| (meta.size, meta.modified_at));
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

                // 全量命中：三指纹一致，暂存区与单路径状态一并复用（0 额外 RTT）。
                if let Some(entry) =
                    remote_diff_cache_get_full(&cache_key, &head_commit, size, mtime, index_fp)
                {
                    let Some(staged) = entry.staged else {
                        // get_full 已保证 index 指纹命中时 staged 必为 Some，
                        // 此分支仅为类型收窄的防御。
                        return Ok(DiffBaseContents {
                            has_staged: false,
                            is_new: false,
                            is_binary: entry.worktree_unsupported,
                            head: entry.contents.0,
                            staged: String::new(),
                            worktree: entry.contents.1,
                        });
                    };
                    return Ok(DiffBaseContents {
                        has_staged: staged
                            .status
                            .map(|s| s.has_staged_changes())
                            .unwrap_or(false),
                        is_new: staged.status.map(|s| s.is_untracked()).unwrap_or(false),
                        is_binary: staged.unsupported || entry.worktree_unsupported,
                        head: entry.contents.0,
                        staged: staged.text,
                        worktree: entry.contents.1,
                    });
                }

                // (head, worktree) 命中：只补拉暂存区与单路径状态。
                let pair_hit = remote_diff_cache_get_pair(&cache_key, &head_commit, size, mtime);

                let head_ref = format!("HEAD:{path}");
                // 第二批并行：暂存区 + 状态始终要拉；(head, worktree) 未命中
                // 时再补拉两侧内容（工作区优先复用编辑器文件缓存）。
                let (staged, staged_unsupported, status, head, worktree, worktree_unsupported) =
                    std::thread::scope(|s| {
                        let h_staged = s.spawn(|| self.staged_content(path));
                        let h_status = s.spawn(|| self.path_status(path));
                        let h_pair = s.spawn(|| {
                            if let Some(pair) = pair_hit {
                                return (pair.0, pair.1, false);
                            }
                            if let Some(cached_new) = file_cache_hit {
                                return (self.head_text(&head_ref), cached_new, false);
                            }
                            let is_large = size != u64::MAX && size > 512 * 1024;
                            let h_old = s.spawn(|| self.head_text(&head_ref));
                            let h_new = s.spawn(move || {
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
                            let old = h_old.join().unwrap_or_default();
                            let (new, unsupported) = h_new.join().unwrap_or((String::new(), false));
                            (old, new, unsupported)
                        });
                        let (staged_text, staged_unsupported) =
                            h_staged.join().unwrap_or((String::new(), false));
                        let (head, worktree, worktree_unsupported) =
                            h_pair
                                .join()
                                .unwrap_or((String::new(), String::new(), false));
                        (
                            staged_text,
                            staged_unsupported,
                            h_status.join().unwrap_or(None),
                            head,
                            worktree,
                            worktree_unsupported,
                        )
                    });
                let staged_side = StagedSide {
                    text: staged,
                    unsupported: staged_unsupported,
                    status,
                };
                let has_staged = status.map(|s| s.has_staged_changes()).unwrap_or(false);
                let is_new = status.map(|s| s.is_untracked()).unwrap_or(false);
                let staged_text = staged_side.text.clone();
                let staged_unsupported = staged_side.unsupported;
                // index 指纹有效才写全量（暂存层可被后续调用复用）；
                // 否则退化为 pair-only，既有暂存层（若有）被保留。
                if index_fp.is_some() {
                    remote_diff_cache_insert_full(
                        cache_key,
                        head_commit,
                        size,
                        mtime,
                        index_fp,
                        staged_side,
                        (head.clone(), worktree.clone()),
                        worktree_unsupported,
                    );
                } else {
                    remote_diff_cache_insert_pair(
                        cache_key,
                        head_commit,
                        size,
                        mtime,
                        (head.clone(), worktree.clone()),
                        worktree_unsupported,
                    );
                }
                Ok(DiffBaseContents {
                    has_staged,
                    is_new,
                    is_binary: staged_unsupported || worktree_unsupported,
                    head,
                    staged: staged_text,
                    worktree,
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
                let mut child = git_command()
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
                // 文件名必须不可预测：可预测的名字（pid+长度）在多用户主机上
                // 可被他人预建符号链接，SFTP 写入会跟随链接覆盖任意可写文件；
                // 同进程同长度的两份补丁并发也会互相覆盖。
                let tmp = format!(
                    "/tmp/deepterm-hunk-{}-{}.patch",
                    std::process::id(),
                    uuid::Uuid::new_v4()
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
                    let _ =
                        sftp_fs::with_cached_session(config, |session| session.remove_file(&tmp));
                }
                result.map_err(|e| format!("暂存失败：{e}")).and_then(|r| {
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
    let output = git_command()
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
    let raw = execute_remote_git_bytes(config, args, cwd)?;
    // 与工作区侧同一套解码：status 里的非 UTF-8 文件名必须和文件树/编辑器
    // 的 `decode_bytes` 解法一致，否则两侧路径对不上、匹配全部失效。
    Ok(decode_bytes_to_string(&raw))
}

/// 远端 git 命令，返回原始字节（编码敏感路径：git show 仓库 blob 等）。
fn execute_remote_git_bytes(
    config: &SshConnectionConfig,
    args: &[&str],
    cwd: &str,
) -> Result<Vec<u8>, String> {
    let mut command = format!("cd {} && git", shell_quote(cwd));
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }

    let raw = sftp_fs::with_cached_session(config, |session| {
        session.execute_command_bytes(&command, REMOTE_GIT_TIMEOUT)
    })
    .map_err(|e| format!("Failed to run remote git: {e}"))?;

    if !raw.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            raw.stderr.trim()
        ));
    }

    Ok(raw.stdout)
}

fn shell_quote(s: &str) -> String {
    crate::utils::shell::shell_quote_single(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// git 子进程必须带着拼好的 `PATH` 启动，且该 `PATH` 含用户级目录。
    ///
    /// git 与语言服务器一样是**按名字**启动的，从 Finder / Dock 启动时本进程
    /// 只有 launchd 那份最小 `PATH`。`/usr/bin/git` 通常在，所以这不是必现
    /// bug；但用户只装了 Homebrew 版 git（或 git 只在 `~/.local/bin`）时不补
    /// 就是静默 `NotFound`——git 面板表现为「仓库列表空着」，没有任何报错，
    /// 排查成本极高。
    ///
    /// 断言的是**真的进了子进程环境**（`get_envs`）：漏掉 `env("PATH", …)`
    /// 这一步时，`utils::path` 的单测仍会全绿。
    #[test]
    #[cfg(unix)]
    fn git_command_carries_a_path_with_user_bin_dirs() {
        let command = git_command();

        let path = command
            .get_envs()
            .find(|(key, _)| *key == "PATH")
            .and_then(|(_, value)| value)
            .expect("git 子进程必须显式设置 PATH，否则非系统目录里的 git 找不到")
            .to_string_lossy()
            .into_owned();
        let dirs: Vec<&str> = path.split(':').collect();

        let home = dirs::home_dir().expect("测试环境必须有主目录");
        for dir in crate::utils::path::USER_BIN_DIRS {
            let expected = home.join(dir);
            assert!(
                dirs.contains(&expected.to_string_lossy().as_ref()),
                "PATH 缺 ~/{dir}：只装在那里的 git 会静默找不到，仓库列表直接空掉：{path}"
            );
        }
    }

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
        // 关掉 `autocrlf`：否则 Git for Windows 的**系统级**配置
        // （C:/Program Files/Git/etc/gitconfig，默认 true）会把提交进 blob 的
        // LF 折成 CRLF，`assert_eq!(blob, "v2\n")` 这类断言只在没装过 Git 的
        // 机器上成立。用例必须与开发机的 git 配置无关。
        let _ = Command::new("git")
            .args(["config", "core.autocrlf", "false"])
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
        assert_eq!(
            sftp_fs::join_remote_path("/", "src/main.rs"),
            "/src/main.rs"
        );
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

        // Diff 应包含该修改（HEAD ↔ 工作区）
        let (old, new) = repo.diff_file_contents("file.txt").unwrap();
        assert_eq!(old, "hello");
        assert_eq!(new, "hello world");

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
        // 未跟踪文件：HEAD 侧为空、工作区侧为全文，is_new 标记为 true。
        let base = repo.diff_base_contents("new.txt").unwrap();
        assert_eq!(base.head, "");
        assert_eq!(base.worktree, "new content\nsecond line");
        assert!(base.is_new);
        assert!(!base.has_staged);

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

        let patch =
            crate::editor::diff_model::build_hunk_patch("a.txt", &old, &new, &hunks[0], false);
        repo.stage_hunk_patch(&patch).unwrap();

        // 暂存区只有第一块，工作区两块都在。
        let staged = repo.staged_text("a.txt");
        assert!(
            staged.contains("ONE\n"),
            "staged should contain first hunk: {staged:?}"
        );
        assert!(
            staged.contains("five\nsix\n"),
            "staged should keep old tail: {staged:?}"
        );
        let status = repo.status().unwrap();
        let entry = status
            .iter()
            .find(|e| e.path == "a.txt")
            .expect("a.txt in status");
        // 部分暂存：既有已暂存也有未暂存。
        assert!(entry.status.has_staged_changes());
        assert!(entry.status.has_unstaged_changes());

        cleanup(&temp);
    }

    /// 未跟踪文件的 hunk 暂存：补丁头必须是 `/dev/null` + `new file mode`，
    /// 否则 `git apply --cached` 会拒绝应用（III.7 回归）。
    #[test]
    fn test_stage_hunk_patch_untracked_new_file() {
        let temp = temp_dir("deepterm-git-hunk-new");
        init_repo(&temp);

        std::fs::write(temp.join("new.txt"), "alpha\nBETA\ngamma\n").unwrap();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        let base = repo.diff_base_contents("new.txt").unwrap();
        assert!(base.is_new, "未跟踪文件 is_new 应为 true");

        // 空 HEAD ↔ 全文：单个“整文件新增”hunk。
        let hunks = crate::editor::diff_model::compute_hunks(&base.head, &base.worktree);
        assert_eq!(hunks.len(), 1);
        let patch = crate::editor::diff_model::build_hunk_patch(
            "new.txt",
            &base.head,
            &base.worktree,
            &hunks[0],
            base.is_new,
        );
        assert!(patch.contains("new file mode 100644"), "新文件头: {patch}");
        assert!(patch.contains("--- /dev/null"), "新文件 preimage: {patch}");
        repo.stage_hunk_patch(&patch).unwrap();

        // index 拿到整文件内容；状态变为 A（已暂存、无未暂存）。
        let staged = repo.staged_text("new.txt");
        assert_eq!(staged, "alpha\nBETA\ngamma\n");
        let status = repo.status().unwrap();
        let entry = status
            .iter()
            .find(|e| e.path == "new.txt")
            .expect("in status");
        assert!(entry.status.has_staged_changes());
        assert!(!entry.status.has_unstaged_changes());

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
            status.iter().all(|e| e.status.has_staged_changes()),
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

    /// 部分暂存场景的 hunk 补丁基线（B1 回归）：
    /// 补丁的 preimage 必须是**暂存区**内容（= `git apply --cached` 的目标），
    /// 不是 HEAD。若用 HEAD，暂存区行号与补丁行号错位，apply 可能写错行。
    #[test]
    fn test_stage_hunk_patch_partial_staged_baseline() {
        let temp = temp_dir("deepterm-git-hunk-partial");
        init_repo(&temp);

        std::fs::write(temp.join("a.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
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

        // 第 1 行改动暂存；第 5 行改动留在工作区（部分暂存）。
        std::fs::write(temp.join("a.txt"), "ONE\ntwo\nthree\nfour\nfive\n").unwrap();
        repo.stage(&["a.txt"]).unwrap();
        std::fs::write(temp.join("a.txt"), "ONE\ntwo\nthree\nfour\nFIVE\n").unwrap();

        let base = repo.diff_base_contents("a.txt").unwrap();
        assert!(base.has_staged);
        // 基线 = 暂存区（ONE 开头），而非 HEAD（one 开头）。
        assert_eq!(base.worktree_base(), "ONE\ntwo\nthree\nfour\nfive\n");

        // 以基线计算的补丁应用到 index（暂存区）必须成功，且只并入 FIVE。
        let hunks = crate::editor::diff_model::compute_hunks(base.worktree_base(), &base.worktree);
        assert_eq!(hunks.len(), 1);
        let patch = crate::editor::diff_model::build_hunk_patch(
            "a.txt",
            base.worktree_base(),
            &base.worktree,
            &hunks[0],
            false,
        );
        repo.stage_hunk_patch(&patch).unwrap();
        let staged = repo.staged_text("a.txt");
        assert_eq!(staged, "ONE\ntwo\nthree\nfour\nFIVE\n");

        cleanup(&temp);
    }

    /// 二进制文件：`is_binary` 置位，两侧给占位文本（B7 数据层回归；
    /// 视图层据此整体只读并禁止保存）。
    #[test]
    fn test_diff_base_contents_binary() {
        let temp = temp_dir("deepterm-git-binary");
        init_repo(&temp);

        // PNG 魔数头。
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0xFF; 64]);
        std::fs::write(temp.join("img.png"), &png).unwrap();
        let _ = Command::new("git")
            .args(["add", "img.png"])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&temp)
            .output();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        std::fs::write(temp.join("img.png"), &{
            let mut v = png.clone();
            v.push(0x00);
            v
        })
        .unwrap();
        let base = repo.diff_base_contents("img.png").unwrap();
        assert!(base.is_binary, "二进制文件 is_binary 必须为 true");

        cleanup(&temp);
    }

    /// 合并命令输出切分（P2 回归）：marker 定位与两段解码。
    #[test]
    fn test_status_numstat_split() {
        let status_part = "M  a.txt\0?? new.rs\0";
        let numstat_part = "2\t1\ta.txt\n";
        let mut bytes = status_part.as_bytes().to_vec();
        bytes.extend_from_slice(STATUS_NUMSTAT_MARKER);
        bytes.extend_from_slice(numstat_part.as_bytes());

        let Some(pos) = find_subslice(&bytes, STATUS_NUMSTAT_MARKER) else {
            panic!("marker must be found");
        };
        let status = decode_bytes_to_string(&bytes[..pos]);
        let numstat = decode_bytes_to_string(&bytes[pos + STATUS_NUMSTAT_MARKER.len()..]);
        assert_eq!(status, status_part);
        assert_eq!(numstat, numstat_part);

        let entries = GitStatus::parse(&status).entries;
        assert_eq!(entries.len(), 2);
        let stats = parse_numstat(&numstat);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].0, "a.txt");
        assert_eq!((stats[0].1.added, stats[0].1.deleted), (2, 1));
    }

    /// 合并命令的 shell 形态验证：marker 的 printf 转义与远端实际输出一致。
    ///
    /// 被验的是**远端** POSIX shell 的字节输出，windows 上没有 `sh`，故只在
    /// unix 上跑。
    #[test]
    #[cfg(unix)]
    fn test_status_numstat_marker_printf() {
        // printf '\0__DEEPTERM_NUMSTAT__\n' 输出的字节必须逐字节等于
        // STATUS_NUMSTAT_MARKER，否则切分永远失败（这类错误只有远端能暴露，
        // 在本地把 shell 语义固定下来）。
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(r"printf '\0__DEEPTERM_NUMSTAT__\n'")
            .output()
            .expect("sh should exist");
        assert!(output.status.success());
        assert_eq!(output.stdout.as_slice(), STATUS_NUMSTAT_MARKER);
    }

    /// 提交/修正：`--cleanup=whitespace` 保留消息中的 `#` 行；
    /// last_commit_message 往返一致。
    #[test]
    fn test_commit_cleanup_and_last_message() {
        let temp = temp_dir("deepterm-git-commit-cleanup");
        init_repo(&temp);
        std::fs::write(temp.join("a.txt"), "a\n").unwrap();
        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        repo.stage(&["a.txt"]).unwrap();

        let message = "标题\n# 这行以 # 开头，不该被剥掉\n\n尾随空白   \n";
        repo.commit(message).unwrap();
        assert_eq!(
            repo.last_commit_message().unwrap(),
            "标题\n# 这行以 # 开头，不该被剥掉\n\n尾随空白",
            "cleanup=whitespace 只去尾随空白，保留 # 行与内部空行"
        );

        repo.commit_amend("amend 标题\n# amend 的 # 行").unwrap();
        assert_eq!(
            repo.last_commit_message().unwrap(),
            "amend 标题\n# amend 的 # 行"
        );

        cleanup(&temp);
    }

    /// branch_status：无上游 (0,0,None)；配好上游后 ahead/behind 正确。
    /// 上游用真实 remote（`remote add` + `push -u`）构造——`@{upstream}`
    /// 读的是本地 remote-tracking ref，另一侧的推进要靠 `fetch` 才可见。
    #[test]
    fn test_branch_status() {
        let temp = temp_dir("deepterm-git-branch-status");
        init_repo(&temp);
        let upstream = temp_dir("deepterm-git-branch-status-up");
        let up_clone = temp_dir("deepterm-git-branch-status-clone");
        let _ = std::fs::create_dir_all(&upstream);
        // 显式指定裸库的初始分支：不指定时 git 用 `init.defaultBranch`
        // （未配置就是 `master`），而下面推的是 `refs/heads/main`。
        // 裸库 HEAD 指错分支时 clone 会得到另一个分支名，上游那次 push
        // 就落到别的 ref 上，`behind` 永远是 0——与实现无关的环境陷阱。
        let _ = Command::new("git")
            .args([
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                upstream.to_str().unwrap(),
            ])
            .output();

        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");

        // 无上游：不是错误。
        assert_eq!(repo.branch_status().unwrap(), (0, 0, None));

        std::fs::write(temp.join("base.txt"), "base\n").unwrap();
        repo.stage(&["base.txt"]).unwrap();
        repo.commit("base").unwrap();
        let _ = Command::new("git")
            .args(["remote", "add", "origin", upstream.to_str().unwrap()])
            .current_dir(&temp)
            .output();
        let _ = Command::new("git")
            .args(["push", "--quiet", "-u", "origin", "HEAD:refs/heads/main"])
            .current_dir(&temp)
            .output();

        // 本地领先 1。
        std::fs::write(temp.join("local.txt"), "local\n").unwrap();
        repo.stage(&["local.txt"]).unwrap();
        repo.commit("local ahead").unwrap();
        let (ahead, behind, upstream_name) = repo.branch_status().unwrap();
        assert_eq!(ahead, 1, "本地应领先 1");
        assert_eq!(behind, 0);
        assert_eq!(upstream_name.unwrap(), "origin/main");

        // 上游领先 1：另一份 clone 推进后 fetch 回来。
        let _ = Command::new("git")
            .args([
                "clone",
                "--quiet",
                upstream.to_str().unwrap(),
                up_clone.to_str().unwrap(),
            ])
            .output();
        std::fs::write(up_clone.join("up.txt"), "up\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&up_clone)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "upstream ahead", "--quiet"])
            .current_dir(&up_clone)
            .output();
        let _ = Command::new("git")
            .args(["push", "--quiet", "origin", "HEAD:refs/heads/main"])
            .current_dir(&up_clone)
            .output();
        let _ = Command::new("git")
            .args(["fetch", "--quiet", "origin"])
            .current_dir(&temp)
            .output();

        let (ahead, behind, upstream_name) = repo.branch_status().unwrap();
        assert_eq!(ahead, 1, "本地仍领先 1");
        assert_eq!(behind, 1, "fetch 后本地应落后 1");
        assert_eq!(upstream_name.unwrap(), "origin/main");

        cleanup(&temp);
        cleanup(&upstream);
        cleanup(&up_clone);
    }

    /// stash 往返：push 后状态干净、log 无新提交；pop 后改动回来。
    #[test]
    fn test_stash_roundtrip() {
        let temp = temp_dir("deepterm-git-stash");
        init_repo(&temp);
        std::fs::write(temp.join("a.txt"), "base\n").unwrap();
        let repo = GitRepository::find(&temp, true, None)
            .unwrap()
            .expect("should find repo");
        repo.stage(&["a.txt"]).unwrap();
        repo.commit("base").unwrap();

        std::fs::write(temp.join("a.txt"), "modified\n").unwrap();
        repo.stash_push(Some("测试贮藏消息")).unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.join("a.txt")).unwrap(),
            "base\n",
            "push 后工作区应回到 HEAD"
        );
        let status = repo.status().unwrap();
        assert!(
            status.is_empty(),
            "push 后 status 应为空（stash 不产生跟踪文件改动）"
        );

        repo.stash_pop().unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.join("a.txt")).unwrap(),
            "modified\n",
            "pop 后改动应回来"
        );

        // 无改动时 stash push 报错（git 原样透传错误）。
        repo.stage(&["a.txt"]).unwrap();
        repo.commit("save modified").unwrap();
        assert!(repo.stash_push(None).is_err());

        cleanup(&temp);
    }

    /// resolve_conflict：真实 merge 冲突下三种解决方式。
    #[test]
    fn test_resolve_conflict() {
        for side in [ConflictSide::Ours, ConflictSide::Theirs, ConflictSide::Both] {
            let temp = temp_dir("deepterm-git-conflict");
            init_repo(&temp);
            let repo = GitRepository::find(&temp, true, None)
                .unwrap()
                .expect("should find repo");

            std::fs::write(temp.join("f.txt"), "line1\nline2\nline3\n").unwrap();
            repo.stage(&["f.txt"]).unwrap();
            repo.commit("base").unwrap();

            let git = |args: &[&str]| {
                let out = Command::new("git")
                    .args(args)
                    .current_dir(&temp)
                    .output()
                    .expect("git");
                assert!(
                    out.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            };
            // 基线在默认分支（不假设其名字），feature 分支与默认分支改同一行。
            git(&["checkout", "--quiet", "-b", "feature"]);
            std::fs::write(temp.join("f.txt"), "line1\nFEATURE\nline3\n").unwrap();
            git(&["add", "."]);
            git(&["commit", "--quiet", "-m", "feature"]);
            git(&["checkout", "--quiet", "-"]);
            std::fs::write(temp.join("f.txt"), "line1\nMAIN\nline3\n").unwrap();
            git(&["add", "."]);
            git(&["commit", "--quiet", "-m", "main"]);
            let merge_out = Command::new("git")
                .args(["merge", "feature"])
                .current_dir(&temp)
                .output()
                .expect("git");
            assert!(!merge_out.status.success(), "必须制造出真实冲突");

            repo.resolve_conflict("f.txt", side).unwrap();
            let content = std::fs::read_to_string(temp.join("f.txt")).unwrap();
            let expected = match side {
                ConflictSide::Ours => "line1\nMAIN\nline3\n",
                ConflictSide::Theirs => "line1\nFEATURE\nline3\n",
                ConflictSide::Both => "line1\nMAIN\nFEATURE\nline3\n",
            };
            assert_eq!(content, expected, "{side:?} 解决结果");
            // 已无未合并条目，且解决结果确实进入 index（stage 0 的 blob
            // 内容即工作区内容；Ours 侧与 HEAD 相同属正常——merge 未产生
            // 新提交）。
            let unmerged = Command::new("git")
                .args(["ls-files", "-u"])
                .current_dir(&temp)
                .output()
                .unwrap();
            assert!(
                unmerged.stdout.is_empty(),
                "{side:?} 解决后不应有未合并条目"
            );
            let indexed = Command::new("git")
                .args(["show", ":f.txt"])
                .current_dir(&temp)
                .output()
                .unwrap();
            assert!(
                indexed.status.success(),
                "{side:?} 解决后 index 中必须有 f.txt"
            );
            assert_eq!(
                String::from_utf8_lossy(&indexed.stdout),
                expected,
                "{side:?} index 内容必须等于解决结果"
            );

            cleanup(&temp);
        }
    }
}
