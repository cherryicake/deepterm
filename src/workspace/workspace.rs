use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use parking_lot::Mutex;

use gpui::{
    AnyWindowHandle, App, Context, CursorStyle, Element, Entity, FocusHandle, Focusable,
    GlobalElementId, InteractiveElement, IntoElement, LayoutId, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Render, Style, Styled, Subscription, Window, div, px,
};
use gpui::{EventEmitter, KeyBinding, actions, prelude::*};

/// 广播输入队列 - 全局共享，用于从TerminalView发送输入到Workspace
/// (workspace_id, panel_id, input_data)
fn get_broadcast_queue() -> &'static Mutex<Vec<(String, String, Vec<u8>)>> {
    static QUEUE: OnceLock<Mutex<Vec<(String, String, Vec<u8>)>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

use crate::components::button::Icon as MenuIcon;
use crate::components::tab_bar::{CopyTabAddress, DraggedTab, DuplicateTab, TabBar, TabItem};
use crate::terminal_view::TerminalView;
use crate::file_manager::TransferStatusReporter;
use crate::file_manager::{
    ConflictPrompt, ConflictResolution, ConflictResolver, SharedConflictResolver,
    TRANSFER_CANCELLED_MSG, TransferManager, TransferRequest, TransferSummary, TransferTaskHandler,
    TransferTaskState, TransferTaskStatus, quick_total_size, transfer_item_name, transfer_label,
};
use crate::ssh::SshConnectionConfig;
use crate::ssh::platform::{RemoteOS, guess_os_from_text};
use crate::ssh_view::picker::SshPicker;
use crate::theme::ActiveTheme;
use crate::workspace::panel::{DraggedPanel, Panel, PanelContent, PanelEvent, PanelPlacement};
use crate::workspace::session_state::{TabSessionState, TabSessionType, TerminalSessionState, WorkspaceSessionState};
use crate::workspace::split_manager::{SplitContainer, SplitDirection};

// Define workspace actions
actions!(workspace, [NewLocalTerminal, CloseSelectedPanel]);

#[derive(Clone, Debug)]
pub enum WorkspaceEvent {
    SessionChanged,
    TabBarChanged,
    OpenPath { path: String, ssh_config: Option<SshConnectionConfig> },
}

impl EventEmitter<WorkspaceEvent> for Workspace {}
pub(crate) fn init(cx: &mut App) {
    // Bind keyboard shortcuts
    // 使用 None 作为 context，让这些快捷键全局有效
    cx.bind_keys([
        #[cfg(target_os = "windows")]
        KeyBinding::new("ctrl-l", NewLocalTerminal, None),
        #[cfg(not(target_os = "windows"))]
        KeyBinding::new("cmd-l", NewLocalTerminal, None),
        KeyBinding::new("cmd-w", CloseSelectedPanel, None),
    ]);
}

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
        // 第一个是"WorkSpace"，之后是"WorkSpace[1]", "WorkSpace[2]"...
        let label = if workspace_index == 0 {
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
    /// WorkSpace创建计数（用于生成标签名，只增不减）
    workspace_index: usize,
    focus_handle: FocusHandle,
    /// Panel事件订阅
    panel_subscriptions: HashMap<String, Subscription>,
    /// Workspace唯一标识
    workspace_id: String,
    /// Resize拖拽状态
    resize_drag: Option<ResizeDragState>,
    /// SSH picker for this window's workspace
    ssh_picker: Option<Entity<SshPicker>>,
    /// 下一个传输任务序号
    next_transfer_id: usize,
    /// 文件传输任务状态
    transfer_tasks: Vec<TransferTaskState>,
}

/// Resize拖拽状态
#[derive(Clone)]
struct ResizeDragState {
    handle_path: Vec<usize>,
    handle_index: usize,
    start_pos: Pixels,
    is_horizontal: bool,
    /// 拖拽开始时该层容器 sizes 的总和，用于正确换算像素→flex 比例
    sizes_sum: f32,
}

impl Workspace {
    pub fn active_tab_id(&self) -> &str {
        &self.active_tab_id
    }

    pub fn session_state(&self, cx: &App) -> WorkspaceSessionState {
        let tab_items = self.tab_bar.read(cx).tabs.clone();
        let tabs = tab_items
            .into_iter()
            .filter_map(|item| {
                let tab = self.tabs.get(item.id.as_ref())?;
                let container = tab.container.session_state(cx)?;
                let icon_os = tab
                    .container
                    .get_first_panel()
                    .and_then(|panel| panel.read(cx).remote_os)
                    .map(|os| os.to_id().to_string());
                Some(TabSessionState {
                    id: tab.id.clone(),
                    label: tab.label.clone(),
                    tab_type: match tab.tab_type {
                        TabType::Normal => TabSessionType::Normal,
                        TabType::Workspace => TabSessionType::Workspace,
                    },
                    broadcast_enabled: tab.broadcast_enabled,
                    icon_os,
                    container,
                })
            })
            .collect();

        WorkspaceSessionState {
            workspace_id: self.workspace_id.clone(),
            active_tab_id: self.active_tab_id.clone(),
            next_tab_id: self.next_tab_id,
            next_panel_id: self.next_panel_id,
            local_terminal_count: self.local_terminal_count,
            workspace_index: self.workspace_index,
            tabs,
        }
    }

    /// 计算指定 tab 的有效广播状态。
    /// 只有 WorkSpace 类型的 tab 才允许真正开启广播。
    fn get_tab_broadcast_state(&self, tab_id: &str) -> bool {
        self.tabs
            .get(tab_id)
            .map(|tab| tab.tab_type == TabType::Workspace && tab.broadcast_enabled)
            .unwrap_or(false)
    }

    /// 获取当前活动tab的广播状态
    fn get_current_broadcast_state(&self) -> bool {
        self.get_tab_broadcast_state(&self.active_tab_id)
    }

    /// 设置当前活动tab的广播状态
    fn set_current_broadcast_state(&mut self, enabled: bool) {
        if let Some(tab) = self.tabs.get_mut(&self.active_tab_id) {
            tab.broadcast_enabled = enabled;
        }
    }

    /// 将指定 tab 下所有 panel 的广播回调与 tab 状态同步。
    fn sync_tab_broadcast_to_panels(&self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(tab_info) = self.tabs.get(tab_id) else {
            return;
        };

        let broadcast_enabled = self.get_tab_broadcast_state(tab_id);
        for panel in tab_info.container.collect_panels_vec() {
            panel.update(cx, |p, cx| {
                p.set_broadcast_enabled(broadcast_enabled, cx);
            });
        }
    }

    /// 普通 tab 不支持广播；在退化为普通 tab 时需要清空其广播状态与回调。
    fn disable_tab_broadcast(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(tab_id) {
            tab.broadcast_enabled = false;
        }

        self.sync_tab_broadcast_to_panels(tab_id, cx);
    }

    /// 生成带序号的标签名称
    /// 第一个是"Base"，之后是"Base[1]", "Base[2]"...
    fn generate_indexed_name(base: &str, counter: &mut usize) -> String {
        let current = *counter;
        *counter += 1;
        if current == 0 {
            base.to_string()
        } else {
            format!("{}[{}]", base, current)
        }
    }

    /// 生成WorkSpace标签名称
    fn generate_workspace_label(&mut self) -> String {
        Self::generate_indexed_name("WorkSpace", &mut self.workspace_index)
    }

    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();

        // 创建初始tab - Local Terminal
        let initial_label = "Local Terminal";
        let local_os_icon = Some(crate::ssh::platform::detect_local_os().icon());
        let initial_tab_items = vec![TabItem::new("tab-0", initial_label).with_icon(local_os_icon)];
        let tab_bar = TabBar::new_with_tabs("tab-bar", cx, initial_tab_items);

        // 生成唯一的workspace ID（先创建，确保与Panel一致）
        let workspace_id = format!("workspace-{}", uuid::Uuid::new_v4());

        // 初始化tabs信息
        let tabs = HashMap::new();

