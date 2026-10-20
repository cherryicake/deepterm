//! 网格行 → 可用于链接/路径识别的文本视图。
//!
//! 网格里有两类「看不见但会污染文本」的单元格，直接当普通字符拼进文本，
//! 会在词法上凭空插进空格，把一个 token 劈成两半：
//!
//! - `c == '\0'`：空白化后残留的空单元格。
//! - `WIDE_CHAR_SPACER` / `LEADING_WIDE_CHAR_SPACER`：宽字符（中日韩、全角
//!   标点、emoji）的占位格。alacritty 用一个**空格**存它（`Cell::default()`
//!   的 `c` 就是 `' '`），而 `Grid::display_iter` 不做过滤、原样吐出。
//!
//! 后果是实打实的：`~/项目/src/main.rs` 会被切成 `~/项`、`目`、`/src/main.rs`
//! 三个 token，真实路径永远识别不到；`文档/README.md` 的尾部还会退化成
//! **绝对路径** `/README.md`，一旦根目录下存在同名目录就会打开错的东西。
//!
//! `grid_search_snapshot`（`crate::terminal::terminal`）早就为搜索跳过这两类
//! 格子，链接/路径识别必须同源——否则同一个屏幕文本，搜索能命中而链接不能。
//!
//! 顺带记录每个字符的**占宽**（宽字符 2 列）：命中区与下划线必须覆盖宽字符
//! 的两格，只算 `col + 1` 会让右半格点不到。

use alacritty_terminal::term::cell::Flags;

use crate::terminal::terminal::IndexedCell;

/// 占位格标志：宽字符的第二格（以及 resize 后遗留的前一格）。
const SPACER_FLAGS: Flags = Flags::WIDE_CHAR_SPACER.union(Flags::LEADING_WIDE_CHAR_SPACER);

/// 单元格是否参与文本/命中区构造。
#[inline]
fn is_textual(cell: &IndexedCell) -> bool {
    cell.cell.c != '\0' && !cell.cell.flags.intersects(SPACER_FLAGS)
}

/// 单行的文本视图，附带字节 → 网格列的映射。
///
/// `byte_to_col` 与 `byte_width` 都按 `text` 的**字节**下标索引（一个多字节
/// 字符的每个字节都指向同一列、同一占宽），所以任意命中都能 O(1) 换算成
/// 列区间，不需要 `chars().count()`。
/// 单行文本的列映射。**只有测试用**（生产路径直接走 [`WrappedText`] 跨行
/// 拼接，单行只是它的特例），因此整体按测试编译。
#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowText {
    pub text: String,
    /// 每个字节所属的网格列号。
    pub byte_to_col: Vec<usize>,
    /// 每个字节所属字符的占宽（1 或 2）。
    pub byte_width: Vec<u8>,
}

#[cfg(test)]
impl RowText {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 字节区间 `[start, end)` → 网格列区间 `(start_col, end_col)`（后者开区间）。
    ///
    /// `end_col` 落在最后一个字符**之后**，且按其占宽推进——宽字符的两格都被
    /// 覆盖，右半格同样可点。
    pub fn span(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        if start >= end || end > self.byte_to_col.len() {
            return None;
        }
        let start_col = *self.byte_to_col.get(start)?;
        let last = end - 1;
        let last_col = *self.byte_to_col.get(last)?;
        let width = self.byte_width.get(last).copied().unwrap_or(1) as usize;
        let end_col = last_col + width;
        (end_col > start_col).then_some((start_col, end_col))
    }
}

/// 从一行的网格单元构造 [`RowText`]（仅测试使用，见其文档）。
#[cfg(test)]
pub fn row_text(cells: &[IndexedCell]) -> RowText {
    let mut out = RowText::default();
    out.text.reserve(cells.len());
    out.byte_to_col.reserve(cells.len());
    out.byte_width.reserve(cells.len());
    for cell in cells {
        if !is_textual(cell) {
            continue;
        }
        let ch = cell.cell.c;
        let col = cell.point.column.0;
        let width = if cell.cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        for _ in 0..ch.len_utf8() {
            out.byte_to_col.push(col);
            out.byte_width.push(width);
        }
        out.text.push(ch);
    }
    out
}

/// 字节的归属：第几行、行内列号、字符占宽。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteLoc {
    pub row: usize,
    pub col: usize,
    pub width: u8,
}

/// 跨行（软换行续行）拼接后的文本视图。
///
/// 软换行在网格里是「本行最后一格带 `WRAPLINE`、内容在下一行第 0 列继续」，
/// 对文本来说是**同一个 token 的两半**。识别时必须拼起来算，否则长 URL 会被
/// 拦腰截断成一个假链接；命中区则要按行切开登记，两半都能点。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WrappedText {
    pub text: String,
    pub byte_loc: Vec<ByteLoc>,
}

