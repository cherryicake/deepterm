//! Editor-based git diff view, modeled after zed's split diff.
//!
//! Both layouts are built on the real editor (`InputState` code editor), so
//! font, colors, selection, scrolling and undo behave exactly like a normal
//! editor tab:
//!
//! - **Split**: left = HEAD content (read-only), right = working tree
//!   content (fully editable). Added/deleted line backgrounds and
//!   word-level highlights are injected through the editor's diff-decoration
//!   API. Scroll is synchronized through the hunk mapping, and every hunk
//!   shows a revert button that restores the HEAD version of that block.
//! - **Unified**: single read-only editor with the merged old+new text
//!   (deleted lines interleaved), line backgrounds and word highlights.
//!
//! The default layout preference is `Auto`: unified (single column) is shown
//! by default, and the view automatically switches to split once the panel
//! width reaches `SPLIT_MIN_WIDTH`. The toolbar button cycles
//! Auto → Unified → Split → Auto to pin a specific layout.
//!
//! Saving (toolbar button or cmd+S via the editor panel) writes the working
//! tree file through the repository backend (local fs / SFTP).

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, HighlightStyle, Hsla, Render, Subscription, WeakEntity, Window, div,
    point, px,
};

use crate::components::input::{EditorInput, InputState};
use crate::editor::editor::Editor;
use crate::syntax::{HighlightTheme, SimpleHighlighter};
use crate::theme::ActiveTheme;
use ropey::Rope;

use super::diff_model::{
    DiffFile, DiffHunk, LineKind, UnifiedSource, build_unified, compute_hunks, line_count,
    line_range, line_starts,
};
use super::git_repository::GitRepository;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffMode {
    Unified,
    Split,
}

/// Layout preference. `Auto` (default) shows the unified single-column
/// layout and switches to split only when the available width is sufficient.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutPreference {
    Auto,
    Unified,
    Split,
}

/// Minimum content width for the automatic split layout (two readable
/// editor columns with gutters).
const SPLIT_MIN_WIDTH: f32 = 960.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

pub struct DiffView {
    path: String,
    repo: GitRepository,
    editor: WeakEntity<Editor>,
    /// File content at HEAD.
    old_text: Arc<str>,
    /// Working-tree content at last load/save (dirty baseline).
    saved_text: String,
    left: Entity<InputState>,
    right: Entity<InputState>,
    unified: Entity<InputState>,
    hunks: Vec<DiffHunk>,
    additions: usize,
    deletions: usize,
    preference: LayoutPreference,
    /// Content width reported by the editor panel; drives `Auto` layout.
    available_width: f32,
    /// Per-source syntax trees for the unified view: HEAD text and
    /// working-tree text (each valid, unlike the merged display text).
    old_highlighter: SimpleHighlighter,
    new_highlighter: SimpleHighlighter,
    /// Texts the two highlighters were last parsed with (skip re-parses).
    old_parsed: Option<Arc<str>>,
    new_parsed: Option<String>,
    dirty: bool,
    saving: bool,
    refreshing: bool,
    recompute_gen: u32,
    scroll_syncing: bool,
    _subscriptions: Vec<Subscription>,
}

impl DiffView {
    pub fn new(
        diff: DiffFile,
        repo: GitRepository,
        editor: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let old_text: Arc<str> = diff.old_text.clone().into();
        let saved_text = diff.new_text.clone();

        let left = Self::new_editor(&diff.language, &diff.old_text, true, window, cx);
        let right = Self::new_editor(&diff.language, &diff.new_text, false, window, cx);
        let unified = Self::new_editor(&diff.language, "", true, window, cx);

        let mut this = Self {
            path: diff.path,
            repo,
            editor,
            old_text,
            saved_text,
            left,
            right,
            unified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
            preference: LayoutPreference::Auto,
            available_width: 0.0,
            old_highlighter: SimpleHighlighter::new(&diff.language),
            new_highlighter: SimpleHighlighter::new(&diff.language),
            old_parsed: None,
            new_parsed: None,
            dirty: false,
            saving: false,
            refreshing: false,
            recompute_gen: 0,
            scroll_syncing: false,
            _subscriptions: Vec::new(),
        };

        this.recompute(cx);

        // Live updates while editing the new side.
        let right = this.right.clone();
        this._subscriptions.push(cx.subscribe(
            &right,
            |this, _, event: &crate::components::input::InputEvent, cx| {
                if matches!(event, crate::components::input::InputEvent::Change) {
                    this.on_right_changed(cx);
                }
            },
        ));
        // Scroll sync both ways.
        this._subscriptions.push(cx.observe(&this.left.clone(), |this, _, cx| {
            this.sync_scroll(Side::Left, cx);
        }));
        this._subscriptions.push(cx.observe(&this.right.clone(), |this, _, cx| {
            this.sync_scroll(Side::Right, cx);
        }));

        this
    }

