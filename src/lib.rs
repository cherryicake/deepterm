//! DeepTerm 应用库：全部业务模块。`main.rs` 仅保留进程入口与全局分配器。
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

pub mod agent;
pub mod assets;
pub mod builtin_commands;
pub mod components;
pub mod editor;
pub mod file_manager;
pub mod keymap;
pub mod lsp;
pub mod programs;
pub mod settings;
pub mod sidebar_view;
pub mod ssh;
pub mod ssh_view;
pub mod terminal;
pub mod terminal_view;
pub mod theme;
pub mod utils;
pub mod vim;
pub mod workspace;

use gpui::prelude::*;
use gpui::{
    AnimationExt, App, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, WeakEntity, Window, actions, div, px, svg,
};

use crate::assets::Icon;
use crate::components::macos_select::{MacosSelect, SelectPopupPosition};
use crate::components::notification::{NotificationKind, NotificationManager};
use crate::editor::{Editor, FileTreeEditorState, FileTreeKey};
use crate::sidebar_view::font_view::FontSettingsView;
use crate::sidebar_view::host_view::HostView;
use crate::sidebar_view::keychain_view::KeychainSidebarView;

use crate::ssh_view::picker::{ShowSshPicker, SshPicker};
use crate::theme::ActiveTheme;
use crate::workspace::ToggleSidebar;
use crate::workspace::client_decorations::client_side_decorations;
use crate::workspace::persistence::PersistenceManager;
use crate::workspace::session_state::{
    AppSessionState, WorkspaceGroupSessionState, WorkspaceSessionState,
};
use crate::workspace::sidebar::{
    DEFAULT_TAB_GROUP_ID, MacosSidebar, SidebarEvent, SidebarTabGroupState, SidebarTabItem,
};
use crate::workspace::title_bar::TitleBar;
use crate::workspace::transfer_bell::TransferBellPanel;
use crate::workspace::window_manager::WindowManager;
use crate::workspace::workspace::{Workspace, WorkspaceEvent};

// Define application-level actions
actions!(app, [Quit, NewWindow]);

const CONTEXT: &str = "AppWindow";

/// 编辑器「窗口跟随」时为主区（终端）保留的最小宽度：窗口变窄时编辑器
/// 按此上限收缩，避免吃掉全部增量把主区挤出窗口。
const MAIN_AREA_MIN_WIDTH: f32 = 320.0;

/// 顶部标签页组下拉在标签页组之后追加的一级菜单（用分割线隔开），
/// 让二级页可以直接切换而无需先返回一级菜单。(菜单 id, 显示名)
const SIDEBAR_QUICK_MENUS: &[(&str, &str)] = &[
    ("host", "Host"),
    ("keychain", "Keychain"),
    ("themes", "主题"),
    ("font", "字体"),
];

/// 每个标签页组保留的「非当前」文件树编辑器状态上限（FIFO 淘汰最旧）。
///
/// 每个条目持有该文件树下所有打开文件的全文基线（`original_content`）、
/// InputState 实体（Rope + 撤销历史 + tree-sitter 树）与 DiffView 实体，
/// 单条目即可达数十 MB。键空间 = 会话中访问过的每个终端/目录：关闭终端
/// 不会回收、只有切回同一棵树才 `take`，无上限时长会话单调增长
/// （长时间使用 RSS 涨到 1GB+ 的主要来源之一）。
///
/// 超出上限时淘汰最旧条目：被淘汰的树切回时按空状态恢复（文件需重新打开），
/// 内存从「随访问过的目录数线性增长」收敛为常数窗口。含未保存修改的条目
/// **不淘汰**（见 [`FileTreeEditorStates::save`]）。
const MAX_FILE_TREE_EDITOR_STATES: usize = 8;

/// `FileTreeKey -> FileTreeEditorState` 的有界表（FIFO 淘汰）。
///
/// 两处使用：默认标签页组（`AppWindow::default_file_tree_editor_states`）与
/// 各标签页组（`TabGroupWorkspace::file_tree_editor_states`）。
#[derive(Default)]
struct FileTreeEditorStates {
    map: HashMap<FileTreeKey, FileTreeEditorState>,
    /// 插入序（只含仍存活的键）。淘汰从队首取；`take` 时同步摘除。
    order: VecDeque<FileTreeKey>,
}

impl FileTreeEditorStates {
    /// 保存一个文件树的状态。已存在的键刷新内容并保持原队位（不重复入队）。
    ///
    /// 淘汰跳过带未保存修改的条目：`FileTreeEditorState` 是这些编辑的唯一
    /// 持有者，淘汰即静默丢失用户还没保存的工作（切回时只能重新从磁盘读）。
    fn save(&mut self, key: FileTreeKey, state: FileTreeEditorState) {
        if self.map.insert(key.clone(), state).is_none() {
            self.order.push_back(key);
        }
        // 淘汰跳过脏条目：每轮最多绕队列一周，全脏时保留全部（宁占内存不丢数据）。
        let mut scanned = 0usize;
        while self.map.len() > MAX_FILE_TREE_EDITOR_STATES && scanned < self.order.len() {
            let Some(oldest) = self.order.front().cloned() else {
                break;
            };
            scanned += 1;
            if self
                .map
                .get(&oldest)
                .is_some_and(|state| state.has_unsaved_edits())
            {
                // 脏条目不可淘汰：挪到队尾，让它排在干净条目之后被淘汰。
                self.order.pop_front();
                self.order.push_back(oldest);
                continue;
            }
            self.order.pop_front();
            self.map.remove(&oldest);
            // 淘汰成功说明队列里还有干净条目，下一轮重新计扫描预算。
            scanned = 0;
        }
    }

    /// 取出并移除一个文件树的状态（切回该树时恢复用）。
    fn take(&mut self, key: &FileTreeKey) -> Option<FileTreeEditorState> {
        self.order.retain(|k| k != key);
        self.map.remove(key)
    }
}

/// Root component that contains title bar, sidebar and workspace
struct TabGroupWorkspace {
    id: String,
    workspace: Option<Entity<Workspace>>,
    pending_session: Option<WorkspaceSessionState>,
    /// 每个文件树的编辑器状态（有界，见 [`FileTreeEditorStates`]）
    file_tree_editor_states: FileTreeEditorStates,
}

