use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, MouseButton, ScrollHandle, Window, div, px};

use crate::assets::Icon;
use crate::sidebar_view::style::{button_bg, button_bg_hover, hover_bg};
use crate::theme::ThemeState;

/// 渲染主题侧边栏视图
pub fn render_theme_sidebar<F, G>(
    cx: &App,
    on_select: Rc<F>,
    theme: crate::theme::UiTheme,
    scroll_handle: &ScrollHandle,
    search_query: &str,
    on_search_change: Rc<G>,
) -> gpui::Div
where
    F: Fn(&str, &mut Window, &mut App) + 'static,
    G: Fn(&str, &mut App) + 'static,
{
    let active_theme_id = ThemeState::global(cx).active_theme_id().to_string();
    let themes = ThemeState::themes(cx).to_vec();

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
                .bg(button_bg(theme))
                .cursor_pointer()
                .hover(|style| style.bg(button_bg_hover(theme)))
                .child(
                    gpui::svg()
                        .size(px(8.0))
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
                .child("主题"),
        )
        .child(div().w(px(28.0)));

    // 搜索框（真实过滤：输入即时过滤下方主题列表）
    let search_box = div()
        .w_full()
        .h(px(32.0))
        .flex()
        .items_center()
        .px(px(10.0))
        .gap(px(6.0))
        .rounded(px(8.0))
        .bg(theme.tab_hover_background)
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
                .child(if search_query.is_empty() {
                    "搜索主题".to_string()
                } else {
                    search_query.to_string()
                }),
        )
        .on_mouse_down(MouseButton::Left, {
            // 点击搜索框：把过滤词清空重新开始输入（简单交互；完整
            // 编辑体验后续可换 InputState 组件）。
            let on_search_change = on_search_change.clone();
            move |_, _window, cx| on_search_change("", cx)
        });

    // 主题列表（按搜索词过滤，名字/中文名不区分大小写匹配）
    let mut theme_list = div().w_full().flex().flex_col().gap(px(2.0));

    let query_lower = search_query.trim().to_lowercase();
    for (index, theme_item) in themes.iter().enumerate() {
        if !query_lower.is_empty() {
            let name_hit = theme_item.name.to_lowercase().contains(&query_lower);
            let display_hit = theme_item.description.to_lowercase().contains(&query_lower);
            if !name_hit && !display_hit {
                continue;
            }
        }
        let is_active = theme_item.id == active_theme_id;
        let theme_id = theme_item.id.clone();
        let preview_colors = theme_item.terminal.preview_colors();

        let accent_preview = div()
            .w(px(14.0))
            .h(px(14.0))
            .rounded(px(4.0))
            .bg(theme_item.ui.accent);

        let ansi_top = div().flex().gap(px(2.0)).children(
            preview_colors[..8]
                .iter()
                .copied()
                .map(|color| div().w(px(9.0)).h(px(9.0)).rounded(px(2.0)).bg(color)),
        );
        let ansi_bottom = div().flex().gap(px(2.0)).children(
            preview_colors[8..]
                .iter()
                .copied()
                .map(|color| div().w(px(9.0)).h(px(9.0)).rounded(px(2.0)).bg(color)),
        );

        let color_preview = div()
            .flex()
            .flex_shrink_0()
            .flex_col()
            .items_center()
            .gap(px(2.0))
            .child(accent_preview)
            .child(ansi_top)
            .child(ansi_bottom);

        let row = div()
            .id(("theme-item", index))
            .w_full()
            .p(px(10.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .when(is_active, |this| {
                this.bg(theme.accent).text_color(theme.accent_foreground)
            })
            .when(!is_active, |this| {
                this.hover(|style| style.bg(hover_bg(theme)))
                    .text_color(theme.text)
            })
            .flex()
            .flex_row()
            .items_start()
            .gap(px(10.0))
            .child(color_preview)
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
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if is_active {
                                        theme.accent_foreground
                                    } else {
                                        theme.text
                                    })
                                    .child(theme_item.name.clone()),
                            )
                            .when(is_active, |this| {
                                this.child(
                                    div()
                                        .px(px(6.0))
                                        .h(px(18.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(999.0))
                                        .bg(theme.accent_foreground)
                                        .child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(theme.accent)
                                                .child("当前"),
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_size(px(10.0))
                            .line_height(px(14.0))
                            .h(px(28.0))
                            .overflow_hidden()
                            .text_color(if is_active {
                                theme.accent_foreground.opacity(0.7)
                            } else {
                                theme.text_muted
                            })
                            .child(theme_item.description.clone()),
                    ),
            )
            .on_click({
                let theme_id = theme_id.clone();
                move |_, _window, cx| {
                    ThemeState::set_active(&theme_id, cx);
                }
            });

        theme_list = theme_list.child(row);
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
                .id("theme-scroll")
                .track_scroll(scroll_handle)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .child(theme_list),
        )
}
