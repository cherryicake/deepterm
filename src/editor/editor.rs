use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, Context, CursorStyle, Element, ElementId, Entity,
    GlobalElementId, IntoElement, KeyBinding, ParentElement, Pixels, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, actions, div, px,
};

use crate::components::input::{EditorInput, InputState};
use crate::components::tab_bar::{DraggedTab, TabBar, TabItem};
use crate::fs_watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::sidebar_view::file_system::RemoteFileSystem;
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;

const EDITOR_DEFAULT_WIDTH: f32 = 680.0;
const EDITOR_MIN_WIDTH: f32 = 200.0;

actions!(editor_panel, [CloseActiveEditorTab]);
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
}

/// 运行时状态，包含已打开的 [`InputState`] entity，切换标签页组时直接交换，不重新读取文件。
pub struct EditorRuntimeState {
    open_files: Vec<EditorFile>,
    active_file_id: Option<SharedString>,
}

impl EditorRuntimeState {
    pub fn to_session_state(&self) -> crate::workspace::session_state::EditorSessionState {
        use crate::workspace::session_state::{EditorFileSessionState, EditorSessionState};
        EditorSessionState {
            open_files: self
                .open_files
                .iter()
                .map(|f| EditorFileSessionState {
                    id: f.id.to_string(),
                    path: f.path.clone(),
                    label: f.label.clone(),
                    ssh_config: f.ssh_config.clone(),
                })
                .collect(),
            active_file_id: self.active_file_id.as_ref().map(|id| id.to_string()),
        }
    }
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

    pub fn session_state(&self) -> crate::workspace::session_state::EditorSessionState {
        use crate::workspace::session_state::{EditorFileSessionState, EditorSessionState};
        let open_files = self
            .open_files
            .iter()
            .map(|f| EditorFileSessionState {
                id: f.id.to_string(),
                path: f.path.clone(),
                label: f.label.clone(),
                ssh_config: f.ssh_config.clone(),
            })
            .collect();
        EditorSessionState {
            open_files,
            active_file_id: self.active_file_id.as_ref().map(|id| id.to_string()),
        }
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
            let weak = cx.entity().downgrade();

            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;

                let content = if let Some(config) = ssh_config {
                    let path_for_task = path.clone();
                    match cx
                        .background_executor()
                        .spawn(async move {
                            crate::ssh::sftp_fs::read_text_preview(
                                &config, &path_for_task,
                            )
                        })
                        .await
                    {
                        Ok(Some(text)) => Some(text),
                        Ok(None) => {
                            log::warn!(
                                "远程文件为二进制或空，无法重载: {}",
                                path
                            );
                            None
                        }
                        Err(e) => {
                            log::warn!(
                                "重载远程文件失败 {}: {}",
                                path, e
                            );
                            None
                        }
                    }
                } else {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => Some(text),
                        Err(e) => {
                            log::warn!(
                                "重载本地文件失败 {}: {}",
                                path, e
                            );
                            None
                        }
                    }
                };

