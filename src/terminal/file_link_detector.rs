//! File-path inference for terminal `Cmd+Click`.
//!
//! The terminal only knows grid characters, not file semantics. This module
//! turns a viewport row's text into plausible file candidates (`ls` output,
//! compiler `file:line:col`, `grep -n` output). Callers resolve candidates
//! against the shell cwd (`PathResolver`) and validate with the filesystem
//! before navigating, so false positives never open anything.
//!
//! Two rules keep the candidate list small enough that the filesystem check is
//! the *last* word instead of the only one:
//!
//! - **形态先筛**：URI 一律排除；`.`/`..`/`~` 这类「到处都能解析」的裸标记
//!   不算路径；纯 ASCII 的裸词必须有像样的扩展名（或在无扩展名白名单里），
//!   否则正文里的 `src`/`target`/`index` 会因为恰好是当前目录下的同名条目
//!   而全变成链接。
//! - **命中区按裁剪结果算**：括号、引号、图标这类被剥掉的前后缀不参与
//!   命中区；宽字符占两格，右半格同样可点（见 `row_text`）。

use crate::terminal::row_text::WrappedText;
use crate::terminal::terminal::IndexedCell;

/// A file candidate found in terminal text.
#[derive(Clone, Debug, PartialEq)]
pub struct FileHit {
    /// Original token text as shown (may include `:line:col` suffix).
    pub raw: String,
    /// Path portion with quotes unescaped and `:line:col` stripped.
    pub path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Row index inside the text passed to `detect_files_in_rows` (soft-wrap
    /// continuations get their own index, so a wrapped path is hittable on
    /// both of its halves).
    pub row: usize,
    /// Column range in terminal grid coordinates (end exclusive).
    pub start_col: usize,
    pub end_col: usize,
}

/// Trailing punctuation that terminates a path but is often glued to it in
/// prose output (`(main.rs)`, `see foo.ts,`, `"bar.rs".`).
const TRAILING_PUNCT: &[char] = &[
    ')', ']', '}', '>', '"', '\'', ',', ';', ':', '.', '!', '?', '、', '，', '。', '；', '：',
    '）', '」', '』',
];

/// Leading openers mirrored by `TRAILING_PUNCT` (`(src/main.rs)`,
/// `[foo.ts]:12`, `<bar.rs>`, `'baz.rs'`).
const LEADING_OPENERS: &[char] = &['(', '[', '{', '<', '"', '\'', '‘', '“', '（', '「', '『'];

/// Returns true for Nerd-Font / icon glyphs (`eza --icons`, `ls --icon`)
/// that prefix filenames. Private-use area + common symbol blocks.
fn is_icon_noise(ch: char) -> bool {
    matches!(ch, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
        || matches!(
            ch,
            '📁' | '📄' | '📦' | '🦀' | '\u{F680}' | '├' | '└' | '│' | '─'
        )
        || ('\u{2500}'..='\u{257F}').contains(&ch)
}

/// 裸词里几乎不会出现、出现即说明这是正文/命令而非路径的字符。
const NOT_PATH_CHARS: &[char] = &[
    '%', '=', '*', '^', '$', '&', '|', ';', '<', '>', '?', '!', '`', ',', '\'', '"', '\\', '+',
    '@', '「', '」', '『', '』', '，', '。', '、', '；', '：', '！', '？', '（', '）',
];

/// 无扩展名但确实是文件的常见名字（大小写不敏感）。
///
/// 刻意**不含** `src` / `target` / `index` / `main`：它们既是 `ls` 输出里的
/// 目录名，也是终端正文里最高频的普通词，按「谁更常出现在正文」取舍。
const NO_EXTENSION_ALLOWLIST: &[&str] = &[
    "makefile",
    "readme",
    "license",
    "licence",
    "copying",
    "notice",
    "authors",
    "changelog",
    "changes",
    "contributing",
    "codeowners",
    "dockerfile",
    "procfile",
    "justfile",
    "rakefile",
    "gemfile",
    "brewfile",
    "podfile",
    "vagrantfile",
    "jenkinsfile",
    "caddyfile",
    "cargo.lock",
    "cargo.toml",
];

/// Strip `:line` / `:line:col` suffix. Returns `(path, line, col)`.
/// Unlike the first version of `PathCompletionEngine::parse_line_column`,
/// a non-numeric tail is never dropped: `a:b` stays `a:b`.
///
/// 这里**不设数值上限**：`grep -n` / 编译器诊断会给出六位以上的行号
/// （生成代码、日志文件），一刀切会把这些输出打成不可点的残片；时间戳
/// 之类的误报由调用方的形态检查（`looks_like_path`）负责拒掉。
pub fn split_line_column(raw: &str) -> (String, Option<u32>, Option<u32>) {
    // Find up to two trailing `:digits` segments.
    let mut rest = raw;
    let mut suffixes: Vec<u32> = Vec::new();
    for _ in 0..2 {
        let Some(idx) = rest.rfind(':') else {
            break;
        };
        let tail = &rest[idx + 1..];
        // Empty tail (`foo:`) or non-numeric tail stops stripping.
        if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        // Windows drive prefix (`C:\...`) must not be treated as line number.
        if rest.len() <= 3 && rest.as_bytes()[1] == b':' {
            break;
        }
        let Ok(num) = tail.parse::<u32>() else {
            break;
        };
        if num == 0 {
            break;
        }
        suffixes.push(num);
        rest = &rest[..idx];
    }
    if rest.is_empty() {
        return (raw.to_string(), None, None);
    }
    match suffixes.len() {
        0 => (raw.to_string(), None, None),
        1 => (rest.to_string(), Some(suffixes[0]), None),
        _ => {
            let (line, col) = (suffixes[1], suffixes[0]);
            if col == 0 {
                (raw.to_string(), None, None)
            } else {
                (rest.to_string(), Some(line), Some(col))
            }
        }
    }
}

/// 剥掉尾部标点，返回裁剪后的相对结束偏移（开区间）。
fn trim_trailing(mut end: usize, s: &str) -> usize {
    for ch in s[..end].chars().rev() {
        if TRAILING_PUNCT.contains(&ch) {
            end -= ch.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// 剥掉前导图标与开括号，返回裁剪后的相对起始偏移。
fn trim_leading(mut start: usize, s: &str) -> usize {
    for ch in s[start..].chars() {
        if is_icon_noise(ch) || ch == '*' || LEADING_OPENERS.contains(&ch) {
            start += ch.len_utf8();
        } else {
            break;
        }
    }
    start
}

/// token 是否带 URI scheme（`https://`、`ssh://`、`git@…`…）。
///
/// `C:\…` / `c:/…` 是 Windows 路径，不是 URI。
fn has_uri_scheme(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[1] == b':'
        && bytes[0].is_ascii_alphabetic()
        && matches!(bytes[2], b'/' | b'\\')
    {
        return false;
    }
    let Some(idx) = path.find(':') else {
        return false;
    };
    let scheme = &path[..idx];
    !scheme.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// 扩展名是否像文件名扩展名（`rs`/`toml`/`gz`），而不是 `1.2.3` 里的 `3`
/// 或 `file.2024` 里的年份。
fn has_plausible_extension(path: &str) -> bool {
    let Some((stem, ext)) = path.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= 10
        && ext.starts_with(|c: char| c.is_alphabetic())
        && ext
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '+' || c == '-')
}

/// Whether a cleaned token is worth a filesystem check.
fn looks_like_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 {
        return false;
    }
    // 任何 URI 形态都交给链接识别，文件解析器不掺和。
    if has_uri_scheme(path) {
        return false;
    }
    // `git@host:owner/repo.git` 这类远端地址本地永远不存在，别浪费一次 stat。
    if let Some(at) = path.find('@')
        && path[..at]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return false;
    }
    // `.` / `..` / `~` 在几乎每个 cwd 下都能解析：一旦算路径，终端里到处
    // 都是「点一下就打开当前目录」的假链接。
    if matches!(path, "." | ".." | "~" | "./" | "../" | ".//") {
        return false;
    }
    // 显式前缀：绝对路径 / 家目录 / 显式相对。
    if path.starts_with('/')
        || path.starts_with("~/")
        || path.starts_with("./")
        || path.starts_with("../")
    {
        return true;
    }
    // Windows 盘符：`C:\Users\me\a.rs`
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[1] == b':'
        && bytes[0].is_ascii_alphabetic()
        && matches!(bytes[2], b'/' | b'\\')
    {
        return true;
    }
    // 含分隔符的交给下游存在性检查裁决：`src/terminal` 会中，正文里的
    // `and/or`、`输入/输出` 不会（对应路径不存在）。
    if path.contains('/') {
        return true;
    }
    // 裸词。文件名里不会出现这些符号；出现即说明这是正文/命令。
    if path.chars().any(|c| NOT_PATH_CHARS.contains(&c)) || path.len() > 255 {
        return false;
    }
    let Some(first) = path.chars().next() else {
        return false;
    };
    // 隐藏文件 / 目录：`.gitignore`、`.cargo`
    if first == '.' {
        return path.len() > 1;
    }
    // 非 ASCII 裸词（中文、日文文件名）放行：这些文字没有词间空格，正文里
    // 的整句都会被切成一个长 token，靠下游存在性检查过滤即可；而它们又几乎
    // 不可能是「纯 ASCII 目录名」那种高频误报。
    if !path.is_ascii() {
        return first.is_alphanumeric();
    }
    // ASCII 裸词：必须看起来像文件名，否则 `src`/`target`/`index` 这类
    // 正文高频词会因为「恰好存在」而变成链接。
    if !(first.is_alphabetic() || first == '_') {
        return false;
    }
    has_plausible_extension(path)
        || NO_EXTENSION_ALLOWLIST
            .iter()
            .any(|w| path.eq_ignore_ascii_case(w))
}

/// Parse plain text (already joined, e.g. with soft-wrap continuation) into
/// candidates with byte ranges.
pub fn parse_file_candidates(text: &str) -> Vec<(usize, usize, String)> {
    // Tokenize respecting single/double quotes and backslash escapes.
    // A quote only opens quoting when a matching close exists ahead;
    // otherwise it is a literal (e.g. the apostrophe in
    // `don't forget src/main.rs` must not swallow the path).
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace. 只按 ASCII 空白断词：非 ASCII 字节（≥0x80）一律
        // 不是空白——多字节字符的续字节里可能有 0x85（NEL）/0xA0（NBSP），
        // 按 `char::is_whitespace`（字节转 char）判会从 CJK 词中间断开 token。
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let tok_start = i;
        let mut in_single = false;
        let mut in_double = false;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            // 已在引用内：配对引号无条件闭合；未在引用内：仅当远处存在
            // 闭合引号时才开启（`don't` 的撇号视为普通字符）。
            if b == b'\'' && !in_double && (in_single || has_close_quote(bytes, i, b'\'')) {
                in_single = !in_single;
                i += 1;
                continue;
            }
            if b == b'"' && !in_single && (in_double || has_close_quote(bytes, i, b'"')) {
                in_double = !in_double;
                i += 1;
                continue;
            }
            if bytes[i].is_ascii_whitespace() && !in_single && !in_double {
                break;
            }
            i += 1;
        }
        let tok_end = i;
        if tok_end > tok_start {
            if let Ok(tok) = std::str::from_utf8(&bytes[tok_start..tok_end]) {
                out.push((tok_start, tok_end, tok.to_string()));
            }
        } else {
            // Defensive: never stall on the same byte.
            i += 1;
        }
    }
    out
}

/// Whether the quote at `pos` has a matching close ahead (bounded scan).
/// Quotes are ASCII so byte indexing is safe.
fn has_close_quote(bytes: &[u8], pos: usize, quote: u8) -> bool {
    // Bound the lookahead: paths longer than this are rejected downstream
    // (`looks_like_path`), so no need to scan further.
    const MAX_LOOKAHEAD: usize = 2048;
    let end = (pos + 1 + MAX_LOOKAHEAD).min(bytes.len());
    let mut j = pos + 1;
    while j < end {
        if bytes[j] == b'\\' {
            j += 2;
            continue;
        }
        if bytes[j] == quote {
            return true;
        }
        j += 1;
    }
    false
}

/// Detect file candidates in one viewport row's cells.
pub fn detect_files_in_line(cells: &[IndexedCell]) -> Vec<FileHit> {
    detect_files_in_rows([cells])
}

/// Detect file candidates in a row and (optionally) its soft-wrap
/// continuations. A candidate that spans a wrap yields one hit per row, so
/// either half of a long path is clickable.
pub fn detect_files_in_rows<'a>(rows: impl IntoIterator<Item = &'a [IndexedCell]>) -> Vec<FileHit> {
    let joined = WrappedText::from_rows(rows);
    if joined.text.trim().is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (bstart, _, tok) in parse_file_candidates(&joined.text) {
        hits.extend(to_file_hits(&tok, bstart, &joined));
    }
    hits
}

