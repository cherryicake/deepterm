#![cfg_attr(target_family = "wasm", no_main)]

pub mod components;
pub mod layout;
pub mod panels;
pub mod terminal;
pub mod terminal_view;
pub mod workspace;

use gpui::{App, Bounds, WindowOptions, size};
use gpui_platform::application;

use workspace::workspace::Workspace;

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(gpui::px(1400.0), gpui::px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| Workspace::new(cx),
        )
        .unwrap();
        cx.activate(true);
    });
}
