//! Title bar component for window management
//!
//! Supports Linux, Windows, and macOS with platform-specific window controls
//!
//! Layout: [TabBar][Draggable Area][Close Button]

use std::time::{Duration, Instant};

use gpui::{
    App, Context, Decorations, ElementId, Entity, IntoElement, MouseButton, ParentElement, Pixels,
    Render, TitlebarOptions, Window, WindowControlArea, div, prelude::*, px,
};

use crate::components::tab_bar::TabBar;

pub const TITLE_BAR_HEIGHT: Pixels = px(34.);

#[cfg(target_os = "macos")]
const TITLE_BAR_LEFT_PADDING: Pixels = px(80.);

#[cfg(not(target_os = "macos"))]
const TITLE_BAR_LEFT_PADDING: Pixels = px(0.);

const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(400);

/// TitleBar used to customize the appearance of the title bar.
/// Layout: [TabBar][Draggable Area][Close Button]
pub struct TitleBar {
    id: ElementId,
    last_click_time: Option<Instant>,
    tab_bar: Option<Entity<TabBar>>,
}

impl TitleBar {
    /// Create a new TitleBar.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            last_click_time: None,
            tab_bar: None,
        }
    }

    /// Create a new TitleBar as an Entity.
    pub fn new_entity(cx: &mut App) -> Entity<Self> {
        cx.new(|_cx| Self::new("title-bar"))
    }

    /// Create a new TitleBar with a TabBar.
    pub fn with_tab_bar(mut self, tab_bar: Entity<TabBar>) -> Self {
        self.tab_bar = Some(tab_bar);
        self
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
            if elapsed < DOUBLE_CLICK_THRESHOLD {
                // Double-click detected, toggle maximize
                window.zoom_window();
                self.last_click_time = None;
                return;
            }
        }

        // Single click - start window move
        self.last_click_time = Some(now);
        window.start_window_move();
    }
}

impl Default for TitleBar {
    fn default() -> Self {
        Self::new("title-bar")
    }
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let is_web = cfg!(target_family = "wasm");
        let is_linux = cfg!(target_os = "linux");
        let is_macos = cfg!(target_os = "macos");

        // Main title bar container - 与工作空间背景色一致
        div()
            .id(self.id.clone())
            .flex()
            .flex_row()
            .items_center()
            .h(TITLE_BAR_HEIGHT)
            .pl(TITLE_BAR_LEFT_PADDING)
            .bg(gpui::rgb(0x0f0f1a))
            // 区域1: TabBar (如果有)
            .when_some(self.tab_bar.clone(), |this, tab_bar| {
                this.child(div().flex_shrink_0().child(tab_bar))
            })
            // 区域2: 可拖拽的空白区域
            .child(
                div()
                    .flex()
                    .h_full()
                    .flex_1()
                    .window_control_area(WindowControlArea::Drag)
                    .when(window.is_fullscreen(), |this| this.pl_3())
                    // Right-click context menu on Linux
                    .when(is_linux && is_client_decorated && !is_web, |this| {
                        this.child(
                            div()
                                .top_0()
                                .left_0()
                                .absolute()
                                .size_full()
                                .h_full()
                                .on_mouse_down(MouseButton::Right, move |ev, window, _| {
                                    window.show_window_menu(ev.position)
                                }),
                        )
                    })
                    // 双击最大化/恢复，单击拖拽
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, _| {
                            this.handle_title_click(window);
                        }),
                    ),
            )
            // 区域3: 关闭窗口按钮
            .when(!is_macos && !is_web, |this| {
                this.child(
                    div()
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
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.quit();
                        }))
                        .child("×"),
                )
            })
    }
}
