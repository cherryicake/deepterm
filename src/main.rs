#![cfg_attr(target_family = "wasm", no_main)]

pub mod components;
pub mod layout;
pub mod panels;
pub mod terminal;
pub mod terminal_view;
pub mod workspace;

use gpui::{
    App, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    WindowOptions, div, prelude::*, px, size,
};
use gpui_platform::application;

use components::title_bar::TitleBar;
use workspace::workspace::Workspace;

/// Root component that contains title bar and workspace
pub struct AppWindow {
    title_bar: Entity<TitleBar>,
    workspace: Entity<Workspace>,
}

impl AppWindow {
    pub fn new(cx: &mut App) -> Self {
        let title_bar = TitleBar::new_entity(cx);
        let workspace = Workspace::new(cx);
        Self {
            title_bar,
            workspace,
        }
    }
}

impl Render for AppWindow {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            // Title bar for window management
            .child(self.title_bar.clone())
            // Workspace content
            .child(div().flex().flex_1().child(self.workspace.clone()))
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(gpui::px(1400.0), gpui::px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| AppWindow::new(cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
