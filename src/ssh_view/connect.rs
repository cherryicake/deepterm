//! SSH 连接进度页面
//!
//! 在 SSH 连接建立期间显示在终端 Panel 中，提供连接状态和动画反馈。
//! 使用 Stepper 组件展示连接各阶段的进度。

use std::time::Duration;

use gpui::prelude::*;
use gpui::{Context, IntoElement, Render, Styled, Task, Window, div, px, rgb, svg};

use crate::assets::Icon;
use crate::components::stepper::{Stepper, StepperItem};
use crate::ssh::config::SshConnectionConfig;

// 颜色常量 - 现代化协调配色方案
// 背景色系 - 保持原有暗色主题
const COLOR_BG: u32 = 0x0f0f1a;
const COLOR_CARD_BG: u32 = 0x181825;
const COLOR_BORDER: u32 = 0x313244;
const COLOR_ICON_BG: u32 = 0x1a1a2e;

// 文字色系 - 提高对比度
const COLOR_TEXT_PRIMARY: u32 = 0xe4e4e7;
const COLOR_TEXT_SECONDARY: u32 = 0xa1a1aa;
const COLOR_TEXT_MUTED: u32 = 0x71717a;

// 状态色系 - 协调的色调
const COLOR_ACCENT: u32 = 0x60a5fa; // 柔和的蓝色
const COLOR_SUCCESS: u32 = 0x4ade80; // 清新的绿色
const COLOR_ERROR: u32 = 0xf87171; // 柔和的红色

/// 连接阶段总数
const NUM_STAGES: usize = 3;

/// SSH 连接阶段
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectStage {
    /// SSH 握手
    Handshake,
    /// 认证
    Authentication,
    /// 建立会话
    Session,
}

impl ConnectStage {
    /// 获取阶段索引（用于 Stepper）
    fn index(&self) -> usize {
        match self {
            ConnectStage::Handshake => 0,
            ConnectStage::Authentication => 1,
            ConnectStage::Session => 2,
        }
    }

    /// 获取阶段显示名称（与 Stepper 标签一致）
    fn label(&self) -> &'static str {
        match self {
            ConnectStage::Handshake => "SSH",
            ConnectStage::Authentication => "Auth",
            ConnectStage::Session => "Session",
        }
    }

    /// 获取下一个阶段
    pub fn next(&self) -> Option<ConnectStage> {
        match self {
            ConnectStage::Handshake => Some(ConnectStage::Authentication),
            ConnectStage::Authentication => Some(ConnectStage::Session),
            ConnectStage::Session => None,
        }
    }
}

/// SSH 连接状态
#[derive(Clone, Debug)]
pub enum ConnectStatus {
    /// 正在连接中，附带当前阶段
    Connecting { stage: ConnectStage },
    /// 连接成功
    Connected,
    /// 连接失败，附带失败阶段和错误信息
    Failed {
        stage: ConnectStage,
        message: String,
    },
}

/// SSH 连接进度视图
///
/// 在 SSH 终端建立连接期间覆盖在终端 Panel 上的 loading 界面。
/// 使用 Stepper 组件展示连接各阶段的进度。
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
        let animation_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
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
            }
        });

        Self {
            config,
            status: ConnectStatus::Connecting {
                stage: ConnectStage::Handshake,
            },
            frame: 0,
            animation_task,
        }
    }

    /// 更新连接阶段（用于 SSH 库报告进度）
    pub fn set_stage(&mut self, stage: ConnectStage, cx: &mut Context<Self>) {
        self.status = ConnectStatus::Connecting { stage };
        cx.notify();
    }

    /// 将状态更新为连接成功
    pub fn set_connected(&mut self, cx: &mut Context<Self>) {
        self.status = ConnectStatus::Connected;
        cx.notify();
    }

    /// 将状态更新为连接失败
    pub fn set_failed(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        // 默认失败阶段为当前阶段
        let stage = match &self.status {
            ConnectStatus::Connecting { stage } => stage.clone(),
            _ => ConnectStage::Handshake,
        };
        self.status = ConnectStatus::Failed {
            stage,
            message: message.into(),
        };
        cx.notify();
    }

    /// 将状态更新为连接失败（指定失败阶段）
    pub fn set_failed_at(
        &mut self,
        stage: ConnectStage,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.status = ConnectStatus::Failed {
            stage,
            message: message.into(),
        };
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
        let is_connected = matches!(self.status, ConnectStatus::Connected);
        let icon_color = if is_failed {
            COLOR_ERROR
        } else if is_connected {
            COLOR_SUCCESS
        } else {
            COLOR_ACCENT
        };

        // 确定当前选中的步骤索引
        let selected_index = match &self.status {
            ConnectStatus::Connecting { stage } => stage.index(),
            ConnectStatus::Connected => NUM_STAGES, // 全部完成（大于最大索引）
            ConnectStatus::Failed { stage, .. } => stage.index(), // 显示失败时的进度
        };

        let nickname = self.config.nickname.clone();

        div()
            .size_full()
            .bg(rgb(COLOR_BG))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(80.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(24.0))
                    .w(px(420.0))
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
                            .bg(rgb(COLOR_ICON_BG))
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
                        // 主机信息
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
                            }),
                    )
                    .child(
                        // Stepper 进度指示器
                        Stepper::new("connect-stepper")
                            .size(crate::components::stepper::StepperSize::Large)
                            .text_center(true)
                            .selected_index(selected_index)
                            .items([
                                StepperItem::new().child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(rgb(COLOR_TEXT_MUTED))
                                        .child("SSH"),
                                ),
                                StepperItem::new().child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(rgb(COLOR_TEXT_MUTED))
                                        .child("Auth"),
                                ),
                                StepperItem::new().child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(rgb(COLOR_TEXT_MUTED))
                                        .child("Session"),
                                ),
                            ]),
                    )
                    .when(is_failed, |this| match &self.status {
                        ConnectStatus::Failed { message, .. } => this.child(
                            div()
                                .text_color(rgb(COLOR_ERROR))
                                .text_size(px(13.0))
                                .child(format!("Failed: {}", message)),
                        ),
                        _ => this,
                    }),
            )
    }
}
