use std::time::Duration;

use crate::theme::ActiveTheme;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div, px};

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
    is_visible: bool,
    opacity: f32,
    focus_handle: FocusHandle,
}

impl AnimatedPanel {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            content: None,
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
    progress: f32,
    focus_handle: FocusHandle,
}

impl TabSwitchAnimation {
    pub fn new(_from: usize, _to: usize, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            progress: 0.0,
            focus_handle,
        })
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.progress = 1.0;
        cx.notify();
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_dragging {
            return div().into_any_element();
        }
        let theme = cx.theme().ui;

        div()
            .id("drag-preview")
            .absolute()
            .w(px(200.0))
            .h(px(150.0))
            .bg(theme.drop_target)
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

/// macOS 风格缓动曲线。
///
/// macOS 的弹窗 / 菜单动画并非简单的 ease-out：它使用 Core Animation 默认的
/// cubic-bézier 曲线，进入时「先快速铺开、末段轻柔收住」，但不像 `(1-t)^5`
/// 那样在前 20% 就冲到 67% 然后长时间蠕动（那会产生「先弹一下再爬行」的廉价感）。
///
/// 这里实现一个通用的三次贝塞尔求解器，并提供与 macOS 体感一致的几条曲线。
pub mod easing {
    /// 通用三次贝塞尔缓动求解器，等价于 CSS `cubic-bezier(x1, y1, x2, y2)`。
    ///
    /// 控制点 P0=(0,0)、P3=(1,1) 固定，传入 P1=(x1,y1)、P2=(x2,y2)。
    /// 给定时间进度 `t`（0..=1），先用牛顿迭代 + 二分回退由 x 反解贝塞尔参数 s，
    /// 再代入 y 曲线得到输出进度。返回值钳制在 [0,1]，满足 GPUI 约束。
    pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32) -> impl Fn(f32) -> f32 {
        move |t: f32| {
            let t = t.clamp(0.0, 1.0);
            if t <= 0.0 {
                return 0.0;
            }
            if t >= 1.0 {
                return 1.0;
            }
            let s = solve_bezier_x(t, x1, x2);
            bezier_axis(s, y1, y2).clamp(0.0, 1.0)
        }
    }

    /// 由目标 x 反解贝塞尔参数 s ∈ [0,1]。
    fn solve_bezier_x(x: f32, x1: f32, x2: f32) -> f32 {
        // 牛顿迭代：收敛快，绝大多数情况下 4 次足够。
        let mut s = x;
        for _ in 0..5 {
            let x_est = bezier_axis(s, x1, x2) - x;
            if x_est.abs() < 1e-4 {
                return s;
            }
            let d = bezier_axis_derivative(s, x1, x2);
            if d.abs() < 1e-6 {
                break;
            }
            s -= x_est / d;
        }
        // 牛顿失败时二分回退，保证单调有界。
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        s = x;
        for _ in 0..20 {
            let x_est = bezier_axis(s, x1, x2);
            if (x_est - x).abs() < 1e-4 {
                break;
            }
            if x_est < x {
                lo = s;
            } else {
                hi = s;
            }
            s = (lo + hi) * 0.5;
        }
        s
    }

    /// 单轴三次贝塞尔取值：P0=0、P3=1，控制点 c1、c2。
    #[inline]
    fn bezier_axis(s: f32, c1: f32, c2: f32) -> f32 {
        let u = 1.0 - s;
        // 3u²s·c1 + 3us²·c2 + s³
        3.0 * u * u * s * c1 + 3.0 * u * s * s * c2 + s * s * s
    }

    /// 单轴三次贝塞尔导数，供牛顿迭代使用。
    #[inline]
    fn bezier_axis_derivative(s: f32, c1: f32, c2: f32) -> f32 {
        let u = 1.0 - s;
        3.0 * u * u * c1 + 6.0 * u * s * (c2 - c1) + 3.0 * s * s * (1.0 - c2)
    }

    /// 进入曲线：贴近 macOS sheet / 通知的弹出手感。
    /// `cubic-bezier(0.32, 0.72, 0, 1)` —— 起步利落、中段顺滑、末段平稳停住，
    /// 没有 `(1-t)^5` 那种长尾蠕动。这是 Apple 在 SwiftUI / iOS sheet 中常用的曲线。
    pub fn macos_enter() -> impl Fn(f32) -> f32 {
        cubic_bezier(0.32, 0.72, 0.0, 1.0)
    }

    /// 退出曲线：`cubic-bezier(0.4, 0.0, 1.0, 1.0)`（标准 ease-in 加速收拢），
    /// 起步轻、收尾快，制造「被吸走」的干脆感。
    pub fn macos_exit() -> impl Fn(f32) -> f32 {
        cubic_bezier(0.4, 0.0, 1.0, 1.0)
    }

    /// 标准对称平滑曲线 `cubic-bezier(0.4, 0.0, 0.2, 1.0)`（Material/通用 standard），
    /// 用于不需要强调方向性的属性过渡。
    pub fn standard() -> impl Fn(f32) -> f32 {
        cubic_bezier(0.4, 0.0, 0.2, 1.0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn curves() -> Vec<Box<dyn Fn(f32) -> f32>> {
            vec![
                Box::new(macos_enter()),
                Box::new(macos_exit()),
                Box::new(standard()),
            ]
        }

        /// 端点必须精确命中 (0,0) 与 (1,1)，否则动画首尾会跳变。
        #[test]
        fn endpoints_are_exact() {
            for f in curves() {
                assert!(f(0.0).abs() < 1e-3, "f(0) should be 0");
                assert!((f(1.0) - 1.0).abs() < 1e-3, "f(1) should be 1");
            }
        }

        /// 这些曲线的控制点都在 [0,1]，输出应单调不减且不越界。
        #[test]
        fn monotonic_and_bounded() {
            for f in curves() {
                let mut prev = f(0.0);
                let mut t = 0.0;
                while t <= 1.0 {
                    let v = f(t);
                    assert!((0.0..=1.0).contains(&v), "value {v} out of range at t={t}");
                    assert!(v + 1e-3 >= prev, "not monotonic at t={t}: {v} < {prev}");
                    prev = v;
                    t += 0.01;
                }
            }
        }

        /// 进入曲线应「前段快」：到时间 30% 时已完成过半进度。
        #[test]
        fn enter_front_loads_progress() {
            let f = macos_enter();
            assert!(f(0.3) > 0.5, "enter curve should pass 0.5 by t=0.3, got {}", f(0.3));
        }

        /// x 反解精度：对线性对角线 cubic-bezier(t)=t 的近似应足够准。
        #[test]
        fn solver_accuracy_on_diagonal() {
            let f = cubic_bezier(0.33, 0.33, 0.66, 0.66);
            for i in 0..=10 {
                let t = i as f32 / 10.0;
                assert!((f(t) - t).abs() < 5e-3, "diagonal mismatch at {t}: {}", f(t));
            }
        }
    }
}

