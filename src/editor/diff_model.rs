//! Diff data model for the git diff view.
//!
//! Line-level and word-level diff computation is ported from zed's
//! `crates/language/src/text_diff.rs` (imara-diff, Histogram algorithm).
//! The word tokenizer is simplified: word characters are alphanumeric + `_`,
//! language-scope-aware classification is not needed here.

use imara_diff::{
    Algorithm, diff,
    intern::{InternedInput, Token},
    sources::lines_with_terminator,
};
use std::iter;
use std::ops::Range;

/// Same limits as zed: only perform word-level diff inside small hunks.
const MAX_WORD_DIFF_LEN: usize = 512;
const MAX_WORD_DIFF_LINE_COUNT: usize = 8;

/// Number of context lines kept around each change when collapsing
/// unchanged regions.
pub const CONTEXT_LINES: u32 = 4;

/// A fully computed diff between the HEAD (old) and working-tree (new)
/// version of one file.
#[derive(Clone, Debug)]
pub struct DiffFile {
    /// Repository-relative path.
    pub path: String,
    /// Language id used for syntax highlighting (see `guess_language_from_extension`).
    pub language: String,
    /// File content at HEAD. Empty for untracked / newly added files.
    pub old_text: String,
    /// Current working-tree content. Empty for deleted files.
    pub new_text: String,
    /// Unified render rows (with long unchanged regions collapsed).
    pub rows: Vec<DiffRow>,
    pub additions: usize,
    pub deletions: usize,
}

/// One row in the unified diff rendering.
#[derive(Clone, Debug)]
pub enum DiffRow {
    /// Unchanged line. Byte range refers to `new_text` (identical in old).
    Context {
        old_lineno: u32,
        new_lineno: u32,
        new_range: Range<usize>,
    },
    /// Added line. Byte range refers to `new_text`.
    Add {
        new_lineno: u32,
        new_range: Range<usize>,
        /// Word-level changed ranges, absolute in `new_text`.
        word_ranges: Vec<Range<usize>>,
    },
    /// Deleted line. Byte range refers to `old_text`.
    Del {
        old_lineno: u32,
        old_range: Range<usize>,
        /// Word-level changed ranges, absolute in `old_text`.
        word_ranges: Vec<Range<usize>>,
    },
    /// Collapsed run of unchanged lines between two hunks.
    Separator {
        hidden_lines: u32,
        /// Region start line in the new text (0-based); also the id used
        /// to expand this separator via `expanded_separators`.
        new_start: u32,
        /// Region start line in the old text (0-based).
        old_start: u32,
    },
}

impl DiffRow {
    pub fn is_separator(&self) -> bool {
        matches!(self, DiffRow::Separator { .. })
    }
}

/// One changed hunk between old and new text, with word-level detail.
#[derive(Clone, Debug)]
pub struct DiffHunk {
    pub old_rows: Range<u32>,
    pub new_rows: Range<u32>,
    /// Word-level changed byte ranges, absolute in the old text.
    pub old_word_ranges: Vec<Range<usize>>,
    /// Word-level changed byte ranges, absolute in the new text.
    pub new_word_ranges: Vec<Range<usize>>,
}

impl DiffHunk {
    pub fn deletions(&self) -> usize {
        (self.old_rows.end - self.old_rows.start) as usize
    }

    pub fn additions(&self) -> usize {
        (self.new_rows.end - self.new_rows.start) as usize
    }
}

/// Compute line-level hunks plus word-level ranges (for small hunks).
pub fn compute_hunks(old_text: &str, new_text: &str) -> Vec<DiffHunk> {
    let old_starts = line_starts(old_text);
    let new_starts = line_starts(new_text);

    line_diff(old_text, new_text)
        .into_iter()
        .map(|(old_rows, new_rows)| {
            let old_byte_range = if old_rows.start < old_rows.end {
                let s = old_starts[old_rows.start as usize];
                let e = line_range(old_text, &old_starts, old_rows.end - 1).end;
                s..e
            } else {
                0..0
            };
            let new_byte_range = if new_rows.start < new_rows.end {
                let s = new_starts[new_rows.start as usize];
                let e = line_range(new_text, &new_starts, new_rows.end - 1).end;
                s..e
            } else {
                0..0
            };

            let (old_word_ranges, new_word_ranges) = if should_word_diff(
                &old_rows,
                &old_byte_range,
                &new_rows,
                &new_byte_range,
            ) {
                let (mut o, mut n) = word_diff_ranges(
                    &old_text[old_byte_range.clone()],
                    &new_text[new_byte_range.clone()],
                );
                for r in &mut o {
                    r.start += old_byte_range.start;
                    r.end += old_byte_range.start;
                }
                for r in &mut n {
                    r.start += new_byte_range.start;
                    r.end += new_byte_range.start;
                }
                (o, n)
            } else {
                (Vec::new(), Vec::new())
            };

            DiffHunk {
                old_rows,
                new_rows,
                old_word_ranges,
                new_word_ranges,
            }
        })
        .collect()
}

