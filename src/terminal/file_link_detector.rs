//! File-path inference for terminal `Cmd+Click`.
//!
//! The terminal only knows grid characters, not file semantics. This module
//! turns a viewport row's text into plausible file candidates (`ls` output,
//! compiler `file:line:col`, `grep -n` output). Callers resolve candidates
//! against the shell cwd (`PathResolver`) and validate with the filesystem
//! before navigating, so false positives never open anything.

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

/// Returns true for Nerd-Font / icon glyphs (`eza --icons`, `ls --icon`)
/// that prefix filenames. Private-use area + common symbol blocks.
fn is_icon_noise(ch: char) -> bool {
    matches!(ch, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
        || matches!(
            ch,
            '📁' | '📄' | '📦' | '🦀' | '' | '├' | '└' | '│' | '─'
        )
        || ('\u{2500}'..='\u{257F}').contains(&ch)
}

/// Strip `:line` / `:line:col` suffix. Returns `(path, line, col)`.
/// Unlike the first version of `PathCompletionEngine::parse_line_column`,
/// a non-numeric tail is never dropped: `a:b` stays `a:b`.
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
        suffixes.push(num);
        rest = &rest[..idx];
    }
    if rest.is_empty() {
        return (raw.to_string(), None, None);
    }
    match suffixes.len() {
        0 => (raw.to_string(), None, None),
        1 => (rest.to_string(), Some(suffixes[0]), None),
        _ => (rest.to_string(), Some(suffixes[1]), Some(suffixes[0])),
    }
}

fn trim_trailing_punct(s: &str) -> &str {
    let mut end = s.len();
    for (idx, ch) in s.char_indices().rev() {
        if TRAILING_PUNCT.contains(&ch) {
            end = idx;
        } else {
            break;
        }
    }
    &s[..end]
}

