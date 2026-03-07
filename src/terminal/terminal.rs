//! Terminal core implementation using alacritty_terminal's EventLoop
//!
//! Based on Zed's terminal implementation

use std::collections::VecDeque;
use std::sync::Arc;

use alacritty_terminal::event::{Event as AlacTermEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Column;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as TtyOptions};
use gpui::{App, Context, Entity, EventEmitter, Hsla, Pixels};

use super::event::Event;

/// Terminal content for rendering
#[derive(Clone, Debug)]
pub struct TerminalContent {
    pub lines: Vec<String>,
    pub cursor: (usize, usize),
    pub cols: usize,
    pub rows: usize,
}

/// A styled cell with its grid position
#[derive(Clone, Debug)]
pub struct IndexedCell {
    pub col: usize,
    pub row: usize,
    pub ch: char,
    pub fg_color: Hsla,
    pub bg_color: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl IndexedCell {
    pub fn new(col: usize, row: usize, ch: char) -> Self {
        Self {
            col,
            row,
            ch,
            fg_color: gpui::rgb(0xcdd6f4).into(),
            bg_color: None,
            bold: false,
            italic: false,
            underline: false,
        }
    }
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
    cell_width: Pixels,
    line_height: Pixels,
    num_cols: usize,
    num_lines: usize,
}

impl TerminalBounds {
    fn new(num_cols: usize, num_lines: usize) -> Self {
        Self {
            cell_width: gpui::px(9.0),
            line_height: gpui::px(16.0),
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
    /// Content generation counter for cache invalidation
    content_generation: u64,
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
        let pty = match tty::new(
            &pty_options,
            TerminalBounds::new(size.0, size.1).into(),
            0, // window_id
        ) {
            Ok(pty) => pty,
            Err(e) => {
                eprintln!("Failed to create PTY: {}", e);
                // Create a dummy terminal without PTY
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
                    content_generation: 0,
                });
            }
        };

        // Create alacritty terminal config
        let config = Config::default();

