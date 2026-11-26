use gpui::prelude::*;
use gpui::{
    AnimationExt, AnyElement, App, AvailableSpace, Bounds, Context, FocusHandle, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseButton, ParentElement,
    Pixels, Point, Render, Styled, Subscription, Task, UniformListDecoration,
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
use crate::components::tooltip::Tooltip;
use crate::editor::git_status::FileStatus;
use crate::file_manager::FileSystem;
use crate::file_manager::file_tree_cache::RemoteFileTreeCache;
use crate::file_manager::file_tree_event::FileTreeEvent;
use crate::file_manager::file_tree_loader::FileTreeLoader;
use crate::file_manager::types::{FileEntry, FileEntryKind};
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;
use crate::theme::UiTheme;

use super::file_tree_context_menu::{DraggedTreeEntry, TreeEditMode, TreeEditing};

impl gpui::EventEmitter<crate::file_manager::file_tree_event::FileTreeEvent> for SidebarFileTree {}

fn entry_icon(entry: &FileEntry) -> Icon {
    if entry.is_dir() {
        Icon::Folder
    } else {
        file_icon_or_default(&entry.path)
    }
}

fn aggregate_git_directory_status(
    statuses: impl IntoIterator<Item = FileStatus>,
) -> Option<FileStatus> {
    statuses.into_iter().max_by_key(|status| {
        if status.is_conflicted() {
            2
        } else if status.is_created() {
            1
        } else {
            0
        }
    })
}

fn git_status_color(status: FileStatus, theme: &UiTheme) -> gpui::Hsla {
    if status.is_conflicted() {
        theme.status_conflict
    } else if status.is_created() {
        theme.status_added
    } else {
        theme.git_modified
    }
}

pub(crate) const LIST_ROW_HEIGHT: f32 = 24.0;
pub(crate) const INDENT_SIZE: f32 = 14.0;
/// watcher 事件触发目录刷新的防抖间隔，避免事件风暴（如 npm install / git checkout）
const REFRESH_DEBOUNCE_DELAY: Duration = Duration::from_millis(300);
/// 初次加载/可见目录加载失败后的重试基数，按指数退避到上限。
const LOAD_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const LOAD_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

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

/// 等待父目录异步加载的定位请求；新请求直接替换旧请求，保证最后一次定位生效。
#[derive(Clone, Debug)]
pub(super) struct PendingReveal {
    pub(super) path: PathBuf,
    pub(super) root_epoch: u32,
}

#[derive(Clone)]
struct RootViewState {
    selected_path: Option<PathBuf>,
    selected_paths: HashSet<PathBuf>,
    selection_anchor: Option<PathBuf>,
    vertical_offset: Point<Pixels>,
    horizontal_offset: Point<Pixels>,
}

#[derive(Clone, Copy, Debug)]
enum TreeTool {
    NewFile,
    NewFolder,
    Refresh,
    CollapseAll,
    LocateActive,
}

/// git 状态快照（文件树着色用）：仓库根 + 仓库相对路径 → 状态。
#[derive(Clone, Default)]
pub struct GitStatusSnapshot {
    pub repo_root: Option<String>,
    pub by_path: Arc<HashMap<String, crate::editor::git_status::FileStatus>>,
}

/// 侧边栏文件树组件 - 单列模式，类似 VSCode 文件树
///
/// View 层：只负责渲染和用户交互，所有文件操作委托给 `FileTreeLoader`。
pub struct SidebarFileTree {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) root_path: PathBuf,
    /// root 切换代际，用于丢弃过期的异步结果（如 home_dir 解析）
    pub(crate) root_epoch: u32,
    /// 主选中项（键盘焦点/最近点击项）；多选集合见 selected_paths。
    pub(crate) selected_path: Option<PathBuf>,
    pub(crate) selected_paths: HashSet<PathBuf>,
    pub(super) selection_anchor: Option<PathBuf>,
    typed: Option<(String, std::time::Instant)>,
    pub(super) pending_reveal: Option<PendingReveal>,
    /// 所有文件操作都委托给 loader
    pub(crate) loader: FileTreeLoader,
    pub(crate) expanded_folders: HashSet<PathBuf>,
    pub(crate) root_expanded: bool,
    pub(crate) hovered_path: Option<PathBuf>,
    pub(crate) flat_cache: Option<Vec<(FileEntry, usize)>>,
    /// 合并行最深层路径 -> 该行起始目录，供选择/reveal 使用。
    compact_roots: HashMap<PathBuf, PathBuf>,
    pub(super) compact_folders: bool,
    expansion_epoch: u64,
    /// git 变更文件着色快照（由 GitPanel 刷新后推送）。
    pub(crate) git_status: GitStatusSnapshot,
    watcher_subscription: Option<Subscription>,
    pub(crate) watched_paths: HashSet<PathBuf>,
    scroll_handle: UniformListScrollHandle,
    horizontal_scroll_handle: gpui::ScrollHandle,
    tree_content_width: f32,
    extra_roots: Vec<PathBuf>,
    collapsed_extra_roots: HashSet<PathBuf>,
    root_view_states: HashMap<PathBuf, RootViewState>,
    /// 防抖中的目录刷新任务（按路径去重）
    refresh_debounce: HashMap<PathBuf, Task<()>>,
    /// 加载失败后的退避重试任务（按路径去重）
    load_retry_tasks: HashMap<PathBuf, Task<()>>,
    /// 连续加载失败次数（指数退避用；成功一次即清零）
    load_failures: HashMap<PathBuf, u32>,
    /// P1 预取任务（hover 预取，去重）
    prefetch_tasks: HashMap<PathBuf, Task<()>>,
    resolving_home: bool,
    /// 行内编辑状态（新建 / 重命名）：同一时刻最多一行处于编辑中
    pub(super) editing: Option<TreeEditing>,
    /// 右键菜单（手动模式：行 / 根 / 空白区各构建不同菜单，避免嵌套双开）
    pub(super) context_menu: Option<gpui::Entity<crate::components::menu::PopupMenu>>,
    pub(super) context_menu_position: Option<gpui::Point<gpui::Pixels>>,
    pub(super) context_menu_subscription: Option<Subscription>,
    /// 打开菜单时的窗口句柄：菜单结束后要把焦点还回来时用。
    pub(super) context_menu_window: Option<gpui::AnyWindowHandle>,
    visible_range: Range<usize>,
    drag_hover: Option<(PathBuf, std::time::Instant)>,
    drag_expand_task: Option<Task<()>>,
}

impl SidebarFileTree {
    pub fn new(
        cx: &mut Context<Self>,
        backend: Arc<dyn FileSystem>,
        root_path: Option<PathBuf>,
    ) -> Self {
        Self::new_inner(cx, backend, root_path, true)
    }

    #[cfg(test)]
    fn new_without_io(
        cx: &mut Context<Self>,
        backend: Arc<dyn FileSystem>,
        root_path: PathBuf,
    ) -> Self {
        Self::new_inner(cx, backend, Some(root_path), false)
    }

