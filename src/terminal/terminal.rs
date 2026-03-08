//! Terminal core implementation using alacritty_terminal's EventLoop
//!
//! Based on Zed's terminal implementation - uses background event processing

use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{Event as AlacTermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode, cell::Cell};
use alacritty_terminal::tty::{self, Options as TtyOptions};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Pixels, Point, Task};

use super::event::Event;
use super::mappings::mouse::{
    alt_scroll, grid_point, grid_point_and_side, mouse_button_report, mouse_moved_report,
    scroll_report,
};

/// Terminal content for rendering - cached like Zed
#[derive(Clone, Debug)]
pub struct TerminalContent {
    /// Cells with styling
    pub cells: Vec<IndexedCell>,
    /// Cursor position
    pub cursor: AlacPoint,
    /// Cursor character
    pub cursor_char: char,
    /// Display offset for scrolling
    pub display_offset: usize,
    /// Terminal mode
    pub mode: alacritty_terminal::term::TermMode,
    /// Terminal dimensions
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

/// A styled cell with its grid position - matches alacritty's IndexedCell
#[derive(Clone, Debug)]
pub struct IndexedCell {
    pub point: AlacPoint,
    pub cell: Cell,
}

/// Event listener that forwards alacritty events to GPUI using futures channel
#[derive(Clone)]
pub struct TerminalEventListener(pub UnboundedSender<AlacTermEvent>);

impl EventListener for TerminalEventListener {
    fn send_event(&self, event: AlacTermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

/// Terminal bounds for dimensions
#[derive(Clone, Copy, Debug)]
pub struct TerminalBounds {
    pub num_cols: usize,
    pub num_lines: usize,
}

impl TerminalBounds {
    pub fn new(num_cols: usize, num_lines: usize) -> Self {
        Self {
            num_cols,
            num_lines,
        }
    }
}

impl TerminalBounds {
    /// Get the last column index
    pub fn last_column(&self) -> Column {
        Column(self.num_cols.saturating_sub(1))
    }

    /// Get the bottommost line index
    pub fn bottommost_line(&self) -> Line {
        Line(self.num_lines.saturating_sub(1) as i32)
    }
}

impl Dimensions for TerminalBounds {
    fn total_lines(&self) -> usize {
        self.num_lines
    }

    fn screen_lines(&self) -> usize {
        self.num_lines
    }

    fn columns(&self) -> usize {
        self.num_cols
    }
}

impl From<TerminalBounds> for WindowSize {
    fn from(val: TerminalBounds) -> Self {
        WindowSize {
            num_lines: val.num_lines as u16,
            num_cols: val.num_cols as u16,
            cell_width: 9,
            cell_height: 16,
        }
    }
}

/// Selection phase for mouse handling
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionPhase {
    /// No selection in progress
    Ended,
    /// Currently selecting with mouse drag
    Selecting,
    /// Selection was just started
    Started,
}

/// Main terminal struct using alacritty_terminal's EventLoop
pub struct Terminal {
    term: Arc<FairMutex<Term<TerminalEventListener>>>,
    sender: Option<EventLoopSender>,
    size: (usize, usize),
    title: Option<String>,
    working_directory: Option<String>,
    /// Receiver for alacritty events
    events_rx: Option<UnboundedReceiver<AlacTermEvent>>,
    /// Cached content for rendering - updated by background task
    pub last_content: TerminalContent,
    /// Content generation counter for cache invalidation
    content_generation: u64,
    /// Dirty flag - set when terminal content has changed but not yet rendered
    content_dirty: bool,
    /// Background event loop task
    event_loop_task: Option<Task<()>>,
    /// Font size for rendering
    font_size: gpui::Pixels,
    /// Terminal bounds for mouse coordinate conversion
    terminal_bounds: TerminalBounds,
    /// Current selection phase
    selection_phase: SelectionPhase,
    /// Last mouse position for hyperlink detection
    last_mouse_position: Option<Point<Pixels>>,
    /// Cell width for mouse coordinate conversion
    cell_width: gpui::Pixels,
    /// Line height for mouse coordinate conversion
    line_height: gpui::Pixels,
}

impl Terminal {
    /// Create a new terminal with EventLoop and background event processing
    pub fn new(cx: &mut App, size: (usize, usize)) -> Entity<Self> {
        // Create event channel using futures (like Zed)
        let (events_tx, events_rx) = unbounded();

        // Create event listener
        let listener = TerminalEventListener(events_tx.clone());

        // Create PTY options
        let pty_options = TtyOptions {
            shell: Some(tty::Shell::new(
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
                vec![],
            )),
            working_directory: std::env::current_dir().ok(),
            ..Default::default()
        };

        // Create PTY
        let pty = match tty::new(&pty_options, TerminalBounds::new(size.0, size.1).into(), 0) {
            Ok(pty) => pty,
            Err(e) => {
                eprintln!("Failed to create PTY: {}", e);
                let config = Config::default();
                let term = Arc::new(FairMutex::new(Term::new(
                    config,
                    &TerminalBounds::new(size.0, size.1),
                    listener.clone(),
                )));

                return cx.new(|_cx| Self {
                    term,
                    sender: None,
                    size,
                    title: None,
                    working_directory: None,
                    events_rx: Some(events_rx),
                    last_content: TerminalContent::default(),
                    content_generation: 0,
                    content_dirty: false,
                    event_loop_task: None,
                    font_size: gpui::px(13.0),
                    terminal_bounds: TerminalBounds::new(size.0, size.1),
                    selection_phase: SelectionPhase::Ended,
                    last_mouse_position: None,
                    cell_width: gpui::px(9.0),
                    line_height: gpui::px(16.0),
                });
            }
        };

        let mut config = Config::default();
        config.scrolling_history = 1000;
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &TerminalBounds::new(size.0, size.1),
            listener.clone(),
        )));