/// 把一个 token 变成命中，列区间按**裁剪后**的字节窗口计算。
///
/// `c_start`/`c_end` 始终是相对 `tok` 起点的字节偏移：引号、图标、包裹
/// 括号、`:` 行号后缀每剥掉一层就同步收缩窗口，所以点不到多余符号。
fn to_file_hits(tok: &str, bstart: usize, joined: &WrappedText) -> Vec<FileHit> {
    let mut c_start = 0usize;
    let mut c_end = tok.len();

    // 剥掉成对引号：`"my file.ts"`
    if tok.len() >= 2 {
        let b = tok.as_bytes();
        if (b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\'') {
            c_start += 1;
            c_end -= 1;
        }
    }
    // 剥图标与包裹括号（`(src/main.rs)`），最多 4 轮：前后配对逐层收敛。
    for _ in 0..4 {
        let start = trim_leading(c_start, tok);
        let end = trim_trailing(c_end, tok);
        if start == c_start && end == c_end {
            break;
        }
        c_start = start;
        c_end = end;
    }
    if c_start >= c_end {
        return Vec::new();
    }

    // 拆 `:line:col` 后缀并收掉窗口尾部。
    let window = &tok[c_start..c_end];
    let (path, line, col) = split_line_column(window);
    if path.len() < window.len() {
        c_end = c_start + path.len();
    }
    // `[foo.ts]:12` 的 `]` 在首轮够不着，拆出行列号后再剥一次。
    c_end = trim_trailing(c_end, tok);
    if c_start >= c_end {
        return Vec::new();
    }
    let path = &tok[c_start..c_end];
    if !looks_like_path(path) {
        return Vec::new();
    }
    // 反转义 `\ `（shell 转义空格）只影响路径值，不影响命中区。
    let path = path.replace("\\ ", " ");

    joined
        .span_rows(bstart + c_start, bstart + c_end)
        .into_iter()
        .map(|(row, start_col, end_col)| FileHit {
            raw: tok.to_string(),
            path: path.clone(),
            line,
            col,
            row,
            start_col,
            end_col,
        })
        .collect()
}

