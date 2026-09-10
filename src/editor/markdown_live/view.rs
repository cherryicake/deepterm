//! [`LiveMarkdownView`]: the Obsidian-style live-preview markdown editor
//! entity. Owns a [`LiveBuffer`] (single truth), plans rows with reveal,
//! and renders decorations + [`LiveLine`](super::element::LiveLine) text.
//!
//! Rewritten for deepterm's GPUI fork; interaction semantics ported from
//! guise `markdown/editor.rs` (`on_key`/`on_mouse_down`/checkbox/link).

use std::collections::HashSet;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, ScrollHandle, ShapedLine, SharedString, StatefulInteractiveElement, Styled,
    StyledText, Subscription, TextRun, Window, div, px, rems, svg,
};

use crate::assets::Icon;
use crate::editor::markdown::{Copy, MarkdownStyle, SelectAll};
use crate::editor::markdown_element::MarkdownElementBuilder;
use crate::editor::markdown_live::block::{split_table_row, table_delimiter_cols, Block};
use crate::editor::markdown_live::buffer::LiveBuffer;
use crate::editor::markdown_live::element::{
    apply_selection, apply_task_done, inline_cell_runs, row_base_style, runs_for_row, LiveColors,
    LiveLine,
};
use crate::editor::markdown_live::highlight::HighlightCache;
use crate::editor::markdown_live::interactions::{
    backspace_marker, classify_alone, insert_link, line_selection, list_cols, on_enter, on_tab,
    table_insert_row_below, table_nav, word_stats,
    toggle_task, toggle_wrap, OrderedCounter,
};
use crate::editor::markdown_live::layout::{
    byte_for_col, col_for_byte, src_for_vis, vis_for_src, RowKind,
};
use crate::editor::markdown_live::model::Pos;
use crate::editor::markdown_live::reveal::{
    plan_document, strip_plan_prefix, CalloutKind, PlannedRow, Reveal,
};
use crate::components::input::{
    blink_cursor::{BlinkCursor, CURSOR_WIDTH},
    cursor::CursorShape,
};
use crate::theme::ActiveTheme;

#[derive(Debug, Clone)]
pub enum LiveViewEvent {
    Change(String),
    LinkClick(String),
}

