//! 内建命令缓冲覆盖层的换行 + 切段算法。
//!
//! 终端 shell 的 readline 在文本到达行末时会把后续字符折回下一行第 0 列继续；
//! DeepTerm 的内建命令缓冲（`builtin_text` + IME 预编辑 `ime_text`）需要呈现同样的语义。
//! 本模块把输入按 cell 列宽切分，输出多行视觉布局；每一行又被进一步切成多个段：
//!
//! - 宽字符（width ≥ 2）独占一段，避免 GPUI `shape_line(force_width=cell_w)` 把宽字符
//!   错误地压成 1 cell 宽（GPUI 按字符基础序号 × cell_w 强制对齐，不区分字符 cell 宽度）。
//! - builtin / IME 边界切段，方便上层用不同颜色渲染。
//!
//! 不依赖 GPUI，纯函数实现，便于单元测试 + 在 prepaint / overlay 定位两处复用。

use unicode_width::UnicodeWidthChar;

/// 视觉行内的一段，对应一次 `shape_line` 调用。
#[derive(Clone, Debug, Default)]
pub struct OverlaySegment {
    /// 段起始列（grid 列，绝对值）。
    pub start_col: usize,
    pub text: String,
    /// 该段属于 IME 预编辑文本（`true`）还是 builtin 文本（`false`）。
    pub is_ime: bool,
    /// 该段占用的 cell 数。
    pub cells: usize,
}

/// 一行视觉行内的换行 + 切段结果。
#[derive(Clone, Debug, Default)]
pub struct OverlayLine {
    /// 该视觉行起始所在的终端列（grid 列）。仅首行可能 `!= 0`，其余换行后均为 0。
    pub start_col: usize,
    /// 行内段集合，按出现顺序排列。
    pub segments: Vec<OverlaySegment>,
    /// 行内 builtin 段占用的 cell 数。
    pub builtin_cells: usize,
    /// 行内 IME 段占用的 cell 数。
    pub ime_cells: usize,
}

impl OverlayLine {
    /// 行内全部字符拼接（仅用于测试 / 调试）。
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }
}

/// 全部输入文本被拆分到多个视觉行后的结果。
#[derive(Clone, Debug)]
pub struct WrappedOverlay {
    pub lines: Vec<OverlayLine>,
    /// 光标所在的视觉行（0 = 起始行）。
    ///
    /// 光标位置与 readline 一致：落在 `builtin` 末尾；若 builtin 末尾恰好填满当前行，
    /// 则跳到下一行第 0 列（即使该行还没有任何字符）。
    pub cursor_row: usize,
    /// 光标所在的视觉列（终端 grid 列，绝对值，含 `start_col`）。
    pub cursor_col: usize,
    /// 视觉行总数 = `max(lines.len(), cursor_row + 1)`。
    pub row_count: usize,
}

/// 把 `builtin` + `ime` 折成多行视觉行布局，每行再切成段。
///
/// `cols == 0`（极端窄面板）退化为单行单段，避免死循环。
pub fn wrap_overlay_text(
    builtin: &str,
    ime: &str,
    start_col: usize,
    cols: usize,
) -> WrappedOverlay {
    if cols == 0 {
        let mut segments = Vec::new();
        if !builtin.is_empty() {
            segments.push(OverlaySegment {
                start_col,
                text: builtin.to_string(),
                is_ime: false,
                cells: 0,
            });
        }
        if !ime.is_empty() {
            segments.push(OverlaySegment {
                start_col,
                text: ime.to_string(),
                is_ime: true,
                cells: 0,
            });
        }
        return WrappedOverlay {
            lines: vec![OverlayLine {
                start_col,
                segments,
                builtin_cells: 0,
                ime_cells: 0,
            }],
            cursor_row: 0,
            cursor_col: start_col,
            row_count: 1,
        };
    }

    let start_col = start_col.min(cols);
    let mut builder = LineBuilder::new(start_col);
    let mut lines: Vec<OverlayLine> = Vec::new();
    let mut builtin_done = false;
    let mut cursor_row: usize = 0;
    let mut cursor_col: usize = start_col;

    let chars = builtin
        .chars()
        .map(|c| (false, c))
        .chain(ime.chars().map(|c| (true, c)));

    for (is_ime, ch) in chars {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);

        // builtin → IME 切换：锁定光标位置（builtin 末尾）。
        if is_ime && !builtin_done {
            cursor_row = lines.len();
            cursor_col = builder.col_cursor;
            builder.flush_open_segment();
            builtin_done = true;
        }

        // 宽度大于 0 且会越过列尾 → 把当前行落地，开新行从 0 列起。
        if w > 0 && builder.col_cursor + w > cols {
            lines.push(builder.finish());
            builder = LineBuilder::new(0);
        }

        builder.push_char(ch, w, is_ime);
    }

    if !builtin_done {
        cursor_row = lines.len();
        cursor_col = builder.col_cursor;
    }
    lines.push(builder.finish());

    // builtin 末尾恰好把行填满 → 光标跳到下一行 0 列（与 readline 行尾换行一致）。
    if cursor_col >= cols {
        cursor_row += 1;
        cursor_col = 0;
    }

    let row_count = lines.len().max(cursor_row + 1);

    WrappedOverlay {
        lines,
        cursor_row,
        cursor_col,
        row_count,
    }
}

