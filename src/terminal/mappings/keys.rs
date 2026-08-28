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
            if (1..=12).contains(&n) {
                // F1-F4 是 SS3 序列，终字符为 P/Q/R/S：
                // 无修饰键发 `ESC O P`，带修饰键发 `ESC [ 1;<m> P`。
                // 它们不是 CSI 序列，走 encode_csi 会变成 `ESC[OP`
                // （应用无法识别）或 `ESC[1;2OP`（多一个 O）。
                const F1_F4_FINAL: [u8; 4] = *b"PQRS";
                if n <= 4 {
                    return Some(encode_ss3_function_key(
                        F1_F4_FINAL[(n - 1) as usize],
                        keystroke,
                    ));
                }
                // F5-F12 对齐 xterm：15~ 17~ 18~ 19~ 20~ 21~ 23~ 24~
                // （16~/22~ 不是标准编码，应用无法识别）
                const F_SUFFIXES: [&str; 8] = [
                    "15~", "17~", "18~", "19~", // F5-F8
                    "20~", "21~", "23~", "24~", // F9-F12
                ];
                return Some(encode_csi(F_SUFFIXES[(n - 5) as usize], keystroke, mode));
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
        // Parse digits directly from suffix bytes without String allocation
        for b in suffix.bytes() {
            if b.is_ascii_digit() {
                result.push(b);
            } else {
                break;
            }
        }
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

/// F1-F4（终字符 P/Q/R/S）的编码：
/// - 无修饰键 → SS3 形式 `ESC O <final>`（如 F1 = `\x1bOP`）
/// - 带修饰键 → CSI 形式 `ESC [ 1;<modifier> <final>`（如 Shift+F1 = `\x1b[1;2P`）
///
/// 注意带修饰键时终字符前**没有** `O`。
fn encode_ss3_function_key(final_byte: u8, keystroke: &Keystroke) -> Vec<u8> {
    let modifier = encode_modifiers(keystroke);
    if modifier > 1 {
        vec![0x1b, b'[', b'1', b';', b'0' + modifier, final_byte]
    } else {
        vec![0x1b, b'O', final_byte]
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn keystroke(key: &str) -> Keystroke {
        Keystroke::parse(key).unwrap_or_else(|e| panic!("无法解析按键 {key}: {e:?}"))
    }

    fn esc(key: &str) -> String {
        let bytes = to_esc_str(&keystroke(key), &TermMode::empty(), false)
            .unwrap_or_else(|| panic!("{key} 未产生序列"));
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn f1_f4_use_ss3_without_modifiers() {
        assert_eq!(esc("f1"), "\u{1b}OP");
        assert_eq!(esc("f2"), "\u{1b}OQ");
        assert_eq!(esc("f3"), "\u{1b}OR");
        assert_eq!(esc("f4"), "\u{1b}OS");
    }

    #[test]
    fn f1_f4_with_modifiers_use_csi_without_o() {
        // Shift+F1 → ESC [ 1;2 P（终字符前没有 O）
        assert_eq!(esc("shift-f1"), "\u{1b}[1;2P");
        assert_eq!(esc("ctrl-f2"), "\u{1b}[1;5Q");
        assert_eq!(esc("alt-f4"), "\u{1b}[1;3S");
    }

    #[test]
    fn f5_f12_match_xterm_codes() {
        assert_eq!(esc("f5"), "\u{1b}[15~");
        assert_eq!(esc("f6"), "\u{1b}[17~");
        assert_eq!(esc("f7"), "\u{1b}[18~");
        assert_eq!(esc("f8"), "\u{1b}[19~");
        assert_eq!(esc("f9"), "\u{1b}[20~");
        assert_eq!(esc("f10"), "\u{1b}[21~");
        assert_eq!(esc("f11"), "\u{1b}[23~");
        assert_eq!(esc("f12"), "\u{1b}[24~");
    }

    #[test]
    fn f5_with_modifier_keeps_tilde_form() {
        assert_eq!(esc("shift-f5"), "\u{1b}[15;2~");
    }

    #[test]
    fn cursor_keys_follow_application_mode() {
        let mode = TermMode::APP_CURSOR;
        let up = to_esc_str(&keystroke("up"), &mode, false).unwrap();
        assert_eq!(String::from_utf8(up).unwrap(), "\u{1b}OA");

        // F1-F4 与 APP_CURSOR 无关，始终是 SS3
        let f1 = to_esc_str(&keystroke("f1"), &mode, false).unwrap();
        assert_eq!(String::from_utf8(f1).unwrap(), "\u{1b}OP");
    }

    #[test]
    fn modifiers_encode_shift_alt_control() {
        let ks = Keystroke {
            modifiers: Modifiers {
                shift: true,
                alt: true,
                control: true,
                ..Default::default()
            },
            key: "f1".into(),
            key_char: None,
        };
        let out = to_esc_str(&ks, &TermMode::empty(), false).unwrap();
        // shift(1) + alt(2) + control(4) + 1 = 8
        assert_eq!(String::from_utf8(out).unwrap(), "\u{1b}[1;8P");
    }
}
