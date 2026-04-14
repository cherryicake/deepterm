use gpui::{Context, SharedString, Window, px};
use ropey::Rope;

#[derive(Clone, Default)]
pub struct HoverDefinition {
    pub label: SharedString,
}

#[derive(Clone, Default)]
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
    pub fn reset(&mut self) {}
    pub fn update(&mut self, _text: &Rope, _window: &mut Window, _cx: &mut Context<super::InputState>) {}
    pub fn document_colors_for_range(&self, _range: std::ops::Range<usize>) -> Vec<(std::ops::Range<usize>, gpui::Hsla)> {
        Vec::new()
    }
}
