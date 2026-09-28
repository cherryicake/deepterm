//! 项目级搜索：跨文件查找（含正则/大小写/全字）与结果聚合。
//!
//! 遍历层复用 [`crate::file_manager::walk`]，匹配层复用单文件查找的语义
//! （同样的 `regex` / `aho-corasick` 组合），保证「单文件查找」与
//! 「项目查找」对同一 pattern 的判定一致——两套正则方言是最容易出
//! 不一致 bug 的地方。
//!
//! 并发模型：遍历是 IO 密集、匹配是 CPU 密集，这里把两者放在**同一条
//! 后台线程**上串行执行，而不是开线程池。理由：
//! - 调用方（gpui 的 `background_executor`）本身已有线程池，再嵌套一层
//!   会与 UI 抢 CPU；
//! - 单线程实现让「取消」语义简单可靠（只需在每个文件边界检查一次），
//!   并行版本要么用原子计数、要么用 channel 收尾，出错面大得多；
//! - 实测项目规模（十万文件量级）下，串行匹配的瓶颈是磁盘读而非 CPU。

use std::ops::Range;
use std::path::Path;

use aho_corasick::AhoCorasick;

use crate::file_manager::walk::{WalkCancellation, WalkOptions, walk_streaming};

/// 单个文件的最大读取体积。超过则跳过并计入 `skipped_large`。
///
/// 搜索是「读全文再匹配」，与编辑器打开文件（20MB 上限）不同：
/// 这里可能一次扫几万个文件，单个超大文件（日志、压缩包解出的数据）
/// 会独占内存且几乎不可能命中，直接跳过更划算。
pub const MAX_SEARCH_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 单个文件的最大命中数。防御 minified 文件（一行几十万字符）刷爆结果集。
pub const MAX_MATCHES_PER_FILE: usize = 1_000;

/// 全局最大命中数上限。
pub const MAX_TOTAL_MATCHES: usize = 20_000;

/// 匹配模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SearchMode {
    /// 字面量子串（默认）。
    #[default]
    Literal,
    /// 正则表达式。
    Regex,
}

/// 项目搜索选项。
#[derive(Clone, Debug)]
pub struct ProjectSearchOptions {
    pub query: String,
    pub case_insensitive: bool,
    pub whole_word: bool,
    pub mode: SearchMode,
    /// 只在这些 glob 匹配的文件里搜（空 = 全部）。
    pub include_globs: Vec<String>,
    /// 排除这些 glob 匹配的文件。
    pub exclude_globs: Vec<String>,
    /// 是否搜索被 .gitignore 忽略的文件。
    pub include_ignored: bool,
}

impl Default for ProjectSearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            case_insensitive: true,
            whole_word: false,
            mode: SearchMode::Literal,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            include_ignored: false,
        }
    }
}

impl ProjectSearchOptions {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Default::default()
        }
    }

    /// 编译匹配器。查询为空或正则非法时返回 `None`，
    /// 调用方据此区分「无查询」与「查询非法」。
    pub fn build_matcher(&self) -> Option<ProjectMatcher> {
        ProjectMatcher::new(self)
    }
}

/// 已编译的匹配器。
///
/// 字面量与正则两条路径，语义与单文件查找（`input/search.rs`）对齐：
/// 全字在字面量模式下走字符边界过滤（比转正则快，且 CJK 行为一致），
/// 在正则模式下包一层 `\b(?:...)\b`。
pub struct ProjectMatcher {
    inner: MatcherInner,
    /// 字面量全字：保留 Aho-Corasick 扫描，命中后按字符边界过滤。
    literal_whole_word: bool,
}

enum MatcherInner {
    Literal(AhoCorasick),
    Regex(regex::Regex),
}

