//! 输入/编辑原语：词法边界、同词查找、括号配对、行首缩进级数。
//!
//! 这些是与 `InputState` 状态无关的纯函数，从 state.rs 拆出独立成文件，
//! 便于单测与复用（state.rs 曾因此膨胀到 5600+ 行）。

use crate::components::input::rope_ext::RopeExt;
use ropey::Rope;
use std::collections::VecDeque;

use std::ops::Range;

/// Sticky scroll 最多钉住的行数。
pub(crate) const STICKY_MAX_LINES: usize = 3;

/// 词边界判定需要 `&str`，只能把文本复制出来；限定窗口后，大文件里按
/// alt-← / alt-backspace 的代价与文件大小无关（窗口整体是空白时才全量扫）。
pub(crate) const WORD_SCAN_WINDOW: usize = 4096;

/// 同词高亮扫描的最大文件长度（字节）。
pub(crate) const OCCURRENCE_MAX_TEXT_LEN: usize = 2_000_000;
/// 同词高亮最多标记的匹配数。
pub(crate) const OCCURRENCE_MAX_MATCHES: usize = 2_000;

/// 单个 CJK 字符不做同词高亮（噪音），由 `word_range_at` 过滤。
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// ASCII 词字符（字节路径专用快速判定）。
pub(crate) fn is_ascii_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 光标处的词字节范围（要求光标在词内或紧贴词边界）。
///
/// 支持 Unicode 标识符（如 `变量1`）；单个非 ASCII 字符（如孤立 CJK 单字）
/// 返回 `None` 以避免全文件单字噪音（延续此前 ASCII 策略的意图）。
pub(crate) fn word_range_at(text: &Rope, offset: usize) -> Option<Range<usize>> {
    use sum_tree::Bias;
    let len = text.len();
    // 光标恒在字符边界（见 `cursor()`），先钳到边界。ropey 2.0 的 `get_char`
    // 取字节下标，前一字符需经 `clip_offset(Left)` 回到其起始边界。
    let offset = text.clip_offset(offset.min(len), Bias::Left);
    let prev_is_word = if offset == 0 {
        false
    } else {
        let pb = text.clip_offset(offset - 1, Bias::Left);
        text.get_char(pb).is_ok_and(is_word_char)
    };
    let next_is_word = offset < len && text.get_char(offset).is_ok_and(is_word_char);
    if !prev_is_word && !next_is_word {
        return None;
    }
    let mut start = offset;
    while start > 0 {
        let pb = text.clip_offset(start - 1, Bias::Left);
        match text.get_char(pb) {
            Ok(c) if is_word_char(c) => start = pb,
            _ => break,
        }
    }
    let mut end = offset;
    while end < len {
        match text.get_char(end) {
            Ok(c) if is_word_char(c) => end += c.len_utf8(),
            _ => break,
        }
    }
    if start >= end {
        return None;
    }
    // 单个非 ASCII 字符：视为噪音，不高亮。
    let mut chars = text.slice(start..end).chars();
    let first = chars.next();
    if chars.next().is_none() && first.is_some_and(|c| !c.is_ascii()) {
        return None;
    }
    Some(start..end)
}

/// 全文件找 `word`（字节范围）的所有边界完整出现。超过 `max_matches` 返回 None。
///
/// ASCII 词走原有字节流式扫描（paint 路径零大分配）；含非 ASCII 的词走
/// `String::match_indices` + 字符边界判定（Unicode 标识符如 `变量` 也能高亮，
/// 边界按 `is_word_char` 而非 ASCII 判定）。
pub(crate) fn find_word_occurrences(
    text: &Rope,
    word: &Range<usize>,
    max_matches: usize,
) -> Option<Vec<Range<usize>>> {
    let wlen = word.len();
    if wlen == 0 {
        return None;
    }
    let target: Vec<u8> = text.slice(word.clone()).bytes().collect();
    let is_ascii_word = target.iter().all(|b| b.is_ascii());
    if !is_ascii_word {
        return find_word_occurrences_unicode(text, word, max_matches);
    }
    let len = text.len();

    // 流式扫描：只保留一个 ≤ `wlen + 2` 字节的滑动窗口（左邻字节 + 目标 +
    // 右邻字节），直接在 rope 的字节迭代器上滑。
    //
    // 旧实现先把整个文档收集成 `Vec<u8>`，而调用点在 paint 路径上、缓存键只
    // 要光标换词就失效 —— 大文件里每敲一个键就要多分配一份全文（上限 2MB）。
    let mut window: VecDeque<u8> = VecDeque::with_capacity(wlen + 2);
    // 已读入的字节数；`window` 覆盖 `[consumed - window.len(), consumed)`。
    let mut consumed = 0usize;
    // 下一个待判定的起点。
    let mut i = 0usize;
    let mut result = Vec::new();

    for byte in text.bytes() {
        window.push_back(byte);
        consumed += 1;
        if window.len() > wlen + 2 {
            window.pop_front();
        }

        // 起点 `i` 要等它完整的 `wlen` 个字节都进窗口才能判定；右邻字节则
        // 要么已经读入，要么已经确定不存在（`i + wlen == len`）。
        while i + wlen <= consumed && (i + wlen < consumed || consumed == len) {
            let start = consumed - window.len();
            let rel = i - start;
            let matched = window.iter().skip(rel).take(wlen).eq(target.iter())
                && (i == 0 || !is_ascii_word_byte(window[rel - 1]))
                && (i + wlen == len || !is_ascii_word_byte(window[rel + wlen]));
            if matched {
                result.push(i..i + wlen);
                if result.len() > max_matches {
                    return None;
                }
                i += wlen;
            } else {
                i += 1;
            }
        }
    }

    Some(result)
}

