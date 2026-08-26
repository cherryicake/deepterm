//! 标签页组：状态管理（创建 / 重命名 / 删除 / 折叠 / 会话恢复）与列表渲染。

use super::*;

#[derive(Clone)]
pub(super) enum CollapseEpochTarget {
    DefaultTabGroup,
    TabGroup(String),
    TabGroups,
    Section(usize),
}

impl MacosSidebar {
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

    pub fn tab_group_select_items(
        &self,
        default_label: impl Into<SharedString>,
    ) -> Vec<(String, SharedString)> {
        let mut items = vec![(DEFAULT_TAB_GROUP_ID.to_string(), default_label.into())];
        items.extend(
            self.tab_groups
                .iter()
                .map(|group| (group.id.clone(), group.label.clone().into())),
        );
        items
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
        cx.emit(SidebarEvent::TabGroupsChanged);
        cx.notify();
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
        cx.emit(SidebarEvent::TabGroupsChanged);
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
        cx.emit(SidebarEvent::TabGroupsChanged);
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

        cx.emit(SidebarEvent::TabGroupsChanged);
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

    pub(super) fn reset_collapse_epoch_after_animation(
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

    pub(super) fn render_tab_groups_section(
        &mut self,
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
        let tab_group_count = self.tab_groups.len();
        for index in 0..tab_group_count {
            let group_id = self.tab_groups[index].id.clone();
            let group_label = self.tab_groups[index].label.clone();
            let group_collapsed = self.tab_groups[index].collapsed;
            let group_epoch = self.tab_groups[index].collapse_epoch;
            let group_editing_input = self.tab_groups[index].editing_input.clone();
            let tabs = self
                .tab_group_tabs
                .get(&group_id)
                .cloned()
                .unwrap_or_default();
            groups = groups
                .child(self.render_tab_group_row(
                    cx,
                    is_dark,
                    theme,
                    group_id.clone(),
                    group_label,
                    group_collapsed,
                    group_editing_input,
                    true,
                    Icon::SquareOnSquare,
                ))
                .child(Self::render_collapsible_content(
                    self.render_tab_group_tabs(&group_id, &tabs, theme, cx),
                    group_collapsed,
                    group_epoch,
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

    pub(super) fn render_tab_group_row(
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
                this.handle_item_select(&click_group_id, cx);
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
                                .anchor(gpui::Anchor::TopLeft)
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

    pub(super) fn render_tab_group_tabs(
        &mut self,
        group_id: &str,
        tabs: &[SidebarTabItem],
        theme: crate::theme::UiTheme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut list = div().w_full().flex().flex_col().gap(px(2.0));

        let group_id_for_cb = group_id.to_string();
        for (index, tab) in tabs.iter().enumerate() {
            let icon = tab.icon.unwrap_or(Icon::Terminal);
            let text_color = if tab.active {
                theme.text
            } else {
                theme.text_muted
            };
            let on_tab_double_click = self.on_tab_double_click.clone();
            let on_tab_single_click = self.on_tab_single_click.clone();
            let tab_label = tab.label.clone();
            let tab_id = tab.tab_id.clone().unwrap_or_default();
            let click_group_id = group_id_for_cb.clone();
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
                    .cursor_pointer()
                    .hover(|style| style.bg(hover_bg(cx.theme().appearance.is_dark())))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            if event.click_count() == 2 {
                                if let Some(ref callback) = on_tab_double_click {
                                    callback(
                                        &click_group_id,
                                        &tab_id,
                                        &tab_label,
                                        this,
                                        window,
                                        cx,
                                    );
                                }
                            } else if event.click_count() == 1 {
                                if let Some(ref callback) = on_tab_single_click {
                                    callback(
                                        &click_group_id,
                                        &tab_id,
                                        &tab_label,
                                        this,
                                        window,
                                        cx,
                                    );
                                }
                            }
                        }),
                    )
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
}
