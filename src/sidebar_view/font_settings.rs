use gpui::{App, Global, SharedString};
use serde::{Deserialize, Serialize};

use crate::settings::SettingsStore;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FontSettings {
    #[serde(default = "default_terminal_font_family")]
    pub terminal_font_family: String,

    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: f32,

    #[serde(default = "default_terminal_line_height")]
    pub terminal_line_height: f32,

    #[serde(default = "default_terminal_font_weight")]
    pub terminal_font_weight: f32,

    /// 代码编辑器正文的字体族（独立于终端字体与 UI 字体）。
    ///
    /// 缩进参考线、列对齐都以等宽字体为前提，所以默认取平台等宽字体，
    /// 而不是 UI 的比例字体（SF Pro 等）。
    #[serde(default = "default_editor_font_family")]
    pub editor_font_family: String,

    /// 代码编辑器正文的字号。
    #[serde(default = "default_editor_font_size")]
    pub editor_font_size: f32,
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            terminal_font_family: default_terminal_font_family(),
            terminal_font_size: default_terminal_font_size(),
            terminal_line_height: default_terminal_line_height(),
            terminal_font_weight: default_terminal_font_weight(),
            editor_font_family: default_editor_font_family(),
            editor_font_size: default_editor_font_size(),
        }
    }
}

impl FontSettings {
    /// 从合并后的设置文档取值。
    pub fn from_settings(content: &crate::settings::SettingsContent) -> Self {
        Self {
            terminal_font_family: content.font.terminal_font_family.clone(),
            terminal_font_size: content.font.terminal_font_size,
            terminal_line_height: content.font.terminal_line_height,
            terminal_font_weight: content.font.terminal_font_weight,
            editor_font_family: content.font.editor_font_family.clone(),
            editor_font_size: content.font.editor_font_size,
        }
    }

    /// 转回设置文档的 `font` 段，用于写回。
    pub fn into_content(self) -> crate::settings::FontContent {
        crate::settings::FontContent {
            terminal_font_family: self.terminal_font_family,
            terminal_font_size: self.terminal_font_size,
            terminal_line_height: self.terminal_line_height,
            terminal_font_weight: self.terminal_font_weight,
            editor_font_family: self.editor_font_family,
            editor_font_size: self.editor_font_size,
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

pub fn default_terminal_font_family() -> String {
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

pub fn default_terminal_font_size() -> f32 {
    18.0
}

pub fn default_terminal_line_height() -> f32 {
    1.3
}

pub fn default_terminal_font_weight() -> f32 {
    400.0
}

/// 编辑器默认字体 = 平台默认等宽字体（与终端默认一致）。
pub fn default_editor_font_family() -> String {
    default_terminal_font_family()
}

pub fn default_editor_font_size() -> f32 {
    16.0
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

/// 字体设置的只读快照。
///
/// 数据的唯一真源是 [`crate::settings::SettingsStore`]；本类型不再自己读写
/// 磁盘，只作为「渲染路径便捷取用」的门面。这样字体的读写与其它设置走同一条
/// 路径（热重载、差异写回、未知键保留都自动生效）。
#[derive(Clone, Debug)]
pub struct FontState {
    pub settings: FontSettings,
}

impl Global for FontState {}

impl FontState {
    /// 安装字体快照。
    ///
    /// 设置 store 缺席时退回内置默认值，而不是 panic：测试与独立渲染
    /// 组件会只初始化本类型。这比旧行为更可复现——旧实现会去读开发者
    /// 机器上的真实 `font.json`。
    pub fn init(cx: &mut App) {
        let settings = cx
            .try_global::<SettingsStore>()
            .map(|store| FontSettings::from_settings(store.content()))
            .unwrap_or_default();
        cx.set_global(FontState { settings });
    }

    /// 从设置 store 刷新快照。`SettingsStore` 变化后由订阅方调用。
    pub fn sync_from_settings(cx: &mut App) {
        let settings = FontSettings::from_settings(cx.global::<SettingsStore>().content());
        // 先比较再取可变引用：`global_mut` 会无条件通知观察者，
        // 设置里改的是别的段落时不该惊动字体的订阅方。
        if cx.global::<FontState>().settings == settings {
            return;
        }
        cx.global_mut::<FontState>().settings = settings;
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// 修改字体设置：写回 `settings.json`，并同步本快照。
    pub fn update<F>(cx: &mut App, f: F)
    where
        F: FnOnce(&mut FontSettings),
    {
        if cx.try_global::<SettingsStore>().is_none() {
            // 无 store 时只改快照，不落盘（测试路径）。
            let state = cx.global_mut::<FontState>();
            f(&mut state.settings);
            cx.refresh_windows();
            return;
        }

        SettingsStore::update_user(cx, |content| {
            let mut font = FontSettings::from_settings(content);
            f(&mut font);
            content.font = font.into_content();
        });
        Self::sync_from_settings(cx);
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
