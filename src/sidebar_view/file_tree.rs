use std::collections::{HashSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Stateful, Styled, Subscription, Task, Window, div, px, svg};

use crate::assets::Icon;
use crate::fs_watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;
use crate::sidebar_view::file_system::FileSystem;

const LIST_ROW_HEIGHT: f32 = 24.0;
const INDENT_SIZE: f32 = 14.0;
const MAX_TREE_DEPTH: usize = 20;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileEntryKind,
}

impl FileEntry {
    pub fn from_dir_entry(entry: std::fs::DirEntry) -> Option<Self> {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
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
        Some(Self { name, path, kind })
    }

    pub fn icon(&self) -> Icon {
        match self.kind {
            FileEntryKind::Directory => Icon::Folder,
            FileEntryKind::File | FileEntryKind::Symlink | FileEntryKind::Other => Icon::File,
        }
    }

    pub fn sort_key(&self) -> (u8, String) {
        let rank = match self.kind {
            FileEntryKind::Directory => 0,
            FileEntryKind::Symlink => 1,
            FileEntryKind::File => 2,
            FileEntryKind::Other => 3,
        };
        (rank, self.name.to_lowercase())
    }
}

/// 侧边栏文件树组件 - 单列模式，类似 VSCode 文件树
pub struct SidebarFileTree {
    focus_handle: FocusHandle,
    root_path: PathBuf,
    selected_path: Option<PathBuf>,
    directory_cache: HashMap<PathBuf, Vec<FileEntry>>,
    expanded_folders: HashSet<PathBuf>,
    hovered_path: Option<PathBuf>,
    flat_cache: Option<Vec<(FileEntry, usize)>>,
    backend: Arc<dyn FileSystem>,
    loading_paths: HashSet<PathBuf>,
    /// Directories that need a refresh after the current in-flight load/refresh completes.
    pending_refresh: HashSet<PathBuf>,
    on_file_open: Option<Rc<dyn Fn(PathBuf, Option<SshConnectionConfig>, &mut Window, &mut App) + Send + Sync>>,
    watcher_subscription: Option<Subscription>,
    watched_paths: HashSet<PathBuf>,
    is_local: bool,
}

impl SidebarFileTree {
    pub fn new(cx: &mut Context<Self>, backend: Arc<dyn FileSystem>, root_path: Option<PathBuf>) -> Self {
        let root_path = root_path.unwrap_or_else(|| {
            backend.home_dir().unwrap_or_else(|| PathBuf::from("/"))
        });
        let is_local = backend.is_local();

        let watcher_subscription = if is_local {
            let watcher = cx.fs_watcher();
            Some(cx.subscribe(&watcher, move |this, _, event: &FsChangeEvent, cx| {
                this.handle_fs_event(event, cx);
            }))
        } else {
            let watcher = cx.remote_fs_watcher();
            Some(cx.subscribe(&watcher, move |this, _, event: &FsChangeEvent, cx| {
                this.handle_fs_event(event, cx);
            }))
        };

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            root_path: root_path.clone(),
            selected_path: None,
            directory_cache: HashMap::new(),
            expanded_folders: HashSet::new(),
            hovered_path: None,
            flat_cache: None,
            backend,
            loading_paths: HashSet::new(),
            pending_refresh: HashSet::new(),
            on_file_open: None,
            watcher_subscription,
            watched_paths: HashSet::new(),
            is_local,
        };

        if !is_local {
            cx.watch_remote_path(&root_path, this.backend.clone());
            this.watched_paths.insert(root_path.clone());
        } else {
            cx.watch_recursive_path(&root_path);
        }

