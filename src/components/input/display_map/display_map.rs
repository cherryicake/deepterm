/// DisplayMap: Public facade for Editor/Input display mapping.
///
/// This combines WrapMap and FoldMap to provide a unified API:
/// - BufferPoint ↔ DisplayPoint conversion
/// - Fold management (fold/unfold/query) with Zed's offset-based fold model
/// - Automatic projection updates on text/layout changes
use std::ops::Range;

use gpui::{App, Font, Pixels};
use ropey::Rope;

use super::fold_map::{Fold, FoldMap};
use super::folding::Crease;
use super::text_wrapper::{LineItem, WrapDisplayPoint};
use super::wrap_map::WrapMap;
use crate::components::input::Point as TreeSitterPoint;
use crate::components::input::rope_ext::RopeExt as _;

/// git hunk 展开区：插在 `before_buffer_row` 之前的只读虚拟行
/// （被删除的旧行，红底展示）。不进入 buffer，也不参与光标/选区映射，
/// 只影响显示行数与 y 几何。
#[derive(Clone, Debug)]
pub struct GitInsertion {
    pub before_buffer_row: usize,
    /// 对应 `InputState::git_hunks` 的下标（点击/回写用）。
    pub hunk_ix: usize,
    pub lines: std::rc::Rc<Vec<GitInsertedLine>>,
}

/// 插入区中的一行。
#[derive(Clone, Debug)]
pub struct GitInsertedLine {
    /// 旧文件行号（1 起）；None 则不显示行号。
    pub old_lineno: Option<u32>,
    pub text: std::rc::Rc<str>,
}

/// DisplayMap is the main interface for Editor/Input coordinate mapping.
///
/// It manages the two-layer projection:
/// 1. Buffer → Wrap (soft-wrapping)
/// 2. Wrap → Display (folding)
///
/// Editor/Input only needs to work with BufferPoint and DisplayPoint.
pub struct DisplayMap {
    wrap_map: WrapMap,
    fold_map: FoldMap,
    /// git hunk 展开区（按 `before_buffer_row` 升序）。
    git_insertions: Vec<GitInsertion>,
}

impl DisplayMap {
    pub fn new(font: Font, font_size: Pixels, wrap_width: Option<Pixels>) -> Self {
        Self {
            wrap_map: WrapMap::new(font, font_size, wrap_width),
            fold_map: FoldMap::new(),
            git_insertions: Vec::new(),
        }
    }

    // ==================== Core Coordinate Mapping ====================

    /// Get total number of visible display rows
    ///
    /// 注意：不含 git 展开区的虚拟行——它们不属于光标/行映射空间
    /// （`move_vertical` 以此为行上限），只增加视觉高度。滚动高度用
    /// [`Self::display_row_count_with_insertions`]。
    #[inline]
    pub fn display_row_count(&self) -> usize {
        self.fold_map.display_row_count()
    }

    /// 可见显示行数 + git 展开区行数（滚动内容高度用）。
    #[inline]
    pub fn display_row_count_with_insertions(&self) -> usize {
        self.display_row_count() + self.git_insertion_rows_total()
    }

    // ==================== Git hunk 展开区（插入行） ====================

    /// 替换展开区（调用方保证按 `before_buffer_row` 升序；当前实现里
    /// hunk 按旧侧行序产出，天然有序）。
    pub fn set_git_insertions(&mut self, insertions: Vec<GitInsertion>) {
        self.git_insertions = insertions;
    }

    #[inline]
    pub fn git_insertions(&self) -> &[GitInsertion] {
        &self.git_insertions
    }

    /// `line` 之前显示的插入行总数（锚点行 `before_buffer_row < line`
    /// 且锚点行未被折叠——折叠的锚点连带隐藏其展开区）。
    pub fn git_insertion_rows_before(&self, line: usize) -> usize {
        self.git_insertions
            .iter()
            .filter(|ins| {
                ins.before_buffer_row < line && !self.fold_map.is_row_hidden(ins.before_buffer_row)
            })
            .map(|ins| ins.lines.len())
            .sum()
    }

