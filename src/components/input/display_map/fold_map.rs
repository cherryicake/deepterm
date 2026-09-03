/// FoldMap: Folding projection layer (Wrap rows → Display rows).
///
/// Ported from Zed's folding model (`zed/crates/editor/src/display_map/fold_map.rs`):
/// a fold is an **offset range** `[start, end)` — starting at the end of the
/// header line and ending at the closing bracket's indentation (or the last
/// non-blank row) — whose hidden text is replaced on screen by a `⋯`
/// placeholder. The text after `end` (e.g. the `}` of a function) stays
/// visible on the same display line as the header:
///
/// ```text
/// fn main() {
///     let a = 1;
/// }
/// ```
/// folded at row 0 displays as `fn main() {⋯}`.
///
/// Hidden buffer rows are `(start.row, end.row]`: every row after the header
/// up to and including the fold's end row (whose visible tail is painted on
/// the header's display row).
use super::folding::Crease;
use super::wrap_map::WrapMap;
use crate::components::input::{Point, RopeExt as _};
use ropey::Rope;

/// A fold, mirroring Zed's `Fold`: hides the buffer text in
/// `start_offset..end_offset`, replacing it with the `⋯` placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    /// Start point — always at the end of the header line.
    pub start: Point,
    /// End point — at the closing bracket's indent or the last non-blank row's end.
    pub end: Point,
    /// Byte offset of `start`.
    pub start_offset: usize,
    /// Byte offset of `end`.
    pub end_offset: usize,
}

impl Fold {
    pub fn from_crease(crease: &Crease, text: &Rope) -> Self {
        Self::from_points(crease.start, crease.end, text)
    }

    pub fn from_points(start: Point, end: Point, text: &Rope) -> Self {
        let start_offset = text.point_to_offset(start);
        let end_offset = text.point_to_offset(end);
        Self {
            start,
            end,
            start_offset,
            end_offset,
        }
    }
}

/// FoldMap projects wrap rows to display rows by hiding folded regions.
pub struct FoldMap {
    /// Mapping: display_row → wrap_row.
    /// index = display_row, value = actual wrap_row.
    visible_wrap_rows: Vec<usize>,

    /// Reverse mapping: wrap_row → display_row.
    /// index = wrap_row, value = Some(display_row) if visible, None if folded.
    wrap_row_to_display_row: Vec<Option<usize>>,

    /// Currently folded ranges, sorted by `start_offset`.
    folds: Vec<Fold>,

    /// Flag indicating if the fold projection needs rebuilding.
    needs_rebuild: bool,

    /// Cached wrap_row_count from last rebuild, used to detect WrapMap changes.
    cached_wrap_row_count: usize,
}

impl FoldMap {
    pub fn new() -> Self {
        Self {
            visible_wrap_rows: Vec::new(),
            wrap_row_to_display_row: Vec::new(),
            folds: Vec::new(),
            needs_rebuild: true,
            cached_wrap_row_count: 0,
        }
    }

    // ==================== Fold mutations (zed semantics) ====================

    /// Creates folds for the given creases.
    ///
    /// Port of Zed's `FoldMapWriter::fold`: empty ranges are ignored; folds
    /// may nest (folding an outer region does not remove inner folds, so
    /// unfolding the outer region reveals the still-folded inner one).
    pub fn fold(&mut self, new_folds: impl IntoIterator<Item = Fold>) -> bool {
        let mut changed = false;
        for fold in new_folds {
            // Ignore any empty ranges.
            if fold.start_offset >= fold.end_offset {
                continue;
            }
            // Ignore duplicates.
            if self
                .folds
                .iter()
                .any(|f| f.start_offset == fold.start_offset)
            {
                continue;
            }
            self.folds.push(fold);
            changed = true;
        }
        if changed {
            self.folds.sort_by_key(|f| f.start_offset);
            self.needs_rebuild = true;
        }
        changed
    }

