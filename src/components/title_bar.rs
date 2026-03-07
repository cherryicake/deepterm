//! Title bar component for window management
//!
//! Supports Linux, Windows, and macOS with platform-specific window controls

use gpui::{
    div, prelude::*, px, App, Context, Decorations, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowControlArea,
};
use std::time::{Duration, Instant};

pub const TITLE_BAR_HEIGHT: Pixels = px(34.);

#[cfg(target_os = "macos")]
const TITLE_BAR_LEFT_PADDING: Pixels = px(80.);

#[cfg(not(target_os = "macos"))]
const TITLE_BAR_LEFT_PADDING: Pixels = px(12.);

const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(400);

/// TitleBar used to customize the appearance of the title bar.
pub struct TitleBar {
    last_click_time: Option<Instant>,
}

impl TitleBar {
    /// Create a new TitleBar.
    pub fn new() -> Self {
        Self {
            last_click_time: None,
        }
    }

    /// Create a new TitleBar as an Entity.
    pub fn new_entity(cx: &mut App) -> Entity<Self> {
        cx.new(|_cx| Self::new())
    }

    /// Returns the default title bar options for compatible with the [`TitleBar`].
    pub fn title_bar_options() -> TitlebarOptions {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(9.0), px(9.0))),
        }
    }

    fn handle_title_click(&mut self, window: &mut Window) {
        let now = Instant::now();

        if let Some(prev_time) = self.last_click_time {
            let elapsed = now.duration_since(prev_time);
            eprintln!("Time since last click: {:?}", elapsed);
            if elapsed < DOUBLE_CLICK_THRESHOLD {
                // Double-click detected, toggle maximize
                eprintln!("Double-click detected! Toggling maximize.");
                window.zoom_window();
                self.last_click_time = None;
                return;
            }
        }

        // Single click - start window move
        eprintln!("Single click detected, starting window move.");
        self.last_click_time = Some(now);
        window.start_window_move();
    }
}

impl Default for TitleBar {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let is_client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let is_web = cfg!(target_family = "wasm");
        let is_linux = cfg!(target_os = "linux");
        let is_macos = cfg!(target_os = "macos");

        // Main title bar container - 与工作空间背景色一致
        let mut title_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(TITLE_BAR_HEIGHT)
            .pl(TITLE_BAR_LEFT_PADDING)
            .bg(gpui::rgb(0x0f0f1a));

        // Left area (draggable + context menu on Linux)
        let mut left_area = div().flex().h_full().flex_1();

        if !is_web {
            left_area = left_area.window_control_area(WindowControlArea::Drag);

            if window.is_fullscreen() {
                left_area = left_area.pl_3();
            }

            // Right-click context menu on Linux
            if is_linux && is_client_decorated {
                left_area = left_area.child(
                    div()
                        .top_0()
                        .left_0()
                        .absolute()
                        .size_full()
                        .h_full()
                        .on_mouse_down(MouseButton::Right, move |ev, window, _| {
                            window.show_window_menu(ev.position)
                        }),
                );
            }
        }

        title_bar = title_bar.child(left_area);

        // Window controls (minimize, maximize, close)
        // On Linux, only show close button; on Windows, show all controls
        if !is_macos && !is_web {
            let mut controls = div().flex().items_center().flex_shrink_0().h_full();

            // Only show minimize and maximize buttons on Windows (not Linux)
            if !is_linux {
                // Minimize button
                let minimize_btn = div()
                    .id("minimize-btn")
                    .flex()
                    .w(TITLE_BAR_HEIGHT)
                    .h_full()
                    .flex_shrink_0()
                    .justify_center()
                    .items_center()
                    .text_color(gpui::rgb(0xcccccc))
                    .hover(|style| style.bg(gpui::rgb(0x5a5a5a)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(|_, window, _| window.minimize_window());
                controls = controls.child(minimize_btn.child("−"));

                // Maximize/Restore button
                let maximize_icon = if window.is_maximized() { "❐" } else { "□" };
                let maximize_btn = div()
                    .id("maximize-btn")
                    .flex()
                    .w(TITLE_BAR_HEIGHT)
                    .h_full()
                    .flex_shrink_0()
                    .justify_center()
                    .items_center()
                    .text_color(gpui::rgb(0xcccccc))
                    .hover(|style| style.bg(gpui::rgb(0x5a5a5a)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(|_, window, _| window.zoom_window());
                controls = controls.child(maximize_btn.child(maximize_icon));
            }

            // Close button (always shown on non-macOS platforms)
            let close_btn = div()
                .id("close-btn")
                .flex()
                .w(TITLE_BAR_HEIGHT)
                .h_full()
                .flex_shrink_0()
                .justify_center()
                .items_center()
                .text_color(gpui::rgb(0xcccccc))
                .hover(|style| style.bg(gpui::rgb(0xe74c3c)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(move |_, _, cx| {
                    cx.quit();
                });
            controls = controls.child(close_btn.child("×"));

            title_bar = title_bar.child(controls);
        }

        // Window dragging - only on the title bar background, not on controls
        // This must be added after controls to ensure controls are on top
        // Double-click to maximize/restore window
        title_bar = title_bar.on_mouse_down(MouseButton::Left, {
            let entity = _cx.entity();
            move |_, window, cx| {
                entity.update(cx, |this, _| {
                    this.handle_title_click(window);
                });
            }
        });

        div().flex_shrink_0().child(title_bar)
    }
}
