//! Terminal scrollbar - based on Zed's implementation approach

use gpui::{
    App, Bounds, Corners, Edges, Entity, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Window, px, quad, size,
};

use crate::terminal::terminal::Terminal;

/// Minimum thumb size — bumped up for better visibility.
pub(crate) const MINIMUM_THUMB_SIZE: Pixels = px(24.);

/// Scrollbar thumb corner radius
const THUMB_CORNER_RADIUS: Pixels = px(4.);

/// Compute scrollbar thumb Y offset and size from terminal metrics.
/// Returns `None` when no scrollbar is needed (content fits in viewport).
pub(crate) fn calculate_thumb_metrics(
    total_lines: usize,
    viewport_lines: usize,
    display_offset: usize,
    line_height: Pixels,
    viewport_height: Pixels,
) -> Option<(Pixels, Pixels)> {
    if total_lines <= viewport_lines {
        return None;
    }

    let content_size = px(total_lines as f32 * line_height.as_f32());
    let viewport_px = px(viewport_lines as f32 * line_height.as_f32());
    let max_offset = content_size - viewport_px;
    let visible_percentage = viewport_px / content_size;
    let thumb_size = MINIMUM_THUMB_SIZE.max(viewport_px * visible_percentage);

    if thumb_size >= viewport_px {
        return None;
    }

    let max_scroll_lines = total_lines.saturating_sub(viewport_lines);
    let scroll_offset = max_scroll_lines.saturating_sub(display_offset);
    let current_offset = px(scroll_offset as f32 * line_height.as_f32());

    let thumb_y = if max_offset.as_f32() > 0.0 {
        px((current_offset.as_f32() / max_offset.as_f32())
            * (viewport_height - thumb_size).as_f32())
    } else {
        px(0.0)
    };

    Some((thumb_y, thumb_size))
}

/// Zed-style scrollbar thumb color
/// Uses a semi-transparent gray similar to Zed's neutral().light_alpha().step_4()
fn scrollbar_thumb_color() -> Hsla {
    Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.55,
        a: 0.35,
    }
}

/// Terminal scrollbar component
pub struct TerminalScrollbar {
    terminal: Entity<Terminal>,
    /// Optional override for the thumb's Y offset within the track.
    /// When `Some`, the thumb is positioned at this exact pixel offset
    /// instead of being derived from `display_offset`.
    thumb_y_override: Option<Pixels>,
}

impl TerminalScrollbar {
    pub fn new(
        terminal: Entity<Terminal>,
        thumb_y_override: Option<Pixels>,
        _cx: &mut App,
    ) -> Self {
        Self {
            terminal,
            thumb_y_override,
        }
    }
}

impl IntoElement for TerminalScrollbar {
    type Element = ScrollbarElement;

    fn into_element(self) -> Self::Element {
        ScrollbarElement {
            terminal: self.terminal,
            thumb_y_override: self.thumb_y_override,
        }
    }
}

/// Scrollbar element
pub struct ScrollbarElement {
    terminal: Entity<Terminal>,
    thumb_y_override: Option<Pixels>,
}

