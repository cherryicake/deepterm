use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    MouseButton, Render, Stateful, Window, div, px, svg, uniform_list,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::spinner::{Spinner, SpinnerSize};
use crate::components::tab_bar::DragPreview;
use crate::file_browser::FileBrowserEvent;
use crate::file_browser::shared::{
    COLUMN_MAX_WIDTH, COLUMN_MIN_WIDTH, FileBrowserLayoutState, FileBrowserViewMode,
    FileBrowserWidthTracker, TracksFileBrowserWidth, format_datetime, format_size,
    max_chars_for_column_width, render_column_resize_handle, render_columns_container,
    render_icon_columns, render_icon_gallery, render_icon_grid, render_icon_list, render_icon_more,
    render_icon_share, render_icon_tag, truncate_middle,
};
use crate::file_transfer::{FileDragPayload, FileEndpoint, TransferRequest};
use crate::theme::ActiveTheme;

const COLUMN_DEFAULT_WIDTH: f32 = 240.0;
const HEADER_HEIGHT: f32 = 46.0;
const LIST_ROW_HEIGHT: f32 = 24.0;

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
    name_lower: String,
    path: PathBuf,
    kind: FileEntryKind,
    size: Option<u64>,
    modified_at: Option<std::time::SystemTime>,
}

impl FileEntry {
    fn from_dir_entry(entry: fs::DirEntry) -> Option<Self> {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // 使用 DirEntry::file_type() 而非 fs::symlink_metadata()。
        // 前者在 Linux/macOS 上利用 readdir 返回的 d_type，无需额外的 stat 系统调用。
        let file_type = entry.file_type().ok()?;
        let kind = if file_type.is_dir() {
            FileEntryKind::Directory
        } else if file_type.is_file() {
            FileEntryKind::File
        } else if file_type.is_symlink() {
            FileEntryKind::Symlink
        } else {
            FileEntryKind::Other
        };

        Some(Self {
            name: name.clone(),
            name_lower: name.to_lowercase(),
            path,
            kind,
            size: None,
            modified_at: None,
        })
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
        (rank, self.name_lower.clone())
    }

    fn size_label(&self) -> String {
        match self.kind {
            FileEntryKind::Directory => "--".to_string(),
            _ => self
                .size
                .map(format_size)
                .unwrap_or_else(|| "--".to_string()),
        }
    }

