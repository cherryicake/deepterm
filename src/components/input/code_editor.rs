//! 多行代码编辑器组件
//!
//! 基于 DeepTerm 现有 Input 组件扩展，支持多行文本编辑和语法高亮。
use crate::theme::ActiveTheme;
use super::change::Change;
use super::history::History;
use gpui::prelude::*;
use gpui::{
    App, Bounds, ContentMask, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, IntoElement,
    KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, PaintQuad, Pixels, Point,
    Render, ScrollHandle, ScrollWheelEvent, ShapedLine, Style, TextAlign, TextRun,
    UTF16Selection, Window, actions, div, fill, point, px, size,
};
use ropey::{LineType, Rope};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub(super) const CONTEXT: &str = "Editor";

actions!(
    editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectAll,
        Home,
        End,
        Copy,
        Cut,
        Paste,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        Undo,
        Redo,
        Enter,
        Save,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("cmd-s", Save, Some(CONTEXT)),
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
        KeyBinding::new("cmd-z", Undo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-z", Redo, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
    ]);
}

const CURSOR_BLINK_INTERVAL: u64 = 530;
const CURSOR_WIDTH: Pixels = px(2.);
const GUTTER_WIDTH: Pixels = px(60.);
const GUTTER_RIGHT_PADDING: Pixels = px(10.);

/// 多行编辑器组件
pub struct Editor {
    focus_handle: FocusHandle,
    text: Rope,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    cursor_visible: bool,
    blinking_started: bool,
    is_selecting: bool,
    scroll_handle: ScrollHandle,
    scroll_size: gpui::Size<Pixels>,
    input_bounds: Bounds<Pixels>,
    last_layout_lines: Option<Vec<(ShapedLine, Bounds<Pixels>, usize, usize)>>,
    history: History<Change>,
    silent_replace: bool,
    was_focused: bool,
    language: Option<String>,
    highlighter: Option<crate::highlighter::SimpleHighlighter>,
    on_change: Option<Box<dyn Fn(&str, &mut App) + Send + Sync>>,
    on_save: Option<Box<dyn Fn(&str, &mut App) + Send + Sync>>,
    /// The deferred scroll offset to apply on next layout.
    deferred_scroll_offset: Option<Point<Pixels>>,
    /// Line height from the last paint, used for scroll_to calculations.
    last_line_height: Option<Pixels>,
}

