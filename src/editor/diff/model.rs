//! What a diff is: files of hunks of lines, computed from two texts.
//!
//! The data model follows rune-kit's `diff` component (`crates/rune-kit/src/diff/model.rs`):
//! a [`FileDiff`] is a list of [`Hunk`]s, each a list of [`Line`]s that know their numbers on
//! both sides, whether they end with a line break, and which byte ranges changed within them
//! (word changes). Line-level and word-level diffing uses `imara-diff`'s Histogram algorithm,
//! which deepterm already depends on for git gutter marks and hunk staging.
//!
//! [`compute_hunks`] and [`build_hunk_patch`] are kept for the staging and revert paths, which
//! only need the changed row ranges on each side.

use std::ops::Range;

use imara_diff::{
    Algorithm, diff,
    intern::{InternedInput, Token},
    sources::lines_with_terminator,
};

/// Lines of context kept around each change, as `git diff` does.
pub const CONTEXT: u32 = 3;

/// The two sides of a diff: the text before the change and after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Old => Side::New,
            Side::New => Side::Old,
        }
    }
}

/// What a line of a hunk is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// Unchanged, on both sides.
    Context,
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Removed,
}

/// One line of a hunk.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub kind: LineKind,
    /// Its 1-based number on the old side, unless it was added.
    pub old: Option<u32>,
    /// Its 1-based number on the new side, unless it was removed.
    pub new: Option<u32>,
    /// The line without its line break.
    pub text: String,
    /// Byte ranges of `text` that differ from the line it replaced or was replaced by (word
    /// changes), when it pairs with one.
    pub changes: Vec<Range<usize>>,
    /// The file ends here without a line break (`\ No newline at end of file`).
    pub no_newline: bool,
}

impl Line {
    fn new(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> Line {
        Line {
            kind,
            old,
            new,
            text: text.to_owned(),
            changes: Vec::new(),
            no_newline: false,
        }
    }

    /// Its number on `side`.
    pub fn number(&self, side: Side) -> Option<u32> {
        match side {
            Side::Old => self.old,
            Side::New => self.new,
        }
    }
}

/// A run of changed lines with the unchanged lines around them.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    /// Where the hunk starts on the old side, as its `@@` header says: the line after which
    /// lines are inserted when `old_count` is 0.
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// The text after the header's closing `@@`, often the enclosing function.
    pub section: String,
    pub lines: Vec<Line>,
}

impl Hunk {
    /// The first line number the hunk covers on `side` (its start, or the line after it when
    /// it covers none there).
    pub(crate) fn first(&self, side: Side) -> u32 {
        let (start, count) = match side {
            Side::Old => (self.old_start, self.old_count),
            Side::New => (self.new_start, self.new_count),
        };
        if count == 0 {
            start.saturating_add(1)
        } else {
            start.max(1)
        }
    }

    /// One past the last line number the hunk covers on `side`.
    pub(crate) fn end(&self, side: Side) -> u32 {
        self.first(side).saturating_add(if side == Side::Old {
            self.old_count
        } else {
            self.new_count
        })
    }

    /// The header line as a patch writes it, `@@ -1,4 +1,5 @@ section`.
    pub fn header(&self) -> String {
        let range = |start: u32, count: u32| {
            if count == 1 {
                format!("{start}")
            } else {
                format!("{start},{count}")
            }
        };
        let mut h = format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_count),
            range(self.new_start, self.new_count)
        );
        if !self.section.is_empty() {
            h.push(' ');
            h.push_str(&self.section);
        }
        h
    }
}

/// A whole text, split into lines.
#[derive(Clone, Debug)]
pub(crate) struct Source {
    pub(crate) text: String,
    /// Each line's byte range, without its line break (which may keep a `\r`).
    starts: Vec<usize>,
}

impl Source {
    pub(crate) fn new(text: &str) -> Source {
        Source {
            text: text.to_owned(),
            starts: line_starts(text),
        }
    }

    /// Line `n` (1-based), without its line break.
    pub(crate) fn line(&self, n: u32) -> Option<&str> {
        let row = n.checked_sub(1)?;
        let range = line_range(&self.text, &self.starts, row);
        let s = self.text.get(range)?;
        Some(s.strip_suffix('\r').unwrap_or(s))
    }