    /// 紧贴 `line` 之上显示的插入块（`before_buffer_row == line`）。
    pub fn git_insertions_at(&self, line: usize) -> impl Iterator<Item = &GitInsertion> {
        self.git_insertions.iter().filter(move |ins| {
            ins.before_buffer_row == line && !self.fold_map.is_row_hidden(ins.before_buffer_row)
        })
    }

    /// 全部展开区行数（含锚点被折叠的——折叠时由查询方按需排除）。
    fn git_insertion_rows_total(&self) -> usize {
        self.git_insertions
            .iter()
            .filter(|ins| !self.fold_map.is_row_hidden(ins.before_buffer_row))
            .map(|ins| ins.lines.len())
            .sum()
    }

    /// Check if a buffer line is completely hidden
    #[inline]
    pub fn is_buffer_line_hidden(&self, line: usize) -> bool {
        self.fold_map.is_row_hidden(line)
    }

    // ==================== Fold management (zed semantics) ====================

    /// Creates folds for the given creases (see [`Self::fold_ranges`]).
    pub fn fold_creases(&mut self, text: &Rope, creases: &[Crease]) -> bool {
        self.fold_ranges(
            text,
            creases.iter().map(|c| c.range(text)).collect::<Vec<_>>(),
        )
    }

    /// Creates folds for the given byte ranges.
    ///
    /// Port of Zed's `Editor::fold_ranges` / `DisplayMap::fold`: empty ranges
    /// are ignored and folds may nest.
    pub fn fold_ranges(&mut self, text: &Rope, ranges: Vec<Range<usize>>) -> bool {
        let folds = ranges
            .into_iter()
            .map(|range| {
                Fold::from_points(
                    text.offset_to_point(range.start),
                    text.offset_to_point(range.end),
                    text,
                )
            })
            .collect::<Vec<_>>();
        let changed = self.fold_map.fold(folds);
        if changed {
            self.rebuild_fold_projection();
        }
        changed
    }

    /// Folds the crease starting on the given buffer row, if any
    /// (port of Zed's `Editor::fold_at`).
    pub fn fold_at(&mut self, text: &Rope, buffer_row: usize) -> bool {
        let Some(crease) = self.crease_for_buffer_row(text, buffer_row) else {
            return false;
        };
        self.fold_creases(text, &[crease])
    }

    /// Removes any folds whose ranges intersect any of the given ranges
    /// (port of Zed's `Editor::unfold_ranges`).
    pub fn unfold_ranges(&mut self, ranges: &[Range<usize>], inclusive: bool) -> bool {
        let changed = self.fold_map.unfold_intersecting(ranges, inclusive);
        if changed {
            self.rebuild_fold_projection();
        }
        changed
    }

    /// All folds, sorted by start offset.
    #[inline]
    pub fn folds(&self) -> &[Fold] {
        self.fold_map.folds()
    }

    /// Whether a fold starts on the given buffer row
    /// (port of Zed's `DisplaySnapshot::is_line_folded`).
    #[inline]
    pub fn is_line_folded(&self, buffer_row: usize) -> bool {
        self.fold_map.is_line_folded(buffer_row)
    }

    /// The outermost fold starting on the given buffer row, if any.
    #[inline]
    pub fn fold_for_row(&self, buffer_row: usize) -> Option<&Fold> {
        self.fold_map.fold_for_row(buffer_row)
    }

    /// Folds intersecting the given byte range
    /// (port of Zed's `DisplaySnapshot::folds_in_range`).
    pub fn folds_in_range(&self, range: Range<usize>) -> impl Iterator<Item = &Fold> {
        self.fold_map.folds_in_range(range)
    }

