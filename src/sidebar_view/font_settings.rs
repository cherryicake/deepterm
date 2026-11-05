use gpui::SharedString;

pub use crate::settings::FontState;
/// 字体设置的兼容别名；设置真源是 [`crate::settings::FontContent`]。
pub use crate::settings::{
    FontContent as FontSettings, default_editor_font_family, default_editor_font_size,
    default_terminal_font_family, default_terminal_font_size, default_terminal_font_weight,
    default_terminal_line_height,
};

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

/// 平台默认终端字体族，用于回退
pub fn platform_default_font_family() -> SharedString {
    default_terminal_font_family().into()
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