    pub(crate) fn len(&self) -> u32 {
        let n = self.starts.len() - 1;
        if self.text.is_empty() || self.text.ends_with('\n') {
            n.saturating_sub(1) as u32
        } else {
            n as u32
        }
    }
}

/// What happened to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
}

/// One file's changes.
#[derive(Clone, Debug)]
pub struct FileDiff {
    /// The path before the change; `None` for an added file.
    pub old_path: Option<String>,
    /// The path after the change; `None` for a deleted file.
    pub new_path: Option<String>,
    pub status: FileStatus,
    /// A rename's or copy's similarity, in percent.
    pub similarity: Option<u8>,
    /// File modes, as `100644`, when the patch gives them.
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    /// The file cannot be shown as text (binary or undecodable).
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    /// The whole old text, when the diff was built from texts: lets hidden unchanged ranges
    /// be expanded.
    old_text: Option<Source>,
    /// The whole new text, same as `old_text`.
    new_text: Option<Source>,
    additions: usize,
    deletions: usize,
}

impl FileDiff {
    fn empty(old_path: Option<String>, new_path: Option<String>) -> FileDiff {
        FileDiff {
            old_path,
            new_path,
            status: FileStatus::Modified,
            similarity: None,
            old_mode: None,
            new_mode: None,
            binary: false,
            hunks: Vec::new(),
            old_text: None,
            new_text: None,
            additions: 0,
            deletions: 0,
        }
    }

    /// The diff of `old` to `new` for a file at `path`, with [`CONTEXT`] lines of context
    /// around each change and word changes on paired lines. Both texts stay with it, so every
    /// unchanged range between hunks can be expanded.
    pub fn from_texts(path: impl Into<String>, old: &str, new: &str) -> FileDiff {
        let path = path.into();
        FileDiff::between(Some(path.clone()), Some(path), old, new)
    }

    /// [`FileDiff::from_texts`] for a file whose path changed (a rename), was added (`old_path`
    /// `None`, `old` empty) or deleted (`new_path` `None`, `new` empty).
    pub fn between(
        old_path: Option<String>,
        new_path: Option<String>,
        old: &str,
        new: &str,
    ) -> FileDiff {
        let mut f = FileDiff::empty(old_path, new_path);
        f.status = match (&f.old_path, &f.new_path) {
            (None, _) => FileStatus::Added,
            (_, None) => FileStatus::Deleted,
            (Some(a), Some(b)) if a != b => FileStatus::Renamed,
            _ => FileStatus::Modified,
        };
        if old.contains('\0') || new.contains('\0') {
            f.binary = true;
            return f;
        }
        f.hunks = hunks(old, new, CONTEXT);
        f.old_text = Some(Source::new(old));
        f.new_text = Some(Source::new(new));
        f.recount();
        f.pair_words();
        f
    }

    /// A diff made of texts that cannot be shown (binary or undecodable content).
    pub fn binary(old_path: Option<String>, new_path: Option<String>) -> FileDiff {
        let mut f = FileDiff::empty(old_path.clone(), new_path.clone());
        f.status = match (&old_path, &new_path) {
            (None, _) => FileStatus::Added,
            (_, None) => FileStatus::Deleted,
            (Some(a), Some(b)) if a != b => FileStatus::Renamed,
            _ => FileStatus::Modified,
        };
        f.binary = true;
        f
    }

