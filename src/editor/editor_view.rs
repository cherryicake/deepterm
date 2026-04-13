use gpui::prelude::*;
use gpui::{App, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div};

use crate::components::input::code_editor::Editor;

pub struct EditorView {
    editor: Entity<Editor>,
    file_path: Option<SharedString>,
}

impl EditorView {
    pub fn new(cx: &mut App) -> Self {
        let editor = cx.new(|cx| Editor::new(cx));
        Self {
            editor,
            file_path: None,
        }
    }

    pub fn with_file(
        path: impl Into<SharedString>,
        content: &str,
        language: Option<&str>,
        cx: &mut App,
    ) -> Self {
        let path = path.into();
        let editor = cx.new(|cx| {
            let mut e = Editor::new(cx).with_text(content);
            if let Some(lang) = language {
                e = e.language(lang);
            }
            e
        });
        Self {
            editor,
            file_path: Some(path),
        }
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.editor.read(_cx).focus_handle(_cx)
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.editor.clone())
    }
}
