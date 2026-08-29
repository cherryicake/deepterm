//! A text input field that allows the user to enter text.
//!
//! Based on the `Input` example from the `gpui` crate.
//! https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs
use gpui::{
    Action, App, AppContext, Bounds, ClipboardItem, Context, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, HighlightStyle, KeyBinding, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollHandle, ScrollWheelEvent,
    ShapedLine, SharedString, Subscription, UTF16Selection, Window, actions, point, px,
};
use gpui::{Half, TextAlign};
use ropey::{Rope, RopeSlice};
use serde::Deserialize;
use std::ops::{Range, RangeInclusive};
use std::rc::Rc;
use sum_tree::Bias;
use unicode_segmentation::*;

use super::{Crease, DisplayMap, MASK_CHAR, blink_cursor::BlinkCursor, mode::InputMode};
use crate::components::button::Size;
use crate::components::input::blink_cursor::CURSOR_WIDTH;
use crate::components::input::change::Change;
use crate::components::input::history::History;
use crate::components::input::movement::MoveDirection;
use crate::components::input::rope_ext::Position;
use crate::components::input::{
    RopeExt as _, Selection, display_map::LineLayout, element::RIGHT_MARGIN,
};
use crate::editor::syntax::DiagnosticSet;
#[cfg(not(target_family = "wasm"))]
use crate::editor::syntax::LanguageRegistry;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = editor, no_json)]
pub struct Enter {
    /// Is confirm with secondary.
    pub secondary: bool,
}

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = editor, no_json)]
pub struct FoldLevel(pub usize);

actions!(
    editor,
    [
        Backspace,
        Delete,
        DeleteToBeginningOfLine,
        DeleteToEndOfLine,
        DeleteToPreviousWordStart,
        DeleteToNextWordEnd,
        Indent,
        Outdent,
        IndentInline,
        OutdentInline,
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        MoveHome,
        MoveEnd,
        MovePageUp,
        MovePageDown,
        SelectAll,
        SelectToStartOfLine,
        SelectToEndOfLine,
        SelectToStart,
        SelectToEnd,
        SelectToPreviousWordStart,
        SelectToNextWordEnd,
        ShowCharacterPalette,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        MoveToStartOfLine,
        MoveToEndOfLine,
        MoveToStart,
        MoveToEnd,
        MoveToPreviousWord,
        MoveToNextWord,
        Escape,
        Search,
        SearchPrevious,
        ToggleComment,
        MoveLineUp,
        MoveLineDown,
        DuplicateLine,
        DeleteLine,
        JoinLines,
        ToggleFold,
        ToggleFoldAll,
        Fold,
        Unfold,
        FoldAll,
        UnfoldAll,
        FoldSelectedRanges,
    ]
);

#[derive(Clone)]
pub enum InputEvent {
    Change,
    PressEnter { secondary: bool },
    /// Emitted when a single-line input with `submit_on_enter` confirms.
    Submit,
    Focus,
    Blur,
}

pub(crate) const CONTEXT: &str = "Input";

/// Git 行变更类型（行号区色条）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitLineMark {
    Added,
    Modified,
    /// 该行上方有被删除的行（红色小三角/色条画在行顶部）。
    Deleted,
}

/// 同词高亮缓存。
pub(crate) struct OccurrenceCache {
    pub(crate) text_version: u64,
    /// 命中条件：光标仍在该词范围内。
    pub(crate) word: Range<usize>,
    pub(crate) ranges: std::rc::Rc<Vec<Range<usize>>>,
}

/// 单个括号的着色信息：`(字节范围, 嵌套层级, 是否开括号, 是否配对)`。
pub(crate) type BracketSpan = (Range<usize>, usize, bool, bool);

/// 括号着色缓存。
pub(crate) struct RainbowCache {
    pub(crate) text_version: u64,
    /// 语法树根指纹；没有语法树时为 `None`。
    pub(crate) tree_fingerprint: Option<usize>,
    pub(crate) spans: std::rc::Rc<Vec<BracketSpan>>,
}

/// 输入校验回调：返回 `false` 表示校验不通过。
pub(crate) type ValidateFn = Box<dyn Fn(&str, &mut Context<InputState>) -> bool + 'static>;

/// 滚动条几何信息（每次 paint 后写入，供拖拽命中与换算）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarGeometry {
    /// thumb 在窗口坐标下的区域。
    pub(crate) thumb: Bounds<Pixels>,
    /// 轨道总高度（= viewport 高度）。
    pub(crate) track_height: Pixels,
    /// thumb 高度。
    pub(crate) thumb_height: Pixels,
    /// 内容总高度。
    pub(crate) content_height: Pixels,
}

/// 括号着色的最大文件长度（字节），超出则跳过（避免每帧全文件扫描）。
pub(crate) const RAINBOW_BRACKETS_MAX_TEXT_LEN: usize = 2_000_000;
/// 同词高亮的最大出现次数，超出则放弃高亮（避免噪音与性能问题）。
const OCCURRENCE_MAX_MATCHES: usize = 2_000;
/// 同词高亮扫描的最大文件长度（字节）。
const OCCURRENCE_MAX_TEXT_LEN: usize = 2_000_000;

/// 行首缩进的每级字节边界：第 k 个元素 = 第 k+1 个缩进单位结束处的字节偏移。
///
/// Tab 算 1 级，每 4 个空格算 1 级（与 SPACE_OUTDENT_WIDTH 一致）。
#[allow(dead_code)] // legacy: kept for tests; guides now use indent columns
pub(crate) fn indent_level_bounds(line_text: &str) -> Vec<usize> {
    let mut bounds = Vec::new();
    let mut spaces = 0;
    for (i, b) in line_text.bytes().enumerate() {
        match b {
            b'\t' => {
                spaces = 0;
                bounds.push(i + 1);
            }
            b' ' => {
                spaces += 1;
                if spaces == 4 {
                    spaces = 0;
                    bounds.push(i + 1);
                }
            }
            _ => break,
        }
    }
    bounds
}

/// Sticky scroll 最多钉住的行数。
const STICKY_MAX_LINES: usize = 3;

/// 是否是适合 sticky scroll 钉住的“定义类”语法节点 kind。
/// 语言无关的字符串匹配（Rust/JS/TS/Python/Java/Go/C 家族常见定义节点）。
fn is_sticky_definition_kind(kind: &str) -> bool {
    matches!(
        kind,
        // Rust
        "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "impl_item"
            | "trait_item"
            | "mod_item"
            | "macro_definition"
            // JS / TS
            | "function_declaration"
            | "function_definition"
            | "method_definition"
            | "method_declaration"
            | "class_declaration"
            | "class_definition"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "namespace_definition"
            // C / C++ / Java / Go
            | "struct_specifier"
            | "struct_declaration"
            | "enum_specifier"
            | "type_declaration"
            | "impl_declaration"
            // Python（function_definition / class_definition 与 JS 重名）
            // 其他
            | "module"
    )
}

/// ASCII 词字符（同词高亮只用 ASCII 词，避免 CJK 单字噪音）。
fn is_ascii_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 光标处的词字节范围（要求光标在词内或紧贴词边界）。
fn word_range_at(text: &Rope, offset: usize) -> Option<Range<usize>> {
    let len = text.len();
    let prev_is_word = offset > 0 && text.get_byte(offset - 1).is_some_and(is_ascii_word_byte);
    let next_is_word = offset < len && text.get_byte(offset).is_some_and(is_ascii_word_byte);
    if !prev_is_word && !next_is_word {
        return None;
    }
    let mut start = offset;
    while start > 0 && text.get_byte(start - 1).is_some_and(is_ascii_word_byte) {
        start -= 1;
    }
    let mut end = offset;
    while end < len && text.get_byte(end).is_some_and(is_ascii_word_byte) {
        end += 1;
    }
    Some(start..end)
}

