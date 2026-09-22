//! Document planning: line reveal sets + block-level reveal expansion +
//! callout detection, on top of [`crate::editor::markdown_live`] `block`/`layout`.
//!
//! *Line reveal* (from guise `editor.rs:986`): unfocused/read-only → no
//! reveal; selection → every touched line; else the cursor line.
//! *Block reveal* (deepterm extension): `Table`/`FrontMatter` reveal as a
//! whole block when the cursor/selection touches any of their lines, so
//! grids don't flicker between source and preview. Code blocks deliberately
//! do NOT expand: code/fence plans are reveal-invariant, and the renderer
//! only flips a code card to source mode when a *fence* line is revealed —
//! clicking into code keeps the rendered card while editing (Obsidian
//! behavior; guise reveals only the cursor line too).
//! *Callouts* (`> [!NOTE]` …): detected on the source line for card chrome.

use std::collections::BTreeSet;

use crate::editor::markdown_live::block::{classify_with_next, Block, DocState};
use crate::editor::markdown_live::inline::collect_refdefs;
use crate::editor::markdown_live::layout::{plan_with_refs, RowPlan, RunAttrs, Seg};
use crate::editor::markdown_live::model::Pos;

/// Five callout kinds (matches the existing read-only preview).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl CalloutKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Note => "NOTE",
            Self::Tip => "TIP",
            Self::Important => "IMPORTANT",
            Self::Warning => "WARNING",
            Self::Caution => "CAUTION",
        }
    }

    fn parse(tag: &str) -> Option<Self> {
        match tag {
            "[!NOTE]" => Some(Self::Note),
            "[!TIP]" => Some(Self::Tip),
            "[!IMPORTANT]" => Some(Self::Important),
            "[!WARNING]" => Some(Self::Warning),
            "[!CAUTION]" => Some(Self::Caution),
            _ => None,
        }
    }
}

/// A quote line that opens or continues a callout card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callout {
    pub kind: CalloutKind,
    /// Text after the `[!TAG]` on the opening line (may be empty).
    pub rest: String,
    /// True for the `> [!TAG]...` opening line.
    pub is_first: bool,
}

/// One fully planned source line.
#[derive(Debug, Clone)]
pub struct PlannedRow {
    pub line: usize,
    pub block: Block,
    pub plan: RowPlan,
    pub callout: Option<Callout>,
}

/// Line-reveal input. `focused=false` (or read-only) forces full preview.
#[derive(Debug, Clone, Copy)]
pub struct Reveal {
    pub cursor: Pos,
    pub selection: Option<(Pos, Pos)>,
    pub focused: bool,
    pub read_only: bool,
    /// Whole-document source mode (`Cmd+/`): every line shows its raw
    /// source, including tables (which otherwise stay rendered grids so a
    /// cell can be edited in place).
    pub show_source: bool,
}

impl Default for Reveal {
    fn default() -> Self {
        Self {
            cursor: Pos::default(),
            selection: None,
            focused: false,
            read_only: false,
            show_source: false,
        }
    }
}

impl Reveal {
    fn line_set(&self, line_count: usize) -> BTreeSet<usize> {
        if !self.focused || self.read_only || line_count == 0 {
            return BTreeSet::new();
        }
        let mut set = BTreeSet::new();
        match self.selection {
            Some((s, e)) => {
                for l in s.line.min(e.line)..=s.line.max(e.line) {
                    if l < line_count {
                        set.insert(l);
                    }
                }
            }
            None => {
                if self.cursor.line < line_count {
                    set.insert(self.cursor.line);
                }
            }
        }
        set
    }
}

/// Detect `> [!TAG]` on a quote source line. `content` is the block's content
/// byte offset; the tag must be the first non-space token there.
fn detect_callout_open(line: &str, content: usize) -> Option<(CalloutKind, String)> {
    let after = line.get(content..)?.trim_start();
    if !after.starts_with("[!") {
        return None;
    }
    let end = after.find(']')?;
    let kind = CalloutKind::parse(&after[..end + 1])?;
    let rest = after[end + 1..].trim_start().to_string();
    Some((kind, rest))
}

