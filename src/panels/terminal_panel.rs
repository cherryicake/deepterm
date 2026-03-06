use gpui::{
    div, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Window,
};

/// 终端面板组件 - 模拟 Termius 的终端界面
pub struct TerminalPanel {
    title: SharedString,
    content: Vec<SharedString>,
    prompt: SharedString,
    focus_handle: FocusHandle,
}

impl TerminalPanel {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        let content = vec![
            "Welcome to Ubuntu 22.04.4 LTS (GNU/Linux 5.15.0-105-generic x86_64)".into(),
            "".into(),
            " * Documentation:  https://help.ubuntu.com".into(),
            " * Management:     https://landscape.canonical.com".into(),
            " * Support:        https://ubuntu.com/pro".into(),
            "".into(),
            "  System information as of Mon Jun 10 11:12:43 UTC 2025".into(),
            "".into(),
            "  System load:  0.0                Processes:           102".into(),
            "  Usage of /:   22.4% of 24.05GB   Users logged in:     1".into(),
            "  Memory usage: 28%                IPv4 address for eth0: 10.0.0.15".into(),
            "  Swap usage:   0%".into(),
            "".into(),
            "0 updates can be applied immediately.".into(),
            "".into(),
            "Last login: Mon Jun 10 11:12:43 2025 from 10.0.0.3".into(),
        ];

        cx.new(|_cx| Self {
            title: "Terminal".into(),
            content,
            prompt: "user@server:~$ ".into(),
            focus_handle,
        })
    }

    pub fn with_title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = title.into();
        self
    }

    pub fn with_content(mut self, content: Vec<impl Into<SharedString>>) -> Self {
        self.content = content.into_iter().map(|s| s.into()).collect();
        self
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let prompt = self.prompt.clone();

        div()
            .id("terminal-panel")
            .size_full()
            .bg(rgb(0x1a1a2e))
            .p_4()
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .overflow_y_hidden()
                    .children(self.content.iter().map(|line| {
                        div()
                            .text_sm()
                            .text_color(rgb(0xcccccc))
                            .child(line.clone())
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x6c5ce7))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .child(prompt),
                            )
                            .child(div().w(px(8.0)).h(px(18.0)).bg(rgb(0x6c5ce7))),
                    ),
            )
    }
}
