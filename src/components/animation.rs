use gpui::{
    div, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    Window,
};
use std::time::Duration;

/// 动画配置
#[derive(Clone, Debug)]
pub struct AnimationConfig {
    pub duration: Duration,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_millis(300),
        }
    }
}

/// 面板动画包装器
pub struct AnimatedPanel {
    content: Option<gpui::AnyElement>,
    animation_config: AnimationConfig,
    is_visible: bool,
    opacity: f32,
    focus_handle: FocusHandle,
}

impl AnimatedPanel {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            content: None,
            animation_config: AnimationConfig::default(),
            is_visible: true,
            opacity: 1.0,
            focus_handle,
        })
    }

    pub fn with_content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    /// 显示面板（带淡入效果）
    pub fn show(&mut self, cx: &mut Context<Self>) {
        if !self.is_visible {
            self.is_visible = true;
            self.opacity = 1.0;
            cx.notify();
        }
    }

    /// 隐藏面板（带淡出效果）
    pub fn hide(&mut self, cx: &mut Context<Self>) {
        if self.is_visible {
            self.is_visible = false;
            self.opacity = 0.0;
            cx.notify();
        }
    }

    /// 切换显示/隐藏
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.is_visible {
            self.hide(cx);
        } else {
            self.show(cx);
        }
    }
}

impl Focusable for AnimatedPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AnimatedPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let opacity = self.opacity;

        div()
            .id("animated-panel")
            .size_full()
            .opacity(opacity)
            .children(self.content.take())
    }
}

/// 标签切换动画
pub struct TabSwitchAnimation {
    from_index: usize,
    to_index: usize,
    progress: f32,
    focus_handle: FocusHandle,
}

impl TabSwitchAnimation {
    pub fn new(from: usize, to: usize, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            from_index: from,
            to_index: to,
            progress: 0.0,
            focus_handle,
        })
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.progress = 1.0;
        cx.notify();
    }

    pub fn progress(&self) -> f32 {
        self.progress
    }
}

impl Focusable for TabSwitchAnimation {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TabSwitchAnimation {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("tab-switch-animation")
            .absolute()
            .size_full()
            .opacity(1.0 - self.progress)
    }
}

/// 分屏调整动画
pub struct SplitResizeAnimation {
    target_sizes: Vec<f32>,
    current_sizes: Vec<f32>,
    focus_handle: FocusHandle,
}

impl SplitResizeAnimation {
    pub fn new(target_sizes: Vec<f32>, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            target_sizes: target_sizes.clone(),
            current_sizes: target_sizes,
            focus_handle,
        })
    }

    pub fn animate_to(&mut self, new_sizes: Vec<f32>, cx: &mut Context<Self>) {
        self.target_sizes = new_sizes.clone();
        self.current_sizes = new_sizes;
        cx.notify();
    }

    pub fn current_sizes(&self) -> &Vec<f32> {
        &self.current_sizes
    }
}

impl Focusable for SplitResizeAnimation {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SplitResizeAnimation {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().id("split-resize-animation").absolute().size_full()
    }
}

/// 拖拽预览
pub struct DragPreview {
    is_dragging: bool,
    opacity: f32,
    focus_handle: FocusHandle,
}

impl DragPreview {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            is_dragging: false,
            opacity: 0.0,
            focus_handle,
        })
    }

    pub fn start_drag(&mut self, cx: &mut Context<Self>) {
        self.is_dragging = true;
        self.opacity = 0.8;
        cx.notify();
    }

    pub fn end_drag(&mut self, cx: &mut Context<Self>) {
        self.is_dragging = false;
        self.opacity = 0.0;
        cx.notify();
    }
}

impl Focusable for DragPreview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_dragging {
            return div().into_any_element();
        }

        div()
            .id("drag-preview")
            .absolute()
            .w(px(200.0))
            .h(px(150.0))
            .bg(rgb(0x6c5ce7))
            .opacity(self.opacity)
            .rounded_md()
            .shadow_md()
            .into_any_element()
    }
}

/// 动画工具函数
pub mod utils {
    use super::*;

    /// 创建快速动画配置
    pub fn quick() -> AnimationConfig {
        AnimationConfig {
            duration: Duration::from_millis(150),
        }
    }

    /// 创建慢速动画配置
    pub fn slow() -> AnimationConfig {
        AnimationConfig {
            duration: Duration::from_millis(500),
        }
    }
}