/// Line kind in the merged unified display text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

/// One line of the merged unified display text.
#[derive(Clone, Debug)]
pub struct UnifiedLine {
    pub kind: LineKind,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    /// Byte range of this line in the merged text (no trailing newline).
    pub merged_range: Range<usize>,
    /// Word-level changed byte ranges in the merged text.
    pub word_ranges: Vec<Range<usize>>,
}

/// Merged old+new text for the unified diff editor, with per-line metadata.
#[derive(Clone, Debug)]
pub struct UnifiedSource {
    pub text: String,
    pub lines: Vec<UnifiedLine>,
}

/// Build the merged unified display: context lines interleaved with deleted
/// (old) and added (new) lines, hunks in file order, nothing collapsed.
pub fn build_unified(old_text: &str, new_text: &str) -> UnifiedSource {
    let hunks = compute_hunks(old_text, new_text);
    build_unified_with_hunks(old_text, new_text, &hunks)
}

/// Like [`build_unified`], but reuses already-computed hunks so the
/// (potentially expensive) line diff runs only once per recompute.
pub(crate) fn build_unified_with_hunks(
    old_text: &str,
    new_text: &str,
    hunks: &[DiffHunk],
) -> UnifiedSource {
    let old_starts = line_starts(old_text);
    let new_starts = line_starts(new_text);
    let new_line_count = line_count(new_text, &new_starts);

    let mut text = String::new();
    let mut lines = Vec::new();

    let push_line = |kind: LineKind,
                         old_lineno: Option<u32>,
                         new_lineno: Option<u32>,
                         source: &str,
                         source_range: Range<usize>,
                         word_ranges: &[Range<usize>],
                         text: &mut String,
                         lines: &mut Vec<UnifiedLine>| {
        let merged_start = text.len();
        text.push_str(&source[source_range.clone()]);
        let merged_end = text.len();
        text.push('\n');
        let word_ranges = word_ranges
            .iter()
            .filter(|r| r.start < source_range.end && r.end > source_range.start)
            .map(|r| {
                let s = r.start.max(source_range.start) - source_range.start + merged_start;
                let e = r.end.min(source_range.end) - source_range.start + merged_start;
                s..e
            })
            .collect();
        lines.push(UnifiedLine {
            kind,
            old_lineno,
            new_lineno,
            merged_range: merged_start..merged_end,
            word_ranges,
        });
    };

    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    for hunk in hunks {
        // Context before this hunk (identical in both texts, use new).
        for row in new_cursor..hunk.new_rows.start {
            let range = line_range(new_text, &new_starts, row);
            push_line(
                LineKind::Context,
                Some(old_cursor + (row - new_cursor)),
                Some(row),
                new_text,
                range,
                &[],
                &mut text,
                &mut lines,
            );
        }
        for row in hunk.old_rows.start..hunk.old_rows.end {
            let range = line_range(old_text, &old_starts, row);
            push_line(
                LineKind::Del,
                Some(row),
                None,
                old_text,
                range,
                &hunk.old_word_ranges,
                &mut text,
                &mut lines,
            );
        }
        for row in hunk.new_rows.start..hunk.new_rows.end {
            let range = line_range(new_text, &new_starts, row);
            push_line(
                LineKind::Add,
                None,
                Some(row),
                new_text,
                range,
                &hunk.new_word_ranges,
                &mut text,
                &mut lines,
            );
        }
        old_cursor = hunk.old_rows.end;
        new_cursor = hunk.new_rows.end;
    }
    // Trailing context.
    for row in new_cursor..new_line_count {
        let range = line_range(new_text, &new_starts, row);
        push_line(
            LineKind::Context,
            Some(old_cursor + (row - new_cursor)),
            Some(row),
            new_text,
            range,
            &[],
            &mut text,
            &mut lines,
        );
    }

    UnifiedSource { text, lines }
}

/// One collapsed (hidden) region of unchanged lines in the unified view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeparatorRow {
    /// Merged row index of the placeholder line in the collapsed text.
    pub merged_row: u32,
    /// Expansion key: new-side start line of the hidden region
    /// (deterministic across recomputes).
    pub id: u32,
    /// Number of unchanged rows hidden between the two context strips.
    pub hidden_lines: u32,
}

