use gpui::prelude::*;
use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, Stateful, Styled, Subscription, Task, Window, div, px, svg,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::assets::Icon;
use crate::file_manager::FileSystem;
use crate::file_manager::file_tree_cache::RemoteFileTreeCache;
use crate::file_manager::file_tree_event::FileTreeEvent;
use crate::file_manager::file_tree_loader::FileTreeLoader;
use crate::file_manager::types::{FileEntry, FileEntryKind};
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;

impl gpui::EventEmitter<crate::file_manager::file_tree_event::FileTreeEvent> for SidebarFileTree {}

fn entry_icon(kind: &FileEntryKind) -> Icon {
    match kind {
        FileEntryKind::Directory => Icon::Folder,
        FileEntryKind::File | FileEntryKind::Symlink | FileEntryKind::Other => Icon::File,
    }
}

const LIST_ROW_HEIGHT: f32 = 24.0;
const INDENT_SIZE: f32 = 14.0;
const MAX_TREE_DEPTH: usize = 20;

/// 侧边栏文件树组件 - 单列模式，类似 VSCode 文件树
///
/// View 层：只负责渲染和用户交互，所有文件操作委托给 `FileTreeLoader`。
pub struct SidebarFileTree {
    focus_handle: FocusHandle,
    root_path: PathBuf,
    selected_path: Option<PathBuf>,
    /// 所有文件操作都委托给 loader
    loader: FileTreeLoader,
    expanded_folders: HashSet<PathBuf>,
    hovered_path: Option<PathBuf>,
    flat_cache: Option<Vec<(FileEntry, usize)>>,
    watcher_subscription: Option<Subscription>,
    watched_paths: HashSet<PathBuf>,
}

impl SidebarFileTree {
    pub fn new(
        cx: &mut Context<Self>,
        backend: Arc<dyn FileSystem>,
        root_path: Option<PathBuf>,
    ) -> Self {
        let root_path =
            root_path.unwrap_or_else(|| backend.home_dir().unwrap_or_else(|| PathBuf::from("/")));
        let is_local = backend.is_local();

        let watcher_subscription = if is_local {
            let watcher = cx.fs_watcher();
            Some(
                cx.subscribe(&watcher, move |this, _, event: &FsChangeEvent, cx| {
                    this.handle_fs_event(event, cx);
                }),
            )
        } else {
            let watcher = cx.remote_fs_watcher();
            Some(
                cx.subscribe(&watcher, move |this, _, event: &FsChangeEvent, cx| {
                    this.handle_fs_event(event, cx);
                }),
            )
        };

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            root_path: root_path.clone(),
            selected_path: None,
            loader: FileTreeLoader::new(backend),
            expanded_folders: HashSet::new(),
            hovered_path: None,
            flat_cache: None,
            watcher_subscription,
            watched_paths: HashSet::new(),
        };

        // 从共享缓存恢复之前的展开状态
        if let Some(config) = this.loader.ssh_config() {
            let cached = RemoteFileTreeCache::global()
                .lock()
                .unwrap()
                .load_expanded(config);
            if !cached.is_empty() {
                this.expanded_folders = cached;
            }
        }

        if is_local {
            cx.watch_recursive_path(&root_path);
        } else {
            cx.watch_remote_path(&root_path, this.loader.backend().clone());
            this.watched_paths.insert(root_path.clone());
        }

