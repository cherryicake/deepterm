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

#[derive(Clone, Debug, PartialEq)]
pub struct HyperlinkRange {
    pub start_col: usize,
    pub end_col: usize,
    pub uri: String,
    pub id: String,
}

/// Group contiguous cells carrying the same OSC8 hyperlink (`id` + `uri`)
/// into per-row ranges. Built from the `IndexedCell.hyperlink` snapshot so
/// `set_link_hover_active` can rebuild without holding the term lock.
pub fn detect_hyperlinks_in_line(cells: &[IndexedCell]) -> Vec<HyperlinkRange> {
    fn flush(
        current: &mut Option<(String, String, usize, usize)>,
        ranges: &mut Vec<HyperlinkRange>,
    ) {
        if let Some((id, uri, start, end)) = current.take() {
            ranges.push(HyperlinkRange {
                start_col: start,
                end_col: end,
                uri,
                id,
            });
        }
    }

    let mut ranges = Vec::new();
    let mut current: Option<(String, String, usize, usize)> = None;

    for cell in cells {
        let col = cell.point.column.0;
        let Some(link) = cell.hyperlink.as_ref().filter(|l| !l.uri().is_empty()) else {
            flush(&mut current, &mut ranges);
            continue;
        };
        // Extend only when the same link continues in the very next column;
        // any gap or link change closes the run so hit-test columns stay exact.
        let continues = matches!(&current,
            Some((cur_id, cur_uri, _, cur_end))
                if cur_id == link.id() && cur_uri == link.uri() && col == *cur_end);
        if continues {
            if let Some((_, _, _, cur_end)) = current.as_mut() {
                *cur_end = col + 1;
            }
            continue;
        }
        flush(&mut current, &mut ranges);
        current = Some((link.id().to_owned(), link.uri().to_owned(), col, col + 1));
    }
    flush(&mut current, &mut ranges);
    ranges
}

pub fn detect_urls_in_line(cells: &[IndexedCell]) -> Vec<UrlRange> {
    let mut text = String::new();
    // byte_offset -> column mapping: for each byte in text, which column it belongs to
    let mut byte_to_col: Vec<usize> = Vec::new();

    for cell in cells {
        let ch = cell.cell.c;
        if ch == '\0' {
            continue;
        }
        let col = cell.point.column.0;
        let len = ch.len_utf8();
        for _ in 0..len {
            byte_to_col.push(col);
        }
        text.push(ch);
    }

    let mut ranges = Vec::new();
    for mat in URL_REGEX.find_iter(&text) {
        // Direct byte-index lookup — O(1) instead of O(n) chars().count()
        let start_col = *byte_to_col.get(mat.start()).unwrap_or(&0);

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
        let end_col = byte_to_col
            .get(end_byte.saturating_sub(1))
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