/// Collapsed unified view: merged text with unchanged gaps reduced to
/// `CONTEXT_LINES` context strips around each hunk plus one placeholder
/// row per collapsed region (GitHub-style expandable separators).
pub(crate) struct CollapsedUnified {
    pub source: UnifiedSource,
    pub separators: Vec<SeparatorRow>,
    /// Merged row of each hunk's first changed line (for hunk navigation
    /// and initial scroll positioning).
    pub hunk_rows: Vec<u32>,
}

/// Like [`build_unified_with_hunks`], but collapses unchanged regions.
/// Regions listed in `expanded` are rendered in full; every other gap
/// larger than `2 * CONTEXT_LINES` becomes a single placeholder row
/// flanked by `CONTEXT_LINES`-line context strips.
pub(crate) fn build_unified_collapsed(
    old_text: &str,
    new_text: &str,
    hunks: &[DiffHunk],
    expanded: &std::collections::HashSet<u32>,
) -> CollapsedUnified {
    let old_starts = line_starts(old_text);
    let new_starts = line_starts(new_text);
    let new_line_count = line_count(new_text, &new_starts);

    let mut text = String::new();
    let mut lines = Vec::new();
    let mut separators = Vec::new();
    let mut hunk_rows = Vec::with_capacity(hunks.len());

    // Push context rows [from..to) from the new side. Unchanged regions have
    // equal line counts on both sides; `old_from` is the corresponding old row.
    fn push_context(
        text: &mut String,
        lines: &mut Vec<UnifiedLine>,
        new_text: &str,
        new_starts: &[usize],
        new_from: u32,
        new_to: u32,
        old_from: u32,
    ) {
        for row in new_from..new_to {
            let range = line_range(new_text, new_starts, row);
            text.push_str(&new_text[range.clone()]);
            let start = text.len() - range.len();
            text.push('\n');
            lines.push(UnifiedLine {
                kind: LineKind::Context,
                old_lineno: Some(old_from + (row - new_from)),
                new_lineno: Some(row),
                merged_range: start..text.len() - 1,
                word_ranges: Vec::new(),
            });
        }
    }

    // Push a single empty placeholder row representing a collapsed region.
    fn push_separator(
        text: &mut String,
        lines: &mut Vec<UnifiedLine>,
        separators: &mut Vec<SeparatorRow>,
        id: u32,
        hidden_lines: u32,
    ) {
        let merged_row = lines.len() as u32;
        let start = text.len();
        text.push('\n');
        lines.push(UnifiedLine {
            kind: LineKind::Context,
            old_lineno: None,
            new_lineno: None,
            merged_range: start..start,
            word_ranges: Vec::new(),
        });
        separators.push(SeparatorRow {
            merged_row,
            id,
            hidden_lines,
        });
    }

    // Push a gap of identical lines, collapsing it when it exceeds the
    // context budget and has not been expanded by the user.
    fn push_gap(
        text: &mut String,
        lines: &mut Vec<UnifiedLine>,
        separators: &mut Vec<SeparatorRow>,
        new_text: &str,
        new_starts: &[usize],
        new_from: u32,
        new_to: u32,
        old_from: u32,
        // `true` for the gap after the last hunk (tail of file).
        trailing: bool,
        expanded: &std::collections::HashSet<u32>,
    ) {
        let gap_len = new_to.saturating_sub(new_from);
        if gap_len == 0 {
            return;
        }
        if gap_len <= 2 * CONTEXT_LINES {
            push_context(text, lines, new_text, new_starts, new_from, new_to, old_from);
            return;
        }

        // Keep `visible` context rows and replace the other `hidden` rows with a
        // single placeholder. Which end the visible rows sit at follows whatever
        // the gap is adjacent to:
        //   - before the next hunk: they must lead into it, so they are the LAST
        //     rows of the gap and the placeholder goes in front of them.
        //   - after the last hunk (trailing): they are the context of that hunk,
        //     so they are the FIRST rows of the gap and the placeholder follows.
        let visible = CONTEXT_LINES.min(gap_len);
        let hidden = gap_len - visible;
        let (vis_from, vis_to, hidden_start) = if trailing {
            (new_from, new_from + visible, new_from + visible)
        } else {
            (new_from + hidden, new_to, new_from)
        };

        // id = new-side start line of the hidden region: stable across
        // recomputes, so it doubles as the expansion key.
        if expanded.contains(&hidden_start) {
            push_context(text, lines, new_text, new_starts, new_from, new_to, old_from);
            return;
        }
        if !trailing {
            push_separator(text, lines, separators, hidden_start, hidden);
        }
        push_context(
            text,
            lines,
            new_text,
            new_starts,
            vis_from,
            vis_to,
            old_from + (vis_from - new_from),
        );
        if trailing {
            push_separator(text, lines, separators, hidden_start, hidden);
        }
    }

    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    for hunk in hunks {
        // Head gap before the first hunk: keep only trailing C rows.
        let gap_new = hunk.new_rows.start.saturating_sub(new_cursor);
        let is_first = old_cursor == 0 && new_cursor == 0;
        if is_first && gap_new > 0 {
            let visible = CONTEXT_LINES.min(gap_new);
            let hidden = gap_new - visible;
            let hidden_id = new_cursor;
            if hidden == 0 || expanded.contains(&hidden_id) {
                push_context(
                    &mut text,
                    &mut lines,
                    new_text,
                    &new_starts,
                    new_cursor,
                    hunk.new_rows.start,
                    old_cursor,
                );
            } else {
                push_separator(&mut text, &mut lines, &mut separators, hidden_id, hidden);
                push_context(
                    &mut text,
                    &mut lines,
                    new_text,
                    &new_starts,
                    hunk.new_rows.start - visible,
                    hunk.new_rows.start,
                    hunk.old_rows.start - visible,
                );
            }
        } else if gap_new > 0 {
            // Between hunks: separator, then context leading into the next hunk.
            push_gap(
                &mut text,
                &mut lines,
                &mut separators,
                new_text,
                &new_starts,
                new_cursor,
                hunk.new_rows.start,
                old_cursor,
                false, // not trailing: the next hunk follows this gap
                expanded,
            );
        }

        hunk_rows.push(lines.len() as u32);
        for row in hunk.old_rows.start..hunk.old_rows.end {
            let range = line_range(old_text, &old_starts, row);
            text.push_str(&old_text[range.clone()]);
            let start = text.len() - range.len();
            text.push('\n');
            lines.push(UnifiedLine {
                kind: LineKind::Del,
                old_lineno: Some(row),
                new_lineno: None,
                merged_range: start..text.len() - 1,
                word_ranges: hunk.old_word_ranges.iter().filter(|r| r.start < range.end && r.end > range.start).map(|r| { let s = r.start.max(range.start) - range.start + start; let e = r.end.min(range.end) - range.start + start; s..e }).collect(),
            });
        }
        for row in hunk.new_rows.start..hunk.new_rows.end {
            let range = line_range(new_text, &new_starts, row);
            text.push_str(&new_text[range.clone()]);
            let start = text.len() - range.len();
            text.push('\n');
            lines.push(UnifiedLine {
                kind: LineKind::Add,
                old_lineno: None,
                new_lineno: Some(row),
                merged_range: start..text.len() - 1,
                word_ranges: hunk.new_word_ranges.iter().filter(|r| r.start < range.end && r.end > range.start).map(|r| { let s = r.start.max(range.start) - range.start + start; let e = r.end.min(range.end) - range.start + start; s..e }).collect(),
            });
        }
        old_cursor = hunk.old_rows.end;
        new_cursor = hunk.new_rows.end;
    }

    // Tail gap after the last hunk: context first, then the placeholder.
    if new_cursor < new_line_count {
        push_gap(
            &mut text,
            &mut lines,
            &mut separators,
            new_text,
            &new_starts,
            new_cursor,
            new_line_count,
            old_cursor,
            true, // trailing: no hunk follows, so the file tail is collapsed
            expanded,
        );
    }

    CollapsedUnified {
        source: UnifiedSource { text, lines },
        separators,
        hunk_rows,
    }
}

