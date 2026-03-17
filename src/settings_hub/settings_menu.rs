use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    px, svg,
};

use crate::assets::Icon;
use crate::components::sidebar::Sidebar;

/// SettingsHub 侧边栏项
#[derive(Clone)]
pub struct SettingsHubItem {
    pub id: SharedString,
    pub icon: Icon,
    pub label: SharedString,
    pub badge: Option<u32>,
    pub is_active: bool,
}

impl SettingsHubItem {
    pub fn new(id: impl Into<SharedString>, icon: Icon, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon,
            label: label.into(),
            badge: None,
            is_active: false,
        }
    }

    pub fn with_badge(mut self, count: u32) -> Self {
        self.badge = Some(count);
        self
    }
}

/// SettingsHub 侧边栏分组
#[derive(Clone)]
pub struct SettingsHubSection {
    pub id: SharedString,
    pub title: Option<SharedString>,
    pub items: Vec<SettingsHubItem>,
}

impl SettingsHubSection {
    pub fn new(id: impl Into<SharedString>, title: Option<impl Into<SharedString>>) -> Self {
        Self {
            id: id.into(),
            title: title.map(|t| t.into()),
            items: Vec::new(),
        }
    }

    pub fn with_items(mut self, items: Vec<SettingsHubItem>) -> Self {
        self.items = items;
        self
    }
}

/// SettingsHub 侧边栏组件
pub struct SettingsHubSidebar {
    sections: Vec<SettingsHubSection>,
    on_item_click: Option<Box<dyn Fn(SharedString, &mut App) + Send + Sync>>,
    focus_handle: FocusHandle,
}

impl SettingsHubSidebar {
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::new_with_sections(cx, Vec::new())
    }

    pub fn new_with_sections(cx: &mut App, sections: Vec<SettingsHubSection>) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            sections,
            on_item_click: None,
            focus_handle,
        })
    }

    pub fn on_item_click<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut App) + Send + Sync + 'static,
    {
        self.on_item_click = Some(Box::new(callback));
    }

    pub fn set_active_item(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut changed = false;
        for section in &mut self.sections {
            for item in &mut section.items {
                let should_be_active = item.id.as_ref() == id;
                if item.is_active != should_be_active {
                    item.is_active = should_be_active;
                    changed = true;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }
}

impl Focusable for SettingsHubSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsHubSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Use reference-based iteration to avoid cloning all data
        let content = div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .children(self.sections.iter().map(|section| {
                let title = section.title.as_ref();
                let items = &section.items;

                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .when(title.is_some(), |this| {
                        this.child(
                            div()
                                .h(px(28.0))
                                .w_full()
                                .flex()
                                .items_center()
                                .px(px(8.0))
                                .pt(px(8.0))
                                .pb_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(Sidebar::section_title_color())
                                        .child(title.unwrap().to_string()),
                                ),
                        )
                    })
                    .child(div().w_full().flex().flex_col().gap(px(2.0)).children(
                        items.iter().map(|item| {
                            let item_id = item.id.clone();
                            let is_active = item.is_active;
                            let icon = item.icon;
                            let label = item.label.clone();
                            let badge = item.badge;

                            div()
                                .id(item_id.clone())
                                .h(px(32.0))
                                .w_full()
                                .flex()
                                .items_center()
                                .px(px(10.0))
                                .gap(px(10.0))
                                .rounded(px(8.0))
                                .cursor_pointer()
                                .when(is_active, |this| this.bg(Sidebar::selected_background()))
                                .when(!is_active, |this| {
                                    this.hover(|style| style.bg(Sidebar::hover_background()))
                                })
                                .child(
                                    div()
                                        .w(px(22.0))
                                        .h(px(22.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(svg().size(px(18.0)).path(icon.path()).text_color(
                                            if is_active {
                                                Sidebar::selected_text_color()
                                            } else {
                                                Sidebar::text_color_secondary()
                                            },
                                        )),
                                )
                                .child(
                                    div().flex_1().child(
                                        div()
                                            .text_sm()
                                            .text_color(if is_active {
                                                Sidebar::selected_text_color()
                                            } else {
                                                Sidebar::text_color()
                                            })
                                            .child(label),
                                    ),
                                )
                                .when(badge.is_some(), |this| {
                                    let count = badge.unwrap();
                                    this.child(
                                        div()
                                            .min_w(px(22.0))
                                            .h(px(18.0))
                                            .px(px(6.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_md()
                                            .bg(Sidebar::badge_background())
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(Sidebar::badge_text_color())
                                                    .child(format!("{}", count)),
                                            ),
                                    )
                                })
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener({
                                        let item_id = item_id.clone();
                                        move |this, _, _, cx| {
                                            this.set_active_item(&item_id, cx);
                                            if let Some(ref callback) = this.on_item_click {
                                                callback(item_id.clone(), cx);
                                            }
                                        }
                                    }),
                                )
                        }),
                    ))
            }));

        Sidebar::new().with_width(200.0).render(content)
    }
}

/// 创建默认的侧边栏数据（主机管理风格）
pub fn create_default_settingshub_sections() -> Vec<SettingsHubSection> {
    vec![
        // 个人收藏分组
        SettingsHubSection::new("hostshub", Some("主机中心")).with_items(vec![
            SettingsHubItem::new("hosts", Icon::Host, "Hosts"),
            SettingsHubItem::new("keychain", Icon::KeyShield, "Keychain"),
        ]),
        // 终端设置分组
        SettingsHubSection::new("terminal_setting", Some("个人偏好")).with_items(vec![
            SettingsHubItem::new("agent", Icon::Server, "Agent"),
            SettingsHubItem::new("theme", Icon::Screen, "Theme"),
        ]),
    ]
}
