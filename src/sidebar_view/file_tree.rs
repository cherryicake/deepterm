use gpui::prelude::*;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Context, FocusHandle, Focusable, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseButton, ParentElement,
    Pixels, Point, Render, Size, Stateful, Styled, Subscription, Task, UniformListDecoration,
    UniformListScrollHandle, Window, div, point, px, size, svg, uniform_list,
};
use smallvec::SmallVec;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::assets::Icon;
use crate::file_icons::file_icon_or_default;
use crate::file_manager::FileSystem;
use crate::file_manager::file_tree_cache::RemoteFileTreeCache;
use crate::file_manager::file_tree_event::FileTreeEvent;
use crate::file_manager::file_tree_loader::FileTreeLoader;
use crate::file_manager::types::{FileEntry, FileEntryKind};
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;
use crate::theme::UiTheme;

impl gpui::EventEmitter<crate::file_manager::file_tree_event::FileTreeEvent> for SidebarFileTree {}

fn entry_icon(entry: &FileEntry) -> Icon {
    match entry.kind {
        FileEntryKind::Directory => Icon::Folder,
        FileEntryKind::File | FileEntryKind::Symlink | FileEntryKind::Other => {
            file_icon_or_default(&entry.path)
        }
    }
}

const LIST_ROW_HEIGHT: f32 = 24.0;
const INDENT_SIZE: f32 = 14.0;
const MAX_TREE_DEPTH: usize = 20;

#[derive(Clone)]
struct StickyFolderCandidate {
    global_index: usize,
    depth: usize,
}

impl StickyFolderCandidate {
    fn depth(&self) -> usize {
        self.depth
    }
}

#[derive(Clone)]
struct StickyAnchor {
    candidate: StickyFolderCandidate,
    index: usize,
    drifting: bool,
}

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
    root_expanded: bool,
    hovered_path: Option<PathBuf>,
    flat_cache: Option<Vec<(FileEntry, usize)>>,
    watcher_subscription: Option<Subscription>,
    watched_paths: HashSet<PathBuf>,
    scroll_handle: UniformListScrollHandle,
    sticky_items_count: usize,
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
            root_expanded: true,
            hovered_path: None,
            flat_cache: None,
            watcher_subscription,
            watched_paths: HashSet::new(),
            scroll_handle: UniformListScrollHandle::new(),
            sticky_items_count: 0,
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

    fn render_root_row(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let root_path = self.root_path.clone();
        let is_expanded = self.root_expanded;
        let is_hovered = self.hovered_path.as_ref() == Some(&root_path);
        let theme = cx.theme().ui;
        let display_name = root_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root_path.to_string_lossy().to_string())
            .to_uppercase();

        let row_background = if is_hovered {
            theme.tab_hover_background
        } else {
            theme.elevated_background.opacity(0.0)
        };

        let hover_path = root_path.clone();
        let row = div()
            .id("sidebar-file-tree-root")
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .pl(px(4.0))
            .pr(px(4.0))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                this.set_hovered_path(hover_path.clone(), *hovered, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    this.root_expanded = !this.root_expanded;
                    this.invalidate_flat_cache();
                    cx.notify();
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
                    div()
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .child(
                            div()
                                .w(px(12.0))
                                .flex_none()
                                .text_color(theme.text_subtle)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(if is_expanded { "▾" } else { "▸" }),
                        )
                        .child(
                            svg()
                                .size(px(14.0))
                                .text_color(theme.accent)
                                .path(Icon::Folder.path()),
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
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(display_name),
                ),
        )
    }

    fn render_sticky_folders(
        &self,
        anchor: &StickyAnchor,
        scroll_offset: Point<Pixels>,
        item_height: Pixels,
        theme: &UiTheme,
    ) -> SmallVec<[AnyElement; 8]> {
        let flat = self.flat_cache.as_ref().unwrap();
        let anchor_depth = anchor.candidate.depth;

        // 收集所有ancestor folders（从anchor向上追溯，不包含anchor本身）
        // anchor本身已经在正常列表中渲染，不需要在sticky中重复显示
        let mut sticky_parents: Vec<(FileEntry, usize)> = Vec::new();
        let mut current_depth = anchor_depth;
        let mut idx = anchor.candidate.global_index;

        while idx > 0 {
            idx -= 1;
            if let Some((entry, depth)) = flat.get(idx) {
                if *depth < current_depth {
                    sticky_parents.push((entry.clone(), *depth + 1));
                    current_depth = *depth;
                    if current_depth == 0 {
                        break;
                    }
                }
            }
        }
        sticky_parents.reverse();

        // 计算drifting偏移
        let drifting_y_offset = if anchor.drifting {
            let scroll_top = -scroll_offset.y;
            let anchor_top = item_height * (anchor.index + 1) as f32;
            let sticky_area_height = item_height * sticky_parents.len() as f32;
            (anchor_top - scroll_top - sticky_area_height).min(Pixels::ZERO)
        } else {
            Pixels::ZERO
        };

        sticky_parents
            .iter()
            .enumerate()
            .map(|(index, (entry, depth))| {
                let is_last = index == sticky_parents.len() - 1;
                self.render_sticky_row(entry, *depth, is_last, drifting_y_offset, theme)
                    .into_any()
            })
            .collect()
    }

    fn render_sticky_row(
        &self,
        entry: &FileEntry,
        depth: usize,
        is_bottom: bool,
        y_offset: Pixels,
        theme: &UiTheme,
    ) -> gpui::Div {
        let indent = px(depth as f32 * INDENT_SIZE);
        let name = entry.name.clone();
        let icon = entry_icon(entry);

        let row = div()
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .pl(px(4.0) + indent)
            .pr(px(4.0));

        let inner = div()
            .w_full()
            .h_full()
            .px(px(2.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .bg(theme.elevated_background)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(1.0))
                    .child(
                        div()
                            .w(px(12.0))
                            .flex_none()
                            .text_color(theme.text_subtle)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("▾"),
                    )
                    .child(
                        svg()
                            .size(px(14.0))
                            .text_color(theme.accent)
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
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(name),
            );

        let mut result = row.child(inner);

        if is_bottom {
            let shadow = div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(-1.5))
                .h(px(1.5))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(theme.shadow.opacity(0.15), 1.0),
                    gpui::linear_color_stop(theme.shadow.opacity(0.0), 0.0),
                ));
            result = result.child(shadow);
        }

        if y_offset != Pixels::ZERO {
            result = result.relative().top(y_offset);
        }

        result
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
        let icon = entry_icon(entry);
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
                                .size(if is_directory { px(14.0) } else { px(18.0) })
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

