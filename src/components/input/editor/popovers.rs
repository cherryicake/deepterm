use gpui::{Context, Entity, Window};

pub struct ContextMenu;
pub struct DiagnosticPopover;
pub struct HoverPopover;

pub struct InputContextMenu;

impl InputContextMenu {
    pub fn hide(&mut self, _cx: &mut Context<Self>) {}
}
