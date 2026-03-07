#![cfg_attr(target_family = "wasm", no_main)]

pub mod components;
pub mod layout;
pub mod panels;
pub mod terminal;
pub mod terminal_view;
pub mod workspace;

use components::title_bar::TitleBar;
use gpui::{
    App, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    WindowOptions, div, prelude::*, px, size,
};
use workspace::workspace::Workspace;

/// Root component that contains title bar and workspace
pub struct AppWindow {
    title_bar: Entity<TitleBar>,
    workspace: Entity<Workspace>,
}

impl AppWindow {
    pub fn new(cx: &mut App) -> Self {
        // 先创建 Workspace
        let workspace = Workspace::new(cx);

        // 从 Workspace 获取 TabBar，然后创建 TitleBar
        let tab_bar = workspace.read(cx).tab_bar.clone();
        let title_bar = cx.new(|_cx| TitleBar::new("title-bar").with_tab_bar(tab_bar));

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
    gpui::Application::with_platform(gpui_platform::current_platform(false)).run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(gpui::px(1400.0), gpui::px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                // macOS: 禁用原生窗口拖拽，改用 GPUI 手动实现
                // 这样可以避免透明标题栏导致整个区域都可拖拽的问题
                #[cfg(target_os = "macos")]
                is_movable: false,
                #[cfg(not(target_os = "macos"))]
                is_movable: true,
                ..Default::default()
            },
            |_, cx| cx.new(|cx| AppWindow::new(cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
