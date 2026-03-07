//! Terminal core implementation using alacritty_terminal's EventLoop
//!
//! Based on Zed's terminal implementation - uses last_content caching

use std::collections::VecDeque;
use std::sync::Arc;

use alacritty_terminal::event::{Event as AlacTermEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{cell::Cell, Config, Term};
use alacritty_terminal::tty::{self, Options as TtyOptions};
use gpui::{App, Context, Entity, EventEmitter, Hsla, Pixels};

use super::event::Event;

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

/// Event listener that forwards alacritty events to GPUI
#[derive(Clone)]
struct TerminalEventListener(async_channel::Sender<AlacTermEvent>);

impl EventListener for TerminalEventListener {
    fn send_event(&self, event: AlacTermEvent) {
        let _ = self.0.try_send(event);
    }
}

/// Terminal bounds for dimensions
#[derive(Clone, Copy, Debug)]
struct TerminalBounds {
    num_cols: usize,
    num_lines: usize,
}

impl TerminalBounds {
    fn new(num_cols: usize, num_lines: usize) -> Self {
        Self {
            num_cols,
            num_lines,
        }
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

/// Main terminal struct using alacritty_terminal's EventLoop
pub struct Terminal {
    term: Arc<FairMutex<Term<TerminalEventListener>>>,
    sender: Option<EventLoopSender>,
    size: (usize, usize),
    title: Option<String>,
    working_directory: Option<String>,
    event_receiver: async_channel::Receiver<AlacTermEvent>,
    events: VecDeque<AlacTermEvent>,
    /// Cached content for rendering - updated only in process_events
    pub last_content: TerminalContent,
}

impl Terminal {
    /// Create a new terminal with EventLoop
    pub fn new(cx: &mut App, size: (usize, usize)) -> Entity<Self> {
        use gpui::AppContext as _;

        // Create event channel
        let (event_sender, event_receiver) = async_channel::unbounded();

        // Create event listener
        let listener = TerminalEventListener(event_sender.clone());

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
                    event_receiver,
                    events: VecDeque::new(),
                    last_content: TerminalContent::default(),
                });
            }
        };

        let config = Config::default();
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
                    event_receiver,
                    events: VecDeque::new(),
                    last_content: TerminalContent::default(),
                });
            }
        };

        let notifier = event_loop.channel();
        let _ = event_loop.spawn();

        cx.new(|_cx| Self {
            term,
            sender: Some(notifier),
            size,
            title: None,
            working_directory: None,
            event_receiver,
            events: VecDeque::new(),
            last_content: TerminalContent::default(),
        })
    }

    /// Process pending events from alacritty - this is where we update last_content
    pub fn process_events(&mut self, cx: &mut Context<Self>) {
        // Collect events from async channel
        let mut event_count = 0;
        while let Ok(event) = self.event_receiver.try_recv() {
            self.events.push_back(event);
            event_count += 1;
        }

        if event_count > 0 {
            eprintln!(
                "[Terminal Events] Received {} events, queue size: {}",
                event_count,
                self.events.len()
            );
        }

        let mut content_changed = false;
        let mut wakeup_count = 0;

        // Process all pending events
        while let Some(event) = self.events.pop_front() {
            match event {
                AlacTermEvent::Title(title) => {
                    self.title = Some(title);
                }
                AlacTermEvent::Wakeup => {
                    content_changed = true;
                    wakeup_count += 1;
                }
                AlacTermEvent::Bell => {}
                AlacTermEvent::Exit => {
                    cx.emit(Event::Exited(0));
                }
                AlacTermEvent::ChildExit(exit_code) => {
                    cx.emit(Event::ExitedWithError(exit_code.to_string()));
                }
                _ => {}
            }
        }

        // Update cached content if changed - ONLY HERE, not in prepaint
        if content_changed {
            eprintln!(
                "[Terminal Update] Processing {} Wakeup events",
                wakeup_count
            );
            self.update_last_content();
            cx.emit(Event::ContentChanged);
        }
    }

    /// Update last_content from terminal - called only in process_events
    fn update_last_content(&mut self) {
        let term = self.term.lock();
        let content = term.renderable_content();

        // Pre-allocate with estimated size
        let estimated_size = content.display_iter.size_hint().0;
        let mut cells = Vec::with_capacity(estimated_size);

        cells.extend(content.display_iter.map(|ic| IndexedCell {
            point: ic.point,
            cell: ic.cell.clone(),
        }));

        // Log cache size for debugging
        let cells_size = cells.len() * std::mem::size_of::<IndexedCell>();
        let total_size = cells_size + std::mem::size_of::<TerminalContent>();
        eprintln!(
            "[Terminal Cache] cells: {}, size: {} bytes, display_offset: {}, cursor: {:?}",
            cells.len(),
            total_size,
            content.display_offset,
            content.cursor.point
        );

        self.last_content = TerminalContent {
            cells,
            cursor: content.cursor.point,
            cursor_char: term.grid()[content.cursor.point].c,
            display_offset: content.display_offset,
            mode: content.mode,
            cols: self.size.0,
            rows: self.size.1,
        };
    }

    /// Get last_content for rendering - fast, no allocation
    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
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
}

impl EventEmitter<Event> for Terminal {}
