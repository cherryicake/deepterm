//! Read-only git diff view, modeled after zed / VS Code.
//!
//! - Unified and side-by-side layouts, switchable from the toolbar.
//! - Code is syntax highlighted with the file's own language via
//!   `SimpleHighlighter` (old and new text are highlighted independently,
//!   like zed's base/buffer pair).
//! - Text style (font family, size, line height, base color) is inherited
//!   from the window text style and the highlight theme's editor colors,
//!   exactly like the regular code editor (`InputState`), so both views
//!   always look identical.
//! - Line backgrounds + word-level inline highlights use colors derived from
//!   the theme's git status colors.

use std::collections::BTreeSet;
use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    App, Context, Font, Hsla, HighlightStyle, Pixels, Render, ScrollHandle, SharedString,
    StyledText, TextRun, Window, black, div, px,
};

use crate::syntax::{HighlightTheme, SyntaxHighlighter};
use crate::theme::ActiveTheme;

use super::diff_model::{DiffFile, DiffRow};
use super::git_repository::GitRepository;

/// Right margin after a line number column, same as the editor's
/// `LINE_NUMBER_RIGHT_MARGIN`.
const LINE_NUMBER_RIGHT_MARGIN: Pixels = px(10.);
/// Width of the +/- sign column in unified mode.
const SIGN_COLUMN_WIDTH: Pixels = px(16.);

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffMode {
    Unified,
    Split,
}

/// One cell of a side-by-side row.
#[derive(Clone)]
enum SplitCell {
    /// Placeholder used to align the two sides.
    Empty,
    Line {
        /// Display text and highlight source: `false` = old text, `true` = new text.
        is_new: bool,
        lineno: Option<u32>,
        range: Range<usize>,
        kind_bg: CellBg,
        /// Word-level changed ranges, absolute in the source text.
        word_ranges: Vec<Range<usize>>,
    },
    Separator { hidden_lines: u32, new_start: u32 },
}

#[derive(Clone, Copy, PartialEq)]
enum CellBg {
    None,
    Added,
    Deleted,
}

struct SplitRow {
    left: SplitCell,
    right: SplitCell,
}

/// Text measurements taken from the window text style, recomputed when the
/// row set changes (and cheap enough to redo every render).
#[derive(Clone, Copy)]
struct Metrics {
    font_size: Pixels,
    line_height: Pixels,
    /// Width of one line-number column (measured, editor-style).
    gutter_w: Pixels,
}

pub struct DiffView {
    path: String,
    language: String,
    repo: GitRepository,
    old_text: String,
    new_text: String,
    old_styles: Vec<(Range<usize>, HighlightStyle)>,
    new_styles: Vec<(Range<usize>, HighlightStyle)>,
    rows: Vec<DiffRow>,
    split_rows: Vec<SplitRow>,
    expanded: Vec<u32>,
    mode: DiffMode,
    additions: usize,
    deletions: usize,
    /// Measured pixel width of the widest row; `None` = needs re-measure.
    content_width: Option<Pixels>,
    refreshing: bool,
    scroll_handle: ScrollHandle,
}