    fn new_editor(
        language: &str,
        text: &str,
        readonly: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true);
            state.set_value(text, window, cx);
            state.disabled = readonly;
            // No soft wrap in diff views: hunk-anchored scroll sync assumes a
            // uniform line height (same as zed's diff default).
            state.soft_wrap = false;
            state
        })
    }

    // ---- colors -----------------------------------------------------------

    fn added_line_bg(cx: &App) -> Hsla {
        cx.theme().status_added.opacity(0.14)
    }

    fn deleted_line_bg(cx: &App) -> Hsla {
        cx.theme().status_deleted.opacity(0.14)
    }

    fn added_word_bg(cx: &App) -> Hsla {
        cx.theme().status_added.opacity(0.38)
    }

    fn deleted_word_bg(cx: &App) -> Hsla {
        cx.theme().status_deleted.opacity(0.38)
    }

    // ---- diff recomputation ------------------------------------------------

    fn on_right_changed(&mut self, cx: &mut Context<Self>) {
        let current = self.right.read(cx).value().to_string();
        self.dirty = current != self.saved_text;
        self.schedule_recompute(cx);
        cx.notify();
    }

    fn schedule_recompute(&mut self, cx: &mut Context<Self>) {
        self.recompute_gen += 1;
        let generation = self.recompute_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            this.update(cx, |view, cx| {
                if view.recompute_gen == generation {
                    view.recompute(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Recompute hunks from HEAD content vs. current right-editor content and
    /// push decorations into both editors.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        let new_text = self.right.read(cx).value().to_string();
        self.hunks = compute_hunks(&self.old_text, &new_text);
        self.additions = self.hunks.iter().map(|h| h.additions()).sum();
        self.deletions = self.hunks.iter().map(|h| h.deletions()).sum();

        let added_line = Self::added_line_bg(cx);
        let deleted_line = Self::deleted_line_bg(cx);
        let added_word = Self::added_word_bg(cx);
        let deleted_word = Self::deleted_word_bg(cx);

        let mut new_line_colors = Vec::new();
        let mut new_word_colors = Vec::new();
        let mut old_line_colors = Vec::new();
        let mut old_word_colors = Vec::new();
        for hunk in &self.hunks {
            for row in hunk.new_rows.clone() {
                new_line_colors.push((row, added_line));
            }
            for row in hunk.old_rows.clone() {
                old_line_colors.push((row, deleted_line));
            }
            for r in &hunk.new_word_ranges {
                new_word_colors.push((r.clone(), added_word));
            }
            for r in &hunk.old_word_ranges {
                old_word_colors.push((r.clone(), deleted_word));
            }
        }

        self.right.update(cx, |state, cx| {
            state.set_diff_decorations(new_line_colors, new_word_colors, cx);
        });
        self.left.update(cx, |state, cx| {
            state.set_diff_decorations(old_line_colors, old_word_colors, cx);
        });

        if self.effective_mode() == DiffMode::Unified {
            self.rebuild_unified(cx);
        }
        cx.notify();
    }

    /// Rebuild the merged unified editor from HEAD + current new content.
    fn rebuild_unified(&mut self, cx: &mut Context<Self>) {
        let new_text = self.right.read(cx).value().to_string();
        let unified = build_unified(&self.old_text, &new_text);

        let added_line = Self::added_line_bg(cx);
        let deleted_line = Self::deleted_line_bg(cx);
        let added_word = Self::added_word_bg(cx);
        let deleted_word = Self::deleted_word_bg(cx);

        let mut line_colors = Vec::new();
        let mut word_colors = Vec::new();
        let mut labels = Vec::with_capacity(unified.lines.len());
        for (row, line) in unified.lines.iter().enumerate() {
            match line.kind {
                LineKind::Context => {}
                LineKind::Add => line_colors.push((row as u32, added_line)),
                LineKind::Del => line_colors.push((row as u32, deleted_line)),
            }
            let word_bg = match line.kind {
                LineKind::Context => None,
                LineKind::Add => Some(added_word),
                LineKind::Del => Some(deleted_word),
            };
            if let Some(bg) = word_bg {
                for r in &line.word_ranges {
                    word_colors.push((r.clone(), bg));
                }
            }
            labels.push(line.new_lineno.or(line.old_lineno));
        }

        // Re-parse only the changed source side(s).
        if self.old_parsed.as_deref() != Some(self.old_text.as_ref()) {
            let rope = Rope::from_str(&self.old_text);
            self.old_highlighter.update(None, &rope, None);
            self.old_parsed = Some(self.old_text.clone());
        }
        if self.new_parsed.as_deref() != Some(new_text.as_str()) {
            let rope = Rope::from_str(&new_text);
            self.new_highlighter.update(None, &rope, None);
            self.new_parsed = Some(new_text.clone());
        }

        let theme = cx.theme().highlight_theme.clone();
        let runs = Self::merged_highlight_runs(
            &unified,
            &self.old_text,
            &new_text,
            &mut self.old_highlighter,
            &mut self.new_highlighter,
            &theme,
        );

        self.unified.update(cx, |state, cx| {
            // set_value needs a window; use the windowless variant.
            state.set_text(unified.text.clone(), cx);
            // The merged text is not valid source code, so syntax
            // highlighting is computed per source text above and injected
            // here (this also skips the internal highlighter parse).
            state.set_external_highlight_runs(Some(runs), cx);
            state.set_diff_decorations(line_colors, word_colors, cx);
            state.set_diff_line_labels(Some(labels), cx);
        });
    }

    /// Build gap-free highlight runs for the merged unified text, colored
    /// per source tree: deleted lines from the HEAD parse, context/added
    /// lines from the working-tree parse. Multi-line captures (strings,
    /// comments) are split at merged line boundaries and byte offsets are
    /// remapped into the merged text.
    fn merged_highlight_runs(
        unified: &UnifiedSource,
        old_text: &str,
        new_text: &str,
        old_highlighter: &mut SimpleHighlighter,
        new_highlighter: &mut SimpleHighlighter,
        theme: &HighlightTheme,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let old_starts = line_starts(old_text);
        let new_starts = line_starts(new_text);

        let mut runs: Vec<(Range<usize>, HighlightStyle)> =
            Vec::with_capacity(unified.lines.len());
        for line in &unified.lines {
            let (highlighter, src_start, src_end) = match line.kind {
                LineKind::Del => {
                    let row = line.old_lineno.unwrap_or(0);
                    let range = line_range(old_text, &old_starts, row);
                    (&mut *old_highlighter, range.start, range.end)
                }
                LineKind::Context | LineKind::Add => {
                    let row = line.new_lineno.unwrap_or(0);
                    let range = line_range(new_text, &new_starts, row);
                    (&mut *new_highlighter, range.start, range.end)
                }
            };
            if src_start >= src_end {
                continue;
            }
            for (range, style) in highlighter.styles(&(src_start..src_end), theme) {
                let start = range.start.max(src_start).min(src_end);
                let end = range.end.min(src_end).max(start);
                if end > start {
                    let offset = line.merged_range.start - src_start;
                    runs.push((start + offset..end + offset, style));
                }
            }
        }

        fill_gaps(&mut runs, unified.text.len());
        runs
    }

    // ---- scroll sync --------------------------------------------------------

    fn sync_scroll(&mut self, source: Side, cx: &mut Context<Self>) {
        if self.effective_mode() != DiffMode::Split || self.scroll_syncing {
            return;
        }
        let (src, dst) = match source {
            Side::Left => (&self.left, &self.right),
            Side::Right => (&self.right, &self.left),
        };
        let (offset, line_height) = {
            let state = src.read(cx);
            let line_height = state
                .last_layout
                .as_ref()
                .map(|l| l.line_height)
                .unwrap_or(px(0.));
            (state.scroll_handle.offset(), line_height)
        };
        if line_height <= px(0.) {
            return;
        }
        let top_line = ((-offset.y) / line_height).max(0.) as u32;
        let mapped = match source {
            Side::Left => map_line_old_to_new(top_line, &self.hunks),
            Side::Right => map_line_new_to_old(top_line, &self.hunks),
        };
        let desired_y = -(line_height * mapped as f32);
        let current = dst.read(cx).scroll_handle.offset();
        if (current.y - desired_y).abs() > line_height / 2. || current.x != offset.x {
            self.scroll_syncing = true;
            dst.update(cx, |state, cx| {
                state.update_scroll_offset(Some(point(offset.x, desired_y)), cx);
            });
            self.scroll_syncing = false;
        }
    }

    // ---- layout -------------------------------------------------------------

    /// The layout actually shown: explicit preference wins; `Auto` picks by
    /// available width (unified by default, split when wide enough).
    fn effective_mode(&self) -> DiffMode {
        match self.preference {
            LayoutPreference::Unified => DiffMode::Unified,
            LayoutPreference::Split => DiffMode::Split,
            LayoutPreference::Auto => {
                if self.available_width >= SPLIT_MIN_WIDTH {
                    DiffMode::Split
                } else {
                    DiffMode::Unified
                }
            }
        }
    }

    /// Called by the editor panel each render with the current content width.
    pub fn set_available_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if (self.available_width - width).abs() < 1.0 {
            return;
        }
        let before = self.effective_mode();
        self.available_width = width;
        let after = self.effective_mode();
        if after == DiffMode::Unified && before != after {
            self.rebuild_unified(cx);
        }
        cx.notify();
    }

    // ---- actions ------------------------------------------------------------

    /// Cycle layout preference: Auto → Unified → Split → Auto.
    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.preference = match self.preference {
            LayoutPreference::Auto => LayoutPreference::Unified,
            LayoutPreference::Unified => LayoutPreference::Split,
            LayoutPreference::Split => LayoutPreference::Auto,
        };
        if self.effective_mode() == DiffMode::Unified {
            self.rebuild_unified(cx);
        }
        cx.notify();
    }

    /// Revert one hunk: replace the new-side lines with the HEAD lines.
    fn revert_hunk(&mut self, hunk_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hunk) = self.hunks.get(hunk_ix).cloned() else {
            return;
        };
        let current = self.right.read(cx).value().to_string();
        let starts = line_starts(&current);
        let count = line_count(&current, &starts);

        let byte_start = if hunk.new_rows.start < count {
            starts[hunk.new_rows.start as usize]
        } else {
            current.len()
        };
        let byte_end = if hunk.new_rows.end < count {
            starts[hunk.new_rows.end as usize]
        } else {
            current.len()
        };

        // Exact HEAD bytes for the reverted line range.
        let old_starts = line_starts(&self.old_text);
        let old_count = line_count(&self.old_text, &old_starts);
        let repl_start = if hunk.old_rows.start < old_count {
            old_starts[hunk.old_rows.start as usize]
        } else {
            self.old_text.len()
        };
        let repl_end = if hunk.old_rows.end < old_count {
            old_starts[hunk.old_rows.end as usize]
        } else {
            self.old_text.len()
        };
        let replacement = &self.old_text[repl_start..repl_end];

        let mut updated =
            String::with_capacity(current.len() - (byte_end - byte_start) + replacement.len());
        updated.push_str(&current[..byte_start]);
        updated.push_str(replacement);
        updated.push_str(&current[byte_end..]);

        self.right.update(cx, |state, cx| {
            state.set_value(updated, window, cx);
        });
        let current = self.right.read(cx).value().to_string();
        self.dirty = current != self.saved_text;
        self.recompute(cx);
    }

    /// Save the working-tree file through the repository backend.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving || !self.dirty {
            return;
        }
        self.saving = true;
        cx.notify();

        let text = self.right.read(cx).value().to_string();
        let repo = self.repo.clone();
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let write_text = text.clone();
            let result = cx
                .background_executor()
                .spawn(async move { repo.write_file(&path, &write_text) })
                .await;
            this.update(cx, |view, cx| {
                view.saving = false;
                match result {
                    Ok(()) => {
                        view.saved_text = text.clone();
                        view.dirty = view.right.read(cx).value().as_ref() != text.as_str();
                    }
                    Err(e) => {
                        log::error!("diff save failed for {}: {e}", view.path);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-fetch HEAD + working-tree content (discards unsaved edits).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        cx.notify();

        let repo = self.repo.clone();
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { repo.diff_file_contents(&path) }
                })
                .await;
            this.update(cx, |view, cx| {
                view.refreshing = false;
                if let Ok((old, new)) = result {
                    view.old_text = old.into();
                    view.saved_text = new.clone();
                    view.dirty = false;
                    view.right.update(cx, |state, cx| {
                        state.set_text(new, cx);
                    });
                    view.recompute(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the diffed file in a normal editor tab.
    fn open_file(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        let ssh_config = self.repo.ssh_config();
        let full_path = self.repo.resolve_path(&self.path);
        window.defer(cx, move |window, cx| {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |e, cx| match ssh_config {
                    Some(config) => e.open_remote_file(full_path, config, window, cx),
                    None => e.open_file(std::path::PathBuf::from(full_path), window, cx),
                });
            }
        });
    }

    /// Line height + scroll offset of the right editor (for overlay placement).
    fn right_scroll_metrics(&self, cx: &App) -> (gpui::Pixels, gpui::Point<gpui::Pixels>) {
        let state = self.right.read(cx);
        let line_height = state
            .last_layout
            .as_ref()
            .map(|l| l.line_height)
            .unwrap_or(px(0.));
        (line_height, state.scroll_handle.offset())
    }

    // ---- rendering ------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mode_label = match self.preference {
            LayoutPreference::Auto => "Layout: Auto",
            LayoutPreference::Unified => "Layout: Unified",
            LayoutPreference::Split => "Layout: Split",
        };
        let busy = self.refreshing || self.saving;
        let dirty = self.dirty;

        let button = |id: &'static str,
                      label: &'static str,
                      enabled: bool,
                      theme: &crate::theme::UiTheme| {
            div()
                .id(id)
                .px(px(8.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.border)
                .when(enabled, |this| this.cursor_pointer())
                .text_color(if enabled {
                    theme.text_muted
                } else {
                    theme.text_subtle
                })
                .when(enabled, |this| {
                    this.hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                })
                .child(label)
        };

        div()
            .flex()
            .w_full()
            .h(px(32.0))
            .flex_none()
            .items_center()
            .px(px(12.0))
            .gap(px(12.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.panel_background)
            .text_size(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(theme.text)
                    .child(self.path.clone()),
            )
            .child(
                div()
                    .text_color(theme.status_added)
                    .child(format!("+{}", self.additions)),
            )
            .child(
                div()
                    .text_color(theme.status_deleted)
                    .child(format!("-{}", self.deletions)),
            )
            .child(
                button("diff-open-file", "Open File", true, theme).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.open_file(window, cx);
                    },
                )),
            )
            .child(
                button(
                    "diff-refresh",
                    if self.refreshing {
                        "Refreshing…"
                    } else {
                        "Refresh"
                    },
                    !busy,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.refresh(cx);
                })),
            )
            .child(
                button(
                    "diff-save",
                    if self.saving {
                        "Saving…"
                    } else if dirty {
                        "● Save"
                    } else {
                        "Save"
                    },
                    dirty && !self.saving,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.save(cx);
                })),
            )
            .child(
                button("diff-toggle-mode", mode_label, true, theme).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.toggle_mode(cx);
                    },
                )),
            )
    }

    fn render_split(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let border = theme.border;
        let editor_bg = theme.editor_background();

        // Revert buttons overlay, positioned by hunk line and scroll offset.
        let (line_height, scroll_offset) = self.right_scroll_metrics(cx);
        let revert_buttons: Vec<gpui::AnyElement> = if line_height > px(0.) {
            self.hunks
                .iter()
                .enumerate()
                .map(|(ix, hunk)| {
                    let y = line_height * hunk.new_rows.start as f32 + scroll_offset.y;
                    div()
                        .id(("revert-hunk", ix))
                        .absolute()
                        .top(y)
                        .right(px(12.0))
                        .h(line_height)
                        .px(px(6.0))
                        .flex()
                        .items_center()
                        .rounded(px(4.0))
                        .bg(theme.elevated_background)
                        .border_1()
                        .border_color(theme.border)
                        .cursor_pointer()
                        .text_color(theme.text_muted)
                        .text_size(px(11.0))
                        .hover(|s| s.text_color(theme.text).border_color(theme.accent))
                        .child("↩ Revert")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.revert_hunk(ix, window, cx);
                        }))
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };

        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .bg(editor_bg)
                    .child(EditorInput::new(&self.left).disabled(true)),
            )
            .child(div().w(px(1.0)).flex_none().h_full().bg(border))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .bg(editor_bg)
                    .child(EditorInput::new(&self.right))
                    .children(revert_buttons),
            )
    }

    fn render_unified(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(theme.editor_background())
            .child(EditorInput::new(&self.unified).disabled(true))
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let editor_bg = theme.editor_background();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(editor_bg)
            .child(self.render_toolbar(cx))
            .child(match self.effective_mode() {
                DiffMode::Split => self.render_split(cx).into_any_element(),
                DiffMode::Unified => self.render_unified(cx).into_any_element(),
            })
    }
}

