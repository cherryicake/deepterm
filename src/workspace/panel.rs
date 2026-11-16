use gpui::prelude::*;
use gpui::{
    App, BoxShadow, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Render, SharedString, Styled, Window, div, point, px, svg,
};
use std::path::PathBuf;

use crate::agent::{AgentStatus, CLIAgent};
use crate::assets::Icon;
use crate::components::menu::{ContextMenuExt, PopupMenu};
use crate::file_manager::TransferRequest;
use crate::ssh::platform::RemoteOS;
use crate::terminal_view::terminal_view::{Event as TerminalViewEvent, TerminalView};
use crate::theme::ActiveTheme;
use crate::workspace::session_state::{
    PanelContentSessionState, PanelSessionState, TerminalSessionState,
};
use crate::workspace::tab_bar::{DragPreview, DraggedTab};

// Re-export for convenience
pub use crate::ssh::SshConnectionConfig;

// 面板右键菜单的动作。
//
// 用 Action 而不是直接闭包：`PopupMenu::menu` 的签名要求 `Box<dyn Action>`，
// 点击后统一走 `cx.dispatch_action`，与 deepterm 其它菜单（标签栏右键等）
// 的机制一致。
/// 拖拽的 Panel 数据
#[derive(Clone, Debug, PartialEq)]
pub struct DraggedPanel {
    pub panel_id: String,
    pub source_tab_id: String,
    pub tab_label: SharedString,
    pub icon: Option<Icon>,
    /// 拖起来那一刻的标签视觉（程序方块等）。
    ///
    /// 与 [`Self::icon`] 分开：`icon` 是 OS / Workspace 图标，只够画旧式标签；
    /// 拖回顶部标签栏时要长出与普通标签**一模一样**的插入预览，需要程序表
    /// 给的方块（品牌底色 + 图形）。在拖拽源处抓一次快照，比在落点处
    /// 反查 panel 更可靠——落点已经拿不到源 panel 的上下文了。
    pub visual: Option<crate::workspace::tab_bar::TrayTabVisual>,
}

impl DraggedPanel {
    /// 落成后这个标签会显示的程序方块。
    ///
    /// 有拖拽快照就用它（与顶部标签栏同一个来源）；纯文本面板没有快照，
    /// 退回 OS 图标 + 标签名，仍然保证插入预览有方块可画。
    pub fn tile_program(&self) -> crate::workspace::tab_bar::TileProgram {
        match &self.visual {
            Some(visual) => visual.program.clone(),
            None => crate::workspace::tab_bar::TileProgram {
                icon: self.icon.map(|icon| icon.path().to_string()),
                name: self.tab_label.to_string(),
                tile: None,
                color: None,
                tint: true,
            },
        }
    }
}

/// Panel placement threshold for drag operations (start at 35% of dimension)
const PANEL_DRAG_THRESHOLD_START: f32 = 0.35;
/// Panel placement threshold for drag operations (end at 65% of dimension)
const PANEL_DRAG_THRESHOLD_END: f32 = 0.65;

/// 面板放置位置
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelPlacement {
    Left,
    Right,
    Top,
    Bottom,
}

/// 面板事件
#[derive(Clone, Debug)]
pub enum PanelEvent {
    /// 拖拽预览变化
    DragPreview {
        panel_id: String,
        placement: Option<PanelPlacement>,
    },
    /// 放置事件（来自Tab拖拽）
    Drop {
        panel_id: String,
        dragged_tab: DraggedTab,
        placement: PanelPlacement,
    },
    /// Panel放置事件（来自Panel标题栏拖拽）
    PanelDrop {
        target_panel_id: String,
        dragged_panel: DraggedPanel,
        placement: PanelPlacement,
    },
    /// Tab 切换事件
    TabChanged {
        panel_id: String,
        tab_id: SharedString,
    },
    /// Tab 图标变更事件
    TabIconChanged {
        panel_id: String,
        os: Option<RemoteOS>,
        ssh_config_id: Option<String>,
    },
    /// 打开路径（文件→编辑器，目录→文件树 reveal；root 恒等于终端 cwd）
    OpenPath {
        panel_id: String,
        path: String,
        line: Option<u32>,
        col: Option<u32>,
        ssh_config: Option<SshConnectionConfig>,
        terminal_cwd: Option<String>,
    },
    /// 文件传输请求
    TransferRequested {
        panel_id: String,
        request: TransferRequest,
    },
    /// 关闭panel事件
    Close {
        panel_id: String,
    },
    /// 选中状态变更事件
    Selected {
        panel_id: String,
    },
    /// 广播状态切换事件
    BroadcastToggle {
        panel_id: String,
    },
    /// 面板标题栏的「向右分屏」（herdr 的 `pane.split direction=right`）。
    SplitRight,
    /// 面板标题栏的「向下分屏」（herdr 的 `pane.split direction=down`）。
    SplitDown,
    /// 面板标题栏的「放大 / 缩回」（herdr 的 `pane.zoom`）。
    ToggleZoom,

    SessionChanged,
    /// Tab 标签变更事件（由终端工作目录/前台进程变化触发）
    TabLabelChanged {
        panel_id: String,
        label: String,
        /// 同一时刻的前台程序快照。标签的「程序方块」用它上色/换图标，
        /// 与 `label` 同源（都由 `TerminalView::tab_label` 那套状态推出），
        /// 一起下发才不会出现「标题是 vim、图标还是 shell」的错位。
        program: crate::programs::Program,
        /// 标签的「名字段」（herdr 两段式标签的前半）。
        ///
        /// 与 `label` 分开：`label` 仍是拼好的 `进程: 目录` 全文，会话恢复、
        /// 侧边栏列表、拖拽预览都还按它显示；新标签栏画的是 `title` + `cwd`
        /// 两段。
        title: String,
        /// 标签的目录段（herdr 的两段式标签：名字 + 目录）。
        cwd: String,
    },
    /// 后台铃：所属 tab 点亮需关注红点，切到该 tab 后自动清除。
    BellRang {
        panel_id: String,
    },
    /// 前台 CLI agent 的身份 / 回合状态变化。
    ///
    /// 带载荷而不是让 workspace 自己去 `terminal.read(cx)`：`Panel` 已经持有
    /// `TerminalView`，在订阅回调里取一次快照最省事，也避免了 workspace 去
    /// 反查「哪个 panel 对应哪个 view」。
    AgentStateChanged {
        panel_id: String,
        agent: Option<CLIAgent>,
        status: AgentStatus,
        /// 此刻是否真的在输出（见 `TerminalView::agent_output_streaming`）。
        /// tab 的扫光只看这个，不看 `status`。
        streaming: bool,
        /// 前台程序快照（图标 / 方块色 / 名称），见 `TerminalView::foreground_program`。
        ///
        /// 与 agent 一起下发而不是让 workspace 回头去读：两者都来自同一次
        /// 前台探测，分开读会出现「图标是 vim、状态点还是 agent 的」这种
        /// 撕裂帧。
        program: crate::programs::Program,
    },
}

/// Panel content type
#[derive(Clone, Debug)]
pub enum PanelContent {
    /// Empty panel with just text
    Text(String),
    /// Terminal panel
    Terminal(Entity<TerminalView>),
}

