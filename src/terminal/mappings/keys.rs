//! Keyboard input mappings for terminal

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Convert keystroke to terminal escape sequence
///
/// Kitty 键盘协议激活时优先走 [`kitty_sequence_for`]（CSI u 编码），
/// 未覆盖的键（未修饰的字符 / 常规功能键）回退到 legacy xterm 编码。
pub fn to_esc_str(keystroke: &Keystroke, mode: &TermMode, _vi_mode: bool) -> Option<Vec<u8>> {
    if let Some(bytes) = kitty_sequence_for(keystroke, mode) {
        return Some(bytes);
    }
    legacy_esc_sequence(keystroke, mode)
}

// ── Kitty keyboard protocol (CSI u) ─────────────────────────────────────────────

/// Kitty 键盘协议下的键编码。
///
/// 返回 `None` 表示该键在此模式下仍应走 legacy 编码（或字符输入路径），
/// 由 [`legacy_esc_sequence`] 接手。
///
/// 参考 alacritty 的 `input/keyboard.rs`（MIT），针对 GPUI Keystroke 调整：
/// - GPUI 不提供按键位置（Numpad）与重复 / 释放事件，因此不编码
///   numpad 专用码，且不宣称事件类型（REPORT_EVENT_TYPES 不参与编码分支）；
/// - GPUI `key` 即基础布局字符（≈ alacritty 的 `key_without_modifiers`），
///   `key_char` 为实际产出文本（含 shift / alt 组合）。
fn kitty_sequence_for(keystroke: &Keystroke, mode: &TermMode) -> Option<Vec<u8>> {
    let kitty_seq = mode.intersects(
        TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::REPORT_EVENT_TYPES,
    );
    if !kitty_seq {
        return None;
    }
    let encode_all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let report_alternate = mode.contains(TermMode::REPORT_ALTERNATE_KEYS);
    let report_text = mode.contains(TermMode::REPORT_ASSOCIATED_TEXT);

    let mods = kitty_modifiers(keystroke);
    let has_mods = mods != 0;
    // 除 shift 外的修饰键（纯 shift 的字符键仍走文本路径，协议语义）
    let has_functional_mods = has_mods && mods != 0b0000_0001;

    // 命名控制键：Tab / Enter / Escape / Space / Backspace
    let named_base: Option<u32> = match keystroke.key.as_str() {
        "tab" => Some(9),
        "return" | "enter" => Some(13),
        "escape" => Some(27),
        "space" => Some(32),
        "backspace" => Some(127),
        _ => None,
    };
    if let Some(base) = named_base {
        // Escape 在 disambiguate 下恒走 CSI u（协议核心目的：与 alt 序列区分）；
        // 其余命名控制键仅在带修饰键或 encode_all 时用 CSI u；
        // 纯 shift 的 Tab/Enter/Backspace 同样 CSI u（协议明确要求消歧）。
        let shift_only_named = matches!(
            keystroke.key.as_str(),
            "tab" | "return" | "enter" | "backspace"
        ) && mods == 0b0000_0001;
        let needs_csi_u = keystroke.key == "escape"
            || encode_all
            || has_mods
            || shift_only_named;
        if needs_csi_u {
            // 这些键不产出文本（除 Space），text 段为 None。
            let text = if keystroke.key == "space" {
                associated_text_codepoint(keystroke, report_text)
            } else {
                None
            };
            return Some(kitty_csi_u(Some(base), None, mods, text));
        }
        return None;
    }

    // 字符键：CSI <codepoint>[:<shifted>] ; mods [;text] u
    let key = keystroke.key.as_str();
    if key.chars().count() == 1 {
        let base_ch = key.chars().next().unwrap();
        let typed = keystroke
            .key_char
            .as_deref()
            .and_then(|s| s.chars().next())
            .unwrap_or(base_ch);

        // encode_all：一切键均为 CSI u；
        // disambiguate：任何修饰键（非纯 shift）触发 CSI u。
        let should = encode_all || has_functional_mods;
        if !should {
            return None;
        }

        let unicode_key_code = base_ch as u32;
        let shifted_code = typed as u32;
        let payload = if report_alternate && shifted_code != unicode_key_code {
            format!("{unicode_key_code}:{shifted_code}")
        } else {
            unicode_key_code.to_string()
        };
        let text = associated_text_codepoint(keystroke, report_text);
        return Some(kitty_csi_u(None, Some(payload), mods, text));
    }

    None
}

