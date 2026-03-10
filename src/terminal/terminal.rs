//! Terminal module - simplified but functional

use std::{borrow::Cow, path::PathBuf, sync::Arc};

use alacritty_terminal::{
    Term,
    event::{Event as AlacTermEvent, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll as AlacScroll},
    index::{Column, Line, Point as AlacPoint, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, TermMode},
    tty::{self},
    vte::ansi::Handler,
};
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::prelude::*;
use gpui::{
    App, AppContext as _, Bounds, ClipboardItem, Context, Entity, EventEmitter, Pixels, Point,
    Size, Task, Window, px,
};

use crate::terminal::mappings::mouse::{
    alt_scroll, grid_point, grid_point_and_side, mouse_button_report, mouse_moved_report,
    scroll_report,
};

/// Terminal events
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    TitleChanged,
    CloseTerminal,
    Bell,
    Wakeup,
    SelectionsChanged,
}

#[derive(Clone)]
pub struct ZedListener(pub UnboundedSender<AlacTermEvent>);

impl EventListener for ZedListener {
    fn send_event(&self, event: AlacTermEvent) {
        self.0.unbounded_send(event).ok();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalBounds {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub bounds: Bounds<Pixels>,
}

impl TerminalBounds {
    pub fn new(line_height: Pixels, cell_width: Pixels, bounds: Bounds<Pixels>) -> Self {
        TerminalBounds {
            cell_width,
            line_height,
            bounds,
        }
    }

    pub fn num_lines(&self) -> usize {
        (self.bounds.size.height.as_f32() / self.line_height.as_f32()).floor() as usize
    }

    pub fn num_columns(&self) -> usize {
        (self.bounds.size.width.as_f32() / self.cell_width.as_f32()).floor() as usize
    }

    pub fn last_column(&self) -> Column {
        Column(self.num_columns().saturating_sub(1))
    }

    pub fn bottommost_line(&self) -> Line {
        Line(self.num_lines().saturating_sub(1) as i32)
    }

    pub fn width(&self) -> Pixels {
        self.bounds.size.width
    }

    pub fn height(&self) -> Pixels {
        self.bounds.size.height
    }
}

impl Default for TerminalBounds {
    fn default() -> Self {
        TerminalBounds::new(
            px(16.0),
            px(9.0),
            Bounds {
                origin: Point::default(),
                size: Size {
                    width: px(500.0),
                    height: px(300.0),
                },
            },
        )
    }
}

impl From<TerminalBounds> for WindowSize {
    fn from(val: TerminalBounds) -> Self {
        WindowSize {
            num_lines: val.num_lines() as u16,
            num_cols: val.num_columns() as u16,
            cell_width: val.cell_width.as_f32() as u16,
            cell_height: val.line_height.as_f32() as u16,
        }
    }
}

impl Dimensions for TerminalBounds {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines()
    }

    fn columns(&self) -> usize {
        self.num_columns()
    }
}

/// Selection phase
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionPhase {
    Ended,
    Selecting,
    Started,
}

/// Terminal content
#[derive(Clone, Debug)]
pub struct TerminalContent {
    pub cells: Vec<IndexedCell>,
    pub cursor: AlacPoint,
    pub cursor_char: char,
    pub display_offset: usize,
    pub mode: alacritty_terminal::term::TermMode,
    pub cols: usize,
    pub rows: usize,
}

impl Default for TerminalContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            cursor: AlacPoint::new(Line(0), Column(0)),
            cursor_char: ' ',
            display_offset: 0,
            mode: alacritty_terminal::term::TermMode::default(),
            cols: 80,
            rows: 24,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexedCell {
    pub point: AlacPoint,
    pub cell: alacritty_terminal::term::cell::Cell,
}

pub struct Terminal {
    term: Arc<FairMutex<Term<ZedListener>>>,
    sender: Option<EventLoopSender>,
    last_content: TerminalContent,
    content_generation: u64,
    content_dirty: bool,
    event_loop_task: Option<Task<()>>,
    terminal_bounds: TerminalBounds,
    selection_phase: SelectionPhase,
    last_mouse_position: Option<Point<Pixels>>,
    /// Scroll accumulator for smooth scrolling (trackpad support)
    /// Accumulates partial scroll amounts and applies them when they reach a full line
    scroll_accumulator: f32,
}

impl Terminal {
    pub fn new(cx: &mut App, working_directory: Option<PathBuf>) -> Entity<Self> {
        let (events_tx, events_rx) = unbounded();
        let listener = ZedListener(events_tx.clone());

        // Choose a sensible default shell per platform. On Windows prefer `COMSPEC` or `cmd.exe`;
        // on Unix-like systems prefer the `SHELL` env var or `/bin/sh`.
        let default_shell = if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        };

