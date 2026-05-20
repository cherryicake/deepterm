//! SSH 连接进度页面
//!
//! 在 SSH 连接建立期间显示在终端 Panel 中，提供连接状态和动画反馈。
//! 使用 Stepper 组件展示连接各阶段的进度。

use std::time::Duration;

use gpui::prelude::*;
use gpui::{Context, EventEmitter, IntoElement, Render, Styled, Task, Window, div, px, svg};

use crate::components::button::{Button, ButtonVariants};

use crate::assets::Icon;
use crate::components::stepper::{Stepper, StepperItem};
use crate::ssh::config::SshConnectionConfig;
use crate::theme::ActiveTheme;

/// SshConnectView 事件
#[derive(Clone, Debug)]
pub enum Event {
    /// 重新连接
    Reconnect {
        /// 连接配置
        config: SshConnectionConfig,
    },
}

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
    animation_task: Option<Task<()>>,
}

impl SshConnectView {
    pub fn new(config: SshConnectionConfig, cx: &mut Context<Self>) -> Self {
        // 启动动画循环：每 400ms 推进一帧
        let animation_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                let should_continue = this
                    .update(cx, |view, _cx| {
                        // 连接完成或失败后停止动画
                        matches!(view.status, ConnectStatus::Connecting { .. })
                    })
                    .unwrap_or(false);
                if !should_continue {
                    break;
                }
                this.update(cx, |view, cx| {
                    view.frame = view.frame.wrapping_add(1);
                    cx.notify();
                })
                .ok();
            }
        });

        Self {
            config,
            status: ConnectStatus::Connecting {
                stage: ConnectStage::Handshake,
            },
            frame: 0,
            animation_task: Some(animation_task),
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

    /// 处理重新连接
    fn handle_reconnect(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // 获取当前配置
        let config = self.config.clone();

        // 关闭当前视图并触发重新连接
        cx.emit(Event::Reconnect { config });
    }
}

impl EventEmitter<Event> for SshConnectView {}

impl Render for SshConnectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_failed = matches!(self.status, ConnectStatus::Failed { .. });
        let is_connected = matches!(self.status, ConnectStatus::Connected);
        let icon_color = if is_failed {
            theme.error
        } else if is_connected {
            theme.success
        } else {
            theme.accent
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
            .bg(theme.panel_background)
            .flex()
            .items_start()
            .justify_center()
            .pt(px(76.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(24.0))
                    .w(px(420.0))
                    .px(px(32.0))
                    .py(px(36.0))
                    .bg(theme.elevated_background)
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        // 图标
                        div()
                            .w(px(56.0))
                            .h(px(56.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(theme.app_background)
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                svg()
                                    .size(px(28.0))
                                    .text_color(icon_color)
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
                                    .text_color(theme.text)
                                    .text_size(px(15.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(self.config.host_label()),
                            )
                            .when_some(nickname, |this, n| {
                                this.child(
                                    div()
                                        .text_color(theme.text_muted)
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
                            .status(if is_failed {
                                crate::components::stepper::StepperStatus::Failed
                            } else if is_connected {
                                crate::components::stepper::StepperStatus::Success
                            } else {
                                crate::components::stepper::StepperStatus::Normal
                            })
                            .items([
                                StepperItem::new().child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_subtle)
                                        .child("SSH"),
                                ),
                                StepperItem::new().child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_subtle)
                                        .child("Auth"),
                                ),
                                StepperItem::new().child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_subtle)
                                        .child("Session"),
                                ),
                            ]),
                    )
                    .when(is_failed, |this| match &self.status {
                        ConnectStatus::Failed { message, .. } => this
                            .child(
                                div()
                                    .text_color(theme.error)
                                    .text_size(px(13.0))
                                    .child(format!("Failed: {}", message)),
                            )
                            .child(
                                div().mt(px(16.0)).child(
                                    Button::new("reconnect-btn")
                                        .label("Reconnect")
                                        .primary()
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.handle_reconnect(_window, cx);
                                        })),
                                ),
                            ),
                        _ => this,
                    }),
            )
    }
}