impl IntoElement for ScrollbarElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for ScrollbarElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        Some(gpui::ElementId::Name("terminal-scrollbar".into()))
    }

    crate::impl_source_location_none!();

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _iid: Option<&gpui::InspectorElementId>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let mut style = gpui::Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        (_window.request_layout(style, [], _cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _iid: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _iid: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let track_bounds = bounds;
        let viewport_size = track_bounds.size.height;

        if viewport_size <= px(0.0) {
            return;
        }

        // Get terminal metrics
        let term = self.terminal.read(cx);
        let line_height = term.terminal_bounds.line_height;
        let total_lines = term.total_lines();
        let viewport_lines = term.viewport_lines();
        let display_offset = term.last_content().display_offset;

        // Only show scrollbar when there's scrollable content
        // (when total_lines > viewport_lines)
        if total_lines == 0 || viewport_lines == 0 || total_lines <= viewport_lines {
            return; // No scrollbar needed
        }

        let Some((start_offset, thumb_size)) = calculate_thumb_metrics(
            total_lines,
            viewport_lines,
            display_offset,
            line_height,
            viewport_size,
        ) else {
            return; // No scrollbar needed
        };

        let max_scroll_lines = total_lines.saturating_sub(viewport_lines);

        let start_offset = if let Some(override_y) = self.thumb_y_override {
            override_y
        } else {
            start_offset
        };

        // Thumb fills the track width (no padding) for a cleaner look
        let thumb_y = track_bounds.origin.y + start_offset;
        let thumb_width = track_bounds.size.width;
        let thumb_height = thumb_size;

        let thumb_bounds = Bounds::new(
            Point::new(track_bounds.origin.x, thumb_y),
            size(thumb_width, thumb_height),
        );

        // Paint thumb with Zed-style color and rounded corners
        let thumb_color = scrollbar_thumb_color();
        window.paint_quad(quad(
            thumb_bounds,
            Corners::all(THUMB_CORNER_RADIUS),
            thumb_color,
            Edges::default(),
            Hsla::transparent_black(),
            gpui::BorderStyle::default(),
        ));

        // Get the persistent drag offset from Terminal
        let thumb_offset = self.terminal.read(cx).scrollbar_drag_offset.clone();

        // Clone for event handlers
        let terminal = self.terminal.clone();
        let terminal_for_down = terminal.clone();
        let thumb_bounds_for_down = thumb_bounds;
        let track_bounds_for_move = track_bounds;
        let thumb_size_for_move = thumb_size;

        // Mouse down handler - start dragging (thumb) or page-scroll (track).
        //
        // These are window-level listeners: they are invoked once per dispatch
        // phase (capture and bubble). Guard on `phase.bubble()` so each event
        // is handled exactly once, and use `stop_propagation()` for any click
        // inside the track so the underlying terminal element does not start
        // a text selection (the scrollbar paints after the terminal element,
        // so in the bubble phase this handler runs first).
        window.on_mouse_event({
            let thumb_offset = thumb_offset.clone();
            let track_bounds_for_down = track_bounds;
            move |event: &MouseDownEvent, phase, _window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }

                if !track_bounds_for_down.contains(&event.position) {
                    return;
                }

                if thumb_bounds_for_down.contains(&event.position) {
                    // Thumb: start dragging at the clicked offset.
                    let offset = event.position.y - thumb_bounds_for_down.origin.y;
                    thumb_offset.set(Some(offset));
                } else if event.position.y < thumb_bounds_for_down.origin.y {
                    // Track above the thumb: page up (towards older scrollback).
                    let target = display_offset
                        .saturating_add(viewport_lines)
                        .min(max_scroll_lines);
                    terminal_for_down.update(cx, |t, cx| {
                        t.set_display_offset(target, cx);
                    });
                } else {
                    // Track below the thumb: page down (towards newest content).
                    let target = display_offset.saturating_sub(viewport_lines);
                    terminal_for_down.update(cx, |t, cx| {
                        t.set_display_offset(target, cx);
                    });
                }
                cx.stop_propagation();
            }
        });

        // Mouse move handler - dragging in progress
        window.on_mouse_event({
            let thumb_offset = thumb_offset.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if !phase.bubble() {
                    return;
                }

                // Check if we're dragging the scrollbar
                let Some(offset) = thumb_offset.get() else {
                    return;
                };

                if !event.dragging() {
                    return;
                }

                let viewport_size = track_bounds_for_move.size.height;

                // Compute new thumb position based on mouse position minus the initial offset
                let thumb_start = (event.position.y - track_bounds_for_move.origin.y - offset)
                    .clamp(px(0.0), viewport_size - thumb_size_for_move);

                // Calculate scroll percentage (0.0 = top, 1.0 = bottom)
                let percentage = if viewport_size > thumb_size_for_move {
                    thumb_start.as_f32() / (viewport_size - thumb_size_for_move).as_f32()
                } else {
                    0.0
                };

                // Convert percentage to display_offset
                // percentage 0.0 -> display_offset = max_scroll_lines (at top)
                // percentage 1.0 -> display_offset = 0 (at bottom)
                let new_display_offset =
                    ((1.0 - percentage) * max_scroll_lines as f32).round() as usize;
                let clamped_offset = new_display_offset.min(max_scroll_lines);

                terminal.update(cx, |t, cx| {
                    t.set_display_offset(clamped_offset, cx);
                });
                cx.stop_propagation();
            }
        });

        // Mouse up handler - end dragging
        window.on_mouse_event({
            // `move` 闭包会直接按值捕获 `thumb_offset`（此处是它的最后一处使用）。
            move |_event: &MouseUpEvent, phase, _window, _cx| {
                if !phase.bubble() {
                    return;
                }
                thumb_offset.set(None);
            }
        });
    }
}
