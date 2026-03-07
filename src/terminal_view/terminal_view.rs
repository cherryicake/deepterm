//! Terminal view component
//!
//! High-level terminal UI component integrating with GPUI

use std::time::Duration;

use gpui::{
    App, Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    IntoElement, Pixels, Point, Render, SharedString, UTF16Selection, Window, div, prelude::*, px,
    rgb,
};

use crate::terminal::event::Event as TerminalEvent;
use crate::terminal::mappings::keys::{TermMode, keystroke_to_bytes};
use crate::terminal::terminal::Terminal;
use crate::terminal_view::terminal_element::TerminalElement;

/// IME state for tracking composition
#[derive(Debug)]
struct ImeState {
    marked_text: String,
}

/// Terminal view component
pub struct TerminalView {
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    title: SharedString,
    ime_state: Option<ImeState>,
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
                ime_state: None,
            };

            // Spawn a timer to process terminal events periodically
            let terminal_clone2 = terminal.clone();
            cx.spawn(async move |_, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                    let _ = terminal_clone2.update(cx, |terminal, cx| {
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

    /// Set the marked (pre-edit) text from IME
    pub fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    /// Get the current marked range (UTF-16)
    pub fn marked_text_range(&self) -> Option<std::ops::Range<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    /// Clear the marked (pre-edit) text state
    pub fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    /// Commit (send) the given text to the PTY
    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.terminal.update(cx, |terminal, _| {
                terminal.write(text.as_bytes());
            });
        }
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
        // Terminal doesn't have selectable text in the traditional sense
        // Return empty selection at position 0
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
        self.marked_text_range()
    }

    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // Terminal doesn't expose its content as editable text
        None
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Commit the final composed text from IME
        self.clear_marked_text(cx);
        self.commit_text(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Handle composition updates (pre-edit text)
        self.set_marked_text(new_text.to_string(), cx);
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Clear composition state
        self.clear_marked_text(cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Return cursor bounds for IME candidate window positioning
        // For now, return None - the system will use default positioning
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Terminal doesn't support point-to-character mapping
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
        // Check if this view is currently focused
        let is_focused = focus_handle.is_focused(window);

        div()
            .id("terminal-view")
            .size_full()
            .track_focus(&self.focus_handle)
            .when(!is_focused, |this| {
                this.on_click(cx.listener(|this, _event, window, cx| {
                    this.focus_handle.focus(window, cx);
                }))
            })
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, _window, cx| {
                    // If key_char is present, IME will handle this input
                    // Don't process it again to avoid duplicate characters
                    if event.keystroke.key_char.is_some() {
                        return;
                    }

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
                is_focused,
                self.cursor_visible,
                view_entity,
            ))
    }
}
