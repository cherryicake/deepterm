use gpui::{HighlightStyle, Hsla};
use ropey::Rope;
use std::ops::Range;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

use crate::theme::{ActiveTheme, ThemeAppearance};

pub struct SimpleHighlighter {
    _language: String,
    parser: Parser,
    query: Option<Query>,
    tree: Option<Tree>,
}

impl SimpleHighlighter {
    pub fn new(lang: &str) -> Self {
        let mut parser = Parser::new();
        let query = match lang {
            "rust" => Self::setup_language(
                &mut parser,
                tree_sitter_rust::LANGUAGE.into(),
                include_str!("languages/rust/highlights.scm"),
            ),
            "javascript" | "js" => Self::setup_language(
                &mut parser,
                tree_sitter_javascript::LANGUAGE.into(),
                include_str!("languages/javascript/highlights.scm"),
            ),
            "typescript" | "ts" => Self::setup_language(
                &mut parser,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                include_str!("languages/typescript/highlights.scm"),
            ),
            "python" | "py" => Self::setup_language(
                &mut parser,
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY,
            ),
            "json" => Self::setup_language(
                &mut parser,
                tree_sitter_json::LANGUAGE.into(),
                include_str!("languages/json/highlights.scm"),
            ),
            "go" => Self::setup_language(
                &mut parser,
                tree_sitter_go::LANGUAGE.into(),
                include_str!("languages/go/highlights.scm"),
            ),
            "html" => Self::setup_language(
                &mut parser,
                tree_sitter_html::LANGUAGE.into(),
                include_str!("languages/html/highlights.scm"),
            ),
            "css" => Self::setup_language(
                &mut parser,
                tree_sitter_css::LANGUAGE.into(),
                tree_sitter_css::HIGHLIGHTS_QUERY,
            ),
            "c" => Self::setup_language(
                &mut parser,
                tree_sitter_c::LANGUAGE.into(),
                tree_sitter_c::HIGHLIGHT_QUERY,
            ),
            "cpp" | "c++" => Self::setup_language(
                &mut parser,
                tree_sitter_cpp::LANGUAGE.into(),
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            ),
            "bash" | "sh" => Self::setup_language(
                &mut parser,
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY,
            ),
            "markdown" | "md" => Self::setup_language(
                &mut parser,
                tree_sitter_md::LANGUAGE.into(),
                include_str!("languages/markdown/highlights.scm"),
            ),
            _ => None,
        };
        Self {
            _language: lang.to_string(),
            parser,
            query,
            tree: None,
        }
    }

    fn setup_language(
        parser: &mut Parser,
        language: tree_sitter::Language,
        query: &str,
    ) -> Option<Query> {
        parser.set_language(&language).ok()?;
        Query::new(&language, query).ok()
    }

    pub fn update(&mut self, text: &Rope) {
        let text_str = text.to_string();
        self.tree = self.parser.parse(&text_str, self.tree.as_ref());
    }

    pub fn styles(
        &mut self,
        text: &Rope,
        cx: &gpui::App,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let query = match &self.query {
            Some(q) => q,
            None => return Vec::new(),
        };

        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance == ThemeAppearance::Dark;

        let text_str = text.to_string();

        // Ensure we have a parsed tree that matches the current text
        let tree = match &self.tree {
            Some(tree) if tree.root_node().end_byte() == text_str.len() => tree.clone(),
            _ => {
                let tree = match self.parser.parse(&text_str, None) {
                    Some(t) => t,
                    None => return Vec::new(),
                };
                self.tree = Some(tree.clone());
                tree
            }
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), text_str.as_bytes());

        let mut styles = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let name = query.capture_names()[capture.index as usize];
                let range = capture.node.byte_range();
                if let Some(style) = style_for_name(name, theme, is_dark) {
                    styles.push((range, style));
                }
            }
        }
        styles.sort_by(|a, b| a.0.start.cmp(&b.0.start));

        // Merge overlapping ranges so that TextRuns align correctly with text
        unique_styles(0..text.len(), styles)
    }
}

