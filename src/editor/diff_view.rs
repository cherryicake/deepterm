//! Read-only git diff view, modeled after zed / VS Code.
//!
//! - Unified and side-by-side layouts, switchable from the toolbar.
//! - Code is syntax highlighted with the file's own language via
//!   `SimpleHighlighter` (old and new text are highlighted independently,
//!   like zed's base/buffer pair).
//! - Line backgrounds + word-level inline highlights use colors derived from
//!   the theme's git status colors.

use std::collections::BTreeSet;
use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    App, Context, Hsla, HighlightStyle, Render, ScrollHandle, SharedString, StyledText, Window,
    div, px,
};

use crate::sidebar_view::font_settings::platform_default_font_family;
use crate::syntax::SyntaxHighlighter;
use crate::theme::ActiveTheme;

use super::diff_model::{DiffFile, DiffRow};

const LINE_HEIGHT: f32 = 20.0;
const FONT_SIZE: f32 = 12.0;
/// Approximate advance of a monospace glyph at `FONT_SIZE`, used to estimate
/// the scrollable content width.
const CHAR_ADVANCE: f32 = FONT_SIZE * 0.62;

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

pub struct DiffView {
    path: String,
    language: String,
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
    /// Estimated pixel width of the widest row (for horizontal scrolling).
    content_width: f32,
    scroll_handle: ScrollHandle,
}

impl DiffView {
    pub fn new(diff: DiffFile, cx: &mut Context<Self>) -> Self {
        let highlight_theme = cx.theme().highlight_theme.clone();

        let mut old_highlighter = SyntaxHighlighter::new(&diff.language);
        let old_rope = ropey::Rope::from_str(&diff.old_text);
        old_highlighter.update(None, &old_rope, None);
        let old_styles = if diff.old_text.is_empty() {
            Vec::new()
        } else {
            old_highlighter.styles(&(0..diff.old_text.len()), &highlight_theme)
        };

        let mut new_highlighter = SyntaxHighlighter::new(&diff.language);
        let new_rope = ropey::Rope::from_str(&diff.new_text);
        new_highlighter.update(None, &new_rope, None);
        let new_styles = if diff.new_text.is_empty() {
            Vec::new()
        } else {
            new_highlighter.styles(&(0..diff.new_text.len()), &highlight_theme)
        };

        let split_rows = build_split_rows(&diff.rows);
        let content_width = estimate_content_width(&diff, gutter_width_for(&diff.rows));

        Self {
            path: diff.path,
            language: diff.language,
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
            content_width,
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
        self.content_width = estimate_content_width(&diff, gutter_width_for(&diff.rows));
        self.split_rows = build_split_rows(&diff.rows);
        self.rows = diff.rows;
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

    fn gutter_width(&self) -> gpui::Pixels {
        px(gutter_width_for(&self.rows))
    }

    fn lineno_cell(lineno: Option<u32>, width: gpui::Pixels, color: Hsla) -> impl IntoElement {
        div()
            .w(width)
            .flex_none()
            .pr(px(8.0))
            .text_right()
            .text_color(color)
            .child(lineno.map(|n| (n + 1).to_string()).unwrap_or_default())
    }

    fn render_unified(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let theme = cx.theme();
        let gutter_w = self.gutter_width();
        let lineno_color = theme.text_subtle;
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
                    .h(px(LINE_HEIGHT))
                    .child(Self::lineno_cell(Some(*old_lineno), gutter_w, lineno_color))
                    .child(Self::lineno_cell(Some(*new_lineno), gutter_w, lineno_color))
                    .child(div().w(px(16.0)).flex_none())
                    .child(self.styled_line(true, new_range, &[], None))
                    .into_any_element(),
                DiffRow::Add {
                    new_lineno,
                    new_range,
                    word_ranges,
                } => div()
                    .flex()
                    .w_full()
                    .h(px(LINE_HEIGHT))
                    .bg(added_bg)
                    .child(Self::lineno_cell(None, gutter_w, lineno_color))
                    .child(Self::lineno_cell(Some(*new_lineno), gutter_w, lineno_color))
                    .child(
                        div()
                            .w(px(16.0))
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
                    .h(px(LINE_HEIGHT))
                    .bg(deleted_bg)
                    .child(Self::lineno_cell(Some(*old_lineno), gutter_w, lineno_color))
                    .child(Self::lineno_cell(None, gutter_w, lineno_color))
                    .child(
                        div()
                            .w(px(16.0))
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
                    .render_separator(*hidden_lines, *new_start, cx)
                    .into_any_element(),
            })
            .collect()
    }

    fn render_separator(
        &self,
        hidden_lines: u32,
        new_start: u32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id(("diff-sep", new_start as usize))
            .flex()
            .w_full()
            .h(px(LINE_HEIGHT))
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
        gutter_w: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let lineno_color = theme.text_subtle;
        match cell {
            SplitCell::Empty => div()
                .flex()
                .w_1_2()
                .h(px(LINE_HEIGHT))
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
                    CellBg::None => (gpui::transparent_black().into(), None),
                    CellBg::Added => (Self::added_line_bg(cx), Some(Self::added_word_bg(cx))),
                    CellBg::Deleted => (Self::deleted_line_bg(cx), Some(Self::deleted_word_bg(cx))),
                };
                div()
                    .flex()
                    .w_1_2()
                    .h(px(LINE_HEIGHT))
                    .overflow_hidden()
                    .bg(line_bg)
                    .child(Self::lineno_cell(*lineno, gutter_w, lineno_color))
                    .child(self.styled_line(*is_new, range, word_ranges, word_bg))
                    .into_any_element()
            }
            SplitCell::Separator { .. } if is_right => div()
                .w_1_2()
                .h(px(LINE_HEIGHT))
                .bg(theme.panel_background)
                .into_any_element(),
            SplitCell::Separator {
                hidden_lines,
                new_start,
            } => div()
                .w_1_2()
                .h(px(LINE_HEIGHT))
                .child(self.render_separator(*hidden_lines, *new_start, cx))
                .into_any_element(),
        }
    }

