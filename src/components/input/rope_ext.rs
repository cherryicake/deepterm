use std::ops::Range;

use ropey::{LineType, Rope, RopeSlice};
use sum_tree::Bias;

#[cfg(not(target_family = "wasm"))]
pub use tree_sitter::{InputEdit, Point};

#[cfg(target_family = "wasm")]
/// Stub type for tree-sitter Point on WASM (tree-sitter not available).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

#[cfg(target_family = "wasm")]
impl Point {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

#[cfg(target_family = "wasm")]
/// Stub type for tree-sitter InputEdit on WASM (tree-sitter not available).
#[derive(Debug, Clone, Copy)]
pub struct InputEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_position: Point,
    pub old_end_position: Point,
    pub new_end_position: Point,
}

/// A zero-based line/character position in a text document.
///
/// `line` is the 0-based line index, `character` the 0-based column within
/// that line measured in **UTF-16 code units** — the encoding the LSP spec
/// mandates by default (and what Zed uses via `PointUtf16`). A character
/// outside the BMP (e.g. an emoji) counts as 2 code units, so `character`
/// is neither a byte offset nor a Unicode scalar count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

impl From<(u32, u32)> for Position {
    fn from((line, character): (u32, u32)) -> Self {
        Self { line, character }
    }
}

/// An iterator over the lines of a `Rope`.
pub struct RopeLines<'a> {
    rope: &'a Rope,
    row: usize,
    end_row: usize,
}

impl<'a> RopeLines<'a> {
    /// Create a new `RopeLines` iterator.
    pub fn new(rope: &'a Rope) -> Self {
        let end_row = rope.lines_len();
        Self {
            row: 0,
            end_row,
            rope,
        }
    }
}
impl<'a> Iterator for RopeLines<'a> {
    type Item = RopeSlice<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.row >= self.end_row {
            return None;
        }

        let line = self.rope.slice_line(self.row);
        self.row += 1;
        Some(line)
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.row = self.row.saturating_add(n);
        self.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.end_row - self.row;
        (len, Some(len))
    }
}

impl std::iter::ExactSizeIterator for RopeLines<'_> {}
impl std::iter::FusedIterator for RopeLines<'_> {}

