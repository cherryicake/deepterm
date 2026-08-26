use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::vte::ansi::NamedColor;
use gpui::{App, Global, Hsla, Pixels, SharedString, WindowAppearance, px, rgb};

pub const CLIENT_SIDE_DECORATION_ROUNDING: Pixels = px(10.0);
pub const CLIENT_SIDE_DECORATION_SHADOW: Pixels = px(10.0);
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::editor::syntax::{
    HighlightTheme, HighlightThemeStyle, StatusColors, SyntaxColors, ThemeStyle,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeAppearance {
    Light,
    #[default]
    Dark,
}

impl ThemeAppearance {
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UiTheme {
    pub app_background: Hsla,
    pub panel_background: Hsla,
    pub elevated_background: Hsla,
    pub overlay_background: Hsla,
    pub title_bar_background: Hsla,
    pub title_bar_hover: Hsla,
    pub tab_bar_background: Hsla,
    pub tab_active_background: Hsla,
    pub tab_inactive_background: Hsla,
    pub tab_hover_background: Hsla,
    /// 激活标签的描边色。历史行为是白色半透明（暗色主题 10%、亮色主题 60%），
    /// 现作为主题字段保留该默认值，可被主题文件覆盖。
    pub tab_active_border: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_subtle: Hsla,
    pub icon: Hsla,
    pub icon_muted: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_foreground: Hsla,
    pub selection: Hsla,
    pub drop_target: Hsla,
    pub drop_target_border: Hsla,
    pub success: Hsla,
    pub success_background: Hsla,
    pub success_foreground: Hsla,
    pub warning: Hsla,
    pub warning_background: Hsla,
    pub error: Hsla,
    pub error_background: Hsla,
    pub error_foreground: Hsla,
    pub notification_background: Hsla,
    /// 弹出菜单（PopupMenu / MacosSelect 下拉）的浮层背景色。
    pub menu_background: Hsla,
    /// 弹出菜单浮层的描边色。
    pub menu_border: Hsla,
    /// 弹出菜单内部分隔线的颜色。
    pub menu_separator: Hsla,
    pub shadow: Hsla,
    // Git status colors
    pub status_modified: Hsla,
    pub status_added: Hsla,
    pub status_deleted: Hsla,
    pub status_conflict: Hsla,
}

impl UiTheme {
    pub fn transparent(&self) -> Hsla {
        gpui::transparent_black()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalTheme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub bright_foreground: Hsla,
    pub dim_foreground: Hsla,
    pub cursor: Hsla,
    pub selection_background: Hsla,
    pub ime_background: Hsla,
    pub ime_underline: Hsla,
    normal: [Hsla; 8],
    bright: [Hsla; 8],
    dim: [Hsla; 8],
    /// Pre-resolved 256-entry ANSI palette. Entries 0..=15 mirror the named
    /// colors (normal/bright); 16..=255 follow the standard xterm 6×6×6 cube
    /// and grayscale ramp. Built once at theme construction so that per-cell
    /// color conversion in the render path is an O(1) array lookup instead of
    /// recomputing the `Rgb -> Hsla` conversion for every indexed cell.
    indexed_palette: [Hsla; 256],
}

impl TerminalTheme {
    pub fn named_color(&self, named: NamedColor) -> Hsla {
        match named {
            NamedColor::Black => self.normal[0],
            NamedColor::Red => self.normal[1],
            NamedColor::Green => self.normal[2],
            NamedColor::Yellow => self.normal[3],
            NamedColor::Blue => self.normal[4],
            NamedColor::Magenta => self.normal[5],
            NamedColor::Cyan => self.normal[6],
            NamedColor::White => self.normal[7],
            NamedColor::BrightBlack => self.bright[0],
            NamedColor::BrightRed => self.bright[1],
            NamedColor::BrightGreen => self.bright[2],
            NamedColor::BrightYellow => self.bright[3],
            NamedColor::BrightBlue => self.bright[4],
            NamedColor::BrightMagenta => self.bright[5],
            NamedColor::BrightCyan => self.bright[6],
            NamedColor::BrightWhite => self.bright[7],
            NamedColor::Foreground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimBlack => self.dim[0],
            NamedColor::DimRed => self.dim[1],
            NamedColor::DimGreen => self.dim[2],
            NamedColor::DimYellow => self.dim[3],
            NamedColor::DimBlue => self.dim[4],
            NamedColor::DimMagenta => self.dim[5],
            NamedColor::DimCyan => self.dim[6],
            NamedColor::DimWhite => self.dim[7],
            NamedColor::BrightForeground => self.bright_foreground,
            NamedColor::DimForeground => self.dim_foreground,
        }
    }

    /// Look up a pre-resolved entry of the 256-color ANSI palette.
    /// Used by the render path for `Indexed` colors, avoiding per-cell
    /// `Rgb -> Hsla` recomputation.
    pub fn indexed_color(&self, idx: u8) -> Hsla {
        self.indexed_palette[idx as usize]
    }

    /// Build the 256-entry palette from the theme's named colors and the
    /// standard xterm 6×6×6 color cube + grayscale ramp formulas.
    fn build_indexed_palette(normal: [Hsla; 8], bright: [Hsla; 8]) -> [Hsla; 256] {
        let mut palette = [gpui::transparent_black(); 256];
        // 0..=15: standard ANSI colors mirror the named normal/bright colors.
        palette[0..8].copy_from_slice(&normal);
        palette[8..16].copy_from_slice(&bright);
        // 16..=231: 6×6×6 color cube.
        for idx in 16..=231u16 {
            let i = idx - 16;
            let r = (i / 36) as u8;
            let g = ((i % 36) / 6) as u8;
            let b = (i % 6) as u8;
            let r = if r == 0 { 0 } else { r * 40 + 55 };
            let g = if g == 0 { 0 } else { g * 40 + 55 };
            let b = if b == 0 { 0 } else { b * 40 + 55 };
            let packed = (r as u32) << 16 | (g as u32) << 8 | b as u32;
            palette[idx as usize] = rgb(packed).into();
        }
        // 232..=255: grayscale ramp.
        for idx in 232..=255u16 {
            let gray = 8 + (idx - 232) as u8 * 10;
            let packed = (gray as u32) << 16 | (gray as u32) << 8 | gray as u32;
            palette[idx as usize] = rgb(packed).into();
        }
        palette
    }

    pub fn preview_colors(&self) -> [Hsla; 16] {
        [
            self.normal[0],
            self.normal[1],
            self.normal[2],
            self.normal[3],
            self.normal[4],
            self.normal[5],
            self.normal[6],
            self.normal[7],
            self.bright[0],
            self.bright[1],
            self.bright[2],
            self.bright[3],
            self.bright[4],
            self.bright[5],
            self.bright[6],
            self.bright[7],
        ]
    }

    /// Generate a highlight theme from terminal ANSI colors.
    pub fn to_highlight_theme(
        &self,
        name: &str,
        appearance: ThemeAppearance,
    ) -> Arc<HighlightTheme> {
        let (editor_bg, editor_fg, active_line, line_number, active_line_number, invisible) = (
            Some(self.background),
            Some(self.foreground),
            Some(gpui::Hsla::from(gpui::Rgba::from(self.background).blend(
                if appearance.is_dark() {
                    gpui::rgba(0xFFFFFF10)
                } else {
                    gpui::rgba(0x0000000C)
                },
            ))),
            Some(self.dim_foreground),
            Some(self.foreground),
            Some(self.dim_foreground),
        );

        let syntax: SyntaxColors = if appearance.is_dark() {
            // Default Dark: approximate One Dark / gpui-component Default Dark
            SyntaxColors {
                attribute: Some(ThemeStyle::color(rgb(0x74ade8).into())),
                boolean: Some(ThemeStyle::color(rgb(0xbf956a).into())),
                comment: Some(ThemeStyle::color(rgb(0x5d636f).into())),
                comment_doc: Some(ThemeStyle::color(rgb(0x878e98).into())),
                constant: Some(ThemeStyle::color(rgb(0xdfc184).into())),
                constructor: Some(ThemeStyle::color(rgb(0x73ade9).into())),
                embedded: Some(ThemeStyle::color(editor_fg.unwrap())),
                // VSCode renders markdown emphasis as italic / strong as bold
                // with the normal foreground color — not a separate hue.
                emphasis: Some(ThemeStyle::italic()),
                emphasis_strong: Some(ThemeStyle::bold()),
                enum_: Some(ThemeStyle::color(rgb(0x6eb4bf).into())),
                function: Some(ThemeStyle::color(rgb(0x73ade9).into())),
                hint: Some(ThemeStyle::color(rgb(0x788ca6).into())),
                keyword: Some(ThemeStyle::color(rgb(0xb477cf).into())),
                label: Some(ThemeStyle::color(rgb(0x74ade8).into())),
                link_text: Some(ThemeStyle::color(rgb(0x73ade9).into())),
                link_uri: Some(ThemeStyle::color(rgb(0x6eb4bf).into())),
                number: Some(ThemeStyle::color(rgb(0xbf956a).into())),
                operator: Some(ThemeStyle::color(rgb(0x6eb4bf).into())),
                predictive: Some(ThemeStyle::color(rgb(0x5a6a87).into())),
                preproc: Some(ThemeStyle::color(rgb(0xb477cf).into())),
                primary: Some(ThemeStyle::color(editor_fg.unwrap())),
                property: Some(ThemeStyle::color(rgb(0xd07277).into())),
                punctuation: Some(ThemeStyle::color(editor_fg.unwrap())),
                punctuation_bracket: Some(ThemeStyle::color(rgb(0xb2b9c6).into())),
                punctuation_delimiter: Some(ThemeStyle::color(rgb(0xb2b9c6).into())),
                punctuation_list_marker: Some(ThemeStyle::color(rgb(0xd07277).into())),
                punctuation_special: Some(ThemeStyle::color(rgb(0xb1574b).into())),
                string: Some(ThemeStyle::color(rgb(0xa1c181).into())),
                string_escape: Some(ThemeStyle::color(rgb(0x878e98).into())),
                string_regex: Some(ThemeStyle::color(rgb(0xbf956a).into())),
                string_special: Some(ThemeStyle::color(rgb(0xbf956a).into())),
                string_special_symbol: Some(ThemeStyle::color(rgb(0xbf956a).into())),
                tag: Some(ThemeStyle::color(rgb(0x74ade8).into())),
                tag_doctype: Some(ThemeStyle::color(rgb(0xb477cf).into())),
                text_code_span: Some(ThemeStyle::color(rgb(0xe1d797).into())),
                text_literal: Some(ThemeStyle::color(rgb(0xa1c181).into())),
                title: Some(ThemeStyle::color(rgb(0xd07277).into())),
                type_: Some(ThemeStyle::color(rgb(0x6eb4bf).into())),
                variable: Some(ThemeStyle::color(editor_fg.unwrap())),
                variable_special: Some(ThemeStyle::color(rgb(0xbf956a).into())),
                variant: Some(ThemeStyle::color(rgb(0x73ade9).into())),
            }
        } else {
            // Default Light: approximate One Light / gpui-component Default Light
            SyntaxColors {
                attribute: Some(ThemeStyle::color(rgb(0x5c78e2).into())),
                boolean: Some(ThemeStyle::color(rgb(0xad6e25).into())),
                comment: Some(ThemeStyle::color(rgb(0xa2a3a7).into())),
                comment_doc: Some(ThemeStyle::color(rgb(0x7c7e86).into())),
                constant: Some(ThemeStyle::color(rgb(0xc18401).into())),
                constructor: Some(ThemeStyle::color(rgb(0x5c78e2).into())),
                embedded: Some(ThemeStyle::color(editor_fg.unwrap())),
                // VSCode renders markdown emphasis as italic / strong as bold
                // with the normal foreground color — not a separate hue.
                emphasis: Some(ThemeStyle::italic()),
                emphasis_strong: Some(ThemeStyle::bold()),
                enum_: Some(ThemeStyle::color(rgb(0x3882b7).into())),
                function: Some(ThemeStyle::color(rgb(0x5b79e3).into())),
                hint: Some(ThemeStyle::color(rgb(0x7274a7).into())),
                keyword: Some(ThemeStyle::color(rgb(0xa449ab).into())),
                label: Some(ThemeStyle::color(rgb(0x5c78e2).into())),
                link_text: Some(ThemeStyle::color(rgb(0x5b79e3).into())),
                link_uri: Some(ThemeStyle::color(rgb(0x3882b7).into())),
                number: Some(ThemeStyle::color(rgb(0xad6e25).into())),
                operator: Some(ThemeStyle::color(rgb(0x3882b7).into())),
                predictive: Some(ThemeStyle::color(rgb(0x9b9ec6).into())),
                preproc: Some(ThemeStyle::color(rgb(0xa449ab).into())),
                primary: Some(ThemeStyle::color(editor_fg.unwrap())),
                property: Some(ThemeStyle::color(rgb(0xd3604f).into())),
                punctuation: Some(ThemeStyle::color(editor_fg.unwrap())),
                punctuation_bracket: Some(ThemeStyle::color(rgb(0x4d4f52).into())),
                punctuation_delimiter: Some(ThemeStyle::color(rgb(0x4d4f52).into())),
                punctuation_list_marker: Some(ThemeStyle::color(rgb(0xd3604f).into())),
                punctuation_special: Some(ThemeStyle::color(rgb(0xb92b46).into())),
                string: Some(ThemeStyle::color(rgb(0x649f57).into())),
                string_escape: Some(ThemeStyle::color(rgb(0x7c7e86).into())),
                string_regex: Some(ThemeStyle::color(rgb(0xad6e26).into())),
                string_special: Some(ThemeStyle::color(rgb(0xad6e26).into())),
                string_special_symbol: Some(ThemeStyle::color(rgb(0xad6e26).into())),
                tag: Some(ThemeStyle::color(rgb(0x5c78e2).into())),
                tag_doctype: Some(ThemeStyle::color(rgb(0xa449ab).into())),
                text_code_span: Some(ThemeStyle::color(rgb(0x6f42c1).into())),
                text_literal: Some(ThemeStyle::color(rgb(0x649f57).into())),
                title: Some(ThemeStyle::color(rgb(0xd3604f).into())),
                type_: Some(ThemeStyle::color(rgb(0x3882b7).into())),
                variable: Some(ThemeStyle::color(editor_fg.unwrap())),
                variable_special: Some(ThemeStyle::color(rgb(0xad6e25).into())),
                variant: Some(ThemeStyle::color(rgb(0x5b79e3).into())),
            }
        };

        Arc::new(HighlightTheme {
            name: name.to_string(),
            appearance,
            style: HighlightThemeStyle {
                editor_background: editor_bg,
                editor_foreground: editor_fg,
                editor_active_line: active_line,
                editor_line_number: line_number,
                editor_active_line_number: active_line_number,
                editor_invisible: invisible,
                status: StatusColors::default(),
                syntax,
            },
        })
    }
}

#[derive(Clone, Debug)]
pub struct AppTheme {
    pub id: String,
    pub name: String,
    pub description: String,
    pub appearance: ThemeAppearance,
    pub ui: UiTheme,
    pub terminal: TerminalTheme,
    pub highlight_theme: Arc<HighlightTheme>,
}

impl std::ops::Deref for AppTheme {
    type Target = UiTheme;

    fn deref(&self) -> &Self::Target {
        &self.ui
    }
}

impl AppTheme {
    pub fn editor_background(&self) -> Hsla {
        self.highlight_theme
            .style
            .editor_background
            .unwrap_or(self.ui.panel_background)
    }

    pub fn input_background(&self) -> Hsla {
        self.ui.panel_background
    }
}

#[derive(Debug, Deserialize)]
struct ThemeBundleFile {
    themes: Vec<AppThemeFile>,
}

#[derive(Debug, Deserialize)]
struct AppThemeFile {
    id: String,
    name: String,
    description: String,
    appearance: ThemeAppearance,
    ui: UiThemeFile,
    terminal: TerminalThemeFile,
    #[serde(default, rename = "highlight")]
    highlight: Option<HighlightThemeStyle>,
}

#[derive(Debug, Deserialize)]
struct UiThemeFile {
    app_background: String,
    panel_background: String,
    elevated_background: String,
    overlay_background: String,
    title_bar_background: String,
    title_bar_hover: String,
    tab_bar_background: String,
    tab_active_background: String,
    tab_inactive_background: String,
    tab_hover_background: String,
    #[serde(default = "default_tab_active_border")]
    tab_active_border: String,
    border: String,
    border_strong: String,
    text: String,
    text_muted: String,
    text_subtle: String,
    icon: String,
    icon_muted: String,
    accent: String,
    accent_hover: String,
    accent_foreground: String,
    selection: String,
    drop_target: String,
    drop_target_border: String,
    success: String,
    success_background: String,
    success_foreground: String,
    warning: String,
    warning_background: String,
    error: String,
    error_background: String,
    error_foreground: String,
    notification_background: String,
    #[serde(default)]
    menu_background: Option<String>,
    #[serde(default)]
    menu_border: Option<String>,
    #[serde(default)]
    menu_separator: Option<String>,
    shadow: String,
    // Git status colors
    #[serde(default = "default_status_modified")]
    status_modified: String,
    #[serde(default = "default_status_added")]
    status_added: String,
    #[serde(default = "default_status_deleted")]
    status_deleted: String,
    #[serde(default = "default_status_conflict")]
    status_conflict: String,
}

#[derive(Debug, Deserialize)]
struct TerminalThemeFile {
    background: String,
    foreground: String,
    bright_foreground: String,
    dim_foreground: String,
    cursor: String,
    selection_background: String,
    ime_background: String,
    ime_underline: String,
    normal: [String; 8],
    bright: [String; 8],
    dim: [String; 8],
}

#[derive(Clone)]
pub struct ThemeState {
    themes: Vec<AppTheme>,
    active_theme_id: SharedString,
}

impl Global for ThemeState {}

pub trait ActiveTheme {
    fn theme(&self) -> &AppTheme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &AppTheme {
        ThemeState::global(self).active_theme()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThemePreference {
    theme_id: String,
}

pub fn init(cx: &mut App) {
    let themes = load_builtin_themes().expect("failed to load embedded builtin themes");
    let default_theme_id = default_theme_id(cx.window_appearance());
    let preferred_theme_id = ThemePreference::load()
        .map(|preference| preference.theme_id)
        .filter(|theme_id| themes.iter().any(|theme| theme.id == *theme_id));
    let active_theme_id = preferred_theme_id.unwrap_or_else(|| default_theme_id.to_string());

    cx.set_global(ThemeState {
        themes,
        active_theme_id: active_theme_id.into(),
    });
}

impl ThemeState {
    pub fn global(cx: &App) -> &Self {
        cx.global::<ThemeState>()
    }

    pub fn themes(cx: &App) -> &[AppTheme] {
        &Self::global(cx).themes
    }

    pub fn active_theme(&self) -> &AppTheme {
        self.themes
            .iter()
            .find(|theme| theme.id == self.active_theme_id.as_ref())
            .unwrap_or_else(|| {
                self.themes
                    .first()
                    .expect("builtin themes should never be empty")
            })
    }

    pub fn active_theme_id(&self) -> &str {
        self.active_theme_id.as_ref()
    }

    pub fn set_active(theme_id: &str, cx: &mut App) -> bool {
        let Some(next_theme_id) = Self::themes(cx)
            .iter()
            .find(|theme| theme.id == theme_id)
            .map(|theme| theme.id.to_string())
        else {
            return false;
        };

        let mut changed = false;
        {
            let state = cx.global_mut::<ThemeState>();
            if state.active_theme_id.as_ref() != next_theme_id {
                state.active_theme_id = next_theme_id.clone().into();
                changed = true;
            }
        }

        if changed {
            if let Err(err) = (ThemePreference {
                theme_id: next_theme_id,
            })
            .save()
            {
                log::warn!("failed to persist theme preference: {err}");
            }
            cx.refresh_windows();
        }

        changed
    }
}

impl ThemePreference {
    fn load() -> Option<Self> {
        let path = Self::config_path()?;
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn save(&self) -> anyhow::Result<()> {
        let Some(path) = Self::config_path() else {
            anyhow::bail!("unable to resolve theme config path");
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    fn config_path() -> Option<PathBuf> {
        let config_dir = dirs::config_dir()?;
        Some(config_dir.join("deepterm").join("theme.json"))
    }
}

fn default_theme_id(appearance: WindowAppearance) -> &'static str {
    match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => "macos26-light",
        WindowAppearance::Dark | WindowAppearance::VibrantDark => "macos26-dark",
    }
}

fn load_builtin_themes() -> anyhow::Result<Vec<AppTheme>> {
    let bundle: ThemeBundleFile =
        serde_json::from_str(include_str!("../../assets/themes/builtin.json"))?;
    bundle.themes.into_iter().map(AppTheme::try_from).collect()
}

fn parse_color(value: &str) -> anyhow::Result<Hsla> {
    let value = value.trim();
    let hex = value
        .strip_prefix('#')
        .or_else(|| value.strip_prefix("0x"))
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    let parsed = u32::from_str_radix(hex, 16)?;

    // Support both #RRGGBB and #RRGGBBAA.
    if hex.len() == 8 {
        let rgb_value = parsed >> 8;
        let alpha = (parsed & 0xFF) as f32 / 255.0;
        let mut color: Hsla = rgb(rgb_value).into();
        color.a = alpha;
        Ok(color)
    } else {
        Ok(rgb(parsed).into())
    }
}

fn default_status_modified() -> String {
    "#e2b93d".to_string()
}

fn default_status_added() -> String {
    "#4caf50".to_string()
}

fn default_status_deleted() -> String {
    "#f44336".to_string()
}

fn default_status_conflict() -> String {
    "#ff9800".to_string()
}

fn default_tab_active_border() -> String {
    "#FFFFFF1A".to_string()
}

impl UiThemeFile {
    /// 转换为 [`UiTheme`]，未在主题文件中声明的菜单颜色按外观回退到默认值
    /// （与历史硬编码行为一致，避免视觉变化）。
    fn into_ui_theme(self, appearance: ThemeAppearance) -> anyhow::Result<UiTheme> {
        let parse_opt = |value: Option<String>, default: Hsla| -> anyhow::Result<Hsla> {
            match value {
                Some(v) => parse_color(&v),
                None => Ok(default),
            }
        };
        Ok(UiTheme {
            app_background: parse_color(&self.app_background)?,
            panel_background: parse_color(&self.panel_background)?,
            elevated_background: parse_color(&self.elevated_background)?,
            overlay_background: parse_color(&self.overlay_background)?,
            title_bar_background: parse_color(&self.title_bar_background)?,
            title_bar_hover: parse_color(&self.title_bar_hover)?,
            tab_bar_background: parse_color(&self.tab_bar_background)?,
            tab_active_background: parse_color(&self.tab_active_background)?,
            tab_inactive_background: parse_color(&self.tab_inactive_background)?,
            tab_hover_background: parse_color(&self.tab_hover_background)?,
            tab_active_border: parse_color(&self.tab_active_border)?,
            border: parse_color(&self.border)?,
            border_strong: parse_color(&self.border_strong)?,
            text: parse_color(&self.text)?,
            text_muted: parse_color(&self.text_muted)?,
            text_subtle: parse_color(&self.text_subtle)?,
            icon: parse_color(&self.icon)?,
            icon_muted: parse_color(&self.icon_muted)?,
            accent: parse_color(&self.accent)?,
            accent_hover: parse_color(&self.accent_hover)?,
            accent_foreground: parse_color(&self.accent_foreground)?,
            selection: parse_color(&self.selection)?,
            drop_target: parse_color(&self.drop_target)?,
            drop_target_border: parse_color(&self.drop_target_border)?,
            success: parse_color(&self.success)?,
            success_background: parse_color(&self.success_background)?,
            success_foreground: parse_color(&self.success_foreground)?,
            warning: parse_color(&self.warning)?,
            warning_background: parse_color(&self.warning_background)?,
            error: parse_color(&self.error)?,
            error_background: parse_color(&self.error_background)?,
            error_foreground: parse_color(&self.error_foreground)?,
            notification_background: parse_color(&self.notification_background)?,
            menu_background: parse_opt(self.menu_background, default_menu_background(appearance))?,
            menu_border: parse_opt(self.menu_border, default_menu_border(appearance))?,
            menu_separator: parse_opt(self.menu_separator, default_menu_separator(appearance))?,
            shadow: parse_color(&self.shadow)?,
            status_modified: parse_color(&self.status_modified)?,
            status_added: parse_color(&self.status_added)?,
            status_deleted: parse_color(&self.status_deleted)?,
            status_conflict: parse_color(&self.status_conflict)?,
        })
    }
}

/// 菜单浮层背景色的默认值：深色取 macOS 系统菜单灰，浅色取纯白。
fn default_menu_background(appearance: ThemeAppearance) -> Hsla {
    if appearance.is_dark() {
        rgb(0x2C2C2E).into()
    } else {
        rgb(0xFFFFFF).into()
    }
}

/// 菜单浮层描边色的默认值：白色低透明度（深色 10% / 浅色 60%），
/// 与历史硬编码行为一致。
fn default_menu_border(appearance: ThemeAppearance) -> Hsla {
    let white: Hsla = rgb(0xFFFFFF).into();
    white.opacity(if appearance.is_dark() { 0.1 } else { 0.6 })
}

/// 菜单分隔线颜色的默认值。
fn default_menu_separator(appearance: ThemeAppearance) -> Hsla {
    if appearance.is_dark() {
        rgb(0x48484A).into()
    } else {
        rgb(0xE5E5EA).into()
    }
}

impl TryFrom<TerminalThemeFile> for TerminalTheme {
    type Error = anyhow::Error;

    fn try_from(value: TerminalThemeFile) -> anyhow::Result<Self> {
        let parse_array = |items: [String; 8]| -> anyhow::Result<[Hsla; 8]> {
            let [a, b, c, d, e, f, g, h] = items;
            Ok([
                parse_color(&a)?,
                parse_color(&b)?,
                parse_color(&c)?,
                parse_color(&d)?,
                parse_color(&e)?,
                parse_color(&f)?,
                parse_color(&g)?,
                parse_color(&h)?,
            ])
        };

        let normal = parse_array(value.normal)?;
        let bright = parse_array(value.bright)?;
        let dim = parse_array(value.dim)?;
        let indexed_palette = Self::build_indexed_palette(normal, bright);
        Ok(Self {
            background: parse_color(&value.background)?,
            foreground: parse_color(&value.foreground)?,
            bright_foreground: parse_color(&value.bright_foreground)?,
            dim_foreground: parse_color(&value.dim_foreground)?,
            cursor: parse_color(&value.cursor)?,
            selection_background: parse_color(&value.selection_background)?,
            ime_background: parse_color(&value.ime_background)?,
            ime_underline: parse_color(&value.ime_underline)?,
            normal,
            bright,
            dim,
            indexed_palette,
        })
    }
}

impl TryFrom<AppThemeFile> for AppTheme {
    type Error = anyhow::Error;

    fn try_from(value: AppThemeFile) -> anyhow::Result<Self> {
        let terminal: TerminalTheme = value.terminal.try_into()?;
        let appearance = value.appearance;
        let highlight_theme = match value.highlight {
            Some(style) => Arc::new(HighlightTheme {
                name: value.name.clone(),
                appearance,
                style,
            }),
            None => terminal.to_highlight_theme(&value.name, appearance),
        };
        Ok(Self {
            id: value.id,
            name: value.name,
            description: value.description,
            appearance,
            ui: value.ui.into_ui_theme(appearance)?,
            terminal,
            highlight_theme,
        })
    }
}
