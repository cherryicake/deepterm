//! Terminal scrollbar

use gpui::prelude::*;
use gpui::{
    Context, Entity, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Render, Window, div, px, rgb,
};

use crate::terminal::terminal::Terminal;

pub struct TerminalScrollbar {
    terminal: Entity<Terminal>,
    dragging: bool,
    drag_start_y: Option<Pixels>,
}

impl TerminalScrollbar {
    pub fn new(terminal: Entity<Terminal>) -> Self {
        Self {
            terminal,
            dragging: false,
            drag_start_y: None,
        }
    }
}

impl Render for TerminalScrollbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self.terminal.read(cx);
        let content = terminal.last_content();
        let display_offset = content.display_offset;
        let total_lines = content.rows;

        // Calculate scrollbar thumb position and size
        let scrollback_size = 10000usize; // Default scrollback buffer size
        let visible_ratio = total_lines as f32 / scrollback_size as f32;
        let thumb_height_px = px((visible_ratio.max(0.1) * 100.0).max(20.0));
        let scroll_ratio = display_offset as f32 / scrollback_size as f32;
        let thumb_position_px = px((scroll_ratio.min(1.0 - visible_ratio) * 100.0).max(0.0));

        div()
            .id("terminal-scrollbar")
            .w(px(12.0))
            .h_full()
            .bg(rgb(0x2a2a3e))
            .child(
                div()
                    .id("scrollbar-thumb")
                    .w_full()
                    .h(thumb_height_px)
                    .mt(thumb_position_px)
                    .bg(rgb(0x5a5a7e))
                    .rounded(px(6.0))
                    .cursor(gpui::CursorStyle::Arrow)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                            this.dragging = true;
                            this.drag_start_y = Some(event.position.y);
                        }),
                    ),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    this.dragging = false;
                    this.drag_start_y = None;
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.dragging {
                    // Handle scrollbar dragging
                    if let Some(start_y) = this.drag_start_y {
                        let delta_y = event.position.y - start_y;
                        // Convert pixel delta to scroll lines
                        let lines = (delta_y.as_f32() / 16.0) as i32; // Assuming 16px line height
                        if lines != 0 {
                            this.terminal.update(cx, |terminal, _cx| {
                                if lines > 0 {
                                    terminal.scroll_line_up();
                                } else {
                                    terminal.scroll_line_down();
                                }
                            });
                            this.drag_start_y = Some(event.position.y);
                        }
                    }
                }
            }))
    }
}
