use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    px, svg,
};

use crate::assets::Icon;
use crate::components::sidebar::Sidebar;
use crate::settings_hub::hosts_view::HostsView;
use crate::settings_hub::new_host_form::NewHostForm;
use crate::workspace::workspace::Workspace;

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

/// Left sidebar width in pixels
pub const LEFT_SIDEBAR_WIDTH: f32 = 200.0;
/// Right sidebar width in pixels
pub const RIGHT_SIDEBAR_WIDTH: f32 = 300.0;

/// Marker trait for sidebar width configuration
pub trait SidebarWidth: 'static {
    const WIDTH: f32;
}

/// Left sidebar width configuration
pub struct LeftSidebarWidth;
impl SidebarWidth for LeftSidebarWidth {
    const WIDTH: f32 = LEFT_SIDEBAR_WIDTH;
}

/// Right sidebar width configuration
pub struct RightSidebarWidth;
impl SidebarWidth for RightSidebarWidth {
    const WIDTH: f32 = RIGHT_SIDEBAR_WIDTH;
}

/// Generic SettingsHub sidebar component
pub struct SettingsHubSidebar<W: SidebarWidth> {
    sections: Vec<SettingsHubSection>,
    on_item_click: Option<Box<dyn Fn(SharedString, &mut App) + Send + Sync>>,
    focus_handle: FocusHandle,
    _width: std::marker::PhantomData<W>,
}

impl<W: SidebarWidth> SettingsHubSidebar<W> {
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::new_with_sections(cx, Vec::new())
    }

    pub fn new_with_sections(cx: &mut App, sections: Vec<SettingsHubSection>) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            sections,
            on_item_click: None,
            focus_handle,
            _width: std::marker::PhantomData,
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

impl SettingsHubSidebar<LeftSidebarWidth> {
    pub fn get_first_item_id(&self) -> Option<String> {
        self.sections
            .first()
            .and_then(|s| s.items.first())
            .map(|item| item.id.to_string())
    }
}

impl<W: SidebarWidth> Focusable for SettingsHubSidebar<W> {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<W: SidebarWidth> Render for SettingsHubSidebar<W> {
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
                            move |this: &mut Self, _, _, cx| {
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

        Sidebar::new().with_width(W::WIDTH).render(content)
    }
}

/// SettingsHub 左侧边栏组件 (type alias for backward compatibility)
pub type SettingsHubLeftSidebar = SettingsHubSidebar<LeftSidebarWidth>;

/// SettingsHub 右侧边栏组件 (type alias for backward compatibility)
pub type SettingsHubRightSidebar = SettingsHubSidebar<RightSidebarWidth>;

/// SettingsHub 中心内容区域 - 根据当前页面渲染不同内容
pub struct SettingsHubCenter {
    current_page: SettingsHubPage,
    hosts_view: Entity<HostsView>,
    focus_handle: FocusHandle,
}

impl SettingsHubCenter {
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::with_page(cx, SettingsHubPage::Hosts, None)
    }

