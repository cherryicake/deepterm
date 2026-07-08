#![cfg_attr(target_family = "wasm", no_main)]
// 在 Windows Release 构建时隐藏控制台窗口，避免双击启动时弹出终端
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::time::Duration;

pub mod assets;
pub mod builtin_commands;
pub mod components;
pub mod editor;
pub mod file_manager;
mod sidebar_view;
pub mod ssh;
pub mod ssh_view;
pub mod terminal;
pub mod terminal_view;
pub mod theme;
pub mod workspace;

use gpui::prelude::*;
use gpui::{
    AnimationExt, App, Context, Entity, IntoElement, KeyBinding, ParentElement, Render,
    Subscription, WeakEntity, Window, actions, div, px,
};

use crate::components::macos_select::{MacosSelect, SelectPopupPosition};
use crate::components::macos_sidebar::{
    DEFAULT_TAB_GROUP_ID, MacosSidebar, SidebarEvent, SidebarTabGroupState, SidebarTabItem,
};
use crate::components::title_bar::TitleBar;
use crate::editor::{Editor, EditorRuntimeState};
use crate::sidebar_view::font_view::FontSettingsView;
use crate::sidebar_view::host_view::HostView;
use crate::sidebar_view::keychain_view::KeychainSidebarView;

use crate::ssh_view::picker::{ShowSshPicker, SshPicker};
use crate::theme::ActiveTheme;
use crate::workspace::client_decorations::client_side_decorations;
use crate::workspace::ToggleSidebar;
use crate::workspace::persistence::PersistenceManager;
use crate::workspace::session_state::{
    AppSessionState, EditorSessionState, WorkspaceGroupSessionState, WorkspaceSessionState,
};
use crate::workspace::window_manager::WindowManager;
use crate::workspace::workspace::{Workspace, WorkspaceEvent};

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Define application-level actions
actions!(app, [Quit, NewWindow]);

const CONTEXT: &str = "AppWindow";

/// Root component that contains title bar, sidebar and workspace
struct TabGroupWorkspace {
    id: String,
    workspace: Option<Entity<Workspace>>,
    pending_session: Option<WorkspaceSessionState>,
    editor_runtime_state: Option<EditorRuntimeState>,
    editor_session_state: Option<EditorSessionState>,
}

pub struct AppWindow {
    title_bar: Entity<TitleBar>,
    default_workspace: Entity<Workspace>,
    workspace: Entity<Workspace>,
    tab_group_workspaces: Vec<TabGroupWorkspace>,
    ssh_picker: Entity<SshPicker>,
    sidebar: Entity<MacosSidebar>,
    editor: Entity<Editor>,
    tab_group_select: Entity<MacosSelect>,
    default_editor_runtime_state: Option<EditorRuntimeState>,
    default_editor_session_state: Option<EditorSessionState>,
    _sidebar_hide_task: Option<gpui::Task<()>>,
    host_view: Entity<HostView>,
    keychain_view: Entity<KeychainSidebarView>,
    persistence: PersistenceManager,
    workspace_subscriptions: HashMap<gpui::EntityId, Subscription>,
    _sidebar_subscription: Option<Subscription>,
}

