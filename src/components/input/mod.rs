pub const MASK_CHAR: char = '•';

pub(crate) mod change;
pub mod history;

pub mod blink_cursor;
pub mod display_map;
pub mod element;
pub mod indent;
pub mod input;
pub mod mode;
pub mod movement;
pub mod rope_ext;
pub mod selection;
pub mod state;

pub use display_map::{BufferPoint, DisplayMap, DisplayPoint, FoldRange};
pub use indent::TabSize;
pub use input::EditorInput;
pub use rope_ext::{InputEdit, Point, RopeExt, RopeLines};
pub use ropey::Rope;
pub use selection::Selection;
pub use state::*;
