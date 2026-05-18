use std::time::Duration;

use gpui::prelude::*;
use gpui::{AnimationExt, IntoElement, div, px};

use crate::theme::ActiveTheme;

const SIDEBAR_WIDTH: f32 = 260.0;
const ANIMATION_ENTER_MS: u64 = 300;
const ANIMATION_EXIT_MS: u64 = 250;

/// macOS 26 风格空侧边栏
///
/// 全高、通透毛玻璃背景、精致高亮边框。
#[derive(Clone)]
pub struct MacosSidebar;

impl MacosSidebar {
    pub fn new() -> Self {
        Self
    }

    /// 渲染带入场/退出动画的侧边栏
    pub fn render_animated(self, visible: bool, epoch: u32, cx: &gpui::App) -> impl IntoElement {
        let target_width = SIDEBAR_WIDTH;
        let is_dark = cx.theme().appearance.is_dark();

        // 毛玻璃背景：使用比主背景略亮/略深的半透明色
        let glass_bg = if is_dark {
            gpui::Hsla::from(gpui::rgb(0x2C2C2E)).opacity(0.85)
        } else {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.85)
        };

        // 高亮边框：提升悬浮感
        let border_color = if is_dark {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.14)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.08)
        };

        let inner = div()
            .w(px(target_width))
            .h_full()
            .flex()
            .flex_col()
            .pt(px(20.0))
            .pb(px(16.0))
            .pl(px(16.0))
            .pr(px(16.0))
            // 空内容区域
            .child(div().flex_1());

        if visible {
            div()
                .id(("macos-sidebar-enter", epoch))
                .h_full()
                .rounded(px(12.0))
                .bg(glass_bg)
                .border(px(0.5))
                .border_color(border_color)
                .overflow_hidden()
                .child(inner)
                .with_animation(
                    ("sidebar-enter", epoch),
                    gpui::Animation::new(Duration::from_millis(ANIMATION_ENTER_MS))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| {
                        let width = target_width * delta;
                        this.w(px(width))
                    },
                )
        } else {
            div()
                .id(("macos-sidebar-exit", epoch))
                .h_full()
                .rounded(px(12.0))
                .bg(glass_bg)
                .border(px(0.5))
                .border_color(border_color)
                .overflow_hidden()
                .child(inner)
                .with_animation(
                    ("sidebar-exit", epoch),
                    gpui::Animation::new(Duration::from_millis(ANIMATION_EXIT_MS))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| {
                        let width = target_width * (1.0 - delta);
                        this.w(px(width.max(0.0)))
                    },
                )
        }
    }
}

impl Default for MacosSidebar {
    fn default() -> Self {
        Self::new()
    }
}
