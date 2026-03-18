use gpui::prelude::*;
use gpui::{AnimationExt, BoxShadow, IntoElement, div, hsla, point, px, rgb};
use std::time::Duration;

/// 动画时长常量
const ANIMATION_DURATION_ENTER_MS: u64 = 450;
const ANIMATION_DURATION_EXIT_MS: u64 = 250;

/// 侧边栏容器组件 - 只负责渲染侧边栏面板本身
/// 内部内容由调用方通过 children 传入
#[derive(Clone)]
pub struct Sidebar {
    width: f32,
}

impl Sidebar {
    /// 创建新的侧边栏容器
    pub fn new() -> Self {
        Self { width: 260.0 }
    }

    /// 设置宽度
    pub fn with_width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// 渲染侧边栏容器，内容由调用方提供
    pub fn render<E: IntoElement>(self, children: E) -> impl IntoElement {
        let width = self.width;

        div()
            .id("sidebar")
            .w(px(width))
            .h_full()
            .rounded(px(12.0))
            .bg(Self::sidebar_background())
            .border_1()
            .border_color(Self::border_color())
            .shadow(vec![BoxShadow {
                color: Self::shadow_color(),
                offset: point(px(2.), px(4.)),
                blur_radius: px(20.),
                spread_radius: px(-4.),
            }])
            .p(px(6.0))
            .flex()
            .flex_col()
            .child(children)
    }

    /// 渲染空的侧边栏容器
    pub fn render_empty(self) -> impl IntoElement {
        self.render(div())
    }

    /// 渲染带入场/退出动画的侧边栏容器
    /// 根据 visible 参数决定是入场还是退出动画
    pub fn render_with_toggle_animation<E: IntoElement + 'static>(
        self,
        animation_id: impl Into<gpui::ElementId>,
        visible: bool,
        children: E,
    ) -> impl IntoElement {
        let target_width = self.width;

        if visible {
            // 入场动画：弹性效果，带边框和阴影
            div()
                .id("sidebar-toggle-enter")
                .h_full()
                .rounded(px(12.0))
                .bg(Self::sidebar_background())
                .border_1()
                .border_color(Self::border_color())
                .shadow(vec![BoxShadow {
                    color: Self::shadow_color(),
                    offset: point(px(2.), px(4.)),
                    blur_radius: px(20.),
                    spread_radius: px(-4.),
                }])
                .overflow_hidden()
                .child(
                    div()
                        .w(px(target_width))
                        .h_full()
                        .p(px(6.0))
                        .child(children),
                )
                .with_animation(
                    animation_id,
                    gpui::Animation::new(Duration::from_millis(ANIMATION_DURATION_ENTER_MS))
                        .with_easing(Self::spring_easing()),
                    move |this, delta| {
                        let overshoot = if delta > 0.8 {
                            let t = (delta - 0.8) / 0.2;
                            1.0 + (0.015 * (1.0 - t) * (std::f32::consts::PI * 1.2).sin())
                        } else {
                            let t = delta / 0.8;
                            1.0 - (1.0 - t).powi(5)
                        };
                        let current_width = target_width * overshoot.min(1.02);
                        this.w(px(current_width))
                    },
                )
        } else {
            // 退出动画：简化样式，移除阴影和边框防止抖动
            div()
                .id("sidebar-toggle-exit")
                .h_full()
                .rounded(px(12.0))
                .bg(Self::sidebar_background())
                .overflow_hidden()
                .child(
                    div()
                        .w(px(target_width))
                        .h_full()
                        .p(px(6.0))
                        .child(children),
                )
                .with_animation(
                    animation_id,
                    gpui::Animation::new(Duration::from_millis(ANIMATION_DURATION_EXIT_MS))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| {
                        let width_factor = 1.0 - delta;
                        let current_width = if delta >= 0.95 {
                            0.0
                        } else {
                            (target_width * width_factor).max(2.0)
                        };
                        this.w(px(current_width))
                    },
                )
        }
    }

    /// macOS 风格的弹性缓动函数
    /// 模拟弹簧效果：快速开始，稍微超过目标，然后回弹稳定
    /// 注意：返回值必须严格在 [0, 1] 范围内（GPUI 要求）
    fn spring_easing() -> impl Fn(f32) -> f32 {
        move |t| {
            // 简化的弹簧效果 - 轻微回弹
            // 使用 ease-out 基础上添加轻微回弹
            let result = if t < 0.8 {
                // 主展开阶段 - 快速加速 (0-80%)
                let normalized = t / 0.8;
                normalized.powi(2) * (3.0 - 2.0 * normalized)
            } else if t < 0.92 {
                // 超调阶段 - 轻微超过目标 (80-92%)
                let normalized = (t - 0.8) / 0.12;
                1.0 + 0.015 * (1.0 - normalized) * (std::f32::consts::PI * 0.4).sin()
            } else {
                // 稳定阶段 - 回弹到 1.0 (92-100%)
                let normalized = (t - 0.92) / 0.08;
                1.0 + 0.015 * (1.0 - normalized) * (std::f32::consts::PI * 0.25).sin()
            };
            // 确保返回值严格在 [0, 1] 范围内
            result.clamp(0.0, 1.0)
        }
    }

    // 颜色定义 - 完全复刻 Hosts 卡片样式
    /// 侧边栏背景 - 复刻 Hosts 卡片: rgb(0x1a1a2e)
    pub fn sidebar_background() -> gpui::Hsla {
        rgb(0x1a1a2e).into()
    }

    /// 侧边栏边框 - 复刻 Hosts 卡片: rgb(0x2d2d44)
    pub fn border_color() -> gpui::Hsla {
        rgb(0x2d2d44).into()
    }

    /// 侧边栏悬停背景 - 复刻 Hosts 卡片 hover: rgb(0x252538)
    pub fn hover_background() -> gpui::Hsla {
        rgb(0x252538).into()
    }

    pub fn selected_background() -> gpui::Hsla {
        // 选中背景 - 类似 macOS 蓝色但适配深色主题
        hsla(211.0 / 360.0, 1.0, 0.5, 0.25)
    }

    pub fn selected_text_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 1.0)
    }

    pub fn section_title_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 0.40)
    }

    pub fn text_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 0.92)
    }

    pub fn text_color_secondary() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 0.55)
    }

    pub fn badge_background() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 0.15)
    }

    pub fn badge_text_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 0.70)
    }

    pub fn shadow_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 0.0, 0.15)
    }

    /// 默认阴影配置 - 供外部组件复用
    pub fn default_box_shadow() -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: Self::shadow_color(),
            offset: point(px(2.), px(4.)),
            blur_radius: px(20.),
            spread_radius: px(-4.),
        }]
    }
}

impl Default for Sidebar {
    fn default() -> Self {
        Self::new()
    }
}
