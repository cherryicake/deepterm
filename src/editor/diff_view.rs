//! Editor-based git diff view, modeled after zed's split diff.
//!
//! Both layouts are built on the real editor (`InputState` code editor), so
//! font, colors, selection, scrolling and undo behave exactly like a normal
//! editor tab:
//!
//! - **Split**: left = HEAD content (read-only), right = working tree
//!   content (fully editable). Added/deleted line backgrounds and
//!   word-level highlights are injected through the editor's diff-decoration
//!   API. Scroll is synchronized through the hunk mapping, and every hunk
//!   shows a revert button that restores the HEAD version of that block.
//! - **Unified**: single read-only editor with the merged old+new text
//!   (deleted lines interleaved), line backgrounds and word highlights.
//!
//! The default layout preference is `Auto`: unified (single column) is shown
//! by default, and the view automatically switches to split once the panel
//! width reaches `SPLIT_MIN_WIDTH`. The toolbar button cycles
//! Auto → Unified → Split → Auto to pin a specific layout.
//!
//! Saving (toolbar button or cmd+S via the editor panel) writes the working
//! tree file through the repository backend (local fs / SFTP).

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, HighlightStyle, Hsla, Render, Subscription, WeakEntity, Window, div,
    point, px,
};

use crate::components::input::{EditorInput, InputState};
use crate::editor::editor::Editor;
use crate::editor::syntax::{HighlightTheme, SimpleHighlighter};
use crate::theme::ActiveTheme;
use ropey::Rope;

use super::diff_model::{
    DiffHunk, LineKind, UnifiedSource, compute_hunks, line_count, line_range, line_starts,
};

/// Maximum length of a line that participates in syntax highlighting.
/// Longer lines (e.g. minified content) fall back to the default color.
const MAX_HIGHLIGHT_LINE_LENGTH: usize = 10_000;
use super::git_repository::GitRepository;

/// 行级背景着色：`(0 基行号, 背景色)`，按行号升序。
type LineColors = Vec<(u32, Hsla)>;
/// 词级背景着色：`(字节范围, 背景色)`。
type WordColors = Vec<(Range<usize>, Hsla)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffMode {
    Unified,
    Split,
}

/// Layout preference. `Auto` (default) shows the unified single-column
/// layout and switches to split only when the available width is sufficient.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutPreference {
    Auto,
    Unified,
    Split,
}

/// Minimum content width for the automatic split layout (two readable
/// editor columns with gutters).
const SPLIT_MIN_WIDTH: f32 = 960.0;

/// 陈旧 hunk 与重算 hunk 之间允许的最大行号偏移。
///
/// 用户在块附近连续编辑会让目标块的行号有些许漂移，这个容差足以覆盖；
/// 但相距过远就意味着误匹配，此时宁可放弃还原，也不能把 HEAD 内容写到
/// 无关的位置上。
const REVERT_MATCH_TOLERANCE: u32 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// Everything the UI needs to render the unified view, produced by the
/// background recompute task.
struct UnifiedPayload {
    text: String,
    runs: Vec<(Range<usize>, HighlightStyle)>,
    line_colors: Vec<(u32, Hsla)>,
    word_colors: Vec<(Range<usize>, Hsla)>,
    labels: Vec<Option<u32>>,
    /// Merged row of each hunk's first changed line（导航/初始定位用）。
    hunk_rows: Vec<u32>,
    /// 折叠占位行信息（展开按钮定位用）。
    separators: Vec<super::diff_model::SeparatorRow>,
}

/// 文本指纹：判断 hunks 是否基于当前文本计算。
/// 只存长度与哈希，避免为校验再保留一份完整文本副本。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TextFingerprint {
    len: usize,
    hash: u64,
}

impl TextFingerprint {
    fn of(text: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        Self {
            len: text.len(),
            hash: hasher.finish(),
        }
    }
}

pub struct DiffView {
    path: String,
    repo: GitRepository,
    editor: WeakEntity<Editor>,
    /// File content at HEAD.
    old_text: Arc<str>,
    /// Working-tree content at last load/save (dirty baseline).
    saved_text: String,
    left: Entity<InputState>,
    right: Entity<InputState>,
    unified: Entity<InputState>,
    hunks: Vec<DiffHunk>,
    /// `hunks` 计算时所依据的右栏文本指纹（长度 + 哈希）。
    /// 用于判断行号是否已过期：hunks 由 200ms 防抖的后台任务产出，
    /// 用户编辑后立刻点"还原此块"会拿到基于旧文本的 `new_rows`。
    hunks_source: TextFingerprint,
    additions: usize,
    deletions: usize,
    preference: LayoutPreference,
    /// Content width reported by the editor panel; drives `Auto` layout.
    available_width: f32,
    /// Per-source syntax trees for the unified view: HEAD text and
    /// working-tree text (each valid, unlike the merged display text).
    old_highlighter: SimpleHighlighter,
    new_highlighter: SimpleHighlighter,
    /// Texts the two highlighters were last parsed with (skip re-parses).
    old_parsed: Option<Arc<str>>,
    new_parsed: Option<String>,
    dirty: bool,
    saving: bool,
    refreshing: bool,
    /// True while the initial content fetch is pending; editors are empty
    /// with a spinner and recompute is deferred.
    loading: bool,
    /// Error from the last fetch, if any.
    load_error: Option<String>,
    /// Error from the last save, if any.
    save_error: Option<String>,
    recompute_gen: u32,
    scroll_syncing: bool,
    /// 已展开的折叠区 id（= 隐藏区 new 侧起始行）。
    expanded_separators: std::collections::HashSet<u32>,
    /// 最近一次 collapsed 构建产出的 hunk 行号/分隔行（导航与跳转用）。
    unified_hunk_rows: Vec<u32>,
    unified_separators: Vec<super::diff_model::SeparatorRow>,
    /// 首次 recompute 是否已完成（初始跳转只做一次）。
    initial_jump_done: bool,
    /// 待执行的初始跳转：recompute 完成时设置，render 时等 line_height
    /// 可用后一次性消费，滚动到首个变更处。
    pending_jump: bool,
    _subscriptions: Vec<Subscription>,
}

impl DiffView {
    /// Creates the view with empty editors and fetches content in the background.
    /// The tab appears instantly; batched remote fetch makes it feel local.
    pub fn new_loading(
        path: String,
        repo: GitRepository,
        editor: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language = crate::editor::guess_language_from_extension(std::path::Path::new(&path));
        let left = Self::new_editor(&language, "", true, window, cx);
        let right = Self::new_editor(&language, "", false, window, cx);
        let unified = Self::new_editor(&language, "", true, window, cx);

        let mut this = Self {
            path,
            repo,
            editor,
            old_text: "".into(),
            saved_text: String::new(),
            left,
            right,
            unified,
            hunks: Vec::new(),
            hunks_source: TextFingerprint::default(),
            additions: 0,
            deletions: 0,
            preference: LayoutPreference::Auto,
            available_width: 0.0,
            old_highlighter: SimpleHighlighter::new(&language),
            new_highlighter: SimpleHighlighter::new(&language),
            old_parsed: None,
            new_parsed: None,
            dirty: false,
            saving: false,
            refreshing: false,
            loading: true,
            load_error: None,
            save_error: None,
            recompute_gen: 0,
            scroll_syncing: false,
            expanded_separators: std::collections::HashSet::new(),
            unified_hunk_rows: Vec::new(),
            unified_separators: Vec::new(),
            initial_jump_done: false,
            pending_jump: false,
            _subscriptions: Vec::new(),
        };

        // Same subscriptions as `new`: live edits and scroll sync.
        let right = this.right.clone();
        this._subscriptions.push(cx.subscribe(
            &right,
            |this, _, event: &crate::components::input::InputEvent, cx| {
                if matches!(event, crate::components::input::InputEvent::Change) {
                    this.on_right_changed(cx);
                }
            },
        ));
        this._subscriptions
            .push(cx.observe(&this.left.clone(), |this, _, cx| {
                this.sync_scroll(Side::Left, cx);
            }));
        this._subscriptions
            .push(cx.observe(&this.right.clone(), |this, _, cx| {
                this.sync_scroll(Side::Right, cx);
            }));

        this.fetch_contents(cx);
        this
    }

