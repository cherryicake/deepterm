use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::vte::ansi::NamedColor;
use gpui::{App, Global, Hsla, SharedString, WindowAppearance, rgb};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::highlighter::{
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
    pub shadow: Hsla,
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
        Arc::new(HighlightTheme {
            name: name.to_string(),
            appearance,
            style: HighlightThemeStyle {
                editor_background: Some(self.background),
                editor_foreground: Some(self.foreground),
                editor_active_line: Some(gpui::Hsla::from(
                    gpui::Rgba::from(self.background).blend(if appearance.is_dark() {
                        gpui::rgba(0xFFFFFF10)
                    } else {
                        gpui::rgba(0x0000000C)
                    }),
                )),
                editor_line_number: Some(self.dim_foreground),
                editor_active_line_number: Some(self.foreground),
                editor_invisible: Some(self.dim_foreground),
                status: StatusColors::default(),
                syntax: SyntaxColors {
                    attribute: Some(ThemeStyle::color(self.normal[6])),
                    boolean: Some(ThemeStyle::color(self.normal[4])),
                    comment: Some(ThemeStyle::color(self.dim_foreground)),
                    comment_doc: Some(ThemeStyle::color(self.dim_foreground)),
                    constant: Some(ThemeStyle::color(self.normal[5])),
                    constructor: Some(ThemeStyle::color(self.normal[3])),
                    embedded: Some(ThemeStyle::color(self.foreground)),
                    enum_: Some(ThemeStyle::color(self.normal[6])),
                    function: Some(ThemeStyle::color(self.normal[3])),
                    keyword: Some(ThemeStyle::color(self.normal[5])),
                    label: Some(ThemeStyle::color(self.normal[3])),
                    link_text: Some(ThemeStyle::color(self.normal[4])),
                    link_uri: Some(ThemeStyle::color(self.normal[4])),
                    number: Some(ThemeStyle::color(self.normal[2])),
                    operator: Some(ThemeStyle::color(self.foreground)),
                    preproc: Some(ThemeStyle::color(self.normal[5])),
                    property: Some(ThemeStyle::color(self.normal[5])),
                    punctuation: Some(ThemeStyle::color(self.dim_foreground)),
                    punctuation_bracket: Some(ThemeStyle::color(self.dim_foreground)),
                    punctuation_delimiter: Some(ThemeStyle::color(self.dim_foreground)),
                    punctuation_special: Some(ThemeStyle::color(self.normal[5])),
                    string: Some(ThemeStyle::color(self.normal[1])),
                    string_escape: Some(ThemeStyle::color(self.normal[6])),
                    string_regex: Some(ThemeStyle::color(self.normal[1])),
                    string_special: Some(ThemeStyle::color(self.normal[3])),
                    tag: Some(ThemeStyle::color(self.normal[1])),
                    text_literal: Some(ThemeStyle::color(self.normal[6])),
                    title: Some(ThemeStyle::color(self.normal[5])),
                    type_: Some(ThemeStyle::color(self.normal[6])),
                    variable: Some(ThemeStyle::color(self.foreground)),
                    variable_special: Some(ThemeStyle::color(self.normal[5])),
                    ..Default::default()
                },
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
    shadow: String,
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
        WindowAppearance::Light | WindowAppearance::VibrantLight => "flexoki-light",
        WindowAppearance::Dark | WindowAppearance::VibrantDark => "deepterm-night",
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
    Ok(rgb(parsed).into())
}

impl TryFrom<UiThemeFile> for UiTheme {
    type Error = anyhow::Error;

    fn try_from(value: UiThemeFile) -> anyhow::Result<Self> {
        Ok(Self {
            app_background: parse_color(&value.app_background)?,
            panel_background: parse_color(&value.panel_background)?,
            elevated_background: parse_color(&value.elevated_background)?,
            overlay_background: parse_color(&value.overlay_background)?,
            title_bar_background: parse_color(&value.title_bar_background)?,
            title_bar_hover: parse_color(&value.title_bar_hover)?,
            tab_bar_background: parse_color(&value.tab_bar_background)?,
            tab_active_background: parse_color(&value.tab_active_background)?,
            tab_inactive_background: parse_color(&value.tab_inactive_background)?,
            tab_hover_background: parse_color(&value.tab_hover_background)?,
            border: parse_color(&value.border)?,
            border_strong: parse_color(&value.border_strong)?,
            text: parse_color(&value.text)?,
            text_muted: parse_color(&value.text_muted)?,
            text_subtle: parse_color(&value.text_subtle)?,
            icon: parse_color(&value.icon)?,
            icon_muted: parse_color(&value.icon_muted)?,
            accent: parse_color(&value.accent)?,
            accent_hover: parse_color(&value.accent_hover)?,
            accent_foreground: parse_color(&value.accent_foreground)?,
            selection: parse_color(&value.selection)?,
            drop_target: parse_color(&value.drop_target)?,
            drop_target_border: parse_color(&value.drop_target_border)?,
            success: parse_color(&value.success)?,
            success_background: parse_color(&value.success_background)?,
            success_foreground: parse_color(&value.success_foreground)?,
            warning: parse_color(&value.warning)?,
            warning_background: parse_color(&value.warning_background)?,
            error: parse_color(&value.error)?,
            error_background: parse_color(&value.error_background)?,
            error_foreground: parse_color(&value.error_foreground)?,
            shadow: parse_color(&value.shadow)?,
        })
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

        Ok(Self {
            background: parse_color(&value.background)?,
            foreground: parse_color(&value.foreground)?,
            bright_foreground: parse_color(&value.bright_foreground)?,
            dim_foreground: parse_color(&value.dim_foreground)?,
            cursor: parse_color(&value.cursor)?,
            selection_background: parse_color(&value.selection_background)?,
            ime_background: parse_color(&value.ime_background)?,
            ime_underline: parse_color(&value.ime_underline)?,
            normal: parse_array(value.normal)?,
            bright: parse_array(value.bright)?,
            dim: parse_array(value.dim)?,
        })
    }
}

impl TryFrom<AppThemeFile> for AppTheme {
    type Error = anyhow::Error;

    fn try_from(value: AppThemeFile) -> anyhow::Result<Self> {
        let terminal: TerminalTheme = value.terminal.try_into()?;
        let highlight_theme = match value.highlight {
            Some(style) => Arc::new(HighlightTheme {
                name: value.name.clone(),
                appearance: value.appearance,
                style,
            }),
            None => terminal.to_highlight_theme(&value.name, value.appearance),
        };
        Ok(Self {
            id: value.id,
            name: value.name,
            description: value.description,
            appearance: value.appearance,
            ui: value.ui.try_into()?,
            terminal,
            highlight_theme,
        })
    }
}
