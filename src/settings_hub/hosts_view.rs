use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    hsla, px, rgb, svg,
};

use crate::assets::Icon;

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
}

impl HostsView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            // Sample groups matching the screenshot
            let groups = vec![
                HostGroup::new("sn2-workers", "sn2-workers", 5)
                    .with_icon_color(HostIconColor::Blue),
            ];

            // Sample hosts matching the screenshot
            let hosts = vec![
                Host::new("host1", "2.webNX-69-9950x | 5090 | UT:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host2", "2.webNX-70-9950x | 4090 | UT:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host3", "2.webNX-71-9950x | 5090 | UT:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host4", "2.Webnx-76| 9950X| 5090 | 96...", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host5", "34.134.161.69", HostType::Ssh)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Blue),
                Host::new("host6", "34.134.161.69", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Blue),
                Host::new("host7", "34.172.25.233", HostType::Ssh)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host8", "34.31.75.139", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host9", "34.45.123.231", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host10", "34.45.123.231", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host11", "34.69.228.237", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host12", "34.87.167.138", HostType::Ssh)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host13", "34.9.147.31", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Blue),
                Host::new("host14", "34.9.147.31", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Blue),
                Host::new("host15", "35.226.233.134", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host16", "35.226.233.134", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Blue),
                Host::new("host17", "173.231.14.170", HostType::Ssh)
                    .with_icon_color(HostIconColor::Blue),
                Host::new("host18", "192.168.194.116", HostType::Ssh)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Blue),
                Host::new("host19", "lingsi-ubuntu-server", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host20", "sn2-gateway", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host21", "sn2-miner", HostType::Telnet)
                    .with_username("zhousong")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host22", "webNX-c01-9950x | UT:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host23", "webNX-c02-9900x | UT:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host24", "webNX-c03-9950x-96G | UT:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host25", "webNX-c04-9950x-96G | UT...", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host26", "webNX-c05-9700x-128G | NY:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
                Host::new("host27", "webNX-c06-9700x-128G | LA:", HostType::Ssh)
                    .with_username("root")
                    .with_icon_color(HostIconColor::Orange),
            ];

            Self {
                search_query: SharedString::new_static(""),
                hosts,
                groups,
                focus_handle,
            }
        })
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
    fn render_action_buttons(&self, _cx: &mut Context<Self>) -> impl IntoElement {
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
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .bg(rgb(0x2d2d44))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0x3d3d5c)))
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

    /// Render a host card
    fn render_host_card(&self, host: &Host) -> impl IntoElement {
        let icon_color = host.icon_color.to_hsla();

        div()
            .id(format!("host-{}", host.id))
            .w_full()
            .min_w(px(200.0))
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
        // Clone data to avoid lifetime issues
        let groups: Vec<HostGroup> = self.groups.clone();

        // Pre-render all group cards to avoid lifetime issues
        let group_cards: Vec<gpui::AnyElement> = groups
            .iter()
            .map(|group| self.render_group_card(group).into_any_element())
            .collect();

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .px(px(16.0))
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
                    .gap(px(12.0))
                    .children(group_cards),
            )
    }

    /// Render the Hosts section with grid
    fn render_hosts_section(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        // Clone data to avoid lifetime issues
        let hosts: Vec<Host> = self.hosts.clone();

        // Pre-render all host cards to avoid lifetime issues
        let host_cards: Vec<gpui::AnyElement> = hosts
            .iter()
            .map(|host| self.render_host_card(host).into_any_element())
            .collect();

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .px(px(16.0))
            .child(
                // Section title
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0x8888aa))
                    .child("Hosts"),
            )
            .child(
                // Hosts grid - 5 columns like the screenshot
                div()
                    .w_full()
                    .grid()
                    .grid_cols(5)
                    .gap(px(12.0))
                    .children(host_cards),
            )
    }
}

impl Focusable for HostsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HostsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(self.render_hosts_section(cx)),
            )
    }
}