                if let Some(content) = content {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(file) =
                            this.open_files.iter_mut().find(|f| f.path == path)
                        {
                            let current =
                                file.editor_state.read(cx).value().to_string();
                            if current == file.original_content {
                                file.original_content = content.clone();
                                file.editor_state.update(cx, |state, cx| {
                                    state.set_text(content, cx);
                                });
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
                        if let Some(file) =
                            this.open_files.iter_mut().find(|f| f.path == path)
                        {
                            file.pending_reload = false;
                        }
                    });
                }
            }).detach();

            return;
        }
    }

    fn watch_file(path: &Path, cx: &mut Context<Self>) {
        cx.watch_path(path);
    }

    fn watch_remote_file(path: &Path, fs: Arc<dyn crate::sidebar_view::file_system::FileSystem>, cx: &mut Context<Self>) {
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
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.clear_tabs(cx);
        });
        cx.notify();
    }

    pub fn restore_from_session_state(
        &mut self,
        state: crate::workspace::session_state::EditorSessionState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        for file_state in state.open_files {
            if let Some(ref ssh_config) = file_state.ssh_config {
                self.open_remote_file_no_notify(
                    file_state.path,
                    ssh_config.clone(),
                    window,
                    cx,
                );
            } else {
                self.open_file_no_notify(PathBuf::from(&file_state.path), window, cx);
            }
        }
        if let Some(active_id) = state.active_file_id {
            self.switch_to_file(&active_id, cx);
        }
    }

    /// 取出当前运行状态（用于切换到其他标签页组前暂存）。
    pub fn take_runtime_state(&mut self, cx: &mut Context<Self>) -> EditorRuntimeState {
        self.unwatch_all_files(cx);
        let mut open_files = Vec::new();
        std::mem::swap(&mut open_files, &mut self.open_files);
        let active_file_id = self.active_file_id.take();
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.clear_tabs(cx);
        });
        cx.notify();
        EditorRuntimeState {
            open_files,
            active_file_id,
        }
    }

    /// 恢复运行状态（直接复用已有的 InputState entity，不重新读取文件）。
    pub fn restore_runtime_state(&mut self, state: EditorRuntimeState, cx: &mut Context<Self>) {
        self.open_files = state.open_files;
        self.active_file_id = state.active_file_id;
        for file in &self.open_files {
            if file.ssh_config.is_none() {
                Self::watch_file(Path::new(&file.path), cx);
            } else {
                let fs = Arc::new(RemoteFileSystem::new(file.ssh_config.clone().unwrap()));
                Self::watch_remote_file(Path::new(&file.path), fs, cx);
            }
        }
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.clear_tabs(cx);
            for file in &self.open_files {
                tab_bar.add_tab(
                    TabItem::new(&file.id, &file.label).with_icon(Some(crate::assets::Icon::File)),
                    cx,
                );
            }
            if let Some(ref active_id) = self.active_file_id {
                tab_bar.set_active_tab(active_id, cx);
            }
        });
        cx.notify();
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

    pub fn notify_app_window_to_save(&self, cx: &mut App) {
        if let Some(aw) = &self.app_window {
            aw.update(cx, |this, cx| {
                this.defer_save_session(cx);
                cx.notify();
            }).ok();
        }
    }

    pub fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path_str = path.to_string_lossy().to_string();
        let tab_id = format!("editor-{}", path_str);

        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        self.open_file_with_content(tab_id, path_str, content, path, None, window, cx, true);
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

        let content = match crate::ssh::sftp_fs::read_text_preview(&ssh_config, &path) {
            Ok(Some(text)) => text,
            Ok(None) => {
                log::warn!("远程文件为二进制或空，无法打开编辑: {}", path);
                String::new()
            }
            Err(e) => {
                log::warn!("读取远程文件失败 {}: {}", path, e);
                String::new()
            }
        };

        self.open_file_with_content(
            tab_id,
            path.clone(),
            content,
            PathBuf::from(&path),
            Some(ssh_config),
            window,
            cx,
            notify_app,
        );
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
        notify_app: bool,
    ) {
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&path_str)
            .to_string();
        let language = guess_language_from_extension(&path);

        let editor_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true);
            state.set_value(&content, window, cx);
            state
        });

        self.open_files.push(EditorFile {
            id: tab_id.clone().into(),
            path: path_str,
            label: label.clone(),
            ssh_config,
            editor_state,
            original_content: content.clone(),
            pending_reload: false,
        });

        let is_local = self.open_files.last().map(|f| f.ssh_config.is_none()).unwrap_or(true);
        if is_local {
            Self::watch_file(&path, cx);
        } else {
            let fs = Arc::new(RemoteFileSystem::new(
                self.open_files.last().unwrap().ssh_config.clone().unwrap(),
            ));
            Self::watch_remote_file(&path, fs, cx);
        }

        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(
                TabItem::new(&tab_id, &label).with_icon(Some(crate::assets::Icon::File)),
                cx,
            );
        });

        self.switch_to_file(&tab_id, cx);
        cx.notify();
        if notify_app {
            self.notify_app_window_to_save(cx);
        }
    }

    pub fn open_file_no_notify(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_str = path.to_string_lossy().to_string();
        let tab_id = format!("editor-{}", path_str);

        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        self.open_file_with_content(tab_id, path_str, content, path, None, window, cx, false);
    }

    fn switch_to_file(&mut self, id: &str, cx: &mut Context<Self>) {
        self.active_file_id = Some(id.into());
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.set_active_tab(id, cx);
        });
        cx.notify();
    }

    fn close_file(&mut self, id: &str, cx: &mut Context<Self>) {
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let file = &self.open_files[index];
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
        self.notify_app_window_to_save(cx);
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
                self.notify_app_window_to_save(cx);
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

fn guess_language_from_extension(path: &Path) -> String {
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
