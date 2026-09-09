//! Code-block highlighting cache for live markdown.
//!
//! Thin wrapper over the existing [`crate::editor::markdown::highlight_code_runs`]
//! (tree-sitter via `SyntaxHighlighter`): results are keyed by
//! `(language, content)` so typing elsewhere never recomputes. Async
//! background refresh is a later perf step; misses compute synchronously
//! like the read-only preview does today.
//!
//! [`HighlightCache::runs_for_block`] highlights a whole fenced block as one
//! document and splits the runs per line, so multi-line constructs (block
//! comments, raw strings, template literals) keep their spans across lines —
//! the parity guise gets from threading `LineState` through
//! `Language::line`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::TextRun;

use crate::editor::markdown::highlight_code_runs;
use crate::theme::AppTheme;

/// A cached highlight result: one flat run list ([`HighlightCache::runs_for`])
/// or per-line run lists ([`HighlightCache::runs_for_block`]).
#[derive(Debug, Clone)]
enum CacheValue {
    Flat(Vec<TextRun>),
    PerLine(Vec<Vec<TextRun>>),
}

type CacheMap = HashMap<(String, String), CacheValue>;

#[derive(Debug, Clone, Default)]
pub struct HighlightCache {
    inner: Rc<RefCell<CacheMap>>,
}

impl HighlightCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Highlighted runs for one line of code. `base` carries the mono font.
    ///
    /// Prefer [`Self::runs_for_block`] for fenced blocks: per-line parses
    /// lose multi-line syntax context.
    pub fn runs_for(
        &self,
        language: &str,
        content: &str,
        base: &gpui::TextStyle,
        theme: &AppTheme,
    ) -> Vec<TextRun> {
        let key = (language.to_string(), content.to_string());
        if let Some(CacheValue::Flat(hit)) = self.inner.borrow().get(&key) {
            return hit.clone();
        }
        let runs = highlight_code_runs(content, language, base, theme);
        self.inner
            .borrow_mut()
            .insert(key, CacheValue::Flat(runs.clone()));
        runs
    }

    /// Per-line highlighted runs for a whole fenced block.
    ///
    /// The block is highlighted as ONE document (so multi-line constructs —
    /// block comments, raw strings, template literals — keep their spans
    /// across lines, matching guise's state-threaded `Language::line`), then
    /// the resulting runs are split at the line boundaries. Keyed by the
    /// joined block content, so typing anywhere in the block recomputes it
    /// once. `lines` are the verbatim source lines (no trailing newline).
    pub fn runs_for_block(
        &self,
        language: &str,
        lines: &[&str],
        base: &gpui::TextStyle,
        theme: &AppTheme,
    ) -> Vec<Vec<TextRun>> {
        let content = lines.join("\n");
        let key = (language.to_string(), content.clone());
        if let Some(CacheValue::PerLine(hit)) = self.inner.borrow().get(&key) {
            return hit.clone();
        }
        let block_runs = highlight_code_runs(&content, language, base, theme);
        // Split the flat run list at line boundaries: walk runs with a byte
        // cursor, taking `len(line)` bytes per line and skipping the `\n`
        // separators (which belong to no line).
        let mut per_line: Vec<Vec<TextRun>> = Vec::with_capacity(lines.len());
        let mut run_ix = 0usize;
        let mut run_left = block_runs.first().map_or(0, |r| r.len);
        for line in lines {
            let mut out: Vec<TextRun> = Vec::new();
            let mut rest = line.len();
            while rest > 0 {
                if run_left == 0 {
                    run_ix += 1;
                    run_left = block_runs.get(run_ix).map_or(0, |r| r.len);
                    if run_left == 0 {
                        // Defensive: runs stopped short (shouldn't happen —
                        // `highlight_code_runs` covers the full content).
                        out.push(base.clone().to_run(rest));
                        rest = 0;
                        break;
                    }
                }
                let take = rest.min(run_left);
                let mut piece = block_runs[run_ix].clone();
                piece.len = take;
                out.push(piece);
                run_left -= take;
                rest -= take;
            }
            per_line.push(out);
            // Skip the newline separator byte.
            let mut skip = 1usize;
            while skip > 0 {
                if run_left == 0 {
                    run_ix += 1;
                    run_left = block_runs.get(run_ix).map_or(0, |r| r.len);
                    if run_left == 0 {
                        break;
                    }
                }
                let take = skip.min(run_left);
                run_left -= take;
                skip -= take;
            }
        }
        self.inner
            .borrow_mut()
            .insert(key, CacheValue::PerLine(per_line.clone()));
        per_line
    }

    pub fn clear(&self) {
        self.inner.borrow_mut().clear();
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ActiveTheme;

    fn base() -> gpui::TextStyle {
        gpui::TextStyle::default()
    }

    /// The per-line split must cover every line exactly and keep the flat
    /// coverage of the joined parse (multi-line context threading is what
    /// `runs_for_block` exists for — the joined parse is the whole point).
    #[gpui::test]
    fn runs_for_block_covers_lines_exactly(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let theme = cx.update(|cx| cx.theme().clone());
        let lines = vec!["/* header", "still comment */ let x = 1;"];
        let hl = HighlightCache::new();
        let per_line = hl.runs_for_block("rust", &lines, &base(), &theme);
        assert_eq!(per_line.len(), 2);
        let l0: usize = per_line[0].iter().map(|r| r.len).sum();
        let l1: usize = per_line[1].iter().map(|r| r.len).sum();
        assert_eq!(l0, lines[0].len(), "line 0 runs must cover exactly");
        assert_eq!(l1, lines[1].len(), "line 1 runs must cover exactly");
        // The split joins back to the flat coverage of the same content.
        let flat = hl.runs_for("rust", &lines.join("\n"), &base(), &theme);
        let flat_total: usize = flat.iter().map(|r| r.len).sum();
        assert_eq!(flat_total, l0 + l1 + 1 /* \n */);
        // Cache hit returns the same split.
        let again = hl.runs_for_block("rust", &lines, &base(), &theme);
        assert_eq!(again.len(), 2);
        let a1: usize = again[1].iter().map(|r| r.len).sum();
        assert_eq!(a1, l1);
    }

    /// Empty lines get empty run lists; the newline skipping stays aligned.
    #[gpui::test]
    fn runs_for_block_handles_empty_lines(cx: &mut gpui::TestAppContext) {
        cx.update(crate::theme::init);
        let theme = cx.update(|cx| cx.theme().clone());
        let lines = vec!["a", "", "b"];
        let hl = HighlightCache::new();
        let per_line = hl.runs_for_block("rust", &lines, &base(), &theme);
        assert_eq!(per_line.len(), 3);
        assert_eq!(per_line[0].iter().map(|r| r.len).sum::<usize>(), 1);
        assert!(per_line[1].is_empty());
        assert_eq!(per_line[2].iter().map(|r| r.len).sum::<usize>(), 1);
    }
}