impl PanelContent {
    pub fn terminal_view(&self) -> Option<&Entity<TerminalView>> {
        match self {
            PanelContent::Terminal(tv) => Some(tv),
            _ => None,
        }
    }
}

/// A panel within the workspace that displays terminal or other content.
///
/// Each panel has its own focus handle and can display either terminal content
/// or placeholder text. Panels support drag-and-drop operations for splitting
/// the workspace layout.
///
/// Note: The tab bar is managed by the Workspace, not individual panels.
/// 恢复会话时给终端用的**网格尺寸提示**。
///
/// # 为什么需要这个类型
///
/// `attach` 必须报出客户端的真实网格尺寸，守护进程才能先 resize 再生产
/// 快照（herdr-core `viewers::attach` 里 `apply_size()` 在 `first_paint()`
/// 之前，就是这个道理）。报错了，快照会按旧列数折行，TUI 布局跟着乱。
///
/// 但恢复会话的路径（`Workspace::from_session_state` → `SplitContainer`
/// → `Panel`）发生在**第一次布局之前**，那时还没有 `TerminalBounds`。
/// 能拿到的最准的东西是窗口尺寸，于是这里按默认字体度量把窗口换算成网格，
/// 作为 attach 时的初值；首帧布局后 `Terminal::resize` 会立刻用精确值
/// 校正并再通知一次守护进程。
///
/// 这比"读一个全局的最近尺寸"可靠：那个值可能是另一个窗口、或上一个
/// 会话留下的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalGridHint {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalGridHint {
    /// 默认字体度量（与 [`crate::terminal::terminal::TerminalBounds::default`]
    /// 一致：单元格 9×16 逻辑像素）。
    const CELL_WIDTH: f32 = 9.0;
    const LINE_HEIGHT: f32 = 16.0;
    /// 侧边栏 + 标签栏占掉的宽度/高度估算，见 [`Self::from_window_size`]。
    const CHROME_WIDTH: f32 = 260.0;
    const CHROME_HEIGHT: f32 = 90.0;

    /// 由窗口尺寸估算终端区域能放下多少格。
    ///
    /// 保守取值：宁可报小一点。报大了守护进程会按过宽的快照折行，客户端
    /// 首帧再缩窄——那一帧就是错位的；报小了只是首帧拉伸一下，内容不会
    /// 重组。
    pub fn from_window_size(width: f32, height: f32) -> Option<Self> {
        if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
            return None;
        }
        let cols = ((width - Self::CHROME_WIDTH) / Self::CELL_WIDTH).floor();
        let rows = ((height - Self::CHROME_HEIGHT) / Self::LINE_HEIGHT).floor();
        if cols < 2.0 || rows < 2.0 {
            return None;
        }
        Some(Self {
            cols: cols.min(f32::from(u16::MAX)) as u16,
            rows: rows.min(f32::from(u16::MAX)) as u16,
        })
    }
}

pub struct Panel {
    /// Unique identifier for this panel
    pub panel_id: String,
    /// Focus handle for keyboard input
    focus_handle: FocusHandle,
    /// Current drag preview position (if any)
    drag_preview: Option<PanelPlacement>,
    /// ID of the currently active tab in this panel
    active_tab_id: Option<SharedString>,
    /// Label for the active tab (shown in title bar)
    active_tab_label: Option<SharedString>,
    /// SSH config ID for saving detected OS info
    ssh_config_id: Option<String>,
    /// 检测到的远程操作系统
    pub remote_os: Option<RemoteOS>,
    /// ID of the workspace this panel belongs to
    pub workspace_id: String,
    /// Whether this is the only panel in the workspace (affects border rendering)
    pub is_solo: bool,
    /// Whether this panel is currently selected
    pub is_selected: bool,
    /// 鼠标是否悬停在 Panel 上（控制标题栏按钮显隐）
    is_hovered: bool,
    /// 本面板是否处于放大状态（由 Workspace 的 `zoomed_panel` 同步下来）。
    ///
    /// 存一份副本而不是每次去问 Workspace：标题栏渲染时手里只有 `Panel`，
    /// 反查要跨实体拿 `Workspace`，而这是个每帧都要读的布尔值。
    pub is_zoomed: bool,
    /// 所在标签的面板数 ≥2 时才允许放大（放大单面板等于什么都没做）。
    pub can_zoom: bool,
    /// 是否可以分屏（终端面板才行；纯文本占位面板没有终端可继承目录）。
    pub can_split: bool,
    /// Content displayed in this panel
    content: PanelContent,
    /// 广播状态 - 由Workspace同步给所有Panel
    pub broadcast_enabled: bool,
}

