//! Vim 移动语义。
//!
//! 与编辑器自带的词移动（`previous_start_of_word` / `next_end_of_word`）**不共用**：
//! 那两者基于 Unicode 分词（`split_word_bound_indices`），而 vim 用的是
//! 「空白 / 词字符 / 标点」三类字符的转移规则。例如 `foo.bar` 在 vim 里
//! `w` 会停在 `.` 上（3 个词段），而 Unicode 分词把它当 1~2 个词。
//! 语义不同，所以这里独立实现，并用测试固定行为。
//!
//! 词字符判定复用 [`crate::components::input::is_word_char`]，与高亮、
//! 双击选词等既有功能保持同一口径。

use ropey::Rope;
use sum_tree::Bias;

use crate::components::input::{RopeExt as _, is_word_char};

/// 字符类别。vim 的移动规则建立在三类字符的转移上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharKind {
    Whitespace,
    Word,
    Punctuation,
}

impl CharKind {
    fn of(ch: Option<char>) -> Self {
        match ch {
            None => CharKind::Whitespace,
            Some(ch) if ch.is_whitespace() => CharKind::Whitespace,
            Some(ch) if is_word_char(ch) => CharKind::Word,
            Some(_) => CharKind::Punctuation,
        }
    }
}

/// 光标处字符的类别。
fn kind_at(text: &Rope, offset: usize) -> CharKind {
    if offset >= text.len() {
        return CharKind::Whitespace;
    }
    CharKind::of(text.get_char(offset).ok())
}

/// `w`：下一个词首。
///
/// 规则：先跳过当前词段，再跳过空白，停在下一个词段的第一个字符。
/// 词段 = 连续的同类字符（词字符或标点各算一段）。
pub fn next_word_start(text: &Rope, from: usize) -> usize {
    let len = text.len();
    if from >= len {
        return len;
    }

    let mut offset = from;
    let start_kind = kind_at(text, offset);

    // 跳过当前词段。若起点在空白上，直接进入下一步。
    if start_kind != CharKind::Whitespace {
        while offset < len && kind_at(text, offset) == start_kind {
            offset = advance(text, offset);
        }
    }

    // 跳过空白（含换行）。
    while offset < len && kind_at(text, offset) == CharKind::Whitespace {
        offset = advance(text, offset);
    }

    offset
}

/// `b`：上一个词首。
///
/// 规则：先退一格，跳过空白，再退到当前词段的开头。
pub fn previous_word_start(text: &Rope, from: usize) -> usize {
    if from == 0 {
        return 0;
    }

    let mut offset = retreat(text, from);

    // 跳过空白（含换行）。
    while offset > 0 && kind_at(text, offset) == CharKind::Whitespace {
        offset = retreat(text, offset);
    }

    let kind = kind_at(text, offset);
    while offset > 0 {
        let prev = retreat(text, offset);
        if kind_at(text, prev) != kind {
            break;
        }
        offset = prev;
    }

    offset
}

/// `e`：当前词或下一个词的词尾（含该字符）。
pub fn next_word_end(text: &Rope, from: usize) -> usize {
    let len = text.len();
    if from >= len {
        return len.saturating_sub(1);
    }

    let mut offset = advance(text, from);
    if offset >= len {
        return len.saturating_sub(1);
    }

    // 跳过空白。
    while offset < len && kind_at(text, offset) == CharKind::Whitespace {
        offset = advance(text, offset);
    }
    if offset >= len {
        return len.saturating_sub(1);
    }

    // 走到当前词段的最后一个字符。
    let kind = kind_at(text, offset);
    while offset < len {
        let next = advance(text, offset);
        if next >= len || kind_at(text, next) != kind {
            break;
        }
        offset = next;
    }

    offset
}

