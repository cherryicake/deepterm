//! Row rendering helpers: `RowPlan` → `TextRun`s + resolved colors.
//!
//! Rewritten for deepterm's GPUI fork. Styling follows guise semantics
//! (markers dimmed, checked tasks dim, links accented, code mono) but every
//! token comes from [`crate::editor::markdown::MarkdownStyle`] / `UiTheme`
//! so dark/light stays consistent with the read-only preview.

use gpui::prelude::*;
use gpui::{
    App, Bounds, DispatchPhase, Element, ElementId, Entity, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, ShapedLine, SharedString, StyledText, TextRun, TextStyle,
    TextStyleRefinement, Window,
};

use crate::components::input::cursor::{CursorLayout, CursorShape};

use crate::editor::markdown::MarkdownStyle;
use crate::editor::markdown_live::highlight::HighlightCache;
use crate::editor::markdown_live::layout::{RowKind, RowPlan};
use crate::editor::markdown_live::view::LiveMarkdownView;
use crate::theme::AppTheme;

/// Resolved row colors for one render pass.
///
/// Values mirror the read-only preview (`markdown_element.rs`) so the live
/// editor renders pixel-consistent chrome: task green, quote border, code
/// border, table dividers, terminal-consistent selection.
#[derive(Debug, Clone)]
pub struct LiveColors {
    pub text: Hsla,
    pub marker: Hsla,
    pub dim: Hsla,
    pub accent: Hsla,
    pub caret: Hsla,
    pub selection: Hsla,
    pub code_bg: Hsla,
    pub highlight_bg: Hsla,
    pub rule: Hsla,
    pub quote_bar: Hsla,
    pub code_border: Hsla,
    pub success: Hsla,
    pub elevated_bg: Hsla,
    pub panel_bg: Hsla,
    pub table_border: Hsla,
}

impl LiveColors {
    pub fn resolve(style: &MarkdownStyle, theme: &AppTheme) -> Self {
        let ui = &theme.ui;
        let is_dark = theme.appearance.is_dark();
        // Same fallback as `SelectableText::selection_color`: terminal
        // selection first, ui.selection at reduced opacity otherwise.
        let selection = if theme.terminal.selection_background.a > 0.01 {
            theme.terminal.selection_background
        } else {
            ui.selection.opacity(0.35)
        };
        Self {
            text: ui.text,
            marker: ui.text_muted,
            dim: ui.text_muted,
            accent: ui.accent,
            caret: ui.accent,
            selection,
            code_bg: theme.editor_background(),
            highlight_bg: ui.warning.opacity(if is_dark { 0.30 } else { 0.22 }),
            rule: style.rule_color,
            quote_bar: style.block_quote_border_color,
            code_border: ui.border.opacity(if is_dark { 0.28 } else { 0.20 }),
            success: ui.success,
            elevated_bg: ui.elevated_background,
            panel_bg: ui.panel_background,
            table_border: ui.border.opacity(if is_dark { 0.28 } else { 0.22 }),
        }
    }
}

/// Base `TextStyle` for a row: body + heading/code/quote refinements.
pub fn row_base_style(row: &RowPlan, style: &MarkdownStyle) -> TextStyle {
    let mut base = style.base_text_style.clone();
    match &row.kind {
        RowKind::Heading(level) => {
            if let Some(r) = style
                .heading_level_styles
                .get((*level).min(6).saturating_sub(1) as usize)
                .and_then(|r| r.as_ref())
            {
                base.refine(r);
            }
        }
        RowKind::Code { .. } | RowKind::Fence { .. } => {
            base.refine(&TextStyleRefinement {
                font_size: Some(gpui::px(13.5).into()),
                line_height: Some(gpui::rems(1.6).into()),
                white_space: Some(gpui::WhiteSpace::Nowrap),
                ..Default::default()
            });
            base.refine(&style.inline_code);
            // Whole-line code must not get the inline capsule background.
            base.background_color = None;
        }
        RowKind::Table | RowKind::FrontMatter => {
            base.refine(&style.inline_code);
            base.background_color = None;
        }
        RowKind::Quote { .. } => {
            base.refine(&style.block_quote);
        }
        _ => {}
    }
    base
}

