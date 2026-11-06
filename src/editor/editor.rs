/// 把状态栏的弹出菜单挂到点击处。
///
/// 两层 `anchored`：外层铺满整个窗口，负责「点外面就关」；内层按锚点定位。
///
/// **`Anchor::BottomRight` 是刻意的**：状态栏在窗口底部，菜单从 chip 正上方
/// 往上长，是唯一不会挡住编辑区的方向；右对齐则保证它不会从窗口右边缘跑
/// 出去（`snap_to_window_with_margin` 管左/上/下三侧）。默认的 `TopLeft`
/// 会让菜单从点击点往右下长，压在编辑器正文上。
///
/// ## 为什么关闭要挂在外层 div 上，而不是菜单自己的 `on_mouse_down_out`
///
/// `on_mouse_down_out` 的触发条件是「指针在该元素的 bounds 之外」，判定完全
/// 依赖命中测试。而这里是 `deferred` + `anchored` 的浮层，命中测试的结果不
/// 总是如我们所愿——实测点了外面菜单毫无反应。外层这块铺满窗口的 div 则没有
/// 这个问题：任何一次按下都先落到它头上（菜单画在它之上，但**捕获阶段是
/// 父先子后**，所以父永远先收到）。
///
/// 代价是「点菜单项也会触发关闭」。这恰好是 Zed / VSCode 的行为（菜单项点完
/// 即关），比留着菜单让人再点一次外面更符合预期。
///
/// 泛型在 `E` 上：语言菜单（`PopupMenu`）与 LSP 菜单（`LspMenu`）只有
/// `Render` 不同，锚定逻辑完全一样。
fn render_status_menu<E: Render + 'static>(
    menu: &Entity<E>,
    position: gpui::Point<Pixels>,
    window: &Window,
    on_dismiss: std::rc::Rc<dyn Fn(&mut Window, &mut gpui::App)>,
) -> gpui::AnyElement {
    gpui::deferred(
        gpui::anchored().child(
            div()
                .w(window.bounds().size.width)
                .h(window.bounds().size.height)
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                // 整窗遮罩：任何按下都关菜单。不再依赖命中测试。
                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                    on_dismiss(window, cx)
                })
                .child(
                    gpui::anchored()
                        .anchor(gpui::Anchor::BottomRight)
                        .position(position)
                        .snap_to_window_with_margin(px(8.0))
                        .child(menu.clone()),
                ),
        ),
    )
    .with_priority(1)
    .into_any_element()
}

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
use crate::assets::file_icon_or_default;
use crate::components::input::RopeExt as _;
use crate::components::input::{EditorInput, InputState};
use crate::components::notification::NotificationKind;
use crate::editor::completion;
use crate::editor::encoding::{self, FileEncoding};
use crate::editor::ex_command;
use crate::editor::git_marks::compute_git_diff_summary;
use crate::editor::git_repository::GitRepository;
use crate::editor::goto_line;
use crate::editor::hover;
use crate::editor::image_viewer::{ActualSize, FitToView, ResetZoom, ZoomIn, ZoomOut};
use crate::editor::rename;
use crate::editor::symbols;
use crate::editor::syntax::languages::Language;
use crate::file_manager::filesystem::RemoteFileSystem;
use crate::file_manager::watcher::{FsChangeEvent, FsChangeKind, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;
use crate::workspace::tab_bar::{DraggedTab, TabBar, TabItem};

const EDITOR_DEFAULT_WIDTH: f32 = 680.0;
const EDITOR_MIN_WIDTH: f32 = 200.0;
/// 首次打开文件时编辑器占「可用宽度」（窗口宽 − 侧边栏占位）的比例：
/// 0.5 即与终端区域平分。只在空 → 非空的开合路径生效。
const EDITOR_OPEN_SPLIT_RATIO: f32 = 0.5;
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
    [
        CloseActiveEditorTab,
        SaveActiveEditorTab,
        GotoLine,
        /// 跳转定义（F12）。
        GotoDefinition,
        /// 跳转声明（ctrl-F12）：C 头文件原型 vs .c 实现。
        GotoDeclaration,
        /// 跳转类型定义（ctrl-shift-F12）：变量 → 它的类型。
        GotoTypeDefinition,
        /// 跳转实现（cmd-F12）：trait → 实现类型。
        GotoImplementation,
        /// 返回上一个跳转位置（ctrl--）。
        NavigateBack,
        /// 查找引用（shift-F12）。
        FindReferences,
        /// 重命名符号（F2）。
        RenameSymbol,
        /// 格式化文档（alt-shift-f）。
        FormatDocument,
        /// 手动触发补全（ctrl-space）。
        TriggerCompletion,
        /// 切换 Vim 模式（状态栏徽章 / 快捷键）。
        ToggleVim,
        /// 打开 Vim `:` 命令行。
        ExCommandLine,
        /// `:` 命令行：提交。
        ExSubmit,
        /// `:` 命令行：取消。
        ExCancel,
        /// `:` 命令行：Tab 补全。
        ExComplete,
        /// `:` 命令行：上一条历史。
        ExHistoryPrev,
        /// `:` 命令行：下一条历史。
        ExHistoryNext,
        /// 补全菜单：上下移动 / 首尾 / 确认 / 取消。
        CompletionNext,
        CompletionPrev,
        CompletionFirst,
        CompletionLast,
        CompletionConfirm,
        CompletionCancel,
        /// 文档符号大纲（cmd-shift-o）。
        DocumentSymbols,
        /// 应用首选 Code Action（快速修复，cmd-.）。
        CodeAction,
        /// 切换 hover 浮层的键盘接管（vim normal 下的 `K`）。
        ///
        /// 第一次按：用**光标位置**请求 hover 并显示。
        /// 第二次按：把焦点移进浮层，此后 `hjkl` / 方向键滚动内容，
        /// 鼠标移动与编辑器失焦都不再收起它。
        ///
        /// 对照 Zed：它的 `K` 绑到 `editor::Hover`，而 `show_keyboard_hover`
        /// （`hover_popover.rs:98`）在已有键盘触发的浮层时会重新显示它，
        /// 配合 `InfoPopover::focused()` 达成同样的「第二次进入」语义。
        Hover
    ]
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
    // 语言服务器控制菜单的 Escape 关闭。
    crate::editor::lsp_menu::init(cx);
    // hover 浮层的键盘接管（`K` 第二次按下）靠全局按键拦截器实现 ——
    // 卡片是 `defer_draw` 出来的，进不了 dispatch 树，`window.focus` 无效。
    // 详情见 `hover::init_keystroke_interceptor`。
    crate::editor::hover::init_keystroke_interceptor(cx);
    // 实时 markdown 与只读预览共用复制/全选语义，仅上下文不同。
    crate::keymap::bind(
        cx,
        [
            KeyBinding::new("cmd-c", crate::editor::markdown::Copy, Some("LiveMarkdown")),
            KeyBinding::new(
                "ctrl-c",
                crate::editor::markdown::Copy,
                Some("LiveMarkdown"),
            ),
            KeyBinding::new(
                "cmd-a",
                crate::editor::markdown::SelectAll,
                Some("LiveMarkdown"),
            ),
            KeyBinding::new(
                "ctrl-a",
                crate::editor::markdown::SelectAll,
                Some("LiveMarkdown"),
            ),
        ],
    );
    crate::keymap::bind(
        cx,
        [
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
            // LSP：跳转定义 / 查找引用 / 重命名 / 格式化，全平台同键。
            KeyBinding::new("f12", GotoDefinition, Some("EditorPanel")),
            KeyBinding::new("ctrl-f12", GotoDeclaration, Some("EditorPanel")),
            KeyBinding::new("ctrl-shift-f12", GotoTypeDefinition, Some("EditorPanel")),
            KeyBinding::new("cmd-f12", GotoImplementation, Some("EditorPanel")),
            // 跳转返回：与 VSCode/Zed 一致用 ctrl--（macOS 上也常见 cmd--）。
            KeyBinding::new("ctrl--", NavigateBack, Some("EditorPanel")),
            KeyBinding::new("shift-f12", FindReferences, Some("EditorPanel")),
            KeyBinding::new("f2", RenameSymbol, Some("EditorPanel")),
            KeyBinding::new("alt-shift-f", FormatDocument, Some("EditorPanel")),
            KeyBinding::new("cmd-shift-o", DocumentSymbols, Some("EditorPanel")),
            KeyBinding::new("cmd-.", CodeAction, Some("EditorPanel")),
            // 键盘触发 hover（vim normal 下的 `K`）：第一次显示浮层，第二次把
            // 焦点移进浮层，此后 hjkl 滚动内容。
            //
            // 放在 Rust 侧而不是 `default.json`：与上面这批 LSP 快捷键一致，且
            // `keymap::tests::default_keymaps_cover_all_migrated_bindings` 会核对
            // JSON 与迁移前的键位总数，加在 JSON 里得同步改那个魔数。
            KeyBinding::new("K", Hover, Some("EditorPanel")),
            // 补全触发/导航键在 `assets/keymaps/completion.json`：
            // - 全部无条件绑在 `Input` 上，先于普通编辑键；
            // - handler 在菜单不可见时 `cx.propagate()`，普通 Enter/Tab/
            //   方向键/escape 照常执行；菜单可见时才消费。
            //
            // **不能**绑在 `EditorPanel`：`Input` 上已有 enter/escape/up/down，
            // 同深度下更深（Input）的绑定先派发并消费事件，补全动作永远收不到。
            KeyBinding::new("cmd-0", ActualSize, Some("ImageViewer")),
            KeyBinding::new("cmd-9", FitToView, Some("ImageViewer")),
            KeyBinding::new("cmd-1", ResetZoom, Some("ImageViewer")),
        ],
    );
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

/// 一个标签当前展示的内容形态。互斥选项收进 enum，避免
/// `markdown/live/image/diff` 同时为 Some 的非法状态。
enum TabContent {
    /// 普通文本编辑器（`editor_state`）。
    Code,
    MarkdownPreview(Entity<crate::editor::markdown::Markdown>),
    Live(Entity<crate::editor::markdown_live::LiveMarkdownView>),
    Image(Entity<crate::editor::image_viewer::ImageViewer>),
    Diff {
        view: Entity<crate::editor::diff_view::DiffView>,
        /// diff 所属仓库；会话恢复时可能暂时缺失，由 DiffView 自行重载。
        repo: Option<crate::editor::git_repository::GitRepository>,
    },
}

impl TabContent {
    fn markdown(&self) -> Option<&Entity<crate::editor::markdown::Markdown>> {
        match self {
            Self::MarkdownPreview(markdown) => Some(markdown),
            _ => None,
        }
    }

    fn live(&self) -> Option<&Entity<crate::editor::markdown_live::LiveMarkdownView>> {
        match self {
            Self::Live(live) => Some(live),
            _ => None,
        }
    }

    fn image(&self) -> Option<&Entity<crate::editor::image_viewer::ImageViewer>> {
        match self {
            Self::Image(image) => Some(image),
            _ => None,
        }
    }

    fn diff(&self) -> Option<&Entity<crate::editor::diff_view::DiffView>> {
        match self {
            Self::Diff { view, .. } => Some(view),
            _ => None,
        }
    }

    fn diff_repo(&self) -> Option<&crate::editor::git_repository::GitRepository> {
        match self {
            Self::Diff { repo, .. } => repo.as_ref(),
            _ => None,
        }
    }
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
    /// 当前标签的展示内容；与 `editor_state`（影子/文本编辑器）配合。
    content: TabContent,
    /// LSP 会话。本地文件由本地子进程服务；远程文件由**那台主机上**的进程
    /// 服务（见 `LspSession::ssh_config`）——远端路径在远端服务器看来就是
    /// 它自己的绝对路径，语义成立。
    ///
    /// `None` 也可能是「该语言没有服务器」或「默认关闭且还没手动起」——
    /// 失败一律降级为普通编辑器。
    lsp: Option<crate::lsp::session::LspSession>,
}

impl EditorFile {
    fn markdown(&self) -> Option<&Entity<crate::editor::markdown::Markdown>> {
        self.content.markdown()
    }

    fn live(&self) -> Option<&Entity<crate::editor::markdown_live::LiveMarkdownView>> {
        self.content.live()
    }

    fn image(&self) -> Option<&Entity<crate::editor::image_viewer::ImageViewer>> {
        self.content.image()
    }

    fn diff(&self) -> Option<&Entity<crate::editor::diff_view::DiffView>> {
        self.content.diff()
    }

    fn diff_repo(&self) -> Option<&crate::editor::git_repository::GitRepository> {
        self.content.diff_repo()
    }

    /// 这个标签的语言是否匹配某个服务器 key。
    ///
    /// 会话是权威来源（它记录了接入时用的 `language_id`，不会随用户在标签里
    /// 临时改语言而变）；没有会话时退回编辑器当前的语言。
    fn lsp_language_matches(&self, language: &str, cx: &App) -> bool {
        match self.lsp.as_ref() {
            Some(session) => session.language_id() == language,
            None => self.editor_state.read(cx).language().as_deref() == Some(language),
        }
    }
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
    /// 有未保存修改：文件树状态淘汰时据此跳过（淘汰会丢全文与撤销历史，
    /// 脏文件必须留下等用户处理）。加载中的文件为 false。
    pub dirty: bool,
    /// diff tab 的仓库快照（仅 `diff-` tab 为 Some）：恢复时后台重载 diff，
    /// 而不是走纯文本 `restore_file_from_cache`（否则页面空白）。
    pub diff_repo: Option<crate::editor::git_repository::GitRepository>,
}

/// 打开文件的三种展示模式。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    Preview,
    Edit,
    Live,
}

/// `open_local_file_inner` / `open_remote_file_inner` 的附加选项。
#[derive(Clone, Copy)]
struct OpenOptions {
    mode: OpenMode,
    silent: bool,
}

