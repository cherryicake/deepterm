//! What a diff view shows and where: the rows files, hunks and expanded ranges make, the
//! line selection, and the mapping back from a line to its row.
//!
//! Pure state: no GPUI types, so the row layout and selection can be unit tested without a
//! window. The view reads these rows by index from a virtual list.

use std::collections::HashMap;

use super::model::line_starts;
use super::model::{FileDiff, LineKind, Side};

/// How a diff lays out its two sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DiffMode {
    /// One column, removed lines above the added lines that replace them.
    #[default]
    Unified,
    /// Old on the left, new on the right, changed lines side by side. An added or deleted
    /// file shows its one side as a single column.
    Split,
}

/// A line of one side of a file: what selections and navigation point at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineRef {
    pub side: Side,
    /// 1-based.
    pub line: u32,
}

impl LineRef {
    pub fn new(side: Side, line: u32) -> LineRef {
        LineRef { side, line }
    }
}

/// Selected lines of one file, from where the selection started to where it ends (either
/// way round), as a click, shift-click or drag down the line numbers makes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineRange {
    pub file: usize,
    pub start: LineRef,
    pub end: LineRef,
}

impl LineRange {
    /// One line.
    pub fn line(file: usize, at: LineRef) -> LineRange {
        LineRange {
            file,
            start: at,
            end: at,
        }
    }
}

/// Which way an unchanged range between hunks opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expand {
    /// [`EXPAND_STEP`] lines up from the hunk below it.
    Up,
    /// [`EXPAND_STEP`] lines down from the hunk above it.
    Down,
    /// All of it.
    All,
}

/// Lines one expand step reveals.
pub const EXPAND_STEP: u32 = 20;

/// One row of the virtual list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A file's header.
    File(u32),
    /// A file with no lines to show: binary, or only renamed or re-moded.
    Note(u32),
    /// The unchanged range before hunk `gap` (`hunks.len()`: after the last hunk), folded,
    /// with its expand buttons and the next hunk's header.
    Gap { f: u32, gap: u32 },
    /// An unchanged line revealed from a folded range, by its old and new numbers.
    Ctx { f: u32, old: u32, new: u32 },
    /// Line `i` of hunk `h`. In split mode, one without a partner on the other side.
    Line { f: u32, h: u32, i: u32 },
    /// Split mode: removed line `del` beside added line `add`, both of hunk `h`.
    Pair { f: u32, h: u32, del: u32, add: u32 },
}

impl Row {
    pub fn file(self) -> usize {
        match self {
            Row::File(f)
            | Row::Note(f)
            | Row::Gap { f, .. }
            | Row::Ctx { f, .. }
            | Row::Line { f, .. }
            | Row::Pair { f, .. } => f as usize,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FileUi {
    collapsed: bool,
    /// Lines revealed at the top and bottom of each unchanged range (`hunks.len() + 1` of them).
    revealed: Vec<(u32, u32)>,
}

/// An unchanged range before a hunk: its first old and new line numbers and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Gap {
    old: u32,
    new: u32,
    len: u32,
}

/// A diff and everything about how it is shown: the app keeps one in its state and
/// [`DiffView`](super::DiffView) draws it.
pub struct DiffState {
    files: Vec<FileDiff>,
    ui: Vec<FileUi>,
    mode: DiffMode,
    selection: Option<LineRange>,
    selecting: bool,
    selection_at_press: Option<LineRange>,
    /// The press was on the one selected line: it clears the selection unless it drags.
    unselect: bool,
    shift: bool,
    /// Per file, per hunk, per side: the index in the hunk of each of its lines on that side.
    side_lines: Vec<Vec<[Vec<u32>; 2]>>,
    gaps: Vec<Vec<Gap>>,
    /// Per file, the digits its line numbers take (at least three).
    digits: Vec<u32>,
    /// The widest line of any file, in columns.
    columns: u32,
    pub(crate) rows: Vec<Row>,
    pub(crate) keys: Vec<String>,
    row_of: HashMap<(u32, Side, u32), u32>,
    file_rows: Vec<u32>,
    hunk_rows: Vec<u32>,
    /// Bumped whenever the rows change, so views know to re-splice their virtual list.
    pub(crate) rev: u64,
    /// The row the reader last navigated to, if any.
    nav: Option<u32>,
}

impl std::fmt::Debug for DiffState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffState")
            .field("files", &self.files.len())
            .field("rows", &self.rows.len())
            .field("mode", &self.mode)
            .finish()
    }
}

