use gpui::HighlightStyle;
use ropey::{ChunkCursor, Rope};
use std::collections::BTreeSet;
use std::ops::Range;
use std::time::Duration;
use streaming_iterator::StreamingIterator;
use sum_tree::Bias;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

/// Data needed to compute injection layers on a background thread.
pub struct InjectionParseData {
    /// The injection query (`injections.scm`) of the host language.
    pub(crate) query: std::sync::Arc<Query>,
}

/// A parsed injection layer: a byte range of the host document, the embedded
/// language name, its parsed tree, and the embedded text (the layer's styles
/// are in the embedded text's coordinate space and offset when composed).
#[derive(Clone)]
pub struct InjectionLayer {
    /// Byte range of the embedded region in the host document.
    pub byte_range: std::ops::Range<usize>,
    /// Language name of the embedded content.
    pub language_name: gpui::SharedString,
    /// Parsed tree of the embedded content.
    pub tree: Option<Tree>,
    /// The embedded text.
    pub text: Rope,
}

/// Process-wide cache of compiled tree-sitter queries, keyed by language
/// name. Compiling a query from `.scm` costs 15–70ms (rust/javascript) and
/// `SimpleHighlighter::new` runs on the UI thread on every theme switch
/// (live markdown clears its highlight cache), every code-file open and
/// every fence edit — sharing the compiled queries drops construction to
/// microseconds. `tree_sitter::Query` is `Send + Sync`, so sharing via
/// `Arc` is sound; parsers/trees stay per-instance (mutable parse state).
/// Only successful compilations are cached (failures keep today's
/// retry-on-next-`new` behavior).
static COMPILED_QUERIES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Query>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Number of compiled queries currently shared process-wide (tests).
#[cfg(test)]
pub fn compiled_query_cache_len() -> usize {
    COMPILED_QUERIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .len()
}

/// Get-or-compile a tree-sitter query for `lang` (cache key), compiling
/// `source` with `language` on first use.
fn compiled_query(
    lang: &str,
    language: &tree_sitter::Language,
    source: &str,
) -> Option<std::sync::Arc<Query>> {
    if let Some(hit) = COMPILED_QUERIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(lang)
    {
        return Some(hit.clone());
    }
    let compiled = std::sync::Arc::new(Query::new(language, source).ok()?);
    COMPILED_QUERIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(lang.to_string(), compiled.clone());
    Some(compiled)
}

pub struct SimpleHighlighter {
    _language: String,
    parser: Parser,
    query: Option<std::sync::Arc<Query>>,
    injections_query: Option<std::sync::Arc<Query>>,
    tree: Option<Tree>,
    injection_layers: Vec<InjectionLayer>,
    text: Rope,
    /// Bounds a single `styles()` query run; on expiry the query halts early
    /// and remaining bytes fall back to the default style.
    query_timeout: Option<Duration>,
}

// All fields are `Send` (`Parser`/`Query`/`Tree` are marked `Send` by
// tree-sitter; `Rope` is `Send + Sync`), so the highlighter can be moved
// into background parse tasks.
//
// # SAFETY（中文安全说明）
// 此 `unsafe impl` 断言上述字段在当前依赖版本下均为 `Send`。
// 若升级 `tree-sitter`/`ropey` 后其类型变为非 `Send`，此处将构成 UB。
// 升级相关依赖时必须重新确认字段的 `Send` 性，并在 CI 中运行
// `cargo test syntax` 验证无数据竞争。
unsafe impl Send for SimpleHighlighter {}

fn clip_offset(rope: &Rope, offset: usize, bias: Bias) -> usize {
    if offset > rope.len() {
        return rope.len();
    }
    if rope.is_char_boundary(offset) {
        return offset;
    }
    if bias == Bias::Left {
        let mut o = offset;
        while o > 0 && !rope.is_char_boundary(o) {
            o -= 1;
        }
        o
    } else {
        let mut o = offset;
        let len = rope.len();
        while o < len && !rope.is_char_boundary(o) {
            o += 1;
        }
        o
    }
}

