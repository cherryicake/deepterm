use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, Context, CursorStyle, Element, Entity, GlobalElementId,
    InteractiveElement, IntoElement, LayoutId, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels,
    Render, ScrollHandle, Subscription, WeakEntity, Window, WindowControlArea, anchored, deferred,
    div, ease_in_out, px,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::menu::{PopupMenu, PopupMenuItem};
use crate::sidebar_view::font_view::FontSettingsView;
use crate::sidebar_view::host_view::HostView;
use crate::sidebar_view::keychain_view::KeychainSidebarView;
use crate::sidebar_view::style::{border_color, button_bg, button_bg_hover, glass_bg, hover_bg};
use crate::sidebar_view::theme_view;
use crate::theme::ActiveTheme;

pub const SIDEBAR_MIN_WIDTH: f32 = 220.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 500.0;
const ANIMATION_ENTER_MS: u64 = 300;
const ANIMATION_EXIT_MS: u64 = 250;
const BUTTON_LEFT_CLOSED: f32 = 84.0;
const BUTTON_TOP: f32 = 12.0;
const TAB_GROUP_SECTION_TITLE: &str = "标签页组";
const SIDEBAR_ROW_HEIGHT: f32 = 28.0;
const SIDEBAR_ROW_GAP: f32 = 2.0;
const SIDEBAR_ROW_ICON_BOX: f32 = 16.0;
const SIDEBAR_ROW_ICON: f32 = 16.0;
const SIDEBAR_ACTION_BOX: f32 = 20.0;
const CHEVRON_RIGHT_SIZE: f32 = 8.0;
const CHEVRON_DOWN_SIZE: f32 = 12.0;
const MENU_COLLAPSE_ANIMATION_MS: u64 = 200;
pub const DEFAULT_TAB_GROUP_ID: &str = "default-tab-group";

struct TabGroupItem {
    id: String,
    label: String,
    collapsed: bool,
    collapse_epoch: u32,
    editing_input: Option<Entity<InputState>>,
    editing_subscription: Option<Subscription>,
}

#[derive(Clone)]
enum CollapseEpochTarget {
    DefaultTabGroup,
    TabGroup(String),
    TabGroups,
    Section(usize),
}

#[derive(Clone, PartialEq, Eq)]
pub struct SidebarTabItem {
    pub label: String,
    pub icon: Option<Icon>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarTabGroupState {
    pub id: String,
    pub label: String,
    pub collapsed: bool,
}

/// 侧边栏菜单项
#[derive(Clone)]
pub struct SidebarMenuItem {
    pub id: String,
    pub label: String,
    pub icon: Option<Icon>,
}

impl SidebarMenuItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>, icon: Option<Icon>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon,
        }
    }
}

/// 侧边栏菜单分组
#[derive(Clone)]
pub struct SidebarMenuSection {
    pub title: Option<String>,
    pub items: Vec<SidebarMenuItem>,
    pub collapsed: bool,
}

impl SidebarMenuSection {
    pub fn new(title: Option<impl Into<String>>, items: Vec<SidebarMenuItem>) -> Self {
        Self {
            title: title.map(|t| t.into()),
            items,
            collapsed: false,
        }
    }

    pub fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
    }
}

/// 侧边栏当前显示的视图
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarView {
    Menu,
    Theme,
    Font,
    Host,
    Keychain,
}

/// macOS 风格侧边栏组件
///
/// 封装了侧边栏的显隐状态、入场/退出动画、宽度拖拽调整、切换按钮和 TitleBar 补偿动画。
/// 内部包含多组菜单结构，支持选中高亮和点击回调。
pub struct MacosSidebar {
    visible: bool,
    animating_out: bool,
    epoch: u32,
    selected_id: Option<String>,
    sections: Vec<SidebarMenuSection>,
    tab_groups: Vec<TabGroupItem>,
    tab_group_tabs: HashMap<String, Vec<SidebarTabItem>>,
    tab_groups_collapsed: bool,
    default_tab_group_collapsed: bool,
    default_tab_group_epoch: u32,
    tab_groups_epoch: u32,
    tab_groups_hovered: bool,
    hovered_tab_group_id: Option<String>,
    next_tab_group_id: usize,
    section_epochs: Vec<u32>,
    hovered_section: Option<usize>,
    current_view: SidebarView,
    theme_scroll_handle: ScrollHandle,
    font_scroll_handle: ScrollHandle,
    font_view: Option<Entity<FontSettingsView>>,
    host_view: Option<Entity<HostView>>,
    keychain_view: Option<Entity<KeychainSidebarView>>,
    width: f32,
    resize_drag_x: Option<gpui::Pixels>,
    dragging: bool,
    app_window: Option<WeakEntity<crate::AppWindow>>,
    on_tab_group_select: Option<Rc<dyn Fn(&str, &mut Window, &mut App)>>,
    tab_group_menu_open: bool,
    tab_group_menu_group_id: Option<String>,
    tab_group_popup_menu: Option<Entity<PopupMenu>>,
    tab_group_menu_position: Option<gpui::Point<Pixels>>,
    tab_group_menu_subscription: Option<Subscription>,
}

