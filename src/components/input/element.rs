use std::{ops::Range, rc::Rc};

use gpui::{
    App, Bounds, Corners, CursorStyle, Element, ElementId, ElementInputHandler, Entity,
    GlobalElementId, Half, HighlightStyle, Hitbox, HitboxBehavior, Hsla, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Path, Pixels, Point, ShapedLine,
    SharedString, Size, Style, TextAlign, TextRun, TextStyle, UnderlineStyle, Window, fill, point,
    px, relative, size,
};
use ropey::Rope;
use smallvec::SmallVec;

use crate::{
    components::button::Size as ButtonSize,
    components::input::{RopeExt as _, blink_cursor::CURSOR_WIDTH, display_map::LineLayout},
    theme::ActiveTheme as _,
};

use super::{
    GitLineMark, InputState, LastLayout, RainbowCache, ScrollbarGeometry, WhitespaceIndicators,
    mode::InputMode,
    state::{
        RAINBOW_BRACKETS_MAX_TEXT_LEN, is_code_bracket, scan_bracket_spans_text,
        scan_bracket_spans_tree,
    },
};

/// Deterministic replacement for `gpui::combine_highlights`.
/// The original uses `FxHashSet` iteration, which is non-deterministic and
/// caused paired brackets (`{}`, `()`) to get different colors. `overlay`
/// is applied after `base` and wins on overlap.
fn combine_highlights_stable(
    base: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    overlay: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut highlights = Vec::new();
    let mut endpoints = Vec::new(); // (position, is_start, id)
    for (range, style) in base.into_iter().chain(overlay) {
        if range.is_empty() {
            continue;
        }
        let id = highlights.len();
        endpoints.push((range.start, true, id));
        endpoints.push((range.end, false, id));
        highlights.push(style);
    }
    if endpoints.is_empty() {
        return Vec::new();
    }
    // Ends (false) before starts (true) at the same position.
    endpoints.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut active = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut i = 0;
    while i < endpoints.len() {
        let pos = endpoints[i].0;
        if pos > cursor && !active.is_empty() {
            let style = active.iter().fold(HighlightStyle::default(), |acc, &id| {
                acc.highlight(highlights[id])
            });
            out.push((cursor..pos, style));
        }
        while i < endpoints.len() && endpoints[i].0 == pos {
            let (_, is_start, id) = endpoints[i];
            if is_start {
                active.insert(id);
            } else {
                active.remove(&id);
            }
            i += 1;
        }
        cursor = pos;
    }
    out
}

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

/// Zed's fold disclosure icons (`assets/icons/chevron_down.svg` /
/// `chevron_right.svg` in the zed repo, copied verbatim into DeepTerm's
/// assets). Rendered via `window.paint_svg` with the muted text color,
/// matching zed's `ui::Disclosure` (IconButton + `IconName::ChevronDown` /
/// `ChevronRight`, `IconSize::Small`, `Color::Muted`).
const FOLD_ICON_OPEN: &str = "icons/chevron_down.svg";
const FOLD_ICON_CLOSED: &str = "icons/chevron_right.svg";

/// Size of the fold disclosure icon, matching zed's `IconSize::Small`
/// (14px) constrained by the crease-toggle available height
/// (`line_height * 0.55`, see zed's `prepaint_crease_toggles`).
fn fold_icon_size(line_height: Pixels) -> Pixels {
    px(14.).min(line_height * 0.55)
}

/// Horizontal padding inside the `⋯` placeholder pill, approximating zed's
/// `FoldPlaceholder::fold_element` (`size_full` rounded background around
/// the ellipsis).
const FOLD_PLACEHOLDER_PADDING: Pixels = px(2.);

/// Layout information for a gutter fold disclosure icon (zed's crease toggle).
struct FoldGutterIcon {
    /// The buffer row the icon toggles.
    row: usize,
    is_folded: bool,
    bounds: Bounds<Pixels>,
    hitbox: Hitbox,
}

/// A shaped `⋯` placeholder plus the fold's trailing text (e.g. `}`),
/// painted inline at the end of the fold's header line (zed's fold
/// placeholder + the text after the fold range on the same display row).
struct FoldPlaceholder {
    /// The buffer row of the fold header.
    row: usize,
    /// Bounds of the `⋯` pill (window space), flush with the end of the
    /// header line's last wrapped row; computed after the cursor layout.
    pill_bounds: Option<Bounds<Pixels>>,
    /// Hitbox of the pill (pointing-hand cursor + click to unfold).
    hitbox: Option<Hitbox>,
    /// Shaped `⋯` (U+22EF).
    ellipsis: ShapedLine,
    /// The text after the fold end on the closing row (e.g. `}`), with syntax
    /// highlighting; empty when the fold has no closing row tail.
    tail: Option<ShapedLine>,
}

impl FoldPlaceholder {
    /// Total width of the placeholder pill (background + `⋯`).
    fn pill_width(&self) -> Pixels {
        self.ellipsis.width + FOLD_PLACEHOLDER_PADDING * 2.
    }
}

