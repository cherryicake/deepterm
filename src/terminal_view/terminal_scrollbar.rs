//! Terminal scrollbar - based on Zed's implementation approach

use gpui::{
    App, Bounds, Corners, Edges, Entity, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Window, px, quad, size,
};

use crate::terminal::terminal::Terminal;

/// Minimum thumb size (same as Zed's MINIMUM_THUMB_SIZE)
const MINIMUM_THUMB_SIZE: Pixels = px(8.);

/// Scrollbar thumb corner radius
const THUMB_CORNER_RADIUS: Pixels = px(4.);

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
}

impl TerminalScrollbar {
    pub fn new(terminal: Entity<Terminal>, _cx: &mut App) -> Self {
        Self { terminal }
    }
}

impl IntoElement for TerminalScrollbar {
    type Element = ScrollbarElement;

    fn into_element(self) -> Self::Element {
        ScrollbarElement {
            terminal: self.terminal,
        }
    }
}

/// Scrollbar element
pub struct ScrollbarElement {
    terminal: Entity<Terminal>,
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

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

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
        ()
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
        let line_height = term.terminal_bounds().line_height;
        let total_lines = term.total_lines();
        let viewport_lines = term.viewport_lines();
        let display_offset = term.last_content().display_offset;

        // Only show scrollbar when there's scrollable content
        // (when total_lines > viewport_lines)
        if total_lines == 0 || viewport_lines == 0 || total_lines <= viewport_lines {
            return; // No scrollbar needed
        }

        // Calculate content and viewport heights (in pixels)
        let content_size = px(total_lines as f32 * line_height.as_f32());
        let viewport_px = px(viewport_lines as f32 * line_height.as_f32());

        // Calculate max scroll in pixels
        let max_offset = content_size - viewport_px;

        // Calculate thumb size
        let visible_percentage = viewport_px / content_size;
        let thumb_size = MINIMUM_THUMB_SIZE.max(viewport_px * visible_percentage);

        if thumb_size >= viewport_px {
            return; // No scrollbar needed
        }

        // Calculate thumb position
        let max_scroll_lines = total_lines.saturating_sub(viewport_lines);
        let scroll_offset = max_scroll_lines.saturating_sub(display_offset);
        let current_offset = px(scroll_offset as f32 * line_height.as_f32());

        let start_offset = if max_offset.as_f32() > 0.0 {
            px((current_offset.as_f32() / max_offset.as_f32()) * (viewport_size - thumb_size).as_f32())
        } else {
            px(0.0)
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
        let thumb_bounds_for_down = thumb_bounds;
        let track_bounds_for_move = track_bounds;
        let thumb_size_for_move = thumb_size;

        // Mouse down handler - start dragging
        window.on_mouse_event({
            let thumb_offset = thumb_offset.clone();
            move |event: &MouseDownEvent, _phase, _window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }

                if thumb_bounds_for_down.contains(&event.position) {
                    // Calculate offset from top of thumb to click position
                    let offset = event.position.y - thumb_bounds_for_down.origin.y;
                    thumb_offset.set(Some(offset));
                    cx.stop_propagation();
                }
            }
        });

        // Mouse move handler - dragging in progress
        window.on_mouse_event({
            let thumb_offset = thumb_offset.clone();
            move |event: &MouseMoveEvent, _phase, _window, cx| {
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
                let new_display_offset = ((1.0 - percentage) * max_scroll_lines as f32).round() as usize;
                let clamped_offset = new_display_offset.min(max_scroll_lines);

                terminal.update(cx, |t, cx| {
                    t.set_display_offset(clamped_offset, cx);
                });
                cx.stop_propagation();
            }
        });

        // Mouse up handler - end dragging
        window.on_mouse_event({
            let thumb_offset = thumb_offset;
            move |_event: &MouseUpEvent, _phase, _window, _cx| {
                thumb_offset.set(None);
            }
        });
    }
}