        let pty_options = tty::Options {
            shell: Some(tty::Shell::new(default_shell, vec![])),
            working_directory,
            ..Default::default()
        };

        let default_bounds = TerminalBounds::default();
        let pty = match tty::new(&pty_options, default_bounds.into(), 0) {
            Ok(pty) => pty,
            Err(e) => {
                eprintln!("Failed to create PTY: {}", e);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };

        let mut config = Config::default();
        config.scrolling_history = 10000;
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &default_bounds,
            listener.clone(),
        )));

        let event_loop = match EventLoop::new(term.clone(), listener, pty, false, false) {
            Ok(event_loop) => event_loop,
            Err(e) => {
                eprintln!("Failed to create EventLoop: {}", e);
                return cx.new(|_cx| Self::create_error_terminal());
            }
        };

        let notifier = event_loop.channel();
        let _ = event_loop.spawn();

        cx.new(|cx| {
            let term_clone = term.clone();

            let event_loop_task = cx.spawn(async move |this, cx| {
                let mut events_rx = events_rx;
                while let Some(event) = events_rx.next().await {
                    let _ = this.update(cx, |terminal: &mut Self, cx| {
                        terminal.process_event(event, cx);
                    });
                    smol::future::yield_now().await;
                }
            });

            Self {
                term: term_clone,
                sender: Some(notifier),
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                event_loop_task: Some(event_loop_task),
                terminal_bounds: default_bounds,
                selection_phase: SelectionPhase::Ended,
                last_mouse_position: None,
                scroll_accumulator: 0.0,
            }
        })
    }

    fn create_error_terminal() -> Self {
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TerminalBounds::default(),
            ZedListener(unbounded().0),
        )));

        Self {
            term,
            sender: None,
            last_content: TerminalContent::default(),
            content_generation: 0,
            content_dirty: false,
            event_loop_task: None,
            terminal_bounds: TerminalBounds::default(),
            selection_phase: SelectionPhase::Ended,
            last_mouse_position: None,
            scroll_accumulator: 0.0,
        }
    }

    fn process_event(&mut self, event: AlacTermEvent, cx: &mut Context<Self>) {
        match event {
            AlacTermEvent::Title(_) => {
                cx.emit(Event::TitleChanged);
            }
            AlacTermEvent::Wakeup => {
                self.content_generation += 1;
                self.content_dirty = true;
                cx.emit(Event::Wakeup);
            }
            AlacTermEvent::Bell => {
                cx.emit(Event::Bell);
            }
            AlacTermEvent::Exit | AlacTermEvent::ChildExit(_) => {
                cx.emit(Event::CloseTerminal);
            }
            _ => {}
        }
    }

    fn update_last_content(&mut self) {
        let term = self.term.lock();
        let content = term.renderable_content();

        let estimated_size = content.display_iter.size_hint().0;
        let cells = &mut self.last_content.cells;
        cells.clear();
        if cells.capacity() < estimated_size {
            cells.reserve(estimated_size);
        }

        cells.extend(content.display_iter.map(|ic| IndexedCell {
            point: ic.point,
            cell: ic.cell.clone(),
        }));

        self.last_content.cursor = content.cursor.point;
        self.last_content.cursor_char = term.grid()[content.cursor.point].c;
        self.last_content.display_offset = content.display_offset;
        self.last_content.mode = content.mode;
        self.last_content.cols = self.terminal_bounds.num_columns();
        self.last_content.rows = self.terminal_bounds.num_lines();
    }

    pub fn sync(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.content_dirty {
            self.update_last_content();
            self.content_dirty = false;
        }
    }

    pub fn write(&mut self, data: &[u8]) {
        if let Some(ref sender) = self.sender {
            let msg = Msg::Input(Cow::Owned(data.to_vec()));
            sender.send(msg);
        }
    }

    pub fn resize(&mut self, bounds: TerminalBounds, cx: &mut Context<Self>) {
        self.terminal_bounds = bounds;

        if let Some(ref sender) = self.sender {
            let window_size: WindowSize = bounds.into();
            sender.send(Msg::Resize(window_size));
        }

        let mut term = self.term.lock();
        term.resize(alacritty_terminal::term::test::TermSize::new(
            bounds.num_columns(),
            bounds.num_lines(),
        ));

        drop(term);
        self.update_last_content();
        cx.emit(Event::Wakeup);
    }

    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    pub fn terminal_bounds(&self) -> TerminalBounds {
        self.terminal_bounds
    }

    pub fn set_terminal_bounds(&mut self, bounds: TerminalBounds) {
        self.terminal_bounds = bounds;
    }

    // Mouse handling
    pub fn mouse_mode(&self, shift: bool) -> bool {
        if shift {
            return false;
        }
        let term = self.term.lock();
        let mode = term.mode();
        mode.intersects(TermMode::MOUSE_MODE)
    }

    pub fn mouse_down(&mut self, e: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        // Convert to local coordinates relative to terminal bounds
        let position = e.position - self.terminal_bounds.bounds.origin;
        let point = grid_point(
            position,
            self.terminal_bounds,
            self.last_content.display_offset,
        );

        if self.mouse_mode(e.modifiers.shift) {
            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, true, self.last_content.mode)
            {
                self.write(&bytes);
            }
        } else {
            match e.button {
                gpui::MouseButton::Left => {
                    let (point, side) = grid_point_and_side(
                        position,
                        self.terminal_bounds,
                        self.last_content.display_offset,
                    );

                    let selection_type = match e.click_count {
                        0 => return,
                        1 => Some(SelectionType::Simple),
                        2 => Some(SelectionType::Semantic),
                        3 => Some(SelectionType::Lines),
                        _ => None,
                    };

                    if let Some(sel_type) = selection_type {
                        let mut term = self.term.lock();
                        let selection = Selection::new(sel_type, point, side);
                        term.selection = Some(selection);
                        self.selection_phase = SelectionPhase::Started;
                        cx.emit(Event::SelectionsChanged);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn mouse_up(&mut self, e: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        // Convert to local coordinates relative to terminal bounds
        let position = e.position - self.terminal_bounds.bounds.origin;

        if self.mouse_mode(e.modifiers.shift) {
            let point = grid_point(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );

            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, false, self.last_content.mode)
            {
                self.write(&bytes);
            }
        } else {
            if e.button == gpui::MouseButton::Left {
                let term = self.term.lock();
                if let Some(text) = term.selection_to_string() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
        }

        self.selection_phase = SelectionPhase::Ended;
    }

    pub fn mouse_drag(&mut self, e: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.mouse_mode(e.modifiers.shift) && self.selection_phase != SelectionPhase::Ended {
            self.selection_phase = SelectionPhase::Selecting;
            // Convert to local coordinates relative to terminal bounds
            let position = e.position - self.terminal_bounds.bounds.origin;
            let (point, side) = grid_point_and_side(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );

            let mut term = self.term.lock();
            if let Some(mut selection) = term.selection.take() {
                selection.update(point, side);
                term.selection = Some(selection);
                cx.emit(Event::SelectionsChanged);
            }
        }
    }

    pub fn mouse_move(&mut self, e: &gpui::MouseMoveEvent, _cx: &mut Context<Self>) {
        // Convert to local coordinates relative to terminal bounds
        let position = e.position - self.terminal_bounds.bounds.origin;
        self.last_mouse_position = Some(position);

        if self.mouse_mode(e.modifiers.shift) {
            let (point, _side) = grid_point_and_side(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );

            if let Some(bytes) =
                mouse_moved_report(point, e.pressed_button, e.modifiers, self.last_content.mode)
            {
                self.write(&bytes);
            }
        }
    }

    /// Handles scroll wheel events with smooth scrolling support.
    /// Uses an accumulator to handle trackpad/trackpoint smooth scrolling.
    pub fn scroll_wheel(&mut self, e: &gpui::ScrollWheelEvent, scroll_multiplier: f32) {
        let mouse_mode = self.mouse_mode(e.shift);
        let scroll_multiplier = if mouse_mode { 1.0 } else { scroll_multiplier };

        // Calculate scroll amount in lines, using accumulator for smooth scrolling
        let scroll_lines =
            self.determine_scroll_lines_with_accumulator(e, scroll_multiplier, mouse_mode);

        if scroll_lines == 0 {
            return;
        }

        if mouse_mode {
            // In mouse mode, send mouse events to the application (e.g., neovim)
            // Convert to local coordinates relative to terminal bounds
            let position = e.position - self.terminal_bounds.bounds.origin;
            let point = grid_point(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
            );

            if let Some(scrolls) = scroll_report(point, scroll_lines, e, self.last_content.mode) {
                for scroll in scrolls {
                    self.write(&scroll);
                }
            }
        } else if self
            .last_content
            .mode
            .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !e.shift
        {
            // In alternate screen with alternate scroll enabled (e.g., less, man pages)
            // Send arrow key sequences instead of scrolling the buffer
            self.write(&alt_scroll(scroll_lines));
        } else {
            // Normal mode: scroll the terminal history buffer
            let scroll = AlacScroll::Delta(scroll_lines);
            let mut term = self.term.lock();
            term.scroll_display(scroll);

            // Mark content as dirty so sync() will update last_content
            self.content_dirty = true;

            #[cfg(debug_assertions)]
            eprintln!("Scrolled {} lines, display_offset will update on next sync", scroll_lines);
        }
    }

    /// Determines the number of lines to scroll, using an accumulator for smooth scrolling.
    /// This allows trackpads and trackpoints to accumulate partial scroll amounts
    /// and scroll only when a full line is reached.
    fn determine_scroll_lines_with_accumulator(
        &mut self,
        e: &gpui::ScrollWheelEvent,
        scroll_multiplier: f32,
        mouse_mode: bool,
    ) -> i32 {
        use gpui::ScrollDelta;

        let raw_scroll = match e.delta {
            ScrollDelta::Lines(lines) => lines.y,
            ScrollDelta::Pixels(pixels) => {
                let line_height = self.terminal_bounds.line_height.as_f32();
                if line_height > 0.0 {
                    pixels.y.as_f32() / line_height
                } else {
                    0.0
                }
            }
        };

        #[cfg(debug_assertions)]
        eprintln!(
            "Scroll: raw={:.4}, multiplier={:.1}, accumulator={:.4}",
            raw_scroll, scroll_multiplier, self.scroll_accumulator
        );

        if raw_scroll == 0.0 {
            // Reset accumulator when scrolling stops to prevent "stuck" scroll
            if !mouse_mode {
                self.scroll_accumulator = 0.0;
            }
            return 0;
        }

        let adjusted_scroll = raw_scroll * scroll_multiplier;

        // In mouse mode, don't accumulate - send each event immediately
        if mouse_mode {
            return adjusted_scroll as i32;
        }

        // Accumulate the scroll amount
        self.scroll_accumulator += adjusted_scroll;

        // Use a threshold of 0.5 lines for trackpad responsiveness
        // This makes scrolling more responsive while still being smooth
        let threshold = 0.5_f32;
        let lines = if self.scroll_accumulator.abs() >= threshold {
            let lines = self.scroll_accumulator.trunc() as i32;
            if lines == 0 {
                // If we haven't reached a full line but passed the threshold, scroll 1 line
                self.scroll_accumulator.signum() as i32
            } else {
                lines
            }
        } else {
            0
        };

        // Keep the fractional part for next time, but also apply threshold logic
        if lines != 0 {
            self.scroll_accumulator -= lines as f32;
        }

        #[cfg(debug_assertions)]
        eprintln!("Scroll result: {} lines, remaining accumulator: {:.4}", lines, self.scroll_accumulator);

        lines
    }

    // Selection
    pub fn selection_text(&self) -> Option<String> {
        let term = self.term.lock();
        term.selection_to_string()
    }

    pub fn selection_range(&self) -> Option<alacritty_terminal::selection::SelectionRange> {
        let term = self.term.lock();
        term.selection.as_ref().and_then(|s| s.to_range(&term))
    }

    pub fn is_cell_selected(&self, point: &AlacPoint) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        let point_line = point.line.0;
        let point_col = point.column.0;

        // 确定选择范围的正常顺序
        let (start, end) = if range.start.line <= range.end.line {
            (range.start, range.end)
        } else {
            (range.end, range.start)
        };

        // 检查行范围
        if point_line < start.line.0 || point_line > end.line.0 {
            return false;
        }

        // 同一行的特殊情况
        if start.line == end.line {
            return point_col >= start.column.0 && point_col <= end.column.0;
        }

        // 多行选择的边界检查
        match point_line.cmp(&start.line.0) {
            std::cmp::Ordering::Equal => point_col >= start.column.0,
            std::cmp::Ordering::Greater if point_line == end.line.0 => point_col <= end.column.0,
            _ => true,
        }
    }

    pub fn clear_selection(&mut self) {
        let mut term = self.term.lock();
        term.selection = None;
        self.selection_phase = SelectionPhase::Ended;
    }

    pub fn select_all(&mut self) {
        let mut term = self.term.lock();
        let start = AlacPoint::new(Line(0), Column(0));
        let selection = Selection::new(SelectionType::Lines, start, Side::Left);
        term.selection = Some(selection);
    }

    // Scrolling
    pub fn scroll_line_up(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Delta(-1));
    }

    pub fn scroll_line_down(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Delta(1));
    }

    pub fn scroll_page_up(&mut self) {
        let mut term = self.term.lock();
        let lines = term.screen_lines() as i32;
        term.scroll_display(AlacScroll::Delta(-lines));
    }

    pub fn scroll_page_down(&mut self) {
        let mut term = self.term.lock();
        let lines = term.screen_lines() as i32;
        term.scroll_display(AlacScroll::Delta(lines));
    }

    pub fn scroll_to_top(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Top);
    }

    pub fn scroll_to_bottom(&mut self) {
        let mut term = self.term.lock();
        term.scroll_display(AlacScroll::Bottom);
    }

    // Copy/Paste
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let term = self.term.lock();
        if let Some(text) = term.selection_to_string() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            let text = item.text().unwrap_or_default();
            self.write(text.as_bytes());
        }
    }

    // Clear
    pub fn clear(&mut self) {
        use alacritty_terminal::vte::ansi::ClearMode;
        let mut term = self.term.lock();
        term.clear_screen(ClearMode::All);
    }
}

impl EventEmitter<Event> for Terminal {}
