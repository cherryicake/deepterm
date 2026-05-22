use gpui::prelude::*;
use gpui::{
    App, Context, Entity, ExternalPaths, FocusHandle, Focusable, IntoElement, Render,
    ScrollHandle, SharedString, Window, actions, div, px, svg,
};

use std::collections::HashSet;
use std::path::PathBuf;

actions!(
    host_actions,
    [ConnectHost, EditHost, DuplicateHost, DeleteHost]
);

use crate::assets::Icon;
use crate::components::button::{
    Button, ButtonCustomVariant, ButtonVariants, Icon as ButtonIconType, Sizable,
};
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::menu::{ContextMenuExt, PopupMenu};
use crate::ssh::config::{AuthMethod, SshConnectionConfig, SshGlobalConfig};
use crate::ssh::keychain::{KeychainItem, KeychainItemKind, KeychainKeyFlavor, KeychainStore};
use crate::ssh::platform::{RemoteOS, guess_os_from_text};
use crate::theme::ActiveTheme;
use crate::workspace::workspace::Workspace;

// Layout constants
const GRID_GAP_PX: f32 = 12.0;
const MIN_CARD_WIDTH_PX: f32 = 280.0;
const MAX_CARD_WIDTH_PX: f32 = 360.0;
const CONTENT_PADDING_PX: f32 = 24.0;

/// Host connection type
#[derive(Clone, Debug, PartialEq)]
pub enum HostType {
    Ssh,
}

/// Host information
#[derive(Clone, Debug)]
pub struct Host {
    pub id: SharedString,
    pub name: SharedString,
    pub host_type: HostType,
    pub username: Option<SharedString>,
    pub hostname: Option<SharedString>,
    pub auth_label: Option<SharedString>,
    pub icon_color: HostIconColor,
    pub os_icon: Option<Icon>,
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
            auth_label: None,
            icon_color: HostIconColor::Orange,
            os_icon: None,
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

    pub fn with_auth_label(mut self, auth_label: impl Into<SharedString>) -> Self {
        self.auth_label = Some(auth_label.into());
        self
    }

    pub fn with_icon_color(mut self, color: HostIconColor) -> Self {
        self.icon_color = color;
        self
    }

    pub fn with_os_icon(mut self, icon: Icon) -> Self {
        self.os_icon = Some(icon);
        self
    }

    /// Get the subtitle text (protocol, username)
    pub fn subtitle(&self) -> String {
        let protocol = match self.host_type {
            HostType::Ssh => "ssh",
        };

        match (&self.username, &self.auth_label) {
            (Some(username), Some(auth_label)) => {
                format!("{}, {}, {}", protocol, username, auth_label)
            }
            (Some(username), None) => format!("{}, {}", protocol, username),
            (None, Some(auth_label)) => format!("{}, {}", protocol, auth_label),
            (None, None) => protocol.to_string(),
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
    pub fn to_hsla(&self, cx: &App) -> gpui::Hsla {
        let theme = cx.theme().ui;
        match self {
            HostIconColor::Orange => theme.warning,
            HostIconColor::Blue => theme.accent,
            HostIconColor::Green => theme.success,
            HostIconColor::Purple => theme.accent_hover,
            HostIconColor::Red => theme.error,
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
    search_input: Entity<InputState>,
    search_query: SharedString,
    hosts: Vec<Host>,
    filtered_hosts: Vec<Host>, // Filtered hosts based on search query
    groups: Vec<HostGroup>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    workspace: Option<Entity<Workspace>>,
    configs: Vec<SshConnectionConfig>,
    on_new_host_click: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
    on_edit_host_click: Option<Box<dyn Fn(&mut App, SshConnectionConfig) + Send + Sync>>,
    selected_host_ids: HashSet<SharedString>,
    context_menu_host_id: Option<SharedString>, // Track which host was right-clicked
    left_sidebar_width: f32,
    right_sidebar_width: f32,
    // Keychain items
    keychain_items: Vec<KeychainItem>,
    selected_keychain_item_id: Option<String>,
    on_new_key_click: Option<Box<dyn Fn(&mut App, Option<PathBuf>) + Send + Sync>>,
    on_new_identity_click: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
    on_edit_keychain_item_click: Option<Box<dyn Fn(&mut App, KeychainItem) + Send + Sync>>,
}

impl HostsView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::new_with_workspace(window, cx, None)
    }

    /// Set the workspace for terminal operations
    pub fn with_workspace(mut self, workspace: Entity<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Create a new HostsView with optional workspace
    pub fn new_with_workspace(
        window: &mut Window,
        cx: &mut App,
        workspace: Option<Entity<Workspace>>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let scroll_handle = ScrollHandle::new();
            let (hosts, groups, configs) = Self::load_hosts_from_config();
            let filtered_hosts = hosts.clone();

            let search_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Find a host or ssh user@hostname...")
                    .placeholder_font_size(px(14.0))
            });

            let subscription = cx.subscribe(
                &search_input,
                |this: &mut HostsView, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        let text = this.search_input.read(cx).value().to_string();
                        this.update_search_query(text, cx);
                    }
                },
            );
            std::mem::forget(subscription);