/// Fold-related layout data for one frame (gutter icons + inline placeholders).
struct FoldIconLayout {
    /// Hitbox for the gutter area (used for hover detection, zed's `gutter_hovered`).
    line_number_hitbox: Hitbox,
    /// Fold disclosure icons for every visible foldable row.
    icons: Vec<FoldGutterIcon>,
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
        // Sticky scroll 点击：命中钉性行 → 滚动到该定义行
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseDownEvent, _, _window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }
                let targets = state.read(cx).sticky_click_targets.clone();
                if let Some((buffer_line, _)) = targets
                    .iter()
                    .find(|(_, bounds)| bounds.contains(&event.position))
                {
                    let buffer_line = *buffer_line;
                    state.update(cx, |state, cx| {
                        let offset = state.text.line_start_offset(buffer_line);
                        state.scroll_to_center(offset, cx);
                    });
                    cx.stop_propagation();
                }
            }
        });

        // 滚动条拖拽：按下命中 thumb → 拖动改 scroll offset → 松开结束
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseDownEvent, _, window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }
                let geometry = state.read(cx).scrollbar_geometry;
                if let Some(geometry) = geometry {
                    let hit = Bounds::new(
                        geometry.thumb.origin - point(px(3.), px(4.)),
                        geometry.thumb.size + size(px(6.), px(8.)),
                    );
                    if hit.contains(&event.position) {
                        state.update(cx, |state, cx| {
                            state.scrollbar_drag =
                                Some((event.position.y, -state.scroll_handle.offset().y));
                            cx.notify();
                        });
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }
            }
        });
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseMoveEvent, _, _window, cx| {
                let Some((start_y, start_scroll)) = state.read(cx).scrollbar_drag else {
                    return;
                };
                let Some(geometry) = state.read(cx).scrollbar_geometry else {
                    return;
                };
                let max_scroll = geometry.content_height - geometry.track_height;
                let scrollable_thumb = geometry.track_height - geometry.thumb_height;
                if scrollable_thumb <= px(0.) || max_scroll <= px(0.) {
                    return;
                }
                let delta = event.position.y - start_y;
                let new_scroll = (start_scroll + delta * (max_scroll / scrollable_thumb))
                    .clamp(px(0.), max_scroll);
                state.update(cx, |state, cx| {
                    let mut offset = state.scroll_handle.offset();
                    offset.y = -new_scroll;
                    state.scroll_handle.set_offset(offset);
                    cx.notify();
                });
            }
        });
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseUpEvent, _, _window, cx| {
                if event.button == MouseButton::Left && state.read(cx).scrollbar_drag.is_some() {
                    state.update(cx, |state, cx| {
                        state.scrollbar_drag = None;
                        cx.notify();
                    });
                }
            }
        });

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
                    InputState::dispatch_background_parse(bg, state.text_version, window, cx);
                }
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
        fold_placeholders: &[FoldPlaceholder],
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
        let top_bottom_margin =
            if state.mode.is_auto_grow() || visible_range.len() < BOTTOM_MARGIN_ROWS * 8 {
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
                //
                // 本行"覆盖"某个偏移，当且仅当它落在
                // `[行首, 行尾含换行符)` 之内。此前判据写成
                // `prev_lines_offset >= target`（取第一个行首 ≥ 目标的行），
                // 命中的其实是光标所在行的**下一行**；更糟的是当光标落在折叠
                // 的最后一个隐藏行时，那一行不满足判据，循环会一直走到折叠后
                // 的第一个可见行，而可见分支用的是 `saturating_sub`，任何早于
                // 该行的光标都会被算成它的第 0 列 —— 当前行高亮因此跳出折叠，
                // 落在折叠之后那一行上。
                let contains_offset = |target: usize| {
                    target >= prev_lines_offset && target < prev_lines_offset + wrap_line.len() + 1
                };
                if contains_offset(cursor) && cursor_pos.is_none() {
                    current_row = Some(row);
                    // A cursor inside a fold — or at its end (the visible tail,
                    // e.g. `}`) — is drawn on the fold header's display row:
                    // at the `⋯` placeholder, or right after the placeholder
                    // and tail when at the fold end (zed maps positions inside
                    // a fold to the placeholder).
                    let display_fold = state
                        .display_map
                        .fold_containing(cursor)
                        .or_else(|| state.display_map.fold_ending_at(cursor));
                    cursor_pos = display_fold.and_then(|fold| {
                        let line = last_layout.line(fold.start.row)?;
                        let placeholder =
                            fold_placeholders.iter().find(|p| p.row == fold.start.row)?;
                        let last_y = offset_y - line_height;
                        let last_width =
                            line.wrapped_lines.last().map(|l| l.width).unwrap_or(px(0.));
                        let x = if cursor >= fold.end_offset {
                            let mut x = last_width
                                + placeholder.ellipsis.width
                                + FOLD_PLACEHOLDER_PADDING * 2.;
                            if let Some(tail) = placeholder.tail.as_ref() {
                                x += tail.width;
                            }
                            x
                        } else {
                            last_width
                        };
                        Some(point(x, last_y))
                    });
                    if cursor_pos.is_none() {
                        cursor_pos = Some(line_origin);
                    }
                }
                // 与 `cursor` 同一套包含判据，选区两端才不会各自偏到下一行。
                if contains_offset(selected_range.start) && cursor_start.is_none() {
                    cursor_start = Some(line_origin);
                }
                if contains_offset(selected_range.end) && cursor_end.is_none() {
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

        // zed tracks the "active row" in display space: when the cursor sits
        // on a row hidden by a fold (e.g. at the fold end, on the closing
        // bracket row), the fold's header row is the active one — it receives
        // the active-line highlight and shows the fold disclosure icon.
        let current_row =
            current_row.map(|row| state.display_map.fold_header_for_row(row).unwrap_or(row));

        bounds.origin += scroll_offset;

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
                builder.move_to(path_origin + point(curr.x - px(ux * r), curr.y - px(uy * r)));
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
                    let start = point(curr.x - px(ux_in * r), curr.y - px(uy_in * r));

                    let dx_out = f32::from(next.x) - f32::from(curr.x);
                    let dy_out = f32::from(next.y) - f32::from(curr.y);
                    let len_out = (dx_out * dx_out + dy_out * dy_out).sqrt();
                    if len_out > 0. {
                        let ux_out = dx_out / len_out;
                        let uy_out = dy_out / len_out;
                        let end = point(curr.x + px(ux_out * r), curr.y + px(uy_out * r));

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

        Self::layout_match_range(range, last_layout, bounds)
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

    /// 括号着色调色板：按嵌套层级循环取色，暗/亮主题各一套。
    fn rainbow_bracket_color(depth: usize, dark: bool) -> Hsla {
        let hex: u32 = if dark {
            match depth % 6 {
                0 => 0xE2B93D, // 金
                1 => 0x4FC1FF, // 蓝
                2 => 0xE06FB8, // 粉
                3 => 0x61D4B8, // 青
                4 => 0xC88B48, // 橙
                _ => 0xA78BFA, // 紫
            }
        } else {
            match depth % 6 {
                0 => 0xB8860B,
                1 => 0x0366D6,
                2 => 0xC4529E,
                3 => 0x0F9B8E,
                4 => 0xB26A1B,
                _ => 0x7C5CBF,
            }
        };
        gpui::rgb(hex).into()
    }

    /// Sticky scroll 布局：首可见行的外围定义行单独 shape（带语法色），
    /// 返回外→内序的钉性行。点击区域同时计算好供 paint 阶段注册。
    #[allow(clippy::too_many_arguments)]
    fn layout_sticky_lines(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        base_style: &TextStyle,
        text_size: Pixels,
        line_height: Pixels,
        line_number_len: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<StickyLine> {
        let (sticky_rows, text, highlighter, highlight_theme) = {
            let state = self.state.read(cx);
            let sticky_rows = state.sticky_lines(last_layout.visible_range.start);
            (
                sticky_rows,
                state.text.clone(),
                match &state.mode {
                    InputMode::CodeEditor { highlighter, .. } => highlighter.clone(),
                    _ => return Vec::new(),
                },
                cx.theme().highlight_theme.clone(),
            )
        };
        if sticky_rows.is_empty() {
            return Vec::new();
        }

        // 高度限制：至少留出一行内容可见
        let available = bounds.size.height - line_height;
        let fit_rows = ((available / line_height).floor().max(0.)) as usize;
        let sticky_rows = if sticky_rows.len() > fit_rows {
            // 保留最内侧（最靠近可见区）的行
            let keep = sticky_rows.len() - fit_rows;
            sticky_rows[keep..].to_vec()
        } else {
            sticky_rows
        };
        if sticky_rows.is_empty() {
            return Vec::new();
        }

        let number_runs = |color: Hsla| {
            vec![TextRun {
                len: line_number_len,
                font: base_style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }]
        };
        let number_muted_runs = number_runs(cx.theme().text_muted);

        let mut sticky_lines = Vec::with_capacity(sticky_rows.len());
        for (ix, &buffer_line) in sticky_rows.iter().enumerate() {
            let line_start = text.line_start_offset(buffer_line);
            let line_end = text.line_end_offset(buffer_line);
            let mut line_text: String = text.slice(line_start..line_end).into();
            // 过长截断（按字符，避免拆 UTF-8）
            if line_text.chars().count() > 200 {
                line_text = line_text.chars().take(200).collect();
            }
            let text_bytes = line_text.len();

            // 语法高亮 runs（绝对范围 → 行内相对）
            let mut runs: Vec<TextRun> = Vec::new();
            {
                let mut hl = highlighter.borrow_mut();
                if let Some(hl) = hl.as_mut() {
                    for (range, style) in hl.styles(&(line_start..line_end), &highlight_theme) {
                        let start = range.start.max(line_start) - line_start;
                        let end = (range.end.min(line_start + text_bytes)) - line_start;
                        if end > start {
                            runs.push(base_style.clone().highlight(style).to_run(end - start));
                        }
                    }
                }
            }
            if runs.is_empty() {
                runs.push(base_style.clone().to_run(text_bytes));
            }
            let runs_len: usize = runs.iter().map(|r| r.len).sum();
            if runs_len < text_bytes {
                runs.push(base_style.clone().to_run(text_bytes - runs_len));
            }

            let shaped_text = window.text_system().shape_line(
                SharedString::from(line_text),
                text_size,
                &runs,
                None,
            );
            let number: SharedString =
                format!("{:>width$}", buffer_line + 1, width = line_number_len).into();
            let shaped_number =
                window
                    .text_system()
                    .shape_line(number, text_size, &number_muted_runs, None);

            let row_bounds = Bounds::new(
                point(bounds.origin.x, bounds.origin.y + line_height * ix as f32),
                size(bounds.size.width, line_height),
            );
            sticky_lines.push(StickyLine {
                buffer_line,
                text: shaped_text,
                number: shaped_number,
                bounds: row_bounds,
            });
        }
        sticky_lines
    }

    /// 括号着色：把可见范围内的括号字符按嵌套层级覆盖为 rainbow 色。
    ///
    /// 优先用 tree-sitter 树扫描（字符串/注释内的括号不参与计数，避免深度被污染），
    /// 无树时退回纯文本扫描；配对算法一致：类型配对 + 交错容忍，未配对标错误色。
    /// 结果按 (text_version, 树指纹) 缓存，树后台重解析落地后自动失效。
    fn merge_rainbow_brackets(
        &self,
        styles: Option<Vec<(Range<usize>, HighlightStyle)>>,
        visible: &Range<usize>,
        cx: &mut App,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        let (enabled, text, version, highlighter) = {
            let state = self.state.read(cx);
            (
                state.rainbow_brackets
                    && state.mode.is_multi_line()
                    && !state.masked
                    && state.external_highlight_runs.is_none(),
                state.text.clone(),
                state.text_version,
                match &state.mode {
                    InputMode::CodeEditor { highlighter, .. } => Some(highlighter.clone()),
                    _ => None,
                },
            )
        };
        if !enabled || text.len() > RAINBOW_BRACKETS_MAX_TEXT_LEN {
            return styles;
        }
        let mut styles = styles?;

        // 树指纹：树被替换（后台解析落地/换语言）时变化，缓存随之失效
        let tree_key = highlighter.as_ref().and_then(|h| {
            h.borrow()
                .as_ref()
                .and_then(|h| h.tree().map(|t| t.root_node().id()))
        });
        let cached = {
            let state = self.state.read(cx);
            match &state.rainbow_cache {
                Some(cache)
                    if cache.text_version == version && cache.tree_fingerprint == tree_key =>
                {
                    Some(cache.spans.clone())
                }
                _ => None,
            }
        };
        let spans = match cached {
            Some(spans) => spans,
            None => {
                let computed = match highlighter
                    .as_ref()
                    .and_then(|h| h.borrow().as_ref().and_then(|h| h.tree().cloned()))
                {
                    Some(tree) => scan_bracket_spans_tree(&tree),
                    None => scan_bracket_spans_text(&text),
                };
                let computed = std::rc::Rc::new(computed);
                self.state.update(cx, |state, _| {
                    state.rainbow_cache = Some(RainbowCache {
                        text_version: version,
                        tree_fingerprint: tree_key,
                        spans: computed.clone(),
                    });
                });
                computed
            }
        };

        let dark = cx.theme().appearance.is_dark();
        let error_color = cx.theme().error;
        let rainbow: Vec<(Range<usize>, HighlightStyle)> = spans
            .iter()
            .filter(|(r, _, _, _)| r.start >= visible.start && r.end <= visible.end)
            .map(|(r, d, is_open, matched)| {
                // 游离闭括号标错误色；未闭合的开括号保持层级色（避免输入时闪红）
                let color = if *matched || *is_open {
                    Self::rainbow_bracket_color(*d, dark)
                } else {
                    error_color
                };
                (
                    r.clone(),
                    HighlightStyle {
                        color: Some(color),
                        ..Default::default()
                    },
                )
            })
            .collect();
        if rainbow.is_empty() {
            return Some(styles);
        }
        styles = combine_highlights_stable(styles, rainbow);
        Some(styles)
    }

    /// 同词高亮：光标处词的全文件出现，返回可见范围内的背景路径。
    fn layout_occurrences(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<Path<Pixels>> {
        let focused = self.state.read(cx).focus_handle.is_focused(window);
        if !focused {
            return Vec::new();
        }
        let ranges = self.state.update(cx, |state, _| state.occurrence_ranges());
        let Some(ranges) = ranges else {
            return Vec::new();
        };
        let mut paths = Vec::with_capacity(ranges.len());
        for range in ranges.iter() {
            if let Some(path) = Self::layout_match_range(range.clone(), last_layout, bounds) {
                paths.push(path);
            }
        }
        paths
    }

    /// 布局括号匹配高亮：光标紧邻括号时，为开/闭括号各生成一个背景路径。
    fn layout_bracket_matches(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        window: &Window,
        cx: &mut App,
    ) -> Vec<Path<Pixels>> {
        let (is_single, is_masked, has_sel, focused, cursor, text, tree) = {
            let state = self.state.read(cx);
            let tree = match &state.mode {
                InputMode::CodeEditor { highlighter, .. } => highlighter
                    .borrow()
                    .as_ref()
                    .and_then(|h| h.tree().cloned()),
                _ => None,
            };
            (
                state.mode.is_single_line(),
                state.masked,
                !state.selected_range.is_empty(),
                state.focus_handle.is_focused(window),
                state.cursor(),
                state.text.clone(),
                tree,
            )
        };
        if is_single || is_masked {
            return Vec::new();
        }
        if has_sel {
            return Vec::new();
        }
        if !focused {
            return Vec::new();
        }
        let Some((open, close)) = InputState::matching_bracket(&text, cursor) else {
            return Vec::new();
        };
        // 有语法树时过滤字符串/注释内的误匹配（纯字节扫描无法区分）。
        if let Some(tree) = tree.as_ref() {
            let ok = [open, close].iter().all(|off| {
                text.get_byte(*off)
                    .is_some_and(|b| is_code_bracket(tree, *off, b))
            });
            if !ok {
                return Vec::new();
            }
        }
        let mut paths = Vec::with_capacity(2);
        for off in [open, close] {
            if let Some(path) = Self::layout_match_range(off..off + 1, last_layout, bounds) {
                paths.push(path);
            }
        }
        paths
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

    /// Layout the gutter fold disclosure icons (zed's crease toggles).
    ///
    /// Mirrors zed's `EditorElement::layout_crease_toggles`: every visible
    /// row that is folded or starts an indent gets an icon hitbox in the
    /// fold area at the right of the line numbers; whether the icon is
    /// painted is decided in `paint_fold_icons` following zed's
    /// `render_crease_toggle` visibility rule.
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

        let mut icon_layout = FoldIconLayout {
            line_number_hitbox,
            icons: vec![],
        };

        let state = self.state.read(cx);
        if !state.mode.is_folding() {
            return icon_layout;
        }

        let line_height = last_layout.line_height;
        let icon_size = fold_icon_size(line_height);
        let mut offset_y = last_layout.visible_top;
        for (i, &buffer_line) in last_layout.visible_buffer_lines.iter().enumerate() {
            let height = last_layout
                .lines
                .get(i)
                .map(|line| line.size(line_height).height)
                .unwrap_or(line_height);

            // zed: `is_foldable |= !use_lsp_folding_ranges && starts_indent(row)`.
            let folded = state.display_map.is_line_folded(buffer_line);
            let is_foldable = folded || state.display_map.starts_indent(&state.text, buffer_line);
            if is_foldable {
                // Center the icon in the fold area (zed centers the crease
                // toggle within `gutter_dimensions.fold_area_width()`), on the
                // row's first display line (zed's `prepaint_crease_toggles`
                // positions toggles at `line_height * display_row`).
                let icon_x = bounds.origin.x + last_layout.line_number_width
                    - FOLD_ICON_HITBOX_WIDTH
                    + (FOLD_ICON_HITBOX_WIDTH - icon_size).half();
                let icon_y = bounds.origin.y + offset_y + (line_height - icon_size).half();
                let icon_bounds = Bounds::new(point(icon_x, icon_y), size(icon_size, icon_size));
                let hitbox = window.insert_hitbox(
                    Bounds::new(
                        point(
                            bounds.origin.x + last_layout.line_number_width
                                - FOLD_ICON_HITBOX_WIDTH,
                            bounds.origin.y + offset_y,
                        ),
                        size(FOLD_ICON_HITBOX_WIDTH, line_height),
                    ),
                    HitboxBehavior::Normal,
                );
                icon_layout.icons.push(FoldGutterIcon {
                    row: buffer_line,
                    is_folded: folded,
                    bounds: icon_bounds,
                    hitbox,
                });
            }
            offset_y += height;
        }

        icon_layout
    }

    /// Shape the `⋯` placeholders and trailing texts for every visible folded
    /// row (zed's fold placeholder is rendered inline where the folded range
    /// was, and the text after the fold end — e.g. the closing `}` — stays
    /// visible on the same display line).
    fn layout_fold_placeholders(
        &self,
        last_layout: &LastLayout,
        text_style: &TextStyle,
        font_size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<FoldPlaceholder> {
        let state = self.state.read(cx);
        if !state.mode.is_folding() || state.text.len() == 0 {
            return Vec::new();
        }

        let text = state.text.clone();
        let mut placeholders = Vec::new();

        let placeholder_color = cx.theme().text_muted;
        let ellipsis_text: SharedString = "⋯".into();
        let ellipsis = window.text_system().shape_line(
            ellipsis_text.clone(),
            font_size,
            &[TextRun {
                len: ellipsis_text.len(),
                font: text_style.font(),
                color: placeholder_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );

        let mut highlighter = state.mode.highlighter().map(|h| h.borrow_mut());
        let highlight_theme = cx.theme().highlight_theme.clone();

        for &buffer_line in last_layout.visible_buffer_lines.iter() {
            let Some(fold) = state.display_map.fold_for_row(buffer_line) else {
                continue;
            };

            // The text after the fold end on the closing row (usually a `}`).
            //
            // `fold.end_offset` is `line_start(end.row) + end.column`, and the
            // old tail_end recomputed exactly that value — so the two were
            // always equal and the tail was never painted (folds rendered as
            // `fn main() {⋯` with the closing brace missing). Take the end of
            // the closing row's *content* instead: that trailing text is what
            // has to stay visible next to the `⋯` placeholder.
            let tail_start = fold.end_offset;
            let tail_end = text.line_content_end_offset(fold.end.row);
            let tail = if tail_end > tail_start {
                let tail_text: SharedString = text.slice(tail_start..tail_end).to_string().into();
                let tail_runs: Vec<TextRun> = if let Some(highlighter) =
                    highlighter.as_deref_mut().and_then(|h| h.as_mut())
                {
                    highlighter
                        .styles(&(tail_start..tail_end), &highlight_theme)
                        .into_iter()
                        .map(|(range, style)| {
                            let len = range.len().min(tail_text.len());
                            text_style.clone().highlight(style).to_run(len)
                        })
                        .filter(|run| run.len > 0)
                        .collect()
                } else {
                    Vec::new()
                };
                let runs = if tail_runs.is_empty() {
                    vec![TextRun {
                        len: tail_text.len(),
                        font: text_style.font(),
                        color: text_style.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }]
                } else {
                    tail_runs
                };
                Some(
                    window
                        .text_system()
                        .shape_line(tail_text, font_size, &runs, None),
                )
            } else {
                None
            };

            placeholders.push(FoldPlaceholder {
                row: buffer_line,
                pill_bounds: None,
                hitbox: None,
                ellipsis: ellipsis.clone(),
                tail,
            });
        }

        placeholders
    }

    /// Compute each placeholder pill's bounds and hitbox. Must run after the
    /// cursor layout (which scrolls `bounds`) so the pills land where the
    /// header line is painted. The pill sits at the end of the header line's
    /// last wrapped row, flush with the line's text (zed renders the
    /// placeholder inline exactly where the folded range was).
    fn layout_fold_placeholder_bounds(
        &self,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        placeholders: &mut [FoldPlaceholder],
        window: &mut Window,
        cx: &App,
    ) {
        let state = self.state.read(cx);
        if !state.mode.is_folding() {
            return;
        }

        let line_height = last_layout.line_height;
        let mut offset_y = last_layout.visible_top;
        for (i, &buffer_line) in last_layout.visible_buffer_lines.iter().enumerate() {
            let height = last_layout
                .lines
                .get(i)
                .map(|line| line.size(line_height).height)
                .unwrap_or(line_height);

            if state.display_map.is_line_folded(buffer_line) {
                if let Some(placeholder) = placeholders.iter_mut().find(|p| p.row == buffer_line) {
                    let line = last_layout.lines.get(i);
                    let wrap_count = line.map(|l| l.wrapped_lines.len()).unwrap_or(1).max(1);
                    let last_width = line
                        .and_then(|l| l.wrapped_lines.last())
                        .map(|l| l.width)
                        .unwrap_or(px(0.));
                    let pill_bounds = Bounds::new(
                        point(
                            bounds.origin.x + last_layout.line_number_width + last_width,
                            bounds.origin.y + offset_y + line_height * (wrap_count as f32 - 1.),
                        ),
                        size(placeholder.pill_width(), line_height),
                    );
                    let hitbox = window.insert_hitbox(pill_bounds, HitboxBehavior::Normal);
                    placeholder.pill_bounds = Some(pill_bounds);
                    placeholder.hitbox = Some(hitbox);
                }
            }

            offset_y += height;
        }
    }

    /// Paint the gutter fold disclosure icons (zed's crease toggles).
    ///
    /// Follows zed's `EditorSnapshot::render_crease_toggle` visibility rule:
    /// the icon is shown when the row is folded, or when it is foldable and
    /// either contains the cursor or the gutter is hovered. The icon is a
    /// chevron (right when folded, down when expanded) in the muted text
    /// color with a pointing-hand cursor, exactly like zed's `ui::Disclosure`.
    fn paint_fold_icons(
        &mut self,
        fold_icon_layout: &mut FoldIconLayout,
        current_row: Option<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<(usize, Bounds<Pixels>)> {
        let is_hovered = fold_icon_layout.line_number_hitbox.is_hovered(window);
        let icon_color = cx.theme().text_muted;
        let mut click_targets = Vec::new();
        for icon in &fold_icon_layout.icons {
            let is_current_line = current_row == Some(icon.row);
            if !icon.is_folded && !is_hovered && !is_current_line {
                // zed: `if folded || (is_foldable && (row_contains_cursor ||
                // gutter_hovered))` — otherwise no icon is rendered.
                continue;
            }

            click_targets.push((icon.row, icon.hitbox.bounds));
            let path = if icon.is_folded {
                FOLD_ICON_CLOSED
            } else {
                FOLD_ICON_OPEN
            };
            let _ = window.paint_svg(
                icon.bounds,
                SharedString::new_static(path),
                None,
                gpui::TransformationMatrix::default(),
                icon_color,
                cx,
            );
            window.set_cursor_style(CursorStyle::PointingHand, &icon.hitbox);
        }
        click_targets
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
                runs,
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
                        runs,
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
        let first_line_start =
            display_text.line_start_offset(*last_layout.visible_buffer_lines.first().unwrap_or(&0));

        for &buffer_line in last_layout.visible_buffer_lines.iter() {
            let line_text: String = display_text.slice_line(buffer_line).into();
            // `buffer_lines` 与 `display_text` 偶尔会失同步（折叠/编辑时序），
            // 用 `expect` 会让整个编辑器在绘制期 panic。降级为跳过该行：少画
            // 一行远好过整个应用崩溃。
            let Some(line_item) = buffer_lines.get(buffer_line) else {
                log::error!(
                    "layout_lines: buffer_line {buffer_line} missing from wrapper \
                     (wrapper_len={}, visible={:?}, text_len={})",
                    buffer_lines.len(),
                    last_layout.visible_buffer_lines,
                    display_text.len()
                );
                continue;
            };

            debug_assert_eq!(line_item.len(), line_text.len());

            let run_offset = display_text.line_start_offset(buffer_line) - first_line_start;
            let mut wrapped_lines = SmallVec::with_capacity(1);

            for range in &line_item.wrapped_lines {
                let line_runs = runs_for_range(runs, run_offset, range);

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
                styles.push((start..end, *style));
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

                    // 各可见段严格递增且互不重叠（超长行与折叠会切段），
                    // 所以无需把已累积的样式一起重排 —— 只需归并本段内部
                    // （tree-sitter 的 captures 天然嵌套重叠），再追加到尾部。
                    // 此前写的是 `combine(styles.clone(), range_styles)`，
                    // 每段都对全部累积样式排序一次，段数一多就退化成
                    // O(K · N log N)。
                    styles.extend(combine_highlights_stable(
                        Vec::<(Range<usize>, HighlightStyle)>::new(),
                        range_styles,
                    ));
                };

            // Group contiguous visible lines into ranges and call styles() once per range
            let mut visible_iter = visible_buffer_lines.iter().peekable();
            let mut range_start: Option<usize> = None;

            while let Some(&line) = visible_iter.next() {
                // Check if this line is too long for highlighting; use content length without terminator
                let line_len = text.line_content(line).len();
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
                // 关键防御：range_start 理论上恒为 Some（上一行刚 get_or_insert），
                // 但 paint 路径不得 unwrap，异常时跳过该段高亮而非崩溃整帧。
                let Some(start_line) = range_start.take() else {
                    continue;
                };
                flush_range(start_line, line, false, &mut styles);
            }

            let diagnostic_styles = diagnostics.styles_for_range(&visible_byte_range, cx);
            styles = combine_highlights_stable(diagnostic_styles, styles);
        }
        drop(highlighter_ref);

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
            styles = combine_highlights_stable(styles, diff_styles);
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
                styles.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.0.end.cmp(&b.0.end)));
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

/// Sticky scroll 单行：顶部钉住的定义行。
pub(crate) struct StickyLine {
    /// buffer 行号（0 起）。
    pub(crate) buffer_line: usize,
    /// 已 shape 的文本（含语法色）。
    pub(crate) text: ShapedLine,
    /// 已 shape 的行号。
    pub(crate) number: ShapedLine,
    /// 整行命中区域（窗口坐标，用于点击跳转）。
    pub(crate) bounds: Bounds<Pixels>,
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
    /// 括号匹配高亮路径（光标紧邻括号时的开/闭括号背景）。
    bracket_paths: Vec<Path<Pixels>>,
    /// 同词高亮路径（光标处词的全文件出现）。
    occurrence_paths: Vec<Path<Pixels>>,
    /// Sticky scroll 行：(buffer 行号， 文本布局， 点击区域)。
    sticky_lines: Vec<StickyLine>,
    bounds: Bounds<Pixels>,
    /// Fold icon layout data (gutter icons + inline placeholders).
    fold_icon_layout: FoldIconLayout,
    /// Shaped `⋯` placeholders + trailing texts for visible folded rows.
    fold_placeholders: Vec<FoldPlaceholder>,
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

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    crate::impl_source_location_none!();

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
            self.calculate_visible_range(state, line_height, bounds.size.height);
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
        // 括号着色：按嵌套层级给可见括号分色（覆盖语法色，最后合并）
        let highlight_styles = self.merge_rainbow_brackets(
            highlight_styles,
            &(visible_start_offset..visible_end_offset),
            cx,
        );

        let state = self.state.read(cx);
        let multi_line = state.mode.is_multi_line();
        let text = state.text.clone();
        let is_empty = text.len() == 0;
        let placeholder = self.placeholder.clone();

        let text_size = match (is_empty, state.placeholder_font_size, state.font_size) {
            (true, Some(placeholder_font_size), _) => placeholder_font_size,
            (_, _, Some(font_size)) => font_size,
            _ => style.font_size.to_pixels(window.rem_size()),
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
            Self::layout_line_numbers(state, &text, text_size, &text_style, window);

        let input_bounds = bounds; // 未滚动的元素 bounds（sticky scroll 锚定用）
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
            Self::layout_whitespace_indicators(state, text_size, &text_style, window, cx);

        let lines = Self::layout_lines(
            state,
            display_text,
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
            let longest_line: SharedString =
                state.text.line_content(longest_row).to_string().into();
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

        // The scrollable height counts *display* rows (folded rows hidden),
        // matching what is actually painted.
        let total_display_lines = state.display_map.display_row_count();
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
            (total_display_lines as f32 * line_height + empty_bottom_height)
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

        // Calculate the scroll offset to keep the cursor in view.
        // The fold placeholders are shaped first so the cursor can be placed
        // after the `⋯` when it sits at a fold boundary (zed maps positions
        // inside a fold to the placeholder).
        let mut fold_placeholders =
            self.layout_fold_placeholders(&last_layout, &text_style, text_size, window, cx);

        let (cursor_bounds, cursor_scroll_offset, current_row) =
            self.layout_cursor(&last_layout, &fold_placeholders, &mut bounds, window, cx);
        last_layout.cursor_bounds = cursor_bounds;

        // Pills are positioned where the header lines are painted, so this
        // must run after the cursor layout has scrolled `bounds`.
        self.layout_fold_placeholder_bounds(
            &bounds,
            &last_layout,
            &mut fold_placeholders,
            window,
            cx,
        );

        let selection_path = self.layout_selections(&last_layout, &mut bounds, window, cx);
        let search_match_paths = self.layout_search_matches(&last_layout, &bounds, cx);
        let bracket_paths = self.layout_bracket_matches(&last_layout, &bounds, window, cx);
        let occurrence_paths = self.layout_occurrences(&last_layout, &bounds, window, cx);

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
                        .shape_line(line_no, text_size, runs, None),
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

        // Sticky scroll：外围定义行（首可见行之上）钉在顶部
        let sticky_lines = self.layout_sticky_lines(
            &last_layout,
            &input_bounds,
            &text_style,
            text_size,
            window.line_height(),
            line_number_len,
            window,
            cx,
        );

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
            bracket_paths,
            occurrence_paths,
            sticky_lines,
            fold_icon_layout,
            fold_placeholders,
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
        // 滚动条相关值提前取出（避免 state 借用跨 line.paint 的 &mut cx）
        let scrollbar_enabled = state.scrollbar && state.mode.is_multi_line();
        let scrollbar_dragging = state.scrollbar_drag.is_some();
        let scrollbar_scroll_y = (-state.scroll_handle.offset().y).max(px(0.));
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

        // Geometry of every visible line: (buffer row, y offset, height).
        //
        // Derived from the layout rather than from `line_numbers`, so the
        // current-line highlight keeps working when line numbers are hidden.
        let line_geometry: Vec<(usize, Pixels, Pixels)> = {
            let mut y = invisible_top_padding;
            let mut out = Vec::with_capacity(prepaint.last_layout.visible_buffer_lines.len());
            for (line, &buffer_line) in prepaint
                .last_layout
                .lines
                .iter()
                .zip(prepaint.last_layout.visible_buffer_lines.iter())
            {
                let height = line_height * line.wrapped_lines.len().max(1) as f32;
                out.push((buffer_line, y, height));
                y += height;
            }
            out
        };

        // Paint the current line background.
        //
        // This used to live inside the `line_numbers` block below, so hiding
        // line numbers also silently disabled the active-line highlight.
        //
        // At most one visible line can be the current row, so a `find` is
        // enough — no need to scan the rest of the geometry.
        //
        // 仅在多行编辑器中获得焦点时绘制（与 Zed 一致）：
        // - 失焦不画 —— 失焦的输入框不应有任何行高亮；
        // - 单行输入框（搜索框、表单）永不画 —— active line 的色值接近
        //   编辑器底色，铺在 `tab_hover_background` 等更亮的输入框容器上
        //   会形成一块色差明显的矩形，导致输入区背景色不一致。
        let single_line = !state.mode.is_multi_line();
        if focused && !single_line && selected_range.is_empty() {
            if let Some(bg_color) = active_line_color {
                if let Some(&(_, y, height)) = line_geometry
                    .iter()
                    .find(|(buffer_line, _, _)| prepaint.current_row == Some(*buffer_line))
                {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(input_bounds.origin.x, origin.y + y),
                            size(bounds.size.width, height),
                        ),
                        bg_color,
                    ));
                }
            }
        }

        if let Some(line_numbers) = prepaint.line_numbers.as_ref() {
            let diff_line_colors = &self.state.read(cx).diff_line_colors;
            // Git 变更标记（行号区右缘窄色条）
            let (git_gutter_enabled, git_line_marks, fold_gutter_width) = {
                let state = self.state.read(cx);
                let fold_w = if state.mode.is_folding() {
                    FOLD_ICON_HITBOX_WIDTH
                } else {
                    px(0.)
                };
                (state.git_gutter, &state.git_line_marks, fold_w)
            };

            // Each item is the normal lines; `line_numbers` is parallel to
            // `line_geometry`, so the height comes from the geometry above.
            for (&(buffer_line, y, height), _) in line_geometry.iter().zip(line_numbers) {
                let p = point(input_bounds.origin.x, origin.y + y);
                // Paint diff line background (added/deleted) below everything else
                if let Some(bg_color) = diff_line_colors.get(&(buffer_line as u32)) {
                    window.paint_quad(fill(
                        Bounds::new(p, size(bounds.size.width, height)),
                        *bg_color,
                    ));
                }
                // Paint git gutter marks：新增绿 / 修改蓝 / 删除红（顶部小短条）
                if git_gutter_enabled {
                    if let Some(mark) = git_line_marks.get(&(buffer_line as u32)) {
                        let bar_x = input_bounds.origin.x + prepaint.last_layout.line_number_width
                            - fold_gutter_width
                            - px(8.);
                        match mark {
                            GitLineMark::Added => {
                                window.paint_quad(fill(
                                    Bounds::new(point(bar_x, p.y), size(px(3.), height)),
                                    cx.theme().success,
                                ));
                            }
                            GitLineMark::Modified => {
                                window.paint_quad(fill(
                                    Bounds::new(point(bar_x, p.y), size(px(3.), height)),
                                    cx.theme().accent,
                                ));
                            }
                            GitLineMark::Deleted => {
                                // 此处上方有删除行：行顶小短条示意
                                window.paint_quad(fill(
                                    Bounds::new(point(bar_x, p.y), size(px(7.), px(3.))),
                                    cx.theme().error,
                                ));
                            }
                        }
                    }
                }
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

        // Paint matching brackets：柔和的 accent 底色标记配对括号
        let bracket_fill = cx.theme().accent.opacity(0.22);
        for path in prepaint.bracket_paths.drain(..) {
            window.paint_path(path, bracket_fill);
        }

        // Paint word occurrences：光标处词的全文件出现，淡选区色
        let occurrence_fill = cx.theme().selection.opacity(0.24);
        for path in prepaint.occurrence_paths.drain(..) {
            window.paint_path(path, occurrence_fill);
        }

        // Paint indent guides — VS Code style: grid-aligned, whitespace-interpolated, active-block highlighted
        if state.indent_guides
            && state.mode.is_multi_line()
            && !state.masked
            && state.text.len() > 0
        {
            // 层级宽度跟随缩进配置（`TabSize`，默认硬 Tab / 4 列），而不是
            // 写死常量 —— 否则把缩进切成 2 空格后参考线仍按 4 列画。
            let indent_size = state.indent_width();
            let style = window.text_style();
            let font_size = state
                .font_size
                .unwrap_or_else(|| style.font_size.to_pixels(window.rem_size()));
            let space_width = window
                .text_system()
                .shape_line(
                    SharedString::from(" "),
                    font_size,
                    &[TextRun {
                        len: 1,
                        font: style.font(),
                        color: style.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
                .width;
            if space_width > px(0.) {
                // Tab 的真实推进宽度：默认缩进单元就是 `"\t"`（`TabSize::HARD_TABS`），
                // 若字体给 Tab 的推进量不等于 `indent_size` 个空格，按空格宽度
                // 线性外推算出来的参考线 x 就会与正文整体错位。
                let tab_width = window
                    .text_system()
                    .shape_line(
                        SharedString::from("\t"),
                        font_size,
                        &[TextRun {
                            len: 1,
                            font: style.font(),
                            color: style.color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    )
                    .width
                    .max(space_width);

                let total_lines = state.text.lines_len();
                // 行首缩进列数（空格记 1 列，Tab 向上取整到 indent_size 的倍数）。
                let leading_columns = |line: &str| -> i32 {
                    let mut cols = 0;
                    for ch in line.chars() {
                        match ch {
                            ' ' => cols += 1,
                            '\t' => cols = cols - cols % indent_size as i32 + indent_size as i32,
                            _ => break,
                        }
                    }
                    cols
                };
                // 每级参考线的实际 x（相对正文左缘）：按前缀里空格 / Tab 的
                // 真实推进量累加，而不是 `space_width * 列数`。
                let guide_xs = |line: &str| -> Vec<Pixels> {
                    let mut out = Vec::new();
                    let mut cols = 0usize;
                    let mut width = px(0.);
                    for ch in line.chars() {
                        match ch {
                            ' ' => {
                                cols += 1;
                                width += space_width;
                            }
                            '\t' => {
                                cols = cols - cols % indent_size + indent_size;
                                width += tab_width;
                            }
                            _ => break,
                        }
                        // Tab 与制表位、参考线层级都是 `indent_size` 的倍数，
                        // 所以层级边界一定落在某个字符之后。
                        while out.len() * indent_size < cols {
                            out.push(width);
                        }
                    }
                    out
                };
                // 空行返回 -1：它们的参考线数量由邻近非空行插值得到。
                let line_indent = |row: usize| -> i32 {
                    let line = state.text.line_content(row).to_string();
                    if line.trim().is_empty() {
                        -1
                    } else {
                        leading_columns(&line)
                    }
                };

                // 只有可视区内的行会被真正画出来，所以把整段计算裁到这个窗口上。
                // 此前每一行都要 `slice_line` 成 String、再跑三趟 O(总行数) 的
                // 前缀/后缀扫描，10 万行文件每帧就是 10 万次堆分配 + 40 万次迭代。
                let cursor_line = state.cursor_position().line as usize;
                let mut win_start = prepaint
                    .last_layout
                    .visible_buffer_lines
                    .iter()
                    .copied()
                    .min()
                    .unwrap_or(0);
                let mut win_end = prepaint
                    .last_layout
                    .visible_buffer_lines
                    .iter()
                    .copied()
                    .max()
                    .map(|m| m + 1)
                    .unwrap_or(0);
                // 活跃块判定要从光标所在行出发，把光标行也纳入窗口。
                win_start = win_start.min(cursor_line).min(total_lines);
                win_end = win_end.max(cursor_line + 1).min(total_lines);
                win_end = win_end.max(win_start);
                let win_lines = win_end - win_start;

                let mut indents = Vec::with_capacity(win_lines);
                let mut indent_xs: Vec<Vec<Pixels>> = Vec::with_capacity(win_lines);
                for idx in win_start..win_end {
                    let line = state.text.line_content(idx).to_string();
                    if line.trim().is_empty() {
                        indents.push(-1);
                        indent_xs.push(Vec::new());
                    } else {
                        indents.push(leading_columns(&line));
                        indent_xs.push(guide_xs(&line));
                    }
                }

                // 窗口外紧邻的非空行缩进：空行要沿用邻近非空行的缩进，而
                // 位于窗口边缘的空行需要向窗口外看一眼。两个循环都在遇到
                // 第一个非空行时停下，正常情况下只走几行。
                let above_window = (0..win_start)
                    .rev()
                    .map(line_indent)
                    .find(|indent| *indent >= 0)
                    .unwrap_or(-1);
                let below_window = (win_end..total_lines)
                    .map(line_indent)
                    .find(|indent| *indent >= 0)
                    .unwrap_or(-1);

                // Guides per line with VS Code whitespace interpolation
                let off_side = matches!(state.language().as_deref(), Some("python") | Some("yaml"));
                let guides_counts = {
                    let mut prev = vec![-1; win_lines];
                    let mut last = above_window;
                    for i in 0..win_lines {
                        if indents[i] >= 0 {
                            last = indents[i];
                        }
                        prev[i] = last;
                    }
                    let mut next = vec![-1; win_lines];
                    let mut nxt = below_window;
                    for i in (0..win_lines).rev() {
                        if indents[i] >= 0 {
                            nxt = indents[i];
                        }
                        next[i] = nxt;
                    }
                    let mut counts = vec![0; win_lines];
                    for i in 0..win_lines {
                        let cur = indents[i];
                        if cur >= 0 {
                            counts[i] = (cur as usize).div_ceil(indent_size);
                        } else {
                            let above = prev[i];
                            let below = next[i];
                            counts[i] = match (above, below) {
                                (-1, _) | (_, -1) => 0,
                                (a, b) if a < b => (a as usize / indent_size) + 1,
                                (a, b) if a == b => (b as usize).div_ceil(indent_size),
                                (_, b) if off_side => (b as usize).div_ceil(indent_size),
                                (_, b) => (b as usize / indent_size) + 1,
                            };
                        }
                    }
                    counts
                };
                // 窗口外的行一律当作 0 层：活跃块的向外扩张因此会停在窗口边缘，
                // 而窗口本来就覆盖了所有可见行，绘制结果完全一致。
                let count_at = |row: usize| -> usize {
                    if row >= win_start && row < win_end {
                        guides_counts[row - win_start]
                    } else {
                        0
                    }
                };
                // Active indent block for the cursor line.
                //
                // 区间统一用 **0-based 半开区间** `[active_start, active_end)`，
                // 旧实现里 `start` 是 1-based 闭区间、`end` 却是 0-based 开区间，
                // 两种约定混在一处，边界极易差一行。
                let (active_start, active_end, active_level) = if cursor_line >= total_lines {
                    (0, 0, 0)
                } else {
                    let mut level = count_at(cursor_line);
                    // 0 行的半开区间：只含光标行本身。
                    let mut start = cursor_line;
                    let mut end = cursor_line + 1;
                    if level == 0 && cursor_line + 1 < total_lines && count_at(cursor_line + 1) == 1
                    {
                        // 光标停在块的第一行（缩进尚未展开）：整块从下一行起算。
                        level = 1;
                        start = cursor_line + 1;
                        end = start + 1;
                    }
                    if level == 0 {
                        (0, 0, 0)
                    } else {
                        // 向上并吞所有缩进不低于本层的行（含首行）。
                        while start > 0 && count_at(start - 1) >= level {
                            start -= 1;
                        }
                        while end < total_lines && count_at(end) >= level {
                            end += 1;
                        }
                        (start, end, level)
                    }
                };
                let base_color = cx.theme().border.opacity(0.28);
                let active_color = cx.theme().border.opacity(0.85);
                let gutter_width = prepaint.last_layout.line_number_width;
                let mut gy = invisible_top_padding;
                for (vi, &buffer_line) in
                    prepaint.last_layout.visible_buffer_lines.iter().enumerate()
                {
                    // `lines` 与 `visible_buffer_lines` 正常情况下一一对应，
                    // 但空文本分支是按 placeholder 生成的，长度可能不等价 ——
                    // 直接下标会在绘制期 panic。
                    let Some(line) = prepaint.last_layout.lines.get(vi) else {
                        break;
                    };
                    let line_h = line.size(line_height).height;
                    let line_y = origin.y + gy;
                    let count = count_at(buffer_line);
                    let xs = if buffer_line >= win_start && buffer_line < win_end {
                        indent_xs[buffer_line - win_start].as_slice()
                    } else {
                        &[]
                    };
                    for level in 1..=count {
                        let visible_column = (level - 1) * indent_size + 1;
                        // 优先用实测推进量；层级超出缩进前缀（空行插值出来
                        // 的层级）时退回按空格宽度线性外推。
                        let offset = match xs.get(level - 1) {
                            Some(x) => *x,
                            None => space_width * (visible_column - 1) as f32,
                        };
                        let x = origin.x + gutter_width + offset;
                        if x >= input_bounds.right() {
                            break;
                        }
                        // 半开区间：[active_start, active_end)
                        let is_active = level == active_level
                            && active_level != 0
                            && buffer_line >= active_start
                            && buffer_line < active_end;
                        let color = if is_active { active_color } else { base_color };
                        window.paint_quad(fill(
                            Bounds::new(point(x, line_y), size(px(1.), line_h)),
                            color,
                        ));
                    }
                    gy += line_h;
                }
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

        // Fold-related click targets (placeholder pills + gutter icons).
        let mut fold_click_targets: Vec<(usize, Bounds<Pixels>)> = Vec::new();
        let fold_placeholder_bg = cx.theme().text.opacity(0.055);

        for (line, _buffer_line) in prepaint
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
            line.paint(
                p,
                line_height,
                text_align,
                Some(prepaint.last_layout.content_width),
                window,
                cx,
            );
            offset_y += line.size(line_height).height;
        }

        // Paint the `⋯` pills and their trailing texts (e.g. `}`) inline at the
        // end of each folded header line — zed's fold placeholder replaces the
        // folded range, keeping the closing delimiter visible on the same
        // display row (`fn main() {⋯}`).
        for placeholder in &prepaint.fold_placeholders {
            let Some(pill_bounds) = placeholder.pill_bounds else {
                continue;
            };
            window.paint_quad(fill(pill_bounds, fold_placeholder_bg).corner_radii(px(2.)));
            _ = placeholder.ellipsis.paint(
                point(
                    pill_bounds.origin.x + FOLD_PLACEHOLDER_PADDING,
                    pill_bounds.origin.y,
                ),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
            // Clicking the placeholder unfolds this fold (zed's
            // `FoldPlaceholder::fold_element` on_click → unfold_ranges).
            fold_click_targets.push((placeholder.row, pill_bounds));
            if let Some(hitbox) = placeholder.hitbox.as_ref() {
                window.set_cursor_style(CursorStyle::PointingHand, hitbox);
            }

            if let Some(tail) = placeholder.tail.as_ref() {
                _ = tail.paint(
                    point(pill_bounds.right(), pill_bounds.origin.y),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
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
        fold_click_targets.extend(self.paint_fold_icons(
            &mut prepaint.fold_icon_layout,
            prepaint.current_row,
            window,
            cx,
        ));

        // Paint sticky scroll：顶部钉住外围定义行（半透明底 + 底边框，点击跳转）
        let mut sticky_click_targets: Vec<(usize, Bounds<Pixels>)> = Vec::new();
        if !prepaint.sticky_lines.is_empty() {
            let sticky_h = line_height * prepaint.sticky_lines.len() as f32;
            window.paint_quad(fill(
                Bounds::new(input_bounds.origin, size(input_bounds.size.width, sticky_h)),
                cx.theme().editor_background().opacity(0.92),
            ));
            window.paint_quad(fill(
                Bounds::new(
                    point(
                        input_bounds.origin.x,
                        input_bounds.origin.y + sticky_h - px(1.),
                    ),
                    size(input_bounds.size.width, px(1.)),
                ),
                cx.theme().border,
            ));
            for sticky in &prepaint.sticky_lines {
                let y = sticky.bounds.origin.y;
                // 行号（与主行号同位左对齐）
                _ = sticky.number.paint(
                    point(input_bounds.origin.x, y),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                // 文本（与正文同起点）
                _ = sticky.text.paint(
                    point(
                        input_bounds.origin.x + prepaint.last_layout.line_number_width,
                        y,
                    ),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                sticky_click_targets.push((sticky.buffer_line, sticky.bounds));
            }
        }

        // Paint scrollbar：右侧悬浮滚动条（内容超出视口时显示，支持拖拽）
        let mut scrollbar_geometry: Option<ScrollbarGeometry> = None;
        if scrollbar_enabled {
            let viewport_h = input_bounds.size.height;
            let content_h = prepaint.scroll_size.height;
            if content_h > viewport_h + px(1.) {
                let track_h = viewport_h;
                let thumb_h = ((viewport_h / content_h) * track_h).max(px(24.));
                let scroll_y = scrollbar_scroll_y;
                let max_scroll = content_h - viewport_h;
                let frac = (f32::from(scroll_y) / f32::from(max_scroll)).clamp(0., 1.);
                let thumb_y = input_bounds.origin.y + (track_h - thumb_h) * frac;
                let thumb_bounds = Bounds::new(
                    point(input_bounds.right() - px(6.), thumb_y),
                    size(px(4.), thumb_h),
                );
                let dragging = scrollbar_dragging;
                let thumb_color = if dragging {
                    cx.theme().border.opacity(0.9)
                } else {
                    cx.theme().border.opacity(0.55)
                };
                window.paint_quad(fill(thumb_bounds, thumb_color).corner_radii(px(2.)));
                scrollbar_geometry = Some(ScrollbarGeometry {
                    thumb: thumb_bounds,
                    track_height: track_h,
                    thumb_height: thumb_h,
                    content_height: content_h,
                });
            }
        }

        self.state.update(cx, |state, cx| {
            state.scrollbar_geometry = scrollbar_geometry;
            state.sticky_click_targets = sticky_click_targets;
            state.fold_click_targets = fold_click_targets;
            state.last_layout = Some(prepaint.last_layout.clone());
            state.last_bounds = Some(bounds);
            state.last_cursor = Some(state.cursor());
            state.set_input_bounds(input_bounds, cx);
            state.last_selected_range = Some(selected_range);
            state.scroll_size = prepaint.scroll_size;
            let scroll_changed =
                state.update_scroll_offset(Some(prepaint.cursor_scroll_offset), cx);
            state.deferred_scroll_offset = None;

            // 这里曾经无条件 `cx.notify()`：paint 阶段标脏会让实体在下一帧
            // 重新走一遍 prepaint + paint，空闲时也是满帧空转，而 paint 里还
            // 叠着若干 O(总行数) 的扫描。
            //
            // 只有滚动位置真的动了才需要再来一帧 —— 钳制后的偏移与本次布局
            // 采用的值不同，可见区要按最终值重算。其余状态（滚动条几何、点击
            // 热区、布局缓存）都只影响后续事件处理，不需要重绘。
            // 光标闪烁走独立的 `blink_cursor` 实体定时器，不依赖这里的 notify。
            if scroll_changed {
                cx.notify();
            }
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

    #[test]
    fn combine_highlights_stable_rainbow_overrides_syntax() {
        // Nested "((  ))" - overlay (rainbow) must win and paired brackets share the same color.
        // 注意：`HighlightStyle` 为 `Copy`，直接按值使用，无需 `.clone()`。
        let gray = HighlightStyle {
            color: Some(gpui::rgb(0xb2b9c6).into()),
            ..Default::default()
        };
        let gold = HighlightStyle {
            color: Some(gpui::rgb(0xE2B93D).into()),
            ..Default::default()
        };
        let blue = HighlightStyle {
            color: Some(gpui::rgb(0x4FC1FF).into()),
            ..Default::default()
        };
        let syntax = vec![(0..1, gray), (1..2, gray), (2..3, gray), (3..4, gray)];
        let rainbow = vec![(0..1, gold), (1..2, blue), (2..3, blue), (3..4, gold)];
        let merged = combine_highlights_stable(syntax, rainbow);
        assert_eq!(
            merged,
            vec![(0..1, gold), (1..2, blue), (2..3, blue), (3..4, gold)]
        );
        assert_eq!(merged[0].1.color, merged[3].1.color);
        assert_eq!(merged[1].1.color, merged[2].1.color);
    }

    #[test]
    fn combine_highlights_stable_deterministic_for_nested() {
        let base = HighlightStyle {
            color: Some(gpui::red()),
            ..Default::default()
        };
        let overlay = HighlightStyle {
            color: Some(gpui::blue()),
            ..Default::default()
        };
        let a = vec![(0..5, base), (4..10, base)];
        let b = vec![(2..6, overlay)];
        let r1 = combine_highlights_stable(a.clone(), b.clone());
        let r2 = combine_highlights_stable(a, b);
        assert_eq!(r1, r2);
        let seg = r1.iter().find(|(r, _)| r.contains(&3)).unwrap();
        assert_eq!(seg.1.color, Some(gpui::blue()));
    }
}