    /// Whether the byte offset lies inside a fold's hidden range.
    #[inline]
    pub fn intersects_fold(&self, offset: usize) -> bool {
        self.fold_map.intersects_fold(offset)
    }

    /// The fold containing `offset` in its hidden range, if any.
    #[inline]
    pub fn fold_containing(&self, offset: usize) -> Option<&Fold> {
        self.fold_map.fold_containing(offset)
    }

    /// The fold whose hidden range ends exactly at `offset`, if any.
    #[inline]
    pub fn fold_ending_at(&self, offset: usize) -> Option<&Fold> {
        self.fold_map.fold_ending_at(offset)
    }

    /// The header row of the outermost fold hiding `row`, if the row is hidden.
    #[inline]
    pub fn fold_header_for_row(&self, row: usize) -> Option<usize> {
        self.fold_map.fold_header_for_row(row)
    }

    /// Clear all folds (port of Zed's `Editor::unfold_all` on the display map).
    pub fn clear_folds(&mut self) {
        self.fold_map.clear_folds();
        self.rebuild_fold_projection();
    }

    // ==================== Creases (zed semantics) ====================

    /// Whether the given buffer row "starts an indent"
    /// (port of Zed's `DisplaySnapshot::starts_indent`).
    pub fn starts_indent(&self, text: &Rope, buffer_row: usize) -> bool {
        super::folding::starts_indent(text, buffer_row)
    }

    /// Computes the foldable region that starts on `buffer_row`
    /// (port of Zed's `DisplaySnapshot::crease_for_buffer_row`).
    pub fn crease_for_buffer_row(&self, text: &Rope, buffer_row: usize) -> Option<Crease> {
        let is_line_folded = |row: usize| self.fold_map.is_line_folded(row);
        super::folding::crease_for_buffer_row(text, buffer_row, is_line_folded)
    }

    // ==================== Text and Layout Updates ====================

    /// Adjust folds for a text edit before updating the wrap map.
    ///
    /// Must be called with the edit range in the OLD text and the
    /// replacement length (see [`FoldMap::adjust_folds_for_edit`]).
    pub fn adjust_folds_for_edit(&mut self, range: &Range<usize>, new_text: &str) {
        self.fold_map
            .adjust_folds_for_edit(range.clone(), new_text.len());
    }

    /// Update text (incremental or full)
    pub fn on_text_changed(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) {
        self.wrap_map
            .on_text_changed(changed_text, range, new_text, cx);
        // Folds are byte-offset based; recompute their row/column points
        // against the new text.
        self.fold_map.refresh_points(self.wrap_map.text());
        self.rebuild_fold_projection();
    }

    /// Update layout parameters (wrap width or font).
    ///
    /// 返回是否真的变了（宽度未变时跳过全量重排与折叠投影重建）。
    pub fn on_layout_changed(&mut self, wrap_width: Option<Pixels>, cx: &mut App) -> bool {
        if !self.wrap_map.on_layout_changed(wrap_width, cx) {
            return false;
        }
        self.rebuild_fold_projection();
        true
    }

