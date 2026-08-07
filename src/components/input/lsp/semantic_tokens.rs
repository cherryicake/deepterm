//! Semantic highlighting tokens (LSP `textDocument/semanticTokens/range`),
//! layered on top of the built-in tree-sitter highlighter.
//!
//! Ported from gpui-component's `input/lsp/semantic_tokens.rs`.

use std::ops::Range;
use std::time::Duration;

use anyhow::Result;
use gpui::{App, Context, HighlightStyle, SharedString, Task, Window};
use lsp_types::{Position, SemanticTokens, SemanticTokensLegend};
use ropey::Rope;

use crate::components::input::{InputState, RopeExt};
use crate::components::input::lsp::Lsp;
use crate::syntax::HighlightTheme;

/// A provider of semantic highlighting tokens, layered on top of the
/// built-in tree-sitter highlighter.
///
/// This is the editor counterpart of the LSP
/// `textDocument/semanticTokens/range` request. It is installed on
/// `InputState::lsp`, fetched asynchronously when the document changes, and
/// its result is cached and composed into the render pipeline. It does
/// **not** replace the tree-sitter highlighter.
///
/// Returned tokens are delta-encoded with a numeric `token_type` that
/// indexes [`legend`](Self::legend)`.token_types`. The editor resolves each
/// type *name* against the active
/// [`HighlightTheme`](crate::syntax::HighlightTheme) at paint time — the
/// same vocabulary the tree-sitter path uses (`"keyword"`, `"comment"`,
/// `"string"`, …; `"keyword.modifier"` falls back to `"keyword"`).
pub trait DocumentRangeSemanticTokensProvider {
    /// The legend naming the numeric `token_type` field of the tokens
    /// returned by [`semantic_tokens`](Self::semantic_tokens).
    fn legend(&self) -> SemanticTokensLegend;

    /// Fetches semantic tokens for the specified byte range.
    ///
    /// textDocument/semanticTokens/range
    fn semantic_tokens(
        &self,
        text: &Rope,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<SemanticTokens>>;
}

impl Lsp {
    /// Get semantic token styles that intersect with the visible byte range,
    /// resolving each cached token's type name against `theme`.
    ///
    /// Called on every paint. The cache is sorted by start position, so this
    /// binary-searches the small window of tokens that can touch the viewport
    /// instead of scanning the whole document.
    pub(crate) fn semantic_tokens_for_range(
        &self,
        text: &Rope,
        visible_range: &Range<usize>,
        theme: &HighlightTheme,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if self.semantic_tokens.is_empty() {
            return Vec::new();
        }

        let visible_start = text.offset_to_position(visible_range.start);
        let visible_end = text.offset_to_position(visible_range.end);

        // Cache is sorted by `range.start`. A token can only touch the
        // viewport if its start is before `visible_end` (upper bound) and it
        // is not on a line entirely above the viewport's first line (lower
        // bound — tokens are single-line, so an earlier line cannot reach in).
        let hi = self
            .semantic_tokens
            .partition_point(|(range, _)| range.start < visible_end);
        let lo = self
            .semantic_tokens
            .partition_point(|(range, _)| range.start.line < visible_start.line);

        self.semantic_tokens[lo..hi]
            .iter()
            .filter_map(|(range, name)| {
                let start = text.position_to_offset(&range.start);
                let end = text.position_to_offset(&range.end);
                if start >= end || start >= visible_range.end || end <= visible_range.start {
                    return None;
                }

                let style = theme.style(name.as_ref())?;
                Some((start..end, style))
            })
            .collect()
    }

    pub(crate) fn update_semantic_tokens(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.semantic_tokens_provider.as_ref() else {
            return;
        };

        let provider = provider.clone();
        let legend = provider.legend();
        let text = text.clone();
        // Fetch the whole document; results are cached and filtered to the
        // viewport at paint time, so a scroll never needs a refetch.
        let range = 0..text.len();
        let input_state = cx.entity();

        // debounce timer 100ms
        self._semantic_tokens_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;

            let task_result = cx
                .update(|window, cx| provider.semantic_tokens(&text, range, window, cx))
                .ok();

            if let Some(task) = task_result {
                if let Ok(tokens) = task.await {
                    let decoded = decode_semantic_tokens(&tokens, &legend);
                    let _ = input_state.update(cx, |input_state, cx| {
                        if decoded != input_state.lsp.semantic_tokens {
                            input_state.lsp.semantic_tokens = decoded;
                            cx.notify();
                        }
                    });
                }
            }
        });
    }
}