    fn render_split(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let gutter_w = self.gutter_width();
        let border = cx.theme().border;
        self.split_rows
            .iter()
            .map(|row| {
                div()
                    .flex()
                    .w_full()
                    .h(px(LINE_HEIGHT))
                    .child(self.render_split_cell(&row.left, false, gutter_w, cx))
                    .child(div().w(px(1.0)).flex_none().h_full().bg(border))
                    .child(self.render_split_cell(&row.right, true, gutter_w, cx))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let editor_bg = theme.editor_background();

        let rows = match self.mode {
            DiffMode::Unified => self.render_unified(cx),
            DiffMode::Split => self.render_split(cx),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(editor_bg)
            .font_family(platform_default_font_family())
            .text_size(px(FONT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .child(self.render_toolbar(cx))
            .child(
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
                            .w(px(self.content_width))
                            .whitespace_nowrap()
                            .children(rows),
                    ),
            )
    }
}

/// Gutter (line number column) width in px, derived from the max line number.
fn gutter_width_for(rows: &[DiffRow]) -> f32 {
    let max_line = rows
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
    let digits = max_line.max(1).ilog10() + 1;
    10.0 + 7.0 * digits.max(2) as f32
}

/// Estimate the scrollable content width: gutters + sign column + widest
/// line. Monospace advance is approximated; wide chars count double.
fn estimate_content_width(diff: &DiffFile, gutter_w: f32) -> f32 {
    let display_len = |text: &str, range: &Range<usize>| -> usize {
        text[range.clone()]
            .chars()
            .map(|c| if c.len_utf8() > 2 { 2 } else { 1 })
            .sum()
    };
    let max_chars = diff
        .rows
        .iter()
        .map(|r| match r {
            DiffRow::Context { new_range, .. } | DiffRow::Add { new_range, .. } => {
                display_len(&diff.new_text, new_range)
            }
            DiffRow::Del { old_range, .. } => display_len(&diff.old_text, old_range),
            DiffRow::Separator { .. } => 40,
        })
        .max()
        .unwrap_or(0);
    gutter_w * 2.0 + 16.0 + max_chars as f32 * CHAR_ADVANCE + 24.0
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
                // Add block without preceding dels (shouldn't happen after the
                // del branch, but handle defensively).
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