/// `$`：行尾字符（不含换行符）。
///
/// vim 的 `$` 停在行内最后一个字符上，而编辑器的 `end_of_line()` 停在换行
/// 之前（即行尾之后）。两者在空行上一致，在非空行上差一个字符——这里取
/// vim 语义。
pub fn end_of_line(text: &Rope, offset: usize) -> usize {
    let row = text.offset_to_point(offset).row;
    let content_end = text.line_content_end_offset(row);
    if content_end > text.line_start_offset(row) {
        // 行内最后一个字符的起始偏移。
        retreat(text, content_end)
    } else {
        // 空行：停在行首。
        content_end
    }
}

/// `0`：行首。
pub fn start_of_line(text: &Rope, offset: usize) -> usize {
    let row = text.offset_to_point(offset).row;
    text.line_start_offset(row)
}

/// 首个非空白字符（`I` 用）。
///
/// 整行都是空白（空行或纯缩进行）时返回行首——vim 里 `I` 在空行上停在
/// 第 0 列，而不是行尾。
pub fn first_non_whitespace(text: &Rope, offset: usize) -> usize {
    let row = text.offset_to_point(offset).row;
    let start = text.line_start_offset(row);
    let end = text.line_content_end_offset(row);
    let mut offset = start;
    while offset < end {
        match text.get_char(offset).ok() {
            Some(ch) if ch == ' ' || ch == '\t' => offset = advance(text, offset),
            _ => return offset,
        }
    }
    // 整行空白：回到行首。
    start
}

/// `f` / `t` / `F` / `T` 的字符查找。
///
/// 返回目标字符的位置；找不到返回 `None`（vim 里光标不动）。
/// `before`/`after` 用于 `t`/`T`——停在目标的前一格/后一格。
pub fn find_char(
    text: &Rope,
    from: usize,
    target: char,
    forward: bool,
    before: bool,
) -> Option<usize> {
    let mut offset = if forward {
        advance(text, from)
    } else {
        if from == 0 {
            return None;
        }
        retreat(text, from)
    };

    // 限定在当前行内查找（vim 的 `f`/`t` 默认不跨行）。
    let row = text.offset_to_point(from).row;
    let line_start = text.line_start_offset(row);
    let line_end = text.line_content_end_offset(row);

    loop {
        if forward {
            if offset >= line_end {
                return None;
            }
        } else if offset < line_start {
            return None;
        }

        if text.get_char(offset).ok() == Some(target) {
            if before {
                // `t`：停在目标之前一格。
                return Some(if forward { retreat(text, offset) } else { advance(text, offset) });
            }
            return Some(offset);
        }

        offset = if forward {
            let next = advance(text, offset);
            if next == offset {
                return None;
            }
            next
        } else {
            if offset == 0 {
                return None;
            }
            retreat(text, offset)
        };
    }
}

/// 前进一个字符，遇 `\r\n` 整体跨过。
fn advance(text: &Rope, offset: usize) -> usize {
    let len = text.len();
    if offset >= len {
        return len;
    }
    let mut next = text.clip_offset(offset + 1, Bias::Right);
    if text.get_char(offset).ok() == Some('\r') && text.get_char(next).ok() == Some('\n') {
        next = text.clip_offset(next + 1, Bias::Right);
    }
    next.min(len)
}

