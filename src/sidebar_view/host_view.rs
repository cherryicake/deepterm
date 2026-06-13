use std::collections::HashSet;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, Render,
    ScrollHandle, SharedString, Window, div, px,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::menu::{ContextMenuExt, PopupMenu};
use crate::components::modal::{Modal, ModalState};
use crate::sidebar_view::host_form::NewHostForm;
use crate::sidebar_view::hosts_view::{
    ConnectHost, CopyHostAddress, DeleteHost, EditHost, load_hosts_from_config,
};
use crate::sidebar_view::style::{button_bg, button_bg_hover, hover_bg};
use crate::ssh::config::{SshConnectionConfig, SshGlobalConfig};
use crate::theme::ActiveTheme;

/// Host 侧边栏视图组件，管理 Host 列表和 New Host Modal。
pub struct HostView {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    selected_ids: HashSet<SharedString>,
    host_form: Option<Entity<NewHostForm>>,
    modal_state: ModalState,
    on_select: Option<Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>>,
    on_connect: Option<Rc<dyn Fn(&SshConnectionConfig, &mut Window, &mut App) + 'static>>,
    on_edit_host: Option<Rc<dyn Fn(&SshConnectionConfig, &mut Window, &mut App) + 'static>>,
    context_menu_host_id: Option<SharedString>,
    search_input: Entity<InputState>,
    search_query: SharedString,
}

