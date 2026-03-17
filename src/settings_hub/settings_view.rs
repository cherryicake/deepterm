use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    px, svg,
};

use crate::assets::Icon;
use crate::components::sidebar::Sidebar;
use crate::settings_hub::hosts_view::HostsView;

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

/// 当前选中的页面类型
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsHubPage {
    Hosts,
    Keychain,
    Agent,
    Theme,
}

impl SettingsHubPage {
    pub fn from_item_id(id: &str) -> Option<Self> {
        match id {
            "hosts" => Some(Self::Hosts),
            "keychain" => Some(Self::Keychain),
            "agent" => Some(Self::Agent),
            "theme" => Some(Self::Theme),
            _ => None,
        }
    }
}

/// SettingsHub 左侧边栏组件
pub struct SettingsHubLeftSidebar {
    sections: Vec<SettingsHubSection>,
    on_item_click: Option<Box<dyn Fn(SharedString, &mut App) + Send + Sync>>,
    focus_handle: FocusHandle,
}

impl SettingsHubLeftSidebar {
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

    pub fn get_first_item_id(&self) -> Option<String> {
        self.sections
            .first()
            .and_then(|s| s.items.first())
            .map(|item| item.id.to_string())
    }
}

impl Focusable for SettingsHubLeftSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsHubLeftSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Build the content using cx.children helper pattern
        let mut content = div().w_full().h_full().flex().flex_col().gap_3();

        // Build sections
        for section in &self.sections {
            let title = section.title.clone();

            // Section container
            let mut section_div = div().w_full().flex().flex_col();

            // Add title if present
            if let Some(t) = title {
                section_div = section_div.child(
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
                                .text_color(Sidebar::section_title_color())
                                .child(t.to_string()),
                        ),
                );
            }

            // Items container
            let mut items_div = div().w_full().flex().flex_col().gap(px(2.0));

            // Add each item using cx.child pattern
            for item in &section.items {
                let item_id = item.id.clone();
                let is_active = item.is_active;
                let icon = item.icon;
                let label = item.label.clone();
                let badge = item.badge;

                let item_element = div()
                    .id(item_id.clone())
                    .h(px(32.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(10.0))
                    .gap_3()
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
                            move |this: &mut SettingsHubLeftSidebar, _, _, cx| {
                                this.set_active_item(&item_id, cx);
                                if let Some(ref callback) = this.on_item_click {
                                    callback(item_id.clone(), cx);
                                }
                            }
                        }),
                    );

                items_div = items_div.child(item_element);
            }

            section_div = section_div.child(items_div);
            content = content.child(section_div);
        }

        Sidebar::new().with_width(200.0).render(content)
    }
}

/// SettingsHub 中心内容区域 - 根据当前页面渲染不同内容
pub struct SettingsHubCenter {
    current_page: SettingsHubPage,
    hosts_view: Entity<HostsView>,
    focus_handle: FocusHandle,
}

impl SettingsHubCenter {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let hosts_view = HostsView::new(cx);

            Self {
                current_page: SettingsHubPage::Hosts,
                hosts_view,
                focus_handle,
            }
        })
    }

    pub fn with_page(cx: &mut App, page: SettingsHubPage) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let hosts_view = HostsView::new(cx);

            Self {
                current_page: page,
                hosts_view,
                focus_handle,
            }
        })
    }

    pub fn set_page(&mut self, page: SettingsHubPage, cx: &mut Context<Self>) {
        self.current_page = page;
        cx.notify();
    }

    fn render_page(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.current_page {
            SettingsHubPage::Hosts => div().size_full().child(self.hosts_view.clone()),
            SettingsHubPage::Keychain => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(gpui::rgb(0x8888aa))
                        .child("Keychain settings coming soon..."),
                ),
            SettingsHubPage::Agent => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(gpui::rgb(0x8888aa))
                        .child("Agent settings coming soon..."),
                ),
            SettingsHubPage::Theme => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(gpui::rgb(0x8888aa))
                        .child("Theme settings coming soon..."),
                ),
        }
    }
}

impl Focusable for SettingsHubCenter {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsHubCenter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-hub-center")
            .size_full()
            .child(self.render_page(cx))
    }
}

/// SettingsHub 主视图 - 组合左侧边栏和中心内容
pub struct SettingsHubView {
    left_sidebar: Entity<SettingsHubLeftSidebar>,
    center: Entity<SettingsHubCenter>,
    focus_handle: FocusHandle,
}

impl SettingsHubView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            // 创建默认的侧边栏数据
            let sections = create_default_settingshub_sections();
            let left_sidebar = SettingsHubLeftSidebar::new_with_sections(cx, sections);

            // 创建中心内容，默认显示 Hosts 页面
            let center = SettingsHubCenter::with_page(cx, SettingsHubPage::Hosts);

            // 设置左侧边栏点击回调
            let center_for_callback = center.clone();
            left_sidebar.update(cx, |sidebar, _cx| {
                sidebar.set_active_item("hosts", _cx);
                sidebar.on_item_click(move |item_id, _app| {
                    if let Some(page) = SettingsHubPage::from_item_id(&item_id) {
                        center_for_callback.update(_app, |center, cx| {
                            center.set_page(page, cx);
                        });
                    }
                });
            });

            Self {
                left_sidebar,
                center,
                focus_handle,
            }
        })
    }

    /// 获取左侧边栏实体
    pub fn left_sidebar(&self) -> &Entity<SettingsHubLeftSidebar> {
        &self.left_sidebar
    }

    /// 获取中心内容实体
    pub fn center(&self) -> &Entity<SettingsHubCenter> {
        &self.center
    }
}

impl Focusable for SettingsHubView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsHubView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-hub-view")
            .size_full()
            .flex()
            .flex_row()
            .p(px(6.0))
            .pt(px(10.0))
            .child(self.left_sidebar.clone())
            .child(div().flex_1().p(px(6.0)).child(self.center.clone()))
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
        SettingsHubSection::new("terminal_setting", Some("偏好设置")).with_items(vec![]),
    ]
}
