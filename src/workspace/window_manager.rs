//! Window manager for multi-window support
//!
//! Manages all open DeepTerm windows and provides operations to open new windows.

use gpui::{App, AppContext, Global, WindowBackgroundAppearance, WindowDecorations, WindowOptions};

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
    /// Open a new DeepTerm window with the default size and options.
    ///
    /// `open_paths`, when given, are opened in the editor once the window is
    /// ready (supports `deepterm <file>...` from the command line).
    pub fn open_window(cx: &mut App, open_paths: Vec<std::path::PathBuf>) {
        let bounds =
            gpui::Bounds::centered(None, gpui::size(gpui::px(1400.0), gpui::px(900.0)), cx);

        if let Err(e) = cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                window_decorations: Some(WindowDecorations::Client),
                #[cfg(target_os = "macos")]
                is_movable: false,
                #[cfg(not(target_os = "macos"))]
                is_movable: true,
                window_background: WindowBackgroundAppearance::Blurred,
                show: false,
                focus: false,
                ..Default::default()
            },
            |window, cx| {
                let app_window = cx.new(|_cx| AppWindow::new(window, _cx));
                let window_handle = app_window.clone();
                window.defer(cx, move |window, cx| {
                    window_handle.update(cx, |app, cx| {
                        app.setup_initial_focus(window, cx);
                        for path in &open_paths {
                            app.editor.update(cx, |editor, cx| {
                                editor.open_file(path.clone(), window, cx);
                            });
                        }
                    });
                    // 窗口内容初始化完成后，再显示并激活窗口
                    // 避免 Windows 上启动时出现空白窗口/闪烁问题
                    window.activate_window();
                });
                app_window
            },
        ) {
            log::error!("Failed to open DeepTerm window: {e:?}");
        }
    }
}
