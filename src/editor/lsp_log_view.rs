//! LSP 报文日志面板（View Logs）。
//!
//! 对照 Zed 的 `language_tools/src/lsp_log_view.rs`：它渲染全项目的服务器树，
//! 每个服务器一个 tab。这里只做**单个服务器**的报文流——菜单在哪个服务器上
//! 点的 View Logs，面板就看那一个，顶栏写明是哪个进程。
//!
//! 布局刻意像 VSCode 的 Output 面板：一行一帧，方向箭头 + 方法 + 预览。
//! 时间戳**不显示**：帧的先后已经由顺序表达，逐帧打时间戳只会让每行变长、
//! 扫不动。
//!
//! 刷新用一个轮询任务（250ms）而不是事件：日志的写入方在后台线程
//! （见 `crate::lsp::log` 的说明），gpui 的事件派发到不了那里，反过来的话
//! 要把每条报文都 `cx.update` 一次——那才是真正的热点路径。

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle, ScrollHandle,
    Styled, Window, div, px,
};

use crate::components::button::{Icon, Sizable, Size};
use crate::lsp::log::{self, Direction, LogMessage, MessageKind};
use crate::lsp::project::{ServerKey, ServerState};
use crate::theme::ActiveTheme as _;

/// 轮询间隔：报文是流式的，250ms 的滞后肉眼看不出来，又不会变成每帧重绘。
const REFRESH_MS: u64 = 250;

/// 列表区的最大高度（px）：报文多时内部滚动，不把编辑器顶跑。
const LIST_MAX_H: f32 = 300.0;

pub struct LspLogView {
    /// 看哪个服务器的日志。
    key: ServerKey,
    /// 顶栏标题（服务器命令名）。
    title: String,
    /// 打开面板那一刻的服务器状态。
    ///
    /// 只是快照，之后不跟着刷新：日志面板的职责是看报文，状态看状态栏 chip。
    /// 要它实时变的话，得给日志存储配一条事件通道——为了顶栏两个字不值当。
    server_state: ServerState,
    /// 最近一次取到的报文。
    messages: Vec<LogMessage>,
    /// 滚动锚点：有新帧时滚到底。
    scroll: ScrollHandle,
    /// 上一次的报文数：据此判断要不要滚。
    last_len: usize,
    focus_handle: FocusHandle,
}

