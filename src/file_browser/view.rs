use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, Render, Window, div,
    px, svg,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::spinner::{Spinner, SpinnerSize};
use crate::theme::ActiveTheme;

const COLUMN_WIDTH: f32 = 208.0;
const HEADER_HEIGHT: f32 = 46.0;

#[derive(Clone, Debug, Eq, PartialEq)]
enum FileEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug)]
struct FileEntry {
    name: String,
    path: PathBuf,
    kind: FileEntryKind,
}

impl FileEntry {
    fn from_dir_entry(entry: fs::DirEntry) -> Option<Self> {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = fs::symlink_metadata(&path).ok()?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            FileEntryKind::Directory
        } else if file_type.is_file() {
            FileEntryKind::File
        } else if file_type.is_symlink() {
            FileEntryKind::Symlink
        } else {
            FileEntryKind::Other
        };

        Some(Self { name, path, kind })
    }

    fn icon(&self) -> Icon {
        match self.kind {
            FileEntryKind::Directory => Icon::Folder,
            FileEntryKind::File | FileEntryKind::Symlink | FileEntryKind::Other => Icon::File,
        }
    }

    fn kind_label(&self) -> &'static str {
        match self.kind {
            FileEntryKind::Directory => "Folder",
            FileEntryKind::File => "File",
            FileEntryKind::Symlink => "Symlink",
            FileEntryKind::Other => "Item",
        }
    }

    fn sort_key(&self) -> (u8, String) {
        let rank = match self.kind {
            FileEntryKind::Directory => 0,
            FileEntryKind::Symlink => 1,
            FileEntryKind::File => 2,
            FileEntryKind::Other => 3,
        };
        (rank, self.name.to_lowercase())
    }
}

#[derive(Clone, Debug)]
struct FilePreview {
    name: String,
    path: String,
    kind_label: String,
    size_label: String,
    modified_label: String,
    location_label: String,
    item_count_label: Option<String>,
    text_preview: Option<String>,
}

pub struct FileBrowserView {
    search_input: Entity<InputState>,
    search_query: String,
    root_path: PathBuf,
    selected_path: Option<PathBuf>,
    directory_cache: HashMap<PathBuf, Vec<FileEntry>>,
    preview: Option<FilePreview>,
    error_message: Option<String>,
}