struct StickyFolderDecoration {
    entity: gpui::Entity<SidebarFileTree>,
}

impl UniformListDecoration for StickyFolderDecoration {
    fn compute(
        &self,
        visible_range: Range<usize>,
        bounds: Bounds<Pixels>,
        scroll_offset: Point<Pixels>,
        item_height: Pixels,
        _item_count: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // 获取可见范围内的entries
        let entries: Vec<(FileEntry, usize)> = self.entity.read_with(cx, |this, _cx| {
            if let Some(flat) = &this.flat_cache {
                visible_range
                    .clone()
                    .filter_map(|ix| flat.get(ix).cloned())
                    .collect()
            } else {
                Vec::new()
            }
        });

        // 找到sticky anchor
        let anchor = Self::find_sticky_anchor_from_entries(&entries, visible_range.start);

        if let Some(anchor) = anchor {
            let theme = cx.theme().ui;
            let sticky_items = self.entity.read_with(cx, |this, _cx| {
                this.render_sticky_folders(&anchor, scroll_offset, item_height, &theme)
            });

            let items_count = sticky_items.len();
            if items_count == 0 {
                return StickyItemsElement {
                    drifting_element: None,
                    rest_elements: SmallVec::new(),
                }
                .into_any();
            }

            // ★★★ 关键：计算base_origin，抵消滚动偏移 ★★★
            let base_origin = bounds.origin - point(px(0.), scroll_offset.y);

            // 计算drifting偏移
            let drifting_y_offset = if anchor.drifting {
                let scroll_top = -scroll_offset.y;
                let anchor_top = item_height * (anchor.index + 1) as f32;
                let sticky_area_height = item_height * items_count as f32;
                (anchor_top - scroll_top - sticky_area_height).min(Pixels::ZERO)
            } else {
                Pixels::ZERO
            };

            // 分离drifting和rest元素
            let mut elements = sticky_items;
            let (mut drifting_element, mut rest_elements) =
                if anchor.drifting && !elements.is_empty() {
                    let last = elements.pop().unwrap();
                    (Some(last), elements)
                } else {
                    (None, elements)
                };

            let element_available_space = size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(item_height),
            );

            // 定位drifting元素（有额外y偏移）
            if let Some(ref mut drifting_element) = drifting_element {
                let sticky_origin = base_origin
                    + point(
                        px(0.),
                        item_height * rest_elements.len() + drifting_y_offset,
                    );
                drifting_element.layout_as_root(element_available_space, window, cx);
                drifting_element.prepaint_at(sticky_origin, window, cx);
            }

            // 定位rest元素
            for (ix, element) in rest_elements.iter_mut().enumerate() {
                let sticky_origin = base_origin + point(px(0.), item_height * ix);
                element.layout_as_root(element_available_space, window, cx);
                element.prepaint_at(sticky_origin, window, cx);
            }

            StickyItemsElement {
                drifting_element,
                rest_elements,
            }
            .into_any()
        } else {
            StickyItemsElement {
                drifting_element: None,
                rest_elements: SmallVec::new(),
            }
            .into_any()
        }
    }
}

