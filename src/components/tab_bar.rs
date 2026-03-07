use std::rc::Rc;

use gpui::{
    App, Context, ElementId, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Window, div, prelude::*, px, rgb,
};

/// 拖拽的 Tab 数据
#[derive(Clone, Debug)]
pub struct DraggedTab {
    pub tab_id: SharedString,
    pub from_index: usize,
}

/// Tab 放置事件
pub struct TabDropEvent {
    pub dragged_tab: DraggedTab,
    pub to_index: usize,
}

/// 边缘区域
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeZone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

/// 拖拽分割事件
pub struct TabSplitEvent {
    pub dragged_tab: DraggedTab,
    pub edge_zone: EdgeZone,
}

/// 拖拽预览视图
pub struct DragPreview {
    tab_label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(28.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .bg(rgb(0x3d3d5c))
            .shadow_md()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8888aa))
                    .child(self.tab_label.clone()),
            )
    }
}

/// 标签页项
#[derive(Clone)]
pub struct TabItem {
    pub id: SharedString,
    pub label: SharedString,
    pub is_active: bool,
    pub is_modified: bool,
}

impl TabItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            is_active: false,
            is_modified: false,
        }
    }

    pub fn with_modified(mut self, modified: bool) -> Self {
        self.is_modified = modified;
        self
    }
}

/// Tab合并事件
pub struct TabMergeEvent {
    pub dragged_tab: DraggedTab,
    pub target_tab_id: SharedString,
}

/// 标签栏组件
pub struct TabBar {
    id: ElementId,
    pub tabs: Vec<TabItem>,
    pub active_tab_id: Option<SharedString>,
    on_tab_click: Option<Rc<dyn Fn(SharedString, &mut App) + Send + Sync>>,
    on_tab_close: Option<Rc<dyn Fn(SharedString, &mut App) + Send + Sync>>,
    on_new_tab: Option<Rc<dyn Fn(&mut App) + Send + Sync>>,
    on_tab_reorder: Option<Rc<dyn Fn(DraggedTab, usize, &mut App) + Send + Sync>>,
    on_tab_split: Option<Rc<dyn Fn(DraggedTab, EdgeZone, &mut App) + Send + Sync>>,
    on_tab_merge: Option<Rc<dyn Fn(DraggedTab, SharedString, &mut App) + Send + Sync>>,
    focus_handle: FocusHandle,
}

impl TabBar {
    pub fn new(id: impl Into<ElementId>, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            id: id.into(),
            tabs: Vec::new(),
            active_tab_id: None,
            on_tab_click: None,
            on_tab_close: None,
            on_new_tab: None,
            on_tab_reorder: None,
            on_tab_split: None,
            on_tab_merge: None,
            focus_handle,
        })
    }

    pub fn new_with_tabs(
        id: impl Into<ElementId>,
        cx: &mut App,
        tabs: Vec<TabItem>,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        let active_tab_id = if tabs.is_empty() {
            None
        } else {
            Some(tabs[0].id.clone())
        };

        cx.new(|_cx| Self {
            id: id.into(),
            tabs,
            active_tab_id,
            on_tab_click: None,
            on_tab_close: None,
            on_new_tab: None,
            on_tab_reorder: None,
            on_tab_split: None,
            on_tab_merge: None,
            focus_handle,
        })
    }

    pub fn on_tab_click<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_click = Some(Rc::new(callback));
    }

    pub fn on_tab_close<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_close = Some(Rc::new(callback));
    }

    pub fn on_new_tab<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_new_tab = Some(Rc::new(callback));
    }

    pub fn on_tab_reorder<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, usize, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_reorder = Some(Rc::new(callback));
    }

    pub fn on_tab_split<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, EdgeZone, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_split = Some(Rc::new(callback));
    }

    pub fn on_tab_merge<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, SharedString, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_merge = Some(Rc::new(callback));
    }

    pub fn add_tab(&mut self, tab: TabItem, cx: &mut Context<Self>) {
        self.tabs.push(tab);
        cx.notify();
    }

    pub fn remove_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|t| t.id.as_ref() == id) {
            self.tabs.remove(index);

            // 更新活动标签
            if self.active_tab_id.as_ref().map(|s| s.as_ref()) == Some(id) {
                self.active_tab_id = if index < self.tabs.len() {
                    Some(self.tabs[index].id.clone())
                } else if !self.tabs.is_empty() {
                    Some(self.tabs[self.tabs.len() - 1].id.clone())
                } else {
                    None
                };
            }
        }
        cx.notify();
    }

    pub fn set_active_tab(&mut self, id: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.active_tab_id = Some(id.into());
        cx.notify();
    }

    pub fn set_tabs(&mut self, tabs: Vec<TabItem>, cx: &mut Context<Self>) {
        self.tabs = tabs;
        if self.active_tab_id.is_none() && !self.tabs.is_empty() {
            self.active_tab_id = Some(self.tabs[0].id.clone());
        }
        cx.notify();
    }

    pub fn active_tab_id(&self) -> Option<&SharedString> {
        self.active_tab_id.as_ref()
    }

    pub fn tabs_count(&self) -> usize {
        self.tabs.len()
    }
}

