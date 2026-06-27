use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Stateful, Styled, Window, div, px, svg};

use crate::assets::Icon;
use crate::theme::ActiveTheme;

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

#[derive(Clone, Debug)]
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

    fn icon(&self) -> Icon {
        match self.kind {
            FileEntryKind::Directory => Icon::Folder,
            FileEntryKind::File | FileEntryKind::Symlink | FileEntryKind::Other => Icon::File,
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

/// 侧边栏文件树组件 - 单列模式，类似 VSCode 文件树
pub struct SidebarFileTree {
    focus_handle: FocusHandle,
    root_path: PathBuf,
    selected_path: Option<PathBuf>,
    directory_cache: std::collections::HashMap<PathBuf, Vec<FileEntry>>,
    expanded_folders: HashSet<PathBuf>,
    hovered_path: Option<PathBuf>,
    flat_cache: Option<Vec<(FileEntry, usize)>>,
}

impl SidebarFileTree {
    pub fn new(cx: &mut App, root_path: Option<PathBuf>) -> Self {
        let root_path = root_path.unwrap_or_else(|| {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        });
        Self {
            focus_handle: cx.focus_handle(),
            root_path: root_path.clone(),
            selected_path: None,
            directory_cache: std::collections::HashMap::new(),
            expanded_folders: HashSet::new(),
            hovered_path: None,
            flat_cache: None,
        }
    }

    pub fn open_path(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        if let Some(path) = path {
            self.root_path = path;
        }
        self.directory_cache.clear();
        self.expanded_folders.clear();
        self.flat_cache = None;
        let root = self.root_path.clone();
        self.load_directory(&root, cx);
        cx.notify();
    }

    fn load_directory(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.directory_cache.contains_key(path) {
            return;
        }
        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut file_entries: Vec<FileEntry> = entries
                    .filter_map(|e| e.ok().and_then(FileEntry::from_dir_entry))
                    .collect();
                file_entries.sort_by_key(|a| a.sort_key());
                self.directory_cache.insert(path.to_path_buf(), file_entries);
                self.invalidate_flat_cache();
                cx.notify();
            }
            Err(e) => {
                log::warn!("Failed to read directory {}: {}", path.display(), e);
            }
        }
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
        } else {
            self.expanded_folders.insert(path.clone());
            self.load_directory(&path, cx);
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
                cx.listener(move |this, _, _, cx| {
                    if is_directory {
                        this.toggle_folder_expanded(click_path.clone(), cx);
                    } else {
                        this.select_path(click_path.clone(), cx);
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
        if !self.directory_cache.contains_key(&directory) {
            self.load_directory(&directory, cx);
        }

        // Build flat cache if needed
        if self.flat_cache.is_none() {
            let mut flat = Vec::new();
            self.build_flat_list(&directory, 0, &mut flat);
            self.flat_cache = Some(flat);
        }

        let flat_count = self.flat_cache.as_ref().map(|v| v.len()).unwrap_or(0);
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
            let flat_rows = self.flat_cache.clone().unwrap_or_default();
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
