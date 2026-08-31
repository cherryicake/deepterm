use super::file_tree::FileTreePanel;
use super::style::{button_bg, button_bg_hover};
use crate::assets::Icon;
use crate::theme::UiTheme;
use gpui::prelude::*;
use gpui::{div, px};

pub const TAB_GAP: f32 = 6.0;

pub struct TabItem {
    pub label: &'static str,
    pub icon: Icon,
    pub panel: FileTreePanel,
    pub active_width: f32,
}

pub const TAB_ITEMS: &[TabItem] = &[
    TabItem {
        label: "文件",
        icon: Icon::Folder,
        panel: FileTreePanel::Files,
        active_width: 70.0,
    },
    TabItem {
        label: "搜索",
        icon: Icon::MagnifyingGlass,
        panel: FileTreePanel::Search,
        active_width: 70.0,
    },
    TabItem {
        label: "Git",
        icon: Icon::Git,
        panel: FileTreePanel::Git,
        active_width: 70.0,
    },
];

pub fn build_tab_button(
    item: &TabItem,
    is_active: bool,
    theme: &UiTheme,
) -> gpui::Stateful<gpui::Div> {
    let text_color = if is_active {
        theme.accent_foreground
    } else {
        theme.text
    };

    div()
        .id(format!("panel-tab-{}", item.label))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(14.0))
        .overflow_hidden()
        .bg(button_bg(*theme))
        .when(is_active, |this| this.bg(theme.accent))
        .cursor_pointer()
        .hover(move |style| {
            if is_active {
                style
            } else {
                style.bg(button_bg_hover(*theme))
            }
        })
        .child(
            gpui::svg()
                .size(px(14.0))
                .flex_none()
                .text_color(text_color)
                .path(item.icon.path()),
        )
}