    fn new_inner(
        cx: &mut Context<Self>,
        backend: Arc<dyn FileSystem>,
        root_path: Option<PathBuf>,
        start_io: bool,
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

        let watcher_subscription = if !start_io {
            None
        } else if is_local {
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
            selected_paths: HashSet::new(),
            selection_anchor: None,
            typed: None,
            pending_reveal: None,
            loader: FileTreeLoader::new(backend),
            expanded_folders: HashSet::new(),
            root_expanded: true,
            hovered_path: None,
            flat_cache: None,
            compact_roots: HashMap::new(),
            compact_folders: true,
            expansion_epoch: 0,
            git_status: GitStatusSnapshot::default(),
            watcher_subscription,
            watched_paths: HashSet::new(),
            scroll_handle: UniformListScrollHandle::new(),
            horizontal_scroll_handle: gpui::ScrollHandle::new(),
            tree_content_width: 240.0,
            extra_roots: Vec::new(),
            collapsed_extra_roots: HashSet::new(),
            root_view_states: HashMap::new(),
            refresh_debounce: HashMap::new(),
            load_retry_tasks: HashMap::new(),
            load_failures: HashMap::new(),
            prefetch_tasks: HashMap::new(),
            resolving_home,
            editing: None,
            context_menu: None,
            context_menu_position: None,
            context_menu_subscription: None,
            context_menu_window: None,
            visible_range: 0..0,
            drag_hover: None,
            drag_expand_task: None,
        };

        if !start_io {
            // 纯状态测试不启动 watcher、目录读取、重试或预取。
        } else if is_local {
            cx.watch_recursive_path(&root_path);
        } else {
            cx.watch_remote_path(&root_path, this.loader.backend().clone());
            this.watched_paths.insert(root_path.clone());
        }

        // 根路径已确定时立即加载；resolving_home 时等待 home 解析结果
        if start_io && !resolving_home {
            let root = this.root_path.clone();
            this.load_directory(root, cx);
        }

        if start_io {
            this.restore_expanded_state(cx);
        }

        if start_io && resolving_home {
            let backend = this.loader.backend().clone();
            let weak = cx.weak_entity();
            let epoch = this.root_epoch;
            cx.spawn(async move |_, cx| {
                // home_dir 对远程后端是一次同步 SFTP RPC；放进 smol 的阻塞
                // 线程池，避免占住 GPUI 的共享后台 worker（见 spawn_read_dir）。
                let home = cx
                    .background_executor()
                    .spawn(async move { smol::unblock(move || backend.home_dir()).await })
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

    fn save_root_view_state(&mut self, root: &Path) {
        let vertical_offset = self.scroll_handle.0.borrow().base_handle.offset();
        self.root_view_states.insert(
            root.to_path_buf(),
            RootViewState {
                selected_path: self.selected_path.clone(),
                selected_paths: self.selected_paths.clone(),
                selection_anchor: self.selection_anchor.clone(),
                vertical_offset,
                horizontal_offset: self.horizontal_scroll_handle.offset(),
            },
        );
    }

    fn restore_root_view_state(&mut self, root: &Path) {
        let Some(state) = self.root_view_states.get(root).cloned() else {
            return;
        };
        self.selected_path = state.selected_path;
        self.selected_paths = state.selected_paths;
        self.selection_anchor = state.selection_anchor;
        self.scroll_handle
            .0
            .borrow()
            .base_handle
            .set_offset(state.vertical_offset);
        self.horizontal_scroll_handle
            .set_offset(state.horizontal_offset);
    }

    pub fn open_path(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.resolving_home && path.is_none() {
            return;
        }
        let old_root = self.root_path.clone();

        if let Some(path) = path {
            if path == self.root_path {
                // 终端已给出明确 cwd：取消进行中的 home 解析并确保根目录已加载。
                // 同一路径此前加载失败时，这里也必须重新尝试；否则复用文件树的
                // 路径（切换 tab / 终端重复上报 cwd）会把空树原样保留下来。
                if self.resolving_home {
                    self.resolving_home = false;
                    self.root_epoch = self.root_epoch.wrapping_add(1);
                }
                if !self.loader.has_cached(&self.root_path)
                    && !self.loader.is_loading(&self.root_path)
                {
                    self.clear_load_failure(&path);
                    self.load_directory(path, cx);
                }
                cx.notify();
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
        self.save_root_view_state(&old_root);
        self.expanded_folders.clear();
        self.flat_cache = None;
        self.selected_path = None;
        self.selected_paths.clear();
        self.selection_anchor = None;
        self.typed = None;
        self.pending_reveal = None;
        self.hovered_path = None;
        self.cancel_editing(cx);
        self.close_context_menu(cx);
        self.loader.clear_pending_refresh();
        self.refresh_debounce.clear();
        self.load_retry_tasks.clear();
        self.load_failures.clear();
        self.prefetch_tasks.clear();
        self.drag_hover = None;
        self.drag_expand_task = None;
        self.scroll_handle = UniformListScrollHandle::new();
        let root = self.root_path.clone();
        self.load_directory(root, cx);
        // 恢复新根下的展开状态与视图选择/滚动。
        self.restore_expanded_state(cx);
        self.restore_root_view_state(&self.root_path.clone());

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

    /// 接收 GitPanel 推送的状态快照（仓库根 + 相对路径 → 状态）。
    pub fn set_git_status(
        &mut self,
        snapshot: Option<(
            String,
            Arc<HashMap<String, crate::editor::git_status::FileStatus>>,
        )>,
        cx: &mut Context<Self>,
    ) {
        self.git_status = match snapshot {
            Some((repo_root, by_path)) => GitStatusSnapshot {
                repo_root: Some(repo_root),
                by_path,
            },
            None => GitStatusSnapshot::default(),
        };
        cx.notify();
    }

    fn git_file_status(&self, path: &Path) -> Option<crate::editor::git_status::FileStatus> {
        let root = self.git_status.repo_root.as_ref()?;
        let rel = path.strip_prefix(root).ok()?;
        let key = rel.to_string_lossy();
        self.git_status.by_path.get(key.as_ref()).copied()
    }

    fn git_directory_status(&self, path: &Path) -> Option<crate::editor::git_status::FileStatus> {
        let root = self.git_status.repo_root.as_ref()?;
        aggregate_git_directory_status(self.git_status.by_path.keys().filter_map(|relative| {
            let changed = Path::new(root).join(relative);
            changed
                .starts_with(path)
                .then(|| self.git_status.by_path[relative])
        }))
    }

    /// 文件名按 git 状态着色：新增=绿、修改/改名=主题蓝、冲突=橙红。
    /// 非 git 仓库内、快照未就绪或文件无变更时返回 None（用默认色）。
    pub(crate) fn git_name_color(&self, path: &Path, theme: &UiTheme) -> Option<gpui::Hsla> {
        let status = self.git_file_status(path)?;
        Some(git_status_color(status, theme))
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
        let workspace_root = std::iter::once(&self.root_path)
            .chain(self.extra_roots.iter())
            .find(|root| path.starts_with(root))
            .cloned();
        let Some(workspace_root) = workspace_root else {
            return false;
        };
        // Expand every ancestor from its workspace root down to the parent.
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut cur = path.clone();
        // Kind of the target as far as the loader knows (`None` = the parent
        // listing hasn't arrived yet).
        let target_kind = self
            .loader
            .get_cached(path.parent().unwrap_or(&workspace_root))
            .and_then(|entries| entries.iter().find(|e| e.path == path))
            .map(FileEntry::is_dir);
        // 向上收集祖先，直到根路径；用 `while let` 表达「有父就继续」。
        while let Some(parent) = cur.parent().map(Path::to_path_buf) {
            if !parent.starts_with(&workspace_root) {
                break;
            }
            if parent != cur {
                ancestors.push(parent.clone());
            }
            if parent == workspace_root {
                break;
            }
            cur = parent;
        }
        ancestors.reverse();
        // Expand the target itself only when it is known to be a directory,
        // or when its kind is still unknown (parent listing in flight):
        // clicking an `ls` directory then expands as soon as it is listed.
        // Known files (`Makefile`, `LICENSE`, …) are never expanded, so no
        // doomed `load_directory` warn-spam and no stale `expanded_folders`.
        if (target_kind == Some(true) || target_kind.is_none()) && path != workspace_root {
            ancestors.push(path.clone());
        }
        let mut newly_expanded = false;
        for dir in ancestors {
            if dir == workspace_root {
                if dir == self.root_path {
                    if !self.root_expanded {
                        self.root_expanded = true;
                        newly_expanded = true;
                    }
                } else {
                    newly_expanded |= self.collapsed_extra_roots.remove(&dir);
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
        self.selected_paths.clear();
        self.selected_paths.insert(path.clone());
        self.selection_anchor = Some(path.clone());
        self.pending_reveal = Some(PendingReveal {
            path,
            root_epoch: self.root_epoch,
        });
        if newly_expanded {
            self.invalidate_flat_cache();
        }
        // 已可见则立即居中；父目录仍在加载时由 settle_pending_reveal 在每次
        // 目录结果落地后继续，不依赖用户再次点击或 watcher 恰好再发事件。
        self.settle_pending_reveal();
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

        let event_root = std::iter::once(&self.root_path)
            .chain(self.extra_roots.iter())
            .find(|root| event.path.starts_with(root))
            .cloned();
        let Some(event_root) = event_root else {
            return;
        };

        // 委托给 loader 处理缓存更新（主根与附加根各自判断根删除语义）
        let affected = self.loader.handle_fs_event(&event_root, event);

        // Remote watcher 直接提供了新 entries → loader 已直接更新本地缓存
        if event.entries.is_some() && self.loader.has_cached(&event.path) {
            self.clear_load_failure(&event.path);
            self.invalidate_flat_cache();
            self.settle_pending_reveal();
            cx.notify();
            return;
        }

        // Remove 事件
        if event.kind == FsChangeKind::Remove {
            let removed_path = &event.path;
            self.expanded_folders
                .retain(|p| !p.starts_with(removed_path));
            self.selected_paths
                .retain(|path| !path.starts_with(removed_path));
            if self
                .selected_path
                .as_ref()
                .is_some_and(|p| p.starts_with(removed_path))
            {
                self.selected_path = self.selected_paths.iter().next().cloned();
            }
            if self
                .selection_anchor
                .as_ref()
                .is_some_and(|path| path.starts_with(removed_path))
            {
                self.selection_anchor = self.selected_path.clone();
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
    pub(crate) fn schedule_refresh(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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

    /// 可见目录加载失败后按指数退避重试。
    ///
    /// 只在目录仍然可见（根目录或已展开目录）时保留任务；收起、换根或成功
    /// 加载都会取消它。远程连接抖动时，watcher 的事件可能也被同一次故障吞掉，
    /// 不能把恢复完全寄托在下一次文件变化上。
    fn schedule_load_retry(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.load_retry_tasks.contains_key(&path) {
            return;
        }
        let attempt = {
            let count = self.load_failures.entry(path.clone()).or_insert(0);
            *count = count.saturating_add(1);
            *count
        };
        let exponent = attempt.saturating_sub(1).min(5);
        let delay = std::cmp::min(
            LOAD_RETRY_BASE_DELAY * 2u32.pow(exponent),
            LOAD_RETRY_MAX_DELAY,
        );
        let root = self.root_path.clone();
        let retry_path = path.clone();
        let weak = cx.weak_entity();
        let task = cx.spawn(async move |_, cx| {
            cx.background_executor().timer(delay).await;
            let _ = weak.update(cx, |this, cx| {
                this.load_retry_tasks.remove(&retry_path);
                if this.root_path != root {
                    this.load_failures.remove(&retry_path);
                    return;
                }
                let visible = retry_path == this.root_path
                    || (this.extra_roots.contains(&retry_path)
                        && !this.collapsed_extra_roots.contains(&retry_path))
                    || this.expanded_folders.contains(&retry_path);
                if !visible {
                    this.load_failures.remove(&retry_path);
                    return;
                }
                if this.loader.is_loading(&retry_path) {
                    return;
                }
                if this.loader.has_cached(&retry_path) {
                    // 刷新失败会保留旧缓存，重试必须走 refresh 而不是被
                    // `has_cached` 直接挡掉。
                    this.refresh_directory(retry_path, cx);
                } else {
                    this.load_directory(retry_path, cx);
                }
            });
        });
        self.load_retry_tasks.insert(path, task);
    }

    /// 加载/刷新成功（或收到了 watcher 的完整目录内容）后重置退避状态。
    fn clear_load_failure(&mut self, path: &Path) {
        self.load_failures.remove(path);
        self.load_retry_tasks.remove(path);
    }

    /// 停止使用该文件树：保存展开状态、释放 watcher 与订阅。
    /// 共享缓存引用由 loader 的 `Drop` 归还，归零后缓存进入保温期。
    pub fn cleanup(&mut self, cx: &mut Context<Self>) {
        // 保存展开状态到共享缓存，以便重新打开时恢复
        self.save_expanded_state();
        self.save_root_view_state(&self.root_path.clone());
        if self.loader.is_local() {
            cx.unwatch_recursive_path(&self.root_path);
            for root in &self.extra_roots {
                cx.unwatch_recursive_path(root);
            }
        } else {
            let backend = self.loader.backend();
            for watched in &self.watched_paths {
                cx.unwatch_remote_path(watched, backend);
            }
            self.watched_paths.clear();
        }
        self.watcher_subscription.take();
        self.cancel_editing(cx);
        self.close_context_menu(cx);
        self.loader.clear_pending_refresh();
        self.refresh_debounce.clear();
        self.load_retry_tasks.clear();
        self.load_failures.clear();
        self.prefetch_tasks.clear();
        self.drag_hover = None;
        self.drag_expand_task = None;
    }

    /// 按主根/附加根分别保存展开状态，避免同服务器不同项目互相覆盖。
    fn save_expanded_state(&self) {
        if let Some(config) = self.loader.ssh_config() {
            let mut cache = RemoteFileTreeCache::global().lock();
            for root in std::iter::once(&self.root_path).chain(self.extra_roots.iter()) {
                let expanded = self
                    .expanded_folders
                    .iter()
                    .filter(|path| path.starts_with(root))
                    .cloned()
                    .collect();
                cache.save_expanded(config, root, expanded);
            }
        }
    }

    /// 逐根恢复展开状态，并为这些目录触发加载与监听。
    fn restore_expanded_state(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.loader.ssh_config().cloned() else {
            return;
        };
        let roots: Vec<PathBuf> = std::iter::once(self.root_path.clone())
            .chain(self.extra_roots.iter().cloned())
            .collect();
        let cached: HashSet<PathBuf> = {
            let mut cache = RemoteFileTreeCache::global().lock();
            roots
                .iter()
                .flat_map(|root| cache.load_expanded(&config, root))
                .collect()
        };
        let is_local = self.loader.is_local();
        for path in cached {
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

    /// 文件事件中的后端标识：本地缓存的占位 local config 不能当成远程连接发出。
    pub(crate) fn event_ssh_config(&self) -> Option<Box<SshConnectionConfig>> {
        if self.loader.is_local() {
            None
        } else {
            self.loader
                .ssh_config()
                .map(|config| Box::new(config.clone()))
        }
    }

    pub fn ssh_config(&self) -> Option<&SshConnectionConfig> {
        self.loader.ssh_config()
    }

    pub fn filesystem(&self) -> &Arc<dyn FileSystem> {
        self.loader.backend()
    }

    pub fn matches_backend(&self, ssh_config: Option<&SshConnectionConfig>) -> bool {
        self.loader
            .backend()
            .matches(self.loader.is_local(), ssh_config)
    }

    pub fn matches_location(
        &self,
        path: &Path,
        is_local: bool,
        ssh_config: Option<&SshConnectionConfig>,
    ) -> bool {
        self.root_path == path && self.loader.backend().matches(is_local, ssh_config)
    }

    pub(crate) fn load_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // 委托给 loader；若已在加载中则跳过，避免重复任务
        if !self.loader.begin_load(&path) {
            return;
        }

        // 先查共享缓存，命中则立即显示 + 按 TTL 决定是否后台刷新
        if let Some(entries) = self.loader.get_shared_cache(&path) {
            let is_fresh = self.loader.is_shared_cache_fresh(&path);
            self.loader.finish_load(&path, entries);
            self.clear_load_failure(&path);
            self.invalidate_flat_cache();
            self.settle_pending_reveal();
            cx.notify();
            if !is_fresh {
                self.spawn_background_refresh(path, cx);
            }
            return;
        }

        let task = self.spawn_read_dir(&path, cx);
        let weak = cx.weak_entity();
        let path_for_loading = path.clone();
        let root_epoch = self.root_epoch;

        cx.spawn(async move |_, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                this.complete_directory_load(path_for_loading, root_epoch, result, cx);
            });
        })
        .detach();
    }

    fn path_in_workspace(&self, path: &Path) -> bool {
        std::iter::once(&self.root_path)
            .chain(self.extra_roots.iter())
            .any(|root| path.starts_with(root))
    }

    fn complete_directory_load(
        &mut self,
        path: PathBuf,
        root_epoch: u32,
        result: Result<Vec<FileEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        // 切走后旧根的结果可以丢弃；若用户已切回同一路径，它仍是当前唯一加载，
        // 不能因为 epoch 不同就 fail_load，否则会留下永不结束的“正在加载”。
        if self.root_epoch != root_epoch && !self.path_in_workspace(&path) {
            self.loader.fail_load(&path);
            return;
        }
        // If the path was removed from loading_paths by a watcher event (which already
        // inserted fresher entries), skip writing potentially stale results.
        if self.loader.is_loading(&path) {
            match result {
                Ok(entries) => {
                    self.loader.set_shared_cache(&path, entries.clone());
                    self.loader.finish_load(&path, entries);
                    self.clear_load_failure(&path);
                    self.invalidate_flat_cache();
                    self.settle_pending_reveal();
                    cx.notify();
                    self.prefetch_dir_first_files(&path, cx);
                }
                Err(error) => {
                    log::warn!("Failed to read directory {}: {error}", path.display());
                    self.loader.fail_load(&path);
                    self.schedule_load_retry(path.clone(), cx);
                    cx.notify();
                }
            }
        }
        if self.loader.take_pending_refresh(&path) {
            self.refresh_directory(path, cx);
        }
    }

    /// 传输完成后刷新受影响的目录（由 `WorkspaceEvent::TransferFinished` 驱动）。
    ///
    /// 只刷新与本树同一文件系统的目录，且限定在根之内 —— 否则本地树会因为一次
    /// 远端传输去读本地路径（或反之），既无意义又会触发无谓的远端 IO。
    pub(crate) fn refresh_affected_dirs(
        &mut self,
        dirs: &[crate::file_manager::FileEndpoint],
        cx: &mut Context<Self>,
    ) {
        let is_local = self.loader.is_local();
        let ssh_config = self.loader.ssh_config().cloned();
        for dir in dirs {
            if !dir.matches_fs(is_local, ssh_config.as_ref()) {
                continue;
            }
            let path = dir.path_buf();
            let in_workspace = std::iter::once(&self.root_path)
                .chain(self.extra_roots.iter())
                .any(|root| path.starts_with(root));
            if in_workspace {
                self.refresh_directory(path, cx);
            }
        }
    }

    pub(crate) fn refresh_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // 委托给 loader；若正在加载则标记 pending，加载完成后补一次刷新
        if !self.loader.request_refresh(&path) {
            return;
        }
        let task = self.spawn_read_dir(&path, cx);
        let weak = cx.weak_entity();
        let path_for_refresh = path.clone();
        let root_epoch = self.root_epoch;

        cx.spawn(async move |_, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                if this.root_epoch != root_epoch && !this.path_in_workspace(&path_for_refresh) {
                    this.loader.fail_load(&path_for_refresh);
                    return;
                }
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
                                this.settle_pending_reveal();
                                cx.notify();
                            }
                            this.clear_load_failure(&path_for_refresh);
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to refresh directory {}: {}",
                                path_for_refresh.display(),
                                e
                            );
                            // 刷新失败不加区分地删缓存会让瞬时网络抖动直接清空
                            // 文件树；保留旧内容，同时安排重试。
                            let had_cache = this.loader.fail_refresh(&path_for_refresh);
                            if had_cache {
                                this.invalidate_flat_cache();
                                cx.notify();
                            }
                            this.schedule_load_retry(path_for_refresh.clone(), cx);
                            // 目录确实消失时，父目录的刷新会让它从列表里消失。
                            if let Some(parent) = path_for_refresh.parent()
                                && (this.loader.has_cached(parent)
                                    || this.loader.is_loading(parent))
                            {
                                this.schedule_refresh(parent.to_path_buf(), cx);
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
        let root_epoch = self.root_epoch;
        cx.spawn(async move |_, cx| {
            let result: Result<Vec<FileEntry>, String> = cx
                .background_executor()
                .spawn({
                    let backend = backend.clone();
                    let path = path.clone();
                    async move { smol::unblock(move || backend.read_dir(&path)).await }
                })
                .await;
            if let Ok(entries) = result {
                let _ = weak.update(cx, |this, cx| {
                    if this.root_epoch != root_epoch {
                        return;
                    }
                    this.loader.set_shared_cache(&path, entries.clone());
                    this.clear_load_failure(&path);
                    if this.loader.update_cached(&path, entries) {
                        this.invalidate_flat_cache();
                    }
                    this.settle_pending_reveal();
                    cx.notify();
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
        // 远程 read_dir 内部是同步 SFTP RPC。直接放在 GPUI 的共享后台线程池里
        // 会占住 worker：远端 watcher 每 3s 会并发发起最多 8 个同样的阻塞调用，
        // 网络一慢就能把 worker 全部占满，根目录读取只能排在后面（最坏等到
        // SFTP 的 300s 超时），表现就是文件树长时间空白。smol::unblock 使用
        // 独立的阻塞线程池，等待结果时 worker 仍然可以调度其他任务。
        cx.background_executor()
            .spawn(async move { smol::unblock(move || backend.read_dir(&path)).await })
    }

    pub(crate) fn invalidate_flat_cache(&mut self) {
        self.flat_cache = None;
    }

    fn rebuild_flat_cache(&mut self) -> usize {
        self.compact_roots.clear();
        let mut flat = Vec::new();
        self.build_flat_list(&self.root_path.clone(), 0, &mut flat);
        for root in self.extra_roots.clone() {
            if !self.collapsed_extra_roots.contains(&root) {
                self.build_flat_list(&root, 0, &mut flat);
            }
        }
        self.tree_content_width = flat
            .iter()
            .map(|(entry, depth)| {
                52.0 + *depth as f32 * INDENT_SIZE
                    + unicode_width::UnicodeWidthStr::width(entry.name.as_str()) as f32 * 7.2
            })
            .fold(240.0, f32::max);
        let len = flat.len();
        self.flat_cache = Some(flat);
        len
    }

    /// 目录加载、刷新或 watcher 直接换入列表后，继续完成最新一次 reveal。
    fn settle_pending_reveal(&mut self) {
        let Some(pending) = self.pending_reveal.clone() else {
            return;
        };
        let in_workspace = std::iter::once(&self.root_path)
            .chain(self.extra_roots.iter())
            .any(|root| pending.path.starts_with(root));
        if pending.root_epoch != self.root_epoch || !in_workspace {
            self.pending_reveal = None;
            return;
        }
        if self.flat_cache.is_none() {
            self.rebuild_flat_cache();
        }
        let index = self.flat_cache.as_ref().and_then(|flat| {
            flat.iter()
                .position(|(entry, _)| entry.path == pending.path)
                .or_else(|| {
                    flat.iter().position(|(entry, _)| {
                        self.compact_roots.get(&entry.path) == Some(&pending.path)
                    })
                })
        });
        if let Some(index) = index {
            self.selected_path = Some(pending.path.clone());
            self.selected_paths.clear();
            self.selected_paths.insert(pending.path.clone());
            self.selection_anchor = Some(pending.path);
            self.scroll_handle
                .scroll_to_item(index, gpui::ScrollStrategy::Center);
            self.pending_reveal = None;
        }
    }

    fn build_flat_list(
        &mut self,
        directory: &Path,
        depth: usize,
        result: &mut Vec<(FileEntry, usize)>,
    ) {
        /// 异常远程符号链接可能让路径字符串无限增长；这是总量保险而非目录深度限制。
        const MAX_FLAT_ROWS: usize = 100_000;
        const MAX_COMPACT_SEGMENTS: usize = 256;
        enum Frame {
            Directory(PathBuf, usize),
            Entry(FileEntry, usize),
            Exit(PathBuf),
        }
        let is_local = self.loader.is_local();
        let identity = |path: &Path| {
            if is_local {
                std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
            } else {
                path.to_path_buf()
            }
        };
        let mut active_directories = HashSet::new();
        let mut stack = vec![Frame::Directory(directory.to_path_buf(), depth)];
        while let Some(frame) = stack.pop() {
            if result.len() >= MAX_FLAT_ROWS {
                log::warn!("文件树拍平达到 {MAX_FLAT_ROWS} 行保险上限，停止继续展开");
                break;
            }
            match frame {
                Frame::Exit(identity) => {
                    active_directories.remove(&identity);
                }
                Frame::Directory(directory, depth) => {
                    let identity = identity(&directory);
                    if !active_directories.insert(identity.clone()) {
                        continue;
                    }
                    stack.push(Frame::Exit(identity));
                    if let Some(editing) = &self.editing
                        && let TreeEditMode::Create {
                            parent,
                            kind,
                            proposed,
                        } = &editing.mode
                        && parent == &directory
                    {
                        result.push((
                            FileEntry {
                                name: proposed.clone(),
                                path: parent.join(proposed),
                                kind: kind.clone(),
                                size: 0,
                                symlink_target_is_dir: false,
                            },
                            depth,
                        ));
                    }
                    if let Some(entries) = self.loader.get_cached(&directory).cloned() {
                        for entry in entries.into_iter().rev() {
                            stack.push(Frame::Entry(entry, depth));
                        }
                    }
                }
                Frame::Entry(entry, depth) => {
                    let is_directory = entry.is_dir();
                    let mut displayed = entry.clone();
                    if is_directory && self.compact_folders {
                        let start = entry.path.clone();
                        let mut current = entry.path.clone();
                        let mut names = vec![entry.name.clone()];
                        let mut compact_seen = HashSet::new();
                        compact_seen.insert(identity(&current));
                        for _ in 0..MAX_COMPACT_SEGMENTS {
                            let only_child = self.loader.get_cached(&current).and_then(
                                |children| match children.as_slice() {
                                    [child] if child.is_dir() => Some(child.clone()),
                                    _ => None,
                                },
                            );
                            let Some(child) = only_child else {
                                break;
                            };
                            if !compact_seen.insert(identity(&child.path)) {
                                break;
                            }
                            names.push(child.name.clone());
                            current = child.path;
                        }
                        if current != start {
                            displayed.path = current.clone();
                            displayed.name = names.join("/");
                            self.compact_roots.insert(current, start);
                        }
                    }
                    let is_expanded =
                        is_directory && self.expanded_folders.contains(&displayed.path);
                    result.push((displayed.clone(), depth));
                    if is_expanded {
                        stack.push(Frame::Directory(displayed.path, depth + 1));
                    }
                }
            }
        }
    }

    /// 折叠全部文件夹（VSCode Collapse Folders：保留根目录展开）。
    pub(super) fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.cancel_editing(cx);
        self.expanded_folders.clear();
        self.root_expanded = true;
        self.expansion_epoch = self.expansion_epoch.wrapping_add(1);
        self.invalidate_flat_cache();
        cx.notify();
    }

    pub(crate) fn toggle_folder_expanded(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.expanded_folders.contains(&path) {
            self.expanded_folders.remove(&path);
            // 清除 pending refresh、防抖任务与失败重试
            self.loader.clear_pending_refresh_for(&path);
            self.refresh_debounce.retain(|p, _| !p.starts_with(&path));
            self.load_retry_tasks.retain(|p, _| !p.starts_with(&path));
            self.load_failures.retain(|p, _| !p.starts_with(&path));
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
            self.clear_load_failure(&path);
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
        self.expansion_epoch = self.expansion_epoch.wrapping_add(1);
        self.invalidate_flat_cache();
        cx.notify();
    }

    fn toggle_dir_recursively(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        const EXPAND_BUDGET: usize = 2_000;
        if self.expanded_folders.contains(&path) {
            self.expanded_folders
                .retain(|expanded| !expanded.starts_with(&path));
            self.invalidate_flat_cache();
            cx.notify();
            return;
        }
        let backend = self.loader.backend().clone();
        let root_epoch = self.root_epoch;
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || {
                        let mut pending = vec![path.clone()];
                        let mut listings = Vec::new();
                        let mut limited = false;
                        while let Some(dir) = pending.pop() {
                            if listings.len() >= EXPAND_BUDGET {
                                limited = true;
                                break;
                            }
                            let entries = match backend.read_dir(&dir) {
                                Ok(entries) => entries,
                                Err(error) => {
                                    log::warn!("递归展开读取失败 {}: {error}", dir.display());
                                    continue;
                                }
                            };
                            for entry in entries.iter().filter(|entry| entry.is_dir()) {
                                pending.push(entry.path.clone());
                            }
                            listings.push((dir, entries));
                        }
                        (listings, limited)
                    })
                    .await
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.root_epoch != root_epoch {
                    return;
                }
                let (listings, limited) = result;
                for (dir, entries) in listings {
                    this.loader.set_shared_cache(&dir, entries.clone());
                    this.loader.update_cached(&dir, entries);
                    this.expanded_folders.insert(dir);
                }
                if limited {
                    log::warn!("递归展开达到 {EXPAND_BUDGET} 个目录上限，已停止后续展开");
                }
                this.invalidate_flat_cache();
                this.settle_pending_reveal();
                cx.notify();
            });
        })
        .detach();
    }

    fn select_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected_path = Some(path.clone());
        self.selected_paths.clear();
        self.selected_paths.insert(path.clone());
        self.selection_anchor = Some(path);
        cx.notify();
    }

    pub(crate) fn is_selected(&self, path: &Path) -> bool {
        self.selected_paths.contains(path)
            || self
                .compact_roots
                .get(path)
                .is_some_and(|root| self.selected_paths.contains(root))
    }

    /// 点击选择：普通点击单选，Cmd/Ctrl 切换，Shift 以锚点扩展可见行范围。
    fn select_with_modifiers(
        &mut self,
        path: PathBuf,
        row_index: usize,
        modifiers: gpui::Modifiers,
    ) {
        if modifiers.shift {
            let anchor_index = self.selection_anchor.as_ref().and_then(|anchor| {
                self.flat_cache
                    .as_ref()?
                    .iter()
                    .position(|(entry, _)| entry.path == *anchor)
            });
            if let Some(anchor_index) = anchor_index {
                let range = anchor_index.min(row_index)..=anchor_index.max(row_index);
                self.selected_paths.clear();
                let rows = self.flat_cache.as_ref().and_then(|flat| {
                    // 监听器来自已绘制帧；watcher 可能已经重建 flat_cache。
                    // 旧索引必须仍指向当前点击项，否则范围选择退化为安全单选。
                    flat.get(row_index)
                        .filter(|(entry, _)| entry.path == path)
                        .and_then(|_| flat.get(range))
                });
                if let Some(rows) = rows {
                    for (entry, _) in rows {
                        self.selected_paths.insert(entry.path.clone());
                    }
                } else {
                    self.selected_paths.insert(path.clone());
                    self.selection_anchor = Some(path.clone());
                }
            } else {
                self.selected_paths.clear();
                self.selected_paths.insert(path.clone());
                self.selection_anchor = Some(path.clone());
            }
        } else if modifiers.secondary() {
            if !self.selected_paths.remove(&path) {
                self.selected_paths.insert(path.clone());
            }
            self.selection_anchor = Some(path.clone());
        } else {
            self.selected_paths.clear();
            self.selected_paths.insert(path.clone());
            self.selection_anchor = Some(path.clone());
        }
        self.selected_path = self
            .selected_paths
            .contains(&path)
            .then_some(path)
            .or_else(|| self.selected_paths.iter().next().cloned());
    }

    /// 返回批量操作应处理的根项：已选目录覆盖其后代，避免重复移动/删除。
    pub(crate) fn selected_operation_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .selected_paths
            .iter()
            .filter(|path| **path != self.root_path)
            .filter(|path| {
                !path.ancestors().skip(1).any(|ancestor| {
                    ancestor != self.root_path && self.selected_paths.contains(ancestor)
                })
            })
            .cloned()
            .collect();
        paths.sort();
        paths
    }

    fn select_row(&mut self, index: usize, extend: bool) {
        let Some((entry, _)) = self
            .flat_cache
            .as_ref()
            .and_then(|flat| flat.get(index))
            .cloned()
        else {
            return;
        };
        if extend {
            self.select_with_modifiers(
                entry.path,
                index,
                gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            );
        } else {
            self.selected_path = Some(entry.path.clone());
            self.selected_paths.clear();
            self.selected_paths.insert(entry.path.clone());
            self.selection_anchor = Some(entry.path);
        }
        self.scroll_handle
            .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
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
        if self.loader.is_local()
            || self.prefetch_tasks.len() >= 8
            || self.prefetch_tasks.contains_key(&path)
        {
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
        if entries.len() > 200 || self.prefetch_tasks.len() >= 8 {
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
                    // 同样是同步 SFTP，挪到阻塞线程池执行。
                    async move {
                        smol::unblock(move || backend.read_preview_bytes(&path, 128 * 1024)).await
                    }
                })
                .await;
            if let Ok(Some(bytes)) = result
                && bytes.len() <= 128 * 1024
                && !crate::ssh::sftp_fs::is_binary_bytes(&bytes)
            {
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
            let _ = weak.update(cx, |this, _| {
                this.prefetch_tasks.remove(&task_path);
            });
        });
        self.prefetch_tasks.insert(path, task);
    }

    pub(super) fn toggle_compact_folders(&mut self, cx: &mut Context<Self>) {
        self.compact_folders = !self.compact_folders;
        self.invalidate_flat_cache();
        cx.notify();
    }

    pub(super) fn refresh_all_loaded(&mut self, cx: &mut Context<Self>) {
        let root = self.root_path.clone();
        let mut dirs = vec![root.clone()];
        dirs.extend(
            self.loader
                .cached_paths()
                .into_iter()
                .filter(|path| path.starts_with(&root)),
        );
        dirs.sort();
        dirs.dedup();
        for dir in dirs {
            self.clear_load_failure(&dir);
            self.refresh_directory(dir, cx);
        }
    }

    fn selected_flat_entry(&self) -> Option<(usize, FileEntry, usize)> {
        let selected = self.selected_path.as_ref()?;
        self.flat_cache
            .as_ref()?
            .iter()
            .enumerate()
            .find_map(|(index, (entry, depth))| {
                (entry.path == *selected).then_some((index, entry.clone(), *depth))
            })
    }

    fn open_selected_from_keyboard(&mut self, cx: &mut Context<Self>) {
        let Some((_, entry, _)) = self.selected_flat_entry() else {
            return;
        };
        if entry.is_dir() {
            self.toggle_folder_expanded(entry.path, cx);
            return;
        }
        let selected = self.selected_operation_paths();
        let flat = self.flat_cache.clone().unwrap_or_default();
        for path in selected {
            if flat
                .iter()
                .any(|(entry, _)| entry.path == path && !entry.is_dir())
            {
                cx.emit(FileTreeEvent::OpenFile {
                    path,
                    ssh_config: self.event_ssh_config(),
                });
            }
        }
    }

    fn typeahead_select(&mut self, text: &str) {
        const RESET_AFTER: Duration = Duration::from_millis(800);
        let now = std::time::Instant::now();
        let mut query = self
            .typed
            .take()
            .filter(|(_, at)| now.duration_since(*at) < RESET_AFTER)
            .map(|(query, _)| query)
            .unwrap_or_default();
        query.push_str(&text.to_lowercase());
        self.typed = Some((query.clone(), now));
        let Some(flat) = &self.flat_cache else {
            return;
        };
        let current = self
            .selected_flat_entry()
            .map(|(index, _, _)| index)
            .unwrap_or(0);
        let start = if query.chars().count() == 1 {
            (current + 1) % flat.len().max(1)
        } else {
            current
        };
        let index = (0..flat.len())
            .map(|offset| (start + offset) % flat.len())
            .find(|index| flat[*index].0.name.to_lowercase().starts_with(&query));
        if let Some(index) = index {
            self.select_row(index, false);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_some() {
            return;
        }
        if self.flat_cache.is_none() {
            self.rebuild_flat_cache();
        }
        let len = self.flat_cache.as_ref().map_or(0, Vec::len);
        if len == 0 {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let current = self
            .selected_flat_entry()
            .map(|(index, _, _)| index)
            .unwrap_or(0);
        let mut handled = true;
        match key {
            "up" => self.select_row(current.saturating_sub(1), modifiers.shift),
            "down" => self.select_row((current + 1).min(len - 1), modifiers.shift),
            "home" => self.select_row(0, modifiers.shift),
            "end" => self.select_row(len - 1, modifiers.shift),
            "pageup" => self.select_row(current.saturating_sub(10), modifiers.shift),
            "pagedown" => self.select_row((current + 10).min(len - 1), modifiers.shift),
            "left" => {
                if let Some((index, entry, depth)) = self.selected_flat_entry() {
                    if entry.is_dir() && self.expanded_folders.contains(&entry.path) {
                        self.toggle_folder_expanded(entry.path, cx);
                    } else if depth > 0 {
                        let parent = self.flat_cache.as_ref().and_then(|flat| {
                            (0..index).rev().find(|parent| flat[*parent].1 < depth)
                        });
                        if let Some(parent) = parent {
                            self.select_row(parent, modifiers.shift);
                        }
                    }
                }
            }
            "right" => {
                if let Some((_, entry, _)) = self.selected_flat_entry()
                    && entry.is_dir()
                {
                    if !self.expanded_folders.contains(&entry.path) {
                        self.toggle_folder_expanded(entry.path, cx);
                    } else {
                        self.select_row((current + 1).min(len - 1), modifiers.shift);
                    }
                }
            }
            "enter" | "return" => self.open_selected_from_keyboard(cx),
            "f2" => {
                if let Some((_, entry, _)) = self.selected_flat_entry() {
                    self.start_rename(&entry, window, cx);
                }
            }
            "delete" | "backspace" => {
                if let Some((_, entry, _)) = self.selected_flat_entry() {
                    self.start_delete(&entry, window, cx);
                }
            }
            "escape" if self.selected_paths.len() > 1 => {
                if let Some(path) = self.selected_path.clone() {
                    self.select_path(path, cx);
                }
            }
            "escape" => cx.emit(FileTreeEvent::FocusActiveTerminal),
            "c" if modifiers.secondary() => {
                self.copy_selection_to_clipboard(false, cx);
            }
            "x" if modifiers.secondary() => {
                self.copy_selection_to_clipboard(true, cx);
            }
            "v" if modifiers.secondary() => {
                let target = self.selected_directory();
                self.paste_into_tree(target, cx);
            }
            "a" if modifiers.secondary() => {
                self.selected_paths = self
                    .flat_cache
                    .as_ref()
                    .into_iter()
                    .flat_map(|flat| flat.iter())
                    .map(|(entry, _)| entry.path.clone())
                    .collect();
            }
            _ => {
                handled = false;
                if !modifiers.secondary()
                    && !modifiers.control
                    && !modifiers.alt
                    && let Some(text) = event.keystroke.key_char.as_deref()
                    && !text.chars().any(char::is_control)
                {
                    self.typeahead_select(text);
                    handled = true;
                }
            }
        }
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn run_tree_tool(&mut self, tool: TreeTool, window: &mut Window, cx: &mut Context<Self>) {
        match tool {
            TreeTool::NewFile => {
                self.start_create(self.root_path.clone(), FileEntryKind::File, window, cx)
            }
            TreeTool::NewFolder => {
                self.start_create(self.root_path.clone(), FileEntryKind::Directory, window, cx)
            }
            TreeTool::Refresh => self.refresh_all_loaded(cx),
            TreeTool::CollapseAll => self.collapse_all(cx),
            TreeTool::LocateActive => cx.emit(FileTreeEvent::LocateActiveFile),
        }
    }

    fn render_root_tool_button(
        &self,
        id: &'static str,
        icon: Icon,
        tool: TreeTool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let tooltip = match tool {
            TreeTool::NewFile => "新建文件",
            TreeTool::NewFolder => "新建文件夹",
            TreeTool::Refresh => "刷新",
            TreeTool::CollapseAll => "折叠全部",
            TreeTool::LocateActive => "定位当前文件",
        };
        div()
            .id(id)
            .w(px(22.0))
            .h(px(22.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .tooltip(Tooltip::text(tooltip))
            .hover(|style| style.bg(theme.title_bar_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.run_tree_tool(tool, window, cx);
                }),
            )
            .child(
                svg()
                    .size(px(20.0))
                    .text_color(theme.text)
                    .path(icon.path()),
            )
            .into_any_element()
    }

    fn render_root_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(1.0))
            .child(self.render_root_tool_button(
                "file-tree-new-file",
                Icon::FileIcon("icons/winc-new-file.svg"),
                TreeTool::NewFile,
                cx,
            ))
            .child(self.render_root_tool_button(
                "file-tree-new-folder",
                Icon::FileIcon("icons/winc-new-folder.svg"),
                TreeTool::NewFolder,
                cx,
            ))
            .child(self.render_root_tool_button(
                "file-tree-refresh",
                Icon::FileIcon("icons/winc-refresh.svg"),
                TreeTool::Refresh,
                cx,
            ))
            .child(self.render_root_tool_button(
                "file-tree-collapse-all",
                Icon::FileIcon("icons/winc-collapse-all.svg"),
                TreeTool::CollapseAll,
                cx,
            ))
            .child(self.render_root_tool_button(
                "file-tree-locate-active",
                Icon::FileIcon("icons/winc-locate.svg"),
                TreeTool::LocateActive,
                cx,
            ))
            .into_any_element()
    }

    fn schedule_drag_expand(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.expanded_folders.contains(&path) {
            return;
        }
        if self
            .drag_hover
            .as_ref()
            .is_some_and(|(current, _)| current == &path)
        {
            return;
        }
        self.drag_hover = Some((path.clone(), std::time::Instant::now()));
        self.drag_expand_task = None;
        let weak = cx.weak_entity();
        self.drag_expand_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(600))
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this
                    .drag_hover
                    .as_ref()
                    .is_some_and(|(current, _)| current == &path)
                {
                    this.drag_hover = None;
                    this.toggle_folder_expanded(path, cx);
                }
            });
        }));
    }

    pub(super) fn clear_drag_expand(&mut self) {
        self.drag_hover = None;
        self.drag_expand_task = None;
    }

    fn auto_scroll_for_drag(&mut self, event: &gpui::DragMoveEvent<DraggedTreeEntry>) {
        let y = event.event.position.y;
        let top = event.bounds.origin.y;
        let bottom = event.bounds.bottom();
        let edge = px(LIST_ROW_HEIGHT);
        let len = self.flat_cache.as_ref().map_or(0, Vec::len);
        if len == 0 {
            return;
        }
        if y < top + edge && self.visible_range.start > 0 {
            self.scroll_handle
                .scroll_to_item(self.visible_range.start - 1, gpui::ScrollStrategy::Nearest);
        } else if y > bottom - edge {
            self.scroll_handle.scroll_to_item(
                self.visible_range.end.min(len - 1),
                gpui::ScrollStrategy::Nearest,
            );
        }
    }

    pub(super) fn add_workspace_folder_prompt(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.loader.is_local() {
            return;
        }
        let picker = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("添加文件夹到当前工作区".into()),
        });
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = picker.await else {
                return;
            };
            let _ = weak.update(cx, |this, cx| {
                for path in paths {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    let overlaps = std::iter::once(&this.root_path)
                        .chain(this.extra_roots.iter())
                        .any(|root| path.starts_with(root) || root.starts_with(&path));
                    if overlaps || !path.is_dir() {
                        continue;
                    }
                    cx.watch_recursive_path(&path);
                    this.load_directory(path.clone(), cx);
                    this.extra_roots.push(path);
                }
                this.invalidate_flat_cache();
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn remove_extra_root(&mut self, root: &Path, cx: &mut Context<Self>) {
        self.extra_roots.retain(|path| path != root);
        self.collapsed_extra_roots.remove(root);
        self.expanded_folders.retain(|path| !path.starts_with(root));
        self.selected_paths.retain(|path| !path.starts_with(root));
        self.loader.remove_cached_recursive(root);
        if self.loader.is_local() {
            cx.unwatch_recursive_path(root);
        }
        self.invalidate_flat_cache();
        cx.notify();
    }

    fn render_extra_root_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.extra_roots
            .clone()
            .into_iter()
            .map(|root| {
                let is_expanded = !self.collapsed_extra_roots.contains(&root);
                let click_root = root.clone();
                let right_root = root.clone();
                let drop_root = root.clone();
                let move_root = root.clone();
                let theme = cx.theme().ui;
                let name = root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string())
                    .to_uppercase();
                div()
                    .id(format!("file-tree-extra-root-{}", root.display()))
                    .w_full()
                    .h(px(LIST_ROW_HEIGHT))
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .cursor_pointer()
                    .tooltip(Tooltip::text(root.to_string_lossy().into_owned()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_extra_roots.remove(&click_root) {
                            this.collapsed_extra_roots.insert(click_root.clone());
                        } else if !this.loader.has_cached(&click_root) {
                            this.load_directory(click_root.clone(), cx);
                        }
                        this.invalidate_flat_cache();
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_context_menu_for_extra_root(
                                right_root.clone(),
                                event.position,
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_drop(
                        cx.listener(move |this, paths: &gpui::ExternalPaths, _, cx| {
                            this.drop_external_paths(paths.paths(), &drop_root, cx);
                        }),
                    )
                    .on_drop(
                        cx.listener(move |this, dragged: &DraggedTreeEntry, window, cx| {
                            let copy = window.modifiers().alt;
                            this.drop_tree_entry(dragged, &move_root, copy, cx);
                        }),
                    )
                    .child(div().w(px(12.0)).child(if is_expanded { "▾" } else { "▸" }))
                    .child(
                        svg()
                            .size(px(14.0))
                            .text_color(theme.accent)
                            .path(Icon::Folder.path()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_root_row(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
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
        let drop_root = root_path.clone();
        let move_root = root_path.clone();
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
            )
            // 右键：根目录菜单（新建 / 折叠全部；根不可重命名 / 删除）
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_context_menu_for_root(event.position, window, cx);
                }),
            )
            // 拖放目标：OS 文件拖入（上传）与树内条目拖入（移动）都落到根目录
            .drag_over::<gpui::ExternalPaths>(|style, _paths, _window, cx| {
                style
                    .bg(cx.theme().ui.drop_target)
                    .border_1()
                    .border_color(cx.theme().ui.drop_target_border)
            })
            .on_drop(
                cx.listener(move |this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.drop_external_paths(paths.paths(), &drop_root, cx);
                }),
            )
            .drag_over::<DraggedTreeEntry>(|style, _dragged, _window, cx| {
                style
                    .bg(cx.theme().ui.drop_target)
                    .border_1()
                    .border_color(cx.theme().ui.drop_target_border)
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedTreeEntry, window, cx| {
                    let copy = window.modifiers().alt;
                    this.drop_tree_entry(dragged, &move_root, copy, cx);
                }),
            );

        let row = row.child(
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
                )
                // 按需求固定在顶层目录名后面，而不是文件/搜索/Git 标签栏。
                .child(self.render_root_toolbar(cx)),
        );
        row.into_any_element()
    }

    fn render_sticky_folders(
        &self,
        anchor: &StickyAnchor,
        scroll_offset: Point<Pixels>,
        item_height: Pixels,
        theme: &UiTheme,
        entity: &gpui::Entity<Self>,
        max_rows: usize,
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
            if let Some((entry, depth)) = flat.get(idx)
                && *depth < current_depth
            {
                sticky_parents.push((entry.clone(), *depth + 1));
                current_depth = *depth;
                if current_depth == 0 {
                    break;
                }
            }
        }
        sticky_parents.reverse();
        sticky_parents.truncate(max_rows);

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
                self.render_sticky_row(entry, *depth, is_last, drifting_y_offset, theme, entity)
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
        entity: &gpui::Entity<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let indent = px(depth as f32 * INDENT_SIZE);
        let name = entry.name.clone();
        let icon = entry_icon(entry);

        let sticky_entity = entity.clone();
        let sticky_path = entry.path.clone();
        let row = div()
            .id(format!("file-tree-sticky-{}", entry.path.display()))
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .pl(px(4.0) + indent)
            .pr(px(4.0))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                sticky_entity.update(cx, |this, _| {
                    this.expanded_folders.remove(&sticky_path);
                    this.flat_cache = None;
                });
                cx.notify(sticky_entity.entity_id());
            });

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
        row_index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = entry.path.clone();
        // 行内编辑（新建合成行 / 重命名行）：输入框替换条目名
        if self
            .editing
            .as_ref()
            .is_some_and(|e| e.editing_path() == path)
        {
            return self.render_editing_row(depth, cx);
        }

        let is_selected = self.is_selected(&path);
        let is_directory = entry.is_dir();
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

        let git_status = if is_directory {
            self.git_directory_status(&path)
        } else {
            self.git_file_status(&path)
        };
        let row_text = if is_selected {
            theme.accent_foreground
        } else if is_directory {
            git_status
                .map(|status| git_status_color(status, &theme))
                .unwrap_or(theme.text)
        } else {
            // git 变更文件着色：新增绿 / 修改蓝 / 冲突橙红（选中态优先）。
            self.git_name_color(&path, &theme).unwrap_or(theme.text)
        };
        let git_badge = git_status.map(|status| {
            let color = if is_selected {
                theme.accent_foreground
            } else {
                git_status_color(status, &theme)
            };
            if is_directory {
                div()
                    .w(px(6.0))
                    .h(px(6.0))
                    .flex_none()
                    .rounded_full()
                    .bg(color)
                    .into_any_element()
            } else {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(color)
                    .child(status.badge_letter().to_string())
                    .into_any_element()
            }
        });

        let click_path = path.clone();
        let right_click_path = path.clone();
        let right_click_entry = entry.clone();
        let hover_path = path.clone();
        let drag_paths = if self.is_selected(&path) {
            self.selected_operation_paths()
        } else {
            vec![path.clone()]
        };
        let indent = px(depth as f32 * INDENT_SIZE);

        let row = div()
            .id(format!("sidebar-file-tree-entry-{}", path.display()))
            .relative()
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .pl(px(4.0) + indent)
            .pr(px(4.0))
            .cursor_pointer()
            .tooltip(Tooltip::text(path.to_string_lossy().into_owned()))
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                this.set_hovered_path(hover_path.clone(), *hovered, cx);
            }))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    let focus = this.focus_handle.clone();
                    window.focus(&focus, cx);
                    let modifiers = event.modifiers();
                    this.select_with_modifiers(click_path.clone(), row_index, modifiers);
                    // 多选手势只改变选择，不展开目录或打开一批文件。
                    if modifiers.shift || modifiers.secondary() {
                        cx.notify();
                        return;
                    }
                    if is_directory {
                        if modifiers.alt {
                            this.toggle_dir_recursively(click_path.clone(), cx);
                        } else {
                            this.toggle_folder_expanded(click_path.clone(), cx);
                        }
                    } else {
                        let event = if event.click_count() >= 2 {
                            FileTreeEvent::OpenFile {
                                path: click_path.clone(),
                                ssh_config: this.event_ssh_config(),
                            }
                        } else {
                            FileTreeEvent::PreviewFile {
                                path: click_path.clone(),
                                ssh_config: this.event_ssh_config(),
                            }
                        };
                        cx.emit(event);
                    }
                }),
            )
            // 右键：选中该行 + 按目标类型（文件 / 文件夹）弹菜单（VSCode 同款区分）
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if !this.is_selected(&right_click_path) {
                        this.select_path(right_click_path.clone(), cx);
                    }
                    let focus = this.focus_handle.clone();
                    window.focus(&focus, cx);
                    this.open_context_menu_for_entry(
                        &right_click_entry,
                        event.position,
                        window,
                        cx,
                    );
                }),
            )
            // 内部拖拽：拖动行移动文件 / 文件夹（拖到文件夹行 = 移入该目录）
            .on_drag(
                DraggedTreeEntry {
                    path: path.clone(),
                    paths: drag_paths,
                    name: if self.selected_paths.len() > 1 && self.is_selected(&path) {
                        format!("{} 个条目", self.selected_paths.len())
                    } else {
                        name.clone()
                    },
                    is_dir: is_directory,
                },
                |entry, _offset, _window, cx| {
                    cx.new(|_| DraggedTreeEntry {
                        path: entry.path.clone(),
                        paths: entry.paths.clone(),
                        name: entry.name.clone(),
                        is_dir: entry.is_dir,
                    })
                },
            );

        let drag_hover_path = path.clone();
        let row = row.on_drag_move(cx.listener(
            move |this, event: &gpui::DragMoveEvent<DraggedTreeEntry>, _, cx| {
                this.auto_scroll_for_drag(event);
                if is_directory {
                    this.schedule_drag_expand(drag_hover_path.clone(), cx);
                } else {
                    this.clear_drag_expand();
                }
            },
        ));

        // 拖放目标：目录行同时接受 OS 文件拖入（上传）与树内条目拖入（移动/复制）
        let row = if is_directory {
            let drop_target = path.clone();
            let move_target = path.clone();
            let hover_target = path.clone();
            row.drag_over::<gpui::ExternalPaths>(|style, _paths, _window, cx| {
                style
                    .bg(cx.theme().ui.drop_target)
                    .border_1()
                    .border_color(cx.theme().ui.drop_target_border)
            })
            .on_drop(
                cx.listener(move |this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.drop_external_paths(paths.paths(), &drop_target, cx);
                }),
            )
            .drag_over::<DraggedTreeEntry>(move |style, dragged, window, cx| {
                let copy = window.modifiers().alt;
                if super::file_tree_context_menu::can_drop_tree_entry(dragged, &hover_target, copy)
                {
                    style
                        .bg(cx.theme().ui.drop_target)
                        .border_1()
                        .border_color(cx.theme().ui.drop_target_border)
                } else {
                    style
                }
            })
            .on_drop(cx.listener(
                move |this, dragged: &DraggedTreeEntry, window, cx| {
                    let copy = window.modifiers().alt;
                    this.drop_tree_entry(dragged, &move_target, copy, cx);
                },
            ))
        } else {
            row
        };

        let row = row.child(
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
                                .text_color(if is_selected {
                                    theme.accent_foreground
                                } else if is_directory {
                                    git_status
                                        .map(|status| git_status_color(status, &theme))
                                        .unwrap_or(theme.accent)
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
                )
                .when(entry.kind == FileEntryKind::Symlink, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.text_subtle)
                            .child("↗"),
                    )
                })
                .when_some(git_badge, |row, badge| row.child(badge)),
        );
        let guides = (0..depth).map(|level| {
            div()
                .absolute()
                .left(px(9.0 + level as f32 * INDENT_SIZE))
                .top_0()
                .bottom_0()
                .w(px(1.0))
                .bg(theme.text_subtle.opacity(0.16))
                .into_any_element()
        });

        // 右键菜单 / 内部拖拽：由 on_mouse_down(Right) / on_drag 处理（见上）
        row.children(guides).into_any_element()
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
                this.render_sticky_folders(
                    &anchor,
                    scroll_offset,
                    item_height,
                    &theme,
                    &self.entity,
                    ((f32::from(bounds.size.height) / f32::from(item_height) * 0.4).floor()
                        as usize)
                        .min(7),
                )
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
        let focus_handle = self.focus_handle.clone();
        let directory = self.root_path.clone();
        let root_drop_target = self.root_path.clone();
        let root_move_target = self.root_path.clone();

        // Build flat cache if needed
        let flat_count = if let Some(ref flat) = self.flat_cache {
            flat.len()
        } else {
            self.rebuild_flat_cache()
        };

        let is_directory_loaded = self.loader.has_cached(&directory);
        let root_is_loading = self.loader.is_loading(&directory);
        let root_failed = self.load_failures.contains_key(&directory);
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
                .children(self.render_extra_root_rows(cx))
                .into_any_element()
        } else if !is_directory_loaded {
            // 加载中/失败重试中必须给出反馈。旧实现这里直接画一个空列表，
            // 与「空文件夹」视觉上没有区别，用户只能看到一棵不出内容的树。
            let status = if root_is_loading {
                "正在加载目录…"
            } else if root_failed {
                "目录加载失败，点击立即重试"
            } else {
                "正在加载目录…"
            };
            div()
                .id("sidebar-file-tree")
                .w_full()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_root_row(cx))
                .children(self.render_extra_root_rows(cx))
                .child(
                    div()
                        .id("file-tree-load-status")
                        .w_full()
                        .px(px(8.0))
                        .pt(px(12.0))
                        .text_sm()
                        .text_color(theme.text_subtle)
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                if root_failed && !this.loader.is_loading(&directory) {
                                    this.clear_load_failure(&directory);
                                    this.load_directory(directory.clone(), cx);
                                }
                            }),
                        )
                        .child(status),
                )
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
                .children(self.render_extra_root_rows(cx))
                .child(
                    div()
                        .id("file-tree-empty-folder")
                        .w_full()
                        .px(px(8.0))
                        .pt(px(12.0))
                        .text_sm()
                        .text_color(theme.text_subtle)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                                if event.click_count >= 2 {
                                    let root = this.root_path.clone();
                                    this.start_create(root, FileEntryKind::File, window, cx);
                                }
                            }),
                        )
                        .child("空文件夹 · 双击新建文件"),
                )
                .into_any_element()
        } else {
            let scroll_handle = self.scroll_handle.clone();

            let list = uniform_list(
                "sidebar-file-tree-list",
                flat_count,
                cx.processor(|this, range: Range<usize>, _window, _cx| {
                    this.visible_range = range.clone();
                    let start = range.start;
                    let entries: Vec<(FileEntry, usize)> = if let Some(flat) = &this.flat_cache {
                        range.filter_map(|ix| flat.get(ix).cloned()).collect()
                    } else {
                        Vec::new()
                    };
                    entries
                        .iter()
                        .enumerate()
                        .map(|(offset, (entry, depth))| {
                            this.render_tree_row(entry, depth + 1, start + offset, _cx)
                        })
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

            let horizontal_scroll = self.horizontal_scroll_handle.clone();
            let content_width = self.tree_content_width;
            let expansion_epoch = self.expansion_epoch;
            div()
                .id("sidebar-file-tree")
                .w_full()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_root_row(cx))
                .children(self.render_extra_root_rows(cx))
                .child(
                    div()
                        .id("file-tree-horizontal-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_x_scroll()
                        // 仅横向容器捕获 x 轴；纵向滚轮必须留给内部 uniform_list。
                        .restrict_scroll_to_axis()
                        .track_scroll(&horizontal_scroll)
                        .with_animation(
                            ("file-tree-expansion", expansion_epoch),
                            gpui::Animation::new(Duration::from_millis(120)),
                            |this, delta| this.opacity(delta),
                        )
                        .child(
                            div()
                                .w(px(content_width))
                                .h_full()
                                .flex()
                                .flex_col()
                                .child(list),
                        ),
                )
                .into_any_element()
        };

        // 右键菜单浮层（手动模式：deferred + anchored，随窗口边缘自适应）
        let menu_overlay = self.render_context_menu_overlay();

        div()
            .track_focus(&focus_handle)
            .key_context("FileTree")
            .on_key_down(cx.listener(Self::handle_key_down))
            .flex_1()
            .h_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            // 兑底拖放 / 右键：树的空白区域 → 根目录（VSCode 行为：新建 / 折叠）
            // drag_over 给空白区也加上高亮，否则拖到行间隙时没有任何落点反馈。
            .drag_over::<gpui::ExternalPaths>(|style, _paths, _window, cx| {
                style
                    .bg(cx.theme().ui.drop_target)
                    .border_1()
                    .border_color(cx.theme().ui.drop_target_border)
            })
            .on_drop(
                cx.listener(move |this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.drop_external_paths(paths.paths(), &root_drop_target, cx);
                }),
            )
            .drag_over::<DraggedTreeEntry>(|style, _dragged, _window, cx| {
                style
                    .bg(cx.theme().ui.drop_target)
                    .border_1()
                    .border_color(cx.theme().ui.drop_target_border)
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedTreeEntry, window, cx| {
                    let copy = window.modifiers().alt;
                    this.drop_tree_entry(dragged, &root_move_target, copy, cx);
                }),
            )
            .on_drag_move(cx.listener(
                |this, event: &gpui::DragMoveEvent<DraggedTreeEntry>, _, _| {
                    this.auto_scroll_for_drag(event);
                },
            ))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_context_menu_for_empty_area(event.position, window, cx);
                }),
            )
            .when_some(menu_overlay, |this, overlay| this.child(overlay))
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

