//! macOS 风格侧边栏（应用导航壳）。
//!
//! 本目录按职责拆分：
//! - [`self`]（mod.rs）：公共类型、状态模型、显隐/动画/宽度拖拽/导航与 `Render` 入口
//! - [`tab_groups`]：标签页组的状态管理（创建/重命名/删除/折叠）与渲染
//! - [`file_tree`]：文件树与 Git 面板的接线（订阅、会话绑定）与渲染
//! - [`content`]：菜单列表与各二级页面（主题/字体/主机/密钥串）渲染、顶部 chrome 按钮
//! - [`resize`]：宽度拖拽的全局鼠标事件元素

mod content;
mod file_tree;
mod resize;
mod tab_groups;

use resize::SidebarResizeElement;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, BoxShadow, Context, CursorStyle, Element, Entity, EventEmitter,
    GlobalElementId, InteractiveElement, IntoElement, LayoutId, MouseButton, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, ScrollHandle, SharedString, Subscription, WeakEntity, Window,
    WindowControlArea, anchored, deferred, div, ease_in_out, point, px,
};

use crate::assets::Icon;
use crate::callbacks::OptionalStrCallback;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::menu::{PopupMenu, PopupMenuItem};
use crate::file_manager::file_tree_event::FileTreeEvent;
use crate::file_manager::filesystem::{FileSystem, LocalFileSystem, RemoteFileSystem};
use crate::sidebar_view::file_tree::{FileTreePanel, FileTreePanelTabs, SidebarFileTree};
use crate::sidebar_view::file_tree_panel_tabs::{self, TAB_ITEMS};
use crate::sidebar_view::font_view::FontSettingsView;
use crate::sidebar_view::host_view::HostView;
use crate::sidebar_view::keychain_view::KeychainSidebarView;
use crate::sidebar_view::style::{
    border_color, button_bg, button_bg_hover, glass_bg, glass_bottom_shade, glass_top_highlight,
    hover_bg,
};
use crate::sidebar_view::theme_view;
use crate::ssh::SshConnectionConfig;
use crate::terminal_view::TerminalView;
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

#[derive(Clone, PartialEq, Eq)]
pub struct SidebarTabItem {
    pub label: String,
    pub icon: Option<Icon>,
    pub active: bool,
    pub tab_id: Option<String>,
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
#[derive(Clone)]
pub enum SidebarEvent {
    TabGroupsChanged,
    OpenFile {
        path: PathBuf,
        ssh_config: Option<Box<SshConnectionConfig>>,
    },
    /// Instant open; content loads asynchronously in DiffView.
    OpenDiffLoading {
        path: String,
        repo: crate::editor::git_repository::GitRepository,
    },
    /// A file's staging state changed (staged or unstaged by the Git panel).
    /// The HEAD → worktree diff for that path must be recomputed.
    GitIndexChanged {
        path: String,
    },
}

impl std::fmt::Debug for SidebarEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TabGroupsChanged => write!(f, "TabGroupsChanged"),
            Self::OpenFile { path, .. } => f.debug_struct("OpenFile").field("path", path).finish(),
            Self::OpenDiffLoading { path, .. } => f
                .debug_struct("OpenDiffLoading")
                .field("path", path)
                .finish(),
            Self::GitIndexChanged { path } => f
                .debug_struct("GitIndexChanged")
                .field("path", path)
                .finish(),
        }
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
    FileTree,
}

