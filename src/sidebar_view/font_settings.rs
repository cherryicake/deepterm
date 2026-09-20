use std::path::PathBuf;

use gpui::{App, Global, SharedString};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FontSettings {
    #[serde(default = "default_terminal_font_family")]
    pub terminal_font_family: String,

    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: f32,

    #[serde(default = "default_terminal_line_height")]
    pub terminal_line_height: f32,

    #[serde(default = "default_terminal_font_weight")]
    pub terminal_font_weight: f32,
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            terminal_font_family: default_terminal_font_family(),
            terminal_font_size: default_terminal_font_size(),
            terminal_line_height: default_terminal_line_height(),
            terminal_font_weight: default_terminal_font_weight(),
        }
    }
}

pub const DEFAULT_FONT_SUFFIX: &str = " (default)";

pub fn format_font_choice(family: &str, is_default: bool) -> String {
    if is_default {
        format!("{}{}", family, DEFAULT_FONT_SUFFIX)
    } else {
        family.to_string()
    }
}

pub fn clean_font_choice(label: &str) -> &str {
    label.trim_end_matches(DEFAULT_FONT_SUFFIX)
}

fn default_terminal_font_family() -> String {
    #[cfg(target_os = "macos")]
    {
        "Menlo".into()
    }
    #[cfg(target_os = "windows")]
    {
        "Consolas".into()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        get_system_monospace_font().into()
    }
}

fn default_terminal_font_size() -> f32 {
    18.0
}

fn default_terminal_line_height() -> f32 {
    1.3
}

fn default_terminal_font_weight() -> f32 {
    400.0
}

/// 获取 Linux 系统默认等宽字体名称（带缓存）
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn get_system_monospace_font() -> &'static str {
    use fontconfig::Fontconfig;
    use std::sync::OnceLock;

    static SYSTEM_MONO_FONT: OnceLock<String> = OnceLock::new();
    SYSTEM_MONO_FONT.get_or_init(|| {
        if let Some(fc) = Fontconfig::new() {
            if let Some(font) = fc.find("monospace", None) {
                return font.name;
            }
        }
        "DejaVu Sans Mono".to_string()
    })
}

impl FontSettings {
    pub fn load() -> Self {
        let path = match Self::config_path() {
            Some(p) => p,
            None => return Self::default(),
        };
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    fn config_path() -> Option<PathBuf> {
        let config_dir = dirs::config_dir()?;
        Some(config_dir.join("deepterm").join("font.json"))
    }
}

#[derive(Clone, Debug)]
pub struct FontState {
    pub settings: FontSettings,
}

impl Global for FontState {}

impl FontState {
    pub fn init(cx: &mut App) {
        let settings = FontSettings::load();
        cx.set_global(FontState { settings });
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn update<F>(cx: &mut App, f: F)
    where
        F: FnOnce(&mut FontSettings),
    {
        let state = cx.global_mut::<FontState>();
        f(&mut state.settings);
        if let Err(e) = state.settings.save() {
            log::warn!("failed to save font settings: {e}");
        }
        cx.refresh_windows();
    }
}

/// 平台默认终端字体族，用于回退
pub fn platform_default_font_family() -> SharedString {
    #[cfg(target_os = "macos")]
    {
        "Menlo".into()
    }
    #[cfg(target_os = "windows")]
    {
        "Consolas".into()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        get_system_monospace_font().into()
    }
}

/// 平台默认字体回退列表。
///
/// 结果缓存在 `LazyLock` 里：`FontFallbacks` 本体就是 `Arc<Vec<String>>`
/// 的包装（`Clone` 只做引用计数自增），而调用方里有**每帧都调**的渲染
/// 路径（`TerminalElement::prepaint`）。此前每次调用都重新构造一遍
/// `Vec<String>`（macOS 7 个、Linux 8 个 `String` + 一次 `Vec` + 一次
/// `Arc` 分配），在每帧的 prepaint 里是纯浪费。
pub fn platform_default_fallbacks() -> gpui::FontFallbacks {
    static FALLBACKS: std::sync::LazyLock<gpui::FontFallbacks> =
        std::sync::LazyLock::new(build_platform_default_fallbacks);
    FALLBACKS.clone()
}

fn build_platform_default_fallbacks() -> gpui::FontFallbacks {
    #[cfg(target_os = "macos")]
    {
        gpui::FontFallbacks::from_fonts(vec![
            "Monaco".to_string(),
            "Courier New".to_string(),
            "Sarasa Mono SC".to_string(),
            "Maple Mono CN".to_string(),
            "Noto Sans Mono CJK SC".to_string(),
            "Source Han Mono SC".to_string(),
            "JetBrainsMono Nerd Font".to_string(),
        ])
    }
    #[cfg(target_os = "windows")]
    {
        gpui::FontFallbacks::from_fonts(vec![
            "Courier New".to_string(),
            "Sarasa Mono SC".to_string(),
            "Maple Mono CN".to_string(),
            "Noto Sans Mono CJK SC".to_string(),
            "Source Han Mono SC".to_string(),
            "JetBrainsMono Nerd Font".to_string(),
        ])
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        gpui::FontFallbacks::from_fonts(vec![
            "Noto Sans Mono".to_string(),
            "Source Code Pro".to_string(),
            "DejaVu Sans Mono".to_string(),
            "Sarasa Mono SC".to_string(),
            "Maple Mono CN".to_string(),
            "Noto Sans Mono CJK SC".to_string(),
            "Source Han Mono SC".to_string(),
            "JetBrainsMono Nerd Font".to_string(),
        ])
    }
}