#[cfg(test)]
mod tree_state_tests {
    use super::*;
    use crate::file_manager::{LocalFileSystem, RemoteFileSystem};
    use gpui::Entity;

    #[gpui::test]
    fn remote_file_event_clones_backend_ssh_config(cx: &mut gpui::TestAppContext) {
        cx.update(crate::file_manager::watcher::init);
        let mut config = SshConnectionConfig::local();
        config.host = "event-config-test".to_string();
        config.username = "remote-user".to_string();
        let backend = Arc::new(RemoteFileSystem::new(config.clone()));
        let tree = cx.update(|cx| {
            cx.new(|cx| SidebarFileTree::new_without_io(cx, backend, PathBuf::from("/remote")))
        });
        tree.update(cx, |tree, cx| {
            let event_config = tree
                .event_ssh_config()
                .expect("远程文件事件必须携带后端 SSH 配置");
            assert_eq!(*event_config, config);
            assert!(tree.matches_backend(Some(&config)));
            tree.cleanup(cx);
        });
    }

    #[gpui::test]
    fn deep_tree_flattens_without_depth_cutoff_and_supports_range_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::file_manager::watcher::init);
        let root = std::env::temp_dir().join(format!(
            "deepterm-deep-tree-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test root");
        let tree = cx.update(|cx| {
            cx.new(|cx| {
                SidebarFileTree::new_without_io(cx, Arc::new(LocalFileSystem), root.clone())
            })
        });
        tree.update(cx, |tree, cx| {
            assert!(
                tree.event_ssh_config().is_none(),
                "本地文件事件绝不能携带缓存占位 SSH 配置"
            );
            assert!(tree.matches_backend(None), "定位按钮必须匹配本地后端");
            let mut parent = root.clone();
            for index in 0..30 {
                let child = parent.join(format!("d{index}"));
                let entry = FileEntry {
                    name: format!("d{index}"),
                    path: child.clone(),
                    kind: FileEntryKind::Directory,
                    size: 0,
                    symlink_target_is_dir: false,
                };
                tree.loader.fail_load(&parent);
                assert!(tree.loader.begin_load(&parent));
                tree.loader.finish_load(&parent, vec![entry]);
                tree.expanded_folders.insert(child.clone());
                parent = child;
            }
            assert_eq!(tree.rebuild_flat_cache(), 1, "紧凑目录合并单子目录链");
            tree.compact_folders = false;
            assert_eq!(tree.rebuild_flat_cache(), 30);
            tree.select_row(5, false);
            tree.select_row(9, true);
            assert_eq!(tree.selected_paths.len(), 5);
            let path_at = |depth: usize| {
                (0..=depth).fold(root.clone(), |path, index| path.join(format!("d{index}")))
            };
            assert!(tree.selected_paths.contains(&path_at(5)));
            assert!(tree.selected_paths.contains(&path_at(9)));

            // 已绘制行的索引可能因 watcher 重建而失效；不得切片越界或选错文件。
            let anchor = tree.flat_cache.as_ref().unwrap()[0].clone();
            tree.flat_cache = Some(vec![anchor.clone()]);
            tree.selection_anchor = Some(anchor.0.path.clone());
            let clicked = path_at(9);
            tree.select_with_modifiers(
                clicked.clone(),
                20,
                gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            );
            assert_eq!(tree.selected_path.as_ref(), Some(&clicked));
            assert!(tree.selected_paths.contains(&clicked));

            let status_path = path_at(0).join("changed.rs");
            tree.set_git_status(
                Some((
                    root.to_string_lossy().into_owned(),
                    Arc::new(
                        [(
                            status_path
                                .strip_prefix(&root)
                                .expect("status path under root")
                                .to_string_lossy()
                                .into_owned(),
                            crate::editor::git_status::FileStatus::Untracked,
                        )]
                        .into_iter()
                        .collect(),
                    ),
                )),
                cx,
            );
            assert_eq!(
                tree.git_directory_status(&path_at(0)),
                Some(crate::editor::git_status::FileStatus::Untracked)
            );

            let conflict = FileStatus::Unmerged(crate::editor::git_status::UnmergedStatus {
                first_head: crate::editor::git_status::UnmergedStatusCode::Updated,
                second_head: crate::editor::git_status::UnmergedStatusCode::Updated,
            });
            let modified = FileStatus::Tracked(crate::editor::git_status::TrackedStatus {
                index_status: crate::editor::git_status::StatusCode::Unmodified,
                worktree_status: crate::editor::git_status::StatusCode::Modified,
            });
            for statuses in [
                vec![FileStatus::Untracked, modified, conflict],
                vec![conflict, modified, FileStatus::Untracked],
                vec![modified, conflict, FileStatus::Untracked],
            ] {
                assert_eq!(
                    aggregate_git_directory_status(statuses),
                    Some(conflict),
                    "冲突必须不受 HashMap/输入顺序影响"
                );
            }

            let symlink_dir = path_at(0);
            tree.flat_cache = Some(vec![(
                FileEntry {
                    name: "linked".into(),
                    path: symlink_dir.clone(),
                    kind: FileEntryKind::Symlink,
                    size: 0,
                    symlink_target_is_dir: true,
                },
                0,
            )]);
            tree.selected_path = Some(symlink_dir.clone());
            assert_eq!(tree.selected_directory(), symlink_dir);
            tree.cleanup(cx);
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn completion_after_switching_back_to_loading_root_is_accepted(cx: &mut gpui::TestAppContext) {
        cx.update(crate::file_manager::watcher::init);
        let root_a = PathBuf::from("/workspace/a");
        let root_b = PathBuf::from("/workspace/b");
        let tree = cx.update(|cx| {
            cx.new(|cx| {
                SidebarFileTree::new_without_io(cx, Arc::new(LocalFileSystem), root_a.clone())
            })
        });
        tree.update(cx, |tree, cx| {
            assert!(tree.loader.begin_load(&root_a));
            let original_epoch = tree.root_epoch;

            tree.root_path = root_b;
            tree.root_epoch += 1;
            tree.root_path = root_a.clone();
            tree.root_epoch += 1;
            // 与原 open_path 一样：已有加载时不会重复创建任务。
            assert!(!tree.loader.begin_load(&root_a));

            let entry = FileEntry {
                name: "main.rs".into(),
                path: root_a.join("main.rs"),
                kind: FileEntryKind::File,
                size: 0,
                symlink_target_is_dir: false,
            };
            tree.complete_directory_load(root_a.clone(), original_epoch, Ok(vec![entry]), cx);
            assert!(tree.loader.has_cached(&root_a));
            assert!(!tree.loader.is_loading(&root_a));
            assert_eq!(tree.rebuild_flat_cache(), 1);
            tree.cleanup(cx);
        });
    }

    struct ScrollHost {
        tree: Entity<SidebarFileTree>,
    }

    impl Render for ScrollHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.tree.clone()
        }
    }

    #[gpui::test]
    fn vertical_wheel_scrolls_list_not_horizontal_container(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::components::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        let root =
            std::env::temp_dir().join(format!("deepterm-scroll-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scroll root");
        let tree = cx.update(|cx| {
            let tree = cx.new(|cx| {
                SidebarFileTree::new_without_io(cx, Arc::new(LocalFileSystem), root.clone())
            });
            tree.update(cx, |tree, _| {
                let entries = (0..100)
                    .map(|index| FileEntry {
                        name: format!("file-{index:03}.rs"),
                        path: root.join(format!("file-{index:03}.rs")),
                        kind: FileEntryKind::File,
                        size: 0,
                        symlink_target_is_dir: false,
                    })
                    .collect();
                tree.loader.begin_load(&root);
                tree.loader.finish_load(&root, entries);
                tree.compact_folders = false;
                tree.invalidate_flat_cache();
            });
            tree
        });
        let window = cx.open_window(gpui::size(px(300.), px(300.)), {
            let tree = tree.clone();
            move |_, _| ScrollHost { tree }
        });
        let handle: gpui::AnyWindowHandle = window.into();
        let cx = &mut gpui::VisualTestContext::from_window(handle, cx);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let before_x = tree.read_with(cx, |tree, _| tree.horizontal_scroll_handle.offset().x);
        assert_eq!(tree.read_with(cx, |tree, _| tree.visible_range.start), 0);
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(px(150.), px(150.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-240.))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let start = tree.read_with(cx, |tree, _| tree.visible_range.clone());
        assert!(start.start > 0, "纵向滚轮必须推进 uniform_list 的可见行");
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.horizontal_scroll_handle.offset().x),
            before_x,
            "纵向滚轮不得被横向容器消费"
        );
        tree.update(cx, |tree, cx| tree.cleanup(cx));
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn context_menu_does_not_reenter_tree_update(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::components::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        let root = std::env::temp_dir().join(format!("deepterm-menu-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create menu root");
        let tree = cx.update(|cx| {
            cx.new(|cx| {
                SidebarFileTree::new_without_io(cx, Arc::new(LocalFileSystem), root.clone())
            })
        });
        let window = cx.open_window(gpui::size(px(400.), px(300.)), {
            let tree = tree.clone();
            move |_, _| ScrollHost { tree }
        });
        let handle: gpui::AnyWindowHandle = window.into();
        let cx = &mut gpui::VisualTestContext::from_window(handle, cx);
        let entry = FileEntry {
            name: "main.rs".into(),
            path: root.join("main.rs"),
            kind: FileEntryKind::File,
            size: 0,
            symlink_target_is_dir: false,
        };
        cx.update(|window, cx| {
            tree.update(cx, |tree, cx| {
                // 必须在同一个 tree.update 内调用，覆盖此前的 double-lease 崩溃路径。
                tree.open_context_menu_for_entry(&entry, gpui::point(px(50.), px(50.)), window, cx);
                assert!(tree.context_menu.is_some());
                tree.close_context_menu(cx);
                tree.open_context_menu_for_root(gpui::point(px(20.), px(20.)), window, cx);
                assert!(tree.context_menu.is_some());
                tree.close_context_menu(cx);
                tree.cleanup(cx);
            });
        });
        let _ = std::fs::remove_dir_all(root);
    }
}