        let event_loop = match EventLoop::new(term.clone(), listener, pty, false, false) {
            Ok(event_loop) => event_loop,
            Err(e) => {
                eprintln!("Failed to create EventLoop: {}", e);
                return cx.new(|_cx| Self {
                    term: term.clone(),
                    sender: None,
                    size,
                    title: None,
                    working_directory: None,
                    events_rx: Some(events_rx),
                    last_content: TerminalContent::default(),
                    content_generation: 0,
                    content_dirty: false,
                    event_loop_task: None,
                    font_size: gpui::px(13.0),
                    terminal_bounds: TerminalBounds::new(size.0, size.1),
                    selection_phase: SelectionPhase::Ended,
                    last_mouse_position: None,
                    cell_width: gpui::px(9.0),
                    line_height: gpui::px(16.0),
                });
            }
        };

        let notifier = event_loop.channel();
        let _ = event_loop.spawn();

        cx.new(|cx| {
            let term_clone = term.clone();

            // Start background event processing task (like Zed)
            let event_loop_task = cx.spawn(async move |this, cx| {
                let mut events_rx = events_rx;
                let mut wakeup_count: u64 = 0;

                while let Some(event) = events_rx.next().await {
                    // Process the first event immediately for low latency
                    let _ = this.update(cx, |terminal: &mut Self, cx| {
                        terminal.process_event(event, cx);
                    });

                    // Collect additional events with a timeout
                    let deadline = std::time::Instant::now() + Duration::from_millis(4);
                    let mut events = Vec::new();
                    let mut wakeup = false;

                    // Try to collect more events within the time window
                    loop {
                        match events_rx.try_next() {
                            Ok(Some(event)) => {
                                if matches!(event, AlacTermEvent::Wakeup) {
                                    wakeup = true;
                                    wakeup_count += 1;
                                    if wakeup_count % 100 == 0 {
                                        eprintln!(
                                            "[Terminal] Wakeup events processed: {}",
                                            wakeup_count
                                        );
                                    }
                                } else {
                                    events.push(event);
                                }
                                if events.len() > 100 {
                                    eprintln!("[Terminal] Events buffer full: {}", events.len());
                                    break;
                                }
                                // Check if we've exceeded the deadline
                                if std::time::Instant::now() >= deadline {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }

                    if !events.is_empty() || wakeup {
                        let _ = this.update(cx, |terminal: &mut Self, cx| {
                            if wakeup {
                                terminal.process_event(AlacTermEvent::Wakeup, cx);
                            }
                            for event in events {
                                terminal.process_event(event, cx);
                            }
                        });
                    }

                    // Yield to allow other tasks to run
                    smol::future::yield_now().await;
                }
            });

            Self {
                term: term_clone,
                sender: Some(notifier),
                size,
                title: None,
                working_directory: None,
                events_rx: None, // Moved to background task
                last_content: TerminalContent::default(),
                content_generation: 0,
                content_dirty: false,
                event_loop_task: Some(event_loop_task),
                font_size: gpui::px(13.0),
                terminal_bounds: TerminalBounds::new(size.0, size.1),
                selection_phase: SelectionPhase::Ended,
                last_mouse_position: None,
                cell_width: gpui::px(9.0),
                line_height: gpui::px(16.0),
            }
        })
    }

    /// Process a single event - called from background task
    fn process_event(&mut self, event: AlacTermEvent, cx: &mut Context<Self>) {
        match event {
            AlacTermEvent::Title(title) => {
                let title_clone = title.clone();
                self.title = Some(title);
                cx.emit(Event::TitleChanged(title_clone));
            }
            AlacTermEvent::Wakeup => {
                self.content_generation += 1;
                self.content_dirty = true;
                cx.emit(Event::ContentChanged);
            }
            AlacTermEvent::Bell => {
                cx.emit(Event::Bell);
            }
            AlacTermEvent::Exit => {
                cx.emit(Event::Exited(0));
            }
            AlacTermEvent::ChildExit(exit_code) => {
                cx.emit(Event::ExitedWithError(exit_code.to_string()));
            }
            _ => {}
        }
    }

    /// Update last_content from terminal - called when content changes
    fn update_last_content(&mut self) {
        let term = self.term.lock();
        let content = term.renderable_content();

        let estimated_size = content.display_iter.size_hint().0;

        // Reuse existing vector capacity if possible
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
        self.last_content.cols = self.size.0;
        self.last_content.rows = self.size.1;

        let cell_count = cells.len();
        eprintln!(
            "[Terminal] update_last_content: cells={}, cols={}, rows={}",
            cell_count, self.size.0, self.size.1
        );
    }

    /// Get content generation counter
    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    /// Get last_content for rendering - fast, no allocation
    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    /// Sync terminal - called from prepaint
    /// Only updates content if it has changed (dirty flag is set)
    pub fn sync(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.content_dirty {
            self.update_last_content();
            self.content_dirty = false;
        }
    }

    /// Write data to terminal (sends to PTY)
    pub fn write(&mut self, data: &[u8]) {
        if let Some(ref sender) = self.sender {
            let msg = Msg::Input(std::borrow::Cow::Owned(data.to_vec()));
            let _ = sender.send(msg);
        }
    }

    /// Resize terminal
    pub fn resize(&mut self, cols: usize, rows: usize, cx: &mut Context<Self>) {
        self.size = (cols, rows);

        if let Some(ref sender) = self.sender {
            let window_size = WindowSize {
                num_lines: rows as u16,
                num_cols: cols as u16,
                cell_width: 9,
                cell_height: 16,
            };
            let _ = sender.send(Msg::Resize(window_size));
        }

        let mut term = self.term.lock();
        term.resize(alacritty_terminal::term::test::TermSize::new(cols, rows));

        // Update cached content after resize
        drop(term);
        self.update_last_content();

        cx.emit(Event::ContentChanged);
    }

    /// Get terminal size
    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// Get title
    pub fn title(&self) -> Option<&String> {
        self.title.as_ref()
    }

    /// Get working directory
    pub fn working_directory(&self) -> Option<&String> {
        self.working_directory.as_ref()
    }

    /// Check if terminal is in mouse mode
    fn mouse_mode(&self, shift: bool) -> bool {
        if shift {
            return false;
        }
        let term = self.term.lock();
        let mode = term.mode();
        mode.intersects(TermMode::MOUSE_MODE)
    }

    /// Check if selection has started
    pub fn selection_started(&self) -> bool {
        self.selection_phase == SelectionPhase::Selecting
    }

    /// Handle mouse down event
    pub fn mouse_down(&mut self, e: &gpui::MouseDownEvent, _cx: &mut Context<Self>) {
        let position = e.position;
        let point = grid_point(
            position,
            self.terminal_bounds,
            self.last_content.display_offset,
            self.cell_width,
            self.line_height,
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
                        self.cell_width,
                        self.line_height,
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
                    }
                }
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                gpui::MouseButton::Middle => {
                    // Paste from primary selection on Linux
                    if let Some(text) = _cx.read_from_primary() {
                        let text = text.text().unwrap_or_default();
                        self.write(text.as_bytes());
                    }
                }
                _ => {}
            }
        }
    }