    fn fetch_contents(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.load_error = None;
        cx.notify();

        let repo = self.repo.clone();
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.diff_file_contents(&path) })
                .await;

            this.update(cx, |view, cx| {
                match result {
                    Ok((old_text, new_text)) => {
                        view.loading = false;
                        view.load_error = None;
                        let language = crate::editor::guess_language_from_extension(
                            std::path::Path::new(&view.path),
                        );
                        view.old_highlighter = SimpleHighlighter::new(&language);
                        view.new_highlighter = SimpleHighlighter::new(&language);
                        view.old_text = old_text.clone().into();
                        view.saved_text = new_text.clone();
                        view.old_parsed = None;
                        view.new_parsed = None;
                        view.left
                            .update(cx, |state, cx| state.set_text(old_text, cx));
                        view.right
                            .update(cx, |state, cx| state.set_text(new_text, cx));
                        view.recompute(cx);
                    }
                    Err(error) => {
                        view.loading = false;
                        view.load_error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn new_editor(
        language: &str,
        text: &str,
        readonly: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true);
            state.set_value(text, window, cx);
            state.disabled = readonly;
            // No soft wrap in diff views: hunk-anchored scroll sync assumes a
            // uniform line height (same as zed's diff default).
            state.soft_wrap = false;
            state
        })
    }

    // ---- colors -----------------------------------------------------------

    fn added_line_bg(cx: &App) -> Hsla {
        cx.theme().status_added.opacity(0.14)
    }

    fn deleted_line_bg(cx: &App) -> Hsla {
        cx.theme().status_deleted.opacity(0.14)
    }

    fn added_word_bg(cx: &App) -> Hsla {
        cx.theme().status_added.opacity(0.38)
    }

    fn deleted_word_bg(cx: &App) -> Hsla {
        cx.theme().status_deleted.opacity(0.38)
    }

    // ---- diff recomputation ------------------------------------------------

    fn on_right_changed(&mut self, cx: &mut Context<Self>) {
        let current = self.right.read(cx).value().to_string();
        self.dirty = current != self.saved_text;
        self.schedule_recompute(cx);
        cx.notify();
    }

    fn schedule_recompute(&mut self, cx: &mut Context<Self>) {
        self.recompute_gen += 1;
        let generation = self.recompute_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            this.update(cx, |view, cx| {
                if view.recompute_gen == generation {
                    view.recompute(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Recompute hunks from HEAD content vs. current right-editor content and
    /// push decorations into both editors. All heavy work (line diff,
    /// tree-sitter parses and highlight queries) runs on the background
    /// executor so opening a large diff — e.g. a newly added markdown file —
    /// never blocks the UI thread. Successive calls supersede each other via
    /// `recompute_gen`.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        self.recompute_gen += 1;
        let generation = self.recompute_gen;

        let old_text = self.old_text.clone();
        let new_text = self.right.read(cx).value().to_string();
        let language = self.old_highlighter.language().to_string();
        let theme = cx.theme().highlight_theme.clone();
        let added_line = Self::added_line_bg(cx);
        let deleted_line = Self::deleted_line_bg(cx);
        let added_word = Self::added_word_bg(cx);
        let deleted_word = Self::deleted_word_bg(cx);
        let old_parsed = self.old_parsed.take();
        let new_parsed = self.new_parsed.take();
        let expanded = self.expanded_separators.clone();

        // Hand the highlighters to the background task; keep a fresh placeholder
        // in the view so a superseding recompute that runs before this task
        // finishes starts from a clean slate.
        let mut old_hl =
            std::mem::replace(&mut self.old_highlighter, SimpleHighlighter::new(&language));
        let mut new_hl =
            std::mem::replace(&mut self.new_highlighter, SimpleHighlighter::new(&language));

        cx.spawn(async move |this, cx| {
            let task_old = old_text.clone();
            let task_new = new_text.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let hunks = compute_hunks(&task_old, &task_new);
                    let additions = hunks.iter().map(|h| h.additions()).sum();
                    let deletions = hunks.iter().map(|h| h.deletions()).sum();

                    // Skip syntax highlighting for large files to keep the task fast.
                    let should_highlight = task_old.len() + task_new.len() <= 600 * 1024;
                    if should_highlight {
                        if old_parsed.as_deref() != Some(task_old.as_ref()) {
                            let rope = Rope::from_str(&task_old);
                            old_hl.update(None, &rope, None);
                        }
                        if new_parsed.as_deref() != Some(task_new.as_str()) {
                            let rope = Rope::from_str(&task_new);
                            new_hl.update(None, &rope, None);
                        }
                    }

                    let unified = super::diff_model::build_unified_collapsed(
                        &task_old, &task_new, &hunks, &expanded,
                    );
                    let hunk_rows = unified.hunk_rows.clone();
                    let separators = unified.separators.clone();
                    let runs = if should_highlight {
                        DiffView::merged_highlight_runs(
                            &unified.source,
                            &task_old,
                            &task_new,
                            &mut old_hl,
                            &mut new_hl,
                            &theme,
                        )
                    } else {
                        let len = unified.source.text.len();
                        if len > 0 {
                            vec![(0..len, HighlightStyle::default())]
                        } else {
                            Vec::new()
                        }
                    };
                    let (line_colors, word_colors, labels) = unified_decorations(
                        &unified.source,
                        added_line,
                        deleted_line,
                        added_word,
                        deleted_word,
                    );

                    Result::<_, ()>::Ok((
                        hunks,
                        additions,
                        deletions,
                        UnifiedPayload {
                            text: unified.source.text,
                            runs,
                            line_colors,
                            word_colors,
                            labels,
                            hunk_rows,
                            separators,
                        },
                        old_hl,
                        new_hl,
                    ))
                })
                .await;

            this.update(cx, |view, cx| {
                if view.recompute_gen != generation {
                    // Superseded by a newer recompute; force re-parses next time.
                    view.old_parsed = None;
                    view.new_parsed = None;
                    return;
                }
                match result {
                    Ok((hunks, additions, deletions, payload, old_hl, new_hl)) => {
                        view.old_highlighter = old_hl;
                        view.new_highlighter = new_hl;
                        view.old_parsed = Some(old_text.clone());
                        view.new_parsed = Some(new_text.clone());
                        view.hunks = hunks;
                        view.hunks_source = TextFingerprint::of(&new_text);
                        view.additions = additions;
                        view.deletions = deletions;
                        view.unified_hunk_rows = payload.hunk_rows.clone();
                        view.unified_separators = payload.separators.clone();

                        let (nl, nw, ol, ow) = hunk_decorations(
                            &view.hunks,
                            added_line,
                            deleted_line,
                            added_word,
                            deleted_word,
                        );
                        view.right.update(cx, |state, cx| {
                            state.set_diff_decorations(nl, nw, cx);
                        });
                        view.left.update(cx, |state, cx| {
                            state.set_diff_decorations(ol, ow, cx);
                        });

                        if view.effective_mode() == DiffMode::Unified {
                            view.unified.update(cx, |state, cx| {
                                state.set_text(payload.text.clone(), cx);
                                // The merged text is not valid source code, so
                                // syntax highlighting is computed per source
                                // text above and injected here (this also skips
                                // the internal highlighter parse).
                                state.set_external_highlight_runs(Some(payload.runs.clone()), cx);
                                state.set_diff_decorations(
                                    payload.line_colors.clone(),
                                    payload.word_colors.clone(),
                                    cx,
                                );
                                state.set_diff_line_labels(Some(payload.labels.clone()), cx);
                            });
                        }
                    }
                    Err(_) => {
                        // Keep the last good state; force re-parses on retry.
                        view.old_parsed = None;
                        view.new_parsed = None;
                    }
                }
                // 首次计算完成：请求初始跳转到第一个变更处（render 时消费）。
                if !view.initial_jump_done {
                    view.initial_jump_done = true;
                    if !view.hunks.is_empty() {
                        view.pending_jump = true;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 滚动指定编辑器到目标 buffer 行（顶部保留 `context` 行上文）。
    fn scroll_editor_to_row(editor: &Entity<InputState>, row: u32, context: u32, cx: &mut App) {
        let target = row.saturating_sub(context);
        let line_height = editor.read(cx).last_layout.as_ref().map(|l| l.line_height);
        let Some(line_height) = line_height else {
            return;
        };
        if line_height <= px(0.) {
            return;
        }
        editor.update(cx, |state, cx| {
            state.update_scroll_offset(Some(point(px(0.), -(line_height * target as f32))), cx);
        });
    }

    /// 消费待执行的初始跳转（需等编辑器完成首次布局拿到 line_height）。
    fn flush_pending_jump(&mut self, cx: &mut Context<Self>) {
        if !self.pending_jump {
            return;
        }
        // 唯一的重试理由是「编辑器还没完成首次布局、拿不到 line_height」。
        //
        // 不能用「滚动偏移有没有变化」当重试条件：目标行在文件开头几行时偏移
        // 量本来就是 0，`scroll_editor_to_row` 写了也没变化，条件恒为真，于是
        // 每一帧都重复设置滚动并 notify —— 一个永久的重绘死循环。
        match self.effective_mode() {
            DiffMode::Unified => {
                let Some(row) = self.unified_hunk_rows.first().copied() else {
                    self.pending_jump = false;
                    return;
                };
                let editor = self.unified.clone();
                if Self::editor_line_height(&editor, cx) <= px(0.) {
                    return;
                }
                Self::scroll_editor_to_row(&editor, row, 3, cx);
                self.pending_jump = false;
            }
            DiffMode::Split => {
                let Some(hunk) = self.hunks.first().cloned() else {
                    self.pending_jump = false;
                    return;
                };
                let right = self.right.clone();
                if Self::editor_line_height(&right, cx) <= px(0.) {
                    return;
                }
                Self::scroll_editor_to_row(&right, hunk.new_rows.start, 3, cx);
                // 左侧经映射同步（显式设置，不依赖 observe 时序）
                let left_row =
                    map_line_new_to_old(hunk.new_rows.start.saturating_sub(3), &self.hunks);
                let left = self.left.clone();
                Self::scroll_editor_to_row(&left, left_row, 0, cx);
                self.pending_jump = false;
            }
        }
    }

    fn editor_line_height(editor: &Entity<InputState>, cx: &App) -> gpui::Pixels {
        editor
            .read(cx)
            .last_layout
            .as_ref()
            .map(|l| l.line_height)
            .unwrap_or(px(0.))
    }

    /// 当前视口顶部所在的 hunk 序号（导航基准）。
    fn active_hunk_index(&self, cx: &App) -> Option<usize> {
        if self.hunks.is_empty() {
            return None;
        }
        let top_row: u32 = match self.effective_mode() {
            DiffMode::Unified => {
                let state = self.unified.read(cx);
                let lh = state
                    .last_layout
                    .as_ref()
                    .map(|l| l.line_height)
                    .unwrap_or(px(0.));
                if lh <= px(0.) {
                    0
                } else {
                    ((-state.scroll_handle.offset().y) / lh).max(0.) as u32
                }
            }
            DiffMode::Split => {
                let state = self.right.read(cx);
                let lh = state
                    .last_layout
                    .as_ref()
                    .map(|l| l.line_height)
                    .unwrap_or(px(0.));
                if lh <= px(0.) {
                    0
                } else {
                    ((-state.scroll_handle.offset().y) / lh).max(0.) as u32
                }
            }
        };
        // 视口顶部所在或之后的第一个 hunk；若已滚过所有 hunk 则取最后一个。
        let rows: Vec<u32> = match self.effective_mode() {
            DiffMode::Unified => self.unified_hunk_rows.clone(),
            DiffMode::Split => self.hunks.iter().map(|h| h.new_rows.start).collect(),
        };
        let idx = rows
            .iter()
            .position(|&r| r + 1 >= top_row)
            .unwrap_or(rows.len().saturating_sub(1));
        Some(idx)
    }

    /// 跳转到第 `ix` 个 hunk（split 双侧联动 / unified 单侧）。
    fn jump_to_hunk(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.hunks.is_empty() {
            return;
        }
        let ix = ix.min(self.hunks.len() - 1);
        match self.effective_mode() {
            DiffMode::Split => {
                let hunk = &self.hunks[ix];
                let left_row =
                    map_line_new_to_old(hunk.new_rows.start.saturating_sub(3), &self.hunks);
                let (right, left) = (self.right.clone(), self.left.clone());
                Self::scroll_editor_to_row(&right, hunk.new_rows.start, 3, cx);
                Self::scroll_editor_to_row(&left, left_row, 0, cx);
            }
            DiffMode::Unified => {
                let Some(row) = self.unified_hunk_rows.get(ix).copied() else {
                    return;
                };
                let editor = self.unified.clone();
                Self::scroll_editor_to_row(&editor, row, 3, cx);
            }
        }
        cx.notify();
    }

    /// 展开一个折叠区：记录 id 后立即重建（滚动偏移保持不变，
    /// 因为展开按钮只对视口内可见的分隔行渲染）。
    fn expand_separator(&mut self, id: u32, cx: &mut Context<Self>) {
        self.expanded_separators.insert(id);
        self.recompute(cx);
    }

    /// Build gap-free highlight runs for the merged unified text, colored
    /// per source tree: deleted lines from the HEAD parse, context/added
    /// lines from the working-tree parse. Multi-line captures (strings,
    /// comments) are split at merged line boundaries and byte offsets are
    /// remapped into the merged text.
    ///
    /// Consecutive lines backed by the same source side form one batched
    /// query per contiguous source range (rather than one query per line),
    /// keeping the tree-sitter query cost proportional to the number of
    /// changed regions instead of the file's line count. Lines longer than
    /// `MAX_HIGHLIGHT_LINE_LENGTH` are skipped (default colors).
    fn merged_highlight_runs(
        unified: &UnifiedSource,
        old_text: &str,
        new_text: &str,
        old_highlighter: &mut SimpleHighlighter,
        new_highlighter: &mut SimpleHighlighter,
        theme: &HighlightTheme,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let old_starts = line_starts(old_text);
        let new_starts = line_starts(new_text);

        let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        let mut i = 0;
        while i < unified.lines.len() {
            // 关键防御：用 get 替代直接索引，陈旧数据导致越界时跳过而非 panic。
            let Some(current_line) = unified.lines.get(i) else {
                i += 1;
                continue;
            };
            let is_old = matches!(current_line.kind, LineKind::Del);
            let (highlighter, src_text, src_starts) = if is_old {
                (&mut *old_highlighter, old_text, &old_starts[..])
            } else {
                (&mut *new_highlighter, new_text, &new_starts[..])
            };

            let first_row = if is_old {
                current_line.old_lineno
            } else {
                current_line.new_lineno
            };
            let Some(first_row) = first_row else {
                i += 1;
                continue;
            };
            if current_line.merged_range.len() > MAX_HIGHLIGHT_LINE_LENGTH {
                i += 1;
                continue;
            }

            // Collect the run of consecutive lines sharing this source side.
            // Rows within a side are emitted contiguously, so the next line's
            // source range always begins exactly one byte (the newline) after
            // the previous line's range end.
            //
            // The group is capped at `MAX_GROUP_LINES` members: tree-sitter
            // highlight queries are bounded by a wall-clock timeout, and a
            // group that covers a whole large file (all unchanged context
            // lines are contiguous on one source side) would exceed it,
            // silently dropping the styles for every line past the cut-off
            // (e.g. everything below a mid-file hunk). Splitting into
            // bounded segments keeps each query well inside the budget while
            // preserving per-source-tree coloring.
            const MAX_GROUP_LINES: usize = 40;

            let first_src = line_range(src_text, src_starts, first_row);
            let mut members: Vec<(Range<usize>, Range<usize>)> =
                vec![(first_src.clone(), current_line.merged_range.clone())];
            let mut group_end = first_src.end;
            let mut j = i + 1;
            while j < unified.lines.len() && members.len() < MAX_GROUP_LINES {
                // 关键防御：用 get 替代索引，数据竞态时中断分组而非 panic。
                let Some(line) = unified.lines.get(j) else {
                    break;
                };
                let same_kind = matches!(
                    (is_old, line.kind),
                    (true, LineKind::Del) | (false, LineKind::Context | LineKind::Add)
                );
                if !same_kind {
                    break;
                }
                let Some(row) = (if is_old {
                    line.old_lineno
                } else {
                    line.new_lineno
                }) else {
                    break;
                };
                if line.merged_range.len() > MAX_HIGHLIGHT_LINE_LENGTH {
                    break;
                }
                let src = line_range(src_text, src_starts, row);
                if src.start != group_end + 1 {
                    // Non-contiguous source — start a fresh group.
                    break;
                }
                group_end = src.end;
                members.push((src, line.merged_range.clone()));
                j += 1;
            }
            i = j;

            let group_start = members[0].0.start;
            if group_end > group_start {
                let group_styles = highlighter.styles(&(group_start..group_end), theme);
                for (range, style) in group_styles {
                    for (src, merged) in &members {
                        let s = range.start.max(src.start);
                        let e = range.end.min(src.end);
                        if e > s {
                            let offset = merged.start - src.start;
                            runs.push((s + offset..e + offset, style));
                        }
                    }
                }
            }
        }

        fill_gaps(&mut runs, unified.text.len());
        runs
    }

    // ---- scroll sync --------------------------------------------------------

    fn sync_scroll(&mut self, source: Side, cx: &mut Context<Self>) {
        if self.effective_mode() != DiffMode::Split || self.scroll_syncing {
            return;
        }
        let (src, dst) = match source {
            Side::Left => (&self.left, &self.right),
            Side::Right => (&self.right, &self.left),
        };
        let (offset, line_height) = {
            let state = src.read(cx);
            let line_height = state
                .last_layout
                .as_ref()
                .map(|l| l.line_height)
                .unwrap_or(px(0.));
            (state.scroll_handle.offset(), line_height)
        };
        if line_height <= px(0.) {
            return;
        }
        let top_line = ((-offset.y) / line_height).max(0.) as u32;
        let mapped = match source {
            Side::Left => map_line_old_to_new(top_line, &self.hunks),
            Side::Right => map_line_new_to_old(top_line, &self.hunks),
        };
        let desired_y = -(line_height * mapped as f32);
        let current = dst.read(cx).scroll_handle.offset();
        if (current.y - desired_y).abs() > line_height / 2. || current.x != offset.x {
            self.scroll_syncing = true;
            dst.update(cx, |state, cx| {
                state.update_scroll_offset(Some(point(offset.x, desired_y)), cx);
            });
            // `update_scroll_offset` notifies the destination entity, but GPUI
            // observers run at *effect flush* time, i.e. after this method
            // returns. Resetting the flag synchronously here would clear it
            // before the mirrored `sync_scroll` ever saw it, leaving both panes
            // free to correct each other forever. `defer` pushes onto the same
            // FIFO effect queue, so it runs strictly after the pending
            // `Effect::Notify` for the destination.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, _cx| this.scroll_syncing = false)
                    .ok();
            });
        }
    }

    // ---- layout -------------------------------------------------------------

    /// The layout actually shown: explicit preference wins; `Auto` picks by
    /// available width (unified by default, split when wide enough).
    fn effective_mode(&self) -> DiffMode {
        match self.preference {
            LayoutPreference::Unified => DiffMode::Unified,
            LayoutPreference::Split => DiffMode::Split,
            LayoutPreference::Auto => {
                if self.available_width >= SPLIT_MIN_WIDTH {
                    DiffMode::Split
                } else {
                    DiffMode::Unified
                }
            }
        }
    }

    /// Called by the editor panel each render with the current content width.
    pub fn set_available_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if (self.available_width - width).abs() < 1.0 {
            return;
        }
        let before = self.effective_mode();
        self.available_width = width;
        let after = self.effective_mode();
        if after == DiffMode::Unified && before != after {
            self.recompute(cx);
        }
        cx.notify();
    }

    // ---- actions ------------------------------------------------------------

    /// Cycle layout preference: Auto → Unified → Split → Auto.
    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.preference = match self.preference {
            LayoutPreference::Auto => LayoutPreference::Unified,
            LayoutPreference::Unified => LayoutPreference::Split,
            LayoutPreference::Split => LayoutPreference::Auto,
        };
        if self.effective_mode() == DiffMode::Unified {
            self.recompute(cx);
        }
        cx.notify();
    }

    /// 把点击的 hunk 下标解析为**与当前右栏文本一致**的 hunk。
    ///
    /// `self.hunks` 由 200ms 防抖的后台任务产出。用户连续编辑后立刻点
    /// "还原此块"时，hunks 仍基于旧文本，其 `new_rows` 已失效 —— 直接按
    /// 字节切片会删掉错误的行、吞掉刚输入的内容。
    ///
    /// 处理方式：指纹不匹配时同步重算 hunks，再用**HEAD 侧行号**重新定位
    /// 目标块（HEAD 文本不随右栏编辑变化，是稳定的锚点）；定位不到就放弃
    /// 本次还原，总好过改错内容。
    fn resolve_hunk_for_revert(&mut self, hunk_ix: usize, current: &str) -> Option<DiffHunk> {
        let stale = self.hunks.get(hunk_ix)?.clone();
        if self.hunks_source == TextFingerprint::of(current) {
            return Some(stale);
        }

        let fresh = super::diff_model::compute_hunks(&self.old_text, current);

        // HEAD 侧行号是稳定锚点（右栏怎么编辑都不会改变它），所以评分以
        // `old_rows` 的距离为主、`new_rows` 的距离为辅。
        //
        // 这里不能用 `find` 取「文件序第一个相交的块」：若用户在上方编辑使
        // 某个更大的块吞并到目标区间，`find` 会返回那个更大的块，还原时就
        // 把用户刚输入的内容一起覆盖掉了。改成取距离最近者，并用
        // `REVERT_MATCH_TOLERANCE` 封顶——相距太远说明是误匹配，宁可放弃。
        let key = |h: &DiffHunk| -> (u32, u32) {
            let intersects = h.old_rows.start < stale.old_rows.end.max(stale.old_rows.start + 1)
                && stale.old_rows.start < h.old_rows.end.max(h.old_rows.start + 1);
            let old_distance = if intersects {
                0
            } else {
                h.old_rows
                    .start
                    .abs_diff(stale.old_rows.start)
                    .min(h.old_rows.start.abs_diff(stale.old_rows.end))
            };
            (
                old_distance,
                h.new_rows.start.abs_diff(stale.new_rows.start),
            )
        };

        let matched = fresh
            .iter()
            .filter(|h| key(h).0 <= REVERT_MATCH_TOLERANCE)
            .min_by_key(|h| key(h))
            .cloned();

        match matched {
            Some(hunk) => {
                // 用重算结果刷新视图状态，避免后续操作重复重算
                self.hunks = fresh;
                self.hunks_source = TextFingerprint::of(current);
                Some(hunk)
            }
            None => {
                log::warn!(
                    "diff revert: 无法定位已变更的 hunk #{}，放弃还原（old_rows={:?}）",
                    hunk_ix,
                    stale.old_rows
                );
                None
            }
        }
    }

    /// Revert one hunk: replace the new-side lines with the HEAD lines.
    fn revert_hunk(&mut self, hunk_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.right.read(cx).value().to_string();
        let Some(hunk) = self.resolve_hunk_for_revert(hunk_ix, &current) else {
            cx.notify();
            return;
        };
        let starts = line_starts(&current);
        let count = line_count(&current, &starts);

        let byte_start = if hunk.new_rows.start < count {
            starts[hunk.new_rows.start as usize]
        } else {
            current.len()
        };
        let byte_end = if hunk.new_rows.end < count {
            starts[hunk.new_rows.end as usize]
        } else {
            current.len()
        };

        // Exact HEAD bytes for the reverted line range.
        let old_starts = line_starts(&self.old_text);
        let old_count = line_count(&self.old_text, &old_starts);
        let repl_start = if hunk.old_rows.start < old_count {
            old_starts[hunk.old_rows.start as usize]
        } else {
            self.old_text.len()
        };
        let repl_end = if hunk.old_rows.end < old_count {
            old_starts[hunk.old_rows.end as usize]
        } else {
            self.old_text.len()
        };
        let replacement = &self.old_text[repl_start..repl_end];

        let mut updated =
            String::with_capacity(current.len() - (byte_end - byte_start) + replacement.len());
        updated.push_str(&current[..byte_start]);
        updated.push_str(replacement);
        updated.push_str(&current[byte_end..]);

        self.right.update(cx, |state, cx| {
            // Undoable on purpose: `set_value` wipes the undo stack and jumps
            // to the top of the file, which made a mis-clicked revert both
            // unrecoverable and disorienting.
            state.set_value_undoable(updated, window, cx);
        });
        let current = self.right.read(cx).value().to_string();
        self.dirty = current != self.saved_text;
        self.recompute(cx);
    }

    /// Save the working-tree file through the repository backend.
    ///
    /// 走与普通编辑器一致的 `save_text_to_file`：按磁盘原编码写回并保留行尾
    /// 风格。此前直接 `repo.write_file` 写的是解码后的 UTF-8 字符串，等于
    /// 悄悄删掉 BOM、把 GBK / UTF-16 文件整体改写成 UTF-8。
    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving || !self.dirty {
            return;
        }
        self.saving = true;
        self.save_error = None;
        cx.notify();

        let text = self.right.read(cx).value().to_string();
        let repo = self.repo.clone();
        let path = self.path.clone();
        let full_path = self.repo.resolve_path(&self.path);
        let ssh_config = self.repo.ssh_config();
        // 磁盘基线：供 `save_text_to_file` 推断文件原本的行尾风格。
        let baseline = self.saved_text.clone();

        cx.spawn(async move |this, cx| {
            let write_text = text.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let encoding = repo.detect_encoding(&path);
                    crate::editor::editor::save_text_to_file(
                        &full_path,
                        ssh_config.as_ref(),
                        &baseline,
                        &write_text,
                        encoding,
                    )
                })
                .await;
            this.update(cx, |view, cx| {
                view.saving = false;
                match result {
                    Ok(written) => {
                        // `written` 是行尾归一化后的实际落盘内容，用它做新的
                        // 脏检查基线，否则下次保存会误判为未修改。
                        view.saved_text = written;
                        view.dirty =
                            view.right.read(cx).value().as_ref() != view.saved_text.as_str();
                    }
                    Err(e) => {
                        log::error!("diff save failed for {}: {e}", view.path);
                        view.save_error = Some(e.clone());
                        // 保存失败必须让用户看见；此前只写了一行日志，用户
                        // 会误以为已经存盘。
                        if let Some(editor) = view.editor.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.show_notification(
                                    format!("保存失败：{e}"),
                                    crate::components::notification::NotificationKind::Error,
                                    cx,
                                );
                            });
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-fetch HEAD + working-tree content (discards unsaved edits).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        cx.notify();

        let repo = self.repo.clone();
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { repo.diff_file_contents(&path) }
                })
                .await;
            this.update(cx, |view, cx| {
                view.refreshing = false;
                if let Ok((old, new)) = result {
                    view.old_text = old.clone().into();
                    if view.dirty {
                        // 右栏有未保存编辑时绝不覆盖：刷新只是重新拉取 HEAD
                        // 与磁盘内容，静默丢弃用户正在编辑的文本是不可接受的。
                        log::warn!(
                            "diff refresh for {}: 保留未保存编辑，仅刷新 HEAD 侧",
                            view.path
                        );
                    } else {
                        view.saved_text = new.clone();
                        view.right.update(cx, |state, cx| {
                            state.set_text(new, cx);
                        });
                    }
                    // 左栏存的是 HEAD 内容，必须一起更新。此前只换了
                    // `old_text` 而不动左栏，hunks 按新 HEAD 计算、左栏却还是
                    // 旧 HEAD，两栏的内容与染色会整体错位。
                    view.left.update(cx, |state, cx| {
                        state.set_text(old, cx);
                    });
                    view.recompute(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the diffed file in a normal editor tab.
    fn open_file(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        let ssh_config = self.repo.ssh_config();
        let full_path = self.repo.resolve_path(&self.path);
        window.defer(cx, move |window, cx| {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |e, cx| match ssh_config {
                    Some(config) => e.open_remote_file(full_path, config, window, cx),
                    None => e.open_file(std::path::PathBuf::from(full_path), window, cx),
                });
            }
        });
    }

    /// Line height + scroll offset of an editor (for overlay placement).
    fn editor_scroll_metrics(
        editor: &Entity<InputState>,
        cx: &App,
    ) -> (gpui::Pixels, gpui::Point<gpui::Pixels>) {
        let state = editor.read(cx);
        let line_height = state
            .last_layout
            .as_ref()
            .map(|l| l.line_height)
            .unwrap_or(px(0.));
        (line_height, state.scroll_handle.offset())
    }

    fn right_scroll_metrics(&self, cx: &App) -> (gpui::Pixels, gpui::Point<gpui::Pixels>) {
        Self::editor_scroll_metrics(&self.right, cx)
    }

    // ---- rendering ------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mode_label = match self.preference {
            LayoutPreference::Auto => "布局：自动",
            LayoutPreference::Unified => "布局：统一",
            LayoutPreference::Split => "布局：分栏",
        };
        let busy = self.refreshing || self.saving;
        let dirty = self.dirty;

        // hunk 导航状态：当前序号（1-based）与总数
        let hunk_total = self.hunks.len();
        let hunk_current = self.active_hunk_index(cx).map(|ix| ix + 1).unwrap_or(0);
        let nav_enabled = hunk_total > 0;

        let button = |id: &'static str,
                      label: &'static str,
                      enabled: bool,
                      theme: &crate::theme::UiTheme| {
            div()
                .id(id)
                .px(px(8.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.border)
                .when(enabled, |this| this.cursor_pointer())
                .text_color(if enabled {
                    theme.text_muted
                } else {
                    theme.text_subtle
                })
                .when(enabled, |this| {
                    this.hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                })
                .child(label)
        };

        div()
            .flex()
            .w_full()
            .h(px(32.0))
            .flex_none()
            .items_center()
            .px(px(12.0))
            .gap(px(12.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.panel_background)
            .text_size(px(12.0))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_color(theme.text)
                    .truncate()
                    .child(self.path.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(2.0))
                    .text_color(theme.status_added)
                    .text_size(px(11.0))
                    .child(format!("+{}", self.additions)),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(2.0))
                    .text_color(theme.status_deleted)
                    .text_size(px(11.0))
                    .child(format!("-{}", self.deletions)),
            )
            // hunk 导航：上一个 / 计数 / 下一个（循环跳转）
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(
                        button("diff-prev-hunk", "‹ 上一个", nav_enabled, theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                let Some(cur) = this.active_hunk_index(cx) else {
                                    return;
                                };
                                let prev = if cur == 0 {
                                    this.hunks.len() - 1
                                } else {
                                    cur - 1
                                };
                                this.jump_to_hunk(prev, cx);
                            }),
                        ),
                    )
                    .child(
                        div()
                            .w(px(44.0))
                            .flex()
                            .justify_center()
                            .text_size(px(11.0))
                            .text_color(if nav_enabled {
                                theme.text_muted
                            } else {
                                theme.text_subtle
                            })
                            .child(if nav_enabled {
                                format!("{}/{}", hunk_current.min(hunk_total), hunk_total)
                            } else {
                                "0/0".to_string()
                            }),
                    )
                    .child(
                        button("diff-next-hunk", "下一个 ›", nav_enabled, theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                let Some(cur) = this.active_hunk_index(cx) else {
                                    return;
                                };
                                let next = (cur + 1) % this.hunks.len();
                                this.jump_to_hunk(next, cx);
                            }),
                        ),
                    ),
            )
            .child(
                button("diff-open-file", "打开文件", true, theme).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.open_file(window, cx);
                    },
                )),
            )
            .child(
                button(
                    "diff-refresh",
                    if self.refreshing {
                        "刷新中…"
                    } else {
                        "刷新"
                    },
                    !busy,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.refresh(cx);
                })),
            )
            .child(
                button(
                    "diff-save",
                    if self.saving {
                        "保存中…"
                    } else if dirty {
                        "● 保存"
                    } else {
                        "保存"
                    },
                    dirty && !self.saving,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.save(cx);
                })),
            )
            .child(
                button("diff-toggle-mode", mode_label, true, theme).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.toggle_mode(cx);
                    },
                )),
            )
    }

    fn render_split(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let border = theme.border;
        let editor_bg = theme.editor_background();

        // Revert buttons overlay, positioned by hunk line and scroll offset.
        let (line_height, scroll_offset) = self.right_scroll_metrics(cx);
        let revert_buttons: Vec<gpui::AnyElement> = if line_height > px(0.) {
            let pill_height = (line_height - px(6.0)).max(px(16.0));
            self.hunks
                .iter()
                .enumerate()
                .map(|(ix, hunk)| {
                    let y = line_height * hunk.new_rows.start as f32 + scroll_offset.y;
                    // 外层占满一行高度做垂直居中，内层药丸固定高度
                    div()
                        .absolute()
                        .top(y)
                        .right(px(12.0))
                        .h(line_height)
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .id(("revert-hunk", ix))
                                .h(pill_height)
                                .px(px(8.0))
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .rounded(px(999.0))
                                .bg(theme.elevated_background)
                                .border_1()
                                .border_color(theme.border)
                                .cursor_pointer()
                                .text_color(theme.text_muted)
                                .text_size(px(11.0))
                                .hover(|s| {
                                    s.text_color(theme.text)
                                        .border_color(theme.accent)
                                        .bg(theme.tab_hover_background)
                                })
                                .child("还原此块")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.revert_hunk(ix, window, cx);
                                })),
                        )
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };

        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .bg(editor_bg)
                    .child(EditorInput::new(&self.left).disabled(true)),
            )
            .child(div().w(px(1.0)).flex_none().h_full().bg(border))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .bg(editor_bg)
                    .child(EditorInput::new(&self.right))
                    .children(revert_buttons),
            )
    }

    fn render_unified(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        // 折叠区浮层：整行「折叠带」+ 居中展开药丸按钮（GitHub 风格）。
        let (line_height, scroll_offset) = Self::editor_scroll_metrics(&self.unified, cx);
        let expand_bands: Vec<gpui::AnyElement> = if line_height > px(0.) {
            let pill_height = (line_height - px(6.0)).max(px(16.0));
            self.unified_separators
                .iter()
                .map(|sep| {
                    let y = line_height * sep.merged_row as f32 + scroll_offset.y;
                    div()
                        .absolute()
                        .top(y)
                        .left(px(0.0))
                        .right(px(0.0))
                        .h(line_height)
                        .flex()
                        .items_center()
                        .justify_center()
                        // 折叠带底色：比编辑器底色深一档，上下缘极淡分隔线
                        .bg(theme.panel_background)
                        .border_t_1()
                        .border_b_1()
                        .border_color(theme.border.opacity(0.4))
                        .child(
                            div()
                                .id(("expand-sep", sep.id))
                                .h(pill_height)
                                .px(px(10.0))
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .rounded(px(999.0))
                                .bg(theme.elevated_background)
                                .border_1()
                                .border_color(theme.border)
                                .cursor_pointer()
                                .text_color(theme.text_muted)
                                .text_size(px(11.0))
                                .hover(|s| {
                                    s.text_color(theme.text)
                                        .border_color(theme.accent)
                                        .bg(theme.tab_hover_background)
                                })
                                .child(format!("展开 {} 行", sep.hidden_lines))
                                .child(
                                    gpui::svg()
                                        .size(px(8.0))
                                        .path(crate::assets::Icon::ChevronDown.path())
                                        .text_color(theme.icon_muted),
                                )
                                .on_click({
                                    let id = sep.id;
                                    cx.listener(move |this, _, _, cx| {
                                        this.expand_separator(id, cx);
                                    })
                                }),
                        )
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };

        div()
            .flex_1()
            .min_h_0()
            .relative()
            .overflow_hidden()
            .bg(theme.editor_background())
            .child(EditorInput::new(&self.unified).disabled(true))
            .children(expand_bands)
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_bg = cx.theme().editor_background();
        let text_muted = cx.theme().text_muted;
        let status_deleted = cx.theme().status_deleted;
        let accent = cx.theme().accent;
        let accent_fg = cx.theme().accent_foreground;

        self.flush_pending_jump(cx);

        let content: gpui::AnyElement = if self.loading {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(text_muted)
                .child("加载中…")
                .into_any_element()
        } else if let Some(error) = self.load_error.as_ref() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(12.0))
                .text_sm()
                .text_color(status_deleted)
                .child(format!("加载失败: {error}"))
                .child(
                    div()
                        .id("diff-retry")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .bg(accent)
                        .text_color(accent_fg)
                        .cursor_pointer()
                        .child("重试")
                        .on_click(cx.listener(|this, _, _, cx| this.fetch_contents(cx))),
                )
                .into_any_element()
        } else {
            match self.effective_mode() {
                DiffMode::Split => self.render_split(cx).into_any_element(),
                DiffMode::Unified => self.render_unified(cx).into_any_element(),
            }
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(editor_bg)
            .child(self.render_toolbar(cx))
            .child(content)
            .into_any_element()
    }
}