/// 打开一个已加载文件的完整请求；把 `open_file_with_content` 的 13 个
/// 位置参数收成一个具名结构，避免布尔参数含义在调用点丢失。
struct OpenFileRequest {
    tab_id: String,
    path_str: String,
    content: String,
    path: PathBuf,
    ssh_config: Option<SshConnectionConfig>,
    encoding: FileEncoding,
    read_only: bool,
    is_loading: bool,
    edit_mode: bool,
    live_mode: bool,
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
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > MAX_EDITOR_FILE_BYTES as u64
    {
        return Err(too_large(meta.len(), true));
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
    if let Ok(meta) = crate::ssh::sftp_fs::metadata(config, path)
        && meta.size > MAX_EDITOR_FILE_BYTES as u64
    {
        return Err(format!(
            "文件过大（{} MB），超过 {MAX_EDITOR_FILE_MB} MB 限制",
            meta.size / (1024 * 1024)
        ));
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

/// 按 tab id 索引的 LSP 会话表：切标签页组/文件树时随编辑器状态换手。
type LspSessionMap = HashMap<SharedString, crate::lsp::session::LspSession>;

/// 每个文件树的编辑器状态
pub struct FileTreeEditorState {
    pub open_files_info: Vec<EditorOpenFileInfo>,
    pub cached_editor_states: HashMap<SharedString, Entity<InputState>>,
    /// diff tab 的视图缓存：切组时整实体保留，未保存的右侧编辑、滚动、折叠状态不丢。
    pub cached_diff_views: HashMap<SharedString, Entity<crate::editor::diff_view::DiffView>>,
    /// 各标签的 LSP 会话：切组时随 `InputState` 一起换手。
    ///
    /// 不能留在 `EditorFile` 里。`save_to_file_tree_state` 会 `clear()` 整个
    /// 编辑器，会话的 `Drop` 于是发出 `didClose`；而恢复路径原本只重建文本与
    /// tab，从不重建会话——服务器进程仍活在 `LspManager` 里，编辑器手上却
    /// 没有 client，hover / 跳转 / 补全全部静默失效。把会话搬进状态表，
    /// 切回来原样放回 `EditorFile`，文档在服务器侧一直开着，无需重连。
    ///
    /// 有界性：随 `FileTreeEditorStates`（`lib.rs`）的 8 条上限一起淘汰；
    /// 淘汰即 drop 会话 → 正常补发 `didClose`。
    pub cached_lsp_sessions: LspSessionMap,
    pub active_file_id: Option<SharedString>,
}

impl FileTreeEditorState {
    /// 是否含未保存修改（决定文件树状态能否被淘汰）。
    ///
    /// 淘汰会丢掉 `cached_editor_states` 里的 InputState 实体——那是该树下
    /// 所有打开文件的全文与撤销历史的唯一持有者，切回时只能重新从磁盘读。
    /// 因此脏条目必须留下，宁可让有界表暂时超限。
    pub fn has_unsaved_edits(&self) -> bool {
        self.open_files_info.iter().any(|info| info.dirty)
    }
}

/// 恢复编辑器时按 tab id 索引的三类缓存。
///
/// 三张表必须一起传：`InputState` 是文本的唯一持有者，LSP 会话与它必须落在
/// 同一个 tab id 上。分开传参时恢复路径只认识前两张，会话就被落下了
/// ——`lsp: None` 那个历史 bug 正是这么来的。
struct RestoreBuffers<'a> {
    states: &'a HashMap<SharedString, Entity<InputState>>,
    diffs: &'a HashMap<SharedString, Entity<crate::editor::diff_view::DiffView>>,
    lsp_sessions: &'a mut LspSessionMap,
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
    /// 「可用宽度」：窗口宽 − 侧边栏占位，由 AppWindow 每帧下发。
    /// 首次打开文件时按它的一半确定默认宽度（与终端区域平分）。
    /// 0.0 表示尚未下发（如单测），此时沿用 [`EDITOR_DEFAULT_WIDTH`]。
    available_width: f32,
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
    /// tab_id -> pending LSP jump (applied after async load).
    ///
    /// LSP 的 `Position.character` 是 UTF-16 code unit，而 `jump_to_line_col`
    /// 的列按 Unicode `char` 计数。若把 LSP 列直接塞进 `pending_jumps`，
    /// 目标行里有 emoji 等非 BMP 字符时光标会左偏。这里保留原始 Position，
    /// 等文件内容加载完成后再按 UTF-16 精确换算字节 offset。
    pending_lsp_jumps: HashMap<String, lsp_types::Position>,
    /// 跳转到行面板（cmd-l / ctrl-g 切换）。
    goto_line_panel: Option<Entity<goto_line::GotoLinePanel>>,
    /// Vim `:` 命令行面板。
    ex_command_panel: Option<Entity<ex_command::ExCommandPanel>>,
    /// 语言选择弹出菜单（状态栏语言 chip 点击时创建）。
    language_menu: Option<Entity<crate::components::menu::PopupMenu>>,
    _language_menu_subscription: Option<Subscription>,
    language_menu_position: gpui::Point<Pixels>,
    /// 语言服务器控制菜单（状态栏 LSP chip 点击时创建）。
    ///
    /// 只服务**当前文件的那一个服务器**：chip 本身就是「这个文件的服务器」，
    /// 用户点它想知道的是「它怎么了、能不能重启」。对照 Zed 的
    /// `crates/language_tools/src/lsp_button.rs`——它在同一位置列出全部服务器，
    /// 那是 workspace 级状态栏的取舍，这里不需要。
    lsp_menu: Option<Entity<crate::editor::lsp_menu::LspMenu>>,
    _lsp_menu_subscription: Option<Subscription>,
    lsp_menu_position: gpui::Point<Pixels>,
    /// LSP 报文日志面板（View Logs）。
    lsp_log_view: Option<Entity<crate::editor::lsp_log_view::LspLogView>>,
    _lsp_log_view_subscription: Option<Subscription>,
    /// LSP 悬浮提示浮层状态。
    hover: hover::HoverState,
    /// LSP 补全菜单状态。
    completion: completion::CompletionState,
    /// LSP 重命名输入面板（F2）。
    rename_panel: Option<Entity<rename::RenamePanel>>,
    /// LSP 文档符号面板（cmd-shift-o）。
    symbols_panel: Option<Entity<symbols::SymbolsPanel>>,
    /// 跳转历史栈：每次跳转前压入「从哪来」，`NavigateBack` 弹栈回去。
    ///
    /// 对照 Zed：它在 workspace pane 上维护 `ItemNavHistory`。这里放在
    /// Editor 上，粒度是「编辑器内的位置」——足够覆盖「跳到定义后返回」，
    /// 不必引入 workspace 层的历史模型。
    nav_history: Vec<NavLocation>,
    /// 上次请求文档高亮时的光标位置（避免重复请求）。
    last_highlight_cursor: Option<usize>,
    /// 上次请求 Inlay Hint 的可见区间（避免重复请求）。
    last_inlay_range: Option<lsp_types::Range>,
    /// 上次请求签名帮助时的光标位置（避免重复请求）。
    last_signature_cursor: Option<usize>,
    /// 当前 hover 浮层里显示的是不是签名帮助。
    ///
    /// 签名帮助复用 hover 浮层，所以不能再用「浮层存在」判断是否该请求：
    /// 那个判断会把签名帮助自己挡住，光标在参数间移动时永远不刷新。
    signature_help_active: bool,
    /// 签名帮助写进浮层时的锚点，用来识别浮层是否已经被普通 hover 换掉。
    signature_hover_anchor: Option<usize>,
    /// 签名帮助请求代号：快速移动光标时只允许最新响应落地。
    signature_request_generation: u64,
    /// 上一次记进日志的 chip（文案 + 状态），`render_lsp_status_chip` 的去重状态。
    ///
    /// 键含文案与状态（不只是文案）：`NotStarted`/`Stopped`/`Ready` 文案相同，
    /// 只记文案就把「点了 Stop」和「起来了但有错」这两条抹平了。
    ///
    /// 存状态本身而不是拼好的键：`render` 每帧都跑，拼键就是每帧 3 次分配。
    ///
    /// 放在 Editor 上而不是 `thread_local!`：多个编辑器（多窗口、多工作区）
    /// 交替重绘时共享一份「上一条」会让文案每帧来回变化，去重形同不存在——
    /// 而它防的正是渲染路径逐帧打日志（实测一次会话 6.9 万行，把有用线索冲掉）。
    last_chip_log: std::cell::RefCell<Option<(String, crate::lsp::status::LspStatus)>>,
    /// 上一次记进日志的补全菜单可见性（同上，`drain_lsp_results` 用）。
    last_completion_visible_log: std::cell::Cell<bool>,
    /// 远端标签：项目根还在后台探测、因此暂时接不上 LSP 的那些。
    ///
    /// 接标签要先算 `ServerKey.root`，而远端的根只能在那台机器上探。这趟
    /// 往返若在 UI 线程上等，主机卡住就是 15 秒的界面冻结——所以先挂起，
    /// 探完由后台任务把对应的标签取回来重走 `start_lsp_for_tab`。
    pending_root_attach: Vec<String>,
    /// LSP 跳转打开的文件：目标路径 → **发起跳转的那个会话的根**。
    ///
    /// 键含主机（`(路径, host:port:user)`）：`/srv/app/main.py` 在两台主机上
    /// 是两个文件，各自的服务器也是两个。
    ///
    /// 为什么要有这张表：跳转的落点经常**不在当前项目里**——Python 点进
    /// `List[int]` 会落到 typeshed 的 `typing.pyi`，Rust 会落到 registry 里
    /// 的源。目标文件若按自己的路径重算根，就在存根目录边上另起了一个服务器
    /// （Python 侧 pyright 冷启动要索引整个 typeshed，用户体感是「跳一次卡
    /// 一次」，且多占一个进程）。这些文件属于**报告这个位置的服务器的工作区**，
    /// 所以把它的根传过去，二者复用同一个进程。
    ///
    /// 用表而不是函数参数：跨文件打开要经过 `open_*_at_line_col`
    /// （终端 Cmd+Click 也用它，那边没有会话概念），而远端文件还是异步加载的
    /// ——真正接 LSP 的时刻在 `finish_file_load` 里，中间没有传参的通道。
    lsp_jump_roots: HashMap<(String, Option<(String, u16, String)>), PathBuf>,
    next_load_generation: u64,
}

/// 光标是否处于「参数列表内」——即前面有未闭合的 `(`。
///
/// 只做括号配平的粗判：不解析字符串/注释里的括号。签名帮助是提示性的，
/// 偶尔多弹一次比引入一套词法分析划算。
///
/// **索引换算一律走 `RopeExt` 的既有原语**（`clip_offset` / `offset_to_char_index`
/// / `char_index_to_offset`）：它们做过边界吸附且有测试覆盖。手写
/// `offset - 1` 或直接把字节偏移喂给 `Rope::char` 会落在多字节字符内部而
/// panic——本项目大量中文注释，一撞就必现崩溃。
///
/// 回溯限长：签名帮助只关心最近的参数列表，不必扫到文件开头（大文件上
/// 每次渲染都全量回溯会拖慢输入）。
fn signature_trigger_for(text: &ropey::Rope, cursor: usize) -> bool {
    use crate::components::input::rope_ext::RopeExt as _;

    const MAX_SCAN_CHARS: usize = 4096;
    if text.len() == 0 {
        return false;
    }
    // 先吸附到字符边界（向左），越界偏移会被钳制。
    let upto = text.clip_offset(cursor.min(text.len()), sum_tree::Bias::Left);
    if upto == 0 {
        return false;
    }
    let mut ci = text.offset_to_char_index(upto);
    let floor = ci.saturating_sub(MAX_SCAN_CHARS);
    let mut depth = 0i32;
    while ci > floor {
        let byte_off = text.char_index_to_offset(ci - 1);
        match text.char(byte_off) {
            ')' => depth += 1,
            '(' => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            _ => {}
        }
        ci -= 1;
    }
    false
}

/// 光标前一个字符（`None` = 文本开头）。
///
/// **不要**把 `byte_to_char_idx` 的结果直接喂给 `Rope::char`：ropey 2.0 的
/// `char` 收的是**字节下标**，字符下标喂进去会落在多字节字符内部并 panic
/// （本项目大量中文/emoji，一撞就必现）。这里统一走 `RopeExt` 的换算原语。
fn char_before(text: &ropey::Rope, offset: usize) -> Option<char> {
    use crate::components::input::rope_ext::RopeExt as _;

    let byte = text.clip_offset(offset.min(text.len()), sum_tree::Bias::Left);
    let ci = text.offset_to_char_index(byte);
    (ci > 0).then(|| text.char(text.char_index_to_offset(ci - 1)))
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
            available_width: 0.0,
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
            pending_lsp_jumps: HashMap::new(),
            goto_line_panel: None,
            ex_command_panel: None,
            language_menu: None,
            _language_menu_subscription: None,
            language_menu_position: gpui::point(px(0.0), px(0.0)),
            lsp_menu: None,
            _lsp_menu_subscription: None,
            lsp_menu_position: gpui::point(px(0.0), px(0.0)),
            lsp_log_view: None,
            _lsp_log_view_subscription: None,
            hover: hover::HoverState::new(),
            completion: completion::CompletionState::new(),
            rename_panel: None,
            symbols_panel: None,
            nav_history: Vec::new(),
            last_highlight_cursor: None,
            last_inlay_range: None,
            last_signature_cursor: None,
            signature_help_active: false,
            signature_hover_anchor: None,
            signature_request_generation: 0,
            last_chip_log: std::cell::RefCell::new(None),
            last_completion_visible_log: std::cell::Cell::new(false),
            pending_root_attach: Vec::new(),
            lsp_jump_roots: HashMap::new(),
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
    ///
    /// 打开时默认宽度 = 可用宽度的一半（与终端区域平分），见
    /// [`Self::apply_open_split_width`]；恢复路径（`suppress_open_animation`）
    /// 不在此列，保留用户当前宽度。
    fn begin_open_animation(&mut self) {
        if self.suppress_open_animation {
            return;
        }
        self.apply_open_split_width();
        self.open_anim_epoch = self.open_anim_epoch.wrapping_add(1);
        self.panel_anim_epoch = self.panel_anim_epoch.wrapping_add(1);
        self.panel_anim_start = 0.0;
        self.panel_anim_end = self.width;
        self.panel_anim_ms = EDITOR_OPEN_ANIMATION_MS;
    }

    /// 首次打开文件时的默认宽度：与主区（终端）平分可用宽度。
    ///
    /// 只在空 → 非空的开合路径生效：打开后用户拖拽的宽度、恢复（切标签页组/
    /// 文件树）保留的宽度都不受影响。可用宽度未知（未下发布局，如单测）时
    /// 保留现有宽度。
    fn apply_open_split_width(&mut self) {
        if self.available_width <= 0.0 {
            return;
        }
        let max_w = self.window_follow_limit.max(EDITOR_MIN_WIDTH);
        self.width =
            (self.available_width * EDITOR_OPEN_SPLIT_RATIO).clamp(EDITOR_MIN_WIDTH, max_w);
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
    ///
    /// `dirty` 必须**当场从实体读**（diff tab 的编辑状态不在 `EditorFile.dirty`
    /// 上：那个字段由 `InputState` 的文本变化驱动，而 diff tab 的占位
    /// `InputState` 永远是空的）。淘汰保护只看这份快照，读错就等于丢编辑。
    pub fn get_open_files_info(&self, cx: &App) -> Vec<EditorOpenFileInfo> {
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
                dirty: match f.diff() {
                    Some(diff) => diff.read(cx).is_dirty(),
                    None => f.dirty,
                },
                diff_repo: f.diff_repo().cloned(),
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
            if !file.is_loading && file.image().is_none() && file.diff().is_none() {
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
            if let Some(diff) = file.diff().cloned() {
                cached.insert(file.id.clone(), diff);
            }
        }
        cached
    }

    /// 取出各标签的 LSP 会话，用于标签页组/文件树切换时随状态一起换手。
    ///
    /// 必须在 `clear(cx)` **之前**调用：`EditorFile` 一旦被析构，`LspSession`
    /// 的 `Drop` 就会发 `didClose`，服务器此后不再持有这些文档；而进程本身
    /// 仍活在 `LspManager` 的表里。会话原样搬进 `FileTreeEditorState`，切回来
    /// 时再放回 `EditorFile`——文档仍开着，hover/跳转无需重连。
    fn take_cached_lsp_sessions(&mut self) -> LspSessionMap {
        let mut sessions = HashMap::new();
        for file in &mut self.open_files {
            if let Some(session) = file.lsp.take() {
                sessions.insert(file.id.clone(), session);
            }
        }
        sessions
    }

    /// 保存当前编辑器状态到文件树状态
    pub fn save_to_file_tree_state(&mut self, cx: &mut Context<Self>) -> FileTreeEditorState {
        // 切标签页组会缓存影子 InputState：先把防抖中的 live 文本落盘，
        // 否则切回来会看到滞后的内容。
        self.flush_all_live_shadows(cx);
        let open_files_info = self.get_open_files_info(cx);
        let cached_editor_states = self.take_cached_editor_states();
        let cached_diff_views = self.take_cached_diff_views();
        // LSP 会话同样必须在 `clear` 之前摘出来：`clear` 释放 `EditorFile`，
        // 会话的 `Drop` 会补发 `didClose`，切回来就只剩一个“进程还在、没人
        // 连得上”的空壳（hover/跳转/补全静默失效）。
        let cached_lsp_sessions = self.take_cached_lsp_sessions();
        let active_file_id = self.active_file_id.clone();

        // 清空编辑器
        self.clear(cx);

        FileTreeEditorState {
            open_files_info,
            cached_editor_states,
            cached_diff_views,
            cached_lsp_sessions,
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
        let FileTreeEditorState {
            open_files_info,
            cached_editor_states,
            cached_diff_views,
            mut cached_lsp_sessions,
            active_file_id,
        } = state;
        self.restore_from_info(
            open_files_info,
            active_file_id,
            RestoreBuffers {
                states: &cached_editor_states,
                diffs: &cached_diff_views,
                lsp_sessions: &mut cached_lsp_sessions,
            },
            window,
            cx,
        );
    }

    /// 从缓存恢复文件列表。
    ///
    /// `buffers.lsp_sessions` 按 tab id 一一对应：能原样放回的会话由
    /// `restore_file_from_cache_with_session` 取走；没被接走的
    /// （加载中的文件、图片、diff、切换期间被删的 tab）在本函数末尾统一
    /// drop，`Drop` 会补发 `didClose`，服务器侧不会留下僵尸文档。
    fn restore_from_info(
        &mut self,
        files_info: Vec<EditorOpenFileInfo>,
        active_file_id: Option<SharedString>,
        buffers: RestoreBuffers<'_>,
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
            if let Some(cached_diff) = buffers.diffs.get(&file_info.id) {
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
                self.open_diff_file_loading(
                    file_info.path.clone(),
                    diff_repo.clone(),
                    false,
                    window,
                    cx,
                );
                continue;
            }
            // 兼容旧缓存：id 是 `diff-` 前缀但没有仓库快照时跳过建空白 tab，
            // 避免切组回来留下一个空白的只读编辑器。
            if file_info.id.as_ref().starts_with("diff-") {
                log::warn!(
                    "skip restoring diff tab without repo snapshot: {}",
                    file_info.id.as_ref()
                );
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
                        OpenOptions {
                            mode: OpenMode::Preview,
                            silent: true,
                        },
                    );
                } else {
                    self.open_local_file_inner(
                        std::path::PathBuf::from(&file_info.path),
                        window,
                        cx,
                        OpenOptions {
                            mode: OpenMode::Preview,
                            silent: true,
                        },
                    );
                }
                continue;
            }

            // 检查是否有缓存的 InputState（远程文件）
            if let Some(cached_state) = buffers.states.get(&file_info.id) {
                // 直接使用缓存的 InputState，并把切走前的 LSP 会话一起放回。
                match buffers.lsp_sessions.remove(&file_info.id) {
                    Some(lsp_session) => self.restore_file_from_cache_with_session(
                        file_info,
                        cached_state.clone(),
                        Some(lsp_session),
                        window,
                        cx,
                    ),
                    // 切走前就没有会话（LSP 关闭 / 用户 Stop / 无服务器）：
                    // 保持没有，不在这里擅自拉起服务器。
                    None => {
                        self.restore_file_from_cache(file_info, cached_state.clone(), window, cx)
                    }
                }
            } else {
                // 本地文件或正在加载的文件，重新打开。
                // 旧会话的编辑器句柄指向即将被替换的 InputState，不能复用；
                // 留在表里由本函数末尾统一 drop（补发 didClose），加载完成
                // 后由 `finish_file_load` 重新接服务器。
                if let Some(ref ssh_config) = file_info.ssh_config {
                    self.open_remote_file_inner(
                        file_info.path.clone(),
                        ssh_config.clone(),
                        window,
                        cx,
                        OpenOptions {
                            mode: OpenMode::Preview,
                            silent: true,
                        },
                    );
                } else {
                    self.open_local_file_inner(
                        std::path::PathBuf::from(&file_info.path),
                        window,
                        cx,
                        OpenOptions {
                            mode: OpenMode::Preview,
                            silent: true,
                        },
                    );
                }
            }
        }
        // 没有任何标签接走的会话：图片/diff/加载中的 tab、切换期间被删的文件。
        // 统一在这里释放，`Drop` 会补发 `didClose`，服务器侧不留僵尸文档。
        buffers.lsp_sessions.clear();
        if let Some(ref active_id) = active_file_id {
            self.switch_to_file(active_id, cx);
        }
        self.suppress_open_animation = false;
    }

    /// 从缓存恢复单个文件（测试与不需要会话恢复的调用方使用）。
    fn restore_file_from_cache(
        &mut self,
        file_info: &EditorOpenFileInfo,
        editor_state: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_file_from_cache_with_session(file_info, editor_state, None, window, cx);
    }

    /// 从缓存恢复单个文件，并尝试接回切走前的 LSP 会话。
    ///
    /// 只有**仍然连着服务器**的会话能原样放回。已经断开的（服务器崩溃、
    /// 用户 Stop、握手未完成就被切走）不能复活：它的 `client` 要么已关闭、
    /// 要么从未就绪，`active_lsp_session()` 拿到它只会让 hover/跳转继续
    /// 静默失败。丢弃后走一次正常接回路径（`start_lsp_for_tab`），由它按
    /// 「用户是否 Stop / 语言是否禁用 / 远端根是否已知」重新决策。
    fn restore_file_from_cache_with_session(
        &mut self,
        file_info: &EditorOpenFileInfo,
        editor_state: Entity<InputState>,
        lsp_session: Option<crate::lsp::session::LspSession>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 复用的 InputState 与切走前是同一个实体，会话里的编辑器句柄仍然有效。
        let (lsp, needs_reattach) = match lsp_session {
            Some(session) if session.is_active() => (Some(session), false),
            Some(session) => {
                log::info!(
                    "lsp: session for {} is not connected after restore; reconnecting",
                    file_info.path
                );
                // drop 会补发 didClose；没 didOpen 过时是无害的空操作。
                drop(session);
                (None, true)
            }
            None => (None, false),
        };
        if needs_reattach {
            // 旧会话留下的 Ready 状态与诊断已经没有来源，且新会话的 didOpen
            // 从 version 0 重新计数；不清理会让状态栏显示一个假 Ready，诊断
            // 版本门控也会把新服务器推来的诊断全判为过期。
            editor_state.update(cx, |state, _| {
                state.reset_lsp_diagnostics();
                state.set_lsp_status(None);
            });
        }
        let path_str = file_info.path.clone();
        let label = file_info.label.clone();
        let ssh_config = file_info.ssh_config.clone();

        let content = editor_state.read(cx).value().to_string();
        let original_content = file_info
            .original_content
            .clone()
            .unwrap_or_else(|| content.clone());
        let is_live = file_info.id.as_ref().ends_with(Self::MARKDOWN_LIVE_SUFFIX);
        // live 标签的影子文本已按模型口径剥掉 `\r`，与保留原始行尾的基线
        // 比较必须同口径，否则 CRLF 文件恢复回来就会被误标为「已修改」。
        let is_dirty = !content_matches_baseline(&content, &original_content, is_live);
        let is_markdown = path_str.ends_with(".md")
            && !file_info.id.as_ref().ends_with(Self::MARKDOWN_EDIT_SUFFIX)
            && !is_live;
        let markdown = if is_markdown {
            // 相对图片路径以文档所在目录为基准（远程文件的路径不在本机，
            // 不拿它当本地目录用）。
            let md_doc_dir = if ssh_config.is_none() {
                std::path::Path::new(&path_str)
                    .parent()
                    .map(std::path::Path::to_path_buf)
            } else {
                None
            };
            Some(cx.new(|cx| {
                crate::editor::markdown::Markdown::new_with_dir(content.clone(), md_doc_dir, cx)
            }))
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
                    move |this,
                          _live_entity,
                          event: &crate::editor::markdown_live::LiveViewEvent,
                          cx| {
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
                        this.on_live_change(&live_tab_id.clone(), *version, cx);
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
            content: if let Some(markdown) = markdown {
                TabContent::MarkdownPreview(markdown)
            } else if let Some(live) = live {
                TabContent::Live(live)
            } else {
                TabContent::Code
            },
            lsp,
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

        // 会话在切走期间断了：像正常打开一样重新接一次。`start_lsp_for_tab`
        // 会尊重用户 Stop / 语言禁用 / 远端根未探到等既有门控；失败时只记
        // 日志并退回「无 LSP」，不会阻塞恢复。
        if needs_reattach {
            self.start_lsp_for_tab(&file_info.id, cx);
        }

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

        // 通知语言服务器「这个文件在磁盘上变了」。
        //
        // 编辑器之外的工具（`cargo add`、`git checkout`、另一个编辑器）改动
        // 文件时服务器不会自动知道，它的分析会基于过期内容。对照 Zed：它订阅
        // 文件系统事件后发 `workspace/didChangeWatchedFiles`。
        //
        // 本地与远端都发，但**事件的来源必须匹配当前会话的归属**：远程事件
        // 推给本地会话（或反过来）时，那个 uri 在服务器眼里根本不存在，
        // 属于无意义的噪声；两台主机有同名路径时更是错配。
        // `is_from_backend(session.ssh_config())` 同时承担了「匹配主机」与
        // 「匹配本地/远程」两件事。
        let backend = self
            .active_lsp_session()
            .and_then(|s| s.ssh_config().cloned());
        if event.is_from_backend(backend.as_ref())
            && let Some(path) = crate::lsp::client::path_to_uri(&event.path).ok()
            && let Some(session) = self.active_lsp_session()
            && let Some(client) = session.client().cloned()
            // **路径过滤**：服务器只关心它注册过的 glob（服务器没注册过时
            // 退回排除 `target/` 等构建产物目录）。缺这一步会形成自持循环 ——
            // rust-analyzer 自己在跑的 `cargo check` 持续写 `target/`，我们把
            // 每次写入都推过去，它就重载工程、再跑 cargo、再写……实测 19 分钟
            // 重载 401 次，期间所有请求都被回 `-32801 content modified`，
            // 表现为「补全不出现、hover 弹几次就没了、状态栏反复变加载中」。
            && client.should_notify_watched_file(&event.path)
        {
            let change_type = match event.kind {
                FsChangeKind::Create => lsp_types::FileChangeType::CREATED,
                FsChangeKind::Modify => lsp_types::FileChangeType::CHANGED,
                FsChangeKind::Remove => lsp_types::FileChangeType::DELETED,
                _ => lsp_types::FileChangeType::CHANGED,
            };
            let changes = vec![lsp_types::FileEvent {
                uri: path,
                typ: change_type,
            }];
            smol::spawn(async move {
                if let Err(e) = client.did_change_watched_files(changes).await {
                    log::debug!("lsp: didChangeWatchedFiles failed: {e}");
                }
            })
            .detach();
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
            let is_image = file.image().is_some();
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
                                if let Some(ref viewer) = file.image() {
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
                            // 与打开路径同规：先 stat 粗筛。磁盘上持续增长的
                            // 大文件（如日志）在外部变更重载时若直接全量读，
                            // 会把整个文件拉进内存（打开时的 20MB 限制
                            // 管不到这条重载路径）。
                            if let Ok(meta) =
                                crate::ssh::sftp_fs::metadata(&config, &path_for_task)
                                && meta.size > MAX_EDITOR_FILE_BYTES as u64
                            {
                                return Err(format!(
                                    "文件过大（{} MB），超过 {MAX_EDITOR_FILE_MB} MB 限制，已跳过重载",
                                    meta.size / (1024 * 1024)
                                ));
                            }
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
                            // 同远端分支：先 metadata 粗筛，超限跳过重载。
                            if let Ok(meta) = std::fs::metadata(&path_for_task)
                                && meta.len() > MAX_EDITOR_FILE_BYTES as u64
                            {
                                return Err(format!(
                                    "文件过大（{} MB），超过 {MAX_EDITOR_FILE_MB} MB 限制，已跳过重载",
                                    meta.len() / (1024 * 1024)
                                ));
                            }
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
            // live 标签的影子文本是归一化（剥 `\r`）的，而磁盘内容与基线保留
            // 原始行尾：比较必须同口径，否则 CRLF 文件在没有本地修改时也会被
            // 误判成「有本地修改」→ 明明只是外部保存，却弹冲突确认。
            let is_live = self.open_files[file_ix].live().is_some();
            let no_local_edits = content_matches_baseline(
                &current,
                &self.open_files[file_ix].original_content,
                is_live,
            );
            let disk_matches_editor = content_matches_baseline(&content, &current, is_live);

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
                    if let Some(ref markdown) = self.open_files[file_ix].markdown() {
                        markdown.update(cx, |renderer, cx| {
                            renderer.set_text(content.clone(), cx);
                        });
                    }
                    if let Some(ref live) = self.open_files[file_ix].live() {
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
                        if let Some(ref markdown) = this.open_files[file_ix].markdown() {
                            markdown.update(cx, |renderer, cx| {
                                renderer.set_text(prompt_content.clone(), cx);
                            });
                        }
                        if let Some(ref live) = this.open_files[file_ix].live() {
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
        let fs: Arc<dyn crate::file_manager::FileSystem> = Arc::new(
            crate::file_manager::filesystem::RemoteFileSystem::new(ssh_config.clone()),
        );
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
        // 标签全部销毁前收起补全：`CompletionState` 是 Editor 级别的，
        // 不清会留下一个指向已释放 InputState 的可见菜单。
        if let Some(editor) = self.active_text_editor(cx) {
            self.completion.hide_in(&editor, cx);
        } else {
            self.completion.hide();
        }
        self.unwatch_all_files(cx);
        self.open_files.clear();
        self.active_file_id = None;
        self.loading_gens.clear();
        self.editor_state_subscriptions.clear();
        // 挂起表里存的是 tab id，标签都没了就没有意义。清掉后，重新打开的
        // 同一路径（id 相同）会自己发起探测，而不是命中 `start_lsp_for_tab`
        // 的「已有探测在飞」早退、把接入托付给一次属于旧标签的回调。
        // 根探测已按目录去重（见 `remote_project_root`），重复发起不会多打一趟。
        self.pending_root_attach.clear();
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
                self.width =
                    (self.width + dw * EDITOR_WINDOW_FOLLOW_RATIO).clamp(EDITOR_MIN_WIDTH, max_w);
            }
        }
        self.last_window_width = win_w;
    }

    /// 由 AppWindow 每帧下发窗口跟随的宽度上限（窗口宽 − 侧边栏占位 −
    /// 主区最小宽）。仅用于钳制窗口跟随，不影响拖拽/最小宽度语义。
    pub fn set_window_follow_limit(&mut self, limit: f32) {
        self.window_follow_limit = limit;
    }

    /// 由 AppWindow 每帧下发「可用宽度」（窗口宽 − 侧边栏占位）。
    /// 仅用于首次打开文件时的默认宽度（与终端区域平分），见
    /// [`Self::apply_open_split_width`]。
    pub fn set_available_width(&mut self, available: f32) {
        self.available_width = available;
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

    /// 切换 Vim 模式。与 Zed 的 `vim::ToggleVimMode` 一致：状态栏徽章可点，
    /// 键位也可绑定 `editor_panel::ToggleVim`。Linux 默认关闭，但用户不必手改
    /// settings.json。
    fn toggle_vim(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if cx.try_global::<crate::settings::SettingsStore>().is_none() {
            return;
        }
        let enabled = crate::vim::enabled(cx);
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = !enabled;
        });
        // `settings::observe` 在正常启动路径也会做这两件事；这里显式调用是为了
        // 组件测试/嵌入式渲染没有注册 observe 时也能立刻生效。
        crate::keymap::reload(cx);
        crate::vim::disable_all(cx);
        cx.notify();
    }

    /// `:`：打开 Vim Ex 命令行。
    fn open_ex_command(&mut self, _: &ExCommandLine, window: &mut Window, cx: &mut Context<Self>) {
        if self.ex_command_panel.take().is_some() {
            // 理论上焦点在命令行里时 `:` 会输入字符而不是再次派发 action；
            // 保留这个兜底，避免状态卡住。
            if let Some(editor) = self.active_text_editor(cx) {
                editor.read(cx).focus_handle.clone().focus(window, cx);
            }
            cx.notify();
            return;
        }
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        let visual = editor.read(cx).vim.as_ref().is_some_and(|vim| {
            matches!(
                vim.mode,
                crate::vim::VimMode::Visual | crate::vim::VimMode::VisualLine
            )
        });
        // `2:` 这类前缀不应把计数留到命令执行后的下一次移动。
        editor.update(cx, |state, cx| {
            if let Some(vim) = state.vim.as_mut() {
                vim.clear_pending();
            }
            cx.notify();
        });
        self.completion.hide_in(&editor, cx);
        let panel = ex_command::ExCommandPanel::new(cx.entity().downgrade(), window, cx);
        if visual {
            panel.update(cx, |panel, cx| panel.set_input("'<,'>", window, cx));
        }
        panel.update(cx, |panel, cx| panel.focus(window, cx));
        self.ex_command_panel = Some(panel);
        cx.notify();
    }

    /// 关闭 `:` 面板并把焦点还给编辑区。
    pub(crate) fn close_ex_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ex_command_panel.take();
        if let Some(editor) = self.active_text_editor(cx) {
            // 从 Visual 进入 `:` 时，命令执行/取消后都应回到 Normal。
            editor.update(cx, |state, cx| {
                if let Some(vim) = state.vim.as_ref() {
                    let was_visual = matches!(
                        vim.mode,
                        crate::vim::VimMode::Visual | crate::vim::VimMode::VisualLine
                    );
                    if was_visual {
                        let cursor = state.cursor();
                        state.selected_range = (cursor..cursor).into();
                    }
                }
                if state.vim.is_some() {
                    state.vim_switch_mode(crate::vim::VimMode::Normal, cx);
                }
            });
            editor.read(cx).focus_handle.clone().focus(window, cx);
        }
        cx.notify();
    }

    /// 执行 `:` 命令。返回 `Close` 时面板关闭，`Message` 时保留面板显示结果。
    pub(crate) fn execute_ex_command(
        &mut self,
        input: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ex_command::ExOutcome, String> {
        let command = ex_command::parse(input)?;
        self.execute_parsed_command(command, None, window, cx)
    }

    fn execute_parsed_command(
        &mut self,
        mut command: ex_command::ExCommand,
        forced_row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ex_command::ExOutcome, String> {
        if let Some(row) = forced_row {
            command.range = Some((
                ex_command::LineExpr::Number(row.saturating_add(1)),
                ex_command::LineExpr::Number(row.saturating_add(1)),
            ));
        }
        let Some(editor) = self.active_text_editor(cx) else {
            return Err("当前没有可编辑的文本标签".into());
        };

        if command.is_goto() {
            let Some((expr, _)) = command.range else {
                return Ok(ex_command::ExOutcome::Close);
            };
            let row = self.ex_resolve_line(expr, false, &editor, cx)?;
            ex_goto_row(&editor, row, cx);
            return Ok(ex_command::ExOutcome::Close);
        }

        match command.name.as_str() {
            "w" => {
                self.ex_save_active(cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "wq" | "x" => {
                self.ex_save_and_close(cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "q" => {
                self.ex_close_active(command.bang, window, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "e" => {
                self.ex_reload_active(command.bang, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "bn" | "tabn" => {
                self.ex_cycle_tab(1, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "bp" | "tabp" => {
                self.ex_cycle_tab(-1, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "noh" => {
                self.ex_clear_search(window, cx);
                Ok(ex_command::ExOutcome::Close)
            }
            "d" => {
                let (start, end) = self.ex_default_range(&command, &editor, true, false, cx)?;
                self.ex_delete_lines(&editor, start, end, window, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "y" => {
                let (start, end) = self.ex_default_range(&command, &editor, true, false, cx)?;
                self.ex_yank_lines(&editor, start, end, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "j" => {
                let (start, end) = self.ex_default_range(&command, &editor, true, true, cx)?;
                self.ex_join_lines(&editor, start, end, window, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "s" => {
                let (start, end) = self.ex_default_range(&command, &editor, true, false, cx)?;
                self.ex_substitute(&editor, start, end, &command.args, window, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "sort" => {
                let (start, end) = self.ex_default_range(&command, &editor, false, false, cx)?;
                self.ex_sort_lines(&editor, start, end, window, cx)?;
                Ok(ex_command::ExOutcome::Close)
            }
            "normal" | "norm" => {
                let keys = command.args.trim().to_string();
                if keys.is_empty() {
                    return Err(":normal 缺少要执行的按键".into());
                }
                let rows = if command.range.is_some() {
                    let (start, end) = self.ex_default_range(&command, &editor, true, false, cx)?;
                    Some((start..=end).collect())
                } else {
                    None
                };
                Ok(ex_command::ExOutcome::Normal { keys, rows })
            }
            "g" | "v" | "global" => self.ex_global(&command, &editor, window, cx),
            "!" => self.ex_shell(&command, &editor, window, cx),
            "set" => self.ex_set(&command.args, window, cx),
            "marks" => Ok(ex_command::ExOutcome::Message(
                self.ex_marks_message(&editor, cx),
            )),
            "registers" => Ok(ex_command::ExOutcome::Message(ex_registers_message(cx))),
            "" => Ok(ex_command::ExOutcome::Close),
            other => Err(format!("未实现的命令：:{other}")),
        }
    }

    // ==================== Ex 命令执行辅助 ====================

    fn ex_resolve_line(
        &self,
        expr: ex_command::LineExpr,
        end: bool,
        editor: &Entity<InputState>,
        cx: &App,
    ) -> Result<usize, String> {
        let state = editor.read(cx);
        let text = state.text();
        let last = text.lines_len().saturating_sub(1);
        let row = match expr {
            ex_command::LineExpr::Current => text.offset_to_point(state.cursor()).row,
            ex_command::LineExpr::Last => last,
            ex_command::LineExpr::Number(number) => number.saturating_sub(1),
            ex_command::LineExpr::Mark('<') | ex_command::LineExpr::Mark('>') => {
                // Visual 命令 `:'<,'>`：取当前可视选区两端所在行。
                // 面板打开不会清掉 InputState 的选区，所以执行时仍可读到。
                let start_row = text
                    .offset_to_point(state.selected_range.start.min(text.len()))
                    .row;
                let mut end_row = text
                    .offset_to_point(state.selected_range.end.min(text.len()))
                    .row;
                if end_row > start_row
                    && text.line_start_offset(end_row) == state.selected_range.end
                {
                    end_row = end_row.saturating_sub(1);
                }
                if matches!(expr, ex_command::LineExpr::Mark('<')) {
                    start_row
                } else {
                    end_row
                }
            }
            ex_command::LineExpr::Mark(name) => {
                let offset = crate::vim::Marks::global(cx)
                    .get(name)
                    .ok_or_else(|| format!("标记 {name} 未设置"))?;
                text.offset_to_point(offset.min(text.len())).row
            }
            ex_command::LineExpr::All => {
                if end {
                    last
                } else {
                    0
                }
            }
        };
        Ok(row.min(last))
    }

    fn ex_default_range(
        &self,
        command: &ex_command::ExCommand,
        editor: &Entity<InputState>,
        default_current: bool,
        default_join: bool,
        cx: &App,
    ) -> Result<(usize, usize), String> {
        let (start, end) = if let Some((start_expr, end_expr)) = command.range {
            (
                self.ex_resolve_line(start_expr, false, editor, cx)?,
                self.ex_resolve_line(end_expr, true, editor, cx)?,
            )
        } else if default_join {
            let state = editor.read(cx);
            let current = state.text().offset_to_point(state.cursor()).row;
            (current, current.saturating_add(1))
        } else if default_current {
            let state = editor.read(cx);
            let current = state.text().offset_to_point(state.cursor()).row;
            (current, current)
        } else {
            let state = editor.read(cx);
            (0, state.text().lines_len().saturating_sub(1))
        };
        let last = editor.read(cx).text().lines_len().saturating_sub(1);
        let start = start.min(last);
        let end = end.min(last);
        if start > end {
            return Err("范围起点在终点之后".into());
        }
        Ok((start, end))
    }

    fn ex_save_active(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(id) = self.active_file_id.clone() else {
            return Err("当前没有打开的文件".into());
        };
        let Some(file) = self.open_files.iter().find(|f| f.id == id) else {
            return Err("当前文件不存在".into());
        };
        if file.read_only {
            return Err("文件是只读的".into());
        }
        self.save_active_file(cx);
        Ok(())
    }

    fn ex_save_and_close(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(id) = self.active_file_id.clone() else {
            return Err("当前没有打开的文件".into());
        };
        let Some(index) = self
            .open_files
            .iter()
            .position(|f| f.id.as_ref() == id.as_ref())
        else {
            return Err("当前文件不存在".into());
        };
        if self.open_files[index].read_only {
            return Err("文件是只读的".into());
        }
        if self.file_is_dirty(&self.open_files[index], cx) {
            self.save_file_by_id(&id, cx, true);
        } else {
            self.close_file_direct(&id, cx);
        }
        Ok(())
    }

    fn ex_close_active(
        &mut self,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.active_file_id.clone() else {
            return Err("当前没有打开的文件".into());
        };
        if force {
            self.close_file_direct(&id, cx);
        } else {
            self.close_file(&id, window, cx);
        }
        Ok(())
    }

    fn ex_cycle_tab(&mut self, delta: isize, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(active) = self.active_file_id.clone() else {
            return Err("当前没有打开的文件".into());
        };
        let len = self.open_files.len();
        if len < 2 {
            return Ok(());
        }
        let Some(index) = self.open_files.iter().position(|f| f.id == active) else {
            return Err("当前文件不存在".into());
        };
        let next = (index as isize + delta).rem_euclid(len as isize) as usize;
        let id = self.open_files[next].id.to_string();
        self.switch_to_file(&id, cx);
        Ok(())
    }

    fn ex_reload_active(&mut self, force: bool, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(id) = self.active_file_id.clone() else {
            return Err("当前没有打开的文件".into());
        };
        let Some(index) = self
            .open_files
            .iter()
            .position(|f| f.id.as_ref() == id.as_ref())
        else {
            return Err("当前文件不存在".into());
        };
        if !force && self.file_is_dirty(&self.open_files[index], cx) {
            return Err("有未保存的修改，使用 :e! 强制重新加载".into());
        }
        let path = self.open_files[index].path.clone();
        let ssh_config = self.open_files[index].ssh_config.clone();
        let task = cx.background_spawn(async move {
            let bytes = if let Some(config) = ssh_config.as_ref() {
                crate::ssh::sftp_fs::read_file_bytes(config, &path)
                    .map_err(|e| format!("读取远程文件失败: {e}"))?
                    .ok_or_else(|| "文件不存在".to_string())?
            } else {
                std::fs::read(&path).map_err(|e| format!("读取文件失败: {e}"))?
            };
            let decoded = encoding::decode_bytes(&bytes);
            Ok::<_, String>((decoded.text, decoded.encoding))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok((content, encoding)) => {
                    this.apply_reloaded_content(id.as_ref(), content, encoding, cx);
                }
                Err(error) => {
                    this.show_notification(error, NotificationKind::Warning, cx);
                }
            })
            .ok();
        })
        .detach();
        Ok(())
    }

    fn apply_reloaded_content(
        &mut self,
        id: &str,
        content: String,
        encoding: crate::editor::encoding::FileEncoding,
        cx: &mut Context<Self>,
    ) {
        self.flush_all_live_shadows(cx);
        let Some(file) = self.open_files.iter().find(|f| f.id.as_ref() == id) else {
            return;
        };
        let path = file.path.clone();
        let ids: Vec<SharedString> = self
            .open_files
            .iter()
            .filter(|f| f.path == path)
            .map(|f| f.id.clone())
            .collect();
        for id in ids {
            let Some(file) = self.open_files.iter_mut().find(|f| f.id == id) else {
                continue;
            };
            file.original_content = content.clone();
            file.encoding = encoding;
            file.dirty = false;
            file.pending_reload = false;
            let tab_id = id.clone();
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.modify_tab(tab_id.as_ref(), |tab| tab.is_modified = false, cx);
            });
            let editor_state = file.editor_state.clone();
            editor_state.update(cx, |state, cx| {
                state.set_text(content.clone(), cx);
            });
            if let Some(markdown) = file.markdown().cloned() {
                markdown.update(cx, |renderer, cx| renderer.set_text(content.clone(), cx));
            }
            if let Some(live) = file.live().cloned() {
                live.update(cx, |view, cx| view.set_text(&content, cx));
            }
        }
        self.show_notification(format!("已重新加载 {path}"), NotificationKind::Success, cx);
        cx.notify();
    }

    fn ex_delete_lines(
        &mut self,
        editor: &Entity<InputState>,
        start: usize,
        end: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (range, yanked) = {
            let state = editor.read(cx);
            let range = ex_line_byte_range(state.text(), start, end);
            let yanked = state.text().slice(range.clone()).to_string();
            (range, yanked)
        };
        cx.update_global::<crate::vim::Registers, _>(|registers, cx| {
            registers.record_delete(&yanked, true, None, cx);
        });
        ex_replace_range(editor, range.clone(), "", range.start, window, cx);
        Ok(())
    }

    fn ex_yank_lines(
        &mut self,
        editor: &Entity<InputState>,
        start: usize,
        end: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (range, yanked) = {
            let state = editor.read(cx);
            let range = ex_line_byte_range(state.text(), start, end);
            let yanked = state.text().slice(range.clone()).to_string();
            (range, yanked)
        };
        cx.update_global::<crate::vim::Registers, _>(|registers, cx| {
            registers.record_yank(&yanked, None, cx);
        });
        editor.update(cx, |state, cx| {
            let pos = range.start.min(state.text().len());
            state.selected_range = (pos..pos).into();
            state.scroll_to_center(pos, cx);
            cx.notify();
        });
        Ok(())
    }

    fn ex_join_lines(
        &mut self,
        editor: &Entity<InputState>,
        start: usize,
        mut end: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let state = editor.read(cx);
        let text = state.text();
        let last = text.lines_len().saturating_sub(1);
        if start == end {
            if start >= last {
                return Err("没有下一行可连接".into());
            }
            end = start + 1;
        }
        let rows: Vec<String> = (start..=end)
            .map(|row| text.line(row, ropey::LineType::LF).to_string())
            .collect();
        let range = ex_line_byte_range(text, start, end);
        let trailing_newline = text.slice(range.clone()).to_string().ends_with('\n');
        let separator = if text.to_string().contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut joined = String::new();
        for (index, line) in rows.iter().enumerate() {
            let content = line.trim_end_matches(['\r', '\n']);
            if index == 0 {
                joined.push_str(content);
            } else {
                let content = content.trim_start();
                if !content.is_empty() {
                    if !joined.ends_with(' ') {
                        joined.push(' ');
                    }
                    joined.push_str(content);
                }
            }
        }
        if trailing_newline {
            joined.push_str(separator);
        }
        ex_replace_range(editor, range.clone(), &joined, range.start, window, cx);
        Ok(())
    }

    fn ex_sort_lines(
        &mut self,
        editor: &Entity<InputState>,
        start: usize,
        end: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (range, mut rows, trailing_newline, separator) = {
            let state = editor.read(cx);
            let text = state.text();
            let rows: Vec<String> = (start..=end)
                .map(|row| {
                    text.line(row, ropey::LineType::LF)
                        .to_string()
                        .trim_end_matches(['\r', '\n'])
                        .to_string()
                })
                .collect();
            let range = ex_line_byte_range(text, start, end);
            let trailing = text.slice(range.clone()).to_string().ends_with('\n');
            let separator = if text.to_string().contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            (range, rows, trailing, separator)
        };
        rows.sort();
        let mut sorted = rows.join(separator);
        if trailing_newline {
            sorted.push_str(separator);
        }
        ex_replace_range(editor, range.clone(), &sorted, range.start, window, cx);
        Ok(())
    }

    fn ex_substitute(
        &mut self,
        editor: &Entity<InputState>,
        start: usize,
        end: usize,
        args: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (pattern, replacement, flags) = ex_command::parse_substitute(args)?;
        let state = editor.read(cx);
        let case_insensitive =
            flags.contains('i') || state.vim.as_ref().is_some_and(|vim| vim.ignore_case);
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| format!("正则无效: {e}"))?;
        let replacement = vim_replacement_to_regex(&replacement);
        let text = state.text();
        let mut rows = Vec::new();
        let mut replaced_any = false;
        for row in start..=end {
            let line = text.line(row, ropey::LineType::LF).to_string();
            let ending = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            let content = line.trim_end_matches(['\r', '\n']);
            let replaced = if flags.contains('g') {
                regex.replace_all(content, replacement.as_str()).to_string()
            } else {
                regex.replace(content, replacement.as_str()).to_string()
            };
            if replaced != content {
                replaced_any = true;
            }
            rows.push((replaced, ending.to_string()));
        }
        if !replaced_any {
            return Err("E486: 未找到匹配".into());
        }
        let mut output = String::new();
        for (content, ending) in rows {
            output.push_str(&content);
            output.push_str(&ending);
        }
        let range = ex_line_byte_range(text, start, end);
        ex_replace_range(editor, range.clone(), &output, range.start, window, cx);
        Ok(())
    }

    fn ex_global(
        &mut self,
        command: &ex_command::ExCommand,
        editor: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ex_command::ExOutcome, String> {
        let (pattern, inner_text) = ex_command::parse_global(&command.args)?;
        let state = editor.read(cx);
        let text = state.text();
        let last = text.lines_len().saturating_sub(1);
        let (start, end) = if command.range.is_some() {
            self.ex_default_range(command, editor, false, false, cx)?
        } else {
            (0, last)
        };
        let case_insensitive = state.vim.as_ref().is_some_and(|vim| vim.ignore_case);
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| format!("正则无效: {e}"))?;
        let invert = command.name == "v" || command.bang;
        let rows: Vec<usize> = (start..=end)
            .filter(|row| {
                let line = text.line(*row, ropey::LineType::LF).to_string();
                let line = line.trim_end_matches(['\r', '\n']);
                regex.is_match(line) != invert
            })
            .collect();
        if rows.is_empty() {
            return Ok(ex_command::ExOutcome::Message("Pattern not found".into()));
        }

        let mut inner = ex_command::parse(&inner_text)?;
        if inner.range.is_some() {
            return Err(":g/:v 的内层命令不能再带范围".into());
        }
        if inner.name == "normal" || inner.name == "norm" {
            if inner.args.trim().is_empty() {
                return Err(":g/:v 的 :normal 缺少按键".into());
            }
            return Ok(ex_command::ExOutcome::Normal {
                keys: inner.args.trim().to_string(),
                rows: Some(rows),
            });
        }
        if inner.name == "y" {
            let mut yanked = String::new();
            for row in &rows {
                let range = ex_line_byte_range(text, *row, *row);
                yanked.push_str(&text.slice(range).to_string());
            }
            cx.update_global::<crate::vim::Registers, _>(|registers, cx| {
                registers.record_yank(&yanked, None, cx);
            });
            let first = *rows.first().unwrap_or(&start);
            ex_goto_row(editor, first, cx);
            return Ok(ex_command::ExOutcome::Close);
        }
        let supported = matches!(inner.name.as_str(), "d" | "s" | "j");
        if !supported {
            return Err(format!(":g/:v 暂不支持内层命令 :{}", inner.name));
        }
        for row in rows.into_iter().rev() {
            inner.range = None;
            self.execute_parsed_command(inner.clone(), Some(row), window, cx)?;
        }
        Ok(ex_command::ExOutcome::Close)
    }

    fn ex_shell(
        &mut self,
        command: &ex_command::ExCommand,
        editor: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ex_command::ExOutcome, String> {
        let shell_command = command.args.trim();
        if shell_command.is_empty() {
            return Err(":! 缺少 shell 命令".into());
        }
        let range = if command.range.is_some() {
            let (start, end) = self.ex_default_range(command, editor, false, false, cx)?;
            Some(ex_line_byte_range(editor.read(cx).text(), start, end))
        } else {
            None
        };
        let mut process = shell_process(shell_command);
        process.stdout(std::process::Stdio::piped());
        process.stderr(std::process::Stdio::piped());
        if let Some(range) = range {
            process.stdin(std::process::Stdio::piped());
            let input = editor.read(cx).text().slice(range.clone()).to_string();
            let mut child = process
                .spawn()
                .map_err(|e| format!("无法启动 shell: {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write as _;
                stdin
                    .write_all(input.as_bytes())
                    .map_err(|e| format!("写入 shell stdin 失败: {e}"))?;
            }
            let output = child
                .wait_with_output()
                .map_err(|e| format!("等待 shell 失败: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("shell 退出码 {:?}: {stderr}", output.status.code()));
            }
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            ex_replace_range(editor, range.clone(), &stdout, range.start, window, cx);
            return Ok(ex_command::ExOutcome::Close);
        }

        let output = process
            .output()
            .map_err(|e| format!("无法启动 shell: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut message = String::new();
        if !stdout.is_empty() {
            message.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !message.is_empty() && !message.ends_with('\n') {
                message.push('\n');
            }
            message.push_str(&stderr);
        }
        if message.trim().is_empty() {
            message = format!("shell 退出码 {:?}", output.status.code());
        }
        Ok(ex_command::ExOutcome::Message(message))
    }

    fn ex_set(
        &mut self,
        args: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ex_command::ExOutcome, String> {
        let Some(editor) = self.active_text_editor(cx) else {
            return Err("当前没有可编辑的文本标签".into());
        };
        let options: Vec<&str> = args.split_whitespace().collect();
        if options.is_empty() {
            return Ok(ex_command::ExOutcome::Message(
                self.ex_options_summary(&editor, cx),
            ));
        }
        for option in options {
            match option {
                "nu" | "number" => editor.update(cx, |state, cx| {
                    state.set_line_number(true);
                    cx.notify();
                }),
                "nonu" | "nonumber" => editor.update(cx, |state, cx| {
                    state.set_line_number(false);
                    cx.notify();
                }),
                "rnu" | "relativenumber" => editor.update(cx, |state, cx| {
                    // vim 的 relativenumber 也要求行号栏存在；只开 rnu 时
                    // 仍要显示当前行绝对行号。
                    state.set_line_number(true);
                    if let Some(vim) = state.vim.as_mut() {
                        vim.relative_line_numbers = true;
                    }
                    cx.notify();
                }),
                "nornu" | "norelativenumber" => editor.update(cx, |state, cx| {
                    if let Some(vim) = state.vim.as_mut() {
                        vim.relative_line_numbers = false;
                    }
                    cx.notify();
                }),
                "wrap" => editor.update(cx, |state, cx| {
                    state.set_soft_wrap(true, window, cx);
                }),
                "nowrap" => editor.update(cx, |state, cx| {
                    state.set_soft_wrap(false, window, cx);
                }),
                "ic" | "ignorecase" => {
                    editor.update(cx, |state, cx| {
                        if let Some(vim) = state.vim.as_mut() {
                            vim.ignore_case = true;
                        }
                        if let Some(panel) = state.search_panel.clone() {
                            panel.update(cx, |panel, cx| {
                                panel.set_case_insensitive(true, cx);
                            });
                        }
                        cx.notify();
                    });
                }
                "noic" | "noignorecase" => {
                    editor.update(cx, |state, cx| {
                        if let Some(vim) = state.vim.as_mut() {
                            vim.ignore_case = false;
                        }
                        if let Some(panel) = state.search_panel.clone() {
                            panel.update(cx, |panel, cx| {
                                panel.set_case_insensitive(false, cx);
                            });
                        }
                        cx.notify();
                    });
                }
                other => return Err(format!("未知选项：{other}")),
            }
        }
        Ok(ex_command::ExOutcome::Close)
    }

    fn ex_options_summary(&self, editor: &Entity<InputState>, cx: &App) -> String {
        let state = editor.read(cx);
        let line_number = state.mode.line_number();
        let vim = state.vim.as_ref();
        format!(
            "{} {} {} {}",
            if line_number { "nu" } else { "nonu" },
            if vim.is_some_and(|vim| vim.relative_line_numbers) {
                "rnu"
            } else {
                "nornu"
            },
            if state.soft_wrap { "wrap" } else { "nowrap" },
            if vim.is_some_and(|vim| vim.ignore_case) {
                "ic"
            } else {
                "noic"
            }
        )
    }

    fn ex_clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_text_editor(cx) {
            editor.update(cx, |state, cx| {
                if let Some(panel) = state.search_panel.clone() {
                    panel.update(cx, |panel, cx| panel.hide(window, cx));
                }
            });
        }
    }

    fn ex_marks_message(&self, editor: &Entity<InputState>, cx: &App) -> String {
        let state = editor.read(cx);
        let text = state.text();
        let marks = crate::vim::Marks::global(cx).entries();
        if marks.is_empty() {
            return "No marks".into();
        }
        marks
            .into_iter()
            .map(|(name, offset)| {
                let point = text.offset_to_point(offset.min(text.len()));
                format!("{}  {}:{}", name, point.row + 1, point.column + 1)
            })
            .collect::<Vec<_>>()
            .join("\n")
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

    /// 重命名：光标处符号改名，应用服务器返回的整个 `WorkspaceEdit`。
    ///
    /// `Editor` 里有会话（uri + client），而命名面板只有编辑器实体，
    /// 所以由面板回调到这里发起请求。
    ///
    /// 跨文件是常态（rust-analyzer 改结构体字段会同时改所有使用点）。
    /// 已打开的标签走编辑事务（保留撤销历史并自动发 didChange），未打开
    /// 的文件在后台读改写回磁盘，保留各自的编码与换行风格。
    pub(crate) fn apply_rename(
        &mut self,
        editor: &Entity<InputState>,
        position: lsp_types::Position,
        new_name: String,
        cx: &mut Context<Self>,
    ) {
        let Some((client, origin_uri)) = self
            .active_lsp_client(cx)
            .zip(self.active_lsp_session().map(|s| s.uri().clone()))
        else {
            return;
        };
        // 请求期间用户可能切标签/切主机；后续编辑必须按发起时的归属处理。
        let ssh = self
            .active_lsp_session()
            .and_then(|s| s.ssh_config().cloned());
        let editor = editor.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Some(edit)) = client.rename(&origin_uri, position, &new_name).await else {
                return;
            };

            if let Some(lsp_types::DocumentChanges::Operations(ops)) = &edit.document_changes {
                let has_resource_ops = ops.iter().any(|op| {
                    matches!(op, lsp_types::DocumentChangeOperation::Op(_))
                });
                if has_resource_ops {
                    // 文件创建/移动/删除无法用 TextEdit 表达，当前只应用文本
                    // 编辑。明确记日志，避免用户以为重命名“什么都没做”。
                    log::warn!("lsp: rename contains file resource operations; text edits only");
                }
            }

            let uris = crate::lsp::features::affected_uris(&edit);
            let mut disk_jobs: Vec<(
                std::path::PathBuf,
                String,
                Option<crate::ssh::SshConnectionConfig>,
                Vec<lsp_types::TextEdit>,
            )> = Vec::new();
            let mut open_files = 0usize;

            let Some(entity) = this.upgrade() else {
                return;
            };
            entity.update(cx, |this, cx| {
                for uri in uris {
                    let Some(edits) = crate::lsp::features::edits_for_uri(&edit, &uri) else {
                        continue;
                    };
                    if edits.is_empty() {
                        continue;
                    }
                    // 发起请求的那个编辑器即使标签已被关闭，也还能安全应用
                    // 编辑（InputState 实体仍然存活）。
                    if crate::lsp::client::uri_eq(&uri, &origin_uri) {
                        editor.update(cx, |state, cx| {
                            crate::lsp::session::apply_text_edits(state, edits, cx);
                        });
                        open_files += 1;
                        continue;
                    }
                    let Some(path) = crate::lsp::client::uri_to_path(&uri) else {
                        log::warn!("lsp: rename target is not a file uri: {uri}");
                        continue;
                    };
                    let path_str = path.to_string_lossy().to_string();
                    if let Some(tab_id) = this.find_open_tab_by_path(&path_str, ssh.as_ref()) {
                        let Some(file) = this.open_files.iter().find(|f| f.id.as_ref() == tab_id)
                        else {
                            continue;
                        };
                        if file.read_only {
                            log::warn!("lsp: skip read-only rename target {path_str}");
                            continue;
                        }
                        let state = file.editor_state.clone();
                        state.update(cx, |state, cx| {
                            crate::lsp::session::apply_text_edits(state, edits, cx);
                        });
                        open_files += 1;
                    } else {
                        disk_jobs.push((path, path_str, ssh.clone(), edits));
                    }
                }
            });

            // 未打开的文件：后台读取 → 应用编辑 → 原子写回。远端文件也走
            // 同一条路径，服务器和文件在同一台主机上。
            let (disk_applied, disk_failed) = if disk_jobs.is_empty() {
                (0usize, 0usize)
            } else {
                cx.background_spawn(async move {
                    let mut applied = 0usize;
                    let mut failed = 0usize;
                    for (path, path_str, ssh, edits) in disk_jobs {
                        let loaded = match ssh.as_ref() {
                            Some(config) => load_remote_text(config, &path_str),
                            None => load_local_text(&path),
                        };
                        let loaded = match loaded {
                            Ok(loaded) => loaded,
                            Err(e) => {
                                log::warn!("lsp: rename read failed for {path_str}: {e}");
                                failed += 1;
                                continue;
                            }
                        };
                        if loaded.read_only {
                            log::warn!("lsp: skip read-only rename target {path_str}");
                            failed += 1;
                            continue;
                        }
                        let rope = ropey::Rope::from(loaded.text.as_str());
                        let byte_edits =
                            crate::lsp::features::to_byte_edits(&rope, edits);
                        let new_text =
                            crate::lsp::features::apply_byte_edits(&loaded.text, &byte_edits);
                        if new_text == loaded.text {
                            applied += 1;
                            continue;
                        }
                        match save_text_to_file(
                            &path_str,
                            ssh.as_ref(),
                            &loaded.text,
                            &new_text,
                            loaded.encoding,
                        ) {
                            Ok(_) => applied += 1,
                            Err(e) => {
                                log::warn!("lsp: rename write failed for {path_str}: {e}");
                                failed += 1;
                            }
                        }
                    }
                    (applied, failed)
                })
                .await
            };

            log::info!(
                "lsp: rename applied to {open_files} open file(s), {disk_applied} unopened file(s), {disk_failed} failed"
            );
            if disk_failed > 0
                && let Some(entity) = this.upgrade()
            {
                entity.update(cx, |this, cx| {
                    this.show_notification(
                        format!("重命名有 {disk_failed} 个文件写入失败，详见 LSP 日志"),
                        NotificationKind::Warning,
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    /// LSP 请求的三件套：编辑器实体、客户端、文件 URI。缺任一即无 LSP。
    fn lsp_context(
        &self,
        cx: &App,
    ) -> Option<(
        Entity<InputState>,
        std::sync::Arc<crate::lsp::client::LspClient>,
        lsp_types::Url,
    )> {
        let editor = self.active_text_editor(cx)?;
        let uri = self.active_lsp_session()?.uri().clone();
        let client = self.active_lsp_client(cx)?;
        Some((editor, client, uri))
    }

    /// 跳转定义：跳到服务器给的第一个位置（**支持跨文件**）。
    ///
    /// 对照 Zed 的 `go_to_definition_of_kind`：它拿到 `Location` 后
    /// `open_local_buffer_via_lsp` 打开目标文件再选中区间——从不因为
    /// 「目标在别的文件」而放弃。
    ///
    /// 之前的实现只在目标 uri 与当前文件相同时才跳，跨文件时静默返回，
    /// 于是「跳转不工作」——而这恰恰是最常见的场景（跳到其它模块的定义）。
    fn goto_definition(&mut self, _: &GotoDefinition, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_kind(LspGotoKind::Definition, window, cx);
    }

    /// 跳转声明（`textDocument/declaration`）。
    fn goto_declaration(
        &mut self,
        _: &GotoDeclaration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goto_kind(LspGotoKind::Declaration, window, cx);
    }

    /// 跳转类型定义（`textDocument/typeDefinition`）。
    fn goto_type_definition(
        &mut self,
        _: &GotoTypeDefinition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goto_kind(LspGotoKind::TypeDefinition, window, cx);
    }

    /// 跳转实现（`textDocument/implementation`）。
    fn goto_implementation(
        &mut self,
        _: &GotoImplementation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goto_kind(LspGotoKind::Implementation, window, cx);
    }

    /// 跳转族的公共实现：四个动作只有请求方法不同。
    fn goto_kind(&mut self, kind: LspGotoKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, client, uri)) = self.lsp_context(cx) else {
            return;
        };
        let position = editor.read(cx).lsp_cursor_position();
        // 归属在发请求**之前**捕获：响应到达时用户可能已经切了标签。
        let origin = self.lsp_jump_origin();
        // 跳转前记录「从哪来」，供 `ctrl--` 返回。
        self.push_nav_history(cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = match kind {
                LspGotoKind::Definition => client.goto_definition(&uri, position).await,
                LspGotoKind::Declaration => client.goto_declaration(&uri, position).await,
                LspGotoKind::TypeDefinition => client.goto_type_definition(&uri, position).await,
                LspGotoKind::Implementation => client.goto_implementation(&uri, position).await,
            };
            let Ok(Some(response)) = result else {
                return;
            };
            let Some(location) = crate::lsp::features::first_location(&response) else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.open_lsp_location(&location, origin, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Cmd+Click（其余平台 Ctrl+Click）跳到点击处的定义——和 VSCode 一样。
    ///
    /// 事件顺序天然对齐 VSCode：更深一层的输入框先收到这次按下，并把光标
    /// 移到点击位置（VSCode `gotoDefinition()` 的第一行也正是
    /// `this.editor.setPosition(position)`），我们随后按**当前光标**发请求
    /// （它的 `goToCommands` 读的同样是 `editor.getPosition()`）。所以这里
    /// 不必自己换算偏移，只需要判定与转发。
    fn goto_definition_at_click(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !is_goto_definition_click(event) {
            return;
        }
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        // 必须点在正文上：行号栏、git 色条、折叠箭头各自有点击语义。
        // 对照 VSCode 的 `target.type === MouseTargetType.CONTENT_TEXT`。
        if !editor.read(cx).click_is_in_text(event.position) {
            return;
        }
        self.goto_kind(LspGotoKind::Definition, window, cx);
    }

    /// 打开一个 LSP `Location`：同文件直接选中，跨文件先开目标文件再跳。
    ///
    /// LSP 的位置是 (line, character)（UTF-16 列，0 基）；编辑器内部的
    /// `jump_to_line_col` 收 1 基行号，所以这里 +1。
    /// 压入一条跳转历史（跳转**前**调用）。
    ///
    /// 记录当前文件与光标偏移，供 [`Self::navigate_back`] 返回。
    fn push_nav_history(&mut self, cx: &App) {
        let Some(tab_id) = self.active_file_id.as_ref() else {
            return;
        };
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        let offset = editor.read(cx).cursor();
        let entry = NavLocation {
            tab_id: tab_id.to_string(),
            offset,
        };
        // 连续在同一点跳转不重复压栈（避免历史里一堆相同项）。
        if self.nav_history.last() == Some(&entry) {
            return;
        }
        self.nav_history.push(entry);
        if self.nav_history.len() > NAV_HISTORY_LIMIT {
            self.nav_history.remove(0);
        }
    }

    /// 返回上一个跳转位置（`ctrl--`）。
    ///
    /// 对照 Zed 的 `navigate_backward`：弹出栈顶并跳回去。栈空时什么都不做
    /// （不报错——用户连按几次到底了是正常操作）。
    fn navigate_back(&mut self, _: &NavigateBack, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.nav_history.pop() else {
            log::debug!("lsp: navigation history is empty");
            return;
        };
        // 目标标签可能已被关闭：继续往回找，直到找到一个还存在的。
        let mut target = target;
        loop {
            let exists = self
                .open_files
                .iter()
                .any(|f| f.id.as_ref() == target.tab_id);
            if exists {
                break;
            }
            match self.nav_history.pop() {
                Some(next) => target = next,
                None => return,
            }
        }

        // 切到那个标签并把光标放回原偏移。
        let tab_id = target.tab_id.clone();
        self.switch_to_file(&tab_id, cx);
        let editor = self
            .open_files
            .iter()
            .find(|f| f.id.as_ref() == tab_id)
            .map(|f| f.editor_state.clone());
        if let Some(editor) = editor {
            editor.update(cx, |state, cx| {
                let offset = target.offset.min(state.text().len());
                state.selected_range = (offset..offset).into();
                state.reveal_offset(offset, cx);
                cx.notify();
            });
        }
        let _ = window;
        cx.notify();
    }

    /// 捕获当前会话的跳转归属（必须在发请求**之前**调，理由见 [`LspJumpOrigin`]）。
    fn lsp_jump_origin(&self) -> Option<LspJumpOrigin> {
        let tab_id = self.active_file_id.clone();
        self.active_lsp_session().map(|s| LspJumpOrigin {
            uri: s.uri().clone(),
            ssh_config: s.ssh_config().cloned(),
            root: s.server_root().map(|r| r.to_path_buf()),
            tab_id,
        })
    }

    /// 打开一个 LSP `Location`：同文件直接选中，跨文件先开目标文件再跳。
    fn open_lsp_location(
        &mut self,
        location: &lsp_types::Location,
        origin: Option<LspJumpOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 目标就是当前文件：直接跳，避免多余的「打开」往返。
        //
        // 必须走 `uri_eq` 而不是 `==`：windows 上 rust-analyzer 回的盘符
        // 小写（`/c:/...`）而我们存的是大写（`/C:/...`），精确比较会把
        // 同文件跳转误判成跨文件 → 每次跳转都新开标签而非原地移光标。
        //
        // 归属（uri + 主机）用**发起请求时**捕获的 `origin`；事后重问
        // `active_lsp_session()` 在用户切过标签后会拿到错的会话。没捕获到的
        // 调用方（测试里的直接跳转）退回当前会话，行为与改动前一致。
        let active = origin.or_else(|| self.lsp_jump_origin());
        let same_file = active
            .as_ref()
            .map(|o| crate::lsp::client::uri_eq(&o.uri, &location.uri))
            .unwrap_or(false);

        match lsp_location_action(location, same_file) {
            // 非 file:// uri（如 untitled:）：无法打开，保持不动作。
            LspJumpAction::Ignore => {
                log::debug!("lsp: cannot open non-file location {}", location.uri);
            }
            LspJumpAction::JumpInPlace { position } => {
                // 跳到**发起请求的那个标签**，不是此刻活动的标签。
                //
                // `same_file` 是用请求前捕获的 `origin` 算的，用户在这段
                // 等待里切了标签时，两者会指向不同文件：按活动标签跳会把
                // 光标打进一个毫不相干的文件。发起标签已关闭时才退回活动
                // 标签（此时没有更好的落点）。
                let tab_id = active
                    .as_ref()
                    .and_then(|o| o.tab_id.clone())
                    .filter(|id| self.open_files.iter().any(|f| &f.id == id))
                    .or_else(|| self.active_file_id.clone());
                if let Some(tab_id) = tab_id {
                    self.jump_to_lsp_position(&tab_id, position, window, cx);
                }
            }
            // 跨文件（或目标未打开）：打开并定位。
            //
            // **按当前会话的归属决定走哪条打开路径**：远端会话（服务器跑在
            // SSH 那台机器上）返回的 Location 路径也在那台机器上，必须走
            // 远程打开；按本地路径打开会去本机磁盘上找一个不存在的文件，
            // 结果是「跳转了但什么也没发生」。
            LspJumpAction::OpenFile { path, position } => {
                // 归属拆成两半：主机决定走哪条打开路径，根决定目标文件接哪个服务器。
                let (remote, origin_root) = match active {
                    Some(o) => (o.ssh_config, o.root),
                    None => (None, None),
                };
                let path_str = path.to_string_lossy().to_string();
                if let Some(root) = origin_root {
                    // 记下「这个文件是这台服务器报出来的」。`start_lsp_for_tab`
                    // 打开它时直接挂回这个根，不再按落点路径另算一个。
                    let key = lsp_jump_root_key(&path_str, remote.as_ref());
                    self.lsp_jump_roots.insert(key.clone(), root);
                    self.prune_lsp_jump_roots(&key);
                }
                match remote.as_ref() {
                    Some(ssh_config) => self.open_remote_file_at_line_col(
                        path_str.clone(),
                        ssh_config.clone(),
                        // 行列留空：目标文本可能还在异步加载，LSP Position
                        // 必须等加载完成后按 UTF-16 换算，不能提前降级成
                        // 编译器口径的 1 基字符列。
                        None,
                        None,
                        window,
                        cx,
                    ),
                    None => self.open_file_at_line_col(path.clone(), None, None, window, cx),
                }
                // 打开动作可能是同步复用已有标签，也可能是新标签（还在
                // loading）。两种情况都交给 `jump_to_lsp_position`：它会在
                // 未加载时把 Position 放进 `pending_lsp_jumps`。
                if let Some(tab_id) = self.find_open_tab_by_path(&path_str, remote.as_ref()) {
                    self.jump_to_lsp_position(&tab_id, position, window, cx);
                } else {
                    log::debug!("lsp: target file opened but tab not found for jump: {path_str}");
                }
            }
        }
    }

    /// 查找引用：跳到第一个结果（**支持跨文件**，与跳转定义同一管线）。
    ///
    /// 完整的引用列表面板需要新的 UI；这里先保证「能跳到第一个」，
    /// 与 `goto_definition` 共用 [`Self::open_lsp_location`]。
    fn find_references(&mut self, _: &FindReferences, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, client, uri)) = self.lsp_context(cx) else {
            return;
        };
        let position = editor.read(cx).lsp_cursor_position();
        // 归属在发请求**之前**捕获（同 `goto_kind`）。
        let origin = self.lsp_jump_origin();
        self.push_nav_history(cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(locations) = client.references(&uri, position, true).await else {
                return;
            };
            let Some(first) = locations.first().cloned() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.open_lsp_location(&first, origin, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 格式化文档：应用服务器返回的 `TextEdit[]`。
    fn format_document(
        &mut self,
        _: &FormatDocument,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((editor, client, uri)) = self.lsp_context(cx) else {
            return;
        };
        let tab_size = editor.read(cx).tab_size;

        // 有选区时格式化**选区**（`textDocument/rangeFormatting`），
        // 无选区才格式化整篇。对照 LSP 规范：两个方法分开正是为了这个
        // 区分——此前一律调 `formatting`，用户选中几行按格式化会改掉整篇。
        let selection = {
            let state = editor.read(cx);
            let range = state.selected_range;
            if range.is_empty() {
                None
            } else {
                let text = state.text();
                Some(lsp_types::Range::new(
                    crate::lsp::features::offset_to_position(text, range.start),
                    crate::lsp::features::offset_to_position(text, range.end),
                ))
            }
        };

        cx.spawn(async move |_this, cx| {
            let edits = match selection {
                Some(range) => {
                    client
                        .range_formatting(
                            &uri,
                            range,
                            tab_size.tab_size as u32,
                            !tab_size.hard_tabs,
                        )
                        .await
                }
                None => {
                    client
                        .formatting(&uri, tab_size.tab_size as u32, !tab_size.hard_tabs)
                        .await
                }
            };
            let Ok(edits) = edits else {
                return;
            };
            cx.update_entity(&editor, |state, cx| {
                crate::lsp::session::apply_text_edits(state, edits, cx);
            });
        })
        .detach();
    }

    /// 手动触发补全。
    fn trigger_completion(
        &mut self,
        _: &TriggerCompletion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        let Some(uri) = self.active_lsp_session().map(|s| s.uri().clone()) else {
            // 没有可用会话：顺手收起可能残留的旧菜单。
            self.completion.hide_in(&editor, cx);
            cx.notify();
            return;
        };
        let client = self.active_lsp_client(cx);
        self.completion.trigger(&editor, client, uri, cx);
        cx.notify();
    }

    /// 切换 hover 浮层的键盘接管（vim normal 下的 `K`）。
    ///
    /// 三态：
    /// 1. 没有浮层 → 按**光标位置**请求 hover（不等防抖），显示卡片；
    /// 2. 有浮层但没被接管 → 打开键盘接管，此后 `hjkl` / 方向键滚动内容，
    ///    鼠标移动与点击外部都不再收起它；
    /// 3. 已被接管 → 退出接管（浮层保留，下次按 K 可再进入）。
    ///
    /// 接管是虚拟的（见 `InputState::hover_keyboard_takeover` 的文档）：
    /// 焦点物理上还在编辑器里，只是 `hjkl` 等键被全局拦截器提前拿走。
    /// 第2步之后隐藏路径都必须先问
    /// [`InputState::hover_keyboard_takeover`] —— 对照 Zed：它在
    /// `element/mouse.rs:466`、`editor.rs:10234`、`element.rs:697` 三处用
    /// `hover_state.focused()` 做同样的守卫。
    fn toggle_hover(&mut self, _: &Hover, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };

        // 态 3：已被接管 → 退出接管（浮层保留）。
        if editor.read(cx).hover_keyboard_takeover() {
            editor.update(cx, |state, cx| {
                state.set_hover_keyboard_takeover(false);
                cx.notify();
            });
            log::info!("lsp: hover 退出键盘接管");
            return;
        }

        // 态 2：浮层已显示 → 打开接管。
        if editor.read(cx).hover_popover().is_some() {
            editor.update(cx, |state, cx| {
                state.set_hover_keyboard_takeover(true);
                // 接管期间不允许有待执行的延迟隐藏悬着。
                state.cancel_hover_hide();
                cx.notify();
            });
            log::info!("lsp: hover 已被键盘接管，hjkl 可滚动内容");
            cx.notify();
            return;
        }

        // 态 1：请求 hover。用光标位置（而不是鼠标位置）—— 按键触发的查询
        // 关心的是「光标在哪」。
        let Some(uri) = self.active_lsp_session().map(|s| s.uri().clone()) else {
            return;
        };
        let client = self.active_lsp_client(cx);
        let cursor = editor.read(cx).cursor();
        // 普通 hover 接管浮层后，签名帮助就不能再认为自己拥有当前卡片；
        // 否则光标再动时它会把注释卡当成签名卡刷新掉。
        self.signature_help_active = false;
        self.signature_hover_anchor = None;
        self.hover
            .on_keyboard_triggered(cursor, &editor, client, uri, cx);
        cx.notify();
    }

    /// 补全菜单：下一项。
    fn completion_next(&mut self, _: &CompletionNext, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.prepare_completion_navigation(window, cx) else {
            return;
        };
        self.completion.move_selection(1);
        self.completion.sync_selection(&editor, cx);
        cx.notify();
    }

    /// 补全菜单：上一项。
    fn completion_prev(&mut self, _: &CompletionPrev, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.prepare_completion_navigation(window, cx) else {
            return;
        };
        self.completion.move_selection(-1);
        self.completion.sync_selection(&editor, cx);
        cx.notify();
    }

    /// 补全菜单：第一项。
    fn completion_first(
        &mut self,
        _: &CompletionFirst,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.prepare_completion_navigation(window, cx) else {
            return;
        };
        self.completion.select_first();
        self.completion.sync_selection(&editor, cx);
        cx.notify();
    }

    /// 补全菜单：最后一项。
    fn completion_last(&mut self, _: &CompletionLast, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.prepare_completion_navigation(window, cx) else {
            return;
        };
        self.completion.select_last();
        self.completion.sync_selection(&editor, cx);
        cx.notify();
    }

    /// 补全导航动作的可见性、焦点与过期校验。
    ///
    /// 菜单不可见或焦点不在编辑区时显式放行按键；一旦菜单当时可见，就先消费
    /// 按键，即使 `validate` 随后发现菜单过期，也不能把方向键透给编辑器。
    fn prepare_completion_navigation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<InputState>> {
        if !self.completion.is_visible() {
            // GPUI 的动作在冒泡阶段**默认阻断**传播；补全拦截器无条件绑定在
            // `Input` 上，菜单不可见时必须显式放行，后面的 `editor::Enter` /
            // `IndentInline` / 方向键才会继续执行。
            cx.propagate();
            return None;
        }
        let Some(editor) = self.focused_completion_editor(window, cx) else {
            cx.propagate();
            return None;
        };
        cx.stop_propagation();
        self.completion.validate(&editor, cx);
        if !self.completion.is_visible() {
            return None;
        }
        Some(editor)
    }

    /// 补全菜单：确认插入。
    fn completion_confirm(
        &mut self,
        _: &CompletionConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.completion.is_visible() {
            // GPUI 的动作在冒泡阶段**默认阻断**传播；补全拦截器无条件绑定在
            // `Input` 上，菜单不可见时必须显式放行，后面的 `editor::Enter` /
            // `IndentInline` / 方向键才会继续执行。
            cx.propagate();
            return;
        }
        log::info!(
            "lsp: completion Confirm 键位触发 anchor={}",
            self.completion.anchor()
        );
        let Some(editor) = self.focused_completion_editor(window, cx) else {
            cx.propagate();
            return;
        };
        // **先消费按键，再校验菜单**。`validate` 可能因为光标/文本/焦点已变
        // 而收起菜单；此时若还没 `stop_propagation`，Enter 会继续派发给
        // `Input` 上的 `editor::Enter`，用户看到的就是「补全没插入，反而多
        // 了一个换行」。只要菜单状态是可见，这次 Enter/Tab 就归补全管。
        cx.stop_propagation();
        // 确认菜单仍属于当前编辑器/光标；切 tab 或点走光标后不能拿旧候选
        // 往新位置插入。
        self.completion.validate(&editor, cx);
        if !self.completion.is_visible() {
            return;
        }
        let client = self.active_lsp_client(cx);
        // 需要 `completionItem/resolve` 时，在这里 spawn：只有
        // `Context<Editor>` 能把 `cx` 带进 async 块。
        let pending = self.completion.confirm(&editor, client.clone(), cx);
        if let (Some((item, range)), Some(client)) = (pending, client) {
            // resolve 是异步的；期间用户可能继续输入。文本变了以后响应里的
            // 区间/附加编辑都基于旧文档，宁可放弃这次插入，也不能写错位置。
            let text_version = editor.read(cx).text_version();
            cx.spawn(async move |_this, cx| {
                let resolved = client.resolve_completion(item.clone()).await;
                let item = match resolved {
                    Ok(item) => item,
                    // resolve 失败不该阻止补全本身：用原候选照常插入。
                    Err(e) => {
                        log::debug!("lsp: completionItem/resolve failed, using raw item: {e}");
                        item
                    }
                };
                // `apply_completion_item` 需要 `&mut App`；`AsyncApp`
                // 通过 `update` 提供。
                cx.update(|cx| {
                    if editor.read(cx).text_version() != text_version {
                        log::debug!("lsp: 文本在 resolve 期间变化，放弃本次补全");
                        return;
                    }
                    crate::editor::completion::apply_completion_item(&item, &editor, range, cx);
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// 补全菜单：取消（Esc）。
    fn completion_cancel(
        &mut self,
        _: &CompletionCancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.completion.is_visible() {
            // GPUI 的动作在冒泡阶段**默认阻断**传播；补全拦截器无条件绑定在
            // `Input` 上，菜单不可见时必须显式放行，后面的 `editor::Enter` /
            // `IndentInline` / 方向键才会继续执行。
            cx.propagate();
            return;
        }
        let Some(editor) = self.focused_completion_editor(window, cx) else {
            // 焦点在编辑区里的查找框/重命名框：这个 Esc 不属于补全。
            cx.propagate();
            return;
        };
        // 只关补全，不再透传给 `Input` 的 Escape（那会清选区/退出 vim 插入）。
        cx.stop_propagation();
        self.completion.hide_in(&editor, cx);
        cx.notify();
    }

    /// 重命名面板（F2）：开则关、关则开。
    fn toggle_rename(&mut self, _: &RenameSymbol, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename_panel.take().is_some() {
            if let Some(editor) = self.active_text_editor(cx) {
                editor.read(cx).focus_handle.clone().focus(window, cx);
            }
            cx.notify();
            return;
        }
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        // 光标不在符号上时不开面板：改名没有意义。
        if editor.read(cx).word_at_cursor().is_empty() {
            return;
        }

        // 先问服务器「这里能重命名吗」（`textDocument/prepareRename`）。
        //
        // 对照 LSP 规范：服务器返回的 `range` 比客户端按词边界猜的更准
        // （例如 `foo.bar` 只应替换 `bar`），且能在**不可重命名**的位置
        // （关键字、字面量）明确拒绝。此前不问，用户在这些位置也会看到
        // 输入框、输完却没有效果。
        if let Some((client, uri)) = self
            .active_lsp_client(cx)
            .zip(self.active_lsp_session().map(|s| s.uri().clone()))
        {
            let position = editor.read(cx).lsp_cursor_position();
            let handle = self.window_handle.unwrap_or_else(|| window.window_handle());
            cx.spawn_in(window, async move |this, cx| {
                match client.prepare_rename(&uri, position).await {
                    // 服务器说可重命名：开面板。placeholder 比词边界更准。
                    Ok(Some(response)) => {
                        let original = match response {
                            lsp_types::PrepareRenameResponse::RangeWithPlaceholder {
                                placeholder,
                                ..
                            } => Some(placeholder),
                            lsp_types::PrepareRenameResponse::DefaultBehavior {
                                default_behavior: false,
                            } => {
                                this.update_in(cx, |this, _window, cx| {
                                    this.show_notification(
                                        "此处不可重命名",
                                        NotificationKind::Warning,
                                        cx,
                                    );
                                })
                                .ok();
                                return;
                            }
                            _ => None,
                        };
                        this.update_in(cx, |this, window, cx| {
                            this.open_rename_panel(editor, handle, original, window, cx);
                        })
                        .ok();
                    }
                    // 服务器明确拒绝：不开面板，给用户一句反馈。
                    Ok(None) => {
                        this.update_in(cx, |this, _window, cx| {
                            this.show_notification("此处不可重命名", NotificationKind::Warning, cx);
                        })
                        .ok();
                    }
                    // 服务器**明确拒绝**（如 "No references found at
                    // position"）：这不是网络问题，是真的不可重命名。
                    // 实测 rust-analyzer 对关键字/无引用位置就回这个错。
                    Err(e) if is_rename_rejection(&e) => {
                        log::debug!("lsp: prepareRename rejected: {e}");
                        this.update_in(cx, |this, _window, cx| {
                            this.show_notification("此处不可重命名", NotificationKind::Warning, cx);
                        })
                        .ok();
                    }
                    // 其它失败（超时、方法未实现）：退回按词边界的老行为，
                    // 不要因为一次传输问题就禁掉功能。
                    Err(e) => {
                        log::debug!("lsp: prepareRename failed, falling back: {e}");
                        this.update_in(cx, |this, window, cx| {
                            this.open_rename_panel(editor, handle, None, window, cx);
                        })
                        .ok();
                    }
                }
            })
            .detach();
            return;
        }

        // 没有 LSP：直接开面板（按词边界）。
        let handle = self.window_handle.unwrap_or_else(|| window.window_handle());
        self.open_rename_panel(editor, handle, None, window, cx);
    }

    /// 打开重命名面板（`prepareRename` 通过后的落点）。
    fn open_rename_panel(
        &mut self,
        editor: Entity<InputState>,
        handle: gpui::AnyWindowHandle,
        original: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = rename::RenamePanel::new(
            cx.entity().downgrade(),
            editor,
            handle,
            original,
            window,
            cx,
        );
        panel.update(cx, |p, cx| p.focus(window, cx));
        self.rename_panel = Some(panel);
        cx.notify();
    }

    /// 文档符号面板（cmd-shift-o）：开则关、关则请求符号列表后打开。
    fn toggle_symbols(&mut self, _: &DocumentSymbols, window: &mut Window, cx: &mut Context<Self>) {
        if self.symbols_panel.take().is_some() {
            if let Some(editor) = self.active_text_editor(cx) {
                editor.read(cx).focus_handle.clone().focus(window, cx);
            }
            cx.notify();
            return;
        }
        let Some((editor, client, uri)) = self.lsp_context(cx) else {
            return;
        };
        let handle = self.window_handle.unwrap_or_else(|| window.window_handle());
        // 先建空面板（立刻给反馈），符号到了再填充。
        let panel = symbols::SymbolsPanel::new(editor.clone(), Vec::new(), handle, cx);
        self.symbols_panel = Some(panel.clone());

        cx.spawn(async move |_this, cx| {
            let Ok(response) = client.document_symbols(&uri).await else {
                return;
            };
            cx.update_entity(&editor, |state, cx| {
                state.set_pending_symbols(response);
                // 结果是异步到达的，必须在**这里** notify：下面那句
                // `cx.notify()` 发生在任务创建之前，对后续到达的结果无效。
                // 少了这次 notify，`drain_lsp_results` 不会被触发，符号面板
                // 拿不到结果。
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 请求 LSP 文档高亮（光标处符号的所有出现位置）。
    ///
    /// 结果覆盖编辑器的「同词高亮」：服务器按语义作用域算，不会把同名但
    /// 不同作用域的符号混在一起。
    fn request_document_highlight(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        let Some(client) = self.active_lsp_session().and_then(|s| s.client().cloned()) else {
            return;
        };
        // 每次光标移动都请求会打挂服务器：只在光标真的换了位置时才发。
        let cursor = editor.read(cx).cursor();
        if self.last_highlight_cursor == Some(cursor) {
            return;
        }
        self.last_highlight_cursor = Some(cursor);
        let Some(uri) = self.active_lsp_session().map(|s| s.uri().clone()) else {
            return;
        };
        let position = editor.read(cx).lsp_cursor_position();

        cx.spawn(async move |_this, cx| {
            let Ok(highlights) = client.document_highlight(&uri, position).await else {
                return;
            };
            cx.update_entity(&editor, |state, cx| {
                let ranges = crate::lsp::features::to_highlight_ranges(state.text(), highlights);
                state.set_lsp_highlights(Some(ranges));
                cx.notify();
            });
        })
        .detach();
    }

    /// 请求签名帮助：光标停在 `(` / `,` 之后时显示当前函数签名。
    ///
    /// 与 hover 共用同一张浮层（同时只会显示一个），但独立请求——
    /// 签名帮助的触发条件（在参数列表内）与 hover（在标识符上）不同。
    fn request_signature_help(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };

        // 先取快照再判断：后面的 `editor.update` 需要可变借用，不能一边读
        // state 一边在同一个表达式里调它。
        let (cursor, in_params, popover_anchor, prev_char) = {
            let state = editor.read(cx);
            let cursor = state.cursor();
            let text = state.text();
            (
                cursor,
                signature_trigger_for(text, cursor),
                state.hover_popover().map(|(anchor, _)| anchor),
                char_before(text, cursor),
            )
        };

        if !in_params {
            // 光标离开参数列表：签名浮层不能继续挂着。只清自己那张卡，
            // 不碰用户可能正在看的 hover。
            self.last_signature_cursor = None;
            let owned = self.signature_help_active
                && popover_anchor.is_some()
                && popover_anchor == self.signature_hover_anchor;
            self.signature_help_active = false;
            self.signature_hover_anchor = None;
            if owned {
                editor.update(cx, |state, cx| {
                    state.clear_hover_popover();
                    cx.notify();
                });
            }
            return;
        }

        // 有浮层时只允许刷新「自己」那张签名卡。普通 hover 仍然优先，
        // 不能被签名帮助覆盖（鼠标悬停时用户想看的是注释）。
        if popover_anchor.is_some() {
            let owned = self.signature_help_active
                && popover_anchor.is_some()
                && popover_anchor == self.signature_hover_anchor;
            if !owned {
                self.signature_help_active = false;
                self.signature_hover_anchor = None;
                return;
            }
        }

        if self.last_signature_cursor == Some(cursor) {
            return;
        }
        let Some(client) = self.active_lsp_session().and_then(|s| s.client().cloned()) else {
            return;
        };
        let Some(uri) = self.active_lsp_session().map(|s| s.uri().clone()) else {
            return;
        };

        self.last_signature_cursor = Some(cursor);
        // 请求代号：慢响应回来时如果光标已经移动/发了新请求，直接丢弃，
        // 避免旧签名覆盖新签名。
        self.signature_request_generation = self.signature_request_generation.wrapping_add(1);
        let generation = self.signature_request_generation;
        let was_active = self.signature_help_active;

        let position = editor.read(cx).lsp_cursor_position();
        let trigger_char = match prev_char {
            Some(c @ ('(' | ',')) => Some(c.to_string()),
            _ => None,
        };
        let context = lsp_types::SignatureHelpContext {
            trigger_kind: if trigger_char.is_some() {
                lsp_types::SignatureHelpTriggerKind::TRIGGER_CHARACTER
            } else {
                lsp_types::SignatureHelpTriggerKind::CONTENT_CHANGE
            },
            trigger_character: trigger_char,
            is_retrigger: was_active,
            active_signature_help: None,
        };

        cx.spawn(async move |this, cx| {
            let reply = client
                .signature_help_with_context(&uri, position, Some(context))
                .await;
            this.update(cx, |this, cx| {
                if this.signature_request_generation != generation {
                    return;
                }
                let Some(active) = this.active_text_editor(cx) else {
                    return;
                };
                if active.entity_id() != editor.entity_id() {
                    return;
                }

                // `editor.update` 里拿不到 `this` 的可变借用（`this` 已经
                // 被本闭包借用），所以先把结果算成一个局部状态，回到外层
                // 再写 Editor 字段。
                let outcome = editor.update(cx, |state, cx| {
                    if state.cursor() != cursor || !signature_trigger_for(state.text(), cursor) {
                        return None;
                    }
                    match reply {
                        Ok(Some(help)) => {
                            let contents = crate::lsp::features::signature_text(&help);
                            if contents.trim().is_empty() {
                                return None;
                            }
                            let contents =
                                crate::editor::hover::HoverInfo::from_plain_text(contents, cx);
                            state.set_pending_hover(contents, cursor);
                            Some(true)
                        }
                        // 服务器没有签名或请求失败：清掉自己那张卡，避免
                        // 已经离开调用点时还显示旧签名。
                        _ => {
                            if was_active {
                                state.clear_hover_popover();
                            }
                            Some(false)
                        }
                    }
                });

                match outcome {
                    Some(true) => {
                        this.signature_help_active = true;
                        this.signature_hover_anchor = Some(cursor);
                        cx.notify();
                    }
                    Some(false) => {
                        this.signature_help_active = false;
                        this.signature_hover_anchor = None;
                        cx.notify();
                    }
                    None => {}
                }
            })
            .ok();
        })
        .detach();
    }

    /// 光标是否处于「参数列表内」——即前面有未闭合的 `(`。
    ///
    /// 只做括号配平的粗判：不解析字符串/注释里的括号。签名帮助是提示性的，
    /// 偶尔多弹一次比引入一套词法分析划算。
    /// 应用 Code Action（快速修复）：取服务器给的**首选**动作并应用。
    ///
    /// 首选 = `isPreferred` 为 true 的那条；没有则取第一条。完整的动作选择
    /// 菜单要一个新的浮层；首版先保证「一键修复」这条最常用路径可用。
    fn apply_code_action(&mut self, _: &CodeAction, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, client, uri)) = self.lsp_context(cx) else {
            return;
        };
        // 用整行范围请求：服务器在整行上给出的动作（如「移除未使用的导入」）
        // 比单字符范围全得多。
        let line_range = {
            let state = editor.read(cx);
            let text = state.text();
            let row = text.byte_to_line_idx(state.cursor().min(text.len()), ropey::LineType::LF);
            lsp_types::Range::new(
                crate::lsp::features::offset_to_position(text, text.line_start_offset(row)),
                crate::lsp::features::offset_to_position(text, text.line_content_end_offset(row)),
            )
        };

        cx.spawn(async move |_this, cx| {
            let Ok(response) = client.code_actions(&uri, line_range, Vec::new()).await else {
                return;
            };
            // 首选优先，否则第一条。
            let chosen = response
                .iter()
                .find(|a| {
                    matches!(a, lsp_types::CodeActionOrCommand::CodeAction(a)
                        if a.is_preferred == Some(true))
                })
                .or_else(|| response.first())
                .cloned();
            let Some(chosen) = chosen else {
                return;
            };

            // `Command` 形态：动作本体是服务器命令，必须让它执行。
            // 此前直接跳过，用户看到的是「点了没反应」。
            if let lsp_types::CodeActionOrCommand::Command(cmd) = &chosen {
                if let Err(e) = client
                    .execute_command(&cmd.command, cmd.arguments.clone().unwrap_or_default())
                    .await
                {
                    log::warn!("lsp: executeCommand `{}` failed: {e}", cmd.command);
                }
                return;
            }

            let lsp_types::CodeActionOrCommand::CodeAction(action) = chosen else {
                return;
            };

            // 惰性动作：服务器可能只给了标题，`edit` 要通过 `codeAction/resolve`
            // 现取（对照 LSP 的 `resolveProvider`）。
            let action = match client.resolve_code_action(action).await {
                Ok(resolved) => resolved,
                Err(e) => {
                    log::debug!("lsp: codeAction/resolve failed: {e}");
                    return;
                }
            };

            // resolve 之后仍可能只有 command（例如「应用到所有」类动作）。
            if action.edit.is_none()
                && let Some(cmd) = action.command.as_ref()
            {
                if let Err(e) = client
                    .execute_command(&cmd.command, cmd.arguments.clone().unwrap_or_default())
                    .await
                {
                    log::warn!("lsp: executeCommand `{}` failed: {e}", cmd.command);
                }
                return;
            }

            let Some(edits) = action
                .edit
                .as_ref()
                .and_then(|e| crate::lsp::features::edits_for_uri(e, &uri))
            else {
                return;
            };
            cx.update_entity(&editor, |state, cx| {
                crate::lsp::session::apply_text_edits(state, edits, cx);
            });
        })
        .detach();
    }

    /// 请求 LSP Inlay Hint（可见区间的）。
    ///
    /// 按「首个可见行 .. 末个可见行」请求，而不是全文件——服务器算全文件
    /// 的 hint 在大文件上很慢，而用户一次只能看到几十行。
    fn request_inlay_hints(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        let Some(client) = self.active_lsp_session().and_then(|s| s.client().cloned()) else {
            return;
        };
        let Some(uri) = self.active_lsp_session().map(|s| s.uri().clone()) else {
            return;
        };
        let Some(range) = editor.read(cx).visible_lsp_range() else {
            return;
        };
        // 可见区间没变就不重发（滚动/打字会频繁触发渲染）。
        if self.last_inlay_range.as_ref() == Some(&range) {
            return;
        }
        self.last_inlay_range = Some(range);

        cx.spawn(async move |_this, cx| {
            let Ok(hints) = client.inlay_hints(&uri, range).await else {
                return;
            };
            cx.update_entity(&editor, |state, cx| {
                let data = crate::lsp::features::to_inlay_hints(state.text(), hints);
                state.set_inlay_hints(data);
                cx.notify();
            });
        })
        .detach();
    }

    /// 安排延迟收起 hover 浮层。
    ///
    /// 调用方：鼠标移出正文（[`Self::hover_pointer_off_target`]）、当前标签
    /// 没有 LSP 会话。启动前会先挡掉「键盘接管」「指针在卡片上」两种情形。
    ///
    /// 与 Zed 的 `hiding_delay_task` 同构：收起被推迟 `HOVER_HIDING_DELAY`，
    /// 让指针有机会移到浮层上操作里面的内容；期间若鼠标又回到符号上，
    /// `HoverState::on_mouse_moved` 会取消这个任务。
    fn schedule_hover_hide(&mut self, cx: &mut Context<Self>) {
        // `target` 是承载浮层状态的 `InputState`（绘制读的是它），
        // `owner` 是 `Editor` 自身——`HoverState` 只清自己的计时器，
        // 清 `InputState` 上的浮层状态得由 `Editor` 走 `hide_in`。
        let Some(target) = self.active_text_editor(cx) else {
            return;
        };
        // 浮层被键盘接管时不安排隐藏 —— 用户正在用 hjkl 看内容。
        if target.read(cx).hover_keyboard_takeover() {
            return;
        }
        // 指针已经在浮层上就别安排了：延迟到期后 `hide_in` 虽然会因这个
        // 标记而跳过收起，但白等 300ms 再让用户看着卡片不动毫无意义。
        if target.read(cx).hover_pointer_inside() {
            return;
        }
        let owner = cx.entity().downgrade();
        self.hover.schedule_hide(cx, move |async_cx| {
            // 延迟到期时实体可能已销毁 —— `update` 返回 `None` 即「无需处理」。
            // 到期也要再问一次：300ms 里指针可能进了卡片、键盘可能接管了。
            owner
                .update(async_cx, |this, cx| {
                    let guarded = target.update(cx, |state, _| {
                        state.hover_keyboard_takeover() || state.hover_pointer_inside()
                    });
                    if guarded {
                        return;
                    }
                    this.hover.hide_in(&target, cx);
                    cx.notify();
                })
                .ok();
        });
    }

    /// 指针离开了 hover 的**目标**（落进 gutter/行号栏，Zed
    /// `element/mouse.rs` 里 `text_hitbox` 不 hovered 的情形）。
    ///
    /// 判定本身在 [`hover::HoverState::off_target_action`]（Zed `hover_at(...
    /// None, position)` 的三支），这里只负责执行：
    ///
    /// 1. 没有卡片 → 作废在飞请求；
    /// 2. 在走远且计时器已排 → 不动它，让计时器跑完；
    /// 3. 在靠近 / 还没起表 → 重启延迟隐藏，给指针时间赶到浮层上。
    ///
    /// `mouse_position` 给 `None` 时判不了远近，一律按「不在靠近」处理 ——
    /// 与 Zed `if let Some(mouse_position) = ...` 的默认值一致。
    fn hover_pointer_off_target(
        &mut self,
        mouse_position: Option<gpui::Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.active_text_editor(cx) else {
            return;
        };
        // 键盘接管 / 指针已在卡片上：Zed 由 `hover_state.focused(..)` 与卡片的
        // `stop_propagation` 提前挡掉，我们放在这里省得白跑一遍距离计算。
        if target.read(cx).hover_keyboard_takeover() || target.read(cx).hover_pointer_inside() {
            return;
        }
        let action = {
            let state = target.read(cx);
            self.hover.off_target_action(
                state.hover_popover().is_some(),
                mouse_position,
                state.hover_popover_bounds(),
            )
        };
        match action {
            hover::OffTargetAction::CancelRequest => self.hover.cancel_request(&target, cx),
            hover::OffTargetAction::KeepHidingDelay => {}
            hover::OffTargetAction::ScheduleHide => self.schedule_hover_hide(cx),
        }
    }

    /// 取走后台算好的 hover / 补全 / 符号结果。
    ///
    /// 在 `render` 里调用：后台任务完成时只能把结果放进 `InputState` 的槽，
    /// 真正的浮层状态在 `Editor` 上，需要一个时机把它们搬过来。渲染前是最
    /// 自然的时机——不额外起任务，且保证下一帧就能看到。
    fn drain_lsp_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_text_editor(cx) else {
            return;
        };
        // 焦点离开编辑区（切到终端 / 状态栏菜单 / 查找框）时收起补全。
        // 菜单是 `defer_draw` 画在编辑区上的，不主动收会一直悬在窗口里。
        if self.completion.is_visible() && !editor.read(cx).focus_handle.is_focused(window) {
            self.completion.hide_in(&editor, cx);
        }
        // 只在「可见性变化」时记录：`drain_lsp_results` 每帧都跑，逐帧打
        // 会瞬间灌满日志文件（实测一次会话 6.9 万行），把有用线索冲掉。
        // 去重状态放在本 Editor 上（不用 `thread_local!`），理由见
        // `Editor::last_completion_visible_log`。
        {
            let visible = self.completion.is_visible();
            if self.last_completion_visible_log.replace(visible) != visible {
                log::info!("lsp: completion 可见性 -> {visible}");
            }
        }
        self.hover.take_pending(&editor, cx);
        // 先校验旧菜单是否还对得上当前编辑器/光标，再取新结果：
        // 点鼠标、方向键、切 tab 这些路径没有文本变更事件，只能在这里收口。
        self.completion.validate(&editor, cx);
        self.completion.take_pending(&editor, cx);
        // Enter 落到 `InputState::enter`（CompletionConfirm 键位没接住）时，
        // 这里补上确认动作。`enter` 已经吞掉了换行，不能让用户什么都得不到。
        if editor.update(cx, |state, _| state.take_completion_accept_request()) {
            log::info!("lsp: completion 处理 Enter/Tab 兜底请求");
            self.completion_confirm(&CompletionConfirm, window, cx);
        }
        if let Some(panel) = self.symbols_panel.clone() {
            let response = editor.update(cx, |state, _| state.take_pending_symbols());
            if let Some(response) = response {
                let symbols =
                    crate::lsp::features::flatten_symbols(editor.read(cx).text(), &response);
                panel.update(cx, |p, _| p.set_symbols(symbols));
            }
        }
    }

    /// 测试用：取某个已打开文件的光标位置（0 基行列）。
    #[cfg(test)]
    pub(crate) fn cursor_position_of_open_path(
        &self,
        path: &std::path::Path,
        cx: &App,
    ) -> Option<(usize, usize)> {
        let want = path.to_string_lossy();
        let file = self.open_files.iter().find(|f| f.path == want)?;
        let pos = file.editor_state.read(cx).cursor_position();
        Some((pos.line as usize, pos.character as usize))
    }

    /// 测试用：跳转之后**键盘焦点**有没有落在这个路径的编辑器上。
    ///
    /// 为什么单独立一条：`cursor_position_of_open_path` 那种断言（选区落在
    /// 目标行列）本来就有，却仍然漏掉了用户报的缺陷——选区设了也没用，
    /// **绘制与按键都以 focus 为门**，界面上看到的就是「只打开了文件」。
    #[cfg(test)]
    pub(crate) fn is_text_editor_of_path_focused(
        &self,
        path: &std::path::Path,
        window: &Window,
        cx: &App,
    ) -> bool {
        let want = path.to_string_lossy();
        self.open_files
            .iter()
            .find(|f| f.path == want)
            .filter(|f| {
                f.diff().is_none()
                    && f.image().is_none()
                    && f.markdown().is_none()
                    && f.live().is_none()
            })
            .is_some_and(|f| f.editor_state.read(cx).focus_handle.is_focused(window))
    }

    /// 测试用：把当前 tab 的 LSP 会话 uri 设成给定值。
    ///
    /// `open_lsp_location` 用「会话 uri 是否等于目标 uri」判断同文件跳转；
    /// 测试里没有真实服务器，所以直接注入。
    #[cfg(test)]
    pub(crate) fn set_session_uri_for_test(&mut self, uri: lsp_types::Url) {
        if let Some(id) = self.active_file_id.clone()
            && let Some(file) = self.open_files.iter_mut().find(|f| f.id == id)
            && let Some(session) = file.lsp.as_mut()
        {
            session.set_uri_for_test(uri);
        }
    }

    /// 测试用：直接走 `open_lsp_location` 这条跳转路径。
    #[cfg(test)]
    pub(crate) fn open_lsp_location_for_test(
        &mut self,
        location: &lsp_types::Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_lsp_location(location, None, window, cx);
    }

    /// 当前激活 tab 对应的文本编辑器（diff/图片/markdown 预览/实时返回 None）。
    pub(crate) fn active_text_editor(&self, _cx: &App) -> Option<Entity<InputState>> {
        let active_id = self.active_file_id.as_ref()?;
        let file = self.open_files.iter().find(|f| f.id == *active_id)?;
        if file.diff().is_some()
            || file.image().is_some()
            || file.markdown().is_some()
            || file.live().is_some()
        {
            return None;
        }
        Some(file.editor_state.clone())
    }

    /// 补全键的归属编辑器：不仅要是当前 tab，还必须**真的持有焦点**。
    ///
    /// 补全键无条件绑在 `Input` 上，因此编辑区内的查找框/重命名框也会
    /// 派发到这些 action；若只看 `completion.is_visible()`，用户在查找框按
    /// Enter 会把候选插进背后的代码编辑器。焦点不匹配时一律放行。
    fn focused_completion_editor(&self, window: &Window, cx: &App) -> Option<Entity<InputState>> {
        let editor = self.active_text_editor(cx)?;
        editor
            .read(cx)
            .focus_handle
            .is_focused(window)
            .then_some(editor)
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
        // 两个菜单不并存：开一个就关另一个。
        self.lsp_menu = None;
        self._lsp_menu_subscription = None;
        cx.notify();
    }

    /// 状态栏 LSP chip → 弹出**当前这一个服务器**的控制菜单。
    ///
    /// 菜单出现在 **chip 正上方**（见 `render_status_menu` 的
    /// `Anchor::BottomRight`）：状态栏在窗口底部，往上下开是唯一不会挡住
    /// 编辑区的方向。
    fn open_lsp_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::editor::lsp_menu::LspMenu;

        // 图片、diff 视图没有可分析的文本，点了不该弹东西。
        // 远程文件**要**弹：服务器跑在那台主机上，启停都在这个菜单里。
        let Some(tab_id) = self.active_file_id.clone() else {
            return;
        };
        if let Some(file) = self.open_files.iter().find(|f| f.id == tab_id)
            && (file.image().is_some() || file.diff().is_some())
        {
            return;
        }

        // 点开菜单是「用户正在等一个答案」的时刻，而候选链很可能还没探过
        // （UI 路径一律只读缓存：远端是网络往返，本机是子进程冷启动）。先交给
        // 后台补探：探完 `notify`，菜单与 chip 上的名字会从语言名补成真正的
        // 命令名。
        let active = self.open_files.iter().find(|f| f.id == tab_id).map(|f| {
            let language = f
                .lsp
                .as_ref()
                .map(|s| s.language_id().to_string())
                .or_else(|| f.editor_state.read(cx).language().map(|l| l.to_string()))
                .unwrap_or_default();
            (language, f.ssh_config.clone())
        });
        if let Some((language, ssh_config)) = active
            && !language.is_empty()
        {
            self.warm_lsp_chain(&language, ssh_config.as_ref(), true, cx);
        }

        let Some(snapshot) = self.active_lsp_snapshot(cx) else {
            return;
        };
        let weak = cx.entity().downgrade();
        let dismiss_weak = cx.entity().downgrade();
        // `LspMenu::new` 要 `Context<LspMenu>`，所以走 `cx.new` 而不能直接把
        // `Context<Editor>` 递过去。
        let menu = cx.new(|cx| LspMenu::new(weak, snapshot, cx));
        let subscription = window.subscribe(&menu, cx, move |_, _: &gpui::DismissEvent, _, cx| {
            let _ = dismiss_weak.update(cx, |this: &mut Editor, cx| {
                this.lsp_menu = None;
                this._lsp_menu_subscription = None;
                cx.notify();
            });
        });
        // 抢焦点：Escape 的 keybinding 挂在菜单的 key context 上，不聚焦就
        // 收不到。`track_focus` 只是「跟踪」，不等于「获得」。
        let handle = menu.read(cx).focus_handle();
        window.focus(&handle, cx);
        self.lsp_menu = Some(menu);
        self._lsp_menu_subscription = Some(subscription);
        self.lsp_menu_position = position;
        // 开 LSP 菜单时关掉语言菜单，两个浮层不叠在一起。
        self.language_menu = None;
        self._language_menu_subscription = None;
        cx.notify();
    }

    /// 让这台主机（本机或远端）的候选链在后台探完，探完再重画一次状态栏。
    ///
    /// UI 侧读的全是 `registry` 的缓存，且**绝不做 I/O**（见
    /// [`crate::lsp::LspManager::configured_command_on`]）——远端探测是一次
    /// 同步 SSH 往返，本机探测是一次子进程冷启动（冷机器上实测 5s+），而
    /// chip 每帧都要问一次服务器名，放在渲染路径上就等于把这些成本按帧交。
    /// 代价是第一次打开文件时「还不知道」，chip 只能退回语言名/「未启动」。
    /// 这里把探测交给后台线程：结果落回同一个缓存，任务完成时 `notify`
    /// 一次，chip 自己就补上真正的命令名。
    ///
    /// 只在「开标签 / 点开菜单」这类事件上调，不在 `render` 里调：待探列表
    /// 本身只是缓存读，但每帧 spawn 一个后台任务毫无意义。
    ///
    /// `allow_cooldown_retry` 只给「用户触发」的那次调用：冷却期结束后自动
    /// 补探一次，**只补一次**。不设上限的话，一次失败会变成每 30 秒一轮的
    /// 自我永续——每轮都是一次连不上主机的 SSH 超时。
    fn warm_lsp_chain(
        &mut self,
        language: &str,
        ssh_config: Option<&crate::ssh::SshConnectionConfig>,
        allow_cooldown_retry: bool,
        cx: &mut Context<Self>,
    ) {
        let pending = crate::lsp::LspManager::pending_chain_probe(cx, language, ssh_config);
        // 探测任务要活到后台线程结束，先把配置拷成自己的。
        let ssh = ssh_config.cloned();
        if pending.is_empty() {
            // 待探列表为空也可能是「主机不可达，正在 30 秒冷却期里」——那之后
            // 没人会再去问一次，chip 会一直停在「还不知道」。排一次延迟重试。
            if !allow_cooldown_retry {
                return;
            }
            let Some(delay) =
                crate::lsp::LspManager::chain_probe_cooldown(cx, language, ssh.as_ref())
            else {
                return;
            };
            // 守卫在这里就占住：冷却期里只允许存在一个重试定时器。
            let Some(guard) = crate::lsp::registry::begin_chain_probe(language, ssh.as_ref())
            else {
                return;
            };
            let language = language.to_string();
            cx.spawn(async move |this, cx| {
                let _guard = guard;
                cx.background_executor().timer(delay).await;
                // 释放占用，让真正的探测能拿到守卫。
                drop(_guard);
                if let Some(entity) = this.upgrade() {
                    entity.update(cx, |editor, cx| {
                        // 补探只补一次：这次再失败就等下一次用户触发。
                        editor.warm_lsp_chain(&language, ssh.as_ref(), false, cx);
                    });
                }
            })
            .detach();
            return;
        }
        // 同一 (主机, 语言) 只允许一个探测任务在飞：连开 N 个远端文件不该
        // 叠出 N 个把同一串超时各撞一遍的任务。守卫随任务移动，任务结束
        // （或被取消）时自动注销。
        let Some(guard) = crate::lsp::registry::begin_chain_probe(language, ssh.as_ref()) else {
            return;
        };
        log::debug!("lsp: warming probe cache for {language}: {pending:?}");
        cx.spawn(async move |this, cx| {
            // 守卫活到任务结束：提前 drop 会让并发的另一次调用立刻再起一个。
            let _guard = guard;
            cx.background_spawn(async move {
                crate::lsp::registry::probe_commands_pending(&pending, ssh.as_ref());
            })
            .await;
            if let Some(entity) = this.upgrade() {
                entity.update(cx, |editor, cx| {
                    editor.reconcile_lsp_placeholder(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 一个标签归属的服务器根：**只读已知信息，绝不联网**。
    ///
    /// 优先级：
    /// 1. 会话已有的根（连上过，或跳转打开时显式指定）。它是进程身份的权威
    ///    ——跳进 typeshed / 依赖源码的标签挂在发起跳转的那个项目上，按落点
    ///    路径重算会算出另一个 `ServerKey`，菜单于是显示「未启动」，点 Start
    ///    还会在存根目录旁另起一个进程。
    /// 2. 远端且根还没探到 → `None`。此时任何 `root_for` 调用都是一次最长
    ///    15s 的同步 SSH 往返，而这两个调用方（菜单快照、按 key 找标签）都
    ///    跑在 UI 线程上。根没探到也意味着这个标签的 `start_lsp_for_tab`
    ///    还挂在 `pending_root_attach` 里、没有会话可操作，跳过它是安全的。
    /// 3. 其余走 [`crate::lsp::project::root_for`]：本地是一次 `Path::exists`
    ///    遍历，远端是缓存命中（`remote_root_is_known` 已保证）。
    fn tab_server_root(&self, file: &EditorFile) -> Option<PathBuf> {
        if let Some(root) = file.lsp.as_ref().and_then(|s| s.server_root()) {
            return Some(root.to_path_buf());
        }
        if let Some(ssh) = file.ssh_config.as_ref()
            && !crate::lsp::project::remote_root_is_known(ssh, &file.path)
        {
            return None;
        }
        Some(crate::lsp::project::root_for(
            Path::new(&file.path),
            file.ssh_config.as_ref(),
        ))
    }

    /// 补探落空后，把「未启动」占位收回来。
    ///
    /// 占位是在「还不知道」时就挂上的：宁可先给一个能点的入口，也别把唯一
    /// 的开启入口藏起来。补探结束后若这门语言在这台主机上确实一个候选都
    /// 没有，占位就该收回——这正是同步探测时代 `language_has_server_on`
    /// 当场答 `false` 的效果。不补这一步，一台什么都没装的机器会留下一盏
    /// 点不动的灯，用户点「启动」只会失败（本地与远端同一逻辑）。
    ///
    /// 只动 `NotStarted`：真的起过进程的标签有自己的状态机（`Starting` /
    /// `Failed`……），那些是「用户已授权的操作」的结果，不该被这里抹掉。
    fn reconcile_lsp_placeholder(&mut self, cx: &mut Context<Self>) {
        for file in &self.open_files {
            let (language, is_placeholder) = {
                let state = file.editor_state.read(cx);
                (
                    state.language().map(|l| l.to_string()).unwrap_or_default(),
                    state.lsp_status() == Some(&crate::lsp::status::LspStatus::NotStarted),
                )
            };
            if language.is_empty() || !is_placeholder {
                continue;
            }
            // 还有候选没探完时 `true`（保持占位）；全部探明且都没有才 `false`。
            if crate::lsp::LspManager::language_has_server_on(
                cx,
                &language,
                file.ssh_config.as_ref(),
            ) {
                continue;
            }
            file.editor_state.update(cx, |state, _| {
                state.set_lsp_status(None);
            });
        }
    }

    /// 当前活动 tab 对应的服务器快照（没有服务器时为 `None`）。
    ///
    /// 快照里带 pid / 版本 / 状态，菜单与日志面板都靠它说话。
    ///
    /// 服务器还没起时**合成一个 `Stopped` 快照**而不是返回 `None`：默认关闭
    /// 下用户点状态栏 chip 的唯一目的就是「把它开起来」，此时没有快照就没菜单，
    /// 用户会被一个看得见却点不动的 chip 卡住。
    fn active_lsp_snapshot(&self, cx: &App) -> Option<crate::lsp::project::ServerSnapshot> {
        let tab_id = self.active_file_id.clone()?;
        let file = self.open_files.iter().find(|f| f.id == tab_id)?;
        // 语言以**会话**为准（服务器是按它起的），没有会话时退回标签自己的
        // 语言。必须由标签兜底：默认关闭时 `start_lsp_for_tab` 在拿不到服务器
        // 那条分支上就 `return` 了（见该函数），`file.lsp` 从未被写入——
        // 而 chip 存在的意义恰恰是这一态，点不开就等于把唯一的开启入口封死。
        // 曾经这里直接 `file.lsp.as_ref()?`，把下面那段「合成 Stopped 快照」
        // 变成不可达代码。
        let language = match file.lsp.as_ref() {
            Some(session) => session.language_id().to_string(),
            None => file
                .editor_state
                .read(cx)
                .language()
                .map(|l| l.to_string())
                .unwrap_or_default(),
        };
        if language.is_empty() {
            // 纯文本、没有语言：这门语言压根没有可接的服务器，菜单没什么可开的。
            return None;
        }
        // 根按归属算，但**不在 UI 线程上联网**：会话已有的根优先（跳转打开
        // 的文件挂的是发起方的根），远端根还没探到时直接放弃这次开菜单——
        // 那个标签此时还没有会话，chip 也不会显示，重试一次即可。
        // 口径统一由 [`Self::tab_server_root`] 提供。
        let Some(root) = self.tab_server_root(file) else {
            log::debug!(
                "lsp: skip menu, remote project root not known yet for {}",
                file.path
            );
            return None;
        };
        let key = crate::lsp::project::ServerKey {
            root,
            language,
            // 远端标签要按主机区分服务器：两台机器的同名项目根是两个进程。
            ssh_config: file.ssh_config.clone(),
        };
        if let Some(found) = crate::lsp::LspManager::snapshots(cx)
            .into_iter()
            .find(|s| s.key == key)
        {
            return Some(found);
        }
        // 表里没有：这门语言还没起过（默认关闭 / 用户停过）。菜单照样要能开，
        // 于是补一份最小快照——状态取 `Stopped`，菜单据此显示「Start Server」。
        Some(crate::lsp::project::ServerSnapshot {
            // 命令名按目标主机挑：菜单上显示的必须是那台机器上实际会起的那个。
            command: crate::lsp::LspManager::configured_command_on(
                cx,
                &key.language,
                key.ssh_config.as_ref(),
            )
            .unwrap_or_else(|| key.language.clone()),
            state: crate::lsp::project::ServerState::Stopped,
            key,
            pid: None,
            version: None,
        })
    }

    /// 打开某个服务器的报文日志面板（View Logs）。
    pub(crate) fn open_lsp_log_view(
        &mut self,
        key: &crate::lsp::project::ServerKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::editor::lsp_log_view::LspLogView;

        // 一次查表拿标题与状态：菜单点开的那个 key 一定在表里，但服务器可能
        // 刚被停掉，那时 state 会是 Stopped，面板顶栏照样说得清。
        let snapshot = crate::lsp::LspManager::snapshots(cx)
            .into_iter()
            .find(|s| &s.key == key);
        let title = snapshot
            .as_ref()
            .map(|s| s.command.clone())
            .unwrap_or_else(|| key.language.clone());
        let state = snapshot
            .map(|s| s.state)
            .unwrap_or(crate::lsp::project::ServerState::Stopped);

        let view = cx.new(|cx| LspLogView::new(key.clone(), title, state, cx));
        // 轮询在这里起而不是在 `LspLogView::new` 里：那会儿还没有实体句柄。
        crate::editor::lsp_log_view::spawn_refresh(&view, &mut cx.to_async());
        let subscription = cx.subscribe_in(&view, window, |this: &mut Editor, _, _, _, cx| {
            this.lsp_log_view = None;
            cx.notify();
        });
        self.lsp_log_view = Some(view);
        self._lsp_log_view_subscription = Some(subscription);
        cx.notify();
    }

    /// 找出所有「属于这个服务器 key」的标签。
    ///
    /// 判定用 `language` + 项目根一致：进程的唯一标识就是
    /// (项目根, 语言)，多个标签共享同一个进程（同一门语言开十个文件）。
    ///
    /// 根的比较按**标签自己的归属**算，且**不在 UI 线程上联网**：会话已有
    /// 的根优先，远端根还没探到的标签直接跳过（它们此刻还没有会话，见
    /// [`Self::tab_server_root`]）。口径与 `active_lsp_snapshot`、服务器表键
    /// 同源——否则这里判定的归属与菜单操作的 key 会错开。
    fn tabs_for_server_key(&self, key: &crate::lsp::project::ServerKey, cx: &App) -> Vec<String> {
        self.open_files
            .iter()
            .filter(|f| {
                // 归属必须一致：本地标签只归本地 key，远端标签只归同一台主机的 key。
                //
                // 按**主机身份**比而不是整份 `SshConnectionConfig`：`os` 是连上后
                // 才探测写回的，标签与 key 各自可能在「探测前/后」两个版本上，
                // 整份比较会把同一台主机的标签全部判成不属于这个 key——
                // 服务器起来了但一个标签都接不回来。
                if crate::lsp::project::host_identity(f.ssh_config.as_ref())
                    != crate::lsp::project::host_identity(key.ssh_config.as_ref())
                {
                    return false;
                }
                let Some(root) = self.tab_server_root(f) else {
                    // 远端根还没探到：这个标签还没有会话（见
                    // `start_lsp_for_tab` 的挂起分支），不属于任何 key。
                    return false;
                };
                root == key.root && f.lsp_language_matches(&key.language, cx)
            })
            .map(|f| f.id.to_string())
            .collect()
    }

    /// 断开某个 key 的所有会话（只发 `didClose`，进程由调用方回收）。
    ///
    /// **不在这里 `session.stop()`**：两个调用方（Stop / Restart）都已经先让
    /// `LspManager` 把进程关掉了。再对每个标签调一次 `stop()` 就是对同一个
    /// 共享 client 并发 N+1 次 `shutdown`——每个克隆都可能在别人 `take()`
    /// 之前看到「进程还在」，于是各发一遍 `shutdown`/`exit` 并各等到超时。
    /// 这里只摘掉编辑器手里的 client 并发 `didClose`，不再重复关进程。
    fn stop_sessions_for_key(
        &mut self,
        key: &crate::lsp::project::ServerKey,
        cx: &mut Context<Self>,
    ) {
        for tab_id in self.tabs_for_server_key(key, cx) {
            let Some(idx) = self.open_files.iter().position(|f| f.id.as_ref() == tab_id) else {
                continue;
            };
            let editor = self.open_files[idx].editor_state.clone();
            if let Some(session) = self.open_files[idx].lsp.as_mut() {
                log::debug!("lsp: closing session for {} ({})", tab_id, key.language);
                session.close();
            }
            // 服务器停了，它上一次发布的诊断已经没有任何来源可以更新。
            // 留着会让用户继续看到旧错误，且版本门控还停在旧会话上。
            editor.update(cx, |state, _| state.reset_lsp_diagnostics());
        }
    }

    /// 把属于某个 key 的所有标签状态置为「已停止」。
    fn mark_server_stopped(
        &mut self,
        key: &crate::lsp::project::ServerKey,
        cx: &mut Context<Self>,
    ) {
        for tab_id in self.tabs_for_server_key(key, cx) {
            let Some(idx) = self.open_files.iter().position(|f| f.id.as_ref() == tab_id) else {
                continue;
            };
            self.open_files[idx].editor_state.update(cx, |state, _| {
                state.set_lsp_status(Some(crate::lsp::status::LspStatus::Stopped));
            });
        }
    }

    /// 停止一个服务器（菜单操作，不依赖当前打开哪个文件）。
    ///
    /// 两步都要做，只做任一个都会漏：
    /// - `LspManager`：把进程从全局表里摘掉并打 stopped 标记，这样
    ///   「换个标签再切回来」不会把它复活；
    /// - `LspSession`：断开编辑器侧持有的 client。
    ///
    /// 日志桶一并回收（桶数无界，见 `lsp::log::remove_server`）。重启不走
    /// 这条路径——它调 `stop_server_for_key`，日志要留着排障。
    pub(crate) fn stop_lsp_server_for_key(
        &mut self,
        key: &crate::lsp::project::ServerKey,
        cx: &mut Context<Self>,
    ) {
        crate::lsp::LspManager::stop_server_for_key_and_purge_logs(cx, key);
        self.stop_sessions_for_key(key, cx);
        self.mark_server_stopped(key, cx);
        log::info!(
            "lsp: user stopped `{}` server for {:?}",
            key.language,
            key.root
        );
        cx.notify();
    }

    /// 启动一个服务器。
    ///
    /// 即使当前没有打开该语言的文件也要启动：进程进 `ready` 表，之后打开
    /// 第一个文件时能立刻握手，省掉一次冷启动（rust-analyzer 要几百毫秒）。
    ///
    /// **绕过全局开关**（`override_disabled`）：LSP 默认关闭，而这一条正是
    /// 用户开启它的入口。若这里也受全局开关约束，「Start Server」在默认
    /// 状态下点了没反应——用户看到 chip、点开菜单、按启动，什么都不会发生。
    /// 语言级的显式禁用不受影响（那是对某门语言的明确选择）。
    pub(crate) fn start_lsp_server_for_key(
        &mut self,
        key: &crate::lsp::project::ServerKey,
        cx: &mut Context<Self>,
    ) {
        crate::lsp::LspManager::resume_server_for_key(cx, key);
        // 先把进程拉起来；结果走 channel，握手完成前不阻塞 UI。
        crate::lsp::LspManager::server_for_key_with_override(cx, key, true);
        // 有关联标签就顺便接上会话（didOpen 要全文）。
        for tab_id in self.tabs_for_server_key(key, cx) {
            self.start_lsp_for_tab(&tab_id, cx);
        }
        log::info!(
            "lsp: user started `{}` server for {:?}",
            key.language,
            key.root
        );
        cx.notify();
    }

    /// 重启一个服务器：先停 → 解标记 → 重新握手。
    ///
    /// 顺序很重要：只解标记不真停的话，`server_for_key` 会把 `ready` 里那个
    /// 半死的 client 直接投递回来，等于没重启。
    ///
    /// 走 `stop_server_for_key`（而非带清桶的 Stop 变体）：重启通常就是为了
    /// 看报文排障，日志桶必须留着。
    ///
    /// 同样绕过全局开关（理由见 [`Self::start_lsp_server_for_key`]）：重启是
    /// 用户显式操作，不该被默认关闭挡回去。
    pub(crate) fn restart_lsp_server_for_key(
        &mut self,
        key: &crate::lsp::project::ServerKey,
        cx: &mut Context<Self>,
    ) {
        crate::lsp::LspManager::stop_server_for_key(cx, key);
        self.stop_sessions_for_key(key, cx);
        crate::lsp::LspManager::resume_server_for_key(cx, key);
        // 先把进程拉起来（绕过全局开关）；下面再给每个标签接会话。
        crate::lsp::LspManager::server_for_key_with_override(cx, key, true);
        // 新会话走完整握手；旧会话在 Drop 时补发 didClose。
        for tab_id in self.tabs_for_server_key(key, cx) {
            self.start_lsp_for_tab(&tab_id, cx);
        }
        log::info!(
            "lsp: user restarted `{}` server for {:?}",
            key.language,
            key.root
        );
        cx.notify();
    }

    /// 当前活动 tab 的 LSP 客户端（无 LSP 时 None）。
    fn active_lsp_client(
        &self,
        _cx: &App,
    ) -> Option<std::sync::Arc<crate::lsp::client::LspClient>> {
        let id = self.active_file_id.clone()?;
        let file = self.open_files.iter().find(|f| f.id == id)?;
        file.lsp.as_ref()?.client().cloned()
    }

    /// 当前活动 tab 的 LSP 会话。
    fn active_lsp_session(&self) -> Option<&crate::lsp::session::LspSession> {
        let id = self.active_file_id.clone()?;
        let file = self.open_files.iter().find(|f| f.id == id)?;
        file.lsp.as_ref()
    }

    /// Status bar position text: `81% 141:28` / `Top 1:1` / `Bot 174:28`.
    fn format_status_position(line: usize, column: usize, total: usize) -> String {
        let total = total.max(1);
        let line = line.min(total.saturating_sub(1));
        let percent = if total <= 1 {
            0.0
        } else {
            line as f64 * 100.0 / (total - 1) as f64
        };
        let percent = percent.round() as usize;
        let prefix = if percent == 0 {
            "Top".to_string()
        } else if percent >= 100 {
            "Bot".to_string()
        } else {
            format!("{percent}%")
        };
        format!("{prefix} {}:{}", line + 1, column + 1)
    }

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
        let language_id = state.language().map(|l| l.to_string()).unwrap_or_default();
        let language_label = Language::from_id(&language_id).display_name().to_string();
        // 就绪态 chip 上要显示服务器名（"rust-analyzer" 而不是"就绪"）。
        //
        // 每帧查一次：候选表的判定结果已按命令名缓存，所以这里只是一次
        // HashMap 查找；换来的是改语言/改设置后立刻生效，不必额外维护缓存。
        //
        // **按归属挑**：远端标签要问那台机器装了什么，否则 chip 上显示的是
        // 本机的服务器名，与远端实际跑的不是一个（本地 pyright、远端 ruff
        // 时会显示成 pyright）。
        let lsp_server_name = if language_id.is_empty() {
            None
        } else {
            crate::lsp::LspManager::configured_command_on(
                cx,
                &language_id,
                file.ssh_config.as_ref(),
            )
        };
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
                    .flex_grow_1()
                    .gap(px(8.0))
                    // Vim 开关 + 模式徽章：放在最左，与 vim 的状态行位置一致。
                    // 未启用时也显示，点击即可开启——Linux 默认关闭，用户不必
                    // 手改 settings.json 才能用 `:`。
                    .child({
                        let vim_label = state
                            .vim
                            .as_ref()
                            .map(|vim| vim.mode.indicator())
                            .unwrap_or("Vim");
                        div()
                            .id("vim-status-chip-hit")
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(1.0))
                            .rounded(px(4.0))
                            .when_some(state.vim.as_ref(), |this, _| {
                                this.bg(theme.accent.opacity(0.15)).text_color(theme.accent)
                            })
                            .when(state.vim.is_none(), |this| {
                                this.bg(theme.panel_background.opacity(0.4))
                            })
                            .hover(|this| this.bg(theme.tab_hover_background))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                                    this.toggle_vim(window, cx);
                                }),
                            )
                            .child(vim_label)
                    })
                    // Ex command line sits right after the vim mode badge.
                    .when_some(self.ex_command_panel.clone(), |this, panel| {
                        this.child(div().flex_grow_1().min_w(px(120.0)).child(panel))
                    })
                    .when(selected_len > 0, |this| {
                        this.child(
                            div()
                                .text_color(theme.text_muted.opacity(0.8))
                                .child(format!("已选 {selected_len} 字")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    // 语言服务器实时状态：转圈（工作时）+ 阶段名/进度/诊断计数。
                    // 没有 LSP 的文件（远程、无服务器）不占位。
                    //
                    // 点击弹出服务器控制菜单（启动/停止/重启/查看日志）——
                    // 状态栏是用户唯一能注意到 LSP 异常的地方，光显示不处理
                    // 等于把用户逼去重启整个应用。
                    .when_some(state.lsp_status(), |this, status| {
                        this.child(
                            div()
                                .id("lsp-status-chip-hit")
                                .cursor_pointer()
                                .rounded(px(6.0))
                                .hover(|this| this.bg(theme.tab_hover_background))
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(
                                        move |this, event: &gpui::MouseDownEvent, window, cx| {
                                            this.open_lsp_menu(event.position, window, cx);
                                        },
                                    ),
                                )
                                .child(render_lsp_status_chip(
                                    status,
                                    lsp_server_name.as_deref(),
                                    &theme,
                                    &self.last_chip_log,
                                )),
                        )
                    })
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
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .text_color(theme.text_muted)
                            .child(Self::format_status_position(
                                position.line as usize,
                                position.character as usize,
                                total,
                            )),
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
        self.open_local_file(path, window, cx);
    }

    /// Terminal `Cmd+Click`: open local file and jump to `line:col`.
    pub fn open_file_at_line_col(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_local_file_at(path, window, cx, line, col);
    }

    /// Terminal `Cmd+Click`: open remote file and jump to `line:col`.
    pub fn open_remote_file_at_line_col(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_at(path, ssh_config, window, cx, line, col);
    }

    /// Apply a `line:col` jump to an already-open tab. No-op when `line` is
    /// `None` or the tab is still loading (caller should use `pending_jumps`).
    ///
    /// 跳转的落点包含**键盘焦点**：见 [`Self::focus_editor_tab`]。
    pub fn jump_to_line_col(
        &mut self,
        tab_id: &str,
        line: u32,
        col: Option<u32>,
        window: &mut Window,
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
            self.pending_jumps.insert(tab_id.to_string(), (line, col));
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
        self.focus_editor_tab(tab_id, window, cx);
    }

    /// LSP 位置跳转：按 UTF-16 列精确换算字节 offset。
    ///
    /// 不能复用 [`Self::jump_to_line_col`]：那条路径给终端 Cmd+Click 用，
    /// 列按 Unicode `char` 计数；LSP 的 `character` 是 UTF-16 code unit。
    /// 目标行在落点之前有 emoji（非 BMP）时，两者会差出若干列。
    fn jump_to_lsp_position(
        &mut self,
        tab_id: &str,
        position: lsp_types::Position,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.open_files.iter().find(|f| f.id.as_ref() == tab_id) else {
            return;
        };
        // 图片/二进制预览没有文本行概念：只切换、不跳转。
        if crate::editor::image_viewer::is_image_file(Path::new(&file.path)) {
            self.switch_to_file(tab_id, cx);
            return;
        }
        if file.is_loading {
            // 内容还没到，拿不到 rope；保留原始 LSP Position，加载完成后换算。
            self.pending_lsp_jumps.insert(tab_id.to_string(), position);
            self.switch_to_file(tab_id, cx);
            return;
        }
        let editor_state = file.editor_state.clone();
        editor_state.update(cx, |state, cx| {
            let offset = crate::lsp::features::position_to_offset(state.text(), position);
            let offset = offset.min(state.text().len());
            state.selected_range = (offset..offset).into();
            state.scroll_to_center(offset, cx);
            cx.notify();
        });
        self.switch_to_file(tab_id, cx);
        self.focus_editor_tab(tab_id, window, cx);
    }

    /// 按路径与后端（本地/远端主机）找一个已打开的标签。
    ///
    /// LSP 跳转打开目标文件后，标签 id 由打开路径与远端归属共同决定；直接
    /// 用 path 猜 id 会在远端或已打开标签上失效。这里按实际字段匹配。
    fn find_open_tab_by_path(
        &self,
        path: &str,
        ssh: Option<&SshConnectionConfig>,
    ) -> Option<String> {
        self.open_files
            .iter()
            .find(|f| {
                f.path == path
                    && match (&f.ssh_config, ssh) {
                        (None, None) => true,
                        (Some(a), Some(b)) => {
                            a.host == b.host && a.port == b.port && a.username == b.username
                        }
                        _ => false,
                    }
            })
            .map(|f| f.id.to_string())
    }

    /// 把键盘焦点交给这个标签的编辑器。
    ///
    /// 只设 `selected_range` 不够：**选区与光标的绘制都以 focus 为门**
    /// （`components/input/element.rs` 的 `layout_selections` 与
    /// `should_paint_cursor` 在非聚焦时直接返回），所以用户看到的是
    /// 「文件打开了，光标却没跳过去，接着敲键还打在原来那个文件里」。
    /// 对照 VSCode：`openCodeEditor` 不带 `preserveFocus`，目标编辑器所在
    /// 分组被激活（`editorGroupView.ts` 的 `activateGroup`）。
    fn focus_editor_tab(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.open_files.iter().find(|f| f.id.as_ref() == tab_id) else {
            return;
        };
        // 这几种标签渲染的不是 InputState（diff / 图片 / markdown 预览 /
        // 同屏实时编辑器各有自己的视图），聚焦一个屏幕上不存在的输入框等于
        // 把键盘焦点从界面上拿走。
        if file.diff().is_some()
            || file.image().is_some()
            || file.markdown().is_some()
            || file.live().is_some()
        {
            return;
        }
        let focus_handle = file.editor_state.read(cx).focus_handle.clone();
        window.focus(&focus_handle, cx);
    }

    fn take_pending_jump(&mut self, tab_id: &str) -> Option<(u32, Option<u32>)> {
        self.pending_jumps.remove(tab_id)
    }

    /// diff 标签 id：仓库标识 + 相对路径。
    /// 本地仓库用 root，远程用 SSH 目标（同一路径在两台服务器上是不同仓库）。
    /// 两种形态都以 `/{rel_path}` 结尾，`refresh_diff_for_path` 的后缀匹配
    /// 依赖这一性质。
    fn diff_tab_id(rel_path: &str, repo: &crate::editor::git_repository::GitRepository) -> String {
        match repo.ssh_config() {
            Some(config) => {
                format!(
                    "diff-{}@{}:{}/{}",
                    config.username, config.host, config.port, rel_path
                )
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
            if let Some(view) = file.diff().cloned() {
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
            content: TabContent::Diff {
                view: diff_view,
                repo: Some(diff_repo),
            },
            lsp: None,
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
            content: TabContent::Diff {
                view: diff_view,
                repo: diff_repo,
            },
            lsp: None,
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
        self.open_local_file_inner(
            path,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Edit,
                silent: false,
            },
        );
    }

    /// 在 Markdown 预览中双击：以同屏实时模式在新 tab 中打开同一文件。
    pub fn open_file_in_live_mode(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_local_file_inner(
            path,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Live,
                silent: false,
            },
        );
    }

    fn open_local_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_local_file_inner(
            path,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Preview,
                silent: false,
            },
        );
    }

    fn open_local_file_at(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
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
                self.jump_to_line_col(&tab_id, line, col, window, cx);
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
            self.open_local_file_inner(
                path,
                window,
                cx,
                OpenOptions {
                    mode: OpenMode::Preview,
                    silent: false,
                },
            );
            return;
        }
        if let Some(line) = line {
            self.pending_jumps.insert(tab_id.clone(), (line, col));
        }
        self.open_local_file_inner(
            path,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Preview,
                silent: false,
            },
        );
    }

    /// `silent` 透传给 [`Self::spawn_file_load`]：恢复会话时为 true，
    /// 加载失败只关标签、不弹模态框（见 [`Self::spawn_file_load`] 的文档）。
    fn open_local_file_inner(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        options: OpenOptions,
    ) {
        let edit_mode = options.mode == OpenMode::Edit;
        let live_mode = Self::default_live_mode(&path, edit_mode, options.mode == OpenMode::Live);
        let silent = options.silent;
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
            self.open_image_file(tab_id, path_str, path, None, window, cx);
            return;
        }

        let load_gen = self.begin_load(&tab_id);
        let path_for_task = path.clone();

        self.open_file_with_content(
            OpenFileRequest {
                tab_id: tab_id.clone(),
                path_str,
                content: String::new(),
                path,
                ssh_config: None,
                encoding: FileEncoding::Utf8,
                read_only: false,
                is_loading: true,
                edit_mode,
                live_mode,
            },
            window,
            cx,
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
                        // `update_in`：这一段的结尾要把跳转落点连**焦点**一起
                        // 应用（`take_pending_jump` → `jump_to_line_col`），而
                        // 聚焦需要 window。
                        let _ = weak.update_in(cx, |this, window, cx| {
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
                                if let Some(ref markdown) = file.markdown() {
                                    markdown.update(cx, |renderer, cx| {
                                        renderer.set_text(loaded.text.clone(), cx);
                                    });
                                }
                                if let Some(ref live) = file.live() {
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
                            // LSP deferred jump: the target text is available
                            // now, so the UTF-16 LSP position can be converted
                            // to an exact byte offset.
                            if let Some(position) = this.pending_lsp_jumps.remove(&tab_id) {
                                this.jump_to_lsp_position(&tab_id, position, window, cx);
                            } else if let Some((line, col)) = this.take_pending_jump(&tab_id) {
                                // Terminal Cmd+Click deferred jump: content just
                                // arrived, apply line:col now (clamped to total).
                                this.jump_to_line_col(&tab_id, line, col, window, cx);
                            }
                            // 加载完成才有内容：此时才接语言服务器（didOpen
                            // 需要把全文发给服务器）。远端同样要接——服务器
                            // 跑在那台主机上，`start_lsp_for_tab` 自己按标签
                            // 的 ssh_config 分流；漏掉这条的话远程文件永远
                            // 只在「同步加载完成」的那一次机会之外没有诊断。
                            this.start_lsp_for_tab(&tab_id, cx);
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

    /// 远程标签的 id：**必须带主机**。
    ///
    /// 只用路径会在两台主机之间撞车——同一份 checkout 在两台机器上路径完全
    /// 相同，于是第二台的主机打开 `a.rs` 会命中第一台已开的 tab：`switch_to_file`
    /// 把用户切到**另一台机器**的内容上，看起来在编辑远端文件，实际保存会写回
    /// 错的那台。带上 `user@host:port` 后，不同主机天然是不同标签
    /// （与 `ServerKey` 按主机分开的口径一致）。
    fn remote_tab_id(path: &str, ssh_config: &SshConnectionConfig, suffix: &str) -> String {
        format!(
            "editor-remote-{}-{path}{suffix}",
            ssh_config.connection_string()
        )
    }

    pub fn open_remote_file(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(
            path,
            ssh_config,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Preview,
                silent: false,
            },
        );
    }

    fn open_remote_file_at(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
        line: Option<u32>,
        col: Option<u32>,
    ) {
        let live_mode = Self::default_live_mode(std::path::Path::new(&path), false, false);
        let tab_id = Self::remote_tab_id(
            &path,
            &ssh_config,
            if live_mode {
                Self::MARKDOWN_LIVE_SUFFIX
            } else {
                ""
            },
        );
        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            if let Some(line) = line {
                self.jump_to_line_col(&tab_id, line, col, window, cx);
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
        if line.is_some() && crate::editor::image_viewer::is_image_file(std::path::Path::new(&path))
        {
            self.open_remote_file_inner(
                path,
                ssh_config,
                window,
                cx,
                OpenOptions {
                    mode: OpenMode::Preview,
                    silent: false,
                },
            );
            return;
        }
        if let Some(line) = line {
            self.pending_jumps.insert(tab_id.clone(), (line, col));
        }
        self.open_remote_file_inner(
            path,
            ssh_config,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Preview,
                silent: false,
            },
        );
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
                self.jump_to_line_col(&tab_id, line, col, window, cx);
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
        self.open_remote_file_inner(
            path,
            ssh_config,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Edit,
                silent: false,
            },
        );
    }

    /// 远程 Markdown 预览中双击：以同屏实时模式在新 tab 中打开同一文件。
    pub fn open_remote_file_in_live_mode(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(
            path,
            ssh_config,
            window,
            cx,
            OpenOptions {
                mode: OpenMode::Live,
                silent: false,
            },
        );
    }

    /// `silent` 透传给 [`Self::spawn_file_load`]：恢复会话时为 true，
    /// 加载失败只关标签、不弹模态框（见 [`Self::spawn_file_load`] 的文档）。
    fn open_remote_file_inner(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
        options: OpenOptions,
    ) {
        let edit_mode = options.mode == OpenMode::Edit;
        let live_mode = Self::default_live_mode(
            std::path::Path::new(&path),
            edit_mode,
            options.mode == OpenMode::Live,
        );
        let silent = options.silent;
        let suffix = if live_mode {
            Self::MARKDOWN_LIVE_SUFFIX
        } else if edit_mode {
            Self::MARKDOWN_EDIT_SUFFIX
        } else {
            ""
        };
        let tab_id = Self::remote_tab_id(&path, &ssh_config, suffix);

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
                OpenFileRequest {
                    tab_id: tab_id.clone(),
                    path_str: path.clone(),
                    content,
                    path: PathBuf::from(&path),
                    ssh_config: Some(ssh_config.clone()),
                    encoding,
                    read_only: false,
                    is_loading: false, // 有缓存内容
                    edit_mode,
                    live_mode,
                },
                window,
                cx,
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
                            // 影子文本与基线同口径比较（live 标签剥 `\r`），
                            // 否则 CRLF 的 md 永远命中不了这条「陈旧缓存刷新」。
                            && content_matches_baseline(
                                &file.editor_state.read(cx).value(),
                                &file.original_content,
                                file.live().is_some(),
                            );
                        if should_update {
                            file.original_content = fresh_text.clone();
                            file.encoding = fresh_enc;
                            file.editor_state
                                .update(cx, |st, cx| st.set_text(fresh_text.clone(), cx));
                            if let Some(md) = file.markdown() {
                                md.update(cx, |r, cx| r.set_text(fresh_text.clone(), cx));
                            }
                            if let Some(live) = file.live() {
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
            OpenFileRequest {
                tab_id: tab_id.clone(),
                path_str: path.clone(),
                content: String::new(),
                path: PathBuf::from(&path),
                ssh_config: Some(ssh_config),
                encoding: FileEncoding::Utf8,
                read_only: false,
                is_loading: true,
                edit_mode,
                live_mode,
            },
            window,
            cx,
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

    fn open_image_file(
        &mut self,
        tab_id: String,
        path_str: String,
        path: PathBuf,
        ssh_config: Option<SshConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let load_gen = self.begin_load(&tab_id);

        self.open_file_with_content(
            OpenFileRequest {
                tab_id: tab_id.clone(),
                path_str: path_str.clone(),
                content: String::new(),
                path: path.clone(),
                ssh_config: ssh_config.clone(),
                encoding: FileEncoding::Utf8,
                read_only: false,
                is_loading: true,
                edit_mode: false, // 图片没有编辑模式
                live_mode: false,
            },
            window,
            cx,
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
                            // 图片没有行列概念：丢弃可能残留的跳转。
                            this.pending_jumps.remove(&tab_id);
                            this.pending_lsp_jumps.remove(&tab_id);
                            if let Some(file) = this.open_files.iter_mut().find(|f| f.id == tab_id)
                            {
                                file.is_loading = false;
                                if let Some(ref viewer) = file.image() {
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

    fn open_file_with_content(
        &mut self,
        request: OpenFileRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let OpenFileRequest {
            tab_id,
            path_str,
            content,
            path,
            ssh_config,
            encoding,
            read_only,
            is_loading,
            edit_mode,
            live_mode,
        } = request;
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
            let mut state = InputState::new_code_editor(language.clone(), window, cx);
            state.set_value(&content, window, cx);
            state.disabled = is_image || is_loading || read_only;
            // 普通文件标签：行首 git 色条可点击展开 hunk
            // （diff 视图自己的编辑器已是展开态，保持默认 false）。
            state.set_git_expandable(true);
            // vim 状态随 tab 创建（`new_code_editor` 内）：每个 tab 一个
            // InputState，模式因此天然隔离。只有代码编辑器接受 vim，且开关
            // 关闭时不装——普通输入框永远不该拿到 vim 状态。
            state
        });

        let is_live_file =
            live_mode && !is_image && path.extension().and_then(|e| e.to_str()) == Some("md");
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
                    move |this,
                          _live_entity,
                          event: &crate::editor::markdown_live::LiveViewEvent,
                          cx| {
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
                    // LSP 文档同步：只在文本真的变了时发 didChange。
                    // 放在这个 O(1) 闸门内，避免每次光标移动都触发一轮
                    // 防抖任务（防抖本身也会救，但省掉更干净）。
                    let text = this.open_files[idx].editor_state.read(cx).text().clone();
                    if let Some(session) = this.open_files[idx].lsp.as_mut() {
                        session.on_text_changed(&text, cx);
                    }
                    // 自动补全：只在光标处于标识符内时触发。
                    // 不对每次击键都请求——否则敲注释/字符串也会打服务器，
                    // 而且候选会一直闪。
                    //
                    // 两个守卫任一不满足都会静默跳过，而「打字完全没有反应」
                    // 正是最难查的一类问题，所以未命中时要说清是哪一条挡住的。
                    let has_session = this.open_files[idx].lsp.is_some();
                    let is_active =
                        this.active_file_id.as_deref() == Some(sync_tab_id.as_str());
                    if !(has_session && is_active) {
                        log::info!(
                            "lsp: 跳过自动补全 has_session={has_session} is_active_tab={is_active} (active={:?} this={sync_tab_id})",
                            this.active_file_id
                        );
                    }
                    if has_session && is_active {
                        let editor = this.open_files[idx].editor_state.clone();
                        // 先用**已有候选**按新前缀本地过滤：LSP 往返通常几十
                        // 到几百毫秒，只等新响应会让菜单在这段时间里继续展示
                        // 与当前输入不匹配的旧候选（「候选内容错误」的主因之一）。
                        this.completion.refilter(&editor, cx);
                        // 触发判定（对照 Zed 的 `completion_query` 与
                        // `completion_triggers`）：
                        // 1. 光标前是「词字符」→ 按标识符补全；
                        // 2. 或者刚输入的字符是**服务器声明的触发字符**
                        //    （`completionProvider.triggerCharacters`，如
                        //    Rust 的 `:`、CSS 的 `@`）→ 带 triggerCharacter
                        //    请求，服务器据此返回对应类别的候选。
                        //
                        // 此前只用第 1 条且把 `.` 硬编码成词字符，于是
                        // 「输入语法匹配不准确」，且 `:`/`@`/`<` 这些服务器
                        // 明确要求的触发点永远不会请求补全。
                        let client = this.active_lsp_client(cx);
                        // 光标前一字符必须走 `char_before`：它用 `RopeExt`
                        // 做边界吸附。`text.char_at(cursor - 1)` 在光标跟在
                        // 中文/emoji 后面时会落在字符中间并 panic。
                        let prev_char = char_before(editor.read(cx).text(), editor.read(cx).cursor());

                        // 服务器声明的触发字符集合。
                        let declared_triggers: Vec<String> = client
                            .as_ref()
                            .map(|c| c.completion_trigger_characters())
                            .unwrap_or_default();

                        let trigger_character = prev_char.and_then(|c| {
                            let s = c.to_string();
                            declared_triggers.iter().any(|t| t == &s).then_some(s)
                        });

                        let in_identifier = prev_char
                            .map(|c| {
                                crate::lsp::features::char_kind(
                                    c,
                                    crate::lsp::features::DEFAULT_EXTRA_WORD_CHARS,
                                ) == crate::lsp::features::CharKind::Word
                            })
                            .unwrap_or(false);

                        if in_identifier || trigger_character.is_some() {
                            let uri = this
                                .active_lsp_session()
                                .map(|s| s.uri().clone());
                            log::info!(
                                "lsp: 补全触发判定命中 in_identifier={in_identifier} trigger={trigger_character:?} 有session={}",
                                uri.is_some()
                            );
                            if let Some(uri) = uri {
                                this.completion.trigger_auto(
                                    &editor,
                                    client,
                                    uri,
                                    trigger_character,
                                    cx,
                                );
                            }
                        } else {
                            // 光标离开标识符且非触发字符：收起候选
                            // （比如刚打完空格）。
                            this.completion.hide_in(&editor, cx);
                        }
                    }
                }

                // 只是移动了光标（点击 / 方向键 / 拖选），或文本变了但
                // 没走到上面的补全分支：菜单必须立刻收起。`drain_lsp_results`
                // 里的 `validate` 只在 Editor 重新渲染时跑，而光标移动通常
                // 只让 InputState 重绘，不保证 Editor 也重绘——菜单会视觉上
                // 「粘」在原地，此时按 Enter 还会按新光标位置确认旧候选。
                if this.active_file_id.as_deref() == Some(sync_tab_id.as_str())
                    && this.completion.is_visible()
                {
                    let cursor = state_entity.read(cx).cursor();
                    if this.completion.anchor() != cursor {
                        this.completion.hide_in(&state_entity, cx);
                        cx.notify();
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
                            && f.markdown().is_some()
                    })
                {
                    let content = state_entity.read(cx).value().to_string();
                    let previews: Vec<Entity<crate::editor::markdown::Markdown>> = this
                        .open_files
                        .iter()
                        .filter(|f| f.id.as_ref() != sync_tab_id.as_str() && f.path == sync_path)
                        .filter_map(|f| f.markdown().cloned())
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
            // 相对图片路径以文档所在目录为基准。
            let md_doc_dir = if ssh_config.is_none() {
                path.parent().map(std::path::Path::to_path_buf)
            } else {
                None
            };
            Some(cx.new(|cx| {
                crate::editor::markdown::Markdown::new_with_dir(content_clone, md_doc_dir, cx)
            }))
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
            content: if let Some(image) = image {
                TabContent::Image(image)
            } else if let Some(live) = live {
                TabContent::Live(live)
            } else if let Some(markdown) = markdown {
                TabContent::MarkdownPreview(markdown)
            } else {
                TabContent::Code
            },
            lsp: None,
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

        // LSP：标签建好后异步接语言服务器（本地与远端都接）。
        // 加载中的文件没有内容，等加载完成再接（见 `finish_file_load`）。
        if !is_loading {
            self.start_lsp_for_tab(&tab_id, cx);
        }

        self.switch_to_file(&tab_id, cx);
        cx.notify();
    }

    /// 收敛 [`Self::lsp_jump_roots`]：只留还开着的标签，外加刚记下的 `keep`。
    ///
    /// 每条记录的作用只在「目标文件被打开并接上 LSP」那一刻——之后归属就存在
    /// 会话自己的 `server_root` 里了，表里这份只是留给「同一文件再次打开」用的。
    /// 长会话里反复跳转会让它无界增长（每个落点一条），所以超过标签数一截就扫
    /// 一遍；没超过就不扫，免得每次跳转都重建一遍路径集合。
    ///
    /// 匹配只看路径、不看主机：同一台主机上的同一个路径才是同一个文件，而
    /// 关掉的标签已经不存在，留着哪条主机的记录都没用。
    fn prune_lsp_jump_roots(&mut self, keep: &(String, Option<(String, u16, String)>)) {
        if self.lsp_jump_roots.len() <= self.open_files.len() + 8 {
            return;
        }
        let open_paths: HashMap<String, ()> = self
            .open_files
            .iter()
            .map(|f| (f.path.to_string(), ()))
            .collect();
        let keep = keep.clone();
        self.lsp_jump_roots
            .retain(|key, _| *key == keep || open_paths.contains_key(&key.0));
    }

    /// 为一个标签接上语言服务器（本地或远端）。
    ///
    /// 全流程异步且可失败：拿到服务器后 didOpen，再订阅诊断。任何一步失败
    /// 都只记日志，编辑器退回纯 tree-sitter 状态（诊断仍由语法树产出）。
    fn start_lsp_for_tab(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        // 测试可关掉 LSP 接入：`server_for_key` 会 `smol::spawn` 到全局
        // 执行器（真实线程），而 gpui 的 `TestScheduler` 会把「其它线程有活动」
        // 判为非确定性并 abort 整个测试二进制。测试关心的是跳转/打开逻辑，
        // 不需要真实语言服务器。
        #[cfg(test)]
        if TEST_DISABLE_LSP.with(|f| f.get()) {
            return;
        }
        let Some(idx) = self.open_files.iter().position(|f| f.id.as_ref() == tab_id) else {
            return;
        };
        // 图片、diff 视图不走 LSP（没有可分析的文本内容）。
        // 远程文件**走**：服务器跑在那台主机上，见 `LspSession` 的 ssh_config。
        if self.open_files[idx].image().is_some() || self.open_files[idx].diff().is_some() {
            return;
        }
        let language = self.open_files[idx]
            .editor_state
            .read(cx)
            .language()
            .map(|l| l.to_string())
            .unwrap_or_default();
        if language.is_empty() {
            return;
        }
        let path = PathBuf::from(&self.open_files[idx].path);
        // 远端主机的配置：None 表示本地文件。
        let ssh_config = self.open_files[idx].ssh_config.clone();
        // 把这台主机（本机或远端）的候选链交给后台探一次（见 `warm_lsp_chain`）。
        // 本函数跑在 UI 线程，而下面紧接着要判「该不该给 chip 占位」——那里
        // 只读缓存，第一次多半还不知道，占位先挂着，探完 notify 再补名字；
        // 探完若发现一个候选都没有，`reconcile_lsp_placeholder` 会把占位收回。
        self.warm_lsp_chain(&language, ssh_config.as_ref(), true, cx);
        // 跳转打开的文件（`.pyi` 存根、依赖源码……）该挂哪个根**已经知道**了：
        // 用发起跳转的那个会话的根，别让落点自己另算一个（见 `lsp_jump_roots`）。
        let jump_root = self
            .lsp_jump_roots
            .get(&lsp_jump_root_key(
                &self.open_files[idx].path,
                ssh_config.as_ref(),
            ))
            .cloned();
        // 远端标签：项目根还不知道就先挂起，别在 UI 线程上等那趟往返。
        //
        // `ServerKey.root` 是进程身份的一部分，而远端的根只能在那台机器上探
        // （逐个父目录 `stat` 找 `Cargo.toml`/`pyproject.toml` 之类的标记）。
        // 下面 `session.connect` 会经 `key_for_file` 走到它，本机文件
        // 是一次 `Path::exists`，远端则是一次同步 SSH 往返——主机慢或不通时，
        // 打开文件这一步就是十几秒的界面冻结。
        //
        // 挂起不等于放弃：探测交给后台，落地后重走本函数。终止性由
        // `remote_project_root` 保证——每条出路要么缓存根，要么记下「这次问不到」
        // 的失败，两者都让 `remote_root_is_known` 返回真，第二次就不再 defer。
        // 那时若仍没有根，`root_for` 退回路径本身的目录（与之前行为一致）。
        //
        // 已经带着根来的（`jump_root`）不在这条路上：既不用探，也就不会挂起。
        if jump_root.is_none()
            && let Some(ssh) = ssh_config.as_ref()
            && !crate::lsp::project::remote_root_is_known(ssh, &self.open_files[idx].path)
        {
            // 已经有一次探测在飞：等它落地后由那边的回调重走本函数。
            // **不能落到下面的 `connect`**——那里的 `key_for_file` 会在
            // UI 线程上做同一趟同步往返，正是这条挂起逻辑要避免的。
            if self.pending_root_attach.iter().any(|t| t == tab_id) {
                return;
            }
            self.pending_root_attach.push(tab_id.to_string());
            let path_for_probe = self.open_files[idx].path.clone();
            let ssh_for_probe = ssh.clone();
            let tab = tab_id.to_string();
            log::debug!("lsp(remote): deferring attach for {tab} until project root is known");
            cx.spawn(async move |this, cx| {
                cx.background_spawn(async move {
                    crate::lsp::project::remote_project_root(&ssh_for_probe, &path_for_probe);
                })
                .await;
                let Some(entity) = this.upgrade() else {
                    return;
                };
                entity.update(cx, |editor, cx| {
                    editor.pending_root_attach.retain(|t| t != &tab);
                    editor.start_lsp_for_tab(&tab, cx);
                });
            })
            .detach();
            return;
        }
        // 记下路径：跨文件跳转时用它判断「是不是同一个文件」。
        self.open_files[idx].editor_state.update(cx, |state, _| {
            state.set_lsp_file_path(path.display().to_string())
        });
        let editor = self.open_files[idx].editor_state.downgrade();

        let Some(mut session) =
            crate::lsp::session::LspSession::new(&path, &language, editor, ssh_config)
        else {
            return;
        };
        // 目标主机配置：从会话里取（`ssh_config` 的所有权已交给会话）。
        // 下面两处都要按它判定——「有没有服务器」与「失败时报哪个命令名」。
        let tab_ssh_config = session.ssh_config().cloned();
        // 跳转打开的文件：挂回**发起跳转的那个服务器**。
        if let Some(root) = jump_root {
            session.set_server_root(root);
        }
        let Some(rx) = session.connect(cx) else {
            // 拿不到服务器。三种原因里只有一种要在状态栏上占位：
            //
            // - 默认关闭（`lsp.enabled = false`）或用户手动停过 → 占位显示
            //   服务器名，chip 可点，点开菜单就能启动；
            // - 这门语言没有可配的服务器 → 真的没有 LSP，不占位（否则
            //   每个 .txt 文件旁边都挂一盏点不动的灯）；
            // - 之前启动失败 → 由 `Failed` 分支自己记账，不在这条路上。
            //
            // 分界靠「目标主机上有没有一个能跑起来的候选」：远端标签要问
            // 那台机器，否则本机装了 pyright、远端什么都没装时也会显示
            // 「未启动」，用户点了启动只会失败。
            let has_server = crate::lsp::LspManager::language_has_server_on(
                cx,
                &language,
                tab_ssh_config.as_ref(),
            );
            log::info!("lsp: no server for {language} ({path:?}), has_server={has_server}");
            if has_server {
                self.open_files[idx].editor_state.update(cx, |state, _| {
                    state.set_lsp_status(Some(crate::lsp::status::LspStatus::NotStarted));
                });
            }
            return;
        };
        log::debug!("lsp: connecting {language} for {path:?}");
        // 状态栏：进程已 spawn，正在握手。
        self.open_files[idx].editor_state.update(cx, |state, _| {
            state.set_lsp_status(Some(crate::lsp::status::LspStatus::Starting));
        });
        // 失败原因要按**目标主机**挑命令名（见下面）。远端标签的 ssh_config
        // 从会话里取——会话是唯一同时拿着路径与主机配置的地方。
        // 必须在 `lsp = Some(session)` 之前取：那次移动之后就拿不到了。
        let status_ssh_config = tab_ssh_config;
        self.open_files[idx].lsp = Some(session);

        // 等服务器就绪（spawn + initialize 握手）。
        let tab_id = tab_id.to_string();
        let language_for_status = language.clone();

        cx.spawn(async move |this, cx| {
            let client = rx.recv().await.ok().flatten();
            let Some(client) = client else {
                // 启动失败：状态栏给出可读原因，而不是停在「启动中」。
                let _ = this.update(cx, |this, cx| {
                    let Some(idx) = this.open_files.iter().position(|f| f.id.as_ref() == tab_id)
                    else {
                        return;
                    };
                    // 指名具体命令：只说「rust 服务器启动失败」，用户不知道
                    // 缺的是哪个二进制。**不断言原因**——这条分支同时覆盖
                    // 「二进制不存在」与「握手失败/服务器立刻退出」，写成
                    // 「找不到 X」在后者上是错的。精确原因在日志里。
                    //
                    // 按目标主机挑命令名：远端启动失败时报的必须是那台机器上
                    // 实际尝试的那个（候选链挑中的），而不是本机装的那个。
                    let reason = match crate::lsp::LspManager::configured_command_on(
                        cx,
                        &language_for_status,
                        status_ssh_config.as_ref(),
                    ) {
                        Some(command) => format!("{command} 启动失败"),
                        None => format!("{language_for_status} 服务器启动失败"),
                    };
                    this.open_files[idx].editor_state.update(cx, |state, _| {
                        state
                            .set_lsp_status(Some(crate::lsp::status::LspStatus::Failed { reason }));
                    });
                    cx.notify();
                });
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(idx) = this.open_files.iter().position(|f| f.id.as_ref() == tab_id) else {
                    return;
                };
                let text = this.open_files[idx].editor_state.read(cx).text().clone();
                // 先克隆实体句柄：下面要可变借 `lsp`，同时借 `open_files`
                // 会撞借用检查。
                let editor = this.open_files[idx].editor_state.clone();
                let file_path = this.open_files[idx].path.clone();
                let Some(session) = this.open_files[idx].lsp.as_mut() else {
                    return;
                };
                log::debug!("lsp: didOpen {file_path} ({} bytes)", text.len());
                session.on_connected(client, &text);
                // 新会话的版本从 0 重新开始：清掉旧会话的诊断与版本门控，
                // 否则旧 applied version 会把新服务器的诊断全判为过期。
                editor.update(cx, |state, _| state.reset_lsp_diagnostics());
                // 诊断监听要在会话拿到 client 之后建。
                session.spawn_diagnostic_listener(&editor, cx);
                // 握手完成 → 就绪。若服务器随后开始索引，它会用
                // `$/progress` 把状态推成 Working。
                editor.update(cx, |state, _| {
                    state.set_lsp_status(Some(crate::lsp::status::LspStatus::Ready {
                        errors: 0,
                        warnings: 0,
                    }));
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn switch_to_file(&mut self, id: &str, cx: &mut Context<Self>) {
        // 切 tab 前收起旧 tab 的补全菜单：`CompletionState` 挂在 Editor 上
        // （不随 tab 走），不清的话菜单锚点/替换区间会属于旧文件，确认时
        // 把候选插进新文件。点当前 tab 不算切换，保留菜单。
        if self.active_file_id.as_deref() != Some(id)
            && let Some(previous) = self.active_text_editor(cx)
        {
            self.completion.hide_in(&previous, cx);
        }
        self.active_file_id = Some(id.into());
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.set_active_tab(id, cx);
        });
        cx.notify();
    }

    /// 判断文件当前是否有未保存的修改（编辑器文本 vs 磁盘基线）。
    fn file_is_dirty(&self, file: &EditorFile, cx: &mut App) -> bool {
        let state = file.editor_state.read(cx);
        // 普通编辑器：直接与基线逐字节比较，不构造 `value()` 的全文副本。
        // 同长度替换（改写字符、替换选中内容、撤销回原样）是每键都会走到的
        // 路径，10 MB 文件就是每键一次 10 MB 分配 + 拷贝。
        if file.live().is_none() {
            return !state.text_equals(&file.original_content);
        }
        // live 标签的影子文本是归一化（剥 `\r`）的，基线保留原始行尾，
        // 字节口径不同，仍交给同口径的比较。
        !content_matches_baseline(state.value().as_ref(), &file.original_content, true)
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
            .filter(|f| f.diff().is_none())
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
            if let Some(view) = file.diff().cloned()
                && (file.path == path || file.id.as_ref().ends_with(suffix.as_str()))
            {
                view.update(cx, |v, cx| v.refresh(cx));
            }
        }
    }

    /// 刷新所有已打开的 diff 页（提交后 HEAD 整体前移）。
    pub fn refresh_all_diff_views(&mut self, cx: &mut Context<Self>) {
        let views: Vec<Entity<crate::editor::diff_view::DiffView>> = self
            .open_files
            .iter()
            .filter_map(|f| f.diff().cloned())
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
    fn on_live_change(&mut self, live_tab_id: &SharedString, version: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.open_files.iter().position(|f| f.id == *live_tab_id) else {
            return;
        };
        // O(1)：版本号对比，绝不 join 全文。
        let dirty = self.open_files[idx]
            .live()
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
                .timer(std::time::Duration::from_millis(
                    Self::LIVE_SHADOW_SYNC_DEBOUNCE_MS,
                ))
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
        let Some(live) = self.open_files[idx].live().cloned() else {
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
            let Some(live) = file.live().cloned() else {
                continue;
            };
            let text = live.read(cx).text();
            if file.editor_state.read(cx).text_equals(&text) {
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
        if let Some(diff) = self.open_files[idx].diff().cloned() {
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
        // live 标签的文本按模型口径（剥 `\r`）与基线比较：行尾差异不算修改，
        // 不该触发一次空写盘。
        if content_matches_baseline(&text, &original, self.open_files[idx].live().is_some()) {
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
                            // LSP：didSave 让服务器重读磁盘（构建脚本/宏展开
                            // 之类依赖文件内容的分析会跟着更新）。放在这里
                            // 是因为下面就要调 `&mut self` 的方法，`file`
                            // 的可变借用到此为止。
                            if let Some(session) = file.lsp.as_ref() {
                                let text = file.editor_state.read(cx).text().clone();
                                session.on_saved(&text);
                            }
                            if let Some(live) = file.live() {
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
        self.pending_lsp_jumps.remove(id);
        self.editor_state_subscriptions.remove(id);
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            // 关闭的是当前 tab：先收起它的补全菜单，否则 `CompletionState`
            // 会保留一个指向已移除 InputState 的可见状态。
            if self.active_file_id.as_deref() == Some(id) {
                let editor = self.open_files[index].editor_state.clone();
                self.completion.hide_in(&editor, cx);
            }
            // LSP：发 didClose 并停掉该 tab 的同步/监听任务。
            // `close()` 内部会 take 掉任务，Drop 时不会再重复发一次。
            if let Some(session) = self.open_files[index].lsp.as_mut() {
                session.close();
            }
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
        if let Some(from) = from_index
            && from != to_index
        {
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
        // 搬移后台算好的 LSP 结果（hover / 补全）：后台任务只能把结果写进
        // `InputState` 的槽，浮层状态在 `Editor` 上，渲染前搬到本帧可见。
        self.drain_lsp_results(window, cx);
        // 光标处的符号高亮：跟着光标走，渲染前发请求（有位置闸门，不会每帧发）。
        self.request_document_highlight(cx);
        self.request_inlay_hints(cx);
        self.request_signature_help(cx);
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
            let base = div().h(px(30.0)).w_full().px(px(6.0)).child(inner);
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
                    if let Some(diff) = file.diff() {
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
                    } else if let Some(image) = file.image() {
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
                    } else if let Some(live) = file.live() {
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
                    } else if let Some(markdown) = file.markdown() {
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
                                    // Cmd+Click 跳定义。挂在**容器**上而不是输入框
                                    // 内部：那里拿不到 uri/client，也碰不到 Editor 的
                                    // 跳转管线。输入框自己的按下监听更深一层，先跑完
                                    // （它把光标移到点击处），我们随后接上——与 VSCode
                                    // 「setPosition 之后再发请求」同序。
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(
                                            move |this,
                                                  event: &gpui::MouseDownEvent,
                                                  window,
                                                  cx| {
                                                this.goto_definition_at_click(event, window, cx);
                                            },
                                        ),
                                    )
                                    // LSP 悬浮提示：鼠标停稳后请求，浮层跟随命中字符。
                                    // 用 `on_mouse_move` 而不是 input 内部的事件：
                                    // 后者既没有 uri/client，也不能碰 Editor 的浮层状态。
                                    .on_mouse_move(cx.listener(
                                        move |this, event: &gpui::MouseMoveEvent, _window, cx| {
                                            let Some(editor) = this.active_text_editor(cx) else {
                                                return;
                                            };
                                            // 浮层被键盘接管时，鼠标移动完全不参与：
                                            // 既不重新查询也不隐藏。
                                            // 对照 Zed `element/mouse.rs:466`：它开头
                                            // 就是 `if editor.hover_state.focused(..) { return }`。
                                            if editor.read(cx).hover_keyboard_takeover() {
                                                return;
                                            }
                                            // **指针在 hover 卡片上时不再向底下发请求。**
                                            //
                                            // 主机制是卡片根节点的 `.occlude()`
                                            // （`hover.rs::render_hover_card`，照抄
                                            // Zed `InfoPopover::render`）：命中测试
                                            // 走到卡片的 hitbox 就停住，编辑器容器的
                                            // hitbox 根本不在 hovered 列表里，本监听器
                                            // 压根不会触发 —— 这是 Zed 的做法（它
                                            // `.occlude()` 与 `stop_propagation` 两个
                                            // 都挂）。
                                            //
                                            // 这里保留几何判断作**兜底**，因为浮层是
                                            // `defer_draw` 到窗口层的，它的 hitbox 要
                                            // 等到自己那一帧的 prepaint 才登记，早于该
                                            // 帧的命中测试里还没有它；而且 `is_hovered`
                                            // 只看「在不在命中列表里」，
                                            // `stop_propagation` 又注册在本监听器之后
                                            // （Bubble 阶段我们先跑），拦不住。
                                            //
                                            // 漏掉这层的后果：`index_for_mouse_position`
                                            // 把卡片上的坐标映射成编辑器里的某个字符，
                                            // 在那儿发一次 hover 请求，拿到「无内容」后
                                            // `clear_hover_popover` 把卡片擦掉 —— 表现为
                                            // **指针一移到卡片上，卡片立即消失**。
                                            if editor
                                                .read(cx)
                                                .hover_popover_contains(event.position)
                                            {
                                                return;
                                            }
                                            // 指针回到编辑区 = 离开了浮层。复位这个标记，
                                            // 否则隐藏逻辑会一直以为指针还在卡片上，
                                            // 浮层就再也收不掉了。
                                            if editor.read(cx).hover_pointer_inside() {
                                                editor.update(cx, |state, cx| {
                                                    state.set_hover_pointer_inside(false);
                                                    cx.notify();
                                                });
                                            }
                                            // 只取命中偏移。浮层的**像素位置在绘制阶段**
                                            // 用当前帧的行布局算（照抄 Zed 的
                                            // `layout_hover_popovers`）——在这里预先算
                                            // 像素会随滚动/软换行过期，正是之前位置错乱的根因。
                                            //
                                            // 取不到偏移 = 指针不在正文上（gutter），
                                            // 走 Zed `hover_at(..., None, position)` 那一支。
                                            let offset = match editor
                                                .read(cx)
                                                .hover_offset_for_mouse(event.position)
                                            {
                                                Some(offset) => offset,
                                                None => {
                                                    this.hover_pointer_off_target(
                                                        Some(event.position),
                                                        cx,
                                                    );
                                                    cx.notify();
                                                    return;
                                                }
                                            };
                                            let uri =
                                                this.active_lsp_session().map(|s| s.uri().clone());
                                            let Some(uri) = uri else {
                                                // 没有 LSP 会话：把可能还挂着的浮层收掉。
                                                this.schedule_hover_hide(cx);
                                                return;
                                            };
                                            let client = this.active_lsp_client(cx);
                                            // 鼠标 hover 显示的是注释卡；此后
                                            // 签名帮助不能再把它当作自己的卡片。
                                            this.signature_help_active = false;
                                            this.signature_hover_anchor = None;
                                            this.hover
                                                .on_mouse_moved(offset, &editor, client, uri, cx);
                                            cx.notify();
                                        },
                                    ))
                                    // 鼠标移出编辑区：收起浮层（否则它会一直挂着）。
                                    // 键盘接管中不收 —— 用户正用 hjkl 看内容，
                                    // 点外部只是顺手，浮层必须留着。
                                    .on_mouse_down_out(cx.listener(
                                        move |this, _: &gpui::MouseDownEvent, _window, cx| {
                                            let Some(editor) = this.active_text_editor(cx) else {
                                                return;
                                            };
                                            if editor.read(cx).hover_keyboard_takeover() {
                                                return;
                                            }
                                            this.hover.hide_in(&editor, cx);
                                            cx.notify();
                                        },
                                    ))
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
                                    })
                                    // LSP 补全菜单：与 hover 同样在绘制阶段定位
                                    // （`TextElement::paint_completion_menu`）。
                                    // LSP 重命名输入：顶部居中
                                    .when_some(self.rename_panel.clone(), |this, panel| {
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
                                    })
                                    // LSP 符号大纲：顶部居中列表
                                    .when_some(self.symbols_panel.clone(), |this, panel| {
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
                            // LSP 报文日志（View Logs）：底部居中，盖在状态栏上方。
                            // 跟上面几个面板不同，它要的是「贴底」而不是「贴顶」
                            // ——那是状态栏 chip 的方向，日志从那儿长出来最自然。
                            .when_some(self.lsp_log_view.clone(), |this, panel| {
                                this.child(
                                    div()
                                        .absolute()
                                        .bottom(px(8.0))
                                        .left_0()
                                        .right_0()
                                        .flex()
                                        .justify_center()
                                        .child(panel),
                                )
                            })
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
        let (anim_start, anim_end, anim_ms) = (
            self.panel_anim_start,
            self.panel_anim_end,
            self.panel_anim_ms,
        );
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
                .on_action(cx.listener(|this, _: &ToggleVim, window, cx| {
                    this.toggle_vim(window, cx);
                }))
                .on_action(cx.listener(|this, _: &ExCommandLine, window, cx| {
                    this.open_ex_command(&ExCommandLine, window, cx);
                }))
                .on_action(cx.listener(|this, _: &GotoDefinition, window, cx| {
                    this.goto_definition(&GotoDefinition, window, cx);
                }))
                .on_action(cx.listener(|this, _: &GotoDeclaration, window, cx| {
                    this.goto_declaration(&GotoDeclaration, window, cx);
                }))
                .on_action(cx.listener(|this, _: &GotoTypeDefinition, window, cx| {
                    this.goto_type_definition(&GotoTypeDefinition, window, cx);
                }))
                .on_action(cx.listener(|this, _: &GotoImplementation, window, cx| {
                    this.goto_implementation(&GotoImplementation, window, cx);
                }))
                .on_action(cx.listener(|this, _: &NavigateBack, window, cx| {
                    this.navigate_back(&NavigateBack, window, cx);
                }))
                .on_action(cx.listener(|this, _: &FindReferences, window, cx| {
                    this.find_references(&FindReferences, window, cx);
                }))
                .on_action(cx.listener(|this, _: &FormatDocument, window, cx| {
                    this.format_document(&FormatDocument, window, cx);
                }))
                .on_action(cx.listener(|this, _: &TriggerCompletion, window, cx| {
                    this.trigger_completion(&TriggerCompletion, window, cx);
                }))
                .on_action(cx.listener(|this, _: &CompletionNext, window, cx| {
                    this.completion_next(&CompletionNext, window, cx);
                }))
                .on_action(cx.listener(|this, _: &CompletionPrev, window, cx| {
                    this.completion_prev(&CompletionPrev, window, cx);
                }))
                .on_action(cx.listener(|this, _: &CompletionFirst, window, cx| {
                    this.completion_first(&CompletionFirst, window, cx);
                }))
                .on_action(cx.listener(|this, _: &CompletionLast, window, cx| {
                    this.completion_last(&CompletionLast, window, cx);
                }))
                .on_action(cx.listener(|this, _: &CompletionConfirm, window, cx| {
                    this.completion_confirm(&CompletionConfirm, window, cx);
                }))
                .on_action(cx.listener(|this, _: &CompletionCancel, window, cx| {
                    this.completion_cancel(&CompletionCancel, window, cx);
                }))
                .on_action(cx.listener(|this, _: &RenameSymbol, window, cx| {
                    this.toggle_rename(&RenameSymbol, window, cx);
                }))
                .on_action(cx.listener(|this, _: &DocumentSymbols, window, cx| {
                    this.toggle_symbols(&DocumentSymbols, window, cx);
                }))
                .on_action(cx.listener(|this, _: &Hover, window, cx| {
                    this.toggle_hover(&Hover, window, cx);
                }))
                .on_action(cx.listener(|this, _: &CodeAction, window, cx| {
                    this.apply_code_action(&CodeAction, window, cx);
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
        // 状态栏 chip 的弹出菜单（语言 / LSP 共用 `render_status_menu`）。
        //
        // 关闭回调在建树前备好：它们要捕获 Editor 的弱句柄，而 `when_some`
        // 的闭包里再 `cx.entity()` 会把 `cx` 的借用和 `self` 的借用搅在一起。
        let editor_weak = cx.entity().downgrade();
        let close_language_menu = {
            let weak = editor_weak.clone();
            std::rc::Rc::new(move |_window: &mut Window, cx: &mut gpui::App| {
                weak.update(cx, |editor: &mut Editor, cx| {
                    editor.language_menu = None;
                    editor._language_menu_subscription = None;
                    cx.notify();
                })
                .ok();
            })
        };
        let close_lsp_menu = {
            let weak = editor_weak.clone();
            std::rc::Rc::new(move |_window: &mut Window, cx: &mut gpui::App| {
                weak.update(cx, |editor: &mut Editor, cx| {
                    editor.lsp_menu = None;
                    editor._lsp_menu_subscription = None;
                    cx.notify();
                })
                .ok();
            })
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
                this.child(render_status_menu(
                    &menu,
                    self.language_menu_position,
                    window,
                    close_language_menu.clone(),
                ))
            })
            .when_some(self.lsp_menu.clone(), |this, menu| {
                this.child(render_status_menu(
                    &menu,
                    self.lsp_menu_position,
                    window,
                    close_lsp_menu.clone(),
                ))
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
                this.child(render_status_menu(
                    &menu,
                    self.language_menu_position,
                    window,
                    close_language_menu.clone(),
                ))
            })
            .when_some(self.lsp_menu.clone(), |this, menu| {
                this.child(render_status_menu(
                    &menu,
                    self.lsp_menu_position,
                    window,
                    close_lsp_menu.clone(),
                ))
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

/// 编辑器文本与磁盘基线是否等价（`file_is_dirty` 等比较的统一口径）。
///
/// 普通编辑器保留原始行尾，精确比较即可。live markdown 标签不同：
/// `markdown_live::model` 的行切分会剥掉所有 `\r`（见 `split_lines`），
/// 影子 `InputState`（保存 / 关闭确认读的那份）同步的也是归一化文本，
/// 而 `original_content` 保留磁盘原始行尾。两者直接比较时，Windows 上
/// CRLF 的 md 文件「打开即已修改」——关闭时弹出未保存确认，实际一个字都没改。
/// live 标签比较前按模型口径去掉 `\r`，行尾差异不算修改。
fn content_matches_baseline(text: &str, original: &str, is_live: bool) -> bool {
    text == original || (is_live && text.replace('\r', "") == original.replace('\r', ""))
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

    if let Some(dir) = parent
        && let Ok(f) = std::fs::File::open(dir)
    {
        let _ = f.sync_all();
    }
    Ok(())
}

/// 状态栏上的语言服务器状态 chip。
///
/// 状态栏 chip 的（文字, 灯色）——纯决策，便于单测。
///
/// 抽出来是因为「默认关闭」这个产品决策正好落在这一处分支上：未启动态
/// 要显示**服务器名**（用户据此知道这个文件本来能接哪个 LSP、点什么能开），
/// 而 `Failed` 要显示原因。两者混在一个 `match` 里，只有渲染时才跑得到，
/// 测不了。
pub(crate) fn lsp_chip_content(
    status: &crate::lsp::status::LspStatus,
    server_name: Option<&str>,
) -> (String, LspChipTone) {
    use crate::lsp::status::LspStatus;

    match status {
        // 就绪 / 已停止 / 未启动都显示服务器名：状态由灯色表达，文字位留给名字。
        LspStatus::Ready { errors, warnings } => {
            let tone = if *errors > 0 {
                LspChipTone::Error
            } else if *warnings > 0 {
                LspChipTone::Warning
            } else {
                LspChipTone::Ok
            };
            // 没有名字时退回**状态本身**，而不是硬写「未启动」：Ready 分支下
            // 进程明明活着（远端候选链还没探完时就会这样），报「未启动」是把
            // 用户往反方向指。
            (
                server_name
                    .map(str::to_string)
                    .unwrap_or_else(|| status.label()),
                tone,
            )
        }
        // 停止是用户的选择，不报警；未启动没有任何进程，灯不亮。
        LspStatus::Stopped => (
            server_name
                .map(str::to_string)
                .unwrap_or_else(|| status.label()),
            LspChipTone::Muted,
        ),
        LspStatus::NotStarted => (
            server_name.unwrap_or("未启动").to_string(),
            LspChipTone::Dim,
        ),
        LspStatus::Starting | LspStatus::Working { .. } => (status.label(), LspChipTone::Busy),
        LspStatus::Failed { .. } => (status.label(), LspChipTone::Error),
    }
}

/// chip 是否点灯：**按状态判断，不按颜色分档**。
///
/// 灯表示「这个文件挂着一台服务器，它现在稳定了」——`Ready` 与 `Stopped`
/// 都点，两者靠灰度区分（见 `LspChipTone::Muted`）；忙的时候用转圈，不用灯。
/// 曾经它挂在 tone 上，于是 `Ready { errors > 0 }` 归到 `Error` 档 → 不画灯——
/// 最需要被看见的状态反倒成了唯一没有灯的态。实测现象：Rust 文件（0 诊断）有
/// 绿点，Python 文件（basedpyright 报了 59 条）只有红字、没有点，用户据此以为
/// 「Python 的 LSP 没起来」。
///
/// `Failed` / `NotStarted` 依然不点灯：那时压根没有可挂的进程。
pub(crate) fn lsp_chip_shows_lamp(status: &crate::lsp::status::LspStatus) -> bool {
    use crate::lsp::status::LspStatus;
    matches!(status, LspStatus::Ready { .. } | LspStatus::Stopped)
}

/// chip 的颜色分档（是否点灯见 [`lsp_chip_shows_lamp`]，是否转圈看 `Busy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LspChipTone {
    /// 有进程在忙：转圈 + 强调色。
    Busy,
    /// 干净就绪：绿灯。
    Ok,
    /// 有警告：黄灯。
    Warning,
    /// 有错误 / 启动失败：红灯。
    Error,
    /// 用户主动停止：最淡的一档，灯仍呼吸。
    Muted,
    /// 未启动：没有进程，灯不亮。
    Dim,
}

/// 状态栏上的 LSP chip。
///
/// 视觉分档：
/// - **就绪**：**服务器名**（rust-analyzer / gopls …）+ 涟漪指示灯。灯芯的
///   颜色即状态：绿=干净、黄=有警告、红=有错误，外圈一圈圈从中心扩散。
///   用名字 + 一盏"活着"的灯代替原来的绿字"就绪"——名字才是用户排查时
///   真正需要的（哪个进程在服务这个文件），"就绪"两个字不携带这个信息；
/// - **工作中/启动中**：转圈图标 + 阶段名（+ 百分比），配色用主题强调色；
/// - **失败**：红色文字 + 原因；
/// - **未启动**（默认关闭时的初始态）：服务器名 + 淡色文字，**不点灯**——
///   没有进程可点。这是用户发现「这个文件可以接 LSP」并点开菜单开启的入口。
///
/// 就绪与已停止都走「涟漪灯 + 服务器名」：服务器名才是用户排查时真正需要的
/// 信息（哪个进程在服务这个文件），两种状态只差灯的颜色。
///
/// 外层由调用方包上点击命中区（`lsp-status-chip-hit`），这个函数只管内容，
/// 免得同一个 `id` 被渲染二遍。
///
/// `server_name`：当前语言的服务器命令名（如 `rust-analyzer`），只有就绪态
/// 会用到；该语言没有可用服务器时为 `None`，退化成原来的"就绪"。
fn render_lsp_status_chip(
    status: &crate::lsp::status::LspStatus,
    server_name: Option<&str>,
    theme: &crate::theme::UiTheme,
    last_log: &std::cell::RefCell<Option<(String, crate::lsp::status::LspStatus)>>,
) -> gpui::AnyElement {
    let (label, tone) = lsp_chip_content(status, server_name);
    // 状态栏是「用户唯一能看到 LSP 状态的地方」，它不变化就没有任何线索。
    // 这里只在标签真的变化时记录（`render` 每帧都跑，逐帧打会淹掉日志）。
    // 去重状态由调用方（每个 Editor 一份）传入，理由见
    // `Editor::last_chip_log`。
    //
    // 键必须带上**状态**：`NotStarted` / `Stopped` / `Ready` 的文案都是服务器
    // 名，只比文案的话「用户点了 Stop」「服务器起来了但一片红」这两条在日志
    // 里都不留痕迹——本次排查就是靠这条日志定位的，差点被这个盲区带偏。
    {
        let mut last = last_log.borrow_mut();
        let changed = match last.as_ref() {
            Some((prev_label, prev_status)) => prev_label != &label || prev_status != status,
            None => true,
        };
        if changed {
            log::info!("lsp-status: {label}   (raw={status:?})");
            *last = Some((label.clone(), status.clone()));
        }
    }
    let color = match tone {
        // 有进程在忙：转圈 + 强调色。
        LspChipTone::Busy => theme.accent,
        LspChipTone::Ok => theme.success,
        LspChipTone::Warning => theme.warning,
        LspChipTone::Error => theme.error,
        // 停止是用户的选择，不是故障：用最淡的一档，不报警。灯照旧呼吸——
        // 「还在跑」和「被停了」靠灰度区分，不靠静止。
        LspChipTone::Muted => theme.text_muted.opacity(0.6),
        // 未启动：没有进程，**不画灯**。灯意味着「有个东西活着」。
        LspChipTone::Dim => theme.text_muted.opacity(0.85),
    };
    let show_lamp = lsp_chip_shows_lamp(status);
    let show_spinner = tone == LspChipTone::Busy;

    let mut chip = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(6.0))
        .text_color(color);

    if show_spinner {
        // 转圈：与 tab 栏的 loading 图标同一套动画参数（800ms 匀速旋转）。
        chip = chip.child(
            gpui::svg()
                .size(px(11.0))
                .text_color(color)
                .path(crate::assets::Icon::Loader.path())
                .with_animation(
                    "lsp-status-spinner",
                    gpui::Animation::new(Duration::from_millis(800)).repeat(),
                    |this, delta| {
                        this.with_transformation(gpui::Transformation::rotate(gpui::percentage(
                            delta,
                        )))
                    },
                ),
        );
    } else if show_lamp {
        // 就绪 / 已停止共用一盏涟漪灯：颜色区分状态，动画表示"活着"。
        chip = chip.child(crate::editor::lsp_lamp::lamp(color));
    }

    chip.child(label).into_any_element()
}

/// 判断 `prepareRename` 的错误是否属于「服务器明确拒绝重命名」。
///
/// LSP 规范没有为「不可重命名」定义专用错误码，实践中服务器回
/// `InvalidParams`(-32602)，消息形如 "No references found at position"
/// （rust-analyzer 实测）。
///
/// 与「传输/超时失败」区分开很重要：前者应直接告诉用户不可重命名，
/// 后者才该退回按词边界猜的老行为。混为一谈会让用户在关键字上看到输入框、
/// 输完却没有任何效果。
pub(crate) fn is_rename_rejection(err: &anyhow::Error) -> bool {
    let msg = format!("{err}");
    // 明确的语义拒绝。
    msg.contains("No references found")
        || msg.contains("cannot be renamed")
        || msg.contains("not renameable")
        || msg.contains("InvalidParams")
        || msg.contains("-32602")
}

// 测试用：关掉 LSP 接入（见 `Editor::start_lsp_for_tab`）。
// 用 `//` 而不是 `///`：下面是宏调用，rustdoc 不收宏调用的文档注释。
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_DISABLE_LSP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 测试用：在闭包内关掉 LSP 接入。
///
/// 打开文件会尝试连接语言服务器，而那条路径用 `smol::spawn`（全局执行器，
/// 真实线程）——gpui 的 `TestScheduler` 会因此判定测试非确定性并 abort
/// 整个二进制。跳转/打开类测试不关心 LSP，关掉即可。
#[cfg(test)]
pub(crate) fn with_lsp_disabled<R>(f: impl FnOnce() -> R) -> R {
    TEST_DISABLE_LSP.with(|flag| {
        let prev = flag.replace(true);
        let out = f();
        flag.set(prev);
        out
    })
}

/// 跳转历史里的一项：从哪个文件的哪个偏移跳出去的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavLocation {
    /// 标签 id（`EditorFile::id`）。
    pub tab_id: String,
    /// 跳转发生时的光标字节偏移。
    pub offset: usize,
}

/// 跳转历史的容量上限。
///
/// 不设上限的话长时间使用会无限增长（每次跳转都压栈）。100 条足够覆盖
/// 任何真实的「往回翻」需求，与多数编辑器的默认值同量级。
pub(crate) const NAV_HISTORY_LIMIT: usize = 100;

/// 跳转种类（四个动作只有请求方法不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LspGotoKind {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
}

/// 发起跳转的那个会话的归属，在**发请求之前**捕获。
///
/// 结果是在 `cx.spawn_in` 里 await 之后才回到 UI 线程的，那时用户可能已经
/// 切了标签。若到那时再问 `active_lsp_session()`，拿到的是**新标签**的会话，
/// 两处会错：
/// - 跨文件时按错的归属开文件——远端结果去本机磁盘上找一个不存在的路径
///   （表现为「跳了但什么也没发生」），本地结果反过来去开一个远程标签；
/// - 同文件判定比的是新标签的 uri，跨标签的跳转会被误判成原地跳。
/// 所以归属必须随请求一起走，不能事后重问。
#[derive(Debug, Clone)]
struct LspJumpOrigin {
    uri: lsp_types::Url,
    ssh_config: Option<crate::ssh::SshConnectionConfig>,
    /// 发起跳转的会话挂在哪个项目根上（= 它的 `ServerKey.root`）。
    ///
    /// 跳转的落点常常在**那个工作区里**而不在当前文件目录下（typeshed 存根、
    /// registry 里的依赖源码）。目标文件因此要挂回同一个根，否则会在落点旁边
    /// 另起一个服务器（见 [`Editor::lsp_jump_roots`]）。
    ///
    /// `None` = 会话还没连上（`connect` 之后才有根），退回按路径自己算。
    root: Option<PathBuf>,
    /// 发起请求时活动标签的 id。
    ///
    /// 「同文件原地跳」必须落回**这个**标签：`same_file` 是用上面的 uri 算
    /// 的，而用户在等待响应期间可以切走。没有它就只能跳当前活动标签，光标
    /// 会进错文件（`uri` 无法反查出标签：同一文件在远端/本地、或两个文件树
    /// 分组里可以有多个 id）。
    tab_id: Option<SharedString>,
}

/// [`Editor::lsp_jump_roots`] 的键：目标文件路径 + 它所在的主机（本地为 `None`）。
///
/// 主机必须在键里：同一个绝对路径（`/srv/app/main.py`）在两台主机上是两个
/// 文件，各自的服务器也是两个进程。这与 `ServerKey` 用
/// `(root, language, 主机)` 认身份是同一个理由。
pub(crate) fn lsp_jump_root_key(
    path: &str,
    ssh: Option<&crate::ssh::SshConnectionConfig>,
) -> (String, Option<(String, u16, String)>) {
    (
        path.to_string(),
        ssh.map(|c| (c.host.clone(), c.port, c.username.clone())),
    )
}

/// LSP 跳转该怎么做（纯决策，便于单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LspJumpAction {
    /// 目标就是当前文件：原地跳。
    JumpInPlace { position: lsp_types::Position },
    /// 目标在别的文件（或尚未打开）：打开它并定位。
    OpenFile {
        path: PathBuf,
        position: lsp_types::Position,
    },
    /// 无法处理（非 `file://` uri）。
    Ignore,
}

/// 由 `Location` 与「是否当前文件」决定跳转动作。
///
/// 对照 Zed 的 `go_to_definition_of_kind`：它拿到 `Location` 后一律打开目标
/// buffer 再选中区间，**不区分**同文件/跨文件。这里区分只是为了省一次
/// 「打开」往返，语义等价。
///
/// 之所以抽成纯函数：跨文件跳转的失败模式（旧实现直接 return）在真实
/// 绘制/文件系统环境里很难断言，而这个决策本身就是唯一的分支点。
///
/// 位置保留 LSP 原始的 0 基 `Position`：它的 `character` 是 UTF-16 code
/// unit，不能直接交给按 Unicode char 计列的 `jump_to_line_col`。真正的
/// 字节 offset 在目标文本可用后由 `position_to_offset` 换算。
pub(crate) fn lsp_location_action(
    location: &lsp_types::Location,
    same_file: bool,
) -> LspJumpAction {
    let position = location.range.start;

    if same_file {
        return LspJumpAction::JumpInPlace { position };
    }
    match crate::lsp::client::uri_to_path(&location.uri) {
        Some(path) => LspJumpAction::OpenFile { path, position },
        None => LspJumpAction::Ignore,
    }
}

/// Cmd+Click 跳定义的触发条件（纯判定，便于单测）。
///
/// 照抄 VSCode `gotoSymbol/browser/link/clickLinkGesture.ts` 的三条守卫：
/// 左键、单次按下（对应它的 `event.detail <= 1`——双击/三击各有语义）、
/// 触发键在**按下那一刻**就按住（它记 `_hasTriggerModifierOnMouseDown`，
/// 避免拖选途中按下修饰键把选区当成一次跳转）。
///
/// macOS 的触发键是 Cmd、其余平台是 Ctrl，这正是
/// [`gpui::Modifiers::secondary`] 的语义。VSCode 默认
/// `editor.multiCursorModifier: "alt"`，把 Cmd/Ctrl 让给了跳转（实测其
/// 安装包里的默认值也是 `"altKey"`）。
pub(crate) fn is_goto_definition_click(event: &gpui::MouseDownEvent) -> bool {
    event.button == gpui::MouseButton::Left && event.modifiers.secondary() && event.click_count <= 1
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
            // `.pyi` 是 Python 的**类型声明存根**（typeshed / stub 包），
            // 语法就是 Python，`List[int]` 这类跳转的落点正是它。
            // 原来只认 `py`，于是点开一个存根得到 `text`：高亮没了、`#`
            // 注释切换没了、LSP 也不接（`text` 不在语言表里，没有候选服务器，
            // chip 都不出现）——用户看到的「跳到了 .pyi
            // 但编辑器不认这个文件」就是这一条。语言 id 保持 `python`
            // （与 pyright/basedpyright 的 `didOpen` 口径一致，它们把存根
            // 也当 python 文档处理）。
            "py" | "pyi" => "python",
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

fn shell_process(command: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut process = std::process::Command::new("cmd");
        process.arg("/C").arg(command);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = std::process::Command::new("sh");
        process.arg("-c").arg(command);
        process
    }
}

/// Ex 命令的行范围：把 0-based 行号（含末行）换算成字节区间。
fn ex_line_byte_range(text: &ropey::Rope, start: usize, end: usize) -> std::ops::Range<usize> {
    let last = text.lines_len().saturating_sub(1);
    let start = start.min(last);
    let end = end.min(last);
    let start_byte = text.line_start_offset(start);
    let end_byte = if end < last {
        text.line_start_offset(end + 1)
    } else {
        text.len()
    };
    start_byte..end_byte
}

fn ex_goto_row(editor: &Entity<InputState>, row: usize, cx: &mut App) {
    editor.update(cx, |state, cx| {
        let row = row.min(state.text().lines_len().saturating_sub(1));
        let offset = state.text().line_start_offset(row);
        state.selected_range = (offset..offset).into();
        state.scroll_to_center(offset, cx);
        cx.notify();
    });
}

fn ex_replace_range(
    editor: &Entity<InputState>,
    range: std::ops::Range<usize>,
    text: &str,
    cursor: usize,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |state, cx| {
        let utf16 = state.range_to_utf16(&range);
        state.replace_text_in_range_silent(Some(utf16), text, window, cx);
        let pos = cursor.min(state.text().len());
        state.selected_range = (pos..pos).into();
        state.scroll_to_center(pos, cx);
        cx.notify();
    });
}

fn ex_registers_message(cx: &App) -> String {
    let entries = crate::vim::Registers::global(cx).entries();
    if entries.is_empty() {
        return "No registers".into();
    }
    entries
        .into_iter()
        .map(|(name, value)| {
            let mut preview = value.replace('\n', "\\n");
            if preview.len() > 120 {
                preview.truncate(120);
                preview.push('…');
            }
            format!("{name}  {preview}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// vim 替换串的 `\1` / `&` 转成 regex crate 的 `$1` / `$0`。
fn vim_replacement_to_regex(replacement: &str) -> String {
    let chars: Vec<char> = replacement.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
            out.push('$');
            out.push(chars[i + 1]);
            i += 2;
        } else if chars[i] == '&' {
            out.push_str("$0");
            i += 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) =
            cx.add_window_view(|window, cx| EditorHost(cx.new(|cx| Editor::new(window, cx))));
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
            dirty: false,
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
                    && f.live().is_some()
                    && f.live()
                        .is_some_and(|live| live.read(cx).text() == "# hi\n")
            });
            assert!(
                live_ok,
                "live tab must keep its LiveMarkdownView after restore"
            );
        });
    }

    /// 回归：Windows 上 CRLF 的 md 用 live 模式打开、一字未改，关闭确认
    /// 不得判为「有未保存的更改」。
    ///
    /// live 模型（`split_lines`）会剥掉 `\r`，关闭前把防抖文本落进影子
    /// `InputState` 时写入的就是 LF 归一化文本；此前它与保留 CRLF 行尾的
    /// `original_content` 直接字符串比较，未修改也被判脏，弹「有未保存的更改」。
    #[gpui::test]
    fn live_tab_with_crlf_content_is_clean_without_edits(cx: &mut gpui::TestAppContext) {
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
        let (host, cx) =
            cx.add_window_view(|window, cx| EditorHost(cx.new(|cx| Editor::new(window, cx))));
        let editor = host.read_with(cx, |h, _| h.0.clone());
        // 关掉 LSP 接入：打开文件会尝试连语言服务器（`smol::spawn` 到全局
        // 执行器），会被 gpui 的 TestScheduler 判为非确定性并 abort 整个测试二进制。
        with_lsp_disabled(|| {
            cx.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.open_file_with_content(
                        OpenFileRequest {
                            tab_id: "notes.md#live".into(),
                            path_str: "notes.md".into(),
                            content: "line1\r\nline2\r\n".to_string(),
                            path: std::path::PathBuf::from("notes.md"),
                            ssh_config: None,
                            encoding: FileEncoding::Utf8,
                            read_only: false,
                            is_loading: false,
                            edit_mode: false,
                            live_mode: true,
                        },
                        window,
                        cx,
                    );
                    // `close_file` 的第一步：把防抖中的 live 文本同步进影子
                    // InputState。正是这次同步把影子文本变成 LF。
                    editor.flush_all_live_shadows(cx);
                    let file = editor
                        .open_files
                        .iter()
                        .find(|f| f.id.as_ref() == "notes.md#live")
                        .expect("live tab should be open");
                    assert!(
                        !editor.file_is_dirty(file, cx),
                        "CRLF md opened in live mode must not read as modified"
                    );
                });
            });
        });
    }

    /// 回归：切文件树后恢复 live 草稿，脏状态必须跟着草稿走。
    /// 磁盘基线是 CRLF、影子草稿是 LF（live 模型归一化后的形态）：
    /// 只差行尾的草稿不能判脏；有真实编辑的必须判脏（不能被归一化抹平）。
    #[gpui::test]
    fn restored_live_tab_crlf_baseline_dirty_state(cx: &mut gpui::TestAppContext) {
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
        let (host, cx) =
            cx.add_window_view(|window, cx| EditorHost(cx.new(|cx| Editor::new(window, cx))));
        let editor = host.read_with(cx, |h, _| h.0.clone());
        let mk_info = |id: &str, path: &str| EditorOpenFileInfo {
            id: id.into(),
            path: path.into(),
            label: path.into(),
            ssh_config: None,
            is_loading: false,
            original_content: Some("a\r\nb\r\n".to_string()),
            encoding: FileEncoding::Utf8,
            read_only: false,
            dirty: false,
            diff_repo: None,
        };
        let clean = mk_info("clean.md#live", "clean.md");
        let edited = mk_info("edited.md#live", "edited.md");
        cx.update(|window, cx| {
            let cached_clean = cx.new(|cx| {
                let mut state = InputState::new(window, cx).multi_line(true);
                state.set_value("a\nb\n", window, cx);
                state
            });
            let cached_edited = cx.new(|cx| {
                let mut state = InputState::new(window, cx).multi_line(true);
                state.set_value("a\nX\nb\n", window, cx);
                state
            });
            editor.update(cx, |editor, cx| {
                editor.restore_file_from_cache(&clean, cached_clean, window, cx);
                editor.restore_file_from_cache(&edited, cached_edited, window, cx);

                let clean_file = editor
                    .open_files
                    .iter()
                    .find(|f| f.id.as_ref() == "clean.md#live")
                    .expect("clean tab should be restored");
                assert!(!clean_file.dirty, "行尾差异不构成未保存修改（tab 标记）");
                assert!(
                    !editor.file_is_dirty(clean_file, cx),
                    "行尾差异不构成未保存修改（关闭确认）"
                );

                let edited_file = editor
                    .open_files
                    .iter()
                    .find(|f| f.id.as_ref() == "edited.md#live")
                    .expect("edited tab should be restored");
                assert!(edited_file.dirty, "真实编辑的草稿必须标脏（tab 标记）");
                assert!(
                    editor.file_is_dirty(edited_file, cx),
                    "真实编辑的草稿必须标脏（关闭确认）"
                );
            });
        });
    }

    /// 回归：普通编辑器的脏检查先比长度，插入/删除类击键不再构造全文
    /// `SharedString`；长度相同（替换、改回原样）仍走精确比较。
    #[gpui::test]
    fn plain_editor_dirty_check_covers_length_changes(cx: &mut gpui::TestAppContext) {
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
        let (host, cx) =
            cx.add_window_view(|window, cx| EditorHost(cx.new(|cx| Editor::new(window, cx))));
        let editor = host.read_with(cx, |h, _| h.0.clone());
        with_lsp_disabled(|| {
            cx.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.open_file_with_content(
                        OpenFileRequest {
                            tab_id: "plain.txt".into(),
                            path_str: "plain.txt".into(),
                            content: "abc".to_string(),
                            path: std::path::PathBuf::from("plain.txt"),
                            ssh_config: None,
                            encoding: FileEncoding::Utf8,
                            read_only: false,
                            is_loading: false,
                            edit_mode: false,
                            live_mode: false,
                        },
                        window,
                        cx,
                    );
                    let state = editor
                        .open_files
                        .iter()
                        .find(|f| f.id.as_ref() == "plain.txt")
                        .expect("tab should be open")
                        .editor_state
                        .clone();
                    let is_dirty = |editor: &Editor, cx: &mut gpui::App| {
                        let file = editor
                            .open_files
                            .iter()
                            .find(|f| f.id.as_ref() == "plain.txt")
                            .expect("tab should stay open");
                        editor.file_is_dirty(file, cx)
                    };
                    assert!(!is_dirty(editor, cx), "初始内容与基线一致");
                    state.update(cx, |state, cx| state.set_value("abcd", window, cx));
                    assert!(is_dirty(editor, cx), "插入后长度不同");
                    state.update(cx, |state, cx| state.set_value("abXd", window, cx));
                    assert!(is_dirty(editor, cx), "同长度替换仍要精确判脏");
                    state.update(cx, |state, cx| state.set_value("abc", window, cx));
                    assert!(!is_dirty(editor, cx), "改回基线内容应恢复干净");
                });
            });
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
        let (host, cx) =
            cx.add_window_view(|window, cx| EditorHost(cx.new(|cx| Editor::new(window, cx))));
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
            dirty: false,
            diff_repo: None,
        };

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.restore_from_info(
                    vec![info],
                    None,
                    RestoreBuffers {
                        states: &HashMap::new(),
                        diffs: &HashMap::new(),
                        lsp_sessions: &mut HashMap::new(),
                    },
                    window,
                    cx,
                );
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) =
            cx.add_window_view(|window, cx| EditorHost(cx.new(|cx| Editor::new(window, cx))));
        let editor = host.read_with(cx, |h, _| h.0.clone());
        // 用临时 git 仓库构造可恢复的 diff 上下文：DiffView 后台加载即可，
        // 断言只关心 tab 重建后 `diff.is_some()`（非空白纯文本）。
        let tmp =
            std::env::temp_dir().join(format!("deepterm-diff-restore-{}", std::process::id()));
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
                .any(|f| f.id.as_ref() == diff_id && f.diff().is_some());
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
                let mut empty_lsp = LspSessionMap::new();
                editor.restore_from_info(
                    infos,
                    active,
                    RestoreBuffers {
                        states: &empty_text,
                        diffs: &empty_diff,
                        lsp_sessions: &mut empty_lsp,
                    },
                    window,
                    cx,
                );
            });
            let diff_ok = editor
                .read(cx)
                .open_files
                .iter()
                .any(|f| f.id.as_ref() == diff_id && f.diff().is_some());
            assert!(diff_ok, "diff tab must rebuild its DiffView after restore");
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 在已建好的编辑器上恢复一个 `restore-<tag>.rs` 标签，供 LSP 换手用例复用。
    ///
    /// 路径带 `tag`：两个用例并行跑时不能共用同一路径，否则文件监视器的
    /// 注册/注销会互相干扰。
    fn lsp_restore_tab(
        editor: &gpui::Entity<Editor>,
        cx: &mut gpui::VisualTestContext,
        tag: &str,
    ) -> EditorOpenFileInfo {
        let path = std::env::temp_dir().join(format!("deepterm-lsp-restore-{tag}.rs"));
        let info = EditorOpenFileInfo {
            id: format!("editor-{}", path.display()).into(),
            path: path.to_string_lossy().to_string(),
            label: "restore.rs".into(),
            ssh_config: None,
            is_loading: false,
            original_content: Some("fn main() {}\n".into()),
            encoding: FileEncoding::Utf8,
            read_only: false,
            dirty: false,
            diff_repo: None,
        };
        cx.update(|window, cx| {
            let cached = cx.new(|cx| {
                let mut state = InputState::new(window, cx).multi_line(true);
                state.set_value("fn main() {}\n", window, cx);
                state
            });
            with_lsp_disabled(|| {
                editor.update(cx, |editor, cx| {
                    editor.restore_file_from_cache(&info, cached, window, cx)
                });
            });
        });
        info
    }

    /// 给 `tab_id` 对应的标签安装会话；`active` 决定它「仍连着」还是「已断开」。
    fn install_lsp_session(
        editor: &gpui::Entity<Editor>,
        tab_id: &SharedString,
        active: bool,
        cx: &mut gpui::VisualTestContext,
    ) {
        editor.update(cx, |editor, _| {
            let file = editor
                .open_files
                .iter_mut()
                .find(|f| &f.id == tab_id)
                .expect("fixture tab must exist");
            let state = file.editor_state.downgrade();
            let mut session =
                crate::lsp::session::LspSession::new(Path::new(&file.path), "rust", state, None)
                    .expect("session");
            if active {
                session.mark_active_for_test();
            }
            file.lsp = Some(session);
        });
    }

    /// 回归：切标签页组/文件树后，LSP 会话必须随编辑器状态一起换手。
    ///
    /// 背景：`FileTreeEditorState` 原本只保存文本与 tab，`restore_file_from_cache`
    /// 又硬编码 `lsp: None`。切走时 `LspSession::drop` 发了 `didClose`，切回来
    /// 服务器进程还在、编辑器却没有 client——hover/跳转/补全全部静默失效。
    #[gpui::test]
    fn lsp_session_survives_file_tree_state_restore(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct Host {
            editor: gpui::Entity<Editor>,
        }
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| Host {
            editor: cx.new(|cx| Editor::new(window, cx)),
        });
        let editor = host.read_with(cx, |h, _| h.editor.clone());
        let info = lsp_restore_tab(&editor, cx, "survives");
        install_lsp_session(&editor, &info.id, true, cx);

        let state = editor.update(cx, |editor, cx| editor.save_to_file_tree_state(cx));
        assert!(
            state.cached_lsp_sessions.contains_key(&info.id),
            "live LSP session must be parked in FileTreeEditorState"
        );
        assert!(
            editor.read_with(cx, |editor, _| editor.open_files.is_empty()),
            "save must clear the editor"
        );

        cx.update(|window, cx| {
            with_lsp_disabled(|| {
                editor.update(cx, |editor, cx| {
                    editor.restore_from_file_tree_state(state, window, cx)
                });
            });
        });
        editor.read_with(cx, |editor, _| {
            let file = editor
                .open_files
                .iter()
                .find(|f| f.id == info.id)
                .expect("tab must be restored");
            assert!(
                file.lsp.as_ref().is_some_and(|session| session.is_active()),
                "restored tab must keep its connected LSP session (hover/jump depend on it)"
            );
        });
    }

    /// 回归：断开/未就绪的会话不得在恢复时被原样复活，旧 Ready 状态也要清掉。
    ///
    /// 覆盖服务器在切走期间崩溃、用户 Stop、握手没完成就切走这三种情况：
    /// 会话没有可用 client，原样放回只会让 `active_lsp_session()` 拿到空壳，
    /// hover/跳转继续静默失败。
    #[gpui::test]
    fn restore_drops_disconnected_lsp_session_and_clears_stale_status(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct Host {
            editor: gpui::Entity<Editor>,
        }
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| Host {
            editor: cx.new(|cx| Editor::new(window, cx)),
        });
        let editor = host.read_with(cx, |h, _| h.editor.clone());
        let info = lsp_restore_tab(&editor, cx, "drops");
        install_lsp_session(&editor, &info.id, false, cx);
        editor.update(cx, |editor, cx| {
            editor.open_files[0].editor_state.update(cx, |state, _| {
                state.set_lsp_status(Some(crate::lsp::status::LspStatus::Ready {
                    errors: 2,
                    warnings: 1,
                }));
            });
        });

        let state = editor.update(cx, |editor, cx| editor.save_to_file_tree_state(cx));
        assert!(
            state.cached_lsp_sessions.contains_key(&info.id),
            "even a disconnected session is parked with the state"
        );

        cx.update(|window, cx| {
            with_lsp_disabled(|| {
                editor.update(cx, |editor, cx| {
                    editor.restore_from_file_tree_state(state, window, cx)
                });
            });
        });
        editor.read_with(cx, |editor, cx| {
            let file = editor
                .open_files
                .iter()
                .find(|f| f.id == info.id)
                .expect("tab must be restored");
            assert!(
                file.lsp.is_none(),
                "disconnected session must not be resurrected"
            );
            assert!(
                file.editor_state.read(cx).lsp_status().is_none(),
                "stale Ready status must be cleared before reconnecting"
            );
        });
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) =
            cx.add_window_view(|window, cx| EditorHost(cx.new(|cx| Editor::new(window, cx))));
        let editor = host.read_with(cx, |h, _| h.0.clone());

        let mk_repo = |tag: &str| {
            let dir = std::env::temp_dir().join(format!(
                "deepterm-diff-collide-{tag}-{}",
                std::process::id()
            ));
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
                .filter(|f| f.diff().is_some())
                .map(|f| &f.id)
                .collect();
            assert_eq!(
                ids.len(),
                2,
                "两个仓库的同名路径必须是两个独立 tab: {ids:?}"
            );
            assert_ne!(ids[0], ids[1]);
            // 每个仓库各有一个可解析到自身的 repo。
            let repos: Vec<&crate::editor::git_repository::GitRepository> = editor
                .open_files
                .iter()
                .filter_map(|f| f.diff_repo())
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
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
            dirty: false,
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
            assert!(
                (e.width - 880.0).abs() <= 0.5,
                "no drift, width={:.1}",
                e.width
            );
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
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
            dirty: false,
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

    /// 首个文件打开时编辑器默认与终端区域平分可用宽度（代码与 markdown
    /// 走同一条打开路径）；之后再打开文件、恢复（切标签页组）都不重新平分。
    #[gpui::test]
    fn editor_opens_at_half_of_available_width(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
        });
        struct Host(gpui::Entity<Editor>);
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let (host, cx) =
            cx.add_window_view(|window, cx| Host(cx.new(|cx| Editor::new(window, cx))));
        let editor = host.read_with(cx, |h, _| h.0.clone());
        // AppWindow 每帧下发：窗口 1600 − 侧边栏占位 280 = 1320 可用；
        // 主区最小宽 320 → 窗口跟随上限 1000。
        editor.update(cx, |e, _| {
            e.set_available_width(1320.0);
            e.set_window_follow_limit(1000.0);
        });

        let open = |editor: &gpui::Entity<Editor>,
                    cx: &mut gpui::VisualTestContext,
                    id: &str,
                    live: bool| {
            with_lsp_disabled(|| {
                cx.update(|window, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.open_file_with_content(
                            OpenFileRequest {
                                tab_id: id.to_string(),
                                path_str: id.to_string(),
                                content: "hello\n".to_string(),
                                path: PathBuf::from(id),
                                ssh_config: None,
                                encoding: FileEncoding::Utf8,
                                read_only: false,
                                is_loading: false,
                                edit_mode: false,
                                live_mode: live,
                            },
                            window,
                            cx,
                        );
                    });
                });
            });
        };

        // 首个文件（markdown live）：与终端平分 1320 → 660，展开动画 0 → 660。
        open(&editor, cx, "notes.md", true);
        editor.update(cx, |e, _| {
            assert!(
                (e.width - 660.0).abs() <= 0.5,
                "first open must split available width in half, width={:.1}",
                e.width
            );
            assert!(
                e.panel_anim_start.abs() <= 0.5 && (e.panel_anim_end - e.width).abs() <= 0.5,
                "open animation must run 0 → half width, got {:.1}->{:.1}",
                e.panel_anim_start,
                e.panel_anim_end
            );
        });

        // 用户拖拽到 900 后打开第二个文件（普通代码）：不重新平分。
        editor.update(cx, |e, _| e.width = 900.0);
        open(&editor, cx, "main.rs", false);
        editor.update(cx, |e, _| {
            assert!(
                (e.width - 900.0).abs() <= 0.5,
                "later opens must keep the user width, width={:.1}",
                e.width
            );
        });

        // 恢复（切标签页组/文件树）保留当前宽度，不因「瞬时为空」重新平分。
        let state = editor.update(cx, |e, cx| e.save_to_file_tree_state(cx));
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.restore_from_file_tree_state(state, window, cx)
            });
        });
        editor.update(cx, |e, _| {
            assert!(
                (e.width - 900.0).abs() <= 0.5,
                "restore must keep the current width, width={:.1}",
                e.width
            );
        });

        // 钳制：窄窗口不低于最小宽度，宽窗口不超过「主区最小宽」给的上限。
        editor.update(cx, |e, _| {
            e.set_available_width(300.0);
            e.set_window_follow_limit(0.0);
            e.apply_open_split_width();
            assert!(
                (e.width - EDITOR_MIN_WIDTH).abs() <= 0.5,
                "narrow window must clamp to min width, width={:.1}",
                e.width
            );
            e.set_available_width(3000.0);
            e.set_window_follow_limit(1000.0);
            e.apply_open_split_width();
            assert!(
                (e.width - 1000.0).abs() <= 0.5,
                "wide window must respect the main area min width, width={:.1}",
                e.width
            );
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
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
            dirty: false,
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
                        cached_lsp_sessions: HashMap::new(),
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
    fn status_position_percent_top_bot() {
        assert_eq!(Editor::format_status_position(140, 27, 174), "81% 141:28");
        assert_eq!(Editor::format_status_position(0, 0, 174), "Top 1:1");
        assert_eq!(Editor::format_status_position(173, 27, 174), "Bot 174:28");
        assert_eq!(Editor::format_status_position(0, 0, 1), "Top 1:1");
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
    fn content_matches_baseline_line_ending_rules() {
        // 完全一致：不论 live 与否。
        assert!(content_matches_baseline("a\nb", "a\nb", false));
        assert!(content_matches_baseline("a\r\nb", "a\r\nb", true));
        // 普通编辑器：行尾不同是真实差异（保存会按原风格写回）。
        assert!(!content_matches_baseline("a\nb", "a\r\nb", false));
        // live：模型剥 `\r`，行尾差异不算修改；真实内容差异仍算。
        assert!(content_matches_baseline("a\nb", "a\r\nb", true));
        assert!(content_matches_baseline("a\r\nb\r\n", "a\nb\n", true));
        assert!(!content_matches_baseline("a\nX\nb", "a\r\nb", true));
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

    /// 建一个带「可见补全菜单」的 Editor，供 Enter 键位回归用例复用。
    fn completion_key_fixture(
        cx: &mut gpui::TestAppContext,
        show_menu: bool,
    ) -> (
        gpui::Entity<Editor>,
        gpui::Entity<InputState>,
        &mut gpui::VisualTestContext,
    ) {
        let _ = env_logger::builder().is_test(true).try_init();
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            crate::file_manager::watcher::init(cx);
            crate::vim::init(cx);
            crate::keymap::init(cx);
        });
        struct Host {
            editor: gpui::Entity<Editor>,
        }
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                div().size_full().child(self.editor.clone())
            }
        }
        let (host, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| Editor::new(window, cx));
            editor.update(cx, |editor, _| editor.suppress_open_animation = true);
            Host { editor }
        });
        let editor = host.read_with(cx, |host, _| host.editor.clone());

        let info = EditorOpenFileInfo {
            id: "a.rs".into(),
            path: "a.rs".into(),
            label: "a.rs".into(),
            ssh_config: None,
            is_loading: false,
            original_content: Some("pri".to_string()),
            encoding: FileEncoding::Utf8,
            read_only: false,
            dirty: false,
            diff_repo: None,
        };
        cx.update(|window, cx| {
            let cached = cx.new(|cx| {
                let mut state = InputState::new(window, cx)
                    .code_editor("rust")
                    .multi_line(true);
                state.set_value("pri", window, cx);
                state
            });
            editor.update(cx, |editor, cx| {
                editor.restore_file_from_cache(&info, cached, window, cx);
            });
        });
        let input = editor.read_with(cx, |editor, _| editor.open_files[0].editor_state.clone());

        input.update(cx, |state, _| {
            state.selected_range = (3..3).into();
        });
        editor.update(cx, |editor, cx| {
            editor.active_file_id = Some(info.id.clone());
            let owner = input.entity_id();
            let uri = lsp_types::Url::parse("file:///a.rs").unwrap();
            if show_menu {
                input.update(cx, |state, _| {
                    assert!(
                        editor.completion.show_for_test(
                            state,
                            owner,
                            uri,
                            vec![lsp_types::CompletionItem {
                                label: "println".into(),
                                kind: Some(lsp_types::CompletionItemKind::FUNCTION),
                                ..Default::default()
                            }],
                            3,
                        ),
                        "前置：补全菜单必须可见"
                    );
                });
            } else {
                // 无菜单时也要设置 owner/uri，确保 completion 只是不可见，
                // 而不是因为状态缺失被动跳过。
                editor.completion.show_none_for_test(owner, uri);
            }
        });

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.focus_handle.clone().focus(window, cx);
            });
        });
        // 生成一帧：`EditorInput` 的键位上下文必须带上 `showing_completions`。
        let _ = cx.refresh();
        (editor, input, cx)
    }

    /// `:` 面板：输入行号回车，应该跳转并自动关闭。
    #[gpui::test]
    fn ex_command_panel_goto_line(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("one\ntwo\nthree", cx);
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        assert!(
            editor.read_with(&mut *cx, |editor, _| editor.ex_command_panel.is_some()),
            "`: ` 应打开命令行面板"
        );
        let _ = cx.refresh();

        cx.simulate_keystrokes("2");
        cx.simulate_keystrokes("enter");

        assert!(
            editor.read_with(&mut *cx, |editor, _| editor.ex_command_panel.is_none()),
            "执行后命令行面板应关闭"
        );
        assert_eq!(
            input.read_with(cx, |state, _| state.cursor()),
            4,
            "`:2` 应跳到第二行行首"
        );
    }

    /// 直接执行 Ex 命令的回归：跳转、范围删除、替换、排序。
    #[gpui::test]
    fn ex_commands_edit_and_jump(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        cx.update(|window, cx| {
            input.update(cx, |state, cx| state.set_text("one\ntwo\nthree", cx));
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("2", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.cursor()),
            4,
            ":2 应跳到第 2 行行首"
        );

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("$", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.cursor()),
            8,
            ":$ 应跳到末行行首"
        );

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("%s/o/0/g", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "0ne\ntw0\nthree"
        );

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("1,2d", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "three"
        );

        cx.update(|window, cx| {
            input.update(cx, |state, cx| state.set_text("c\na\nb", cx));
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("sort", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "a\nb\nc"
        );
    }

    /// 状态栏 Vim 徽章/快捷键切换：Linux 默认关闭时应能一键开启。
    #[gpui::test]
    fn toggle_vim_from_status_bar(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        cx.update(|_, cx| crate::settings::SettingsStore::init_for_test(cx));
        assert!(!cx.update(|_, cx| crate::vim::enabled(cx)));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| editor.toggle_vim(window, cx));
        });
        assert!(cx.update(|_, cx| crate::vim::enabled(cx)));
        assert!(input.read_with(cx, |state, _| state.vim.is_some()));

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| editor.toggle_vim(window, cx));
        });
        assert!(!cx.update(|_, cx| crate::vim::enabled(cx)));
        assert!(input.read_with(cx, |state, _| state.vim.is_none()));
    }

    /// 真实按 `:` 应打开 Ex 面板（而不是只验证键位表解析）。
    #[gpui::test]
    fn colon_key_opens_ex_panel(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("one", cx);
            state.vim = Some(crate::vim::VimState::default());
        });
        cx.update(|_, cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            crate::keymap::reload(cx);
        });
        let _ = cx.refresh();
        let focus = input.read_with(&mut *cx, |state, _| state.focus_handle.clone());
        cx.update(|window, cx| focus.focus(window, cx));
        cx.simulate_keystrokes(":");
        assert!(
            editor.read_with(&mut *cx, |editor, _| editor.ex_command_panel.is_some()),
            "Vim Normal 下按 : 应打开 Ex 面板"
        );
    }

    /// `:normal` 通过面板执行，并在关闭后把按键派发给编辑区。
    #[gpui::test]
    fn ex_normal_runs_via_panel(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| state.set_text("abc", cx));
        input.update(&mut *cx, |state, _| {
            state.vim = Some(crate::vim::VimState::default());
        });
        cx.update(|_, cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            crate::keymap::reload(cx);
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                if let Some(panel) = editor.ex_command_panel.as_ref() {
                    panel.update(cx, |panel, cx| {
                        panel.set_input("normal x", window, cx);
                    });
                }
            });
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let _ = cx.refresh();
        let _ = cx.refresh();
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "bc",
            ":normal x 应删除光标处字符"
        );
    }

    /// `:normal` 支持 `f`/`t` 的 pending 字符。
    #[gpui::test]
    fn ex_normal_pending_char(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("abxc", cx);
            state.vim = Some(crate::vim::VimState::default());
        });
        cx.update(|_, cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            crate::keymap::reload(cx);
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                if let Some(panel) = editor.ex_command_panel.as_ref() {
                    panel.update(cx, |panel, cx| {
                        panel.set_input("normal fxx", window, cx);
                    });
                }
            });
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "abc"
        );
    }

    /// `:normal` 支持操作符键序列（`dd`）。
    #[gpui::test]
    fn ex_normal_operator_sequence(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("abc\ndef", cx);
            state.vim = Some(crate::vim::VimState::default());
        });
        cx.update(|_, cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            crate::keymap::reload(cx);
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                if let Some(panel) = editor.ex_command_panel.as_ref() {
                    panel.update(cx, |panel, cx| {
                        panel.set_input("normal dd", window, cx);
                    });
                }
            });
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "def"
        );
    }

    /// `:normal` 行范围：每行执行一次，插入文本后自动回 Normal。
    #[gpui::test]
    fn ex_normal_range_applies_to_each_line(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("a\nb\nc", cx);
            state.vim = Some(crate::vim::VimState::default());
        });
        cx.update(|_, cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            crate::keymap::reload(cx);
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                if let Some(panel) = editor.ex_command_panel.as_ref() {
                    panel.update(cx, |panel, cx| {
                        panel.set_input("1,2normal A!", window, cx);
                    });
                }
            });
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "a!\nb!\nc"
        );
    }

    /// `:g/pat/normal`：匹配行逐行执行普通模式命令。
    #[gpui::test]
    fn ex_global_normal(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("a1\nb2\na3", cx);
            state.vim = Some(crate::vim::VimState::default());
        });
        cx.update(|_, cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            crate::keymap::reload(cx);
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let _ = cx.refresh();
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                if let Some(panel) = editor.ex_command_panel.as_ref() {
                    panel.update(cx, |panel, cx| {
                        panel.set_input("g/^b/normal A!", window, cx);
                    });
                }
            });
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "a1\nb2!\na3"
        );
    }

    /// Visual 打开 `:` 时预填 `'<,'>`。
    #[gpui::test]
    fn ex_visual_panel_prefills_range(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("one\ntwo", cx);
            state.vim = Some(crate::vim::VimState::default());
            if let Some(vim) = state.vim.as_mut() {
                vim.mode = crate::vim::VimMode::Visual;
            }
            state.selected_range = (0..3).into();
        });
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let value = editor.read_with(&mut *cx, |editor, cx| {
            editor
                .ex_command_panel
                .as_ref()
                .map(|panel| panel.read(cx).input.read(cx).value().to_string())
        });
        assert_eq!(value.as_deref(), Some("'<,'>"));
    }

    /// `:g` / `:v` 全局命令。
    #[gpui::test]
    fn ex_global_and_invert(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| state.set_text("a1\nb2\na3", cx));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("g/^a/d", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "b2\n"
        );

        input.update(&mut *cx, |state, cx| state.set_text("a1\nb2\na3", cx));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("v/^a/d", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "a1\na3"
        );

        input.update(&mut *cx, |state, cx| state.set_text("foo\nbar\nfoo", cx));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(
                    editor
                        .execute_ex_command("g/^foo/s/foo/XX/", window, cx)
                        .is_ok()
                );
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "XX\nbar\nXX"
        );

        let message = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                match editor.execute_ex_command("g/zzz/d", window, cx).unwrap() {
                    crate::editor::ex_command::ExOutcome::Message(message) => message,
                    _ => panic!("no match should report a message"),
                }
            })
        });
        assert!(message.contains("Pattern not found"));
    }

    /// Visual 选区 `:'<,'>` 范围。
    #[gpui::test]
    fn ex_visual_range_deletes_selection_lines(cx: &mut gpui::TestAppContext) {
        let (_editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| {
            state.set_text("one\ntwo\nthree", cx);
            state.vim = Some(crate::vim::VimState::default());
            if let Some(vim) = state.vim.as_mut() {
                vim.mode = crate::vim::VimMode::VisualLine;
            }
            state.selected_range = (0..4).into();
        });
        cx.update(|window, cx| {
            _editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("'<,'>d", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "two\nthree"
        );
    }

    /// `:e!` 异步从磁盘重载。
    #[gpui::test]
    fn ex_reload_is_async(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        let path =
            std::env::temp_dir().join(format!("deepterm-ex-reload-{}.txt", std::process::id()));
        std::fs::write(&path, "disk").unwrap();
        editor.update(&mut *cx, |editor, cx| {
            let file = editor.open_files.first_mut().unwrap();
            file.path = path.to_string_lossy().to_string();
            file.original_content = "old".into();
            file.dirty = false;
            file.editor_state.update(cx, |state, cx| {
                state.set_text("old", cx);
            });
        });
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("e!", window, cx).is_ok());
            });
        });
        cx.run_until_parked();
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "disk"
        );
        let _ = std::fs::remove_file(path);
    }

    /// `:!` 与范围过滤。
    #[cfg(unix)]
    #[gpui::test]
    fn ex_shell_command_and_filter(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        let message = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                match editor.execute_ex_command("!printf hi", window, cx).unwrap() {
                    crate::editor::ex_command::ExOutcome::Message(message) => message,
                    _ => panic!("shell command should return output"),
                }
            })
        });
        assert!(message.contains("hi"));

        input.update(&mut *cx, |state, cx| state.set_text("abc\ndef", cx));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(
                    editor
                        .execute_ex_command("1!sed s/a/A/", window, cx)
                        .is_ok()
                );
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "Abc\ndef"
        );
    }

    /// `:` 面板的历史记录：执行过的命令可用 ↑ 召回。
    #[gpui::test]
    fn ex_command_panel_history(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| state.set_text("one\ntwo", cx));
        let _ = cx.refresh();

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let _ = cx.refresh();
        cx.simulate_keystrokes("2");
        cx.simulate_keystrokes("enter");
        assert!(editor.read_with(&mut *cx, |editor, _| editor.ex_command_panel.is_none()));

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.open_ex_command(&ExCommandLine, window, cx);
            });
        });
        let _ = cx.refresh();
        cx.simulate_keystrokes("up");
        let value = editor.read_with(&mut *cx, |editor, cx| {
            editor
                .ex_command_panel
                .as_ref()
                .map(|panel| panel.read(cx).input.read(cx).value().to_string())
        });
        assert_eq!(value.as_deref(), Some("2"), "↑ 应召回上一条命令");

        cx.simulate_keystrokes("escape");
        assert!(editor.read_with(&mut *cx, |editor, _| editor.ex_command_panel.is_none()));
    }

    /// `:y` / `:j` / `:marks` / `:registers`。
    #[gpui::test]
    fn ex_yank_join_marks_and_registers(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, cx| state.set_text("a\nb\nc", cx));

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("1y", window, cx).is_ok());
            });
            let yanked = crate::vim::Registers::global(cx).read(None, cx);
            assert_eq!(yanked, "a\n");
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert!(editor.execute_ex_command("1j", window, cx).is_ok());
            });
        });
        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "a b\nc"
        );

        cx.update(|_, cx| crate::vim::Marks::global_mut(cx).set('a', 0));
        let message = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                match editor.execute_ex_command("marks", window, cx).unwrap() {
                    crate::editor::ex_command::ExOutcome::Message(message) => message,
                    _ => panic!("marks should return a message"),
                }
            })
        });
        assert!(message.contains('a'), "{message}");

        let message = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                match editor.execute_ex_command("registers", window, cx).unwrap() {
                    crate::editor::ex_command::ExOutcome::Message(message) => message,
                    _ => panic!("registers should return a message"),
                }
            })
        });
        assert!(message.contains('a'), "{message}");
    }

    /// `:set nu` / `:set rnu` / `:set ic` 应该落到当前编辑器状态。
    #[gpui::test]
    fn ex_set_options_apply(cx: &mut gpui::TestAppContext) {
        let (_editor, input, cx) = completion_key_fixture(cx, false);
        input.update(&mut *cx, |state, _| {
            state.vim = Some(crate::vim::VimState::default());
        });
        cx.update(|window, cx| {
            _editor.update(cx, |editor, cx| {
                assert!(
                    editor
                        .execute_ex_command("set nu rnu ic", window, cx)
                        .is_ok()
                );
            });
        });
        input.read_with(cx, |state, _| {
            assert!(state.mode.line_number());
            let vim = state.vim.as_ref().expect("vim state");
            assert!(vim.relative_line_numbers);
            assert!(vim.ignore_case);
        });
    }

    /// 回归：补全菜单可见时按 Enter，不能同时插入换行。
    ///
    /// 走**真实**的 `Editor` 渲染树与击键派发：`CompletionConfirm` 和
    /// `editor::Enter` 会同时匹配，前者必须 `stop_propagation()` 并消费掉
    /// 这次按键，否则用户看到的就是「确认补全后又多了一行」。
    #[gpui::test]
    fn enter_confirming_completion_does_not_insert_newline(cx: &mut gpui::TestAppContext) {
        let (_editor, input, cx) = completion_key_fixture(cx, true);
        cx.simulate_keystrokes("enter");

        let text = input.read_with(cx, |state, _| state.text().to_string());
        assert_eq!(
            text, "println",
            "Enter 只能确认补全，不能再触发 editor::Enter 插入换行"
        );
        assert!(
            input.read_with(cx, |state, _| state.completion_menu().is_none()),
            "确认后菜单快照必须清掉"
        );
    }

    /// 回归：候选区间起点越过光标（例如 3..3 光标在 3 后一字节）时，
    /// 确认必须退回当前词区间，不能在光标后/下一行插入补全。
    #[gpui::test]
    fn completion_range_past_cursor_is_clamped(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, true);
        editor.update(&mut *cx, |editor, _| {
            editor.completion.break_selected_range_for_test(4..4);
        });

        cx.simulate_keystrokes("enter");

        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "println",
            "越过光标的候选区间必须退回当前词，不能把补全写到下一个位置"
        );
    }

    /// 回归：函数补全必须按 snippet 展开成 `foo(a)`，并选中第一个参数，
    /// 而不是只插入函数名。
    #[gpui::test]
    fn function_snippet_completion_inserts_parentheses_and_selects_arg(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, input, cx) = completion_key_fixture(cx, true);
        let owner = input.entity_id();
        let uri = lsp_types::Url::parse("file:///a.rs").unwrap();
        editor.update(&mut *cx, |editor, cx| {
            input.update(cx, |state, _| {
                assert!(editor.completion.show_for_test(
                    state,
                    owner,
                    uri,
                    vec![lsp_types::CompletionItem {
                        label: "foo(…)".into(),
                        filter_text: Some("pri".into()),
                        kind: Some(lsp_types::CompletionItemKind::FUNCTION),
                        insert_text_format: Some(lsp_types::InsertTextFormat::SNIPPET),
                        text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                            range: lsp_types::Range::new(
                                lsp_types::Position::new(0, 0),
                                lsp_types::Position::new(0, 3),
                            ),
                            new_text: "foo(${1:a})$0".into(),
                        },)),
                        ..Default::default()
                    }],
                    3,
                ));
            });
        });

        cx.simulate_keystrokes("enter");

        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "foo(a)",
            "函数补全必须带上括号和参数占位符"
        );
        assert_eq!(
            input.read_with(cx, |state, _| (
                state.selected_range.start,
                state.selected_range.end
            )),
            (4, 5),
            "第一个 tabstop 应该选中参数 a"
        );
    }

    /// 回归：补全键的归属编辑器必须真的持有焦点；焦点在编辑区里的其它
    /// 输入框时，不能把候选插进背后的代码编辑器。
    #[gpui::test]
    fn completion_owner_requires_focus(cx: &mut gpui::TestAppContext) {
        let (editor, input, cx) = completion_key_fixture(cx, true);
        let other = cx.update(|window, cx| {
            let other = cx.new(|cx| InputState::new(window, cx).multi_line(true));
            other.update(cx, |state, cx| {
                state.focus_handle.clone().focus(window, cx);
            });
            other
        });

        let owner_focused = cx.update(|window, cx| {
            editor.read_with(cx, |editor, cx| {
                editor.focused_completion_editor(window, cx).is_some()
            })
        });
        assert!(!owner_focused, "焦点在别的输入框时不能认领补全键");
        assert_eq!(
            input.read_with(&mut *cx, |state, _| state.text().to_string()),
            "pri",
            "失焦只应收起菜单，不能写入候选"
        );
        drop(other);

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.focus_handle.clone().focus(window, cx);
            });
        });
        let owner_focused = cx.update(|window, cx| {
            editor.read_with(cx, |editor, cx| {
                editor.focused_completion_editor(window, cx).is_some()
            })
        });
        assert!(owner_focused, "焦点回到编辑区后应重新认领补全键");
    }

    /// 回归：补全菜单不可见时，无条件的 CompletionConfirm 绑定必须让位，
    /// Enter 仍然只插入一个换行（不能吞掉普通编辑键）。
    #[gpui::test]
    fn enter_without_completion_menu_still_inserts_newline(cx: &mut gpui::TestAppContext) {
        let (_editor, input, cx) = completion_key_fixture(cx, false);
        cx.simulate_keystrokes("enter");

        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "pri\n",
            "菜单不可见时 Enter 必须回到普通换行语义"
        );
    }

    /// 回归：补全菜单可见时按 Tab 也只确认补全，不能同时插入缩进。
    #[gpui::test]
    fn tab_confirming_completion_does_not_indent(cx: &mut gpui::TestAppContext) {
        let (_editor, input, cx) = completion_key_fixture(cx, true);
        cx.simulate_keystrokes("tab");

        assert_eq!(
            input.read_with(cx, |state, _| state.text().to_string()),
            "println",
            "Tab 只能确认补全，不能再插入缩进"
        );
    }

    /// 回归：补全菜单不可见时，Tab 仍然执行普通缩进。
    #[gpui::test]
    fn tab_without_completion_menu_still_indents(cx: &mut gpui::TestAppContext) {
        let (_editor, input, cx) = completion_key_fixture(cx, false);
        cx.simulate_keystrokes("tab");

        let text = input.read_with(cx, |state, _| state.text().to_string());
        assert!(
            text.starts_with("pri") && text.len() > 3 && !text.contains('\n'),
            "菜单不可见时 Tab 必须回到普通缩进语义，实际 {text:?}"
        );
    }

    /// 回归：Enter 没被 `CompletionConfirm` 键位接住时，必须走
    /// `InputState::enter` 的兜底，交给 Editor 下一帧确认补全，而不是换行。
    ///
    /// 真实环境中键位上下文可能晚一帧（失焦/重渲染/用户覆盖键位），这条
    /// 路径保证「菜单可见时按 Enter」永远不会退化成换行。
    #[gpui::test]
    fn enter_action_falls_back_to_completion_instead_of_newline(cx: &mut gpui::TestAppContext) {
        let (_editor, input, cx) = completion_key_fixture(cx, true);
        // 绕过键位派发，直接走 `editor::Enter` 的 handler —— 模拟
        // `CompletionConfirm` 没有被派发的情况。
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.enter(
                    &crate::components::input::Enter { secondary: false },
                    window,
                    cx,
                );
            });
        });
        assert!(
            !input
                .read_with(&mut *cx, |state, _| state.text().to_string())
                .contains('\n'),
            "兜底路径本身不能插入换行"
        );

        // 下一帧由 Editor 取走 accept 请求并执行确认。
        let _ = cx.refresh();
        assert_eq!(
            input.read_with(&mut *cx, |state, _| state.text().to_string()),
            "println",
            "Editor 必须把 Enter 兜底请求转成补全确认"
        );
    }
}

