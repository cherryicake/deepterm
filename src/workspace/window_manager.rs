//! Window manager for multi-window support
//!
//! Manages all open DeepTerm windows and provides operations to open new windows.

use gpui::{
    App, Global, WindowBackgroundAppearance, WindowDecorations, WindowOptions, AppContext,
};

use crate::AppWindow;
use crate::components::title_bar::TitleBar;

/// Manages all open DeepTerm windows.
///
/// Stored as a GPUI global so it can be accessed from anywhere in the app.
pub struct WindowManager;

impl Global for WindowManager {}

impl WindowManager {
    pub fn init(cx: &mut App) {
        cx.set_global(Self);

        // Quit the application when the last window is closed.
        cx.set_quit_mode(gpui::QuitMode::LastWindowClosed);
    }

    /// Open a new DeepTerm window with the default size and options.
    pub fn open_window(cx: &mut App) {
        let bounds = gpui::Bounds::centered(
            None,
            gpui::size(gpui::px(1400.0), gpui::px(900.0)),
            cx,
        );

        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                window_decorations: Some(WindowDecorations::Client),
                #[cfg(target_os = "macos")]
                is_movable: false,
                #[cfg(not(target_os = "macos"))]
                is_movable: true,
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            |window, cx| {
                let app_window = cx.new(|_cx| AppWindow::new(window, _cx));
                let window_handle = app_window.clone();
                window.defer(cx, move |window, cx| {
                    window_handle.update(cx, |app, cx| {
                        app.setup_initial_focus(window, cx);
                    });
                });
                app_window
            },
        )
        .unwrap();
    }
}