impl Panel {
    /// 创建 Panel 的通用字段初始化（避免每个 new_* 重复写 is_hovered 等）
    fn new_common(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        content: PanelContent,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            panel_id: panel_id.into(),
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label,
            ssh_config_id: None,
            remote_os: None,
            is_solo: false,
            is_selected: false,
            is_zoomed: false,
            can_zoom: false,
            can_split: false,
            is_hovered: false,
            content,
            broadcast_enabled: false,
        }
    }

    /// Creates a new panel with text content.
    ///
    /// # Arguments
    ///
    /// * `panel_id` - Unique identifier for this panel
    /// * `workspace_id` - ID of the workspace this panel belongs to
    /// * `active_tab_id` - ID of the currently active tab
    /// * `cx` - The context for entity creation
    pub fn new(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let content_text = format!("Panel: {}", panel_id);
        Self::new_common(
            panel_id,
            workspace_id,
            active_tab_id,
            None,
            PanelContent::Text(content_text),
            cx,
        )
    }

    /// Creates a new panel with a terminal view.
    ///
    /// # Arguments
    ///
    /// * `panel_id` - Unique identifier for this panel
    /// * `workspace_id` - ID of the workspace this panel belongs to
    /// * `active_tab_id` - ID of the currently active tab
    /// * `active_tab_label` - Label shown in the title bar
    /// * `cx` - The context for entity creation
    ///
    /// Subscribes to TerminalView close events and emits PanelEvent::Close when the terminal exits.
    fn subscribe_terminal_events(
        panel_id: &str,
        terminal_view: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let close_panel_id = panel_id.to_string();
        cx.subscribe(
            terminal_view,
            move |this, view, event: &TerminalViewEvent, cx| match event {
                TerminalViewEvent::CloseTerminal => {
                    cx.emit(PanelEvent::Close {
                        panel_id: close_panel_id.clone(),
                    });
                }
                TerminalViewEvent::ProcessExited(status) => {
                    // 子进程退出，但不关闭面板（除非用户执行 exit）
                    // 这里可以添加逻辑来显示退出状态或保持面板打开
                    log::info!("Panel {}: 子进程退出，状态码: {}", close_panel_id, status);
                }
                TerminalViewEvent::OpenPath {
                    path,
                    line,
                    col,
                    ssh_config,
                    terminal_cwd,
                } => {
                    cx.emit(PanelEvent::OpenPath {
                        panel_id: close_panel_id.clone(),
                        path: path.clone(),
                        line: *line,
                        col: *col,
                        ssh_config: ssh_config.clone(),
                        terminal_cwd: terminal_cwd.clone(),
                    });
                }
                TerminalViewEvent::StartTransfer { request } => {
                    cx.emit(PanelEvent::TransferRequested {
                        panel_id: close_panel_id.clone(),
                        request: request.clone(),
                    });
                }
                TerminalViewEvent::SshReconnect { config } => {
                    // SSH 连接断开，创建新的终端视图进行重连
                    log::info!("Panel {}: SSH 连接断开，创建重连视图", close_panel_id);
                    let remote_cwd = match view.read(cx).session_state(cx) {
                        TerminalSessionState::Ssh { remote_cwd, .. } => remote_cwd,
                        TerminalSessionState::Local { .. } => None,
                    };
                    // 主动终止被替换的旧终端（旧连接已断开，close 为幂等安全操作）
                    view.update(cx, |tv, inner_cx| tv.kill(inner_cx));
                    let new_terminal_view =
                        TerminalView::new_ssh_with_remote_cwd(cx, config, remote_cwd);
                    // 重新订阅新 TerminalView 的事件
                    Self::subscribe_terminal_events(&close_panel_id, &new_terminal_view, cx);
                    // 更新 panel 内容
                    this.set_content(PanelContent::Terminal(new_terminal_view), cx);
                    // 新视图没有广播回调，若广播仍开启需要重新装配
                    this.attach_broadcast_callback(cx);
                }
                TerminalViewEvent::RemoteOsDetected { os } => {
                    this.remote_os = Some(*os);
                    cx.emit(PanelEvent::TabIconChanged {
                        panel_id: close_panel_id.clone(),
                        os: Some(*os),
                        ssh_config_id: this.ssh_config_id.clone(),
                    });
                }
                TerminalViewEvent::WorkingDirectoryChanged | TerminalViewEvent::TitleChanged => {
                    let label = view.read(cx).tab_label(cx);
                    let (title, cwd) = view.read(cx).tab_title_parts(cx);
                    let program = view.read(cx).foreground_program(cx);
                    cx.emit(PanelEvent::TabLabelChanged {
                        panel_id: close_panel_id.clone(),
                        label,
                        program,
                        title,
                        cwd,
                    });
                    cx.notify(); // 触发 Panel 标题栏重绘
                    if matches!(event, TerminalViewEvent::WorkingDirectoryChanged) {
                        cx.emit(PanelEvent::SessionChanged);
                    }
                }
                TerminalViewEvent::BellRangWhileUnfocused => {
                    cx.emit(PanelEvent::BellRang {
                        panel_id: close_panel_id.clone(),
                    });
                }
                TerminalViewEvent::AgentChanged => {
                    // 现场取快照而不是让事件带载荷：`AgentChanged` 只表示
                    // 「变了」，具体变成什么以 terminal 当前状态为准。
                    let (agent, status) = view.read(cx).agent_tab_state(cx);
                    let streaming = view.read(cx).agent_output_streaming(cx);
                    let program = view.read(cx).foreground_program(cx);
                    cx.emit(PanelEvent::AgentStateChanged {
                        panel_id: close_panel_id.clone(),
                        agent,
                        status,
                        streaming,
                        program,
                    });
                }
            },
        )
        .detach();
    }

    /// 新建终端视图：**优先**放进守护进程，失败则退回进程内 PTY。
    ///
    /// 退回这一点很重要：守护进程起不来（只读 HOME、受限沙箱、旧二进制）
    /// 是可能发生的，而"持久化不可用"不该升级成"终端打不开"。用户最多
    /// 失去"关窗口不杀程序"这一条增强，其余行为完全不变。
    fn create_terminal_view(
        cx: &mut gpui::App,
        working_directory: Option<PathBuf>,
        command: Option<&str>,
        args: &[String],
    ) -> Entity<TerminalView> {
        // 新建终端时的初始网格。
        //
        // 这里以前硬编码 80×24，于是守护进程按 80 列建 PTY；首帧布局发现
        // 实际是 175 列，再 resize——全屏程序（nvim / htop）会先按 80 列
        // 画一遍、再按 175 列重排，用户看到的就是"刚打开时布局是乱的"。
        //
        // 优先用最近一次布局记下的真实网格；没有（首次启动、还没渲染过）
        // 才退回 80×24。
        let (cols, rows) = TerminalView::grid_size_hint()
            .or_else(|| TerminalView::grid_size_from_active_window(cx))
            .unwrap_or((80, 24));
        match TerminalView::new_persistent(cx, working_directory.clone(), command, args, cols, rows)
        {
            Ok(view) => view,
            Err(e) => {
                log::warn!("守护进程不可用（{e}），退回进程内终端");
                match command {
                    Some(command) => {
                        TerminalView::new_with_command(cx, command, args, working_directory)
                    }
                    None => TerminalView::new_with_working_directory(cx, working_directory),
                }
            }
        }
    }

    pub fn with_terminal(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let terminal_view = Self::create_terminal_view(cx, dirs::home_dir(), None, &[]);
        Self::subscribe_terminal_events(&panel_id, &terminal_view, cx);
        Self::new_common(
            panel_id,
            workspace_id,
            active_tab_id,
            active_tab_label,
            PanelContent::Terminal(terminal_view),
            cx,
        )
    }

    pub fn with_terminal_working_directory(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        working_directory: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let terminal_view = Self::create_terminal_view(cx, working_directory, None, &[]);
        Self::subscribe_terminal_events(&panel_id, &terminal_view, cx);
        Self::new_common(
            panel_id,
            workspace_id,
            active_tab_id,
            active_tab_label,
            PanelContent::Terminal(terminal_view),
            cx,
        )
    }

    /// Creates a new panel with a terminal view using a specific command.
    ///
    /// `working_directory` 是宿主侧起点目录（复制 / 重连标签继承来源标签的
    /// 当前目录）；WSL 的 Linux cwd 由 [`TerminalView::new_with_command`]
    /// 翻译成 `wsl.exe --cd` 参数。
    pub fn with_terminal_command(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        command: &str,
        args: &[String],
        working_directory: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let terminal_view = Self::create_terminal_view(cx, working_directory, Some(command), args);
        Self::subscribe_terminal_events(&panel_id, &terminal_view, cx);
        Self::new_common(
            panel_id,
            workspace_id,
            active_tab_id,
            active_tab_label,
            PanelContent::Terminal(terminal_view),
            cx,
        )
    }

    /// Creates a new panel with an SSH remote terminal view.
    pub fn with_ssh_terminal(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        ssh_config: &crate::ssh::SshConnectionConfig,
        remote_cwd: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        // 先试着放进守护进程：这样关掉 deepterm 时 SSH 连接不会断，远端的
        // 长任务继续跑。失败（守护进程不可用）退回进程内 SSH——持久化是
        // 增强，不能变成"连不上服务器"的新故障点。
        // 与本地终端同理：用最近一次布局的真实网格，避免先按 80 列建
        // 再重排。见 `create_terminal_view` 的说明。
        let (ssh_cols, ssh_rows) = TerminalView::grid_size_hint()
            .or_else(|| TerminalView::grid_size_from_active_window(cx))
            .unwrap_or((80, 24));
        let terminal_view = match TerminalView::new_persistent_ssh(
            cx,
            ssh_config,
            remote_cwd.clone(),
            ssh_cols,
            ssh_rows,
        ) {
            Ok(view) => view,
            Err(e) => {
                log::warn!("远程终端未交给守护进程（{e}），使用进程内 SSH");
                if let Some(cwd) = remote_cwd {
                    TerminalView::new_ssh_with_remote_cwd(cx, ssh_config, Some(cwd))
                } else {
                    TerminalView::new_ssh(cx, ssh_config)
                }
            }
        };
        Self::subscribe_terminal_events(&panel_id, &terminal_view, cx);
        Self {
            panel_id,
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label,
            ssh_config_id: Some(ssh_config.id()),
            remote_os: None,
            is_solo: false,
            is_selected: false,
            is_zoomed: false,
            can_zoom: false,
            can_split: false,
            is_hovered: false,
            content: PanelContent::Terminal(terminal_view),
            broadcast_enabled: false,
        }
    }

    /// 设置panel内容
    pub fn set_content(&mut self, content: PanelContent, _cx: &mut Context<Self>) {
        self.content = content;
    }

    /// 重新连接终端：SSH 会话重建连接，本地会话在当前工作目录重启 shell
    pub fn reconnect_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(terminal_view) = self.content.terminal_view().cloned() else {
            return;
        };

        let state = terminal_view.read(cx).session_state(cx);
        // 主动终止被替换的旧终端，避免进程/连接泄漏（旧实体随后被 set_content 释放）
        terminal_view.update(cx, |tv, inner_cx| tv.kill(inner_cx));
        let new_terminal_view = match &state {
            TerminalSessionState::Ssh { remote_cwd, .. } => {
                // 经 `ssh_config_with_proxy` 还原代理标记：重连必须与首次连接走
                // 同一条传输路径，否则海外主机重连就退回直连。
                let config = state.ssh_config_with_proxy().expect("Ssh 分支必有配置");
                log::info!(
                    "重新连接面板 {} 的 SSH 会话: {}",
                    self.panel_id,
                    config.host_label()
                );
                TerminalView::new_ssh_with_remote_cwd(cx, &config, remote_cwd.clone())
            }
            TerminalSessionState::Local {
                cwd, command, args, ..
            } => {
                log::info!("重新启动面板 {} 的本地会话", self.panel_id);
                if let Some(cmd) = command {
                    // 命令型标签（picker 选的 PowerShell/CMD/WSL）也要回到当前
                    // 目录：此前直接走 `new_with_command`，cwd 被丢在家目录。
                    TerminalView::new_with_command(cx, cmd, args, cwd.clone().map(PathBuf::from))
                } else {
                    TerminalView::new_with_working_directory(
                        cx,
                        cwd.clone().map(std::path::PathBuf::from),
                    )
                }
            }
        };

        Self::subscribe_terminal_events(&self.panel_id, &new_terminal_view, cx);
        self.set_content(PanelContent::Terminal(new_terminal_view), cx);
        // 新视图没有广播回调，若广播仍开启需要重新装配
        self.attach_broadcast_callback(cx);
        cx.notify();
    }

    /// Returns true if this panel contains a terminal view.
    pub fn has_terminal_content(&self) -> bool {
        matches!(self.content, PanelContent::Terminal(_))
    }

    pub fn is_remote(&self) -> bool {
        self.ssh_config_id.is_some()
    }

    /// Returns a reference to the panel's content.
    pub fn content(&self) -> &PanelContent {
        &self.content
    }

    pub fn session_state(&self, cx: &App) -> Option<PanelSessionState> {
        match &self.content {
            PanelContent::Terminal(terminal_view) => Some(PanelSessionState {
                id: self.panel_id.clone(),
                content: PanelContentSessionState::Terminal(
                    terminal_view.read(cx).session_state(cx),
                ),
            }),
            _ => None,
        }
    }

    pub fn from_session_state(
        state: &PanelSessionState,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        hint: Option<TerminalGridHint>,
        cx: &mut Context<Self>,
    ) -> Self {
        match &state.content {
            PanelContentSessionState::Terminal(TerminalSessionState::Local {
                cwd,
                command,
                args,
                daemon_address,
                daemon_grid,
            }) => {
                let working_directory = cwd.as_ref().map(PathBuf::from);

                // 先试着接回守护进程里的**同一个终端**。
                //
                // 这是"重启 deepterm 后立刻恢复退出前状态"的关键一步：接回
                // 意味着屏幕上还是断开前那一屏、程序还在原来那一步，用户
                // 不需要重新 `cd`、不会丢掉正在跑的任务。接不上（终端已退出、
                // 守护进程重启过）才退回下面的重建路径。
                if let Some(address) = daemon_address.as_deref()
                    && let Some(terminal_view) = TerminalView::attach_existing(
                        cx,
                        address,
                        working_directory.clone(),
                        command.as_deref(),
                        args,
                        daemon_grid
                            .map(|(c, r)| TerminalGridHint { cols: c, rows: r })
                            .or(hint),
                    )
                {
                    log::info!("已接回守护进程中的终端 {address}");
                    Self::subscribe_terminal_events(&state.id, &terminal_view, cx);
                    return Self::new_common(
                        state.id.clone(),
                        workspace_id,
                        active_tab_id,
                        active_tab_label,
                        PanelContent::Terminal(terminal_view),
                        cx,
                    );
                }
                if daemon_address.is_some() {
                    log::info!("终端已不在守护进程中，按会话重建面板 {}", state.id);
                }

                match command {
                    // 命令型标签（picker 里的 PowerShell / CMD / WSL）：恢复时
                    // 保持原命令与起点，丢掉它会把 CMD/WSL 标签换成默认 shell；
                    // WSL 更糟——Linux cwd 会被当 Windows 进程 cwd 交给 conpty，
                    // 直接起不来（见 `plan_local_session_startup`）。
                    Some(cmd) => Self::with_terminal_command(
                        state.id.clone(),
                        workspace_id,
                        active_tab_id,
                        active_tab_label,
                        cmd,
                        args,
                        working_directory,
                        cx,
                    ),
                    None => Self::with_terminal_working_directory(
                        state.id.clone(),
                        workspace_id,
                        active_tab_id,
                        active_tab_label,
                        working_directory,
                        cx,
                    ),
                }
            }
            PanelContentSessionState::Terminal(TerminalSessionState::Ssh {
                remote_cwd,
                daemon_address,
                ..
            }) => {
                let panel_id = state.id.clone();

                // 先试着接回同一个 SSH 会话：远端的长任务还在跑，重连会让你
                // 丢掉它（`htop` 退出、编译中断）。
                if let Some(address) = daemon_address.as_deref()
                    && let Some(config) = state.content.ssh_config_with_proxy()
                    && let Some(terminal_view) =
                        TerminalView::attach_existing(cx, address, None, None, &[], hint)
                {
                    log::info!("已接回守护进程中的远程终端 {address}");
                    terminal_view.update(cx, |this, _cx| {
                        this.adopt_ssh_session(config, remote_cwd.clone());
                    });
                    Self::subscribe_terminal_events(&panel_id, &terminal_view, cx);
                    let mut panel = Self::new_common(
                        panel_id,
                        workspace_id,
                        active_tab_id,
                        active_tab_label,
                        PanelContent::Terminal(terminal_view),
                        cx,
                    );
                    panel.ssh_config_id = Some(
                        state
                            .content
                            .ssh_config_with_proxy()
                            .map(|c| c.id())
                            .unwrap_or_default(),
                    );
                    return panel;
                }
                if daemon_address.is_some() {
                    log::info!("远程终端已不在守护进程中，重新连接 {}", state.id);
                }
                // 经 `ssh_config_with_proxy` 还原代理标记：`config` 上的
                // `use_system_proxy` 是 `serde(skip)` 的，标记存在会话状态的同级
                // 字段里，不合回去的话恢复出来的海外主机是直连。
                let config = state
                    .content
                    .ssh_config_with_proxy()
                    .expect("Ssh 分支必有配置");
                let terminal_view =
                    TerminalView::new_ssh_with_remote_cwd(cx, &config, remote_cwd.clone());
                Self::subscribe_terminal_events(&panel_id, &terminal_view, cx);
                let mut panel = Self::new_common(
                    panel_id,
                    workspace_id,
                    active_tab_id,
                    active_tab_label,
                    PanelContent::Terminal(terminal_view),
                    cx,
                );
                panel.ssh_config_id = Some(config.id());
                panel
            }
        }
    }

    /// 获取Tab标签名
    pub fn tab_label(&self) -> Option<&SharedString> {
        self.active_tab_label.as_ref()
    }

    /// 设置关联的 workspace_id
    pub fn set_workspace_id(&mut self, workspace_id: impl Into<String>) {
        self.workspace_id = workspace_id.into();
    }

    /// 设置是否为唯一的panel
    pub fn set_solo(&mut self, is_solo: bool, cx: &mut Context<Self>) {
        if self.is_solo == is_solo {
            return;
        }
        self.is_solo = is_solo;
        cx.notify();
    }

    /// 设置选中状态
    pub fn set_selected(&mut self, is_selected: bool, cx: &mut Context<Self>) {
        self.is_selected = is_selected;
        cx.notify();
    }

    /// 设置广播状态
    pub fn set_broadcast_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        // 状态未改变时跳过所有工作
        if self.broadcast_enabled == enabled {
            return;
        }

        self.broadcast_enabled = enabled;

        // 根据广播状态设置或清除TerminalView的广播回调
        if enabled {
            self.attach_broadcast_callback(cx);
        } else if let PanelContent::Terminal(terminal_view) = &self.content {
            terminal_view.update(cx, |tv, _cx| {
                tv.clear_broadcast_callback();
            });
        }

        cx.notify();
    }

    /// 为当前终端内容装配广播回调（把本面板输入投递到全局广播队列）。
    ///
    /// 在开启广播和终端视图被替换（SSH 重连 / 手动重连）后调用，
    /// 确保新 TerminalView 的输入仍会广播给组内其他面板。
    fn attach_broadcast_callback(&self, cx: &mut Context<Self>) {
        // 若本面板当前未开启广播则直接返回，不要装配回调。
        //
        // 重连（SSH 断开自动重连 / 手动 reconnect_terminal）会无条件调用本函数，
        // 若不守卫，关闭广播后重连仍会把回调重新挂上；而 process_broadcast_queue /
        // handle_broadcast_input 在广播关闭时直接 return，全局队列永远不会被消费，
        // 导致队列无限增长（内存泄漏），且日后开启广播会回放海量历史按键到组内其它面板。
        if !self.broadcast_enabled {
            return;
        }

        let Some(terminal_view) = self.content.terminal_view() else {
            return;
        };
        let panel_id = self.panel_id.clone();
        let workspace_id = self.workspace_id.clone();
        terminal_view.update(cx, |tv, _cx| {
            // 创建广播回调，当终端有输入时添加到全局队列，由Workspace的render方法处理；
            // 消息携带 workspace_id 以区分不同 WorkSpace 的广播
            tv.set_broadcast_callback(Box::new(move |msg| {
                crate::workspace::workspace::Workspace::queue_broadcast_msg(
                    &workspace_id,
                    &panel_id,
                    msg,
                );
            }));
        });
    }

    /// 获取活动 tab ID
    pub fn active_tab_id(&self) -> Option<&SharedString> {
        self.active_tab_id.as_ref()
    }

    /// 设置活动 tab
    pub fn set_active_tab(&mut self, tab_id: impl Into<SharedString>, cx: &mut Context<Self>) {
        let tab_id: SharedString = tab_id.into();
        let tab_id_for_event = tab_id.clone();
        self.active_tab_id = Some(tab_id);

        cx.emit(PanelEvent::TabChanged {
            panel_id: self.panel_id.clone(),
            tab_id: tab_id_for_event,
        });
        cx.notify();
    }

    /// 计算拖拽放置位置
    fn calculate_placement(
        &self,
        bounds: &gpui::Bounds<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
    ) -> Option<PanelPlacement> {
        // 使用 gpui-component 的方式：35%/65% 阈值
        if position.x < bounds.left() + bounds.size.width * PANEL_DRAG_THRESHOLD_START {
            Some(PanelPlacement::Left)
        } else if position.x > bounds.left() + bounds.size.width * PANEL_DRAG_THRESHOLD_END {
            Some(PanelPlacement::Right)
        } else if position.y < bounds.top() + bounds.size.height * PANEL_DRAG_THRESHOLD_START {
            Some(PanelPlacement::Top)
        } else if position.y > bounds.top() + bounds.size.height * PANEL_DRAG_THRESHOLD_END {
            Some(PanelPlacement::Bottom)
        } else {
            None
        }
    }
}