    /// Removes any folds whose ranges intersect any of the given ranges.
    ///
    /// Port of Zed's `FoldMapWriter::unfold_intersecting`: with `inclusive`,
    /// folds merely *touching* a range boundary are removed as well.
    pub fn unfold_intersecting(
        &mut self,
        ranges: &[std::ops::Range<usize>],
        inclusive: bool,
    ) -> bool {
        let before = self.folds.len();
        self.folds.retain(|fold| {
            !ranges.iter().any(|range| {
                if inclusive {
                    fold.start_offset <= range.end && fold.end_offset >= range.start
                } else {
                    fold.start_offset < range.end && fold.end_offset > range.start
                }
            })
        });
        let changed = self.folds.len() != before;
        if changed {
            self.needs_rebuild = true;
        }
        changed
    }

    /// Clear all folds.
    pub fn clear_folds(&mut self) {
        if !self.folds.is_empty() {
            self.folds.clear();
            self.needs_rebuild = true;
        }
    }

    // ==================== Queries ====================

    /// All folds, sorted by start offset.
    #[inline]
    pub fn folds(&self) -> &[Fold] {
        &self.folds
    }

    /// Whether a fold starts on the given buffer row
    /// (port of Zed's `FoldSnapshot::is_line_folded`).
    pub fn is_line_folded(&self, buffer_row: usize) -> bool {
        self.folds.iter().any(|f| f.start.row == buffer_row)
    }

    /// The outermost fold starting on the given buffer row, if any.
    /// Used to render the `⋯` placeholder and its trailing text.
    pub fn fold_for_row(&self, buffer_row: usize) -> Option<&Fold> {
        self.folds
            .iter()
            .filter(|f| f.start.row == buffer_row)
            .max_by_key(|f| f.end_offset)
    }

