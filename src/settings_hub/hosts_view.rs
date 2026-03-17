use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window,
    div, hsla, px, rgb, svg,
};

use crate::assets::Icon;
use crate::settings_hub::settings_view::{LEFT_SIDEBAR_WIDTH, RIGHT_SIDEBAR_WIDTH};
use crate::ssh::config::{SshGlobalConfig, AuthMethod, SshConnectionConfig};
use crate::workspace::workspace::Workspace;

// Layout constants
const GRID_GAP_PX: f32 = 12.0;
const MIN_CARD_WIDTH_PX: f32 = 260.0;
const CONTENT_PADDING_PX: f32 = 16.0;
const MAX_GRID_COLUMNS: u16 = 5;

/// Host connection type
#[derive(Clone, Debug, PartialEq)]
pub enum HostType {
    Ssh,
    Telnet,
}

/// Host information
#[derive(Clone, Debug)]
pub struct Host {
    pub id: SharedString,
    pub name: SharedString,
    pub host_type: HostType,
    pub username: Option<SharedString>,
    pub hostname: Option<SharedString>,
    pub icon_color: HostIconColor,
}

impl Host {
    pub fn new(
        id: impl Into<SharedString>,
        name: impl Into<SharedString>,
        host_type: HostType,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            host_type,
            username: None,
            hostname: None,
            icon_color: HostIconColor::Orange,
        }
    }

    pub fn with_username(mut self, username: impl Into<SharedString>) -> Self {
        self.username = Some(username.into());
        self
    }

    pub fn with_hostname(mut self, hostname: impl Into<SharedString>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    pub fn with_icon_color(mut self, color: HostIconColor) -> Self {
        self.icon_color = color;
        self
    }

    /// Get the subtitle text (protocol, username)
    pub fn subtitle(&self) -> String {
        let protocol = match self.host_type {
            HostType::Ssh => "ssh",
            HostType::Telnet => "telnet",
        };

        if let Some(ref username) = self.username {
            format!("{}, {}", protocol, username)
        } else {
            protocol.to_string()
        }
    }
}

/// Host icon colors matching the screenshot
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HostIconColor {
    Orange,
    Blue,
    Green,
    Purple,
    Red,
}

impl HostIconColor {
    pub fn to_hsla(&self) -> gpui::Hsla {
        match self {
            HostIconColor::Orange => hsla(25.0 / 360.0, 0.85, 0.55, 1.0),
            HostIconColor::Blue => hsla(210.0 / 360.0, 0.80, 0.55, 1.0),
            HostIconColor::Green => hsla(145.0 / 360.0, 0.70, 0.45, 1.0),
            HostIconColor::Purple => hsla(270.0 / 360.0, 0.70, 0.60, 1.0),
            HostIconColor::Red => hsla(0.0 / 360.0, 0.75, 0.55, 1.0),
        }
    }
}

/// Host group
#[derive(Clone, Debug)]
pub struct HostGroup {
    pub id: SharedString,
    pub name: SharedString,
    pub host_count: u32,
    pub icon_color: HostIconColor,
}

impl HostGroup {
    pub fn new(
        id: impl Into<SharedString>,
        name: impl Into<SharedString>,
        host_count: u32,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            host_count,
            icon_color: HostIconColor::Blue,
        }
    }

    pub fn with_icon_color(mut self, color: HostIconColor) -> Self {
        self.icon_color = color;
        self
    }
}

/// Hosts View Component - Main content for the Hosts settings page
pub struct HostsView {
    search_query: SharedString,
    hosts: Vec<Host>,
    groups: Vec<HostGroup>,
    focus_handle: FocusHandle,
    workspace: Option<Entity<Workspace>>,
    configs: Vec<SshConnectionConfig>,
    on_new_host_click: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
}

