//! Vim 文本对象（`i` / `a` 之后的那个字符）。
//!
//! 与 Zed 的 `Object` 枚举同构，但只实现编辑器里真正可用的那些：Zed 有一半
//! 对象依赖 tree-sitter 的 `text_object_ranges`（函数、类、注释），deepterm
//! 的语法层还没有那个接口，所以这里先做**字符扫描类**对象——它们不依赖
//! 语法树，且是日常使用频率最高的（`ciw`、`ci"`、`da(`、`dip`）。
//!
//! 每个对象返回「范围 + 选区语义」：`iw` 是词本身，`aw` 含尾随空白；
//! `i"` 是引号内的内容，`a"` 含引号本身。语义差异直接影响 `d`/`c` 的结果。

use ropey::Rope;

use super::state::MotionKind;
use crate::components::input::RopeExt as _;

/// 文本对象种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObject {
    /// `w` 词。
    Word,
    /// `"` 双引号。
    DoubleQuote,
    /// `'` 单引号。
    SingleQuote,
    /// `` ` `` 反引号。
    BackQuote,
    /// `(` / `)` / `b` 圆括号。
    Paren,
    /// `[` / `]` 方括号。
    Bracket,
    /// `{` / `}` / `B` 花括号。
    Brace,
    /// `<` / `>` 尖括号。
    Angle,
    /// `p` 段落。
    Paragraph,
    /// `t` HTML/XML 标签（字符扫描的简化版）。
    Tag,
}

impl TextObject {
    /// 从按键字符解析对象种类。`None` 表示这个键不是对象名。
    pub fn from_key(key: char) -> Option<Self> {
        Some(match key {
            'w' => TextObject::Word,
            '"' => TextObject::DoubleQuote,
            '\'' => TextObject::SingleQuote,
            '`' => TextObject::BackQuote,
            '(' | ')' | 'b' => TextObject::Paren,
            '[' | ']' => TextObject::Bracket,
            '{' | '}' | 'B' => TextObject::Brace,
            '<' | '>' => TextObject::Angle,
            'p' => TextObject::Paragraph,
            't' => TextObject::Tag,
            _ => return None,
        })
    }

    /// 该对象的定界符（若有）。
    fn delimiters(self) -> Option<(char, char)> {
        Some(match self {
            TextObject::DoubleQuote => ('"', '"'),
            TextObject::SingleQuote => ('\'', '\''),
            TextObject::BackQuote => ('`', '`'),
            TextObject::Paren => ('(', ')'),
            TextObject::Bracket => ('[', ']'),
            TextObject::Brace => ('{', '}'),
            TextObject::Angle => ('<', '>'),
            TextObject::Word | TextObject::Paragraph | TextObject::Tag => return None,
        })
    }
}

/// 找到对象范围。
///
/// `around = false` 对应 `i`（内部），`true` 对应 `a`（含定界符/尾随空白）。
/// 找不到时返回 `None`，调用方应保持光标不动（与 vim 一致）。
pub fn find(
    text: &Rope,
    cursor: usize,
    object: TextObject,
    around: bool,
) -> Option<(std::ops::Range<usize>, MotionKind)> {
    match object {
        TextObject::Word => find_word(text, cursor, around),
        TextObject::Paragraph => find_paragraph(text, cursor, around),
        TextObject::Tag => find_tag(text, cursor, around),
        _ => {
            let (open, close) = object.delimiters()?;
            find_delimited(text, cursor, open, close, around)
        }
    }
}

