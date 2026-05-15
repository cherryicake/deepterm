//! URL link detection in terminal content

use regex::Regex;
use std::sync::LazyLock;

use crate::terminal::terminal::IndexedCell;

static URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(https?://|www\.)[a-zA-Z0-9\-._~:/?#\[\]@!$&'()*+,;=%]+").unwrap()
});

/// Trailing characters that should not be included in URLs.
const URL_TRAILING_PUNCTUATION: &[u8] = b")>\"\'.,;:";

#[derive(Clone, Debug, PartialEq)]
pub struct UrlRange {
    pub start_col: usize,
    pub end_col: usize,
    pub url: String,
}

pub fn detect_urls_in_line(cells: &[IndexedCell]) -> Vec<UrlRange> {
    let mut text = String::new();
    let mut char_to_col: Vec<usize> = Vec::new();

    for cell in cells {
        let ch = cell.cell.c;
        if ch == '\0' {
            continue;
        }
        char_to_col.push(cell.point.column.0);
        text.push(ch);
    }

    let mut ranges = Vec::new();
    for mat in URL_REGEX.find_iter(&text) {
        let start_char_idx = text[..mat.start()].chars().count();

        // Trim trailing punctuation from the URL
        let raw = mat.as_str().as_bytes();
        let mut trim = 0;
        for b in raw.iter().rev() {
            if URL_TRAILING_PUNCTUATION.contains(b) {
                trim += 1;
            } else {
                break;
            }
        }
        let end_byte = mat.end() - trim;
        let end_char_idx = text[..end_byte].chars().count();

        let start_col = *char_to_col.get(start_char_idx).unwrap_or(&0);
        let end_col = char_to_col
            .get(end_char_idx.saturating_sub(1))
            .map(|c| *c + 1)
            .unwrap_or(start_col + 1);

        let url_str = std::str::from_utf8(&raw[..raw.len() - trim]).unwrap_or(mat.as_str());
        let mut url = url_str.to_string();
        if url.starts_with("www.") && !url.contains("://") {
            url = format!("https://{}", url);
        }

        ranges.push(UrlRange {
            start_col,
            end_col,
            url,
        });
    }
    ranges
}
