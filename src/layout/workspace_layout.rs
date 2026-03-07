use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Window, div, prelude::*, rgb,
};

use crate::components::sidebar::{Sidebar, SidebarMode};
use crate::components::tab_bar::TabBar;

/// 工作区布局状态
#[derive(Clone, Debug)]
pub struct WorkspaceLayoutState {
    pub sidebar_mode: SidebarMode,
    pub sidebar_width: f32,
    pub active_sidebar_item: Option<SharedString>,
    pub active_tab_id: Option<SharedString>,
}

impl Default for WorkspaceLayoutState {
    fn default() -> Self {
        Self {
            sidebar_mode: SidebarMode::Expanded,
            sidebar_width: 240.0,
            active_sidebar_item: Some("hosts".into()),
            active_tab_id: None,
        }
    }
}

/// 工作区布局组件 - 管理整体布局结构
pub struct WorkspaceLayout {
    sidebar: Entity<Sidebar>,
    tab_bar: Entity<TabBar>,
    state: WorkspaceLayoutState,
    content: Option<AnyElement>,
    focus_handle: FocusHandle,
}

impl WorkspaceLayout {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        // 创建侧边栏
        let sidebar = Sidebar::new(cx);

        // 创建标签栏
        let tab_bar = TabBar::new(cx);

        cx.new(|_cx| Self {
            sidebar,
            tab_bar,
            state: WorkspaceLayoutState::default(),
            content: None,
            focus_handle,
        })
    }

    pub fn set_content(&mut self, content: impl IntoElement, cx: &mut Context<Self>) {
        self.content = Some(content.into_any_element());
        cx.notify();
    }

    pub fn sidebar(&self) -> &Entity<Sidebar> {
        &self.sidebar
    }

    pub fn tab_bar(&self) -> &Entity<TabBar> {
        &self.tab_bar
    }

    pub fn state(&self) -> &WorkspaceLayoutState {
        &self.state
    }

    pub fn set_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        self.state.sidebar_mode = mode;
        cx.notify();
    }
}

impl Focusable for WorkspaceLayout {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceLayout {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("workspace-layout")
            .size_full()
            .flex()
            .flex_row()
            .bg(rgb(0x0f0f1a))
            .child(self.sidebar.clone())
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(self.tab_bar.clone())
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .children(self.content.take()),
                    ),
            )
    }
}