/// `iw` / `aw`。
fn find_word(
    text: &Rope,
    cursor: usize,
    around: bool,
) -> Option<(std::ops::Range<usize>, MotionKind)> {
    // 复用编辑器的词判定：与双击选词、同词高亮同一口径。
    let word = crate::components::input::word_range_at(text, cursor).or_else(|| {
        // 光标落在空白/标点上时，`iw` 选中的是那一段非空白字符
        // （vim 里 `diw` 在标点上会删掉该标点）。
        let kind = char_kind(text, cursor);
        if kind == CharKind::Whitespace {
            return None;
        }
        let start = expand_back(text, cursor, kind);
        let end = expand_forward(text, cursor, kind);
        (start < end).then_some(start..end)
    })?;

    if !around {
        return Some((word, MotionKind::Exclusive));
    }

    // `aw`：吃掉尾随空白；行尾则吃前置空白。
    let row = text.offset_to_point(word.end).row;
    let line_end = text.line_content_end_offset(row);
    let mut end = word.end;
    // 必须按字符推进（`clip_offset`），不能用 `end += 1`：多字节空白
    // （U+3000 全角空格、U+00A0 等）逐字节推进会停在字符中间，
    // 随后 `Rope::slice` 会因「不在字符边界」panic。
    while end < line_end && text.get_char(end).ok().is_some_and(char::is_whitespace) {
        end = text.clip_offset(end + 1, sum_tree::Bias::Right);
    }
    if end > word.end {
        return Some((word.start..end, MotionKind::Exclusive));
    }

    let mut start = word.start;
    while start > 0 {
        let prev = text.clip_offset(start - 1, sum_tree::Bias::Left);
        match text.get_char(prev).ok() {
            Some(ch) if ch.is_whitespace() && ch != '\n' => start = prev,
            _ => break,
        }
    }
    Some((start..word.end, MotionKind::Exclusive))
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharKind {
    Whitespace,
    Word,
    Punct,
}

fn char_kind(text: &Rope, offset: usize) -> CharKind {
    match text.get_char(offset).ok() {
        None => CharKind::Whitespace,
        Some(ch) if ch.is_whitespace() => CharKind::Whitespace,
        Some(ch) if crate::components::input::is_word_char(ch) => CharKind::Word,
        Some(_) => CharKind::Punct,
    }
}

fn expand_back(text: &Rope, cursor: usize, kind: CharKind) -> usize {
    let mut start = cursor;
    while start > 0 {
        let prev = text.clip_offset(start - 1, sum_tree::Bias::Left);
        if char_kind(text, prev) != kind {
            break;
        }
        start = prev;
    }
    start
}

fn expand_forward(text: &Rope, cursor: usize, kind: CharKind) -> usize {
    let mut end = cursor;
    while end < text.len() && char_kind(text, end) == kind {
        end = text.clip_offset(end + 1, sum_tree::Bias::Right);
    }
    end
}

/// 引号 / 括号类：在当前行内找最内层的一对定界符。
///
/// 定界符成对相同时（引号）不做嵌套计数——vim 里 `ci"` 也是取最近的一对。
/// 括号类做深度计数，保证 `dib` 在嵌套里取最内层。
fn find_delimited(
    text: &Rope,
    cursor: usize,
    open: char,
    close: char,
    around: bool,
) -> Option<(std::ops::Range<usize>, MotionKind)> {
    let row = text.offset_to_point(cursor).row;
    let line_start = text.line_start_offset(row);
    let line_end = text.line_content_end_offset(row);

    let same_delimiter = open == close;

    if same_delimiter {
        // 引号：找光标所在的那一对。
        // 先向左找最近的 open，再向右找对应的 close。
        let mut left = None;
        let mut offset = cursor;
        loop {
            if text.get_char(offset).ok() == Some(open) {
                left = Some(offset);
                break;
            }
            if offset <= line_start {
                break;
            }
            offset = text.clip_offset(offset - 1, sum_tree::Bias::Left);
        }
        let start = left?;

        let mut right = None;
        let mut offset = text.clip_offset(start + 1, sum_tree::Bias::Right);
        while offset < line_end {
            match text.get_char(offset).ok() {
                Some(ch) if ch == close => {
                    right = Some(offset);
                    break;
                }
                Some('\\') => {
                    // 跳过转义的下一个字符。
                    offset = text.clip_offset(offset + 1, sum_tree::Bias::Right);
                }
                _ => {}
            }
            offset = text.clip_offset(offset + 1, sum_tree::Bias::Right);
        }
        let end = right?;

        return Some(if around {
            (start..text.clip_offset(end + 1, sum_tree::Bias::Right), MotionKind::Exclusive)
        } else {
            (
                text.clip_offset(start + 1, sum_tree::Bias::Right)..end,
                MotionKind::Exclusive,
            )
        });
    }

    // 括号：找「包含光标的最内层」一对。
    //
    // 分两步，避免嵌套时取错层：
    // 1. 向右扫描找光标之后最近的 `close`（这是最内层的闭合符）；
    // 2. 从该 `close` 向左做深度计数，配到对应的 `open`。
    // 反过来先向左扫会先碰到内层的 `open`，再向右配到外层的 `close`，
    // 于是 `i(` 在 `f(a, g(b), c)` 上返回整个 `a, g(b), c`。
    let mut close_at = None;
    let mut offset = cursor;
    while offset < line_end {
        if text.get_char(offset).ok() == Some(close) {
            close_at = Some(offset);
            break;
        }
        offset = text.clip_offset(offset + 1, sum_tree::Bias::Right);
    }
    let end = close_at?;

    let mut depth = 0usize;
    let mut open_at = None;
    let mut offset = end;
    loop {
        match text.get_char(offset).ok() {
            Some(ch) if ch == close => depth += 1,
            Some(ch) if ch == open => {
                depth -= 1;
                if depth == 0 {
                    open_at = Some(offset);
                    break;
                }
            }
            _ => {}
        }
        if offset <= line_start {
            break;
        }
        offset = text.clip_offset(offset - 1, sum_tree::Bias::Left);
    }
    let start = open_at?;

    Some(if around {
        (start..text.clip_offset(end + 1, sum_tree::Bias::Right), MotionKind::Exclusive)
    } else {
        (
            text.clip_offset(start + 1, sum_tree::Bias::Right)..end,
            MotionKind::Exclusive,
        )
    })
}

/// `ip` / `ap`：段落。
///
/// 段落 = 连续的空白行之间的连续非空行。`ip` 是段落本身（不含前后空行），
/// `ap` 含尾随空行。
fn find_paragraph(
    text: &Rope,
    cursor: usize,
    around: bool,
) -> Option<(std::ops::Range<usize>, MotionKind)> {
    let row = text.offset_to_point(cursor).row;
    let total = text.lines_len();

    let is_blank = |row: usize| -> bool {
        text.line_content_end_offset(row) == text.line_start_offset(row)
    };

    // 光标在空行上时，`ip` 选中的是这段空行本身。
    if is_blank(row) {
        let mut start = row;
        while start > 0 && is_blank(start - 1) {
            start -= 1;
        }
        let mut end = row;
        while end + 1 < total && is_blank(end + 1) {
            end += 1;
        }
        let range = text.line_start_offset(start)..line_end_inclusive(text, end);
        return Some((range, MotionKind::Linewise));
    }

    let mut start = row;
    while start > 0 && !is_blank(start - 1) {
        start -= 1;
    }
    let mut end = row;
    while end + 1 < total && !is_blank(end + 1) {
        end += 1;
    }

    if around {
        // `ap` 含尾随空行。
        while end + 1 < total && is_blank(end + 1) {
            end += 1;
        }
    }

    let range = text.line_start_offset(start)..line_end_inclusive(text, end);
    Some((range, MotionKind::Linewise))
}

/// 行尾（含换行），末行则到文本末尾。
fn line_end_inclusive(text: &Rope, row: usize) -> usize {
    if row + 1 < text.lines_len() {
        text.line_start_offset(row + 1)
    } else {
        text.len()
    }
}

/// `it` / `at`：标签内容（字符扫描的简化版）。
///
/// 只处理同一行内的 `<tag>...</tag>`，不做嵌套——完整的标签对象需要
/// 语法树支持，这里覆盖最常见的单行场景。
fn find_tag(
    text: &Rope,
    cursor: usize,
    around: bool,
) -> Option<(std::ops::Range<usize>, MotionKind)> {
    let row = text.offset_to_point(cursor).row;
    let line_start = text.line_start_offset(row);
    let line_end = text.line_content_end_offset(row);
    let line = text.slice(line_start..line_end).to_string();

    // 找光标所在的 `<...>` 区间。
    let cursor_col = cursor - line_start;
    let _bytes = line.as_bytes();

    // 向左找最近的 `<`，向右找其 `>`。
    let open_start = line[..cursor_col.min(line.len())].rfind('<')?;
    let name_end = line[open_start..].find(|c: char| c == '>' || c.is_whitespace())? + open_start;
    let name = &line[open_start + 1..name_end];
    if name.is_empty() || name.starts_with('/') {
        return None;
    }
    let open_close = line[open_start..].find('>')? + open_start;
    let close_tag = format!("</{name}>");
    let close_start = line[open_close..].find(&close_tag)? + open_close;

    let open_range = line_start + open_start..line_start + open_close + 1;
    let inner_range = line_start + open_close + 1..line_start + close_start;
    let around_range = line_start + open_start..line_start + close_start + close_tag.len();

    Some(if around {
        (around_range, MotionKind::Exclusive)
    } else {
        (inner_range.clone(), MotionKind::Exclusive)
    })
    .map(|(range, kind)| {
        let _ = open_range;
        (range, kind)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    fn range_of(text: &Rope, cursor: usize, obj: TextObject, around: bool) -> Option<String> {
        find(text, cursor, obj, around).map(|(range, _)| text.slice(range).to_string())
    }

    #[test]
    fn word_object_inner_and_around() {
        let text = rope("hello world");
        assert_eq!(range_of(&text, 0, TextObject::Word, false).as_deref(), Some("hello"));
        assert_eq!(
            range_of(&text, 0, TextObject::Word, true).as_deref(),
            Some("hello "),
            "`aw` 应含尾随空白"
        );
    }

    #[test]
    fn word_object_at_line_end_eats_leading_space() {
        let text = rope("hello world\n");
        assert_eq!(
            range_of(&text, 6, TextObject::Word, true).as_deref(),
            Some(" world"),
            "行尾词无尾随空白时应吃前置空白"
        );
    }

    #[test]
    fn double_quote_inner_and_around() {
        let text = rope(r#"let x = "hello";"#);
        let cursor = 10; // 引号内
        assert_eq!(
            range_of(&text, cursor, TextObject::DoubleQuote, false).as_deref(),
            Some("hello")
        );
        assert_eq!(
            range_of(&text, cursor, TextObject::DoubleQuote, true).as_deref(),
            Some("\"hello\"")
        );
    }

    #[test]
    fn quote_object_handles_escapes() {
        let text = rope(r#"a = "x\"y";"#);
        // 转义的引号不应被当成结束定界符。
        let result = range_of(&text, 5, TextObject::DoubleQuote, false);
        assert_eq!(result.as_deref(), Some(r#"x\"y"#));
    }

    #[test]
    fn paren_object_takes_innermost() {
        // `f(a, g(b), c)`：`b` 在索引 7，内层括号是 6..8，外层是 1..12。
        let text = rope("f(a, g(b), c)");
        assert_eq!(
            range_of(&text, 7, TextObject::Paren, false).as_deref(),
            Some("b"),
            "`i(` 应取最内层括号"
        );
        assert_eq!(
            range_of(&text, 7, TextObject::Paren, true).as_deref(),
            Some("(b)")
        );
    }

    #[test]
    fn paren_object_outside_inner_takes_outer() {
        // 光标在内层括号之外（`, ` 上），应取外层括号的内容。
        let text = rope("f(a, g(b), c)");
        assert_eq!(
            range_of(&text, 9, TextObject::Paren, false).as_deref(),
            Some("a, g(b), c"),
            "光标不在内层括号里时应取外层"
        );
    }

    #[test]
    fn brace_object_inner() {
        let text = rope("fn f() { body }");
        assert_eq!(
            range_of(&text, 12, TextObject::Brace, false).as_deref(),
            Some(" body ")
        );
    }

    #[test]
    fn paragraph_inner_selects_consecutive_lines() {
        let text = rope("one\ntwo\n\nthree\n");
        // 光标在第二行。
        let range = find(&text, 5, TextObject::Paragraph, false).unwrap();
        assert_eq!(range.1, MotionKind::Linewise);
        assert_eq!(text.slice(range.0).to_string(), "one\ntwo\n");
    }

    #[test]
    fn paragraph_around_includes_trailing_blank() {
        let text = rope("one\ntwo\n\nthree\n");
        let range = find(&text, 5, TextObject::Paragraph, true).unwrap();
        assert_eq!(text.slice(range.0).to_string(), "one\ntwo\n\n");
    }

    #[test]
    fn paragraph_on_blank_line_selects_blank_run() {
        let text = rope("one\n\n\ntwo\n");
        let range = find(&text, 5, TextObject::Paragraph, false).unwrap();
        assert_eq!(text.slice(range.0).to_string(), "\n\n");
    }

    #[test]
    fn tag_object_inner_and_around() {
        let text = rope("<div>content</div>");
        assert_eq!(
            range_of(&text, 6, TextObject::Tag, false).as_deref(),
            Some("content")
        );
        assert_eq!(
            range_of(&text, 6, TextObject::Tag, true).as_deref(),
            Some("<div>content</div>")
        );
    }

    #[test]
    fn missing_delimiter_returns_none() {
        let text = rope("no quotes here");
        assert!(find(&text, 0, TextObject::DoubleQuote, false).is_none());
    }

    #[test]
    fn object_keys_map_to_objects() {
        assert_eq!(TextObject::from_key('w'), Some(TextObject::Word));
        assert_eq!(TextObject::from_key('"'), Some(TextObject::DoubleQuote));
        assert_eq!(TextObject::from_key('b'), Some(TextObject::Paren));
        assert_eq!(TextObject::from_key(')'), Some(TextObject::Paren));
        assert_eq!(TextObject::from_key('p'), Some(TextObject::Paragraph));
        assert_eq!(TextObject::from_key('z'), None);
    }
}
