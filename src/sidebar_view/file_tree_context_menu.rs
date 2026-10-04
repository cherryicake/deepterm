//! 文件树右键菜单与文件操作（新建 / 重命名 / 删除 / 下载 / 拖拽上传）。
//!
//! 菜单挂载在各行（参考 VSCode / Zed 项目面板）：`context_menu()` 在右键
//! 打开时按行捕获的数据构建菜单项。新建与重命名采用树上行内编辑
//! （`InputState` 内联在行里，Enter 提交、Esc 取消、失焦提交，与 VSCode 一致）。
//! 所有文件系统写操作走 `FileSystem` trait 的阻塞方法（后台线程执行），
//! 完成后刷新受影响目录并同步展开 / 选中 / 监听状态。

use gpui::prelude::*;
use gpui::{Context, Entity, Focusable, PromptLevel, Styled, Window, div, px, relative};

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
                // 非法名：保留编辑行让用户修正（Enter 无反应），Esc 可退出
                if validate_rename_to(&name).is_err() {
                    log::warn!("新建名称不合法: {name:?}");
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
                if validate_rename_to(&name).is_err() {
                    log::warn!("重命名名称不合法: {name:?}");
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
                    this.prompt_operation_error("重命名失败", err, window_handle, cx);
                }
            });
        })
        .detach();
    }

    /// 删除入口（菜单）：确认对话框 → 后台删除 → 状态清理。
    pub(super) fn start_delete(
        &mut self,
        entry: &FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_editing(cx);
        let path = entry.path.clone();
        let is_dir = entry.kind == FileEntryKind::Directory;
        let name = entry.name.clone();
        let backend = self.loader.backend().clone();
        let root_epoch = self.root_epoch;
        let window_handle = window.window_handle();

        let detail = if is_dir {
            format!("将递归删除文件夹「{name}」及其全部内容，此操作不可撤销。")
        } else {
            format!("将删除文件「{name}」，此操作不可撤销。")
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            "删除",
            Some(&detail),
            &["删除", "取消"],
            cx,
        );

        cx.spawn(async move |this, cx| {
            let Ok(choice) = prompt.await else {
                return;
            };
            if choice != 0 {
                return;
            }
            let removed_after = path.clone();
            let path_in_op = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { backend.remove_path(&path_in_op) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    if this.root_epoch == root_epoch {
                        this.apply_path_removed(&removed_after, cx);
                    }
                }
                Err(err) => {
                    log::warn!("删除失败 {}: {err}", path.display());
                    this.prompt_operation_error("删除失败", err, window_handle, cx);
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
    fn apply_path_renamed(
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
        if let Some(selected) = self.selected_path.clone() {
            if selected.starts_with(from) {
                self.selected_path = Some(rekey_path(&selected, from, to));
            }
        }
        if let Some(hovered) = self.hovered_path.clone() {
            if hovered.starts_with(from) {
                self.hovered_path = Some(rekey_path(&hovered, from, to));
            }
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
            ssh_config: self.loader.ssh_config().cloned().map(Box::new),
        });
        self.invalidate_flat_cache();
        cx.notify();
    }

    /// 删除成功：清理展开 / 选中 / 监听 / 缓存，刷新父目录。
    /// 本地树 watcher 事件也会执行同样清理（幂等）；远端无事件由这里兜底。
    fn apply_path_removed(&mut self, removed: &std::path::Path, cx: &mut Context<Self>) {
        self.expanded_folders.retain(|p| !p.starts_with(removed));
        if self
            .selected_path
            .as_ref()
            .is_some_and(|p| p.starts_with(removed))
        {
            self.selected_path = None;
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
            ssh_config: self.loader.ssh_config().cloned().map(Box::new),
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
        let remote_path = entry.path.to_string_lossy().to_string();
        let entry_name = entry.name.clone();

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
                    sources: vec![FileEndpoint::Remote {
                        ssh_config,
                        path: remote_path,
                    }],
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
            // Esc 取消：InputState 的 Escape 会向上冒泡（未启用 clean_on_escape）
            .on_action(cx.listener(|this, _: &Escape, _window, cx| {
                this.cancel_editing(cx);
            }))
            .into_any_element()
    }
}

/// 路径前缀替换：`from`（或其后代）→ 同形态 `to` 路径。
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

        if restore_focus {
            if let (Some(menu), Some(handle)) = (menu, window_handle) {
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

    /// 条目右键菜单（VSCode 区分文件 / 文件夹）：
    /// - 文件：下载（远端）/ 重命名 / 删除 —— 没有新建项
    /// - 文件夹：下载（远端）/ 新建文件 / 新建文件夹 / 重命名 / 删除
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
        let is_dir = entry.kind == FileEntryKind::Directory;
        self.open_menu(position, window, cx, move |menu, _window, _cx| {
            let mut menu = menu;

            // 下载到本地（仅远端）
            if is_remote {
                let tree_dl = tree.clone();
                let dl_entry = entry.clone();
                menu = menu.item(
                    PopupMenuItem::new("下载到本地…")
                        .icon(crate::components::button::Icon::new("icons/download.svg"))
                        .on_click(move |_, window, cx| {
                            tree_dl.update(cx, |this, cx| {
                                this.close_context_menu(cx);
                                this.start_download(&dl_entry, window, cx);
                            });
                        }),
                );
                menu = menu.separator();
            }

            // 新建（仅文件夹：在该文件夹内创建）
            if is_dir {
                let tree_file = tree.clone();
                let parent_file = entry.path.clone();
                menu = menu.item(
                    PopupMenuItem::new("新建文件")
                        .icon(crate::components::button::Icon::new("icons/new_file.svg"))
                        .on_click(move |_, window, cx| {
                            tree_file.update(cx, |this, cx| {
                                this.close_context_menu(cx);
                                this.start_create(
                                    parent_file.clone(),
                                    FileEntryKind::File,
                                    window,
                                    cx,
                                );
                            });
                        }),
                );
                let tree_dir = tree.clone();
                let parent_dir = entry.path.clone();
                menu = menu.item(
                    PopupMenuItem::new("新建文件夹")
                        .icon(crate::components::button::Icon::new("icons/new_folder.svg"))
                        .on_click(move |_, window, cx| {
                            tree_dir.update(cx, |this, cx| {
                                this.close_context_menu(cx);
                                this.start_create(
                                    parent_dir.clone(),
                                    FileEntryKind::Directory,
                                    window,
                                    cx,
                                );
                            });
                        }),
                );
                menu = menu.separator();
            }

            // 重命名 / 删除
            let tree_rename = tree.clone();
            let rename_entry = entry.clone();
            menu = menu.item(
                PopupMenuItem::new("重命名")
                    .icon(crate::components::button::Icon::new("icons/pencil.svg"))
                    .on_click(move |_, window, cx| {
                        tree_rename.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.start_rename(&rename_entry, window, cx);
                        });
                    }),
            );
            let tree_delete = tree.clone();
            let delete_entry = entry.clone();
            menu = menu.item(
                PopupMenuItem::new(if is_dir { "删除文件夹" } else { "删除" })
                    .icon(crate::components::button::Icon::new("icons/trash_zed.svg"))
                    .on_click(move |_, window, cx| {
                        tree_delete.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.start_delete(&delete_entry, window, cx);
                        });
                    }),
            );

            menu
        });
    }

    /// 根目录右键菜单：新建文件 / 新建文件夹 / 折叠全部（根不可重命名 / 删除）。
    pub(super) fn open_context_menu_for_root(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tree = cx.entity();
        let root = self.root_path.clone();
        self.open_menu(position, window, cx, move |menu, _window, _cx| {
            let tree_file = tree.clone();
            let root_file = root.clone();
            let tree_dir = tree.clone();
            let root_dir = root.clone();
            let tree_collapse = tree.clone();
            menu.item(
                PopupMenuItem::new("新建文件")
                    .icon(crate::components::button::Icon::new("icons/new_file.svg"))
                    .on_click(move |_, window, cx| {
                        tree_file.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.start_create(root_file.clone(), FileEntryKind::File, window, cx);
                        });
                    }),
            )
            .item(
                PopupMenuItem::new("新建文件夹")
                    .icon(crate::components::button::Icon::new("icons/new_folder.svg"))
                    .on_click(move |_, window, cx| {
                        tree_dir.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.start_create(
                                root_dir.clone(),
                                FileEntryKind::Directory,
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .separator()
            .item(
                PopupMenuItem::new("折叠全部")
                    .icon(crate::components::button::Icon::new("icons/collapse.svg"))
                    .on_click(move |_, _window, cx| {
                        tree_collapse.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.collapse_all(cx);
                        });
                    }),
            )
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

    /// 树内条目拖入目标目录：构建 Move 传输请求（复用统一链路的进度 / 冲突 / 取消）。
    /// 拖到自身或其后代目录无效（VSCode 同款限制）。
    pub(super) fn drop_tree_entry(
        &mut self,
        dragged: &DraggedTreeEntry,
        target_dir: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        // 自拖自 / 拖入后代目录：无效
        if target_dir.starts_with(&dragged.path) {
            log::debug!(
                "忽略无效拖拽：{} → {}（自身或后代）",
                dragged.path.display(),
                target_dir.display()
            );
            return;
        }
        let source = if self.loader.is_local() {
            FileEndpoint::Local {
                path: dragged.path.clone(),
            }
        } else {
            let Some(config) = self.loader.ssh_config().cloned() else {
                return;
            };
            FileEndpoint::Remote {
                ssh_config: config,
                path: dragged.path.to_string_lossy().to_string(),
            }
        };
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
            sources: vec![source],
            destination,
            operation: TransferOperation::Move,
            conflict_policy: ConflictPolicy::Ask,
            rename_to: None,
        };
        cx.emit(FileTreeEvent::StartTransfer {
            request: Box::new(request),
        });
        // 同上：刷新交给传输结束事件，Move 的「目标目录 + 源父目录」都会覆盖到。
    }
}

// ------------------------------------------------------------------
// 树内拖拽负载 + 拖拽预览
// ------------------------------------------------------------------

/// 树内拖拽负载：行 `on_drag` 携带，目录行 `on_drop` 接收。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraggedTreeEntry {
    pub path: std::path::PathBuf,
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
}
