//! Block-level markdown line classification.
//!
//! [`classify`] maps one source line to a [`Block`], threading a [`DocState`]
//! through consecutive lines so fenced code blocks and YAML frontmatter carry
//! across them (feed lines in document order, one state per pass).
//!
//! All offsets returned in a [`Block`] are **byte** offsets into the line.
//! Every prefix this module recognizes is pure ASCII (whitespace, `#`, `>`,
//! digits, `-*+`, `[x]`), so those offsets are also char columns.

/// What a source line is, at the block level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// `#`–`######` heading; `content` is where the text starts.
    Heading { level: u8, content: usize },
    /// `- ` / `* ` / `+ ` list item.
    Bullet { indent: usize, content: usize },
    /// `1. ` / `1) ` list item.
    Ordered {
        indent: usize,
        number: u64,
        content: usize,
    },
    /// `- [ ] ` / `- [x] ` list item; `state` is the byte of the char
    /// inside the brackets (the one a toggle rewrites).
    Task {
        indent: usize,
        checked: bool,
        state: usize,
        content: usize,
    },
    /// `> ` quote line; `depth` counts nested `>` markers.
    Quote { depth: u8, content: usize },
    /// A ``` or ~~~ fence line. `open` is whether it starts a block;
    /// `lang` is the info string on openers (lowercased).
    Fence { open: bool, lang: Option<String> },
    /// A `$$` delimiter line of a display-math block (Typora math).
    MathDelim { open: bool },
    /// Display math: a single-line `$$...$$` formula, or a line inside a
    /// `$$` block; `start`/`end` bound the formula bytes within the line.
    Math { start: usize, end: usize },
    /// A line inside a fenced code block (or 4-space indented code).
    CodeLine,
    /// `: definition` line of a definition list; `content` skips the marker.
    Definition { content: usize },
    /// `---` / `***` / `___` thematic break.
    Rule,
    /// A `|`-leading table line.
    Table,
    /// A `=`/`-` setext underline, consumed from the paragraph line above
    /// via lookahead (`Title` + `===` → Heading + SetextUnderline). The
    /// content line is classified as [`Block::Heading`] with `content: 0`.
    SetextUnderline { level: u8 },
    /// A line of the YAML frontmatter block (including its `---` delimiters).
    FrontMatter,
    /// Only whitespace.
    Blank,
    /// Anything else.
    Paragraph,
}

/// Classifier state carried from one line to the next. Start each document
/// pass with `DocState::default()`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocState {
    line_no: usize,
    frontmatter: bool,
    /// Open fence: (fence char, marker run length, info string).
    fence: Option<(u8, usize, Option<String>)>,
    /// Inside a `$$` display-math block.
    math: bool,
    /// The previous line was a paragraph: a GFM table can never open on a
    /// lazy continuation line (CommonMark: the table is part of a new block).
    prev_paragraph: bool,
    /// The previous line was a table row, so a following plain line continues
    /// the grid (needed for tables that drop the outer pipes).
    in_table: bool,
    /// Content column of the innermost open list item. A fence nested in a
    /// list is indented past the 3-space document limit (CommonMark allows 3
    /// spaces *on top of* the list's content indent), so the limit has to
    /// follow the list context or `- item` + 4-space ` ``` ` reads as prose.
    list_content: Option<usize>,
    /// The previous classify returned a setext heading (the next line is its
    /// `=`/`-` underline).
    setext_pending: Option<u8>,
}

impl DocState {
    /// Whether two states agree on everything that affects *future*
    /// classification. `line_no` is excluded: it only feeds the "is this the
    /// document's first line" frontmatter check, so once both states are past
    /// line 0 their `line_no` values are irrelevant to later results.
    ///
    /// This is the convergence test for incremental classification: once a
    /// re-classification run reaches a line whose state matches the previously
    /// saved state, every later line classifies identically, so the old tail
    /// can be reused verbatim.
    pub fn same_parse_state(&self, other: &Self) -> bool {
        self.frontmatter == other.frontmatter
            && self.fence == other.fence
            && self.math == other.math
            && self.prev_paragraph == other.prev_paragraph
            && self.in_table == other.in_table
            && self.list_content == other.list_content
            && self.setext_pending == other.setext_pending
    }

    /// The language of the fenced code block the *next* line would belong to.
    pub fn fence_lang(&self) -> Option<&str> {
        self.fence.as_ref().and_then(|(_, _, lang)| lang.as_deref())
    }

