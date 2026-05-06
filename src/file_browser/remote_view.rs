use std::collections::{HashMap, HashSet};

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    MouseButton, Render, Stateful, Window, div, px, svg,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::spinner::{Spinner, SpinnerSize};
use crate::components::tab_bar::DragPreview;
use crate::file_browser::FileBrowserEvent;
use crate::file_browser::shared::{
    FileBrowserLayoutState, FileBrowserViewMode, FileBrowserWidthTracker, PREVIEW_MIN_WIDTH, TracksFileBrowserWidth,
    format_datetime, format_size, max_chars_for_column_width, render_column_resize_handle,
    render_columns_container, render_icon_columns, render_icon_gallery, render_icon_grid,
    render_icon_list, render_icon_more, render_icon_share, render_icon_tag, truncate_middle,
};
use crate::file_transfer::{FileDragPayload, FileEndpoint, TransferRequest};
use crate::ssh::SshConnectionConfig;
use crate::ssh::sftp_fs::{self, RemoteFileEntry as SftpFileEntry, RemoteFileKind};
use crate::theme::ActiveTheme;

const COLUMN_DEFAULT_WIDTH: f32 = 240.0;
const HEADER_HEIGHT: f32 = 46.0;
const LIST_ROW_HEIGHT: f32 = 24.0;
const LIST_HEADER_HEIGHT: f32 = 26.0;

#[derive(Clone, Debug)]
struct FileEntry {
    name: String,
    name_lower: String,
    path: String,
    kind: RemoteFileKind,
    size: u64,
    modified_at: Option<std::time::SystemTime>,
}

impl FileEntry {
    fn from_sftp_entry(entry: SftpFileEntry) -> Self {
        Self {
            name: entry.name.clone(),
            name_lower: entry.name.to_lowercase(),
            path: entry.path,
            kind: entry.kind,
            size: entry.size,
            modified_at: entry.modified_at,
        }
    }

    fn icon(&self) -> Icon {
        match self.kind {
            RemoteFileKind::Directory => Icon::Folder,
            RemoteFileKind::File | RemoteFileKind::Symlink | RemoteFileKind::Other => Icon::File,
        }
    }

