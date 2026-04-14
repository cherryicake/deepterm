use gpui::{Context, Entity, IntoElement, Render, SharedString, Window, div};

pub struct ContextMenu;
impl Render for ContextMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub struct DiagnosticPopover {
    pub diagnostic: String,
}
impl DiagnosticPopover {
    pub fn new() -> Self {
        Self { diagnostic: String::new() }
    }
    pub fn show(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
}
impl Render for DiagnosticPopover {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub struct HoverPopover;
impl Render for HoverPopover {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub struct InputContextMenu;

impl InputContextMenu {
    pub fn new(_entity: Entity<super::InputState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self
    }
    pub fn hide(&mut self, _cx: &mut Context<Self>) {}
}
