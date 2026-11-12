use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    App, Context, CursorStyle, Element, Entity, FocusHandle, GlobalElementId, InteractiveElement,
    IntoElement, LayoutId, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, Style,
    Styled, Subscription, Window, div, px, relative,
};
use gpui::{EventEmitter, KeyBinding, actions, prelude::*};

/// 广播输入队列 - 全局共享，用于从TerminalView发送输入到Workspace
/// (workspace_id, panel_id, message)
fn get_broadcast_queue() -> &'static Mutex<Vec<(String, String, BroadcastMsg)>> {
    static QUEUE: OnceLock<Mutex<Vec<(String, String, BroadcastMsg)>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

use crate::agent::AgentStatus;
use crate::assets::Icon;
use crate::components::button::{
    Button, ButtonVariants, Icon as MenuIcon, Sizable, Size as ButtonSize,
};
use crate::components::modal::Modal;
use crate::file_manager::{FileEndpoint, TransferStatusReporter, TransferTaskState};
use crate::ssh::SshConnectionConfig;
use crate::ssh::platform::{RemoteOS, guess_os_from_text};
use crate::ssh_view::picker::SshPicker;
use crate::terminal_view::{BroadcastMsg, TerminalView};
use crate::theme::ActiveTheme;
use crate::workspace::panel::{DraggedPanel, Panel, PanelContent, PanelEvent, PanelPlacement};
use crate::workspace::session_state::{
    TabSessionState, TabSessionType, TerminalSessionState, WorkspaceSessionState,
};
use crate::workspace::split_manager::{SplitContainer, SplitDirection, SplitNavDirection};
use crate::workspace::tab_bar::{
    CopyTabAddress, DraggedTab, DuplicateTab, ReconnectTab, TabAgentSnapshot, TabBar, TabItem,
};

// Define workspace actions
actions!(
    workspace,
    [
        NewLocalTerminal,
        CloseSelectedPanel,
        NewTerminalTab,
        ReopenClosedTab,
        NextTab,
        PreviousTab,
        FocusSplitLeft,
        FocusSplitRight,
        FocusSplitUp,
        FocusSplitDown,
        /// 把当前面板向右分屏（herdr 的 `pane.split direction=right`）。
        SplitRight,
        /// 把当前面板向下分屏（herdr 的 `pane.split direction=down`）。
        SplitDown,
        /// 放大 / 缩回当前面板（herdr 的 `pane.zoom`）。
        TogglePaneZoom
    ]
);

/// 跳转到第 N 个 tab（0 基）：cmd-1..9 为前 9 个，cmd-0 为第 10 个。
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workspace, no_json)]
pub struct GotoTab(pub usize);

actions!(
    workspace,
    [IncreaseFontSize, DecreaseFontSize, ResetFontSize]
);

/// 终端字号上下限：cmd +/- 在此范围内调整。
const FONT_SIZE_MIN: f32 = 6.0;
const FONT_SIZE_MAX: f32 = 40.0;
const FONT_SIZE_STEP: f32 = 1.0;

/// 最后一个 tab 关闭时的回调（不需要跨线程，故无 `Send + Sync`）。
///
/// 回调接收**已经算好的** `WorkspaceSessionState`：调用发生在
/// `Workspace::do_tab_close` 的 `&mut self` 更新过程中，若回调里再去
/// `AppWindow::default_workspace.read(cx)` 会触发 GPUI 的
/// "cannot read ... while it is already being updated" panic。
type LastTabCloseCallback = Box<dyn Fn(&mut gpui::App, WorkspaceSessionState)>;
/// 可选 [`LastTabCloseCallback`]。
type OptionalLastTabCloseCallback = Option<LastTabCloseCallback>;

/// `SshConnectionConfig` 有 184 字节，直接内嵌会让整个枚举膨胀到 208 字节，
/// 而 `SessionChanged` / `TabBarChanged` 这类高频小事件也要跟着付这份拷贝代价。
/// 装箱后枚举降到 32 字节。
#[derive(Clone, Debug)]
pub enum WorkspaceEvent {
    SessionChanged,
    TabBarChanged,
    OpenPath {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
        ssh_config: Option<Box<SshConnectionConfig>>,
        terminal_cwd: Option<String>,
    },
    /// 一次传输任务到达终态（完成 / 跳过 / 取消 / 失败）。
    /// `dirs` 是受影响的目录（目标目录，Move 时再带上源所在目录），
    /// 供文件树等在**传输结束后**刷新 —— 之前只能在发起时刷新，等于空转。
    TransferFinished {
        dirs: Vec<FileEndpoint>,
    },
}

impl EventEmitter<WorkspaceEvent> for Workspace {}
pub fn init(cx: &mut App) {
    // Bind keyboard shortcuts
    // 使用 None 作为 context，让这些快捷键全局有效
    crate::keymap::bind(
        cx,
        [
            #[cfg(target_os = "windows")]
            KeyBinding::new("ctrl-l", NewLocalTerminal, None),
            #[cfg(not(target_os = "windows"))]
            KeyBinding::new("cmd-l", NewLocalTerminal, None),
            KeyBinding::new("cmd-w", CloseSelectedPanel, None),
            // 新 tab：cmd-t（win/linux 用 ctrl-shift-t，ctrl-t 是 shell 的输出重定向切换）
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-t", NewTerminalTab, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-shift-t", NewTerminalTab, None),
            // 重开最近关闭的 tab：cmd-shift-t（mac）/ ctrl-alt-t（其他）
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-shift-t", ReopenClosedTab, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-alt-t", ReopenClosedTab, None),
            // Tab 轮换：cmd-shift-] / cmd-shift-[（win/linux 用 ctrl-pageup/down）
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-shift-]", NextTab, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-shift-[", PreviousTab, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-pageup", PreviousTab, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            // cmd+1..9 直接切到第 N 个 tab（0 = 第 10 个）
            KeyBinding::new("cmd-1", GotoTab(0), None),
            KeyBinding::new("cmd-2", GotoTab(1), None),
            KeyBinding::new("cmd-3", GotoTab(2), None),
            KeyBinding::new("cmd-4", GotoTab(3), None),
            KeyBinding::new("cmd-5", GotoTab(4), None),
            KeyBinding::new("cmd-6", GotoTab(5), None),
            KeyBinding::new("cmd-7", GotoTab(6), None),
            KeyBinding::new("cmd-8", GotoTab(7), None),
            KeyBinding::new("cmd-9", GotoTab(8), None),
            // cmd-0 是第 10 个 tab（与 cmd-1..9 同源），字号复位让位给 cmd-shift-0
            KeyBinding::new("cmd-0", GotoTab(9), None),
            // 分屏导航：cmd+alt+方向键（win/linux 用 ctrl+alt+方向键）
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-alt-left", FocusSplitLeft, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-alt-right", FocusSplitRight, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-alt-up", FocusSplitUp, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-alt-down", FocusSplitDown, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-alt-left", FocusSplitLeft, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-alt-right", FocusSplitRight, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-alt-up", FocusSplitUp, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-alt-down", FocusSplitDown, None),
            // 字号：cmd +/-/0（win/linux 用 ctrl +/-/0）。
            // 注意 win/linux 下 ctrl+0 / ctrl+plus 不会与浏览器风格冲突。
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-+", IncreaseFontSize, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-", DecreaseFontSize, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-shift-0", ResetFontSize, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-+", IncreaseFontSize, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-", DecreaseFontSize, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-shift-0", ResetFontSize, None),
        ],
    );
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
    /// 被放大的面板 id（herdr 的 `pane.zoom`）。
    ///
    /// 缩放是**每个标签各自的状态**，不是窗口级的：一个标签里放大某块面板，
    /// 切到别的标签再切回来，它应该还是放大的。`None` 表示正常分屏。
    pub zoomed_panel: Option<String>,
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
            zoomed_panel: None,
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
    /// Resize拖拽状态
    resize_drag: Option<ResizeDragState>,
    /// SSH picker for this window's workspace
    ssh_picker: Option<Entity<SshPicker>>,
    /// 最后一个 tab 关闭、窗口即将移除前的回调（由 AppWindow 注入，
    /// 用于同步落盘会话——防抖任务会随窗口销毁而丢失）。
    on_last_tab_close: OptionalLastTabCloseCallback,
    /// 下一个传输任务序号
    pub(crate) next_transfer_id: usize,
    /// 文件传输任务状态
    pub(crate) transfer_tasks: Vec<TransferTaskState>,
    /// 关闭确认弹窗状态：仅当待关闭终端有存活前台进程时弹出
    close_confirm: Entity<crate::components::modal::ModalState>,
    /// 待确认的关闭目标（确认后真正执行）
    pending_close: Option<PendingClose>,
    /// 已关闭 tab 的 undo 栈（cmd-shift-t 恢复）；上限 10，逐出时 kill 其终端
    closed_tabs: VecDeque<ClosedTab>,
}