/// `TextRun`s for a row's `visible` text.
///
/// Code rows with a language go through tree-sitter (`HighlightCache`);
/// everything else maps `RunAttrs` → refined styles: marker/dim → muted,
/// link → accent + underline, inline code → mono capsule, highlight →
/// tinted bg, strike → strikethrough, bold/heading → BOLD, italic → italic.
pub fn runs_for_row(
    row: &RowPlan,
    base: &TextStyle,
    colors: &LiveColors,
    style: &MarkdownStyle,
    hl: &HighlightCache,
    theme: &AppTheme,
) -> Vec<TextRun> {
    if let RowKind::Code { lang: Some(lang) } = &row.kind {
        if !row.visible.is_empty() {
            return hl.runs_for(lang, &row.visible, base, theme);
        }
    }
    let mono_block = matches!(
        row.kind,
        RowKind::Code { .. } | RowKind::Fence { .. } | RowKind::Table | RowKind::FrontMatter
    );
    let mut runs = Vec::with_capacity(row.runs.len().max(1));
    let mut at = 0usize;
    for r in &row.runs {
        let mut s = base.clone();
        let st = &r.style;
        if r.marker || r.dim {
            s.refine(&TextStyleRefinement {
                color: Some(if r.marker { colors.marker } else { colors.dim }),
                ..Default::default()
            });
        }
        if st.link {
            s.refine(&style.link);
        }
        if st.code && !mono_block {
            s.refine(&style.inline_code);
        }
        if st.highlight {
            s.refine(&TextStyleRefinement {
                background_color: Some(colors.highlight_bg),
                ..Default::default()
            });
        }
        if st.strike {
            s.refine(&TextStyleRefinement {
                strikethrough: Some(gpui::StrikethroughStyle {
                    thickness: gpui::px(1.),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        if st.bold || matches!(row.kind, RowKind::Heading(_)) {
            s.refine(&TextStyleRefinement {
                font_weight: Some(gpui::FontWeight::BOLD),
                ..Default::default()
            });
        }
        if st.italic {
            s.refine(&TextStyleRefinement {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            });
        }
        runs.push(s.to_run(r.len));
        at += r.len;
    }
    if at < row.visible.len() {
        runs.push(base.to_run(row.visible.len() - at));
    }
    if runs.is_empty() && !row.visible.is_empty() {
        runs.push(base.to_run(row.visible.len()));
    }
    runs
}

/// Apply the completed-task style (preview parity): muted text +
/// strikethrough, like `markdown_element.rs` pushes for checked items.
pub fn apply_task_done(mut runs: Vec<TextRun>, dim: Hsla, strike_color: Hsla) -> Vec<TextRun> {
    for run in &mut runs {
        run.color = dim;
        run.strikethrough = Some(gpui::StrikethroughStyle {
            color: Some(strike_color),
            thickness: gpui::px(1.),
        });
    }
    runs
}

/// Inline-styled runs for a table cell's raw text (no markers/dim: cells
/// render links, code capsules, emphasis like the preview's grid).
pub fn inline_cell_runs(
    text: &str,
    base: &TextStyle,
    colors: &LiveColors,
    style: &MarkdownStyle,
) -> Vec<TextRun> {
    use crate::editor::markdown_live::inline::parse as parse_inline;
    let inline = parse_inline(text);
    let mut runs = Vec::new();
    for span in &inline.spans {
        // Markers stay visible inside table source cells (tables edit as
        // source when revealed; the grid shows rendered text).
        let mut s = base.clone();
        let st = &span.style;
        if st.link {
            s.refine(&style.link);
        }
        if st.code {
            s.refine(&style.inline_code);
        }
        if st.highlight {
            s.refine(&TextStyleRefinement {
                background_color: Some(colors.highlight_bg),
                ..Default::default()
            });
        }
        if st.strike {
            s.refine(&TextStyleRefinement {
                strikethrough: Some(gpui::StrikethroughStyle {
                    color: None,
                    thickness: gpui::px(1.),
                }),
                ..Default::default()
            });
        }
        if st.bold {
            s.refine(&TextStyleRefinement {
                font_weight: Some(gpui::FontWeight::BOLD),
                ..Default::default()
            });
        }
        if st.italic {
            s.refine(&TextStyleRefinement {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            });
        }
        let len = span.range.len();
        if len > 0 {
            runs.push(s.to_run(len));
        }
    }
    if runs.is_empty() && !text.is_empty() {
        runs.push(base.to_run(text.len()));
    }
    runs
}

/// Overlay a visible-byte selection range onto runs (splits runs, tints the
/// middle with `selection` background). `vsel` is `None` when the row has no
/// selection.
pub fn apply_selection(
    runs: Vec<TextRun>,
    visible_len: usize,
    vsel: Option<(usize, usize)>,
    selection: Hsla,
) -> Vec<TextRun> {
    let Some((s, e)) = vsel else {
        return runs;
    };
    let (s, e) = (s.min(visible_len), e.min(visible_len));
    if s >= e || runs.is_empty() {
        return runs;
    }
    let mut cuts = vec![0, s, e, visible_len];
    cuts.sort_unstable();
    cuts.dedup();
    let mut result: Vec<TextRun> = Vec::new();
    let mut templates = runs.into_iter();
    let mut tpl = templates.next();
    let mut t_left = tpl.as_ref().map(|r| r.len).unwrap_or(0);
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let in_sel = a >= s && b <= e;
        let mut rest = b - a;
        while rest > 0 {
            let tpl_ref = tpl.as_ref().expect("runs must cover visible");
            let take = rest.min(t_left);
            let mut run = tpl_ref.clone();
            run.len = take;
            if in_sel {
                run.background_color = Some(selection);
            }
            match result.last_mut() {
                Some(last)
                    if last.background_color == run.background_color
                        && last.color == run.color
                        && last.font == run.font
                        && last.underline == run.underline
                        && last.strikethrough == run.strikethrough =>
                {
                    last.len += take
                }
                _ => result.push(run),
            }
            t_left -= take;
            rest -= take;
            if t_left == 0 {
                tpl = templates.next();
                t_left = tpl.as_ref().map(|r| r.len).unwrap_or(0);
            }
        }
    }
    // Any trailing template bytes (rounding) pass through untouched.
    while let Some(t) = tpl.take() {
        if t_left > 0 {
            let mut run = t.clone();
            run.len = t_left;
            result.push(run);
        }
        tpl = templates.next();
        t_left = tpl.as_ref().map(|r| r.len).unwrap_or(0);
    }
    result
}

// ---- interactive line element ---------------------------------------------

/// One rendered source line: `StyledText` + hitbox + caret + mouse gestures.
///
/// Created fresh every frame by [`LiveMarkdownView`](super::view::LiveMarkdownView);
/// all editing state lives in the view entity. Mirrors `SelectableText`'s
/// three-phase pattern (`request_layout`/`prepaint`/`paint`).
pub struct LiveLine {
    id: ElementId,
    pub text: SharedString,
    pub runs: Vec<TextRun>,
    pub line: usize,
    pub view: Entity<LiveMarkdownView>,
    /// True when the row has an interactive gutter on the left (task
    /// checkbox). Clicks left of the text are ignored then, so they never
    /// fight the checkbox toggle; otherwise they land at line start.
    pub gutter: bool,
    /// Visible byte offset of the caret when the cursor is on this line.
    pub caret_vis: Option<usize>,
    pub caret_color: Hsla,
    pub caret_width: Pixels,
    pub caret_shape: CursorShape,
    pub caret_block_width: Pixels,
    pub caret_block_text: Option<ShapedLine>,
    pub line_height: Pixels,
    /// Set on the first rendered row only: its drag handler also covers
    /// positions outside every row's band (blank rows, margins), extending
    /// the selection to the nearest line — guise's continuous drag.
    pub drag_fallback: bool,
}

impl LiveLine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<ElementId>,
        text: SharedString,
        runs: Vec<TextRun>,
        line: usize,
        view: Entity<LiveMarkdownView>,
        gutter: bool,
        caret_vis: Option<usize>,
        caret_color: Hsla,
        caret_width: Pixels,
        line_height: Pixels,
    ) -> Self {
        Self {
            caret_shape: CursorShape::Bar,
            caret_block_width: caret_width,
            caret_block_text: None,
            id: id.into(),
            text,
            runs,
            line,
            view,
            gutter,
            caret_vis,
            caret_color,
            caret_width,
            line_height,
            drag_fallback: false,
        }
    }

    /// Enable the dead-zone drag fallback for this row (see field docs).
    pub fn with_drag_fallback(mut self, on: bool) -> Self {
        self.drag_fallback = on;
        self
    }

    pub fn with_caret_shape(mut self, shape: CursorShape) -> Self {
        self.caret_shape = shape;
        self
    }

    pub fn with_caret_block(
        mut self,
        block_width: Pixels,
        block_text: Option<ShapedLine>,
    ) -> Self {
        self.caret_block_width = block_width;
        self.caret_block_text = block_text;
        self
    }
}

impl Element for LiveLine {
    type RequestLayoutState = StyledText;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    crate::impl_source_location_none!();

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let _ = cx;
        let mut styled = StyledText::new(self.text.clone()).with_runs(self.runs.clone());
        let (layout_id, _) = styled.request_layout(_id, _inspector_id, window, cx);
        (layout_id, styled)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        styled: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let _ = cx;
        styled.prepaint(_id, _inspector_id, bounds, &mut (), window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        styled: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        styled.paint(_id, _inspector_id, bounds, &mut (), &mut (), window, cx);
        window.set_cursor_style(gpui::CursorStyle::IBeam, hitbox);

        #[cfg(test)]
        record_paint(self.line, self.text.len());

        let layout = styled.layout().clone();
        let text_len = self.text.len();

        // Record this row's layout for the view's visual-row movement and
        // dead-zone hit fallbacks (plain field write; no notify).
        {
            let view = self.view.clone();
            let layout = layout.clone();
            view.update(cx, |v, _| {
                v.record_row_layout(self.line, layout, text_len);
            });
        }

        // Caret: `position_for_index` already returns window coordinates
        // (it folds `bounds.origin` in), so paint at `pos` directly —
        // adding `bounds.origin` again would shift the caret down-right.
        if let Some(vis) = self.caret_vis {
            let pos = layout
                .position_for_index(vis.min(text_len))
                // Empty text has no glyphs to resolve: caret sits at origin.
                .or_else(|| (text_len == 0).then_some(bounds.origin));
            if let Some(pos) = pos {
                let block_width = if matches!(self.caret_shape, CursorShape::Bar) {
                    self.caret_width
                } else {
                    self.caret_block_width.max(self.caret_width)
                };
                let mut caret = CursorLayout::new(
                    pos,
                    block_width,
                    self.line_height,
                    self.caret_color,
                    self.caret_shape,
                    self.caret_block_text.clone(),
                );
                caret.paint(window, cx);
            }
        }

        let line = self.line;
        let view = self.view.clone();
        let gutter = self.gutter;
        let layout_down = layout.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != gpui::MouseButton::Left {
                return;
            }
            // Claim clicks anywhere on this row's horizontal band so the
            // cursor can land past end-of-line on short lines — not just on
            // glyphs. Left of the text maps to line start, except on task
            // rows where the checkbox gutter owns that space.
            if event.position.y < bounds.top() || event.position.y > bounds.bottom() {
                return;
            }
            let raw = if event.position.x >= bounds.right() {
                // Empty area past EOL → line end.
                text_len
            } else if event.position.x < bounds.left() {
                if gutter {
                    // Interactive gutter (task checkbox) owns this space.
                    return;
                }
                // Margin/padding left of plain text → line start.
                0
            } else {
                match layout_down.index_for_position(event.position) {
                    Ok(ix) | Err(ix) => ix.min(text_len),
                }
            };
            let shift = event.modifiers.shift;
            let platform = event.modifiers.platform;
            view.update(cx, |v, cx| {
                v.on_line_mouse_down(line, raw, event.click_count, shift, platform, window, cx)
            });
        });

        let view = self.view.clone();
        let hitbox_move = hitbox.clone();
        let layout_move = layout.clone();
        let drag_fallback = self.drag_fallback;
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            if !matches!(event.pressed_button, Some(gpui::MouseButton::Left)) {
                return;
            }
            if bounds.top() > event.position.y || event.position.y > bounds.bottom() {
                // Out of this row's band: the designated fallback row maps
                // the drag to the nearest line (blank rows, margins).
                if drag_fallback {
                    let position = event.position;
                    view.update(cx, |v, cx| v.on_fallback_drag(position, cx));
                }
                return;
            }
            let _ = hitbox_move;
            let raw = match layout_move.index_for_position(event.position) {
                Ok(ix) | Err(ix) => ix.min(text_len),
            };
            view.update(cx, |v, cx| v.on_line_drag(line, raw, window, cx));
        });

        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            if event.button != gpui::MouseButton::Left {
                return;
            }
            view.update(cx, |v, cx| v.on_line_mouse_up(cx));
        });
    }
}