/// Decode the LSP delta-encoding of `tokens` into absolute
/// (position-range, type-name) pairs, sorted by start position.
///
/// The type name is looked up in `legend.token_types`; tokens whose
/// `token_type` index is out of bounds are skipped. Color resolution is
/// deferred to paint time so theme switches take effect without a refetch.
pub(crate) fn decode_semantic_tokens(
    tokens: &SemanticTokens,
    legend: &SemanticTokensLegend,
) -> Vec<(lsp_types::Range, SharedString)> {
    let names: Vec<SharedString> = legend
        .token_types
        .iter()
        .map(|t| SharedString::from(t.as_str().to_owned()))
        .collect();

    let mut out = Vec::with_capacity(tokens.data.len());
    let mut line: u32 = 0;
    let mut character: u32 = 0;

    for token in &tokens.data {
        if token.delta_line > 0 {
            line += token.delta_line;
            character = token.delta_start;
        } else {
            character += token.delta_start;
        }

        let Some(name) = names.get(token.token_type as usize) else {
            continue;
        };

        let start = Position::new(line, character);
        let end = Position::new(line, character + token.length);
        out.push((lsp_types::Range { start, end }, name.clone()));
    }

    out.sort_by_key(|(range, _)| range.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{HighlightTheme, HighlightThemeStyle, SyntaxColors, ThemeStyle};

    fn test_theme() -> HighlightTheme {
        HighlightTheme {
            name: "test".into(),
            appearance: crate::theme::ThemeAppearance::Dark,
            style: HighlightThemeStyle {
                syntax: SyntaxColors {
                    keyword: Some(ThemeStyle::color(gpui::red())),
                    comment: Some(ThemeStyle::color(gpui::green())),
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    fn legend() -> SemanticTokensLegend {
        SemanticTokensLegend {
            token_types: vec![
                lsp_types::SemanticTokenType::KEYWORD,
                lsp_types::SemanticTokenType::COMMENT,
            ],
            token_modifiers: vec![],
        }
    }

    #[test]
    fn test_decode_semantic_tokens_delta() {
        let tokens = SemanticTokens {
            result_id: None,
            data: vec![
                lsp_types::SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 4,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                lsp_types::SemanticToken {
                    delta_line: 1,
                    delta_start: 2,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let decoded = decode_semantic_tokens(&tokens, &legend());
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].0.start, Position::new(0, 0));
        assert_eq!(decoded[0].0.end, Position::new(0, 4));
        assert_eq!(decoded[0].1.as_ref(), "keyword");
        assert_eq!(decoded[1].0.start, Position::new(1, 2));
        assert_eq!(decoded[1].0.end, Position::new(1, 7));
        assert_eq!(decoded[1].1.as_ref(), "comment");
    }

    #[test]
    fn test_decode_skips_out_of_legend_index() {
        let tokens = SemanticTokens {
            result_id: None,
            data: vec![lsp_types::SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 3,
                token_type: 99,
                token_modifiers_bitset: 0,
            }],
        };
        assert!(decode_semantic_tokens(&tokens, &legend()).is_empty());
    }

    #[test]
    fn test_for_range_resolves_and_windows() {
        let text = Rope::from("SELECT * FROM users\n-- a comment line\n");
        let theme = test_theme();

        let mut lsp = Lsp::default();
        lsp.semantic_tokens = vec![
            (
                lsp_types::Range {
                    start: Position::new(0, 0),
                    end: Position::new(0, 6),
                },
                SharedString::from("keyword"),
            ),
            (
                lsp_types::Range {
                    start: Position::new(1, 0),
                    end: Position::new(1, 17),
                },
                SharedString::from("comment"),
            ),
        ];

        // Visible range covering only line 0 (bytes 0..19).
        let styles = lsp.semantic_tokens_for_range(&text, &(0..19), &theme);
        assert_eq!(styles.len(), 1, "only the line-0 token should be windowed in");
        assert_eq!(styles[0].0, 0..6, "keyword token maps to bytes 0..6");
        assert!(
            styles[0].1 != HighlightStyle::default(),
            "'keyword' should resolve to a non-default style"
        );
    }

    #[test]
    fn test_for_range_binary_search_window() {
        let text = Rope::from("foo bar\n".repeat(100).as_str());
        let theme = test_theme();

        let mut lsp = Lsp::default();
        lsp.semantic_tokens = (0..100u32)
            .map(|line| {
                (
                    lsp_types::Range {
                        start: Position::new(line, 0),
                        end: Position::new(line, 3),
                    },
                    SharedString::from("keyword"),
                )
            })
            .collect();

        let line_bytes = "foo bar\n".len();
        let start = 50 * line_bytes;
        let styles = lsp.semantic_tokens_for_range(&text, &(start..start + 3), &theme);
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].0, start..start + 3);

        assert!(
            lsp.semantic_tokens_for_range(&text, &(0..0), &theme)
                .is_empty()
        );
    }
}