        // 先创建 workspace，再使用 workspace entity 创建 panel
        let workspace = cx.new(|cx| {
            let mut workspace = Self {
                tab_bar,
                tabs,
                active_tab_id: "tab-0".to_string(),
                next_tab_id: 1,
                next_panel_id: 1,
                local_terminal_count: 1, // 初始已有一个本地终端
                workspace_index: 0,      // 从0开始计数
                focus_handle,
                panel_subscriptions: HashMap::new(),
                workspace_id: workspace_id.clone(),
                resize_drag: None,
                ssh_picker: None,
                next_transfer_id: 1,
                transfer_tasks: Vec::new(),
            };

            // 在创建时完成所有初始化
            workspace.subscribe_to_panels(window, cx);
            workspace.setup_tab_bar_handlers(cx);

            workspace
        });

        // 创建初始本地终端 panel
        let panel_0 = cx.new(|cx| {
            Panel::with_terminal(
                "panel-0",
                workspace_id,
                Some("tab-0".into()),
                Some(initial_label.into()),
                cx,
            )
        });

        workspace.update(cx, |workspace, _cx| {
            workspace.tabs.insert(
                "tab-0".to_string(),
                TabInfo::new_normal("tab-0", initial_label, panel_0),
            );
        });

        // 订阅初始 panel 的事件（之前的 subscribe_to_panels 调用时尚未有 panel）
        workspace.update(cx, |workspace, cx| {
            workspace.subscribe_to_panels(window, cx);
        });

