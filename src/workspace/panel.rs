use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    Render, SharedString, Styled, Window, div, px, rgb,
};

use crate::components::tab_bar::{DragPreview, DraggedTab};
use crate::terminal_view::terminal_view::TerminalView;

// Re-export for convenience
pub use crate::ssh::SshConnectionConfig;

/// 拖拽的 Panel 数据
#[derive(Clone, Debug)]
pub struct DraggedPanel {
    pub panel_id: String,
    pub source_tab_id: String,
}

/// Panel placement threshold for drag operations (start at 35% of dimension)
const PANEL_DRAG_THRESHOLD_START: f32 = 0.35;
/// Panel placement threshold for drag operations (end at 65% of dimension)
const PANEL_DRAG_THRESHOLD_END: f32 = 0.65;

/// 面板放置位置
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelPlacement {
    Left,
    Right,
    Top,
    Bottom,
}

/// 面板事件
#[derive(Clone, Debug)]
pub enum PanelEvent {
    /// 拖拽预览变化
    DragPreview {
        panel_id: String,
        placement: Option<PanelPlacement>,
    },
    /// 放置事件（来自Tab拖拽）
    Drop {
        panel_id: String,
        dragged_tab: DraggedTab,
        placement: PanelPlacement,
    },
    /// Panel放置事件（来自Panel标题栏拖拽）
    PanelDrop {
        target_panel_id: String,
        dragged_panel: DraggedPanel,
        placement: PanelPlacement,
    },
    /// Tab 切换事件
    TabChanged {
        panel_id: String,
        tab_id: SharedString,
    },
    /// 关闭panel事件
    Close { panel_id: String },
    /// 选中状态变更事件
    Selected { panel_id: String },
    /// 广播状态切换事件
    BroadcastToggle { panel_id: String },
}

/// Panel content type
#[derive(Clone, Debug)]
pub enum PanelContent {
    /// Empty panel with just text
    Text(String),
    /// Terminal panel
    Terminal(Entity<TerminalView>),
}

/// A panel within the workspace that displays terminal or other content.
///
/// Each panel has its own focus handle and can display either terminal content
/// or placeholder text. Panels support drag-and-drop operations for splitting
/// the workspace layout.
///
/// Note: The tab bar is managed by the Workspace, not individual panels.
pub struct Panel {
    /// Unique identifier for this panel
    panel_id: String,
    /// Focus handle for keyboard input
    focus_handle: FocusHandle,
    /// Current drag preview position (if any)
    drag_preview: Option<PanelPlacement>,
    /// ID of the currently active tab in this panel
    active_tab_id: Option<SharedString>,
    /// Label for the active tab (shown in title bar)
    active_tab_label: Option<SharedString>,
    /// ID of the workspace this panel belongs to
    workspace_id: String,
    /// Whether this is the only panel in the workspace (affects border rendering)
    is_solo: bool,
    /// Whether this panel is currently selected
    is_selected: bool,
    /// Content displayed in this panel
    content: PanelContent,
    /// 广播状态 - 由Workspace同步给所有Panel
    broadcast_enabled: bool,
}

impl Panel {
    /// Creates a new panel with text content.
    ///
    /// # Arguments
    ///
    /// * `panel_id` - Unique identifier for this panel
    /// * `workspace_id` - ID of the workspace this panel belongs to
    /// * `active_tab_id` - ID of the currently active tab
    /// * `cx` - The context for entity creation
    pub fn new(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let content_text = format!("Panel: {}", panel_id);

        Self {
            panel_id,
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label: None,
            is_solo: false,
            is_selected: false,
            content: PanelContent::Text(content_text),
            broadcast_enabled: false,
        }
    }