/// An extension trait for [`Rope`] to provide additional utility methods.
pub trait RopeExt {
    /// Start offset of the line at the given row (0-based) index.
    ///
    /// # Example
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    ///
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.line_start_offset(0), 0);
    /// assert_eq!(rope.line_start_offset(1), 6);
    /// ```
    fn line_start_offset(&self, row: usize) -> usize;

    /// Line the end offset (including `\n`) of the line at the given row (0-based) index.
    ///
    /// Return the end of the rope if the row is out of bounds.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.line_end_offset(0), 5); // "Hello\n"
    /// assert_eq!(rope.line_end_offset(1), 12); // "World\r\n"
    /// ```
    fn line_end_offset(&self, row: usize) -> usize;

    /// Return a line slice at the given row (0-based) index. including `\r` if present, but not `\n`.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.slice_line(0).to_string(), "Hello");
    /// assert_eq!(rope.slice_line(1).to_string(), "World\r");
    /// assert_eq!(rope.slice_line(2).to_string(), "This is a test 中文");
    /// assert_eq!(rope.slice_line(6).to_string(), ""); // out of bounds
    /// ```
    fn slice_line(&self, row: usize) -> RopeSlice<'_>;

    /// Return a slice of rows in the given range (0-based, end exclusive).
    ///
    /// If the range is out of bounds, it will be clamped to the valid range.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.slice_lines(0..2).to_string(), "Hello\nWorld\r");
    /// assert_eq!(rope.slice_lines(1..3).to_string(), "World\r\nThis is a test 中文");
    /// assert_eq!(rope.slice_lines(2..5).to_string(), "This is a test 中文\nRope");
    /// assert_eq!(rope.slice_lines(3..10).to_string(), "Rope");
    /// assert_eq!(rope.slice_lines(5..10).to_string(), ""); // out of bounds
    /// assert_eq!(rope.slice_lines(2..2).to_string(), ""); // empty range
    /// ```
    fn slice_lines(&self, rows_range: Range<usize>) -> RopeSlice<'_>;

    /// The line's content with **any** trailing terminator removed, i.e. both
    /// `\n` and `\r\n` are stripped (unlike [`RopeExt::slice_line`], which keeps
    /// a trailing `\r`).
    ///
    /// Use this for line-level operations (join, move line, delete line, …),
    /// otherwise CRLF documents leak stray `\r` bytes into the content.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nlast");
    /// assert_eq!(rope.line_content(0).to_string(), "Hello");
    /// assert_eq!(rope.line_content(1).to_string(), "World");
    /// assert_eq!(rope.line_content(2).to_string(), "last");
    /// assert_eq!(rope.line_content(9).to_string(), ""); // out of bounds
    /// ```
    fn line_content(&self, row: usize) -> RopeSlice<'_>;

    /// Byte length of the line's trailing terminator: `2` for `\r\n`, `1` for
    /// `\n`, `0` when the line has no terminator (last line).
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nlast");
    /// assert_eq!(rope.line_terminator_len(0), 1);
    /// assert_eq!(rope.line_terminator_len(1), 2);
    /// assert_eq!(rope.line_terminator_len(2), 0);
    /// ```
    fn line_terminator_len(&self, row: usize) -> usize;

    /// Byte offset just past the line's content, i.e. **before** its
    /// terminator. Equals `line_end_offset(row) - line_terminator_len(row)`.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nlast");
    /// assert_eq!(rope.line_content_end_offset(0), 5);
    /// assert_eq!(rope.line_content_end_offset(1), 11);
    /// assert_eq!(rope.line_content_end_offset(2), 17);
    /// ```
    fn line_content_end_offset(&self, row: usize) -> usize;

    /// Return an iterator over all lines in the rope.
    ///
    /// Each line slice includes `\r` if present, but not `\n`.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// let lines: Vec<_> = rope.iter_lines().map(|r| r.to_string()).collect();
    /// assert_eq!(lines, vec!["Hello", "World\r", "This is a test 中文", "Rope"]);
    /// ```
    fn iter_lines(&self) -> RopeLines<'_>;

    /// Return the number of lines in the rope.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.lines_len(), 4);
    /// ```
    fn lines_len(&self) -> usize;

    /// Return the length of the row (0-based) in characters, including `\r` if present, but not `\n`.
    ///
    /// If the row is out of bounds, return 0.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.line_byte_len(0), 5); // "Hello"
    /// assert_eq!(rope.line_byte_len(1), 6); // "World\r"
    /// assert_eq!(rope.line_byte_len(2), 21); // "This is a test 中文"
    /// assert_eq!(rope.line_byte_len(4), 0); // out of bounds
    /// ```
    fn line_byte_len(&self, row: usize) -> usize;

    /// Replace the text in the given byte range with new text.
    ///
    /// # Panics
    ///
    /// - If the range is not on char boundary.
    /// - If the range is out of bounds.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let mut rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// rope.replace(6..11, "Universe");
    /// assert_eq!(rope.to_string(), "Hello\nUniverse\r\nThis is a test 中文\nRope");
    /// ```
    fn replace(&mut self, range: Range<usize>, new_text: &str);

    /// Get char at the given offset (byte).
    ///
    /// - If the offset is in the middle of a multi-byte character will panic.
    /// - If the offset is out of bounds, return None.
    fn char_at(&self, offset: usize) -> Option<char>;

    /// Get the byte offset from the given line, column [`Position`] (0-based).
    ///
    /// The column is in **UTF-16 code units** (the LSP default encoding).
    /// A character outside the BMP counts as 2 units.
    fn position_to_offset(&self, line_col: &Position) -> usize;

    /// Get the line, column [`Position`] (0-based) from the given byte offset.
    ///
    /// The column is in **UTF-16 code units** (the LSP default encoding).
    fn offset_to_position(&self, offset: usize) -> Position;

    /// Get point (row, column) from the given byte offset.
    ///
    /// The column is in bytes.
    fn offset_to_point(&self, offset: usize) -> Point;

    /// Get byte offset from the given point (row, column).
    ///
    /// The column is 0-based in bytes.
    ///
    /// The result is always a valid offset into the rope: the row is clamped
    /// to the last line, the column is clamped to that row's *content* (so an
    /// over-long column never spills into the next line), and the final offset
    /// is snapped to a UTF-8 character boundary (so callers can slice at it
    /// without panicking).
    ///
    /// ```
    /// use deepterm::components::input::{Point, rope_ext::RopeExt};
    /// use ropey::Rope;
    /// let rope = Rope::from("Hello\nWorld\r\n中文 ab");
    /// assert_eq!(rope.point_to_offset(Point::new(0, 0)), 0);
    /// assert_eq!(rope.point_to_offset(Point::new(1, 5)), 11);
    /// // Column past the end of the line is clamped, not spilled over.
    /// assert_eq!(rope.point_to_offset(Point::new(1, 99)), 11);
    /// // Row past the end of the buffer clamps to the buffer end.
    /// assert_eq!(rope.point_to_offset(Point::new(99, 0)), rope.len());
    /// // A byte column landing mid-character snaps to the boundary.
    /// assert_eq!(rope.point_to_offset(Point::new(2, 1)), 13);
    /// ```
    fn point_to_offset(&self, point: Point) -> usize;

    /// Get the word byte range at the given byte offset (0-based).
    fn word_range(&self, offset: usize) -> Option<Range<usize>>;

    /// Get word at the given byte offset (0-based).
    fn word_at(&self, offset: usize) -> String;

    /// Get the line byte range at the given byte offset (0-based).
    fn line_range(&self, offset: usize) -> Option<Range<usize>>;

    /// Convert offset in UTF-16 to byte offset (0-based).
    ///
    /// Runs in O(log N) time.
    fn offset_utf16_to_offset(&self, offset_utf16: usize) -> usize;

    /// Convert byte offset (0-based) to offset in UTF-16.
    ///
    /// Runs in O(log N) time.
    fn offset_to_offset_utf16(&self, offset: usize) -> usize;

    /// Get a clipped offset (avoid in a char boundary).
    ///
    /// - If Bias::Left and inside the char boundary, return the ix - 1;
    /// - If Bias::Right and in inside char boundary, return the ix + 1;
    /// - Otherwise return the ix.
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// use sum_tree::Bias;
    ///
    /// let rope = Rope::from("Hello 中文🎉 test\nRope");
    /// assert_eq!(rope.clip_offset(5, Bias::Left), 5);
    /// // Inside multi-byte character '中' (3 bytes)
    /// assert_eq!(rope.clip_offset(7, Bias::Left), 6);
    /// assert_eq!(rope.clip_offset(7, Bias::Right), 9);
    /// ```
    fn clip_offset(&self, offset: usize, bias: Bias) -> usize;

    /// Convert offset in characters to byte offset (0-based).
    ///
    /// Run in O(log n) time.
    ///
    /// # Example
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("a 中文🎉 test\nRope");
    /// assert_eq!(rope.char_index_to_offset(0), 0);
    /// assert_eq!(rope.char_index_to_offset(1), 1);
    /// assert_eq!(rope.char_index_to_offset(3), "a 中".len());
    /// assert_eq!(rope.char_index_to_offset(5), "a 中文🎉".len());
    /// ```
    fn char_index_to_offset(&self, char_index: usize) -> usize;

    /// Convert byte offset (0-based) to offset in characters.
    ///
    /// Run in O(log n) time.
    ///
    /// # Example
    ///
    /// ```
    /// use deepterm::components::input::rope_ext::RopeExt;
    /// use ropey::Rope;
    /// let rope = Rope::from("a 中文🎉 test\nRope");
    /// assert_eq!(rope.offset_to_char_index(0), 0);
    /// assert_eq!(rope.offset_to_char_index(1), 1);
    /// assert_eq!(rope.offset_to_char_index(3), 3);
    /// assert_eq!(rope.offset_to_char_index(4), 3);
    /// ```
    fn offset_to_char_index(&self, offset: usize) -> usize;
}

