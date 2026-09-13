//! Git 源代码管理面板：对标 VSCode Source Control 视图。
//!
//! 结构（自上而下）：提交输入框（含分支占位）→ Commit 分段按钮 → Amend 行 →
//! 分组列表（合并更改 / 已暂存的更改 / 更改）。行布局与交互（文件名 + 右侧
//! 增删行数 "+N -M" + 状态字母，悬停替换为打开/放弃/暂存操作）与 Zed/VSCode
//! 一致；完整路径以 tooltip 与 diff 页顶部标题呈现，不在行内展示。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseButton, ParentElement, PromptLevel, Render, Stateful, Styled, Subscription, Task,
    UniformListScrollHandle, Window, div, px, svg, uniform_list,
};

use crate::assets::Icon;
use crate::assets::file_icon_or_default;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::menu::{PopupMenu, PopupMenuItem};
use crate::components::tooltip::Tooltip;
use crate::editor::git_repository::{ConflictSide, GitRepository};
use crate::editor::git_status::{DiffStat, FileStatus, Section};
use crate::file_manager::filesystem::FileSystem;
use crate::file_manager::watcher::{FsChangeEvent, WatcherContext};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;

const LIST_ROW_HEIGHT: f32 = 24.0;
const REFRESH_DEBOUNCE_DELAY: Duration = Duration::from_millis(150);
/// Fallback poll for remote git status (now also watches .git/index directly,
/// so interval can be longer than file watcher but still catch missed events).
const REMOTE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// 分支信息（分支名 + ahead/behind）节流间隔。
/// status 轮询是 10s，这里错开成 30s：两者合并一次后台任务取回。
const BRANCH_INFO_TTL: Duration = Duration::from_secs(30);

/// Git panel events
pub enum GitPanelEvent {
    /// Instant open: content loads asynchronously in DiffView.
    /// `staged` 为 true 时打开已暂存 diff（HEAD↔暂存区，只读）。
    OpenDiffLoading {
        path: String,
        repo: GitRepository,
        staged: bool,
    },
    /// 在文件树/编辑器中打开该文件（右键菜单“打开文件”）。
    OpenFile {
        path: std::path::PathBuf,
        ssh_config: Option<SshConnectionConfig>,
    },
    /// Emitted after `git add` succeeds; the HEAD → worktree diff must be recomputed.
    StageFile { path: String },
    /// Emitted after `git reset` succeeds; same effect as [`Self::StageFile`].
    UnstageFile { path: String },
    /// 放弃更改成功（含未跟踪文件删除）：编辑器行标记与 diff 页需刷新。
    DiscardFile { path: String },
    /// 提交成功：调用方弹成功提示并刷新 diff 页。
    CommitDone,
    /// 仓库结构级变化（stash push/pop、冲突解决、初始化等）：
    /// diff 页与编辑器行标记需整体刷新，但不弹通知（操作本身已有反馈）。
    RepoChanged,
    /// 暂存/取消暂存/放弃/提交失败：调用方弹错误提示。
    OperationFailed { message: String },
    /// 状态列表已刷新（含清空）：文件树据此着色变更文件。
    StatusChanged,
}

/// A single entry in the git panel list
#[derive(Clone, Debug)]
pub enum GitListEntry {
    Header(Section),
    File(GitFileEntry),
}

#[derive(Clone, Debug)]
pub struct GitFileEntry {
    pub path: String,
    pub status: FileStatus,
    pub section: Section,
}

impl GitFileEntry {
    /// 右侧单字母徽标（VSCode 风格 M/A/D/U/C/R/T）。
    pub fn badge_letter(&self) -> char {
        self.status.badge_letter()
    }

    pub fn badge_color(&self, theme: &crate::theme::UiTheme) -> gpui::Hsla {
        match self.badge_letter() {
            'A' | 'U' => theme.status_added,
            'D' => theme.status_deleted,
            'M' => theme.status_modified,
            // C 冲突：冲突色；R 改名 / T 类型变更：沿用修改色（VSCode 同款）。
            'C' => theme.status_conflict,
            'R' | 'T' => theme.status_modified,
            _ => theme.text_subtle,
        }
    }

    /// 是否显示“暂存”按钮（含未跟踪文件；冲突文件不可暂存）。
    pub fn can_stage(&self) -> bool {
        !self.status.is_conflicted() && self.status.has_unstaged_changes()
    }

    /// 是否显示“取消暂存”按钮。
    pub fn can_unstage(&self) -> bool {
        self.status.has_staged_changes()
    }

    /// 行内主操作：有未暂存改动则暂存，否则取消暂存。
    pub fn should_stage(&self) -> bool {
        self.can_stage()
    }

    /// 文件名与父目录（VSCode 行：文件名左对齐，目录右侧弱化）。
    pub fn file_name(&self) -> &str {
        match self.path.rsplit_once('/') {
            Some((_, name)) => name,
            None => &self.path,
        }
    }

    pub fn parent_dir(&self) -> Option<&str> {
        match self.path.rsplit_once('/') {
            Some((dir, _)) => Some(dir),
            None => None,
        }
    }
}

/// 提交范围：全部（无已暂存时先暂存全部，即智能提交）或仅已暂存。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitScope {
    Smart,
    StagedOnly,
}

/// 暂存区批量操作的目标。
#[derive(Clone, Copy, PartialEq, Eq)]
enum IndexOp {
    Stage,
    Unstage,
}

/// Git panel component
pub struct GitPanel {
    focus_handle: FocusHandle,
    workspace_path: Option<std::path::PathBuf>,
    is_local: bool,
    ssh_config: Option<SshConnectionConfig>,
    filesystem: Option<Arc<dyn FileSystem>>,
    repo: Option<GitRepository>,
    repo_error: Option<String>,
    entries: Vec<GitListEntry>,
    file_entries: Vec<GitFileEntry>,
    /// 每个文件相对 HEAD 的增删行数（`git diff --numstat`），行内 "+N -M" 展示。
    diff_stats: HashMap<String, DiffStat>,
    scroll_handle: UniformListScrollHandle,
    loading: bool,
    /// hover 行标识。此前存列表下标：entries 刷新（status 轮询/暂存操作）
    /// 后下标与行错位，hover 高亮与 diff 预取作用到错误文件。改用 path
    /// 作稳定键。
    hovered_path: Option<String>,
    /// 当前打开 diff 的文件（选中态，与文件树 `theme.selection` 对齐）。
    selected_path: Option<String>,
    /// 已折叠的分组。
    collapsed: HashSet<Section>,
    /// 当前分支名（后台获取，提交框占位用）。
    branch_name: Option<String>,
    /// ahead/behind 计数与上游名（`(领先, 落后, upstream)`，None = 尚未取回）。
    branch_status: Option<(u32, u32, Option<String>)>,
    /// 分支信息上次取回时间（节流：30s 内的重复请求直接复用现值）。
    branch_info_fetched_at: Option<Instant>,
    /// 提交输入框（懒创建：需要 Window，只能在有窗口的回调中初始化）。
    commit_editor: Option<Entity<InputState>>,
    commit_subscription: Option<Subscription>,
    /// “修正”复选框状态。
    amend_checked: bool,
    committing: bool,
    initializing: bool,
    /// 最近一次 git 写操作的错误（行内展示 + 事件透出给通知）。
    op_error: Option<String>,
    /// 右键菜单（手动模式，与文件树一致：实体 + 位置 + 订阅）。
    context_menu: Option<Entity<PopupMenu>>,
    context_menu_position: Option<gpui::Point<gpui::Pixels>>,
    context_menu_subscription: Option<Subscription>,
    context_menu_window: Option<gpui::AnyWindowHandle>,
    watcher_subscription: Option<Subscription>,
    refresh_debounce_task: Option<Task<()>>,
    status_poll_task: Option<Task<()>>,
    refresh_pending: bool,
    workspace_generation: u64,
    /// Extra remote watches for `.git/index` and `.git/HEAD` to notice stage/commit quickly.
    remote_git_watched: Vec<std::path::PathBuf>,
}

