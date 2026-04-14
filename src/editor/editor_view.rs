use gpui::prelude::*;
use gpui::{App, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div};

use crate::components::input::editor::InputState;

pub struct EditorView {
    editor: Entity<InputState>,
    file_path: Option<SharedString>,
}

impl EditorView {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let editor = cx.new(|cx| InputState::new(window, cx).multi_line(true));
        Self {
            editor,
            file_path: None,
        }
    }

    pub fn with_file(
        path: impl Into<SharedString>,
        content: &str,
        language: Option<&str>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let path = path.into();
        let content = content.to_string();
        let lang = language.map(|s| s.to_string()).unwrap_or_else(|| "text".to_string());
        let editor = cx.new(move |cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(lang)
                .multi_line(true);
            state.set_value(&content, window, cx);
            state
        });
        Self {
            editor,
            file_path: Some(path),
        }
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.editor.clone())
    }
}