crate::impl_focusable!(Panel);

impl Panel {
    /// 获取内部 TerminalView 的 focus handle（如果有）
    pub fn content_focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match &self.content {
            PanelContent::Terminal(terminal_view) => Some(terminal_view.read(cx).focus_handle(cx)),
            _ => None,
        }
    }

    pub fn active_content_focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        self.content_focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for Panel {}

impl Panel {
    /// 生成 Panel 拖回 TabBar 后应该使用的标签。
    ///
    /// Panel 在 Workspace 容器里时，`active_tab_label` 会被同步成 Workspace 的名称，
    /// 这是为了标题栏表达“当前属于哪个 Workspace”。但拖回 TabBar 时，新标签必须
    /// 恢复成终端自身的名字，否则远程终端会丢失 Host 名称并显示成 WorkSpace[n]。
    fn drag_tab_label(&self, cx: &App) -> SharedString {
        if let PanelContent::Terminal(terminal_view) = &self.content {
            let terminal_view = terminal_view.read(cx);
            if let Some(config) = terminal_view.ssh_config() {
                return config
                    .nickname
                    .clone()
                    .unwrap_or_else(|| config.host_label())
                    .into();
            }

            let label = terminal_view.tab_label(cx);
            if !label.is_empty() && label != "Terminal" {
                return label.into();
            }

            // 本地终端内部默认标题可能只是 Terminal，按应用层语义恢复。
            return "Local Terminal".into();
        }

        self.active_tab_label
            .clone()
            .unwrap_or_else(|| "Panel".into())
    }