impl GitPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace_path: None,
            is_local: true,
            ssh_config: None,
            filesystem: None,
            repo: None,
            repo_error: None,
            entries: Vec::new(),
            file_entries: Vec::new(),
            diff_stats: HashMap::new(),
            scroll_handle: UniformListScrollHandle::new(),
            loading: false,
            hovered_path: None,
            selected_path: None,
            collapsed: HashSet::new(),
            branch_name: None,
            branch_status: None,
            branch_info_fetched_at: None,
            commit_editor: None,
            commit_subscription: None,
            amend_checked: false,
            committing: false,
            initializing: false,
            op_error: None,
            context_menu: None,
            context_menu_position: None,
            context_menu_subscription: None,
            context_menu_window: None,
            watcher_subscription: None,
            refresh_debounce_task: None,
            status_poll_task: None,
            refresh_pending: false,
            workspace_generation: 0,
            remote_git_watched: Vec::new(),
        }
    }

    /// 懒创建提交输入框（需要 Window；在 Tab 切换/占位点击等有窗口的回调中调用）。
    /// 已存在时同步分支占位（分支是后台获取的）。
    pub fn ensure_commit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.commit_editor.clone() {
            let want = self.commit_placeholder();
            if editor.read(cx).placeholder.as_ref() != want.as_str() {
                let _ = cx.update_entity(&editor, |s, cx| {
                    s.set_placeholder(want, window, cx);
                });
            }
            return;
        }
        let input = cx.new(|cx| {
            // VSCode 提交框行为：随内容长高（3~6 行），长行自动折行。
            // 字号与其他侧栏输入框一致（13px），比正文编辑器小一档。
            InputState::new(window, cx)
                .font_size(px(13.0))
                .auto_grow(3, 6)
                .placeholder("消息")
        });
        let sub = cx.subscribe(
            &input,
            |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => cx.notify(),
                // Ctrl/Cmd+Enter 提交（与 VSCode 一致）。
                InputEvent::PressEnter { secondary } if *secondary => {
                    this.do_commit(CommitScope::Smart, cx);
                }
                _ => {}
            },
        );
        self.commit_editor = Some(input);
        self.commit_subscription = Some(sub);
        cx.notify();
    }

    fn commit_message(&self, cx: &App) -> String {
        self.commit_editor
            .as_ref()
            .map(|e| e.read(cx).value().to_string())
            .unwrap_or_default()
    }

    fn commit_placeholder(&self) -> String {
        match self.branch_name.as_deref() {
            Some(branch) if !branch.is_empty() => {
                format!("消息（Ctrl+Enter 提交到 \"{branch}\"）")
            }
            _ => "消息（Ctrl+Enter 提交）".to_string(),
        }
    }

    fn staged_count(&self) -> usize {
        self.file_entries
            .iter()
            .filter(|e| e.status.has_staged_changes())
            .count()
    }

    /// Set the workspace path and refresh git status
    pub fn set_workspace_path(
        &mut self,
        path: std::path::PathBuf,
        is_local: bool,
        ssh_config: Option<SshConnectionConfig>,
        filesystem: Option<Arc<dyn FileSystem>>,
        cx: &mut Context<Self>,
    ) {
        // Skip if same path and backend
        if self.workspace_path.as_ref() == Some(&path)
            && self.is_local == is_local
            && self.ssh_config == ssh_config
        {
            return;
        }

        // Unwatch old path if exists
        if let Some(ref old_path) = self.workspace_path {
            self.watcher_subscription.take();
            self.refresh_debounce_task.take();
            self.status_poll_task.take();
            self.refresh_pending = false;
            if self.is_local {
                cx.unwatch_recursive_path(old_path);
            } else if let Some(ref old_fs) = self.filesystem {
                // unwatch 需要与当初 watch 同一台服务器的 fs 才能定位到引用计数
                cx.unwatch_remote_path(old_path, old_fs);
                for p in self.remote_git_watched.drain(..) {
                    cx.unwatch_remote_path(&p, old_fs);
                }
            }
        }

        self.workspace_path = Some(path.clone());
        self.is_local = is_local;
        self.ssh_config = ssh_config.clone();
        self.filesystem = filesystem.clone();
        self.refresh_debounce_task.take();
        self.status_poll_task.take();
        self.refresh_pending = false;
        self.repo = None;
        self.repo_error = None;
        self.entries.clear();
        self.file_entries.clear();
        self.diff_stats.clear();
        self.branch_name = None;
        self.branch_status = None;
        self.branch_info_fetched_at = None;
        self.selected_path = None;
        self.op_error = None;
        self.committing = false;
        self.amend_checked = false;
        self.close_context_menu(cx);
        self.loading = true;
        cx.notify();

        // Watch the workspace directory recursively.
        // 注意：`.git/index` / `.git/HEAD` 不在这里 watch —— 此时尚不知
        // 是否为仓库；对非仓库目录提前 watch 会让轮询器每轮都在 SFTP 上
        // 报 `No such file`（日志刷屏 + 浪费往返）。确认是仓库后
        // （下方 find 完成回调）再按需 watch，离开时由上面的 unwatch 分支统一释放。
        if is_local {
            cx.watch_recursive_path(&path);
        } else if let Some(ref fs) = filesystem {
            cx.watch_remote_path(&path, fs.clone());
        }

        // Set up file watching for auto-refresh
        // Use a separate subscription that checks if we're already loading
        let weak = cx.entity().downgrade();
        self.watcher_subscription = if is_local {
            let watcher = cx.fs_watcher();
            Some(
                cx.subscribe(&watcher, move |_, _, event: &FsChangeEvent, cx| {
                    if is_git_noise_path(event) {
                        return;
                    }
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.schedule_refresh(cx);
                        });
                    });
                }),
            )
        } else {
            let watcher = cx.remote_fs_watcher();
            // 参数本体后续 `GitRepository::find` 还要用，闭包捕获副本。
            let ssh_config = ssh_config.clone();
            Some(
                cx.subscribe(&watcher, move |_, _, event: &FsChangeEvent, cx| {
                    if is_git_noise_path(event) {
                        return;
                    }
                    // 远程事件按来源服务器过滤：多台服务器可能 watch 同一路径，
                    // 只响应自己后端的事件，避免其他主机的变更误触发本仓状态刷新。
                    if !event.is_from_backend(ssh_config.as_ref()) {
                        return;
                    }
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.schedule_refresh(cx);
                        });
                    });
                }),
            )
        };

        self.workspace_generation += 1;
        let generation = self.workspace_generation;
        let weak = cx.entity().downgrade();

        // 远程仓库：文件事件不可靠（commit 只改 .git 内部，远程轮询看不到），
        // 用低频定时器兜底刷新状态。本地仓库依赖 inotify 事件即可，不轮询。
        if !is_local {
            let poll_weak = weak.clone();
            self.status_poll_task = Some(cx.spawn(async move |_, cx| {
                loop {
                    cx.background_executor()
                        .timer(REMOTE_STATUS_POLL_INTERVAL)
                        .await;
                    let should_stop = poll_weak
                        .update(cx, |this, cx| {
                            if this.workspace_generation != generation {
                                return true; // 已切换工作区，退出旧轮询
                            }
                            if this.repo.is_some() && !this.loading {
                                this.schedule_refresh(cx);
                            }
                            false
                        })
                        .unwrap_or(true); // entity 已销毁，退出
                    if should_stop {
                        break;
                    }
                }
            }));
        }

        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { GitRepository::find(&path, is_local, ssh_config) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Some(repo)) => {
                        this.repo = Some(repo);
                        this.repo_error = None;
                        // 确认是仓库后才 watch `.git/index|HEAD`（stage/commit 信号，
                        // 无需等低频状态轮询）。非仓库目录不监听，避免轮询器每轮
                        // 都报 `No such file`。代际已在上方校验，fs 与 workspace
                        // 路径取当前值即为本次代际的；离开工作区时由
                        // `set_workspace_path` 的 unwatch 分支统一释放。
                        if !this.is_local {
                            if let (Some(fs), Some(root)) =
                                (this.filesystem.clone(), this.workspace_path.clone())
                            {
                                for git_path in
                                    [root.join(".git/index"), root.join(".git/HEAD")]
                                {
                                    cx.watch_remote_path(&git_path, fs.clone());
                                    this.remote_git_watched.push(git_path);
                                }
                            }
                        }
                        this.refresh(cx);
                        this.fetch_branch_info(false, cx);
                    }
                    Ok(None) => {
                        this.repo = None;
                        this.repo_error = None;
                        // 非仓库：文件树着色快照同步清空。
                        cx.emit(GitPanelEvent::StatusChanged);
                    }
                    Err(e) => {
                        this.repo = None;
                        this.repo_error = Some(e);
                        cx.emit(GitPanelEvent::StatusChanged);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 在当前工作区初始化仓库（空态“初始化仓库”按钮）。
    fn init_repo(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.workspace_path.clone() else {
            return;
        };
        if self.initializing {
            return;
        }
        self.initializing = true;
        self.op_error = None;
        cx.notify();

        let ssh_config = self.ssh_config.clone();
        let weak = cx.entity().downgrade();
        let generation = self.workspace_generation;
        cx.spawn(async move |_, cx| {
            let root_str = root.to_string_lossy().to_string();
            let result = cx
                .background_executor()
                .spawn(async move { GitRepository::init(&root_str, ssh_config.as_ref()) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.initializing = false;
                if generation != this.workspace_generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        // 强制重走 find 流程：先清路径标记再设回。
                        if let Some(p) = this.workspace_path.clone() {
                            let (is_local, ssh, fs) = (
                                this.is_local,
                                this.ssh_config.clone(),
                                this.filesystem.clone(),
                            );
                            this.workspace_path = None;
                            this.set_workspace_path(p, is_local, ssh, fs, cx);
                        }
                    }
                    Err(e) => this.report_op_error(format!("初始化仓库失败：{e}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 后台获取分支信息：分支名 + ahead/behind，合并为一次后台任务。
    /// 30s 节流，`force` 用于提交、stash 等已知改变 HEAD 的操作后绕过节流。
    fn fetch_branch_info(&mut self, force: bool, cx: &mut Context<Self>) {
        if !force
            && self
                .branch_info_fetched_at
                .is_some_and(|at| at.elapsed() < BRANCH_INFO_TTL)
        {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.branch_info_fetched_at = Some(Instant::now());
        let weak = cx.entity().downgrade();
        let generation = self.workspace_generation;
        cx.spawn(async move |_, cx| {
            let (name, status) = cx
                .background_executor()
                .spawn(async move {
                    // branch_status 失败（空仓库无 HEAD 等）按“无上游”处理，
                    // 不是面板级错误。
                    (
                        repo.branch_name(),
                        repo.branch_status().unwrap_or((0, 0, None)),
                    )
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.branch_name = name;
                this.branch_status = Some(status);
                cx.notify();
            });
        })
        .detach();
    }

    /// Refresh git status
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            self.entries.clear();
            self.file_entries.clear();
            self.diff_stats.clear();
            cx.emit(GitPanelEvent::StatusChanged);
            cx.notify();
            return;
        };

        self.loading = true;
        cx.notify();

        let repo = repo.clone();
        let repo_for_stats = repo.clone();
        let weak = cx.entity().downgrade();
        let generation = self.workspace_generation;

        cx.spawn(async move |_, cx| {
            let status_result = cx
                .background_executor()
                .spawn(async move { repo.status() })
                .await;
            // 增删行数（numstat）与 status 共用同一个仓库缓存，顺带取回。
            let diff_stats: HashMap<String, DiffStat> = cx
                .background_executor()
                .spawn(async move {
                    repo_for_stats
                        .diff_stat()
                        .unwrap_or_default()
                        .into_iter()
                        .collect()
                })
                .await;

            let _ = weak.update(cx, |this, cx| {
                this.loading = false;
                // 刷新期间切换过工作区：丢弃过期结果，避免旧仓库状态覆盖新列表。
                if generation != this.workspace_generation {
                    return;
                }

                match status_result {
                    Ok(status_entries) => {
                        this.diff_stats = diff_stats;
                        let mut file_entries: Vec<GitFileEntry> = status_entries
                            .into_iter()
                            .map(|entry| GitFileEntry {
                                path: entry.path,
                                status: entry.status,
                                section: entry.section,
                            })
                            .collect();

                        // VSCode 顺序：合并更改 < 已暂存的更改 < 更改，同组按路径排序。
                        // 兼容旧 `Tracked` / `New` 变体（order 归入更改组）。
                        file_entries.sort_by(|a, b| {
                            a.section
                                .order()
                                .cmp(&b.section.order())
                                .then_with(|| a.path.cmp(&b.path))
                        });

                        this.file_entries = file_entries;

                        // 列表结构统一经 rebuild_entries 构建（含提交记录区）。
                        this.rebuild_entries();

                        // 选中文件若已不在列表（提交/放弃后消失），清除选中态。
                        if let Some(sel) = this.selected_path.clone()
                            && !this.file_entries.iter().any(|e| e.path == sel)
                        {
                            this.selected_path = None;
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to get git status: {e}");
                        this.entries.clear();
                        this.file_entries.clear();
                        this.diff_stats.clear();
                    }
                }

                // 状态列表变了：文件树着色随快照刷新。
                cx.emit(GitPanelEvent::StatusChanged);

                // Prefetch first few files so the initial click is instant.
                if let Some(repo) = this.repo.clone().filter(|r| r.ssh_config().is_some()) {
                    for entry in this.file_entries.iter().take(3) {
                        let repo = repo.clone();
                        let path = entry.path.clone();
                        cx.background_executor()
                            .spawn(async move {
                                let _ = repo.diff_file_contents(&path);
                            })
                            .detach();
                    }
                }

                // If a refresh was requested while we were loading, schedule another one
                if this.refresh_pending && this.repo.is_some() {
                    this.refresh_pending = false;
                    this.schedule_refresh(cx);
                }

                // 周期刷新顺带补分支信息（30s TTL 内自动跳过，只有跨过
                // TTL 的那次刷新会真正发起后台任务）。
                this.fetch_branch_info(false, cx);

                cx.notify();
            });
        })
        .detach();
    }

    /// 文件树着色用快照：`(仓库根, 仓库相对路径 → 状态)`。
    ///
    /// 仓库根是文件树路径的前缀（本地为绝对路径，远程为远程绝对路径），
    /// 文件树条目剥掉该前缀后在此查状态。非仓库（或尚未就绪）返回 None。
    pub fn status_snapshot(&self) -> Option<(String, Arc<HashMap<String, FileStatus>>)> {
        let root = self.repo.as_ref()?.root().trim_end_matches('/').to_string();
        if root.is_empty() {
            return None;
        }
        let map: HashMap<String, FileStatus> = self
            .file_entries
            .iter()
            .map(|e| (e.path.clone(), e.status))
            .collect();
        Some((root, Arc::new(map)))
    }

    /// Schedule a refresh with a debounce to avoid refreshing too frequently
    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {        self.refresh_pending = true;
        if self.refresh_debounce_task.is_some() {
            return;
        }

        let weak = cx.entity().downgrade();
        self.refresh_debounce_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE_DELAY).await;
            let _ = weak.update(cx, |this, cx| {
                this.refresh_debounce_task = None;
                if this.refresh_pending && !this.loading {
                    this.refresh_pending = false;
                    this.refresh(cx);
                } else if this.refresh_pending {
                    // 上一次刷新尚未完成（loading）：保留 pending，稍后重试，
                    // 避免刷新请求在 loading 期间丢失后永远停滞。
                    this.schedule_refresh(cx);
                }
            });
        }));
    }

    /// Opens the diff tab instantly; content loads in the background.
    /// 已暂存分组的文件打开已暂存 diff（HEAD↔暂存区，只读），其余打开工作区 diff。
    fn open_diff(&mut self, entry: &GitFileEntry, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        self.selected_path = Some(entry.path.clone());
        let staged = entry.section == Section::Staged;
        // Instant emit — no await.
        cx.emit(GitPanelEvent::OpenDiffLoading {
            path: entry.path.clone(),
            repo: repo.clone(),
            staged,
        });
        cx.notify();
    }

    /// 在编辑器中打开工作区文件（右键菜单）。
    fn open_file(&self, entry: &GitFileEntry, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let full = repo.resolve_path(&entry.path);
        cx.emit(GitPanelEvent::OpenFile {
            path: std::path::PathBuf::from(full),
            ssh_config: repo.ssh_config(),
        });
    }

    /// Warms the diff cache in the background for faster open on click.
    fn prefetch_diff(&self, path: &str, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref().filter(|r| r.ssh_config().is_some()) else {
            return;
        };
        let repo = repo.clone();
        let path = path.to_owned();
        cx.background_executor()
            .spawn(async move {
                let _ = repo.diff_file_contents(&path);
            })
            .detach();
    }

    fn report_op_error(&mut self, message: String, cx: &mut Context<Self>) {
        log::warn!("git panel op failed: {message}");
        self.op_error = Some(message.clone());
        cx.emit(GitPanelEvent::OperationFailed { message });
        cx.notify();
    }

    /// Stage / Unstage 中不需要二次确认的部分。
    fn run_index_op(&self, paths: Vec<String>, op: IndexOp, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        if paths.is_empty() {
            return;
        }

        let repo = repo.clone();
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let op_paths: Vec<String> = paths.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let refs: Vec<&str> = op_paths.iter().map(|s| s.as_str()).collect();
                    match op {
                        IndexOp::Stage => repo.stage(&refs),
                        IndexOp::Unstage => repo.unstage(&refs),
                    }
                })
                .await;

            let _ = weak.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.op_error = None;
                    for path in &paths {
                        match op {
                            IndexOp::Stage => {
                                cx.emit(GitPanelEvent::StageFile { path: path.clone() });
                            }
                            IndexOp::Unstage => {
                                cx.emit(GitPanelEvent::UnstageFile { path: path.clone() });
                            }
                        }
                    }
                    this.refresh(cx);
                }
                Err(e) => {
                    let verb = match op {
                        IndexOp::Stage => "暂存",
                        IndexOp::Unstage => "取消暂存",
                    };
                    this.report_op_error(format!("{verb}失败：{e}"), cx);
                }
            });
        })
        .detach();
    }

    /// Stage a file（含未跟踪文件：`git add` 同样适用）。
    fn stage_file(&self, path: &str, cx: &mut Context<Self>) {
        self.run_index_op(vec![path.to_string()], IndexOp::Stage, cx);
    }

    /// Unstage a file
    fn unstage_file(&self, path: &str, cx: &mut Context<Self>) {
        self.run_index_op(vec![path.to_string()], IndexOp::Unstage, cx);
    }

    fn stage_all_in(&self, section: Section, cx: &mut Context<Self>) {
        let paths: Vec<String> = self
            .file_entries
            .iter()
            .filter(|e| e.section == section && e.can_stage())
            .map(|e| e.path.clone())
            .collect();
        self.run_index_op(paths, IndexOp::Stage, cx);
    }

    fn unstage_all_in(&self, section: Section, cx: &mut Context<Self>) {
        let paths: Vec<String> = self
            .file_entries
            .iter()
            .filter(|e| e.section == section && e.can_unstage())
            .map(|e| e.path.clone())
            .collect();
        self.run_index_op(paths, IndexOp::Unstage, cx);
    }

    /// 放弃更改：跟踪文件 `git restore` 回 HEAD；未跟踪文件经文件系统删除（需二次确认）。
    ///
    /// 跟踪文件同样有二次确认：`restore --staged --worktree` 会把**已暂存 +
    /// 工作区**的改动一并打回，与删除未跟踪文件一样不可逆（此前只有未跟踪
    /// 文件确认，tracked 直接执行，行为不对称且更危险——暂存的内容也丢了）。
    fn discard_file(&self, entry: &GitFileEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        if entry.status.is_conflicted() {
            self.report_op_error_deferred("冲突文件请先解决冲突后再放弃更改".to_string(), cx);
            return;
        }
        if entry.status.is_untracked() {
            self.delete_untracked_files(&[entry.path.clone()], window, cx);
            return;
        }

        let repo = repo.clone();
        let path = entry.path.clone();
        let detail = format!("将放弃「{path}」的更改（含已暂存部分），此操作不可撤销。");
        let prompt = window.prompt(
            PromptLevel::Warning,
            "放弃更改",
            Some(&detail),
            &["放弃", "取消"],
            cx,
        );
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let Ok(choice) = prompt.await else {
                return;
            };
            if choice != 0 {
                return;
            }
            let path_for_task = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { repo.discard(&[path_for_task.as_str()]) })
                .await;
            let _ = weak.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.op_error = None;
                    cx.emit(GitPanelEvent::DiscardFile { path });
                    this.refresh(cx);
                }
                Err(e) => this.report_op_error(format!("放弃更改失败：{e}"), cx),
            });
        })
        .detach();
    }

    /// 放弃整个分组的更改（VSCode 组头垃圾桶）：跟踪文件走 git，未跟踪文件删除。
    /// 统一二次确认，不可撤销。
    fn discard_all_in(&self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        let entries: Vec<GitFileEntry> = self
            .file_entries
            .iter()
            .filter(|e| {
                e.section == section && !e.status.is_conflicted() && !e.status.is_untracked()
                    && (e.status.has_unstaged_changes() || e.status.has_staged_changes())
            })
            .cloned()
            .collect();
        let untracked: Vec<String> = self
            .file_entries
            .iter()
            .filter(|e| e.section == section && e.status.is_untracked())
            .map(|e| e.path.clone())
            .collect();
        if entries.is_empty() && untracked.is_empty() {
            return;
        }
        let tracked_count = entries.len();
        let untracked_count = untracked.len();
        let detail = match (tracked_count, untracked_count) {
            (t, 0) => format!("将放弃 {t} 个文件的更改，此操作不可撤销。"),
            (0, u) => format!("将删除 {u} 个未跟踪文件，此操作不可撤销。"),
            (t, u) => {
                format!("将放弃 {t} 个文件的更改并删除 {u} 个未跟踪文件，此操作不可撤销。")
            }
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            "放弃更改",
            Some(&detail),
            &["放弃", "取消"],
            cx,
        );
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let root = self.workspace_path.clone();
        let is_local = self.is_local;
        let filesystem = self.filesystem.clone();
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let Ok(choice) = prompt.await else {
                return;
            };
            if choice != 0 {
                return;
            }
            let tracked_paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
            let untracked_for_task = untracked.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !tracked_paths.is_empty() {
                        let refs: Vec<&str> =
                            tracked_paths.iter().map(|s| s.as_str()).collect();
                        repo.discard(&refs)?;
                    }
                    for rel in &untracked_for_task {
                        let full = root
                            .as_ref()
                            .map(|r| r.join(rel))
                            .unwrap_or_else(|| std::path::PathBuf::from(rel));
                        if is_local {
                            crate::file_manager::filesystem::LocalFileSystem
                                .remove_path(&full)
                                .map_err(|e| e.to_string())?;
                        } else if let Some(ref fs) = filesystem {
                            fs.remove_path(&full).map_err(|e| e.to_string())?;
                        } else {
                            return Err("缺少远端文件系统，无法删除".to_string());
                        }
                    }
                    Ok::<_, String>(tracked_paths)
                })
                .await;
            let _ = weak.update(cx, |this, cx| match result {
                Ok(discarded) => {
                    this.op_error = None;
                    for path in discarded.into_iter().chain(untracked) {
                        cx.emit(GitPanelEvent::DiscardFile { path });
                    }
                    this.refresh(cx);
                }
                Err(e) => this.report_op_error(format!("放弃更改失败：{e}"), cx),
            });
        })
        .detach();
    }

    fn report_op_error_deferred(&self, message: String, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |this, cx| {
                this.report_op_error(message, cx);
            });
        });
    }

    /// 未跟踪文件没有 HEAD 可回退：放弃 = 删除文件（Warning 二次确认，不可撤销）。
    fn delete_untracked_files(
        &self,
        rels: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.workspace_path.clone() else {
            return;
        };
        let rels = rels.to_vec();
        let detail = if rels.len() == 1 {
            format!("将删除「{}」，此操作不可撤销。", rels[0])
        } else {
            format!("将删除 {} 个未跟踪文件，此操作不可撤销。", rels.len())
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            "删除未跟踪文件",
            Some(&detail),
            &["删除", "取消"],
            cx,
        );
        let is_local = self.is_local;
        let filesystem = self.filesystem.clone();
        let weak = cx.entity().downgrade();
        let rels_for_task = rels.clone();
        cx.spawn(async move |_, cx| {
            let Ok(choice) = prompt.await else {
                return;
            };
            if choice != 0 {
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    for rel in &rels_for_task {
                        let full = root.join(rel);
                        if is_local {
                            crate::file_manager::filesystem::LocalFileSystem
                                .remove_path(&full)
                                .map_err(|e| e.to_string())?;
                        } else if let Some(ref fs) = filesystem {
                            fs.remove_path(&full).map_err(|e| e.to_string())?;
                        } else {
                            return Err("缺少远端文件系统，无法删除".to_string());
                        }
                    }
                    Ok::<_, String>(())
                })
                .await;
            let _ = weak.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.op_error = None;
                    for path in rels {
                        cx.emit(GitPanelEvent::DiscardFile { path });
                    }
                    this.refresh(cx);
                }
                Err(e) => this.report_op_error(format!("删除失败：{e}"), cx),
            });
        })
        .detach();
    }

    /// 提交。Smart 范围：无已暂存内容时先暂存全部（VSCode 智能提交）；
    /// StagedOnly：只提交已暂存，无已暂存时报错。Amend 复选框决定是否 `--amend`。
    /// 贮藏当前更改（消息取提交框内容，空则不带消息）。
    /// 无更改时行内报错；成功后刷新面板 + 通知 diff 页重算。
    fn stash_push(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if self.file_entries.is_empty() {
            self.report_op_error("没有更改可贮藏".to_string(), cx);
            return;
        }
        let message = self.commit_message(cx);
        let message = (!message.trim().is_empty()).then_some(message);
        self.run_repo_op(cx, async move { repo.stash_push(message.as_deref()) }, "贮藏失败");
    }

    /// 弹出最近贮藏（有冲突时 git 保留贮藏并报错，用户可重试）。
    fn stash_pop(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.run_repo_op(cx, async move { repo.stash_pop() }, "弹出贮藏失败");
    }

    /// 贮藏/弹出的公共骨架：后台执行 → 成功发 RepoChanged + 刷新面板，
    /// 失败行内报错。这两类操作都会整体改变工作区（多文件同时变化），
    /// 不能像 stage/discard 那样只按单路径刷新。
    fn run_repo_op<F>(&mut self, cx: &mut Context<Self>, op: F, error_prefix: &'static str)
    where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx.background_executor().spawn(op).await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.op_error = None;
                        cx.emit(GitPanelEvent::RepoChanged);
                        this.fetch_branch_info(true, cx);
                    }
                    Err(e) => {
                        this.report_op_error(format!("{error_prefix}：{e}"), cx);
                    }
                }
                // 失败也可能已部分改动工作区（stash pop 冲突时 git 保留
                // 贮藏但改动已落盘），一律刷新列表防过期。
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 冲突解决三选一（右键菜单）：Ours/Theirs 丢弃另一侧，需二次确认；
    /// Both 保留两侧不确认。成功后 RepoChanged + 刷新。
    fn resolve_conflict(
        &mut self,
        path: &str,
        side: ConflictSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let path = path.to_string();
        let prompt = if matches!(side, ConflictSide::Ours | ConflictSide::Theirs) {
            let side_label = match side {
                ConflictSide::Ours => "当前",
                ConflictSide::Theirs => "传入",
                ConflictSide::Both => "两侧",
            };
            Some(window.prompt(
                PromptLevel::Warning,
                "解决冲突",
                Some(&format!(
                    "采用{side_label}更改解决「{path}」的冲突？另一侧的改动将被覆盖。"
                )),
                &["采用", "取消"],
                cx,
            ))
        } else {
            None
        };
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(prompt) = prompt
                && let Ok(choice) = prompt.await
                && choice != 0
            {
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move { repo.resolve_conflict(&path, side) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.op_error = None;
                        cx.emit(GitPanelEvent::RepoChanged);
                    }
                    Err(e) => this.report_op_error(format!("解决冲突失败：{e}"), cx),
                }
                // 失败也可能处于中间态（union 已写回但暂存失败），
                // 一律刷新防止列表与实际状态脱节。
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 勾选「修正」且提交框为空时，预填上次提交信息（VSCode amend 行为：
    /// 用户只想修个错字时不必重新输入全文）。
    fn prefill_amend_message(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let weak = cx.entity().downgrade();
        let generation = self.workspace_generation;
        cx.spawn(async move |_, cx| {
            let message = cx
                .background_executor()
                .spawn(async move { repo.last_commit_message() })
                .await;
            let Some(message) = message else {
                return;
            };
            let _ = weak.update(cx, |this, cx| {
                // 取回期间用户取消勾选 / 切了工作区 / 手动输入了内容：不覆盖。
                if generation != this.workspace_generation
                    || !this.amend_checked
                    || !this.commit_message(cx).trim().is_empty()
                {
                    return;
                }
                if let Some(editor) = this.commit_editor.clone() {
                    editor.update(cx, |state, cx| state.set_text(message, cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn do_commit(&mut self, scope: CommitScope, cx: &mut Context<Self>) {
        if self.committing {
            return;
        }
        let message = self.commit_message(cx);
        if message.trim().is_empty() {
            self.report_op_error("请输入提交信息".to_string(), cx);
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let amend = self.amend_checked;
        // VSCode 语义：智能提交只在**普通提交**时兜底暂存全部；amend 只提交
        // 暂存区。此前 amend + 无暂存也会 `git add -A`，把整个工作区的改动
        // 悄悄塞进被修正的提交——与注释声明的 VSCode 行为相反。
        let need_stage_all =
            matches!(scope, CommitScope::Smart) && !amend && self.staged_count() == 0;
        if matches!(scope, CommitScope::StagedOnly) && self.staged_count() == 0 {
            self.report_op_error("没有已暂存的更改，先暂存要提交的文件".to_string(), cx);
            return;
        }
        if amend && self.staged_count() == 0 {
            // 明确拒绝而不是悄悄 stage_all（见上方注释）。
            self.report_op_error(
                "修正是提交已暂存的更改：先暂存要并入的改动，或取消勾选「修正」".to_string(),
                cx,
            );
            return;
        }
        self.committing = true;
        self.op_error = None;
        cx.notify();

        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if need_stage_all {
                        repo.stage_all()?;
                    }
                    if amend {
                        repo.commit_amend(&message)
                    } else {
                        repo.commit(&message)
                    }
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.committing = false;
                match result {
                    Ok(()) => {
                        if let Some(editor) = this.commit_editor.clone() {
                            editor.update(cx, |state, cx| state.set_text(String::new(), cx));
                        }
                        this.amend_checked = false;
                        cx.emit(GitPanelEvent::CommitDone);
                        this.refresh(cx);
                        // amend 会改写 HEAD；提交也可能触发 hooks 改分支状态
                        //（如 pre-push hook、commit --fixup 流程）。分支名、
                        // ahead/behind 与提交历史一并刷新，避免显示过期。
                        this.fetch_branch_info(true, cx);
                    }
                    Err(e) => {
                        let verb = if amend { "修正" } else { "提交" };
                        this.report_op_error(format!("{verb}失败：{e}"), cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Commit 分段按钮右侧的下拉菜单（VSCode 同款选项）。
    fn open_commit_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        self.open_menu(position, window, cx, move |menu, _window, _cx| {
            let mut menu = menu;
            {
                let this = this.clone();
                menu = menu.item(
                    PopupMenuItem::new("提交").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.do_commit(CommitScope::Smart, cx);
                        });
                    }),
                );
            }
            {
                let this = this.clone();
                menu = menu.item(
                    PopupMenuItem::new("提交已暂存部分").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.do_commit(CommitScope::StagedOnly, cx);
                        });
                    }),
                );
            }
            {
                let this = this.clone();
                menu = menu.item(
                    PopupMenuItem::new("修正上次提交").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.amend_checked = true;
                            this.do_commit(CommitScope::Smart, cx);
                        });
                    }),
                );
            }
            menu = menu.separator();
            {
                let this = this.clone();
                menu = menu.item(
                    PopupMenuItem::new("贮藏更改").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.stash_push(cx);
                        });
                    }),
                );
            }
            {
                let this = this.clone();
                menu = menu.item(
                    PopupMenuItem::new("弹出最近贮藏").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.stash_pop(cx);
                        });
                    }),
                );
            }
            menu
        });
    }

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&section) {
            self.collapsed.insert(section);
        }
        self.rebuild_entries();
        cx.notify();
    }

    fn rebuild_entries(&mut self) {
        self.entries.clear();
        let mut current_section: Option<Section> = None;
        for entry in &self.file_entries {
            if current_section != Some(entry.section) {
                current_section = Some(entry.section);
                self.entries.push(GitListEntry::Header(entry.section));
            }
            if !self.collapsed.contains(&entry.section) {
                self.entries.push(GitListEntry::File(entry.clone()));
            }
        }
    }

    // ---- 右键菜单（手动模式，与文件树一致） ----

    fn open_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu,
    ) {
        self.close_context_menu(cx);
        let menu = PopupMenu::build(window, cx, build);
        menu.focus_handle(cx).focus(window, cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &gpui::DismissEvent, cx| {
            this.close_context_menu(cx);
        });
        self.context_menu = Some(menu);
        self.context_menu_position = Some(position);
        self.context_menu_subscription = Some(subscription);
        self.context_menu_window = Some(window.window_handle());
        cx.notify();
    }

    fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        self.context_menu.take();
        self.context_menu_position = None;
        self.context_menu_subscription.take();
        self.context_menu_window.take();
        cx.notify();
    }

    fn render_context_menu_overlay(&self) -> Option<gpui::AnyElement> {
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

    fn open_context_menu_for_entry(
        &mut self,
        entry: &GitFileEntry,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        let entry = entry.clone();
        let can_stage = entry.can_stage();
        let can_unstage = entry.can_unstage();
        let is_conflicted = entry.status.is_conflicted();
        self.open_menu(position, window, cx, move |menu, _window, _cx| {
            let mut menu = menu;
            {
                let this = this.clone();
                let entry = entry.clone();
                menu = menu.item(
                    PopupMenuItem::new("打开更改").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.open_diff(&entry, cx);
                        });
                    }),
                );
            }
            {
                let this = this.clone();
                let entry = entry.clone();
                menu = menu.item(
                    PopupMenuItem::new("打开文件").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.open_file(&entry, cx);
                        });
                    }),
                );
            }
            menu = menu.separator();
            // 冲突文件：三选一解决（VSCode 合并更改组的右键菜单同款）。
            if is_conflicted {
                for (label, side) in [
                    ("采用当前更改", ConflictSide::Ours),
                    ("采用传入更改", ConflictSide::Theirs),
                    ("保留两侧更改", ConflictSide::Both),
                ] {
                    let this = this.clone();
                    let entry = entry.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| {
                                this.close_context_menu(cx);
                                this.resolve_conflict(&entry.path, side, window, cx);
                            });
                        }),
                    );
                }
                menu = menu.separator();
            }
            if can_stage {
                let this = this.clone();
                let path = entry.path.clone();
                menu = menu.item(
                    PopupMenuItem::new("暂存更改").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.stage_file(&path, cx);
                        });
                    }),
                );
            }
            if can_unstage {
                let this = this.clone();
                let path = entry.path.clone();
                menu = menu.item(
                    PopupMenuItem::new("取消暂存更改").on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.unstage_file(&path, cx);
                        });
                    }),
                );
            }
            {
                let this = this.clone();
                let entry = entry.clone();
                menu = menu.item(
                    PopupMenuItem::new("放弃更改").on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| {
                            this.close_context_menu(cx);
                            this.discard_file(&entry, window, cx);
                        });
                    }),
                );
            }
            menu = menu.separator();
            {
                let path = entry.path.clone();
                menu = menu.item(
                    PopupMenuItem::new("复制路径").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    }),
                );
            }
            menu
        });
    }

    // ---- 渲染（对标 VSCode Source Control） ----

    /// 顶部提交区：输入框 + Commit 分段按钮 + 修正复选框 + 行内错误。
    fn render_commit_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let message_empty = self.commit_message(cx).trim().is_empty();
        let op_error = self.op_error.clone();
        let amend_checked = self.amend_checked;
        let committing = self.committing;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(8.0))
            .pt(px(8.0))
            .pb(px(4.0))
            .child({
                if let Some(editor) = self.commit_editor.clone() {
                    div()
                        .w_full()
                        .min_h(px(64.0))
                        .overflow_hidden()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.elevated_background)
                        .px(px(8.0))
                        .py(px(6.0))
                        .child(
                            EditorInput::new(&editor),
                        )
                        .into_any_element()
                } else {
                    div()
                        .id("git-commit-placeholder")
                        .w_full()
                        .min_h(px(64.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.elevated_background)
                        .px(px(8.0))
                        .py(px(6.0))
                        .cursor_text()
                        .text_sm()
                        .text_color(theme.text_subtle)
                        .child(self.commit_placeholder())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.ensure_commit_editor(window, cx);
                            }),
                        )
                        .into_any_element()
                }
            })
            .when_some(op_error, |this, err| {
                this.child(
                    div()
                        .w_full()
                        .rounded(px(6.0))
                        .px(px(8.0))
                        .py(px(6.0))
                        .bg(theme.error_background)
                        .text_xs()
                        .text_color(theme.error)
                        .child(err),
                )
            })
            // Commit 分段按钮：主体提交 + 右侧下拉（VSCode 同款）。
            .child(
                div()
                    .w_full()
                    .h(px(28.0))
                    .flex()
                    .flex_row()
                    .rounded(px(6.0))
                    .overflow_hidden()
                    .bg(if committing {
                        theme.elevated_background
                    } else {
                        theme.accent
                    })
                    .text_color(if committing {
                        theme.text_subtle
                    } else {
                        theme.accent_foreground
                    })
                    .child(
                        div()
                            .id("git-commit-button")
                            .flex_1()
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .when(!committing, |this| this.cursor_pointer())
                            .child(if committing {
                                "提交中…".to_string()
                            } else if amend_checked {
                                "修正".to_string()
                            } else {
                                "提交".to_string()
                            })
                            .when(!committing, |this| {
                                this.on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.do_commit(CommitScope::Smart, cx);
                                    }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w(px(1.0))
                            .h_full()
                            .flex_none()
                            .bg(theme.accent_foreground.opacity(0.3)),
                    )
                    .child(
                        div()
                            .id("git-commit-menu")
                            .w(px(32.0))
                            .h_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .tooltip(Tooltip::text("提交选项"))
                            // macOS 分段控件：悬停段轻微提亮。
                            .when(!committing, |this| {
                                this.cursor_pointer()
                                    .hover(|s| s.bg(theme.text.opacity(0.12)))
                            })
                            .child(
                                svg()
                                    .size(px(13.0))
                                    .text_color(theme.accent_foreground)
                                    .path(Icon::ChevronDownThick.path()),
                            )
                            .when(!committing, |this| {
                                this.on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(
                                        |this, event: &gpui::MouseDownEvent, window, cx| {
                                            cx.stop_propagation();
                                            this.open_commit_menu(event.position, window, cx);
                                        },
                                    ),
                                )
                            }),
                    ),
            )
            // 修正复选框 + 刷新（VSCode 提交区底部行）。
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("git-amend-check")
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.amend_checked = !this.amend_checked;
                                    // 勾选且提交框为空 → 预填上次提交信息
                                    //（后台取，回来时再校验一次勾选状态）。
                                    if this.amend_checked
                                        && this.commit_message(cx).trim().is_empty()
                                    {
                                        this.prefill_amend_message(cx);
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .w(px(14.0))
                                    .h(px(14.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .when(amend_checked, |this| this.bg(theme.accent))
                                    .when(amend_checked, |this| {
                                        this.child(
                                            svg()
                                                .size(px(10.0))
                                                .text_color(theme.accent_foreground)
                                                .path(Icon::Check.path()),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.text_muted)
                                    .child("修正"),
                            ),
                    )
                    // ahead/behind 徽标（仅任一非零时显示；上游名在 tooltip）。
                    .when_some(
                        self.branch_status.clone().and_then(|(a, b, up)| {
                            (a > 0 || b > 0).then_some((a, b, up))
                        }),
                        |this, (ahead, behind, upstream)| {
                            let label = match (ahead > 0, behind > 0) {
                                (true, true) => format!("↑{ahead} ↓{behind}"),
                                (true, false) => format!("↑{ahead}"),
                                _ => format!("↓{behind}"),
                            };
                            let tooltip = match upstream {
                                Some(up) => format!("领先上游 {ahead} 个提交，落后 {behind} 个（{up}）"),
                                None => format!("领先上游 {ahead} 个提交，落后 {behind} 个"),
                            };
                            this.child(
                                div()
                                    .id("git-ahead-behind")
                                    .flex_none()
                                    .text_xs()
                                    .text_color(theme.text_muted)
                                    .tooltip(Tooltip::text(tooltip))
                                    .child(label),
                            )
                        },
                    )
                    .child(div().flex_1())
                    .when(!message_empty, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.text_subtle)
                                .child("Ctrl+Enter 提交"),
                        )
                    })
                    .child({
                        div()
                            .id("git-refresh")
                            .flex_none()
                            .w(px(20.0))
                            .h(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                            .tooltip(Tooltip::text("刷新"))
                            .child(
                                svg()
                                    .size(px(13.0))
                                    .text_color(theme.text.opacity(0.85))
                                    .path(Icon::RefreshThick.path()),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.op_error = None;
                                    this.refresh(cx);
                                    this.fetch_branch_info(true, cx);
                                }),
                            )
                    }),
            )
    }

    /// 分组头：折叠箭头 + 标题 + 计数 + 悬停批量操作（VSCode 同款）。
    fn render_header(&self, section: Section, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let theme = cx.theme().ui;
        let collapsed = self.collapsed.contains(&section);

        let count = self
            .file_entries
            .iter()
            .filter(|e| e.section == section)
            .count();

        // 折叠箭头：面板专版（12 viewBox 描边 1.7 旋转双胞胎）——同尺寸下
        // › 竖向 4×8、⌄ 横向 8×4，光学一致且明显小于操作图标。共享的
        // chevron.down/right 保持原件，供侧栏/菜单等处使用。
        let chevron = if collapsed {
            svg()
                .size(px(12.0))
                .text_color(theme.text_muted)
                .path(Icon::ChevronRightThick.path())
        } else {
            svg()
                .size(px(12.0))
                .text_color(theme.text_muted)
                .path(Icon::ChevronDownThick.path())
        };

        // VSCode 组头操作：更改 → 暂存全部(+) + 放弃全部(垃圾桶)；
        // 已暂存的更改 → 全部取消暂存(−)；合并更改 → 无。
        let show_stage_all = section == Section::Changes
            && self.file_entries.iter().any(|e| e.section == section && e.can_stage());
        let show_discard_all = section == Section::Changes
            && self.file_entries.iter().any(|e| {
                e.section == section
                    && !e.status.is_conflicted()
                    && (e.status.has_unstaged_changes() || e.status.has_staged_changes())
            });
        let show_unstage_all = matches!(section, Section::Staged)
            && self.file_entries.iter().any(|e| e.section == section && e.can_unstage());

        div()
            .id(format!("git-header-{section:?}"))
            .w_full()
            .h(px(28.0))
            .flex()
            .items_center()
            .px(px(8.0))
            .gap(px(2.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.toggle_section(section, cx);
                }),
            )
            .child(
                div()
                    .w(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(chevron),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(section.title().to_uppercase()),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(format!("{count}")),
            )
            .child(div().flex_1())
            .when(show_stage_all, |this| {
                this.child(
                    div()
                        .id(format!("git-stage-all-{section:?}"))
                        .flex_none()
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                        .tooltip(Tooltip::text("暂存全部更改"))
                        .child(
                            svg()
                                .size(px(12.0))
                                .text_color(theme.text.opacity(0.85))
                                .path(Icon::PlusThick.path()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.stage_all_in(section, cx);
                            }),
                        ),
                )
            })
            .when(show_unstage_all, |this| {
                this.child(
                    div()
                        .id(format!("git-unstage-all-{section:?}"))
                        .flex_none()
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                        .tooltip(Tooltip::text("取消暂存全部更改"))
                        .child(
                            svg()
                                .size(px(12.0))
                                .text_color(theme.text.opacity(0.85))
                                .path(Icon::MinusThick.path()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.unstage_all_in(section, cx);
                            }),
                        ),
                )
            })
            .when(show_discard_all, |this| {
                this.child(
                    div()
                        .id(format!("git-discard-all-{section:?}"))
                        .flex_none()
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                        .tooltip(Tooltip::text("放弃全部更改"))
                        .child(
                            svg()
                                .size(px(12.0))
                                .text_color(theme.text.opacity(0.85))
                                .path(Icon::DiscardThick.path()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.discard_all_in(section, window, cx);
                            }),
                        ),
                )
            })
    }

    /// 文件行：图标 + 文件名 + 右侧目录 + 状态字母；
    /// 悬停时右侧替换为打开/放弃/暂存操作（VSCode 同款）。
    fn render_file_entry(
        &self,
        entry: &GitFileEntry,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let theme = cx.theme().ui;
        let path = entry.path.clone();
        let is_hovered = self.hovered_path.as_deref() == Some(path.as_str());
        let is_selected = self.selected_path.as_deref() == Some(path.as_str());
        let badge_letter = entry.badge_letter();
        let badge_color = entry.badge_color(&theme);
        let entry_clone = entry.clone();
        let entry_for_menu = entry.clone();
        let show_actions = is_hovered || is_selected;

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

        let hover_path = path.clone();
        let file_icon = file_icon_or_default(std::path::Path::new(&path));
        let can_stage = entry.can_stage();
        let can_unstage = entry.can_unstage();
        let name = entry.file_name().to_string();
        // 相对 HEAD 的增删行数（numstat 没有的文件——如未跟踪——不显示计数）。
        let diff_stat = self.diff_stats.get(&path).copied();

        // 悬停操作按钮（20px 命中区，组头同款），附功能说明 tooltip。
        // 图标 14px / 85% 白：行高 24px 内保持可点性，同时线条足够清晰。
        let action_btn = |id: String, icon: Icon, tip: &'static str| {
            div()
                .id(id)
                .flex_none()
                .w(px(20.0))
                .h(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                .tooltip(Tooltip::text(tip))
                .child(
                    svg()
                        .size(px(14.0))
                        .text_color(theme.text.opacity(0.85))
                        .path(icon.path()),
                )
        };

        div()
            .id(format!("git-entry-{index}"))
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .px(px(4.0))
            .cursor_pointer()
            .tooltip(Tooltip::text(path.clone()))
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                if *hovered {
                    this.hovered_path = Some(hover_path.clone());
                    // Warm cache on hover.
                    this.prefetch_diff(&hover_path, cx);
                } else if this.hovered_path.as_deref() == Some(hover_path.as_str()) {
                    this.hovered_path = None;
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.open_diff(&entry_clone, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.selected_path = Some(entry_for_menu.path.clone());
                    this.open_context_menu_for_entry(&entry_for_menu, event.position, window, cx);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w_full()
                    .h_full()
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .bg(row_background)
                    .child(
                        svg()
                            .size(px(16.0))
                            .flex_none()
                            .text_color(if is_selected {
                                theme.accent_foreground
                            } else {
                                // macOS 次级标签层级；icon_muted 在深色主题里过浅。
                                theme.text_muted
                            })
                            .path(file_icon.path()),
                    )
                    .child(
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
                    .when(!show_actions, |this| {
                        this.when_some(diff_stat, |this, stat| {
                            this.child(
                                // "+N -M" 增删行数：定宽右对齐让各行的数字成列。
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .text_xs()
                                    .child(
                                        div()
                                            .min_w(px(38.0))
                                            .text_right()
                                            .text_color(theme.status_added)
                                            .child(format!("+{}", stat.added)),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(38.0))
                                            .text_color(theme.status_deleted)
                                            .child(format!("-{}", stat.deleted)),
                                    ),
                            )
                        })
                        .child(
                            // 状态字母徽标 M/A/D/U/C
                            div()
                                .flex_none()
                                .w(px(16.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(if is_selected { row_text } else { badge_color })
                                .child(badge_letter.to_string()),
                        )
                    })
                    .when(show_actions, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(2.0))
                                .child(
                                    action_btn(
                                        format!("git-row-open-{path}"),
                                        Icon::GoToFile,
                                        "打开文件",
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener({
                                            let entry = entry.clone();
                                            move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.open_file(&entry, cx);
                                            }
                                        }),
                                    ),
                                )
                                .child({
                                    let entry = entry.clone();
                                    action_btn(
                                        format!("git-row-discard-{path}"),
                                        Icon::DiscardThick,
                                        "放弃更改",
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.discard_file(&entry, window, cx);
                                        }),
                                    )
                                })
                                .when(can_stage, |this| {
                                    let p = path.clone();
                                    this.child(
                                        action_btn(
                                            format!("git-row-stage-{p}"),
                                            Icon::PlusThick,
                                            "暂存更改",
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.stage_file(&p, cx);
                                            }),
                                        ),
                                    )
                                })
                                .when(can_unstage, |this| {
                                    let p = path.clone();
                                    this.child(
                                        action_btn(
                                            format!("git-row-unstage-{p}"),
                                            Icon::MinusThick,
                                            "取消暂存更改",
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.unstage_file(&p, cx);
                                            }),
                                        ),
                                    )
                                }),
                        )
                    }),
            )
    }
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

