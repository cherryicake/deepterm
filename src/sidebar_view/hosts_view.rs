use gpui::{App, SharedString};
use gpui::actions;

actions!(
    host_actions,
    [ConnectHost, EditHost, CopyHostAddress, DeleteHost]
);

use crate::assets::Icon;
use crate::ssh::config::{AuthMethod, SshConnectionConfig, SshGlobalConfig};
use crate::ssh::keychain::{KeychainKeyFlavor, KeychainStore};
use crate::ssh::platform::{RemoteOS, guess_os_from_text};
use crate::theme::ActiveTheme;

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


/// Load hosts from SSH config file, returning hosts and configs
pub fn load_hosts_from_config() -> (Vec<Host>, Vec<SshConnectionConfig>) {
    let config = SshGlobalConfig::load();
    let keychain = KeychainStore::load_or_default();

    let hosts: Vec<Host> = config
        .saved_connections
        .iter()
        .map(|conn| {
            let display_name = conn.nickname.clone().unwrap_or_else(|| conn.display_name());

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

    (hosts, config.saved_connections)
}