pub struct AppWindow {
    title_bar: Entity<TitleBar>,
    /// 传输任务铃铛面板：通过 TitleBar 的 trailing 插槽挂载。
    transfer_bell: Entity<TransferBellPanel>,
    default_workspace: Entity<Workspace>,
    workspace: Entity<Workspace>,
    tab_group_workspaces: Vec<TabGroupWorkspace>,
    ssh_picker: Entity<SshPicker>,
    sidebar: Entity<MacosSidebar>,
    pub(crate) editor: Entity<Editor>,
    notification_manager: Entity<NotificationManager>,
    tab_group_select: Entity<MacosSelect>,
    /// 默认标签页组的文件树编辑器状态（有界，见 [`FileTreeEditorStates`]）
    default_file_tree_editor_states: FileTreeEditorStates,
    /// 当前活动的文件树标识符
    current_file_tree_key: Option<FileTreeKey>,
    _sidebar_hide_task: Option<gpui::Task<()>>,
    host_view: Entity<HostView>,
    keychain_view: Entity<KeychainSidebarView>,
    persistence: PersistenceManager,
    /// 本窗口是否负责写盘（多窗口共用 session_state.json，只能有一个写者）
    is_main_window: bool,
    workspace_subscriptions: HashMap<gpui::EntityId, Subscription>,
    pending_save_task: Option<gpui::Task<()>>,
    _sidebar_subscription: Option<Subscription>,
    /// 标题栏 padding 动画状态：侧边栏 + 编辑器共同驱动（render_main_area 内检测翻转后更新）
    title_pad_epoch: u32,
    title_pad_start: f32,
    title_pad_end: f32,
    title_pad_ms: u64,
    last_sidebar_epoch: u32,
    /// 上帧侧边栏宽度：拖拽右边缘时宽度连续变化但不换 epoch，用它算增量
    /// 并让编辑器按比例分摊（显隐动画期间宽度不变，不会误触发）。
    last_sidebar_width: f32,
    last_editor_epoch: u32,
    /// 编辑器 tab 栏左内边距动画纪元：与侧边栏/编辑器翻转同步递增。
    tab_pad_epoch: u32,
    /// 上次渲染时的全屏状态，用于全屏切换后重新同步静态 pad。
    last_fullscreen: bool,
}

/// AppWindow::new 构建的全部实体（从 300 行内联构造拆出，便于阅读）。
struct BuiltEntities {
    workspace: Entity<Workspace>,
    transfer_bell: Entity<TransferBellPanel>,
    title_bar: Entity<TitleBar>,
    ssh_picker: Entity<SshPicker>,
    host_view: Entity<HostView>,
    keychain_view: Entity<KeychainSidebarView>,
    sidebar: Entity<MacosSidebar>,
    is_main_window: bool,
}

/// wire_callbacks 产出的实体与订阅。
struct WiredCallbacks {
    subscription: Subscription,
    tab_group_select: Entity<MacosSelect>,
    editor: Entity<Editor>,
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
        let bundle = Self::build_entities(workspace.clone(), window, cx);

        let wired = Self::wire_callbacks(&bundle, window, cx);
        let mut this = Self {
            title_bar: bundle.title_bar.clone(),
            transfer_bell: bundle.transfer_bell.clone(),
            default_workspace: workspace.clone(),
            workspace: bundle.workspace.clone(),
            tab_group_workspaces: Vec::new(),
            ssh_picker: bundle.ssh_picker.clone(),
            sidebar: bundle.sidebar.clone(),
            editor: wired.editor,
            notification_manager: cx.new(|_cx| NotificationManager::new()),
            tab_group_select: wired.tab_group_select,
            default_file_tree_editor_states: FileTreeEditorStates::default(),
            current_file_tree_key: None,
            _sidebar_hide_task: None,
            host_view: bundle.host_view.clone(),
            keychain_view: bundle.keychain_view.clone(),
            persistence,
            is_main_window: bundle.is_main_window,
            workspace_subscriptions: HashMap::new(),
            pending_save_task: None,
            _sidebar_subscription: Some(wired.subscription),
            title_pad_epoch: 0,
            title_pad_start: 0.0,
            title_pad_end: 0.0,
            title_pad_ms: 0,
            last_sidebar_epoch: 0,
            last_sidebar_width: 0.0,
            last_editor_epoch: 0,
            tab_pad_epoch: 0,
            last_fullscreen: window.is_fullscreen(),
        };

        this.subscribe_workspace_events(workspace.clone(), window, cx);
        if let Some(session) = session {
            this.restore_tab_group_workspaces(session, cx);
        }
        this.sync_sidebar_tab_groups(cx);

        // 首帧 `render` 之前也可能打开文件：`deepterm <file>` 在 `window.defer`
        // 里执行，早于第一帧渲染。先下发一次可用宽度，保证这种打开也能按
        // 「与终端平分」确定默认宽度（之后每帧由 `render` 刷新）。
        let available_width = this.main_area_width(window, cx);
        this.editor.update(cx, |editor, _cx| {
            editor.set_available_width(available_width);
        });

