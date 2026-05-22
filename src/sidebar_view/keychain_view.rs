use std::rc::Rc;

use gpui::{App, MouseButton, ScrollHandle, Window, div, px};
use gpui::prelude::*;

use crate::assets::Icon;
use crate::sidebar_view::style::{button_bg, button_bg_hover, hover_bg};
use crate::ssh::keychain::{KeychainItem, KeychainItemKind, KeychainStore};
use crate::theme::ActiveTheme;

/// 渲染 Keychain 侧边栏视图（同时展示 Keys 和 Identities）
pub fn render_keychain_sidebar<F>(
    _cx: &App,
    on_select: Rc<F>,
    is_dark: bool,
    theme: crate::theme::UiTheme,
    scroll_handle: &ScrollHandle,
    selected_id: Option<&str>,
) -> gpui::Div
where
    F: Fn(&str, &mut Window, &mut App) + 'static,
{
    let items = KeychainStore::load_or_default().items;
    let keys: Vec<&KeychainItem> = items.iter().filter(|i| i.kind == KeychainItemKind::Key).collect();
    let identities: Vec<&KeychainItem> = items.iter().filter(|i| i.kind == KeychainItemKind::Identity).collect();

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
                .child("Keychain"),
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
                .child("搜索密钥或身份"),
        );

    let mut list = div().w_full().flex().flex_col();

    // Keys 区域
    if !keys.is_empty() {
        list = list.child(
            div()
                .h(px(28.0))
                .w_full()
                .flex()
                .items_center()
                .px(px(8.0))
                .pt_3()
                .pb_3()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text_subtle)
                        .child(format!("Keys ({})", keys.len())),
                ),
        );

        for (index, item) in keys.iter().enumerate() {
            list = list.child(render_keychain_item_row(
                item,
                index,
                Icon::KeyShield,
                theme.accent,
                is_dark,
                theme,
                selected_id,
                on_select.clone(),
            ));
        }
    }

    // Identities 区域
    if !identities.is_empty() {
        list = list.child(
            div()
                .h(px(28.0))
                .w_full()
                .flex()
                .items_center()
                .px(px(8.0))
                .pt_3()
                .pb_3()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text_subtle)
                        .child(format!("Identities ({})", identities.len())),
                ),
        );

        for (index, item) in identities.iter().enumerate() {
            list = list.child(render_keychain_item_row(
                item,
                index + keys.len(),
                Icon::Host,
                theme.success,
                is_dark,
                theme,
                selected_id,
                on_select.clone(),
            ));
        }
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
                .id("keychain-scroll")
                .track_scroll(scroll_handle)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .child(list),
        )
}

fn render_keychain_item_row<F>(
    item: &KeychainItem,
    index: usize,
    icon: Icon,
    icon_color: gpui::Hsla,
    is_dark: bool,
    theme: crate::theme::UiTheme,
    selected_id: Option<&str>,
    on_select: Rc<F>,
) -> impl IntoElement
where
    F: Fn(&str, &mut Window, &mut App) + 'static,
{
    let item_id = item.id.clone();
    let is_selected = selected_id == Some(&item_id);

    div()
        .id(("keychain-item", index))
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
                        .path(icon.path())
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
                                .child(item.label.clone()),
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
                        .child(item.subtitle()),
                ),
        )
        .on_click({
            let id = item_id.clone();
            move |_, window, cx| on_select(&id, window, cx)
        })
}