/// Unicode 词的全文件查找：`match_indices` 定位子串，再按字符判定左右边界。
/// `&str` 的 `chars().next_back()` 只解码末尾最多 4 字节，逐匹配 O(1)。
pub(crate) fn find_word_occurrences_unicode(
    text: &Rope,
    word: &Range<usize>,
    max_matches: usize,
) -> Option<Vec<Range<usize>>> {
    let target = text.slice(word.clone()).to_string();
    if target.is_empty() {
        return None;
    }
    let full = text.to_string();
    let mut result = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = full[search_from..].find(&target) {
        let s = search_from + rel;
        let e = s + target.len();
        let left_ok = s == 0 || !full[..s].chars().next_back().is_some_and(is_word_char);
        let right_ok = e == full.len() || !full[e..].chars().next().is_some_and(is_word_char);
        if left_ok && right_ok {
            result.push(s..e);
            if result.len() > max_matches {
                return None;
            }
        }
        search_from = s + target.len().max(1);
        if search_from >= full.len() {
            break;
        }
    }
    Some(result)
}

/// 朴素全量实现，仅用于测试对照（流式扫描的正确性基准）。
#[cfg(test)]
pub(crate) fn find_word_occurrences_naive(
    text: &Rope,
    word: &Range<usize>,
    max_matches: usize,
) -> Option<Vec<Range<usize>>> {
    let wlen = word.len();
    if wlen == 0 {
        return None;
    }
    let target: Vec<u8> = text.slice(word.clone()).bytes().collect();
    let len = text.len();
    let bytes: Vec<u8> = text.bytes().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i + wlen <= len {
        if &bytes[i..i + wlen] == target.as_slice()
            && (i == 0 || !is_ascii_word_byte(bytes[i - 1]))
            && (i + wlen == len || !is_ascii_word_byte(bytes[i + wlen]))
        {
            result.push(i..i + wlen);
            if result.len() > max_matches {
                return None;
            }
            i += wlen;
        } else {
            i += 1;
        }
    }
    Some(result)
}

/// ASCII 括号字符：开括号返回 None，闭括号返回对应的开括号。
pub(crate) fn matching_open(b: u8) -> Option<u8> {
    match b {
        b')' => Some(b'('),
        b']' => Some(b'['),
        b'}' => Some(b'{'),
        _ => None,
    }
}

/// 对有序括号 token 序列做配对着色（VSCode Rainbow Brackets 同款算法）。
///
/// - 类型配对：`([)]` 交错时弹栈直到同类开括号，保证配对双方同色
/// - 游离闭括号（无同类开括号）与直到结尾未闭合的开括号都标 `matched = false`
///   （渲染上游离闭括号标错误色，未闭合开括号保持层级色，避免输入时闪红）
///
/// `tokens`: (字节偏移, 是否开括号, 括号字符)，必须按偏移升序。
/// 返回每个括号的 (范围, 层级, 是否开括号, 是否配对)。
pub(crate) fn pair_bracket_tokens(
    tokens: &[(usize, bool, u8)],
) -> Vec<(Range<usize>, usize, bool, bool)> {
    // (开括号 span 下标, 开括号字符)
    let mut stack: Vec<(usize, u8)> = Vec::new();
    // (范围, 层级, 是否开括号, 是否配对)
    let mut spans: Vec<(Range<usize>, usize, bool, bool)> = Vec::with_capacity(tokens.len());

    for &(offset, is_open, ch) in tokens {
        if is_open {
            let depth = stack.len();
            spans.push((offset..offset + 1, depth, true, false));
            stack.push((spans.len() - 1, ch));
        } else {
            let Some(want) = matching_open(ch) else {
                continue;
            };
            // 弹栈直到找到同类开括号（容忍交错）；被跨越的开括号视为未配对
            while let Some(&(_, open_ch)) = stack.last() {
                if open_ch == want {
                    break;
                }
                stack.pop();
            }
            if let Some(&(ix, _)) = stack.last() {
                stack.pop();
                spans[ix].3 = true;
                let open_depth = spans[ix].1; // paired brackets share depth
                spans.push((offset..offset + 1, open_depth, false, true));
            } else {
                // 游离闭括号
                spans.push((offset..offset + 1, stack.len(), false, false));
            }
        }
    }
    spans
}

