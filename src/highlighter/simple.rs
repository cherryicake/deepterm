use gpui::HighlightStyle;
use ropey::{ChunkCursor, Rope};
use std::collections::BTreeSet;
use std::ops::Range;
use streaming_iterator::StreamingIterator;
use sum_tree::Bias;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

use crate::components::input::editor::RopeExt as _;

pub struct InjectionParseData;
pub struct InjectionLayer;

pub struct SimpleHighlighter {
    _language: String,
    parser: Parser,
    query: Option<Query>,
    tree: Option<Tree>,
    text: Rope,
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
            text: Rope::new(),
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

    pub fn update(
        &mut self,
        edit: Option<tree_sitter::InputEdit>,
        text: &Rope,
        _timeout: Option<std::time::Duration>,
    ) -> bool {
        self.text = text.clone();
        let mut old_tree = self.tree.clone();

        if let Some(tree) = old_tree.as_mut() {
            let tree_len = tree.root_node().end_byte();
            let text_len = self.text.len();
            if tree_len > 0 && text_len > 0 {
                if let Some(edit) = edit {
                    tree.edit(&edit);
                }
            } else {
                // Cannot safely reuse tree for empty <-> non-empty transitions
                old_tree = None;
            }
        }

        self.tree = self.parser.parse_with(
            &mut |offset, _| {
                if offset >= self.text.len() {
                    return &[] as &[u8];
                }
                let (chunk, chunk_byte_ix) = self.text.chunk(offset);
                let start_in_chunk = offset - chunk_byte_ix;
                chunk.as_bytes().get(start_in_chunk..).unwrap_or(&[])
            },
            old_tree.as_ref(),
        );
        true
    }

    /// gpui-component compatible interface: styles for a byte range with a given theme.
    pub fn styles(
        &mut self,
        range: &Range<usize>,
        theme: &crate::highlighter::HighlightTheme,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let query = match &self.query {
            Some(q) => q,
            None => return Vec::new(),
        };

        // Ensure we have a parsed tree
        let tree = match &self.tree {
            Some(tree) => tree.clone(),
            None => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());
        let provider = RopeTextProvider(&self.text);
        let mut matches = cursor.matches(query, tree.root_node(), provider);

        let mut styles = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let name = query.capture_names()[capture.index as usize];
                let cap_range = capture.node.byte_range();
                let mut s = cap_range.start.max(range.start).min(range.end);
                let mut e = cap_range.end.min(range.end).max(s);
                // Ensure byte offsets are on char boundaries to avoid invalid UTF-8 slices
                s = self.text.clip_offset(s, Bias::Right);
                e = self.text.clip_offset(e, Bias::Left);
                if e > s {
                    styles.push((s..e, theme.style(name).unwrap_or_default()));
                }
            }
        }
        styles.sort_by(|a, b| a.0.start.cmp(&b.0.start));

        // If the matched styles is empty, return a default range.
        if styles.is_empty() {
            return vec![(range.start..range.end, HighlightStyle::default())];
        }

        unique_styles(range, styles)
    }

    // Additional stubs for gpui-component compatibility
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    pub fn language(&self) -> &str {
        &self._language
    }

    pub fn injection_parse_data(&self) -> Option<InjectionParseData> {
        None
    }

    pub fn apply_background_tree(
        &mut self,
        tree: Tree,
        _text: &Rope,
        _injection_layers: std::collections::HashMap<gpui::SharedString, InjectionLayer>,
    ) {
        self.tree = Some(tree);
    }

    pub fn compute_injection_layers(
        _data: InjectionParseData,
        _tree: &Tree,
        _text: &Rope,
    ) -> std::collections::HashMap<gpui::SharedString, InjectionLayer> {
        std::collections::HashMap::new()
    }
}

struct RopeTextProvider<'a>(&'a Rope);
struct RopeByteChunks<'a> {
    cursor: ChunkCursor<'a>,
    node_start: usize,
    node_end: usize,
    at_first: bool,
}

