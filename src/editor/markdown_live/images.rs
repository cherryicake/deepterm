//! Local image resolution helpers for live markdown.
//!
//! Mirrors the guards in `markdown_element::load_local_image`: remote URLs
//! never load, `..` escapes and non-image extensions are rejected, and files
//! over 20MB are refused. Loading itself stays in the view (background
//! executor + `Arc<Image>` cache); this module only decides *what* may load.

/// Max image file size: 20MB (same as the preview + editor guards).
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp", "svg", "ico"];

/// Normalize a markdown image destination to a loadable relative path, or
/// `None` when it must render as a placeholder (remote, unsafe, too long).
pub fn local_image_candidate(url: &str) -> Option<String> {
    let t = url.trim();
    if t.is_empty() || t.len() > 1024 {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return None; // remote: placeholder by policy
    }
    let path = t.strip_prefix("file://").unwrap_or(t);
    if path.contains("..") {
        return None; // directory-escape guard
    }
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    Some(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_never_loads() {
        assert_eq!(local_image_candidate("https://x.dev/a.png"), None);
        assert_eq!(local_image_candidate("http://x.dev/a.jpg"), None);
    }

    #[test]
    fn escapes_and_bad_extensions_rejected() {
        assert_eq!(local_image_candidate("../../.ssh/id_rsa"), None);
        assert_eq!(local_image_candidate("../a.png"), None);
        assert_eq!(local_image_candidate("notes.md"), None);
        assert_eq!(local_image_candidate(""), None);
    }

    #[test]
    fn local_images_accepted() {
        assert_eq!(
            local_image_candidate("assets/icon.png"),
            Some("assets/icon.png".into())
        );
        assert_eq!(
            local_image_candidate("file://assets/a.JPG"),
            Some("assets/a.JPG".into())
        );
    }
}