impl Default for DiffState {
    fn default() -> Self {
        DiffState::new(Vec::new())
    }
}

impl DiffState {
    pub fn new(files: Vec<FileDiff>) -> DiffState {
        let mut s = DiffState {
            files: Vec::new(),
            ui: Vec::new(),
            mode: DiffMode::Unified,
            selection: None,
            selecting: false,
            selection_at_press: None,
            unselect: false,
            shift: false,
            side_lines: Vec::new(),
            gaps: Vec::new(),
            digits: Vec::new(),
            columns: 0,
            rows: Vec::new(),
            keys: Vec::new(),
            row_of: HashMap::new(),
            file_rows: Vec::new(),
            hunk_rows: Vec::new(),
            rev: 0,
            nav: None,
        };
        s.set_files(files);
        s
    }

    /// Show `files` instead, every range collapsed again and the selection cleared.
    pub fn set_files(&mut self, files: Vec<FileDiff>) {
        self.ui = files
            .iter()
            .map(|f| FileUi {
                collapsed: false,
                revealed: vec![(0, 0); f.hunks.len() + 1],
            })
            .collect();
        self.side_lines = files
            .iter()
            .map(|f| {
                f.hunks
                    .iter()
                    .map(|h| {
                        let on = |side: Side| {
                            (0..h.lines.len() as u32)
                                .filter(|&i| h.lines[i as usize].number(side).is_some())
                                .collect()
                        };
                        [on(Side::Old), on(Side::New)]
                    })
                    .collect()
            })
            .collect();
        self.gaps = files.iter().map(gaps).collect();
        self.digits = files
            .iter()
            .map(|f| f.last_line().max(1).ilog10() + 1)
            .map(|d| d.max(3))
            .collect();
        self.columns = files.iter().map(columns_of).max().unwrap_or(0);
        self.files = files;
        self.selection = None;
        self.rebuild();
    }

    pub fn files(&self) -> &[FileDiff] {
        &self.files
    }

    /// The index of the file at `path` (its new path, or old for a deleted file).
    pub fn file_index(&self, path: &str) -> Option<usize> {
        self.files
            .iter()
            .position(|f| f.path() == path || f.old_path.as_deref() == Some(path))
    }

    pub fn mode(&self) -> DiffMode {
        self.mode
    }

    /// Switch between unified and split. Rows keep their keys where they can (unchanged and
    /// removed lines), so the reader's place can be kept across the switch.
    pub fn set_mode(&mut self, mode: DiffMode) {
        if self.mode != mode {
            self.mode = mode;
            self.rebuild();
        }
    }

    /// In split mode, the one side `file` shows across the whole width, as a single column:
    /// an added file's new side or a deleted file's old side ([`FileDiff::sole_side`]).
    /// `None` in unified mode and for a file with both sides.
    pub fn single_column(&self, file: usize) -> Option<Side> {
        if self.mode != DiffMode::Split {
            return None;
        }
        self.files.get(file)?.sole_side()
    }

    pub fn collapsed(&self, file: usize) -> bool {
        self.ui.get(file).is_some_and(|u| u.collapsed)
    }

    /// Show only `file`'s header, or its lines again.
    pub fn set_collapsed(&mut self, file: usize, collapsed: bool) {
        if let Some(u) = self.ui.get_mut(file).filter(|u| u.collapsed != collapsed) {
            u.collapsed = collapsed;
            self.rebuild();
        }
    }

    /// Lines of `file`'s unchanged range before hunk `gap` still hidden. `gap ==
    /// files[file].hunks.len()` is the range after the last hunk.
    pub fn hidden(&self, file: usize, gap: usize) -> u32 {
        let Some(g) = self.gaps.get(file).and_then(|g| g.get(gap)) else {
            return 0;
        };
        let (top, bottom) = self.ui[file].revealed[gap];
        g.len.saturating_sub(top + bottom)
    }

    /// The lines hidden before hunk `gap`: their old and new numbers and count.
    pub fn gap_range(&self, file: usize, gap: usize) -> Option<(u32, u32, u32)> {
        let g = self.gaps.get(file)?.get(gap)?;
        let (top, _) = self.ui[file].revealed[gap];
        Some((g.old + top, g.new + top, self.hidden(file, gap)))
    }