pub struct LiveMarkdownView {
    buffer: LiveBuffer,
    focus: FocusHandle,
    scroll: ScrollHandle,
    hl: HighlightCache,
    tab_size: usize,
    read_only: bool,
    placeholder: SharedString,
    selecting: bool,
    goal_x: Option<f32>,
    planned_version: u64,
    planned_key: (Pos, Option<(Pos, Pos)>, bool, bool),
    planned: Vec<PlannedRow>,
    copied_code_blocks: HashSet<usize>,
    /// Bring the caret's row into view on the next render (set by every
    /// edit/move; consumed by `scroll_to_item` on the rows column).
    scroll_to_cursor: bool,
    /// Source-line range per rendered row element (dead-zone hit fallback:
    /// clicks between/around rows map to the nearest element, then to a
    /// line within it via the recorded layouts).
    row_targets: Vec<(usize, usize)>,
    /// Last frame's per-line text layouts, recorded during `LiveLine`
    /// paint. Powers visual-row (soft-wrap) movement and the dead-zone
    /// click/drag fallbacks.
    row_layouts: std::collections::HashMap<usize, RowLayoutInfo>,
    /// Fingerprint of the style inputs that bake colors into the highlight
    /// cache; a change (theme switch) clears it.
    hl_style_key: u64,
    blink: Entity<BlinkCursor>,
    cursor_shape: CursorShape,
    cursor_blink: bool,
    blink_on: bool,
    /// Typora 风格大纲侧栏是否展开（`Cmd+Shift+O` / 状态条切换）。
    show_outline: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LiveViewEvent> for LiveMarkdownView {}

impl LiveMarkdownView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::with_text("", cx)
    }

    pub fn with_text(text: &str, cx: &mut Context<Self>) -> Self {
        let blink = cx.new(|_| BlinkCursor::new());
        let _subscriptions = vec![cx.observe(&blink, |_, _, cx| cx.notify())];
        Self {
            buffer: LiveBuffer::new(text),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            hl: HighlightCache::new(),
            tab_size: 4,
            read_only: false,
            placeholder: SharedString::from(""),
            selecting: false,
            goal_x: None,
            planned_version: u64::MAX,
            planned_key: (Pos::default(), None, false, false),
            planned: Vec::new(),
            copied_code_blocks: HashSet::new(),
            scroll_to_cursor: false,
            row_targets: Vec::new(),
            row_layouts: std::collections::HashMap::new(),
            hl_style_key: 0,
            blink,
            cursor_shape: CursorShape::Bar,
            cursor_blink: true,
            blink_on: false,
            show_outline: false,
            _subscriptions,
        }
    }

    pub fn value(mut self, text: &str) -> Self {
        self.buffer = LiveBuffer::new(text);
        self
    }

    pub fn placeholder(mut self, text: impl Into<SharedString>) -> Self {
        self.placeholder = text.into();
        self
    }

    pub fn read_only(mut self, ro: bool) -> Self {
        self.read_only = ro;
        self
    }

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        self.pause_caret(cx);
        self.hl.clear();
        self.copied_code_blocks.clear();
        self.scroll_to_cursor = true;
        cx.emit(LiveViewEvent::Change(self.buffer.text()));
        cx.notify();
    }

    pub fn mark_saved(&mut self) {
        self.buffer.mark_saved();
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.is_dirty()
    }

    pub fn text_version(&self) -> u64 {
        self.buffer.text_version()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn set_cursor_shape(&mut self, shape: CursorShape, cx: &mut Context<Self>) {
        if self.cursor_shape != shape {
            self.cursor_shape = shape;
            self.blink.update(cx, |b, cx| b.show_cursor(cx));
            cx.notify();
        }
    }

    pub fn set_cursor_blink(&mut self, blink: bool, cx: &mut Context<Self>) {
        if self.cursor_blink != blink {
            self.cursor_blink = blink;
            self.blink.update(cx, |b, cx| b.set_blink_enabled(blink, cx));
            cx.notify();
        }
    }

    fn pause_caret(&mut self, cx: &mut Context<Self>) {
        if self.blink_on {
            self.blink.update(cx, |b, cx| b.pause_blinking(cx));
        }
    }

    fn show_caret(&self, window: &Window, cx: &App) -> bool {
        !self.read_only
            && self.focus.is_focused(window)
            && window.is_window_active()
            && self.blink.read(cx).visible()
    }

    fn caret_block_for(
        shape: CursorShape,
        ch: Option<char>,
        base: &gpui::TextStyle,
        font_size: Pixels,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> (Pixels, Option<ShapedLine>) {
        if !matches!(
            shape,
            CursorShape::Block | CursorShape::Hollow | CursorShape::Underline
        ) {
            return (CURSOR_WIDTH, None);
        }
        let fallback = (line_height * 0.6).max(CURSOR_WIDTH);
        let Some(ch) = ch else {
            return (fallback, None);
        };
        if ch == '\n' || ch == '\r' {
            return (fallback, None);
        }
        let editor_bg = cx.theme().editor_background();
        let text_color = if editor_bg.a < 0.75 {
            if cx.theme().appearance.is_dark() {
                gpui::black()
            } else {
                gpui::white()
            }
        } else {
            editor_bg
        };
        let s: SharedString = ch.to_string().into();
        let shaped = window.text_system().shape_line(
            s.clone(),
            font_size,
            &[TextRun {
                len: s.len(),
                font: base.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let width = if ch.is_ascii_whitespace() {
            fallback
        } else {
            fallback.max(shaped.width)
        };
        (width, Some(shaped))
    }

    /// Current vertical scroll offset (0 at top, negative when scrolled
    /// down). Diagnostics/test helper.
    pub fn scroll_offset_y(&self) -> f32 {
        f32::from(self.scroll.offset().y)
    }

    /// Record a row's layout from `LiveLine::paint` (element.rs): powers
    /// visual-row movement and the dead-zone hit fallbacks.
    pub(crate) fn record_row_layout(
        &mut self,
        line: usize,
        layout: gpui::TextLayout,
        text_len: usize,
    ) {
        self.row_layouts
            .insert(line, RowLayoutInfo { layout, text_len });
    }

    /// Mutate with change-event + rerender (the `edit` transaction).
    pub fn edit<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut crate::editor::markdown_live::model::EditorModel) -> R) -> R {
        let before = self.buffer.text();
        self.scroll_to_cursor = true;
        self.pause_caret(cx);
        let out = self.buffer.edit(f);
        if self.buffer.text() != before {
            cx.emit(LiveViewEvent::Change(self.buffer.text()));
        }
        cx.notify();
        out
    }

    fn planned_rows(&mut self, focused: bool) -> &[PlannedRow] {
        let key = (
            self.buffer.cursor(),
            self.buffer.selection(),
            focused,
            self.read_only,
        );
        if self.planned_version != self.buffer.text_version() || self.planned_key != key {
            let reveal = Reveal {
                cursor: self.buffer.cursor(),
                selection: self.buffer.selection(),
                focused,
                read_only: self.read_only,
            };
            let mut planned = plan_document(self.buffer.lines(), reveal);
            // Callout openers hide their `[!TAG] ` prefix when not revealed,
            // matching the preview which consumes the tag. Stripped here so
            // paint and hit-testing share one mapping.
            for row in &mut planned {
                let is_opener = row.callout.as_ref().is_some_and(|c| c.is_first);
                if !is_opener || row.plan.revealed {
                    continue;
                }
                let Some(callout) = row.callout.as_ref() else {
                    continue;
                };
                let tag_len = callout.kind.label().len() + 3; // `[!TAG]`
                let src_line = self.buffer.line(row.line).unwrap_or("");
                let content_off = match &row.block {
                    Block::Quote { content, .. } => *content,
                    _ => 0,
                };
                let after_tag = src_line
                    .get(content_off + tag_len..)
                    .unwrap_or("")
                    .strip_prefix(' ')
                    .map(|_| tag_len + 1)
                    .unwrap_or(tag_len);
                if after_tag <= row.plan.visible.len() {
                    let src_off = (content_off + after_tag).min(src_line.len());
                    row.plan = strip_plan_prefix(&row.plan, after_tag, src_off);
                }
            }
            self.planned = planned;
            self.planned_version = self.buffer.text_version();
            self.planned_key = key;
        }
        &self.planned
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        cx.emit(LiveViewEvent::Change(self.buffer.text()));
        cx.notify();
        cx.stop_propagation();
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        cx.stop_propagation();
    }

    /// True when the cursor's row is source-only (code / fence / table /
    /// frontmatter): rich inline toggles (Cmd+B/I/K) must insert literally
    /// there instead of wrapping. Reads the last frame's plan for document
    /// context (fences are only knowable with it).
    fn cursor_is_source_only(&self) -> bool {
        let line = self.buffer.cursor().line;
        self.planned
            .iter()
            .find(|r| r.line == line)
            .is_some_and(|r| crate::editor::markdown_live::interactions::is_source_only(&r.block))
    }

    // ---- dead-zone hit fallback (B4/B5) ---------------------------------

    /// The rendered row element whose band contains (or is nearest to)
    /// window-space `y`, as a source-line range. Uses the scroll handle's
    /// per-child bounds (document space) and the current offset.
    fn nearest_row_for_y(&self, y: Pixels) -> Option<(usize, usize)> {
        let n = self.row_targets.len();
        if n == 0 {
            return None;
        }
        let doc_y = f32::from(y) - f32::from(self.scroll.offset().y);
        let mut best: Option<(usize, f32)> = None;
        for ix in 0..n {
            let Some(b) = self.scroll.bounds_for_item(ix) else {
                continue;
            };
            let (top, bottom) = (f32::from(b.top()), f32::from(b.bottom()));
            if doc_y >= top && doc_y <= bottom {
                return Some(self.row_targets[ix]);
            }
            let dist = if doc_y < top {
                top - doc_y
            } else {
                doc_y - bottom
            };
            if best.is_none_or(|(_, d)| dist < d) {
                best = Some((ix, dist));
            }
        }
        best.map(|(ix, _)| self.row_targets[ix])
    }

    /// Resolve a click to a concrete source line: the row element range
    /// from [`Self::nearest_row_for_y`], then the line within the range
    /// whose recorded layout band contains the click.
    fn line_for_click(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let (a, b) = self.nearest_row_for_y(position.y)?;
        for line in a..=b {
            if let Some(info) = self.row_layouts.get(&line) {
                let lb = info.layout.bounds();
                if position.y >= lb.top() && position.y <= lb.bottom() {
                    return Some(line);
                }
            }
        }
        Some(a)
    }

    /// Column for a fallback click on `line`: through the recorded layout
    /// when one exists (x-precise like a real text click), else line end.
    fn col_for_click(&self, line: usize, position: gpui::Point<Pixels>) -> usize {
        let (Some(info), Some(row), Some(text)) = (
            self.row_layouts.get(&line),
            self.planned.iter().find(|r| r.line == line),
            self.buffer.line(line),
        ) else {
            return usize::MAX;
        };
        let b = info.layout.bounds();
        let y = position.y.clamp(b.top(), b.bottom() - px(1.));
        let x = position.x.max(b.left());
        let vis = match info.layout.index_for_position(gpui::point(x, y)) {
            Ok(ix) | Err(ix) => ix.min(info.text_len),
        };
        let src = src_for_vis(&row.plan.segs, vis);
        col_for_byte(text, src)
    }

    /// Root-level fallback for clicks that no `LiveLine`/row element
    /// claimed: margins between rows, card paddings, areas past EOL.
    /// Mirrors guise's continuous `hit()` — every click in the document
    /// area lands the cursor somewhere sensible.
    fn on_fallback_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only claim clicks inside the document scroll area; everything
        // else (app chrome, other panes) keeps bubbling.
        let sb = self.scroll.bounds();
        if ev.position.y < sb.top() || ev.position.y > sb.bottom() {
            return;
        }
        window.focus(&self.focus, cx);
        self.goal_x = None;
        let Some(line) = self.line_for_click(ev.position) else {
            return;
        };
        let col = self.col_for_click(line, ev.position);
        let shift = ev.modifiers.shift;
        let click_count = ev.click_count;
        self.edit(cx, |m| {
            m.move_to(line, col, shift);
            if !shift {
                if click_count == 2 {
                    m.select_word();
                } else if click_count > 2 {
                    m.select_line();
                }
            }
        });
        self.selecting = click_count <= 1;
        cx.stop_propagation();
        cx.notify();
    }

    /// Fallback for drags through zones no `LiveLine` covers (blank rows,
    /// margins): keep the selection extending to the nearest line.
    pub(crate) fn on_fallback_drag(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let Some(line) = self.line_for_click(position) else {
            return;
        };
        let col = self.col_for_click(line, position);
        self.edit(cx, |m| m.move_to(line, col, true));
    }

    // ---- visual-row movement (B6) ---------------------------------------

    /// Up/Down across *visual* rows (soft-wrapped lines inside a source
    /// line), with a sticky row-relative goal x — guise `move_visual`
    /// parity. Falls back to plain source-line movement when the last
    /// frame's layouts are stale or missing.
    fn move_visual(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        if self.planned_version != self.buffer.text_version() || self.row_layouts.is_empty() {
            self.edit(cx, |m| {
                if down {
                    m.move_down(extend)
                } else {
                    m.move_up(extend)
                }
            });
            self.after_move(cx);
            return;
        }
        let cursor = self.buffer.cursor();
        let fallback = |this: &mut Self, cx: &mut Context<Self>| {
            this.edit(cx, |m| {
                if down {
                    m.move_down(extend)
                } else {
                    m.move_up(extend)
                }
            });
            this.after_move(cx);
        };
        let Some(row) = self.planned.iter().find(|r| r.line == cursor.line) else {
            fallback(self, cx);
            return;
        };
        let Some(info) = self.row_layouts.get(&cursor.line) else {
            fallback(self, cx);
            return;
        };
        let text = self.buffer.line(cursor.line).unwrap_or("").to_string();
        let vis = vis_for_src(&row.plan.segs, byte_for_col(&text, cursor.col));
        let Some(pos) = info.layout.position_for_index(vis.min(info.text_len)) else {
            fallback(self, cx);
            return;
        };
        let origin = info.layout.bounds().origin;
        let line_h = f32::from(info.layout.line_height()).max(1.0);
        let text_h = f32::from(info.layout.bounds().size.height);
        // `position_for_index` has END semantics at wrap boundaries: the
        // caret before a wrapped row's first char reports the PREVIOUS row's
        // end. Detect that (the next byte sits lower) and adopt start
        // semantics — guise's `Row::caret()` does the same with explicit
        // wrap boundaries.
        let pos_y = f32::from(pos.y);
        let next_pos = info
            .layout
            .position_for_index((vis + 1).min(info.text_len));
        let (vrow_f, caret_x_rel) = match next_pos {
            Some(np) if vis < info.text_len && f32::from(np.y) > pos_y => {
                // `vis` starts a new visual row: its row is the next one and
                // its x is the row start.
                (
                    ((f32::from(np.y - origin.y)) / line_h).round(),
                    0.0,
                )
            }
            _ => (
                // Round, not floor: pixel snapping can put a row top at
                // 26.0 with line_height 26.25 and floor would miscount.
                ((f32::from(pos.y - origin.y)) / line_h).round(),
                f32::from(pos.x - origin.x),
            ),
        };
        let vrow = vrow_f as usize;
        let vrow_count = ((text_h / line_h).ceil() as usize).max(1);
        // Sticky goal x, relative to the row's text origin so it survives
        // scroll and inset changes between frames.
        let goal_rel = self.goal_x.unwrap_or(caret_x_rel);
        self.goal_x = Some(goal_rel);

        let target = if down {
            if vrow + 1 < vrow_count {
                Some((cursor.line, vrow + 1))
            } else if cursor.line + 1 < self.buffer.line_count() {
                Some((cursor.line + 1, 0))
            } else {
                None
            }
        } else if vrow > 0 {
            Some((cursor.line, vrow - 1))
        } else if cursor.line > 0 {
            let prev = cursor.line - 1;
            let prev_count = self
                .row_layouts
                .get(&prev)
                .map(|i| {
                    let h = f32::from(i.layout.bounds().size.height);
                    let lh = f32::from(i.layout.line_height()).max(1.0);
                    ((h / lh).ceil() as usize).max(1)
                })
                .unwrap_or(1);
            Some((prev, prev_count.saturating_sub(1)))
        } else {
            None
        };
        let Some((tline, tvrow)) = target else {
            self.edit(cx, |m| {
                if down {
                    m.doc_end(extend)
                } else {
                    m.doc_start(extend)
                }
            });
            self.after_move(cx);
            return;
        };
        let col = if let Some(tinfo) = self.row_layouts.get(&tline) {
            let torigin = tinfo.layout.bounds().origin;
            let tline_h = f32::from(tinfo.layout.line_height()).max(1.0);
            let ty = torigin.y + px((tvrow as f32 + 0.5) * tline_h);
            let tx = torigin.x + px(goal_rel);
            let vis = match tinfo.layout.index_for_position(gpui::point(tx, ty)) {
                Ok(ix) | Err(ix) => ix.min(tinfo.text_len),
            };
            let trow = self.planned.iter().find(|r| r.line == tline);
            let ttext = self.buffer.line(tline).unwrap_or("");
            let src = trow.map_or(vis, |r| src_for_vis(&r.plan.segs, vis));
            col_for_byte(ttext, src)
        } else {
            usize::MAX // blank rows: line end
        };
        self.edit(cx, |m| m.move_to(tline, col, extend));
        self.after_move(cx);
    }

    // ---- mouse ----------------------------------------------------------

    /// Hit raw visible byte `raw` on `line`: link activation first, then
    /// cursor placement / word / line selection.
    #[allow(clippy::too_many_arguments)]
    pub fn on_line_mouse_down(
        &mut self,
        line: usize,
        raw: usize,
        click_count: usize,
        shift: bool,
        platform: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.goal_x = None;
        let Some(src_line) = self.buffer.line(line).map(str::to_string) else {
            return;
        };
        // Snapshot the plan mapping before the mutable edit borrow.
        let (vis_len, segs, link) = match self.planned.iter().find(|r| r.line == line) {
            Some(row) => {
                let vis = raw.min(row.plan.visible.len());
                let src = src_for_vis(&row.plan.segs, vis);
                (row.plan.visible.len(), row.plan.segs.clone(), row.plan.link_at(src).map(str::to_string))
            }
            None => return,
        };
        let _ = vis_len;
        let vis = raw.min(vis_len);
        let src = src_for_vis(&segs, vis);
        if (platform || self.read_only) && let Some(target) = link {
            cx.emit(LiveViewEvent::LinkClick(target));
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let col = col_for_byte(&src_line, src);
        self.edit(cx, |m| {
            // `move_to` clamps, and `extend` anchors a selection even when
            // none exists yet (shift-click starts a selection like guise).
            m.move_to(line, col, shift);
            if !shift {
                if click_count == 2 {
                    m.select_word();
                } else if click_count > 2 {
                    m.select_line();
                }
            }
        });
        self.selecting = click_count <= 1;
        // Claim the event so the root dead-zone fallback doesn't also run.
        cx.stop_propagation();
    }

    pub fn on_line_drag(&mut self, line: usize, raw: usize, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let (vis_len, segs, src_line) = match self.planned.iter().find(|r| r.line == line) {
            Some(row) => (
                row.plan.visible.len(),
                row.plan.segs.clone(),
                self.buffer.line(line).map(str::to_string).unwrap_or_default(),
            ),
            None => return,
        };
        let vis = raw.min(vis_len);
        let col = col_for_byte(&src_line, src_for_vis(&segs, vis));
        self.edit(cx, |m| m.move_to(line, col, true));
        // Claim the drag so the dead-zone fallback doesn't also run.
        cx.stop_propagation();
    }

    pub fn on_line_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            cx.notify();
        }
    }

    pub fn toggle_task_at(&mut self, line: usize, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(text) = self.buffer.line(line).map(str::to_string) else {
            return;
        };
        let changed = self.edit(cx, |m| toggle_task(m, &text, line));
        if changed {
            self.after_edit(cx);
        }
    }

    // ---- keyboard --------------------------------------------------------

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let key = ks.key.as_str();
        if key != "up" && key != "down" {
            self.goal_x = None;
        }
        let shift = ks.modifiers.shift;
        let platform = ks.modifiers.platform;
        let control = ks.modifiers.control;
        let alt = ks.modifiers.alt;

        match key {
            "left" => {
                self.edit(cx, |m| {
                    if platform {
                        m.home(shift)
                    } else if alt {
                        m.word_left(shift)
                    } else {
                        m.move_left(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "right" => {
                self.edit(cx, |m| {
                    if platform {
                        m.end(shift)
                    } else if alt {
                        m.word_right(shift)
                    } else {
                        m.move_right(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "up" => {
                if platform {
                    self.edit(cx, |m| m.doc_start(shift));
                    self.after_move(cx);
                } else {
                    self.move_visual(false, shift, cx);
                }
                return;
            }
            "down" => {
                if platform {
                    self.edit(cx, |m| m.doc_end(shift));
                    self.after_move(cx);
                } else {
                    self.move_visual(true, shift, cx);
                }
                return;
            }
            "home" => {
                self.edit(cx, |m| {
                    if platform {
                        m.doc_start(shift)
                    } else {
                        m.home(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "end" => {
                self.edit(cx, |m| {
                    if platform {
                        m.doc_end(shift)
                    } else {
                        m.end(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "backspace" => {
                if self.read_only {
                    return;
                }
                // Marker strip first (plain backspace, no selection).
                let cur = self.buffer.cursor();
                let sel_none = self.buffer.selection().is_none();
                if sel_none && !platform && !alt {
                    if let Some(text) = self.buffer.line(cur.line).map(str::to_string) {
                        let stripped = self.edit(cx, |m| backspace_marker(m, &text));
                        if stripped {
                            self.after_edit(cx);
                            return;
                        }
                    }
                }
                let changed = self.edit(cx, |m| {
                    if m.selection().is_some() {
                        m.delete_selection()
                    } else if platform {
                        m.home(true);
                        m.delete_selection()
                    } else if alt {
                        m.word_left(true);
                        m.delete_selection()
                    } else {
                        m.backspace()
                    }
                });
                if changed {
                    self.after_edit(cx);
                } else {
                    cx.stop_propagation();
                }
                return;
            }
            "delete" => {
                if self.read_only {
                    return;
                }
                let changed = self.edit(cx, |m| {
                    if m.selection().is_some() {
                        m.delete_selection()
                    } else if platform {
                        m.end(true);
                        m.delete_selection()
                    } else if alt {
                        m.word_right(true);
                        m.delete_selection()
                    } else {
                        m.delete()
                    }
                });
                if changed {
                    self.after_edit(cx);
                } else {
                    cx.stop_propagation();
                }
                return;
            }
            "enter" => {
                if platform {
                    if self.read_only {
                        return;
                    }
                    let cur = self.buffer.cursor();
                    if let Some(text) = self.buffer.line(cur.line).map(str::to_string) {
                        let toggled = self.edit(cx, |m| toggle_task(m, &text, cur.line));
                        if toggled {
                            self.after_edit(cx);
                        }
                    }
                    return;
                }
                if self.read_only {
                    return;
                }
                let cur = self.buffer.cursor();
                let text = self.buffer.line(cur.line).map(str::to_string).unwrap_or_default();
                let planned_block = self
                    .planned
                    .iter()
                    .find(|r| r.line == cur.line)
                    .map(|r| r.block.clone());
                // Typora Enter inside a table: empty row below (no selection).
                if self.buffer.selection().is_none()
                    && matches!(planned_block, Some(Block::Table))
                    && self.edit(cx, table_insert_row_below)
                {
                    self.after_edit(cx);
                    return;
                }
                let in_code = matches!(
                    planned_block,
                    Some(Block::CodeLine | Block::Fence { .. } | Block::FrontMatter)
                );
                self.edit(cx, |m| {
                    on_enter(m, &text, in_code);
                });
                self.after_edit(cx);
                return;
            }
            "tab" => {
                if platform || self.read_only {
                    return;
                }
                let cur = self.buffer.cursor();
                let text = self.buffer.line(cur.line).map(str::to_string).unwrap_or_default();
                let tab_size = self.tab_size;
                // Typora Tab inside a table: grid navigation (no selection).
                if self.buffer.selection().is_none()
                    && self
                        .planned
                        .iter()
                        .find(|r| r.line == cur.line)
                        .is_some_and(|r| matches!(r.block, Block::Table))
                {
                    let grid: HashSet<usize> = self
                        .planned
                        .iter()
                        .filter(|r| matches!(r.block, Block::Table))
                        .map(|r| r.line)
                        .collect();
                    let forward = !shift;
                    self.edit(cx, |m| {
                        table_nav(m, forward, &|l| grid.contains(&l));
                    });
                    self.after_edit(cx);
                    return;
                }
                // Code/fence/frontmatter keep literal tabs.
                if !matches!(
                    classify_alone(&text),
                    Block::CodeLine | Block::Fence { .. } | Block::FrontMatter
                ) {
                    self.edit(cx, |m| on_tab(m, &text, shift, tab_size));
                    self.after_edit(cx);
                } else if !shift {
                    self.edit(cx, |m| {
                        m.tab();
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "escape" => {
                if self.buffer.selection().is_some() {
                    self.edit(cx, |m| m.clear_selection());
                    cx.notify();
                }
                return;
            }
            "a" if platform => {
                self.edit(cx, |m| m.select_all());
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "b" if platform => {
                if !self.read_only {
                    let literal = self.cursor_is_source_only();
                    self.edit(cx, |m| {
                        if literal {
                            m.insert("**");
                        } else {
                            toggle_wrap(m, "**");
                        }
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "i" if platform => {
                if !self.read_only {
                    let literal = self.cursor_is_source_only();
                    self.edit(cx, |m| {
                        if literal {
                            m.insert("*");
                        } else {
                            toggle_wrap(m, "*");
                        }
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "k" if platform => {
                if !self.read_only {
                    let literal = self.cursor_is_source_only();
                    self.edit(cx, |m| {
                        if literal {
                            m.insert("[]()");
                        } else {
                            insert_link(m);
                        }
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "c" if platform => {
                if let Some(t) = self.selected_source_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(t));
                }
                cx.stop_propagation();
                return;
            }
            "x" if platform => {
                if self.read_only {
                    if let Some(t) = self.selected_source_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                    }
                    cx.stop_propagation();
                    return;
                }
                let cut = self.edit(cx, |m| m.cut());
                match cut {
                    Some(t) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                        self.after_edit(cx);
                    }
                    None => cx.stop_propagation(),
                }
                return;
            }
            "v" if platform => {
                if self.read_only {
                    cx.stop_propagation();
                    return;
                }
                if let Some(item) = cx.read_from_clipboard()
                    && let Some(text) = item.text()
                {
                    let text = text.replace('\r', "");
                    if !text.is_empty() {
                        self.edit(cx, |m| m.insert(&text));
                        self.after_edit(cx);
                        return;
                    }
                }
                cx.stop_propagation();
                return;
            }
            "z" if platform => {
                if self.read_only {
                    cx.stop_propagation();
                    return;
                }
                let changed = self.edit(cx, |m| {
                    if shift {
                        m.redo()
                    } else {
                        m.undo()
                    }
                });
                if changed {
                    self.after_edit(cx);
                } else {
                    cx.stop_propagation();
                }
                return;
            }
            "o" if platform && shift => {
                // Typora 大纲侧栏显隐（与状态条按钮同一开关）。
                self.show_outline = !self.show_outline;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            _ => {}
        }
        // Printable input.
        if !self.read_only
            && !platform
            && !control
            && let Some(text) = ks.key_char.as_deref().filter(|t| !t.is_empty())
        {
            self.edit(cx, |m| {
                m.insert(text);
            });
            self.after_edit(cx);
        }
    }

    /// Selected text as *source* markdown (markers included), for copy.
    fn selected_source_text(&self) -> Option<String> {
        let (s, e) = self.buffer.selection()?;
        let mut out = String::new();
        for line in s.line..=e.line {
            let text = self.buffer.line(line)?;
            let chars: Vec<char> = text.chars().collect();
            let (a, b) = if line == s.line && line == e.line {
                (s.col.min(chars.len()), e.col.min(chars.len()))
            } else if line == s.line {
                (s.col.min(chars.len()), chars.len())
            } else if line == e.line {
                (0, e.col.min(chars.len()))
            } else {
                (0, chars.len())
            };
            if line > s.line {
                out.push('\n');
            }
            out.extend(chars[a..b].iter());
        }
        Some(out)
    }

    /// Copy-button feedback for fenced code cards (mirrors `Markdown`).
    pub fn mark_code_block_copied(&mut self, id: usize, cx: &mut Context<Self>) {
        self.copied_code_blocks.insert(id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update(cx, |this, cx| {
                this.copied_code_blocks.remove(&id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Fingerprint of the style inputs baked into highlighted runs: a change
/// (theme/appearance switch, font size change) must invalidate the
/// `HighlightCache`, which otherwise returns stale-colored runs.
fn style_fingerprint(style: &MarkdownStyle, is_dark: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    is_dark.hash(&mut h);
    let b = &style.base_text_style;
    b.color.h.to_bits().hash(&mut h);
    b.color.s.to_bits().hash(&mut h);
    b.color.l.to_bits().hash(&mut h);
    b.color.a.to_bits().hash(&mut h);
    b.font_family.hash(&mut h);
    b.font_weight.0.to_bits().hash(&mut h);
    b.font_style.hash(&mut h);
    f32::from(b.font_size.to_pixels(px(16.))).to_bits().hash(&mut h);
    // The code-block mono style feeds `row_base_style` for code runs.
    style.inline_code.font_family.hash(&mut h);
    if let Some(fs) = &style.inline_code.font_size {
        f32::from(fs.to_pixels(px(16.))).to_bits().hash(&mut h);
    }
    h.finish()
}

/// Callout accent per kind (same mapping as the read-only preview).
fn callout_accent(kind: CalloutKind, theme: &crate::theme::AppTheme) -> Hsla {
    let ui = &theme.ui;
    match kind {
        CalloutKind::Note => ui.accent,
        CalloutKind::Tip => ui.success,
        CalloutKind::Important => gpui::rgb(0x8250DF).into(),
        CalloutKind::Warning => ui.warning,
        CalloutKind::Caution => ui.error,
    }
}

/// Outline entries `(source line, level, title)` in document order.
fn outline_entries(rows: &[PlannedRow]) -> Vec<(usize, u8, String)> {
    rows
        .iter()
        .filter_map(|r| match &r.block {
            Block::Heading { level, .. } => Some((r.line, *level, outline_title(&r.plan.visible))),
            _ => None,
        })
        .collect()
}

/// Outline display title: single line, max 42 chars.
fn outline_title(visible: &str) -> String {
    let one = visible
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one.is_empty() {
        return "(空标题)".to_string();
    }
    if one.chars().count() > 42 {
        format!("{}…", one.chars().take(42).collect::<String>())
    } else {
        one
    }
}

/// If the whole line is a single `![alt](url)` image, return `(alt, url)`.
fn parse_image_line(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    let inner = t.strip_prefix("![")?;
    let close = inner.find("](")?;
    let alt = inner[..close].to_string();
    let rest = &inner[close + 2..];
    let end = rest.find(')')?;
    let url = rest[..end].split_whitespace().next().unwrap_or("").to_string();
    if url.is_empty() || !rest[end + 1..].trim().is_empty() {
        return None;
    }
    Some((alt, url))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellAlign {
    Left,
    Center,
    Right,
    None,
}

fn align_of(cell: &str) -> CellAlign {
    let c = cell.trim();
    let l = c.starts_with(':');
    let r = c.ends_with(':');
    match (l, r) {
        (true, true) => CellAlign::Center,
        (true, false) => CellAlign::Left,
        (false, true) => CellAlign::Right,
        _ => CellAlign::None,
    }
}

impl Focusable for LiveMarkdownView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Per-row painted data, computed once per frame before grouping.
struct RowPaint {
    runs: Vec<TextRun>,
    caret_vis: Option<usize>,
    /// Caret height matching this row's real line height (C4: was
    /// hardcoded per kind and drifted from the rendered metrics).
    caret_h: Pixels,
    caret_block_width: Pixels,
    caret_block_text: Option<ShapedLine>,
}

/// A source line's last-frame text layout, recorded by `LiveLine::paint`.
#[derive(Clone)]
pub(crate) struct RowLayoutInfo {
    pub layout: gpui::TextLayout,
    pub text_len: usize,
}

/// A render group: consecutive rows sharing one chrome container.
enum Group {
    One(usize),
    Fence {
        open: usize,
        code: Vec<usize>,
        close: Option<usize>,
        lang: Option<String>,
    },
    /// Consecutive plain-paragraph rows rendered as ONE block: the preview
    /// joins them into a single paragraph (soft-wrap + hard breaks), so
    /// the live editor must not put inter-row margins between them (C2).
    Para(Vec<usize>),
    Quote(Vec<usize>),
    Table(Vec<usize>),
}

impl Render for LiveMarkdownView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = theme.ui;
        let is_dark = theme.appearance.is_dark();
        let style = MarkdownStyle::themed(window, cx);
        let colors = LiveColors::resolve(&style, &theme);
        let focused = self.focus.is_focused(window);
        let cursor = self.buffer.cursor();
        let selection = self.buffer.selection().map(|(mut s, mut e)| {
            if e < s {
                std::mem::swap(&mut s, &mut e);
            }
            (s, e)
        });

        // Highlight colors are baked into cached runs; a theme/base-style
        // change (appearance switch, font size change) must invalidate them.
        let hl_key = style_fingerprint(&style, is_dark);
        if self.hl_style_key != hl_key {
            self.hl_style_key = hl_key;
            self.hl.clear();
        }

        // Blink management (Zed BlinkManager parity): enable on focus+active,
        // disable otherwise. `enable` is idempotent; `disable` is guarded by
        // `blink_on` so unfocused frames don't notify-loop.
        let window_active = window.is_window_active();
        let want_blink = focused && window_active && !self.read_only;
        if want_blink && !self.blink_on {
            self.blink.update(cx, |b, cx| b.enable(cx));
            self.blink_on = true;
        } else if !want_blink && self.blink_on {
            self.blink.update(cx, |b, cx| b.disable(cx));
            self.blink_on = false;
        }
        let show_caret = self.show_caret(window, cx);

        // Fresh per-frame hit geometry (LiveLine::paint repopulates).
        self.row_layouts.clear();
        self.row_targets.clear();

        // Plan rows (cached on version+cursor+selection+focus).
        let rows: Vec<PlannedRow> = self.planned_rows(focused).to_vec();
        let last_line = rows.len().saturating_sub(1);
        let entity = cx.entity();

        // ---- per-row paint (runs + selection + caret) --------------------
        // (`row.plan` already carries callout tag-stripping from
        // `planned_rows`, shared with hit-testing.)
        //
        // Code rows highlight as whole blocks (one tree-sitter parse per
        // fence, split per line) so multi-line constructs keep their spans;
        // pre-compute the per-line runs here, keyed by source line.
        let mut code_runs: std::collections::HashMap<usize, Vec<TextRun>> =
            std::collections::HashMap::new();
        {
            let mut i = 0;
            while i < rows.len() {
                let lang = match &rows[i].block {
                    Block::Fence { open: true, lang: Some(l) } => l.clone(),
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                let start = i;
                i += 1;
                while i < rows.len() && matches!(rows[i].block, Block::CodeLine) {
                    i += 1;
                }
                let code = start + 1..i;
                if i < rows.len() && matches!(rows[i].block, Block::Fence { open: false, .. }) {
                    i += 1;
                }
                if code.is_empty() {
                    continue;
                }
                let base = row_base_style(&rows[code.start].plan, &style);
                let src_lines: Vec<&str> = code
                    .clone()
                    .map(|pi| self.buffer.line(rows[pi].line).unwrap_or(""))
                    .collect();
                let per_line = self.hl.runs_for_block(&lang, &src_lines, &base, &theme);
                for (pi, runs) in code.zip(per_line) {
                    code_runs.insert(rows[pi].line, runs);
                }
            }
        }
        let mut paints: Vec<RowPaint> = Vec::with_capacity(rows.len());
        for row in &rows {
            let plan = &row.plan;
            let base = row_base_style(plan, &style);
            let mut runs = if let Some(block_runs) = code_runs.get(&row.line) {
                block_runs.clone()
            } else {
                runs_for_row(plan, &base, &colors, &style, &self.hl, &theme)
            };
            if matches!(row.plan.kind, RowKind::Task { checked: true, .. }) {
                runs = apply_task_done(
                    runs,
                    ui.text_muted,
                    ui.text_muted.opacity(0.6),
                );
            }
            // Selection overlay: source cols → bytes → visible bytes.
            let vsel = selection.and_then(|(s, e)| {
                let src_line = self.buffer.line(row.line)?;
                let line_len = src_line.chars().count();
                let (cs, ce, _) = line_selection(s, e, row.line, line_len)?;
                let bs = byte_for_col(src_line, cs);
                let be = byte_for_col(src_line, ce);
                Some((
                    vis_for_src(&plan.segs, bs),
                    vis_for_src(&plan.segs, be),
                ))
            });
            runs = apply_selection(runs, plan.visible.len(), vsel, colors.selection);
            let caret_vis = if show_caret && cursor.line == row.line {
                let src_line = self.buffer.line(row.line).unwrap_or("");
                Some(vis_for_src(
                    &plan.segs,
                    byte_for_col(src_line, cursor.col),
                ))
            } else {
                None
            };
            // Caret height = this row's real rendered line height (base
            // style × line-height multiplier), not a hardcoded per-kind
            // value (C4).
            let font_size = base.font_size.to_pixels(window.rem_size());
            let caret_h = window.pixel_snap(
                base.line_height
                    .to_pixels(font_size.into(), window.rem_size()),
            );
            let (caret_block_width, caret_block_text) = if caret_vis.is_some() {
                let src_line = self.buffer.line(row.line).unwrap_or("");
                let ch = src_line.chars().nth(cursor.col);
                Self::caret_block_for(
                    self.cursor_shape,
                    ch,
                    &base,
                    font_size,
                    caret_h,
                    window,
                    cx,
                )
            } else {
                (CURSOR_WIDTH, None)
            };
            // Stash paint for the grouping pass below.
            paints.push(RowPaint {
                runs,
                caret_vis,
                caret_h,
                caret_block_width,
                caret_block_text,
            });
        }
        // ---- grouping ------------------------------------------------------
        let mut groups: Vec<Group> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            match &rows[i].block {
                Block::Fence { open: true, lang } => {
                    let lang = lang.clone();
                    let open = i;
                    i += 1;
                    let mut code = Vec::new();
                    while i < rows.len() && matches!(rows[i].block, Block::CodeLine) {
                        code.push(i);
                        i += 1;
                    }
                    let close = if i < rows.len()
                        && matches!(rows[i].block, Block::Fence { open: false, .. })
                    {
                        let c = Some(i);
                        i += 1;
                        c
                    } else {
                        None
                    };
                    groups.push(Group::Fence {
                        open,
                        code,
                        close,
                        lang,
                    });
                }
                Block::Quote { depth, .. } => {
                    let depth = *depth;
                    let mut v = vec![i];
                    i += 1;
                    while i < rows.len()
                        && matches!(rows[i].block, Block::Quote { depth: d, .. } if d == depth)
                    {
                        v.push(i);
                        i += 1;
                    }
                    groups.push(Group::Quote(v));
                }
                Block::Paragraph => {
                    let mut v = vec![i];
                    i += 1;
                    while i < rows.len() && matches!(rows[i].block, Block::Paragraph) {
                        v.push(i);
                        i += 1;
                    }
                    groups.push(Group::Para(v));
                }
                Block::Table => {
                    let mut v = vec![i];
                    i += 1;
                    while i < rows.len() && matches!(rows[i].block, Block::Table) {
                        v.push(i);
                        i += 1;
                    }
                    groups.push(Group::Table(v));
                }
                _ => {
                    groups.push(Group::One(i));
                    i += 1;
                }
            }
        }

        // ---- render helpers (per-row LiveLine) ------------------------------
        // Ordered display numbers per nesting depth (preview parity: see
        // `OrderedCounter`). Computed over ALL rows in document order so
        // grouped paragraphs still reset the counters between lists.
        let mut ordered = OrderedCounter::new();
        let display_numbers: Vec<Option<u64>> = rows
            .iter()
            .map(|row| ordered.visit(&row.plan.kind, list_cols(&row.plan.kind)))
            .collect();
        // row_els index of the element containing the caret's source line,
        // for `scroll_to_item` (the rows column registers every direct
        // child's bounds with the scroll handle).
        let mut caret_el_ix: Option<usize> = None;
        let mut row_els: Vec<gpui::AnyElement> = Vec::new();
        // The first row element's LiveLine also carries the dead-zone drag
        // fallback (exactly one handler per frame); the closure consumes
        // the flag so call sites stay unchanged.
        let mut first_row_el = true;
        // `render_line` needs lots of context: build rows inline via closure.
        let cursor_shape = self.cursor_shape;
        let mut live_line = |pi: usize,
                             paints: &[RowPaint],
                             entity: &Entity<Self>,
                             colors: &LiveColors|
         -> LiveLine {
            let drag_fallback = first_row_el;
            first_row_el = false;
            let paint = &paints[pi];
            let plan = &rows[pi].plan;
            LiveLine::new(
                ("live-line", rows[pi].line),
                SharedString::from(plan.visible.clone()),
                paint.runs.clone(),
                rows[pi].line,
                entity.clone(),
                matches!(rows[pi].plan.kind, RowKind::Task { .. }),
                paint.caret_vis,
                colors.caret,
                CURSOR_WIDTH,
                paint.caret_h,
            )
            .with_drag_fallback(drag_fallback)
            .with_caret_shape(cursor_shape)
            .with_caret_block(paint.caret_block_width, paint.caret_block_text.clone())
        };

        for group in &groups {
            match group {
                Group::One(pi) => {
                    let pi = *pi;
                    let row = &rows[pi];
                    let is_first = row.line == 0;
                    let is_last = row.line == last_line;
                    // Ordered display numbers sequence like the preview
                    // (precomputed over all rows; see display_numbers).
                    let display_number = display_numbers[pi];
                    let el: gpui::AnyElement = match &row.plan.kind {
                        RowKind::Blank => {
                            // Blanks have no text hitbox: explicit click moves
                            // the cursor here. The cursor line also paints the
                            // caret inside an (empty) LiveLine.
                            let line = row.line;
                            let blank = div()
                                .id(("live-blank", line))
                                .debug_selector(|| "live-blank-row".into())
                                .w_full()
                                .h(px(26.))
                                // The scroll column is a fixed-height flex
                                // container: without shrink-0 a blank row's
                                // fixed 26px collapses to 0 on any doc taller
                                // than the viewport (unclickable + caret
                                // overlap). Every row element needs this.
                                .flex_shrink_0()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _e, window, cx| {
                                        window.focus(&this.focus, cx);
                                        // Col clamps to the line end, like
                                        // guise's hit() (whitespace-only
                                        // lines land at their end, not col 0).
                                        this.edit(cx, |m| m.move_to(line, usize::MAX, false));
                                        cx.notify();
                                        cx.stop_propagation();
                                    }),
                                );
                            if paints[pi].caret_vis.is_some() {
                                blank
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                blank.into_any_element()
                            }
                        }
                        RowKind::Rule => {
                            if row.plan.revealed {
                                div()
                                    .flex_shrink_0()
                                    .py(px(2.))
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                // Preview parity: 1px soft rule, my_6.
                                // The rule itself has no text: clicks land the
                                // cursor on this source line (revealing it).
                                let line = row.line;
                                let mut d = div()
                                    .id(("live-rule", line))
                                    .flex_shrink_0()
                                    .h(px(1.))
                                    .w_full()
                                    .my_6()
                                    .bg(colors.rule)
                                    .rounded_full()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, window, cx| {
                                            window.focus(&this.focus, cx);
                                            this.edit(cx, |m| {
                                                m.move_to(line, usize::MAX, false)
                                            });
                                            cx.notify();
                                            cx.stop_propagation();
                                        }),
                                    );
                                if is_first {
                                    d = d.mt_0();
                                }
                                if is_last {
                                    d = d.mb_0();
                                }
                                d.into_any_element()
                            }
                        }
                        RowKind::Heading(level) => {
                            let level = *level;
                            let mut head = match level {
                                1 => div().mt_8().mb_4().pb_2(),
                                2 => div().mt_7().mb_3().pb_2(),
                                3 => div().mt_6().mb_3(),
                                4 => div().mt_6().mb_2(),
                                _ => div().mt_4().mb_2(),
                            };
                            if is_first {
                                head = head.mt_0();
                            }
                            if is_last {
                                head = head.mb_0();
                            }
                            head = head
                                .w_full()
                                .flex_shrink_0()
                                .flex()
                                .flex_wrap()
                                .items_start()
                                .group("heading");
                            head.style().refine(&style.heading);
                            if let Some(r) = style
                                .heading_level_styles
                                .get(level as usize - 1)
                                .cloned()
                                .flatten()
                            {
                                head.style().refine(&gpui::StyleRefinement {
                                    text: r,
                                    ..Default::default()
                                });
                            }
                            if level <= 2
                                && let Some(color) = style.heading_border_color
                            {
                                head = head.border_b_1().border_color(color);
                            }
                            let anchor = div()
                                .ml_2()
                                .flex_shrink_0()
                                .text_size(px(13.))
                                .text_color(ui.text_muted.opacity(0.35))
                                .invisible()
                                .group_hover("heading", |s| s.visible())
                                .child("#")
                                .into_any_element();
                            head.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(live_line(pi, &paints, &entity, &colors)),
                            )
                            .child(anchor)
                            .into_any_element()
                        }
                        RowKind::Bullet { cols }
                        | RowKind::Ordered { cols, .. }
                        | RowKind::Task { cols, .. } => {
                            // Preview parity: nesting indent 24px per level,
                            // item row flex/gap/mb/lh, preview-exact gutters.
                            let depth = cols / 2;
                            let indent = px(depth as f32 * 24.0);
                            let gutter: gpui::AnyElement = match &row.plan.kind {
                                RowKind::Task { checked, .. } => {
                                    let checked = *checked;
                                    let line = row.line;
                                    let box_el = if checked {
                                        div()
                                            .size(px(16.))
                                            .rounded(px(4.))
                                            .bg(colors.success)
                                            .border_1()
                                            .border_color(colors.success)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                svg()
                                                    .size(px(10.))
                                                    .text_color(gpui::white())
                                                    .path(Icon::Check.path()),
                                            )
                                            .into_any_element()
                                    } else {
                                        div()
                                            .size(px(16.))
                                            .rounded(px(4.))
                                            .border_1()
                                            .border_color(ui.text_muted.opacity(0.35))
                                            .bg(gpui::transparent_black())
                                            .into_any_element()
                                    };
                                    div()
                                        .id(("live-task", line))
                                        .w(px(22.))
                                        .flex_shrink_0()
                                        .mt(px(3.))
                                        .child(
                                            div()
                                                .child(box_el)
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(
                                                        move |this, _e, _w, cx| {
                                                            this.toggle_task_at(line, cx);
                                                            cx.stop_propagation();
                                                        },
                                                    ),
                                                ),
                                        )
                                        .into_any_element()
                                }
                                RowKind::Ordered { number, .. } => {
                                    let n = display_number.unwrap_or(*number);
                                    div()
                                        .w(px(24.))
                                        .flex()
                                        .justify_end()
                                        .flex_shrink_0()
                                        .text_size(px(14.))
                                        .font_weight(gpui::FontWeight(500.))
                                        .pr(px(4.))
                                        .child(format!("{n}."))
                                        .text_color(ui.text_muted)
                                        .into_any_element()
                                }
                                _ => div()
                                    .w(px(16.))
                                    .flex()
                                    .justify_center()
                                    .flex_shrink_0()
                                    .text_size(px(22.))
                                    .child("•")
                                    .text_color(ui.accent)
                                    .into_any_element(),
                            };
                            let mut item = div()
                                .flex()
                                .flex_row()
                                .flex_shrink_0()
                                .items_start()
                                .gap_1()
                                .w_full()
                                .mb_2()
                                .line_height(rems(1.75))
                                .pl(indent);
                            if is_first {
                                item = item.mt_2();
                            }
                            item.child(gutter)
                                .child(
                                    div().flex_1().w_0().min_w_0().child(live_line(pi, &paints, &entity, &colors)),
                                )
                                .into_any_element()
                        }
                        RowKind::Quote { .. } => {
                            // Single quote lines are grouped above; unreachable.
                            div()
                                .flex_shrink_0()
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                        RowKind::Setext => {
                            // A setext underline (`===`/`---` under a heading
                            // line): the preview consumes it into the heading,
                            // so in rendered mode it is a small gap whose
                            // click targets the line; revealed, it shows the
                            // raw marker for editing.
                            if row.plan.revealed {
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                let line = row.line;
                                div()
                                    .id(("live-setext", line))
                                    .flex_shrink_0()
                                    .w_full()
                                    .h(px(8.))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, window, cx| {
                                            window.focus(&this.focus, cx);
                                            this.edit(cx, |m| {
                                                m.move_to(line, usize::MAX, false)
                                            });
                                            cx.notify();
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .into_any_element()
                            }
                        }
                        RowKind::Math => {
                            // Display formula: centered italic line
                            // (Typora-style; KaTeX approx, no TeX engine).
                            div()
                                .w_full()
                                .flex_shrink_0()
                                .flex()
                                .justify_center()
                                .my_2()
                                .line_height(rems(1.75))
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                        RowKind::MathDelim { .. } => {
                            // A hidden `$$` delimiter: revealed it shows the
                            // raw marker; otherwise a small click gap
                            // (Setext parity) that targets the line.
                            if row.plan.revealed {
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                let line = row.line;
                                div()
                                    .id(("live-math", line))
                                    .flex_shrink_0()
                                    .w_full()
                                    .h(px(8.))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, window, cx| {
                                            window.focus(&this.focus, cx);
                                            this.edit(cx, |m| {
                                                m.move_to(line, usize::MAX, false)
                                            });
                                            cx.notify();
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .into_any_element()
                            }
                        }
                        RowKind::Code { .. }
                        | RowKind::Fence { .. }
                        | RowKind::Table
                        | RowKind::FrontMatter => {
                            // Ungrouped source-only rows (revealed blocks,
                            // lone fences, frontmatter): plain mono lines.
                            div()
                                .flex_shrink_0()
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                        RowKind::Paragraph => {
                            // Paragraphs are grouped above; unreachable
                            // (kept as a defensive plain row).
                            div()
                                .flex_shrink_0()
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                    };
                    if row.line == cursor.line {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((row.line, row.line));
                    row_els.push(el);
                }
                Group::Fence {
                    open,
                    code,
                    close,
                    lang,
                } => {
                    // Only the fence LINES flip the card to source mode (the
                    // cursor sitting on ```). Code lines stay rendered in
                    // the card while editing them — Obsidian behavior — so
                    // clicking into code never un-renders it.
                    let revealed = rows[*open].plan.revealed
                        || close.is_some_and(|c| rows[c].plan.revealed);
                    let block_id = rows[*open].line;
                    if revealed || code.is_empty() {
                        // Source editing mode: mono lines, fences visible.
                        // One horizontal scroll container per block so long
                        // Nowrap lines scroll instead of being clipped
                        // (preview parity: its code card scrolls too).
                        let mut idxs = vec![*open];
                        idxs.extend(code.iter().cloned());
                        idxs.extend(close.iter().cloned());
                        let mut src = div()
                            .id(("live-code-src", block_id))
                            .flex_shrink_0()
                            .overflow_x_scroll();
                        for &pi in &idxs {
                            if rows[pi].line == cursor.line {
                                caret_el_ix = Some(row_els.len());
                            }
                            src = src.child(live_line(pi, &paints, &entity, &colors));
                        }
                        let last_idx = *idxs.last().unwrap();
                        self.row_targets.push((rows[*open].line, rows[last_idx].line));
                        row_els.push(src.into_any_element());
                        continue;
                    }
                    // Preview-parity code card (fences hidden, like preview).
                    let is_first = rows[*open].line == 0;
                    let is_last = close.is_some_and(|c| rows[c].line == last_line);
                    let code_text = code
                        .iter()
                        .map(|c| self.buffer.line(rows[*c].line).unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let mut outer = div()
                        .group("live-code")
                        .relative()
                        .w_full()
                        .flex_shrink_0()
                        .rounded(px(8.0))
                        .overflow_hidden();
                    outer.style().refine(&style.code_block);
                    if is_first {
                        outer.style().refine(&gpui::StyleRefinement {
                            margin: gpui::EdgesRefinement {
                                top: Some(gpui::Length::Definite(px(0.).into())),
                                ..Default::default()
                            },
                            ..Default::default()
                        });
                    }
                    if is_last {
                        outer.style().refine(&gpui::StyleRefinement {
                            margin: gpui::EdgesRefinement {
                                bottom: Some(gpui::Length::Definite(px(0.).into())),
                                ..Default::default()
                            },
                            ..Default::default()
                        });
                    }
                    let mut inner = div()
                        .id(("live-code-inner", block_id))
                        .debug_selector(|| "live-code-inner".into())
                        // Preview parity: the read-only card scrolls long
                        // Nowrap lines horizontally instead of clipping them.
                        .overflow_x_scroll();
                    if lang.is_some() {
                        inner = inner.pt(px(24.0));
                    }
                    for pi in code {
                        inner = inner.child(live_line(*pi, &paints, &entity, &colors));
                    }
                    let mut card = outer.child(inner);
                    if let Some(lang) = lang {
                        card = card.child(
                            div()
                                .absolute()
                                .top(px(7.0))
                                .left(px(12.0))
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(4.0))
                                .bg(ui.text.opacity(0.06))
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight(600.))
                                .text_color(ui.text_muted)
                                .child(lang.to_uppercase())
                                .into_any_element(),
                        );
                    }
                    let is_copied = self.copied_code_blocks.contains(&block_id);
                    let view_ent = entity.clone();
                    let copy_btn = div()
                        .id(("live-copy", block_id))
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .invisible()
                        .group_hover("live-code", |s| s.visible())
                        .child(if is_copied {
                            svg()
                                .size(px(16.0))
                                .text_color(ui.success)
                                .path(Icon::Check.path())
                        } else {
                            svg()
                                .size(px(16.0))
                                .text_color(ui.text_muted)
                                .path(Icon::Copy.path())
                        })
                        .on_click(move |_: &ClickEvent, _, cx: &mut gpui::App| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                code_text.clone(),
                            ));
                            view_ent.update(cx, |v, cx| {
                                v.mark_code_block_copied(block_id, cx)
                            });
                        })
                        .into_any_element();
                    card = card.child(
                        div()
                            .absolute()
                            .top(px(6.0))
                            .right(px(6.0))
                            .flex()
                            .rounded(px(6.0))
                            .bg(theme.editor_background())
                            .border_1()
                            .border_color(ui.border.opacity(0.18))
                            .shadow_sm()
                            .child(copy_btn)
                            .into_any_element(),
                    );
                    let group_last_line = close
                        .map(|c| rows[c].line)
                        .unwrap_or_else(|| rows[*code.last().unwrap()].line);
                    if rows[*open].line <= cursor.line && cursor.line <= group_last_line {
                        caret_el_ix = Some(row_els.len());
                    }
                    // Fallback target: the first code line (lands the cursor
                    // inside the block without flipping the card).
                    self.row_targets.push((
                        rows[*code.first().unwrap()].line,
                        group_last_line,
                    ));
                    row_els.push(card.into_any_element());
                }
                Group::Para(idxs) => {
                    // Consecutive paragraphs render as ONE block (preview
                    // parity: joined into a single paragraph there, so no
                    // inter-row margins). Image-only lines inside the run
                    // keep their shared card.
                    let first = &rows[idxs[0]];
                    let is_first = first.line == 0;
                    let is_last = rows[*idxs.last().unwrap()].line == last_line;
                    let mut d = div()
                        .w_full()
                        .flex_shrink_0()
                        .my_3()
                        .line_height(rems(1.75))
                        .debug_selector(|| "live-para".into());
                    if is_first {
                        d = d.mt_0();
                    }
                    if is_last {
                        d = d.mb_0();
                    }
                    for pi in idxs {
                        let src = self.buffer.line(rows[*pi].line).unwrap_or("");
                        if !rows[*pi].plan.revealed
                            && let Some((alt, url)) = parse_image_line(src)
                        {
                            d = d.child(MarkdownElementBuilder::image_card(
                                &url, &alt, None, cx,
                            ));
                        } else {
                            d = d.child(live_line(*pi, &paints, &entity, &colors));
                        }
                    }
                    let a = rows[idxs[0]].line;
                    let b = rows[*idxs.last().unwrap()].line;
                    if a <= cursor.line && cursor.line <= b {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((a, b));
                    row_els.push(d.into_any_element());
                }
                Group::Quote(idxs) => {
                    let first = &rows[idxs[0]];
                    let depth = match first.block {
                        Block::Quote { depth, .. } => depth,
                        _ => 1,
                    };
                    let callout = first
                        .callout
                        .as_ref()
                        .filter(|c| c.is_first)
                        .map(|c| c.kind);
                    let is_first = first.line == 0;
                    let is_last = rows[*idxs.last().unwrap()].line == last_line;
                    let (border_color, bg, bw) = match callout {
                        Some(kind) => {
                            let accent = callout_accent(kind, &theme);
                            (
                                accent,
                                accent.opacity(if is_dark { 0.18 } else { 0.13 }),
                                px(4.),
                            )
                        }
                        None => (
                            colors.quote_bar,
                            ui.text.opacity(if is_dark { 0.05 } else { 0.03 }),
                            px(3.),
                        ),
                    };
                    let mut card = div()
                        .flex_shrink_0()
                        .my_3()
                        .py_3()
                        .pr_4()
                        .rounded(px(6.0))
                        .bg(bg);
                    card.style().refine(&gpui::StyleRefinement {
                        padding: gpui::EdgesRefinement {
                            left: Some(gpui::DefiniteLength::Absolute(
                                gpui::AbsoluteLength::Pixels(px(20.)),
                            )),
                            ..Default::default()
                        },
                        border_widths: gpui::EdgesRefinement {
                            left: Some(gpui::AbsoluteLength::Pixels(bw)),
                            ..Default::default()
                        },
                        border_color: Some(border_color),
                        ..Default::default()
                    });
                    if is_first {
                        card = card.mt_0();
                    }
                    if is_last {
                        card = card.mb_0();
                    }
                    if depth > 1 {
                        card = card.ml(px((depth - 1) as f32 * 20.0));
                    }
                    if let Some(kind) = callout {
                        let accent = callout_accent(kind, &theme);
                        card = card.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .mb_2()
                                .text_size(px(12.))
                                .font_weight(gpui::FontWeight(700.))
                                .text_color(accent)
                                .child(div().size(px(10.)).rounded_full().bg(accent))
                                .child(kind.label().to_string())
                                .into_any_element(),
                        );
                    }
                    for pi in idxs {
                        card = card.child(live_line(*pi, &paints, &entity, &colors));
                    }
                    if idxs.iter().any(|pi| rows[*pi].line == cursor.line) {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((
                        rows[idxs[0]].line,
                        rows[*idxs.last().unwrap()].line,
                    ));
                    row_els.push(card.into_any_element());
                }
                Group::Table(idxs) => {
                    let revealed = idxs.iter().any(|c| rows[*c].plan.revealed);
                    let src_lines: Vec<String> = idxs
                        .iter()
                        .map(|c| self.buffer.line(rows[*c].line).unwrap_or("").to_string())
                        .collect();
                    let grid_rows: Vec<Vec<String>> =
                        src_lines.iter().map(|l| split_table_row(l)).collect();
                    // GFM: the delimiter row must carry the header's column
                    // count, otherwise this is not a table at all (same rule
                    // pulldown-cmark applies in the read-only preview).
                    let has_delimiter = grid_rows.len() > 1
                        && table_delimiter_cols(&src_lines[1]).is_some_and(|c| {
                            c == grid_rows[0].len() && c > 0
                        });
                    if revealed || !has_delimiter {
                        // Source editing (or not really a table): mono rows.
                        for pi in idxs {
                            if rows[*pi].line == cursor.line {
                                caret_el_ix = Some(row_els.len());
                            }
                            self.row_targets.push((rows[*pi].line, rows[*pi].line));
                            row_els.push(
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(*pi, &paints, &entity, &colors))
                                    .into_any_element(),
                            );
                        }
                        continue;
                    }
                    // Preview-parity grid.
                    let col_count = grid_rows.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
                    let aligns: Vec<CellAlign> = (0..col_count)
                        .map(|c| {
                            grid_rows
                                .get(1)
                                .and_then(|r| r.get(c))
                                .map(|c| align_of(c))
                                .unwrap_or(CellAlign::None)
                        })
                        .collect();
                    let start_line = rows[idxs[0]].line;
                    let is_first = start_line == 0;
                    let is_last = rows[*idxs.last().unwrap()].line == last_line;
                    let mut outer = div()
                        .id(("live-table", start_line))
                        .debug_selector(|| "live-table-outer".into())
                        .w_full()
                        .flex_shrink_0()
                        .mt_6()
                        .mb_4()
                        .rounded(px(8.0))
                        .border(px(1.0))
                        .border_color(colors.table_border)
                        .bg(colors.panel_bg.opacity(0.5));
                    if is_first {
                        outer = outer.mt_0();
                    }
                    if is_last {
                        outer = outer.mb_0();
                    }
                    // Click the grid to jump the cursor into the block source.
                    outer = outer.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _w, cx| {
                            this.edit(cx, |m| m.move_to(start_line, 0, false));
                            cx.notify();
                        }),
                    );
                    let mut grid = div()
                        .grid()
                        .grid_cols(col_count as u16)
                        .w_full()
                        .rounded(px(8.0))
                        .overflow_hidden();
                    let base = style.base_text_style.clone();
                    // grid_rows[0] = header, [1] = delimiter (skipped),
                    // [2..] = data. `preview_idx` matches the preview's
                    // row indices (header 0, data 1, 2, …) for dividers/zebra.
                    for (di, cells) in grid_rows.iter().enumerate() {
                        if di == 1 {
                            continue;
                        }
                        let is_header = di == 0;
                        let preview_idx = di - usize::from(di > 0);
                        for (col, align) in aligns.iter().enumerate() {
                            let cell_text =
                                cells.get(col).map(|s| s.as_str()).unwrap_or("");
                            let mut cell_div = div().px_3().py_2().text_size(px(14.));
                            if col > 0 || preview_idx > 0 {
                                cell_div = cell_div.border_color(colors.table_border);
                                if col > 0 {
                                    cell_div = cell_div.border_l(px(1.0));
                                }
                                if preview_idx > 0 {
                                    cell_div = cell_div.border_t(px(1.0));
                                }
                            }
                            if is_header {
                                cell_div = cell_div.bg(colors.elevated_bg);
                            } else if preview_idx % 2 == 0 {
                                cell_div = cell_div.bg(colors.panel_bg);
                            }
                            match align {
                                CellAlign::Left => cell_div = cell_div.justify_start(),
                                CellAlign::Center => cell_div = cell_div.justify_center(),
                                CellAlign::Right => cell_div = cell_div.justify_end(),
                                CellAlign::None => {}
                            }
                            let cell_runs =
                                inline_cell_runs(cell_text, &base, &colors, &style);
                            cell_div = cell_div.child(
                                StyledText::new(SharedString::from(cell_text.to_string()))
                                    .with_runs(cell_runs),
                            );
                            grid = grid.child(cell_div.into_any_element());
                        }
                    }
                    outer = outer.child(grid);
                    if idxs.iter().any(|pi| rows[*pi].line == cursor.line) {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((
                        rows[idxs[0]].line,
                        rows[*idxs.last().unwrap()].line,
                    ));
                    row_els.push(outer.into_any_element());
                }
            }
        }

        // Scroll-to-caret: the rows column (`live-markdown-content`) tracks
        // every direct child with the scroll handle, so `scroll_to_item`
        // scrolls the minimal amount to bring the caret's element into view
        // (guise `ensure_cursor_visible` parity). Consumed by the column's
        // prepaint in this same frame.
        if self.scroll_to_cursor {
            self.scroll_to_cursor = false;
            if let Some(ix) = caret_el_ix {
                self.scroll.scroll_to_item(ix);
            }
        }

        if rows.is_empty() || self.buffer.is_empty() {
            let show_ph = self.buffer.is_empty() && !focused && !self.placeholder.is_empty();
            if show_ph {
                self.row_targets.push((0, 0));
                row_els.push(
                    div()
                        .text_color(colors.marker)
                        .child(self.placeholder.clone())
                        .into_any_element(),
                );
            } else if rows.is_empty() {
                let ent = entity.clone();
                if show_caret {
                    caret_el_ix = Some(row_els.len());
                }
                self.row_targets.push((0, 0));
                row_els.push(
                    div()
                        .flex_1()
                        .child(
                            LiveLine::new(
                                ("live-line", 0usize),
                                SharedString::from(""),
                                vec![],
                                0,
                                ent,
                                false,
                                show_caret.then_some(0),
                                colors.caret,
                                CURSOR_WIDTH,
                                px(26.),
                            )
                            .with_caret_shape(self.cursor_shape),
                        )
                        .into_any_element(),
                );
            }
        }

        // Scroll-to-caret: the rows column (`live-markdown-content`) tracks
        // every direct child with the scroll handle, so `scroll_to_item`
        // scrolls the minimal amount to bring the caret's element into view
        // (guise `ensure_cursor_visible` parity). Consumed by the column's
        // prepaint in this same frame.
        if self.scroll_to_cursor {
            self.scroll_to_cursor = false;
            if let Some(ix) = caret_el_ix {
                self.scroll.scroll_to_item(ix);
            }
        }

        // Outline (Typora Outline parity) + current section highlight.
        let outline = outline_entries(&rows);
        let current_section = outline
            .iter()
            .filter(|(line, _, _)| *line <= cursor.line)
            .last()
            .map(|(line, _, _)| *line);

        // Status strip (Typora status-bar parity): cursor + counts + outline toggle.
        let mut stat_chars = 0usize;
        for line in self.buffer.lines() {
            stat_chars += line.chars().filter(|c| !c.is_whitespace()).count();
        }
        let stat_lines = self.buffer.lines().len();
        let sel_chars = self
            .selected_source_text()
            .map(|t| t.chars().filter(|c| !c.is_whitespace()).count());
        let show_outline = self.show_outline;
        let status = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .mt(px(2.))
            .pt(px(6.))
            .border_t_1()
            .border_color(ui.border.opacity(0.4))
            .text_size(px(11.))
            .text_color(ui.text_muted)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(format!("行 {}, 列 {}", cursor.line + 1, cursor.col + 1))
                    .child(format!("共 {stat_chars} 字 · {stat_lines} 行"))
                    .when_some(sel_chars, |this, n| {
                        this.child(
                            div()
                                .text_color(ui.text_muted.opacity(0.8))
                                .child(format!("已选 {n} 字")),
                        )
                    }),
            )
            .child(
                div()
                    .id("live-outline-toggle")
                    .cursor_pointer()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .text_color(if show_outline {
                        ui.accent
                    } else {
                        ui.text_muted
                    })
                    .hover(|this| this.bg(ui.tab_hover_background))
                    .child(if show_outline { "大纲 ✓" } else { "大纲" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, window, cx| {
                            window.focus(&this.focus, cx);
                            this.show_outline = !this.show_outline;
                            cx.notify();
                            cx.stop_propagation();
                        }),
                    ),
            );

        // Toggleable outline side panel: click jumps (focus kept).
        let outline_panel = show_outline.then(|| {
            div()
                .id("live-outline")
                .w(px(230.))
                .h_full()
                .flex_shrink_0()
                .border_l_1()
                .border_color(ui.border.opacity(0.5))
                .bg(ui.panel_background)
                .flex()
                .flex_col()
                .py_4()
                .child(
                    div()
                        .px(px(12.))
                        .mb_2()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight(600.))
                        .text_color(ui.text_muted)
                        .child("大纲"),
                )
                .child(if outline.is_empty() {
                    div()
                        .px(px(12.))
                        .text_size(px(12.))
                        .text_color(ui.text_muted.opacity(0.6))
                        .child("暂无标题")
                        .into_any_element()
                } else {
                    div()
                        .id("live-outline-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px(px(6.))
                        .children(outline.iter().map(|(line, level, title)| {
                            let line = *line;
                            let current = Some(line) == current_section;
                            div()
                                .id(("live-outline-item", line))
                                .pl(px(6. + (*level as f32 - 1.) * 12.))
                                .pr(px(6.))
                                .py(px(3.))
                                .rounded(px(6.))
                                .text_size(px(12.))
                                .whitespace_nowrap()
                                .truncate()
                                .overflow_hidden()
                                .cursor_pointer()
                                .text_color(if current { ui.accent } else { ui.text })
                                .when(current, |this| this.bg(ui.accent.opacity(0.12)))
                                .hover(|this| this.bg(ui.tab_hover_background))
                                .child(title.clone())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _e, window, cx| {
                                        window.focus(&this.focus, cx);
                                        this.edit(cx, |m| m.move_to(line, 0, false));
                                        cx.notify();
                                        cx.stop_propagation();
                                    }),
                                )
                                .into_any_element()
                        }))
                        .into_any_element()
                })
        });

        // Preview-parity container: centered 780px column, no editor chrome.
        div()
            .id("live-markdown")
            .key_context("LiveMarkdown")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            // Dead-zone fallback: clicks no row element claimed (margins,
            // card paddings, past-EOL areas) land the cursor on the nearest
            // line. Runs after every row handler (registration order), so
            // claimed clicks never reach it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.on_fallback_mouse_down(event, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &Copy, _window, cx| {
                if let Some(t) = this.selected_source_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(t));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _window, cx| {
                this.edit(cx, |m| m.select_all());
                cx.notify();
            }))
            .flex()
            .justify_center()
            .size_full()
            .overflow_hidden()
            .bg(ui.app_background)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .max_w(px(780.))
                            .px_8()
                            .pt_8()
                            .pb_2()
                            .h_full()
                            .overflow_hidden()
                            .child(
                                div()
                                    .id("live-markdown-content")
                                    .debug_selector(|| "live-markdown-content".into())
                                    .flex()
                                    .flex_col()
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .children(row_els),
                            )
                            .child(status),
                    ),
            )
            .when_some(outline_panel, |this, panel| this.child(panel))
    }
}

#[cfg(test)]
mod live_render_tests {
    use super::*;
    use gpui::{point, size};

    const SAMPLE: &str = "# Title\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\npara text\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    const SHOWCASE: &str = include_str!("../../../markdown-showcase.md");

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    /// Every non-blank planned row (code, table, quote, …) must shape to a
    /// non-zero box with the real theme + fonts + tree-sitter runs.
    #[gpui::test]
    fn live_lines_shape_with_nonzero_size(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        // 1. Build the exact LiveLines the view would build.
        struct Case {
            label: String,
            line: LiveLine,
        }
        let cases: Vec<Case> = cx.update(|window, cx| {
            let theme = cx.theme().clone();
            let style = MarkdownStyle::themed(window, cx);
            let colors = LiveColors::resolve(&style, &theme);
            let hl = HighlightCache::new();
            let lines: Vec<String> = SAMPLE.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
                },
            );
            rows.iter()
                .enumerate()
                .filter(|(_, r)| !r.plan.visible.is_empty())
                .map(|(pi, r)| {
                    let base = row_base_style(&r.plan, &style);
                    let runs = runs_for_row(&r.plan, &base, &colors, &style, &hl, &theme);
                    let total: usize = runs.iter().map(|x| x.len).sum();
                    let label = format!("line {} {:?} {:?}", r.line, r.plan.kind, r.plan.visible);
                    assert_eq!(total, r.plan.visible.len(), "runs must cover {label}");
                    for run in &runs {
                        assert!(
                            run.color.a > 0.01,
                            "transparent text run for {label} (run={run:?})"
                        );
                    }
                    let line = LiveLine::new(
                        ("live-test-line", pi),
                        SharedString::from(r.plan.visible.clone()),
                        runs,
                        r.line,
                        view.clone(),
                        false,
                        None,
                        colors.caret,
                        px(2.),
                        px(24.),
                    );
                    Case { label, line }
                })
                .collect()
        });
        assert!(!cases.is_empty(), "sample must produce rows");
        // 2. Shape each line for real: full request_layout → prepaint → paint.
        for case in cases {
            let (styled, _) = cx.draw(
                point(px(0.), px(0.)),
                size(px(700.), px(200.)),
                |_, _| case.line,
            );
            let bounds = styled.layout().bounds();
            assert!(
                bounds.size.width > px(0.),
                "zero width for {}",
                case.label
            );
            assert!(
                bounds.size.height > px(0.),
                "zero height for {}",
                case.label
            );
        }
    }

    /// Showcase repro: shape every row of the real showcase doc and dump
    /// heights per kind. Code rows must not collapse.
    #[gpui::test]
    fn showcase_rows_keep_sane_heights(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SHOWCASE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        struct Case {
            label: String,
            line: LiveLine,
        }
        let cases: Vec<Case> = cx.update(|window, cx| {
            let theme = cx.theme().clone();
            let style = MarkdownStyle::themed(window, cx);
            let colors = LiveColors::resolve(&style, &theme);
            let hl = HighlightCache::new();
            let lines: Vec<String> = SHOWCASE.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
                },
            );
            rows.iter()
                .enumerate()
                .filter(|(_, r)| !r.plan.visible.is_empty())
                .map(|(pi, r)| {
                    let base = row_base_style(&r.plan, &style);
                    let runs = runs_for_row(&r.plan, &base, &colors, &style, &hl, &theme);
                    let label = format!("L{} {:?} {:?}", r.line, r.plan.kind, r.plan.visible.chars().take(24).collect::<String>());
                    Case {
                        label,
                        line: LiveLine::new(
                            ("live-showcase-line", pi),
                            SharedString::from(r.plan.visible.clone()),
                            runs,
                            r.line,
                            view.clone(),
                            false,
                            None,
                            colors.caret,
                            px(2.),
                            px(24.),
                        ),
                    }
                })
                .collect()
        });
        let mut code_h = Vec::new();
        let mut body_h = Vec::new();
        for case in cases {
            let label = case.label.clone();
            let (styled, _) = cx.draw(
                point(px(0.), px(0.)),
                size(px(700.), px(200.)),
                |_, _| case.line,
            );
            let b = styled.layout().bounds();
            if label.contains("Code {") {
                code_h.push((label, b.size.height, b.size.width));
            } else if label.contains("Paragraph") {
                body_h.push(b.size.height);
            }
        }
        eprintln!("SHOWCASE code rows: {code_h:?}");
        eprintln!("SHOWCASE body heights: {body_h:?}");
        assert!(!code_h.is_empty(), "showcase must have code rows");
        for (label, h, _) in &code_h {
            assert!(*h > px(10.), "collapsed code row {label} h={h:?}");
        }
    }
}


#[cfg(test)]
mod live_frame_tests {
    use super::*;

    const SAMPLE: &str = "# Title\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\npara text\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Full-frame assertions shared by all window sizes.
    fn assert_frames(cx: &mut gpui::VisualTestContext, view: &gpui::Entity<LiveMarkdownView>) {
        // Frame 1: unfocused full preview → code card + table grid present.
        cx.refresh().unwrap();
        cx.run_until_parked();
        let code = cx.debug_bounds("live-code-inner");
        let table = cx.debug_bounds("live-table-outer");
        eprintln!("F1 CODE bounds: {code:?}");
        eprintln!("F1 TABLE bounds: {table:?}");
        assert!(
            code.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
            "code card inner missing/collapsed: {code:?}"
        );
        assert!(
            table.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
            "table grid missing/collapsed: {table:?}"
        );
        // Frame 2: focused, cursor on a CODE line → the card stays and the
        // code is edited in place (Obsidian behavior; only fence lines flip
        // a block to source mode).
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
            view.update(cx, |v, cx| {
                v.edit(cx, |m| m.move_to(3, 2, false));
            });
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-code-inner").is_some(),
            "cursor on a code line must keep the rendered card"
        );
        // Frame 3: cursor on the opening fence line → source mode, the
        // card disappears (fences visible for editing).
        cx.update(|_window, cx| {
            view.update(cx, |v, cx| {
                v.edit(cx, |m| m.move_to(2, 3, false));
            });
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-code-inner").is_none(),
            "cursor on the fence line must drop the card (source mode)"
        );
    }

    /// Dynamic repro: type inside a code block and move the cursor around;
    /// code rows must keep painting after every replan (no stale caches).
    /// Since the card no longer flips while editing code (only fence lines
    /// flip it), the card must stay present the whole time.
    #[gpui::test]
    fn live_typing_keeps_code_painted(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        // Reset the paint log so assertions below see only this test's
        // frames (it is a process-wide static).
        let _ = crate::editor::markdown_live::element::LIVE_LINE_PAINT_LOG
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        // Move cursor into the code block (line 3) and type.
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(3, 2, false));
        });
        cx.simulate_input("x");
        cx.run_until_parked();
        let painted: Vec<(usize, usize)> = crate::editor::markdown_live::element::LIVE_LINE_PAINT_LOG
            .lock()
            .unwrap()
            .drain(..)
            .collect();
        eprintln!("AFTER-TYPE painted: {painted:?}");
        // Card mode: the code lines render inside the card; the fence
        // lines themselves stay hidden.
        for must in [3usize, 4, 5] {
            assert!(
                painted.iter().any(|(l, _)| *l == must),
                "code line {must} missing after typing (painted={painted:?})"
            );
        }
        // (No negative "fence hidden" assertion here: the paint log is a
        // process-wide static and parallel tests pollute it. The card
        // presence below is the semantic check — a flipped block would have
        // dropped `live-code-inner`.)
        assert!(
            cx.debug_bounds("live-code-inner").is_some_and(|b| b.size.height > px(50.)),
            "code card must stay while editing inside it"
        );
        // Move cursor far away (line 0): the card stays (it never left) and
        // code keeps painting.
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 0, false));
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-code-inner").is_some_and(|b| b.size.height > px(50.)),
            "code card missing after leaving the block"
        );
        // The typed char must have stuck in the buffer.
        let text = view.read_with(cx, |v, _| v.text());
        assert!(text.contains("fnx ") || text.contains("fn x"), "typed char lost: {text:?}");
    }

    /// Repro: the real showcase doc, unfocused (full preview) — code cards
    /// and table grids must all be in the render tree with real boxes.
    /// Guards the `:---` delimiter regression (left-aligned columns used to
    /// fail, so every grid fell back to raw source lines).
    #[gpui::test]
    fn live_showcase_frame_renders_code_and_tables(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = include_str!("../../../markdown-showcase.md");
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx)))
        });
        let _view = host.read_with(cx, |h, _| h.0.clone());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let code = cx.debug_bounds("live-code-inner");
        let table = cx.debug_bounds("live-table-outer");
        eprintln!("SHOWCASE CODE: {code:?}");
        eprintln!("SHOWCASE TABLE: {table:?}");
        assert!(
            code.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
            "showcase code card missing: {code:?}"
        );
        assert!(
            table.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
            "showcase table grid missing: {table:?}"
        );
    }

    /// Real repo docs: every fenced block / table in the file must come out
    /// as a card in the live editor too, not as raw source lines.
    #[gpui::test]
    fn live_real_docs_render_code_and_tables(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        for (name, doc, want_code, want_table) in [
            (
                "CODE_REVIEW",
                include_str!("../../../docs/CODE_REVIEW.md"),
                false,
                true,
            ),
            (
                "TAB_SPEC",
                include_str!("../../../docs/safari-26-tab-spec.md"),
                true,
                true,
            ),
            (
                "AUDIT",
                include_str!("../../../docs/CODE_AUDIT_2026-09-04.md"),
                false,
                true,
            ),
        ] {
            let (host, cx) = cx.add_window_view(|_, cx| {
                Host(cx.new(|cx| LiveMarkdownView::with_text(doc, cx)))
            });
            let _view = host.read_with(cx, |h, _| h.0.clone());
            cx.refresh().unwrap();
            cx.run_until_parked();
            let code = cx.debug_bounds("live-code-inner");
            let table = cx.debug_bounds("live-table-outer");
            assert_eq!(
                code.is_some_and(|b| b.size.height > px(0.)),
                want_code,
                "{name}: code card present={} expected={want_code}",
                code.is_some()
            );
            assert_eq!(
                table.is_some_and(|b| b.size.height > px(0.)),
                want_table,
                "{name}: table grid present={} expected={want_table}",
                table.is_some()
            );
        }
    }

    /// Repro of the field bug: the view nested inside the editor tab
    /// container (`flex_1 / min_h_0 / overflow_hidden` chain), where code
    /// cards collapsed to a thin pill strip — language badge visible, code
    /// rows gone. The bare-window tests above can't see this.
    #[gpui::test]
    fn live_inside_editor_tab_container_keeps_code_rows(cx: &mut gpui::TestAppContext) {
        struct EditorHost(gpui::Entity<LiveMarkdownView>);
        impl gpui::Render for EditorHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                // Mirrors the `md-live` branch in editor.rs.
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("md-live")
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .px(px(6.0))
                            .pb(px(12.0))
                            .child(self.0.clone()),
                    )
            }
        }
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            EditorHost(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let code = cx.debug_bounds("live-code-inner");
        eprintln!("TAB-CONTAINER CODE: {code:?}");
        assert!(
            code.is_some_and(|b| b.size.height > px(50.)),
            "code rows collapsed inside the tab container: {code:?}"
        );
        let _ = view;
    }

    #[gpui::test]
    fn live_frame_maximized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        assert_frames(cx, &view);
    }

    #[gpui::test]
    fn live_frame_narrow_pane(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(800.), px(600.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        assert_frames(&mut vc, &view);
    }
}

#[cfg(test)]
mod live_showcase_tests {
    use super::*;

    const SHOWCASE: &str = include_str!("../../../markdown-showcase.md");

    /// The exact user repro: plan the showcase doc and report every code /
    /// table row whose visible text is empty or suspiciously short.
    #[test]
    fn showcase_code_and_table_rows_have_visible_text() {
        let lines: Vec<String> = SHOWCASE.split('\n').map(str::to_string).collect();
        let rows = plan_document(
            &lines,
            Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: false,
                read_only: false,
            },
        );
        assert_eq!(rows.len(), lines.len());
        let mut empties = Vec::new();
        let mut code_count = 0;
        let mut table_count = 0;
        for row in &rows {
            match &row.plan.kind {
                RowKind::Code { lang } => {
                    code_count += 1;
                    let src = &lines[row.line];
                    if row.plan.visible.is_empty() && !src.is_empty() {
                        empties.push((row.line, format!("CODE[{lang:?}] src={src:?}")));
                    }
                }
                RowKind::Table => {
                    table_count += 1;
                    let src = &lines[row.line];
                    if row.plan.visible.is_empty() && !src.is_empty() {
                        empties.push((row.line, format!("TABLE src={src:?}")));
                    }
                }
                RowKind::Fence { .. } => {}
                _ => {}
            }
        }
        eprintln!("code rows: {code_count}, table rows: {table_count}");
        assert!(
            code_count > 10,
            "showcase must contain plenty of code rows"
        );
        assert!(
            empties.is_empty(),
            "code/table rows with empty visible: {empties:?}"
        );
    }

    /// Parity guard: every block the read-only preview (pulldown-cmark)
    /// turns into a code card / table grid must be classified the same way
    /// by the live classifier, or the two renderers drift apart again —
    /// which is exactly how `:---` tables and list-nested fences were lost.
    #[test]
    fn outline_collects_headings_in_order() {
        let doc = "# A\n\ntext\n\n## B\n\n### C\n\n## D\n";
        let lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
        let rows = plan_document(
            &lines,
            Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: false,
                read_only: false,
            },
        );
        let outline = outline_entries(&rows);
        assert_eq!(
            outline
                .iter()
                .map(|(l, v, t)| (*l, *v, t.clone()))
                .collect::<Vec<_>>(),
            vec![
                (0, 1, "A".to_string()),
                (4, 2, "B".to_string()),
                (6, 3, "C".to_string()),
                (8, 2, "D".to_string()),
            ]
        );
        assert_eq!(outline_title("  a  b  "), "a b");
        assert_eq!(outline_title(""), "(空标题)");
    }

    #[test]
    fn live_blocks_match_preview_blocks() {
        for (name, doc) in [
            ("showcase", SHOWCASE),
            (
                "list_fence",
                "- item\n\n    ```python\n    x = 1\n    ```\n\nafter\n",
            ),
            ("pipeless_table", "\na | b\n--- | ---\n1 | 2\n\nend\n"),
            (
                "aligned_table",
                "| left | center | right |\n| :--- | :---: | ---: |\n| a | b | c |\n",
            ),
        ] {
            let lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
                },
            );
            for (kind, a, b) in preview_block_ranges(doc) {
                for row in rows.iter().take(b + 1).skip(a) {
                    let ok = match kind {
                        "code" => matches!(
                            row.block,
                            Block::Fence { .. } | Block::CodeLine
                        ),
                        _ => matches!(row.block, Block::Table),
                    };
                    assert!(
                        ok,
                        "{name}: preview sees {kind} on L{}, live says {:?} (src={:?})",
                        row.line,
                        row.block,
                        lines.get(row.line)
                    );
                }
            }
        }
    }

    /// Source line ranges of code blocks / tables, as pulldown-cmark sees
    /// them (same options as the read-only preview).
    fn preview_block_ranges(doc: &str) -> Vec<(&'static str, usize, usize)> {
        use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
        let mut starts = vec![0usize];
        let mut off = 0usize;
        for l in doc.split('\n') {
            off += l.len() + 1;
            starts.push(off);
        }
        let line_of = |b: usize| starts.iter().rposition(|&s| s <= b).unwrap_or(0);
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_TASKLISTS);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        let mut out = Vec::new();
        let mut code: Option<usize> = None;
        let mut table: Option<usize> = None;
        for (ev, range) in Parser::new_ext(doc, opts).into_offset_iter() {
            match ev {
                Event::Start(Tag::CodeBlock(_)) => code = Some(range.start),
                Event::End(TagEnd::CodeBlock) => {
                    if let Some(s) = code.take() {
                        out.push(("code", line_of(s), line_of(range.end.saturating_sub(1))));
                    }
                }
                Event::Start(Tag::Table(_)) => table = Some(range.start),
                Event::End(TagEnd::Table) => {
                    if let Some(s) = table.take() {
                        out.push(("table", line_of(s), line_of(range.end.saturating_sub(1))));
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// Manual visual diagnostic: renders the live view offscreen with real Metal
/// and saves `/tmp/live_code.png`.
/// Requires a macOS GUI session + main thread (fails headless):
/// `cargo test -p deepterm --lib live_code_screenshot -- --ignored --test-threads=1`
#[cfg(all(test, target_os = "macos"))]
mod live_screenshot_tests {
    use super::*;

    /// Mirrors markdown-showcase.md §5.1: one fenced block per language.
    const SHOT_SAMPLE: &str = "### 5.1 fence\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n```python\ndef f(x):\n    return x\n```\n";

    struct ShotHost(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for ShotHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    #[test]
    #[ignore]
    fn live_code_screenshot() {
        let mut cx = gpui::VisualTestAppContext::new(gpui_platform::current_platform(false));
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let window = cx
            .open_offscreen_window(gpui::size(px(900.), px(800.)), |_, cx| {
                cx.new(|cx| ShotHost(cx.new(|cx| LiveMarkdownView::with_text(SHOT_SAMPLE, cx))))
            })
            .unwrap();
        cx.run_until_parked();
        let shot = cx.capture_screenshot(window.into()).unwrap();
        shot.save("/tmp/live_code.png").unwrap();
        eprintln!("saved /tmp/live_code.png");
    }
}

#[cfg(test)]
mod layout_shrink_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Regression: `live-markdown-content` is a fixed-height flex column;
    /// every row element must be `flex_shrink_0` or blank/rule rows collapse
    /// to 0px on any document taller than the viewport (unclickable blank
    /// lines, caret overlapping the next row, lost paragraph spacing).
    #[gpui::test]
    fn long_doc_small_window_keeps_row_heights(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::new();
        for i in 0..12 {
            doc.push_str(&format!("# Heading {i}\n\n"));
            doc.push_str(
                "```rust\nfn a() {\n    let x = 1;\n    let y = 2;\n    println!(\"{x} {y}\");\n}\n```\n\n",
            );
            doc.push_str("some paragraph text to fill space and wrap around a bit here\n\n");
            doc.push_str("- item one\n- item two\n- item three\n\n");
            doc.push_str("---\n\n");
        }
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(400.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(&doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let _view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();

        let code = vc.debug_bounds("live-code-inner");
        assert!(
            code.is_some_and(|b| b.size.height > px(50.)),
            "code card collapsed on long doc: {code:?}"
        );
        // Blank rows must keep their 26px (they were 0px before the
        // flex_shrink_0 fix): clickable + visible caret row.
        let blank = vc.debug_bounds("live-blank-row");
        assert!(
            blank.is_some_and(|b| b.size.height > px(10.)),
            "blank row collapsed on long doc: {blank:?}"
        );
        // The scroll container must exist with a definite height.
        let content = vc.debug_bounds("live-markdown-content");
        assert!(
            content.is_some_and(|b| b.size.height > px(0.)),
            "content bounds missing: {content:?}"
        );
    }
}

#[cfg(test)]
mod scroll_to_caret_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Moving the caret below the viewport must scroll it into view
    /// (guise `ensure_cursor_visible` parity; the rows column tracks its
    /// children with the scroll handle and we `scroll_to_item` the caret's
    /// element).
    #[gpui::test]
    fn caret_below_viewport_scrolls_into_view(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::new();
        for i in 0..30 {
            doc.push_str(&format!("paragraph {i} with some text\n\n"));
        }
        doc.push_str("final line\n");
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(300.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(&doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();

        // Before: at top (offset 0).
        let offset0 = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert_eq!(offset0, 0.0, "initial scroll must be at top");

        // Focus + jump the caret to the last line.
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.doc_end(false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        let offset1 = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert!(
            offset1 < -50.0,
            "caret at doc end must scroll down (offset {offset1})"
        );

        // Jump back to the top: the caret's element scrolls back up.
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.doc_start(false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        let offset2 = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert!(
            offset2 > -50.0,
            "caret back at doc start must scroll up (offset {offset2})"
        );
    }
}

#[cfg(test)]
mod interaction_fallback_tests {
    use super::*;
    use gpui::point;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    const DOC: &str = "hello world text\n\nsecond paragraph here";

    fn setup(
        cx: &mut gpui::TestAppContext,
        doc: &str,
    ) -> (gpui::Entity<LiveMarkdownView>, gpui::VisualTestContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(500.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        (view, vc)
    }

    /// B4: a click on live text is claimed by the LiveLine and lands a
    /// precise column.
    #[gpui::test]
    fn click_on_text_is_precise(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, "hello world text");
        let para = vc.debug_bounds("live-para").expect("para group");
        // Click 40px into the first paragraph's text.
        vc.simulate_mouse_down(
            point(para.left() + px(40.), para.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let cursor = view.read_with(&vc, |v, _| v.buffer.cursor());
        assert_eq!(cursor.line, 0, "click in first paragraph");
        assert!(
            (1..=12).contains(&cursor.col),
            "x-precise column expected, got {}",
            cursor.col
        );
    }

    /// B4: clicks in dead zones (margins between rows, the blank row's
    /// padding) still land the cursor on the nearest line — no dead clicks.
    #[gpui::test]
    fn dead_zone_clicks_land_nearest_line(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, DOC);
        let blank = vc.debug_bounds("live-blank-row").expect("blank row");
        // 1) Click inside the blank row itself: its handler targets line 1.
        vc.simulate_mouse_down(
            point(blank.left() + px(100.), blank.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        assert_eq!(view.read_with(&vc, |v, _| v.buffer.cursor()).line, 1);

        // 2) Click in the margin just above the blank (paragraph A's
        // my_3 margin — no row element owns it): the root fallback maps to
        // the nearest line.
        vc.simulate_mouse_down(
            point(blank.left() + px(100.), blank.top() - px(6.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let cursor = view.read_with(&vc, |v, _| v.buffer.cursor());
        assert!(
            cursor.line <= 1,
            "margin click should land on the blank or the paragraph above, got line {}",
            cursor.line
        );

        // 3) Click in the margin just below the blank (paragraph B's
        // margin): nearest is the blank or paragraph B.
        vc.simulate_mouse_down(
            point(blank.left() + px(100.), blank.bottom() + px(6.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let cursor = view.read_with(&vc, |v, _| v.buffer.cursor());
        assert!(
            (1..=2).contains(&cursor.line),
            "margin click below the blank should land on line 1 or 2, got {}",
            cursor.line
        );
    }

    /// B5: dragging across the blank row keeps extending the selection
    /// (the drag fallback covers positions no LiveLine band contains).
    #[gpui::test]
    fn drag_across_blank_keeps_selecting(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, DOC);
        let blank = vc.debug_bounds("live-blank-row").expect("blank row");
        // Press on paragraph A, drag into the blank band, then into B.
        vc.simulate_mouse_down(
            point(blank.left() + px(10.), blank.top() - px(40.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.simulate_mouse_move(
            point(blank.left() + px(60.), blank.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let mid = view.read_with(&vc, |v, _| v.buffer.selection());
        assert!(
            mid.is_some_and(|(s, e)| s.line == 0 && e.line == 1),
            "drag into the blank should extend the selection to line 1, got {:?}",
            mid
        );
        vc.simulate_mouse_move(
            point(blank.left() + px(60.), blank.bottom() + px(40.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let end = view.read_with(&vc, |v, _| v.buffer.selection());
        assert!(
            end.is_some_and(|(s, e)| s.line == 0 && e.line == 2),
            "drag into paragraph B should extend the selection to line 2, got {:?}",
            end
        );
    }

    /// B6: Up/Down move by visual (soft-wrapped) rows with a sticky goal x.
    #[gpui::test]
    fn visual_row_movement(cx: &mut gpui::TestAppContext) {
        let long = "word ".repeat(60); // wraps to 3+ visual rows at 700px
        let doc = format!("{long}\nsecond line\n");
        let (view, mut vc) = setup(cx, &doc);
        let cursor = |vc: &gpui::VisualTestContext| {
            view.read_with(vc, |v, _| v.buffer.cursor())
        };
        // Start at the beginning of the wrapped paragraph.
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 0, false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        // Down twice stays on source line 0 (visual rows 1 → 2).
        view.update(&mut vc, |v, cx| v.move_visual(true, false, cx));
        let c1 = cursor(&vc);
        assert_eq!(c1.line, 0, "first Down stays on the wrapped line");
        assert!(c1.col > 0, "column advanced to visual row 2");
        // Keep moving down within the wrapped line: columns only advance.
        let mut prev = c1.col;
        let mut downs = 0;
        while cursor(&vc).line == 0 && downs < 10 {
            view.update(&mut vc, |v, cx| v.move_visual(true, false, cx));
            downs += 1;
            let c = cursor(&vc);
            if c.line != 0 {
                break; // crossed to the next source line
            }
            assert!(c.col > prev, "Down must advance within the line: {c:?}");
            prev = c.col;
        }
        // It eventually crosses to the next source line.
        let c3 = cursor(&vc);
        assert_eq!(c3.line, 1, "repeated Down crosses to line 1");
        // Up goes back to the LAST visual row of line 0 (sticky goal x).
        view.update(&mut vc, |v, cx| v.move_visual(false, false, cx));
        let c4 = cursor(&vc);
        assert_eq!(c4.line, 0, "Up returns to line 0");
        assert!(c4.col > c1.col, "and lands on its last visual row");
    }

    /// C2: consecutive paragraph lines render as ONE group without
    /// inter-row margins (the preview joins them into one paragraph).
    #[gpui::test]
    fn paragraph_run_renders_as_one_block(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, "line one here\nline two here");
        let para = vc.debug_bounds("live-para").expect("para group");
        // Two 15px/1.75 rows ≈ 52.5px. The old per-row my_3 layout added a
        // 24px gap between them (≈76.5px); one block must stay near 2 rows.
        assert!(
            para.size.height < px(65.),
            "two joined paragraph lines must not carry inter-row margins: {para:?}"
        );
        assert!(
            para.size.height > px(45.),
            "two lines of text: {para:?}"
        );
        let _ = view;
    }
}

#[cfg(test)]
mod setext_parity_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::Reveal;

    /// Live setext classification must match pulldown-cmark (the read-only
    /// preview's parser) on the constructs where setext, rules, and tables
    /// interact — the precedence was chosen from pulldown's behavior.
    #[test]
    fn setext_matches_pulldown() {
        use pulldown_cmark::{Event, Options, Parser, Tag};
        for (doc, expect_h) in [
            ("Title\n===\n", vec![(0, 1)]),
            ("Sub\n---\n", vec![(0, 1)]),
            ("a\nb\n===\n", vec![(0, 2)]),
            // Single-column delimiter after a word: setext wins (h2).
            ("a\n---\n", vec![(0, 1)]),
            // Multi-column delimiter row: a table, not a heading.
            ("a | b\n--- | ---\n", vec![]),
            // Blank line between: no setext.
            ("a\n\n---\n", vec![]),
        ] {
            let mut opts = Options::empty();
            opts.insert(Options::ENABLE_TABLES);
            let mut pulldown_ranges: Vec<(usize, usize)> = Vec::new();
            let mut starts = vec![0usize];
            let mut off = 0usize;
            for l in doc.split('\n') {
                off += l.len() + 1;
                starts.push(off);
            }
            let line_of = |b: usize| starts.iter().rposition(|&s| s <= b).unwrap_or(0);
            let mut open: Option<usize> = None;
            for (ev, range) in Parser::new_ext(doc, opts).into_offset_iter() {
                match ev {
                    Event::Start(Tag::Heading { .. }) => open = Some(range.start),
                    Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                        if let Some(s) = open.take() {
                            pulldown_ranges.push((line_of(s), line_of(range.end.saturating_sub(1))));
                        }
                    }
                    _ => {}
                }
            }
            let lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
                },
            );
            let mut live_ranges: Vec<(usize, usize)> = Vec::new();
            let mut open: Option<usize> = None;
            for row in &rows {
                let is_heading = matches!(row.block, Block::Heading { .. })
                    && !matches!(row.plan.kind, RowKind::Setext);
                let is_underline = matches!(row.block, Block::SetextUnderline { .. });
                if is_heading && open.is_none() {
                    open = Some(row.line);
                }
                if is_underline && let Some(s) = open.take() {
                    live_ranges.push((s, row.line));
                }
                if !is_heading && !is_underline && open.is_some() {
                    live_ranges.push((open.unwrap(), open.unwrap()));
                    open = None;
                }
            }
            if let Some(s) = open.take() {
                live_ranges.push((s, s));
            }
            assert_eq!(
                live_ranges, expect_h,
                "{doc:?}: live setext heading ranges {:?} != expected",
                live_ranges
            );
            assert_eq!(
                pulldown_ranges, expect_h,
                "{doc:?}: pulldown heading ranges {:?} != expected (live drift guard)",
                pulldown_ranges
            );
        }
    }
}

#[cfg(test)]
mod e2e_smoke_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Combined document exercising every fixed feature at once: setext
    /// headings, paragraph runs, a code card (edited in place), ordered
    /// numbering, blank rows, and scroll-to-caret.
    #[gpui::test]
    fn combined_document_smoke(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let doc = "Title\n===\n\nintro one\nintro two\n\n```rust\nfn main() {}\n```\n\n1. first\n2. second\n\nfinal\n";
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(240.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();

        // Everything renders: code card, table-free, setext gap row.
        assert!(
            vc.debug_bounds("live-code-inner").is_some_and(|b| b.size.height > px(20.)),
            "code card renders"
        );
        // Focus + edit inside the code: card stays.
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(7, 11, false)); // inside fn main() {}
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        assert!(
            vc.debug_bounds("live-code-inner").is_some(),
            "card stays while editing code"
        );
        // Jump to the end: scrolls into view.
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.doc_end(false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        assert!(
            view.read_with(&vc, |v, _| v.scroll_offset_y()) < -10.0,
            "doc end scrolls into view"
        );
        // Click the blank above "final" via the fallback: lands a line.
        let blank = vc.debug_bounds("live-blank-row").expect("blank");
        vc.simulate_mouse_down(
            gpui::point(blank.left() + px(50.), blank.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let line = view.read_with(&vc, |v, _| v.buffer.cursor().line);
        assert!(line >= 10, "blank click lands near the end, got line {line}");
    }
}
