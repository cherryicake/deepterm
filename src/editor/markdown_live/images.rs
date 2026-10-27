//! Local image *paste/drop* helpers for live markdown.
//!
//! Loading a `![](...)` reference is [`crate::editor::md_image`]'s job (local
//! file + remote URL resolution, background loading and caching). This module
//! only covers the write path: clipboard image bytes / dropped files are saved
//! into a Typora-style `<stem>.assets/` directory next to the document.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
