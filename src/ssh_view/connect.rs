//! SSH 连接进度页面
//!
//! 在 SSH 连接建立期间显示在终端 Panel 中，提供连接状态和动画反馈。

use std::time::Duration;

use gpui::prelude::*;
use gpui::{Context, IntoElement, Render, Styled, Task, Window, div, px, rgb, svg};

use crate::assets::Icon;
use crate::ssh::config::SshConnectionConfig;

// 颜色常量（与 picker.rs 保持一致的暗色主题）
const COLOR_BG: u32 = 0x0f0f1a;
const COLOR_CARD_BG: u32 = 0x181825;
const COLOR_BORDER: u32 = 0x313244;
const COLOR_TEXT_PRIMARY: u32 = 0x9aa3b8;
const COLOR_TEXT_SECONDARY: u32 = 0x5d5d7c;
const COLOR_ACCENT: u32 = 0x4a9eff;
const COLOR_ERROR: u32 = 0xff5f57;

/// SSH 连接状态
#[derive(Clone, Debug)]
pub enum ConnectStatus {
    /// 正在连接中
    Connecting,
    /// 连接失败，附带错误信息
    Failed { message: String },
}

/// SSH 连接进度视图
///
/// 在 SSH 终端建立连接期间覆盖在终端 Panel 上的 loading 界面。
/// 支持动画（每 400ms 更新一次点动画）和连接状态显示。
pub struct SshConnectView {
    /// SSH 连接配置（用于显示连接目标信息）
    pub config: SshConnectionConfig,
    /// 当前连接状态
    pub status: ConnectStatus,
    /// 动画帧计数（用于 "..." 点动画）
    frame: u32,
    /// 动画任务（持有此字段使任务保持运行，drop 时自动停止）
    #[allow(dead_code)]
    animation_task: Task<()>,
}

impl SshConnectView {
    pub fn new(config: SshConnectionConfig, cx: &mut Context<Self>) -> Self {
        // 启动动画循环：每 400ms 推进一帧
        let animation_task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let cont = this
                .update(cx, |view, cx| {
                    view.frame = view.frame.wrapping_add(1);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !cont {
                break;
            }
        });

        Self { config, status: ConnectStatus::Connecting, frame: 0, animation_task }
    }

    /// 将状态更新为连接失败
    pub fn set_failed(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status = ConnectStatus::Failed { message: message.into() };
        cx.notify();
    }

    fn spinner_dots(&self) -> &'static str {
        match self.frame % 4 {
            0 => "   ",
            1 => ".  ",
            2 => ".. ",
            _ => "...",
        }
    }
}

impl Render for SshConnectView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let is_failed = matches!(self.status, ConnectStatus::Failed { .. });
        let icon_color = if is_failed { COLOR_ERROR } else { COLOR_ACCENT };

        let status_text = match &self.status {
            ConnectStatus::Connecting => format!("Connecting{}", self.spinner_dots()),
            ConnectStatus::Failed { message } => format!("Connection failed: {}", message),
        };

        let nickname = self.config.nickname.clone();

        div()
            .size_full()
            .bg(rgb(COLOR_BG))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(100.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(20.0))
                    .w(px(320.0))
                    .px(px(32.0))
                    .py(px(36.0))
                    .bg(rgb(COLOR_CARD_BG))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(rgb(COLOR_BORDER))
                    .child(
                        // 图标
                        div()
                            .w(px(56.0))
                            .h(px(56.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(0x1a1a2e))
                            .border_1()
                            .border_color(rgb(COLOR_BORDER))
                            .child(
                                svg()
                                    .size(px(28.0))
                                    .text_color(rgb(icon_color))
                                    .path(Icon::Server.path()),
                            ),
                    )
                    .child(
                        // 主机信息 + 状态
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .text_color(rgb(COLOR_TEXT_PRIMARY))
                                    .text_size(px(15.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(self.config.host_label()),
                            )
                            .when_some(nickname, |this, n| {
                                this.child(
                                    div()
                                        .text_color(rgb(COLOR_TEXT_SECONDARY))
                                        .text_size(px(12.0))
                                        .child(n),
                                )
                            })
                            .child(
                                div()
                                    .text_color(rgb(icon_color))
                                    .text_size(px(13.0))
                                    .child(status_text),
                            ),
                    ),
            )
    }
}
