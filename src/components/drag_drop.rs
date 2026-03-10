use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Pixels, Render, Window, div, px, rgb,
};

/// 拖拽状态
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DragState {
    Idle,
    Dragging,
    Dropping,
}

/// 拖拽区域类型
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropZone {
    None,
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

/// 拖拽上下文
pub struct DragContext {
    pub state: DragState,
    pub source_id: Option<String>,
    pub drop_zone: DropZone,
    pub drag_start_pos: Option<gpui::Point<Pixels>>,
    pub current_pos: Option<gpui::Point<Pixels>>,
}

impl Default for DragContext {
    fn default() -> Self {
        Self {
            state: DragState::Idle,
            source_id: None,
            drop_zone: DropZone::None,
            drag_start_pos: None,
            current_pos: None,
        }
    }
}

/// 可拖拽面板包装器
pub struct DraggablePanel {
    panel_id: String,
    content: Option<gpui::AnyElement>,
    drag_context: DragContext,
    on_drag_start:
        Option<Box<dyn Fn(String, gpui::Point<Pixels>, &mut Window, &mut App) + Send + Sync>>,
    on_drag_end: Option<Box<dyn Fn(String, DropZone, &mut Window, &mut App) + Send + Sync>>,
    focus_handle: FocusHandle,
}

impl DraggablePanel {
    pub fn new(panel_id: impl Into<String>, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            panel_id: panel_id.into(),
            content: None,
            drag_context: DragContext::default(),
            on_drag_start: None,
            on_drag_end: None,
            focus_handle,
        })
    }

    pub fn with_content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    pub fn on_drag_start<F>(mut self, callback: F) -> Self
    where
        F: Fn(String, gpui::Point<Pixels>, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_drag_start = Some(Box::new(callback));
        self
    }

    pub fn on_drag_end<F>(mut self, callback: F) -> Self
    where
        F: Fn(String, DropZone, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_drag_end = Some(Box::new(callback));
        self
    }

    fn handle_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == gpui::MouseButton::Left {
            self.drag_context.state = DragState::Dragging;
            self.drag_context.drag_start_pos = Some(event.position);
            self.drag_context.source_id = Some(self.panel_id.clone());

            if let Some(ref callback) = self.on_drag_start {
                callback(self.panel_id.clone(), event.position, _window, cx);
            }
            cx.notify();
        }
    }

    fn handle_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_context.state == DragState::Dragging {
            self.drag_context.current_pos = Some(event.position);
            cx.notify();
        }
    }

    fn handle_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_context.state == DragState::Dragging {
            self.drag_context.state = DragState::Idle;

            if let Some(ref callback) = self.on_drag_end {
                callback(
                    self.panel_id.clone(),
                    self.drag_context.drop_zone,
                    _window,
                    cx,
                );
            }

            self.drag_context.drag_start_pos = None;
            self.drag_context.current_pos = None;
            self.drag_context.drop_zone = DropZone::None;
            cx.notify();
        }
    }

    pub fn set_drop_zone(&mut self, zone: DropZone, cx: &mut Context<Self>) {
        self.drag_context.drop_zone = zone;
        cx.notify();
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_context.state == DragState::Dragging
    }
}

impl Focusable for DraggablePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DraggablePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dragging = self.is_dragging();
        let panel_id = self.panel_id.clone();

        div()
            .id(format!("draggable-panel-{}", panel_id))
            .size_full()
            .when(is_dragging, |this| this.opacity(0.7).cursor_grabbing())
            .when(!is_dragging, |this| this.cursor_grab())
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(Self::handle_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .children(self.content.take())
    }
}

/// 拖放区域指示器
pub struct DropZoneIndicator {
    zone: DropZone,
    is_active: bool,
    focus_handle: FocusHandle,
}

impl DropZoneIndicator {
    pub fn new(zone: DropZone, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            zone,
            is_active: false,
            focus_handle,
        })
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.is_active = active;
        cx.notify();
    }
}