impl MacosSidebar {
    pub fn new() -> Self {
        Self {
            visible: false,
            animating_out: false,
            epoch: 0,
            selected_id: Some(DEFAULT_TAB_GROUP_ID.to_string()),
            sections: vec![
                SidebarMenuSection::new(
                    Some("已保存"),
                    vec![
                        SidebarMenuItem::new("host", "Host", Some(Icon::Host)),
                        SidebarMenuItem::new("keychain", "Keychain", Some(Icon::KeyShield)),
                    ],
                ),
                SidebarMenuSection::new(
                    Some("偏好设置"),
                    vec![
                        SidebarMenuItem::new("themes", "主题", Some(Icon::Theme)),
                        SidebarMenuItem::new("font", "字体", Some(Icon::Font)),
                    ],
                ),
            ],
            tab_groups: Vec::new(),
            tab_group_tabs: HashMap::new(),
            tab_groups_collapsed: false,
            default_tab_group_collapsed: false,
            default_tab_group_epoch: 0,
            tab_groups_epoch: 0,
            tab_groups_hovered: false,
            hovered_tab_group_id: None,
            next_tab_group_id: 0,
            section_epochs: vec![0; 2],
            hovered_section: None,
            current_view: SidebarView::Menu,
            theme_scroll_handle: ScrollHandle::new(),
            font_scroll_handle: ScrollHandle::new(),
            font_view: None,
            host_view: None,
            keychain_view: None,
            width: 280.0,
            resize_drag_x: None,
            dragging: false,
            app_window: None,
            on_tab_group_select: None,
            tab_group_menu_open: false,
            tab_group_menu_group_id: None,
            tab_group_popup_menu: None,
            tab_group_menu_position: None,
            tab_group_menu_subscription: None,
        }
    }

    /// 绑定 AppWindow 用于状态变化时通知父组件重绘
    pub fn bind_app_window(&mut self, app_window: WeakEntity<crate::AppWindow>) {
        self.app_window = Some(app_window);
    }

    pub fn on_tab_group_select<F>(&mut self, callback: F)
    where
        F: Fn(&str, &mut Window, &mut App) + 'static,
    {
        self.on_tab_group_select = Some(Rc::new(callback));
    }

    pub fn set_tab_group_tabs(
        &mut self,
        group_id: impl Into<String>,
        tabs: Vec<SidebarTabItem>,
        cx: &mut Context<Self>,
    ) {
        let group_id = group_id.into();
        if self.tab_group_tabs.get(&group_id) == Some(&tabs) {
            return;
        }

        self.tab_group_tabs.insert(group_id, tabs);
        cx.notify();
    }

    pub fn tab_group_states(&self) -> Vec<SidebarTabGroupState> {
        self.tab_groups
            .iter()
            .map(|group| SidebarTabGroupState {
                id: group.id.clone(),
                label: group.label.clone(),
                collapsed: group.collapsed,
            })
            .collect()
    }

    pub fn restore_tab_groups(
        &mut self,
        groups: Vec<SidebarTabGroupState>,
        cx: &mut Context<Self>,
    ) {
        let mut seen_ids = std::collections::HashSet::new();
        self.tab_groups = groups
            .into_iter()
            .filter(|group| seen_ids.insert(group.id.clone()))
            .map(|group| TabGroupItem {
                id: group.id,
                label: group.label,
                collapsed: group.collapsed,
                collapse_epoch: 0,
                editing_input: None,
                editing_subscription: None,
            })
            .collect();
        self.next_tab_group_id = self
            .tab_groups
            .iter()
            .filter_map(|group| {
                group
                    .id
                    .strip_prefix("tab-group-")
                    .and_then(|suffix| suffix.parse::<usize>().ok())
            })
            .max()
            .map(|max_id| max_id + 1)
            .unwrap_or(0);
        cx.notify();
    }

    /// 通知父级 AppWindow 重绘
    pub fn notify_app_window(&self, cx: &mut App) {
        if let Some(aw) = &self.app_window {
            aw.update(cx, |_, cx| cx.notify()).ok();
        }
    }

    /// 当前宽度
    pub fn width(&self) -> f32 {
        self.width
    }

    /// 开始拖拽调整宽度
    pub fn start_resize(&mut self, pos: gpui::Pixels) {
        self.resize_drag_x = Some(pos);
    }

    /// 更新拖拽位置
    pub fn update_resize(&mut self, pos: gpui::Pixels) {
        if let Some(start) = self.resize_drag_x {
            let delta: f32 = (pos - start).into();
            self.width = (self.width + delta).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
            self.resize_drag_x = Some(pos);
        }
    }

    /// 结束拖拽
    pub fn end_resize(&mut self) {
        self.resize_drag_x = None;
    }

    /// 是否正在拖拽调整宽度
    pub fn is_dragging(&self) -> bool {
        self.resize_drag_x.is_some()
    }

    /// 切换侧边栏显隐。
    ///
    /// 返回 `true` 表示侧边栏正在关闭，需要外部启动一个延迟任务
    /// 在动画结束后调用 [`Self::finish_animation`]；返回 `false` 表示正在打开。
    pub fn toggle(&mut self) -> bool {
        self.end_resize();
        if self.visible {
            self.visible = false;
            self.animating_out = true;
            self.epoch = self.epoch.wrapping_add(1);
            true
        } else {
            self.visible = true;
            self.animating_out = false;
            self.epoch = self.epoch.wrapping_add(1);
            false
        }
    }

    /// 结束退出动画，标记侧边栏不再参与渲染。
    pub fn finish_animation(&mut self) {
        self.animating_out = false;
    }

    /// 选中指定 ID 的菜单项。
    pub fn select_item(&mut self, id: &str) {
        self.selected_id = Some(id.to_string());
    }

    /// 获取当前选中的菜单项 ID。
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    /// 导航到指定视图
    pub fn navigate_to(&mut self, view: SidebarView) {
        self.current_view = view;
    }

    /// 返回上一级视图
    pub fn go_back(&mut self) {
        self.current_view = SidebarView::Menu;
    }

    /// 获取当前视图
    pub fn current_view(&self) -> &SidebarView {
        &self.current_view
    }

    /// 设置字体设置视图
    pub fn set_font_view(&mut self, view: Entity<FontSettingsView>) {
        self.font_view = Some(view);
    }

    /// 设置 Host 视图
    pub fn set_host_view(&mut self, view: Entity<HostView>) {
        self.host_view = Some(view);
    }