/// 纯文本括号扫描（无语法树时的 fallback）：跳过字符串与 C 风格注释后，
/// 配对算法与 tree 版一致（类型配对 + 交错容忍）。
///
/// 跳过 `\"...\"` / `'...'` / `` `...` ``（`\\` 转义）与 `//...` / `/*...*/`。
/// `#` 注释不跳过（Rust 属性 `#[...]` 与 Python `#` 无法在无语言信息下区分；
/// 有语法树时走 `scan_bracket_spans_tree` 精确排除，此处保守只处理 C 风格）。
pub(crate) fn scan_bracket_spans_text(text: &Rope) -> Vec<(Range<usize>, usize, bool, bool)> {
    let bytes: Vec<u8> = text.bytes().collect();
    let len = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut string: Option<u8> = None;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_depth = 0usize;
    while i < len {
        let b = bytes[i];
        if let Some(q) = string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                string = None;
            }
            i += 1;
            continue;
        }
        if line_comment {
            if b == b'\n' {
                line_comment = false;
            }
            i += 1;
            continue;
        }
        if block_depth > 0 {
            if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
                block_depth += 1;
                i += 2;
                continue;
            }
            if b == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                block_depth -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => {
                string = Some(b);
                i += 1;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                line_comment = true;
                i += 2;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                block_depth = 1;
                i += 2;
            }
            b'(' | b'[' | b'{' => {
                tokens.push((i, true, b));
                i += 1;
            }
            b')' | b']' | b'}' => {
                tokens.push((i, false, b));
                i += 1;
            }
            _ => i += 1,
        }
    }
    pair_bracket_tokens(&tokens)
}

/// 判定 `offset` 处的括号字节是否为代码括号（非字符串/注释内）。
///
/// 复用 tree 版的“独立叶子 token”定义：覆盖该字节的最小节点必须恰为
/// 单字节且 kind 等于括号字面量；字符串/注释内的括号属于更大的原子节点，
/// `byte_range` 更长，直接拒绝。无树时调用方保留旧行为。
pub(crate) fn is_code_bracket(tree: &tree_sitter::Tree, offset: usize, b: u8) -> bool {
    let expected: &str = match b {
        b'(' => "(",
        b')' => ")",
        b'[' => "[",
        b']' => "]",
        b'{' => "{",
        b'}' => "}",
        _ => return false,
    };
    let Some(node) = tree
        .root_node()
        .descendant_for_byte_range(offset, offset + 1)
    else {
        return false;
    };
    node.byte_range() == (offset..offset + 1) && node.kind() == expected
}

/// 从 tree-sitter 语法树收集括号 token：只把「独立叶子 token」的括号算作代码括号，
/// 字符串/注释内部的括号（它们是 string/comment 原子节点的一部分）天然被排除。
///
/// tree-sitter 匿名 token 的 kind 就是字面文本，故 `kind() == "("` 即括号 token。
pub(crate) fn scan_bracket_spans_tree(
    tree: &tree_sitter::Tree,
) -> Vec<(Range<usize>, usize, bool, bool)> {
    let mut tokens = Vec::new();
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        if node.child_count() == 0 {
            match node.kind() {
                "(" | "[" | "{" => {
                    tokens.push((node.start_byte(), true, node.kind().as_bytes()[0]))
                }
                ")" | "]" | "}" => {
                    tokens.push((node.start_byte(), false, node.kind().as_bytes()[0]))
                }
                _ => {}
            }
        }

        // 深度优先遍历
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    pair_bracket_tokens(&tokens)
}

/// 括号匹配的单向扫描上限（字节），防止超大文件下卡顿。
pub(crate) const BRACKET_MATCH_SCAN_LIMIT: usize = 500_000;

/// 从 `start`（一个开/闭括号所在偏移）向配对方向扫描，返回配对括号偏移。
///
/// - `backward: true` 表示 `start` 是闭括号，向前找开括号
/// - `open`/`close` 是这对括号的字节值
///
/// 只比较 ASCII 字节，多字节 UTF-8 序列的后续字节 \u{2265} 0x80，不会误命中。
pub(crate) fn scan_matching_bracket(
    text: &Rope,
    start: usize,
    backward: bool,
    open: u8,
    close: u8,
    limit: usize,
) -> Option<usize> {
    let mut depth = 0usize;
    if backward {
        let lo = start.saturating_sub(limit);
        for i in (lo..=start).rev() {
            let Some(b) = text.get_byte(i) else { continue };
            if b == close {
                depth += 1;
            } else if b == open {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    } else {
        let hi = text.len().min(start + limit);
        for i in start..hi {
            let Some(b) = text.get_byte(i) else { continue };
            if b == open {
                depth += 1;
            } else if b == close {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}
