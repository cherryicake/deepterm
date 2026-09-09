//! Pure editing interactions for live markdown (zero GPUI).
//!
//! Ported from guise `markdown/editor.rs:1406-1537` pure helpers plus the
//! `toggle_task`/`on_enter`/`on_tab`/`backspace_marker` logic expressed
//! against [`crate::editor::markdown_live::model::EditorModel`]. The view
//! layer (`view.rs`) wraps these in `buffer.edit(...)` transactions.

use crate::editor::markdown_live::block::{classify, Block, DocState};
use crate::editor::markdown_live::layout::RowKind;
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
