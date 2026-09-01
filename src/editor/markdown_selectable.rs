use gpui::{
    App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, SharedString, StyledText, TextLayout, TextRun, Window, px,
};

use crate::theme::ActiveTheme;

use super::markdown::Markdown;

// SelectableText: StyledText with mouse drag selection and highlight.
pub struct SelectableText {
    id: ElementId,
    text: SharedString,
    runs: Vec<TextRun>,
    fragment_id: usize,
    markdown: gpui::Entity<Markdown>,
}

fn word_range_for_index(text: &str, index: usize) -> std::ops::Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let mut start = index;
    // Move start left to char boundary
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    // Find word start
    let mut s = start;
    while s > 0 {
        let prev = text[..s].chars().next_back().unwrap();
        if prev.is_alphanumeric() || prev == '_' {
            s -= prev.len_utf8();
        } else {
            break;
        }
    }
    let mut e = index;
    while e < text.len() && !text.is_char_boundary(e) {
        e += 1;
    }
    // Find word end
    while e < text.len() {
        let ch = text[e..].chars().next().unwrap();
        if ch.is_alphanumeric() || ch == '_' {
            e += ch.len_utf8();
        } else {
            break;
        }
    }
    // If no word char at index, return single char or empty
    if s == e {
        // No alphanumeric word, select single char or whitespace run?
        // Fallback to single character
        if e < text.len() {
            let ch = text[e..].chars().next().unwrap();
            e += ch.len_utf8();
        }
    }
    s..e
}

fn line_range_for_index(text: &str, index: usize) -> std::ops::Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let mut idx = index.min(text.len());
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    let start = text[..idx].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    let end = text[idx..]
        .find('\n')
        .map(|pos| idx + pos)
        .unwrap_or(text.len());
    start..end
}

fn closest_offset_for_position(layout: &TextLayout, text: &str, raw_index: usize, x: Pixels) -> usize {
    if raw_index >= text.len() {
        return text.len();
    }
    // Snap raw_index to char boundary first
    let mut idx = raw_index;
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    if idx >= text.len() {
        return text.len();
    }
    // Find next char boundary
    let ch = text[idx..].chars().next().unwrap();
    let next_idx = idx + ch.len_utf8();
    // Get x positions for idx and next_idx
    let pos = layout.position_for_index(idx);
    let next_pos = layout.position_for_index(next_idx);
    match (pos, next_pos) {
        (Some(a), Some(b)) if (b.y - a.y).abs() < px(1.) => {
            // Same line: use midpoint
            if x > (a.x + b.x) / 2. {
                next_idx
            } else {
                idx
            }
        }
        // Different lines (wrapped) or missing data: keep idx
        _ => idx,
    }
}

impl SelectableText {
    pub fn new(
        id: impl Into<ElementId>,
        text: SharedString,
        runs: Vec<TextRun>,
        fragment_id: usize,
        markdown: gpui::Entity<Markdown>,
    ) -> Self {
        Self {
            id: id.into(),
            text,
            runs,
            fragment_id,
            markdown,
        }
    }

    fn selection_color(cx: &App) -> gpui::Hsla {
        let theme = cx.theme();
        // Use terminal selection background for consistency with editor/terminal selections.
        // Fallback to ui.selection with reduced opacity if needed.
        let bg = theme.terminal.selection_background;
        if bg.a > 0.01 {
            bg
        } else {
            theme.ui.selection.opacity(0.35)
        }
    }

    fn build_highlighted_runs(
        original: &[TextRun],
        local_selection: Option<std::ops::Range<usize>>,
        selection_bg: gpui::Hsla,
    ) -> Vec<TextRun> {
        if local_selection.is_none() {
            return original.to_vec();
        }
        let sel = local_selection.unwrap();
        if sel.start >= sel.end {
            return original.to_vec();
        }
        let mut out = Vec::new();
        let mut offset = 0usize;
        for run in original {
            let run_start = offset;
            let run_end = offset + run.len;
            if run_end <= sel.start || run_start >= sel.end {
                out.push(run.clone());
            } else {
                // Overlap: split into up to 3 pieces
                let overlap_start = sel.start.max(run_start);
                let overlap_end = sel.end.min(run_end);
                if run_start < overlap_start {
                    let mut left = run.clone();
                    left.len = overlap_start - run_start;
                    out.push(left);
                }
                {
                    let mut mid = run.clone();
                    mid.len = overlap_end - overlap_start;
                    mid.background_color = Some(selection_bg);
                    out.push(mid);
                }
                if overlap_end < run_end {
                    let mut right = run.clone();
                    right.len = run_end - overlap_end;
                    out.push(right);
                }
            }
            offset = run_end;
        }
        out
    }
}

impl Element for SelectableText {
    type RequestLayoutState = StyledText;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Determine local selection for this fragment
        let local_range = {
            let markdown = self.markdown.read(cx);
            markdown.local_selection_for_fragment(self.fragment_id, self.text.len())
        };

        let runs = if let Some(range) = local_range {
            let bg = Self::selection_color(cx);
            Self::build_highlighted_runs(&self.runs, Some(range), bg)
        } else {
            self.runs.clone()
        };