/// Byte offsets of every line start in a text, plus a sentinel past the end
/// so `line_range` works for the last line.
pub(crate) fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (ix, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(ix + 1);
        }
    }
    starts.push(text.len() + 1); // sentinel
    starts
}

/// Number of content lines. A trailing newline does not start a new line.
pub(crate) fn line_count(text: &str, starts: &[usize]) -> u32 {
    let n = starts.len() - 1; // exclude sentinel
    if text.is_empty() || text.ends_with('\n') {
        n.saturating_sub(1) as u32
    } else {
        n as u32
    }
}

/// Byte range of line `row` (0-based), excluding the trailing newline.
pub(crate) fn line_range(text: &str, starts: &[usize], row: u32) -> Range<usize> {
    let start = starts[row as usize];
    let mut end = starts[row as usize + 1].min(text.len());
    if end > start && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    start..end
}

impl DiffFile {
    /// Build the diff model from old/new file contents.
    ///
    /// `expanded_separators` contains `new_start` values of separator rows
    /// that the user expanded; those regions are rendered in full.
    pub fn build(
        path: String,
        language: String,
        old_text: String,
        new_text: String,
        expanded_separators: &[u32],
    ) -> Self {
        let hunks = compute_hunks(&old_text, &new_text);
        let new_starts = line_starts(&new_text);
        let old_starts = line_starts(&old_text);
        let old_line_count = line_count(&old_text, &old_starts);
        let new_line_count = line_count(&new_text, &new_starts);

        let mut rows = Vec::new();
        let mut additions = 0usize;
        let mut deletions = 0usize;

        let mut old_cursor = 0u32;
        let mut new_cursor = 0u32;

        let push_context = |old_from: u32,
                                old_to: u32,
                                new_from: u32,
                                new_to: u32,
                                rows: &mut Vec<DiffRow>| {
            let len = new_to - new_from;
            debug_assert_eq!(len, old_to - old_from);
            // Collapse long unchanged runs, keeping CONTEXT_LINES on each side.
            if len > CONTEXT_LINES * 2 + 1 && !expanded_separators.contains(&new_from) {
                for i in 0..CONTEXT_LINES {
                    rows.push(DiffRow::Context {
                        old_lineno: old_from + i,
                        new_lineno: new_from + i,
                        new_range: line_range(&new_text, &new_starts, new_from + i),
                    });
                }
                rows.push(DiffRow::Separator {
                    hidden_lines: len - CONTEXT_LINES * 2,
                    new_start: new_from,
                    old_start: old_from,
                });
                for i in len - CONTEXT_LINES..len {
                    rows.push(DiffRow::Context {
                        old_lineno: old_from + i,
                        new_lineno: new_from + i,
                        new_range: line_range(&new_text, &new_starts, new_from + i),
                    });
                }
            } else {
                for i in 0..len {
                    rows.push(DiffRow::Context {
                        old_lineno: old_from + i,
                        new_lineno: new_from + i,
                        new_range: line_range(&new_text, &new_starts, new_from + i),
                    });
                }
            }
        };

        for hunk in &hunks {
            let old_rows = &hunk.old_rows;
            let new_rows = &hunk.new_rows;
            // Context before this hunk.
            push_context(
                old_cursor,
                old_rows.start,
                new_cursor,
                new_rows.start,
                &mut rows,
            );

            let old_word_ranges = &hunk.old_word_ranges;
            let new_word_ranges = &hunk.new_word_ranges;

            deletions += hunk.deletions();
            additions += hunk.additions();

            for row in old_rows.start..old_rows.end {
                let range = line_range(&old_text, &old_starts, row);
                let word_ranges = old_word_ranges
                    .iter()
                    .filter(|r| r.start < range.end && r.end > range.start)
                    .map(|r| r.start.max(range.start)..r.end.min(range.end))
                    .collect();
                rows.push(DiffRow::Del {
                    old_lineno: row,
                    old_range: range,
                    word_ranges,
                });
            }
            for row in new_rows.start..new_rows.end {
                let range = line_range(&new_text, &new_starts, row);
                let word_ranges = new_word_ranges
                    .iter()
                    .filter(|r| r.start < range.end && r.end > range.start)
                    .map(|r| r.start.max(range.start)..r.end.min(range.end))
                    .collect();
                rows.push(DiffRow::Add {
                    new_lineno: row,
                    new_range: range,
                    word_ranges,
                });
            }

            old_cursor = old_rows.end;
            new_cursor = new_rows.end;
        }

        // Trailing context after the last hunk.
        push_context(
            old_cursor,
            old_line_count,
            new_cursor,
            new_line_count,
            &mut rows,
        );

        DiffFile {
            path,
            language,
            old_text,
            new_text,
            rows,
            additions,
            deletions,
        }
    }
}

