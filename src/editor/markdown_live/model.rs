//! Pure multiline text-editing model: the document as lines, a char-index
//! cursor, an anchor-based selection, and snapshot undo/redo with coalesced
//! typing. No UI and no gpui — fully unit-testable; the `Editor` entity
//! drives it from key/mouse events and renders from `lines()`/`selection()`.
//!
//! The multiline successor to [`TextEdit`](crate::input::TextEdit): same
//! char-index cursor and word-boundary semantics, plus selections and history.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;

/// A document line. `Arc<String>` (not `String`) is what keeps undo history
/// affordable: a snapshot clones the *line table* (one refcount bump per
/// line) instead of the text, so 200 retained steps cost ~`lines × 16` bytes
/// rather than `200 × document`. Editing a line that a snapshot still
/// references copies just that one line (copy-on-write via `Arc::make_mut`).
pub type Line = Arc<String>;

/// A position in the document: `line` index plus `col` as a **char** index
/// (not bytes) in `0..=line_len`. Ordering is document order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
  pub line: usize,
  pub col: usize,
}

impl Pos {
  pub fn new(line: usize, col: usize) -> Self {
    Self { line, col }
  }
}

/// One undo/redo step: the document and cursor as they were before an edit.
///
/// `lines` holds shared line text, so cloning this is O(lines) pointer copies
/// (no text bytes). See [`Line`].
#[derive(Debug, Clone)]
struct Snapshot {
  lines: Vec<Line>,
  cursor: Pos,
}

/// Bounds `undo`/`redo` retention. Each snapshot shares line text with the
/// live document and with every other snapshot, so retained memory is bounded
/// by `steps × lines` pointer slots plus the text of the lines that were
/// actually edited. Dropping the oldest step is O(1) on a `VecDeque`.
const MAX_UNDO_STEPS: usize = 200;

/// A multiline editing model with cursor, selection, and undo history.
///
/// The document is a `Vec<Line>` (no trailing `\n` stored); an empty document
/// is one empty line. All columns are char indices, so multibyte text (é,
/// 日本語) edits correctly. Runs of single-char typing coalesce into one undo
/// step; any other edit or movement breaks the run.
#[derive(Debug, Clone)]
pub struct EditorModel {
  lines: Vec<Line>,
  cursor: Pos,
  /// Selection anchor; the selection spans anchor..cursor in either order.
  anchor: Option<Pos>,
  /// Sticky column for vertical movement through short lines.
  goal_col: Option<usize>,
  undo: VecDeque<Snapshot>,
  redo: VecDeque<Snapshot>,
  /// Whether the last edit was a coalescable single-char insert.
  coalescing: bool,
  /// Spaces per tab stop for [`tab`](Self::tab).
  tab_size: usize,
  /// Monotonic text revision, bumped only by mutations of `lines`.
  /// Lets `LiveBuffer::edit` detect text changes in O(1) without joining
  /// the full document on every keystroke / cursor move.
  text_version: u64,
}

impl Default for EditorModel {
  fn default() -> Self {
    Self::new("")
  }
}

/// Empty-pair table shared by `insert_pair`/`delete_pair`.
const PAIRS: [(char, char); 6] = [
  ('(', ')'),
  ('[', ']'),
  ('{', '}'),
  ('"', '"'),
  ('\'', '\''),
  ('`', '`'),
];

impl EditorModel {
  /// Start editing `text` with the cursor at the document start.
  pub fn new(text: &str) -> Self {
    Self {
      lines: split_lines(text),
      cursor: Pos::default(),
      anchor: None,
      goal_col: None,
      undo: VecDeque::new(),
      redo: VecDeque::new(),
      coalescing: false,
      tab_size: 4,
      text_version: 0,
    }
  }

  // ---- document access ----

  pub fn text(&self) -> String {
    // `Vec<Arc<String>>` has no `join`, and building the separator-joined
    // string directly avoids an intermediate `Vec<&str>` per call.
    let mut out = String::with_capacity(
      self.lines.iter().map(|l| l.len() + 1).sum::<usize>().saturating_sub(1),
    );
    for (i, line) in self.lines.iter().enumerate() {
      if i > 0 {
        out.push('\n');
      }
      out.push_str(line);
    }
    out
  }

  /// O(1) text revision for cheap edit detection (`LiveBuffer::edit`).
  pub fn text_version(&self) -> u64 {
    self.text_version
  }

  fn bump_text(&mut self) {
    self.text_version = self.text_version.wrapping_add(1);
  }

  /// Replace the whole document, resetting cursor, selection, and history.
  pub fn set_text(&mut self, text: &str) {
    self.lines = split_lines(text);
    self.cursor = Pos::default();
    self.anchor = None;
    self.goal_col = None;
    self.undo.clear();
    self.redo.clear();
    self.coalescing = false;
    self.bump_text();
  }

  pub fn is_empty(&self) -> bool {
    self.lines.len() == 1 && self.lines[0].is_empty()
  }

  pub fn line_count(&self) -> usize {
    self.lines.len()
  }

  /// The text of line `i`, if it exists.
  pub fn line(&self, i: usize) -> Option<&str> {
    self.lines.get(i).map(|l| l.as_str())
  }

  /// All lines, for rendering.
  pub fn lines(&self) -> &[Line] {
    &self.lines
  }

  pub fn cursor(&self) -> Pos {
    self.cursor
  }

  pub fn tab_size(&self) -> usize {
    self.tab_size
  }

  /// Spaces per tab stop (min 1, default 4).
  pub fn set_tab_size(&mut self, n: usize) {
    self.tab_size = n.max(1);
  }

  // ---- editing ----

  /// Insert `s` at the cursor, replacing the selection if there is one.
  /// Embedded newlines split lines; CRLF is normalized to `\n`.
  pub fn insert(&mut self, s: &str) {
    let s: Cow<str> = if s.contains('\r') {
      Cow::Owned(s.replace('\r', ""))
    } else {
      Cow::Borrowed(s)
    };
    if s.is_empty() {
      return;
    }
    let coalesce = self.selection().is_none() && s.chars().count() == 1 && !s.contains('\n');
    self.push_undo(coalesce);
    self.remove_selection();
    self.insert_at_cursor(&s);
    self.goal_col = None;
  }

  /// Delete the selection, or the char before the cursor (joining lines at
  /// a line start). Returns whether anything changed.
  pub fn backspace(&mut self) -> bool {
    if self.selection().is_some() {
      return self.delete_selection();
    }
    if self.cursor == Pos::default() {
      return false;
    }
    self.push_undo(false);
    let start = self.prev_pos(self.cursor);
    self.remove_range(start, self.cursor);
    self.goal_col = None;
    true
  }

  /// Delete the selection, or the char at the cursor (joining lines at a
  /// line end). Returns whether anything changed.
  pub fn delete(&mut self) -> bool {
    if self.selection().is_some() {
      return self.delete_selection();
    }
    let end = self.next_pos(self.cursor);
    if end == self.cursor {
      return false;
    }
    self.push_undo(false);
    self.remove_range(self.cursor, end);
    self.goal_col = None;
    true
  }

  /// Split the line at the cursor, auto-indenting the new line with the
  /// current line's leading whitespace (capped at the cursor column, so
  /// splitting inside the indent doesn't over-indent).
  pub fn newline(&mut self) {
    self.push_undo(false);
    self.remove_selection();
    let indent: String = self.lines[self.cursor.line]
      .chars()
      .take(self.cursor.col)
      .take_while(|c| c.is_whitespace())
      .collect();
    self.insert_at_cursor("\n");
    self.insert_at_cursor(&indent);
    self.goal_col = None;
  }