/// Byte length of `line` with any trailing `\n` / `\r\n` removed.
///
/// This is a free function rather than a trait method on purpose:
/// [`RopeExt::line_start_offset`] is implemented in terms of
/// [`RopeExt::point_to_offset`], so `point_to_offset` must be able to measure a
/// line without routing back through the trait (that would recurse forever).
///
/// Only ASCII terminator bytes are trimmed, so the result always sits on a
/// UTF-8 char boundary and `slice(..end)` cannot panic.
fn content_len(line: &RopeSlice<'_>) -> usize {
    let mut end = line.len();
    while end > 0 {
        match line.byte(end - 1) {
            b'\r' | b'\n' => end -= 1,
            _ => break,
        }
    }
    end
}

impl RopeExt for Rope {
    fn slice_line(&self, row: usize) -> RopeSlice<'_> {
        let total_lines = self.lines_len();
        if row >= total_lines {
            return self.slice(0..0);
        }

        let line = self.line(row, LineType::LF);
        if line.len() > 0 {
            let line_end = line.len() - 1;
            if line.is_char_boundary(line_end) && line.char(line_end) == '\n' {
                return line.slice(..line_end);
            }
        }

        line
    }

    fn slice_lines(&self, rows_range: Range<usize>) -> RopeSlice<'_> {
        // An empty (or inverted) range has no content. Returning early also
        // avoids a panic: `line_end_offset(end - 1)` would land *before*
        // `line_start_offset(start)`, and `Rope::slice` panics when
        // `start > end`.
        if rows_range.start >= rows_range.end {
            return self.slice(0..0);
        }

        let start = self.line_start_offset(rows_range.start);
        let end = self.line_end_offset(rows_range.end.saturating_sub(1));
        self.slice(start..end.max(start))
    }

    fn line_content(&self, row: usize) -> RopeSlice<'_> {
        let line = self.slice_line(row);
        line.slice(..content_len(&line))
    }

    fn line_terminator_len(&self, row: usize) -> usize {
        if row >= self.lines_len() {
            return 0;
        }

        let full = self.line(row, LineType::LF).len();
        full.saturating_sub(self.line_content(row).len())
    }

    fn line_content_end_offset(&self, row: usize) -> usize {
        self.line_start_offset(row) + self.line_content(row).len()
    }

    fn iter_lines(&self) -> RopeLines<'_> {
        RopeLines::new(self)
    }

    fn line_byte_len(&self, row: usize) -> usize {
        self.slice_line(row).len()
    }

    fn line_start_offset(&self, row: usize) -> usize {
        self.point_to_offset(Point::new(row, 0))
    }

    fn offset_to_point(&self, offset: usize) -> Point {
        let offset = self.clip_offset(offset, Bias::Left);
        let row = self.byte_to_line_idx(offset, LineType::LF);
        let line_start = self.line_to_byte_idx(row, LineType::LF);
        let column = offset.saturating_sub(line_start);
        Point::new(row, column)
    }

    fn point_to_offset(&self, point: Point) -> usize {
        if point.row >= self.lines_len() {
            return self.len();
        }

        // Only ropey primitives below: `line_start_offset` delegates back to
        // this method, so any `RopeExt` call here would recurse forever.
        let line_start = self.line_to_byte_idx(point.row, LineType::LF);
        let line = self.line(point.row, LineType::LF);
        // Clamp the column to the row's content, excluding its terminator, so
        // a stale or externally supplied column cannot leap into the following
        // line or past the end of the buffer.
        let offset = line_start + point.column.min(content_len(&line));
        // The column counts bytes and may land inside a multi-byte character.
        self.clip_offset(offset, Bias::Left)
    }

    /// LSP `Position` → 字节偏移。
    ///
    /// `character` 是 **UTF-16 code unit** 计数（LSP 规范默认编码，Zed 同此），
    /// 不是字符数也不是字节数——星体字符（emoji 等）占 2 个 code unit，
    /// 按字符数换算会让其后的所有位置整体偏移。
    ///
    /// 列超出本行内容时钳到行尾（不含换行符），与 `point_to_offset` 一致：
    /// 服务器给出的过期列不该越到下一行。
    fn position_to_offset(&self, pos: &Position) -> usize {
        let row = pos.line as usize;
        if row >= self.lines_len() {
            return self.len();
        }
        let line = self.slice_line(row);
        let line_start = self.line_start_offset(row);
        // 行内容长度（不含 `\n` / `\r\n`）：列的上限。
        let content_len = content_len(&line);
        // 先把列钳到本行的 UTF-16 长度**再**换算：`utf16_to_byte_idx` 对
        // 越界列会 `assert!` panic，服务器给的过期列不能直接喂进去。
        let line_utf16_len = line.len_utf16();
        let character = (pos.character as usize).min(line_utf16_len);
        let byte_col = line.utf16_to_byte_idx(character);
        // 再钳到行内容长度：`line` 可能带换行符，而列不该跨过它。
        let byte_col = byte_col.min(content_len);
        line_start + byte_col
    }

    /// 字节偏移 → LSP `Position`。
    ///
    /// `character` 输出 **UTF-16 code unit** 计数（见 `position_to_offset`）。
    /// 与 `position_to_offset` 严格互逆：对任意字符边界偏移，往返得到原值。
    fn offset_to_position(&self, offset: usize) -> Position {
        let offset = offset.min(self.len());
        let row = self.byte_to_line_idx(offset, LineType::LF);
        let line = self.slice_line(row);
        let line_start = self.line_start_offset(row);
        // 行内字节列（不含换行符），再换算成 UTF-16 code unit 计数。
        let byte_col = offset.saturating_sub(line_start).min(content_len(&line));
        let character = line.byte_to_utf16_idx(byte_col);
        Position::new(row as u32, character as u32)
    }

    fn line_end_offset(&self, row: usize) -> usize {
        if row > self.lines_len() {
            return self.len();
        }

        self.line_start_offset(row) + self.line_byte_len(row)
    }

    fn lines_len(&self) -> usize {
        self.len_lines(LineType::LF)
    }

    fn char_at(&self, offset: usize) -> Option<char> {
        if offset > self.len() {
            return None;
        }

        self.get_char(offset).ok()
    }

    fn word_range(&self, offset: usize) -> Option<Range<usize>> {
        if offset >= self.len() {
            return None;
        }

        let mut left = String::new();
        let offset = self.clip_offset(offset, Bias::Left);
        for c in self.chars_at(offset).reversed() {
            if c.is_alphanumeric() || c == '_' {
                left.insert(0, c);
            } else {
                break;
            }
        }
        let start = offset.saturating_sub(left.len());

        let right = self
            .chars_at(offset)
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>();

        let end = offset + right.len();

        if start == end { None } else { Some(start..end) }
    }

    fn word_at(&self, offset: usize) -> String {
        if let Some(range) = self.word_range(offset) {
            self.slice(range).to_string()
        } else {
            String::new()
        }
    }

    fn line_range(&self, offset: usize) -> Option<Range<usize>> {
        // Allow offset == len so triple-click at EOF selects the last line.
        if offset > self.len() {
            return None;
        }
        let point = self.offset_to_point(offset);
        let start = self.line_start_offset(point.row);
        let end = self.line_end_offset(point.row);
        if start == end { None } else { Some(start..end) }
    }

    #[inline]
    fn offset_utf16_to_offset(&self, offset_utf16: usize) -> usize {
        if offset_utf16 > self.len_utf16() {
            return self.len();
        }

        self.utf16_to_byte_idx(offset_utf16)
    }

    #[inline]
    fn offset_to_offset_utf16(&self, offset: usize) -> usize {
        if offset > self.len() {
            return self.len_utf16();
        }

        self.byte_to_utf16_idx(offset)
    }

    fn replace(&mut self, range: Range<usize>, new_text: &str) {
        let range =
            self.clip_offset(range.start, Bias::Left)..self.clip_offset(range.end, Bias::Right);
        self.remove(range.clone());
        self.insert(range.start, new_text);
    }

    fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        if offset > self.len() {
            return self.len();
        }

        if self.is_char_boundary(offset) {
            return offset;
        }

        if bias == Bias::Left {
            self.floor_char_boundary(offset)
        } else {
            self.ceil_char_boundary(offset)
        }
    }

    fn char_index_to_offset(&self, char_offset: usize) -> usize {
        // `char_to_byte_idx` 是 O(log n) 的树查询，但越界会 panic；旧实现
        // （`chars().take(n)`）越界时饱和到全文长度，这里沿用该语义。
        if char_offset >= self.len_chars() {
            self.len()
        } else {
            self.char_to_byte_idx(char_offset)
        }
    }

    fn offset_to_char_index(&self, offset: usize) -> usize {
        // `clip_offset` 已保证落在字符边界且不越界，因此 `byte_to_char_idx`
        // （O(log n)）不会 panic，且端点语义与旧实现一致。
        let offset = self.clip_offset(offset, Bias::Right);
        self.byte_to_char_idx(offset)
    }
}

