//! 全应用共享的悬停提示气泡（tooltip）。
//!
//! 基于 gpui 原生的 `.tooltip()` 机制：悬停延迟后在光标附近弹出小气泡，
//! 显示图标按钮等无文字控件的功能说明（VSCode 同款交互）。
//!
//! 用法：
//! ```ignore
//! use crate::components::tooltip::Tooltip;
//!
//! div()
//!     .id("my-icon-button")
//!     // ...按钮样式...
//!     .tooltip(Tooltip::text("暂存全部更改"))
//! ```

use gpui::prelude::*;
use gpui::{
    AnyView, App, Context, IntoElement, Render, SharedString, Styled, Window, div, px, relative,
};

use crate::theme::ActiveTheme;

/// 悬停提示气泡视图。
pub struct Tooltip {
    label: SharedString,
}

impl Tooltip {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }

    /// 供 gpui `.tooltip()` 直接使用的构造器。
    ///
    /// 返回的闭包是 `'static` 的，每次弹出时创建新的气泡视图：
    /// `.tooltip(Tooltip::text("暂存全部更改"))`
    pub fn text(label: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let label = label.into();
        move |_window, cx| {
            cx.new(|_| Tooltip {
                label: label.clone(),
            })
            .into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .max_w(px(320.0))
            .px_2()
            .py_1()
            .rounded(px(6.0))
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .shadow(vec![gpui::BoxShadow {
                color: theme.shadow,
                offset: gpui::point(px(0.0), px(4.0)),
                blur_radius: px(12.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .text_size(px(12.0))
            .text_color(theme.text)
            .line_height(relative(1.3))
            .child(self.label.clone())
    }
}