impl HostsView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            // Try to load SSH config from file
            let (hosts, groups, configs) = Self::load_hosts_from_config();

            Self {
                search_query: SharedString::new_static(""),
                hosts,
                groups,
                focus_handle,
                workspace: None,
                configs,
                on_new_host_click: None,
            }
        })
    }

    /// Set the workspace for terminal operations
    pub fn with_workspace(mut self, workspace: Entity<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Create a new HostsView with optional workspace
    pub fn new_with_workspace(cx: &mut App, workspace: Option<Entity<Workspace>>) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            // Try to load SSH config from file
            let (hosts, groups, configs) = Self::load_hosts_from_config();

            Self {
                search_query: SharedString::new_static(""),
                hosts,
                groups,
                focus_handle,
                workspace,
                configs,
                on_new_host_click: None,
            }
        })
    }

    /// Refresh hosts and groups from SSH config file
    pub fn refresh_hosts(&mut self, cx: &mut Context<Self>) {
        let (hosts, groups, configs) = Self::load_hosts_from_config();
        self.hosts = hosts;
        self.groups = groups;
        self.configs = configs;
        cx.notify();
    }

    /// Load hosts and groups from SSH config file, returning hosts, groups, and configs
    fn load_hosts_from_config() -> (Vec<Host>, Vec<HostGroup>, Vec<SshConnectionConfig>) {
        let config = SshGlobalConfig::load().unwrap_or_else(SshGlobalConfig::new);

        // Convert saved connections to Host structs
        let hosts: Vec<Host> = config
            .saved_connections
            .iter()
            .map(|conn| {
                let display_name = conn.nickname.clone().unwrap_or_else(|| conn.display_name());

                // Determine icon color based on auth method
                let icon_color = match conn.auth_method {
                    AuthMethod::Agent => HostIconColor::Blue,
                    AuthMethod::PrivateKey { .. } => HostIconColor::Green,
                    AuthMethod::Password { .. } => HostIconColor::Orange,
                    _ => HostIconColor::Orange,
                };

                Host::new(conn.id(), display_name, HostType::Ssh)
                    .with_username(&conn.username)
                    .with_hostname(&conn.host)
                    .with_icon_color(icon_color)
            })
            .collect();

        // Create a default group containing all hosts
        let host_count = hosts.len() as u32;
        let groups = if host_count > 0 {
            vec![HostGroup::new("all-hosts", "All Hosts", host_count).with_icon_color(HostIconColor::Blue)]
        } else {
            vec![]
        };

        (hosts, groups, config.saved_connections)
    }

    /// Render the search bar
    fn render_search_bar(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h(px(40.0))
            .px(px(16.0))
            .flex()
            .items_center()
            .child(
                // Search input with background - CONNECT button inside
                div()
                    .flex_1()
                    .h_10()
                    .pl(px(12.0))
                    .pr(px(6.0))
                    .rounded(px(8.0))
                    .bg(rgb(0x1a1a2e))
                    .border_1()
                    .border_color(rgb(0x2d2d44))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x666688))
                            .child("Find a host or ssh user@hostname..."),
                    )
                    .child(
                        // Connect button inside search box
                        div()
                            .px(px(10.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x252538))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x2d2d44)))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(0x666688))
                                    .child("CONNECT"),
                            ),
                    ),
            )
    }

    /// Render dropdown arrow using ChevronDown icon
    fn render_dropdown_arrow() -> impl IntoElement {
        svg()
            .size(px(8.0))
            .path(Icon::ChevronDown.path())
            .text_color(rgb(0x8888aa))
    }

    /// Render action buttons (NEW HOST, TERMINAL, SERIAL)
    fn render_action_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h(px(20.0))
            .px(px(16.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                // NEW HOST button with dropdown
                div()
                    .id("new-host-button")
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .bg(rgb(0x2d2d44))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0x3d3d5c)))
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        if let Some(ref callback) = this.on_new_host_click {
                            callback(cx);
                        }
                    }))
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                svg()
                                    .size(px(10.0))
                                    .path(Icon::Plus.path())
                                    .text_color(rgb(0x8888aa)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(0xaaaaaa))
                                    .child("NEW HOST"),
                            ),
                    )
                    .child(div().w(px(1.0)).h(px(14.0)).bg(rgb(0x3d3d5c)))
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(4.0))
                            .child(Self::render_dropdown_arrow()),
                    ),
            )
            .child(
                // TERMINAL button
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .bg(rgb(0x2d2d44))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0x3d3d5c)))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        svg()
                            .size(px(10.0))
                            .path(Icon::Terminal.path())
                            .text_color(rgb(0x8888aa)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xaaaaaa))
                            .child("TERMINAL"),
                    ),
            )
            .child(
                // SERIAL button
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .bg(rgb(0x2d2d44))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0x3d3d5c)))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(10.0))
                            .h(px(10.0))
                            .rounded_sm()
                            .bg(rgb(0x8888aa))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().w(px(6.0)).h(px(2.0)).bg(rgb(0x2d2d44))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xaaaaaa))
                            .child("SERIAL"),
                    )
                    .child(Self::render_dropdown_arrow()),
            )
    }

    /// Render a group card
    fn render_group_card(&self, group: &HostGroup) -> impl IntoElement {
        let icon_color = group.icon_color.to_hsla();

        div()
            .id(format!("group-{}", group.id))
            .w(px(280.0))
            .h(px(56.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .rounded(px(10.0))
            .bg(rgb(0x1a1a2e))
            .border_1()
            .border_color(rgb(0x2d2d44))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(0x252538)))
            .child(
                // Group icon
                div()
                    .w(px(36.0))
                    .h(px(36.0))
                    .rounded(px(8.0))
                    .bg(icon_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(20.0))
                            .path(Icon::Server.path())
                            .text_color(rgb(0xffffff)),
                    ),
            )
            .child(
                // Group info
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xcccccc))
                            .child(group.name.to_string()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x666688))
                            .child(format!("{} Hosts", group.host_count)),
                    ),
            )
    }

    /// Connect to a host by its ID
    fn connect_to_host(&self, host_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = match self.workspace.clone() {
            Some(w) => w,
            None => return,
        };

        // Find the config matching this host ID
        if let Some(config) = self.configs.iter().find(|c| c.id() == host_id) {
            let config = config.clone();
            workspace.update(cx, |w, cx| {
                w.create_ssh_terminal_with_config(&config, window, cx);
            });
        }
    }

    /// Set callback for when NEW HOST button is clicked
    pub fn on_new_host_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_new_host_click = Some(Box::new(callback));
    }

    /// Render a host card
    fn render_host_card(&self, host: &Host, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let icon_color = host.icon_color.to_hsla();
        let host_id = host.id.to_string();

        div()
            .id(format!("host-{}", host.id))
            .w_full()
            .h(px(64.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .rounded(px(10.0))
            .bg(rgb(0x1a1a2e))
            .border_1()
            .border_color(rgb(0x2d2d44))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(0x252538)))
            // Double click to connect
            .on_click(cx.listener({
                let host_id = host_id.clone();
                move |this, event: &gpui::ClickEvent, window, cx| {
                    // Connect on double click
                    if event.click_count() == 2 {
                        this.connect_to_host(&host_id, window, cx);
                    }
                }
            }))
            .child(
                // Host icon
                div()
                    .w(px(40.0))
                    .h(px(40.0))
                    .rounded(px(8.0))
                    .bg(icon_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(22.0))
                            .path(Icon::Linux.path())
                            .text_color(rgb(0xffffff)),
                    ),
            )
            .child(
                // Host info
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xcccccc))
                            .truncate()
                            .child(host.name.to_string()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x666688))
                            .child(host.subtitle()),
                    ),
            )
    }

    /// Render the Groups section
    fn render_groups_section(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(GRID_GAP_PX))
            .px(px(CONTENT_PADDING_PX))
            .child(
                // Section title
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0x8888aa))
                    .child("Groups"),
            )
            .child(
                // Groups grid
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .gap(px(GRID_GAP_PX))
                    .children(self.groups.iter().map(|group| {
                        self.render_group_card(group).into_any_element()
                    })),
            )
    }

    /// Calculate the number of columns based on available width
    /// Accounts for sidebars (left: ~200px, right: ~300px when visible)
    fn calculate_columns(&self, available_width: f32) -> u16 {
        // Account for left sidebar, right sidebar, and padding
        // Each sidebar has ~12px outer padding
        let sidebar_padding = 24.0 + 24.0; // left + right outer paddings
        let sidebar_width = LEFT_SIDEBAR_WIDTH + RIGHT_SIDEBAR_WIDTH + sidebar_padding;
        let content_padding = CONTENT_PADDING_PX * 2.0;
        let effective_width = available_width - sidebar_width - content_padding;

        // Calculate max possible columns
        let max_cols = ((effective_width + GRID_GAP_PX) / (MIN_CARD_WIDTH_PX + GRID_GAP_PX)).floor() as u16;

        // Clamp between 1 and MAX_GRID_COLUMNS
        max_cols.clamp(1, MAX_GRID_COLUMNS)
    }

    /// Render the Hosts section with responsive flex wrap layout
    fn render_hosts_section(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(GRID_GAP_PX))
            .px(px(CONTENT_PADDING_PX))
            .child(
                // Section title
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0x8888aa))
                    .child("Hosts"),
            )
            .child(self.render_hosts_grid(window, cx))
    }

    /// Render hosts grid with responsive columns and placeholders
    fn render_hosts_grid(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Get available width and calculate columns accounting for sidebars
        let viewport_width = window.viewport_size().width.as_f32();
        let cols = self.calculate_columns(viewport_width);

        let host_count = self.hosts.len();

        // Calculate how many placeholders needed to fill the last row
        let remainder = host_count % (cols as usize);
        let placeholder_count = if remainder == 0 {
            0
        } else {
            (cols as usize) - remainder
        };

        div()
            .w_full()
            .grid()
            .grid_cols(cols)
            .gap(px(GRID_GAP_PX))
            .children(self.hosts.iter().map(|host| {
                self.render_host_card(host, window, cx).into_any_element()
            }))
            // Add invisible placeholders to maintain grid alignment
            .children((0..placeholder_count).map(|_| {
                div()
                    .w_full()
                    .h(px(64.0))
                    .into_any_element()
            }))
    }
}

impl Focusable for HostsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HostsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("hosts-view")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x0f0f1a))
            // Search bar
            .child(self.render_search_bar(cx))
            // Spacer between search bar and action buttons
            .child(div().h_5())
            // Action buttons
            .child(self.render_action_buttons(cx))
            // Scrollable content
            .child(
                div()
                    .id("hosts-content")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(24.0))
                    .py(px(16.0))
                    // Groups section
                    .child(self.render_groups_section(cx))
                    // Hosts section
                    .child(self.render_hosts_section(window, cx)),
            )
    }
}