    /// Reveal more of `file`'s unchanged range before hunk `gap` (`hunks.len()`: after the
    /// last). Does nothing without the file's whole text.
    pub fn expand(&mut self, file: usize, gap: usize, how: Expand) {
        if !self.files.get(file).is_some_and(FileDiff::expandable) {
            return;
        }
        let hidden = self.hidden(file, gap);
        if hidden == 0 {
            return;
        }
        let hunks = self.files[file].hunks.len();
        let r = &mut self.ui[file].revealed[gap];
        let step = hidden.min(EXPAND_STEP);
        match how {
            Expand::All => r.0 += hidden,
            // "Up" grows from the hunk below the range, so it takes from its bottom; after
            // the last hunk there is no hunk below, and it grows from the top instead.
            Expand::Up if gap < hunks => r.1 += step,
            Expand::Up => r.0 += step,
            // "Down" grows from the hunk above, so it takes from the range's top.
            Expand::Down => r.0 += step,
        }
        self.rebuild();
    }

    /// Reveal everything in every range.
    pub fn expand_all(&mut self) {
        for (f, ui) in self.ui.iter_mut().enumerate() {
            for (gap, r) in ui.revealed.iter_mut().enumerate() {
                let len = self.gaps[f][gap].len;
                *r = (len, 0);
            }
        }
        self.rebuild();
    }

    /// Fold every unchanged range again.
    pub fn collapse_all(&mut self) {
        for ui in self.ui.iter_mut() {
            for r in ui.revealed.iter_mut() {
                *r = (0, 0);
            }
        }
        self.rebuild();
    }

    pub fn selection(&self) -> Option<LineRange> {
        self.selection
    }

    pub fn set_selection(&mut self, sel: Option<LineRange>) {
        self.selection = sel;
    }