    /// 设置 Keychain 视图
    pub fn set_keychain_view(&mut self, view: Entity<KeychainSidebarView>) {
        self.keychain_view = Some(view);
    }

    /// 处理侧边栏菜单项点击，统一封装导航逻辑。
    pub fn handle_item_select(&mut self, id: &str) {
        match id {
            "themes" => {
                self.select_item(id);
                self.navigate_to(SidebarView::Theme);
            }
            "font" => {
                self.select_item(id);
                self.navigate_to(SidebarView::Font);
            }
            "host" => {
                self.select_item(id);
                self.navigate_to(SidebarView::Host);
            }
            "keychain" => {
                self.select_item(id);
                self.navigate_to(SidebarView::Keychain);
            }
            "__back" => self.go_back(),
            _ => self.select_item(id),
        }
    }

    pub fn create_tab_group(&mut self, window: &mut Window, cx: &mut Context<Self>) -> String {
        self.commit_all_tab_group_names(cx);

        let id = format!("tab-group-{}", self.next_tab_group_id);
        let label = if self.next_tab_group_id == 0 {
            "未命名".to_string()
        } else {
            format!("未命名 {}", self.next_tab_group_id + 1)
        };
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).font_size(px(15.0));
            input.set_value(&label, window, cx);
            input.selected_range = (0..input.text().len()).into();
            input
        });

        let group_id = id.clone();
        let subscription =
            cx.subscribe(&input, move |this, _, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_tab_group_name(&group_id, cx);
                }
                _ => {}
            });

        self.next_tab_group_id += 1;
        self.selected_id = Some(id.clone());
        self.tab_groups.push(TabGroupItem {
            id: id.clone(),
            label,
            collapsed: false,
            collapse_epoch: 0,
            editing_input: Some(input.clone()),
            editing_subscription: Some(subscription),
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
        });

        id
    }

    fn commit_tab_group_name(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(group) = self.tab_groups.iter_mut().find(|group| group.id == id) else {
            return;
        };
        let Some(input) = group.editing_input.take() else {
            return;
        };
        group.editing_subscription.take();

        let value = input.read(cx).text().to_string();
        let value = value.trim();
        if !value.is_empty() {
            group.label = value.to_string();
        }
        cx.notify();
    }

    fn commit_all_tab_group_names(&mut self, cx: &mut Context<Self>) {
        for group in &mut self.tab_groups {
            let Some(input) = group.editing_input.take() else {
                continue;
            };

            group.editing_subscription.take();
            let value = input.read(cx).text().to_string();
            let value = value.trim();
            if !value.is_empty() {
                group.label = value.to_string();
            }
        }
    }

    fn rename_tab_group(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_all_tab_group_names(cx);

        let Some(group) = self.tab_groups.iter_mut().find(|group| group.id == id) else {
            return;
        };

        if group.editing_input.is_some() {
            return;
        }

        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).font_size(px(15.0));
            input.set_value(&group.label, window, cx);
            input.selected_range = (0..input.text().len()).into();
            input
        });

        let group_id = id.to_string();
        let subscription =
            cx.subscribe(&input, move |this, _, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_tab_group_name(&group_id, cx);
                }
                _ => {}
            });

        group.editing_input = Some(input.clone());
        group.editing_subscription = Some(subscription);

        input.update(cx, |input, cx| {
            input.focus(window, cx);
        });

        cx.notify();
    }

    fn delete_tab_group(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.tab_group_menu_group_id.as_deref() == Some(id) {
            self.close_tab_group_menu(cx);
        }

        self.tab_groups.retain(|group| group.id != id);
        self.tab_group_tabs.remove(id);

        if self.selected_id.as_deref() == Some(id) {
            self.selected_id = Some(DEFAULT_TAB_GROUP_ID.to_string());
        }

        cx.notify();
        self.notify_app_window(cx);
    }

    fn open_tab_group_menu(&mut self, group_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_group_menu(cx);

        let rename_group_id = group_id.to_string();
        let delete_group_id = group_id.to_string();
        let sidebar = cx.entity().downgrade();
        let sidebar2 = sidebar.clone();

        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            menu.item(
                PopupMenuItem::new("重新命名").on_click(move |_event, _window, cx| {
                    if let Some(sidebar) = sidebar.upgrade() {
                        sidebar.update(cx, |this, cx| {
                            this.close_tab_group_menu(cx);
                            this.rename_tab_group(&rename_group_id, _window, cx);
                        });
                    }
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new("删除").on_click(move |_event, _window, cx| {
                    if let Some(sidebar) = sidebar2.upgrade() {
                        sidebar.update(cx, |this, cx| {
                            this.close_tab_group_menu(cx);
                            this.delete_tab_group(&delete_group_id, cx);
                        });
                    }
                }),
            )
        });

        let subscription = cx.subscribe(&menu, move |this, _, _: &gpui::DismissEvent, cx| {
            this.close_tab_group_menu(cx);
        });

        self.tab_group_menu_open = true;
        self.tab_group_menu_group_id = Some(group_id.to_string());
        self.tab_group_menu_position = Some(window.mouse_position());
        self.tab_group_popup_menu = Some(menu);
        self.tab_group_menu_subscription = Some(subscription);
        cx.notify();
    }

    fn close_tab_group_menu(&mut self, cx: &mut Context<Self>) {
        self.tab_group_menu_open = false;
        self.tab_group_menu_group_id = None;
        self.tab_group_popup_menu = None;
        self.tab_group_menu_position = None;
        self.tab_group_menu_subscription.take();
        cx.notify();
    }

    fn toggle_tab_group_collapsed(&mut self, id: &str) -> Option<(CollapseEpochTarget, u32)> {
        if id == DEFAULT_TAB_GROUP_ID {
            self.default_tab_group_collapsed = !self.default_tab_group_collapsed;
            self.default_tab_group_epoch = self.default_tab_group_epoch.wrapping_add(1);
            Some((
                CollapseEpochTarget::DefaultTabGroup,
                self.default_tab_group_epoch,
            ))
        } else if let Some(group) = self.tab_groups.iter_mut().find(|group| group.id == id) {
            group.collapsed = !group.collapsed;
            group.collapse_epoch = group.collapse_epoch.wrapping_add(1);
            Some((
                CollapseEpochTarget::TabGroup(group.id.clone()),
                group.collapse_epoch,
            ))
        } else {
            None
        }
    }

    fn toggle_tab_groups_section(&mut self) -> u32 {
        self.tab_groups_collapsed = !self.tab_groups_collapsed;
        self.tab_groups_epoch = self.tab_groups_epoch.wrapping_add(1);
        self.tab_groups_epoch
    }

    fn reset_collapse_epoch_after_animation(
        cx: &mut Context<Self>,
        target: CollapseEpochTarget,
        epoch: u32,
    ) {
        let sidebar = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(MENU_COLLAPSE_ANIMATION_MS))
                .await;

            let _ = sidebar.update(cx, |this, cx| {
                let reset = match target {
                    CollapseEpochTarget::DefaultTabGroup => {
                        if this.default_tab_group_epoch == epoch {
                            this.default_tab_group_epoch = 0;
                            true
                        } else {
                            false
                        }
                    }
                    CollapseEpochTarget::TabGroups => {
                        if this.tab_groups_epoch == epoch {
                            this.tab_groups_epoch = 0;
                            true
                        } else {
                            false
                        }
                    }
                    CollapseEpochTarget::TabGroup(group_id) => {
                        if let Some(group) = this
                            .tab_groups
                            .iter_mut()
                            .find(|group| group.id == group_id)
                        {
                            if group.collapse_epoch == epoch {
                                group.collapse_epoch = 0;
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    }
                    CollapseEpochTarget::Section(section_idx) => {
                        if this.section_epochs.get(section_idx).copied() == Some(epoch) {
                            if let Some(section_epoch) = this.section_epochs.get_mut(section_idx) {
                                *section_epoch = 0;
                            }
                            true
                        } else {
                            false
                        }
                    }
                };

                if reset {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 当前是否可见（用于外部组件判断动画方向）。
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 当前动画 epoch（用于外部组件同步动画）。
    pub fn animation_epoch(&self) -> u32 {
        self.epoch
    }

    fn render_top_icon_button<F>(
        &self,
        cx: &gpui::App,
        is_fullscreen: bool,
        offset_from_toggle: f32,
        id: &'static str,
        icon: Icon,
        icon_size: f32,
        on_mouse_down: F,
    ) -> gpui::AnyElement
    where
        F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    {
        let theme = cx.theme().ui;
        let toggle_left_closed = if cfg!(target_os = "macos") && !is_fullscreen {
            BUTTON_LEFT_CLOSED
        } else {
            8.0
        };
        let toggle_left_open = self.width - 40.0;
        let button_left_closed = toggle_left_closed - offset_from_toggle;
        let button_left_open = toggle_left_open - offset_from_toggle;
        let (start_left, end_left, duration_ms) = if self.visible {
            (button_left_closed, button_left_open, ANIMATION_ENTER_MS)
        } else {
            (button_left_open, button_left_closed, ANIMATION_EXIT_MS)
        };

        let button = div()
            .id((id, self.epoch))
            .absolute()
            .top(px(if self.visible { BUTTON_TOP } else { 8.0 }))
            .flex()
            .items_center()
            .justify_center()
            .w(px(36.))
            .h(px(if self.visible { 22. } else { 30. }))
            .rounded(px(999.0))
            .text_color(theme.text)
            .hover(|style| style.bg(theme.title_bar_hover))
            .cursor_pointer()
            .on_mouse_down(gpui::MouseButton::Left, on_mouse_down)
            .child(
                gpui::svg()
                    .size(px(icon_size))
                    .text_color(theme.text)
                    .path(icon.path()),
            );

        if self.epoch == 0 {
            button.left(px(end_left)).into_any_element()
        } else {
            button
                .left(px(start_left))
                .with_animation(
                    (id, self.epoch),
                    gpui::Animation::new(Duration::from_millis(duration_ms))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| this.left(px(start_left + (end_left - start_left) * delta)),
                )
                .into_any_element()
        }
    }

    pub fn render_new_tab_group_button<F>(
        &self,
        cx: &gpui::App,
        is_fullscreen: bool,
        on_create: F,
    ) -> gpui::AnyElement
    where
        F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    {
        if !self.visible {
            return div().into_any_element();
        }

        self.render_top_icon_button(
            cx,
            is_fullscreen,
            40.0,
            "new-tab-group-btn",
            Icon::PlusSquareOnSquare,
            SIDEBAR_ROW_ICON,
            on_create,
        )
    }

    /// 渲染侧边栏切换按钮（含位移动画）。
    ///
    /// 事件处理器通过 `on_toggle` 传入，调用者通常使用 `cx.listener` 构造。
    /// `is_fullscreen` 用于 macOS 全屏模式：此时没有交通灯按钮，无需额外左偏移。
    pub fn render_toggle_button<F>(
        &self,
        cx: &gpui::App,
        is_fullscreen: bool,
        on_toggle: F,
    ) -> gpui::AnyElement
    where
        F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    {
        self.render_top_icon_button(
            cx,
            is_fullscreen,
            0.0,
            "sidebar-toggle-btn",
            Icon::Sidebar,
            20.0,
            on_toggle,
        )
    }

    fn render_menu_content(
        &self,
        cx: &mut Context<Self>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div {
        let mut menu_content = div().w_full().flex().flex_col();
        let default_tabs = self
            .tab_group_tabs
            .get(DEFAULT_TAB_GROUP_ID)
            .cloned()
            .unwrap_or_default();
        let default_label = format!("{}个标签页", default_tabs.len());

        menu_content = menu_content
            .child(self.render_tab_group_row(
                cx,
                is_dark,
                theme,
                DEFAULT_TAB_GROUP_ID.to_string(),
                default_label,
                self.default_tab_group_collapsed,
                None,
                false,
                Icon::Macbook,
            ))
            .child(Self::render_collapsible_content(
                self.render_tab_group_tabs(DEFAULT_TAB_GROUP_ID, &default_tabs, theme),
                self.default_tab_group_collapsed,
                self.default_tab_group_epoch,
                (default_tabs.len() as f32 * (SIDEBAR_ROW_HEIGHT + SIDEBAR_ROW_GAP)
                    - SIDEBAR_ROW_GAP)
                    .max(0.0),
                ("default-tab-group-tabs", self.default_tab_group_epoch),
            ))
            .child(div().h(px(16.0)))
            .child(self.render_tab_groups_section(cx, is_dark, theme))
            .child(div().h(px(12.0)));

        // 分组菜单
        let mut item_index: usize = 0;
        for section_idx in 0..self.sections.len() {
            let section = &self.sections[section_idx];

            let section_epoch = self.section_epochs.get(section_idx).copied().unwrap_or(0);

            if let Some(ref title) = section.title {
                let is_collapsed = section.collapsed;
                let is_hovered = self.hovered_section == Some(section_idx);

                let chevron_icon = if is_collapsed {
                    gpui::svg()
                        .size(px(8.0))
                        .path(Icon::ChevronRight.path())
                        .text_color(theme.text_subtle)
                } else {
                    gpui::svg()
                        .size(px(12.0))
                        .path(Icon::ChevronDown.path())
                        .text_color(theme.text_subtle)
                };

                let sidebar = cx.entity().downgrade();
                let on_hover = move |hovered: &bool, _: &mut Window, cx: &mut App| {
                    if let Some(sidebar) = sidebar.upgrade() {
                        sidebar.update(cx, |this, cx| {
                            this.hovered_section = if *hovered { Some(section_idx) } else { None };
                            cx.notify();
                        });
                    }
                };

                menu_content = menu_content.child(
                    div()
                        .id(format!("sidebar-section-header-{}", section_idx))
                        .h(px(28.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(8.0))
                        .pt_3()
                        .pb_3()
                        .on_hover(on_hover)
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text_subtle)
                                .child(title.clone()),
                        )
                        .child(
                            div()
                                .w(px(20.0))
                                .h(px(20.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .cursor_pointer()
                                .when(is_hovered, |this| this.child(chevron_icon))
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |this, _, _window, cx| {
                                        if let Some(section) = this.sections.get_mut(section_idx) {
                                            section.toggle_collapsed();
                                            let mut next_epoch = None;
                                            if let Some(epoch) =
                                                this.section_epochs.get_mut(section_idx)
                                            {
                                                *epoch = epoch.wrapping_add(1);
                                                next_epoch = Some(*epoch);
                                            }
                                            cx.notify();
                                            if let Some(epoch) = next_epoch {
                                                Self::reset_collapse_epoch_after_animation(
                                                    cx,
                                                    CollapseEpochTarget::Section(section_idx),
                                                    epoch,
                                                );
                                            }
                                        }
                                    }),
                                ),
                        ),
                );
            }

            let mut items_div = div().w_full().flex().flex_col().gap(px(2.0));
            for item in &section.items {
                let item_id = item.id.clone();
                let is_selected = self.selected_id.as_deref() == Some(&item_id);
                let icon = item.icon.unwrap_or(Icon::Screen);
                let current_index = item_index;
                item_index += 1;

                let row = div()
                    .id(("sidebar-item", current_index))
                    .h(px(28.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(10.0))
                    .gap(px(6.0))
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
                            .size(px(SIDEBAR_ROW_ICON_BOX))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                gpui::svg()
                                    .size(px(SIDEBAR_ROW_ICON))
                                    .path(icon.path())
                                    .text_color(if is_selected {
                                        theme.accent_foreground
                                    } else {
                                        theme.text
                                    }),
                            ),
                    )
                    .child(
                        div().flex_1().child(
                            div()
                                .text_sm()
                                .text_color(if is_selected {
                                    theme.accent_foreground
                                } else {
                                    theme.text
                                })
                                .child(item.label.clone()),
                        ),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.handle_item_select(&item_id);
                        this.notify_app_window(cx);
                    }));

                items_div = items_div.child(row);
            }

            let animated_content: gpui::AnyElement = if section.title.is_some() {
                Self::render_collapsible_content(
                    items_div,
                    section.collapsed,
                    section_epoch,
                    (section.items.len() as f32 * 30.0 - 2.0).max(0.0),
                    ("section-content", section_idx),
                )
            } else {
                items_div.into_any_element()
            };

            menu_content = menu_content.child(animated_content);

            menu_content = menu_content.child(div().h(px(12.0)));
        }

        menu_content
    }

    fn render_tab_groups_section(
        &self,
        cx: &mut Context<Self>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div {
        let chevron_icon = if self.tab_groups_collapsed {
            gpui::svg()
                .size(px(CHEVRON_RIGHT_SIZE))
                .path(Icon::ChevronRight.path())
                .text_color(theme.text_subtle)
        } else {
            gpui::svg()
                .size(px(CHEVRON_DOWN_SIZE))
                .path(Icon::ChevronDown.path())
                .text_color(theme.text_subtle)
        };
        let sidebar = cx.entity().downgrade();
        let on_hover = move |hovered: &bool, _: &mut Window, cx: &mut App| {
            if let Some(sidebar) = sidebar.upgrade() {
                sidebar.update(cx, |this, cx| {
                    this.tab_groups_hovered = *hovered;
                    cx.notify();
                });
            }
        };

        let header = div()
            .id("sidebar-tab-groups-header")
            .h(px(28.0))
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .px(px(8.0))
            .pt_3()
            .pb_3()
            .on_hover(on_hover)
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_subtle)
                    .child(TAB_GROUP_SECTION_TITLE),
            )
            .child(
                div()
                    .id("sidebar-tab-groups-toggle")
                    .size(px(SIDEBAR_ACTION_BOX))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .when(self.tab_groups_hovered, |this| this.child(chevron_icon))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _window, cx| {
                            cx.stop_propagation();
                            let epoch = this.toggle_tab_groups_section();
                            cx.notify();
                            this.notify_app_window(cx);
                            Self::reset_collapse_epoch_after_animation(
                                cx,
                                CollapseEpochTarget::TabGroups,
                                epoch,
                            );
                        }),
                    ),
            );

        let mut groups = div().w_full().flex().flex_col().gap(px(2.0));
        for (index, group) in self.tab_groups.iter().enumerate() {
            let tabs = self
                .tab_group_tabs
                .get(&group.id)
                .cloned()
                .unwrap_or_default();
            groups = groups
                .child(self.render_tab_group_row(
                    cx,
                    is_dark,
                    theme,
                    group.id.clone(),
                    group.label.clone(),
                    group.collapsed,
                    group.editing_input.clone(),
                    true,
                    Icon::SquareOnSquare,
                ))
                .child(Self::render_collapsible_content(
                    self.render_tab_group_tabs(&group.id, &tabs, theme),
                    group.collapsed,
                    group.collapse_epoch,
                    (tabs.len() as f32 * (SIDEBAR_ROW_HEIGHT + SIDEBAR_ROW_GAP) - SIDEBAR_ROW_GAP)
                        .max(0.0),
                    ("tab-group-tabs", index),
                ));
        }

        let visible_group_rows = self.tab_groups.len();
        let visible_tab_rows = self
            .tab_groups
            .iter()
            .filter(|group| !group.collapsed)
            .map(|group| {
                self.tab_group_tabs
                    .get(&group.id)
                    .map(|tabs| tabs.len())
                    .unwrap_or(0)
            })
            .sum::<usize>();
        let total_rows = visible_group_rows + visible_tab_rows;
        let full_height =
            (total_rows as f32 * (SIDEBAR_ROW_HEIGHT + SIDEBAR_ROW_GAP) - SIDEBAR_ROW_GAP).max(0.0);
        let groups_element = Self::render_collapsible_content(
            groups,
            self.tab_groups_collapsed,
            self.tab_groups_epoch,
            full_height,
            ("tab-groups-content", self.tab_groups_epoch),
        );

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(header)
            .child(groups_element)
    }

    fn render_tab_group_row(
        &self,
        cx: &mut Context<Self>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
        group_id: String,
        label: String,
        collapsed: bool,
        editing_input: Option<Entity<InputState>>,
        show_more_action: bool,
        icon: Icon,
    ) -> gpui::AnyElement {
        let is_selected = self.selected_id.as_deref() == Some(group_id.as_str());
        let icon_color = if is_selected {
            theme.accent_foreground
        } else {
            theme.text
        };
        let text_color = if is_selected {
            theme.accent_foreground
        } else {
            theme.text
        };
        let is_row_hovered = self.hovered_tab_group_id.as_deref() == Some(group_id.as_str());
        let sidebar = cx.entity().downgrade();
        let hover_group_id = group_id.clone();
        let on_hover = move |hovered: &bool, _: &mut Window, cx: &mut App| {
            if let Some(sidebar) = sidebar.upgrade() {
                let hover_group_id = hover_group_id.clone();
                sidebar.update(cx, |this, cx| {
                    if *hovered {
                        this.hovered_tab_group_id = Some(hover_group_id);
                    } else if this.hovered_tab_group_id.as_deref() == Some(hover_group_id.as_str())
                    {
                        this.hovered_tab_group_id = None;
                    }
                    cx.notify();
                });
            }
        };

        let mut row = div()
            .id(format!("sidebar-tab-group-{}", group_id))
            .h(px(SIDEBAR_ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(6.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .on_hover(on_hover)
            .when(is_selected, |this| {
                this.bg(theme.accent).text_color(theme.accent_foreground)
            })
            .when(!is_selected, |this| {
                this.hover(|style| style.bg(hover_bg(is_dark)))
                    .text_color(theme.text)
            })
            .child(
                div()
                    .size(px(SIDEBAR_ROW_ICON_BOX))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        gpui::svg()
                            .size(px(SIDEBAR_ROW_ICON))
                            .path(icon.path())
                            .text_color(icon_color),
                    ),
            )
            .child(match editing_input.clone() {
                Some(input) => div()
                    .flex_1()
                    .min_w_0()
                    .h(px(24.0))
                    .bg(hover_bg(is_dark))
                    .border(px(2.0))
                    .border_color(theme.accent.opacity(0.65))
                    .overflow_hidden()
                    .child(EditorInput::new(&input).transparent())
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(text_color)
                    .child(label)
                    .into_any_element(),
            });

        if editing_input.is_none() {
            if show_more_action {
                row = row.child(
                    div()
                        .size(px(SIDEBAR_ACTION_BOX))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .when(is_row_hovered, |this| {
                            this.child(
                                gpui::svg()
                                    .size(px(SIDEBAR_ROW_ICON))
                                    .path(Icon::EllipsisCircleFill.path())
                                    .text_color(icon_color),
                            )
                        })
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener({
                                let group_id = group_id.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_tab_group_menu(&group_id, window, cx);
                                }
                            }),
                        ),
                );
            }

            row = row.child(
                div()
                    .size(px(SIDEBAR_ACTION_BOX))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .when(is_row_hovered, |this| {
                        this.child(
                            gpui::svg()
                                .size(if collapsed {
                                    px(CHEVRON_RIGHT_SIZE)
                                } else {
                                    px(CHEVRON_DOWN_SIZE)
                                })
                                .path(if collapsed {
                                    Icon::ChevronRight.path()
                                } else {
                                    Icon::ChevronDown.path()
                                })
                                .text_color(theme.text_subtle),
                        )
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener({
                            let group_id = group_id.clone();
                            move |this, _, _window, cx| {
                                cx.stop_propagation();
                                let collapse_epoch = this.toggle_tab_group_collapsed(&group_id);
                                cx.notify();
                                this.notify_app_window(cx);
                                if let Some((target, epoch)) = collapse_epoch {
                                    Self::reset_collapse_epoch_after_animation(cx, target, epoch);
                                }
                            }
                        }),
                    ),
            );
        }

        let is_menu_open = self.tab_group_menu_open
            && self.tab_group_menu_group_id.as_deref() == Some(group_id.as_str());

        let click_group_id = group_id.clone();
        let mut row = row
            .on_click(cx.listener(move |this, _, window, cx| {
                this.handle_item_select(&click_group_id);
                if let Some(callback) = this.on_tab_group_select.clone() {
                    callback(&click_group_id, window, cx);
                }
                this.notify_app_window(cx);
            }))
            .when(is_menu_open, |this| {
                if let Some(menu) = self.tab_group_popup_menu.clone() {
                    let position = self
                        .tab_group_menu_position
                        .unwrap_or(gpui::Point::default());
                    this.child(
                        deferred(
                            anchored()
                                .position(position)
                                .anchor(gpui::Corner::TopLeft)
                                .snap_to_window_with_margin(px(8.))
                                .child(menu),
                        )
                        .with_priority(100),
                    )
                } else {
                    this
                }
            });

        if show_more_action {
            let right_click_group_id = group_id.clone();
            row = row.on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.open_tab_group_menu(&right_click_group_id, window, cx);
                }),
            );
        }

        row.into_any_element()
    }

    fn render_tab_group_tabs(
        &self,
        group_id: &str,
        tabs: &[SidebarTabItem],
        theme: crate::theme::UiTheme,
    ) -> gpui::Div {
        let mut list = div().w_full().flex().flex_col().gap(px(2.0));

        for (index, tab) in tabs.iter().enumerate() {
            let icon = tab.icon.unwrap_or(Icon::Terminal);
            let text_color = if tab.active {
                theme.text
            } else {
                theme.text_muted
            };
            list = list.child(
                div()
                    .id(format!("sidebar-tab-group-tab-{}-{}", group_id, index))
                    .h(px(SIDEBAR_ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .pl(px(26.0))
                    .pr(px(10.0))
                    .gap(px(6.0))
                    .rounded(px(8.0))
                    .child(
                        div()
                            .size(px(SIDEBAR_ROW_ICON_BOX))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                gpui::svg()
                                    .size(px(SIDEBAR_ROW_ICON))
                                    .path(icon.path())
                                    .text_color(text_color),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_sm()
                            .text_color(text_color)
                            .child(tab.label.clone()),
                    ),
            );
        }

        list
    }

    fn render_collapsible_content(
        content: gpui::Div,
        collapsed: bool,
        epoch: u32,
        full_height: f32,
        animation_id: impl Into<gpui::ElementId>,
    ) -> gpui::AnyElement {
        if epoch == 0 {
            if collapsed {
                content
                    .h(px(0.0))
                    .overflow_hidden()
                    .opacity(0.0)
                    .into_any_element()
            } else {
                content.into_any_element()
            }
        } else {
            let start_height = if collapsed { full_height } else { 0.0 };
            let start_opacity = if collapsed { 1.0 } else { 0.0 };

            content
                .overflow_hidden()
                .h(px(start_height))
                .opacity(start_opacity)
                .with_animation(
                    animation_id,
                    Animation::new(Duration::from_millis(MENU_COLLAPSE_ANIMATION_MS))
                        .with_easing(ease_in_out),
                    move |this, delta| {
                        let height = if collapsed {
                            full_height * (1.0 - delta)
                        } else {
                            full_height * delta
                        };
                        let opacity = if collapsed { 1.0 - delta } else { delta };
                        this.h(px(height)).opacity(opacity)
                    },
                )
                .into_any_element()
        }
    }

    fn render_font_content(
        &self,
        cx: &mut Context<Self>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div {
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
                            .size(px(8.0))
                            .path(Icon::ChevronLeft.path())
                            .text_color(theme.text),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _window, cx| {
                            this.handle_item_select("__back");
                            this.notify_app_window(cx);
                        }),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child("字体"),
            )
            .child(div().w(px(28.0)));

        let font_content = self
            .font_view
            .clone()
            .map(|view| view.into_any_element())
            .unwrap_or_else(|| div().into_any_element());

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("font-scroll")
                    .track_scroll(&self.font_scroll_handle)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(font_content),
            )
    }

    fn render_theme_content(
        &self,
        cx: &mut Context<Self>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div {
        let on_select = Rc::new({
            let sidebar = cx.entity().downgrade();
            move |id: &str, _window: &mut Window, cx: &mut App| {
                if let Some(s) = sidebar.upgrade() {
                    s.update(cx, |this, cx| {
                        this.handle_item_select(id);
                        this.notify_app_window(cx);
                    });
                }
            }
        });
        theme_view::render_theme_sidebar(cx, on_select, is_dark, theme, &self.theme_scroll_handle)
    }

    fn render_host_content(&self) -> gpui::Div {
        self.host_view
            .clone()
            .map(|view| div().w_full().h_full().child(view))
            .unwrap_or_else(|| div().w_full().h_full())
    }

    fn render_keychain_content(&self) -> gpui::Div {
        self.keychain_view
            .clone()
            .map(|view| div().w_full().h_full().child(view))
            .unwrap_or_else(|| div().w_full().h_full())
    }
}