impl LspLogView {
    pub fn new(
        key: ServerKey,
        title: String,
        server_state: ServerState,
        cx: &mut Context<Self>,
    ) -> Self {
        // 先取日志再建实体：那会儿还没有 `Context<Self>` 可用。
        let messages = log::messages(&key);
        Self {
            key,
            title,
            server_state,
            messages,
            scroll: ScrollHandle::new(),
            last_len: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    /// 重新拉一次日志。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.messages = log::messages(&self.key);
        if self.messages.len() != self.last_len {
            self.last_len = self.messages.len();
            // 有新增：滚到底，让用户看到最新一帧。
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    fn clear_log(&mut self, cx: &mut Context<Self>) {
        log::clear(&self.key);
        self.messages.clear();
        self.last_len = 0;
        cx.notify();
    }

    /// 一行报文：方向 + 方法 + 精简信息。
    ///
    /// 布局是「方法列定宽、信息列吃剩馀」而不是反过来：排障时先看**做了
    /// 什么**（方法名），再看细节。信息列截断用 `text_ellipsis` 交给排版层，
    /// 不在这里切字符串——那样长行会把方法列顶歪。
    fn render_message(&self, message: &LogMessage, cx: &Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let arrow_color = match message.direction {
            // 发出去的是我们主动问的，用静息色；收到的是服务器推的，
            // 用强调色——扫一眼就知道这一行是谁起的头。
            Direction::Outgoing => theme.text_muted,
            Direction::Incoming => theme.accent,
        };
        // 出错的响应单独染色：`-32801 content modified` 这类正是排障目标。
        let is_error =
            message.kind == MessageKind::Response && message.summary.starts_with("error:");

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(20.0))
            .px(px(4.0))
            .rounded(px(4.0))
            .text_sm()
            .child(
                div()
                    .w(px(12.0))
                    .flex_shrink_0()
                    .text_color(arrow_color)
                    .child(message.arrow()),
            )
            .child(
                div()
                    .w(px(220.0))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(if is_error { theme.error } else { theme.text })
                    .child(message.label()),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(if is_error {
                        theme.error
                    } else {
                        theme.text_muted
                    })
                    .child(message.summary.clone()),
            )
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for LspLogView {}
crate::impl_focusable!(LspLogView);

impl Render for LspLogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let state_text = match self.server_state {
            ServerState::Running => "运行中",
            ServerState::Starting => "启动中",
            ServerState::Stopped => "已停止",
            ServerState::Failed => "失败",
        };

        div()
            .id("lsp-log-view")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .w(px(600.0))
            .bg(theme.menu_background)
            .text_color(theme.text)
            .border(px(0.5))
            .border_color(theme.menu_border)
            .rounded(px(12.0))
            .shadow(vec![gpui::BoxShadow {
                color: theme.shadow.opacity(0.3),
                offset: gpui::point(px(0.0), px(8.0)),
                blur_radius: px(24.0),
                spread_radius: px(-8.0),
                inset: false,
            }])
            .occlude()
            // ── 顶栏 ────────────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(26.0))
                    .px(px(8.0))
                    .border_b(px(0.5))
                    .border_color(theme.menu_border)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .text_color(theme.text_muted)
                            .text_sm()
                            .child(format!("{} · {state_text}", self.title))
                            .child(format!("· {} 帧", self.messages.len())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(2.0))
                            .child(tool_button(
                                "log-clear",
                                "icons/trash.svg",
                                theme.text_muted,
                                theme.tab_hover_background,
                                {
                                    let view = cx.entity();
                                    move |_, _, cx| {
                                        view.update(cx, |view, cx| view.clear_log(cx));
                                    }
                                },
                            ))
                            .child(tool_button(
                                "log-close",
                                "icons/xmark.circle.fill.svg",
                                theme.text_muted,
                                theme.tab_hover_background,
                                {
                                    let view = cx.entity();
                                    move |_, _, cx| {
                                        view.update(cx, |_, cx| cx.emit(DismissEvent));
                                    }
                                },
                            )),
                    ),
            )
            // ── 报文列表 ────────────────────────────────────────────
            .child(
                div()
                    .id("lsp-log-scroll")
                    .flex()
                    .flex_col()
                    .gap_y(px(1.0))
                    .p(px(6.0))
                    .max_h(px(LIST_MAX_H))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(
                        self.messages
                            .iter()
                            .map(|m| self.render_message(m, cx))
                            .collect::<Vec<_>>(),
                    )
                    .when(self.messages.is_empty(), |this| {
                        this.child(
                            div()
                                .py(px(12.0))
                                .text_sm()
                                .text_color(theme.text_muted)
                                .child("还没有报文"),
                        )
                    }),
            )
    }
}

/// 顶栏的小图标按钮。
///
/// 主题色当参数传进来：这个函数是自由函数，没有 `cx` 可读。
fn tool_button(
    id: &'static str,
    icon: &'static str,
    icon_color: gpui::Hsla,
    hover_background: gpui::Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl gpui::IntoElement {
    div()
        .id(id)
        .w(px(20.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .cursor_pointer()
        .text_color(icon_color)
        .hover(move |this| this.bg(hover_background))
        .on_click(on_click)
        .child(Icon::new(icon).with_size(Size::XSmall))
}

/// 轮询任务：让面板定期拉一次新报文。
///
/// **必须用 `WeakEntity`**：`Entity::update` 拿不到实体时是 panic 而不是返
/// 回错误，而任务攥着强引用又会让实体永不消失——那样面板关了以后这个循环
/// 还在空转，而且再也停不下来。攥弱引用，面板一关 `upgrade()` 就是 `None`，
/// 循环自然了断。
pub fn spawn_refresh(view: &Entity<LspLogView>, cx: &mut gpui::AsyncApp) {
    let weak = view.downgrade();
    cx.spawn(async move |cx| {
        loop {
            smol::Timer::after(Duration::from_millis(REFRESH_MS)).await;
            let Some(view) = weak.upgrade() else {
                break;
            };
            view.update(cx, |view, cx| {
                view.refresh(cx);
            });
        }
    })
    .detach();
}
