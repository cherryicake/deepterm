//! 文件树右键菜单与文件操作（新建 / 重命名 / 删除 / 下载 / 拖拽上传）。
//!
//! 菜单挂载在各行（参考 VSCode / Zed 项目面板）：`context_menu()` 在右键
//! 打开时按行捕获的数据构建菜单项。新建与重命名采用树上行内编辑
//! （`InputState` 内联在行里，Enter 提交、Esc 取消、失焦提交，与 VSCode 一致）。
//! 所有文件系统写操作走 `FileSystem` trait 的阻塞方法（后台线程执行），
//! 完成后刷新受影响目录并同步展开 / 选中 / 监听状态。

use gpui::prelude::*;
use gpui::{Context, Entity, Focusable, PromptLevel, Styled, Window, div, px, relative};
use std::path::PathBuf;

use crate::components::input::{EditorInput, Escape, InputEvent, InputState};
use crate::components::menu::{PopupMenu, PopupMenuItem};
use crate::file_manager::file_tree_event::FileTreeEvent;
use crate::file_manager::filesystem::FileOpError;
use crate::file_manager::types::{FileEntry, FileEntryKind};
use crate::file_manager::validate_rename_to;
use crate::file_manager::watcher::WatcherContext;
use crate::file_manager::{ConflictPolicy, FileEndpoint, TransferOperation, TransferRequest};
use crate::theme::ActiveTheme;

use super::file_tree::{INDENT_SIZE, LIST_ROW_HEIGHT, SidebarFileTree};

/// 新建撞名时最多换号重试的次数（含首次尝试）。
const MAX_CREATE_ATTEMPTS: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeClipboardOperation {
    Copy,
    Cut,
}

#[derive(Clone, Debug)]
struct TreeClipboard {
    operation: TreeClipboardOperation,
    sources: Vec<FileEndpoint>,
}

static TREE_CLIPBOARD: std::sync::OnceLock<parking_lot::Mutex<Option<TreeClipboard>>> =
    std::sync::OnceLock::new();

fn tree_clipboard() -> &'static parking_lot::Mutex<Option<TreeClipboard>> {
    TREE_CLIPBOARD.get_or_init(|| parking_lot::Mutex::new(None))
}

fn tree_menu_action(
    label: impl Into<gpui::SharedString>,
    icon: &'static str,
    tree: Entity<SidebarFileTree>,
    action: impl Fn(&mut SidebarFileTree, &mut Window, &mut Context<SidebarFileTree>) + 'static,
) -> PopupMenuItem {
    PopupMenuItem::new(label)
        .icon(crate::components::button::Icon::new(icon))
        .on_click(move |_, window, cx| {
            tree.update(cx, |this, cx| {
                this.close_context_menu(cx);
                action(this, window, cx);
            });
        })
}

/// 生成第 `attempt` 次重试的候选名：文件取「主名 2.扩展名」，目录取「名字 2」。
fn next_candidate_name(target: &std::path::Path, attempt: u32, is_dir: bool) -> std::path::PathBuf {
    let Some(parent) = target.parent() else {
        return target.to_path_buf();
    };
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // 序号从 2 开始：第 1 次冲突产生「名字 2」。
    let index = attempt + 1;
    let new_name = if is_dir {
        format!("{name} {index}")
    } else {
        match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => format!("{stem} {index}.{ext}"),
            _ => format!("{name} {index}"),
        }
    };
    parent.join(new_name)
}

fn validate_create_path(name: &str) -> Result<(), String> {
    use std::path::Component;
    if name.is_empty() || name.contains('\0') || name.ends_with('/') {
        return Err("名称不能为空、以 / 结尾或包含 NUL".to_string());
    }
    let path = std::path::Path::new(name);
    if path.is_absolute() {
        return Err("请输入工作区内的相对路径".to_string());
    }
    let mut count = 0;
    for component in path.components() {
        match component {
            Component::Normal(_) => count += 1,
            _ => return Err("路径只能包含普通目录和文件名，不能包含 . 或 ..".to_string()),
        }
    }
    if count == 0 {
        Err("名称不能为空".to_string())
    } else {
        Ok(())
    }
}

/// 行内编辑模式
#[derive(Clone)]
pub(crate) enum TreeEditMode {
    /// 在 `parent` 下新建 `kind`；`proposed` 为占位名（合成行的身份）。
    Create {
        parent: std::path::PathBuf,
        kind: FileEntryKind,
        proposed: String,
    },
    /// 重命名 `path`（原名 `original_name`）。
    Rename {
        path: std::path::PathBuf,
        original_name: String,
    },
}

/// 行内编辑状态：一棵树同一时刻只有一行处于编辑中。
pub(crate) struct TreeEditing {
    pub mode: TreeEditMode,
    pub input: Entity<InputState>,
    /// 提交操作进行中：忽略重复 Enter / 失焦提交。
    pub locked: bool,
    pub error: Option<String>,
    /// 编辑所在窗口（spawn 的提交操作用它弹错误提示）。
    pub window_handle: gpui::AnyWindowHandle,
    pub _subscription: gpui::Subscription,
}

impl TreeEditing {
    /// 当前编辑行对应的展示路径（用于渲染匹配与合成行构建）。
    pub fn editing_path(&self) -> std::path::PathBuf {
        match &self.mode {
            TreeEditMode::Create {
                parent, proposed, ..
            } => parent.join(proposed),
            TreeEditMode::Rename { path, .. } => path.clone(),
        }
    }
}

impl SidebarFileTree {
    // ------------------------------------------------------------------
    // 行内编辑：启动 / 取消 / 提交
    // ------------------------------------------------------------------

