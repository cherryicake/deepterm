//! 项目级文件遍历层。
//!
//! 侧栏「搜索」（按文件名筛选项目文件）的前置：需要「把工作区里所有
//! 应当被检索的文件列出来」，而这个能力此前完全不存在——文件树的
//! `read_dir` 是逐目录懒加载，只服务 UI 展开。
//!
//! 用 `ignore` crate 而不是 `walkdir` 自己拼：忽略规则的语义比看上去复杂
//! （父目录的 `.gitignore` 约束所有子目录、`!` 白名单反选、`.git/info/exclude`、
//! 全局 gitignore、`.ignore` 优先级高于 `.gitignore`），手写必然出错。

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// 遍历产出的一条文件记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// 绝对路径。
    pub path: PathBuf,
    /// 相对遍历根的路径（永远用 `/` 分隔，保证跨平台匹配逻辑一致）。
    pub rel_path: String,
}

impl FileEntry {
    /// 文件名（不含目录）。
    pub fn file_name(&self) -> &str {
        self.rel_path.rsplit('/').next().unwrap_or(&self.rel_path)
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
    /// 是否因为达到 [`MAX_WALK_ENTRIES`] 而提前结束。
    pub truncated: bool,
}

/// 同步遍历一个根目录，收集所有应当被检索的文件。
///
/// **必须在后台线程调用**（大仓库会阻塞数百毫秒以上）。
pub fn walk(root: &Path) -> WalkOutcome {
    let mut outcome = WalkOutcome::default();
    let walker = WalkBuilder::new(root)
        // 忽略规则族：`.gitignore` / `.ignore` / `.git/info/exclude` / 全局。
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        // 隐藏文件默认跳过：与 Zed 的 file finder 行为一致。
        .hidden(true)
        // 单线程：遍历本身是 IO 密集，调用方已经在后台线程上，
        // 再开线程池只会和 UI 抢 CPU。
        .threads(1)
        .build();

    for result in walker {
        if outcome.entries.len() >= MAX_WALK_ENTRIES {
            outcome.truncated = true;
            break;
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
        outcome.entries.push(FileEntry {
            path: path.to_path_buf(),
            rel_path,
        });
    }
    outcome
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

        let outcome = walk(&dir);
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

        let outcome = walk(&dir);
        let rels: Vec<_> = outcome
            .entries
            .iter()
            .map(|e| e.rel_path.as_str())
            .collect();
        assert!(rels.contains(&"kept.rs"), "应保留 kept.rs，实际 {rels:?}");
        assert!(!rels.contains(&"ignored.rs"), "应忽略 ignored.rs");
        assert!(
            !rels.iter().any(|r| r.starts_with("build/")),
            "应忽略 build/ 目录"
        );
    }

    #[test]
    fn hidden_files_are_skipped() {
        let dir = tmpdir("hidden");
        write(&dir.join("visible.rs"), "");
        write(&dir.join(".hidden.rs"), "");

        let outcome = walk(&dir);
        let rels: Vec<_> = outcome
            .entries
            .iter()
            .map(|e| e.rel_path.as_str())
            .collect();
        assert!(rels.contains(&"visible.rs"));
        assert!(!rels.contains(&".hidden.rs"), "默认应跳过隐藏文件");
    }

    #[test]
    fn empty_directory_yields_nothing() {
        let dir = tmpdir("empty");
        let outcome = walk(&dir);
        assert!(outcome.entries.is_empty());
        assert!(!outcome.truncated);
    }

    #[test]
    fn nonexistent_root_does_not_panic() {
        let dir = tmpdir("missing").join("does-not-exist");
        let outcome = walk(&dir);
        assert!(outcome.entries.is_empty());
    }
}
