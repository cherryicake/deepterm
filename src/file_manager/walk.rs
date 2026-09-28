//! 项目级文件遍历层。
//!
//! 项目级搜索（侧栏「搜索」）与模糊文件查找器的**共同前置**：
//! 两者都需要「把工作区里所有应当被检索的文件列出来」，而这个能力此前
//! 完全不存在——文件树的 `read_dir` 是逐目录懒加载，只服务 UI 展开。
//!
//! 用 `ignore` crate 而不是 `walkdir` 自己拼：忽略规则的语义比看上去复杂
//! （父目录的 `.gitignore` 约束所有子目录、`!` 白名单反选、`.git/info/exclude`、
//! 全局 gitignore、`.ignore` 优先级高于 `.gitignore`），手写必然出错。
//!
//! 遍历在后台线程跑，结果通过 channel 流式回传：大仓库（Linux 内核量级）
//! 首次全量遍历可能几百毫秒到数秒，一次性收集再返回会让 UI 卡住且无法
//! 边遍历边出结果。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

/// 遍历产出的一条文件记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// 绝对路径。
    pub path: PathBuf,
    /// 相对遍历根的路径，用于展示与模糊匹配（永远用 `/` 分隔，
    /// 保证跨平台匹配逻辑一致）。
    pub rel_path: String,
}

impl FileEntry {
    /// 文件名（不含目录）。
    pub fn file_name(&self) -> &str {
        self.rel_path.rsplit('/').next().unwrap_or(&self.rel_path)
    }

    /// 所在目录的相对路径（顶层文件返回空串）。
    pub fn parent_dir(&self) -> &str {
        match self.rel_path.rfind('/') {
            Some(ix) => &self.rel_path[..ix],
            None => "",
        }
    }
}

/// 遍历配置。
#[derive(Clone, Debug)]
pub struct WalkOptions {
    /// 是否尊重 `.gitignore` / `.ignore` / 全局 gitignore。默认 true。
    pub respect_ignore_files: bool,
    /// 是否包含隐藏文件（点开头）。默认 false（与 Zed/VSCode 一致）。
    pub include_hidden: bool,
    /// 额外排除的 glob（相对根路径），如 `target/**`。
    pub exclude_globs: Vec<String>,
    /// 只包含的 glob；非空时其余全部排除。
    pub include_globs: Vec<String>,
    /// 是否跟随符号链接。默认 false（避免链接环导致无限遍历）。
    pub follow_links: bool,
    /// 最大遍历深度（相对根）。None = 不限。
    pub max_depth: Option<usize>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            respect_ignore_files: true,
            include_hidden: false,
            exclude_globs: Vec::new(),
            include_globs: Vec::new(),
            follow_links: false,
            max_depth: None,
        }
    }
}

impl WalkOptions {
    /// 项目级搜索用的默认配置。
    pub fn for_search() -> Self {
        Self::default()
    }

    /// 模糊文件查找器用的配置：与搜索一致，但不受 include/exclude 限制。
    pub fn for_file_finder() -> Self {
        Self::default()
    }

    pub fn exclude_globs(mut self, globs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exclude_globs = globs.into_iter().map(Into::into).collect();
        self
    }

    pub fn include_globs(mut self, globs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.include_globs = globs.into_iter().map(Into::into).collect();
        self
    }
}

/// 遍历取消令牌。
///
/// 用户改一次搜索词就会重开一轮遍历，旧的那轮必须能停下来——否则
/// 在几十万文件的仓库里连续输入会把线程池占满。
#[derive(Clone, Default)]
pub struct WalkCancellation {
    cancelled: Arc<AtomicBool>,
}

impl WalkCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// 单次遍历的最大文件数上限。
///
/// 防御性上限：monorepo / 误把 `/` 当根时会产出百万级条目，全部驻留内存
/// 会把应用拖垮。到达上限即停止遍历并标记 `truncated`。
pub const MAX_WALK_ENTRIES: usize = 200_000;

/// 遍历结果。
#[derive(Debug, Default)]
pub struct WalkOutcome {
    pub entries: Vec<FileEntry>,
    /// 是否因为达到 [`MAX_WALK_ENTRIES`] 或出错而提前结束。
    pub truncated: bool,
}

