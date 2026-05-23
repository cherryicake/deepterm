use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, Render, ScrollHandle,
    SharedString, Window, div, px,
};

use crate::assets::Icon;
use crate::components::modal::Modal;
use crate::sidebar_view::new_host_form::NewHostForm;
use crate::sidebar_view::style::{button_bg, button_bg_hover, hover_bg};
use crate::ssh::config::SshConnectionConfig;
use crate::ssh_view::hosts_view::HostsView;
use crate::theme::ActiveTheme;

/// Host 侧边栏视图组件，管理 Host 列表和 New Host Modal。
pub struct HostView {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    selected_id: Option<SharedString>,
    host_form: Option<Entity<NewHostForm>>,
    show_modal: bool,
    on_select: Option<Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>>,
    on_connect: Option<Rc<dyn Fn(&SshConnectionConfig, &mut Window, &mut App) + 'static>>,
}

impl HostView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            Self {
                focus_handle: cx.focus_handle(),
                scroll_handle: ScrollHandle::new(),
                selected_id: None,
                host_form: None,
                show_modal: false,
                on_select: None,
                on_connect: None,
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
                    this.show_modal = false;
                    cx.notify();
                });
            });
            let entity_clone2 = entity.clone();
            form.on_saved(move |cx| {
                entity_clone2.update(cx, |this, cx| {
                    this.show_modal = false;
                    cx.notify();
                });
            });
        });
        self.show_modal = true;
        cx.notify();
    }

    /// 构造 Modal 弹窗元素。调用者（如 AppWindow）应在窗口根节点渲染它，
    /// 以确保 Modal 全屏居中，不被侧边栏等容器裁剪。
    pub fn build_modal(&mut self, cx: &mut Context<Self>) -> Option<Modal> {
        if !self.show_modal {
            return None;
        }
        let host_form = self.host_form.as_ref()?.clone();
        let entity = cx.entity();
        Some(
            Modal::new("host-modal")
                .width(px(480.))
                .max_height(px(700.))
                .on_close(move |_window, cx| {
                    entity.update(cx, |this, cx| {
                        this.show_modal = false;
                        cx.notify();
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
        let (hosts, _, configs) = HostsView::load_hosts_from_config();

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
                            .size(px(16.0))
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
        let search_box = div()
            .w_full()
            .h(px(32.0))
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(6.0))
            .rounded(px(8.0))
            .bg(hover_bg(is_dark))
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
                    .child("搜索主机"),
            );

        // Host 列表
        let mut host_list = div().w_full().flex().flex_col();

        for (index, host) in hosts.iter().enumerate() {
            let icon_color = host.icon_color.to_hsla(cx);
            let host_id = host.id.to_string();
            let is_selected = self.selected_id.as_ref().map(|s| s.as_ref()) == Some(host_id.as_str());
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
                .on_click({
                    let on_select = on_select.clone();
                    let on_connect = on_connect.clone();
                    let id = host_id.clone();
                    move |event, window, cx| {
                        if event.click_count() == 1 {
                            if let Some(ref f) = on_select {
                                f(&id, window, cx);
                            }
                        } else if event.click_count() == 2 {
                            if let Some(ref config) = host_config {
                                if let Some(ref f) = on_connect {
                                    f(config, window, cx);
                                }
                            }
                        }
                    }
                });

            host_list = host_list.child(row);
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
