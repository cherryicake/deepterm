use gpui::{
    div, prelude::*, px, rgb, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, SharedString, Styled, Window,
};

use crate::components::tab_bar::DraggedTab;
use crate::terminal_view::terminal_view::TerminalView;

/// 拖拽阈值常量
const DRAG_THRESHOLD_START: f32 = 0.35;
const DRAG_THRESHOLD_END: f32 = 0.65;

/// 面板放置位置
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// 放置事件
    Drop {
        panel_id: String,
        dragged_tab: DraggedTab,
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
}

/// Panel content type
#[derive(Clone, Debug)]
pub enum PanelContent {
    /// Empty panel with just text
    Text(String),
    /// Terminal panel
    Terminal(Entity<TerminalView>),
}

/// 单个面板视图 - 只包含内容区域，不包含TabBar
/// TabBar由Workspace统一管理
pub struct Panel {
    panel_id: String,
    focus_handle: FocusHandle,
    /// 当前拖拽预览位置
    drag_preview: Option<PanelPlacement>,
    /// 当前活动 tab ID
    active_tab_id: Option<SharedString>,
    /// 当前活动 tab 标签名（用于标题栏显示）
    active_tab_label: Option<SharedString>,
    /// 关联的 workspace_id
    workspace_id: String,
    /// 是否是工作区中唯一的panel（不渲染边框）
    is_solo: bool,
    /// 是否被选中（点击后边框加深）
    is_selected: bool,
    /// Panel content
    content: PanelContent,
}

impl Panel {
    /// 创建新的Panel
    /// panel_id: 面板唯一标识
    /// workspace_id: 所属workspace组的标识
    /// active_tab_id: 当前活动的tab id
    pub fn new(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let workspace_id = workspace_id.into();

        let panel_id_clone = panel_id.clone();
        Self {
            panel_id,
            workspace_id,
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label: None,
            is_solo: false,
            is_selected: false,
            content: PanelContent::Text(format!("Panel: {}", panel_id_clone)),
        }
    }

    /// 创建带终端的Panel
    pub fn with_terminal(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let workspace_id = workspace_id.into();
        let terminal_view = TerminalView::new(cx);

        Self {
            panel_id: panel_id.clone(),
            workspace_id,
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label,
            is_solo: false,
            is_selected: false,
            content: PanelContent::Terminal(terminal_view),
        }
    }

    /// 设置panel内容
    pub fn set_content(&mut self, content: PanelContent, cx: &mut Context<Self>) {
        self.content = content;
        cx.notify();
    }

    /// 获取panel内容
    pub fn content(&self) -> &PanelContent {
        &self.content
    }

    /// 检查是否是终端内容
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
        if position.x < bounds.left() + bounds.size.width * DRAG_THRESHOLD_START {
            Some(PanelPlacement::Left)
        } else if position.x > bounds.left() + bounds.size.width * DRAG_THRESHOLD_END {
            Some(PanelPlacement::Right)
        } else if position.y < bounds.top() + bounds.size.height * DRAG_THRESHOLD_START {
            Some(PanelPlacement::Top)
        } else if position.y > bounds.top() + bounds.size.height * DRAG_THRESHOLD_END {
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

impl EventEmitter<PanelEvent> for Panel {}

impl Panel {
    /// 渲染标题栏
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_id = self.panel_id.clone();
        // 使用Tab标签名作为标题，如果没有则使用Tab ID，最后使用Panel ID
        let title = self
            .active_tab_label
            .as_ref()
            .map(|label| label.to_string())
            .or_else(|| self.active_tab_id.as_ref().map(|id| id.to_string()))
            .unwrap_or_else(|| self.panel_id.clone());

        div()
            .h(px(28.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .child(
                // 标题 - 显示Tab标签名（使用更小的字体）
                div().text_xs().text_color(rgb(0x8888aa)).child(title),
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
                        cx.listener({
                            let panel_id = panel_id.clone();
                            move |_this, _event, _window, cx| {
                                println!("Panel {} close button clicked", panel_id);
                                cx.emit(PanelEvent::Close {
                                    panel_id: panel_id.clone(),
                                });
                            }
                        }),
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
            PanelContent::Terminal(terminal_view) => {
                div()
                    .flex_1()
                    .min_h(px(100.0)) // Ensure minimum height
                    .child(terminal_view.clone())
            }
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
                println!(
                    "Panel {} drag preview cleared (left content)",
                    self.panel_id
                );
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
            println!("Panel {} drag preview: {:?}", self.panel_id, preview);

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
        println!(
            "Panel {} on_drop called, preview={:?}",
            self.panel_id, self.drag_preview
        );

        // 如果有preview就使用它，否则默认使用Right
        let placement = self.drag_preview.unwrap_or(PanelPlacement::Right);
        println!("Panel {} drop: {:?}", self.panel_id, placement);

        // 发送放置事件
        cx.emit(PanelEvent::Drop {
            panel_id: self.panel_id.clone(),
            dragged_tab: dragged.clone(),
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
                        println!("Panel {} clicked, requesting selection", this.panel_id);
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
            // 拖拽检测
            .on_drag_move::<DraggedTab>(cx.listener(move |this, event, _window, cx| {
                this.handle_drag_move(event, cx);
            }))
            // 放置处理
            .on_drop(cx.listener(move |this, dragged, _window, cx| {
                this.handle_drop(dragged, cx);
            }))
            // 添加一个通用的on_drop来捕获任何类型的拖拽
            .on_drop(cx.listener(move |this, _dragged: &(), _window, _cx| {
                println!("Panel {} received generic drop event", this.panel_id);
            }))
    }
}
