use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{AnimationExt, ScrollHandle, StatefulInteractiveElement, Window, div, px};

use crate::assets::Icon;
use crate::theme::{ActiveTheme, ThemeState};

const SIDEBAR_WIDTH: f32 = 260.0;
const ANIMATION_ENTER_MS: u64 = 300;
const ANIMATION_EXIT_MS: u64 = 250;
const BUTTON_LEFT_OPEN: f32 = 228.0;
const BUTTON_LEFT_CLOSED: f32 = 84.0;
const BUTTON_TOP: f32 = 12.0;
const TITLE_BAR_EXTRA_PAD: f32 = 108.0;

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
                    Some("iCloud 标签页"),
                    vec![SidebarMenuItem::new(
                        "iphone",
                        "周松的 iPhone",
                        Some(Icon::Host),
                    )],
                ),
                SidebarMenuSection::new(
                    Some("偏好设置"),
                    vec![
                        SidebarMenuItem::new("themes", "主题", Some(Icon::Folder)),
                        SidebarMenuItem::new("font", "字体", Some(Icon::Eye)),
                    ],
                ),
            ],
            current_view: SidebarView::Menu,
            theme_scroll_handle: ScrollHandle::new(),
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

    /// 当前是否处于可见或正在退出的状态（需要渲染）。
    pub fn should_render(&self) -> bool {
        self.visible || self.animating_out
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

    /// 渲染侧边栏本体（含入场/退出动画、菜单内容）。
    pub fn render_sidebar<F>(&self, cx: &gpui::App, on_select: F) -> impl IntoElement
    where
        F: Fn(&str, &mut Window, &mut gpui::App) + 'static,
    {
        let target_width = SIDEBAR_WIDTH;
        let is_dark = cx.theme().appearance.is_dark();
        let theme = cx.theme().ui;
        let on_select = Rc::new(on_select);

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

        // 根据当前视图渲染不同内容
        let content = match self.current_view {
            SidebarView::Menu => self.render_menu_content(cx, on_select.clone(), is_dark, theme),
            SidebarView::Theme => self.render_theme_content(cx, on_select.clone(), is_dark, theme),
        };

        let inner = div()
            .w(px(target_width))
            .h_full()
            .rounded(px(12.0))
            .bg(Self::glass_bg(is_dark))
            .border(px(0.5))
            .border_color(Self::border_color(is_dark))
            .flex()
            .flex_col()
            .pt(px(48.0))
            .pb(px(16.0))
            .pl(px(12.0))
            .pr(px(12.0))
            .child(content);

        div()
            .id((anim_id, self.epoch))
            .h_full()
            .overflow_hidden()
            .child(inner)
            .with_animation(
                (anim_key, self.epoch),
                gpui::Animation::new(Duration::from_millis(duration_ms))
                    .with_easing(gpui::ease_in_out),
                move |this, delta| {
                    let width = width_fn(delta);
                    this.w(px(width.max(0.0)))
                },
            )
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
                this.hover(|style| style.bg(Self::hover_bg(is_dark)))
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
                        this.hover(|style| style.bg(Self::hover_bg(is_dark)))
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

    fn render_theme_content<F>(
        &self,
        cx: &gpui::App,
        on_select: Rc<F>,
        is_dark: bool,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div
    where
        F: Fn(&str, &mut Window, &mut gpui::App) + 'static,
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
                    .bg(Self::button_bg(is_dark))
                    .cursor_pointer()
                    .hover(|style| style.bg(Self::button_bg_hover(is_dark)))
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
                    .child("主题"),
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
            .bg(Self::hover_bg(is_dark))
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
                    .child("搜索主题"),
            );

        // 主题列表 —— 直接复制 settings_view.rs 的 render_theme_card 色块布局，等比缩小
        let mut theme_list = div().w_full().flex().flex_col().gap(px(2.0));

        for (index, theme_item) in themes.iter().enumerate() {
            let is_active = theme_item.id == active_theme_id;
            let theme_id = theme_item.id.clone();
            let preview_colors = theme_item.terminal.preview_colors();

            // 第一行：仅 accent 色块，比 ANSI 色块大 1.5 倍
            let accent_preview = div()
                .w(px(14.0))
                .h(px(14.0))
                .rounded(px(4.0))
                .bg(theme_item.ui.accent);

            // ANSI normal & bright
            let ansi_top = div().flex().gap(px(2.0)).children(
                preview_colors[..8].iter().copied().map(|color| {
                    div().w(px(9.0)).h(px(9.0)).rounded(px(2.0)).bg(color)
                }),
            );
            let ansi_bottom = div().flex().gap(px(2.0)).children(
                preview_colors[8..].iter().copied().map(|color| {
                    div().w(px(9.0)).h(px(9.0)).rounded(px(2.0)).bg(color)
                }),
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
                .when(is_active, |this| this.bg(theme.accent).text_color(theme.accent_foreground))
                .when(!is_active, |this| {
                    this.hover(|style| style.bg(Self::hover_bg(is_dark)))
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
                    .track_scroll(&self.theme_scroll_handle)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(theme_list),
            )
    }

    /// 渲染切换按钮（含位移动画）。
    ///
    /// 事件处理器通过 `on_toggle` 传入，调用者通常使用 `cx.listener` 构造。
    pub fn render_toggle_button<F>(&self, cx: &gpui::App, on_toggle: F) -> impl IntoElement
    where
        F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    {
        let theme = cx.theme().ui;
        let (start_left, end_left, duration_ms) = if self.visible {
            (BUTTON_LEFT_CLOSED, BUTTON_LEFT_OPEN, ANIMATION_ENTER_MS)
        } else {
            (BUTTON_LEFT_OPEN, BUTTON_LEFT_CLOSED, ANIMATION_EXIT_MS)
        };

        div()
            .id(("sidebar-toggle-btn", self.epoch))
            .absolute()
            .top(px(BUTTON_TOP))
            .left(px(start_left))
            .flex()
            .items_center()
            .justify_center()
            .w(px(36.))
            .h(px(22.))
            .rounded(px(12.0))
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

    /// 将 TitleBar 和 Workspace 包装成右侧主内容区域，并应用侧边栏开关时的
    /// padding 补偿动画。
    pub fn render_main_area(
        &self,
        title_bar: impl IntoElement,
        workspace: impl IntoElement,
        _cx: &gpui::App,
    ) -> impl IntoElement {
        let animated_title_bar = if cfg!(target_os = "macos") {
            let (start_pl, end_pl, dur_ms) = if self.visible {
                (TITLE_BAR_EXTRA_PAD, 0.0, ANIMATION_ENTER_MS)
            } else {
                (0.0, TITLE_BAR_EXTRA_PAD, ANIMATION_EXIT_MS)
            };
            div().child(title_bar).with_animation(
                ("titlebar-pad-anim", self.epoch),
                gpui::Animation::new(Duration::from_millis(dur_ms)).with_easing(gpui::ease_in_out),
                move |this, delta| this.pl(px(start_pl + (end_pl - start_pl) * delta)),
            )
        } else {
            div().child(title_bar).with_animation(
                ("titlebar-pad-anim", self.epoch),
                gpui::Animation::new(Duration::from_millis(1)).with_easing(gpui::ease_in_out),
                move |this, _| this,
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(animated_title_bar)
            .child(div().flex_1().min_h_0().overflow_hidden().child(workspace))
    }

    fn glass_bg(is_dark: bool) -> gpui::Hsla {
        if is_dark {
            gpui::Hsla::from(gpui::rgb(0x2C2C2E)).opacity(0.85)
        } else {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.85)
        }
    }

    fn border_color(is_dark: bool) -> gpui::Hsla {
        if is_dark {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.14)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.08)
        }
    }

    fn hover_bg(is_dark: bool) -> gpui::Hsla {
        if is_dark {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.08)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.06)
        }
    }

    fn button_bg(is_dark: bool) -> gpui::Hsla {
        if is_dark {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.10)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.06)
        }
    }

    fn button_bg_hover(is_dark: bool) -> gpui::Hsla {
        if is_dark {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.15)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.10)
        }
    }
}

impl Default for MacosSidebar {
    fn default() -> Self {
        Self::new()
    }
}