impl ProjectMatcher {
    pub fn new(options: &ProjectSearchOptions) -> Option<Self> {
        if options.query.is_empty() {
            return None;
        }
        match options.mode {
            SearchMode::Regex => {
                let expr = if options.whole_word {
                    format!(r"\b(?:{})\b", options.query)
                } else {
                    options.query.clone()
                };
                let expr = if options.case_insensitive {
                    format!("(?i){expr}")
                } else {
                    expr
                };
                // 非法正则 → None（调用方标记错误态），不 panic。
                regex::Regex::new(&expr)
                    .ok()
                    .map(|re| Self {
                        inner: MatcherInner::Regex(re),
                        literal_whole_word: false,
                    })
            }
            SearchMode::Literal => {
                let ac = AhoCorasick::builder()
                    .ascii_case_insensitive(options.case_insensitive)
                    .build([options.query.as_str()])
                    .ok()?;
                Some(Self {
                    inner: MatcherInner::Literal(ac),
                    literal_whole_word: options.whole_word,
                })
            }
        }
    }

    /// 在 `text` 中找出所有命中区间（字节偏移）。
    ///
    /// `limit` 为单个文件的命中上限，防止 minified 文件刷爆结果集。
    pub fn find_all(&self, text: &str, limit: usize) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        match &self.inner {
            MatcherInner::Literal(ac) => {
                for m in ac.find_iter(text) {
                    if out.len() >= limit {
                        break;
                    }
                    if self.literal_whole_word && !is_word_bounded(text, m.start(), m.end()) {
                        continue;
                    }
                    out.push(m.start()..m.end());
                }
            }
            MatcherInner::Regex(re) => {
                for m in re.find_iter(text) {
                    if out.len() >= limit {
                        break;
                    }
                    // 零宽匹配（如 `^`）不产出结果：无法定位也无法高亮。
                    if m.start() == m.end() {
                        continue;
                    }
                    out.push(m.start()..m.end());
                }
            }
        }
        out
    }
}

/// 命中的字符两侧是否为词边界（Unicode 词定义，与同词高亮一致）。
fn is_word_bounded(text: &str, start: usize, end: usize) -> bool {
    let before_ok = text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !is_word_char(c));
    let after_ok = text[end..].chars().next().is_none_or(|c| !is_word_char(c));
    before_ok && after_ok
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 一个文件内的命中。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatch {
    /// 文件绝对路径。
    pub path: std::path::PathBuf,
    /// 相对搜索根的路径（`/` 分隔）。
    pub rel_path: String,
    /// 该文件内的命中行。
    pub lines: Vec<MatchLine>,
    /// 是否因为达到单文件上限而截断。
    pub truncated: bool,
}

/// 一条命中行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchLine {
    /// 1 起的行号（面向用户展示）。
    pub line_number: usize,
    /// 整行内容（不含换行符，超长时已截断）。
    pub line_text: String,
    /// 命中在该行内的**字符列**区间（0 起，半开）。
    ///
    /// 用字符列而不是字节偏移：渲染侧要按列做高亮切片，
    /// 字节偏移在 CJK/emoji 行上会切错。
    pub columns: Vec<Range<usize>>,
}

/// 搜索结果。
#[derive(Clone, Debug, Default)]
pub struct ProjectSearchOutcome {
    pub files: Vec<FileMatch>,
    /// 命中的总行数。
    pub total_lines: usize,
    /// 是否因为全局上限或取消而提前结束。
    pub truncated: bool,
    /// 因超过 [`MAX_SEARCH_FILE_BYTES`] 而跳过的文件数。
    pub skipped_large: usize,
    /// 扫描过的文件数（用于「已扫描 N 个文件」进度展示）。
    pub scanned_files: usize,
}

