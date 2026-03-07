//! ANSI escape sequence parser
//!
//! Parses ANSI color codes and other terminal escape sequences

use gpui::Hsla;

/// ANSI color codes
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnsiColor {
    /// Standard 16 colors
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    /// 256 color palette
    Indexed(u8),
    /// RGB color
    Rgb(u8, u8, u8),
}

impl AnsiColor {
    /// Convert ANSI color to Hsla
    pub fn to_hsla(self) -> Hsla {
        match self {
            AnsiColor::Black => gpui::rgb(0x000000).into(),
            AnsiColor::Red => gpui::rgb(0xcd0000).into(),
            AnsiColor::Green => gpui::rgb(0x00cd00).into(),
            AnsiColor::Yellow => gpui::rgb(0xcdcd00).into(),
            AnsiColor::Blue => gpui::rgb(0x0000ee).into(),
            AnsiColor::Magenta => gpui::rgb(0xcd00cd).into(),
            AnsiColor::Cyan => gpui::rgb(0x00cdcd).into(),
            AnsiColor::White => gpui::rgb(0xe5e5e5).into(),
            AnsiColor::BrightBlack => gpui::rgb(0x7f7f7f).into(),
            AnsiColor::BrightRed => gpui::rgb(0xff0000).into(),
            AnsiColor::BrightGreen => gpui::rgb(0x00ff00).into(),
            AnsiColor::BrightYellow => gpui::rgb(0xffff00).into(),
            AnsiColor::BrightBlue => gpui::rgb(0x5c5cff).into(),
            AnsiColor::BrightMagenta => gpui::rgb(0xff00ff).into(),
            AnsiColor::BrightCyan => gpui::rgb(0x00ffff).into(),
            AnsiColor::BrightWhite => gpui::rgb(0xffffff).into(),
            AnsiColor::Indexed(idx) => {
                // Convert 256 color index to RGB
                let (r, g, b) = if idx < 16 {
                    // Standard 16 colors
                    AnsiColor::from_standard_16(idx).to_rgb()
                } else if idx < 232 {
                    // 216 color cube (6x6x6)
                    let idx = idx - 16;
                    let r = (idx / 36) as u8;
                    let g = ((idx % 36) / 6) as u8;
                    let b = (idx % 6) as u8;
                    (
                        if r > 0 { r * 40 + 55 } else { 0 },
                        if g > 0 { g * 40 + 55 } else { 0 },
                        if b > 0 { b * 40 + 55 } else { 0 },
                    )
                } else {
                    // Grayscale (24 steps)
                    let gray = (idx - 232) as u8;
                    let value = gray * 10 + 8;
                    (value, value, value)
                };
                gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | (b as u32)).into()
            }
            AnsiColor::Rgb(r, g, b) => {
                gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | (b as u32)).into()
            }
        }
    }

    fn from_standard_16(idx: u8) -> AnsiColor {
        match idx {
            0 => AnsiColor::Black,
            1 => AnsiColor::Red,
            2 => AnsiColor::Green,
            3 => AnsiColor::Yellow,
            4 => AnsiColor::Blue,
            5 => AnsiColor::Magenta,
            6 => AnsiColor::Cyan,
            7 => AnsiColor::White,
            8 => AnsiColor::BrightBlack,
            9 => AnsiColor::BrightRed,
            10 => AnsiColor::BrightGreen,
            11 => AnsiColor::BrightYellow,
            12 => AnsiColor::BrightBlue,
            13 => AnsiColor::BrightMagenta,
            14 => AnsiColor::BrightCyan,
            15 => AnsiColor::BrightWhite,
            _ => AnsiColor::Black,
        }
    }

    fn to_rgb(self) -> (u8, u8, u8) {
        let hsla = self.to_hsla();
        // Simple HSLA to RGB conversion would go here
        // For now, return approximate values
        (128, 128, 128)
    }
}