  /// Insert spaces up to the next tab stop (see [`set_tab_size`](Self::set_tab_size)).
  pub fn tab(&mut self) {
    self.push_undo(false);
    self.remove_selection();
    let n = self.tab_size - (self.cursor.col % self.tab_size);
    self.insert_at_cursor(&" ".repeat(n));
    self.goal_col = None;
  }

  // ---- movement (extend = shift held: grow the selection) ----

  pub fn move_left(&mut self, extend: bool) {
    self.goal_col = None;
    if !extend {
      if let Some((start, _)) = self.selection() {
        self.coalescing = false;
        self.anchor = None;
        self.cursor = start;
        return;
      }
    }
    self.start_move(extend);
    self.cursor = self.prev_pos(self.cursor);
  }

  pub fn move_right(&mut self, extend: bool) {
    self.goal_col = None;
    if !extend {
      if let Some((_, end)) = self.selection() {
        self.coalescing = false;
        self.anchor = None;
        self.cursor = end;
        return;
      }
    }
    self.start_move(extend);
    self.cursor = self.next_pos(self.cursor);
  }

  /// Move up one line, keeping the goal column through shorter lines.
  pub fn move_up(&mut self, extend: bool) {
    self.start_move(extend);
    let goal = self.goal_col.unwrap_or(self.cursor.col);
    if self.cursor.line > 0 {
      self.cursor.line -= 1;
      self.cursor.col = goal.min(self.line_len(self.cursor.line));
    }
    self.goal_col = Some(goal);
  }

  /// Move down one line, keeping the goal column through shorter lines.
  pub fn move_down(&mut self, extend: bool) {
    self.start_move(extend);
    let goal = self.goal_col.unwrap_or(self.cursor.col);
    if self.cursor.line + 1 < self.lines.len() {
      self.cursor.line += 1;
      self.cursor.col = goal.min(self.line_len(self.cursor.line));
    }
    self.goal_col = Some(goal);
  }

  pub fn home(&mut self, extend: bool) {
    self.start_move(extend);
    self.cursor.col = 0;
    self.goal_col = None;
  }

  pub fn end(&mut self, extend: bool) {
    self.start_move(extend);
    self.cursor.col = self.line_len(self.cursor.line);
    self.goal_col = None;
  }

  pub fn doc_start(&mut self, extend: bool) {
    self.start_move(extend);
    self.cursor = Pos::default();
    self.goal_col = None;
  }

  pub fn doc_end(&mut self, extend: bool) {
    self.start_move(extend);
    let line = self.lines.len() - 1;
    self.cursor = Pos::new(line, self.line_len(line));
    self.goal_col = None;
  }

  /// Move left to the start of the previous word (Option+Left), crossing
  /// line boundaries.
  pub fn word_left(&mut self, extend: bool) {
    self.start_move(extend);
    let mut p = self.cursor;
    while let Some(c) = self.char_before(p) {
      if is_word(c) {
        break;
      }
      p = self.prev_pos(p);
    }
    while let Some(c) = self.char_before(p) {
      if !is_word(c) {
        break;
      }
      p = self.prev_pos(p);
    }
    self.cursor = p;
    self.goal_col = None;
  }

  /// Move right past the end of the next word (Option+Right), crossing
  /// line boundaries.
  pub fn word_right(&mut self, extend: bool) {
    self.start_move(extend);
    let mut p = self.cursor;
    while let Some(c) = self.char_at(p) {
      if is_word(c) {
        break;
      }
      p = self.next_pos(p);
    }
    while let Some(c) = self.char_at(p) {
      if !is_word(c) {
        break;
      }
      p = self.next_pos(p);
    }
    self.cursor = p;
    self.goal_col = None;
  }

  // ---- mouse ----

  /// Clamp a raw (line, col) from mouse hit-testing to a valid position:
  /// line into the document, col to that line's char length.
  pub fn pos_for_click(&self, line: usize, col: usize) -> Pos {
    let line = line.min(self.lines.len() - 1);
    Pos::new(line, col.min(self.line_len(line)))
  }

  /// Move the cursor to a clicked position (clamped), extending the
  /// selection when `extend` (shift-click or drag).
  pub fn move_to(&mut self, line: usize, col: usize, extend: bool) {
    let pos = self.pos_for_click(line, col);
    self.start_move(extend);
    self.cursor = pos;
    self.goal_col = None;
  }

  // ---- selection ----

  /// The selection as a normalized (start, end) pair in document order, or
  /// `None` when there is no selection (or it is empty).
  pub fn selection(&self) -> Option<(Pos, Pos)> {
    let anchor = self.anchor?;
    if anchor == self.cursor {
      return None;
    }
    Some(if anchor < self.cursor {
      (anchor, self.cursor)
    } else {
      (self.cursor, anchor)
    })
  }

  /// 选区锚点（拖拽/Shift 扩展的起点），`None` = 无选区意图。
  /// 与 [`Self::selection`]（按文档序归一化）不同，这里保留原始方向，
  /// 跨格拖拽的「起点是否在表内」判断需要它。
  pub fn anchor(&self) -> Option<Pos> {
    self.anchor
  }

  pub fn clear_selection(&mut self) {
    self.anchor = None;
  }

  pub fn select_all(&mut self) {
    self.coalescing = false;
    self.goal_col = None;
    self.anchor = Some(Pos::default());
    let line = self.lines.len() - 1;
    self.cursor = Pos::new(line, self.line_len(line));
  }

  /// Select the word around the cursor (double-click). On a non-word char,
  /// selects just that char.
  pub fn select_word(&mut self) {
    self.coalescing = false;
    self.goal_col = None;
    let line = self.cursor.line;
    let chars: Vec<char> = self.lines[line].chars().collect();
    if chars.is_empty() {
      return;
    }
    let col = self.cursor.col.min(chars.len());
    // The word char under the cursor, or the one before at a word end.
    let seed = if col < chars.len() && is_word(chars[col]) {
      col
    } else if col > 0 && is_word(chars[col - 1]) {
      col - 1
    } else {
      let start = if col < chars.len() { col } else { col - 1 };
      self.anchor = Some(Pos::new(line, start));
      self.cursor = Pos::new(line, start + 1);
      return;
    };
    let mut start = seed;
    while start > 0 && is_word(chars[start - 1]) {
      start -= 1;
    }
    let mut end = seed + 1;
    while end < chars.len() && is_word(chars[end]) {
      end += 1;
    }
    self.anchor = Some(Pos::new(line, start));
    self.cursor = Pos::new(line, end);
  }

  /// Select the cursor's whole line (triple-click).
  pub fn select_line(&mut self) {
    self.coalescing = false;
    self.goal_col = None;
    let line = self.cursor.line;
    self.anchor = Some(Pos::new(line, 0));
    self.cursor = Pos::new(line, self.line_len(line));
  }

