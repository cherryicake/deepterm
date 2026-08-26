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
use crate::components::notification::NotificationKind;
use crate::components::tab_bar::{DraggedTab, TabBar, TabItem};
use crate::editor::encoding::{self, FileEncoding};
use crate::editor::goto_line;
use crate::editor::syntax::languages::Language;
use crate::editor::image_viewer::{ActualSize, FitToView, ResetZoom, ZoomIn, ZoomOut};
use crate::file_icons::file_icon_or_default;
use crate::file_manager::filesystem::RemoteFileSystem;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;

const EDITOR_DEFAULT_WIDTH: f32 = 680.0;
const EDITOR_MIN_WIDTH: f32 = 200.0;

/// 编辑器展开/收起动画时长（与侧边栏展开动画一致，ease_in_out）。
const EDITOR_OPEN_ANIMATION_MS: u64 = 120;

actions!(editor_panel, [CloseActiveEditorTab, SaveActiveEditorTab, GotoLine]);
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
        // 跳转到行：ctrl-g 全平台；cmd-l 仅 macOS（覆盖工作区的新建终端）
        KeyBinding::new("ctrl-g", GotoLine, Some("EditorPanel")),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-l", GotoLine, Some("EditorPanel")),
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
    /// 文件的磁盘编码（加载时检测，保存时按此编码写回）。
    encoding: FileEncoding,
    /// 文件无写权限：编辑器置为只读，避免保存时才报错。
    read_only: bool,
    pending_reload: bool,
    /// 外部变更与本地未保存修改冲突时，确认弹窗已打开。
    reload_prompt_pending: bool,
    is_loading: bool,
    /// 正在保存中：防止并发保存（与 DiffView 相同的防护）。
    saving: bool,
    /// 关闭确认弹窗已打开：防止重复弹窗。
    close_prompt_pending: bool,
    /// 上一次脏检查时的 InputState::text_version（O(1) 闸门，避免每次光标移动全量比较）。
    last_checked_text_version: u64,
    /// 当前标签页是否显示未保存标记。
    dirty: bool,
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
    /// 磁盘上的内容基线（最后一次加载/保存后的内容），用于跨文件树切换后恢复未保存状态。
    /// 加载中的文件为 None。
    pub original_content: Option<String>,
    /// 磁盘编码（跨标签页组保留，避免重新检测）。
    pub encoding: FileEncoding,
    /// 只读文件标记。
    pub read_only: bool,
}

/// 后台加载得到的文件内容：解码后的文本 + 检测到的编码 + 只读标记。
pub(crate) struct LoadedText {
    pub text: String,
    pub encoding: FileEncoding,
    pub read_only: bool,
}

/// 读取并解码本地文件字节（后台线程调用）。
pub(crate) fn load_local_text(path: &Path) -> Result<LoadedText, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if crate::ssh::sftp_fs::is_binary_bytes(&bytes) {
        return Err("二进制文件，无法作为文本编辑".to_string());
    }
    let decoded = encoding::decode_bytes(&bytes);
    let read_only = std::fs::metadata(path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false);
    Ok(LoadedText {
        text: decoded.text,
        encoding: decoded.encoding,
        read_only,
    })
}

/// 读取并解码远程文件字节（后台线程调用）。
pub(crate) fn load_remote_text(
    config: &SshConnectionConfig,
    path: &str,
) -> Result<LoadedText, String> {
    let bytes = crate::ssh::sftp_fs::read_file_bytes(config, path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "文件不存在或为空".to_string())?;
    if crate::ssh::sftp_fs::is_binary_bytes(&bytes) {
        return Err("二进制文件，无法作为文本编辑".to_string());
    }
    let decoded = encoding::decode_bytes(&bytes);
    // 任意写位（owner/group/other）都不存在 → 判定只读。
    let read_only = crate::ssh::sftp_fs::file_permissions(config, path)
        .ok()
        .flatten()
        .map(|mode| mode & 0o222 == 0)
        .unwrap_or(false);
    Ok(LoadedText {
        text: decoded.text,
        encoding: decoded.encoding,
        read_only,
    })
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
    /// 主窗口句柄：fs 事件回调中没有 Window，弹窗时用它取回窗口。
    window_handle: Option<gpui::AnyWindowHandle>,
    tab_bar_pad_epoch: u32,
    tab_bar_pad_start: f32,
    tab_bar_pad_end: f32,
    tab_bar_pad_duration_ms: u64,
    /// 编辑器展开/收起动画纪元：每次「空 ↔ 非空」转变 +1，作为 with_animation 的 key。
    open_anim_epoch: u32,
    /// 展开/收起动画起始宽度
    open_anim_start: f32,
    /// 展开/收起动画目标宽度
    open_anim_end: f32,
    /// 恢复文件树/会话状态期间为 true：禁止播放展开动画（避免切换标签页组/文件树时闪动）
    suppress_open_animation: bool,
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
    /// 跳转到行面板（cmd-l / ctrl-g 切换）。
    goto_line_panel: Option<Entity<goto_line::GotoLinePanel>>,
    /// 语言选择弹出菜单（状态栏语言 chip 点击时创建）。
    language_menu: Option<Entity<crate::components::menu::PopupMenu>>,
    _language_menu_subscription: Option<Subscription>,
    language_menu_position: gpui::Point<Pixels>,
    next_load_generation: u64,
}

impl Editor {
    /// Markdown 编辑模式 tab 的 id 后缀：预览中双击后新开的纯编辑 tab。
    const MARKDOWN_EDIT_SUFFIX: &str = "#edit";

    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tab_bar = TabBar::new("editor-tab-bar", cx);

