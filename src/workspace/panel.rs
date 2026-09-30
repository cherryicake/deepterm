use gpui::prelude::*;
use gpui::{
    App, BoxShadow, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Render, SharedString, Styled, Window, div, point, px, svg,
};
use std::path::PathBuf;

use crate::agent::{AgentStatus, CLIAgent};
use crate::assets::Icon;
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

/// 拖拽的 Panel 数据
#[derive(Clone, Debug)]
pub struct DraggedPanel {
    pub panel_id: String,
    pub source_tab_id: String,
    pub tab_label: SharedString,
    pub icon: Option<Icon>,
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
    SessionChanged,
    /// Tab 标签变更事件（由终端工作目录/前台进程变化触发）
    TabLabelChanged {
        panel_id: String,
        label: String,
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
                    cx.emit(PanelEvent::TabLabelChanged {
                        panel_id: close_panel_id.clone(),
                        label,
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
                    cx.emit(PanelEvent::AgentStateChanged {
                        panel_id: close_panel_id.clone(),
                        agent,
                        status,
                    });
                }
            },
        )
        .detach();
    }

    pub fn with_terminal(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let terminal_view = TerminalView::new(cx);
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
        let terminal_view = TerminalView::new_with_working_directory(cx, working_directory);
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
    pub fn with_terminal_command(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        command: &str,
        args: &[String],
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let terminal_view = TerminalView::new_with_command(cx, command, args);
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
        let terminal_view = if let Some(cwd) = remote_cwd {
            TerminalView::new_ssh_with_remote_cwd(cx, ssh_config, Some(cwd))
        } else {
            TerminalView::new_ssh(cx, ssh_config)
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
            TerminalSessionState::Local { cwd, command, args } => {
                log::info!("重新启动面板 {} 的本地会话", self.panel_id);
                if let Some(cmd) = command {
                    TerminalView::new_with_command(cx, cmd, args)
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
        cx: &mut Context<Self>,
    ) -> Self {
        match &state.content {
            PanelContentSessionState::Terminal(TerminalSessionState::Local { cwd, .. }) => {
                let working_directory = cwd.as_ref().map(PathBuf::from);
                Self::with_terminal_working_directory(
                    state.id.clone(),
                    workspace_id,
                    active_tab_id,
                    active_tab_label,
                    working_directory,
                    cx,
                )
            }
            PanelContentSessionState::Terminal(TerminalSessionState::Ssh {
                remote_cwd, ..
            }) => {
                let panel_id = state.id.clone();
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
                    .id(format!("panel-title-{}", panel_id))
                    .on_drag(
                        DraggedPanel {
                            panel_id: panel_id.clone(),
                            source_tab_id,
                            tab_label: drag_title.clone().into(),
                            icon: self.remote_os.map(|os| os.icon()),
                        },
                        move |_drag_data, position, window, cx| {
                            // 阻止事件冒泡，避免被上层拖拽逻辑打断导致预览不同步
                            cx.stop_propagation();
                            let preview =
                                DragPreview::new(drag_title.clone().into(), position, window, cx);
                            cx.new(move |_cx| preview)
                        },
                    )
                    .when_some(self.remote_os, |this, os| {
                        this.child(
                            svg()
                                .size(px(14.0))
                                .path(os.icon().path())
                                .text_color(theme.text_muted)
                                .mr(px(6.0)),
                        )
                    })
                    .child(div().text_xs().text_color(theme.text_muted).child(title)),
            )
            .when(self.is_hovered, |this| {
                this.child(
                    // 右侧按钮组：广播按钮 + 关闭按钮
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
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
    }
}
