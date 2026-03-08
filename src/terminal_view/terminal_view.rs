//! Terminal view component - Full featured

use gpui::{
    div, prelude::*, px, rgb, App, Bounds, Context, CursorStyle, Entity, EntityInputHandler,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString, UTF16Selection, Window,
};

use crate::terminal::mappings::keys::to_esc_str;
use crate::terminal::terminal::{Event as TerminalEvent, Terminal, TerminalBounds};
use crate::terminal_view::terminal_element::TerminalElement;
use crate::terminal_view::terminal_scrollbar::TerminalScrollbar;

pub struct TerminalView {
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    title: SharedString,
    show_scrollbar: bool,
    selecting: bool,
}

impl TerminalView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let terminal = Terminal::new(cx, std::env::current_dir().ok());
        let focus_handle = cx.focus_handle();

        cx.new(|cx| {
            let terminal_clone = terminal.clone();

            cx.subscribe(
                &terminal_clone,
                |this: &mut TerminalView, _terminal, event: &TerminalEvent, cx| match event {
                    TerminalEvent::TitleChanged => {
                        this.title = "Terminal".into();
                        cx.notify();
                    }
                    TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {}
                },
            )
            .detach();

            Self {
                terminal: terminal_clone,
                focus_handle,
                focused: false,
                cursor_visible: true,
                title: "Terminal".into(),
                show_scrollbar: true,
                selecting: false,
            }
        })
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    // Actions
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            terminal.copy(cx);
        });
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            terminal.paste(cx);
        });
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.clear();
        });
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.select_all();
        });
        cx.notify();
    }

    // Scrolling
    pub fn scroll_line_up(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_line_up();
        });
        cx.notify();
    }

    pub fn scroll_line_down(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_line_down();
        });
        cx.notify();
    }

    pub fn scroll_page_up(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_page_up();
        });
        cx.notify();
    }

    pub fn scroll_page_down(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_page_down();
        });
        cx.notify();
    }

    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_to_top();
        });
        cx.notify();
    }

    pub fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_to_bottom();
        });
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handled = self.terminal.update(cx, |terminal, _cx| {
            let mode = terminal.last_content().mode;
            if let Some(bytes) = to_esc_str(&event.keystroke, &mode, false) {
                terminal.write(&bytes);
                true
            } else {
                false
            }
        });

        if handled {
            // Stop propagation only if we handled the key
            cx.stop_propagation();
        }
        // Otherwise, let the event propagate to EntityInputHandler
        // which will handle regular character input (including uppercase letters)
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TerminalView {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        None
    }

    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal.update(cx, |terminal, _| {
            terminal.write(text.as_bytes());
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal.update(cx, |terminal, _| {
            terminal.write(new_text.as_bytes());
        });
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self.terminal.clone();
        let focus_handle = self.focus_handle.clone();
        let view_entity = cx.entity().clone();
        let is_focused = focus_handle.is_focused(window);
        let show_scrollbar = self.show_scrollbar;

        div()
            .id("terminal-view")
            .size_full()
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.selecting = true;
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_down(event, cx);
                    });
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.selecting = false;
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_up(event, cx);
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.selecting {
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_drag(event, cx);
                    });
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                this.terminal.update(cx, |terminal, _cx| {
                    terminal.scroll_wheel(event, 3.0);
                });
                cx.notify();
            }))
            .child(TerminalElement::new(
                terminal,
                focus_handle,
                is_focused,
                self.cursor_visible,
                view_entity,
            ))
    }
}