/// The text-only half of planning: document-order block classification
/// (fence/frontmatter state threading) plus the setext backward restyle.
/// Depends only on `lines`, **not** on the cursor/reveal — so the view can
/// cache it and skip these two O(n) passes on cursor-only moves (P1).
pub struct Classified {
    pub blocks: Vec<Block>,
    pub lang_before: Vec<Option<String>>,
}

/// Classify every line in document order (P1 text-only half).
pub fn classify_document(lines: &[String]) -> Classified {
    let mut state = DocState::default();
    let mut lang_before: Vec<Option<String>> = Vec::with_capacity(lines.len());
    let mut blocks: Vec<Block> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        // Lookahead: GFM tables may drop the outer pipes, which is only
        // decidable together with the delimiter row below.
        let next = lines.get(i + 1).map(String::as_str);
        lang_before.push(state.fence_lang().map(str::to_string));
        blocks.push(classify_with_next(line, next, &mut state));
    }

    // 1.5 Setext runs: the lookahead only styles the paragraph line that
    // directly precedes the underline; a multi-line run ("a\nb\n===") is
    // one heading in the preview, so restyle the whole paragraph run
    // backward. (Each source line still renders as its own heading row —
    // a line-based live editor cannot join them; the styling matches.)
    for i in (0..blocks.len()).rev() {
        if let Block::SetextUnderline { level, .. } = blocks[i] {
            let mut j = i;
            while j > 0
                && (matches!(blocks[j - 1], Block::Paragraph)
                    // Setext-created headings (content: 0) are earlier lines
                    // of the same run; `#` headings (content > 0) end it.
                    || matches!(blocks[j - 1], Block::Heading { content: 0, .. }))
            {
                j -= 1;
                blocks[j] = Block::Heading {
                    level,
                    content: 0,
                };
            }
        }
    }

    Classified { blocks, lang_before }
}

/// Plan every row from a precomputed classification + a (cursor-dependent)
/// reveal. The heavy reveal-set / refdefs / per-line passes still run here,
/// but the two O(n) classification passes are skipped when `classify_document`
/// was cached (P1).
pub fn plan_rows(lines: &[String], classified: &Classified, reveal: Reveal) -> Vec<PlannedRow> {
    plan_rows_with_refs(
        lines,
        classified,
        &refdefs_for_lines(lines, &classified.blocks),
        reveal,
    )
}

/// Whether a block's lines are exempt from registering reference definitions
/// (`[label]: dest`); their text is literal content.
pub(crate) fn is_refdef_skipped(b: &Block) -> bool {
    matches!(
        b,
        Block::CodeLine
            | Block::Fence { .. }
            | Block::FrontMatter
            | Block::Table
            | Block::Math { .. }
            | Block::MathDelim { .. }
    )
}

/// Reference definitions (`[label]: dest`) for document-wide link resolution.
/// A pure function of (`lines`, `blocks`) — both text-only — so the view caches
/// it and skips this O(n) scan on cursor-only moves (P1).
pub fn refdefs_for_lines(
    lines: &[String],
    blocks: &[Block],
) -> std::collections::HashMap<String, String> {
    collect_refdefs(lines, &|i| blocks.get(i).is_some_and(is_refdef_skipped))
}