    fn recount(&mut self) {
        let (mut adds, mut dels) = (0, 0);
        for hunk in &self.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Added => adds += 1,
                    LineKind::Removed => dels += 1,
                    LineKind::Context => {}
                }
            }
        }
        self.additions = adds;
        self.deletions = dels;
    }

    /// The path to show: the new one, or the old one for a deleted file.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    /// Lines added.
    pub fn additions(&self) -> usize {
        self.additions
    }

    /// Lines removed.
    pub fn deletions(&self) -> usize {
        self.deletions
    }

    /// The highlighter language for the path's extension (or name, for a `Makefile`), as
    /// written: `"rs"`, `"tsx"`.
    pub fn language(&self) -> Option<&str> {
        language_of(self.path())
    }

    /// Whether the unchanged lines between hunks can be shown.
    pub fn expandable(&self) -> bool {
        self.old_text.is_some() || self.new_text.is_some()
    }

    /// The only side the file has: `New` for an added file, `Old` for a deleted one, `None`
    /// when it has both. A file diffed from texts counts as added when its old text is empty
    /// and its new one isn't (and the other way round as deleted), unless it moved.
    pub fn sole_side(&self) -> Option<Side> {
        match self.status {
            FileStatus::Added => Some(Side::New),
            FileStatus::Deleted => Some(Side::Old),
            FileStatus::Modified => {
                let empty = |t: &Option<Source>| t.as_ref().map(|t| t.text.is_empty());
                match (empty(&self.old_text), empty(&self.new_text)) {
                    (Some(true), Some(false)) => Some(Side::New),
                    (Some(false), Some(true)) => Some(Side::Old),
                    _ => None,
                }
            }
            FileStatus::Renamed | FileStatus::Copied => None,
        }
    }

    /// The whole text of `side`, when the diff has it.
    pub fn text(&self, side: Side) -> Option<&str> {
        match side {
            Side::Old => self.old_text.as_ref().map(|s| s.text.as_str()),
            Side::New => self.new_text.as_ref().map(|s| s.text.as_str()),
        }
    }

    /// Line `n` (1-based, without its line break) of the whole text of `side`.
    pub fn side_line(&self, side: Side, n: u32) -> Option<&str> {
        match side {
            Side::Old => self.old_text.as_ref().and_then(|s| s.line(n)),
            Side::New => self.new_text.as_ref().and_then(|s| s.line(n)),
        }
    }

    /// Unchanged line `n` of the old side (with `new` its number on the new side), from
    /// whichever whole text the diff has.
    pub(crate) fn context_line(&self, old: u32, new: u32) -> Option<&str> {
        self.old_text
            .as_ref()
            .and_then(|t| t.line(old))
            .or_else(|| self.new_text.as_ref().and_then(|t| t.line(new)))
    }

    /// How many lines the old side has, when the diff knows: from the old text, or the new
    /// text and the hunks' line counts.
    pub(crate) fn old_len(&self) -> Option<u32> {
        if let Some(t) = &self.old_text {
            return Some(t.len());
        }
        let t = self.new_text.as_ref()?;
        let delta = self.additions as i64 - self.deletions as i64;
        Some((t.len() as i64 - delta).max(0) as u32)
    }

    /// The largest line number either side shows.
    pub(crate) fn last_line(&self) -> u32 {
        let hunks = self.hunks.last().map_or(0, |h| {
            h.end(Side::Old).max(h.end(Side::New)).saturating_sub(1)
        });
        let texts = [&self.old_text, &self.new_text]
            .into_iter()
            .flatten()
            .map(|t| t.len())
            .max()
            .unwrap_or(0);
        hunks.max(texts)
    }

    /// Word changes for each removed line followed by an added one, pairing them in order.
    fn pair_words(&mut self) {
        for h in &mut self.hunks {
            let mut i = 0;
            while i < h.lines.len() {
                let del = i;
                while i < h.lines.len() && h.lines[i].kind == LineKind::Removed {
                    i += 1;
                }
                let add = i;
                while i < h.lines.len() && h.lines[i].kind == LineKind::Added {
                    i += 1;
                }
                if add == del || i == add {
                    i = i.max(del + 1);
                    continue;
                }
                let pairs = (add - del).min(i - add);
                if pairs <= 128 {
                    for k in 0..pairs {
                        let (a, b) = word_changes(&h.lines[del + k].text, &h.lines[add + k].text);
                        h.lines[del + k].changes = a;
                        h.lines[add + k].changes = b;
                    }
                }
            }
        }
    }
}

/// The highlighter language a path's extension names (or its name, for a `Makefile`).
pub fn language_of(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => Some(ext),
        _ => Some(name).filter(|n| matches!(*n, "Makefile" | "Dockerfile")),
    }
}

/// `text`'s line `n` (1-based), without its line break (and without a carriage return).
pub fn line_of(text: &str, n: u32) -> Option<&str> {
    if n == 0 || n > line_count(text) {
        return None;
    }
    let starts = line_starts(text);
    let range = line_range(text, &starts, n - 1);
    text.get(range).map(|s| s.strip_suffix('\r').unwrap_or(s))
}

/// Byte offsets of every line start in a text, plus a sentinel past the end so
/// [`line_range`] works for the last line.
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts.push(text.len() + 1); // sentinel
    starts
}

