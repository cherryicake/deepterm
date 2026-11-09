//! A diff view for reviewing changes: a file's hunks, unified or split, with word changes,
//! syntax colours, expandable unchanged ranges and line selection.
//!
//! [`FileDiff::from_texts`] / [`FileDiff::between`] build the model from two texts, and
//! [`line_diff`] / [`compute_hunks`] give the row ranges the git gutter and staging paths use.
//! The app keeps a [`DiffState`] in its state and draws it with [`DiffView`].
//!
//! The component is ported from rune-kit's `diff` module, with the data model and row layout
//! kept and the rendering rewritten for GPUI.

mod model;
mod state;
mod theme;
mod view;

pub use model::{
    CONTEXT, DiffHunk, FileDiff, FileStatus, Hunk, Line, LineKind, Side, build_hunk_patch,
    compute_hunks, line_count, line_diff, line_of, line_range, line_starts,
};
pub use state::{DiffMode, DiffState, EXPAND_STEP, Expand, LineRange, LineRef, Row};
pub use theme::{DiffTheme, Tints};
pub use view::DiffView;
