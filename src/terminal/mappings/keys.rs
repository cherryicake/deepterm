//! Keyboard input mappings for terminal

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Convert keystroke to terminal escape sequence
pub fn to_esc_str(keystroke: &Keystroke, mode: &TermMode, _vi_mode: bool) -> Option<Vec<u8>> {
    let key = keystroke.key.as_str();

    // Handle special keys
    match key {
        "return" | "enter" => return Some(vec![b'\r']),
        "tab" => return Some(vec![b'\t']),
        "backspace" => return Some(vec![0x7f]),
        "escape" => return Some(vec![0x1b]),
        "space" => return Some(vec![b' ']),
        "up" => return Some(encode_csi("A", keystroke, mode)),
        "down" => return Some(encode_csi("B", keystroke, mode)),
        "right" => return Some(encode_csi("C", keystroke, mode)),
        "left" => return Some(encode_csi("D", keystroke, mode)),
        "home" => return Some(encode_csi("H", keystroke, mode)),
        "end" => return Some(encode_csi("F", keystroke, mode)),
        "pageup" => return Some(encode_csi("5~", keystroke, mode)),
        "pagedown" => return Some(encode_csi("6~", keystroke, mode)),
        "delete" => return Some(encode_csi("3~", keystroke, mode)),
        "insert" => return Some(encode_csi("2~", keystroke, mode)),
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
                return Some(encode_csi(seq, keystroke, mode));
            }
        }
    }

    // Handle regular characters
    let mut chars = key.chars();
    if let Some(ch) = chars.next() {
        if chars.next().is_none() {
            let mut bytes = vec![];

            // Handle Ctrl+key combinations
            if keystroke.modifiers.control {
                if ch.is_ascii_alphabetic() {
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

            // Handle Alt+key combinations
            if keystroke.modifiers.alt {
                bytes.push(0x1b);
                let mut buf = [0; 4];
                ch.encode_utf8(&mut buf);
                bytes.extend_from_slice(&buf[..ch.len_utf8()]);
                return Some(bytes);
            }
        }
    }

    None
}

fn encode_csi(suffix: &str, keystroke: &Keystroke, mode: &TermMode) -> Vec<u8> {
    let mut result = vec![0x1b, b'['];

    let modifier = encode_modifiers(keystroke);
    let application_mode = mode.contains(TermMode::APP_CURSOR);

    if suffix.contains('~') {
        let num: String = suffix.chars().take_while(|c| c.is_ascii_digit()).collect();
        result.extend_from_slice(num.as_bytes());
        if modifier > 1 {
            result.push(b';');
            result.push(b'0' + modifier);
        }
        result.push(b'~');
    } else {
        if application_mode && matches!(suffix, "A" | "B" | "C" | "D" | "H" | "F") {
            // Application mode: ESC O instead of ESC [
            result = vec![0x1b, b'O'];
        }
        if modifier > 1 {
            result.push(b'1');
            result.push(b';');
            result.push(b'0' + modifier);
        }
        result.extend_from_slice(suffix.as_bytes());
    }

    result
}

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