    fn modified_label(&self) -> String {
        self.modified_at
            .map(format_datetime)
            .unwrap_or_else(|| "--".to_string())
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
    focus_handle: FocusHandle,
    search_input: Entity<InputState>,
    search_query: String,
    root_path: PathBuf,
    selected_path: Option<PathBuf>,
    directory_cache: HashMap<PathBuf, Vec<FileEntry>>,
    preview: Option<FilePreview>,
    error_message: Option<String>,
    view_mode: FileBrowserViewMode,
    expanded_folders: HashSet<PathBuf>,
    hovered_path: Option<PathBuf>,
    layout: FileBrowserLayoutState,
    /// Lazy cache for the flattened tree used by List view virtual scrolling.
    list_flat_cache: Option<Vec<(FileEntry, usize)>>,
}

impl FileBrowserView {
    pub fn new_with_path(
        window: &mut Window,
        cx: &mut App,
        requested_path: Option<PathBuf>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let search_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Search")
                    .placeholder_font_size(px(15.0))
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
                focus_handle: cx.focus_handle(),
                search_input,
                search_query: String::new(),
                root_path: Self::default_root_path(),
                selected_path: None,
                directory_cache: HashMap::new(),
                preview: None,
                error_message: None,
                view_mode: FileBrowserViewMode::Columns,
                expanded_folders: HashSet::new(),
                hovered_path: None,
                layout: FileBrowserLayoutState::new(px(COLUMN_DEFAULT_WIDTH)),
                list_flat_cache: None,
            };
            this.open_requested_path(requested_path, window, cx);
            this
        })
    }

    pub fn open_requested_path(
        &mut self,
        requested_path: Option<PathBuf>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    fn set_view_mode(&mut self, mode: FileBrowserViewMode, cx: &mut Context<Self>) {
        self.view_mode = mode;
        self.layout.columns_mut().clear_resize();
        self.hovered_path = None;
        cx.notify();
    }

    fn set_hovered_path(&mut self, path: PathBuf, hovered: bool, cx: &mut Context<Self>) {
        let next = if hovered {
            Some(path)
        } else if self.hovered_path.as_ref() == Some(&path) {
            None
        } else {
            return;
        };

        if self.hovered_path != next {
            self.hovered_path = next;
            cx.notify();
        }
    }

    fn column_width_at(&self, index: usize) -> gpui::Pixels {
        self.layout.columns().width_at(index)
    }

    fn handle_columns_mouse_move(&mut self, mouse_x: gpui::Pixels, cx: &mut Context<Self>) {
        if self.layout.columns_mut().resize_to(mouse_x) {
            cx.notify();
        }
    }

    fn handle_columns_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.layout.columns_mut().finish_resize() {
            cx.notify();
        }
    }

    fn invalidate_list_cache(&mut self) {
        self.list_flat_cache = None;
    }

    fn toggle_folder_expanded(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.expanded_folders.contains(&path) {
            self.expanded_folders.remove(&path);
        } else {
            self.expanded_folders.insert(path.clone());
            let _ = self.ensure_directory_loaded(&path);
        }
        self.invalidate_list_cache();
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

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.directory_cache.clear();
        self.invalidate_list_cache();
        self.refresh_visible_data(cx);
    }

    fn select_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected_path = Some(path);
        self.refresh_visible_data(cx);
    }

    fn drag_payload_for_path(&self, path: PathBuf) -> FileDragPayload {
        FileDragPayload::move_one(FileEndpoint::Local { path })
    }

    fn destination_for_path(&self, path: PathBuf, is_directory: bool) -> FileEndpoint {
        let destination_path = if is_directory {
            path
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root_path.clone())
        };

        FileEndpoint::Local {
            path: destination_path,
        }
    }

    fn request_transfer_to_path(
        &mut self,
        payload: FileDragPayload,
        path: PathBuf,
        is_directory: bool,
        cx: &mut Context<Self>,
    ) {
        let destination = self.destination_for_path(path, is_directory);
        cx.emit(FileBrowserEvent::TransferRequested(
            TransferRequest::from_drag_payload(payload, destination),
        ));
    }

    fn request_transfer_to_root(&mut self, payload: FileDragPayload, cx: &mut Context<Self>) {
        cx.emit(FileBrowserEvent::TransferRequested(
            TransferRequest::from_drag_payload(
                payload,
                FileEndpoint::Local {
                    path: self.root_path.clone(),
                },
            ),
        ));
    }

    fn refresh_visible_data(&mut self, cx: &mut Context<Self>) {
        let mut error_message = None;
        let mut newly_loaded = Vec::new();
        for directory in self.visible_directory_paths() {
            if !self.directory_cache.contains_key(&directory) {
                if let Err(error) = self.ensure_directory_loaded(&directory) {
                    error_message = Some(error);
                } else {
                    newly_loaded.push(directory);
                }
            }
        }

        for directory in newly_loaded {
            self.start_metadata_load(directory, cx);
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
        self.invalidate_list_cache();
        Ok(())
    }

    fn start_metadata_load(&self, directory: PathBuf, cx: &mut Context<Self>) {
        let Some(entries) = self.directory_cache.get(&directory).cloned() else {
            return;
        };

        let weak = cx.weak_entity();
        let task = cx.background_executor().spawn(async move {
            let mut updated = entries;
            for entry in &mut updated {
                if entry.size.is_some() && entry.modified_at.is_some() {
                    continue;
                }
                if let Ok(metadata) = fs::symlink_metadata(&entry.path) {
                    entry.size = Some(metadata.len());
                    entry.modified_at = metadata.modified().ok();
                }
            }
            updated
        });

        cx.spawn(async move |_, cx| {
            let updated = task.await;
            if let Some(entity) = weak.upgrade() {
                let _ = entity.update(cx, |this, cx| {
                    this.directory_cache.insert(directory, updated);
                    cx.notify();
                });
            }
        })
        .detach();
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
            format_size(metadata.len())
        };
        let modified_label = metadata
            .modified()
            .ok()
            .map(format_datetime)
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
        use crate::file_browser::shared::is_binary_content;
        const PREVIEW_BYTES: usize = 64 * 1024;

        let file = fs::File::open(path).ok()?;
        let mut reader = std::io::BufReader::new(file).take(PREVIEW_BYTES as u64);
        let mut bytes = Vec::new();
        if reader.read_to_end(&mut bytes).ok()? == 0 {
            return Some("Empty file".to_string());
        }

        if is_binary_content(&bytes) {
            return None;
        }

        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.is_empty() {
            return None;
        }

        Some(text)
    }

    fn entry_count_for(&self, path: &Path) -> usize {
        self.directory_cache.get(path).map(Vec::len).unwrap_or(0)
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

    fn matches_search(&self, entry: &FileEntry) -> bool {
        self.search_query.is_empty() || entry.name_lower.contains(&self.search_query)
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
                    .text_sm()
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
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(self.toolbar_directory_name()),
            )
            .when(is_loading, |this| {
                this.child(Spinner::new().size(SpinnerSize::Small).color(theme.accent))
            })
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;

        div()
            .w_full()
            .h(px(HEADER_HEIGHT))
            .px(px(12.0))
            .bg(theme.app_background)
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
                            render_icon_grid(&theme),
                            false,
                            cx,
                        ))
                        .child(div().w(px(1.0)).h_full().bg(theme.border))
                        .child(
                            self.render_toolbar_segment_button(
                                render_icon_list(&theme),
                                self.view_mode == FileBrowserViewMode::List,
                                cx,
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.set_view_mode(FileBrowserViewMode::List, cx);
                                }),
                            ),
                        )
                        .child(div().w(px(1.0)).h_full().bg(theme.border))
                        .child(
                            self.render_toolbar_segment_button(
                                render_icon_columns(&theme),
                                self.view_mode == FileBrowserViewMode::Columns,
                                cx,
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.set_view_mode(FileBrowserViewMode::Columns, cx);
                                }),
                            ),
                        )
                        .child(div().w(px(1.0)).h_full().bg(theme.border))
                        .child(self.render_toolbar_segment_button(
                            render_icon_gallery(&theme),
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
                                    render_icon_share(&theme),
                                    false,
                                    cx,
                                ))
                                .child(div().w(px(1.0)).h_full().bg(theme.border))
                                .child(self.render_toolbar_segment_button(
                                    render_icon_tag(&theme),
                                    false,
                                    cx,
                                ))
                                .child(div().w(px(1.0)).h_full().bg(theme.border))
                                .child(self.render_toolbar_segment_button(
                                    render_icon_more(&theme),
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

    fn render_column_row(
        &mut self,
        entry: &FileEntry,
        selected_child: Option<&PathBuf>,
        name_max_chars: usize,
        theme: crate::theme::UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let path = entry.path.clone();
        let name = entry.name.clone();
        let is_directory = entry.kind == FileEntryKind::Directory;
        let is_selected = selected_child == Some(&entry.path);
        let is_deepest_selected = self.selected_path.as_ref() == Some(&entry.path);
        let is_active = is_selected || is_deepest_selected;
        let is_hovered = self.hovered_path.as_ref() == Some(&entry.path);
        let row_background = if is_active {
            theme.selection
        } else if is_hovered {
            theme.tab_hover_background
        } else {
            gpui::transparent_black()
        };
        let row_text = if is_active {
            theme.accent_foreground
        } else {
            theme.text
        };
        let row_meta = if is_active {
            theme.accent_foreground
        } else {
            theme.text_subtle
        };

        let hover_path = path.clone();
        let click_path = path.clone();
        let drag_payload = self.drag_payload_for_path(path.clone());
        let drag_label = drag_payload.preview_label();
        let drop_path = path.clone();
        let mut row = div()
            .id(format!("file-entry-{}", path.display()))
            .w_full()
            .h(px(24.0))
            .px(px(6.0))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                this.set_hovered_path(hover_path.clone(), *hovered, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.select_path(click_path.clone(), cx);
                }),
            )
            .on_drag(drag_payload, move |_, position, window, cx| {
                cx.stop_propagation();
                cx.new(|cx| DragPreview::new(drag_label.clone().into(), position, window, cx))
            })
            .on_drop(cx.listener(move |this, payload: &FileDragPayload, _, cx| {
                this.request_transfer_to_path(payload.clone(), drop_path.clone(), is_directory, cx);
            }));

        if is_directory {
            row = row
                .drag_over::<FileDragPayload>(move |style, _payload, _window, _cx| {
                    style
                        .border_1()
                        .border_color(theme.drop_target_border.opacity(0.5))
                })
                .rounded(px(6.0));
        }

        row.child(
            div()
                .w_full()
                .h_full()
                .px(px(4.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .bg(row_background)
                .child(
                    svg()
                        .size(px(14.0))
                        .text_color(match entry.kind {
                            FileEntryKind::Directory => theme.accent,
                            _ if is_active => theme.accent_foreground,
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
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .truncate()
                                .text_sm()
                                .text_color(row_text)
                                .child(truncate_middle(&name, name_max_chars)),
                        )
                        .child(div().text_sm().text_color(row_meta).child(
                            if matches!(entry.kind, FileEntryKind::Directory) {
                                "›".to_string()
                            } else {
                                String::new()
                            },
                        )),
                ),
        )
    }

    fn render_directory_column(
        &mut self,
        directory: &Path,
        column_index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().ui.clone();
        let column_width: f32 = self.column_width_at(column_index).into();
        let name_max_chars = max_chars_for_column_width(column_width, 13.0);
        let is_directory_loaded = self.directory_cache.contains_key(directory);

        let filtered_count = self
            .directory_cache
            .get(directory)
            .map(|entries| entries.iter().filter(|e| self.matches_search(e)).count())
            .unwrap_or(0);

        let directory_buf = directory.to_path_buf();

        let rows_element: gpui::AnyElement = if is_directory_loaded && filtered_count == 0 {
            div()
                .id(format!("file-browser-column-{}", directory.display()))
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .w_full()
                        .px(px(12.0))
                        .pt(px(18.0))
                        .text_sm()
                        .text_color(theme.text_subtle)
                        .child(if self.search_query.is_empty() {
                            "This folder is empty.".to_string()
                        } else {
                            "No matching items.".to_string()
                        }),
                )
                .into_any_element()
        } else {
            uniform_list(
                format!("file-browser-column-{}", directory.display()),
                filtered_count,
                cx.processor({
                    let directory_buf = directory_buf.clone();
                    let name_max_chars = name_max_chars;
                    let theme = theme.clone();
                    move |this, range: std::ops::Range<usize>, _window, cx| {
                        let entries: Vec<FileEntry> = this
                            .directory_cache
                            .get(&directory_buf)
                            .map(|entries| {
                                entries
                                    .iter()
                                    .filter(|e| this.matches_search(e))
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();

                        let selected_child = this.selected_child_for_directory(&directory_buf);

                        range
                            .filter_map(|ix| {
                                if ix >= entries.len() {
                                    return None;
                                }
                                let entry = &entries[ix];
                                Some(this.render_column_row(
                                    entry,
                                    selected_child.as_ref(),
                                    name_max_chars,
                                    theme.clone(),
                                    cx,
                                ))
                            })
                            .collect()
                    }
                }),
            )
            .w_full()
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        let column_dir = directory_buf.clone();
        div()
            .w(self.column_width_at(column_index))
            .h_full()
            .bg(theme.app_background)
            .flex()
            .flex_col()
            .child(rows_element)
            .on_drop(cx.listener(move |this, payload: &FileDragPayload, _, cx| {
                this.request_transfer_to_path(payload.clone(), column_dir.clone(), true, cx);
            }))
    }

    fn render_resize_handle(
        &self,
        column_index: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        render_column_resize_handle(
            column_index,
            self.layout.columns().is_resizing_column(column_index),
            "file-browser-col-handle",
            "file-browser-col-handle-",
            cx,
            move |this, ev, _, cx| {
                this.layout
                    .columns_mut()
                    .start_resize(column_index, ev.position.x);
                cx.stop_propagation();
                cx.notify();
            },
        )
    }

    fn render_columns(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let theme = cx.theme().ui;
        let directories = self.visible_directory_paths();
        let show_preview = self.layout.should_show_preview(self.should_show_preview());

        let mut columns = div().h_full().flex().bg(theme.app_background);
        for (index, directory) in directories.iter().enumerate() {
            if index > 0 {
                columns = columns.child(self.render_resize_handle(index - 1, cx));
            }

            columns = columns.child(self.render_directory_column(directory, index, cx));
        }

        if show_preview {
            let preview_index = directories.len();
            if preview_index > 0 {
                columns = columns.child(self.render_resize_handle(preview_index - 1, cx));
            }
            columns = columns.child(self.render_preview(cx));
        } else if !directories.is_empty() {
            columns = columns.child(self.render_resize_handle(directories.len() - 1, cx));
        }

        render_columns_container("file-browser-columns-container", &theme, columns)
    }

    fn build_flat_list(
        &self,
        directory: &Path,
        depth: usize,
        result: &mut Vec<(FileEntry, usize)>,
    ) {
        const MAX_TREE_DEPTH: usize = 20;
        if depth > MAX_TREE_DEPTH {
            return;
        }

        if let Some(entries) = self.directory_cache.get(directory) {
            for entry in entries.iter().filter(|e| self.matches_search(e)) {
                let is_directory = entry.kind == FileEntryKind::Directory;
                let is_expanded = is_directory && self.expanded_folders.contains(&entry.path);
                result.push((entry.clone(), depth));
                if is_expanded {
                    self.build_flat_list(&entry.path, depth + 1, result);
                }
            }
        }
    }

    fn render_list_row(
        &mut self,
        entry: &FileEntry,
        depth: usize,
        tree_name_max_chars: usize,
        theme: crate::theme::UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let path = entry.path.clone();
        let is_selected = self.selected_path.as_ref() == Some(&path);
        let is_directory = entry.kind == FileEntryKind::Directory;
        let is_expanded = is_directory && self.expanded_folders.contains(&path);
        let name = entry.name.clone();
        let modified_label = entry.modified_label();
        let size_label = entry.size_label();
        let kind_label = entry.kind_label().to_string();
        let icon = entry.icon();
        let is_hovered = self.hovered_path.as_ref() == Some(&path);
        let row_background = if is_selected {
            theme.selection
        } else if is_hovered {
            theme.tab_hover_background
        } else {
            theme.elevated_background.opacity(0.0)
        };
        let row_text = if is_selected {
            theme.accent_foreground
        } else {
            theme.text
        };
        let row_meta = if is_selected {
            theme.accent_foreground.opacity(0.72)
        } else {
            theme.text_subtle
        };

        let click_path = path.clone();
        let hover_path = path.clone();
        let drag_payload = self.drag_payload_for_path(path.clone());
        let drag_label = drag_payload.preview_label();
        let drop_path = path.clone();
        let indent = px(depth as f32 * 16.0);

        let mut row = div()
            .id(format!("file-browser-list-entry-{}", path.display()))
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .pl(px(6.0) + indent)
            .pr(px(6.0))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                this.set_hovered_path(hover_path.clone(), *hovered, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if is_directory {
                        this.toggle_folder_expanded(click_path.clone(), cx);
                    } else {
                        this.select_path(click_path.clone(), cx);
                    }
                }),
            )
            .on_drag(drag_payload, move |_, position, window, cx| {
                cx.stop_propagation();
                cx.new(|cx| DragPreview::new(drag_label.clone().into(), position, window, cx))
            })
            .on_drop(cx.listener(move |this, payload: &FileDragPayload, _, cx| {
                this.request_transfer_to_path(payload.clone(), drop_path.clone(), is_directory, cx);
            }));

        if is_directory {
            row = row.drag_over::<FileDragPayload>(move |style, _payload, _window, _cx| {
                style
                    .border_1()
                    .border_color(theme.drop_target_border.opacity(0.5))
            });
        }

        row.child(
            div()
                .w_full()
                .h_full()
                .px(px(4.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .bg(row_background)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .child(
                            div()
                                .w(px(14.0))
                                .flex_none()
                                .text_xl()
                                .text_color(row_meta)
                                .flex()
                                .items_center()
                                .justify_start()
                                .child(if is_directory {
                                    if is_expanded { "▾" } else { "▸" }
                                } else {
                                    ""
                                }),
                        )
                        .child(
                            svg()
                                .size(px(14.0))
                                .text_color(if is_directory {
                                    theme.accent
                                } else if is_selected {
                                    theme.accent_foreground
                                } else {
                                    theme.icon_muted
                                })
                                .path(icon.path()),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .truncate()
                        .text_sm()
                        .text_color(row_text)
                        .child(truncate_middle(&name, tree_name_max_chars)),
                )
                .child(
                    div()
                        .w(px(140.0))
                        .text_sm()
                        .text_color(row_meta)
                        .child(modified_label),
                )
                .child(
                    div()
                        .w(px(80.0))
                        .text_sm()
                        .text_color(row_meta)
                        .child(size_label),
                )
                .child(
                    div()
                        .w(px(80.0))
                        .text_sm()
                        .text_color(row_meta)
                        .child(kind_label),
                ),
        )
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui.clone();
        let directory = self.root_path.clone();
        let is_directory_loaded = self.directory_cache.contains_key(&directory);

        if self.list_flat_cache.is_none() {
            let mut flat = Vec::new();
            self.build_flat_list(&directory, 0, &mut flat);
            self.list_flat_cache = Some(flat);
        }
        let flat_count = self.list_flat_cache.as_ref().map(|v| v.len()).unwrap_or(0);

        let rows_element: gpui::AnyElement = if is_directory_loaded && flat_count == 0 {
            div()
                .id("file-browser-list")
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .w_full()
                        .px(px(12.0))
                        .pt(px(18.0))
                        .text_sm()
                        .text_color(theme.text_subtle)
                        .child(if self.search_query.is_empty() {
                            "This folder is empty.".to_string()
                        } else {
                            "No matching items.".to_string()
                        }),
                )
                .into_any_element()
        } else {
            let available_width: f32 = self.layout.available_width.map_or(800.0, |w| w.into());
            let tree_fixed_overhead = 12.0 + 12.0 + 14.0 + 14.0 + 1.0 + 140.0 + 80.0 + 80.0;

            let flat_rows = self.list_flat_cache.clone().unwrap_or_default();

            uniform_list(
                "file-browser-list",
                flat_rows.len(),
                cx.processor({
                    let theme = theme.clone();
                    let flat_rows = flat_rows;
                    move |this, range: std::ops::Range<usize>, _window, cx| {
                        let indent_px = |depth: usize| -> f32 { px(depth as f32 * 16.0).into() };
                        range
                            .filter_map(|ix| {
                                if ix >= flat_rows.len() {
                                    return None;
                                }
                                let (entry, depth) = &flat_rows[ix];
                                let indent = indent_px(*depth);
                                let tree_name_max_chars =
                                    ((available_width - indent - tree_fixed_overhead) / 7.5)
                                        .max(20.0) as usize;
                                Some(this.render_list_row(
                                    entry,
                                    *depth,
                                    tree_name_max_chars,
                                    theme.clone(),
                                    cx,
                                ))
                            })
                            .collect()
                    }
                }),
            )
            .w_full()
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        div()
            .flex_1()
            .h_full()
            .bg(theme.app_background)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(rows_element)
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
                    .text_sm()
                    .text_color(theme.text_subtle)
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_base()
                    .text_color(theme.text)
                    .line_height(px(22.0))
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
                let mut text_preview_div = div()
                    .id("file-browser-text-preview")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .overflow_x_hidden()
                    .p(px(12.0))
                    .rounded(px(8.0))
                    .bg(theme.panel_background)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .w_full()
                            .text_sm()
                            .line_height(px(21.0))
                            .text_color(theme.text)
                            .child(text_preview),
                    )
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
                text_preview_div.style().restrict_scroll_to_axis = Some(true);
                content = content.child(text_preview_div);
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
                                .text_base()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(preview.name),
                        )
                        .child(
                            div()
                                .text_sm()
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
                    .text_sm()
                    .text_color(theme.text_subtle)
                    .child("Select a file or folder to inspect it here."),
            );
        }

        div()
            .min_w(px(COLUMN_MIN_WIDTH * 2.0))
            .max_w(px(COLUMN_MAX_WIDTH))
            .flex_1()
            .h_full()
            .bg(theme.app_background)
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
                        .text_base()
                        .text_color(theme.text)
                        .child(message.clone()),
                )
        })
    }
}

