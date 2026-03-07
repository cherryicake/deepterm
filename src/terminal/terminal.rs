//! Terminal core implementation using alacritty_terminal's EventLoop
//!
//! Based on Zed's terminal implementation - uses background event processing

use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{Event as AlacTermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, cell::Cell};
use alacritty_terminal::tty::{self, Options as TtyOptions};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};

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
}

impl EventEmitter<Event> for Terminal {}