#[cfg(test)]
mod tests {
    use ropey::Rope;
    use sum_tree::Bias;
    use tree_sitter::Point;

    use crate::components::input::RopeExt;
    use crate::components::input::rope_ext::Position;

    #[test]
    fn test_slice_line() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert_eq!(rope.slice_line(0).to_string(), "Hello");
        assert_eq!(rope.slice_line(1).to_string(), "World\r");
        assert_eq!(rope.slice_line(2).to_string(), "This is a test 中文");
        assert_eq!(rope.slice_line(3).to_string(), "Rope");

        // over bounds
        assert_eq!(rope.slice_line(6).to_string(), "");

        // only have \r end
        let rope = Rope::from("Hello\r");
        assert_eq!(rope.slice_line(0).to_string(), "Hello\r");
        assert_eq!(rope.slice_line(1).to_string(), "");
    }

    #[test]
    fn test_lines_len() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert_eq!(rope.lines_len(), 4);
        let rope = Rope::from("");
        assert_eq!(rope.lines_len(), 1);
        let rope = Rope::from("Single line");
        assert_eq!(rope.lines_len(), 1);

        // only have \r end
        let rope = Rope::from("Hello\r");
        assert_eq!(rope.lines_len(), 1);
    }

    #[test]
    fn test_lines() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope\r");
        let lines: Vec<_> = rope.iter_lines().map(|r| r.to_string()).collect();
        assert_eq!(
            lines,
            vec!["Hello", "World\r", "This is a test 中文", "Rope\r"]
        );
    }

    #[test]
    fn test_eq() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert!(rope.eq(&Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope")));
        assert!(!rope.eq(&Rope::from("Hello\nWorld")));

        let rope1 = rope.clone();
        assert!(rope.eq(&rope1));
    }

    #[test]
    fn test_iter_lines() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        let lines: Vec<_> = rope
            .iter_lines()
            .skip(1)
            .take(2)
            .map(|r| r.to_string())
            .collect();
        assert_eq!(lines, vec!["World\r", "This is a test 中文"]);
    }

    #[test]
    fn test_line_start_end_offset() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert_eq!(rope.line_start_offset(0), 0);
        assert_eq!(rope.line_end_offset(0), 5);

        assert_eq!(rope.line_start_offset(1), 6);
        assert_eq!(rope.line_end_offset(1), 12);

        assert_eq!(rope.line_start_offset(2), 13);
        assert_eq!(rope.line_end_offset(2), 34);

        assert_eq!(rope.line_start_offset(3), 35);
        assert_eq!(rope.line_end_offset(3), 39);

        assert_eq!(rope.line_start_offset(4), 39);
        assert_eq!(rope.line_end_offset(4), 39);
    }

    #[test]
    fn test_line_column() {
        // 列按 **UTF-16 code unit** 计数（LSP 语义）：
        //   'a'=0..1  ' '=1..2  '中'=2..3  '文'=3..4  '🎉'=4..6  ' '=6..7 ...
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.position_to_offset(&Position::new(0, 3)), "a 中".len());
        // 列 6 才是 emoji 之后（emoji 占 4..6 两个 code unit）。
        assert_eq!(
            rope.position_to_offset(&Position::new(0, 6)),
            "a 中文🎉".len()
        );
        assert_eq!(
            rope.position_to_offset(&Position::new(1, 1)),
            "a 中文🎉 test\nR".len()
        );

        assert_eq!(
            rope.offset_to_position("a 中文🎉 test\nR".len()),
            Position::new(1, 1)
        );
        assert_eq!(
            rope.offset_to_position("a 中文🎉".len()),
            Position::new(0, 6),
            "emoji counts as 2 UTF-16 code units"
        );
    }

    #[test]
    fn test_offset_to_point() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.offset_to_point(0), Point::new(0, 0));
        assert_eq!(rope.offset_to_point(1), Point::new(0, 1));
        assert_eq!(rope.offset_to_point("a 中".len()), Point::new(0, 5));
        assert_eq!(rope.offset_to_point("a 中文🎉".len()), Point::new(0, 12));
        assert_eq!(
            rope.offset_to_point("a 中文🎉 test\nR".len()),
            Point::new(1, 1)
        );
    }

    #[test]
    fn test_point_to_offset() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.point_to_offset(Point::new(0, 0)), 0);
        assert_eq!(rope.point_to_offset(Point::new(0, 1)), 1);
        assert_eq!(rope.point_to_offset(Point::new(0, 5)), "a 中".len());
        assert_eq!(rope.point_to_offset(Point::new(0, 12)), "a 中文🎉".len());
        assert_eq!(
            rope.point_to_offset(Point::new(1, 1)),
            "a 中文🎉 test\nR".len()
        );
    }

    #[test]
    fn test_char_at() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文🎉\nRope");
        assert_eq!(rope.char_at(0), Some('H'));
        assert_eq!(rope.char_at(5), Some('\n'));
        assert_eq!(rope.char_at(13), Some('T'));
        assert_eq!(rope.char_at(28), Some('中'));
        assert_eq!(rope.char_at(34), Some('🎉'));
        assert_eq!(rope.char_at(38), Some('\n'));
        assert_eq!(rope.char_at(50), None);
    }

    #[test]
    fn test_word_at() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文 世界\nRope");
        assert_eq!(rope.word_at(0), "Hello");
        assert_eq!(rope.word_range(0), Some(0..5));
        assert_eq!(rope.word_at(8), "World");
        assert_eq!(rope.word_range(8), Some(6..11));
        assert_eq!(rope.word_at(12), "");
        assert_eq!(rope.word_range(12), None);
        assert_eq!(rope.word_at(13), "This");
        assert_eq!(rope.word_range(13), Some(13..17));
        assert_eq!(rope.word_at(31), "中文");
        assert_eq!(rope.word_range(31), Some(28..34));
        assert_eq!(rope.word_at(38), "世界");
        assert_eq!(rope.word_range(38), Some(35..41));
        assert_eq!(rope.word_at(44), "Rope");
        assert_eq!(rope.word_range(44), Some(42..46));
        assert_eq!(rope.word_at(45), "Rope");
    }

    #[test]
    fn test_offset_utf16_conversion() {
        let rope = Rope::from("hello 中文🎉 test\nRope");
        assert_eq!(rope.offset_to_offset_utf16("hello".len()), 5);
        assert_eq!(rope.offset_to_offset_utf16("hello 中".len()), 7);
        assert_eq!(rope.offset_to_offset_utf16("hello 中文".len()), 8);
        assert_eq!(rope.offset_to_offset_utf16("hello 中文🎉".len()), 10);
        assert_eq!(rope.offset_to_offset_utf16(100), 20);

        assert_eq!(rope.offset_utf16_to_offset(5), "hello".len());
        assert_eq!(rope.offset_utf16_to_offset(7), "hello 中".len());
        assert_eq!(rope.offset_utf16_to_offset(8), "hello 中文".len());
        assert_eq!(rope.offset_utf16_to_offset(10), "hello 中文🎉".len());
        assert_eq!(rope.offset_utf16_to_offset(100), rope.len());
    }

    /// LSP `Position` ↔ 字节偏移：`character` 必须是 **UTF-16 code unit**。
    ///
    /// 回归：曾用字符数（`.chars().count()`）换算，星体字符（emoji）只算 1
    /// 而 LSP 要求算 2，导致 emoji 之后的所有位置整体偏移 1。
    #[test]
    fn test_position_uses_utf16_code_units() {
        // "a🎉b"：a=1 unit, 🎉=2 units, b=1 unit → 共 4 units / 6 bytes。
        let rope = Rope::from("a🎉b");

        // 行内 UTF-16 列 → 字节偏移。
        assert_eq!(rope.position_to_offset(&Position::new(0, 0)), 0);
        assert_eq!(
            rope.position_to_offset(&Position::new(0, 1)),
            1,
            "after 'a'"
        );
        // 🎉 起点：UTF-16 列 1（= 'a' 之后）。
        assert_eq!(
            rope.position_to_offset(&Position::new(0, 3)),
            5,
            "after emoji"
        );
        assert_eq!(
            rope.position_to_offset(&Position::new(0, 4)),
            6,
            "after 'b'"
        );

        // 字节偏移 → UTF-16 列。emoji 之后必须是 3，而不是 2（字符数）。
        assert_eq!(rope.offset_to_position(0), Position::new(0, 0));
        assert_eq!(rope.offset_to_position(1), Position::new(0, 1));
        assert_eq!(
            rope.offset_to_position(5),
            Position::new(0, 3),
            "emoji occupies 2 UTF-16 code units, not 1 character"
        );
        assert_eq!(rope.offset_to_position(6), Position::new(0, 4));
    }

    /// 往返必须精确：任意字符边界偏移 → Position → 原偏移。
    #[test]
    fn test_position_offset_roundtrip_is_exact_with_non_bmp() {
        let rope = Rope::from("let s = \"🎉🎉\";\nlet t = \"中文\";\n");
        for offset in 0..=rope.len() {
            if !rope.is_char_boundary(offset) {
                continue;
            }
            let pos = rope.offset_to_position(offset);
            let back = rope.position_to_offset(&pos);
            assert_eq!(
                back, offset,
                "round-trip broke at byte {offset} (pos {pos:?})"
            );
        }
    }

    /// 多行 + 星体字符：行号与列都要对。
    #[test]
    fn test_position_multiline_with_emoji() {
        let rope = Rope::from("🎉x\ny🎉z\n");
        // 第 1 行（0 基）"y🎉z"：y=1, 🎉=2, z=1。
        assert_eq!(
            rope.offset_to_position(rope.line_start_offset(1)),
            Position::new(1, 0)
        );
        assert_eq!(
            rope.offset_to_position(rope.line_start_offset(1) + 1),
            Position::new(1, 1)
        );
        assert_eq!(
            rope.offset_to_position(rope.line_start_offset(1) + 5),
            Position::new(1, 3),
            "after emoji on line 1"
        );
    }

    /// 列超出本行内容时钳到行尾，不越到下一行（服务器给过期列时的兜底）。
    #[test]
    fn test_position_column_clamps_to_line_end() {
        let rope = Rope::from("ab\ncd\n");
        // 第 0 行内容长 2（字节）/"ab"：列 99 钳到行尾。
        assert_eq!(rope.position_to_offset(&Position::new(0, 99)), 2);
        // 行号越界 → 文本末尾。
        assert_eq!(rope.position_to_offset(&Position::new(99, 0)), rope.len());
    }

    #[test]
    fn test_replace() {
        let mut rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        rope.replace(6..11, "Universe");
        assert_eq!(
            rope.to_string(),
            "Hello\nUniverse\r\nThis is a test 中文\nRope"
        );

        rope.replace(0..5, "Hi");
        assert_eq!(
            rope.to_string(),
            "Hi\nUniverse\r\nThis is a test 中文\nRope"
        );

        rope.replace(rope.len() - 4..rope.len(), "String");
        assert_eq!(
            rope.to_string(),
            "Hi\nUniverse\r\nThis is a test 中文\nString"
        );

        // Test for not on a char boundary
        let mut rope = Rope::from("中文");
        rope.replace(0..1, "New");
        // autocorrect-disable
        assert_eq!(rope.to_string(), "New文");
        let mut rope = Rope::from("中文");
        rope.replace(0..2, "New");
        assert_eq!(rope.to_string(), "New文");
        let mut rope = Rope::from("中文");
        rope.replace(0..3, "New");
        assert_eq!(rope.to_string(), "New文");
        // autocorrect-enable
        let mut rope = Rope::from("中文");
        rope.replace(1..4, "New");
        assert_eq!(rope.to_string(), "New");
    }

    #[test]
    fn test_clip_offset() {
        let rope = Rope::from("Hello 中文🎉 test\nRope");
        // Inside multi-byte character '中' (3 bytes)
        assert_eq!(rope.clip_offset(5, Bias::Left), 5);
        assert_eq!(rope.clip_offset(7, Bias::Left), 6);
        assert_eq!(rope.clip_offset(7, Bias::Right), 9);
        assert_eq!(rope.clip_offset(9, Bias::Left), 9);

        // Inside multi-byte character '🎉' (4 bytes)
        assert_eq!(rope.clip_offset(13, Bias::Left), 12);
        assert_eq!(rope.clip_offset(13, Bias::Right), 16);
        assert_eq!(rope.clip_offset(16, Bias::Left), 16);

        // At character boundary
        assert_eq!(rope.clip_offset(5, Bias::Left), 5);
        assert_eq!(rope.clip_offset(5, Bias::Right), 5);

        // Out of bounds
        assert_eq!(rope.clip_offset(26, Bias::Left), 26);
        assert_eq!(rope.clip_offset(100, Bias::Left), 26);
    }

    #[test]
    fn test_char_index_to_offset() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.char_index_to_offset(0), 0);
        assert_eq!(rope.char_index_to_offset(1), 1);
        assert_eq!(rope.char_index_to_offset(3), "a 中".len());
        assert_eq!(rope.char_index_to_offset(5), "a 中文🎉".len());
        assert_eq!(rope.char_index_to_offset(6), "a 中文🎉 ".len());

        assert_eq!(rope.offset_to_char_index(0), 0);
        assert_eq!(rope.offset_to_char_index(1), 1);
        assert_eq!(rope.offset_to_char_index(3), 3);
        assert_eq!(rope.offset_to_char_index(4), 3);
        assert_eq!(rope.offset_to_char_index(5), 3);
        assert_eq!(rope.offset_to_char_index(6), 4);
        assert_eq!(rope.offset_to_char_index(10), 5);
    }

    /// 越界/边界语义：`char_index_to_offset` 饱和到全文长度，不 panic。
    #[test]
    fn test_char_offset_conversions_are_saturating() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        let chars = rope.chars().count();
        assert_eq!(rope.char_index_to_offset(chars), rope.len());
        assert_eq!(rope.char_index_to_offset(chars + 1), rope.len());
        assert_eq!(rope.char_index_to_offset(usize::MAX), rope.len());
        assert_eq!(rope.offset_to_char_index(usize::MAX), chars);
        assert_eq!(rope.offset_to_char_index(rope.len()), chars);

        // 空 rope 也不能 panic。
        let empty = Rope::new();
        assert_eq!(empty.char_index_to_offset(0), 0);
        assert_eq!(empty.char_index_to_offset(7), 0);
        assert_eq!(empty.offset_to_char_index(0), 0);
        assert_eq!(empty.offset_to_char_index(3), 0);
    }

    /// 差分测试：O(log n) 的树查询必须与朴素逐字符实现在每个字节/字符位置上都
    /// 给出相同答案（审计 3.11 把这两处从 O(n) 换成了 O(log n)）。
    #[test]
    fn char_offset_conversions_match_naive() {
        let text = "a 中文🎉 test\nRope\tending 中文abc\n🎉🎉";
        let rope = Rope::from(text);
        let naive_char_to_byte =
            |n: usize| -> usize { text.chars().take(n).map(|c| c.len_utf8()).sum::<usize>() };
        let naive_byte_to_char = |offset: usize| -> usize {
            let offset = offset.min(text.len());
            let mut end = offset;
            while !text.is_char_boundary(end) {
                end += 1;
            }
            text[..end].chars().count()
        };

        for i in 0..=rope.len_chars() + 2 {
            assert_eq!(
                rope.char_index_to_offset(i),
                naive_char_to_byte(i),
                "char_index_to_offset({i})"
            );
        }
        for b in 0..=rope.len() + 2 {
            assert_eq!(
                rope.offset_to_char_index(b),
                naive_byte_to_char(b),
                "offset_to_char_index({b})"
            );
        }
    }

    #[test]
    fn test_slice_lines_edge_cases() {
        let rope = Rope::from("a\nb\nc");

        // Empty ranges used to panic: `line_end_offset(end - 1)` landed before
        // `line_start_offset(start)`, and `Rope::slice` rejects `start > end`.
        assert_eq!(rope.slice_lines(2..2).to_string(), "");
        assert_eq!(rope.slice_lines(0..0).to_string(), "");
        // 注意：此处用变量构造反转区间，避免 clippy::empty_range 对字面量 `3..1` 报错；
        // 语义仍是 start > end，应返回空串而非 panic。
        let inverted_start = 3;
        let inverted_end = 1;
        assert_eq!(
            rope.slice_lines(inverted_start..inverted_end).to_string(),
            ""
        );

        // Normal ranges keep working.
        assert_eq!(rope.slice_lines(0..2).to_string(), "a\nb");
        assert_eq!(rope.slice_lines(1..3).to_string(), "b\nc");
    }

    #[test]
    fn test_line_content_strips_crlf() {
        let rope = Rope::from("Hello\nWorld\r\nlast");

        // `slice_line` deliberately keeps the `\r`…
        assert_eq!(rope.slice_line(1).to_string(), "World\r");
        // …whereas `line_content` strips any terminator.
        assert_eq!(rope.line_content(0).to_string(), "Hello");
        assert_eq!(rope.line_content(1).to_string(), "World");
        assert_eq!(rope.line_content(2).to_string(), "last");

        assert_eq!(rope.line_terminator_len(0), 1);
        assert_eq!(rope.line_terminator_len(1), 2);
        assert_eq!(rope.line_terminator_len(2), 0);
        // Out of bounds rows report no content and no terminator.
        assert_eq!(rope.line_content(9).to_string(), "");
        assert_eq!(rope.line_terminator_len(9), 0);
    }

    #[test]
    fn test_line_content_end_offset() {
        let rope = Rope::from("Hello\nWorld\r\nlast");
        // "Hello\nWorld\r\nlast": W starts at 6, so row 1's content ends at 11.
        assert_eq!(rope.line_content_end_offset(0), 5);
        assert_eq!(rope.line_content_end_offset(1), 11);
        assert_eq!(rope.line_content_end_offset(2), 17);

        // Content end must sit exactly where the terminator starts.
        for row in 0..3 {
            let content_end = rope.line_content_end_offset(row);
            let next_start = rope.line_start_offset(row + 1);
            assert_eq!(
                next_start - content_end,
                rope.line_terminator_len(row),
                "row {row}: terminator should fill the gap to the next line"
            );
        }
    }

    #[test]
    fn test_line_content_with_multibyte_and_crlf() {
        // Regression guard: trimming the terminator must not slice into a
        // multi-byte character.
        let rope = Rope::from("中文\r\nsecond");
        assert_eq!(rope.line_content(0).to_string(), "中文");
        assert_eq!(rope.line_content_end_offset(0), "中文".len());
        assert_eq!(rope.line_terminator_len(0), 2);
        assert_eq!(rope.line_content(1).to_string(), "second");
    }

    #[test]
    fn test_point_to_offset_clamps_column_to_line_content() {
        // "Hello\n" = 0..6, "World\r\n" = 6..13, "中文 ab" = 13..22
        let rope = Rope::from("Hello\nWorld\r\n中文 ab");
        assert_eq!(rope.point_to_offset(Point::new(0, 0)), 0);
        assert_eq!(rope.point_to_offset(Point::new(1, 0)), 6);
        assert_eq!(rope.point_to_offset(Point::new(1, 5)), 11);

        // Columns past the row's content must clamp instead of spilling into
        // the next line (row 1's content ends at 11, its `\r\n` runs 11..13).
        assert_eq!(rope.point_to_offset(Point::new(1, 99)), 11);
        assert_eq!(rope.point_to_offset(Point::new(0, 99)), 5);

        // Rows past the end clamp to the buffer end.
        assert_eq!(rope.point_to_offset(Point::new(99, 0)), rope.len());
    }

    #[test]
    fn test_point_to_offset_snaps_to_char_boundary() {
        let rope = Rope::from("Hello\nWorld\r\n中文 ab");
        // Byte column 1 lands inside `中` (bytes 13..16); the result must snap
        // back to 13 so that callers can slice at it without panicking.
        let offset = rope.point_to_offset(Point::new(2, 1));
        assert_eq!(offset, 13);
        assert!(rope.is_char_boundary(offset));

        // Every byte column of the multibyte row must produce a boundary.
        for column in 0..12 {
            let offset = rope.point_to_offset(Point::new(2, column));
            assert!(
                rope.is_char_boundary(offset),
                "column {column} produced non-boundary offset {offset}"
            );
            assert!(offset <= rope.len());
        }
    }

    #[test]
    fn test_point_to_offset_round_trips_with_offset_to_point() {
        let rope = Rope::from("Hello\nWorld\r\n中文 ab\nlast\n");
        for row in 0..rope.lines_len() {
            let start = rope.line_start_offset(row);
            let content_end = rope.line_content_end_offset(row);
            for offset in start..=content_end {
                let clipped = rope.clip_offset(offset, Bias::Left);
                let point = rope.offset_to_point(clipped);
                assert_eq!(
                    rope.point_to_offset(point),
                    clipped,
                    "round trip failed for offset {offset} -> {point:?}"
                );
            }
        }
    }

    #[test]
    fn test_point_to_offset_clamps_terminator_offsets() {
        // Offsets that land *on* a terminator are not representable as a point
        // any more: the column is clamped back to the end of the line content,
        // exactly like Zed's `clip_point`.
        let rope = Rope::from("Hello\nWorld\r\nlast");
        // 11 is the `\r` and 12 the `\n` of row 1; row 1's content ends at 11.
        assert_eq!(rope.offset_to_point(11), Point::new(1, 5));
        assert_eq!(rope.point_to_offset(Point::new(1, 6)), 11);
        assert_eq!(rope.point_to_offset(Point::new(1, 7)), 11);
        assert_eq!(rope.line_content_end_offset(1), 11);
    }
}