impl Focusable for DropZoneIndicator {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DropZoneIndicator {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let zone = self.zone;
        let is_active = self.is_active;

        div()
            .id(format!("drop-zone-{:?}", zone))
            .absolute()
            .bg(if is_active {
                rgb(0x6c5ce7)
            } else {
                rgb(0x2d2d44)
            })
            .opacity(if is_active { 0.5 } else { 0.0 })
            .when(zone == DropZone::Left, |this| {
                this.left(px(0.0)).top(px(0.0)).bottom(px(0.0)).w(px(100.0))
            })
            .when(zone == DropZone::Right, |this| {
                this.right(px(0.0))
                    .top(px(0.0))
                    .bottom(px(0.0))
                    .w(px(100.0))
            })
            .when(zone == DropZone::Top, |this| {
                this.top(px(0.0)).left(px(0.0)).right(px(0.0)).h(px(100.0))
            })
            .when(zone == DropZone::Bottom, |this| {
                this.bottom(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .h(px(100.0))
            })
            .when(zone == DropZone::Center, |this| this.inset(px(100.0)))
    }
}

/// 拖拽管理器
pub struct DragManager {
    drag_context: DragContext,
    drop_zones: Vec<Entity<DropZoneIndicator>>,
    focus_handle: FocusHandle,
}

impl DragManager {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        // 创建拖放区域指示器
        let drop_zones = vec![
            DropZoneIndicator::new(DropZone::Left, cx),
            DropZoneIndicator::new(DropZone::Right, cx),
            DropZoneIndicator::new(DropZone::Top, cx),
            DropZoneIndicator::new(DropZone::Bottom, cx),
            DropZoneIndicator::new(DropZone::Center, cx),
        ];

        cx.new(|_cx| Self {
            drag_context: DragContext::default(),
            drop_zones,
            focus_handle,
        })
    }

    pub fn start_drag(
        &mut self,
        panel_id: String,
        pos: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.drag_context.state = DragState::Dragging;
        self.drag_context.source_id = Some(panel_id);
        self.drag_context.drag_start_pos = Some(pos);
        cx.notify();
    }

    pub fn update_drag(&mut self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        self.drag_context.current_pos = Some(pos);

        // 根据位置计算拖放区域
        let zone = self.calculate_drop_zone(pos);
        self.drag_context.drop_zone = zone;

        // 更新区域指示器
        for zone_indicator in &self.drop_zones {
            zone_indicator.update(cx, |indicator, cx| {
                indicator.set_active(indicator.zone == zone, cx);
            });
        }

        cx.notify();
    }

    pub fn end_drag(&mut self, cx: &mut Context<Self>) -> Option<(String, DropZone)> {
        let result = self
            .drag_context
            .source_id
            .clone()
            .map(|id| (id, self.drag_context.drop_zone));

        self.drag_context = DragContext::default();

        // 重置所有区域指示器
        for zone_indicator in &self.drop_zones {
            zone_indicator.update(cx, |indicator, cx| {
                indicator.set_active(false, cx);
            });
        }

        cx.notify();
        result
    }

    fn calculate_drop_zone(&self, pos: gpui::Point<Pixels>) -> DropZone {
        // 简化的区域计算逻辑
        // 实际应用中需要根据容器大小计算
        if pos.x < px(100.0) {
            DropZone::Left
        } else if pos.x > px(700.0) {
            DropZone::Right
        } else if pos.y < px(100.0) {
            DropZone::Top
        } else if pos.y > px(500.0) {
            DropZone::Bottom
        } else {
            DropZone::Center
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_context.state == DragState::Dragging
    }
}

impl Focusable for DragManager {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DragManager {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("drag-manager")
            .absolute()
            .size_full()
            .when(self.is_dragging(), |this| {
                this.children(self.drop_zones.clone())
            })
    }
}
