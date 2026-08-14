use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, Context, CursorStyle, Element, ElementId, Entity,
    GlobalElementId, IntoElement, KeyBinding, ParentElement, Pixels, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, actions, div, px,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputState};
use crate::components::tab_bar::{DraggedTab, TabBar, TabItem};
use crate::editor::image_viewer::{ActualSize, FitToView, ResetZoom, ZoomIn, ZoomOut};
use crate::file_icons::file_icon_or_default;
use crate::file_manager::filesystem::RemoteFileSystem;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;

const EDITOR_DEFAULT_WIDTH: f32 = 680.0;
const EDITOR_MIN_WIDTH: f32 = 200.0;

actions!(editor_panel, [CloseActiveEditorTab, SaveActiveEditorTab]);
actions!(
    editor_tab_context_menu,
    [
        CloseTab,
        CloseOtherTabs,
        CloseRightTabs,
        CloseLeftTabs,
        CloseAllTabs,
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-w", CloseActiveEditorTab, Some("EditorPanel")),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-s", SaveActiveEditorTab, Some("EditorPanel")),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-s", SaveActiveEditorTab, Some("EditorPanel")),
        KeyBinding::new("cmd-+", ZoomIn, Some("ImageViewer")),
        KeyBinding::new("cmd-=", ZoomIn, Some("ImageViewer")),
        KeyBinding::new("cmd--", ZoomOut, Some("ImageViewer")),
        KeyBinding::new("cmd-0", ActualSize, Some("ImageViewer")),
        KeyBinding::new("cmd-9", FitToView, Some("ImageViewer")),
        KeyBinding::new("cmd-1", ResetZoom, Some("ImageViewer")),
    ]);
}

struct EditorFile {
    id: SharedString,
    path: String,
    label: String,
    ssh_config: Option<SshConnectionConfig>,
    editor_state: Entity<InputState>,
    original_content: String,
    pending_reload: bool,
    is_loading: bool,
    markdown: Option<Entity<crate::editor::markdown::Markdown>>,
    image: Option<Entity<crate::editor::image_viewer::ImageViewer>>,
    diff: Option<Entity<crate::editor::diff_view::DiffView>>,
}

/// 编辑器打开文件的简化信息，用于标签页组切换时保存
#[derive(Clone)]
pub struct EditorOpenFileInfo {
    pub id: SharedString,
    pub path: String,
    pub label: String,
    pub ssh_config: Option<SshConnectionConfig>,
    pub is_loading: bool,
}

/// 文件树标识符，用于区分不同的文件树（终端会话）
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct FileTreeKey(String);

impl FileTreeKey {
    pub fn new(
        path: Option<PathBuf>,
        is_local: bool,
        ssh_config: Option<SshConnectionConfig>,
    ) -> Self {
        let key = match (path, is_local, ssh_config) {
            (Some(path), true, _) => format!("local:{}", path.display()),
            (Some(path), false, Some(config)) => {
                format!(
                    "remote:{}@{}:{}{}",
                    config.username,
                    config.host,
                    config.port,
                    path.display()
                )
            }
            (None, true, _) => "local:".to_string(),
            (None, false, Some(config)) => {
                format!("remote:{}@{}:{}", config.username, config.host, config.port)
            }
            _ => "unknown".to_string(),
        };
        Self(key)
    }
}

/// 每个文件树的编辑器状态
pub struct FileTreeEditorState {
    pub open_files_info: Vec<EditorOpenFileInfo>,
    pub cached_editor_states: HashMap<SharedString, Entity<InputState>>,
    pub active_file_id: Option<SharedString>,
}

pub struct Editor {
    tab_bar: Entity<TabBar>,
    open_files: Vec<EditorFile>,
    active_file_id: Option<SharedString>,
    width: f32,
    resize_drag_x: Option<Pixels>,
    app_window: Option<WeakEntity<crate::AppWindow>>,
    tab_bar_pad_epoch: u32,
    tab_bar_pad_start: f32,
    tab_bar_pad_end: f32,
    tab_bar_pad_duration_ms: u64,
    content_left_padding: f32,
    content_bottom_padding: f32,
    watcher_subscription: Option<Subscription>,
    remote_watcher_subscription: Option<Subscription>,
    /// Subscriptions to each open editor's InputState so that state changes
    /// (hover popup, completion menu, search matches, cursor, …) re-render
    /// the Editor — the popups are rendered by EditorInput, not by the input
    /// element itself, so without this the UI never updates.
    editor_state_subscriptions: Vec<Subscription>,
    /// tab_id -> in-flight load generation (supersedes stale tasks after tab-group swap).
    loading_gens: HashMap<String, u64>,
    next_load_generation: u64,
}

impl Editor {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tab_bar = TabBar::new("editor-tab-bar", cx);

        let mut this = Self {
            tab_bar,
            open_files: Vec::new(),
            active_file_id: None,
            width: EDITOR_DEFAULT_WIDTH,
            resize_drag_x: None,
            app_window: None,
            tab_bar_pad_epoch: 0,
            tab_bar_pad_start: 6.0,
            tab_bar_pad_end: 6.0,
            tab_bar_pad_duration_ms: 0,
            content_left_padding: 0.0,
            content_bottom_padding: 0.0,
            watcher_subscription: None,
            remote_watcher_subscription: None,
            editor_state_subscriptions: Vec::new(),
            loading_gens: HashMap::new(),
            next_load_generation: 0,
        };

