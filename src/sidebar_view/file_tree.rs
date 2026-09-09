use gpui::prelude::*;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Context, FocusHandle, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseButton, ParentElement,
    Pixels, Point, Render, Stateful, Styled, Subscription, Task, UniformListDecoration,
    UniformListScrollHandle, Window, div, point, px, size, svg, uniform_list,
};
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::assets::Icon;
use crate::assets::file_icon_or_default;
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
/// watcher 事件触发目录刷新的防抖间隔，避免事件风暴（如 npm install / git checkout）
const REFRESH_DEBOUNCE_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone)]
struct StickyFolderCandidate {
    global_index: usize,
    depth: usize,
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
    /// root 切换代际，用于丢弃过期的异步结果（如 home_dir 解析）
    root_epoch: u32,
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
    /// 防抖中的目录刷新任务（按路径去重）
    refresh_debounce: HashMap<PathBuf, Task<()>>,
    /// P1 预取任务（hover 预取，去重）
    prefetch_tasks: HashMap<PathBuf, Task<()>>,
    resolving_home: bool,
}

impl SidebarFileTree {
    pub fn new(
        cx: &mut Context<Self>,
        backend: Arc<dyn FileSystem>,
        root_path: Option<PathBuf>,
    ) -> Self {
        let resolving_home = root_path.is_none() && !backend.is_local();
        let is_local = backend.is_local();
        // 本地无 cwd：退到用户主目录，避免对 "/" 建立递归监听；
        // 远程无 cwd：先用 "/" 占位，异步解析 home 后再切换
        let root_path = root_path.unwrap_or_else(|| {
            if is_local {
                backend.home_dir().unwrap_or_else(|| PathBuf::from("/"))
            } else {
                PathBuf::from("/")
            }
        });

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
            root_epoch: 0,
            selected_path: None,
            loader: FileTreeLoader::new(backend),
            expanded_folders: HashSet::new(),
            root_expanded: true,
            hovered_path: None,
            flat_cache: None,
            watcher_subscription,
            watched_paths: HashSet::new(),
            scroll_handle: UniformListScrollHandle::new(),
            refresh_debounce: HashMap::new(),
            prefetch_tasks: HashMap::new(),
            resolving_home,
        };

        if is_local {
            cx.watch_recursive_path(&root_path);
        } else {
            cx.watch_remote_path(&root_path, this.loader.backend().clone());
            this.watched_paths.insert(root_path.clone());
        }

        // 根路径已确定时立即加载；resolving_home 时等待 home 解析结果
        if !resolving_home {
            let root = this.root_path.clone();
            this.load_directory(root, cx);
        }

        // 从共享缓存恢复之前的展开状态（仅保留当前根下的路径）
        this.restore_expanded_state(cx);