    /// 开始行内「新建文件 / 文件夹」：确保父目录展开后出现合成输入行。
    pub(super) fn start_create(
        &mut self,
        parent: std::path::PathBuf,
        kind: FileEntryKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_editing(cx);

        // 展开父目录（必要时加载）：根 / 普通目录
        if parent == self.root_path {
            self.root_expanded = true;
        } else if !self.expanded_folders.contains(&parent) {
            self.toggle_folder_expanded(parent.clone(), cx);
        }

        // 占位名与既有条目去重：新建文件 → 新建文件 2 → …
        let base_name = if kind == FileEntryKind::Directory {
            "新建文件夹"
        } else {
            "新建文件"
        };
        let existing: Vec<String> = self
            .loader
            .get_cached(&parent)
            .map(|entries| entries.iter().map(|e| e.name.clone()).collect())
            .unwrap_or_default();
        let proposed = unique_placeholder_name(base_name, &existing);

        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).font_size(px(13.0));
            state.set_text(proposed.clone(), cx);
            state.selected_range = (0..proposed.len()).into();
            state
        });
        let subscription = self.subscribe_editing_input(&input, cx);
        self.editing = Some(TreeEditing {
            mode: TreeEditMode::Create {
                parent,
                kind,
                proposed,
            },
            input: input.clone(),
            locked: false,
            error: None,
            window_handle: window.window_handle(),
            _subscription: subscription,
        });
        input.read(cx).focus_handle.clone().focus(window, cx);

        self.invalidate_flat_cache();
        cx.notify();
    }

    /// 开始行内重命名：初始值为原名，文件默认选中主名（不含扩展名）。
    pub(super) fn start_rename(
        &mut self,
        entry: &FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_editing(cx);
        let original = entry.name.clone();
        // 文件选中主名（不含扩展名），目录 / 无扩展名选中全名 —— VSCode 同款
        let select_len = if entry.kind == FileEntryKind::Directory {
            original.len()
        } else {
            original
                .rfind('.')
                .filter(|&ix| ix > 0)
                .unwrap_or(original.len())
        };

        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).font_size(px(13.0));
            state.set_text(original.clone(), cx);
            state.selected_range = (0..select_len).into();
            state
        });
        let subscription = self.subscribe_editing_input(&input, cx);
        self.editing = Some(TreeEditing {
            mode: TreeEditMode::Rename {
                path: entry.path.clone(),
                original_name: original,
            },
            input: input.clone(),
            locked: false,
            error: None,
            window_handle: window.window_handle(),
            _subscription: subscription,
        });
        input.read(cx).focus_handle.clone().focus(window, cx);
        cx.notify();
    }

    fn subscribe_editing_input(
        &mut self,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> gpui::Subscription {
        cx.subscribe(input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Submit => {
                this.commit_editing(cx);
            }
            InputEvent::Blur => {
                // 点击别处：与 VSCode 一致按「确认」处理（空名回退为取消）。
                this.commit_editing(cx);
            }
            InputEvent::Change | InputEvent::Focus => {}
        })
    }

    fn set_editing_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(editing) = self.editing.as_mut() {
            editing.error = Some(error);
        }
        cx.notify();
    }

    pub(super) fn cancel_editing(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.invalidate_flat_cache();
            cx.notify();
        }
    }

    /// 提交行内编辑：Enter / 失焦触发；校验 + 后台执行 + 状态同步。
    pub(super) fn commit_editing(&mut self, cx: &mut Context<Self>) {
        // 先取值再操作，避免跨借用调用 self 的方法
        let Some(editing) = self.editing.as_ref() else {
            return;
        };
        if editing.locked {
            return;
        }
        let mode = editing.mode.clone();
        let raw = editing.input.read(cx).value().to_string();
        let name = raw.trim().to_string();

        match mode {
            TreeEditMode::Create {
                parent,
                kind,
                proposed,
            } => {
                // 空名视为放弃创建
                if name.is_empty() {
                    self.cancel_editing(cx);
                    return;
                }
                // 新建允许普通相对路径组件，例如 a/b/c.txt；拒绝绝对路径和 ..。
                if let Err(error) = validate_create_path(&name) {
                    self.set_editing_error(error, cx);
                    return;
                }
                let window_handle = match self.editing.as_mut() {
                    Some(editing) => {
                        editing.locked = true;
                        editing.window_handle
                    }
                    None => return,
                };
                // 用户没改过占位名（如「新建文件」）时，撞名就自动换号重试；
                // 用户手打了名字却撞名，则保留输入行让他自己改 —— 悄悄改名更让人困惑。
                let auto_rename = name == proposed;
                let target = parent.join(&name);
                self.spawn_create_op(parent, target, kind, auto_rename, window_handle, cx);
            }
            TreeEditMode::Rename {
                path,
                original_name,
            } => {
                if name.is_empty() || name == original_name {
                    self.cancel_editing(cx);
                    return;
                }
                if let Err(error) = validate_rename_to(&name) {
                    self.set_editing_error(error.to_string(), cx);
                    return;
                }
                let window_handle = match self.editing.as_mut() {
                    Some(editing) => {
                        editing.locked = true;
                        editing.window_handle
                    }
                    None => return,
                };
                let target = match path.parent() {
                    Some(parent) => parent.join(&name),
                    None => path.clone(),
                };
                self.spawn_rename_op(path, target, window_handle, cx);
            }
        }
    }

    // ------------------------------------------------------------------
    // 后台文件操作
    // ------------------------------------------------------------------

    fn spawn_create_op(
        &mut self,
        parent: std::path::PathBuf,
        target: std::path::PathBuf,
        kind: FileEntryKind,
        auto_rename_on_conflict: bool,
        window_handle: gpui::AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let backend = self.loader.backend().clone();
        let is_local = self.loader.is_local();
        let ssh_config = self.loader.ssh_config().cloned();
        let root_epoch = self.root_epoch;

        let is_dir = kind == FileEntryKind::Directory;
        let target_for_log = target.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut candidate = target.clone();
                    // 多级新建只补父链；最终目标仍由下方 no-replace 创建。
                    if let Some(parent) = target.parent() {
                        backend.create_dir_all(parent)?;
                    }
                    // 撞名自动换号重试：目录缓存缺失时（父目录还没加载过）
                    // 占位名去重无从下手，重试兜住「连点两次新建」这种场景。
                    for attempt in 1..=MAX_CREATE_ATTEMPTS {
                        let outcome = if is_dir {
                            backend.create_dir(&candidate)
                        } else {
                            backend.create_file(&candidate)
                        };
                        match outcome {
                            Ok(()) => return Ok(candidate),
                            Err(FileOpError::AlreadyExists(_))
                                if auto_rename_on_conflict && attempt < MAX_CREATE_ATTEMPTS =>
                            {
                                candidate = next_candidate_name(&target, attempt, is_dir);
                                log::debug!("新建撞名，改用 {:?}", candidate);
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    unreachable!("最后一次尝试失败时已 return Err")
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(created) => {
                    this.finish_editing_success(cx);
                    if this.root_epoch == root_epoch {
                        this.apply_path_created(&parent, &created, kind, is_local, ssh_config, cx);
                    }
                }
                Err(err) => {
                    log::warn!("新建失败 {}: {err}", target_for_log.display());
                    // 失败回退：解除锁定，输入行保留让用户改名重试或 Esc 退出
                    if let Some(editing) = this.editing.as_mut() {
                        editing.locked = false;
                    }
                    this.set_editing_error(err.to_string(), cx);
                    this.prompt_operation_error("新建失败", err, window_handle, cx);
                }
            });
        })
        .detach();
    }

    fn spawn_rename_op(
        &mut self,
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        window_handle: gpui::AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let backend = self.loader.backend().clone();
        let root_epoch = self.root_epoch;
        let from_in_op = from.clone();
        let to_in_op = to.clone();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.rename_path(&from_in_op, &to_in_op) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.finish_editing_success(cx);
                    if this.root_epoch == root_epoch {
                        this.apply_path_renamed(&from, &to, cx);
                    }
                }
                Err(err) => {
                    log::warn!("重命名失败 {} → {}: {err}", from.display(), to.display());
                    if let Some(editing) = this.editing.as_mut() {
                        editing.locked = false;
                    }
                    this.set_editing_error(err.to_string(), cx);
                    this.prompt_operation_error("重命名失败", err, window_handle, cx);
                }
            });
        })
        .detach();
    }

    /// 删除入口：本地默认废纸篓，远程因无系统废纸篓而明确永久删除。
    pub(super) fn start_delete(
        &mut self,
        entry: &FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let permanent = !self.loader.is_local();
        self.begin_delete(entry, permanent, window, cx);
    }

    pub(super) fn start_permanent_delete(
        &mut self,
        entry: &FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_delete(entry, true, window, cx);
    }

    fn begin_delete(
        &mut self,
        entry: &FileEntry,
        permanent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_editing(cx);
        let mut paths = self.selected_operation_paths();
        if paths.is_empty() {
            paths.push(entry.path.clone());
        }
        let use_trash = self.loader.is_local() && !permanent;
        let backend = self.loader.backend().clone();
        let root_epoch = self.root_epoch;
        let window_handle = window.window_handle();
        let count = paths.len();
        let detail = match (count, use_trash) {
            (1, true) => format!("将把「{}」移到系统废纸篓。", entry.name),
            (1, false) => format!(
                "将永久删除「{}」{}，此操作不可撤销。",
                entry.name,
                if entry.kind == FileEntryKind::Directory {
                    "及其全部内容"
                } else {
                    ""
                }
            ),
            (_, true) => format!("将把选中的 {count} 个条目移到系统废纸篓。"),
            (_, false) => format!("将永久删除选中的 {count} 个条目，此操作不可撤销。"),
        };
        let action = if use_trash {
            "移到废纸篓"
        } else {
            "永久删除"
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            action,
            Some(&detail),
            &[action, "取消"],
            cx,
        );

        cx.spawn(async move |this, cx| {
            let Ok(choice) = prompt.await else {
                return;
            };
            if choice != 0 {
                return;
            }
            let paths_for_op = paths.clone();
            let results = cx
                .background_executor()
                .spawn(async move {
                    let mut results = Vec::with_capacity(paths_for_op.len());
                    for path in paths_for_op {
                        let result = if use_trash {
                            trash::delete(&path)
                                .map_err(|error| FileOpError::Other(error.to_string()))
                        } else {
                            backend.remove_path(&path)
                        };
                        results.push((path, result));
                    }
                    results
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.root_epoch != root_epoch {
                    return;
                }
                let mut failures = Vec::new();
                for (path, result) in results {
                    match result {
                        Ok(()) => this.apply_path_removed(&path, cx),
                        Err(error) => failures.push(format!("{}: {error}", path.display())),
                    }
                }
                if !failures.is_empty() {
                    this.prompt_operation_error(
                        "删除失败",
                        FileOpError::Other(failures.join("\n")),
                        window_handle,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // 操作成功后的状态同步
    // ------------------------------------------------------------------

    /// 新建成功：刷新父目录、选中新条目；新建文件同时打开（VSCode 行为）。
    fn apply_path_created(
        &mut self,
        parent: &std::path::Path,
        target: &std::path::Path,
        kind: FileEntryKind,
        is_local: bool,
        ssh_config: Option<crate::ssh::SshConnectionConfig>,
        cx: &mut Context<Self>,
    ) {
        self.refresh_directory(parent.to_path_buf(), cx);
        self.selected_path = Some(target.to_path_buf());
        self.selected_paths.clear();
        self.selected_paths.insert(target.to_path_buf());
        self.selection_anchor = Some(target.to_path_buf());
        self.pending_reveal = Some(crate::sidebar_view::file_tree::PendingReveal {
            path: target.to_path_buf(),
            root_epoch: self.root_epoch,
        });
        if kind == FileEntryKind::File {
            // 新建文件直接打开编辑（VSCode / Zed 行为）
            cx.emit(FileTreeEvent::OpenFile {
                path: target.to_path_buf(),
                ssh_config: if is_local {
                    None
                } else {
                    ssh_config.map(Box::new)
                },
            });
        }
        self.invalidate_flat_cache();
        cx.notify();
    }

    /// 重命名成功：以旧路径为前缀同步所有按路径键控的状态。
    pub(crate) fn apply_path_renamed(
        &mut self,
        from: &std::path::Path,
        to: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        // 展开状态前缀替换，并收集改名前已展开的目录（改名后需重新加载）
        let rekeyed_dirs: Vec<std::path::PathBuf> = {
            let mut dirs = Vec::new();
            self.expanded_folders = self
                .expanded_folders
                .iter()
                .map(|p| rekey_path(p, from, to))
                .inspect(|new| {
                    if new.starts_with(to) {
                        dirs.push(new.clone());
                    }
                })
                .collect();
            dirs
        };
        // 远端监听前缀替换：旧路径解除、新路径注册
        if !self.loader.is_local() {
            let rekeyed: Vec<(std::path::PathBuf, std::path::PathBuf)> = self
                .watched_paths
                .iter()
                .filter(|p| p.starts_with(from))
                .map(|p| (p.clone(), rekey_path(p, from, to)))
                .collect();
            let backend = self.loader.backend();
            for (old, new) in rekeyed {
                cx.unwatch_remote_path(&old, backend);
                self.watched_paths.remove(&old);
                if self.watched_paths.insert(new.clone()) {
                    cx.watch_remote_path(&new, self.loader.backend().clone());
                }
            }
        }
        // 选中 / 悬停同步
        self.selected_paths = self
            .selected_paths
            .iter()
            .map(|path| rekey_path(path, from, to))
            .collect();
        if let Some(anchor) = self.selection_anchor.clone() {
            self.selection_anchor = Some(rekey_path(&anchor, from, to));
        }
        if let Some(selected) = self.selected_path.clone()
            && selected.starts_with(from)
        {
            self.selected_path = Some(rekey_path(&selected, from, to));
        }
        if let Some(hovered) = self.hovered_path.clone()
            && hovered.starts_with(from)
        {
            self.hovered_path = Some(rekey_path(&hovered, from, to));
        }
        // 缓存清理：旧路径及其后代全部失效，改名前展开的目录重新加载
        self.loader.remove_cached_recursive(from);
        self.loader.remove_shared_cache(from);
        for dir in rekeyed_dirs {
            self.load_directory(dir, cx);
        }
        // 父目录刷新以反映新名字
        if let Some(parent) = to.parent() {
            self.refresh_directory(parent.to_path_buf(), cx);
        }
        // 让编辑器跟着改路径（可能是目录：其下所有已打开的后代都要改）。
        cx.emit(FileTreeEvent::PathRenamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            ssh_config: self.event_ssh_config(),
        });
        self.invalidate_flat_cache();
        cx.notify();
    }

    /// 删除成功：清理展开 / 选中 / 监听 / 缓存，刷新父目录。
    /// 本地树 watcher 事件也会执行同样清理（幂等）；远端无事件由这里兜底。
    fn apply_path_removed(&mut self, removed: &std::path::Path, cx: &mut Context<Self>) {
        self.expanded_folders.retain(|p| !p.starts_with(removed));
        self.selected_paths
            .retain(|path| !path.starts_with(removed));
        if self
            .selected_path
            .as_ref()
            .is_some_and(|p| p.starts_with(removed))
        {
            self.selected_path = self.selected_paths.iter().next().cloned();
        }
        if self
            .selection_anchor
            .as_ref()
            .is_some_and(|path| path.starts_with(removed))
        {
            self.selection_anchor = self.selected_path.clone();
        }
        if self
            .hovered_path
            .as_ref()
            .is_some_and(|p| p.starts_with(removed))
        {
            self.hovered_path = None;
        }
        if !self.loader.is_local() {
            let to_unwatch: Vec<std::path::PathBuf> = self
                .watched_paths
                .iter()
                .filter(|p| **p != self.root_path && p.starts_with(removed))
                .cloned()
                .collect();
            let backend = self.loader.backend();
            for p in to_unwatch {
                cx.unwatch_remote_path(&p, backend);
                self.watched_paths.remove(&p);
            }
        }
        self.loader.remove_cached_recursive(removed);
        self.loader.remove_shared_cache(removed);
        if let Some(parent) = removed.parent() {
            self.refresh_directory(parent.to_path_buf(), cx);
        }
        // 让编辑器关掉对应的标签：文件已经不在了，留着只会在保存时被写回来。
        cx.emit(FileTreeEvent::PathRemoved {
            path: removed.to_path_buf(),
            ssh_config: self.event_ssh_config(),
        });
        self.invalidate_flat_cache();
        cx.notify();
    }

    fn finish_editing_success(&mut self, cx: &mut Context<Self>) {
        self.editing.take();
        self.invalidate_flat_cache();
        cx.notify();
    }

    /// 操作失败提示（单按钮确认框）。
    fn prompt_operation_error(
        &mut self,
        title: &str,
        error: FileOpError,
        window_handle: gpui::AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let title = title.to_string();
        let error = error.to_string();
        cx.spawn(async move |_, cx| {
            if let Ok(receiver) = cx.update_window(window_handle, |_, window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    title.as_str(),
                    Some(&error),
                    &["确定"],
                    cx,
                )
            }) {
                let _ = receiver.await;
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // 文件剪贴板与路径复制
    // ------------------------------------------------------------------

    fn endpoint_for_path(&self, path: &std::path::Path) -> Option<FileEndpoint> {
        if self.loader.is_local() {
            Some(FileEndpoint::Local {
                path: path.to_path_buf(),
            })
        } else {
            Some(FileEndpoint::Remote {
                ssh_config: self.loader.ssh_config()?.clone(),
                path: path.to_string_lossy().into_owned(),
            })
        }
    }

    pub(super) fn copy_selection_to_clipboard(&mut self, cut: bool, cx: &mut Context<Self>) {
        let paths = self.selected_operation_paths();
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        let sources = paths
            .iter()
            .filter_map(|path| self.endpoint_for_path(path))
            .collect();
        *tree_clipboard().lock() = Some(TreeClipboard {
            operation: if cut {
                TreeClipboardOperation::Cut
            } else {
                TreeClipboardOperation::Copy
            },
            sources,
        });
    }

    pub(super) fn copy_selected_paths(&self, relative: bool, cx: &mut Context<Self>) {
        let mut paths = self.selected_operation_paths();
        if paths.is_empty() && !relative {
            paths.push(self.root_path.clone());
        }
        let text = paths
            .iter()
            .map(|path| {
                if relative {
                    path.strip_prefix(&self.root_path)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/")
                } else {
                    path.to_string_lossy().into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    pub(super) fn has_file_clipboard(&self) -> bool {
        tree_clipboard()
            .lock()
            .as_ref()
            .is_some_and(|clipboard| !clipboard.sources.is_empty())
    }

    pub(super) fn paste_into_tree(
        &mut self,
        target_dir: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(destination) = self.endpoint_for_path(&target_dir) else {
            return;
        };
        let clipboard = tree_clipboard().lock().clone();
        let Some(clipboard) = clipboard else {
            return;
        };
        if clipboard.sources.iter().any(|source| {
            source.is_same_device(&destination) && target_dir.starts_with(source.path_buf())
        }) {
            self.prompt_simple_error("无法粘贴", "目标目录不能是源条目自身或其后代。", cx);
            return;
        }
        let operation = match clipboard.operation {
            TreeClipboardOperation::Copy => TransferOperation::Copy,
            TreeClipboardOperation::Cut => TransferOperation::Move,
        };
        cx.emit(FileTreeEvent::StartTransfer {
            request: Box::new(TransferRequest {
                sources: clipboard.sources,
                destination,
                operation,
                conflict_policy: ConflictPolicy::Ask,
                rename_to: None,
            }),
        });
        // 剪切语义为移动一次；复制保留剪贴板，可连续粘贴多份。
        if clipboard.operation == TreeClipboardOperation::Cut {
            tree_clipboard().lock().take();
        }
    }

    pub(super) fn selected_directory(&self) -> std::path::PathBuf {
        let Some(path) = self.selected_path.as_ref() else {
            return self.root_path.clone();
        };
        let is_dir = self
            .flat_cache
            .as_ref()
            .and_then(|flat| flat.iter().find(|(entry, _)| entry.path == *path))
            .is_some_and(|(entry, _)| entry.is_dir());
        if is_dir {
            path.clone()
        } else {
            path.parent().unwrap_or(&self.root_path).to_path_buf()
        }
    }

    fn prompt_simple_error(&self, title: &str, message: &str, cx: &mut Context<Self>) {
        let title = title.to_string();
        let message = message.to_string();
        if let Some(window) = self.context_menu_window {
            cx.spawn(async move |_, cx| {
                if let Ok(receiver) = cx.update_window(window, |_, window, cx| {
                    window.prompt(PromptLevel::Warning, &title, Some(&message), &["确定"], cx)
                }) {
                    let _ = receiver.await;
                }
            })
            .detach();
        }
    }

    fn open_selected_entries(&self, cx: &mut Context<Self>) {
        let selected = self.selected_operation_paths();
        if let Some(flat) = &self.flat_cache {
            for (entry, _) in flat
                .iter()
                .filter(|(entry, _)| selected.contains(&entry.path) && !entry.is_dir())
            {
                cx.emit(FileTreeEvent::OpenFile {
                    path: entry.path.clone(),
                    ssh_config: self.event_ssh_config(),
                });
            }
        }
    }

    fn compare_selected_entries(&self, cx: &mut Context<Self>) {
        let selected = self.selected_operation_paths();
        let files: Vec<PathBuf> = self
            .flat_cache
            .as_ref()
            .into_iter()
            .flat_map(|flat| flat.iter())
            .filter(|(entry, _)| {
                selected.contains(&entry.path) && entry.kind == FileEntryKind::File
            })
            .map(|(entry, _)| entry.path.clone())
            .collect();
        if let [left, right] = files.as_slice() {
            cx.emit(FileTreeEvent::CompareFiles {
                left: left.clone(),
                right: right.clone(),
                ssh_config: self.event_ssh_config(),
            });
        }
    }

    // ------------------------------------------------------------------
    // 下载（仅远端）
    // ------------------------------------------------------------------

    /// 远端条目下载到本地：选择目录 → StartTransfer（复用统一传输链路）。
    pub(super) fn start_download(
        &mut self,
        entry: &FileEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_editing(cx);
        let Some(ssh_config) = self.loader.ssh_config().cloned() else {
            return;
        };
        let mut paths = self.selected_operation_paths();
        if paths.is_empty() {
            paths.push(entry.path.clone());
        }
        let entry_name = if paths.len() == 1 {
            entry.name.clone()
        } else {
            format!("{} 个条目", paths.len())
        };
        let sources = paths
            .iter()
            .map(|path| FileEndpoint::Remote {
                ssh_config: ssh_config.clone(),
                path: path.to_string_lossy().into_owned(),
            })
            .collect();

        let picker = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(format!("选择「{entry_name}」的下载位置").into()),
        });

        cx.spawn(async move |this, cx| {
            let destination_dir = match picker.await {
                Ok(Ok(Some(paths))) if !paths.is_empty() => paths[0].clone(),
                _ => return,
            };
            let _ = this.update(cx, |_, cx| {
                let request = TransferRequest {
                    sources,
                    destination: FileEndpoint::Local {
                        path: destination_dir,
                    },
                    operation: TransferOperation::Copy,
                    conflict_policy: ConflictPolicy::Ask,
                    rename_to: None,
                };
                cx.emit(FileTreeEvent::StartTransfer {
                    request: Box::new(request),
                });
            });
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // 本地文件拖入（ExternalPaths → 上传 / 复制）
    // ------------------------------------------------------------------

    /// 拖入目标文件夹（OS 文件拖到树行 / 树空白区）：构建传输请求并发出。
    pub(super) fn drop_external_paths(
        &mut self,
        paths: &[std::path::PathBuf],
        target_dir: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let sources: Vec<FileEndpoint> = paths
            .iter()
            .map(|p| FileEndpoint::Local { path: p.clone() })
            .collect();
        let destination = if self.loader.is_local() {
            FileEndpoint::Local {
                path: target_dir.to_path_buf(),
            }
        } else {
            let Some(config) = self.loader.ssh_config().cloned() else {
                return;
            };
            FileEndpoint::Remote {
                ssh_config: config,
                path: target_dir.to_string_lossy().to_string(),
            }
        };
        let request = TransferRequest {
            sources,
            destination,
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::Ask,
            rename_to: None,
        };
        cx.emit(FileTreeEvent::StartTransfer {
            request: Box::new(request),
        });
        // 不在这里刷新：此刻传输才刚发起，刷新必然读到旧内容（等于空转）。
        // 目录刷新统一由 `WorkspaceEvent::TransferFinished` 在传输结束后触发。
    }

    // ------------------------------------------------------------------
    // 行内编辑行渲染
    // ------------------------------------------------------------------

    /// 渲染行内编辑行（新建合成行 / 重命名行）：输入框内联替换条目名。
    pub(super) fn render_editing_row(
        &mut self,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let Some(editing) = self.editing.as_ref() else {
            return div().into_any_element();
        };
        let input = editing.input.clone();
        let error = editing.error.clone();
        let icon = match &editing.mode {
            TreeEditMode::Create { kind, .. } => {
                if *kind == FileEntryKind::Directory {
                    crate::assets::Icon::Folder
                } else {
                    crate::assets::Icon::File
                }
            }
            TreeEditMode::Rename { path, .. } => {
                if path_is_directory_cached(self, path) {
                    crate::assets::Icon::Folder
                } else {
                    crate::assets::Icon::File
                }
            }
        };
        let indent = px(depth as f32 * INDENT_SIZE);

        div()
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .pl(px(4.0) + indent)
            .pr(px(4.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(1.0))
                    .child(div().w(px(12.0)).flex_none())
                    .child(
                        gpui::svg()
                            .size(px(14.0))
                            .text_color(theme.accent)
                            .path(icon.path()),
                    ),
            )
            .child(div().flex_1().min_w_0().overflow_hidden().child(
                // 行内紧凑输入：transparent 去掉输入框背景/内边距，融入树行
                EditorInput::new(&input).small().transparent(),
            ))
            .when_some(error, |row, error| {
                row.child(
                    div()
                        .id("file-tree-editing-error")
                        .w(px(120.0))
                        .flex_none()
                        .truncate()
                        .text_xs()
                        .text_color(theme.status_conflict)
                        .tooltip(crate::components::tooltip::Tooltip::text(error.clone()))
                        .child(error),
                )
            })
            // Esc 取消：InputState 的 Escape 会向上冒泡（未启用 clean_on_escape）
            .on_action(cx.listener(|this, _: &Escape, _window, cx| {
                this.cancel_editing(cx);
            }))
            .into_any_element()
    }
}

/// 路径前缀替换：`from`（或其后代）→ 同形态 `to` 路径。
fn reveal_local_path(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let result = crate::utils::console::hidden_command("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(|_| ());
    #[cfg(target_os = "windows")]
    let result = crate::utils::console::hidden_command("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .map(|_| ());
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result = open::that(path.parent().unwrap_or(path)).map(|_| ());

    if let Err(error) = result {
        log::warn!("无法打开系统文件管理器 {}: {error}", path.display());
    }
}

pub(crate) fn rekey_path(
    path: &std::path::Path,
    from: &std::path::Path,
    to: &std::path::Path,
) -> std::path::PathBuf {
    if path == from {
        return to.to_path_buf();
    }
    match path.strip_prefix(from) {
        Ok(rest) => to.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// 占位名去重：基础名被占用时追加序号（新建文件 → 新建文件 2 → …）。
pub(crate) fn unique_placeholder_name(base: &str, existing: &[String]) -> String {
    if !existing.iter().any(|n| n == base) {
        return base.to_string();
    }
    for index in 2.. {
        let candidate = format!("{base} {index}");
        if !existing.iter().any(|n| n == &candidate) {
            return candidate;
        }
    }
    unreachable!("existing entries are finite");
}

/// 重命名行的图标兜底：从父目录缓存判断类型（缓存缺失按文件显示）。
fn path_is_directory_cached(tree: &SidebarFileTree, path: &std::path::Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    tree.loader
        .get_cached(parent)
        .and_then(|entries| entries.iter().find(|e| e.path == path))
        .is_some_and(|e| e.kind == FileEntryKind::Directory)
}

// ------------------------------------------------------------------
// 右键菜单（手动模式：按目标类型构建，避免嵌套 ContextMenuExt 双开）
// ------------------------------------------------------------------

impl SidebarFileTree {
    /// 打开右键菜单：构建 PopupMenu 实体 + 记录弹出位置 + 订阅关闭事件。
    fn open_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu,
    ) {
        self.close_context_menu(cx);
        let menu = crate::components::menu::PopupMenu::build(window, cx, build);
        // 聚焦菜单：键盘上下选择 / Enter 确认 / Esc 关闭才生效（与 ContextMenuExt 行为一致）
        menu.focus_handle(cx).focus(window, cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &gpui::DismissEvent, cx| {
            // 菜单自行关闭（Esc / 点到别处）：焦点还挂在菜单上就还给文件树。
            this.close_context_menu_and_refocus(cx);
        });
        self.context_menu = Some(menu);
        self.context_menu_position = Some(position);
        self.context_menu_subscription = Some(subscription);
        self.context_menu_window = Some(window.window_handle());
        cx.notify();
    }

    /// 关闭菜单（点了菜单项等情况）：不碰焦点，避免抢走下一步该聚焦的元素
    /// （例如「新建文件」紧接着要把焦点给行内输入框）。
    pub(super) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        self.close_context_menu_inner(false, cx);
    }

    fn close_context_menu_and_refocus(&mut self, cx: &mut Context<Self>) {
        self.close_context_menu_inner(true, cx);
    }

    fn close_context_menu_inner(&mut self, restore_focus: bool, cx: &mut Context<Self>) {
        let menu = self.context_menu.take();
        self.context_menu_position = None;
        self.context_menu_subscription.take();
        let window_handle = self.context_menu_window.take();

        if restore_focus && let (Some(menu), Some(handle)) = (menu, window_handle) {
            let menu_focus = menu.focus_handle(cx).clone();
            let tree_focus = self.focus_handle.clone();
            cx.spawn(async move |_, cx| {
                let _ = cx.update_window(handle, |_, window, cx| {
                    // 只有焦点还在菜单上（Esc 关闭）才还回去；
                    // 用户点到了别处（终端 / 编辑器）就绝不能抢焦点。
                    if menu_focus.is_focused(window) {
                        window.focus(&tree_focus, cx);
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// 右键菜单浮层渲染（deferred + anchored，窗口边缘自适应）。
    pub(super) fn render_context_menu_overlay(&self) -> Option<gpui::AnyElement> {
        let menu = self.context_menu.clone()?;
        let position = self.context_menu_position.unwrap_or_default();
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(position)
                    .anchor(gpui::Anchor::TopLeft)
                    .snap_to_window_with_margin(px(8.0))
                    .child(menu),
            )
            .with_priority(100)
            .into_any_element(),
        )
    }

    /// 条目右键菜单：单选提供完整文件操作，多选仅保留有意义的批量动作。
    pub(super) fn open_context_menu_for_entry(
        &mut self,
        entry: &FileEntry,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tree = cx.entity();
        let entry = entry.clone();
        let is_remote = !self.loader.is_local();
        let is_dir = entry.is_dir();
        let multi = self.selected_paths.len() > 1;
        let can_compare = self.selected_paths.len() == 2
            && self
                .flat_cache
                .as_ref()
                .into_iter()
                .flat_map(|flat| flat.iter())
                .filter(|(candidate, _)| self.selected_paths.contains(&candidate.path))
                .filter(|(candidate, _)| candidate.kind == FileEntryKind::File)
                .count()
                == 2;
        let can_paste = self.has_file_clipboard();
        let event_ssh_config = self.event_ssh_config();
        let paste_target = if is_dir {
            entry.path.clone()
        } else {
            entry.path.parent().unwrap_or(&self.root_path).to_path_buf()
        };

        self.open_menu(position, window, cx, move |menu, _window, _cx| {
            let mut menu = menu.item(tree_menu_action(
                if multi { "打开所选项" } else { "打开" },
                "icons/go.to.file.svg",
                tree.clone(),
                |this, _, cx| this.open_selected_entries(cx),
            ));
            if can_compare {
                menu = menu.item(tree_menu_action(
                    "比较所选文件",
                    "icons/git.svg",
                    tree.clone(),
                    |this, _, cx| this.compare_selected_entries(cx),
                ));
            }
            menu = menu.separator();

            if is_dir && !multi {
                for (label, icon, kind) in [
                    ("新建文件", "icons/new_file.svg", FileEntryKind::File),
                    (
                        "新建文件夹",
                        "icons/new_folder.svg",
                        FileEntryKind::Directory,
                    ),
                ] {
                    let parent = entry.path.clone();
                    menu = menu.item(tree_menu_action(
                        label,
                        icon,
                        tree.clone(),
                        move |this, window, cx| {
                            this.start_create(parent.clone(), kind.clone(), window, cx)
                        },
                    ));
                }
                menu = menu.separator();
            }

            if !is_remote {
                let path = entry.path.clone();
                menu = menu.item(tree_menu_action(
                    "在系统文件管理器中显示",
                    "icons/reveal.svg",
                    tree.clone(),
                    move |_, _, _| reveal_local_path(&path),
                ));
            }
            let terminal_path = if is_dir {
                entry.path.clone()
            } else {
                entry.path.parent().unwrap_or(&entry.path).to_path_buf()
            };
            let terminal_config = event_ssh_config.clone();
            menu = menu.item(tree_menu_action(
                "在终端中打开",
                "icons/terminal.svg",
                tree.clone(),
                move |_, _, cx| {
                    cx.emit(FileTreeEvent::OpenInTerminal {
                        path: terminal_path.clone(),
                        ssh_config: terminal_config.clone(),
                        run_script: false,
                    });
                },
            ));
            if is_dir && !is_remote {
                let path = entry.path.clone();
                menu = menu.item(tree_menu_action(
                    "在此目录中搜索",
                    "icons/magnifying_glass.svg",
                    tree.clone(),
                    move |_, _, cx| {
                        cx.emit(FileTreeEvent::FindInFolder { path: path.clone() });
                    },
                ));
            }
            if !is_dir && !multi && !is_remote {
                let path = entry.path.clone();
                let config = event_ssh_config.clone();
                menu = menu.item(tree_menu_action(
                    "在终端中运行",
                    "icons/terminal.svg",
                    tree.clone(),
                    move |_, _, cx| {
                        cx.emit(FileTreeEvent::OpenInTerminal {
                            path: path.clone(),
                            ssh_config: config.clone(),
                            run_script: true,
                        });
                    },
                ));
            }
            menu = menu.separator();

            menu = menu
                .item(tree_menu_action(
                    "剪切",
                    "icons/scissors.svg",
                    tree.clone(),
                    |this, _, cx| this.copy_selection_to_clipboard(true, cx),
                ))
                .item(tree_menu_action(
                    "复制",
                    "icons/copy.svg",
                    tree.clone(),
                    |this, _, cx| this.copy_selection_to_clipboard(false, cx),
                ));
            if can_paste && !multi {
                let target = paste_target.clone();
                menu = menu.item(tree_menu_action(
                    "粘贴",
                    "icons/document.on.document.svg",
                    tree.clone(),
                    move |this, _, cx| this.paste_into_tree(target.clone(), cx),
                ));
            }
            if is_remote {
                let download_entry = entry.clone();
                menu = menu.item(tree_menu_action(
                    if multi {
                        "下载所选项到本地…"
                    } else {
                        "下载到本地…"
                    },
                    "icons/download.svg",
                    tree.clone(),
                    move |this, window, cx| this.start_download(&download_entry, window, cx),
                ));
            }
            menu = menu.separator();
            menu = menu
                .item(tree_menu_action(
                    "复制路径",
                    "icons/copy.svg",
                    tree.clone(),
                    |this, _, cx| this.copy_selected_paths(false, cx),
                ))
                .item(tree_menu_action(
                    "复制相对路径",
                    "icons/copy.svg",
                    tree.clone(),
                    |this, _, cx| this.copy_selected_paths(true, cx),
                ))
                .separator();

            if !multi {
                let rename_entry = entry.clone();
                menu = menu.item(tree_menu_action(
                    "重命名",
                    "icons/pencil.svg",
                    tree.clone(),
                    move |this, window, cx| this.start_rename(&rename_entry, window, cx),
                ));
            }
            let delete_entry = entry.clone();
            menu = menu.item(tree_menu_action(
                if is_remote {
                    "永久删除"
                } else if multi {
                    "将所选项移到废纸篓"
                } else {
                    "移到废纸篓"
                },
                "icons/trash_zed.svg",
                tree.clone(),
                move |this, window, cx| this.start_delete(&delete_entry, window, cx),
            ));
            if !is_remote {
                let delete_entry = entry.clone();
                menu = menu.item(tree_menu_action(
                    "永久删除…",
                    "icons/trash.svg",
                    tree.clone(),
                    move |this, window, cx| this.start_permanent_delete(&delete_entry, window, cx),
                ));
            }
            menu
        });
    }

    /// 根目录与空白区菜单：根本身不可改名/删除，但可作为所有目录操作的目标。
    pub(super) fn open_context_menu_for_root(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tree = cx.entity();
        let root = self.root_path.clone();
        let is_remote = !self.loader.is_local();
        let can_paste = self.has_file_clipboard();
        let compact = self.compact_folders;
        let event_ssh_config = self.event_ssh_config();
        self.open_menu(position, window, cx, move |menu, _window, _cx| {
            let mut menu = menu;
            for (label, icon, kind) in [
                ("新建文件", "icons/new_file.svg", FileEntryKind::File),
                (
                    "新建文件夹",
                    "icons/new_folder.svg",
                    FileEntryKind::Directory,
                ),
            ] {
                let parent = root.clone();
                menu = menu.item(tree_menu_action(
                    label,
                    icon,
                    tree.clone(),
                    move |this, window, cx| {
                        this.start_create(parent.clone(), kind.clone(), window, cx)
                    },
                ));
            }
            if !is_remote {
                menu = menu.item(tree_menu_action(
                    "添加文件夹到工作区…",
                    "icons/folder.badge.plus.svg",
                    tree.clone(),
                    |this, window, cx| this.add_workspace_folder_prompt(window, cx),
                ));
            }
            menu = menu.separator();
            if !is_remote {
                let path = root.clone();
                menu = menu.item(tree_menu_action(
                    "在系统文件管理器中显示",
                    "icons/reveal.svg",
                    tree.clone(),
                    move |_, _, _| reveal_local_path(&path),
                ));
            }
            let terminal_path = root.clone();
            let config = event_ssh_config.clone();
            menu = menu.item(tree_menu_action(
                "在终端中打开",
                "icons/terminal.svg",
                tree.clone(),
                move |_, _, cx| {
                    cx.emit(FileTreeEvent::OpenInTerminal {
                        path: terminal_path.clone(),
                        ssh_config: config.clone(),
                        run_script: false,
                    });
                },
            ));
            if !is_remote {
                let path = root.clone();
                menu = menu.item(tree_menu_action(
                    "在此目录中搜索",
                    "icons/magnifying_glass.svg",
                    tree.clone(),
                    move |_, _, cx| {
                        cx.emit(FileTreeEvent::FindInFolder { path: path.clone() });
                    },
                ));
            }
            menu = menu.separator();
            if can_paste {
                let target = root.clone();
                menu = menu.item(tree_menu_action(
                    "粘贴",
                    "icons/document.on.document.svg",
                    tree.clone(),
                    move |this, _, cx| this.paste_into_tree(target.clone(), cx),
                ));
            }
            let copy_root = root.clone();
            menu = menu.item(tree_menu_action(
                "复制路径",
                "icons/copy.svg",
                tree.clone(),
                move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        copy_root.to_string_lossy().into_owned(),
                    ));
                },
            ));
            menu = menu
                .separator()
                .item(tree_menu_action(
                    if compact {
                        "逐级显示目录"
                    } else {
                        "合并连续单子目录"
                    },
                    "icons/folder.svg",
                    tree.clone(),
                    |this, _, cx| this.toggle_compact_folders(cx),
                ))
                .item(tree_menu_action(
                    "刷新",
                    "icons/arrow.clockwise.svg",
                    tree.clone(),
                    |this, _, cx| this.refresh_all_loaded(cx),
                ))
                .item(tree_menu_action(
                    "折叠全部",
                    "icons/collapse.svg",
                    tree.clone(),
                    |this, _, cx| this.collapse_all(cx),
                ));
            menu
        });
    }

    pub(super) fn open_context_menu_for_extra_root(
        &mut self,
        root: PathBuf,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tree = cx.entity();
        self.open_menu(position, window, cx, move |menu, _, _| {
            menu.item(tree_menu_action(
                "从工作区移除（不删除文件）",
                "icons/trash.svg",
                tree,
                move |this, _, cx| this.remove_extra_root(&root, cx),
            ))
        });
    }

    /// 空白区域右键菜单：新建文件 / 新建文件夹 / 折叠全部（在根目录创建）。
    pub(super) fn open_context_menu_for_empty_area(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_context_menu_for_root(position, window, cx);
    }

    // ------------------------------------------------------------------
    // 树内拖拽移动（DraggedTreeEntry → Move）
    // ------------------------------------------------------------------

    /// 树内条目拖入目标目录：多选移动，按住 Option/Alt 松开则复制。
    /// 拖到自身、后代或原地移动均视为无效，不产生传输任务。
    pub(super) fn drop_tree_entry(
        &mut self,
        dragged: &DraggedTreeEntry,
        target_dir: &std::path::Path,
        copy: bool,
        cx: &mut Context<Self>,
    ) {
        self.clear_drag_expand();
        if !can_drop_tree_entry(dragged, target_dir, copy) {
            return;
        }
        let paths = if dragged.paths.is_empty() {
            vec![dragged.path.clone()]
        } else {
            dragged.paths.clone()
        };
        let sources: Vec<FileEndpoint> = paths
            .iter()
            .filter_map(|path| self.endpoint_for_path(path))
            .collect();
        let Some(destination) = self.endpoint_for_path(target_dir) else {
            return;
        };
        let request = TransferRequest {
            sources,
            destination,
            operation: if copy {
                TransferOperation::Copy
            } else {
                TransferOperation::Move
            },
            conflict_policy: ConflictPolicy::Ask,
            rename_to: None,
        };
        cx.emit(FileTreeEvent::StartTransfer {
            request: Box::new(request),
        });
        // 刷新和真实移动路径同步由 TransferFinished 统一完成。
    }
}

// ------------------------------------------------------------------
// 树内拖拽负载 + 拖拽预览
// ------------------------------------------------------------------

/// 树内拖拽负载：行 `on_drag` 携带，目录行 `on_drop` 接收。
pub(crate) fn can_drop_tree_entry(
    dragged: &DraggedTreeEntry,
    target_dir: &std::path::Path,
    copy: bool,
) -> bool {
    let paths = if dragged.paths.is_empty() {
        std::slice::from_ref(&dragged.path)
    } else {
        dragged.paths.as_slice()
    };
    !paths.iter().any(|path| target_dir.starts_with(path))
        && (copy || !paths.iter().all(|path| path.parent() == Some(target_dir)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraggedTreeEntry {
    pub path: std::path::PathBuf,
    pub paths: Vec<std::path::PathBuf>,
    pub name: String,
    pub is_dir: bool,
}

impl gpui::Render for DraggedTreeEntry {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let icon_path = if self.is_dir {
            crate::assets::Icon::Folder.path()
        } else {
            crate::assets::file_icon_or_default(&self.path).path()
        };
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(6.0))
            .bg(theme.elevated_background.opacity(0.95))
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .child(
                gpui::svg()
                    .size(px(13.0))
                    .text_color(if self.is_dir {
                        theme.accent
                    } else {
                        theme.icon_muted
                    })
                    .path(icon_path),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.text)
                    .line_height(relative(1.2))
                    .max_w(px(200.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(self.name.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rekey_path_replaces_prefix_and_descendants() {
        use std::path::Path;
        let from = Path::new("/a/b");
        let to = Path::new("/a/c");
        // 断言用 `Path` 而非 `to_string_lossy()`：`Path::join` 在 windows 上
        // 用 `\` 拼回，字符串比对会把平台差异误判成逻辑错误。
        // 自身
        assert_eq!(rekey_path(Path::new("/a/b"), from, to), Path::new("/a/c"));
        // 后代多层
        assert_eq!(
            rekey_path(Path::new("/a/b/x/y.md"), from, to),
            to.join("x").join("y.md")
        );
        // 无关路径原样返回
        assert_eq!(
            rekey_path(Path::new("/a/bb/other"), from, to),
            Path::new("/a/bb/other")
        );
        // 前缀不是按路径层级匹配的：/a/b2 不是 /a/b 的后代
        assert_eq!(rekey_path(Path::new("/a/b2"), from, to), Path::new("/a/b2"));
    }

    #[test]
    fn create_retry_names_number_files_and_folders() {
        // 文件：序号插在扩展名之前（a.txt → a 2.txt）
        assert_eq!(
            next_candidate_name(std::path::Path::new("/p/a.txt"), 1, false),
            std::path::PathBuf::from("/p/a 2.txt")
        );
        assert_eq!(
            next_candidate_name(std::path::Path::new("/p/a.txt"), 2, false),
            std::path::PathBuf::from("/p/a 3.txt")
        );
        // 无扩展名的文件 / 目录：直接追加
        assert_eq!(
            next_candidate_name(std::path::Path::new("/p/plain"), 1, false),
            std::path::PathBuf::from("/p/plain 2")
        );
        assert_eq!(
            next_candidate_name(std::path::Path::new("/p/dir"), 1, true),
            std::path::PathBuf::from("/p/dir 2")
        );
        // 目录里的点号不是扩展名（不能变成 "my 2.dir"）
        assert_eq!(
            next_candidate_name(std::path::Path::new("/p/my.dir"), 1, true),
            std::path::PathBuf::from("/p/my.dir 2")
        );
        // 点开头的文件（.gitignore）没有主名，整体追加
        assert_eq!(
            next_candidate_name(std::path::Path::new("/p/.gitignore"), 1, false),
            std::path::PathBuf::from("/p/.gitignore 2")
        );
    }

    #[test]
    fn unique_placeholder_name_appends_counter() {
        assert_eq!(unique_placeholder_name("新建文件", &[]), "新建文件");
        let taken = vec!["新建文件".to_string()];
        assert_eq!(unique_placeholder_name("新建文件", &taken), "新建文件 2");
        let taken = vec!["新建文件".to_string(), "新建文件 2".to_string()];
        assert_eq!(unique_placeholder_name("新建文件", &taken), "新建文件 3");
        // 无冲突时占位名不同则原样返回
        assert_eq!(
            unique_placeholder_name("src", &["other".to_string()]),
            "src"
        );
    }

    #[test]
    fn create_path_allows_nested_names_without_traversal() {
        assert!(validate_create_path("a/b/c.txt").is_ok());
        assert!(validate_create_path("单个.txt").is_ok());
        for invalid in ["", "../escape", "a/../../escape", "/absolute", "a/", "."] {
            assert!(
                validate_create_path(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn drag_validation_handles_batches_and_copy() {
        let dragged = DraggedTreeEntry {
            path: PathBuf::from("/work/a"),
            paths: vec![PathBuf::from("/work/a"), PathBuf::from("/work/b")],
            name: "2 个条目".into(),
            is_dir: true,
        };
        assert!(!can_drop_tree_entry(
            &dragged,
            Path::new("/work/a/child"),
            false
        ));
        assert!(!can_drop_tree_entry(&dragged, Path::new("/work"), false));
        assert!(can_drop_tree_entry(&dragged, Path::new("/work"), true));
        assert!(can_drop_tree_entry(
            &dragged,
            Path::new("/elsewhere"),
            false
        ));
    }
}
