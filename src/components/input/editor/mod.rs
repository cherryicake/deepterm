pub const MASK_CHAR: char = '•';

pub mod blink_cursor;
pub mod change;
pub mod cursor;
pub mod display_map;
pub mod element;
pub mod indent;
pub mod lsp;
pub mod mask_pattern;
pub mod mode;
pub mod movement;
pub mod number_input;
pub mod popovers;
pub mod rope_ext;
pub mod search;
pub mod selection;
pub mod state;

pub use blink_cursor::*;
pub use display_map::{BufferPoint, DisplayMap, DisplayPoint, FoldRange};
pub use indent::TabSize;
pub use rope_ext::{InputEdit, Point, RopeExt, RopeLines};
pub use ropey::Rope;
pub use selection::Selection;
pub use state::*;

pub use lsp_types::Position;