impl ProjectSearchOutcome {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

/// 单行展示的最大字符数。超过则截断（长行在结果列表里无法阅读，
/// 且会把 UI 布局撑坏）。
const MAX_LINE_DISPLAY_CHARS: usize = 400;

/// 在 `root` 下执行项目级搜索。
///
/// **必须在后台线程调用**。`cancel` 用于用户在结果返回前改词/关闭面板时
/// 中止本轮——每个文件边界检查一次，粒度足够且无锁。
pub fn search_project(
    root: &Path,
    options: &ProjectSearchOptions,
    cancel: &WalkCancellation,
) -> ProjectSearchOutcome {
    let mut outcome = ProjectSearchOutcome::default();
    let Some(matcher) = options.build_matcher() else {
        return outcome;
    };

    let walk_options = WalkOptions {
        // `include_ignored` 为真时关闭忽略规则族。
        respect_ignore_files: !options.include_ignored,
        include_globs: options.include_globs.clone(),
        exclude_globs: options.exclude_globs.clone(),
        ..WalkOptions::for_search()
    };

    walk_streaming(root, &walk_options, cancel, |entry| {
        if cancel.is_cancelled() {
            return false;
        }
        outcome.scanned_files += 1;

        // 体积预检：避免把 1GB 日志读进内存。
        if let Ok(meta) = std::fs::metadata(&entry.path) {
            if meta.len() > MAX_SEARCH_FILE_BYTES {
                outcome.skipped_large += 1;
                return true;
            }
        }

        let bytes = match std::fs::read(&entry.path) {
            Ok(bytes) => bytes,
            // 竞态删除/权限：跳过该文件，不中断整轮。
            Err(err) => {
                log::debug!("搜索跳过 {}: {err}", entry.rel_path);
                return true;
            }
        };
        // 二进制文件：含 NUL 字节即判定为二进制。用 memchr 而不是
        // `str::from_utf8` 全量校验——后者对每个文件都是 O(字节数) 的
        // 额外一遍扫描，而 NUL 检测在绝大多数文本文件上前几个字节就结束。
        if memchr::memchr(0, &bytes).is_some() {
            return true;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            // 非 UTF-8（GBK 等）：跳过。编码探测在搜索路径上成本过高，
            // 且命中结果的行列映射会失真。
            return true;
        };

        let ranges = matcher.find_all(text, MAX_MATCHES_PER_FILE);
        if ranges.is_empty() {
            return true;
        }

        let mut lines: Vec<MatchLine> = Vec::new();
        let mut truncated = ranges.len() >= MAX_MATCHES_PER_FILE;

        // 按行切分并归类命中。`line_start` 是当前行的字节起点，
        // 逐行推进避免为每个命中都做一次「从头数换行」。
        let mut line_start = 0usize;
        let mut line_number = 1usize;
        let mut range_ix = 0usize;
        while line_start <= text.len() {
            let line_end = text[line_start..]
                .find('\n')
                .map(|ix| line_start + ix)
                .unwrap_or(text.len());
            // 收集落在本行的命中。
            let mut columns: Vec<Range<usize>> = Vec::new();
            while range_ix < ranges.len() && ranges[range_ix].start < line_end {
                let r = &ranges[range_ix];
                let (s, e) = (r.start.max(line_start), r.end.min(line_end));
                if s < e {
                    // 字节偏移 → 字符列。行内截断后列号会失真，所以先算列
                    // 再截断文本，且只保留落在展示范围内的命中。
                    let start_col = text[line_start..s].chars().count();
                    let end_col = start_col + text[s..e].chars().count();
                    if start_col < MAX_LINE_DISPLAY_CHARS {
                        columns.push(start_col..end_col.min(MAX_LINE_DISPLAY_CHARS));
                    }
                }
                range_ix += 1;
            }

            if !columns.is_empty() {
                let raw = &text[line_start..line_end];
                let line_text: String = raw.chars().take(MAX_LINE_DISPLAY_CHARS).collect();
                let was_cut = raw.chars().count() > MAX_LINE_DISPLAY_CHARS;
                truncated |= was_cut;
                lines.push(MatchLine {
                    line_number,
                    line_text,
                    columns,
                });
                outcome.total_lines += 1;
            }

            if line_end >= text.len() {
                break;
            }
            line_start = line_end + 1;
            line_number += 1;
        }

        if !lines.is_empty() {
            outcome.files.push(FileMatch {
                path: entry.path,
                rel_path: entry.rel_path,
                lines,
                truncated,
            });
        }

        // 全局上限：达到即停止遍历（结果集已远超可浏览规模）。
        outcome.total_lines < MAX_TOTAL_MATCHES
    });

    outcome.truncated = outcome.truncated
        || outcome.total_lines >= MAX_TOTAL_MATCHES
        || cancel.is_cancelled();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deepterm-psearch-{name}-{}-{}",
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

    fn run(root: &Path, options: &ProjectSearchOptions) -> ProjectSearchOutcome {
        search_project(root, options, &WalkCancellation::new())
    }

    #[test]
    fn finds_literal_matches_across_files_with_line_numbers() {
        let dir = tmpdir("literal");
        write(&dir.join("a.rs"), "fn alpha() {}\nfn beta() {}\n");
        write(&dir.join("sub/b.rs"), "// alpha here\n");

        let outcome = run(&dir, &ProjectSearchOptions::new("alpha"));
        assert_eq!(outcome.file_count(), 2, "{:?}", outcome.files);
        assert_eq!(outcome.total_lines, 2);

        let a = outcome
            .files
            .iter()
            .find(|f| f.rel_path == "a.rs")
            .expect("a.rs 命中");
        assert_eq!(a.lines.len(), 1);
        assert_eq!(a.lines[0].line_number, 1);
        assert_eq!(a.lines[0].line_text, "fn alpha() {}");
        // "fn " 是 3 列 → alpha 从第 3 列开始，长 5。
        assert_eq!(a.lines[0].columns, vec![3..8]);
    }

    #[test]
    fn reports_multiple_matches_on_same_line_as_separate_columns() {
        let dir = tmpdir("multi-col");
        write(&dir.join("a.txt"), "x x x\n");
        let outcome = run(&dir, &ProjectSearchOptions::new("x"));
        let f = &outcome.files[0];
        assert_eq!(f.lines.len(), 1);
        assert_eq!(f.lines[0].columns, vec![0..1, 2..3, 4..5]);
    }

    #[test]
    fn case_sensitivity_is_respected() {
        let dir = tmpdir("case");
        write(&dir.join("a.txt"), "Alpha\nalpha\n");

        let insensitive = ProjectSearchOptions::new("alpha");
        assert_eq!(run(&dir, &insensitive).total_lines, 2);

        let sensitive = ProjectSearchOptions {
            case_insensitive: false,
            ..ProjectSearchOptions::new("alpha")
        };
        assert_eq!(run(&dir, &sensitive).total_lines, 1);
    }

    #[test]
    fn whole_word_filters_substrings() {
        let dir = tmpdir("whole-word");
        write(&dir.join("a.txt"), "cat\nconcatenate\ncat.\n");
        let options = ProjectSearchOptions {
            whole_word: true,
            ..ProjectSearchOptions::new("cat")
        };
        let outcome = run(&dir, &options);
        assert_eq!(outcome.total_lines, 2, "{:?}", outcome.files);
        let lines = &outcome.files[0].lines;
        assert_eq!(lines[0].line_number, 1);
        // "cat." 里 cat 后是标点，算词边界。
        assert_eq!(lines[1].line_number, 3);
    }

    #[test]
    fn regex_mode_matches_patterns() {
        let dir = tmpdir("regex");
        write(&dir.join("a.rs"), "let x1 = 1;\nlet y2 = 2;\nlet z = 3;\n");
        let options = ProjectSearchOptions {
            mode: SearchMode::Regex,
            query: r"let [a-z]\d".to_string(),
            ..Default::default()
        };
        let outcome = run(&dir, &options);
        assert_eq!(outcome.total_lines, 2, "{:?}", outcome.files);
    }

    #[test]
    fn invalid_regex_yields_no_matcher_and_no_results() {
        let dir = tmpdir("bad-regex");
        write(&dir.join("a.rs"), "anything\n");
        let options = ProjectSearchOptions {
            mode: SearchMode::Regex,
            query: "(".to_string(),
            ..Default::default()
        };
        assert!(options.build_matcher().is_none());
        let outcome = run(&dir, &options);
        assert!(outcome.is_empty());
    }

    #[test]
    fn empty_query_returns_nothing() {
        let dir = tmpdir("empty-query");
        write(&dir.join("a.rs"), "content\n");
        let outcome = run(&dir, &ProjectSearchOptions::new(""));
        assert!(outcome.is_empty());
        assert_eq!(outcome.scanned_files, 0, "空查询不应触发遍历");
    }

    #[test]
    fn skips_binary_files() {
        let dir = tmpdir("binary");
        std::fs::write(dir.join("bin.dat"), b"needle\x00needle").unwrap();
        write(&dir.join("text.txt"), "needle\n");

        let outcome = run(&dir, &ProjectSearchOptions::new("needle"));
        assert_eq!(outcome.file_count(), 1, "二进制文件应被跳过");
        assert_eq!(outcome.files[0].rel_path, "text.txt");
    }

    #[test]
    fn skips_files_over_size_limit() {
        let dir = tmpdir("large");
        // 直接造一个超过上限的文件（稀疏内容，写入快）。
        let big = dir.join("big.txt");
        let content = "needle\n".repeat((MAX_SEARCH_FILE_BYTES as usize / 7) + 10);
        std::fs::write(&big, content).unwrap();
        write(&dir.join("small.txt"), "needle\n");

        let outcome = run(&dir, &ProjectSearchOptions::new("needle"));
        assert_eq!(outcome.skipped_large, 1);
        assert_eq!(outcome.file_count(), 1);
        assert_eq!(outcome.files[0].rel_path, "small.txt");
    }

    #[test]
    fn cjk_columns_are_character_based_not_byte_based() {
        let dir = tmpdir("cjk");
        // 「中文」各占 3 字节；"中文abc" 中 abc 的字符列是 2..5。
        write(&dir.join("a.txt"), "中文abc\n");
        let outcome = run(&dir, &ProjectSearchOptions::new("abc"));
        let line = &outcome.files[0].lines[0];
        assert_eq!(line.columns, vec![2..5], "列号必须按字符而非字节");
    }

    #[test]
    fn respects_gitignore_by_default_and_can_include_ignored() {
        let dir = tmpdir("ignored");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        write(&dir.join(".gitignore"), "ignored.txt\n");
        write(&dir.join("ignored.txt"), "needle\n");
        write(&dir.join("kept.txt"), "needle\n");

        let outcome = run(&dir, &ProjectSearchOptions::new("needle"));
        assert_eq!(outcome.file_count(), 1);
        assert_eq!(outcome.files[0].rel_path, "kept.txt");

        let options = ProjectSearchOptions {
            include_ignored: true,
            ..ProjectSearchOptions::new("needle")
        };
        let outcome = run(&dir, &options);
        assert_eq!(outcome.file_count(), 2, "{:?}", outcome.files);
    }

    #[test]
    fn include_globs_restrict_search() {
        let dir = tmpdir("globs");
        write(&dir.join("a.rs"), "needle\n");
        write(&dir.join("b.py"), "needle\n");
        let options = ProjectSearchOptions {
            include_globs: vec!["*.rs".to_string()],
            ..ProjectSearchOptions::new("needle")
        };
        let outcome = run(&dir, &options);
        assert_eq!(outcome.file_count(), 1);
        assert_eq!(outcome.files[0].rel_path, "a.rs");
    }

    #[test]
    fn long_lines_are_truncated_without_breaking_columns() {
        let dir = tmpdir("longline");
        let long = "a".repeat(MAX_LINE_DISPLAY_CHARS + 100);
        write(&dir.join("a.txt"), &format!("needle{long}\n"));
        let outcome = run(&dir, &ProjectSearchOptions::new("needle"));
        let line = &outcome.files[0].lines[0];
        assert!(line.line_text.chars().count() <= MAX_LINE_DISPLAY_CHARS);
        assert_eq!(line.columns, vec![0..6]);
    }

    #[test]
    fn cancellation_marks_outcome_truncated() {
        let dir = tmpdir("cancel");
        for i in 0..30 {
            write(&dir.join(format!("f{i}.txt")), "needle\n");
        }
        let cancel = WalkCancellation::new();
        cancel.cancel();
        let outcome = search_project(&dir, &ProjectSearchOptions::new("needle"), &cancel);
        assert!(outcome.truncated, "取消后应标记截断");
    }

    #[test]
    fn clean_tree_with_no_matches_reports_scanned_count() {
        let dir = tmpdir("nomatch");
        write(&dir.join("a.txt"), "nothing here\n");
        write(&dir.join("b.txt"), "nor here\n");
        let outcome = run(&dir, &ProjectSearchOptions::new("needle"));
        assert!(outcome.is_empty());
        assert_eq!(outcome.scanned_files, 2);
    }
}