/// 同步遍历一个根目录，收集所有应当被检索的文件。
///
/// **必须在后台线程调用**（大仓库会阻塞数百毫秒以上）。
/// 需要流式/可取消时用 [`walk_streaming`]。
pub fn walk(root: &Path, options: &WalkOptions) -> WalkOutcome {
    let mut outcome = WalkOutcome::default();
    walk_streaming(root, options, &WalkCancellation::new(), |entry| {
        outcome.entries.push(entry);
        outcome.entries.len() < MAX_WALK_ENTRIES
    });
    outcome.truncated = outcome.entries.len() >= MAX_WALK_ENTRIES;
    outcome
}

/// 流式遍历：每发现一个文件就回调一次。
///
/// 回调返回 `false` 表示停止遍历（调用方已收集够、或用户已取消）。
/// 回调在遍历线程上同步执行，**不要在里面做重活**。
pub fn walk_streaming<F>(
    root: &Path,
    options: &WalkOptions,
    cancel: &WalkCancellation,
    mut on_file: F,
) where
    F: FnMut(FileEntry) -> bool,
{
    let mut builder = WalkBuilder::new(root);
    builder
        // 忽略规则族：`.gitignore` / `.ignore` / `.git/info/exclude` / 全局。
        .git_ignore(options.respect_ignore_files)
        .git_global(options.respect_ignore_files)
        .git_exclude(options.respect_ignore_files)
        .ignore(options.respect_ignore_files)
        .parents(options.respect_ignore_files)
        // 隐藏文件默认跳过：与 Zed 的 file finder 行为一致。
        .hidden(!options.include_hidden)
        .follow_links(options.follow_links)
        // 单线程：遍历本身是 IO 密集，调用方已经在后台线程上，
        // 再开线程池只会和 UI 抢 CPU；需要并行时在调用侧按目录分片。
        .threads(1);

    if let Some(depth) = options.max_depth {
        builder.max_depth(Some(depth));
    }

    // include/exclude glob：include 非空时先全部排除再按 include 放行。
    if !options.include_globs.is_empty() || !options.exclude_globs.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        if !options.include_globs.is_empty() {
            // `ignore` 的 overrides 是「先 whitelist 后 blacklist」语义：
            // 空 whitelist 表示全排除，所以 include 非空时必须显式列白名单。
            for glob in &options.include_globs {
                if overrides.add(glob).is_err() {
                    // 用户输入了非法 glob：忽略该条而不是整轮失败。
                    log::debug!("忽略非法 include glob: {glob}");
                }
            }
        }
        for glob in &options.exclude_globs {
            // `!` 前缀在 OverrideBuilder 里表示「排除」。
            let pattern = format!("!{glob}");
            if overrides.add(&pattern).is_err() {
                log::debug!("忽略非法 exclude glob: {glob}");
            }
        }
        if let Ok(overrides) = overrides.build() {
            builder.overrides(overrides);
        }
    }

    let walker = builder.build();

    for result in walker {
        if cancel.is_cancelled() {
            return;
        }
        let entry = match result {
            Ok(entry) => entry,
            // 单个目录读不了（权限、竞态删除）不应中断整轮遍历。
            Err(err) => {
                log::debug!("遍历跳过条目: {err}");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel_path = match path.strip_prefix(root) {
            Ok(rel) => to_slash(rel),
            // 根自身或符号链接解析到根之外：用完整路径兜底。
            Err(_) => to_slash(path),
        };
        if !on_file(FileEntry {
            path: path.to_path_buf(),
            rel_path,
        }) {
            return;
        }
    }
}

/// 把路径转成 `/` 分隔的字符串（Windows 上 `\` → `/`）。
fn to_slash(path: &Path) -> String {
    let s = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        s.into_owned()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deepterm-walk-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn walks_files_and_reports_relative_slash_paths() {
        let dir = tmpdir("basic");
        write(&dir.join("a.rs"), "fn a() {}");
        write(&dir.join("sub/b.rs"), "fn b() {}");

        let outcome = walk(&dir, &WalkOptions::for_search());
        let mut rels: Vec<_> = outcome.entries.iter().map(|e| e.rel_path.clone()).collect();
        rels.sort();
        assert_eq!(rels, vec!["a.rs", "sub/b.rs"]);
        assert!(!outcome.truncated);

        let b = outcome
            .entries
            .iter()
            .find(|e| e.rel_path == "sub/b.rs")
            .unwrap();
        assert_eq!(b.file_name(), "b.rs");
        assert_eq!(b.parent_dir(), "sub");
        assert!(b.path.is_absolute());
    }

    #[test]
    fn respects_gitignore() {
        let dir = tmpdir("gitignore");
        // `ignore` 只在 git 仓库内应用 .gitignore，先建 .git。
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        write(&dir.join(".gitignore"), "ignored.rs\nbuild/\n");
        write(&dir.join("kept.rs"), "");
        write(&dir.join("ignored.rs"), "");
        write(&dir.join("build/out.rs"), "");

        let outcome = walk(&dir, &WalkOptions::for_search());
        let rels: Vec<_> = outcome.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&"kept.rs"), "应保留 kept.rs，实际 {rels:?}");
        assert!(!rels.contains(&"ignored.rs"), "应忽略 ignored.rs");
        assert!(
            !rels.iter().any(|r| r.starts_with("build/")),
            "应忽略 build/ 目录"
        );
    }

    #[test]
    fn gitignore_can_be_disabled() {
        let dir = tmpdir("no-ignore");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        write(&dir.join(".gitignore"), "ignored.rs\n");
        write(&dir.join("ignored.rs"), "");

        let options = WalkOptions {
            respect_ignore_files: false,
            ..WalkOptions::for_search()
        };
        let outcome = walk(&dir, &options);
        let rels: Vec<_> = outcome.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&"ignored.rs"), "关闭忽略后应能看到，实际 {rels:?}");
    }

    #[test]
    fn hidden_files_skipped_by_default_and_included_on_request() {
        let dir = tmpdir("hidden");
        write(&dir.join("visible.rs"), "");
        write(&dir.join(".hidden.rs"), "");

        let outcome = walk(&dir, &WalkOptions::for_search());
        let rels: Vec<_> = outcome.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&"visible.rs"));
        assert!(!rels.contains(&".hidden.rs"), "默认应跳过隐藏文件");

        let options = WalkOptions {
            include_hidden: true,
            ..WalkOptions::for_search()
        };
        let outcome = walk(&dir, &options);
        let rels: Vec<_> = outcome.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&".hidden.rs"), "显式开启后应能看到");
    }

    #[test]
    fn exclude_globs_filter_results() {
        let dir = tmpdir("exclude");
        write(&dir.join("src/a.rs"), "");
        write(&dir.join("target/b.rs"), "");

        let options = WalkOptions::for_search().exclude_globs(["target/**"]);
        let outcome = walk(&dir, &options);
        let rels: Vec<_> = outcome.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&"src/a.rs"), "实际 {rels:?}");
        assert!(!rels.iter().any(|r| r.starts_with("target/")), "实际 {rels:?}");
    }

    #[test]
    fn include_globs_restrict_results() {
        let dir = tmpdir("include");
        write(&dir.join("a.rs"), "");
        write(&dir.join("b.py"), "");

        let options = WalkOptions::for_search().include_globs(["*.rs"]);
        let outcome = walk(&dir, &options);
        let rels: Vec<_> = outcome.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&"a.rs"), "实际 {rels:?}");
        assert!(!rels.contains(&"b.py"), "include 非空时应排除其余，实际 {rels:?}");
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let dir = tmpdir("cancel");
        for i in 0..50 {
            write(&dir.join(format!("f{i}.rs")), "");
        }
        let cancel = WalkCancellation::new();
        let mut seen = 0usize;
        walk_streaming(&dir, &WalkOptions::for_search(), &cancel, |_| {
            seen += 1;
            // 第 5 个之后请求取消。
            if seen == 5 {
                cancel.cancel();
            }
            true
        });
        assert_eq!(seen, 5, "取消后不应继续产出，实际 {seen}");
    }

    #[test]
    fn callback_returning_false_stops_the_walk() {
        let dir = tmpdir("stop");
        for i in 0..20 {
            write(&dir.join(format!("f{i}.rs")), "");
        }
        let mut seen = 0usize;
        walk_streaming(&dir, &WalkOptions::for_search(), &WalkCancellation::new(), |_| {
            seen += 1;
            seen < 3
        });
        assert_eq!(seen, 3);
    }

    #[test]
    fn empty_directory_yields_nothing() {
        let dir = tmpdir("empty");
        let outcome = walk(&dir, &WalkOptions::for_search());
        assert!(outcome.entries.is_empty());
        assert!(!outcome.truncated);
    }

    #[test]
    fn nonexistent_root_does_not_panic() {
        let dir = tmpdir("missing").join("does-not-exist");
        let outcome = walk(&dir, &WalkOptions::for_search());
        assert!(outcome.entries.is_empty());
    }
}
