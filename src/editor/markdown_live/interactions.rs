//! Pure editing interactions for live markdown (zero GPUI).
//!
//! Ported from guise `markdown/editor.rs:1406-1537` pure helpers plus the
//! `toggle_task`/`on_enter`/`on_tab`/`backspace_marker` logic expressed
//! against [`crate::editor::markdown_live::model::EditorModel`]. The view
//! layer (`view.rs`) wraps these in `buffer.edit(...)` transactions.

use crate::editor::markdown_live::block::{classify, table_delimiter_cols, Block, DocState};
use std::ops::Range;

use crate::editor::markdown_live::layout::{col_for_byte, RowKind};
use crate::editor::markdown_live::model::{EditorModel, Pos};

/// Ordered-list display numbering (preview parity).
///
/// CommonMark/pulldown-cmark semantics the read-only preview renders:
/// each list's first item shows its source number and subsequent items
/// increment; a *nested* list starts from its own source number; blank
/// lines continue a (loose) list; any other block row — or a bullet/task
/// item, which starts a different list — ends the runs at that depth and
/// deeper. A single global counter (the old code) mis-numbered nested and
/// post-bullet lists.
#[derive(Default)]
pub struct OrderedCounter {
    /// Next display number per nesting depth.
    next: Vec<u64>,
}

impl OrderedCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that a row of `kind` (indent `cols` columns) is being rendered;
    /// returns the display number when the row is an ordered item.
    pub fn visit(&mut self, kind: &RowKind, cols: usize) -> Option<u64> {
        match kind {
            RowKind::Ordered { number, .. } => {
                let depth = cols / 2;
                let n = self.next.get(depth).copied().unwrap_or(*number);
                self.next.truncate(depth + 1);
                while self.next.len() <= depth {
                    self.next.push(0);
                }
                self.next[depth] = n + 1;
                Some(n)
            }
            RowKind::Bullet { .. } | RowKind::Task { .. } => {
                self.next.truncate(cols / 2);
                None
            }
            RowKind::Blank => None,
            _ => {
                self.next.clear();
                None
            }
        }
    }
}

/// Indent columns of a list row kind (0 for non-list rows).
pub fn list_cols(kind: &RowKind) -> usize {
    match kind {
        RowKind::Bullet { cols } | RowKind::Ordered { cols, .. } | RowKind::Task { cols, .. } => {
            *cols
        }
        _ => 0,
    }
}

/// Classify a single line without document context (fences/frontmatter need
/// the full pass; lists/quotes/headings don't). Matches guise
/// `classify_alone`: feed one empty line so a lone `---` reads as a rule.
pub fn classify_alone(line: &str) -> Block {
    let mut state = DocState::default();
    classify("", &mut state);
    classify(line, &mut state)
}

/// True for rows where rich inline toggles (Cmd+B/I/K) must insert literally
/// instead: code, fences and frontmatter are source-only.
pub fn is_source_only(block: &Block) -> bool {
    matches!(
        block,
        Block::CodeLine | Block::Fence { .. } | Block::FrontMatter | Block::Table
    )
}

/// The list/quote marker continuing `line` onto the next line, ready to
/// insert after Enter. `None` when the line isn't a continuable item.
pub fn continuation(line: &str) -> Option<String> {
    match classify_alone(line) {
        Block::Task { indent, .. } => {
            let ch = line.as_bytes().get(indent).copied().unwrap_or(b'-') as char;
            Some(format!("{ch} [ ] "))
        }
        Block::Bullet { indent, .. } => {
            let ch = line.as_bytes().get(indent).copied().unwrap_or(b'-') as char;
            Some(format!("{ch} "))
        }
        Block::Ordered { indent, number, .. } => {
            let delim = line[indent..]
                .bytes()
                .find(|b| *b == b'.' || *b == b')')
                .unwrap_or(b'.') as char;
            Some(format!("{}{delim} ", number + 1))
        }
        Block::Quote { depth, .. } => Some("> ".repeat(depth as usize)),
        _ => None,
    }
}

/// Where a continuable item's marker prefix ends (its content offset).
pub fn prefix_end(line: &str) -> usize {
    match classify_alone(line) {
        Block::Task { content, .. }
        | Block::Bullet { content, .. }
        | Block::Ordered { content, .. }
        | Block::Quote { content, .. } => content,
        _ => 0,
    }
}