    /// Creates a new panel with a terminal view.
    ///
    /// # Arguments
    ///
    /// * `panel_id` - Unique identifier for this panel
    /// * `workspace_id` - ID of the workspace this panel belongs to
    /// * `active_tab_id` - ID of the currently active tab
    /// * `active_tab_label` - Label shown in the title bar
    /// * `cx` - The context for entity creation
    pub fn with_terminal(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let terminal_view = TerminalView::new(cx);

        Self {
            panel_id: panel_id.into(),
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label,
            is_solo: false,
            is_selected: false,
            content: PanelContent::Terminal(terminal_view),
            broadcast_enabled: false,
        }
    }

    /// Creates a new panel with an SSH remote terminal view.
    ///
    /// # Arguments
    ///
    /// * `panel_id` - Unique identifier for this panel
    /// * `workspace_id` - ID of the workspace this panel belongs to
    /// * `active_tab_id` - ID of the currently active tab
    /// * `active_tab_label` - Label shown in the title bar
    /// * `ssh_config` - SSH connection configuration
    /// * `on_data` - Callback for sending data to SSH
    /// * `on_resize` - Callback for resizing SSH terminal
    /// * `cx` - The context for entity creation
    pub fn with_ssh_terminal(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        ssh_config: &crate::ssh::SshConnectionConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        let terminal_view = TerminalView::new_ssh(cx, ssh_config);

        Self {
            panel_id: panel_id.into(),
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label,
            is_solo: false,
            is_selected: false,
            content: PanelContent::Terminal(terminal_view),
            broadcast_enabled: false,
        }
    }

    /// 设置panel内容
    pub fn set_content(&mut self, content: PanelContent, cx: &mut Context<Self>) {
        self.content = content;
        cx.notify();
    }

    /// Returns a reference to the panel's content.
    pub fn content(&self) -> &PanelContent {
        &self.content
    }

    /// Returns true if this panel contains a terminal view.
    pub fn has_terminal_content(&self) -> bool {
        matches!(self.content, PanelContent::Terminal(_))
    }

    /// 设置Tab标签名
    pub fn set_tab_label(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.active_tab_label = Some(label.into());
        cx.notify();
    }

    /// 获取Tab标签名
    pub fn tab_label(&self) -> Option<&SharedString> {
        self.active_tab_label.as_ref()
    }

    pub fn panel_id(&self) -> &str {
        &self.panel_id
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// 设置关联的 workspace_id
    pub fn set_workspace_id(&mut self, workspace_id: impl Into<String>) {
        self.workspace_id = workspace_id.into();
    }

    /// 设置是否为唯一的panel
    pub fn set_solo(&mut self, is_solo: bool, cx: &mut Context<Self>) {
        self.is_solo = is_solo;
        cx.notify();
    }

    /// 获取是否为唯一的panel
    pub fn is_solo(&self) -> bool {
        self.is_solo
    }

    /// 设置选中状态
    pub fn set_selected(&mut self, is_selected: bool, cx: &mut Context<Self>) {
        self.is_selected = is_selected;
        cx.notify();
    }

    /// 获取选中状态
    pub fn is_selected(&self) -> bool {
        self.is_selected
    }

    /// 设置广播状态
    pub fn set_broadcast_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.broadcast_enabled = enabled;

        // 根据广播状态设置或清除TerminalView的广播回调
        if let PanelContent::Terminal(terminal_view) = &self.content {
            let panel_id = self.panel_id.clone();
            let workspace_id = self.workspace_id.clone();
            terminal_view.update(cx, |tv, _cx| {
                if enabled {
                    // 创建广播回调，当终端有输入时添加到全局队列
                    tv.set_broadcast_callback(Box::new(move |data: &[u8]| {
                        // 将输入添加到全局队列，由Workspace的render方法处理
                        // 包含workspace_id以区分不同WorkSpace的广播
                        crate::workspace::workspace::Workspace::queue_broadcast_input(
                            &workspace_id, &panel_id, data,
                        );
                    }));
                } else {
                    tv.clear_broadcast_callback();
                }
            });
        }

        cx.notify();
    }

