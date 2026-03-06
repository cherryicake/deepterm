use gpui::{
    div, prelude::*, rgb, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Window,
};

use crate::components::tab_bar::DraggedTab;

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
    /// 关联的 workspace_id
    workspace_id: String,
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

        Self {
            panel_id,
            workspace_id,
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
        }
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
        if position.x < bounds.left() + bounds.size.width * 0.35 {
            Some(PanelPlacement::Left)
        } else if position.x > bounds.left() + bounds.size.width * 0.65 {
            Some(PanelPlacement::Right)
        } else if position.y < bounds.top() + bounds.size.height * 0.35 {
            Some(PanelPlacement::Top)
        } else if position.y > bounds.top() + bounds.size.height * 0.65 {
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

impl Render for Panel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _panel_id = self.panel_id.clone();

        div()
            .id(format!("panel-{}", self.panel_id))
            .size_full()
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x2d2d44))
            .relative()
            // 内容区域背景
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        self.active_tab_id
                            .as_ref()
                            .map(|id| format!("Panel: {} | Tab: {}", self.panel_id, id))
                            .unwrap_or_else(|| format!("Panel: {}", self.panel_id)),
                    ),
            )
            // 渲染拖拽预览层
            .when(self.drag_preview.is_some(), |this| {
                let preview = self.drag_preview.unwrap();
                this.child(div().absolute().bg(rgb(0x6c5ce7)).map(|div| {
                    match preview {
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
                    }
                }))
            })
            // 拖拽检测
            .on_drag_move::<DraggedTab>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<DraggedTab>, _window, cx| {
                    let bounds = event.bounds;
                    let position = event.event.position;

                    // 检查鼠标是否还在内容区域内
                    let is_inside = position.x >= bounds.left()
                        && position.x <= bounds.right()
                        && position.y >= bounds.top()
                        && position.y <= bounds.bottom();

                    if !is_inside {
                        // 鼠标离开内容区域，清除预览
                        if this.drag_preview.is_some() {
                            this.drag_preview = None;
                            println!(
                                "Panel {} drag preview cleared (left content)",
                                this.panel_id
                            );
                            cx.emit(PanelEvent::DragPreview {
                                panel_id: this.panel_id.clone(),
                                placement: None,
                            });
                            cx.notify();
                        }
                        return;
                    }

                    let preview = this.calculate_placement(&bounds, position);

                    if preview != this.drag_preview {
                        this.drag_preview = preview;
                        println!("Panel {} drag preview: {:?}", this.panel_id, preview);

                        // 发送事件给父组件
                        cx.emit(PanelEvent::DragPreview {
                            panel_id: this.panel_id.clone(),
                            placement: preview,
                        });

                        cx.notify();
                    }
                },
            ))
            // 放置处理
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _window, cx| {
                println!(
                    "Panel {} on_drop called, preview={:?}",
                    this.panel_id, this.drag_preview
                );

                // 如果有preview就使用它，否则默认使用Right
                let placement = this.drag_preview.unwrap_or(PanelPlacement::Right);
                println!("Panel {} drop: {:?}", this.panel_id, placement);

                // 发送放置事件
                cx.emit(PanelEvent::Drop {
                    panel_id: this.panel_id.clone(),
                    dragged_tab: dragged.clone(),
                    placement,
                });

                this.drag_preview = None;
                cx.notify();
            }))
            // 添加一个通用的on_drop来捕获任何类型的拖拽
            .on_drop(cx.listener(move |this, _dragged: &(), _window, cx| {
                println!("Panel {} received generic drop event", this.panel_id);
            }))
    }
}