impl SimpleHighlighter {
    pub fn new(lang: &str) -> Self {
        let mut parser = Parser::new();
        let query = match lang {
            "rust" => Self::setup_language(
                &mut parser,
                tree_sitter_rust::LANGUAGE.into(),
                include_str!("languages/rust/highlights.scm"),
                "rust",
            ),
            "javascript" | "js" => Self::setup_language(
                &mut parser,
                tree_sitter_javascript::LANGUAGE.into(),
                include_str!("languages/javascript/highlights.scm"),
                "javascript",
            ),
            "typescript" | "ts" => Self::setup_language(
                &mut parser,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                include_str!("languages/typescript/highlights.scm"),
                "typescript",
            ),
            "python" | "py" => Self::setup_language(
                &mut parser,
                tree_sitter_python::LANGUAGE.into(),
                include_str!("languages/python/highlights.scm"),
                "python",
            ),
            "json" => Self::setup_language(
                &mut parser,
                tree_sitter_json::LANGUAGE.into(),
                include_str!("languages/json/highlights.scm"),
                "json",
            ),
            "go" => Self::setup_language(
                &mut parser,
                tree_sitter_go::LANGUAGE.into(),
                include_str!("languages/go/highlights.scm"),
                "go",
            ),
            "html" => Self::setup_language(
                &mut parser,
                tree_sitter_html::LANGUAGE.into(),
                include_str!("languages/html/highlights.scm"),
                "html",
            ),
            "css" | "scss" => Self::setup_language(
                &mut parser,
                tree_sitter_css::LANGUAGE.into(),
                tree_sitter_css::HIGHLIGHTS_QUERY,
                "css",
            ),
            "c" => Self::setup_language(
                &mut parser,
                tree_sitter_c::LANGUAGE.into(),
                include_str!("languages/c/highlights.scm"),
                "c",
            ),
            "cpp" | "c++" => Self::setup_language(
                &mut parser,
                tree_sitter_cpp::LANGUAGE.into(),
                tree_sitter_cpp::HIGHLIGHT_QUERY,
                "cpp",
            ),
            "bash" | "sh" => Self::setup_language(
                &mut parser,
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "bash",
            ),
            "markdown" | "md" => Self::setup_language(
                &mut parser,
                tree_sitter_md::LANGUAGE.into(),
                include_str!("languages/markdown/highlights.scm"),
                "markdown",
            ),
            "markdown_inline" | "markdown-inline" => Self::setup_language(
                &mut parser,
                tree_sitter_md::INLINE_LANGUAGE.into(),
                include_str!("languages/markdown_inline/highlights.scm"),
                "markdown_inline",
            ),
            "toml" => Self::setup_language(
                &mut parser,
                tree_sitter_toml_ng::LANGUAGE.into(),
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                "toml",
            ),
            "tsx" => Self::setup_language(
                &mut parser,
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                include_str!("languages/tsx/highlights.scm"),
                "tsx",
            ),
            "lua" => Self::setup_language(
                &mut parser,
                tree_sitter_lua::LANGUAGE.into(),
                include_str!("languages/lua/highlights.scm"),
                "lua",
            ),
            "kotlin" | "kt" => Self::setup_language(
                &mut parser,
                tree_sitter_kotlin_ng::LANGUAGE.into(),
                include_str!("languages/kotlin/highlights.scm"),
                "kotlin",
            ),
            "make" | "makefile" | "mk" => Self::setup_language(
                &mut parser,
                tree_sitter_make::LANGUAGE.into(),
                include_str!("languages/makefile/highlights.scm"),
                "make",
            ),
            "zig" => Self::setup_language(
                &mut parser,
                tree_sitter_zig::LANGUAGE.into(),
                include_str!("languages/zig/highlights.scm"),
                "zig",
            ),
            "java" => Self::setup_language(
                &mut parser,
                tree_sitter_java::LANGUAGE.into(),
                tree_sitter_java::HIGHLIGHTS_QUERY,
                "java",
            ),
            "xml" => Self::setup_language(
                &mut parser,
                tree_sitter_xml::LANGUAGE_XML.into(),
                tree_sitter_xml::XML_HIGHLIGHT_QUERY,
                "xml",
            ),
            "yaml" => Self::setup_language(
                &mut parser,
                tree_sitter_yaml::LANGUAGE.into(),
                tree_sitter_yaml::HIGHLIGHTS_QUERY,
                "yaml",
            ),
            _ => None,
        };
        let injections_query = crate::editor::syntax::LanguageRegistry::singleton()
            .language(lang)
            .and_then(|config| {
                compiled_query(
                    &("injections:".to_string() + lang),
                    &config.language,
                    &config.injections,
                )
            });

        Self {
            _language: lang.to_string(),
            parser,
            query,
            injections_query,
            tree: None,
            injection_layers: Vec::new(),
            text: Rope::new(),
            query_timeout: Some(Duration::from_secs(1)),
        }
    }

