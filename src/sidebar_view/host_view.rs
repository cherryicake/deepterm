use std::rc::Rc;

use gpui::{App, MouseButton, ScrollHandle, Window, div, px};
use gpui::prelude::*;

use crate::assets::Icon;
use crate::sidebar_view::style::{button_bg, button_bg_hover, hover_bg};
use crate::ssh_view::hosts_view::HostsView;
use crate::theme::ActiveTheme;

/// 渲染 Host 侧边栏视图
pub fn render_host_sidebar<F>(
    cx: &App,
    on_select: Rc<F>,
    is_dark: bool,
    theme: crate::theme::UiTheme,
    scroll_handle: &ScrollHandle,
    selected_id: Option<&str>,
) -> gpui::Div
where
    F: Fn(&str, &mut Window, &mut App) + 'static,
{
    let (hosts, _, _) = HostsView::load_hosts_from_config();

    // 顶部导航栏
    let header = div()
        .w_full()
        .h(px(36.0))
        .flex()
        .items_center()
        .justify_between()
        .px(px(4.0))
        .mb(px(8.0))
        .child(
            div()
                .w(px(28.0))
                .h(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(999.0))
                .bg(button_bg(is_dark))
                .cursor_pointer()
                .hover(|style| style.bg(button_bg_hover(is_dark)))
                .child(
                    gpui::svg()
                        .size(px(16.0))
                        .path(Icon::ChevronLeft.path())
                        .text_color(theme.text),
                )
                .on_mouse_down(MouseButton::Left, {
                    let on_select = on_select.clone();
                    move |_, window, cx| on_select("__back", window, cx)
                }),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child("Host"),
        )
        .child(div().w(px(28.0)));

    // 搜索框
    let search_box = div()
        .w_full()
        .h(px(32.0))
        .flex()
        .items_center()
        .px(px(10.0))
        .gap(px(6.0))
        .rounded(px(8.0))
        .bg(hover_bg(is_dark))
        .mb(px(12.0))
        .child(
            gpui::svg()
                .size(px(14.0))
                .path(Icon::MagnifyingGlass.path())
                .text_color(theme.text_muted),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.text_muted)
                .child("搜索主机"),
        );

    // Host 列表
    let mut host_list = div().w_full().flex().flex_col();

    for (index, host) in hosts.iter().enumerate() {
        let icon_color = host.icon_color.to_hsla(cx);
        let host_id = host.id.to_string();
        let is_selected = selected_id == Some(&host_id);

        let row = div()
            .id(("host-item", index))
            .w_full()
            .h(px(52.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .when(is_selected, |this| {
                this.bg(theme.accent).text_color(theme.accent_foreground)
            })
            .when(!is_selected, |this| {
                this.hover(|style| style.bg(hover_bg(is_dark)))
                    .text_color(theme.text)
            })
            .child(
                div()
                    .w(px(32.0))
                    .h(px(32.0))
                    .rounded(px(8.0))
                    .bg(if is_selected {
                        theme.accent_foreground
                    } else {
                        icon_color
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        gpui::svg()
                            .size(px(16.0))
                            .path(host.os_icon.unwrap_or(Icon::Linux).path())
                            .text_color(if is_selected {
                                theme.accent
                            } else {
                                theme.accent_foreground
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if is_selected {
                                        theme.accent_foreground
                                    } else {
                                        theme.text
                                    })
                                    .child(host.name.to_string()),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_size(px(10.0))
                            .line_height(px(14.0))
                            .text_color(if is_selected {
                                theme.accent_foreground.opacity(0.7)
                            } else {
                                theme.text_muted
                            })
                            .child(host.subtitle()),
                    ),
            )
            .on_click({
                let on_select = on_select.clone();
                let id = host_id.clone();
                move |_, window, cx| on_select(&id, window, cx)
            });

        host_list = host_list.child(row);
    }

    div()
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .child(header)
        .child(search_box)
        .child(
            div()
                .id("host-scroll")
                .track_scroll(scroll_handle)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .child(host_list),
        )
}