/// Fill the gaps of a sorted, non-overlapping run list with the default
/// style so the result is contiguous and covers `[0, total_len)` — the
/// renderer requires full coverage (same guarantee `unique_styles` gives
/// for the built-in highlighter).
fn fill_gaps(runs: &mut Vec<(Range<usize>, HighlightStyle)>, total_len: usize) {
    let input = std::mem::take(runs);
    let mut filled = Vec::with_capacity(input.len() * 2 + 1);
    let mut cursor = 0usize;
    for (range, style) in input {
        let start = range.start.max(cursor);
        if start > cursor {
            filled.push((cursor..start, HighlightStyle::default()));
        }
        let end = range.end.max(start);
        if end > start {
            filled.push((start..end, style));
        }
        cursor = end;
    }
    if cursor < total_len {
        filled.push((cursor..total_len, HighlightStyle::default()));
    }
    *runs = filled;
}

/// Map a line number on the old side to the corresponding line on the new
/// side, using cumulative hunk deltas (context regions map exactly).
fn map_line_old_to_new(old_line: u32, hunks: &[DiffHunk]) -> u32 {
    let mut delta = 0i64;
    for h in hunks {
        if old_line < h.old_rows.start {
            break;
        }
        if old_line >= h.old_rows.end {
            delta += (h.new_rows.end - h.new_rows.start) as i64
                - (h.old_rows.end - h.old_rows.start) as i64;
        } else {
            // Inside a hunk: approximate with proportional offset.
            let rel = (old_line - h.old_rows.start) as i64;
            return (h.new_rows.start as i64 + rel).max(0) as u32;
        }
    }
    (old_line as i64 + delta).max(0) as u32
}