/// 组装 `CSI [payload ;] mods[;text] u`。
///
/// - `base` / `payload` 二选一：数字基础码或含 alternate 的复合 payload；
/// - 修饰键位序（kitty 协议）：shift=1, alt=2, ctrl=4, super=8，
///   序列中值 = 位集 + 1；无修饰且无文本时可省略（kitty 默认 1）。
fn kitty_csi_u(
    base: Option<u32>,
    payload: Option<String>,
    mods: u8,
    text: Option<u32>,
) -> Vec<u8> {
    let mut seq = String::from("\x1b[");
    if let Some(p) = payload.as_deref() {
        seq.push_str(p);
    } else if let Some(b) = base {
        seq.push_str(&b.to_string());
    } else {
        // 无 payload（不应发生）：退化为 0
        seq.push('0');
    }
    if mods != 0 || text.is_some() {
        seq.push_str(&format!(";{}", mods + 1));
    }
    if let Some(t) = text {
        seq.push_str(&format!(";{t}"));
    }
    seq.push('u');
    seq.into_bytes()
}

/// Kitty 修饰键位集（不含 +1 偏移）：shift=1, alt=2, ctrl=4, super=8。
///
/// GPUI 的 `platform` 修饰键（macOS cmd / Windows win）对应 kitty 的 super。
fn kitty_modifiers(keystroke: &Keystroke) -> u8 {
    let mut bits = 0u8;
    if keystroke.modifiers.shift {
        bits |= 0b0000_0001;
    }
    if keystroke.modifiers.alt {
        bits |= 0b0000_0010;
    }
    if keystroke.modifiers.control {
        bits |= 0b0000_0100;
    }
    if keystroke.modifiers.platform {
        bits |= 0b0000_1000;
    }
    bits
}

/// 关联文本（flag 16）：单字符、非控制字符时返回其码点。
///
/// 修饰键产生的合成文本（macOS alt 组合如 `ß`）按协议也算关联文本；
/// 无产出文本（key_char 为 None / 空 / 控制字符）返回 None。
fn associated_text_codepoint(keystroke: &Keystroke, report_text: bool) -> Option<u32> {
    if !report_text {
        return None;
    }
    let text = keystroke.key_char.as_deref()?;
    let ch = text.chars().next()?;
    if text.chars().count() != 1 {
        return None;
    }
    let codepoint = ch as u32;
    // 控制字符（C0/C1/DEL）不带文本段。
    if codepoint < 0x20 || (0x7f..=0x9f).contains(&codepoint) {
        return None;
    }
    Some(codepoint)
}

// ── Legacy xterm encoding ───────────────────────────────────────────────────────

