//! Color mappings for terminal

use alacritty_terminal::vte::ansi::Color as AlacColor;
use gpui::Hsla;

use crate::theme::TerminalTheme;

/// Convert alacritty color to GPUI Hsla
pub fn convert_alacritty_color(color: &AlacColor, theme: &TerminalTheme) -> Hsla {
    match color {
        AlacColor::Named(named) => theme.named_color(*named),
        AlacColor::Spec(rgb) => {
            gpui::rgb((rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32).into()
        }
        // Indexed colors resolve to the theme's pre-built 256-entry palette
        // (one O(1) array lookup) instead of recomputing the `Rgb -> Hsla`
        // conversion and the cube/grayscale math on every call. This is on the
        // per-cell render hot path (`text_run_for_cell`, background spans, etc).
        AlacColor::Indexed(idx) => theme.indexed_color(*idx),
    }
}