/// Number of content lines. A trailing newline does not start a new line.
pub fn line_count(text: &str) -> u32 {
    let starts = line_starts(text);
    let n = starts.len() - 1; // exclude sentinel
    if text.is_empty() || text.ends_with('\n') {
        n.saturating_sub(1) as u32
    } else {
        n as u32
    }
}

/// Byte range of line `row` (0-based), excluding the trailing newline (keeping `\r`).
pub fn line_range(text: &str, starts: &[usize], row: u32) -> Range<usize> {
    let start = starts[row as usize];
    let mut end = starts[row as usize + 1].min(text.len());
    if end > start && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    start..end
}

/// Columns `line` takes with tabs stopping every four.
pub(crate) fn columns(line: &str) -> u32 {
    line.chars().fold(0, |col, c| {
        if c == '\t' {
            col + 4 - col % 4
        } else {
            col + 1
        }
    })
}

/// One changed block between two texts: its rows on each side, 0-based and half-open.
///
/// Used by the staging patch builder, the git gutter marks and the revert path, which only
/// need row ranges (not the full [`Hunk`] view model).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_rows: Range<u32>,
    pub new_rows: Range<u32>,
}

impl DiffHunk {
    pub fn deletions(&self) -> usize {
        (self.old_rows.end - self.old_rows.start) as usize
    }

    pub fn additions(&self) -> usize {
        (self.new_rows.end - self.new_rows.start) as usize
    }
}

/// Computes a line-based diff, returning `(old_row_range, new_row_range)` for each changed
/// block. Ported from zed `text_diff.rs::line_diff` (imara-diff, Histogram).
pub fn line_diff(old_text: &str, new_text: &str) -> Vec<(Range<u32>, Range<u32>)> {
    let mut edits = Vec::new();
    let input = InternedInput::new(
        lines_with_terminator(old_text),
        lines_with_terminator(new_text),
    );
    diff_internal(&input, &mut |_, _, old_rows, new_rows| {
        edits.push((old_rows, new_rows));
    });
    edits
}

/// [`line_diff`] as [`DiffHunk`]s.
pub fn compute_hunks(old_text: &str, new_text: &str) -> Vec<DiffHunk> {
    line_diff(old_text, new_text)
        .into_iter()
        .map(|(old_rows, new_rows)| DiffHunk { old_rows, new_rows })
        .collect()
}

