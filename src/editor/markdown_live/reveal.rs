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
use std::sync::Arc;

use crate::editor::markdown_live::block::{Block, DocState, classify_with_next};
use crate::editor::markdown_live::inline::collect_refdefs;
use crate::editor::markdown_live::layout::{RowPlan, RunAttrs, Seg, plan_with_refs};
use crate::editor::markdown_live::model::{Line, Pos};

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
#[derive(Debug, Clone, Copy, Default)]
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

impl Reveal {
    pub(crate) fn line_set(&self, line_count: usize) -> BTreeSet<usize> {
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

/// The full line-reveal set for a document: cursor/selection lines plus the
/// block-level expansion over source-only runs (`FrontMatter` / math).
///
/// Extracted from [`plan_rows_with_refs`] so the incremental planner can
/// compute the set on its own and diff it against the previous frame's — a
/// cursor move only changes a handful of lines, and those are exactly the
/// rows whose plan needs recomputing.
pub fn reveal_set(blocks: &[Block], line_count: usize, reveal: Reveal) -> BTreeSet<usize> {
    let mut revealed = reveal.line_set(line_count);
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
        if let Some(s) = start
            && (s..end).any(|l| revealed.contains(&l))
        {
            revealed.extend(s..end);
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
    flush(run_start.take(), blocks.len(), &mut revealed);
    revealed
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
    pub lang_before: Vec<Option<Arc<str>>>,
    /// Lines this pass could have classified differently from the previous
    /// one, as a half-open range.
    ///
    /// Callers that keep their own snapshot of `blocks` (the view caches the
    /// previous plan's kinds to decide whether cached geometry is still valid)
    /// must only compare inside this span. Comparing the whole table is
    /// correct but costs a full pass per keystroke, which is exactly what the
    /// incremental classifier exists to avoid.
    ///
    /// A full pass reports `0..lines.len()`.
    pub changed: std::ops::Range<usize>,
    /// Classifier state entering every `CLASSIFY_CHECKPOINT`-th line
    /// (`states[k]` = state before line `k * CLASSIFY_CHECKPOINT`).
    ///
    /// Storing every line's state would cost ~80 B/line; checkpointing keeps
    /// the memory at `lines / 64 × 80 B` (25 KB on a 20k-line document) while
    /// still allowing an exact resume: replay at most 63 lines from the
    /// nearest checkpoint to reach any line.
    pub states: Vec<DocState>,
}

/// Lines between classifier-state checkpoints. See [`Classified::states`].
pub const CLASSIFY_CHECKPOINT: usize = 64;

/// Incremental document classification.
///
/// Classification is a forward state machine (`DocState`), so an edit only
/// affects lines from the edit point until the running state **converges**
/// with the previously saved state — after that every later line classifies
/// identically and the old tail is reused verbatim.
///
/// The convergence test is exact: the resumed run's state is compared with
/// the checkpointed state saved alongside the previous classification, so
/// there is no heuristic that a new `DocState` field could silently break.
///
/// `dirty` is the range of lines whose text may have changed (exclusive end,
/// matching [`EditorModel::dirty_range`](crate::editor::markdown_live::model::EditorModel::dirty_range));
/// `prev` is the previous classification (empty on the first call, forcing a
/// full pass). Returns the classification and how many lines were actually
/// re-classified (tests assert this stays small for a single-line edit).
pub fn classify_incremental(
    lines: &[Line],
    prev: Option<&Classified>,
    dirty: std::ops::Range<usize>,
) -> (Classified, usize) {
    let n = lines.len();
    let prev = prev.filter(|p| p.blocks.len() == n && p.lang_before.len() == n);
    let dirty_from = dirty.start;
    // The reused tail must begin strictly after the last possibly-changed line.
    // A single scalar `dirty_from` is not enough: one edit can touch several
    // separated lines (replace-all, `replace_ranges_each`, a coalesced undo),
    // and the merged range's *end* is what bounds the tail.
    let tail_floor = dirty.end;
    let can_resume = prev.is_some() && dirty_from > 0 && dirty_from < n;

    let mut blocks: Vec<Block> = Vec::with_capacity(n);
    let mut lang_before: Vec<Option<Arc<str>>> = Vec::with_capacity(n);
    let mut states: Vec<DocState> = Vec::with_capacity(n / CLASSIFY_CHECKPOINT + 2);
    let mut state = DocState::default();
    let mut start = 0usize;
    let mut reprocessed = 0usize;

    if let (true, Some(prev)) = (can_resume, prev) {
        // Resume from the checkpoint strictly *before* `dirty_from`.
        //
        // The copied prefix gets no lookahead, but line `from - 1` was
        // originally classified with `next = lines[from]` — the line that just
        // changed. Resuming *at* `dirty_from` (the old `dirty_from / CHECKPOINT`
        // rounding) therefore froze a stale classification for the line before
        // the edit whenever the edit landed on a checkpoint multiple: typing a
        // setext underline on such a line left the heading above it rendered as
        // a plain paragraph, permanently. One checkpoint of margin costs at
        // most `CLASSIFY_CHECKPOINT - 1` extra lines and removes the whole
        // boundary class.
        let mut ck =
            ((dirty_from - 1) / CLASSIFY_CHECKPOINT).min(prev.states.len().saturating_sub(1));
        // The setext restyle at the end of this function rewrites *earlier*
        // lines of the paragraph run ending at an underline, and those
        // rewritten rows are what `prev.blocks` stores. When the resume point
        // sits inside such a run, the underline justifying the artifacts may be
        // exactly what the edit removed, so the prefix must not be copied:
        // walk back to the run's start. `Heading { content: 0 }` is precisely
        // the restyle marker (a `#` heading carries `content > 0`, and an
        // untouched paragraph stays `Paragraph`), so this stops at the first
        // line the restyle cannot have reached.
        while ck > 0 {
            let boundary = ck * CLASSIFY_CHECKPOINT;
            if !matches!(
                prev.blocks.get(boundary - 1),
                Some(Block::Heading { content: 0, .. })
            ) {
                break;
            }
            ck -= 1;
        }
        let from = ck * CLASSIFY_CHECKPOINT;
        state = prev.states[ck].clone();
        // Copy the saved prefix verbatim (no re-classification, no lookahead).
        blocks.extend_from_slice(&prev.blocks[..from]);
        lang_before.extend_from_slice(&prev.lang_before[..from]);
        states.extend(prev.states[..ck].iter().cloned());
        // Re-derive the state for the checkpoint line itself, so `states`
        // stays aligned with `blocks`.
        if from.is_multiple_of(CLASSIFY_CHECKPOINT) {
            states.push(state.clone());
        }
        start = from;
    } else {
        states.push(DocState::default());
    }

    // Classify forward from `start`, stopping as soon as the state matches the
    // saved state at the same line (the tail is then provably identical).
    let mut converged_at: Option<usize> = None;
    let mut i = start;
    while i < n {
        if i > start && i.is_multiple_of(CLASSIFY_CHECKPOINT) {
            states.push(state.clone());
        }
        let next = lines.get(i + 1).map(|l| l.as_str());
        let lang_here = state.fence_lang().map(Arc::<str>::from);
        let block = classify_with_next(&lines[i], next, &mut state);
        blocks.push(block);
        lang_before.push(lang_here);
        reprocessed += 1;

        // Exact convergence: the running state equals the state the previous
        // classification entered the next line with, and this line's own
        // results already match. Everything from `i + 1` on is identical —
        // provided the reused tail starts after every line the edit touched
        // (`tail_floor`); otherwise a later edited line would be copied stale.
        if let Some(prev) = prev
            && i + 1 < n
            && i + 1 >= tail_floor
            && prev.blocks.get(i) == Some(&blocks[i])
            && prev.lang_before.get(i) == lang_before.get(i)
            && prev_state_at(prev, i + 1).is_some_and(|s| s.same_parse_state(&state))
        {
            converged_at = Some(i + 1);
            break;
        }
        i += 1;
    }

    if let (Some(at), Some(prev)) = (converged_at, prev)
        && at < n
    {
        blocks.extend_from_slice(&prev.blocks[at..]);
        lang_before.extend_from_slice(&prev.lang_before[at..]);
        // Checkpoints for the reused tail come straight from the old table.
        let first_ck = at.div_ceil(CLASSIFY_CHECKPOINT);
        let last_ck = (n - 1) / CLASSIFY_CHECKPOINT;
        for k in first_ck..=last_ck {
            if let Some(s) = prev.states.get(k) {
                states.truncate(k);
                states.push(s.clone());
            }
        }
    }

    // Setext runs: the lookahead only styles the paragraph line that directly
    // precedes the underline; a multi-line run ("a\nb\n===") is one heading in
    // the preview, so restyle the whole paragraph run backward. Only rows in
    // the re-classified window can change, but the run can start before it —
    // so this pass runs over the whole table (cheap, allocation-free).
    //
    // It can also reach *below* `start`: an underline in the reused tail whose
    // run extends back into freshly-classified lines rewrites those earlier
    // rows. `changed_lo` tracks the furthest such reach so the reported span
    // stays a true superset of everything this pass touched.
    let mut changed_lo = start;
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
                blocks[j] = Block::Heading { level, content: 0 };
            }
            changed_lo = changed_lo.min(j);
        }
    }
    // Everything from the resume point up to the convergence point was
    // classified fresh; the restyle may have reached further back.
    let changed = changed_lo..converged_at.unwrap_or(n);

