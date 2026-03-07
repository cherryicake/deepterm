//! Keyboard input mappings for terminal
//!
//! Maps GPUI keystrokes to terminal escape sequences
//! Based on Zed's terminal implementation

use gpui::{Keystroke, Modifiers};

/// Convert a keystroke to terminal input bytes
/// Follows Zed's approach: process based on keystroke.key, not key_char
pub fn keystroke_to_bytes(
    keystroke: &Keystroke,
    _modifiers: &Modifiers,
    _mode: TermMode,
) -> Option<Vec<u8>> {
    let key = keystroke.key.as_str();

    // Handle special keys
    match key {
        "return" | "enter" => return Some(vec![b'\r']),
        "tab" => return Some(vec![b'\t']),
        "backspace" => return Some(vec![0x7f]),
        "escape" => return Some(vec![0x1b]),
        "space" => return Some(vec![b' ']),
        "up" => return Some(encode_csi("A", keystroke)),
        "down" => return Some(encode_csi("B", keystroke)),
        "right" => return Some(encode_csi("C", keystroke)),
        "left" => return Some(encode_csi("D", keystroke)),
        "home" => return Some(encode_csi("H", keystroke)),
        "end" => return Some(encode_csi("F", keystroke)),
        "pageup" => return Some(encode_csi("5~", keystroke)),
        "pagedown" => return Some(encode_csi("6~", keystroke)),
        "delete" => return Some(encode_csi("3~", keystroke)),
        "insert" => return Some(encode_csi("2~", keystroke)),
        _ => {}
    }

    // Handle function keys F1-F12
    if let Some(f_num) = key.strip_prefix('f') {
        if let Ok(n) = f_num.parse::<u8>() {
            if n >= 1 && n <= 12 {
                let seq = match n {
                    1 => "OP",
                    2 => "OQ",
                    3 => "OR",
                    4 => "OS",
                    5..=8 => &format!("{}~", n + 10),
                    9..=12 => &format!("{}~", n + 11),
                    _ => return None,
                };
                return Some(encode_csi(seq, keystroke));
            }
        }
    }

    // Handle regular characters and multi-byte Unicode characters
    // Check if the key is a single character (could be multi-byte in UTF-8)
    let mut chars = key.chars();
    if let Some(ch) = chars.next() {
        // Make sure there's only one character
        if chars.next().is_none() {
            let mut bytes = vec![];

            // Handle Ctrl+key combinations
            if keystroke.modifiers.control {
                if ch.is_ascii_alphabetic() {
                    // Ctrl+A = 0x01, Ctrl+B = 0x02, etc.
                    // For uppercase letters, we still send the control code
                    // (same as lowercase in terminal)
                    let lower = ch.to_ascii_lowercase();
                    bytes.push((lower as u8) & 0x1f);
                    return Some(bytes);
                }
                match ch {
                    '2' | '@' | ' ' => bytes.push(0),
                    '3' | '[' => bytes.push(0x1b),
                    '4' | '\\' => bytes.push(0x1c),
                    '5' | ']' => bytes.push(0x1d),
                    '6' | '^' => bytes.push(0x1e),
                    '7' | '_' | '/' => bytes.push(0x1f),
                    '8' | '?' => bytes.push(0x7f),
                    _ => {}
                }
                if !bytes.is_empty() {
                    return Some(bytes);
                }
            }

            // Handle Alt+key combinations (send ESC prefix)
            if keystroke.modifiers.alt {
                bytes.push(0x1b);
                let mut buf = [0; 4];
                ch.encode_utf8(&mut buf);
                bytes.extend_from_slice(&buf[..ch.len_utf8()]);
                return Some(bytes);
            }

            // For regular characters without modifiers, return None
            // This allows IME to handle the input and prevents duplicate characters
            // The IME will send the text via EntityInputHandler::replace_text_in_range
        }
    }

    None
}

/// Encode CSI sequence with modifiers
fn encode_csi(suffix: &str, keystroke: &Keystroke) -> Vec<u8> {
    let mut result = vec![0x1b, b'['];

    // Add modifier parameter if needed
    let modifier = encode_modifiers(keystroke);
    if modifier > 1 || suffix.contains('~') {
        // For keys with ~ suffix, insert modifier before ~
        if suffix.contains('~') {
            let num: String = suffix.chars().take_while(|c| c.is_ascii_digit()).collect();
            result.extend_from_slice(num.as_bytes());
            if modifier > 1 {
                result.push(b';');
                result.push(b'0' + modifier);
            }
            result.push(b'~');
        } else {
            if modifier > 1 {
                result.push(b'1');
                result.push(b';');
                result.push(b'0' + modifier);
            }
            result.extend_from_slice(suffix.as_bytes());
        }
    } else {
        result.extend_from_slice(suffix.as_bytes());
    }

    result
}

/// Encode modifier bits
fn encode_modifiers(keystroke: &Keystroke) -> u8 {
    let mut modifier = 1u8;
    if keystroke.modifiers.shift {
        modifier += 1;
    }
    if keystroke.modifiers.alt {
        modifier += 2;
    }
    if keystroke.modifiers.control {
        modifier += 4;
    }
    modifier
}

/// Terminal mode flags (simplified)
#[derive(Clone, Copy, Debug, Default)]
pub struct TermMode {
    pub cursor_keys: bool,
    pub keypad: bool,
    pub bracketed_paste: bool,
    pub mouse_mode: MouseMode,
}

/// Mouse reporting mode
#[derive(Clone, Copy, Debug, Default)]
pub enum MouseMode {
    #[default]
    None,
    X10,
    Normal,
    Sgr,
    Utf8,
}
