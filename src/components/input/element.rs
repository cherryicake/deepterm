use std::{ops::Range, rc::Rc};

use gpui::{
    AnyElement, App, Bounds, Corners, Element, ElementId, ElementInputHandler, Entity,
    GlobalElementId, Half, HighlightStyle, Hitbox, HitboxBehavior, Hsla, IntoElement, LayoutId,
    MouseButton, MouseMoveEvent, ParentElement as _, Path, Pixels, Point, ShapedLine, SharedString,
    Size, Style, TextAlign, TextRun, TextStyle, UnderlineStyle, Window, div, fill, point, px,
    relative, size,
};
use gpui::prelude::*;
use ropey::Rope;
use smallvec::SmallVec;

use crate::{
    components::button::Size as ButtonSize,
    components::input::{RopeExt as _, blink_cursor::CURSOR_WIDTH, display_map::LineLayout},
    theme::ActiveTheme as _,
};

use super::{InputState, LastLayout, WhitespaceIndicators, mode::InputMode};

const BOTTOM_MARGIN_ROWS: usize = 3;
pub(super) const RIGHT_MARGIN: Pixels = px(10.);
pub(super) const LINE_NUMBER_RIGHT_MARGIN: Pixels = px(10.);
const FOLD_ICON_HITBOX_WIDTH: Pixels = px(18.);
const MAX_HIGHLIGHT_LINE_LENGTH: usize = 10_000;

use super::MASK_CHAR;

/// Convert a byte offset in the original text to a byte offset in the masked display string.
///
/// The masked string consists of `MASK_CHAR` repeated once per character in the original text.
/// Since `MASK_CHAR` may be multi-byte in UTF-8, the byte offset in the masked string is
/// `char_index * MASK_CHAR.len_utf8()`.
fn masked_display_offset(text: &Rope, original_offset: usize) -> usize {
    text.offset_to_char_index(original_offset) * MASK_CHAR.len_utf8()
}

/// Layout information for fold icons.
struct FoldIconLayout {
    /// Hitbox for the line number area (used for hover detection)
    line_number_hitbox: Hitbox,
    /// List of (display_row, is_folded, icon_element) pairs for each fold candidate
    icons: Vec<(usize, bool, gpui::AnyElement)>,
}

pub(crate) struct TextElement {
    pub(crate) state: Entity<InputState>,
    placeholder: SharedString,
}

impl TextElement {
    pub(super) fn new(state: Entity<InputState>) -> Self {
        Self {
            state,
            placeholder: SharedString::default(),
        }
    }

