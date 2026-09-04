//! 侧边栏内容渲染：菜单列表、各二级页面（主题/字体/主机/密钥串）、顶部 chrome 按钮。

use super::tab_groups::CollapseEpochTarget;
use super::*;

impl MacosSidebar {
    fn render_top_element<E>(
        &self,
        is_fullscreen: bool,
        offset_from_toggle: f32,
        id: &'static str,
        element: E,
        animate: bool,
    ) -> gpui::AnyElement
    where
        E: gpui::IntoElement,
    {
        let toggle_left_closed = if cfg!(target_os = "macos") && !is_fullscreen {
            BUTTON_LEFT_CLOSED
        } else {
            8.0
        };
        let toggle_left_open = self.width - 40.0;
        let button_left_closed = toggle_left_closed - offset_from_toggle;
        let button_left_open = toggle_left_open - offset_from_toggle;
        let end_left = if self.visible {
            button_left_open
        } else {
            button_left_closed
        };

        let container = div()
            .id((id, self.epoch))
            .absolute()
            .top(px(if self.visible { BUTTON_TOP } else { 8.0 }))
            .flex()
            .items_center()
            .justify_center()
            .child(element);

        if !animate || self.epoch == 0 {
            container.left(px(end_left)).into_any_element()
        } else {
            let (start_left, duration_ms) = if self.visible {
                (button_left_closed, ANIMATION_ENTER_MS)
            } else {
                (button_left_open, ANIMATION_EXIT_MS)
            };
            container
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

    fn render_top_icon_button<F>(
        &self,
        cx: &gpui::App,
        is_fullscreen: bool,
        offset_from_toggle: f32,
        id: &'static str,
        icon: Icon,
        icon_size: f32,
        on_mouse_down: F,
        animate: bool,
    ) -> gpui::AnyElement
    where
        F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    {
        let theme = cx.theme().ui;
        let button = div()
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
        self.render_top_element(is_fullscreen, offset_from_toggle, id, button, animate)
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
            false,
        )
    }

    pub fn render_tab_group_select<E>(
        &self,
        _cx: &gpui::App,
        is_fullscreen: bool,
        select_element: E,
    ) -> gpui::AnyElement
    where
        E: IntoElement,
    {
        if !self.visible {
            return div().into_any_element();
        }

        const LEFT_GAP: f32 = 4.0;
        const RIGHT_GAP: f32 = 4.0;
        let left_edge = if cfg!(target_os = "macos") && !is_fullscreen {
            BUTTON_LEFT_CLOSED + LEFT_GAP
        } else {
            8.0 + LEFT_GAP
        };
        let toggle_left_open = self.width - 40.0;
        let right_edge = toggle_left_open - RIGHT_GAP;
        let select_width = (right_edge - left_edge).max(36.0);
        let offset_from_toggle = toggle_left_open - left_edge;

        // 外层定位容器占据可用区域（不可见），内部选择器宽度随选中的
        // 标签组名自适应并右对齐，贴近侧边栏开关按钮；仅以可用区域
        // 作为宽度上限，避免随侧边栏宽度整体拉伸。
        let wrapper = div()
            .w(px(select_width))
            .h(px(24.))
            .flex()
            .justify_end()
            .child(select_element);
        self.render_top_element(
            is_fullscreen,
            offset_from_toggle,
            "tab-group-select",
            wrapper,
            false,
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
            true,
        )
    }

    pub(super) fn render_menu_content(
        &mut self,
        cx: &mut Context<Self>,
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
                theme,
                DEFAULT_TAB_GROUP_ID.to_string(),
                default_label,
                self.default_tab_group_collapsed,
                None,
                false,
                Icon::Macbook,
            ))
            .child(Self::render_collapsible_content(
                self.render_tab_group_tabs(DEFAULT_TAB_GROUP_ID, &default_tabs, theme, cx),
                self.default_tab_group_collapsed,
                self.default_tab_group_epoch,
                (default_tabs.len() as f32 * (SIDEBAR_ROW_HEIGHT + SIDEBAR_ROW_GAP)
                    - SIDEBAR_ROW_GAP)
                    .max(0.0),
                ("default-tab-group-tabs", self.default_tab_group_epoch),
            ))
            .child(div().h(px(16.0)))
            .child(self.render_tab_groups_section(cx, theme))
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
                        this.hover(|style| style.bg(hover_bg(theme)))
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
                        this.handle_item_select(&item_id, cx);
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

    pub(super) fn render_collapsible_content(
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

    pub(super) fn render_font_content(
        &self,
        cx: &mut Context<Self>,
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
                    .bg(button_bg(theme))
                    .cursor_pointer()
                    .hover(|style| style.bg(button_bg_hover(theme)))
                    .child(
                        gpui::svg()
                            .size(px(8.0))
                            .path(Icon::ChevronLeft.path())
                            .text_color(theme.text),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _window, cx| {
                            this.handle_item_select("__back", cx);
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

    pub(super) fn render_theme_content(
        &self,
        cx: &mut Context<Self>,
        theme: crate::theme::UiTheme,
    ) -> gpui::Div {
        let on_select = Rc::new({
            let sidebar = cx.entity().downgrade();
            move |id: &str, _window: &mut Window, cx: &mut App| {
                if let Some(s) = sidebar.upgrade() {
                    s.update(cx, |this, cx| {
                        this.handle_item_select(id, cx);
                        this.notify_app_window(cx);
                    });
                }
            }
        });
        let search_query = self.theme_search_query.clone();
        theme_view::render_theme_sidebar(
            cx,
            on_select,
            theme,
            &self.theme_scroll_handle,
            &search_query,
            {
                // 搜索输入直接写回 MacosSidebar 实体并 notify 过滤列表
                let sidebar = cx.entity().downgrade();
                Rc::new(move |text: &str, cx: &mut App| {
                    if let Some(s) = sidebar.upgrade() {
                        s.update(cx, |this, cx| {
                            this.theme_search_query = text.to_string();
                            cx.notify();
                        });
                    }
                })
            },
        )
    }

    pub(super) fn render_host_content(&self) -> gpui::Div {
        self.host_view
            .clone()
            .map(|view| div().w_full().h_full().child(view))
            .unwrap_or_else(|| div().w_full().h_full())
    }

    pub(super) fn render_keychain_content(&self) -> gpui::Div {
        self.keychain_view
            .clone()
            .map(|view| div().w_full().h_full().child(view))
            .unwrap_or_else(|| div().w_full().h_full())
    }
}