impl Focusable for TabBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TabBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let on_tab_click = self.on_tab_click.clone();
        let on_tab_close = self.on_tab_close.clone();
        let on_new_tab = self.on_new_tab.clone();
        let on_tab_reorder = self.on_tab_reorder.clone();
        let on_tab_merge = self.on_tab_merge.clone();

        println!(
            "TabBar render: on_tab_merge is {:?}",
            on_tab_merge.is_some()
        );

        div()
            .id(self.id.clone())
            .h(px(34.0))
            .flex()
            .items_center()
            .px(px(8.0))
            .gap(px(4.0))
            // 阻止 TabBar 区域的鼠标事件冒泡到 TitleBar，防止触发窗口拖拽
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(
                // 标签列表（包含新建标签按钮）
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .overflow_x_hidden()
                    .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                        let tab_id = tab.id.clone();
                        let is_active = self.active_tab_id.as_ref() == Some(&tab.id);
                        let label = tab.label.clone();
                        let is_modified = tab.is_modified;

                        div()
                            .id(tab_id.clone())
                            .h(px(28.0))
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .rounded(px(6.0))
                            .bg(if is_active {
                                rgb(0x2d2d44)
                            } else {
                                rgb(0x1e1e2e)
                            })
                            .hover(|style| {
                                if !is_active {
                                    style.bg(rgb(0x252538))
                                } else {
                                    style
                                }
                            })
                            .cursor_pointer()
                            // 拖拽功能
                            .on_drag(
                                DraggedTab {
                                    tab_id: tab_id.clone(),
                                    from_index: index,
                                },
                                {
                                    let label = label.clone();
                                    move |_drag_data, _, _, cx| {
                                        // 阻止事件冒泡，防止触发窗口拖拽
                                        cx.stop_propagation();
                                        // 返回拖拽时的视觉反馈 - 创建一个新的视图实体
                                        cx.new(|_cx| DragPreview {
                                            tab_label: label.clone(),
                                        })
                                    }
                                },
                            )
                            // 拖拽悬停效果
                            .drag_over::<DraggedTab>({
                                let tab_id = tab_id.clone();
                                move |tab, dragged: &DraggedTab, _, _cx| {
                                    if dragged.tab_id != tab_id {
                                        tab.border_color(rgb(0x6c5ce7)).border_l_2()
                                    } else {
                                        tab
                                    }
                                }
                            })
                            // 放置处理 - 只处理重新排序，不处理合并
                            .on_drop(cx.listener({
                                let on_tab_reorder = on_tab_reorder.clone();
                                let tab_id = tab_id.clone();
                                move |this, dragged: &DraggedTab, _window, cx| {
                                    if dragged.tab_id != tab_id {
                                        // TabBar上的拖拽只处理重新排序
                                        if let Some(ref callback) = on_tab_reorder {
                                            if let Some(to_index) =
                                                this.tabs.iter().position(|t| t.id == tab_id)
                                            {
                                                callback(dragged.clone(), to_index, cx);
                                            }
                                        }
                                    }
                                }
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if is_active {
                                        rgb(0xffffff)
                                    } else {
                                        rgb(0x8888aa)
                                    })
                                    .child(label),
                            )
                            .child(
                                div()
                                    .w(px(14.0))
                                    .h(px(14.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .hover(|style| style.bg(rgb(0x2d2d44)))
                                    .cursor_pointer()
                                    .text_color(rgb(0x666688))
                                    .text_xs()
                                    .child(if is_modified { "●" } else { "×" })
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener({
                                            let on_tab_close = on_tab_close.clone();
                                            let tab_id = tab_id.clone();
                                            move |_this, _, _, cx| {
                                                if let Some(ref callback) = on_tab_close {
                                                    callback(tab_id.clone(), cx);
                                                }
                                            }
                                        }),
                                    ),
                            )
                            .on_click(cx.listener({
                                let on_tab_click = on_tab_click.clone();
                                let tab_id = tab_id.clone();
                                move |_this, _event: &gpui::ClickEvent, _window, cx| {
                                    // 只有在没有发生拖拽的情况下才触发点击
                                    if let Some(ref callback) = on_tab_click {
                                        callback(tab_id.clone(), cx);
                                    }
                                }
                            }))
                            // 阻止事件冒泡到 TitleBar，防止在 macOS 上触发窗口拖拽
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                    }))
                    // 新建标签按钮 - 放在最后一个tab后面
                    .child(
                        div()
                            .w(px(28.0))
                            .h(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .bg(rgb(0x1e1e2e))
                            .hover(|style| style.bg(rgb(0x2d2d44)))
                            .child(div().text_color(rgb(0x666688)).child("+"))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |_, _, _, cx| {
                                    if let Some(ref callback) = on_new_tab {
                                        callback(cx);
                                    }
                                }),
                            ),
                    ),
            )
    }
}