    /// Folds intersecting the given byte range.
    pub fn folds_in_range(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = &Fold> {
        self.folds
            .iter()
            .filter(move |f| f.start_offset < range.end && f.end_offset > range.start)
    }

    /// Whether the offset lies inside a fold's hidden range (exclusive of the
    /// fold boundaries themselves).
    pub fn intersects_fold(&self, offset: usize) -> bool {
        self.folds
            .iter()
            .any(|f| offset > f.start_offset && offset < f.end_offset)
    }

    /// The fold containing `offset` in its hidden range, if any.
    pub fn fold_containing(&self, offset: usize) -> Option<&Fold> {
        self.folds
            .iter()
            .find(|f| offset > f.start_offset && offset < f.end_offset)
    }

    /// The fold whose hidden range ends exactly at `offset`, if any.
    pub fn fold_ending_at(&self, offset: usize) -> Option<&Fold> {
        self.folds.iter().find(|f| f.end_offset == offset)
    }

    /// The header row of the outermost fold hiding `row`, if the row is hidden.
    pub fn fold_header_for_row(&self, row: usize) -> Option<usize> {
        self.folds
            .iter()
            .filter(|f| row > f.start.row && row <= f.end.row)
            .map(|f| f.start.row)
            .min()
    }

    /// Whether the buffer row is hidden by any fold
    /// (row ∈ (start.row, end.row] for some fold).
    pub fn is_row_hidden(&self, row: usize) -> bool {
        self.folds
            .iter()
            .any(|f| row > f.start.row && row <= f.end.row)
    }

    // ==================== Projection ====================

    /// Get total number of visible display rows.
    pub fn display_row_count(&self) -> usize {
        if self.folds.is_empty() {
            return self.cached_wrap_row_count;
        }
        self.visible_wrap_rows.len()
    }

    /// Convert wrap_row to display_row. Returns None if the wrap_row is hidden.
    pub fn wrap_row_to_display_row(&self, wrap_row: usize) -> Option<usize> {
        if self.folds.is_empty() {
            return if wrap_row < self.cached_wrap_row_count {
                Some(wrap_row)
            } else {
                None
            };
        }
        self.wrap_row_to_display_row
            .get(wrap_row)
            .copied()
            .flatten()
    }

    /// 严格小于 `wrap_row` 的可见 wrap 行数。
    ///
    /// 也就是「`wrap_row` 处（若被折叠，则是其后首个可见行）的 display row」——
    /// 用于把「目标行上方的累计高度」从 O(行号) 的逐行累加换成 O(log n) 查询。
    /// `visible_wrap_rows` 按升序构建，可以直接二分。
    pub fn visible_display_rows_before(&self, wrap_row: usize) -> usize {
        if self.folds.is_empty() {
            return wrap_row.min(self.cached_wrap_row_count);
        }
        self.visible_wrap_rows
            .partition_point(|&row| row < wrap_row)
    }

    /// Convert display_row to wrap_row.
    pub fn display_row_to_wrap_row(&self, display_row: usize) -> Option<usize> {
        if self.folds.is_empty() {
            return if display_row < self.cached_wrap_row_count {
                Some(display_row)
            } else {
                None
            };
        }
        self.visible_wrap_rows.get(display_row).copied()
    }

    /// Find the nearest visible display_row for a given wrap_row.
    pub fn nearest_visible_display_row(&self, wrap_row: usize) -> usize {
        if self.folds.is_empty() {
            return wrap_row.min(self.cached_wrap_row_count.saturating_sub(1));
        }

        if let Some(dr) = self.wrap_row_to_display_row(wrap_row) {
            return dr;
        }

        match self.visible_wrap_rows.binary_search(&wrap_row) {
            Ok(idx) => idx,
            Err(insert_pos) => insert_pos.saturating_sub(1),
        }
    }

    // ==================== Edits ====================

    /// Adjust folds after a text edit (before the wrap map is updated).
    ///
    /// `edit` is the replaced range in the OLD text; `new_len` is the length
    /// of the replacement. Folds are byte-offset based, so:
    /// - edits strictly inside a fold keep the fold (its end shifts),
    /// - edits overlapping a fold boundary drop the fold,
    /// - folds after the edit shift by the byte delta.
    pub fn adjust_folds_for_edit(&mut self, edit: std::ops::Range<usize>, new_len: usize) {
        if self.folds.is_empty() {
            return;
        }
        let edit = edit.start.min(edit.end)..edit.end;
        let delta = new_len as isize - edit.len() as isize;
        // 位移后仍不能越过 0；`saturating_add_signed` 会把负数夹到 0 而不是绕回。
        let shift = |offset: usize| offset.saturating_add_signed(delta);

        let mut folds = std::mem::take(&mut self.folds);
        folds.retain_mut(|fold| {
            if edit.end <= fold.start_offset {
                // Edit entirely before the fold (or a pure insertion at its
                // start): shift the whole fold.
                fold.start_offset = shift(fold.start_offset);
                fold.end_offset = shift(fold.end_offset);
            } else if edit.start >= fold.end_offset {
                // Edit entirely after the fold (or a pure insertion at its
                // end, i.e. at the start of the closing line): unchanged.
            } else if edit.start > fold.start_offset && edit.end <= fold.end_offset {
                // Edit inside the fold that leaves the closing line alone:
                // keep the fold, just move its end.
                fold.end_offset = shift(fold.end_offset);
            } else {
                // Edit touches the fold's header boundary: drop the fold.
                return false;
            }
            fold.start_offset < fold.end_offset
        });
        self.folds = folds;
        self.needs_rebuild = true;
    }

    /// Recompute each fold's row/column points from its byte offsets against
    /// the current text, dropping degenerate folds.
    pub fn refresh_points(&mut self, text: &Rope) {
        if self.folds.is_empty() {
            return;
        }
        let len = text.len();
        self.folds.retain_mut(|fold| {
            fold.start_offset = fold.start_offset.min(len);
            fold.end_offset = fold.end_offset.min(len);
            fold.start = text.offset_to_point(fold.start_offset);
            fold.end = text.offset_to_point(fold.end_offset);
            fold.start_offset < fold.end_offset && fold.end.row > fold.start.row
        });
        self.needs_rebuild = true;
    }

    // ==================== Rebuild ====================

    /// Update cached wrap_row_count without a full rebuild (identity mapping).
    pub(super) fn mark_dirty_with_wrap_count(&mut self, wrap_row_count: usize) {
        self.needs_rebuild = true;
        self.cached_wrap_row_count = wrap_row_count;
    }

    /// Rebuild the fold projection after wrap_map or fold state changes.
    pub fn rebuild(&mut self, wrap_map: &WrapMap) {
        let wrap_row_count = wrap_map.wrap_row_count();

        if !self.needs_rebuild && wrap_row_count == self.cached_wrap_row_count {
            return;
        }

        self.cached_wrap_row_count = wrap_row_count;

        self.visible_wrap_rows.clear();
        self.wrap_row_to_display_row.clear();
        self.wrap_row_to_display_row.resize(wrap_row_count, None);

        if self.folds.is_empty() {
            // Fast path: no folds, all wrap rows are visible.
            self.visible_wrap_rows.extend(0..wrap_row_count);
            for (display_row, &wrap_row) in self.visible_wrap_rows.iter().enumerate() {
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
            }
            self.needs_rebuild = false;
            return;
        }

        // Hidden wrap rows: the wrap rows of every buffer row in
        // (fold.start.row, fold.end.row] for each fold.
        let mut hidden_ranges = Vec::new();
        for fold in &self.folds {
            let hide_start_line = fold.start.row + 1;
            let hide_end_line = fold.end.row;
            if hide_start_line > hide_end_line {
                continue;
            }
            let start_wrap_row = wrap_map.buffer_line_to_first_wrap_row(hide_start_line);
            let end_wrap_row = if hide_end_line + 1 < wrap_map.buffer_line_count() {
                wrap_map.buffer_line_to_first_wrap_row(hide_end_line + 1)
            } else {
                wrap_row_count
            };
            if start_wrap_row < end_wrap_row {
                hidden_ranges.push(start_wrap_row..end_wrap_row);
            }
        }

        // Merge overlapping hidden ranges.
        hidden_ranges.sort_by_key(|r| r.start);
        let mut merged_hidden: Vec<usize> = Vec::new();
        for range in hidden_ranges {
            if let Some(last) = merged_hidden.last_mut() {
                if range.start <= *last {
                    *last = (*last).max(range.end);
                } else {
                    merged_hidden.push(range.start);
                    merged_hidden.push(range.end);
                }
            } else {
                merged_hidden.push(range.start);
                merged_hidden.push(range.end);
            }
        }

        // Scan all wrap rows and filter out hidden ones.
        let mut display_row = 0;
        let mut hidden_iter = merged_hidden.chunks_exact(2);
        let mut current_hidden = hidden_iter.next();

        for wrap_row in 0..wrap_row_count {
            let is_hidden = if let Some(&[start, end]) = current_hidden {
                if wrap_row >= end {
                    current_hidden = hidden_iter.next();
                    if let Some(&[new_start, new_end]) = current_hidden {
                        wrap_row >= new_start && wrap_row < new_end
                    } else {
                        false
                    }
                } else {
                    wrap_row >= start && wrap_row < end
                }
            } else {
                false
            };

            if !is_hidden {
                self.visible_wrap_rows.push(wrap_row);
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
                display_row += 1;
            }
        }

        self.needs_rebuild = false;
    }

    /// Test helper: rebuild assuming one wrap row per buffer line.
    #[cfg(test)]
    fn rebuild_identity(&mut self, line_count: usize) {
        self.needs_rebuild = true;
        self.cached_wrap_row_count = line_count;
        self.visible_wrap_rows.clear();
        self.wrap_row_to_display_row.clear();
        self.wrap_row_to_display_row.resize(line_count, None);

        let mut hidden = vec![false; line_count];
        for fold in &self.folds {
            let start = fold.start.row.saturating_add(1);
            if start >= line_count || start > fold.end.row {
                continue;
            }
            let end = fold.end.row.min(line_count.saturating_sub(1));
            for line in start..=end {
                hidden[line] = true;
            }
        }

        let mut display_row = 0;
        for wrap_row in 0..line_count {
            if !hidden[wrap_row] {
                self.visible_wrap_rows.push(wrap_row);
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
                display_row += 1;
            }
        }
        self.needs_rebuild = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(text: &Rope, start_row: usize) -> Option<Fold> {
        crate::components::input::display_map::folding::crease_for_buffer_row(
            text,
            start_row,
            |_| false,
        )
        .map(|c| Fold::from_crease(&c, text))
    }

    #[test]
    fn hides_rows_after_header_through_end_row() {
        // "fn main() {\n    a();\n}\nnext\n" (5 lines, last empty)
        let text = Rope::from("fn main() {\n    a();\n}\nnext\n");
        let mut map = FoldMap::new();
        map.fold([fold(&text, 0).unwrap()]);
        map.rebuild_identity(5);

        // Header row 0 visible; rows 1-2 hidden (row 2's `}` tail shows on row 0).
        assert_eq!(map.wrap_row_to_display_row(0), Some(0));
        assert_eq!(map.wrap_row_to_display_row(1), None);
        assert_eq!(map.wrap_row_to_display_row(2), None);
        assert_eq!(map.wrap_row_to_display_row(3), Some(1));
        assert_eq!(map.wrap_row_to_display_row(4), Some(2));
        assert_eq!(map.display_row_count(), 3);
    }

    #[test]
    fn nested_folds_coexist() {
        // fn main() {
        //     if true {
        //         a();
        //     }
        // }
        let text = Rope::from("fn main() {\n    if true {\n        a();\n    }\n}\n");
        let mut map = FoldMap::new();
        let outer = fold(&text, 0).unwrap();
        let inner = fold(&text, 1).unwrap();
        map.fold([inner]);
        assert!(map.is_line_folded(1));
        assert!(!map.is_line_folded(0));

        // Folding the outer keeps the inner fold in storage (zed semantics).
        map.fold([outer]);
        assert!(map.is_line_folded(0));
        assert!(map.is_line_folded(1));
        map.rebuild_identity(5);
        // Only header row 0 and trailing row 5 (if any) visible.
        assert_eq!(map.wrap_row_to_display_row(0), Some(0));
        for row in 1..=4 {
            assert_eq!(map.wrap_row_to_display_row(row), None);
        }

        // Unfolding at the outer header's row (zed's unfold_at: the row's full
        // line range) reveals the still-folded inner.
        map.unfold_intersecting(&[0..11], true);
        assert!(!map.is_line_folded(0));
        assert!(map.is_line_folded(1));
        map.rebuild_identity(5);
        assert_eq!(map.wrap_row_to_display_row(0), Some(0));
        assert_eq!(map.wrap_row_to_display_row(1), Some(1));
        assert_eq!(map.wrap_row_to_display_row(2), None);
        assert_eq!(map.wrap_row_to_display_row(3), None);
        assert_eq!(map.wrap_row_to_display_row(4), Some(2));
    }

    #[test]
    fn unfold_intersecting_variants() {
        let text = Rope::from("fn main() {\n    a();\n}\nnext\n");
        let mut map = FoldMap::new();
        let f = fold(&text, 0).unwrap();
        map.fold([f]);

        // Touching the end boundary only removes with inclusive=true.
        map.unfold_intersecting(&[f.end_offset..f.end_offset + 1], false);
        assert!(map.is_line_folded(0));
        map.unfold_intersecting(&[f.end_offset..f.end_offset + 1], true);
        assert!(!map.is_line_folded(0));
    }

    #[test]
    fn fold_for_row_returns_outermost() {
        let text = Rope::from("a {\n  b {\n    c;\n  }\n}\n");
        let mut map = FoldMap::new();
        let outer = fold(&text, 0).unwrap();
        let inner = fold(&text, 1).unwrap();
        map.fold([inner, outer]);
        assert_eq!(map.fold_for_row(0), Some(&outer));
        assert_eq!(map.fold_for_row(1), Some(&inner));
    }

    #[test]
    fn adjust_folds_keeps_internal_edits_and_drops_boundary_edits() {
        let text = Rope::from("fn main() {\n    let a = 1;\n    let b = 2;\n}\nnext\n");
        let mut map = FoldMap::new();
        let f = fold(&text, 0).unwrap();
        map.fold([f]);

        // Edit strictly inside the fold (e.g. change "let a = 1" → "let x"):
        // offset of 'a' is 15; replace 1 byte with 3 bytes.
        map.adjust_folds_for_edit(15..16, 3);
        assert_eq!(map.folds().len(), 1);
        assert_eq!(map.folds()[0].end_offset, f.end_offset + 2);

        // Edit overlapping the fold start (join header line with next line):
        // delete the newline at fold.start_offset.
        map.adjust_folds_for_edit(f.start_offset..f.start_offset + 1, 0);
        assert!(map.folds().is_empty());
    }

    /// 纯插入落在折叠边界上时不应该丢折叠。
    ///
    /// 折叠区间是 `start..end`（`end` 为闭合行的行首），两侧边界上的纯插入都应保留折叠：
    /// - 插在 `start_offset`：折叠整体后移（头行变长，内容跟着走）；
    /// - 插在 `end_offset`：插到闭合行行首，折叠区间本身不动。
    #[test]
    fn adjust_folds_keeps_pure_insertion_at_boundaries() {
        // "fn main() {\n    a();\n}\nnext\n"
        // fold = 12..22（12 = 头行行尾，22 = `}` 行行首）
        let text = Rope::from("fn main() {\n    a();\n}\nnext\n");
        let mut map = FoldMap::new();
        let f = fold(&text, 0).unwrap();
        map.fold([f]);
        let (start, end) = (f.start_offset, f.end_offset);

        // 在 end_offset 处纯插入 3 字节：插到闭合行行首，折叠区间不动。
        map.adjust_folds_for_edit(end..end, 3);
        assert_eq!(map.folds().len(), 1, "end_offset 处纯插入不应丢弃折叠");
        assert_eq!(map.folds()[0].start_offset, start);
        assert_eq!(map.folds()[0].end_offset, end);

        // 在 start_offset 处纯插入 3 字节：头行变长，折叠整体右移。
        map.adjust_folds_for_edit(start..start, 3);
        assert_eq!(map.folds().len(), 1, "start_offset 处纯插入不应丢弃折叠");
        assert_eq!(map.folds()[0].start_offset, start + 3);
        assert_eq!(map.folds()[0].end_offset, end + 3);
    }

    /// 替换区间**恰好止于** `end_offset`（闭合行未被触碰）时应保留折叠。
    #[test]
    fn adjust_folds_keeps_edit_ending_at_fold_end() {
        // "fn main() {\n    a();\n}\nnext\n" → fold = 11..21
        let text = Rope::from("fn main() {\n    a();\n}\nnext\n");
        let mut map = FoldMap::new();
        let f = fold(&text, 0).unwrap();
        map.fold([f]);
        let end = f.end_offset;

        // 替换折叠内最后一行的内容（17..21 → 2 字节），闭合行未动。
        map.adjust_folds_for_edit(end - 4..end, 2);
        assert_eq!(map.folds().len(), 1, "闭合行未参与编辑时应保留折叠");
        assert_eq!(map.folds()[0].end_offset, end - 2);
    }

    /// `delta` 为大负数时不能下溢（debug 下 panic）或产生反转区间。
    #[test]
    fn adjust_folds_survives_huge_deletion() {
        let text = Rope::from("fn main() {\n    a();\n}\nnext\n");
        let mut map = FoldMap::new();
        let f = fold(&text, 0).unwrap();
        map.fold([f]);

        // 折叠之后的整段被删除（delta = -100）。
        map.adjust_folds_for_edit(30..130, 0);
        // 折叠整体在编辑点之前，不受影响。
        assert_eq!(map.folds().len(), 1);
        assert_eq!(map.folds()[0].start_offset, f.start_offset);

        // 折叠之前的整段被删除（delta 为大负数）→ 折叠退化被丢弃，且不能 panic。
        map.adjust_folds_for_edit(0..200, 0);
        assert!(map.folds().is_empty() || map.folds()[0].start_offset <= map.folds()[0].end_offset);
    }

    #[test]
    fn adjust_folds_shifts_following_folds() {
        // Two functions:
        let text = Rope::from("fn a() {\n    a();\n}\n\nfn b() {\n    b();\n}\n");
        let mut map = FoldMap::new();
        let f1 = fold(&text, 0).unwrap();
        let f2 = fold(&text, 4).unwrap();
        map.fold([f1, f2]);

        // Insert 4 bytes before the second function (at end of line 3).
        map.adjust_folds_for_edit(17..17, 4);
        assert_eq!(map.folds()[1].start_offset, f2.start_offset + 4);
    }

    /// `visible_display_rows_before` 是滚动定位用的 O(log n) 查询：它必须与
    /// 「逐行累加可见行数」完全一致，否则光标在折叠区之后会滚错位置。
    #[test]
    fn visible_display_rows_before_matches_naive_sum() {
        let naive = |map: &FoldMap, wrap_row: usize| -> usize {
            (0..wrap_row)
                .filter(|&r| map.wrap_row_to_display_row(r).is_some())
                .count()
        };

        // 无折叠：identity 映射。
        let mut map = FoldMap::new();
        map.rebuild_identity(10);
        for wr in 0..=12 {
            assert_eq!(
                map.visible_display_rows_before(wr),
                naive(&map, wr),
                "wr={wr}"
            );
        }

        // 有折叠："fn main() {\n    a();\n}\nnext\n" → 行 1、2 被隐藏，
        // 可见 wrap 行 = [0, 3, 4]。
        let text = Rope::from("fn main() {\n    a();\n}\nnext\n");
        let mut map = FoldMap::new();
        map.fold([fold(&text, 0).unwrap()]);
        map.rebuild_identity(5);
        for wr in 0..=7 {
            assert_eq!(
                map.visible_display_rows_before(wr),
                naive(&map, wr),
                "wr={wr}"
            );
        }
        assert_eq!(map.visible_display_rows_before(1), 1);
        assert_eq!(map.visible_display_rows_before(3), 1);
        assert_eq!(map.visible_display_rows_before(4), 2);
    }

    #[test]
    fn refresh_points_recomputes_rows() {
        let text = Rope::from("fn main() {\n    a();\n}\nnext\n");
        let mut map = FoldMap::new();
        let f = fold(&text, 0).unwrap();
        map.fold([f]);

        // Simulate the fold offsets pointing at a different header in a new text.
        let new_text = Rope::from("fn main() {\n    a();\n}\nnext\nfn other() {\n}\n");
        let mut moved = f;
        // "fn other() {" ends at offset 40 (row 4); "}" is at offset 41 (row 5).
        moved.start_offset = 40;
        moved.end_offset = 41;
        map.folds = vec![moved];
        map.refresh_points(&new_text);
        let f = &map.folds()[0];
        assert_eq!(f.start.row, 4);
        assert_eq!(f.end.row, 5);
        assert_eq!(f.end.column, 0);
    }
}