        let mut styled = StyledText::new(self.text.clone()).with_runs(runs);
        let (layout_id, _) = styled.request_layout(_id, _inspector_id, window, cx);
        // We need to keep styled for prepaint/paint. Store it as RequestLayoutState.
        // StyledText's request_layout returns (LayoutId, ()), but we want to keep styled.
        // Instead, we will reconstruct styled in prepaint/paint via same runs?
        // To keep layout, we store styled itself.
        // However StyledText's RequestLayoutState is (), so we need to store styled.
        // We'll just return layout_id and styled as state.
        // But trait expects RequestLayoutState = StyledText, so we return styled.
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

        // Cursor style: IBeam when hovered
        window.set_cursor_style(gpui::CursorStyle::IBeam, hitbox);

        // Capture layout for hit testing
        let layout = styled.layout().clone();
        let fragment_id = self.fragment_id;
        let markdown = self.markdown.clone();
        let text_len = self.text.len();
        let hitbox_for_down = hitbox.clone();
        let layout_for_down = layout.clone();
        // MouseDown
        window.on_mouse_event({
            let markdown = markdown.clone();
            let hitbox = hitbox_for_down.clone();
            let layout = layout_for_down.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if event.button != gpui::MouseButton::Left {
                    return;
                }
                if !hitbox.is_hovered(window) {
                    return;
                }
                let raw_index = match layout.index_for_position(event.position) {
                    Ok(ix) | Err(ix) => ix.min(text_len),
                };
                let frag_text = markdown.read(cx).fragment_texts.borrow().get(&fragment_id).cloned().unwrap_or_default();
                let text_ref = if !frag_text.is_empty() { frag_text } else { layout.text() };
                let mut index = raw_index;
                while index < text_ref.len() && !text_ref.is_char_boundary(index) {
                    index += 1;
                }
                index = closest_offset_for_position(&layout, &text_ref, index, event.position.x).min(text_len);

                markdown.update(cx, |markdown, cx| {
                    if event.modifiers.shift && !markdown.selection.is_empty() {
                        markdown.selection.set_focus(fragment_id, index);
                        markdown.selection.pending = true;
                    } else {
                        match event.click_count {
                            2 => {
                                // Double-click: select word
                                let range = word_range_for_index(&text_ref, index);
                                markdown.selection.set_anchor(fragment_id, range.start);
                                markdown.selection.set_focus(fragment_id, range.end);
                                markdown.selection.pending = false;
                            }
                            3 => {
                                // Triple-click: select line
                                let range = line_range_for_index(&text_ref, index);
                                markdown.selection.set_anchor(fragment_id, range.start);
                                markdown.selection.set_focus(fragment_id, range.end);
                                markdown.selection.pending = false;
                            }
                            _ => {
                                markdown.selection.set_anchor(fragment_id, index);
                                markdown.selection.set_focus(fragment_id, index);
                                markdown.selection.pending = true;
                            }
                        }
                    }
                    markdown.focus_handle.focus(window, cx);
                    cx.notify();
                });
                window.prevent_default();
                cx.stop_propagation();
            }
        });

        // MouseMove: update focus if pending
        window.on_mouse_event({
            let markdown = markdown.clone();
            let hitbox = hitbox.clone();
            let layout = layout.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if event.pressed_button != Some(gpui::MouseButton::Left) {
                    return;
                }
                // Only update if selection is pending
                let is_pending = markdown.read(cx).selection.pending;
                if !is_pending {
                    return;
                }
                let vertically_inside = event.position.y >= hitbox.bounds.top()
                    && event.position.y < hitbox.bounds.bottom();
                if !vertically_inside {
                    return;
                }
                let frag_text = markdown.read(cx).fragment_texts.borrow().get(&fragment_id).cloned().unwrap_or_default();
                let text_ref = if !frag_text.is_empty() { frag_text } else { layout.text() };
                let index = if event.position.x < hitbox.bounds.left() {
                    0
                } else if event.position.x >= hitbox.bounds.right() {
                    text_len
                } else {
                    let raw_index = match layout.index_for_position(event.position) {
                        Ok(ix) | Err(ix) => ix.min(text_len),
                    };
                    let mut idx = raw_index;
                    while idx < text_ref.len() && !text_ref.is_char_boundary(idx) {
                        idx += 1;
                    }
                    closest_offset_for_position(&layout, &text_ref, idx, event.position.x).min(text_len)
                };
                markdown.update(cx, |markdown, cx| {
                    markdown.selection.set_focus(fragment_id, index);
                    cx.notify();
                });
            }
        });

        // MouseUp: end pending
        window.on_mouse_event({
            let markdown = markdown.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if event.button != gpui::MouseButton::Left {
                    return;
                }
                let was_pending = markdown.read(cx).selection.pending;
                if was_pending {
                    markdown.update(cx, |markdown, cx| {
                        markdown.selection.pending = false;
                        cx.notify();
                    });
                    // On Linux, also copy to primary clipboard
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    {
                        let text = markdown.read(cx).selected_plain_text(cx);
                        if !text.is_empty() {
                            cx.write_to_primary(gpui::ClipboardItem::new_string(text));
                        }
                    }
                }
            }
        });
    }
}

impl IntoElement for SelectableText {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}