        this
    }

    pub fn open_path(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let old_root = self.root_path.clone();

        if let Some(path) = path {
            if path == self.root_path {
                return;
            }
            self.root_path = path;
        }

        if self.is_local {
            cx.unwatch_recursive_path(&old_root);
        } else {
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched);
            }
            self.watched_paths.clear();
        }

        self.directory_cache.clear();
        self.expanded_folders.clear();
        self.flat_cache = None;
        self.pending_refresh.clear();
        let root = self.root_path.clone();
        self.load_directory(root, cx);

        if self.is_local {
            cx.watch_recursive_path(&self.root_path);
        } else {
            cx.watch_remote_path(&self.root_path, self.backend.clone());
            self.watched_paths.insert(self.root_path.clone());
        }

        cx.notify();
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn set_on_file_open(
        &mut self,
        callback: Rc<dyn Fn(PathBuf, Option<SshConnectionConfig>, &mut Window, &mut App) + Send + Sync>,
    ) {
        self.on_file_open = Some(callback);
    }

    fn handle_fs_event(&mut self, event: &FsChangeEvent, cx: &mut Context<Self>) {
        if !event.path.starts_with(&self.root_path) {
            return;
        }

        // Remote directory polling can provide the new entries directly. In that case we can
        // update the cached directory without an extra async load and without a temporary empty
        // state, which prevents flicker.
        if let Some(ref entries) = event.entries {
            if self.directory_cache.contains_key(&event.path)
                || self.loading_paths.contains(&event.path)
            {
                if self.directory_cache.get(&event.path) != Some(entries) {
                    self.loading_paths.remove(&event.path);
                    self.directory_cache.insert(event.path.clone(), entries.clone());
                    self.invalidate_flat_cache();
                    cx.notify();
                }
                return;
            }
        }

        // For Remove events, the path itself is gone. Drop it and its descendants from cache so
        // they disappear from the tree, then refresh the parent directory so it disappears from
        // the parent's listing too.
        if event.kind == FsChangeKind::Remove {
            let removed_path = &event.path;
            let is_root = removed_path == &self.root_path;
            let cache_before = self.directory_cache.len();
            let expanded_before = self.expanded_folders.len();
            if is_root {
                // Keep the root entry itself to avoid a render-driven reload loop; just clear
                // any expanded descendants.
                self.directory_cache.retain(|p, _| p == removed_path);
            } else {
                self.directory_cache.retain(|p, _| !p.starts_with(removed_path));
            }
            self.expanded_folders.retain(|p| !p.starts_with(removed_path));
            let changed = self.directory_cache.len() != cache_before
                || self.expanded_folders.len() != expanded_before
                || self.loading_paths.remove(removed_path);
            if changed {
                self.invalidate_flat_cache();
                cx.notify();
            }
            let mut ancestor = removed_path.parent();
            while let Some(p) = ancestor {
                if self.directory_cache.contains_key(p) || self.loading_paths.contains(p) {
                    self.refresh_directory(p.to_path_buf(), cx);
                    return;
                }
                ancestor = p.parent();
            }
            return;
        }

        // For local Create/Modify events or remote events without entries, find the closest cached
        // ancestor and refresh it incrementally without clearing the cache first.
        let mut ancestor = Some(event.path.as_path());
        while let Some(p) = ancestor {
            if self.directory_cache.contains_key(p) || self.loading_paths.contains(p) {
                self.refresh_directory(p.to_path_buf(), cx);
                return;
            }
            ancestor = p.parent();
        }
    }

    pub fn cleanup(&mut self, cx: &mut Context<Self>) {
        if self.is_local {
            cx.unwatch_recursive_path(&self.root_path);
        } else {
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched);
            }
            self.watched_paths.clear();
        }
        self.watcher_subscription.take();
        self.pending_refresh.clear();
    }

    fn load_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.directory_cache.contains_key(&path) {
            return;
        }
        let Some(task) = self.spawn_read_dir(&path, cx) else {
            return;
        };
        let weak = cx.weak_entity();
        let path_for_loading = path.clone();

        cx.spawn(async move |_, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                // If the path was removed from loading_paths by a watcher event (which already
                // inserted fresher entries), skip writing potentially stale results.
                let was_loading = this.loading_paths.remove(&path_for_loading);
                if was_loading {
                    match result {
                        Ok(entries) => {
                            this.directory_cache.insert(path_for_loading.clone(), entries);
                            this.invalidate_flat_cache();
                            cx.notify();
                        }
                        Err(e) => {
                            log::warn!("Failed to read directory {}: {}", path_for_loading.display(), e);
                            cx.notify();
                        }
                    }
                }
                // If a refresh was requested while this directory was loading, run it now so we
                // don't drop events that arrived during the load.
                if this.pending_refresh.remove(&path_for_loading) {
                    this.refresh_directory(path_for_loading, cx);
                }
            });
        }).detach();
    }

    fn refresh_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.loading_paths.contains(&path) {
            self.pending_refresh.insert(path);
            return;
        }
        let Some(task) = self.spawn_read_dir(&path, cx) else {
            return;
        };
        let weak = cx.weak_entity();
        let path_for_loading = path.clone();

        cx.spawn(async move |_, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                // If a watcher event already removed this path from loading_paths and inserted
                // fresher entries, skip writing potentially stale results.
                let was_loading = this.loading_paths.remove(&path_for_loading);
                if was_loading {
                    match result {
                        Ok(entries) => {
                            if this.directory_cache.get(&path_for_loading) != Some(&entries) {
                                this.directory_cache.insert(path_for_loading.clone(), entries);
                                this.invalidate_flat_cache();
                                cx.notify();
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to refresh directory {}: {}", path_for_loading.display(), e);
                            if this.directory_cache.remove(&path_for_loading).is_some() {
                                this.invalidate_flat_cache();
                                cx.notify();
                                // The path is gone; refresh the parent so it disappears from the
                                // parent's listing too.
                                if let Some(parent) = path_for_loading.parent() {
                                    if this.directory_cache.contains_key(parent)
                                        || this.loading_paths.contains(parent)
                                    {
                                        this.refresh_directory(parent.to_path_buf(), cx);
                                    }
                                }
                            }
                        }
                    }
                }
                // If a refresh was requested while this directory was loading, run it now so we
                // don't drop events that arrived during the load.
                if this.pending_refresh.remove(&path_for_loading) {
                    this.refresh_directory(path_for_loading, cx);
                }
            });
        }).detach();
    }

    fn spawn_read_dir(
        &mut self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<Vec<FileEntry>, String>>> {
        if self.loading_paths.contains(path) {
            return None;
        }
        let path = path.to_path_buf();
        self.loading_paths.insert(path.clone());
        let backend = self.backend.clone();
        Some(cx.background_executor().spawn(async move {
            backend.read_dir(&path)
        }))
    }

    fn invalidate_flat_cache(&mut self) {
        self.flat_cache = None;
    }

    fn build_flat_list(&self, directory: &Path, depth: usize, result: &mut Vec<(FileEntry, usize)>) {
        if depth > MAX_TREE_DEPTH {
            return;
        }
        if let Some(entries) = self.directory_cache.get(directory) {
            for entry in entries {
                let is_directory = entry.kind == FileEntryKind::Directory;
                let is_expanded = is_directory && self.expanded_folders.contains(&entry.path);
                result.push((entry.clone(), depth));
                if is_expanded {
                    self.build_flat_list(&entry.path, depth + 1, result);
                }
            }
        }
    }

    fn toggle_folder_expanded(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.expanded_folders.contains(&path) {
            self.expanded_folders.remove(&path);
            self.pending_refresh.remove(&path);
            if !self.is_local {
                // Unwatch this folder and any nested folders that are still being watched.
                let to_unwatch: Vec<PathBuf> = self
                    .watched_paths
                    .iter()
                    .filter(|p| p.starts_with(&path))
                    .cloned()
                    .collect();
                for p in to_unwatch {
                    cx.unwatch_remote_path(&p);
                    self.watched_paths.remove(&p);
                }
            }
        } else {
            self.expanded_folders.insert(path.clone());
            // 重新展开时强制刷新缓存，确保能看到外部变化
            self.directory_cache.remove(&path);
            self.loading_paths.remove(&path);
            self.load_directory(path.clone(), cx);
            if !self.is_local {
                cx.watch_remote_path(&path, self.backend.clone());
                self.watched_paths.insert(path);
            }
        }
        self.invalidate_flat_cache();
        cx.notify();
    }

    fn select_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected_path = Some(path);
        cx.notify();
    }

    fn set_hovered_path(&mut self, path: PathBuf, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.hovered_path = Some(path);
        } else if self.hovered_path.as_ref() == Some(&path) {
            self.hovered_path = None;
        }
        cx.notify();
    }

    fn render_tree_row(
        &mut self,
        entry: &FileEntry,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let path = entry.path.clone();
        let is_selected = self.selected_path.as_ref() == Some(&path);
        let is_directory = entry.kind == FileEntryKind::Directory;
        let is_expanded = is_directory && self.expanded_folders.contains(&path);
        let name = entry.name.clone();
        let icon = entry.icon();
        let is_hovered = self.hovered_path.as_ref() == Some(&path);
        let theme = cx.theme().ui;

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

        let click_path = path.clone();
        let hover_path = path.clone();
        let indent = px(depth as f32 * INDENT_SIZE);

        let row = div()
            .id(format!("sidebar-file-tree-entry-{}", path.display()))
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .pl(px(4.0) + indent)
            .pr(px(4.0))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                this.set_hovered_path(hover_path.clone(), *hovered, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if is_directory {
                        this.toggle_folder_expanded(click_path.clone(), cx);
                    } else {
                        this.select_path(click_path.clone(), cx);
                        let cb = this.on_file_open.clone();
                        let ssh_config = this.backend.ssh_config();
                        if let Some(callback) = cb {
                            callback(click_path.clone(), ssh_config, window, cx);
                        }
                    }
                }),
            );

        row.child(
            div()
                .w_full()
                .h_full()
                .px(px(2.0))
                .rounded(px(4.0))
                .flex()
                .items_center()
                .gap(px(2.0))
                .bg(row_background)
                .child(
                    // Chevron + Icon
                    div()
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .child(
                            div()
                                .w(px(12.0))
                                .flex_none()
                                .text_color(if is_selected { theme.accent_foreground } else { theme.text_subtle })
                                .flex()
                                .items_center()
                                .justify_center()
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
                    // File name
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .truncate()
                        .text_sm()
                        .text_color(row_text)
                        .child(name),
                ),
        )
    }
}