/// Fill the gaps of a sorted, non-overlapping run list with the default
/// style so the result is contiguous and covers `[0, total_len)` — the
/// renderer requires full coverage (same guarantee `unique_styles` gives
/// for the built-in highlighter).
fn fill_gaps(runs: &mut Vec<(Range<usize>, HighlightStyle)>, total_len: usize) {
    let input = std::mem::take(runs);
    let mut filled = Vec::with_capacity(input.len() * 2 + 1);
    let mut cursor = 0usize;
    for (range, style) in input {
        let start = range.start.max(cursor);
        if start > cursor {
            filled.push((cursor..start, HighlightStyle::default()));
        }
        let end = range.end.max(start);
        if end > start {
            filled.push((start..end, style));
        }
        cursor = end;
    }
    if cursor < total_len {
        filled.push((cursor..total_len, HighlightStyle::default()));
    }
    *runs = filled;
}

/// Compute per-line and per-word background decorations plus gutter labels
/// for the merged unified text.
fn unified_decorations(
    unified: &UnifiedSource,
    added_line: Hsla,
    deleted_line: Hsla,
    added_word: Hsla,
    deleted_word: Hsla,
) -> (LineColors, WordColors, Vec<Option<u32>>) {
    let mut line_colors = Vec::new();
    let mut word_colors = Vec::new();
    let mut labels = Vec::with_capacity(unified.lines.len());
    for (row, line) in unified.lines.iter().enumerate() {
        match line.kind {
            LineKind::Context => {}
            LineKind::Add => line_colors.push((row as u32, added_line)),
            LineKind::Del => line_colors.push((row as u32, deleted_line)),
        }
        let word_bg = match line.kind {
            LineKind::Context => None,
            LineKind::Add => Some(added_word),
            LineKind::Del => Some(deleted_word),
        };
        if let Some(bg) = word_bg {
            for r in &line.word_ranges {
                word_colors.push((r.clone(), bg));
            }
        }
        labels.push(line.new_lineno.or(line.old_lineno));
    }
    (line_colors, word_colors, labels)
}

