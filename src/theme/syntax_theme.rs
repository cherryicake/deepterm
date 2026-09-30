//! 语法高亮主题类型（SyntaxColors / HighlightTheme 等）。
//!
//! 这些类型由 `theme` 与 `editor::syntax` 共同消费，放在 editor 之下会造成
//! `theme → editor` 的层次倒置（基础 UI 模块反向依赖编辑器功能模块），
//! 故独立于 `theme` 内、由 `editor::syntax` re-export 保持旧路径兼容。

use gpui::{App, FontWeight, HighlightStyle, Hsla};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::ops::Deref;

use crate::theme::ThemeAppearance;

pub const HIGHLIGHT_NAMES: [&str; 41] = [
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constructor",
    "embedded",
    "emphasis",
    "emphasis.strong",
    "enum",
    "function",
    "hint",
    "keyword",
    "label",
    "link_text",
    "link_uri",
    "number",
    "operator",
    "predictive",
    "preproc",
    "primary",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.list_marker",
    "punctuation.special",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "string.special.symbol",
    "tag",
    "tag.doctype",
    "text.code.span",
    "text.literal",
    "title",
    "type",
    "variable",
    "variable.special",
    "variant",
];

/// Theme for Tree-sitter Highlight
///
/// https://docs.rs/tree-sitter-highlight/0.25.4/tree_sitter_highlight/
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, JsonSchema, Serialize, Deserialize)]
pub struct SyntaxColors {
    pub attribute: Option<ThemeStyle>,
    pub boolean: Option<ThemeStyle>,
    pub comment: Option<ThemeStyle>,
    #[serde(rename = "comment.doc")]
    pub comment_doc: Option<ThemeStyle>,
    pub constant: Option<ThemeStyle>,
    pub constructor: Option<ThemeStyle>,
    pub embedded: Option<ThemeStyle>,
    pub emphasis: Option<ThemeStyle>,
    #[serde(rename = "emphasis.strong")]
    pub emphasis_strong: Option<ThemeStyle>,
    #[serde(rename = "enum")]
    pub enum_: Option<ThemeStyle>,
    pub function: Option<ThemeStyle>,
    pub hint: Option<ThemeStyle>,
    pub keyword: Option<ThemeStyle>,
    pub label: Option<ThemeStyle>,
    #[serde(rename = "link_text")]
    pub link_text: Option<ThemeStyle>,
    #[serde(rename = "link_uri")]
    pub link_uri: Option<ThemeStyle>,
    pub number: Option<ThemeStyle>,
    pub operator: Option<ThemeStyle>,
    pub predictive: Option<ThemeStyle>,
    pub preproc: Option<ThemeStyle>,
    pub primary: Option<ThemeStyle>,
    pub property: Option<ThemeStyle>,
    pub punctuation: Option<ThemeStyle>,
    #[serde(rename = "punctuation.bracket")]
    pub punctuation_bracket: Option<ThemeStyle>,
    #[serde(rename = "punctuation.delimiter")]
    pub punctuation_delimiter: Option<ThemeStyle>,
    #[serde(rename = "punctuation.list_marker")]
    pub punctuation_list_marker: Option<ThemeStyle>,
    #[serde(rename = "punctuation.special")]
    pub punctuation_special: Option<ThemeStyle>,
    pub string: Option<ThemeStyle>,
    #[serde(rename = "string.escape")]
    pub string_escape: Option<ThemeStyle>,
    #[serde(rename = "string.regex")]
    pub string_regex: Option<ThemeStyle>,
    #[serde(rename = "string.special")]
    pub string_special: Option<ThemeStyle>,
    #[serde(rename = "string.special.symbol")]
    pub string_special_symbol: Option<ThemeStyle>,
    pub tag: Option<ThemeStyle>,
    #[serde(rename = "tag.doctype")]
    pub tag_doctype: Option<ThemeStyle>,
    #[serde(rename = "text.code.span")]
    pub text_code_span: Option<ThemeStyle>,
    #[serde(rename = "text.literal")]
    pub text_literal: Option<ThemeStyle>,
    pub title: Option<ThemeStyle>,
    #[serde(rename = "type")]
    pub type_: Option<ThemeStyle>,
    pub variable: Option<ThemeStyle>,
    #[serde(rename = "variable.special")]
    pub variable_special: Option<ThemeStyle>,
    pub variant: Option<ThemeStyle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FontStyle {
    Normal,
    Italic,
    Underline,
}

impl From<FontStyle> for gpui::FontStyle {
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => gpui::FontStyle::Normal,
            FontStyle::Italic => gpui::FontStyle::Italic,
            FontStyle::Underline => gpui::FontStyle::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Serialize_repr, Deserialize_repr, JsonSchema)]