impl WrappedText {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 把若干行（首行 + 若干续行）拼成一段文本。
    pub fn from_rows<'a>(rows: impl IntoIterator<Item = &'a [IndexedCell]>) -> WrappedText {
        let mut out = WrappedText::default();
        for (row, cells) in rows.into_iter().enumerate() {
            // 每行最多新增 `cells.len()` 个字符（占位格会被跳过），先预留上界。
            out.text.reserve(cells.len());
            out.byte_loc.reserve(cells.len());
            for cell in cells {
                if !is_textual(cell) {
                    continue;
                }
                let ch = cell.cell.c;
                let width = if cell.cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                for _ in 0..ch.len_utf8() {
                    out.byte_loc.push(ByteLoc {
                        row,
                        col: cell.point.column.0,
                        width,
                    });
                }
                out.text.push(ch);
            }
        }
        out
    }

    /// 字节区间 `[start, end)` → 按行切开的 `(row, start_col, end_col)`。
    ///
    /// 跨行命中的每一行各得一段；行内区间同样是开区间列区间。字节不连续或
    /// 空区间返回空 vec。
    pub fn span_rows(&self, start: usize, end: usize) -> Vec<(usize, usize, usize)> {
        if start >= end || end > self.byte_loc.len() {
            return Vec::new();
        }
        let mut out: Vec<(usize, usize, usize)> = Vec::new();
        let mut prev_cell: Option<(usize, usize)> = None;
        for byte in start..end {
            let loc = self.byte_loc[byte];
            // 同一格的多字节字符只算一次，否则一个宽字符会吐出一串重复区间
            if prev_cell == Some((loc.row, loc.col)) {
                continue;
            }
            prev_cell = Some((loc.row, loc.col));
            let end_col = loc.col + loc.width as usize;
            // 行内相邻（软换行拼接后常见）就延长上一段，而不是切出一堆碎片
            match out.last_mut() {
                Some(last) if last.0 == loc.row && last.2 == loc.col => last.2 = end_col,
                _ => out.push((loc.row, loc.col, end_col)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::terminal::{IndexedCell, PackedCell};
    use alacritty_terminal::index::{Column, Line, Point};

    fn cell(ch: char, col: usize, flags: Flags) -> IndexedCell {
        let mut packed = PackedCell::from(&alacritty_terminal::term::cell::Cell::default());
        packed.c = ch;
        packed.flags = flags;
        IndexedCell {
            point: Point::new(Line(0), Column(col)),
            cell: packed,
            zerowidth: None,
            hyperlink: None,
        }
    }

    #[test]
    fn wide_char_spacer_is_not_text() {
        // 「文」占两格，第二格是 spacer（内容是空格）
        let cells = vec![
            cell('文', 0, Flags::WIDE_CHAR),
            cell(' ', 1, Flags::WIDE_CHAR_SPACER),
            cell('a', 2, Flags::empty()),
        ];
        let row = row_text(&cells);
        assert_eq!(row.text, "文a");
        // `文` 后的路径/单词不再被空格劈开
        assert_eq!(row.span(0, row.text.len()), Some((0, 3)));
    }

    #[test]
    fn span_covers_both_columns_of_wide_char() {
        let cells = vec![
            cell('x', 0, Flags::empty()),
            cell('文', 1, Flags::WIDE_CHAR),
            cell(' ', 2, Flags::WIDE_CHAR_SPACER),
        ];
        let row = row_text(&cells);
        // 命中「文」时右半格（第 2 列）也在区间内
        let start = row.text.find('文').unwrap();
        assert_eq!(row.span(start, start + '文'.len_utf8()), Some((1, 3)));
    }

    #[test]
    fn span_rows_splits_wrapped_match() {
        let first = vec![cell('h', 0, Flags::WRAPLINE), cell('t', 1, Flags::empty())];
        let second = vec![cell('t', 0, Flags::empty()), cell('p', 1, Flags::empty())];
        let joined = WrappedText::from_rows([first.as_slice(), second.as_slice()]);
        assert_eq!(joined.text, "http");
        assert_eq!(joined.span_rows(0, 4), vec![(0, 0, 2), (1, 0, 2)]);
    }

    #[test]
    fn span_rows_merges_adjacent_columns() {
        let cells = vec![cell('a', 3, Flags::empty()), cell('b', 4, Flags::empty())];
        let joined = WrappedText::from_rows([cells.as_slice()]);
        assert_eq!(joined.span_rows(0, 2), vec![(0, 3, 5)]);
    }
}