/// ANSI escape sequence parser
pub struct AnsiParser {
    state: ParserState,
    buffer: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ParserState {
    Normal,
    Escape,
    Csi, // Control Sequence Introducer
    CsiParam,
    Osc, // Operating System Command
    OscParam,
}

/// Terminal style attributes
#[derive(Clone, Debug, Default)]
pub struct TerminalStyle {
    pub fg_color: Option<AnsiColor>,
    pub bg_color: Option<AnsiColor>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

impl TerminalStyle {
    /// Reset all attributes
    pub fn reset(&mut self) {
        *self = TerminalStyle::default();
    }
}

/// Parsed terminal output
#[derive(Clone, Debug)]
pub struct ParsedOutput {
    pub text: String,
    pub style: TerminalStyle,
}

impl AnsiParser {
    /// Create a new ANSI parser
    pub fn new() -> Self {
        Self {
            state: ParserState::Normal,
            buffer: String::new(),
        }
    }

    /// Parse input string and return parsed output
    pub fn parse(&mut self, input: &str) -> Vec<ParsedOutput> {
        let mut results = Vec::new();
        let mut current_style = TerminalStyle::default();
        let mut current_text = String::new();

        for ch in input.chars() {
            match self.state {
                ParserState::Normal => {
                    if ch == '\x1b' {
                        // ESC character
                        if !current_text.is_empty() {
                            results.push(ParsedOutput {
                                text: current_text.clone(),
                                style: current_style.clone(),
                            });
                            current_text.clear();
                        }
                        self.state = ParserState::Escape;
                    } else {
                        current_text.push(ch);
                    }
                }
                ParserState::Escape => {
                    match ch {
                        '[' => {
                            self.state = ParserState::Csi;
                            self.buffer.clear();
                        }
                        ']' => {
                            self.state = ParserState::Osc;
                            self.buffer.clear();
                        }
                        _ => {
                            // Unknown escape sequence, ignore
                            self.state = ParserState::Normal;
                        }
                    }
                }
                ParserState::Csi => {
                    if ch == '?' || ch == '>' || ch == '!' {
                        // Private sequence introducer, skip it
                        self.buffer.push(ch);
                    } else if ch.is_ascii_digit() || ch == ';' {
                        self.buffer.push(ch);
                        self.state = ParserState::CsiParam;
                    } else if ch.is_ascii_alphabetic() || ch == '@' {
                        // End of CSI sequence
                        self.process_csi(ch, &mut current_style);
                        self.state = ParserState::Normal;
                    } else {
                        // Invalid character in CSI sequence, discard buffer
                        self.buffer.clear();
                        self.state = ParserState::Normal;
                    }
                }
                ParserState::CsiParam => {
                    if ch.is_ascii_digit() || ch == ';' || ch == '?' || ch == '>' || ch == '!' {
                        self.buffer.push(ch);
                    } else if ch.is_ascii_alphabetic() || ch == '@' {
                        // End of CSI sequence
                        self.process_csi(ch, &mut current_style);
                        self.state = ParserState::Normal;
                    } else {
                        // Invalid character in CSI sequence, discard buffer
                        self.buffer.clear();
                        self.state = ParserState::Normal;
                    }
                }
                ParserState::Osc => {
                    if ch.is_ascii_digit() {
                        self.buffer.push(ch);
                        self.state = ParserState::OscParam;
                    } else {
                        self.state = ParserState::Normal;
                    }
                }
                ParserState::OscParam => {
                    if ch == '\x07' || ch == '\x1b' {
                        // End of OSC sequence
                        self.process_osc();
                        self.state = ParserState::Normal;
                    } else {
                        self.buffer.push(ch);
                    }
                }
            }
        }

        // Add remaining text
        if !current_text.is_empty() {
            results.push(ParsedOutput {
                text: current_text,
                style: current_style,
            });
        }

        results
    }

    fn process_csi(&self, command: char, style: &mut TerminalStyle) {
        // Check if this is a private sequence (starts with ?, >, or !)
        let buffer = &self.buffer;
        if buffer.starts_with('?') || buffer.starts_with('>') || buffer.starts_with('!') {
            // Private sequence - ignore for now
            // These include: bracketed paste mode (?2004), etc.
            return;
        }

        let params: Vec<u8> = buffer.split(';').filter_map(|s| s.parse().ok()).collect();

        match command {
            'm' => {
                // SGR - Select Graphic Rendition
                self.process_sgr(&params, style);
            }
            _ => {
                // Other CSI sequences - ignore for now
            }
        }
    }

