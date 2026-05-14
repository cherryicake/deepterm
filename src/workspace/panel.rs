use gpui::prelude::*;
use gpui::{
    App, BoxShadow, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Render, SharedString, Styled, Subscription, Window, div, point, px, svg,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputState};
use crate::components::tab_bar::{DragPreview, DraggedTab};
use crate::file_browser::{
    FileBrowserEvent, FileBrowserTarget, FileBrowserView, RemoteFileBrowserView,
};
use crate::file_transfer::TransferRequest;
use crate::settings_hub::settings_view::{SettingsHubLeftSidebar, SettingsHubView};
use crate::ssh::platform::RemoteOS;
use crate::terminal_view::terminal_view::{Event as TerminalViewEvent, TerminalView};
use crate::theme::ActiveTheme;

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
    /// 打开文件浏览器
    OpenFileBrowser {
        panel_id: String,
        target: FileBrowserTarget,
    },
    /// 关闭当前 Panel 上的文件浏览器覆盖层
    FileBrowserOverlayClosed { panel_id: String },
    /// 文件浏览器请求传输文件
    TransferRequested {
        panel_id: String,
        request: TransferRequest,
    },
    /// 在编辑器中打开文件
    OpenFile { panel_id: String, path: String },
    /// 关闭panel事件
    Close { panel_id: String },
    /// 选中状态变更事件
    Selected { panel_id: String },
    /// 广播状态切换事件
    BroadcastToggle { panel_id: String },
}

/// Panel content type
#[derive(Clone, Debug)]
pub enum PanelContent {
    /// Empty panel with just text
    Text(String),
    /// Terminal panel
    Terminal(Entity<TerminalView>),
    /// SettingsHub sidebar panel
    SettingsHubSidebar(Entity<SettingsHubLeftSidebar>),
    /// SettingsHub full view (sidebar + center content)
    SettingsHubView(Entity<SettingsHubView>),
    /// Local file browser
    FileBrowser(Entity<FileBrowserView>),
    /// Remote file browser
    RemoteFileBrowser(Entity<RemoteFileBrowserView>),
    /// Editor panel
    Editor(Entity<InputState>),
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
    /// Temporary file browser rendered over terminal content.
    file_browser_overlay: Option<PanelContent>,
    file_browser_overlay_subscription: Option<Subscription>,
    file_browser_content_subscription: Option<Subscription>,
    /// 广播状态 - 由Workspace同步给所有Panel
    pub broadcast_enabled: bool,
}

impl Panel {
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

