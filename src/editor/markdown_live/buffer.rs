//! File-level wrapper around [`crate::editor::markdown_live::model::EditorModel`].
//!
//! `EditorModel` (ported verbatim from guise) owns lines + cursor +
//! selection + snapshot undo. `LiveBuffer` adds what a file editor needs:
//! a monotonic [`LiveBuffer::text_version`] (dirty gate for `observe`,
//! highlight/image caches), the disk baseline for [`LiveBuffer::is_dirty`],
//! and line-ending preservation for save.

use crate::editor::markdown_live::model::{EditorModel, Line, Pos};

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
    /// `text_version` at which `original` was last synchronized with the
    /// document (load / save / external reload). Comparing versions is O(1),
    /// which is what lets the editor skip per-keystroke full-document dirty
    /// comparisons — the old path joined the whole document to compare
    /// against `original` on every edit.
    ///
    /// A version match guarantees the text is unchanged since the baseline was
    /// taken (versions only advance on real text mutations), so `is_dirty` can
    /// answer `false` without touching the text. A mismatch falls back to the
    /// full comparison, keeping the answer exact even if the text was edited
    /// back to its original content (which must read as *not* dirty).
    clean_version: TextVersion,
    pub line_ending: LineEnding,
    /// Byte offset of every line start, plus the total text length at the
    /// end (`len == line_count + 1`). A pure function of the text, rebuilt
    /// on every text mutation.
    ///
    /// This turns offset ↔ [`Pos`] conversion from an O(lines) scan into an
    /// O(log lines) binary search. That matters on the hot paths: search
    /// highlighting converts *every* match range (O(matches × lines) before,
    /// which is what made a 10k-line document stutter while typing with the
    /// find panel open), and the IME path asks for the caret offset
    /// repeatedly. The rebuild itself is one linear pass of additions over
    /// the line table — no allocation beyond reusing this `Vec` — and it
    /// replaces several O(n) scans per keystroke.
    line_starts: Vec<usize>,
}

impl LiveBuffer {
    pub fn new(text: &str) -> Self {
        let ending = LineEnding::detect(text);
        // Model strips `\r` (split_lines); baseline keeps normalized text so
        // dirty compares like-for-like and save re-applies the ending.
        let model = EditorModel::new(text);
        let original = model.text();
        let mut buf = Self {
            model,
            version: 0,
            original,
            clean_version: 0,
            line_ending: ending,
            line_starts: Vec::new(),
        };
        buf.rebuild_line_starts();
        buf
    }

    /// Recompute the line-start prefix table from the current line table.
    fn rebuild_line_starts(&mut self) {
        let lines = self.model.lines();
        self.line_starts.clear();
        self.line_starts.reserve(lines.len() + 1);
        let mut off = 0usize;
        for line in lines {
            self.line_starts.push(off);
            off += line.len() + 1;
        }
        self.line_starts.push(off);
    }

    /// Load from disk bytes already decoded to UTF-8. Resets history, version
    /// keeps increasing so caches invalidate even when content is identical.
    pub fn set_text(&mut self, text: &str) {
        self.line_ending = LineEnding::detect(text);
        self.model.set_text(text);
        self.original = self.model.text();
        self.rebuild_line_starts();
        self.bump();
        self.clean_version = self.version;
    }

    /// External reload that becomes the new clean baseline.
    pub fn set_baseline(&mut self, text: &str) {
        self.set_text(text);
    }

    /// Mark the current text as saved (clears dirty without touching history).
    pub fn mark_saved(&mut self) {
        self.original = self.model.text();
        self.clean_version = self.version;
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
        // O(1) fast path: no text mutation since the baseline was taken.
        if self.version == self.clean_version {
            return false;
        }
        // Text changed (or changed and changed back): compare exactly.
        self.model.text() != self.original
    }

    /// O(1) dirty check that may answer `false` when the text was edited back
    /// to its baseline. Only for callers that re-check on the next edit (the
    /// tab's modified marker); `is_dirty` remains exact.
    pub fn is_dirty_fast(&self) -> bool {
        self.version != self.clean_version
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

    pub fn lines(&self) -> &[Line] {
        self.model.lines()
    }

    pub fn cursor(&self) -> Pos {
        self.model.cursor()
    }

    /// Byte offset of `pos` in the joined document (`lines` joined by `\n`).
    /// O(log lines).
    pub fn offset_of_pos(&self, pos: Pos) -> usize {
        let line = pos.line.min(self.model.lines().len().saturating_sub(1));
        let start = self
            .line_starts
            .get(line)
            .copied()
            .unwrap_or(0);
        start + crate::editor::markdown_live::layout::byte_for_col(
            self.model.line(line).unwrap_or(""),
            pos.col,
        )
    }

    /// [`Pos`] of a joined-document byte offset. O(log lines).
    /// Interior offsets floor to the char start; past-end clamps to the end.
    pub fn pos_of_offset(&self, off: usize) -> Pos {
        let lines = self.model.lines();
        let total = self.line_starts.last().copied().unwrap_or(0);
        let off = off.min(total);
        // `partition_point` finds the first line start strictly after `off`;
        // the line before it contains the offset. `off == line_starts[i]`
        // (line start) must land on line `i`, hence `<=`.
        let idx = self.line_starts.partition_point(|&s| s <= off);
        let line = idx.saturating_sub(1).min(lines.len().saturating_sub(1));
        let start = self.line_starts.get(line).copied().unwrap_or(0);
        let local = off.saturating_sub(start);
        Pos::new(
            line,
            crate::editor::markdown_live::layout::col_for_byte(
                lines.get(line).map(|l| l.as_str()).unwrap_or(""),
                local,
            ),
        )
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
            self.rebuild_line_starts();
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

    /// The offset index must agree with the naive scan it replaces, for every
    /// byte offset of a document with mixed line lengths and multibyte text.
    #[test]
    fn offset_index_matches_naive_scan() {
        use crate::editor::markdown_live::interactions::pos_of_offset;
        let texts = [
            "",
            "a",
            "a\nb",
            "a\n\nb",
            "hello\nworld\n",
            "\n\n\n",
            "中文行\nsecond line\n第三行有 emoji 🎉\n",
            "trailing newline\n",
        ];
        for text in texts {
            let mut b = LiveBuffer::new(text);
            let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(text);
            let total: usize = lines.iter().map(|l| l.len() + 1).sum::<usize>();
            for off in 0..=total {
                let fast = b.pos_of_offset(off);
                let slow = pos_of_offset(&lines, off);
                assert_eq!(
                    (fast.line, fast.col),
                    (slow.line, slow.col),
                    "pos_of_offset({off}) diverged for {text:?}"
                );
            }
            // Round-trip through `offset_of_pos` for real positions.
            for (li, line) in lines.iter().enumerate() {
                for col in 0..=line.chars().count() {
                    let pos = Pos::new(li, col);
                    let off = b.offset_of_pos(pos);
                    let back = b.pos_of_offset(off);
                    assert_eq!(
                        (back.line, back.col),
                        (li, col),
                        "round trip failed for {pos:?} in {text:?}"
                    );
                }
            }
            // Edits must keep the index in sync.
            b.edit(|m| m.insert("X\nY"));
            let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(&b.text());
            let total: usize = lines.iter().map(|l| l.len() + 1).sum::<usize>();
            for off in 0..=total {
                let fast = b.pos_of_offset(off);
                let slow = pos_of_offset(&lines, off);
                assert_eq!(
                    (fast.line, fast.col),
                    (slow.line, slow.col),
                    "post-edit pos_of_offset({off}) diverged for {text:?}"
                );
            }
        }
    }
}