    fn setup_language(
        parser: &mut Parser,
        language: tree_sitter::Language,
        query: &str,
        cache_lang: &str,
    ) -> Option<std::sync::Arc<Query>> {
        parser.set_language(&language).ok()?;
        compiled_query(cache_lang, &language, query)
    }

    pub fn update(
        &mut self,
        edit: Option<tree_sitter::InputEdit>,
        text: &Rope,
        timeout: Option<Duration>,
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

        // NOTE: tree-sitter 0.25.10's `parse_with_options` / `QueryCursorOptions`
        // progress-callback plumbing is broken: passing `ParseOptions` makes
        // `parse_with_options` return `None` unconditionally, and passing
        // `QueryCursorOptions` makes `matches_with_options` return only ~2
        // captures regardless of the input. Both silently destroy syntax
        // highlighting, so we parse/query without options here and bound the
        // work with the C-side `set_timeout_micros` instead, which is
        // unaffected by the binding bug.
        #[allow(deprecated)]
        self.parser
            .set_timeout_micros(timeout.map_or(0, |t| t.as_micros() as u64));
        let new_tree = self.parser.parse_with_options(
            &mut |offset, _| {
                if offset >= self.text.len() {
                    return &[] as &[u8];
                }
                let (chunk, chunk_byte_ix) = self.text.chunk(offset);
                let start_in_chunk = offset - chunk_byte_ix;
                chunk.as_bytes().get(start_in_chunk..).unwrap_or(&[])
            },
            old_tree.as_ref(),
            None,
        );
        if new_tree.is_some() {
            self.tree = new_tree;
        }
        // On timeout/failure keep the previous tree (already shifted by the
        // edit): dropping it would blank the highlights until the background
        // parse lands, making the editor flicker on every keystroke in large
        // files. The return value still reports success so callers schedule a
        // background parse when the sync parse did not finish.
        self.tree.is_some()
    }

    /// gpui-component compatible interface: styles for a byte range with a given theme.
    pub fn styles(
        &mut self,
        range: &Range<usize>,
        theme: &crate::editor::syntax::HighlightTheme,
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
        // NOTE: `matches_with_options` + progress callback is broken in
        // tree-sitter 0.25.10 (see `update()` above); use plain `matches()`
        // with the C-side `set_timeout_micros` bound instead.
        if let Some(timeout) = self.query_timeout {
            #[allow(deprecated)]
            cursor.set_timeout_micros(timeout.as_micros() as u64);
        }
        let provider = RopeTextProvider(&self.text);
        // NOTE: iterate with `captures()`, not `matches()`. The captures
        // iterator yields same-node captures in *query pattern order*, so a
        // later, more specific pattern overrides an earlier catch-all on the
        // same node — the convention every official tree-sitter query is
        // written against (catch-alls first, specific rules after). The
        // matches() iterator instead emits ancestor-rooted patterns before
        // identifier-rooted ones, which inverts that priority and makes
        // generic rules override structural captures (e.g. every function
        // name painted as a plain variable).
        let mut captures = cursor.captures(query, tree.root_node(), provider);

        let mut styles = Vec::new();
        while let Some((mat, cap_ix)) = captures.next() {
            let capture = mat.captures[*cap_ix];
            let name = query.capture_names()[capture.index as usize];
            let cap_range = capture.node.byte_range();
            let mut s = cap_range.start.max(range.start).min(range.end);
            let mut e = cap_range.end.min(range.end).max(s);
            // Ensure byte offsets are on char boundaries to avoid invalid UTF-8 slices
            s = clip_offset(&self.text, s, Bias::Right);
            e = clip_offset(&self.text, e, Bias::Left);
            if e > s {
                styles.push((s..e, theme.style(name).unwrap_or_default()));
            }
        }
        styles.sort_by_key(|a| a.0.start);

        // Empty range (e.g. cursor on an empty document): a single default
        // segment, like the pre-injection behavior.
        if range.is_empty() {
            return vec![(range.start..range.end, HighlightStyle::default())];
        }
        // If the matched styles is empty, return a default range.
        if styles.is_empty() {
            styles.push((range.start..range.end, HighlightStyle::default()));
        }

        // Compose embedded-language (injection) styles on top of the host
        // styles. Each layer's subtree styles are in the embedded text's
        // coordinate space; offset them into the host document.
        let layers = std::mem::take(&mut self.injection_layers);
        for layer in &layers {
            let Some(tree) = &layer.tree else {
                continue;
            };
            let Some(config) =
                crate::editor::syntax::LanguageRegistry::singleton().language(&layer.language_name)
            else {
                continue;
            };
            let Ok(sub_query) = Query::new(&config.language, &config.highlights) else {
                continue;
            };

            let mut cursor = QueryCursor::new();
            let provider = RopeTextProvider(&layer.text);
            // `captures()` — see the host-loop note above: same-node captures
            // must resolve by pattern order so specific rules win.
            let mut captures = cursor.captures(&sub_query, tree.root_node(), provider);
            while let Some((mat, cap_ix)) = captures.next() {
                let capture = mat.captures[*cap_ix];
                let name = sub_query.capture_names()[capture.index as usize];
                let cap_range = capture.node.byte_range();
                let mut s = layer.byte_range.start + cap_range.start;
                let mut e = layer.byte_range.start + cap_range.end;
                // Guard against a stale layer after an edit: the layer's
                // byte range may no longer fit the (edited) host text.
                if s >= self.text.len() || e > self.text.len() {
                    continue;
                }
                // Clip to the host range and char boundaries.
                if e <= range.start || s >= range.end {
                    continue;
                }
                s = s.max(range.start);
                e = e.min(range.end);
                s = clip_offset(&self.text, s, Bias::Right);
                e = clip_offset(&self.text, e, Bias::Left);
                if e > s {
                    if let Some(style) = theme.style(name) {
                        styles.push((s..e, style));
                    }
                }
            }
        }
        self.injection_layers = layers;

        styles.sort_by_key(|a| a.0.start);
        unique_styles(range, styles)
    }