/// Handle Enter on the cursor line. Returns true when handled.
/// - Empty item (`marker` + blank rest) → strip the marker, exit the list.
/// - Continuable item → newline + marker.
/// - Else → plain newline (indent preserved by `EditorModel::newline`).
pub fn on_enter(model: &mut EditorModel, line_text: &str, in_code: bool) -> bool {
    if model.selection().is_some() || in_code {
        model.newline();
        return true;
    }
    let Some(marker) = continuation(line_text) else {
        model.newline();
        return true;
    };
    let rest = line_text.get(prefix_end(line_text)..).unwrap_or("");
    if rest.trim().is_empty() {
        // Empty item: delete the whole marker and leave the list.
        let col = model.cursor().col;
        let line = model.cursor().line;
        let end = prefix_end(line_text).min(col);
        model.move_to(line, 0, false);
        model.move_to(line, end, true);
        model.delete_selection();
        return true;
    }
    model.newline();
    model.insert(&marker);
    true
}

/// Handle Tab / Shift+Tab. List items indent/outdent by `tab_size` spaces
/// from anywhere on the line; otherwise Tab inserts to the next stop
/// (outdent is a no-op there).
pub fn on_tab(model: &mut EditorModel, line_text: &str, outdent: bool, tab_size: usize) {
    let is_item = matches!(
        classify_alone(line_text),
        Block::Bullet { .. } | Block::Ordered { .. } | Block::Task { .. }
    );
    if !is_item {
        if !outdent {
            model.tab();
        }
        return;
    }
    let n = tab_size.max(1);
    let line = model.cursor().line;
    let col = model.cursor().col;
    if outdent {
        let lead = line_text
            .chars()
            .take_while(|c| *c == ' ')
            .take(n)
            .count();
        if lead > 0 {
            model.move_to(line, 0, false);
            model.move_to(line, lead, true);
            model.delete_selection();
        }
    } else {
        let pad: String = " ".repeat(n);
        model.move_to(line, 0, false);
        model.insert(&pad);
        model.move_to(line, col + n, false);
    }
}

/// Strip a list/quote marker when Backspace sits at the content start.
/// Returns true when a marker was removed.
pub fn backspace_marker(model: &mut EditorModel, line_text: &str) -> bool {
    let (content, is_marked) = match classify_alone(line_text) {
        Block::Bullet { content, .. }
        | Block::Ordered { content, .. }
        | Block::Task { content, .. }
        | Block::Quote { content, .. } => (content, true),
        _ => (0, false),
    };
    if !is_marked || model.cursor().col != content {
        return false;
    }
    let line = model.cursor().line;
    model.move_to(line, 0, false);
    model.move_to(line, content, true);
    model.delete_selection();
    true
}

/// Toggle the checkbox on `line`. Returns true when the line is a task.
pub fn toggle_task(model: &mut EditorModel, line_text: &str, line: usize) -> bool {
    let Block::Task { checked, state, .. } = classify_alone(line_text) else {
        return false;
    };
    let saved = model.cursor();
    let saved_sel = model.selection();
    model.move_to(line, state, false);
    model.move_to(line, state + 1, true);
    model.insert(if checked { " " } else { "x" });
    // Restore cursor (shifted by 0: same-length replacement).
    match saved_sel {
        Some((s, e)) => {
            model.move_to(s.line, s.col, false);
            model.move_to(e.line, e.col, true);
        }
        None => model.move_to(saved.line, saved.col, false),
    }
    true
}

