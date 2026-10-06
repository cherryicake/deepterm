//! A text input field that allows the user to enter text.
//!
//! Based on the `Input` example from the `gpui` crate.
//! https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs

#[path = "edit_primitives.rs"]
mod edit_primitives;
use edit_primitives::{
    BRACKET_MATCH_SCAN_LIMIT, OCCURRENCE_MAX_MATCHES, OCCURRENCE_MAX_TEXT_LEN,
    STICKY_MAX_LINES, WORD_SCAN_WINDOW, scan_matching_bracket,
};

// 词法/括号原语已迁至 edit_primitives.rs；re-export 保持
// `state::is_word_char` 等旧引用路径（element.rs / search.rs）不变。
pub(crate) use edit_primitives::{
    is_code_bracket, is_word_char, scan_bracket_spans_text, scan_bracket_spans_tree,
};
/// vim 的 `iw`/`aw` 复用同一套词判定。
pub(crate) use edit_primitives::word_range_at;

use gpui::{
    Action, App, AppContext, Bounds, ClipboardItem, Context, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, HighlightStyle, KeyDownEvent, MouseButton, WeakEntity,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollHandle, ScrollWheelEvent,
    ShapedLine, SharedString, Subscription, UTF16Selection, Window, actions, point, px,
};
use gpui::{Half, TextAlign};
use ropey::{Rope, RopeSlice};
use serde::{Deserialize, Serialize};
use std::ops::{Range, RangeInclusive};
use std::rc::Rc;
use sum_tree::Bias;
use unicode_segmentation::*;

use super::{
    Crease, DisplayMap, MASK_CHAR, blink_cursor::BlinkCursor, cursor::CursorShape, mode::InputMode,
};
use super::display_map::{GitInsertion, GitInsertedLine};
use crate::components::button::Size;
use crate::components::input::blink_cursor::CURSOR_WIDTH;
use crate::components::input::change::Change;
use crate::components::input::history::History;
use crate::components::input::indent::TabSize;
use crate::components::input::movement::MoveDirection;
use crate::components::input::rope_ext::Position;
use crate::components::input::{
    RopeExt as _, Selection, display_map::LineLayout, element::RIGHT_MARGIN,
};
use crate::editor::syntax::{Diagnostic, DiagnosticSet, syntax_diagnostics_opt};
#[cfg(not(target_family = "wasm"))]
use crate::editor::syntax::LanguageRegistry;

/// 按下 Enter。
///
/// 可经 JSON 构造：`keymap.json` 里写
/// `["editor::Enter", { "secondary": true }]` 即可绑定「次要确认」。
#[derive(Action, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[action(namespace = editor)]
pub struct Enter {
    /// Is confirm with secondary.
    pub secondary: bool,
}

/// 折叠到指定层级；`0` 表示全部折叠。
#[derive(Action, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[action(namespace = editor)]
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
        ToggleSoftWrap,
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
    PressEnter {
        secondary: bool,
    },
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

/// 展开区显示的一条被删除旧行（只读虚拟行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDeletedLine {
    /// 旧文件行号（1 起，对齐 git diff 的旧侧行号）。
    pub old_lineno: u32,
    /// 行内容（不含换行符）。
    pub text: String,
}

/// 一个 git diff hunk（HEAD→工作区），供行首色条点击就地展开。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHunkInfo {
    /// 被删除行显示在该 buffer 行（0 起）之前：Modified/Added hunk 为
    /// 首个新行，纯删除 hunk 为删除点之后第一行（与行标记锚点一致）。
    pub before_line: u32,
    /// hunk 覆盖的新行（0 起半开区间）；纯删除 hunk 为空。
    pub new_rows: std::ops::Range<u32>,
    /// 被删除的旧行（按原顺序）；为空表示纯新增，无可展开内容。
    pub deleted: Vec<GitDeletedLine>,
}

impl GitHunkInfo {
    /// 该 hunk 是否可展开（有被删除的旧行可显示）。
    pub fn expandable(&self) -> bool {
        !self.deleted.is_empty()
    }

    /// 该 buffer 行（0 起）是否落在此 hunk 内（含纯删除锚点行）。
    pub fn contains_line(&self, line: u32) -> bool {
        self.new_rows.contains(&line) || (self.new_rows.is_empty() && self.before_line == line)
    }
}

/// HEAD→工作区 diff 的行标记 + hunk 列表（`compute_git_diff_summary` 产物）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitDiffSummary {
    pub marks: Vec<(u32, GitLineMark)>,
    pub hunks: Vec<GitHunkInfo>,
}

/// Diff 行种类（unified / split 视图的行底色装饰）。
/// 决定 gutter 与行号的配套装饰：新增行用实色条＋着色行号，
/// 删除行用虚线条＋着色行号（参考 diff UI）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffLineKind {
    Added,
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
///
/// 三个长度量都**沿轴**取值：纵向条是高/高/高，横向条是宽/宽/宽。
/// 几何算法与 markdown 编辑器共用 `thumb_metrics` / `scroll_for_drag`。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarGeometry {
    /// thumb 在窗口坐标下的区域。
    pub(crate) thumb: Bounds<Pixels>,
    /// 沿轴的视口长度（纵向 = 视口高，横向 = 视口宽）。
    pub(crate) viewport_len: Pixels,
    /// 沿轴的 thumb 长度。
    pub(crate) thumb_len: Pixels,
    /// 沿轴的内容总长度。
    pub(crate) content_len: Pixels,
}

/// 括号着色的最大文件长度（字节），超出则跳过（避免每帧全文件扫描）。
pub(crate) const RAINBOW_BRACKETS_MAX_TEXT_LEN: usize = 2_000_000;
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

/// 安装编辑器的 App 级钩子。
///
/// 目前只有一件事：vim 的「等待一个字符」命令（`f`/`t`/`F`/`T`/`r`）需要在
/// 绑定解析**之前**截住参数。这必须走 `App::intercept_keystrokes`——它比
/// 绑定解析更早运行（`window.rs:5297` vs `:5305`），而 `on_key_down` /
/// `capture_key_down` 都在 action 派发之后（`:5402`），且 action 派发默认
/// 中止传播，参数会被吞掉。
pub fn init(cx: &mut App) {
    crate::keymap::init(cx);

    // 订阅必须活到进程结束：`Subscription` 一旦 drop，拦截器就被注销，
    // `f`/`t`/`r` 的参数会重新被当作命令派发。挂在全局上保活。
    let subscription = cx.intercept_keystrokes(|event, window, cx| {
        let Some(focused) = focused_for_intercept(window, cx) else {
            return;
        };
        let consumed = focused.update(cx, |state, cx| {
            state.on_intercept_keystroke(event, window, cx)
        });
        if consumed {
            cx.stop_propagation();
        }
    });
    cx.set_global(KeystrokeInterceptor(subscription));
}