impl IntoElement for LiveLine {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Test observability: records `(source line, visible bytes)` for every
/// non-empty `LiveLine` paint, so frame tests can assert a row really painted
/// (layout existing is not enough — invisible paint is the failure mode).
#[cfg(test)]
pub static LIVE_LINE_PAINT_LOG: std::sync::Mutex<Vec<(usize, usize)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn record_paint(line: usize, visible_len: usize) {
    if visible_len > 0 {
        LIVE_LINE_PAINT_LOG.lock().unwrap().push((line, visible_len));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_overlay_splits_runs() {
        let mk = |len: usize| TextRun {
            len,
            ..Default::default()
        };
        let runs = vec![mk(5), mk(5)];
        let sel = Hsla::default();
        let out = apply_selection(runs, 10, Some((3, 7)), sel);
        let total: usize = out.iter().map(|r| r.len).sum();
        assert_eq!(total, 10);
        assert_eq!(out.len(), 3);
        assert!(out[0].background_color.is_none());
        assert_eq!(out[1].background_color, Some(sel));
        assert!(out[2].background_color.is_none());
        // No selection → untouched.
        let runs = vec![mk(4)];
        assert_eq!(apply_selection(runs, 4, None, sel).len(), 1);
        // Empty range → untouched.
        let runs = vec![mk(4)];
        assert_eq!(apply_selection(runs, 4, Some((2, 2)), sel).len(), 1);
    }
}
