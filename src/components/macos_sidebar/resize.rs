use super::*;

/// 全局 resize 处理元素 - 在 paint 阶段注册全局鼠标事件
pub(super) struct SidebarResizeElement {
    pub(super) sidebar: WeakEntity<MacosSidebar>,
}

impl Element for SidebarResizeElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(gpui::Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) -> () {
        ()
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut (),
        _prepaint_state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(sidebar) = self.sidebar.upgrade() else {
            return;
        };

        // 拖拽中全局覆盖光标样式，保持 resize 光标不变
        if sidebar.read(cx).is_dragging() {
            window.set_window_cursor_style(CursorStyle::ResizeColumn);
        }

        // 全局鼠标移动事件 - 处理 resize 拖拽
        window.on_mouse_event({
            let sidebar = sidebar.clone();
            move |event: &MouseMoveEvent, _, _window, cx| {
                sidebar.update(cx, |this, cx| {
                    if this.is_dragging() {
                        this.update_resize(event.position.x);
                        cx.notify();
                        this.notify_app_window(cx);
                    }
                });
            }
        });

        // 全局鼠标释放事件 - 结束 resize 拖拽
        window.on_mouse_event({
            let sidebar = sidebar.clone();
            move |_: &MouseUpEvent, _, _, cx| {
                sidebar.update(cx, |this, cx| {
                    if this.is_dragging() {
                        this.end_resize();
                        cx.notify();
                        this.notify_app_window(cx);
                    }
                });
            }
        });
    }
}

impl IntoElement for SidebarResizeElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}