    /// 面板标题栏拖拽时跟随光标的视觉：与顶部标签栏同一套（herdr 托盘）。
    ///
    /// 只有终端面板有程序方块可言；纯文面板返回 `None`，调用方退回文字胶囊。
    ///
    /// 宽度固定取 `TAB_WIDTH_PX`：面板没有"在标签栏里被压缩"这回事，
    /// 它拖出去以后要落在标签栏或分屏区域，用标准标签宽度最好认。
    fn drag_tray_visual(&self, cx: &App) -> Option<crate::workspace::tab_bar::TrayTabVisual> {
        let view = self.content.terminal_view()?.read(cx);
        Some(
            crate::workspace::tab_bar::TrayTabVisual::from_terminal_view(
                view,
                self.is_selected,
                crate::workspace::tab_bar::TAB_WIDTH_PX.into(),
                format!("panel-{}", self.panel_id),
                cx,
            ),
        )
    }

    /// 面板标题栏的小图标按钮（herdr 的 `pane.button()`）。
    ///
    /// herdr 是 `p(3).rounded(4).text_color(t.dim)` + hover 时浮出底色、
    /// 图形换正常文字色；图形 14px。这里保持一致，只是底色换成本主题令牌。
    fn pane_button(
        id: String,
        icon: Icon,
        theme: crate::theme::UiTheme,
        on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        let group = format!("{}-grp", id);
        div()
            .id(id)
            .flex_none()
            .p(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_color(theme.icon_muted)
            // group：hover 时**图标本身**也要变亮。`text_color` 不会传给 svg，
            // 所以 svg 那侧用 `group_hover` 自己取色。
            .group(group.clone())
            .hover(move |style| style.bg(theme.title_bar_hover))
            .on_click(on_click)
            // `text_color` 必须**直接设在 svg 上**：gpui 的 svg 按 alpha mask
            // 渲染、用自身的前景色着色，不继承父容器的文字色。父层那句
            // `.text_color()` 只影响文字节点，svh 会沿用默认色——在被挤成
            // 14px 的图标上就是「看不见」。
            .child(
                svg()
                    .size(px(14.0))
                    .text_color(theme.icon_muted)
                    .group_hover(group, move |style| style.text_color(theme.text))
                    .path(icon.path()),
            )
    }

    /// 渲染标题栏
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        // 如果包含 TerminalView，使用其动态标签（实时反映 shell/cwd/进程）
        let title = self
            .content
            .terminal_view()
            .map(|tv| tv.read(cx).tab_label(cx))
            .or_else(|| self.active_tab_label.as_ref().map(|s| s.to_string()))
            .or_else(|| self.active_tab_id.as_ref().map(|s| s.to_string()))
            .unwrap_or_else(|| self.panel_id.clone());

        let panel_id = self.panel_id.clone();
        let panel_id_for_broadcast = panel_id.clone();
        let panel_id_for_close = panel_id.clone();
        let source_tab_id = self
            .active_tab_id
            .clone()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let drag_title = self.drag_tab_label(cx).to_string();
        let broadcast_enabled = self.broadcast_enabled;
        // 面板拖起来时跟随光标的预览：与顶部标签栏同一个渲染器、同一份
        // 视觉来源（`TrayTabVisual::from_terminal_view`）。两个入口拖同一条
        // 终端，画面必须一致，否则用户会以为拖的是两样东西。
        let drag_visual = self.drag_tray_visual(cx);
        // 标题栏左侧的方块要在拖拽闭包之外先算好：闭包会把 `drag_visual`
        // move 进去（预览要用它）。
        let title_tiles = drag_visual
            .as_ref()
            .map(|visual| visual.tiles.clone())
            .unwrap_or_else(|| {
                vec![crate::workspace::tab_bar::TileProgram {
                    icon: self.remote_os.map(|os| os.icon().path().to_string()),
                    name: self
                        .active_tab_label
                        .as_ref()
                        .map(|label| label.to_string())
                        .unwrap_or_default(),
                    tile: None,
                    color: None,
                    tint: true,
                }]
            });

        div()
            .h(px(28.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .cursor_move()
            .bg(theme.app_background)
            .child(
                // 标题栏左侧主区域都可拖拽（排除右侧控制按钮区域）
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    // 图标与标题之间的间距（herdr 的 pane 标题栏是 `gap(8)`）。
                    // 原来这里写的是 `.mr(6)`，但它挂在整个容器上，并没有
                    // 落在图标与文字之间——两者贴在一起。
                    .gap(px(6.0))
                    .id(format!("panel-title-{}", panel_id))
                    .on_drag(
                        DraggedPanel {
                            panel_id: panel_id.clone(),
                            source_tab_id,
                            tab_label: drag_title.clone().into(),
                            icon: self.remote_os.map(|os| os.icon()),
                            visual: drag_visual.clone(),
                        },
                        move |_drag_data, position, window, cx| {
                            // 阻止事件冒泡，避免被上层拖拽逻辑打断导致预览不同步
                            cx.stop_propagation();
                            match drag_visual.clone() {
                                // 终端面板：与顶部标签栏同一套托盘视觉。
                                Some(visual) => {
                                    let width = visual.width;
                                    cx.new(move |_cx| DragPreview::tab(visual, width, position))
                                }
                                // 没有终端（纯文本面板）：退回旧的一行文字胶囊。
                                None => {
                                    let preview = DragPreview::new(
                                        drag_title.clone().into(),
                                        position,
                                        window,
                                        cx,
                                    );
                                    cx.new(move |_cx| preview)
                                }
                            }
                        },
                    )
                    // 标题栏左侧的程序图标：与顶部标签**同源但不同风格**。
                    //
                    // herdr 顶部的 `tile()` 是品牌色实心方块（导航要醒目），
                    // 面板标题栏的 `icon(program, 14, t.dim)` 只画单色图形、
                    // 取主题弱化色（正在看的那一个不必再抢注意力）。这里照做。
                    .child(crate::workspace::tab_bar::TabBar::render_pane_header_icon(
                        &title_tiles,
                        theme.text_muted,
                    ))
                    .child(div().text_xs().text_color(theme.text_muted).child(title)),
            )
            .when(self.is_hovered, |this| {
                let zoomed = self.is_zoomed;
                let can_split = self.can_split;
                this.child(
                    // 右侧按钮组：分屏 / 放大 / 广播 / 关闭
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        // herdr 的三个 pane 按钮：只在面板数 ≥2 时才有意义的是
                        // zoom；分屏在单面板时也允许（正是要裂开它）。
                        .when(can_split, |this| {
                            this.child(Self::pane_button(
                                format!("split-right-{}", panel_id),
                                Icon::SplitRight,
                                theme,
                                cx.listener(|_this, _: &gpui::ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    cx.emit(PanelEvent::SplitRight);
                                }),
                            ))
                        })
                        .when(can_split, |this| {
                            this.child(Self::pane_button(
                                format!("split-down-{}", panel_id),
                                Icon::SplitDown,
                                theme,
                                cx.listener(|_this, _: &gpui::ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    cx.emit(PanelEvent::SplitDown);
                                }),
                            ))
                        })
                        .when(self.can_zoom, |this| {
                            this.child(Self::pane_button(
                                format!("zoom-{}", panel_id),
                                if zoomed { Icon::Unzoom } else { Icon::Zoom },
                                theme,
                                cx.listener(move |_this, _: &gpui::ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    cx.emit(PanelEvent::ToggleZoom);
                                }),
                            ))
                        })
                        .child(
                            // 广播按钮 - 开启/关闭终端输入广播功能
                            div()
                                .id(format!("broadcast-btn-{}", panel_id))
                                .w_5()
                                .h_5()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .when(broadcast_enabled, |this| this.bg(theme.accent))
                                .when(!broadcast_enabled, |this| {
                                    this.hover(move |style| style.bg(theme.title_bar_hover))
                                })
                                .cursor_pointer()
                                .child(
                                    svg()
                                        .size(px(14.0))
                                        .text_color(if broadcast_enabled {
                                            theme.accent_foreground
                                        } else {
                                            theme.icon_muted
                                        })
                                        .path(Icon::BroadcastInput.path()),
                                )
                                .on_click(cx.listener(
                                    move |_this, _event: &gpui::ClickEvent, _window, cx| {
                                        cx.emit(PanelEvent::BroadcastToggle {
                                            panel_id: panel_id_for_broadcast.clone(),
                                        });
                                        cx.stop_propagation();
                                    },
                                )),
                        )
                        .child(
                            // 关闭按钮
                            div()
                                .id(format!("close-btn-{}", panel_id))
                                .w_5()
                                .h_5()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .hover(move |style| style.bg(theme.title_bar_hover))
                                .cursor_pointer()
                                .child(
                                    svg()
                                        .size(px(14.0))
                                        .text_color(theme.icon_muted)
                                        .path(Icon::WindowClose.path()),
                                )
                                .on_click(cx.listener(
                                    move |_this, _event: &gpui::ClickEvent, _window, cx| {
                                        cx.emit(PanelEvent::Close {
                                            panel_id: panel_id_for_close.clone(),
                                        });
                                        cx.stop_propagation();
                                    },
                                )),
                        ),
                )
            })
    }

    /// 渲染内容区域
    fn render_content(&self) -> impl IntoElement {
        match &self.content {
            PanelContent::Text(text) => {
                div().flex_1().flex().items_center().justify_center().child(
                    self.active_tab_id
                        .as_ref()
                        .map(|id| format!("{} | Tab: {}", text, id))
                        .unwrap_or_else(|| text.clone()),
                )
            }
            PanelContent::Terminal(terminal_view) => div().flex_1().child(terminal_view.clone()),
        }
    }

    /// 渲染拖拽预览层
    fn render_drag_preview(&self, cx: &App) -> Option<impl IntoElement> {
        let theme = cx.theme().ui;
        self.drag_preview.map(|preview| {
            div()
                .absolute()
                .bg(theme.drop_target)
                .opacity(0.95)
                .rounded(px(10.0))
                .map(|div| match preview {
                    PanelPlacement::Left => div
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(gpui::DefiniteLength::Fraction(0.5)),
                    PanelPlacement::Right => div
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(gpui::DefiniteLength::Fraction(0.5)),
                    PanelPlacement::Top => div
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(gpui::DefiniteLength::Fraction(0.5)),
                    PanelPlacement::Bottom => div
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(gpui::DefiniteLength::Fraction(0.5)),
                })
        })
    }

    /// 处理拖拽移动事件
    fn handle_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        let bounds = event.bounds;
        let position = event.event.position;

        // 检查鼠标是否还在内容区域内
        let is_inside = position.x >= bounds.left()
            && position.x <= bounds.right()
            && position.y >= bounds.top()
            && position.y <= bounds.bottom();

        if !is_inside {
            // 鼠标离开内容区域，清除预览
            if self.drag_preview.is_some() {
                self.drag_preview = None;
                cx.emit(PanelEvent::DragPreview {
                    panel_id: self.panel_id.clone(),
                    placement: None,
                });
                cx.notify();
            }
            return;
        }

        let preview = self.calculate_placement(&bounds, position);

        if preview != self.drag_preview {
            self.drag_preview = preview;

            // 发送事件给父组件
            cx.emit(PanelEvent::DragPreview {
                panel_id: self.panel_id.clone(),
                placement: preview,
            });

            cx.notify();
        }
    }

    /// 处理放置事件
    fn handle_drop(&mut self, dragged: &DraggedTab, cx: &mut Context<Self>) {
        // drag_preview == None 表示指针落在面板中心区（calculate_placement
        // 明确返回 None 的"无操作"区域），此时屏幕上没有任何预览提示。
        // 若默认按 Right 分屏，会出现"预览说无操作、松手却意外分屏"的
        // 状态不同步 —— 与视觉预览保持一致：中心区直接吞掉事件。
        let Some(placement) = self.drag_preview else {
            cx.notify();
            return;
        };

        // 发送放置事件
        cx.emit(PanelEvent::Drop {
            panel_id: self.panel_id.clone(),
            dragged_tab: dragged.clone(),
            placement,
        });

        self.drag_preview = None;
        cx.notify();
    }

    /// 处理Panel拖拽移动事件
    fn handle_panel_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedPanel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 获取拖拽数据
        let dragged: &DraggedPanel = event.drag(cx);

        // 不处理自己拖拽到自己
        if dragged.panel_id == self.panel_id {
            return;
        }

        let bounds = event.bounds;
        let position = event.event.position;

        // 检查鼠标是否还在内容区域内
        let is_inside = position.x >= bounds.left()
            && position.x <= bounds.right()
            && position.y >= bounds.top()
            && position.y <= bounds.bottom();

        if !is_inside {
            // 鼠标离开内容区域，清除预览
            if self.drag_preview.is_some() {
                self.drag_preview = None;
                cx.emit(PanelEvent::DragPreview {
                    panel_id: self.panel_id.clone(),
                    placement: None,
                });
                cx.notify();
            }
            return;
        }

        let preview = self.calculate_placement(&bounds, position);

        if preview != self.drag_preview {
            self.drag_preview = preview;

            // 发送事件给父组件
            cx.emit(PanelEvent::DragPreview {
                panel_id: self.panel_id.clone(),
                placement: preview,
            });

            cx.notify();
        }
    }

    /// 处理Panel放置事件
    fn handle_panel_drop(&mut self, dragged: &DraggedPanel, cx: &mut Context<Self>) {
        // 不处理自己拖拽到自己
        if dragged.panel_id == self.panel_id {
            return;
        }

        // 与 handle_drop 同一原则：中心区（drag_preview == None）无预览
        // 提示，不产生任何放置行为，避免预览与行为脱节。
        let Some(placement) = self.drag_preview else {
            cx.notify();
            return;
        };

        // 发送Panel放置事件
        cx.emit(PanelEvent::PanelDrop {
            target_panel_id: self.panel_id.clone(),
            dragged_panel: dragged.clone(),
            placement,
        });

        self.drag_preview = None;
        cx.notify();
    }
}