#[repr(u16)]
pub enum FontWeightContent {
    Thin = 100,
    ExtraLight = 200,
    Light = 300,
    Normal = 400,
    Medium = 500,
    Semibold = 600,
    Bold = 700,
    ExtraBold = 800,
    Black = 900,
}

impl From<FontWeightContent> for FontWeight {
    fn from(value: FontWeightContent) -> Self {
        match value {
            FontWeightContent::Thin => FontWeight::THIN,
            FontWeightContent::ExtraLight => FontWeight::EXTRA_LIGHT,
            FontWeightContent::Light => FontWeight::LIGHT,
            FontWeightContent::Normal => FontWeight::NORMAL,
            FontWeightContent::Medium => FontWeight::MEDIUM,
            FontWeightContent::Semibold => FontWeight::SEMIBOLD,
            FontWeightContent::Bold => FontWeight::BOLD,
            FontWeightContent::ExtraBold => FontWeight::EXTRA_BOLD,
            FontWeightContent::Black => FontWeight::BLACK,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema, Serialize, Deserialize)]
pub struct ThemeStyle {
    color: Option<Hsla>,
    font_style: Option<FontStyle>,
    font_weight: Option<FontWeightContent>,
}

impl ThemeStyle {
    pub fn color(color: Hsla) -> Self {
        Self {
            color: Some(color),
            font_style: None,
            font_weight: None,
        }
    }

    /// Italic text with the inherited foreground color (markdown emphasis).
    pub fn italic() -> Self {
        Self {
            color: None,
            font_style: Some(FontStyle::Italic),
            font_weight: None,
        }
    }

    /// Bold text with the inherited foreground color (markdown strong).
    pub fn bold() -> Self {
        Self {
            color: None,
            font_style: None,
            font_weight: Some(FontWeightContent::Bold),
        }
    }
}

impl From<ThemeStyle> for HighlightStyle {
    fn from(style: ThemeStyle) -> Self {
        HighlightStyle {
            color: style.color,
            font_weight: style.font_weight.map(Into::into),
            font_style: style.font_style.map(Into::into),
            ..Default::default()
        }
    }
}

impl SyntaxColors {
    /// Map bare capture aliases that appear in language queries but are not
    /// part of the canonical 41-token palette to a supported token name.
    ///
    /// Mapping rules follow VSCode/Zed semantics:
    /// - `variable.parameter` is a plain variable (VSCode shows params in the
    ///   normal variable color, NOT the `self`/`this` special color).
    /// - `variable.member` / `field` are properties.
    /// - Doc comments (`comment.documentation`) are `comment.doc`; docstrings
    ///   (`string.doc`) are still strings.
    fn normalize_name(name: &str) -> &str {
        match name {
            "escape" => "string.escape",
            "string.documentation" | "string.doc" => "string",
            "comment.documentation" => "comment.doc",
            "character" | "char" => "string",
            "import" | "cImport" => "keyword",
            // CSS at-rules (emitted by tree-sitter-css) are keywords in VSCode.
            "charset" | "keyframes" | "media" | "supports" => "keyword",
            "function.macro" | "function.method" | "function.call" | "function.builtin"
            | "function.special" | "method" | "method.call" | "method.definition" => "function",
            "constant.builtin" | "constant.macro" => "constant",
            "float" | "number.float" => "number",
            "variable.builtin" | "variable.special" => "variable.special",
            "variable.parameter" | "parameter" | "definition.parameter" => "variable",
            "variable.member" | "field" | "member" => "property",
            "namespace" | "module" => "type",
            "type.builtin"
            | "type.definition"
            | "type.interface"
            | "type.super"
            | "type.class.definition" => "type",
            "keyword.control"
            | "keyword.modifier"
            | "keyword.type"
            | "keyword.coroutine"
            | "keyword.function"
            | "keyword.import"
            | "keyword.repeat"
            | "keyword.return"
            | "keyword.debug"
            | "keyword.exception"
            | "keyword.conditional"
            | "keyword.conditional.ternary"
            | "keyword.directive"
            | "keyword.directive.define"
            | "keyword.export"
            | "keyword.operator"
            | "conditional"
            | "repeat"
            | "exception"
            | "include"
            | "directive" => "keyword",
            // Lua emits bare `@delimiter` for `.` and `;`.
            "delimiter" => "punctuation.delimiter",
            "tag.error" | "tag.delimiter" => "tag",
            "tag.attribute" => "attribute",
            "text.reference" => "link_text",
            "text.uri" => "link_uri",
            "text.danger" | "text.warning" | "text.note" => "string.special",
            "selector" | "selector.pseudo" => "type",
            "punctuation.markup" => "punctuation",
            "diff.plus" | "diff.minus" => "string.special",
            "_class" | "_function" | "_jsdoc_comment" | "_name" | "spell" | "injection.content"
            | "injection.language" => "",
            "boolean" => "boolean",
            "none" => "",
            _ => name,
        }
    }

