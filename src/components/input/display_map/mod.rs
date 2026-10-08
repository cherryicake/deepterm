/// Display mapping system for Editor/Input.
///
/// This module implements a layered display mapping architecture:
/// - **WrapMap**: Handles soft-wrapping (buffer → wrap rows)
/// - **FoldMap**: Handles folding (wrap rows → display rows)
/// - **DisplayMap**: Public facade for Editor/Input
///
/// The goal is to provide a clean, unified API where Editor only needs to know
/// about `BufferPoint ↔ DisplayPoint` mapping, without worrying about internal wrap/fold complexity.
mod display_map;
mod fold_map;
mod folding;
mod text_wrapper;
mod wrap_map;

// Re-export public API
pub use self::display_map::DisplayMap;
pub use self::display_map::{GitInsertedLine, GitInsertion};
pub(crate) use self::text_wrapper::LineLayout;

// Re-export fold model (zed-style)
pub use fold_map::Fold;
pub use folding::{Crease, crease_for_buffer_row, starts_indent};

/// Position in the buffer (logical text).
///
/// - `line`: 0-based logical line number (split by `\n`)
/// - `col`: 0-based column offset (byte offset)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferPoint {
    pub line: usize,
    pub col: usize,
}

impl BufferPoint {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Final display position (after soft-wrapping and folding).
///
/// - `row`: 0-based display row (final visible row)
/// - `col`: 0-based display column
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayPoint {
    pub row: usize,
    pub col: usize,
}

impl DisplayPoint {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}
