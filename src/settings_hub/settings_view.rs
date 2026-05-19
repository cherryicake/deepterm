use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Window, div, px, svg,
};
use std::path::PathBuf;

use crate::assets::Icon;
use crate::components::sidebar::Sidebar;
use crate::settings_hub::font_view::FontSettingsView;
use crate::settings_hub::host_form::HostForm;
use crate::settings_hub::hosts_view::HostsView;
use crate::settings_hub::keychain_view::{KeychainEditor, KeychainView};
use crate::ssh::config::SshConnectionConfig;
use crate::ssh::keychain::KeychainItem;
use crate::theme::{ActiveTheme, AppTheme, ThemeState};
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
    Fonts,
}

impl SettingsHubPage {
    pub fn from_item_id(id: &str) -> Option<Self> {
        match id {
            "hosts" => Some(Self::Hosts),
            "keychain" => Some(Self::Keychain),
            "agent" => Some(Self::Agent),
            "theme" => Some(Self::Theme),
            "fonts" => Some(Self::Fonts),
            _ => None,
        }
    }
}

/// Left sidebar width in pixels
pub const LEFT_SIDEBAR_WIDTH: f32 = 200.0;
/// Right sidebar width in pixels
pub const RIGHT_SIDEBAR_WIDTH: f32 = 360.0;

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
                                .text_color(Sidebar::section_title_color(cx))
                                .child(t.to_string()),
                        ),
                );
            }

            // Items container
            let mut items_div = div().w_full().flex().flex_col().gap(px(6.0));

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
                    .when(is_active, |this| this.bg(Sidebar::selected_background(cx)))
                    .when(!is_active, |this| {
                        this.hover(|style| style.bg(Sidebar::hover_background(cx)))
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
                                    Sidebar::selected_text_color(cx)
                                } else {
                                    Sidebar::text_color_secondary(cx)
                                },
                            )),
                    )
                    .child(
                        div().flex_1().child(
                            div()
                                .text_sm()
                                .text_color(if is_active {
                                    Sidebar::selected_text_color(cx)
                                } else {
                                    Sidebar::text_color(cx)
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
                                .bg(Sidebar::badge_background(cx))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(Sidebar::badge_text_color(cx))
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

        Sidebar::new().with_width(W::WIDTH).render(content, cx)
    }
}

/// SettingsHub 左侧边栏组件 (type alias for backward compatibility)
pub type SettingsHubLeftSidebar = SettingsHubSidebar<LeftSidebarWidth>;

/// SettingsHub 右侧边栏组件 (type alias for backward compatibility)
pub type SettingsHubRightSidebar = SettingsHubSidebar<RightSidebarWidth>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RightSidebarContent {
    HostForm,
    KeychainEditor,
}

/// SettingsHub 中心内容区域 - 根据当前页面渲染不同内容
pub struct SettingsHubCenter {
    current_page: SettingsHubPage,
    hosts_view: Entity<HostsView>,
    keychain_view: Entity<KeychainView>,
    font_view: Entity<FontSettingsView>,
    theme_scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
}

impl SettingsHubCenter {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::with_page(window, cx, SettingsHubPage::Hosts, None)
    }

    pub fn with_page(
        window: &mut Window,
        cx: &mut App,
        page: SettingsHubPage,
        workspace: Option<Entity<Workspace>>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let hosts_view = HostsView::new_with_workspace(window, cx, workspace);
            let keychain_view = KeychainView::new(window, cx);
            let font_view = FontSettingsView::new(window, cx);
            let theme_scroll_handle = ScrollHandle::new();

            Self {
                current_page: page,
                hosts_view,
                keychain_view,
                font_view,
                theme_scroll_handle,
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

    /// Setup the callback for editing a host in HostsView
    pub fn setup_edit_host_callback<F>(&self, cx: &mut Context<Self>, callback: F)
    where
        F: Fn(&mut App, SshConnectionConfig) + Send + Sync + Clone + 'static,
    {
        self.hosts_view.update(cx, |hosts_view, _cx| {
            hosts_view.on_edit_host_click(callback);
        });
    }

    pub fn setup_new_key_callback<F>(&self, cx: &mut Context<Self>, callback: F)
    where
        F: Fn(&mut App, Option<PathBuf>) + Send + Sync + Clone + 'static,
    {
        self.keychain_view.update(cx, |keychain_view, _cx| {
            keychain_view.on_new_key_click(callback);
        });
    }

    pub fn setup_new_identity_callback<F>(&self, cx: &mut Context<Self>, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + Clone + 'static,
    {
        self.keychain_view.update(cx, |keychain_view, _cx| {
            keychain_view.on_new_identity_click(callback);
        });
    }

    pub fn setup_edit_keychain_item_callback<F>(&self, cx: &mut Context<Self>, callback: F)
    where
        F: Fn(&mut App, KeychainItem) + Send + Sync + Clone + 'static,
    {
        self.keychain_view.update(cx, |keychain_view, _cx| {
            keychain_view.on_edit_item_click(callback);
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

    pub fn refresh_keychain(&self, cx: &mut Context<Self>) {
        self.keychain_view.update(cx, |keychain_view, cx| {
            keychain_view.refresh_items(cx);
        });
    }

    fn render_preview_row(colors: &[Hsla]) -> gpui::Div {
        div().flex().gap_1().children(
            colors
                .iter()
                .copied()
                .map(|color| div().w(px(18.0)).h(px(18.0)).rounded(px(4.0)).bg(color)),
        )
    }

    fn render_theme_card(theme: &AppTheme, is_active: bool, cx: &mut Context<Self>) -> gpui::Div {
        let preview_colors = theme.terminal.preview_colors();
        let theme_id = theme.id.to_string();
        let appearance_label = if theme.appearance.is_dark() {
            "Dark"
        } else {
            "Light"
        };

        let ansi_top = Self::render_preview_row(&preview_colors[..8]);
        let ansi_bottom = Self::render_preview_row(&preview_colors[8..]);

        div()
            .w_full()
            .p(px(14.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(if is_active {
                theme.ui.accent
            } else {
                theme.ui.border
            })
            .bg(theme.ui.panel_background)
            .cursor_pointer()
            .hover({
                let hover_border = theme.ui.accent_hover;
                move |style| style.border_color(hover_border)
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.ui.text)
                                            .child(theme.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .px(px(8.0))
                                            .h(px(22.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(999.0))
                                            .bg(theme.ui.elevated_background)
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.ui.text_subtle)
                                                    .child(appearance_label),
                                            ),
                                    )
                                    .when(is_active, |this| {
                                        this.child(
                                            div()
                                                .px(px(8.0))
                                                .h(px(22.0))
                                                .flex()
                                                .items_center()
                                                .rounded(px(999.0))
                                                .bg(theme.ui.accent)
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(theme.ui.accent_foreground)
                                                        .child("Active"),
                                                ),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(px(20.0))
                                    .text_color(theme.ui.text_muted)
                                    .child(theme.description.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .w(px(256.0))
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(28.0))
                                            .h(px(28.0))
                                            .rounded(px(8.0))
                                            .bg(theme.ui.title_bar_background),
                                    )
                                    .child(
                                        div()
                                            .w(px(28.0))
                                            .h(px(28.0))
                                            .rounded(px(8.0))
                                            .bg(theme.ui.panel_background),
                                    )
                                    .child(
                                        div()
                                            .w(px(28.0))
                                            .h(px(28.0))
                                            .rounded(px(8.0))
                                            .bg(theme.ui.accent),
                                    )
                                    .child(
                                        div()
                                            .w(px(28.0))
                                            .h(px(28.0))
                                            .rounded(px(8.0))
                                            .bg(theme.terminal.background),
                                    ),
                            )
                            .child(ansi_top)
                            .child(ansi_bottom),
                    ),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |_this, _event, _window, cx| {
                    if ThemeState::set_active(&theme_id, cx) {
                        cx.notify();
                    }
                }),
            )
    }

    fn render_theme_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let active_theme_id = ThemeState::global(cx).active_theme_id().to_string();
        let current_theme_name = cx.theme().name.clone();
        let themes = ThemeState::themes(cx).to_vec();
        let mut theme_cards = div().w_full().flex().flex_col().gap_3();

        for theme in themes.iter() {
            theme_cards = theme_cards.child(Self::render_theme_card(
                theme,
                theme.id == active_theme_id.as_str(),
                cx,
            ));
        }

        div().size_full().flex().flex_col().overflow_hidden().child(
            div()
                .w_full()
                .max_w(px(960.0))
                .mx_auto()
                .flex()
                .flex_col()
                .gap_5()
                .p(px(24.0))
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_xl()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(cx.theme().ui.text)
                                .child("Theme"),
                        )
                        .child(div().text_sm().text_color(cx.theme().ui.text_muted).child(
                            format!(
                                "当前主题: {}。界面表面和终端 ANSI/selection/cursor 会一起切换。",
                                current_theme_name
                            ),
                        )),
                )
                .child(
                    div()
                        .id("theme-scroll")
                        .track_scroll(&self.theme_scroll_handle)
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(theme_cards),
                ),
        )
    }

    fn render_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match self.current_page {
            SettingsHubPage::Hosts => div().size_full().child(self.hosts_view.clone()),
            SettingsHubPage::Keychain => div().size_full().child(self.keychain_view.clone()),
            SettingsHubPage::Agent => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(cx.theme().ui.text_muted)
                        .child("Agent settings coming soon..."),
                ),
            SettingsHubPage::Theme => self.render_theme_page(cx),
            SettingsHubPage::Fonts => div().size_full().child(self.font_view.clone()),
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
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_page(cx))
    }
}

/// SettingsHub 主视图 - 组合左侧边栏、中心内容和右侧边栏
pub struct SettingsHubView {
    left_sidebar: Entity<SettingsHubLeftSidebar>,
    center: Entity<SettingsHubCenter>,
    host_form: Entity<HostForm>,
    keychain_editor: Entity<KeychainEditor>,
    right_sidebar_visible: bool,
    /// 侧边栏是否应该被渲染（包括退出动画期间）
    right_sidebar_rendered: bool,
    right_sidebar_content: RightSidebarContent,
    focus_handle: FocusHandle,
}

impl SettingsHubView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::new_with_workspace(window, cx, None)
    }

    pub fn new_with_workspace(
        window: &mut Window,
        cx: &mut App,
        workspace: Option<Entity<Workspace>>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            // 创建默认的侧边栏数据
            let sections = create_default_settingshub_sections();
            let left_sidebar = SettingsHubLeftSidebar::new_with_sections(cx, sections);

            // 创建中心内容，默认显示 Hosts 页面，传入 workspace
            let center =
                SettingsHubCenter::with_page(window, cx, SettingsHubPage::Hosts, workspace.clone());

            // 创建新主机表单
            let host_form = HostForm::new(window, cx);
            let keychain_editor = KeychainEditor::new(window, cx);

            left_sidebar.update(cx, |sidebar, _cx| {
                sidebar.set_active_item("hosts", _cx);
            });

            // 注意：回调会在创建 Entity 后设置

            Self {
                left_sidebar,
                center,
                host_form,
                keychain_editor,
                right_sidebar_visible: false,
                right_sidebar_rendered: false,
                right_sidebar_content: RightSidebarContent::HostForm,
                focus_handle,
            }
        })
    }

    /// Setup callbacks after entity creation
    pub fn setup_callbacks(&self, cx: &mut Context<Self>) {
        let view_entity_for_sidebar = cx.entity();
        let center_for_sidebar = self.center.clone();
        self.left_sidebar.update(cx, |sidebar, _cx| {
            sidebar.on_item_click(move |item_id, app| {
                if let Some(page) = SettingsHubPage::from_item_id(&item_id) {
                    center_for_sidebar.update(app, |center, cx| {
                        center.set_page(page, cx);
                    });
                    view_entity_for_sidebar.update(app, |view, cx| {
                        view.close_right_sidebar(cx);
                    });
                }
            });
        });

        // 设置 HostsView 的 NEW HOST 按钮回调
        let view_entity = cx.entity();
        self.center.update(cx, |center, cx| {
            center.setup_new_host_callback(cx, move |app| {
                view_entity.update(app, |view, cx| {
                    // Reset form before opening for new host
                    view.host_form.update(cx, |form, cx| {
                        form.reset_form(cx);
                    });
                    view.open_right_sidebar_with(RightSidebarContent::HostForm, cx);
                });
            });
        });

        // 设置 HostsView 的编辑主机回调
        let view_entity_for_edit = cx.entity();
        self.center.update(cx, |center, cx| {
            center.setup_edit_host_callback(cx, move |app, config| {
                view_entity_for_edit.update(app, |view, cx| {
                    view.edit_host(&config, cx);
                });
            });
        });

        let view_entity_for_new_key = cx.entity();
        self.center.update(cx, |center, cx| {
            center.setup_new_key_callback(cx, move |app, path| {
                view_entity_for_new_key.update(app, |view, cx| {
                    view.keychain_editor.update(cx, |editor, cx| {
                        if let Some(path) = path.clone() {
                            editor.import_dropped_key_path(path, cx);
                        } else {
                            editor.start_new_key(cx);
                        }
                    });
                    view.open_right_sidebar_with(RightSidebarContent::KeychainEditor, cx);
                });
            });
        });

        let view_entity_for_new_identity = cx.entity();
        self.center.update(cx, |center, cx| {
            center.setup_new_identity_callback(cx, move |app| {
                view_entity_for_new_identity.update(app, |view, cx| {
                    view.keychain_editor.update(cx, |editor, cx| {
                        editor.start_new_identity(cx);
                    });
                    view.open_right_sidebar_with(RightSidebarContent::KeychainEditor, cx);
                });
            });
        });

        let view_entity_for_edit_keychain = cx.entity();
        self.center.update(cx, |center, cx| {
            center.setup_edit_keychain_item_callback(cx, move |app, item| {
                view_entity_for_edit_keychain.update(app, |view, cx| {
                    view.keychain_editor.update(cx, |editor, cx| {
                        editor.load_item(&item, cx);
                    });
                    view.open_right_sidebar_with(RightSidebarContent::KeychainEditor, cx);
                });
            });
        });

        // 设置 HostForm 的回调
        let view_entity_for_cancel = cx.entity();
        let view_entity_for_save = cx.entity();
        let center_for_refresh = self.center.clone();

        self.host_form.update(cx, |form, _cx| {
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

        let view_entity_for_keychain_cancel = cx.entity();
        let view_entity_for_keychain_save = cx.entity();
        let center_for_keychain_refresh = self.center.clone();
        self.keychain_editor.update(cx, |editor, _cx| {
            editor.on_cancel(move |app| {
                view_entity_for_keychain_cancel.update(app, |view, cx| {
                    view.close_right_sidebar(cx);
                });
            });

            editor.on_saved(move |app| {
                center_for_keychain_refresh.update(app, |center, cx| {
                    center.refresh_keychain(cx);
                });
                view_entity_for_keychain_save.update(app, |view, cx| {
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
    fn open_right_sidebar(&mut self, cx: &mut Context<Self>) {
        self.right_sidebar_visible = true;
        self.right_sidebar_rendered = true;
        cx.notify();
    }

    fn open_right_sidebar_with(&mut self, content: RightSidebarContent, cx: &mut Context<Self>) {
        self.right_sidebar_content = content;
        self.open_right_sidebar(cx);
    }

    pub fn toggle_right_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.right_sidebar_visible {
            // 关闭侧边栏：先播放退出动画，然后隐藏
            self.right_sidebar_visible = false;
            cx.notify();

            // 延迟后停止渲染侧边栏（与退出动画时长 ANIMATION_DURATION_EXIT_MS 一致）
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(
                        crate::components::sidebar::ANIMATION_DURATION_EXIT_MS,
                    ))
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.right_sidebar_rendered = false;
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.open_right_sidebar(cx);
        }
    }

    /// 直接关闭侧边栏（用于取消/保存回调）
    fn close_right_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.right_sidebar_visible {
            self.toggle_right_sidebar(cx);
        }
    }

    /// Open sidebar for editing a host
    pub fn edit_host(&mut self, config: &SshConnectionConfig, cx: &mut Context<Self>) {
        self.host_form.update(cx, |form, cx| {
            form.load_config(config, cx);
        });
        self.open_right_sidebar_with(RightSidebarContent::HostForm, cx);
    }
}

impl Render for SettingsHubView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let show_right_sidebar = self.right_sidebar_visible;
        let render_sidebar = self.right_sidebar_rendered;
        let sidebar_content = match self.right_sidebar_content {
            RightSidebarContent::HostForm => self.host_form.clone().into_any_element(),
            RightSidebarContent::KeychainEditor => self.keychain_editor.clone().into_any_element(),
        };

        div()
            .id("settings-hub-view")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_row()
            .p(px(6.0))
            .pt(px(10.0))
            .child(self.left_sidebar.clone())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p(px(6.0))
                    .child(self.center.clone()),
            )
            .when(render_sidebar, |this| {
                this.child(
                    Sidebar::new()
                        .with_width(RIGHT_SIDEBAR_WIDTH)
                        .render_with_toggle_animation(
                            "right-sidebar-animation",
                            show_right_sidebar,
                            sidebar_content,
                            _cx,
                        ),
                )
            })
    }
}

/// 创建默认的侧边栏数据
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
