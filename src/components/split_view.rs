use gpui::prelude::*;
use gpui::{
    AnyElement, App, Axis, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window,
    div,
};

/// 分割方向
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitDirection {
    Horizontal, // 水平分割 - 左右
    Vertical,   // 垂直分割 - 上下
}

impl SplitDirection {
    pub fn axis(&self) -> Axis {
        match self {
            SplitDirection::Horizontal => Axis::Horizontal,
            SplitDirection::Vertical => Axis::Vertical,
        }
    }
}

/// 分屏视图组件
pub struct SplitView {
    direction: SplitDirection,
    children: Vec<AnyElement>,
    sizes: Vec<f32>,
    focus_handle: FocusHandle,
}

impl SplitView {
    pub fn new(direction: SplitDirection, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            direction,
            children: Vec::new(),
            sizes: Vec::new(),
            focus_handle,
        })
    }

    pub fn add_panel(&mut self, panel: impl IntoElement, cx: &mut Context<Self>) {
        self.children.push(panel.into_any_element());
        self.recalculate_sizes();
        cx.notify();
    }

    fn recalculate_sizes(&mut self) {
        let count = self.children.len();
        if count > 0 {
            let equal_size = 1.0 / count as f32;
            self.sizes = vec![equal_size; count];
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.children.clear();
        self.sizes.clear();
        cx.notify();
    }

    pub fn panel_count(&self) -> usize {
        self.children.len()
    }
}

impl Focusable for SplitView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SplitView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let direction = self.direction;

        div()
            .id("split-view")
            .size_full()
            .flex()
            .when(direction == SplitDirection::Horizontal, |this| {
                this.flex_row()
            })
            .when(direction == SplitDirection::Vertical, |this| {
                this.flex_col()
            })
            .children(self.children.drain(..))
    }
}