impl Render for Panel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_solo = self.is_solo;
        let is_selected = self.is_selected;

        div()
            .id(format!("panel-{}", self.panel_id))
            .size_full()
            .bg(theme.panel_background)
            .when(!is_solo, |this| {
                // 选中时边框颜色加深，未选中时让边框更接近背景色，仅微微可见
                let border_color = if is_selected {
                    theme.border_strong
                } else {
                    theme.border
                };
                this.border_1()
                    .border_color(border_color)
                    // 阴影 - 复刻 Hosts 卡片风格
                    .shadow(vec![BoxShadow {
                        color: theme.shadow.opacity(if cx.theme().appearance.is_dark() {
                            0.32
                        } else {
                            0.18
                        }),
                        offset: point(px(2.), px(4.)),
                        blur_radius: px(20.),
                        spread_radius: px(-4.),
                        inset: false,
                    }])
            })
            .rounded(px(12.0))
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if *hovered != this.is_hovered {
                    this.is_hovered = *hovered;
                    cx.notify();
                }
            }))
            .flex()
            .flex_col()
            .overflow_hidden()
            // 点击选中panel - 发送事件给Workspace统一管理
            .on_click(
                cx.listener(move |this, _event: &gpui::ClickEvent, _window, cx| {
                    if !this.is_selected {
                        cx.emit(PanelEvent::Selected {
                            panel_id: this.panel_id.clone(),
                        });
                    }
                }),
            )
            // 标题栏 - 只在非solo状态下显示（一组Panel时）
            .when(!is_solo, |this| this.child(self.render_title_bar(cx)))
            // 内容区域
            .child(self.render_content())
            // 渲染拖拽预览层
            .when(self.drag_preview.is_some(), |this| {
                if let Some(preview) = self.render_drag_preview(cx) {
                    this.child(preview)
                } else {
                    this
                }
            })
            // 拖拽检测 - DraggedTab
            .on_drag_move::<DraggedTab>(cx.listener(move |this, event, _window, cx| {
                this.handle_drag_move(event, cx);
            }))
            // 拖拽检测 - DraggedPanel
            .on_drag_move::<DraggedPanel>(cx.listener(move |this, event, window, cx| {
                this.handle_panel_drag_move(event, window, cx);
            }))
            // 放置处理 - DraggedTab
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _window, cx| {
                this.handle_drop(dragged, cx);
            }))
            // 放置处理 - DraggedPanel
            .on_drop(
                cx.listener(move |this, dragged: &DraggedPanel, _window, cx| {
                    this.handle_panel_drop(dragged, cx);
                }),
            )
            // 添加一个通用的on_drop来捕获任何类型的拖拽
            .on_drop(cx.listener(move |_this, _dragged: &(), _window, _cx| {
                // Generic drop handler
            }))
            // 右键即选中本面板。
            //
            // 菜单里的「关闭面板」走 `CloseSelectedPanel`（关的是**选中**
            // 那个），而右键完全可能落在非选中面板上。不先选中，就会出现
            // 「右键 A、关掉 B」。菜单打开前选中，语义才对得上。
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _event, _window, cx| {
                    if !this.is_selected {
                        cx.emit(PanelEvent::Selected {
                            panel_id: this.panel_id.clone(),
                        });
                    }
                }),
            )
            // 右键菜单：分屏 / 放大 / 关闭。
            //
            // 文案、图标、快捷键都照 herdr 的 pane 菜单（`mods/menu/main.js`
            // 的 `items()` + `menuIcon()`）：
            //   Split right / Split down / Zoom / Close pane
            // 图标分别是 panel-right / panel-right / square / x。
            //
            // 面板标题栏只在分屏（多面板）时才显示，所以单面板时这几个操作
            // 在界面上无处可点，菜单补上这个入口。
            //
            // 菜单项挂 **Workspace 级** action：菜单浮层是独立 entity，
            // 点选项走 `window.dispatch_action` 沿焦点链分发，挂在 Panel 上的
            // handler 收不到，只有 Workspace 接得住。
            .context_menu(format!("panel-menu-{}", self.panel_id), {
                let can_split = self.can_split;
                let can_zoom = self.can_zoom;
                let is_zoomed = self.is_zoomed;
                move |menu: PopupMenu, _window, _cx| {
                    use crate::components::button::Icon as MenuIcon;
                    use crate::workspace::workspace as ws;
                    // 快捷键提示必须与实际绑定一致：workspace.rs 里用的是
                    // `secondary-*`（macOS = ⌘，Linux/Windows = Ctrl）。
                    let mac = cfg!(target_os = "macos");
                    let (k_right, k_down, k_zoom, k_close) = if mac {
                        ("⌘D", "⇧⌘D", "⇧⌘↩", "⌘W")
                    } else {
                        ("Ctrl+D", "Ctrl+Shift+D", "Ctrl+Shift+Enter", "Ctrl+W")
                    };
                    // 只有终端面板能分屏（新面板要继承目录）。
                    let menu = if can_split {
                        menu.menu_with_shortcut(
                            "Split right",
                            MenuIcon::new("icons/panel-right.svg"),
                            k_right,
                            Box::new(ws::SplitRight),
                        )
                        .menu_with_shortcut(
                            "Split down",
                            MenuIcon::new("icons/panel-right.svg"),
                            k_down,
                            Box::new(ws::SplitDown),
                        )
                    } else {
                        menu
                    };
                    // 放大只在多面板时有意义。
                    let menu = if can_zoom {
                        menu.separator().menu_with_shortcut(
                            if is_zoomed { "Unzoom" } else { "Zoom" },
                            MenuIcon::new("icons/square.svg"),
                            k_zoom,
                            Box::new(ws::TogglePaneZoom),
                        )
                    } else {
                        menu
                    };
                    menu.separator().menu_with_shortcut(
                        "Close pane",
                        MenuIcon::new("icons/search-xmark.svg"),
                        k_close,
                        Box::new(ws::CloseSelectedPanel),
                    )
                }
            })
    }
}