impl AppWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let persistence = PersistenceManager::new();
        let session = persistence.load_session().ok().flatten();
        let workspace = session
            .as_ref()
            .and_then(|session| {
                session.groups.iter().find_map(|group| {
                    (group.id == DEFAULT_TAB_GROUP_ID)
                        .then_some(group.workspace.as_ref())
                        .flatten()
                        .and_then(|workspace| Workspace::from_session_state(workspace, window, cx))
                })
            })
            .unwrap_or_else(|| Workspace::new(window, cx));

        let tab_bar = workspace.read(cx).tab_bar.clone();
        let workspace_weak = workspace.downgrade();
        let title_bar = cx.new(|cx| {
            let mut bar = TitleBar::new("title-bar").with_tab_bar(tab_bar);
            bar.bind_workspace(workspace_weak, cx);
            bar
        });

        let workspace_for_picker = workspace.clone();
        let ssh_picker =
            cx.new(|cx| SshPicker::new(window, cx).with_workspace(workspace_for_picker));

        workspace.update(cx, |ws, cx| {
            ws.set_ssh_picker(ssh_picker.clone(), cx);
        });

        // 创建侧边栏相关视图
        let font_settings_view = FontSettingsView::new(window, cx);
        let host_view = HostView::new(window, cx);
        let keychain_view = KeychainSidebarView::new(window, cx);

        // 创建 Sidebar Entity
        let app_window = cx.entity();
        let sidebar = cx.new(|_cx| {
            let mut sidebar = MacosSidebar::new();
            sidebar.set_font_view(font_settings_view);
            sidebar.set_host_view(host_view.clone());
            sidebar.set_keychain_view(keychain_view.clone());
            sidebar.bind_app_window(app_window.downgrade());
            sidebar
        });

        let app_window_for_tab_group = app_window.downgrade();
        sidebar.update(cx, |sidebar, _cx| {
            sidebar.on_tab_group_select(move |group_id, window, cx| {
                if let Some(app_window) = app_window_for_tab_group.upgrade() {
                    let group_id = group_id.to_string();
                    app_window.update(cx, |this, cx| {
                        this.switch_tab_group_workspace(&group_id, window, cx);
                    });
                }
            });
        });

        let app_window_for_select = app_window.downgrade();
        let tab_group_select = MacosSelect::new(window, cx, vec!["0个标签页".into()], "0个标签页");
        tab_group_select.update(cx, |select, cx| {
            select.set_popup_position(SelectPopupPosition::BelowTrigger, cx);
            select.on_select(move |_label, index, window, cx| {
                if let Some(app_window) = app_window_for_select.upgrade() {
                    app_window.update(cx, |this, cx| {
                        let default_label = {
                            let count = this.default_workspace.read(cx).tab_bar.read(cx).tabs.len();
                            format!("{}个标签页", count)
                        };
                        let items = this.sidebar.read(cx).tab_group_select_items(default_label);
                        if let Some((group_id, _)) = items.get(index) {
                            this.switch_tab_group_workspace(group_id, window, cx);
                        }
                    });
                }
            });
        });

        // 设置侧边栏标签页双击回调：立即打开文件树页，后台加载内容
        let app_window_for_dblclick = app_window.downgrade();
        let sidebar_for_dblclick = sidebar.clone();
        sidebar.update(cx, |sidebar, _cx| {
            sidebar.on_tab_double_click(move |group_id, tab_id, _tab_label, this, window, cx| {
                let sidebar = sidebar_for_dblclick.clone();
                let group_id = group_id.to_string();
                let tab_id = tab_id.to_string();
                if let Some(workspace) = Self::activate_sidebar_tab(
                    app_window_for_dblclick.clone(),
                    &group_id,
                    &tab_id,
                    this,
                    window,
                    cx,
                ) {
                    this.show_file_tree_page();
                    window.defer(cx, move |_window, cx| {
                        let _ = workspace.update(cx, |ws, cx| {
                            ws.handle_sidebar_tab_double_click(&tab_id, sidebar, cx);
                        });
                    });
                }
            });
        });

        // 设置侧边栏标签页单击回调：切换 workspace + 顶部标签页 + 聚焦终端 + 高亮侧边栏（立即执行）
        let app_window_for_single = app_window.downgrade();
        sidebar.update(cx, |sidebar, _cx| {
            sidebar.on_tab_single_click(move |group_id, tab_id, _tab_label, this, window, cx| {
                Self::activate_sidebar_tab(
                    app_window_for_single.clone(),
                    &group_id,
                    &tab_id,
                    this,
                    window,
                    cx,
                );
            });
        });
        let sidebar_for_host = sidebar.clone();
        host_view.update(cx, |hv, _cx| {
            hv.on_select(move |id, _window, cx| {
                sidebar_for_host.update(cx, |this, cx| {
                    this.handle_item_select(id, cx);
                    this.notify_app_window(cx);
                });
            });
        });
        let app_window_for_host = app_window.downgrade();
        host_view.update(cx, |hv, _cx| {
            hv.on_connect(move |config, window, cx| {
                if let Some(app_window) = app_window_for_host.upgrade() {
                    app_window.update(cx, |this, cx| {
                        this.workspace.update(cx, |ws, cx| {
                            ws.create_ssh_terminal_with_config(config, window, cx);
                        });
                    });
                }
            });
        });

        // 设置 KeychainSidebarView 回调
        let sidebar_for_keychain = sidebar.clone();
        keychain_view.update(cx, |kv, _cx| {
            kv.on_select(move |id, _window, cx| {
                sidebar_for_keychain.update(cx, |this, cx| {
                    this.handle_item_select(id, cx);
                    this.notify_app_window(cx);
                });
            });
        });

        let sidebar_subscription = cx.subscribe_in(
            &sidebar,
            window,
            move |_this, _, event: &SidebarEvent, window, cx| match event {
                SidebarEvent::TabGroupsChanged => {
                    let app_window = cx.entity().clone();
                    window.defer(cx, move |_window, cx| {
                        let _ = app_window.update(cx, |this, cx| {
                            this.sync_sidebar_tab_groups(cx);
                        });
                    });
                }
            },
        );

        // 创建 Editor（侧栏和 Workspace 之间的文件编辑器）
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(window, cx));
        editor.update(cx, |editor, _cx| {
            editor.bind_app_window(app_window.downgrade());
        });

        let default_editor_session_state = session.as_ref().and_then(|session| {
            session
                .groups
                .iter()
                .find(|group| group.id == DEFAULT_TAB_GROUP_ID)
                .and_then(|group| group.editor.clone())
        });
        if let Some(ref state) = default_editor_session_state {
            editor.update(cx, |editor, cx| {
                editor.restore_from_session_state(state.clone(), window, cx);
            });
        }

        // 设置文件打开回调：侧边栏文件树点击文件 → Editor 打开文件
        let editor_for_open = editor.downgrade();
        sidebar.update(cx, |sidebar, _cx| {
            sidebar.set_on_file_open(move |path, ssh_config, window, cx| {
                if let Some(editor) = editor_for_open.upgrade() {
                    editor.update(cx, |e, cx| {
                        if let Some(config) = ssh_config {
                            e.open_remote_file(path.to_string_lossy().to_string(), config, window, cx);
                        } else {
                            e.open_file(path, window, cx);
                        }
                    });
                }
            });
        });

        let mut this = Self {
            title_bar,
            default_workspace: workspace.clone(),
            workspace: workspace.clone(),
            tab_group_workspaces: Vec::new(),
            ssh_picker,
            sidebar,
            editor,
            tab_group_select,
            default_editor_runtime_state: None,
            default_editor_session_state,
            _sidebar_hide_task: None,
            host_view,
            keychain_view,
            persistence,
            workspace_subscriptions: HashMap::new(),
            _sidebar_subscription: Some(sidebar_subscription),
        };

        this.subscribe_workspace_events(workspace.clone(), window, cx);
        if let Some(session) = session {
            this.restore_tab_group_workspaces(session, cx);
        }
        this.sync_sidebar_tab_groups(cx);

        this
    }

    /// 提取侧边栏标签页单击/双击的公共切换逻辑：切换 workspace、top tab、聚焦终端、高亮侧边栏。
    /// 返回当前 workspace entity，供双击回调后续用于创建文件树。
    fn activate_sidebar_tab(
        app_window: WeakEntity<Self>,
        group_id: &str,
        tab_id: &str,
        sidebar: &mut MacosSidebar,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<Workspace>> {
        let aw = app_window.upgrade()?;
        let workspace = {
            let app = aw.read(cx);
            if group_id == DEFAULT_TAB_GROUP_ID {
                app.default_workspace.clone()
            } else if let Some(ws) = app.tab_group_workspace(group_id) {
                ws
            } else {
                log::warn!("未找到标签页组 workspace: {}", group_id);
                return None;
            }
        };
        aw.update(cx, |app_window, cx| {
            app_window.switch_tab_group_workspace(group_id, window, cx);
        });
        workspace.update(cx, |ws, cx| {
            ws.switch_to_tab(tab_id, cx);
            ws.focus_active_tab_panel(window, cx);
        });
        sidebar.select_item(group_id);
        aw.update(cx, |_, cx| cx.notify());
        Some(workspace)
    }

    fn subscribe_workspace_events(
        &mut self,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity_id = workspace.entity_id();
        if self.workspace_subscriptions.contains_key(&entity_id) {
            return;
        }

        let subscription = cx.subscribe_in(
            &workspace,
            window,
            |this, _, event: &WorkspaceEvent, window, cx| match event {
                WorkspaceEvent::SessionChanged => {
                    this.defer_save_session(cx);
                    // 如果侧边栏正在显示文件树，且顶部 TabBar 切换到了别的终端，
                    // 延迟同步文件树到当前活动终端。
                    let sidebar = this.sidebar.clone();
                    let workspace = this.workspace.clone();
                    window.defer(cx, move |_window, cx| {
                        if sidebar.read(cx).current_view() != &crate::components::macos_sidebar::SidebarView::FileTree {
                            return;
                        }
                        let active_tab_id = workspace.read(cx).active_tab_id().to_string();
                        let active_terminal = workspace.read(cx).terminal_view_for_tab(&active_tab_id, cx);
                        let current_terminal = sidebar.read(cx).file_tree_terminal_view();
                        if active_terminal == current_terminal {
                            return;
                        }
                        if active_terminal.is_some() {
                            let _ = workspace.update(cx, |ws, cx| {
                                ws.handle_sidebar_tab_double_click(&active_tab_id, sidebar, cx);
                            });
                        }
                    });
                }
                WorkspaceEvent::TabBarChanged => {
                    let app_window = cx.entity();
                    window.defer(cx, move |_window, cx| {
                        let _ = app_window.update(cx, |this, cx| {
                            this.sync_sidebar_tab_groups(cx);
                        });
                    });
                }
                WorkspaceEvent::OpenPath { path, ssh_config } => {
                    crate::sidebar_view::open_path::handle_open_path(
                        this.editor.clone(),
                        this.sidebar.clone(),
                        path.clone(),
                        ssh_config.clone(),
                        window,
                        cx,
                    );
                }
            },
        );
        self.workspace_subscriptions.insert(entity_id, subscription);
    }

    fn tab_group_workspace(&self, group_id: &str) -> Option<Entity<Workspace>> {
        self.tab_group_workspaces
            .iter()
            .find(|group| group.id == group_id)
            .and_then(|group| group.workspace.clone())
    }

    fn active_tab_group_id(&self) -> String {
        self.tab_group_workspaces
            .iter()
            .find_map(|group| {
                let workspace = group.workspace.as_ref()?;
                (workspace.entity_id() == self.workspace.entity_id()).then(|| group.id.clone())
            })
            .unwrap_or_else(|| DEFAULT_TAB_GROUP_ID.to_string())
    }

    fn restore_tab_group_workspaces(&mut self, session: AppSessionState, cx: &mut Context<Self>) {
        let mut seen_group_ids = std::collections::HashSet::new();
        let sidebar_groups = session
            .groups
            .iter()
            .filter(|group| {
                group.id != DEFAULT_TAB_GROUP_ID && seen_group_ids.insert(group.id.clone())
            })
            .map(|group| SidebarTabGroupState {
                id: group.id.clone(),
                label: group.label.clone().unwrap_or_else(|| group.id.clone()),
                collapsed: group.collapsed,
            })
            .collect();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.restore_tab_groups(sidebar_groups, cx);
        });

        // 用户创建的标签页组只恢复侧边栏元数据，具体 workspace 延迟到用户
        // 手动选中该组时再创建。这样启动后不会自动连接非默认组里的 SSH，
        // 也不会默认打开用户未选中的标签页组内容。
        let mut seen_group_ids = std::collections::HashSet::new();
        self.tab_group_workspaces = session
            .groups
            .iter()
            .filter(|group| {
                group.id != DEFAULT_TAB_GROUP_ID && seen_group_ids.insert(group.id.clone())
            })
            .map(|group| TabGroupWorkspace {
                id: group.id.clone(),
                workspace: None,
                pending_session: group.workspace.clone(),
                editor_runtime_state: None,
                editor_session_state: group.editor.clone(),
            })
            .collect();

        // 侧边栏默认已选中 DEFAULT_TAB_GROUP_ID（MacosSidebar::new 中设置），
        // 而 workspace 启动时始终是默认标签页组（见 AppWindow::new 中 workspace 的创建逻辑）。
        // 两者天然对齐，无需用 session.active_group_id 覆盖侧边栏选中项。
        // 此外自定义标签页组的 workspace 会在用户手动点击时才创建（延迟创建），
        // 此时用 session.active_group_id 同步侧边栏会导致高亮与实际内容不一致。
        self.sync_sidebar_tab_groups(cx);
    }

    fn session_state(&self, cx: &mut Context<Self>) -> AppSessionState {
        let active_group_id = self.active_tab_group_id();
        let mut groups = Vec::with_capacity(self.tab_group_workspaces.len() + 1);

        let default_editor = if active_group_id == DEFAULT_TAB_GROUP_ID {
            Some(self.editor.read(cx).session_state())
        } else {
            self.default_editor_runtime_state
                .as_ref()
                .map(|state| state.to_session_state())
                .or_else(|| self.default_editor_session_state.clone())
        };
        groups.push(WorkspaceGroupSessionState {
            id: DEFAULT_TAB_GROUP_ID.to_string(),
            label: Some("标签页".to_string()),
            collapsed: false,
            workspace: Some(self.default_workspace.read(cx).session_state(cx)),
            editor: default_editor,
        });

        let sidebar_groups = self.sidebar.read(cx).tab_group_states();
        let mut seen_group_ids = std::collections::HashSet::new();
        groups.extend(sidebar_groups.into_iter().filter_map(|sidebar_group| {
            if !seen_group_ids.insert(sidebar_group.id.clone()) {
                return None;
            }
            let workspace_entry = self
                .tab_group_workspaces
                .iter()
                .find(|group| group.id == sidebar_group.id);
            let editor = workspace_entry.and_then(|group| {
                if group.id == active_group_id {
                    Some(self.editor.read(cx).session_state())
                } else {
                    group
                        .editor_runtime_state
                        .as_ref()
                        .map(|state| state.to_session_state())
                        .or_else(|| group.editor_session_state.clone())
                }
            });
            Some(WorkspaceGroupSessionState {
                id: sidebar_group.id,
                label: Some(sidebar_group.label),
                collapsed: sidebar_group.collapsed,
                workspace: workspace_entry.and_then(|group| {
                    group
                        .workspace
                        .as_ref()
                        .map(|workspace| workspace.read(cx).session_state(cx))
                        .or_else(|| group.pending_session.clone())
                }),
                editor,
            })
        }));

        AppSessionState {
            version: crate::workspace::session_state::SESSION_STATE_VERSION,
            active_group_id,
            groups,
        }
    }

    fn save_session(&self, cx: &mut Context<Self>) {
        let session = self.session_state(cx);
        if let Err(error) = self.persistence.save_session(&session) {
            log::warn!("Failed to save session state: {error}");
        }
    }

    /// 设置初始焦点到工作区的第一个终端
    fn setup_initial_focus(&self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.set_initial_focus(window, cx);
        });
    }

    fn bind_active_workspace(
        &mut self,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace = workspace.clone();
        self.title_bar.update(cx, |title_bar, cx| {
            title_bar.set_workspace(workspace.clone(), cx);
        });
        self.ssh_picker.update(cx, |picker, _cx| {
            picker.set_workspace(workspace.clone());
        });
        workspace.update(cx, |workspace, cx| {
            workspace.set_ssh_picker(self.ssh_picker.clone(), cx);
        });
        self.subscribe_workspace_events(workspace.clone(), window, cx);
        workspace.update(cx, |_, cx| {
            cx.emit(WorkspaceEvent::TabBarChanged);
        });
        cx.notify();
    }

    fn defer_save_session(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(0))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.save_session(cx);
            });
        })
        .detach();
    }

    fn sync_sidebar_tab_groups(&self, cx: &mut Context<Self>) {
        let default_tabs = Self::sidebar_tabs_for_workspace(&self.default_workspace, cx);
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_tab_group_tabs(DEFAULT_TAB_GROUP_ID, default_tabs, cx);
        });

        for group in &self.tab_group_workspaces {
            if let Some(workspace) = &group.workspace {
                let tabs = Self::sidebar_tabs_for_workspace(workspace, cx);
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_tab_group_tabs(group.id.clone(), tabs, cx);
                });
            }
        }

        let default_tab_count = self.default_workspace.read(cx).tab_bar.read(cx).tabs.len();
        let default_label = format!("{}个标签页", default_tab_count);
        let (items, selected_label) = self.sidebar.update(cx, |sidebar, _cx| {
            let items = sidebar.tab_group_select_items(default_label.clone());
            let selected_label = items
                .iter()
                .find(|(id, _)| id == &self.active_tab_group_id())
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| default_label.into());
            (items, selected_label)
        });
        let item_labels: Vec<_> = items.into_iter().map(|(_, label)| label).collect();
        self.tab_group_select.update(cx, |select, cx| {
            select.set_items(item_labels, cx);
            select.set_selected(selected_label, cx);
        });
    }

    fn sidebar_tabs_for_workspace(
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Vec<SidebarTabItem> {
        let tab_bar = workspace.read(cx).tab_bar.clone();
        tab_bar
            .read(cx)
            .tabs
            .iter()
            .map(|tab| SidebarTabItem {
                label: tab.label.to_string(),
                icon: tab.icon,
                active: tab_bar
                    .read(cx)
                    .active_tab_id()
                    .map(|active_id| active_id == &tab.id)
                    .unwrap_or(false),
                tab_id: Some(tab.id.to_string()),
            })
            .collect()
    }

    fn create_tab_group_workspace(
        &mut self,
        group_id: String,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = Workspace::new(window, cx);
        self.subscribe_workspace_events(workspace.clone(), window, cx);
        self.tab_group_workspaces.push(TabGroupWorkspace {
            id: group_id.clone(),
            workspace: Some(workspace.clone()),
            pending_session: None,
            editor_runtime_state: None,
            editor_session_state: None,
        });
        self.switch_tab_group_workspace(&group_id, window, cx);
    }

    fn switch_tab_group_workspace(
        &mut self,
        group_id: &str,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let from_group_id = self.active_tab_group_id();
        if from_group_id == group_id {
            return;
        }

        // 保存当前组的运行时状态（不重新读取文件）。
        let current_runtime_state =
            self.editor
                .update(cx, |editor, cx| editor.take_runtime_state(cx));
        if from_group_id == DEFAULT_TAB_GROUP_ID {
            self.default_editor_runtime_state = Some(current_runtime_state);
        } else if let Some(group) = self
            .tab_group_workspaces
            .iter_mut()
            .find(|group| group.id == from_group_id)
        {
            group.editor_runtime_state = Some(current_runtime_state);
        }

        if group_id == DEFAULT_TAB_GROUP_ID {
            self.bind_active_workspace(self.default_workspace.clone(), window, cx);
            let target_state = self.default_editor_runtime_state.take();
            if let Some(state) = target_state {
                self.editor.update(cx, |editor, cx| {
                    editor.restore_runtime_state(state, cx);
                });
            } else if let Some(ref state) = self.default_editor_session_state {
                self.editor.update(cx, |editor, cx| {
                    editor.restore_from_session_state(state.clone(), window, cx);
                });
                self.default_editor_session_state = None;
            }
            self.sync_file_tree_with_active_tab(window, cx);
            self.defer_save_session(cx);
            return;
        }

        let workspace = if let Some(workspace) = self.tab_group_workspace(group_id) {
            workspace
        } else {
            let pending_session = self
                .tab_group_workspaces
                .iter_mut()
                .find(|group| group.id == group_id)
                .and_then(|group| group.pending_session.take());
            let workspace = pending_session
                .as_ref()
                .and_then(|state| Workspace::from_session_state(state, window, cx))
                .unwrap_or_else(|| Workspace::new(window, cx));
            self.subscribe_workspace_events(workspace.clone(), window, cx);
            if let Some(group) = self
                .tab_group_workspaces
                .iter_mut()
                .find(|group| group.id == group_id)
            {
                group.workspace = Some(workspace.clone());
            } else {
                self.tab_group_workspaces.push(TabGroupWorkspace {
                    id: group_id.to_string(),
                    workspace: Some(workspace.clone()),
                    pending_session: None,
                    editor_runtime_state: None,
                    editor_session_state: None,
                });
            }
            workspace
        };

        self.bind_active_workspace(workspace, window, cx);

        let group = self
            .tab_group_workspaces
            .iter_mut()
            .find(|group| group.id == group_id);
        if let Some(group) = group {
            if let Some(state) = group.editor_runtime_state.take() {
                self.editor.update(cx, |editor, cx| {
                    editor.restore_runtime_state(state, cx);
                });
            } else if let Some(state) = group.editor_session_state.take() {
                self.editor.update(cx, |editor, cx| {
                    editor.restore_from_session_state(state, window, cx);
                });
            }
        }

        self.sync_file_tree_with_active_tab(window, cx);
        self.defer_save_session(cx);
    }

    /// 如果当前侧边栏处于文件树页面，将文件树同步到当前活动工作区的活动终端目录。
    fn sync_file_tree_with_active_tab(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sidebar = self.sidebar.clone();
        let workspace = self.workspace.clone();
        window.defer(cx, move |_window, cx| {
            if *sidebar.read(cx).current_view()
                != crate::components::macos_sidebar::SidebarView::FileTree
            {
                return;
            }
            let active_tab_id = workspace.read(cx).active_tab_id().to_string();
            let _ = workspace.update(cx, |ws, cx| {
                ws.handle_sidebar_tab_double_click(&active_tab_id, sidebar, cx);
            });
        });
    }

    /// 切换侧边栏显示/隐藏
    fn toggle_sidebar(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let needs_hide_task = self.sidebar.update(cx, |s, _cx| s.toggle());
        if needs_hide_task {
            let sidebar = self.sidebar.clone();
            let app_window = cx.entity();
            self._sidebar_hide_task = Some(cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250 + 40))
                    .await;
                sidebar.update(cx, |s, cx| {
                    s.finish_animation();
                    cx.notify();
                });
                let _ = app_window.update(cx, |_, cx| cx.notify());
            }));
        } else {
            self._sidebar_hide_task = None;
        }
        cx.notify();
    }

    /// 渲染侧边栏切换按钮
    fn render_sidebar_toggle(
        &self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.sidebar.read(cx).render_toggle_button(
            cx,
            window.is_fullscreen(),
            cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_sidebar(window, cx);
            }),
        )
    }

    /// 渲染新建标签页组按钮
    fn render_new_tab_group_button(
        &self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.sidebar.read(cx).render_new_tab_group_button(
            cx,
            window.is_fullscreen(),
            cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                let group_id = this.sidebar.update(cx, |sidebar, cx| {
                    let group_id = sidebar.create_tab_group(window, cx);
                    cx.notify();
                    group_id
                });
                this.create_tab_group_workspace(group_id, window, cx);
            }),
        )
    }

    /// 渲染标签页组选择下拉列表（在侧边栏二级页面时显示）。
    fn render_tab_group_select(
        &self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.sidebar.read(cx).render_tab_group_select(
            cx,
            window.is_fullscreen(),
            self.tab_group_select.clone(),
        )
    }

    /// 渲染右侧主内容区域（TitleBar + Workspace）。
    /// TitleBar 的 padding 补偿动画与侧边栏显隐同步。
    fn render_main_area(&self, window: &gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        const TITLE_BAR_EXTRA_PAD_MACOS: f32 = 116.0;
        const TITLE_BAR_EXTRA_PAD_MACOS_FS: f32 = 40.0;
        const TITLE_BAR_EXTRA_PAD_WIN: f32 = 44.0;
        const SIDEBAR_ENTER_MS: u64 = 300;
        const SIDEBAR_EXIT_MS: u64 = 250;

        let is_fullscreen = window.is_fullscreen();

        // 从 Sidebar Entity 读取状态
        let (is_visible, epoch) = {
            let sidebar = self.sidebar.read(cx);
            (sidebar.is_visible(), sidebar.animation_epoch())
        };

        let editor_is_empty = self.editor.read(cx).is_empty();

        // 编辑器打开时，由 TitleBar 内部 left_padding 控制 3px 间距，
        // 外层无需额外偏移；编辑器关闭时才需要外层 padding 为交通灯+toggle预留空间。
        let needs_traffic_pad = cfg!(target_os = "macos") && !is_fullscreen;
        let compact_pad = if cfg!(target_os = "macos") {
            TITLE_BAR_EXTRA_PAD_MACOS_FS
        } else {
            TITLE_BAR_EXTRA_PAD_WIN
        };
        let closed_pad = if !editor_is_empty {
            0.0
        } else if needs_traffic_pad {
            TITLE_BAR_EXTRA_PAD_MACOS
        } else {
            compact_pad
        };
        let (start_pl, end_pl, dur_ms) = if is_visible {
            (closed_pad, 0.0, SIDEBAR_ENTER_MS)
        } else {
            (0.0, closed_pad, SIDEBAR_EXIT_MS)
        };

        // 编辑器打开时，TitleBar 内部 left_padding 设为 3px，
        // 使终端 TabBar 与编辑器分割线统一保持 3px 间距；
        // 编辑器关闭时恢复平台默认内边距。
        self.title_bar.update(cx, |tb, _| {
            if editor_is_empty {
                tb.reset_left_padding();
            } else {
                tb.set_left_padding(px(6.0));
            }
        });

        // 初始状态（epoch == 0）直接应用最终 padding，避免打开时出现一次不必要的展开动画
        let animated_title_bar: gpui::AnyElement = if epoch == 0 {
            div()
                .child(self.title_bar.clone())
                .pl(px(end_pl))
                .into_any_element()
        } else {
            div()
                .child(self.title_bar.clone())
                .with_animation(
                    ("titlebar-pad-anim", epoch),
                    gpui::Animation::new(std::time::Duration::from_millis(dur_ms))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| this.pl(px(start_pl + (end_pl - start_pl) * delta)),
                )
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(animated_title_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.workspace.clone()),
            )
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        let sidebar_visible = self.sidebar.read(cx).is_visible();
        let editor_is_empty = self.editor.read(cx).is_empty();

        let closed_tab_bar_pad = || -> f32 {
            if cfg!(target_os = "macos") && !window.is_fullscreen() {
                124.0
            } else if cfg!(target_os = "macos") {
                48.0
            } else {
                50.0
            }
        };

        if editor_is_empty {
            self.editor.update(cx, |editor, cx| {
                editor.set_paddings(0.0, 0.0, cx);
            });
        } else {
            let pad = if sidebar_visible { 6.0 } else { closed_tab_bar_pad() };
            let content_left = if sidebar_visible { 0.0 } else { 12.0 };
            let content_bottom = 8.0;
            self.editor.update(cx, |editor, cx| {
                editor.set_paddings(content_left, content_bottom, cx);
                editor.set_tab_bar_pad_animation(0, pad, pad, 0, cx);
            });
        }

        let is_menu_view = *self.sidebar.read(cx).current_view() == crate::components::macos_sidebar::SidebarView::Menu;
        let top_control: gpui::AnyElement = if is_menu_view {
            self.render_new_tab_group_button(window, cx).into_any_element()
        } else {
            self.render_tab_group_select(window, cx).into_any_element()
        };

        let mut root = div()
            .size_full()
            .bg(theme.app_background)
            .flex()
            .flex_row()
            .relative()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &ShowSshPicker, window, cx| {
                this.ssh_picker
                    .update(cx, |picker, cx| picker.toggle(window, cx));
            }))
            .on_action(cx.listener(|_this, _: &NewWindow, _window, cx| {
                WindowManager::open_window(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                this.toggle_sidebar(window, cx);
            }))
            .child(self.sidebar.clone())
            .child(self.editor.clone())
            .child(self.render_main_area(window, cx))
            .child(top_control)
            .child(self.render_sidebar_toggle(window, cx))
            .child(self.ssh_picker.clone());

        // Host Modal 必须在窗口根节点渲染，才能全屏居中，不被侧边栏裁剪。
        if let Some(modal) = self.host_view.update(cx, |hv, cx| hv.build_modal(cx)) {
            root = root.child(modal);
        }

        // Keychain Modal 同样在窗口根节点渲染
        if let Some(modal) = self.keychain_view.update(cx, |kv, cx| kv.build_modal(cx)) {
            root = root.child(modal);
        }

        client_side_decorations(root, window, cx)
    }
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Debug)
        .filter_module("russh", log::LevelFilter::Warn)
        .init();

    log::info!("=== DeepTerm 启动 ===");

    use crate::assets::Assets;

    gpui::Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(Assets)
        .run(|cx: &mut App| {
        theme::init(cx);
        sidebar_view::font_settings::FontState::init(cx);
        file_manager::watcher::init(cx);
        components::init(cx);
        terminal_view::init(cx);
        workspace::init(cx);
        editor::init(cx);
        WindowManager::init(cx);
            crate::builtin_commands::executor::init_global_executor();
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                #[cfg(target_os = "windows")]
                KeyBinding::new("ctrl-k", ShowSshPicker, None),
                #[cfg(not(target_os = "windows"))]
                KeyBinding::new("cmd-k", ShowSshPicker, None),
                KeyBinding::new("cmd-n", NewWindow, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());

            WindowManager::open_window(cx);
            cx.activate(true);
        });
}