/// 全文件找 `word`（字节范围）的所有边界完整出现。超过 `max_matches` 返回 None。
fn find_word_occurrences(
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
        if bytes[i] == target[0]
            && &bytes[i..i + wlen] == target.as_slice()
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
fn matching_open(b: u8) -> Option<u8> {
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

/// 纯文本括号扫描（无语法树时的 fallback）：不区分字符串/注释，
/// 但配对算法与 tree 版一致（类型配对 + 交错容忍）。
pub(crate) fn scan_bracket_spans_text(
    text: &Rope,
) -> Vec<(Range<usize>, usize, bool, bool)> {
    let mut tokens = Vec::new();
    for (i, b) in text.bytes().enumerate() {
        match b {
            b'(' | b'[' | b'{' => tokens.push((i, true, b)),
            b')' | b']' | b'}' => tokens.push((i, false, b)),
            _ => {}
        }
    }
    pair_bracket_tokens(&tokens)
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
const BRACKET_MATCH_SCAN_LIMIT: usize = 500_000;

/// 从 `start`（一个开/闭括号所在偏移）向配对方向扫描，返回配对括号偏移。
///
/// - `backward: true` 表示 `start` 是闭括号，向前找开括号
/// - `open`/`close` 是这对括号的字节值
///
/// 只比较 ASCII 字节，多字节 UTF-8 序列的后续字节 \u{2265} 0x80，不会误命中。
fn scan_matching_bracket(
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

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("shift-backspace", Backspace, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("shift-delete", Delete, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-backspace", DeleteToBeginningOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-delete", DeleteToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-delete", DeleteToNextWordEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-delete", DeleteToNextWordEnd, Some(CONTEXT)),
        KeyBinding::new("enter", Enter { secondary: false }, Some(CONTEXT)),
        KeyBinding::new("shift-enter", Enter { secondary: false }, Some(CONTEXT)),
        KeyBinding::new("secondary-enter", Enter { secondary: true }, Some(CONTEXT)),
        KeyBinding::new("escape", Escape, Some(CONTEXT)),
        KeyBinding::new("up", MoveUp, Some(CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(CONTEXT)),
        KeyBinding::new("pageup", MovePageUp, Some(CONTEXT)),
        KeyBinding::new("pagedown", MovePageDown, Some(CONTEXT)),
        KeyBinding::new("tab", IndentInline, Some(CONTEXT)),
        KeyBinding::new("shift-tab", OutdentInline, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-]", Indent, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-]", Indent, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-[", Outdent, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-[", Outdent, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("home", MoveHome, Some(CONTEXT)),
        KeyBinding::new("end", MoveEnd, Some(CONTEXT)),
        KeyBinding::new("shift-home", SelectToStartOfLine, Some(CONTEXT)),
        KeyBinding::new("shift-end", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift-a", SelectToStartOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift-e", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("shift-cmd-left", SelectToStartOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("shift-cmd-right", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-a", MoveHome, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-left", MoveHome, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-e", MoveEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-right", MoveEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-z", Undo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-z", Redo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-up", MoveToStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-down", MoveToEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-left", MoveToPreviousWord, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-right", MoveToNextWord, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-left", MoveToPreviousWord, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-right", MoveToNextWord, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-up", SelectToStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-down", SelectToEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-f", Search, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-f", Search, Some(CONTEXT)),
        KeyBinding::new("shift-enter", SearchPrevious, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-/", ToggleComment, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-/", ToggleComment, Some(CONTEXT)),
        KeyBinding::new("alt-up", MoveLineUp, Some(CONTEXT)),
        KeyBinding::new("alt-down", MoveLineDown, Some(CONTEXT)),
        KeyBinding::new("alt-shift-up", DuplicateLine, Some(CONTEXT)),
        KeyBinding::new("alt-shift-down", DuplicateLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-k", DeleteLine, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-k", DeleteLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-j", JoinLines, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-j", JoinLines, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-[", Fold, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-[", Fold, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-]", Unfold, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-]", Unfold, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-l", ToggleFold, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-l", ToggleFold, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-0", FoldAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-0", FoldAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-j", UnfoldAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-j", UnfoldAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-1", FoldLevel(1), Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-1", FoldLevel(1), Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-2", FoldLevel(2), Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-2", FoldLevel(2), Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-3", FoldLevel(3), Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-3", FoldLevel(3), Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-4", FoldLevel(4), Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-4", FoldLevel(4), Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-5", FoldLevel(5), Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-5", FoldLevel(5), Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-6", FoldLevel(6), Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-6", FoldLevel(6), Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-7", FoldLevel(7), Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-7", FoldLevel(7), Some(CONTEXT)),
    ]);
}

/// Whitespace indicators for rendering spaces and tabs.
#[derive(Clone, Default)]
pub(crate) struct WhitespaceIndicators {
    /// Shaped line for space character indicator (•)
    pub(crate) space: ShapedLine,
    /// Shaped line for tab character indicator (→)
    pub(crate) tab: ShapedLine,
}

#[derive(Clone)]
pub(crate) struct LastLayout {
    /// The visible range (no wrap) of lines in the viewport, the value is row (0-based) index.
    /// This is the buffer line range that encompasses all visible lines.
    pub(crate) visible_range: Range<usize>,
    /// The list of visible buffer line indices (excludes hidden/folded lines).
    /// Parallel to `lines`: `visible_buffer_lines[i]` is the buffer line index of `lines[i]`.
    pub(crate) visible_buffer_lines: Vec<usize>,
    /// Byte offset of each visible buffer line in the Rope (parallel to visible_buffer_lines/lines).
    pub(crate) visible_line_byte_offsets: Vec<usize>,
    /// The first visible line top position in scroll viewport.
    pub(crate) visible_top: Pixels,
    /// The range of byte offset of the visible lines.
    pub(crate) visible_range_offset: Range<usize>,
    /// The last layout lines (Only have visible lines, no empty entries for hidden lines).
    pub(crate) lines: Rc<Vec<LineLayout>>,
    /// The line_height of text layout, this will change will InputElement painted.
    pub(crate) line_height: Pixels,
    /// The wrap width of text layout, this will change will InputElement painted.
    pub(crate) wrap_width: Option<Pixels>,
    /// The line number area width of text layout, if not line number, this will be 0px.
    pub(crate) line_number_width: Pixels,
    /// The cursor position (top, left) in pixels.
    pub(crate) cursor_bounds: Option<Bounds<Pixels>>,
    /// The text align of the text layout.
    pub(crate) text_align: TextAlign,
    /// The content width of the text layout.
    pub(crate) content_width: Pixels,
}

impl LastLayout {
    /// Get the line layout for the given buffer row (0-based).
    ///
    /// Uses binary search on `visible_buffer_lines` to find the line.
    /// Returns None if the row is not visible (out of range or folded).
    pub(crate) fn line(&self, row: usize) -> Option<&LineLayout> {
        let pos = self.visible_buffer_lines.binary_search(&row).ok()?;
        self.lines.get(pos)
    }

    /// Get the alignment offset for the given line width.
    pub(crate) fn alignment_offset(&self, line_width: Pixels) -> Pixels {
        match self.text_align {
            TextAlign::Left => px(0.),
            TextAlign::Center => (self.content_width - line_width).half().max(px(0.)),
            TextAlign::Right => (self.content_width - line_width).max(px(0.)),
        }
    }
}

/// InputState to keep editing state of the [`super::Input`].
pub struct InputState {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) mode: InputMode,
    pub(crate) text: Rope,
    pub(crate) display_map: DisplayMap,
    pub(crate) history: History<Change>,
    pub(crate) blink_cursor: Entity<BlinkCursor>,
    pub(crate) loading: bool,
    /// Range in UTF-8 length for the selected text.
    ///
    /// - "Hello 世界💝" = 16
    /// - "💝" = 4
    pub(crate) selected_range: Selection,
    pub(crate) searchable: bool,
    /// Whether the editor allows find-replace (replace button in the search
    /// panel). Defaults to true for code editors.
    pub(crate) replaceable: bool,
    /// The find / replace panel, lazily created on the first `Search` action.
    pub(crate) search_panel: Option<Entity<super::search::SearchPanel>>,
    /// Range for save the selected word, use to keep word range when drag move.
    pub(crate) selected_word_range: Option<Selection>,
    pub(crate) selection_reversed: bool,
    /// The marked range is the temporary insert text on IME typing.
    pub(crate) ime_marked_range: Option<Selection>,
    pub(crate) last_layout: Option<LastLayout>,
    pub(crate) last_cursor: Option<usize>,
    /// The input container bounds
    pub(crate) input_bounds: Bounds<Pixels>,
    /// The text bounds
    pub(crate) last_bounds: Option<Bounds<Pixels>>,
    pub(crate) last_selected_range: Option<Selection>,
    pub(crate) selecting: bool,
    pub(crate) size: Size,
    pub(crate) disabled: bool,
    pub(crate) masked: bool,
    pub(crate) clean_on_escape: bool,
    pub(crate) soft_wrap: bool,
    /// 同词高亮（光标停在词内时高亮全文件出现）。
    pub(crate) occurrences: bool,
    /// 同词高亮缓存：键为 (text_version, 词范围)，避免每帧全文件扫描。
    pub(crate) occurrence_cache: Option<OccurrenceCache>,
    /// 括号着色（rainbow brackets，按嵌套深度分色）。
    pub(crate) rainbow_brackets: bool,
    /// 括号着色缓存；元素含义见 [`RainbowCache`] 与 [`BracketSpan`]。
    pub(crate) rainbow_cache: Option<RainbowCache>,
    /// 缩进参考线。
    pub(crate) indent_guides: bool,
    /// 悬浮滚动条。
    pub(crate) scrollbar: bool,
    /// 滚动条几何（每次 paint 后写入）。
    pub(crate) scrollbar_geometry: Option<ScrollbarGeometry>,
    /// 滚动条拖拽状态：(按下时鼠标 y, 按下时滚动位置)。
    pub(crate) scrollbar_drag: Option<(Pixels, Pixels)>,
    /// 滚动条拖拽状态：(按下时鼠标 y, 按下时滚动位置)。
    /// Sticky scroll：顶部固定外围定义行。
    pub(crate) sticky_scroll: bool,
    /// Sticky 行点击目标（窗口坐标，由 paint 阶段写入，鼠标点击跳转用）。
    pub(crate) sticky_click_targets: Vec<(usize, Bounds<Pixels>)>,
    /// 折叠 gutter / ··· 点击目标：(buffer start_line, 窗口坐标 bounds)。
    pub(crate) fold_click_targets: Vec<(usize, Bounds<Pixels>)>,
    /// Git 变更标记（行号区色条）。
    pub(crate) git_gutter: bool,
    /// Git 变更标记数据：buffer 行号 → 变更类型（由 Editor 侧异步计算写入）。
    pub(crate) git_line_marks: std::collections::HashMap<u32, GitLineMark>,
    pub(crate) show_whitespaces: bool,
    /// This flag tells the renderer to prefer the end of the current visual line.
    pub(crate) cursor_line_end_affinity: bool,
    pub(crate) pattern: Option<regex::Regex>,
    pub(crate) validate: Option<ValidateFn>,
    pub(crate) scroll_handle: ScrollHandle,
    /// The deferred scroll offset to apply on next layout.
    pub(crate) deferred_scroll_offset: Option<Point<Pixels>>,
    /// The size of the scrollable content.
    pub(crate) scroll_size: gpui::Size<Pixels>,
    pub(crate) text_align: TextAlign,
    pub(crate) placeholder: SharedString,
    pub(crate) placeholder_font_size: Option<Pixels>,
    pub(crate) font_size: Option<Pixels>,

    /// A flag to indicate if we have a pending update to the text.
    ///
    /// If true, will call some update (for example Syntax Highlight) before render.
    pub(crate) _pending_update: bool,

    /// To remember the horizontal column (x-coordinate) of the cursor position for keep column for move up/down.
    ///
    /// The first element is the x-coordinate (Pixels), preferred to use this.
    /// The second element is the column (usize), fallback to use this.
    pub(crate) preferred_column: Option<(Pixels, usize)>,
    /// Diff decorations: per-buffer-line background colors (e.g. added /
    /// deleted lines in a diff view), painted full-width behind the text.
    pub(crate) diff_line_colors: std::collections::HashMap<u32, gpui::Hsla>,
    /// Diff decorations: word-level background colors over byte ranges.
    pub(crate) diff_range_colors: Vec<(Range<usize>, gpui::Hsla)>,
    /// Diff decorations: per-buffer-line line-number label override.
    /// `Some(vec)` indexed by buffer row; `None` entry = blank label (e.g.
    /// deleted lines on the new side of a unified diff).
    pub(crate) diff_line_labels: Option<Vec<Option<u32>>>,
    /// Precomputed syntax highlight runs for the whole buffer (used by the
    /// unified diff view, which cannot parse its merged text with one tree).
    /// Sorted, contiguous and gap-free over `[0, text.len())`; when set, the
    /// internal tree-sitter highlighter is bypassed.
    pub(crate) external_highlight_runs: Option<Vec<(Range<usize>, HighlightStyle)>>,
    /// Monotonic text version, incremented on every edit. Background
    /// parses carry the version they were started for; mismatches are
    /// discarded (prevents stale-tree flicker while typing).
    pub(crate) text_version: u64,
    /// Extra empty rows below the last line the editor can scroll into.
    /// See [`Self::scroll_beyond_last_line`].
    pub(crate) scroll_beyond_last_line: Option<usize>,
    /// Minimum visible rows kept above/below the cursor when scrolling.
    /// See [`Self::cursor_surrounding_lines`].
    pub(crate) cursor_surrounding_lines: Option<usize>,
    /// Single-line inputs: pressing Enter emits the submit event instead of
    /// just propagating.
    pub(crate) submit_on_enter: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<InputEvent> for InputState {}

/// 回车缩进决策结果。
#[derive(Debug, PartialEq, Eq)]
enum NewlineIndent {
    /// 继承当前行缩进（保留原有 max(当前, 下一行) 行为）
    Inherit,
    /// 深入一级（行尾 `{`/`[`/`(` 或 `:`）
    Deeper,
    /// 括号对之间插入空行（`{|}` 中回车）
    BlankBetween,
}

impl InputState {
    /// Create a Input state with default [`InputMode::SingleLine`] mode.
    ///
    /// See also: [`Self::multi_line`], [`Self::auto_grow`] to set other mode.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let blink_cursor = cx.new(|_| BlinkCursor::new());
        let history = History::new().group_interval(std::time::Duration::from_secs(1));

        let _subscriptions = vec![
            // Observe the blink cursor to repaint the view when it changes.
            cx.observe(&blink_cursor, |_, _, cx| cx.notify()),
            // Blink the cursor when the window is active, pause when it's not.
            cx.observe_window_activation(window, |input, window, cx| {
                if window.is_window_active() {
                    let focus_handle = input.focus_handle.clone();
                    if focus_handle.is_focused(window) {
                        input.blink_cursor.update(cx, |blink_cursor, cx| {
                            blink_cursor.start(cx);
                        });
                    }
                }
            }),
            cx.on_focus(&focus_handle, window, Self::on_focus),
            cx.on_blur(&focus_handle, window, Self::on_blur),
        ];

        let text_style = window.text_style();

        Self {
            focus_handle: focus_handle.clone(),
            text: "".into(),
            display_map: DisplayMap::new(text_style.font(), window.rem_size(), None),
            blink_cursor,
            history,
            selected_range: Selection::default(),
            searchable: false,
            replaceable: false,
            search_panel: None,
            selected_word_range: None,
            selection_reversed: false,
            ime_marked_range: None,
            input_bounds: Bounds::default(),
            selecting: false,
            disabled: false,
            masked: false,
            clean_on_escape: false,
            soft_wrap: true,
            occurrences: true,
            occurrence_cache: None,
            rainbow_brackets: true,
            rainbow_cache: None,
            indent_guides: true,
            scrollbar: true,
            scrollbar_geometry: None,
            scrollbar_drag: None,
            sticky_scroll: true,
            sticky_click_targets: Vec::new(),
            fold_click_targets: Vec::new(),
            git_gutter: true,
            git_line_marks: std::collections::HashMap::new(),
            show_whitespaces: false,
            loading: false,
            pattern: None,
            validate: None,
            mode: InputMode::default(),
            last_layout: None,
            last_bounds: None,
            last_selected_range: None,
            last_cursor: None,
            scroll_handle: ScrollHandle::new(),
            scroll_size: gpui::size(px(0.), px(0.)),
            deferred_scroll_offset: None,
            preferred_column: None,
            placeholder: SharedString::default(),
            placeholder_font_size: None,
            font_size: None,
            text_align: TextAlign::Left,
            size: Size::default(),
            _subscriptions,
            _pending_update: false,
            cursor_line_end_affinity: false,
            diff_line_colors: std::collections::HashMap::new(),
            diff_range_colors: Vec::new(),
            diff_line_labels: None,
            external_highlight_runs: None,
            text_version: 0,
            scroll_beyond_last_line: None,
            cursor_surrounding_lines: None,
            submit_on_enter: false,
        }
    }

    /// Override the line-number labels (indexed by buffer row; `None` entry
    /// renders a blank gutter cell). Pass `None` to restore default numbering.
    pub fn set_diff_line_labels(&mut self, labels: Option<Vec<Option<u32>>>, cx: &mut Context<Self>) {
        self.diff_line_labels = labels;
        cx.notify();
    }

    /// Set diff decorations: per-line background colors (buffer line index →
    /// color) and word-level background colors (byte range → color).
    /// Used by the git diff view to render added/deleted lines and inline
    /// word highlights on top of the regular editor rendering.
    pub fn set_diff_decorations(
        &mut self,
        line_colors: Vec<(u32, gpui::Hsla)>,
        range_colors: Vec<(Range<usize>, gpui::Hsla)>,
        cx: &mut Context<Self>,
    ) {
        self.diff_line_colors = line_colors.into_iter().collect();
        self.diff_range_colors = range_colors;
        cx.notify();
    }

    /// Remove all diff decorations.
    #[allow(dead_code)]
    pub fn clear_diff_decorations(&mut self, cx: &mut Context<Self>) {
        self.diff_line_colors.clear();
        self.diff_range_colors.clear();
        self.diff_line_labels = None;
        self.external_highlight_runs = None;
        cx.notify();
    }

    /// Replace the syntax highlight runs with externally computed ones (e.g.
    /// per-source-tree highlights for the unified diff view). `Some(runs)`
    /// must be sorted, contiguous and gap-free over the whole buffer, and
    /// takes over rendering from the internal tree-sitter highlighter (which
    /// is reset to avoid wasted parses of the merged text); `None` restores
    /// normal highlighting.
    pub fn set_external_highlight_runs(
        &mut self,
        runs: Option<Vec<(Range<usize>, HighlightStyle)>>,
        cx: &mut Context<Self>,
    ) {
        self.external_highlight_runs = runs;
        if self.external_highlight_runs.is_some() {
            self.reset_highlighter(cx);
            self._pending_update = false;
        }
        cx.notify();
    }

    /// Set Input to use multi line mode.
    ///
    /// Default rows is 2.
    pub fn multi_line(mut self, multi_line: bool) -> Self {
        self.mode = self.mode.multi_line(multi_line);
        self
    }

    /// Set Input to use [`InputMode::AutoGrow`] mode with min, max rows limit.
    pub fn auto_grow(mut self, min_rows: usize, max_rows: usize) -> Self {
        self.mode = InputMode::auto_grow(min_rows, max_rows);
        self
    }

    /// Set Input to use [`InputMode::CodeEditor`] mode.
    ///
    /// Default options:
    ///
    /// - line_number: true
    /// - tab_size: 2
    /// - hard_tabs: false
    /// - height: 100%
    /// - multi_line: true
    /// - indent_guides: true
    ///
    /// If `highlighter` is None, will use the default highlighter.
    ///
    /// Code Editor aim for help used to simple code editing or display, not a full-featured code editor.
    ///
    /// ## Features
    ///
    /// - Syntax Highlighting
    /// - Auto Indent
    /// - Line Number
    /// - Large Text support, up to 50K lines.
    pub fn code_editor(mut self, language: impl Into<SharedString>) -> Self {
        let language: SharedString = language.into();
        self.mode = InputMode::code_editor(language);
        self.searchable = true;
        self.replaceable = true;
        self
    }

    /// Set this input is replaceable (find-replace in the search panel),
    /// default is false (Default true for Code Editor).
    pub fn replaceable(mut self, replaceable: bool) -> Self {
        self.replaceable = replaceable;
        self
    }

    /// Set this input is searchable, default is false (Default true for Code Editor).
    pub fn searchable(mut self, searchable: bool) -> Self {
        debug_assert!(self.mode.is_multi_line());
        self.searchable = searchable;
        self
    }

    /// Set placeholder
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Set placeholder font size
    pub fn placeholder_font_size(mut self, size: Pixels) -> Self {
        self.placeholder_font_size = Some(size);
        self
    }

    pub fn font_size(mut self, size: Pixels) -> Self {
        self.font_size = Some(size);
        self
    }

    /// Set enable/disable code folding, only for [`InputMode::CodeEditor`] mode.
    ///
    /// Default: true
    pub fn folding(mut self, folding: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { folding: f, .. } = &mut self.mode {
            *f = folding;
        }
        self
    }

    /// Set code folding at runtime, only for [`InputMode::CodeEditor`] mode.
    ///
    /// When disabling, all existing folds are cleared.
    pub fn set_folding(&mut self, folding: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { folding: f, .. } = &mut self.mode {
            *f = folding;
        }
        if !folding {
            self.display_map.clear_folds();
        }
        cx.notify();
    }

    /// Set enable/disable line number, only for [`InputMode::CodeEditor`] mode.
    pub fn line_number(mut self, line_number: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { line_number: l, .. } = &mut self.mode {
            *l = line_number;
        }
        self
    }

    /// Set line number, only for [`InputMode::CodeEditor`] mode.
    pub fn set_line_number(&mut self, line_number: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { line_number: l, .. } = &mut self.mode {
            *l = line_number;
        }
        cx.notify();
    }

    /// Set the number of rows for the multi-line Textarea.
    ///
    /// This is only used when `multi_line` is set to true.
    ///
    /// default: 2
    pub fn rows(mut self, rows: usize) -> Self {
        match &mut self.mode {
            InputMode::PlainText { rows: r, .. } | InputMode::CodeEditor { rows: r, .. } => {
                *r = rows
            }
            InputMode::AutoGrow {
                max_rows: max_r,
                rows: r,
                ..
            } => {
                *r = rows;
                *max_r = rows;
            }
        }
        self
    }

    /// Set highlighter language for for [`InputMode::CodeEditor`] mode.
    pub fn set_highlighter(
        &mut self,
        new_language: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
                language,
                highlighter,
                parse_task,
                ..
            } = &mut self.mode {
            *language = new_language.into();
            *highlighter.borrow_mut() = None;
            parse_task.borrow_mut().take();
        }
        cx.notify();
    }

    /// 切换语言并立即全量重新解析/高亮（不产生历史记录）。
    pub fn set_language(
        &mut self,
        new_language: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_highlighter(new_language, cx);
        // 空编辑 + force 触发新建 highlighter 并全量解析
        let range = 0..0;
        let bg = self
            .mode
            .update_highlighter(&range, &self.text, "", true, cx);
        if let Some(bg) = bg {
            Self::dispatch_background_parse(bg, self.text_version, window, cx);
        }
        cx.notify();
    }

    fn reset_highlighter(&mut self, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor {
                highlighter,
                parse_task,
                ..
            } = &mut self.mode {
            *highlighter.borrow_mut() = None;
            parse_task.borrow_mut().take();
        }
        cx.notify();
    }

    #[inline]
    pub fn diagnostics(&self) -> Option<&DiagnosticSet> {
        match &self.mode {
            InputMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }

    #[inline]
    pub fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        self.mode.diagnostics_mut()
    }

    /// 当前编辑器语言名（仅 [`InputMode::CodeEditor`]），如 "rust"、"python"。
    pub fn language(&self) -> Option<SharedString> {
        match &self.mode {
            InputMode::CodeEditor { language, .. } => Some(language.clone()),
            _ => None,
        }
    }

    /// 语言对应的行注释 token（无行注释语法的返回 None，如 JSON/CSS/HTML）。
    pub(crate) fn line_comment_token(language: Option<&str>) -> Option<&'static str> {
        // 语言名与 editor::syntax::languages 的 Language::id 对应
        Some(match language? {
            "rust" | "javascript" | "typescript" | "tsx" | "go" | "c" | "cpp" | "kotlin"
            | "swift" | "java" | "zig" => "//",
            "python" | "bash" | "makefile" | "toml" | "yaml" | "ruby" | "perl" => "#",
            "lua" | "sql" => "--",
            _ => return None,
        })
    }

    /// 给单行加注释（缩进之后插入 `token + " "`）。返回 (新行, 插入点)。
    fn add_line_comment(line: &str, token: &str) -> (String, usize) {
        let indent_len = line.len() - line.trim_start().len();
        let mut out = String::with_capacity(line.len() + token.len() + 1);
        out.push_str(&line[..indent_len]);
        out.push_str(token);
        out.push(' ');
        out.push_str(&line[indent_len..]);
        (out, indent_len)
    }

    /// 去掉单行注释（连同 token 后的一个空格）。返回 (新行, 原 token 位置)。
    fn strip_line_comment(line: &str, token: &str) -> (String, usize) {
        let Some(idx) = line.find(token) else {
            return (line.to_string(), 0);
        };
        let mut rest = &line[idx + token.len()..];
        if let Some(stripped) = rest.strip_prefix(' ') {
            rest = stripped;
        }
        let mut out = String::with_capacity(line.len());
        out.push_str(&line[..idx]);
        out.push_str(rest);
        (out, idx)
    }

    /// 括号/引号自动配对表：`(开, 闭)`。
    const AUTO_PAIRS: &[(char, char)] = &[
        ('(', ')'),
        ('[', ']'),
        ('{', '}'),
        ('"', '"'),
        ('\'', '\''),
        ('`', '`'),
    ];

    /// Set placeholder
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Find which line and sub-line the given offset belongs to, along with the position within that sub-line.
    ///
    /// Returns:
    ///
    /// - The index of the line (zero-based) containing the offset.
    /// - The index of the sub-line (zero-based) within the line containing the offset.
    /// - The position of the offset.
    pub(crate) fn line_and_position_for_offset(
        &self,
        offset: usize,
    ) -> (usize, usize, Option<Point<Pixels>>) {
        let Some(last_layout) = &self.last_layout else {
            return (0, 0, None);
        };
        let line_height = last_layout.line_height;

        let mut y_offset = last_layout.visible_top;
        for (vi, line) in last_layout.lines.iter().enumerate() {
            let prev_lines_offset = last_layout.visible_line_byte_offsets[vi];
            let local_offset = offset.saturating_sub(prev_lines_offset);
            if let Some(pos) = line.position_for_index(local_offset, last_layout, false) {
                let sub_line_index = (pos.y / line_height) as usize;
                let adjusted_pos = point(pos.x + last_layout.line_number_width, pos.y + y_offset);
                return (vi, sub_line_index, Some(adjusted_pos));
            }

            y_offset += line.size(line_height).height;
        }
        (0, 0, None)
    }

    /// Set the text of the input field.
    ///
    /// And the selection_range will be reset to 0..0.
    pub fn set_value(
        &mut self,
        value: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history.ignore = true;
        self.replace_text(value, window, cx);
        self.history.ignore = false;

        // Ensure cursor to start when set text
        if self.mode.is_single_line() {
            self.selected_range = (self.text.len()..self.text.len()).into();
        } else {
            self.selected_range.clear();
        }

        if self.mode.is_code_editor() {
            self._pending_update = true;
        }

        // Move scroll to top
        self.scroll_handle.set_offset(point(px(0.), px(0.)));

        cx.notify();
    }

    /// Set the text of the input field without requiring a window.
    ///
    /// Unlike [`Self::set_value`], this does not need a `Window` and is safe to call from
    /// background file loads. It updates the rope, display map,
    /// and schedules a highlight refresh for code editors.
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.history.ignore = true;
        let text: SharedString = text.into();
        self.text = Rope::from(text.as_ref());
        if self.mode.is_single_line() {
            self.selected_range = (self.text.len()..self.text.len()).into();
        } else {
            self.selected_range.clear();
        }
        self.ime_marked_range = None;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        self.display_map.set_text(&self.text, cx);
        if self.mode.is_code_editor() {
            self.reset_highlighter(cx);
            self._pending_update = true;
        }
        self.history.ignore = false;
        self.update_search(cx);
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    /// Clear the input text.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
    }

    /// Insert text at the current cursor position.
    ///
    /// And the cursor will be moved to the end of inserted text.
    pub fn insert(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_disabled = self.disabled;
        self.disabled = false;
        let text: SharedString = text.into();
        let range_utf16 = self.range_to_utf16(&(self.cursor()..self.cursor()));
        self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
        self.selected_range = (self.selected_range.end..self.selected_range.end).into();
        self.disabled = was_disabled;
    }

    /// Replace text at the current cursor position.
    ///
    /// And the cursor will be moved to the end of replaced text.
    pub fn replace(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_disabled = self.disabled;
        self.disabled = false;
        let text: SharedString = text.into();
        self.replace_text_in_range_silent(None, &text, window, cx);
        self.selected_range = (self.selected_range.end..self.selected_range.end).into();
        self.disabled = was_disabled;
    }

    fn replace_text(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_disabled = self.disabled;
        self.disabled = false;
        let text: SharedString = text.into();
        let range = 0..self.text.chars().map(|c| c.len_utf16()).sum();
        self.replace_text_in_range_silent(Some(range), &text, window, cx);
        self.reset_highlighter(cx);
        self.disabled = was_disabled;
    }

    /// Set with disabled mode.
    ///
    /// See also: [`Self::set_disabled`], [`Self::is_disabled`].
    #[allow(unused)]
    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Set with password masked state.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn masked(mut self, masked: bool) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.masked = masked;
        self
    }

    /// Set the password masked state of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn set_masked(&mut self, masked: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_single_line());
        self.masked = masked;
        cx.notify();
    }

    /// Set true to clear the input by pressing Escape key.
    pub fn clean_on_escape(mut self) -> Self {
        self.clean_on_escape = true;
        self
    }

    /// Set the soft wrap mode for multi-line input, default is true.
    pub fn soft_wrap(mut self, wrap: bool) -> Self {
        debug_assert!(self.mode.is_multi_line());
        self.soft_wrap = wrap;
        self
    }

    /// `editor.scrollBeyondLastLine`: allow scrolling `rows` empty rows past
    /// the last line.
    pub fn scroll_beyond_last_line(mut self, rows: Option<usize>) -> Self {
        self.scroll_beyond_last_line = rows;
        self
    }

    /// Keep at least `lines` visible rows above/below the cursor when the
    /// editor scrolls.
    pub fn cursor_surrounding_lines(mut self, lines: Option<usize>) -> Self {
        self.cursor_surrounding_lines = lines;
        self
    }

    /// Single-line inputs: pressing Enter emits the submit event.
    pub fn submit_on_enter(mut self, submit: bool) -> Self {
        self.submit_on_enter = submit;
        self
    }

    /// Set whether to show whitespace characters.
    pub fn show_whitespaces(mut self, show: bool) -> Self {
        self.show_whitespaces = show;
        self
    }

    /// 同词高亮（默认开启）。
    pub fn occurrences(mut self, enable: bool) -> Self {
        self.occurrences = enable;
        self
    }

    /// 括号着色（默认开启）。
    pub fn rainbow_brackets(mut self, enable: bool) -> Self {
        self.rainbow_brackets = enable;
        self
    }

    /// 缩进参考线（默认开启）。
    pub fn indent_guides(mut self, enable: bool) -> Self {
        self.indent_guides = enable;
        self
    }

    /// 悬浮滚动条（默认开启）。
    pub fn scrollbar(mut self, enable: bool) -> Self {
        self.scrollbar = enable;
        self
    }

    /// Sticky scroll（默认开启，仅 code_editor 多行模式生效）。
    pub fn sticky_scroll(mut self, enable: bool) -> Self {
        self.sticky_scroll = enable;
        self
    }

    /// Git 变更标记（默认开启）。
    pub fn git_gutter(mut self, enable: bool) -> Self {
        self.git_gutter = enable;
        self
    }

    /// 写入 Git 行变更标记（由 Editor 侧异步计算后调用）。
    pub fn set_git_line_marks(
        &mut self,
        marks: Vec<(u32, GitLineMark)>,
        cx: &mut Context<Self>,
    ) {
        self.git_line_marks = marks.into_iter().collect();
        cx.notify();
    }

    /// 同词高亮：返回光标处词的全文件出现范围（带缓存）。
    /// None 表示当前无需高亮（非词内/超出限制/未启用）。
    pub(crate) fn occurrence_ranges(&mut self) -> Option<std::rc::Rc<Vec<Range<usize>>>> {
        if !self.occurrences || self.mode.is_single_line() || self.masked {
            return None;
        }
        if !self.selected_range.is_empty() {
            return None;
        }
        if self.text.len() > OCCURRENCE_MAX_TEXT_LEN {
            return None;
        }
        let cursor = self.cursor();
        if let Some(cache) = &self.occurrence_cache
            && cache.text_version == self.text_version
            && cache.word.contains(&cursor)
        {
            return Some(cache.ranges.clone());
        }
        let word = word_range_at(&self.text, cursor)?;
        let ranges = find_word_occurrences(&self.text, &word, OCCURRENCE_MAX_MATCHES)?;
        let rc = std::rc::Rc::new(ranges);
        self.occurrence_cache = Some(OccurrenceCache {
            text_version: self.text_version,
            word,
            ranges: rc.clone(),
        });
        Some(rc)
    }

    /// Sticky scroll：首可见行的外围定义节点所在行，外→内序，最多
    /// [`STICKY_MAX_LINES`] 行。仅返回完全位于首可见行之上的行；
    /// 无语法树 / 非代码模式返回空。
    pub(crate) fn sticky_lines(&self, first_visible_row: usize) -> Vec<usize> {
        if !self.sticky_scroll || self.mode.is_single_line() || self.masked {
            return Vec::new();
        }
        // diff 视图（外部高亮）与图片等场景不做 sticky
        if self.external_highlight_runs.is_some() {
            return Vec::new();
        }
        if first_visible_row == 0 {
            return Vec::new();
        }
        let InputMode::CodeEditor { highlighter, .. } = &self.mode else {
            return Vec::new();
        };
        let hl = highlighter.borrow();
        let Some(hl) = hl.as_ref() else {
            return Vec::new();
        };
        let Some(tree) = hl.tree() else {
            return Vec::new();
        };
        let root = tree.root_node();

        // 首可见行起点的字节偏移（防御：越界时取最后一行）
        let last_line = self.text.len_lines(ropey::LineType::LF).saturating_sub(1);
        let row = first_visible_row.min(last_line);
        let start_byte = self.text.line_start_offset(row);

        // 从该字节最深的节点向上收集定义类祖先（内→外序）
        let mut node = root.descendant_for_byte_range(start_byte, start_byte);
        let mut rows: Vec<usize> = Vec::new();
        while let Some(n) = node {
            let start_row = n.start_position().row;
            if start_row < first_visible_row
                && is_sticky_definition_kind(n.kind())
                && !rows.contains(&start_row)
            {
                rows.push(start_row);
            }
            node = n.parent();
        }
        rows.reverse(); // 外→内
        if rows.len() > STICKY_MAX_LINES {
            let keep = rows.len() - STICKY_MAX_LINES;
            rows.drain(0..keep);
        }
        rows
    }

    /// Update the soft wrap mode for multi-line input, default is true.
    pub fn set_soft_wrap(&mut self, wrap: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_multi_line());
        self.soft_wrap = wrap;
        if wrap {
            let wrap_width = self
                .last_layout
                .as_ref()
                .and_then(|b| b.wrap_width)
                .unwrap_or(self.input_bounds.size.width);

            self.display_map.on_layout_changed(Some(wrap_width), cx);

            // Reset scroll to left 0
            let mut offset = self.scroll_handle.offset();
            offset.x = px(0.);
            self.scroll_handle.set_offset(offset);
        } else {
            self.display_map.on_layout_changed(None, cx);
        }
        cx.notify();
    }

    /// Update whether to show whitespace characters.
    pub fn set_show_whitespaces(&mut self, show: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.show_whitespaces = show;
        cx.notify();
    }

    /// Set the regular expression pattern of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn pattern(mut self, pattern: regex::Regex) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.pattern = Some(pattern);
        self
    }

    /// Set the regular expression pattern of the input field with reference.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn set_pattern(
        &mut self,
        pattern: regex::Regex,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        debug_assert!(self.mode.is_single_line());
        self.pattern = Some(pattern);
    }

    /// Set the validation function of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn validate(mut self, f: impl Fn(&str, &mut Context<Self>) -> bool + 'static) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.validate = Some(Box::new(f));
        self
    }

    /// Set true to show spinner at the input right.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn set_loading(&mut self, loading: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_single_line());
        self.loading = loading;
        cx.notify();
    }

    /// Set the default value of the input field.
    pub fn default_value(mut self, value: impl Into<SharedString>) -> Self {
        let text: SharedString = value.into();
        self.text = Rope::from(text.as_str());
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        // Note: We can't call display_map.set_text here because it needs cx.
        // The text will be set during prepare_if_need in element.rs
        self._pending_update = true;
        self
    }

    /// Return the value of the input field.
    pub fn value(&self) -> SharedString {
        SharedString::new(self.text.to_string())
    }

    /// The unmasked value: identical to [`Self::value`] when not masked
    /// (deepterm stores the raw text; masking is a display-time concern).
    pub fn unmask_value(&self) -> SharedString {
        SharedString::new(self.text.to_string())
    }

    /// Return the portion of the value within the input field that
    /// is selected by the user
    pub fn selected_value(&self) -> SharedString {
        SharedString::new(self.selected_text().to_string())
    }

    /// Return the text [`Rope`] of the input field.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// Return the (0-based) [`Position`] of the cursor.
    pub fn cursor_position(&self) -> Position {
        let offset = self.cursor();
        self.text.offset_to_position(offset)
    }

    /// Set (0-based) [`Position`] of the cursor.
    ///
    /// This will move the cursor to the specified line and column, and update the selection range.
    pub fn set_cursor_position(
        &mut self,
        position: impl Into<Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position: Position = position.into();
        let offset = self.text.position_to_offset(&position);

        self.move_to(offset, None, cx);
        self.update_preferred_column();
        self.focus(window, cx);
    }

    /// Focus the input field.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.start(cx);
        });
    }

    pub(crate) fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    pub(crate) fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    pub(crate) fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_single_line() {
            return;
        }
        let offset = self.start_of_line().saturating_sub(1);
        self.select_to(self.previous_boundary(offset), cx);
    }

    pub(crate) fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_single_line() {
            return;
        }
        let offset = (self.end_of_line() + 1).min(self.text.len());
        self.select_to(self.next_boundary(offset), cx);
    }

    pub(crate) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = (0..self.text.len()).into();
        cx.notify();
    }

    pub(crate) fn select_to_start(
        &mut self,
        _: &SelectToStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(0, cx);
    }

    pub(crate) fn select_to_end(
        &mut self,
        _: &SelectToEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = self.text.len();
        self.select_to(end, cx);
    }

    pub(crate) fn select_to_start_of_line(
        &mut self,
        _: &SelectToStartOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.start_of_line();
        self.select_to(offset, cx);
    }

    pub(crate) fn select_to_end_of_line(
        &mut self,
        _: &SelectToEndOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.end_of_line();
        self.select_to(offset, cx);
    }

    pub(crate) fn select_to_previous_word(
        &mut self,
        _: &SelectToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.previous_start_of_word();
        self.select_to(offset, cx);
    }

    pub(crate) fn select_to_next_word(
        &mut self,
        _: &SelectToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.next_end_of_word();
        self.select_to(offset, cx);
    }

    /// Return the start offset of the previous word.
    pub(crate) fn previous_start_of_word(&mut self) -> usize {
        let offset = self.selected_range.start;
        let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
        // FIXME: Avoid to_string
        let left_part = self.text.slice(0..offset).to_string();

        UnicodeSegmentation::split_word_bound_indices(left_part.as_str())
            .rfind(|(_, s)| !s.trim_start().is_empty())
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Return the next end offset of the next word.
    pub(crate) fn next_end_of_word(&mut self) -> usize {
        let offset = self.cursor();
        let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
        let right_part = self.text.slice(offset..self.text.len()).to_string();

        UnicodeSegmentation::split_word_bound_indices(right_part.as_str())
            .find(|(_, s)| !s.trim_start().is_empty())
            .map(|(i, s)| offset + i + s.len())
            .unwrap_or(self.text.len())
    }

    /// Get start of line byte offset of cursor.
    ///
    /// When soft wrap is active, first press goes to visual line start,
    /// second press (already at visual start) goes to logical line start.
    pub(crate) fn start_of_line(&self) -> usize {
        if self.mode.is_single_line() {
            return 0;
        }

        let row = self.text.offset_to_point(self.cursor()).row;
        let logical_start = self.text.line_start_offset(row);

        if self.soft_wrap && self.mode.is_code_editor() {
            let wrap_point = self.display_map.offset_to_wrap_display_point(self.cursor());
            if let Some(line) = self.display_map.lines().get(row)
                && let Some(range) = line.wrapped_lines.get(wrap_point.local_row)
            {
                let visual_start = logical_start + range.start;
                if self.cursor() != visual_start {
                    return visual_start;
                }
            }
        }

        logical_start
    }

    /// Get end of line byte offset of cursor.
    ///
    /// When soft wrap is active, first press goes to visual line end,
    /// second press (already at visual end) goes to logical line end.
    pub(crate) fn end_of_line(&self) -> usize {
        if self.mode.is_single_line() {
            return self.text.len();
        }

        let row = self.text.offset_to_point(self.cursor()).row;
        let logical_start = self.text.line_start_offset(row);
        let logical_end = self.text.line_end_offset(row);

        if self.soft_wrap && self.mode.is_code_editor() {
            let wrap_point = self.display_map.offset_to_wrap_display_point(self.cursor());
            if let Some(line) = self.display_map.lines().get(row)
                && let Some(range) = line.wrapped_lines.get(wrap_point.local_row)
            {
                let visual_end = logical_start + range.end;
                if self.cursor() != visual_end {
                    return visual_end;
                }
            }
        }

        logical_end
    }

    ///
    /// To get current and next line indent, to return more depth one.
    pub(crate) fn indent_of_next_line(&mut self) -> String {
        if self.mode.is_single_line() {
            return "".into();
        }

        let mut current_indent = String::new();
        let mut next_indent = String::new();
        let current_line_start_pos = self.start_of_line();
        let next_line_start_pos = self.end_of_line();
        for c in self.text.slice(current_line_start_pos..).chars() {
            if !c.is_whitespace() {
                break;
            }
            if c == '\n' || c == '\r' {
                break;
            }
            current_indent.push(c);
        }

        for c in self.text.slice(next_line_start_pos..).chars() {
            if !c.is_whitespace() {
                break;
            }
            if c == '\n' || c == '\r' {
                break;
            }
            next_indent.push(c);
        }

        if next_indent.len() > current_indent.len() {
            next_indent
        } else {
            current_indent
        }
    }

    /// 代码编辑器的回车：智能缩进。
    fn enter_with_smart_indent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        let line_start = self.text.line_start_offset(row);
        let before = self.text.slice(line_start..cursor).to_string();
        let base = before[..before.len() - before.trim_start().len()].to_string();
        let next = self.text.slice(cursor..).chars().next();
        let unit = Self::INDENT_UNIT;

        match Self::newline_indent_kind(&before, next) {
            NewlineIndent::BlankBetween => {
                // `{` + 紧跟的配对闭括号 → 在括号对间插入空行，光标停在中间行
                let text = format!("\n{base}{unit}\n{base}");
                self.replace_text_in_range_silent(None, &text, window, cx);
                let new_cursor = cursor + 1 + base.len() + unit.len();
                self.selected_range = (new_cursor..new_cursor).into();
                self.update_preferred_column();
                cx.notify();
            }
            NewlineIndent::Deeper => {
                let text = format!("\n{base}{unit}");
                self.replace_text_in_range_silent(None, &text, window, cx);
            }
            NewlineIndent::Inherit => {
                let indent = self.indent_of_next_line();
                let text = format!("\n{indent}");
                self.replace_text_in_range_silent(None, &text, window, cx);
            }
        }
    }

    /// 括号匹配（纯函数，便于测试）：光标紧邻（前或后）一个括号时，
    /// 返回 (开括号, 闭括号) 的字节偏移。未命中返回 None。
    ///
    /// 仅处理 `()[]{}`；扫描范围限制在前后 [`BRACKET_MATCH_SCAN_LIMIT`] 字节内。
    pub(crate) fn matching_bracket(text: &Rope, cursor: usize) -> Option<(usize, usize)> {
        let len = text.len();
        let mut candidates = Vec::with_capacity(2);
        if cursor > 0 {
            candidates.push(cursor - 1);
        }
        if cursor < len {
            candidates.push(cursor);
        }
        for off in candidates {
            let Some(b) = text.get_byte(off) else {
                continue;
            };
            match b {
                b'(' | b'[' | b'{' => {
                    let close = match b {
                        b'(' => b')',
                        b'[' => b']',
                        _ => b'}',
                    };
                    if let Some(m) =
                        scan_matching_bracket(text, off, false, b, close, BRACKET_MATCH_SCAN_LIMIT)
                    {
                        return Some((off, m));
                    }
                }
                b')' | b']' | b'}' => {
                    let (open, close) = match b {
                        b')' => (b'(', b')'),
                        b']' => (b'[', b']'),
                        _ => (b'{', b'}'),
                    };
                    if let Some(m) =
                        scan_matching_bracket(text, off, true, open, close, BRACKET_MATCH_SCAN_LIMIT)
                    {
                        return Some((m, off));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// 回车缩进决策（纯函数，便于测试）：基于光标前行内容与光标后字符。
    fn newline_indent_kind(before: &str, next: Option<char>) -> NewlineIndent {
        let trimmed_end = before.trim_end();
        let ends_with_open = trimmed_end.ends_with('{')
            || trimmed_end.ends_with('[')
            || trimmed_end.ends_with('(');
        if ends_with_open {
            let open = trimmed_end.chars().last().unwrap();
            if let Some(next) = next
                && Self::AUTO_PAIRS.iter().any(|(o, c)| *o == open && *c == next)
            {
                return NewlineIndent::BlankBetween;
            }
            return NewlineIndent::Deeper;
        }
        // 行尾 `:` 且行内无花括号：Python/YAML/JSON 风格的块并深入
        let ends_with_colon =
            trimmed_end.ends_with(':') && !trimmed_end.contains('{') && !trimmed_end.contains('}');
        if ends_with_colon {
            NewlineIndent::Deeper
        } else {
            NewlineIndent::Inherit
        }
    }

    /// 括号/引号自动配对：在 [`InputState::replace_text_in_range`]（键盘输入路径）
    /// 中拦截单个 ASCII 字符处理。返回 true 表示已消费本次输入。
    fn handle_auto_pair_input(
        &mut self,
        input: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ch) = input.chars().next() else {
            return false;
        };

        // 1) 键入闭括号/引号且光标后紧跟同一字符 → 跳过而非重复插入
        let is_closer = matches!(ch, ')' | ']' | '}' | '"' | '\'' | '`');
        if self.selected_range.is_empty() && is_closer {
            let cursor = self.cursor();
            if self.text.slice(cursor..).chars().next() == Some(ch) {
                self.selected_range = (cursor + 1..cursor + 1).into();
                self.update_preferred_column();
                cx.notify();
                return true;
            }
        }

        // 2) 键入 `}`/`)`/`]` 且当前行只有缩进 → 行首 outdent 一级再插入
        if matches!(ch, '}' | ')' | ']') && self.selected_range.is_empty() {
            let cursor = self.cursor();
            let row = self.text.offset_to_point(cursor).row;
            let line_start = self.text.line_start_offset(row);
            let prefix = self.text.slice(line_start..cursor).to_string();
            if !prefix.is_empty() && prefix.trim().is_empty() {
                let new_prefix = match prefix.strip_suffix('\t') {
                    Some(p) => p.to_string(),
                    None => {
                        // 空格缩进：去掉一个单位（至多 SPACE_OUTDENT_WIDTH 个空格）
                        // 按**字符**裁剪：行首空白可能是全角空格（U+3000）等
                        // 多字节字符，按字节切片会落在字符中间导致 panic。
                        let chars: Vec<char> = prefix.chars().collect();
                        let keep = chars.len().saturating_sub(Self::SPACE_OUTDENT_WIDTH);
                        chars[..keep].iter().collect()
                    }
                };
                let text = format!("{new_prefix}{ch}");
                let range_utf16 = self.range_to_utf16(&(line_start..cursor));
                self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
                return true;
            }
        }

        // 3) 键入开括号/引号 → 自动配对；有选区时包裹选区
        if let Some(&(_, close)) = Self::AUTO_PAIRS.iter().find(|(o, _)| *o == ch) {
            let sel: Range<usize> = self.selected_range.into();
            let selected = self.text.slice(sel.clone()).to_string();
            let quote = ch == close;

            if !selected.is_empty() {
                // 包裹选区：(|sel|)
                let text = format!("{ch}{selected}{close}");
                let range_utf16 = self.range_to_utf16(&sel);
                self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
                let inner = sel.start + 1..sel.start + 1 + selected.len();
                self.selected_range = inner.into();
                self.update_preferred_column();
                cx.notify();
                return true;
            }

            // 空选区：引号只在词边界处配对（避免 don't、a"b 这类场景）
            if quote {
                let wordlike = |c: Option<char>| {
                    c.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false)
                };
                let before = self.text.slice(..sel.start).chars().last();
                let after = self.text.slice(sel.end..).chars().next();
                if wordlike(before)
                    || after.map(|c| wordlike(Some(c)) || c == ch).unwrap_or(false)
                {
                    return false; // 按普通引号插入
                }
            }

            let text = format!("{ch}{close}");
            let range_utf16 = self.range_to_utf16(&sel);
            self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
            self.selected_range = (sel.start + 1..sel.start + 1).into();
            self.update_preferred_column();
            cx.notify();
            return true;
        }

        false
    }

    /// 光标/选区覆盖的逻辑行范围（选区末尾恰在行首时不包含该行）。
    fn selected_logical_rows(&self) -> RangeInclusive<usize> {
        let sel: Range<usize> = self.selected_range.into();
        let start_row = self.text.offset_to_point(sel.start).row;
        let mut end_row = self.text.offset_to_point(sel.end).row;
        if sel.end > sel.start
            && end_row > start_row
            && self.text.line_start_offset(end_row) == sel.end
        {
            end_row -= 1;
        }
        start_row..=end_row
    }

    /// 行 span：[首行行首, 末行下一行行首或文件尾)（含末行行尾换行）。
    fn logical_rows_span(&self, rows: &RangeInclusive<usize>) -> Range<usize> {
        let start = self.text.line_start_offset(*rows.start());
        let end = if *rows.end() < self.text.lines_len().saturating_sub(1) {
            self.text.line_start_offset(*rows.end() + 1)
        } else {
            self.text.len()
        };
        start..end
    }

    /// cmd-/：行注释切换。选区跨行时逐行注释；所有非空行已注释时取消注释。
    pub(crate) fn toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_code_editor() {
            cx.propagate();
            return;
        }
        let Some(token) = Self::line_comment_token(self.language().as_deref()) else {
            return;
        };

        let rows = self.selected_logical_rows();
        let span = self.logical_rows_span(&rows);
        if span.start >= span.end {
            return;
        }

        let span_text = self.text.slice(span.clone()).to_string();
        let raw_lines: Vec<&str> = span_text.split_inclusive('\n').collect();
        let multi = raw_lines.len() > 1;
        let commented = |core: &str| core.trim_start().starts_with(token);
        let all_commented = raw_lines.iter().any(|l| !l.trim().is_empty())
            && raw_lines
                .iter()
                .filter(|l| !l.trim().is_empty())
                .all(|l| commented(l.trim_end_matches(['\n', '\r'])));

        // 逐行重建；记录每行 (orig_start, new_start, delta, insert_point) 以恢复选区
        let mut new_span = String::new();
        let mut orig_offset = span.start;
        let mut line_maps: Vec<(usize, usize, isize, usize)> = Vec::new();
        for raw in &raw_lines {
            let new_line_start = new_span.len();
            let has_nl = raw.ends_with('\n');
            let no_nl = raw.strip_suffix('\n').unwrap_or(raw);
            let (line, has_cr) = match no_nl.strip_suffix('\r') {
                Some(core) => (core, true),
                None => (no_nl, false),
            };

            let (new_core, insert_point) =
                if all_commented && commented(line) {
                    Self::strip_line_comment(line, token)
                } else if line.trim().is_empty() && multi {
                    (line.to_string(), 0)
                } else {
                    Self::add_line_comment(line, token)
                };

            let new_raw_len = new_core.len() + has_cr as usize + has_nl as usize;
            let delta = new_raw_len as isize - raw.len() as isize;
            line_maps.push((orig_offset, new_line_start, delta, insert_point));
            orig_offset += raw.len();
            new_span.push_str(&new_core);
            if has_cr {
                new_span.push('\r');
            }
            if has_nl {
                new_span.push('\n');
            }
        }

        // 选区/光标映射：行内 insert_point 之后的偏移叠加本行 delta，
        // 后续行叠加之前行的累计 delta
        let remap = |offset: usize| -> usize {
            let mut cumulative: isize = 0;
            for (i, (orig_start, new_start, delta, insert_point)) in line_maps.iter().enumerate() {
                let line_len = raw_lines[i].len();
                if offset < *orig_start {
                    break;
                }
                if offset <= *orig_start + line_len {
                    let pos = offset - orig_start;
                    let adjusted = if pos > *insert_point { pos as isize + delta } else { pos as isize };
                    return span.start + new_start + adjusted as usize;
                }
                cumulative += delta;
            }
            (offset as isize + cumulative).max(0) as usize
        };

        let sel: Range<usize> = self.selected_range.into();
        let new_sel = remap(sel.start)..remap(sel.end).max(remap(sel.start));
        let range_utf16 = self.range_to_utf16(&span);
        self.replace_text_in_range_silent(Some(range_utf16), &new_span, window, cx);
        self.selected_range = new_sel.into();
        self.update_preferred_column();
        cx.notify();
    }

    /// alt-up：上移当前行/选区行块。
    pub(crate) fn move_line_up(
        &mut self,
        _: &MoveLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_multi_line() {
            return;
        }
        let rows = self.selected_logical_rows();
        if *rows.start() == 0 {
            return;
        }
        let span = self.logical_rows_span(&rows);
        let prev_start = self.text.line_start_offset(*rows.start() - 1);
        let block = self.text.slice(span.clone()).to_string();
        let prev = self.text.slice(prev_start..span.start).to_string();

        // 交换相邻两个行块：新内容 = block + prev
        let new_text = if block.ends_with('\n') {
            format!("{block}{prev}")
        } else {
            // block 在文件尾无换行：需补分隔换行并去掉 prev 尾部换行
            format!("{block}\n{}", &prev[..prev.len() - 1])
        };
        let range_utf16 = self.range_to_utf16(&(prev_start..span.end));
        self.replace_text_in_range_silent(Some(range_utf16), &new_text, window, cx);
        let sel: Range<usize> = self.selected_range.into();
        let shift = prev.len() as isize;
        let new_sel = ((sel.start as isize - shift).max(0) as usize)
            ..((sel.end as isize - shift).max(0) as usize);
        self.selected_range = new_sel.into();
        self.scroll_to(self.cursor(), None, cx);
        self.update_preferred_column();
        cx.notify();
    }

    /// alt-down：下移当前行/选区行块。
    pub(crate) fn move_line_down(
        &mut self,
        _: &MoveLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_multi_line() {
            return;
        }
        let rows = self.selected_logical_rows();
        let span = self.logical_rows_span(&rows);
        if span.end >= self.text.len() {
            return; // 已是最后一行
        }
        let block = self.text.slice(span.clone()).to_string();
        let below_end = {
            let next_row = *rows.end() + 2; // below 行块的下一行
            if next_row <= self.text.lines_len().saturating_sub(1) {
                self.text.line_start_offset(next_row)
            } else {
                self.text.len()
            }
        };
        let below = self.text.slice(span.end..below_end).to_string();

        // 交换相邻两个行块：新内容 = below + block
        let new_text = if below.ends_with('\n') {
            format!("{below}{block}")
        } else {
            // below 是文件尾无换行的行：补分隔换行并去掉 block 尾部换行
            format!("{below}\n{}", &block[..block.len() - 1])
        };
        let range_utf16 = self.range_to_utf16(&(span.start..below_end));
        self.replace_text_in_range_silent(Some(range_utf16), &new_text, window, cx);
        let sel: Range<usize> = self.selected_range.into();
        let shift = below.len();
        self.selected_range = ((sel.start + shift)..(sel.end + shift)).into();
        self.scroll_to(self.cursor(), None, cx);
        self.update_preferred_column();
        cx.notify();
    }

    /// shift-alt-up/down：向下复制当前行/选区行块（保留原选区）。
    pub(crate) fn duplicate_line(
        &mut self,
        _: &DuplicateLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_multi_line() {
            return;
        }
        let rows = self.selected_logical_rows();
        let span = self.logical_rows_span(&rows);
        if span.start >= span.end {
            return;
        }
        let content = self.text.slice(span.clone()).to_string();
        let insert = if content.ends_with('\n') {
            content.clone()
        } else {
            format!("\n{content}")
        };
        let range_utf16 = self.range_to_utf16(&(span.end..span.end));
        self.replace_text_in_range_silent(Some(range_utf16), &insert, window, cx);
        self.scroll_to(self.cursor(), None, cx);
        cx.notify();
    }

    /// cmd-shift-k：删除当前行/选区行块。
    pub(crate) fn delete_line(
        &mut self,
        _: &DeleteLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_multi_line() {
            return;
        }
        let rows = self.selected_logical_rows();
        let mut span = self.logical_rows_span(&rows);
        if span.start >= span.end {
            return;
        }
        // 删除文件最后的行时连同前一个换行一起删，避免留下尾部空行
        if span.end >= self.text.len() && span.start > 0 {
            span.start -= 1;
        }
        let range_utf16 = self.range_to_utf16(&span);
        self.replace_text_in_range_silent(Some(range_utf16), "", window, cx);
        let cursor = self.cursor().min(self.text.len());
        self.selected_range = (cursor..cursor).into();
        self.scroll_to(cursor, None, cx);
        self.update_preferred_column();
        cx.notify();
    }

    /// cmd-j：合并当前行与下一行（去下一行行首空白，必要时补一个空格）。
    pub(crate) fn join_lines(
        &mut self,
        _: &JoinLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_multi_line() {
            return;
        }
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        if row + 1 > self.text.lines_len().saturating_sub(1) {
            return; // 已是最后一行
        }
        let line_end = self.text.line_end_offset(row);
        let next_end = self.text.line_end_offset(row + 1);
        let next_line = self.text.slice(line_end + 1..next_end).to_string();
        if next_line.trim().is_empty() && next_end >= self.text.len() {
            // 下一行是文件尾空行：仅删换行符
            let range_utf16 = self.range_to_utf16(&(line_end..line_end + 1));
            self.replace_text_in_range_silent(Some(range_utf16), "", window, cx);
            cx.notify();
            return;
        }
        let cur_line = self.text.slice(self.text.line_start_offset(row)..line_end).to_string();
        let trimmed = next_line.trim_start();
        let sep = if cur_line.is_empty() || cur_line.ends_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        let text = format!("{sep}{trimmed}");
        let range_utf16 = self.range_to_utf16(&(line_end..next_end));
        self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
        let new_cursor = line_end + sep.len();
        self.selected_range = (new_cursor..new_cursor).into();
        self.update_preferred_column();
        cx.notify();
    }

    // ==================== Code Folding (ported from zed's fold.rs) ====================

    /// Clamps the cursor out of any folded region, then re-scrolls.
    pub(crate) fn after_fold_change(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let clamped = self.clamp_offset_to_visible_backward(cursor);
        if clamped != cursor {
            self.move_to(clamped, None, cx);
        }
        cx.notify();
    }

    /// The cursor's buffer row, clamped out of folded regions.
    fn clamped_cursor_row(&self) -> usize {
        let cursor = self
            .clamp_offset_to_visible_backward(self.cursor())
            .min(self.text.len());
        self.text.offset_to_point(cursor).row
    }

    /// Port of Zed's `Editor::toggle_fold`: if any fold intersects the current
    /// selection, unfold it; otherwise fold the enclosing crease.
    pub(crate) fn toggle_fold(
        &mut self,
        _: &ToggleFold,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_folding() {
            return;
        }

        let selection = self.selected_range;
        let range = if selection.is_empty() {
            let row = self.clamped_cursor_row();
            let mut end = self
                .text
                .point_to_offset(crate::components::input::Point::new(
                    row,
                    self.text.slice_line(row).len(),
                ));
            // A folded row's display row spans the whole fold (zed converts
            // the selection's display range through the fold map), so a
            // toggle on the fold header must see the fold as intersecting.
            if let Some(fold) = self.display_map.fold_for_row(row) {
                end = end.max(fold.end_offset);
            }
            self.text
                .point_to_offset(crate::components::input::Point::new(row, 0))
                ..end
        } else {
            selection.start..selection.end
        };

        if self.display_map.folds_in_range(range.clone()).next().is_some() {
            self.unfold_lines(cx);
        } else {
            self.fold_selection(cx);
        }
    }

    /// Port of Zed's `Editor::toggle_fold_all`.
    pub(crate) fn toggle_fold_all(
        &mut self,
        _: &ToggleFoldAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_folding() {
            return;
        }
        let has_folds = self
            .display_map
            .folds_in_range(0..self.text.len())
            .next()
            .is_some();
        if has_folds {
            self.unfold_all(&UnfoldAll, window, cx);
        } else {
            self.fold_all(&FoldAll, window, cx);
        }
    }

    /// Port of Zed's `Editor::fold`: fold the creases touched by the selection.
    ///
    /// When the selection spans multiple rows, every crease intersecting the
    /// selected rows is folded; otherwise the innermost crease enclosing the
    /// cursor row is folded.
    pub(crate) fn fold(&mut self, _: &Fold, _: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_folding() {
            return;
        }
        self.fold_selection(cx);
    }

    fn fold_selection(&mut self, cx: &mut Context<Self>) {
        let selection = self.selected_range;
        let (range_start, range_end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        let buffer_start_row = self.text.offset_to_point(range_start).row;
        let range_end_row = self.text.offset_to_point(range_end.min(self.text.len())).row;

        let mut to_fold = Vec::new();

        if range_start != range_end && buffer_start_row != range_end_row {
            let mut found = false;
            let mut row = buffer_start_row;
            while row <= range_end_row {
                if let Some(crease) = self.display_map.crease_for_buffer_row(&self.text, row) {
                    found = true;
                    row = crease.end.row + 1;
                    to_fold.push(crease);
                } else {
                    row += 1;
                }
            }
            if found {
                self.fold_creases(to_fold, cx);
                return;
            }
        }

        for row in (0..=buffer_start_row).rev() {
            if let Some(crease) = self.display_map.crease_for_buffer_row(&self.text, row) {
                if crease.end.row >= buffer_start_row {
                    to_fold.push(crease);
                    if row <= buffer_start_row {
                        break;
                    }
                }
            }
        }

        self.fold_creases(to_fold, cx);
    }

    /// Port of Zed's `Editor::fold_creases`.
    fn fold_creases(&mut self, creases: Vec<Crease>, cx: &mut Context<Self>) {
        if creases.is_empty() {
            return;
        }
        self.display_map.fold_creases(&self.text, &creases);
        self.after_fold_change(cx);
    }

    /// Port of Zed's `Editor::fold_at`: fold the crease starting on `buffer_row`.
    pub(crate) fn fold_at(&mut self, buffer_row: usize, cx: &mut Context<Self>) {
        if !self.mode.is_folding() {
            return;
        }
        if self.display_map.fold_at(&self.text, buffer_row) {
            self.after_fold_change(cx);
        }
    }

    /// Port of Zed's `Editor::unfold_at`: unfold every fold intersecting
    /// `buffer_row`'s line.
    pub(crate) fn unfold_at(&mut self, buffer_row: usize, cx: &mut Context<Self>) {
        if !self.mode.is_folding() {
            return;
        }
        let intersection_range =
            self.text.point_to_offset(crate::components::input::Point::new(buffer_row, 0))
                ..self.text.point_to_offset(crate::components::input::Point::new(
                    buffer_row,
                    self.text.slice_line(buffer_row).len(),
                ));
        if self
            .display_map
            .unfold_ranges(&[intersection_range], true)
        {
            self.after_fold_change(cx);
        }
    }

    /// Port of Zed's `Editor::unfold_lines`: unfold every fold intersecting
    /// the selected rows.
    pub(crate) fn unfold(&mut self, _: &Unfold, _: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_folding() {
            return;
        }
        self.unfold_lines(cx);
    }

    fn unfold_lines(&mut self, cx: &mut Context<Self>) {
        let selection = self.selected_range;
        let (start, end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        let start_row = self.text.offset_to_point(start).row;
        let end_row = self.text.offset_to_point(end.min(self.text.len())).row;
        let range = self
            .text
            .point_to_offset(crate::components::input::Point::new(start_row, 0))
            ..self.text.point_to_offset(crate::components::input::Point::new(
                end_row,
                self.text.slice_line(end_row).len(),
            ));
        if self.display_map.unfold_ranges(&[range], true) {
            self.after_fold_change(cx);
        }
    }

    /// Port of Zed's `Editor::fold_all`: fold every crease in the buffer.
    pub(crate) fn fold_all(&mut self, _: &FoldAll, _: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_folding() {
            return;
        }

        let mut fold_creases = Vec::new();
        for row in 0..self.text.lines_len() {
            if let Some(foldable_range) = self.display_map.crease_for_buffer_row(&self.text, row)
            {
                fold_creases.push(foldable_range);
            }
        }

        self.fold_creases(fold_creases, cx);
    }

    /// Port of Zed's `Editor::unfold_all`.
    pub(crate) fn unfold_all(&mut self, _: &UnfoldAll, _: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_folding() {
            return;
        }
        #[allow(clippy::single_range_in_vec_init)]
        self.display_map.unfold_ranges(&[0..self.text.len()], true);
        cx.notify();
    }

    /// Port of Zed's `Editor::fold_selected_ranges`: fold the selection itself.
    pub(crate) fn fold_selected_ranges(
        &mut self,
        _: &FoldSelectedRanges,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_folding() {
            return;
        }
        let selection = self.selected_range;
        if selection.is_empty() {
            return;
        }
        let (start, end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        #[allow(clippy::single_range_in_vec_init)]
        self.display_map.fold_ranges(&self.text, vec![start..end]);
        self.after_fold_change(cx);
    }

    /// Port of Zed's `Editor::fold_at_level`: fold every crease whose nesting
    /// depth is `level` (1 = outermost), skipping folds that contain a selection.
    pub(crate) fn fold_level(
        &mut self,
        action: &FoldLevel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_folding() {
            return;
        }
        let fold_at_level = action.0;
        if fold_at_level == 0 {
            self.display_map.clear_folds();
            cx.notify();
            return;
        }

        let max_row = self.text.lines_len().saturating_sub(1);
        let mut to_fold = Vec::new();
        let mut stack = vec![(0usize, max_row, 1usize)];

        // Rows covered by the current selection are kept visible
        // (Zed: "Fold iff there is no selection completely contained within
        // the fold region").
        let selection = self.selected_range;
        let (sel_start, sel_end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        let sel_start_row = self.text.offset_to_point(sel_start).row;
        let sel_end_row = self.text.offset_to_point(sel_end.min(self.text.len())).row;
        #[allow(clippy::single_range_in_vec_init)]
        let row_ranges_to_keep: Vec<Range<usize>> = vec![sel_start_row..sel_end_row];

        while let Some((mut start_row, end_row, current_level)) = stack.pop() {
            while start_row < end_row {
                match self
                    .display_map
                    .crease_for_buffer_row(&self.text, start_row)
                {
                    Some(crease) => {
                        let nested_start_row = crease.start.row + 1;
                        let nested_end_row = crease.end.row;

                        if current_level < fold_at_level {
                            stack.push((nested_start_row, nested_end_row, current_level + 1));
                        } else if current_level == fold_at_level {
                            // Fold iff there is no selection completely contained
                            // within the fold region
                            if !row_ranges_to_keep.iter().any(|selection| {
                                selection.end >= nested_start_row
                                    && selection.start <= nested_end_row
                            }) {
                                to_fold.push(crease);
                            }
                        }

                        start_row = nested_end_row + 1;
                    }
                    None => start_row += 1,
                }
            }
        }

        self.fold_creases(to_fold, cx);
    }

    pub(crate) fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        // 代码编辑器：空选区时光标夹在空括号/引号对中间 → 一并删除
        if self.selected_range.is_empty()
            && self.mode.is_code_editor()
            && self.ime_marked_range.is_none()
        {
            let cursor = self.cursor();
            let prev = self.text.slice(..cursor).chars().last();
            let next = self.text.slice(cursor..).chars().next();
            if let (Some(p), Some(n)) = (prev, next)
                && Self::AUTO_PAIRS.iter().any(|(o, c)| (*o, *c) == (p, n))
            {
                let range = cursor - p.len_utf8()..cursor + n.len_utf8();
                let range_utf16 = self.range_to_utf16(&range);
                self.replace_text_in_range_silent(Some(range_utf16), "", window, cx);
                self.pause_blink_cursor(cx);
                return;
            }
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor()), cx)
        }
        self.replace_text_in_range(None, "", window, cx);
        self.pause_blink_cursor(cx);
    }

    pub(crate) fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor()), cx)
        }
        self.replace_text_in_range(None, "", window, cx);
        self.pause_blink_cursor(cx);
    }

    pub(crate) fn delete_to_beginning_of_line(
        &mut self,
        _: &DeleteToBeginningOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let mut offset = self.start_of_line();
        if offset == self.cursor() {
            offset = offset.saturating_sub(1);
        }
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(offset..self.cursor()))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(crate) fn delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let mut offset = self.end_of_line();
        if offset == self.cursor() {
            offset = (offset + 1).clamp(0, self.text.len());
        }
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(self.cursor()..offset))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(crate) fn delete_previous_word(
        &mut self,
        _: &DeleteToPreviousWordStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let offset = self.previous_start_of_word();
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(offset..self.cursor()))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(crate) fn delete_next_word(
        &mut self,
        _: &DeleteToNextWordEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let offset = self.next_end_of_word();
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(self.cursor()..offset))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(crate) fn enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_multi_line() {
            if self.mode.is_code_editor() {
                self.enter_with_smart_indent(window, cx);
            } else {
                self.replace_text_in_range_silent(None, "\n", window, cx);
            }
            self.pause_blink_cursor(cx);
        } else if self.submit_on_enter {
            // Single line input with submit_on_enter: emit the submit event.
            self.clean_on_escape = false;
            cx.emit(InputEvent::Submit);
        } else {
            // Single line input, just emit the event (e.g.: In a dialog to confirm).
            cx.propagate();
        }

        cx.emit(InputEvent::PressEnter {
            secondary: action.secondary,
        });
    }

    /// 代码编辑器中 Tab / Shift+Tab 的缩进单元：
    /// 统一使用 Tab 字符（Makefile 等对制表符敏感的格式依赖它；
    /// 后续若引入软缩进配置可替换此处）。
    const INDENT_UNIT: &'static str = "\t";
    /// 无 Tab 风格（空格缩进）下 outdent 一次最多删除的空格数。
    const SPACE_OUTDENT_WIDTH: usize = 4;

    pub(crate) fn indent(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.indent_or_outdent_lines(false, window, cx);
    }

    pub(crate) fn outdent(&mut self, _: &Outdent, window: &mut Window, cx: &mut Context<Self>) {
        self.indent_or_outdent_lines(true, window, cx);
    }

    pub(crate) fn indent_inline(
        &mut self,
        _: &IndentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.indent_or_outdent_lines(false, window, cx);
    }

    pub(crate) fn outdent_inline(
        &mut self,
        _: &OutdentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.indent_or_outdent_lines(true, window, cx);
    }

    /// 行首一个缩进单元的长度（字节）：优先 Tab，其次最多 4 个空格。
    fn outdent_unit_len(line: &str) -> usize {
        let bytes = line.as_bytes();
        if bytes.first() == Some(&b'\t') {
            1
        } else {
            bytes
                .iter()
                .take_while(|&&b| b == b' ')
                .take(Self::SPACE_OUTDENT_WIDTH)
                .count()
        }
    }

    /// Tab / Shift+Tab / Cmd+] / Cmd+[ 的核心实现。
    ///
    /// - 无选区（光标）：Tab 在光标处插入缩进单元；Shift+Tab 删除当前行行首一个缩进单元。
    /// - 有选区：对选区涉及的所有行整体缩进 / 反缩进，选区范围按实际插入/删除量平移。
    fn indent_or_outdent_lines(
        &mut self,
        outdent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled || !self.mode.is_multi_line() {
            return;
        }
        self.pause_blink_cursor(cx);

        let sel = self.selected_range;
        let (start, end) = if sel.start <= sel.end {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };

        // 无选区：仅光标位置操作。
        if start == end {
            if outdent {
                self.outdent_cursor_line(start, window, cx);
            } else {
                self.replace_text_in_range_silent(None, Self::INDENT_UNIT, window, cx);
            }
            return;
        }

        // 多行/行内选区：定位涉及的行范围。
        let start_row = self.text.offset_to_point(start).row;
        let end_point = self.text.offset_to_point(end);
        let mut last_row = end_point.row;
        // 选区恰好结束在「某行行首」（多行选择）时，最后一行不属于选区。
        if last_row > start_row && self.text.line_start_offset(last_row) == end {
            last_row -= 1;
        }

        let line_start = self.text.line_start_offset(start_row);
        let line_end = self.text.line_end_offset(last_row);

        let mut new_text = String::new();
        let mut removed = Vec::with_capacity(last_row - start_row + 1);
        for row in start_row..=last_row {
            let line = self.text.slice_line(row).to_string();
            if outdent {
                let n = Self::outdent_unit_len(&line);
                removed.push(n);
                new_text.push_str(&line[n..]);
            } else {
                new_text.push_str(Self::INDENT_UNIT);
                new_text.push_str(&line);
            }
            if row != last_row {
                new_text.push('\n');
            }
        }

        let range_utf16 = self.range_to_utf16(&(line_start..line_end));
        self.replace_text_in_range(Some(range_utf16), &new_text, window, cx);

        // 恢复选区：按每行行首的插入/删除量平移开始与结束。
        let (new_start, new_end) = if outdent {
            let edits: Vec<(usize, usize)> = (start_row..=last_row)
                .zip(removed.iter().copied())
                .map(|(row, n)| (self.text.line_start_offset(row), n))
                .collect();
            (
                shifted_outdent_offset(start, &edits),
                shifted_outdent_offset(end, &edits),
            )
        } else {
            (start + 1, end + (last_row - start_row + 1))
        };
        self.selected_range = Selection::new(new_start, new_end);
        self.update_preferred_column();
    }

    /// Shift+Tab：删除光标所在行行首的一个缩进单元，光标按删除量回退。
    fn outdent_cursor_line(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self.text.offset_to_point(offset).row;
        let line_start = self.text.line_start_offset(row);
        let line = self.text.slice_line(row).to_string();
        let n = Self::outdent_unit_len(&line);
        if n == 0 {
            return;
        }
        let range_utf16 = self.range_to_utf16(&(line_start..line_start + n));
        self.replace_text_in_range(Some(range_utf16), "", window, cx);
        // 光标若在被删除区间内（含行首），钳制到新行首；否则回退删除量。
        let new_cursor = if offset >= line_start + n {
            offset - n
        } else {
            line_start
        };
        self.selected_range = Selection::new(new_cursor, new_cursor);
        self.update_preferred_column();
    }

    pub(crate) fn clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text("", window, cx);
        self.selected_range = (0..0).into();
        self.scroll_to(0, None, cx);
    }

    pub(crate) fn escape(&mut self, _action: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        

        if self.ime_marked_range.is_some() {
            self.unmark_text(window, cx);
        }

        if self.clean_on_escape {
            return self.clean(window, cx);
        }

        cx.propagate();
    }

    pub(crate) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ime_marked_range) = &self.ime_marked_range {
            if ime_marked_range.is_empty() {
                self.ime_marked_range = None;
            }
        }

        if event.button == MouseButton::Left {
            if let Some((start_line, _)) = self
                .fold_click_targets
                .iter()
                .find(|(_, bounds)| bounds.contains(&event.position))
            {
                let start_line = *start_line;
                if self.mode.is_folding() {
                    // Zed's gutter Disclosure / fold placeholder behavior:
                    // clicking a folded row unfolds it; clicking an unfolded
                    // foldable row folds it.
                    if self.display_map.is_line_folded(start_line) {
                        self.unfold_at(start_line, cx);
                    } else {
                        self.fold_at(start_line, cx);
                    }
                }
                return;
            }
        }

        self.selecting = true;
        let offset = self.index_for_mouse_position(event.position);

        if event.button == MouseButton::Right {
            return;
        }

        match event.click_count {
            1 => {
                if event.modifiers.shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, None, cx);
                    self.selected_word_range = None;
                }
            }
            2 => {
                self.select_word_at(offset, cx);
            }
            3 => {
                self.selected_word_range = None;
                self.select_line_at(offset, cx);
            }
            _ => {
                if event.modifiers.shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, None, cx)
                }
            }
        }
    }

    pub(crate) fn select_word_at(&mut self, offset: usize, cx: &mut Context<Self>) {
        if let Some(range) = self.text.word_range(offset) {
            self.selected_word_range = Some(range.clone().into());
            self.selected_range = range.into();
            cx.notify();
        }
    }

    pub(crate) fn select_line_at(&mut self, offset: usize, cx: &mut Context<Self>) {
        if let Some(range) = self.text.line_range(offset) {
            self.selected_range = range.into();
            cx.notify();
        }
    }

    pub(crate) fn on_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.selection_reversed = false;
        }
        self.selecting = false;
        self.selected_word_range = None;
    }

    pub(crate) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self
            .last_layout
            .as_ref()
            .map(|layout| layout.line_height)
            .unwrap_or(window.line_height());
        let delta = event.delta.pixel_delta(line_height);

        let old_offset = self.scroll_handle.offset();
        self.update_scroll_offset(Some(old_offset + delta), cx);

        // Only stop propagation if the offset actually changed
        if self.scroll_handle.offset() != old_offset {
            cx.stop_propagation();
        }
    }

    pub(crate) fn update_scroll_offset(
        &mut self,
        offset: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let mut offset = offset.unwrap_or(self.scroll_handle.offset());
        // In addition to left alignment, a cursor position will be reserved on the right side
        let safe_x_offset = if self.text_align == TextAlign::Left {
            px(0.)
        } else {
            -CURSOR_WIDTH
        };

        let safe_y_range =
            (-self.scroll_size.height + self.input_bounds.size.height).min(px(0.0))..px(0.);
        let safe_x_range = (-self.scroll_size.width + self.input_bounds.size.width + safe_x_offset)
            .min(safe_x_offset)..px(0.);

        offset.y = if self.mode.is_single_line() {
            px(0.)
        } else {
            offset.y.clamp(safe_y_range.start, safe_y_range.end)
        };
        offset.x = offset.x.clamp(safe_x_range.start, safe_x_range.end);
        self.scroll_handle.set_offset(offset);
        cx.notify();
    }

    /// Scroll to make the given offset visible.
    ///
    /// If `direction` is Some, will keep edges at the same side.
    pub(crate) fn scroll_to(
        &mut self,
        offset: usize,
        direction: Option<MoveDirection>,
        cx: &mut Context<Self>,
    ) {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return;
        };
        let Some(bounds) = self.last_bounds.as_ref() else {
            return;
        };

        let mut scroll_offset = self.scroll_handle.offset();
        let was_offset = scroll_offset;
        let line_height = last_layout.line_height;

        let point = self.text.offset_to_point(offset);

        let row = point.row;

        let mut row_offset_y = px(0.);
        for (ix, _wrap_line) in self.display_map.lines().iter().enumerate() {
            if ix == row {
                break;
            }

            // Only accumulate height for visible (non-folded) wrap rows
            let visible_wrap_rows = self.display_map.visible_wrap_row_count_for_buffer_line(ix);
            row_offset_y += line_height * visible_wrap_rows;
        }

        // Apart from left alignment, just leave enough space for the cursor size on the right side.
        let safety_margin = if last_layout.text_align == TextAlign::Left {
            RIGHT_MARGIN
        } else {
            CURSOR_WIDTH
        };
        if let Some(line) = last_layout
            .lines
            .get(row.saturating_sub(last_layout.visible_range.start))
        {
            // Check to scroll horizontally and soft wrap lines
            if let Some(pos) = line.position_for_index(point.column, last_layout, false) {
                let bounds_width = bounds.size.width - last_layout.line_number_width;
                let col_offset_x = pos.x;
                row_offset_y += pos.y;
                if col_offset_x - safety_margin < -scroll_offset.x {
                    // If the position is out of the visible area, scroll to make it visible
                    scroll_offset.x = -col_offset_x + safety_margin;
                } else if col_offset_x + safety_margin > -scroll_offset.x + bounds_width {
                    scroll_offset.x = -(col_offset_x - bounds_width + safety_margin);
                }
            }
        }

        // Check if row_offset_y is out of the viewport
        // If row offset is not in the viewport, scroll to make it visible.
        let edge_height = if direction.is_some() && self.mode.is_code_editor() {
            match self.cursor_surrounding_lines {
                Some(lines) => (lines as f32 * line_height).min(
                    (last_layout.visible_range.len() as f32 * line_height).half(),
                ),
                None => 3 * line_height,
            }
        } else {
            line_height
        };
        if row_offset_y - edge_height + line_height < -scroll_offset.y {
            // Scroll up
            scroll_offset.y = -row_offset_y + edge_height - line_height;
        } else if row_offset_y + edge_height > -scroll_offset.y + bounds.size.height {
            // Scroll down
            scroll_offset.y = -(row_offset_y - bounds.size.height + edge_height);
        }

        // Avoid necessary scroll, when it was already in the correct position.
        if direction == Some(MoveDirection::Up) {
            scroll_offset.y = scroll_offset.y.max(was_offset.y);
        } else if direction == Some(MoveDirection::Down) {
            scroll_offset.y = scroll_offset.y.min(was_offset.y);
        }

        scroll_offset.x = scroll_offset.x.min(px(0.));
        scroll_offset.y = scroll_offset.y.min(px(0.));
        self.deferred_scroll_offset = Some(scroll_offset);
        cx.notify();
    }

    /// 滚动使给定偏移所在行垂直居中（搜索匹配跳转用）。
    ///
    /// 与 [`Self::scroll_to`] 的区别：不保留边缘可见性，而是把目标行
    /// 对齐到视口中部；水平方向沿用同样的「保证可见」逻辑。
    pub(crate) fn scroll_to_center(&mut self, offset: usize, cx: &mut Context<Self>) {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return;
        };
        let Some(bounds) = self.last_bounds.as_ref() else {
            return;
        };

        let line_height = last_layout.line_height;
        let point = self.text.offset_to_point(offset);
        let row = point.row;

        let mut row_offset_y = px(0.);
        for (ix, _wrap_line) in self.display_map.lines().iter().enumerate() {
            if ix == row {
                break;
            }

            // Only accumulate height for visible (non-folded) wrap rows
            let visible_wrap_rows = self.display_map.visible_wrap_row_count_for_buffer_line(ix);
            row_offset_y += line_height * visible_wrap_rows;
        }

        let mut scroll_offset = self.scroll_handle.offset();

        // 行内列偏移：在前面各行总高度之上，同时供下方水平可见性逻辑复用
        // （与 scroll_to 不同，这里只需查询一次）。
        let pos = last_layout
            .lines
            .get(row.saturating_sub(last_layout.visible_range.start))
            .and_then(|line| line.position_for_index(point.column, last_layout, false));
        if let Some(pos) = pos {
            row_offset_y += pos.y;
        }

        // 垂直：目标行中心对齐视口中心。
        scroll_offset.y = -(row_offset_y + line_height.half() - bounds.size.height.half());

        // 水平：与 scroll_to 相同，仅保证目标列可见。
        if let Some(pos) = pos {
            let safety_margin = if last_layout.text_align == TextAlign::Left {
                RIGHT_MARGIN
            } else {
                CURSOR_WIDTH
            };
            let bounds_width = bounds.size.width - last_layout.line_number_width;
            if pos.x - safety_margin < -scroll_offset.x {
                scroll_offset.x = -pos.x + safety_margin;
            } else if pos.x + safety_margin > -scroll_offset.x + bounds_width {
                scroll_offset.x = -(pos.x - bounds_width + safety_margin);
            }
        }

        scroll_offset.x = scroll_offset.x.min(px(0.));
        scroll_offset.y = scroll_offset.y.min(px(0.));
        self.deferred_scroll_offset = Some(scroll_offset);
        cx.notify();
    }

    pub(crate) fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    pub(crate) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // 仅在多行编辑器（代码编辑器）中，无选区时复制整行（与 Zed 一致）；
        // 单行输入框保持系统直觉：无选区不做事。
        let selected_text = if self.selected_range.is_empty() && self.mode.is_multi_line() {
            line_text_for_clipboard(&self.text, self.cursor()).0
        } else if self.selected_range.is_empty() {
            return;
        } else {
            self.text.slice(self.selected_range).to_string()
        };
        cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
    }

    pub(crate) fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            if !self.mode.is_multi_line() {
                return;
            }
            // 无选区：剪切当前整行（含换行），与 Zed 的 cut_common 一致。
            let (selected_text, range) = line_text_for_clipboard(&self.text, self.cursor());
            cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
            let range_utf16 = self.range_to_utf16(&range);
            self.replace_text_in_range_silent(Some(range_utf16), "", window, cx);
            // 删除后光标落在 range.start（上一行末尾 / 文档开头 / 末尾），
            // 由 replace_text_in_range 统一设置。
        } else {
            let selected_text = self.text.slice(self.selected_range).to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(selected_text));

            self.replace_text_in_range_silent(None, "", window, cx);
        }
    }

    pub(crate) fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(clipboard) = cx.read_from_clipboard() {
            let mut new_text = clipboard.text().unwrap_or_default();
            if !self.mode.is_multi_line() {
                new_text = new_text.replace('\n', "");
            }

            self.replace_text_in_range_silent(None, &new_text, window, cx);
            self.scroll_to(self.cursor(), None, cx);
        }
    }

    fn push_history(&mut self, text: &Rope, range: &Range<usize>, new_text: &str) {
        if self.history.ignore {
            return;
        }

        let range =
            text.clip_offset(range.start, Bias::Left)..text.clip_offset(range.end, Bias::Right);
        let old_text = text.slice(range.clone()).to_string();
        let new_range = range.start..range.start + new_text.len();

        self.history
            .push(Change::new(range, &old_text, new_range, new_text));
    }

    pub(crate) fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.undo() {
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.new_range);
                self.replace_text_in_range_silent(Some(range_utf16), &change.old_text, window, cx);
            }
        }
        self.history.ignore = false;
    }

    pub(crate) fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.redo() {
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.old_range);
                self.replace_text_in_range_silent(Some(range_utf16), &change.new_text, window, cx);
            }
        }
        self.history.ignore = false;
    }

    /// Get byte offset of the cursor.
    ///
    /// The offset is the UTF-8 offset.
    pub fn cursor(&self) -> usize {
        if let Some(ime_marked_range) = &self.ime_marked_range {
            return ime_marked_range.end;
        }

        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(crate) fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        // If the text is empty, always return 0
        if self.text.len() == 0 {
            return 0;
        }

        let (Some(bounds), Some(last_layout)) =
            (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };

        let line_height = last_layout.line_height;
        let line_number_width = last_layout.line_number_width;

        // TIP: About the IBeam cursor
        //
        // If cursor style is IBeam, the mouse mouse position is in the middle of the cursor (This is special in OS)

        // The position is relative to the bounds of the text input
        //
        // bounds.origin:
        //
        // - included the input padding.
        // - included the scroll offset.
        let inner_position = position - bounds.origin - point(line_number_width, px(0.));

        let mut y_offset = last_layout.visible_top;

        // Traverse visible buffer lines (compact, no hidden entries)
        for (vi, (line_layout, _buffer_line)) in last_layout
            .lines
            .iter()
            .zip(last_layout.visible_buffer_lines.iter())
            .enumerate()
        {
            let line_start_offset = last_layout.visible_line_byte_offsets[vi];

            // Calculate line origin for this display row
            let line_origin = point(px(0.), y_offset);
            let pos = inner_position - line_origin;

            // Return offset by use closest_index_for_x if is single line mode.
            if self.mode.is_single_line() {
                let local_index = line_layout.closest_index_for_x(pos.x, last_layout);
                let index = line_start_offset + local_index;
                return if self.masked {
                    self.text.char_index_to_offset(index / MASK_CHAR.len_utf8())
                } else {
                    index.min(self.text.len())
                };
            }

            // Check if mouse is in this line's bounds
            if let Some(local_index) = line_layout.closest_index_for_position(pos, last_layout) {
                let index = line_start_offset + local_index;
                return if self.masked {
                    self.text.char_index_to_offset(index / MASK_CHAR.len_utf8())
                } else {
                    index.min(self.text.len())
                };
            } else if pos.y < px(0.) {
                // Mouse is above this line, return start of this line
                return if self.masked {
                    self.text
                        .char_index_to_offset(line_start_offset / MASK_CHAR.len_utf8())
                } else {
                    line_start_offset
                };
            }

            y_offset += line_layout.size(line_height).height;
        }

        // Mouse is below all visible lines, return end of text
        self.text.len()
    }

    /// Returns a y offsetted point for the line origin.
    /// Select the text from the current cursor position to the given offset.
    ///
    /// The offset is the UTF-8 offset.
    ///
    /// Ensure the offset use self.next_boundary or self.previous_boundary to get the correct offset.
    pub(crate) fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.clamp(0, self.text.len());
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };

        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = (self.selected_range.end..self.selected_range.start).into();
        }

        // Ensure keep word selected range
        if let Some(word_range) = self.selected_word_range.as_ref() {
            if self.selected_range.start > word_range.start {
                self.selected_range.start = word_range.start;
            }
            if self.selected_range.end < word_range.end {
                self.selected_range.end = word_range.end;
            }
        }
        if self.selected_range.is_empty() {
            self.update_preferred_column();
        }
        cx.notify()
    }

    /// Unselects the currently selected text.
    pub fn unselect(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.cursor();
        self.selected_range = (offset..offset).into();
        cx.notify()
    }

    #[inline]
    pub(crate) fn offset_from_utf16(&self, offset: usize) -> usize {
        self.text.offset_utf16_to_offset(offset)
    }

    #[inline]
    pub(crate) fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text.offset_to_offset_utf16(offset)
    }

    #[inline]
    pub(crate) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    #[inline]
    pub(crate) fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// If the offset falls inside a fold's hidden range, clamp backward to the
    /// fold's start (the end of the fold header line — the last visible
    /// position before the `⋯` placeholder).
    fn clamp_offset_to_visible_backward(&self, offset: usize) -> usize {
        match self.display_map.fold_containing(offset) {
            Some(fold) => fold.start_offset,
            None => offset,
        }
    }

    /// If the offset falls inside a fold's hidden range, clamp forward to the
    /// fold's end (the start of the visible trailing text, e.g. the `}`).
    fn clamp_offset_to_visible_forward(&self, offset: usize) -> usize {
        match self.display_map.fold_containing(offset) {
            Some(fold) => fold.end_offset,
            None => offset,
        }
    }

    pub(crate) fn previous_boundary(&self, offset: usize) -> usize {
        let mut offset = self.text.clip_offset(offset.saturating_sub(1), Bias::Left);
        if let Some(ch) = self.text.char_at(offset) {
            if ch == '\r' {
                offset -= 1;
            }
        }
        self.clamp_offset_to_visible_backward(offset)
    }

    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        let mut offset = self.text.clip_offset(offset + 1, Bias::Right);
        if let Some(ch) = self.text.char_at(offset) {
            if ch == '\r' {
                offset += 1;
            }
        }
        self.clamp_offset_to_visible_forward(offset)
    }

    /// Returns the true to let InputElement to render cursor, when Input is focused and current BlinkCursor is visible.
    pub(crate) fn show_cursor(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.is_focused(window)
            && !self.disabled
            && self.blink_cursor.read(cx).visible()
            && window.is_window_active()
    }

    fn on_focus(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.start(cx);
        });
        cx.emit(InputEvent::Focus);
    }

    fn on_blur(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // NOTE: Do not cancel select, when blur.
        // Because maybe user want to copy the selected text by AppMenuBar (will take focus handle).

        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.stop(cx);
        });
        cx.emit(InputEvent::Blur);
        cx.notify();
    }

    pub(crate) fn pause_blink_cursor(&mut self, cx: &mut Context<Self>) {
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.pause(cx);
        });
    }

    pub(crate) fn on_key_down(
        &mut self,
        _event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pause_blink_cursor(cx);
    }

    pub(crate) fn on_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text.len() == 0 {
            return;
        }

        if self.last_layout.is_none() {
            return;
        }

        if !self.focus_handle.is_focused(window) {
            return;
        }

        if !self.selecting {
            return;
        }

        let offset = self.index_for_mouse_position(event.position);
        self.select_to(offset, cx);
    }

    fn is_valid_input(&self, new_text: &str, cx: &mut Context<Self>) -> bool {
        if new_text.is_empty() {
            return true;
        }

        if let Some(validate) = &self.validate {
            if !validate(new_text, cx) {
                return false;
            }
        }

        let Some(pattern) = &self.pattern else {
            return true;
        };

        pattern.is_match(new_text)
    }

    pub(crate) fn set_input_bounds(&mut self, new_bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        let wrap_width_changed = self.input_bounds.size.width != new_bounds.size.width;
        self.input_bounds = new_bounds;

        // Update display_map wrap_width if changed.
        if let Some(last_layout) = self.last_layout.as_ref() {
            if wrap_width_changed {
                let wrap_width = if !self.soft_wrap {
                    // None to disable wrapping (will use Pixels::MAX)
                    None
                } else {
                    last_layout.wrap_width
                };

                self.display_map.on_layout_changed(wrap_width, cx);
                self.mode.update_auto_grow(&self.display_map);
                cx.notify();
            }
        }
    }

    pub(crate) fn selected_text(&self) -> RopeSlice<'_> {
        let range_utf16 = self.range_to_utf16(&self.selected_range.into());
        let range = self.range_from_utf16(&range_utf16);
        self.text.slice(range)
    }

    /// Return the rendered bounds for a UTF-8 byte range in the current input contents.
    ///
    /// Returns `None` when the requested range is not currently laid out or visible.
    pub fn range_to_bounds(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let last_bounds = self.last_bounds?;

        let (_, _, start_pos) = self.line_and_position_for_offset(range.start);
        let (_, _, end_pos) = self.line_and_position_for_offset(range.end);

        let start_pos = start_pos?;
        let end_pos = end_pos?;

        Some(Bounds::from_corners(
            last_bounds.origin + start_pos,
            last_bounds.origin + end_pos + point(px(0.), last_layout.line_height),
        ))
    }

    /// Replace text in range in silent.
    pub(crate) fn replace_text_in_range_silent(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 程序化插入（undo/redo、粘贴、缩进、行操作等）不触发自动配对：
        // 否则 redo 回放单字符 "(" 会再次配对成 "()"，与原始键入结果不一致。
        self.replace_text_in_range_inner(range_utf16, new_text, false, window, cx);
    }

    /// `replace_text_in_range` 的实现体。
    ///
    /// `allow_auto_pair` 区分「用户键入」与「程序化插入」：
    /// 只有真实键入单个 ASCII 字符时才走括号/引号自动配对，undo/redo、
    /// 粘贴、缩进与行操作一律绕过，保证回放结果与原始编辑一致。
    fn replace_text_in_range_inner(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        allow_auto_pair: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }

        self.pause_blink_cursor(cx);

        // 代码编辑器：括号/引号自动配对、跳过与 outdent。
        // 仅拦截「单次键入的 ASCII 字符」；IME 组字、粘贴、程序化插入不受影响。
        if allow_auto_pair
            && self.mode.is_code_editor()
            && self.ime_marked_range.is_none()
            && new_text.len() == 1
            && new_text.is_ascii()
            && self.handle_auto_pair_input(new_text, window, cx)
        {
            return;
        }

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.ime_marked_range.map(|range| {
                let range = self.range_to_utf16(&(range.start..range.end));
                self.range_from_utf16(&range)
            }))
            .unwrap_or(self.selected_range.into());

        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        let new_offset = (range.start + new_text.len()).min(self.text.len());

        if self.mode.is_single_line() {
            let pending_text = self.text.to_string();
            // Check if the new text is valid
            if !self.is_valid_input(&pending_text, cx) {
                self.text = old_text;
                return;
            }
        }

        self.push_history(&old_text, &range, new_text);
        self.history.end_grouping();
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        // Adjust folds before updating wrap map (zed's offset-based model:
        // edits inside a fold keep it; boundary-overlapping edits drop it).
        self.display_map.adjust_folds_for_edit(&range, new_text);
        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        let bg = self
            .mode
            .update_highlighter(&range, &self.text, new_text, true, cx);
        if let Some(bg) = bg {
            Self::dispatch_background_parse(bg, self.text_version, window, cx);
        }

        self.selected_range = (new_offset..new_offset).into();
        self.ime_marked_range.take();
        self.update_preferred_column();
        self.mode.update_auto_grow(&self.display_map);
        self.text_version = self.text_version.wrapping_add(1);
        self.update_search(cx);
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    /// Spawn a background parse after the synchronous parse timed out.
    ///
    /// Dropping the returned `Task` (stored in `parse_task`) cancels the
    /// parse, which naturally debounces rapid edits.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn dispatch_background_parse(
        pending: super::mode::PendingBackgroundParse,
        text_version: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let highlighter_rc = pending.highlighter;
        let parse_task_rc = pending.parse_task;
        let language = pending.language;
        let text = pending.text;

        let old_tree = highlighter_rc
            .borrow()
            .as_ref()
            .and_then(|h| h.tree().cloned());

        // Extract injection parse data on the main thread before spawning, so that
        // compute_injection_layers can also run on the background thread.
        let injection_data = highlighter_rc
            .borrow()
            .as_ref()
            .and_then(|h| h.injection_parse_data());

        // The text version this parse was started for. When the parse
        // finishes, if the document has changed meanwhile (the user kept
        // typing), the result is discarded instead of being applied: applying
        // a stale tree would repaint the whole file's highlights, which reads
        // as constant flicker on every keystroke (the fresh parse for the new
        // text is already scheduled by the edit).
        let parse_version = text_version;
        let text_for_apply = text.clone();
        let task = cx.spawn_in(window, async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let config = LanguageRegistry::singleton().language(&language)?;

                    let mut parser = tree_sitter::Parser::new();
                    if parser.set_language(&config.language).is_err() {
                        return None;
                    }

                    let new_tree = parser.parse_with_options(
                        &mut |offset, _| {
                            if offset >= text.len() {
                                &[] as &[u8]
                            } else {
                                let (chunk, chunk_byte_ix) = text.chunk(offset);
                                let start_in_chunk = offset - chunk_byte_ix;
                                chunk.as_bytes().get(start_in_chunk..).unwrap_or(&[])
                            }
                        },
                        old_tree.as_ref(),
                        None,
                    )?;

                    // Compute injection layers in the background to avoid blocking the
                    // main thread with combined-injection parsing (e.g. PHP, HTML+JS/CSS).
                    let injection_layers = if let Some(data) = injection_data {
                        crate::editor::syntax::SyntaxHighlighter::compute_injection_layers(
                            data, &new_tree, &text,
                        )
                    } else {
                        Default::default()
                    };

                    Some((new_tree, injection_layers))
                })
                .await;

            if let Some((new_tree, injection_layers)) = result {
                let stale = entity
                    .update(cx, |state, _| state.text_version != parse_version)
                    .unwrap_or(true);
                if stale {
                    // Document changed while parsing; the fresh parse for the
                    // current text is already scheduled. Do not apply a stale
                    // tree (would repaint the whole file's highlights).
                    return;
                }
                if let Some(h) = highlighter_rc.borrow_mut().as_mut() {
                    h.apply_background_tree(new_tree, &text_for_apply, injection_layers);
                }

                // Trigger re-render so the new highlights are displayed.
                _ = entity.update(cx, |_, cx| {
                    cx.notify();
                });
            }
        });

        parse_task_rc.borrow_mut().replace(task);
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn dispatch_background_parse(
        _pending: super::mode::PendingBackgroundParse,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // No-op
    }
}