/// Computes a line-based diff, returning `(old_row_range, new_row_range)`
/// for each changed hunk. Ported from zed `text_diff.rs::line_diff`.
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

/// Computes word-level diff ranges between two strings.
/// Returns `(old_ranges, new_ranges)` as byte ranges of changed words.
/// Ported from zed `text_diff.rs::word_diff_ranges` with a simplified
/// tokenizer (word chars = alphanumeric + `_`).
pub fn word_diff_ranges(old_text: &str, new_text: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(tokenize(old_text));
    input.update_after(tokenize(new_text));

    let mut old_ranges: Vec<Range<usize>> = Vec::new();
    let mut new_ranges: Vec<Range<usize>> = Vec::new();

    diff_internal(&input, &mut |old_byte_range, new_byte_range, _, _| {
        if !old_byte_range.is_empty() {
            if let Some(last) = old_ranges.last_mut()
                && last.end >= old_byte_range.start
            {
                last.end = old_byte_range.end;
            } else {
                old_ranges.push(old_byte_range);
            }
        }
        if !new_byte_range.is_empty() {
            if let Some(last) = new_ranges.last_mut()
                && last.end >= new_byte_range.start
            {
                last.end = new_byte_range.end;
            } else {
                new_ranges.push(new_byte_range);
            }
        }
    });

    (old_ranges, new_ranges)
}

