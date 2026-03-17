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
    Linux,
    Server,
    Screen,
    Terminal,
    MagnifyingGlass,
    BroadcastInput,
    WindowClose,
    SettingsHub,
    Plus,
    Host,
    KeyShield,
    ChevronDown,
}

impl Icon {
    /// Get the asset path for this icon
    pub fn path(&self) -> SharedString {
        // Use new_static to avoid allocation - these are compile-time constants
        match self {
            Icon::Linux => SharedString::new_static("icons/linux.svg"),
            Icon::Server => SharedString::new_static("icons/server.svg"),
            Icon::Screen => SharedString::new_static("icons/screen.svg"),
            Icon::Terminal => SharedString::new_static("icons/terminal.svg"),
            Icon::MagnifyingGlass => SharedString::new_static("icons/magnifying_glass.svg"),
            Icon::BroadcastInput => SharedString::new_static("icons/broadcast_input.svg"),
            Icon::WindowClose => SharedString::new_static("icons/window-close.svg"),
            Icon::SettingsHub => SharedString::new_static("icons/settingshub.svg"),
            Icon::Plus => SharedString::new_static("icons/plus.svg"),
            Icon::Host => SharedString::new_static("icons/host.svg"),
            Icon::KeyShield => SharedString::new_static("icons/key_shield.svg"),
            Icon::ChevronDown => SharedString::new_static("icons/chevron_down.svg"),
        }
    }

    /// Get the icon color based on icon type
    pub fn color(&self) -> gpui::Rgba {
        match self {
            Icon::Linux => gpui::rgb(0xaaaaaa),
            Icon::Server => gpui::rgb(0x8888aa),
            Icon::Screen => gpui::rgb(0xaaaaaa),
            Icon::Terminal => gpui::rgb(0xaaaaaa),
            Icon::MagnifyingGlass => gpui::rgb(0x666688),
            Icon::BroadcastInput => gpui::rgb(0x8888aa),
            Icon::WindowClose => gpui::rgb(0x8888aa),
            Icon::SettingsHub => gpui::rgb(0x8888aa),
            Icon::Plus => gpui::rgb(0x666688),
            Icon::Host => gpui::rgb(0x8888aa),
            Icon::KeyShield => gpui::rgb(0x8888aa),
            Icon::ChevronDown => gpui::rgb(0x8888aa),
        }
    }
}

impl From<Icon> for SharedString {
    fn from(icon: Icon) -> Self {
        icon.path()
    }
}
