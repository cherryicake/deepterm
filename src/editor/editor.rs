use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, BoxShadow, Context, CursorStyle, Element, ElementId, Entity,
    GlobalElementId, IntoElement, KeyBinding, ParentElement, Pixels, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, actions, div, point, px,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputState};
use crate::components::notification::NotificationKind;
use crate::editor::encoding::{self, FileEncoding};
use crate::editor::git_marks::compute_git_diff_summary;
use crate::editor::git_repository::GitRepository;
use crate::editor::goto_line;
use crate::editor::image_viewer::{ActualSize, FitToView, ResetZoom, ZoomIn, ZoomOut};
use crate::editor::syntax::languages::Language;
use crate::assets::file_icon_or_default;
use crate::file_manager::filesystem::RemoteFileSystem;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;
use crate::workspace::tab_bar::{DraggedTab, TabBar, TabItem};

const EDITOR_DEFAULT_WIDTH: f32 = 680.0;
const EDITOR_MIN_WIDTH: f32 = 200.0;
/// 窗口拉宽/窄时编辑器吃掉的增量比例：0.5 与主区（终端）平分，
/// 1.0 则编辑器独占（终端宽度只随内部分屏变化）。
const EDITOR_WINDOW_FOLLOW_RATIO: f32 = 0.5;

/// 图片解码任务：从本地磁盘或远端 SFTP 读取字节并解码，返回 `(图片, 像素尺寸)`。
/// 用 `Arc` 包装以便在后台执行器与 UI 线程之间共享。
type ImageLoader = Arc<dyn Fn() -> Result<(Arc<gpui::Image>, (u32, u32)), String> + Send + Sync>;

/// 编辑器展开/收起动画时长（与侧边栏展开动画一致，ease_in_out）。
const EDITOR_OPEN_ANIMATION_MS: u64 = 120;