        if resolving_home {
            let backend = this.loader.backend().clone();
            let weak = cx.weak_entity();
            let epoch = this.root_epoch;
            cx.spawn(async move |_, cx| {
                let home = cx
                    .background_executor()
                    .spawn(async move { backend.home_dir() })
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    this.resolving_home = false;
                    // 期间 root 已被显式切换（如终端 cwd 同步）→ 丢弃过期结果
                    if this.root_epoch != epoch {
                        return;
                    }
                    let home = home.unwrap_or_else(|| PathBuf::from("/"));
                    if home == this.root_path {
                        let root = this.root_path.clone();
                        this.load_directory(root, cx);
                        cx.notify();
                    } else {
                        this.open_path(Some(home), cx);
                    }
                });
            })
            .detach();
        }

        this
    }

    pub fn open_path(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.resolving_home && path.is_none() {
            return;
        }
        let old_root = self.root_path.clone();

        if let Some(path) = path {
            if path == self.root_path {
                // 终端已给出明确 cwd：取消进行中的 home 解析并确保根目录已加载
                if self.resolving_home {
                    self.resolving_home = false;
                    self.root_epoch = self.root_epoch.wrapping_add(1);
                    let root = self.root_path.clone();
                    self.load_directory(root, cx);
                    cx.notify();
                }
                return;
            }
            self.root_path = path;
            self.root_epoch = self.root_epoch.wrapping_add(1);
            self.resolving_home = false;
            cx.emit(FileTreeEvent::RootChanged);
        }

        if self.loader.is_local() {
            cx.unwatch_recursive_path(&old_root);
        } else {
            let backend = self.loader.backend();
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched, backend);
            }
            self.watched_paths.clear();
        }

        // 切走前保存展开状态，便于切回时恢复；
        // 不清空 directory_cache，保留已加载的目录数据以加速切换
        self.save_expanded_state();
        self.expanded_folders.clear();
        self.flat_cache = None;
        self.selected_path = None;
        self.hovered_path = None;
        self.loader.clear_pending_refresh();
        self.refresh_debounce.clear();
        self.prefetch_tasks.clear();
        self.scroll_handle = UniformListScrollHandle::new();
        let root = self.root_path.clone();
        self.load_directory(root, cx);
        // 恢复新根下的展开状态
        self.restore_expanded_state(cx);

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

    /// Terminal `Cmd+Click` reveal: expand ancestor chain + select + scroll
    /// into view **without** switching `root_path`.
    ///
    /// Returns `false` when `path` is outside the current root (the file-tree
    /// root always equals the terminal cwd, so callers keep it as-is; files
    /// outside the root still open in the editor).
    /// Missing intermediate directories are loaded asynchronously; the
    /// selection lands once their listings arrive (via the normal
    /// `load_directory` completion → `flat_cache` rebuild).
    pub fn reveal(&mut self, path: PathBuf, cx: &mut Context<Self>) -> bool {
        if !path.starts_with(&self.root_path) {
            return false;
        }
        // Expand every ancestor from root down to the parent.
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut cur = path.clone();
        // Kind of the target as far as the loader knows (`None` = the parent
        // listing hasn't arrived yet).
        let target_kind = self
            .loader
            .get_cached(path.parent().unwrap_or(&self.root_path))
            .and_then(|entries| entries.iter().find(|e| e.path == path))
            .map(|e| e.kind.clone());
        loop {
            let Some(parent) = cur.parent().map(Path::to_path_buf) else {
                break;
            };
            if parent.starts_with(&self.root_path) {
                if parent != cur {
                    ancestors.push(parent.clone());
                }
                if parent == self.root_path {
                    break;
                }
                cur = parent;
            } else {
                break;
            }
        }
        ancestors.reverse();
        // Expand the target itself only when it is known to be a directory,
        // or when its kind is still unknown (parent listing in flight):
        // clicking an `ls` directory then expands as soon as it is listed.
        // Known files (`Makefile`, `LICENSE`, …) are never expanded, so no
        // doomed `load_directory` warn-spam and no stale `expanded_folders`.
        if target_kind == Some(FileEntryKind::Directory) || target_kind.is_none() {
            if path != self.root_path {
                ancestors.push(path.clone());
            }
        }
        let mut newly_expanded = false;
        for dir in ancestors {
            if dir == self.root_path {
                if !self.root_expanded {
                    self.root_expanded = true;
                    newly_expanded = true;
                }
                if !self.loader.has_cached(&dir) && !self.loader.is_loading(&dir) {
                    self.load_directory(dir, cx);
                }
                continue;
            }
            if !self.expanded_folders.contains(&dir) {
                self.expanded_folders.insert(dir.clone());
                newly_expanded = true;
                if !self.loader.has_cached(&dir) && !self.loader.is_loading(&dir) {
                    self.load_directory(dir.clone(), cx);
                }
                if !self.loader.is_local() && self.watched_paths.insert(dir.clone()) {
                    cx.watch_remote_path(&dir, self.loader.backend().clone());
                }
            }
        }
        self.selected_path = Some(path.clone());
        if newly_expanded {
            self.invalidate_flat_cache();
        }
        // Scroll to the revealed row when the flat list already contains it.
        // When ancestors are still loading, the row appears after their
        // completion; selection is already set so the next render highlights
        // it, and a follow-up `reveal` (or watcher refresh) scrolls to it.
        let flat: Vec<(FileEntry, usize)> = if let Some(flat) = self.flat_cache.clone() {
            flat
        } else {
            let mut flat = Vec::new();
            self.build_flat_list(&self.root_path.clone(), 0, &mut flat);
            self.flat_cache = Some(flat.clone());
            flat
        };
        if let Some(ix) = flat.iter().position(|(e, _)| e.path == path) {
            self.scroll_handle
                .scroll_to_item(ix, gpui::ScrollStrategy::Center);
        }
        cx.notify();
        true
    }

    fn handle_fs_event(&mut self, event: &FsChangeEvent, cx: &mut Context<Self>) {
        // 事件按来源后端过滤：多台服务器可能 watch 同一路径（两个终端 tab 连
        // 不同主机、cwd 都是 /root），只应用来自自己后端的事件，否则 A 服务器的
        // 目录项会污染 B 服务器的文件树。（本地后端无 ssh_config，只匹配
        // server=None 的本地事件。）
        if !event.is_from_backend(self.loader.backend().ssh_config().as_ref()) {
            return;
        }

        if !event.path.starts_with(&self.root_path) {
            return;
        }

        // 委托给 loader 处理缓存更新
        let affected = self.loader.handle_fs_event(&self.root_path, event);

        // Remote watcher 直接提供了新 entries → loader 已直接更新本地缓存
        if event.entries.is_some()
            && (self.loader.has_cached(&event.path) || self.loader.is_loading(&event.path))
        {
            self.invalidate_flat_cache();
            cx.notify();
            return;
        }

        // Remove 事件
        if event.kind == FsChangeKind::Remove {
            let removed_path = &event.path;
            self.expanded_folders
                .retain(|p| !p.starts_with(removed_path));
            if self
                .selected_path
                .as_ref()
                .is_some_and(|p| p.starts_with(removed_path))
            {
                self.selected_path = None;
            }
            if self
                .hovered_path
                .as_ref()
                .is_some_and(|p| p.starts_with(removed_path))
            {
                self.hovered_path = None;
            }
            // 远程：停掉被删目录（及其子目录）的轮询，避免持续轮询不存在的路径。
            // 根路径保留监听，以便目录重建时能收到恢复事件。
            if !self.loader.is_local() {
                let to_unwatch: Vec<PathBuf> = self
                    .watched_paths
                    .iter()
                    .filter(|p| **p != self.root_path && p.starts_with(removed_path))
                    .cloned()
                    .collect();
                let backend = self.loader.backend();
                for p in to_unwatch {
                    cx.unwatch_remote_path(&p, backend);
                    self.watched_paths.remove(&p);
                }
            }
            if affected.is_some() || self.loader.has_cached(&self.root_path) {
                self.invalidate_flat_cache();
                cx.notify();
            }
            if let Some(ancestor) = affected {
                self.schedule_refresh(ancestor, cx);
            }
            return;
        }

        // Create/Modify 事件
        if let Some(ancestor) = affected {
            self.schedule_refresh(ancestor, cx);
        }
    }

    /// 防抖地刷新目录：短时间内的大量 watcher 事件只触发一次真实读取。
    fn schedule_refresh(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.refresh_debounce.contains_key(&path) {
            return;
        }
        let key = path.clone();
        let weak = cx.weak_entity();
        let task = cx.spawn(async move |_, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE_DELAY).await;
            let _ = weak.update(cx, |this, cx| {
                this.refresh_debounce.remove(&path);
                this.refresh_directory(path, cx);
            });
        });
        self.refresh_debounce.insert(key, task);
    }

    /// 停止使用该文件树：保存展开状态、释放 watcher 与订阅。
    /// 共享缓存引用由 loader 的 `Drop` 归还，归零后缓存进入保温期。
    pub fn cleanup(&mut self, cx: &mut Context<Self>) {
        // 保存展开状态到共享缓存，以便重新打开时恢复
        self.save_expanded_state();
        if self.loader.is_local() {
            cx.unwatch_recursive_path(&self.root_path);
        } else {
            let backend = self.loader.backend();
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched, backend);
            }
            self.watched_paths.clear();
        }
        self.watcher_subscription.take();
        self.loader.clear_pending_refresh();
        self.refresh_debounce.clear();
        self.prefetch_tasks.clear();
    }

    /// 保存展开状态到共享缓存
    fn save_expanded_state(&self) {
        if let Some(config) = self.loader.ssh_config() {
            RemoteFileTreeCache::global()
                .lock()
                .save_expanded(config, self.expanded_folders.clone());
        }
    }

    /// 从共享缓存恢复展开状态（仅保留位于当前根下的路径），
    /// 并为这些目录触发加载与监听
    fn restore_expanded_state(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.loader.ssh_config().cloned() else {
            return;
        };
        let cached = RemoteFileTreeCache::global().lock().load_expanded(&config);
        if cached.is_empty() {
            return;
        }
        let is_local = self.loader.is_local();
        let root = self.root_path.clone();
        for path in cached {
            // 只恢复当前根下的路径，避免为无关目录建立远程轮询
            if !path.starts_with(&root) {
                continue;
            }
            self.expanded_folders.insert(path.clone());
            self.load_directory(path.clone(), cx);
            if !is_local && self.watched_paths.insert(path.clone()) {
                cx.watch_remote_path(&path, self.loader.backend().clone());
            }
        }
    }

    pub fn is_local(&self) -> bool {
        self.loader.is_local()
    }

    pub fn ssh_config(&self) -> Option<&SshConnectionConfig> {
        self.loader.ssh_config()
    }

    pub fn filesystem(&self) -> &Arc<dyn FileSystem> {
        self.loader.backend()
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

        // 先查共享缓存，命中则立即显示 + 按 TTL 决定是否后台刷新
        if let Some(entries) = self.loader.get_shared_cache(&path) {
            let is_fresh = self.loader.is_shared_cache_fresh(&path);
            self.loader.finish_load(&path, entries);
            self.invalidate_flat_cache();
            cx.notify();
            if !is_fresh {
                self.spawn_background_refresh(path, cx);
            }
            return;
        }

        let task = self.spawn_read_dir(&path, cx);
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
                            // P1: 目录加载完成后预取前3个小文件
                            this.prefetch_dir_first_files(&path_for_loading, cx);
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
        // 委托给 loader；若正在加载则标记 pending，加载完成后补一次刷新
        if !self.loader.request_refresh(&path) {
            return;
        }
        let task = self.spawn_read_dir(&path, cx);
        let weak = cx.weak_entity();
        let path_for_refresh = path.clone();

        cx.spawn(async move |_, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                // If a watcher event already removed this path from loading_paths and inserted
                // fresher entries, skip writing potentially stale results.
                if this.loader.is_loading(&path_for_refresh) {
                    match result {
                        Ok(entries) => {
                            // finish_refresh 清除加载标记；内容有变化时更新缓存
                            if this
                                .loader
                                .finish_refresh(&path_for_refresh, entries.clone())
                            {
                                // 更新共享缓存
                                this.loader.set_shared_cache(&path_for_refresh, entries);
                                this.invalidate_flat_cache();
                                cx.notify();
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to refresh directory {}: {}",
                                path_for_refresh.display(),
                                e
                            );
                            if this.loader.fail_refresh(&path_for_refresh) {
                                // 从共享缓存中也移除
                                this.loader.remove_shared_cache(&path_for_refresh);
                                this.invalidate_flat_cache();
                                cx.notify();
                                // The path is gone; refresh the parent so it disappears from the
                                // parent's listing too.
                                if let Some(parent) = path_for_refresh.parent() {
                                    if this.loader.has_cached(parent)
                                        || this.loader.is_loading(parent)
                                    {
                                        this.schedule_refresh(parent.to_path_buf(), cx);
                                    }
                                }
                            }
                        }
                    }
                }
                // If a refresh was requested while this directory was loading, run it now so we
                // don't drop events that arrived during the load.
                if this.loader.take_pending_refresh(&path_for_refresh) {
                    this.refresh_directory(path_for_refresh, cx);
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
    ) -> Task<Result<Vec<FileEntry>, String>> {
        let path = path.to_path_buf();
        let backend = self.loader.backend().clone();
        cx.background_executor()
            .spawn(async move { backend.read_dir(&path) })
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
            // 清除 pending refresh 与防抖任务
            self.loader.clear_pending_refresh_for(&path);
            self.refresh_debounce.retain(|p, _| !p.starts_with(&path));
            if !self.loader.is_local() {
                // Unwatch this folder and any nested folders that are still being watched.
                let to_unwatch: Vec<PathBuf> = self
                    .watched_paths
                    .iter()
                    .filter(|p| p.starts_with(&path))
                    .cloned()
                    .collect();
                let backend = self.loader.backend();
                for p in to_unwatch {
                    cx.unwatch_remote_path(&p, backend);
                    self.watched_paths.remove(&p);
                }
            }
        } else {
            self.expanded_folders.insert(path.clone());
            self.refresh_debounce.remove(&path);
            if self.loader.has_cached(&path) {
                if !self.loader.is_shared_cache_fresh(&path) {
                    self.spawn_background_refresh(path.clone(), cx);
                }
            } else {
                self.load_directory(path.clone(), cx);
            }
            if !self.loader.is_local() && self.watched_paths.insert(path.clone()) {
                cx.watch_remote_path(&path, self.loader.backend().clone());
            }
            self.prefetch_dir_first_files(&path, cx);
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
            self.hovered_path = Some(path.clone());
            // P1 预取：悬停文件 80ms 后后台拉取 <128K 文本，二次点击秒开
            self.schedule_prefetch(path, cx);
        } else if self.hovered_path.as_ref() == Some(&path) {
            self.hovered_path = None;
        }
        cx.notify();
    }

    /// Prefetch a hovered file if it is a small remote file not yet cached.
    fn schedule_prefetch(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.loader.is_local() || self.prefetch_tasks.contains_key(&path) {
            return;
        }
        let Some(ssh_cfg) = self.loader.ssh_config().cloned() else {
            return;
        };
        if RemoteFileTreeCache::global()
            .lock()
            .is_file_fresh(&ssh_cfg, &path)
        {
            return;
        }
        if self.is_large_or_non_file(&path) {
            return;
        }
        self.spawn_prefetch_task(path, ssh_cfg, Duration::from_millis(80), true, cx);
    }

    /// Prefetch the first few small files in a newly loaded directory.
    fn prefetch_dir_first_files(&mut self, dir: &Path, cx: &mut Context<Self>) {
        if self.loader.is_local() {
            return;
        }
        let Some(entries) = self.loader.get_cached(dir).cloned() else {
            return;
        };
        let Some(ssh_cfg) = self.loader.ssh_config().cloned() else {
            return;
        };
        let mut spawned = 0;
        for entry in entries.iter().filter(|e| e.kind == FileEntryKind::File) {
            if spawned >= 3 {
                break;
            }
            if entry.size > 128 * 1024 {
                continue;
            }
            let path = &entry.path;
            if self.prefetch_tasks.contains_key(path) {
                continue;
            }
            let cache = RemoteFileTreeCache::global().lock();
            if cache.is_file_fresh(&ssh_cfg, path)
                || cache.get_file_content(&ssh_cfg, path).is_some()
            {
                continue;
            }
            drop(cache);
            self.spawn_prefetch_task(
                path.clone(),
                ssh_cfg.clone(),
                Duration::from_millis(150),
                false,
                cx,
            );
            spawned += 1;
        }
    }

    fn is_large_or_non_file(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        let Some(entries) = self.loader.get_cached(parent) else {
            return false;
        };
        entries
            .iter()
            .find(|e| e.path == path)
            .is_some_and(|e| e.kind != FileEntryKind::File || e.size > 128 * 1024)
    }

    fn spawn_prefetch_task(
        &mut self,
        path: PathBuf,
        ssh_cfg: SshConnectionConfig,
        delay: Duration,
        check_hover: bool,
        cx: &mut Context<Self>,
    ) {
        let backend = self.loader.backend().clone();
        let weak = cx.weak_entity();
        let task_path = path.clone();
        let task_cfg = ssh_cfg.clone();
        let task = cx.spawn(async move |_, cx| {
            cx.background_executor().timer(delay).await;
            if check_hover {
                let still_hovered = weak
                    .update(cx, |this, _| this.hovered_path.as_ref() == Some(&task_path))
                    .unwrap_or(false);
                if !still_hovered {
                    let _ = weak.update(cx, |this, _| {
                        this.prefetch_tasks.remove(&task_path);
                    });
                    return;
                }
            }
            let result = cx
                .background_executor()
                .spawn({
                    let backend = backend.clone();
                    let path = task_path.clone();
                    // 关键性能：用 `read_preview_bytes(128KB)` 替代全量读，
                    // 先 metadata 预检，超限直接跳过，避免大文件浪费 IO/RTT。
                    async move { backend.read_preview_bytes(&path, 128 * 1024) }
                })
                .await;
            if let Ok(Some(bytes)) = result {
                if bytes.len() <= 128 * 1024 && !crate::ssh::sftp_fs::is_binary_bytes(&bytes) {
                    let decoded = crate::editor::encoding::decode_bytes(&bytes);
                    RemoteFileTreeCache::global().lock().insert_file_content(
                        &task_cfg,
                        &task_path,
                        decoded.text,
                        decoded.encoding,
                        // hover/目录预取只进内存缓存，不落盘（隐私边界）：
                        // 此前鼠标划过服务器上的 .env / 私钥即明文持久化
                        // 到 sftp_cache.json 且无过期机制。
                        false,
                    );
                }
            }
            let _ = weak.update(cx, |this, _| {
                this.prefetch_tasks.remove(&task_path);
            });
        });
        self.prefetch_tasks.insert(path, task);
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
        let Some(flat) = self.flat_cache.as_ref() else {
            return SmallVec::new();
        };
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
                                this.loader.ssh_config().map(|c| Box::new(c.clone()))
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

crate::impl_focusable!(SidebarFileTree);

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

    crate::impl_source_location_none!();

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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FileTreePanel {
    #[default]
    Files,
    Search,
    Git,
}

/// 文件树面板标签页状态（current/prev）
#[derive(Default)]
pub struct FileTreePanelTabs {
    pub current: FileTreePanel,
    pub prev: FileTreePanel,
    pub anim_epoch: u32,
}

impl FileTreePanelTabs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn select(&mut self, panel: FileTreePanel) {
        if self.current != panel {
            self.prev = std::mem::replace(&mut self.current, panel);
            self.anim_epoch = self.anim_epoch.wrapping_add(1);
        }
    }
}