    /// How far a fence opener may be indented: 3 spaces past the enclosing
    /// list item's content column (3 at document level).
    fn max_fence_indent(&self) -> usize {
        self.list_content.map_or(3, |c| c + 3)
    }
}

/// Classify one line, advancing `state`.
pub fn classify(line: &str, state: &mut DocState) -> Block {
    classify_with_next(line, None, state)
}

/// Classify one line, advancing `state`.
///
/// `next` is the following source line, used for the one construct that
/// needs lookahead: a GFM table whose rows drop the outer pipes
/// (`a | b` / `--- | ---`). Pass `None` when classifying the last line or
/// when the caller has no lookahead (then such a table reads as a
/// paragraph, which is exactly what it looks like in isolation).
pub fn classify_with_next(line: &str, next: Option<&str>, state: &mut DocState) -> Block {
    let was_in_fence = state.fence.is_some();
    let block = classify_inner(line, next, state);
    state.prev_paragraph = matches!(block, Block::Paragraph);
    state.in_table = matches!(block, Block::Table);
    // List context: a nested fence keeps the list open, so freeze the column
    // while a code block is running (code lines are not list content).
    if !was_in_fence {
        let indent = line.len() - line.trim_start().len();
        state.list_content = match &block {
            Block::Bullet { content, .. }
            | Block::Ordered { content, .. }
            | Block::Task { content, .. } => Some(*content),
            Block::Blank | Block::FrontMatter => state.list_content,
            _ => match state.list_content {
                Some(col) if indent >= col => Some(col),
                _ => None,
            },
        };
    }
    block
}

fn classify_inner(line: &str, next: Option<&str>, state: &mut DocState) -> Block {
    let first = state.line_no == 0;
    state.line_no += 1;

    // The line after a setext heading is its underline, whatever it looks
    // like (the lookahead already verified it).
    if let Some(level) = state.setext_pending.take() {
        return Block::SetextUnderline { level };
    }

    if state.frontmatter {
        if line.trim() == "---" {
            state.frontmatter = false;
        }
        return Block::FrontMatter;
    }
    if first && line.trim_end() == "---" {
        state.frontmatter = true;
        return Block::FrontMatter;
    }

    if let Some((ch, len, _)) = state.fence {
        let t = line.trim_start();
        let run = t.bytes().take_while(|&b| b == ch).count();
        if run >= len && t[run..].trim().is_empty() {
            state.fence = None;
            return Block::Fence {
                open: false,
                lang: None,
            };
        }
        return Block::CodeLine;
    }
    if let Some((ch, len, lang)) = fence_marker(line, state.max_fence_indent()) {
        state.fence = Some((ch, len, lang.clone()));
        return Block::Fence { open: true, lang };
    }

    // Display math (`$$`, Typora parity): after fence handling so `$$` inside
    // a code fence stays literal, before Blank so empty lines inside a math
    // block keep the block contiguous (pulldown folds them into DisplayMath).
    if state.math {
        if line.trim() == "$$" {
            state.math = false;
            return Block::MathDelim { open: false };
        }
        return Block::Math {
            start: 0,
            end: line.len(),
        };
    }
    if let Some(block) = math_line(line, state) {
        return block;
    }

    if line.trim().is_empty() {
        return Block::Blank;
    }
    if is_rule(line) {
        return Block::Rule;
    }

    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];

    if let Some((level, content)) = heading_marker(rest) {
        return Block::Heading {
            level,
            content: indent + content,
        };
    }
    if rest.starts_with('>') {
        let (depth, content) = quote_marker(rest);
        return Block::Quote {
            depth,
            content: indent + content,
        };
    }
    if let Some((checked, state, content)) = task_marker(rest) {
        return Block::Task {
            indent,
            checked,
            state: indent + state,
            content: indent + content,
        };
    }
    if let Some(content) = bullet_marker(rest) {
        return Block::Bullet {
            indent,
            content: indent + content,
        };
    }
    if let Some((number, content)) = ordered_marker(rest) {
        return Block::Ordered {
            indent,
            number,
            content: indent + content,
        };
    }
    // Indented code (4+ spaces, preview parity): cannot interrupt a
    // paragraph, and never steals list continuations (the item's content
    // column wins over the document-level 4-space rule).
    if indent >= 4 && !state.prev_paragraph && state.list_content.is_none_or(|col| indent < col) {
        return Block::CodeLine;
    }
    // Definition-list definition (`: def`, up to 3 leading spaces).
    if indent <= 3 && (rest == ":" || rest.starts_with(": ")) {
        return Block::Definition {
            content: (indent + 2).min(line.len()),
        };
    }
    // Continuation rows of a table that drops the outer pipes: no marker of
    // their own, so they follow the row above (a blank line or any other
    // block construct above already ended the run).
    if state.in_table {
        return Block::Table;
    }
    if rest.starts_with('|') {
        return Block::Table;
    }
    // Setext heading: a paragraph line directly followed by a `=`/`-`
    // underline (preview parity: pulldown-cmark renders `Title\n===` as h1
    // and `a\n---` as h2 — the setext read wins over a single-column table
    // delimiter, matching pulldown). Multi-column delimiter rows carry `|`
    // and never look like underlines, so tables still win there.
    if let Some(next) = next
        && let Some(level) = setext_underline(next)
    {
        state.setext_pending = Some(level);
        return Block::Heading { level, content: 0 };
    }
    // GFM tables may drop the outer pipes: `a | b` followed by `--- | ---`.
    // Requires the delimiter row to carry the same column count as the header,
    // and refuses lazy continuation lines, matching pulldown-cmark.
    if let Some(next) = next {
        let cols = split_table_row(rest).len();
        if !state.prev_paragraph
            && rest.contains('|')
            && cols > 0
            && table_delimiter_cols(next) == Some(cols)
        {
            return Block::Table;
        }
    }
    Block::Paragraph
}

