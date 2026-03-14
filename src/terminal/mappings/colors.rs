//! Color mappings for terminal

use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor};
use gpui::Hsla;

/// Convert alacritty color to GPUI Hsla
pub fn convert_alacritty_color(color: &AlacColor) -> Hsla {
    match color {
        AlacColor::Named(named) => match named {
            NamedColor::Black => gpui::rgb(0x181825).into(),
            NamedColor::Red => gpui::rgb(0xff728c).into(),
            NamedColor::Green => gpui::rgb(0x7bf797).into(),
            NamedColor::Yellow => gpui::rgb(0xffdb72).into(),
            NamedColor::Blue => gpui::rgb(0xb88eea).into(),
            NamedColor::Magenta => gpui::rgb(0x4c2d72).into(),
            NamedColor::Cyan => gpui::rgb(0xcba6f7).into(),
            NamedColor::White => gpui::rgb(0xc5c8d0).into(),
            NamedColor::BrightBlack => gpui::rgb(0x9994a0).into(),
            NamedColor::BrightRed => gpui::rgb(0xd08090).into(),
            NamedColor::BrightGreen => gpui::rgb(0x90c8a0).into(),
            NamedColor::BrightYellow => gpui::rgb(0xd4c894).into(),
            NamedColor::BrightBlue => gpui::rgb(0xb8a8d8).into(),
            NamedColor::BrightMagenta => gpui::rgb(0xdfc4ff).into(),
            NamedColor::BrightCyan => gpui::rgb(0xc5b8d8).into(),
            NamedColor::BrightWhite => gpui::rgb(0xd8dbe0).into(),
            NamedColor::Foreground => gpui::rgb(0x9aa3b8).into(),
            NamedColor::Background => gpui::rgb(0x1e1e2e).into(),
            NamedColor::Cursor => gpui::rgb(0xcba6f7).into(),
            _ => gpui::rgb(0xcdd6f4).into(),
        },
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
                convert_alacritty_color(&AlacColor::Named(named))
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