            Self {
                search_input,
                search_query: SharedString::new_static(""),
                hosts,
                filtered_hosts,
                groups,
                focus_handle,
                scroll_handle,
                workspace,
                configs,
                on_new_host_click: None,
                on_edit_host_click: None,
                selected_host_ids: HashSet::new(),
                context_menu_host_id: None,
                left_sidebar_width: 0.0,
                right_sidebar_width: 0.0,
                keychain_items: Self::load_keychain_items(),
                selected_keychain_item_id: None,
                on_new_key_click: None,
                on_new_identity_click: None,
                on_edit_keychain_item_click: None,
            }
        })
    }

    /// Refresh hosts and groups from SSH config file
    pub fn refresh_hosts(&mut self, cx: &mut Context<Self>) {
        let (hosts, groups, configs) = Self::load_hosts_from_config();
        self.hosts = hosts;
        self.filtered_hosts = self.hosts.clone();
        self.groups = groups;
        self.configs = configs;
        // Clean up selected hosts that no longer exist
        let valid_ids: HashSet<_> = self.hosts.iter().map(|h| h.id.clone()).collect();
        self.selected_host_ids.retain(|id| valid_ids.contains(id));
        // Clear search query
        self.search_query = SharedString::new_static("");
        self.search_input.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    /// Load keychain items from store
    fn load_keychain_items() -> Vec<KeychainItem> {
        KeychainStore::load_or_default().items
    }

    /// Refresh keychain items
    pub fn refresh_items(&mut self, cx: &mut Context<Self>) {
        self.keychain_items = Self::load_keychain_items();
        cx.notify();
    }

    /// Set callback for NEW KEY button
    pub fn on_new_key_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App, Option<PathBuf>) + Send + Sync + 'static,
    {
        self.on_new_key_click = Some(Box::new(callback));
    }

    /// Set callback for NEW IDENTITY button
    pub fn on_new_identity_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_new_identity_click = Some(Box::new(callback));
    }

    /// Set callback for editing a keychain item
    pub fn on_edit_keychain_item_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App, KeychainItem) + Send + Sync + 'static,
    {
        self.on_edit_keychain_item_click = Some(Box::new(callback));
    }

    /// Update search query and filter hosts
    fn update_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query.to_lowercase().into();
        self.filter_hosts();
        cx.notify();
    }

    /// Filter hosts based on search query
    fn filter_hosts(&mut self) {
        let query = self.search_query.as_ref();
        if query.is_empty() {
            self.filtered_hosts = self.hosts.clone();
        } else {
            self.filtered_hosts = self
                .hosts
                .iter()
                .filter(|host| {
                    host.name.to_lowercase().contains(query)
                        || host
                            .hostname
                            .as_ref()
                            .map_or(false, |h| h.to_lowercase().contains(query))
                        || host
                            .username
                            .as_ref()
                            .map_or(false, |u| u.to_lowercase().contains(query))
                        || host
                            .auth_label
                            .as_ref()
                            .map_or(false, |a| a.to_lowercase().contains(query))
                })
                .cloned()
                .collect();
        }
    }

    /// Load hosts and groups from SSH config file, returning hosts, groups, and configs
    pub fn load_hosts_from_config() -> (Vec<Host>, Vec<HostGroup>, Vec<SshConnectionConfig>) {
        let config = SshGlobalConfig::load();
        let keychain = KeychainStore::load_or_default();

        // Convert saved connections to Host structs
        let hosts: Vec<Host> = config
            .saved_connections
            .iter()
            .map(|conn| {
                let display_name = conn.nickname.clone().unwrap_or_else(|| conn.display_name());

                // Determine icon color based on auth method
                let (icon_color, auth_label) = match &conn.auth_method {
                    AuthMethod::PasswordRef { keychain_id } => {
                        if keychain.find_item(keychain_id).is_some() {
                            (HostIconColor::Orange, "identity".to_string())
                        } else {
                            (HostIconColor::Red, "missing identity".to_string())
                        }
                    }
                    AuthMethod::PrivateKeyRef { keychain_id } => {
                        match keychain.find_item(keychain_id) {
                            Some(item) => {
                                let label = match item.key_flavor {
                                    KeychainKeyFlavor::Key => "key",
                                    KeychainKeyFlavor::Certificate => "certificate",
                                    KeychainKeyFlavor::TouchId => "touch id",
                                    KeychainKeyFlavor::Fido2 => "fido2",
                                };
                                let color = match item.key_flavor {
                                    KeychainKeyFlavor::Key => HostIconColor::Green,
                                    KeychainKeyFlavor::Certificate => HostIconColor::Blue,
                                    KeychainKeyFlavor::TouchId => HostIconColor::Purple,
                                    KeychainKeyFlavor::Fido2 => HostIconColor::Blue,
                                };
                                (color, label.to_string())
                            }
                            None => (HostIconColor::Red, "missing key".to_string()),
                        }
                    }
                    AuthMethod::Agent => (HostIconColor::Blue, "agent".to_string()),
                    AuthMethod::PrivateKey { .. } => (HostIconColor::Green, "key".to_string()),
                    AuthMethod::Password { .. } => (HostIconColor::Orange, "password".to_string()),
                    _ => (HostIconColor::Orange, "auto".to_string()),
                };

                let os = conn
                    .os
                    .as_deref()
                    .map(RemoteOS::from_id)
                    .unwrap_or_else(|| guess_os_from_text(&display_name));
                let host = Host::new(conn.id(), display_name, HostType::Ssh)
                    .with_username(&conn.username)
                    .with_hostname(&conn.host)
                    .with_auth_label(auth_label)
                    .with_icon_color(icon_color);
                if os != RemoteOS::Unknown {
                    host.with_os_icon(os.icon())
                } else {
                    host
                }
            })
            .collect();

        // Create a default group containing all hosts
        let host_count = hosts.len() as u32;
        let groups = if host_count > 0 {
            vec![
                HostGroup::new("all-hosts", "All Hosts", host_count)
                    .with_icon_color(HostIconColor::Blue),
            ]
        } else {
            vec![]
        };

        (hosts, groups, config.saved_connections)
    }

    /// Render the search bar (matches picker.rs search style, 600px centered)
    fn render_search_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();
        // Background matching macos_sidebar theme search box
        let search_bg = if is_dark {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.08)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.06)
        };
        // Match picker.rs PANEL_WIDTH = 600.0
        const SEARCH_WIDTH: f32 = 600.0;
        div()
            .w_full()
            .px(px(12.0))
            .py(px(12.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .w(px(SEARCH_WIDTH))
                    .h(px(36.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .bg(search_bg)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(8.0))
                    .child(
                        // Search icon
                        div().w(px(16.0)).h(px(16.0)).child(
                            svg()
                                .size(px(16.0))
                                .text_color(theme.icon_muted)
                                .path(Icon::MagnifyingGlass.path()),
                        ),
                    )
                    .child(
                        // Input component
                        EditorInput::new(&self.search_input).transparent(),
                    )
                    .child(
                        // Shortcut hint
                        div()
                            .text_color(theme.text_subtle)
                            .text_size(px(12.0))
                            .child(if cfg!(target_os = "windows") { "Ctrl+K" } else { "⌘+K" }),
                    ),
            )
    }

    /// Render action buttons (NEW HOST, NEW KEY, NEW IDENTITY, TERMINAL, CONNECT HOSTS)
    fn render_action_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        // Unified button background matching macos_sidebar theme search box
        let button_bg = if cx.theme().appearance.is_dark() {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.08)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.06)
        };
        let button_bg_hover = if cx.theme().appearance.is_dark() {
            gpui::Hsla::from(gpui::rgb(0xFFFFFF)).opacity(0.12)
        } else {
            gpui::Hsla::from(gpui::rgb(0x000000)).opacity(0.10)
        };
        let button_variant = ButtonCustomVariant::new(cx)
            .color(button_bg)
            .foreground(theme.text)
            .hover(button_bg_hover)
            .active(button_bg);

        div().w_full().px(px(12.0)).flex().justify_center().child(
            div()
                .h(px(28.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    // NEW HOST button
                    Button::new("new-host-btn")
                        .label("NEW HOST")
                        .icon(ButtonIconType::new("icons/plus.svg"))
                        .small()
                        .custom(button_variant.clone())
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            if let Some(ref callback) = this.on_new_host_click {
                                callback(cx);
                            }
                        })),
                )
                .child(
                    // NEW KEY button
                    Button::new("new-key-btn")
                        .label("NEW KEY")
                        .icon(ButtonIconType::new("icons/plus.svg"))
                        .small()
                        .custom(button_variant.clone())
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            if let Some(ref callback) = this.on_new_key_click {
                                callback(cx, None);
                            }
                        })),
                )
                .child(
                    // NEW IDENTITY button
                    Button::new("new-identity-btn")
                        .label("NEW IDENTITY")
                        .icon(ButtonIconType::new("icons/plus.svg"))
                        .small()
                        .custom(button_variant.clone())
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            if let Some(ref callback) = this.on_new_identity_click {
                                callback(cx);
                            }
                        })),
                )
                .child(
                    // TERMINAL button
                    Button::new("terminal-btn")
                        .label("TERMINAL")
                        .icon(ButtonIconType::new("icons/terminal.svg"))
                        .small()
                        .custom(button_variant.clone())
                        .on_click(cx.listener(|this, _event, window, cx| {
                            if let Some(ref workspace) = this.workspace {
                                workspace.update(cx, |w, cx| {
                                    w.create_local_terminal(window, cx);
                                });
                            }
                        })),
                )
                .child(
                    // CONNECT HOSTS button
                    Button::new("connect-hosts-btn")
                        .label("CONNECT HOSTS")
                        .small()
                        .custom(button_variant)
                        .on_click(cx.listener(|this, _event, window, cx| {
                            // Connect to selected hosts or show picker
                            let selected_ids: Vec<_> =
                                this.selected_host_ids.iter().cloned().collect();
                            for host_id in selected_ids {
                                this.connect_to_host(&host_id, window, cx);
                            }
                        })),
                ),
        )
    }

    /// Render a group card
    fn render_group_card(&self, group: &HostGroup, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        let icon_color = group.icon_color.to_hsla(cx);

        div()
            .id(format!("group-{}", group.id))
            .w(px(MIN_CARD_WIDTH_PX))
            .max_w(px(MAX_CARD_WIDTH_PX))
            .h(px(76.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .rounded(px(10.0))
            .bg(icon_color.opacity(if cx.theme().appearance.is_dark() {
                0.08
            } else {
                0.05
            }))
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(move |style| style.bg(theme.tab_hover_background))
            .child(
                // Group icon
                div()
                    .w(px(44.0))
                    .h(px(44.0))
                    .rounded(px(8.0))
                    .bg(icon_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(24.0))
                            .path(Icon::Server.path())
                            .text_color(theme.accent_foreground),
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
                            .text_color(theme.text)
                            .child(group.name.to_string()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_subtle)
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

    /// Delete a host by its ID
    fn delete_host(&mut self, host_id: &str, cx: &mut Context<Self>) {
        // Remove from SSH config
        if let Ok(mut config) = SshGlobalConfig::load_or_create() {
            if config.remove_connection(host_id).is_some() {
                // Save the updated config
                let _ = config.save();
            }
        }
        // Refresh the view
        self.refresh_hosts(cx);
    }

    /// Set callback for when NEW HOST button is clicked
    pub fn on_new_host_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_new_host_click = Some(Box::new(callback));
    }

    /// Set callback for when a host card is clicked (single click to edit)
    pub fn on_edit_host_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App, SshConnectionConfig) + Send + Sync + 'static,
    {
        self.on_edit_host_click = Some(Box::new(callback));
    }

    /// Render a host card
    fn render_host_card(
        &self,
        host: &Host,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;
        let icon_color = host.icon_color.to_hsla(cx);
        let host_id = host.id.clone();
        let host_id_for_menu = host_id.clone();
        let is_selected = self.selected_host_ids.contains(&host_id);
        let bg_color = if is_selected {
            theme.accent.opacity(if cx.theme().appearance.is_dark() {
                0.18
            } else {
                0.10
            })
        } else {
            icon_color.opacity(if cx.theme().appearance.is_dark() {
                0.08
            } else {
                0.05
            })
        };
        let border_color = if is_selected {
            theme.accent
        } else {
            theme.border
        };

        div()
            .id(format!("host-{}", host.id))
            .w_full()
            .min_w(px(MIN_CARD_WIDTH_PX))
            .max_w(px(MAX_CARD_WIDTH_PX))
            .h(px(76.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .rounded(px(10.0))
            .bg(bg_color)
            .border_1()
            .border_color(border_color)
            .cursor_pointer()
            .hover(|style| {
                if is_selected {
                    style
                } else {
                    style.bg(theme.tab_hover_background)
                }
            })
            .on_click(cx.listener({
                let host_id = host_id.clone();
                move |this, event: &gpui::ClickEvent, window, cx| {
                    let is_cmd_pressed = event.modifiers().secondary(); // Cmd on macOS, Ctrl on other platforms

                    if event.click_count() == 1 {
                        if is_cmd_pressed {
                            // Cmd+click: toggle selection
                            if this.selected_host_ids.contains(&host_id) {
                                this.selected_host_ids.remove(&host_id);
                            } else {
                                this.selected_host_ids.insert(host_id.clone());
                            }
                        } else {
                            // Normal click: clear all and select only this one
                            this.selected_host_ids.clear();
                            this.selected_host_ids.insert(host_id.clone());
                        }
                        cx.notify();
                    } else if event.click_count() == 2 {
                        // Double click: connect to host (clear selection first for single connection)
                        this.selected_host_ids.clear();
                        this.selected_host_ids.insert(host_id.clone());
                        cx.notify();
                        this.connect_to_host(&host_id, window, cx);
                    }
                }
            }))
            // Select host on right mouse down (before context menu shows)
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener({
                    let host_id = host_id_for_menu.clone();
                    move |this, _event, _window, cx| {
                        // Track which host was right-clicked for context menu operations
                        this.context_menu_host_id = Some(host_id.clone());
                        // If right-clicked host is not already selected, select only this one
                        if !this.selected_host_ids.contains(&host_id) {
                            this.selected_host_ids.clear();
                            this.selected_host_ids.insert(host_id.clone());
                        }
                        cx.notify();
                    }
                }),
            )
            // Add context menu for right-click
            .context_menu({
                use crate::components::button::Icon as MenuIcon;
                move |menu: PopupMenu, _window, _cx| {
                    menu.menu_with_icon(
                        "Connect",
                        MenuIcon::new("icons/link.svg"),
                        Box::new(ConnectHost),
                    )
                    .separator()
                    .menu_with_icon("Edit", MenuIcon::new("icons/edit.svg"), Box::new(EditHost))
                    .menu("Duplicate", Box::new(DuplicateHost))
                    .separator()
                    .menu_with_icon(
                        "Delete",
                        MenuIcon::new("icons/trash.svg"),
                        Box::new(DeleteHost),
                    )
                }
            })
            .child(
                // Host icon
                div()
                    .w(px(44.0))
                    .h(px(44.0))
                    .rounded(px(8.0))
                    .bg(icon_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(24.0))
                            .path(host.os_icon.unwrap_or(Icon::Linux).path())
                            .text_color(theme.accent_foreground),
                    ),
            )
            .child(
                // Host info
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(host.name.to_string()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_subtle)
                            .child(host.subtitle()),
                    ),
            )
    }

    /// Render the Groups section
    fn render_groups_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
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
                    .text_color(theme.text_muted)
                    .child("Groups"),
            )
            .child(
                // Groups grid
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .gap(px(GRID_GAP_PX))
                    .children(
                        self.groups
                            .iter()
                            .map(|group| self.render_group_card(group, cx).into_any_element()),
                    ),
            )
    }

    /// Set sidebar widths for layout calculations
    pub fn set_sidebar_widths(&mut self, left: f32, right: f32) {
        self.left_sidebar_width = left;
        self.right_sidebar_width = right;
    }

    /// Calculate the number of columns based on available width
    fn calculate_columns(&self, available_width: f32) -> u16 {
        const MAX_CONTENT_WIDTH: f32 = 1200.0;

        // HostsView sits in SshNewTabView's flex_1 area; when right sidebar is open,
        // the remaining width is reduced by the sidebar width.
        let hosts_view_width = available_width - self.left_sidebar_width - self.right_sidebar_width;

        // Content container has max_w(1200.0), so cap to that to avoid over-counting columns
        let content_width = hosts_view_width.max(0.0).min(MAX_CONTENT_WIDTH);

        // Subtract section padding (24px on each side)
        let grid_width = (content_width - CONTENT_PADDING_PX * 2.0).max(0.0);

        // Calculate max possible columns
        let max_cols =
            ((grid_width + GRID_GAP_PX) / (MIN_CARD_WIDTH_PX + GRID_GAP_PX)).floor() as u16;

        // Clamp to at least 1 column
        max_cols.max(1)
    }

    /// Render the Hosts section with responsive flex wrap layout
    fn render_hosts_section(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(GRID_GAP_PX))
            .px(px(CONTENT_PADDING_PX))
            .child(
                // Section title with host count
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(format!("Hosts ({})", self.filtered_hosts.len())),
            )
            .child(self.render_hosts_grid(window, cx))
    }

    /// Render hosts grid with responsive columns and placeholders
    fn render_hosts_grid(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Get available width and calculate columns accounting for sidebars
        let viewport_width = window.viewport_size().width.as_f32();
        let cols = self.calculate_columns(viewport_width);

        let host_count = self.filtered_hosts.len();

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
            .children(
                self.filtered_hosts
                    .iter()
                    .map(|host| self.render_host_card(host, window, cx).into_any_element()),
            )
            // Add invisible placeholders to maintain grid alignment
            .children((0..placeholder_count).map(|_| div().w_full().h(px(76.0)).into_any_element()))
    }

    /// Render a keychain item card
    fn render_keychain_card(
        &self,
        item: &KeychainItem,
        icon: Icon,
        icon_color: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_selected = self.selected_keychain_item_id.as_ref() == Some(&item.id);
        let item_id = item.id.clone();
        let item_for_callback = item.clone();

        div()
            .id(format!("keychain-card-{}", item.id))
            .w_full()
            .min_w(px(MIN_CARD_WIDTH_PX))
            .max_w(px(MAX_CARD_WIDTH_PX))
            .h(px(76.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .rounded(px(10.0))
            .bg(if is_selected {
                theme.accent.opacity(if cx.theme().appearance.is_dark() {
                    0.18
                } else {
                    0.10
                })
            } else {
                icon_color.opacity(if cx.theme().appearance.is_dark() {
                    0.08
                } else {
                    0.05
                })
            })
            .border_1()
            .border_color(if is_selected {
                theme.accent
            } else {
                theme.border
            })
            .cursor_pointer()
            .hover(|style| {
                if is_selected {
                    style
                } else {
                    style.bg(theme.tab_hover_background)
                }
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.selected_keychain_item_id = Some(item_id.clone());
                    if let Some(ref callback) = this.on_edit_keychain_item_click {
                        callback(cx, item_for_callback.clone());
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(44.0))
                    .h(px(44.0))
                    .rounded(px(8.0))
                    .bg(icon_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(24.0))
                            .path(icon.path())
                            .text_color(theme.accent_foreground),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(item.label.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_subtle)
                            .child(item.subtitle()),
                    ),
            )
    }

    /// Render keychain items grid with responsive columns (same as hosts)
    fn render_keychain_grid(
        &self,
        window: &mut Window,
        items: &[&KeychainItem],
        icon: Icon,
        icon_color: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let viewport_width = window.viewport_size().width.as_f32();
        let cols = self.calculate_columns(viewport_width);

        let item_count = items.len();
        let remainder = item_count % (cols as usize);
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
            .children(items.iter().map(|item| {
                self.render_keychain_card(item, icon, icon_color, cx)
                    .into_any_element()
            }))
            // Add invisible placeholders to maintain grid alignment
            .children((0..placeholder_count).map(|_| div().w_full().h(px(76.0)).into_any_element()))
    }

    /// Render the Keys section
    fn render_keys_section(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let keys: Vec<&KeychainItem> = self
            .keychain_items
            .iter()
            .filter(|item| item.kind == KeychainItemKind::Key)
            .collect();

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(GRID_GAP_PX))
            .px(px(CONTENT_PADDING_PX))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(format!("Keys ({})", keys.len())),
            )
            .child(self.render_keychain_grid(window, &keys, Icon::KeyShield, theme.accent, cx))
    }

    /// Render the Identities section
    fn render_identities_section(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;
        let identities: Vec<&KeychainItem> = self
            .keychain_items
            .iter()
            .filter(|item| item.kind == KeychainItemKind::Identity)
            .collect();

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(GRID_GAP_PX))
            .px(px(CONTENT_PADDING_PX))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(format!("Identities ({})", identities.len())),
            )
            .child(self.render_keychain_grid(window, &identities, Icon::Host, theme.success, cx))
    }
}