    /// 获取广播状态
    pub fn is_broadcast_enabled(&self) -> bool {
        self.broadcast_enabled
    }

    /// 获取活动 tab ID
    pub fn active_tab_id(&self) -> Option<&SharedString> {
        self.active_tab_id.as_ref()
    }

    /// 设置活动 tab
    pub fn set_active_tab(&mut self, tab_id: impl Into<SharedString>, cx: &mut Context<Self>) {
        let tab_id: SharedString = tab_id.into();
        let tab_id_for_event = tab_id.clone();
        self.active_tab_id = Some(tab_id);

        cx.emit(PanelEvent::TabChanged {
            panel_id: self.panel_id.clone(),
            tab_id: tab_id_for_event,
        });
        cx.notify();
    }

    /// 计算拖拽放置位置
    fn calculate_placement(
        &self,
        bounds: &gpui::Bounds<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
    ) -> Option<PanelPlacement> {
        // 使用 gpui-component 的方式：35%/65% 阈值
        if position.x < bounds.left() + bounds.size.width * PANEL_DRAG_THRESHOLD_START {
            Some(PanelPlacement::Left)
        } else if position.x > bounds.left() + bounds.size.width * PANEL_DRAG_THRESHOLD_END {
            Some(PanelPlacement::Right)
        } else if position.y < bounds.top() + bounds.size.height * PANEL_DRAG_THRESHOLD_START {
            Some(PanelPlacement::Top)
        } else if position.y > bounds.top() + bounds.size.height * PANEL_DRAG_THRESHOLD_END {
            Some(PanelPlacement::Bottom)
        } else {
            None
        }
    }
}

impl Focusable for Panel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel {
    /// 获取内部 TerminalView 的 focus handle（如果有）
    pub fn terminal_focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match &self.content {
            PanelContent::Terminal(terminal_view) => {
                Some(terminal_view.read(cx).focus_handle(cx))
            }
            _ => None,
        }
    }
}

impl EventEmitter<PanelEvent> for Panel {}

impl Panel {
    /// 渲染标题栏
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 使用Tab标签名作为标题，如果没有则使用Tab ID，最后使用Panel ID
        let title = self
            .active_tab_label
            .as_ref()
            .map(|s| s.to_string())
            .or_else(|| self.active_tab_id.as_ref().map(|s| s.to_string()))
            .unwrap_or_else(|| self.panel_id.clone());

        let panel_id = self.panel_id.clone();
        let panel_id_for_broadcast = panel_id.clone();
        let panel_id_for_close = panel_id.clone();
        let source_tab_id = self
            .active_tab_id
            .clone()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let drag_title = title.clone();
        let broadcast_enabled = self.broadcast_enabled;

