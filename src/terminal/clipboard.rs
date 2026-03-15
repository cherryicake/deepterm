//! OSC 52 Clipboard integration for terminal
//!
//! OSC 52 is a standard escape sequence that allows terminal applications
//! (like tmux, vim, neovim) to interact with the system clipboard.
//!
//! Format: ESC ] 52 ; <clipboard> ; <data> BEL
//!         ESC ] 52 ; <clipboard> ; <data> ESC \
//!
//! Where:
//! - <clipboard> is 'c' (clipboard), 'p' (primary), 's' (secondary), or 'q' (selection)
//! - <data> is base64-encoded text, or '?' to query, or empty to clear

use gpui::{App, ClipboardItem};

/// OSC 52 clipboard operation
#[derive(Debug, Clone)]
pub enum Osc52Operation {
    /// Set clipboard content
    Set { clipboard: char, data: String },
    /// Query clipboard content (not implemented - security concern)
    Query { clipboard: char },
    /// Clear clipboard
    Clear { clipboard: char },
}

/// Parse OSC 52 escape sequence
/// Returns Some(Osc52Operation) if valid OSC 52, None otherwise
pub fn parse_osc52(data: &str) -> Option<Osc52Operation> {
    // OSC 52 format: "52;<clipboard>;<base64-data>"
    let mut parts = data.splitn(3, ';');

    let prefix = parts.next()?;
    if prefix != "52" {
        return None;
    }

    let clipboard = parts.next()?;
    if clipboard.is_empty() {
        return None;
    }
    let clipboard_char = clipboard.chars().next()?;

    let data_part = parts.next().unwrap_or("");

    if data_part == "?" {
        Some(Osc52Operation::Query {
            clipboard: clipboard_char,
        })
    } else if data_part.is_empty() {
        Some(Osc52Operation::Clear {
            clipboard: clipboard_char,
        })
    } else {
        // Decode base64 data
        match base64_decode(data_part) {
            Some(decoded) => Some(Osc52Operation::Set {
                clipboard: clipboard_char,
                data: decoded,
            }),
            None => None,
        }
    }
}

/// Handle OSC 52 operation by writing to system clipboard
pub fn handle_osc52(cx: &mut App, operation: &Osc52Operation) {
    match operation {
        Osc52Operation::Set { clipboard: _, data } => {
            // Write to system clipboard
            // We treat all clipboard types ('c', 'p', 's', 'q') as system clipboard
            // as most modern systems only have one clipboard
            cx.write_to_clipboard(ClipboardItem::new_string(data.clone()));
        }
        Osc52Operation::Query { .. } => {
            // Query operation is not implemented for security reasons
            // We don't want terminal applications to be able to read clipboard
            // without user interaction
        }
        Osc52Operation::Clear { .. } => {
            // Clear operation - write empty string to clipboard
            cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
        }
    }
}

/// Base64 decoder using static lookup table for efficiency
fn base64_decode(input: &str) -> Option<String> {
    // Static lookup table: 256 bytes, no allocations, O(1) access
    // Value 0 means invalid character, values 1-64 are actual values + 1
    static DECODE_TABLE: [u8; 256] = {
        let mut table = [0u8; 256];
        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 {
            table[chars[i] as usize] = (i + 1) as u8;
            i += 1;
        }
        table
    };

    let mut result = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits_in_buffer = 0;

    for ch in input.bytes() {
        if ch == b'=' {
            // Padding character, ignore
            continue;
        }

        let val = DECODE_TABLE[ch as usize];
        if val == 0 {
            return None; // Invalid character
        }

        buffer = (buffer << 6) | (val - 1) as u32;
        bits_in_buffer += 6;

        if bits_in_buffer >= 8 {
            bits_in_buffer -= 8;
            result.push((buffer >> bits_in_buffer) as u8);
            buffer &= (1 << bits_in_buffer) - 1;
        }
    }

    String::from_utf8(result).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_decode() {
        assert_eq!(base64_decode("SGVsbG8="), Some("Hello".to_string()));
        assert_eq!(base64_decode("V29ybGQ="), Some("World".to_string()));
        assert_eq!(
            base64_decode("SGVsbG8gV29ybGQ="),
            Some("Hello World".to_string())
        );
        assert_eq!(base64_decode(""), Some("".to_string()));
    }

    #[test]
    fn test_parse_osc52_set() {
        let result = parse_osc52("52;c;SGVsbG8=");
        assert!(matches!(result, Some(Osc52Operation::Set { clipboard: 'c', data }) if data == "Hello"));
    }

    #[test]
    fn test_parse_osc52_query() {
        let result = parse_osc52("52;c;?");
        assert!(matches!(result, Some(Osc52Operation::Query { clipboard: 'c' })));
    }

    #[test]
    fn test_parse_osc52_clear() {
        let result = parse_osc52("52;c;");
        assert!(matches!(result, Some(Osc52Operation::Clear { clipboard: 'c' })));
    }

    #[test]
    fn test_parse_invalid() {
        assert!(parse_osc52("50;c;SGVsbG8=").is_none()); // Wrong OSC number
        assert!(parse_osc52("52;").is_none()); // Missing data
        assert!(parse_osc52("").is_none());
    }
}
