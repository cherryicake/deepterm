//! Color mappings for terminal

use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor};
use gpui::Hsla;

use crate::theme::TerminalTheme;

/// Convert alacritty color to GPUI Hsla
pub fn convert_alacritty_color(color: &AlacColor, theme: &TerminalTheme) -> Hsla {
    match color {
        AlacColor::Named(named) => theme.named_color(*named),
        AlacColor::Spec(rgb) => {
            gpui::rgb((rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32).into()
        }
        AlacColor::Indexed(idx) => match idx {
            0..=15 => {
                let named = match idx {
                    0 => NamedColor::Black,
                    1 => NamedColor::Red,
                    2 => NamedColor::Green,
                    3 => NamedColor::Yellow,
                    4 => NamedColor::Blue,
                    5 => NamedColor::Magenta,
                    6 => NamedColor::Cyan,
                    7 => NamedColor::White,
                    8 => NamedColor::BrightBlack,
                    9 => NamedColor::BrightRed,
                    10 => NamedColor::BrightGreen,
                    11 => NamedColor::BrightYellow,
                    12 => NamedColor::BrightBlue,
                    13 => NamedColor::BrightMagenta,
                    14 => NamedColor::BrightCyan,
                    15 => NamedColor::BrightWhite,
                    _ => NamedColor::Foreground,
                };
                convert_alacritty_color(&AlacColor::Named(named), theme)
            }
            16..=231 => {
                let idx = *idx - 16;
                let r = (idx / 36) as u8;
                let g = ((idx % 36) / 6) as u8;
                let b = (idx % 6) as u8;
                let r = if r == 0 { 0 } else { r * 40 + 55 };
                let g = if g == 0 { 0 } else { g * 40 + 55 };
                let b = if b == 0 { 0 } else { b * 40 + 55 };
                gpui::rgb((r as u32) << 16 | (g as u32) << 8 | b as u32).into()
            }
            232..=255 => {
                let gray = 8 + (*idx - 232) as u8 * 10;
                gpui::rgb((gray as u32) << 16 | (gray as u32) << 8 | gray as u32).into()
            }
        },
    }
}