  /// Select the word under the cursor, **clamped to `[start, end)` char
  /// columns** (table cells: a double-click must not spill into the
  /// neighbouring cells or the pipes).
  pub fn select_word_within(&mut self, start: usize, end: usize) {
    self.coalescing = false;
    self.goal_col = None;
    let line = self.cursor.line;
    let chars: Vec<char> = self.lines[line].chars().collect();
    let start = start.min(chars.len());
    let end = end.min(chars.len()).max(start);
    let col = self.cursor.col.clamp(start, end);
    if start >= end {
      return;
    }
    // The word char under the cursor, or the one before at a word end.
    let seed = if col < end && is_word(chars[col]) {
      col
    } else if col > start && is_word(chars[col - 1]) {
      col - 1
    } else {
      // No word under the cursor: select the whole (trimmed) cell content,
      // which is what a double-click on punctuation does in Typora.
      self.anchor = Some(Pos::new(line, start));
      self.cursor = Pos::new(line, end);
      return;
    };
    let mut ws = seed;
    while ws > start && is_word(chars[ws - 1]) {
      ws -= 1;
    }
    let mut we = seed + 1;
    while we < end && is_word(chars[we]) {
      we += 1;
    }
    self.anchor = Some(Pos::new(line, ws));
    self.cursor = Pos::new(line, we);
  }

  /// Select an explicit char-column range on the cursor's line.
  pub fn select_range(&mut self, start: usize, end: usize) {
    self.coalescing = false;
    self.goal_col = None;
    let line = self.cursor.line;
    let len = self.line_len(line);
    let start = start.min(len);
    let end = end.min(len).max(start);
    self.anchor = Some(Pos::new(line, start));
    self.cursor = Pos::new(line, end);
  }

  /// The selected text, `None` when nothing is selected.
  pub fn selected_text(&self) -> Option<String> {
    let (start, end) = self.selection()?;
    if start.line == end.line {
      let line = &self.lines[start.line];
      return Some(line[byte_idx(line, start.col)..byte_idx(line, end.col)].to_string());
    }
    let first = &self.lines[start.line];
    let mut out = first[byte_idx(first, start.col)..].to_string();
    for line in &self.lines[start.line + 1..end.line] {
      out.push('\n');
      out.push_str(line);
    }
    let last = &self.lines[end.line];
    out.push('\n');
    out.push_str(&last[..byte_idx(last, end.col)]);
    Some(out)
  }

  /// Delete the selected text. Returns whether anything changed.
  pub fn delete_selection(&mut self) -> bool {
    if self.selection().is_none() {
      return false;
    }
    self.push_undo(false);
    self.remove_selection();
    self.goal_col = None;
    true
  }

  // ---- clipboard (the entity layer talks to the OS) ----

  /// Remove and return the selected text; `None` when nothing is selected.
  pub fn cut(&mut self) -> Option<String> {
    let text = self.selected_text()?;
    self.push_undo(false);
    self.remove_selection();
    self.goal_col = None;
    Some(text)
  }

  /// The selected text without modifying the document.
  pub fn copy(&self) -> Option<String> {
    self.selected_text()
  }

  // ---- history ----

  /// Replace `start..end` with `text` as ONE undo step (find/replace
  /// parity with the code editor's silent replace). Cursor lands after
  /// the inserted text.
  /// Auto-pair: insert `open` + `close` with the cursor between them
  /// (single undo step; Typora parity for `([{` quotes backticks).
  pub fn insert_pair(&mut self, open: char, close: char) {
    let mut s = String::new();
    s.push(open);
    s.push(close);
    self.push_undo(false);
    self.anchor = None;
    self.insert_at_cursor(&s);
    self.move_left(false);
    self.goal_col = None;
  }

  /// Closer key: skip over an identical char ahead, else insert it.
  /// Returns true when it skipped (nothing inserted).
  pub fn insert_closer(&mut self, close: char) -> bool {
    let ahead = self
      .line(self.cursor.line)
      .and_then(|l| l.chars().nth(self.cursor.col))
      == Some(close);
    if ahead {
      self.move_right(false);
      true
    } else {
      self.insert(&close.to_string());
      false
    }
  }

  /// Wrap the selection with `open`/`close` (single undo step).
  /// The cursor lands after the pair; returns false without a selection.
  pub fn wrap_selection(&mut self, open: char, close: char) -> bool {
    let Some(text) = self.selected_text() else {
      return false;
    };
    self.push_undo(false);
    self.remove_selection();
    let mut s = String::new();
    s.push(open);
    s.push_str(&text);
    s.push(close);
    self.insert_at_cursor(&s);
    self.goal_col = None;
    true
  }

  /// Backspace inside an empty pair (`()`, `[]`, `{}`, `""`, `''`,
  /// ` `` `): delete both chars as ONE undo step (Typora parity).
  /// Returns false when the cursor doesn't sit inside an empty pair.
  pub fn delete_pair(&mut self) -> bool {
    if self.selection().is_some() || self.cursor.col == 0 {
      return false;
    }
    let Some(line) = self.line(self.cursor.line) else {
      return false;
    };
    let chars: Vec<char> = line.chars().collect();
    let pair = match (chars.get(self.cursor.col - 1), chars.get(self.cursor.col)) {
      (Some(o), Some(c)) => PAIRS.contains(&(*o, *c)),
      _ => false,
    };
    if !pair {
      return false;
    }
    self.push_undo(false);
    let start = self.prev_pos(self.cursor);
    let end = self.next_pos(self.cursor);
    self.remove_range(start, end);
    self.goal_col = None;
    true
  }

  pub fn replace_range(&mut self, start: Pos, end: Pos, text: &str) {
    self.replace_ranges(&[(start, end)], text);
  }

