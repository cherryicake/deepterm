use gpui::HighlightStyle;
use ropey::Rope;
use std::ops::Range;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

use crate::theme::ActiveTheme;

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

        let highlight_theme = &cx.theme().highlight_theme;
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
                if let Some(style) = highlight_theme.style(name) {
                    styles.push((range, style));
                }
            }
        }
        styles.sort_by(|a, b| a.0.start.cmp(&b.0.start));

        // Merge overlapping ranges so that TextRuns align correctly with text
        unique_styles(0..text.len(), styles)
    }
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
