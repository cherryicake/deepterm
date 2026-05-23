use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{AnimationExt, Entity, ScrollHandle, StatefulInteractiveElement, Window, div, px};

use crate::assets::Icon;
use crate::sidebar_view::font_view::FontSettingsView;
use crate::sidebar_view::style::{border_color, button_bg, button_bg_hover, glass_bg, hover_bg};
use crate::sidebar_view::host_view::HostView;
use crate::sidebar_view::{host_view, keychain_view, theme_view};
use crate::theme::ActiveTheme;

const SIDEBAR_WIDTH: f32 = 260.0;
const ANIMATION_ENTER_MS: u64 = 300;
const ANIMATION_EXIT_MS: u64 = 250;
const BUTTON_LEFT_OPEN: f32 = 220.0;
const BUTTON_LEFT_CLOSED: f32 = 84.0;
const BUTTON_TOP: f32 = 12.0;

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
}

impl SidebarMenuSection {
    pub fn new(title: Option<impl Into<String>>, items: Vec<SidebarMenuItem>) -> Self {
        Self {
            title: title.map(|t| t.into()),
            items,
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
}

/// macOS 风格侧边栏组件
///
/// 封装了侧边栏的显隐状态、入场/退出动画、切换按钮和 TitleBar 补偿动画。
/// 内部包含多组菜单结构，支持选中高亮和点击回调。
#[derive(Clone)]
pub struct MacosSidebar {
    visible: bool,
    animating_out: bool,
    epoch: u32,
    selected_id: Option<String>,
    sections: Vec<SidebarMenuSection>,
    current_view: SidebarView,
    theme_scroll_handle: ScrollHandle,
    font_scroll_handle: ScrollHandle,
    host_scroll_handle: ScrollHandle,
    keychain_scroll_handle: ScrollHandle,
    font_view: Option<Entity<FontSettingsView>>,
    host_view: Option<Entity<HostView>>,
}

impl MacosSidebar {
    pub fn new() -> Self {
        Self {
            visible: false,
            animating_out: false,
            epoch: 0,
            selected_id: Some("homepage".to_string()),
            sections: vec![
                SidebarMenuSection::new(
                    Some("标签页组"),
                    vec![SidebarMenuItem::new(
                        "agent",
                        "agent",
                        Some(Icon::Workspace),
                    )],
                ),
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
            current_view: SidebarView::Menu,
            theme_scroll_handle: ScrollHandle::new(),
            font_scroll_handle: ScrollHandle::new(),
            host_scroll_handle: ScrollHandle::new(),
            keychain_scroll_handle: ScrollHandle::new(),
            font_view: None,
            host_view: None,
        }
    }

    /// 切换侧边栏显隐。
    ///
    /// 返回 `true` 表示侧边栏正在关闭，需要外部启动一个延迟任务
    /// 在动画结束后调用 [`Self::finish_animation`]；返回 `false` 表示正在打开。
    pub fn toggle(&mut self) -> bool {
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

    /// 当前是否可见（用于外部组件判断动画方向）。
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 当前动画 epoch（用于外部组件同步动画）。
    pub fn animation_epoch(&self) -> u32 {
        self.epoch
    }

    /// 渲染侧边栏本体（含入场/退出动画、菜单内容）。
    /// 完全收起时返回宽度为 0 的占位，保证始终参与 flex 布局。
    pub fn render_sidebar<F, G>(&self, cx: &gpui::App, on_select: F, on_double_click: G) -> gpui::AnyElement
    where
        F: Fn(&str, &mut Window, &mut gpui::App) + 'static,
        G: Fn(&crate::ssh::config::SshConnectionConfig, &mut Window, &mut gpui::App) + 'static,
    {
        if !self.visible && !self.animating_out {
            return div()
                .id(("macos-sidebar-placeholder", self.epoch))
                .h_full()
                .w(px(0.0))
                .into_any_element();
        }

        let target_width = SIDEBAR_WIDTH;
        let is_dark = cx.theme().appearance.is_dark();
        let theme = cx.theme().ui;
        let on_select = Rc::new(on_select);
        let _on_double_click = Rc::new(on_double_click);

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

        let inner = div()
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
            .child(match self.current_view {
                SidebarView::Menu => self.render_menu_content(cx, on_select.clone(), is_dark, theme).into_any_element(),
                SidebarView::Theme => {
                    theme_view::render_theme_sidebar(cx, on_select.clone(), is_dark, theme, &self.theme_scroll_handle).into_any_element()
                }
                SidebarView::Font => self.render_font_content(cx, on_select.clone(), is_dark, theme).into_any_element(),
                SidebarView::Host => {
                    self.host_view.clone().map(|v| v.into_any_element()).unwrap_or_else(|| div().into_any_element())
                }
                SidebarView::Keychain => {
                    keychain_view::render_keychain_sidebar(cx, on_select.clone(), is_dark, theme, &self.keychain_scroll_handle, self.selected_id.as_deref()).into_any_element()
                }
            });

        // 控制侧边栏整体轮廓
        let padded = div()
            .w(px(target_width))
            .pl(px(8.0))
            .pt(px(8.0))
            .pb(px(8.0))
            .h_full()
            .child(inner);

        div()
            .id((anim_id, self.epoch))
            .h_full()
            .overflow_hidden()
            .child(padded)
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

    fn render_menu_content<F>(
        &self,
        _cx: &gpui::App,
        on_select: Rc<F>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div
    where
        F: Fn(&str, &mut Window, &mut gpui::App) + 'static,
    {
        let mut menu_content = div().w_full().flex().flex_col();

        // 起始页（顶部高亮项）
        let is_homepage_selected = self.selected_id.as_deref() == Some("homepage");
        let homepage = div()
            .id("sidebar-item-homepage")
            .h(px(28.0))
            .w_full()
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(6.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .when(is_homepage_selected, |this| {
                this.bg(theme.accent).text_color(theme.accent_foreground)
            })
            .when(!is_homepage_selected, |this| {
                        this.hover(|style| style.bg(hover_bg(is_dark)))
                    .text_color(theme.text)
            })
            .child(
                gpui::svg()
                    .size(px(14.0))
                    .path(Icon::Screen.path())
                    .text_color(if is_homepage_selected {
                        theme.accent_foreground
                    } else {
                        theme.text_muted
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child("起始页"),
            )
            .on_click({
                let on_select = on_select.clone();
                move |_, window, cx| on_select("homepage", window, cx)
            });

        menu_content = menu_content.child(homepage).child(div().h(px(16.0)));

        // 分组菜单
        let mut item_index: usize = 0;
        for section in &self.sections {
            // 分组标题
            if let Some(ref title) = section.title {
                menu_content = menu_content.child(
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
                                .child(title.clone()),
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
                            .w(px(18.0))
                            .h(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(gpui::svg().size(px(14.0)).path(icon.path()).text_color(
                                if is_selected {
                                    theme.accent_foreground
                                } else {
                                    theme.text_muted
                                },
                            )),
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
                    .on_click({
                        let on_select = on_select.clone();
                        let id = item_id.clone();
                        move |_, window, cx| on_select(&id, window, cx)
                    });

                items_div = items_div.child(row);
            }

            menu_content = menu_content.child(items_div).child(div().h(px(12.0)));
        }

        menu_content
    }

    fn render_font_content<F>(
        &self,
        _cx: &gpui::App,
        on_select: Rc<F>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div
    where
        F: Fn(&str, &mut Window, &mut gpui::App) + 'static,
    {
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
                    .on_mouse_down(gpui::MouseButton::Left, {
                        let on_select = on_select.clone();
                        move |_, window, cx| on_select("__back", window, cx)
                    }),
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

    /// 渲染切换按钮（含位移动画）。
    ///
    /// 事件处理器通过 `on_toggle` 传入，调用者通常使用 `cx.listener` 构造。
    /// `is_fullscreen` 用于 macOS 全屏模式：此时没有交通灯按钮，无需额外左偏移。
    pub fn render_toggle_button<F>(
        &self,
        cx: &gpui::App,
        is_fullscreen: bool,
        on_toggle: F,
    ) -> impl IntoElement
    where
        F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    {
        let theme = cx.theme().ui;
        let button_left_closed = if cfg!(target_os = "macos") && !is_fullscreen {
            BUTTON_LEFT_CLOSED
        } else {
            // Windows/Linux 没有交通灯按钮；macOS 全屏时也没有
            8.0
        };
        // 侧边栏打开时，按钮位置所有平台一致
        let button_left_open = BUTTON_LEFT_OPEN;
        let (start_left, end_left, duration_ms) = if self.visible {
            (button_left_closed, button_left_open, ANIMATION_ENTER_MS)
        } else {
            (button_left_open, button_left_closed, ANIMATION_EXIT_MS)
        };

        div()
            .id(("sidebar-toggle-btn", self.epoch))
            .absolute()
            .top(px(if self.visible { BUTTON_TOP } else { 8.0 }))
            .left(px(start_left))
            .flex()
            .items_center()
            .justify_center()
            .w(px(36.))
            .h(px(if self.visible { 22. } else { 30. }))
            .rounded(px(999.0))
            .text_color(theme.text)
            .hover(|style| style.bg(theme.title_bar_hover))
            .cursor_pointer()
            .on_mouse_down(gpui::MouseButton::Left, on_toggle)
            .child(
                gpui::svg()
                    .size(px(20.0))
                    .text_color(theme.text)
                    .path(crate::assets::Icon::Sidebar.path()),
            )
            .with_animation(
                ("sidebar-toggle-anim", self.epoch),
                gpui::Animation::new(Duration::from_millis(duration_ms))
                    .with_easing(gpui::ease_in_out),
                move |this, delta| this.left(px(start_left + (end_left - start_left) * delta)),
            )
    }
}

impl Default for MacosSidebar {
    fn default() -> Self {
        Self::new()
    }
}
