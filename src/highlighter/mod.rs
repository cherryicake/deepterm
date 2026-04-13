mod simple;
pub use simple::*;

mod registry;
pub use registry::*;

pub mod languages;

use ropey::Rope;

/// Stub DiagnosticSet for compatibility
#[derive(Clone, Default)]
pub struct DiagnosticSet;

impl DiagnosticSet {
    pub fn new(_text: &Rope) -> Self {
        Self
    }
}

/// Stub SyntaxHighlighter alias pointing to our SimpleHighlighter
pub use simple::SimpleHighlighter as SyntaxHighlighter;