/// Compute per-line and per-word background decorations for the split view:
/// `(new_line, new_word, old_line, old_word)` in the two editors' coordinate
/// spaces.
fn hunk_decorations(
    hunks: &[DiffHunk],
    added_line: Hsla,
    deleted_line: Hsla,
    added_word: Hsla,
    deleted_word: Hsla,
) -> (LineColors, WordColors, LineColors, WordColors) {
    let mut new_line_colors = Vec::new();
    let mut new_word_colors = Vec::new();
    let mut old_line_colors = Vec::new();
    let mut old_word_colors = Vec::new();
    for hunk in hunks {
        for row in hunk.new_rows.clone() {
            new_line_colors.push((row, added_line));
        }
        for row in hunk.old_rows.clone() {
            old_line_colors.push((row, deleted_line));
        }
        for r in &hunk.new_word_ranges {
            new_word_colors.push((r.clone(), added_word));
        }
        for r in &hunk.old_word_ranges {
            old_word_colors.push((r.clone(), deleted_word));
        }
    }
    (
        new_line_colors,
        new_word_colors,
        old_line_colors,
        old_word_colors,
    )
}

/// Map a line number on the old side to the corresponding line on the new
/// side, using cumulative hunk deltas (context regions map exactly).
///
/// Inside a hunk there is no line-for-line correspondence, so the offset
/// within the hunk is reused directly and then **clamped** to the hunk's last
/// new-side row. The clamp is what makes the two directions converge: feeding
/// the clamped row back through [`map_line_new_to_old`] lands inside the same
/// hunk, and the round trip settles on a fixed point within two steps. Without
/// it a shrinking hunk would produce e.g. `15 -> 15 -> 23 -> 15 -> 23 ...`,
/// which `sync_scroll` sees as a permanent mismatch and corrects every frame
/// (visible ping-pong between the two panes).
fn map_line_old_to_new(old_line: u32, hunks: &[DiffHunk]) -> u32 {
    let mut delta = 0i64;
    for h in hunks {
        if old_line < h.old_rows.start {
            break;
        }
        if old_line >= h.old_rows.end {
            delta += (h.new_rows.end - h.new_rows.start) as i64
                - (h.old_rows.end - h.old_rows.start) as i64;
        } else {
            let rel = old_line - h.old_rows.start;
            // A pure deletion has an empty new range; `saturating_sub` plus the
            // `max(start)` keeps the result inside `new_rows.start..=start`.
            let last_new_row = h.new_rows.end.saturating_sub(1).max(h.new_rows.start);
            return h.new_rows.start.saturating_add(rel).min(last_new_row);
        }
    }
    (old_line as i64 + delta).max(0) as u32
}