#[cfg(test)]
mod lsp_signature_tests {
    use super::signature_trigger_for;
    use ropey::Rope;

    #[test]
    fn open_paren_puts_cursor_in_params() {
        let t = Rope::from("foo(1, 2");
        assert!(
            signature_trigger_for(&t, t.len()),
            "cursor inside `(` should trigger"
        );
    }

    #[test]
    fn closed_paren_does_not_trigger() {
        let t = Rope::from("foo(1, 2)");
        // 光标在 `)` 之后：不在参数列表内。
        assert!(!signature_trigger_for(&t, t.len()));
    }

    #[test]
    fn no_paren_never_triggers() {
        let t = Rope::from("let x = 1;");
        assert!(!signature_trigger_for(&t, t.len()));
    }

    #[test]
    fn nested_paren_still_triggers() {
        let t = Rope::from("outer(inner(1");
        assert!(signature_trigger_for(&t, t.len()));
    }

    /// 回归：多字节字符（中文注释）不能让回溯 panic。
    ///
    /// 曾经按字节回退 `offset - 1` 并调 `Rope::char`，落在多字节字符
    /// 内部会直接 panic——本项目的中文注释里必现，表现为打开 .rs 文件
    /// 立刻崩溃。
    #[test]
    fn multibyte_text_does_not_panic() {
        let src = "fn main() { // 中文注释\n    foo(1\n";
        let t = Rope::from(src);
        // 光标在末尾（参数列表内），前面全是 CJK 字节。
        assert!(signature_trigger_for(&t, t.len()));
    }

