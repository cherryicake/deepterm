//! Foldable-region ("crease") computation, ported from Zed.
//!
//! This mirrors `EditorSnapshot::crease_for_buffer_row` /
//! `EditorSnapshot::starts_indent` / `EditorSnapshot::closing_bracket_indent_len`
//! in `zed/crates/editor/src/display_map.rs`: creases are computed **on demand**
//! from line indentation (no precomputed candidate list), and a fold range
//! always starts at the end of the header line and ends at the indentation of
//! the closing bracket line (so the closing delimiter stays visible on the
//! same display line as the fold placeholder), or at the last non-blank row.

use ropey::Rope;

use crate::components::input::{Point, RopeExt as _};

/// Closing bracket pairs used by `closing_bracket_indent_len`.
///
/// Zed resolves the bracket list from the language scope at the row; DeepTerm
/// has no per-language bracket table, so we use the common closing delimiters.
const CLOSING_BRACKETS: [&str; 3] = ["}", "]", ")"];

/// A foldable region, equivalent to Zed's inline `Crease`.
///
/// - `start` is at the **end of the header line** (row, line_len).
/// - `end` is at the closing bracket's indentation (`(closing_row, indent_len)`),
///   or at the end of the last non-blank row when there is no closing row.
///
/// The text between `start` and `end` is what gets hidden and replaced by the
/// `⋯` placeholder; the text after `end` (e.g. the `}` of a function) remains
/// visible on the same display line as the placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crease {
    pub start: Point,
    pub end: Point,
}

impl Crease {
    pub fn range(&self, text: &Rope) -> std::ops::Range<usize> {
        text.point_to_offset(self.start)..text.point_to_offset(self.end)
    }
}

/// Byte length of a line's leading whitespace ("indent"), and whether the
/// whole line is blank. Equivalent to Zed's `LineIndent { raw_len, is_line_blank }`.
fn line_indent(text: &Rope, row: usize) -> (usize, bool) {
    let line = text.slice_line(row);
    let mut raw_len = 0;
    let mut is_blank = true;
    for ch in line.chars() {
        if ch == ' ' || ch == '\t' {
            raw_len += ch.len_utf8();
        } else {
            is_blank = false;
            break;
        }
    }
    // A `\r` remainder after the whitespace still counts as blank.
    if is_blank && raw_len == 0 && !line.chars().any(|c| c != '\r') {
        is_blank = true;
    }
    (raw_len, is_blank)
}

/// Returns true if the given row "starts an indent", i.e. the line is not
/// blank and the next non-blank line is indented further.
///
/// Verbatim port of Zed's `EditorSnapshot::starts_indent`.
pub fn starts_indent(text: &Rope, buffer_row: usize) -> bool {
    let max_row = text.lines_len().saturating_sub(1);
    if buffer_row >= max_row {
        return false;
    }

    let (indent_len, is_blank) = line_indent(text, buffer_row);
    if is_blank {
        return false;
    }

    (buffer_row + 1..=max_row)
        .find_map(|next_row| {
            let (next_indent_len, next_is_blank) = line_indent(text, next_row);
            if next_indent_len > indent_len {
                Some(true)
            } else if !next_is_blank {
                Some(false)
            } else {
                None
            }
        })
        .unwrap_or(false)
}

/// Returns the indent length of `row` if the row starts with a closing
/// bracket, so the fold can end before the closing delimiter and keep it
/// visible.
///
/// Port of Zed's `DisplaySnapshot::closing_bracket_indent_len` (with a static
/// bracket list instead of language scopes).
fn closing_bracket_indent_len(text: &Rope, row: usize) -> Option<usize> {
    let (indent_len, _) = line_indent(text, row);
    let line: String = text
        .slice_line(row)
        .chars()
        .skip_while(|ch| *ch == ' ' || *ch == '\t')
        .collect();
    let line = line.trim_end_matches('\r');
    if CLOSING_BRACKETS.iter().any(|br| line.starts_with(br)) {
        Some(indent_len)
    } else {
        None
    }
}