        let mut this = Self {
            tab_bar,
            open_files: Vec::new(),
            active_file_id: None,
            width: EDITOR_DEFAULT_WIDTH,
            resize_drag_x: None,
            app_window: None,
            window_handle: Some(_window.window_handle()),
            tab_bar_pad_epoch: 0,
            tab_bar_pad_start: 6.0,
            tab_bar_pad_end: 6.0,
            tab_bar_pad_duration_ms: 0,
            open_anim_epoch: 0,
            open_anim_start: 0.0,
            open_anim_end: 0.0,
            suppress_open_animation: false,
            content_left_padding: 0.0,
            content_bottom_padding: 0.0,
            watcher_subscription: None,
            remote_watcher_subscription: None,
            editor_state_subscriptions: Vec::new(),
            loading_gens: HashMap::new(),
            goto_line_panel: None,
            language_menu: None,
            _language_menu_subscription: None,
            language_menu_position: gpui::point(px(0.0), px(0.0)),
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

    /// 展开/收起动画纪元（供 AppWindow 同步标题栏 padding 动画）。
    pub fn open_anim_epoch(&self) -> u32 {
        self.open_anim_epoch
    }

    /// 编辑器从关闭 → 打开（首个文件打开）：记录展开动画（0 → 当前宽度）。
    fn begin_open_animation(&mut self) {
        if self.suppress_open_animation {
            return;
        }
        self.open_anim_epoch = self.open_anim_epoch.wrapping_add(1);
        self.open_anim_start = 0.0;
        self.open_anim_end = self.width;
    }

    /// 编辑器从打开 → 关闭（最后一个文件关闭）：记录收起动画（当前宽度 → 0）。
    fn begin_close_animation(&mut self) {
        self.open_anim_epoch = self.open_anim_epoch.wrapping_add(1);
        self.open_anim_start = self.width;
        self.open_anim_end = 0.0;
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
                original_content: if f.is_loading {
                    None
                } else {
                    Some(f.original_content.clone())
                },
                encoding: f.encoding,
                read_only: f.read_only,
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
        // 恢复期间不播放展开动画（编辑器宽度保持原值，标题栏 padding 同步保持瞬时）
        self.suppress_open_animation = true;
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
        self.suppress_open_animation = false;
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
        let original_content = file_info
            .original_content
            .clone()
            .unwrap_or_else(|| content.clone());
        let is_dirty = content.as_str() != original_content.as_str();
        let is_markdown = path_str.ends_with(".md")
            && !file_info.id.as_ref().ends_with(Self::MARKDOWN_EDIT_SUFFIX);
        let markdown = if is_markdown {
            Some(cx.new(|cx| crate::editor::markdown::Markdown::new(content.clone(), cx)))
        } else {
            None
        };

        // Re-render the Editor when the restored editor's InputState changes
        // (hover popup / completion menu / search matches).
        let restored_tab_id = file_info.id.clone();
        self.editor_state_subscriptions.push(cx.observe(
            &editor_state,
            move |this, state_entity, cx| {
                // 与 open_file_with_content 相同的脏检查逻辑，恢复后同样需要维护未保存标记。
                let text_version = state_entity.read(cx).text_version;
                let idx = this
                    .open_files
                    .iter()
                    .position(|f| f.id == restored_tab_id);
                if let Some(idx) = idx
                    && this.open_files[idx].last_checked_text_version != text_version
                {
                    let dirty = this.file_is_dirty(&this.open_files[idx], cx);
                    this.open_files[idx].last_checked_text_version = text_version;
                    if this.open_files[idx].dirty != dirty {
                        this.open_files[idx].dirty = dirty;
                        this.sync_tab_modified(&restored_tab_id, dirty, cx);
                    }
                }
                cx.notify();
            },
        ));

        let file = EditorFile {
            id: file_info.id.clone(),
            path: path_str.clone(),
            label,
            ssh_config: ssh_config.clone(),
            editor_state: editor_state.clone(),
            // 恢复磁盘基线，保留文件树切换前的未保存状态；
            // 旧状态可能没有该字段（兼容 None = 以缓存内容为准）。
            original_content,
            encoding: file_info.encoding,
            read_only: file_info.read_only,
            pending_reload: false,
            reload_prompt_pending: false,
            is_loading: false,
            saving: false,
            close_prompt_pending: false,
            last_checked_text_version: 0,
            dirty: is_dirty,
            markdown,
            image: None,
            diff: None,
        };

        // 只读文件：恢复后同样置为禁用，避免可编辑后保存才报错。
        if file_info.read_only {
            editor_state.update(cx, |state, _| state.disabled = true);
        }

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
                    .with_icon(Some(file_icon_or_default(Path::new(&file_info.path))))
                    .with_modified(is_dirty),
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

                // 读取完整字节并检测编码（磁盘编码可能已变化），
                // 不再用 read_to_string：GBK 等非 UTF-8 文件也能正确重载。
                let loaded = if let Some(config) = ssh_config {
                    let path_for_task = path.clone();
                    cx.background_executor()
                        .spawn(async move {
                            let bytes =
                                crate::ssh::sftp_fs::read_file_bytes(&config, &path_for_task)
                                    .map_err(|e| e.to_string())?
                                    .ok_or_else(|| "文件不存在".to_string())?;
                            if crate::ssh::sftp_fs::is_binary_bytes(&bytes) {
                                return Err("内容为二进制".to_string());
                            }
                            Ok(crate::editor::encoding::decode_bytes(&bytes))
                        })
                        .await
                } else {
                    let path_for_task = path.clone();
                    cx.background_executor()
                        .spawn(async move {
                            let bytes = std::fs::read(&path_for_task).map_err(|e| e.to_string())?;
                            if crate::ssh::sftp_fs::is_binary_bytes(&bytes) {
                                return Err("内容为二进制".to_string());
                            }
                            Ok(crate::editor::encoding::decode_bytes(&bytes))
                        })
                        .await
                };

                match loaded {
                    Ok(decoded) => {
                        let content = decoded.text;
                        let encoding = decoded.encoding;
                        let _ = weak.update(cx, |this, cx| {
                            this.apply_external_reload(&path, content, encoding, cx);
                            cx.notify();
                        });
                    }
                    Err(e) => {
                        log::warn!("重载文件失败 {}: {}", path, e);
                        let _ = weak.update(cx, |this, _cx| {
                            let ids: Vec<SharedString> = this
                                .open_files
                                .iter()
                                .filter(|f| f.path == path)
                                .map(|f| f.id.clone())
                                .collect();
                            for id in ids {
                                if let Some(file) = this.open_files.iter_mut().find(|f| f.id == id) {
                                    file.pending_reload = false;
                                }
                            }
                        });
                    }
                }
            })
            .detach();