  /// Replace every range with `text` as ONE undo step. Ranges must be
  /// non-overlapping; applied back-to-front so earlier offsets stay valid.
  /// The cursor lands after the first replacement.
  pub fn replace_ranges(&mut self, ranges: &[(Pos, Pos)], text: &str) {
    if ranges.is_empty() {
      return;
    }
    let text = text.replace('\r', "");
    let mut order: Vec<(Pos, Pos)> = ranges
      .iter()
      .map(|(s, e)| if e < s { (*e, *s) } else { (*s, *e) })
      .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0));
    self.push_undo(false);
    self.anchor = None;
    for (s, e) in order {
      self.remove_range(s, e);
      self.cursor = s;
      self.insert_at_cursor(&text);
    }
    self.goal_col = None;
  }

  /// Replace every range with its own text as ONE undo step (bulk
  /// replace with per-match case preservation). Ranges must be
  /// non-overlapping; applied back-to-front so earlier offsets stay valid.
  pub fn replace_ranges_each(&mut self, ranges: &[(Pos, Pos, String)]) {
    if ranges.is_empty() {
      return;
    }
    let mut order: Vec<(Pos, Pos, String)> = ranges
      .iter()
      .map(|(s, e, t)| {
        let (s, e) = if e < s { (*e, *s) } else { (*s, *e) };
        (s, e, t.replace('\r', ""))
      })
      .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0));
    self.push_undo(false);
    self.anchor = None;
    for (s, e, text) in order {
      self.remove_range(s, e);
      self.cursor = s;
      self.insert_at_cursor(&text);
    }
    self.goal_col = None;
  }

  /// 整行批量改写 + 光标重映射，撤销**并入当前步**（`push_undo(true)` 语义：
  /// 连续打字 run 中不插入额外撤销步，结尾的 `move_to` 也不打断 run）——
  /// 表格自动重排（Typora spec #13）用它把「打字 + 重排」合成一步撤销；
  /// 不在打字 run 中时则自成一步。
  pub fn replace_rows_and_move(&mut self, edits: &[(usize, String)], to: Option<Pos>) {
    if edits.is_empty() {
      return;
    }
    let was_coalescing = self.coalescing;
    self.push_undo(true);
    self.anchor = None;
    for (i, text) in edits {
      let Some(len) = self.lines.get(*i).map(|l| l.chars().count()) else {
        continue;
      };
      self.remove_range(Pos::new(*i, 0), Pos::new(*i, len));
      self.insert_at_cursor(text);
    }
    if let Some(p) = to {
      self.move_to(p.line, p.col, false);
    }
    self.coalescing = was_coalescing;
    self.goal_col = None;
  }

  pub fn can_undo(&self) -> bool {
    !self.undo.is_empty()
  }

  pub fn can_redo(&self) -> bool {
    !self.redo.is_empty()
  }

  /// Revert the last edit (a coalesced typing run counts as one). Returns
  /// whether anything changed.
  pub fn undo(&mut self) -> bool {
    let Some(snap) = self.undo.pop_back() else {
      return false;
    };
    self.bump_text();
    let lines = std::mem::replace(&mut self.lines, snap.lines);
    let cursor = self.cursor;
    Self::push_step_bounded(&mut self.redo, Snapshot { lines, cursor });
    self.cursor = snap.cursor;
    self.anchor = None;
    self.goal_col = None;
    self.coalescing = false;
    true
  }

  /// Re-apply the last undone edit. Returns whether anything changed.
  pub fn redo(&mut self) -> bool {
    let Some(snap) = self.redo.pop_back() else {
      return false;
    };
    self.bump_text();
    let lines = std::mem::replace(&mut self.lines, snap.lines);
    let cursor = self.cursor;
    Self::push_step_bounded(&mut self.undo, Snapshot { lines, cursor });
    self.cursor = snap.cursor;
    self.anchor = None;
    self.goal_col = None;
    self.coalescing = false;
    true
  }

  // ---- internals ----

  fn line_len(&self, i: usize) -> usize {
    self.lines[i].chars().count()
  }

  /// The position one char before `p` (line boundaries count as one char).
  fn prev_pos(&self, p: Pos) -> Pos {
    if p.col > 0 {
      Pos::new(p.line, p.col - 1)
    } else if p.line > 0 {
      Pos::new(p.line - 1, self.line_len(p.line - 1))
    } else {
      p
    }
  }

  /// The position one char after `p` (line boundaries count as one char).
  fn next_pos(&self, p: Pos) -> Pos {
    if p.col < self.line_len(p.line) {
      Pos::new(p.line, p.col + 1)
    } else if p.line + 1 < self.lines.len() {
      Pos::new(p.line + 1, 0)
    } else {
      p
    }
  }

  /// The char before `p`, with `\n` at line starts; `None` at doc start.
  fn char_before(&self, p: Pos) -> Option<char> {
    if p.col > 0 {
      self.lines[p.line].chars().nth(p.col - 1)
    } else if p.line > 0 {
      Some('\n')
    } else {
      None
    }
  }

  /// The char at `p`, with `\n` at line ends; `None` at doc end.
  fn char_at(&self, p: Pos) -> Option<char> {
    if p.col < self.line_len(p.line) {
      self.lines[p.line].chars().nth(p.col)
    } else if p.line + 1 < self.lines.len() {
      Some('\n')
    } else {
      None
    }
  }

  /// Movement prologue: break undo coalescing and set/clear the anchor.
  fn start_move(&mut self, extend: bool) {
    self.coalescing = false;
    if extend {
      if self.anchor.is_none() {
        self.anchor = Some(self.cursor);
      }
    } else {
      self.anchor = None;
    }
  }

  /// Record the current state for undo unless coalescing with the previous
  /// single-char insert. Any edit invalidates the redo stack.
  fn push_undo(&mut self, coalesce: bool) {
    if !(coalesce && self.coalescing) {
      let lines = self.lines.clone();
      let cursor = self.cursor;
      Self::push_step_bounded(&mut self.undo, Snapshot { lines, cursor });
    }
    self.coalescing = coalesce;
    self.redo.clear();
  }

  /// Push a history step onto `stack` (the undo or redo deque), dropping the
  /// oldest step first once the capacity is reached. Bounding is what keeps
  /// `MAX_UNDO_STEPS × document` as a hard ceiling on history memory.
  fn push_step_bounded(stack: &mut VecDeque<Snapshot>, snap: Snapshot) {
    if stack.len() >= MAX_UNDO_STEPS {
      stack.pop_front();
    }
    stack.push_back(snap);
  }

  /// Remove the selected range if any, leaving the cursor at its start.
  fn remove_selection(&mut self) {
    if let Some((start, end)) = self.selection() {
      self.remove_range(start, end);
    }
    self.anchor = None;
  }

  /// Remove `start..end` (document order), leaving the cursor at `start`.
  fn remove_range(&mut self, start: Pos, end: Pos) {
    if start == end {
      self.cursor = start;
      self.anchor = None;
      return;
    }
    self.bump_text();
    if start.line == end.line {
      let line = Arc::make_mut(&mut self.lines[start.line]);
      let a = byte_idx(line, start.col);
      let b = byte_idx(line, end.col);
      line.replace_range(a..b, "");
    } else {
      let last = &self.lines[end.line];
      let tail = last[byte_idx(last, end.col)..].to_string();
      let first = Arc::make_mut(&mut self.lines[start.line]);
      first.truncate(byte_idx(first, start.col));
      first.push_str(&tail);
      self.lines.drain(start.line + 1..=end.line);
    }
    self.cursor = start;
    self.anchor = None;
  }

  /// Insert already-normalized text at the cursor, advancing past it.
  fn insert_at_cursor(&mut self, s: &str) {
    if s.is_empty() {
      return;
    }
    self.bump_text();
    let Pos { line, col } = self.cursor;
    let at = byte_idx(&self.lines[line], col);
    if !s.contains('\n') {
      Arc::make_mut(&mut self.lines[line]).insert_str(at, s);
      self.cursor.col += s.chars().count();
      return;
    }
    let tail = Arc::make_mut(&mut self.lines[line]).split_off(at);
    let mut parts = s.split('\n');
    if let Some(first) = parts.next() {
      Arc::make_mut(&mut self.lines[line]).push_str(first);
    }
    let mut last = line;
    for part in parts {
      last += 1;
      self.lines.insert(last, Arc::new(part.to_string()));
    }
    self.cursor = Pos::new(last, self.line_len(last));
    Arc::make_mut(&mut self.lines[last]).push_str(&tail);
  }
}

/// Byte offset of char index `col` in `line` (`line.len()` past the end).
fn byte_idx(line: &str, col: usize) -> usize {
  line
    .char_indices()
    .nth(col)
    .map(|(i, _)| i)
    .unwrap_or(line.len())
}

/// Document text as lines, normalizing CRLF.
fn split_lines(text: &str) -> Vec<Line> {
  let text = text.replace('\r', "");
  text.split('\n').map(|l| Arc::new(l.to_string())).collect()
}

/// Test/bench helper: build a `Line` table from a document string, mirroring
/// [`split_lines`]. Kept public so the other `markdown_live` modules and the
/// integration tests can construct the same shape without duplicating the
/// normalization rules.
#[cfg(any(test, feature = "alloc-probe"))]
pub fn lines_from_text(text: &str) -> Vec<Line> {
  split_lines(text)
}