/// Legacy xterm 键编码（无 kitty 协议时的全部路径，与升级前行为一致）。
fn legacy_esc_sequence(keystroke: &Keystroke, mode: &TermMode) -> Option<Vec<u8>> {
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

    // ── Kitty keyboard protocol ───────────────────────────────────────────────────

    fn kitty_esc(key: &str, mode: TermMode) -> Option<String> {
        to_esc_str(&keystroke(key), &mode, false)
            .map(|b| String::from_utf8(b).unwrap())
    }

    fn disambiguate() -> TermMode {
        TermMode::DISAMBIGUATE_ESC_CODES
    }

    #[test]
    fn kitty_disambiguate_escape_becomes_csi_u() {
        // 协议核心：Escape 在 disambiguate 下为 CSI 27 u，与 alt 序列区分
        assert_eq!(kitty_esc("escape", disambiguate()).unwrap(), "\u{1b}[27u");
    }

    #[test]
    fn kitty_disambiguate_unmodified_enter_tab_stay_legacy() {
        // 未修饰的 Enter/Tab/Backspace 保持 legacy（\r / \t / DEL）
        assert_eq!(kitty_esc("enter", disambiguate()).unwrap(), "\r");
        assert_eq!(kitty_esc("tab", disambiguate()).unwrap(), "\t");
        assert_eq!(kitty_esc("backspace", disambiguate()).unwrap(), "\u{7f}");
    }

    #[test]
    fn kitty_disambiguate_modified_control_keys_use_csi_u() {
        assert_eq!(kitty_esc("ctrl-enter", disambiguate()).unwrap(), "\u{1b}[13;5u");
        assert_eq!(kitty_esc("shift-tab", disambiguate()).unwrap(), "\u{1b}[9;2u");
        assert_eq!(kitty_esc("alt-backspace", disambiguate()).unwrap(), "\u{1b}[127;3u");
        assert_eq!(kitty_esc("shift-enter", disambiguate()).unwrap(), "\u{1b}[13;2u");
    }

    #[test]
    fn kitty_disambiguate_ctrl_letter_uses_csi_u() {
        // ctrl+a → CSI 97;5u（替换 legacy \x01）
        assert_eq!(kitty_esc("ctrl-a", disambiguate()).unwrap(), "\u{1b}[97;5u");
        // alt+a → CSI 97;3u（替换 legacy ESC a）
        assert_eq!(kitty_esc("alt-a", disambiguate()).unwrap(), "\u{1b}[97;3u");
        // ctrl+shift+a → CSI 97;6u
        assert_eq!(kitty_esc("ctrl-shift-a", disambiguate()).unwrap(), "\u{1b}[97;6u");
    }

    #[test]
    fn kitty_disambiguate_plain_letter_falls_through_to_text_path() {
        // 未修饰字符不产生序列（走 IME / 文本提交路径）
        assert_eq!(kitty_esc("a", disambiguate()), None);
        // 纯 shift 的字符键同样走文本路径
        assert_eq!(kitty_esc("shift-a", disambiguate()), None);
    }

    #[test]
    fn kitty_alternate_keys_and_text_flags() {
        let mut mode = TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_ALTERNATE_KEYS
            | TermMode::REPORT_ASSOCIATED_TEXT;
        // alt+a → CSI 97:65;3u？—— alternate 报的是 shift 后的键位；
        // alt+a 无 shift（key_char 是 alt 组合或 None），此处验证不带 alternate。
        assert_eq!(kitty_esc("alt-a", mode).unwrap(), "\u{1b}[97;3u");

        // shift + ctrl + a：alternate = 'A'（shift 产出），text = 'A'
        let ks = Keystroke {
            modifiers: Modifiers {
                shift: true,
                control: true,
                ..Default::default()
            },
            key: "a".into(),
            key_char: Some("A".into()),
        };
        let out = to_esc_str(&ks, &mode, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\u{1b}[97:65;6;65u");

        // 关闭 alternate 后不携带 :65
        mode.remove(TermMode::REPORT_ALTERNATE_KEYS);
        let out = to_esc_str(&ks, &mode, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\u{1b}[97;6;65u");
    }

    #[test]
    fn kitty_report_all_keys_encodes_plain_letters() {
        let mode = TermMode::REPORT_ALL_KEYS_AS_ESC;
        // 未修饰 'a' → CSI 97u（此模式下应用自己从码点解码文本）
        let ks = Keystroke {
            modifiers: Modifiers::default(),
            key: "a".into(),
            key_char: Some("a".into()),
        };
        let out = to_esc_str(&ks, &mode, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\u{1b}[97u");

        // Enter 也要 CSI u
        assert_eq!(kitty_esc("enter", mode).unwrap(), "\u{1b}[13u");
        // Backspace → CSI 127u
        assert_eq!(kitty_esc("backspace", mode).unwrap(), "\u{1b}[127u");
    }

    #[test]
    fn kitty_arrows_remain_legacy_forms() {
        // 方向键 / 功能键在 kitty 协议下保持 CSI A / SS3 P 等既有形式
        assert_eq!(kitty_esc("up", disambiguate()).unwrap(), "\u{1b}[A");
        assert_eq!(kitty_esc("ctrl-up", disambiguate()).unwrap(), "\u{1b}[1;5A");
        assert_eq!(kitty_esc("f1", disambiguate()).unwrap(), "\u{1b}OP");
    }
}