/// The unified patch of one changed block, as `git apply --cached` consumes it
/// (`--unidiff-zero`, zero context lines).
///
/// Line numbers are 1-based; an empty side takes the insertion point, as git writes it (0 for
/// a file header). A last line without a line break gets `\ No newline at end of file`, or
/// apply would add one and the index would disagree with the working tree.
///
/// `is_new`: the path is not in the index yet (untracked file). The preimage must be
/// `/dev/null` with `new file mode`, or `git apply --cached` refuses to apply.
pub fn build_hunk_patch(
    path: &str,
    old_text: &str,
    new_text: &str,
    hunk: &DiffHunk,
    is_new: bool,
) -> String {
    let old_starts = line_starts(old_text);
    let new_starts = line_starts(new_text);
    let old_count = hunk.old_rows.end.saturating_sub(hunk.old_rows.start);
    let new_count = hunk.new_rows.end.saturating_sub(hunk.new_rows.start);
    // An empty side takes the 0-based insertion point (0 for a file header); the other side
    // turns 1-based.
    let old_start = if old_count == 0 {
        hunk.old_rows.start
    } else {
        hunk.old_rows.start + 1
    };
    let new_start = if new_count == 0 {
        hunk.new_rows.start
    } else {
        hunk.new_rows.start + 1
    };

    let mut patch = String::new();
    patch.push_str(&format!("diff --git a/{path} b/{path}\n"));
    if is_new {
        patch.push_str("new file mode 100644\n");
        patch.push_str("--- /dev/null\n");
    } else {
        patch.push_str(&format!("--- a/{path}\n"));
    }
    patch.push_str(&format!("+++ b/{path}\n"));
    patch.push_str(&format!(
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
    ));

    let old_no_nl = !old_text.is_empty() && !old_text.ends_with('\n');
    let new_no_nl = !new_text.is_empty() && !new_text.ends_with('\n');
    let old_last = line_count(old_text).saturating_sub(1);
    let new_last = line_count(new_text).saturating_sub(1);

    for row in hunk.old_rows.start..hunk.old_rows.end {
        let range = line_range(old_text, &old_starts, row);
        patch.push('-');
        patch.push_str(&old_text[range]);
        patch.push('\n');
        if old_no_nl && row == old_last {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
    for row in hunk.new_rows.start..hunk.new_rows.end {
        let range = line_range(new_text, &new_starts, row);
        patch.push('+');
        patch.push_str(&new_text[range]);
        patch.push('\n');
        if new_no_nl && row == new_last {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
    patch
}

/// Hunks of `old` to `new` with `context` lines around each change, merged when their
/// context would touch.
fn hunks(old: &str, new: &str, context: u32) -> Vec<Hunk> {
    let old_len = line_count(old);
    let new_len = line_count(new);
    let (old_starts, new_starts) = (line_starts(old), line_starts(new));
    let blocks = line_diff(old, new);

    // Group changes whose context strips would touch.
    let mut groups: Vec<Vec<(Range<u32>, Range<u32>)>> = Vec::new();
    for block in blocks {
        match groups
            .last_mut()
            .and_then(|g| g.last().cloned().map(|l| (g, l)))
        {
            Some((g, last)) if block.0.start - last.0.end <= 2 * context => g.push(block),
            _ => groups.push(vec![block]),
        }
    }

    let line_text = |starts: &[usize], text: &str, row: u32| -> String {
        let range = line_range(text, starts, row);
        let s = &text[range];
        s.strip_suffix('\r').unwrap_or(s).to_owned()
    };

    groups
        .into_iter()
        .map(|g| {
            let (first, last) = (&g[0], &g[g.len() - 1]);
            let start = first.0.start.saturating_sub(context);
            let shift = first.0.start - start;
            let (os, ns) = (start, first.1.start.saturating_sub(shift));
            let end_old = (last.0.end + context).min(old_len);
            let mut lines = Vec::new();
            let (mut i, mut j) = (os, ns);
            for c in &g {
                while i < c.0.start {
                    lines.push(Line::new(
                        LineKind::Context,
                        Some(i + 1),
                        Some(j + 1),
                        &line_text(&old_starts, old, i),
                    ));
                    i += 1;
                    j += 1;
                }
                for n in c.0.clone() {
                    lines.push(Line::new(
                        LineKind::Removed,
                        Some(n + 1),
                        None,
                        &line_text(&old_starts, old, n),
                    ));
                }
                for n in c.1.clone() {
                    lines.push(Line::new(
                        LineKind::Added,
                        None,
                        Some(n + 1),
                        &line_text(&new_starts, new, n),
                    ));
                }
                i = c.0.end;
                j = c.1.end;
            }
            while i < end_old {
                lines.push(Line::new(
                    LineKind::Context,
                    Some(i + 1),
                    Some(j + 1),
                    &line_text(&old_starts, old, i),
                ));
                i += 1;
                j += 1;
            }
            let old_missing = !old.is_empty() && !old.ends_with('\n');
            let new_missing = !new.is_empty() && !new.ends_with('\n');
            for l in &mut lines {
                l.no_newline = match l.kind {
                    LineKind::Removed => l.old == Some(old_len) && old_missing,
                    LineKind::Added => l.new == Some(new_len) && new_missing,
                    LineKind::Context => l.old == Some(old_len) && old_missing,
                };
            }
            let (oc, nc) = (i - os, j - ns);
            Hunk {
                old_start: if oc == 0 { os } else { os + 1 },
                old_count: oc,
                new_start: if nc == 0 { ns } else { ns + 1 },
                new_count: nc,
                section: section(old, os),
                lines,
            }
        })
        .filter(|h| !h.lines.is_empty())
        .collect()
}

/// The nearest line above 0-based line `before` that starts with a letter, `_` or `$` (a
/// function or type at the left margin), as `git diff` names a hunk by default.
fn section(text: &str, before: u32) -> String {
    let starts = line_starts(text);
    (1..=before.min(line_count(text)))
        .rev()
        .take(2000)
        .filter_map(|n| {
            let range = line_range(text, &starts, n - 1);
            text.get(range)
        })
        .find(|l| l.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '$'))
        .map(|l| l.trim_end().chars().take(80).collect())
        .unwrap_or_default()
}

/// Byte ranges of `old` and `new` that differ, by words: runs of letters, digits and `_`,
/// runs of whitespace, and single other characters. Changes separated only by whitespace join
/// into one range. Empty when the lines share too little for word changes to mean anything.
pub fn word_changes(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old == new || old.len() > 1000 || new.len() > 1000 {
        return (Vec::new(), Vec::new());
    }
    let (ta, tb) = (tokens(old), tokens(new));
    let sa: Vec<&str> = ta.iter().map(|r| &old[r.clone()]).collect();
    let sb: Vec<&str> = tb.iter().map(|r| &new[r.clone()]).collect();
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(sa.iter().copied());
    input.update_after(sb.iter().copied());
    let (mut ra, mut rb): (Vec<Range<usize>>, Vec<Range<usize>>) = (Vec::new(), Vec::new());
    // Token ranges (not bytes) that differ, to measure how much content the lines share.
    let mut changed_old: Vec<(usize, usize)> = Vec::new();
    diff(
        Algorithm::Histogram,
        &input,
        |before: std::ops::Range<u32>, after: std::ops::Range<u32>| {
            if !before.is_empty() {
                ra.push(ta[before.start as usize].start..ta[before.end as usize - 1].end);
                changed_old.push((before.start as usize, before.end as usize));
            }
            if !after.is_empty() {
                rb.push(tb[after.start as usize].start..tb[after.end as usize - 1].end);
            }
        },
    );
    let content = |s: &str| s.chars().filter(|c| !c.is_whitespace()).count().max(1);
    let changed = |i: usize| changed_old.iter().any(|&(s, e)| s <= i && i < e);
    let shared: usize = (0..ta.len())
        .filter(|&i| !changed(i))
        .map(|i| sa[i].len())
        .sum();
    if shared * 4 < content(old).max(content(new)) {
        return (Vec::new(), Vec::new());
    }
    (join(old, ra), join(new, rb))
}

fn join(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if text[last.end..r.start].chars().all(char::is_whitespace) => {
                last.end = r.end
            }
            _ => out.push(r),
        }
    }
    out
}

fn tokens(s: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq, Clone, Copy)]
    enum K {
        Word,
        Space,
        Other,
    }
    let kind = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            K::Word
        } else if c.is_whitespace() {
            K::Space
        } else {
            K::Other
        }
    };
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut prev: Option<K> = None;
    for (i, c) in s.char_indices() {
        let k = kind(c);
        match out.last_mut() {
            Some(last) if prev == Some(k) && k != K::Other => last.end = i + c.len_utf8(),
            _ => out.push(i..i + c.len_utf8()),
        }
        prev = Some(k);
    }
    out
}