/// Map a line number on the new side to the corresponding line on the old
/// side, using cumulative hunk deltas.
///
/// See [`map_line_old_to_new`] for why the in-hunk result is clamped.
fn map_line_new_to_old(new_line: u32, hunks: &[DiffHunk]) -> u32 {
    let mut delta = 0i64;
    for h in hunks {
        if new_line < h.new_rows.start {
            break;
        }
        if new_line >= h.new_rows.end {
            delta += (h.old_rows.end - h.old_rows.start) as i64
                - (h.new_rows.end - h.new_rows.start) as i64;
        } else {
            let rel = new_line - h.new_rows.start;
            let last_old_row = h.old_rows.end.saturating_sub(1).max(h.old_rows.start);
            return h.old_rows.start.saturating_add(rel).min(last_old_row);
        }
    }
    (new_line as i64 + delta).max(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::diff_model::build_unified;
    use crate::editor::syntax::{HighlightTheme, HighlightThemeStyle, SyntaxColors, ThemeStyle};
    use crate::theme::ThemeAppearance;

    fn test_theme() -> HighlightTheme {
        HighlightTheme {
            name: "test".into(),
            appearance: ThemeAppearance::Dark,
            style: HighlightThemeStyle {
                syntax: SyntaxColors {
                    keyword: Some(ThemeStyle::color(gpui::red())),
                    type_: Some(ThemeStyle::color(gpui::blue())),
                    function: Some(ThemeStyle::color(gpui::green())),
                    number: Some(ThemeStyle::color(gpui::yellow())),
                    string: Some(ThemeStyle::color(gpui::blue())),
                    comment: Some(ThemeStyle::color(gpui::red())),
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    fn build_runs(old: &str, new: &str) -> (UnifiedSource, Vec<(Range<usize>, HighlightStyle)>) {
        let unified = build_unified(old, new);
        let mut old_hl = SimpleHighlighter::new("rust");
        let mut new_hl = SimpleHighlighter::new("rust");
        old_hl.update(None, &Rope::from_str(old), None);
        new_hl.update(None, &Rope::from_str(new), None);
        let theme = test_theme();
        let runs =
            DiffView::merged_highlight_runs(&unified, old, new, &mut old_hl, &mut new_hl, &theme);
        (unified, runs)
    }

    fn assert_contiguous(runs: &[(Range<usize>, HighlightStyle)], total_len: usize) {
        let mut cursor = 0;
        for (range, _) in runs {
            assert_eq!(range.start, cursor, "runs must be contiguous");
            assert!(range.end > range.start, "no empty runs");
            cursor = range.end;
        }
        assert_eq!(cursor, total_len, "runs must cover the full text");
    }

    /// Find the byte offset of `needle` inside a merged line.
    fn line_offset(unified: &UnifiedSource, line: usize, needle: &str) -> usize {
        let text = &unified.text[unified.lines[line].merged_range.clone()];
        unified.lines[line].merged_range.start + text.find(needle).expect("needle in line")
    }

    /// The merged text of a multi-line-string edit contains an unclosed
    /// quote (the added line re-opens a string), which would swallow the
    /// rest of the file under a single merged-text parse. Each source side
    /// must still be highlighted from its own valid tree.
    #[test]
    fn test_unclosed_string_in_added_line() {
        let old = "fn main() {\n    let s = \"abc\n    def\";\n    let y = s;\n}\n";
        let new = "fn main() {\n    let s = \"abc\n    xyz\";\n    let y = s;\n}\n";
        let (unified, runs) = build_runs(old, new);
        assert_contiguous(&runs, unified.text.len());

        let del = unified
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Del)
            .unwrap();
        let add = unified
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Add)
            .unwrap();

        // Deleted `def";` is still inside the HEAD string literal.
        let off = line_offset(&unified, del, "def\"");
        let run = runs
            .iter()
            .find(|(r, _)| r.start <= off && r.end > off)
            .unwrap();
        assert_eq!(run.1.color, Some(gpui::blue()), "del line inside string");

        // Code after the hunk must NOT be swallowed by the merged-text
        // string: the `let` keyword after the add block is keyword-colored.
        let let_off = line_offset(&unified, add + 1, "let");
        let run = runs
            .iter()
            .find(|(r, _)| r.start <= let_off && r.end > let_off)
            .unwrap();
        assert_eq!(run.1.color, Some(gpui::red()), "keyword after hunk");
    }

    /// Deleted and added lines each keep the colors of their own source
    /// text, even when the merged text stays parseable.
    #[test]
    fn test_del_and_add_from_own_trees() {
        let old = "fn main() {\n    if x {\n        foo();\n    }\n}\n";
        let new = "fn main() {\n    if x {\n        bar();\n    }\n}\n";
        let (unified, runs) = build_runs(old, new);
        assert_contiguous(&runs, unified.text.len());

        let del = unified
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Del)
            .unwrap();
        let add = unified
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Add)
            .unwrap();

        let off = line_offset(&unified, del, "foo");
        let run = runs
            .iter()
            .find(|(r, _)| r.start <= off && r.end > off)
            .unwrap();
        assert_eq!(
            run.1.color,
            Some(gpui::green()),
            "deleted call from HEAD tree"
        );

        let off = line_offset(&unified, add, "bar");
        let run = runs
            .iter()
            .find(|(r, _)| r.start <= off && r.end > off)
            .unwrap();
        assert_eq!(
            run.1.color,
            Some(gpui::green()),
            "added call from working-tree"
        );
    }

    #[test]
    fn test_fill_gaps() {
        let mut runs = vec![
            (
                2..4,
                HighlightStyle {
                    color: Some(gpui::red()),
                    ..Default::default()
                },
            ),
            (
                5..6,
                HighlightStyle {
                    color: Some(gpui::blue()),
                    ..Default::default()
                },
            ),
        ];
        fill_gaps(&mut runs, 8);
        assert_eq!(runs.len(), 5);
        assert_eq!(runs[0].0, 0..2);
        assert!(runs[0].1.color.is_none());
        assert_eq!(runs[1].0, 2..4);
        assert_eq!(runs[1].1.color, Some(gpui::red()));
        assert_eq!(runs[2].0, 4..5);
        assert!(runs[2].1.color.is_none());
        assert_eq!(runs[3].0, 5..6);
        assert_eq!(runs[4].0, 6..8);
        assert!(runs[4].1.color.is_none());
    }

    /// Simulate the renderer's consumption of the external runs for a large
    /// file with a hunk in the middle: the visible-window clip in
    /// `highlight_lines` plus the `layout_lines` run-length walk must cover
    /// the visible span exactly, and a line far below the hunk must keep
    /// keyword colors from its own source tree (not be swallowed by an
    /// unclosed string in the hunk).
    #[test]
    fn test_below_hunk_window_keeps_highlighting() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..200 {
            old.push_str(&format!(
                "fn fn_{i}() -> u32 {{\n    let v = {i};\n    v + 1\n}}\n"
            ));
        }
        new.push_str(&old[..old.len()]);
        // Mid-file hunk: replace one block, leaving an unclosed string in the
        // added line so a merged-text parse would swallow everything below.
        let marker = "fn fn_100";
        let start = new.find(marker).unwrap();
        let end = new[start..].find("\n}\n").map(|p| start + p).unwrap() + 3;
        new.replace_range(
            start..end,
            "fn fn_100() -> u32 {\n    let s = \"unclosed\n    s.len() as u32\n}\n",
        );

        let (unified, runs) = build_runs(&old, &new);
        assert_contiguous(&runs, unified.text.len());

        // Pick a window below the hunk (hunk occupies merged lines 400..404),
        // clip like highlight_lines does (first..last visible buffer line
        // incl. trailing newline).
        let merged_starts = line_starts(&unified.text);
        let first = 410;
        let last = 450;
        let byte_start = merged_starts[first];
        let byte_end = merged_starts[last + 1];
        let mut styles: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        let mut cursor = byte_start;
        for (range, style) in &runs {
            if range.end <= byte_start || range.start >= byte_end {
                continue;
            }
            let s = range.start.max(byte_start);
            let e = range.end.min(byte_end);
            if s > cursor {
                styles.push((cursor..s, HighlightStyle::default()));
            }
            styles.push((s..e, *style));
            cursor = e.max(cursor);
        }
        if cursor < byte_end {
            styles.push((cursor..byte_end, HighlightStyle::default()));
        }

        // Run lengths must sum exactly to the visible span (layout consumes
        // line bytes + trailing '\n' per visible line).
        let consumed: usize = styles.iter().map(|(r, _)| r.len()).sum();
        assert_eq!(consumed, byte_end - byte_start);

        // A `fn` keyword far below the hunk must be keyword-colored, proving
        // the unclosed string did not leak into the below-hunk region.
        let line_ix = (0..unified.lines.len())
            .find(|&ix| {
                unified.text[unified.lines[ix].merged_range.clone()].starts_with("fn fn_103")
            })
            .expect("fn_103 line in merged text");
        let line = &unified.text[merged_starts[line_ix]..merged_starts[line_ix + 1]];
        let kw = line.find("fn ").unwrap();
        let off = merged_starts[line_ix] + kw;
        let run = styles
            .iter()
            .find(|(r, _)| r.start <= off && r.end > off)
            .unwrap();
        assert_eq!(run.1.color, Some(gpui::red()), "keyword below hunk");
    }

    /// Full render-consumption chain: `highlight_lines` clips the gap-free
    /// external runs to the visible window, prepaint converts them to a
    /// length-only `TextRun` list (offsets dropped), and `layout_lines`
    /// walks `run_offset` over visible buffer lines (line bytes + trailing
    /// '\n'). Every wrapped sub-line must pull back exactly its own byte
    /// length of runs, for every possible visible window of a large file.
    #[test]
    fn test_layout_consumption_matches_runs_for_all_windows() {
        use crate::components::input::rope_ext::RopeExt;

        let mut old = String::new();
        let mut new = String::new();
        for i in 0..120 {
            old.push_str(&format!(
                "fn f{i}() -> u32 {{\n    let v = {i};\n    v + 1\n}}\n"
            ));
        }
        new.push_str(&old[..old.len()]);
        // Mid-file hunk with an unclosed string in an added line.
        let marker = "fn f60";
        let start = new.find(marker).unwrap();
        let end = new[start..].find("\n}").map(|p| start + p).unwrap() + 2;
        new.replace_range(
            start..end,
            "fn f60() -> u32 {\n    let s = \"unclosed\n    x + 1\n}\n",
        );

        let (unified, runs) = build_runs(&old, &new);
        assert_contiguous(&runs, unified.text.len());
        let rope = Rope::from_str(&unified.text);

        for first in 0..rope.lines_len() {
            for last in first..rope.lines_len().min(first + 25) {
                // ---- highlight_lines external branch (is_multi_line) ----
                let byte_start = rope.line_start_offset(first);
                let byte_end = rope.line_start_offset(last + 1);
                let mut styles: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
                let mut cursor = byte_start;
                for (range, style) in &runs {
                    if range.end <= byte_start || range.start >= byte_end {
                        continue;
                    }
                    let s = range.start.max(byte_start);
                    let e = range.end.min(byte_end);
                    if s > cursor {
                        styles.push((cursor..s, HighlightStyle::default()));
                    }
                    styles.push((s..e, *style));
                    cursor = e.max(cursor);
                }
                if cursor < byte_end {
                    styles.push((cursor..byte_end, HighlightStyle::default()));
                }

                // ---- prepaint: offsets dropped, lengths only ----
                let run_lens: Vec<usize> = styles.iter().map(|(r, _)| r.len()).collect();

                // ---- layout_lines: run_offset walk over visible lines ----
                let mut run_offset = 0;
                for buffer_line in first..=last {
                    let line_text: String = rope.slice_line(buffer_line).into();
                    let wrapped: Vec<Range<usize>> = if line_text.is_empty() {
                        vec![0..0]
                    } else {
                        vec![0..line_text.len()]
                    };
                    for range in &wrapped {
                        // Inline copy of runs_for_range
                        let g_range = (run_offset + range.start)..(run_offset + range.end);
                        let mut cur = 0usize;
                        let mut pulled = 0usize;
                        for &len in &run_lens {
                            let run_start = cur;
                            let run_end = cur + len;
                            if run_end <= g_range.start {
                                cur = run_end;
                                continue;
                            }
                            if run_start >= g_range.end {
                                break;
                            }
                            let s = g_range.start.max(run_start) - run_start;
                            let e = g_range.end.min(run_end) - run_start;
                            pulled += e.saturating_sub(s);
                            cur = run_end;
                        }
                        assert_eq!(
                            pulled,
                            range.len(),
                            "window first={first} last={last} buffer_line={buffer_line} range={range:?}"
                        );
                    }
                    run_offset += line_text.len() + 1;
                }
            }
        }
    }

    /// Real-world corner: CRLF line endings + multi-byte (Chinese) text. The
    /// render consumption chain must stay in sync with the byte offsets the
    /// background task produces, just as for plain LF/ASCII files.
    #[test]
    fn test_layout_consumption_crlf_and_cjk() {
        use crate::components::input::rope_ext::RopeExt;

        let mut old = String::new();
        let mut new = String::new();
        for i in 0..60 {
            old.push_str(&format!(
                "fn f{i}() -> u32 {{\r\n    let s = \"中文{i}；测试\\n\";\r\n    s.len()\r\n}}\r\n"
            ));
        }
        new.push_str(&old[..old.len()]);
        let marker = "fn f30";
        let start = new.find(marker).unwrap();
        let end = new[start..].find("\r\n}").map(|p| start + p).unwrap() + 3;
        new.replace_range(
            start..end,
            "fn f30() -> u32 {\r\n    let s = \"未闭合\r\n    x + 1\r\n}\r\n",
        );

        let (unified, runs) = build_runs(&old, &new);
        assert_contiguous(&runs, unified.text.len());
        let rope = Rope::from_str(&unified.text);

        for first in 0..rope.lines_len() {
            for last in first..rope.lines_len().min(first + 25) {
                let byte_start = rope.line_start_offset(first);
                let byte_end = rope.line_start_offset(last + 1);
                let mut styles: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
                let mut cursor = byte_start;
                for (range, style) in &runs {
                    if range.end <= byte_start || range.start >= byte_end {
                        continue;
                    }
                    let s = range.start.max(byte_start);
                    let e = range.end.min(byte_end);
                    if s > cursor {
                        styles.push((cursor..s, HighlightStyle::default()));
                    }
                    styles.push((s..e, *style));
                    cursor = e.max(cursor);
                }
                if cursor < byte_end {
                    styles.push((cursor..byte_end, HighlightStyle::default()));
                }
                let run_lens: Vec<usize> = styles.iter().map(|(r, _)| r.len()).collect();

                let mut run_offset = 0;
                for buffer_line in first..=last {
                    let line_text: String = rope.slice_line(buffer_line).into();
                    let wrapped: Vec<Range<usize>> = if line_text.is_empty() {
                        vec![0..0]
                    } else {
                        vec![0..line_text.len()]
                    };
                    for range in &wrapped {
                        let g_range = (run_offset + range.start)..(run_offset + range.end);
                        let mut cur = 0usize;
                        let mut pulled = 0usize;
                        for &len in &run_lens {
                            let run_start = cur;
                            let run_end = cur + len;
                            if run_end <= g_range.start {
                                cur = run_end;
                                continue;
                            }
                            if run_start >= g_range.end {
                                break;
                            }
                            let s = g_range.start.max(run_start) - run_start;
                            let e = g_range.end.min(run_end) - run_start;
                            pulled += e.saturating_sub(s);
                            cur = run_end;
                        }
                        assert_eq!(
                            pulled,
                            range.len(),
                            "window first={first} last={last} buffer_line={buffer_line} range={range:?}"
                        );
                    }
                    run_offset += line_text.len() + 1;
                }
            }
        }
    }

    /// Renderer-visible symptom check: every external run segment must start
    /// and end on a character boundary of the merged text (otherwise the
    /// shaper renders a half-colored word, e.g. the first half of a keyword
    /// highlighted and the rest white), and the byte at the start of every
    /// `fn` keyword must carry the keyword color when a window covering it is
    /// clipped like `highlight_lines` does.
    #[test]
    fn test_merged_runs_char_boundaries_and_keyword_colors() {
        // Mixed CJK + ASCII + keywords, with a hunk so both source trees are
        // exercised and merged lines remap across source sides.
        let old = "fn main() -> u32 {\n    // 中文注释测试\n    let s = \"你好世界，hello world\";\n    println!(\"{}\", s);\n    s.len()\n}\n";
        let new = "fn main() -> u32 {\n    // 中文注释测试\n    let s = \"你好世界，hello world\";\n    println!(\"{}\", s);\n    s.len() + 1\n}\n";
        let (unified, runs) = build_runs(&old, &new);
        assert_contiguous(&runs, unified.text.len());

        // 1. Every run boundary is a char boundary of the merged text.
        for (r, _) in &runs {
            assert!(
                unified.text.is_char_boundary(r.start),
                "run start {} not a char boundary (text={:?})",
                r.start,
                &unified.text[r.start..(r.start + 4).min(unified.text.len())]
            );
            assert!(
                unified.text.is_char_boundary(r.end),
                "run end {} not a char boundary",
                r.end
            );
        }

        // 2. Every `fn` keyword byte carries the keyword color.
        for (ix, line) in unified.lines.iter().enumerate() {
            let text = &unified.text[line.merged_range.clone()];
            let mut search_from = 0;
            while let Some(rel) = text[search_from..].find("fn ") {
                let off = line.merged_range.start + search_from + rel;
                // Skip `fn ` inside a string literal (e.g. "xxx fn yyy")
                if off > 0 && unified.text.as_bytes()[off - 1].is_ascii_alphanumeric() {
                    search_from += rel + 1;
                    continue;
                }
                let run = runs
                    .iter()
                    .find(|(r, _)| r.start <= off && r.end > off)
                    .unwrap_or_else(|| panic!("no run covers fn at {off} (line {ix})"));
                assert_eq!(
                    run.1.color,
                    Some(gpui::red()),
                    "fn at {off} not keyword-colored (line {ix}, kind={:?})",
                    line.kind
                );
                search_from += rel + 1;
            }
        }
    }

    /// Full render reconstruction: for scrolled windows (first visible line
    /// > 0), clip the external runs like `highlight_lines` does, drop offsets
    /// like prepaint does, then walk `layout_lines`' `run_offset` and rebuild
    /// the per-byte colors each visible line would be painted with. These
    /// must match the colors the original runs assign to that line's bytes —
    /// otherwise a keyword is painted half-colored (front half highlighted,
    /// tail white), which is the visible regression reported.
    #[test]
    fn test_render_reconstruction_matches_original_colors_scrolled() {
        use crate::components::input::rope_ext::RopeExt;

        let mut old = String::new();
        let mut new = String::new();
        for i in 0..80 {
            old.push_str(&format!(
                "fn fn_{i}() -> u32 {{\n    let s = \"测试 {i} abc\";\n    s.len()\n}}\n"
            ));
        }
        new.push_str(&old[..old.len()]);
        let marker = "fn fn_40";
        let start = new.find(marker).unwrap();
        let end = new[start..].find("\n}").map(|p| start + p).unwrap() + 2;
        new.replace_range(start..end, "fn fn_40() -> u32 {\n    let t = 42;\n}\n");

        let (unified, runs) = build_runs(&old, &new);
        assert_contiguous(&runs, unified.text.len());
        let rope = Rope::from_str(&unified.text);

        for first in [0usize, 20, 60, 100] {
            let last = (first + 25).min(rope.lines_len().saturating_sub(1));
            if first >= rope.lines_len() {
                continue;
            }

            // ---- highlight_lines external branch (is_multi_line) ----
            let byte_start = rope.line_start_offset(first);
            let byte_end = rope.line_start_offset(last + 1);
            let mut styles: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
            let mut cursor = byte_start;
            for (range, style) in &runs {
                if range.end <= byte_start || range.start >= byte_end {
                    continue;
                }
                let s = range.start.max(byte_start);
                let e = range.end.min(byte_end);
                if s > cursor {
                    styles.push((cursor..s, HighlightStyle::default()));
                }
                styles.push((s..e, *style));
                cursor = e.max(cursor);
            }
            if cursor < byte_end {
                styles.push((cursor..byte_end, HighlightStyle::default()));
            }

            // ---- prepaint: offset-dropped TextRun list (len + color) ----
            let text_runs: Vec<(usize, Option<gpui::Hsla>)> =
                styles.iter().map(|(r, s)| (r.len(), s.color)).collect();

            // ---- layout_lines walk ----
            let mut run_offset = 0;
            for buffer_line in first..=last {
                let line_text: String = rope.slice_line(buffer_line).into();
                let wrapped: Vec<Range<usize>> = if line_text.is_empty() {
                    vec![0..0]
                } else {
                    vec![0..line_text.len()]
                };
                for range in &wrapped {
                    // runs_for_range equivalent, returning (len, color)
                    let g_range = (run_offset + range.start)..(run_offset + range.end);
                    let mut line_runs: Vec<(usize, Option<gpui::Hsla>)> = Vec::new();
                    let mut cur = 0usize;
                    for &(len, color) in &text_runs {
                        let run_start = cur;
                        let run_end = cur + len;
                        if run_end <= g_range.start {
                            cur = run_end;
                            continue;
                        }
                        if run_start >= g_range.end {
                            break;
                        }
                        let s = g_range.start.max(run_start) - run_start;
                        let e = g_range.end.min(run_end) - run_start;
                        if e > s {
                            line_runs.push((e - s, color));
                        }
                        cur = run_end;
                    }

                    // Rebuild per-byte colors for this line.
                    let mut line_colors = vec![None; range.len()];
                    let mut pos = 0usize;
                    for &(len, color) in &line_runs {
                        for b in pos..(pos + len).min(line_colors.len()) {
                            line_colors[b] = color;
                        }
                        pos += len;
                    }

                    // Expected colors straight from the original runs.
                    let line_global = (byte_start + run_offset + range.start)
                        ..(byte_start + run_offset + range.end);
                    let mut expected = vec![None; range.len()];
                    for (r, s) in &runs {
                        let s0 = r.start.max(line_global.start);
                        let e0 = r.end.min(line_global.end);
                        if e0 > s0 {
                            for b in (s0 - line_global.start)..(e0 - line_global.start) {
                                expected[b] = s.color;
                            }
                        }
                    }

                    assert_eq!(
                        line_colors, expected,
                        "window first={first} last={last} buffer_line={buffer_line} range={range:?}"
                    );
                }
                run_offset += line_text.len() + 1;
            }
        }
    }

    /// Extreme Unicode: emoji (multi-code-unit), combining marks, ZWJ
    /// sequences, CJK. Run segments must still start/end on char boundaries
    /// and every `let`/`fn` keyword byte must keep its color — otherwise the
    /// shaper paints a word half-highlighted / half-white.
    #[test]
    fn test_merged_runs_extreme_unicode_boundaries() {
        let old = "fn f() -> u32 {\n    let emoji = \"🎉🚀 test\";\n    let combo = \"e\u{301} x\";\n    let zwj = \"👨‍👩‍👧\";\n    let cjk = \"中文测试\";\n    f()\n}\n";
        let mut new = String::from(old);
        new.push_str("// added\n");
        let (unified, runs) = build_runs(&old, &new);
        assert_contiguous(&runs, unified.text.len());

        for (r, _) in &runs {
            assert!(
                unified.text.is_char_boundary(r.start),
                "run start {} not char boundary",
                r.start
            );
            assert!(
                unified.text.is_char_boundary(r.end),
                "run end {} not char boundary",
                r.end
            );
        }

        // Every `fn` / `let` keyword byte is keyword-colored.
        let mut off = 0;
        for needle in ["fn ", "let "] {
            while let Some(rel) = unified.text[off..].find(needle) {
                let pos = off + rel;
                if pos > 0 && unified.text.as_bytes()[pos - 1].is_ascii_alphanumeric() {
                    off = pos + 1;
                    continue;
                }
                let run = runs
                    .iter()
                    .find(|(r, _)| r.start <= pos && r.end > pos)
                    .unwrap_or_else(|| panic!("no run at {pos}"));
                assert_eq!(run.1.color, Some(gpui::red()), "keyword at {pos} not red");
                off = pos + 1;
            }
        }
    }

    /// Fold scenario: `highlight_lines` flushes one gap-free range per
    /// contiguous visible run; folded lines belong to no range, so styles are
    /// sparse. The fix (1) fills the visible span with default runs and
    /// (2) makes `layout_lines` walk *global* byte offsets (folded lines
    /// count), keeping runs and layout in the same coordinate space. Without
    /// either, every run past a fold shifts and words are painted
    /// half-highlighted / half-white. This test simulates the full chain and
    /// asserts per-byte colors land where the original runs say.
    #[test]
    fn test_layout_consumption_sparse_styles_fold() {
        use crate::components::input::rope_ext::RopeExt;

        // 3 lines, middle one folded away. Byte layout:
        //   line0: "abcd\n" (0..5)  line1(folded): "EFGH\n" (5..10)  line2: "ijkl\n" (10..15)
        let text = "abcd\nEFGH\nijkl\n";
        let rope = Rope::from_str(text);
        let visible = vec![0usize, 2]; // line 1 folded

        // Internal branch output before the fill: two disjoint ranges with
        // distinct colors so misalignment is visible.
        let mut styles: Vec<(Range<usize>, HighlightStyle)> = vec![
            (
                0..4,
                HighlightStyle {
                    color: Some(gpui::red()),
                    ..Default::default()
                },
            ),
            (
                10..14,
                HighlightStyle {
                    color: Some(gpui::blue()),
                    ..Default::default()
                },
            ),
        ];

        // ---- fix (1): fill the visible span [line0_start, line2_end+\n) ----
        let first = *visible.first().unwrap();
        let last = *visible.last().unwrap();
        let fill_start = rope.line_start_offset(first);
        let fill_end = rope.line_start_offset(last + 1);
        styles.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.0.end.cmp(&b.0.end)));
        let input = std::mem::take(&mut styles);
        let mut filled: Vec<(Range<usize>, HighlightStyle)> =
            Vec::with_capacity(input.len() * 2 + 1);
        let mut cursor = fill_start;
        for (range, style) in input {
            if range.end <= fill_start || range.start >= fill_end {
                continue;
            }
            let s = range.start.max(fill_start);
            let e = range.end.min(fill_end);
            if e <= cursor {
                continue;
            }
            if s > cursor {
                filled.push((cursor..s, HighlightStyle::default()));
            }
            filled.push((s..e, style));
            cursor = e;
        }
        if cursor < fill_end {
            filled.push((cursor..fill_end, HighlightStyle::default()));
        }
        styles = filled;

        // The runs list now covers the whole visible span exactly.
        let consumed_total: usize = styles.iter().map(|(r, _)| r.len()).sum();
        assert_eq!(consumed_total, fill_end - fill_start);

        // prepaint: offsets dropped, (len, color) kept.
        let text_runs: Vec<(usize, Option<gpui::Hsla>)> =
            styles.iter().map(|(r, s)| (r.len(), s.color)).collect();

        // ---- fix (2): layout_lines walks global offsets ----
        let first_line_start = rope.line_start_offset(first);
        let mut total_consumed = 0usize;
        for &buffer_line in &visible {
            let line_text: String = rope.slice_line(buffer_line).into();
            let run_offset = rope.line_start_offset(buffer_line) - first_line_start;
            let g_range = run_offset..(run_offset + line_text.len());

            let mut line_runs: Vec<(usize, Option<gpui::Hsla>)> = Vec::new();
            let mut cur = 0usize;
            for &(len, color) in &text_runs {
                let run_start = cur;
                let run_end = cur + len;
                if run_end <= g_range.start {
                    cur = run_end;
                    continue;
                }
                if run_start >= g_range.end {
                    break;
                }
                let s = g_range.start.max(run_start) - run_start;
                let e = g_range.end.min(run_end) - run_start;
                if e > s {
                    line_runs.push((e - s, color));
                }
                cur = run_end;
            }

            // Rebuild per-byte colors for this line.
            let mut line_colors = vec![None; line_text.len()];
            let mut pos = 0usize;
            for &(len, color) in &line_runs {
                for b in pos..(pos + len).min(line_colors.len()) {
                    line_colors[b] = color;
                }
                pos += len;
            }

            // Expected colors from the original (pre-fill) styles.
            let line_global = g_range;
            let mut expected: Vec<Option<gpui::Hsla>> = vec![None; line_text.len()];
            for (r, s) in &[(0..4, gpui::red()), (10..14, gpui::blue())] {
                let s0 = r.start.max(line_global.start);
                let e0 = r.end.min(line_global.end);
                if e0 > s0 {
                    for b in (s0 - line_global.start)..(e0 - line_global.start) {
                        expected[b] = Some(*s);
                    }
                }
            }

            assert_eq!(
                line_colors, expected,
                "line {buffer_line} colors misaligned after fold"
            );
            total_consumed += line_text.len();
        }
        assert_eq!(total_consumed, 8, "only visible bytes consumed");
    }

    // ---- scroll-sync line mapping (regression: ping-pong between panes) ----

    fn hunk(old: Range<u32>, new: Range<u32>) -> DiffHunk {
        DiffHunk {
            old_rows: old,
            new_rows: new,
            old_word_ranges: Vec::new(),
            new_word_ranges: Vec::new(),
        }
    }

    /// The counterexample from the audit: a hunk that shrinks `10..20` to
    /// `10..12`. Before the clamp, `15 -> 15 -> 23 -> 15 -> 23 ...` never
    /// settled, so `sync_scroll` corrected both panes on every frame.
    #[test]
    fn test_scroll_sync_map_shrinking_hunk_settles() {
        let hunks = vec![hunk(10..20, 10..12)];
        assert_eq!(map_line_old_to_new(15, &hunks), 11);
        // Round trip must be a fixed point, not an oscillation.
        assert_eq!(map_line_new_to_old(11, &hunks), 11);
        assert_eq!(map_line_old_to_new(11, &hunks), 11);
    }

    #[test]
    fn test_scroll_sync_map_never_leaves_the_hunk() {
        let hunks = vec![
            hunk(10..20, 10..12), // shrink
            hunk(30..40, 22..42), // grow
            hunk(50..52, 52..62), // pure insertion region on the new side
        ];
        for old in 0..80u32 {
            let new = map_line_old_to_new(old, &hunks);
            let starts_hunk = hunks.iter().any(|h| h.old_rows.contains(&old));
            if starts_hunk {
                let h = hunks.iter().find(|h| h.old_rows.contains(&old)).unwrap();
                assert!(
                    new >= h.new_rows.start && new < h.new_rows.end.max(h.new_rows.start + 1),
                    "old {old} inside new {:?} mapped to {new}, outside the new range",
                    h.new_rows
                );
            }
        }
    }

    #[test]
    fn test_scroll_sync_map_is_monotone() {
        let hunks = vec![
            hunk(10..20, 10..12),
            hunk(30..40, 22..42),
            hunk(50..55, 52..53),
        ];
        let mut prev_old = map_line_old_to_new(0, &hunks);
        for old in 1..100u32 {
            let cur = map_line_old_to_new(old, &hunks);
            assert!(cur >= prev_old, "old->new not monotone at {old}");
            prev_old = cur;
        }
        let mut prev_new = map_line_new_to_old(0, &hunks);
        for new in 1..100u32 {
            let cur = map_line_new_to_old(new, &hunks);
            assert!(cur >= prev_new, "new->old not monotone at {new}");
            prev_new = cur;
        }
    }

    /// Outside every hunk the two sides are offset by a constant delta, and
    /// that mapping must be exactly invertible.
    #[test]
    fn test_scroll_sync_map_context_is_exact() {
        let hunks = vec![hunk(10..20, 10..12), hunk(30..40, 22..42)];
        for old in [0u32, 5, 9, 25, 29, 45, 60] {
            let new = map_line_old_to_new(old, &hunks);
            assert_eq!(
                map_line_new_to_old(new, &hunks),
                old,
                "context row {old} did not survive the round trip"
            );
        }
        // Deltas: -8 after the first hunk, +2 after the second.
        assert_eq!(map_line_old_to_new(25, &hunks), 17);
        assert_eq!(map_line_old_to_new(45, &hunks), 47);
    }

    /// A pure deletion has an empty `new_rows` range; the mapping must not
    /// underflow or escape the hunk.
    #[test]
    fn test_scroll_sync_map_pure_deletion_does_not_underflow() {
        let hunks = vec![hunk(10..20, 10..10)];
        assert_eq!(map_line_old_to_new(14, &hunks), 10);
        assert_eq!(map_line_old_to_new(19, &hunks), 10);
        // Row 25 sits 5 rows past the hunk, so it shifts by -10.
        assert_eq!(map_line_old_to_new(25, &hunks), 15);
    }

    #[test]
    fn test_scroll_sync_map_pure_insertion_is_transparent() {
        let hunks = vec![hunk(10..10, 10..20)];
        // No old row falls inside the hunk, so every row keeps its number
        // until the inserted block is passed, then shifts by +10.
        assert_eq!(map_line_old_to_new(9, &hunks), 9);
        assert_eq!(map_line_old_to_new(10, &hunks), 20);
        assert_eq!(map_line_new_to_old(15, &hunks), 10);
    }

    /// Exhaustive check that the alternating correction loop used by
    /// `sync_scroll` reaches a fixed point within one round trip.
    #[test]
    fn test_scroll_sync_round_trip_is_idempotent() {
        // Every hunkset below is a *consistent* diff: the context gap between
        // two consecutive hunks is the same number of rows on both sides.
        let hunksets = vec![
            vec![hunk(10..20, 10..12)],
            vec![hunk(10..12, 10..20)],
            vec![hunk(10..20, 10..12), hunk(30..40, 22..42)],
            vec![
                hunk(10..20, 10..12),
                hunk(30..40, 22..42),
                hunk(50..55, 52..53),
            ],
            vec![hunk(0..5, 0..30), hunk(20..25, 45..46)],
            vec![hunk(10..20, 10..10)],
            vec![hunk(10..10, 10..20)],
            Vec::new(),
        ];
        for hunks in &hunksets {
            for old in 0..80u32 {
                let y0 = map_line_old_to_new(old, hunks);
                let x1 = map_line_new_to_old(y0, hunks);
                let y1 = map_line_old_to_new(x1, hunks);
                let x2 = map_line_new_to_old(y1, hunks);
                assert_eq!(
                    y1, y0,
                    "new-side oscillation at old {old} ({y0} -> {x1} -> {y1})"
                );
                assert_eq!(
                    x2, x1,
                    "old-side oscillation at old {old} ({y0} -> {x1} -> {y1} -> {x2})"
                );
            }
        }
    }
}