/// Plan every row from a precomputed classification + reveal, using the given
/// (possibly cached) reference definitions. This is the variant the view uses
/// so it can reuse `refdefs_for_lines` across cursor-only moves.
pub fn plan_rows_with_refs(
    lines: &[String],
    classified: &Classified,
    refdefs: &std::collections::HashMap<String, String>,
    reveal: Reveal,
) -> Vec<PlannedRow> {
    if lines.is_empty() {
        return Vec::new();
    }
    let blocks = &classified.blocks;

    // 2. Line reveal set, then block expansion over source-only runs.
    let mut revealed = reveal.line_set(lines.len());
    // Table rows never take part in line-level reveal: the grid stays
    // rendered and the *cell* under the cursor reveals its own source (see
    // `Group::Table`), so clicking a cell edits it in place instead of
    // flipping the whole block back to source lines.
    //
    // Exception: whole-document source mode (`Cmd+/`) must still show the
    // raw table — it is the escape hatch for bulk-editing alignment and
    // formatting.
    if !reveal.show_source {
        revealed.retain(|l| !matches!(blocks.get(*l), Some(Block::Table)));
    }
    // Expand over contiguous FrontMatter/math runs (code/fence rows are
    // reveal-invariant and keep the rendered card while editing).
    //
    // Tables are deliberately NOT expanded: a table stays a rendered grid
    // and the *cell* under the cursor reveals its own source (see
    // `Group::Table`), so clicking a cell edits it in place instead of
    // flipping the whole block back to source lines. The whole-table
    // source view is still available through `show_source` (`Cmd+/`),
    // which reveals every line via the selection in `plan_rows`.
    let mut run_start: Option<usize> = None;
    let flush = |start: Option<usize>, end: usize, revealed: &mut BTreeSet<usize>| {
        if let Some(s) = start {
            if (s..end).any(|l| revealed.contains(&l)) {
                revealed.extend(s..end);
            }
        }
    };
    for (i, b) in blocks.iter().enumerate() {
        let in_run = matches!(
            b,
            Block::FrontMatter | Block::Math { .. } | Block::MathDelim { .. }
        );
        if in_run && run_start.is_none() {
            run_start = Some(i);
        } else if !in_run && run_start.is_some() {
            flush(run_start.take(), i, &mut revealed);
        }
    }
    flush(run_start.take(), lines.len(), &mut revealed);

    // 3. Plan each line + track callout cards across quote runs.
    let mut rows = Vec::with_capacity(lines.len());
    let mut callout_kind: Option<CalloutKind> = None;
    let mut prev_was_quote = false;
    for (i, (line, block)) in lines.iter().zip(blocks.iter()).enumerate() {
        let is_quote = matches!(block, Block::Quote { .. });
        if !is_quote {
            callout_kind = None;
        }
        let callout = match block {
            Block::Quote { content, .. } => {
                if !prev_was_quote || callout_kind.is_none() {
                    // Possible card opener.
                    match detect_callout_open(line, *content) {
                        Some((kind, rest)) => {
                            callout_kind = Some(kind);
                            Some(Callout {
                                kind,
                                rest,
                                is_first: true,
                            })
                        }
                        None => {
                            callout_kind = None;
                            None
                        }
                    }
                } else {
                    // Continuation line inside an open card.
                    callout_kind.map(|kind| Callout {
                        kind,
                        rest: String::new(),
                        is_first: false,
                    })
                }
            }
            _ => None,
        };
        prev_was_quote = is_quote;

        let lang = match block {
            Block::CodeLine => classified.lang_before[i].as_deref(),
            _ => None,
        };
        let row_plan = plan_with_refs(line, block, lang, revealed.contains(&i), &refdefs);
        rows.push(PlannedRow {
            line: i,
            block: block.clone(),
            plan: row_plan,
            callout,
        });
    }
    rows
}