fn should_word_diff(
    old_rows: &Range<u32>,
    old_byte_range: &Range<usize>,
    new_rows: &Range<u32>,
    new_byte_range: &Range<usize>,
) -> bool {
    !old_byte_range.is_empty()
        && !new_byte_range.is_empty()
        && old_byte_range.len() <= MAX_WORD_DIFF_LEN
        && new_byte_range.len() <= MAX_WORD_DIFF_LEN
        && old_rows.len() <= MAX_WORD_DIFF_LINE_COUNT
        && new_rows.len() <= MAX_WORD_DIFF_LINE_COUNT
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

/// Simplified word tokenizer: word chars are alphanumeric + `_`;
/// runs of whitespace are single tokens; each punctuation char is its own
/// token (same splitting policy as zed's CharClassifier-based tokenizer).
fn tokenize(text: &str) -> impl Iterator<Item = &str> {
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Word,
        Whitespace,
        Punctuation,
    }

    fn kind(c: char) -> Kind {
        if c.is_alphanumeric() || c == '_' {
            Kind::Word
        } else if c.is_whitespace() {
            Kind::Whitespace
        } else {
            Kind::Punctuation
        }
    }

    let mut chars = text.char_indices();
    let mut prev: Option<(char, Kind)> = None;
    let mut start_ix = 0;
    iter::from_fn(move || {
        for (ix, c) in chars.by_ref() {
            let mut token = None;
            let k = kind(c);
            if let Some((prev_char, prev_kind)) = prev
                && (k != prev_kind || (k == Kind::Punctuation && c != prev_char))
            {
                token = Some(&text[start_ix..ix]);
                start_ix = ix;
            }
            prev = Some((c, k));
            if token.is_some() {
                return token;
            }
        }
        if start_ix < text.len() {
            let token = &text[start_ix..];
            start_ix = text.len();
            return Some(token);
        }
        None
    })
}

fn token_len(input: &InternedInput<&str>, tokens: &[Token]) -> usize {
    tokens
        .iter()
        .map(|token| input.interner[*token].len())
        .sum()
}

