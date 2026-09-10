pub const MASK_CHAR: char = '•';

pub(crate) mod change;
pub mod history;

pub mod blink_cursor;
pub mod cursor;
pub mod display_map;
pub mod element;
pub mod indent;
pub mod input;
pub mod mode;
pub mod movement;
pub mod rope_ext;
pub mod search;
pub mod selection;
pub mod state;

pub use cursor::{CursorLayout, CursorShape};
pub use display_map::{BufferPoint, Crease, DisplayMap, DisplayPoint, Fold};
pub use indent::TabSize;
pub use input::EditorInput;
pub use rope_ext::{InputEdit, Point, Position, RopeExt, RopeLines};
pub use ropey::Rope;
pub use selection::Selection;
pub use state::*;