crate::impl_focusable!(GitPanel);

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        // 同步提交框占位中的分支名（分支是后台获取的，到了后刷新一次占位即停）。
        if let Some(editor) = self.commit_editor.clone() {
            let want = self.commit_placeholder();
            if editor.read(cx).placeholder.as_ref() != want.as_str() {
                let _ = cx.update_entity(&editor, |s, cx| {
                    s.set_placeholder(want, window, cx);
                });
            }
        }

        if self.loading && self.entries.is_empty() && self.repo.is_none() {
            return div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_subtle)
                .child("加载中…")
                .into_any_element();
        }

        if self.repo.is_none() {
            let message = self
                .repo_error
                .clone()
                .unwrap_or_else(|| "当前文件夹不是 Git 仓库".to_string());
            let initializing = self.initializing;
            let op_error = self.op_error.clone();
            return div()
                .w_full()
                .h_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .px(px(16.0))
                .text_sm()
                .text_color(theme.text_subtle)
                .child(message)
                .when_some(op_error, |this, err| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(theme.error)
                            .child(err),
                    )
                })
                .child(
                    div()
                        .id("git-init-repo")
                        .px(px(12.0))
                        .h(px(28.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .bg(if initializing {
                            theme.elevated_background
                        } else {
                            theme.accent
                        })
                        .text_color(if initializing {
                            theme.text_subtle
                        } else {
                            theme.accent_foreground
                        })
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .when(!initializing, |this| this.cursor_pointer())
                        .child(if initializing {
                            "初始化中…".to_string()
                        } else {
                            "初始化仓库".to_string()
                        })
                        .when(!initializing, |this| {
                            this.on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.init_repo(cx);
                                }),
                            )
                        }),
                )
                .child(
                    div()
                        .id("git-empty-refresh")
                        .px(px(12.0))
                        .h(px(26.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .text_color(theme.text_muted)
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                        .child("重新检测")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                // 重新触发 find：清空路径后设回同一路径
                                if let Some(p) = this.workspace_path.clone() {
                                    let (is_local, ssh, fs) = (
                                        this.is_local,
                                        this.ssh_config.clone(),
                                        this.filesystem.clone(),
                                    );
                                    this.workspace_path = None;
                                    this.set_workspace_path(p, is_local, ssh, fs, cx);
                                }
                            }),
                        ),
                )
                .into_any_element();
        }

        let menu_overlay = self.render_context_menu_overlay();

        let list_element: gpui::AnyElement = if self.entries.is_empty() {
            div()
                .w_full()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_subtle)
                .child("没有更改")
                .into_any_element()
        } else {
            let entry_count = self.entries.len();
            let scroll_handle = self.scroll_handle.clone();

            uniform_list(
                "git-panel-list",
                entry_count,
                cx.processor(|this, range: std::ops::Range<usize>, _window, _cx| {
                    range
                        .map(|ix| {
                            let Some(entry) = this.entries.get(ix).cloned() else {
                                return div().h(px(LIST_ROW_HEIGHT)).into_any_element();
                            };
                            match entry {
                                GitListEntry::Header(section) => {
                                    this.render_header(section, _cx).into_any_element()
                                }
                                GitListEntry::File(file_entry) => this
                                    .render_file_entry(&file_entry, ix, _cx)
                                    .into_any_element(),
                            }
                        })
                        .collect()
                }),
            )
            .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
            .track_scroll(&scroll_handle)
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .child(self.render_commit_box(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .pt(px(4.0))
                    .child(list_element),
            )
            .when_some(menu_overlay, |this, overlay| this.child(overlay))
            .into_any_element()
    }
}