    /// Handle mouse up event
    pub fn mouse_up(&mut self, e: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        let position = e.position;

        if self.mouse_mode(e.modifiers.shift) {
            let point = grid_point(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
                self.cell_width,
                self.line_height,
            );

            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, false, self.last_content.mode)
            {
                self.write(&bytes);
            }
        } else {
            if e.button == gpui::MouseButton::Left {
                // Copy selection to clipboard
                let term = self.term.lock();
                if let Some(text) = term.selection_to_string() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                }
            }
        }

        self.selection_phase = SelectionPhase::Ended;
    }

    /// Handle mouse move event
    pub fn mouse_move(&mut self, e: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        let position = e.position;
        self.last_mouse_position = Some(position);

        if self.mouse_mode(e.modifiers.shift) {
            let (point, side) = grid_point_and_side(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
                self.cell_width,
                self.line_height,
            );

            if let Some(bytes) =
                mouse_moved_report(point, e.pressed_button, e.modifiers, self.last_content.mode)
            {
                self.write(&bytes);
            }
        } else if e.pressed_button.is_some() && self.selection_phase != SelectionPhase::Ended {
            self.selection_phase = SelectionPhase::Selecting;
            let (point, side) = grid_point_and_side(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
                self.cell_width,
                self.line_height,
            );

            let mut term = self.term.lock();
            if let Some(mut selection) = term.selection.take() {
                selection.update(point, side);
                term.selection = Some(selection);
                cx.notify();
            }
        }
    }

    /// Handle mouse drag event
    pub fn mouse_drag(
        &mut self,
        e: &gpui::MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if !self.mouse_mode(e.modifiers.shift) {
            self.selection_phase = SelectionPhase::Selecting;
            let position = e.position;
            let (point, side) = grid_point_and_side(
                position,
                self.terminal_bounds,
                self.last_content.display_offset,
                self.cell_width,
                self.line_height,
            );

            let mut term = self.term.lock();
            if let Some(mut selection) = term.selection.take() {
                selection.update(point, side);
                term.selection = Some(selection);
                cx.notify();
            }
        }
    }

    /// Handle scroll wheel event
    pub fn scroll_wheel(&mut self, e: &gpui::ScrollWheelEvent, scroll_multiplier: f32) {
        let mouse_mode = self.mouse_mode(e.shift);
        let scroll_multiplier = if mouse_mode { 1.0 } else { scroll_multiplier };

        if let Some(scroll_lines) = self.determine_scroll_lines(e, scroll_multiplier) {
            if scroll_lines == 0 {
                return;
            }

            if mouse_mode {
                let point = grid_point(
                    e.position,
                    self.terminal_bounds,
                    self.last_content.display_offset,
                    self.cell_width,
                    self.line_height,
                );

                if let Some(scrolls) = scroll_report(point, scroll_lines, e, self.last_content.mode)
                {
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
                self.write(&alt_scroll(scroll_lines));
            } else {
                let scroll = alacritty_terminal::grid::Scroll::Delta(scroll_lines);
                let mut term = self.term.lock();
                term.scroll_display(scroll);
            }
        }
    }

    /// Determine scroll lines from scroll wheel event
    fn determine_scroll_lines(
        &self,
        e: &gpui::ScrollWheelEvent,
        scroll_multiplier: f32,
    ) -> Option<i32> {
        use gpui::{ScrollDelta, px};

        let lines = match e.delta {
            ScrollDelta::Lines(lines) => lines.y,
            ScrollDelta::Pixels(pixels) => {
                let line_height = self.terminal_bounds.num_lines as f32;
                if line_height > 0.0 {
                    pixels.y / px(line_height)
                } else {
                    0.0
                }
            }
        };

        if lines == 0.0 {
            return None;
        }

        Some((lines * scroll_multiplier) as i32)
    }

    /// Set terminal bounds for mouse coordinate conversion
    pub fn set_terminal_bounds(&mut self, bounds: TerminalBounds) {
        self.terminal_bounds = bounds;
    }

    /// Get current terminal bounds
    pub fn terminal_bounds(&self) -> TerminalBounds {
        self.terminal_bounds
    }

    /// Get selection text
    pub fn selection_text(&self) -> Option<String> {
        let term = self.term.lock();
        term.selection_to_string()
    }

    /// Get selection range for rendering
    pub fn selection_range(&self) -> Option<alacritty_terminal::selection::SelectionRange> {
        let term = self.term.lock();
        term.selection.as_ref().and_then(|s| s.to_range(&term))
    }

    /// Check if a cell is within the selection
    pub fn is_cell_selected(&self, point: &AlacPoint) -> bool {
        if let Some(range) = self.selection_range() {
            // Check if point is within the selection range
            let start = range.start;
            let end = range.end;
            
            // Normalize the range (start should be before end)
            let (start_line, start_col, end_line, end_col) = if start.line < end.line || 
                (start.line == end.line && start.column <= end.column) {
                (start.line.0, start.column.0, end.line.0, end.column.0)
            } else {
                (end.line.0, end.column.0, start.line.0, start.column.0)
            };
            
            let point_line = point.line.0;
            let point_col = point.column.0;
            
            // Check if point is within range
            if point_line < start_line || point_line > end_line {
                return false;
            }
            
            if point_line == start_line && point_line == end_line {
                // Single line selection
                return point_col >= start_col && point_col <= end_col;
            } else if point_line == start_line {
                // First line of multi-line selection
                return point_col >= start_col;
            } else if point_line == end_line {
                // Last line of multi-line selection
                return point_col <= end_col;
            } else {
                // Middle line of multi-line selection
                return true;
            }
        }
        false
    }

    /// Clear selection
    pub fn clear_selection(&mut self) {
        let mut term = self.term.lock();
        term.selection = None;
        self.selection_phase = SelectionPhase::Ended;
    }
}

impl EventEmitter<Event> for Terminal {}
