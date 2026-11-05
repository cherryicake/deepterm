//! 设置文档的强类型表示。
//!
//! 每个子系统一个结构体，字段全部带 `#[serde(default)]`，因此用户文件里
//! 缺字段、多字段都不会导致解析失败——缺失项由默认文档兜底，未知项忽略。
//!
//! 与 Zed 的差异：Zed 用 `#[serde(flatten)]` 把各子系统的字段摊平到顶层。
//! 这里保留嵌套命名空间（`theme.id` 而不是顶层 `theme_id`），因为 deepterm
//! 的子系统的字段名冲突概率更高（例如 font 与 editor 都有字号），
//! 嵌套结构读起来也更清晰。

use serde::{Deserialize, Serialize};

use super::store::Settings;
pub use crate::lsp::registry::LspContent;

/// 一份完整的设置文档（默认值与用户覆盖共用此类型）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsContent {
    pub theme: ThemeContent,
    pub font: FontContent,
    pub editor: EditorContent,
    pub terminal: TerminalContent,
    pub vim: VimContent,
    /// 语言服务器配置。缺省时走 [`crate::lsp::registry`] 的内置默认表。
    pub lsp: LspContent,
}

impl SettingsContent {
    /// 用 `other` 覆盖 `self`：`Some` 覆盖、`None` 跳过。
    ///
    /// 各字段都是非 `Option` 的强类型结构体，所以这里的语义等价于
    /// 「整字段替换」。之所以仍写成显式函数而不是直接赋值，是为了在
    /// 引入 `Option<T>` 字段时有一个明确的落点，也便于单元测试固定语义。
    pub fn merge_from(&mut self, other: &Self) {
        merge(&mut self.theme, &other.theme);
        merge(&mut self.font, &other.font);
        merge(&mut self.editor, &other.editor);
        merge(&mut self.terminal, &other.terminal);
        merge(&mut self.vim, &other.vim);
        merge(&mut self.lsp, &other.lsp);
    }
}

/// 递归合并的占位实现：当前所有设置结构体都是「整体覆盖」语义。
fn merge<T: Clone>(target: &mut T, source: &T) {
    *target = source.clone();
}

// ============================== theme ==============================

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeContent {
    /// 活动主题 id；为空表示跟随系统外观。
    pub id: String,
}

// ============================== font ==============================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontContent {
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

impl Default for FontContent {
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

/// 获取 Linux 系统默认等宽字体名称（带缓存）。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn get_system_monospace_font() -> &'static str {
    use fontconfig::Fontconfig;
    use std::sync::OnceLock;

    static SYSTEM_MONO_FONT: OnceLock<String> = OnceLock::new();
    SYSTEM_MONO_FONT.get_or_init(|| {
        if let Some(fc) = Fontconfig::new()
            && let Some(font) = fc.find("monospace", None)
        {
            return font.name;
        }
        "DejaVu Sans Mono".to_string()
    })
}

// ============================== editor ==============================

/// 代码编辑器的默认行为。这些值原先硬编码在 `InputState` 的构造与
/// `Editor` 的渲染路径里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorContent {
    /// 光标形状：`bar` / `block` / `underline` / `hollow`。
    pub cursor_shape: String,
    pub cursor_blink: bool,
    /// 软换行。对齐 Zed 的 `soft_wrap` 默认值。
    pub soft_wrap: bool,
    pub tab_size: usize,
    pub hard_tabs: bool,
    pub line_number: bool,
    pub folding: bool,
    pub indent_guides: bool,
}

impl Default for EditorContent {
    fn default() -> Self {
        Self {
            cursor_shape: "bar".to_string(),
            cursor_blink: true,
            soft_wrap: false,
            tab_size: 4,
            hard_tabs: false,
            line_number: true,
            folding: true,
            indent_guides: true,
        }
    }
}

// ============================== terminal ==============================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalContent {
    /// 终端回滚缓冲区行数。
    pub scrollback_lines: usize,
    /// 光标形状：`bar` / `block` / `underline`。
    pub cursor_shape: String,
}

impl Default for TerminalContent {
    fn default() -> Self {
        Self {
            scrollback_lines: 10_000,
            cursor_shape: "bar".to_string(),
        }
    }
}

// ============================== vim ==============================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VimContent {
    pub enabled: bool,
    /// 进入编辑器时的初始模式：`normal` / `insert`。
    pub default_mode: String,
    /// Normal 模式光标形状。
    pub normal_cursor_shape: String,
    /// 是否随模式切换相对行号。
    pub relative_line_numbers: bool,
}

impl Default for VimContent {
    fn default() -> Self {
        Self {
            enabled: false,
            default_mode: "normal".to_string(),
            normal_cursor_shape: "block".to_string(),
            relative_line_numbers: false,
        }
    }
}

// ============================== 注册 ==============================
//
// 每个子系统都能作为独立类型被 `Settings::get_global` 读取。`from_settings`
// 直接克隆自身段落——这些结构体已经是「最终形态」，不需要再做派生。

impl Settings for ThemeContent {
    fn from_settings(content: &SettingsContent) -> Self {
        content.theme.clone()
    }
}

impl Settings for FontContent {
    fn from_settings(content: &SettingsContent) -> Self {
        content.font.clone()
    }
}

impl Settings for EditorContent {
    fn from_settings(content: &SettingsContent) -> Self {
        content.editor.clone()
    }
}

impl Settings for TerminalContent {
    fn from_settings(content: &SettingsContent) -> Self {
        content.terminal.clone()
    }
}

impl Settings for VimContent {
    fn from_settings(content: &SettingsContent) -> Self {
        content.vim.clone()
    }
}

impl Settings for LspContent {
    fn from_settings(content: &SettingsContent) -> Self {
        content.lsp.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let json = serde_json::to_string_pretty(&SettingsContent::default()).unwrap();
        let parsed: SettingsContent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SettingsContent::default());
    }

    #[test]
    fn partial_user_document_keeps_defaults() {
        // 用户只写了一个字段，其余必须回落到默认值。
        let parsed: SettingsContent =
            serde_json::from_str(r#"{ "font": { "editor_font_size": 20.0 } }"#).unwrap();
        assert_eq!(parsed.font.editor_font_size, 20.0);
        assert_eq!(
            parsed.font.terminal_font_size,
            SettingsContent::default().font.terminal_font_size
        );
        assert!(!parsed.vim.enabled);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let parsed: SettingsContent =
            serde_json::from_str(r#"{ "font": { "nope": 1 }, "unknown_section": {} }"#).unwrap();
        assert_eq!(parsed, SettingsContent::default());
    }

    #[test]
    fn merge_overrides_only_present_sections() {
        let mut base = SettingsContent::default();
        let user: SettingsContent =
            serde_json::from_str(r#"{ "vim": { "enabled": true } }"#).unwrap();
        base.merge_from(&user);
        assert!(base.vim.enabled);
        // font 段未出现在用户文档里，合并后仍是默认值。
        assert_eq!(base.font, SettingsContent::default().font);
    }

    #[test]
    fn each_section_is_readable_as_its_own_type() {
        let mut content = SettingsContent::default();
        content.vim.enabled = true;
        content.editor.tab_size = 2;
        assert!(VimContent::from_settings(&content).enabled);
        assert_eq!(EditorContent::from_settings(&content).tab_size, 2);
    }
}