/// Map a line number on the new side to the corresponding line on the old
/// side, using cumulative hunk deltas.
fn map_line_new_to_old(new_line: u32, hunks: &[DiffHunk]) -> u32 {
    let mut delta = 0i64;
    for h in hunks {
        if new_line < h.new_rows.start {
            break;
        }
        if new_line >= h.new_rows.end {
            delta += (h.old_rows.end - h.old_rows.start) as i64
                - (h.new_rows.end - h.new_rows.start) as i64;
        } else {
            let rel = (new_line - h.new_rows.start) as i64;
            return (h.old_rows.start as i64 + rel).max(0) as u32;
        }
    }
    (new_line as i64 + delta).max(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{HighlightTheme, HighlightThemeStyle, SyntaxColors, ThemeStyle};
    use crate::theme::ThemeAppearance;

    fn test_theme() -> HighlightTheme {
        HighlightTheme {
            name: "test".into(),
            appearance: ThemeAppearance::Dark,
            style: HighlightThemeStyle {
                syntax: SyntaxColors {
                    keyword: Some(ThemeStyle::color(gpui::red())),
                    type_: Some(ThemeStyle::color(gpui::blue())),
                    function: Some(ThemeStyle::color(gpui::green())),
                    number: Some(ThemeStyle::color(gpui::yellow())),
                    string: Some(ThemeStyle::color(gpui::blue())),
                    comment: Some(ThemeStyle::color(gpui::red())),
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    fn build_runs(old: &str, new: &str) -> (UnifiedSource, Vec<(Range<usize>, HighlightStyle)>) {
        let unified = build_unified(old, new);
        let mut old_hl = SimpleHighlighter::new("rust");
        let mut new_hl = SimpleHighlighter::new("rust");
        old_hl.update(None, &Rope::from_str(old), None);
        new_hl.update(None, &Rope::from_str(new), None);
        let theme = test_theme();
        let runs = DiffView::merged_highlight_runs(&unified, old, new, &mut old_hl, &mut new_hl, &theme);
        (unified, runs)
    }

    fn assert_contiguous(runs: &[(Range<usize>, HighlightStyle)], total_len: usize) {
        let mut cursor = 0;
        for (range, _) in runs {
            assert_eq!(range.start, cursor, "runs must be contiguous");
            assert!(range.end > range.start, "no empty runs");
            cursor = range.end;
        }
        assert_eq!(cursor, total_len, "runs must cover the full text");
    }

    /// Find the byte offset of `needle` inside a merged line.
    fn line_offset(unified: &UnifiedSource, line: usize, needle: &str) -> usize {
        let text = &unified.text[unified.lines[line].merged_range.clone()];
        unified.lines[line].merged_range.start + text.find(needle).expect("needle in line")
    }

    /// The merged text of a multi-line-string edit contains an unclosed
    /// quote (the added line re-opens a string), which would swallow the
    /// rest of the file under a single merged-text parse. Each source side
    /// must still be highlighted from its own valid tree.
    #[test]
    fn test_unclosed_string_in_added_line() {
        let old = "fn main() {\n    let s = \"abc\n    def\";\n    let y = s;\n}\n";
        let new = "fn main() {\n    let s = \"abc\n    xyz\";\n    let y = s;\n}\n";
        let (unified, runs) = build_runs(old, new);
        assert_contiguous(&runs, unified.text.len());

        let del = unified.lines.iter().position(|l| l.kind == LineKind::Del).unwrap();
        let add = unified.lines.iter().position(|l| l.kind == LineKind::Add).unwrap();

        // Deleted `def";` is still inside the HEAD string literal.
        let off = line_offset(&unified, del, "def\"");
        let run = runs.iter().find(|(r, _)| r.start <= off && r.end > off).unwrap();
        assert_eq!(run.1.color, Some(gpui::blue()), "del line inside string");

        // Code after the hunk must NOT be swallowed by the merged-text
        // string: the `let` keyword after the add block is keyword-colored.
        let let_off = line_offset(&unified, add + 1, "let");
        let run = runs.iter().find(|(r, _)| r.start <= let_off && r.end > let_off).unwrap();
        assert_eq!(run.1.color, Some(gpui::red()), "keyword after hunk");
    }

    /// Deleted and added lines each keep the colors of their own source
    /// text, even when the merged text stays parseable.
    #[test]
    fn test_del_and_add_from_own_trees() {
        let old = "fn main() {\n    if x {\n        foo();\n    }\n}\n";
        let new = "fn main() {\n    if x {\n        bar();\n    }\n}\n";
        let (unified, runs) = build_runs(old, new);
        assert_contiguous(&runs, unified.text.len());

        let del = unified.lines.iter().position(|l| l.kind == LineKind::Del).unwrap();
        let add = unified.lines.iter().position(|l| l.kind == LineKind::Add).unwrap();

        let off = line_offset(&unified, del, "foo");
        let run = runs.iter().find(|(r, _)| r.start <= off && r.end > off).unwrap();
        assert_eq!(run.1.color, Some(gpui::green()), "deleted call from HEAD tree");

        let off = line_offset(&unified, add, "bar");
        let run = runs.iter().find(|(r, _)| r.start <= off && r.end > off).unwrap();
        assert_eq!(run.1.color, Some(gpui::green()), "added call from working-tree");
    }

    #[test]
    fn test_fill_gaps() {
        let mut runs = vec![
            (2..4, HighlightStyle {
                color: Some(gpui::red()),
                ..Default::default()
            }),
            (5..6, HighlightStyle {
                color: Some(gpui::blue()),
                ..Default::default()
            }),
        ];
        fill_gaps(&mut runs, 8);
        assert_eq!(runs.len(), 5);
        assert_eq!(runs[0].0, 0..2);
        assert!(runs[0].1.color.is_none());
        assert_eq!(runs[1].0, 2..4);
        assert_eq!(runs[1].1.color, Some(gpui::red()));
        assert_eq!(runs[2].0, 4..5);
        assert!(runs[2].1.color.is_none());
        assert_eq!(runs[3].0, 5..6);
        assert_eq!(runs[4].0, 6..8);
        assert!(runs[4].1.color.is_none());
    }
}