    pub fn with_page(
        cx: &mut App,
        page: SettingsHubPage,
        workspace: Option<Entity<Workspace>>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let hosts_view = HostsView::new_with_workspace(cx, workspace);

            Self {
                current_page: page,
                hosts_view,
                focus_handle,
            }
        })
    }

    /// Setup the callback for the NEW HOST button in HostsView
    pub fn setup_new_host_callback<F>(&self, cx: &mut Context<Self>, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + Clone + 'static,
    {
        self.hosts_view.update(cx, |hosts_view, _cx| {
            hosts_view.on_new_host_click(callback);
        });
    }

    pub fn set_page(&mut self, page: SettingsHubPage, cx: &mut Context<Self>) {
        self.current_page = page;
        cx.notify();
    }

    /// Refresh hosts list from config
    pub fn refresh_hosts(&self, cx: &mut Context<Self>) {
        self.hosts_view.update(cx, |hosts_view, cx| {
            hosts_view.refresh_hosts(cx);
        });
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

/// SettingsHub 主视图 - 组合左侧边栏、中心内容和右侧边栏
pub struct SettingsHubView {
    left_sidebar: Entity<SettingsHubLeftSidebar>,
    center: Entity<SettingsHubCenter>,
    new_host_form: Entity<NewHostForm>,
    right_sidebar_visible: bool,
    /// 侧边栏是否应该被渲染（包括退出动画期间）
    right_sidebar_rendered: bool,
    focus_handle: FocusHandle,
}

impl SettingsHubView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::new_with_workspace(cx, None)
    }

    pub fn new_with_workspace(
        cx: &mut App,
        workspace: Option<Entity<Workspace>>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            // 创建默认的侧边栏数据
            let sections = create_default_settingshub_sections();
            let left_sidebar = SettingsHubLeftSidebar::new_with_sections(cx, sections);

            // 创建中心内容，默认显示 Hosts 页面，传入 workspace
            let center = SettingsHubCenter::with_page(cx, SettingsHubPage::Hosts, workspace.clone());

            // 创建新主机表单
            let new_host_form = NewHostForm::new(cx);

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

            // 注意：回调会在创建 Entity 后设置

            Self {
                left_sidebar,
                center,
                new_host_form,
                right_sidebar_visible: false,
                right_sidebar_rendered: false,
                focus_handle,
            }
        })
    }

    /// Setup callbacks after entity creation
    pub fn setup_callbacks(&self, cx: &mut Context<Self>) {
        // 设置 HostsView 的 NEW HOST 按钮回调
        let view_entity = cx.entity();
        self.center.update(cx, |center, cx| {
            center.setup_new_host_callback(cx, move |app| {
                view_entity.update(app, |view, cx| {
                    view.toggle_right_sidebar(cx);
                });
            });
        });

        // 设置 NewHostForm 的回调
        let view_entity_for_cancel = cx.entity();
        let view_entity_for_save = cx.entity();
        let center_for_refresh = self.center.clone();

        self.new_host_form.update(cx, |form, _cx| {
            // 取消回调 - 关闭侧边栏（带动画）
            form.on_cancel(move |app| {
                view_entity_for_cancel.update(app, |view, cx| {
                    view.close_right_sidebar(cx);
                });
            });

            // 保存成功回调 - 刷新列表并关闭侧边栏（带动画）
            form.on_saved(move |app| {
                // 刷新 HostsView 的列表
                center_for_refresh.update(app, |center, cx| {
                    // 重新加载 HostsView
                    center.refresh_hosts(cx);
                });

                // 关闭侧边栏（带动画）
                view_entity_for_save.update(app, |view, cx| {
                    view.close_right_sidebar(cx);
                });
            });
        });
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

impl SettingsHubView {
    /// Toggle the right sidebar visibility with animation
    pub fn toggle_right_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.right_sidebar_visible {
            // 关闭侧边栏：先播放退出动画，然后隐藏
            self.right_sidebar_visible = false;
            cx.notify();

            // 延迟后停止渲染侧边栏（与退出动画时长 ANIMATION_DURATION_EXIT_MS 一致）
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.right_sidebar_rendered = false;
                    cx.notify();
                });
            })
            .detach();
        } else {
            // 打开侧边栏：立即渲染并播放入场动画
            self.right_sidebar_visible = true;
            self.right_sidebar_rendered = true;
            cx.notify();
        }
    }

    /// 直接关闭侧边栏（用于取消/保存回调）
    fn close_right_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.right_sidebar_visible {
            self.toggle_right_sidebar(cx);
        }
    }
}

impl Render for SettingsHubView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let show_right_sidebar = self.right_sidebar_visible;
        let render_sidebar = self.right_sidebar_rendered;

        div()
            .id("settings-hub-view")
            .size_full()
            .flex()
            .flex_row()
            .p(px(6.0))
            .pt(px(10.0))
            .child(self.left_sidebar.clone())
            .child(div().flex_1().p(px(6.0)).child(self.center.clone()))
            .when(render_sidebar, |this| {
                this.child(
                    Sidebar::new()
                        .with_width(RIGHT_SIDEBAR_WIDTH)
                        .render_with_toggle_animation(
                            "right-sidebar-animation",
                            show_right_sidebar,
                            self.new_host_form.clone(),
                        ),
                )
            })
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

/// 创建默认的右侧边栏数据
pub fn create_default_right_sidebar_sections() -> Vec<SettingsHubSection> {
    vec![
        // 快捷操作分组
        SettingsHubSection::new("quick_actions", Some("快捷操作")).with_items(vec![
            SettingsHubItem::new("add_host", Icon::Plus, "添加主机"),
            SettingsHubItem::new("import", Icon::Terminal, "导入配置"),
        ]),
        // 系统信息分组
        SettingsHubSection::new("system_info", Some("系统信息")).with_items(vec![]),
    ]
}