#[cfg(test)]
mod grid_hint_tests {
    use super::TerminalGridHint;

    /// 窗口尺寸必须能换算成合理的网格。
    ///
    /// 回归价值：这个值会被 `attach` 报给守护进程，用来**先 resize 再生产
    /// 快照**。报错就会让快照按错列数折行，TUI 布局当场错位。所以至少
    /// 要保证：常见窗口尺寸下结果落在"合理的终端大小"区间里。
    #[test]
    fn a_typical_window_maps_to_a_sane_grid() {
        // 1400×900 是默认开窗尺寸。
        let hint = TerminalGridHint::from_window_size(1400.0, 900.0).expect("默认尺寸应能换算");
        assert!(
            (100..=130).contains(&hint.cols),
            "1400 宽应该约 120 列，实际 {}",
            hint.cols
        );
        assert!(
            (40..=60).contains(&hint.rows),
            "900 高应该约 50 行，实际 {}",
            hint.rows
        );
    }

    /// 全屏窗口要换算出一个明显更大的网格。
    ///
    /// 与上一条互补：固定的常量偏移（侧边栏/标签栏）不该吃掉大窗口的
    /// 比例——1920 与 1400 之间必须有可见差异，否则拖大窗口后 attach
    /// 报的尺寸还是旧的，快照仍然错位。
    #[test]
    fn a_bigger_window_maps_to_more_cells() {
        let small = TerminalGridHint::from_window_size(1400.0, 900.0).unwrap();
        let large = TerminalGridHint::from_window_size(1920.0, 1080.0).unwrap();
        assert!(
            large.cols > small.cols,
            "{} 应大于 {}",
            large.cols,
            small.cols
        );
        assert!(
            large.rows > small.rows,
            "{} 应大于 {}",
            large.rows,
            small.rows
        );
    }

    /// 退化输入不能产出 0 行列（那会让守护进程拒绝 resize 或算出空网格）。
    #[test]
    fn degenerate_sizes_are_rejected() {
        assert_eq!(TerminalGridHint::from_window_size(0.0, 900.0), None);
        assert_eq!(TerminalGridHint::from_window_size(1400.0, 0.0), None);
        assert_eq!(TerminalGridHint::from_window_size(f32::NAN, 900.0), None);
        assert_eq!(
            TerminalGridHint::from_window_size(f32::INFINITY, 900.0),
            None
        );
        // 比侧边栏还窄的窗口。
        assert_eq!(TerminalGridHint::from_window_size(200.0, 900.0), None);
    }
}