impl Editor {
    pub fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            text: Rope::new(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            cursor_visible: true,
            blinking_started: false,
            is_selecting: false,
            scroll_handle: ScrollHandle::new(),
            scroll_size: gpui::size(px(0.), px(0.)),
            input_bounds: Bounds::default(),
            last_layout_lines: None,
            history: History::new().group_interval(std::time::Duration::from_secs(1)),
            silent_replace: false,
            was_focused: false,
            language: None,
            highlighter: None,
            on_change: None,
            on_save: None,
            deferred_scroll_offset: None,
            last_line_height: None,
        }
    }

    pub fn with_text(mut self, text: &str) -> Self {
        self.text = Rope::from(text);
        self.selected_range = 0..0;
        self
    }

    pub fn language(mut self, lang: impl Into<String>) -> Self {
        let lang = lang.into();
        self.highlighter = Some(crate::highlighter::SimpleHighlighter::new(&lang));
        self.language = Some(lang);
        self
    }

    pub fn on_change<F>(mut self, callback: F) -> Self
    where
        F: Fn(&str, &mut App) + Send + Sync + 'static,
    {
        self.on_change = Some(Box::new(callback));
        self
    }

    pub fn on_save<F>(mut self, callback: F) -> Self
    where
        F: Fn(&str, &mut App) + Send + Sync + 'static,
    {
        self.on_save = Some(Box::new(callback));
        self
    }

    pub fn text(&self) -> String {
        self.text.to_string()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.text = Rope::from(text);
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn line_height(&self, window: &mut Window) -> Pixels {
        window.line_height()
    }

    fn line_count(&self) -> usize {
        self.text.len_lines(LineType::LF)
    }

    fn line_start(&self, line: usize) -> usize {
        self.text.line_to_byte_idx(line.min(self.line_count().saturating_sub(1)), LineType::LF)
    }

    fn line_end(&self, line: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let start = self.text.line_to_byte_idx(line, LineType::LF);
        let line_text = self.text.line(line, LineType::LF).to_string();
        let display_text = line_text.trim_end_matches('\n').trim_end_matches('\r');
        start + display_text.len()
    }

    fn offset_to_line_col(&self, offset: usize) -> (usize, usize) {
        let line = self.text.byte_to_line_idx(offset, LineType::LF);
        let line_start = self.text.line_to_byte_idx(line, LineType::LF);
        let col = offset.saturating_sub(line_start);
        (line, col)
    }

    fn line_col_to_offset(&self, line: usize, col: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let line_start = self.text.line_to_byte_idx(line, LineType::LF);
        let line_text = self.text.line(line, LineType::LF).to_string();
        let display_text = line_text.trim_end_matches('\n').trim_end_matches('\r');
        let max_col = display_text.len();
        line_start + col.min(max_col)
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp_to_char_boundary(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.scroll_to(offset, cx);
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp_to_char_boundary(offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.scroll_to(self.cursor_offset(), cx);
        cx.notify();
    }

    /// Scroll to make the given offset visible.
    fn scroll_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let Some(lines) = self.last_layout_lines.as_ref() else {
            return;
        };
        let (row, _) = self.offset_to_line_col(offset);
        let Some((shaped, _, line_start, _)) = lines.get(row) else {
            return;
        };

        let mut scroll_offset = self.scroll_handle.offset();
        let line_height = self.last_line_height.unwrap_or_else(|| px(16.));
        let row_offset_y = line_height * row as f32;

        let cursor_byte = offset.max(*line_start);
        let cursor_char = self.text.slice(*line_start..cursor_byte).chars().count();
        let col_offset_x = shaped.x_for_index(cursor_char);

        let bounds_width = self.input_bounds.size.width - GUTTER_WIDTH;
        let bounds_height = self.input_bounds.size.height;
        let safety_margin = CURSOR_WIDTH;

        // horizontal
        if col_offset_x + scroll_offset.x < safety_margin {
            scroll_offset.x = safety_margin - col_offset_x;
        } else if col_offset_x + scroll_offset.x + safety_margin > bounds_width {
            scroll_offset.x = bounds_width - col_offset_x - safety_margin;
        }

        // vertical
        let edge_height = line_height;
        if row_offset_y + scroll_offset.y < px(0.) {
            scroll_offset.y = -row_offset_y;
        } else if row_offset_y + scroll_offset.y + edge_height > bounds_height {
            scroll_offset.y = bounds_height - row_offset_y - edge_height;
        }

        scroll_offset.x = scroll_offset.x.min(px(0.));
        scroll_offset.y = scroll_offset.y.min(px(0.));
        self.deferred_scroll_offset = Some(scroll_offset);
        cx.notify();
    }

    fn clamp_to_char_boundary(&self, offset: usize) -> usize {
        let len = self.text.len();
        if offset >= len {
            return len;
        }
        let mut o = offset;
        while o > 0 && !self.text.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    fn start_blinking(&mut self, cx: &mut Context<Self>) {
        if self.blinking_started {
            return;
        }
        self.blinking_started = true;
        self.cursor_visible = true;
        self.blink_cursors(cx);
    }

    fn stop_blinking(&mut self, cx: &mut Context<Self>) {
        self.blinking_started = false;
        self.cursor_visible = false;
        cx.notify();
    }

    fn blink_cursors(&mut self, cx: &mut Context<Self>) {
        if !self.blinking_started {
            return;
        }
        self.cursor_visible = !self.cursor_visible;
        cx.notify();
        let interval = std::time::Duration::from_millis(CURSOR_BLINK_INTERVAL);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.blink_cursors(cx));
            }
        })
        .detach();
    }

    fn reset_blinking(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        let s = self.text.to_string();
        s.grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let s = self.text.to_string();
        s.grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(s.len())
    }

    fn notify_change(&self, cx: &mut Context<Self>) {
        if let Some(ref callback) = self.on_change {
            callback(&self.text.to_string(), cx);
        }
    }

    fn finish_edit(&mut self, cx: &mut Context<Self>) {
        self.reset_blinking(cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
        self.finish_edit(cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
        self.finish_edit(cx);
    }

    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        let indent = self.indent_of_current_line();
        let new_line_text = format!("\n{}", indent);
        self.replace_text_in_range(None, &new_line_text, window, cx);
        self.finish_edit(cx);
    }

    /// Get leading whitespace of the current line for auto-indent on Enter.
    fn indent_of_current_line(&self) -> String {
        let line_start = self.line_start(self.offset_to_line_col(self.cursor_offset()).0);
        self.text
            .slice(line_start..)
            .chars()
            .take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
            .collect()
    }

    fn left(&mut self, _: &Left, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _window: &mut Window, cx: &mut Context<Self>) {
        let (line, col) = self.offset_to_line_col(self.cursor_offset());
        if line > 0 {
            self.move_to(self.line_col_to_offset(line - 1, col), cx);
        }
    }

    fn down(&mut self, _: &Down, _window: &mut Window, cx: &mut Context<Self>) {
        let (line, col) = self.offset_to_line_col(self.cursor_offset());
        if line + 1 < self.line_count() {
            self.move_to(self.line_col_to_offset(line + 1, col), cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _window: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        };
        let (line, col) = self.offset_to_line_col(offset);
        if line > 0 {
            self.select_to(self.line_col_to_offset(line - 1, col), cx);
        }
    }

    fn select_down(&mut self, _: &SelectDown, _window: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        };
        let (line, col) = self.offset_to_line_col(offset);
        if line + 1 < self.line_count() {
            self.select_to(self.line_col_to_offset(line + 1, col), cx);
        }
    }

    fn home(&mut self, _: &Home, _window: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self.offset_to_line_col(self.cursor_offset());
        self.move_to(self.line_start(line), cx);
    }

    fn end(&mut self, _: &End, _window: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self.offset_to_line_col(self.cursor_offset());
        self.move_to(self.line_end(line), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.text.len(), cx);
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            let selected_text = &self.text.to_string()[self.selected_range.clone()];
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(selected_text.to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        self.copy(&Copy, window, cx);
        self.replace_text_in_range(None, "", window, cx);
        self.finish_edit(cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(clipboard) = cx.read_from_clipboard() {
            if let Some(text) = clipboard.text() {
                if !text.is_empty() {
                    self.replace_text_in_range(None, &text, window, cx);
                    self.finish_edit(cx);
                }
            }
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.undo() {
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.new_range);
                self.replace_text_in_range_silent(Some(range_utf16), &change.old_text, window, cx);
            }
        }
        self.history.ignore = false;
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.redo() {
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.old_range);
                self.replace_text_in_range_silent(Some(range_utf16), &change.new_text, window, cx);
            }
        }
        self.history.ignore = false;
    }

    fn save(&mut self, _: &Save, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ref callback) = self.on_save {
            callback(&self.text.to_string(), cx);
        }
    }

    fn replace_text_in_range_silent(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.silent_replace = true;
        self.replace_text_in_range(range_utf16, new_text, window, cx);
        self.silent_replace = false;
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.text.len() == 0 {
            return 0;
        }

        let Some(lines) = self.last_layout_lines.as_ref() else {
            return 0;
        };

        // Position relative to the bounds origin (includes padding and scroll offset)
        let inner_position = position - self.input_bounds.origin;

        for (shaped, line_bounds, line_start, line_len) in lines {
            // Position relative to this visual line's top-left
            let pos = inner_position - line_bounds.origin + self.input_bounds.origin;

            // Check if mouse is within this line's vertical bounds
            if pos.y >= px(0.) && pos.y < line_bounds.size.height {
                let local_index = shaped.closest_index_for_x(pos.x).min(*line_len);
                return (line_start + local_index).min(self.text.len());
            } else if pos.y < px(0.) {
                // Mouse is above this line, return start of this line
                return *line_start;
            }
        }

        // Mouse is below all lines, return end of text
        self.text.len()
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.start_blinking(cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        let offset = self.index_for_mouse_position(event.position);
        self.select_to(offset, cx);
    }

    fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_scroll_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let line_height = window.line_height();
        let delta = event.delta.pixel_delta(line_height);
        let mut offset = self.scroll_handle.offset() + delta;
        let max_scroll_y = if self.scroll_size.height > self.input_bounds.size.height {
            self.input_bounds.size.height - self.scroll_size.height
        } else {
            px(0.)
        };
        offset.y = offset.y.clamp(max_scroll_y, px(0.));
        // Horizontal scroll clamp (account for gutter width)
        let content_width = (self.input_bounds.size.width - GUTTER_WIDTH).max(px(0.));
        let max_scroll_x = if self.scroll_size.width > content_width {
            content_width - self.scroll_size.width
        } else {
            px(0.)
        };
        offset.x = offset.x.clamp(max_scroll_x, px(0.));
        self.scroll_handle.set_offset(offset);
        cx.notify();
        cx.stop_propagation();
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.text.to_string().chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.text.to_string().chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _theme = cx.theme().ui;
        let is_focused = self.focus_handle.is_focused(window);
        if is_focused && !self.was_focused {
            self.start_blinking(cx);
        } else if !is_focused && self.blinking_started {
            self.stop_blinking(cx);
        }
        self.was_focused = is_focused;

        div()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Editor::backspace))
            .on_action(cx.listener(Editor::delete))
            .on_action(cx.listener(Editor::left))
            .on_action(cx.listener(Editor::right))
            .on_action(cx.listener(Editor::up))
            .on_action(cx.listener(Editor::down))
            .on_action(cx.listener(Editor::select_left))
            .on_action(cx.listener(Editor::select_right))
            .on_action(cx.listener(Editor::select_up))
            .on_action(cx.listener(Editor::select_down))
            .on_action(cx.listener(Editor::select_all))
            .on_action(cx.listener(Editor::home))
            .on_action(cx.listener(Editor::end))
            .on_action(cx.listener(Editor::copy))
            .on_action(cx.listener(Editor::cut))
            .on_action(cx.listener(Editor::paste))
            .on_action(cx.listener(Editor::undo))
            .on_action(cx.listener(Editor::redo))
            .on_action(cx.listener(Editor::enter))
            .on_action(cx.listener(Editor::save))
            .on_mouse_down(MouseButton::Left, cx.listener(Editor::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Editor::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Editor::on_mouse_up))
            .on_mouse_move(cx.listener(Editor::on_mouse_move))
            .child(
                div()
                    .size_full()
                    .overflow_hidden()
                    .on_scroll_wheel(cx.listener(Editor::on_scroll_wheel))
                    .child(EditorTextElement { editor: cx.entity() })
            )
    }
}

