//! Terminal view component
//!
//! High-level terminal UI component integrating with GPUI

use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Window,
};

use crate::terminal::event::Event as TerminalEvent;
use crate::terminal::mappings::keys::{keystroke_to_bytes, TermMode};
use crate::terminal::terminal::Terminal;
use crate::terminal_view::terminal_element::TerminalElement;

/// Terminal view component
pub struct TerminalView {
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    title: SharedString,
}

impl TerminalView {
    /// Create a new terminal view
    pub fn new(cx: &mut App) -> Entity<Self> {
        let terminal = Terminal::new(cx, (80, 24));
        let focus_handle = cx.focus_handle();

        cx.new(|cx| {
            let terminal_clone = terminal.clone();

            // Subscribe to terminal events
            cx.subscribe(
                &terminal_clone,
                |this: &mut TerminalView, _terminal, event: &TerminalEvent, cx| {
                    match event {
                        TerminalEvent::TitleChanged(title) => {
                            this.title = title.clone().into();
                            cx.notify();
                        }
                        TerminalEvent::ContentChanged => {
                            cx.notify();
                        }
                        TerminalEvent::Bell => {
                            // Handle bell
                        }
                        TerminalEvent::Exited(_) | TerminalEvent::ExitedWithError(_) => {
                            // Handle exit
                        }
                        _ => {}
                    }
                },
            )
            .detach();

            let view = Self {
                terminal: terminal_clone,
                focus_handle,
                focused: false,
                cursor_visible: true,
                title: "Terminal".into(),
            };

            // Spawn a timer to process terminal events periodically
            let terminal_clone2 = terminal.clone();
            cx.spawn(async move |_, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(16)).await;
                    let _ = terminal_clone2
                        .update(cx, |terminal, cx| {
                            terminal.process_events(cx);
                        });
                }
            })
            .detach();

            view
        })
    }

    /// Get the terminal entity
    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    /// Get the title
    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// Write data to terminal
    pub fn write(&self, data: &[u8], cx: &mut App) {
        self.terminal.update(cx, |terminal: &mut Terminal, _cx| {
            terminal.write(data);
        });
    }

    /// Resize terminal
    pub fn resize(&mut self, cols: usize, rows: usize, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal: &mut Terminal, _cx| {
            terminal.resize(cols, rows, _cx);
        });
        cx.notify();
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self.terminal.clone();
        let focused = self.focused;
        let focus_handle = self.focus_handle.clone();

        div()
            .id("terminal-view")
            .size_full()
            .track_focus(&self.focus_handle)
            .when(!focused, |this| {
                this.on_click(cx.listener(|this, _event, window, cx| {
                    this.focused = true;
                    this.focus_handle.focus(window, cx);
                }))
            })
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, _window, cx| {
                    let term_mode = TermMode::default();
                    if let Some(bytes) =
                        keystroke_to_bytes(&event.keystroke, &event.keystroke.modifiers, term_mode)
                    {
                        this.terminal.update(cx, |terminal, _cx| {
                            terminal.write(&bytes);
                        });
                    }
                }),
            )
            .child(TerminalElement::new(
                terminal,
                focus_handle,
                focused,
                self.cursor_visible,
            ))
    }
}
