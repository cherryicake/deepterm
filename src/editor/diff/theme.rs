//! Colours and type for the diff view, from the app's active theme.
//!
//! The tints follow rune-kit's diff look: added and removed lines take a quiet wash of the
//! success and danger colours over the editor background, their gutters a stronger one, and
//! word changes a stronger one still, with a coloured bar at the row's edge. GPUI blends in
//! sRGB, so the ratios are tuned per appearance to land on the same measured colours as the
//! Rune component.

use gpui::{App, FontStyle, FontWeight, Hsla, Pixels, SharedString, px};

use crate::editor::syntax::HighlightTheme;
use crate::settings::FontState;
use crate::theme::ActiveTheme;

/// Width of the coloured bar that marks an added or removed line.
pub const BAR: f32 = 3.0;
/// Space between the line numbers and the code, and after the code.
pub const CODE_PAD: (f32, f32) = (12.0, 16.0);
/// Height of a file's header row.
pub const HEADER_H: f32 = 36.0;
/// Height of a folded unchanged range's row.
pub const GAP_H: f32 = 28.0;
/// Height of a note row (binary file, no changes).
pub const NOTE_H: f32 = 32.0;

/// The five steps of a change's colour: the line's wash, its gutter's, a word change's, the
/// line number's, and the edge bar.
#[derive(Clone, Copy, Debug)]
pub struct Tints {
    pub line: Hsla,
    pub gutter: Hsla,
    pub word: Hsla,
    pub number: Hsla,
    pub bar: Hsla,
}

/// Colours and type, read from the app's theme and font settings.
#[derive(Clone)]
pub struct DiffTheme {
    pub font_family: SharedString,
    pub font_size: Pixels,
    /// The line height the code is laid out at.
    pub line_height: Pixels,
    /// The background of the whole view.
    pub bg: Hsla,
    /// The editor's own background (may differ slightly from `bg`).
    pub editor_bg: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub border: Hsla,
    pub header_bg: Hsla,
    pub hover: Hsla,
    pub add: Tints,
    pub del: Tints,
    /// The wash of a folded unchanged range's row.
    pub gap_bg: Hsla,
    /// The fill of a cell with no line on that side (split mode).
    pub empty_bg: Hsla,
    /// The fill of selected lines.
    pub selected: Hsla,
    /// The fill of a selected line's gutter.
    pub selected_gutter: Hsla,
    /// The colour of a text selection drawn over code.
    pub selection: Hsla,
    /// The accent used by buttons and the comment "+".
    pub accent: Hsla,
    pub on_accent: Hsla,
    /// The syntax colours for the current theme.
    pub syntax: HighlightTheme,
}

impl DiffTheme {
    pub fn new(cx: &App) -> DiffTheme {
        let theme = cx.theme();
        let ui = &theme.ui;
        let editor_bg = theme.editor_background();
        let bg = editor_bg;
        let dark = theme.appearance.is_dark();

        let font = cx
            .try_global::<FontState>()
            .map(|f| f.settings.clone())
            .unwrap_or_default();
        let font_size = px(font.editor_font_size);
        // The editor's own line height: 1.5× the font size, as the code editor lays text out.
        let line_height = px((font.editor_font_size * 1.5)
            .round()
            .max(font.editor_font_size + 4.0));

        let tints = |hue: Hsla, line_t: f32, gutter_t: f32, word_t: f32| Tints {
            line: tint(bg, hue, line_t),
            gutter: tint(bg, hue, gutter_t),
            word: tint(bg, hue, word_t),
            number: hue,
            bar: hue,
        };
        let (line_t, gutter_t, word_t) = if dark {
            (0.16, 0.24, 0.36)
        } else {
            (0.10, 0.17, 0.27)
        };

        DiffTheme {
            font_family: font.editor_font_family.clone().into(),
            font_size,
            line_height,
            bg,
            editor_bg,
            text: theme
                .highlight_theme
                .style
                .editor_foreground
                .unwrap_or(ui.text),
            muted: ui.text_muted,
            faint: ui.text_subtle,
            border: ui.border,
            header_bg: ui.panel_background,
            hover: ui.tab_hover_background,
            add: tints(ui.status_added, line_t, gutter_t, word_t),
            del: tints(ui.status_deleted, line_t, gutter_t, word_t),
            gap_bg: ui.elevated_background,
            empty_bg: ui
                .elevated_background
                .opacity(if dark { 0.35 } else { 0.5 }),
            selected: tint(bg, ui.accent, if dark { 0.22 } else { 0.16 }),
            selected_gutter: tint(bg, ui.accent, if dark { 0.32 } else { 0.26 }),
            selection: ui.selection,
            accent: ui.accent,
            on_accent: ui.accent_foreground,
            syntax: theme.highlight_theme.as_ref().clone(),
        }
    }

    /// The tints of a line kind, `None` for unchanged lines.
    pub fn tints(&self, kind: super::model::LineKind) -> Option<&Tints> {
        match kind {
            super::model::LineKind::Added => Some(&self.add),
            super::model::LineKind::Removed => Some(&self.del),
            super::model::LineKind::Context => None,
        }
    }

    /// The fill of a line-number cell.
    pub fn gutter_fill(&self, kind: super::model::LineKind, selected: bool) -> Hsla {
        match (selected, self.tints(kind)) {
            (true, _) => self.selected_gutter,
            (false, Some(t)) => t.gutter,
            (false, None) => self.editor_bg,
        }
    }

    /// The fill of a line's own row.
    pub fn line_fill(&self, kind: super::model::LineKind, selected: bool) -> Hsla {
        match (selected, self.tints(kind)) {
            (true, _) => self.selected,
            (false, Some(t)) => t.line,
            (false, None) => self.editor_bg,
        }
    }

    /// The text run shared by every glyph of code.
    pub fn code_font(&self) -> gpui::Font {
        gpui::Font {
            family: self.font_family.clone(),
            features: Default::default(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        }
    }

    /// The number of digits a line number column needs for `digits`, in points.
    pub fn number_width(&self, digits: u32) -> Pixels {
        // A mono digit is about 0.6 em wide.
        let adv = self.font_size.as_f32() * 0.6;
        px((digits as f32 * adv + 12.0).ceil())
    }
}

/// `t` of the way from `base` toward `hue`, keeping `base`'s alpha.
///
/// This is an sRGB mix (`Hsla::blend`), while rune mixes in OKLab; the tint ratios above are
/// tuned so the light and dark results land on the same measured colours.
fn tint(base: Hsla, hue: Hsla, t: f32) -> Hsla {
    base.blend(hue.opacity(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tints_sit_between_the_background_and_the_status_colour() {
        let bg = Hsla::white();
        let green = gpui::rgb(0x00792f).into();
        let t = tint(bg, green, 0.1);
        let (a, b) = (gpui::white().l, t.l);
        assert!(b < a, "a wash of green over white darkens it: {a} -> {b}");
        // The three steps get stronger in order.
        let (gutter, word) = (tint(bg, green, 0.17), tint(bg, green, 0.27));
        assert!(gutter.l < t.l && word.l < gutter.l);
    }
}
