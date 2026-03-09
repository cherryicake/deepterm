use std::collections::HashMap;

use gpui::prelude::*;
use gpui::{
    div, px, rgb, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled,
    Subscription, Window,
};

use crate::components::tab_bar::{DraggedTab, TabBar, TabItem};
use crate::terminal_view::terminal_view::TerminalView;
use crate::workspace::panel::{Panel, PanelContent, PanelEvent, PanelPlacement};
use crate::workspace::split_manager::{SplitContainer, SplitDirection};

/// Tab类型
#[derive(Clone, Debug, PartialEq)]
pub enum TabType {
    /// 普通tab - 对应单个panel
    Normal,
    /// Workspace tab - 对应一组panel
    Workspace,
}

/// Tab信息
pub struct TabInfo {
    pub id: String,
    pub label: String,
    pub tab_type: TabType,
    /// 该tab对应的容器（单个Panel或分割的Split）
    pub container: SplitContainer,
}

impl TabInfo {
    pub fn new_normal(
        id: impl Into<String>,
        label: impl Into<String>,
        panel: Entity<Panel>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            tab_type: TabType::Normal,
            container: SplitContainer::Panel(panel),
        }
    }

    pub fn new_workspace(
        id: impl Into<String>,
        children: Vec<SplitContainer>,
        direction: SplitDirection,
    ) -> Self {
        let sizes = vec![1.0; children.len()];
        Self {
            id: id.into(),
            label: "WorkSpace".to_string(),
            tab_type: TabType::Workspace,
            container: SplitContainer::Split {
                direction,
                children,
                sizes,
            },
        }
    }
}

/// 主工作区组件
pub struct Workspace {
    /// 全局TabBar
    pub tab_bar: Entity<TabBar>,
    /// 所有tab信息
    tabs: HashMap<String, TabInfo>,
    /// 当前活动的tab ID
    active_tab_id: String,
    /// 下一个tab序号
    next_tab_id: usize,
    /// 下一个panel序号
    next_panel_id: usize,
    /// Local Terminal创建计数（用于生成标签名，只增不减）
    local_terminal_count: usize,
    focus_handle: FocusHandle,
    /// Panel事件订阅
    panel_subscriptions: HashMap<String, Subscription>,
}

impl Workspace {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        // 创建初始tab - 只有一个终端Tab
        let initial_tab_items = vec![TabItem::new("tab-0", "Local Terminal")];
        let tab_bar = TabBar::new_with_tabs("tab-bar", cx, initial_tab_items);

        // 初始化tabs信息
        let mut tabs = HashMap::new();

        // tab-0 对应 panel-0 - 使用终端
        let panel_0 = cx.new(|cx| {
            Panel::with_terminal(
                "panel-0",
                "group-0",
                Some("tab-0".into()),
                Some("Local Terminal".into()),
                cx,
            )
        });
        tabs.insert(
            "tab-0".to_string(),
            TabInfo::new_normal("tab-0", "Local Terminal", panel_0),
        );

