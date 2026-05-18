use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{AnimationExt, Window, div, px};

use crate::assets::Icon;
use crate::theme::ActiveTheme;

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
                    Some("已保存"),
                    vec![
                        SidebarMenuItem::new("bookmarks", "书签", Some(Icon::Folder)),
                        SidebarMenuItem::new("reading-list", "阅读列表", Some(Icon::Eye)),
                        SidebarMenuItem::new("shared", "与你共享", Some(Icon::Bell)),
                    ],
                ),
            ],
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

    /// 渲染侧边栏本体（含入场/退出动画、菜单内容）。
    pub fn render_sidebar<F>(&self, cx: &gpui::App, on_select: F) -> impl IntoElement
    where
        F: Fn(&str, &mut Window, &mut gpui::App) + 'static,
    {
        let target_width = SIDEBAR_WIDTH;
        let is_dark = cx.theme().appearance.is_dark();
        let theme = cx.theme().ui;
        let on_select = Rc::new(on_select);

        let (anim_id, anim_key, duration_ms, width_fn): (
            &str,
            &str,
            u64,
            Box<dyn Fn(f32) -> f32>,
        ) = if self.visible {
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

        // ── 菜单内容 ──
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
                this.bg(theme.accent)
                    .text_color(theme.accent_foreground)
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
                        this.bg(theme.accent)
                            .text_color(theme.accent_foreground)
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
                            .child(
                                gpui::svg()
                                    .size(px(14.0))
                                    .path(icon.path())
                                    .text_color(if is_selected {
                                        theme.accent_foreground
                                    } else {
                                        theme.text_muted
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(
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
            .child(menu_content);

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
                move |this, delta| {
                    this.left(px(start_left + (end_left - start_left) * delta))
                },
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
                gpui::Animation::new(Duration::from_millis(dur_ms))
                    .with_easing(gpui::ease_in_out),
                move |this, delta| {
                    this.pl(px(start_pl + (end_pl - start_pl) * delta))
                },
            )
        } else {
            div().child(title_bar).with_animation(
                ("titlebar-pad-anim", self.epoch),
                gpui::Animation::new(Duration::from_millis(1))
                    .with_easing(gpui::ease_in_out),
                move |this, _| this,
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(animated_title_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(workspace),
            )
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
}

impl Default for MacosSidebar {
    fn default() -> Self {
        Self::new()
    }
}