    /// The selected lines' text on the side the selection started on, one per line, without
    /// numbers or markers: what copying the selection gives.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let side = sel.start.side;
        let (a, b) = self.selected_rows(&sel)?;
        let mut out = Vec::new();
        for r in a..=b {
            if let Some(n) = self.row_line(self.rows[r as usize], side) {
                out.push(
                    self.side_line(sel.file, side, n)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
        Some(out.join("\n"))
    }

    /// The selection's first and last rows.
    fn selected_rows(&self, sel: &LineRange) -> Option<(u32, u32)> {
        let a = *self
            .row_of
            .get(&(sel.file as u32, sel.start.side, sel.start.line))?;
        let b = *self
            .row_of
            .get(&(sel.file as u32, sel.end.side, sel.end.line))?;
        Some((a.min(b), a.max(b)))
    }

    /// Whether line `at` is in `sel`.
    pub fn contains(&self, sel: &LineRange, at: LineRef) -> bool {
        let Some((a, b)) = self.selected_rows(sel) else {
            return false;
        };
        self.row_of
            .get(&(sel.file as u32, at.side, at.line))
            .is_some_and(|r| (a..=b).contains(r))
    }

    /// Whether row `r`'s line on `side` is selected. `None` reads the row as a whole (any
    /// side selected counts).
    pub(crate) fn row_selected(&self, r: usize, side: Option<Side>) -> bool {
        let Some(sel) = self.selection else {
            return false;
        };
        let Some((a, b)) = self.selected_rows(&sel) else {
            return false;
        };
        if self.rows.get(r).is_none_or(|row| row.file() != sel.file)
            || !(a as usize..=b as usize).contains(&r)
        {
            return false;
        }
        side.is_none_or(|s| s == sel.start.side || s == sel.end.side)
    }

    /// A press on line `at`'s number: a new selection, or with Shift held one extended to it.
    /// Pressing the one selected line clears it on release, unless the press drags.
    pub(crate) fn press_line(&mut self, file: usize, at: LineRef) {
        self.selection_at_press = self.selection;
        let sel = match self.selection {
            Some(s) if self.shift && s.file == file => LineRange { end: at, ..s },
            _ => LineRange::line(file, at),
        };
        self.unselect = !self.shift && self.selection == Some(sel);
        self.selecting = true;
        self.set_selection(Some(sel));
    }

    /// The pointer, dragging from a line number, reached line `at`.
    pub(crate) fn drag_to(&mut self, file: usize, at: LineRef) {
        if let Some(s) = self
            .selection
            .filter(|s| self.selecting && s.file == file && s.end != at)
        {
            self.unselect = false;
            self.set_selection(Some(LineRange { end: at, ..s }));
        }
    }

    /// The press on the line numbers ended: the selection it leaves.
    pub(crate) fn release_line(&mut self) -> Option<LineRange> {
        self.selection_at_press = None;
        if std::mem::take(&mut self.unselect) {
            self.selection = None;
        }
        self.selecting = false;
        self.selection
    }

    pub fn cancel_line(&mut self) {
        if self.selecting {
            self.selection = self.selection_at_press.take();
        }
        self.selecting = false;
        self.unselect = false;
    }

    /// Shift is held (or was released), for the next press on a line number.
    pub fn set_shift(&mut self, shift: bool) {
        self.shift = shift;
    }

    /// The row a hunk starts at, or `None` past the end.
    pub fn hunk_row(&self, index: usize) -> Option<u32> {
        self.hunk_rows.get(index).copied()
    }

    pub fn hunk_count(&self) -> usize {
        self.hunk_rows.len()
    }

    /// The row of the next (or previous) hunk after `from`, wrapping is the caller's choice.
    pub fn next_hunk_row(&self, from: u32, forward: bool) -> Option<u32> {
        if forward {
            self.hunk_rows.iter().copied().find(|&r| r > from)
        } else {
            self.hunk_rows.iter().rev().copied().find(|&r| r < from)
        }
    }

    /// The first hunk's row.
    pub fn first_hunk_row(&self) -> Option<u32> {
        self.hunk_rows.first().copied()
    }

    /// The row of a file's header.
    pub fn file_row(&self, file: usize) -> Option<u32> {
        self.file_rows.get(file).copied()
    }

    /// The row showing `line` of `side`, when it is on screen (not folded).
    pub fn row_of_line(&self, file: usize, at: LineRef) -> Option<u32> {
        self.row_of.get(&(file as u32, at.side, at.line)).copied()
    }

    /// The index of the row with `key`, if it survived the last rebuild.
    pub fn row_of_key(&self, key: &str) -> Option<usize> {
        self.keys.iter().position(|k| k == key)
    }

    /// The key of row `r`.
    pub fn key(&self, r: usize) -> Option<&str> {
        self.keys.get(r).map(String::as_str)
    }

    /// Line `n` of `side` of file `f`, from its whole text, its hunks or an unchanged range.
    pub(crate) fn side_line(&self, f: usize, side: Side, n: u32) -> Option<&str> {
        let file = &self.files[f];
        let hunks = &file.hunks;
        let h = hunks.partition_point(|h| h.end(side) <= n);
        if let Some(hunk) = hunks.get(h).filter(|h| h.first(side) <= n) {
            let i = self.side_lines[f][h][side as usize].get((n - hunk.first(side)) as usize)?;
            return Some(&hunk.lines[*i as usize].text);
        }
        let g = self.gaps[f][h];
        let off = n.checked_sub(if side == Side::Old { g.old } else { g.new })?;
        file.context_line(g.old.saturating_add(off), g.new.saturating_add(off))
    }

    /// The same as [`DiffState::side_line`] but public, for the view's highlight pass.
    pub fn line_text(&self, file: usize, side: Side, n: u32) -> Option<&str> {
        self.side_line(file, side, n)
    }

    /// Row `r`'s line number on `side`.
    pub fn row_line(&self, r: Row, side: Side) -> Option<u32> {
        match r {
            Row::Ctx { old, new, .. } => Some(if side == Side::Old { old } else { new }),
            Row::Line { f, h, i } => {
                self.files[f as usize].hunks[h as usize].lines[i as usize].number(side)
            }
            Row::Pair { f, h, del, add } => {
                let i = if side == Side::Old { del } else { add };
                self.files[f as usize].hunks[h as usize].lines[i as usize].number(side)
            }
            _ => None,
        }
    }

    /// The digits `file`'s line numbers take.
    pub fn digits(&self, file: usize) -> u32 {
        self.digits.get(file).copied().unwrap_or(3)
    }

    /// The widest line of any file, in columns.
    pub fn columns(&self) -> u32 {
        self.columns
    }

    /// Lay the rows out again from the files, modes and expanded ranges.
    fn rebuild(&mut self) {
        self.rev += 1;
        self.nav = None;
        let mut rows = Vec::new();
        self.file_rows.clear();
        self.hunk_rows.clear();
        for (f, file) in self.files.iter().enumerate() {
            let fu = f as u32;
            self.file_rows.push(rows.len() as u32);
            rows.push(Row::File(fu));
            if self.ui[f].collapsed {
                continue;
            }
            if file.hunks.is_empty() {
                rows.push(Row::Note(fu));
                continue;
            }
            for gap in 0..=file.hunks.len() {
                let g = self.gaps[f][gap];
                let (top, bottom) = self.ui[f].revealed[gap];
                let ctx = |k: u32| Row::Ctx {
                    f: fu,
                    old: g.old + k,
                    new: g.new + k,
                };
                rows.extend((0..top.min(g.len)).map(ctx));
                let hidden = g.len.saturating_sub(top + bottom);
                // The hunk row is where navigation lands: the folded range above the hunk
                // (whose row also shows the hunk's `@@` header), or the hunk's first line.
                let first_row = rows.len() as u32;
                if hidden > 0 {
                    rows.push(Row::Gap {
                        f: fu,
                        gap: gap as u32,
                    });
                }
                let keep = bottom.min(g.len.saturating_sub(top));
                rows.extend((g.len - keep..g.len).map(ctx));
                let Some(h) = file.hunks.get(gap) else { break };
                self.hunk_rows.push(first_row);
                let hu = gap as u32;
                let lines = &h.lines;
                let mut i = 0;
                while i < lines.len() {
                    if self.mode == DiffMode::Split && lines[i].kind == LineKind::Removed {
                        let del = i;
                        while i < lines.len() && lines[i].kind == LineKind::Removed {
                            i += 1;
                        }
                        let add = i;
                        while i < lines.len() && lines[i].kind == LineKind::Added {
                            i += 1;
                        }
                        let (nd, na) = (add - del, i - add);
                        for k in 0..nd.max(na) {
                            rows.push(match (k < nd, k < na) {
                                (true, true) => Row::Pair {
                                    f: fu,
                                    h: hu,
                                    del: (del + k) as u32,
                                    add: (add + k) as u32,
                                },
                                (true, false) => Row::Line {
                                    f: fu,
                                    h: hu,
                                    i: (del + k) as u32,
                                },
                                _ => Row::Line {
                                    f: fu,
                                    h: hu,
                                    i: (add + k) as u32,
                                },
                            });
                        }
                        continue;
                    }
                    rows.push(Row::Line {
                        f: fu,
                        h: hu,
                        i: i as u32,
                    });
                    i += 1;
                }
            }
        }
        self.keys = rows.iter().map(|&r| self.key_of(r)).collect();
        self.row_of.clear();
        for (i, &r) in rows.iter().enumerate() {
            for side in [Side::Old, Side::New] {
                if let Some(n) = self.row_line(r, side) {
                    self.row_of.insert((r.file() as u32, side, n), i as u32);
                }
            }
        }
        self.rows = rows;
    }

    /// A row's key: the same for an unchanged or removed line in both modes, so switching
    /// moves it rather than replacing it.
    fn key_of(&self, r: Row) -> String {
        match r {
            Row::File(f) => format!("df{f}"),
            Row::Note(f) => format!("df{f}.note"),
            Row::Gap { f, gap } => format!("df{f}.g{gap}"),
            Row::Ctx { f, old, .. } => format!("df{f}.c{old}"),
            Row::Line { f, h, i } => {
                let l = &self.files[f as usize].hunks[h as usize].lines[i as usize];
                match l.kind {
                    LineKind::Context => format!("df{f}.c{}", l.old.unwrap_or(0)),
                    LineKind::Removed => format!("df{f}.o{}", l.old.unwrap_or(0)),
                    LineKind::Added => format!("df{f}.n{}", l.new.unwrap_or(0)),
                }
            }
            Row::Pair { f, h, del, .. } => format!(
                "df{f}.o{}",
                self.files[f as usize].hunks[h as usize].lines[del as usize]
                    .old
                    .unwrap_or(0)
            ),
        }
    }
}

/// The unchanged ranges before each hunk of `f` and after its last.
fn gaps(f: &FileDiff) -> Vec<Gap> {
    let mut out = Vec::with_capacity(f.hunks.len() + 1);
    let (mut old, mut new) = (1, 1);
    for h in &f.hunks {
        let first = h.first(Side::Old);
        out.push(Gap {
            old,
            new,
            len: first.saturating_sub(old),
        });
        old = h.end(Side::Old);
        new = h.end(Side::New);
    }
    let len = f
        .old_len()
        .map(|n| (n + 1).saturating_sub(old))
        .unwrap_or(0);
    out.push(Gap {
        old,
        new,
        len: if f.hunks.is_empty() { 0 } else { len },
    });
    out
}

/// The widest line of `f`, in columns with tabs stopping every four.
fn columns_of(f: &FileDiff) -> u32 {
    let hunks = f
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| super::model::columns(&l.text));
    let texts = [Side::Old, Side::New].into_iter().flat_map(|side| {
        f.text(side).map(|text| {
            let starts = line_starts(text);
            (0..super::model::line_count(text))
                .filter_map(move |n| {
                    let r = super::model::line_range(text, &starts, n);
                    text.get(r).map(super::model::columns)
                })
                .collect::<Vec<u32>>()
        })
    });
    hunks.chain(texts.flatten()).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::diff::FileDiff;

    fn numbered(n: u32) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    fn sample() -> DiffState {
        let old = numbered(100);
        let new = old.replace("line 50\n", "line fifty\n");
        DiffState::new(vec![FileDiff::from_texts("a.txt", &old, &new)])
    }

    fn shown(s: &DiffState) -> Vec<String> {
        s.rows
            .iter()
            .map(|&r| match r {
                Row::File(_) => "file".into(),
                Row::Note(_) => "note".into(),
                Row::Gap { gap, .. } => format!("gap{gap}:{}", s.hidden(0, gap as usize)),
                Row::Ctx { old, new, .. } => format!("{old}={new}"),
                Row::Line { .. } | Row::Pair { .. } => {
                    let (o, n) = (s.row_line(r, Side::Old), s.row_line(r, Side::New));
                    format!(
                        "{}|{}",
                        o.map_or("-".into(), |v| v.to_string()),
                        n.map_or("-".into(), |v| v.to_string())
                    )
                }
            })
            .collect()
    }

    #[test]
    fn collapsed_ranges_expand_by_twenty_and_all() {
        let mut s = sample();
        assert_eq!(shown(&s)[..3], ["file", "gap0:46", "47|47"]);
        assert_eq!(s.hidden(0, 1), 47);
        s.expand(0, 0, Expand::Up);
        assert_eq!(s.hidden(0, 0), 26);
        let rows = shown(&s);
        assert_eq!(rows[1..4], ["gap0:26", "27=27", "28=28"]);
        s.expand(0, 1, Expand::Down);
        let rows = shown(&s);
        let after = rows.iter().position(|r| r == "53|53").unwrap();
        assert_eq!(rows[after + 1..after + 3], ["54=54", "55=55"]);
        assert_eq!(rows.last().unwrap(), "gap1:27");
        s.expand(0, 1, Expand::All);
        assert_eq!(s.hidden(0, 1), 0);
        assert_eq!(shown(&s).last().unwrap(), "100=100");
        s.expand(0, 0, Expand::All);
        assert_eq!(shown(&s).len(), 1 + 101);
        assert_eq!(s.side_line(0, Side::Old, 10), Some("line 10"));
        assert_eq!(s.side_line(0, Side::New, 50), Some("line fifty"));
    }

    #[test]
    fn split_pairs_changes_and_keeps_keys() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nC\nX\nd\n";
        let mut s = DiffState::new(vec![FileDiff::from_texts("x", old, new)]);
        let unified = shown(&s);
        assert_eq!(
            unified,
            ["file", "1|1", "2|-", "3|-", "-|2", "-|3", "-|4", "4|5"]
        );
        let key_b = s.keys[2].clone();
        s.set_mode(DiffMode::Split);
        assert_eq!(shown(&s), ["file", "1|1", "2|2", "3|3", "-|4", "4|5"]);
        assert_eq!(s.keys[2], key_b);
    }

