use gpui::{
    div, prelude::*, rgb, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    Styled, Window,
};
use std::collections::HashMap;

use crate::components::tab_bar::{DraggedTab, TabBar, TabItem};
use crate::workspace::panel::{Panel, PanelEvent, PanelPlacement};
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
    tab_bar: Entity<TabBar>,
    /// 所有tab信息
    tabs: HashMap<String, TabInfo>,
    /// 当前活动的tab ID
    active_tab_id: String,
    /// 下一个tab序号
    next_tab_id: usize,
    /// 下一个panel序号
    next_panel_id: usize,
    focus_handle: FocusHandle,
}

impl Workspace {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        // 创建初始tabs（都是普通tab）
        let initial_tab_items = vec![
            TabItem::new("tab-0", "admin@server1"),
            TabItem::new("tab-1", "root@db-server"),
            TabItem::new("tab-2", "dev@localhost"),
        ];
        let tab_bar = TabBar::new_with_tabs(cx, initial_tab_items);

        // 初始化tabs信息，每个tab有自己的panel
        let mut tabs = HashMap::new();

        // tab-0 对应 panel-0
        let panel_0 = cx.new(|cx| Panel::new("panel-0", "group-0", Some("tab-0".into()), cx));
        tabs.insert(
            "tab-0".to_string(),
            TabInfo::new_normal("tab-0", "admin@server1", panel_0),
        );

        // tab-1 对应 panel-1
        let panel_1 = cx.new(|cx| Panel::new("panel-1", "group-0", Some("tab-1".into()), cx));
        tabs.insert(
            "tab-1".to_string(),
            TabInfo::new_normal("tab-1", "root@db-server", panel_1),
        );

        // tab-2 对应 panel-2
        let panel_2 = cx.new(|cx| Panel::new("panel-2", "group-0", Some("tab-2".into()), cx));
        tabs.insert(
            "tab-2".to_string(),
            TabInfo::new_normal("tab-2", "dev@localhost", panel_2),
        );

        let workspace = cx.new(|_cx| Self {
            tab_bar,
            tabs,
            active_tab_id: "tab-0".to_string(),
            next_tab_id: 3,
            next_panel_id: 3,
            focus_handle,
        });

        // 订阅所有面板的事件
        workspace.update(cx, |this, cx| {
            this.subscribe_to_panels(cx);
            this.setup_tab_bar_handlers(cx);
        });