/// 后退一个字符，遇 `\r\n` 整体跨过。
fn retreat(text: &Rope, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut prev = text.clip_offset(offset - 1, Bias::Left);
    if text.get_char(prev).ok() == Some('\n') && prev > 0 {
        let before = text.clip_offset(prev - 1, Bias::Left);
        if text.get_char(before).ok() == Some('\r') {
            prev = before;
        }
    }
    prev
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(text: &str) -> Rope {
        Rope::from_str(text)
    }

    #[test]
    fn next_word_start_treats_punctuation_as_its_own_segment() {
        // vim 里 `foo.bar baz` 的 `w` 依次停在 `.`、`bar`、`baz`。
        let text = rope("foo.bar baz");
        let dot = next_word_start(&text, 0);
        assert_eq!(text.char_to_byte_idx(3), dot, "第一个词首是 `.`");

        let bar = next_word_start(&text, dot);
        assert_eq!(text.char_to_byte_idx(4), bar, "第二个词首是 `bar`");

        let baz = next_word_start(&text, bar);
        assert_eq!(text.char_to_byte_idx(8), baz, "第三个词首是 `baz`");
    }

    #[test]
    fn next_word_start_crosses_newlines() {
        let text = rope("one\ntwo");
        assert_eq!(next_word_start(&text, 0), 4, "应跨过换行停在下一行词首");
    }

    #[test]
    fn previous_word_start_walks_back_segments() {
        let text = rope("foo.bar baz");
        let baz = text.char_to_byte_idx(8);
        let bar = previous_word_start(&text, baz);
        assert_eq!(text.char_to_byte_idx(4), bar);

        let dot = previous_word_start(&text, bar);
        assert_eq!(text.char_to_byte_idx(3), dot);

        let foo = previous_word_start(&text, dot);
        assert_eq!(0, foo);
    }

    #[test]
    fn previous_word_start_at_document_start_stays_put() {
        let text = rope("abc");
        assert_eq!(previous_word_start(&text, 0), 0);
    }

    #[test]
    fn next_word_end_stops_on_last_char_of_segment() {
        // `foo.bar` 的 `e` 依次停在 `o`(2)、`r`(6)。
        let text = rope("foo.bar");
        assert_eq!(next_word_end(&text, 0), 2, "第一个词尾是最后一个 `o`");
        assert_eq!(next_word_end(&text, 3), 6, "下一个词尾是 `r`");
    }

    #[test]
    fn end_of_line_stops_on_last_character_not_newline() {
        let text = rope("abc\ndef");
        // vim 的 `$` 停在第 3 个字符（索引 2），不是换行符（索引 3）。
        assert_eq!(end_of_line(&text, 0), 2);
        assert_eq!(end_of_line(&text, 4), 6);
    }

    #[test]
    fn end_of_line_on_empty_line_stays_at_line_start() {
        let text = rope("abc\n\ndef");
        assert_eq!(end_of_line(&text, 4), 4, "空行的行尾即行首");
    }

    #[test]
    fn start_of_line_handles_crlf() {
        let text = rope("abc\r\ndef");
        assert_eq!(start_of_line(&text, 0), 0);
        // 第二行从 `\r\n` 之后开始。
        assert_eq!(start_of_line(&text, 5), 5);
    }

    #[test]
    fn first_non_whitespace_skips_indent() {
        let text = rope("    let x = 1;");
        assert_eq!(first_non_whitespace(&text, 0), 4);
    }

    #[test]
    fn first_non_whitespace_on_blank_line_stays_at_start() {
        let text = rope("abc\n   \ndef");
        assert_eq!(first_non_whitespace(&text, 4), 4);
    }

    #[test]
    fn motions_are_multibyte_safe() {
        // 中文与 emoji 都应作为一个字符移动，不切开 UTF-8。
        let text = rope("中文 测试🎉 end");
        let offset = next_word_start(&text, 0);
        assert!(text.is_char_boundary(offset), "必须落在字符边界上");
        assert_eq!(offset, "中文 ".len());

        let back = previous_word_start(&text, offset);
        assert_eq!(back, 0);
        assert!(text.is_char_boundary(back));
    }

    #[test]
    fn advance_and_retreat_round_trip() {
        let text = rope("a\r\nb");
        // 从 `a` 前进一格落在 `\r` 上。
        assert_eq!(advance(&text, 0), 1);
        // 从 `\r` 前进应整体跨过 `\r\n`。
        assert_eq!(advance(&text, 1), 3, "`\\r\\n` 应整体跨过");
        // 从 `b` 后退应整体跨回 `\r\n` 之前。
        assert_eq!(retreat(&text, 3), 1, "`\\r\\n` 应整体跨过");
        assert_eq!(retreat(&text, 1), 0);
    }

    #[test]
    fn advance_and_retreat_clamp_at_document_edges() {
        let text = rope("ab");
        assert_eq!(advance(&text, 1), 2);
        assert_eq!(advance(&text, 2), 2, "末尾前进应饱和");
        assert_eq!(retreat(&text, 0), 0, "开头后退应饱和");
    }
}
