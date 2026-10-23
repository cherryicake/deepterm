//! Live-preview markdown: Obsidian-style single-buffer editing.
//!
//! Pure pipeline (zero GPUI, fully unit-testable):
//! `buffer text -> block::classify -> layout::plan (+ inline::parse inside)
//!  -> reveal::plan_document -> element/view render`.
//!
//! Ported from `guise::markdown` (`block`/`layout` verbatim,
//! `inline` + backslash-escape extension, `model` = guise `EditorModel`
//! verbatim). Rendering (`element`/`view`) is rewritten for deepterm's
//! GPUI fork (zed `rev d25fa01`) on top of the existing `MarkdownStyle`,
//! `SyntaxHighlighter` and `SelectableText` primitives.

pub mod block;
pub mod buffer;
pub mod element;
pub mod emoji;
pub mod highlight;
pub mod images;
pub mod inline;
pub mod interactions;
pub mod layout;
pub mod mermaid;
pub mod model;
pub mod reveal;
pub mod scrollbar;
pub mod table_ops;
pub mod view;

pub use block::{Block, DocState, classify, indent_cols};
pub use buffer::LiveBuffer;
pub use inline::{Inline, InlineSpan, InlineStyle};
pub use layout::{
    RowKind, RowMetrics, RowPlan, RunAttrs, Seg, byte_for_col, col_for_byte, metrics, plan,
    src_for_vis, vis_for_src,
};
pub use model::{EditorModel, Pos};
pub use reveal::{Callout, CalloutKind, PlannedRow, Reveal, plan_document, strip_plan_prefix};
pub use view::{LiveMarkdownView, LiveViewEvent};