/// A setext underline candidate: `=`+ (level 1) or `-`+ (level 2), up to 3
/// leading spaces, trailing whitespace allowed. A pipe-carrying delimiter
/// row is not one (that is a table, checked first).
fn setext_underline(line: &str) -> Option<u8> {
    let indent = line.len() - line.trim_start().len();
    if indent > 3 {
        return None;
    }
    let t = line.trim();
    let first = *t.as_bytes().first()?;
    if (first != b'=' && first != b'-') || !t.bytes().all(|b| b == first) {
        return None;
    }
    Some(if first == b'=' { 1 } else { 2 })
}

/// Split a GFM table row into cells at the *untrimmed* pipe-delimited spans:
/// `| a | b |` **and** `a | b` both yield two spans. Cell text keeps its
/// padding (callers trim for display); byte offsets stay line-absolute.
///
/// GFM splits on **every** unescaped `|` — including inside code spans
/// (`| `a|b` |` is *three* cells, verified against pulldown-cmark; the
/// backslash is eaten by the table parser, not by the code span). Only a
/// `|` preceded by an odd run of backslashes is literal, so escape
/// awareness is a backslash-parity check rather than a code-span scan.
///
/// Outer pipes are dropped so the count matches GFM columns.
pub fn table_cell_spans(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            // Backslash: skip the escaped char. A run of `\` is consumed one
            // pair at a time, which is exactly the odd/even parity rule for
            // "is this pipe escaped".
            b'\\' => i += 2,
            b'|' => {
                bounds.push((start, i));
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    bounds.push((start, bytes.len()));
    // Outer pipes produce zero-width edge spans (`| a |` scans as
    // `(0,0),(1,4),(5,5)`): drop them so the count matches GFM columns.
    if line.trim_start().starts_with('|') && matches!(bounds.first(), Some((a, b)) if a == b) {
        bounds.remove(0);
    }
    if line.trim_end().ends_with('|') && matches!(bounds.last(), Some((a, b)) if a == b) {
        bounds.pop();
    }
    if bounds.is_empty() {
        bounds.push((0, line.len()));
    }
    bounds
}

/// Split a GFM table row into trimmed cells: `| a | b |` **and** `a | b`.
///
/// Display-oriented wrapper over [`table_cell_spans`]; see there for the
/// exact split rule (every unescaped `|`, code spans included).
pub fn split_table_row(line: &str) -> Vec<String> {
    table_cell_spans(line)
        .into_iter()
        .map(|(s, e)| line[s..e].trim().to_string())
        .collect()
}

/// Is `cell` a delimiter cell (`-`, `---`, `:--`, `--:`, `:-:`)?
///
/// Both the stripped-prefix and stripped-suffix steps fall back to the
/// *current* text, not the original cell: `":---"` must peel the leading
/// colon and then keep the peeled form (`:---` → `---`), otherwise every
/// left-aligned column reads as "not a delimiter" and the whole grid
/// falls back to source lines.
pub fn is_delimiter_cell(cell: &str) -> bool {
    let c = cell.trim();
    let c = c.strip_prefix(':').unwrap_or(c);
    let c = c.strip_suffix(':').unwrap_or(c);
    !c.is_empty() && c.chars().all(|ch| ch == '-')
}

/// The column count if `line` is a GFM delimiter row (`| :--- | ---: |`).
pub fn table_delimiter_cols(line: &str) -> Option<usize> {
    let cells = split_table_row(line);
    if cells.is_empty() || cells.iter().any(|c| c.is_empty()) {
        return None;
    }
    if cells.iter().all(|c| is_delimiter_cell(c)) {
        Some(cells.len())
    } else {
        None
    }
}

/// Leading-whitespace width in columns, with tabs as 4.
pub fn indent_cols(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// A `$$` display-math line: bare `$$` opens a block, `$$...$$` with a
/// non-empty formula and nothing but whitespace after the closer is one
/// inline block line. Returns the block (opening sets `state.math`).
/// Anything else (e.g. `$$x$$ trailing text`) is not math.
fn math_line(line: &str, state: &mut DocState) -> Option<Block> {
    let trimmed = line.trim();
    if !trimmed.starts_with("$$") {
        return None;
    }
    let indent = line.len() - line.trim_start().len();
    let after_open = &trimmed[2..];
    let Some(close) = after_open.find("$$") else {
        if after_open.trim().is_empty() {
            state.math = true;
            return Some(Block::MathDelim { open: true });
        }
        return None;
    };
    if !after_open[close + 2..].trim().is_empty() {
        return None;
    }
    if after_open[..close].trim().is_empty() {
        state.math = true;
        return Some(Block::MathDelim { open: true });
    }
    Some(Block::Math {
        start: indent + 2,
        end: indent + 2 + close,
    })
}

/// A ``` / ~~~ opener: (fence char, run length, info).
///
/// `max_indent` is 3 at document level and `list_content + 3` inside a list
/// item, mirroring CommonMark's container-relative indent rule.
fn fence_marker(line: &str, max_indent: usize) -> Option<(u8, usize, Option<String>)> {
    let indent = line.len() - line.trim_start().len();
    if indent > max_indent {
        return None;
    }
    let t = &line[indent..];
    let ch = *t.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = t.bytes().take_while(|&b| b == ch).count();
    if len < 3 {
        return None;
    }
    let info = t[len..].trim();
    // An info string with a backtick is not a fence (CommonMark).
    if ch == b'`' && info.contains('`') {
        return None;
    }
    let lang = info
        .split_whitespace()
        .next()
        .map(|s| s.to_ascii_lowercase());
    Some((ch, len, lang))
}

/// `---` / `***` / `___` with optional interior spaces, three or more.
fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if t.len() < 3 {
        return false;
    }
    let ch = t.chars().next().unwrap();
    matches!(ch, '-' | '*' | '_') && t.chars().all(|c| c == ch)
}

/// `#{1,6}` followed by a space (or end of line): (level, content offset).
fn heading_marker(rest: &str) -> Option<(u8, usize)> {
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    match rest.as_bytes().get(level) {
        None => Some((level as u8, level)),
        Some(b' ') => Some((level as u8, level + 1)),
        _ => None,
    }
}

/// One or more `>` markers (optionally space-separated): (depth, content).
fn quote_marker(rest: &str) -> (u8, usize) {
    let bytes = rest.as_bytes();
    let mut depth = 0u8;
    let mut at = 0;
    while at < bytes.len() && bytes[at] == b'>' {
        depth = depth.saturating_add(1);
        at += 1;
        if bytes.get(at) == Some(&b' ') {
            at += 1;
        }
    }
    (depth, at)
}

/// `- ` / `* ` / `+ ` (or the bare marker at end of line): content offset.
fn bullet_marker(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    if !matches!(bytes.first(), Some(b'-' | b'*' | b'+')) {
        return None;
    }
    match bytes.get(1) {
        None => Some(1),
        Some(b' ') => Some(2),
        _ => None,
    }
}

/// A bullet followed by `[ ]` / `[x]` and a space or end of line:
/// (checked, state-char offset, content offset).
fn task_marker(rest: &str) -> Option<(bool, usize, usize)> {
    let open = bullet_marker(rest)?;
    let bytes = rest.as_bytes();
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let checked = match bytes.get(open + 1) {
        Some(b' ') => false,
        Some(b'x' | b'X') => true,
        _ => return None,
    };
    if bytes.get(open + 2) != Some(&b']') {
        return None;
    }
    match bytes.get(open + 3) {
        None => Some((checked, open + 1, open + 3)),
        Some(b' ') => Some((checked, open + 1, open + 4)),
        _ => None,
    }
}

/// `1. ` / `1) ` (≤9 digits): (number, content offset).
fn ordered_marker(rest: &str) -> Option<(u64, usize)> {
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    if !matches!(rest.as_bytes().get(digits), Some(b'.' | b')')) {
        return None;
    }
    let number: u64 = rest[..digits].parse().ok()?;
    match rest.as_bytes().get(digits + 1) {
        None => Some((number, digits + 1)),
        Some(b' ') => Some((number, digits + 2)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Block {
        // Pretend mid-document, so a `---` reads as a rule and not as front
        // matter.
        let mut state = DocState {
            line_no: 1,
            ..Default::default()
        };
        classify(line, &mut state)
    }

    fn doc(lines: &[&str]) -> Vec<Block> {
        let mut state = DocState::default();
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let next = lines.get(i + 1).copied();
                classify_with_next(l, next, &mut state)
            })
            .collect()
    }

    #[test]
    fn indented_code_needs_blank_before() {
        // After a blank line / heading: code.
        let blocks = doc(&["# h", "", "    code();"]);
        assert!(matches!(blocks[2], Block::CodeLine));
        // Directly after paragraph text: lazy continuation, not code.
        let blocks = doc(&["para", "    cont();"]);
        assert!(matches!(blocks[1], Block::Paragraph));
        // List continuation keeps its paragraph even at 4 spaces.
        let blocks = doc(&["- item", "    cont"]);
        assert!(matches!(blocks[1], Block::Paragraph));
    }

    #[test]
    fn definition_lines_classify() {
        assert!(matches!(one(": def"), Block::Definition { content: 2 }));
        assert!(matches!(one("   : def"), Block::Definition { content: 5 }));
        // 4-space indent is code, not a definition.
        assert!(matches!(one("    : def"), Block::CodeLine));
        // `:smile:` is an emoji token, not a definition marker.
        assert!(matches!(one(":smile:"), Block::Paragraph));
    }

    #[test]
    fn headings() {
        assert_eq!(
            one("# Title"),
            Block::Heading {
                level: 1,
                content: 2
            }
        );
        assert_eq!(
            one("###### deep"),
            Block::Heading {
                level: 6,
                content: 7
            }
        );
        assert_eq!(
            one("  ## indented"),
            Block::Heading {
                level: 2,
                content: 5
            }
        );
        assert_eq!(
            one("##"),
            Block::Heading {
                level: 2,
                content: 2
            }
        );
        assert_eq!(one("#######"), Block::Paragraph); // 7 deep is not a heading
        assert_eq!(one("#tag"), Block::Paragraph); // no space: a tag, not a heading
    }

    #[test]
    fn lists() {
        assert_eq!(
            one("- item"),
            Block::Bullet {
                indent: 0,
                content: 2
            }
        );
        assert_eq!(
            one("* item"),
            Block::Bullet {
                indent: 0,
                content: 2
            }
        );
        assert_eq!(
            one("  + item"),
            Block::Bullet {
                indent: 2,
                content: 4
            }
        );
        assert_eq!(
            one("-"),
            Block::Bullet {
                indent: 0,
                content: 1
            }
        );
        assert_eq!(one("-nope"), Block::Paragraph);
        assert_eq!(
            one("3. third"),
            Block::Ordered {
                indent: 0,
                number: 3,
                content: 3
            }
        );
        assert_eq!(
            one("12) x"),
            Block::Ordered {
                indent: 0,
                number: 12,
                content: 4
            }
        );
        assert_eq!(one("3.x"), Block::Paragraph);
    }

    #[test]
    fn tasks() {
        assert_eq!(
            one("- [ ] todo"),
            Block::Task {
                indent: 0,
                checked: false,
                state: 3,
                content: 6
            }
        );
        assert_eq!(
            one("- [x] done"),
            Block::Task {
                indent: 0,
                checked: true,
                state: 3,
                content: 6
            }
        );
        assert_eq!(
            one("- [X] done"),
            Block::Task {
                indent: 0,
                checked: true,
                state: 3,
                content: 6
            }
        );
        assert_eq!(
            one("- [ ]"),
            Block::Task {
                indent: 0,
                checked: false,
                state: 3,
                content: 5
            }
        );
        assert_eq!(
            one("  - [ ] in"),
            Block::Task {
                indent: 2,
                checked: false,
                state: 5,
                content: 8
            }
        );
        assert_eq!(
            one("- [y] no"),
            Block::Bullet {
                indent: 0,
                content: 2
            }
        );
    }

    #[test]
    fn quotes() {
        assert_eq!(
            one("> hi"),
            Block::Quote {
                depth: 1,
                content: 2
            }
        );
        assert_eq!(
            one(">hi"),
            Block::Quote {
                depth: 1,
                content: 1
            }
        );
        assert_eq!(
            one("> > deep"),
            Block::Quote {
                depth: 2,
                content: 4
            }
        );
    }

    #[test]
    fn setext_headings_via_lookahead() {
        let mut state = DocState::default();
        // `Title` + `===` → heading level 1; the underline is consumed next.
        assert_eq!(
            classify_with_next("Title", Some("==="), &mut state),
            Block::Heading {
                level: 1,
                content: 0
            }
        );
        assert_eq!(
            classify("===", &mut state),
            Block::SetextUnderline { level: 1 }
        );
        // `-` underline → level 2 (setext wins over thematic break here).
        let mut state = DocState::default();
        assert_eq!(
            classify_with_next("Sub", Some("-"), &mut state),
            Block::Heading {
                level: 2,
                content: 0
            }
        );
        assert_eq!(
            classify("-", &mut state),
            Block::SetextUnderline { level: 2 }
        );
    }

    #[test]
    fn setext_underline_rules() {
        // Only after a paragraph line; >3 spaces of indent is not one.
        let mut state = DocState::default();
        assert_eq!(
            classify_with_next("a", Some("    ==="), &mut state),
            Block::Paragraph
        );
        assert_eq!(classify("    ===", &mut state), Block::Paragraph);
        // Mixed content is not an underline.
        let mut state = DocState::default();
        assert_eq!(
            classify_with_next("a", Some("== x"), &mut state),
            Block::Paragraph
        );
        // A pipe delimiter row is a table, not a setext underline.
        let mut state = DocState::default();
        assert_eq!(
            classify_with_next("a | b", Some("--- | ---"), &mut state),
            Block::Table
        );
        // Single-column delimiter after a plain word: setext wins (pulldown
        // renders `a\n---` as h2).
        let mut state = DocState::default();
        assert_eq!(
            classify_with_next("a", Some("---"), &mut state),
            Block::Heading {
                level: 2,
                content: 0
            }
        );
        // A header with a pipe but a NON-matching delimiter falls through to
        // setext only if the delimiter is a clean underline; `-- x` is neither.
        let mut state = DocState::default();
        assert_eq!(
            classify_with_next("a | b", Some("-"), &mut state),
            Block::Heading {
                level: 2,
                content: 0
            }
        );
    }

    #[test]
    fn rules_and_tables() {
        assert_eq!(one("---"), Block::Rule);
        assert_eq!(one("***"), Block::Rule);
        assert_eq!(one("___"), Block::Rule);
        assert_eq!(one("- - -"), Block::Rule);
        assert_eq!(one("--"), Block::Paragraph);
        assert_eq!(one("| a | b |"), Block::Table);
    }

    /// Regression: `:---` / `:-:` / `---:` must all read as delimiter cells.
    /// The old local copy in `view.rs` fell back to the un-peeled cell after
    /// stripping the leading colon, so left-aligned columns were rejected and
    /// the whole grid rendered as raw source lines.
    #[test]
    fn delimiter_cells_all_alignments() {
        for cell in [
            "-", "---", "--------", ":--", ":---", "--:", "---:", ":-:", ":-",
        ] {
            assert!(is_delimiter_cell(cell), "{cell:?} must be a delimiter cell");
        }
        for cell in ["", " ", "a", "--a", ":-a", "-|-", "|-|", "::"] {
            assert!(!is_delimiter_cell(cell), "{cell:?} must not be a delimiter");
        }
    }

    #[test]
    fn delimiter_row_column_counts() {
        assert_eq!(table_delimiter_cols("|---|---|"), Some(2));
        assert_eq!(table_delimiter_cols("| :--- | ---: |"), Some(2));
        assert_eq!(table_delimiter_cols(":--- | :-: | ---:"), Some(3));
        assert_eq!(table_delimiter_cols("--- | ---"), Some(2));
        assert_eq!(table_delimiter_cols("---"), Some(1));
        assert_eq!(table_delimiter_cols("| a | b |"), None);
        assert_eq!(table_delimiter_cols("|---|  |"), None); // empty column
    }

    #[test]
    fn table_cells_split_with_or_without_outer_pipes() {
        assert_eq!(split_table_row("| a | b |"), vec!["a", "b"]);
        assert_eq!(split_table_row("a | b"), vec!["a", "b"]);
        assert_eq!(split_table_row("| a |"), vec!["a"]);
    }

    /// GFM lets a table drop the outer pipes; it is only decidable together
    /// with the delimiter row below.
    #[test]
    fn pipeless_tables_need_lookahead() {
        let mut state = DocState::default();
        let doc = ["", "a | b", "--- | ---", "1 | 2", ""];
        let blocks: Vec<Block> = doc
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let next = doc.get(i + 1).copied();
                classify_with_next(l, next, &mut state)
            })
            .collect();
        assert_eq!(blocks[0], Block::Blank);
        assert_eq!(blocks[1], Block::Table);
        assert_eq!(blocks[2], Block::Table);
        assert_eq!(blocks[3], Block::Table);
        assert_eq!(blocks[4], Block::Blank);
    }

    #[test]
    fn pipeless_table_needs_matching_column_count() {
        let mut state = DocState::default();
        // Two header columns, three delimiter columns: not a table.
        assert_eq!(
            classify_with_next("a | b", Some("--- | --- | ---"), &mut state),
            Block::Paragraph
        );
    }

    #[test]
    fn lazy_continuation_line_is_not_a_table() {
        let mut state = DocState::default();
        assert_eq!(
            classify_with_next("prose | with pipe", None, &mut state),
            Block::Paragraph
        );
        // Still paragraph territory: a delimiter row cannot open a table here.
        assert_eq!(
            classify_with_next("more | prose", Some("--- | ---"), &mut state),
            Block::Paragraph
        );
        // After a blank line the same shape is a table.
        assert_eq!(classify_with_next("", None, &mut state), Block::Blank);
        assert_eq!(
            classify_with_next("a | b", Some("--- | ---"), &mut state),
            Block::Table
        );
    }

    #[test]
    fn blank_and_paragraph() {
        assert_eq!(one(""), Block::Blank);
        assert_eq!(one("   "), Block::Blank);
        assert_eq!(one("plain text"), Block::Paragraph);
    }

    #[test]
    fn fenced_code_threads_state() {
        let mut state = DocState::default();
        let doc = ["```rust", "fn main() {}", "```", "after"];
        let blocks: Vec<Block> = doc.iter().map(|l| classify(l, &mut state)).collect();
        assert_eq!(
            blocks[0],
            Block::Fence {
                open: true,
                lang: Some("rust".into())
            }
        );
        assert_eq!(blocks[1], Block::CodeLine);
        assert_eq!(
            blocks[2],
            Block::Fence {
                open: false,
                lang: None
            }
        );
        assert_eq!(blocks[3], Block::Paragraph);
    }

    #[test]
    fn fence_lang_is_readable_mid_block() {
        let mut state = DocState::default();
        classify("```json", &mut state);
        assert_eq!(state.fence_lang(), Some("json"));
        classify("{}", &mut state);
        assert_eq!(state.fence_lang(), Some("json"));
        classify("```", &mut state);
        assert_eq!(state.fence_lang(), None);
    }

    /// A fence nested in a list is indented past the 3-space document limit
    /// (CommonMark allows 3 spaces on top of the list content column), so the
    /// limit has to follow the list context — otherwise `- item` + a 4-space
    /// fence renders as prose while the preview shows a code card.
    #[test]
    fn fence_nested_in_list_opens() {
        let mut state = DocState::default();
        let doc = [
            "- item",
            "",
            "    ```python",
            "    x = 1",
            "    ```",
            "after",
        ];
        let blocks: Vec<Block> = doc
            .iter()
            .enumerate()
            .map(|(i, l)| classify_with_next(l, doc.get(i + 1).copied(), &mut state))
            .collect();
        assert!(matches!(blocks[0], Block::Bullet { .. }));
        assert_eq!(
            blocks[2],
            Block::Fence {
                open: true,
                lang: Some("python".into())
            }
        );
        assert_eq!(blocks[3], Block::CodeLine);
        assert_eq!(
            blocks[4],
            Block::Fence {
                open: false,
                lang: None
            }
        );
        // Leaving the list resets the limit: 4 spaces alone is not a fence.
        assert_eq!(blocks[5], Block::Paragraph);
    }

    #[test]
    fn display_math_block_classifies() {
        let mut state = DocState::default();
        assert_eq!(classify("$$", &mut state), Block::MathDelim { open: true });
        assert_eq!(
            classify("E = mc^2", &mut state),
            Block::Math { start: 0, end: 8 }
        );
        assert_eq!(classify("", &mut state), Block::Math { start: 0, end: 0 });
        assert_eq!(classify("$$", &mut state), Block::MathDelim { open: false });
        assert_eq!(classify("after", &mut state), Block::Paragraph);
    }

    #[test]
    fn single_line_display_math() {
        let mut state = DocState::default();
        assert_eq!(
            classify("$$x + y$$", &mut state),
            Block::Math { start: 2, end: 7 }
        );
        // No swallowing: still paragraph context afterwards.
        assert_eq!(classify("text", &mut state), Block::Paragraph);
        // Trailing text after the closer is not math.
        let mut state = DocState::default();
        assert_eq!(classify("$$x$$ trailing", &mut state), Block::Paragraph);
        // Unclosed single-line `$$x` is not math either.
        let mut state = DocState::default();
        assert_eq!(classify("$$x", &mut state), Block::Paragraph);
    }

    #[test]
    fn math_delim_inside_fence_stays_code() {
        let mut state = DocState::default();
        classify("```", &mut state);
        assert_eq!(classify("$$", &mut state), Block::CodeLine);
        assert_eq!(classify("x", &mut state), Block::CodeLine);
    }

    #[test]
    fn indent_four_is_not_a_fence_at_document_level() {
        let mut state = DocState::default();
        classify("    ```", &mut state);
        assert!(state.fence.is_none());
    }

    #[test]
    fn unterminated_fence_swallows_the_rest() {
        let mut state = DocState::default();
        classify("```", &mut state);
        assert_eq!(classify("# not a heading", &mut state), Block::CodeLine);
        assert_eq!(classify("- not a list", &mut state), Block::CodeLine);
    }

    #[test]
    fn markdown_inside_code_is_literal() {
        let mut state = DocState::default();
        classify("~~~", &mut state);
        assert_eq!(classify("---", &mut state), Block::CodeLine);
        // ``` does not close a ~~~ fence
        assert_eq!(classify("```", &mut state), Block::CodeLine);
        assert_eq!(
            classify("~~~", &mut state),
            Block::Fence {
                open: false,
                lang: None
            }
        );
    }

    #[test]
    fn frontmatter_only_at_document_start() {
        let mut state = DocState::default();
        let doc = ["---", "title: x", "---", "body"];
        let blocks: Vec<Block> = doc.iter().map(|l| classify(l, &mut state)).collect();
        assert_eq!(blocks[0], Block::FrontMatter);
        assert_eq!(blocks[1], Block::FrontMatter);
        assert_eq!(blocks[2], Block::FrontMatter);
        assert_eq!(blocks[3], Block::Paragraph);
        // Mid-document `---` is a rule, not frontmatter.
        let mut state = DocState::default();
        classify("intro", &mut state);
        assert_eq!(classify("---", &mut state), Block::Rule);
    }

    #[test]
    fn indent_cols_counts_tabs_as_four() {
        assert_eq!(indent_cols("    x"), 4);
        assert_eq!(indent_cols("\tx"), 4);
        assert_eq!(indent_cols(" \t x"), 6);
        assert_eq!(indent_cols("x"), 0);
    }
}

