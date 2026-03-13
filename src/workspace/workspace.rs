use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled, Subscription, Window,
    div, px, rgb,
};

/// 广播输入队列 - 全局共享，用于从TerminalView发送输入到Workspace
/// (workspace_id, panel_id, input_data)
fn get_broadcast_queue() -> &'static Mutex<Vec<(String, String, Vec<u8>)>> {
    static QUEUE: OnceLock<Mutex<Vec<(String, String, Vec<u8>)>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

use crate::components::tab_bar::{DraggedTab, TabBar, TabItem};
use crate::workspace::panel::{DraggedPanel, Panel, PanelContent, PanelEvent, PanelPlacement};
use crate::ssh::SshConnectionConfig;
use crate::workspace::split_manager::{SplitContainer, SplitDirection};
use crate::GlobalSshPicker;

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
    /// 广播状态 - 每个WorkSpace类型的tab有独立的广播状态
    pub broadcast_enabled: bool,
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
            broadcast_enabled: false,
        }
    }

    pub fn new_workspace(
        id: impl Into<String>,
        children: Vec<SplitContainer>,
        direction: SplitDirection,
        workspace_index: usize,
    ) -> Self {
        let sizes = vec![1.0; children.len()];
        let label = if workspace_index <= 1 {
            "WorkSpace".to_string()
        } else {
            format!("WorkSpace[{}]", workspace_index)
        };
        Self {
            id: id.into(),
            label,
            tab_type: TabType::Workspace,
            container: SplitContainer::Split {
                direction,
                children,
                sizes,
            },
            broadcast_enabled: false,
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
    /// Workspace唯一标识
    workspace_id: String,
}

impl Workspace {
    /// 获取当前活动tab的广播状态
    fn get_current_broadcast_state(&self) -> bool {
        self.tabs
            .get(&self.active_tab_id)
            .map(|tab| tab.broadcast_enabled)
            .unwrap_or(false)
    }

    /// 设置当前活动tab的广播状态
    fn set_current_broadcast_state(&mut self, enabled: bool) {
        if let Some(tab) = self.tabs.get_mut(&self.active_tab_id) {
            tab.broadcast_enabled = enabled;
        }
    }

    /// 生成WorkSpace标签名称
    /// 根据当前WorkSpace类型的tab索引生成下一个不重复的索引
    fn generate_workspace_label(&self) -> String {
        // 收集所有现有WorkSpace的索引
        let mut indices: Vec<usize> = self
            .tabs
            .values()
            .filter(|tab| tab.tab_type == TabType::Workspace)
            .filter_map(|tab| {
                // 解析标签中的索引，如 "WorkSpace[2]" -> 2, "WorkSpace" -> 1
                if tab.label == "WorkSpace" {
                    Some(1)
                } else if let Some(start) = tab.label.find('[') {
                    if let Some(end) = tab.label.find(']') {
                        tab.label[start + 1..end].parse().ok()
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        indices.sort_unstable();

        // 找到下一个可用的索引
        let next_index = if indices.is_empty() {
            1
        } else {
            indices.last().unwrap() + 1
        };

        // 生成标签
        if next_index == 1 {
            "WorkSpace".to_string()
        } else {
            format!("WorkSpace[{}]", next_index)
        }
    }

    pub fn new(cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        // 创建初始tab - 本地终端
        let initial_tab_items = vec![TabItem::new("tab-0", "Local Terminal")];
        let tab_bar = TabBar::new_with_tabs("tab-bar", cx, initial_tab_items);

        // 生成唯一的workspace ID（先创建，确保与Panel一致）
        let workspace_id = format!("workspace-{}", uuid::Uuid::new_v4());

        // 初始化tabs信息
        let mut tabs = HashMap::new();

        // tab-0 对应 panel-0 - 使用本地终端
        // 使用正确的 workspace_id 创建 Panel
        let panel_0 = cx.new(|cx| {
            Panel::with_terminal(
                "panel-0",
                workspace_id.clone(),
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
                local_terminal_count: 0,
                focus_handle,
                panel_subscriptions: HashMap::new(),
                workspace_id,
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

            // 设置新建tab事件 - 显示 SSH 选择器弹窗
            tab_bar.on_new_tab({
                move |window, cx| {
                    // 通过全局状态获取 ssh_picker 并显示
                    let ssh_picker = cx.global::<GlobalSshPicker>().0.clone();
                    ssh_picker.update(cx, |picker, cx| {
                        picker.toggle(window, cx);
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

            // 设置 Panel 拖拽到 TabBar 事件
            tab_bar.on_panel_drop({
                let workspace = workspace.clone();
                move |dragged_panel, insert_index, cx| {
                    cx.defer({
                        let workspace = workspace.clone();
                        move |cx| {
                            workspace.update(cx, |this, cx| {
                                this.handle_panel_drop_on_tab_bar(dragged_panel, insert_index, cx);
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

        // 获取当前存在的所有panel ID
        let current_panel_ids: std::collections::HashSet<String> = panels
            .iter()
            .map(|p| p.read(cx).panel_id().to_string())
            .collect();

        // 清理已不存在panel的订阅
        self.panel_subscriptions
            .retain(|panel_id, _| current_panel_ids.contains(panel_id));

        for panel in panels {
            // 检查是否已经订阅过这个panel
            let panel_id = panel.read(cx).panel_id().to_string();
            let already_subscribed = self.panel_subscriptions.contains_key(&panel_id);

            if !already_subscribed {
                log::info!("Subscribing to panel: {}", panel_id);
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
            PanelEvent::PanelDrop {
                target_panel_id,
                dragged_panel,
                placement,
            } => {
                self.handle_panel_drop_on_panel(target_panel_id, dragged_panel, *placement, cx);
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
            PanelEvent::BroadcastToggle { .. } => {
                self.handle_broadcast_toggle(cx);
            }
        }
    }

    /// 处理广播状态切换
    fn handle_broadcast_toggle(&mut self, cx: &mut Context<Self>) {
        // 切换当前tab的广播状态
        let new_state = !self.get_current_broadcast_state();
        self.set_current_broadcast_state(new_state);

        // 更新当前活动tab下所有panel的广播状态
        if let Some(tab_info) = self.tabs.get(&self.active_tab_id) {
            let panels = tab_info.container.collect_panels_vec();
            log::info!("Updating broadcast state for {} panels", panels.len());
            for panel in panels {
                let panel_id = panel.read(cx).panel_id().to_string();
                panel.update(cx, |p, cx| {
                    log::info!("Setting panel {} broadcast_enabled to {}", panel_id, new_state);
                    p.set_broadcast_enabled(new_state, cx);
                });
            }
        }

        cx.notify();
    }

    /// 处理终端输入广播 - 将输入发送给当前Workspace中的所有其他终端
    fn handle_broadcast_input(&mut self, source_panel_id: &str, data: &[u8], cx: &mut Context<Self>) {
        // 获取当前活动tab的广播状态
        if !self.get_current_broadcast_state() {
            return;
        }

        // 获取当前活动tab
        let Some(tab_info) = self.tabs.get(&self.active_tab_id) else {
            return;
        };

        // 获取所有panel，并筛选出需要接收广播的目标
        let panels = tab_info.container.collect_panels_vec();
        let target_panels: Vec<Entity<Panel>> = panels
            .into_iter()
            .filter(|panel| {
                let panel_ref = panel.read(cx);
                panel_ref.panel_id() != source_panel_id && panel_ref.has_terminal_content()
            })
            .collect();

        // 将输入发送给目标panel的终端
        for panel in target_panels {
            panel.update(cx, |panel_ref, _cx| {
                if let PanelContent::Terminal(terminal_view) = panel_ref.content() {
                    terminal_view.update(_cx, |tv, cx| {
                        tv.terminal().update(cx, |term, _| {
                            term.write(data);
                        });
                    });
                }
            });
        }
    }

    /// 获取当前广播状态
    pub fn is_broadcast_enabled(&self) -> bool {
        self.get_current_broadcast_state()
    }

    /// 处理广播输入队列中的数据
    /// 这个方法应该在适当的时机（如render时）调用
    pub fn process_broadcast_queue(&mut self, cx: &mut Context<Self>) {
        // 获取当前活动tab的广播状态
        if !self.get_current_broadcast_state() {
            return;
        }

        // 从队列中取出所有待处理的输入（只处理属于当前Workspace的输入）
        let inputs: Vec<(String, Vec<u8>)> = {
            let mut queue = get_broadcast_queue().lock().unwrap();
            if queue.is_empty() {
                return;
            }
            // 只过滤出属于当前workspace的输入
            let workspace_id = self.workspace_id.clone();
            queue
                .drain(..)
                .filter(|(ws_id, _, _)| ws_id == &workspace_id)
                .map(|(_, panel_id, data)| (panel_id, data))
                .collect()
        };

        // 处理每个输入
        for (panel_id, data) in inputs {
            self.handle_broadcast_input(&panel_id, &data, cx);
        }
    }

    /// 添加输入到广播队列（由TerminalView调用）
    pub fn queue_broadcast_input(workspace_id: &str, panel_id: &str, data: &[u8]) {
        let mut queue = get_broadcast_queue().lock().unwrap();
        queue.push((workspace_id.to_string(), panel_id.to_string(), data.to_vec()));
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
        let workspace_label = self.generate_workspace_label();
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.remove_tab(&dragged_tab_id, cx);
            // 更新目标tab的标签
            let tabs: Vec<TabItem> = tab_bar
                .tabs
                .iter()
                .map(|t| {
                    if t.id == target_tab_id {
                        TabItem::new(&target_tab_id, &workspace_label)
                    } else {
                        t.clone()
                    }
                })
                .collect();
            tab_bar.set_tabs(tabs, cx);
        });

        // 重新订阅所有panel
        self.subscribe_to_panels(cx);

        // 设置新加入的panels的广播状态和workspace_id（与当前Workspace tab保持一致）
        let broadcast_enabled = self.get_current_broadcast_state();
        let workspace_id = self.workspace_id.clone();
        for panel in &dragged_panels {
            panel.update(cx, |p, cx| {
                p.set_workspace_id(workspace_id.clone());
                p.set_broadcast_enabled(broadcast_enabled, cx);
            });
        }

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

    /// 处理Panel拖拽到另一个Panel上的事件
    /// 这与Tab拖拽合并的行为完全一致，只是数据源是同一WorkSpace内的Panel
    fn handle_panel_drop_on_panel(
        &mut self,
        target_panel_id: &str,
        dragged_panel: &DraggedPanel,
        placement: PanelPlacement,
        cx: &mut Context<Self>,
    ) {
        // 如果被拖拽的是同一个panel，不做处理
        if dragged_panel.panel_id == target_panel_id {
            return;
        }

        // 保存广播状态（当前tab的广播状态）
        let broadcast_enabled = self.get_current_broadcast_state();
        let active_tab_id = self.active_tab_id.clone();

        // 获取当前活动的tab信息
        let Some(tab_info) = self.tabs.get_mut(&active_tab_id) else {
            return;
        };

        // 从container中提取出被拖拽的panel（同时检查是否存在）
        let Some(dragged_panel_entity) =
            tab_info.container.find_panel(&dragged_panel.panel_id, cx)
        else {
            // 被拖拽的panel不在当前tab中，不处理
            // （未来可以扩展为支持跨Tab拖拽）
            return;
        };

        // 先从当前container中移除被拖拽的panel
        // 这样可以避免同一个panel在容器中出现两次
        let container_after_remove = tab_info.container.remove_panel(&dragged_panel.panel_id, cx);

        // 创建包含被拖拽panel的容器
        let dragged_container = SplitContainer::Panel(dragged_panel_entity);

        // 在目标panel处进行分割
        let new_container =
            container_after_remove.split_at(target_panel_id, placement, dragged_container, cx);

        // 更新container
        tab_info.container = new_container;

        // 如果当前不是WorkSpace类型，更新为WorkSpace类型
        if tab_info.tab_type != TabType::Workspace {
            tab_info.tab_type = TabType::Workspace;

            // 更新TabBar显示
            let workspace_label = self.generate_workspace_label();
            self.tab_bar.update(cx, |tab_bar, cx| {
                let tabs: Vec<TabItem> = tab_bar
                    .tabs
                    .iter()
                    .map(|t| {
                        if t.id == active_tab_id {
                            TabItem::new(&active_tab_id, &workspace_label)
                        } else {
                            t.clone()
                        }
                    })
                    .collect();
                tab_bar.set_tabs(tabs, cx);
            });
        }

        // 保存被拖拽panel的ID和广播状态，用于后续设置
        let dragged_panel_id = dragged_panel.panel_id.clone();

        // 重新订阅所有panel（这会改变self）
        self.subscribe_to_panels(cx);

        // 设置被拖拽的panel的广播状态和workspace_id（与当前Workspace保持一致）
        let workspace_id = self.workspace_id.clone();
        if let Some(tab_info) = self.tabs.get(&active_tab_id) {
            if let Some(panel) = tab_info.container.find_panel(&dragged_panel_id, cx) {
                panel.update(cx, |p, cx| {
                    p.set_workspace_id(workspace_id.clone());
                    p.set_broadcast_enabled(broadcast_enabled, cx);
                });
            }
        }

        // 设置被拖拽的panel为选中状态
        let dragged_panel_id = dragged_panel.panel_id.clone();
        let workspace = cx.entity();
        cx.defer(move |cx| {
            workspace.update(cx, |this, cx| {
                this.handle_panel_selected(&dragged_panel_id, cx);
            });
        });

        cx.notify();
    }

    /// 处理 Panel 拖拽到 TabBar 的事件
    /// 将 Panel 从当前 WorkSpace 中分离，创建一个新的独立 Tab
    fn handle_panel_drop_on_tab_bar(
        &mut self,
        dragged_panel: DraggedPanel,
        insert_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let active_tab_id = self.active_tab_id.clone();

        // 获取当前活动的tab信息
        let Some(tab_info) = self.tabs.get_mut(&active_tab_id) else {
            return;
        };

        // 从container中提取出被拖拽的panel（同时检查是否存在）
        let Some(dragged_panel_entity) =
            tab_info.container.find_panel(&dragged_panel.panel_id, cx)
        else {
            // 被拖拽的panel不在当前tab中，不处理跨Tab拖拽
            return;
        };

        // 获取panel的标签名用于新tab
        let panel_label = dragged_panel_entity
            .read(cx)
            .tab_label()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Panel".to_string());

        // 从当前container中移除被拖拽的panel
        let new_container = tab_info.container.remove_panel(&dragged_panel.panel_id, cx);

        // 更新原tab的container
        tab_info.container = new_container;

        // 检查原tab是否只剩一个panel，如果是则恢复为Normal类型
        let panel_count = tab_info.container.panel_count();
        if panel_count == 1 && tab_info.tab_type == TabType::Workspace {
            tab_info.tab_type = TabType::Normal;

            // 获取剩余panel的标签名
            if let Some(first_panel) = tab_info.container.get_first_panel() {
                let new_label = first_panel
                    .read(cx)
                    .tab_label()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| tab_info.label.clone());
                tab_info.label = new_label;
            }

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

        // 创建新的tab
        let new_tab_id = format!("tab-{}", self.next_tab_id);
        self.next_tab_id += 1;

        // 创建新的tab信息，使用被拖拽panel的标签名
        let new_tab_info = TabInfo::new_normal(
            &new_tab_id,
            &panel_label,
            dragged_panel_entity.clone(),
        );
        self.tabs.insert(new_tab_id.clone(), new_tab_info);

        // 添加到TabBar的指定位置
        let new_tab_item = TabItem::new(&new_tab_id, &panel_label);
        self.tab_bar.update(cx, |tab_bar, _cx| {
            match insert_index {
                Some(index) => {
                    // 插入到指定位置
                    if index <= tab_bar.tabs.len() {
                        tab_bar.tabs.insert(index, new_tab_item);
                    } else {
                        tab_bar.tabs.push(new_tab_item);
                    }
                }
                None => {
                    // 添加到末尾
                    tab_bar.tabs.push(new_tab_item);
                }
            }
            // 不在这里调用 cx.notify()，外部会统一通知
        });

        // 订阅新panel的事件
        // 只订阅这个新panel，不需要重新订阅所有panel
        let subscription =
            cx.subscribe(&dragged_panel_entity,
                |this, _panel, event: &PanelEvent, cx| {
                    this.handle_panel_event(event, cx);
                },
            );
        self.panel_subscriptions
            .insert(dragged_panel.panel_id.clone(), subscription);

        // 切换到新tab
        self.active_tab_id = new_tab_id.clone();
        self.switch_to_tab(&new_tab_id, cx);

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

    /// 创建本地终端Tab
    pub fn create_local_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.generate_terminal_tab_name();
        let panel = cx.new(|cx| {
            Panel::with_terminal(
                format!("panel-{}", self.next_panel_id),
                "group-0",
                Some(format!("tab-{}", self.next_tab_id).into()),
                Some(label.clone().into()),
                cx,
            )
        });
        self.add_terminal_tab(label, panel, window, cx);
    }

    /// 使用指定配置创建 SSH 终端
    pub fn create_ssh_terminal_with_config(
        &mut self,
        config: &SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = config.display_name();
        let panel = cx.new(|cx| {
            Panel::with_ssh_terminal(
                format!("panel-{}", self.next_panel_id),
                "group-0",
                Some(format!("tab-{}", self.next_tab_id).into()),
                Some(label.clone().into()),
                config,
                cx,
            )
        });
        self.add_terminal_tab(label, panel, window, cx);
    }

    /// 添加终端到Tab（通用方法）
    fn add_terminal_tab(
        &mut self,
        label: String,
        panel: Entity<Panel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_id = format!("tab-{}", self.next_tab_id);
        self.next_tab_id += 1;
        self.next_panel_id += 1;

        self.tabs.insert(
            tab_id.clone(),
            TabInfo::new_normal(&tab_id, &label, panel.clone()),
        );

        self.tab_bar
            .update(cx, |tab_bar, cx| tab_bar.add_tab(TabItem::new(&tab_id, &label), cx));

        let subscription = cx.subscribe(&panel, |this, _, event: &PanelEvent, cx| {
            this.handle_panel_event(event, cx);
        });
        self.panel_subscriptions
            .insert(panel.read(cx).panel_id().to_string(), subscription);

        self.active_tab_id = tab_id.clone();
        self.switch_to_tab(&tab_id, cx);

        cx.defer_in(window, move |_, window, cx| {
            if let Some(handle) = panel.read(cx).terminal_focus_handle(cx) {
                window.focus(&handle, cx);
            }
        });
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
        // 只在广播开启时处理队列
        if self.get_current_broadcast_state() {
            self.process_broadcast_queue(cx);
        }

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