    #[test]
    fn multibyte_without_paren_does_not_panic() {
        let t = Rope::from("// 中文注释 emoji 🎉 结尾");
        assert!(!signature_trigger_for(&t, t.len()));
    }

    #[test]
    fn cursor_at_zero_or_past_end_is_safe() {
        let t = Rope::from("abc");
        assert!(!signature_trigger_for(&t, 0));
        // 越界光标：内部会钳制到文本长度。
        assert!(!signature_trigger_for(&t, 999));
    }

    /// 光标恰好落在多字节字符**中间**也不能 panic（上游偏移算错时的情形）。
    #[test]
    fn cursor_inside_multibyte_char_is_safe() {
        let t = Rope::from("中文abc");
        assert!(!signature_trigger_for(&t, 1));
        assert!(!signature_trigger_for(&t, 2));
    }

    /// 回归：`Rope::char` 收的是**字节下标**，把 char 下标直接喂进去会在
    /// 中文/emoji 内部 panic。签名帮助每次请求都会取「光标前一个字符」。
    #[test]
    fn char_before_handles_multibyte_text() {
        use super::char_before;

        let t = Rope::from("中文abc");
        assert_eq!(char_before(&t, 0), None);
        assert_eq!(char_before(&t, 3), Some('中'));
        assert_eq!(char_before(&t, 6), Some('文'));
        assert_eq!(char_before(&t, 7), Some('a'));
        // 落在多字节字符内部：向左吸附后取前一个字符（1 → 0，无前字符）。
        assert_eq!(char_before(&t, 1), None);
        assert_eq!(char_before(&t, 4), Some('中'));
        // 越界钳制到末尾。
        assert_eq!(char_before(&t, 999), Some('c'));

        let e = Rope::from("a🎉");
        assert_eq!(char_before(&e, e.len()), Some('🎉'));
        assert_eq!(char_before(&e, 3), Some('a'));
    }
}
