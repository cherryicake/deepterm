//! Local image resolution helpers for live markdown.
//!
//! Mirrors the guards in `markdown_element::load_local_image`: remote URLs
//! never load, `..` escapes and non-image extensions are rejected, and files
//! over 20MB are refused. Loading itself stays in the view (background
//! executor + `Arc<Image>` cache); this module only decides *what* may load.

/// Max image file size: 20MB (same as the preview + editor guards).
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

use std::path::{Path, PathBuf};

/// Extension for a clipboard image format (pastes are saved as files).
pub fn format_extension(format: gpui::ImageFormat) -> &'static str {
    match format {
        gpui::ImageFormat::Png => "png",
        gpui::ImageFormat::Jpeg => "jpg",
        gpui::ImageFormat::Webp => "webp",
        gpui::ImageFormat::Gif => "gif",
        gpui::ImageFormat::Svg => "svg",
        gpui::ImageFormat::Bmp => "bmp",
        gpui::ImageFormat::Tiff => "tiff",
        gpui::ImageFormat::Ico => "ico",
        gpui::ImageFormat::Pnm => "ppm",
    }
}

/// True when `path` names a locally loadable image (render allowlist).
pub fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Typora-style assets dir for a document: `<doc_dir>/<stem>.assets`.
pub fn assets_dir(doc: &Path) -> PathBuf {
    let stem = doc
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    doc.parent()
        .unwrap_or(Path::new("."))
        .join(format!("{stem}.assets"))
}

/// Non-colliding file name inside `dir`: `prefix.ext`, `prefix-1.ext`, …
pub fn unique_file_name(dir: &Path, prefix: &str, ext: &str) -> String {
    let mut name = format!("{prefix}.{ext}");
    let mut i = 1;
    while dir.join(&name).exists() {
        name = format!("{prefix}-{i}.{ext}");
        i += 1;
    }
    name
}

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
    fn format_extensions_cover_clipboard() {
        use gpui::ImageFormat::*;
        assert_eq!(format_extension(Png), "png");
        assert_eq!(format_extension(Jpeg), "jpg");
        assert_eq!(format_extension(Svg), "svg");
        assert_eq!(format_extension(Pnm), "ppm");
    }

    #[test]
    fn assets_dir_follows_typora_layout() {
        assert_eq!(
            assets_dir(Path::new("/docs/notes.md")),
            PathBuf::from("/docs/notes.assets")
        );
        assert_eq!(
            assets_dir(Path::new("/docs/my.notes.md")),
            PathBuf::from("/docs/my.notes.assets")
        );
    }

    #[test]
    fn unique_names_bump_on_collision() {
        let dir = std::env::temp_dir().join("deepterm-md-img-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("image.png"), b"x").unwrap();
        assert_eq!(unique_file_name(&dir, "image", "png"), "image-1.png");
        std::fs::remove_file(dir.join("image.png")).unwrap();
        assert_eq!(unique_file_name(&dir, "image", "png"), "image.png");
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