        div()
            .h(px(28.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .cursor_move()
            .child(
                // 标题 - 显示Tab标签名（使用更小的字体）
                // 将on_drag绑定到标题元素上，确保拖拽位置计算正确
                div()
                    .text_xs()
                    .text_color(rgb(0x8888aa))
                    .id(format!("panel-title-{}", panel_id))
                    .on_drag(
                        DraggedPanel {
                            panel_id: panel_id.clone(),
                            source_tab_id,
                        },
                        move |_drag_data, _position, _window, cx| {
                            cx.new(|_cx| DragPreview {
                                tab_label: drag_title.clone().into(),
                            })
                        },
                    )
                    .child(title),
            )
            .child(
                // 右侧按钮组：广播按钮 + 关闭按钮
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        // 广播按钮 - 开启/关闭终端输入广播功能
                        div()
                            .id(format!("broadcast-btn-{}", panel_id))
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .when(broadcast_enabled, |this| {
                                this.bg(rgb(0x4a9eff))
                                    .text_color(rgb(0xffffff))
                            })
                            .when(!broadcast_enabled, |this| {
                                this.text_color(rgb(0x666688))
                                    .hover(|style| style.bg(rgb(0x3d3d5c)))
                            })
                            .cursor_pointer()
                            .text_xs()
                            .child(if broadcast_enabled { "B" } else { "b" })
                            .on_click(
                                cx.listener(move |_this, _event: &gpui::ClickEvent, _window, cx| {
                                    cx.emit(PanelEvent::BroadcastToggle {
                                        panel_id: panel_id_for_broadcast.clone(),
                                    });
                                    cx.stop_propagation();
                                }),
                            ),
                    )
                    .child(
                        // 关闭按钮
                        div()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .hover(|style| style.bg(rgb(0x3d3d5c)))
                            .cursor_pointer()
                            .text_color(rgb(0x666688))
                            .text_xs()
                            .child("×")
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |_this, _event, _window, cx| {
                                    cx.emit(PanelEvent::Close {
                                        panel_id: panel_id_for_close.clone(),
                                    });
                                    cx.stop_propagation();
                                }),
                            ),
                    ),
            )
    }

    /// 渲染内容区域
    fn render_content(&self) -> impl IntoElement {
        match &self.content {
            PanelContent::Text(text) => {
                div().flex_1().flex().items_center().justify_center().child(
                    self.active_tab_id
                        .as_ref()
                        .map(|id| format!("{} | Tab: {}", text, id))
                        .unwrap_or_else(|| text.clone()),
                )
            }
            PanelContent::Terminal(terminal_view) => div().flex_1().child(terminal_view.clone()),
        }
    }

    /// 渲染拖拽预览层
    fn render_drag_preview(&self) -> Option<impl IntoElement> {
        self.drag_preview.map(|preview| {
            div()
                .absolute()
                .bg(rgb(0x6c5ce7))
                .rounded(px(8.0))
                .map(|div| match preview {
                    PanelPlacement::Left => div
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(gpui::DefiniteLength::Fraction(0.5)),
                    PanelPlacement::Right => div
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(gpui::DefiniteLength::Fraction(0.5)),
                    PanelPlacement::Top => div
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(gpui::DefiniteLength::Fraction(0.5)),
                    PanelPlacement::Bottom => div
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(gpui::DefiniteLength::Fraction(0.5)),
                })
        })
    }

    /// 处理拖拽移动事件
    fn handle_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        let bounds = event.bounds;
        let position = event.event.position;

        // 检查鼠标是否还在内容区域内
        let is_inside = position.x >= bounds.left()
            && position.x <= bounds.right()
            && position.y >= bounds.top()
            && position.y <= bounds.bottom();

        if !is_inside {
            // 鼠标离开内容区域，清除预览
            if self.drag_preview.is_some() {
                self.drag_preview = None;
                cx.emit(PanelEvent::DragPreview {
                    panel_id: self.panel_id.clone(),
                    placement: None,
                });
                cx.notify();
            }
            return;
        }

        let preview = self.calculate_placement(&bounds, position);

        if preview != self.drag_preview {
            self.drag_preview = preview;

            // 发送事件给父组件
            cx.emit(PanelEvent::DragPreview {
                panel_id: self.panel_id.clone(),
                placement: preview,
            });

            cx.notify();
        }
    }

    /// 处理放置事件
    fn handle_drop(&mut self, dragged: &DraggedTab, cx: &mut Context<Self>) {
        // 如果有preview就使用它，否则默认使用Right
        let placement = self.drag_preview.unwrap_or(PanelPlacement::Right);

        // 发送放置事件
        cx.emit(PanelEvent::Drop {
            panel_id: self.panel_id.clone(),
            dragged_tab: dragged.clone(),
            placement,
        });

        self.drag_preview = None;
        cx.notify();
    }

    /// 处理Panel拖拽移动事件
    fn handle_panel_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedPanel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 获取拖拽数据
        let dragged: &DraggedPanel = event.drag(cx);

        // 不处理自己拖拽到自己
        if dragged.panel_id == self.panel_id {
            return;
        }

        let bounds = event.bounds;
        let position = event.event.position;

        // 检查鼠标是否还在内容区域内
        let is_inside = position.x >= bounds.left()
            && position.x <= bounds.right()
            && position.y >= bounds.top()
            && position.y <= bounds.bottom();

        if !is_inside {
            // 鼠标离开内容区域，清除预览
            if self.drag_preview.is_some() {
                self.drag_preview = None;
                cx.emit(PanelEvent::DragPreview {
                    panel_id: self.panel_id.clone(),
                    placement: None,
                });
                cx.notify();
            }
            return;
        }

        let preview = self.calculate_placement(&bounds, position);

        if preview != self.drag_preview {
            self.drag_preview = preview;

            // 发送事件给父组件
            cx.emit(PanelEvent::DragPreview {
                panel_id: self.panel_id.clone(),
                placement: preview,
            });

            cx.notify();
        }
    }

    /// 处理Panel放置事件
    fn handle_panel_drop(&mut self, dragged: &DraggedPanel, cx: &mut Context<Self>) {
        // 不处理自己拖拽到自己
        if dragged.panel_id == self.panel_id {
            return;
        }

        // 如果有preview就使用它，否则默认使用Right
        let placement = self.drag_preview.unwrap_or(PanelPlacement::Right);

        // 发送Panel放置事件
        cx.emit(PanelEvent::PanelDrop {
            target_panel_id: self.panel_id.clone(),
            dragged_panel: dragged.clone(),
            placement,
        });

        self.drag_preview = None;
        cx.notify();
    }
}