        this
    }

    fn build_entities(
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BuiltEntities {
        let tab_bar = workspace.read(cx).tab_bar.clone();
        let workspace_weak = workspace.downgrade();
        // 传输铃铛面板：独立的业务组件，通过 TitleBar 的 trailing 插槽挂载，
        // 自行观察 workspace 的传输任务。
        let transfer_bell = cx.new(|cx| {
            let mut bell = TransferBellPanel::new();
            bell.bind_workspace(workspace_weak, cx);
            bell
        });
        let title_bar = cx.new(|_cx| {
            TitleBar::new("title-bar")
                .with_tab_bar(tab_bar)
                .with_trailing(transfer_bell.clone())
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
        // 多窗口共用同一个 session_state.json，必须指定唯一窗口负责写盘，
        // 否则后落盘的窗口会覆盖其他窗口的全部标签页与 SSH 会话。
        let is_main_window = WindowManager::claim_main_window(cx, &app_window);
        let sidebar = cx.new(|_cx| {
            let mut sidebar = MacosSidebar::new();
            sidebar.set_font_view(font_settings_view);
            sidebar.set_host_view(host_view.clone());
            sidebar.set_keychain_view(keychain_view.clone());
            sidebar.bind_app_window(app_window.downgrade());
            sidebar
        });
        BuiltEntities {
            workspace: workspace.clone(),
            transfer_bell,
            title_bar,
            ssh_picker,
            host_view: host_view.clone(),
            keychain_view: keychain_view.clone(),
            sidebar: sidebar.clone(),
            is_main_window,
        }
    }

    fn wire_callbacks(
        bundle: &BuiltEntities,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WiredCallbacks {
        // 解构为与旧 new 内联代码相同的局部名，避免逐行改写接线代码。
        let BuiltEntities {
            workspace,
            transfer_bell: _,
            title_bar: _,
            ssh_picker: _,
            host_view,
            keychain_view,
            sidebar,
            is_main_window: _,
        } = bundle;
        let _workspace = workspace.clone();
        let host_view = host_view.clone();
        let keychain_view = keychain_view.clone();
        let sidebar = sidebar.clone();
        let app_window = cx.entity();

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
                    // 先以只读方式取标签页组快照，避免在回调内嵌套 update AppWindow
                    let tab_items = {
                        let app = app_window.read(cx);
                        let count = app.default_workspace.read(cx).tab_bar.read(cx).tabs.len();
                        app.sidebar
                            .read(cx)
                            .tab_group_select_items(format!("{}个标签页", count))
                    };
                    let tab_count = tab_items.len();
                    if index < tab_count {
                        if let Some((group_id, _)) = tab_items.get(index) {
                            let group_id = group_id.clone();
                            app_window.update(cx, |this, cx| {
                                this.switch_tab_group_workspace(&group_id, window, cx);
                            });
                        }
                    } else if let Some((menu_id, menu_label)) =
                        SIDEBAR_QUICK_MENUS.get(index - tab_count)
                    {
                        let menu_id = menu_id.to_string();
                        let menu_label = menu_label.to_string();
                        // 延后到下一帧更新 sidebar / MacosSelect，避开
                        // MacosSelect::confirm 更新栈内的重入 panic
                        let app_window = app_window.clone();
                        window.defer(cx, move |_window, cx| {
                            app_window.update(cx, |this, cx| {
                                this.sidebar.update(cx, |sidebar, cx| {
                                    sidebar.handle_item_select(&menu_id, cx);
                                });
                                // 二级页本身即为选中态，触发器显示对应一级菜单名
                                this.tab_group_select.update(cx, |select, cx| {
                                    select.set_selected(menu_label.clone(), cx);
                                });
                                cx.notify();
                            });
                        });
                    }
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
                    window.defer(cx, move |window, cx| {
                        workspace.update(cx, |ws, cx| {
                            ws.handle_sidebar_tab_double_click(&tab_id, sidebar, window, cx);
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
                    group_id,
                    tab_id,
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

        // 创建 Editor
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(window, cx));
        editor.update(cx, |editor, _cx| {
            editor.bind_app_window(app_window.downgrade());
        });

        // 文件树点击 → Editor 打开文件
        let editor_for_open = editor.downgrade();
        let sidebar_subscription = cx.subscribe_in(
            &sidebar,
            window,
            move |_this, _, event: &SidebarEvent, window, cx| match event {
                SidebarEvent::TabGroupsChanged => {
                    let app_window = cx.entity().clone();
                    window.defer(cx, move |_window, cx| {
                        app_window.update(cx, |this, cx| {
                            this.sync_sidebar_tab_groups(cx);
                        });
                    });
                }
                SidebarEvent::OpenFile { path, ssh_config } => {
                    let editor = editor_for_open.clone();
                    let path = path.clone();
                    let config = ssh_config.clone();
                    window.defer(cx, move |window, cx| {
                        if let Some(editor) = editor.upgrade() {
                            editor.update(cx, |e, cx| {
                                if let Some(config) = config {
                                    e.open_remote_file(
                                        path.to_string_lossy().to_string(),
                                        *config,
                                        window,
                                        cx,
                                    );
                                } else {
                                    e.open_file(path, window, cx);
                                }
                            });
                        }
                    });
                }
                SidebarEvent::StartTransfer { request } => {
                    // 文件树发起的传输（右键下载 / 拖拽上传）：交给当前激活 workspace
                    // 的传输协调器，复用统一的进度 / 冲突 / 取消链路。
                    let workspace = _this.workspace.clone();
                    let request = (**request).clone();
                    window.defer(cx, move |window, cx| {
                        workspace.update(cx, |ws, cx| {
                            ws.start_transfer_request(
                                "sidebar".to_string(),
                                request,
                                window.window_handle(),
                                cx,
                            );
                        });
                    });
                }
                SidebarEvent::PathRenamed {
                    from,
                    to,
                    ssh_config,
                } => {
                    // 文件树里改了名：已打开的标签必须跟着改，否则保存时会把内容
                    // 写回旧路径、把刚删掉的旧文件又复活出来。
                    let editor = editor_for_open.clone();
                    let (from, to) = (from.clone(), to.clone());
                    let config = ssh_config.as_deref().cloned();
                    window.defer(cx, move |_window, cx| {
                        if let Some(editor) = editor.upgrade() {
                            editor.update(cx, |e, cx| {
                                e.rename_open_path(&from, &to, config.as_ref(), cx);
                            });
                        }
                    });
                }
                SidebarEvent::PathRemoved { path, ssh_config } => {
                    // 文件树里删掉了：关掉对应标签（不弹未保存确认，文件已不存在）。
                    let editor = editor_for_open.clone();
                    let path = path.clone();
                    let config = ssh_config.as_deref().cloned();
                    window.defer(cx, move |_window, cx| {
                        if let Some(editor) = editor.upgrade() {
                            editor.update(cx, |e, cx| {
                                e.close_open_path(&path, config.as_ref(), cx);
                            });
                        }
                    });
                }
                SidebarEvent::OpenDiffLoading { path, repo, staged } => {
                    let editor = editor_for_open.clone();
                    let (path, repo) = (path.clone(), repo.clone());
                    let staged = *staged;
                    window.defer(cx, move |window, cx| {
                        if let Some(editor) = editor.upgrade() {
                            editor.update(cx, |e, cx| {
                                e.open_diff_file_loading(path, repo, staged, window, cx)
                            });
                        }
                    });
                }
                SidebarEvent::GitIndexChanged { path } => {
                    // Git 面板已经自行刷新列表，这里让编辑器重算行标记；
                    // 放弃更改还会改变工作区内容，已打开的 diff 页一并刷新。
                    let editor = editor_for_open.clone();
                    let path = path.clone();
                    window.defer(cx, move |_, cx| {
                        if let Some(editor) = editor.upgrade() {
                            editor.update(cx, |e, cx| {
                                e.refresh_git_marks_for_path(&path, cx);
                                e.refresh_diff_for_path(&path, cx);
                            });
                        }
                    });
                }
                SidebarEvent::GitOperationFailed { message } => {
                    _this.show_notification(
                        message.clone(),
                        crate::components::notification::NotificationKind::Error,
                        cx,
                    );
                }
                SidebarEvent::GitCommitDone => {
                    let editor = editor_for_open.clone();
                    window.defer(cx, move |_, cx| {
                        if let Some(editor) = editor.upgrade() {
                            editor.update(cx, |e, cx| {
                                // HEAD 前移：所有 diff 页的左栏与 hunk 全部过期。
                                e.refresh_all_diff_views(cx);
                                // 已打开文件的行变更标记（HEAD↔工作区）同样
                                // 整体过期，一并重算——此前缺失，刚提交完的
                                // 文件行号区还显示满屏“已修改”色条。
                                e.refresh_all_git_marks(cx);
                            });
                        }
                    });
                    _this.show_notification(
                        "提交成功",
                        crate::components::notification::NotificationKind::Success,
                        cx,
                    );
                }
                SidebarEvent::GitRepoChanged => {
                    let editor = editor_for_open.clone();
                    window.defer(cx, move |_, cx| {
                        if let Some(editor) = editor.upgrade() {
                            editor.update(cx, |e, cx| {
                                // stash pop/冲突解决会整体改写工作区：所有
                                // diff 页与行标记全部重算；操作本身在面板
                                // 已有行内反馈，这里不再弹通知。
                                e.refresh_all_diff_views(cx);
                                e.refresh_all_git_marks(cx);
                            });
                        }
                    });
                }
            },
        );

        WiredCallbacks {
            subscription: sidebar_subscription,
            tab_group_select,
            editor,
        }
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
                    let sidebar = this.sidebar.clone();
                    let workspace = this.workspace.clone();
                    window.defer(cx, move |window, cx| {
                        if sidebar.read(cx).current_view()
                            != &crate::workspace::sidebar::SidebarView::FileTree
                        {
                            return;
                        }
                        let active_tab_id = workspace.read(cx).active_tab_id().to_string();
                        workspace.update(cx, |ws, cx| {
                            ws.handle_sidebar_tab_double_click(&active_tab_id, sidebar, window, cx);
                        });
                    });
                }
                WorkspaceEvent::TabBarChanged => {
                    let app_window = cx.entity();
                    window.defer(cx, move |_window, cx| {
                        app_window.update(cx, |this, cx| {
                            this.sync_sidebar_tab_groups(cx);
                        });
                    });
                }
                WorkspaceEvent::OpenPath {
                    path,
                    line,
                    col,
                    ssh_config,
                    terminal_cwd,
                } => {
                    crate::workspace::sidebar::handle_open_path(
                        this.editor.clone(),
                        this.sidebar.clone(),
                        path.clone(),
                        *line,
                        *col,
                        ssh_config.as_deref().cloned(),
                        terminal_cwd.clone(),
                        window,
                        cx,
                    );
                }
                WorkspaceEvent::TransferFinished { dirs } => {
                    // 传输真正结束后再刷新文件树（远端没有 watcher，只能靠这条链路）。
                    let sidebar = this.sidebar.clone();
                    let dirs = dirs.clone();
                    window.defer(cx, move |_window, cx| {
                        let Some(tree) = sidebar.read(cx).file_tree_view() else {
                            return;
                        };
                        tree.update(cx, |tree, cx| tree.refresh_affected_dirs(&dirs, cx));
                    });
                }
            },
        );
        self.workspace_subscriptions.insert(entity_id, subscription);

        // 注入「最后一个 tab 关闭、窗口即将移除」回调：
        // 关窗会销毁本窗口的 AppWindow，防抖任务随之丢失，必须同步落盘。
        let app_window_weak = cx.entity().downgrade();
        workspace.update(cx, |ws, _cx| {
            ws.set_on_last_tab_close(Box::new(move |cx, default_state| {
                if let Some(app) = app_window_weak.upgrade() {
                    app.update(cx, |app, cx| {
                        app.flush_pending_save_with_default(default_state, cx)
                    });
                }
            }));
        });
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
                file_tree_editor_states: FileTreeEditorStates::default(),
            })
            .collect();

        // 侧边栏默认已选中 DEFAULT_TAB_GROUP_ID（MacosSidebar::new 中设置），
        // 而 workspace 启动时始终是默认标签页组（见 AppWindow::new 中 workspace 的创建逻辑）。
        // 两者天然对齐，无需用 session.active_group_id 覆盖侧边栏选中项。
        // 此外自定义标签页组的 workspace 会在用户手动点击时才创建（延迟创建），
        // 此时用 session.active_group_id 同步侧边栏会导致高亮与实际内容不一致。
        self.sync_sidebar_tab_groups(cx);
    }

    /// 构造完整窗口会话状态；默认标签页组的状态由调用方提供。
    ///
    /// 关窗路径上 `default_workspace` 正在被 `do_tab_close` 更新，不能再
    /// `read(cx)`；调用方（`Workspace::before_window_remove`）已经在自己
    /// 的 `&mut self` 里算好了这份状态。
    fn session_state_with_default(
        &self,
        default_state: WorkspaceSessionState,
        cx: &mut Context<Self>,
    ) -> AppSessionState {
        let active_group_id = self.active_tab_group_id();
        let mut groups = Vec::with_capacity(self.tab_group_workspaces.len() + 1);

        groups.push(WorkspaceGroupSessionState {
            id: DEFAULT_TAB_GROUP_ID.to_string(),
            label: Some("标签页".to_string()),
            collapsed: false,
            workspace: Some(default_state),
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
            })
        }));

        AppSessionState {
            version: crate::workspace::session_state::SESSION_STATE_VERSION,
            active_group_id,
            groups,
        }
    }

    fn save_session(&mut self, cx: &mut Context<Self>) {
        let default_state = self.default_workspace.read(cx).session_state(cx);
        self.save_session_with_default(default_state, cx);
    }

    /// 落盘会话；默认标签页组的状态由调用方提供，避免重入读
    /// `default_workspace`（关窗路径正在 `&mut` 更新它）。
    fn save_session_with_default(
        &mut self,
        default_state: WorkspaceSessionState,
        cx: &mut Context<Self>,
    ) {
        // 只有主窗口写盘：多窗口共用 session_state.json，
        // 若各窗口都写，后写者会整体覆盖其他窗口的标签页与 SSH 会话。
        if !self.ensure_session_ownership(cx) {
            log::debug!("跳过会话落盘：本窗口不是主窗口");
            return;
        }
        let session = self.session_state_with_default(default_state, cx);
        if let Err(error) = self.persistence.save_session(&session) {
            log::warn!("Failed to save session state: {error}");
        }
    }

    /// 主窗口关闭后，由存活的窗口接管落盘职责，
    /// 避免"主窗口一关，剩下的窗口再也无法持久化"。
    fn ensure_session_ownership(&mut self, cx: &mut Context<Self>) -> bool {
        if self.is_main_window {
            return true;
        }
        let entity = cx.entity();
        if WindowManager::take_over_main_window(cx, &entity) {
            log::info!("主窗口已关闭，本窗口接管会话持久化");
            self.is_main_window = true;
        }
        self.is_main_window
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
        self.transfer_bell.update(cx, |bell, cx| {
            bell.bind_workspace(workspace.downgrade(), cx);
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
        // 胶囊挂在**当前** workspace 的标签栏上：切换分组后要把入口重新挂到
        // 新的那一条，否则胶囊还指着上一组的标签栏（点了没反应）。
        self.rebind_workspace_chip(cx);
        cx.notify();
    }

    /// 把顶部胶囊（标签页组入口）挂到当前活动 workspace 的标签栏上。
    ///
    /// 胶囊的文案来自标签页组名，点击回落到侧边栏的选择器，因此只需要挂一次
    /// 入口；文案随后由 `sync_sidebar_tab_groups` 每帧刷新。
    fn rebind_workspace_chip(&self, cx: &mut Context<Self>) {
        // 切换分组后标签栏是另一条，入口（回调）得重新挂上去；文案由
        // `sync_sidebar_tab_groups` 的下一帧覆盖，这里先沿用旧的以免闪一下空。
        let sidebar_for_chip = self.sidebar.clone();
        let on_chip: crate::workspace::tab_bar::NewTabCallback =
            std::rc::Rc::new(move |window, cx| {
                sidebar_for_chip.update(cx, |sidebar, cx| {
                    sidebar.open_tab_group_select(window, cx);
                });
            });
        self.workspace.update(cx, |workspace, cx| {
            workspace.tab_bar.update(cx, |bar, cx| {
                let label = bar.workspace_chip_label().cloned();
                bar.set_workspace_chip(label, Some(on_chip), cx);
            });
        });
    }

    fn defer_save_session(&mut self, cx: &mut Context<Self>) {
        // 去抖：300ms 内多次触发仅落盘一次，旧任务随 Task drop 自动取消
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = this.update(cx, |this, cx| {
                // 清理句柄，避免持有 Task
                this.pending_save_task = None;
                this.save_session(cx);
            });
        });
        self.pending_save_task = Some(task);
    }

    /// 同步落盘会话并取消防抖任务。退出/关窗前调用，
    /// 避免防抖窗口内的最后变更丢失。
    pub fn flush_pending_save(&mut self, cx: &mut Context<Self>) {
        let default_state = self.default_workspace.read(cx).session_state(cx);
        self.flush_pending_save_with_default(default_state, cx);
    }

    /// 与 [`Self::flush_pending_save`] 相同，但默认标签页组状态由
    /// `Workspace::before_window_remove` 预先算好，避免在 Workspace
    /// 正在更新时重入 `read(cx)`。
    pub fn flush_pending_save_with_default(
        &mut self,
        default_state: WorkspaceSessionState,
        cx: &mut Context<Self>,
    ) {
        self.pending_save_task.take();
        self.save_session_with_default(default_state, cx);
    }

    /// 消费侧边栏「展开标签页组选择器」的请求（顶部胶囊点出来的）。
    ///
    /// 胶囊在标签栏里，真正的 `MacosSelect` 在侧边栏顶部：两者不在同一棵
    /// 子树，只能经侧边栏的一个一次性标志中转。放在每帧渲染前消费，
    /// 保证侧边栏已经按展开后的状态完成了一次布局，弹窗才落得对位置。
    fn flush_tab_group_select_request(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let requested = self.sidebar.update(cx, |sidebar, _| {
            sidebar.take_tab_group_select_open_request()
        });
        if requested {
            self.tab_group_select.update(cx, |select, cx| {
                select.open(window, cx);
            });
        }
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
        let (items, tab_selected_label) = self.sidebar.update(cx, |sidebar, _cx| {
            let items = sidebar.tab_group_select_items(default_label.clone());
            let selected_label = items
                .iter()
                .find(|(id, _)| id == &self.active_tab_group_id())
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| default_label.clone().into());
            (items, selected_label)
        });
        // 根据侧边栏当前视图决定下拉的选中态：二级页时高亮对应一级菜单
        let current_view = self.sidebar.read(cx).current_view().clone();
        let selected_label: SharedString = match current_view {
            crate::workspace::sidebar::SidebarView::Host => "Host".into(),
            crate::workspace::sidebar::SidebarView::Keychain => "Keychain".into(),
            crate::workspace::sidebar::SidebarView::Theme => "主题".into(),
            crate::workspace::sidebar::SidebarView::Font => "字体".into(),
            _ => tab_selected_label,
        };
        let mut item_labels: Vec<SharedString> =
            items.into_iter().map(|(_, label)| label).collect();
        let separator_after = item_labels.len().checked_sub(1);
        // 追加一级菜单，带分隔线与标签页组隔开
        item_labels.extend(
            SIDEBAR_QUICK_MENUS
                .iter()
                .map(|(_, label)| SharedString::from(*label)),
        );
        self.tab_group_select.update(cx, |select, cx| {
            select.set_items_with_separator(item_labels, separator_after, cx);
            select.set_selected(selected_label.clone(), cx);
        });

        // 顶部「空间胶囊」：herdr 的 Terminals LAYOUT 用一个胶囊显示当前 space
        // 并点开选择器；deepterm 的对应概念是标签页组。文案与下拉的选中项同源，
        // 只有在一级页（文件树）时才给胶囊——二级页（Host / 主题 / 字体…）的
        // 选中态属于「菜单」，用它当胶囊文案会让人以为自己在某个标签页组里。
        //
        // 胶囊挂在**当前活动 workspace** 的标签栏上（不是默认那条）：切了分组
        // 之后入口要跟着走，否则点它还在说上一组的名字。
        let chip_label = (!matches!(
            current_view,
            crate::workspace::sidebar::SidebarView::Host
                | crate::workspace::sidebar::SidebarView::Keychain
                | crate::workspace::sidebar::SidebarView::Theme
                | crate::workspace::sidebar::SidebarView::Font
        ))
        .then(|| selected_label.clone());
        // 点胶囊 = 展开侧边栏的标签页组选择器（herdr 的 space picker 对应物），
        // 与侧边栏顶部那个下拉是同一份 items / 同一个选中态。
        let sidebar_for_chip = self.sidebar.clone();
        let on_chip: crate::workspace::tab_bar::NewTabCallback =
            std::rc::Rc::new(move |window, cx| {
                sidebar_for_chip.update(cx, |sidebar, cx| {
                    sidebar.open_tab_group_select(window, cx);
                });
            });
        self.workspace.update(cx, |workspace, cx| {
            workspace.tab_bar.update(cx, |bar, cx| {
                bar.set_workspace_chip(chip_label, Some(on_chip.clone()), cx);
            });
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
                tiles: tab.stacked_tiles(None),
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
            file_tree_editor_states: FileTreeEditorStates::default(),
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
            self.defer_return_to_file_tree(window, cx);
            return;
        }

        // 保存当前组的编辑器状态到对应的文件树
        if let Some(file_tree_key) = self.current_file_tree_key.clone() {
            let file_tree_state = self
                .editor
                .update(cx, |editor, cx| editor.save_to_file_tree_state(cx));

            if from_group_id == DEFAULT_TAB_GROUP_ID {
                self.default_file_tree_editor_states
                    .save(file_tree_key, file_tree_state);
            } else if let Some(group) = self
                .tab_group_workspaces
                .iter_mut()
                .find(|group| group.id == from_group_id)
            {
                group
                    .file_tree_editor_states
                    .save(file_tree_key, file_tree_state);
            }
        }

        // 清空当前文件树标识符
        self.current_file_tree_key = None;
        // 切换标签页组时丢弃侧边栏拖拽基线：下一帧按当前宽度重新建立基线，
        // 避免把跨组的宽度差当成一次拖拽增量分摊给编辑器（编辑器宽度是
        // 全局的，而基线属于上一组的状态）。
        self.last_sidebar_width = 0.0;

        if group_id == DEFAULT_TAB_GROUP_ID {
            self.bind_active_workspace(self.default_workspace.clone(), window, cx);
            self.defer_return_to_file_tree(window, cx);
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
                    file_tree_editor_states: FileTreeEditorStates::default(),
                });
            }
            workspace
        };

        self.bind_active_workspace(workspace, window, cx);
        self.defer_return_to_file_tree(window, cx);
        self.defer_save_session(cx);
    }

    /// 延后返回文件树：若当前在二级页（Host/Keychain/主题/字体）则切到文件树，
    /// 并将文件树同步到当前活动终端的目录。
    ///
    /// 必须通过 window.defer 延后执行：切换可能发自 MacosSelect::confirm 的
    /// 更新栈内，同步读取/更新 MacosSidebar 会触发重入 panic。
    /// 已在文件树/菜单页时同步为无操作（同终端提前返回 / 非文件树页跳过）。
    fn defer_return_to_file_tree(&self, window: &mut Window, cx: &mut Context<Self>) {
        let app_window = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            if let Some(app_window) = app_window.upgrade() {
                app_window.update(cx, |this, cx| {
                    let is_secondary_view = {
                        let view = this.sidebar.read(cx).current_view();
                        !matches!(
                            view,
                            crate::workspace::sidebar::SidebarView::Menu
                                | crate::workspace::sidebar::SidebarView::FileTree
                        )
                    };
                    if is_secondary_view {
                        this.sidebar.update(cx, |sidebar, _| {
                            sidebar.show_file_tree_page();
                        });
                    }
                    this.sync_file_tree_with_active_tab(window, cx);
                });
            }
        });
    }

    /// 文件树切换时调用：保存旧文件树的编辑器状态，恢复新文件树的编辑器状态
    pub fn on_file_tree_switched(
        &mut self,
        old_key: Option<FileTreeKey>,
        new_key: FileTreeKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 1. 保存旧文件树的编辑器状态
        if let Some(old_key) = old_key {
            let file_tree_state = self
                .editor
                .update(cx, |editor, cx| editor.save_to_file_tree_state(cx));

            let from_group_id = self.active_tab_group_id();
            if from_group_id == DEFAULT_TAB_GROUP_ID {
                self.default_file_tree_editor_states
                    .save(old_key, file_tree_state);
            } else if let Some(group) = self
                .tab_group_workspaces
                .iter_mut()
                .find(|group| group.id == from_group_id)
            {
                group.file_tree_editor_states.save(old_key, file_tree_state);
            }
        }

        // 2. 更新当前文件树标识符
        self.current_file_tree_key = Some(new_key.clone());

        // 3. 从新文件树的存储中恢复之前保存的文件
        let from_group_id = self.active_tab_group_id();
        let file_tree_state = if from_group_id == DEFAULT_TAB_GROUP_ID {
            self.default_file_tree_editor_states.take(&new_key)
        } else {
            self.tab_group_workspaces
                .iter_mut()
                .find(|group| group.id == from_group_id)
                .and_then(|group| group.file_tree_editor_states.take(&new_key))
        };

        // 4. 如果有需要恢复的文件，恢复到编辑器
        if let Some(state) = file_tree_state
            && !state.open_files_info.is_empty()
        {
            self.editor.update(cx, |editor, cx| {
                editor.restore_from_file_tree_state(state, window, cx);
            });
        }
    }

    /// 如果当前侧边栏处于文件树页面，将文件树同步到当前活动工作区的活动终端目录。
    fn sync_file_tree_with_active_tab(&self, window: &mut Window, cx: &mut Context<Self>) {
        let sidebar = self.sidebar.clone();
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            if *sidebar.read(cx).current_view() != crate::workspace::sidebar::SidebarView::FileTree
            {
                return;
            }
            let active_tab_id = workspace.read(cx).active_tab_id().to_string();
            workspace.update(cx, |ws, cx| {
                ws.handle_sidebar_tab_double_click(&active_tab_id, sidebar, window, cx);
            });
        });
    }

    /// 切换侧边栏显示/隐藏
    fn toggle_sidebar(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) {
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
                app_window.update(cx, |_, cx| cx.notify());
            }));
        } else {
            self._sidebar_hide_task = None;
        }
        cx.notify();
    }

    /// 编辑器与主区（终端）共享的可用横向宽度：窗口宽 − 侧边栏占位。
    ///
    /// 编辑器首次打开文件时按它的一半作为默认宽度（与终端区域平分），
    /// 窗口跟随的宽度上限也由它推出。侧边栏隐藏时占位为 0。
    fn main_area_width(&self, window: &gpui::Window, cx: &gpui::App) -> f32 {
        let win_w: f32 = window.viewport_size().width.into();
        let sidebar = self.sidebar.read(cx);
        let sidebar_occ = if sidebar.is_visible() {
            sidebar.width() + 8.0
        } else {
            0.0
        };
        (win_w - sidebar_occ).max(0.0)
    }

    /// 编辑器「窗口跟随」的宽度上限：可用宽度 − 主区最小宽。
    /// `render` 与 `render_main_area` 共用（两处都要下发同一上限）。
    fn editor_follow_limit(&self, window: &gpui::Window, cx: &gpui::App) -> f32 {
        (self.main_area_width(window, cx) - MAIN_AREA_MIN_WIDTH).max(0.0)
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
    /// TitleBar 的 padding 补偿动画与侧边栏显隐/编辑器展开收起同步。
    fn render_main_area(
        &mut self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        const TITLE_BAR_EXTRA_PAD_MACOS: f32 = 116.0;
        const TITLE_BAR_EXTRA_PAD_MACOS_FS: f32 = 40.0;
        const TITLE_BAR_EXTRA_PAD_WIN: f32 = 44.0;
        const SIDEBAR_ENTER_MS: u64 = 300;
        const SIDEBAR_EXIT_MS: u64 = 250;
        const EDITOR_ANIM_MS: u64 = 120;

        let is_fullscreen = window.is_fullscreen();

        // 全屏切换会改变 closed pad 的目标值；重置 tab 栏 pad 动画纪元，
        // 以便下方的静态分支重新与目标同步。
        let fullscreen_changed = is_fullscreen != self.last_fullscreen;
        self.last_fullscreen = is_fullscreen;
        if fullscreen_changed {
            self.tab_pad_epoch = 0;
        }

        // 从 Sidebar / Editor Entity 读取状态
        let (is_visible, sidebar_epoch, sidebar_w) = {
            let sidebar = self.sidebar.read(cx);
            (
                sidebar.is_visible(),
                sidebar.animation_epoch(),
                sidebar.width(),
            )
        };
        let (editor_is_empty, editor_epoch) = {
            let editor = self.editor.read(cx);
            (editor.is_empty(), editor.open_anim_epoch())
        };

        // 编辑器打开时，由 TitleBar 内部 left_padding 控制 3px 间距，
        // 外层无需额外偏移；编辑器关闭时才需要外层 padding 为交通灯+toggle预留空间。
        let needs_traffic_pad = cfg!(target_os = "macos") && !is_fullscreen;
        let compact_pad = if cfg!(target_os = "macos") {
            TITLE_BAR_EXTRA_PAD_MACOS_FS
        } else {
            TITLE_BAR_EXTRA_PAD_WIN
        };
        // 编辑器为空时的标题栏预留宽度（平台/全屏相关常量，不随编辑器状态变化）
        let closed_pad = if needs_traffic_pad {
            TITLE_BAR_EXTRA_PAD_MACOS
        } else {
            compact_pad
        };
        // 最终目标：编辑器打开或侧边栏可见时为 0，否则为 closed_pad
        let pad_target = if editor_is_empty && !is_visible {
            closed_pad
        } else {
            0.0
        };

        // 检测动画触发源：编辑器展开/收起（150ms）或侧边栏显隐（300/250ms）。
        // 两个源共用一个动画元素（key = title_pad_epoch），翻转时从「上一目标」滑向「新目标」。
        // 编辑器的 tab 栏左内边距同步驱动：侧边栏显隐时与侧边栏宽度动画同时长/同缓动，
        // 保证 tab 标签随编辑器整体平滑移动而不是先瞬时弹开再滑回。
        let tab_pad_open = 6.0;
        let tab_pad_closed = if cfg!(target_os = "macos") && !is_fullscreen {
            124.0
        } else if cfg!(target_os = "macos") {
            48.0
        } else {
            50.0
        };
        let tab_pad_target = if editor_is_empty || is_visible {
            tab_pad_open
        } else {
            tab_pad_closed
        };

        // 恢复（切标签页组/文件树）请求：内容被整体替换，`open_anim_epoch` 与
        // `sidebar_epoch` 都不变，下面三条同步路径都不进，残留的旧 epoch 会在
        // 下次渲染带着旧起点重播一次滑动动画（文件 tab 从中间滑到左侧）。
        // 无条件消费，避免请求滞留到之后某帧误触发。
        let pad_snap = self
            .editor
            .update(cx, |editor, _cx| editor.take_tab_bar_pad_snap_request());
        // 本帧是否有真实翻转：分支体会更新 `last_*`，必须在进入分支前判定
        // （否则恢复时的静态对齐会覆盖同帧真实的侧边栏 pad 动画）。
        let editor_flipped = editor_epoch != self.last_editor_epoch;
        let sidebar_flipped = sidebar_epoch != self.last_sidebar_epoch;

        if editor_flipped {
            self.last_editor_epoch = editor_epoch;
            self.title_pad_epoch = self.title_pad_epoch.wrapping_add(1);
            // 编辑器翻转前的目标：打开侧恒为 0；空侧取决于侧边栏是否可见
            self.title_pad_start = if editor_is_empty || is_visible {
                0.0
            } else {
                closed_pad
            };
            self.title_pad_end = pad_target;
            self.title_pad_ms = EDITOR_ANIM_MS;
            // 编辑器展开/收起时 tab 栏处于裁剪渐显/渐隐中，pad 瞬时应用即可
            if !editor_is_empty {
                self.editor.update(cx, |editor, cx| {
                    editor.set_tab_bar_pad_animation(0, tab_pad_target, tab_pad_target, 0, cx);
                });
            }
        } else if sidebar_flipped {
            self.last_sidebar_epoch = sidebar_epoch;
            self.title_pad_epoch = self.title_pad_epoch.wrapping_add(1);
            // 翻转前侧边栏处于相反可见状态
            let before_visible = !is_visible;
            self.title_pad_start = if editor_is_empty && !before_visible {
                closed_pad
            } else {
                0.0
            };
            self.title_pad_end = pad_target;
            self.title_pad_ms = if is_visible {
                SIDEBAR_ENTER_MS
            } else {
                SIDEBAR_EXIT_MS
            };
            // tab 栏 pad 与侧边栏动画同步：从翻转前的 pad 滑向目标 pad
            if !editor_is_empty {
                let before = if before_visible {
                    tab_pad_open
                } else {
                    tab_pad_closed
                };
                self.tab_pad_epoch = self.tab_pad_epoch.wrapping_add(1);
                let epoch = self.tab_pad_epoch;
                let ms = if is_visible {
                    SIDEBAR_ENTER_MS
                } else {
                    SIDEBAR_EXIT_MS
                };
                self.editor.update(cx, |editor, cx| {
                    editor.set_tab_bar_pad_animation(epoch, before, tab_pad_target, ms, cx);
                });
            }
            // 侧边栏显隐释放/占用的宽度：编辑器按比例分摊，其余留给主区（终端）。
            // 与侧边栏动画同时长同缓动，避免终端先跳变再被拉回。
            let sidebar_delta = if is_visible { sidebar_w } else { -sidebar_w };
            let ms = if is_visible {
                SIDEBAR_ENTER_MS
            } else {
                SIDEBAR_EXIT_MS
            };
            let limit = self.editor_follow_limit(window, cx);
            self.editor.update(cx, |editor, cx| {
                editor.follow_sidebar_width(sidebar_delta, limit, ms, cx);
            });
        } else if self.tab_pad_epoch == 0 {
            // 无翻转（初始状态/全屏切换等）：保持静态 pad 与目标同步
            if !editor_is_empty {
                self.editor.update(cx, |editor, cx| {
                    editor.set_tab_bar_pad_animation(0, tab_pad_target, tab_pad_target, 0, cx);
                });
            }
        }

        // 恢复后的静态对齐：放在翻转分支之后，避免覆盖同帧真实的侧边栏/开合动画。
        // 仅在「本帧无翻转」时生效（有翻转时 pad 已由上面分支同步到目标）。
        if pad_snap && !editor_is_empty && !editor_flipped && !sidebar_flipped {
            self.editor.update(cx, |editor, cx| {
                editor.set_tab_bar_pad_animation(0, tab_pad_target, tab_pad_target, 0, cx);
            });
        }

        // 侧边栏拖拽：宽度连续变化但不换 epoch（显隐动画只改渲染层，不动
        // `width`），所以这里的增量只可能来自拖拽。与显隐走同一套分摊比例，
        // 但不播动画（拖拽要 1:1 跟手，按当前鼠标位置立即结算）。
        // 0.0 作为「尚未记录」的哨兵（宽度下限 220，不会与真实值冲突）。
        if self.last_sidebar_width > 0.0 && (sidebar_w - self.last_sidebar_width).abs() > 0.5 {
            let delta = sidebar_w - self.last_sidebar_width;
            let limit = self.editor_follow_limit(window, cx);
            self.editor.update(cx, |editor, cx| {
                editor.follow_sidebar_width(delta, limit, 0, cx);
            });
        }
        self.last_sidebar_width = sidebar_w;

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

        // 初始状态（title_pad_epoch == 0）直接应用最终 padding，避免启动时出现不必要的展开动画
        let animated_title_bar: gpui::AnyElement = if self.title_pad_epoch == 0 {
            div()
                .child(self.title_bar.clone())
                .pl(px(pad_target))
                .into_any_element()
        } else {
            let (start, end, ms) = (self.title_pad_start, self.title_pad_end, self.title_pad_ms);
            let epoch = self.title_pad_epoch;
            div()
                .child(self.title_bar.clone())
                .with_animation(
                    ("titlebar-pad-anim", epoch),
                    gpui::Animation::new(std::time::Duration::from_millis(ms))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| this.pl(px(start + (end - start) * delta)),
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

    pub fn show_notification(
        &mut self,
        message: impl Into<SharedString>,
        kind: NotificationKind,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .notification_manager
            .update(cx, |manager, _cx| manager.add(message, kind));
        let entity = self.notification_manager.downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(4)).await;
                let should_dismiss = entity
                    .update(cx, |manager, _cx| manager.should_auto_dismiss(id))
                    .unwrap_or(true);
                if should_dismiss {
                    let _ = entity.update(cx, |manager, cx| {
                        manager.dismiss(id);
                        cx.notify();
                    });
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        self.flush_tab_group_select_request(window, cx);

        let editor_is_empty = self.editor.read(cx).is_empty();

        // 编辑器「窗口跟随」的宽度上限：窗口宽 − 侧边栏占位 − 主区最小宽。
        // 每帧下发，保证窗口缩窄时编辑器让位，不会把主区挤出窗口。
        // 同时下发可用宽度：编辑器首次打开文件时按它的一半作为默认宽度
        //（与终端区域平分）。
        let main_area_width = self.main_area_width(window, cx);
        let editor_limit = (main_area_width - MAIN_AREA_MIN_WIDTH).max(0.0);
        self.editor.update(cx, |editor, _cx| {
            editor.set_window_follow_limit(editor_limit);
            editor.set_available_width(main_area_width);
        });

        if editor_is_empty {
            self.editor.update(cx, |editor, cx| {
                editor.set_paddings(0.0, 0.0, cx);
            });
        } else {
            // 左 padding 不随侧边栏显隐变化：内容区与面板边框的间距需保持一致
            //（侧边栏开关按钮只悬浮在 TabBar 区域，由 tab_pad_target 单独补偿）。
            let content_left = 0.0;
            let content_bottom = 8.0;
            self.editor.update(cx, |editor, cx| {
                editor.set_paddings(content_left, content_bottom, cx);
            });
        }

        let is_menu_view =
            *self.sidebar.read(cx).current_view() == crate::workspace::sidebar::SidebarView::Menu;
        let top_control: gpui::AnyElement = if is_menu_view {
            self.render_new_tab_group_button(window, cx)
                .into_any_element()
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
                WindowManager::open_window(cx, Vec::new());
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

        // 全局通知 Toast（单条：最新一条覆盖旧的）
        if let Some(notification) = self.notification_manager.read(cx).current() {
            let theme = cx.theme().ui;
            let id = notification.id;
            let entity = self.notification_manager.clone();
            let (bg_color, icon_color, icon) = match notification.kind {
                NotificationKind::Error => {
                    (theme.error_background, theme.error, Icon::XmarkCircleFill)
                }
                NotificationKind::Warning => (
                    theme.warning_background,
                    theme.warning,
                    Icon::EllipsisCircleFill,
                ),
                NotificationKind::Success => (
                    theme.success_background,
                    theme.success,
                    Icon::PlusCircleFull,
                ),
                NotificationKind::Info => (theme.notification_background, theme.text, Icon::Bell),
            };

            let entity_hover = self.notification_manager.clone();
            let toast = div()
                .id("global-notification")
                .absolute()
                .top(px(12.0))
                .left(px(12.0))
                .right(px(12.0))
                .flex()
                .justify_center()
                .on_hover(move |hovered, _window, cx| {
                    cx.update_entity(&entity_hover, |manager, _cx| {
                        manager.set_hovered(id, *hovered);
                    });
                })
                .child(
                    div()
                        .px(px(16.0))
                        .py(px(10.0))
                        .rounded(px(10.0))
                        .bg(bg_color)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            svg()
                                .size(px(18.0))
                                .text_color(icon_color)
                                .path(icon.path()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(theme.text)
                                .child(notification.message.clone()),
                        )
                        .child(
                            div()
                                .id("notification-dismiss")
                                .cursor_pointer()
                                .on_click(move |_event, _window, cx| {
                                    cx.update_entity(&entity, |manager, cx| {
                                        manager.dismiss(id);
                                        cx.notify();
                                    });
                                })
                                .child(
                                    svg()
                                        .size(px(16.0))
                                        .text_color(theme.text_muted)
                                        .path(Icon::WindowClose.path()),
                                ),
                        ),
                )
                .into_any_element();
            root = root.child(toast);
        }

        client_side_decorations(root, window, cx)
    }
}

/// 测试二进制的分配计数器装配（见 [`utils::alloc_probe`]）。
/// 仅 `cargo test`（lib 自身 harness）时存在；bench 由各自的
/// bench crate 自行声明，正式构建不受影响。
#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: utils::alloc_probe::CountingAllocator<std::alloc::System> =
    utils::alloc_probe::CountingAllocator {
        inner: std::alloc::System,
    };

/// 应用启动时的子系统初始化（`main.rs` 调用）。
///
/// 抽到库里是为了**可测**：`main.rs` 是二进制目标，`cargo test --lib`
/// 覆盖不到它。此前 `vim::init` 漏调用导致 `Registers` / `Marks` 两个全局
/// 从未注册，用户按 `yy` 直接 panic——而测试全绿，因为它们各自手动装了
/// 那两个全局，走的不是生产路径。
///
/// 顺序有依赖，不要随意调整：
/// 1. `settings` 必须最先（theme/font/keymap 都从它取值）；
/// 2. `keymap` 依赖 settings（vim 开关决定是否加载 vim 键位表）；
/// 3. `lsp` 必须在 `editor` 之前（开 tab 时向它查询服务器）；
/// 4. `vim` 必须在任何编辑操作之前（寄存器/标记是编辑路径上的全局）。
pub fn init(cx: &mut gpui::App) {
    settings::init(cx);
    theme::init(cx);
    sidebar_view::font_settings::FontState::init(cx);
    keymap::init(cx);
    file_manager::watcher::init(cx);
    lsp::init(cx);
    vim::init(cx);
    components::init(cx);
    terminal_view::init(cx);
    workspace::init(cx);
    editor::init(cx);
    workspace::window_manager::WindowManager::init(cx);
}

#[cfg(test)]
mod init_tests {
    /// 启动初始化必须注册 vim 的两个全局。
    ///
    /// 回归：`vim::init` 曾在 `main.rs` 里漏调用，`Registers` / `Marks`
    /// 从未注册，用户按 `yy` 就 panic
    /// （`no global registered of type deepterm::vim::registers::Registers`）。
    ///
    /// 这条用例驱动的是**生产同一条初始化函数**（`crate::init`），
    /// 而不是测试里手动装全局——后者会让这类漏接线永远隐形。
    #[gpui::test]
    fn init_registers_vim_globals(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::init(cx);
        });
        cx.update(|cx| {
            assert!(
                cx.has_global::<crate::vim::Registers>(),
                "crate::init must register vim Registers"
            );
            assert!(
                cx.has_global::<crate::vim::Marks>(),
                "crate::init must register vim Marks"
            );
        });
    }

    /// 启动初始化必须注册其余关键全局（同一类缺陷的通用护栏）。
    #[gpui::test]
    fn init_registers_core_globals(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::init(cx);
        });
        cx.update(|cx| {
            assert!(
                cx.has_global::<crate::settings::SettingsStore>(),
                "SettingsStore"
            );
            assert!(cx.has_global::<crate::theme::ThemeState>(), "ThemeState");
            assert!(cx.has_global::<crate::settings::FontState>(), "FontState");
            assert!(cx.has_global::<crate::lsp::LspManager>(), "LspManager");
        });
    }
}
