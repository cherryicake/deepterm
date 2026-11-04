/// WrapMap: Soft-wrapping layer (Buffer → Wrap rows).
///
/// This module wraps the existing TextWrapper and provides:
/// - Soft-wrapped row bookkeeping（行数、行首偏移）
/// - Efficient buffer_line → wrap_row queries via prefix sum cache
/// - Incremental updates when text or layout changes
use std::ops::Range;

use gpui::{App, Font, Pixels};
use ropey::Rope;

use super::fold_map::FoldMap;
use super::text_wrapper::{LineItem, TextWrapper};

/// WrapMap manages soft-wrapping and provides buffer ↔ wrap coordinate mapping.
pub struct WrapMap {
    /// The underlying text wrapper (reuses existing implementation)
    wrapper: TextWrapper,

    /// Prefix sum cache: buffer_line_starts[line] = first wrap_row for buffer line `line`
    /// This allows O(1) lookup of buffer_line → wrap_row
    buffer_line_starts: Vec<usize>,

    /// Cached line count from last rebuild
    cached_line_count: usize,

    /// Cached total wrap row count from last rebuild.
    /// Used together with `cached_line_count` to detect if the cache is stale.
    /// When soft wrap changes a line's wrap count without changing buffer line count,
    /// this catches the staleness.
    cached_wrap_row_count: usize,
}

impl WrapMap {
    pub fn new(font: Font, font_size: Pixels, wrap_width: Option<Pixels>) -> Self {
        Self {
            wrapper: TextWrapper::new(font, font_size, wrap_width),
            buffer_line_starts: Vec::new(),
            cached_line_count: 0,
            cached_wrap_row_count: 0,
        }
    }

    /// Get total number of wrap rows (visual rows after soft-wrapping)
    #[inline]
    pub fn wrap_row_count(&self) -> usize {
        self.wrapper.len()
    }

    /// Get total number of buffer lines (logical lines)
    #[inline]
    pub fn buffer_line_count(&self) -> usize {
        self.wrapper.lines.len()
    }

    /// Get the first wrap row for a given buffer line
    pub fn buffer_line_to_first_wrap_row(&self, line: usize) -> usize {
        if line >= self.buffer_line_starts.len() {
            return self.wrap_row_count();
        }
        self.buffer_line_starts[line]
    }

    /// Get the wrap row range for a buffer line: [start, end)
    pub fn buffer_line_to_wrap_row_range(&self, line: usize) -> Range<usize> {
        let start = self.buffer_line_to_first_wrap_row(line);
        let end = if line + 1 < self.buffer_line_starts.len() {
            self.buffer_line_starts[line + 1]
        } else {
            self.wrap_row_count()
        };
        start..end
    }

    /// Update text (incremental or full)
    pub fn on_text_changed(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) -> usize {
        let dirty_from = self.wrapper.update(changed_text, range, new_text, cx);
        self.rebuild_cache_from(dirty_from);
        dirty_from
    }

    /// Update layout parameters (wrap width or font).
    ///
    /// 返回是否真的变了。宽度没变时直接返回 —— `rebuild_cache` 是 O(总行数)
    /// 的全量重建，而 `set_input_bounds` 每帧都会调用到这里。
    pub fn on_layout_changed(&mut self, wrap_width: Option<Pixels>, cx: &mut App) -> bool {
        if self.wrapper.wrap_width() == wrap_width {
            return false;
        }
        self.wrapper.set_wrap_width(wrap_width, cx);
        self.rebuild_cache();
        true
    }