    /// Set the placeholder text of the input field.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    fn paint_mouse_listeners(&mut self, window: &mut Window, _: &mut App) {
        window.on_mouse_event({
            let state = self.state.clone();

            move |event: &MouseMoveEvent, _, window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    state.update(cx, |state, cx| {
                        state.on_drag_move(event, window, cx);
                    });
                }
            }
        });
    }

    /// Prepare the state before layout.
    ///
    /// Handles pending updates (moved from InputState::render) and ensures
    /// the display map is ready for layout.
    fn prepare_if_needed(&mut self, window: &mut Window, cx: &mut App) {
        self.state.update(cx, |state, cx| {
            if state._pending_update {
                let bg = state
                    .mode
                    .update_highlighter(&(0..0), &state.text, "", false, cx);
                if let Some(bg) = bg {
                    InputState::dispatch_background_parse(bg, window, cx);
                }
                state.update_fold_candidates();
                state.lsp.update(&state.text, window, cx);
                state._pending_update = false;
            }
            state.display_map.set_font(
                window.text_style().font(),
                state
                    .font_size
                    .unwrap_or_else(|| window.text_style().font_size.to_pixels(window.rem_size())),
                cx,
            );
            state.display_map.ensure_text_prepared(&state.text, cx);
        });
    }

    /// Returns the:
    ///
    /// - cursor bounds
    /// - scroll offset
    /// - current row index (No only the visible lines, but all lines)
    ///
    /// This method also will update for track scroll to cursor.
    fn layout_cursor(
        &self,
        last_layout: &LastLayout,
        bounds: &mut Bounds<Pixels>,
        _: &mut Window,
        cx: &mut App,
    ) -> (Option<Bounds<Pixels>>, Point<Pixels>, Option<usize>) {
        let state = self.state.read(cx);

        let line_height = last_layout.line_height;
        let visible_range = &last_layout.visible_range;
        let lines = &last_layout.lines;
        let line_number_width = last_layout.line_number_width;

        let mut selected_range = state.selected_range;

        if let Some(ime_marked_range) = &state.ime_marked_range {
            selected_range = (ime_marked_range.end..ime_marked_range.end).into();
        }
        let is_selected_all = selected_range.len() == state.text.len();

        let mut cursor = state.cursor();
        if state.masked {
            selected_range.start = masked_display_offset(&state.text, selected_range.start);
            selected_range.end = masked_display_offset(&state.text, selected_range.end);
            cursor = masked_display_offset(&state.text, cursor);
        }

        let mut current_row = None;
        let mut scroll_offset = state.scroll_handle.offset();
        let mut cursor_bounds = None;

        // If the input has a fixed height (Otherwise is auto-grow), we need to add a bottom margin to the input.
        let top_bottom_margin = if state.mode.is_auto_grow() {
            line_height
        } else if visible_range.len() < BOTTOM_MARGIN_ROWS * 8 {
            line_height
        } else {
            BOTTOM_MARGIN_ROWS * line_height
        };

        // The cursor corresponds to the current cursor position in the text no only the line.
        let mut cursor_pos = None;
        let mut cursor_start = None;
        let mut cursor_end = None;

        let mut prev_lines_offset = 0;
        let mut offset_y = px(0.);
        let buffer_lines = state.display_map.lines();
        let visible_buffer_lines = &last_layout.visible_buffer_lines;
        let mut vi = 0; // index into visible_buffer_lines / lines
        for (ix, wrap_line) in buffer_lines.iter().enumerate() {
            let row = ix;
            let line_origin = point(px(0.), offset_y);

            // break loop if all cursor positions are found
            if cursor_pos.is_some() && cursor_start.is_some() && cursor_end.is_some() {
                break;
            }

            // Check if this buffer line has a LineLayout in the compact lines vec
            let line_layout = if vi < visible_buffer_lines.len() && visible_buffer_lines[vi] == ix {
                let l = &lines[vi];
                vi += 1;
                Some(l)
            } else {
                None
            };

            if let Some(line) = line_layout {
                if cursor_pos.is_none() {
                    let offset = cursor.saturating_sub(prev_lines_offset);
                    if let Some(pos) =
                        line.position_for_index(offset, last_layout, state.cursor_line_end_affinity)
                    {
                        current_row = Some(row);
                        cursor_pos = Some(line_origin + pos);
                    }
                }
                if cursor_start.is_none() {
                    let offset = selected_range.start.saturating_sub(prev_lines_offset);
                    if let Some(pos) = line.position_for_index(offset, last_layout, false) {
                        cursor_start = Some(line_origin + pos);
                    }
                }
                if cursor_end.is_none() {
                    let offset = selected_range.end.saturating_sub(prev_lines_offset);
                    if let Some(pos) = line.position_for_index(offset, last_layout, false) {
                        cursor_end = Some(line_origin + pos);
                    }
                }

                offset_y += line.size(line_height).height;
                // +1 for the last `\n`
                prev_lines_offset += wrap_line.len() + 1;
            } else {
                // Not visible (before visible range or hidden/folded).
                // Just increase the offset_y and prev_lines_offset for scroll tracking.
                if prev_lines_offset >= cursor && cursor_pos.is_none() {
                    current_row = Some(row);
                    cursor_pos = Some(line_origin);
                }
                if prev_lines_offset >= selected_range.start && cursor_start.is_none() {
                    cursor_start = Some(line_origin);
                }
                if prev_lines_offset >= selected_range.end && cursor_end.is_none() {
                    cursor_end = Some(line_origin);
                }

                let visible_wrap_rows =
                    state.display_map.visible_wrap_row_count_for_buffer_line(ix);
                offset_y += line_height * visible_wrap_rows;
                // +1 for the last `\n`
                prev_lines_offset += wrap_line.len() + 1;
            }
        }

        if let (Some(cursor_pos), Some(cursor_start), Some(cursor_end)) =
            (cursor_pos, cursor_start, cursor_end)
        {
            let selection_changed = state.last_selected_range != Some(selected_range);
            if selection_changed && !is_selected_all {
                // Apart from left alignment, just leave enough space for the cursor size on the right side.
                let safety_margin = if last_layout.text_align == TextAlign::Left {
                    RIGHT_MARGIN
                } else {
                    CURSOR_WIDTH
                };

                scroll_offset.x = if scroll_offset.x + cursor_pos.x
                    > (bounds.size.width - line_number_width - safety_margin)
                {
                    // cursor is out of right
                    bounds.size.width - line_number_width - safety_margin - cursor_pos.x
                } else if scroll_offset.x + cursor_pos.x < px(0.) {
                    // cursor is out of left
                    scroll_offset.x - cursor_pos.x
                } else {
                    scroll_offset.x
                };

                // If we change the scroll_offset.y, GPUI will render and trigger the next run loop.
                // So, here we just adjust offset by `line_height` for move smooth.
                scroll_offset.y =
                    if scroll_offset.y + cursor_pos.y > bounds.size.height - top_bottom_margin {
                        // cursor is out of bottom
                        scroll_offset.y - line_height
                    } else if scroll_offset.y + cursor_pos.y < top_bottom_margin {
                        // cursor is out of top
                        (scroll_offset.y + line_height).min(px(0.))
                    } else {
                        scroll_offset.y
                    };

                // For selection to move scroll
                if state.selection_reversed {
                    if scroll_offset.x + cursor_start.x < px(0.) {
                        // selection start is out of left
                        scroll_offset.x = -cursor_start.x;
                    }
                    if scroll_offset.y + cursor_start.y < px(0.) {
                        // selection start is out of top
                        scroll_offset.y = -cursor_start.y;
                    }
                } else {
                    // TODO: Consider to remove this part,
                    // maybe is not necessary (But selection_reversed is needed).
                    if scroll_offset.x + cursor_end.x <= px(0.) {
                        // selection end is out of left
                        scroll_offset.x = -cursor_end.x;
                    }
                    if scroll_offset.y + cursor_end.y <= px(0.) {
                        // selection end is out of top
                        scroll_offset.y = -cursor_end.y;
                    }
                }
            }

            // cursor bounds
            let cursor_height = match state.size {
                ButtonSize::Large => 1.,
                ButtonSize::Small => 0.75,
                _ => 0.85,
            } * line_height;

            cursor_bounds = Some(Bounds::new(
                point(
                    bounds.left() + cursor_pos.x + line_number_width + scroll_offset.x,
                    bounds.top() + cursor_pos.y + ((line_height - cursor_height) / 2.),
                ),
                size(CURSOR_WIDTH, cursor_height),
            ));
        }

        if let Some(deferred_scroll_offset) = state.deferred_scroll_offset {
            scroll_offset = deferred_scroll_offset;
        }

        bounds.origin = bounds.origin + scroll_offset;

        (cursor_bounds, scroll_offset, current_row)
    }

    /// Layout the match range to a Path.
    pub(crate) fn layout_match_range(
        range: Range<usize>,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
    ) -> Option<Path<Pixels>> {
        if range.is_empty() {
            return None;
        }

        if range.start < last_layout.visible_range_offset.start
            || range.end > last_layout.visible_range_offset.end
        {
            return None;
        }

        let line_height = last_layout.line_height;
        let visible_top = last_layout.visible_top;
        let lines = &last_layout.lines;
        let line_number_width = last_layout.line_number_width;

        let start_ix = range.start;
        let end_ix = range.end;

        // Start from visible_top (which already accounts for all lines before visible range)
        let mut offset_y = visible_top;
        let mut line_corners = vec![];

        // Iterate only over visible (non-hidden) buffer lines
        for (prev_lines_offset, line) in last_layout
            .visible_line_byte_offsets
            .iter()
            .zip(lines.iter())
        {
            let prev_lines_offset = *prev_lines_offset;
            let line_size = line.size(line_height);
            let line_wrap_width = line_size.width;

            let line_origin = point(px(0.), offset_y);

            let line_cursor_start = line.position_for_index(
                start_ix.saturating_sub(prev_lines_offset),
                last_layout,
                false,
            );
            let line_cursor_end = line.position_for_index(
                end_ix.saturating_sub(prev_lines_offset),
                last_layout,
                false,
            );

            if line_cursor_start.is_some() || line_cursor_end.is_some() {
                let start = line_cursor_start.unwrap_or_else(|| {
                    line.position_for_index(0, last_layout, false)
                        .unwrap_or(point(px(0.), px(0.)))
                });

                let end = line_cursor_end.unwrap_or_else(|| {
                    line.position_for_index(line.len(), last_layout, false)
                        .unwrap_or(point(px(0.), px(0.)))
                });

                // Split the selection into multiple items
                let wrapped_lines =
                    (end.y / line_height).ceil() as usize - (start.y / line_height).ceil() as usize;

                let mut end_x = end.x;
                if wrapped_lines > 0 {
                    end_x = line_wrap_width;
                }

                // Ensure at least 6px width for the selection for empty lines.
                end_x = end_x.max(start.x + px(6.));

                line_corners.push(Corners {
                    top_left: line_origin + point(start.x, start.y),
                    top_right: line_origin + point(end_x, start.y),
                    bottom_left: line_origin + point(start.x, start.y + line_height),
                    bottom_right: line_origin + point(end_x, start.y + line_height),
                });

                // wrapped lines
                for i in 1..=wrapped_lines {
                    let start = point(px(0.), start.y + i as f32 * line_height);
                    let mut end = point(end.x, end.y + i as f32 * line_height);
                    if i < wrapped_lines {
                        end.x = line_size.width;
                    }

                    line_corners.push(Corners {
                        top_left: line_origin + point(start.x, start.y),
                        top_right: line_origin + point(end.x, start.y),
                        bottom_left: line_origin + point(start.x, start.y + line_height),
                        bottom_right: line_origin + point(end.x, start.y + line_height),
                    });
                }
            }

            if line_cursor_start.is_some() && line_cursor_end.is_some() {
                break;
            }

            offset_y += line_size.height;
        }

        let mut points = vec![];
        if line_corners.is_empty() {
            return None;
        }

        // Fix corners to make sure the left to right direction
        for corners in &mut line_corners {
            if corners.top_left.x > corners.top_right.x {
                std::mem::swap(&mut corners.top_left, &mut corners.top_right);
                std::mem::swap(&mut corners.bottom_left, &mut corners.bottom_right);
            }
        }

        for corners in &line_corners {
            points.push(corners.top_right);
            points.push(corners.bottom_right);
            points.push(corners.bottom_left);
        }

        let mut rev_line_corners = line_corners.iter().rev().peekable();
        while let Some(corners) = rev_line_corners.next() {
            points.push(corners.top_left);
            if let Some(next) = rev_line_corners.peek() {
                if next.top_left.x > corners.top_left.x {
                    points.push(point(next.top_left.x, corners.top_left.y));
                }
            }
        }

        let path_origin = bounds.origin + point(line_number_width, px(0.));
        let n = points.len();
        if n < 3 {
            return None;
        }

        let corner_radius = px(4.);
        let r = f32::from(corner_radius);

        // Determine which vertices are convex (exposed outer corners) using cross product.
        // The polygon is counterclockwise, so convex corners have positive cross product.
        let is_convex: Vec<bool> = (0..n)
            .map(|i| {
                let prev = points[(i + n - 1) % n];
                let curr = points[i];
                let next = points[(i + 1) % n];
                let cross = (f32::from(curr.x) - f32::from(prev.x))
                    * (f32::from(next.y) - f32::from(prev.y))
                    - (f32::from(curr.y) - f32::from(prev.y))
                        * (f32::from(next.x) - f32::from(prev.x));
                cross > 0.
            })
            .collect();

        // Build path with rounded corners at convex vertices
        let mut builder = gpui::PathBuilder::fill();

        // For vertex[0], compute the arrive point and move there directly
        // so that close() draws the closing edge correctly.
        if is_convex[0] {
            let prev = points[n - 1];
            let curr = points[0];
            let dx = f32::from(curr.x) - f32::from(prev.x);
            let dy = f32::from(curr.y) - f32::from(prev.y);
            let len = (dx * dx + dy * dy).sqrt();
            if len > 0. {
                let ux = dx / len;
                let uy = dy / len;
                builder.move_to(path_origin + point(
                    curr.x - px(ux * r),
                    curr.y - px(uy * r),
                ));
            } else {
                builder.move_to(path_origin + curr);
            }
        } else {
            builder.move_to(path_origin + points[0]);
        }

        for i in 0..n {
            let curr = points[i];
            let next = points[(i + 1) % n];

            if is_convex[i] {
                let prev = points[(i + n - 1) % n];
                let dx_in = f32::from(curr.x) - f32::from(prev.x);
                let dy_in = f32::from(curr.y) - f32::from(prev.y);
                let len_in = (dx_in * dx_in + dy_in * dy_in).sqrt();
                if len_in > 0. {
                    let ux_in = dx_in / len_in;
                    let uy_in = dy_in / len_in;
                    let start = point(
                        curr.x - px(ux_in * r),
                        curr.y - px(uy_in * r),
                    );

                    let dx_out = f32::from(next.x) - f32::from(curr.x);
                    let dy_out = f32::from(next.y) - f32::from(curr.y);
                    let len_out = (dx_out * dx_out + dy_out * dy_out).sqrt();
                    if len_out > 0. {
                        let ux_out = dx_out / len_out;
                        let uy_out = dy_out / len_out;
                        let end = point(
                            curr.x + px(ux_out * r),
                            curr.y + px(uy_out * r),
                        );

                        // Skip line_to for vertex[0] since move_to already placed us at start
                        if i > 0 {
                            builder.line_to(path_origin + start);
                        }
                        builder.curve_to(path_origin + end, path_origin + curr);
                    } else {
                        if i > 0 {
                            builder.line_to(path_origin + curr);
                        }
                    }
                } else if i > 0 {
                    builder.line_to(path_origin + curr);
                }
            } else if i > 0 {
                builder.line_to(path_origin + curr);
            }
        }

        builder.build().ok()
    }

    fn layout_selections(
        &self,
        last_layout: &LastLayout,
        bounds: &mut Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Path<Pixels>> {
        let state = self.state.read(cx);
        if !state.focus_handle.is_focused(window) {
            return None;
        }

        let mut selected_range = state.selected_range;
        if let Some(ime_marked_range) = &state.ime_marked_range {
            if !ime_marked_range.is_empty() {
                selected_range = (ime_marked_range.end..ime_marked_range.end).into();
            }
        }
        if selected_range.is_empty() {
            return None;
        }

        if state.masked {
            selected_range.start = masked_display_offset(&state.text, selected_range.start);
            selected_range.end = masked_display_offset(&state.text, selected_range.end);
        }

        let (start_ix, end_ix) = if selected_range.start < selected_range.end {
            (selected_range.start, selected_range.end)
        } else {
            (selected_range.end, selected_range.start)
        };

        let range = start_ix.max(last_layout.visible_range_offset.start)
            ..end_ix.min(last_layout.visible_range_offset.end);

        Self::layout_match_range(range, &last_layout, bounds)
    }

    /// Layout highlight paths for every search match in the visible range.
    /// Returns (path, is_current_match); the current match is painted with
    /// the selection color on top of the weaker match color.
    fn layout_search_matches(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &mut App,
    ) -> Vec<(Path<Pixels>, bool)> {
        let state = self.state.read(cx);
        let Some(search_panel) = state.search_panel.clone() else {
            return Vec::new();
        };

        let Some(matcher) = search_panel.read(cx).matcher() else {
            return Vec::new();
        };
        let ranges = matcher.matched_ranges.clone();
        let current_match_ix = matcher.current_match_ix;

        let mut paths = Vec::with_capacity(ranges.as_ref().len());
        for (index, range) in ranges.as_ref().iter().enumerate() {
            if let Some(path) = Self::layout_match_range(range.clone(), last_layout, bounds) {
                paths.push((path, current_match_ix == index));
            }
        }
        paths
    }

    /// Draw the hover and completion popups as window overlays, anchored at
    /// the symbol / cursor (zed's `window.defer_draw` approach — no CSS
    /// absolute positioning, so coordinates are exact).
    fn draw_popups(&self, bounds: &Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        // Build popups inside the state borrow (creating elements only needs
        // immutable cx), then defer_draw them after the borrow ends.
        let popups: Vec<(AnyElement, Point<Pixels>)> = {
            let state = self.state.read(cx);
            let line_height = state
                .last_layout
                .as_ref()
                .map(|l| l.line_height)
                .unwrap_or(px(16.0));
            let viewport_top = bounds.origin.y;
            let viewport_bottom = bounds.origin.y + bounds.size.height;
            let mut popups = Vec::new();

            if let Some(popover) = &state.hover_popover {
                if !popover.contents.is_empty() {
                    if let Some(pos) =
                        state.line_and_position_for_offset(popover.symbol_range.start).2
                    {
                        let below = bounds.origin + pos + point(px(0.), line_height);
                        let origin = if below.y + px(200.0) > viewport_bottom {
                            point(below.x, (below.y - px(200.0)).max(viewport_top))
                        } else {
                            below
                        };
                        popups.push((Self::hover_popup_element(state, cx), origin));
                    }
                }
            }

            if let Some(menu) = &state.completion_menu {
                if menu.visible && !menu.items.is_empty() {
                    if let Some(pos) = state.line_and_position_for_offset(state.cursor()).2 {
                        let below = bounds.origin + pos + point(px(0.), line_height);
                        let origin = if below.y + px(240.0) > viewport_bottom {
                            point(below.x, (below.y - px(240.0)).max(viewport_top))
                        } else {
                            below
                        };
                        popups.push((Self::completion_popup_element(state, cx), origin));
                    }
                }
            }
            popups
        };

        for (popup, origin) in popups {
            window.defer_draw(popup, origin, 2, None);
        }
    }

    /// The hover popup element (markdown-ish contents).
    fn hover_popup_element(state: &InputState, cx: &App) -> AnyElement {
        let popover = state.hover_popover.as_ref().expect("hover popover set");
        let theme = cx.theme();
        div()
            .id("hover-popover")
            .max_w(px(480.0))
            .max_h(px(200.0))
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .text_color(theme.text)
            .child(popover.contents.clone())
            .into_any_element()
    }

    /// The completion popup element (zed-style: kind letter + label + detail).
    fn completion_popup_element(state: &InputState, cx: &App) -> AnyElement {
        let menu = state.completion_menu.as_ref().expect("completion menu set");
        let theme = cx.theme();
        let items = menu
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let selected = i == menu.selected;
                let (kind_char, kind_color) =
                    Self::completion_kind_letter(item.kind, theme);
                let row = div()
                    .id(SharedString::from(format!("completion-item-{i}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .text_color(if selected {
                        theme.accent_foreground
                    } else {
                        theme.text
                    })
                    .bg(if selected {
                        theme.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .child(
                        div()
                            .w(px(16.0))
                            .text_color(if selected {
                                theme.accent_foreground
                            } else {
                                kind_color
                            })
                            .text_center()
                            .child(kind_char.to_string()),
                    )
                    .child(div().flex_1().overflow_x_hidden().child(item.label.clone()));
                match item.detail.as_ref() {
                    Some(detail) => row.child(
                        div()
                            .text_color(if selected {
                                theme.accent_foreground.opacity(0.8)
                            } else {
                                theme.text_muted
                            })
                            .child(detail.clone()),
                    ),
                    None => row,
                }
            })
            .collect::<Vec<_>>();
        div()
            .id("completion-menu")
            .min_w(px(260.0))
            .max_w(px(520.0))
            .max_h(px(240.0))
            .overflow_y_scroll()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .children(items)
            .into_any_element()
    }

    /// Zed-style completion-kind letter + colour.
    fn completion_kind_letter(
        kind: Option<lsp_types::CompletionItemKind>,
        theme: &crate::theme::AppTheme,
    ) -> (char, gpui::Hsla) {
        use lsp_types::CompletionItemKind as K;
        match kind {
            Some(K::FUNCTION) | Some(K::METHOD) | Some(K::CONSTRUCTOR) => ('ƒ', theme.success),
            Some(K::VARIABLE) | Some(K::FIELD) | Some(K::PROPERTY) => ('x', theme.accent),
            Some(K::STRUCT)
            | Some(K::CLASS)
            | Some(K::ENUM)
            | Some(K::INTERFACE)
            | Some(K::TYPE_PARAMETER) => ('S', gpui::rgb(0x9b59b6).into()),
            Some(K::KEYWORD) => ('k', theme.error),
            Some(K::TEXT) | Some(K::FILE) => ('s', gpui::rgb(0xe67e22).into()),
            Some(K::CONSTANT) => ('n', theme.warning),
            Some(K::MODULE) => ('m', gpui::rgb(0x16a085).into()),
            _ => ('•', theme.text_muted),
        }
    }

    /// Calculate the visible range of lines in the viewport.
    ///
    /// Returns
    ///
    /// - visible_range: The visible range is based on unwrapped lines (Zero based).
    /// - visible_buffer_lines: Indices of non-hidden buffer lines within the visible range.
    /// - visible_top: The top position of the first visible line in the scroll viewport.
    fn calculate_visible_range(
        &self,
        state: &InputState,
        line_height: Pixels,
        input_height: Pixels,
    ) -> (Range<usize>, Vec<usize>, Pixels) {
        // Add extra rows to avoid showing empty space when scroll to bottom.
        let extra_rows = 1;
        let mut visible_top = px(0.);
        if state.mode.is_single_line() {
            return (0..1, vec![0], visible_top);
        }

        let total_lines = state.display_map.wrap_row_count();
        let scroll_top = if let Some(deferred_scroll_offset) = state.deferred_scroll_offset {
            deferred_scroll_offset.y
        } else {
            state.scroll_handle.offset().y
        };

        let mut visible_range = 0..total_lines;
        let mut line_bottom = px(0.);
        for (ix, _line) in state.display_map.lines().iter().enumerate() {
            let visible_wrap_rows = state.display_map.visible_wrap_row_count_for_buffer_line(ix);

            if visible_wrap_rows == 0 {
                continue;
            }

            let wrapped_height = line_height * visible_wrap_rows;
            line_bottom += wrapped_height;

            if line_bottom < -scroll_top {
                visible_top = line_bottom - wrapped_height;
                visible_range.start = ix;
            }

            if line_bottom + scroll_top >= input_height {
                visible_range.end = (ix + extra_rows).min(total_lines);
                break;
            }
        }

        // Collect non-hidden buffer lines within the visible range
        let mut visible_buffer_lines = Vec::with_capacity(visible_range.len());
        for ix in visible_range.start..visible_range.end {
            let visible_wrap_rows = state.display_map.visible_wrap_row_count_for_buffer_line(ix);
            if visible_wrap_rows > 0 {
                visible_buffer_lines.push(ix);
            }
        }

        (visible_range, visible_buffer_lines, visible_top)
    }

    /// Return (line_number_width, line_number_len)
    fn layout_line_numbers(
        state: &InputState,
        text: &Rope,
        font_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
    ) -> (Pixels, usize) {
        let total_lines = text.lines_len();
        let line_number_len = match total_lines {
            0..=9999 => 5,
            10000..=99999 => 6,
            100000..=999999 => 7,
            _ => 8,
        };

        let mut line_number_width = if state.mode.line_number() {
            let empty_line_number = window.text_system().shape_line(
                "+".repeat(line_number_len).into(),
                font_size,
                &[TextRun {
                    len: line_number_len,
                    font: style.font(),
                    color: style.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );

            empty_line_number.width + LINE_NUMBER_RIGHT_MARGIN
        } else if state.mode.is_code_editor() && state.mode.is_multi_line() {
            LINE_NUMBER_RIGHT_MARGIN
        } else {
            px(0.)
        };

        if state.mode.is_folding() {
            // Add extra space for fold icons
            line_number_width += FOLD_ICON_HITBOX_WIDTH
        }

        (line_number_width, line_number_len)
    }

    /// Layout shaped lines for whitespace indicators (space and tab).
    ///
    /// Returns `WhitespaceIndicators` with shaped lines for space and tab characters.
    fn layout_whitespace_indicators(
        state: &InputState,
        text_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
        cx: &App,
    ) -> Option<WhitespaceIndicators> {
        if !state.show_whitespaces {
            return None;
        }

        let invisible_color = cx
            .theme()
            .highlight_theme
            .style
            .editor_invisible
            .unwrap_or(cx.theme().text_muted);

        let space_font_size = text_size.half();
        let tab_font_size = text_size;

        let space_text = SharedString::new_static("•");
        let space = window.text_system().shape_line(
            space_text.clone(),
            space_font_size,
            &[TextRun {
                len: space_text.len(),
                font: style.font(),
                color: invisible_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );

        let tab_text = SharedString::new_static("→");
        let tab = window.text_system().shape_line(
            tab_text.clone(),
            tab_font_size,
            &[TextRun {
                len: tab_text.len(),
                font: style.font(),
                color: invisible_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );

        Some(WhitespaceIndicators { space, tab })
    }

    /// Return (line_number_width, line_number_len)
    /// Layout fold icon hitboxes during prepaint phase.
    ///
    /// This creates hitboxes for the fold icon area, positioned to the right of line numbers.
    /// Icons are created and prepainted here to avoid panics.
    fn layout_fold_icons(
        &self,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        window: &mut Window,
        cx: &mut App,
    ) -> FoldIconLayout {
        let line_number_hitbox = window.insert_hitbox(
            Bounds::new(
                bounds.origin + point(px(0.), last_layout.visible_top),
                size(last_layout.line_number_width, bounds.size.height),
            ),
            HitboxBehavior::Normal,
        );

        let icon_layout = FoldIconLayout {
            line_number_hitbox,
            icons: vec![],
        };

        // Fold icons disabled in deepterm (missing Button/Icon integration)
        let _ = (last_layout, bounds, window, cx);
        icon_layout
    }

    /// Paint fold icons using prepaint hitboxes.
    ///
    /// This handles:
    /// - Rendering fold icons (chevron-right for folded, chevron-down for expanded)
    /// - Mouse click handling to toggle fold state
    /// - Cursor style changes on hover
    /// - Only show icon on hover or for current line
    fn paint_fold_icons(
        &mut self,
        fold_icon_layout: &mut FoldIconLayout,
        current_row: Option<usize>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let is_hovered = fold_icon_layout.line_number_hitbox.is_hovered(window);
        for (display_row, is_folded, icon) in fold_icon_layout.icons.iter_mut() {
            let is_current_line = current_row == Some(*display_row);

            if !is_hovered && !is_current_line && !*is_folded {
                continue;
            }

            icon.paint(window, cx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_lines(
        state: &InputState,
        display_text: &Rope,
        last_layout: &LastLayout,
        font_size: Pixels,
        runs: &[TextRun],
        _default_color: Hsla,
        whitespace_indicators: Option<WhitespaceIndicators>,
        window: &mut Window,
    ) -> Vec<LineLayout> {
        let is_single_line = state.mode.is_single_line();
        let buffer_lines = state.display_map.lines();

        if is_single_line {
            let shaped_line = window.text_system().shape_line(
                display_text.to_string().into(),
                font_size,
                &runs,
                None,
            );

            let line_layout = LineLayout::new()
                .lines(smallvec::smallvec![shaped_line])
                .with_whitespaces(whitespace_indicators);
            return vec![line_layout];
        }

        // Empty to use placeholder, the placeholder is not in the wrapper map.
        if state.text.len() == 0 {
            let text = display_text.to_string();
            return text
                .split("\n")
                .map(|line| {
                    let shaped_line = window.text_system().shape_line(
                        SharedString::from(line.to_string()),
                        font_size,
                        &runs,
                        None,
                    );
                    LineLayout::new()
                        .lines(smallvec::smallvec![shaped_line])
                        .with_whitespaces(whitespace_indicators.clone())
                })
                .collect();
        }

        let mut lines = Vec::with_capacity(last_layout.visible_buffer_lines.len());
        // run_offset is this buffer line's byte offset relative to the first
        // visible line, in the *global* text coordinate space (folded lines
        // count too). `highlight_lines` now returns a gap-free runs list over
        // the whole visible span (folded lines included), so matching global
        // offsets keep the two in sync even when lines are folded — without
        // this, every run after a fold is shifted and words get painted
        // half-highlighted / half-white.
        let first_line_start = display_text
            .line_start_offset(*last_layout.visible_buffer_lines.first().unwrap_or(&0));

        for (_vi, &buffer_line) in last_layout.visible_buffer_lines.iter().enumerate() {
            let line_text: String = display_text.slice_line(buffer_line).into();
            let line_item = buffer_lines
                .get(buffer_line)
                .expect("line should exists in wrapper");

            debug_assert_eq!(line_item.len(), line_text.len());

            let run_offset = display_text.line_start_offset(buffer_line) - first_line_start;
            let mut wrapped_lines = SmallVec::with_capacity(1);

            for range in &line_item.wrapped_lines {
                let line_runs = runs_for_range(runs, run_offset, &range);

                // Defensive checks to avoid panics from out-of-sync display map
                if range.start > line_text.len()
                    || range.end > line_text.len()
                    || !line_text.is_char_boundary(range.start)
                    || !line_text.is_char_boundary(range.end)
                {
                    log::error!(
                        "layout_lines invalid range: line_text.len={}, range={:?}, buffer_line={}",
                        line_text.len(),
                        range,
                        buffer_line
                    );
                    continue;
                }

                let sub_line: SharedString = line_text[range.clone()].to_string().into();
                let runs_len: usize = line_runs.iter().map(|r| r.len).sum();
                if runs_len != sub_line.len() {
                    let runs_total: usize = runs.iter().map(|r| r.len).sum();
                    let vis = last_layout.visible_buffer_lines.clone();
                    let first_line_start =
                        display_text.line_start_offset(*vis.first().unwrap_or(&0));
                    log::error!(
                        "layout_lines run length mismatch: sub_line.len={}, runs_len={}, buffer_line={}, range={:?}, run_offset={}, runs_total={}, visible={:?}, text_len={}, first_line_start={}",
                        sub_line.len(),
                        runs_len,
                        buffer_line,
                        range,
                        run_offset,
                        runs_total,
                        vis,
                        display_text.len(),
                        first_line_start
                    );
                }
                let shaped_line = window
                    .text_system()
                    .shape_line(sub_line, font_size, &line_runs, None);

                wrapped_lines.push(shaped_line);
            }

            let line_layout = LineLayout::new()
                .lines(wrapped_lines)
                .with_whitespaces(whitespace_indicators.clone());
            lines.push(line_layout);
        }

        lines
    }

    /// First usize is the offset of skipped.
    fn highlight_lines(
        &mut self,
        visible_buffer_lines: &[usize],
        _visible_top: Pixels,
        visible_byte_range: Range<usize>,
        cx: &mut App,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        let state = self.state.read(cx);
        let text = &state.text;
        let is_multi_line = state.mode.is_multi_line();

        let (mut highlighter_ref, diagnostics) = match &state.mode {
            InputMode::CodeEditor {
                highlighter,
                diagnostics,
                ..
            } => (highlighter.borrow_mut(), diagnostics.clone()),
            _ => return None,
        };

        let mut styles = Vec::with_capacity(visible_buffer_lines.len());

        if let Some(external) = &state.external_highlight_runs {
            // Externally computed runs (unified diff view): the list covers
            // the whole buffer gap-free; clip it to the visible span. The
            // internal tree-sitter highlighter is bypassed because the merged
            // diff text is not valid source code.
            let first = *visible_buffer_lines.first()?;
            let last = *visible_buffer_lines.last()?;
            let byte_start = text.line_start_offset(first);
            let byte_end = if is_multi_line {
                // +1 for `\n`, same as flush_range below
                text.line_start_offset(last + 1)
            } else {
                text.line_end_offset(last)
            };
            let mut cursor = byte_start;
            for (range, style) in external {
                if range.end <= byte_start || range.start >= byte_end {
                    continue;
                }
                let start = range.start.max(byte_start);
                let end = range.end.min(byte_end);
                if start > cursor {
                    styles.push((cursor..start, HighlightStyle::default()));
                }
                styles.push((start..end, style.clone()));
                cursor = end.max(cursor);
            }
            if cursor < byte_end {
                styles.push((cursor..byte_end, HighlightStyle::default()));
            }
        } else {
            let highlighter = highlighter_ref.as_mut()?;

            let highlight_theme = cx.theme().highlight_theme.clone();

            // Helper to flush a contiguous range of lines
            let mut flush_range =
                |start_line: usize, end_line: usize, skip: bool, styles: &mut Vec<_>| {
                    let byte_start = text.line_start_offset(start_line);
                    let byte_end = if is_multi_line {
                        // +1 for `\n`
                        text.line_start_offset(end_line + 1)
                    } else {
                        text.line_end_offset(end_line)
                    };
                    let range_styles = if skip {
                        vec![(byte_start..byte_end, HighlightStyle::default())]
                    } else {
                        highlighter.styles(&(byte_start..byte_end), &highlight_theme)
                    };

                    *styles = gpui::combine_highlights(styles.clone(), range_styles).collect();
                };

            // Group contiguous visible lines into ranges and call styles() once per range
            let mut visible_iter = visible_buffer_lines.iter().peekable();
            let mut range_start: Option<usize> = None;

            while let Some(&line) = visible_iter.next() {
                // Check if this line is too long for highlighting
                let line_len = text.slice_line(line).len();
                if line_len > MAX_HIGHLIGHT_LINE_LENGTH {
                    // Flush any accumulated range first
                    if let Some(start) = range_start.take() {
                        flush_range(start, line - 1, false, &mut styles);
                    }

                    flush_range(line, line, true, &mut styles);
                    continue;
                }

                range_start.get_or_insert(line);

                // Check if next line is contiguous, if so keep accumulating
                if visible_iter
                    .peek()
                    .map(|&&next| next == line + 1)
                    .unwrap_or(false)
                {
                    continue;
                }

                // Flush the contiguous range
                let start_line = range_start.take().unwrap();
                flush_range(start_line, line, false, &mut styles);
            }

            let diagnostic_styles = diagnostics.styles_for_range(&visible_byte_range, cx);
            styles = gpui::combine_highlights(diagnostic_styles, styles).collect();
        }
        drop(highlighter_ref);

        // LSP semantic tokens layered on top of the tree-sitter styles
        // (composed below the diff word colors). Only compose tokens that
        // were fetched for the *current* text version — stale tokens (byte
        // ranges from a pre-edit document) would flicker as the text shifts
        // while typing.
        // Semantic tokens are byte-ranged and shifted on each edit, so they
        // stay composed while typing (no flicker); they are only dropped for
        // tokens overlapping the edited region, which the next fetch replaces.
        let semantic_styles = if state.lsp.semantic_tokens_version() == state.text_version {
            state.lsp.semantic_tokens_for_range(
                &state.text,
                &visible_byte_range,
                &cx.theme().highlight_theme,
            )
        } else {
            Vec::new()
        };
        if !semantic_styles.is_empty() {
            styles = gpui::combine_highlights(semantic_styles, styles).collect();
        }

        // Hovered symbol underline (go-to-definition affordance).
        if let Some((range, hover_style)) = self.layout_hover_definition(cx) {
            styles.push((range, hover_style));
        }

        // Merge diff word-level decorations (e.g. inline changed-word
        // backgrounds in the git diff view).
        if !state.diff_range_colors.is_empty() {
            let diff_styles = state.diff_range_colors.iter().map(|(range, color)| {
                (
                    range.clone(),
                    HighlightStyle {
                        background_color: Some(*color),
                        ..Default::default()
                    },
                )
            });
            styles = gpui::combine_highlights(styles, diff_styles).collect();
        }

        // The internal tree-sitter branch flushes one gap-free range per
        // contiguous visible run, and folded (hidden) lines belong to no
        // range, so `combine_highlights` leaves byte gaps between runs.
        // Prepaint drops offsets (runs carry only lengths) and `layout_lines`
        // walks a contiguous offset from the first visible line — any missing
        // bytes shift every later run, painting words half-highlighted /
        // half-white past a fold. Fill the visible span so the runs list
        // always covers it exactly (the external branch is already gap-free,
        // so this is a no-op there).
        if let (Some(first), Some(last)) = (
            visible_buffer_lines.first().copied(),
            visible_buffer_lines.last().copied(),
        ) {
            let fill_start = text.line_start_offset(first);
            let fill_end = if is_multi_line {
                text.line_start_offset(last + 1)
            } else {
                text.line_end_offset(last)
            };
            if fill_end > fill_start {
                styles.sort_by(|a, b| {
                    a.0.start.cmp(&b.0.start).then(a.0.end.cmp(&b.0.end))
                });
                let input = std::mem::take(&mut styles);
                let mut filled: Vec<(Range<usize>, HighlightStyle)> =
                    Vec::with_capacity(input.len() * 2 + 1);
                let mut cursor = fill_start;
                for (range, style) in input {
                    if range.end <= fill_start || range.start >= fill_end {
                        continue;
                    }
                    let s = range.start.max(fill_start);
                    let e = range.end.min(fill_end);
                    if e <= cursor {
                        continue;
                    }
                    if s > cursor {
                        filled.push((cursor..s, HighlightStyle::default()));
                    }
                    filled.push((s..e, style));
                    cursor = e;
                }
                if cursor < fill_end {
                    filled.push((cursor..fill_end, HighlightStyle::default()));
                }
                styles = filled;
            }
        }

        if styles.is_empty() {
            None
        } else {
            Some(styles)
        }
    }
}

pub(crate) struct PrepaintState {
    /// The lines of entire lines.
    last_layout: LastLayout,
    /// The lines only contains the visible lines in the viewport, based on `visible_range`.
    ///
    /// The child is the soft lines.
    line_numbers: Option<Vec<SmallVec<[ShapedLine; 1]>>>,
    /// Size of the scrollable area by entire lines.
    scroll_size: Size<Pixels>,
    cursor_bounds: Option<Bounds<Pixels>>,
    cursor_scroll_offset: Point<Pixels>,
    /// row index (zero based), no wrap, same line as the cursor.
    current_row: Option<usize>,
    selection_path: Option<Path<Pixels>>,
    /// Search-match highlight paths: (path, is_current_match). Painted below
    /// selections so both remain visible.
    search_match_paths: Vec<(Path<Pixels>, bool)>,
    bounds: Bounds<Pixels>,
    /// Fold icon layout data
    fold_icon_layout: FoldIconLayout,
}

impl PrepaintState {
    /// Returns cursor bounds adjusted for scroll offset, if available.
    fn cursor_bounds_with_scroll(&self) -> Option<Bounds<Pixels>> {
        self.cursor_bounds.map(|mut bounds| {
            bounds.origin.y += self.cursor_scroll_offset.y;
            bounds
        })
    }
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// A debug function to print points as SVG path.
#[allow(unused)]
fn print_points_as_svg_path(
    line_corners: &Vec<Corners<Point<Pixels>>>,
    points: &Vec<Point<Pixels>>,
) {
    for corners in line_corners {
        println!(
            "tl: ({}, {}), tr: ({}, {}), bl: ({}, {}), br: ({}, {})",
            corners.top_left.x.as_f32() as i32,
            corners.top_left.y.as_f32() as i32,
            corners.top_right.x.as_f32() as i32,
            corners.top_right.y.as_f32() as i32,
            corners.bottom_left.x.as_f32() as i32,
            corners.bottom_left.y.as_f32() as i32,
            corners.bottom_right.x.as_f32() as i32,
            corners.bottom_right.y.as_f32() as i32,
        );
    }

    if points.len() > 0 {
        println!(
            "M{},{}",
            points[0].x.as_f32() as i32,
            points[0].y.as_f32() as i32
        );
        for p in points.iter().skip(1) {
            println!("L{},{}", p.x.as_f32() as i32, p.y.as_f32() as i32);
        }
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.prepare_if_needed(window, cx);

        let state = self.state.read(cx);
        let line_height = window.line_height();

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        if state.mode.is_multi_line() {
            style.flex_grow = 1.0;
            style.size.height = relative(1.).into();
            if state.mode.is_auto_grow() {
                // Auto grow to let height match to rows, but not exceed max rows.
                let rows = state.mode.rows();
                style.min_size.height = (rows * line_height).into();
            } else {
                style.min_size.height = line_height.into();
            }
        } else {
            // For single-line inputs, the minimum height should be the line height
            style.size.height = line_height.into();
        };

        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();

        let state = self.state.read(cx);
        let line_height = window.line_height();

        let (visible_range, visible_buffer_lines, visible_top) =
            self.calculate_visible_range(&state, line_height, bounds.size.height);
        let visible_start_offset = state.text.line_start_offset(visible_range.start);
        let visible_end_offset = state
            .text
            .line_end_offset(visible_range.end.saturating_sub(1));

        let highlight_styles = self.highlight_lines(
            &visible_buffer_lines,
            visible_top,
            visible_start_offset..visible_end_offset,
            cx,
        );

        let state = self.state.read(cx);
        let multi_line = state.mode.is_multi_line();
        let text = state.text.clone();
        let is_empty = text.len() == 0;
        let placeholder = self.placeholder.clone();

        let text_size = if is_empty && state.placeholder_font_size.is_some() {
            state.placeholder_font_size.unwrap()
        } else if let Some(font_size) = state.font_size {
            font_size
        } else {
            style.font_size.to_pixels(window.rem_size())
        };

        let mut text_style = window.text_style();
        let fg = cx
            .theme()
            .highlight_theme
            .style
            .editor_foreground
            .unwrap_or(text_style.color);
        text_style.color = fg;
        let (display_text, text_color) = if is_empty {
            (&Rope::from(placeholder.as_str()), cx.theme().text_muted)
        } else if state.masked {
            (
                &Rope::from(MASK_CHAR.to_string().repeat(text.chars().count())),
                fg,
            )
        } else {
            (&text, fg)
        };

        // Calculate the width of the line numbers
        let (line_number_width, line_number_len) =
            Self::layout_line_numbers(&state, &text, text_size, &text_style, window);

        let mut bounds = bounds;
        let wrap_width = if multi_line && state.soft_wrap {
            Some(bounds.size.width - line_number_width - RIGHT_MARGIN)
        } else {
            None
        };

        let visible_line_byte_offsets: Vec<usize> = visible_buffer_lines
            .iter()
            .map(|&bl| state.text.line_start_offset(bl))
            .collect();

        // For password input (masked: true), convert byte offsets to masked display byte offsets so that
        // layout_match_range and position_for_index work in the correct coordinate space.
        let (visible_line_byte_offsets, visible_range_offset) = if state.masked {
            let offsets = visible_line_byte_offsets
                .iter()
                .map(|&o| masked_display_offset(&text, o))
                .collect();
            let range_offset = masked_display_offset(&text, visible_start_offset)
                ..masked_display_offset(&text, visible_end_offset);
            (offsets, range_offset)
        } else {
            (
                visible_line_byte_offsets,
                visible_start_offset..visible_end_offset,
            )
        };

        let mut last_layout = LastLayout {
            visible_range,
            visible_buffer_lines,
            visible_line_byte_offsets,
            visible_top,
            visible_range_offset,
            line_height,
            wrap_width,
            line_number_width,
            lines: Rc::new(vec![]),
            cursor_bounds: None,
            text_align: state.text_align,
            content_width: bounds.size.width,
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked_run = TextRun {
            len: 0,
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(text_color),
                wavy: false,
            }),
            strikethrough: None,
        };

        let runs = if !is_empty {
            if let Some(highlight_styles) = highlight_styles {
                let mut runs = Vec::with_capacity(highlight_styles.len());

                runs.extend(highlight_styles.iter().map(|(range, style)| {
                    let mut run = text_style.clone().highlight(*style).to_run(range.len());
                    if let Some(ime_marked_range) = &state.ime_marked_range {
                        if range.start >= ime_marked_range.start
                            && range.end <= ime_marked_range.end
                        {
                            run.color = marked_run.color;
                            run.strikethrough = marked_run.strikethrough;
                            run.underline = marked_run.underline;
                        }
                    }

                    run
                }));

                runs.into_iter().filter(|run| run.len > 0).collect()
            } else {
                vec![run]
            }
        } else if let Some(ime_marked_range) = &state.ime_marked_range {
            // IME marked text
            vec![
                TextRun {
                    len: ime_marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: ime_marked_range.end - ime_marked_range.start,
                    underline: marked_run.underline,
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - ime_marked_range.end,
                    ..run.clone()
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        // Create shaped lines for whitespace indicators before layout
        let whitespace_indicators =
            Self::layout_whitespace_indicators(&state, text_size, &text_style, window, cx);

        let lines = Self::layout_lines(
            &state,
            &display_text,
            &last_layout,
            text_size,
            &runs,
            fg,
            whitespace_indicators,
            window,
        );

        let mut longest_line_width = wrap_width.unwrap_or(px(0.));
        // 1. Single line
        // 2. Multi-line with soft wrap disabled.
        if state.mode.is_single_line() || !state.soft_wrap {
            let longest_row = state.display_map.longest_row();
            let longest_line: SharedString = state.text.slice_line(longest_row).to_string().into();
            longest_line_width = window
                .text_system()
                .shape_line(
                    longest_line.clone(),
                    text_size,
                    &[TextRun {
                        len: longest_line.len(),
                        font: style.font(),
                        color: fg,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    wrap_width,
                )
                .width;
        }
        last_layout.lines = Rc::new(lines);

        let total_wrapped_lines = state.display_map.wrap_row_count();
        let empty_bottom_height = if state.mode.is_code_editor() {
            let base = bounds
                .size
                .height
                .half()
                .max(BOTTOM_MARGIN_ROWS * line_height);
            // `scroll_beyond_last_line`: allow scrolling extra empty rows
            // past the last line.
            match state.scroll_beyond_last_line {
                Some(rows) => base.max(rows as f32 * line_height),
                None => base,
            }
        } else {
            px(0.)
        };

        let mut scroll_size = size(
            if longest_line_width + line_number_width + RIGHT_MARGIN > bounds.size.width {
                longest_line_width + line_number_width + RIGHT_MARGIN
            } else {
                longest_line_width
            },
            (total_wrapped_lines as f32 * line_height + empty_bottom_height)
                .max(bounds.size.height),
        );

        // TODO: should be add some gap to right, to convenient to focus on boundary position
        if last_layout.text_align == TextAlign::Right || last_layout.text_align == TextAlign::Center
        {
            scroll_size.width = longest_line_width + line_number_width;
        }

        // `position_for_index` for example
        //
        // #### text
        //
        // Hello 世界，this is GPUI component.
        // The GPUI Component is a collection of UI components for
        // GPUI framework, including Button, Input, Checkbox, Radio,
        // Dropdown, Tab, and more...
        //
        // wrap_width: 444px, line_height: 20px
        //
        // #### lines[0]
        //
        // | index | pos              | line |
        // |-------|------------------|------|
        // | 5     | (37 px, 0.0)     | 0    |
        // | 38    | (261.7 px, 20.0) | 0    |
        // | 40    | None             | -    |
        //
        // #### lines[1]
        //
        // | index | position              | line |
        // |-------|-----------------------|------|
        // | 5     | (43.578125 px, 0.0)   | 0    |
        // | 56    | (422.21094 px, 0.0)   | 0    |
        // | 57    | (11.6328125 px, 20.0) | 1    |
        // | 114   | (429.85938 px, 20.0)  | 1    |
        // | 115   | (11.3125 px, 40.0)    | 2    |

        // Calculate the scroll offset to keep the cursor in view

        let (cursor_bounds, cursor_scroll_offset, current_row) =
            self.layout_cursor(&last_layout, &mut bounds, window, cx);
        last_layout.cursor_bounds = cursor_bounds;

        let selection_path = self.layout_selections(&last_layout, &mut bounds, window, cx);
        let search_match_paths = self.layout_search_matches(&last_layout, &mut bounds, cx);

        let state = self.state.read(cx);
        let line_numbers = if state.mode.line_number() {
            let mut line_numbers = Vec::with_capacity(last_layout.visible_buffer_lines.len());
            let other_line_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().text_muted,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let current_line_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().text,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];

            // build line numbers
            for (line, &buffer_line) in last_layout
                .lines
                .iter()
                .zip(last_layout.visible_buffer_lines.iter())
            {
                let line_no: SharedString = if let Some(labels) = &state.diff_line_labels {
                    match labels.get(buffer_line).copied().flatten() {
                        Some(n) => format!("{:>width$}", n + 1, width = line_number_len).into(),
                        None => " ".repeat(line_number_len).into(),
                    }
                } else {
                    format!("{:>width$}", buffer_line + 1, width = line_number_len).into()
                };

                let runs = if current_row == Some(buffer_line) {
                    &current_line_runs
                } else {
                    &other_line_runs
                };

                let mut sub_lines: SmallVec<[ShapedLine; 1]> = SmallVec::new();
                sub_lines.push(
                    window
                        .text_system()
                        .shape_line(line_no, text_size, &runs, None),
                );
                for _ in 0..line.wrapped_lines.len().saturating_sub(1) {
                    sub_lines.push(ShapedLine::default());
                }
                line_numbers.push(sub_lines);
            }
            Some(line_numbers)
        } else {
            None
        };

        let fold_icon_layout = self.layout_fold_icons(&bounds, &last_layout, window, cx);

        // Zed-style popovers: hover + completion popups are drawn as window
        // overlays (`defer_draw` with window coordinates) from the input
        // element itself. Because TextElement observes InputState, setting
        // the popup state repaints it, so the popups show up and follow the
        // cursor — no CSS-absolute coordinate juggling, no external
        // subscription needed.
        self.draw_popups(&bounds, window, cx);

        PrepaintState {
            bounds,
            last_layout,
            scroll_size,
            line_numbers,
            cursor_bounds,
            cursor_scroll_offset,
            current_row,
            selection_path,
            search_match_paths,
            fold_icon_layout,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        input_bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let state = self.state.read(cx);
        let focus_handle = state.focus_handle.clone();
        let show_cursor = state.show_cursor(window, cx);
        let focused = focus_handle.is_focused(window);
        let bounds = prepaint.bounds;
        let selected_range = state.selected_range;
        let text_align = prepaint.last_layout.text_align;

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.state.clone()),
            cx,
        );

        // Paint multi line text
        let line_height = window.line_height();
        let origin = bounds.origin;

        let invisible_top_padding = prepaint.last_layout.visible_top;
        let active_line_color = cx.theme().highlight_theme.style.editor_active_line;

        // Paint active line
        let mut offset_y = px(0.);
        if let Some(line_numbers) = prepaint.line_numbers.as_ref() {
            offset_y += invisible_top_padding;

            let diff_line_colors = &self.state.read(cx).diff_line_colors;

            // Each item is the normal lines.
            for (lines, &buffer_line) in line_numbers
                .iter()
                .zip(prepaint.last_layout.visible_buffer_lines.iter())
            {
                let is_active = prepaint.current_row == Some(buffer_line);
                let p = point(input_bounds.origin.x, origin.y + offset_y);
                let height = line_height * lines.len() as f32;
                // Paint diff line background (added/deleted) below everything else
                if let Some(bg_color) = diff_line_colors.get(&(buffer_line as u32)) {
                    window.paint_quad(fill(
                        Bounds::new(p, size(bounds.size.width, height)),
                        *bg_color,
                    ));
                }
                // Paint the current line background
                if is_active && selected_range.is_empty() {
                    if let Some(bg_color) = active_line_color {
                        window.paint_quad(fill(
                            Bounds::new(p, size(bounds.size.width, height)),
                            bg_color,
                        ));
                    }
                }
                offset_y += height;
            }
        }

        // Paint selections
        if window.is_window_active() {
            if let Some(path) = prepaint.selection_path.take() {
                window.paint_path(path, cx.theme().selection);
            }
        }

        // Paint search matches (below the text, above the background): every
        // match gets a faint selection color, the current one a stronger one.
        for (path, is_active) in prepaint.search_match_paths.iter() {
            window.paint_path(path.clone(), cx.theme().selection.opacity(0.45));
            if *is_active {
                window.paint_path(path.clone(), cx.theme().selection);
            }
        }

        // Paint text
        let mut offset_y = invisible_top_padding;

        // Keep scrollbar offset always be positive，Start from the left position
        let scroll_offset = if text_align == TextAlign::Right {
            (prepaint.scroll_size.width - prepaint.bounds.size.width).max(px(0.))
        } else if text_align == TextAlign::Center {
            (prepaint.scroll_size.width - prepaint.bounds.size.width)
                .half()
                .max(px(0.))
        } else {
            px(0.)
        };

        for (line, &_buffer_line) in prepaint
            .last_layout
            .lines
            .iter()
            .zip(prepaint.last_layout.visible_buffer_lines.iter())
        {
            let line_y = origin.y + offset_y;
            let p = point(
                origin.x + prepaint.last_layout.line_number_width + (scroll_offset),
                line_y,
            );

            // Paint the actual line
            _ = line.paint(
                p,
                line_height,
                text_align,
                Some(prepaint.last_layout.content_width),
                window,
                cx,
            );
            offset_y += line.size(line_height).height;
        }

        // Paint blinking cursor
        if focused && show_cursor {
            if let Some(cursor_bounds) = prepaint.cursor_bounds_with_scroll() {
                window.paint_quad(fill(cursor_bounds, cx.theme().accent));
            }
        }

        // Paint line numbers
        let mut offset_y = px(0.);
        if let Some(line_numbers) = prepaint.line_numbers.as_ref() {
            offset_y += invisible_top_padding;

            window.paint_quad(fill(
                Bounds {
                    origin: input_bounds.origin,
                    size: size(
                        prepaint.last_layout.line_number_width - LINE_NUMBER_RIGHT_MARGIN,
                        input_bounds.size.height,
                    ),
                },
                cx.theme().editor_background(),
            ));

            // Each item is the normal lines.
            for (lines, &buffer_line) in line_numbers
                .iter()
                .zip(prepaint.last_layout.visible_buffer_lines.iter())
            {
                let p = point(input_bounds.origin.x, origin.y + offset_y);
                let is_active = prepaint.current_row == Some(buffer_line);

                let height = line_height * lines.len() as f32;
                // paint active line number background
                if is_active && selected_range.is_empty() {
                    if let Some(bg_color) = active_line_color {
                        window.paint_quad(fill(
                            Bounds::new(p, size(prepaint.last_layout.line_number_width, height)),
                            bg_color,
                        ));
                    }
                }

                for line in lines {
                    _ = line.paint(p, line_height, TextAlign::Left, None, window, cx);
                    offset_y += line_height;
                }
            }
        }

        // Paint fold icons (only visible on hover or for current line)
        self.paint_fold_icons(
            &mut prepaint.fold_icon_layout,
            prepaint.current_row,
            window,
            cx,
        );

        self.state.update(cx, |state, cx| {
            state.last_layout = Some(prepaint.last_layout.clone());
            state.last_bounds = Some(bounds);
            state.last_cursor = Some(state.cursor());
            state.set_input_bounds(input_bounds, cx);
            state.last_selected_range = Some(selected_range);
            state.scroll_size = prepaint.scroll_size;
            state.update_scroll_offset(Some(prepaint.cursor_scroll_offset), cx);
            state.deferred_scroll_offset = None;

            cx.notify();
        });

        self.paint_mouse_listeners(window, cx);
    }
}

/// Get the runs for the given range.
///
/// The range is the byte range of the wrapped line.
pub(super) fn runs_for_range(
    runs: &[TextRun],
    line_offset: usize,
    range: &Range<usize>,
) -> Vec<TextRun> {
    let mut result = vec![];
    let range = (line_offset + range.start)..(line_offset + range.end);
    let mut cursor = 0;

    for run in runs {
        let run_start = cursor;
        let run_end = cursor + run.len;

        if run_end <= range.start {
            cursor = run_end;
            continue;
        }

        if run_start >= range.end {
            break;
        }

        let start = range.start.max(run_start) - run_start;
        let end = range.end.min(run_end) - run_start;
        let len = end - start;

        if len > 0 {
            result.push(TextRun { len, ..run.clone() });
        }

        cursor = run_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runs_for_range() {
        let run = TextRun {
            len: 0,
            font: gpui::font(".SystemUIFont"),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // use hello this-is-test
        let runs = vec![
            // use
            TextRun {
                len: 3,
                ..run.clone()
            },
            // \s
            TextRun {
                len: 1,
                ..run.clone()
            },
            // hello
            TextRun {
                len: 5,
                ..run.clone()
            },
            // \s
            TextRun {
                len: 1,
                ..run.clone()
            },
            // this-is-test
            TextRun {
                len: 12,
                ..run.clone()
            },
        ];

        #[track_caller]
        fn assert_runs(actual: Vec<TextRun>, expected: &[usize]) {
            let left = actual.iter().map(|run| run.len).collect::<Vec<_>>();
            assert_eq!(left, expected);
        }

        assert_runs(runs_for_range(&runs, 0, &(0..0)), &[]);
        assert_runs(runs_for_range(&runs, 0, &(0..100)), &[3, 1, 5, 1, 12]);

        assert_runs(runs_for_range(&runs, 0, &(0..6)), &[3, 1, 2]);
        assert_runs(runs_for_range(&runs, 0, &(1..6)), &[2, 1, 2]);
        assert_runs(runs_for_range(&runs, 0, &(3..10)), &[1, 5, 1]);
        assert_runs(runs_for_range(&runs, 0, &(5..8)), &[3]);
        assert_runs(runs_for_range(&runs, 3, &(0..3)), &[1, 2]);
        assert_runs(runs_for_range(&runs, 3, &(2..10)), &[4, 1, 3]);
        assert_runs(runs_for_range(&runs, 9, &(0..8)), &[1, 7]);
    }
}