struct EditorTextElement {
    editor: Entity<Editor>,
}

struct EditorPrepaintState {
    lines: Vec<(ShapedLine, Bounds<Pixels>, usize, usize)>, // line, bounds, start_byte, line_len
    line_numbers: Vec<(ShapedLine, Point<Pixels>)>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    scroll_size: gpui::Size<Pixels>,
    scroll_offset: Point<Pixels>,
}

impl IntoElement for EditorTextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element { self }
}

impl Element for EditorTextElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaintState;

    fn id(&self) -> Option<ElementId> { None }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> { None }

    fn request_layout(&mut self, _id: Option<&GlobalElementId>, _inspector: Option<&gpui::InspectorElementId>, window: &mut Window, _cx: &mut App) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, [], _cx), ())
    }

    fn prepaint(&mut self, _id: Option<&GlobalElementId>, _inspector: Option<&gpui::InspectorElementId>, bounds: Bounds<Pixels>, _req: &mut Self::RequestLayoutState, window: &mut Window, cx: &mut App) -> Self::PrepaintState {
        let all_styles = self.editor.update(cx, |editor, cx| {
            editor.highlighter.as_mut().map(|h| h.styles(&editor.text, cx))
        });

        let editor = self.editor.read(cx);
        let text = editor.text.clone();
        let selected_range = editor.selected_range.clone();
        let cursor = editor.cursor_offset();
        let cursor_line = editor.offset_to_line_col(cursor).0;
        let line_height = editor.line_height(window);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let theme = cx.theme().ui;
        let is_focused = editor.focus_handle.is_focused(window);
        let mut scroll_offset = editor.scroll_handle.offset();
        if let Some(deferred) = editor.deferred_scroll_offset {
            scroll_offset = deferred;
        }

        let content_left = bounds.left() + GUTTER_WIDTH;

        let mut lines = Vec::new();
        let mut line_numbers = Vec::new();
        let mut max_width = px(0.);
        let line_count = text.len_lines(LineType::LF);
        for line_idx in 0..line_count {
            let line_start = text.line_to_byte_idx(line_idx, LineType::LF);
            let line_slice = text.line(line_idx, LineType::LF);
            let line_text = line_slice.to_string();
            // strip newline for display
            let display_text = line_text.trim_end_matches('\n').trim_end_matches('\r').to_string();
            let display_len = display_text.len();

            let runs: Vec<TextRun> = if display_len > 0 {
                let line_end = line_start + display_len;
                let mut line_styles = Vec::new();
                if let Some(ref styles) = all_styles {
                    for (range, hl_style) in styles {
                        let s = range.start.max(line_start).min(line_end);
                        let e = range.end.min(line_end).max(s);
                        if e > s {
                            line_styles.push((s - line_start, e - line_start, hl_style));
                        }
                    }
                }
                line_styles.sort_by(|a, b| a.0.cmp(&b.0));

                let mut runs = Vec::new();
                let mut pos = 0;
                for (s, e, hl_style) in line_styles {
                    if s > pos {
                        runs.push(TextRun {
                            len: s - pos,
                            font: style.font(),
                            color: theme.text,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        });
                    }
                    runs.push(TextRun {
                        len: e - s,
                        font: style.font(),
                        color: hl_style.color.unwrap_or(theme.text),
                        background_color: hl_style.background_color,
                        underline: hl_style.underline,
                        strikethrough: hl_style.strikethrough,
                    });
                    pos = e;
                }
                if pos < display_len {
                    runs.push(TextRun {
                        len: display_len - pos,
                        font: style.font(),
                        color: theme.text,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                }
                if runs.is_empty() {
                    runs.push(TextRun {
                        len: display_len,
                        font: style.font(),
                        color: theme.text,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                }
                runs
            } else {
                Vec::new()
            };

            let shaped = if display_len > 0 {
                window.text_system().shape_line(display_text.into(), font_size, &runs, None)
            } else {
                // empty line still needs height; shape a space to get metrics
                window.text_system().shape_line(" ".into(), font_size, &[
                    TextRun { len: 1, font: style.font(), color: theme.text, background_color: None, underline: None, strikethrough: None }
                ], None)
            };
            let y = bounds.top() + line_height * line_idx as f32 + scroll_offset.y;
            let line_bounds = Bounds::new(
                point(content_left + scroll_offset.x, y),
                size(shaped.width, line_height),
            );
            max_width = max_width.max(shaped.width);
            lines.push((shaped, line_bounds, line_start, display_len));

            // line number
            let num_text = format!("{}", line_idx + 1);
            let num_color = if line_idx == cursor_line { theme.text } else { theme.text_muted };
            let num_runs = vec![TextRun {
                len: num_text.len(),
                font: style.font(),
                color: num_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let num_shaped = window.text_system().shape_line(num_text.into(), font_size, &num_runs, None);
            let num_x = content_left - GUTTER_RIGHT_PADDING - num_shaped.width;
            let num_y = y;
            line_numbers.push((num_shaped, point(num_x, num_y)));
        }

        let scroll_size = gpui::Size {
            width: max_width,
            height: line_height * line_count as f32,
        };

        // selection quads
        let mut selection_quads = Vec::new();
        if !selected_range.is_empty() {
            for (shaped, line_bounds, line_start, _line_len) in &lines {
                let remaining = text.slice(*line_start..).to_string();
                let display_text = remaining.split(['\n', '\r']).next().unwrap_or("");
                let line_end_byte = line_start + display_text.len();
                let sel_start = selected_range.start.max(*line_start).min(line_end_byte);
                let sel_end = selected_range.end.min(line_end_byte).max(sel_start);
                if sel_end > sel_start {
                    let start_char = text.slice(*line_start..sel_start).chars().count();
                    let end_char = text.slice(*line_start..sel_end).chars().count();
                    let x1 = line_bounds.origin.x + shaped.x_for_index(start_char);
                    let x2 = line_bounds.origin.x + shaped.x_for_index(end_char);
                    selection_quads.push(fill(
                        Bounds::from_corners(
                            Point::new(x1, line_bounds.top()),
                            Point::new(x2, line_bounds.bottom()),
                        ),
                        theme.accent.opacity(if cx.theme().appearance.is_dark() { 0.24 } else { 0.18 }),
                    ));
                }
            }
        }

        // cursor
        let cursor_quad = if selected_range.is_empty() && is_focused && editor.cursor_visible {
            let (line_idx, _col) = editor.offset_to_line_col(cursor);
            if let Some((shaped, line_bounds, line_start, _line_len)) = lines.get(line_idx) {
                let cursor_byte = cursor.max(*line_start);
                let char_idx = text.slice(*line_start..cursor_byte).chars().count();
                let cursor_x = line_bounds.origin.x + shaped.x_for_index(char_idx);
                let cursor_y = line_bounds.top();
                Some(fill(
                    Bounds::new(point(cursor_x, cursor_y), size(CURSOR_WIDTH, line_height)),
                    theme.accent,
                ))
            } else {
                None
            }
        } else {
            None
        };

        EditorPrepaintState {
            lines,
            line_numbers,
            cursor: cursor_quad,
            selection: selection_quads,
            scroll_size,
            scroll_offset,
        }
    }

    fn paint(&mut self, _id: Option<&GlobalElementId>, _inspector: Option<&gpui::InspectorElementId>, bounds: Bounds<Pixels>, _req: &mut Self::RequestLayoutState, prepaint: &mut Self::PrepaintState, window: &mut Window, cx: &mut App) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.editor.clone()), cx);

        // Global drag listener so selection continues when mouse leaves the element
        window.on_mouse_event({
            let editor = self.editor.clone();
            move |event: &MouseMoveEvent, _, _window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    editor.update(cx, |editor, cx| {
                        if editor.is_selecting {
                            let offset = editor.index_for_mouse_position(event.position);
                            editor.select_to(offset, cx);
                        }
                    });
                }
            }
        });

        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (num_shaped, origin) in &prepaint.line_numbers {
                let _ = num_shaped.paint(*origin, window.line_height(), TextAlign::Left, None, window, cx);
            }
            for (line, line_bounds, _, _) in &prepaint.lines {
                let _ = line.paint(line_bounds.origin, window.line_height(), TextAlign::Left, None, window, cx);
            }
        });

        if let Some(cursor) = prepaint.cursor.take() {
            let cursor_left = cursor.bounds.left();
            let cursor_right = cursor.bounds.right();
            if cursor_left >= bounds.left() - px(2.) && cursor_right <= bounds.right() + px(2.) {
                window.paint_quad(cursor);
            }
        }

        let line_height = window.line_height();
        self.editor.update(cx, |editor, cx| {
            editor.scroll_size = prepaint.scroll_size;
            editor.input_bounds = bounds;
            editor.last_layout_lines = Some(prepaint.lines.clone());
            editor.last_line_height = Some(line_height);

            // Commit deferred scroll offset to scroll handle
            editor.scroll_handle.set_offset(prepaint.scroll_offset);
            editor.deferred_scroll_offset = None;
            cx.notify();
        });
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(&mut self, range_utf16: Range<usize>, actual_range: &mut Option<Range<usize>>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        let s = self.text.to_string();
        Some(s[range].to_string())
    }

    fn selected_text_range(&mut self, _ignore: bool, _window: &mut Window, _cx: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, _window: &mut Window, cx: &mut Context<Self>) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let len = self.text.len();
        let start = range.start.min(len);
        let end = range.end.min(len);

        if !self.silent_replace && !self.history.ignore {
            let old_text = self.text.slice(start..end).to_string();
            let new_range = start..start + new_text.len();
            self.history.push(Change::new(start..end, &old_text, new_range, new_text));
            self.history.end_grouping();
        }

        self.text.remove(start..end);
        self.text.insert(start, new_text);
        if let Some(highlighter) = self.highlighter.as_mut() {
            highlighter.update(&self.text);
        }
        self.selected_range = start + new_text.len()..start + new_text.len();
        self.marked_range.take();
        self.reset_blinking(cx);
        self.scroll_to(self.cursor_offset(), cx);
        self.notify_change(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, new_selected_range_utf16: Option<Range<usize>>, _window: &mut Window, cx: &mut Context<Self>) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let len = self.text.len();
        let start = range.start.min(len);
        let end = range.end.min(len);

        self.text.remove(start..end);
        self.text.insert(start, new_text);
        if !new_text.is_empty() {
            self.marked_range = Some(start..start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + start..r.end + end)
            .unwrap_or_else(|| start + new_text.len()..start + new_text.len());
        self.scroll_to(self.cursor_offset(), cx);
        cx.notify();
    }

    fn bounds_for_range(&mut self, range_utf16: Range<usize>, bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        // Simplified: return full bounds
        let range = self.range_from_utf16(&range_utf16);
        let s = self.text.to_string();
        let prefix = &s[..range.start.min(s.len())];
        let selected = &s[range.start.min(s.len())..range.end.min(s.len())];
        let avg_char_width = px(8.0);
        Some(Bounds::from_corners(
            Point::new(bounds.left() + avg_char_width * prefix.len() as f32, bounds.top()),
            Point::new(bounds.left() + avg_char_width * (prefix.len() + selected.len()) as f32, bounds.bottom()),
        ))
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        // Simplified fallback
        let local_f32: f32 = (point.x - self.input_bounds.left()).into();
        let avg_char_width: f32 = px(8.0).into();
        let idx = (local_f32 / avg_char_width.max(1.0)) as usize;
        Some(self.offset_to_utf16(idx.min(self.text.len())))
    }
}
