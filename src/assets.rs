//! Embedded assets for DeepTerm
//!
//! Provides embedded SVG icons using rust_embed.

use anyhow::anyhow;
use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// Native implementation using RustEmbed
#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Self::get(path)
            .map(|f| Some(f.data))
            .ok_or_else(|| anyhow!("asset not found: {}", path))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect())
    }
}

/// Icon names for embedded SVG assets
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Folder,
    File,
    Linux,
    Server,
    Screen,
    Terminal,
    MagnifyingGlass,
    BroadcastInput,
    WindowClose,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    SettingsHub,
    Plus,
    Host,
    KeyShield,
    ChevronDown,
    ChevronUpDown,
    ChevronLeft,
    Eye,
    EyeOff,
    Loader,
    ProgressIndicator,
    // OS icons
    Macos,
    Ubuntu,
    Debian,
    Centos,
    Arch,
    Windows,
    LinuxMint,
    Fedora,
    Rhel,
    Opensuse,
    Manjaro,
    Workspace,
    Bell,
    Reveal,
    Sidebar,
    Theme,
    Font,
    PlusCircleFull,
    PersonBadgeKey,
    Clock,
    FolderOutline,
}

impl Icon {
    /// Get the asset path for this icon
    pub fn path(&self) -> SharedString {
        // Use new_static to avoid allocation - these are compile-time constants
        match self {
            Icon::Folder => SharedString::new_static("icons/folder.fill.svg"),
            Icon::File => SharedString::new_static("icons/file.svg"),
            Icon::Linux => SharedString::new_static("icons/linux.svg"),
            Icon::Server => SharedString::new_static("icons/server.svg"),
            Icon::Screen => SharedString::new_static("icons/screen.svg"),
            Icon::Terminal => SharedString::new_static("icons/terminal.svg"),
            Icon::MagnifyingGlass => SharedString::new_static("icons/magnifying_glass.svg"),
            Icon::BroadcastInput => SharedString::new_static("icons/broadcast_input.svg"),
            Icon::WindowClose => SharedString::new_static("icons/window-close.svg"),
            Icon::WindowMinimize => SharedString::new_static("icons/window-minimize.svg"),
            Icon::WindowMaximize => SharedString::new_static("icons/window-maximize.svg"),
            Icon::WindowRestore => SharedString::new_static("icons/window-restore.svg"),
            Icon::SettingsHub => SharedString::new_static("icons/settingshub.svg"),
            Icon::Plus => SharedString::new_static("icons/plus.svg"),
            Icon::Host => SharedString::new_static("icons/host.svg"),
            Icon::KeyShield => SharedString::new_static("icons/key_shield.svg"),
            Icon::ChevronDown => SharedString::new_static("icons/chevron_down.svg"),
            Icon::ChevronUpDown => SharedString::new_static("icons/chevron_up_down.svg"),
            Icon::ChevronLeft => SharedString::new_static("icons/chevron_left.svg"),
            Icon::Eye => SharedString::new_static("icons/eye.svg"),
            Icon::EyeOff => SharedString::new_static("icons/eye_off.svg"),
            Icon::Loader => SharedString::new_static("icons/loader.svg"),
            Icon::ProgressIndicator => SharedString::new_static("icons/progress.indicator.svg"),
            Icon::Macos => SharedString::new_static("icons/macos.svg"),
            Icon::Ubuntu => SharedString::new_static("icons/ubuntu.svg"),
            Icon::Debian => SharedString::new_static("icons/debian.svg"),
            Icon::Centos => SharedString::new_static("icons/centos.svg"),
            Icon::Arch => SharedString::new_static("icons/arch.svg"),
            Icon::Windows => SharedString::new_static("icons/windows.svg"),
            Icon::LinuxMint => SharedString::new_static("icons/linuxmint.svg"),
            Icon::Fedora => SharedString::new_static("icons/fedora.svg"),
            Icon::Rhel => SharedString::new_static("icons/rhel.svg"),
            Icon::Opensuse => SharedString::new_static("icons/opensuse.svg"),
            Icon::Manjaro => SharedString::new_static("icons/manjaro.svg"),
            Icon::Workspace => SharedString::new_static("icons/workspace.svg"),
            Icon::Bell => SharedString::new_static("icons/bell.svg"),
            Icon::Reveal => SharedString::new_static("icons/reveal.svg"),
            Icon::Sidebar => SharedString::new_static("icons/sidebar.svg"),
            Icon::Theme => SharedString::new_static("icons/theme.svg"),
            Icon::Font => SharedString::new_static("icons/font.svg"),
            Icon::PlusCircleFull => SharedString::new_static("icons/plus.circle.full.svg"),
            Icon::PersonBadgeKey => SharedString::new_static("icons/person.badge.key.fill.svg"),
            Icon::Clock => SharedString::new_static("icons/clock.svg"),
            Icon::FolderOutline => SharedString::new_static("icons/folder.svg"),
        }
    }
}

impl From<Icon> for SharedString {
    fn from(icon: Icon) -> Self {
        icon.path()
    }
}