/// Find the candidate under a grid column of a specific row.
pub fn file_hit_at_col(hits: &[FileHit], row: usize, col: usize) -> Option<&FileHit> {
    hits.iter()
        .find(|h| h.row == row && col >= h.start_col && col < h.end_col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_line_column() {
        assert_eq!(
            split_line_column("src/main.rs:12:3"),
            ("src/main.rs".to_string(), Some(12), Some(3))
        );
        assert_eq!(
            split_line_column("a.rs:10"),
            ("a.rs".to_string(), Some(10), None)
        );
        assert_eq!(split_line_column("a:b"), ("a:b".to_string(), None, None));
        assert_eq!(split_line_column("foo:"), ("foo:".to_string(), None, None));
    }

    #[test]
    fn timestamp_and_version_tokens_never_become_links() {
        // 时间戳/端口：`:30` 会被当成行号，但剩下的路径部分必须被形态筛掉
        let (path, ..) = split_line_column("12:30:45");
        assert!(!looks_like_path(&path), "{path} must not be a path");
        let (path, ..) = split_line_column("0.0.0.0:8080");
        assert!(!looks_like_path(&path), "{path} must not be a path");
        // 版本号
        assert!(!looks_like_path("1.2.3"));
        // 行号从 1 开始：`foo:0` 不拆，整体带 scheme 形态同样被排除
        assert_eq!(
            split_line_column("foo:0"),
            ("foo:0".to_string(), None, None)
        );
        assert!(!looks_like_path("foo:0"));
    }

    #[test]
    fn large_line_and_column_numbers_still_parse() {
        // 六位以上行号（生成代码 / 日志文件的 `grep -n` 输出）曾经撞上限被
        // 拆错或整段不可拆；上限移除后必须完整解析。
        assert_eq!(
            split_line_column("src/gen.cc:150342:10"),
            ("src/gen.cc".to_string(), Some(150342), Some(10))
        );
        assert_eq!(
            split_line_column("src/gen.cc:120:20000"),
            ("src/gen.cc".to_string(), Some(120), Some(20000))
        );
        assert_eq!(
            split_line_column("app.log:1234567"),
            ("app.log".to_string(), Some(1234567), None)
        );
    }

    #[test]
    fn cjk_continuation_bytes_do_not_break_tokens() {
        // `呅`（U+5445）的最后一个 UTF-8 字节是 0x85：按「字节转 char」判空白
        // 会把它当成 NEL 换行，从词中间切断 token（整词甚至因非法 UTF-8 被丢）。
        let toks: Vec<String> = parse_file_candidates("呅呅 foo.rs")
            .into_iter()
            .map(|(_, _, tok)| tok)
            .collect();
        assert_eq!(toks, vec!["呅呅".to_string(), "foo.rs".to_string()]);
    }

    #[test]
    fn parses_ls_tokens() {
        let toks = parse_file_candidates("src/ foo.rs  Cargo.toml");
        assert_eq!(toks.len(), 3);
    }

    #[test]
    fn parses_quoted_spaces() {
        let toks = parse_file_candidates("\"my file.ts\" other.rs");
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0].2, "\"my file.ts\"");
    }

    #[test]
    fn filters_urls() {
        assert!(!looks_like_path("https://example.com/a"));
        assert!(!looks_like_path("ssh://git@host/repo.git"));
        assert!(!looks_like_path("git@github.com:zed/z.git"));
        assert!(looks_like_path("./src/main.rs"));
        assert!(looks_like_path("/abs/path"));
        assert!(looks_like_path("~/x"));
        assert!(looks_like_path("foo.rs"));
        assert!(looks_like_path("src/main.rs"));
        assert!(looks_like_path("Cargo.toml"));
        assert!(looks_like_path(".gitignore"));
        assert!(looks_like_path("C:\\Users\\me\\a.rs"));
    }

    #[test]
    fn rejects_prose_words_and_markers() {
        // 高频正文词不再因为「恰好是当前目录下的条目」而变成链接
        for word in ["src", "target", "index", "main", "error", "lib"] {
            assert!(!looks_like_path(word), "{word} should not be a path");
        }
        // 版本号、IP、纯数字
        for tok in ["1.2.3", "0.0.0.0", "2024", "12:30"] {
            assert!(!looks_like_path(tok), "{tok} should not be a path");
        }
        // 到处都能解析的裸标记
        for tok in [".", "..", "~", "./", "../"] {
            assert!(!looks_like_path(tok), "{tok} should not be a path");
        }
        // 命令与 URL 残片
        for tok in ["cargo", "rm", "--verbose", "https:"] {
            assert!(!looks_like_path(tok), "{tok} should not be a path");
        }
    }

    #[test]
    fn separator_bearing_tokens_defer_to_existence_check() {
        // 含 `/` 的候选交给下游存在性检查裁决：`src/terminal` 是真路径，
        // 正文里的 `and/or` 对应文件不存在，不会被打开。
        assert!(looks_like_path("src/terminal"));
        assert!(looks_like_path("and/or"));
        assert!(!looks_like_path("git@github.com:zed/z.git"));
    }

    #[test]
    fn accepts_allowlisted_and_non_ascii_bare_words() {
        assert!(looks_like_path("Makefile"));
        assert!(looks_like_path("dockerfile"));
        assert!(looks_like_path("文档"));
        assert!(looks_like_path("src/terminal"));
    }

    #[test]
    fn trims_punct_with_offsets() {
        let s = "(main.rs),";
        assert_eq!(&s[trim_leading(0, s)..trim_trailing(s.len(), s)], "main.rs");
    }

    #[test]
    fn apostrophe_does_not_swallow_path() {
        // `don't` 的 `'` 无配对闭合引号，必须视为普通字符。
        let toks = parse_file_candidates("don't forget src/main.rs");
        assert!(toks.iter().any(|(_, _, t)| t == "src/main.rs"));
    }
}