/// Computes the foldable region ("crease") that starts on `buffer_row`.
///
/// Port of the indent-based branch of Zed's
/// `DisplaySnapshot::crease_for_buffer_row`:
///
/// 1. The row must start an indent (see [`starts_indent`]).
/// 2. Scan forward for the first non-blank row whose indent is not greater —
///    the "closing row".
/// 3. If the closing row starts with a closing bracket, the fold ends at that
///    bracket's indentation (the bracket stays visible next to the `⋯`).
/// 4. Otherwise the fold ends at the end of the last non-blank row before the
///    closing row (or before the end of the buffer).
///
/// Pass `is_line_folded` so a row that already has an active fold does not
/// produce a new crease (matching Zed's `!self.is_line_folded(start.row)`).
pub fn crease_for_buffer_row(
    text: &Rope,
    buffer_row: usize,
    is_line_folded: impl Fn(usize) -> bool,
) -> Option<Crease> {
    let start = Point::new(buffer_row, text.slice_line(buffer_row).len());
    if !starts_indent(text, buffer_row) || is_line_folded(start.row) {
        return None;
    }

    let max_row = text.lines_len().saturating_sub(1);
    let (start_indent_len, _) = line_indent(text, buffer_row);

    let mut closing_row = None;
    for row in (buffer_row + 1)..=max_row {
        let (indent_len, is_blank) = line_indent(text, row);
        if !is_blank && indent_len <= start_indent_len {
            closing_row = Some(row);
            break;
        }
    }

    let last_non_blank_row = |from_row: usize| -> Point {
        let mut row = from_row;
        while row > start.row {
            let (_, is_blank) = line_indent(text, row);
            if !is_blank {
                break;
            }
            row -= 1;
        }
        Point::new(row, text.slice_line(row).len())
    };

    let end = if let Some(row) = closing_row {
        if let Some(indent_len) = closing_bracket_indent_len(text, row) {
            // Include newline and whitespace before the closing delimiter,
            // so it appears on the same display line as the fold placeholder.
            Point::new(row, indent_len)
        } else {
            last_non_blank_row(row - 1)
        }
    } else {
        last_non_blank_row(max_row)
    };

    if end <= start {
        return None;
    }

    Some(Crease { start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crease(text: &str, row: usize) -> Option<Crease> {
        crease_for_buffer_row(&Rope::from(text), row, |_| false)
    }

    #[test]
    fn crease_for_rust_function() {
        let text = "fn main() {\n    let a = 1;\n    let b = 2;\n}\nnext\n";
        // Fold starts at end of line 0, ends at start of `}` on line 3.
        assert_eq!(
            crease(text, 0),
            Some(Crease {
                start: Point::new(0, 11),
                end: Point::new(3, 0),
            })
        );
    }

    #[test]
    fn crease_keeps_indented_closing_bracket_visible() {
        // The closing row (`    }`, indent 4) is the first row whose indent is
        // not greater than the header row 1's indent (4), so the fold ends at
        // (4, 4) — the `}` itself stays visible after the placeholder.
        let text = "mod m {\n    fn main() {\n        let a = 1;\n        }\n    }\n";
        assert_eq!(
            crease(text, 1),
            Some(Crease {
                start: Point::new(1, 15),
                end: Point::new(4, 4),
            })
        );
    }

    #[test]
    fn crease_without_closing_row_ends_at_last_non_blank() {
        let text = "def foo():\n    a = 1\n    b = 2\n\n\n";
        // No subsequent line with indent <= 0, so the fold ends at the last
        // non-blank row (line 2, end).
        assert_eq!(
            crease(text, 0),
            Some(Crease {
                start: Point::new(0, 10),
                end: Point::new(2, 9),
            })
        );
    }

    #[test]
    fn crease_skips_blank_lines_before_closing_row() {
        let text = "fn main() {\n    let a = 1;\n\n}\nnext\n";
        // Blank line 2 is inside the fold; closing row is 3.
        assert_eq!(
            crease(text, 0),
            Some(Crease {
                start: Point::new(0, 11),
                end: Point::new(3, 0),
            })
        );
    }

    #[test]
    fn starts_indent_requires_deeper_next_line() {
        let text = "fn main() {\n    let a = 1;\n}\n";
        assert!(starts_indent(&Rope::from(text), 0));
        assert!(!starts_indent(&Rope::from(text), 1));
        assert!(!starts_indent(&Rope::from(text), 2));
        // Blank line never starts an indent.
        assert!(!starts_indent(&Rope::from("\n  a\n"), 0));
        // Last row can never start an indent.
        assert!(!starts_indent(&Rope::from("a:\n"), 1));
    }

    #[test]
    fn nested_indent_creases() {
        let text = "fn main() {\n    if true {\n        a();\n    }\n}\n";
        assert_eq!(
            crease(text, 0),
            Some(Crease {
                start: Point::new(0, 11),
                end: Point::new(4, 0),
            })
        );
        assert_eq!(
            crease(text, 1),
            Some(Crease {
                start: Point::new(1, 13),
                end: Point::new(3, 4),
            })
        );
    }
}