            return;
        }
    }

    /// 外部变更到达时，对同路径的所有 tab 应用三路处理：
    ///
    /// 1. 无本地修改 → 自动重载磁盘内容（含编码）；
    /// 2. 磁盘内容 == 编辑器内容（在别处保存/写入的正是当前文本）→ 只更新基线；
    /// 3. 本地修改与磁盘内容冲突 → 弹窗让用户选择「重新加载 / 保留我的修改」。
    fn apply_external_reload(
        &mut self,
        path: &str,
        content: String,
        encoding: FileEncoding,
        cx: &mut Context<Self>,
    ) {
        let matched_ids: Vec<SharedString> = self
            .open_files
            .iter()
            .filter(|f| f.path == path)
            .map(|f| f.id.clone())
            .collect();

        for id in matched_ids {
            let Some(file_ix) = self.open_files.iter().position(|f| f.id == id) else {
                continue;
            };

            let current = self.open_files[file_ix].editor_state.read(cx).value().to_string();
            let no_local_edits = current == self.open_files[file_ix].original_content;
            let disk_matches_editor = content == current;

            if no_local_edits || disk_matches_editor {
                // 分支 1+2：直接采用磁盘内容，同步基线/编码/标签。
                let changed = self.open_files[file_ix].original_content != content;
                self.open_files[file_ix].original_content = content.clone();
                self.open_files[file_ix].encoding = encoding;
                self.open_files[file_ix].dirty = false;
                if changed {
                    self.open_files[file_ix].editor_state.update(cx, |state, cx| {
                        state.set_text(content.clone(), cx);
                    });
                    if let Some(ref markdown) = self.open_files[file_ix].markdown {
                        markdown.update(cx, |renderer, cx| {
                            renderer.set_text(content.clone(), cx);
                        });
                    }
                }
                let tab_id = id.clone();
                self.tab_bar.update(cx, |tab_bar, cx| {
                    tab_bar.modify_tab(tab_id.as_ref(), |tab| tab.is_modified = false, cx);
                });
                // 同步更新远程文件内容缓存，避免重开后仍是旧内容
                if let Some(ref ssh_config) = self.open_files[file_ix].ssh_config {
                    crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                        .lock()
                        .unwrap()
                        .insert_file_content(
                            ssh_config,
                            std::path::Path::new(path),
                            content.clone(),
                            encoding,
                        );
                }
                self.open_files[file_ix].pending_reload = false;
                continue;
            }

            // 分支 3：冲突。
            log::info!(
                "File changed externally with unsaved local edits: {}",
                path
            );
            let file = &mut self.open_files[file_ix];
            file.dirty = true;
            let tab_id = id.clone();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.modify_tab(tab_id.as_ref(), |tab| tab.is_modified = true, cx);
            });

            // 已有弹窗（关闭确认 / 其他重载确认）挂着：不叠加原生弹窗，
            // 保持脏标记，等下一次事件再试。
            if self.any_prompt_pending() {
                self.open_files[file_ix].pending_reload = false;
                continue;
            }

            let Some(window_handle) = self.window_handle else {
                self.open_files[file_ix].pending_reload = false;
                continue;
            };
            // reload_prompt_pending 保持 true：弹窗期间抑制同路径后续事件。
            self.open_files[file_ix].reload_prompt_pending = true;
            let label = self.open_files[file_ix].label.clone();
            let weak = cx.entity().downgrade();
            let prompt_path = path.to_string();
            let prompt_tab_id = id.to_string();
            // 冲突分支把磁盘内容交给弹窗回调使用；克隆一份避免移走循环体
            // 后续 tab 仍在使用的 `content`。
            let prompt_content = content.clone();
            cx.spawn(async move |_, cx| {
                let answer = match cx.update_window(window_handle, |_view, window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        "文件已在磁盘上被修改",
                        Some(&format!(
                            "“{label}”在磁盘上的内容已变化，而编辑器中还有未保存的修改。\n\n选择“重新加载”将丢弃本地修改并载入磁盘内容。"
                        )),
                        &["重新加载", "保留我的修改"],
                        cx,
                    )
                }) {
                    Ok(promise) => promise.await.unwrap_or(1),
                    Err(_) => 1,
                };

                let _ = weak.update(cx, |this, cx| {
                    let Some(file_ix) = this
                        .open_files
                        .iter()
                        .position(|f| f.id.as_ref() == prompt_tab_id.as_str())
                    else {
                        return;
                    };
                    this.open_files[file_ix].reload_prompt_pending = false;
                    this.open_files[file_ix].pending_reload = false;
                    if answer == 0 {
                        // 重新加载：丢弃本地修改，采用磁盘内容与编码。
                        this.open_files[file_ix].original_content = prompt_content.clone();
                        this.open_files[file_ix].encoding = encoding;
                        this.open_files[file_ix].dirty = false;
                        this.open_files[file_ix].editor_state.update(cx, |state, cx| {
                            state.set_text(prompt_content.clone(), cx);
                        });
                        if let Some(ref markdown) = this.open_files[file_ix].markdown {
                            markdown.update(cx, |renderer, cx| {
                                renderer.set_text(prompt_content.clone(), cx);
                            });
                        }
                        if let Some(ref ssh_config) = this.open_files[file_ix].ssh_config {
                            crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                .lock()
                                .unwrap()
                                .insert_file_content(
                                    ssh_config,
                                    std::path::Path::new(&prompt_path),
                                    prompt_content.clone(),
                                    encoding,
                                );
                        }
                        this.tab_bar.update(cx, |tab_bar, cx| {
                            tab_bar.modify_tab(prompt_tab_id.as_str(), |tab| {
                                tab.is_modified = false
                            }, cx);
                        });
                    }
                    // 「保留我的修改」：维持现状（脏标记 + 旧基线），下次保存覆盖磁盘。
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// 任一标签页的原生确认弹窗（关闭确认 / 重载确认）是否已打开。
    fn any_prompt_pending(&self) -> bool {
        self.open_files
            .iter()
            .any(|f| f.close_prompt_pending || f.reload_prompt_pending)
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
        let mut paths: Vec<(PathBuf, bool)> = Vec::new();
        for f in &self.open_files {
            let path = PathBuf::from(&f.path);
            if !paths.iter().any(|(p, _)| p == &path) {
                paths.push((path, f.ssh_config.is_some()));
            }
        }
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

    /// cmd-l / ctrl-g：切换「跳转到行」悬浮面板。
    fn toggle_goto_line(&mut self, _: &GotoLine, window: &mut Window, cx: &mut Context<Self>) {
        // 面板已开（或当前不是文本编辑器）→ 关闭并把焦点还给编辑器
        let active_editor = self.active_text_editor(cx);
        if let Some(panel) = self.goto_line_panel.take() {
            if let Some(editor) = active_editor {
                editor.read(cx).focus_handle.clone().focus(window, cx);
            } else {
                panel.read(cx).input.read(cx).focus_handle.clone().focus(window, cx);
            }
            cx.notify();
            return;
        }
        let Some(editor) = active_editor else {
            return;
        };
        let handle = self
            .window_handle
            .unwrap_or_else(|| window.window_handle());
        let panel = goto_line::GotoLinePanel::new(editor, handle, window, cx);
        panel.update(cx, |p, cx| p.focus(window, cx));
        self.goto_line_panel = Some(panel);
        cx.notify();
    }

    /// 当前激活 tab 对应的文本编辑器（diff/图片/markdown 预览返回 None）。
    fn active_text_editor(&self, _cx: &App) -> Option<Entity<InputState>> {
        let active_id = self.active_file_id.as_ref()?;
        let file = self.open_files.iter().find(|f| f.id == *active_id)?;
        if file.diff.is_some() || file.image.is_some() || file.markdown.is_some() {
            return None;
        }
        Some(file.editor_state.clone())
    }

    /// 状态栏语言 chip → 弹出语言选择菜单。
    fn open_language_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::components::menu::PopupMenu;

        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        let current = editor
            .read(cx)
            .language()
            .map(|l| l.to_string())
            .unwrap_or_default();
        let weak = cx.entity().downgrade();
        let dismiss_weak = cx.entity().downgrade();

        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let mut menu = menu.label("切换语言");
            for lang in Language::all() {
                let id = lang.name().to_string();
                let display = lang.display_name().to_string();
                let selected = id == current;
                let weak = weak.clone();
                menu = menu.item(
                    crate::components::menu::PopupMenuItem::new(display)
                        .checked(selected)
                        .on_click(move |_, _window, cx| {
                            if let Some(editor) = weak
                                .update(cx, |this, cx| this.active_text_editor(cx))
                                .ok()
                                .flatten()
                            {
                                editor.update(cx, |state, cx| {
                                    state.set_language(id.as_str(), _window, cx)
                                });
                            }
                        }),
                );
            }
            menu
        });

        let subscription = window.subscribe(&menu, cx, move |_, _: &gpui::DismissEvent, _, cx| {
            let _ = dismiss_weak.update(cx, |this: &mut Editor, cx| {
                this.language_menu = None;
                this._language_menu_subscription = None;
                cx.notify();
            });
        });
        self.language_menu = Some(menu);
        self._language_menu_subscription = Some(subscription);
        self.language_menu_position = position;
        cx.notify();
    }

    /// 底部状态栏（仅文本编辑器）：行/列、选区、编码、缩进、语言。
    fn render_status_bar(
        &self,
        file: &EditorFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::components::input::RopeExt;

        let theme = cx.theme().ui;
        let state = file.editor_state.read(cx);
        let position = state.cursor_position(); // 0 基
        let selected_len = state.selected_range.len();
        let total = state.text().lines_len().max(1);
        let encoding_label = file.encoding.label();
        let language_label = state
            .language()
            .map(|l| {
                let id = l.to_string();
                Language::from_str(&id).display_name().to_string()
            })
            .unwrap_or_else(|| "纯文本".to_string());

        let chip = |label: String| {
            div()
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(8.0))
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(label)
        };

        div()
            .id("editor-status-bar")
            .flex()
            .items_center()
            .justify_between()
            .mx(px(10.0))
            .mb(px(6.0))
            .mt(px(2.0))
            .px(px(10.0))
            .py(px(3.0))
            .rounded(px(12.0))
            // 液态玻璃：半透明底 + 细边 + 柔和阴影
            .bg(theme.panel_background.opacity(0.72))
            .border_1()
            .border_color(theme.border.opacity(0.7))
            .shadow_sm()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(div().child(format!(
                        "行 {}, 列 {}",
                        position.line + 1,
                        position.character + 1
                    )))
                    .when(selected_len > 0, |this| {
                        this.child(
                            div()
                                .text_color(theme.text_muted.opacity(0.8))
                                .child(format!("（已选 {selected_len} 字符）")),
                        )
                    })
                    .child(
                        div()
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(format!("共 {total} 行")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(chip(format!("缩进: Tab")))
                    .child(chip(encoding_label.to_string()))
                    // 语言 chip：点击弹出语言菜单
                    .child(
                        div()
                            .id("language-chip")
                            .cursor_pointer()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(8.0))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .hover(|this| this.bg(theme.tab_hover_background))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                    this.open_language_menu(event.position, window, cx);
                                }),
                            )
                            .child(language_label),
                    ),
            )
            .into_any_element()
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

        if self.open_files.is_empty() {
            self.begin_open_animation();
        }
        self.open_files.push(EditorFile {
            id: tab_id.clone().into(),
            path: path.clone(),
            label,
            ssh_config: None,
            editor_state,
            original_content: String::new(),
            encoding: FileEncoding::Utf8,
            read_only: false,
            pending_reload: false,
            reload_prompt_pending: false,
            is_loading: false,
            saving: false,
            close_prompt_pending: false,
            last_checked_text_version: 0,
            dirty: false,
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

    /// 在 Markdown 预览中双击：以纯编辑模式（代码编辑器）在新 tab 中打开同一文件。
    pub fn open_file_in_edit_mode(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_local_file_inner(path, window, cx, true, true);
    }

    fn open_local_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
    ) {
        self.open_local_file_inner(path, window, cx, notify_app, false);
    }

    fn open_local_file_inner(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
        edit_mode: bool,
    ) {
        let path_str = path.to_string_lossy().to_string();
        let tab_id = if edit_mode {
            format!("editor-{}{}", path_str, Self::MARKDOWN_EDIT_SUFFIX)
        } else {
            format!("editor-{}", path_str)
        };
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
            FileEncoding::Utf8,
            false,
            window,
            cx,
            notify_app,
            true,
            edit_mode,
        );

        self.spawn_file_load(
            tab_id,
            load_gen,
            Arc::new(move || load_local_text(&path_for_task)),
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
        load_content: Arc<dyn Fn() -> Result<LoadedText, String> + Send + Sync>,
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
                    Ok(loaded) => {
                        let _ = weak.update(cx, |this, cx| {
                            if !this.is_load_current(&tab_id, load_generation) {
                                return;
                            }
                            this.loading_gens.remove(&tab_id);
                            if let Some(file) = this.open_files.iter_mut().find(|f| f.id == tab_id)
                            {
                                file.original_content = loaded.text.clone();
                                file.encoding = loaded.encoding;
                                file.read_only = loaded.read_only;
                                file.is_loading = false;
                                // 加载完成：尚未编辑，不显示未保存标记。
                                file.dirty = false;
                                // 只读文件：置为禁用，避免编辑后保存才报错。
                                file.editor_state.update(cx, |state, cx| {
                                    state.disabled = loaded.read_only;
                                    state.set_text(loaded.text.clone(), cx);
                                });
                                if let Some(ref markdown) = file.markdown {
                                    markdown.update(cx, |renderer, cx| {
                                        renderer.set_text(loaded.text.clone(), cx);
                                    });
                                }
                                // 缓存远程文件内容到 RemoteFileTreeCache
                                if let Some(ref ssh_config) = file.ssh_config {
                                    crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                        .lock()
                                        .unwrap()
                                        .insert_file_content(
                                            ssh_config,
                                            std::path::Path::new(&file.path),
                                            loaded.text.clone(),
                                            loaded.encoding,
                                        );
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
                                    this.close_file_direct(&tab_id, cx);
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
        self.open_remote_file_inner(path, ssh_config, window, cx, true, false);
    }

    pub fn open_remote_file_no_notify(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, false, false);
    }

    /// 在 Markdown 预览中双击：以纯编辑模式（代码编辑器）在新 tab 中打开同一远程文件。
    pub fn open_remote_file_in_edit_mode(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, true, true);
    }

    fn open_remote_file_inner(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
        edit_mode: bool,
    ) {
        let tab_id = if edit_mode {
            format!("editor-remote-{}{}", path, Self::MARKDOWN_EDIT_SUFFIX)
        } else {
            format!("editor-remote-{}", path)
        };

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

        if let Some((content, encoding)) = cached_content {
            // 有缓存，直接使用缓存内容打开文件（编码一并恢复，避免保存时写错编码）
            self.open_file_with_content(
                tab_id,
                path.clone(),
                content,
                PathBuf::from(&path),
                Some(ssh_config),
                encoding,
                false,
                window,
                cx,
                notify_app,
                false, // is_loading = false，因为有缓存内容
                edit_mode,
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
            FileEncoding::Utf8,
            false,
            window,
            cx,
            notify_app,
            true,
            edit_mode,
        );

        self.spawn_file_load(
            tab_id,
            load_gen,
            Arc::new(move || {
                // 必须读完整文件并检测编码：GBK 等非 UTF-8 内容
                // 也要能正确打开，二进制则报错拦截（旧实现把二进制当空文件打开，
                // 保存时会静默清空原文件）。
                match load_remote_text(&ssh_config_for_task, &path_for_task) {
                    Ok(loaded) => Ok(loaded),
                    Err(e) => {
                        log::warn!("读取远程文件失败 {}: {}", path_for_log, e);
                        Err(e)
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
            FileEncoding::Utf8,
            false,
            window,
            cx,
            notify_app,
            true,
            false, // 图片没有编辑模式
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
                                    this.close_file_direct(&tab_id, cx);
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
        encoding: FileEncoding,
        read_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        _notify_app: bool,
        is_loading: bool,
        edit_mode: bool,
    ) {
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&path_str)
            .to_string();
        // 编辑模式 tab 与预览 tab 区分标识，避免两个同名 tab 混淆
        let label = if edit_mode {
            format!("{} (编辑)", label)
        } else {
            label
        };
        let is_image = crate::editor::image_viewer::is_image_file(&path);
        let language = guess_language_from_extension(&path);

        let editor_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(language.clone())
                .multi_line(true);
            state.set_value(&content, window, cx);
            state.disabled = is_image || is_loading || read_only;
            state
        });

        // Re-render the Editor when this editor's InputState changes
        // (search matches / cursor): the popups are rendered by EditorInput,
        // which only re-renders when the Editor does.
        let sync_path = path_str.clone();
        let sync_tab_id = tab_id.clone();
        self.editor_state_subscriptions.push(
            cx.observe(&editor_state, move |this, state_entity, cx| {
                // 未保存标记：仅在文本版本变化时重新比较（O(1) 闸门，
                // 避免每次光标移动 / 重绘都对整个缓冲区做全量比较）。
                let text_version = state_entity.read(cx).text_version;
                let idx = this
                    .open_files
                    .iter()
                    .position(|f| f.id.as_ref() == sync_tab_id.as_str());
                if let Some(idx) = idx
                    && this.open_files[idx].last_checked_text_version != text_version
                {
                    let dirty = this.file_is_dirty(&this.open_files[idx], cx);
                    this.open_files[idx].last_checked_text_version = text_version;
                    if this.open_files[idx].dirty != dirty {
                        this.open_files[idx].dirty = dirty;
                        this.sync_tab_modified(&sync_tab_id, dirty, cx);
                    }
                }

                // 编辑 tab 内容变化时，同步刷新同路径的 markdown 预览 tab，
                // 保证双击进入编辑模式后，预览不会停留在旧内容上。
                // 先用轻量检查确认存在同路径预览，避免普通文件/大文件
                // 每次按键都深拷贝整个缓冲区（value() 会 to_string 全文）。
                if edit_mode
                    && this.open_files.iter().any(|f| {
                        f.id.as_ref() != sync_tab_id.as_str()
                            && f.path == sync_path
                            && f.markdown.is_some()
                    })
                {
                    let content = state_entity.read(cx).value().to_string();
                    let previews: Vec<Entity<crate::editor::markdown::Markdown>> = this
                        .open_files
                        .iter()
                        .filter(|f| {
                            f.id.as_ref() != sync_tab_id.as_str() && f.path == sync_path
                        })
                        .filter_map(|f| f.markdown.clone())
                        .collect();
                    for preview in previews {
                        let content = content.clone();
                        preview.update(cx, |renderer, cx| {
                            renderer.set_text(content, cx);
                        });
                    }
                }
                cx.notify();
            }),
        );

        let is_markdown = !is_image
            && !edit_mode
            && path.extension().and_then(|e| e.to_str()) == Some("md");
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

        if self.open_files.is_empty() {
            self.begin_open_animation();
        }
        self.open_files.push(EditorFile {
            id: tab_id.clone().into(),
            path: path_str,
            label: label.clone(),
            ssh_config,
            editor_state,
            original_content: content.clone(),
            encoding,
            read_only,
            pending_reload: false,
            reload_prompt_pending: false,
            is_loading,
            saving: false,
            close_prompt_pending: false,
            last_checked_text_version: 0,
            dirty: false,
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

    /// 判断文件当前是否有未保存的修改（编辑器文本 vs 磁盘基线）。
    fn file_is_dirty(&self, file: &EditorFile, cx: &mut App) -> bool {
        file.editor_state.read(cx).value().as_ref() != file.original_content.as_str()
    }

    /// 同步标签栏的未保存标记。
    fn sync_tab_modified(&self, tab_id: &str, dirty: bool, cx: &mut Context<Self>) {
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.modify_tab(tab_id, |tab| tab.is_modified = dirty, cx);
        });
    }

    /// 保存当前激活的标签页（普通编辑文件 / diff 视图）。
    fn save_active_file(&mut self, cx: &mut Context<Self>) {
        let Some(active_id) = self.active_file_id.clone() else {
            return;
        };
        self.save_file_by_id(&active_id, cx, false);
    }

    /// 保存指定标签页的编辑文件；`close_after_save` 用于关闭确认弹窗选「保存」后的流程。
    ///
    /// 保存失败时绝不关闭标签页，也绝不清除脏标记：编辑器内容必须保留，
    /// 并通过通知提示用户重试，避免静默丢数据。
    fn save_file_by_id(
        &mut self,
        tab_id: &str,
        cx: &mut Context<Self>,
        close_after_save: bool,
    ) {
        let idx = self.open_files.iter().position(|f| f.id.as_ref() == tab_id);
        let Some(idx) = idx else {
            return;
        };

        // diff 视图走 DiffView 自己的保存逻辑（基于 Git 后端，本地/远程）。
        if let Some(diff) = self.open_files[idx].diff.clone() {
            diff.update(cx, |view, cx| view.save(cx));
            return;
        }

        if self.open_files[idx].saving || self.open_files[idx].is_loading {
            return;
        }

        let text = self.open_files[idx].editor_state.read(cx).value().to_string();
        let original = self.open_files[idx].original_content.clone();
        if text == original {
            // 无修改：无需写入磁盘。
            return;
        }

        self.open_files[idx].saving = true;
        cx.notify();

        let path = self.open_files[idx].path.clone();
        let label = self.open_files[idx].label.clone();
        let ssh_config = self.open_files[idx].ssh_config.clone();
        let file_encoding = self.open_files[idx].encoding;
        let weak = cx.entity().downgrade();
        let tab_id = tab_id.to_string();

        cx.spawn(async move |_, cx| {
            // 后台写入，绝不阻塞 UI 线程（远程 SFTP 可能较慢）。
            let result = cx
                .background_executor()
                .spawn(async move {
                    save_text_to_file(
                        &path,
                        ssh_config.as_ref(),
                        &original,
                        &text,
                        file_encoding,
                    )
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                let Some(file) = this
                    .open_files
                    .iter_mut()
                    .find(|f| f.id.as_ref() == tab_id)
                else {
                    return;
                };
                file.saving = false;
                match result {
                    Ok(written) => {
                        // 磁盘基线更新为实际写入的内容：脏标记清除，
                        // 且随后的 watcher 事件（本地 notify / 远程轮询）
                        // 会走「无变化重载」路径，不会覆盖编辑器内容。
                        file.original_content = written.clone();
                        file.dirty = false;
                        if let Some(config) = &file.ssh_config {
                            crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                .lock()
                                .unwrap()
                                .insert_file_content(
                                    config,
                                    std::path::Path::new(&file.path),
                                    written,
                                    file_encoding,
                                );
                        }
                        let msg = format!("已保存 {label}");
                        this.sync_tab_modified(&tab_id, false, cx);
                        this.show_notification(msg, NotificationKind::Success, cx);
                        if close_after_save {
                            this.close_file_direct(&tab_id, cx);
                            return;
                        }
                    }
                    Err(e) => {
                        this.show_notification(
                            format!("保存失败: {e}"),
                            NotificationKind::Error,
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_file(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.loading_gens.remove(id);
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            // 保存进行中：不允许关闭，否则保存结束前的额外编辑可能丢失；
            // 保存完成（或失败）后标签页会保持打开，用户可再次关闭。
            if self.open_files[index].saving {
                return;
            }
            // 有未保存更改时先弹出确认框，避免误关丢数据。
            if self.file_is_dirty(&self.open_files[index], cx)
                && !self.open_files[index].close_prompt_pending
            {
                // 已有其它标签页的确认框在弹出：原生弹窗不可叠加，
                // 本次不弹、也不关闭，等用户处理完当前弹窗后再试。
                if self.any_prompt_pending() {
                    return;
                }
                let label = self.open_files[index].label.clone();
                self.open_files[index].close_prompt_pending = true;
                cx.notify();
                let weak = cx.entity().downgrade();
                let tab_id = id.to_string();
                let receiver = window.prompt(
                    gpui::PromptLevel::Warning,
                    &format!("“{label}”有未保存的更改"),
                    Some("关闭前是否保存？"),
                    &["保存", "不保存", "取消"],
                    cx,
                );
                cx.spawn(async move |_, cx| {
                    let answer = receiver.await.unwrap_or(2);
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(file) = this
                            .open_files
                            .iter_mut()
                            .find(|f| f.id.as_ref() == tab_id)
                        {
                            file.close_prompt_pending = false;
                        }
                        match answer {
                            // 保存成功后由 save_file_by_id 的 close_after_save 关闭；
                            // 保存失败则保持打开并提示错误。
                            0 => this.save_file_by_id(&tab_id, cx, true),
                            1 => this.close_file_direct(&tab_id, cx),
                            _ => {
                                cx.notify();
                            }
                        }
                    })
                    .ok();
                })
                .detach();
                return;
            }
            self.close_file_direct(id, cx);
        }
    }

    /// 直接关闭标签页（不做未保存确认）。
    ///
    /// 仅在以下场景使用：
    /// - 已通过 close_file 的弹窗确认；
    /// - 文件加载失败后关闭（加载失败的文件保持 disabled，不可能有未保存内容）。
    fn close_file_direct(&mut self, id: &str, cx: &mut Context<Self>) {
        self.loading_gens.remove(id);
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let path = self.open_files[index].path.clone();
            let is_remote = self.open_files[index].ssh_config.is_some();
            // 同一路径可能同时打开预览/编辑两个 tab：
            // 仅当这是最后一个引用该路径的 tab 时才取消文件监听并清理内容缓存。
            let has_other_tab = self
                .open_files
                .iter()
                .filter(|f| f.path == path && f.id.as_ref() != id)
                .next()
                .is_some();
            if !has_other_tab {
                // 文件关闭后同步清理 RemoteFileTreeCache 中的内容缓存，避免重新打开时看到旧内容
                if is_remote {
                    crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                        .lock()
                        .unwrap()
                        .remove_file(
                            self.open_files[index].ssh_config.as_ref().unwrap(),
                            std::path::Path::new(&path),
                        );
                }
                if is_remote {
                    Self::unwatch_remote_file(Path::new(&path), cx);
                } else {
                    Self::unwatch_file(Path::new(&path), cx);
                }
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
            // 最后一个文件关闭：编辑器收起（宽度动画到 0）
            if self.open_files.is_empty() {
                self.begin_close_animation();
            }
        }
        cx.notify();
    }

    fn close_other_tabs(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .open_files
            .iter()
            .filter(|f| f.id.as_ref() != id)
            .map(|f| f.id.to_string())
            .collect();
        for file_id in ids {
            self.close_file(&file_id, window, cx);
        }
    }

    fn close_right_tabs(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let ids: Vec<String> = self.open_files[index + 1..]
                .iter()
                .map(|f| f.id.to_string())
                .collect();
            for file_id in ids {
                self.close_file(&file_id, window, cx);
            }
        }
    }

    fn close_left_tabs(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let ids: Vec<String> = self.open_files[..index]
                .iter()
                .map(|f| f.id.to_string())
                .collect();
            for file_id in ids {
                self.close_file(&file_id, window, cx);
            }
        }
    }

    fn close_all_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.open_files.iter().map(|f| f.id.to_string()).collect();
        for file_id in ids {
            self.close_file(&file_id, window, cx);
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
                        cx.defer_in(window, move |this, window, cx| {
                            this.close_file(&id, window, cx);
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                        let path = file.path.clone();
                        let ssh_config = file.ssh_config.clone();
                        div()
                            .id("md-preview")
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
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        // 双击预览区域：新开 tab 进入 md 编辑模式
                                        if event.click_count == 2 {
                                            if let Some(config) = &ssh_config {
                                                this.open_remote_file_in_edit_mode(
                                                    path.clone(),
                                                    config.clone(),
                                                    window,
                                                    cx,
                                                );
                                            } else {
                                                this.open_file_in_edit_mode(
                                                    PathBuf::from(&path),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        }
                                    },
                                ),
                            )
                            .child(markdown.clone())
                            .into_any_element()
                    } else {
                        let status_bar = self.render_status_bar(file, window, cx);
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .bg(theme.app_background)
                            .when(self.content_left_padding > 0.0, |this| {
                                this.pl(px(self.content_left_padding))
                            })
                            .when(self.content_bottom_padding > 0.0, |this| {
                                this.pb(px(self.content_bottom_padding))
                            })
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_h_0()
                                    .child(EditorInput::new(&file.editor_state))
                                    // 跳转到行：顶部居中悬浮卡片
                                    .when_some(
                                        self.goto_line_panel.clone(),
                                        |this, panel| {
                                            this.child(
                                                div()
                                                    .absolute()
                                                    .top(px(8.0))
                                                    .left_0()
                                                    .right_0()
                                                    .flex()
                                                    .justify_center()
                                                    .child(panel),
                                            )
                                        },
                                    ),
                            )
                            .child(status_bar)
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

        // 展开/收起动画：外层 wrapper 控制可见宽度（动画期间由 with_animation 从
        // anim_start 驱动到 anim_end），内层 root 用 w_full() 填满 wrapper ——
        // 否则内层固定在 anim_start(=0) 会导致内容/分割线全部 0 宽不可见。
        // epoch == 0（从未播放过动画，如启动/恢复）或动画目标与当前状态不匹配
        // （clear() 等状态突变但 epoch 未变）时直接应用目标宽度，避免残留旧动画终值。
        let anim_epoch = self.open_anim_epoch;
        let (anim_start, anim_end) = (self.open_anim_start, self.open_anim_end);
        let target_width = if is_empty { 0.0 } else { self.width };
        let animating = anim_epoch > 0 && (anim_end - target_width).abs() <= 0.5;
        let display_width = if animating {
            anim_start
        } else {
            target_width
        };

        let root: gpui::AnyElement = div()
            .key_context("EditorPanel")
            .on_action(cx.listener(|this, _: &CloseActiveEditorTab, window, cx| {
                if let Some(active_id) = this.active_file_id.clone() {
                    let id = active_id.as_ref().to_string();
                    this.close_file(&id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SaveActiveEditorTab, _window, cx| {
                this.save_active_file(cx);
            }))
            .on_action(cx.listener(|this, _: &GotoLine, window, cx| {
                this.toggle_goto_line(&GotoLine, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_file(&tab_id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseOtherTabs, window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_other_tabs(&tab_id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseRightTabs, window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_right_tabs(&tab_id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseAllTabs, window, cx| {
                this.close_all_tabs(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseLeftTabs, window, cx| {
                if let Some(tab_id) = this.tab_bar.read(cx).context_menu_tab_id() {
                    this.close_left_tabs(&tab_id, window, cx);
                }
            }))
            .w_full()
            .h_full()
            .relative()
            .flex()
            .flex_col()
            .items_start()
            .overflow_hidden()
            .child(tab_bar)
            .child(content)
            .child(resize_handle)
            // 语言选择弹出菜单：与 ContextMenu 相同的 deferred+anchored 机制
            .when_some(self.language_menu.clone(), |this, menu| {
                this.child(
                    gpui::deferred(
                        gpui::anchored().child(
                            div()
                                .w(window.bounds().size.width)
                                .h(window.bounds().size.height)
                                .on_scroll_wheel(|_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(
                                    gpui::anchored()
                                        .position(self.language_menu_position)
                                        .snap_to_window_with_margin(px(8.0))
                                        .child(menu.clone()),
                                ),
                        ),
                    )
                    .with_priority(1),
                )
            })
            .when(!is_empty, |this| {
                this.child(EditorResizeElement {
                    editor: cx.entity().downgrade(),
                })
            })
            .into_any_element();

        let wrapper = div().w(px(display_width)).h_full().child(root);
        if animating {
            wrapper
                .with_animation(
                    ("editor-open-anim", anim_epoch),
                    Animation::new(Duration::from_millis(EDITOR_OPEN_ANIMATION_MS))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| this.w(px(anim_start + (anim_end - anim_start) * delta)),
                )
                .into_any_element()
        } else {
            wrapper.into_any_element()
        }
    }
}

/// 文件换行风格：保存时按文件的原始风格还原，避免 CRLF 文件被改写成 LF（或反之）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineEnding {
    Unix,
    Windows,
}

impl LineEnding {
    /// 检测文本的换行风格：只看前 1000 字符内第一个换行符（与 Zed 一致）。
    pub(crate) fn detect(text: &str) -> Self {
        let mut max_ix = std::cmp::min(text.len(), 1000);
        while !text.is_char_boundary(max_ix) {
            max_ix -= 1;
        }

        if let Some(ix) = text[..max_ix].find('\n') {
            if ix > 0 && text.as_bytes()[ix - 1] == b'\r' {
                Self::Windows
            } else {
                Self::Unix
            }
        } else {
            #[cfg(windows)]
            {
                Self::Windows
            }
            #[cfg(not(windows))]
            {
                Self::Unix
            }
        }
    }
}

/// 保存前的换行统一与还原：
/// 先把编辑器中可能混用的 `\r\n` 统一为 `\n`，再按文件原始风格还原，
/// 避免把 `\r\n` 直接 `.replace('\n', "\r\n")` 变成 `\r\r\n`。
pub(crate) fn normalize_line_endings(text: &str, line_ending: LineEnding) -> String {
    let unified = text.replace("\r\n", "\n");
    match line_ending {
        LineEnding::Unix => unified,
        LineEnding::Windows => unified.replace('\n', "\r\n"),
    }
}

/// 将文本写入本地或远程文件，返回实际写入的内容（用于更新磁盘基线）。
///
/// - 本地：创建缺失的父目录后直接覆盖写入（与 Zed 的 LocalFs::save 一致，
///   保留已存在 inode 的权限位）。
/// - 远程：通过 SFTP 按原始字节覆盖写入（复用连接缓存，失败自动重连一次）。
/// - 按文件原编码写回；若文本含该编码无法表示的字符则报错（阻止保存），
///   避免静默替换为 `?` 破坏内容。
pub(crate) fn save_text_to_file(
    path: &str,
    ssh_config: Option<&SshConnectionConfig>,
    original: &str,
    text: &str,
    encoding: FileEncoding,
) -> Result<String, String> {
    let line_ending = LineEnding::detect(original);
    let output = normalize_line_endings(text, line_ending);

    let (bytes, had_errors) = encoding::encode_text(&output, encoding);
    if had_errors {
        return Err(format!(
            "文件为 {} 编码，内容包含该编码无法表示的字符（如 emoji 或特殊符号）。\n可将文件编码转换为 UTF-8 后重试。",
            encoding.label()
        ));
    }

    if let Some(config) = ssh_config {
        crate::ssh::sftp_fs::write_file_bytes(config, path, &bytes)
            .map_err(|e| format!("写入远程文件失败: {e}"))?;
    } else {
        let p = Path::new(path);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录失败: {e}"))?;
            }
        }
        std::fs::write(p, &bytes).map_err(|e| format!("写入本地文件失败: {e}"))?;
    }
    Ok(output)
}

pub(crate) fn guess_language_from_extension(path: &Path) -> String {
    // Makefile 通常没有扩展名：按文件名匹配（Makefile / GNUmakefile / BSDmakefile）。
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if lower == "makefile"
            || lower == "gnumakefile"
            || lower == "bsdmakefile"
            || lower == "makefile.am"
            || lower == "makefile.in"
        {
            return "make".to_string();
        }
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| match ext {
            "mk" | "mak" | "make" => "make",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_line_ending_lf() {
        assert_eq!(LineEnding::detect("line1\nline2\n"), LineEnding::Unix);
        assert_eq!(LineEnding::detect("hello world"), LineEnding::Unix); // 无换行 → 平台默认
    }

    #[test]
    fn detect_line_ending_crlf() {
        assert_eq!(
            LineEnding::detect("line1\r\nline2\r\n"),
            LineEnding::Windows
        );
        // 混合换行：以第一个换行符为准
        assert_eq!(
            LineEnding::detect("first\r\nsecond\nthird"),
            LineEnding::Windows
        );
        assert_eq!(
            LineEnding::detect("first\nsecond\r\nthird"),
            LineEnding::Unix
        );
    }

    #[test]
    fn normalize_line_endings_unix() {
        assert_eq!(
            normalize_line_endings("a\r\nb\nc", LineEnding::Unix),
            "a\nb\nc"
        );
        // 无任何换行
        assert_eq!(normalize_line_endings("plain", LineEnding::Unix), "plain");
    }

    #[test]
    fn normalize_line_endings_windows() {
        // 编辑器中已含 CRLF：不能双份转换（\r\r\n）
        assert_eq!(
            normalize_line_endings("a\r\nb\nc", LineEnding::Windows),
            "a\r\nb\r\nc"
        );
        assert_eq!(
            normalize_line_endings("a\nb\nc", LineEnding::Windows),
            "a\r\nb\r\nc"
        );
    }

    #[test]
    fn save_text_to_file_local_roundtrip_preserves_crlf() {
        let dir = std::env::temp_dir().join(format!(
            "deepterm-save-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crlf.txt");
        let original = "a\r\nb\r\nc\r\n";
        let edited = "a\r\nX\nc\r\n"; // 用户编辑混入了 LF
        std::fs::write(&path, original).unwrap();
        let written = save_text_to_file(
            path.to_str().unwrap(),
            None,
            original,
            edited,
            FileEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(written, "a\r\nX\r\nc\r\n");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "a\r\nX\r\nc\r\n");
        assert!(!on_disk.contains("\r\r\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_text_to_file_local_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "deepterm-save-test-{}-parent",
            std::process::id()
        ));
        let path = dir.join("sub").join("new.txt");
        let written = save_text_to_file(
            path.to_str().unwrap(),
            None,
            "",
            "hello\n",
            FileEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(written, "hello\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_text_to_file_gbk_roundtrip() {
        let dir = std::env::temp_dir().join(format!("deepterm-save-gbk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gbk.txt");
        let written = save_text_to_file(
            path.to_str().unwrap(),
            None,
            "旧基线",
            "你好，世界\n",
            FileEncoding::Gbk,
        )
        .unwrap();
        assert_eq!(written, "你好，世界\n");
        // 磁盘上是 GBK 字节，不足 UTF-8
        let bytes = std::fs::read(&path).unwrap();
        assert_ne!(bytes, "你好，世界\n".as_bytes());
        let decoded = crate::editor::encoding::decode_bytes(&bytes);
        assert_eq!(decoded.encoding, FileEncoding::Gbk);
        assert_eq!(decoded.text, "你好，世界\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_text_to_file_gbk_unencodable_blocks_save() {
        let err = save_text_to_file(
            "/tmp/never-written.txt",
            None,
            "",
            "emoji: \u{1f600}",
            FileEncoding::Gbk,
        )
        .unwrap_err();
        assert!(err.contains("GBK"));
    }
}