        this.setup_tab_bar_handlers(cx);
        this.setup_watcher(cx);
        this
    }

    pub fn bind_app_window(&mut self, app_window: WeakEntity<crate::AppWindow>) {
        self.app_window = Some(app_window);
    }

    pub fn is_empty(&self) -> bool {
        self.open_files.is_empty()
    }

    /// 获取当前活动文件 ID
    pub fn active_file_id(&self) -> Option<&SharedString> {
        self.active_file_id.as_ref()
    }

    /// 获取打开文件的基本信息列表，用于标签页组切换
    pub fn get_open_files_info(&self) -> Vec<EditorOpenFileInfo> {
        self.open_files
            .iter()
            .map(|f| EditorOpenFileInfo {
                id: f.id.clone(),
                path: f.path.clone(),
                label: f.label.clone(),
                ssh_config: f.ssh_config.clone(),
                is_loading: f.is_loading,
            })
            .collect()
    }

    /// 取出远程文件的 InputState entity，用于标签页组切换
    /// 本地文件返回 None（不需要缓存内容）
    pub fn take_cached_editor_states(&mut self) -> HashMap<SharedString, Entity<InputState>> {
        let mut cached_states = HashMap::new();
        for file in &mut self.open_files {
            // 缓存所有非加载中的文件，避免本地文件切换时重复从磁盘读取。
            // 图片文件不走文本 InputState 恢复，无需缓存。
            if !file.is_loading && file.image.is_none() {
                let state = file.editor_state.clone();
                cached_states.insert(file.id.clone(), state);
            }
        }
        cached_states
    }

    /// 保存当前编辑器状态到文件树状态
    pub fn save_to_file_tree_state(&mut self, cx: &mut Context<Self>) -> FileTreeEditorState {
        let open_files_info = self.get_open_files_info();
        let cached_editor_states = self.take_cached_editor_states();
        let active_file_id = self.active_file_id.clone();

        // 清空编辑器
        self.clear(cx);

        FileTreeEditorState {
            open_files_info,
            cached_editor_states,
            active_file_id,
        }
    }

    /// 从文件树状态恢复编辑器
    pub fn restore_from_file_tree_state(
        &mut self,
        state: FileTreeEditorState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_from_info(
            state.open_files_info,
            state.active_file_id,
            &state.cached_editor_states,
            window,
            cx,
        );
    }

    /// 从缓存恢复文件列表
    pub fn restore_from_info(
        &mut self,
        files_info: Vec<EditorOpenFileInfo>,
        active_file_id: Option<SharedString>,
        cached_states: &HashMap<SharedString, Entity<InputState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        for file_info in &files_info {
            let is_image = crate::editor::image_viewer::is_image_file(Path::new(&file_info.path));

            // 图片文件不能从文本 InputState 缓存恢复，必须重新加载二进制
            if is_image {
                if let Some(ref ssh_config) = file_info.ssh_config {
                    self.open_remote_file_no_notify(
                        file_info.path.clone(),
                        ssh_config.clone(),
                        window,
                        cx,
                    );
                } else {
                    self.open_file_no_notify(std::path::PathBuf::from(&file_info.path), window, cx);
                }
                continue;
            }

            // 检查是否有缓存的 InputState（远程文件）
            if let Some(cached_state) = cached_states.get(&file_info.id) {
                // 直接使用缓存的 InputState
                self.restore_file_from_cache(file_info, cached_state.clone(), window, cx);
            } else {
                // 本地文件或正在加载的文件，重新打开
                if let Some(ref ssh_config) = file_info.ssh_config {
                    self.open_remote_file_no_notify(
                        file_info.path.clone(),
                        ssh_config.clone(),
                        window,
                        cx,
                    );
                } else {
                    self.open_file_no_notify(std::path::PathBuf::from(&file_info.path), window, cx);
                }
            }
        }
        if let Some(ref active_id) = active_file_id {
            self.switch_to_file(active_id, cx);
        }
    }

    /// 从缓存恢复单个文件
    fn restore_file_from_cache(
        &mut self,
        file_info: &EditorOpenFileInfo,
        editor_state: Entity<InputState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_str = file_info.path.clone();
        let label = file_info.label.clone();
        let ssh_config = file_info.ssh_config.clone();

        let content = editor_state.read(cx).value().to_string();
        let is_markdown = path_str.ends_with(".md");
        let markdown = if is_markdown {
            Some(cx.new(|cx| crate::editor::markdown::Markdown::new(content.clone(), cx)))
        } else {
            None
        };

        // Re-render the Editor when the restored editor's InputState changes
        // (hover popup / completion menu / search matches).
        self.editor_state_subscriptions
            .push(cx.observe(&editor_state, |_, _, cx| cx.notify()));

        let file = EditorFile {
            id: file_info.id.clone(),
            path: path_str.clone(),
            label,
            ssh_config: ssh_config.clone(),
            editor_state: editor_state.clone(),
            original_content: content,
            pending_reload: false,
            is_loading: false,
            markdown,
            image: None,
            diff: None,
        };

        self.open_files.push(file);

        // 启动文件监视器
        if let Some(config) = ssh_config {
            let fs = Arc::new(RemoteFileSystem::new(config));
            Self::watch_remote_file(Path::new(&file_info.path), fs, cx);
        } else {
            Self::watch_file(Path::new(&file_info.path), cx);
        }

        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(
                TabItem::new(&file_info.id, &file_info.label)
                    .with_icon(Some(file_icon_or_default(Path::new(&file_info.path)))),
                cx,
            );
        });

        self.switch_to_file(&file_info.id, cx);
        cx.notify();
    }

    fn setup_watcher(&mut self, cx: &mut Context<Self>) {
        let watcher = cx.fs_watcher();
        let remote_watcher = cx.remote_fs_watcher();
        self.watcher_subscription = Some(cx.subscribe(
            &watcher,
            move |this, _, event: &FsChangeEvent, cx| {
                this.handle_fs_event(event, cx);
            },
        ));
        self.remote_watcher_subscription = Some(cx.subscribe(
            &remote_watcher,
            move |this, _, event: &FsChangeEvent, cx| {
                this.handle_fs_event(event, cx);
            },
        ));
    }

    fn handle_fs_event(&mut self, event: &FsChangeEvent, cx: &mut Context<Self>) {
        if !matches!(event.kind, FsChangeKind::Modify | FsChangeKind::Create) {
            return;
        }

        for file in &mut self.open_files {
            if Path::new(&file.path) != event.path.as_path() {
                continue;
            }
            if file.pending_reload {
                return;
            }

            file.pending_reload = true;
            let path = file.path.clone();
            let ssh_config = file.ssh_config.clone();
            let is_image = file.image.is_some();
            let weak = cx.entity().downgrade();

            if is_image {
                cx.spawn(async move |_, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(200))
                        .await;

                    let result = if let Some(config) = ssh_config {
                        let path_for_task = path.clone();
                        cx.background_executor()
                            .spawn(async move {
                                let bytes =
                                    crate::ssh::sftp_fs::read_file_bytes(&config, &path_for_task)?;
                                bytes
                                    .ok_or_else(|| anyhow::anyhow!("Empty file"))
                                    .and_then(|b| {
                                        crate::editor::image_viewer::load_image_from_bytes(
                                            &b,
                                            Path::new(&path_for_task),
                                        )
                                    })
                            })
                            .await
                    } else {
                        let path_for_task = path.clone();
                        cx.background_executor()
                            .spawn(async move {
                                let bytes = std::fs::read(&path_for_task)?;
                                crate::editor::image_viewer::load_image_from_bytes(
                                    &bytes,
                                    Path::new(&path_for_task),
                                )
                            })
                            .await
                    };

                    if let Ok((image, size)) = result {
                        let _ = weak.update(cx, |this, cx| {
                            if let Some(file) = this.open_files.iter_mut().find(|f| f.path == path)
                            {
                                if let Some(ref viewer) = file.image {
                                    viewer.update(cx, |viewer, cx| {
                                        viewer.set_image(image, size, cx);
                                    });
                                }
                                file.pending_reload = false;
                            }
                            cx.notify();
                        });
                    } else {
                        let _ = weak.update(cx, |this, _cx| {
                            if let Some(file) = this.open_files.iter_mut().find(|f| f.path == path)
                            {
                                file.pending_reload = false;
                            }
                        });
                    }
                })
                .detach();

                return;
            }

            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;

                let content = if let Some(config) = ssh_config {
                    let path_for_task = path.clone();
                    match cx
                        .background_executor()
                        .spawn(async move {
                            crate::ssh::sftp_fs::read_text_preview(&config, &path_for_task)
                        })
                        .await
                    {
                        Ok(Some(text)) => Some(text),
                        Ok(None) => {
                            log::warn!("远程文件为二进制或空，无法重载: {}", path);
                            None
                        }
                        Err(e) => {
                            log::warn!("重载远程文件失败 {}: {}", path, e);
                            None
                        }
                    }
                } else {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => Some(text),
                        Err(e) => {
                            log::warn!("重载本地文件失败 {}: {}", path, e);
                            None
                        }
                    }
                };

                if let Some(content) = content {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(file) = this.open_files.iter_mut().find(|f| f.path == path) {
                            let current = file.editor_state.read(cx).value().to_string();
                            if current == file.original_content {
                                file.original_content = content.clone();
                                file.editor_state.update(cx, |state, cx| {
                                    state.set_text(content.clone(), cx);
                                });
                                if let Some(ref markdown) = file.markdown {
                                    markdown.update(cx, |renderer, cx| {
                                        renderer.set_text(content.clone(), cx);
                                    });
                                }
                                // 同步更新远程文件内容缓存，避免重开后仍是旧内容
                                if let Some(ref ssh_config) = file.ssh_config {
                                    crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                        .lock()
                                        .unwrap()
                                        .insert_file_content(
                                            ssh_config,
                                            std::path::Path::new(&path),
                                            content,
                                        );
                                }
                            } else {
                                log::info!(
                                    "File changed externally but has unsaved changes: {}",
                                    path
                                );
                            }
                            file.pending_reload = false;
                        }
                    });
                } else {
                    let _ = weak.update(cx, |this, _cx| {
                        if let Some(file) = this.open_files.iter_mut().find(|f| f.path == path) {
                            file.pending_reload = false;
                        }
                    });
                }
            })
            .detach();

            return;
        }
    }

    fn watch_file(path: &Path, cx: &mut Context<Self>) {
        cx.watch_path(path);
    }

    fn watch_remote_file(
        path: &Path,
        fs: Arc<dyn crate::file_manager::FileSystem>,
        cx: &mut Context<Self>,
    ) {
        cx.watch_remote_path(path, fs);
    }

    fn unwatch_file(path: &Path, cx: &mut Context<Self>) {
        cx.unwatch_path(path);
    }

    fn unwatch_remote_file(path: &Path, cx: &mut Context<Self>) {
        cx.unwatch_remote_path(path);
    }

    fn unwatch_all_files(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<(PathBuf, bool)> = self
            .open_files
            .iter()
            .map(|f| (PathBuf::from(&f.path), f.ssh_config.is_some()))
            .collect();
        for (path, is_remote) in paths {
            if is_remote {
                Self::unwatch_remote_file(&path, cx);
            } else {
                Self::unwatch_file(&path, cx);
            }
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.unwatch_all_files(cx);
        self.open_files.clear();
        self.active_file_id = None;
        self.loading_gens.clear();
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.clear_tabs(cx);
        });
        cx.notify();
    }

    fn begin_load(&mut self, tab_id: &str) -> u64 {
        self.next_load_generation = self.next_load_generation.wrapping_add(1);
        let load_gen = self.next_load_generation;
        self.loading_gens.insert(tab_id.to_string(), load_gen);
        load_gen
    }

    fn is_load_current(&self, tab_id: &str, load_gen: u64) -> bool {
        self.loading_gens.get(tab_id).copied() == Some(load_gen)
    }

    pub fn is_dragging(&self) -> bool {
        self.resize_drag_x.is_some()
    }

    pub fn start_resize(&mut self, pos: Pixels) {
        self.resize_drag_x = Some(pos);
    }

    pub fn update_resize(&mut self, pos: Pixels) {
        if let Some(start) = self.resize_drag_x {
            let delta: f32 = (pos - start).into();
            self.width = (self.width + delta).max(EDITOR_MIN_WIDTH);
            self.resize_drag_x = Some(pos);
        }
    }

    pub fn end_resize(&mut self) {
        self.resize_drag_x = None;
    }

    pub fn set_paddings(&mut self, content_left: f32, content_bottom: f32, cx: &mut Context<Self>) {
        if (self.content_left_padding - content_left).abs() > 0.5
            || (self.content_bottom_padding - content_bottom).abs() > 0.5
        {
            self.content_left_padding = content_left;
            self.content_bottom_padding = content_bottom;
            cx.notify();
        }
    }

    pub fn set_tab_bar_pad_animation(
        &mut self,
        epoch: u32,
        start: f32,
        end: f32,
        duration_ms: u64,
        cx: &mut Context<Self>,
    ) {
        if self.tab_bar_pad_epoch != epoch
            || (self.tab_bar_pad_start - start).abs() > 0.5
            || (self.tab_bar_pad_end - end).abs() > 0.5
            || self.tab_bar_pad_duration_ms != duration_ms
        {
            self.tab_bar_pad_epoch = epoch;
            self.tab_bar_pad_start = start;
            self.tab_bar_pad_end = end;
            self.tab_bar_pad_duration_ms = duration_ms;
            cx.notify();
        }
    }

    pub fn notify_app_window(&self, cx: &mut App) {
        if let Some(aw) = &self.app_window {
            aw.update(cx, |_, cx| cx.notify()).ok();
        }
    }

    pub fn show_notification(
        &self,
        message: impl Into<gpui::SharedString>,
        kind: crate::components::notification::NotificationKind,
        cx: &mut App,
    ) {
        if let Some(aw) = &self.app_window {
            aw.update(cx, |this, cx| {
                this.show_notification(message, kind, cx);
            })
            .ok();
        }
    }

    pub fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_local_file(path, window, cx, true);
    }

    pub fn open_file_no_notify(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_local_file(path, window, cx, false);
    }

    /// Open a diff view for a file
    pub fn open_diff_file(
        &mut self,
        path: String,
        diff: crate::editor::diff_model::DiffFile,
        repo: crate::editor::git_repository::GitRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_id = format!("diff-{}", path);

        // If already open, just switch to it
        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        let label = format!("Diff: {}", path);
        // Placeholder editor state; the diff tab renders `diff` instead.
        let editor_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx).multi_line(true);
            state.disabled = true;
            state
        });
        let editor_weak = cx.entity().downgrade();
        let diff_view = cx.new(|cx| {
            crate::editor::diff_view::DiffView::new(diff, repo, editor_weak, window, cx)
        });

        self.open_files.push(EditorFile {
            id: tab_id.clone().into(),
            path: path.clone(),
            label,
            ssh_config: None,
            editor_state,
            original_content: String::new(),
            pending_reload: false,
            is_loading: false,
            markdown: None,
            image: None,
            diff: Some(diff_view),
        });

        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(
                TabItem::new(&tab_id, &format!("Diff: {}", path)).with_icon(Some(Icon::Git)),
                cx,
            );
        });

        self.switch_to_file(&tab_id, cx);
        cx.notify();
    }

    fn open_local_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
    ) {
        let path_str = path.to_string_lossy().to_string();
        let tab_id = format!("editor-{}", path_str);
        let is_image = crate::editor::image_viewer::is_image_file(&path);
        log::debug!("open_local_file: path={:?}, is_image={}", path, is_image);

        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        if self.loading_gens.contains_key(&tab_id) {
            return;
        }

        if is_image {
            self.open_image_file(tab_id, path_str, path, None, window, cx, notify_app);
            return;
        }

        let load_gen = self.begin_load(&tab_id);
        let path_for_task = path.clone();

        self.open_file_with_content(
            tab_id.clone(),
            path_str,
            String::new(),
            path,
            None,
            window,
            cx,
            notify_app,
            true,
        );

        self.spawn_file_load(
            tab_id,
            load_gen,
            Arc::new(move || std::fs::read_to_string(&path_for_task).map_err(|e| e.to_string())),
            "读取文件失败",
            "无法读取本地文件",
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_file_load(
        &mut self,
        tab_id: String,
        load_generation: u64,
        load_content: Arc<dyn Fn() -> Result<String, String> + Send + Sync>,
        error_title: &'static str,
        error_prefix: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window_handle = window.window_handle();
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                let still_current = weak
                    .update(cx, |this, _cx| {
                        this.is_load_current(&tab_id, load_generation)
                            && this.open_files.iter().any(|f| f.id == tab_id)
                    })
                    .unwrap_or(false);
                if !still_current {
                    let _ = weak.update(cx, |this, _cx| {
                        if this.is_load_current(&tab_id, load_generation) {
                            this.loading_gens.remove(&tab_id);
                        }
                    });
                    break;
                }

                let load_content = load_content.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { load_content() })
                    .await;

                match result {
                    Ok(content) => {
                        let _ = weak.update(cx, |this, cx| {
                            if !this.is_load_current(&tab_id, load_generation) {
                                return;
                            }
                            this.loading_gens.remove(&tab_id);
                            if let Some(file) = this.open_files.iter_mut().find(|f| f.id == tab_id)
                            {
                                file.original_content = content.clone();
                                file.is_loading = false;
                                // 缓存远程文件内容到 RemoteFileTreeCache
                                if let Some(ref ssh_config) = file.ssh_config {
                                    crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                        .lock()
                                        .unwrap()
                                        .insert_file_content(
                                            ssh_config,
                                            std::path::Path::new(&file.path),
                                            content.clone(),
                                        );
                                }
                                file.editor_state.update(cx, |state, cx| {
                                    state.disabled = false;
                                    state.set_text(content.clone(), cx);
                                });
                                if let Some(ref markdown) = file.markdown {
                                    markdown.update(cx, |renderer, cx| {
                                        renderer.set_text(content.clone(), cx);
                                    });
                                }
                            }
                            let file_path = this
                                .open_files
                                .iter()
                                .find(|f| f.id == tab_id)
                                .map(|f| f.path.clone());
                            if let Some(file_path) = file_path {
                                this.tab_bar.update(cx, |tab_bar, cx| {
                                    tab_bar.modify_tab(
                                        &tab_id,
                                        |tab| {
                                            tab.icon = Some(file_icon_or_default(Path::new(
                                                &file_path,
                                            )));
                                            tab.is_loading = false;
                                        },
                                        cx,
                                    );
                                });
                            }
                            cx.notify();
                        });
                        break;
                    }
                    Err(msg) => {
                        let still_current = weak
                            .update(cx, |this, _cx| {
                                this.is_load_current(&tab_id, load_generation)
                                    && this.open_files.iter().any(|f| f.id == tab_id)
                            })
                            .unwrap_or(false);
                        if !still_current {
                            break;
                        }

                        let close = match cx.update_window(window_handle, |_view, window, cx| {
                            window.prompt(
                                gpui::PromptLevel::Warning,
                                error_title,
                                Some(&format!("{}:\n{}\n\n重试或关闭标签页?", error_prefix, msg)),
                                &["重新加载", "关闭标签页"],
                                cx,
                            )
                        }) {
                            Ok(promise) => promise.await.unwrap_or(1) != 0,
                            Err(_) => true,
                        };
                        if close {
                            let _ = weak.update(cx, |this, cx| {
                                if this.is_load_current(&tab_id, load_generation) {
                                    this.loading_gens.remove(&tab_id);
                                    this.close_file(&tab_id, cx);
                                }
                                cx.notify();
                            });
                            break;
                        }
                    }
                }
            }
        })
        .detach();
    }

    pub fn open_remote_file(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, true);
    }

    pub fn open_remote_file_no_notify(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, false);
    }

    fn open_remote_file_inner(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
    ) {
        let tab_id = format!("editor-remote-{}", path);

        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        if self.loading_gens.contains_key(&tab_id) {
            return;
        }

        if crate::editor::image_viewer::is_image_file(Path::new(&path)) {
            self.open_image_file(
                tab_id,
                path.clone(),
                PathBuf::from(&path),
                Some(ssh_config),
                window,
                cx,
                notify_app,
            );
            return;
        }

        // 检查是否有缓存的文件内容
        let cached_content = crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
            .lock()
            .unwrap()
            .get_file_content(&ssh_config, std::path::Path::new(&path));

        if let Some(content) = cached_content {
            // 有缓存，直接使用缓存内容打开文件
            self.open_file_with_content(
                tab_id,
                path.clone(),
                content,
                PathBuf::from(&path),
                Some(ssh_config),
                window,
                cx,
                notify_app,
                false, // is_loading = false，因为有缓存内容
            );
            return;
        }

        // 没有缓存，正常异步加载
        let load_gen = self.begin_load(&tab_id);
        let ssh_config_for_task = ssh_config.clone();
        let path_for_task = path.clone();
        let path_for_log = path.clone();

        self.open_file_with_content(
            tab_id.clone(),
            path.clone(),
            String::new(),
            PathBuf::from(&path),
            Some(ssh_config),
            window,
            cx,
            notify_app,
            true,
        );

        self.spawn_file_load(
            tab_id,
            load_gen,
            Arc::new(move || {
                match crate::ssh::sftp_fs::read_text_preview(&ssh_config_for_task, &path_for_task) {
                    Ok(Some(text)) => Ok(text),
                    Ok(None) => {
                        log::warn!("远程文件为二进制或空，无法打开编辑: {}", path_for_log);
                        Ok(String::new())
                    }
                    Err(e) => {
                        log::warn!("读取远程文件失败 {}: {}", path_for_log, e);
                        Err(e.to_string())
                    }
                }
            }),
            "读取文件失败",
            "无法读取远程文件",
            window,
            cx,
        );
    }

    fn open_image_file(
        &mut self,
        tab_id: String,
        path_str: String,
        path: PathBuf,
        ssh_config: Option<SshConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
    ) {
        let load_gen = self.begin_load(&tab_id);

        self.open_file_with_content(
            tab_id.clone(),
            path_str.clone(),
            String::new(),
            path.clone(),
            ssh_config.clone(),
            window,
            cx,
            notify_app,
            true,
        );

        let load_image: Arc<
            dyn Fn() -> Result<(Arc<gpui::Image>, (u32, u32)), String> + Send + Sync,
        > = if let Some(config) = ssh_config {
            Arc::new(move || {
                let bytes = crate::ssh::sftp_fs::read_file_bytes(&config, &path_str)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Empty file".to_string())?;
                crate::editor::image_viewer::load_image_from_bytes(&bytes, Path::new(&path_str))
                    .map_err(|e| e.to_string())
            })
        } else {
            let path_for_task = path.clone();
            let path_for_log = path_str.clone();
            Arc::new(move || {
                let bytes = std::fs::read(&path_for_task).map_err(|e| e.to_string())?;
                crate::editor::image_viewer::load_image_from_bytes(&bytes, &path_for_task)
                    .map_err(|e| format!("{}: {}", path_for_log, e))
            })
        };

        self.spawn_image_load(tab_id, load_gen, load_image, window, cx);
    }

    fn spawn_image_load(
        &mut self,
        tab_id: String,
        load_generation: u64,
        load_image: Arc<dyn Fn() -> Result<(Arc<gpui::Image>, (u32, u32)), String> + Send + Sync>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window_handle = window.window_handle();
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                let still_current = weak
                    .update(cx, |this, _cx| {
                        this.is_load_current(&tab_id, load_generation)
                            && this.open_files.iter().any(|f| f.id == tab_id)
                    })
                    .unwrap_or(false);
                if !still_current {
                    let _ = weak.update(cx, |this, _cx| {
                        if this.is_load_current(&tab_id, load_generation) {
                            this.loading_gens.remove(&tab_id);
                        }
                    });
                    break;
                }

                let load_image = load_image.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { load_image() })
                    .await;

                match result {
                    Ok((image, size)) => {
                        let _ = weak.update(cx, |this, cx| {
                            if !this.is_load_current(&tab_id, load_generation) {
                                return;
                            }
                            this.loading_gens.remove(&tab_id);
                            if let Some(file) = this.open_files.iter_mut().find(|f| f.id == tab_id)
                            {
                                file.is_loading = false;
                                if let Some(ref viewer) = file.image {
                                    viewer.update(cx, |viewer, cx| {
                                        viewer.set_image(image, size, cx);
                                    });
                                }
                                this.tab_bar.update(cx, |tab_bar, cx| {
                                    tab_bar.modify_tab(
                                        &tab_id,
                                        |tab| {
                                            tab.icon =
                                                Some(file_icon_or_default(Path::new(&file.path)));
                                            tab.is_loading = false;
                                        },
                                        cx,
                                    );
                                });
                            }
                            cx.notify();
                        });
                        break;
                    }
                    Err(msg) => {
                        let still_current = weak
                            .update(cx, |this, _cx| {
                                this.is_load_current(&tab_id, load_generation)
                                    && this.open_files.iter().any(|f| f.id == tab_id)
                            })
                            .unwrap_or(false);
                        if !still_current {
                            break;
                        }

                        log::warn!("加载图片失败: {}", msg);
                        let close = match cx.update_window(window_handle, |_view, window, cx| {
                            window.prompt(
                                gpui::PromptLevel::Warning,
                                "加载图片失败",
                                Some(&format!("无法加载图片:\n{}\n\n重试或关闭标签页?", msg)),
                                &["重新加载", "关闭标签页"],
                                cx,
                            )
                        }) {
                            Ok(promise) => promise.await.unwrap_or(1) != 0,
                            Err(_) => true,
                        };

                        if close {
                            let _ = weak.update(cx, |this, cx| {
                                if this.is_load_current(&tab_id, load_generation) {
                                    this.loading_gens.remove(&tab_id);
                                    this.close_file(&tab_id, cx);
                                }
                                cx.notify();
                            });
                            break;
                        }
                    }
                }
            }
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn open_file_with_content(
        &mut self,
        tab_id: String,
        path_str: String,
        content: String,
        path: PathBuf,
        ssh_config: Option<SshConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
        _notify_app: bool,
        is_loading: bool,
    ) {
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&path_str)
            .to_string();
        let is_image = crate::editor::image_viewer::is_image_file(&path);
        let language = guess_language_from_extension(&path);

        let editor_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(language.clone())
                .multi_line(true);
            state.set_value(&content, window, cx);
            state.disabled = is_image || is_loading;
            state
        });

        // Re-render the Editor when this editor's InputState changes
        // (search matches / cursor): the popups are rendered by EditorInput,
        // which only re-renders when the Editor does.
        self.editor_state_subscriptions.push(
            cx.observe(&editor_state, |_, _, cx| cx.notify()),
        );

        let is_markdown = !is_image && path.extension().and_then(|e| e.to_str()) == Some("md");
        let markdown = if is_markdown {
            let content_clone = content.clone();
            Some(cx.new(|cx| crate::editor::markdown::Markdown::new(content_clone, cx)))
        } else {
            None
        };

        let image = if is_image {
            Some(cx.new(crate::editor::image_viewer::ImageViewer::new))
        } else {
            None
        };

        self.open_files.push(EditorFile {
            id: tab_id.clone().into(),
            path: path_str,
            label: label.clone(),
            ssh_config,
            editor_state,
            original_content: content.clone(),
            pending_reload: false,
            is_loading,
            markdown,
            image,
            diff: None,
        });

        let is_local = self
            .open_files
            .last()
            .map(|f| f.ssh_config.is_none())
            .unwrap_or(true);
        if is_local {
            Self::watch_file(&path, cx);
        } else {
            let fs = Arc::new(RemoteFileSystem::new(
                self.open_files.last().unwrap().ssh_config.clone().unwrap(),
            ));
            Self::watch_remote_file(&path, fs, cx);
        }

        let tab_icon = if is_loading {
            Some(Icon::Loader)
        } else {
            Some(file_icon_or_default(&path))
        };
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(
                TabItem::new(&tab_id, &label)
                    .with_icon(tab_icon)
                    .with_loading(is_loading),
                cx,
            );
        });

        self.switch_to_file(&tab_id, cx);
        cx.notify();
    }

    fn switch_to_file(&mut self, id: &str, cx: &mut Context<Self>) {
        self.active_file_id = Some(id.into());
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.set_active_tab(id, cx);
        });
        cx.notify();
    }

    fn close_file(&mut self, id: &str, cx: &mut Context<Self>) {
        self.loading_gens.remove(id);
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let file = &self.open_files[index];
            // 文件关闭后同步清理 RemoteFileTreeCache 中的内容缓存，避免重新打开时看到旧内容
            if file.ssh_config.is_some() {
                crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                    .lock()
                    .unwrap()
                    .remove_file(
                        file.ssh_config.as_ref().unwrap(),
                        std::path::Path::new(&file.path),
                    );
            }
            if file.ssh_config.is_none() {
                Self::unwatch_file(Path::new(&file.path), cx);
            } else {
                Self::unwatch_remote_file(Path::new(&file.path), cx);
            }
            self.open_files.remove(index);
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.remove_tab(id, cx);
            });

            let was_active = self.active_file_id.as_ref().map(|s| s.as_ref()) == Some(id);
            if was_active {
                let next_id = if index < self.open_files.len() {
                    Some(self.open_files[index].id.clone())
                } else if !self.open_files.is_empty() {
                    Some(self.open_files[self.open_files.len() - 1].id.clone())
                } else {
                    None
                };
                if let Some(next) = next_id {
                    self.switch_to_file(&next, cx);
                } else {
                    self.active_file_id = None;
                }
            }
        }
        cx.notify();
    }

    fn close_other_tabs(&mut self, id: &str, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .open_files
            .iter()
            .filter(|f| f.id.as_ref() != id)
            .map(|f| f.id.to_string())
            .collect();
        for file_id in ids {
            self.close_file(&file_id, cx);
        }
    }

    fn close_right_tabs(&mut self, id: &str, cx: &mut Context<Self>) {
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let ids: Vec<String> = self.open_files[index + 1..]
                .iter()
                .map(|f| f.id.to_string())
                .collect();
            for file_id in ids {
                self.close_file(&file_id, cx);
            }
        }
    }

    fn close_left_tabs(&mut self, id: &str, cx: &mut Context<Self>) {
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let ids: Vec<String> = self.open_files[..index]
                .iter()
                .map(|f| f.id.to_string())
                .collect();
            for file_id in ids {
                self.close_file(&file_id, cx);
            }
        }
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.open_files.iter().map(|f| f.id.to_string()).collect();
        for file_id in ids {
            self.close_file(&file_id, cx);
        }
    }

    fn handle_tab_reorder(
        &mut self,
        dragged_tab: DraggedTab,
        to_index: usize,
        cx: &mut Context<Self>,
    ) {
        let dragged_id = dragged_tab.tab_id.as_ref();
        let from_index = self
            .open_files
            .iter()
            .position(|f| f.id.as_ref() == dragged_id);
        if let Some(from) = from_index {
            if from != to_index {
                let file = self.open_files.remove(from);
                let insert_idx = if to_index > from {
                    to_index.saturating_sub(1)
                } else {
                    to_index
                };
                let capped = insert_idx.min(self.open_files.len());
                self.open_files.insert(capped, file);
                cx.notify();
            }
        }
    }

    fn setup_tab_bar_handlers(&mut self, cx: &mut Context<Self>) {
        let editor = cx.entity();
        self.tab_bar.update(cx, |tab_bar, _cx| {
            tab_bar.on_tab_click({
                let editor = editor.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    editor.update(cx, |_, cx| {
                        cx.defer_in(window, move |this, _window, cx| {
                            this.switch_to_file(&id, cx);
                        });
                    });
                }
            });

            tab_bar.on_tab_close({
                let editor = editor.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    editor.update(cx, |_, cx| {
                        cx.defer_in(window, move |this, _window, cx| {
                            this.close_file(&id, cx);
                        });
                    });
                }
            });

            tab_bar.on_tab_reorder({
                let editor = editor.clone();
                move |dragged, to_index, cx| {
                    editor.update(cx, |this, cx| {
                        this.handle_tab_reorder(dragged, to_index, cx);
                    });
                }
            });

            tab_bar.on_context_menu({
                move |_tab_id, menu, _window, _cx| {
                    menu.menu("关闭", Box::new(CloseTab))
                        .menu("关闭其他标签页", Box::new(CloseOtherTabs))
                        .menu("关闭左侧标签页", Box::new(CloseLeftTabs))
                        .menu("关闭右侧标签页", Box::new(CloseRightTabs))
                        .menu("关闭所有标签页", Box::new(CloseAllTabs))
                }
            });
        });
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.open_files.is_empty();
        let theme = cx.theme().ui;

        let inner = self.tab_bar.clone();

        let tab_bar: gpui::AnyElement = if is_empty {
            div()
                .w(px(0.0))
                .h(px(44.0))
                .overflow_hidden()
                .child(inner)
                .into_any_element()
        } else {
            let epoch = self.tab_bar_pad_epoch;
            let start = self.tab_bar_pad_start;
            let end = self.tab_bar_pad_end;
            let dur = self.tab_bar_pad_duration_ms;

            if epoch > 0 {
                let id = ("editor-tab-pad", epoch);
                div()
                    .h(px(44.0))
                    .w(px((self.width - 8.0).max(0.0)))
                    .child(inner)
                    .with_animation(
                        id,
                        Animation::new(Duration::from_millis(dur)).with_easing(gpui::ease_in_out),
                        move |this, delta| this.pl(px(start + (end - start) * delta)),
                    )
                    .into_any_element()
            } else if end > 0.0 {
                div()
                    .h(px(44.0))
                    .w(px((self.width - 8.0).max(0.0)))
                    .pl(px(end))
                    .child(inner)
                    .into_any_element()
            } else {
                div()
                    .h(px(44.0))
                    .w(px((self.width - 8.0).max(0.0)))
                    .pl(px(6.0))
                    .child(inner)
                    .into_any_element()
            }
        };

        let content: gpui::AnyElement = if !is_empty {
            if let Some(active_id) = &self.active_file_id {
                if let Some(file) = self.open_files.iter().find(|f| f.id == *active_id) {
                    if let Some(diff) = &file.diff {
                        // Report the available content width so `Auto` layout can
                        // switch between unified (default) and split.
                        diff.update(cx, |view, cx| {
                            view.set_available_width(self.width - self.content_left_padding, cx)
                        });
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .bg(theme.app_background)
                            .when(self.content_left_padding > 0.0, |this| {
                                this.pl(px(self.content_left_padding))
                            })
                            .when(self.content_bottom_padding > 0.0, |this| {
                                this.pb(px(self.content_bottom_padding))
                            })
                            .child(diff.clone())
                            .into_any_element()
                    } else if let Some(image) = &file.image {
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .bg(theme.app_background)
                            .when(self.content_left_padding > 0.0, |this| {
                                this.pl(px(self.content_left_padding))
                            })
                            .when(self.content_bottom_padding > 0.0, |this| {
                                this.pb(px(self.content_bottom_padding))
                            })
                            .child(image.clone())
                            .into_any_element()
                    } else if let Some(markdown) = &file.markdown {
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .bg(theme.app_background)
                            .when(self.content_left_padding > 0.0, |this| {
                                this.pl(px(self.content_left_padding))
                            })
                            .when(self.content_bottom_padding > 0.0, |this| {
                                this.pb(px(self.content_bottom_padding))
                            })
                            .child(markdown.clone())
                            .into_any_element()
                    } else {
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .bg(theme.app_background)
                            .when(self.content_left_padding > 0.0, |this| {
                                this.pl(px(self.content_left_padding))
                            })
                            .when(self.content_bottom_padding > 0.0, |this| {
                                this.pb(px(self.content_bottom_padding))
                            })
                            .child(EditorInput::new(&file.editor_state))
                            .into_any_element()
                    }
                } else {
                    div().into_any_element()
                }
            } else {
                div().into_any_element()
            }
        } else {
            div().into_any_element()
        };

        let resize_handle: gpui::AnyElement = if !is_empty {
            div()
                .absolute()
                .right(px(0.0))
                .top(px(0.0))
                .bottom(px(0.0))
                .w(px(2.0))
                .cursor_col_resize()
                .bg(theme.border.opacity(0.5))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        this.start_resize(event.position.x);
                        cx.notify();
                        this.notify_app_window(cx);
                    }),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        div()
            .key_context("EditorPanel")
            .on_action(cx.listener(|this, _: &CloseActiveEditorTab, _window, cx| {
                if let Some(active_id) = this.active_file_id.clone() {
                    this.close_file(active_id.as_ref(), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SaveActiveEditorTab, _window, cx| {
                if let Some(active_id) = this.active_file_id.clone() {
                    if let Some(file) = this.open_files.iter().find(|f| f.id == *active_id) {
                        if let Some(diff) = &file.diff {
                            diff.update(cx, |view, cx| view.save(cx));
                        }
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_file(&tab_id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseOtherTabs, _window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_other_tabs(&tab_id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseRightTabs, _window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_right_tabs(&tab_id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseAllTabs, _window, cx| {
                this.close_all_tabs(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseLeftTabs, _window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_left_tabs(&tab_id, cx);
                }
            }))
            .w(px(if is_empty { 0.0 } else { self.width }))
            .h_full()
            .relative()
            .flex()
            .flex_col()
            .items_start()
            .child(tab_bar)
            .child(content)
            .child(resize_handle)
            .when(!is_empty, |this| {
                this.child(EditorResizeElement {
                    editor: cx.entity().downgrade(),
                })
            })
            .into_any_element()
    }
}

pub(crate) fn guess_language_from_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| match ext {
            "rs" => "rust",
            "js" | "jsx" => "javascript",
            "ts" => "typescript",
            "tsx" => "tsx",
            "json" => "json",
            "py" => "python",
            "go" => "go",
            "cpp" | "cc" | "cxx" | "hpp" => "cpp",
            "c" | "h" => "c",
            "html" | "htm" | "vue" | "svelte" => "html",
            "css" => "css",
            "scss" | "sass" => "scss",
            "md" | "markdown" => "markdown",
            "sh" | "bash" | "zsh" => "bash",
            "toml" => "toml",
            "yaml" | "yml" => "yaml",
            "xml" => "xml",
            "sql" => "sql",
            "rb" => "ruby",
            "java" => "java",
            "kt" | "kts" => "kotlin",
            "swift" => "swift",
            "php" => "php",
            "pl" | "pm" => "perl",
            "lua" => "lua",
            "zig" => "zig",
            "gradle" => "groovy",
            _ => "text",
        })
        .unwrap_or("text")
        .to_string()
}

struct EditorResizeElement {
    editor: WeakEntity<Editor>,
}

impl Element for EditorResizeElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        (window.request_layout(gpui::Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint_state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };

        if editor.read(cx).is_dragging() {
            window.set_window_cursor_style(CursorStyle::ResizeColumn);
        }

        let ed = editor.clone();
        window.on_mouse_event(move |event: &gpui::MouseMoveEvent, _, _, cx| {
            ed.update(cx, |this, cx| {
                if this.is_dragging() {
                    this.update_resize(event.position.x);
                    cx.notify();
                    this.notify_app_window(cx);
                }
            });
        });

        let ed = editor.clone();
        window.on_mouse_event(move |_: &gpui::MouseUpEvent, _, _, cx| {
            ed.update(cx, |this, cx| {
                if this.is_dragging() {
                    this.end_resize();
                    cx.notify();
                    this.notify_app_window(cx);
                }
            });
        });
    }
}

impl IntoElement for EditorResizeElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}