impl DiffView {
    pub fn new(diff: DiffFile, repo: GitRepository, cx: &mut Context<Self>) -> Self {
        let highlight_theme = cx.theme().highlight_theme.clone();
        let (old_styles, new_styles) =
            compute_styles(&diff.language, &diff.old_text, &diff.new_text, &highlight_theme);
        let split_rows = build_split_rows(&diff.rows);

        Self {
            path: diff.path,
            language: diff.language,
            repo,
            old_text: diff.old_text,
            new_text: diff.new_text,
            old_styles,
            new_styles,
            rows: diff.rows,
            split_rows,
            expanded: Vec::new(),
            mode: DiffMode::Unified,
            additions: diff.additions,
            deletions: diff.deletions,
            content_width: None,
            refreshing: false,
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn rebuild_rows(&mut self) {
        let diff = DiffFile::build(
            self.path.clone(),
            self.language.clone(),
            self.old_text.clone(),
            self.new_text.clone(),
            &self.expanded,
        );
        self.split_rows = build_split_rows(&diff.rows);
        self.rows = diff.rows;
        self.content_width = None;
    }

    /// Re-fetch the diff from git and re-render (toolbar refresh button).
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
                    view.apply_new_content(old, new, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_new_content(&mut self, old_text: String, new_text: String, cx: &mut Context<Self>) {
        let diff = DiffFile::build(
            self.path.clone(),
            self.language.clone(),
            old_text,
            new_text,
            &self.expanded,
        );
        let highlight_theme = cx.theme().highlight_theme.clone();
        let (old_styles, new_styles) =
            compute_styles(&self.language, &diff.old_text, &diff.new_text, &highlight_theme);
        self.old_text = diff.old_text;
        self.new_text = diff.new_text;
        self.old_styles = old_styles;
        self.new_styles = new_styles;
        self.split_rows = build_split_rows(&diff.rows);
        self.rows = diff.rows;
        self.additions = diff.additions;
        self.deletions = diff.deletions;
        self.content_width = None;
    }

    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            DiffMode::Unified => DiffMode::Split,
            DiffMode::Split => DiffMode::Unified,
        };
        cx.notify();
    }

    fn expand_separator(&mut self, new_start: u32, cx: &mut Context<Self>) {
        self.expanded.push(new_start);
        self.rebuild_rows();
        cx.notify();
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

    // ---- measuring --------------------------------------------------------

    /// Measure text width exactly like the editor measures its line-number
    /// gutter: shape the text with the inherited font.
    fn measure_width(
        window: &mut Window,
        text: SharedString,
        font_size: Pixels,
        font: &Font,
    ) -> Pixels {
        let len = text.len();
        window
            .text_system()
            .shape_line(
                text,
                font_size,
                &[TextRun {
                    len,
                    font: font.clone(),
                    color: black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width
    }

    fn digits(&self) -> usize {
        let max_line = self
            .rows
            .iter()
            .map(|r| match r {
                DiffRow::Context { old_lineno, .. } => *old_lineno,
                DiffRow::Del { old_lineno, .. } => *old_lineno,
                DiffRow::Add { new_lineno, .. } => *new_lineno,
                DiffRow::Separator { .. } => 0,
            })
            .max()
            .unwrap_or(0)
            + 1;
        ((max_line.max(1).ilog10() + 1) as usize).max(2)
    }

    fn metrics(&self, window: &mut Window) -> Metrics {
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let font = text_style.font();
        let digits = self.digits();
        let gutter_w = Self::measure_width(
            window,
            "+".repeat(digits).into(),
            font_size,
            &font,
        ) + LINE_NUMBER_RIGHT_MARGIN;
        Metrics {
            font_size,
            line_height,
            gutter_w,
        }
    }

    /// Measure the widest visible line (top candidates by char count) so the
    /// scroll area gets a real horizontal extent.
    fn ensure_content_width(&mut self, m: Metrics, window: &mut Window) -> Pixels {
        if let Some(w) = self.content_width {
            return w;
        }
        let font = window.text_style().font();
        let mut candidates: Vec<String> = self
            .rows
            .iter()
            .map(|r| match r {
                DiffRow::Context { new_range, .. } | DiffRow::Add { new_range, .. } => {
                    self.new_text[new_range.clone()].to_string()
                }
                DiffRow::Del { old_range, .. } => self.old_text[old_range.clone()].to_string(),
                DiffRow::Separator { .. } => String::new(),
            })
            .collect();
        candidates.sort_by_key(|b| std::cmp::Reverse(b.len()));
        candidates.truncate(32);

        let max_line_w = candidates
            .into_iter()
            .map(|line| Self::measure_width(window, line.into(), m.font_size, &font))
            .max()
            .unwrap_or(px(0.));
        let w = m.gutter_w * 2.0 + SIGN_COLUMN_WIDTH + max_line_w + px(24.);
        self.content_width = Some(w);
        w
    }

    // ---- style slicing ----------------------------------------------------

    /// Slice full-text highlight spans down to a line range, shifting them to
    /// be relative to the line start.
    fn slice_styles(
        styles: &[(Range<usize>, HighlightStyle)],
        range: &Range<usize>,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        styles
            .iter()
            .filter_map(|(r, s)| {
                let start = r.start.max(range.start);
                let end = r.end.min(range.end);
                if end > start {
                    Some((start - range.start..end - range.start, *s))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Overlay word-level background color onto base highlight spans.
    /// `styles` must be sorted, non-overlapping, line-relative.
    fn apply_word_highlights(
        styles: Vec<(Range<usize>, HighlightStyle)>,
        word_ranges: &[Range<usize>],
        line_range: &Range<usize>,
        bg: Hsla,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if word_ranges.is_empty() {
            return styles;
        }
        let line_len = line_range.end - line_range.start;
        let mut cuts: BTreeSet<usize> = BTreeSet::new();
        cuts.insert(0);
        cuts.insert(line_len);
        for (r, _) in &styles {
            cuts.insert(r.start);
            cuts.insert(r.end);
        }
        for wr in word_ranges {
            let s = (wr.start - line_range.start).min(line_len);
            let e = (wr.end - line_range.start).min(line_len);
            if e > s {
                cuts.insert(s);
                cuts.insert(e);
            }
        }

        let points: Vec<usize> = cuts.into_iter().collect();
        let mut result = Vec::new();
        for pair in points.windows(2) {
            let (s, e) = (pair[0], pair[1]);
            if e <= s {
                continue;
            }
            let mut style = styles
                .iter()
                .find(|(r, _)| r.start <= s && r.end >= e)
                .map(|(_, st)| *st)
                .unwrap_or_default();
            let abs_s = s + line_range.start;
            let abs_e = e + line_range.start;
            if word_ranges
                .iter()
                .any(|wr| wr.start < abs_e && wr.end > abs_s)
            {
                style.background_color = Some(bg);
            }
            result.push((s..e, style));
        }
        result
    }

    fn styled_line(
        &self,
        is_new: bool,
        range: &Range<usize>,
        word_ranges: &[Range<usize>],
        word_bg: Option<Hsla>,
    ) -> StyledText {
        let (text, styles) = if is_new {
            (&self.new_text, &self.new_styles)
        } else {
            (&self.old_text, &self.old_styles)
        };
        let line_text = &text[range.clone()];
        let line_styles = Self::slice_styles(styles, range);
        let line_styles = match word_bg {
            Some(bg) => Self::apply_word_highlights(line_styles, word_ranges, range, bg),
            None => line_styles,
        };
        StyledText::new(SharedString::from(line_text.to_string())).with_highlights(line_styles)
    }

    // ---- rendering --------------------------------------------------------

    fn lineno_cell(
        lineno: Option<u32>,
        m: Metrics,
        color: Hsla,
    ) -> impl IntoElement {
        div()
            .w(m.gutter_w)
            .flex_none()
            .pr(LINE_NUMBER_RIGHT_MARGIN)
            .text_right()
            .text_color(color)
            .child(lineno.map(|n| (n + 1).to_string()).unwrap_or_default())
    }

    fn render_unified(&self, m: Metrics, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let theme = cx.theme();
        let lineno_color = theme.text_muted;
        let added_bg = Self::added_line_bg(cx);
        let deleted_bg = Self::deleted_line_bg(cx);
        let added_word = Self::added_word_bg(cx);
        let deleted_word = Self::deleted_word_bg(cx);
        let added_sign = theme.status_added;
        let deleted_sign = theme.status_deleted;

        self.rows
            .iter()
            .map(|row| match row {
                DiffRow::Context {
                    old_lineno,
                    new_lineno,
                    new_range,
                } => div()
                    .flex()
                    .w_full()
                    .h(m.line_height)
                    .child(Self::lineno_cell(Some(*old_lineno), m, lineno_color))
                    .child(Self::lineno_cell(Some(*new_lineno), m, lineno_color))
                    .child(div().w(SIGN_COLUMN_WIDTH).flex_none())
                    .child(self.styled_line(true, new_range, &[], None))
                    .into_any_element(),
                DiffRow::Add {
                    new_lineno,
                    new_range,
                    word_ranges,
                } => div()
                    .flex()
                    .w_full()
                    .h(m.line_height)
                    .bg(added_bg)
                    .child(Self::lineno_cell(None, m, lineno_color))
                    .child(Self::lineno_cell(Some(*new_lineno), m, lineno_color))
                    .child(
                        div()
                            .w(SIGN_COLUMN_WIDTH)
                            .flex_none()
                            .text_center()
                            .text_color(added_sign)
                            .child("+"),
                    )
                    .child(self.styled_line(true, new_range, word_ranges, Some(added_word)))
                    .into_any_element(),
                DiffRow::Del {
                    old_lineno,
                    old_range,
                    word_ranges,
                } => div()
                    .flex()
                    .w_full()
                    .h(m.line_height)
                    .bg(deleted_bg)
                    .child(Self::lineno_cell(Some(*old_lineno), m, lineno_color))
                    .child(Self::lineno_cell(None, m, lineno_color))
                    .child(
                        div()
                            .w(SIGN_COLUMN_WIDTH)
                            .flex_none()
                            .text_center()
                            .text_color(deleted_sign)
                            .child("-"),
                    )
                    .child(self.styled_line(false, old_range, word_ranges, Some(deleted_word)))
                    .into_any_element(),
                DiffRow::Separator {
                    hidden_lines,
                    new_start,
                    ..
                } => self
                    .render_separator(*hidden_lines, *new_start, m.line_height, cx)
                    .into_any_element(),
            })
            .collect()
    }

    fn render_separator(
        &self,
        hidden_lines: u32,
        new_start: u32,
        line_height: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id(("diff-sep", new_start as usize))
            .flex()
            .w_full()
            .h(line_height)
            .items_center()
            .justify_center()
            .bg(theme.panel_background)
            .cursor_pointer()
            .hover(|s| s.bg(theme.tab_hover_background))
            .text_color(theme.text_muted)
            .child(format!("{hidden_lines} unchanged lines — click to expand"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.expand_separator(new_start, cx);
            }))
    }

    fn render_split_cell(
        &self,
        cell: &SplitCell,
        is_right: bool,
        m: Metrics,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let lineno_color = theme.text_muted;
        match cell {
            SplitCell::Empty => div()
                .flex()
                .w_1_2()
                .h(m.line_height)
                .bg(theme.panel_background.opacity(0.5))
                .into_any_element(),
            SplitCell::Line {
                is_new,
                lineno,
                range,
                kind_bg,
                word_ranges,
            } => {
                let (line_bg, word_bg) = match kind_bg {
                    CellBg::None => (gpui::transparent_black(), None),
                    CellBg::Added => (Self::added_line_bg(cx), Some(Self::added_word_bg(cx))),
                    CellBg::Deleted => (Self::deleted_line_bg(cx), Some(Self::deleted_word_bg(cx))),
                };
                div()
                    .flex()
                    .w_1_2()
                    .h(m.line_height)
                    .overflow_hidden()
                    .bg(line_bg)
                    .child(Self::lineno_cell(*lineno, m, lineno_color))
                    .child(self.styled_line(*is_new, range, word_ranges, word_bg))
                    .into_any_element()
            }
            SplitCell::Separator { .. } if is_right => div()
                .w_1_2()
                .h(m.line_height)
                .bg(theme.panel_background)
                .into_any_element(),
            SplitCell::Separator {
                hidden_lines,
                new_start,
            } => div()
                .w_1_2()
                .h(m.line_height)
                .child(self.render_separator(*hidden_lines, *new_start, m.line_height, cx))
                .into_any_element(),
        }
    }

    fn render_split(&self, m: Metrics, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let border = cx.theme().border;
        self.split_rows
            .iter()
            .map(|row| {
                div()
                    .flex()
                    .w_full()
                    .h(m.line_height)
                    .child(self.render_split_cell(&row.left, false, m, cx))
                    .child(div().w(px(1.0)).flex_none().h_full().bg(border))
                    .child(self.render_split_cell(&row.right, true, m, cx))
                    .into_any_element()
            })
            .collect()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mode_label = match self.mode {
            DiffMode::Unified => "Split",
            DiffMode::Split => "Unified",
        };
        let refreshing = self.refreshing;
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
                div()
                    .id("diff-refresh")
                    .px(px(8.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .text_color(if refreshing {
                        theme.text_subtle
                    } else {
                        theme.text_muted
                    })
                    .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                    .child(if refreshing { "Refreshing…" } else { "Refresh" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh(cx);
                    })),
            )
            .child(
                div()
                    .id("diff-toggle-mode")
                    .px(px(8.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                    .child(mode_label)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_mode(cx);
                    })),
            )
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let editor_bg = theme.editor_background();
        let editor_fg = theme
            .highlight_theme
            .style
            .editor_foreground
            .unwrap_or(theme.text);

        let m = self.metrics(window);

        let body: gpui::AnyElement = if self.rows.is_empty() && !self.refreshing {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .text_color(theme.text_muted)
                .child("No changes")
                .into_any_element()
        } else {
            let content_w = self.ensure_content_width(m, window);
            let rows = match self.mode {
                DiffMode::Unified => self.render_unified(m, cx),
                DiffMode::Split => self.render_split(m, cx),
            };
            div()
                .id("diff-scroll")
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .track_scroll(&self.scroll_handle)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_full()
                        .w(content_w)
                        .whitespace_nowrap()
                        .children(rows),
                )
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(editor_bg)
            .text_color(editor_fg)
            .child(self.render_toolbar(cx))
            .child(body)
    }
}

/// Highlight old/new texts with the file's language, mirroring how zed keeps
/// the diff base in a same-language buffer.
fn compute_styles(
    language: &str,
    old_text: &str,
    new_text: &str,
    theme: &HighlightTheme,
) -> (
    Vec<(Range<usize>, HighlightStyle)>,
    Vec<(Range<usize>, HighlightStyle)>,
) {
    let mut old_highlighter = SyntaxHighlighter::new(language);
    let old_rope = ropey::Rope::from_str(old_text);
    old_highlighter.update(None, &old_rope, None);
    let old_styles = if old_text.is_empty() {
        Vec::new()
    } else {
        old_highlighter.styles(&(0..old_text.len()), theme)
    };

    let mut new_highlighter = SyntaxHighlighter::new(language);
    let new_rope = ropey::Rope::from_str(new_text);
    new_highlighter.update(None, &new_rope, None);
    let new_styles = if new_text.is_empty() {
        Vec::new()
    } else {
        new_highlighter.styles(&(0..new_text.len()), theme)
    };

    (old_styles, new_styles)
}

/// Pair deleted/added line runs into aligned side-by-side rows.
fn build_split_rows(rows: &[DiffRow]) -> Vec<SplitRow> {
    let mut split = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        match &rows[i] {
            DiffRow::Context {
                old_lineno,
                new_lineno,
                new_range,
            } => {
                split.push(SplitRow {
                    left: SplitCell::Line {
                        is_new: true,
                        lineno: Some(*old_lineno),
                        range: new_range.clone(),
                        kind_bg: CellBg::None,
                        word_ranges: Vec::new(),
                    },
                    right: SplitCell::Line {
                        is_new: true,
                        lineno: Some(*new_lineno),
                        range: new_range.clone(),
                        kind_bg: CellBg::None,
                        word_ranges: Vec::new(),
                    },
                });
                i += 1;
            }
            DiffRow::Separator {
                hidden_lines,
                new_start,
                ..
            } => {
                split.push(SplitRow {
                    left: SplitCell::Separator {
                        hidden_lines: *hidden_lines,
                        new_start: *new_start,
                    },
                    right: SplitCell::Separator {
                        hidden_lines: *hidden_lines,
                        new_start: *new_start,
                    },
                });
                i += 1;
            }
            DiffRow::Del { .. } => {
                // Collect the del block and the add block that follows it.
                let mut dels = Vec::new();
                while i < rows.len() {
                    if let DiffRow::Del {
                        old_lineno,
                        old_range,
                        word_ranges,
                    } = &rows[i]
                    {
                        dels.push(SplitCell::Line {
                            is_new: false,
                            lineno: Some(*old_lineno),
                            range: old_range.clone(),
                            kind_bg: CellBg::Deleted,
                            word_ranges: word_ranges.clone(),
                        });
                        i += 1;
                    } else {
                        break;
                    }
                }
                let mut adds = Vec::new();
                while i < rows.len() {
                    if let DiffRow::Add {
                        new_lineno,
                        new_range,
                        word_ranges,
                    } = &rows[i]
                    {
                        adds.push(SplitCell::Line {
                            is_new: true,
                            lineno: Some(*new_lineno),
                            range: new_range.clone(),
                            kind_bg: CellBg::Added,
                            word_ranges: word_ranges.clone(),
                        });
                        i += 1;
                    } else {
                        break;
                    }
                }
                let n = dels.len().max(adds.len());
                for k in 0..n {
                    split.push(SplitRow {
                        left: dels.get(k).cloned().unwrap_or(SplitCell::Empty),
                        right: adds.get(k).cloned().unwrap_or(SplitCell::Empty),
                    });
                }
            }
            DiffRow::Add { .. } => {
                // Add block without preceding dels (defensive).
                let mut adds = Vec::new();
                while i < rows.len() {
                    if let DiffRow::Add {
                        new_lineno,
                        new_range,
                        word_ranges,
                    } = &rows[i]
                    {
                        adds.push(SplitCell::Line {
                            is_new: true,
                            lineno: Some(*new_lineno),
                            range: new_range.clone(),
                            kind_bg: CellBg::Added,
                            word_ranges: word_ranges.clone(),
                        });
                        i += 1;
                    } else {
                        break;
                    }
                }
                for cell in adds {
                    split.push(SplitRow {
                        left: SplitCell::Empty,
                        right: cell,
                    });
                }
            }
        }
    }
    split
}