impl Render for Panel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_solo = self.is_solo;
        let is_selected = self.is_selected;

        div()
            .id(format!("panel-{}", self.panel_id))
            .size_full()
            // 所有Panel使用一致的背景色
            .bg(rgb(0x0f0f1a))
            .when(!is_solo, |this| {
                // 选中时边框颜色加深，未选中时使用浅色边框
                let border_color = if is_selected {
                    rgb(0x5d5d7c) // 浅色边框（选中状态）
                } else {
                    rgb(0x2d2d44) // 深色边框（未选中状态）
                };
                this.border_1().border_color(border_color)
            })
            .rounded(px(8.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            // 点击选中panel - 发送事件给Workspace统一管理
            .on_click(
                cx.listener(move |this, _event: &gpui::ClickEvent, _window, cx| {
                    if !this.is_selected {
                        cx.emit(PanelEvent::Selected {
                            panel_id: this.panel_id.clone(),
                        });
                    }
                }),
            )
            // 标题栏 - 只在非solo状态下显示（一组Panel时）
            .when(!is_solo, |this| this.child(self.render_title_bar(cx)))
            // 内容区域
            .child(self.render_content())
            // 渲染拖拽预览层
            .when(self.drag_preview.is_some(), |this| {
                if let Some(preview) = self.render_drag_preview() {
                    this.child(preview)
                } else {
                    this
                }
            })
            // 拖拽检测 - DraggedTab
            .on_drag_move::<DraggedTab>(cx.listener(move |this, event, _window, cx| {
                this.handle_drag_move(event, cx);
            }))
            // 拖拽检测 - DraggedPanel
            .on_drag_move::<DraggedPanel>(cx.listener(move |this, event, window, cx| {
                this.handle_panel_drag_move(event, window, cx);
            }))
            // 放置处理 - DraggedTab
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _window, cx| {
                this.handle_drop(dragged, cx);
            }))
            // 放置处理 - DraggedPanel
            .on_drop(
                cx.listener(move |this, dragged: &DraggedPanel, _window, cx| {
                    this.handle_panel_drop(dragged, cx);
                }),
            )
            // 添加一个通用的on_drop来捕获任何类型的拖拽
            .on_drop(cx.listener(move |this, _dragged: &(), _window, _cx| {
                // Generic drop handler
            }))
    }
}
