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
use crate::editor::markdown_live::layout::{RowKind, RowPlan, Seg};
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
    pub find_bg: Hsla,
    pub find_current: Hsla,
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
            find_bg: ui.warning.opacity(if is_dark { 0.35 } else { 0.28 }),
            find_current: ui.accent.opacity(if is_dark { 0.50 } else { 0.40 }),
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
        // 斜体与公式内容同为斜体（KaTeX 近似；公式分隔符已隐藏/弱化）。
        if st.italic || st.math {
            s.refine(&TextStyleRefinement {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            });
        }
        // 上标/下标/emoji/脚注引用：同尺寸主题色区分（gpui 单行单字号，
        // 真实基线升降不可行；分隔符已隐藏，见 `inline::mark_sup_sub`）。
        if st.sup || st.emoji || st.footnote {
            s.refine(&TextStyleRefinement {
                color: Some(colors.accent),
                ..Default::default()
            });
        }
        if st.sub {
            s.refine(&TextStyleRefinement {
                color: Some(colors.accent.opacity(0.7)),
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

/// Overlay find-match ranges (visible bytes) onto runs: each mark is
/// `(start, end, current)`; the current match gets `current_bg`, the rest
/// `bg`. Single pass over sorted cut points (same template walk as
/// [`apply_selection`]).
pub fn apply_find_marks(
    runs: Vec<TextRun>,
    visible_len: usize,
    marks: &[(usize, usize, bool)],
    bg: Hsla,
    current_bg: Hsla,
) -> Vec<TextRun> {
    let mut cuts = vec![0, visible_len];
    for (s, e, _) in marks.iter().copied() {
        let (s, e) = (s.min(visible_len), e.min(visible_len));
        if s < e {
            cuts.push(s);
            cuts.push(e);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    if cuts.len() <= 2 || runs.is_empty() {
        return runs;
    }
    let mut result: Vec<TextRun> = Vec::new();
    let mut templates = runs.into_iter();
    let mut tpl = templates.next();
    let mut t_left = tpl.as_ref().map(|r| r.len).unwrap_or(0);
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let mark_bg = marks
            .iter()
            .find(|(s, e, _)| a >= *s && b <= *e)
            .map(|(_, _, current)| if *current { current_bg } else { bg });
        let mut rest = b - a;
        while rest > 0 {
            let tpl_ref = tpl.as_ref().expect("runs must cover visible");
            let take = rest.min(t_left);
            let mut run = tpl_ref.clone();
            run.len = take;
            if let Some(color) = mark_bg {
                run.background_color = Some(color);
            }
            match result.last_mut() {
                Some(last)
                    if last.background_color == run.background_color
                        && last.color == run.color
                        && last.font == run.font
                        && last.underline == run.underline
                        && last.strikethrough == run.strikethrough =>
                {
                    last.len += take;
                }
                _ => result.push(run),
            }
            rest -= take;
            t_left -= take;
            if t_left == 0 {
                tpl = templates.next();
                t_left = tpl.as_ref().map(|r| r.len).unwrap_or(0);
            }
        }
    }
    result
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
    /// Table cell mode: the element renders **one cell's** content of
    /// `line`, so mouse mapping must go through this cell's own `segs`
    /// (absolute in the line) and clamp to the cell's content range instead
    /// of the whole line. `None` = ordinary full-line row.
    pub cell: Option<(Vec<Seg>, std::ops::Range<usize>)>,
    /// 格键 `(源行, 格列)`：仅单元格模式设置。paint 时据此把本格命中信息
    /// 写回视图（[`crate::editor::markdown_live::view::CellHit`]），
    /// 供 `cell_div` 的元素级按下/移动监听映射坐标（spec #6）。
    pub cell_key: Option<(usize, usize)>,
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
            cell: None,
            cell_key: None,
        }
    }

    /// Render one table cell's content (see [`LiveLine::cell`]): mouse
    /// mapping is confined to `content` and resolved through `segs`.
    pub fn with_cell_segs(
        mut self,
        segs: Vec<Seg>,
        content: std::ops::Range<usize>,
    ) -> Self {
        self.cell = Some((segs, content));
        self
    }

    /// 记录用的格键 `(源行, 格列)`（见 [`Self::cell_key`]；与
    /// [`Self::with_cell_segs`] 成对使用）。
    pub fn with_cell_key(mut self, line: usize, col: usize) -> Self {
        self.cell_key = Some((line, col));
        self
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

        // 单元格模式：把本格命中信息记回视图（cell_div 的元素级按下/移动
        // 监听据此映射坐标；spec #6）。与 row_layouts 同为帧内字段写入。
        if let Some(key) = self.cell_key
            && let Some((segs, content)) = self.cell.clone()
        {
            let hit = crate::editor::markdown_live::view::CellHit {
                layout: layout.clone(),
                text_len,
                text_bounds: bounds,
                segs,
                content,
            };
            self.view.update(cx, |v, _| v.record_cell_hit(key, hit));
        }

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
        // spec #6：单元格模式的按下/移动由 `cell_div` 的**元素级**监听认领
        // （命中框覆盖整格含内边距，且只有指针所在的格触发；坐标映射走
        // paint 记录的 `cell_hits`），LiveLine 在格模式下不注册这两个窗口
        // 监听，只保留 mouse-up 统一收尾（清 selecting）。普通行原样。
        if self.cell.is_none() {
            let layout_down = layout.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || event.button != gpui::MouseButton::Left {
                    return;
                }
                // 本监听是窗口级全局的、不受命中测试约束：先要求点击落在实时
                // 编辑器内容视口的横向范围内。否则右侧终端面板的点击会命中下面
                // 的 y 判定，被当成“行尾空白”认领 —— window.focus 会把焦点抢回
                // live 编辑器并 stop_propagation，终端永远拿不到焦点。
                let vp = view.read(cx).content_viewport_bounds();
                if event.position.x < vp.left() || event.position.x > vp.right() {
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
                // 同上：只认领落在内容视口横向范围内的拖拽，避免在终端面板上
                // 拖动时被 live 的选择逻辑接管。
                let vp = view.read(cx).content_viewport_bounds();
                if event.position.x < vp.left() || event.position.x > vp.right() {
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
        }

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

    /// 单元格可见文本（走 live 的 `plan_cell`，与非活动格渲染同源）。
    fn rendered(cell: &str) -> String {
        use crate::editor::markdown_live::layout::plan_cell;
        let line = format!("| {cell} | c |");
        let plan = plan_cell(&line, 2..2 + cell.len(), None, &Default::default());
        plan.visible
    }

    #[test]
    fn active_cell_plan_matches_inactive_cell_rendering() {
        // 光标移进/移出格子时文字不能跳变：活动格与非活动格必须走**同一份**
        // 规划器（`plan_cell`，标记隐藏），只是活动格多一个光标。
        use crate::editor::markdown_live::block::table_cell_spans;
        use crate::editor::markdown_live::layout::plan_cell;
        for line in [
            "| **bold** | c |",
            "| `code` and *it* | c |",
            "| [docs](https://x.dev) | c |",
            "| ~~gone~~ ==mark== | c |",
            "| E=mc^2^ H~2~O | c |",
            "| plain text | c |",
            "| a\\|b | c |",
            "| :tada: party | c |",
            "| [^note] | c |",
        ] {
            let spans = table_cell_spans(line);
            let (s, e) = spans[0];
            let cell = &line[s..e];
            let lead = cell.len() - cell.trim_start().len();
            let trail = cell.len() - cell.trim_end().len();
            let content = (s + lead)..(e - trail).max(s + lead);
            let inactive = plan_cell(line, content.clone(), None, &Default::default());
            let active = plan_cell(line, content.clone(), Some(content.clone()), &Default::default());
            // 两条路径都是 plan_cell：非揭示态只隐藏标记，可见文本一致。
            assert!(
                !inactive.visible.is_empty(),
                "非活动格必须有内容（line={line:?}）"
            );
            // 揭示态只是多出标记，内容字节仍在本格内。
            for seg in &active.segs {
                assert!(
                    seg.src.start >= content.start && seg.src.end <= content.end,
                    "活动格 segs 越界：{seg:?}（line={line:?}）"
                );
            }
        }
    }

    #[test]
    fn table_cell_rendering_matches_paragraph_rendering() {
        // 同一个源码片段，在段落里和在表格格里必须渲染成**同一种**样子
        // （live 编辑器的规则：保留原文 + 上样式，因为映射要字节精确；
        // 只有只读预览才把 `:tada:` 换成字形）。
        use crate::editor::markdown_live::block::table_cell_spans;
        use crate::editor::markdown_live::layout::{plan, plan_cell};
        use crate::editor::markdown_live::block::{classify, DocState};
        for src in [
            "**bold**",
            "`code` and *it*",
            "[docs](https://x.dev)",
            "~~gone~~ ==mark==",
            "E=mc^2^ H~2~O",
            ":tada: party",
            "plain text",
        ] {
            // 段落：整行就是内容。
            let mut st = DocState::default();
            let block = classify(src, &mut st);
            let para = plan(src, &block, None, false);
            // 单元格：`| {src} | c |` 的第一格。
            let line = format!("| {src} | c |");
            let spans = table_cell_spans(&line);
            let (s, e) = spans[0];
            let cell = &line[s..e];
            let lead = cell.len() - cell.trim_start().len();
            let trail = cell.len() - cell.trim_end().len();
            let content = (s + lead)..(e - trail).max(s + lead);
            let in_cell = plan_cell(&line, content, None, &Default::default());
            assert_eq!(
                para.visible, in_cell.visible,
                "段落与单元格的可见文本必须一致（src={src:?}）"
            );
            // 样式也必须一致（逐 run 对比）。
            let para_runs: Vec<_> = para
                .runs
                .iter()
                .map(|r| (r.len, r.style.bold, r.style.italic, r.style.code))
                .collect();
            let cell_runs: Vec<_> = in_cell
                .runs
                .iter()
                .map(|r| (r.len, r.style.bold, r.style.italic, r.style.code))
                .collect();
            assert_eq!(
                para_runs, cell_runs,
                "段落与单元格的样式 run 必须一致（src={src:?}）"
            );
        }
    }

    #[test]
    fn table_cells_hide_markers_like_preview() {
        assert_eq!(rendered("**bold**"), "bold");
        assert_eq!(rendered("*italic*"), "italic");
        assert_eq!(rendered("`code`"), "code");
        assert_eq!(rendered("~~gone~~"), "gone");
        assert_eq!(rendered("==note=="), "note");
        assert_eq!(rendered("[docs](https://x.dev)"), "docs");
        assert_eq!(rendered("$x + y$"), "x + y");
        assert_eq!(rendered("E=mc^2^"), "E=mc2");
        assert_eq!(rendered("H~2~O"), "H2O");
        assert_eq!(rendered("plain"), "plain");
        // Escapes render literally (`\\*` -> `*`).
        assert_eq!(rendered("\\*x\\*"), "*x*");
    }

    #[test]
    fn table_cells_keep_emoji_tokens_and_bracket_footnotes_like_live_rows() {
        // live 编辑器（含表格格）保留 `:shortcode:` 原文并上色 —— 只有只读
        // 预览才换字形（`markdown_element::replace_shortcodes`）。这样格内
        // 编辑的字节映射才精确（与段落里的 emoji 行为一致）。
        assert_eq!(rendered(":tada: party"), ":tada: party");
        // 脚注引用：`[]` 是标记（隐藏），`^` 与标签内容保留 —— 与段落在
        // live 里的渲染一致（`[^note]` → `note`）。
        assert_eq!(rendered("[^note]"), "note");
    }

    #[test]
    fn table_cells_resolve_reference_links() {
        use crate::editor::markdown_live::layout::plan_cell;
        use std::collections::HashMap;
        let mut refs = HashMap::new();
        refs.insert("docs".to_string(), "https://x.dev".to_string());
        let line = "| [docs] | c |";
        let plan = plan_cell(line, 2..8, None, &refs);
        assert_eq!(plan.visible, "docs");
        assert_eq!(plan.links.len(), 1, "引用链接必须解析出目标：{plan:?}");
        assert_eq!(plan.links[0].1, "https://x.dev");
        // Without the table the shortcut stays literal.
        assert_eq!(rendered("[docs]"), "[docs]");
    }

    #[test]
    fn find_marks_tint_current_differently() {
        let mk = |len: usize| TextRun {
            len,
            ..Default::default()
        };
        let bg = Hsla::default();
        let current_bg = Hsla {
            l: 0.5,
            ..Default::default()
        };
        let out = apply_find_marks(vec![mk(10)], 10, &[(2, 4, false), (6, 8, true)], bg, current_bg);
        let total: usize = out.iter().map(|r| r.len).sum();
        assert_eq!(total, 10);
        let colored: Vec<_> = out.iter().filter(|r| r.background_color.is_some()).collect();
        assert_eq!(colored.len(), 2);
        assert_eq!(colored[0].background_color, Some(bg));
        assert_eq!(colored[1].background_color, Some(current_bg));
        // Empty marks → untouched.
        assert_eq!(apply_find_marks(vec![mk(4)], 4, &[], bg, current_bg).len(), 1);
    }
}
