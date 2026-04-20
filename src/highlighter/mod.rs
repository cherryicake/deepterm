mod simple;
pub use simple::*;

mod registry;
pub use registry::*;

pub mod languages;

use ropey::Rope;

/// Stub DiagnosticSet for compatibility
#[derive(Clone, Default)]
pub struct DiagnosticSet;

pub struct DiagnosticItem {
    pub range: std::ops::Range<usize>,
}

impl DiagnosticSet {
    pub fn new(_text: &Rope) -> Self {
        Self
    }
    pub fn reset(&mut self, _text: &Rope) {}
    pub fn for_offset(&self, _offset: usize) -> Option<DiagnosticItem> {
        None
    }
    pub fn styles_for_range(
        &self,
        _range: &std::ops::Range<usize>,
        _cx: &gpui::App,
    ) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
        Vec::new()
    }
}

/// Stub SyntaxHighlighter alias pointing to our SimpleHighlighter
pub use simple::SimpleHighlighter as SyntaxHighlighter;