#[cfg(test)]
mod showcase_fence_tests {
    use super::*;

    fn classify_doc(doc: &[&str]) -> Vec<Block> {
        let mut st = DocState::default();
        doc.iter().map(|l| classify(l, &mut st)).collect()
    }

    #[test]
    fn blank_inside_fence_stays_codeline() {
        // Mirrors showcase §5.1 rust block (line 104 is blank).
        let doc = [
            "```rust",
            "// comment",
            "use gpui::{div, px, rems};",
            "",
            "fn main() {",
            "}",
            "```",
        ];
        let blocks = classify_doc(&doc);
        assert!(
            matches!(blocks[0], Block::Fence { open: true, .. }),
            "{:?}",
            blocks[0]
        );
        for (i, b) in blocks[1..6].iter().enumerate() {
            assert_eq!(
                *b,
                Block::CodeLine,
                "line {} should be CodeLine, got {b:?}",
                i + 1
            );
        }
        assert!(matches!(blocks[6], Block::Fence { open: false, .. }));
    }

    #[test]
    fn consecutive_fence_blocks_each_close() {
        let doc = ["```rust", "a", "```", "", "```python", "b", "```"];
        let blocks = classify_doc(&doc);
        assert!(matches!(blocks[0], Block::Fence { open: true, .. }));
        assert_eq!(blocks[1], Block::CodeLine);
        assert!(matches!(blocks[2], Block::Fence { open: false, .. }));
        assert_eq!(blocks[3], Block::Blank);
        assert!(matches!(blocks[4], Block::Fence { open: true, .. }));
        assert_eq!(blocks[5], Block::CodeLine);
        assert!(matches!(blocks[6], Block::Fence { open: false, .. }));
    }
}
