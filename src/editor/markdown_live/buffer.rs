//! File-level wrapper around [`crate::editor::markdown_live::model::EditorModel`].
//!
//! `EditorModel` (ported verbatim from guise) owns lines + cursor +
//! selection + snapshot undo. `LiveBuffer` adds what a file editor needs:
//! a monotonic [`LiveBuffer::text_version`] (dirty gate for `observe`,
//! highlight/image caches), the disk baseline for [`LiveBuffer::is_dirty`],
//! and line-ending preservation for save.

use crate::editor::markdown_live::model::{EditorModel, Pos};

/// Monotonic document revision. Bumped on every text mutation (never on pure
/// cursor/selection moves) so observers and caches can cheaply detect edits.
pub type TextVersion = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    /// Detect from the raw file bytes (first `\n` decides, like `editor.rs`).
    pub fn detect(raw: &str) -> Self {
        if raw.find('\n').is_some_and(|i| i > 0 && raw.as_bytes()[i - 1] == b'\r') {
            // Only checks the first newline; mixed files follow the majority
            // opener. Save normalizes through this ending.
            Self::Crlf
        } else {
            Self::Lf
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

/// Single live markdown document: editing truth + file sync metadata.
#[derive(Debug, Clone)]
pub struct LiveBuffer {
    model: EditorModel,
    version: TextVersion,
    /// Disk baseline (`original_content`): `is_dirty = text() != original`.
    original: String,
    pub line_ending: LineEnding,
}

impl LiveBuffer {
    pub fn new(text: &str) -> Self {
        let ending = LineEnding::detect(text);
        // Model strips `\r` (split_lines); baseline keeps normalized text so
        // dirty compares like-for-like and save re-applies the ending.
        let model = EditorModel::new(text);
        let original = model.text();
        Self {
            model,
            version: 0,
            original,
            line_ending: ending,
        }
    }

    /// Load from disk bytes already decoded to UTF-8. Resets history, version
    /// keeps increasing so caches invalidate even when content is identical.
    pub fn set_text(&mut self, text: &str) {
        self.line_ending = LineEnding::detect(text);
        self.model.set_text(text);
        self.original = self.model.text();
        self.bump();
    }

    /// External reload that becomes the new clean baseline.
    pub fn set_baseline(&mut self, text: &str) {
        self.set_text(text);
    }

    /// Mark the current text as saved (clears dirty without touching history).
    pub fn mark_saved(&mut self) {
        self.original = self.model.text();
    }

    /// Text with the file's line ending applied (for save).
    pub fn text_for_save(&self) -> String {
        match self.line_ending {
            LineEnding::Lf => self.model.text(),
            LineEnding::Crlf => self.model.text().replace('\n', "\r\n"),
        }
    }

    pub fn text(&self) -> String {
        self.model.text()
    }

    pub fn is_dirty(&self) -> bool {
        self.model.text() != self.original
    }

    pub fn is_empty(&self) -> bool {
        self.model.is_empty()
    }

    pub fn text_version(&self) -> TextVersion {
        self.version
    }

    pub fn line_count(&self) -> usize {
        self.model.line_count()
    }

    pub fn line(&self, i: usize) -> Option<&str> {
        self.model.line(i)
    }

    pub fn lines(&self) -> &[String] {
        self.model.lines()
    }

    pub fn cursor(&self) -> Pos {
        self.model.cursor()
    }

    pub fn selection(&self) -> Option<(Pos, Pos)> {
        self.model.selection()
    }

    pub fn model(&self) -> &EditorModel {
        &self.model
    }

    /// Mutate through the model; bumps the version iff the text changed.
    /// Returns the mutation's return value. Uses the model's O(1) text
    /// revision instead of joining the full document twice, so pure cursor
    /// moves never allocate.
    pub fn edit<R>(&mut self, f: impl FnOnce(&mut EditorModel) -> R) -> R {
        let before = self.model.text_version();
        let out = f(&mut self.model);
        if self.model.text_version() != before {
            self.bump();
        }
        out
    }

    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_bumps_on_edit_not_on_move() {
        let mut b = LiveBuffer::new("hello");
        assert_eq!(b.text_version(), 0);
        assert!(!b.is_dirty());
        b.edit(|m| m.move_to(0, 5, false));
        assert_eq!(b.text_version(), 0, "cursor move must not bump");
        b.edit(|m| m.insert("!"));
        assert_eq!(b.text_version(), 1);
        assert!(b.is_dirty());
        b.mark_saved();
        assert!(!b.is_dirty());
    }

    #[test]
    fn crlf_detected_and_restored_on_save() {
        let mut b = LiveBuffer::new("a\r\nb");
        assert_eq!(b.line_ending, LineEnding::Crlf);
        assert_eq!(b.text(), "a\nb", "model normalizes to LF");
        assert_eq!(b.text_for_save(), "a\r\nb");
        b.edit(|m| m.insert("!"));
        assert_eq!(b.text_for_save(), "!a\r\nb");
    }

    #[test]
    fn set_text_resets_dirty_and_invalidates() {
        let mut b = LiveBuffer::new("a");
        b.edit(|m| m.insert("b"));
        assert!(b.is_dirty());
        let v = b.text_version();
        b.set_text("a");
        assert!(!b.is_dirty());
        assert!(b.text_version() > v, "reload must invalidate caches");
    }
}