/// 一行视觉行的逐字符累积器。负责按"宽字符独占 / IME↔builtin 切换"切段。
struct LineBuilder {
    line: OverlayLine,
    /// 当前正在累积的段（尚未落入 `line.segments`）。
    open_text: String,
    open_start_col: usize,
    open_is_ime: bool,
    open_cells: usize,
    /// 当前视觉列游标（grid 列，绝对值）。
    col_cursor: usize,
}

impl LineBuilder {
    fn new(start_col: usize) -> Self {
        Self {
            line: OverlayLine {
                start_col,
                ..Default::default()
            },
            open_text: String::new(),
            open_start_col: start_col,
            open_is_ime: false,
            open_cells: 0,
            col_cursor: start_col,
        }
    }

    fn push_char(&mut self, ch: char, w: usize, is_ime: bool) {
        let is_wide = w >= 2;

        // style 切换或遇到宽字符前先把当前段落地。
        let needs_flush_before =
            !self.open_text.is_empty() && (is_ime != self.open_is_ime || is_wide);
        if needs_flush_before {
            self.flush_open_segment();
        }

        if self.open_text.is_empty() {
            self.open_start_col = self.col_cursor;
            self.open_is_ime = is_ime;
            self.open_cells = 0;
        }
        self.open_text.push(ch);
        self.open_cells += w;
        self.col_cursor += w;

        if is_ime {
            self.line.ime_cells += w;
        } else {
            self.line.builtin_cells += w;
        }

        if is_wide {
            // 宽字符独占段，立即落地。
            self.flush_open_segment();
        }
    }

    fn flush_open_segment(&mut self) {
        if self.open_text.is_empty() {
            return;
        }
        self.line.segments.push(OverlaySegment {
            start_col: self.open_start_col,
            text: std::mem::take(&mut self.open_text),
            is_ime: self.open_is_ime,
            cells: self.open_cells,
        });
        self.open_cells = 0;
        self.open_start_col = self.col_cursor;
    }

    fn finish(mut self) -> OverlayLine {
        self.flush_open_segment();
        self.line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_segments_cover(line: &OverlayLine) {
        let mut col = line.start_col;
        let mut builtin_cells = 0;
        let mut ime_cells = 0;
        for seg in &line.segments {
            assert_eq!(seg.start_col, col, "segment start_col must be contiguous");
            col += seg.cells;
            if seg.is_ime {
                ime_cells += seg.cells;
            } else {
                builtin_cells += seg.cells;
            }
        }
        assert_eq!(builtin_cells, line.builtin_cells);
        assert_eq!(ime_cells, line.ime_cells);
    }

    #[test]
    fn wraps_to_single_line_when_within_cols() {
        let r = wrap_overlay_text("/scp foo", "", 0, 80);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text(), "/scp foo");
        assert_eq!(r.lines[0].start_col, 0);
        assert_eq!(r.lines[0].builtin_cells, 8);
        assert_eq!(r.cursor_row, 0);
        assert_eq!(r.cursor_col, 8);
        assert_segments_cover(&r.lines[0]);
    }