/// 关闭确认的目标：整个 tab 或单个 panel（多 panel tab 内 cmd-w 只关选中 panel）
#[derive(Clone, Debug)]
enum PendingClose {
    Tab(String),
    Panel(String),
}

/// 被 undo 关闭的 tab 快照：进程与滚动区保留，重开时原样放回
struct ClosedTab {
    /// tab 原标签（恢复时沿用；若 label 冲突 Workspace 类型会重新编号，可接受）
    label: String,
    tab_type: TabType,
    container: SplitContainer,
    /// 恢复时用的 tab id（取关闭时原 id，避免 session 内引用旧 id）
    tab_id: String,
    /// 关闭时的广播状态（重开后原样恢复，而不是一律关掉）
    broadcast_enabled: bool,
}

/// undo 关闭 tab 的栈上限：超过后逐出最旧并 kill 其终端。
///
/// 每个条目保活一个完整终端（PTY 线程 + 回滚 + Kitty 图片），250×59、
/// 10k 行时单个可达 60 MB 级。原先 10 个能把“关掉的标签”累积到 600 MB，
/// 是“关了很多标签内存不降”的第一解释；降到 3 个把上限压到约 180 MB，
/// 同时保留最近三次撤销能力。
const MAX_CLOSED_TABS: usize = 3;

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

    /// 刷新一个标签的标题与图标叠列：标题取**当前选中 panel** 正在运行的
    /// 程序名 + 目录，图标取该标签下所有 panel 的程序方块（选中项在最上层）。
    ///
    /// 依据 herdr 的做法：标签表达的是「这一格正在干什么」，不是「它是个
    /// 组合容器」。deepterm 的标签可以容纳多个分屏 panel，于是把各 panel 的
    /// 程序方块横向叠起来，选中的那个置顶。
    ///
    /// 只有一个 panel 时与普通标签完全一致（叠放退化成一个方块）。
    fn refresh_tab_visual(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(tab_info) = self.tabs.get(tab_id) else {
            return;
        };
        let panels = tab_info.container.collect_panels_vec();
        if panels.is_empty() {
            return;
        }

        // 每个 panel 的当前程序；顺序即面板顺序。
        let mut entries: Vec<(bool, crate::workspace::tab_bar::TileProgram, String, String)> =
            Vec::with_capacity(panels.len());
        for panel in &panels {
            let panel = panel.read(cx);
            let selected = panel.is_selected;
            let Some(view) = panel.content().terminal_view() else {
                // 非终端 panel（纯文本占位）：不参与图标叠列，但仍可能被选中。
                continue;
            };
            let view = view.read(cx);
            let (title, cwd) = view.tab_title_parts(cx);
            entries.push((
                selected,
                crate::workspace::tab_bar::TileProgram::from_program(&view.foreground_program(cx)),
                title,
                cwd,
            ));
        }

        if entries.is_empty() {
            return;
        }

        // 选中的放最后 = 叠放时最上层；标题也取它。
        let selected_pos = entries.iter().position(|(selected, ..)| *selected);
        let (title, cwd) = match selected_pos {
            Some(index) => (entries[index].2.clone(), entries[index].3.clone()),
            None => (entries[0].2.clone(), entries[0].3.clone()),
        };
        let mut ordered = entries;
        if let Some(index) = selected_pos {
            let picked = ordered.remove(index);
            ordered.push(picked);
        }
        let tiles: Vec<_> = ordered.into_iter().map(|(_, tile, ..)| tile).collect();

        // 标签自己的 label 也同步成"选中 panel 的程序名"：会话恢复、
        // 侧边栏列表、拖拽预览都读它，不能只改渲染层。
        if let Some(tab_info) = self.tabs.get_mut(tab_id) {
            tab_info.label = title.clone();
        }
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.set_tab_panels(tab_id, tiles, title.into(), cwd.into(), cx);
        });
    }

    /// 刷新所有标签的标题与图标叠列（标签增删、合并、重排后的统一收口）。
    fn refresh_all_tab_visuals(&mut self, cx: &mut Context<Self>) {
        let tab_ids: Vec<String> = self.tabs.keys().cloned().collect();
        for tab_id in tab_ids {
            self.refresh_tab_visual(&tab_id, cx);
        }
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
                focus_handle,
                panel_subscriptions: HashMap::new(),
                workspace_id: workspace_id.clone(),
                resize_drag: None,
                ssh_picker: None,
                on_last_tab_close: None,
                next_transfer_id: 1,
                transfer_tasks: Vec::new(),
                close_confirm: crate::components::modal::ModalState::entity(cx),
                pending_close: None,
                closed_tabs: VecDeque::new(),
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

        // 标签名先与会话里存的一致；恢复完成后由 `refresh_all_tab_visuals`
        // 按「当前选中的 panel 正在跑什么」重新计算。
        let label_for = |tab: &TabSessionState| -> String { tab.label.clone() };

        let tab_items = state
            .tabs
            .iter()
            .map(|tab| TabItem::new(&tab.id, label_for(tab)))
            .collect();
        let tab_bar = TabBar::new_with_tabs("tab-bar", cx, tab_items);

        let active_tab_id = if state.tabs.iter().any(|tab| tab.id == state.active_tab_id) {
            state.active_tab_id.clone()
        } else {
            state.tabs[0].id.clone()
        };

        let tabs = state
            .tabs
            .iter()
            .map(|tab| {
                let label = label_for(tab);
                let container = SplitContainer::from_session_state(
                    &tab.container,
                    &workspace_id,
                    &tab.id,
                    &label,
                    cx,
                );
                let tab_info = TabInfo {
                    id: tab.id.clone(),
                    label,
                    tab_type: match tab.tab_type {
                        TabSessionType::Normal => TabType::Normal,
                        TabSessionType::Workspace => TabType::Workspace,
                    },
                    container,
                    broadcast_enabled: tab.broadcast_enabled,
                    // 缩放是运行时状态，不跨会话恢复：重启后按正常分屏显示。
                    zoomed_panel: None,
                };
                (tab.id.clone(), tab_info)
            })
            .collect::<HashMap<_, _>>();

        // 从现有 tab id（`tab-{n}`）解析最大序号：仅用 max(tabs.len()) 时，
        // 非连续 id（如中间关过 tab 后保存的 [tab-3, tab-5, tab-8]）会算出
        // 已被占用的序号，新 tab id 与现存 id 冲突并覆盖 HashMap 里的 TabInfo。
        let max_restored_tab_seq = state
            .tabs
            .iter()
            .filter_map(|t| t.id.strip_prefix("tab-"))
            .filter_map(|s| s.parse::<usize>().ok())
            .max()
            .map_or(0, |m| m + 1);
        // panel id 同理：从恢复出的分割树里解析最大序号。
        let mut restored_panel_ids = Vec::new();
        for tab in &state.tabs {
            tab.container.collect_panel_ids(&mut restored_panel_ids);
        }
        let max_restored_panel_seq = restored_panel_ids
            .iter()
            .filter_map(|id| id.strip_prefix("panel-"))
            .filter_map(|s| s.parse::<usize>().ok())
            .max()
            .map_or(0, |m| m + 1);
        let workspace = cx.new(|cx| {
            let mut workspace = Self {
                tab_bar,
                tabs,
                active_tab_id: active_tab_id.clone(),
                next_tab_id: state
                    .next_tab_id
                    .max(state.tabs.len())
                    .max(max_restored_tab_seq),
                next_panel_id: state.next_panel_id.max(1).max(max_restored_panel_seq),
                local_terminal_count: state.local_terminal_count.max(1),
                focus_handle,
                panel_subscriptions: HashMap::new(),
                workspace_id,
                resize_drag: None,
                ssh_picker: None,
                on_last_tab_close: None,
                next_transfer_id: 1,
                transfer_tasks: Vec::new(),
                close_confirm: crate::components::modal::ModalState::entity(cx),
                pending_close: None,
                closed_tabs: VecDeque::new(),
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
            workspace.tab_bar.update(cx, |tab_bar, cx| {
                for (tab_id, is_remote) in remote_status {
                    tab_bar.modify_tab(tab_id.as_str(), |tab| tab.is_remote = is_remote, cx);
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

    /// 注入「最后一个 tab 关闭、窗口即将移除」回调（AppWindow 用它同步落盘会话）。
    pub fn set_on_last_tab_close(
        &mut self,
        callback: Box<dyn Fn(&mut gpui::App, WorkspaceSessionState)>,
    ) {
        self.on_last_tab_close = Some(callback);
    }

    /// 注入后调用此辅助：关窗前同步落盘。
    ///
    /// 会话状态必须在**进入回调之前**、由 `&mut self` 自己算好：
    /// 回调会去 `AppWindow::session_state` 里读 `default_workspace`，
    /// 而此刻同一个 Workspace 正在被 `do_tab_close` 更新，重入读会 panic。
    /// 这里算状态只读 `self.tabs`/`tab_bar`/各 panel，不会重入 Workspace。
    fn before_window_remove(&mut self, cx: &mut Context<Self>) {
        if self.on_last_tab_close.is_none() {
            return;
        }
        let default_state = self.session_state(cx);
        if let Some(callback) = self.on_last_tab_close.take() {
            // &mut Context 经 DerefMut 强转为 &mut App
            callback(cx, default_state);
        }
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

    /// 将焦点设置到当前活动 tab 的第一个 panel
    pub fn focus_active_tab_panel(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab_info) = self.tabs.get(&self.active_tab_id)
            && let Some(panel) = tab_info.container.get_first_panel()
            && let Some(handle) = panel.read(cx).active_content_focus_handle(cx)
        {
            window.focus(&handle, cx);
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
            self.before_window_remove(cx);
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

            // 重新连接 - 普通标签重连单个终端，WorkSpace 标签重连其下全部终端
            tab_bar.on_tab_reconnect({
                let workspace = workspace.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    workspace.update(cx, |_this, cx| {
                        cx.defer_in(window, move |this, window, cx| {
                            this.handle_tab_reconnect(&id, window, cx);
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
                    .menu_with_icon(
                        "重新连接",
                        MenuIcon::new("icons/arrow.clockwise.svg"),
                        Box::new(ReconnectTab),
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
            tab_info.container.collect_panels_into(&mut panels);
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
                }) && let Some(os) = os
                {
                    let icon = os.icon();
                    self.tab_bar.update(cx, |tab_bar, cx| {
                        tab_bar.modify_tab(
                            &tab_id,
                            |tab| {
                                tab.icon = Some(icon);
                            },
                            cx,
                        );
                    });
                    cx.emit(WorkspaceEvent::TabBarChanged);
                }
                // 保存检测到的OS到SSH配置
                if let (Some(config_id), Some(os)) = (ssh_config_id, os) {
                    let os_id = os.to_id();
                    let mut config = crate::ssh::config::SshGlobalConfig::load();
                    if let Some(conn) = config
                        .saved_connections
                        .iter_mut()
                        .find(|c| c.id() == *config_id)
                        && conn.os.as_deref() != Some(os_id)
                    {
                        conn.os = Some(os_id.to_string());
                        if let Err(e) = config.save() {
                            log::warn!("保存 SSH 配置失败: {}", e);
                        }
                    }
                }
            }
            PanelEvent::OpenPath {
                panel_id,
                path,
                line,
                col,
                ssh_config,
                terminal_cwd,
            } => {
                // 判断路径是文件还是目录
                cx.emit(WorkspaceEvent::OpenPath {
                    path: path.clone(),
                    line: *line,
                    col: *col,
                    ssh_config: ssh_config.as_ref().map(|c| Box::new(c.clone())),
                    terminal_cwd: terminal_cwd.clone(),
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
            PanelEvent::SplitRight => {
                self.split_selected_panel(PanelPlacement::Right, window, cx);
            }
            PanelEvent::SplitDown => {
                self.split_selected_panel(PanelPlacement::Bottom, window, cx);
            }
            PanelEvent::ToggleZoom => {
                self.toggle_selected_panel_zoom(cx);
            }
            PanelEvent::TabLabelChanged {
                panel_id,
                label,
                program,
                title,
                cwd,
            } => {
                let _ = (label, program, title, cwd);
                // 不再区分标签类型：一个标签显示什么，**完全由它里面的 panel
                // 决定**（选中 panel 的程序名 + 目录，以及各 panel 的图标叠列）。
                // 以前这里把 Workspace 标签钉死成 "WorkSpace"，于是多个终端
                // 合成一格后就看不出它在跑什么了。
                if let Some(tab_id) = self.tabs.iter().find_map(|(tab_id, tab_info)| {
                    tab_info
                        .container
                        .contains_panel(panel_id, cx)
                        .then(|| tab_id.clone())
                }) {
                    self.refresh_tab_visual(&tab_id, cx);
                    cx.emit(WorkspaceEvent::SessionChanged);
                    cx.emit(WorkspaceEvent::TabBarChanged);
                }
            }
            PanelEvent::SessionChanged => {
                cx.emit(WorkspaceEvent::SessionChanged);
            }
            PanelEvent::BellRang { panel_id } => {
                // 后台铃 / 通知：点亮所属 tab 的红点。当前活动 tab 不点（用户正在看）。
                if let Some((tab_id, _)) = self.tabs.iter().find(|(tab_id, tab_info)| {
                    tab_info.container.contains_panel(panel_id, cx)
                        && **tab_id != self.active_tab_id
                }) {
                    let tab_id = tab_id.clone();
                    self.tab_bar.update(cx, |tab_bar, cx| {
                        tab_bar.modify_tab(&tab_id, |tab| tab.needs_attention = true, cx);
                    });
                }
            }
            PanelEvent::AgentStateChanged {
                panel_id,
                agent,
                status,
                streaming,
                program,
            } => {
                // 找到包含该 panel 的 tab，更新其 agent 图标 / 状态点。
                let Some(tab_id) = self.tabs.iter().find_map(|(tab_id, tab_info)| {
                    tab_info
                        .container
                        .contains_panel(panel_id, cx)
                        .then_some(tab_id.clone())
                }) else {
                    return;
                };

                // agent 消失时要把图标交还给原来的 OS / Workspace 图标，
                // 否则 tab 会永远留着一个已经退出的 agent 的图形。
                let fallback = self.fallback_tab_icon(&tab_id, panel_id, cx);
                let is_active = tab_id == self.active_tab_id;

                let previous = self.tab_bar.update(cx, |tab_bar, cx| {
                    tab_bar.set_tab_agent(
                        &tab_id,
                        TabAgentSnapshot {
                            agent: *agent,
                            status: *status,
                            streaming: *streaming,
                            fallback,
                            program: Some(program.clone()),
                        },
                        cx,
                    )
                });

                // 「需关注」红点只认 Working → Done 这个**边沿**，而不是
                // 「现在是 Done」：后者会让每次 Stop 都重复点亮、也会在用户
                // 切回 tab 清除红点后又被残留的 Done 立刻点亮。
                if should_mark_unread(previous, *status, is_active) {
                    self.tab_bar.update(cx, |tab_bar, cx| {
                        tab_bar.modify_tab(&tab_id, |tab| tab.needs_attention = true, cx);
                    });
                }

                // agent 上下线会改变这个 panel 的前台程序（claude → shell），
                // 叠列里对应的方块必须跟着换。
                self.refresh_tab_visual(&tab_id, cx);

                cx.emit(WorkspaceEvent::TabBarChanged);
            }
        }
    }

    /// agent 退出后 tab 图标应该回落到什么。
    ///
    /// 现在标签图标由「各 panel 前台程序」叠放决定（`refresh_tab_visual`），
    /// 不再有 Workspace / 发行版那一套兜底：agent 退出后前台就是 shell，
    /// 程序表会把方块画成 shell 的终端图形。因此这里不再提供回落图标。
    fn fallback_tab_icon(&self, _tab_id: &str, _panel_id: &str, _cx: &App) -> Option<Icon> {
        None
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
            log::debug!("Updating broadcast state for {} panels", panels.len());
            for panel in panels {
                let panel_id = panel.read(cx).panel_id.to_string();
                panel.update(cx, |p, cx| {
                    log::debug!(
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
        msg: BroadcastMsg,
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
        match msg {
            BroadcastMsg::Raw(data) => {
                for panel in target_panels {
                    panel.update(cx, |panel_ref, _cx| {
                        if let PanelContent::Terminal(terminal_view) = panel_ref.content() {
                            terminal_view.update(_cx, |tv, cx| {
                                tv.terminal().update(cx, |term, _| {
                                    term.write(&data);
                                });
                            });
                        }
                    });
                }
            }
            BroadcastMsg::BuiltinSync {
                text,
                cursor,
                dismissed,
            } => {
                for panel in target_panels {
                    panel.update(cx, |panel_ref, _cx| {
                        if let PanelContent::Terminal(terminal_view) = panel_ref.content() {
                            terminal_view.update(_cx, |tv, cx| {
                                // 镜像发起面板的内建命令输入（文本 + 光标 + 浮层折叠）
                                tv.apply_broadcast_builtin_input(&text, cursor, dismissed, cx);
                            });
                        }
                    });
                }
            }
            BroadcastMsg::BuiltinEnd => {
                for panel in target_panels {
                    panel.update(cx, |panel_ref, _cx| {
                        if let PanelContent::Terminal(terminal_view) = panel_ref.content() {
                            terminal_view.update(_cx, |tv, cx| {
                                // 同步清空镜像的内建输入（Ctrl+C 取消 / 提交清理）
                                tv.apply_broadcast_builtin_end(cx);
                            });
                        }
                    });
                }
            }
            BroadcastMsg::BuiltinCommand(command_line) => {
                for panel in target_panels {
                    panel.update(cx, |panel_ref, _cx| {
                        if let PanelContent::Terminal(terminal_view) = panel_ref.content() {
                            terminal_view.update(_cx, |tv, cx| {
                                // 各目标面板用自己的 cwd / ssh 上下文静默执行，
                                // 不弹 Toast / 冲突对话框（UI 副作用仅发生在源面板）；
                                // /scp 等全局副作用在接收面板会被抑制，只由源面板执行一次
                                tv.execute_broadcast_command(&command_line, cx);
                            });
                        }
                    });
                }
            }
        }
    }

    /// 处理广播输入队列中的数据
    /// 这个方法应该在适当的时机（如render时）调用
    pub fn process_broadcast_queue(&mut self, cx: &mut Context<Self>) {
        // 获取当前活动tab的广播状态
        if !self.get_current_broadcast_state() {
            return;
        }

        // 取出属于当前 Workspace 的输入，并保留其它 Workspace 的输入。
        //
        // 不能用 drain(..) 抽干整个全局队列：多 WorkSpace（多窗口）场景下，
        // 先 render 的 Workspace 会把其它 Workspace 的广播消息一并丢弃，导致丢失。
        // 使用 take+partition 避免 retain 在持锁期间 clone，并减少锁持有时间。
        let inputs: Vec<(String, BroadcastMsg)> = {
            let mut queue = get_broadcast_queue().lock();
            if queue.is_empty() {
                return;
            }
            let all = std::mem::take(&mut *queue);
            let mut taken = Vec::new();
            let mut remaining = Vec::with_capacity(all.len().saturating_sub(1));
            let workspace_id = self.workspace_id.as_str();
            for (ws_id, panel_id, msg) in all {
                if ws_id == workspace_id {
                    taken.push((panel_id, msg));
                } else {
                    remaining.push((ws_id, panel_id, msg));
                }
            }
            *queue = remaining;
            taken
        };

        // 处理每个输入
        for (panel_id, msg) in inputs {
            self.handle_broadcast_input(&panel_id, msg, cx);
        }
    }

    /// 添加广播消息到队列（由Panel的广播回调投递，Workspace 在 render 时统一消费）。
    ///
    /// 消息携带 workspace_id / panel_id：前者用于多 WorkSpace 场景下过滤，
    /// 后者用于分发时排除发起面板自身。
    pub fn queue_broadcast_msg(workspace_id: &str, panel_id: &str, msg: BroadcastMsg) {
        let mut queue = get_broadcast_queue().lock();
        queue.push((workspace_id.to_string(), panel_id.to_string(), msg));
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
            // 标签的标题与"哪个图标在最上层"都跟着选中 panel 变。
            let tab_id = self.active_tab_id.clone();
            self.refresh_tab_visual(&tab_id, cx);
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

    /// 处理关闭panel事件（带确认拦截）
    fn handle_panel_close(&mut self, panel_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 关闭确认：该终端有存活前台进程时先弹窗
        if self.panel_needs_confirmation(panel_id, cx) {
            self.pending_close = Some(PendingClose::Panel(panel_id.to_string()));
            self.close_confirm.update(cx, |state, cx| state.open(cx));
            cx.notify();
            return;
        }
        self.do_panel_close(panel_id, window, cx);
    }

    /// 取出指定视图的全部 Kitty 纹理并逐个 `drop_image`。
    ///
    /// 视图关闭后不再绘制，其 `ImageCache` 随视图释放，但 GPU 图集 tile 必须
    /// 显式释放（见 `ImageCache` 文档），否则残留到窗口关闭。undo 恢复的是
    /// 同一视图实体：缓存已清空，重绘按需重建，无副作用。
    fn drop_view_kitty_textures(view: &Entity<TerminalView>, window: &mut Window, cx: &mut App) {
        let textures = view.update(cx, |view, _| view.take_kitty_textures());
        if !textures.is_empty() {
            log::debug!("释放已关闭终端的 Kitty 纹理 {} 张", textures.len());
        }
        for texture in textures {
            if let Err(err) = window.drop_image(texture) {
                log::warn!("释放已关闭终端的 Kitty 纹理失败: {err}");
            }
        }
    }

    /// 释放在关闭前指定 panel 里终端视图缓存的 Kitty GPU 纹理。
    fn drop_panel_kitty_textures(&self, panel_id: &str, window: &mut Window, cx: &mut App) {
        let view = self.tabs.values().find_map(|tab_info| {
            tab_info
                .container
                .find_panel(panel_id, cx)?
                .read(cx)
                .content()
                .terminal_view()
                .cloned()
        });
        if let Some(view) = view {
            Self::drop_view_kitty_textures(&view, window, cx);
        }
    }

    /// 释放关闭前某 tab 内所有终端视图的 Kitty GPU 纹理。
    fn drop_tab_kitty_textures(&self, tab_id: &str, window: &mut Window, cx: &mut App) {
        let Some(tab_info) = self.tabs.get(tab_id) else {
            return;
        };
        for panel in tab_info.container.collect_panels_vec() {
            // 先把 `read` 借用结束（`cloned` 出 owned 实体），再 `update`，
            // 否则两个可变借用打架。
            let view = panel.read(cx).content().terminal_view().cloned();
            if let Some(view) = view {
                Self::drop_view_kitty_textures(&view, window, cx);
            }
        }
    }

    /// 真正关闭 panel（已过确认）：从其所在 tab 的容器中移除。
    fn do_panel_close(&mut self, panel_id: &str, window: &mut Window, cx: &mut Context<Self>) {
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
            // （走 do_tab_close：入 undo 栈 + 正确的窗口收尾）
            self.do_tab_close(&target_tab_id, window, cx);
            return;
        }

        // 先释放该 panel 的 GPU 纹理（只动关闭的这一个，同 tab 其他 panel 不受影响；
        // 单 panel 的情况走下面的 do_tab_close，那边会整 tab 释放）。
        self.drop_panel_kitty_textures(panel_id, window, cx);

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
            // 被移除的可能是Workspace标签：剩余Workspace标签重新编号
            self.refresh_all_tab_visuals(cx);
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
                .and_then(|p| p.read(cx).remote_os)
                .map(|os| os.icon());

            // 更新TabBar显示
            let label = tab_info
                .container
                .get_first_panel()
                .and_then(|p| p.read(cx).tab_label().map(|s| s.to_string()))
                .unwrap_or_else(|| tab_info.label.clone());
            tab_info.label = label.clone();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.modify_tab(
                    &target_tab_id,
                    |tab| {
                        tab.label = label.clone().into();
                        tab.icon = remaining_icon;
                    },
                    cx,
                );
            });
            // 该tab降级为普通标签：剩余Workspace标签重新编号
            self.refresh_all_tab_visuals(cx);
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

    /// 把当前选中的面板向右 / 向下分屏，新面板里起一个本地终端。
    ///
    /// 对应 herdr 面板标题栏的两个按钮（`pane.split direction=right|down`）。
    /// 新面板继承源面板的工作目录——在 `src/` 里按分屏，多半就是想在那儿
    /// 再开一个 shell，从 `~` 重新 cd 一遍是纯损耗。
    pub fn split_selected_panel(
        &mut self,
        placement: PanelPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_id = self.active_tab_id.clone();
        let Some(tab_info) = self.tabs.get(&tab_id) else {
            return;
        };
        // 目标：选中的那个，没有就第一个。
        let Some(target) = tab_info
            .container
            .get_selected_panel(cx)
            .or_else(|| tab_info.container.get_first_panel())
        else {
            return;
        };
        let target_id = target.read(cx).panel_id.to_string();

        // 新面板的工作目录跟源面板走。
        let cwd = target.read(cx).content().terminal_view().and_then(|view| {
            match view.read(cx).session_state(cx) {
                TerminalSessionState::Local { cwd, .. } => cwd.map(std::path::PathBuf::from),
                _ => None,
            }
        });

        let workspace_id = self.workspace_id.clone();
        let panel_id = format!("panel-{}", self.next_panel_id);
        let new_panel = cx.new(|cx| {
            Panel::with_terminal_working_directory(
                panel_id.clone(),
                workspace_id,
                Some(format!("tab-{}", self.next_tab_id).into()),
                None,
                cwd,
                cx,
            )
        });
        self.next_panel_id += 1;

        let workspace_id = self.workspace_id.clone();
        new_panel.update(cx, |p, cx| {
            p.set_workspace_id(workspace_id);
            p.set_broadcast_enabled(false, cx);
        });

        // 分屏后标签里就多于一个面板了。
        let Some(tab_info) = self.tabs.get_mut(&tab_id) else {
            return;
        };
        tab_info.container = tab_info.container.split_at(
            &target_id,
            placement,
            SplitContainer::Panel(new_panel.clone()),
            cx,
        );
        if tab_info.tab_type != TabType::Workspace {
            tab_info.tab_type = TabType::Workspace;
        }

        let subscription = cx.subscribe_in(
            &new_panel,
            window,
            |this, _, event: &PanelEvent, window, cx| {
                this.handle_panel_event(event, window, cx);
            },
        );
        self.panel_subscriptions
            .insert(new_panel.read(cx).panel_id.to_string(), subscription);

        // 新面板接手选中状态与焦点。
        self.handle_panel_selected(&panel_id, cx);
        if let Some(handle) = new_panel.read(cx).active_content_focus_handle(cx) {
            window.focus(&handle, cx);
        }
        self.refresh_tab_visual(&tab_id, cx);
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
        cx.notify();
    }

    /// 放大 / 缩回当前面板（herdr 的 `pane.zoom`）。
    ///
    /// 缩放只改渲染，不动 `SplitContainer` 的分屏结构与尺寸比例：缩回时
    /// 布局必须精确复原，重建容器做不到这一点。
    pub fn toggle_selected_panel_zoom(&mut self, cx: &mut Context<Self>) {
        let tab_id = self.active_tab_id.clone();
        let Some(tab_info) = self.tabs.get_mut(&tab_id) else {
            return;
        };
        // 只有一个面板时缩放没有意义（放大前后长得一样）。
        if tab_info.container.panel_count() < 2 {
            return;
        }
        let already_zoomed = tab_info.zoomed_panel.is_some();
        tab_info.zoomed_panel = if already_zoomed {
            None
        } else {
            tab_info
                .container
                .get_selected_panel(cx)
                .or_else(|| tab_info.container.get_first_panel())
                .map(|panel| panel.read(cx).panel_id.to_string())
        };
        cx.notify();
    }

    /// 当前面板是否处于放大状态（面板标题栏据此切换 zoom / unzoom 图标）。
    pub fn is_panel_zoomed(&self, panel_id: &str, cx: &App) -> bool {
        let _ = cx;
        self.tabs
            .get(&self.active_tab_id)
            .and_then(|info| info.zoomed_panel.as_deref())
            .is_some_and(|zoomed| zoomed == panel_id)
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

        // 合并后目标标签容纳多个 panel。它显示什么**完全由里面的 panel
        // 决定**（图标叠列 + 选中 panel 的标题），不再是「一个叫 WorkSpace
        // 的容器」——这是与 herdr 对齐的关键：标签说的是「这格在跑什么」。
        target_tab_info.container = new_container;
        target_tab_info.tab_type = TabType::Workspace;

        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.remove_tab(&dragged_tab_id, cx);
        });

        // 被拖拽的tab已移除、目标tab新容纳多个 panel：按各 panel 程序重算视觉
        self.refresh_all_tab_visuals(cx);

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

        // 面板变多了，但标签的外观仍由「里面的 panel 在跑什么」决定，
        // 不需要为它换成一个 Workspace 专用图标。
        if tab_info.tab_type != TabType::Workspace {
            tab_info.tab_type = TabType::Workspace;
            self.refresh_all_tab_visuals(cx);
        }

        // 保存被拖拽panel的ID和广播状态，用于后续设置
        let dragged_panel_id = dragged_panel.panel_id.clone();

        // 重新订阅所有panel（这会改变self）
        self.subscribe_to_panels(window, cx);

        // 设置被拖拽的panel的广播状态和workspace_id（与当前Workspace保持一致）
        let workspace_id = self.workspace_id.clone();
        if let Some(tab_info) = self.tabs.get(&active_tab_id)
            && let Some(panel) = tab_info.container.find_panel(&dragged_panel_id, cx)
        {
            panel.update(cx, |p, cx| {
                p.set_workspace_id(workspace_id.clone());
                p.set_broadcast_enabled(broadcast_enabled, cx);
            });
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
            // 被移除的可能是Workspace标签：剩余Workspace标签重新编号
            self.refresh_all_tab_visuals(cx);
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
                tab_bar.modify_tab(
                    &active_tab_id,
                    |tab| {
                        tab.icon = remaining_icon;
                    },
                    cx,
                );
            });
            // 该tab降级为普通标签：剩余Workspace标签重新编号
            self.refresh_all_tab_visuals(cx);
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
    ///
    /// TabBar 组件在触发回调前已就地应用重排（`to_index` 为重排前列表的
    /// 插入位语义），此处只需同步派生状态并广播变更。
    fn handle_tab_reorder(
        &mut self,
        _dragged_tab_id: &str,
        _to_index: usize,
        cx: &mut Context<Self>,
    ) {
        // 标签顺序变化：按新的显示顺序重新为Workspace标签编号
        self.refresh_all_tab_visuals(cx);
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
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
    /// 调整终端字号（cmd +/-）：clamp 到上下限后持久化 + 全窗口刷新。
    pub fn change_font_size(&mut self, delta: f32, cx: &mut App) {
        let current = crate::sidebar_view::font_settings::FontState::global(cx)
            .settings
            .terminal_font_size;
        let next = (current + delta).clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        // 距离过小（浮点噪声）时跳过，避免无效写入
        if (next - current).abs() < f32::EPSILON {
            return;
        }
        crate::sidebar_view::font_settings::FontState::update(cx, |settings| {
            settings.terminal_font_size = next;
        });
    }

    /// Tab 轮换：step=1 下一个，step=-1 上一个（wrap-around）。
    pub fn cycle_tab(&mut self, step: i32, cx: &mut Context<Self>) {
        let tabs = self.tab_bar.read(cx).tabs.clone();
        if tabs.len() < 2 {
            return;
        }
        let active = self.active_tab_id.clone();
        let idx = tabs.iter().position(|t| t.id == active).unwrap_or(0);
        let next = (idx as i32 + step).rem_euclid(tabs.len() as i32) as usize;
        let next_id = tabs[next].id.clone();
        self.switch_to_tab(&next_id, cx);
    }

    /// 直跳第 N 个 tab（0 基）。越界忽略。
    pub fn goto_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let tabs = self.tab_bar.read(cx).tabs.clone();
        if let Some(tab) = tabs.get(index) {
            let id = tab.id.clone();
            self.switch_to_tab(&id, cx);
        }
    }

    /// 分屏导航：从当前选中 panel 向指定方向找相邻 panel 并聚焦其终端。
    pub fn focus_split_direction(
        &mut self,
        nav: SplitNavDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(&self.active_tab_id) else {
            return;
        };
        let container = tab.container.clone();
        let Some(selected) = container.get_selected_panel(cx) else {
            return;
        };
        let panel_id = selected.read(cx).panel_id.clone();
        let Some(target) = container.focus_neighbor(&panel_id, nav, cx) else {
            return;
        };
        // 切选中并聚焦目标终端（复用选中事件流：取消其他 panel 选中 + 聚焦）
        let target_id = target.read(cx).panel_id.clone();
        self.handle_panel_selected(&target_id, cx);
        if let Some(handle) = target.read(cx).active_content_focus_handle(cx) {
            window.focus(&handle, cx);
        }
    }

    pub fn switch_to_tab(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if self.tabs.contains_key(tab_id) {
            // 切走的标签释放视口快照/行缓存（1–2 MB/标签）；再切回来时
            // content_dirty 会重建。只处理当前标签，避免每次切换遍历全部。
            let previous_tab = self.active_tab_id.clone();
            if previous_tab != tab_id
                && let Some(tab_info) = self.tabs.get(&previous_tab)
            {
                let views: Vec<_> = tab_info
                    .container
                    .collect_panels_vec()
                    .into_iter()
                    .filter_map(|panel| panel.read(cx).content().terminal_view().cloned())
                    .collect();
                let window_handle = cx.active_window();
                for view in views {
                    view.update(cx, |view, cx| {
                        view.release_background_memory(cx);
                    });
                    // GPU 纹理必须用 Window::drop_image 释放；CPU 图像仍留在
                    // 存储里，切回来时 paint 会按需重新上传。
                    if let Some(window_handle) = window_handle {
                        let _ = cx.update_window(window_handle, |_, window, cx| {
                            Self::drop_view_kitty_textures(&view, window, cx);
                        });
                    }
                }
            }
            self.active_tab_id = tab_id.to_string();
            let active_tab_id = self.active_tab_id.clone();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.set_active_tab(active_tab_id, cx);
            });

            // 切回来的标签可能**一个面板都没被标为选中**（切走时不会清、但
            // 新建/合并/恢复路径可能留下这种状态）。此时标签标题会取第一个
            // 面板、叠放也没有「谁在最上层」，与用户看到的焦点不一致。
            // 这里兜一次底：已有选中就尊重它，没有就选第一个面板。
            let has_selection = self
                .tabs
                .get(tab_id)
                .is_some_and(|info| info.container.get_selected_panel(cx).is_some());
            if !has_selection
                && let Some(first) = self
                    .tabs
                    .get(tab_id)
                    .and_then(|info| info.container.get_first_panel())
            {
                let first_id = first.read(cx).panel_id.to_string();
                self.handle_panel_selected(&first_id, cx);
            }
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
        let workspace_id = self.workspace_id.clone();
        let panel = cx.new(|cx| {
            Panel::with_terminal(
                format!("panel-{}", self.next_panel_id),
                workspace_id,
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
        self.create_local_terminal_with_command_and_working_directory(
            command, args, None, window, cx,
        );
    }

    /// 使用指定命令与起始目录创建本地终端Tab（复制 / 重连标签的起点继承）。
    ///
    /// `working_directory` 是宿主进程侧目录；WSL 会话的 Linux cwd 由
    /// [`TerminalView::new_with_command`] 翻译成 `wsl.exe --cd` 参数，
    /// 不会当 Windows 进程 cwd 传给 conpty。
    pub fn create_local_terminal_with_command_and_working_directory(
        &mut self,
        command: &str,
        args: &[String],
        working_directory: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let icon = if command.to_lowercase().contains("wsl") {
            Some(crate::ssh::platform::detect_wsl_os(command, args).icon())
        } else {
            Some(crate::ssh::platform::detect_local_os().icon())
        };
        let workspace_id = self.workspace_id.clone();
        let panel = cx.new(|cx| {
            Panel::with_terminal_command(
                format!("panel-{}", self.next_panel_id),
                workspace_id,
                Some(format!("tab-{}", self.next_tab_id).into()),
                None,
                command,
                args,
                working_directory,
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
        let workspace_id = self.workspace_id.clone();
        let panel = cx.new(|cx| {
            Panel::with_ssh_terminal(
                format!("panel-{}", self.next_panel_id),
                workspace_id,
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
        // 新标签的第一个面板要立即成为「选中面板」：否则 `is_selected`
        // 全是 false，`refresh_tab_visual` 既选不出标题的代表面板，也排不出
        // 叠放的最上层——表现为标题取错、切过去以后图标顺序才纠正。
        let first_panel_id = panel.read(cx).panel_id.to_string();
        self.handle_panel_selected(&first_panel_id, cx);
        // 立刻算一次标签视觉：新标签在第一帧就该有程序方块，而不是等到
        // 终端第一次吐出标题/PWD 才补上。
        //
        // herdr 那边不存在这个空窗：它的 `lead()` 永远能从一个已存在的
        // pane 取到 `program`。deepterm 的标签是独立实体，不主动填就是空的，
        // 于是「新建的标签没图标，点一下才出现」。
        self.refresh_tab_visual(&tab_id, cx);

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
        // 关闭确认：tab 内任一终端有存活前台进程时先弹窗，确认后再真正关闭。
        if self.tab_needs_confirmation(tab_id, cx) {
            self.pending_close = Some(PendingClose::Tab(tab_id.to_string()));
            self.close_confirm.update(cx, |state, cx| state.open(cx));
            cx.notify();
            return;
        }
        self.do_tab_close(tab_id, window, cx);
    }

    /// 真正关闭 tab（已过确认）：入 undo 栈 → 移除 → 切换/关窗。
    fn do_tab_close(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 先释放该 tab 内所有终端视图的 GPU 纹理（视图移除后不再绘制）。
        self.drop_tab_kitty_textures(tab_id, window, cx);
        // undo 栈：有其他 tab 可切回，且终端还在跑才值得恢复
        // （最后一个 tab 关闭即关窗，进程随窗口销毁；已 exit 的终端重开是死壳）
        let has_other_tabs = self.tabs.len() > 1;
        if has_other_tabs
            && self.tab_has_live_terminal(tab_id, cx)
            && let Some(tab_info) = self.tabs.get(tab_id)
        {
            let closed = ClosedTab {
                label: tab_info.label.clone(),
                tab_type: tab_info.tab_type.clone(),
                container: tab_info.container.clone(),
                tab_id: tab_id.to_string(),
                broadcast_enabled: tab_info.broadcast_enabled,
            };
            self.push_closed_tab(closed, cx);
        }

        self.tabs.remove(tab_id);
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.remove_tab(tab_id, cx);
        });
        // 剩余标签重算视觉（面板数量/选中项都可能受影响）
        self.refresh_all_tab_visuals(cx);

        // 如果关闭的是当前活动tab，切换到相邻 tab（以渲染顺序为准：优先下一个，否则上一个）
        if self.active_tab_id == tab_id {
            let next_tab_id = self.tab_bar.read(cx).active_tab_id().map(|s| s.to_string());
            if let Some(next_tab_id) = next_tab_id {
                self.active_tab_id = next_tab_id.clone();
                self.switch_to_tab(&next_tab_id, cx);
                self.focus_active_tab_panel(window, cx);
            } else {
                self.before_window_remove(cx);
                window.remove_window();
            }
        }

        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
        cx.notify();
    }

    /// tab 是否需要关闭确认（任一 panel 的终端有存活前台进程）
    fn tab_needs_confirmation(&self, tab_id: &str, cx: &App) -> bool {
        let Some(tab_info) = self.tabs.get(tab_id) else {
            return false;
        };
        tab_info.container.collect_panels_vec().iter().any(|panel| {
            panel
                .read(cx)
                .content()
                .terminal_view()
                .is_some_and(|view| view.read(cx).terminal().read(cx).needs_close_confirmation())
        })
    }

    /// tab 内是否存在仍在运行的终端。
    ///
    /// 执行 `exit` 正常退出的 tab 不该进 undo 栈——重开只会得到一个已经结束的
    /// 死终端（iTerm2 / ghostty 同样只对「被用户关掉的活终端」提供恢复）。
    fn tab_has_live_terminal(&self, tab_id: &str, cx: &App) -> bool {
        self.tabs
            .get(tab_id)
            .map(|tab_info| {
                tab_info.container.collect_panels_vec().iter().any(|panel| {
                    panel
                        .read(cx)
                        .content()
                        .terminal_view()
                        .is_some_and(|view| view.read(cx).terminal().read(cx).process_alive())
                })
            })
            .unwrap_or(false)
    }

    /// panel 是否需要关闭确认（其终端有存活前台进程）
    fn panel_needs_confirmation(&self, panel_id: &str, cx: &App) -> bool {
        self.tabs.values().any(|tab| {
            tab.container.find_panel(panel_id, cx).is_some_and(|panel| {
                panel
                    .read(cx)
                    .content()
                    .terminal_view()
                    .is_some_and(|view| {
                        view.read(cx).terminal().read(cx).needs_close_confirmation()
                    })
            })
        })
    }

    /// 压入 undo 栈；超过上限时逐出最旧的并 kill 其终端（释放进程与滚动区）。
    fn push_closed_tab(&mut self, closed: ClosedTab, cx: &mut Context<Self>) {
        if self.closed_tabs.len() >= MAX_CLOSED_TABS
            && let Some(oldest) = self.closed_tabs.pop_front()
        {
            for panel in oldest.container.collect_panels_vec() {
                if let Some(view) = panel.read(cx).content().terminal_view() {
                    let terminal = view.read(cx).terminal().clone();
                    terminal.update(cx, |terminal, _| terminal.kill());
                }
            }
        }
        self.closed_tabs.push_back(closed);
    }

    /// 确认关闭：执行挂起目标。
    fn confirm_pending_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_close.take() else {
            self.close_confirm.update(cx, |state, cx| state.close(cx));
            return;
        };
        self.close_confirm.update(cx, |state, cx| state.close(cx));
        match pending {
            PendingClose::Tab(tab_id) => self.do_tab_close(&tab_id, window, cx),
            PendingClose::Panel(panel_id) => self.do_panel_close(&panel_id, window, cx),
        }
    }

    /// 取消关闭：丢弃挂起目标（终端继续运行）。
    fn cancel_pending_close(&mut self, cx: &mut Context<Self>) {
        self.pending_close = None;
        self.close_confirm.update(cx, |state, cx| state.close(cx));
        cx.notify();
    }

    /// 重开最近关闭的 tab（undo）：进程与滚动区原样恢复。
    pub fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.closed_tabs.pop_back() else {
            return; // 栈空：无操作
        };

        let tab_id = closed.tab_id.clone();
        // 若 id 已被新 tab 占用（极端：重开后又建了同名 id —— id 自增，不会冲突，防御处理）
        if self.tabs.contains_key(&tab_id) {
            log::warn!("重开 tab 失败：id {} 已存在", tab_id);
            self.closed_tabs.push_back(closed);
            return;
        }

        let is_remote = closed
            .container
            .get_first_panel()
            .is_some_and(|p| p.read(cx).remote_os.is_some());

        let label = closed.label.clone();
        self.tabs.insert(
            tab_id.clone(),
            TabInfo {
                id: tab_id.clone(),
                label: closed.label.clone(),
                tab_type: closed.tab_type.clone(),
                container: closed.container.clone(),
                broadcast_enabled: closed.broadcast_enabled,
                zoomed_panel: None,
            },
        );
        self.tab_bar.update(cx, |tab_bar, cx| {
            let mut item = TabItem::new(tab_id.clone(), label.clone());
            item.is_remote = is_remote;
            tab_bar.add_tab(item, cx);
        });
        self.refresh_all_tab_visuals(cx);
        // 恢复 panel 事件订阅（关闭时被 retain 清掉）
        self.subscribe_to_panels(window, cx);
        self.switch_to_tab(&tab_id, cx);
        self.focus_active_tab_panel(window, cx);
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.emit(WorkspaceEvent::TabBarChanged);
        cx.notify();
    }

    pub fn terminal_view_for_tab(&self, tab_id: &str, cx: &App) -> Option<Entity<TerminalView>> {
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
        sidebar: Entity<crate::workspace::sidebar::MacosSidebar>,
        window: &mut gpui::Window,
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

        sidebar.update(cx, |s, cx| {
            s.set_terminal_file_tree(terminal_view, window, cx)
        });
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
    fn handle_tab_duplicate(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal_view) = self.terminal_view_for_tab(tab_id, cx) else {
            return;
        };

        let state = terminal_view.read(cx).session_state(cx);
        match &state {
            TerminalSessionState::Ssh { remote_cwd, .. } => {
                // 复制标签页继承代理选择：复制出来的标签连的是同一台海外主机，
                // 丢掉标记等于复制出一个必然不稳的直连会话。
                let config = state.ssh_config_with_proxy().expect("Ssh 分支必有配置");
                let workspace_id = self.workspace_id.clone();
                let panel = cx.new(|cx| {
                    Panel::with_ssh_terminal(
                        format!("panel-{}", self.next_panel_id),
                        workspace_id,
                        Some(format!("tab-{}", self.next_tab_id).into()),
                        Some(config.display_name().into()),
                        &config,
                        remote_cwd.clone(),
                        cx,
                    )
                });
                let label = Self::terminal_label_from_panel(&panel, cx);
                self.add_terminal_tab(label, panel, None, window, cx);
            }
            TerminalSessionState::Local { cwd, command, args } => {
                if let Some(cmd) = command {
                    // 起点继承：命令型本地标签（picker 里的 PowerShell/CMD/WSL）
                    // 此前丢掉了 cwd，复制出来固定在家目录启动。WSL 的 Linux
                    // cwd 不能当宿主进程 cwd，由该函数翻译成 `--cd` 参数。
                    self.create_local_terminal_with_command_and_working_directory(
                        cmd,
                        args,
                        cwd.as_ref().map(std::path::PathBuf::from),
                        window,
                        cx,
                    );
                } else {
                    let workspace_id = self.workspace_id.clone();
                    let panel = cx.new(|cx| {
                        Panel::with_terminal_working_directory(
                            format!("panel-{}", self.next_panel_id),
                            workspace_id,
                            Some(format!("tab-{}", self.next_tab_id).into()),
                            None,
                            cwd.clone().map(std::path::PathBuf::from),
                            cx,
                        )
                    });
                    let label = Self::terminal_label_from_panel(&panel, cx);
                    self.add_terminal_tab(label, panel, None, window, cx);
                }
            }
        }
    }

    /// 重新连接标签页 - WorkSpace 标签重连其下全部终端 panel，普通标签重连单个终端
    fn handle_tab_reconnect(&mut self, tab_id: &str, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_info) = self.tabs.get(tab_id) else {
            log::warn!("未找到标签页: {}", tab_id);
            return;
        };

        let mut panels: Vec<Entity<Panel>> = Vec::new();
        tab_info.container.collect_panels_into(&mut panels);

        if panels.is_empty() {
            log::warn!("标签页 {} 没有可重新连接的 panel", tab_id);
            return;
        }

        for panel in panels {
            panel.update(cx, |panel, cx| panel.reconnect_terminal(cx));
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
                        .flex_basis(gpui::DefiniteLength::Fraction(size))
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
pub(crate) struct TaskStatusReporter {
    pub(crate) status_message: Arc<Mutex<String>>,
    pub(crate) total_bytes: Arc<AtomicU64>,
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

crate::impl_focusable!(Workspace);

impl Drop for Workspace {
    /// 窗口销毁时清理全局广播队列中属于本 workspace 的残留消息：
    /// 队列以 workspace_id 过滤消费，Workspace 消失后其消息永远不会
    /// 被取出，长期多开多关窗口会缓慢积压。
    fn drop(&mut self) {
        let workspace_id = self.workspace_id.as_str();
        let mut queue = get_broadcast_queue().lock();
        queue.retain(|(ws_id, _, _)| ws_id != workspace_id);
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

        // 更新所有 panel 的 is_solo 与标题栏按钮可用性
        if let Some(container) = active_container {
            let panel_count = container.panel_count();
            let zoomed = self
                .tabs
                .get(&self.active_tab_id)
                .and_then(|info| info.zoomed_panel.clone());
            let panels = container.collect_panels_vec();
            for panel in panels {
                let is_zoomed = zoomed
                    .as_deref()
                    .is_some_and(|id| panel.read(cx).panel_id == id);
                let can_split = panel.read(cx).content().terminal_view().is_some();
                panel.update(cx, |p, cx| {
                    p.set_solo(panel_count == 1, cx);
                    // 放大按钮只在真有多块面板时出现——单面板放大无效果。
                    p.can_zoom = panel_count > 1;
                    p.can_split = can_split;
                    if p.is_zoomed != is_zoomed {
                        p.is_zoomed = is_zoomed;
                        cx.notify();
                    }
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
                        // 缩放：被放大的面板独占整个内容区，分屏结构不变
                        // （取消放大即原样回来）。herdr 的 `pane.zoom` 同义。
                        let zoomed = self
                            .tabs
                            .get(&self.active_tab_id)
                            .and_then(|info| info.zoomed_panel.clone())
                            .filter(|id| container.contains_panel(id, cx));
                        match zoomed {
                            Some(panel_id) => this.child(
                                container
                                    .find_panel(&panel_id, cx)
                                    .map(|panel| div().size_full().overflow_hidden().child(panel))
                                    .unwrap_or_else(|| {
                                        self.render_split_container(container, &[], cx)
                                    }),
                            ),
                            None => this.child(self.render_split_container(container, &[], cx)),
                        }
                    }),
            )
            .on_action(cx.listener(|this, _: &NewLocalTerminal, window, cx| {
                this.create_local_terminal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseSelectedPanel, window, cx| {
                this.close_selected_panel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split_selected_panel(PanelPlacement::Right, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split_selected_panel(PanelPlacement::Bottom, window, cx);
            }))
            .on_action(cx.listener(|this, _: &TogglePaneZoom, _window, cx| {
                this.toggle_selected_panel_zoom(cx);
            }))
            // 新 tab：cmd-t / ctrl-shift-t（直接新建本地终端 tab，同 tab 栏 + 按钮行为）
            .on_action(cx.listener(|this, _: &NewTerminalTab, window, cx| {
                this.create_local_terminal(window, cx);
            }))
            // 重开最近关闭的 tab：从 undo 栈恢复（无则忽略）
            .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                this.reopen_closed_tab(window, cx);
            }))
            // Tab 轮换 / cmd+N 直跳
            .on_action(cx.listener(|this, _: &NextTab, _window, cx| {
                this.cycle_tab(1, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, _window, cx| {
                this.cycle_tab(-1, cx);
            }))
            .on_action(cx.listener(|this, action: &GotoTab, _window, cx| {
                this.goto_tab(action.0, cx);
            }))
            // 分屏导航：结构遍历相邻 panel 后聚焦其终端
            .on_action(cx.listener(|this, _: &FocusSplitLeft, window, cx| {
                this.focus_split_direction(SplitNavDirection::Left, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSplitRight, window, cx| {
                this.focus_split_direction(SplitNavDirection::Right, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSplitUp, window, cx| {
                this.focus_split_direction(SplitNavDirection::Up, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSplitDown, window, cx| {
                this.focus_split_direction(SplitNavDirection::Down, window, cx);
            }))
            // 字号：经 FontState::update 持久化 + refresh_windows，
            // terminal element 下一帧自动用新字号重建行布局。
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _window, cx| {
                this.change_font_size(FONT_SIZE_STEP, cx);
            }))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, _window, cx| {
                this.change_font_size(-FONT_SIZE_STEP, cx);
            }))
            .on_action(cx.listener(|_this, _: &ResetFontSize, _window, cx| {
                let default_size =
                    crate::sidebar_view::font_settings::FontSettings::default().terminal_font_size;
                crate::sidebar_view::font_settings::FontState::update(cx, |settings| {
                    settings.terminal_font_size = default_size;
                });
            }))
            // 关闭确认弹窗（有存活前台进程时）
            .when(self.close_confirm.read(cx).is_rendered(), |this| {
                let state = self.close_confirm.read(cx);
                // Modal::on_close 是普通闭包（无 listener）：经 WeakEntity 回工作区取消挂起
                let workspace = cx.entity().downgrade();
                this.child(
                    Modal::new("workspace-close-confirm")
                        .closing(state.is_closing())
                        .epoch(state.epoch())
                        .width(px(420.0))
                        .on_close(move |_window, cx| {
                            if let Some(workspace) = workspace.upgrade() {
                                workspace.update(cx, |this, cx| {
                                    this.cancel_pending_close(cx);
                                });
                            }
                        })
                        .child(self.render_close_confirm_body(cx)),
                )
            })
    }
}

impl Workspace {
    /// 关闭确认弹窗内容：标题 + 正文 + 按钮。
    fn render_close_confirm_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let target_desc = match &self.pending_close {
            Some(PendingClose::Panel(_)) => "当前分屏面板",
            _ => "当前标签页",
        };
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("关闭终端？"),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .line_height(relative(1.5))
                            .child(format!(
                                "{}中仍有前台进程在运行，关闭后进程将被终止。",
                                target_desc
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .justify_end()
                    .child(
                        Button::new("close-confirm-cancel")
                            .label("取消")
                            .rounded(px(10.0))
                            .with_size(ButtonSize::Small)
                            .px(px(14.0))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.cancel_pending_close(cx);
                            })),
                    )
                    .child(
                        Button::new("close-confirm-ok")
                            .label("关闭")
                            .danger()
                            .rounded(px(10.0))
                            .with_size(ButtonSize::Small)
                            .px(px(14.0))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_pending_close(window, cx);
                            })),
                    ),
            )
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

    crate::impl_source_location_none!();

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
    ) {
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

/// 纯函数：这次 agent 状态变化该不该点亮 tab 的「需关注」红点。
///
/// 只认 `Working → Done` 这一个**边沿**，而不是「当前是 Done」：
///
/// - 只看状态的话，同一个 `Done` 会被重复上报（`Stop` 重发、状态机重放），
///   每次都给已经亮着的 tab 重新点亮，用户切回来清掉又立刻亮起。
/// - 边沿还能自然处理「用户没在看」这个前提：`is_active` 时说明用户正盯着
///   这个 tab，agent 说完话他直接就能看见，不需要红点。
///
/// `Waiting`（等授权 / 提问）**不**点亮红点：它是 agent 在等人，而人可能
/// 就坐在这个 tab 前面；状态点已经用空心圆环把这件事说清楚了。
///
/// `pub(crate)` 而非私有：它点亮的是 [`crate::workspace::tab_bar::TabItem::needs_attention`]，
/// 而那个标志**能否画出来**由 tab_bar 的 `should_show_attention_dot` 决定。
/// 两者曾经互相抵消（这边点亮、那边让位），各自却都有测试。tab_bar 的
/// `done_edge_flag_actually_renders_a_dot` 需要把它接起来做组合断言。
pub(crate) fn should_mark_unread(
    previous: Option<AgentStatus>,
    next: AgentStatus,
    is_active: bool,
) -> bool {
    !is_active && previous == Some(AgentStatus::Working) && next == AgentStatus::Done
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 红点只在「后台 tab 跑完一个回合」时点亮。
    #[test]
    fn unread_dot_only_on_working_to_done_edge() {
        use AgentStatus::*;

        // 唯一该点亮的组合：上一状态 Working、新状态 Done、且不是当前 tab。
        assert!(should_mark_unread(Some(Working), Done, false));

        // 当前 tab 不点：用户正看着，不需要红点提醒。
        assert!(!should_mark_unread(Some(Working), Done, true));

        // 非 Working → Done 的迁移都不点。
        assert!(!should_mark_unread(None, Done, false));
        assert!(!should_mark_unread(Some(Idle), Done, false));
        assert!(!should_mark_unread(Some(Waiting), Done, false));
        assert!(!should_mark_unread(Some(Done), Done, false));

        // Waiting 是「agent 在等人」，不是「干完了」，不点红点。
        assert!(!should_mark_unread(Some(Working), Waiting, false));
        assert!(!should_mark_unread(Some(Working), Working, false));
        assert!(!should_mark_unread(Some(Working), Idle, false));
    }
}