    /// Set font parameters. Returns whether the measurement actually changed;
    /// callers use this to skip the O(lines) fold-projection rebuild on the
    /// (common) steady-state frame where the font is unchanged.
    pub fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) -> bool {
        let changed = self.wrapper.set_font(font, font_size, cx);
        if changed {
            self.rebuild_cache();
        }
        changed
    }

    /// Ensure text is prepared (initializes wrapper if needed)
    pub fn ensure_text_prepared(&mut self, text: &Rope, cx: &mut App) -> bool {
        let did_initialize = self.wrapper.prepare_if_need(text, cx);
        if did_initialize {
            self.rebuild_cache();
        }
        did_initialize
    }

    /// Initialize with text
    pub fn set_text(&mut self, text: &Rope, cx: &mut App) {
        // 必须先让 `update_all` 看到旧文本：它用旧长度作为全量替换 range，
        // 预先写入新文本会让你以为旧尾部已经覆盖，实际留下陈旧行。
        self.wrapper.update_all(text, cx);
        self.rebuild_cache();
    }

    /// Rebuild the prefix sum cache: buffer_line_starts.
    ///
    /// Full rebuild from row 0. Used when every line may have changed (text
    /// replaced wholesale, font / wrap-width / first-prepare changes).
    fn rebuild_cache(&mut self) {
        self.rebuild_cache_from(0);
    }

    /// Rebuild the prefix sum cache from buffer row `from` onward, leaving
    /// `buffer_line_starts[0..from]` untouched.
    ///
    /// `buffer_line_starts[i]` is the wrap row where buffer line `i` starts
    /// (= the prefix sum of each earlier line's wrap count). When an edit
    /// replaces lines starting at `from`, every line before `from` keeps its
    /// content, identity and wrap count, so its prefix entry is unchanged.
    /// Only the tail needs rewriting — strictly less work than the old
    /// all-lines rebuild, and identical output (preserves correctness).
    ///
    /// `from` is the dirty start row reported by the wrapper's incremental
    /// update. It is clamped into range and never below 0; `from == 0` falls
    /// back to a full rebuild (the common case before this change).
    fn rebuild_cache_from(&mut self, from: usize) {
        let line_count = self.wrapper.lines.len();
        let wrap_row_count = self.wrapper.len();

        // Skip if nothing changed: both buffer line count and total wrap row count must match.
        // Checking wrap_row_count is essential because soft-wrap can change the number of
        // wrap rows per line without changing the buffer line count.
        if line_count == self.cached_line_count
            && wrap_row_count == self.cached_wrap_row_count
            && !self.buffer_line_starts.is_empty()
        {
            return;
        }

        let from = from.min(line_count);

        // Empty document: no rows exist, prefix is empty.
        if line_count == 0 {
            self.buffer_line_starts.clear();
            self.cached_line_count = line_count;
            self.cached_wrap_row_count = wrap_row_count;
            return;
        }

        // Keep the clean prefix, resize to the new line count.
        if self.buffer_line_starts.len() != line_count {
            self.buffer_line_starts.resize(line_count, 0);
        }

        // `buffer_line_starts[from]` is the prefix *before* row `from`, which
        // the edit leaves intact. Accumulate onward from it.
        let mut acc = self.buffer_line_starts[from.min(line_count.saturating_sub(1))];
        for (i, line_item) in self.wrapper.lines.iter().enumerate().skip(from) {
            self.buffer_line_starts[i] = acc;
            acc += line_item.lines_len();
        }

        self.cached_line_count = line_count;
        self.cached_wrap_row_count = wrap_row_count;
    }

    /// Get access to the underlying wrapper (for rendering/hit-testing)
    pub(crate) fn wrapper(&self) -> &TextWrapper {
        &self.wrapper
    }

    /// Get access to line items (for rendering)
    pub(crate) fn lines(&self) -> &[LineItem] {
        &self.wrapper.lines
    }

    /// Get the rope text
    pub fn text(&self) -> &Rope {
        self.wrapper.text()
    }

    /// Calculate how many wrap rows of a buffer line are visible (not folded)
    pub fn visible_wrap_row_count_for_line(&self, line: usize, fold_map: &FoldMap) -> usize {
        let wrap_range = self.buffer_line_to_wrap_row_range(line);
        wrap_range
            .filter(|&wr| fold_map.wrap_row_to_display_row(wr).is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::input::rope_ext::RopeExt as _;
    use gpui::px;

    /// Render host used only to open a window (so a real text system exists)
    /// and run the parity check. Renders nothing.
    struct ParityHost;

    impl gpui::Render for ParityHost {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    fn run_parity(cx: &mut gpui::App) {
        let font = gpui::Font::default();
        let width = px(90.);
        let mut incr = WrapMap::new(font.clone(), px(13.), Some(width));
        let mut full = WrapMap::new(font, px(13.), Some(width));

        let mut text = Rope::from(INITIAL);
        incr.set_text(&text, cx);
        full.set_text(&text, cx);

        // Deterministic edit script: mid-text single-char insert (wrap-count
        // change), a multi-line block insert (line-count change), a delete
        // across a wrapped line, a top-of-file insert (the worst case for
        // incremental — dirty row 0) and an end-of-buffer append.
        let edits: [(std::ops::Range<usize>, &str); 5] = [
            (7..7, "INSERT "),
            (14..16, "xx\nnew line yy\nzz"),
            (40..46, ""),
            (0..0, "PREFIX\n"),
            (INITIAL.len()..INITIAL.len(), "\nEnd text"),
        ];

        for (range, new) in edits {
            text.replace(range.clone(), new);
            let new_rope = Rope::from(new);
            incr.on_text_changed(&text, &range, &new_rope, cx);
            full.on_text_changed(&text, &range, &new_rope, cx);
            // Force a full rebuild on the second instance.
            full.rebuild_cache();
            assert_eq!(
                incr.buffer_line_starts, full.buffer_line_starts,
                "buffer_line_starts diverged after edit at {:?}",
                range
            );
        }
    }

    /// The incremental prefix-sum rebuild must produce the *same*
    /// `buffer_line_starts` as a full rebuild for identical edits. We drive
    /// both strategies over the same deterministic edit sequence and compare
    /// every step.
    #[gpui::test]
    fn incremental_prefix_matches_full(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::theme::init(cx));
        let _handle = cx.open_window(gpui::size(px(1200.), px(800.)), |_, cx| {
            run_parity(cx);
            ParityHost
        });
    }

    const INITIAL: &str = "The quick brown fox jumps over the lazy dog while the whole world watches.\nshort\n\nAnother fairly long line that should wrap around more than once if the width is small.\nlast";
}
