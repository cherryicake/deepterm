use chrono::{DateTime, Local};
use gpui::prelude::*;
use gpui::{Div, div, px};

use crate::theme::UiTheme;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileBrowserViewMode {
    Columns,
    List,
}

pub fn format_datetime(timestamp: std::time::SystemTime) -> String {
    DateTime::<Local>::from(timestamp)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

pub fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", size, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

pub fn render_icon_grid(theme: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .gap(px(2.0))
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                )
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                ),
        )
        .child(
            div()
                .flex()
                .gap(px(2.0))
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                )
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                ),
        )
}

pub fn render_icon_list(theme: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .w(px(10.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(10.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(10.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
}

pub fn render_icon_columns(theme: &UiTheme) -> Div {
    div()
        .flex()
        .gap(px(2.0))
        .child(
            div()
                .w(px(3.0))
                .h(px(10.0))
                .rounded(px(999.0))
                .bg(theme.text),
        )
        .child(
            div()
                .w(px(3.0))
                .h(px(10.0))
                .rounded(px(999.0))
                .bg(theme.text),
        )
        .child(
            div()
                .w(px(3.0))
                .h(px(10.0))
                .rounded(px(999.0))
                .bg(theme.text),
        )
}

pub fn render_icon_gallery(theme: &UiTheme) -> Div {
    div()
        .w(px(11.0))
        .h(px(8.0))
        .rounded(px(2.0))
        .border_1()
        .border_color(theme.text_muted)
        .child(
            div()
                .w_full()
                .h(px(2.0))
                .mt(px(4.0))
                .bg(theme.text_muted.opacity(0.8)),
        )
}

pub fn render_icon_share(theme: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(1.0))
        .child(
            div()
                .w(px(2.0))
                .h(px(6.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(8.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
}

pub fn render_icon_tag(theme: &UiTheme) -> Div {
    div()
        .w(px(10.0))
        .h(px(7.0))
        .rounded(px(2.0))
        .border_1()
        .border_color(theme.text_muted)
}

pub fn render_icon_more(theme: &UiTheme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .child(
            div()
                .w(px(2.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(2.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(2.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
}
