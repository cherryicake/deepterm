use std::rc::Rc;
use std::time::Duration;
use std::{cell::RefCell, ops::Range};

use gpui::{App, SharedString, Task};
use ropey::Rope;

use super::display_map::DisplayMap;
use crate::components::input::{InputEdit, RopeExt as _};
use crate::editor::syntax::{DiagnosticSet, SyntaxHighlighter, syntax_diagnostics_opt};

pub(crate) struct PendingBackgroundParse {
    pub highlighter: Rc<RefCell<Option<SyntaxHighlighter>>>,
    pub parse_task: Rc<RefCell<Option<Task<()>>>>,
    pub language: SharedString,
    pub text: Rope,
}

#[derive(Clone)]
pub(crate) enum InputMode {
    /// A plain text input mode.
    PlainText { multi_line: bool, rows: usize },
    /// An auto grow input mode.
    AutoGrow {
        rows: usize,
        min_rows: usize,
        max_rows: usize,
    },
    /// A code editor input mode.
    CodeEditor {
        multi_line: bool,
        rows: usize,
        /// Show line number
        line_number: bool,
        language: SharedString,
        folding: bool,
        highlighter: Rc<RefCell<Option<SyntaxHighlighter>>>,
        diagnostics: DiagnosticSet,
        parse_task: Rc<RefCell<Option<Task<()>>>>,
    },
}

impl Default for InputMode {
    fn default() -> Self {
        InputMode::plain_text()
    }
}

impl InputMode {
    /// Create a plain input mode with default settings.
    pub(super) fn plain_text() -> Self {
        InputMode::PlainText {
            multi_line: false,
            rows: 1,
        }
    }

    /// Create a code editor input mode with default settings.
    pub(super) fn code_editor(language: impl Into<SharedString>) -> Self {
        InputMode::CodeEditor {
            rows: 2,
            multi_line: true,
            language: language.into(),
            highlighter: Rc::new(RefCell::new(None)),
            line_number: true,
            folding: true,
            diagnostics: DiagnosticSet::new(&Rope::new()),
            parse_task: Rc::new(RefCell::new(None)),
        }
    }

    /// Create an auto grow input mode with given min and max rows.
    pub(super) fn auto_grow(min_rows: usize, max_rows: usize) -> Self {
        InputMode::AutoGrow {
            rows: min_rows,
            min_rows,
            max_rows,
        }
    }

    pub(super) fn multi_line(mut self, multi_line: bool) -> Self {
        match &mut self {
            InputMode::PlainText { multi_line: ml, .. } => *ml = multi_line,
            InputMode::CodeEditor { multi_line: ml, .. } => *ml = multi_line,
            InputMode::AutoGrow { .. } => {}
        }
        self
    }

    #[inline]
    pub(crate) fn is_single_line(&self) -> bool {
        !self.is_multi_line()
    }

    #[inline]
    pub(crate) fn is_code_editor(&self) -> bool {
        matches!(self, InputMode::CodeEditor { .. })
    }

    /// Return true if the mode is code editor and `folding: true`, `multi_line: true`.
    #[inline]
    pub(crate) fn is_folding(&self) -> bool {
        if cfg!(target_family = "wasm") {
            return false;
        }

        matches!(
            self,
            InputMode::CodeEditor {
                folding: true,
                multi_line: true,
                ..
            }
        )
    }

    #[inline]
    pub(crate) fn is_auto_grow(&self) -> bool {
        matches!(self, InputMode::AutoGrow { .. })
    }

    #[inline]
    pub(crate) fn is_multi_line(&self) -> bool {
        match self {
            InputMode::PlainText { multi_line, .. } => *multi_line,
            InputMode::CodeEditor { multi_line, .. } => *multi_line,
            InputMode::AutoGrow { max_rows, .. } => *max_rows > 1,
        }
    }

    pub(crate) fn set_rows(&mut self, new_rows: usize) {
        match self {
            InputMode::PlainText { rows, .. } => {
                *rows = new_rows;
            }
            InputMode::CodeEditor { rows, .. } => {
                *rows = new_rows;
            }
            InputMode::AutoGrow {
                rows,
                min_rows,
                max_rows,
            } => {
                *rows = new_rows.clamp(*min_rows, *max_rows);
            }
        }
    }

    pub(crate) fn update_auto_grow(&mut self, display_map: &DisplayMap) {
        if self.is_single_line() {
            return;
        }

        let wrapped_lines = display_map.wrap_row_count();
        self.set_rows(wrapped_lines);
    }