    fn process_sgr(&self, params: &[u8], style: &mut TerminalStyle) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => style.reset(),
                1 => style.bold = true,
                3 => style.italic = true,
                4 => style.underline = true,
                9 => style.strikethrough = true,
                22 => style.bold = false,
                23 => style.italic = false,
                24 => style.underline = false,
                29 => style.strikethrough = false,
                // Foreground colors (30-37)
                30 => style.fg_color = Some(AnsiColor::Black),
                31 => style.fg_color = Some(AnsiColor::Red),
                32 => style.fg_color = Some(AnsiColor::Green),
                33 => style.fg_color = Some(AnsiColor::Yellow),
                34 => style.fg_color = Some(AnsiColor::Blue),
                35 => style.fg_color = Some(AnsiColor::Magenta),
                36 => style.fg_color = Some(AnsiColor::Cyan),
                37 => style.fg_color = Some(AnsiColor::White),
                38 => {
                    // Extended foreground color
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            5 if i + 2 < params.len() => {
                                // 256 color
                                style.fg_color = Some(AnsiColor::Indexed(params[i + 2]));
                                i += 2;
                            }
                            2 if i + 4 < params.len() => {
                                // RGB
                                style.fg_color = Some(AnsiColor::Rgb(
                                    params[i + 2],
                                    params[i + 3],
                                    params[i + 4],
                                ));
                                i += 4;
                            }
                            _ => {}
                        }
                    }
                }
                39 => style.fg_color = None,
                // Background colors (40-47)
                40 => style.bg_color = Some(AnsiColor::Black),
                41 => style.bg_color = Some(AnsiColor::Red),
                42 => style.bg_color = Some(AnsiColor::Green),
                43 => style.bg_color = Some(AnsiColor::Yellow),
                44 => style.bg_color = Some(AnsiColor::Blue),
                45 => style.bg_color = Some(AnsiColor::Magenta),
                46 => style.bg_color = Some(AnsiColor::Cyan),
                47 => style.bg_color = Some(AnsiColor::White),
                48 => {
                    // Extended background color
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            5 if i + 2 < params.len() => {
                                // 256 color
                                style.bg_color = Some(AnsiColor::Indexed(params[i + 2]));
                                i += 2;
                            }
                            2 if i + 4 < params.len() => {
                                // RGB
                                style.bg_color = Some(AnsiColor::Rgb(
                                    params[i + 2],
                                    params[i + 3],
                                    params[i + 4],
                                ));
                                i += 4;
                            }
                            _ => {}
                        }
                    }
                }
                49 => style.bg_color = None,
                // Bright foreground colors (90-97)
                90 => style.fg_color = Some(AnsiColor::BrightBlack),
                91 => style.fg_color = Some(AnsiColor::BrightRed),
                92 => style.fg_color = Some(AnsiColor::BrightGreen),
                93 => style.fg_color = Some(AnsiColor::BrightYellow),
                94 => style.fg_color = Some(AnsiColor::BrightBlue),
                95 => style.fg_color = Some(AnsiColor::BrightMagenta),
                96 => style.fg_color = Some(AnsiColor::BrightCyan),
                97 => style.fg_color = Some(AnsiColor::BrightWhite),
                // Bright background colors (100-107)
                100 => style.bg_color = Some(AnsiColor::BrightBlack),
                101 => style.bg_color = Some(AnsiColor::BrightRed),
                102 => style.bg_color = Some(AnsiColor::BrightGreen),
                103 => style.bg_color = Some(AnsiColor::BrightYellow),
                104 => style.bg_color = Some(AnsiColor::BrightBlue),
                105 => style.bg_color = Some(AnsiColor::BrightMagenta),
                106 => style.bg_color = Some(AnsiColor::BrightCyan),
                107 => style.bg_color = Some(AnsiColor::BrightWhite),
                _ => {}
            }
            i += 1;
        }
    }

    fn process_osc(&self) {
        // OSC sequences - handle title changes, etc.
        // For now, just ignore
    }
}

impl Default for AnsiParser {
    fn default() -> Self {
        Self::new()
    }
}