    // Pad the checkpoint table to cover the whole document.
    while states.len() <= (n.saturating_sub(1)) / CLASSIFY_CHECKPOINT {
        // Should be unreachable for the full-pass path (states are pushed in
        // the loop); the guard keeps the table well-formed if `n` grew.
        let k = states.len();
        let from = k * CLASSIFY_CHECKPOINT;
        let mut st = states[k - 1].clone();
        for j in from.saturating_sub(CLASSIFY_CHECKPOINT)..from.min(n) {
            let next = lines.get(j + 1).map(|l| l.as_str());
            classify_with_next(&lines[j], next, &mut st);
        }
        states.push(st);
    }

    (
        Classified {
            blocks,
            lang_before,
            changed,
            states,
        },
        reprocessed,
    )
}

/// The checkpointed classifier state entering line `i`, if recorded.
fn prev_state_at(prev: &Classified, i: usize) -> Option<&DocState> {
    if !i.is_multiple_of(CLASSIFY_CHECKPOINT) {
        return None;
    }
    prev.states.get(i / CLASSIFY_CHECKPOINT)
}

/// Classify every line in document order (full pass; the incremental variant
/// is [`classify_incremental`]). Convenience wrapper for tests and the
/// differential oracle.
pub fn classify_document(lines: &[Line]) -> Classified {
    classify_incremental(lines, None, 0..0).0
}