        workspace
    }

    /// 设置TabBar事件处理器
    fn setup_tab_bar_handlers(&mut self, cx: &mut Context<Self>) {
        let workspace_entity = cx.entity().clone();

        self.tab_bar.update(cx, |tab_bar, _cx| {
            // 设置tab点击事件 - 使用defer避免在update中update
            tab_bar.on_tab_click(move |tab_id, cx| {
                let id = tab_id.to_string();
                let ws_entity = workspace_entity.clone();
                cx.defer(move |cx| {
                    ws_entity.update(cx, |workspace, cx| {
                        workspace.active_tab_id = id.clone();
                        workspace.switch_to_tab(&id, cx);
                    });
                });
            });
        });

        let workspace_entity = cx.entity().clone();
        self.tab_bar.update(cx, |tab_bar, _cx| {
            // 设置tab重新排序事件
            tab_bar.on_tab_reorder(move |dragged_tab, to_index, cx| {
                let dragged_id = dragged_tab.tab_id.to_string();
                let ws_entity = workspace_entity.clone();
                cx.defer(move |cx| {
                    ws_entity.update(cx, |workspace, cx| {
                        workspace.handle_tab_reorder(&dragged_id, to_index, cx);
                    });
                });
            });
        });

        let workspace_entity = cx.entity().clone();
        self.tab_bar.update(cx, |tab_bar, _cx| {
            // 设置新建tab事件
            tab_bar.on_new_tab(move |cx| {
                let ws_entity = workspace_entity.clone();
                cx.defer(move |cx| {
                    ws_entity.update(cx, |workspace, cx| {
                        workspace.handle_new_tab(cx);
                    });
                });
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
            cx.subscribe(&panel, |this, _panel, event: &PanelEvent, cx| {
                this.handle_panel_event(event, cx);
            })
            .detach();
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
                println!(
                    "Panel {} drop: dragged_tab={}, placement={:?}",
                    panel_id, dragged_tab.tab_id, placement
                );
                self.handle_tab_drop_on_panel(panel_id, dragged_tab, *placement, cx);
            }
            PanelEvent::TabChanged { .. } => {
                // Tab切换事件
            }
        }
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
            println!("Same tab, skipping merge");
            return;
        }

        // 获取被拖拽的tab信息
        let Some(dragged_tab_info) = self.tabs.remove(&dragged_tab_id) else {
            println!("Dragged tab {} not found", dragged_tab_id);
            return;
        };

        // 获取目标tab信息
        let Some(target_tab_info) = self.tabs.get_mut(&target_tab_id) else {
            println!("Target tab {} not found", target_tab_id);
            return;
        };

        println!(
            "Merging tab {} into tab {} at panel {}",
            dragged_tab_id, target_tab_id, target_panel_id
        );

        // 获取被拖拽的container
        let dragged_container = dragged_tab_info.container;

        // 在目标container中找到目标panel并进行分割
        let new_container = Self::split_container_at_panel(
            &target_tab_info.container,
            target_panel_id,
            placement,
            dragged_container,
            cx,
        );

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

        cx.notify();
        println!("Tab merged into workspace: {}", target_tab_id);
    }

    /// 在container中找到指定panel并进行分割
    fn split_container_at_panel(
        container: &SplitContainer,
        target_panel_id: &str,
        placement: PanelPlacement,
        new_container: SplitContainer,
        cx: &App,
    ) -> SplitContainer {
        let direction = SplitDirection::from(placement);

        match container {
            SplitContainer::Panel(panel) => {
                // 检查当前panel是否是目标panel
                if panel.read(cx).panel_id() == target_panel_id {
                    // 是目标panel，创建一个分割
                    // 根据placement决定children的顺序
                    // Left/Top: 新panel在前（左边/上边），目标panel在后
                    // Right/Bottom: 目标panel在前，新panel在后（右边/下边）
                    let children = match placement {
                        PanelPlacement::Left | PanelPlacement::Top => {
                            // 新panel在左边/上边
                            vec![new_container, container.clone()]
                        }
                        PanelPlacement::Right | PanelPlacement::Bottom => {
                            // 新panel在右边/下边
                            vec![container.clone(), new_container]
                        }
                    };

                    SplitContainer::Split {
                        direction,
                        children,
                        sizes: vec![1.0, 1.0],
                    }
                } else {
                    // 不是目标panel，保持原样
                    container.clone()
                }
            }
            SplitContainer::Split {
                direction: dir,
                children,
                sizes,
            } => {
                // 在子容器中递归查找，只处理包含目标panel的那个
                let new_children: Vec<SplitContainer> = children
                    .iter()
                    .map(|child| {
                        // 检查这个子容器是否包含目标panel
                        if Self::container_contains_panel(child, target_panel_id, cx) {
                            Self::split_container_at_panel(
                                child,
                                target_panel_id,
                                placement,
                                new_container.clone(),
                                cx,
                            )
                        } else {
                            // 不包含目标panel，保持原样
                            child.clone()
                        }
                    })
                    .collect();

                SplitContainer::Split {
                    direction: *dir,
                    children: new_children,
                    sizes: sizes.clone(),
                }
            }
        }
    }

    /// 检查container是否包含指定的panel
    fn container_contains_panel(container: &SplitContainer, panel_id: &str, cx: &App) -> bool {
        match container {
            SplitContainer::Panel(panel) => panel.read(cx).panel_id() == panel_id,
            SplitContainer::Split { children, .. } => children
                .iter()
                .any(|child| Self::container_contains_panel(child, panel_id, cx)),
        }
    }

    /// 处理tab重新排序
    fn handle_tab_reorder(
        &mut self,
        dragged_tab_id: &str,
        to_index: usize,
        cx: &mut Context<Self>,
    ) {
        println!("Reordering tab: {} to index {}", dragged_tab_id, to_index);

        self.tab_bar.update(cx, |tab_bar, cx| {
            if let Some(from_index) = tab_bar.tabs.iter().position(|t| t.id == dragged_tab_id) {
                if from_index != to_index {
                    let tab = tab_bar.tabs.remove(from_index);
                    let insert_index = if to_index > from_index {
                        to_index
                    } else {
                        to_index
                    };
                    tab_bar.tabs.insert(insert_index, tab);
                    cx.notify();
                    println!(
                        "Tab reordered: {} from {} to {}",
                        dragged_tab_id, from_index, insert_index
                    );
                }
            }
        });
    }

    /// 切换到指定tab
    fn switch_to_tab(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let tab_id_owned = tab_id.to_string();
        if let Some(tab_info) = self.tabs.get(&tab_id_owned) {
            println!(
                "Switching to tab: {} (type: {:?})",
                tab_id_owned, tab_info.tab_type
            );
            // 更新TabBar的活动tab
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.set_active_tab(tab_id_owned.clone(), cx);
            });
            cx.notify();
        }
    }

    /// 处理新建tab
    fn handle_new_tab(&mut self, cx: &mut Context<Self>) {
        let new_tab_id = format!("tab-{}", self.next_tab_id);
        let new_tab_label = format!("new-tab@{}", self.next_tab_id);
        let new_panel_id = format!("panel-{}", self.next_panel_id);
        self.next_tab_id += 1;
        self.next_panel_id += 1;

        println!("Creating new tab: {} - {}", new_tab_id, new_tab_label);

        // 创建新的panel
        let new_panel = cx.new(|cx| {
            Panel::new(
                &new_panel_id,
                "group-0",
                Some(new_tab_id.clone().into()),
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

        println!("New tab created: {}", new_tab_id);
    }

    /// 递归渲染分割容器
    fn render_split_container(&self, container: &SplitContainer, cx: &Context<Self>) -> gpui::Div {
        match container {
            SplitContainer::Panel(panel) => div().size_full().child(panel.clone()),
            SplitContainer::Split {
                direction,
                children,
                sizes,
            } => {
                let is_horizontal = *direction == SplitDirection::Horizontal;

                div()
                    .size_full()
                    .flex()
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

        div()
            .id("workspace")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x0f0f1a))
            // 顶部全局TabBar
            .child(self.tab_bar.clone())
            // 内容区域 - 只渲染当前活动tab对应的panel(s)
            .child(
                div()
                    .flex_1()
                    .when_some(active_container, |this, container| {
                        this.child(self.render_split_container(container, cx))
                    }),
            )
    }
}