        workspace
    }

    pub fn from_session_state(
        state: &WorkspaceSessionState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        if state.tabs.is_empty() {
            return None;
        }

        let focus_handle = cx.focus_handle();
        let workspace_id = if state.workspace_id.is_empty() {
            format!("workspace-{}", uuid::Uuid::new_v4())
        } else {
            state.workspace_id.clone()
        };

        let tab_items = state
            .tabs
            .iter()
            .map(|tab| {
                let icon = match tab.tab_type {
                    // Workspace tab 是一个布局容器，不代表某一台终端；恢复会话时必须保留
                    // Workspace 自己的图标，不能用第一个 Panel 的系统图标覆盖它。
                    TabSessionType::Workspace => Some(crate::assets::Icon::Workspace),
                    TabSessionType::Normal => tab
                        .icon_os
                        .as_deref()
                        .map(RemoteOS::from_id)
                        .filter(|os| *os != RemoteOS::Unknown)
                        .map(|os| os.icon()),
                };
                TabItem::new(&tab.id, &tab.label).with_icon(icon)
            })
            .collect();
        let tab_bar = TabBar::new_with_tabs("tab-bar", cx, tab_items);

        let active_tab_id = state
            .tabs
            .iter()
            .any(|tab| tab.id == state.active_tab_id)
            .then(|| state.active_tab_id.clone())
            .unwrap_or_else(|| state.tabs[0].id.clone());

        let tabs = state
            .tabs
            .iter()
            .map(|tab| {
                let container = SplitContainer::from_session_state(
                    &tab.container,
                    &workspace_id,
                    &tab.id,
                    &tab.label,
                    cx,
                );
                let tab_info = TabInfo {
                    id: tab.id.clone(),
                    label: tab.label.clone(),
                    tab_type: match tab.tab_type {
                        TabSessionType::Normal => TabType::Normal,
                        TabSessionType::Workspace => TabType::Workspace,
                    },
                    container,
                    broadcast_enabled: tab.broadcast_enabled,
                };
                (tab.id.clone(), tab_info)
            })
            .collect::<HashMap<_, _>>();

        let workspace = cx.new(|cx| {
            let mut workspace = Self {
                tab_bar,
                tabs,
                active_tab_id: active_tab_id.clone(),
                next_tab_id: state.next_tab_id.max(state.tabs.len()),
                next_panel_id: state.next_panel_id.max(1),
                local_terminal_count: state.local_terminal_count.max(1),
                workspace_index: state.workspace_index,
                focus_handle,
                panel_subscriptions: HashMap::new(),
                workspace_id,
                resize_drag: None,
                ssh_picker: None,
                next_transfer_id: 1,
                transfer_tasks: Vec::new(),
            };

            workspace.setup_tab_bar_handlers(cx);
            workspace.subscribe_to_panels(window, cx);
            workspace.switch_to_tab(&active_tab_id, cx);

            // 同步 TabItem 的 is_remote 标记
            let remote_status: Vec<(String, bool)> = workspace
                .tabs
                .iter()
                .map(|(tab_id, info)| {
                    let is_remote = info
                        .container
                        .get_first_panel()
                        .map(|p| p.read(cx).is_remote())
                        .unwrap_or(false);
                    (tab_id.clone(), is_remote)
                })
                .collect();
            workspace.tab_bar.update(cx, |tab_bar, _cx| {
                for tab in tab_bar.tabs.iter_mut() {
                    if let Some((_, is_remote)) =
                        remote_status.iter().find(|(id, _)| id == tab.id.as_ref())
                    {
                        tab.is_remote = *is_remote;
                    }
                }
            });

            workspace
        });

        Some(workspace)
    }

    /// 设置初始焦点到当前活动 tab 的第一个 panel 的 terminal
    /// 需要在 window 创建完成后调用
    pub fn set_initial_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_active_tab_panel(window, cx);
    }

    /// 设置该 workspace 使用的 SSH picker
    pub fn set_ssh_picker(&mut self, picker: Entity<SshPicker>, cx: &mut Context<Self>) {
        self.ssh_picker = Some(picker.clone());
        let workspace = cx.entity();
        self.tab_bar.update(cx, |tab_bar, _cx| {
            tab_bar.on_new_tab(move |window, cx| {
                workspace.update(cx, |workspace, cx| {
                    if let Some(ref picker) = workspace.ssh_picker {
                        let picker = picker.clone();
                        cx.defer_in(window, move |_, window, cx| {
                            picker.update(cx, |picker, cx| {
                                picker.show(window, cx);
                            });
                        });
                    }
                });
            });
        });
    }

    /// 创建新 Tab（本地终端）
    pub fn create_new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create_local_terminal(window, cx);
    }

    /// 将焦点设置到当前活动 tab 的第一个 panel
    pub fn focus_active_tab_panel(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab_info) = self.tabs.get(&self.active_tab_id) {
            if let Some(panel) = tab_info.container.get_first_panel() {
                if let Some(handle) = panel.read(cx).active_content_focus_handle(cx) {
                    window.focus(&handle, cx);
                }
            }
        }
    }

    /// tab 关闭后激活下一个 tab 并聚焦其第一个 panel。
    /// 若无剩余 tab 则退出应用。
    fn activate_next_tab(&mut self, cx: &mut Context<Self>) {
        // tab_bar.remove_tab 已按渲染顺序选好了下一个/上一个 tab
        let next_tab_id = self.tab_bar.read(cx).active_tab_id().map(|s| s.to_string());
        if let Some(next_tab_id) = next_tab_id {
            self.active_tab_id = next_tab_id.clone();
            self.switch_to_tab(&next_tab_id, cx);

            let focus_handle = self
                .tabs
                .get(&next_tab_id)
                .and_then(|t| t.container.get_first_panel())
                .and_then(|panel| panel.read(cx).active_content_focus_handle(cx));

            if let (Some(fh), Some(wh)) = (focus_handle, cx.active_window()) {
                let _ = cx.update_window(wh, |_, window, cx| {
                    window.focus(&fh, cx);
                });
            }
        } else {
            // 关闭当前窗口而不是退出整个应用
            if let Some(wh) = cx.active_window() {
                let _ = cx.update_window(wh, |_, window, _| {
                    window.remove_window();
                });
            }
        }
    }

    /// 设置TabBar事件处理器
    fn setup_tab_bar_handlers(&mut self, cx: &mut Context<Self>) {
        let workspace = cx.entity();

        self.tab_bar.update(cx, |tab_bar, _cx| {
            // 设置tab点击事件
            tab_bar.on_tab_click({
                let workspace = workspace.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    workspace.update(cx, |_this, cx| {
                        cx.defer_in(window, move |this, window, cx| {
                            this.active_tab_id = id.clone();
                            this.switch_to_tab(&id, cx);
                            // 将焦点设置到目标tab的第一个panel
                            this.focus_active_tab_panel(window, cx);
                        });
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

            // 设置tab关闭事件
            tab_bar.on_tab_close({
                let workspace = workspace.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    workspace.update(cx, |_this, cx| {
                        cx.defer_in(window, move |this, window, cx| {
                            this.handle_tab_close(&id, window, cx);
                        });
                    });
                }
            });

            // 设置 Panel 拖拽到 TabBar 事件
            tab_bar.on_panel_drop({
                let workspace = workspace.clone();
                move |dragged_panel, insert_index, window, cx| {
                    let workspace = workspace.clone();
                    window.defer(cx, move |window, cx| {
                        workspace.update(cx, |this, cx| {
                            this.handle_panel_drop_on_tab_bar(
                                dragged_panel,
                                insert_index,
                                window,
                                cx,
                            );
                        });
                    });
                }
            });

            // 设置溢出检测回调 - 当 TabBar 标签过多时触发
            tab_bar.on_overflow_changed({
                let workspace = workspace.clone();
                move |is_overflowing, cx| {
                    cx.defer({
                        let workspace = workspace.clone();
                        move |cx| {
                            workspace.update(cx, |this, cx| {
                                this.handle_tab_bar_overflow(is_overflowing, cx);
                            });
                        }
                    });
                }
            });

            // 拷贝地址 - 将终端连接地址写入剪贴板
            tab_bar.on_tab_copy_address({
                let workspace = workspace.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    workspace.update(cx, |_this, cx| {
                        cx.defer_in(window, move |this, window, cx| {
                            this.handle_tab_copy_address(&id, window, cx);
                        });
                    });
                }
            });

            // 复制标签页 - 创建新的终端标签页并初始化到相同的工作目录
            tab_bar.on_tab_duplicate({
                let workspace = workspace.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    workspace.update(cx, |_this, cx| {
                        cx.defer_in(window, move |this, window, cx| {
                            this.handle_tab_duplicate(&id, window, cx);
                        });
                    });
                }
            });

            // 右键菜单
            tab_bar.on_context_menu({
                let workspace = workspace.clone();
                move |tab_id, menu, _window, cx| {
                    let tabs = workspace.read(cx).tab_bar.read(cx).tabs.clone();
                    let is_remote = tabs
                        .iter()
                        .find(|t| t.id == tab_id)
                        .map(|t| t.is_remote)
                        .unwrap_or(false);

                    let m = if is_remote {
                        menu.menu_with_icon(
                            "拷贝地址",
                            MenuIcon::new("icons/document.on.document.svg"),
                            Box::new(CopyTabAddress),
                        )
                    } else {
                        menu
                    };
                    m.menu_with_icon(
                        "复制标签页",
                        MenuIcon::new("icons/plus.square.on.square.svg"),
                        Box::new(DuplicateTab),
                    )
                }
            });
        });
    }

    /// 订阅所有面板的事件
    fn subscribe_to_panels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 收集所有panel
        let mut panels: Vec<Entity<Panel>> = Vec::new();
        for tab_info in self.tabs.values() {
            tab_info.container.collect_panels_vec_recursive(&mut panels);
        }

        // 获取当前存在的所有panel ID
        let current_panel_ids: std::collections::HashSet<String> = panels
            .iter()
            .map(|p| p.read(cx).panel_id.to_string())
            .collect();

        // 清理已不存在panel的订阅
        self.panel_subscriptions
            .retain(|panel_id, _| current_panel_ids.contains(panel_id));

        for panel in panels {
            // 检查是否已经订阅过这个panel
            let panel_id = panel.read(cx).panel_id.to_string();
            let already_subscribed = self.panel_subscriptions.contains_key(&panel_id);

            if !already_subscribed {
                log::info!("Subscribing to panel: {}", panel_id);
                let subscription = cx.subscribe_in(
                    &panel,
                    window,
                    |this, _panel, event: &PanelEvent, window, cx| {
                        this.handle_panel_event(event, window, cx);
                    },
                );
                self.panel_subscriptions.insert(panel_id, subscription);
            }
        }
    }

    /// 处理面板事件
    fn handle_panel_event(
        &mut self,
        event: &PanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PanelEvent::DragPreview { .. } => {
                // 预览事件由Panel自己处理
            }
            PanelEvent::Drop {
                panel_id,
                dragged_tab,
                placement,
            } => {
                self.handle_tab_drop_on_panel(panel_id, dragged_tab, *placement, window, cx);
            }
            PanelEvent::PanelDrop {
                target_panel_id,
                dragged_panel,
                placement,
            } => {
                self.handle_panel_drop_on_panel(
                    target_panel_id,
                    dragged_panel,
                    *placement,
                    window,
                    cx,
                );
            }
            PanelEvent::TabChanged { .. } => {
                // Tab切换事件
            }
            PanelEvent::TabIconChanged {
                panel_id,
                os,
                ssh_config_id,
            } => {
                // 找到包含该panel的tab，更新tab图标
                if let Some(tab_id) = self.tabs.iter().find_map(|(tab_id, tab_info)| {
                    tab_info
                        .container
                        .contains_panel(panel_id, cx)
                        .then_some(tab_id.clone())
                }) {
                    if let Some(os) = os {
                        let icon = os.icon();
                        self.tab_bar.update(cx, |tab_bar, cx| {
                            tab_bar.modify_tab(&tab_id, |tab| {
                                tab.icon = Some(icon);
                            }, cx);
                        });
                        cx.emit(WorkspaceEvent::TabBarChanged);
                    }
                }
                // 保存检测到的OS到SSH配置
                if let (Some(config_id), Some(os)) = (ssh_config_id, os) {
                    let os_id = os.to_id();
                    let mut config = crate::ssh::config::SshGlobalConfig::load();
                    if let Some(conn) = config
                        .saved_connections
                        .iter_mut()
                        .find(|c| c.id() == *config_id)
                    {
                        if conn.os.as_deref() != Some(os_id) {
                            conn.os = Some(os_id.to_string());
                            if let Err(e) = config.save() {
                                log::warn!("保存 SSH 配置失败: {}", e);
                            }
                        }
                    }
                }
            }
            PanelEvent::OpenPath { panel_id, path, ssh_config } => {
                // 判断路径是文件还是目录
                cx.emit(WorkspaceEvent::OpenPath {
                    path: path.clone(),
                    ssh_config: ssh_config.clone(),
                });
                // 聚焦原 panel
                if let Some(panel) = self.find_panel(panel_id, cx) {
                    self.focus_panel_content(panel, window, cx);
                }
            }
            PanelEvent::TransferRequested { panel_id, request } => {
                let window_handle = window.window_handle();
                self.start_transfer_request(panel_id.clone(), request.clone(), window_handle, cx);
            }
            PanelEvent::Close { panel_id } => {
                self.handle_panel_close(panel_id, window, cx);
            }
            PanelEvent::Selected { panel_id } => {
                self.handle_panel_selected(panel_id, cx);
            }
            PanelEvent::BroadcastToggle { .. } => {
                self.handle_broadcast_toggle(cx);
            }
            PanelEvent::TabLabelChanged { panel_id, label } => {
                if let Some((tab_id, tab_type)) = self.tabs.iter().find_map(|(tab_id, tab_info)| {
                    tab_info
                        .container
                        .contains_panel(panel_id, cx)
                        .then_some((tab_id.clone(), tab_info.tab_type.clone()))
                }) {
                    // Workspace 类型的 tab 保持 "WorkSpace" 标签不变；
                    // Panel 标题栏的更新已在 Panel 内部通过 cx.notify() 完成。
                    if matches!(tab_type, TabType::Normal) {
                        if let Some(tab_info) = self.tabs.get_mut(&tab_id) {
                            tab_info.label = label.clone();
                        }
                        self.tab_bar.update(cx, |tab_bar, cx| {
                            tab_bar.modify_tab(&tab_id, |tab| {
                                tab.label = label.clone().into();
                            }, cx);
                        });
                        cx.emit(WorkspaceEvent::SessionChanged);
                        cx.emit(WorkspaceEvent::TabBarChanged);
                    }
                }
            }
            PanelEvent::SessionChanged => {
                cx.emit(WorkspaceEvent::SessionChanged);
            }
        }
    }

    fn start_transfer_request(
        &mut self,
        panel_id: String,
        request: TransferRequest,
        window_handle: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        log::info!(
            "Starting transfer from panel {}: {:?} -> {:?}",
            panel_id,
            request.sources,
            request.destination
        );

        if request.is_noop() {
            log::info!("Transfer is noop (source == destination), skipping");
            return;
        }

        let transfer_id = self.next_transfer_id;
        self.next_transfer_id += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = cancelled.clone();
        // 供冲突对话框监听任务感知"取消"，避免取消后仍弹出/继续弹出冲突对话框。
        let conflict_cancelled = cancelled.clone();
        let weak = cx.weak_entity();
        let bytes_transferred = Arc::new(AtomicU64::new(0));
        let progress = bytes_transferred.clone();
        let is_done = Arc::new(AtomicBool::new(false));
        let is_done_for_task = is_done.clone();

        let status_message = Arc::new(Mutex::new(String::new()));
        let total_bytes_for_reporter = Arc::new(AtomicU64::new(0));
        let reporter = TaskStatusReporter {
            status_message: status_message.clone(),
            total_bytes: total_bytes_for_reporter.clone(),
        };

        self.transfer_tasks.push(TransferTaskState {
            id: transfer_id,
            label: transfer_label(&request),
            item_name: transfer_item_name(&request),
            status: TransferTaskStatus::Running,
            cancelled,
            bytes_transferred: progress.clone(),
            total_bytes: 0,
            started_at: std::time::Instant::now(),
            status_message,
        });
        cx.notify();

        let request_for_size = request.clone();
        let weak_for_size = weak.clone();
        cx.spawn(async move |_, cx| {
            let total_bytes = cx
                .background_executor()
                .spawn(async move { quick_total_size(&request_for_size) })
                .await;
            if let Some(entity) = weak_for_size.upgrade() {
                let _ = entity.update(cx, |this, cx| {
                    if let Some(task) = this.transfer_tasks.iter_mut().find(|t| t.id == transfer_id)
                    {
                        let transferred = task.bytes_transferred.load(Ordering::Relaxed);
                        if transferred <= total_bytes || task.total_bytes == 0 {
                            task.total_bytes = total_bytes;
                        }
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        // 创建冲突解决通道和共享状态
        let (conflict_tx, conflict_rx) = async_channel::unbounded::<ConflictPrompt>();
        let shared_resolver = Arc::new(SharedConflictResolver::new(conflict_tx));
        shared_resolver.reset();
        let resolver_ref: Arc<dyn ConflictResolver> = shared_resolver.clone();

        // UI 任务：监听冲突请求并弹出对话框。
        // 持有传输的 `cancelled` 句柄，确保用户点取消后不再弹出（或继续弹出）冲突对话框 ——
        // 否则后台传输已停止，detached 监听任务却仍会把排队的 prompt 渲染成原生对话框，
        // 表现为"点取消后过段时间又冒出文件已存在弹窗"。
        //
        // 采用 100ms 轮询而非 `recv().await` 阻塞：监听任务自身持有 `shared_resolver`
        // （内含 channel 发送端），若一直阻塞在 `recv()`，则传输正常结束后发送端不会全部
        // 释放，channel 不关闭，任务永久泄漏。轮询让它能在「取消」或「传输完成」时及时退出。
        let conflict_done = is_done.clone();
        cx.spawn(async move |_, cx| {
            loop {
                // 已取消：立即退出，不再处理任何 prompt。
                if conflict_cancelled.load(Ordering::Relaxed) {
                    break;
                }

                let prompt = match conflict_rx.try_recv() {
                    Ok(prompt) => prompt,
                    Err(async_channel::TryRecvError::Empty) => {
                        // 没有待处理冲突：传输已结束则退出（释放发送端，避免任务泄漏），
                        // 否则稍后再轮询。
                        if conflict_done.load(Ordering::Relaxed) {
                            break;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(100))
                            .await;
                        continue;
                    }
                    Err(async_channel::TryRecvError::Closed) => break,
                };

                // 取出 prompt 后再确认一次：取消后绝不弹窗，直接回 Stop 让后台尽快收敛。
                if conflict_cancelled.load(Ordering::Relaxed) {
                    let _ = prompt.response.try_send(ConflictResolution::Stop);
                    break;
                }

                // 检查是否已启用 "应用到全部"
                if shared_resolver.has_cached_resolution() {
                    if let Some(resolution) = shared_resolver.get_cached_resolution() {
                        let _ = prompt.response.try_send(resolution);
                        continue;
                    }
                }

                let answer = cx.update_window(window_handle, |_view, window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        "文件已存在",
                        Some(&format!(
                            "名为 \"{}\" 的文件已存在于该位置。\n是否替换为正在移动的文件？",
                            prompt.file_name
                        )),
                        &["停止", "跳过", "替换", "保留两者"],
                        cx,
                    )
                });

                let button_index = match answer {
                    Ok(receiver) => match receiver.await {
                        Ok(idx) => idx,
                        Err(_) => break,
                    },
                    Err(_) => break,
                };

                // 对话框还开着的时候用户可能点了取消：此时把决策强制为 Stop，
                // 并跳过后续的 "应用到全部" 二次对话框。
                if conflict_cancelled.load(Ordering::Relaxed) {
                    let _ = prompt.response.try_send(ConflictResolution::Stop);
                    break;
                }

                let resolution = match button_index {
                    0 => ConflictResolution::Stop,
                    1 => ConflictResolution::Skip,
                    2 => ConflictResolution::Overwrite,
                    3 => ConflictResolution::AutoRename,
                    _ => ConflictResolution::Skip,
                };

                if resolution == ConflictResolution::Stop {
                    let _ = prompt.response.try_send(resolution);
                    break;
                }

                // 询问是否应用到全部
                let apply_all_answer = cx.update_window(window_handle, |_view, window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Info,
                        "应用到全部",
                        Some("是否将此选择应用到所有后续冲突文件？"),
                        &["仅此文件", "应用到全部"],
                        cx,
                    )
                });

                if let Ok(receiver) = apply_all_answer {
                    if let Ok(1) = receiver.await {
                        // 用户选择 "应用到全部"
                        shared_resolver.set_cached_resolution(resolution.clone());
                    }
                }

                let _ = prompt.response.try_send(resolution);
            }
        })
        .detach();

        let task = cx.background_executor().spawn(async move {
            let result = TransferManager::execute_blocking_with_cancel_and_progress_and_resolver_and_reporter(
                request,
                task_cancelled,
                Some(progress),
                Some(resolver_ref.as_ref()),
                Some(&reporter),
            );
            is_done_for_task.store(true, Ordering::Relaxed);
            result
        });

        let weak2 = weak.clone();
        cx.spawn(async move |_, cx| {
            let result = task.await;
            if let Some(entity) = weak2.upgrade() {
                let _ = entity.update(cx, |this, cx| {
                    this.finish_transfer_task(transfer_id, result, cx);
                });
            }
        })
        .detach();

        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                if is_done.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(entity) = weak.upgrade() {
                    let tb = total_bytes_for_reporter.load(Ordering::Relaxed);
                    let _ = entity.update(cx, |this, cx| {
                        if let Some(task) =
                            this.transfer_tasks.iter_mut().find(|t| t.id == transfer_id)
                        {
                            if tb > 0 && task.total_bytes == 0 {
                                task.total_bytes = tb;
                            }
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
    }

    fn finish_transfer_task(
        &mut self,
        transfer_id: usize,
        result: anyhow::Result<TransferSummary>,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self
            .transfer_tasks
            .iter_mut()
            .find(|task| task.id == transfer_id)
        else {
            return;
        };

        match result {
            Ok(summary) if summary.files_transferred == 0 && summary.bytes_transferred == 0 => {
                // 空操作（如拖拽到同一文件夹），标记为已跳过
                task.status = TransferTaskStatus::Skipped;
                cx.notify();
                return;
            }
            Ok(summary) => {
                log::info!(
                    "Transfer finished: {} files, {} bytes, {} skipped",
                    summary.files_transferred,
                    summary.bytes_transferred,
                    summary.skipped
                );
                // 完成时把 total 对齐到实际传输字节，保证进度条精确显示 100%
                // （quick_total_size 只是预估，可能偏大导致进度条停在 <100%）。
                task.total_bytes = summary.bytes_transferred;
                task.bytes_transferred
                    .store(summary.bytes_transferred, Ordering::Relaxed);
                task.status = TransferTaskStatus::Completed {
                    files: summary.files_transferred,
                    bytes: summary.bytes_transferred,
                };
                if summary.skipped > 0 {
                    log::warn!(
                        "传输完成但有 {} 个条目被跳过（冲突跳过 / 不支持的类型）",
                        summary.skipped
                    );
                }
            }
            Err(error) if error.to_string().contains(TRANSFER_CANCELLED_MSG) => {
                log::info!("Transfer cancelled");
                task.status = TransferTaskStatus::Cancelled;
            }
            Err(error) => {
                log::warn!("Transfer failed: {:#}", error);
                task.status = TransferTaskStatus::Failed(error.to_string());
            }
        }
        cx.notify();

        // 已完成/失败/取消的任务保留在列表中，仅通过用户点击"清除"或"×"手动移除。
        let _ = transfer_id;
    }

    /// 处理广播状态切换
    fn handle_broadcast_toggle(&mut self, cx: &mut Context<Self>) {
        let is_workspace_tab = self
            .tabs
            .get(&self.active_tab_id)
            .map(|tab| tab.tab_type == TabType::Workspace)
            .unwrap_or(false);
        if !is_workspace_tab {
            return;
        }

        // 切换当前tab的广播状态
        let new_state = !self.get_current_broadcast_state();
        self.set_current_broadcast_state(new_state);

        // 更新当前活动tab下所有panel的广播状态
        if let Some(tab_info) = self.tabs.get(&self.active_tab_id) {
            let panels = tab_info.container.collect_panels_vec();
            log::info!("Updating broadcast state for {} panels", panels.len());
            for panel in panels {
                let panel_id = panel.read(cx).panel_id.to_string();
                panel.update(cx, |p, cx| {
                    log::info!(
                        "Setting panel {} broadcast_enabled to {}",
                        panel_id,
                        new_state
                    );
                    p.set_broadcast_enabled(new_state, cx);
                });
            }
        }

        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    /// 处理终端输入广播 - 将输入发送给当前Workspace中的所有其他终端
    fn handle_broadcast_input(
        &mut self,
        source_panel_id: &str,
        data: &[u8],
        cx: &mut Context<Self>,
    ) {
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
                panel_ref.panel_id != source_panel_id && panel_ref.has_terminal_content()
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
            let mut queue = get_broadcast_queue().lock();
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
        let mut queue = get_broadcast_queue().lock();
        queue.push((
            workspace_id.to_string(),
            panel_id.to_string(),
            data.to_vec(),
        ));
    }

    /// 处理panel选中事件 - 确保同一组只有一个panel被选中
    fn handle_panel_selected(&mut self, selected_panel_id: &str, cx: &mut Context<Self>) {
        // 获取当前活动的tab信息
        let Some(tab_info) = self.tabs.get(&self.active_tab_id) else {
            return;
        };

        // 获取该tab下的所有panel
        let panels = tab_info.container.collect_panels_vec();

        let mut changed = false;

        // 取消其他panel的选中状态，设置当前panel为选中
        for panel in panels {
            panel.update(cx, |p, cx| {
                let should_be_selected = p.panel_id == selected_panel_id;
                if p.is_selected != should_be_selected {
                    p.set_selected(should_be_selected, cx);
                    changed = true;
                }
            });
        }

        if changed {
            // 通知文件树跟随切换到当前选中 panel 的终端目录
            cx.emit(WorkspaceEvent::SessionChanged);
        }

        cx.notify();
    }

    /// 关闭当前选中的 panel
    /// 如果当前 Tab 下只有一个 Panel，则关闭整个 Tab
    pub fn close_selected_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 获取当前活动tab
        let Some(tab_info) = self.tabs.get(&self.active_tab_id) else {
            return;
        };

        let panel_count = tab_info.container.panel_count();

        // 如果当前 Tab 下只有一个 Panel，关闭整个 Tab
        if panel_count <= 1 {
            let active_tab_id = self.active_tab_id.clone();
            self.handle_tab_close(&active_tab_id, window, cx);
            return;
        }

        // 找到当前选中的panel
        let panels = tab_info.container.collect_panels_vec();
        for panel in panels {
            if panel.read(cx).is_selected {
                let panel_id = panel.read(cx).panel_id.to_string();
                self.handle_panel_close(&panel_id, window, cx);
                return;
            }
        }

        // 如果没有选中的panel，但有多个panel存在，关闭第一个
        if let Some(first_panel) = tab_info.container.get_first_panel() {
            let panel_id = first_panel.read(cx).panel_id.to_string();
            self.handle_panel_close(&panel_id, window, cx);
        }
    }

    /// 处理关闭panel事件
    fn handle_panel_close(&mut self, panel_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 在所有 tab 中搜索包含该 panel 的 tab
        let Some(target_tab_id) = self
            .tabs
            .iter()
            .find(|(_, tab_info)| tab_info.container.contains_panel(panel_id, cx))
            .map(|(tab_id, _)| tab_id.clone())
        else {
            return;
        };

        // 获取移除前的 panel 数量
        let panel_count_before = self
            .tabs
            .get(&target_tab_id)
            .map(|t| t.container.panel_count())
            .unwrap_or(0);

        if panel_count_before <= 1 {
            // 这是 tab 中唯一的 panel，关闭整个 tab
            self.tabs.remove(&target_tab_id);
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.remove_tab(&target_tab_id, cx);
            });

            if self.active_tab_id == target_tab_id {
                self.activate_next_tab(cx);
            }
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.emit(WorkspaceEvent::TabBarChanged);
            cx.notify();
            return;
        }

        // 从container中移除指定的panel
        let new_container = {
            let Some(tab_info) = self.tabs.get_mut(&target_tab_id) else {
                return;
            };
            tab_info.container.remove_panel(panel_id, cx)
        };

        let Some(new_container) = new_container else {
            self.tabs.remove(&target_tab_id);
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.remove_tab(&target_tab_id, cx);
            });
            if self.active_tab_id == target_tab_id {
                self.activate_next_tab(cx);
            }
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.emit(WorkspaceEvent::TabBarChanged);
            cx.notify();
            return;
        };

        let panel_count = new_container.panel_count();
        // 更新container
        let Some(tab_info) = self.tabs.get_mut(&target_tab_id) else {
            return;
        };
        tab_info.container = new_container;

        // 如果移除后只剩一个panel，更新tab类型为Normal
        if panel_count == 1 && tab_info.tab_type == TabType::Workspace {
            tab_info.tab_type = TabType::Normal;

            // 保留剩余panel的系统图标
            let remaining_icon = tab_info
                .container
                .get_first_panel()
                .map(|p| p.read(cx).remote_os)
                .flatten()
                .map(|os| os.icon());

            // 更新TabBar显示
            let label = tab_info
                .container
                .get_first_panel()
                .and_then(|p| p.read(cx).tab_label().map(|s| s.to_string()))
                .unwrap_or_else(|| tab_info.label.clone());
            tab_info.label = label.clone();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.modify_tab(&target_tab_id, |tab| {
                    tab.label = label.clone().into();
                    tab.icon = remaining_icon;
                }, cx);
            });
        }

        // 重新订阅所有panel
        self.subscribe_to_panels(window, cx);

        if panel_count == 1 {
            self.disable_tab_broadcast(&target_tab_id, cx);
        }

        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
        cx.notify();
    }

    /// 处理tab拖拽到panel上的事件
    fn handle_tab_drop_on_panel(
        &mut self,
        target_panel_id: &str,
        dragged_tab: &DraggedTab,
        placement: PanelPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dragged_tab_id = dragged_tab.tab_id.to_string();

        // 获取目标tab（当前活动的tab）
        let target_tab_id = self.active_tab_id.clone();

        // 如果被拖拽的是同一个tab，不做处理
        if dragged_tab_id == target_tab_id {
            return;
        }

        // 预生成workspace标签，避免在借用self.tabs后再次借用self
        let workspace_label = self.generate_workspace_label();

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
        target_tab_info.label.clone_from(&workspace_label);

        // 更新TabBar显示（Workspace使用统一图标）
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.remove_tab(&dragged_tab_id, cx);
            tab_bar.modify_tab(&target_tab_id, |tab| {
                tab.label = workspace_label.clone().into();
                tab.icon = Some(crate::assets::Icon::Workspace);
            }, cx);
        });

        // 重新订阅所有panel
        self.subscribe_to_panels(window, cx);

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
                .map(|p| p.read(cx).panel_id.to_string())
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
        window: &mut Window,
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
        let Some(dragged_panel_entity) = tab_info.container.find_panel(&dragged_panel.panel_id, cx)
        else {
            // 被拖拽的panel不在当前tab中，不处理
            // （未来可以扩展为支持跨Tab拖拽）
            return;
        };

        // 先从当前container中移除被拖拽的panel
        // 这样可以避免同一个panel在容器中出现两次
        let container_after_remove = tab_info.container.remove_panel(&dragged_panel.panel_id, cx);

        let Some(container_after_remove) = container_after_remove else {
            self.tabs.remove(&active_tab_id);
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.remove_tab(&active_tab_id, cx);
            });
            if self.active_tab_id == active_tab_id {
                self.activate_next_tab(cx);
            }
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.emit(WorkspaceEvent::TabBarChanged);
            cx.notify();
            return;
        };

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

            // 更新TabBar显示（Workspace使用统一图标）
            let workspace_label = self.generate_workspace_label();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.modify_tab(&active_tab_id, |tab| {
                    tab.label = workspace_label.clone().into();
                    tab.icon = Some(crate::assets::Icon::Workspace);
                }, cx);
            });
        }

        // 保存被拖拽panel的ID和广播状态，用于后续设置
        let dragged_panel_id = dragged_panel.panel_id.clone();

        // 重新订阅所有panel（这会改变self）
        self.subscribe_to_panels(window, cx);

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

        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
        cx.notify();
    }

    /// 处理 Panel 拖拽到 TabBar 的事件
    /// 将 Panel 从当前 WorkSpace 中分离，创建一个新的独立 Tab
    fn handle_panel_drop_on_tab_bar(
        &mut self,
        dragged_panel: DraggedPanel,
        insert_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_tab_id = self.active_tab_id.clone();

        // 获取当前活动的tab信息
        let Some(tab_info) = self.tabs.get_mut(&active_tab_id) else {
            return;
        };

        // 从container中提取出被拖拽的panel（同时检查是否存在）
        let Some(dragged_panel_entity) = tab_info.container.find_panel(&dragged_panel.panel_id, cx)
        else {
            // 被拖拽的panel不在当前tab中，不处理跨Tab拖拽
            return;
        };

        // 拖回 TabBar 时使用拖拽开始时捕获的“内容标签”。Workspace 内的 Panel
        // 可见标题会被同步成 Workspace 名称，如果这里重新读取 Panel::tab_label，
        // 终端脱离 Workspace 后就会错误地变成 WorkSpace[n]。
        let panel_label = {
            let label = dragged_panel.tab_label.to_string();
            if label.trim().is_empty() {
                "Panel".to_string()
            } else {
                label
            }
        };

        // 从当前container中移除被拖拽的panel
        let new_container = tab_info.container.remove_panel(&dragged_panel.panel_id, cx);

        let Some(new_container) = new_container else {
            self.tabs.remove(&active_tab_id);
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.remove_tab(&active_tab_id, cx);
            });
            if self.active_tab_id == active_tab_id {
                self.activate_next_tab(cx);
            }
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.emit(WorkspaceEvent::TabBarChanged);
            cx.notify();
            return;
        };

        // 更新原tab的container
        tab_info.container = new_container;

        // 检查原tab是否只剩一个panel，如果是则恢复为Normal类型
        let panel_count = tab_info.container.panel_count();
        if panel_count == 1 && tab_info.tab_type == TabType::Workspace {
            tab_info.tab_type = TabType::Normal;
            tab_info.broadcast_enabled = false;

            // 获取剩余panel的标签名
            if let Some(first_panel) = tab_info.container.get_first_panel() {
                let new_label = first_panel
                    .read(cx)
                    .tab_label()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| tab_info.label.clone());
                tab_info.label = new_label;
            }

            // 更新TabBar显示（使用剩余panel的icon）
            let remaining_icon = tab_info
                .container
                .get_first_panel()
                .and_then(|p| p.read(cx).remote_os)
                .map(|os| os.icon());
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.modify_tab(&active_tab_id, |tab| {
                    tab.icon = remaining_icon;
                }, cx);
            });
        }

        // 创建新的tab
        let new_tab_id = format!("tab-{}", self.next_tab_id);
        self.next_tab_id += 1;

        // 创建新的tab信息，使用被拖拽panel的标签名
        let new_tab_info =
            TabInfo::new_normal(&new_tab_id, &panel_label, dragged_panel_entity.clone());
        self.tabs.insert(new_tab_id.clone(), new_tab_info);

        // 添加到TabBar的指定位置，使用拖拽panel自身的icon
        let is_remote = dragged_panel_entity.read(cx).is_remote();
        let new_tab_item = TabItem::new(&new_tab_id, &panel_label)
            .with_icon(dragged_panel.icon)
            .with_is_remote(is_remote);
        self.tab_bar.update(cx, |tab_bar, cx| {
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
            tab_bar.update_overflow_state(cx);
            cx.notify();
        });

        // 更新 panel 的广播状态（普通 tab 不支持广播，禁用广播）
        dragged_panel_entity.update(cx, |p, cx| {
            p.set_broadcast_enabled(false, cx);
        });

        // 订阅新panel的事件
        // 只订阅这个新panel，不需要重新订阅所有panel
        let subscription = cx.subscribe_in(
            &dragged_panel_entity,
            window,
            |this, _panel, event: &PanelEvent, window, cx| {
                this.handle_panel_event(event, window, cx);
            },
        );
        self.panel_subscriptions
            .insert(dragged_panel.panel_id.clone(), subscription);

        // 切换到新tab
        self.active_tab_id = new_tab_id.clone();
        self.switch_to_tab(&new_tab_id, cx);

        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    /// 处理tab重新排序
    fn handle_tab_reorder(
        &mut self,
        dragged_tab_id: &str,
        to_index: usize,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        self.tab_bar.update(cx, |tab_bar, cx| {
            if let Some(from_index) = tab_bar.tabs.iter().position(|t| t.id == dragged_tab_id) {
                if from_index != to_index {
                    let tab = tab_bar.tabs.remove(from_index);
                    tab_bar.tabs.insert(to_index, tab);
                    changed = true;
                    cx.notify();
                }
            }
        });
        if changed {
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.emit(WorkspaceEvent::TabBarChanged);
        }
    }

    /// 处理 TabBar 溢出事件
    /// 当标签页数量过多导致需要滚动时触发
    fn handle_tab_bar_overflow(&mut self, is_overflowing: bool, _cx: &mut Context<Self>) {
        // TabBar 已经自动处理滚动，这里只记录日志
        // 可以在这里添加其他视觉效果，如显示滚动指示器
        if is_overflowing {
            eprintln!("[WORKSPACE] TabBar is overflowing!");
        } else {
            eprintln!("[WORKSPACE] TabBar overflow resolved");
        }
    }

    fn find_panel(&self, panel_id: &str, cx: &App) -> Option<Entity<Panel>> {
        self.tabs
            .values()
            .find_map(|tab_info| tab_info.container.find_panel(panel_id, cx))
    }

    fn focus_panel_content(
        &self,
        panel: Entity<Panel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.defer_in(window, move |_, window, cx| {
            if let Some(handle) = panel.read(cx).active_content_focus_handle(cx) {
                window.focus(&handle, cx);
            }
        });
    }

    /// 切换到指定tab
    pub fn switch_to_tab(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if self.tabs.contains_key(tab_id) {
            self.active_tab_id = tab_id.to_string();
            let active_tab_id = self.active_tab_id.clone();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.set_active_tab(active_tab_id, cx);
            });
            self.sync_tab_broadcast_to_panels(tab_id, cx);
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.emit(WorkspaceEvent::TabBarChanged);
            cx.notify();
        }
    }

    /// 从 Panel 的 TerminalView 读取初始标签
    fn terminal_label_from_panel(panel: &Entity<Panel>, cx: &App) -> String {
        panel
            .read(cx)
            .content()
            .terminal_view()
            .map(|tv| tv.read(cx).tab_label(cx))
            .unwrap_or_else(|| "Local Terminal".into())
    }

    /// 创建本地终端Tab
    pub fn create_local_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let icon = Some(crate::ssh::platform::detect_local_os().icon());
        let panel = cx.new(|cx| {
            Panel::with_terminal(
                format!("panel-{}", self.next_panel_id),
                "group-0",
                Some(format!("tab-{}", self.next_tab_id).into()),
                None,
                cx,
            )
        });
        let label = Self::terminal_label_from_panel(&panel, cx);
        self.add_terminal_tab(label, panel, icon, window, cx);
    }

    /// 使用指定命令创建本地终端Tab
    pub fn create_local_terminal_with_command(
        &mut self,
        command: &str,
        args: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let icon = if command.to_lowercase().contains("wsl") {
            Some(crate::ssh::platform::detect_wsl_os(command, args).icon())
        } else {
            Some(crate::ssh::platform::detect_local_os().icon())
        };
        let panel = cx.new(|cx| {
            Panel::with_terminal_command(
                format!("panel-{}", self.next_panel_id),
                "group-0",
                Some(format!("tab-{}", self.next_tab_id).into()),
                None,
                command,
                args,
                cx,
            )
        });
        let label = Self::terminal_label_from_panel(&panel, cx);
        self.add_terminal_tab(label, panel, icon, window, cx);
    }

    /// 使用指定配置创建 SSH 终端
    pub fn create_ssh_terminal_with_config(
        &mut self,
        config: &SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = config.display_name();
        let initial_icon = config
            .os
            .as_deref()
            .and_then(|id| {
                let os = RemoteOS::from_id(id);
                (os != RemoteOS::Unknown).then(|| os.icon())
            })
            .or_else(|| {
                let os = guess_os_from_text(config.nickname.as_deref().unwrap_or(&config.host));
                (os != RemoteOS::Unknown).then(|| os.icon())
            });
        let panel = cx.new(|cx| {
            Panel::with_ssh_terminal(
                format!("panel-{}", self.next_panel_id),
                "group-0",
                Some(format!("tab-{}", self.next_tab_id).into()),
                Some(label.clone().into()),
                config,
                None,
                cx,
            )
        });
        self.add_terminal_tab(label, panel, initial_icon, window, cx);
    }

    /// 添加终端到Tab（通用方法）
    fn add_terminal_tab(
        &mut self,
        label: String,
        panel: Entity<Panel>,
        icon: Option<crate::assets::Icon>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_id = format!("tab-{}", self.next_tab_id);
        self.next_tab_id += 1;
        self.next_panel_id += 1;

        // 设置正确的 workspace_id 和广播状态
        let workspace_id = self.workspace_id.clone();
        panel.update(cx, |p, cx| {
            p.set_workspace_id(workspace_id);
            p.set_broadcast_enabled(false, cx);
        });

        self.tabs.insert(
            tab_id.clone(),
            TabInfo::new_normal(&tab_id, &label, panel.clone()),
        );

        let is_remote = panel.read(cx).is_remote();
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(
                TabItem::new(&tab_id, &label)
                    .with_icon(icon)
                    .with_is_remote(is_remote),
                cx,
            )
        });

        let subscription =
            cx.subscribe_in(&panel, window, |this, _, event: &PanelEvent, window, cx| {
                this.handle_panel_event(event, window, cx);
            });
        self.panel_subscriptions
            .insert(panel.read(cx).panel_id.to_string(), subscription);

        self.active_tab_id = tab_id.clone();
        self.switch_to_tab(&tab_id, cx);

        cx.defer_in(window, move |_, window, cx| {
            if let Some(handle) = panel.read(cx).active_content_focus_handle(cx) {
                window.focus(&handle, cx);
            }
        });

        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
    }

    /// 处理关闭tab事件
    fn handle_tab_close(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.remove(tab_id);
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.remove_tab(tab_id, cx);
        });

        // 如果关闭的是当前活动tab，切换到相邻 tab（以渲染顺序为准：优先下一个，否则上一个）
        if self.active_tab_id == tab_id {
            let next_tab_id = self.tab_bar.read(cx).active_tab_id().map(|s| s.to_string());
            if let Some(next_tab_id) = next_tab_id {
                self.active_tab_id = next_tab_id.clone();
                self.switch_to_tab(&next_tab_id, cx);
                self.focus_active_tab_panel(window, cx);
            } else {
                window.remove_window();
            }
        }

        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
        cx.notify();
    }

    pub fn terminal_view_for_tab(
        &self,
        tab_id: &str,
        cx: &App,
    ) -> Option<Entity<TerminalView>> {
        let tab_info = self.tabs.get(tab_id)?;
        let panel = tab_info
            .container
            .get_selected_panel(cx)
            .or_else(|| tab_info.container.get_first_panel())?;
        panel.read(cx).content().terminal_view().cloned()
    }

    /// 处理侧边栏终端标签页双击事件：在侧边栏打开文件树
    pub fn handle_sidebar_tab_double_click(
        &mut self,
        tab_id: &str,
        sidebar: Entity<crate::components::macos_sidebar::MacosSidebar>,
        cx: &mut Context<Self>,
    ) {
        if !self.tabs.contains_key(tab_id) {
            log::warn!("未找到标签页: {}", tab_id);
            return;
        }

        let Some(terminal_view) = self.terminal_view_for_tab(tab_id, cx) else {
            log::warn!("标签页 {} 没有终端视图", tab_id);
            return;
        };

        let _ = sidebar.update(cx, |s, cx| s.set_terminal_file_tree(terminal_view, cx));
    }

    /// 拷贝地址 - 将终端连接地址写入剪贴板
    fn handle_tab_copy_address(
        &mut self,
        tab_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui::ClipboardItem;

        let Some(terminal_view) = self.terminal_view_for_tab(tab_id, cx) else {
            return;
        };

        let state = terminal_view.read(cx).session_state(cx);
        match state {
            TerminalSessionState::Ssh { config, .. } => {
                cx.write_to_clipboard(ClipboardItem::new_string(config.host.clone()));
            }
            TerminalSessionState::Local { cwd, .. } => {
                if let Some(path) = cwd {
                    cx.write_to_clipboard(ClipboardItem::new_string(path));
                }
            }
        }
    }

    /// 复制标签页 - 创建新的终端标签页并初始化到相同的工作目录
    fn handle_tab_duplicate(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(terminal_view) = self.terminal_view_for_tab(tab_id, cx) else {
            return;
        };

        let state = terminal_view.read(cx).session_state(cx);
        match state {
            TerminalSessionState::Ssh {
                config,
                remote_cwd,
            } => {
                let panel = cx.new(|cx| {
                    Panel::with_ssh_terminal(
                        format!("panel-{}", self.next_panel_id),
                        "group-0",
                        Some(format!("tab-{}", self.next_tab_id).into()),
                        Some(config.display_name().into()),
                        &config,
                        remote_cwd,
                        cx,
                    )
                });
                let label = Self::terminal_label_from_panel(&panel, cx);
                self.add_terminal_tab(label, panel, None, window, cx);
            }
            TerminalSessionState::Local { cwd, command, args } => {
                if let Some(_cmd) = command {
                    self.create_local_terminal_with_command(&_cmd, &args, window, cx);
                } else {
                    let panel = cx.new(|cx| {
                        Panel::with_terminal_working_directory(
                            format!("panel-{}", self.next_panel_id),
                            "group-0",
                            Some(format!("tab-{}", self.next_tab_id).into()),
                            None,
                            cwd.map(std::path::PathBuf::from),
                            cx,
                        )
                    });
                    let label = Self::terminal_label_from_panel(&panel, cx);
                    self.add_terminal_tab(label, panel, None, window, cx);
                }
            }
        }
    }

    fn start_resize(
        &mut self,
        handle_path: Vec<usize>,
        handle_index: usize,
        start_pos: Pixels,
        is_horizontal: bool,
        sizes_sum: f32,
    ) {
        self.resize_drag = Some(ResizeDragState {
            handle_path,
            handle_index,
            start_pos,
            is_horizontal,
            sizes_sum,
        });
    }

    /// 递归渲染分割容器，支持 resize handle
    fn render_split_container(
        &self,
        container: &SplitContainer,
        path: &[usize],
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().ui;
        match container {
            SplitContainer::Panel(panel) => {
                div().size_full().overflow_hidden().child(panel.clone())
            }
            SplitContainer::Split {
                direction,
                children,
                sizes,
            } => {
                let is_horizontal = *direction == SplitDirection::Horizontal;

                let mut elements: Vec<gpui::AnyElement> = Vec::new();

                for (i, child) in children.iter().enumerate() {
                    let size = sizes.get(i).copied().unwrap_or(1.0);
                    let child_path: Vec<usize> =
                        path.iter().copied().chain(std::iter::once(i)).collect();

                    let child_element = div()
                        .flex_basis(gpui::DefiniteLength::Fraction(size.into()))
                        .flex_shrink(1.0)
                        .overflow_hidden()
                        .min_w(px(0.0))
                        .child(self.render_split_container(child, &child_path, cx))
                        .into_any_element();

                    elements.push(child_element);

                    if i < children.len() - 1 {
                        let handle_path = path.to_vec();
                        let handle_index = i;
                        let sizes_sum: f32 = sizes.iter().sum();

                        let resize_div = div()
                            .when(is_horizontal, |this| {
                                this.w(px(12.0)).h_full().cursor_col_resize()
                            })
                            .when(!is_horizontal, |this| {
                                this.h(px(12.0)).w_full().cursor_row_resize()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .when(is_horizontal, |this| this.w(px(1.0)).h(px(24.0)))
                                    .when(!is_horizontal, |this| this.h(px(1.0)).w(px(24.0)))
                                    .bg(theme.border_strong)
                                    .rounded_sm(),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(
                                    move |this: &mut Self,
                                          event: &gpui::MouseDownEvent,
                                          _window,
                                          _cx| {
                                        this.start_resize(
                                            handle_path.clone(),
                                            handle_index,
                                            if is_horizontal {
                                                event.position.x
                                            } else {
                                                event.position.y
                                            },
                                            is_horizontal,
                                            sizes_sum,
                                        );
                                    },
                                ),
                            );

                        elements.push(resize_div.into_any_element());
                    }
                }

                div()
                    .size_full()
                    .overflow_hidden()
                    .flex()
                    .when(is_horizontal, |this| this.flex_row())
                    .when(!is_horizontal, |this| this.flex_col())
                    .children(elements)
            }
        }
    }
}