    /// Set font parameters
    pub fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) {
        if self.wrap_map.set_font(font, font_size, cx) {
            self.rebuild_fold_projection();
        }
    }

    /// Ensure text is prepared (initializes wrapper if needed)
    pub fn ensure_text_prepared(&mut self, text: &Rope, cx: &mut App) {
        let did_initialize = self.wrap_map.ensure_text_prepared(text, cx);
        if did_initialize {
            self.rebuild_fold_projection();
        }
    }

    /// Initialize with text
    pub fn set_text(&mut self, text: &Rope, cx: &mut App) {
        self.wrap_map.set_text(text, cx);
        self.rebuild_fold_projection();
    }

    // ==================== Internal Helpers ====================

    /// Rebuild fold projection after wrap_map or fold state changes.
    fn rebuild_fold_projection(&mut self) {
        if !self.fold_map.folds().is_empty() {
            self.fold_map.rebuild(&self.wrap_map);
        } else {
            // No active folds: identity mapping (wrap_row == display_row).
            self.fold_map
                .mark_dirty_with_wrap_count(self.wrap_map.wrap_row_count());
        }
    }

    // ==================== Wrap Display Point Operations ====================

    /// Convert byte offset to wrap display point (with soft wrap info).
    #[inline]
    pub(crate) fn offset_to_wrap_display_point(&self, offset: usize) -> WrapDisplayPoint {
        self.wrap_map.wrapper().offset_to_display_point(offset)
    }

    /// Convert wrap display point to byte offset.
    #[inline]
    pub(crate) fn wrap_display_point_to_offset(&self, point: WrapDisplayPoint) -> usize {
        self.wrap_map.wrapper().display_point_to_offset(point)
    }

    /// Convert wrap display point to TreeSitterPoint (buffer line/col).
    #[inline]
    pub(crate) fn wrap_display_point_to_point(&self, point: WrapDisplayPoint) -> TreeSitterPoint {
        self.wrap_map.wrapper().display_point_to_point(point)
    }

    /// Convert a wrap row to a display row (skipping folded rows).
    /// Returns None if the wrap row is folded.
    #[inline]
    pub fn wrap_row_to_display_row(&self, wrap_row: usize) -> Option<usize> {
        self.fold_map.wrap_row_to_display_row(wrap_row)
    }

    /// Find the nearest visible display row for a given wrap row.
    #[inline]
    pub fn nearest_visible_display_row(&self, wrap_row: usize) -> usize {
        self.fold_map.nearest_visible_display_row(wrap_row)
    }

    /// Convert a display row to a wrap row.
    #[inline]
    pub fn display_row_to_wrap_row(&self, display_row: usize) -> Option<usize> {
        self.fold_map.display_row_to_wrap_row(display_row)
    }

    /// Get the longest row index (by byte length).
    #[inline]
    pub(crate) fn longest_row(&self) -> usize {
        self.wrap_map.wrapper().longest_row.row
    }

    // ==================== Access Methods ====================

    /// Get access to line items (for rendering)
    #[inline]
    pub(crate) fn lines(&self) -> &[LineItem] {
        self.wrap_map.lines()
    }

    /// Get the rope text
    #[inline]
    pub fn text(&self) -> &Rope {
        self.wrap_map.text()
    }

    /// Calculate how many wrap rows of a buffer line are visible (not folded)
    #[inline]
    pub fn visible_wrap_row_count_for_buffer_line(&self, line: usize) -> usize {
        self.wrap_map
            .visible_wrap_row_count_for_line(line, &self.fold_map)
    }

    /// 目标 buffer 行之前的可见显示行数 —— 即该行首个子行所在的 display row。
    ///
    /// 等价于 `sum(visible_wrap_row_count_for_buffer_line(ix) for ix in 0..line)`，
    /// 但走 wrap_map 的前缀和与 fold_map 的可见行二分，是 O(log n)。调用点
    /// 在每次光标移动的滚动路径上，逐行累加在 10 万行文件末尾是 10 万次迭代。
    ///
    /// 含 git 展开区插入行（锚点行在 `line` 之前的块）——滚动定位的 y
    /// 必须包含这些视觉行高。
    pub fn display_row_for_buffer_line(&self, line: usize) -> usize {
        let wrap_row = self.wrap_map.buffer_line_to_first_wrap_row(line);
        self.fold_map.visible_display_rows_before(wrap_row) + self.git_insertion_rows_before(line)
    }

    /// Get the wrap row count (before folding)
    #[inline]
    pub fn wrap_row_count(&self) -> usize {
        self.wrap_map.wrap_row_count()
    }

    /// Get the buffer line count (logical lines)
    #[inline]
    pub fn buffer_line_count(&self) -> usize {
        self.wrap_map.buffer_line_count()
    }
}