impl Default for MacosSidebar {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for MacosSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible && !self.animating_out {
            return div().h_full().w(px(0.0)).into_any_element();
        }

        let target_width = self.width;
        let is_dark = cx.theme().appearance.is_dark();
        let theme = cx.theme().ui;

        let (anim_id, anim_key, duration_ms, width_fn): (&str, &str, u64, Box<dyn Fn(f32) -> f32>) =
            if self.visible {
                (
                    "macos-sidebar-enter",
                    "sidebar-enter",
                    ANIMATION_ENTER_MS,
                    Box::new(move |delta| target_width * delta),
                )
            } else {
                (
                    "macos-sidebar-exit",
                    "sidebar-exit",
                    ANIMATION_EXIT_MS,
                    Box::new(move |delta| target_width * (1.0 - delta)),
                )
            };

        let is_macos = cfg!(target_os = "macos");
        let is_web = cfg!(target_arch = "wasm32");

        // 顶部空白区域：允许在此处拖拽整个窗口；按钮稍后渲染在上方，不会阻挡点击。
        let drag_region = div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .h(px(48.0))
            .when(!is_macos && !is_web, |this| {
                this.window_control_area(WindowControlArea::Drag)
            })
            .when(is_macos, |this| {
                this.on_mouse_down_out(cx.listener(|this, _, _, _| {
                    this.dragging = false;
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.dragging = false;
                    }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.dragging = true;
                    }),
                )
                .on_mouse_move(cx.listener(|this, _, window, _| {
                    if this.dragging {
                        this.dragging = false;
                        window.start_window_move();
                    }
                }))
            });

        let inner = div()
            .relative()
            .h_full()
            .rounded(px(12.0))
            .bg(glass_bg(is_dark))
            .border(px(0.5))
            .border_color(border_color(is_dark))
            .flex()
            .flex_col()
            .pt(px(48.0))
            .pb(px(16.0))
            .pl(px(12.0))
            .pr(px(12.0))
            .child(drag_region)
            .child(match self.current_view {
                SidebarView::Menu => self
                    .render_menu_content(cx, is_dark, theme)
                    .into_any_element(),
                SidebarView::Theme => self
                    .render_theme_content(cx, is_dark, theme)
                    .into_any_element(),
                SidebarView::Font => self
                    .render_font_content(cx, is_dark, theme)
                    .into_any_element(),
                SidebarView::Host => self.render_host_content().into_any_element(),
                SidebarView::Keychain => self.render_keychain_content().into_any_element(),
            });

        // Resize handle：贴 sidebar 右边缘，默认透明，hover 显淡色背景
        let resize_handle = div()
            .absolute()
            .right(px(0.0))
            .top(px(8.0))
            .bottom(px(8.0))
            .w(px(4.0))
            .cursor_col_resize()
            .hover(|style| style.bg(theme.drop_target.opacity(0.3)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.start_resize(event.position.x);
                    cx.notify();
                    this.notify_app_window(cx);
                }),
            );

        // 控制侧边栏整体轮廓
        let padded = div()
            .w(px(target_width))
            .pl(px(8.0))
            .pt(px(8.0))
            .pb(px(8.0))
            .h_full()
            .relative()
            .child(inner)
            .child(resize_handle);

        div()
            .id((anim_id, self.epoch))
            .h_full()
            .overflow_hidden()
            .child(padded)
            .child(SidebarResizeElement {
                sidebar: cx.entity().downgrade(),
            })
            .with_animation(
                (anim_key, self.epoch),
                gpui::Animation::new(Duration::from_millis(duration_ms))
                    .with_easing(gpui::ease_in_out),
                move |this, delta| {
                    let width = width_fn(delta);
                    this.w(px(width.max(0.0)))
                },
            )
            .into_any_element()
    }
}

