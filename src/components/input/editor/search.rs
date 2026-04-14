use gpui::{App, Context, Entity, Window};

pub struct SearchPanel;

impl SearchPanel {
    pub fn new() -> Self {
        Self
    }
    pub fn is_open(&self) -> bool {
        false
    }
    pub fn open(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
    pub fn close(&mut self, _cx: &mut Context<Self>) {}
    pub fn matcher(&self) -> Option<()> {
        None
    }
}

pub fn init(_cx: &mut App) {}