        // Create alacritty terminal
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &TerminalBounds::new(size.0, size.1),
            listener.clone(),
        )));

        // Create EventLoop
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
                    content_generation: 0,
                });
            }
        };

        // Get notifier for sending input
        let notifier = event_loop.channel();

        // Spawn EventLoop in background
        let _ = event_loop.spawn();

        cx.new(|_cx| Self {
            term,
            sender: Some(notifier),
            size,
            title: None,
            working_directory: None,
            event_receiver,
            events: VecDeque::new(),
            content_generation: 0,
        })
    }

    /// Process pending events from alacritty
    pub fn process_events(&mut self, cx: &mut Context<Self>) {
        // Collect events from async channel
        while let Ok(event) = self.event_receiver.try_recv() {
            self.events.push_back(event);
        }

        let mut content_changed = false;

        // Process all pending events
        while let Some(event) = self.events.pop_front() {
            match event {
                AlacTermEvent::Title(title) => {
                    self.title = Some(title);
                }
                AlacTermEvent::Wakeup => {
                    content_changed = true;
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

        if content_changed {
            self.content_generation += 1;
            cx.emit(Event::ContentChanged);
        }
    }

    /// Get content generation counter
    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    /// Write data to terminal (sends to PTY)
    pub fn write(&mut self, data: &[u8]) {
        if let Some(ref notifier) = self.sender {
            let msg = Msg::Input(std::borrow::Cow::Owned(data.to_vec()));
            let _ = notifier.send(msg);
        }
    }

    /// Resize terminal
    pub fn resize(&mut self, cols: usize, rows: usize, cx: &mut Context<Self>) {
        self.size = (cols, rows);

        if let Some(ref notifier) = self.sender {
            let window_size = WindowSize {
                num_lines: rows as u16,
                num_cols: cols as u16,
                cell_width: 9,
                cell_height: 16,
            };
            let _ = notifier.send(Msg::Resize(window_size));
        }

        let mut term = self.term.lock();
        term.resize(alacritty_terminal::term::test::TermSize::new(cols, rows));

        cx.emit(Event::ContentChanged);
    }

    /// Get renderable content
    pub fn renderable_content(&self) -> TerminalContent {
        let term = self.term.lock();
        let mut lines = Vec::new();

        let grid = term.grid();
        let screen_lines = grid.screen_lines();
        let columns = grid.columns();
        let display_offset = grid.display_offset();

        for row in 0..screen_lines {
            let line_idx = (display_offset + row) as i32;
            let line_num = alacritty_terminal::index::Line(line_idx);

            if line_idx < 0 || line_idx >= grid.total_lines() as i32 {
                lines.push(String::new());
                continue;
            }

            let grid_row = &grid[line_num];
            let mut line_text = String::new();

            for col in 0..columns {
                let cell = &grid_row[Column(col)];
                let ch = cell.c;
                if ch != '\0' {
                    line_text.push(ch);
                } else {
                    line_text.push(' ');
                }
            }

            lines.push(line_text);
        }

        let cursor = &grid.cursor;
        let cursor_line = (cursor.point.line.0 + display_offset as i32) as usize;
        let cursor_pos = (cursor.point.column.0, cursor_line);

        TerminalContent {
            lines,
            cursor: cursor_pos,
            cols: self.size.0,
            rows: self.size.1,
        }
    }

    /// Get renderable cells with styling information
    pub fn renderable_cells(&self) -> Vec<IndexedCell> {
        let term = self.term.lock();
        let mut cells = Vec::new();

        let grid = term.grid();
        let screen_lines = grid.screen_lines();
        let columns = grid.columns();
        let display_offset = grid.display_offset();

        for row in 0..screen_lines {
            let line_idx = (display_offset + row) as i32;
            let line_num = alacritty_terminal::index::Line(line_idx);

            if line_idx < 0 || line_idx >= grid.total_lines() as i32 {
                continue;
            }

            let grid_row = &grid[line_num];

            for col in 0..columns {
                let cell = &grid_row[Column(col)];
                let ch = cell.c;

                if ch == '\0' {
                    continue;
                }

                let fg_color = convert_alacritty_color(&cell.fg);
                let bg_color = if cell.bg
                    == alacritty_terminal::vte::ansi::Color::Named(
                        alacritty_terminal::vte::ansi::NamedColor::Background,
                    ) {
                    None
                } else {
                    Some(convert_alacritty_color(&cell.bg))
                };

                let flags = cell.flags;

                cells.push(IndexedCell {
                    col,
                    row,
                    ch,
                    fg_color,
                    bg_color,
                    bold: flags.contains(alacritty_terminal::term::cell::Flags::BOLD),
                    italic: flags.contains(alacritty_terminal::term::cell::Flags::ITALIC),
                    underline: flags.contains(alacritty_terminal::term::cell::Flags::UNDERLINE),
                });
            }
        }

        cells
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    pub fn title(&self) -> Option<&String> {
        self.title.as_ref()
    }

    pub fn working_directory(&self) -> Option<&String> {
        self.working_directory.as_ref()
    }
}

impl EventEmitter<Event> for Terminal {}

fn convert_alacritty_color(color: &alacritty_terminal::vte::ansi::Color) -> Hsla {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    match color {
        Color::Named(named) => match named {
            NamedColor::Black => gpui::rgb(0x181825).into(),
            NamedColor::Red => gpui::rgb(0xff728c).into(),
            NamedColor::Green => gpui::rgb(0x7bf797).into(),
            NamedColor::Yellow => gpui::rgb(0xffdb72).into(),
            NamedColor::Blue => gpui::rgb(0xb88eea).into(),
            NamedColor::Magenta => gpui::rgb(0x4c2d72).into(),
            NamedColor::Cyan => gpui::rgb(0xcba6f7).into(),
            NamedColor::White => gpui::rgb(0xf6efff).into(),
            NamedColor::BrightBlack => gpui::rgb(0x9994a0).into(),
            NamedColor::BrightRed => gpui::rgb(0xff9fb1).into(),
            NamedColor::BrightGreen => gpui::rgb(0xa5ffba).into(),
            NamedColor::BrightYellow => gpui::rgb(0xffe8a5).into(),
            NamedColor::BrightBlue => gpui::rgb(0xd8b7ff).into(),
            NamedColor::BrightMagenta => gpui::rgb(0xdfc4ff).into(),
            NamedColor::BrightCyan => gpui::rgb(0xead9ff).into(),
            NamedColor::BrightWhite => gpui::rgb(0xfbf8ff).into(),
            NamedColor::Foreground => gpui::rgb(0xcdd6f4).into(),
            NamedColor::Background => gpui::rgb(0x1e1e2e).into(),
            NamedColor::Cursor => gpui::rgb(0xcba6f7).into(),
            _ => gpui::rgb(0xcdd6f4).into(),
        },
        Color::Spec(rgb) => {
            gpui::rgb((rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32).into()
        }
        Color::Indexed(idx) => match idx {
            0..=15 => {
                let named = match idx {
                    0 => NamedColor::Black,
                    1 => NamedColor::Red,
                    2 => NamedColor::Green,
                    3 => NamedColor::Yellow,
                    4 => NamedColor::Blue,
                    5 => NamedColor::Magenta,
                    6 => NamedColor::Cyan,
                    7 => NamedColor::White,
                    8 => NamedColor::BrightBlack,
                    9 => NamedColor::BrightRed,
                    10 => NamedColor::BrightGreen,
                    11 => NamedColor::BrightYellow,
                    12 => NamedColor::BrightBlue,
                    13 => NamedColor::BrightMagenta,
                    14 => NamedColor::BrightCyan,
                    15 => NamedColor::BrightWhite,
                    _ => NamedColor::Foreground,
                };
                convert_alacritty_color(&Color::Named(named))
            }
            16..=231 => {
                let idx = *idx - 16;
                let r = (idx / 36) as u8;
                let g = ((idx % 36) / 6) as u8;
                let b = (idx % 6) as u8;
                let r = if r == 0 { 0 } else { r * 40 + 55 };
                let g = if g == 0 { 0 } else { g * 40 + 55 };
                let b = if b == 0 { 0 } else { b * 40 + 55 };
                gpui::rgb((r as u32) << 16 | (g as u32) << 8 | b as u32).into()
            }
            232..=255 => {
                let gray = 8 + (*idx - 232) as u8 * 10;
                gpui::rgb((gray as u32) << 16 | (gray as u32) << 8 | gray as u32).into()
            }
            _ => gpui::rgb(0xc0c0c0).into(),
        },
    }
}