/// Plan every row from a precomputed classification + reveal. Convenience
/// wrapper that collects refdefs on the fly; the view uses
/// [`plan_rows_with_refs`] with its cached table instead.
pub fn plan_rows(lines: &[Line], classified: &Classified, reveal: Reveal) -> Vec<PlannedRow> {
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
    lines: &[Line],
    blocks: &[Block],
) -> std::collections::HashMap<String, String> {
    collect_refdefs(lines, &|i| blocks.get(i).is_some_and(is_refdef_skipped))
}

/// In-place incremental classification for the common "same line count" edit.
///
/// The allocating [`classify_incremental`] builds fresh `blocks` /
/// `lang_before` vectors and copies the unchanged prefix/tail on every
/// keystroke. When the line count is unchanged the old vectors can be mutated
/// in place: prefix rows are already correct, re-classified rows are
/// overwritten, and the converged tail is left untouched. This removes the
/// O(document) `extend_from_slice` (and its `Arc`/String clones) from the
/// editor's typing path.
///
/// Returns the number of lines actually re-classified.
pub fn classify_incremental_in_place(
    classified: &mut Classified,
    lines: &[Line],
    dirty: std::ops::Range<usize>,
) -> usize {
    let n = lines.len();
    if n == 0 || classified.blocks.len() != n || classified.lang_before.len() != n {
        // Caller should fall back to `classify_incremental`; keep this safe.
        let (fresh, reprocessed) = classify_incremental(lines, Some(classified), dirty);
        *classified = fresh;
        return reprocessed;
    }

    let dirty_from = dirty.start;
    let tail_floor = dirty.end;
    let can_resume = dirty_from > 0 && dirty_from < n;

    let mut state = DocState::default();
    let mut start = 0usize;
    if can_resume {
        let mut ck =
            ((dirty_from - 1) / CLASSIFY_CHECKPOINT).min(classified.states.len().saturating_sub(1));
        while ck > 0 {
            let boundary = ck * CLASSIFY_CHECKPOINT;
            if !matches!(
                classified.blocks.get(boundary - 1),
                Some(Block::Heading { content: 0, .. })
            ) {
                break;
            }
            ck -= 1;
        }
        let from = ck * CLASSIFY_CHECKPOINT;
        state = classified.states[ck].clone();
        start = from;
    }

    let mut reprocessed = 0usize;
    let mut converged_at: Option<usize> = None;
    let mut i = start;
    while i < n {
        if i > start && i.is_multiple_of(CLASSIFY_CHECKPOINT) {
            let ck = i / CLASSIFY_CHECKPOINT;
            if ck < classified.states.len() {
                classified.states[ck] = state.clone();
            } else {
                classified.states.push(state.clone());
            }
        }
        let next = lines.get(i + 1).map(|l| l.as_str());
        let lang_here = state.fence_lang().map(Arc::<str>::from);
        let block = classify_with_next(&lines[i], next, &mut state);
        // Compare against the old values *before* overwriting them.
        let same_block = classified.blocks.get(i) == Some(&block);
        let same_lang = classified.lang_before.get(i) == Some(&lang_here);
        classified.blocks[i] = block;
        classified.lang_before[i] = lang_here;
        reprocessed += 1;

        if i + 1 < n
            && i + 1 >= tail_floor
            && same_block
            && same_lang
            && prev_state_at(classified, i + 1).is_some_and(|s| s.same_parse_state(&state))
        {
            converged_at = Some(i + 1);
            break;
        }
        i += 1;
    }

    // Same setext backward restyle as `classify_incremental`; it only mutates
    // rows, so it is safe in place.
    let mut changed_lo = start;
    for i in (0..classified.blocks.len()).rev() {
        if let Block::SetextUnderline { level, .. } = classified.blocks[i] {
            let mut j = i;
            while j > 0
                && (matches!(classified.blocks[j - 1], Block::Paragraph)
                    || matches!(classified.blocks[j - 1], Block::Heading { content: 0, .. }))
            {
                j -= 1;
                classified.blocks[j] = Block::Heading { level, content: 0 };
            }
            changed_lo = changed_lo.min(j);
        }
    }
    classified.changed = changed_lo..converged_at.unwrap_or(n);

    // Pad the checkpoint table if it is shorter than the document requires.
    while classified.states.len() <= n.saturating_sub(1) / CLASSIFY_CHECKPOINT {
        let k = classified.states.len();
        let from = k * CLASSIFY_CHECKPOINT;
        let mut st = classified.states[k - 1].clone();
        for j in from.saturating_sub(CLASSIFY_CHECKPOINT)..from.min(n) {
            let next = lines.get(j + 1).map(|l| l.as_str());
            classify_with_next(&lines[j], next, &mut st);
        }
        classified.states.push(st);
    }

    reprocessed
}