    // Additional stubs for gpui-component compatibility
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Whether the language's highlight query compiled and loaded. A failed
    /// `Query::new` silently disables highlighting (returns `None`), so tests
    /// assert on this to catch broken query files.
    #[cfg(test)]
    pub(crate) fn query_loaded(&self) -> bool {
        self.query.is_some()
    }

    /// Whether the language's injections query compiled (if it has one).
    /// Failures are silently swallowed in `new()` and would disable all
    /// embedded-language highlighting.
    #[cfg(test)]
    pub(crate) fn injections_query_loaded(&self) -> bool {
        self.injections_query.is_some()
    }

    pub fn language(&self) -> &str {
        &self._language
    }

    pub fn injection_parse_data(&self) -> Option<InjectionParseData> {
        self.injections_query
            .clone()
            .map(|query| InjectionParseData { query })
    }

    pub fn apply_background_tree(
        &mut self,
        tree: Tree,
        _text: &Rope,
        injection_layers: std::collections::HashMap<gpui::SharedString, InjectionLayer>,
    ) {
        self.tree = Some(tree);
        self.injection_layers = injection_layers.into_values().collect();
    }

    /// Find embedded regions (`injections.scm` of the host language), parse
    /// each embedded language's content and return one layer per language.
    /// Single-level only (no recursive injection).
    pub fn compute_injection_layers(
        data: InjectionParseData,
        tree: &Tree,
        text: &Rope,
    ) -> std::collections::HashMap<gpui::SharedString, InjectionLayer> {
        // 1. Match the host injections query for (content_range, language).
        let mut regions: Vec<(Range<usize>, gpui::SharedString)> = Vec::new();
        {
            let mut cursor = QueryCursor::new();
            let provider = RopeTextProvider(text);
            let mut matches = cursor.matches(&data.query, tree.root_node(), provider);
            while let Some(m) = matches.next() {
                let mut content: Option<Range<usize>> = None;
                let mut language: Option<gpui::SharedString> = None;
                for capture in m.captures {
                    let name = data.query.capture_names()[capture.index as usize];
                    let r = capture.node.byte_range();
                    match name {
                        "injection.content" => content = Some(r),
                        "injection.language" => {
                            language = Some(gpui::SharedString::from(
                                text.slice(r).to_string().trim().to_owned(),
                            ))
                        }
                        _ => {}
                    }
                }
                // `(#set! injection.language "js")` is a property setting, not
                // a capture — read it from the pattern's settings.
                if language.is_none() {
                    for prop in data.query.property_settings(m.pattern_index) {
                        if prop.key.as_ref() == "injection.language" {
                            if let Some(value) = prop.value.as_ref() {
                                language = Some(gpui::SharedString::from(value.as_ref()));
                            }
                            break;
                        }
                    }
                }
                if let (Some(content), Some(language)) = (content, language) {
                    regions.push((content, language));
                }
            }
        }

        // 2. Group by language.
        let mut by_lang: std::collections::HashMap<gpui::SharedString, Vec<Range<usize>>> =
            std::collections::HashMap::new();
        for (range, lang) in regions {
            by_lang.entry(lang).or_default().push(range);
        }

        // 3. Parse each language's embedded content (merged ranges).
        let mut layers = std::collections::HashMap::new();
        for (lang, ranges) in by_lang {
            let Some(config) = crate::editor::syntax::LanguageRegistry::singleton().language(&lang)
            else {
                continue;
            };
            let mut parser = Parser::new();
            if parser.set_language(&config.language).is_err() {
                continue;
            }

            // Parse each range independently (embedded regions can be
            // disjoint); combine into one layer with one tree per range is
            // complex, so parse the union span and clip styles per region.
            for range in ranges {
                if range.len() > 64 * 1024 {
                    continue; // guard against pathological embeddings
                }
                let sub_text = Rope::from(text.slice(range.clone()));
                let tree = parser.parse_with_options(
                    &mut |offset, _| {
                        if offset >= sub_text.len() {
                            return &[] as &[u8];
                        }
                        let (chunk, chunk_byte_ix) = sub_text.chunk(offset);
                        let start_in_chunk = offset - chunk_byte_ix;
                        chunk.as_bytes().get(start_in_chunk..).unwrap_or(&[])
                    },
                    None,
                    None,
                );
                let layer = InjectionLayer {
                    byte_range: range.clone(),
                    language_name: lang.clone(),
                    tree,
                    text: sub_text,
                };
                layers
                    .entry(lang.clone())
                    .and_modify(|existing: &mut InjectionLayer| {
                        // Keep the first (or largest) layer per language;
                        // multi-region embedding is rare.
                        if range.len() > existing.byte_range.len() {
                            *existing = layer.clone();
                        }
                    })
                    .or_insert(layer);
            }
        }

        layers
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
        if !self.at_first && !self.cursor.next() {
            return None;
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

    let mut result: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(styles.len());
    let mut active_styles: Vec<usize> = Vec::with_capacity(8);
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
    use crate::editor::syntax::{HighlightTheme, HighlightThemeStyle};
    use crate::theme::ThemeAppearance;
    use crate::theme::syntax_theme::SyntaxColors;

    fn test_theme() -> HighlightTheme {
        HighlightTheme {
            name: "test".into(),
            appearance: ThemeAppearance::Dark,
            style: HighlightThemeStyle {
                syntax: SyntaxColors {
                    keyword: Some(crate::editor::syntax::ThemeStyle::color(gpui::red())),
                    type_: Some(crate::editor::syntax::ThemeStyle::color(gpui::blue())),
                    function: Some(crate::editor::syntax::ThemeStyle::color(gpui::green())),
                    number: Some(crate::editor::syntax::ThemeStyle::color(gpui::yellow())),
                    string: Some(crate::editor::syntax::ThemeStyle::color(gpui::blue())),
                    comment: Some(crate::editor::syntax::ThemeStyle::color(gpui::red())),
                    property: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0x9b59b6).into(),
                    )),
                    constant: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0x00bcd4).into(),
                    )),
                    tag: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0x9b59b6).into(),
                    )),
                    attribute: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0xe91e63).into(),
                    )),
                    variable: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0xffeb3b).into(),
                    )),
                    variable_special: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0xff9800).into(),
                    )),
                    comment_doc: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0x607d8b).into(),
                    )),
                    punctuation: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0xcaccca).into(),
                    )),
                    punctuation_bracket: Some(crate::editor::syntax::ThemeStyle::color(
                        gpui::rgb(0xcaccca).into(),
                    )),
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
    fn fold_header_and_tail_brackets_get_same_style() {
        // 回归锁定：折叠占位符两侧括号的着色基础必须一致——
        // 表头行（如 `struct Engine {`）走整行 range 的 styles() 调用，
        // 折叠 tail（收尾行的 `}`）走 tail_start..tail_end 的 styles() 调用，
        // 两次调用对同类括号必须返回相同的 capture 样式。
        let code = "struct Engine {\n    x: u32,\n}\n";
        let mut h = SimpleHighlighter::new("rust");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);
        let theme = test_theme();

        // Row 0 = "struct Engine {\n" → 0..16；tail `}` = 28..29
        let header_styles = h.styles(&(0..16), &theme);
        let tail_styles = h.styles(&(28..29), &theme);

        let header_brace = header_styles
            .iter()
            .find(|(r, _)| &code[r.clone()] == "{")
            .map(|(_, s)| s.color)
            .expect("header range must style the `{`");
        let tail_brace = tail_styles
            .iter()
            .find(|(r, _)| &code[r.clone()] == "}")
            .map(|(_, s)| s.color)
            .expect("tail range must style the `}`");
        assert_eq!(
            header_brace, tail_brace,
            "fold header and tail brackets must get the same style"
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

    /// HTML embeds `<script>` (JavaScript) and `<style>` (CSS); the injected
    /// content must be highlighted with its own language's query, composed
    /// onto the host (HTML) styles at the correct byte offsets.
    #[test]
    fn test_html_injection_script_and_style() {
        let code =
            "<html>\n<style>body { color: red; }</style>\n<script>let x = 1;</script>\n</html>\n";
        let mut h = SimpleHighlighter::new("html");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let data = h
            .injection_parse_data()
            .expect("html has an injections query");
        let tree = h.tree().expect("host tree").clone();
        let layers = SimpleHighlighter::compute_injection_layers(data, &tree, &rope);
        assert!(
            layers.len() >= 2,
            "expected script + style layers, got {}",
            layers.len()
        );

        h.apply_background_tree(tree, &rope, layers);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);

        // `let` inside <script> is a JavaScript keyword → red.
        let let_pos = code.find("let").expect("let in script");
        let run = styles
            .iter()
            .find(|(r, _)| r.start <= let_pos && r.end > let_pos)
            .unwrap_or_else(|| panic!("no style covers `let` at {let_pos}"));
        assert_eq!(
            run.1.color,
            Some(gpui::red()),
            "`let` inside <script> should be keyword-colored (injected JS)"
        );

        // Inside <style>, some run must carry a non-default color from the
        // injected CSS query (e.g. the `body` selector / `red` value).
        let style_start = code.find("<style>").expect("style tag") + "<style>".len();
        let style_end = code.find("</style>").expect("style close");
        let styled = styles
            .iter()
            .any(|(r, s)| r.start >= style_start && r.end <= style_end && s.color.is_some());
        assert!(
            styled,
            "expected injected CSS styles inside <style> (range {style_start}..{style_end})"
        );
    }

    /// Every registered language must have a compiled highlight query. A
    /// failing `Query::new` silently disables highlighting, which is how two
    /// regressions went unnoticed before (kotlin query written for a
    /// different grammar; tsx wired to the TS-additions-only query).
    /// 回归：主题切换/打开代码文件时 `SimpleHighlighter::new` 不得重复
    /// 编译 tree-sitter 查询（曾每次 15–70ms，主线程同步执行导致卡死）。
    /// 编译好的查询进程内共享：同一语言只编译一次，且共享结果一致。
    #[test]
    fn compiled_queries_are_shared() {
        // 注：测试进程内并行共享同一缓存，不可断言增长量，只断言
        // “重复构造不再新增编译”（复用）+ 结果一致。
        let mut a = SimpleHighlighter::new("rust");
        let mut b = SimpleHighlighter::new("rust");
        assert!(
            a.query_loaded() && b.query_loaded(),
            "rust highlight query must compile"
        );
        let cached = super::compiled_query_cache_len();
        assert!(cached >= 1, "rust query must be in the shared cache");
        let _ = SimpleHighlighter::new("rust");
        assert_eq!(
            super::compiled_query_cache_len(),
            cached,
            "repeat construction must reuse the cache, not recompile"
        );
        let code = "fn main() {\n    let x = 1;\n}\n";
        let theme = test_theme();
        let styles_of = |h: &mut SimpleHighlighter| {
            let rope = Rope::from_str(code);
            h.update(None, &rope, None);
            h.styles(&(0..code.len()), &theme)
        };
        assert_eq!(styles_of(&mut a), styles_of(&mut b));
    }

    #[test]
    fn test_all_languages_load_queries() {
        let languages_with_injections = [
            "rust",
            "zig",
            "markdown",
            "html",
            "javascript",
            "typescript",
        ];
        for lang in crate::editor::syntax::languages::Language::all() {
            let name = lang.name();
            if name == "text" {
                continue; // Plain text intentionally has no query
            }
            let h = SimpleHighlighter::new(name);
            assert!(
                h.query_loaded(),
                "highlight query failed to load for language {name:?}"
            );
            if languages_with_injections.contains(&name) {
                assert!(
                    h.injections_query_loaded(),
                    "injections query failed to load for language {name:?}"
                );
            }
        }
    }

    /// Function parameters must use the plain variable color, not the
    /// `variable.special` color reserved for `self`/`this` (VSCode parity).
    #[test]
    fn test_rust_parameter_uses_variable_color() {
        let code = "fn add(left: i32) { self }";
        let mut h = SimpleHighlighter::new("rust");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);

        let variable = gpui::rgb(0xffeb3b).into();
        let variable_special = gpui::rgb(0xff9800).into();
        let left_pos = code.find("left").expect("param in code");
        let left_run = styles
            .iter()
            .find(|(r, _)| r.start <= left_pos && r.end > left_pos)
            .expect("style covers `left`");
        assert_eq!(
            left_run.1.color,
            Some(variable),
            "parameter `left` should use the plain variable color"
        );
        assert_ne!(
            left_run.1.color,
            Some(variable_special),
            "parameter must not use variable.special (self/this) color"
        );

        // `self` keeps the special-variable color.
        let self_pos = code.find("self").expect("self in code");
        let self_run = styles
            .iter()
            .find(|(r, _)| r.start <= self_pos && r.end > self_pos)
            .expect("style covers `self`");
        assert_eq!(
            self_run.1.color,
            Some(variable_special),
            "`self` should keep the variable.special color"
        );
    }

    /// TSX must be highlighted with the full combined query: JavaScript base
    /// rules (strings, comments, functions, keywords) plus JSX rules
    /// (element names as tags, attributes).
    #[test]
    fn test_tsx_highlights_js_base_and_jsx() {
        let code = r#"const x: number = 1;
// note
export function App() { return <div className="a">hi</div>; }
"#;
        let mut h = SimpleHighlighter::new("tsx");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);

        // JS base rules: keyword `const`, comment, string.
        let colored = |needle: &str| {
            let pos = code
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} in code"));
            styles
                .iter()
                .find(|(r, _)| r.start <= pos && r.end > pos)
                .unwrap_or_else(|| panic!("style covers `{needle}`"))
                .1
                .color
        };
        assert_eq!(colored("const"), Some(gpui::red()), "`const` is a keyword");
        assert_eq!(colored("// note"), Some(gpui::red()), "comment is styled");
        assert_eq!(colored("App"), Some(gpui::green()), "function name");
        // JSX: element name as tag, attribute name as attribute.
        assert_eq!(colored("div"), Some(gpui::rgb(0x9b59b6).into()), "JSX tag");
        assert_eq!(
            colored("className"),
            Some(gpui::rgb(0xe91e63).into()),
            "JSX attribute"
        );
    }

    /// Function definitions must win over any identifier-level pattern
    /// (the reason the `(identifier) @variable` catch-alls were removed from
    /// the vendored queries).
    #[test]
    fn test_python_function_definition_name() {
        let code = "def add(left):\n    return left\n";
        let mut h = SimpleHighlighter::new("python");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);
        let add_pos = code.find("add").expect("def name");
        let run = styles
            .iter()
            .find(|(r, _)| r.start <= add_pos && r.end > add_pos)
            .expect("style covers `add`");
        assert_eq!(
            run.1.color,
            Some(gpui::green()),
            "python def name must be function-colored, not plain variable"
        );
    }

    /// Kotlin (kotlin-ng grammar): function declarations, properties, builtin
    /// types and constructor calls. Also guards the query against the
    /// kotlin-ng node-type mismatch that previously made it fail to load.
    #[test]
    fn test_kotlin_highlights() {
        let code = "class Person(val name: String) {\n    fun greet(): String = name\n}\nfun main() {\n    val p = Person(\"a\")\n}\n";
        let mut h = SimpleHighlighter::new("kotlin");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);
        let color_at = |needle: &str| {
            let pos = code
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} in code"));
            styles
                .iter()
                .find(|(r, _)| r.start <= pos && r.end > pos)
                .unwrap_or_else(|| panic!("style covers `{needle}`"))
                .1
                .color
        };
        assert_eq!(color_at("class"), Some(gpui::red()), "keyword");
        assert_eq!(color_at("Person"), Some(gpui::blue()), "class name as type");
        assert_eq!(
            color_at("name"),
            Some(gpui::rgb(0x9b59b6).into()),
            "class property"
        );
        assert_eq!(color_at("String"), Some(gpui::blue()), "builtin type");
        assert_eq!(
            color_at("greet"),
            Some(gpui::green()),
            "function declaration"
        );
        assert_eq!(
            color_at("val p"),
            Some(gpui::red()),
            "`val` keyword colored"
        );
        assert_eq!(color_at("main"), Some(gpui::green()), "top-level fun");
    }

    /// Go: function declaration names must be function-colored (previously
    /// overridden by the `(identifier) @variable` catch-all).
    #[test]
    fn test_go_function_declaration_name() {
        let code = "func main() {\n\tprintln(\"hi\")\n}\n";
        let mut h = SimpleHighlighter::new("go");
        let rope = Rope::from_str(code);
        h.update(None, &rope, None);

        let theme = test_theme();
        let styles = h.styles(&(0..code.len()), &theme);
        let main_pos = code.find("main").expect("func name");
        let run = styles
            .iter()
            .find(|(r, _)| r.start <= main_pos && r.end > main_pos)
            .expect("style covers `main`");
        assert_eq!(
            run.1.color,
            Some(gpui::green()),
            "go func name must be function-colored"
        );
    }

    /// Java / XML / YAML：新增语言的高亮冒烟测试（至少能解析并命中若干 capture）。
    #[test]
    fn test_java_xml_yaml_highlights_smoke() {
        let theme = test_theme();

        // Java：class 名应命中 type 色，int 应命中 keyword 色
        let java_code = "public class Main {\n    int count = 42;\n}\n";
        let mut h = SimpleHighlighter::new("java");
        let rope = Rope::from_str(java_code);
        assert!(h.update(None, &rope, None));
        let styles = h.styles(&(0..java_code.len()), &theme);
        let color_at = |code: &str, needle: &str| {
            let pos = code.find(needle).expect("needle in code");
            styles
                .iter()
                .find(|(r, _)| r.start <= pos && r.end > pos)
                .map(|(_, s)| s.color)
        };
        assert_eq!(
            color_at(java_code, "Main"),
            Some(Some(gpui::blue())),
            "java class name as type"
        );
        assert_eq!(
            color_at(java_code, "int"),
            Some(Some(gpui::blue())),
            "java primitive type builtin"
        );
        assert_eq!(
            color_at(java_code, "public"),
            Some(Some(gpui::red())),
            "java modifier keyword"
        );

        // XML：标签名应命中 tag 色，属性名应命中 attribute 色
        let xml_code = "<note id=\"1\">hi</note>\n";
        let mut h = SimpleHighlighter::new("xml");
        let rope = Rope::from_str(xml_code);
        assert!(h.update(None, &rope, None));
        let styles = h.styles(&(0..xml_code.len()), &theme);
        let pos = xml_code.find("note").unwrap();
        assert!(
            styles.iter().any(|(r, _)| r.start <= pos && r.end > pos),
            "xml tag name has a style"
        );

        // YAML：键应命中 property 色，字符串值命中 string 色
        let yaml_code = "name: deepterm\nversion: 2\n";
        let mut h = SimpleHighlighter::new("yaml");
        let rope = Rope::from_str(yaml_code);
        assert!(h.update(None, &rope, None));
        let styles = h.styles(&(0..yaml_code.len()), &theme);
        let pos = yaml_code.find("name").unwrap();
        assert!(
            styles.iter().any(|(r, _)| r.start <= pos && r.end > pos),
            "yaml key has a style"
        );
    }
}