impl StickyFolderDecoration {
    fn find_sticky_anchor_from_entries(
        entries: &[(FileEntry, usize)],
        visible_range_start: usize,
    ) -> Option<StickyAnchor> {
        let mut iter = entries.iter().enumerate().peekable();
        while let Some((ix, (_entry, depth))) = iter.next() {
            // 情况1: depth < ix → 该项的缩进层级小于它在可见区域的偏移
            if *depth < ix {
                return Some(StickyAnchor {
                    candidate: StickyFolderCandidate {
                        global_index: visible_range_start + ix,
                        depth: *depth,
                    },
                    index: visible_range_start + ix,
                    drifting: false,
                });
            }

            // 情况2: 检查下一项是否"外缩"
            if let Some(&(_next_ix, (_next_entry, next_depth))) = iter.peek() {
                let next_item_outdented = *next_depth + 1 == *depth;
                let depth_same_as_index = *depth == ix;
                let depth_greater_than_index = *depth == ix + 1;

                if next_item_outdented && (depth_same_as_index || depth_greater_than_index) {
                    return Some(StickyAnchor {
                        candidate: StickyFolderCandidate {
                            global_index: visible_range_start + ix,
                            depth: *depth,
                        },
                        index: visible_range_start + ix,
                        drifting: depth_greater_than_index,
                    });
                }
            }
        }
        None
    }
}

struct StickyItemsElement {
    drifting_element: Option<AnyElement>,
    rest_elements: SmallVec<[AnyElement; 8]>,
}

impl IntoElement for StickyItemsElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for StickyItemsElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(gpui::Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 先画drifting元素
        if let Some(ref mut drifting_element) = self.drifting_element {
            drifting_element.paint(window, cx);
        }
        // 再画rest元素
        for item in self.rest_elements.iter_mut().rev() {
            item.paint(window, cx);
        }
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
        let root_expanded = self.root_expanded;

        let rows_element: gpui::AnyElement = if !root_expanded {
            div()
                .id("sidebar-file-tree")
                .w_full()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_root_row(cx))
                .into_any_element()
        } else if is_directory_loaded && flat_count == 0 {
            div()
                .id("sidebar-file-tree")
                .w_full()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_root_row(cx))
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
            let scroll_handle = self.scroll_handle.clone();

            let list = uniform_list(
                "sidebar-file-tree-list",
                flat_count,
                cx.processor(|this, range: Range<usize>, _window, _cx| {
                    let entries: Vec<(FileEntry, usize)> = if let Some(flat) = &this.flat_cache {
                        range.filter_map(|ix| flat.get(ix).cloned()).collect()
                    } else {
                        Vec::new()
                    };
                    entries
                        .iter()
                        .map(|(entry, depth)| this.render_tree_row(entry, depth + 1, _cx))
                        .collect()
                }),
            )
            .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
            .track_scroll(&scroll_handle)
            .with_decoration(StickyFolderDecoration {
                entity: cx.entity(),
            })
            .flex_1()
            .min_h_0();

            div()
                .id("sidebar-file-tree")
                .w_full()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_root_row(cx))
                .child(list)
                .into_any_element()
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

/// 文件树侧边栏内的子面板
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileTreePanel {
    Files,
    Search,
    Git,
}

/// 文件树面板标签页状态（current/prev）
pub struct FileTreePanelTabs {
    pub current: FileTreePanel,
    pub prev: FileTreePanel,
    pub anim_epoch: u32,
}

impl FileTreePanelTabs {
    pub fn new() -> Self {
        Self {
            current: FileTreePanel::Files,
            prev: FileTreePanel::Files,
            anim_epoch: 0,
        }
    }

    pub fn select(&mut self, panel: FileTreePanel) {
        if self.current != panel {
            self.prev = std::mem::replace(&mut self.current, panel);
            self.anim_epoch = self.anim_epoch.wrapping_add(1);
        }
    }
}
