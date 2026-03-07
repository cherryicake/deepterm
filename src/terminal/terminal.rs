//! Terminal core implementation using alacritty_terminal
//!
//! Uses alacritty_terminal's VTE implementation for proper ANSI sequence handling

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::Arc;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use gpui::{App, Context, Entity, EventEmitter, Hsla};

use super::event::{Event, InternalEvent};
use super::pty::{self, Pty, PtyConfig};

/// Terminal content for rendering
#[derive(Clone, Debug)]
pub struct TerminalContent {
    /// Lines of text (simplified for rendering)
    pub lines: Vec<String>,
    /// Cursor position (col, row)
    pub cursor: (usize, usize),
    /// Terminal dimensions
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
    /// Create a new indexed cell with default styling (Ghostty dankcolors theme)
    pub fn new(col: usize, row: usize, ch: char) -> Self {
        Self {
            col,
            row,
            ch,
            fg_color: gpui::rgb(0xcdd6f4).into(), // foreground color
            bg_color: None,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

/// Main terminal struct
pub struct Terminal {
    /// PTY handle - wrapped in Arc<Mutex<>> for thread-safe sharing
    pty: Option<Arc<parking_lot::Mutex<Box<dyn Pty>>>>,
    /// Event queue
    events: Arc<parking_lot::Mutex<VecDeque<InternalEvent>>>,
    /// Terminal size
    size: (usize, usize),
    /// alacritty terminal instance
    term: Arc<parking_lot::Mutex<Term<VoidListener>>>,
    /// VTE processor
    processor: Processor<StdSyncHandler>,
    /// Title
    title: Option<String>,
    /// Working directory
    working_directory: Option<String>,
    /// PTY reader thread
    _reader_thread: Option<std::thread::JoinHandle<()>>,
}

impl Terminal {
    /// Create a new terminal
    pub fn new(cx: &mut App, size: (usize, usize)) -> Entity<Self> {
        use gpui::AppContext as _;

        // Try to create PTY, but fallback to None if it fails
        let pty_config = PtyConfig {
            size,
            ..Default::default()
        };

        let pty = pty::create_pty(pty_config).ok();
        let pty = pty.map(|p| Arc::new(parking_lot::Mutex::new(p)));
        let events = Arc::new(parking_lot::Mutex::new(VecDeque::new()));

        // Create alacritty terminal
        let config = Config::default();
        let term = Arc::new(parking_lot::Mutex::new(Term::new(
            config,
            &TermSize::new(size.0, size.1),
            VoidListener,
        )));

        // Start reader thread if PTY was created
        let _reader_thread = if let Some(pty_arc) = pty.clone() {
            let events_clone = events.clone();
            Some(std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    // Try to read from PTY
                    let pty_guard = pty_arc.lock();
                    match pty_guard.try_read(&mut buf) {
                        Ok(0) => {
                            // No data available, release lock and sleep
                            drop(pty_guard);
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Ok(n) => {
                            let data = buf[..n].to_vec();
                            drop(pty_guard);
                            events_clone.lock().push_back(InternalEvent::PtyData(data));
                        }
                        Err(e) => {
                            drop(pty_guard);
                            if e.kind() == io::ErrorKind::WouldBlock {
                                // No data available, sleep briefly
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            } else {
                                eprintln!("PTY read error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }))
        } else {
            None
        };

        cx.new(|_cx| Self {
            pty,
            events,
            size,
            term: term.clone(),
            processor: Processor::new(),
            title: None,
            working_directory: None,
            _reader_thread,
        })
    }

    /// Process pending events
    pub fn process_events(&mut self, cx: &mut Context<Self>) {
        // Collect all events first to avoid borrow issues
        let events_to_process: Vec<InternalEvent> = {
            let mut events = self.events.lock();
            std::iter::from_fn(|| events.pop_front()).collect()
        };

        for event in events_to_process {
            match event {
                InternalEvent::PtyData(data) => {
                    // Process PTY data using alacritty's VTE processor
                    self.process_pty_data(&data);
                    cx.emit(Event::ContentChanged);
                }
                InternalEvent::Write(data) => {
                    // Write to PTY
                    if let Some(ref pty_arc) = self.pty {
                        let pty = pty_arc.lock();
                        let _ = pty.try_write(&data);
                    }
                }
                InternalEvent::Resize { cols, rows } => {
                    self.resize(cols, rows, cx);
                }
                InternalEvent::Scroll(_lines) => {
                    cx.emit(Event::ContentChanged);
                }
                InternalEvent::ClearScrollback => {
                    // Clear scrollback not implemented with alacritty terminal
                    cx.emit(Event::ContentChanged);
                }
            }
        }
    }

    /// Process PTY data using alacritty's VTE processor
    fn process_pty_data(&mut self, data: &[u8]) {
        // Convert LF to CRLF for Windows compatibility
        let mut converted = Vec::with_capacity(data.len());
        let mut prev_byte = 0u8;
        for &byte in data {
            if byte == b'\n' && prev_byte != b'\r' {
                converted.push(b'\r');
            }
            converted.push(byte);
            prev_byte = byte;
        }

        let mut term = self.term.lock();
        for byte in &converted {
            self.processor.advance(&mut *term, *byte);
        }
    }

    /// Write data to terminal
    pub fn write(&mut self, data: &[u8]) {
        if let Some(ref pty_arc) = self.pty {
            let pty = pty_arc.lock();
            let _ = pty.try_write(data);
        }
    }

    /// Resize terminal
    pub fn resize(&mut self, cols: usize, rows: usize, cx: &mut Context<Self>) {
        self.size = (cols, rows);

        if let Some(ref pty_arc) = self.pty {
            let pty = pty_arc.lock();
            let _ = pty.resize(cols, rows);
        }

        // Resize alacritty terminal
        let mut term = self.term.lock();
        term.resize(TermSize::new(cols, rows));

        cx.emit(Event::ContentChanged);
    }

    /// Get renderable content
    pub fn renderable_content(&self) -> TerminalContent {
        let term = self.term.lock();
        let mut lines = Vec::new();

        // Get visible lines from the terminal grid
        let grid = term.grid();
        let screen_lines = grid.screen_lines();
        let columns = grid.columns();

        // Get the viewport (visible area) - use display_offset directly
        let display_offset = grid.display_offset();

        // Iterate through visible screen lines
        for row in 0..screen_lines {
            // Calculate actual line index in the grid
            let line_idx = (display_offset + row) as i32;
            let line_num = alacritty_terminal::index::Line(line_idx);

            // Check if line index is valid
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

        // Get cursor position
        let cursor = &grid.cursor;
        let cursor_pos = (cursor.point.column.0, cursor.point.line.0 as usize);

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

        // Get visible lines from the terminal grid
        let grid = term.grid();
        let screen_lines = grid.screen_lines();
        let columns = grid.columns();

        // Get the viewport (visible area) - use display_offset directly
        let display_offset = grid.display_offset();

        // Iterate through visible screen lines
        for row in 0..screen_lines {
            // Calculate actual line index in the grid
            let line_idx = (display_offset + row) as i32;
            let line_num = alacritty_terminal::index::Line(line_idx);

            // Check if line index is valid
            if line_idx < 0 || line_idx >= grid.total_lines() as i32 {
                continue;
            }

            let grid_row = &grid[line_num];

            for col in 0..columns {
                let cell = &grid_row[Column(col)];
                let ch = cell.c;

                // Skip null cells but keep spaces for proper layout
                if ch == '\0' {
                    continue;
                }

                // Convert alacritty colors to GPUI Hsla
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

/// Convert alacritty terminal color to GPUI Hsla
/// Uses Ghostty dankcolors theme
fn convert_alacritty_color(color: &alacritty_terminal::vte::ansi::Color) -> Hsla {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    match color {
        Color::Named(named) => match named {
            // Standard 16 colors - dankcolors theme
            NamedColor::Black => gpui::rgb(0x181825).into(), // palette 0
            NamedColor::Red => gpui::rgb(0xff728c).into(),   // palette 1
            NamedColor::Green => gpui::rgb(0x7bf797).into(), // palette 2
            NamedColor::Yellow => gpui::rgb(0xffdb72).into(), // palette 3
            NamedColor::Blue => gpui::rgb(0xb88eea).into(),  // palette 4
            NamedColor::Magenta => gpui::rgb(0x4c2d72).into(), // palette 5
            NamedColor::Cyan => gpui::rgb(0xcba6f7).into(),  // palette 6
            NamedColor::White => gpui::rgb(0xf6efff).into(), // palette 7
            NamedColor::BrightBlack => gpui::rgb(0x9994a0).into(), // palette 8
            NamedColor::BrightRed => gpui::rgb(0xff9fb1).into(), // palette 9
            NamedColor::BrightGreen => gpui::rgb(0xa5ffba).into(), // palette 10
            NamedColor::BrightYellow => gpui::rgb(0xffe8a5).into(), // palette 11
            NamedColor::BrightBlue => gpui::rgb(0xd8b7ff).into(), // palette 12
            NamedColor::BrightMagenta => gpui::rgb(0xdfc4ff).into(), // palette 13
            NamedColor::BrightCyan => gpui::rgb(0xead9ff).into(), // palette 14
            NamedColor::BrightWhite => gpui::rgb(0xfbf8ff).into(), // palette 15
            NamedColor::Foreground => gpui::rgb(0xcdd6f4).into(), // foreground
            NamedColor::Background => gpui::rgb(0x1e1e2e).into(), // background
            NamedColor::Cursor => gpui::rgb(0xcba6f7).into(), // cursor-color
            _ => gpui::rgb(0xcdd6f4).into(),
        },
        Color::Spec(rgb) => {
            gpui::rgb((rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32).into()
        }
        Color::Indexed(idx) => {
            // ANSI 256 color palette - basic implementation
            match idx {
                0..=15 => {
                    // Standard 16 colors
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
                    // 216 colors (6x6x6 cube)
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
                    // Grayscale
                    let gray = 8 + (*idx - 232) as u8 * 10;
                    gpui::rgb((gray as u32) << 16 | (gray as u32) << 8 | gray as u32).into()
                }
                _ => gpui::rgb(0xc0c0c0).into(),
            }
        }
    }
}