    #[test]
    fn wraps_when_overflow() {
        // cols=10, "/scp /tmp/abcdef" 长度 16
        let r = wrap_overlay_text("/scp /tmp/abcdef", "", 0, 10);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].text(), "/scp /tmp/");
        assert_eq!(r.lines[0].start_col, 0);
        assert_eq!(r.lines[1].text(), "abcdef");
        assert_eq!(r.lines[1].start_col, 0);
        assert_eq!(r.cursor_row, 1);
        assert_eq!(r.cursor_col, 6);
    }

    #[test]
    fn cjk_uses_two_cells_and_wraps_at_boundary() {
        // cols=4, builtin = "ab中" → "ab" 占 2 cell，"中" 占 2 cell，刚好填满 → 不换行
        let r = wrap_overlay_text("ab中", "", 0, 4);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.cursor_row, 1);
        assert_eq!(r.cursor_col, 0);

        // cols=3, "ab中" → "中" 放不下，应换行
        let r = wrap_overlay_text("ab中", "", 0, 3);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].text(), "ab");
        assert_eq!(r.lines[1].text(), "中");
        assert_eq!(r.cursor_row, 1);
        assert_eq!(r.cursor_col, 2);
    }

    #[test]
    fn zero_width_char_attached_without_advancing_column() {
        // U+200D ZWJ 宽度为 0
        let r = wrap_overlay_text("a\u{200d}b", "", 0, 4);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text(), "a\u{200d}b");
        assert_eq!(r.cursor_col, 2);
    }

    #[test]
    fn cursor_jumps_to_next_row_when_filling_cols() {
        let r = wrap_overlay_text("/scp", "", 0, 4);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.cursor_row, 1);
        assert_eq!(r.cursor_col, 0);
        assert_eq!(r.row_count, 2);
    }

    #[test]
    fn ime_after_full_line_lands_on_next_row() {
        let r = wrap_overlay_text("/scp", "中", 0, 4);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].text(), "/scp");
        assert_eq!(r.lines[0].ime_cells, 0);
        assert_eq!(r.lines[1].text(), "中");
        assert_eq!(r.lines[1].start_col, 0);
        assert_eq!(r.lines[1].builtin_cells, 0);
        assert_eq!(r.lines[1].ime_cells, 2);
        assert_eq!(r.cursor_row, 1);
        assert_eq!(r.cursor_col, 0);
    }

    #[test]
    fn ime_segment_can_span_multiple_rows() {
        let r = wrap_overlay_text("ab", "cdefgh", 0, 4);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].text(), "abcd");
        assert_eq!(r.lines[0].builtin_cells, 2);
        assert_eq!(r.lines[0].ime_cells, 2);
        assert_eq!(r.lines[1].text(), "efgh");
        assert_eq!(r.lines[1].builtin_cells, 0);
        assert_eq!(r.lines[1].ime_cells, 4);
        assert_eq!(r.cursor_row, 0);
        assert_eq!(r.cursor_col, 2);
    }

    #[test]
    fn start_col_offset_pushes_first_row() {
        let r = wrap_overlay_text("abcdefgh", "", 5, 10);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].start_col, 5);
        assert_eq!(r.lines[0].text(), "abcde");
        assert_eq!(r.lines[1].start_col, 0);
        assert_eq!(r.lines[1].text(), "fgh");
        assert_eq!(r.cursor_row, 1);
        assert_eq!(r.cursor_col, 3);
    }

    #[test]
    fn cols_zero_falls_back_to_single_line() {
        let r = wrap_overlay_text("/scp foo", "", 0, 0);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text(), "/scp foo");
        assert_eq!(r.cursor_row, 0);
        assert_eq!(r.cursor_col, 0);
    }

    #[test]
    fn empty_input_returns_single_empty_line() {
        let r = wrap_overlay_text("", "", 3, 10);
        assert_eq!(r.lines.len(), 1);
        assert!(r.lines[0].segments.is_empty());
        assert_eq!(r.lines[0].start_col, 3);
        assert_eq!(r.cursor_row, 0);
        assert_eq!(r.cursor_col, 3);
    }

    #[test]
    fn wide_char_wider_than_cols_still_lays_out() {
        let r = wrap_overlay_text("中", "", 0, 1);
        assert!(!r.lines.is_empty());
    }

    #[test]
    fn wide_chars_each_get_their_own_segment() {
        // builtin="ab中文cd" → 段切分为 "ab" / "中" / "文" / "cd"
        let r = wrap_overlay_text("ab中文cd", "", 0, 80);
        assert_eq!(r.lines.len(), 1);
        let segs = &r.lines[0].segments;
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["ab", "中", "文", "cd"]);
        assert_eq!(segs[0].start_col, 0);
        assert_eq!(segs[0].cells, 2);
        assert_eq!(segs[1].start_col, 2);
        assert_eq!(segs[1].cells, 2);
        assert_eq!(segs[2].start_col, 4);
        assert_eq!(segs[2].cells, 2);
        assert_eq!(segs[3].start_col, 6);
        assert_eq!(segs[3].cells, 2);
        assert_segments_cover(&r.lines[0]);
    }

    #[test]
    fn builtin_ime_boundary_splits_segments() {
        // builtin="ab", ime="cd" → 同一行，但 builtin 和 ime 各占一段
        let r = wrap_overlay_text("ab", "cd", 0, 80);
        assert_eq!(r.lines.len(), 1);
        let segs = &r.lines[0].segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "ab");
        assert!(!segs[0].is_ime);
        assert_eq!(segs[1].text, "cd");
        assert!(segs[1].is_ime);
        assert_eq!(segs[1].start_col, 2);
    }

    #[test]
    fn cjk_in_builtin_each_segment_aligned() {
        // builtin 含中文：cols=80, "ab中cd" → 段 "ab" / "中" / "cd"，光标在 builtin 末尾
        let r = wrap_overlay_text("ab中cd", "", 0, 80);
        assert_eq!(r.lines.len(), 1);
        let texts: Vec<&str> = r.lines[0].segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["ab", "中", "cd"]);
        assert_eq!(r.cursor_col, 6);
        assert_segments_cover(&r.lines[0]);
    }

    #[test]
    fn cols_zero_keeps_builtin_and_ime_separate_segments() {
        let r = wrap_overlay_text("ab", "cd", 7, 0);
        assert_eq!(r.lines.len(), 1);
        let segs = &r.lines[0].segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "ab");
        assert!(!segs[0].is_ime);
        assert_eq!(segs[1].text, "cd");
        assert!(segs[1].is_ime);
    }
}