fn diff_internal(
    input: &InternedInput<&str>,
    on_change: &mut dyn FnMut(Range<usize>, Range<usize>, Range<u32>, Range<u32>),
) {
    let mut old_offset = 0;
    let mut new_offset = 0;
    let mut old_token_ix = 0;
    let mut new_token_ix = 0;
    diff(
        Algorithm::Histogram,
        input,
        |old_tokens: Range<u32>, new_tokens: Range<u32>| {
            old_offset += token_len(
                input,
                &input.before[old_token_ix as usize..old_tokens.start as usize],
            );
            new_offset += token_len(
                input,
                &input.after[new_token_ix as usize..new_tokens.start as usize],
            );
            let old_len = token_len(
                input,
                &input.before[old_tokens.start as usize..old_tokens.end as usize],
            );
            let new_len = token_len(
                input,
                &input.after[new_tokens.start as usize..new_tokens.end as usize],
            );
            let old_byte_range = old_offset..old_offset + old_len;
            let new_byte_range = new_offset..new_offset + new_len;
            old_token_ix = old_tokens.end;
            new_token_ix = new_tokens.end;
            old_offset = old_byte_range.end;
            new_offset = new_byte_range.end;
            on_change(old_byte_range, new_byte_range, old_tokens, new_tokens);
        },
    );
}