/// 将 scp 过程状态实时写入任务共享状态，供 UI 渲染线程读取。
struct TaskStatusReporter {
    status_message: Arc<Mutex<String>>,
    total_bytes: Arc<AtomicU64>,
}

impl TransferStatusReporter for TaskStatusReporter {
    fn report(&self, message: String) {
        let mut guard = self.status_message.lock();
        *guard = message;
    }
    fn set_total_bytes(&self, bytes: u64) {
        self.total_bytes.store(bytes, Ordering::Relaxed);
    }
}

impl TransferTaskHandler for Workspace {
    fn cancel_transfer_task(&mut self, transfer_id: usize, cx: &mut Context<Self>) {
        if let Some(task) = self
            .transfer_tasks
            .iter_mut()
            .find(|task| task.id == transfer_id)
        {
            task.cancelled.store(true, Ordering::Relaxed);
            task.status = TransferTaskStatus::Cancelled;
            cx.notify();
        }
    }

    fn remove_transfer_task(&mut self, transfer_id: usize, cx: &mut Context<Self>) {
        let initial_len = self.transfer_tasks.len();
        self.transfer_tasks.retain(|task| task.id != transfer_id);
        if self.transfer_tasks.len() != initial_len {
            cx.notify();
        }
    }
}

impl Workspace {
    /// 读取当前所有传输任务（只读视图）。
    pub(crate) fn transfer_tasks(&self) -> &[TransferTaskState] {
        &self.transfer_tasks
    }

