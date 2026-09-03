//! Title bar component for window management
//!
//! Supports Linux, Windows, and macOS with platform-specific window controls
//!
//! Layout: [TabBar][Draggable Area][Trailing Slot][Window Controls]
//!
//! 传输任务铃铛等业务内容通过 trailing 插槽挂载（见
//! [`crate::workspace::transfer_bell::TransferBellPanel`]），
//! 本组件只保留通用窗口管理布局。

use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    AnyView, App, Context, Decorations, ElementId, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseMoveEvent, ParentElement, Pixels, Render, TitlebarOptions, Window,
    WindowControlArea, div, px,
};

use crate::theme::ActiveTheme;
use crate::workspace::tab_bar::TabBar;
use crate::workspace::workspace::Workspace;

pub const TITLE_BAR_HEIGHT: Pixels = px(44.);

#[cfg(target_os = "macos")]
const TITLE_BAR_LEFT_PADDING: Pixels = px(8.);

#[cfg(not(target_os = "macos"))]
const TITLE_BAR_LEFT_PADDING: Pixels = px(6.);

const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(400);

/// TitleBar used to customize the appearance of the title bar.
/// Layout: [TabBar][Draggable Area][Trailing Slot][Window Controls]
pub struct TitleBar {
    id: ElementId,
    last_click_time: Option<Instant>,
    tab_bar: Option<Entity<TabBar>>,
    /// 用于跟踪是否应该移动窗口（macOS 手动拖拽实现）
    should_move: bool,
    /// trailing 插槽：挂在拖拽区与窗口控制按钮之间的业务视图
    /// （例如传输任务铃铛面板）。
    trailing: Option<AnyView>,
    /// 覆盖 TitleBar 默认左侧内边距。
    /// 当编辑器打开时设为 `3px`，使终端 TabBar 与编辑器分割线保持 3px 间距。
    left_padding: Pixels,
}

impl TitleBar {
    /// Create a new TitleBar.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            last_click_time: None,
            tab_bar: None,
            should_move: false,
            trailing: None,
            left_padding: TITLE_BAR_LEFT_PADDING,
        }
    }

    /// 设置 TitleBar 左侧内边距覆盖值。用于在编辑器打开时精确控制
    /// 终端 TabBar 与编辑器分割线的间距。
    pub fn set_left_padding(&mut self, padding: Pixels) {
        self.left_padding = padding;
    }

    /// 重置 TitleBar 左侧内边距为平台默认值。
    pub fn reset_left_padding(&mut self) {
        self.left_padding = TITLE_BAR_LEFT_PADDING;
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

    /// Create a new TitleBar with a trailing view (e.g. the transfer bell panel).
    pub fn with_trailing(mut self, view: impl Into<AnyView>) -> Self {
        self.trailing = Some(view.into());
        self
    }

    /// 切换 TitleBar 绑定的工作区（更新 TabBar 引用）。
    pub fn set_workspace(&mut self, workspace: Entity<Workspace>, cx: &mut Context<Self>) {
        self.tab_bar = Some(workspace.read(cx).tab_bar.clone());
        cx.notify();
    }

    /// Returns the default title bar options for compatible with the [`TitleBar`].
    pub fn title_bar_options() -> TitlebarOptions {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(15.0), px(16.0))),
        }
    }

    /// Windows/Linux: 单击立即进入窗口移动（配合 `window_control_area`），
    /// 双击最大化/还原。
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

    /// macOS: 按下不立即拖拽，只有真正移动后才触发 `start_window_move`，
    /// 双击直接缩放窗口。避免与系统 traffic lights / TabBar 的点击冲突。
    fn handle_macos_press(&mut self, window: &mut Window) {
        let now = Instant::now();

        if let Some(prev_time) = self.last_click_time {
            let elapsed = now.duration_since(prev_time);
            if elapsed < DOUBLE_CLICK_THRESHOLD {
                // Double-click detected, toggle maximize
                self.should_move = false;
                window.zoom_window();
                self.last_click_time = None;
                return;
            }
        }

        self.last_click_time = Some(now);
        self.should_move = true;
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
        let theme = cx.theme().ui;

        // TabBar 自行管理标签溢出滚动（overflow_x_scroll + ScrollHandle），
        // 外部无需感知可用宽度或溢出状态

        // Main title bar container - 与工作空间背景色一致
        div()
            .id(self.id.clone())
            .flex()
            .flex_row()
            .items_center()
            .h(TITLE_BAR_HEIGHT)
            .pl(self.left_padding)
            .bg(theme.app_background)
            // TabBar (如果有) - 阻止事件冒泡以防止触发窗口拖拽
            .when_some(self.tab_bar.clone(), |this, tab_bar| {
                this.child(
                    div()
                        .flex_shrink(1.0)
                        .min_w_0() // 允许收缩到 0 宽度
                        .max_w_full()
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
                    // macOS: 手动实现窗口拖拽（移动后才触发），双击缩放
                    .when(is_macos, |this| {
                        this.on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| {
                                this.should_move = false;
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, _| {
                                this.handle_macos_press(window);
                            }),
                        )
                        .on_mouse_move(cx.listener(
                            |this, event: &MouseMoveEvent, window, _| {
                                // 按下的按钮已不在按压状态（例如按下后在
                                // 标题栏外释放、或干脆没按下）时，清除可能
                                // 卡住的拖拽标记，避免回到标题栏误触发拖拽。
                                if event.pressed_button.is_none() {
                                    this.should_move = false;
                                    return;
                                }
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
            // trailing 插槽：业务视图（如传输任务铃铛）
            .when_some(self.trailing.clone(), |this, trailing| this.child(trailing))
            // 窗口控制按钮组 (仅 Windows/Linux)
            .when(!is_macos && !is_web, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_row()
                        .h_full()
                        .flex_shrink_0()
                        // 最小化按钮
                        .child(
                            div()
                                .id("minimize-btn")
                                .flex()
                                .w(TITLE_BAR_HEIGHT)
                                .h_full()
                                .justify_center()
                                .items_center()
                                .hover(move |style| style.bg(theme.title_bar_hover))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(|_, _, window, _| {
                                    window.minimize_window();
                                }))
                                .child(
                                    gpui::svg()
                                        .size(px(16.))
                                        .text_color(theme.icon)
                                        .path(crate::assets::Icon::WindowMinimize.path()),
                                ),
                        )
                        // 最大化/恢复按钮
                        .child(
                            div()
                                .id("maximize-btn")
                                .flex()
                                .w(TITLE_BAR_HEIGHT)
                                .h_full()
                                .justify_center()
                                .items_center()
                                .hover(move |style| style.bg(theme.title_bar_hover))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(|_, _, window, _| {
                                    window.zoom_window();
                                }))
                                .child(gpui::svg().size(px(16.)).text_color(theme.icon).path(
                                    if window.is_maximized() {
                                        crate::assets::Icon::WindowRestore.path()
                                    } else {
                                        crate::assets::Icon::WindowMaximize.path()
                                    },
                                )),
                        )
                        // 关闭按钮
                        .child(
                            div()
                                .id("close-btn")
                                .flex()
                                .w(TITLE_BAR_HEIGHT)
                                .h_full()
                                .justify_center()
                                .items_center()
                                .hover(move |style| style.bg(theme.error))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.quit();
                                }))
                                .child(
                                    gpui::svg()
                                        .size(px(16.))
                                        .text_color(theme.icon)
                                        .path(crate::assets::Icon::WindowClose.path()),
                                ),
                        ),
                )
            })
    }
}