actions!(
    editor_panel,
    [CloseActiveEditorTab, SaveActiveEditorTab, GotoLine]
);
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
    crate::editor::markdown::Markdown::init(cx);
    // 实时 markdown 与只读预览共用复制/全选语义，仅上下文不同。
    crate::keymap::bind(cx, [
        KeyBinding::new("cmd-c", crate::editor::markdown::Copy, Some("LiveMarkdown")),
        KeyBinding::new("ctrl-c", crate::editor::markdown::Copy, Some("LiveMarkdown")),
        KeyBinding::new("cmd-a", crate::editor::markdown::SelectAll, Some("LiveMarkdown")),
        KeyBinding::new("ctrl-a", crate::editor::markdown::SelectAll, Some("LiveMarkdown")),
    ]);
    crate::keymap::bind(cx, [
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

/// 路径前缀替换：`from` 自身 → `to`，`from` 的后代 → `to` 下的同层级路径。
/// 与文件树 `rekey_path` 同语义，供已打开标签跟随目录重命名。
fn rekey_open_path(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    if path == from {
        return Some(to.to_path_buf());
    }
    path.strip_prefix(from).ok().map(|rest| to.join(rest))
}

/// `path` 是否等于 `ancestor` 或位于其下。
fn path_is_under(path: &Path, ancestor: &Path) -> bool {
    path == ancestor || path.starts_with(ancestor)
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
    /// 同屏实时 markdown 编辑器（`#live` tab）：持有文档唯一真相源。
    live: Option<Entity<crate::editor::markdown_live::LiveMarkdownView>>,
    image: Option<Entity<crate::editor::image_viewer::ImageViewer>>,
    diff: Option<Entity<crate::editor::diff_view::DiffView>>,
    /// diff tab 的仓库（含 root + local/remote 后端）：`path` 存的是仓库相对路径，
    /// 切标签页组后重建 `DiffView` 需要它（后台重新 `diff_file_contents`）。
    diff_repo: Option<crate::editor::git_repository::GitRepository>,
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
    /// diff tab 的仓库快照（仅 `diff-` tab 为 Some）：恢复时后台重载 diff，
    /// 而不是走纯文本 `restore_file_from_cache`（否则页面空白）。
    pub diff_repo: Option<crate::editor::git_repository::GitRepository>,
}

/// 后台加载得到的文件内容：解码后的文本 + 检测到的编码 + 只读标记。
pub(crate) struct LoadedText {
    pub text: String,
    pub encoding: FileEncoding,
    pub read_only: bool,
}

/// 编辑器允许打开的最大文件（字节）。超过则拒绝，避免一次分配过大导致 OOM。
const MAX_EDITOR_FILE_MB: usize = 20;
const MAX_EDITOR_FILE_BYTES: usize = MAX_EDITOR_FILE_MB * 1024 * 1024;

/// 读取并解码本地文件字节（后台线程调用）。
pub(crate) fn load_local_text(path: &Path) -> Result<LoadedText, String> {
    let too_large = |len: u64, with_hint: bool| {
        let mb = len / (1024 * 1024);
        if with_hint {
            format!("文件过大（{mb} MB），超过 {MAX_EDITOR_FILE_MB} MB 限制，请用外部编辑器打开")
        } else {
            format!("文件过大（{mb} MB），超过 {MAX_EDITOR_FILE_MB} MB 限制")
        }
    };
    // 先用 metadata 粗筛，避免对超大文件做无谓的 read
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_EDITOR_FILE_BYTES as u64 {
            return Err(too_large(meta.len(), true));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_EDITOR_FILE_BYTES {
        return Err(too_large(bytes.len() as u64, false));
    }
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

/// Reads and decodes a remote file (permission check is deferred to avoid extra RTT).
pub(crate) fn load_remote_text(
    config: &SshConnectionConfig,
    path: &str,
) -> Result<LoadedText, String> {
    // 关键性能修复：先经 `stat` 预检大小，>20MB 直接拒绝，
    // 避免对 1GB 远端文件全量 `read_to_end` 进内存导致 OOM/卡死。
    // `metadata` 走 SFTP 会话缓存，仅一次往返；与本地 `load_local_text` 的
    // `metadata` 粗筛对称。>512KB 的大文件后续走流水线读取（见 sftp_fs）。
    if let Ok(meta) = crate::ssh::sftp_fs::metadata(config, path) {
        if meta.size > MAX_EDITOR_FILE_BYTES as u64 {
            return Err(format!(
                "文件过大（{} MB），超过 {MAX_EDITOR_FILE_MB} MB 限制",
                meta.size / (1024 * 1024)
            ));
        }
    }
    let bytes = crate::ssh::sftp_fs::read_file_bytes(config, path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "文件不存在或为空".to_string())?;
    if bytes.len() > MAX_EDITOR_FILE_BYTES {
        return Err(format!(
            "文件过大（{} MB），超过 {MAX_EDITOR_FILE_MB} MB 限制",
            bytes.len() / (1024 * 1024)
        ));
    }
    if crate::ssh::sftp_fs::is_binary_bytes(&bytes) {
        return Err("二进制文件，无法作为文本编辑".to_string());
    }
    let decoded = encoding::decode_bytes(&bytes);
    Ok(LoadedText {
        text: decoded.text,
        encoding: decoded.encoding,
        read_only: false,
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
    /// diff tab 的视图缓存：切组时整实体保留，未保存的右侧编辑、滚动、折叠状态不丢。
    pub cached_diff_views: HashMap<SharedString, Entity<crate::editor::diff_view::DiffView>>,
    pub active_file_id: Option<SharedString>,
}

pub struct Editor {
    tab_bar: Entity<TabBar>,
    open_files: Vec<EditorFile>,
    active_file_id: Option<SharedString>,
    width: f32,
    resize_drag_x: Option<Pixels>,
    /// 上帧窗口宽：`render` 用它算窗口增量并按比例跟随（`width` 保持
    /// 固定 px，拖拽/最小宽度语义不变）。0 表示尚未记录（首帧只记不跟）。
    last_window_width: f32,
    /// 窗口跟随的宽度上限：由 AppWindow 每帧下发（窗口宽 − 侧边栏占位 −
    /// 主区最小宽）。`f32::INFINITY` 表示未下发（如单测），不设上限。
    window_follow_limit: f32,
    app_window: Option<WeakEntity<crate::AppWindow>>,
    /// 主窗口句柄：fs 事件回调中没有 Window，弹窗时用它取回窗口。
    window_handle: Option<gpui::AnyWindowHandle>,
    tab_bar_pad_epoch: u32,
    tab_bar_pad_start: f32,
    tab_bar_pad_end: f32,
    tab_bar_pad_duration_ms: u64,
    /// 编辑器「空 ↔ 非空」翻转纪元：每次转变 +1。AppWindow 用它同步标题栏
    /// padding / tab 栏内边距动画，因此**不能**被侧边栏跟随（面板宽度动画）污染。
    open_anim_epoch: u32,
    /// 面板宽度动画纪元：开合与侧边栏跟随共用，作为 with_animation 的 key。
    panel_anim_epoch: u32,
    /// 面板宽度动画起始宽度
    panel_anim_start: f32,
    /// 面板宽度动画目标宽度
    panel_anim_end: f32,
    /// 面板宽度动画时长（开合 120ms；侧边栏跟随与侧边栏动画等长）
    panel_anim_ms: u64,
    /// 恢复文件树/会话状态期间为 true：禁止播放展开动画（避免切换标签页组/文件树时闪动）
    suppress_open_animation: bool,
    /// 恢复（切标签页组/文件树）后请求 AppWindow 静态对齐 tab 栏 pad。
    ///
    /// 恢复期间编辑器内容被整体替换，pad 的目标值（随「编辑器空满 × 侧边栏
    /// 显隐」变化）无人同步；残留的旧 epoch 会在下次渲染时带着旧起点重播一次
    /// 滑动动画。AppWindow 消费此标志后按当前目标静态应用。
    tab_bar_pad_snap_request: bool,
    content_left_padding: f32,
    content_bottom_padding: f32,
    watcher_subscription: Option<Subscription>,
    remote_watcher_subscription: Option<Subscription>,
    /// Subscriptions to each open editor's InputState so that state changes
    /// (hover popup, completion menu, search matches, cursor, …) re-render
    /// the Editor — the popups are rendered by EditorInput, not by the input
    /// element itself, so without this the UI never updates.
    /// key = tab_id, value = subscription; removed when tab closes to avoid leak.
    editor_state_subscriptions: HashMap<SharedString, Subscription>,
    /// tab_id -> in-flight load generation (supersedes stale tasks after tab-group swap).
    loading_gens: HashMap<String, u64>,
    /// live tab 的影子 `InputState` 待同步集合：live 编辑器不再每次击键
    /// 都把全文写进影子 `InputState`（那是 `Rope` 重建 + 整篇软换行，纯浪费
    /// —— live tab 根本不渲染它，只有保存/切标签页组/关闭确认才读）。
    /// 击键只登记版本号并起一个防抖任务，真正同步时取最终文本。
    pending_live_sync: HashMap<SharedString, u64>,
    /// tab_id -> pending `Cmd+Click` line/col jump (applied after async load).
    pending_jumps: HashMap<String, (u32, Option<u32>)>,
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
    /// Markdown 实时预览编辑 tab 的 id 后缀：同屏所见即所得，直接编辑渲染后的文档。
    const MARKDOWN_LIVE_SUFFIX: &str = "#live";

    /// live tab 影子 `InputState` 同步的防抖窗口。影子状态只服务于
    /// 保存/切组/关闭确认，不参与渲染；连续打字只在停顿后同步一次最终文本。
    const LIVE_SHADOW_SYNC_DEBOUNCE_MS: u64 = 250;

    /// md 文档默认走实时渲染：与 `open_file_with_content` 的 live 门控保持一致（仅 `.md`）。
    /// 显式指定的 `edit_mode` 不会被劫持；`live_mode` 已为 true 时直接透传。
    fn default_live_mode(path: &std::path::Path, edit_mode: bool, live_mode: bool) -> bool {
        live_mode || (!edit_mode && path.extension().and_then(|e| e.to_str()) == Some("md"))
    }

    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tab_bar = TabBar::new("editor-tab-bar", cx);
        tab_bar.update(cx, |bar, cx| {
            bar.set_transparent_bg(true, cx);
            bar.set_compact(true, cx);
            // 编辑器栏只显示文件：agent 图标 / 状态点 / 扫光都属于跑着 agent 的
            // 终端标签，混进来会让人误以为「这个文件在跑 agent」。
            bar.set_accepts_agent_state(false, cx);
        });

        let mut this = Self {
            tab_bar,
            open_files: Vec::new(),
            active_file_id: None,
            width: EDITOR_DEFAULT_WIDTH,
            resize_drag_x: None,
            last_window_width: 0.0,
            window_follow_limit: f32::INFINITY,
            app_window: None,
            window_handle: Some(_window.window_handle()),
            tab_bar_pad_epoch: 0,
            tab_bar_pad_start: 6.0,
            tab_bar_pad_end: 6.0,
            tab_bar_pad_duration_ms: 0,
            open_anim_epoch: 0,
            panel_anim_epoch: 0,
            panel_anim_start: 0.0,
            panel_anim_end: 0.0,
            panel_anim_ms: EDITOR_OPEN_ANIMATION_MS,
            suppress_open_animation: false,
            tab_bar_pad_snap_request: false,
            content_left_padding: 0.0,
            content_bottom_padding: 0.0,
            watcher_subscription: None,
            remote_watcher_subscription: None,
            editor_state_subscriptions: HashMap::new(),
            loading_gens: HashMap::new(),
            pending_live_sync: HashMap::new(),
            pending_jumps: HashMap::new(),
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
        self.panel_anim_epoch = self.panel_anim_epoch.wrapping_add(1);
        self.panel_anim_start = 0.0;
        self.panel_anim_end = self.width;
        self.panel_anim_ms = EDITOR_OPEN_ANIMATION_MS;
    }

    /// 编辑器从打开 → 关闭（最后一个文件关闭）：记录收起动画（当前宽度 → 0）。
    fn begin_close_animation(&mut self) {
        self.open_anim_epoch = self.open_anim_epoch.wrapping_add(1);
        self.panel_anim_epoch = self.panel_anim_epoch.wrapping_add(1);
        self.panel_anim_start = self.width;
        self.panel_anim_end = 0.0;
        self.panel_anim_ms = EDITOR_OPEN_ANIMATION_MS;
    }

    /// 内容被整体替换（恢复文件树/标签页组状态）后，把面板宽度动画槽钉死为
    /// 「当前宽度 → 当前宽度」：渲染层的「起止无差」守卫会跳过插值，恢复后
    /// 第一帧就是最终宽度。
    ///
    /// 不做这一步的话，恢复前残留的 `panel_anim_*`（旧起点）会与新内容一起
    /// 渲染，表现为编辑器/终端整体滑动一下。
    fn snap_panel_width(&mut self) {
        self.panel_anim_epoch = 0;
        self.panel_anim_start = self.width;
        self.panel_anim_end = self.width;
        self.panel_anim_ms = 0;
    }

    /// AppWindow 取走「恢复后需要静态对齐 tab 栏 pad」的请求（取走即清除）。
    ///
    /// 恢复期间 `open_anim_epoch` 不 bump（`suppress_open_animation`），
    /// `sidebar_epoch` 也不变，pad 的三条同步路径都不进；若不显式对齐，
    /// `tab_bar_pad_end` 会保持旧值并在下次渲染重播一次滑动动画。
    pub fn take_tab_bar_pad_snap_request(&mut self) -> bool {
        std::mem::take(&mut self.tab_bar_pad_snap_request)
    }

    /// 侧边栏改变了宽度（显隐动画或拖拽）：编辑器按
    /// `EDITOR_WINDOW_FOLLOW_RATIO` 分摊其中一部分，其余留给主区（终端）。
    ///
    /// `sidebar_delta` 是侧边栏宽度的变化量（正 = 侧边栏变宽、主区可用宽度
    /// 变小）。`ms > 0` 时播放等长动画（显隐，由调用方与侧边栏动画对齐，
    /// 保证两者同步插值、终端不会先跳变再被拉回）；`ms == 0` 时立即结算
    /// （拖拽需要 1:1 跟手）。
    ///
    /// `width` 始终立即写为目标值（作为窗口跟随/拖拽的基准），动画只负责
    /// 渲染插值；拖拽或窗口缩放一旦改动 `width`，渲染层的目标守卫会自动
    /// 取消该动画，与开合动画同一套失效逻辑。
    pub fn follow_sidebar_width(
        &mut self,
        sidebar_delta: f32,
        limit: f32,
        ms: u64,
        cx: &mut Context<Self>,
    ) {
        // 空编辑器不参与布局（宽度目标为 0），变化的宽度全部归主区。
        if self.is_empty() {
            return;
        }
        let max_w = limit.max(EDITOR_MIN_WIDTH);
        let start = self.width;
        let end = (self.width - sidebar_delta * EDITOR_WINDOW_FOLLOW_RATIO)
            .clamp(EDITOR_MIN_WIDTH, max_w);
        if (end - self.width).abs() <= 0.5 {
            return;
        }
        // 立即结算（拖拽）时让起点=终点：渲染层的「起止无差」守卫会跳过动画，
        // 既保证 1:1 跟手，也避免之后拖回旧目标宽度时重播一次过期的动画。
        self.panel_anim_start = if ms > 0 { start } else { end };
        self.panel_anim_end = end;
        self.panel_anim_ms = ms;
        if ms > 0 {
            self.panel_anim_epoch = self.panel_anim_epoch.wrapping_add(1);
        }
        self.width = end;
        cx.notify();
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
                diff_repo: f.diff_repo.clone(),
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
            // diff tab 的 InputState 只是占位的空 disabled state，内容由 DiffView
            // 后台从 git 重载：不缓存，避免恢复时走纯文本路径导致空白。
            if !file.is_loading && file.image.is_none() && file.diff.is_none() {
                let state = file.editor_state.clone();
                cached_states.insert(file.id.clone(), state);
            }
        }
        cached_states
    }

    /// 取出 diff tab 的 DiffView 实体，用于标签页组切换时整实体保留。
    fn take_cached_diff_views(
        &mut self,
    ) -> HashMap<SharedString, Entity<crate::editor::diff_view::DiffView>> {
        let mut cached = HashMap::new();
        for file in &mut self.open_files {
            if let Some(diff) = file.diff.clone() {
                cached.insert(file.id.clone(), diff);
            }
        }
        cached
    }

    /// 保存当前编辑器状态到文件树状态
    pub fn save_to_file_tree_state(&mut self, cx: &mut Context<Self>) -> FileTreeEditorState {
        // 切标签页组会缓存影子 InputState：先把防抖中的 live 文本落盘，
        // 否则切回来会看到滞后的内容。
        self.flush_all_live_shadows(cx);
        let open_files_info = self.get_open_files_info();
        let cached_editor_states = self.take_cached_editor_states();
        let cached_diff_views = self.take_cached_diff_views();
        let active_file_id = self.active_file_id.clone();

        // 清空编辑器
        self.clear(cx);

        FileTreeEditorState {
            open_files_info,
            cached_editor_states,
            cached_diff_views,
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
            &state.cached_diff_views,
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
        cached_diffs: &HashMap<SharedString, Entity<crate::editor::diff_view::DiffView>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        // 恢复期间不播放展开动画（编辑器宽度保持原值，标题栏 padding 同步保持瞬时）
        self.suppress_open_animation = true;
        // 内容整体替换：面板宽度动画槽钉死为当前宽度（不滑动），并请求
        // AppWindow 在本帧渲染时静态对齐 tab 栏 pad（见字段注释）。
        self.snap_panel_width();
        self.tab_bar_pad_snap_request = true;
        for file_info in &files_info {
            // diff tab：优先复用缓存的 DiffView 实体（保留未保存编辑/滚动/折叠），
            // 无缓存时用保存的仓库后台重载；走纯文本恢复会得到空白页。
            if let Some(cached_diff) = cached_diffs.get(&file_info.id) {
                let diff_repo = file_info.diff_repo.clone();
                self.push_existing_diff_tab(
                    file_info.id.to_string(),
                    file_info.path.clone(),
                    file_info.label.clone(),
                    cached_diff.clone(),
                    diff_repo,
                    window,
                    cx,
                );
                continue;
            }
            if let Some(ref diff_repo) = file_info.diff_repo {
                self.open_diff_file_loading(file_info.path.clone(), diff_repo.clone(), false, window, cx);
                continue;
            }
            // 兼容旧缓存：id 是 `diff-` 前缀但没有仓库快照时跳过建空白 tab，
            // 避免切组回来留下一个空白的只读编辑器。
            if file_info.id.as_ref().starts_with("diff-") {
                log::warn!("skip restoring diff tab without repo snapshot: {}", file_info.id.as_ref());
                continue;
            }
            let is_image = crate::editor::image_viewer::is_image_file(Path::new(&file_info.path));

            // 图片文件不能从文本 InputState 缓存恢复，必须重新加载二进制
            if is_image {
                if let Some(ref ssh_config) = file_info.ssh_config {
                    self.open_remote_file_inner(
                        file_info.path.clone(),
                        ssh_config.clone(),
                        window,
                        cx,
                        false,
                        false,
                        false,
                        true,
                    );
                } else {
                    self.open_local_file_inner(
                        std::path::PathBuf::from(&file_info.path),
                        window,
                        cx,
                        false,
                        false,
                        false,
                        true,
                    );
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
                    self.open_remote_file_inner(
                        file_info.path.clone(),
                        ssh_config.clone(),
                        window,
                        cx,
                        false,
                        false,
                        false,
                        true,
                    );
                } else {
                    self.open_local_file_inner(
                        std::path::PathBuf::from(&file_info.path),
                        window,
                        cx,
                        false,
                        false,
                        false,
                        true,
                    );
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
        let is_live = file_info.id.as_ref().ends_with(Self::MARKDOWN_LIVE_SUFFIX);
        let is_markdown = path_str.ends_with(".md")
            && !file_info.id.as_ref().ends_with(Self::MARKDOWN_EDIT_SUFFIX)
            && !is_live;
        let markdown = if is_markdown {
            Some(cx.new(|cx| crate::editor::markdown::Markdown::new(content.clone(), cx)))
        } else {
            None
        };
        let live = if is_live {
            let content_clone = content.clone();
            // 图片粘贴/拖拽落盘需要文档位置（Typora `<stem>.assets/` 布局）。
            let live_doc_path = path_str.clone();
            let live_read_only = file_info.read_only;
            Some(cx.new(|cx| {
                crate::editor::markdown_live::LiveMarkdownView::with_text(&content_clone, cx)
                    .read_only(live_read_only)
                    .doc_path(std::path::Path::new(&live_doc_path))
            }))
        } else {
            None
        };

        // Re-render the Editor when the restored editor's InputState changes
        // (hover popup / completion menu / search matches).
        let restored_tab_id = file_info.id.clone();
        if let Some(live) = live.clone() {
            // Live tab：脏标记由 LiveMarkdownView 的 Change 事件驱动。
            let live_tab_id = restored_tab_id.clone();
            self.editor_state_subscriptions.insert(
                restored_tab_id.clone(),
                cx.subscribe(
                    &live,
                    move |this, _live_entity, event: &crate::editor::markdown_live::LiveViewEvent, cx| {
                        if !matches!(
                            event,
                            crate::editor::markdown_live::LiveViewEvent::Change { .. }
                        ) {
                            return;
                        }
                        let crate::editor::markdown_live::LiveViewEvent::Change { version } = event
                        else {
                            return;
                        };
                        this.on_live_change(&live_tab_id.clone().into(), *version, cx);
                    },
                ),
            );
        } else {
            self.editor_state_subscriptions.insert(
                restored_tab_id.clone(),
                cx.observe(&editor_state, move |this, state_entity, cx| {
                // 与 open_file_with_content 相同的脏检查逻辑，恢复后同样需要维护未保存标记。
                let text_version = state_entity.read(cx).text_version;
                let idx = this.open_files.iter().position(|f| f.id == restored_tab_id);
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
            }),
        );
        }

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
            live,
            image: None,
            diff: None,
            diff_repo: None,
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
            // 事件来源必须与文件后端一致：多台主机可能存在相同路径字符串的文件
            // （如两台机器的 /root/notes.txt），只有来自同一后端的事件才触发重载。
            if !event.is_from_backend(file.ssh_config.as_ref()) {
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
                                if let Some(file) = this.open_files.iter_mut().find(|f| f.id == id)
                                {
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
        // 影子 `InputState` 是 250ms 防抖同步的，而下面的「有没有本地修改」判据
        // 正是读它。不先落盘的话，刚打完字（防抖未到期）就遇到外部写入时，
        // `current` 还是旧文本 → `no_local_edits` 误判为真 → 分支 1 直接用磁盘
        // 内容覆盖 live 视图，而 `EditorModel::set_text` 会清空撤销栈，用户刚输入
        // 的内容既无提示也无法撤销。这里必须先对齐，判据才是当下的真实状态。
        self.flush_all_live_shadows(cx);

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

            let current = self.open_files[file_ix]
                .editor_state
                .read(cx)
                .value()
                .to_string();
            let no_local_edits = current == self.open_files[file_ix].original_content;
            let disk_matches_editor = content == current;

            if no_local_edits || disk_matches_editor {
                // 分支 1+2：直接采用磁盘内容，同步基线/编码/标签。
                let changed = self.open_files[file_ix].original_content != content;
                self.open_files[file_ix].original_content = content.clone();
                self.open_files[file_ix].encoding = encoding;
                self.open_files[file_ix].dirty = false;
                if changed {
                    self.open_files[file_ix]
                        .editor_state
                        .update(cx, |state, cx| {
                            state.set_text(content.clone(), cx);
                        });
                    if let Some(ref markdown) = self.open_files[file_ix].markdown {
                        markdown.update(cx, |renderer, cx| {
                            renderer.set_text(content.clone(), cx);
                        });
                    }
                    if let Some(ref live) = self.open_files[file_ix].live {
                        live.update(cx, |view, cx| {
                            view.set_text(&content, cx);
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
                        .insert_file_content(
                            ssh_config,
                            std::path::Path::new(path),
                            content.clone(),
                            encoding,
                            true,
                        );
                }
                self.open_files[file_ix].pending_reload = false;
                continue;
            }

            // 分支 3：冲突。
            log::info!("File changed externally with unsaved local edits: {}", path);
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
                        if let Some(ref live) = this.open_files[file_ix].live {
                            live.update(cx, |view, cx| {
                                view.set_text(&prompt_content, cx);
                            });
                        }
                        if let Some(ref ssh_config) = this.open_files[file_ix].ssh_config {
                            crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                .lock()
                                .insert_file_content(
                                    ssh_config,
                                    std::path::Path::new(&prompt_path),
                                    prompt_content.clone(),
                                    encoding,
                                    true,
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

    fn unwatch_remote_file(
        path: &Path,
        ssh_config: &crate::ssh::SshConnectionConfig,
        cx: &mut Context<Self>,
    ) {
        // unwatch 需要与 watch 同一台服务器的 fs 才能定位到引用计数
        let fs: Arc<dyn crate::file_manager::FileSystem> =
            Arc::new(crate::file_manager::filesystem::RemoteFileSystem::new(
                ssh_config.clone(),
            ));
        cx.unwatch_remote_path(path, &fs);
    }

    fn unwatch_all_files(&mut self, cx: &mut Context<Self>) {
        // 按路径去重，并把 ssh_config 一并带走：远程文件需要自己的服务器 key
        // 才能定位到 watch 引用计数（同一编辑器可打开多台主机的文件）。
        let mut paths: Vec<(PathBuf, Option<crate::ssh::SshConnectionConfig>)> = Vec::new();
        for f in &self.open_files {
            let path = PathBuf::from(&f.path);
            if !paths.iter().any(|(p, _)| p == &path) {
                paths.push((path, f.ssh_config.clone()));
            }
        }
        for (path, ssh_config) in paths {
            match ssh_config {
                Some(config) => Self::unwatch_remote_file(&path, &config, cx),
                None => Self::unwatch_file(&path, cx),
            }
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.unwatch_all_files(cx);
        self.open_files.clear();
        self.active_file_id = None;
        self.loading_gens.clear();
        self.editor_state_subscriptions.clear();
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
            // 上限与窗口跟随一致：拖拽也不能把主区挤出窗口（固定 px 为硬约束）。
            let max_w = self.window_follow_limit.max(EDITOR_MIN_WIDTH);
            self.width = (self.width + delta).clamp(EDITOR_MIN_WIDTH, max_w);
            self.resize_drag_x = Some(pos);
        }
    }

    pub fn end_resize(&mut self) {
        self.resize_drag_x = None;
    }

    /// 窗口宽度跟随：窗口每变化 1px，编辑器跟 `EDITOR_WINDOW_FOLLOW_RATIO`。
    /// 调用方保证非拖拽态（拖拽中分割线必须与鼠标 1:1，窗口增量只更新基线，
    /// 松手不结算，避免弹跳）。首帧（基线为 0）只记录不跟随。
    /// 上限 `window_follow_limit`（窗口宽 − 侧边栏占位 − 主区最小宽）保证
    /// 跟随不会把主区挤出窗口。
    fn apply_window_follow(&mut self, win_w: f32) {
        if self.last_window_width > 0.0 {
            let dw = win_w - self.last_window_width;
            if dw.abs() > 0.5 {
                let max_w = self.window_follow_limit.max(EDITOR_MIN_WIDTH);
                self.width = (self.width + dw * EDITOR_WINDOW_FOLLOW_RATIO)
                    .clamp(EDITOR_MIN_WIDTH, max_w);
            }
        }
        self.last_window_width = win_w;
    }

    /// 由 AppWindow 每帧下发窗口跟随的宽度上限（窗口宽 − 侧边栏占位 −
    /// 主区最小宽）。仅用于钳制窗口跟随，不影响拖拽/最小宽度语义。
    pub fn set_window_follow_limit(&mut self, limit: f32) {
        self.window_follow_limit = limit;
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
                panel
                    .read(cx)
                    .input
                    .read(cx)
                    .focus_handle
                    .clone()
                    .focus(window, cx);
            }
            cx.notify();
            return;
        }
        let Some(editor) = active_editor else {
            return;
        };
        let handle = self.window_handle.unwrap_or_else(|| window.window_handle());
        let panel = goto_line::GotoLinePanel::new(editor, handle, window, cx);
        panel.update(cx, |p, cx| p.focus(window, cx));
        self.goto_line_panel = Some(panel);
        cx.notify();
    }

    /// 当前激活 tab 对应的文本编辑器（diff/图片/markdown 预览/实时返回 None）。
    fn active_text_editor(&self, _cx: &App) -> Option<Entity<InputState>> {
        let active_id = self.active_file_id.as_ref()?;
        let file = self.open_files.iter().find(|f| f.id == *active_id)?;
        if file.diff.is_some()
            || file.image.is_some()
            || file.markdown.is_some()
            || file.live.is_some()
        {
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
                Language::from_id(&id).display_name().to_string()
            })
            .unwrap_or_else(|| "纯文本".to_string());
        // 缩进单元：硬 Tab 显示 "Tab"，空格缩进显示单元宽度。
        let tab_size = state.tab_size;
        let indent_label = if tab_size.hard_tabs {
            "缩进: Tab".to_string()
        } else {
            format!("缩进: {} 空格", tab_size.tab_size)
        };

        div()
            .id("editor-status-bar")
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            // 两端各收进 12px：父容器为 flex_col（default stretch），
            // 依赖 stretch 减去外边距后铺满，不能再用 w_full（会叠加溢出）
            .mx(px(12.0))
            .mt(px(2.0))
            .pt(px(6.0))
            // 与 Markdown 实时渲染编辑器一致：仅用一条分割线与内容区隔开
            .border_t_1()
            .border_color(theme.border.opacity(0.4))
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    // vim 模式徽章：只在 vim 启用时出现。放在最左，与 vim 的
                    // 状态行位置一致，扫一眼就知道当前是 Normal 还是 Insert。
                    .when_some(state.vim.as_ref(), |this, vim| {
                        let mode = vim.mode;
                        this.child(
                            div()
                                .px(px(6.0))
                                .py(px(1.0))
                                .rounded(px(4.0))
                                .bg(theme.accent.opacity(0.15))
                                .text_color(theme.accent)
                                .child(mode.indicator()),
                        )
                    })
                    .child(format!(
                        "行 {}, 列 {}",
                        position.line + 1,
                        position.character + 1
                    ))
                    .when(selected_len > 0, |this| {
                        this.child(
                            div()
                                .text_color(theme.text_muted.opacity(0.8))
                                .child(format!("已选 {selected_len} 字")),
                        )
                    })
                    .child(format!("共 {total} 行")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .child(indent_label),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .child(encoding_label),
                    )
                    // 语言：点击弹出语言菜单
                    .child(
                        div()
                            .id("language-chip")
                            .cursor_pointer()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .hover(|this| this.bg(theme.tab_hover_background))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        this.open_language_menu(event.position, window, cx);
                                    },
                                ),
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

    /// Terminal `Cmd+Click`: open local file and jump to `line:col`.
    pub fn open_file_at_line_col_no_notify(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_local_file_at(path, window, cx, false, line, col);
    }

    /// Terminal `Cmd+Click`: open remote file and jump to `line:col`.
    pub fn open_remote_file_at_line_col_no_notify(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_at(path, ssh_config, window, cx, false, line, col);
    }

    /// Apply a `line:col` jump to an already-open tab. No-op when `line` is
    /// `None` or the tab is still loading (caller should use `pending_jumps`).
    pub fn jump_to_line_col(
        &mut self,
        tab_id: &str,
        line: u32,
        col: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        use crate::components::input::RopeExt;
        let Some(file) = self.open_files.iter().find(|f| f.id.as_ref() == tab_id) else {
            return;
        };
        // 图片/二进制预览没有文本行概念：只切换、不跳转。
        if crate::editor::image_viewer::is_image_file(Path::new(&file.path)) {
            self.switch_to_file(tab_id, cx);
            return;
        }
        if file.is_loading {
            self.pending_jumps
                .insert(tab_id.to_string(), (line, col));
            self.switch_to_file(tab_id, cx);
            return;
        }
        let editor_state = file.editor_state.clone();
        editor_state.update(cx, |state, cx| {
            let total = state.text().lines_len().max(1);
            let line = (line as usize).clamp(1, total);
            let col = col.unwrap_or(1).max(1) as usize;
            let row = line - 1;
            let line_start = state.text().line_start_offset(row);
            let line_end = state.text().line_content_end_offset(row);
            let mut offset = line_start;
            let mut remaining = col - 1;
            for ch in state.text().slice(line_start..line_end).chars() {
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                offset += ch.len_utf8();
            }
            state.selected_range = (offset..offset).into();
            state.scroll_to_center(offset, cx);
            cx.notify();
        });
        self.switch_to_file(tab_id, cx);
    }

    fn take_pending_jump(&mut self, tab_id: &str) -> Option<(u32, Option<u32>)> {
        self.pending_jumps.remove(tab_id)
    }

    /// diff 标签 id：仓库标识 + 相对路径。
    /// 本地仓库用 root，远程用 SSH 目标（同一路径在两台服务器上是不同仓库）。
    /// 两种形态都以 `/{rel_path}` 结尾，`refresh_diff_for_path` 的后缀匹配
    /// 依赖这一性质。
    fn diff_tab_id(
        rel_path: &str,
        repo: &crate::editor::git_repository::GitRepository,
    ) -> String {
        match repo.ssh_config() {
            Some(config) => {
                format!("diff-{}@{}:{}/{}", config.username, config.host, config.port, rel_path)
            }
            None => format!("diff-{}/{}", repo.root(), rel_path),
        }
    }

    /// Opens the diff tab instantly; content loads in the background.
    /// 同一文件只有一个 diff tab（VSCode 行为）：已存在则切换并按需切换
    /// 工作区/已暂存内容；`staged` 为 true 打开已暂存 diff（HEAD↔暂存区）。
    pub fn open_diff_file_loading(
        &mut self,
        path: String,
        repo: crate::editor::git_repository::GitRepository,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // tab id 必须含仓库标识：此前只有 `diff-{relpath}`，同一编辑器里
        // 打开两个仓库的同名相对路径（本地 A + 远程 B 的 src/main.rs）时
        // 会命中旧 tab，显示另一个仓库的内容。
        let tab_id = Self::diff_tab_id(&path, &repo);
        if let Some(file) = self.open_files.iter().find(|file| file.id == tab_id) {
            if let Some(view) = file.diff.clone() {
                view.update(cx, |v, cx| v.set_kind(staged, cx));
            }
            self.switch_to_file(&tab_id, cx);
            return;
        }

        // VSCode 风格：tab 只显示文件名。
        let label = match path.rsplit('/').next() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => path.clone(),
        };
        let editor_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx).multi_line(true);
            state.disabled = true;
            state
        });
        let editor_weak = cx.entity().downgrade();
        let diff_repo = repo.clone();
        let diff_view = cx.new(|cx| {
            crate::editor::diff_view::DiffView::new_loading(
                path.clone(),
                repo,
                staged,
                editor_weak,
                window,
                cx,
            )
        });
        self.push_diff_tab(tab_id, path, label, editor_state, diff_view, diff_repo, cx);
    }

    fn push_diff_tab(
        &mut self,
        tab_id: String,
        path: String,
        label: String,
        editor_state: Entity<InputState>,
        diff_view: Entity<crate::editor::diff_view::DiffView>,
        diff_repo: crate::editor::git_repository::GitRepository,
        cx: &mut Context<Self>,
    ) {
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
            live: None,
            image: None,
            diff: Some(diff_view),
            diff_repo: Some(diff_repo),
        });

        self.tab_bar.update(cx, |tab_bar, cx| {
            let tab_label = self
                .open_files
                .iter()
                .find(|f| f.id.as_ref() == tab_id)
                .map(|f| f.label.to_string())
                .unwrap_or_else(|| tab_id.clone());
            tab_bar.add_tab(
                TabItem::new(&tab_id, tab_label).with_icon(Some(Icon::Git)),
                cx,
            );
        });

        self.switch_to_file(&tab_id, cx);
        cx.notify();
    }

    /// 复用切组前缓存的 DiffView 实体：保留未保存编辑/滚动/折叠，后台内容无需重载。
    fn push_existing_diff_tab(
        &mut self,
        tab_id: String,
        path: String,
        label: String,
        diff_view: Entity<crate::editor::diff_view::DiffView>,
        diff_repo: Option<crate::editor::git_repository::GitRepository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_files.iter().any(|file| file.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }
        if self.open_files.is_empty() {
            self.begin_open_animation();
        }
        let editor_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx).multi_line(true);
            state.disabled = true;
            state
        });
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
            live: None,
            image: None,
            diff: Some(diff_view),
            diff_repo,
        });
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(
                TabItem::new(&tab_id, format!("Diff: {}", path)).with_icon(Some(Icon::Git)),
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
        self.open_local_file_inner(path, window, cx, true, true, false, false);
    }

    /// 在 Markdown 预览中双击：以同屏实时模式在新 tab 中打开同一文件。
    pub fn open_file_in_live_mode(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_local_file_inner(path, window, cx, true, false, true, false);
    }

    fn open_local_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
    ) {
        self.open_local_file_inner(path, window, cx, notify_app, false, false, false);
    }

    fn open_local_file_at(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
        line: Option<u32>,
        col: Option<u32>,
    ) {
        let path_str = path.to_string_lossy().to_string();
        let live_mode = Self::default_live_mode(&path, false, false);
        let tab_id = if live_mode {
            format!("editor-{}{}", path_str, Self::MARKDOWN_LIVE_SUFFIX)
        } else {
            format!("editor-{}", path_str)
        };
        // Already open (or loading): switch + jump (or defer via pending).
        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            if let Some(line) = line {
                self.jump_to_line_col(&tab_id, line, col, cx);
            }
            return;
        }
        if self.loading_gens.contains_key(&tab_id) {
            if let Some(line) = line {
                self.pending_jumps.insert(tab_id, (line, col));
            }
            return;
        }
        // 图片走 ImageViewer，没有行列概念：不记录 pending，避免泄漏后误跳。
        if line.is_some() && crate::editor::image_viewer::is_image_file(&path) {
            self.open_local_file_inner(path, window, cx, notify_app, false, false, false);
            return;
        }
        if let Some(line) = line {
            self.pending_jumps.insert(tab_id.clone(), (line, col));
        }
        self.open_local_file_inner(path, window, cx, notify_app, false, false, false);
    }

    /// `silent` 透传给 [`Self::spawn_file_load`]：恢复会话时为 true，
    /// 加载失败只关标签、不弹模态框（见 [`Self::spawn_file_load`] 的文档）。
    #[allow(clippy::too_many_arguments)]
    fn open_local_file_inner(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
        edit_mode: bool,
        live_mode: bool,
        silent: bool,
    ) {
        let live_mode = Self::default_live_mode(&path, edit_mode, live_mode);
        let path_str = path.to_string_lossy().to_string();
        let tab_id = if live_mode {
            format!("editor-{}{}", path_str, Self::MARKDOWN_LIVE_SUFFIX)
        } else if edit_mode {
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
            live_mode,
        );

        self.spawn_file_load(
            tab_id,
            load_gen,
            Arc::new(move || load_local_text(&path_for_task)),
            "读取文件失败",
            "无法读取本地文件",
            silent,
            window,
            cx,
        );
    }

    /// `silent` 控制加载失败时要不要弹窗。
    ///
    /// # 为什么恢复会话必须静默
    ///
    /// 恢复标签页组/文件树时会**重新加载**每个打开的文件，而文件可能在组被
    /// 挂起期间被删掉、被移走，或者远端主机连不上。那些是正常情况，不是用户
    /// 当下发起的操作——对着一个后台恢复动作弹「无法读取文件：No such file or
    /// directory (os error 2)」的模态框，用户既没做错什么，也不知道该点哪个
    /// 按钮（「重新加载」只会再失败一次）。直接关掉这个标签即可。
    ///
    /// 用户**主动**打开文件失败时仍然要弹：那时错误是操作的结果，需要可见。
    #[allow(clippy::too_many_arguments)]
    fn spawn_file_load(
        &mut self,
        tab_id: String,
        load_generation: u64,
        load_content: Arc<dyn Fn() -> Result<LoadedText, String> + Send + Sync>,
        error_title: &'static str,
        error_prefix: &'static str,
        silent: bool,
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
                                if let Some(ref live) = file.live {
                                    let live_read_only = loaded.read_only;
                                    live.update(cx, |view, cx| {
                                        view.set_text(&loaded.text, cx);
                                        view.set_read_only(live_read_only, cx);
                                    });
                                }
                                // 缓存远程文件内容到 RemoteFileTreeCache
                                if let Some(ref ssh_config) = file.ssh_config {
                                    crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                        .lock()
                                        .insert_file_content(
                                            ssh_config,
                                            std::path::Path::new(&file.path),
                                            loaded.text.clone(),
                                            loaded.encoding,
                                            true,
                                        );
                                }
                                // 后台计算 Git 行变更标记（行号区色条）
                                this.refresh_git_marks(&tab_id, cx);
                            }
                            // Terminal Cmd+Click deferred jump: content just
                            // arrived, apply line:col now (clamped to total).
                            if let Some((line, col)) = this.take_pending_jump(&tab_id) {
                                this.jump_to_line_col(&tab_id, line, col, cx);
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

                        // 静默（会话/标签页组恢复）：文件可能在挂起期间被删掉或
                        // 远端不可达。直接关掉标签，不弹模态框——见
                        // `spawn_file_load` 的文档。
                        if silent {
                            let _ = weak.update(cx, |this, cx| {
                                if this.is_load_current(&tab_id, load_generation) {
                                    this.loading_gens.remove(&tab_id);
                                    this.close_file_direct(&tab_id, cx);
                                }
                                cx.notify();
                            });
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
        self.open_remote_file_inner(path, ssh_config, window, cx, true, false, false, false);
    }

    fn open_remote_file_at(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
        line: Option<u32>,
        col: Option<u32>,
    ) {
        let live_mode =
            Self::default_live_mode(std::path::Path::new(&path), false, false);
        let tab_id = if live_mode {
            format!("editor-remote-{}{}", path, Self::MARKDOWN_LIVE_SUFFIX)
        } else {
            format!("editor-remote-{}", path)
        };
        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            if let Some(line) = line {
                self.jump_to_line_col(&tab_id, line, col, cx);
            }
            return;
        }
        if self.loading_gens.contains_key(&tab_id) {
            if let Some(line) = line {
                self.pending_jumps.insert(tab_id, (line, col));
            }
            return;
        }
        // 图片走 ImageViewer，没有行列概念：不记录 pending，避免泄漏后误跳。
        if line.is_some()
            && crate::editor::image_viewer::is_image_file(std::path::Path::new(&path))
        {
            self.open_remote_file_inner(path, ssh_config, window, cx, notify_app, false, false, false);
            return;
        }
        if let Some(line) = line {
            self.pending_jumps.insert(tab_id.clone(), (line, col));
        }
        self.open_remote_file_inner(path, ssh_config, window, cx, notify_app, false, false, false);
        // Cached remote files open synchronously (`is_loading=false`):
        // apply the pending jump immediately instead of waiting for a load.
        if let Some((line, col)) = self.take_pending_jump(&tab_id) {
            let still_loading = self
                .open_files
                .iter()
                .find(|f| f.id.as_ref() == tab_id.as_str())
                .is_some_and(|f| f.is_loading);
            if still_loading {
                self.pending_jumps.insert(tab_id.clone(), (line, col));
            } else {
                self.jump_to_line_col(&tab_id, line, col, cx);
            }
        }
    }

    /// 在 Markdown 预览中双击：以纯编辑模式（代码编辑器）在新 tab 中打开同一远程文件。
    pub fn open_remote_file_in_edit_mode(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, true, true, false, false);
    }

    /// 远程 Markdown 预览中双击：以同屏实时模式在新 tab 中打开同一文件。
    pub fn open_remote_file_in_live_mode(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, true, false, true, false);
    }

    /// `silent` 透传给 [`Self::spawn_file_load`]：恢复会话时为 true，
    /// 加载失败只关标签、不弹模态框（见 [`Self::spawn_file_load`] 的文档）。
    #[allow(clippy::too_many_arguments)]
    fn open_remote_file_inner(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
        edit_mode: bool,
        live_mode: bool,
        silent: bool,
    ) {
        let live_mode =
            Self::default_live_mode(std::path::Path::new(&path), edit_mode, live_mode);
        let tab_id = if live_mode {
            format!("editor-remote-{}{}", path, Self::MARKDOWN_LIVE_SUFFIX)
        } else if edit_mode {
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

        // 检查是否有缓存的文件内容（P1: 带 TTL 的 stale-while-revalidate）
        let (cached_content, is_fresh) = {
            let cache = crate::file_manager::file_tree_cache::RemoteFileTreeCache::global().lock();
            let c = cache.get_file_content(&ssh_config, std::path::Path::new(&path));
            let f = cache.is_file_fresh(&ssh_config, std::path::Path::new(&path));
            (c, f)
        };

        if let Some((content, encoding)) = cached_content {
            // 有缓存，直接使用缓存内容秒开
            let tab_id_for_bg = tab_id.clone();
            self.open_file_with_content(
                tab_id.clone(),
                path.clone(),
                content,
                PathBuf::from(&path),
                Some(ssh_config.clone()),
                encoding,
                false,
                window,
                cx,
                notify_app,
                false, // is_loading = false，因为有缓存内容
                edit_mode,
                live_mode,
            );
            if !is_fresh {
                let ssh_config = ssh_config.clone();
                let path = path.clone();
                let tab_id = tab_id_for_bg;
                let weak = cx.entity().downgrade();
                cx.spawn(async move |_, cx| {
                    let fetched = cx
                        .background_executor()
                        .spawn({
                            let ssh_config = ssh_config.clone();
                            let path = path.clone();
                            async move {
                                let bytes =
                                    crate::ssh::sftp_fs::read_file_bytes(&ssh_config, &path)
                                        .ok()
                                        .flatten()?;
                                if crate::ssh::sftp_fs::is_binary_bytes(&bytes) {
                                    return None;
                                }
                                let decoded = crate::editor::encoding::decode_bytes(&bytes);
                                Some((decoded.text, decoded.encoding))
                            }
                        })
                        .await;
                    let Some((fresh_text, fresh_enc)) = fetched else {
                        return;
                    };
                    let _ = weak.update(cx, |this, cx| {
                        // 与 `apply_external_reload` 同理：`file.dirty` 与影子
                        // `InputState` 都是防抖滞后的，不先对齐就会把「刚打过字」
                        // 误判成「无本地修改」，随后用磁盘内容覆盖 live 视图。
                        this.flush_all_live_shadows(cx);
                        let Some(file) =
                            this.open_files.iter_mut().find(|f| f.id.as_str() == tab_id)
                        else {
                            return;
                        };
                        let should_update = !file.is_loading
                            && !file.dirty
                            && file.original_content != fresh_text
                            && file.editor_state.read(cx).value() == file.original_content;
                        if should_update {
                            file.original_content = fresh_text.clone();
                            file.encoding = fresh_enc;
                            file.editor_state
                                .update(cx, |st, cx| st.set_text(fresh_text.clone(), cx));
                            if let Some(md) = &file.markdown {
                                md.update(cx, |r, cx| r.set_text(fresh_text.clone(), cx));
                            }
                            if let Some(live) = &file.live {
                                live.update(cx, |v, cx| v.set_text(&fresh_text, cx));
                            }
                            cx.notify();
                        }
                        crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                            .lock()
                            .insert_file_content(
                                &ssh_config,
                                Path::new(&path),
                                fresh_text,
                                fresh_enc,
                                true,
                            );
                    });
                })
                .detach();
            }
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
            live_mode,
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
            silent,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
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
            false,
        );

        let load_image: ImageLoader = if let Some(config) = ssh_config {
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
        load_image: ImageLoader,
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
                            // 图片没有行列概念：丢弃可能残留的 Cmd+Click pending 跳转。
                            this.pending_jumps.remove(&tab_id);
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
        live_mode: bool,
    ) {
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&path_str)
            .to_string();
        // 编辑模式 tab 与预览 tab 区分标识，避免两个同名 tab 混淆
        let label = if live_mode {
            format!("{} (实时)", label)
        } else if edit_mode {
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
            // 普通文件标签：行首 git 色条可点击展开 hunk
            // （diff 视图自己的编辑器已是展开态，保持默认 false）。
            state.set_git_expandable(true);
            // vim 状态随 tab 创建：每个 tab 一个 InputState，模式因此天然隔离。
            state.vim = crate::vim::initial_state(cx);
            state
        });

        let is_live_file = live_mode
            && !is_image
            && path.extension().and_then(|e| e.to_str()) == Some("md");
        let live = if is_live_file {
            let content_clone = content.clone();
            let live_doc_path = path.clone();
            Some(cx.new(|cx| {
                crate::editor::markdown_live::LiveMarkdownView::with_text(&content_clone, cx)
                    .read_only(read_only)
                    .doc_path(&live_doc_path)
            }))
        } else {
            None
        };

        // Re-render the Editor when this editor's InputState changes
        // (search matches / cursor): the popups are rendered by EditorInput,
        // which only re-renders when the Editor does.
        let sync_path = path_str.clone();
        let sync_tab_id = tab_id.clone();
        if let Some(live) = live.clone() {
            // Live tab：脏标记 + 影子 InputState 同步由 Change 事件驱动。
            let live_tab_id = sync_tab_id.clone();
            self.editor_state_subscriptions.insert(
                sync_tab_id.clone().into(),
                cx.subscribe(
                    &live,
                    move |this, _live_entity, event: &crate::editor::markdown_live::LiveViewEvent, cx| {
                        if !matches!(
                            event,
                            crate::editor::markdown_live::LiveViewEvent::Change { .. }
                        ) {
                            return;
                        }
                        let crate::editor::markdown_live::LiveViewEvent::Change { version } = event
                        else {
                            return;
                        };
                        this.on_live_change(&live_tab_id.clone().into(), *version, cx);
                    },
                ),
            );
        } else {
        self.editor_state_subscriptions.insert(
            sync_tab_id.clone().into(),
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
                        .filter(|f| f.id.as_ref() != sync_tab_id.as_str() && f.path == sync_path)
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
        }

        let is_markdown = !is_image
            && !edit_mode
            && !live_mode
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
            live,
            image,
            diff: None,
            diff_repo: None,
        });

        let is_local = self
            .open_files
            .last()
            .map(|f| f.ssh_config.is_none())
            .unwrap_or(true);
        if is_local {
            Self::watch_file(&path, cx);
        } else {
            // 关键防御：用 let-else 替代双重 unwrap，避免并发变更导致 panic。
            // 若状态不一致则降级为不监听，文件仍可正常打开编辑。
            let Some(last) = self.open_files.last() else {
                return;
            };
            let Some(ssh_config) = last.ssh_config.clone() else {
                Self::watch_file(&path, cx);
                return;
            };
            let fs = Arc::new(RemoteFileSystem::new(ssh_config));
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
    /// 后台计算 Git 行变更标记（HEAD → 工作区）并写入编辑器状态。
    ///
    /// 在文件加载完成与保存成功后调用：标记反映磁盘状态，因此无需
    /// 随击键重算（未保存的缓冲区修改不属于工作区 diff）。
    fn refresh_git_marks(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(file) = self.open_files.iter().find(|f| f.id.as_ref() == tab_id) else {
            return;
        };
        let path = file.path.clone();
        let ssh_config = file.ssh_config.clone();
        let weak = cx.entity().downgrade();
        let tab_id = tab_id.to_string();

        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let is_local = ssh_config.is_none();
                    // find_local 以该路径为 git 子进程的 current_dir：必须传目录，
                    // 传文件路径会让 spawn 直接 ENOENT（编辑器色条因此从未加载）。
                    let anchor = std::path::Path::new(&path)
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| std::path::PathBuf::from(&path));
                    let repo = match GitRepository::find(&anchor, is_local, ssh_config) {
                        Ok(Some(repo)) => repo,
                        Ok(None) => {
                            log::debug!("git marks: not a git repo: {path:?}");
                            return None;
                        }
                        Err(e) => {
                            log::debug!("git marks: find failed: {e}");
                            return None;
                        }
                    };
                    // 仓库相对路径：git show HEAD:<rel> 在仓库根目录下执行
                    let rel = match path.strip_prefix(repo.root()) {
                        Some(rel) => rel,
                        None => {
                            log::debug!(
                                "git marks: strip_prefix failed root={:?} path={path:?}",
                                repo.root()
                            );
                            return None;
                        }
                    };
                    let rel = rel.trim_start_matches('/');
                    if rel.is_empty() {
                        log::debug!("git marks: empty rel");
                        return None;
                    }
                    let (old, new) = match repo.diff_file_contents(rel) {
                        Ok(v) => v,
                        Err(e) => {
                            log::debug!("git marks: diff_file_contents err {e} rel={rel}");
                            return None;
                        }
                    };
                    Some(compute_git_diff_summary(&old, &new))
                })
                .await;
            let Some(summary) = result else {
                return;
            };
            let _ = weak.update(cx, |this, cx| {
                if let Some(file) = this.open_files.iter().find(|f| f.id.as_ref() == tab_id) {
                    file.editor_state.update(cx, |state, cx| {
                        state.set_git_diff(summary, cx);
                    });
                }
            });
        })
        .detach();
    }

    /// 按文件路径刷新 Git 行标记（可能对应多个标签页）。
    ///
    /// 供外部通知使用：Git 面板 stage / unstage 会改变暂存区，从而使
    /// HEAD → 工作区的 diff 结果变化，已打开文件的行标记需要重算。
    ///
    /// 注意路径形态：Git 面板发的是仓库相对路径（如 `src/main.rs`），
    /// 而普通编辑标签存的是绝对路径——后缀匹配才能命中；diff 标签存的
    /// 恰好是相对路径，用精确匹配。
    pub fn refresh_git_marks_for_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let suffix = format!("/{path}");
        let ids: Vec<gpui::SharedString> = self
            .open_files
            .iter()
            .filter(|f| f.path == path || f.path.ends_with(suffix.as_str()))
            .map(|f| f.id.clone())
            .collect();
        for id in ids {
            self.refresh_git_marks(&id, cx);
        }
    }

    /// 刷新所有普通编辑标签的 Git 行标记。
    ///
    /// 提交 / amend 后 HEAD 前移，所有已打开文件相对 HEAD 的差异整体过期
    /// （刚提交完的文件不应继续显示满屏“已修改”色条）。StageFile 等
    /// 事件都带路径刷新过标记，唯独 CommitDone 此前没有任何刷新。
    pub fn refresh_all_git_marks(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<gpui::SharedString> = self
            .open_files
            .iter()
            .filter(|f| f.diff.is_none())
            .map(|f| f.id.clone())
            .collect();
        for id in ids {
            self.refresh_git_marks(&id, cx);
        }
    }

    /// 刷新指定仓库相对路径的 diff 页（放弃更改/提交后 HEAD 或工作区已变）。
    ///
    /// diff tab id 现含仓库标识（见 [`Self::diff_tab_id`]，多仓库同名路径
    /// 不再共用 tab），且以 `/{rel_path}` 结尾——事件只带相对路径，按后缀
    /// 匹配即可命中任意仓库的对应 tab（多命中时多刷一遍，无害）。
    pub fn refresh_diff_for_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let suffix = format!("/{path}");
        for file in self.open_files.iter() {
            if let Some(view) = file.diff.clone() {
                if file.path == path || file.id.as_ref().ends_with(suffix.as_str()) {
                    view.update(cx, |v, cx| v.refresh(cx));
                }
            }
        }
    }

    /// 刷新所有已打开的 diff 页（提交后 HEAD 整体前移）。
    pub fn refresh_all_diff_views(&mut self, cx: &mut Context<Self>) {
        let views: Vec<Entity<crate::editor::diff_view::DiffView>> = self
            .open_files
            .iter()
            .filter_map(|f| f.diff.clone())
            .collect();
        for view in views {
            view.update(cx, |v, cx| v.refresh(cx));
        }
    }

    fn sync_tab_modified(&self, tab_id: &str, dirty: bool, cx: &mut Context<Self>) {
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.modify_tab(tab_id, |tab| tab.is_modified = dirty, cx);
        });
    }

    /// live tab 的 `Change` 事件处理（两个订阅点共用）。
    ///
    /// 每击键只做 O(1) 的事：更新标签页的未保存标记（`is_dirty_fast`，不
    /// join 全文），并把「影子 `InputState` 需要同步」登记进
    /// [`Self::pending_live_sync`] + 起一个防抖任务。真正的全文同步在
    /// [`Self::flush_live_shadow`] 里做 —— 影子 `InputState` 只服务于
    /// 保存/切标签页组/关闭确认，live tab 渲染时完全不读它，因此不必跟手。
    fn on_live_change(
        &mut self,
        live_tab_id: &SharedString,
        version: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self
            .open_files
            .iter()
            .position(|f| f.id == *live_tab_id)
        else {
            return;
        };
        // O(1)：版本号对比，绝不 join 全文。
        let dirty = self.open_files[idx]
            .live
            .as_ref()
            .is_some_and(|live| live.read(cx).is_dirty_fast());
        if self.open_files[idx].dirty != dirty {
            self.open_files[idx].dirty = dirty;
            self.sync_tab_modified(live_tab_id, dirty, cx);
        }
        self.schedule_live_shadow_sync(live_tab_id, version, cx);
        cx.notify();
    }

    /// 登记待同步的影子 `InputState` 版本，并起一个防抖任务。
    ///
    /// 连续击键只会在防抖窗口后同步一次最终文本（中间版本直接跳过）：
    /// 这正是省掉「每次击键一次 `Rope` 重建 + 整篇软换行」的关键。
    fn schedule_live_shadow_sync(
        &mut self,
        tab_id: &SharedString,
        version: u64,
        cx: &mut Context<Self>,
    ) {
        self.pending_live_sync.insert(tab_id.clone(), version);
        let tab_id = tab_id.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(Self::LIVE_SHADOW_SYNC_DEBOUNCE_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.flush_live_shadow(&tab_id, cx);
            });
        })
        .detach();
    }

    /// 把 live 文档的最终文本同步进影子 `InputState`（防抖落地点）。
    ///
    /// 已经是登记时的版本就跳过：说明期间没有新编辑，同步已完成或不需要。
    fn flush_live_shadow(&mut self, tab_id: &SharedString, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_live_sync.remove(tab_id) else {
            return;
        };
        let Some(idx) = self.open_files.iter().position(|f| f.id == *tab_id) else {
            return;
        };
        let Some(live) = self.open_files[idx].live.clone() else {
            return;
        };
        if live.read(cx).text_version() != pending {
            // 期间又有编辑：让那次编辑的防抖任务负责（它已重新登记）。
            return;
        }
        let text = live.read(cx).text();
        self.open_files[idx].editor_state.update(cx, |st, cx| {
            st.set_text(text, cx);
        });
        // 影子状态已是最新 → 脏标记可以按精确口径重算。
        // `is_dirty_fast` 只看版本号，打字后又撤销回原样会误报「已修改」；
        // 这里文本已经拿到，顺手用精确比较把标记收敛回来。
        let dirty = live.read(cx).is_dirty();
        if self.open_files[idx].dirty != dirty {
            self.open_files[idx].dirty = dirty;
            self.sync_tab_modified(tab_id, dirty, cx);
        }
    }

    /// 立即把所有待同步的 live 影子 `InputState` 落盘到最新文本。
    ///
    /// 在「保存 / 切标签页组 / 关闭确认」这些真的会读影子 `InputState` 的
    /// 路径前调用，保证防抖不改变可观测行为。
    ///
    /// 只处理 live tab（数量等于打开的 live 标签数，通常 0~2），且文本相同
    /// 时跳过写入 —— 这些路径都在用户操作上，不在帧路径上。
    pub fn flush_all_live_shadows(&mut self, cx: &mut Context<Self>) {
        // 先兑现登记过的（防抖任务可能还没跑）。
        let ids: Vec<SharedString> = self.pending_live_sync.keys().cloned().collect();
        for id in ids {
            self.flush_live_shadow(&id, cx);
        }
        // 再兜底对齐：登记表为空但 live 文本与影子不一致时（例如防抖任务
        // 在切组时被丢弃）也要写回，保证保存/关闭读到的是最新内容。
        for file in &mut self.open_files {
            let Some(live) = file.live.clone() else {
                continue;
            };
            let text = live.read(cx).text();
            if file.editor_state.read(cx).value().as_ref() == text {
                continue;
            }
            file.editor_state.update(cx, |st, cx| {
                st.set_text(text, cx);
            });
        }
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
    fn save_file_by_id(&mut self, tab_id: &str, cx: &mut Context<Self>, close_after_save: bool) {
        // 影子 InputState 可能是防抖滞后的：保存前先落盘最新文本。
        self.flush_all_live_shadows(cx);
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

        let text = self.open_files[idx]
            .editor_state
            .read(cx)
            .value()
            .to_string();
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
                    save_text_to_file(&path, ssh_config.as_ref(), &original, &text, file_encoding)
                })
                .await;
            let _ = weak
                .update(cx, |this, cx| {
                    let Some(file) = this.open_files.iter_mut().find(|f| f.id.as_ref() == tab_id)
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
                            if let Some(live) = &file.live {
                                live.update(cx, |view, _cx| view.mark_saved());
                            }
                            if let Some(config) = &file.ssh_config {
                                crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                                    .lock()
                                    .insert_file_content(
                                        config,
                                        std::path::Path::new(&file.path),
                                        written,
                                        file_encoding,
                                        true,
                                    );
                            }
                            let msg = format!("已保存 {label}");
                            this.sync_tab_modified(&tab_id, false, cx);
                            // 保存后工作区变化：重算 Git 行变更标记
                            this.refresh_git_marks(&tab_id, cx);
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
        // 关闭确认读的是影子 InputState 的脏状态：先落盘防抖中的 live 文本，
        // 否则刚打完字立刻关闭会漏掉「有未保存更改」的提示。
        self.flush_all_live_shadows(cx);
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
                    let _ = weak
                        .update(cx, |this, cx| {
                            if let Some(file) =
                                this.open_files.iter_mut().find(|f| f.id.as_ref() == tab_id)
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
        self.pending_jumps.remove(id);
        self.editor_state_subscriptions.remove(id);
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            let path = self.open_files[index].path.clone();
            let is_remote = self.open_files[index].ssh_config.is_some();
            // 同一路径可能同时打开预览/编辑两个 tab：
            // 仅当这是最后一个引用该路径的 tab 时才取消文件监听并清理内容缓存。
            let has_other_tab = self
                .open_files
                .iter()
                .find(|f| f.path == path && f.id.as_ref() != id)
                .is_some();
            if !has_other_tab {
                // 文件关闭后同步清理 RemoteFileTreeCache 中的内容缓存，避免重新打开时看到旧内容
                if is_remote {
                    // 关键防御：用 let-else 替代 unwrap，状态不一致时跳过缓存清理，
                    // 文件关闭流程本身不受影响。
                    let Some(ssh_config) = self
                        .open_files
                        .get(index)
                        .and_then(|f| f.ssh_config.as_ref())
                    else {
                        return;
                    };
                    crate::file_manager::file_tree_cache::RemoteFileTreeCache::global()
                        .lock()
                        .remove_file(ssh_config, std::path::Path::new(&path));
                    Self::unwatch_remote_file(Path::new(&path), ssh_config, cx);
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

    /// 外部重命名后同步已打开的标签（文件树发起的重命名）。
    ///
    /// `from` 可能是目录：其下所有已打开的后代都要跟着改路径，否则保存时会
    /// 把内容写回旧路径、把刚删掉的旧文件又「复活」出来。
    pub fn rename_open_path(
        &mut self,
        from: &Path,
        to: &Path,
        ssh_config: Option<&SshConnectionConfig>,
        cx: &mut Context<Self>,
    ) {
        let mut renamed = 0usize;
        for file in &mut self.open_files {
            // 后端必须匹配：两台主机上可能有同名路径的文件。
            if file.ssh_config.as_ref() != ssh_config {
                continue;
            }
            let Some(new_path) = rekey_open_path(Path::new(&file.path), from, to) else {
                continue;
            };
            file.path = new_path.to_string_lossy().to_string();
            file.label = new_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| file.path.clone());
            let id = file.id.to_string();
            let label = file.label.clone();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.modify_tab(&id, |tab| tab.label = label.clone().into(), cx);
            });
            renamed += 1;
        }
        if renamed > 0 {
            log::info!("重命名后同步了 {renamed} 个已打开的标签");
            cx.notify();
        }
    }

    /// 外部删除后关闭对应的标签（文件树发起的删除）。
    ///
    /// 直接关闭、不弹「未保存」确认：文件已经不存在了，留着只会在保存时
    /// 把它重新写回磁盘。
    pub fn close_open_path(
        &mut self,
        removed: &Path,
        ssh_config: Option<&SshConnectionConfig>,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<String> = self
            .open_files
            .iter()
            // 同上：只关同一后端的标签
            .filter(|f| {
                f.ssh_config.as_ref() == ssh_config && path_is_under(Path::new(&f.path), removed)
            })
            .map(|f| f.id.to_string())
            .collect();
        for id in ids {
            self.close_file_direct(&id, cx);
        }
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
                let tabs = self.tab_bar.clone();
                move |tab_id, menu, _window, _cx| {
                    // 记录本次右键的 tab：ContextMenu 包装器在 window 层先收到
                    // 右键并 stop_propagation，行内 Right 监听器收不到事件，
                    // 构建闭包是唯一可靠的记录点（defer 一帧后执行，无竞态）。
                    tabs.update(_cx, |bar, _| {
                        bar.set_context_menu_tab(tab_id.clone());
                    });
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
        // 窗口跟随只吃“变化量”：拖拽中仅更新基线（松手不结算、无弹跳）。
        // 空编辑器不参与布局（宽度目标为 0），此时不累积窗口增量、只追踪基线，
        // 避免关闭期间改窗口大小导致之后打开文件时宽度突变。
        let win_w: f32 = window.viewport_size().width.into();
        let is_empty = self.open_files.is_empty();
        if self.resize_drag_x.is_some() || is_empty {
            self.last_window_width = win_w;
        } else {
            self.apply_window_follow(win_w);
        }
        let theme = cx.theme().ui;

        let inner = self.tab_bar.clone();

        let tab_bar: gpui::AnyElement = if is_empty {
            div()
                .w(px(0.0))
                .h(px(30.0))
                .overflow_hidden()
                .child(inner)
                .into_any_element()
        } else {
            let base = div()
                .h(px(30.0))
                .w_full()
                .px(px(6.0))
                .child(inner);
            let epoch = self.tab_bar_pad_epoch;
            let start = self.tab_bar_pad_start;
            let end = self.tab_bar_pad_end;
            let dur = self.tab_bar_pad_duration_ms;
            if epoch > 0 {
                let id = ("editor-tab-pad", epoch);
                base.with_animation(
                    id,
                    Animation::new(Duration::from_millis(dur)).with_easing(gpui::ease_in_out),
                    move |this, delta| this.pl(px(start + (end - start) * delta)),
                )
                .into_any_element()
            } else if end > 0.0 {
                base.pl(px(end)).into_any_element()
            } else {
                base.into_any_element()
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
                            .bg(gpui::transparent_black())
                            .px(px(6.0))
                            .pl(px(self.content_left_padding + 6.0))
                            .pb(px(self.content_bottom_padding))
                            .child(diff.clone())
                            .into_any_element()
                    } else if let Some(image) = &file.image {
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .bg(gpui::transparent_black())
                            .px(px(6.0))
                            .pl(px(self.content_left_padding + 6.0))
                            .pb(px(self.content_bottom_padding))
                            .child(image.clone())
                            .into_any_element()
                    } else if let Some(live) = &file.live {
                        div()
                            .id("md-live")
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .bg(gpui::transparent_black())
                            .px(px(6.0))
                            .pl(px(self.content_left_padding + 6.0))
                            .pb(px(self.content_bottom_padding))
                            .child(live.clone())
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
                            .bg(gpui::transparent_black())
                            .px(px(6.0))
                            .pl(px(self.content_left_padding + 6.0))
                            .pb(px(self.content_bottom_padding))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        // 双击预览区域：新开 tab 进入 md 实时模式
                                        if event.click_count == 2 {
                                            if let Some(config) = &ssh_config {
                                                this.open_remote_file_in_live_mode(
                                                    path.clone(),
                                                    config.clone(),
                                                    window,
                                                    cx,
                                                );
                                            } else {
                                                this.open_file_in_live_mode(
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
                            .bg(gpui::transparent_black())
                            .px(px(6.0))
                            .pl(px(self.content_left_padding + 6.0))
                            .pb(px(self.content_bottom_padding))
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_h_0()
                                    .child(EditorInput::new(&file.editor_state))
                                    // 跳转到行：顶部居中悬浮卡片
                                    .when_some(self.goto_line_panel.clone(), |this, panel| {
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
                                    }),
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

        let is_dark = cx.theme().appearance.is_dark();
        let resize_handle: gpui::AnyElement = if !is_empty {
            div()
                .absolute()
                .right(px(0.0))
                .top(px(8.0))
                .bottom(px(8.0))
                .w(px(4.0))
                .cursor_col_resize()
                .hover(|s| s.bg(theme.drop_target.opacity(0.3)))
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

        // 面板宽度动画（开合 / 侧边栏跟随）：外层 wrapper 控制可见宽度（动画
        // 期间由 with_animation 从 anim_start 驱动到 anim_end），内层 root 用
        // w_full() 填满 wrapper —— 否则内层固定在 anim_start(=0) 会导致内容/
        // 分割线全部 0 宽不可见。
        // epoch == 0（从未播放过动画，如启动/恢复）或动画目标与当前宽度不匹配
        // （clear()/外部改写宽度等状态突变）时直接应用目标宽度，避免残留旧动画终值。
        let anim_epoch = self.panel_anim_epoch;
        let (anim_start, anim_end, anim_ms) =
            (self.panel_anim_start, self.panel_anim_end, self.panel_anim_ms);
        let target_width = if is_empty { 0.0 } else { self.width };
        let animating = anim_epoch > 0
            && (anim_end - target_width).abs() <= 0.5
            && (anim_end - anim_start).abs() > 0.5;

        let with_panel_actions = |panel: gpui::Div| {
            panel
                .on_action(cx.listener(|this, _: &CloseActiveEditorTab, window, cx| {
                    if let Some(active_id) = this.active_file_id.clone() {
                        this.close_file(active_id.as_ref(), window, cx);
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
        };
        let language_menu = |menu: Entity<crate::components::menu::PopupMenu>| {
            gpui::deferred(
                gpui::anchored().child(
                    div()
                        .w(window.bounds().size.width)
                        .h(window.bounds().size.height)
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .child(
                            gpui::anchored()
                                .position(self.language_menu_position)
                                .snap_to_window_with_margin(px(8.0))
                                .child(menu.clone()),
                        ),
                ),
            )
            .with_priority(1)
        };
        let root: gpui::AnyElement = if is_empty {
            with_panel_actions(
                div()
                    .key_context("EditorPanel")
                    .w_full()
                    .h_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .items_start()
                    .overflow_hidden(),
            )
            .child(tab_bar)
            .child(content)
            .when_some(self.language_menu.clone(), |this, menu| {
                this.child(language_menu(menu))
            })
            .into_any_element()
        } else {
            let is_selected = self.tab_bar.read(cx).is_focused(window)
                || self.active_file_id.as_ref().is_some_and(|active_id| {
                    self.open_files.iter().any(|f| {
                        &f.id == active_id
                            && f.editor_state.read(cx).focus_handle.is_focused(window)
                    })
                });
            let border_color = if is_selected {
                theme.border_strong
            } else {
                theme.border
            };
            let glass_panel: gpui::AnyElement = div()
                .relative()
                .h_full()
                .w_full()
                .rounded(px(12.0))
                .bg(theme.app_background)
                .border_1()
                .border_color(border_color)
                .shadow(vec![
                    BoxShadow {
                        color: theme.shadow.opacity(if is_dark { 0.45 } else { 0.22 }),
                        offset: point(px(0.0), px(2.0)),
                        blur_radius: px(8.0),
                        spread_radius: px(-1.0),
                        inset: false,
                    },
                    BoxShadow {
                        color: theme.shadow.opacity(if is_dark { 0.34 } else { 0.14 }),
                        offset: point(px(0.0), px(14.0)),
                        blur_radius: px(36.0),
                        spread_radius: px(-10.0),
                        inset: false,
                    },
                ])
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(tab_bar)
                .child(content)
                .into_any_element();
            let padded: gpui::AnyElement = div()
                .w_full()
                .h_full()
                .pl(px(8.0))
                .pt(px(8.0))
                .pb(px(8.0))
                .relative()
                .child(glass_panel)
                .child(resize_handle)
                .child(EditorResizeElement {
                    editor: cx.entity().downgrade(),
                })
                .into_any_element();
            with_panel_actions(
                div()
                    .key_context("EditorPanel")
                    .w_full()
                    .h_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .overflow_hidden(),
            )
            .child(padded)
            .when_some(self.language_menu.clone(), |this, menu| {
                this.child(language_menu(menu))
            })
            .into_any_element()
        };

        // wrapper 保持固定 px：拖拽 1:1 跟手、最小宽度钳制精确。
        // 窗口级跟随另见 render 开头的 `apply_window_follow`（按比例吃掉
        // 窗口宽度增量），不经过 flex，避免 basis/渲染 1:2 脱节和
        // “basis 已触底、渲染还很宽”的假最小宽度问题。
        let display_width = if animating { anim_start } else { target_width };
        let wrapper = div()
            .id("editor-panel-width")
            .debug_selector(|| "editor-panel-width".into())
            .w(px(display_width))
            .h_full()
            // 固定 px 必须是硬约束：GPUI 默认 flex_shrink=1，否则面板过宽时
            // 会被 flex 收缩，实际渲染宽度与 `self.width` 脱节，破坏拖拽基准。
            .flex_shrink_0()
            .overflow_hidden()
            .child(root);
        if animating {
            wrapper
                .with_animation(
                    ("editor-panel-anim", anim_epoch),
                    Animation::new(Duration::from_millis(anim_ms)).with_easing(gpui::ease_in_out),
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
        crate::ssh::sftp_fs::write_file_bytes_atomic(config, path, &bytes)
            .map_err(|e| format!("写入远程文件失败: {e}"))?;
    } else {
        atomic_write_local(Path::new(path), &bytes)
            .map_err(|e| format!("写入本地文件失败: {e}"))?;
    }
    Ok(output)
}

/// 原子写本地文件：先写同目录临时文件再 rename，保证崩溃时不产生半截文件。
fn atomic_write_local(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = parent {
        std::fs::create_dir_all(dir)?;
    }

    #[cfg(unix)]
    let orig_mode = std::fs::metadata(path)
        .ok()
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()));

    let tmp = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
        let mut p = path.to_path_buf();
        p.set_file_name(format!(".{file_name}.tmp.{pid}-{nanos}"));
        p
    };

    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = orig_mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode));
        }
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Some(dir) = parent {
        if let Ok(f) = std::fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }
    Ok(())
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
            "css" | "scss" | "sass" => "css",
            "md" | "markdown" => "markdown",
            "sh" | "bash" | "zsh" => "bash",
            "toml" => "toml",
            "yaml" | "yml" => "yaml",
            "xml" => "xml",
            // 暂无高亮查询的语言统一回落 plain，避免存下无配置的非规范 id
            //（"sql"/"ruby"/"swift"/"php"/"perl"/"groovy" 会使高亮静默为空
            // 且状态栏需二次回落）。后续补 query 时再把对应分支改回规范 id。
            "java" => "java",
            "kt" | "kts" => "kotlin",
            "lua" => "lua",
            "zig" => "zig",
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

    crate::impl_source_location_none!();

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

    /// `line_ending = None` 表示「跟平台默认」，即 [`LineEnding::detect`] 对
    /// **无换行文本**的取值（windows 为 CRLF，其余为 LF）。断言必须跟着平台走，
    /// 否则同一份用例在 unix 与 windows 上结论相反。
    fn platform_default_eol() -> LineEnding {
        if cfg!(windows) {
            LineEnding::Windows
        } else {
            LineEnding::Unix
        }
    }

    /// 把只含 LF 的文本按平台默认行尾展开。
    fn with_platform_default_eol(text: &str) -> String {
        if cfg!(windows) {
            text.replace('\n', "\r\n")
        } else {
            text.to_string()
        }
    }

    /// 回归：标签页组切换后 `#live` 的 md 文件必须保持实时渲染。
    /// `restore_file_from_cache` 曾构造了 `LiveMarkdownView` 却以 `live: None`
    /// 存入 `EditorFile`，导致切组回来后退化为纯文本编辑态。
    #[gpui::test]
    fn restore_live_tab_from_cache_keeps_live_view(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct EditorHost(gpui::Entity<Editor>);
        impl gpui::Render for EditorHost {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            EditorHost(cx.new(|cx| Editor::new(window, cx)))
        });
        let editor = host.read_with(cx, |h, _| h.0.clone());
        let info = EditorOpenFileInfo {
            id: "notes.md#live".into(),
            path: "notes.md".into(),
            label: "notes.md".into(),
            ssh_config: None,
            is_loading: false,
            original_content: Some("# hi\n".to_string()),
            encoding: FileEncoding::Utf8,
            read_only: false,
            diff_repo: None,
        };
        cx.update(|window, cx| {
            let cached = cx.new(|cx| {
                let mut state = InputState::new(window, cx).multi_line(true);
                state.set_value("# hi\n", window, cx);
                state
            });
            editor.update(cx, |editor, cx| {
                editor.restore_file_from_cache(&info, cached, window, cx);
            });
            let live_ok = editor.read(cx).open_files.iter().any(|f| {
                f.id.as_ref() == "notes.md#live"
                    && f.live.is_some()
                    && f.live.as_ref().is_some_and(|live| {
                        live.read(cx).text() == "# hi\n"
                    })
            });
            assert!(live_ok, "live tab must keep its LiveMarkdownView after restore");
        });
    }

    /// 回归：恢复标签页组时，**已消失的文件**必须静默关掉标签，不弹模态框。
    ///
    /// 恢复会重新加载每个打开的文件，而文件可能在组被挂起期间被删掉/移走，
    /// 远端也可能连不上。曾经这类失败会走和「用户主动打开」同一条路径，弹出
    /// 「无法读取文件：No such file or directory (os error 2)」的模态框——
    /// 用户没做错什么，也不知道该点哪个按钮（「重新加载」只会再失败一次）。
    ///
    /// 这条断言的是恢复路径**不弹窗**：没有 window 也不该 panic，且失败后
    /// 该标签被清掉、不会留下一个永远加载中的空壳。
    #[gpui::test]
    fn restore_of_vanished_file_closes_tab_without_prompt(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct EditorHost(gpui::Entity<Editor>);
        impl gpui::Render for EditorHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            EditorHost(cx.new(|cx| Editor::new(window, cx)))
        });
        let editor = host.read_with(cx, |h, _| h.0.clone());

        // 指向一个保证不存在的路径，模拟「文件在挂起期间被删掉」。
        let missing = std::env::temp_dir().join("deepterm-restore-vanished-does-not-exist.txt");
        let _ = std::fs::remove_file(&missing);
        assert!(!missing.exists(), "前提：该路径必须不存在");

        let info = EditorOpenFileInfo {
            id: format!("editor-{}", missing.display()).into(),
            path: missing.to_string_lossy().to_string(),
            label: "vanished.txt".into(),
            ssh_config: None,
            is_loading: false,
            original_content: None,
            encoding: FileEncoding::Utf8,
            read_only: false,
            diff_repo: None,
        };

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.restore_from_info(vec![info], None, &HashMap::new(), &HashMap::new(), window, cx);
            });
        });
        // 让后台加载跑完并回到 UI 线程处理失败。
        cx.run_until_parked();

        editor.read_with(cx, |editor, _| {
            assert!(
                editor.open_files.iter().all(|f| !f.is_loading),
                "失败后不该留下永远加载中的标签"
            );
        });
    }

    /// 回归：标签页组切换后 git diff tab 必须重建 DiffView，而不是空白纯文本页。
    /// `restore_file_from_cache` 曾以 `diff: None` 存入，导致切组回来后空白。
    #[gpui::test]
    fn restore_diff_tab_from_cache_rebuilds_diff_view(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct EditorHost(gpui::Entity<Editor>);
        impl gpui::Render for EditorHost {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            EditorHost(cx.new(|cx| Editor::new(window, cx)))
        });
        let editor = host.read_with(cx, |h, _| h.0.clone());
        // 用临时 git 仓库构造可恢复的 diff 上下文：DiffView 后台加载即可，
        // 断言只关心 tab 重建后 `diff.is_some()`（非空白纯文本）。
        let tmp = std::env::temp_dir().join(format!("deepterm-diff-restore-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let _ = crate::editor::git_repository::git_command()
            .args(["init", "--quiet"])
            .current_dir(&tmp)
            .output();
        let repo = crate::editor::git_repository::GitRepository::find(&tmp, true, None)
            .expect("find repo")
            .expect("should find temp repo");
        // tab id 现含仓库标识（diff_tab_id），用同一规则构造期望 id
        //（repo.root() 是 rev-parse 的输出，macOS 上 /tmp 可能解析为
        // /private/tmp，不能拿 tmp 原样拼）。
        let diff_id: String = Editor::diff_tab_id("test.txt", &repo);
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_diff_file_loading("test.txt".to_string(), repo, false, window, cx);
            });
        });
        let state = editor.update(cx, |editor, cx| editor.save_to_file_tree_state(cx));
        assert_eq!(state.open_files_info.len(), 1);
        assert!(
            state.open_files_info[0].diff_repo.is_some(),
            "diff tab must carry its repo for reload"
        );
        let diff_key: SharedString = diff_id.clone().into();
        assert!(
            !state.cached_editor_states.contains_key(&diff_key),
            "diff placeholder state must not be cached as text"
        );
        // 路径一：整实体复用（保留未保存编辑），走真实切组路径。
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.restore_from_file_tree_state(state, window, cx);
            });
            let diff_ok = editor
                .read(cx)
                .open_files
                .iter()
                .any(|f| f.id.as_ref() == diff_id && f.diff.is_some());
            assert!(diff_ok, "diff tab must reuse its DiffView after restore");
        });
        // 路径二：无实体缓存时后台重载（兜底），同样不能是空白纯文本页。
        let state2 = editor.update(cx, |editor, cx| editor.save_to_file_tree_state(cx));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                let infos = state2.open_files_info.clone();
                let active = state2.active_file_id.clone();
                let empty_text: std::collections::HashMap<SharedString, Entity<InputState>> =
                    std::collections::HashMap::new();
                let empty_diff: std::collections::HashMap<
                    SharedString,
                    Entity<crate::editor::diff_view::DiffView>,
                > = std::collections::HashMap::new();
                editor.restore_from_info(infos, active, &empty_text, &empty_diff, window, cx);
            });
            let diff_ok = editor
                .read(cx)
                .open_files
                .iter()
                .any(|f| f.id.as_ref() == diff_id && f.diff.is_some());
            assert!(diff_ok, "diff tab must rebuild its DiffView after restore");
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 回归（B4）：两个不同仓库的同名相对路径不得共用 diff tab。
    /// 此前 tab id 只有 `diff-{relpath}`，第二个仓库的 src/main.rs 会命中
    /// 第一个仓库的旧 tab，显示错误内容。
    #[gpui::test]
    fn diff_tabs_from_distinct_repos_do_not_collide(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct EditorHost(gpui::Entity<Editor>);
        impl gpui::Render for EditorHost {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            EditorHost(cx.new(|cx| Editor::new(window, cx)))
        });
        let editor = host.read_with(cx, |h, _| h.0.clone());

        let mk_repo = |tag: &str| {
            let dir = std::env::temp_dir()
                .join(format!("deepterm-diff-collide-{tag}-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let _ = crate::editor::git_repository::git_command()
                .args(["init", "--quiet"])
                .current_dir(&dir)
                .output();
            dir
        };
        let dir_a = mk_repo("a");
        let dir_b = mk_repo("b");
        let repo_a = crate::editor::git_repository::GitRepository::find(&dir_a, true, None)
            .unwrap()
            .expect("repo a");
        let repo_b = crate::editor::git_repository::GitRepository::find(&dir_b, true, None)
            .unwrap()
            .expect("repo b");

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_diff_file_loading(
                    "src/main.rs".to_string(),
                    repo_a.clone(),
                    false,
                    window,
                    cx,
                );
                editor.open_diff_file_loading(
                    "src/main.rs".to_string(),
                    repo_b.clone(),
                    false,
                    window,
                    cx,
                );
            });
        });
        editor.read_with(cx, |editor, _cx| {
            let ids: Vec<&SharedString> = editor
                .open_files
                .iter()
                .filter(|f| f.diff.is_some())
                .map(|f| &f.id)
                .collect();
            assert_eq!(ids.len(), 2, "两个仓库的同名路径必须是两个独立 tab: {ids:?}");
            assert_ne!(ids[0], ids[1]);
            // 每个仓库各有一个可解析到自身的 repo。
            let repos: Vec<&crate::editor::git_repository::GitRepository> = editor
                .open_files
                .iter()
                .filter_map(|f| f.diff_repo.as_ref())
                .collect();
            assert_eq!(repos.len(), 2);
            assert_ne!(repos[0].root(), repos[1].root());
        });

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// 回归：窗口拉宽时编辑器必须按比例跟着变宽（而不是钉死在拖拽宽度上），
    /// 且拖拽/最小宽度语义保持精确（固定 px，不经过 flex）。
    #[gpui::test]
    fn editor_width_follows_window(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct WidthHost {
            editor: gpui::Entity<Editor>,
        }
        impl gpui::Render for WidthHost {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
                    .size_full()
                    .flex()
                    .flex_row()
                    .child(self.editor.clone())
                    .child(gpui::div().flex_1())
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| Editor::new(window, cx));
            editor.update(cx, |e, _| e.suppress_open_animation = true);
            WidthHost { editor }
        });
        let editor = host.read_with(cx, |h, _| h.editor.clone());
        let info = EditorOpenFileInfo {
            id: "a.txt".into(),
            path: "a.txt".into(),
            label: "a.txt".into(),
            ssh_config: None,
            is_loading: false,
            original_content: Some("hello\n".to_string()),
            encoding: FileEncoding::Utf8,
            read_only: false,
            diff_repo: None,
        };
        cx.update(|window, cx| {
            let cached = cx.new(|cx| {
                let mut state = InputState::new(window, cx).multi_line(true);
                state.set_value("hello\n", window, cx);
                state
            });
            editor.update(cx, |editor, cx| {
                editor.restore_file_from_cache(&info, cached, window, cx);
            });
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let w0: f32 = cx.update(|window, _| window.viewport_size().width.into());
        let w1 = cx
            .debug_bounds("editor-panel-width")
            .map(|b| f32::from(b.size.width))
            .unwrap_or(-1.0);
        assert!((w1 - 680.0).abs() <= 1.0, "default width, got {w1:.1}");
        // 窗口跟随吃“变化量”（测试 harness 的 viewport 固定 1920，
        // resize 测不出，直接调 `apply_window_follow` 验证数学）。
        // 窗口 1200 -> 1600（+400）：编辑器吃掉一半增量。
        editor.update(cx, |e, _| {
            e.last_window_width = w0;
            e.apply_window_follow(w0 + 400.0);
            assert!(
                (e.width - 880.0).abs() <= 0.5,
                "editor must take half the window growth, width={:.1}",
                e.width
            );
            // 同一宽度重复调不漂移。
            e.apply_window_follow(w0 + 400.0);
            assert!((e.width - 880.0).abs() <= 0.5, "no drift, width={:.1}", e.width);
            // 窗口骤窄：按比例收缩，钳在最小宽度。
            e.apply_window_follow(200.0);
            assert!(
                (e.width - EDITOR_MIN_WIDTH).abs() <= 0.5,
                "shrinks with floor, width={:.1}",
                e.width
            );
            // 上限：已到 limit 时窗口继续变宽也不增长，避免把主区挤出窗口。
            e.width = 480.0;
            e.window_follow_limit = 500.0;
            e.apply_window_follow(600.0); // last=200 → dw=400
            assert!(
                (e.width - 500.0).abs() <= 0.5,
                "follow must respect limit, width={:.1}",
                e.width
            );
            e.window_follow_limit = f32::INFINITY;
        });
        let w2 = 880.0;
        // 拖拽 1:1 且最小宽度钳制精确（247px 上限内直接算，不依赖鼠标事件）。
        editor.update(cx, |e, _| {
            e.width = w2;
            e.start_resize(gpui::px(1000.0));
            e.update_resize(gpui::px(900.0));
            assert!(
                (e.width - (w2 - 100.0)).abs() <= 0.5,
                "drag must track 1:1, width={:.1} base={w2:.1}",
                e.width
            );
            e.update_resize(gpui::px(-10000.0));
            assert!(
                (e.width - EDITOR_MIN_WIDTH).abs() <= 0.5,
                "min width clamp must be exact, width={:.1}",
                e.width
            );
            // 上限：拖拽同样不能超过 limit（固定 px 是硬约束，不交给 flex 收缩）。
            e.window_follow_limit = 500.0;
            e.update_resize(gpui::px(20000.0));
            assert!(
                (e.width - 500.0).abs() <= 0.5,
                "drag must respect limit, width={:.1}",
                e.width
            );
            e.window_follow_limit = f32::INFINITY;
            e.end_resize();
        });
    }

    /// 侧边栏改变宽度（显隐/拖拽）时，编辑器必须按同一比例分摊，
    /// 而不是让主区（终端）独吞全部增量。
    #[gpui::test]
    fn editor_shares_sidebar_width_change(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct WidthHost {
            editor: gpui::Entity<Editor>,
        }
        impl gpui::Render for WidthHost {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
                    .size_full()
                    .flex()
                    .flex_row()
                    .child(self.editor.clone())
                    .child(gpui::div().flex_1())
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| Editor::new(window, cx));
            editor.update(cx, |e, _| e.suppress_open_animation = true);
            WidthHost { editor }
        });
        let editor = host.read_with(cx, |h, _| h.editor.clone());
        let info = EditorOpenFileInfo {
            id: "a.txt".into(),
            path: "a.txt".into(),
            label: "a.txt".into(),
            ssh_config: None,
            is_loading: false,
            original_content: Some("hello\n".to_string()),
            encoding: FileEncoding::Utf8,
            read_only: false,
            diff_repo: None,
        };
        // 空编辑器不参与分摊：侧边栏宽度变化全部归主区。
        editor.update(cx, |e, cx| {
            e.open_files.clear();
            e.active_file_id = None;
            e.width = EDITOR_DEFAULT_WIDTH;
            e.follow_sidebar_width(280.0, 10_000.0, 0, cx);
            assert!(
                (e.width - EDITOR_DEFAULT_WIDTH).abs() <= 0.5,
                "empty editor must not take sidebar width, width={:.1}",
                e.width
            );
        });

        // 恢复文件（非空）后继续验证分摊数学。
        cx.update(|window, cx| {
            let cached = cx.new(|cx| {
                let mut state = InputState::new(window, cx).multi_line(true);
                state.set_value("hello\n", window, cx);
                state
            });
            editor.update(cx, |editor, cx| {
                editor.restore_file_from_cache(&info, cached, window, cx);
            });
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        editor.update(cx, |e, cx| {
            e.width = 680.0;
            // 收起侧边栏：delta = -280（侧边栏变窄）→ 编辑器 +140。
            e.follow_sidebar_width(-280.0, 10_000.0, 0, cx);
            assert!(
                (e.width - 820.0).abs() <= 0.5,
                "collapse must share half with editor, width={:.1}",
                e.width
            );
            // 展开侧边栏：对称让回一半，来回切换可完全复原。
            e.follow_sidebar_width(280.0, 10_000.0, 0, cx);
            assert!(
                (e.width - 680.0).abs() <= 0.5,
                "expand must symmetrically give back, width={:.1}",
                e.width
            );
            // 上限：侧边栏收起时也不能把主区挤到 MAIN_AREA_MIN_WIDTH 以下。
            e.width = 680.0;
            e.follow_sidebar_width(-280.0, 700.0, 0, cx);
            assert!(
                (e.width - 700.0).abs() <= 0.5,
                "must respect limit, width={:.1}",
                e.width
            );
            // 最小宽度：侧边栏大幅变宽时编辑器不会缩到 200 以下。
            e.follow_sidebar_width(5000.0, 10_000.0, 0, cx);
            assert!(
                (e.width - EDITOR_MIN_WIDTH).abs() <= 0.5,
                "must respect min width, width={:.1}",
                e.width
            );
        });

        // 显隐动画：宽度立即写为目标值（基准），渲染层插值由 panel 动画槽驱动。
        editor.update(cx, |e, cx| {
            e.width = 680.0;
            e.follow_sidebar_width(-280.0, 10_000.0, 300, cx);
            assert!((e.width - 820.0).abs() <= 0.5, "width is the target");
            assert!(
                (e.panel_anim_start - 680.0).abs() <= 0.5
                    && (e.panel_anim_end - 820.0).abs() <= 0.5
                    && e.panel_anim_ms == 300,
                "animated branch must record start/end/ms, got {:.1}->{:.1} {}ms",
                e.panel_anim_start,
                e.panel_anim_end,
                e.panel_anim_ms
            );
        });

        // 拖拽（ms == 0）：立即结算且不记录动画（起止相等），保证 1:1 跟手；
        // 同时不动 panel epoch，避免之后拖回旧目标时重播一次过期动画。
        editor.update(cx, |e, cx| {
            e.width = 680.0;
            e.panel_anim_epoch = 0;
            e.follow_sidebar_width(100.0, 10_000.0, 0, cx);
            assert!(
                (e.width - 630.0).abs() <= 0.5,
                "drag must share half, width={:.1}",
                e.width
            );
            assert!(
                (e.panel_anim_start - e.panel_anim_end).abs() <= 0.5,
                "immediate path must not record an animation"
            );
            assert_eq!(e.panel_anim_epoch, 0, "immediate path must not bump epoch");
        });
    }

    /// 回归：恢复（切标签页组/文件树）后不得残留面板宽度动画与 tab 栏 pad 动画。
    ///
    /// 恢复期间 `open_anim_epoch` 不 bump、`sidebar_epoch` 也不变，pad 的三条
    /// 同步路径都不进；残留的旧起点会在下次渲染重播一次滑动动画（用户可见：
    /// 文件 tab 从中间滑到左侧、终端整体左移）。
    #[gpui::test]
    fn restore_does_not_replay_stale_animations(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct Host {
            editor: gpui::Entity<Editor>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
                    .size_full()
                    .flex()
                    .flex_row()
                    .child(self.editor.clone())
                    .child(gpui::div().flex_1())
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| Editor::new(window, cx));
            Host { editor }
        });
        let editor = host.read_with(cx, |h, _| h.editor.clone());
        let info = EditorOpenFileInfo {
            id: "a.txt".into(),
            path: "a.txt".into(),
            label: "a.txt".into(),
            ssh_config: None,
            is_loading: false,
            original_content: Some("hello\n".to_string()),
            encoding: FileEncoding::Utf8,
            read_only: false,
            diff_repo: None,
        };
        let open_one = |editor: &gpui::Entity<Editor>, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let cached = cx.new(|cx| {
                    let mut state = InputState::new(window, cx).multi_line(true);
                    state.set_value("hello\n", window, cx);
                    state
                });
                editor.update(cx, |editor, cx| {
                    editor.restore_file_from_cache(&info, cached, window, cx);
                });
            });
            cx.refresh().unwrap();
            cx.run_until_parked();
        };

        open_one(&editor, cx);
        // 造出「残留旧动画 + 已播完的 pad 动画」：面板槽指向一个过期的目标，
        // pad 槽带着非零 epoch 与旧起点。
        editor.update(cx, |e, _| {
            e.panel_anim_epoch = 7;
            e.panel_anim_start = 100.0;
            e.panel_anim_end = 900.0;
            e.panel_anim_ms = 300;
            e.tab_bar_pad_epoch = 5;
            e.tab_bar_pad_start = 124.0;
            e.tab_bar_pad_end = 6.0;
            e.tab_bar_pad_duration_ms = 300;
        });

        // 走真实的「保存 → 恢复」链路（切标签页组/文件树所走的那条）。
        let state = editor.update(cx, |e, cx| e.save_to_file_tree_state(cx));
        assert!(
            state.open_files_info.len() == 1,
            "save must capture the open file"
        );
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.restore_from_file_tree_state(state, window, cx);
            });
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        editor.update(cx, |e, _| {
            assert_eq!(
                e.panel_anim_epoch, 0,
                "restore must reset the panel width animation slot"
            );
            assert!(
                (e.panel_anim_start - e.panel_anim_end).abs() <= 0.5,
                "restore must pin the panel slot to the current width, got {:.1}->{:.1}",
                e.panel_anim_start,
                e.panel_anim_end
            );
            assert!(
                (e.panel_anim_end - e.width).abs() <= 0.5,
                "panel slot must target the restored width, got {:.1} vs {:.1}",
                e.panel_anim_end,
                e.width
            );
            assert!(
                e.tab_bar_pad_snap_request,
                "restore must request a static tab bar pad sync"
            );
        });

        // 消费请求：取走即清除，且重复调用不再触发。
        assert!(
            editor.update(cx, |e, _| e.take_tab_bar_pad_snap_request()),
            "first take must observe the request"
        );
        assert!(
            !editor.update(cx, |e, _| e.take_tab_bar_pad_snap_request()),
            "request must be cleared after being taken"
        );
    }

    /// 恢复请求只能把 pad 静态对齐到目标值；目标由「编辑器空满 × 侧边栏显隐」
    /// 决定，恢复后若编辑器为空则不应写任何 pad 状态（AppWindow 侧有对应守卫）。
    #[gpui::test]
    fn snap_request_clears_when_editor_empty(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        let (host, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| Editor::new(window, cx));
            struct Host {
                editor: gpui::Entity<Editor>,
            }
            impl gpui::Render for Host {
                fn render(
                    &mut self,
                    _: &mut Window,
                    _: &mut Context<Self>,
                ) -> impl gpui::IntoElement {
                    gpui::div().size_full().child(self.editor.clone())
                }
            }
            Host { editor }
        });
        let editor = host.read_with(cx, |h, _| h.editor.clone());

        // 空编辑器恢复：请求仍会被消费（不滞留），但 AppWindow 不会写 pad。
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.restore_from_file_tree_state(
                    FileTreeEditorState {
                        open_files_info: Vec::new(),
                        cached_editor_states: HashMap::new(),
                        cached_diff_views: HashMap::new(),
                        active_file_id: None,
                    },
                    window,
                    cx,
                );
            });
        });
        assert!(
            editor.update(cx, |e, _| e.take_tab_bar_pad_snap_request()),
            "request must be raised even for an empty restore"
        );
        assert!(
            !editor.update(cx, |e, _| e.take_tab_bar_pad_snap_request()),
            "request must be cleared after being taken"
        );
    }

    #[test]
    fn detect_line_ending_lf() {
        assert_eq!(LineEnding::detect("line1\nline2\n"), LineEnding::Unix);
        // 无换行 → 平台默认
        assert_eq!(LineEnding::detect("hello world"), platform_default_eol());
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
        let dir = std::env::temp_dir().join(format!("deepterm-save-test-{}", std::process::id()));
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
        let dir =
            std::env::temp_dir().join(format!("deepterm-save-test-{}-parent", std::process::id()));
        let path = dir.join("sub").join("new.txt");
        let written = save_text_to_file(
            path.to_str().unwrap(),
            None,
            "",
            "hello\n",
            FileEncoding::Utf8,
        )
        .unwrap();
        let expected = with_platform_default_eol("hello\n");
        assert_eq!(written, expected);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
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
        let expected = with_platform_default_eol("你好，世界\n");
        assert_eq!(written, expected);
        // 磁盘上是 GBK 字节，不足 UTF-8
        let bytes = std::fs::read(&path).unwrap();
        assert_ne!(bytes, "你好，世界\r\n".as_bytes());
        let decoded = crate::editor::encoding::decode_bytes(&bytes);
        assert_eq!(decoded.encoding, FileEncoding::Gbk);
        assert_eq!(decoded.text, expected);
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