    /// 清除所有已完成 / 失败 / 已取消的传输任务，保留运行中的条目。
    pub(crate) fn clear_finished_transfer_tasks(&mut self, cx: &mut Context<Self>) {
        let initial_len = self.transfer_tasks.len();
        self.transfer_tasks
            .retain(|task| matches!(task.status, TransferTaskStatus::Running));
        if self.transfer_tasks.len() != initial_len {
            cx.notify();
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
        let theme = cx.theme().ui;
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

        // 自定义 Element 用于全局鼠标事件捕获
        let resize_handler = ResizeHandlerElement {
            workspace: cx.entity().downgrade(),
        };

        div()
            .id("workspace")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.app_background)
            .child(resize_handler)
            // 内容区域 - 只渲染当前活动tab对应的panel(s)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .pt(px(2.0))
                    .pl(px(8.0))
                    .pr(px(8.0))
                    .pb(px(8.0))
                    .when_some(active_container, |this, container| {
                        this.child(self.render_split_container(container, &[], cx))
                    }),
            )
            .on_action(cx.listener(|this, _: &NewLocalTerminal, window, cx| {
                this.create_local_terminal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseSelectedPanel, window, cx| {
                this.close_selected_panel(window, cx);
            }))
    }
}

/// 全局 resize 处理元素 - 在 paint 阶段注册全局鼠标事件
struct ResizeHandlerElement {
    workspace: gpui::WeakEntity<Workspace>,
}