/// Join a line table back into document text (the inverse of
/// [`lines_from_text`]). Test/bench helper.
#[cfg(any(test, feature = "alloc-probe"))]
pub fn join_lines(lines: &[Line]) -> String {
  lines
    .iter()
    .map(|l| l.as_str())
    .collect::<Vec<_>>()
    .join("\n")
}

/// Word characters for word-wise navigation — mirrors `input::edit`.
fn is_word(c: char) -> bool {
  c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
  use super::*;

  fn at(line: usize, col: usize) -> Pos {
    Pos::new(line, col)
  }

  #[test]
  fn empty_doc_is_one_empty_line() {
    let m = EditorModel::new("");
    assert_eq!(m.line_count(), 1);
    assert!(m.is_empty());
    assert_eq!(m.text(), "");
    assert_eq!(m.cursor(), at(0, 0));
  }

  #[test]
  fn text_roundtrip() {
    let m = EditorModel::new("a\nb\n\nc");
    assert_eq!(m.line_count(), 4);
    assert_eq!(m.line(2), Some(""));
    assert_eq!(m.line(4), None);
    assert_eq!(m.text(), "a\nb\n\nc");
  }

  #[test]
  fn new_normalizes_crlf() {
    let m = EditorModel::new("a\r\nb");
    assert_eq!(m.text(), "a\nb");
    assert_eq!(m.line_count(), 2);
  }

  #[test]
  fn set_text_resets_cursor_selection_and_history() {
    let mut m = EditorModel::new("abc");
    m.doc_end(false);
    m.insert("d");
    m.select_all();
    m.set_text("xyz");
    assert_eq!(m.text(), "xyz");
    assert_eq!(m.cursor(), at(0, 0));
    assert_eq!(m.selection(), None);
    assert!(!m.can_undo());
    assert!(!m.can_redo());
  }

  #[test]
  fn insert_advances_cursor() {
    let mut m = EditorModel::new("ac");
    m.move_right(false);
    m.insert("b");
    assert_eq!(m.text(), "abc");
    assert_eq!(m.cursor(), at(0, 2));
  }

  #[test]
  fn insert_multiline_splits_lines() {
    let mut m = EditorModel::new("hello world");
    m.move_to(0, 5, false);
    m.insert("\nmid\n");
    assert_eq!(m.text(), "hello\nmid\n world");
    assert_eq!(m.cursor(), at(2, 0));
  }

  #[test]
  fn insert_replaces_selection() {
    let mut m = EditorModel::new("one two three");
    m.move_to(0, 4, false);
    m.move_to(0, 7, true);
    m.insert("2");
    assert_eq!(m.text(), "one 2 three");
    assert_eq!(m.cursor(), at(0, 5));
    assert_eq!(m.selection(), None);
  }

  #[test]
  fn insert_replaces_multiline_selection() {
    let mut m = EditorModel::new("aaa\nbbb\nccc");
    m.move_to(0, 1, false);
    m.move_to(2, 2, true);
    m.insert("X");
    assert_eq!(m.text(), "aXc");
    assert_eq!(m.cursor(), at(0, 2));
  }

  #[test]
  fn backspace_within_line_and_join() {
    let mut m = EditorModel::new("ab\ncd");
    m.move_to(1, 1, false);
    assert!(m.backspace());
    assert_eq!(m.text(), "ab\nd");
    // At the line start: joins with the previous line.
    assert!(m.backspace());
    assert_eq!(m.text(), "abd");
    assert_eq!(m.cursor(), at(0, 2));
  }

  #[test]
  fn backspace_at_doc_start_is_noop() {
    let mut m = EditorModel::new("ab");
    assert!(!m.backspace());
    assert_eq!(m.text(), "ab");
    assert!(!m.can_undo());
  }

  #[test]
  fn delete_within_line_and_join() {
    let mut m = EditorModel::new("ab\ncd");
    m.move_to(0, 1, false);
    assert!(m.delete());
    assert_eq!(m.text(), "a\ncd");
    // At the line end: joins with the next line.
    assert!(m.delete());
    assert_eq!(m.text(), "acd");
    assert_eq!(m.cursor(), at(0, 1));
  }

  #[test]
  fn delete_at_doc_end_is_noop() {
    let mut m = EditorModel::new("ab");
    m.doc_end(false);
    assert!(!m.delete());
    assert!(!m.can_undo());
  }

  #[test]
  fn newline_copies_leading_whitespace() {
    let mut m = EditorModel::new("    foo");
    m.end(false);
    m.newline();
    assert_eq!(m.text(), "    foo\n    ");
    assert_eq!(m.cursor(), at(1, 4));
  }

  #[test]
  fn newline_inside_indent_caps_the_copy() {
    let mut m = EditorModel::new("    foo");
    m.move_to(0, 2, false);
    m.newline();
    assert_eq!(m.text(), "  \n    foo");
    assert_eq!(m.cursor(), at(1, 2));
  }

  #[test]
  fn newline_replaces_selection() {
    let mut m = EditorModel::new("  ab cd");
    m.move_to(0, 4, false);
    m.move_to(0, 5, true);
    m.newline();
    assert_eq!(m.text(), "  ab\n  cd");
    assert_eq!(m.cursor(), at(1, 2));
  }

  #[test]
  fn tab_advances_to_next_stop() {
    let mut m = EditorModel::new("");
    m.tab();
    assert_eq!(m.text(), "    ");
    let mut m = EditorModel::new("ab");
    m.end(false);
    m.tab();
    assert_eq!(m.text(), "ab  ");
    assert_eq!(m.cursor(), at(0, 4));
    let mut m = EditorModel::new("x");
    m.set_tab_size(2);
    m.end(false);
    m.tab();
    assert_eq!(m.text(), "x ");
  }

  #[test]
  fn horizontal_movement_crosses_lines() {
    let mut m = EditorModel::new("ab\ncd");
    m.move_to(0, 2, false);
    m.move_right(false);
    assert_eq!(m.cursor(), at(1, 0));
    m.move_left(false);
    assert_eq!(m.cursor(), at(0, 2));
    // Doc edges are no-ops.
    m.doc_start(false);
    m.move_left(false);
    assert_eq!(m.cursor(), at(0, 0));
    m.doc_end(false);
    m.move_right(false);
    assert_eq!(m.cursor(), at(1, 2));
  }

  #[test]
  fn vertical_movement_keeps_goal_column() {
    let mut m = EditorModel::new("hello\nhi\nworld!");
    m.move_to(0, 4, false);
    m.move_down(false);
    assert_eq!(m.cursor(), at(1, 2)); // clamped by the short line
    m.move_down(false);
    assert_eq!(m.cursor(), at(2, 4)); // goal column restored
    m.move_up(false);
    m.move_up(false);
    assert_eq!(m.cursor(), at(0, 4));
  }

  #[test]
  fn horizontal_movement_resets_goal_column() {
    let mut m = EditorModel::new("hello\nhi\nworld!");
    m.move_to(0, 4, false);
    m.move_down(false); // (1, 2), goal 4
    m.move_left(false); // (1, 1), goal cleared
    m.move_down(false);
    assert_eq!(m.cursor(), at(2, 1));
  }

  #[test]
  fn vertical_movement_stops_at_edges() {
    let mut m = EditorModel::new("a\nb");
    m.move_up(false);
    assert_eq!(m.cursor(), at(0, 0));
    m.doc_end(false);
    m.move_down(false);
    assert_eq!(m.cursor(), at(1, 1));
  }

  #[test]
  fn home_end_and_doc_edges() {
    let mut m = EditorModel::new("abc\ndef");
    m.move_to(1, 1, false);
    m.home(false);
    assert_eq!(m.cursor(), at(1, 0));
    m.end(false);
    assert_eq!(m.cursor(), at(1, 3));
    m.doc_start(false);
    assert_eq!(m.cursor(), at(0, 0));
    m.doc_end(false);
    assert_eq!(m.cursor(), at(1, 3));
  }

  #[test]
  fn word_movement_within_line() {
    let mut m = EditorModel::new("foo bar baz");
    m.doc_end(false);
    m.word_left(false);
    assert_eq!(m.cursor(), at(0, 8));
    m.word_left(false);
    assert_eq!(m.cursor(), at(0, 4));
    m.word_right(false);
    assert_eq!(m.cursor(), at(0, 7));
  }

  #[test]
  fn word_movement_crosses_lines() {
    let mut m = EditorModel::new("foo\nbar");
    m.move_to(1, 0, false);
    m.word_left(false);
    assert_eq!(m.cursor(), at(0, 0));
    m.word_right(false);
    assert_eq!(m.cursor(), at(0, 3));
    m.word_right(false);
    assert_eq!(m.cursor(), at(1, 3));
  }

  #[test]
  fn extend_grows_and_normalizes_selection() {
    let mut m = EditorModel::new("abcdef");
    m.move_to(0, 2, false);
    m.move_right(true);
    m.move_right(true);
    assert_eq!(m.selection(), Some((at(0, 2), at(0, 4))));
    // Extending left of the anchor still yields a normalized range.
    let mut m = EditorModel::new("abcdef");
    m.move_to(0, 4, false);
    m.move_left(true);
    m.move_left(true);
    assert_eq!(m.selection(), Some((at(0, 2), at(0, 4))));
    assert_eq!(m.selected_text().as_deref(), Some("cd"));
  }

  #[test]
  fn empty_selection_is_none() {
    let mut m = EditorModel::new("abc");
    m.move_right(true);
    m.move_left(true); // back to the anchor
    assert_eq!(m.selection(), None);
    assert_eq!(m.selected_text(), None);
  }

  #[test]
  fn plain_move_collapses_selection_to_edge() {
    let mut m = EditorModel::new("abcdef");
    m.move_to(0, 2, false);
    m.move_to(0, 4, true);
    m.move_left(false);
    assert_eq!(m.cursor(), at(0, 2));
    assert_eq!(m.selection(), None);
    let mut m = EditorModel::new("abcdef");
    m.move_to(0, 2, false);
    m.move_to(0, 4, true);
    m.move_right(false);
    assert_eq!(m.cursor(), at(0, 4));
    assert_eq!(m.selection(), None);
  }

  #[test]
  fn selected_text_multiline() {
    let mut m = EditorModel::new("aaa\nbbb\nccc");
    m.move_to(0, 1, false);
    m.move_to(2, 2, true);
    assert_eq!(m.selected_text().as_deref(), Some("aa\nbbb\ncc"));
  }

  #[test]
  fn delete_selection_joins_lines() {
    let mut m = EditorModel::new("aaa\nbbb\nccc");
    m.move_to(0, 2, false);
    m.move_to(2, 1, true);
    assert!(m.delete_selection());
    assert_eq!(m.text(), "aacc");
    assert_eq!(m.cursor(), at(0, 2));
    assert!(!m.delete_selection());
  }

  #[test]
  fn cut_and_copy() {
    let mut m = EditorModel::new("hello world");
    assert_eq!(m.copy(), None);
    assert_eq!(m.cut(), None);
    m.move_to(0, 0, false);
    m.word_right(true);
    assert_eq!(m.copy().as_deref(), Some("hello"));
    assert_eq!(m.text(), "hello world"); // copy leaves the doc alone
    assert_eq!(m.cut().as_deref(), Some("hello"));
    assert_eq!(m.text(), " world");
    assert!(m.undo());
    assert_eq!(m.text(), "hello world");
  }

  #[test]
  fn select_all_spans_document() {
    let mut m = EditorModel::new("ab\ncd");
    m.select_all();
    assert_eq!(m.selection(), Some((at(0, 0), at(1, 2))));
    assert_eq!(m.selected_text().as_deref(), Some("ab\ncd"));
    m.clear_selection();
    assert_eq!(m.selection(), None);
  }

  #[test]
  fn select_word_variants() {
    // Mid-word.
    let mut m = EditorModel::new("foo bar_baz qux");
    m.move_to(0, 6, false);
    m.select_word();
    assert_eq!(m.selected_text().as_deref(), Some("bar_baz"));
    // At a word end (cursor just past the last char).
    m.move_to(0, 3, false);
    m.select_word();
    assert_eq!(m.selected_text().as_deref(), Some("foo"));
    // On a non-word char (with no word char adjacent on the left):
    // selects just that char.
    let mut m = EditorModel::new("foo .. bar");
    m.move_to(0, 5, false);
    m.select_word();
    assert_eq!(m.selection(), Some((at(0, 5), at(0, 6))));
    // Empty line: nothing to select.
    let mut m = EditorModel::new("");
    m.select_word();
    assert_eq!(m.selection(), None);
  }

  #[test]
  fn select_line_spans_line() {
    let mut m = EditorModel::new("abc\ndef");
    m.move_to(1, 1, false);
    m.select_line();
    assert_eq!(m.selection(), Some((at(1, 0), at(1, 3))));
    assert_eq!(m.selected_text().as_deref(), Some("def"));
  }

  #[test]
  fn select_word_within_clamps_to_the_cell() {
    // `| foo bar | baz |`：第二格内容在 char 12..15（`baz`）。
    let line = "| foo bar | baz |";
    let mut m = EditorModel::new(line);
    // 在第二格里双击 `baz` 中间 → 只选中 `baz`，不会带上管道或前一格。
    m.move_to(0, 13, false);
    m.select_word_within(12, 15);
    assert_eq!(m.selected_text().as_deref(), Some("baz"));
    // 第一格里双击 `bar` → 只选中 `bar`（不越到 `foo` 或管道）。
    m.move_to(0, 8, false);
    m.select_word_within(2, 9);
    assert_eq!(m.selected_text().as_deref(), Some("bar"));
    // 格首双击非词字符 → 选中整个格内容。
    m.move_to(0, 12, false);
    m.select_word_within(12, 15);
    assert_eq!(m.selected_text().as_deref(), Some("baz"));
  }

  #[test]
  fn select_range_selects_the_cell_content() {
    // 三击选格：`| foo bar | baz |` 第二格内容 12..15。
    let mut m = EditorModel::new("| foo bar | baz |");
    m.move_to(0, 13, false);
    m.select_range(12, 15);
    assert_eq!(m.selected_text().as_deref(), Some("baz"));
    // 空格：范围折叠，不产生选区。
    m.select_range(5, 5);
    assert_eq!(m.selection(), None);
  }

  #[test]
  fn select_word_within_never_escapes_the_range() {
    // 词跨越范围边界时必须截断（拖到格边界就停）。
    let mut m = EditorModel::new("aaa bbb ccc");
    m.move_to(0, 5, false);
    m.select_word_within(4, 7);
    assert_eq!(m.selected_text().as_deref(), Some("bbb"));
    // 光标在范围外时夹进范围。
    m.move_to(0, 9, false);
    m.select_word_within(4, 7);
    let sel = m.selection().expect("有选区");
    assert!(sel.0.col >= 4 && sel.1.col <= 7, "选区必须夹在范围内：{sel:?}");
  }

  #[test]
  fn undo_redo_roundtrip() {
    let mut m = EditorModel::new("ab");
    m.doc_end(false);
    m.newline();
    m.insert("cd");
    assert_eq!(m.text(), "ab\ncd");
    assert!(m.undo());
    assert_eq!(m.text(), "ab\n");
    assert!(m.undo());
    assert_eq!(m.text(), "ab");
    assert_eq!(m.cursor(), at(0, 2));
    assert!(!m.undo());
    assert!(m.redo());
    assert_eq!(m.text(), "ab\n");
    assert!(m.redo());
    assert_eq!(m.text(), "ab\ncd");
    assert!(!m.redo());
  }

  #[test]
  fn typing_coalesces_into_one_undo_step() {
    let mut m = EditorModel::new("");
    m.insert("a");
    m.insert("b");
    m.insert("c");
    assert!(m.undo());
    assert_eq!(m.text(), "");
    assert!(!m.undo());
    assert!(m.redo());
    assert_eq!(m.text(), "abc");
  }

  #[test]
  fn replace_rows_and_move_merges_into_typing_run() {
    // spec #13：表格自动重排与打字合成**一步**撤销（重排收尾的 move_to
    // 不打断 run，行改写也不额外入栈）。
    let mut m = EditorModel::new("| a |\n");
    m.move_to(0, 2, false);
    m.insert("x"); // 打字 run 开始（pre 状态入栈）
    m.replace_rows_and_move(&[(0, "| xa |".to_string())], Some(Pos::new(0, 3)));
    assert_eq!(m.text(), "| xa |\n");
    assert_eq!(m.cursor(), at(0, 3));
    assert!(m.undo(), "一步撤销应跨过打字+重排");
    assert_eq!(m.text(), "| a |\n");
    assert!(!m.undo(), "不应再有第二步");
  }

  #[test]
  fn replace_rows_and_move_stands_alone_as_one_step() {
    // 不在打字 run 中时，重排自成一步撤销。
    let mut m = EditorModel::new("old\n");
    m.replace_rows_and_move(&[(0, "new".to_string())], None);
    assert_eq!(m.text(), "new\n");
    assert!(m.undo());
    assert_eq!(m.text(), "old\n");
  }

  #[test]
  fn movement_breaks_coalescing() {
    let mut m = EditorModel::new("");
    m.insert("a");
    m.insert("b");
    m.move_left(false);
    m.move_right(false);
    m.insert("c");
    assert!(m.undo());
    assert_eq!(m.text(), "ab");
    assert!(m.undo());
    assert_eq!(m.text(), "");
  }

  #[test]
  fn structural_edits_break_coalescing() {
    let mut m = EditorModel::new("");
    m.insert("a");
    m.newline();
    m.insert("b");
    assert!(m.undo());
    assert_eq!(m.text(), "a\n");
    assert!(m.undo());
    assert_eq!(m.text(), "a");
    assert!(m.undo());
    assert_eq!(m.text(), "");
    // Backspace also breaks a run.
    let mut m = EditorModel::new("");
    m.insert("a");
    m.insert("b");
    m.backspace();
    m.insert("c");
    assert!(m.undo());
    assert_eq!(m.text(), "a");
    assert!(m.undo());
    assert_eq!(m.text(), "ab");
    assert!(m.undo());
    assert_eq!(m.text(), "");
  }

  #[test]
  fn selection_replacement_is_its_own_undo_step() {
    let mut m = EditorModel::new("");
    m.insert("a");
    m.select_all();
    m.insert("b"); // single char, but it replaced a selection
    assert!(m.undo());
    assert_eq!(m.text(), "a");
    assert!(m.undo());
    assert_eq!(m.text(), "");
  }

  #[test]
  fn new_edit_clears_redo() {
    let mut m = EditorModel::new("");
    m.insert("a");
    m.undo();
    assert!(m.can_redo());
    m.insert("b");
    assert!(!m.can_redo());
    assert!(!m.redo());
    assert_eq!(m.text(), "b");
  }

  #[test]
  fn pos_for_click_clamps() {
    let m = EditorModel::new("ab\ncdef");
    assert_eq!(m.pos_for_click(0, 99), at(0, 2)); // past the line end
    assert_eq!(m.pos_for_click(9, 1), at(1, 1)); // past the last line
    assert_eq!(m.pos_for_click(9, 99), at(1, 4)); // past both
  }

  #[test]
  fn move_to_extend_selects_like_a_drag() {
    let mut m = EditorModel::new("hello\nworld");
    m.move_to(0, 1, false);
    m.move_to(1, 3, true);
    assert_eq!(m.selected_text().as_deref(), Some("ello\nwor"));
    // Dragging backwards past the anchor flips the range.
    m.move_to(0, 0, true);
    assert_eq!(m.selection(), Some((at(0, 0), at(0, 1))));
  }

  #[test]
  fn utf8_editing() {
    let mut m = EditorModel::new("café");
    m.doc_end(false);
    assert_eq!(m.cursor(), at(0, 4)); // chars, not bytes
    assert!(m.backspace());
    assert_eq!(m.text(), "caf");
    m.insert("é");
    assert_eq!(m.text(), "café");
  }

  #[test]
  fn utf8_cjk_positions() {
    let mut m = EditorModel::new("日本語\nhello");
    m.move_to(0, 1, false);
    m.insert("!");
    assert_eq!(m.text(), "日!本語\nhello");
    assert_eq!(m.cursor(), at(0, 2));
    assert!(m.delete());
    assert_eq!(m.text(), "日!語\nhello");
    // Vertical movement counts chars, and clamps clicks per line.
    m.end(false);
    m.move_down(false);
    assert_eq!(m.cursor(), at(1, 3));
    assert_eq!(m.pos_for_click(0, 99), at(0, 3));
  }

  #[test]
  fn delete_pair_removes_both_as_one_undo() {
    let mut m = EditorModel::new("(a)");
    m.move_to(0, 1, false);
    m.insert_pair('[', ']');
    assert_eq!(m.text(), "([]a)");
    assert!(m.delete_pair());
    assert_eq!(m.text(), "(a)");
    assert_eq!(m.cursor(), at(0, 1));
    // One undo restores the just-deleted pair (not the pre-insert state).
    assert!(m.undo());
    assert_eq!(m.text(), "([]a)");
    assert!(m.undo());
    assert_eq!(m.text(), "(a)");
    // Not inside a pair: no-op.
    m.move_to(0, 2, false);
    assert!(!m.delete_pair());
    assert_eq!(m.text(), "(a)");
  }

  #[test]
  fn pair_insert_and_closer_skip() {
    let mut m = EditorModel::new("()");
    m.move_to(0, 0, false);
    m.insert_pair('(', ')');
    assert_eq!(m.text(), "()()");
    assert_eq!(m.cursor(), at(0, 1));
    assert!(m.undo());
    assert_eq!(m.text(), "()");
    // Closer skips over the identical char ahead.
    m.move_to(0, 1, false);
    assert!(m.insert_closer(')'));
    assert_eq!(m.cursor(), at(0, 2));
    // …but inserts when the next char differs.
    m.move_to(0, 0, false);
    assert!(!m.insert_closer(']'));
    assert_eq!(m.text(), "]()");
  }

  #[test]
  fn wrap_selection_single_undo() {
    let mut m = EditorModel::new("hello");
    m.move_to(0, 0, false);
    m.move_to(0, 5, true);
    assert!(m.wrap_selection('*', '*'));
    assert_eq!(m.text(), "*hello*");
    assert!(m.undo());
    assert_eq!(m.text(), "hello");
    assert!(!m.wrap_selection('(', ')'));
  }

  #[test]
  fn replace_ranges_single_undo_step() {
    let mut m = EditorModel::new("foo bar foo");
    m.replace_ranges(&[(at(0, 0), at(0, 3)), (at(0, 8), at(0, 11))], "baz");
    assert_eq!(m.text(), "baz bar baz");
    assert!(m.can_undo());
    assert!(m.undo());
    assert_eq!(m.text(), "foo bar foo");
  }

  #[test]
  fn replace_ranges_each_single_undo_step() {
    let mut m = EditorModel::new("foo bar foo");
    m.replace_ranges_each(&[
      (at(0, 0), at(0, 3), "BAZ".to_string()),
      (at(0, 8), at(0, 11), "qux".to_string()),
    ]);
    assert_eq!(m.text(), "BAZ bar qux");
    assert!(m.undo());
    assert_eq!(m.text(), "foo bar foo");
  }

  #[test]
  fn replace_range_normalizes_reversed() {
    let mut m = EditorModel::new("hello");
    m.replace_range(at(0, 5), at(0, 0), "hi");
    assert_eq!(m.text(), "hi");
  }

  #[test]
  fn utf8_selection_and_words() {
    let mut m = EditorModel::new("voilà café");
    m.doc_end(false);
    m.word_left(false);
    assert_eq!(m.cursor(), at(0, 6)); // é is a word char
    m.word_left(true);
    assert_eq!(m.selected_text().as_deref(), Some("voilà "));
    m.move_to(0, 8, false);
    m.select_word();
    assert_eq!(m.selected_text().as_deref(), Some("café"));
  }

  /// The undo history is size-bounded: beyond `MAX_UNDO_STEPS` distinct edits
  /// the oldest steps are dropped, so `undo().len()` never exceeds the cap.
  /// This is the memory optimization of M1 — a hard bound on how much full
  /// document history is retained, at the cost of losing the most ancient
  /// undo steps (a deliberate product trade-off).
  #[test]
  fn undo_history_is_bounded_and_drops_oldest() {    let mut m = EditorModel::new("");
    // Volume far above the cap on purpose: the retained depth must stay flat
    // (memory ≤ cap × doc) no matter how many edits accumulate.
    let n = MAX_UNDO_STEPS + 2000;
    for _ in 0..n {
      // `insert_pair` always records a fresh non-coalesced undo step.
      m.insert_pair('x', 'x');
    }
    // Only the most recent MAX steps are retained.
    let mut undone = 0usize;
    while m.undo() {
      undone += 1;
    }
    assert_eq!(
      undone, MAX_UNDO_STEPS,
      "must retain exactly the most recent {MAX_UNDO_STEPS} steps"
    );
    // Nothing older remains: the oldest 30 edits were dropped.
    assert!(!m.undo());
    // The document after undoing all retained steps equals the state right
    // after edit #30 (i.e. `n - MAX_UNDO_STEPS` edits of "xx").
    assert_eq!(m.text(), "xx".repeat(n - MAX_UNDO_STEPS));
    // Redo is bounded the same way (it inherits the retained undo depth).
    let mut redone = 0usize;
    while m.redo() {
      redone += 1;
    }
    assert_eq!(redone, MAX_UNDO_STEPS);
    assert!(!m.redo());
    assert_eq!(m.text(), "xx".repeat(n));
  }

  /// Undo snapshots share line text with the live document (`Arc<String>`),
  /// so retained history is bounded by *pointer tables*, not by
  /// `steps × document bytes`.
  ///
  /// This is the memory fix for the live editor's large-document blow-up.
  /// The remaining per-step cost is inherent to whole-document snapshots:
  /// `lines × 8` bytes for the `Vec<Arc<String>>` table. So the win over the
  /// old full copy is a factor of `document_bytes / (lines × 8)` — i.e. the
  /// average line length over 8 — which for prose is ~5-6x, and grows with
  /// line length (long paragraphs, CJK text, base64/URL-heavy lines).
  ///
  /// Measured with the test allocator's live-byte high-water mark, so it
  /// proves *retained* memory rather than allocation counts.
  #[test]
  fn undo_snapshots_share_line_text_instead_of_copying_it() {
    use crate::utils::alloc_probe::AllocObserver;

    const LINES: usize = 2000;
    let mut doc = String::new();
    for i in 0..LINES {
      // ~56 B/line: a realistic prose line.
      doc.push_str(&format!("line {i:04} with some filler text to make it long\n"));
    }
    let doc_bytes = doc.len() as u64;
    let mut m = EditorModel::new(&doc);

    // One edit per step, each non-coalesced, so every snapshot is retained
    // until the cap. `insert_pair` never coalesces.
    let ((), stats) = AllocObserver::scope(|| {
      for _ in 0..MAX_UNDO_STEPS {
        m.insert_pair('z', 'z');
      }
    });

    // A full-copy implementation retains ≈ MAX_UNDO_STEPS × doc_bytes
    // (≈ 19 MB here). Sharing retains one pointer table per step
    // (MAX_UNDO_STEPS × LINES × 8 ≈ 3.2 MB) plus the edited line text.
    let full_copy = doc_bytes * MAX_UNDO_STEPS as u64;
    let pointer_tables = LINES as u64 * 8 * MAX_UNDO_STEPS as u64;
    // Generous slack for the edited lines' text and allocator bookkeeping.
    let budget = pointer_tables * 2;
    assert!(
      stats.live_bytes_high_water < budget,
      "undo history retained {} bytes for a {} byte document \
       (budget {}): snapshots are copying line text instead of sharing it",
      stats.live_bytes_high_water,
      doc_bytes,
      budget
    );
    assert!(
      stats.live_bytes_high_water < full_copy / 4,
      "sharing must beat the old full-copy cost by a wide margin: \
       {} vs {} bytes",
      stats.live_bytes_high_water,
      full_copy
    );

    // Sharing must not break undo semantics: the retained steps still walk
    // back correctly, and the untouched lines are byte-identical.
    let mut steps = 0usize;
    while m.undo() {
      steps += 1;
    }
    assert_eq!(steps, MAX_UNDO_STEPS);
    assert!(m.text().starts_with("line 0000 with some filler text"));
    assert!(m.text().contains("line 1999 with some filler text"));
  }

  /// Copy-on-write must not alias: editing a line after a snapshot was taken
  /// must not mutate the snapshot's copy (the classic `Arc` sharing bug).
  #[test]
  fn snapshot_lines_are_isolated_from_later_edits() {
    let mut m = EditorModel::new("alpha\nbeta\ngamma");
    m.move_to(1, 4, false);
    m.insert("!"); // snapshot now holds "beta"
    assert_eq!(m.text(), "alpha\nbeta!\ngamma");
    m.undo();
    assert_eq!(m.text(), "alpha\nbeta\ngamma", "snapshot text was mutated in place");
    m.redo();
    assert_eq!(m.text(), "alpha\nbeta!\ngamma");
  }
}