/// Plan the whole document: classify then plan rows. Pure function of
/// (lines, reveal). Backwards-compatible wrapper: call sites that don't need
/// the P1 classification cache keep using this. The view routes cursor-only
/// moves through `plan_rows` with a cached `classify_document` result.
pub fn plan_document(lines: &[String], reveal: Reveal) -> Vec<PlannedRow> {
    plan_rows(lines, &classify_document(lines), reveal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::markdown_live::layout::RowKind;

    fn reveal_at(line: usize) -> Reveal {
        Reveal {
            cursor: Pos::new(line, 0),
            selection: None,
            focused: true,
            read_only: false,
            show_source: false,
        }
    }

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    #[test]
    fn cursor_line_revealed_others_preview() {
        let ls = lines("# Hi\n\nbody **b**");
        let rows = plan_document(&ls, reveal_at(2));
        assert!(!rows[0].plan.revealed);
        assert_eq!(rows[0].plan.visible, "Hi");
        assert!(rows[2].plan.revealed);
        assert_eq!(rows[2].plan.visible, "body **b**");
    }

    #[test]
    fn unfocused_or_readonly_forces_preview() {
        let ls = lines("# Hi");
        let r = Reveal {
            focused: false,
            ..reveal_at(0)
        };
        assert!(!plan_document(&ls, r)[0].plan.revealed);
        let r = Reveal {
            read_only: true,
            ..reveal_at(0)
        };
        assert!(!plan_document(&ls, r)[0].plan.revealed);
    }

    #[test]
    fn selection_reveals_touched_lines() {
        let ls = lines("# A\n# B\n# C");
        let r = Reveal {
            cursor: Pos::new(2, 0),
            selection: Some((Pos::new(0, 0), Pos::new(1, 0))),
            focused: true,
            read_only: false,
            ..Default::default()
        };
        let rows = plan_document(&ls, r);
        assert!(rows[0].plan.revealed && rows[1].plan.revealed);
        assert!(!rows[2].plan.revealed);
    }

    #[test]
    fn setext_headings_match_preview() {
        // `Title` + `===` is an h1; `Sub` + `---` an h2.
        let ls = lines("Title\n===\n\nSub\n---\n");
        let rows = plan_document(&ls, reveal_at(5));
        assert!(matches!(
            rows[0].block,
            Block::Heading { level: 1, content: 0 }
        ));
        assert!(matches!(
            rows[1].block,
            Block::SetextUnderline { level: 1 }
        ));
        assert!(matches!(
            rows[3].block,
            Block::Heading { level: 2, content: 0 }
        ));
        assert_eq!(rows[0].plan.kind, RowKind::Heading(1));
        // Preview mode hides the underline (visible is empty) and styles
        // the heading text.
        assert_eq!(rows[1].plan.visible, "");
        assert_eq!(rows[1].plan.kind, RowKind::Setext);
        assert_eq!(rows[0].plan.visible, "Title");
        // Revealed, the underline shows its marker.
        let rows = plan_document(&ls, reveal_at(1));
        assert_eq!(rows[1].plan.visible, "===");
        assert!(rows[1].plan.revealed);
        // Multi-line runs restyle every paragraph line of the run.
        let ls = lines("a\nb\n===");
        let rows = plan_document(&ls, reveal_at(3));
        assert!(rows.iter().take(2).all(|r| matches!(
            r.block,
            Block::Heading { level: 1, .. }
        )));
        // A mid-document `---` without a paragraph above stays a rule.
        let ls = lines("# h\n\n---\n");
        let rows = plan_document(&ls, reveal_at(3));
        assert!(matches!(rows[2].block, Block::Rule));
    }

    #[test]
    fn code_lines_reveal_individually() {
        let ls = lines("```rust\nlet x = 1;\n```\n\nafter");
        // Cursor on a code line: only that line is revealed — the fence
        // lines stay hidden so the rendered card remains while editing
        // (the card flips to source mode only via a revealed fence line).
        let rows = plan_document(&ls, reveal_at(1));
        assert!(!rows[0].plan.revealed);
        assert!(rows[1].plan.revealed);
        assert!(!rows[2].plan.revealed);
        assert!(!rows[4].plan.revealed);
        // Cursor on the opening fence line: just that line flips the card.
        let rows = plan_document(&ls, reveal_at(0));
        assert!(rows[0].plan.revealed);
        assert!(!rows[1].plan.revealed);
    }

    /// Tables are **not** revealed as a unit any more: the grid stays
    /// rendered and the active cell reveals its own source, so clicking a
    /// cell edits it in place (the whole-block source view is `Cmd+/`).
    #[test]
    fn table_does_not_reveal_as_unit() {
        let ls = lines("| a | b |\n|---|---|\n| 1 | 2 |\n\ntext");
        let rows = plan_document(&ls, reveal_at(2));
        assert!(
            !rows[0].plan.revealed && !rows[1].plan.revealed && !rows[2].plan.revealed,
            "表格必须保持网格渲染（活动格由 view 单独揭示）"
        );
        assert!(!rows[4].plan.revealed);
    }

    #[test]
    fn callout_card_detected() {
        let ls = lines("> [!NOTE]\n> hello\n\nplain");
        let rows = plan_document(&ls, reveal_at(3));
        let c0 = rows[0].callout.as_ref().expect("opener");
        assert_eq!(c0.kind, CalloutKind::Note);
        assert!(c0.is_first);
        let c1 = rows[1].callout.as_ref().expect("continuation");
        assert_eq!(c1.kind, CalloutKind::Note);
        assert!(!c1.is_first);
        assert!(rows[3].callout.is_none());
        assert!(matches!(rows[0].block, Block::Quote { .. }));
    }

    #[test]
    fn callout_with_same_line_rest() {
        let ls = lines("> [!TIP] same line\n> more");
        let rows = plan_document(&ls, reveal_at(1));
        assert_eq!(rows[0].callout.as_ref().unwrap().rest, "same line");
    }

    #[test]
    fn fence_lang_threads_to_code_rows() {
        let ls = lines("```json\n{}\n```");
        let rows = plan_document(&ls, reveal_at(2));
        assert_eq!(
            rows[1].plan.kind,
            RowKind::Code {
                lang: Some("json".into())
            }
        );
    }
}

/// Strip a byte prefix from a planned row (callout `[!TAG] ` on non-revealed
/// openers, so the card matches the preview which consumes the tag).
///
/// `n_vis` visible bytes and `n_src` source bytes are dropped from the front;
/// runs, segments and link ranges shift accordingly so caret/selection
/// mapping keeps working on the trimmed row.
pub fn strip_plan_prefix(plan: &RowPlan, n_vis: usize, n_src: usize) -> RowPlan {
    let mut out = plan.clone();
    out.visible = out.visible.get(n_vis..).unwrap_or("").to_string();
    // Runs: trim n_vis bytes from the front.
    let mut runs = Vec::new();
    let mut drop = n_vis;
    for r in &plan.runs {
        if drop >= r.len {
            drop -= r.len;
            continue;
        }
        let mut nr: RunAttrs = *r;
        nr.len -= drop;
        drop = 0;
        runs.push(nr);
    }
    out.runs = runs;
    // Segments: drop segs ending at/before n_src, trim straddlers, and shift
    // every kept visible offset down by n_vis.
    let mut segs = Vec::new();
    for s in &plan.segs {
        if s.src.end <= n_src {
            continue;
        }
        let cut_src = n_src.saturating_sub(s.src.start);
        let vis_len = s.vis.end - s.vis.start;
        let cut_vis = cut_src.min(vis_len);
        let src_start = s.src.start + cut_src;
        if src_start >= s.src.end {
            continue;
        }
        segs.push(Seg {
            src: src_start..s.src.end,
            vis: (s.vis.start + cut_vis).saturating_sub(n_vis)..s.vis.end.saturating_sub(n_vis),
        });
    }
    out.segs = segs;
    out.links = plan
        .links
        .iter()
        .filter_map(|(r, t)| {
            if r.end <= n_src {
                None
            } else {
                Some((r.start.max(n_src) - n_src..r.end - n_src, t.clone()))
            }
        })
        .collect();
    out
}

#[cfg(test)]
mod strip_tests {
    use super::super::block::{classify, DocState};
    use super::*;

    fn plan_quote(line: &str, reveal: bool) -> RowPlan {
        let mut st = DocState::default();
        classify("x", &mut st);
        let block = classify(line, &mut st);
        crate::editor::markdown_live::layout::plan(line, &block, None, reveal)
    }

    #[test]
    fn strip_callout_tag_keeps_mapping() {
        // `> ` (2 bytes) hidden in preview; content `[!NOTE] hi` visible.
        let plan = plan_quote("> [!NOTE] hi", false);
        assert_eq!(plan.visible, "[!NOTE] hi");
        // Strip `[!NOTE] ` (8 visible bytes, 2 + 8 source bytes).
        let stripped = strip_plan_prefix(&plan, 8, 10);
        assert_eq!(stripped.visible, "hi");
        // Visible start maps back to source `hi`.
        let src = crate::editor::markdown_live::layout::src_for_vis(&stripped.segs, 0);
        assert_eq!(src, 10);
        // Runs still cover visible exactly.
        let total: usize = stripped.runs.iter().map(|r| r.len).sum();
        assert_eq!(total, 2);
    }
}

#[cfg(test)]
mod showcase_tests {
    use super::*;

    // The repo's manual QA document must plan without panics and keep every
    // row's segments/runs covering their strings exactly.
    const SHOWCASE: &str = include_str!("../../../markdown-showcase.md");

    fn check_row(line: &str, plan: &RowPlan) {
        let mut src_at = 0;
        let mut vis_at = 0;
        for seg in &plan.segs {
            assert_eq!(seg.src.start, src_at);
            assert_eq!(seg.vis.start, vis_at);
            src_at = seg.src.end;
            vis_at = seg.vis.end;
        }
        assert_eq!(src_at, line.len(), "segs must cover source: {line:?}");
        assert_eq!(vis_at, plan.visible.len(), "segs must cover visible");
        let total: usize = plan.runs.iter().map(|r| r.len).sum();
        assert_eq!(total, plan.visible.len(), "runs must cover visible");
    }

    #[test]
    fn showcase_plans_cleanly_for_every_cursor_line() {
        let lines: Vec<String> = SHOWCASE.split('\n').map(str::to_string).collect();
        // Full preview (unfocused) + a few revealed variants.
        let configs = [
            Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: false,
                read_only: false,
                show_source: false,
            },
            Reveal {
                cursor: Pos::new(lines.len() / 2, 0),
                selection: None,
                focused: true,
                read_only: false,
            ..Default::default()
        },
            Reveal {
                cursor: Pos::new(0, 0),
                selection: Some((Pos::new(0, 0), Pos::new(lines.len().saturating_sub(1), 0))),
                focused: true,
                read_only: false,
            ..Default::default()
        },
        ];
        for reveal in configs {
            let rows = plan_document(&lines, reveal);
            assert_eq!(rows.len(), lines.len());
            for (row, line) in rows.iter().zip(lines.iter()) {
                check_row(line, &row.plan);
            }
        }
    }

    /// P1: the aggregate `plan_document` must equal classification-then-plan
    /// (`plan_rows(classify_document(...))`), which is how the view reuses the
    /// classification across cursor-only moves. If they ever diverge, the
    /// classification cache feeds back different rows than a fresh full plan.
    #[test]
    fn plan_document_equals_classify_then_plan_rows() {
        let doc = "# Heading\n\npara **bold**\n\n```rust\nfn f() {}\n```\n\n- item\n\n| a | b |\n|---|---|\n| 1 | 2 |";
        let ls: Vec<String> = doc.split('\n').map(str::to_string).collect();
        let classified = classify_document(&ls);
        let refs = refdefs_for_lines(&ls, &classified.blocks);
        let n = ls.len();
        for line in 0..n {
            let reveal = Reveal {
                cursor: Pos::new(line, 0),
                selection: None,
                focused: true,
                read_only: false,
                show_source: false,
            };
            let merged = plan_document(&ls, reveal);
            let split = plan_rows(&ls, &classified, reveal);
            // P1 cached path: reuse the precomputed classification + refs.
            let split_cached =
                plan_rows_with_refs(&ls, &classified, &refs, reveal);
            assert_eq!(merged.len(), split.len());
            assert_eq!(merged.len(), split_cached.len());
            for (a, b) in merged.iter().zip(split.iter()) {
                assert_eq!(a.line, b.line);
                assert_eq!(a.block, b.block);
                assert_eq!(a.plan.visible, b.plan.visible);
                assert_eq!(a.plan.revealed, b.plan.revealed);
            }
            // Cached refs must produce identical rows to the fresh full plan.
            for (a, b) in merged.iter().zip(split_cached.iter()) {
                assert_eq!(a.line, b.line);
                assert_eq!(a.block, b.block);
                assert_eq!(a.plan.visible, b.plan.visible);
                assert_eq!(a.plan.revealed, b.plan.revealed);
            }
        }
    }
}