    /// At least 1 row be return.
    pub(crate) fn rows(&self) -> usize {
        if !self.is_multi_line() {
            return 1;
        }

        match self {
            InputMode::PlainText { rows, .. } => *rows,
            InputMode::CodeEditor { rows, .. } => *rows,
            InputMode::AutoGrow { rows, .. } => *rows,
        }
        .max(1)
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(crate) fn line_number(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                line_number,
                multi_line,
                ..
            } => *line_number && *multi_line,
            _ => false,
        }
    }

    /// Update the syntax highlighter with new text.
    ///
    /// Returns `Some(PendingBackgroundParse)` when the synchronous parse
    /// timed out and the caller should dispatch a background parse.
    /// Returns `None` when parsing completed (or no highlighter is active).
    pub(super) fn update_highlighter(
        &mut self,
        selected_range: &Range<usize>,
        text: &Rope,
        new_text: &str,
        force: bool,
        _cx: &mut App,
    ) -> Option<PendingBackgroundParse> {
        match &mut *self {
            InputMode::CodeEditor {
                language,
                highlighter,
                parse_task,
                diagnostics,
                ..
            } => {
                if !force && highlighter.borrow().is_some() {
                    return None;
                }

                let mut highlighter_ref = highlighter.borrow_mut();
                if highlighter_ref.is_none() {
                    let new_highlighter = SyntaxHighlighter::new(language);
                    highlighter_ref.replace(new_highlighter);
                }

                let h = highlighter_ref.as_mut()?;

                // When full text changed, the selected_range may be out of bound (The before version).
                let mut selected_range = selected_range.clone();
                selected_range.end = selected_range.end.min(text.len());

                // If insert a chart, this is 1.
                // If backspace or delete, this is -1.
                // If selected to delete, this is the length of the selected text.
                // let changed_len = new_text.len() as isize - selected_range.len() as isize;
                let changed_len = new_text.len() as isize - selected_range.len() as isize;
                let new_end = (selected_range.end as isize + changed_len) as usize;

                let start_pos = text.offset_to_point(selected_range.start);
                let old_end_pos = text.offset_to_point(selected_range.end);
                let new_end_pos = text.offset_to_point(new_end);

                let edit = InputEdit {
                    start_byte: selected_range.start,
                    old_end_byte: selected_range.end,
                    new_end_byte: new_end,
                    start_position: start_pos,
                    old_end_position: old_end_pos,
                    new_end_position: new_end_pos,
                };

                const SYNC_PARSE_TIMEOUT: Duration = Duration::from_millis(30);
                let completed = h.update(Some(edit), text, Some(SYNC_PARSE_TIMEOUT));
                if completed {
                    // 语法诊断与高亮共用同一棵树：同步解析成功时立即刷新。
                    // 超时时**保留旧诊断**（后台解析落地后会再刷一次），
                    // 否则每次击键都会让下划线闪一下。
                    diagnostics.set(syntax_diagnostics_opt(h.tree()), text);
                    // Sync parse succeeded, cancel any pending background parse.
                    parse_task.borrow_mut().take();
                    None
                } else {
                    // Timed out. Return the data needed for background parsing.
                    let pending = PendingBackgroundParse {
                        language: SharedString::from(h.language().to_string()),
                        text: text.clone(),
                        highlighter: highlighter.clone(),
                        parse_task: parse_task.clone(),
                    };
                    drop(highlighter_ref);
                    Some(pending)
                }
            }
            _ => None,
        }
    }

    pub(super) fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        match self {
            InputMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }

    /// Get a reference to the highlighter (if available)
    pub(super) fn highlighter(&self) -> Option<&Rc<RefCell<Option<SyntaxHighlighter>>>> {
        match self {
            InputMode::CodeEditor { highlighter, .. } => Some(highlighter),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ropey::Rope;

    use crate::editor::syntax::DiagnosticSet;

    use super::InputMode;

    #[test]
    fn test_code_editor() {
        let mode = InputMode::code_editor("rust");
        assert_eq!(mode.is_code_editor(), true);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), true);
        assert_eq!(mode.is_folding(), true);

        let mode = InputMode::CodeEditor {
            multi_line: false,
            line_number: true,
            folding: true,
            rows: 0,
            language: "rust".into(),
            highlighter: Default::default(),
            diagnostics: DiagnosticSet::new(&Rope::new()),
            parse_task: Default::default(),
        };
        assert_eq!(mode.is_code_editor(), true);
        assert_eq!(mode.is_multi_line(), false);
        assert_eq!(mode.is_single_line(), true);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.is_folding(), false);
    }

    #[test]
    fn test_plain() {
        let mode = InputMode::PlainText {
            multi_line: true,
            rows: 5,
        };
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 5);

        let mode = InputMode::plain_text();
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), false);
        assert_eq!(mode.is_single_line(), true);
        assert_eq!(mode.line_number(), false);
    }

    #[test]
    fn test_auto_grow() {
        let mut mode = InputMode::auto_grow(2, 5);
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 2);

        mode.set_rows(4);
        assert_eq!(mode.rows(), 4);

        mode.set_rows(1);
        assert_eq!(mode.rows(), 2);

        mode.set_rows(10);
        assert_eq!(mode.rows(), 5);
    }

    #[test]
    fn test_max_rows() {
        let mut mode = InputMode::auto_grow(2, 5);
        assert_eq!(mode.rows(), 2);

        mode.set_rows(4);
        assert_eq!(mode.rows(), 4);

        mode.set_rows(1);
        assert_eq!(mode.rows(), 2);

        mode.set_rows(10);
        assert_eq!(mode.rows(), 5);
    }
}