        cx.new(|cx| {
            let mut workspace = Self {
                tab_bar,
                tabs,
                active_tab_id: "tab-0".to_string(),
                next_tab_id: 1,
                next_panel_id: 1,
                local_terminal_count: 1,
                focus_handle,
                panel_subscriptions: HashMap::new(),
            };

            // 在创建时完成所有初始化
            workspace.subscribe_to_panels(cx);
            workspace.setup_tab_bar_handlers(cx);

            workspace
        })
    }

    /// 设置TabBar事件处理器
    fn setup_tab_bar_handlers(&mut self, cx: &mut Context<Self>) {
        let workspace = cx.entity();

        self.tab_bar.update(cx, |tab_bar, _cx| {
            // 设置tab点击事件
            tab_bar.on_tab_click({
                let workspace = workspace.clone();
                move |tab_id, cx| {
                    let id = tab_id.to_string();
                    cx.defer({
                        let workspace = workspace.clone();
                        move |cx| {
                            workspace.update(cx, |this, cx| {
                                this.active_tab_id = id.clone();
                                this.switch_to_tab(&id, cx);
                            });
                        }
                    });
                }
            });

            // 设置tab重新排序事件
            tab_bar.on_tab_reorder({
                let workspace = workspace.clone();
                move |dragged_tab, to_index, cx| {
                    let dragged_id = dragged_tab.tab_id.to_string();
                    cx.defer({
                        let workspace = workspace.clone();
                        move |cx| {
                            workspace.update(cx, |this, cx| {
                                this.handle_tab_reorder(&dragged_id, to_index, cx);
                            });
                        }
                    });
                }
            });

            // 设置新建tab事件
            tab_bar.on_new_tab({
                let workspace = workspace.clone();
                move |cx| {
                    cx.defer({
                        let workspace = workspace.clone();
                        move |cx| {
                            workspace.update(cx, |this, cx| {
                                this.handle_new_tab(cx);
                            });
                        }
                    });
                }
            });

            // 设置tab关闭事件
            tab_bar.on_tab_close({
                let workspace = workspace.clone();
                move |tab_id, cx| {
                    let id = tab_id.to_string();
                    cx.defer({
                        let workspace = workspace.clone();
                        move |cx| {
                            workspace.update(cx, |this, cx| {
                                this.handle_tab_close(&id, cx);
                            });
                        }
                    });
                }
            });
        });
    }

    /// 订阅所有面板的事件
    fn subscribe_to_panels(&mut self, cx: &mut Context<Self>) {
        // 收集所有panel
        let mut panels: Vec<Entity<Panel>> = Vec::new();
        for tab_info in self.tabs.values() {
            tab_info.container.collect_panels_vec_recursive(&mut panels);
        }

        for panel in panels {
            // 检查是否已经订阅过这个panel
            let panel_id = panel.read(cx).panel_id().to_string();
            let already_subscribed = self.panel_subscriptions.contains_key(&panel_id);

            if !already_subscribed {
                let subscription = cx.subscribe(&panel, |this, _panel, event: &PanelEvent, cx| {
                    this.handle_panel_event(event, cx);
                });
                self.panel_subscriptions.insert(panel_id, subscription);
            }
        }
    }

    /// 处理面板事件
    fn handle_panel_event(&mut self, event: &PanelEvent, cx: &mut Context<Self>) {
        match event {
            PanelEvent::DragPreview { .. } => {
                // 预览事件由Panel自己处理
            }
            PanelEvent::Drop {
                panel_id,
                dragged_tab,
                placement,
            } => {
                self.handle_tab_drop_on_panel(panel_id, dragged_tab, *placement, cx);
            }
            PanelEvent::TabChanged { .. } => {
                // Tab切换事件
            }
            PanelEvent::Close { panel_id } => {
                self.handle_panel_close(panel_id, cx);
            }
            PanelEvent::Selected { panel_id } => {
                self.handle_panel_selected(panel_id, cx);
            }
        }
    }

    /// 处理panel选中事件 - 确保同一组只有一个panel被选中
    fn handle_panel_selected(&mut self, selected_panel_id: &str, cx: &mut Context<Self>) {
        // 获取当前活动的tab信息
        let Some(tab_info) = self.tabs.get(&self.active_tab_id) else {
            return;
        };

        // 获取该tab下的所有panel
        let panels = tab_info.container.collect_panels_vec();

        // 取消其他panel的选中状态，设置当前panel为选中
        for panel in panels {
            panel.update(cx, |p, cx| {
                let should_be_selected = p.panel_id() == selected_panel_id;
                if p.is_selected() != should_be_selected {
                    p.set_selected(should_be_selected, cx);
                }
            });
        }

        cx.notify();
    }

    /// 处理关闭panel事件
    fn handle_panel_close(&mut self, panel_id: &str, cx: &mut Context<Self>) {
        let active_tab_id = self.active_tab_id.clone();

        // 获取当前活动的tab信息
        let Some(tab_info) = self.tabs.get_mut(&active_tab_id) else {
            return;
        };

        // 从container中移除指定的panel
        let new_container = tab_info.container.remove_panel(panel_id, cx);

        // 更新container
        tab_info.container = new_container;

        // 如果移除后只剩一个panel，更新tab类型为Normal
        let panel_count = tab_info.container.panel_count();
        if panel_count == 1 && tab_info.tab_type == TabType::Workspace {
            tab_info.tab_type = TabType::Normal;

            // 更新TabBar显示
            self.tab_bar.update(cx, |tab_bar, cx| {
                let tabs: Vec<TabItem> = tab_bar
                    .tabs
                    .iter()
                    .map(|t| {
                        if t.id == active_tab_id {
                            TabItem::new(&active_tab_id, &tab_info.label)
                        } else {
                            t.clone()
                        }
                    })
                    .collect();
                tab_bar.set_tabs(tabs, cx);
            });
        }

        // 重新订阅所有panel
        self.subscribe_to_panels(cx);

        cx.notify();
    }

    /// 处理tab拖拽到panel上的事件
    fn handle_tab_drop_on_panel(
        &mut self,
        target_panel_id: &str,
        dragged_tab: &DraggedTab,
        placement: PanelPlacement,
        cx: &mut Context<Self>,
    ) {
        let dragged_tab_id = dragged_tab.tab_id.to_string();

        // 获取目标tab（当前活动的tab）
        let target_tab_id = self.active_tab_id.clone();

        // 如果被拖拽的是同一个tab，不做处理
        if dragged_tab_id == target_tab_id {
            return;
        }

        // 获取被拖拽的tab信息
        let Some(dragged_tab_info) = self.tabs.remove(&dragged_tab_id) else {
            return;
        };

        // 获取目标tab信息
        let Some(target_tab_info) = self.tabs.get_mut(&target_tab_id) else {
            return;
        };

        // 获取被拖拽的container
        let dragged_container = dragged_tab_info.container;

        // 先获取被拖入的panels（在dragged_container被move之前）
        let dragged_panels = dragged_container.collect_panels_vec();

        // 在目标container中找到目标panel并进行分割
        let new_container =
            target_tab_info
                .container
                .split_at(target_panel_id, placement, dragged_container, cx);

        // 更新目标tab为Workspace类型
        target_tab_info.container = new_container;
        target_tab_info.tab_type = TabType::Workspace;

        // 更新TabBar显示
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.remove_tab(&dragged_tab_id, cx);
            // 更新目标tab的标签
            let tabs: Vec<TabItem> = tab_bar
                .tabs
                .iter()
                .map(|t| {
                    if t.id == target_tab_id {
                        TabItem::new(&target_tab_id, "WorkSpace")
                    } else {
                        t.clone()
                    }
                })
                .collect();
            tab_bar.set_tabs(tabs, cx);
        });

        // 重新订阅所有panel
        self.subscribe_to_panels(cx);

        // 设置新拖入的panel为选中状态，其他panel取消选中
        if !dragged_panels.is_empty() {
            let panel_ids: Vec<_> = dragged_panels
                .iter()
                .map(|p| p.read(cx).panel_id().to_string())
                .collect();
            let workspace = cx.entity();
            cx.defer(move |cx| {
                workspace.update(cx, |this, cx| {
                    for panel_id in panel_ids {
                        this.handle_panel_selected(&panel_id, cx);
                    }
                });
            });
        }

        cx.notify();
    }

    /// 处理tab重新排序
    fn handle_tab_reorder(
        &mut self,
        dragged_tab_id: &str,
        to_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.tab_bar.update(cx, |tab_bar, cx| {
            if let Some(from_index) = tab_bar.tabs.iter().position(|t| t.id == dragged_tab_id) {
                if from_index != to_index {
                    let tab = tab_bar.tabs.remove(from_index);
                    tab_bar.tabs.insert(to_index, tab);
                    cx.notify();
                }
            }
        });
    }

    /// 切换到指定tab
    fn switch_to_tab(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if self.tabs.get(tab_id).is_some() {
            // 更新TabBar的活动tab
            let tab_id = tab_id.to_string();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.set_active_tab(tab_id, cx);
            });
            cx.notify();
        }
    }

    /// 处理新建tab
    fn handle_new_tab(&mut self, cx: &mut Context<Self>) {
        let new_tab_id = format!("tab-{}", self.next_tab_id);
        let new_panel_id = format!("panel-{}", self.next_panel_id);
        self.next_tab_id += 1;
        self.next_panel_id += 1;

        // 生成Tab标签名：Local Terminal 或 Local Terminal[编号]
        let new_tab_label = self.generate_terminal_tab_name();

        // 创建新的panel（默认创建终端Panel）
        let new_panel = cx.new(|cx| {
            Panel::with_terminal(
                &new_panel_id,
                "group-0",
                Some(new_tab_id.clone().into()),
                Some(new_tab_label.clone().into()),
                cx,
            )
        });

        // 创建新的tab信息
        let new_tab_info = TabInfo::new_normal(&new_tab_id, &new_tab_label, new_panel);
        self.tabs.insert(new_tab_id.clone(), new_tab_info);

        // 添加到TabBar
        let new_tab_item = TabItem::new(&new_tab_id, &new_tab_label);
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(new_tab_item, cx);
        });

        // 订阅新panel的事件
        if let Some(panel) = self
            .tabs
            .get(&new_tab_id)
            .map(|t| t.container.get_panel_entity(cx))
            .flatten()
        {
            cx.subscribe(&panel, |this, _panel, event: &PanelEvent, cx| {
                this.handle_panel_event(event, cx);
            })
            .detach();
        }

        // 切换到新tab
        self.active_tab_id = new_tab_id.clone();
        self.switch_to_tab(&new_tab_id, cx);
    }

    /// 生成终端类型的Tab名称
    /// 第一个返回"Local Terminal"，之后返回"Local Terminal[编号]"
    /// 编号根据local_terminal_count累加，不管之前的Tab是否关闭
    fn generate_terminal_tab_name(&mut self) -> String {
        let base_name = "Local Terminal";

        // 累加计数器
        self.local_terminal_count += 1;

        // 第一个是Local Terminal，之后是Local Terminal[1], Local Terminal[2], ...
        if self.local_terminal_count == 1 {
            base_name.to_string()
        } else {
            format!("{}[{}]", base_name, self.local_terminal_count - 1)
        }
    }

    /// 处理关闭tab事件
    fn handle_tab_close(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        // 从tabs中移除
        self.tabs.remove(tab_id);

        // 从TabBar中移除
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.remove_tab(tab_id, cx);
        });

        // 如果关闭的是当前活动tab，切换到其他tab
        if self.active_tab_id == tab_id {
            // 尝试切换到第一个可用的tab
            if let Some(first_tab_id) = self.tabs.keys().next().cloned() {
                self.active_tab_id = first_tab_id.clone();
                self.switch_to_tab(&first_tab_id, cx);
            } else {
                // 没有tab了，退出应用程序
                cx.quit();
            }
        }

        cx.notify();
    }

    /// 递归渲染分割容器
    fn render_split_container(&self, container: &SplitContainer, cx: &Context<Self>) -> gpui::Div {
        match container {
            SplitContainer::Panel(panel) => {
                // 为每个panel添加padding和圆角容器
                div().size_full().child(panel.clone())
            }
            SplitContainer::Split {
                direction,
                children,
                sizes,
            } => {
                let is_horizontal = *direction == SplitDirection::Horizontal;

                div()
                    .size_full()
                    .flex()
                    .gap(px(4.0))
                    .when(is_horizontal, |this| this.flex_row())
                    .when(!is_horizontal, |this| this.flex_col())
                    .children(children.iter().enumerate().map(|(i, child)| {
                        let _size = sizes.get(i).copied().unwrap_or(1.0);
                        div().flex_1().child(self.render_split_container(child, cx))
                    }))
            }
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 获取当前活动tab对应的容器
        let active_container = self
            .tabs
            .get(&self.active_tab_id)
            .map(|tab_info| &tab_info.container);

        // 更新所有panel的is_solo状态
        if let Some(container) = active_container {
            let panel_count = container.panel_count();
            let panels = container.collect_panels_vec();
            for panel in panels {
                panel.update(cx, |p, cx| {
                    p.set_solo(panel_count == 1, cx);
                });
            }
        }

        div()
            .id("workspace")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x0f0f1a))
            // 内容区域 - 只渲染当前活动tab对应的panel(s)
            .child(
                div()
                    .flex_1()
                    .pl(px(4.0))
                    .pr(px(4.0))
                    .pb(px(4.0))
                    .when_some(active_container, |this, container| {
                        this.child(self.render_split_container(container, cx))
                    }),
            )
    }
}