impl FileBrowserView {
    pub fn new_with_path(window: &mut Window, cx: &mut App, requested_path: Option<PathBuf>) -> Entity<Self> {
        cx.new(|cx| {
            let search_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Search")
                    .placeholder_font_size(px(13.0))
            });

            let subscription = cx.subscribe(
                &search_input,
                |this: &mut FileBrowserView, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        let text = this.search_input.read(cx).value().to_string();
                        this.update_search_query(text, cx);
                    }
                },
            );
            // Keep subscription alive for the lifetime of the view
            std::mem::forget(subscription);

            let mut this = Self {
                search_input,
                search_query: String::new(),
                root_path: Self::default_root_path(),
                selected_path: None,
                directory_cache: HashMap::new(),
                preview: None,
                error_message: None,
            };
            this.open_requested_path(requested_path, window, cx);
            this
        })
    }

    pub fn open_requested_path(&mut self, requested_path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let default_root = Self::default_root_path();
        let mut pending_error = None;
        let requested_path = requested_path.unwrap_or_else(|| default_root.clone());
        let resolved_path = match Self::resolve_target_path(requested_path) {
            Ok(path) => path,
            Err(message) => {
                pending_error = Some(message);
                default_root
            }
        };

        self.search_query.clear();
        self.search_input.update(cx, |input, cx| input.clear(cx));
        self.open_path(resolved_path, cx);
        if pending_error.is_some() {
            self.error_message = pending_error;
            cx.notify();
        }
    }

    fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                self.root_path = path;
                self.selected_path = None;
            }
            Ok(_) => {
                self.root_path = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(Self::default_root_path);
                self.selected_path = Some(path);
            }
            Err(error) => {
                self.root_path = Self::default_root_path();
                self.selected_path = None;
                self.error_message = Some(format!("Unable to open location: {}", error));
            }
        }

        self.refresh_visible_data(cx);
    }

    fn default_root_path() -> PathBuf {
        std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn resolve_target_path(path: PathBuf) -> Result<PathBuf, String> {
        let resolved_path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path)
        };

        if resolved_path.exists() {
            match fs::canonicalize(&resolved_path) {
                Ok(path) => Ok(path),
                Err(_) => Ok(resolved_path),
            }
        } else {
            Err(format!("Path not found: {}", resolved_path.display()))
        }
    }

    fn update_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query.to_lowercase();
        cx.notify();
    }

    fn navigate_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.root_path.parent() {
            let previous_root = self.root_path.clone();
            self.root_path = parent.to_path_buf();
            self.selected_path = Some(previous_root);
            self.refresh_visible_data(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.directory_cache.clear();
        self.refresh_visible_data(cx);
    }

    fn select_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected_path = Some(path);
        self.refresh_visible_data(cx);
    }

    fn refresh_visible_data(&mut self, cx: &mut Context<Self>) {
        let mut error_message = None;
        for directory in self.visible_directory_paths() {
            if let Err(error) = self.ensure_directory_loaded(&directory) {
                error_message = Some(error);
            }
        }

        self.preview = self.build_preview();
        self.error_message = error_message;
        cx.notify();
    }

    fn ensure_directory_loaded(&mut self, directory: &Path) -> Result<(), String> {
        if self.directory_cache.contains_key(directory) {
            return Ok(());
        }

        let mut entries = Vec::new();
        let reader = fs::read_dir(directory)
            .map_err(|error| format!("Unable to read {}: {}", directory.display(), error))?;

        for entry in reader.flatten() {
            if let Some(entry) = FileEntry::from_dir_entry(entry) {
                entries.push(entry);
            }
        }

        entries.sort_by_key(FileEntry::sort_key);
        self.directory_cache
            .insert(directory.to_path_buf(), entries);
        Ok(())
    }

    fn visible_directory_paths(&self) -> Vec<PathBuf> {
        let mut directories = vec![self.root_path.clone()];
        let Some(selected_path) = self.selected_path.as_ref() else {
            return directories;
        };

        if !selected_path.starts_with(&self.root_path) {
            return directories;
        }

        let mut current = self.root_path.clone();
        let selected_directory = if selected_path.is_dir() {
            selected_path.clone()
        } else {
            selected_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root_path.clone())
        };

        if selected_directory == self.root_path {
            return directories;
        }

        let Ok(relative_path) = selected_directory.strip_prefix(&self.root_path) else {
            return directories;
        };

        for component in relative_path.components() {
            current.push(component.as_os_str());
            directories.push(current.clone());
        }

        directories
    }

    fn build_preview(&self) -> Option<FilePreview> {
        let preview_path = self
            .selected_path
            .as_ref()
            .unwrap_or(&self.root_path)
            .to_path_buf();
        let metadata = fs::symlink_metadata(&preview_path).ok()?;
        let is_directory = metadata.is_dir();
        let name = Self::display_name(&preview_path);
        let kind_label = if is_directory { "Folder" } else { "File" }.to_string();
        let size_label = if is_directory {
            "Directory".to_string()
        } else {
            Self::format_size(metadata.len())
        };
        let modified_label = metadata
            .modified()
            .ok()
            .map(Self::format_datetime)
            .unwrap_or_else(|| "Unknown".to_string());
        let location_label = preview_path
            .parent()
            .map(|parent| Self::display_path(parent))
            .unwrap_or_else(|| Self::display_path(&preview_path));
        let item_count_label = if is_directory {
            Some(format!("{} items", self.entry_count_for(&preview_path)))
        } else {
            None
        };
        let text_preview = if is_directory {
            None
        } else {
            Self::read_text_preview(&preview_path)
        };

        Some(FilePreview {
            name,
            path: Self::display_path(&preview_path),
            kind_label,
            size_label,
            modified_label,
            location_label,
            item_count_label,
            text_preview,
        })
    }

    fn read_text_preview(path: &Path) -> Option<String> {
        const PREVIEW_BYTES: usize = 64 * 1024;

        let file = fs::File::open(path).ok()?;
        let mut reader = std::io::BufReader::new(file).take(PREVIEW_BYTES as u64);
        let mut bytes = Vec::new();
        if reader.read_to_end(&mut bytes).ok()? == 0 {
            return Some("Empty file".to_string());
        }

        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.is_empty() {
            return None;
        }

        Some(text)
    }

    fn entry_count_for(&self, path: &Path) -> usize {
        self.directory_cache
            .get(path)
            .map(Vec::len)
            .or_else(|| fs::read_dir(path).ok().map(|reader| reader.count()))
            .unwrap_or(0)
    }

    fn display_name(path: &Path) -> String {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| path.display().to_string())
    }

    fn display_path(path: &Path) -> String {
        if let Some(home) = dirs::home_dir()
            && let Ok(relative_path) = path.strip_prefix(&home)
        {
            if relative_path.as_os_str().is_empty() {
                return "~".to_string();
            }

            return format!("~/{}", relative_path.display());
        }

        path.display().to_string()
    }

    fn format_datetime(timestamp: std::time::SystemTime) -> String {
        DateTime::<Local>::from(timestamp)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    }

    fn format_size(size: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

        let mut value = size as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }

        if unit == 0 {
            format!("{} {}", size, UNITS[unit])
        } else {
            format!("{:.1} {}", value, UNITS[unit])
        }
    }

    fn matches_search(&self, entry: &FileEntry) -> bool {
        self.search_query.is_empty() || entry.name.to_lowercase().contains(&self.search_query)
    }

    fn selected_child_for_directory(&self, directory: &Path) -> Option<PathBuf> {
        let selected_path = self.selected_path.as_ref()?;
        if selected_path == directory {
            return None;
        }

        let relative_path = selected_path.strip_prefix(directory).ok()?;
        let next_component = relative_path.components().next()?;
        let mut child_path = directory.to_path_buf();
        child_path.push(next_component.as_os_str());
        Some(child_path)
    }

    fn toolbar_directory_name(&self) -> String {
        if let Some(selected_path) = self.selected_path.as_ref() {
            let target = if selected_path.is_dir() {
                selected_path.as_path()
            } else {
                selected_path.parent().unwrap_or(&self.root_path)
            };

            return Self::display_name(target);
        }

        Self::display_name(&self.root_path)
    }

    fn should_show_preview(&self) -> bool {
        self.selected_path
            .as_ref()
            .and_then(|path| fs::symlink_metadata(path).ok())
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    }

    fn render_chrome_button(
        &self,
        id: &'static str,
        label: &'static str,
        active: bool,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;

        div()
            .id(id)
            .w(px(28.0))
            .h(px(24.0))
            .bg(if active {
                theme.tab_hover_background
            } else {
                theme.panel_background
            })
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(label),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    on_click(this, cx);
                }),
            )
    }

    fn render_toolbar_segment_button(
        &self,
        content: impl IntoElement,
        active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .w(px(30.0))
            .h(px(24.0))
            .bg(if active {
                theme.tab_hover_background
            } else {
                theme.panel_background
            })
            .flex()
            .items_center()
            .justify_center()
            .child(content)
    }

    fn render_toolbar_directory_label(
        &self,
        is_loading: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .min_w(px(120.0))
            .max_w(px(220.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .min_w_0()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(self.toolbar_directory_name()),
            )
            .when(is_loading, |this| {
                this.child(Spinner::new().size(SpinnerSize::Small).color(theme.accent))
            })
    }

    fn render_icon_grid(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .flex()
                    .gap(px(2.0))
                    .child(
                        div()
                            .w(px(4.0))
                            .h(px(4.0))
                            .rounded(px(1.0))
                            .bg(theme.text_muted),
                    )
                    .child(
                        div()
                            .w(px(4.0))
                            .h(px(4.0))
                            .rounded(px(1.0))
                            .bg(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(2.0))
                    .child(
                        div()
                            .w(px(4.0))
                            .h(px(4.0))
                            .rounded(px(1.0))
                            .bg(theme.text_muted),
                    )
                    .child(
                        div()
                            .w(px(4.0))
                            .h(px(4.0))
                            .rounded(px(1.0))
                            .bg(theme.text_muted),
                    ),
            )
    }

    fn render_icon_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .w(px(10.0))
                    .h(px(2.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
            .child(
                div()
                    .w(px(10.0))
                    .h(px(2.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
            .child(
                div()
                    .w(px(10.0))
                    .h(px(2.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
    }

    fn render_icon_columns(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .flex()
            .gap(px(2.0))
            .child(
                div()
                    .w(px(3.0))
                    .h(px(10.0))
                    .rounded(px(999.0))
                    .bg(theme.text),
            )
            .child(
                div()
                    .w(px(3.0))
                    .h(px(10.0))
                    .rounded(px(999.0))
                    .bg(theme.text),
            )
            .child(
                div()
                    .w(px(3.0))
                    .h(px(10.0))
                    .rounded(px(999.0))
                    .bg(theme.text),
            )
    }

    fn render_icon_gallery(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .w(px(11.0))
            .h(px(8.0))
            .rounded(px(2.0))
            .border_1()
            .border_color(theme.text_muted)
            .child(
                div()
                    .w_full()
                    .h(px(2.0))
                    .mt(px(4.0))
                    .bg(theme.text_muted.opacity(0.8)),
            )
    }

    fn render_icon_share(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(1.0))
            .child(
                div()
                    .w(px(2.0))
                    .h(px(6.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
            .child(
                div()
                    .w(px(8.0))
                    .h(px(2.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
    }

    fn render_icon_tag(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .w(px(10.0))
            .h(px(7.0))
            .rounded(px(2.0))
            .border_1()
            .border_color(theme.text_muted)
    }

    fn render_icon_more(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .w(px(2.0))
                    .h(px(2.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
            .child(
                div()
                    .w(px(2.0))
                    .h(px(2.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
            .child(
                div()
                    .w(px(2.0))
                    .h(px(2.0))
                    .rounded(px(999.0))
                    .bg(theme.text_muted),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .w_full()
            .h(px(HEADER_HEIGHT))
            .px(px(12.0))
            .bg(theme.app_background)
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .h(px(26.0))
                            .rounded(px(8.0))
                            .bg(theme.app_background)
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .child(self.render_chrome_button(
                                "file-browser-up",
                                "<",
                                false,
                                |this, cx| this.navigate_up(cx),
                                cx,
                            ))
                            .child(div().w(px(1.0)).h_full().bg(theme.border))
                            .child(self.render_chrome_button(
                                "file-browser-refresh",
                                ">",
                                false,
                                |this, cx| this.refresh(cx),
                                cx,
                            )),
                    )
                    .child(self.render_toolbar_directory_label(false, cx)),
            )
            .child(
                div().flex().items_center().gap(px(6.0)).child(
                    div()
                        .h(px(26.0))
                        .rounded(px(8.0))
                        .bg(theme.app_background)
                        .border_1()
                        .border_color(theme.border)
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .child(self.render_toolbar_segment_button(
                            self.render_icon_grid(cx),
                            false,
                            cx,
                        ))
                        .child(div().w(px(1.0)).h_full().bg(theme.border))
                        .child(self.render_toolbar_segment_button(
                            self.render_icon_list(cx),
                            false,
                            cx,
                        ))
                        .child(div().w(px(1.0)).h_full().bg(theme.border))
                        .child(self.render_toolbar_segment_button(
                            self.render_icon_columns(cx),
                            true,
                            cx,
                        ))
                        .child(div().w(px(1.0)).h_full().bg(theme.border))
                        .child(self.render_toolbar_segment_button(
                            self.render_icon_gallery(cx),
                            false,
                            cx,
                        )),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div().flex().items_center().gap(px(6.0)).child(
                            div()
                                .h(px(26.0))
                                .rounded(px(8.0))
                                .bg(theme.app_background)
                                .border_1()
                                .border_color(theme.border)
                                .flex()
                                .items_center()
                                .overflow_hidden()
                                .child(self.render_toolbar_segment_button(
                                    self.render_icon_share(cx),
                                    false,
                                    cx,
                                ))
                                .child(div().w(px(1.0)).h_full().bg(theme.border))
                                .child(self.render_toolbar_segment_button(
                                    self.render_icon_tag(cx),
                                    false,
                                    cx,
                                ))
                                .child(div().w(px(1.0)).h_full().bg(theme.border))
                                .child(self.render_toolbar_segment_button(
                                    self.render_icon_more(cx),
                                    false,
                                    cx,
                                )),
                        ),
                    )
                    .child(
                        div()
                            .w(px(160.0))
                            .h(px(26.0))
                            .px(px(9.0))
                            .rounded(px(999.0))
                            .bg(theme.app_background)
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                svg()
                                    .size(px(11.0))
                                    .text_color(theme.icon_muted)
                                    .path(Icon::MagnifyingGlass.path()),
                            )
                            .child(EditorInput::new(&self.search_input).transparent()),
                    ),
            )
    }

    fn render_directory_column(&self, directory: &Path, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;
        let selected_child = self.selected_child_for_directory(directory);
        let is_directory_loaded = self.directory_cache.contains_key(directory);
        let entries = self
            .directory_cache
            .get(directory)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| self.matches_search(entry))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut rows = div()
            .id(format!("file-browser-column-{}", directory.display()))
            .w_full()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .overflow_y_scroll();

        if is_directory_loaded && entries.is_empty() {
            rows = rows.child(
                div()
                    .w_full()
                    .px(px(12.0))
                    .pt(px(18.0))
                    .text_xs()
                    .text_color(theme.text_subtle)
                    .child(if self.search_query.is_empty() {
                        "This folder is empty.".to_string()
                    } else {
                        "No matching items.".to_string()
                    }),
            );
        } else {
            for entry in entries {
                let path = entry.path.clone();
                let name = entry.name.clone();
                let kind_label = entry.kind_label().to_string();
                let is_selected = selected_child.as_ref() == Some(&entry.path);
                let is_deepest_selected_folder = entry.kind == FileEntryKind::Directory
                    && self.selected_path.as_ref() == Some(&entry.path);
                let row_background = if is_deepest_selected_folder {
                    theme.selection.opacity(0.96)
                } else if is_selected {
                    theme.selection.opacity(0.64)
                } else {
                    theme.elevated_background.opacity(0.0)
                };
                let row_text = if is_selected {
                    theme.accent_foreground
                } else {
                    theme.text
                };
                let row_meta = if is_selected {
                    theme
                        .accent_foreground
                        .opacity(if is_deepest_selected_folder {
                            0.88
                        } else {
                            0.72
                        })
                } else {
                    theme.text_subtle
                };

                rows = rows.child(
                    div()
                        .id(format!("file-entry-{}", path.display()))
                        .w_full()
                        .h(px(22.0))
                        .px(px(8.0))
                        .rounded(px(5.0))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .bg(row_background)
                        .when(!is_selected, |this| {
                            this.hover(move |style| style.bg(theme.tab_hover_background))
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.select_path(path.clone(), cx);
                            }),
                        )
                        .child(
                            svg()
                                .size(px(12.0))
                                .text_color(match entry.kind {
                                    FileEntryKind::Directory if is_selected => {
                                        theme.accent_foreground
                                    }
                                    FileEntryKind::Directory => theme.accent,
                                    _ if is_selected => theme.accent_foreground,
                                    _ => theme.icon_muted,
                                })
                                .path(entry.icon().path()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(6.0))
                                .child(div().text_xs().text_color(row_text).child(name))
                                .child(div().text_xs().text_color(row_meta).child(
                                    if matches!(entry.kind, FileEntryKind::Directory) {
                                        "›".to_string()
                                    } else {
                                        kind_label
                                    },
                                )),
                        ),
                );
            }
        }

        div()
            .w(px(COLUMN_WIDTH))
            .h_full()
            .bg(theme.app_background)
            .flex()
            .flex_col()
            .child(rows)
    }

    fn render_columns(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;
        let directories = {
            let visible = self.visible_directory_paths();
            let start = visible.len().saturating_sub(4);
            visible[start..].to_vec()
        };

        let mut columns = div().h_full().flex().min_w_full().bg(theme.app_background);
        for (index, directory) in directories.iter().enumerate() {
            if index > 0 {
                columns = columns.child(div().h_full().w(px(1.0)).bg(theme.border));
            }

            columns = columns.child(self.render_directory_column(directory, cx));
        }

        div()
            .flex_1()
            .h_full()
            .bg(theme.app_background)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(columns)
    }

    fn render_preview_row(
        &self,
        label: &str,
        value: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(theme.border.opacity(0.65))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_subtle)
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.text)
                    .line_height(px(20.0))
                    .child(value.into()),
            )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;
        let preview = self.preview.clone();

        let mut content = div()
            .w_full()
            .flex_1()
            .min_h_0()
            .px(px(16.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(14.0));

        if let Some(preview) = preview {
            if let Some(text_preview) = preview.text_preview.clone() {
                content = content.child(
                    div()
                        .id("file-browser-text-preview")
                        .w_full()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p(px(12.0))
                        .rounded(px(8.0))
                        .bg(theme.panel_background)
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .w_full()
                                .text_xs()
                                .line_height(px(18.0))
                                .text_color(theme.text)
                                .child(text_preview),
                        ),
                );
            } else {
                content = content.child(
                    div()
                        .w(px(52.0))
                        .h(px(52.0))
                        .rounded(px(14.0))
                        .bg(theme.panel_background)
                        .border_1()
                        .border_color(theme.border)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(svg().size(px(22.0)).text_color(theme.accent).path(
                            if preview.kind_label == "Folder" {
                                Icon::Folder.path()
                            } else {
                                Icon::File.path()
                            },
                        )),
                );
            }

            content = content
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(preview.name),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child(format!("{} - {}", preview.kind_label, preview.size_label)),
                        ),
                )
                .child(self.render_preview_row("Path", preview.path, cx))
                .child(self.render_preview_row("Location", preview.location_label, cx))
                .child(self.render_preview_row("Modified", preview.modified_label, cx));

            if let Some(item_count_label) = preview.item_count_label {
                content = content.child(self.render_preview_row("Contents", item_count_label, cx));
            }
        } else {
            content = content.child(
                div()
                    .text_xs()
                    .text_color(theme.text_subtle)
                    .child("Select a file or folder to inspect it here."),
            );
        }

        div()
            .flex_1()
            .min_w(px(280.0))
            .h_full()
            .bg(theme.app_background)
            .border_l_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .child(content)
    }

    fn render_error_banner(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let theme = cx.theme().ui;
        self.error_message.as_ref().map(|message| {
            div()
                .w_full()
                .px(px(12.0))
                .py(px(10.0))
                .rounded(px(12.0))
                .bg(theme.warning_background)
                .border_1()
                .border_color(theme.warning.opacity(0.35))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.text)
                        .child(message.clone()),
                )
        })
    }
}

impl Focusable for FileBrowserView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.read(cx).focus_handle(cx)
    }
}

impl Render for FileBrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        div()
            .id("file-browser-view")
            .size_full()
            .bg(theme.app_background)
            .flex()
            .flex_col()
            .when_some(self.render_error_banner(cx), |this, banner| {
                this.child(banner)
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .rounded(px(12.0))
                    .bg(theme.app_background)
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(px(0.0))
                    .child(self.render_toolbar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .gap(px(0.0))
                            .child(self.render_columns(cx))
                            .when(self.should_show_preview(), |this| {
                                this.child(self.render_preview(cx))
                            }),
                    ),
            )
    }
}