    pub fn style(&self, name: &str) -> Option<HighlightStyle> {
        if name.is_empty() {
            return None;
        }

        let name = Self::normalize_name(name);
        if name.is_empty() {
            return None;
        }

        let style = match name {
            "attribute" => self.attribute,
            "boolean" => self.boolean,
            "comment" => self.comment,
            "comment.doc" => self.comment_doc,
            "constant" => self.constant,
            "constructor" => self.constructor,
            "embedded" => self.embedded,
            "emphasis" => self.emphasis,
            "emphasis.strong" => self.emphasis_strong,
            "enum" => self.enum_,
            "function" => self.function,
            "hint" => self.hint,
            "keyword" => self.keyword,
            "label" => self.label,
            "link_text" => self.link_text,
            "link_uri" => self.link_uri,
            "number" => self.number,
            "operator" => self.operator,
            "predictive" => self.predictive,
            "preproc" => self.preproc,
            "primary" => self.primary,
            "property" => self.property,
            "punctuation" => self.punctuation,
            "punctuation.bracket" => self.punctuation_bracket,
            "punctuation.delimiter" => self.punctuation_delimiter,
            "punctuation.list_marker" => self.punctuation_list_marker,
            "punctuation.special" => self.punctuation_special,
            "string" => self.string,
            "string.escape" => self.string_escape,
            "string.regex" => self.string_regex,
            "string.special" => self.string_special,
            "string.special.symbol" => self.string_special_symbol,
            "tag" => self.tag,
            "tag.doctype" => self.tag_doctype,
            "text.code.span" => self.text_code_span,
            "text.literal" => self.text_literal,
            "title" => self.title,
            "type" => self.type_,
            "variable" => self.variable,
            "variable.special" => self.variable_special,
            "variant" => self.variant,
            _ => None,
        }
        .map(|s| s.into());

        if style.is_some() {
            style
        } else if name.contains('.') {
            // Fallback `keyword.modifier` to `keyword`
            name.split('.').next().and_then(|prefix| self.style(prefix))
        } else {
            None
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, JsonSchema, Serialize, Deserialize)]
pub struct StatusColors {
    #[serde(rename = "error")]
    error: Option<Hsla>,
    #[serde(rename = "error.background")]
    error_background: Option<Hsla>,
    #[serde(rename = "error.border")]
    error_border: Option<Hsla>,
    #[serde(rename = "warning")]
    warning: Option<Hsla>,
    #[serde(rename = "warning.background")]
    warning_background: Option<Hsla>,
    #[serde(rename = "warning.border")]
    warning_border: Option<Hsla>,
    #[serde(rename = "info")]
    info: Option<Hsla>,
    #[serde(rename = "info.background")]
    info_background: Option<Hsla>,
    #[serde(rename = "info.border")]
    info_border: Option<Hsla>,
    #[serde(rename = "success")]
    success: Option<Hsla>,
    #[serde(rename = "success.background")]
    success_background: Option<Hsla>,
    #[serde(rename = "success.border")]
    success_border: Option<Hsla>,
    #[serde(rename = "hint")]
    hint: Option<Hsla>,
    #[serde(rename = "hint.background")]
    hint_background: Option<Hsla>,
    #[serde(rename = "hint.border")]
    hint_border: Option<Hsla>,
}

impl StatusColors {
    #[inline]
    pub fn error(&self, _cx: &App) -> Hsla {
        self.error.unwrap_or(gpui::red())
    }

    #[inline]
    pub fn error_background(&self, _cx: &App) -> Hsla {
        self.error_background.unwrap_or(gpui::red().alpha(0.2))
    }

    #[inline]
    pub fn error_border(&self, _cx: &App) -> Hsla {
        self.error_border.unwrap_or(self.error(_cx))
    }

    #[inline]
    pub fn warning(&self, _cx: &App) -> Hsla {
        self.warning.unwrap_or(gpui::yellow())
    }

    #[inline]
    pub fn warning_background(&self, _cx: &App) -> Hsla {
        self.warning_background.unwrap_or(gpui::yellow().alpha(0.2))
    }

    #[inline]
    pub fn warning_border(&self, _cx: &App) -> Hsla {
        self.warning_border.unwrap_or(self.warning(_cx))
    }

    #[inline]
    pub fn info(&self, _cx: &App) -> Hsla {
        self.info.unwrap_or(gpui::blue())
    }

    #[inline]
    pub fn info_background(&self, _cx: &App) -> Hsla {
        self.info_background.unwrap_or(gpui::blue().alpha(0.2))
    }