impl Focusable for FileBrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<FileBrowserEvent> for FileBrowserView {}

impl TracksFileBrowserWidth for FileBrowserView {
    fn set_tracked_width(&mut self, width: Option<gpui::Pixels>, cx: &mut Context<Self>) {
        if self.layout.set_available_width(width) {
            cx.notify();
        }
    }
}

impl Render for FileBrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        let inner = div()
            .id("file-browser-view")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .bg(theme.app_background)
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.control
                    && event.keystroke.key.eq_ignore_ascii_case("c")
                {
                    cx.emit(FileBrowserEvent::Close);
                    cx.stop_propagation();
                }
            }))
            .when(self.layout.columns().is_resizing(), |this| {
                this.on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _, cx| {
                    this.handle_columns_mouse_move(ev.position.x, cx);
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.handle_columns_mouse_up(cx);
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.handle_columns_mouse_up(cx);
                    }),
                )
            })
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
                            .overflow_hidden()
                            .flex()
                            .gap(px(0.0))
                            .on_drop(cx.listener(|this, payload: &FileDragPayload, _, cx| {
                                this.request_transfer_to_root(payload.clone(), cx);
                            }))
                            .child({
                                if self.view_mode == FileBrowserViewMode::List {
                                    self.render_list(cx).into_any_element()
                                } else {
                                    self.render_columns(cx).into_any_element()
                                }
                            }),
                    ),
            )
            .into_any_element();

        FileBrowserWidthTracker::new("file-browser-render", cx.entity(), inner)
    }
}