/// Toggle `marker` (`**`/`*`) around the selection or word at caret.
/// Returns true when handled (read-only and multiline are rejected by the
/// caller; source-only lines must be checked before calling).
pub fn toggle_wrap(model: &mut EditorModel, marker: &str) -> bool {
    if model.selection().is_none() {
        model.select_word();
    }
    let Some((s, e)) = model.selection() else {
        // Empty spot: insert a pair and park inside.
        model.insert(&format!("{marker}{marker}"));
        let cur = model.cursor();
        let back = marker.chars().count();
        model.move_to(cur.line, cur.col.saturating_sub(back), false);
        return true;
    };
    if s.line != e.line {
        return true; // multiline: handled (no-op), stop propagation
    }
    let mlen = marker.chars().count();
    // Expand when the selection sits exactly inside markers.
    let line = model.line(s.line).unwrap_or_default().to_string();
    let chars: Vec<char> = line.chars().collect();
    let starts_inside = s.col >= mlen
        && chars.get(s.col - mlen..s.col).map(|w| w.iter().collect::<String>()) == Some(marker.to_string());
    let ends_inside = chars.get(e.col..e.col + mlen).map(|w| w.iter().collect::<String>()) == Some(marker.to_string());
    let (mut s, mut e) = (s, e);
    if starts_inside && ends_inside {
        s = Pos::new(s.line, s.col - mlen);
        e = Pos::new(e.line, e.col + mlen);
    }
    let sel_text = {
        let line = model.line(s.line).unwrap_or_default();
        let chars: Vec<char> = line.chars().collect();
        let (a, b) = (s.col.min(chars.len()), e.col.min(chars.len()));
        if a >= b {
            None
        } else {
            Some(chars[a..b].iter().collect::<String>())
        }
    };
    let unwrapped = sel_text
        .as_ref()
        .is_some_and(|t| t.starts_with(marker) && t.ends_with(marker) && t.len() >= 2 * mlen);
    let replacement = match sel_text {
        Some(t) if unwrapped => t[mlen..t.len() - mlen].to_string(),
        Some(t) => format!("{marker}{t}{marker}"),
        None => return true,
    };
    model.move_to(s.line, s.col, false);
    model.move_to(e.line, e.col, true);
    model.insert(&replacement);
    true
}

/// Wrap the selection as `[sel]()` (caret into parens) or insert `[]()`
/// (caret into brackets). Single-line selections only; caller checks.
pub fn insert_link(model: &mut EditorModel) -> bool {
    match model.selection() {
        Some((s, e)) if s.line == e.line => {
            let cur = e;
            model.insert("]()");
            // Re-select original + brackets: insert `[` at start.
            model.move_to(s.line, s.col, false);
            model.insert("[");
            // Park caret inside parens: end moved by `[` (1) + `]()` (3).
            model.move_to(cur.line, cur.col + 1 + 3 - 1, false);
            true
        }
        _ => {
            model.insert("[]()");
            let cur = model.cursor();
            model.move_to(cur.line, cur.col.saturating_sub(3), false);
            true
        }
    }
}

/// The selected char-column range on `line` for normalized `(start, end)`,
/// or `None` on a miss. Bool = continues past this line's end.
/// 文档字数统计（Typora 状态栏 parity）：非空白字符数 + 行数。
/// 空文档计 1 行；中英文统一按字符计（`共 N 字`）。
pub fn word_stats(text: &str) -> (usize, usize) {
    let chars = text.chars().filter(|c| !c.is_whitespace()).count();
    let lines = if text.is_empty() { 1 } else { text.lines().count() };
    (chars, lines)
}

