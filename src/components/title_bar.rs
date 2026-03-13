//! Title bar component for window management
//!
//! Supports Linux, Windows, and macOS with platform-specific window controls
//!
//! Layout: [TabBar][Draggable Area][Close Button]

use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    App, Context, Decorations, ElementId, Entity, IntoElement, MouseButton, ParentElement, Pixels,
    Render, TitlebarOptions, Window, WindowControlArea, div, px,
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
    /// 用于跟踪是否应该移动窗口（macOS 手动拖拽实现）
    should_move: bool,
}

impl TitleBar {
    /// Create a new TitleBar.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            last_click_time: None,
            tab_bar: None,
            should_move: false,
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
        let is_fullscreen = window.is_fullscreen();

        // Main title bar container - 与工作空间背景色一致
        div()
            .id(self.id.clone())
            .flex()
            .flex_row()
            .items_center()
            .h(TITLE_BAR_HEIGHT)
            // macOS: 非全屏时留出红绿灯按钮位置，全屏时占据全部宽度
            .when(is_macos && !is_fullscreen, |this| this.pl(TITLE_BAR_LEFT_PADDING))
            .when(is_macos && is_fullscreen, |this| this.pl_3())
            .when(!is_macos, |this| this.pl(TITLE_BAR_LEFT_PADDING))
            .bg(gpui::rgb(0x0f0f1a))
            // TabBar (如果有) - 阻止事件冒泡以防止触发窗口拖拽
            .when_some(self.tab_bar.clone(), |this, tab_bar| {
                this.child(
                    div()
                        .flex_shrink_0()
                        // 阻止 TabBar 区域的鼠标事件冒泡到 TitleBar，防止触发窗口拖拽
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(tab_bar),
                )
            })
            // 可拖拽区域容器 - 只有空白区域可以拖拽窗口
            .child(
                div()
                    .flex()
                    .h_full()
                    .flex_1()
                    // Windows/Linux: 使用 window_control_area
                    // macOS: 不使用 window_control_area，改用 GPUI 手动事件处理
                    .when(!is_macos && !is_web, |this| {
                        this.window_control_area(WindowControlArea::Drag)
                    })
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
                    // macOS: 手动实现窗口拖拽
                    .when(is_macos, |this| {
                        this.on_mouse_down_out(cx.listener(|this, _, _, _| {
                            this.should_move = false;
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| {
                                this.should_move = false;
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| {
                                this.should_move = true;
                            }),
                        )
                        .on_mouse_move(cx.listener(
                            |this, _, window, _| {
                                if this.should_move {
                                    this.should_move = false;
                                    window.start_window_move();
                                }
                            },
                        ))
                    })
                    // Windows/Linux: 双击最大化/恢复，单击拖拽
                    .when(!is_macos && !is_web, |this| {
                        this.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, _| {
                                this.handle_title_click(window);
                            }),
                        )
                    }),
            )
            // 关闭窗口按钮 (仅 Windows/Linux)
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