fn token_len(input: &InternedInput<&str>, tokens: &[Token]) -> usize {
    tokens
        .iter()
        .map(|token| input.interner[*token].len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_make_hunks_with_three_lines_of_context() {
        let old: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 5\n", "line five\n")
            .replace("line 17\n", "")
            .replace("line 20\n", "line 20\nline 21\n");
        let f = FileDiff::from_texts("a.txt", &old, &new);
        let headers: Vec<String> = f.hunks.iter().map(Hunk::header).collect();
        assert_eq!(
            headers,
            vec!["@@ -2,7 +2,7 @@ line 1", "@@ -14,7 +14,7 @@ line 13"]
        );
        let second: Vec<(LineKind, Option<u32>, Option<u32>)> = f.hunks[1]
            .lines
            .iter()
            .map(|l| (l.kind, l.old, l.new))
            .collect();
        assert_eq!(second[3], (LineKind::Removed, Some(17), None));
        assert_eq!(second.last(), Some(&(LineKind::Added, None, Some(20))));
        assert_eq!((f.additions(), f.deletions()), (2, 2));
        let five = &f.hunks[0].lines;
        let added = five.iter().find(|l| l.kind == LineKind::Added).unwrap();
        assert_eq!(&added.text[added.changes[0].clone()], "five");
    }

    #[test]
    fn a_missing_final_newline_is_a_change() {
        let f = FileDiff::from_texts("a", "one\ntwo", "one\ntwo\n");
        let l: Vec<(LineKind, &str, bool)> = f.hunks[0]
            .lines
            .iter()
            .map(|l| (l.kind, l.text.as_str(), l.no_newline))
            .collect();
        assert_eq!(
            l,
            vec![
                (LineKind::Context, "one", false),
                (LineKind::Removed, "two", true),
                (LineKind::Added, "two", false)
            ]
        );
        let both_missing = FileDiff::from_texts("a", "one\ntwo", "one\nthree");
        let l2: Vec<(LineKind, &str, bool)> = both_missing.hunks[0]
            .lines
            .iter()
            .map(|l| (l.kind, l.text.as_str(), l.no_newline))
            .collect();
        assert_eq!(
            l2,
            vec![
                (LineKind::Context, "one", false),
                (LineKind::Removed, "two", true),
                (LineKind::Added, "three", true)
            ]
        );
        assert!(
            FileDiff::from_texts("a", "same\n", "same\n")
                .hunks
                .is_empty()
        );
        assert!(FileDiff::from_texts("a", "same", "same").hunks.is_empty());
        // CRLF and LF of the same lines are one change (the line breaks).
        let crlf = FileDiff::from_texts("a", "one\r\ntwo\r\n", "one\nthree\n");
        assert_eq!(crlf.hunks[0].lines[0].text, "one");
        assert_eq!(crlf.hunks[0].lines[1].text, "two");
        // NUL bytes mark a binary file.
        let bin = FileDiff::from_texts("img.bin", "a\0b", "c\0d");
        assert!(bin.binary && bin.hunks.is_empty());
    }

    #[test]
    fn an_added_file_and_an_insertion_count_from_zero() {
        let f = FileDiff::between(None, Some("new.rs".into()), "", "fn main() {}\n");
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.hunks[0].header(), "@@ -0,0 +1 @@");
        assert_eq!(f.sole_side(), Some(Side::New));
        let d = FileDiff::between(Some("gone.rs".into()), None, "bye\n", "");
        assert_eq!(d.status, FileStatus::Deleted);
        assert_eq!(d.hunks[0].header(), "@@ -1 +0,0 @@");
        assert_eq!(d.sole_side(), Some(Side::Old));
    }

    #[test]
    fn languages_come_from_the_extension() {
        let f = |p: &str| {
            FileDiff::from_texts(p, "", "x")
                .language()
                .map(str::to_owned)
        };
        assert_eq!(f("src/lib.rs").as_deref(), Some("rs"));
        assert_eq!(f("web/App.tsx").as_deref(), Some("tsx"));
        assert_eq!(f(".gitignore"), None);
        assert_eq!(f("Makefile").as_deref(), Some("Makefile"));
    }

    #[test]
    fn line_helpers_agree_on_trailing_newlines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_of("a\nb", 2), Some("b"));
        assert_eq!(line_of("a\r\nb\r\n", 1), Some("a"));
        assert_eq!(line_of("a\n", 2), None);
    }

    #[test]
    fn staging_patch_keeps_missing_final_newline() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc";
        let hunks = compute_hunks(old, new);
        assert_eq!(hunks.len(), 1);
        let patch = build_hunk_patch("a.txt", old, new, &hunks[0], false);
        assert!(patch.contains("@@ -2,2 +2,2 @@"), "{patch}");
        assert!(patch.contains("\\ No newline at end of file\n"), "{patch}");
        assert!(
            patch.ends_with("+c\n\\ No newline at end of file\n"),
            "{patch}"
        );
    }
}