fn trim_leading_noise(s: &str) -> &str {
    let mut start = 0;
    for (idx, ch) in s.char_indices() {
        if is_icon_noise(ch) || ch == '*' {
            start = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    &s[start..]
}

/// Leading openers mirrored by `TRAILING_PUNCT` (`(src/main.rs)`,
/// `[foo.ts]:12`, `<bar.rs>`, `'baz.rs'`).
const LEADING_OPENERS: &[char] = &['(', '[', '{', '<', '"', '\'', '‘', '“', '（', '「', '『'];

fn trim_leading_openers(s: &str) -> &str {
    let mut start = 0;
    for (idx, ch) in s.char_indices() {
        if LEADING_OPENERS.contains(&ch) {
            start = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    &s[start..]
}

/// Whether a cleaned token is worth a filesystem check.
fn looks_like_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.") {
        return false;
    }
    // Absolute / home / explicit relative prefixes are always candidates.
    if path.starts_with('/')
        || path.starts_with("~/")
        || path == "~"
        || path.starts_with("./")
        || path.starts_with("../")
    {
        return path.len() > 1 || path == "~";
    }
    // Otherwise require a slash, a dot-extension, or a bare word that could
    // be an `ls` entry (validated by existence check downstream).
    if path.contains('/') {
        return true;
    }
    if path.starts_with('.') && path.len() > 1 {
        return true;
    }
    // `name.ext`, `Makefile`, `src`, `foo-bar_baz` — reject pure prose.
    let mut chars = path.chars();
    let first = chars.next().unwrap_or('\0');
    if !(first.is_alphanumeric() || first == '_' || first == '.' || first == '~') {
        return false;
    }
    if path.contains('.') {
        return true;
    }
    // Bare word (possible directory from `ls`): allow, existence check decides.
    path.len() <= 255 && !path.contains(char::is_whitespace)
}

/// Parse plain text (already joined, e.g. with soft-wrap continuation) into
/// candidates with byte ranges. `byte_to_col` maps each byte to a grid column
/// for the *first* row; bytes beyond the first row reuse the last column and
/// are filtered by the caller via `first_row_bytes`.
pub fn parse_file_candidates(text: &str) -> Vec<(usize, usize, String)> {
    // Tokenize respecting single/double quotes and backslash escapes.
    // A quote only opens quoting when a matching close exists ahead;
    // otherwise it is a literal (e.g. the apostrophe in
    // `don't forget src/main.rs` must not swallow the path).
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
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
            if b == b'\'' && !in_double {
                if in_single || has_close_quote(bytes, i, b'\'') {
                    in_single = !in_single;
                    i += 1;
                    continue;
                }
            } else if b == b'"' && !in_single {
                if in_double || has_close_quote(bytes, i, b'"') {
                    in_double = !in_double;
                    i += 1;
                    continue;
                }
            }
            if (b as char).is_whitespace() && !in_single && !in_double {
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
    let mut text = String::new();
    let mut byte_to_col: Vec<usize> = Vec::new();
    for cell in cells {
        let ch = cell.cell.c;
        if ch == '\0' {
            continue;
        }
        let col = cell.point.column.0;
        let len = ch.len_utf8();
        for _ in 0..len {
            byte_to_col.push(col);
        }
        text.push(ch);
    }
    if text.trim().is_empty() {
        return Vec::new();
    }
    map_candidates(&text, &byte_to_col)
}

/// Detect in joined text (for soft-wrapped rows). `first_row_bytes` is the
/// byte length of the first physical row; only hits overlapping it are kept
/// so hover on row N never highlights row N+1's text.
pub fn detect_files_in_joined_text(
    joined: &str,
    first_row_byte_to_col: &[usize],
    first_row_bytes: usize,
) -> Vec<FileHit> {
    let mut hits = Vec::new();
    for (bstart, bend, tok) in parse_file_candidates(joined) {
        if bstart >= first_row_bytes {
            continue;
        }
        let clamped_end = bend.min(first_row_bytes);
        if clamped_end <= bstart {
            continue;
        }
        if let Some(hit) = to_file_hit(&tok, bstart, clamped_end, first_row_byte_to_col) {
            hits.push(hit);
        }
    }
    hits
}

fn map_candidates(text: &str, byte_to_col: &[usize]) -> Vec<FileHit> {
    let mut hits = Vec::new();
    for (bstart, bend, tok) in parse_file_candidates(text) {
        if let Some(hit) = to_file_hit(&tok, bstart, bend, byte_to_col) {
            hits.push(hit);
        }
    }
    hits
}

fn to_file_hit(
    tok: &str,
    bstart: usize,
    bend: usize,
    byte_to_col: &[usize],
) -> Option<FileHit> {
    // Strip surrounding quotes first (`"my file.ts"`), keep inner content.
    let mut cleaned = tok.trim();
    if cleaned.len() >= 2 {
        let b = cleaned.as_bytes();
        if (b[0] == b'"' && b[b.len() - 1] == b'"')
            || (b[0] == b'\'' && b[b.len() - 1] == b'\'')
        {
            cleaned = &cleaned[1..cleaned.len() - 1];
        }
    }
    cleaned = trim_leading_noise(cleaned.trim_start());
    // Iteratively strip wrapping punctuation (`(foo.rs)`, `[a.ts]:12`):
    // leading openers and trailing closers mirror each other.
    for _ in 0..4 {
        let before = cleaned;
        cleaned = trim_leading_openers(cleaned.trim_start());
        cleaned = trim_trailing_punct(cleaned.trim_end());
        if cleaned == before {
            break;
        }
    }
    // Unescape `\ ` (shell-escaped spaces) for the path; display keeps raw.
    let unescaped = cleaned.replace("\\ ", " ");
    let cleaned = unescaped.trim();
    if cleaned.is_empty() {
        return None;
    }
    let (path, line, col) = split_line_column(cleaned);
    // 后缀前的闭合符（如 `[foo.ts]:12` 的 `]`）在首轮 trim 时够不着，
    // 拆出行列号后再剥一次。
    let path = trim_trailing_punct(path.trim()).trim();
    // If stripping left an empty path (`:12`), the token was not a path.
    if path.is_empty() || !looks_like_path(path) {
        return None;
    }
    let start_col = byte_to_col.get(bstart).copied().unwrap_or(0);
    let end_col = bend
        .checked_sub(1)
        .and_then(|last| byte_to_col.get(last).copied())
        .map(|c| c + 1)
        .unwrap_or(start_col + 1);
    if end_col <= start_col {
        return None;
    }
    Some(FileHit {
        raw: tok.to_string(),
        path: path.to_string(),
        line,
        col,
        start_col,
        end_col,
    })
}

/// Find the candidate under a grid column.
pub fn file_hit_at_col<'a>(hits: &'a [FileHit], col: usize) -> Option<&'a FileHit> {
    hits
        .iter()
        .find(|h| col >= h.start_col && col < h.end_col)
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
        assert_eq!(
            split_line_column("a:b"),
            ("a:b".to_string(), None, None)
        );
        assert_eq!(
            split_line_column("foo:"),
            ("foo:".to_string(), None, None)
        );
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
        assert!(looks_like_path("./src/main.rs"));
        assert!(looks_like_path("/abs/path"));
        assert!(looks_like_path("~/x"));
        assert!(looks_like_path("foo.rs"));
        assert!(looks_like_path("src"));
    }

    #[test]
    fn trims_punct() {
        assert_eq!(trim_trailing_punct("(main.rs),"), "(main.rs");
        // Inner paren kept, trailing stripped by caller layers.
        assert_eq!(trim_trailing_punct("foo.ts..."), "foo.ts");
    }

    #[test]
    fn apostrophe_does_not_swallow_path() {
        // `don't` 的 `'` 无配对闭合引号，必须视为普通字符。
        let toks = parse_file_candidates("don't forget src/main.rs");
        assert!(toks.iter().any(|(_, _, t)| t == "src/main.rs"));
    }

    #[test]
    fn wrapped_parens_hit_resolves() {
        // `(src/main.rs)` / `[foo.ts]:12` 经 to_file_hit 应得干净路径。
        let text = "(src/main.rs) [foo.ts]:12";
        let mut byte_to_col = Vec::new();
        for (col, ch) in text.chars().enumerate() {
            for _ in 0..ch.len_utf8() {
                byte_to_col.push(col);
            }
        }
        let cells_text = text;
        let hits = map_candidates(cells_text, &byte_to_col);
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"), "got {paths:?}");
        assert!(paths.contains(&"foo.ts"), "got {paths:?}");
        let foo = hits.iter().find(|h| h.path == "foo.ts").unwrap();
        assert_eq!(foo.line, Some(12));
    }
}