    #[test]
    fn line_selection_copies_one_side() {
        let old = "keep\nold one\nold two\nend\n";
        let new = "keep\nnew one\nnew two\nend\n";
        let mut s = DiffState::new(vec![FileDiff::from_texts("x", old, new)]);
        s.press_line(0, LineRef::new(Side::New, 1));
        s.release_line();
        s.shift = true;
        s.press_line(0, LineRef::new(Side::New, 4));
        s.release_line();
        assert_eq!(
            s.selected_text().as_deref(),
            Some("keep\nnew one\nnew two\nend")
        );
        s.set_selection(Some(LineRange {
            file: 0,
            start: LineRef::new(Side::Old, 3),
            end: LineRef::new(Side::Old, 2),
        }));
        assert_eq!(s.selected_text().as_deref(), Some("old one\nold two"));
        // Pressing the one selected line clears it, unless the press drags.
        s.shift = false;
        let one = LineRange::line(0, LineRef::new(Side::New, 1));
        s.press_line(0, one.start);
        s.release_line();
        s.press_line(0, one.start);
        assert_eq!(s.release_line(), None);
        s.press_line(0, one.start);
        s.release_line();
        s.press_line(0, one.start);
        s.drag_to(0, LineRef::new(Side::New, 2));
        assert_eq!(
            s.release_line(),
            Some(LineRange {
                end: LineRef::new(Side::New, 2),
                ..one
            })
        );
    }

