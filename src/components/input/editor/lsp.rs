use gpui::{SharedString, px};
use ropey::Rope;

#[derive(Clone)]
pub struct HoverDefinition {
    pub label: SharedString,
}

#[derive(Clone)]
pub struct InlineCompletion {
    pub text: SharedString,
    pub range: std::ops::Range<usize>,
}

#[derive(Default)]
pub struct Lsp;

impl Lsp {
    pub fn new() -> Self {
        Self
    }
}