/// 判断事件是否来自 git 内部的高频噪声路径。
///
/// 注意：不能一刀切过滤所有 `.git` 事件——`git commit` / `add` / `checkout`
/// 只修改 `.git/index`、`.git/HEAD`、`.git/refs/*`，工作区文件不变；
/// 如果全部忽略，终端里提交后 Git 面板会永远停留在过期状态。
/// 这里只过滤高频噪声（objects/logs/hooks/锁文件），
/// 放行 index/HEAD/refs 等能反映仓库状态变化的信号路径。
fn is_git_noise_path(event: &FsChangeEvent) -> bool {
    let mut components = event.path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() != ".git" {
            continue;
        }
        // 扫描 .git 下所有层级：锁文件/临时文件可能出现在任意深度
        // （如 .git/index.lock、.git/refs/heads/main.lock）
        for rest in components.by_ref() {
            match rest.as_os_str().to_str() {
                Some("objects") | Some("logs") | Some("hooks") | Some("info") => return true,
                Some(name) if name.ends_with(".lock") || name.starts_with("tmp_") => {
                    return true;
                }
                _ => {}
            }
        }
        // index/HEAD/refs/MERGE_HEAD 等状态信号路径（含 rebase 中间态目录）→ 放行刷新
        return false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::git_status::{StageStatus, StatusCode, TrackedStatus};
    use crate::file_manager::watcher::FsChangeKind;

    fn event(path: &str) -> FsChangeEvent {
        FsChangeEvent::new(std::path::PathBuf::from(path), FsChangeKind::Modify)
    }

    #[test]
    fn git_state_signal_paths_are_not_noise() {
        // commit/add/checkout 只改这些路径，必须触发刷新
        assert!(!is_git_noise_path(&event("/repo/.git/index")));
        assert!(!is_git_noise_path(&event("/repo/.git/HEAD")));
        assert!(!is_git_noise_path(&event("/repo/.git/refs/heads/main")));
        assert!(!is_git_noise_path(&event("/repo/.git/COMMIT_EDITMSG")));
        assert!(!is_git_noise_path(&event("/repo/.git/MERGE_HEAD")));
    }

    #[test]
    fn git_high_frequency_paths_are_noise() {
        // objects/logs/hooks 每次写对象都会刷事件，必须忽略
        assert!(is_git_noise_path(&event("/repo/.git/objects/ab/cdef123")));
        assert!(is_git_noise_path(&event("/repo/.git/logs/HEAD")));
        assert!(is_git_noise_path(&event("/repo/.git/hooks/pre-commit")));
        assert!(is_git_noise_path(&event("/repo/.git/info/exclude")));
    }

    #[test]
    fn git_lock_and_temp_files_are_noise() {
        assert!(is_git_noise_path(&event("/repo/.git/index.lock")));
        assert!(is_git_noise_path(&event("/repo/.git/refs/heads/main.lock")));
        assert!(is_git_noise_path(&event("/repo/.git/tmp_obj_123")));
    }

    #[test]
    fn working_tree_and_non_git_paths_are_not_noise() {
        assert!(!is_git_noise_path(&event("/repo/src/main.rs")));
        assert!(!is_git_noise_path(&event("/other/project/file.txt")));
        // .git 目录本身变化（init 等）→ 刷新
        assert!(!is_git_noise_path(&event("/repo/.git")));
        // 名字恰好含 .git 的普通文件不受影响
        assert!(!is_git_noise_path(&event("/repo/github.md")));
    }

    /// 回归：`set_workspace_path` 不得在确认仓库前 watch `.git/index|HEAD`。
    /// 否则非仓库目录会让远程轮询器每轮都在 SFTP 上报 `No such file`
    /// （日志刷屏 + 浪费往返）；`.git` 监听只在 find 确认 `Ok(Some)` 后建立。
    ///
    /// 只断言同步行为（set 返回后立即检查），不依赖后台 find 任务的时序；
    /// find 指向本机关闭端口，会快速失败，不影响断言。
    #[gpui::test]
    fn git_panel_defers_git_file_watches_until_repo_confirmed(
        cx: &mut gpui::TestAppContext,
    ) {
        // set_workspace_path 内部经 WatcherContext 取全局 watcher，先初始化。
        cx.update(|cx| crate::file_manager::watcher::init(cx));
        let panel = cx.new(GitPanel::new);
        let mut config = SshConnectionConfig::local();
        config.host = "127.0.0.1".to_string();
        config.port = 9;
        let fs: Arc<dyn FileSystem> =
            Arc::new(crate::file_manager::filesystem::RemoteFileSystem::new(config.clone()));
        panel.update(cx, |p, cx| {
            p.set_workspace_path(
                std::path::PathBuf::from("/root/sn105_workspace/docs"),
                false,
                Some(config),
                Some(fs),
                cx,
            );
        });
        panel.update(cx, |p, _| {
            assert!(
                p.remote_git_watched.is_empty(),
                "确认仓库前不得 watch .git 文件，实际: {:?}",
                p.remote_git_watched
            );
            assert_eq!(
                p.workspace_path.as_deref(),
                Some(std::path::Path::new("/root/sn105_workspace/docs"))
            );
        });
    }

    #[test]
    fn untracked_files_stage_not_unstage() {
        // 回归：未跟踪文件必须走 `git add`（can_stage），而不是 `git reset`。
        // VSCode 模型下未跟踪文件归入“更改”组。
        let entry = GitFileEntry {
            path: "new.txt".to_string(),
            status: FileStatus::Untracked,
            section: Section::Changes,
        };
        assert!(entry.can_stage());
        assert!(!entry.can_unstage());
        assert!(entry.should_stage());
        assert_eq!(entry.badge_letter(), 'U');
        assert_eq!(entry.file_name(), "new.txt");
        assert_eq!(entry.parent_dir(), None);
    }

    #[test]
    fn staged_file_unstages_not_stages() {
        let entry = GitFileEntry {
            path: "a.rs".to_string(),
            status: FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Modified,
                worktree_status: StatusCode::Unmodified,
            }),
            section: Section::Staged,
        };
        assert_eq!(entry.status.staging(), StageStatus::Staged);
        assert!(!entry.can_stage());
        assert!(entry.can_unstage());
        assert!(!entry.should_stage());
    }

    #[test]
    fn file_name_and_dir_split() {
        let entry = GitFileEntry {
            path: "src/main.rs".to_string(),
            status: FileStatus::Untracked,
            section: Section::Changes,
        };
        assert_eq!(entry.file_name(), "main.rs");
        assert_eq!(entry.parent_dir(), Some("src"));
    }

    /// 提交框式输入（PlainText multi_line）在窄宽度下必须软换行，
    /// 而不是横向无限延伸。回归：min_h/max_h 固定盒 + w_full 约束。
    struct WrapProbeHost {
        input: Entity<InputState>,
    }

    impl Render for WrapProbeHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> impl IntoElement {
            use crate::components::input::EditorInput;
            div()
                .w(px(300.))
                .child(
                    div()
                        .w_full()
                        .min_h(px(64.0))
                        .max_h(px(160.0))
                        .overflow_hidden()
                        .child(EditorInput::new(&self.input)),
                )
        }
    }

    #[gpui::test]
    fn commit_style_input_soft_wraps(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;

        cx.update(|cx| crate::theme::init(cx));
        let long = "这是一段很长的提交信息应该在到达盒子右缘时自动折行而不是横向滚出视野 ".repeat(6);
        let window = cx.open_window(gpui::size(px(400.), px(200.)), |window, cx| {
            let input = cx.new(|cx| {
                // 与生产提交框同配置：auto_grow(3, 6)。
                InputState::new(window, cx)
                    .auto_grow(3, 6)
                    .placeholder("消息")
            });
            input.update(cx, |state, cx| state.set_text(long.clone(), cx));
            WrapProbeHost { input }
        });
        let handle: gpui::AnyWindowHandle = window.into();
        let cx = &mut gpui::VisualTestContext::from_window(handle, cx);
        cx.run_until_parked();

        // VisualTestContext::update 会进 window 栈，直接用底层的 App 上下文读实体。
        let wrap_rows = cx.cx.update(|cx| {
            window
                .read_with(cx, |host, cx| {
                    host.input.read(cx).display_map.wrap_row_count()
                })
                .unwrap_or(0)
        });
        assert!(
            wrap_rows > 1,
            "单行长文本在 300px 盒子里必须折成多行，实际 wrap 行数={wrap_rows}"
        );

        // 真实打字：聚焦后输入长文本，wrap 行数必须增长；
        // 回车必须插入换行（VSCode 提交框行为），而不是提交或吞掉。
        let handle = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, cx| {
                    host.input.read(cx).focus_handle.clone()
                })
                .unwrap()
        });
        cx.update(|w, cx| {
            w.focus(&handle, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("第一行提交信息 ");
        cx.simulate_input(&"很长的补充说明 ".repeat(10));
        cx.run_until_parked();
        let (typed_text, typed_wraps) = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, cx| {
                    let state = host.input.read(cx);
                    (
                        state.value().to_string(),
                        state.display_map.wrap_row_count(),
                    )
                })
                .unwrap_or_default()
        });
        assert!(
            typed_text.contains("补充说明"),
            "打字必须进入输入框，实际={typed_text:?}"
        );
        assert!(
            typed_wraps > 1,
            "打字输入的长文本必须折行，实际 wrap 行数={typed_wraps}"
        );

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let after_enter = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, cx| {
                    host.input.read(cx).value().to_string()
                })
                .unwrap_or_default()
        });
        assert!(
            after_enter.contains('\n'),
            "回车必须插入换行，实际={after_enter:?}"
        );

        // 纯中文无空格长行（典型提交信息）：必须在任意字符处折行，
        // 不能依赖空格断点横向滚出。
        let input = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, _| host.input.clone())
                .unwrap()
        });
        input.update(cx, |state, cx| {
            state.set_text("提交信息没有换行需要检查修复验证回归测试覆盖".repeat(8), cx);
        });
        cx.run_until_parked();
        let cjk_wraps = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, cx| {
                    host.input.read(cx).display_map.wrap_row_count()
                })
                .unwrap_or(0)
        });
        assert!(
            cjk_wraps > 1,
            "纯中文无空格长行必须折行，实际 wrap 行数={cjk_wraps}"
        );

        // 无断点 ASCII 长串同样必须折行（不能横向滚出）。
        input.update(cx, |state, cx| {
            state.set_text("x".repeat(300), cx);
        });
        cx.run_until_parked();
        let ascii_wraps = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, cx| {
                    host.input.read(cx).display_map.wrap_row_count()
                })
                .unwrap_or(0)
        });
        assert!(
            ascii_wraps > 1,
            "无断点 ASCII 长串必须折行，实际 wrap 行数={ascii_wraps}"
        );

        // auto_grow：多行硬换行后盒子行数增长（VSCode 提交框随内容长高，上限 6）。
        input.update(cx, |state, cx| {
            state.set_text("l1\nl2\nl3\nl4\nl5", cx);
        });
        cx.run_until_parked();
        let grown_rows = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, cx| host.input.read(cx).mode.rows())
                .unwrap_or(0)
        });
        assert_eq!(grown_rows, 5, "5 行内容盒子应长到 5 行，实际={grown_rows}");
        // 超上限钳制在 6 行。
        input.update(cx, |state, cx| {
            state.set_text("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8", cx);
        });
        cx.run_until_parked();
        let capped_rows = cx.cx.update(|cx| {
            window
                .read_with(cx, |host: &WrapProbeHost, cx| host.input.read(cx).mode.rows())
                .unwrap_or(0)
        });
        assert_eq!(capped_rows, 6, "8 行内容应钳制在 6 行，实际={capped_rows}");
    }
}