impl<'a> tree_sitter::TextProvider<&'a [u8]> for RopeTextProvider<'a> {
    type I = RopeByteChunks<'a>;

    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        let range = node.byte_range();
        let cursor = self.0.chunk_cursor_at(range.start);
        RopeByteChunks {
            cursor,
            node_start: range.start,
            node_end: range.end,
            at_first: true,
        }
    }
}

impl<'a> Iterator for RopeByteChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if !self.at_first {
            if !self.cursor.next() {
                return None;
            }
        }
        self.at_first = false;

        let chunk_byte_start = self.cursor.byte_offset();
        if chunk_byte_start >= self.node_end {
            return None;
        }

        let chunk = self.cursor.chunk().as_bytes();
        let start_in_chunk = self.node_start.saturating_sub(chunk_byte_start);
        let end_in_chunk = (self.node_end - chunk_byte_start).min(chunk.len());

        if start_in_chunk >= end_in_chunk {
            return None;
        }

        Some(&chunk[start_in_chunk..end_in_chunk])
    }
}

/// Merge overlapping highlight ranges into non-overlapping segments.
/// When multiple styles overlap, the later-started styles override earlier ones.
/// Gaps are filled with `HighlightStyle::default()` so the result always covers
/// the entire `total_range`.
fn unique_styles(
    total_range: &Range<usize>,
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

    // Track significant intervals (where style ranges end) for merging decisions.
    let mut significant_intervals: BTreeSet<usize> = BTreeSet::new();
    for (range, _) in &styles {
        significant_intervals.insert(range.end);
    }

    let mut result: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut active_styles: Vec<usize> = Vec::new();
    let mut last_pos = total_range.start;

    for (pos, is_start, style_idx) in intervals {
        let is_boundary = style_idx == usize::MAX;

        if pos > last_pos {
            let interval = last_pos..pos;
            let combined_style = if active_styles.is_empty() {
                HighlightStyle::default()
            } else {
                let mut combined = HighlightStyle::default();
                for &idx in &active_styles {
                    merge_highlight_style(&mut combined, &styles[idx].1);
                }
                combined
            };
            result.push((interval, combined_style));
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

    // Merge adjacent ranges with the same style, but not across significant boundaries.
    let mut merged: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(result.len());
    for (range, style) in result {
        if let Some((last_range, last_style)) = merged.last_mut() {
            if last_range.end == range.start
                && *last_style == style
                && !significant_intervals.contains(&range.start)
            {
                last_range.end = range.end;
                continue;
            }
        }
        merged.push((range, style));
    }

    merged
}

/// Merge `other` style on top of `style`.
fn merge_highlight_style(style: &mut HighlightStyle, other: &HighlightStyle) {
    if let Some(color) = other.color {
        style.color = Some(color);
    }
    if let Some(font_weight) = other.font_weight {
        style.font_weight = Some(font_weight);
    }
    if let Some(font_style) = other.font_style {
        style.font_style = Some(font_style);
    }
    if let Some(background_color) = other.background_color {
        style.background_color = Some(background_color);
    }
    if let Some(underline) = other.underline {
        style.underline = Some(underline);
    }
    if let Some(strikethrough) = other.strikethrough {
        style.strikethrough = Some(strikethrough);
    }
    if let Some(fade_out) = other.fade_out {
        style.fade_out = Some(fade_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlighter::{registry::SyntaxColors, HighlightTheme, HighlightThemeStyle};
    use crate::theme::ThemeAppearance;

    fn test_theme() -> HighlightTheme {
        HighlightTheme {
            name: "test".into(),
            appearance: ThemeAppearance::Dark,
            style: HighlightThemeStyle {
                syntax: SyntaxColors {
                    keyword: Some(crate::highlighter::ThemeStyle::color(gpui::red())),
                    type_: Some(crate::highlighter::ThemeStyle::color(gpui::blue())),
                    function: Some(crate::highlighter::ThemeStyle::color(gpui::green())),
                    number: Some(crate::highlighter::ThemeStyle::color(gpui::yellow())),
                    string: Some(crate::highlighter::ThemeStyle::color(gpui::blue())),
                    comment: Some(crate::highlighter::ThemeStyle::color(gpui::red())),
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    #[test]
    fn test_rust_highlight_let() {
        let code = "let x = 1;\nconst Y: i32 = 2;\nfn main() {}\n";
        let mut h = SimpleHighlighter::new("rust");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);
        for (range, style) in &styles {
            println!(
                "{:?}: {:?} -> {:?}",
                range,
                &code[range.clone()],
                style.color
            );
        }

        // Check that 'let' is highlighted as keyword
        let let_range = 0..3;
        assert!(
            styles.iter().any(|(r, s)| r.start <= let_range.start
                && r.end >= let_range.end
                && s.color == Some(gpui::red())),
            "'let' should be highlighted as keyword (red)"
        );

        // Check that 'const' is highlighted as keyword
        let const_range = 11..16;
        assert!(
            styles.iter().any(|(r, s)| r.start <= const_range.start
                && r.end >= const_range.end
                && s.color == Some(gpui::red())),
            "'const' should be highlighted as keyword (red)"
        );

        // Check that 'fn' is highlighted as keyword
        let fn_range = 29..31;
        assert!(
            styles.iter().any(|(r, s)| r.start <= fn_range.start
                && r.end >= fn_range.end
                && s.color == Some(gpui::red())),
            "'fn' should be highlighted as keyword (red)"
        );

        // Check that 'i32' is highlighted as type
        let i32_range = 20..23;
        assert!(
            styles.iter().any(|(r, s)| r.start <= i32_range.start
                && r.end >= i32_range.end
                && s.color == Some(gpui::blue())),
            "'i32' should be highlighted as type (blue)"
        );

        // Check that 'main' is highlighted as function
        let main_range = 32..36;
        assert!(
            styles.iter().any(|(r, s)| r.start <= main_range.start
                && r.end >= main_range.end
                && s.color == Some(gpui::green())),
            "'main' should be highlighted as function (green)"
        );
    }

    #[test]
    fn test_rust_highlight_use_path() {
        let code = "use crate::components::title_bar::TitleBar;\n";
        let mut h = SimpleHighlighter::new("rust");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);
        for (range, style) in &styles {
            println!(
                "{:?}: {:?} -> {:?}",
                range,
                &code[range.clone()],
                style.color
            );
        }

        // Check coverage
        let total_len: usize = styles.iter().map(|(r, _)| r.len()).sum();
        assert_eq!(total_len, code.len(), "styles should cover entire range");
    }

    #[test]
    fn test_delete_all_cjk_text_no_panic() {
        let code = "fn 中文() {}";
        let mut h = SimpleHighlighter::new("rust");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        // Simulate cutting all text: empty rope
        let empty_rope = Rope::new();
        h.update(None, &empty_rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..0), &theme);
        assert_eq!(styles.len(), 1);
    }

    #[test]
    fn test_undo_restores_highlights() {
        let code = "fn main() {}";
        let mut h = SimpleHighlighter::new("rust");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let original_styles = h.styles(&(0..code.len()), &theme);
        // Ensure we have actual highlight colors (keyword 'fn' or function 'main')
        let has_color = original_styles.iter().any(|(_, s)| s.color.is_some());
        assert!(has_color, "original text should have syntax highlights");

        // Simulate cut-all (empty text)
        let empty_rope = Rope::new();
        h.update(None, &empty_rope, None);

        // Undo: restore original text
        h.update(None, &rope, None);

        let restored_styles = h.styles(&(0..code.len()), &theme);
        let restored_color = restored_styles.iter().any(|(_, s)| s.color.is_some());
        assert!(
            restored_color,
            "restored text should still have syntax highlights after undo"
        );
    }
}
