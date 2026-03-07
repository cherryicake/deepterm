use gpui::{
    App, Axis, Context, Entity, FocusHandle, Focusable, IntoElement, Pixels, Render, Window, div,
    prelude::*, px, rgb,
};

/// 可调整大小的拖拽句柄
pub struct ResizeHandle {
    axis: Axis,
    on_resize: Option<Box<dyn Fn(Pixels, &mut Window, &mut App) + Send + Sync>>,
    is_dragging: bool,
    drag_start_pos: Option<Pixels>,
    focus_handle: FocusHandle,
}

impl ResizeHandle {
    pub fn new(axis: Axis, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            axis,
            on_resize: None,
            is_dragging: false,
            drag_start_pos: None,
            focus_handle,
        })
    }

    pub fn on_resize<F>(mut self, callback: F) -> Self
    where
        F: Fn(Pixels, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_resize = Some(Box::new(callback));
        self
    }

    fn handle_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == gpui::MouseButton::Left {
            self.is_dragging = true;
            self.drag_start_pos = Some(match self.axis {
                Axis::Horizontal => event.position.x,
                Axis::Vertical => event.position.y,
            });
            cx.notify();
        }
    }

    fn handle_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_dragging {
            let current_pos = match self.axis {
                Axis::Horizontal => event.position.x,
                Axis::Vertical => event.position.y,
            };

            if let Some(start_pos) = self.drag_start_pos {
                let delta = current_pos - start_pos;
                if let Some(ref callback) = self.on_resize {
                    callback(delta, window, cx);
                }
                self.drag_start_pos = Some(current_pos);
            }
        }
    }

    fn handle_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_dragging = false;
        self.drag_start_pos = None;
        cx.notify();
    }
}

impl Focusable for ResizeHandle {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResizeHandle {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let axis = self.axis;
        let is_dragging = self.is_dragging;

        div()
            .id("resize-handle")
            .bg(if is_dragging {
                rgb(0x6c5ce7)
            } else {
                rgb(0x2d2d44)
            })
            .when(axis == Axis::Horizontal, |this| {
                this.w(px(4.0)).h_full().cursor_col_resize()
            })
            .when(axis == Axis::Vertical, |this| {
                this.h(px(4.0)).w_full().cursor_row_resize()
            })
            .hover(|style| style.bg(rgb(0x6c5ce7)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(Self::handle_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::handle_mouse_up))
    }
}