        Self {
            panel_id,
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label: None,
            ssh_config_id: None,
            remote_os: None,
            is_solo: false,
            is_selected: false,
            is_hovered: false,
            content: PanelContent::Text(content_text),
            file_browser_overlay: None,
            file_browser_overlay_subscription: None,
            file_browser_content_subscription: None,
            broadcast_enabled: false,
        }
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
    /// Subscribes to TerminalView close events and emits PanelEvent::Close when the terminal exits.
    fn subscribe_terminal_events(
        panel_id: &str,
        terminal_view: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let close_panel_id = panel_id.to_string();
        cx.subscribe(
            terminal_view,
            move |this, _view, event: &TerminalViewEvent, cx| match event {
                TerminalViewEvent::CloseTerminal => {
                    cx.emit(PanelEvent::Close {
                        panel_id: close_panel_id.clone(),
                    });
                }
                TerminalViewEvent::OpenFileBrowser { target } => {
                    cx.emit(PanelEvent::OpenFileBrowser {
                        panel_id: close_panel_id.clone(),
                        target: target.clone(),
                    });
                }
                TerminalViewEvent::OpenFile { path } => {
                    cx.emit(PanelEvent::OpenFile {
                        panel_id: close_panel_id.clone(),
                        path: path.clone(),
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
                    let new_terminal_view = TerminalView::new_ssh(cx, config);
                    // 重新订阅新 TerminalView 的事件
                    Self::subscribe_terminal_events(&close_panel_id, &new_terminal_view, cx);
                    // 更新 panel 内容
                    this.set_content(PanelContent::Terminal(new_terminal_view), cx);
                }
                TerminalViewEvent::RemoteOsDetected { os } => {
                    this.remote_os = Some(*os);
                    cx.emit(PanelEvent::TabIconChanged {
                        panel_id: close_panel_id.clone(),
                        os: Some(*os),
                        ssh_config_id: this.ssh_config_id.clone(),
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
        Self {
            panel_id,
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
            content: PanelContent::Terminal(terminal_view),
            file_browser_overlay: None,
            file_browser_overlay_subscription: None,
            file_browser_content_subscription: None,
            broadcast_enabled: false,
        }
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
        Self {
            panel_id,
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
            content: PanelContent::Terminal(terminal_view),
            file_browser_overlay: None,
            file_browser_overlay_subscription: None,
            file_browser_content_subscription: None,
            broadcast_enabled: false,
        }
    }

    /// Creates a new panel with an SSH remote terminal view.
    pub fn with_ssh_terminal(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        active_tab_label: Option<SharedString>,
        ssh_config: &crate::ssh::SshConnectionConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel_id = panel_id.into();
        let terminal_view = TerminalView::new_ssh(cx, ssh_config);
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
            file_browser_overlay: None,
            file_browser_overlay_subscription: None,
            file_browser_content_subscription: None,
            broadcast_enabled: false,
        }
    }

    pub fn with_file_browser_view(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        file_browser_view: Entity<FileBrowserView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(
            &file_browser_view,
            |this: &mut Panel, _, event: &FileBrowserEvent, cx| {
                this.handle_file_browser_event(event, false, cx);
            },
        );

        Self {
            panel_id: panel_id.into(),
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label: Some("Files".into()),
            ssh_config_id: None,
            remote_os: None,
            is_solo: false,
            is_selected: false,
            is_hovered: false,
            content: PanelContent::FileBrowser(file_browser_view),
            file_browser_overlay: None,
            file_browser_overlay_subscription: None,
            file_browser_content_subscription: Some(subscription),
            broadcast_enabled: false,
        }
    }

    pub fn with_editor(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        editor_state: Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            panel_id: panel_id.into(),
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label: Some("Editor".into()),
            ssh_config_id: None,
            remote_os: None,
            is_solo: false,
            is_selected: false,
            is_hovered: false,
            content: PanelContent::Editor(editor_state),
            file_browser_overlay: None,
            file_browser_overlay_subscription: None,
            file_browser_content_subscription: None,
            broadcast_enabled: false,
        }
    }

    /// Creates a new panel with a HostsHubView (sidebar + center content).
    pub fn with_hostshub_view(
        panel_id: impl Into<String>,
        workspace_id: impl Into<String>,
        active_tab_id: Option<SharedString>,
        settings_hub_view: Entity<SettingsHubView>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            panel_id: panel_id.into(),
            workspace_id: workspace_id.into(),
            focus_handle: cx.focus_handle(),
            drag_preview: None,
            active_tab_id,
            active_tab_label: Some("Hosts".into()),
            ssh_config_id: None,
            remote_os: None,
            is_solo: false,
            is_selected: false,
            is_hovered: false,
            content: PanelContent::SettingsHubView(settings_hub_view),
            file_browser_overlay: None,
            file_browser_overlay_subscription: None,
            file_browser_content_subscription: None,
            broadcast_enabled: false,
        }
    }

    pub fn show_file_browser_overlay(
        &mut self,
        target: FileBrowserTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            FileBrowserTarget::Local { requested_path } => {
                if let Some(PanelContent::FileBrowser(file_browser)) = &self.file_browser_overlay {
                    file_browser.update(cx, |file_browser, cx| {
                        file_browser.open_requested_path(requested_path, window, cx);
                    });
                } else {
                    let file_browser = FileBrowserView::new_with_path(window, cx, requested_path);
                    self.subscribe_file_browser_overlay(&file_browser, cx);
                    self.file_browser_overlay = Some(PanelContent::FileBrowser(file_browser));
                }
            }
            FileBrowserTarget::Remote {
                ssh_config,
                requested_path,
            } => {
                let existing_view = match &self.file_browser_overlay {
                    Some(PanelContent::RemoteFileBrowser(file_browser)) => {
                        Some(file_browser.clone())
                    }
                    _ => None,
                };

                if let Some(file_browser) = existing_view {
                    if file_browser.read(cx).ssh_config() == &ssh_config {
                        file_browser.update(cx, |file_browser, cx| {
                            file_browser.open_requested_path(requested_path, window, cx);
                        });
                    } else {
                        let file_browser =
                            RemoteFileBrowserView::new(window, cx, ssh_config, requested_path);
                        self.subscribe_remote_file_browser_overlay(&file_browser, cx);
                        self.file_browser_overlay =
                            Some(PanelContent::RemoteFileBrowser(file_browser));
                    }
                } else {
                    let file_browser =
                        RemoteFileBrowserView::new(window, cx, ssh_config, requested_path);
                    self.subscribe_remote_file_browser_overlay(&file_browser, cx);
                    self.file_browser_overlay = Some(PanelContent::RemoteFileBrowser(file_browser));
                }
            }
        }

        cx.notify();
    }

    fn subscribe_file_browser_overlay(
        &mut self,
        file_browser: &Entity<FileBrowserView>,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe(file_browser, |this, _, event: &FileBrowserEvent, cx| {
            this.handle_file_browser_event(event, true, cx);
        });
        self.file_browser_overlay_subscription = Some(subscription);
    }

    fn subscribe_remote_file_browser_overlay(
        &mut self,
        file_browser: &Entity<RemoteFileBrowserView>,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe(file_browser, |this, _, event: &FileBrowserEvent, cx| {
            this.handle_file_browser_event(event, true, cx);
        });
        self.file_browser_overlay_subscription = Some(subscription);
    }

    fn handle_file_browser_event(
        &mut self,
        event: &FileBrowserEvent,
        is_overlay: bool,
        cx: &mut Context<Self>,
    ) {
        match event {
            FileBrowserEvent::Close if is_overlay => self.close_file_browser_overlay(cx),
            FileBrowserEvent::Close => {}
            FileBrowserEvent::TransferRequested(request) => {
                cx.emit(PanelEvent::TransferRequested {
                    panel_id: self.panel_id.clone(),
                    request: request.clone(),
                });
            }
        }
    }

    fn close_file_browser_overlay(&mut self, cx: &mut Context<Self>) {
        if self.file_browser_overlay.take().is_some() {
            self.file_browser_overlay_subscription = None;
            cx.emit(PanelEvent::FileBrowserOverlayClosed {
                panel_id: self.panel_id.clone(),
            });
            cx.notify();
        }
    }

    /// 设置panel内容
    pub fn set_content(&mut self, content: PanelContent, cx: &mut Context<Self>) {
        self.file_browser_content_subscription = match &content {
            PanelContent::FileBrowser(file_browser) => Some(cx.subscribe(
                file_browser,
                |this, _, event: &FileBrowserEvent, cx| {
                    this.handle_file_browser_event(event, false, cx);
                },
            )),
            PanelContent::RemoteFileBrowser(file_browser) => Some(cx.subscribe(
                file_browser,
                |this, _, event: &FileBrowserEvent, cx| {
                    this.handle_file_browser_event(event, false, cx);
                },
            )),
            _ => None,
        };
        self.content = content;
        cx.notify();
    }

    pub fn refresh_file_browser_content(&mut self, cx: &mut Context<Self>) {
        match &self.content {
            PanelContent::FileBrowser(file_browser) => {
                file_browser.update(cx, |file_browser, cx| file_browser.refresh(cx));
            }
            PanelContent::RemoteFileBrowser(file_browser) => {
                file_browser.update(cx, |file_browser, cx| file_browser.refresh(cx));
            }
            _ => {}
        }

        match &self.file_browser_overlay {
            Some(PanelContent::FileBrowser(file_browser)) => {
                file_browser.update(cx, |file_browser, cx| file_browser.refresh(cx));
            }
            Some(PanelContent::RemoteFileBrowser(file_browser)) => {
                file_browser.update(cx, |file_browser, cx| file_browser.refresh(cx));
            }
            _ => {}
        }
    }

    /// Returns true if this panel contains a terminal view.
    pub fn has_terminal_content(&self) -> bool {
        matches!(self.content, PanelContent::Terminal(_))
    }

    /// Returns a reference to the panel's content.
    pub fn content(&self) -> &PanelContent {
        &self.content
    }

    /// 设置Tab标签名
    pub fn set_tab_label(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.active_tab_label = Some(label.into());
        cx.notify();
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
        if let PanelContent::Terminal(terminal_view) = &self.content {
            let panel_id = self.panel_id.clone();
            let workspace_id = self.workspace_id.clone();
            terminal_view.update(cx, |tv, _cx| {
                if enabled {
                    // 创建广播回调，当终端有输入时添加到全局队列
                    tv.set_broadcast_callback(Box::new(move |data: &[u8]| {
                        // 将输入添加到全局队列，由Workspace的render方法处理
                        // 包含workspace_id以区分不同WorkSpace的广播
                        crate::workspace::workspace::Workspace::queue_broadcast_input(
                            &workspace_id,
                            &panel_id,
                            data,
                        );
                    }));
                } else {
                    tv.clear_broadcast_callback();
                }
            });
        }

        cx.notify();
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

impl Focusable for Panel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel {
    /// 获取内部 TerminalView 的 focus handle（如果有）
    pub fn content_focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match &self.content {
            PanelContent::Terminal(terminal_view) => Some(terminal_view.read(cx).focus_handle(cx)),
            PanelContent::FileBrowser(file_browser) => Some(file_browser.read(cx).focus_handle(cx)),
            PanelContent::RemoteFileBrowser(file_browser) => {
                Some(file_browser.read(cx).focus_handle(cx))
            }
            PanelContent::SettingsHubSidebar(sidebar) => Some(sidebar.read(cx).focus_handle(cx)),
            PanelContent::SettingsHubView(settings_hub_view) => {
                Some(settings_hub_view.read(cx).focus_handle(cx))
            }
            PanelContent::Editor(editor_view) => Some(editor_view.read(cx).focus_handle(cx)),
            _ => None,
        }
    }

    pub fn active_content_focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match &self.file_browser_overlay {
            Some(PanelContent::FileBrowser(file_browser)) => {
                Some(file_browser.read(cx).focus_handle(cx))
            }
            Some(PanelContent::RemoteFileBrowser(file_browser)) => {
                Some(file_browser.read(cx).focus_handle(cx))
            }
            _ => self.content_focus_handle(cx),
        }
    }
}

impl EventEmitter<PanelEvent> for Panel {}

impl Panel {
    /// 渲染标题栏
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        // 使用Tab标签名作为标题，如果没有则使用Tab ID，最后使用Panel ID
        let title = self
            .active_tab_label
            .as_ref()
            .map(|s| s.to_string())
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
        let drag_title = title.clone();
        let broadcast_enabled = self.broadcast_enabled;

        div()
            .h(px(28.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .cursor_move()
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
        let content = self.file_browser_overlay.as_ref().unwrap_or(&self.content);

        match content {
            PanelContent::Text(text) => {
                div().flex_1().flex().items_center().justify_center().child(
                    self.active_tab_id
                        .as_ref()
                        .map(|id| format!("{} | Tab: {}", text, id))
                        .unwrap_or_else(|| text.clone()),
                )
            }
            PanelContent::Terminal(terminal_view) => div().flex_1().child(terminal_view.clone()),
            PanelContent::SettingsHubSidebar(settingshub_sidebar) => {
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .p(px(6.0))
                    .child(settingshub_sidebar.clone())
                    // 右侧填充区域（可放置其他内容）
                    .child(div().flex_1())
            }
            PanelContent::SettingsHubView(settings_hub_view) => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(settings_hub_view.clone()),
            PanelContent::FileBrowser(file_browser_view) => div()
                .flex_1()
                .min_h_0()
                .min_w(px(0.0))
                .overflow_hidden()
                .child(file_browser_view.clone()),
            PanelContent::RemoteFileBrowser(file_browser_view) => div()
                .flex_1()
                .min_h_0()
                .min_w(px(0.0))
                .overflow_hidden()
                .child(file_browser_view.clone()),
            PanelContent::Editor(editor_state) => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(EditorInput::new(&editor_state)),
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
        // 如果有preview就使用它，否则默认使用Right
        let placement = self.drag_preview.unwrap_or(PanelPlacement::Right);

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

        // 如果有preview就使用它，否则默认使用Right
        let placement = self.drag_preview.unwrap_or(PanelPlacement::Right);

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
            // SettingsHub Panel 使用透明背景以显示毛玻璃效果
            .when(
                !matches!(self.content, PanelContent::SettingsHubSidebar(_)),
                |this| this.bg(theme.panel_background),
            )
            .when(!is_solo, |this| {
                // 选中时边框颜色加深，未选中时让边框更接近背景色，仅微微可见
                let border_color = if is_selected {
                    theme.border_strong
                } else {
                    theme.border.opacity(0.35)
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