fn style_for_name(
    name: &str,
    theme: crate::theme::UiTheme,
    is_dark: bool,
) -> Option<HighlightStyle> {
    let color = match name {
        // Comments
        "comment" => Hsla::from(gpui::rgb(0x6A9955)),
        "comment.doc" => Hsla::from(gpui::rgb(0x6A9955)),

        // Strings
        "string" => Hsla::from(gpui::rgb(0xCE9178)),
        "string.escape" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xD7BA7D))
            } else {
                Hsla::from(gpui::rgb(0xA04545))
            }
        }
        "string.regex" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xD16969))
            } else {
                Hsla::from(gpui::rgb(0xA04545))
            }
        }
        "string.special" => Hsla::from(gpui::rgb(0xCE9178)),
        "string.special.symbol" => Hsla::from(gpui::rgb(0xCE9178)),

        // Numbers and booleans
        "number" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xB5CEA8))
            } else {
                Hsla::from(gpui::rgb(0x098658))
            }
        }
        "boolean" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x569CD6))
            } else {
                Hsla::from(gpui::rgb(0x0000FF))
            }
        }

        // Functions
        "function" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xDCDCAA))
            } else {
                Hsla::from(gpui::rgb(0x795E26))
            }
        }
        "function.method" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xDCDCAA))
            } else {
                Hsla::from(gpui::rgb(0x795E26))
            }
        }
        "function.macro" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xDCDCAA))
            } else {
                Hsla::from(gpui::rgb(0x795E26))
            }
        }
        "function.builtin" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xDCDCAA))
            } else {
                Hsla::from(gpui::rgb(0x795E26))
            }
        }

        // Types
        "type" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }
        "type.builtin" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }

        // Variables and properties
        "property" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x9CDCFE))
            } else {
                Hsla::from(gpui::rgb(0x001080))
            }
        }
        "variable" => theme.text,
        "variable.member" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x9CDCFE))
            } else {
                Hsla::from(gpui::rgb(0x001080))
            }
        }
        "variable.parameter" => theme.text,
        "variable.builtin" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x9CDCFE))
            } else {
                Hsla::from(gpui::rgb(0x001080))
            }
        }
        "variable.special" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x9CDCFE))
            } else {
                Hsla::from(gpui::rgb(0x001080))
            }
        }
        "field" => theme.text,

        // Constants
        "constant" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4FC1FF))
            } else {
                Hsla::from(gpui::rgb(0x0070C1))
            }
        }
        "constant.builtin" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x569CD6))
            } else {
                Hsla::from(gpui::rgb(0x0000FF))
            }
        }
        "constructor" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }

        // Keywords
        "keyword" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x569CD6))
            } else {
                Hsla::from(gpui::rgb(0x0000FF))
            }
        }

        // Operators
        "operator" => theme.text,

        // Punctuation
        "punctuation" => theme.text_muted,
        "punctuation.bracket" => theme.text_muted,
        "punctuation.delimiter" => theme.text_muted,
        "punctuation.list_marker" => theme.text_muted,
        "punctuation.special" => theme.text_muted,

        // Tags
        "tag" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x569CD6))
            } else {
                Hsla::from(gpui::rgb(0x800000))
            }
        }
        "tag.doctype" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x569CD6))
            } else {
                Hsla::from(gpui::rgb(0x800000))
            }
        }
        "tag.error" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xF28B9A))
            } else {
                Hsla::from(gpui::rgb(0xC5060B))
            }
        }

        // Attributes
        "attribute" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }

        // Labels
        "label" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }

        // Other
        "namespace" => theme.text,
        "embedded" => theme.text_muted,
        "enum" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }
        "preproc" => theme.text_muted,
        "primary" => theme.text,
        "text.literal" => Hsla::from(gpui::rgb(0xCE9178)),
        "title" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }
        "variant" => theme.text,
        "link_text" => Hsla::from(gpui::rgb(0xCE9178)),
        "link_uri" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4FC1FF))
            } else {
                Hsla::from(gpui::rgb(0x0070C1))
            }
        }
        "hint" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x9CDCFE))
            } else {
                Hsla::from(gpui::rgb(0x001080))
            }
        }
        "predictive" => theme.text_muted,
        "escape" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xD7BA7D))
            } else {
                Hsla::from(gpui::rgb(0xA04545))
            }
        }
        "text.uri" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4FC1FF))
            } else {
                Hsla::from(gpui::rgb(0x0070C1))
            }
        }
        "text.reference" => Hsla::from(gpui::rgb(0xCE9178)),

        "none" => return None,

        _ => {
            // Fallback: try prefix for dotted names like `punctuation.bracket`
            if let Some(prefix) = name.split('.').next() {
                if prefix != name {
                    return style_for_name(prefix, theme, is_dark);
                }
            }
            return None;
        }
    };

    Some(HighlightStyle {
        color: Some(color),
        ..Default::default()
    })
}

/// Merge overlapping highlight ranges into non-overlapping segments.
/// When multiple styles overlap, the last-started style wins.
fn unique_styles(
    total_range: Range<usize>,
    styles: Vec<(Range<usize>, HighlightStyle)>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if styles.is_empty() {
        return styles;
    }

    // Create intervals: (position, is_start, style_index)
    let mut intervals: Vec<(usize, bool, usize)> = Vec::with_capacity(styles.len() * 2 + 2);
    for (i, (range, _)) in styles.iter().enumerate() {
        intervals.push((range.start, true, i));
        intervals.push((range.end, false, i));
    }

    intervals.push((total_range.start, true, usize::MAX));
    intervals.push((total_range.end, false, usize::MAX));

    // Sort by position, with ends before starts at same position.
    // This ensures we close ranges before opening new ones at boundaries.
    intervals.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut result: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut active_styles: Vec<usize> = Vec::new();
    let mut last_pos = total_range.start;

    for (pos, is_start, style_idx) in intervals {
        let is_boundary = style_idx == usize::MAX;

        if pos > last_pos && !active_styles.is_empty() {
            // Use the last active style (later-started styles override earlier ones)
            let style = styles[*active_styles.last().unwrap()].1.clone();
            result.push((last_pos..pos, style));
        }

        if !is_boundary {
            if is_start {
                active_styles.push(style_idx);
            } else {
                active_styles.retain(|&i| i != style_idx);
            }
        }

        last_pos = pos;
    }

    result
}