    #[inline]
    pub fn info_border(&self, _cx: &App) -> Hsla {
        self.info_border.unwrap_or(self.info(_cx))
    }

    #[inline]
    pub fn success(&self, _cx: &App) -> Hsla {
        self.success.unwrap_or(gpui::green())
    }

    #[inline]
    pub fn success_background(&self, _cx: &App) -> Hsla {
        self.success_background.unwrap_or(gpui::green().alpha(0.2))
    }

    #[inline]
    pub fn success_border(&self, _cx: &App) -> Hsla {
        self.success_border.unwrap_or(self.success(_cx))
    }

    #[inline]
    pub fn hint(&self, _cx: &App) -> Hsla {
        self.hint.unwrap_or(gpui::rgb(0x00FFFF).into())
    }

    #[inline]
    pub fn hint_background(&self, _cx: &App) -> Hsla {
        self.hint_background.unwrap_or_else(|| {
            let c: Hsla = gpui::rgb(0x00FFFF).into();
            c.opacity(0.2)
        })
    }

    #[inline]
    pub fn hint_border(&self, _cx: &App) -> Hsla {
        self.hint_border.unwrap_or(self.hint(_cx))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, JsonSchema, Serialize, Deserialize)]
pub struct HighlightThemeStyle {
    #[serde(rename = "editor.background")]
    pub editor_background: Option<Hsla>,
    #[serde(rename = "editor.foreground")]
    pub editor_foreground: Option<Hsla>,
    #[serde(rename = "editor.active_line.background")]
    pub editor_active_line: Option<Hsla>,
    #[serde(rename = "editor.line_number")]
    pub editor_line_number: Option<Hsla>,
    #[serde(rename = "editor.active_line_number")]
    pub editor_active_line_number: Option<Hsla>,
    #[serde(rename = "editor.invisible")]
    pub editor_invisible: Option<Hsla>,
    #[serde(flatten)]
    pub status: StatusColors,
    #[serde(rename = "syntax")]
    pub syntax: SyntaxColors,
}

/// Theme for Tree-sitter Highlight from JSON theme file.
///
/// This json is compatible with the Zed theme format.
///
/// https://zed.dev/docs/extensions/languages#syntax-highlighting
#[derive(Debug, Clone, PartialEq, Eq, Hash, JsonSchema, Serialize, Deserialize)]
pub struct HighlightTheme {
    pub name: String,
    #[serde(default)]
    pub appearance: ThemeAppearance,
    pub style: HighlightThemeStyle,
}

impl Deref for HighlightTheme {
    type Target = SyntaxColors;

    fn deref(&self) -> &Self::Target {
        &self.style.syntax
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_name_semantics() {
        let n = SyntaxColors::normalize_name;

        // Parameters are plain variables (VSCode parity), not self/this.
        assert_eq!(n("variable.parameter"), "variable");
        assert_eq!(n("variable.member"), "property");
        assert_eq!(n("field"), "property");
        assert_eq!(n("comment.documentation"), "comment.doc");
        assert_eq!(n("string.documentation"), "string");
        assert_eq!(n("string.doc"), "string");
        assert_eq!(n("escape"), "string.escape");
        assert_eq!(n("char"), "string");
        assert_eq!(n("import"), "keyword");
    }

    #[test]
    fn test_style_lookup_for_remapped_captures() {
        let colors = SyntaxColors {
            variable: Some(ThemeStyle::color(gpui::red())),
            variable_special: Some(ThemeStyle::color(gpui::green())),
            property: Some(ThemeStyle::color(gpui::blue())),
            function: Some(ThemeStyle::color(gpui::black())),
            keyword: Some(ThemeStyle::color(gpui::white())),
            comment_doc: Some(ThemeStyle::color(gpui::rgb(0xff00ff).into())),
            string: Some(ThemeStyle::color(gpui::yellow())),
            string_escape: Some(ThemeStyle::color(gpui::rgb(0x00ffff).into())),
            attribute: Some(ThemeStyle::color(gpui::rgb(0xffa500).into())),
            punctuation_delimiter: Some(ThemeStyle::color(gpui::rgb(0xff00aa).into())),
            ..Default::default()
        };

        let color_of = |name: &str| colors.style(name).and_then(|s| s.color);
        assert_eq!(color_of("variable.parameter"), color_of("variable"));
        assert_eq!(color_of("method.call"), color_of("function"));
        assert_eq!(color_of("field"), color_of("property"));
        assert_eq!(color_of("comment.documentation"), color_of("comment.doc"));
        assert_eq!(color_of("string.doc"), color_of("string"));
        assert_eq!(color_of("media"), color_of("keyword"));
        assert_eq!(color_of("delimiter"), color_of("punctuation.delimiter"));
    }
}