impl EntityInputHandler for InputState {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        adjusted_range.replace(self.range_to_utf16(&range));
        Some(self.text.slice(range).to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range.into()),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.ime_marked_range
            .map(|range| self.range_to_utf16(&range.into()))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.ime_marked_range = None;
    }

    /// Replace text in range.
    ///
    /// - If the new text is invalid, it will not be replaced.
    /// - If `range_utf16` is not provided, the current selected range will be used.
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_in_range_inner(range_utf16, new_text, true, window, cx);
    }

    /// Mark text is the IME temporary insert on typing.
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.ime_marked_range.map(|range| {
                let range = self.range_to_utf16(&(range.start..range.end));
                self.range_from_utf16(&range)
            }))
            .unwrap_or(self.selected_range.into());

        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        if self.mode.is_single_line() {
            let pending_text = self.text.to_string();
            if !self.is_valid_input(&pending_text, cx) {
                self.text = old_text;
                return;
            }
        }

        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        // Adjust folds before updating wrap map (zed's offset-based model:
        // edits inside a fold keep it; boundary-overlapping edits drop it).
        self.display_map.adjust_folds_for_edit(&range, new_text);
        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        let bg = self
            .mode
            .update_highlighter(&range, &self.text, new_text, true, cx);
        if let Some(bg) = bg {
            Self::dispatch_background_parse(bg, self.text_version, window, cx);
        }

        if new_text.is_empty() {
            // Cancel selection, when cancel IME input.
            self.selected_range = (range.start..range.start).into();
            self.ime_marked_range = None;
        } else {
            self.ime_marked_range = Some((range.start..range.start + new_text.len()).into());
            // `new_selected_range_utf16` 是相对「刚插入的组字文本」的偏移，
            // 基准是 range.start 而非 range.end —— 第二帧起组字区间非空，
            // 用 range.end 平移会让选区终点持续右移。
            self.selected_range = new_selected_range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .map(|new_range| new_range.start + range.start..new_range.end + range.start)
                .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len())
                .into();
        }
        self.mode.update_auto_grow(&self.display_map);
        self.history.start_grouping();
        self.push_history(&old_text, &range, new_text);
        cx.notify();
    }

    /// Used to position IME candidates.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let line_height = last_layout.line_height;
        let line_number_width = last_layout.line_number_width;
        let range = self.range_from_utf16(&range_utf16);

        let mut start_origin = None;
        let mut end_origin = None;
        let line_number_origin = point(line_number_width, px(0.));
        let mut y_offset = last_layout.visible_top;

        for (vi, line) in last_layout.lines.iter().enumerate() {
            if start_origin.is_some() && end_origin.is_some() {
                break;
            }

            let index_offset = last_layout.visible_line_byte_offsets[vi];

            if start_origin.is_none() {
                if let Some(p) = line.position_for_index(
                    range.start.saturating_sub(index_offset),
                    last_layout,
                    false,
                ) {
                    start_origin = Some(p + point(px(0.), y_offset));
                }
            }

            if end_origin.is_none() {
                if let Some(p) = line.position_for_index(
                    range.end.saturating_sub(index_offset),
                    last_layout,
                    false,
                ) {
                    end_origin = Some(p + point(px(0.), y_offset));
                }
            }

            y_offset += line.size(line_height).height;
        }

        let start_origin = start_origin.unwrap_or_default();
        let mut end_origin = end_origin.unwrap_or_default();
        // Ensure at same line.
        end_origin.y = start_origin.y;

        Some(Bounds::from_corners(
            bounds.origin + line_number_origin + start_origin,
            // + line_height for show IME panel under the cursor line.
            bounds.origin + line_number_origin + point(end_origin.x, end_origin.y + line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let last_layout = self.last_layout.as_ref()?;
        let line_point = self.last_bounds?.localize(&point)?;

        for (vi, line) in last_layout.lines.iter().enumerate() {
            let offset = last_layout.visible_line_byte_offsets[vi];
            if let Some(utf8_index) = line.index_for_position(line_point, last_layout) {
                return Some(self.offset_to_utf16(offset + utf8_index));
            }
        }

        None
    }
}

impl Focusable for InputState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// 将原始偏移 o 映射到 outdent 后的新偏移（module 级，便于单元测试）：
/// `edits` 为 (行首偏移, 该行删除的字节数) 列表，按行序排列。
/// 累计此前各行的删除量；位置落在某个被删除区间内时钳制到该行新行首。
fn shifted_outdent_offset(o: usize, edits: &[(usize, usize)]) -> usize {
    let mut shift: isize = 0;
    for &(line_start, n) in edits {
        if n == 0 {
            continue;
        }
        if o >= line_start + n {
            shift -= n as isize;
        } else if o >= line_start {
            // 位置落在被删除区间内：取该行被删除后的行首。
            return (line_start as isize + shift).max(0) as usize;
        } else {
            break;
        }
    }
    (o as isize + shift).max(0) as usize
}

/// 光标所在行用于复制/剪切的文本与字节范围（module 级，便于单元测试）。
///
/// 与 Zed 的复制/剪切语义一致：
/// - 非最后一行：整行内容 + 行尾换行符（LF/CRLF 原样保留）；
/// - 最后一行（无结尾换行）：内容后补一个 `\n`（等同 Zed 的 add_trailing_newline）。
fn line_text_for_clipboard(text: &ropey::Rope, offset: usize) -> (String, std::ops::Range<usize>) {
    let row = text.offset_to_point(offset).row;
    let start = text.line_start_offset(row);
    if row + 1 < text.lines_len() {
        // 到下一行行首：包含本行换行符。
        let end = text.line_start_offset(row + 1);
        (text.slice(start..end).to_string(), start..end)
    } else {
        // 最后一行：内容到行尾，并补结尾换行。
        let end = text.line_end_offset(row);
        let mut content = text.slice(start..end).to_string();
        content.push('\n');
        (content, start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::input::{InputState, Rope};

    #[test]
    fn outdent_unit_len_prefers_tab() {
        assert_eq!(InputState::outdent_unit_len("\tfoo"), 1);
        assert_eq!(InputState::outdent_unit_len("\t\tfoo"), 1);
        assert_eq!(InputState::outdent_unit_len("foo"), 0);
        assert_eq!(InputState::outdent_unit_len("    foo"), 4); // 空格缩进
        assert_eq!(InputState::outdent_unit_len("  foo"), 2);
        assert_eq!(InputState::outdent_unit_len("      foo"), 4); // 最多 4 个
        assert_eq!(InputState::outdent_unit_len("  \tfoo"), 2); // 空格在 tab 前
        assert_eq!(InputState::outdent_unit_len(""), 0);
    }

    #[test]
    fn shifted_outdent_offset_basic() {
        // 两行均删除 2 空格："  foo\n  bar" -> "foo\nbar"
        let edits = [(0, 2), (6, 2)];
        // 选区起点 0 不变
        assert_eq!(shifted_outdent_offset(0, &edits), 0);
        // 选区终点 11（全文结尾）回退 4
        assert_eq!(shifted_outdent_offset(11, &edits), 7);
        // 光标在第二行行首 6：落在删除区间内 → 钳制到新行首 4
        assert_eq!(shifted_outdent_offset(6, &edits), 4);
        // 光标在第二行内容 (8)：回退 2（第一行 2 + 第二行 2）
        assert_eq!(shifted_outdent_offset(8, &edits), 4);
    }

    #[test]
    fn shifted_outdent_offset_single_line_selection() {
        // 光标在行中间 3（"  foo" 内），行首删除 2 → 新位置 1
        let edits = [(0, 2)];
        assert_eq!(shifted_outdent_offset(3, &edits), 1);
        // 光标在行首：钳制到 0
        assert_eq!(shifted_outdent_offset(0, &edits), 0);
        // 光标在删除区中间 (1)：钳制到 0
        assert_eq!(shifted_outdent_offset(1, &edits), 0);
    }

    #[test]
    fn line_text_for_clipboard_middle_line_includes_newline() {
        let rope = Rope::from("first\nsecond\nthird");
        // 光标在第二行行中
        let (text, range) = line_text_for_clipboard(&rope, "first\nsec".len() + 1);
        assert_eq!(text, "second\n");
        assert_eq!(range, "first\n".len().."first\nsecond\n".len());
    }

    #[test]
    fn line_text_for_clipboard_last_line_appends_newline() {
        let rope = Rope::from("first\nsecond\nthird");
        let (text, range) = line_text_for_clipboard(&rope, rope.len());
        assert_eq!(text, "third\n");
        assert_eq!(range, "first\nsecond\n".len()..rope.len());
    }

    #[test]
    fn line_text_for_clipboard_single_line_document() {
        let rope = Rope::from("only line");
        let (text, range) = line_text_for_clipboard(&rope, 0);
        assert_eq!(text, "only line\n");
        assert_eq!(range, 0..9);
    }

    #[test]
    fn line_text_for_clipboard_crlf_preserved() {
        let rope = Rope::from("a\r\nb\r\nc");
        let (text, range) = line_text_for_clipboard(&rope, 3);
        assert_eq!(text, "b\r\n");
        assert_eq!(range, "a\r\n".len().."a\r\nb\r\n".len());
    }

    #[test]
    fn line_text_for_clipboard_blank_line() {
        let rope = Rope::from("a\n\nb"); // 中间空行
        let (text, range) = line_text_for_clipboard(&rope, 2);
        assert_eq!(text, "\n");
        assert_eq!(range, 2..3);
    }

    #[test]
    fn comment_tokens_by_language() {
        assert_eq!(InputState::line_comment_token(Some("rust")), Some("//"));
        assert_eq!(InputState::line_comment_token(Some("python")), Some("#"));
        assert_eq!(InputState::line_comment_token(Some("lua")), Some("--"));
        // 无行注释语义/暂不支持的语言
        assert_eq!(InputState::line_comment_token(Some("json")), None);
        assert_eq!(InputState::line_comment_token(Some("css")), None);
        assert_eq!(InputState::line_comment_token(None), None);
    }

    #[test]
    fn add_and_strip_line_comment() {
        let (out, pt) = InputState::add_line_comment("    let x = 1;", "//");
        assert_eq!(out, "    // let x = 1;");
        assert_eq!(pt, 4);
        let (out, pt) = InputState::strip_line_comment(&out, "//");
        assert_eq!(out, "    let x = 1;");
        assert_eq!(pt, 4);
        // 无空格分隔的注释也能取消
        let (out, _) = InputState::strip_line_comment("  #comment", "#");
        assert_eq!(out, "  comment");
        // 无注释 token 时原样返回
        let (out, _) = InputState::strip_line_comment("let x = 1;", "//");
        assert_eq!(out, "let x = 1;");
    }

    #[test]
    fn newline_indent_decisions() {
        use NewlineIndent::*;
        // 行尾开括号 → 深入
        assert_eq!(InputState::newline_indent_kind("fn main() {", None), Deeper);
        // 开括号后紧跟配对闭括号 → 空行
        assert_eq!(
            InputState::newline_indent_kind("fn main() {", Some('}')),
            BlankBetween
        );
        // 不配对的闭括号不触发空行
        assert_eq!(
            InputState::newline_indent_kind("foo(", Some('}')),
            Deeper
        );
        // 冒号（Python/YAML 风格，行内无花括号）→ 深入
        assert_eq!(InputState::newline_indent_kind("def foo():", None), Deeper);
        // 行尾冒号但行内含花括号（JS 对象字面量）→ 不深入
        assert_eq!(InputState::newline_indent_kind("  foo({ key:", None), Inherit);
        assert_eq!(InputState::newline_indent_kind("let x = 1", None), Inherit);
        assert_eq!(InputState::newline_indent_kind("", None), Inherit);
    }

    #[test]
    fn word_occurrence_scanning() {
        let rope = Rope::from_str("let foo = foo + foo2 + foo;");
        // 光标在词内
        assert_eq!(word_range_at(&rope, 4), Some(4..7));
        // 光标紧贴词尾（边界处仍算在词内）
        assert_eq!(word_range_at(&rope, 7), Some(4..7));
        // 光标在空格处（两侧都非词字符）
        assert_eq!(word_range_at(&rope, 8), None);
        // 词边界匹配：foo 命中 3 处，foo2 不命中
        let occ = find_word_occurrences(&rope, &(4..7), 100).unwrap();
        assert_eq!(occ.len(), 3);
        assert_eq!(occ[0], 4..7);
        assert_eq!(occ[1], 10..13);
        assert_eq!(occ[2], 23..26);

        // 下划线词
        let rope = Rope::from_str("my_var my_var2");
        assert_eq!(word_range_at(&rope, 2), Some(0..6));
        let occ = find_word_occurrences(&rope, &(0..6), 100).unwrap();
        assert_eq!(occ.len(), 1);

        // CJK 不参与同词高亮
        let rope = Rope::from_str("中文 foo 中文");
        assert_eq!(word_range_at(&rope, 1), None);

        // 超出次数上限 → None
        let many = "ab ".repeat(50);
        let rope = Rope::from_str(&many);
        assert!(find_word_occurrences(&rope, &(0..2), 10).is_none());
    }

    #[test]
    fn indent_level_bounds_scan() {
        // Tab：每 1 个 = 1 级
        assert_eq!(indent_level_bounds("\t\tfoo"), vec![1, 2]);
        // 空格：每 4 个 = 1 级
        assert_eq!(indent_level_bounds("        bar"), vec![4, 8]);
        // 混合：tab + 4 空格 = 2 级
        assert_eq!(indent_level_bounds("\t    baz"), vec![1, 5]);
        // 不足 4 个空格不算一级
        assert_eq!(indent_level_bounds("   x"), Vec::<usize>::new());
        // 空行 / 无缩进
        assert_eq!(indent_level_bounds(""), Vec::<usize>::new());
        assert_eq!(indent_level_bounds("no indent"), Vec::<usize>::new());
        // 只有空白
        assert_eq!(indent_level_bounds("  \t "), vec![3]);
    }

    #[test]
    fn rainbow_pair_basic() {
        let rope = Rope::from_str("(a[b]{c})");
        let spans = scan_bracket_spans_text(&rope)
            .into_iter()
            .map(|(r, d, is_open, m)| (r.start, d, is_open, m))
            .collect::<Vec<_>>();
        // ( 层级0，[ 层级1，] 层级1，{ 层级1，} 层级1，) 层级0，全部配对
        assert_eq!(
            spans,
            vec![
                (0, 0, true, true),
                (2, 1, true, true),
                (4, 1, false, true),
                (5, 1, true, true),
                (7, 1, false, true),
                (8, 0, false, true),
            ]
        );
    }

    #[test]
    fn rainbow_pair_unmatched() {
        // 游离闭括号 → 未配对，深度 0
        let rope = Rope::from_str(")(");
        let spans = scan_bracket_spans_text(&rope);
        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[0].0.start, spans[0].1, spans[0].2, spans[0].3),
            (0, 0, false, false)
        );
        assert_eq!(
            (spans[1].0.start, spans[1].1, spans[1].2, spans[1].3),
            (1, 0, true, false)
        );

        // 未闭合的开括号 → 未配对（但保留层级，渲染不标红）
        let rope = Rope::from_str("(((");
        let spans = scan_bracket_spans_text(&rope);
        assert!(spans.iter().all(|(_, _, _, m)| !m));
        assert_eq!(spans[2].1, 2); // 第三个 ( 在第 2 层
    }

    #[test]
    fn rainbow_pair_interleaved() {
        // 交错括号 ([)]：配对算法保证 ( 与 ) 同色（层级 0），[ 与 ] 被跨越 → 未配对
        let rope = Rope::from_str("([)]");
        let spans = scan_bracket_spans_text(&rope)
            .into_iter()
            .map(|(r, d, is_open, m)| (r.start, d, is_open, m))
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            vec![
                (0, 0, true, true),
                (1, 1, true, false),
                (2, 0, false, true),
                (3, 0, false, false)
            ]
        );
    }

    #[test]
    fn rainbow_tree_scan_excludes_strings_and_comments() {
        // rust 语法树：字符串/注释里的括号不应参与 rainbow
        let code = r#"fn main() {
    let s = "a)b(c";
    // ) comment
    let v = (1 + 2);
}
"#;
        let mut h = crate::editor::syntax::SimpleHighlighter::new("rust");
        let rope = Rope::from_str(code);
        assert!(h.update(None, &rope, None));
        let tree = h.tree().expect("rust tree");
        let spans = scan_bracket_spans_tree(tree);
        let starts: Vec<usize> = spans.iter().map(|(r, _, _, _)| r.start).collect();
        // 只应有 main 的 ()、(1+2) 的 () 和函数体的 {}
        assert_eq!(starts, vec![7, 8, 10, 62, 68, 71]);
        assert!(spans.iter().all(|(_, _, _, m)| *m));

        // 纯文本扫描则会计入字符串/注释里的括号（fallback 行为，共 9 个）
        assert_eq!(scan_bracket_spans_text(&rope).len(), 9);
    }

    #[test]
    fn bracket_matching_offsets() {
        let rope = Rope::from_str("fn main() {\n    let a = (1 + 2);\n}\n");
        // 光标在 `{` 后 → 匹配 `{`..`}`
        assert_eq!(InputState::matching_bracket(&rope, 11), Some((10, 33)));
        // 光标在 `(` 后 → 前向匹配
        assert_eq!(InputState::matching_bracket(&rope, 25), Some((24, 30)));
        // 光标在 `)` 前（同一个闭括号，后向匹配）
        assert_eq!(InputState::matching_bracket(&rope, 31), Some((24, 30)));
        // 光标在 `}` 后 → 后向匹配到 `{`
        assert_eq!(InputState::matching_bracket(&rope, 34), Some((10, 33)));
        // 非括号处 → None
        assert_eq!(InputState::matching_bracket(&rope, 0), None);

        // 嵌套：光标在第一个 `(` 后，应跳过内层括号
        let nested = Rope::from_str("a (b (c)) d");
        assert_eq!(InputState::matching_bracket(&nested, 3), Some((2, 8)));
        // 内层
        assert_eq!(InputState::matching_bracket(&nested, 6), Some((5, 7)));

        // 未闭合括号 → None
        let unclosed = Rope::from_str("(abc");
        assert_eq!(InputState::matching_bracket(&unclosed, 1), None);
        let unopened = Rope::from_str("abc)");
        assert_eq!(InputState::matching_bracket(&unopened, 4), None);

        // 多字节字符中的括号不会被误判（中文字节 ≥ 0x80，不会与 ASCII 括号混淆）
        // 布局：中文(0-5) （(6-8) 注(9-11) ）(12-14) ((15) )(16) “(17-19) (20) ”(21-23)
        let cjk = Rope::from_str("中文（注）()“ ”");
        assert_eq!(InputState::matching_bracket(&cjk, 16), Some((15, 16)));
        assert_eq!(InputState::matching_bracket(&cjk, 17), Some((15, 16)));
        // 中文全角括号（多字节）不参与 ASCII 括号匹配
        assert_eq!(InputState::matching_bracket(&cjk, 9), None);

        // 空文本/边界
        let empty = Rope::from_str("");
        assert_eq!(InputState::matching_bracket(&empty, 0), None);
    }

    #[test]
    fn rainbow_pair_depth_stable_for_nested_mixed() {
        let cases = [
            ("((()))", vec![(0, 0), (1, 1), (2, 2), (3, 2), (4, 1), (5, 0)]),
            ("({[]})", vec![(0, 0), (1, 1), (2, 2), (3, 2), (4, 1), (5, 0)]),
            ("a(b[c]{d}e)f", vec![(1, 0), (3, 1), (5, 1), (6, 1), (8, 1), (10, 0)]),
        ];
        for (text, expected) in cases {
            let rope = Rope::from_str(text);
            let spans = scan_bracket_spans_text(&rope);
            let depths: Vec<(usize, usize)> = spans.iter().map(|(r, d, _, _)| (r.start, *d)).collect();
            assert_eq!(depths, expected, "text={text}");
        }
    }

    #[test]
    fn rainbow_pair_all_brackets_paired_same_color_depth() {
        let rope = Rope::from_str("((a){b}[c])");
        let spans = scan_bracket_spans_text(&rope);
        let got: Vec<(usize, usize, bool, bool)> = spans
            .into_iter()
            .map(|(r, d, o, m)| (r.start, d, o, m))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, 0, true, true),
                (1, 1, true, true),
                (3, 1, false, true),
                (4, 1, true, true),
                (6, 1, false, true),
                (7, 1, true, true),
                (9, 1, false, true),
                (10, 0, false, true),
            ]
        );
    }

    /// H3 回归：程序化插入（undo/redo 回放等）不走括号自动配对，
    /// 否则 redo 回放单字符 "(" 会再次配对成 "()"，与原始键入结果不一致；
    /// 而真实键入路径（EntityInputHandler）仍应配对。
    #[gpui::test]
    fn programmatic_insert_bypasses_auto_pair(cx: &mut gpui::TestAppContext) {
        fn build_code_input(
            cx: &mut gpui::TestAppContext,
        ) -> (gpui::Entity<InputState>, &mut gpui::VisualTestContext) {
            struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
            impl gpui::Render for Host {
                fn render(
                    &mut self,
                    _: &mut Window,
                    _: &mut Context<Self>,
                ) -> impl gpui::IntoElement {
                    gpui::div()
                }
            }
            let cell = std::cell::RefCell::new(None);
            let (_host, cx) = cx.add_window_view(|window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
                *cell.borrow_mut() = Some(input.clone());
                Host(input)
            });
            (cell.into_inner().unwrap(), cx)
        }

        // 程序化插入：不配对
        let (input, cx) = build_code_input(cx);
        input
            .downgrade()
            .update_in(cx, |state, window, cx| {
                state.replace_text_in_range_silent(None, "(", window, cx);
                assert_eq!(state.value().as_ref(), "(");
            })
            .unwrap();

        // 键入路径（EntityInputHandler）：仍应自动配对成 "()"
        let (input, cx) = build_code_input(cx);
        input
            .downgrade()
            .update_in(cx, |state, window, cx| {
                gpui::EntityInputHandler::replace_text_in_range(
                    state, None, "(", window, cx,
                );
                assert_eq!(state.value().as_ref(), "()");
                // 光标应停在配对括号中间
                assert_eq!(state.cursor(), 1);
            })
            .unwrap();
    }

    // ==================== Code folding (zed semantics) ====================

    const FOLD_TEST_TEXT: &str =
        "fn main() {\n    let a = 1;\n    if true {\n        a();\n    }\n}\n\nfn other() {\n    let b = 2;\n}\n";

    fn build_fold_input(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<InputState>, &mut gpui::VisualTestContext) {
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let cell = std::cell::RefCell::new(None);
        let (_host, cx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                let mut state =
                    InputState::new(window, cx).code_editor("rust").multi_line(true);
                state.set_value(FOLD_TEST_TEXT, window, cx);
                state
            });
            *cell.borrow_mut() = Some(input.clone());
            Host(input)
        });
        (cell.into_inner().unwrap(), cx)
    }

    #[gpui::test]
    fn fold_at_hides_rows_and_keeps_closing_bracket(cx: &mut gpui::TestAppContext) {
        let (input, cx) = build_fold_input(cx);
        input.update(cx, |state, cx| {
            // Rows: 0 `fn main() {`, 1 `let a`, 2 `if true {`, 3 `a();`, 4 `}` (inner),
            // 5 `}` (outer), 6 blank, 7 `fn other() {`, 8 `let b`, 9 `}`
            state.fold_at(0, cx);
            assert!(state.display_map.is_line_folded(0));
            assert!(!state.display_map.is_line_folded(7));
            // Rows 1-5 hidden (row 5's `}` tail is painted on row 0).
            for row in 1..=5 {
                assert!(state.display_map.is_buffer_line_hidden(row), "row {row}");
            }
            assert!(!state.display_map.is_buffer_line_hidden(6));
            assert!(!state.display_map.is_buffer_line_hidden(7));

            // Unfolding at the header row restores everything.
            state.unfold_at(0, cx);
            assert!(!state.display_map.is_line_folded(0));
            assert!(!state.display_map.is_buffer_line_hidden(1));
        });
    }

    #[gpui::test]
    fn nested_folds_survive_folding_the_outer(cx: &mut gpui::TestAppContext) {
        let (input, cx) = build_fold_input(cx);
        input.update(cx, |state, cx| {
            // Fold the inner `if true {` block first.
            state.fold_at(2, cx);
            assert!(state.display_map.is_line_folded(2));
            assert!(!state.display_map.is_line_folded(0));

            // Fold the outer function; the inner fold stays in storage.
            state.fold_at(0, cx);
            assert!(state.display_map.is_line_folded(0));
            assert!(state.display_map.is_line_folded(2));

            // Unfolding the outer reveals the still-folded inner
            // (zed's unfold_at only removes folds intersecting the row).
            state.unfold_at(0, cx);
            assert!(!state.display_map.is_line_folded(0));
            assert!(state.display_map.is_line_folded(2));
            assert!(state.display_map.is_buffer_line_hidden(3));
            assert!(state.display_map.is_buffer_line_hidden(4));
            assert!(!state.display_map.is_buffer_line_hidden(5));
        });
    }

    #[gpui::test]
    fn cursor_is_clamped_out_of_folds(cx: &mut gpui::TestAppContext) {
        let (input, cx) = build_fold_input(cx);
        input.update(cx, |state, cx| {
            state.fold_at(0, cx);

            // Put the cursor inside the fold, then move by boundaries — the
            // clamps keep the cursor out of the hidden range.
            state.selected_range = (30..30).into(); // row 3 (`a();`) inside the fold
            let backward = state.previous_boundary(30);
            assert_eq!(backward, 11, "should clamp back to the fold start (end of row 0)");
            let forward = state.next_boundary(30);
            assert_eq!(forward, 60, "should clamp forward to the fold end (row 5 closing brace start)");
        });
    }

    #[gpui::test]
    fn fold_all_and_unfold_all(cx: &mut gpui::TestAppContext) {
        let (input, cx) = build_fold_input(cx);
        input.update(cx, |state, cx| {
            state.display_map.clear_folds();
            state.fold_at(0, cx);
            state.fold_at(2, cx);
            state.fold_at(7, cx);
            assert!(state.display_map.is_line_folded(0));
            assert!(state.display_map.is_line_folded(2));
            assert!(state.display_map.is_line_folded(7));

            // Unfold everything via the whole-buffer range (zed's unfold_all).
            state
                .display_map
                .unfold_ranges(&[0..state.text.len()], true);
            assert!(state.display_map.folds().is_empty());
            assert!(!state.display_map.is_buffer_line_hidden(1));
        });
    }
}
