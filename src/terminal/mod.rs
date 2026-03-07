pub mod ansi;
pub mod event;
pub mod mappings;
pub mod pty;
pub mod terminal;

pub use ansi::{AnsiColor, AnsiParser, ParsedOutput, TerminalStyle};
pub use event::{Event, InternalEvent};
pub use terminal::{Terminal, TerminalContent};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};

/// Convert a grid point to a byte offset in the terminal content
pub fn point_to_offset<D: Dimensions>(point: Point, dims: &D) -> usize {
    let line = point.line.0 as usize;
    let col = point.column.0 as usize;
    line * dims.columns() + col
}

/// Convert a byte offset to a grid point
pub fn offset_to_point<D: Dimensions>(offset: usize, dims: &D) -> Point {
    let cols = dims.columns();
    let line = Line((offset / cols) as i32);
    let col = Column((offset % cols) as usize);
    Point::new(line, col)
}
