use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    prelude::*, px, rgb,
};

use crate::terminal_view::terminal_view::TerminalView;

/// Terminal panel component - integrates real terminal functionality
pub struct TerminalPanel {
    title: SharedString,
    terminal_view: Entity<TerminalView>,
    focus_handle: FocusHandle,
}

impl TerminalPanel {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        let terminal_view = TerminalView::new(cx);

        cx.new(|_cx| Self {
            title: "Terminal".into(),
            terminal_view,
            focus_handle,
        })
    }

    pub fn with_title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = title.into();
        self
    }

    /// Get the terminal view entity
    pub fn terminal_view(&self) -> &Entity<TerminalView> {
        &self.terminal_view
    }

    /// Get the title
    pub fn title(&self) -> SharedString {
        self.title.clone()
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("terminal-panel")
            .size_full()
            .bg(rgb(0x1a1a2e))
            .child(self.terminal_view.clone())
    }
}
