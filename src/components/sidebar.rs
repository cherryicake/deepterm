use gpui::prelude::*;
use gpui::{BoxShadow, IntoElement, div, hsla, point, px};

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

    // 颜色定义 - 公开供外部使用
    pub fn sidebar_background() -> gpui::Hsla {
        hsla(240.0 / 360.0, 0.33, 0.08, 0.85)
    }

    pub fn border_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 0.05)
    }

    pub fn shadow_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 0.0, 0.15)
    }

    pub fn selected_background() -> gpui::Hsla {
        hsla(210.0 / 360.0, 0.80, 0.55, 0.35)
    }

    pub fn selected_text_color() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 1.0)
    }

    pub fn hover_background() -> gpui::Hsla {
        hsla(0.0 / 360.0, 0.0, 1.0, 0.08)
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