/// 保活 `intercept_keystrokes` 返回的订阅。
struct KeystrokeInterceptor(#[allow(dead_code)] gpui::Subscription);

impl gpui::Global for KeystrokeInterceptor {}

/// 测试入口：直接跑一次拦截逻辑，不依赖 App 级订阅。
#[cfg(test)]
pub(crate) fn intercept_for_test(
    state: &Entity<InputState>,
    keystroke: &gpui::Keystroke,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let event = gpui::KeystrokeEvent {
        keystroke: keystroke.clone(),
        action: None,
        context_stack: Vec::new(),
    };
    let consumed = state.update(cx, |state, cx| {
        state.on_intercept_keystroke(&event, window, cx)
    });
    if consumed {
        cx.stop_propagation();
    }
    consumed
}

/// 当前存活的所有 `InputState`。
///
/// 供 vim 开关切换时批量同步模式状态。注册表只存弱引用，已析构的条目
/// 在这里顺带清理。
pub(crate) fn all_input_states() -> Vec<Entity<InputState>> {
    FOCUSED_INPUTS.with(|inputs| {
        let mut inputs = inputs.borrow_mut();
        inputs.retain(|(_, weak)| weak.upgrade().is_some());
        inputs
            .iter()
            .filter_map(|(_, weak)| weak.upgrade())
            .collect()
    })
}

/// 找到当前持有键盘焦点的 `InputState`。
///
/// 遍历注册表并按焦点句柄匹配——比给「谁有焦点」再造一套全局状态更直接，
/// 且天然支持多窗口（各自窗口的 `is_focused` 只会命中自己的那个）。
/// 顺带清理已析构的条目。
fn focused_for_intercept(
    window: &mut Window,
    _cx: &mut App,
) -> Option<Entity<InputState>> {
    FOCUSED_INPUTS.with(|inputs| {
        let mut inputs = inputs.borrow_mut();
        inputs.retain(|(_, weak)| weak.upgrade().is_some());
        inputs
            .iter()
            .find(|(handle, _)| handle.is_focused(window))
            .and_then(|(_, weak)| weak.upgrade())
    })
}

thread_local! {
    /// 已注册的 `InputState`：焦点句柄 + 弱引用。
    ///
    /// 必须持弱引用：强引用会让每个创建过的 `InputState` 都无法析构，
    /// gpui 的实体泄漏检查会直接判测试失败（实测 `host_form` 等用例
    /// 因此报 "Leaked handle for entity InputState"）。
    static FOCUSED_INPUTS: std::cell::RefCell<Vec<(FocusHandle, WeakEntity<InputState>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
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
    /// The line number area width of text layout, if not line number, this will be 0px.
    pub(crate) line_number_width: Pixels,
    /// 行号数字位数（展开区旧行号按同宽格式化）。
    pub(crate) line_number_len: usize,
    /// git 展开区行数前缀表（平行于 `visible_buffer_lines`）：
    /// `git_insert_shift[i]` = 锚点 ≤ `visible_buffer_lines[i]` 的累计插入行数。
    /// 与 `git_insert_baseline` 配合，任意遍历可见行的 y 累计点都可 O(1)
    /// 补上块高：`delta(i) = shift[i] - (i == 0 ? baseline : shift[i-1])`。
    pub(crate) git_insert_shift: Rc<Vec<u32>>,
    /// 首个可见行**之上**的插入行数（`visible_top` 已包含，不计入 delta）。
    pub(crate) git_insert_baseline: usize,
    /// The cursor position (top, left) in pixels.
    pub(crate) cursor_bounds: Option<Bounds<Pixels>>,
    /// The text align of the text layout.
    pub(crate) text_align: TextAlign,
    /// The content width of the text layout.
    pub(crate) content_width: Pixels,
}

impl LastLayout {
    /// 第 i 个可见行相对上一行新增的 git 展开区行数（锚在该行上的块）。
    /// 无展开区时恒为 0。
    pub(crate) fn git_insert_delta(&self, i: usize) -> u32 {
        if self.git_insert_shift.is_empty() {
            return 0;
        }
        let cur = self
            .git_insert_shift
            .get(i)
            .or(self.git_insert_shift.last())
            .copied()
            .unwrap_or(0);
        let prev = if i == 0 {
            self.git_insert_baseline as u32
        } else {
            self.git_insert_shift
                .get(i - 1)
                .copied()
                .unwrap_or(self.git_insert_baseline as u32)
        };
        cur.saturating_sub(prev)
    }

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
    /// 缩进单元配置（硬 Tab / 空格，以及单元列宽）。
    ///
    /// 默认 [`TabSize::default`]（4 空格软缩进，与 Zed 一致）。
    pub(crate) tab_size: TabSize,
    /// 悬浮滚动条。
    pub(crate) scrollbar: bool,
    /// 纵向滚动条几何（每次 paint 后写入）。
    pub(crate) scrollbar_geometry: Option<ScrollbarGeometry>,
    /// 纵向滚动条拖拽状态：(按下时鼠标 y, 按下时滚动位置)。
    pub(crate) scrollbar_drag: Option<(Pixels, Pixels)>,
    /// 横向滚动条几何（内容横向溢出时才有，每次 paint 后写入）。
    pub(crate) scrollbar_geometry_h: Option<ScrollbarGeometry>,
    /// 横向滚动条拖拽状态：(按下时鼠标 x, 按下时横向滚动位置)。
    pub(crate) scrollbar_drag_h: Option<(Pixels, Pixels)>,
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
    /// Git diff hunk 列表（与 `git_line_marks` 同源），支撑行首色条点击展开。
    pub(crate) git_hunks: Vec<GitHunkInfo>,
    /// 已展开的 hunk 下标（升序）；diff 重算时整体清空。
    pub(crate) expanded_git_hunks: std::collections::BTreeSet<usize>,
    /// 行首色条点击目标：(buffer 行号, 窗口坐标 bounds)，paint 阶段写入。
    pub(crate) git_bar_click_targets: Vec<(u32, Bounds<Pixels>)>,
    /// 是否允许点击色条展开 hunk（普通文件编辑标签为 true；
    /// diff 视图的编辑器本身已是展开态，置 false）。
    pub(crate) git_expandable: bool,
    pub(crate) show_whitespaces: bool,
    /// This flag tells the renderer to prefer the end of the current visual line.
    pub(crate) cursor_line_end_affinity: bool,
    pub(crate) cursor_shape: CursorShape,
    /// 本次 Insert 会话输入的文本，供 `.` 重放。
    pub(crate) vim_insert_buffer: String,
    /// 本次 Insert 会话的起点偏移。
    pub(crate) vim_insert_start: Option<usize>,
    /// Vim 模式状态。`None` 表示 vim 未启用（或非代码编辑器）。
    ///
    /// 状态挂在 `InputState` 上而不是 `Editor` 上：每个文件 tab 持有独立的
    /// `InputState`，模式因此天然随 tab 走，切换标签不会串味。
    pub(crate) vim: Option<crate::vim::VimState>,
    pub(crate) cursor_blink: bool,
    /// 光标四角弹簧动画状态（跨帧存活；元素每帧只 `step` 一次）。
    pub(crate) cursor_animator: crate::components::cursor_animation::CursorAnimator,
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
    /// The kind drives gutter/number decorations (solid bar + tinted number
    /// for added rows, dotted bar + tinted number for deleted rows).
    pub(crate) diff_line_colors: std::collections::HashMap<u32, (gpui::Hsla, DiffLineKind)>,
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
    /// 该编辑器对应的文件路径（LSP 用，用于判断跨文件跳转）。
    ///
    /// 由 `Editor` 在开 tab / 加载完成时写入。`InputState` 本身不持有路径
    /// （同一个 state 会被 diff、预览等多种 tab 复用）。
    lsp_file_path: Option<String>,
    /// 语言服务器实时状态（状态栏显示）。
    ///
    /// 放 `InputState` 而不是 `Editor`：状态栏渲染时从 `file.editor_state`
    /// 读，而进度通知的监听任务也只持有 `Entity<InputState>`。
    /// `None` = 该文件没有 LSP（远程文件 / 无服务器 / 已禁用）。
    lsp_status: Option<crate::lsp::status::LspStatus>,
    /// 当前 hover 请求代号；见 [`Self::hover_request_generation`]。
    hover_request_generation: u64,
    /// 当前 hover 请求的目标字符偏移；见 [`Self::hover_offset`]。
    hover_offset: Option<usize>,
    /// 当前补全请求代号；见 [`Self::completion_request_generation`]。
    completion_request_generation: u64,
    /// 进行中的 `$/progress` 令牌表（服务器可同时开多个）。
    lsp_progress: crate::lsp::status::ProgressTracker,
    /// 已应用诊断对应的文档版本（`publishDiagnostics.version`）。
    ///
    /// 用来丢弃**过期**的诊断推送：服务器可能先发出针对 v5 的分析、再发出
    /// 针对 v7 的，而两者到达顺序不保证（异步、可能重排）。若把 v5 的结果
    /// 盖在 v7 之上，用户会看到基于旧文本的错误位置。
    ///
    /// `None` = 尚未收到带版本号的推送（此时任何推送都接受）。
    /// `Some(v)` = 已应用到版本 v，只接受 `version >= v` 的推送。
    applied_diagnostics_version: Option<i32>,
    /// 上一次实际放置 LSP 浮层的窗口坐标（观测点）。
    ///
    /// 浮层位置由 `TextElement` 在 prepaint 里算好后 `defer_draw` 出去，
    /// 绘制阶段读不到。这里把算好的位置回写一份，便于测试断言
    /// 「浮层到底落在锚点上方还是下方」——这是用户反复报告的缺陷点，
    /// 只测纯函数无法覆盖调用方传错矩形这类错误。
    last_popover_origin: Option<gpui::Point<gpui::Pixels>>,
    /// 上一次浮层锚点行的屏幕 y（与 `last_popover_origin` 配对，测试用）。
    last_popover_anchor_y: Option<gpui::Pixels>,
    /// LSP 悬浮浮层：(锚点字节偏移, 内容)。
    ///
    /// 放这里而不是 `Editor`：`TextElement` 只拿得到 `Entity<InputState>`，
    /// 绘制时要同时读锚点与内容。
    hover_popover: Option<(usize, crate::editor::hover::HoverInfo)>,
    /// LSP 悬浮提示的落地槽。
    ///
    /// hover 请求在后台完成，那时闭包里拿不到 `Editor` 上的浮层状态；
    /// 结果先放这里，由 Editor 在下一帧取走。
    pending_hover: Option<(usize, crate::editor::hover::HoverInfo)>,
    /// 指针是否正落在 hover 浮层上；见
    /// [`Self::hover_pointer_inside`]。
    hover_pointer_inside: bool,
    /// hover 浮层在窗口坐标系里的矩形；见
    /// [`Self::record_hover_popover_bounds`]。
    hover_popover_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    /// hover 浮层被键盘接管（`K` 第二次按下）。
    ///
    /// **不用真焦点，用标志位**。卡片是用 `defer_draw` 画到窗口层上的，
    /// 它从不经过 prepaint，也就进不了 GPUI 的 dispatch 树 ——
    /// `window.focus()` 在它身上是空操作（`focus_node_id_in_rendered_frame`
    /// 查不到就回退到根节点，按键照样走编辑器的路径）。
    ///
    /// 所以「接管」是虚拟的：焦点物理上还在编辑器里，但 `hjkl` 等键被全局
    /// 按键拦截器（见 `hover::init_keystroke_interceptor`）提前拿走去滚动
    /// 卡片，不再进 vim / 编辑器。所有隐藏路径都要先问这个标志，
    /// 对照 Zed 用 `hover_state.focused()` 守卫的三处。
    hover_keyboard_takeover: bool,
    /// hover 浮层计划隐藏的时刻（`None` = 没安排）。
    ///
    /// 「鼠标移到空白处」不能**立刻**清浮层：从符号把指针挪进卡片的路上，
    /// 必然经过编辑区的其它字符，每一步都会发一次 hover 请求 —— 只要其中
    /// 一次先返回「无内容」并立刻清掉，卡片就会在指针到达之前消失
    /// （用户报的「移上去就关闭」，大概率就是这个时序）。
    ///
    /// 对照 Zed 的 `hiding_delay_task`：无内容时只**安排**一次延迟隐藏，
    /// 到期才真正收起；期间新内容到来、指针进入卡片、新的请求发出，
    /// 都会取消它。
    hover_hide_deadline: Option<std::time::Instant>,
    /// LSP 补全候选的落地槽（同 `pending_hover` 的理由）。
    pending_completions: Option<crate::editor::completion::PendingCompletions>,
    /// 补全要替换的字节区间。
    pending_completion_range: std::ops::Range<usize>,
    /// LSP 文档符号的落地槽。
    pending_symbols: Option<lsp_types::DocumentSymbolResponse>,
    /// LSP 文档高亮（`documentHighlight`）的结果；优先于本地同词高亮。
    lsp_highlights: Option<Vec<Range<usize>>>,
    /// LSP Inlay Hint，按 buffer 行号分组（渲染时按可见行查找）。
    inlay_hints: std::collections::HashMap<usize, Vec<crate::lsp::features::InlayHintData>>,
    /// LSP 折叠范围的落地槽。
    pending_folds: Vec<crate::lsp::features::FoldRange>,
    /// 补全菜单的文本锚点（字节偏移）。
    ///
    /// 与 `hover_popover` 同一套模型：存偏移、绘制时算像素。
    completion_anchor: Option<usize>,
    /// 补全菜单快照（绘制用；`None` = 不显示）。
    completion_menu: Option<crate::editor::completion::CompletionMenu>,

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

        let state = Self {
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
            tab_size: TabSize::default(),
            scrollbar: true,
            scrollbar_geometry: None,
            scrollbar_drag: None,
            scrollbar_geometry_h: None,
            scrollbar_drag_h: None,
            sticky_scroll: true,
            sticky_click_targets: Vec::new(),
            fold_click_targets: Vec::new(),
            git_gutter: true,
            git_line_marks: std::collections::HashMap::new(),
            git_hunks: Vec::new(),
            expanded_git_hunks: std::collections::BTreeSet::new(),
            git_bar_click_targets: Vec::new(),
            git_expandable: false,
            show_whitespaces: false,
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
            lsp_file_path: None,
            last_popover_origin: None,
            last_popover_anchor_y: None,
            applied_diagnostics_version: None,
            lsp_status: None,
            hover_request_generation: 0,
            hover_offset: None,
            completion_request_generation: 0,
            lsp_progress: crate::lsp::status::ProgressTracker::default(),
            hover_popover: None,
            pending_hover: None,
            hover_pointer_inside: false,
            hover_popover_bounds: None,
            hover_keyboard_takeover: false,
            hover_hide_deadline: None,
            pending_completions: None,
            pending_completion_range: 0..0,
            pending_symbols: None,
            lsp_highlights: None,
            inlay_hints: std::collections::HashMap::new(),
            pending_folds: Vec::new(),
            completion_anchor: None,
            completion_menu: None,
            _pending_update: false,
            cursor_line_end_affinity: false,
            cursor_shape: CursorShape::default(),
            vim_insert_buffer: String::new(),
            vim_insert_start: None,
            vim: None,
            cursor_animator: crate::components::cursor_animation::CursorAnimator::new(),
            cursor_blink: true,
            diff_line_colors: std::collections::HashMap::new(),
            diff_range_colors: Vec::new(),
            diff_line_labels: None,
            external_highlight_runs: None,
            text_version: 0,
            scroll_beyond_last_line: None,
            cursor_surrounding_lines: None,
            submit_on_enter: false,
        };

        // 注册焦点句柄，供 App 级击键拦截器找到「当前聚焦的编辑器」
        // （vim 的 `f`/`t`/`r` 参数需要在绑定解析前截住）。
        // 只存弱引用：强引用会阻止 `InputState` 析构，触发 gpui 的实体泄漏检查。
        let entity = cx.entity();
        let weak = entity.downgrade();
        FOCUSED_INPUTS.with(|inputs| {
            inputs.borrow_mut().push((focus_handle.clone(), weak));
        });

        state
    }

    /// Override the line-number labels (indexed by buffer row; `None` entry
    /// renders a blank gutter cell). Pass `None` to restore default numbering.
    pub fn set_diff_line_labels(
        &mut self,
        labels: Option<Vec<Option<u32>>>,
        cx: &mut Context<Self>,
    ) {
        self.diff_line_labels = labels;
        cx.notify();
    }

    /// Set diff decorations: per-line background colors (buffer line index →
    /// color) and word-level background colors (byte range → color).
    /// Used by the git diff view to render added/deleted lines and inline
    /// word highlights on top of the regular editor rendering.
    pub(crate) fn set_diff_decorations(
        &mut self,
        line_colors: Vec<(u32, gpui::Hsla, DiffLineKind)>,
        range_colors: Vec<(Range<usize>, gpui::Hsla)>,
        cx: &mut Context<Self>,
    ) {
        self.diff_line_colors = line_colors
            .into_iter()
            .map(|(row, color, kind)| (row, (color, kind)))
            .collect();
        self.diff_range_colors = range_colors;
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
    /// - 缩进单元：4 个空格（[`TabSize::default`]，可用
    ///   [`Self::tab_size`] 改成硬 Tab 或其它宽度）
    /// - height: 100%
    /// - multi_line: true
    /// - indent_guides: true
    /// - **soft_wrap: false**（与 Zed 默认 `"soft_wrap": "none"` 一致：
    ///   长行不折行，改为水平滚动；其它多行输入框仍是默认的软换行）
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
        // 代码编辑器默认不软换行（Zed: `"soft_wrap": "none"`，VSCode:
        // `editor.wordWrap: off`）：长行保持原始行结构，靠水平滚动查看。
        self.soft_wrap = false;
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

    /// Set enable/disable line number, only for [`InputMode::CodeEditor`] mode.
    pub fn line_number(mut self, line_number: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { line_number: l, .. } = &mut self.mode {
            *l = line_number;
        }
        self
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
        } = &mut self.mode
        {
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
        } = &mut self.mode
        {
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
    /// 记录浮层实际落点与锚点行屏幕 y（由 `TextElement` 在 prepaint 里调用）。
    ///
    /// 同时记下锚点行的屏幕 y，测试就能直接比较「浮层在锚点上方还是下方」，
    /// 不必在测试里重算一遍坐标公式——重算容易与实现漂移，那样测试会
    /// 自洽地通过而真实行为是错的。
    pub fn record_popover_placement(
        &mut self,
        origin: gpui::Point<gpui::Pixels>,
        anchor_y: gpui::Pixels,
    ) {
        self.last_popover_origin = Some(origin);
        self.last_popover_anchor_y = Some(anchor_y);
    }

    /// 记录 hover 浮层在**窗口坐标系**里的实际矩形（`defer_draw` 之后的 bounds）。
    ///
    /// 有了它才能回答「滚轮事件是不是发生在浮层上」。这是必要的，因为：
    /// - GPUI 为每个 `overflow: Scroll` 容器注册的滚动 listener
    ///   （`div.rs::paint_scroll_listener`）**不会** `stop_propagation`，
    ///   所以事件会继续冒泡到 `InputState::on_scroll_wheel`，把编辑器滚走；
    /// - 但那套 listener 是按 paint 顺序注册的，靠「谁先 stop_propagation」
    ///   去和它抢顺序并不可靠。
    ///
    /// 于是改成在**编辑器侧**用几何判断：滚轮落在浮层矩形内就不滚编辑器。
    /// 浮层自身的滚动仍由 GPUI 那套 listener 完成，两者互不干扰。
    ///
    /// 对照 Zed：它的浮层由 `Editor` 自己绘制在独立的 popover 元素里，
    /// 滚轮不会走到编辑器容器上，所以不需要这个判断。
    pub fn record_hover_popover_bounds(&mut self, bounds: gpui::Bounds<gpui::Pixels>) {
        self.hover_popover_bounds = Some(bounds);
    }

    /// 上一次记录的 hover 浮层矩形（见 [`Self::record_hover_popover_bounds`]）。
    pub fn hover_popover_bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.hover_popover_bounds
    }

    /// 某个窗口坐标是否落在当前 hover 浮层内。
    ///
    /// 只在**确实有浮层**时才算命中。浮层消失时 `hover_popover_bounds` 必须
    /// 一并清掉 —— 否则指针恰好落在上一张卡片的位置上时，编辑器会莫名其妙
    /// 地滚不动。
    pub fn hover_popover_contains(&self, position: gpui::Point<gpui::Pixels>) -> bool {
        if self.hover_popover.is_none() {
            return false;
        }
        self.hover_popover_bounds
            .is_some_and(|b| b.contains(&position))
    }

    /// 浮层当前是否被键盘接管（`K` 第二次按下）。
    ///
    /// 隐藏逻辑要先问它：被接管期间鼠标移动与点击外部都**不该**收起浮层，
    /// 否则用户刚用 `K` 打开、还没来得及滚动就没了。
    pub fn hover_keyboard_takeover(&self) -> bool {
        self.hover_popover.is_some() && self.hover_keyboard_takeover
    }

    /// 设置键盘接管标志（见字段文档）。
    pub fn set_hover_keyboard_takeover(&mut self, takeover: bool) {
        // 只在有浮层时才有意义：没浮层时置位只会让一个「不存在的接管」
        // 挡住下一次浮层的正常隐藏。
        self.hover_keyboard_takeover = takeover && self.hover_popover.is_some();
    }

    /// 安排一次延迟隐藏：`delay` 之后到期（见字段文档）。
    ///
    /// 调用方是「服务器返回无内容」的后台任务。注意它**不负责**真正收起，
    /// 只记时刻 —— 到期由 [`Self::hover_hide_expired`] 判定，检查点有两处：
    /// 后台任务睡醒后，以及 Editor 每帧的 `take_pending`。
    pub fn schedule_hover_hide(&mut self, delay: std::time::Duration) {
        self.hover_hide_deadline = Some(std::time::Instant::now() + delay);
    }

    /// 取消待执行的延迟隐藏。
    ///
    /// 新内容到来、新请求发出、指针进入卡片时调用 —— 对照 Zed 在 `show_hover`
    /// 里把 `hiding_delay_task` 置空。
    pub fn cancel_hover_hide(&mut self) {
        self.hover_hide_deadline = None;
    }

    /// 直接指定隐藏时刻（供后台任务认领用，见 `hover.rs` 的延迟隐藏）。
    pub fn set_hover_hide_deadline(&mut self, deadline: std::time::Instant) {
        self.hover_hide_deadline = Some(deadline);
    }

    /// 当前安排的隐藏时刻（`None` = 没安排）。
    pub fn hover_hide_deadline(&self) -> Option<std::time::Instant> {
        self.hover_hide_deadline
    }

    /// 延迟隐藏是否已到期（且浮层还在、指针不在卡片上、没被键盘接管）。
    ///
    /// 三个「不隐藏」的条件缺一不可：
    /// - 指针在卡片上：用户正要操作内容；
    /// - 键盘接管中：用户正用 hjkl 看内容；
    /// - 浮层已经没了：无事可做。
    pub fn hover_hide_expired(&self) -> bool {
        self.hover_popover.is_some()
            && !self.hover_pointer_inside
            && !self.hover_keyboard_takeover
            && self
                .hover_hide_deadline
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    }

    /// 上一次浮层落点（测试用观测点）。
    pub fn last_popover_origin(&self) -> Option<gpui::Point<gpui::Pixels>> {
        self.last_popover_origin
    }

    /// 上一次浮层锚点行的屏幕 y（测试用观测点）。
    pub fn last_popover_anchor_y(&self) -> Option<gpui::Pixels> {
        self.last_popover_anchor_y
    }

    /// 语言服务器实时状态（`None` = 该文件没有 LSP）。
    pub fn lsp_status(&self) -> Option<&crate::lsp::status::LspStatus> {
        self.lsp_status.as_ref()
    }

    /// 设置语言服务器状态。
    pub fn set_lsp_status(&mut self, status: Option<crate::lsp::status::LspStatus>) {
        self.lsp_status = status;
    }

    /// 应用一条 `$/progress` 通知，并据此刷新状态。
    ///
    /// 有进行中的进度 → `Working`；全部结束 → 回到 `Ready`
    /// （诊断计数沿用上次 `publishDiagnostics` 的结果）。
    pub fn apply_lsp_progress(
        &mut self,
        token: &lsp_types::ProgressToken,
        value: &lsp_types::WorkDoneProgress,
    ) {
        if !self.lsp_progress.apply(token, value) {
            return;
        }
        self.refresh_status_from_progress();
    }

    /// 依据进度表刷新 `lsp_status`。
    ///
    /// 保留 `Ready` 上的诊断计数：进度结束不等于诊断清空。
    fn refresh_status_from_progress(&mut self) {
        use crate::lsp::status::LspStatus;

        if let Some((title, message, percentage)) = self.lsp_progress.representative() {
            self.lsp_status = Some(LspStatus::Working {
                title,
                message,
                percentage,
            });
            return;
        }
        // 没有进行中的进度：回到就绪，沿用已有诊断计数。
        let (errors, warnings) = match self.lsp_status.as_ref() {
            Some(LspStatus::Ready { errors, warnings }) => (*errors, *warnings),
            _ => (0, 0),
        };
        self.lsp_status = Some(LspStatus::Ready { errors, warnings });
    }

    /// 记录诊断计数（`Ready` 状态的「实时状态」部分）。
    ///
    /// 正在工作时只记数不切状态——进度条还在跑，不该被诊断覆盖。
    pub fn set_lsp_diagnostic_counts(&mut self, errors: usize, warnings: usize) {
        use crate::lsp::status::LspStatus;
        if self.lsp_progress.is_empty() {
            self.lsp_status = Some(LspStatus::Ready { errors, warnings });
        } else if let Some(LspStatus::Ready { .. }) = self.lsp_status {
            // 已是 Ready 时补上新计数。
            self.lsp_status = Some(LspStatus::Ready { errors, warnings });
        }
    }

    /// 设置悬浮浮层（锚点 + 内容）。
    ///
    /// `InputState` 是浮层状态的**唯一真相源**：`TextElement` 只持有
    /// `Entity<InputState>`，绘制时要同时拿到锚点与内容才能定位和排版。
    ///
    /// 锚点存**字节偏移而非像素**：像素坐标随滚动/软换行/行号栏宽度变化，
    /// 存下来必然过期（这正是之前浮层位置错乱的原因）。
    pub fn set_hover_popover(&mut self, anchor_offset: usize, info: crate::editor::hover::HoverInfo) {
        self.hover_popover = Some((anchor_offset, info));
        // 新内容落定：之前安排的延迟隐藏作废。
        self.hover_hide_deadline = None;
    }

    /// 当前悬浮浮层（锚点偏移 + 内容）。
    pub fn hover_popover(&self) -> Option<(usize, &crate::editor::hover::HoverInfo)> {
        self.hover_popover.as_ref().map(|(o, i)| (*o, i))
    }

    /// 正显示的 hover 卡片是否已经覆盖 `offset`。
    ///
    /// 对照 Zed `hover_popover.rs::same_info_hover`：命中区间的
    /// `start..=end` 包含新偏移，或锚点完全相同（服务器没给命中区间时的
    /// 兜底 —— 签名、纯文本 hover 都是 `range: None`）。
    ///
    /// `HoverState::request_at` 用它决定「要不要重新起防抖」：已经显示在
    /// 覆盖同一区间的卡片上时直接返回，既不重启计时器也不重新发请求 ——
    /// 否则鼠标在同一个符号里抖动会把任务一直往后推，卡片可能永远不出现。
    ///
    /// 用**闭区间** `start..=end`：LSP 常把 hover 结果挂在整个符号区间上，
    /// 而鼠标正好停在符号末字符（该字节属于下一个偏移）时必须算「还在里面」。
    /// Zed 这里同样是 `hover_range.start..=hover_range.end`。
    pub fn hover_popover_covers(&self, offset: usize) -> bool {
        let Some((anchor, info)) = self.hover_popover() else {
            return false;
        };
        if anchor == offset {
            return true;
        }
        info.range
            .as_ref()
            .is_some_and(|range| (range.start..=range.end).contains(&offset))
    }

    /// 清空悬浮浮层。
    pub fn clear_hover_popover(&mut self) {
        self.hover_popover = None;
        // 矩形必须同步清掉：否则 `hover_popover_contains` 会拿一个已经不存在的
        // 卡片去判断命中，导致该区域内的滚动被无谓地吞掉。
        self.hover_popover_bounds = None;
        // 浮层没了，「指针在浮层上」这件事也随之失效。不重置的话，下一次
        // 浮层弹出时 `hover_pointer_inside` 仍是 true，`hide` 会被无谓地跳过，
        // 表现为「鼠标明明已经移开，浮层却赖着不走」。
        self.hover_pointer_inside = false;
        // 浮层都没了，待执行的延迟隐藏与键盘接管一并作废。
        self.hover_hide_deadline = None;
        self.hover_keyboard_takeover = false;
    }

    /// 标记指针是否正落在 hover 浮层上。
    ///
    /// 由浮层自己的 `on_mouse_move` / `on_mouse_down` 置位，由编辑区的
    /// `on_mouse_move` 复位。隐藏逻辑（`HoverState::hide_in` 与延迟隐藏）
    /// 要先问它：指针在卡片上时不能收起，否则用户根本没法把指针移进去
    /// 点链接或滚动内容 —— 卡片会在指针到达之前就消失。
    ///
    /// 对照 Zed `hover_popover.rs:1068`：它在浮层上挂 `on_mouse_move` 并
    /// `cx.stop_propagation()`，顺带把 `closest_mouse_distance` 归零、
    /// 清掉 `hiding_delay_task`。我们没有独立的 `closest_mouse_distance`，
    /// 用这个布尔量表达同一件事。
    pub fn set_hover_pointer_inside(&mut self, inside: bool) {
        self.hover_pointer_inside = inside;
    }

    /// 指针是否正落在 hover 浮层上（见 [`Self::set_hover_pointer_inside`]）。
    pub fn hover_pointer_inside(&self) -> bool {
        self.hover_pointer_inside
    }

    /// 补全菜单的文本锚点（字节偏移）。
    ///
    /// 与 hover 同一套模型：**只存文本偏移，像素在绘制时算**（照抄 Zed）。
    /// 菜单位置依赖当前帧行布局与滚动偏移，预先算好会过期。
    pub fn set_completion_anchor(&mut self, offset: usize) {
        self.completion_anchor = Some(offset);
    }

    /// 当前补全菜单的文本锚点。
    pub fn completion_anchor(&self) -> Option<usize> {
        self.completion_anchor
    }

    /// 清空补全菜单锚点。
    pub fn clear_completion_anchor(&mut self) {
        self.completion_anchor = None;
    }

    /// 设置补全菜单快照（绘制用）。
    ///
    /// 与锚点一起构成绘制所需的全部信息；两者都由 `Editor` 在候选到达时写入。
    pub fn set_completion_menu(
        &mut self,
        candidates: Vec<crate::editor::completion::Candidate>,
        selected: usize,
    ) {
        self.completion_menu = Some(crate::editor::completion::CompletionMenu {
            candidates,
            selected,
        });
    }

    /// 当前补全菜单快照。
    pub fn completion_menu(&self) -> Option<&crate::editor::completion::CompletionMenu> {
        self.completion_menu.as_ref()
    }

    /// 清空补全菜单（锚点 + 快照）。
    pub fn clear_completion_menu(&mut self) {
        self.completion_anchor = None;
        self.completion_menu = None;
    }

    /// 设置 LSP 折叠范围（供显示层合并进折叠状态）。
    pub fn set_pending_folds(&mut self, folds: Vec<crate::lsp::features::FoldRange>) {
        self.pending_folds = folds;
    }

    /// 取走 LSP 折叠范围。
    pub fn take_pending_folds(&mut self) -> Option<Vec<crate::lsp::features::FoldRange>> {
        if self.pending_folds.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending_folds))
    }

    /// 设置 LSP Inlay Hint（按 buffer 行号分组）。
    pub fn set_inlay_hints(&mut self, hints: Vec<crate::lsp::features::InlayHintData>) {
        self.inlay_hints.clear();
        for hint in hints {
            // hint 的偏移 → 行号：inlay 画在它所属那一行。
            let row = self
                .text
                .byte_to_line_idx(hint.offset.min(self.text.len()), ropey::LineType::LF);
            self.inlay_hints.entry(row).or_default().push(hint);
        }
    }

    /// 按 buffer 行号索引的 Inlay Hint（渲染用）。
    pub fn inlay_hints_by_line(
        &self,
    ) -> &std::collections::HashMap<usize, Vec<crate::lsp::features::InlayHintData>> {
        &self.inlay_hints
    }

    /// LSP 文档高亮的落地槽：`Some(vec)` 时优先于本地的「同词高亮」。
    ///
    /// 服务器知道作用域，比按词文本匹配准（同名局部变量与全局变量不会被
    /// 混在一起）。为 `None` 时回退本地逻辑。
    pub fn set_lsp_highlights(&mut self, ranges: Option<Vec<Range<usize>>>) {
        self.lsp_highlights = ranges;
    }

    /// 当前可见区间对应的 LSP `Range`（Inlay Hint 等「只算可见部分」的请求用）。
    ///
    /// 未布局过时返回 `None`。行号取可见行范围（不含折叠隐藏的行）。
    pub fn visible_lsp_range(&self) -> Option<lsp_types::Range> {
        let layout = self.last_layout.as_ref()?;
        let first = *layout.visible_buffer_lines.first()?;
        let last = *layout.visible_buffer_lines.last()?;
        let start = crate::lsp::features::offset_to_position(
            &self.text,
            self.text.line_start_offset(first),
        );
        let end = crate::lsp::features::offset_to_position(
            &self.text,
            self.text.line_content_end_offset(last),
        );
        Some(lsp_types::Range::new(start, end))
    }

    /// 存放后台文档符号请求的结果。
    pub fn set_pending_symbols(&mut self, response: lsp_types::DocumentSymbolResponse) {
        self.pending_symbols = Some(response);
    }

    /// 取走后台文档符号结果。
    pub fn take_pending_symbols(&mut self) -> Option<lsp_types::DocumentSymbolResponse> {
        self.pending_symbols.take()
    }

    /// 光标下的词（命名面板的初始值、重命名前的校验都用它）。
    ///
    /// 光标不在词内时返回空串——调用方据此判断「这里没有可重命名的符号」。
    pub fn word_at_cursor(&self) -> String {
        use crate::components::input::rope_ext::RopeExt as _;
        let offset = self.cursor();
        let range = crate::lsp::features::word_range_before(&self.text, offset);
        // 光标右侧也纳入：光标停在词首时 `word_range_before` 得到空区间。
        //
        // 按字符下标前进并吸附起点：`end += 1` 按字节走会停在多字节
        // 字符中间，随后的 `slice` 会 panic。
        let end = {
            let clamped = self
                .text
                .clip_offset(offset.min(self.text.len()), sum_tree::Bias::Left);
            let len = self.text.chars().count();
            let mut ci = self.text.offset_to_char_index(clamped);
            while ci < len {
                let byte_off = self.text.char_index_to_offset(ci);
                let ch = self.text.char(byte_off);
                if ch.is_alphanumeric() || ch == '_' {
                    ci += 1;
                } else {
                    break;
                }
            }
            self.text.char_index_to_offset(ci)
        };
        if range.start >= end {
            return String::new();
        }
        self.text.slice(range.start..end).to_string()
    }

    /// 该编辑器对应的文件路径（LSP 用）。
    pub fn lsp_file_path(&self) -> Option<&str> {
        self.lsp_file_path.as_deref()
    }

    /// 设置 LSP 用的文件路径。
    pub fn set_lsp_file_path(&mut self, path: impl Into<String>) {
        self.lsp_file_path = Some(path.into());
    }

    /// 光标处的 LSP `Position`（发起 hover / 补全 / 跳转请求时用它）。
    pub fn lsp_cursor_position(&self) -> lsp_types::Position {
        crate::lsp::features::offset_to_position(&self.text, self.cursor())
    }

    /// 光标在内容坐标系里的像素位置（上一次布局的结果）。
    pub fn cursor_bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.last_layout.as_ref()?.cursor_bounds
    }

    /// 存放后台 hover 请求的结果（供浮层取走）。
    ///
    /// `offset` 是**这次请求**的目标字符偏移，必须和结果一起存 ——
    /// 取出时拿它当卡片锚点。若只存内容、取出时用「当前鼠标/光标位置」，
    /// 而两者之间又有新请求发出，卡片内容（A 的）和位置（B 的）就会错位。
    /// 这正是之前 `HoverState::last_result_offset` 那个镜像字段想解决的，
    /// 但镜像和槽是两处状态，照样有竞态；合在一起才是原子的。
    pub fn set_pending_hover(
        &mut self,
        info: crate::editor::hover::HoverInfo,
        offset: usize,
    ) {
        self.pending_hover = Some((offset, info));
    }

    /// 记录当前 hover 请求的代号（见 `HoverState::request_generation`）。
    ///
    /// 存在这里而不是只留在 `HoverState` 上，是为了让后台任务能比对：
    /// 任务持有 `WeakEntity<InputState>`，够不到 `HoverState`。
    /// 用途是让**过期响应**自我丢弃 —— GPUI 的 `Task` drop 只是 detach，
    /// 旧任务照样会跑完并回写。
    pub fn set_hover_request_generation(&mut self, generation: u64) {
        self.hover_request_generation = generation;
    }

    /// 当前 hover 请求代号（供后台任务判断自己的响应是否过期）。
    pub fn hover_request_generation(&self) -> u64 {
        self.hover_request_generation
    }

    /// 记录当前 hover 请求的目标字符偏移。
    ///
    /// 代号只随 `on_mouse_moved` 递增，所以鼠标**移出编辑区**时代号不变 ——
    /// 只靠代号拦不住那种过期响应。这个偏移是第二道闸门：后台任务比对它，
    /// 不一致就丢弃。
    pub fn set_hover_offset(&mut self, offset: usize) {
        self.hover_offset = Some(offset);
    }

    /// 当前 hover 请求的目标字符偏移（见 [`Self::set_hover_offset`]）。
    pub fn hover_offset(&self) -> Option<usize> {
        self.hover_offset
    }

    /// 记录当前补全请求代号（见 [`Self::completion_request_generation`]）。
    pub fn set_completion_request_generation(&mut self, generation: u64) {
        self.completion_request_generation = generation;
    }

    /// 当前补全请求代号（供后台任务判断自己的响应是否过期）。
    pub fn completion_request_generation(&self) -> u64 {
        self.completion_request_generation
    }

    /// 取走后台 hover 结果（取走即清空，避免重复弹出同一张卡片）。
    ///
    /// 返回 `(请求时的偏移, 内容)` —— 调用方必须用这个偏移当锚点。
    pub fn take_pending_hover(
        &mut self,
    ) -> Option<(usize, crate::editor::hover::HoverInfo)> {
        self.pending_hover.take()
    }

    /// 存放后台补全请求的结果。
    ///
    /// `cursor_offset` 是**发请求时**的光标字节偏移，不是收到结果时的。
    ///
    /// 必须记下来：`drain_lsp_results` 在 `render` 里跑，而后台任务结束与
    /// 下一帧之间用户可能又敲了字符。那时若直接用「当前光标」去算
    /// `replace_range` 与菜单锚点，就会拿 A 位置的候选按 B 位置替换 ——
    /// 用户看到的正是「候选内容和光标处该有的东西对不上」。同理
    /// [`Self::take_pending_completions`] 要靠它丢弃已过期的结果。
    pub fn set_pending_completions(
        &mut self,
        items: Vec<lsp_types::CompletionItem>,
        position: lsp_types::Position,
        cursor_offset: usize,
    ) {
        self.pending_completions = Some(crate::editor::completion::PendingCompletions {
            items,
            position,
            cursor_offset,
        });
    }

    /// 取走后台补全结果，**但光标已经移开时返回 `None`**。
    ///
    /// 光标一动，先前那份候选就不再适用：显示它会让用户看到与光标处不匹配
    /// 的内容，按下去还会替换错区间。与其展示一个错的菜单，不如让下一次
    /// 请求的结果直接接管（`isIncomplete` 时 rust-analyzer 会继续补全）。
    pub fn take_pending_completions(
        &mut self,
    ) -> Option<crate::editor::completion::PendingCompletions> {
        let pending = self.pending_completions.take()?;
        if pending.cursor_offset != self.cursor() {
            log::debug!(
                "lsp: completion 结果已过期（请求于 offset={}，当前 offset={}），丢弃 {} 个候选",
                pending.cursor_offset,
                self.cursor(),
                pending.items.len()
            );
            return None;
        }
        Some(pending)
    }

    /// 按候选算出补全要替换的字节区间，并记下来供 `confirm` 使用。
    pub fn completion_replace_range_for(
        &mut self,
        items: &[lsp_types::CompletionItem],
    ) -> std::ops::Range<usize> {
        let offset = self.cursor();
        self.completion_replace_range_at(offset, items)
    }

    /// 与 [`Self::completion_replace_range_for`] 相同，但**显式指定**用哪个
    /// 光标偏移来算。
    ///
    /// 补全菜单必须用它而不是「当前光标」：候选是按**请求时**的位置取的，
    /// 两者必须同源。`take_pending_completions` 已经保证取出时光标没动，
    /// 这里传进来的 `offset` 才是那次请求的真实位置。
    pub fn completion_replace_range_at(
        &mut self,
        offset: usize,
        items: &[lsp_types::CompletionItem],
    ) -> std::ops::Range<usize> {
        let range = match items.first() {
            Some(item) => {
                crate::lsp::features::completion_replace_range(self.text(), offset, item)
            }
            None => crate::lsp::features::word_range_before(self.text(), offset),
        };
        self.pending_completion_range = range.clone();
        range
    }

    /// 应用一条补全：主插入 + `additionalTextEdits`（如自动导入）。
    ///
    /// LSP 的 `CompletionItem.additionalTextEdits` 是**接受该候选时必须一并
    /// 应用**的编辑——rust-analyzer 用它插入 `use` 语句（自动导入）。
    /// 不处理的话，接受 `HashMap` 会得到「未导入」的错误而不是可用的代码。
    ///
    /// 顺序很重要：**先应用附加编辑，再插入主文本**。附加编辑的 range 是
    /// 基于「插入前」的文档坐标（通常落在文件顶部），若先插入主文本，顶部
    /// 之后的偏移会整体平移，附加编辑就会打到错误位置。
    ///
    /// 返回 false 表示主插入的区间失效（文本被并发改短），调用方应放弃本次
    /// 补全——此时附加编辑也不该生效。
    pub fn apply_completion_with_edits(
        &mut self,
        range: std::ops::Range<usize>,
        new_text: &str,
        additional: &[lsp_types::TextEdit],
        cx: &mut Context<Self>,
    ) -> bool {
        // 先把附加编辑换算成字节区间（基于当前文本），并从后往前排序。
        let additional_bytes = crate::lsp::features::to_byte_edits(self.text(), additional.to_vec());

        // 逐条应用（已按起点降序）。任一区间失效就整体放弃，避免留下
        // 「导入加了但名字没插」的半成品。
        for edit in &additional_bytes {
            if !self.apply_completion(edit.range.clone(), &edit.new_text, cx) {
                log::debug!("lsp: stale additionalTextEdit range, aborting completion");
                return false;
            }
        }

        // 主插入的区间是基于**原始**文档算的；附加编辑若落在它之前，
        // 偏移会平移。按平移量修正后再插入。
        let shift: i64 = additional_bytes
            .iter()
            .filter(|e| e.range.start <= range.start)
            .map(|e| e.new_text.len() as i64 - (e.range.end - e.range.start) as i64)
            .sum();
        let shifted = if shift == 0 {
            range
        } else {
            let start = (range.start as i64 + shift).max(0) as usize;
            let end = (range.end as i64 + shift).max(0) as usize;
            start..end
        };

        self.apply_completion(shifted, new_text, cx)
    }

    /// 记下的补全替换区间。
    pub fn pending_completion_range(&self) -> std::ops::Range<usize> {
        self.pending_completion_range.clone()
    }

    /// 应用一条 LSP 补全：按字节区间替换文本并把光标放到插入文本之后。
    ///
    /// 单独成方法而不是复用 `replace_text_in_range`：后者要 `Window` 且收
    /// UTF-16 区间，而补全结果本来就是字节区间（在 `features` 里换算过），
    /// 再转一次 UTF-16 只会引入偏移错误的机会。
    ///
    /// 返回 false 表示区间非法（文本在请求期间被改短了），调用方应放弃本次
    /// 补全而不是写坏缓冲区。
    pub fn apply_completion(
        &mut self,
        range: std::ops::Range<usize>,
        new_text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let len = self.text.len();
        if range.start > range.end || range.end > len {
            return false;
        }
        // 区间必须落在字符边界上，否则 `Rope` 的切片/渲染会 panic。
        if !self.text.is_char_boundary(range.start) || !self.text.is_char_boundary(range.end) {
            return false;
        }
        if self.disabled {
            return false;
        }

        // 撤销记录需要**编辑前**的文本：只克隆受影响的那一段。
        let old_text: String = self.text.slice(range.clone()).to_string();
        let new_offset = range.start + new_text.len();
        let old_selection = self.selected_range;

        // 折叠要先按这次编辑调整：折叠存的是字节偏移，不调整的话它们会
        // 指向新文本里无关的位置（甚至越界）。顺序与
        // `replace_text_in_range_inner` 一致。
        self.display_map.adjust_folds_for_edit(&range, new_text);
        self.adjust_git_diff_for_edit(&range, new_text);

        self.text.remove(range.clone());
        self.text.insert(range.start, new_text);

        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        self.history.push(
            Change::new(range.clone(), &old_text, range.start..new_offset, new_text).with_selections(
                old_selection.start..old_selection.end,
                new_offset..new_offset,
            ),
        );
        self.history.end_grouping();
        self.bump_text_version();
        self.selected_range = (new_offset..new_offset).into();
        self.ime_marked_range.take();
        self.update_preferred_column();
        self.mode.update_auto_grow(&self.display_map);
        self.update_search(cx);

        // 语法高亮走同步路径：这里没有 `Window`，起不了后台解析任务。
        let _ = self
            .mode
            .update_highlighter(&range, &self.text, new_text, true, cx);

        cx.emit(InputEvent::Change);
        cx.notify();
        true
    }

    /// 整体替换诊断集合（LSP 的主要入口）。
    ///
    /// 与 [`Self::diagnostics_mut`] 的区别：LSP 诊断要先按当前文本换算成
    /// 字节区间，调用方手上只有 `&mut self`，拿不到第二个不可变借用。
    /// 所以把「算 + 写」收进一个方法里，由内部决定借用顺序。
    ///
    /// 返回 false 表示这不是代码编辑器。
    pub fn set_diagnostics(&mut self, items: Vec<Diagnostic>) -> bool {
        let Some(set) = self.mode.diagnostics_mut() else {
            return false;
        };
        set.set(items, &self.text);
        true
    }

    /// 带版本门控的诊断替换（`publishDiagnostics` 的入口）。
    ///
    /// 服务器可能乱序推送（先发 v5 的分析、后发 v7 的），若让旧的覆盖新的，
    /// 用户会看到基于旧文本的错误位置。规则：
    /// - `version` 为 `Some(v)` 且 `v < 已应用版本` → **丢弃**，返回 `false`；
    /// - `version` 为 `None` → 照常应用（部分服务器不报版本，不能因此拒收）；
    /// - 否则应用并记下版本。
    ///
    /// 返回值表示「是否应用了」；`false` 既可能是版本过期，也可能是该编辑器
    /// 不是代码编辑器（无诊断集合）。
    pub fn set_diagnostics_versioned(
        &mut self,
        items: Vec<Diagnostic>,
        version: Option<i32>,
    ) -> bool {
        if let Some(v) = version
            && let Some(applied) = self.applied_diagnostics_version
            && v < applied
        {
            log::debug!(
                "lsp: dropping stale diagnostics (version {v} < applied {applied})"
            );
            return false;
        }
        if !self.set_diagnostics(items) {
            return false;
        }
        if version.is_some() {
            self.applied_diagnostics_version = version;
        }
        true
    }

    /// 已应用诊断的文档版本（测试与调试用）。
    pub fn applied_diagnostics_version(&self) -> Option<i32> {
        self.applied_diagnostics_version
    }

    /// 滚到某个偏移并居中（LSP 跳转定位用）。
    pub fn reveal_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.scroll_to_center(offset, cx);
    }

    /// 应用 LSP 给的折叠范围。
    ///
    /// 只在**当前没有任何折叠**时应用：用户手动折过/展开过之后不再覆盖，
    /// 否则服务器每推一次范围就把用户的操作重置回去。
    pub fn apply_lsp_folds(
        &mut self,
        folds: Vec<crate::lsp::features::FoldRange>,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_folding() || folds.is_empty() {
            return;
        }
        // `folds()` 为空才算「用户没折过」：一旦有过折叠就交回用户控制。
        if !self.display_map.folds().is_empty() {
            return;
        }
        let text = self.text.clone();
        let mut ranges = Vec::with_capacity(folds.len());
        for fold in folds {
            let start = text.line_start_offset(fold.start_line as usize);
            // 结束行用「下一行行首」而不是本行行尾：折起来后要连带吃掉
            // 行尾换行，否则折叠区与下一行之间会留一条空缝。
            let end = text
                .line_start_offset(fold.end_line as usize + 1)
                .min(text.len());
            if start < end {
                ranges.push(start..end);
            }
        }
        if ranges.is_empty() {
            return;
        }
        self.display_map.fold_ranges(&text, ranges);
        self.after_fold_change(cx);
    }

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
    /// `at` 为显式插入列（字节偏移）；`None` 时沿用本行自身缩进。
    fn add_line_comment(line: &str, token: &str) -> (String, usize) {
        Self::add_line_comment_at(line, token, None)
    }

    /// 在 `at` 列插入行注释（用于多行对齐到最小缩进列，Zed 行为）。
    fn add_line_comment_at(line: &str, token: &str, at: Option<usize>) -> (String, usize) {
        let indent_len = line.len() - line.trim_start().len();
        let at = at.unwrap_or(indent_len).min(line.len());
        // `at` 必须落在行首空白区内，否则回落自身缩进（避免把注释插进代码中间）。
        let at = if line[..at].trim().is_empty() {
            at
        } else {
            indent_len
        };
        let mut out = String::with_capacity(line.len() + token.len() + 1);
        out.push_str(&line[..at]);
        out.push_str(token);
        out.push(' ');
        out.push_str(&line[at..]);
        (out, at)
    }

    /// 去掉单行注释（连同 token 后的一个空格）。返回 (新行, 原 token 位置)。
    /// 只认行首空白后的 token，避免误删字符串内的 `//`（如 `let s = "// hi"`）。
    fn strip_line_comment(line: &str, token: &str) -> (String, usize) {
        let indent_len = line.len() - line.trim_start().len();
        let Some(rest) = line[indent_len..].strip_prefix(token) else {
            return (line.to_string(), 0);
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let mut out = String::with_capacity(line.len());
        out.push_str(&line[..indent_len]);
        out.push_str(rest);
        (out, indent_len)
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

    /// Replace the whole content as an **undoable** edit, preserving both the
    /// scroll offset and (as far as possible) the cursor.
    ///
    /// [`Self::set_value`] clears the undo history and jumps to the top of the
    /// document, which is the right behaviour when loading a file but wrong for
    /// a user-visible mutation — reverting a diff hunk went through it and was
    /// therefore impossible to undo, while throwing the user back to line 1.
    pub fn set_value_undoable(
        &mut self,
        value: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scroll = self.scroll_handle.offset();
        let cursor = self.cursor();

        let was_ignoring = self.history.ignore;
        self.history.ignore = false;
        self.replace_text(value, window, cx);
        self.history.ignore = was_ignoring;

        // Clamp the cursor into the new text instead of resetting it to 0.
        let cursor = cursor.min(self.text.len());
        self.selected_range = (cursor..cursor).into();

        if self.mode.is_code_editor() {
            self._pending_update = true;
        }

        self.update_preferred_column();
        self.scroll_handle.set_offset(scroll);
        cx.notify();
    }

    /// 记录「文档内容已变化」，并使所有按版本号做的缓存/闸门失效。
    ///
    /// **不变式：任何改写 `self.text` 的路径都必须调用它。** 目前有且仅有三处：
    /// - [`Self::replace_text_in_range_inner`] —— 常规键入 / 粘贴 / `set_value`
    /// - [`Self::set_text`] —— 整体替换（文件加载、切标签页、重载）
    /// - `EntityInputHandler::replace_and_mark_text_in_range` —— IME 组字
    ///
    /// 漏掉的后果不止一处：
    /// - `editor.rs` 的脏检查以 `last_checked_text_version != text_version` 为闸门，
    ///   版本不动 → 未保存标记永远不更新（IME 输入后文件不显示“已修改”）。
    /// - `occurrence_cache`（同词高亮）以 `text_version` 为缓存键 → 文本换了而键
    ///   没换，会把旧文档的命中范围套在新文档上。
    /// - `dispatch_background_parse` 的 stale 判定形同虚设 → 可能把针对旧文本解析
    ///   出的语法树应用到新文本上。
    fn bump_text_version(&mut self) {
        self.text_version = self.text_version.wrapping_add(1);
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
        self.bump_text_version();
        if self.mode.is_single_line() {
            self.selected_range = (self.text.len()..self.text.len()).into();
        } else {
            self.selected_range.clear();
        }
        self.ime_marked_range = None;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        // Folds are stored as raw byte offsets into the previous buffer. After
        // a wholesale text replacement those offsets point into unrelated
        // content (and may even exceed the new buffer length), so keeping them
        // folds arbitrary regions of the new file. `set_text` is used for file
        // loads / tab switches, where a fresh document must start unfolded.
        self.display_map.clear_folds();
        self.display_map.set_text(&self.text, cx);
        // 程序化替换后行数也要跟上内容（否则 auto_grow 输入框在 set_text
        // 后高度不变——如提交成功清空后盒子一直撑着）。普通编辑走编辑
        // 路径已各自更新，这里只补 set_text 这一条程序化路径。
        self.mode.update_auto_grow(&self.display_map);
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
        // `Rope::len_utf16` is O(1); walking every char was O(N) on every
        // replace, i.e. on every file load and every keystroke in the
        // single-line path.
        let range = 0..self.text.len_utf16();
        self.replace_text_in_range_silent(Some(range), &text, window, cx);
        self.reset_highlighter(cx);
        self.disabled = was_disabled;
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

    /// 缩进单元配置（默认 [`TabSize::default`]，即 4 空格）。
    ///
    /// 影响 Tab / Shift+Tab、回车智能缩进、闭合括号自动反缩进与缩进参考线层级。
    pub fn tab_size(mut self, tab_size: TabSize) -> Self {
        self.tab_size = tab_size;
        self
    }

    /// 当前缩进单元的列宽（缩进参考线层级用）。
    pub(crate) fn indent_width(&self) -> usize {
        self.tab_size.tab_size.clamp(1, 64)
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

    /// 写入 Git 行标记与 hunk 列表（HEAD→工作区 diff 重算完成后调用）。
    ///
    /// 旧 hunk 的展开态随内容一并失效：行号与被删文本都基于旧 diff，
    /// 保留只会显示过期内容，这里统一收起。
    pub fn set_git_diff(&mut self, summary: GitDiffSummary, cx: &mut Context<Self>) {
        self.git_line_marks = summary.marks.into_iter().collect();
        self.git_hunks = summary.hunks;
        self.expanded_git_hunks.clear();
        self.sync_git_expansions();
        cx.notify();
    }

    /// 普通文件编辑标签开启行首色条点击展开（diff 视图编辑器不开启）。
    pub fn set_git_expandable(&mut self, enable: bool) {
        self.git_expandable = enable;
    }

    /// 点击该 buffer 行的色条：展开/收起锚定在该行上的**全部**可展开
    /// hunk（修改 hunk 与共享锚点的纯删除 hunk 一起展开，一次看全）。
    pub fn toggle_git_hunk_for_line(&mut self, line: u32, cx: &mut Context<Self>) {
        if !self.git_expandable {
            return;
        }
        let indexes: Vec<usize> = self
            .git_hunks
            .iter()
            .enumerate()
            .filter(|(_, hunk)| hunk.contains_line(line) && hunk.expandable())
            .map(|(ix, _)| ix)
            .collect();
        if indexes.is_empty() {
            return;
        }
        let expand = !self.expanded_git_hunks.contains(&indexes[0]);
        for ix in indexes {
            if expand {
                self.expanded_git_hunks.insert(ix);
            } else {
                self.expanded_git_hunks.remove(&ix);
            }
        }
        self.sync_git_expansions();
        cx.notify();
    }

    /// 该 buffer 行（0 起）所属的 hunk 下标（取第一个命中的：Modified hunk
    /// 可能与相邻纯删除 hunk 共享锚点行）。
    pub(crate) fn git_hunk_index_for_line(&self, line: u32) -> Option<usize> {
        self.git_hunks
            .iter()
            .position(|h| h.contains_line(line) && h.expandable())
    }

    /// 该行是否处于已展开 hunk 的新行范围内（展开时整行加绿底，对齐 diff view）。
    pub(crate) fn line_in_expanded_hunk(&self, line: u32) -> bool {
        self.expanded_git_hunks.iter().any(|&ix| {
            self.git_hunks
                .get(ix)
                .is_some_and(|hunk| hunk.new_rows.contains(&line))
        })
    }

    /// 把当前展开态同步到 display map 的插入区。
    fn sync_git_expansions(&mut self) {
        let insertions = self
            .expanded_git_hunks
            .iter()
            .filter_map(|&ix| {
                let hunk = self.git_hunks.get(ix)?;
                if hunk.deleted.is_empty() {
                    return None;
                }
                Some(GitInsertion {
                    before_buffer_row: hunk.before_line as usize,
                    hunk_ix: ix,
                    lines: std::rc::Rc::new(
                        hunk.deleted
                            .iter()
                            .map(|d| GitInsertedLine {
                                old_lineno: Some(d.old_lineno),
                                text: std::rc::Rc::from(d.text.as_str()),
                            })
                            .collect(),
                    ),
                })            })
            .collect();
        self.display_map.set_git_insertions(insertions);
    }

    /// 文本编辑后随行调整 hunk 锚点与行标记（与 `adjust_folds_for_edit`
    /// 同一调用点：行标记本身已是过期数据，这里只保证“整体随行平移”，
    /// 让色条与展开区不会停在编辑前的旧行号上）。
    pub(crate) fn adjust_git_diff_for_edit(&mut self, range: &Range<usize>, new_text: &str) {
        if self.git_hunks.is_empty() && self.git_line_marks.is_empty() {
            return;
        }
        // 行漂移量 = 换行符数量变化（行号只对换行敏感）。
        let removed = self
            .text
            .slice(range.clone())
            .to_string()
            .matches('\n')
            .count();
        let delta = new_text.matches('\n').count() as i64 - removed as i64;
        let edit_start_line = self.text.offset_to_point(range.start).row;
        let edit_end_line = self.text.offset_to_point(range.end).row;

        if delta != 0 {
            // 行首纯插入（如在行首按回车）：该行的既有内容整体下移一行，
            // 内容性标记（Added/Modified 行、hunk 新行范围）随之平移；
            // 纯删除锚点是「间隙在此行之前」的位置性标记，不随内容动。
            let at_line_start = range.start == range.end
                && self.text.line_start_offset(edit_start_line) == range.start;
            let content_shift = |line: u32| -> u32 {
                if line > edit_start_line as u32
                    || (at_line_start && line >= edit_start_line as u32)
                {
                    (line as i64 + delta).clamp(0, u32::MAX as i64) as u32
                } else {
                    line
                }
            };
            let anchor_shift = |line: u32| -> u32 {
                if line > edit_start_line as u32 {
                    (line as i64 + delta).clamp(0, u32::MAX as i64) as u32
                } else {
                    line
                }
            };
            self.git_line_marks = self
                .git_line_marks
                .drain()
                .map(|(line, mark)| (content_shift(line), mark))
                .collect();
            for hunk in &mut self.git_hunks {
                hunk.before_line = if hunk.new_rows.is_empty() {
                    anchor_shift(hunk.before_line)
                } else {
                    content_shift(hunk.before_line)
                };
                hunk.new_rows.start = content_shift(hunk.new_rows.start);
                hunk.new_rows.end = content_shift(hunk.new_rows.end);
            }
        }

        // 编辑落进某个 hunk 的新行范围：该 hunk 内容已变，收起其展开区。
        let touched = |hunk: &GitHunkInfo| -> bool {
            if hunk.new_rows.is_empty() {
                edit_start_line as u32 <= hunk.before_line
                    && hunk.before_line <= (edit_end_line + 1) as u32
            } else {
                hunk.new_rows.contains(&((edit_start_line) as u32))
                    || hunk.new_rows.contains(&((edit_end_line) as u32))
            }
        };
        let stale: Vec<usize> = self
            .expanded_git_hunks
            .iter()
            .copied()
            .filter(|&ix| self.git_hunks.get(ix).is_some_and(&touched))
            .collect();
        for ix in stale {
            self.expanded_git_hunks.remove(&ix);
        }
        self.sync_git_expansions();
    }

    /// 同词高亮：返回光标处词的全文件出现范围（带缓存）。
    /// None 表示当前无需高亮（非词内/超出限制/未启用）。
    pub(crate) fn occurrence_ranges(&mut self) -> Option<std::rc::Rc<Vec<Range<usize>>>> {
        // LSP 的 `documentHighlight` 已知结果时直接用：它按语义作用域算，
        // 比按词文本匹配准。空 vec 也是有效结果（服务器说「没有其它出现」）。
        if let Some(ranges) = &self.lsp_highlights {
            return Some(std::rc::Rc::new(ranges.clone()));
        }
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
        let word = edit_primitives::word_range_at(&self.text, cursor)?;
        let ranges = edit_primitives::find_word_occurrences(&self.text, &word, OCCURRENCE_MAX_MATCHES)?;
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
            // 换行宽度与 prepaint / `set_input_bounds` 同一套算法：元素宽 −
            // 行号栏 − 右边距。这里原本直接用 `last_layout.wrap_width`，但
            // 关着软换行时它恒为 None，退回「整元素宽」会让首行比可视区宽出
            // 一个行号栏 —— 刚打开软换行的文件会立刻溢出右侧再横滚一次。
            let line_number_width = self
                .last_layout
                .as_ref()
                .map(|l| l.line_number_width)
                .unwrap_or(px(0.));
            let wrap_width =
                (self.input_bounds.size.width - line_number_width - RIGHT_MARGIN).max(px(1.));

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

    /// [`ToggleSoftWrap`]（VSCode `alt-z` / Zed `cmd-k z`）：切换软换行。
    ///
    /// 代码编辑器默认关（长行水平滚动，与 Zed/VSCode 一致），此 action 让
    /// 单个输入框随时切回软换行 —— 设置系统落地前的临时出口。
    pub(crate) fn toggle_soft_wrap(
        &mut self,
        _: &ToggleSoftWrap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_multi_line() {
            return;
        }
        let wrap = !self.soft_wrap;
        self.set_soft_wrap(wrap, window, cx);
    }

    /// Set the regular expression pattern of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn pattern(mut self, pattern: regex::Regex) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.pattern = Some(pattern);
        self
    }

    /// Set the validation function of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn validate(mut self, f: impl Fn(&str, &mut Context<Self>) -> bool + 'static) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.validate = Some(Box::new(f));
        self
    }

    /// Return the value of the input field.
    pub fn value(&self) -> SharedString {
        SharedString::new(self.text.to_string())
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
        let offset = self.smart_home_offset();
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

    /// 词边界分段里最后一个词段的子词起点（相对 `text`；`None` = 全是空白）。
    ///
    /// - 尾随纯标点段跳过一段（VSCode/Zed 行为：`hello.|` 的 alt-← 落到 `hello` 首）；
    /// - 词段内再按 `_` 与驼峰切子词（`foo_bar`、`fooBar`、`HTMLParser` 逐段停）。
    fn last_word_start(text: &str) -> Option<usize> {
        let segs: Vec<(usize, &str)> = UnicodeSegmentation::split_word_bound_indices(text)
            .filter(|(_, s)| !s.trim_start().is_empty())
            .collect();
        if segs.is_empty() {
            return None;
        }
        // 跳过紧贴光标的尾随纯标点段（无字母数字/`_`）。
        let mut idx = segs.len() - 1;
        if idx > 0 && !segs[idx].1.chars().any(|c| c.is_alphanumeric() || c == '_') {
            idx -= 1;
        }
        let (seg_start, seg) = segs[idx];
        let sub = Self::last_subword_start_in(seg)?;
        Some(seg_start + sub)
    }

    /// 词段内最后一个子词的起点（相对段首字节偏移）。
    fn last_subword_start_in(seg: &str) -> Option<usize> {
        let starts = Self::subword_starts(seg);
        starts.last().copied()
    }

    /// 词段的子词起点表（含 0），按 `_` 与驼峰切分。
    fn subword_starts(seg: &str) -> Vec<usize> {
        let chars: Vec<(usize, char)> = seg.char_indices().collect();
        if chars.is_empty() {
            return Vec::new();
        }
        let mut starts = vec![0];
        for k in 1..chars.len() {
            let (byte_i, c) = chars[k];
            let (_, p) = chars[k - 1];
            let next = chars.get(k + 1).map(|(_, n)| *n);
            let boundary = c == '_'
                || p == '_'
                || (p.is_lowercase() && c.is_uppercase())
                || (p.is_ascii_digit() && c.is_uppercase())
                || (p.is_lowercase() && c.is_ascii_digit())
                || (p.is_uppercase()
                    && c.is_uppercase()
                    && next.is_some_and(|n| n.is_lowercase()));
            if boundary {
                starts.push(byte_i);
            }
        }
        starts
    }

    /// 词段内第一个子词的终点（相对段首字节偏移）。
    fn first_subword_end_in(seg: &str) -> Option<usize> {
        let starts = Self::subword_starts(seg);
        if starts.is_empty() {
            return None;
        }
        if starts.len() >= 2 {
            Some(starts[1])
        } else {
            Some(seg.len())
        }
    }

    /// 词边界分段里第一个词段的首个子词终点（相对 `text`；`None` = 全是空白）。
    /// 前导纯标点段直接越过（返回其终点）；词段内按子词切分，`fooBar` 先停 `foo` 末。
    fn first_word_end(text: &str) -> Option<usize> {
        for (i, s) in UnicodeSegmentation::split_word_bound_indices(text) {
            if s.trim_start().is_empty() {
                continue;
            }
            if !s.chars().any(|c| c.is_alphanumeric() || c == '_') {
                return Some(i + s.len());
            }
            return Self::first_subword_end_in(s).map(|e| i + e);
        }
        None
    }

    /// Return the start offset of the previous word.
    pub(crate) fn previous_start_of_word(&mut self) -> usize {
        let offset = self.selected_range.start;
        let offset = self.offset_from_utf16(self.offset_to_utf16(offset));

        // `split_word_bound_indices` 需要 `&str`，无法直接吃 `RopeSlice`。
        // 每次都把光标之前的整段复制成 String 的话，2MB 文件在末尾按一次
        // alt-← 就要分配 2MB。词边界几乎总落在最近的一段文本里，所以先只扫
        // `WORD_SCAN_WINDOW` 字节；窗口整体是空白（超长空行/缩进）时才退回
        // 全量扫描，保证语义与不做窗口时完全一致。
        let start = self
            .text
            .clip_offset(offset.saturating_sub(WORD_SCAN_WINDOW), Bias::Right);
        let left_part = self.text.slice(start..offset).to_string();

        match Self::last_word_start(&left_part) {
            Some(i) => start + i,
            None if start > 0 => {
                Self::last_word_start(&self.text.slice(0..offset).to_string()).unwrap_or(0)
            }
            None => 0,
        }
    }

    /// Return the next end offset of the next word.
    pub(crate) fn next_end_of_word(&mut self) -> usize {
        let offset = self.cursor();
        let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
        let len = self.text.len();
        let end = self
            .text
            .clip_offset((offset + WORD_SCAN_WINDOW).min(len), Bias::Left);
        let right_part = self.text.slice(offset..end).to_string();

        match Self::first_word_end(&right_part) {
            Some(i) => offset + i,
            // 窗口内没有非空白内容：要么文档尾部就是空白（结果同为 `len`），
            // 要么窗口被空白填满，需要放宽到余下全文。
            None if end < len => Self::first_word_end(&self.text.slice(offset..len).to_string())
                .map_or(len, |i| offset + i),
            None => len,
        }
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
        // 用内容边界而非 `line_end_offset`：后者在 CRLF 下指向 `\n`，会把 `\r`
        // 算进行内容，导致 End 键停在 `\r` 之后、`cmd-delete` 删出孤立 `\r`。
        let logical_end = self.text.line_content_end_offset(row);

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
        let unit = self.tab_size.indent_unit();

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
                        edit_primitives::scan_matching_bracket(text, off, false, b, close, BRACKET_MATCH_SCAN_LIMIT)
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
                    if let Some(m) = scan_matching_bracket(
                        text,
                        off,
                        true,
                        open,
                        close,
                        BRACKET_MATCH_SCAN_LIMIT,
                    ) {
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
        let ends_with_open =
            trimmed_end.ends_with('{') || trimmed_end.ends_with('[') || trimmed_end.ends_with('(');
        if ends_with_open {
            let open = trimmed_end.chars().last().unwrap();
            if let Some(next) = next
                && Self::AUTO_PAIRS
                    .iter()
                    .any(|(o, c)| *o == open && *c == next)
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
                        // 空格缩进：去掉一个单元（`tab_size` 个空格）。
                        // 按**字符**裁剪：行首空白可能是全角空格（U+3000）等
                        // 多字节字符，按字节切片会落在字符中间导致 panic。
                        let chars: Vec<char> = prefix.chars().collect();
                        let keep = chars.len().saturating_sub(self.indent_width());
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
                let wordlike =
                    |c: Option<char>| c.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
                let before = self.text.slice(..sel.start).chars().last();
                let after = self.text.slice(sel.end..).chars().next();
                if wordlike(before) || after.map(|c| wordlike(Some(c)) || c == ch).unwrap_or(false)
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

    /// 插入新行时使用的行尾符，取自 `row` 自身的终止符（`\r\n` 或 `\n`）。
    ///
    /// 编辑 CRLF 文档时若硬编码 `\n`，会往文件里掺入混用行尾——这也是
    /// 合并行 / 上下移动行 / 复制行在 CRLF 文件上产出错乱内容的根因。
    fn insert_line_ending(&self, row: usize) -> &'static str {
        if self.text.line_terminator_len(row) == 2 {
            "\r\n"
        } else {
            "\n"
        }
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
        // 多行注释时对齐到最小缩进列（Zed：min_column），避免每行注释符参差不齐。
        let min_indent = if !all_commented {
            raw_lines
                .iter()
                .filter_map(|raw| {
                    let no_nl = raw.strip_suffix('\n').unwrap_or(raw);
                    let line = no_nl.strip_suffix('\r').unwrap_or(no_nl);
                    if line.trim().is_empty() {
                        None
                    } else {
                        Some(line.len() - line.trim_start().len())
                    }
                })
                .min()
        } else {
            None
        };
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

            let (new_core, insert_point) = if all_commented && commented(line) {
                Self::strip_line_comment(line, token)
            } else if line.trim().is_empty() && multi {
                (line.to_string(), 0)
            } else if let Some(min) = min_indent {
                Self::add_line_comment_at(line, token, Some(min))
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
                    let adjusted = if pos > *insert_point {
                        pos as isize + delta
                    } else {
                        pos as isize
                    };
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
            // 两块各自带着自己的终止符，直接交换即可（LF / CRLF 都对）。
            format!("{block}{prev}")
        } else {
            // block 是文件尾、没有终止符：它要接管 prev 的终止符。
            // 必须整段剥掉 prev 的终止符（CRLF 是 2 字节，只砍 1 字节会留下
            // 孤立的 `\r`），再按文档自身的行尾风格重新插回。
            let prev_row = *rows.start() - 1;
            let content_len = prev
                .len()
                .saturating_sub(self.text.line_terminator_len(prev_row));
            format!(
                "{block}{}{}",
                self.insert_line_ending(prev_row),
                &prev[..content_len]
            )
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
            // below 是文件尾、没有终止符，而 block 带着自己的终止符。
            // 交换后 block 变成末行，因此要把它的终止符让给 below：整段剥掉
            // block 的终止符（CRLF 是 2 字节），再按文档行尾风格插到中间。
            let block_last_row = *rows.end();
            let content_len = block
                .len()
                .saturating_sub(self.text.line_terminator_len(block_last_row));
            format!(
                "{below}{}{}",
                self.insert_line_ending(block_last_row),
                &block[..content_len]
            )
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
            // 末行没有终止符：补一个与文档风格一致的行尾符再接内容。
            format!("{}{content}", self.insert_line_ending(*rows.end()))
        };
        let range_utf16 = self.range_to_utf16(&(span.end..span.end));
        self.replace_text_in_range_silent(Some(range_utf16), &insert, window, cx);
        self.scroll_to(self.cursor(), None, cx);
        self.update_preferred_column();
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
        // 删除文件最后的行时连同前一个行尾符一起删，避免留下尾部空行。
        // 行尾符可能是 2 字节（`\r\n`），只退 1 字节会让文件以孤立的 `\r` 结尾。
        if span.end >= self.text.len() && span.start > 0 {
            let start_row = self.text.offset_to_point(span.start).row;
            let prev_row = start_row.saturating_sub(1);
            span.start = span
                .start
                .saturating_sub(self.text.line_terminator_len(prev_row));
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
        // 一律用「内容边界」而不是 `line_end_offset`：`line_end_offset` 在 CRLF
        // 文档里指向 `\n`，其前面的 `\r` 会被算进行内容，导致合并时把 `\r`
        // 拼进正文、并且因为 `ends_with(whitespace)` 恒真而永远不加空格。
        let row_start = self.text.line_start_offset(row);
        let row_content_end = self.text.line_content_end_offset(row);
        let next_start = self.text.line_start_offset(row + 1);
        let next_content_end = self.text.line_content_end_offset(row + 1);

        let next_line = self.text.slice(next_start..next_content_end).to_string();
        if next_line.trim().is_empty() && next_content_end >= self.text.len() {
            // 下一行是文件尾空行：仅删本行的行尾符（CRLF 下是 2 字节）
            let range_utf16 = self.range_to_utf16(&(row_content_end..next_start));
            self.replace_text_in_range_silent(Some(range_utf16), "", window, cx);
            cx.notify();
            return;
        }

        let cur_line = self.text.slice(row_start..row_content_end).to_string();
        let trimmed = next_line.trim_start();
        let sep = if cur_line.is_empty() || cur_line.ends_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        let text = format!("{sep}{trimmed}");
        // 替换区间 = 本行行尾符 + 下一行内容，下一行的行尾符原样保留。
        let range_utf16 = self.range_to_utf16(&(row_content_end..next_content_end));
        self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
        let new_cursor = row_content_end + sep.len();
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
    pub(crate) fn toggle_fold(&mut self, _: &ToggleFold, _: &mut Window, cx: &mut Context<Self>) {
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
                .point_to_offset(crate::components::input::Point::new(row, 0))..end
        } else {
            selection.start..selection.end
        };

        if self
            .display_map
            .folds_in_range(range.clone())
            .next()
            .is_some()
        {
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
        let range_end_row = self
            .text
            .offset_to_point(range_end.min(self.text.len()))
            .row;

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
        let intersection_range = self
            .text
            .point_to_offset(crate::components::input::Point::new(buffer_row, 0))
            ..self
                .text
                .point_to_offset(crate::components::input::Point::new(
                    buffer_row,
                    self.text.slice_line(buffer_row).len(),
                ));
        if self.display_map.unfold_ranges(&[intersection_range], true) {
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
            ..self
                .text
                .point_to_offset(crate::components::input::Point::new(
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
            if let Some(foldable_range) = self.display_map.crease_for_buffer_row(&self.text, row) {
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
            // 已在行内容末尾：连行尾符一起删掉。CRLF 下是 2 字节，只退 1 字节
            // 会留下孤立的 `\r`。
            let row = self.text.offset_to_point(self.cursor()).row;
            let terminator = self.text.line_terminator_len(row);
            offset = (offset + terminator).clamp(0, self.text.len());
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

    /// 代码编辑器中 Tab / Shift+Tab 的缩进单元由 [`Self::tab_size`] 决定：
    /// 默认 [`TabSize::default`]（4 空格，与 Zed 一致），Makefile 等对
    /// 制表符敏感的文件可切成 [`TabSize::HARD_TABS`]。
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
                let unit = self.tab_size.indent_unit();
                self.replace_text_in_range_silent(None, &unit, window, cx);
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

        let unit = self.tab_size.indent_unit();
        let mut new_text = String::new();
        let mut removed = Vec::with_capacity(last_row - start_row + 1);
        for row in start_row..=last_row {
            let line = self.text.slice_line(row).to_string();
            if outdent {
                // 行首反缩进一次要删掉的字节数由 [`Self::tab_size`] 决定。
                let n = self.tab_size.outdent_len(&line);
                removed.push(n);
                new_text.push_str(&line[n..]);
            } else {
                new_text.push_str(&unit);
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
            // 每行行首插入一个缩进单元：起点平移一个单元，终点平移「行数 ×
            // 单元长度」（此前写死 +1，只在缩进单元是单个 Tab 时成立）。
            let unit_len = unit.len();
            (
                start + unit_len,
                end + unit_len * (last_row - start_row + 1),
            )
        };
        self.selected_range = Selection::new(new_start, new_end);
        self.update_preferred_column();
    }

    /// Shift+Tab：删除光标所在行行首的一个缩进单元，光标按删除量回退。
    fn outdent_cursor_line(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.text.offset_to_point(offset).row;
        let line_start = self.text.line_start_offset(row);
        let line = self.text.slice_line(row).to_string();
        let n = self.tab_size.outdent_len(&line);
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
            // 行首 git 色条：点击展开/收起所在 hunk（不移动光标）。
            if let Some((line, _)) = self
                .git_bar_click_targets
                .iter()
                .find(|(_, bounds)| bounds.contains(&event.position))
            {
                let line = *line;
                self.toggle_git_hunk_for_line(line, cx);
                return;
            }
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

        // Zed：点行号区当三击处理（`crates/editor/src/element/mouse.rs`
        // `click_count = 3 // Simulate triple-click when clicking the gutter
        // to select lines`）= 选中整行。gutter 是**不随横向滚动平移**的固定
        // 绘制区，用 x 反推列在横滚后必然算错（点行号会跳到第几十列），
        // 所以这条路完全不问列、按行处理。
        let in_gutter = event.button == MouseButton::Left
            && self.input_bounds.contains(&event.position)
            && event.position.x < self.content_left_x();
        let click_count = if in_gutter { 3 } else { event.click_count };

        self.selecting = true;
        let offset = self.index_for_mouse_position(event.position);

        if event.button == MouseButton::Right {
            return;
        }

        match click_count {
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
        // **滚轮落在 hover 浮层上时不滚编辑器。**
        //
        // GPUI 为每个 `overflow: Scroll` 容器自动注册的滚动 listener 不会
        // `stop_propagation`，所以浮层滚得动的同时事件也冒泡到了这里，结果是
        // 「浮层和背后的代码一起滚」。浮层自己的滚动不需要我们操心（那是
        // GPUI 那套 listener 的事），这里只负责不添乱。
        //
        // 用几何判断而非 `hover_pointer_inside`：后者只在 `on_mouse_move` 时
        // 更新，而滚轮可能在鼠标静止不动时到来（例如用户停在卡片上直接滚）。
        if self.hover_popover_contains(event.position) {
            cx.stop_propagation();
            return;
        }

        let line_height = self
            .last_layout
            .as_ref()
            .map(|layout| layout.line_height)
            .unwrap_or(window.line_height());
        let mut delta = event.delta.pixel_delta(line_height);

        // Shift + 滚轮 → 横向滚动（VSCode/Zed 同款）。gpui 把
        // `scrollingDeltaX/Y` 原样透传（`gpui_macos/events.rs`），不做换轴：
        // 触控板横扫自带 x 分量，只有 x 为 0 的滚轮/鼠标才需要换，避免双重
        // 交换把横扫打成竖滚。
        if event.modifiers.shift && delta.x == px(0.) {
            delta = point(delta.y, px(0.));
        }

        let old_offset = self.scroll_handle.offset();
        self.update_scroll_offset(Some(old_offset + delta), cx);

        // Only stop propagation if the offset actually changed
        if self.scroll_handle.offset() != old_offset {
            cx.stop_propagation();
        }
    }

    /// 应用滚动偏移，返回**是否发生了变化**。
    ///
    /// 调用方据此决定是否需要再请求一帧。此前这里无条件 `cx.notify()`，而
    /// `paint` 每帧都会调用本函数 —— 于是画完就把实体标脏、下一帧再画一遍，
    /// 空闲时也是满帧空转（叠加 paint 里的 O(N) 扫描，代价很可观）。
    pub(crate) fn update_scroll_offset(
        &mut self,
        offset: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let previous = self.scroll_handle.offset();
        let mut offset = offset.unwrap_or(previous);
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
        // 只有真的挪动了才需要重绘：钳制后的偏移与原来相同是常态（例如已经
        // 停在顶部还在往上滚），这种情况下没必要再请求一帧。
        let changed = offset != previous;
        if changed {
            cx.notify();
        }
        changed
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

        // 目标行上方的累计高度 = 它之前的可见显示行数 × 行高。逐行累加是
        // O(光标行号)，光标在长文件末尾时每次移动都要扫全表；改走 display_map
        // 的前缀和 + 二分（`display_row_for_buffer_line`）后是 O(log n)。
        let mut row_offset_y =
            line_height * self.display_map.display_row_for_buffer_line(row) as f32;

        // Apart from left alignment, just leave enough space for the cursor size on the right side.
        let safety_margin = if last_layout.text_align == TextAlign::Left {
            RIGHT_MARGIN
        } else {
            CURSOR_WIDTH
        };
        // `lines` is a compact array of *visible* lines, so it cannot be
        // indexed with `row - visible_range.start`; that arithmetic only holds
        // when nothing is folded. `LastLayout::line` binary-searches
        // `visible_buffer_lines` and is correct in both cases.
        if let Some(line) = last_layout.line(row) {
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
                Some(lines) => (lines as f32 * line_height)
                    .min((last_layout.visible_range.len() as f32 * line_height).half()),
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

        // 同 `scroll_to`：目标行上方的累计高度走 O(log n) 的 display row 查询。
        let mut row_offset_y =
            line_height * self.display_map.display_row_for_buffer_line(row) as f32;

        let mut scroll_offset = self.scroll_handle.offset();

        // 行内列偏移：在前面各行总高度之上，同时供下方水平可见性逻辑复用
        // （与 scroll_to 不同，这里只需查询一次）。
        // Same as `scroll_to`: `lines` is compact, so look the row up through
        // `LastLayout::line`'s binary search rather than subtracting the
        // visible range start (which is wrong as soon as a fold is active).
        let pos = last_layout
            .line(row)
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
        if self.selected_range.is_empty() && self.mode.is_multi_line() {
            let (selected_text, _) = line_text_for_clipboard(&self.text, self.cursor());
            let meta = vec![ClipboardSelection::entire_line(
                selected_text.len(),
                self.first_line_indent_for_offset(self.cursor()),
            )];
            cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(
                selected_text,
                meta,
            ));
        } else if !self.selected_range.is_empty() {
            let selected_text = self.text.slice(self.selected_range).to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
        }
    }

    pub(crate) fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            if !self.mode.is_multi_line() {
                return;
            }
            // 无选区：剪切当前整行（含换行），与 Zed 的 cut_common 一致。
            let (selected_text, range) = line_text_for_clipboard(&self.text, self.cursor());
            let meta = vec![ClipboardSelection::entire_line(
                selected_text.len(),
                self.first_line_indent_for_offset(self.cursor()),
            )];
            cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(
                selected_text,
                meta,
            ));
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

    /// 复制时记录的首行缩进（列数），用于整行粘贴的元数据（Zed `ClipboardSelection` 子集）。
    fn first_line_indent_for_offset(&self, offset: usize) -> u32 {
        let row = self.text.offset_to_point(offset).row;
        self.tab_size.indent_count(&self.text.slice_line(row)) as u32
    }

    pub(crate) fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(clipboard) = cx.read_from_clipboard() {
            let mut new_text = clipboard.text().unwrap_or_default();
            if !self.mode.is_multi_line() {
                // 单行输入框：整段压平。CRLF 文本里的 `\r` 也要去掉，否则会
                // 在输入框里显示成一串裸 `\r`。
                new_text = new_text
                    .chars()
                    .filter(|c| *c != '\n' && *c != '\r')
                    .collect();
            }

            // 整行复制的粘贴：空选区时插到当前行首之前（新起一行），而非光标处
            // 截断当前行（Zed `do_paste` 的 entire_line 分支单光标子集）。
            // 有选区时沿用替换选区语义；缩进调整暂不做（无 auto-indent 引擎），
            // 原缩进存于元数据供后续使用。
            let entire_line = !self.mode.is_single_line()
                && self.selected_range.is_empty()
                && clipboard.entries().first().is_some_and(|e| {
                    use gpui::ClipboardEntry;
                    matches!(e, ClipboardEntry::String(s)
                        if s.metadata_json::<Vec<ClipboardSelection>>()
                            .is_some_and(|m| m.first().is_some_and(|s| s.is_entire_line)))
                });
            if entire_line {
                let row = self.text.offset_to_point(self.cursor()).row;
                let line_start = self.text.line_start_offset(row);
                let range_utf16 = self.range_to_utf16(&(line_start..line_start));
                self.replace_text_in_range_silent(Some(range_utf16), &new_text, window, cx);
            } else {
                self.replace_text_in_range_silent(None, &new_text, window, cx);
            }
            self.scroll_to(self.cursor(), None, cx);
        }
    }

    fn push_history(
        &mut self,
        text: &Rope,
        range: &Range<usize>,
        new_text: &str,
        old_selection: Range<usize>,
        new_selection: Range<usize>,
    ) {
        if self.history.ignore {
            return;
        }

        let range =
            text.clip_offset(range.start, Bias::Left)..text.clip_offset(range.end, Bias::Right);
        let old_text = text.slice(range.clone()).to_string();
        let new_range = range.start..range.start + new_text.len();

        self.history.push(
            Change::new(range, &old_text, new_range, new_text)
                .with_selections(old_selection, new_selection),
        );
    }

    /// 把选区恢复到指定偏移，越界时钳到文档末尾。
    fn restore_selection(&mut self, selection: Option<(usize, usize)>) {
        let Some((start, end)) = selection else {
            return;
        };
        let len = self.text.len();
        let start = start.min(len);
        let end = end.clamp(start, len);
        self.selected_range = (start..end).into();
        self.selection_reversed = false;
    }

    pub(crate) fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.undo() {
            // `changes` 逆序排列（最近一次编辑在前），所以最后一条才是本组
            // 最早的编辑 —— 它记录的 `old_selection` 就是整组开始前的光标。
            // 没有它，撤销后光标会停在被恢复文本的末尾而不是编辑前的位置。
            let restore = changes.last().and_then(|c| c.old_selection);
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.new_range);
                self.replace_text_in_range_silent(Some(range_utf16), &change.old_text, window, cx);
            }
            self.restore_selection(restore);
            self.update_preferred_column();
            self.scroll_to(self.cursor(), None, cx);
        }
        self.history.ignore = false;
        // 撤销本身不属于任何编辑组：否则紧接着的输入会被并进刚撤销那一组的
        // 版本号里，与"撤销"这个动作混为一谈。
        self.history.break_group();
        cx.notify();
    }

    pub(crate) fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.redo() {
            // redo 栈里 `changes` 是正序的（最早的在前），最后一条是本组最近
            // 的编辑，它的 `new_selection` 才是整组完成后的光标位置。
            let restore = changes.last().and_then(|c| c.new_selection);
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.old_range);
                self.replace_text_in_range_silent(Some(range_utf16), &change.new_text, window, cx);
            }
            self.restore_selection(restore);
            self.update_preferred_column();
            self.scroll_to(self.cursor(), None, cx);
        }
        self.history.ignore = false;
        self.history.break_group();
        cx.notify();
    }

    /// Get byte offset of the cursor.
    ///
    /// The offset is the UTF-8 offset, and is always clamped into the current
    /// text and snapped to a char boundary: `selected_range` / `ime_marked_range`
    /// can transiently point past the end (external `set_value`, IME
    /// composition while the text shrinks), which would make the ubiquitous
    /// `text.slice(..cursor)` panic.
    pub fn cursor(&self) -> usize {
        let offset = if let Some(ime_marked_range) = &self.ime_marked_range {
            ime_marked_range.end
        } else if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        };

        self.text
            .clip_offset(offset.min(self.text.len()), Bias::Left)
    }

    /// 正文左缘的窗口 x（= 输入框左缘 + 行号栏宽）。
    ///
    /// 行号栏 / 行底色 / git 色条等 gutter 元素都按 `input_bounds` 固定绘制，
    /// **不随横向滚动平移**；只有正文跟着 `scroll_handle.offset().x` 平移。
    /// 命中测试里凡是要区分「点在 gutter 上」还是「点在正文上」的地方都用它。
    pub(crate) fn content_left_x(&self) -> Pixels {
        let gutter = self
            .last_layout
            .as_ref()
            .map(|l| l.line_number_width)
            .unwrap_or(px(0.));
        self.input_bounds.origin.x + gutter
    }

    /// 鼠标位置 → 字节偏移。
    ///
    /// 公开（去掉 `pub(crate)`）供 LSP 的悬浮命中测试使用：LSP 代码在
    /// 别的模块里，调不到 `pub(crate)` 的原方法。
    pub fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
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
        // gutter 的 x 不是内容坐标：它固定绘制、不随横向滚动平移，而
        // `bounds.origin` 含横滚偏移，两者相减会把「点在行号上的 x」算成
        // 一段很深的内容列（横滚 600px 后点行号 → 跳到第几十列）。
        // 落在 gutter 里的指针一律钳到内容左缘（当前可见首列）。
        let inner_position = {
            let content_left = self.content_left_x();
            if position.x < content_left {
                point(
                    content_left - bounds.origin.x - line_number_width,
                    inner_position.y,
                )
            } else {
                inner_position
            }
        };

        let mut y_offset = last_layout.visible_top;

        // Traverse visible buffer lines (compact, no hidden entries)
        for (vi, (line_layout, _buffer_line)) in last_layout
            .lines
            .iter()
            .zip(last_layout.visible_buffer_lines.iter())
            .enumerate()
        {
            // git 展开区：锚在当前行上的块高先行（与绘制阶段同一累计）。
            // 点击落在展开区内时，会越过块命中其后第一行的行首。
            y_offset += line_height * last_layout.git_insert_delta(vi) as f32;

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

    /// The previous cursor position, never resting inside a line terminator.
    ///
    /// A `\r\n` pair counts as a single stop: stepping left from the start of a
    /// line lands on the end of the previous line's *content*, not between
    /// `\r` and `\n`.
    pub(crate) fn previous_boundary(&self, offset: usize) -> usize {
        let mut offset = self.text.clip_offset(offset.saturating_sub(1), Bias::Left);
        // Step back over the terminator. `\n` and `\r` are ASCII, so the
        // single-byte step stays on a char boundary; `saturating_sub` guards
        // the start of the document, where the old `offset -= 1` underflowed
        // whenever the text began with `\r\n`.
        if self.text.char_at(offset) == Some('\n') {
            offset = offset.saturating_sub(1);
        }
        self.clamp_offset_to_visible_backward(offset)
    }

    /// The next cursor position, skipping whole line terminators.
    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        let mut offset = self.text.clip_offset(offset.saturating_add(1), Bias::Right);
        if self.text.char_at(offset) == Some('\r') && self.text.char_at(offset + 1) == Some('\n') {
            offset += 2;
        } else if self.text.char_at(offset) == Some('\n') {
            offset += 1;
        }
        self.clamp_offset_to_visible_forward(offset)
    }

    /// 光标当前的绘制不透明度（0.0 = 完全隐藏，1.0 = 实心）。
    ///
    /// 与终端一致：闪烁相位末尾 100ms 线性淡出 / 淡入。失焦 / 禁用 / 常显
    /// 模式下恒为 1.0。
    ///
    /// 注意淡入窗口内 `blink.visible()` 仍为 false（它是布尔语义），
    /// 所以绘制侧要同时接受「淡入中」：见 [`Self::should_paint_cursor`]。
    pub(crate) fn cursor_opacity(&self, window: &Window, cx: &App) -> f32 {
        if !self.focus_handle.is_focused(window) || self.disabled || !window.is_window_active()
        {
            return 1.0;
        }
        self.blink_cursor.read(cx).opacity()
    }

    /// 本帧是否应当绘制光标。
    ///
    /// 与单纯的 `blink.visible()` 的区别：淡入窗口内它仍是 false
    /// （相位尚未翻转），但透明度已经大于 0——此时必须绘制，否则淡入会
    /// 变成「到点突然出现」。
    pub(crate) fn should_paint_cursor(&self, window: &Window, cx: &App) -> bool {
        if !self.focus_handle.is_focused(window) || self.disabled || !window.is_window_active()
        {
            return false;
        }
        let blink = self.blink_cursor.read(cx);
        blink.visible() || blink.needs_repaint()
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.effective_cursor_shape()
    }

    pub(crate) fn effective_cursor_shape(&self) -> CursorShape {
        if self.masked || self.ime_marked_range.is_some() {
            return CursorShape::Bar;
        }
        // vim 启用时由模式决定：Normal/Visual 用块状，Insert 沿用编辑器设置。
        if let Some(vim) = self.vim.as_ref() {
            return vim.cursor_shape(self.cursor_shape);
        }
        self.cursor_shape
    }

    pub fn set_cursor_shape(&mut self, shape: CursorShape, cx: &mut Context<Self>) {
        if self.cursor_shape != shape {
            self.cursor_shape = shape;
            self.blink_cursor.update(cx, |cursor, cx| {
                cursor.show_cursor(cx);
            });
            cx.notify();
        }
    }

    pub fn set_cursor_blink(&mut self, blink: bool, cx: &mut Context<Self>) {
        if self.cursor_blink != blink {
            self.cursor_blink = blink;
            self.blink_cursor.update(cx, |cursor, cx| {
                cursor.set_blink_enabled(blink, cx);
            });
            cx.notify();
        }
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
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pause_blink_cursor(cx);

        if !self.should_intercept_key(event) {
            return;
        }

        // 消费掉这次击键，阻止平台输入处理器把它当字符插入。
        //
        // 这不会阻断 action 派发：绑定解析发生在 `dispatch_key_event` 的
        // 匹配阶段，`on_key_down` 监听器在其后的 `finish_dispatch_key_event`
        // 里才被调用（gpui `window.rs:5305` vs `:5417`）。所以已绑定的键
        // 照常触发 action，未绑定的键被安静吞掉——与 vim 一致。
        cx.stop_propagation();
    }

    /// 「等待一个字符」的命令（`f`/`t`/`F`/`T`/`r`）消费本次击键。
    ///
    /// 必须走 **App 级击键拦截器**（`App::intercept_keystrokes`），而不是
    /// `on_key_down` 或 `capture_key_down`：后两者都在 gpui 的
    /// `finish_dispatch_key_event` 里执行，而那一步在**绑定解析与 action 派发
    /// 之后**（`window.rs:5392` 派发 action，`:5402` 才调 key 监听器），
    /// 且 action 派发默认会中止事件传播。结果是 `f` 之后的 `o`（本身绑定到
    /// `vim::OpenLineBelow`）会被派发成命令，参数永远收不到。
    ///
    /// 拦截器在绑定解析**之前**运行（`window.rs:5297` vs `:5305`），
    /// 因此能稳定地把这类字符截为参数。
    pub(crate) fn on_intercept_keystroke(
        &mut self,
        event: &gpui::KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.vim_pending_char_is_set() {
            return false;
        }
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return false;
        }
        let Some(ch) = keystroke
            .key_char
            .as_deref()
            .and_then(|s| s.chars().next())
            .filter(|ch| !ch.is_control())
        else {
            return false;
        };

        self.vim_do_apply_pending_char(ch, window, cx);
        // 返回 true = 已消费，后续不再解析绑定。
        true
    }

    /// 是否有待决的字符输入（`f`/`t`/`r` 正在等字符）。
    fn vim_pending_char_is_set(&self) -> bool {
        self.vim
            .as_ref()
            .is_some_and(|vim| vim.pending_char.is_some())
    }

    /// 这次击键是否应由 vim 拦截（即：不该作为文本插入）。
    ///
    /// 只拦「一定是命令」的输入：Normal/Visual 模式下、无 Cmd/Ctrl 修饰、
    /// 且不在 IME 组合中。Insert 模式、快捷键组合、IME 组字一律放行，
    /// 否则会破坏中文输入法与系统快捷键。
    fn should_intercept_key(&self, event: &KeyDownEvent) -> bool {
        let Some(vim) = self.vim.as_ref() else {
            return false;
        };
        if !vim.blocks_text_input() {
            return false;
        }
        if self.ime_marked_range.is_some() {
            return false;
        }
        if self.disabled {
            return false;
        }

        let modifiers = &event.keystroke.modifiers;
        !modifiers.platform && !modifiers.control
    }

    /// 测试入口：直接判定某个击键是否被拦截，不经过窗口事件。
    #[cfg(test)]
    pub(crate) fn should_intercept_key_for_test(&self, keystroke: &gpui::Keystroke) -> bool {
        self.should_intercept_key(&KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        })
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

    /// 是否真的挂了校验规则（`validate` 闭包或 `pattern` 正则）。
    ///
    /// 没有规则时 `is_valid_input` 必然返回 true，此时不必为了「校验」把整段
    /// 文本复制成 `String`（单行输入每次按键都会走到那里）。
    fn needs_validation(&self) -> bool {
        self.validate.is_some() || self.pattern.is_some()
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

    /// 同步元素尺寸，必要时让 display_map 按新宽度重排软换行。
    ///
    /// 判据必须是**换行宽度本身**，而不是 `bounds.size.width`：换行宽度 =
    /// 元素宽度 − 行号栏 − 右安全边距。行数越过 9999/99999 阈值（行号位数
    /// 变化）或开关折叠图标时，行号栏宽度变了而元素宽度没变，只比对元素宽度
    /// 会让 display_map 永远收不到更新，软换行一直停在陈旧宽度上。
    ///
    /// 另外这里读的是**本帧**的 `last_layout.line_number_width`（paint 结束前
    /// 刚写进去的），而不是上一帧算出的 `wrap_width` —— 后者滞后一帧。
    pub(crate) fn set_input_bounds(&mut self, new_bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        self.input_bounds = new_bounds;

        // 与 prepaint 中 `wrap_width` 的算法保持一致。
        let wrap_width = match self.last_layout.as_ref() {
            Some(last_layout) if self.soft_wrap && self.mode.is_multi_line() => Some(
                (new_bounds.size.width - last_layout.line_number_width - RIGHT_MARGIN).max(px(1.)),
            ),
            // None 表示不换行（内部按 Pixels::MAX 处理）
            _ => None,
        };

        // 宽度没变时 `on_layout_changed` 会直接返回，不会触发全文档重排。
        if self.display_map.on_layout_changed(wrap_width, cx) {
            self.mode.update_auto_grow(&self.display_map);
            cx.notify();
        }
    }

    pub(crate) fn selected_text(&self) -> RopeSlice<'_> {
        let range_utf16 = self.range_to_utf16(&self.selected_range.into());
        let range = self.range_from_utf16(&range_utf16);
        self.text.slice(range)
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
        // 记录 Insert 会话中键入的文本，供 `.` 重放整次修改。
        // 只记真实键入（`allow_auto_pair`）且处于 Insert 模式时：
        // 程序化插入（撤销回放、缩进、行操作）不属于「用户输入」。
        if allow_auto_pair
            && self
                .vim
                .as_ref()
                .is_some_and(|vim| vim.mode == crate::vim::VimMode::Insert)
        {
            self.vim_insert_buffer.push_str(new_text);
        }
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

        // 编辑前的选区：记进 undo 记录，撤销时才能把光标放回原处。
        let old_selection: Range<usize> = self.selected_range.into();
        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        let new_offset = (range.start + new_text.len()).min(self.text.len());

        if self.mode.is_single_line() && self.needs_validation() {
            let pending_text = self.text.to_string();
            // Check if the new text is valid
            if !self.is_valid_input(&pending_text, cx) {
                self.text = old_text;
                return;
            }
        }

        self.push_history(
            &old_text,
            &range,
            new_text,
            old_selection,
            new_offset..new_offset,
        );
        // IME 组字期间开的分组在这里收口：组字中途按 Esc 走的是 `unmark_text`，
        // 那条路径不经过本函数，必须自己复位，否则其后键入的第一个字符会被
        // 并进 IME 那一组一起撤销。
        self.history.end_grouping();
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        // Adjust folds before updating wrap map (zed's offset-based model:
        // edits inside a fold keep it; boundary-overlapping edits drop it).
        self.display_map.adjust_folds_for_edit(&range, new_text);
        self.adjust_git_diff_for_edit(&range, new_text);
        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        // Bump the version *before* dispatching the background parse. The task
        // captures the current version and, on completion, drops its result if
        // the version moved on (the user kept typing). Dispatching first and
        // bumping afterwards made that comparison always true, so every
        // background tree was silently discarded.
        self.bump_text_version();

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

                // 后台解析落地：同步刷新语法诊断。同步解析超时的那次
                // 没有更新诊断（保留旧值避免闪烁），这里是补上的时机。
                entity
                    .update(cx, |state, cx| {
                        let items = syntax_diagnostics_opt(
                            highlighter_rc
                                .borrow()
                                .as_ref()
                                .and_then(|h| h.tree()),
                        );
                        if let Some(diagnostics) = state.mode.diagnostics_mut() {
                            diagnostics.set(items, &state.text);
                        }
                        cx.notify();
                    })
                    .ok();
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
            reversed: self.selection_reversed,
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
        // Esc 取消组字。分组是在 `replace_and_mark_text_in_range` 里开的，而
        // 那条路径不经过 `replace_text_in_range_inner`（后者负责收口），所以
        // 必须在这里显式结束：否则 `grouping` 一直为真，之后键入的第一个字符
        // 会被并进刚才那次 IME 组里，一次撤销就全没了。
        self.history.end_grouping();
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

        // 组字开始前的选区，供 undo 恢复光标。
        let old_selection: Range<usize> = self.selected_range.into();
        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        if self.mode.is_single_line() && self.needs_validation() {
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
        self.adjust_git_diff_for_edit(&range, new_text);
        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        // IME 组字同样是文档编辑：每次组字更新都要推进版本号，否则脏检查闸门、
        // 同词高亮缓存、后台解析的 stale 判定全会把它当成「文本没变」。
        self.bump_text_version();

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
        self.push_history(
            &old_text,
            &range,
            new_text,
            old_selection,
            self.selected_range.into(),
        );
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
        let bounds = self.last_bounds?;
        let mut line_point = bounds.localize(&point)?;
        // 坐标系：`localize` 得到的是元素内坐标，而正文从**行号栏之后**开始
        // 绘制（见 paint 段 `origin.x + line_number_width`），这里不减行号栏
        // 就会把 IME 定位整体右移一个 gutter。gutter 又不随横向滚动平移，
        // 所以落在 gutter 里的点另钳到内容左缘（与 `index_for_mouse_position`
        // 同一套规则）。
        let content_left = self.content_left_x();
        line_point.x = if point.x < content_left {
            content_left - bounds.origin.x - last_layout.line_number_width
        } else {
            line_point.x - last_layout.line_number_width
        };

        for (vi, line) in last_layout.lines.iter().enumerate() {
            let offset = last_layout.visible_line_byte_offsets[vi];
            if let Some(utf8_index) = line.index_for_position(line_point, last_layout) {
                return Some(self.offset_to_utf16(offset + utf8_index));
            }
        }

        None
    }

    /// Normal/Visual 模式下不接受文本输入。
    ///
    /// 这是按键拦截的第二道防线：`on_key_down` 的 `stop_propagation` 挡住
    /// 平台输入处理器，而这里告诉 gpui「当前视图不参与文本输入」，
    /// 使 IME 与输入法切换的判定也正确。仓库里已有两处先例：
    /// `markdown_live/view.rs` 与 `terminal_view.rs`。
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        match self.vim.as_ref() {
            Some(vim) => !vim.blocks_text_input(),
            None => true,
        }
    }
}

crate::impl_focusable!(InputState);

/// 整行复制的剪贴板元数据（Zed `ClipboardSelection` 单光标子集）。
/// `len` 为文本字节数，`first_line_indent` 为复制时首行缩进列（供后续自动缩进用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardSelection {
    len: usize,
    is_entire_line: bool,
    first_line_indent: u32,
}

impl ClipboardSelection {
    fn entire_line(len: usize, first_line_indent: u32) -> Self {
        Self {
            len,
            is_entire_line: true,
            first_line_indent,
        }
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
        //
        // 补的换行符要沿用文档自身的风格：CRLF 文档补 `\n` 会制造混用行尾。
        // 末行本身没有终止符，所以从上一行推断，没有上一行时回退到 `\n`。
        let end = text.line_content_end_offset(row);
        let mut content = text.slice(start..end).to_string();
        if row > 0 && text.line_terminator_len(row - 1) == 2 {
            content.push_str("\r\n");
        } else {
            content.push('\n');
        }
        (content, start..end)
    }
}

#[cfg(test)]
#[path = "lsp_anchor_tests.rs"]
mod lsp_anchor_tests;

#[cfg(test)]
#[path = "lsp_paint_tests.rs"]
mod lsp_paint_tests;

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