        // 为已恢复的展开文件夹触发加载并设置监听
        if !this.expanded_folders.is_empty() {
            let expanded: Vec<PathBuf> = this.expanded_folders.iter().cloned().collect();
            for path in expanded {
                this.load_directory(path.clone(), cx);
                if !this.watched_paths.contains(&path) && !is_local {
                    cx.watch_remote_path(&path, this.loader.backend().clone());
                    this.watched_paths.insert(path);
                }
            }
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

        if self.loader.is_local() {
            cx.unwatch_recursive_path(&old_root);
        } else {
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched);
            }
            self.watched_paths.clear();
        }

        // 不再清空 directory_cache，保留共享缓存中的目录数据以加速切换
        self.expanded_folders.clear();
        self.flat_cache = None;
        self.loader.clear_pending_refresh();
        let root = self.root_path.clone();
        self.load_directory(root, cx);

        if self.loader.is_local() {
            cx.watch_recursive_path(&self.root_path);
        } else {
            cx.watch_remote_path(&self.root_path, self.loader.backend().clone());
            self.watched_paths.insert(self.root_path.clone());
        }

        cx.notify();
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    fn handle_fs_event(&mut self, event: &FsChangeEvent, cx: &mut Context<Self>) {
        if !event.path.starts_with(&self.root_path) {
            return;
        }

        // 委托给 loader 处理缓存更新
        let affected = self.loader.handle_fs_event(&self.root_path, event);

        // Remote watcher 直接提供了新 entries → loader 已直接更新本地缓存
        // 检查是否有带 entries 的事件已被 loader 直接处理
        if let Some(ref _entries) = event.entries {
            if self.loader.has_cached(&event.path) || self.loader.is_loading(&event.path) {
                // loader 已处理，只需通知 UI
                self.invalidate_flat_cache();
                cx.notify();
                return;
            }
        }

        // Remove 事件
        if event.kind == FsChangeKind::Remove {
            let removed_path = &event.path;
            self.expanded_folders
                .retain(|p| !p.starts_with(removed_path));
            if affected.is_some() || self.loader.has_cached(&self.root_path) {
                self.invalidate_flat_cache();
                cx.notify();
            }
            if let Some(ancestor) = affected {
                self.refresh_directory(ancestor, cx);
            }
            return;
        }

        // Create/Modify 事件
        if let Some(ancestor) = affected {
            self.refresh_directory(ancestor, cx);
        }
    }

    pub fn cleanup(&mut self, cx: &mut Context<Self>) {
        self.loader.release_shared_cache();
        if self.loader.is_local() {
            cx.unwatch_recursive_path(&self.root_path);
        } else {
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched);
            }
            self.watched_paths.clear();
        }
        self.watcher_subscription.take();
        self.loader.clear_pending_refresh();
    }

    /// 分离：做 cleanup 可选是否释放共享缓存引用。
    /// 用于同一服务器上切换终端时保留缓存，或远程切远程时归还引用。
    pub fn detach(&mut self, cx: &mut Context<Self>, release_cache: bool) {
        // 保存展开状态到共享缓存，以便切换回来时恢复
        if let Some(config) = self.loader.ssh_config() {
            RemoteFileTreeCache::global()
                .lock()
                .unwrap()
                .save_expanded(config, self.expanded_folders.clone());
        }
        if release_cache {
            self.loader.release_shared_cache();
        }
        if self.loader.is_local() {
            cx.unwatch_recursive_path(&self.root_path);
        } else {
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched);
            }
            self.watched_paths.clear();
        }
        self.watcher_subscription.take();
        self.loader.clear_pending_refresh();
    }

    pub fn is_local(&self) -> bool {
        self.loader.is_local()
    }

    pub fn matches_location(
        &self,
        path: &Path,
        is_local: bool,
        ssh_config: Option<&SshConnectionConfig>,
    ) -> bool {
        self.root_path == path && self.loader.backend().matches(is_local, ssh_config)
    }

    fn load_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // 委托给 loader；若已在加载中则跳过，避免重复任务
        if !self.loader.begin_load(&path) {
            return;
        }

        // 先查共享缓存，命中则立即显示 + 后台刷新
        if let Some(entries) = self.loader.get_shared_cache(&path) {
            self.loader.finish_load(&path, entries);
            self.invalidate_flat_cache();
            cx.notify();
            self.spawn_background_refresh(path, cx);
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
                if this.loader.is_loading(&path_for_loading) {
                    match result {
                        Ok(entries) => {
                            // 写入共享缓存
                            this.loader
                                .set_shared_cache(&path_for_loading, entries.clone());
                            this.loader.finish_load(&path_for_loading, entries);
                            this.invalidate_flat_cache();
                            cx.notify();
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to read directory {}: {}",
                                path_for_loading.display(),
                                e
                            );
                            this.loader.fail_load(&path_for_loading);
                            cx.notify();
                        }
                    }
                }
                // If a refresh was requested while this directory was loading, run it now so we
                // don't drop events that arrived during the load.
                if this.loader.take_pending_refresh(&path_for_loading) {
                    this.refresh_directory(path_for_loading, cx);
                }
            });
        })
        .detach();
    }

    fn refresh_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.loader.request_refresh(&path) {
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
                if this.loader.is_loading(&path_for_loading) {
                    match result {
                        Ok(entries) => {
                            if this.loader.get_cached(&path_for_loading) != Some(&entries) {
                                // 更新共享缓存
                                this.loader
                                    .set_shared_cache(&path_for_loading, entries.clone());
                                this.loader.finish_load(&path_for_loading, entries);
                                this.invalidate_flat_cache();
                                cx.notify();
                            } else {
                                this.loader.fail_load(&path_for_loading);
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to refresh directory {}: {}",
                                path_for_loading.display(),
                                e
                            );
                            if this.loader.fail_refresh(&path_for_loading) {
                                // 从共享缓存中也移除
                                this.loader.remove_shared_cache(&path_for_loading);
                                this.invalidate_flat_cache();
                                cx.notify();
                                // The path is gone; refresh the parent so it disappears from the
                                // parent's listing too.
                                if let Some(parent) = path_for_loading.parent() {
                                    if this.loader.has_cached(parent)
                                        || this.loader.is_loading(parent)
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
                if this.loader.take_pending_refresh(&path_for_loading) {
                    this.refresh_directory(path_for_loading, cx);
                }
            });
        })
        .detach();
    }

    /// 缓存命中后发起的后台刷新：不显示加载状态，静默更新
    fn spawn_background_refresh(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let backend = self.loader.backend().clone();
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let result: Result<Vec<FileEntry>, String> = cx
                .background_executor()
                .spawn({
                    let backend = backend.clone();
                    let path = path.clone();
                    async move { backend.read_dir(&path) }
                })
                .await;
            if let Ok(entries) = result {
                let _ = weak.update(cx, |this, cx| {
                    this.loader.set_shared_cache(&path, entries.clone());
                    if this.loader.update_cached(&path, entries) {
                        this.invalidate_flat_cache();
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn spawn_read_dir(
        &mut self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<Vec<FileEntry>, String>>> {
        let path = path.to_path_buf();
        let backend = self.loader.backend().clone();
        Some(
            cx.background_executor()
                .spawn(async move { backend.read_dir(&path) }),
        )
    }

    fn invalidate_flat_cache(&mut self) {
        self.flat_cache = None;
    }

    fn build_flat_list(
        &self,
        directory: &Path,
        depth: usize,
        result: &mut Vec<(FileEntry, usize)>,
    ) {
        if depth > MAX_TREE_DEPTH {
            return;
        }
        if let Some(entries) = self.loader.get_cached(directory) {
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
            // 清除 pending refresh
            self.loader.clear_pending_refresh_for(&path);
            if !self.loader.is_local() {
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
            self.loader.invalidate_path(&path);
            self.load_directory(path.clone(), cx);
            if !self.loader.is_local() {
                cx.watch_remote_path(&path, self.loader.backend().clone());
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
        let icon = entry_icon(&entry.kind);
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
                cx.listener(move |this, _, _window, cx| {
                    if is_directory {
                        this.toggle_folder_expanded(click_path.clone(), cx);
                    } else {
                        this.select_path(click_path.clone(), cx);
                        cx.emit(FileTreeEvent::OpenFile {
                            path: click_path.clone(),
                            ssh_config: if this.loader.is_local() {
                                None
                            } else {
                                this.loader.ssh_config().cloned()
                            },
                        });
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
                                .text_color(if is_selected {
                                    theme.accent_foreground
                                } else {
                                    theme.text_subtle
                                })
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
        if !self.loader.has_cached(&directory) && !self.loader.is_loading(&directory) {
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

        let is_directory_loaded = self.loader.has_cached(&directory);

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