/// Cell spans of a table source row as byte ranges (Typora grid nav).
///
/// Splits on `|` outside code spans, honoring `\\|` escapes; the outer
/// pipes may be absent (GFM pipeless rows) — each segment still counts.
pub fn table_cell_bounds(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut bounds = Vec::new();
    let mut start = 0usize;
    let mut in_code = false;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            // Skip the escaped char (overshoot at EOL safely ends the loop;
            // mid-UTF8 bytes never match ASCII below, so bounds stay valid).
            b'\\' => i += 2,
            b'`' => {
                in_code = !in_code;
                i += 1;
            }
            b'|' if !in_code => {
                bounds.push((start, i));
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    bounds.push((start, bytes.len()));
    // Outer pipes produce zero-width edge cells (`| a |` scans as
    // `(0,0),(1,4),(5,5)`): drop them so the count matches GFM columns.
    if line.trim_start().starts_with('|')
        && matches!(bounds.first(), Some((a, b)) if a == b)
    {
        bounds.remove(0);
    }
    if line.trim_end().ends_with('|')
        && matches!(bounds.last(), Some((a, b)) if a == b)
    {
        bounds.pop();
    }
    if bounds.is_empty() {
        bounds.push((0, line.len()));
    }
    bounds
}

/// Char column of `byte` in `line` (interior offsets round down).
fn char_col(line: &str, byte: usize) -> usize {
    line.get(..byte.min(line.len()))
        .unwrap_or("")
        .chars()
        .count()
}

/// Content char-cols `(start, end)` of each cell: trimmed of padding, so
/// the caret lands on text (empty cells land just before the right pipe).
fn table_cell_contents(line: &str) -> Vec<(usize, usize)> {
    table_cell_bounds(line)
        .iter()
        .map(|(s, e)| {
            let cell = &line[*s..*e];
            let lead = cell.len() - cell.trim_start().len();
            let trail = cell.len() - cell.trim_end().len();
            let cs = char_col(line, s + lead);
            // Empty cells (`|  |`) would invert: clamp to the start.
            (cs, char_col(line, e - trail).max(cs))
        })
        .collect()
}

/// Move the cursor to the next (`forward`) / previous table cell, spanning
/// the whole grid via `is_table`. Past the last cell a fresh empty row is
/// appended (Typora); before the first cell the cursor goes to line start.
/// Returns true when handled (the caller owns selection clearing, if any).
pub fn table_nav(model: &mut EditorModel, forward: bool, is_table: &dyn Fn(usize) -> bool) -> bool {
    let cur = model.cursor();
    let Some(line_text) = model.line(cur.line).map(str::to_string) else {
        return false;
    };
    let cells = table_cell_contents(&line_text);
    if cells.is_empty() {
        return false;
    }
    // Cell stops are content starts: forward takes the first stop past
    // the cursor, backward the last stop before it (strict on both sides,
    // so Tab/Shift+Tab never get stuck inside one cell).
    if forward {
        if let Some((cs, _)) = cells.iter().find(|(cs, _)| *cs > cur.col).copied() {
            model.move_to(cur.line, cs, false);
            return true;
        }
        let next = cur.line + 1;
        if is_table(next) {
            let text = model.line(next).map(str::to_string).unwrap_or_default();
            let (cs, _) = table_cell_contents(&text).first().copied().unwrap_or((0, 0));
            model.move_to(next, cs, false);
            return true;
        }
        return table_insert_row_below(model);
    }
    if let Some((cs, _)) = cells.iter().rev().find(|(cs, _)| *cs < cur.col).copied() {
        model.move_to(cur.line, cs, false);
        return true;
    }
    if cur.line > 0 && is_table(cur.line - 1) {
        let prev = cur.line - 1;
        let text = model.line(prev).map(str::to_string).unwrap_or_default();
        let span = table_cell_contents(&text).last().copied().unwrap_or((0, 0));
        model.move_to(prev, span.0, false);
        return true;
    }
    model.move_to(cur.line, 0, false);
    true
}

/// Insert an empty table row below the cursor row and land in its first
/// cell (Typora Enter). When the next line is the delimiter row, the new
/// row goes below it so header + delimiter stay glued.
pub fn table_insert_row_below(model: &mut EditorModel) -> bool {
    let cur = model.cursor();
    let Some(line_text) = model.line(cur.line).map(str::to_string) else {
        return false;
    };
    let cols = table_cell_bounds(&line_text).len();
    if cols == 0 {
        return false;
    }
    let mut target = cur.line;
    if let Some(next) = model.line(cur.line + 1).map(str::to_string)
        && table_delimiter_cols(&next) == Some(cols)
    {
        target = cur.line + 1;
    }
    model.move_to(target, usize::MAX, false);
    model.newline();
    model.insert(&format!("|{}", "  |".repeat(cols)));
    let fresh = model.line(target + 1).map(str::to_string).unwrap_or_default();
    let (cs, _) = table_cell_contents(&fresh).first().copied().unwrap_or((0, 0));
    model.move_to(target + 1, cs, false);
    true
}

/// [`Pos`] of full-text byte `off` (lines joined by single newlines).
/// Interior offsets floor to the char start; past-end clamps to doc end.
pub fn pos_of_offset(lines: &[String], mut off: usize) -> Pos {
    for (i, line) in lines.iter().enumerate() {
        if off <= line.len() {
            return Pos::new(i, col_for_byte(line, off));
        }
        off -= line.len() + 1;
    }
    let last = lines.len().saturating_sub(1);
    Pos::new(
        last,
        lines.get(last).map(|l| l.chars().count()).unwrap_or(0),
    )
}

/// Next/prev match index from a caret byte offset (mirrors the code
/// editor's `jump_target`): next is the first match at/after the caret, or
/// the one after when the caret sits inside a match (wraps); prev mirrors
/// it. After a jump the selection covers the match, so `start < caret`
/// alone would re-pick the same match for "prev" — the inside-match arms
/// guard exactly that.
pub fn find_jump_target(ranges: &[Range<usize>], caret: usize, dir: i32) -> Option<usize> {
    let len = ranges.len();
    if len == 0 {
        return None;
    }
    if dir >= 0 {
        for (ix, r) in ranges.iter().enumerate() {
            if caret < r.start {
                return Some(ix);
            }
            if caret < r.end {
                return Some((ix + 1) % len);
            }
        }
        return Some(0);
    }
    for (ix, r) in ranges.iter().enumerate().rev() {
        if r.start < caret {
            if caret <= r.end {
                return Some((ix + len - 1) % len);
            }
            return Some(ix);
        }
    }
    Some(len - 1)
}

pub fn line_selection(
    start: Pos,
    end: Pos,
    line: usize,
    line_len: usize,
) -> Option<(usize, usize, bool)> {
    if line < start.line || line > end.line {
        return None;
    }
    let s = if line == start.line {
        start.col.min(line_len)
    } else {
        0
    };
    let e = if line == end.line {
        end.col.min(line_len)
    } else {
        line_len
    };
    let e = e.max(s);
    let newline = line < end.line;
    if e == s && !newline {
        return None;
    }
    Some((s, e, newline))
}

/// Visual rows spanned by visible range, given wrap-boundary bytes.
pub fn split_visual(bounds: &[usize], vs: usize, ve: usize) -> (usize, usize) {
    let sr = bounds.iter().filter(|&&b| b <= vs).count();
    let er = bounds.iter().filter(|&&b| b < ve).count();
    (sr, er.max(sr))
}

/// Adjust scroll offset (0/negative) so `top..bottom` is inside `view`.
pub fn scroll_adjust(offset: f32, view: f32, top: f32, bottom: f32) -> f32 {
    let mut adjusted = offset;
    if bottom + adjusted > view {
        adjusted = view - bottom;
    }
    if top + adjusted < 0.0 {
        adjusted = -top;
    }
    adjusted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pos_of_offset_roundtrips_lines() {
        let lines = vec!["ab".to_string(), "cde".to_string(), "".to_string()];
        assert_eq!(pos_of_offset(&lines, 0), Pos::new(0, 0));
        assert_eq!(pos_of_offset(&lines, 2), Pos::new(0, 2));
        assert_eq!(pos_of_offset(&lines, 3), Pos::new(1, 0));
        assert_eq!(pos_of_offset(&lines, 6), Pos::new(1, 3));
        assert_eq!(pos_of_offset(&lines, 7), Pos::new(2, 0));
        assert_eq!(pos_of_offset(&lines, 999), Pos::new(2, 0));
    }

    #[test]
    fn find_jump_target_next_prev_wrap() {
        let ranges = vec![2..4, 10..12, 20..22];
        assert_eq!(find_jump_target(&ranges, 0, 1), Some(0));
        assert_eq!(find_jump_target(&ranges, 3, 1), Some(1));
        assert_eq!(find_jump_target(&ranges, 12, 1), Some(2));
        assert_eq!(find_jump_target(&ranges, 99, 1), Some(0));
        assert_eq!(find_jump_target(&ranges, 99, -1), Some(2));
        assert_eq!(find_jump_target(&ranges, 11, -1), Some(0));
        assert_eq!(find_jump_target(&ranges, 10, -1), Some(0));
        assert_eq!(find_jump_target(&ranges, 0, -1), Some(2));
        assert!(find_jump_target(&[], 5, 1).is_none());
    }

    #[test]
    fn continuation_markers() {
        assert_eq!(continuation("- item"), Some("- ".into()));
        assert_eq!(continuation("* item"), Some("* ".into()));
        assert_eq!(continuation("  - item"), Some("- ".into()));
        assert_eq!(continuation("- [x] done"), Some("- [ ] ".into()));
        assert_eq!(continuation("3. third"), Some("4. ".into()));
        assert_eq!(continuation("3) third"), Some("4) ".into()));
        assert_eq!(continuation("> quote"), Some("> ".into()));
        assert_eq!(continuation("> > deep"), Some("> > ".into()));
        assert_eq!(continuation("plain"), None);
        assert_eq!(continuation("# head"), None);
    }

    #[test]
    fn prefix_end_finds_content() {
        assert_eq!(prefix_end("- item"), 2);
        assert_eq!(prefix_end("  - [ ] x"), 8);
        assert_eq!(prefix_end("> q"), 2);
        assert_eq!(prefix_end("plain"), 0);
    }

    #[test]
    fn enter_continues_and_exits_lists() {
        let mut m = EditorModel::new("- item");
        m.move_to(0, 6, false);
        on_enter(&mut m, "- item", false);
        assert_eq!(m.text(), "- item\n- ");
        // Empty item exits.
        let mut m = EditorModel::new("- ");
        m.move_to(0, 2, false);
        on_enter(&mut m, "- ", false);
        assert_eq!(m.text(), "");
        // Ordered increments, delimiter preserved.
        let mut m = EditorModel::new("3) x");
        m.move_to(0, 4, false);
        on_enter(&mut m, "3) x", false);
        assert_eq!(m.text(), "3) x\n4) ");
        // Code: plain newline.
        let mut m = EditorModel::new("let x = 1;");
        m.move_to(0, 10, false);
        on_enter(&mut m, "let x = 1;", true);
        assert_eq!(m.text(), "let x = 1;\n");
    }

    #[test]
    fn tab_indents_list_items() {
        let mut m = EditorModel::new("- item");
        m.move_to(0, 3, false);
        on_tab(&mut m, "- item", false, 4);
        assert_eq!(m.text(), "    - item");
        on_tab(&mut m, "    - item", true, 4);
        assert_eq!(m.text(), "- item");
    }

    #[test]
    fn table_cells_split_on_pipes() {
        assert_eq!(table_cell_bounds("| a | b |"), vec![(1, 4), (5, 8)]);
        assert_eq!(table_cell_bounds("a | b"), vec![(0, 2), (3, 5)]);
        // `\\|` is not a boundary; code spans shield pipes.
        assert_eq!(table_cell_bounds("| a\\|b | c |").len(), 2);
        assert_eq!(table_cell_bounds("| `a|b` | c |").len(), 2);
    }

    #[test]
    fn table_nav_moves_across_cells_and_rows() {
        let mut m = EditorModel::new("| a | b |\n| 1 | 2 |");
        let is_table = |l: usize| l < 2;
        m.move_to(0, 0, false);
        assert!(table_nav(&mut m, true, &is_table));
        assert_eq!(m.cursor(), Pos::new(0, 2));
        assert!(table_nav(&mut m, true, &is_table));
        assert_eq!(m.cursor(), Pos::new(0, 6));
        assert!(table_nav(&mut m, true, &is_table));
        assert_eq!(m.cursor(), Pos::new(1, 2));
        assert!(table_nav(&mut m, false, &is_table));
        assert_eq!(m.cursor(), Pos::new(0, 6));
    }

    #[test]
    fn table_nav_appends_row_past_the_end() {
        let mut m = EditorModel::new("| a |\n| 1 |");
        let is_table = |l: usize| l < 2;
        m.move_to(1, 2, false);
        assert!(table_nav(&mut m, true, &is_table));
        assert_eq!(m.text(), "| a |\n| 1 |\n|  |");
        assert_eq!(m.cursor(), Pos::new(2, 3));
    }

    #[test]
    fn table_insert_row_below_keeps_delimiter_glued() {
        let mut m = EditorModel::new("| a | b |\n|---|---|");
        m.move_to(0, 3, false);
        assert!(table_insert_row_below(&mut m));
        assert_eq!(m.text(), "| a | b |\n|---|---|\n|  |  |");
        assert_eq!(m.cursor(), Pos::new(2, 3));
    }

    #[test]
    fn word_stats_counts_chars_and_lines() {
        assert_eq!(word_stats(""), (0, 1));
        assert_eq!(word_stats("hello world"), (10, 1));
        assert_eq!(word_stats("你好世界"), (4, 1));
        assert_eq!(word_stats("hi 你好\n\nbye"), (7, 3));
    }

    #[test]
    fn backspace_strips_marker_at_content_start() {
        let mut m = EditorModel::new("- item");
        m.move_to(0, 2, false);
        assert!(backspace_marker(&mut m, "- item"));
        assert_eq!(m.text(), "item");
        let mut m = EditorModel::new("- item");
        m.move_to(0, 3, false);
        assert!(!backspace_marker(&mut m, "- item"));
    }

    #[test]
    fn task_toggle_rewrites_state_char() {
        let mut m = EditorModel::new("- [ ] todo");
        assert!(toggle_task(&mut m, "- [ ] todo", 0));
        assert_eq!(m.text(), "- [x] todo");
        assert!(toggle_task(&mut m, "- [x] todo", 0));
        assert_eq!(m.text(), "- [ ] todo");
        let mut m = EditorModel::new("- item");
        assert!(!toggle_task(&mut m, "- item", 0));
    }

    #[test]
    fn ordered_numbering_matches_preview() {
        use crate::editor::markdown_live::block::{classify, DocState};
        use crate::editor::markdown_live::layout::plan;

        // Plan each doc line (preview-mode) and collect the ordered display
        // numbers exactly like the render loop does.
        let numbers = |doc: &str| -> Vec<String> {
            let mut st = DocState::default();
            let mut counter = OrderedCounter::new();
            let mut out = Vec::new();
            for line in doc.split('\n') {
                let block = classify(line, &mut st);
                let p = plan(line, &block, None, false);
                if let Some(n) = counter.visit(&p.kind, list_cols(&p.kind)) {
                    out.push(format!("{n}."));
                }
            }
            out
        };
        // A blank line keeps a loose list going: sequential numbers.
        assert_eq!(numbers("1. a\n\n3. b"), vec!["1.", "2."]);
        // Nested lists restart at their own source number.
        assert_eq!(numbers("1. a\n   1. b\n   2. c\n2. d"), vec!["1.", "1.", "2.", "2."]);
        // A bullet item ends the ordered run: the next ordered list starts
        // from its own source number (CommonMark: a different marker type
        // begins a new list).
        assert_eq!(numbers("1. a\n- b\n1. c"), vec!["1.", "1."]);
        // Any non-list block row resets everything.
        assert_eq!(numbers("1. a\ntext\n5. b"), vec!["1.", "5."]);
        // Sibling ordered lists after a bullet child restart correctly.
        assert_eq!(numbers("1. a\n   - b\n   1. c"), vec!["1.", "1."]);
    }

    #[test]
    fn split_visual_rows() {
        assert_eq!(split_visual(&[], 0, 10), (0, 0));
        assert_eq!(split_visual(&[10], 2, 8), (0, 0));
        assert_eq!(split_visual(&[10], 2, 15), (0, 1));
        assert_eq!(split_visual(&[10], 12, 15), (1, 1));
        assert_eq!(split_visual(&[10], 10, 15), (1, 1));
        assert_eq!(split_visual(&[10], 2, 10), (0, 0));
        assert_eq!(split_visual(&[10], 10, 10), (1, 1));
    }

    #[test]
    fn line_selection_matches_editor_semantics() {
        let s = Pos::new(1, 2);
        let e = Pos::new(1, 5);
        assert_eq!(line_selection(s, e, 1, 9), Some((2, 5, false)));
        assert_eq!(line_selection(Pos::new(0, 3), Pos::new(2, 2), 1, 9), Some((0, 9, true)));
        assert_eq!(line_selection(s, e, 0, 9), None);
        assert_eq!(line_selection(Pos::new(0, 0), Pos::new(2, 2), 1, 0), Some((0, 0, true)));
    }

    #[test]
    fn scroll_adjust_reveals_target() {
        assert_eq!(scroll_adjust(-10.0, 100.0, 20.0, 40.0), -10.0);
        assert_eq!(scroll_adjust(-50.0, 100.0, 20.0, 40.0), -20.0);
        assert_eq!(scroll_adjust(0.0, 100.0, 150.0, 170.0), -70.0);
    }
}