impl Element for ResizeHandlerElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) -> () {
        ()
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint_state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        // 拖拽中全局覆盖光标样式，保持 resize 光标不变
        if let Some(drag_state) = workspace.read(cx).resize_drag.as_ref() {
            let cursor = if drag_state.is_horizontal {
                CursorStyle::ResizeColumn
            } else {
                CursorStyle::ResizeRow
            };
            window.set_window_cursor_style(cursor);
        }

        // 全局鼠标移动事件 - 处理 resize 拖拽
        window.on_mouse_event({
            let workspace = workspace.clone();
            move |event: &MouseMoveEvent, _phase, window, cx: &mut App| {
                workspace.update(cx, |this, cx| {
                    if let Some(ref mut drag_state) = this.resize_drag {
                        let current_pos = if drag_state.is_horizontal {
                            event.position.x
                        } else {
                            event.position.y
                        };
                        let delta = current_pos - drag_state.start_pos;
                        let delta_f32: f32 = delta.into();
                        let viewport = window.viewport_size();
                        let container_size: f32 = if drag_state.is_horizontal {
                            viewport.width.into()
                        } else {
                            viewport.height.into()
                        };
                        // 正确换算：delta_ratio × (container_size / sizes_sum) = delta_pixels
                        // 即 panel 实际像素变化 = delta_pixels，分割条与光标完全同步
                        let delta_ratio = delta_f32 * drag_state.sizes_sum / container_size;

                        if let Some(tab_info) = this.tabs.get_mut(&this.active_tab_id) {
                            let resized = tab_info.container.resize_at_path(
                                &drag_state.handle_path,
                                drag_state.handle_index,
                                delta_ratio,
                                0.1,
                            );
                            if resized {
                                drag_state.start_pos = current_pos;
                                cx.notify();
                            }
                        }
                    }
                });
            }
        });

        // 全局鼠标释放事件 - 结束 resize 拖拽
        window.on_mouse_event({
            let workspace = workspace.clone();
            move |_: &MouseUpEvent, _phase, _window, cx: &mut App| {
                workspace.update(cx, |this, cx| {
                    if this.resize_drag.is_some() {
                        this.resize_drag = None;
                        cx.notify();
                    }
                });
            }
        });
    }
}

impl IntoElement for ResizeHandlerElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}