/// Tab 点击回调：`(group_id, tab_id, tab_title, sidebar, window, cx)`。
type TabClickCallback = Rc<dyn Fn(&str, &str, &str, &mut MacosSidebar, &mut Window, &mut App)>;

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
    file_tree_view: Option<Entity<SidebarFileTree>>,
    file_tree_target_label: Option<String>,
    file_tree_terminal_view: Option<Entity<TerminalView>>,
    file_tree_subscription: Option<Subscription>,
    file_tree_event_subscription: Option<Subscription>,
    file_tree_panel_tabs: FileTreePanelTabs,
    git_panel: Option<Entity<crate::sidebar_view::git_panel::GitPanel>>,
    git_panel_subscription: Option<Subscription>,
    width: f32,
    resize_drag_x: Option<gpui::Pixels>,
    dragging: bool,
    app_window: Option<WeakEntity<crate::AppWindow>>,
    on_tab_group_select: OptionalStrCallback,
    on_tab_double_click: Option<TabClickCallback>,
    on_tab_single_click: Option<TabClickCallback>,

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
            file_tree_view: None,
            file_tree_target_label: None,
            file_tree_terminal_view: None,
            file_tree_subscription: None,
            file_tree_event_subscription: None,
            file_tree_panel_tabs: FileTreePanelTabs::new(),
            git_panel: None,
            git_panel_subscription: None,
            width: 280.0,
            resize_drag_x: None,
            dragging: false,
            app_window: None,
            on_tab_group_select: None,
            on_tab_double_click: None,
            on_tab_single_click: None,
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

    pub fn on_tab_double_click<F>(&mut self, callback: F)
    where
        F: Fn(&str, &str, &str, &mut Self, &mut Window, &mut App) + 'static,
    {
        self.on_tab_double_click = Some(Rc::new(callback));
    }

    pub fn on_tab_single_click<F>(&mut self, callback: F)
    where
        F: Fn(&str, &str, &str, &mut Self, &mut Window, &mut App) + 'static,
    {
        self.on_tab_single_click = Some(Rc::new(callback));
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

    /// 返回上一级视图（清除文件树订阅与 watcher）
    pub fn go_back(&mut self, cx: &mut Context<Self>) {
        self.file_tree_subscription.take();
        if let Some(tree) = self.file_tree_view.take() {
            tree.update(cx, |tree, cx| tree.cleanup(cx));
        }
        self.file_tree_terminal_view = None;
        self.file_tree_target_label = None;
        self.file_tree_panel_tabs = FileTreePanelTabs::new();
        self.current_view = SidebarView::Menu;
        self.selected_id = Some(DEFAULT_TAB_GROUP_ID.to_string());
    }

    /// 获取当前视图
    pub fn current_view(&self) -> &SidebarView {
        &self.current_view
    }

    pub fn file_tree_terminal_view(&self) -> Option<Entity<TerminalView>> {
        self.file_tree_terminal_view.clone()
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
    pub fn handle_item_select(&mut self, id: &str, cx: &mut Context<Self>) {
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
            "__back" => self.go_back(cx),
            _ => self.select_item(id),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 当前动画 epoch（用于外部组件同步动画）。
    pub fn animation_epoch(&self) -> u32 {
        self.epoch
    }
}

impl Default for MacosSidebar {
    fn default() -> Self {
        Self::new()
    }
}

impl EventEmitter<SidebarEvent> for MacosSidebar {}

/// 侧边栏显隐动画参数：`(动画元素 id, 动画键, 时长(ms), 宽度缓动函数)`。
type SidebarAnimSpec<'a> = (&'a str, &'a str, u64, Box<dyn Fn(f32) -> f32>);

impl Render for MacosSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible && !self.animating_out {
            return div().h_full().w(px(0.0)).into_any_element();
        }

        let target_width = self.width;
        let is_dark = cx.theme().appearance.is_dark();
        let theme = cx.theme().ui;

        let (anim_id, anim_key, duration_ms, width_fn): SidebarAnimSpec<'_> = if self.visible {
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

        // 玻璃装饰层：顶部反光 + 底部体积阴影，均带 12px 圆角与容器对齐
        //（GPUI 的 absolute 子层不做圆角裁剪，矩形层会以直角伸出容器外）。
        // 高度拉到 140/120px 让渐隐更缓，与 glass_bg 的整高明度坡度无缝衔接，
        // 避免在 72px 处出现新的色阶拐点。二者不注册鼠标监听，不形成交互
        // hitbox，不挡下层交互。
        let glass_highlight = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(140.0))
            .rounded(px(12.0))
            .bg(glass_top_highlight(theme, is_dark));
        let glass_shade = div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(120.0))
            .rounded(px(12.0))
            .bg(glass_bottom_shade(theme, is_dark));

        let inner = div()
            .relative()
            .h_full()
            .rounded(px(12.0))
            .bg(glass_bg(theme, is_dark))
            .border(px(if is_dark { 0.5 } else { 0.75 }))
            .border_color(border_color(theme, is_dark))
            // 双层投影制造悬浮感：近影收紧轮廓、远影大范围柔化，
            // 亮色下明显加强以弥补白色主题下描边不足的层次。
            .shadow(vec![
                BoxShadow {
                    color: theme.shadow.opacity(if is_dark { 0.45 } else { 0.22 }),
                    offset: point(px(0.0), px(2.0)),
                    blur_radius: px(8.0),
                    spread_radius: px(-1.0),
                    inset: false,
                },
                BoxShadow {
                    color: theme.shadow.opacity(if is_dark { 0.34 } else { 0.14 }),
                    offset: point(px(0.0), px(14.0)),
                    blur_radius: px(36.0),
                    spread_radius: px(-10.0),
                    inset: false,
                },
            ])
            .flex()
            .flex_col()
            .pt(px(48.0))
            .pb(px(16.0))
            .pl(px(12.0))
            .pr(px(12.0))
            .child(glass_shade)
            .child(glass_highlight)
            .child(drag_region)
            .child(match self.current_view {
                SidebarView::Menu => self.render_menu_content(cx, theme).into_any_element(),
                SidebarView::Theme => self.render_theme_content(cx, theme).into_any_element(),
                SidebarView::Font => self.render_font_content(cx, theme).into_any_element(),
                SidebarView::Host => self.render_host_content().into_any_element(),
                SidebarView::Keychain => self.render_keychain_content().into_any_element(),
                SidebarView::FileTree => self.render_file_tree_content(cx).into_any_element(),
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