impl Focusable for HostsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HostsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        // Header height: search bar (60) + spacer (20) + action buttons (28) = 108px
        const HEADER_HEIGHT: f32 = 108.0;
        // Persistent gap between action buttons bottom and scroll content
        const CONTENT_TOP_GAP: f32 = 16.0;
        // Total height the fixed header occupies (includes the gap)
        const TOTAL_HEADER_HEIGHT: f32 = HEADER_HEIGHT + CONTENT_TOP_GAP;

        div()
            .id("hosts-view")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .bg(theme.panel_background)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                if let Some(path) = paths.paths().first() {
                    this.selected_keychain_item_id = None;
                    if let Some(ref callback) = this.on_new_key_click {
                        callback(cx, Some(path.clone()));
                    }
                    log::info!("Dropped key path into hosts view: {:?}", path);
                }
            }))
            // Register context menu actions
            .on_action(cx.listener(|this, _: &ConnectHost, window, cx| {
                // Connect to all selected hosts
                let selected_ids: Vec<_> = this.selected_host_ids.iter().cloned().collect();
                for host_id in selected_ids {
                    this.connect_to_host(&host_id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &EditHost, _window, cx| {
                // Edit the host that was right-clicked, or the first selected host
                let host_id_to_edit = this
                    .context_menu_host_id
                    .clone()
                    .or_else(|| this.selected_host_ids.iter().next().cloned());

                if let Some(host_id) = host_id_to_edit {
                    if let Some(config) = this
                        .configs
                        .iter()
                        .find(|c| c.id() == host_id.as_ref())
                        .cloned()
                    {
                        if let Some(ref callback) = this.on_edit_host_click {
                            callback(cx, config);
                        }
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &DuplicateHost, _window, _cx| {
                // TODO: Implement duplicate host functionality for multiple hosts
                println!("Duplicate hosts: {:?}", this.selected_host_ids);
            }))
            .on_action(cx.listener(|this, _: &DeleteHost, window, cx| {
                // Delete all selected hosts
                let host_ids_to_delete: Vec<_> = this.selected_host_ids.iter().cloned().collect();
                if host_ids_to_delete.is_empty() {
                    return;
                }

                let host_count = host_ids_to_delete.len();
                let message = if host_count == 1 {
                    if let Some(host) = this.hosts.iter().find(|h| h.id == host_ids_to_delete[0]) {
                        format!("确定要删除主机 '{}' 吗？此操作不可撤销。", host.name)
                    } else {
                        "确定要删除选中的主机吗？此操作不可撤销。".to_string()
                    }
                } else {
                    format!("确定要删除选中的 {} 个主机吗？此操作不可撤销。", host_count)
                };

                let weak = cx.weak_entity();
                let answer = window.prompt(
                    gpui::PromptLevel::Warning,
                    "确认删除",
                    Some(&message),
                    &["删除", "取消"],
                    cx,
                );
                cx.spawn(async move |_, cx| {
                    if let Ok(0) = answer.await {
                        // User confirmed deletion (button index 0)
                        if let Some(entity) = weak.upgrade() {
                            let _ = entity.update(cx, |this, cx| {
                                for host_id in &host_ids_to_delete {
                                    this.delete_host(host_id, cx);
                                }
                            });
                        }
                    }
                })
                .detach();
            }))
            // Scrollable content (rendered first = lower z-order)
            // Fills the entire parent; top-padding reserves space for the fixed header
            .child(
                div()
                    .id("hosts-content")
                    .track_scroll(&self.scroll_handle)
                    .size_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(24.0))
                    // Top padding = total header area (header + gap), so content always starts below
                    .pt(px(TOTAL_HEADER_HEIGHT))
                    .pb(px(16.0))
                    .child(
                        div()
                            .w_full()
                            .max_w(px(1200.0))
                            .flex()
                            .flex_col()
                            .gap(px(24.0))
                            // Groups section
                            .child(self.render_groups_section(cx))
                            // Hosts section
                            .child(self.render_hosts_section(window, cx))
                            // Keys section
                            .child(self.render_keys_section(window, cx))
                            // Identities section
                            .child(self.render_identities_section(window, cx)),
                    ),
            )
            // Fixed header (rendered last = highest z-order, covers scrolled content)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(TOTAL_HEADER_HEIGHT))
                    .flex()
                    .flex_col()
                    .bg(theme.panel_background)
                    // Search bar
                    .child(self.render_search_bar(cx))
                    // Spacer between search bar and action buttons
                    .child(div().h(px(20.0)))
                    // Action buttons
                    .child(self.render_action_buttons(cx)),
            )
    }
}