// Re-export nothing; keep the module self-contained.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_diff_basic() {
        let old = "one\ntwo\nthree\n";
        let new = "one\nTWO\nthree\n";
        let hunks = line_diff(old, new);
        assert_eq!(hunks, vec![(1..2, 1..2)]);
    }

    #[test]
    fn test_line_diff_insert() {
        let old = "one\nthree\n";
        let new = "one\ntwo\nthree\n";
        let hunks = line_diff(old, new);
        assert_eq!(hunks, vec![(1..1, 1..2)]);
    }

    #[test]
    fn test_word_diff_ranges() {
        let old = "let x = 1;";
        let new = "let x = 42;";
        let (old_ranges, new_ranges) = word_diff_ranges(old, new);
        assert_eq!(old_ranges, vec![8..9]);
        assert_eq!(new_ranges, vec![8..10]);
    }

    #[test]
    fn test_tokenize() {
        let text = "one.two(three)";
        assert_eq!(
            tokenize(text).collect::<Vec<_>>(),
            vec!["one", ".", "two", "(", "three", ")"]
        );
    }

    #[test]
    fn test_build_modified_file() {
        let old = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n".to_string();
        let new = "fn main() {\n    let x = 2;\n    println!(\"{x}\");\n}\n".to_string();
        let diff = DiffFile::build("a.rs".into(), "rust".into(), old, new, &[]);
        assert_eq!(diff.additions, 1);
        assert_eq!(diff.deletions, 1);
        // 3 context rows (lines are short, no separator) + 1 del + 1 add
        assert_eq!(diff.rows.len(), 5);
        let add = diff
            .rows
            .iter()
            .find_map(|r| match r {
                DiffRow::Add {
                    new_range,
                    word_ranges,
                    ..
                } => Some((new_range, word_ranges)),
                _ => None,
            })
            .expect("should have an Add row");
        assert_eq!(&diff.new_text[add.0.clone()], "    let x = 2;");
        assert!(!add.1.is_empty(), "word ranges should highlight `2`");
    }

    #[test]
    fn test_build_new_file_all_added() {
        let diff = DiffFile::build(
            "new.rs".into(),
            "rust".into(),
            String::new(),
            "a\nb\nc\n".to_string(),
            &[],
        );
        assert_eq!(diff.additions, 3);
        assert_eq!(diff.deletions, 0);
        assert!(diff.rows.iter().all(|r| matches!(r, DiffRow::Add { .. })));
    }

    #[test]
    fn test_build_collapses_long_context() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..40 {
            if i == 20 {
                old.push_str("old_line\n");
                new.push_str("new_line\n");
            } else {
                let line = format!("line{i}\n");
                old.push_str(&line);
                new.push_str(&line);
            }
        }
        let diff = DiffFile::build("f.txt".into(), "text".into(), old, new, &[]);
        let separators = diff.rows.iter().filter(|r| r.is_separator()).count();
        assert_eq!(separators, 2, "context before and after should collapse");

        // Expanded separators render everything.
        let sep_starts: Vec<u32> = diff
            .rows
            .iter()
            .filter_map(|r| match r {
                DiffRow::Separator { new_start, .. } => Some(*new_start),
                _ => None,
            })
            .collect();
        let diff2 = DiffFile::build(
            "f.txt".into(),
            "text".into(),
            diff.old_text.clone(),
            diff.new_text.clone(),
            &sep_starts,
        );
        assert!(!diff2.rows.iter().any(|r| r.is_separator()));
        // 39 context lines + 1 del + 1 add
        assert_eq!(diff2.rows.len(), 41);
    }

    #[test]
    fn test_build_unified() {
        let old = "fn main() {\n    let x = 1;\n}\n".to_string();
        let new = "fn main() {\n    let x = 2;\n    dbg!(x);\n}\n".to_string();
        let unified = build_unified(&old, &new);
        // context: "fn main() {" ; del x=1 ; add x=2 ; add dbg! ; context "}"
        assert_eq!(unified.lines.len(), 5);
        assert_eq!(unified.lines[0].kind, LineKind::Context);
        assert_eq!(unified.lines[1].kind, LineKind::Del);
        assert_eq!(unified.lines[2].kind, LineKind::Add);
        assert_eq!(unified.lines[3].kind, LineKind::Add);
        assert_eq!(unified.lines[4].kind, LineKind::Context);

        let del = &unified.lines[1];
        assert_eq!(&unified.text[del.merged_range.clone()], "    let x = 1;");
        // word range should point at `1` in the merged text
        assert_eq!(del.word_ranges.len(), 1);
        assert_eq!(&unified.text[del.word_ranges[0].clone()], "1");

        let add = &unified.lines[2];
        assert_eq!(add.word_ranges.len(), 1);
        // Histogram alignment may include the trailing `;` in the changed run
        // (same merging behavior as zed); the range must start at `2`.
        assert!(unified.text[add.word_ranges[0].clone()].starts_with('2'));

        // Merged text reconstructs all lines
        assert_eq!(unified.text.lines().count(), 5);
    }

    #[test]
    fn test_compute_hunks_pure_addition() {
        let old = "a\nb\n".to_string();
        let new = "a\nx\nb\n".to_string();
        let hunks = compute_hunks(&old, &new);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_rows, 1..1);
        assert_eq!(hunks[0].new_rows, 1..2);
        assert!(hunks[0].old_word_ranges.is_empty());
    }
}

#[cfg(test)]
mod collapsed_tests {
    use super::*;
    use std::collections::HashSet;

    /// old: 20 行相同 + 改动 + 20 行相同；new 同结构
    fn sample_texts() -> (String, String) {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..20 {
            old.push_str(&format!("ctx{i}\n"));
            new.push_str(&format!("ctx{i}\n"));
        }
        old.push_str("old line\n");
        new.push_str("new line A\n");
        new.push_str("new line B\n");
        for i in 20..40 {
            old.push_str(&format!("tail{i}\n"));
            new.push_str(&format!("tail{i}\n"));
        }
        (old, new)
    }

    #[test]
    fn small_gap_fully_visible() {
        // gap = 1 行，远小于 2*CONTEXT_LINES → 无分隔行
        let hunks = compute_hunks("a\nb\nc\n", "a\nX\nc\n");
        let out = build_unified_collapsed("a\nb\nc\n", "a\nX\nc\n", &hunks, &HashSet::new());
        assert!(out.separators.is_empty());
    }