    fn kind_label(&self) -> &'static str {
        match self.kind {
            RemoteFileKind::Directory => "Folder",
            RemoteFileKind::File => "File",
            RemoteFileKind::Symlink => "Symlink",
            RemoteFileKind::Other => "Item",
        }
    }

    fn sort_key(&self) -> (u8, String) {
        let rank = match self.kind {
            RemoteFileKind::Directory => 0,
            RemoteFileKind::Symlink => 1,
            RemoteFileKind::File => 2,
            RemoteFileKind::Other => 3,
        };
        (rank, self.name_lower.clone())
    }

    fn size_label(&self) -> String {
        match self.kind {
            RemoteFileKind::Directory => "--".to_string(),
            _ => format_size(self.size),
        }
    }

    fn modified_label(&self) -> String {
        self.modified_at
            .map(format_datetime)
            .unwrap_or_else(|| "Unknown".to_string())
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

#[derive(Clone, Debug)]
enum LoadRequest {
    OpenRequested {
        requested_path: Option<String>,
    },
    ShowLocation {
        root_path: String,
        selected_path: Option<String>,
        selected_kind: Option<RemoteFileKind>,
    },
}

#[derive(Clone, Debug)]
struct LoadedState {
    home_path: String,
    root_path: String,
    selected_path: Option<String>,
    selected_kind: Option<RemoteFileKind>,
    directory_cache: HashMap<String, Vec<FileEntry>>,
    error_message: Option<String>,
}

pub struct RemoteFileBrowserView {
    focus_handle: FocusHandle,
    ssh_config: SshConnectionConfig,
    search_input: Entity<InputState>,
    search_query: String,
    home_path: Option<String>,
    root_path: String,
    selected_path: Option<String>,
    selected_kind: Option<RemoteFileKind>,
    directory_cache: HashMap<String, Vec<FileEntry>>,
    preview: Option<FilePreview>,
    error_message: Option<String>,
    is_loading: bool,
    is_preview_loading: bool,
    load_generation: u64,
    preview_generation: u64,
    view_mode: FileBrowserViewMode,
    expanded_folders: HashSet<String>,
    hovered_path: Option<String>,
    layout: FileBrowserLayoutState,
}

impl RemoteFileBrowserView {
    pub fn new(
        window: &mut Window,
        cx: &mut App,
        ssh_config: SshConnectionConfig,
        requested_path: Option<String>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let search_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Search")
                    .placeholder_font_size(px(15.0))
            });

            let subscription = cx.subscribe(
                &search_input,
                |this: &mut RemoteFileBrowserView, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        let text = this.search_input.read(cx).value().to_string();
                        this.update_search_query(text, cx);
                    }
                },
            );
            std::mem::forget(subscription);

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                ssh_config,
                search_input,
                search_query: String::new(),
                home_path: None,
                root_path: "/".to_string(),
                selected_path: None,
                selected_kind: None,
                directory_cache: HashMap::new(),
                preview: None,
                error_message: None,
                is_loading: false,
                is_preview_loading: false,
                load_generation: 0,
                preview_generation: 0,
                view_mode: FileBrowserViewMode::Columns,
                expanded_folders: HashSet::new(),
                hovered_path: None,
                layout: FileBrowserLayoutState::new(px(COLUMN_DEFAULT_WIDTH)),
            };
            this.open_requested_path(requested_path, window, cx);
            this
        })
    }

    pub fn open_requested_path(
        &mut self,
        requested_path: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_query.clear();
        self.search_input.update(cx, |input, cx| input.clear(cx));
        self.start_load(LoadRequest::OpenRequested { requested_path }, true, cx);
    }

    pub fn ssh_config(&self) -> &SshConnectionConfig {
        &self.ssh_config
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

    fn set_hovered_path(&mut self, path: String, hovered: bool, cx: &mut Context<Self>) {
        let next = if hovered {
            Some(path.clone())
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

    fn toggle_folder_expanded(&mut self, path: String, cx: &mut Context<Self>) {
        if self.expanded_folders.contains(&path) {
            self.expanded_folders.remove(&path);
            cx.notify();
        } else {
            self.expanded_folders.insert(path.clone());
            if self.directory_cache.contains_key(&path) {
                cx.notify();
            } else {
                self.load_directory(path, cx);
            }
        }
    }

    fn load_directory(&mut self, path: String, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        let ssh_config = self.ssh_config.clone();
        let path_for_task = path.clone();
        let task = cx.background_executor().spawn(async move {
            sftp_fs::with_cached_session(&ssh_config, |session| session.read_dir(&path_for_task))
                .map_err(|e| e.to_string())
        });

        cx.spawn(async move |_, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                let _ = entity.update(cx, |this, cx| {
                    match result {
                        Ok(entries) => {
                            let mut entries: Vec<FileEntry> = entries
                                .into_iter()
                                .map(FileEntry::from_sftp_entry)
                                .collect();
                            entries.sort_by_key(FileEntry::sort_key);
                            this.directory_cache.insert(path, entries);
                        }
                        Err(error) => {
                            if this.error_message.is_none() {
                                this.error_message =
                                    Some(format!("Unable to read {}: {}", path, error));
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn navigate_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = sftp_fs::remote_parent(&self.root_path) {
            let previous_root = self.root_path.clone();
            self.show_location(
                parent,
                Some(previous_root),
                Some(RemoteFileKind::Directory),
                false,
                cx,
            );
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.show_location(
            self.root_path.clone(),
            self.selected_path.clone(),
            self.selected_kind.clone(),
            true,
            cx,
        );
    }

    fn select_path(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(entry) = self.find_cached_entry(&path).cloned() {
            self.selected_path = Some(path.clone());
            self.selected_kind = Some(entry.kind.clone());
            self.error_message = None;
            self.preview = None;
            self.is_preview_loading = false;
            cx.notify();

            if entry.kind == RemoteFileKind::Directory && !self.directory_cache.contains_key(&path)
            {
                self.show_location(
                    self.root_path.clone(),
                    Some(path),
                    Some(RemoteFileKind::Directory),
                    false,
                    cx,
                );
            } else if entry.kind == RemoteFileKind::File {
                self.start_preview_load(cx);
            }
            return;
        }

        self.show_location(self.root_path.clone(), Some(path), None, false, cx);
    }

    fn drag_payload_for_path(&self, path: String) -> FileDragPayload {
        FileDragPayload::move_one(FileEndpoint::Remote {
            ssh_config: self.ssh_config.clone(),
            path,
        })
    }

    fn destination_for_path(&self, path: String, is_directory: bool) -> FileEndpoint {
        let destination_path = if is_directory {
            path
        } else {
            sftp_fs::remote_parent(&path).unwrap_or_else(|| self.root_path.clone())
        };

        FileEndpoint::Remote {
            ssh_config: self.ssh_config.clone(),
            path: destination_path,
        }
    }

    fn request_transfer_to_path(
        &mut self,
        payload: FileDragPayload,
        path: String,
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
                FileEndpoint::Remote {
                    ssh_config: self.ssh_config.clone(),
                    path: self.root_path.clone(),
                },
            ),
        ));
    }

    fn show_location(
        &mut self,
        root_path: String,
        selected_path: Option<String>,
        selected_kind: Option<RemoteFileKind>,
        force_refresh: bool,
        cx: &mut Context<Self>,
    ) {
        if !force_refresh
            && self.has_all_visible_directories_cached(
                &root_path,
                selected_path.as_deref(),
                selected_kind.as_ref(),
            )
        {
            self.root_path = root_path;
            self.selected_path = selected_path;
            self.selected_kind = selected_kind;
            self.error_message = None;
            self.is_loading = false;
            self.preview = None;
            self.is_preview_loading = false;
            cx.notify();

            if self.selected_kind == Some(RemoteFileKind::File) {
                self.start_preview_load(cx);
            }
            return;
        }

        self.start_load(
            LoadRequest::ShowLocation {
                root_path,
                selected_path,
                selected_kind,
            },
            force_refresh,
            cx,
        );
    }

    fn start_load(&mut self, request: LoadRequest, clear_cache: bool, cx: &mut Context<Self>) {
        self.load_generation += 1;
        let load_generation = self.load_generation;
        self.is_loading = true;
        self.is_preview_loading = false;
        self.error_message = None;
        self.preview = None;
        if clear_cache {
            self.directory_cache.clear();
        }
        cx.notify();

        let weak = cx.weak_entity();
        let ssh_config = self.ssh_config.clone();
        let home_hint = self.home_path.clone();
        let existing_cache = self.directory_cache.clone();
        let task = cx.background_executor().spawn(async move {
            load_state_blocking(&ssh_config, home_hint, existing_cache, request)
        });

        cx.spawn(async move |_, cx| {
            let loaded = task.await;
            if let Some(entity) = weak.upgrade() {
                let _ = entity.update(cx, |this, cx| {
                    if this.load_generation != load_generation {
                        return;
                    }

                    this.is_loading = false;
                    match loaded {
                        Ok(state) => this.apply_loaded_state(state, cx),
                        Err(error) => {
                            this.error_message = Some(error);
                            cx.notify();
                        }
                    }
                });
            }
        })
        .detach();
    }

    fn apply_loaded_state(&mut self, state: LoadedState, cx: &mut Context<Self>) {
        self.home_path = Some(state.home_path);
        self.root_path = state.root_path;
        self.selected_path = state.selected_path;
        self.selected_kind = state.selected_kind;
        self.directory_cache = state.directory_cache;
        self.error_message = state.error_message;
        cx.notify();
        self.start_preview_load(cx);
    }

    fn start_preview_load(&mut self, cx: &mut Context<Self>) {
        let preview_path = self
            .selected_path
            .clone()
            .unwrap_or_else(|| self.root_path.clone());
        let home_path = self.home_path.clone().unwrap_or_else(|| "/".to_string());
        let selected_kind = self.selected_kind.clone();

        self.preview_generation += 1;
        let preview_generation = self.preview_generation;
        self.is_preview_loading = true;
        self.preview = None;
        cx.notify();

        let weak = cx.weak_entity();
        let ssh_config = self.ssh_config.clone();
        let directory_cache = self.directory_cache.clone();
        let task = cx.background_executor().spawn(async move {
            build_preview_with_cached_session(
                &ssh_config,
                &home_path,
                &preview_path,
                selected_kind.as_ref(),
                &directory_cache,
            )
        });

        cx.spawn(async move |_, cx| {
            let preview = task.await;
            if let Some(entity) = weak.upgrade() {
                let _ = entity.update(cx, |this, cx| {
                    if this.preview_generation != preview_generation {
                        return;
                    }

                    this.is_preview_loading = false;
                    match preview {
                        Ok(preview) => this.preview = preview,
                        Err(error) => {
                            if this.error_message.is_none() {
                                this.error_message = Some(error);
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn visible_directory_paths(&self) -> Vec<String> {
        let mut directories = vec![self.root_path.clone()];
        let Some(selected_path) = self.selected_path.as_ref() else {
            return directories;
        };

        let root_components = sftp_fs::remote_components(&self.root_path);
        let selected_directory = if self.selected_kind == Some(RemoteFileKind::Directory) {
            selected_path.clone()
        } else {
            sftp_fs::remote_parent(selected_path).unwrap_or_else(|| self.root_path.clone())
        };
        let selected_components = sftp_fs::remote_components(&selected_directory);

        if selected_components.len() < root_components.len()
            || selected_components[..root_components.len()] != root_components[..]
        {
            return directories;
        }

        if selected_directory == self.root_path {
            return directories;
        }

        let mut current = self.root_path.clone();
        for component in &selected_components[root_components.len()..] {
            current = sftp_fs::join_remote_path(&current, component);
            directories.push(current.clone());
        }

        directories
    }

    fn has_all_visible_directories_cached(
        &self,
        root_path: &str,
        selected_path: Option<&str>,
        selected_kind: Option<&RemoteFileKind>,
    ) -> bool {
        visible_directory_paths_for(root_path, selected_path, selected_kind)
            .into_iter()
            .all(|directory| self.directory_cache.contains_key(&directory))
    }

    fn display_name(path: &str) -> String {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
        }
    }

    fn matches_search(&self, entry: &FileEntry) -> bool {
        self.search_query.is_empty() || entry.name_lower.contains(&self.search_query)
    }

    fn selected_child_for_directory(&self, directory: &str) -> Option<String> {
        let selected_path = self.selected_path.as_ref()?;
        if selected_path == directory {
            return None;
        }

        let directory_components = sftp_fs::remote_components(directory);
        let selected_components = sftp_fs::remote_components(selected_path);
        if selected_components.len() <= directory_components.len()
            || selected_components[..directory_components.len()] != directory_components[..]
        {
            return None;
        }

        Some(sftp_fs::join_remote_path(
            directory,
            selected_components[directory_components.len()],
        ))
    }

    fn toolbar_directory_name(&self) -> String {
        if let Some(selected_path) = self.selected_path.as_ref() {
            let target = if self.selected_kind == Some(RemoteFileKind::Directory) {
                selected_path.clone()
            } else {
                sftp_fs::remote_parent(selected_path).unwrap_or_else(|| self.root_path.clone())
            };

            return Self::display_name(&target);
        }

        Self::display_name(&self.root_path)
    }

    fn should_show_preview(&self) -> bool {
        self.selected_kind == Some(RemoteFileKind::File) || self.is_preview_loading
    }

    fn find_cached_entry(&self, path: &str) -> Option<&FileEntry> {
        let parent = sftp_fs::remote_parent(path)?;
        self.directory_cache
            .get(&parent)?
            .iter()
            .find(|entry| entry.path == path)
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

    fn render_toolbar_directory_label(&self, cx: &mut Context<Self>) -> gpui::Div {
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
                    .child(format!(
                        "{}  {}",
                        self.ssh_config.host_label(),
                        self.toolbar_directory_name()
                    )),
            )
            .when(self.is_loading, |this| {
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
                                "remote-file-browser-up",
                                "<",
                                false,
                                |this, cx| this.navigate_up(cx),
                                cx,
                            ))
                            .child(div().w(px(1.0)).h_full().bg(theme.border))
                            .child(self.render_chrome_button(
                                "remote-file-browser-refresh",
                                ">",
                                false,
                                |this, cx| this.refresh(cx),
                                cx,
                            )),
                    )
                    .child(self.render_toolbar_directory_label(cx)),
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
                            .cursor_pointer()
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
                            .cursor_pointer()
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

    fn render_directory_column(
        &self,
        directory: &str,
        column_index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().ui;
        let column_width: f32 = self.column_width_at(column_index).into();
        let name_max_chars = max_chars_for_column_width(column_width, 13.0);
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
            .id(format!("remote-file-browser-column-{}", directory))
            .w_full()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pt(px(6.0))
            .overflow_y_scroll()
            .overflow_x_hidden();

        let column_dir = directory.to_string();
        rows = rows
            .on_drop(cx.listener(move |this, payload: &FileDragPayload, _, cx| {
                this.request_transfer_to_path(
                    payload.clone(),
                    column_dir.clone(),
                    true,
                    cx,
                );
            }));

        if is_directory_loaded && entries.is_empty() {
            rows = rows.child(
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
            );
        } else {
            for entry in entries {
                let path = entry.path.clone();
                let name = entry.name.clone();
                let is_directory = entry.kind == RemoteFileKind::Directory;
                let is_selected = selected_child.as_ref() == Some(&entry.path);
                let is_deepest_selected_folder = entry.kind == RemoteFileKind::Directory
                    && self.selected_path.as_ref() == Some(&entry.path);
                let is_active = is_selected || is_deepest_selected_folder;
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
                    theme.accent_foreground.opacity(0.72)
                } else {
                    theme.text_subtle
                };

                let hover_path = path.clone();
                let click_path = path.clone();
                let drag_payload = self.drag_payload_for_path(path.clone());
                let drag_label = drag_payload.preview_label();
                let drop_path = path.clone();
                rows = rows.child(
                    div()
                        .id(format!("remote-file-entry-{}", path))
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
                            cx.new(|cx| {
                                DragPreview::new(drag_label.clone().into(), position, window, cx)
                            })
                        })
                        .on_drop(cx.listener(move |this, payload: &FileDragPayload, _, cx| {
                            this.request_transfer_to_path(
                                payload.clone(),
                                drop_path.clone(),
                                is_directory,
                                cx,
                            );
                        }))
                        .when(is_directory, |this| {
                            this.drag_over::<FileDragPayload>(move |style, _payload, _window, _cx| {
                                style.border_1().border_color(theme.drop_target_border.opacity(0.5))
                            })
                            .rounded(px(6.0))
                        })
                        .child(
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
                                            RemoteFileKind::Directory => theme.accent,
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
                                                .child(truncate_middle(
                                                    &name, name_max_chars,
                                                )),
                                        )
                                        .child(div().text_sm().text_color(row_meta).child(
                                            if matches!(entry.kind, RemoteFileKind::Directory) {
                                                "›".to_string()
                                            } else {
                                                String::new()
                                            },
                                        )),
                                ),
                        ),
                );
            }
        }

        div()
            .w(self.column_width_at(column_index))
            .h_full()
            .bg(theme.app_background)
            .flex()
            .flex_col()
            .child(rows)
    }

    fn render_resize_handle(
        &self,
        column_index: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        render_column_resize_handle(
            column_index,
            self.layout.columns().is_resizing_column(column_index),
            "remote-file-browser-col-handle",
            "remote-file-browser-col-handle-",
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

    fn render_columns(&self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
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

        render_columns_container("remote-file-browser-columns-container", &theme, columns)
    }

    fn render_tree_rows(
        &self,
        directory: &str,
        depth: usize,
        rows: Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        const MAX_TREE_DEPTH: usize = 20;
        if depth > MAX_TREE_DEPTH {
            return rows;
        }

        let theme = cx.theme().ui;
        let entries: Vec<&FileEntry> = self
            .directory_cache
            .get(directory)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| self.matches_search(entry))
                    .collect()
            })
            .unwrap_or_default();

        let mut rows = rows;
        let indent = px(depth as f32 * 16.0);
        let available_width: f32 = self.layout.available_width().map_or(800.0, |w| w.into());
        let indent_px: f32 = indent.into();
        let tree_fixed_overhead = 12.0 + 12.0 + 14.0 + 14.0 + 1.0 + 140.0 + 80.0 + 80.0;
        let tree_name_max_chars =
            ((available_width - indent_px - tree_fixed_overhead) / 7.5).max(20.0) as usize;

        for entry in entries {
            let path = entry.path.clone();
            let is_selected = self.selected_path.as_ref() == Some(&path);
            let is_directory = entry.kind == RemoteFileKind::Directory;
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
                gpui::transparent_black()
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

            rows = rows.child(
                div()
                    .id(format!("remote-file-browser-list-entry-{}", path))
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
                        cx.new(|cx| {
                            DragPreview::new(drag_label.clone().into(), position, window, cx)
                        })
                    })
                    .on_drop(cx.listener(move |this, payload: &FileDragPayload, _, cx| {
                        this.request_transfer_to_path(
                            payload.clone(),
                            drop_path.clone(),
                            is_directory,
                            cx,
                        );
                    }))
                    .when(is_directory, |this| {
                        this.drag_over::<FileDragPayload>(move |style, _payload, _window, _cx| {
                            style.border_1().border_color(theme.drop_target_border.opacity(0.5))
                        })
                    })
                    .child(
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
                                    .child(truncate_middle(
                                        &name, tree_name_max_chars,
                                    )),
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
                    ),
            );

            if is_expanded {
                rows = self.render_tree_rows(&path, depth + 1, rows, cx);
            }
        }

        rows
    }

    fn render_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;
        let directory = self.root_path.clone();
        let is_directory_loaded = self.directory_cache.contains_key(&directory);

        let mut rows = div()
            .id("remote-file-browser-list")
            .w_full()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .overflow_x_hidden();

        if is_directory_loaded
            && self
                .directory_cache
                .get(&directory)
                .map_or(true, |e| !e.iter().any(|e| self.matches_search(e)))
        {
            rows = rows.child(
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
            );
        } else {
            rows = self.render_tree_rows(&directory, 0, rows, cx);
        }

        div()
            .flex_1()
            .h_full()
            .bg(theme.app_background)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(rows)
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
                content = content.child(
                    div()
                        .id("remote-file-browser-text-preview")
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
            content = content.child(if self.is_preview_loading {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(Spinner::new().size(SpinnerSize::Small).color(theme.accent))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_subtle)
                            .child("Loading preview..."),
                    )
            } else {
                div()
                    .text_sm()
                    .text_color(theme.text_subtle)
                    .child("Select a file or folder to inspect it here.")
            });
        }

        div()
            .min_w(px(PREVIEW_MIN_WIDTH)).flex_1()
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

fn load_state_blocking(
    ssh_config: &SshConnectionConfig,
    home_hint: Option<String>,
    existing_cache: HashMap<String, Vec<FileEntry>>,
    request: LoadRequest,
) -> Result<LoadedState, String> {
    sftp_fs::with_cached_session(ssh_config, |session| {
        let home_path = home_hint.clone().unwrap_or_else(|| {
            session
                .canonicalize(".")
                .unwrap_or_else(|_| "/".to_string())
        });

        match &request {
            LoadRequest::OpenRequested { requested_path } => {
                let requested_path = requested_path.clone().unwrap_or_else(|| ".".to_string());
                let mut error_message = None;
                let resolved_path = match session.canonicalize(&requested_path) {
                    Ok(path) => path,
                    Err(error) => {
                        error_message = Some(format!("Unable to resolve remote path: {}", error));
                        home_path.clone()
                    }
                };

                let (root_path, selected_path, selected_kind) = match session
                    .metadata(&resolved_path)
                {
                    Ok(metadata) if metadata.kind == RemoteFileKind::Directory => {
                        (resolved_path, None, None)
                    }
                    Ok(metadata) => (
                        sftp_fs::remote_parent(&resolved_path).unwrap_or_else(|| home_path.clone()),
                        Some(resolved_path),
                        Some(metadata.kind),
                    ),
                    Err(error) => {
                        error_message = Some(format!("Unable to open remote location: {}", error));
                        (home_path.clone(), None, None)
                    }
                };

                build_loaded_state(
                    session,
                    home_path,
                    root_path,
                    selected_path,
                    selected_kind,
                    existing_cache.clone(),
                    error_message,
                )
                .map_err(anyhow::Error::msg)
            }
            LoadRequest::ShowLocation {
                root_path,
                selected_path,
                selected_kind,
            } => build_loaded_state(
                session,
                home_path,
                root_path.clone(),
                selected_path.clone(),
                selected_kind.clone(),
                existing_cache.clone(),
                None,
            )
            .map_err(anyhow::Error::msg),
        }
    })
    .map_err(|error| error.to_string())
}

fn build_loaded_state(
    session: &mut sftp_fs::BlockingSftpSession,
    home_path: String,
    root_path: String,
    selected_path: Option<String>,
    selected_kind_hint: Option<RemoteFileKind>,
    mut directory_cache: HashMap<String, Vec<FileEntry>>,
    initial_error: Option<String>,
) -> Result<LoadedState, String> {
    let selected_kind = match (&selected_path, selected_kind_hint) {
        (_, Some(kind)) => Some(kind),
        (Some(path), None) => {
            let parent = sftp_fs::remote_parent(path);
            parent
                .as_ref()
                .and_then(|parent| directory_cache.get(parent))
                .and_then(|entries| entries.iter().find(|entry| entry.path == *path))
                .map(|entry| entry.kind.clone())
                .or_else(|| session.metadata(path).map(|metadata| metadata.kind).ok())
        }
        (None, None) => None,
    };

    let visible_directories =
        visible_directory_paths_for(&root_path, selected_path.as_deref(), selected_kind.as_ref());
    let mut error_message = initial_error;

    for directory in visible_directories {
        if directory_cache.contains_key(&directory) {
            continue;
        }

        match session.read_dir(&directory) {
            Ok(entries) => {
                let mut entries: Vec<FileEntry> = entries
                    .into_iter()
                    .map(FileEntry::from_sftp_entry)
                    .collect();
                entries.sort_by_key(FileEntry::sort_key);
                directory_cache.insert(directory, entries);
            }
            Err(error) => {
                if error_message.is_none() {
                    error_message = Some(format!("Unable to read {}: {}", directory, error));
                }
            }
        }
    }

    Ok(LoadedState {
        home_path,
        root_path,
        selected_path,
        selected_kind,
        directory_cache,
        error_message,
    })
}

fn visible_directory_paths_for(
    root_path: &str,
    selected_path: Option<&str>,
    selected_kind: Option<&RemoteFileKind>,
) -> Vec<String> {
    let mut directories = vec![root_path.to_string()];
    let Some(selected_path) = selected_path else {
        return directories;
    };

    let root_components = sftp_fs::remote_components(root_path);
    let selected_directory = if selected_kind == Some(&RemoteFileKind::Directory) {
        selected_path.to_string()
    } else {
        sftp_fs::remote_parent(selected_path).unwrap_or_else(|| root_path.to_string())
    };
    let selected_components = sftp_fs::remote_components(&selected_directory);

    if selected_components.len() < root_components.len()
        || selected_components[..root_components.len()] != root_components[..]
    {
        return directories;
    }

    if selected_directory == root_path {
        return directories;
    }

    let mut current = root_path.to_string();
    for component in &selected_components[root_components.len()..] {
        current = sftp_fs::join_remote_path(&current, component);
        directories.push(current.clone());
    }

    directories
}

fn build_preview_blocking(
    session: &mut sftp_fs::BlockingSftpSession,
    home_path: &str,
    preview_path: &str,
    selected_kind: Option<&RemoteFileKind>,
    directory_cache: &HashMap<String, Vec<FileEntry>>,
) -> Option<FilePreview> {
    let metadata = session.metadata(preview_path).ok()?;
    let is_directory = metadata.kind == RemoteFileKind::Directory;
    let name = RemoteFileBrowserView::display_name(preview_path);
    let kind_label = if is_directory { "Folder" } else { "File" }.to_string();
    let size_label = if is_directory {
        "Directory".to_string()
    } else {
        format_size(metadata.size)
    };
    let modified_label = metadata
        .modified_at
        .map(format_datetime)
        .unwrap_or_else(|| "Unknown".to_string());
    let location_label = sftp_fs::remote_parent(preview_path)
        .map(|parent| display_path_with_home(home_path, &parent))
        .unwrap_or_else(|| display_path_with_home(home_path, preview_path));
    let item_count_label = if is_directory {
        Some(format!(
            "{} items",
            directory_cache.get(preview_path).map(Vec::len).unwrap_or(0)
        ))
    } else {
        None
    };
    let text_preview = if selected_kind == Some(&RemoteFileKind::File) {
        session.read_text_preview(preview_path).ok().flatten()
    } else {
        None
    };

    Some(FilePreview {
        name,
        path: display_path_with_home(home_path, preview_path),
        kind_label,
        size_label,
        modified_label,
        location_label,
        item_count_label,
        text_preview,
    })
}

fn build_preview_with_cached_session(
    ssh_config: &SshConnectionConfig,
    home_path: &str,
    preview_path: &str,
    selected_kind: Option<&RemoteFileKind>,
    directory_cache: &HashMap<String, Vec<FileEntry>>,
) -> Result<Option<FilePreview>, String> {
    sftp_fs::with_cached_session(ssh_config, |session| {
        Ok(build_preview_blocking(
            session,
            home_path,
            preview_path,
            selected_kind,
            directory_cache,
        ))
    })
    .map_err(|error| error.to_string())
}

fn display_path_with_home(home_path: &str, path: &str) -> String {
    if path == home_path {
        return "~".to_string();
    }

    if let Some(relative_path) = path.strip_prefix(home_path) {
        if relative_path.starts_with('/') {
            return format!("~{}", relative_path);
        }
    }

    path.to_string()
}

impl Focusable for RemoteFileBrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<FileBrowserEvent> for RemoteFileBrowserView {}

impl TracksFileBrowserWidth for RemoteFileBrowserView {
    fn set_tracked_width(&mut self, width: Option<gpui::Pixels>, cx: &mut Context<Self>) {
        if self.layout.set_available_width(width) {
            cx.notify();
        }
    }
}

impl Render for RemoteFileBrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        let inner = div()
            .id("remote-file-browser-view")
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

        FileBrowserWidthTracker::new("remote-file-browser-render", cx.entity(), inner)
    }
}