impl Focusable for SidebarFileTree {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SidebarFileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let directory = self.root_path.clone();

        // Ensure root directory is loaded
        if !self.directory_cache.contains_key(&directory) && !self.loading_paths.contains(&directory) {
            self.load_directory(directory.clone(), cx);
        }

        // Build flat cache if needed
        let flat_count = if let Some(ref flat) = self.flat_cache {
            flat.len()
        } else {
            let mut flat = Vec::new();
            self.build_flat_list(&directory, 0, &mut flat);
            let count = flat.len();
            self.flat_cache = Some(flat);
            count
        };

        let is_directory_loaded = self.directory_cache.contains_key(&directory);

        let rows_element: gpui::AnyElement = if is_directory_loaded && flat_count == 0 {
            div()
                .id("sidebar-file-tree")
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .w_full()
                        .px(px(8.0))
                        .pt(px(12.0))
                        .text_sm()
                        .text_color(theme.text_subtle)
                        .child("空文件夹"),
                )
                .into_any_element()
        } else {
            let flat_rows = self.flat_cache.take().unwrap_or_default();
            let mut list = div()
                .id("sidebar-file-tree")
                .w_full()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col();

            for (entry, depth) in flat_rows {
                list = list.child(self.render_tree_row(&entry, depth, cx));
            }

            list.into_any_element()
        };

        div()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(rows_element)
    }
}