/// Plan a single row, including callout-card tracking and the callout-opener
/// prefix strip.
///
/// `prev_was_quote`/`callout_kind` carry the callout run state from the
/// preceding row, exactly as the whole-document loop threads it. Returns the
/// planned row plus the state for the next line.
///
/// This is the incremental unit: a keystroke only changes a contiguous run of
/// rows, and everything outside it keeps its previous [`PlannedRow`] verbatim.
///
/// The `[!TAG] ` prefix strip lives here (not in the caller) so the
/// whole-document and incremental planners cannot diverge on it.
#[allow(clippy::too_many_arguments)]
fn plan_one(
    lines: &[Line],
    blocks: &[Block],
    lang_before: &[Option<Arc<str>>],
    refs: &std::collections::HashMap<String, String>,
    revealed: &BTreeSet<usize>,
    i: usize,
    prev_was_quote: bool,
    callout_kind: Option<CalloutKind>,
) -> (PlannedRow, bool, Option<CalloutKind>) {
    let line = &lines[i];
    let block = &blocks[i];
    let is_quote = matches!(block, Block::Quote { .. });
    let mut next_kind = callout_kind;
    if !is_quote {
        next_kind = None;
    }
    let callout = match block {
        Block::Quote { content, .. } => {
            if !prev_was_quote || callout_kind.is_none() {
                // Possible card opener.
                match detect_callout_open(line, *content) {
                    Some((kind, rest)) => {
                        next_kind = Some(kind);
                        Some(Callout {
                            kind,
                            rest,
                            is_first: true,
                        })
                    }
                    None => {
                        next_kind = None;
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

    let lang = match block {
        Block::CodeLine => lang_before.get(i).and_then(|l| l.as_deref()),
        _ => None,
    };
    let row_plan = plan_with_refs(line, block, lang, revealed.contains(&i), refs);
    (
        PlannedRow {
            line: i,
            block: block.clone(),
            plan: row_plan,
            callout,
        },
        is_quote,
        next_kind,
    )
}

/// Plan a contiguous row range `start..end` given the reveal set and the
/// callout state entering the range.
///
/// Used by [`plan_rows_with_refs`] for the whole document and by the view's
/// incremental planner for the dirty range.
#[allow(clippy::too_many_arguments)]
pub fn plan_range(
    lines: &[Line],
    classified: &Classified,
    refs: &std::collections::HashMap<String, String>,
    revealed: &BTreeSet<usize>,
    start: usize,
    end: usize,
    mut prev_was_quote: bool,
    mut callout_kind: Option<CalloutKind>,
) -> Vec<PlannedRow> {
    let blocks = &classified.blocks;
    let mut rows = Vec::with_capacity(end.saturating_sub(start));
    for i in start..end.min(lines.len()) {
        let (row, was_quote, kind) = plan_one(
            lines,
            blocks,
            &classified.lang_before,
            refs,
            revealed,
            i,
            prev_was_quote,
            callout_kind,
        );
        prev_was_quote = was_quote;
        callout_kind = kind;
        rows.push(row);
    }
    rows
}

/// Plan every row from a precomputed classification + reveal, using the given
/// (possibly cached) reference definitions. This is the variant the view uses
/// so it can reuse `refdefs_for_lines` across cursor-only moves.
pub fn plan_rows_with_refs(
    lines: &[Line],
    classified: &Classified,
    refdefs: &std::collections::HashMap<String, String>,
    reveal: Reveal,
) -> Vec<PlannedRow> {
    if lines.is_empty() {
        return Vec::new();
    }
    let blocks = &classified.blocks;

    // 2. Line reveal set + block-level expansion over source-only runs.
    let revealed = reveal_set(blocks, lines.len(), reveal);

    // 3. Plan each line + track callout cards across quote runs.
    plan_range(
        lines,
        classified,
        refdefs,
        &revealed,
        0,
        lines.len(),
        false,
        None,
    )
}

/// Plan the whole document: classify then plan rows. Pure function of
/// (lines, reveal). Backwards-compatible wrapper: call sites that don't need
/// the P1 classification cache keep using this. The view routes cursor-only
/// moves through `plan_rows` with a cached `classify_document` result.
pub fn plan_document(lines: &[Line], reveal: Reveal) -> Vec<PlannedRow> {
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

    fn lines(s: &str) -> Vec<Line> {
        crate::editor::markdown_live::model::lines_from_text(s)
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
            Block::Heading {
                level: 1,
                content: 0
            }
        ));
        assert!(matches!(rows[1].block, Block::SetextUnderline { level: 1 }));
        assert!(matches!(
            rows[3].block,
            Block::Heading {
                level: 2,
                content: 0
            }
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
        assert!(
            rows.iter()
                .take(2)
                .all(|r| matches!(r.block, Block::Heading { level: 1, .. }))
        );
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
/// Hide a callout opener's `[!TAG] ` prefix (preview parity: the preview
/// consumes the tag). Applied by the view after planning so that the planner
/// itself keeps emitting rows whose segments cover the source line exactly
/// (`showcase_plans_cleanly_for_every_cursor_line` pins that invariant).
///
/// `line` is the row's source text; no-op unless the row opens a callout card
/// and is not revealed.
pub fn strip_callout_prefix(row: &mut PlannedRow, line: &str) {
    let Some(callout) = row.callout.as_ref().filter(|c| c.is_first) else {
        return;
    };
    if row.plan.revealed {
        return;
    }
    let tag_len = callout.kind.label().len() + 3; // `[!TAG]`
    let content_off = match &row.block {
        Block::Quote { content, .. } => *content,
        _ => 0,
    };
    let after_tag = line
        .get(content_off + tag_len..)
        .unwrap_or("")
        .strip_prefix(' ')
        .map(|_| tag_len + 1)
        .unwrap_or(tag_len);
    if after_tag <= row.plan.visible.len() {
        let src_off = (content_off + after_tag).min(line.len());
        row.plan = strip_plan_prefix(&row.plan, after_tag, src_off);
    }
}

pub fn strip_plan_prefix(plan: &RowPlan, n_vis: usize, n_src: usize) -> RowPlan {
    let mut out = plan.clone();
    out.visible = out.visible.get(n_vis..).unwrap_or("").into();
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
    use super::super::block::{DocState, classify};
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
    const SHOWCASE: &str = include_str!("../../../docs/markdown-showcase.md");

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
        let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(SHOWCASE);
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
        let ls: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(doc);
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
            let split_cached = plan_rows_with_refs(&ls, &classified, &refs, reveal);
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

#[cfg(test)]
mod incremental_classify_tests {
    use super::*;

    /// Assert `changed` is a true superset of the lines whose block actually
    /// differs from `prev`.
    ///
    /// The view compares only inside this span to decide whether cached
    /// geometry is still valid, so an under-reported span silently renders
    /// stale grouping. The span is deliberately allowed to be too wide; that
    /// only costs a little extra comparison.
    fn assert_changed_is_superset(prev: &Classified, inc: &Classified, what: &str) {
        for i in 0..inc.blocks.len().min(prev.blocks.len()) {
            if inc.blocks[i] != prev.blocks[i] {
                assert!(
                    inc.changed.contains(&i),
                    "{what}: changed={:?} omits line {i}, whose block went {:?} -> {:?}",
                    inc.changed,
                    prev.blocks[i],
                    inc.blocks[i]
                );
            }
        }
    }

    fn doc(n: usize) -> Vec<Line> {
        let mut text = String::new();
        for i in 0..n {
            text.push_str(&format!("# H{i}\n\npara {i} text\n\n"));
        }
        crate::editor::markdown_live::model::lines_from_text(&text)
    }

    /// Incremental classification must equal a full pass, for edits that open
    /// and close every stateful construct (fences, math, frontmatter, tables,
    /// setext runs) at various positions.
    #[test]
    fn incremental_classify_matches_full_pass() {
        let edits: &[&str] = &[
            "x",
            "```",
            "```rust",
            "$$",
            "| a | b |",
            "|---|",
            "===",
            "---",
            "> quote",
            "[!NOTE]",
            "1. item",
            "    indented",
        ];
        for n in [5usize, 40, 200] {
            let mut lines = doc(n);
            let mut prev = classify_document(&lines);
            for round in 0..6 {
                for edit in edits {
                    let at = (round * 5 + 3) % lines.len();
                    // Append to a line (the common keystroke shape).
                    let mut t = lines[at].to_string();
                    t.push_str(edit);
                    lines[at] = std::sync::Arc::new(t);
                    let (inc, _) = classify_incremental(&lines, Some(&prev), at..at + 1);
                    let full = classify_document(&lines);
                    assert_eq!(
                        inc.blocks, full.blocks,
                        "blocks diverged (n={n} round={round} edit={edit:?} at={at})"
                    );
                    assert_eq!(
                        inc.lang_before, full.lang_before,
                        "lang_before diverged (n={n} round={round} edit={edit:?} at={at})"
                    );
                    assert_changed_is_superset(
                        &prev,
                        &inc,
                        &format!("n={n} round={round} edit={edit:?} at={at}"),
                    );
                    prev = inc;
                }
            }
        }
    }

    /// A single-line edit must re-classify a bounded window whose size does
    /// **not** grow with the document.
    ///
    /// The bound is the checkpoint granularity (replay from the nearest saved
    /// state) plus the convergence run; both are constants, so the same edit
    /// costs the same on a 500-line and a 20k-line document.
    #[test]
    fn incremental_classify_window_is_document_independent() {
        let bound = CLASSIFY_CHECKPOINT + 4;
        let mut measured = Vec::new();
        for n in [500usize, 5_000, 20_000] {
            let mut lines = doc(n);
            let prev = classify_document(&lines);
            let at = n / 2;
            let mut t = lines[at].to_string();
            t.push('x');
            lines[at] = std::sync::Arc::new(t);
            let (_, reprocessed) = classify_incremental(&lines, Some(&prev), at..at + 1);
            assert!(
                reprocessed < bound,
                "a one-line edit re-classified {reprocessed} lines of {n} \
                 (bound {bound}); convergence is not working"
            );
            measured.push(reprocessed);
        }
        assert!(
            measured.iter().all(|&m| m == measured[0]),
            "re-classified window grew with document size: {measured:?}"
        );
    }

    /// Opening a fence must invalidate the tail (no false convergence), and
    /// closing it must let the tail converge again.
    #[test]
    fn fence_open_and_close_re_classify_the_tail() {
        let n = 300;
        let mut lines = doc(n);
        let prev = classify_document(&lines);
        let at = 10;
        lines[at] = std::sync::Arc::new("```rust".to_string());
        let (inc, reprocessed) = classify_incremental(&lines, Some(&prev), at..at + 1);
        assert!(
            reprocessed > n / 2,
            "an unclosed fence must invalidate the tail (got {reprocessed} of {n})"
        );
        assert_eq!(inc.blocks, classify_document(&lines).blocks);
    }

    /// A prose document whose paragraph runs straddle checkpoint boundaries.
    /// The old fixtures were all shorter than `CLASSIFY_CHECKPOINT` (or edited
    /// at positions that never landed on a multiple of it), so the checkpoint
    /// resume path was never exercised by the differential tests.
    fn prose(n: usize) -> Vec<Line> {
        let mut text = String::new();
        for i in 0..n {
            if i % 3 == 2 {
                text.push('\n');
            } else {
                text.push_str(&format!("paragraph line {i} with some prose text\n"));
            }
        }
        crate::editor::markdown_live::model::lines_from_text(&text)
    }

    /// Regression: an edit landing **exactly on** a checkpoint multiple must
    /// not freeze the line above it.
    ///
    /// The resume point used to round down to `dirty_from` itself, but line
    /// `from - 1` had been classified with `next = lines[from]` — the line that
    /// just changed. Typing a setext underline on such a line therefore left
    /// the heading above it rendered as a paragraph, permanently (later
    /// unrelated edits did not repair it).
    #[test]
    fn setext_edit_on_checkpoint_multiple_reclassifies_the_line_above() {
        for n in [CLASSIFY_CHECKPOINT * 3, 200] {
            let base = prose(n);
            let prev = classify_document(&base);
            for multiple in 1..(n / CLASSIFY_CHECKPOINT) {
                let at = multiple * CLASSIFY_CHECKPOINT;
                // Make the line above the checkpoint a paragraph, then replace
                // the checkpoint line itself with a setext underline.
                let mut lines = base.clone();
                lines[at - 1] = std::sync::Arc::new("Section title".to_string());
                lines[at] = std::sync::Arc::new("===".to_string());
                let (inc, _) = classify_incremental(&lines, Some(&prev), at..at + 1);
                let full = classify_document(&lines);
                assert_eq!(
                    inc.blocks, full.blocks,
                    "diverged with the edit exactly on checkpoint {at} (n={n})"
                );
                assert_eq!(inc.lang_before, full.lang_before, "lang_before at {at}");
            }
        }
    }

    /// Regression: deleting the underline of a paragraph run that **straddles a
    /// checkpoint** must revert the whole run.
    ///
    /// The stored prefix is post-restyle: `a\nb\n===` rewrites the entire run
    /// into `Heading { content: 0 }`. Copying that prefix verbatim re-introduced
    /// headings whose justifying underline had just been deleted.
    #[test]
    fn deleting_underline_reverts_a_run_straddling_a_checkpoint() {
        let n = 200;
        for run_len in [8usize, 40, 70] {
            let underline = run_len; // run occupies 0..run_len, underline follows
            let mut text = String::new();
            for i in 0..run_len {
                text.push_str(&format!("para line {i}\n"));
            }
            text.push_str("===\n");
            for i in 0..(n - run_len - 1) {
                text.push_str(&format!("tail {i}\n"));
            }
            let mut lines = crate::editor::markdown_live::model::lines_from_text(&text);
            let prev = classify_document(&lines);
            assert!(
                matches!(prev.blocks[0], Block::Heading { content: 0, .. }),
                "fixture must start as a restyled heading run"
            );
            // Delete the underline.
            lines[underline] = std::sync::Arc::new(String::new());
            let (inc, _) = classify_incremental(&lines, Some(&prev), underline..underline + 1);
            let full = classify_document(&lines);
            assert_changed_is_superset(&prev, &inc, "setext at checkpoint");
            assert_eq!(
                inc.blocks, full.blocks,
                "run of {run_len} lines diverged after deleting its underline"
            );
            assert_changed_is_superset(&prev, &inc, &format!("delete underline, run {run_len}"));
        }
    }

    /// A newly created setext underline must report the whole paragraph run it
    /// restyles as changed, even when the run starts far *above* the resume
    /// point.
    ///
    /// The restyle pass rewrites earlier lines of the run (that is what makes
    /// "a\nb\n===" one heading in the preview), and it walks back past the
    /// checkpoint the classifier resumed from. `changed` is what tells the view
    /// which rows to re-compare for cached-geometry validity, so if it started
    /// at the resume point instead of the run start, those rows would keep
    /// rendering as paragraphs.
    #[test]
    fn created_underline_reports_the_whole_restyled_run() {
        let n = 300;
        let run = 130; // spans two checkpoints, so the run starts before `start`
        // One unbroken paragraph run: `prose` inserts blank lines, which would
        // split the run and defeat the point of the test.
        let mut text = String::new();
        for i in 0..run {
            text.push_str(&format!("paragraph line {i} with some prose text\n"));
        }
        for i in 0..(n - run - 1) {
            text.push_str(&format!("tail {i}\n"));
        }
        let mut lines = crate::editor::markdown_live::model::lines_from_text(&text);
        let prev = classify_document(&lines);
        assert!(
            matches!(prev.blocks[run - 1], Block::Paragraph),
            "fixture must be a plain paragraph run, got {:?}",
            prev.blocks[run - 1]
        );
        assert!(
            matches!(prev.blocks[0], Block::Paragraph),
            "the run must reach line 0 so the restyle walks back past the resume point"
        );

        // Turn the line after the run into a setext underline.
        lines[run] = std::sync::Arc::new("===".to_string());
        let (inc, _) = classify_incremental(&lines, Some(&prev), run..run + 1);
        let full = classify_document(&lines);
        assert_eq!(inc.blocks, full.blocks, "restyled run diverged");
        // The run really was restyled...
        assert!(
            matches!(inc.blocks[0], Block::Heading { content: 0, .. }),
            "line 0 must become part of the restyled heading run, got {:?}",
            inc.blocks[0]
        );
        // ...and all of it is reported as changed.
        assert_changed_is_superset(&prev, &inc, "created underline restyles the run above");
    }

    /// Regression: one edit touching **two separated lines** must not reuse a
    /// tail containing the later one.
    ///
    /// `dirty_from` used to be a single scalar (the range start), so
    /// classification converged inside the untouched gap and copied the second
    /// edited line verbatim. This is the shape of replace-all and of a
    /// coalesced undo.
    #[test]
    fn multi_region_edit_does_not_reuse_an_edited_tail() {
        let n = 300;
        let mut lines = prose(n);
        let prev = classify_document(&lines);
        let (first, second) = (20usize, 200usize);
        lines[first] = std::sync::Arc::new("para tweaked".to_string());
        lines[second] = std::sync::Arc::new("```rust".to_string());
        let (inc, _) = classify_incremental(&lines, Some(&prev), first..second + 1);
        let full = classify_document(&lines);
        assert_changed_is_superset(&prev, &inc, "multi-region edit");
        assert_eq!(inc.blocks, full.blocks, "multi-region edit diverged");
        assert_eq!(inc.lang_before, full.lang_before);
    }
}
