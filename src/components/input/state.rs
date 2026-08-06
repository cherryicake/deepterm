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
use std::ops::Range;
use std::rc::Rc;
use sum_tree::Bias;
use unicode_segmentation::*;

use super::{DisplayMap, MASK_CHAR, blink_cursor::BlinkCursor, mode::InputMode};
use crate::components::button::Size;
use crate::components::input::blink_cursor::CURSOR_WIDTH;
use crate::components::input::change::Change;
use crate::components::input::history::History;
use crate::components::input::movement::MoveDirection;
use crate::components::input::{
    RopeExt as _, Selection, display_map::LineLayout, element::RIGHT_MARGIN,
};
use crate::syntax::DiagnosticSet;
#[cfg(not(target_family = "wasm"))]
use crate::syntax::LanguageRegistry;
use lsp_types::Position;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = editor, no_json)]
pub struct Enter {
    /// Is confirm with secondary.
    pub secondary: bool,
}

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
        ToggleCodeActions,
        Search,
        GoToDefinition,
    ]
);

#[derive(Clone)]
pub enum InputEvent {
    Change,
    PressEnter { secondary: bool },
    Focus,
    Blur,
}

pub(crate) const CONTEXT: &str = "Input";

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
        KeyBinding::new("cmd-.", ToggleCodeActions, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-.", ToggleCodeActions, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-f", Search, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-f", Search, Some(CONTEXT)),
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
    pub(crate) show_whitespaces: bool,
    /// This flag tells the renderer to prefer the end of the current visual line.
    pub(crate) cursor_line_end_affinity: bool,
    pub(crate) pattern: Option<regex::Regex>,
    pub(crate) validate: Option<Box<dyn Fn(&str, &mut Context<Self>) -> bool + 'static>>,
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
    /// If true, will call some update (for example LSP, Syntax Highlight) before render.
    pub(crate) _pending_update: bool,
    /// A flag to indicate if we should ignore the next completion event.
    pub(crate) silent_replace_text: bool,

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
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<InputEvent> for InputState {}

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
            selected_word_range: None,
            selection_reversed: false,
            ime_marked_range: None,
            input_bounds: Bounds::default(),
            selecting: false,
            disabled: false,
            masked: false,
            clean_on_escape: false,
            soft_wrap: true,
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
            silent_replace_text: false,
            size: Size::default(),
            _subscriptions,
            _pending_update: false,
            cursor_line_end_affinity: false,
            diff_line_colors: std::collections::HashMap::new(),
            diff_range_colors: Vec::new(),
            diff_line_labels: None,
            external_highlight_runs: None,
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
        match &mut self.mode {
            InputMode::CodeEditor {
                language,
                highlighter,
                parse_task,
                ..
            } => {
                *language = new_language.into();
                *highlighter.borrow_mut() = None;
                parse_task.borrow_mut().take();
            }
            _ => {}
        }
        cx.notify();
    }

    fn reset_highlighter(&mut self, cx: &mut Context<Self>) {
        match &mut self.mode {
            InputMode::CodeEditor {
                highlighter,
                parse_task,
                ..
            } => {
                *highlighter.borrow_mut() = None;
                parse_task.borrow_mut().take();
            }
            _ => {}
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
    /// async completion paths (e.g. background file loads). It updates the rope, display map,
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

    /// Set whether to show whitespace characters.
    pub fn show_whitespaces(mut self, show: bool) -> Self {
        self.show_whitespaces = show;
        self
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
            return next_indent;
        } else {
            return current_indent;
        }
    }

    pub(crate) fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
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
            // Get current line indent
            let indent = if self.mode.is_code_editor() {
                self.indent_of_next_line()
            } else {
                "".to_string()
            };

            // Add newline and indent
            let new_line_text = format!("\n{}", indent);
            self.replace_text_in_range_silent(None, &new_line_text, window, cx);
            self.pause_blink_cursor(cx);
        } else {
            // Single line input, just emit the event (e.g.: In a dialog to confirm).
            cx.propagate();
        }

        cx.emit(InputEvent::PressEnter {
            secondary: action.secondary,
        });
    }

    pub(crate) fn clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text("", window, cx);
        self.selected_range = (0..0).into();
        self.scroll_to(0, None, cx);
    }

    pub(crate) fn escape(&mut self, _action: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.clean_on_escape {}

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
            if ime_marked_range.len() == 0 {
                self.ime_marked_range = None;
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

    pub(crate) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Check if mouse is within bounds
        let _within_bounds = self
            .last_bounds
            .as_ref()
            .map(|bounds| bounds.contains(&event.position))
            .unwrap_or(false);
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
        // If row offset is not in the viewport, scroll to make it visible
        let edge_height = if direction.is_some() && self.mode.is_code_editor() {
            3 * line_height
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

    pub(crate) fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    pub(crate) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }

        let selected_text = self.text.slice(self.selected_range).to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
    }

    pub(crate) fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }

        let selected_text = self.text.slice(self.selected_range).to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(selected_text));

        self.replace_text_in_range_silent(None, "", window, cx);
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
                let range_utf16 = self.range_to_utf16(&change.new_range.into());
                self.replace_text_in_range_silent(Some(range_utf16), &change.old_text, window, cx);
            }
        }
        self.history.ignore = false;
    }

    pub(crate) fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.redo() {
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.old_range.into());
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

    /// If offset falls on a hidden (folded) line, clamp backward to the end of
    /// the fold header line (last visible position before the fold).
    fn clamp_offset_to_visible_backward(&self, offset: usize) -> usize {
        let line = self.text.offset_to_point(offset).row;
        if self.display_map.is_buffer_line_hidden(line) {
            for fold in self.display_map.folded_ranges() {
                if line > fold.start_line && line <= fold.end_line {
                    return self.text.line_end_offset(fold.start_line);
                }
            }
        }
        offset
    }

    /// If offset falls on a hidden (folded) line, clamp forward to the start of
    /// the fold end line (first visible position after the fold).
    fn clamp_offset_to_visible_forward(&self, offset: usize) -> usize {
        let line = self.text.offset_to_point(offset).row;
        if self.display_map.is_buffer_line_hidden(line) {
            for fold in self.display_map.folded_ranges() {
                if line > fold.start_line && line <= fold.end_line {
                    return self.text.line_start_offset(fold.end_line);
                }
            }
        }
        offset
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

    pub(crate) fn on_key_down(&mut self, _: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
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
        let Some(last_layout) = self.last_layout.as_ref() else {
            return None;
        };

        let Some(last_bounds) = self.last_bounds else {
            return None;
        };

        let (_, _, start_pos) = self.line_and_position_for_offset(range.start);
        let (_, _, end_pos) = self.line_and_position_for_offset(range.end);

        let Some(start_pos) = start_pos else {
            return None;
        };
        let Some(end_pos) = end_pos else {
            return None;
        };

        Some(Bounds::from_corners(
            last_bounds.origin + start_pos,
            last_bounds.origin + end_pos + point(px(0.), last_layout.line_height),
        ))
    }

    /// Replace text in range in silent.
    ///
    /// This will not trigger any UI interaction, such as auto-completion.
    pub(crate) fn replace_text_in_range_silent(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.silent_replace_text = true;
        self.replace_text_in_range(range_utf16, new_text, window, cx);
        self.silent_replace_text = false;
    }

    /// Update fold candidates from tree-sitter syntax tree (full extraction).
    /// Used only on initial load or language changes.
    pub(crate) fn update_fold_candidates(&mut self) {
        if !self.mode.is_folding() {
            return;
        }

        let Some(highlighter_rc) = self.mode.highlighter() else {
            return;
        };

        let highlighter = highlighter_rc.borrow();
        let Some(highlighter) = highlighter.as_ref() else {
            return;
        };

        let Some(tree) = highlighter.tree() else {
            return;
        };

        let fold_ranges = crate::components::input::display_map::extract_fold_ranges(tree);
        self.display_map.set_fold_candidates(fold_ranges);
    }

    /// Incrementally update fold candidates after a text edit.
    /// Only traverses the edited region of the syntax tree instead of the full tree.
    fn update_fold_candidates_incremental(&mut self, edit_range: &Range<usize>, new_text: &str) {
        if !self.mode.is_folding() {
            return;
        }

        let Some(highlighter_rc) = self.mode.highlighter() else {
            return;
        };

        let highlighter = highlighter_rc.borrow();
        let Some(highlighter) = highlighter.as_ref() else {
            return;
        };

        let Some(tree) = highlighter.tree() else {
            return;
        };

        // The new byte range in the updated text after the edit
        let new_end = edit_range.start + new_text.len();
        self.display_map.update_fold_candidates_for_edit(
            tree,
            edit_range.start..new_end,
            &self.text,
        );
    }

    /// Spawn a background parse after the synchronous parse timed out.
    ///
    /// Dropping the returned `Task` (stored in `parse_task`) cancels the
    /// parse, which naturally debounces rapid edits.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn dispatch_background_parse(
        pending: super::mode::PendingBackgroundParse,
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

        let text_for_apply = text.clone();
        let task = cx.spawn_in(window, async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let Some(config) = LanguageRegistry::singleton().language(&language) else {
                        return None;
                    };

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
                        crate::syntax::SyntaxHighlighter::compute_injection_layers(
                            data, &new_tree, &text,
                        )
                    } else {
                        Default::default()
                    };

                    Some((new_tree, injection_layers))
                })
                .await;

            if let Some((new_tree, injection_layers)) = result {
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
        if self.disabled {
            return;
        }

        self.pause_blink_cursor(cx);

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

        self.push_history(&old_text, &range, &new_text);
        self.history.end_grouping();
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        // Adjust folds before updating wrap map: remove overlapping folds and shift others
        self.display_map
            .adjust_folds_for_edit(&old_text, &range, new_text);
        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        let bg = self
            .mode
            .update_highlighter(&range, &self.text, &new_text, true, cx);
        if let Some(bg) = bg {
            Self::dispatch_background_parse(bg, window, cx);
        }

        self.update_fold_candidates_incremental(&range, new_text);
        self.selected_range = (new_offset..new_offset).into();
        self.ime_marked_range.take();
        self.update_preferred_column();
        self.mode.update_auto_grow(&self.display_map);
        cx.emit(InputEvent::Change);
        cx.notify();
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
        // Adjust folds before updating wrap map: remove overlapping folds and shift others
        self.display_map
            .adjust_folds_for_edit(&old_text, &range, new_text);
        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        let bg = self
            .mode
            .update_highlighter(&range, &self.text, &new_text, true, cx);
        if let Some(bg) = bg {
            Self::dispatch_background_parse(bg, window, cx);
        }

        self.update_fold_candidates_incremental(&range, new_text);
        if new_text.is_empty() {
            // Cancel selection, when cancel IME input.
            self.selected_range = (range.start..range.start).into();
            self.ime_marked_range = None;
        } else {
            self.ime_marked_range = Some((range.start..range.start + new_text.len()).into());
            self.selected_range = new_selected_range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .map(|new_range| new_range.start + range.start..new_range.end + range.end)
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

#[cfg(test)]
mod tests {
    // Tests disabled: gpui testing utilities not available in DeepTerm
}
