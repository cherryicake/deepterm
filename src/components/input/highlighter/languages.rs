use gpui::SharedString;

use crate::components::input::highlighter::LanguageConfig;

#[cfg(not(feature = "tree-sitter-languages"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, enum_iterator::Sequence)]
pub enum Language {
    Json,
}

#[cfg(feature = "tree-sitter-languages")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, enum_iterator::Sequence)]
pub enum Language {
    Json,
    Plain,
    Bash,
    C,
    Cpp,
    Css,
    Go,
    Html,
    JavaScript,
    Kotlin,
    Lua,
    Markdown,
    MarkdownInline,
    Python,
    Rust,
    Toml,
    Tsx,
    TypeScript,
    Zig,
}

impl From<Language> for SharedString {
    fn from(language: Language) -> Self {
        language.name().into()
    }
}

impl Language {
    pub fn all() -> impl Iterator<Item = Self> {
        enum_iterator::all::<Language>()
    }

    pub fn name(&self) -> &'static str {
        #[cfg(not(feature = "tree-sitter-languages"))]
        return "json";

        #[cfg(feature = "tree-sitter-languages")]
        match self {
            Self::Plain => "text",
            Self::Bash => "bash",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Css => "css",
            Self::Go => "go",
            Self::Html => "html",
            Self::JavaScript => "javascript",
            Self::Json => "json",
            Self::Kotlin => "kotlin",
            Self::Lua => "lua",
            Self::Markdown => "markdown",
            Self::MarkdownInline => "markdown_inline",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Toml => "toml",
            Self::Tsx => "tsx",
            Self::TypeScript => "typescript",
            Self::Zig => "zig",
        }
    }

    #[allow(unused)]
    pub fn from_str(s: &str) -> Self {
        #[cfg(not(feature = "tree-sitter-languages"))]
        return Self::Json;

        #[cfg(feature = "tree-sitter-languages")]
        match s {
            "bash" | "sh" => Self::Bash,
            "c" => Self::C,
            "cpp" | "c++" => Self::Cpp,
            "css" | "scss" => Self::Css,
            "go" => Self::Go,
            "html" => Self::Html,
            "javascript" | "js" => Self::JavaScript,
            "json" | "jsonc" => Self::Json,
            "kotlin" | "kt" | "kts" => Self::Kotlin,
            "lua" => Self::Lua,
            "markdown" | "md" | "mdx" => Self::Markdown,
            "markdown_inline" | "markdown-inline" => Self::MarkdownInline,
            "python" | "py" => Self::Python,
            "rust" | "rs" => Self::Rust,
            "toml" => Self::Toml,
            "tsx" => Self::Tsx,
            "typescript" | "ts" => Self::TypeScript,
            "zig" => Self::Zig,
            _ => Self::Plain,
        }
    }

    #[allow(unused)]
    pub(super) fn injection_languages(&self) -> Vec<SharedString> {
        #[cfg(not(feature = "tree-sitter-languages"))]
        return vec![];

        #[cfg(feature = "tree-sitter-languages")]
        match self {
            Self::Markdown => vec!["markdown-inline", "html", "toml"],
            Self::MarkdownInline => vec![],
            Self::Html => vec!["javascript", "css"],
            Self::Rust => vec!["rust"],
            Self::JavaScript | Self::TypeScript => {
                vec!["json", "css", "html", "typescript", "javascript", "tsx"]
            }
            _ => vec![],
        }
        .into_iter()
        .map(|s| s.into())
        .collect()
    }

    /// Return the language info for the language.
    ///
    /// (language, query, injection, locals)
    pub(super) fn config(&self) -> LanguageConfig {
        #[cfg(not(feature = "tree-sitter-languages"))]
        let (language, query, injection, locals) = match self {
            Self::Json => (
                tree_sitter_json::LANGUAGE,
                include_str!("languages/json/highlights.scm"),
                "",
                "",
            ),
        };

        #[cfg(feature = "tree-sitter-languages")]
        let (language, query, injection, locals) = match self {
            Self::Plain => (tree_sitter_json::LANGUAGE, "", "", ""),
            Self::Json => (
                tree_sitter_json::LANGUAGE,
                include_str!("languages/json/highlights.scm"),
                "",
                "",
            ),
            Self::Markdown => (
                tree_sitter_md::LANGUAGE,
                include_str!("languages/markdown/highlights.scm"),
                include_str!("languages/markdown/injections.scm"),
                "",
            ),
            Self::MarkdownInline => (
                tree_sitter_md::INLINE_LANGUAGE,
                include_str!("languages/markdown_inline/highlights.scm"),
                "",
                "",
            ),
            Self::Toml => (
                tree_sitter_toml_ng::LANGUAGE,
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::Rust => (
                tree_sitter_rust::LANGUAGE,
                include_str!("languages/rust/highlights.scm"),
                include_str!("languages/rust/injections.scm"),
                "",
            ),
            Self::Go => (
                tree_sitter_go::LANGUAGE,
                include_str!("languages/go/highlights.scm"),
                "",
                "",
            ),
            Self::C => (
                tree_sitter_c::LANGUAGE,
                tree_sitter_c::HIGHLIGHT_QUERY,
                "",
                "",
            ),
            Self::Cpp => (
                tree_sitter_cpp::LANGUAGE,
                tree_sitter_cpp::HIGHLIGHT_QUERY,
                "",
                "",
            ),
            Self::JavaScript => (
                tree_sitter_javascript::LANGUAGE,
                include_str!("languages/javascript/highlights.scm"),
                include_str!("languages/javascript/injections.scm"),
                tree_sitter_javascript::LOCALS_QUERY,
            ),
            Self::Python => (
                tree_sitter_python::LANGUAGE,
                tree_sitter_python::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::Bash => (
                tree_sitter_bash::LANGUAGE,
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "",
                "",
            ),
            Self::Html => (
                tree_sitter_html::LANGUAGE,
                include_str!("languages/html/highlights.scm"),
                include_str!("languages/html/injections.scm"),
                "",
            ),
            Self::Css => (
                tree_sitter_css::LANGUAGE,
                tree_sitter_css::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::Kotlin => (
                tree_sitter_kotlin_ng::LANGUAGE,
                include_str!("languages/kotlin/highlights.scm"),
                "",
                "",
            ),
            Self::Lua => (
                tree_sitter_lua::LANGUAGE,
                tree_sitter_lua::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
            Self::TypeScript => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                include_str!("languages/typescript/highlights.scm"),
                include_str!("languages/javascript/injections.scm"),
                tree_sitter_typescript::LOCALS_QUERY,
            ),
            Self::Tsx => (
                tree_sitter_typescript::LANGUAGE_TSX,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                "",
                tree_sitter_typescript::LOCALS_QUERY,
            ),
            Self::Zig => (
                tree_sitter_zig::LANGUAGE,
                tree_sitter_zig::HIGHLIGHTS_QUERY,
                include_str!("languages/zig/injections.scm"),
                "",
            ),
        };

        let language = tree_sitter::Language::new(language);

        LanguageConfig::new(
            self.name(),
            language,
            self.injection_languages(),
            query,
            injection,
            locals,
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_language_name() {
        use super::*;

        assert_eq!(Language::MarkdownInline.name(), "markdown_inline");
        assert_eq!(Language::Markdown.name(), "markdown");
        assert_eq!(Language::Json.name(), "json");
        assert_eq!(Language::Rust.name(), "rust");
        assert_eq!(Language::Go.name(), "go");
        assert_eq!(Language::C.name(), "c");
        assert_eq!(Language::Cpp.name(), "cpp");
        assert_eq!(Language::JavaScript.name(), "javascript");
        assert_eq!(Language::TypeScript.name(), "typescript");
        assert_eq!(Language::Tsx.name(), "tsx");
    }
}