impl HostView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let scroll_handle = ScrollHandle::new();

            let search_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索主机")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });

            let subscription = cx.subscribe(
                &search_input,
                |this: &mut HostView, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        let text = this.search_input.read(cx).value().to_string();
                        this.search_query = text.to_lowercase().into();
                        cx.notify();
                    }
                },
            );
            std::mem::forget(subscription);

            Self {
                focus_handle,
                scroll_handle,
                selected_ids: HashSet::new(),
                host_form: None,
                modal_state: ModalState::new(),
                on_select: None,
                on_connect: None,
                on_edit_host: None,
                context_menu_host_id: None,
                search_input,
                search_query: SharedString::new_static(""),
            }
        })
    }

    /// 设置返回/导航回调。当用户点击返回按钮或选择菜单项时触发。
    pub fn on_select<F>(&mut self, callback: F)
    where
        F: Fn(&str, &mut Window, &mut App) + 'static,
    {
        self.on_select = Some(Rc::new(callback));
    }

    /// 设置连接主机回调。当用户双击主机时触发。
    pub fn on_connect<F>(&mut self, callback: F)
    where
        F: Fn(&SshConnectionConfig, &mut Window, &mut App) + 'static,
    {
        self.on_connect = Some(Rc::new(callback));
    }

    /// 设置编辑主机回调。当用户右键菜单选择 Edit 时触发。
    pub fn on_edit_host<F>(&mut self, callback: F)
    where
        F: Fn(&SshConnectionConfig, &mut Window, &mut App) + 'static,
    {
        self.on_edit_host = Some(Rc::new(callback));
    }

    fn open_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_form.is_none() {
            self.host_form = Some(NewHostForm::new(window, cx));
        }
        let form = self.host_form.as_ref().unwrap().clone();
        let entity = cx.entity().clone();
        form.update(cx, |form, _cx| {
            form.reset(_cx);
            let entity_clone = entity.clone();
            form.on_cancel(move |cx| {
                entity_clone.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
            let entity_clone2 = entity.clone();
            form.on_saved(move |cx| {
                entity_clone2.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
        });
        self.modal_state.open();
        cx.notify();
    }

    fn open_edit_modal(
        &mut self,
        config: &SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_form.is_none() {
            self.host_form = Some(NewHostForm::new(window, cx));
        }
        let form = self.host_form.as_ref().unwrap().clone();
        let entity = cx.entity().clone();
        form.update(cx, |form, cx| {
            form.edit_config(config, cx);
            let entity_clone = entity.clone();
            form.on_cancel(move |cx| {
                entity_clone.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
            let entity_clone2 = entity.clone();
            form.on_saved(move |cx| {
                entity_clone2.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
        });
        self.modal_state.open();
        cx.notify();
    }

    /// 构造 Modal 弹窗元素。调用者（如 AppWindow）应在窗口根节点渲染它，
    /// 以确保 Modal 全屏居中，不被侧边栏等容器裁剪。
    pub fn build_modal(&mut self, cx: &mut Context<Self>) -> Option<Modal> {
        if !self.modal_state.is_rendered() {
            return None;
        }
        let host_form = self.host_form.as_ref()?.clone();
        let entity = cx.entity();
        Some(
            Modal::new("host-modal")
                .width(px(540.))
                .max_height(px(700.))
                .top_padding(px(120.))
                .closing(self.modal_state.is_closing())
                .epoch(self.modal_state.epoch())
                .on_close(move |_window, cx| {
                    entity.update(cx, |this, cx| {
                        this.modal_state.close(cx, |this| &mut this.modal_state);
                    });
                })
                .child(host_form),
        )
    }
}

impl Focusable for HostView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HostView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();
        let (hosts, configs) = load_hosts_from_config();

        let on_select = self.on_select.clone();
        let on_connect = self.on_connect.clone();

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
                    .on_mouse_down(MouseButton::Left, {
                        let on_select = on_select.clone();
                        move |_, window, cx| {
                            if let Some(ref f) = on_select {
                                f("__back", window, cx);
                            }
                        }
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child("Host"),
            )
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
                            .size(px(12.0))
                            .path(Icon::Plus.path())
                            .text_color(theme.text),
                    )
                    .on_mouse_down(MouseButton::Left, {
                        cx.listener(|this, _, window, cx| {
                            this.open_modal(window, cx);
                        })
                    }),
            );

        // 搜索框
        let search_focused = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let search_box = div()
            .w_full()
            .h(px(32.0))
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(6.0))
            .rounded(px(12.0))
            .bg(hover_bg(is_dark))
            .when(search_focused, |this| {
                this.border(px(3.0)).border_color(theme.accent.opacity(0.5))
            })
            .mb(px(12.0))
            .child(
                gpui::svg()
                    .size(px(14.0))
                    .path(Icon::MagnifyingGlass.path())
                    .text_color(theme.text_muted),
            )
            .child(EditorInput::new(&self.search_input).transparent());

        // 根据搜索查询过滤 hosts（复用 hosts_view.rs 的过滤逻辑）
        let query = self.search_query.as_ref();
        let filtered_hosts: Vec<_> = if query.is_empty() {
            hosts.iter().collect()
        } else {
            hosts
                .iter()
                .filter(|host| {
                    host.name.to_lowercase().contains(query)
                        || host
                            .hostname
                            .as_ref()
                            .map_or(false, |h| h.to_lowercase().contains(query))
                        || host
                            .username
                            .as_ref()
                            .map_or(false, |u| u.to_lowercase().contains(query))
                        || host
                            .auth_label
                            .as_ref()
                            .map_or(false, |a| a.to_lowercase().contains(query))
                })
                .collect()
        };

        // Host 列表
        let mut host_list = div().w_full().flex().flex_col();

        for (index, host) in filtered_hosts.iter().enumerate() {
            let icon_color = host.icon_color.to_hsla(cx);
            let host_id = host.id.to_string();
            let is_selected = self.selected_ids.iter().any(|s| s.as_ref() == host_id);
            let host_config = configs.iter().find(|c| c.id() == host_id).cloned();

            let row = div()
                .id(("host-item", index))
                .w_full()
                .h(px(52.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(10.0))
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
                        .w(px(32.0))
                        .h(px(32.0))
                        .rounded(px(8.0))
                        .bg(if is_selected {
                            theme.accent_foreground
                        } else {
                            icon_color
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            gpui::svg()
                                .size(px(16.0))
                                .path(host.os_icon.unwrap_or(Icon::Linux).path())
                                .text_color(if is_selected {
                                    theme.accent
                                } else {
                                    theme.accent_foreground
                                }),
                        ),
                )
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
                                        .text_size(px(12.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(if is_selected {
                                            theme.accent_foreground
                                        } else {
                                            theme.text
                                        })
                                        .child(host.name.to_string()),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .text_size(px(10.0))
                                .line_height(px(14.0))
                                .text_color(if is_selected {
                                    theme.accent_foreground.opacity(0.7)
                                } else {
                                    theme.text_muted
                                })
                                .child(host.subtitle()),
                        ),
                )
                .on_click(cx.listener({
                    let on_select = on_select.clone();
                    let on_connect = on_connect.clone();
                    let host_config = host_config.clone();
                    let id = host_id.clone();
                    move |this, event: &gpui::ClickEvent, window, cx| {
                        let is_cmd_pressed = event.modifiers().secondary();
                        if event.click_count() == 1 {
                            if is_cmd_pressed {
                                let id_shared: SharedString = id.clone().into();
                                if this.selected_ids.contains(&id_shared) {
                                    this.selected_ids.remove(&id_shared);
                                } else {
                                    this.selected_ids.insert(id_shared);
                                }
                            } else {
                                this.selected_ids.clear();
                                this.selected_ids.insert(id.clone().into());
                            }
                            if let Some(ref f) = on_select {
                                f(&id, window, cx);
                            }
                            cx.notify();
                        } else if event.click_count() == 2 {
                            this.selected_ids.clear();
                            this.selected_ids.insert(id.clone().into());
                            cx.notify();
                            if let Some(ref config) = host_config {
                                if let Some(ref f) = on_connect {
                                    f(config, window, cx);
                                }
                            }
                        }
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener({
                        let host_id = host_id.clone();
                        move |this, _, _, cx| {
                            let id_shared: SharedString = host_id.clone().into();
                            this.context_menu_host_id = Some(id_shared.clone());
                            if !this.selected_ids.contains(&id_shared) {
                                this.selected_ids.clear();
                                this.selected_ids.insert(id_shared);
                            }
                            cx.notify();
                        }
                    }),
                )
                .context_menu({
                    let entity = cx.entity().clone();
                    use crate::components::button::Icon as MenuIcon;
                    move |menu: PopupMenu, _window, _cx| {
                        let count = entity.read(_cx).selected_ids.len();
                        let mut m = menu;
                        let conn_label = if count == 1 {
                            "连接".to_string()
                        } else {
                            format!("连接 {} 台主机", count)
                        };
                        m = m
                            .menu_with_icon(
                                &conn_label,
                                MenuIcon::new("icons/link.svg"),
                                Box::new(ConnectHost),
                            )
                            .separator();
                        if count == 1 {
                            m = m
                                .menu_with_icon(
                                    "编辑",
                                    MenuIcon::new("icons/edit.svg"),
                                    Box::new(EditHost),
                                )
                                .menu_with_icon(
                                    "拷贝地址",
                                    MenuIcon::new("icons/document.on.document.svg"),
                                    Box::new(CopyHostAddress),
                                )
                                .separator();
                        }
                        let label = if count == 1 {
                            "删除".to_string()
                        } else {
                            format!("删除 {} 台主机", count)
                        };
                        m.menu_with_icon(
                            &label,
                            MenuIcon::new("icons/trash.svg"),
                            Box::new(DeleteHost),
                        )
                    }
                });

            host_list = host_list.child(row);
        }

        let configs_rc = std::rc::Rc::new(configs.clone());
        let on_connect_action = self.on_connect.clone();

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .on_action(cx.listener({
                let configs_rc = configs_rc.clone();
                move |this, _: &ConnectHost, window, cx| {
                    let ids: Vec<_> = this.selected_ids.iter().cloned().collect();
                    for id in ids {
                        if let Some(config) = configs_rc.iter().find(|c| c.id() == id.as_ref()) {
                            if let Some(ref f) = on_connect_action {
                                f(config, window, cx);
                            }
                        }
                    }
                }
            }))
            .on_action(cx.listener({
                let configs_rc = configs_rc.clone();
                move |this, _: &EditHost, window, cx| {
                    let host_id = this
                        .context_menu_host_id
                        .clone()
                        .or_else(|| this.selected_ids.iter().next().cloned());
                    if let Some(id) = host_id {
                        if let Some(config) =
                            configs_rc.iter().find(|c| c.id() == id.as_ref()).cloned()
                        {
                            this.open_edit_modal(&config, window, cx);
                        }
                    }
                }
            }))
            .on_action(cx.listener({
                let configs_rc = configs_rc.clone();
                move |this, _: &CopyHostAddress, _window, cx| {
                    let host_id = this
                        .context_menu_host_id
                        .clone()
                        .or_else(|| this.selected_ids.iter().next().cloned());
                    if let Some(id) = host_id {
                        if let Some(config) = configs_rc.iter().find(|c| c.id() == id.as_ref()) {
                            cx.write_to_clipboard(ClipboardItem::new_string(config.host.clone()));
                        }
                    }
                }
            }))
            .on_action(cx.listener(move |this, _: &DeleteHost, window, cx| {
                let host_ids: Vec<_> = this.selected_ids.iter().cloned().collect();
                if host_ids.is_empty() {
                    return;
                }
                let count = host_ids.len();
                let names: Vec<_> = host_ids
                    .iter()
                    .filter_map(|id| {
                        hosts
                            .iter()
                            .find(|h| h.id.as_ref() == id.as_ref())
                            .map(|h| h.name.to_string())
                    })
                    .collect();

                let message = if count == 1 {
                    if let Some(name) = names.first() {
                        format!("确定要删除主机 '{}' 吗？此操作不可撤销。", name)
                    } else {
                        "确定要删除该主机吗？此操作不可撤销。".to_string()
                    }
                } else {
                    format!("确定要删除选中的 {} 个主机吗？此操作不可撤销。", count)
                };

                let weak = cx.weak_entity();
                let ids_to_delete = host_ids.clone();
                let answer = window.prompt(
                    gpui::PromptLevel::Warning,
                    "确认删除",
                    Some(&message),
                    &["删除", "取消"],
                    cx,
                );
                cx.spawn(async move |_, cx| {
                    if let Ok(0) = answer.await {
                        if let Some(entity) = weak.upgrade() {
                            let _ = entity.update(cx, |this, cx| {
                                if let Ok(mut global) = SshGlobalConfig::load_or_create() {
                                    for id in &ids_to_delete {
                                        global.remove_connection(id);
                                    }
                                    let _ = global.save();
                                }
                                this.selected_ids.clear();
                                this.context_menu_host_id = None;
                                cx.notify();
                            });
                        }
                    }
                })
                .detach();
            }))
            .child(header)
            .child(search_box)
            .child(
                div()
                    .id("host-scroll")
                    .track_scroll(&self.scroll_handle)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(host_list),
            )
    }
}