    #[test]
    fn large_gap_collapsed_with_placeholder() {
        let (old, new) = sample_texts();
        let hunks = compute_hunks(&old, &new);
        assert_eq!(hunks.len(), 1);
        let out = build_unified_collapsed(&old, &new, &hunks, &HashSet::new());
        // 头部 20 行 → 分隔(16) + 尾部 4 行上下文；尾部同理
        assert_eq!(out.separators.len(), 2);
        assert_eq!(out.separators[0].id, 0); // 文件头隐藏区从第 0 行开始
        assert_eq!(out.separators[0].hidden_lines, 20 - CONTEXT_LINES);
        assert_eq!(out.separators[1].hidden_lines, 20 - CONTEXT_LINES);
        // 占位行是空行（merged_range 为空）
        for sep in &out.separators {
            let line = &out.source.lines[sep.merged_row as usize];
            assert_eq!(line.merged_range.start, line.merged_range.end);
            assert!(line.old_lineno.is_none() && line.new_lineno.is_none());
        }
        // 总行数 = 全量行数 - 两个隐藏区 + 2 个占位行
        let full = build_unified_with_hunks(&old, &new, &hunks);
        assert_eq!(
            out.source.lines.len(),
            full.lines.len() - 2 * (20 - CONTEXT_LINES as usize) + 2
        );
    }

    #[test]
    fn expanded_region_rendered_in_full() {
        let (old, new) = sample_texts();
        let hunks = compute_hunks(&old, &new);
        let mut expanded = HashSet::new();
        expanded.insert(0u32); // 展开头部分隔区
        let out = build_unified_collapsed(&old, &new, &hunks, &expanded);
        assert_eq!(out.separators.len(), 1); // 只剩尾部
        // 尾部：new 22..42 是尾部 gap，前 CONTEXT_LINES 行作为上下文保留，
        // 隐藏区从 22 + CONTEXT_LINES 开始。
        assert_eq!(out.separators[0].id, 22 + CONTEXT_LINES);
    }

    #[test]
    fn tail_gap_keeps_context_adjacent_to_last_hunk() {
        // 回归：尾部折叠必须保留紧贴最后一个 hunk 的上下文行，
        // 占位行应排在它们之后，而不是跑到文件末尾下方。
        let (old, new) = sample_texts();
        let hunks = compute_hunks(&old, &new);
        let out = build_unified_collapsed(&old, &new, &hunks, &HashSet::new());

        let seps: Vec<u32> = out.separators.iter().map(|s| s.merged_row).collect();
        let last_sep = *seps.last().unwrap() as usize;

        // 占位行之前的那几行必须是 hunk 结束后紧接着的 tail20..tail23，
        // 而不是文件末尾的 tail36..tail39。
        let ctx: Vec<&str> = (last_sep - CONTEXT_LINES as usize..last_sep)
            .map(|i| &out.source.text[out.source.lines[i].merged_range.clone()])
            .collect();
        assert_eq!(ctx, vec!["tail20", "tail21", "tail22", "tail23"]);

        // 占位行之后不应再有任何内容行。
        assert_eq!(last_sep, out.source.lines.len() - 1);

        // 且它们必须是 new 侧的 22..26（hunk 结束后的前 4 行），
        // 而不是文件末尾的 38..41。
        for (offset, i) in (last_sep - CONTEXT_LINES as usize..last_sep).enumerate() {
            assert_eq!(out.source.lines[i].new_lineno, Some(22 + offset as u32));
        }
    }

    #[test]
    fn hunk_rows_point_at_changed_lines() {
        let (old, new) = sample_texts();
        let hunks = compute_hunks(&old, &new);
        let out = build_unified_collapsed(&old, &new, &hunks, &HashSet::new());
        assert_eq!(out.hunk_rows.len(), hunks.len());
        // hunk 首行应是 Del 或 Add（变更行本身）
        let line = &out.source.lines[out.hunk_rows[0] as usize];
        assert!(matches!(line.kind, LineKind::Del | LineKind::Add));
    }

    #[test]
    fn empty_diff_no_separators() {
        let text = "same\nlines\n";
        let out = build_unified_collapsed(text, text, &[], &HashSet::new());
        // 无 hunk：尾部 gap 走 push_gap，2 行 < 2*C → 全可见
        assert!(out.separators.is_empty());
        assert!(out.hunk_rows.is_empty());
        assert_eq!(out.source.lines.len(), 2);
    }
}
