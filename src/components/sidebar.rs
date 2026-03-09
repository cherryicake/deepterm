use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    div, px, rgb, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Window,
};

/// 侧边栏项
#[derive(Clone)]
pub struct SidebarItem {
    pub id: SharedString,
    pub icon: SharedString,
    pub label: SharedString,
    pub is_active: bool,
}

impl SidebarItem {
    pub fn new(
        id: impl Into<SharedString>,
        icon: impl Into<SharedString>,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon: icon.into(),
            label: label.into(),
            is_active: false,
        }
    }
}

/// 侧边栏显示模式
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidebarMode {
    /// 展开模式 - 显示图标和文字
    Expanded,
    /// 折叠模式 - 只显示图标
    Collapsed,
}

/// 侧边栏组件
pub struct Sidebar {
    items: Vec<SidebarItem>,
    mode: SidebarMode,
    expanded_width: f32,
    collapsed_width: f32,
    on_item_click: Option<Arc<dyn Fn(SharedString, &mut App) + Send + Sync>>,
    on_toggle: Option<Arc<dyn Fn(SidebarMode, &mut App) + Send + Sync>>,
    focus_handle: FocusHandle,
}

impl Sidebar {
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::new_with_items(cx, Vec::new())
    }

    pub fn new_with_items(cx: &mut App, items: Vec<SidebarItem>) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            items,
            mode: SidebarMode::Expanded,
            expanded_width: 240.0,
            collapsed_width: 48.0,
            on_item_click: None,
            on_toggle: None,
            focus_handle,
        })
    }

    pub fn on_item_click<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut App) + Send + Sync + 'static,
    {
        self.on_item_click = Some(Arc::new(callback));
    }

    pub fn on_toggle<F>(&mut self, callback: F)
    where
        F: Fn(SidebarMode, &mut App) + Send + Sync + 'static,
    {
        self.on_toggle = Some(Arc::new(callback));
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            SidebarMode::Expanded => SidebarMode::Collapsed,
            SidebarMode::Collapsed => SidebarMode::Expanded,
        };

        cx.emit(SidebarToggleEvent { mode: self.mode });
        cx.notify();
    }

    pub fn set_active_item(&mut self, id: &str, cx: &mut Context<Self>) {
        for item in &mut self.items {
            item.is_active = item.id.as_ref() == id;
        }
        cx.notify();
    }

    pub fn mode(&self) -> SidebarMode {
        self.mode
    }

    fn current_width(&self) -> f32 {
        match self.mode {
            SidebarMode::Expanded => self.expanded_width,
            SidebarMode::Collapsed => self.collapsed_width,
        }
    }
}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// 侧边栏切换事件
#[derive(Clone, Copy, Debug)]
pub struct SidebarToggleEvent {
    pub mode: SidebarMode,
}

impl gpui::EventEmitter<SidebarToggleEvent> for Sidebar {}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = self.current_width();
        let is_expanded = self.mode == SidebarMode::Expanded;

        div()
            .id("sidebar")
            .h_full()
            .w(px(width))
            .bg(rgb(0x1a1a2e))
            .border_r_1()
            .border_color(rgb(0x2d2d44))
            .flex()
            .flex_col()
            .child(
                // 顶部切换按钮
                div()
                    .h(px(40.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_b_1()
                    .border_color(rgb(0x2d2d44))
                    .child(
                        div()
                            .cursor_pointer()
                            .p_2()
                            .rounded_md()
                            .hover(|style| style.bg(rgb(0x2d2d44)))
                            .child(if is_expanded { "◀" } else { "▶" })
                            .text_color(rgb(0x8888aa))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.toggle(cx);
                                }),
                            ),
                    ),
            )
            .child(
                // 侧边栏项列表
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .children(self.items.iter().map(|item| {
                        let item_id = item.id.clone();
                        let is_active = item.is_active;
                        let icon = item.icon.clone();
                        let label = item.label.clone();

                        div()
                            .id(item_id.clone())
                            .h(px(40.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .px(if is_expanded { px(12.0) } else { px(0.0) })
                            .when(!is_expanded, |this| this.justify_center())
                            .when(is_expanded, |this| this.justify_start())
                            .gap(if is_expanded { px(12.0) } else { px(0.0) })
                            .rounded_md()
                            .when(is_active, |this| this.bg(rgb(0x2d2d44)))
                            .when(!is_active, |this| {
                                this.hover(|style| style.bg(rgb(0x252538)))
                            })
                            .cursor_pointer()
                            .child(
                                div()
                                    .text_lg()
                                    .text_color(if is_active {
                                        rgb(0x6c5ce7)
                                    } else {
                                        rgb(0x8888aa)
                                    })
                                    .child(icon),
                            )
                            .when(is_expanded, |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(if is_active {
                                            rgb(0xffffff)
                                        } else {
                                            rgb(0x8888aa)
                                        })
                                        .child(label),
                                )
                            })
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.set_active_item(&item_id, cx);
                                    if let Some(ref callback) = this.on_item_click {
                                        callback(item_id.clone(), cx);
                                    }
                                }),
                            )
                    })),
            )
    }
}

/// 创建默认的 Termius 风格侧边栏项
pub fn create_default_sidebar_items() -> Vec<SidebarItem> {
    vec![
        SidebarItem::new("hosts", "🖥️", "Hosts"),
        SidebarItem::new("groups", "📁", "Groups"),
        SidebarItem::new("vaults", "🔐", "Vaults"),
        SidebarItem::new("sftp", "📂", "SFTP"),
        SidebarItem::new("snippets", "📝", "Snippets"),
        SidebarItem::new("keys", "🔑", "Keychain"),
        SidebarItem::new("port-forward", "🔄", "Port Forwarding"),
        SidebarItem::new("logs", "📋", "Logs"),
    ]
}