/// 全局 resize 处理元素 - 在 paint 阶段注册全局鼠标事件
struct SidebarResizeElement {
    sidebar: WeakEntity<MacosSidebar>,
}

impl Element for SidebarResizeElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(gpui::Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) -> () {
        ()
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut (),
        _prepaint_state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(sidebar) = self.sidebar.upgrade() else {
            return;
        };

        // 拖拽中全局覆盖光标样式，保持 resize 光标不变
        if sidebar.read(cx).is_dragging() {
            window.set_window_cursor_style(CursorStyle::ResizeColumn);
        }

        // 全局鼠标移动事件 - 处理 resize 拖拽
        window.on_mouse_event({
            let sidebar = sidebar.clone();
            move |event: &MouseMoveEvent, _, _window, cx| {
                sidebar.update(cx, |this, cx| {
                    if this.is_dragging() {
                        this.update_resize(event.position.x);
                        cx.notify();
                        this.notify_app_window(cx);
                    }
                });
            }
        });

        // 全局鼠标释放事件 - 结束 resize 拖拽
        window.on_mouse_event({
            let sidebar = sidebar.clone();
            move |_: &MouseUpEvent, _, _, cx| {
                sidebar.update(cx, |this, cx| {
                    if this.is_dragging() {
                        this.end_resize();
                        cx.notify();
                        this.notify_app_window(cx);
                    }
                });
            }
        });
    }
}

impl IntoElement for SidebarResizeElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}