    #[test]
    fn cancelling_a_gutter_drag_restores_the_selection_before_the_press() {
        let mut s = sample();
        for original in [None, Some(LineRange::line(0, LineRef::new(Side::Old, 2)))] {
            s.set_selection(original);
            s.press_line(0, LineRef::new(Side::New, 1));
            s.drag_to(0, LineRef::new(Side::New, 3));
            s.cancel_line();
            assert_eq!(s.selection(), original);
            assert!(!s.selecting && !s.unselect);
        }
    }

    #[test]
    fn expanding_everything_and_folding_back_returns_the_same_rows() {
        let mut s = sample();
        let collapsed = shown(&s);
        s.expand_all();
        assert_eq!(s.hidden(0, 0), 0);
        assert_eq!(s.hidden(0, 1), 0);
        s.collapse_all();
        assert_eq!(shown(&s), collapsed);
    }

    #[test]
    fn hunks_navigate_in_order() {
        let old: String = (1..=100).map(|i| format!("{i}\n")).collect();
        let new = old
            .replace("\n50\n", "\nfifty\n")
            .replace("\n80\n", "\neighty\n");
        let s = DiffState::new(vec![FileDiff::from_texts("n.txt", &old, &new)]);
        assert_eq!(s.hunk_count(), 2);
        let first = s.first_hunk_row().unwrap();
        let second = s.hunk_row(1).unwrap();
        assert!(second > first);
        // From above everything, the first hunk is next; from it, the second.
        assert_eq!(s.next_hunk_row(0, true), Some(first));
        assert_eq!(s.next_hunk_row(first, true), Some(second));
        assert_eq!(s.next_hunk_row(first, false), None);
        assert_eq!(s.next_hunk_row(second, false), Some(first));
    }

    #[test]
    fn rows_map_back_to_their_lines() {
        let mut s = sample();
        s.expand(0, 0, Expand::All);
        let row = s.row_of_line(0, LineRef::new(Side::New, 10)).unwrap();
        assert_eq!(s.row_line(s.rows[row as usize], Side::New), Some(10));
        assert_eq!(s.row_of_key("df0.c10"), Some(row as usize));
        // A folded line has no row.
        let s = sample();
        assert!(s.row_of_line(0, LineRef::new(Side::New, 10)).is_none());
    }

    #[test]
    fn single_side_files_use_one_column_in_split() {
        let f = FileDiff::between(None, Some("new.rs".into()), "", "a\nb\n");
        let mut s = DiffState::new(vec![f]);
        s.set_mode(DiffMode::Split);
        assert_eq!(s.single_column(0), Some(Side::New));
        // Every row is on the new side only.
        for &r in &s.rows {
            if matches!(r, Row::Line { .. } | Row::Pair { .. }) {
                assert_eq!(s.row_line(r, Side::Old), None);
            }
        }
    }
}
