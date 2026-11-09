//! Drawing a [`DiffState`]: a virtual list of file headers, folded ranges and lines, and the
//! handlers that feed selection, expansion and the reading position back into the state.
//!
//! The layout follows rune-kit's diff component: a header row per file, a folded row per
//! unchanged range, and lines that are either unified (removed above added) or split (old on
//! the left, new on the right). Code is drawn as one [`StyledText`] per cell, with syntax
//! colours under the word-change backgrounds.

use std::ops::Range;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, ListAlignment, ListOffset, ListState,
    MouseButton, MouseDownEvent, Pixels, Render, SharedString, StatefulInteractiveElement,
    StyledText, Subscription, TextRun, WeakEntity, Window, div, px,
};
use ropey::Rope;

use super::model::{
    DiffHunk, FileDiff, FileStatus, Hunk, LineKind, Side, build_hunk_patch, compute_hunks,
    language_of, line_starts,
};
use super::state::{DiffMode, DiffState, Expand, LineRef, Row};
use super::theme::{BAR, CODE_PAD, DiffTheme, GAP_H, HEADER_H, NOTE_H};
use crate::components::menu::{PopupMenu, PopupMenuItem};
use crate::components::tooltip::Tooltip;
use crate::editor::editor::Editor;
use crate::editor::git_repository::GitRepository;
use crate::editor::syntax::SimpleHighlighter;
use crate::theme::ActiveTheme;

/// Minimum content width at which the automatic layout splits into two columns.
const SPLIT_MIN_WIDTH: f32 = 960.0;

/// Maximum length of a line that participates in syntax highlighting.
const MAX_HIGHLIGHT_LINE_LENGTH: usize = 10_000;

/// Past this size (old + new bytes) syntax highlighting is skipped for speed.
const MAX_HIGHLIGHT_BYTES: usize = 600 * 1024;

/// A stale hunk may drift this many rows from its recomputed position and still be matched;
/// farther means the edit was elsewhere and the operation is abandoned.
const REVERT_MATCH_TOLERANCE: u32 = 20;

/// Layout preference for the diff view.
///
/// The default is [`LayoutPreference::Unified`] (single column). `Auto` is opt-in: it keeps
/// the width-based behaviour (split once [`SPLIT_MIN_WIDTH`] is available).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LayoutPreference {
    Auto,
    /// 默认布局：单栏（合并视图）。
    #[default]
    Unified,
    Split,
}

impl LayoutPreference {
    fn resolve(self, available_width: f32) -> DiffMode {
        match self {
            LayoutPreference::Unified => DiffMode::Unified,
            LayoutPreference::Split => DiffMode::Split,
            LayoutPreference::Auto => {
                if available_width >= SPLIT_MIN_WIDTH {
                    DiffMode::Split
                } else {
                    DiffMode::Unified
                }
            }
        }
    }
}

/// Diff 对比目标：工作区（基线=暂存区或 HEAD，只读评审）或已暂存
/// （HEAD↔暂存区，只读）。VSCode 按点击来源分组决定展示哪一种，同一文件共用一个 tab。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffTarget {
    Worktree,
    Staged,
}

/// 文本指纹：判断 hunks 是否基于当前文本计算。
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

/// Highlight runs of one side of the file, split per line so drawing a row is a lookup.
#[derive(Default)]
struct Highlights {
    /// For each line (0-based), the runs on it, relative to its first byte.
    lines: Vec<Vec<(Range<usize>, HighlightStyle)>>,
}

impl Highlights {
    fn of(
        text: &str,
        language: Option<&str>,
        theme: &crate::editor::syntax::HighlightTheme,
    ) -> Highlights {
        let count = super::model::line_count(text) as usize;
        let mut lines = vec![Vec::new(); count];
        if text.is_empty() || count == 0 || text.len() > MAX_HIGHLIGHT_BYTES {
            return Highlights { lines };
        }
        let starts = line_starts(text);
        let mut highlighter = SimpleHighlighter::new(language.unwrap_or("text"));
        let rope = Rope::from_str(text);
        highlighter.update(None, &rope, None);
        let runs = highlighter.styles(&(0..text.len()), theme);
        for (range, style) in runs {
            let mut i = starts
                .partition_point(|&s| s <= range.start)
                .saturating_sub(1);
            while i < count && starts[i] < range.end {
                let line_start = starts[i];
                let line_end = starts[i + 1].min(text.len());
                // The drawn line has no line break (`\r\n` or `\n`).
                let mut content_end = line_end;
                if content_end > line_start && text.as_bytes()[content_end - 1] == b'\n' {
                    content_end -= 1;
                    if content_end > line_start && text.as_bytes()[content_end - 1] == b'\r' {
                        content_end -= 1;
                    }
                }
                let (s, e) = (range.start.max(line_start), range.end.min(content_end));
                if e > s {
                    lines[i].push((s - line_start..e - line_start, style));
                }
                i += 1;
            }
        }
        Highlights { lines }
    }

    /// The runs on line `n` (1-based).
    fn line(&self, n: u32) -> &[(Range<usize>, HighlightStyle)] {
        let Some(i) = n.checked_sub(1).map(|i| i as usize) else {
            return &[];
        };
        self.lines.get(i).map(Vec::as_slice).unwrap_or(&[])
    }
}

pub struct DiffView {
    path: String,
    repo: GitRepository,
    editor: WeakEntity<Editor>,
    /// 左侧基线文本：工作区视图 = 暂存区（部分暂存时）否则 HEAD；已暂存视图 = HEAD。
    base_text: Arc<str>,
    /// 右侧文本（工作区或暂存区内容），视图只读。
    new_text: String,
    /// 最近一次加载/写盘时的右侧文本（还原时作基线）。
    saved_text: String,
    hunks: Vec<DiffHunk>,
    hunks_source: TextFingerprint,
    /// The rows the view draws and their selection.
    diff: DiffState,
    old_hl: Highlights,
    new_hl: Highlights,
    preference: LayoutPreference,
    available_width: f32,
    list_state: ListState,
    /// The `DiffState::rev` the list was last reset for.
    list_rev: u64,
    focus_handle: FocusHandle,
    /// Colours and metrics for the current theme/font settings, rebuilt each frame.
    theme: DiffTheme,
    target: DiffTarget,
    is_new: bool,
    is_binary: bool,
    loading: bool,
    load_error: Option<String>,
    refreshing: bool,
    staging_hunk: Option<usize>,
    staging_file: bool,
    diff_menu: Option<Entity<PopupMenu>>,
    diff_menu_position: Option<gpui::Point<Pixels>>,
    diff_menu_subscription: Option<Subscription>,
    /// The hunk the viewport is in, for the toolbar's counter.
    current_hunk: usize,
    /// The first load's jump to the first change has been made.
    jumped: bool,
    recompute_gen: u32,
    _subscriptions: Vec<Subscription>,
}

impl DiffView {
    /// Creates the view with an empty list and fetches content in the background.
    /// The tab appears instantly; batched remote fetch makes it feel local.
    pub fn new_loading(
        path: String,
        repo: GitRepository,
        staged: bool,
        editor: WeakEntity<Editor>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            path,
            repo,
            editor,
            base_text: "".into(),
            new_text: String::new(),
            saved_text: String::new(),
            hunks: Vec::new(),
            hunks_source: TextFingerprint::default(),
            diff: DiffState::new(Vec::new()),
            old_hl: Highlights::default(),
            new_hl: Highlights::default(),
            preference: LayoutPreference::default(),
            available_width: 0.0,
            list_state: ListState::new(0, ListAlignment::Top, px(600.))
                .with_uniform_item_height(px(22.)),
            list_rev: 0,
            focus_handle: cx.focus_handle(),
            theme: DiffTheme::new(cx),
            target: if staged {
                DiffTarget::Staged
            } else {
                DiffTarget::Worktree
            },
            is_new: false,
            is_binary: false,
            loading: true,
            load_error: None,
            refreshing: false,
            staging_hunk: None,
            staging_file: false,
            diff_menu: None,
            diff_menu_position: None,
            diff_menu_subscription: None,
            current_hunk: 0,
            jumped: false,
            recompute_gen: 0,
            _subscriptions: Vec::new(),
        };
        this.watch_scroll(cx);
        this.fetch_contents(cx);
        this
    }

    /// The list's scroll position chooses the toolbar's current hunk.
    fn watch_scroll(&mut self, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        let mut last = usize::MAX;
        self.list_state
            .set_scroll_handler(move |_event, _window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let top = this.list_state.logical_scroll_top();
                    let ix = top.item_ix;
                    if ix == last {
                        return;
                    }
                    last = ix;
                    let mut hunk = None;
                    for (i, &row) in this.diff_hunk_rows().iter().enumerate() {
                        if row as usize <= ix {
                            hunk = Some(i);
                        }
                    }
                    let hunk = hunk.unwrap_or(0);
                    if hunk != this.current_hunk {
                        this.current_hunk = hunk;
                        cx.notify();
                    }
                });
            });
    }

    fn diff_hunk_rows(&self) -> Vec<u32> {
        (0..self.diff.hunk_count())
            .filter_map(|i| self.diff.hunk_row(i))
            .collect()
    }

    fn fetch_contents(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.load_error = None;
        self.close_diff_menu(cx);
        cx.notify();

        let repo = self.repo.clone();
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.diff_base_contents(&path) })
                .await;

            this.update(cx, |view, cx| {
                match result {
                    Ok(base) => {
                        view.loading = false;
                        view.load_error = None;
                        view.is_new = base.is_new;
                        view.is_binary = base.is_binary;
                        let left_text = match view.target {
                            DiffTarget::Worktree => base.worktree_base().to_string(),
                            DiffTarget::Staged => base.head.clone(),
                        };
                        view.base_text = left_text.into();
                        view.saved_text = match view.target {
                            DiffTarget::Worktree => base.worktree.clone(),
                            DiffTarget::Staged => base.staged.clone(),
                        };
                        view.new_text = view.saved_text.clone();
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

    /// 切换工作区/已暂存内容（同一 tab 内）：重拉三元组。
    pub fn set_kind(&mut self, staged: bool, cx: &mut Context<Self>) {
        let kind = if staged {
            DiffTarget::Staged
        } else {
            DiffTarget::Worktree
        };
        if self.target == kind {
            return;
        }
        self.target = kind;
        self.fetch_contents(cx);
    }

    /// 仅刷新左侧基线（暂存 hunk/文件后暂存区变了，右栏不动）。
    fn refresh_base(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let path = self.path.clone();
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.diff_base_contents(&path) })
                .await;
            weak.update(cx, |view, cx| {
                if let Ok(base) = result {
                    view.is_new = base.is_new;
                    view.is_binary = base.is_binary;
                    let left_text = match view.target {
                        DiffTarget::Worktree => base.worktree_base().to_string(),
                        DiffTarget::Staged => base.head.clone(),
                    };
                    view.base_text = left_text.into();
                    if view.target == DiffTarget::Staged {
                        view.saved_text = base.staged.clone();
                        view.new_text = base.staged;
                    }
                    view.recompute(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn notify_success(&self, cx: &mut Context<Self>, message: &str, refresh_marks: bool) {
        let message = message.to_string();
        let path = self.path.clone();
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |ed, cx| {
                ed.show_notification(
                    message,
                    crate::components::notification::NotificationKind::Success,
                    cx,
                );
                if refresh_marks {
                    ed.refresh_git_marks_for_path(&path, cx);
                }
            });
        }
    }

    fn notify_error(&self, cx: &mut Context<Self>, message: String) {
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |ed, cx| {
                ed.show_notification(
                    message,
                    crate::components::notification::NotificationKind::Error,
                    cx,
                );
            });
        }
    }

    fn notify_warning(&self, cx: &mut Context<Self>, message: &str) {
        let message = message.to_string();
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |ed, cx| {
                ed.show_notification(
                    message,
                    crate::components::notification::NotificationKind::Warning,
                    cx,
                );
            });
        }
    }

    /// 暂存单个 hunk（`git apply --cached` 零上下文补丁）。
    fn stage_hunk(&mut self, hunk_ix: usize, cx: &mut Context<Self>) {
        if self.target != DiffTarget::Worktree || self.staging_hunk.is_some() {
            return;
        }
        if self.is_binary {
            self.notify_warning(cx, "二进制文件不支持按块暂存");
            return;
        }
        let Some(hunk) = self.resolve_hunk_for_revert(hunk_ix, &self.new_text.clone()) else {
            self.notify_warning(cx, "该块位置已变化，请稍后重试");
            return;
        };
        let patch = build_hunk_patch(
            &self.path,
            &self.base_text,
            &self.new_text,
            &hunk,
            self.is_new,
        );
        self.staging_hunk = Some(hunk_ix);
        cx.notify();

        let repo = self.repo.clone();
        let path = self.path.clone();
        let this_weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.stage_hunk_patch(&patch) })
                .await;
            this_weak
                .update(cx, |view, cx| {
                    view.staging_hunk = None;
                    match result {
                        Ok(()) => {
                            view.notify_success(cx, "已暂存该块", true);
                            super::super::git_repository::invalidate_remote_diff_cache(
                                view.repo.root(),
                                &path,
                            );
                            view.refresh_base(cx);
                        }
                        Err(err) => view.notify_error(cx, format!("暂存该块失败：{err}")),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// 暂存整个文件（工具栏）。
    fn stage_file(&mut self, cx: &mut Context<Self>) {
        if self.staging_file || self.target != DiffTarget::Worktree {
            return;
        }
        self.staging_file = true;
        cx.notify();
        let repo = self.repo.clone();
        let path = self.path.clone();
        let this_weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let path_for_task = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { repo.stage(&[path_for_task.as_str()]) })
                .await;
            this_weak
                .update(cx, |view, cx| {
                    view.staging_file = false;
                    match result {
                        Ok(()) => {
                            view.notify_success(cx, "已暂存该文件", true);
                            view.refresh_base(cx);
                        }
                        Err(err) => view.notify_error(cx, format!("暂存失败：{err}")),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Recompute the diff and the rows, in the background, then hand them to the view.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        self.recompute_gen += 1;
        let generation = self.recompute_gen;

        let old_text = self.base_text.to_string();
        let new_text = self.new_text.clone();
        let path = self.path.clone();
        let binary = self.is_binary;
        let language = language_of(&path).map(str::to_owned);
        let theme = cx.theme().highlight_theme.clone();
        let mode = self.preference.resolve(self.available_width);
        let top_key = self.top_row_key();

        cx.spawn(async move |this, cx| {
            let task_old = old_text.clone();
            let task_new = new_text.clone();
            let task_path = path.clone();
            let task_language = language.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let file = if binary {
                        FileDiff::binary(Some(task_path.clone()), Some(task_path.clone()))
                    } else {
                        FileDiff::from_texts(task_path, &task_old, &task_new)
                    };
                    let additions = file.additions();
                    let deletions = file.deletions();
                    // The same blocks the view draws: staging and reverting act on exactly what
                    // the reader sees, and one diff pass serves both.
                    let hunks: Vec<DiffHunk> = file.hunks.iter().map(hunk_ranges).collect();
                    let old_hl = if binary {
                        Highlights::default()
                    } else {
                        Highlights::of(&task_old, task_language.as_deref(), &theme)
                    };
                    let new_hl = if binary {
                        Highlights::default()
                    } else {
                        Highlights::of(&task_new, task_language.as_deref(), &theme)
                    };
                    (file, hunks, old_hl, new_hl, additions, deletions)
                })
                .await;

            this.update(cx, |view, cx| {
                if view.recompute_gen != generation {
                    return;
                }
                let (file, hunks, old_hl, new_hl, _additions, _deletions) = result;
                view.hunks = hunks;
                view.hunks_source = TextFingerprint::of(&view.new_text);
                view.old_hl = old_hl;
                view.new_hl = new_hl;
                view.diff.set_files(vec![file]);
                view.diff.set_mode(mode);
                view.sync_list();
                if let Some(key) = top_key {
                    view.restore_top(&key);
                }
                view.current_hunk = 0;
                view.loading = false;
                // Open on the first change, as a review starts there, not on the file head.
                if !view.jumped {
                    view.jumped = true;
                    if let Some(row) = view.diff.first_hunk_row() {
                        view.scroll_to_row(row, 1, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The key of the row at the top of the viewport, to keep the reader's place when the
    /// rows are rebuilt.
    fn top_row_key(&self) -> Option<String> {
        let ix = self.list_state.logical_scroll_top().item_ix;
        self.diff
            .key(ix.min(self.diff.keys.len().saturating_sub(1)))
            .map(str::to_owned)
    }

    /// Scroll back to the row that `key` names, if it survived the rebuild.
    fn restore_top(&self, key: &str) {
        if let Some(ix) = self.diff.row_of_key(key) {
            self.list_state.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
        }
    }

    /// Reset the list when the rows changed, so its item count and heights match again.
    fn sync_list(&mut self) {
        if self.list_rev != self.diff.rev {
            self.list_rev = self.diff.rev;
            let count = self.diff.rows.len();
            let estimate = self.diff_line_height();
            self.list_state.reset_with_uniform_height(count, estimate);
        }
    }

    fn diff_line_height(&self) -> Pixels {
        self.theme.line_height
    }

    /// Scroll the list to `row` with `context` rows above it.
    fn scroll_to_row(&mut self, row: u32, context: u32, cx: &mut Context<Self>) {
        self.sync_list();
        let ix = row.saturating_sub(context) as usize;
        let ix = ix.min(self.diff.rows.len().saturating_sub(1));
        self.list_state.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    /// Handle a key press: `j`/`k` move between hunks, `[`/`]` between files (a single file
    /// here), Escape clears the selection and Cmd-C copies it.
    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        if mods.platform && key.eq_ignore_ascii_case("c") {
            if let Some(text) = self.diff.selected_text() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                return true;
            }
            return false;
        }
        if mods.platform || mods.control || mods.alt {
            return false;
        }
        match key {
            "j" => {
                let from = self.list_state.logical_scroll_top().item_ix as u32;
                if let Some(row) = self.diff.next_hunk_row(from, true) {
                    self.scroll_to_row(row, 1, cx);
                }
                true
            }
            "k" => {
                let from = self.list_state.logical_scroll_top().item_ix as u32;
                if let Some(row) = self.diff.next_hunk_row(from, false) {
                    self.scroll_to_row(row, 1, cx);
                }
                true
            }
            "escape" => {
                if self.diff.selection().is_none() {
                    return false;
                }
                self.diff.set_selection(None);
                cx.notify();
                true
            }
            _ => false,
        }
    }

    fn active_hunk_index(&self) -> Option<usize> {
        let rows = self.diff_hunk_rows();
        if rows.is_empty() {
            return None;
        }
        let top = self.list_state.logical_scroll_top().item_ix as u32;
        let ix = rows.iter().rposition(|&r| r <= top).unwrap_or(0);
        Some(ix)
    }

    /// Jump to hunk `ix`: its row goes near the top of the viewport.
    fn jump_to_hunk(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.diff.hunk_row(ix) else {
            return;
        };
        self.scroll_to_row(row, 0, cx);
        self.current_hunk = ix;
    }

    fn expand_gap(&mut self, file: usize, gap: usize, how: Expand, cx: &mut Context<Self>) {
        let Some((old, new, _)) = self.diff.gap_range(file, gap) else {
            return;
        };
        self.diff.expand(file, gap, how);
        self.sync_list();
        // Keep the first revealed line where the folded row was.
        let side = if self.diff.single_column(file) == Some(Side::Old) {
            Side::Old
        } else {
            Side::New
        };
        let at = LineRef::new(side, if side == Side::Old { old } else { new });
        if let Some(row) = self.diff.row_of_line(file, at) {
            self.list_state.scroll_to(ListOffset {
                item_ix: row as usize,
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }

    fn expand_all(&mut self, cx: &mut Context<Self>) {
        let anchor = self.top_row_key();
        self.diff.expand_all();
        self.sync_list();
        if let Some(key) = anchor {
            self.restore_top(&key);
        }
        cx.notify();
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.diff.collapse_all();
        self.sync_list();
        let top = self.diff.first_hunk_row().unwrap_or(0) as usize;
        self.list_state.scroll_to(ListOffset {
            item_ix: top,
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    fn effective_mode(&self) -> DiffMode {
        self.preference.resolve(self.available_width)
    }

    /// Called by the editor panel each render with the current content width.
    pub fn set_available_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if (self.available_width - width).abs() < 1.0 {
            return;
        }
        let before = self.effective_mode();
        self.available_width = width;
        let after = self.effective_mode();
        if after != before {
            let anchor = self.top_row_key();
            self.diff.set_mode(after);
            self.sync_list();
            if let Some(key) = anchor {
                self.restore_top(&key);
            }
            cx.notify();
        }
    }

    fn set_preference(&mut self, preference: LayoutPreference, cx: &mut Context<Self>) {
        if self.preference == preference {
            return;
        }
        self.preference = preference;
        let mode = self.effective_mode();
        let anchor = self.top_row_key();
        self.diff.set_mode(mode);
        self.sync_list();
        if let Some(key) = anchor {
            self.restore_top(&key);
        }
        cx.notify();
    }

    /// 把点击的 hunk 下标解析为**与当前右侧文本一致**的 hunk。
    ///
    /// 只读视图下 hunks 与文本总是同步的；这里仍保留指纹校验，以防外部刷新与点击竞争。
    fn resolve_hunk_for_revert(&mut self, hunk_ix: usize, current: &str) -> Option<DiffHunk> {
        let stale = self.hunks.get(hunk_ix)?.clone();
        if self.hunks_source == TextFingerprint::of(current) {
            return Some(stale);
        }
        let fresh = compute_hunks(&self.base_text, current);
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

    /// Revert one hunk: replace the new-side rows with the base rows and write the file back.
    ///
    /// The view is read-only, so this is the only path that changes the working tree from
    /// here: it writes through `save_text_to_file`, keeping the file's encoding and line
    /// endings, then reloads.
    fn revert_hunk(&mut self, hunk_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_binary {
            self.notify_warning(cx, "二进制文件不支持按块还原");
            return;
        }
        if self.target != DiffTarget::Worktree {
            self.notify_warning(cx, "已暂存视图只读，不能还原");
            return;
        }
        let current = self.new_text.clone();
        let Some(hunk) = self.resolve_hunk_for_revert(hunk_ix, &current) else {
            cx.notify();
            return;
        };
        let starts = line_starts(&current);
        let count = super::model::line_count(&current);
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

        let old_starts = line_starts(&self.base_text);
        let old_count = super::model::line_count(&self.base_text);
        let repl_start = if hunk.old_rows.start < old_count {
            old_starts[hunk.old_rows.start as usize]
        } else {
            self.base_text.len()
        };
        let repl_end = if hunk.old_rows.end < old_count {
            old_starts[hunk.old_rows.end as usize]
        } else {
            self.base_text.len()
        };
        let replacement = &self.base_text[repl_start..repl_end];

        let mut updated =
            String::with_capacity(current.len() - (byte_end - byte_start) + replacement.len());
        updated.push_str(&current[..byte_start]);
        updated.push_str(replacement);
        updated.push_str(&current[byte_end..]);

        let repo = self.repo.clone();
        let path = self.path.clone();
        let baseline = self.saved_text.clone();
        let this_weak = cx.entity().downgrade();
        let full_path = repo.resolve_path(&path);
        let ssh_config = repo.ssh_config();
        window.defer(cx, move |_window, cx| {
            cx.spawn(async move |cx| {
                let write = updated.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let encoding = repo.detect_encoding(&path);
                        crate::editor::editor::save_text_to_file(
                            &full_path,
                            ssh_config.as_ref(),
                            &baseline,
                            &write,
                            encoding,
                        )
                    })
                    .await;
                this_weak
                    .update(cx, |view, cx| {
                        match result {
                            Ok(_written) => {
                                view.saved_text = updated.clone();
                                view.new_text = updated;
                                view.refresh(cx);
                                view.notify_success(cx, "已还原该块", true);
                            }
                            Err(e) => {
                                view.notify_error(cx, format!("还原失败：{e}"));
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .detach();
        });
    }

    /// 是否有未保存修改。只读视图恒为 `false`；保留该接口供编辑器查询。
    pub fn is_dirty(&self) -> bool {
        false
    }

    /// Cmd-S on a diff tab: the view is read-only, so there is nothing to save.
    pub fn save(&mut self, _cx: &mut Context<Self>) {}

    /// Re-fetch HEAD + staged + working-tree content.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing || self.loading {
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
                    async move { repo.diff_base_contents(&path) }
                })
                .await;
            this.update(cx, |view, cx| {
                view.refreshing = false;
                if let Ok(base) = result {
                    view.is_new = base.is_new;
                    view.is_binary = base.is_binary;
                    let left_text = match view.target {
                        DiffTarget::Worktree => base.worktree_base().to_string(),
                        DiffTarget::Staged => base.head.clone(),
                    };
                    view.base_text = left_text.into();
                    match view.target {
                        DiffTarget::Worktree => {
                            view.saved_text = base.worktree.clone();
                            view.new_text = base.worktree;
                        }
                        DiffTarget::Staged => {
                            view.saved_text = base.staged.clone();
                            view.new_text = base.staged;
                        }
                    }
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

    // ---- menu ---------------------------------------------------------------

    fn close_diff_menu(&mut self, cx: &mut Context<Self>) {
        self.diff_menu.take();
        self.diff_menu_position = None;
        self.diff_menu_subscription.take();
        cx.notify();
    }

    fn render_diff_menu_overlay(&self) -> Option<AnyElement> {
        let menu = self.diff_menu.clone()?;
        let position = self.diff_menu_position.unwrap_or_default();
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

    fn open_diff_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        let is_worktree = self.target == DiffTarget::Worktree;
        let staging = self.staging_hunk.is_some() || self.staging_file;
        let has_hunks = !self.hunks.is_empty();
        let has_hidden = (0..self.diff.hunk_count() + 1).any(|g| self.diff.hidden(0, g) > 0);
        let preference = self.preference;
        let path = self.path.clone();

        self.diff_menu.take();
        self.diff_menu_subscription.take();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let mut menu = menu;
            if is_worktree && !staging && has_hunks {
                let this = entity.clone();
                menu = menu.item(PopupMenuItem::new("暂存该块").on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.close_diff_menu(cx);
                        if let Some(ix) = this.active_hunk_index() {
                            this.stage_hunk(ix, cx);
                        }
                    });
                }));
                let this = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new("还原该块").on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| {
                            this.close_diff_menu(cx);
                            if let Some(ix) = this.active_hunk_index() {
                                this.revert_hunk(ix, window, cx);
                            }
                        });
                    }),
                );
                let this = entity.clone();
                menu = menu.item(PopupMenuItem::new("暂存文件").on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.close_diff_menu(cx);
                        this.stage_file(cx);
                    });
                }));
                menu = menu.separator();
            }
            if has_hidden {
                let this = entity.clone();
                menu = menu.item(PopupMenuItem::new("展开全部未更改区域").on_click(
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_diff_menu(cx);
                            this.expand_all(cx);
                        });
                    },
                ));
            }
            {
                let this = entity.clone();
                menu = menu.item(PopupMenuItem::new("折叠全部未更改区域").on_click(
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.close_diff_menu(cx);
                            this.collapse_all(cx);
                        });
                    },
                ));
            }
            {
                let this = entity.clone();
                menu = menu.item(PopupMenuItem::new("刷新").on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.close_diff_menu(cx);
                        this.refresh(cx);
                    });
                }));
            }
            {
                let this = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new("打开文件").on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| {
                            this.close_diff_menu(cx);
                            this.open_file(window, cx);
                        });
                    }),
                );
            }
            {
                let path = path.clone();
                menu = menu.item(PopupMenuItem::new("复制路径").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.clone()));
                }));
            }
            menu = menu.separator();
            let options = [
                ("自动布局（按宽度）", LayoutPreference::Auto),
                ("单栏（合并视图）", LayoutPreference::Unified),
                ("并排（对照视图）", LayoutPreference::Split),
            ];
            for (label, pref) in options {
                let this = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(preference == pref)
                        .on_click(move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.close_diff_menu(cx);
                                this.set_preference(pref, cx);
                            });
                        }),
                );
            }
            menu
        });
        menu.focus_handle(cx).focus(window, cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &gpui::DismissEvent, cx| {
            this.close_diff_menu(cx);
        });
        self.diff_menu = Some(menu);
        self.diff_menu_position = Some(position);
        self.diff_menu_subscription = Some(subscription);
        cx.notify();
    }

    // ---- rendering ----------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_worktree = self.target == DiffTarget::Worktree;

        let hunk_total = self.diff.hunk_count();
        let hunk_current = self.active_hunk_index().map(|i| i + 1).unwrap_or(0);
        let nav_enabled = hunk_total > 0;

        let icon_btn = |id: &'static str,
                        icon: crate::assets::Icon,
                        tip: SharedString,
                        enabled: bool,
                        theme: &crate::theme::UiTheme| {
            div()
                .id(id)
                .w(px(26.0))
                .h(px(26.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .tooltip(Tooltip::text(tip))
                .when(enabled, |this| this.cursor_pointer())
                .text_color(if enabled {
                    theme.text.opacity(0.85)
                } else {
                    theme.text_subtle
                })
                .when(enabled, |this| {
                    this.hover(|s| s.bg(theme.tab_hover_background).text_color(theme.text))
                })
                .child(
                    gpui::svg()
                        .size(px(14.0))
                        .text_color(if enabled {
                            theme.text.opacity(0.85)
                        } else {
                            theme.text_subtle
                        })
                        .path(icon.path()),
                )
        };

        let mode_button = |id: &'static str, label: &'static str, mode: DiffMode| {
            let selected = self.effective_mode() == mode;
            div()
                .id(id)
                .px(px(8.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .text_size(px(11.0))
                .when(selected, |this| {
                    this.bg(theme.elevated_background).text_color(theme.text)
                })
                .when(!selected, |this| {
                    this.text_color(theme.text_muted)
                        .hover(|s| s.text_color(theme.text))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let pref = match mode {
                        DiffMode::Unified => LayoutPreference::Unified,
                        DiffMode::Split => LayoutPreference::Split,
                    };
                    this.set_preference(pref, cx);
                }))
        };

        div()
            .flex()
            .w_full()
            .h(px(36.0))
            .flex_none()
            .items_center()
            .px(px(8.0))
            .gap(px(4.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.panel_background)
            .text_size(px(12.0))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .truncate()
                    .text_color(theme.text)
                    .child(self.path.clone()),
            )
            .when(!is_worktree, |this| {
                this.child(
                    div()
                        .flex_none()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .bg(theme.elevated_background)
                        .text_size(px(11.0))
                        .text_color(theme.text_subtle)
                        .child("已暂存"),
                )
            })
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex_none()
                            .px(px(6.0))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(format!(
                                "+{} −{}",
                                self.diff
                                    .files()
                                    .first()
                                    .map(|f| f.additions())
                                    .unwrap_or(0),
                                self.diff
                                    .files()
                                    .first()
                                    .map(|f| f.deletions())
                                    .unwrap_or(0),
                            )),
                    )
                    .child(div().id("diff-mode-unified").child(mode_button(
                        "diff-unified",
                        "Unified",
                        DiffMode::Unified,
                    )))
                    .child(div().id("diff-mode-split").child(mode_button(
                        "diff-split",
                        "Split",
                        DiffMode::Split,
                    ))),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .id("diff-prev-hunk")
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .tooltip(Tooltip::text("上一个更改"))
                            .text_size(px(13.0))
                            .when(nav_enabled, |this| this.cursor_pointer())
                            .text_color(if nav_enabled {
                                theme.text_muted
                            } else {
                                theme.text_subtle
                            })
                            .when(nav_enabled, |this| {
                                this.hover(|s| {
                                    s.bg(theme.tab_hover_background).text_color(theme.text)
                                })
                            })
                            .child("↑")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(cur) = this.active_hunk_index() else {
                                    return;
                                };
                                let prev = if cur == 0 {
                                    this.diff.hunk_count() - 1
                                } else {
                                    cur - 1
                                };
                                this.jump_to_hunk(prev, cx);
                            })),
                    )
                    .child(
                        div()
                            .w(px(40.0))
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
                        div()
                            .id("diff-next-hunk")
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .tooltip(Tooltip::text("下一个更改"))
                            .text_size(px(13.0))
                            .when(nav_enabled, |this| this.cursor_pointer())
                            .text_color(if nav_enabled {
                                theme.text_muted
                            } else {
                                theme.text_subtle
                            })
                            .when(nav_enabled, |this| {
                                this.hover(|s| {
                                    s.bg(theme.tab_hover_background).text_color(theme.text)
                                })
                            })
                            .child("↓")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(cur) = this.active_hunk_index() else {
                                    return;
                                };
                                let next = (cur + 1) % this.diff.hunk_count();
                                this.jump_to_hunk(next, cx);
                            })),
                    ),
            )
            .child(
                icon_btn(
                    "diff-more",
                    crate::assets::Icon::Ellipsis,
                    "更多操作".into(),
                    true,
                    &theme,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.open_diff_menu(event.position, window, cx);
                    }),
                ),
            )
    }

    fn render_file_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().ui;
        let Some(file) = self.diff.files().first() else {
            return div().into_any_element();
        };
        let collapsed = self.diff.collapsed(0);
        let path = file.path().to_string();
        let (adds, dels) = (file.additions(), file.deletions());
        let status = match file.status {
            FileStatus::Added => Some(("Added", theme.status_added)),
            FileStatus::Deleted => Some(("Deleted", theme.status_deleted)),
            FileStatus::Renamed => Some(("Renamed", theme.text_muted)),
            FileStatus::Copied => Some(("Copied", theme.text_muted)),
            FileStatus::Modified => None,
        };
        let mut inner = div()
            .flex()
            .w_full()
            .flex_1()
            .px(px(10.0))
            .gap(px(8.0))
            .items_center()
            .child(
                div()
                    .id("diff-collapse-file")
                    .size(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.tab_hover_background))
                    .child(
                        gpui::svg()
                            .size(px(14.0))
                            .path(if collapsed {
                                crate::assets::Icon::ChevronRight.path()
                            } else {
                                crate::assets::Icon::ChevronDown.path()
                            })
                            .text_color(theme.text_muted),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        let collapsed = this.diff.collapsed(0);
                        this.diff.set_collapsed(0, !collapsed);
                        this.sync_list();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(path),
                    ),
            );
        if let Some((word, color)) = status {
            inner = inner.child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(color)
                    .child(word),
            );
        }
        inner = inner.child(
            div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(theme.status_added)
                .child(format!("+{adds}")),
        );
        inner = inner.child(
            div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(theme.status_deleted)
                .child(format!("−{dels}")),
        );

        div()
            .id("diff-file-header")
            .debug_selector(|| "diff-file-header".into())
            .w_full()
            .h(px(HEADER_H))
            .flex()
            .items_center()
            .bg(theme.panel_background)
            .border_b_1()
            .border_color(theme.border)
            .child(inner)
            .into_any_element()
    }

    fn render_note(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().ui;
        let Some(file) = self.diff.files().first() else {
            return div().into_any_element();
        };
        let msg = if file.binary {
            "Binary file not shown"
        } else if file.status == FileStatus::Renamed || file.status == FileStatus::Copied {
            "File moved without changes"
        } else if file.old_mode != file.new_mode {
            "File mode changed"
        } else if file.status == FileStatus::Added || file.status == FileStatus::Deleted {
            "Empty file"
        } else {
            "No changes"
        };
        div()
            .w_full()
            .h(px(NOTE_H))
            .flex()
            .items_center()
            .px(px(16.0))
            .text_size(px(12.0))
            .text_color(theme.text_muted)
            .child(msg)
            .into_any_element()
    }

    fn render_gap(&self, file: usize, gap: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let hidden = self.diff.hidden(file, gap);
        let label = if hidden == 1 {
            "1 unmodified line".to_string()
        } else {
            format!("{hidden} unmodified lines")
        };
        let hunks = self.diff.hunk_count();
        let last = gap == hunks;

        let mut buttons = div().flex().items_center().gap(px(2.0));
        if hidden <= super::state::EXPAND_STEP {
            buttons = buttons.child(self.expand_button(
                file,
                gap,
                Expand::All,
                crate::assets::Icon::ChevronUpDownThick,
                "Show all hidden lines",
                theme,
                cx,
            ));
        } else {
            if gap > 0 || last {
                buttons = buttons.child(self.expand_button(
                    file,
                    gap,
                    Expand::Up,
                    crate::assets::Icon::ChevronUpThick,
                    "Show more lines above",
                    theme,
                    cx,
                ));
            }
            if !last {
                buttons = buttons.child(self.expand_button(
                    file,
                    gap,
                    Expand::Down,
                    crate::assets::Icon::ChevronDownThick,
                    "Show more lines below",
                    theme,
                    cx,
                ));
            }
        }

        let next_header = self
            .diff
            .files()
            .first()
            .and_then(|f| f.hunks.get(gap))
            .map(|h| h.header());

        let mut row = div()
            .id(("diff-gap", gap as u64))
            .debug_selector(move || format!("diff-gap-{gap}"))
            .w_full()
            .h(px(GAP_H))
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(8.0))
            .bg(theme.gap_bg)
            .child(buttons);
        row = row.child(
            div()
                .text_size(px(11.0))
                .text_color(theme.muted)
                .child(label),
        );
        if let Some(header) = next_header {
            row = row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(theme.faint)
                    .child(header),
            );
        }
        row.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_button(
        &self,
        file: usize,
        gap: usize,
        how: Expand,
        icon: crate::assets::Icon,
        tip: &'static str,
        theme: &DiffTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selector = format!(
            "diff-expand-{gap}-{}",
            if how == Expand::All {
                "all"
            } else if how == Expand::Up {
                "up"
            } else {
                "down"
            }
        );
        div()
            .id(("diff-expand", gap as u64 * 64 + how as u64))
            .debug_selector(move || selector)
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover))
            .tooltip(Tooltip::text(tip))
            .child(
                gpui::svg()
                    .size(px(12.0))
                    .path(icon.path())
                    .text_color(theme.muted),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.expand_gap(file, gap, how, cx)))
            .into_any_element()
    }

    fn render_line_row(&self, ix: usize, r: Row, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let st = &self.diff;
        let file = r.file();
        match self.effective_mode() {
            DiffMode::Unified => {
                let side_cell = self.cell_of(r, Side::Old);
                let new_cell = self.cell_of(r, Side::New);
                match (side_cell, new_cell) {
                    (Some(old), Some(new)) if old.kind == LineKind::Context => {
                        self.unified_row(ix, r, Some(old), Some(new), theme, cx)
                    }
                    (Some(cell), _) => {
                        let kind = cell.kind;
                        match kind {
                            LineKind::Removed => {
                                self.unified_row(ix, r, Some(cell), None, theme, cx)
                            }
                            _ => self.unified_row(ix, r, None, Some(cell), theme, cx),
                        }
                    }
                    (None, Some(cell)) => self.unified_row(ix, r, None, Some(cell), theme, cx),
                    (None, None) => div().into_any_element(),
                }
            }
            DiffMode::Split => {
                let single = st.single_column(file);
                let old = self.cell_of(r, Side::Old);
                let new = self.cell_of(r, Side::New);
                let half =
                    |side: Side, cell: Option<Cell>, theme: &DiffTheme, cx: &mut Context<Self>| {
                        self.split_half(ix, r, side, cell, theme, cx)
                    };
                let line = div()
                    .w_full()
                    .min_h(theme.line_height)
                    .flex()
                    .items_stretch();
                match single {
                    Some(Side::Old) => line
                        .child(half(Side::Old, old, theme, cx))
                        .into_any_element(),
                    Some(Side::New) => line
                        .child(half(Side::New, new, theme, cx))
                        .into_any_element(),
                    None => line
                        .child(half(Side::Old, old, theme, cx))
                        .child(
                            div()
                                .id(("diff-divider", ix))
                                .debug_selector(move || format!("diff-divider-{ix}"))
                                .w(px(1.0))
                                .flex_none()
                                .bg(theme.border),
                        )
                        .child(half(Side::New, new, theme, cx))
                        .into_any_element(),
                }
            }
        }
    }

    fn cell_of(&self, r: Row, side: Side) -> Option<Cell> {
        match r {
            Row::Ctx { old, new, .. } => {
                let n = if side == Side::Old { old } else { new };
                let text = self.diff.line_text(r.file(), side, n).unwrap_or("");
                Some(Cell {
                    kind: LineKind::Context,
                    old: Some(old),
                    new: Some(new),
                    text: text.to_owned(),
                    changes: Vec::new(),
                    no_newline: false,
                })
            }
            Row::Line { f, h, i } => {
                let l = &self.diff.files()[f as usize].hunks[h as usize].lines[i as usize];
                if l.kind != LineKind::Context && l.number(side).is_none() {
                    return None;
                }
                let text = if l.kind == LineKind::Context {
                    self.diff
                        .line_text(f as usize, side, l.number(side)?)
                        .unwrap_or(&l.text)
                } else {
                    &l.text
                };
                Some(Cell {
                    kind: l.kind,
                    old: l.old,
                    new: l.new,
                    text: text.to_owned(),
                    changes: l.changes.clone(),
                    no_newline: l.no_newline,
                })
            }
            Row::Pair { f, h, del, add } => {
                let i = if side == Side::Old { del } else { add };
                let l = &self.diff.files()[f as usize].hunks[h as usize].lines[i as usize];
                l.number(side)?;
                let text = if l.kind == LineKind::Context {
                    self.diff
                        .line_text(f as usize, side, l.number(side)?)
                        .unwrap_or(&l.text)
                } else {
                    &l.text
                };
                Some(Cell {
                    kind: l.kind,
                    old: l.old,
                    new: l.new,
                    text: text.to_owned(),
                    changes: l.changes.clone(),
                    no_newline: l.no_newline,
                })
            }
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn unified_row(
        &self,
        ix: usize,
        r: Row,
        old: Option<Cell>,
        new: Option<Cell>,
        theme: &DiffTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let file = r.file();
        let selected = self.diff.row_selected(ix, None);
        let kind = old
            .as_ref()
            .map(|c| c.kind)
            .or_else(|| new.as_ref().map(|c| c.kind))
            .unwrap_or(LineKind::Context);
        let fill = theme.line_fill(kind, selected);
        let digits = {
            let f = &self.diff.files()[file];
            f.last_line().max(1).ilog10() + 1
        };
        let number_w = theme.number_width(digits.max(3));
        let gutter = div()
            .flex_none()
            .self_stretch()
            .flex()
            .items_start()
            .bg(theme.gutter_fill(kind, selected))
            .child(self.bar(
                kind,
                theme,
                format!("diff-bar-old-{}", self.diff.key(ix).unwrap_or("row")),
            ))
            .child(self.number_cell(
                r,
                Side::Old,
                old.as_ref().and_then(|c| c.old),
                kind,
                selected,
                number_w,
                theme,
                cx,
            ))
            .child(self.number_cell(
                r,
                Side::New,
                new.as_ref().and_then(|c| c.new),
                kind,
                selected,
                number_w,
                theme,
                cx,
            ));
        // Unified rows show the new side's text when there is one, as split mode does.
        let shown = new.clone().or(old.clone()).unwrap_or_else(Cell::empty);
        let code = self.code_element(&shown, kind, theme, &self.diff, file);

        let row_key = self.diff.key(ix).unwrap_or("row").to_string();
        div()
            .id(("diff-row", ix))
            .debug_selector(move || format!("diff-row-{row_key}"))
            .w_full()
            .min_h(theme.line_height)
            .flex()
            .items_stretch()
            .bg(fill)
            .child(gutter)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(CODE_PAD.0))
                    .pr(px(CODE_PAD.1))
                    .child(code),
            )
            .on_mouse_move(cx.listener(move |this, _event, _window, cx| {
                if this.selecting() {
                    let side = if this.diff.row_line(r, Side::New).is_some() {
                        Side::New
                    } else {
                        Side::Old
                    };
                    if let Some(line) = this.diff.row_line(r, side).map(|n| LineRef::new(side, n)) {
                        this.drag_selection(line, cx);
                    }
                }
            }))
            .into_any_element()
    }

    fn split_half(
        &self,
        ix: usize,
        r: Row,
        side: Side,
        cell: Option<Cell>,
        theme: &DiffTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let file = r.file();
        let mut h = div()
            .flex_1()
            .min_w_0()
            .min_h(theme.line_height)
            .flex()
            .items_stretch();
        let Some(cell) = cell else {
            let key = self.diff.key(ix).unwrap_or("row").to_string();
            let empty = format!(
                "diff-{}-{key}",
                if side == Side::Old { "old" } else { "new" }
            );
            return h
                .bg(theme.empty_bg)
                .debug_selector(move || empty)
                .into_any_element();
        };
        let selected = self.diff.row_selected(ix, Some(side));
        let kind = cell.kind;
        let fill = theme.line_fill(kind, selected);
        let n = if side == Side::Old {
            cell.old
        } else {
            cell.new
        };
        let digits = {
            let f = &self.diff.files()[file];
            f.last_line().max(1).ilog10() + 1
        };
        let number_w = theme.number_width(digits.max(3));

        let key = self.diff.key(ix).unwrap_or("row").to_string();
        let half = format!(
            "diff-{}-{}",
            if side == Side::Old { "old" } else { "new" },
            key
        );
        let bar = format!(
            "diff-bar-{}-{key}",
            if side == Side::Old { "old" } else { "new" }
        );
        h = h
            .bg(fill)
            .debug_selector(move || half)
            .child(self.bar(kind, theme, bar))
            .child(self.number_cell(r, side, n, kind, selected, number_w, theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(CODE_PAD.0))
                    .pr(px(CODE_PAD.1))
                    .child(self.code_element(&cell, kind, theme, &self.diff, file)),
            )
            .on_mouse_move(cx.listener(move |this, _event, _window, cx| {
                if this.selecting()
                    && let Some(line) = this.diff.row_line(r, side).map(|n| LineRef::new(side, n))
                {
                    this.drag_selection(line, cx);
                }
            }));
        h.into_any_element()
    }

    fn selecting(&self) -> bool {
        self.diff.selection().is_some()
    }

    fn drag_selection(&mut self, at: LineRef, cx: &mut Context<Self>) {
        self.diff.drag_to(0, at);
        cx.notify();
    }

    fn bar(&self, kind: LineKind, theme: &DiffTheme, id: String) -> AnyElement {
        let b = div()
            .flex_none()
            .w(px(BAR))
            .self_stretch()
            .debug_selector(move || id);
        match theme.tints(kind) {
            Some(t) => b.bg(t.bar).into_any_element(),
            None => b.into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn number_cell(
        &self,
        _r: Row,
        side: Side,
        n: Option<u32>,
        kind: LineKind,
        selected: bool,
        width: Pixels,
        theme: &DiffTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = if selected {
            theme.text
        } else {
            theme.tints(kind).map_or(theme.faint, |t| t.number)
        };
        let label = n.map(|n| n.to_string()).unwrap_or_default();
        let at = n.map(|n| LineRef::new(side, n));
        let cell = div()
            .flex_none()
            .w(width)
            .min_h(theme.line_height)
            .flex()
            .justify_end()
            .items_start()
            .px(px(6.0))
            .text_size(theme.font_size)
            .line_height(theme.line_height)
            .font(theme.code_font())
            .text_color(color)
            .child(label);
        match at {
            Some(at) => cell
                .id(("diff-line-no", at.line as u64 * 2 + side as u64))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus_handle, cx);
                        this.diff.set_shift(event.modifiers.shift);
                        this.diff.press_line(0, at);
                        cx.notify();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.diff.release_line();
                        cx.notify();
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.diff.release_line();
                        cx.notify();
                    }),
                )
                .into_any_element(),
            None => cell.into_any_element(),
        }
    }

    fn code_element(
        &self,
        cell: &Cell,
        kind: LineKind,
        theme: &DiffTheme,
        st: &DiffState,
        file: usize,
    ) -> AnyElement {
        let (side, n) = cell.source();
        let text = if cell.text.is_empty() {
            ""
        } else {
            cell.text.as_str()
        };
        let syntax = self.highlights_for(file, side, n, text);
        let word_bg = theme.tints(kind).map(|t| t.word);
        let runs = build_runs(
            text,
            syntax,
            if kind == LineKind::Context {
                &[]
            } else {
                &cell.changes
            },
            word_bg,
            theme,
            &self.diff,
        );
        let (shown, runs) = expand_tabs(text, runs);
        let shown: SharedString = if shown.is_empty() {
            " ".into()
        } else {
            shown.into()
        };
        let _ = st;
        let mut code = div()
            .w_full()
            .min_w_0()
            .font(theme.code_font())
            .text_size(theme.font_size)
            .line_height(theme.line_height)
            .text_color(theme.text)
            .child(StyledText::new(shown).with_runs(runs));
        if cell.no_newline {
            code = code.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.muted.opacity(0.6))
                    .child("No newline at end of file"),
            );
        }
        code.into_any_element()
    }

    fn highlights_for(
        &self,
        _file: usize,
        side: Side,
        n: u32,
        text: &str,
    ) -> &[(Range<usize>, HighlightStyle)] {
        if text.len() > MAX_HIGHLIGHT_LINE_LENGTH {
            return &[];
        }
        match side {
            Side::Old => self.old_hl.line(n),
            Side::New => self.new_hl.line(n),
        }
    }

    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(&r) = self.diff.rows.get(ix) else {
            return div().into_any_element();
        };
        match r {
            Row::File(_) => div().h(px(0.0)).into_any_element(),
            Row::Note(_) => self.render_note(cx),
            Row::Gap { f, gap } => self.render_gap(f as usize, gap as usize, cx),
            Row::Ctx { .. } | Row::Line { .. } | Row::Pair { .. } => {
                self.render_line_row(ix, r, cx)
            }
        }
    }
}

/// The row ranges of a hunk, for the staging patch builder and the revert path.
fn hunk_ranges(h: &Hunk) -> DiffHunk {
    DiffHunk {
        old_rows: h.first(Side::Old).saturating_sub(1)..h.end(Side::Old).saturating_sub(1),
        new_rows: h.first(Side::New).saturating_sub(1)..h.end(Side::New).saturating_sub(1),
    }
}

/// One side's line in a row: what a cell draws.
#[derive(Clone, Debug)]
struct Cell {
    kind: LineKind,
    old: Option<u32>,
    new: Option<u32>,
    text: String,
    changes: Vec<Range<usize>>,
    no_newline: bool,
}

impl Cell {
    fn empty() -> Cell {
        Cell {
            kind: LineKind::Context,
            old: None,
            new: None,
            text: String::new(),
            changes: Vec::new(),
            no_newline: false,
        }
    }

    /// The side its text is read from, and its number there.
    fn source(&self) -> (Side, u32) {
        match (self.kind, self.old, self.new) {
            (LineKind::Removed, Some(n), _) => (Side::Old, n),
            (_, _, Some(n)) => (Side::New, n),
            (_, Some(n), None) => (Side::Old, n),
            _ => (Side::New, 1),
        }
    }
}

/// Runs for a line: syntax colours under word-change backgrounds, cut where either changes.
fn build_runs(
    text: &str,
    syntax: &[(Range<usize>, HighlightStyle)],
    words: &[Range<usize>],
    word_bg: Option<Hsla>,
    theme: &DiffTheme,
    _st: &DiffState,
) -> Vec<TextRun> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut cuts: Vec<usize> = vec![0, text.len()];
    cuts.extend(
        syntax
            .iter()
            .flat_map(|(r, _)| [r.start.min(text.len()), r.end.min(text.len())]),
    );
    cuts.extend(
        words
            .iter()
            .flat_map(|r| [r.start.min(text.len()), r.end.min(text.len())]),
    );
    cuts.retain(|&c| c <= text.len() && text.is_char_boundary(c));
    cuts.sort_unstable();
    cuts.dedup();

    let base_font = theme.code_font();
    let mut out: Vec<TextRun> = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a >= b {
            continue;
        }
        let style = syntax
            .iter()
            .find(|(r, _)| r.start <= a && a < r.end)
            .map(|(_, s)| s)
            .copied()
            .unwrap_or_default();
        let on = words.iter().any(|r| r.start <= a && a < r.end);
        let bg = word_bg.filter(|_| on).or(style.background_color);
        let font = gpui::Font {
            weight: style.font_weight.unwrap_or(base_font.weight),
            style: style.font_style.unwrap_or(base_font.style),
            ..base_font.clone()
        };
        let run = TextRun {
            len: b - a,
            font,
            color: style.color.unwrap_or(theme.text),
            background_color: bg,
            underline: style.underline,
            strikethrough: style.strikethrough,
        };
        match out.last_mut() {
            Some(last)
                if last.color == run.color
                    && last.background_color == run.background_color
                    && last.font == run.font
                    && last.underline == run.underline =>
            {
                last.len += run.len;
            }
            _ => out.push(run),
        }
    }
    out
}

/// `text` with tabs replaced by spaces up to the next four-column stop, and `runs` moved to
/// match the displayed bytes.
fn expand_tabs(text: &str, mut runs: Vec<TextRun>) -> (String, Vec<TextRun>) {
    if !text.contains('\t') {
        return (text.to_owned(), runs);
    }
    let mut shown = String::with_capacity(text.len() + 8);
    // (byte in `text` after the replacement, bytes added up to and including it)
    let mut shifts: Vec<(usize, usize)> = Vec::new();
    let mut col = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '\t' {
            let pad = 4 - col % 4;
            shown.extend(std::iter::repeat_n(' ', pad));
            col += pad;
            shifts.push((i + 1, shifts.last().map_or(0, |s| s.1) + pad - 1));
        } else {
            shown.push(ch);
            col += 1;
        }
    }
    let shift = |b: usize| match shifts.partition_point(|s| s.0 <= b) {
        0 => b,
        n => b + shifts[n - 1].1,
    };
    let mut out = Vec::with_capacity(runs.len());
    let mut pos = 0usize;
    for run in runs.drain(..) {
        let start = shift(pos);
        pos += run.len;
        let end = shift(pos);
        let mut run = run;
        run.len = end.saturating_sub(start);
        if run.len > 0 {
            out.push(run);
        }
    }
    (shown, out)
}

impl Focusable for DiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.theme = DiffTheme::new(cx);
        self.sync_list();
        let theme = cx.theme().ui;
        let editor_bg = cx.theme().editor_background();
        let accent = theme.accent;
        let accent_fg = theme.accent_foreground;
        let status_deleted = theme.status_deleted;
        let text_muted = theme.text_muted;

        let content: AnyElement = if self.loading {
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
            let count = self.diff.rows.len();
            let list = gpui::list(
                self.list_state.clone(),
                cx.processor(|this, ix: usize, _window, cx| this.render_row(ix, cx)),
            )
            .flex_1()
            .min_h_0();
            let _ = count;
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .bg(editor_bg)
                .child(self.render_file_header(cx))
                .child(list)
                .into_any_element()
        };

        let menu_overlay = self.render_diff_menu_overlay();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(editor_bg)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if this.on_key(event, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.selecting() {
                        this.diff.release_line();
                        cx.notify();
                    }
                }),
            )
            .child(self.render_toolbar(cx))
            .child(content)
            .when_some(menu_overlay, |this, overlay| this.child(overlay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> DiffTheme {
        DiffTheme {
            font_family: "mono".into(),
            font_size: px(13.),
            line_height: px(20.),
            bg: Hsla::white(),
            editor_bg: Hsla::white(),
            text: Hsla::black(),
            muted: Hsla::black(),
            faint: Hsla::black(),
            border: Hsla::black(),
            header_bg: Hsla::white(),
            hover: Hsla::white(),
            add: crate::editor::diff::Tints {
                line: Hsla::white(),
                gutter: Hsla::white(),
                word: Hsla::white(),
                number: Hsla::black(),
                bar: Hsla::black(),
            },
            del: crate::editor::diff::Tints {
                line: Hsla::white(),
                gutter: Hsla::white(),
                word: Hsla::white(),
                number: Hsla::black(),
                bar: Hsla::black(),
            },
            gap_bg: Hsla::white(),
            empty_bg: Hsla::white(),
            selected: Hsla::white(),
            selected_gutter: Hsla::white(),
            selection: Hsla::white(),
            accent: Hsla::black(),
            on_accent: Hsla::white(),
            syntax: crate::editor::syntax::HighlightTheme {
                name: "test".into(),
                appearance: crate::theme::ThemeAppearance::Dark,
                style: crate::editor::syntax::HighlightThemeStyle::default(),
            },
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn runs_cover_the_text_and_carry_word_backgrounds() {
        let t = theme();
        let green = gpui::rgb(0x00ff00).into();
        let syntax = vec![(
            0..3,
            HighlightStyle {
                color: Some(gpui::rgb(0xff0000).into()),
                ..Default::default()
            },
        )];
        let changes = [4..5];
        let runs = build_runs(
            "let x = 1;",
            &syntax,
            &changes,
            Some(green),
            &t,
            &DiffState::default(),
        );
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, "let x = 1;".len());
        assert_eq!(runs[0].color, gpui::rgb(0xff0000).into());
        assert!(runs.iter().any(|r| r.background_color == Some(green)));
    }

    #[test]
    fn tabs_expand_to_four_columns_and_runs_follow() {
        let t = theme();
        let (shown, runs) = expand_tabs(
            "\tx",
            build_runs("\tx", &[], &[], None, &t, &DiffState::default()),
        );
        assert_eq!(shown, "    x");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), shown.len());
        let (shown, _) = expand_tabs("a\tb", Vec::new());
        assert_eq!(shown, "a   b");
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::editor::git_repository::GitRepository;
    use gpui::{px, size};

    struct Host {
        diff: Entity<DiffView>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.diff.clone())
        }
    }

    fn init(cx: &mut App) {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
    }

    /// A temp repo with `head` committed and `worktree` written, so the view can load it.
    fn repo_with(head: &str, worktree: &str) -> (std::path::PathBuf, GitRepository) {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!(
            "deepterm-diff-view-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let _ = Command::new("git").args(args).current_dir(&dir).output();
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("a.rs"), head).unwrap();
        run(&["add", "a.rs"]);
        run(&["commit", "-m", "init", "--quiet"]);
        std::fs::write(dir.join("a.rs"), worktree).unwrap();
        let repo = GitRepository::find(&dir, true, None)
            .unwrap()
            .expect("repo");
        (dir, repo)
    }

    fn frame(cx: &mut gpui::VisualTestContext) {
        for _ in 0..10 {
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
    }

    #[gpui::test]
    fn the_view_loads_a_repo_file_and_draws_rows(cx: &mut gpui::TestAppContext) {
        let old = "fn main() {\n    let a = 1;\n}\n";
        let new = "fn main() {\n    let a = 2;\n}\n";
        cx.update(init);
        let (dir, repo) = repo_with(old, new);
        let (host, cx) = cx.add_window_view(|window, cx| Host {
            diff: cx.new(|cx| {
                DiffView::new_loading(
                    "a.rs".into(),
                    repo,
                    false,
                    WeakEntity::new_invalid(),
                    window,
                    cx,
                )
            }),
        });
        cx.simulate_resize(size(px(900.), px(600.)));
        let diff = host.read_with(cx, |h, _| h.diff.clone());
        for _ in 0..30 {
            frame(cx);
            if diff.read_with(cx, |v, _| !v.loading) {
                break;
            }
        }
        frame(cx);
        assert!(!diff.read_with(cx, |v, _| v.loading), "loaded");
        let (rows, mode) = diff.read_with(cx, |v, _| (v.diff.rows.len(), v.effective_mode()));
        assert!(rows > 3, "{rows} rows");
        assert_eq!(mode, DiffMode::Unified);
        let header = cx.debug_bounds("diff-file-header").expect("file header");
        assert_eq!(header.size.height, px(HEADER_H));
        assert_eq!(header.size.width, px(900.));
        let line_h = diff.read_with(cx, |v, _| v.theme.line_height);
        let first = cx.debug_bounds("diff-row-df0.c1").expect("first line row");
        assert_eq!(first.size.height, line_h);
        let bar = cx
            .debug_bounds("diff-bar-old-df0.o2")
            .expect("removed line bar");
        assert_eq!(bar.size.width, px(BAR));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn split_mode_draws_two_equal_halves_and_a_divider(cx: &mut gpui::TestAppContext) {
        let old = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
        let new = "fn main() {\n    let a = 9;\n}\n";
        cx.update(init);
        let (dir, repo) = repo_with(old, new);
        let (host, cx) = cx.add_window_view(|window, cx| Host {
            diff: cx.new(|cx| {
                DiffView::new_loading(
                    "a.rs".into(),
                    repo,
                    false,
                    WeakEntity::new_invalid(),
                    window,
                    cx,
                )
            }),
        });
        cx.simulate_resize(size(px(900.), px(600.)));
        let diff = host.read_with(cx, |h, _| h.diff.clone());
        for _ in 0..30 {
            frame(cx);
            if diff.read_with(cx, |v, _| !v.loading) {
                break;
            }
        }
        diff.update(cx, |v, cx| v.set_preference(LayoutPreference::Split, cx));
        frame(cx);
        assert_eq!(
            diff.read_with(cx, |v, _| v.effective_mode()),
            DiffMode::Split
        );
        let left = cx.debug_bounds("diff-old-df0.o2").expect("old half");
        let right = cx.debug_bounds("diff-new-df0.o2").expect("new half");
        assert!((left.size.width - px(449.5)).abs() <= px(1.5), "{:?}", left);
        assert!(
            (right.size.width - px(449.5)).abs() <= px(1.5),
            "{:?}",
            right
        );
        assert!(right.origin.x > left.origin.x);
        // A row whose old side ran out is filled with the empty wash.
        let empty = cx.debug_bounds("diff-old-df0.o3").expect("empty old half");
        assert_eq!(empty.size.width, right.size.width);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn clicking_a_folded_range_reveals_twenty_lines(cx: &mut gpui::TestAppContext) {
        let old: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 50\n", "line fifty\n");
        cx.update(init);
        let (dir, repo) = repo_with(&old, &new);
        let (host, cx) = cx.add_window_view(|window, cx| Host {
            diff: cx.new(|cx| {
                DiffView::new_loading(
                    "a.rs".into(),
                    repo,
                    false,
                    WeakEntity::new_invalid(),
                    window,
                    cx,
                )
            }),
        });
        cx.simulate_resize(size(px(900.), px(700.)));
        let diff = host.read_with(cx, |h, _| h.diff.clone());
        for _ in 0..30 {
            frame(cx);
            if diff.read_with(cx, |v, _| !v.loading) {
                break;
            }
        }
        frame(cx);
        let (hidden, rows) = diff.read_with(cx, |v, _| (v.diff.hidden(0, 0), v.diff.rows.len()));
        assert_eq!(hidden, 46, "the range before the hunk folds");
        let gap = cx.debug_bounds("diff-gap-0").expect("folded range row");
        assert_eq!(gap.size.height, px(GAP_H));
        // Pressing the expand button reveals twenty lines.
        let button = cx
            .debug_bounds("diff-expand-0-down")
            .expect("expand button");
        let point = button.center();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_event(gpui::MouseDownEvent {
            button: MouseButton::Left,
            position: point,
            modifiers: Default::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: MouseButton::Left,
            position: point,
            modifiers: Default::default(),
            click_count: 1,
        });
        frame(cx);
        let (hidden, new_rows) =
            diff.read_with(cx, |v, _| (v.diff.hidden(0, 0), v.diff.rows.len()));
        assert_eq!(hidden, 26, "one step reveals twenty of the 46 hidden lines");
        assert!(new_rows > rows, "{new_rows} > {rows}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Golden-image helpers: render the view headlessly and compare a few pixels against the
/// colours the theme's tints should paint.
#[cfg(test)]
mod visual_tests {
    use super::*;
    use crate::editor::git_repository::GitRepository;
    use gpui::{px, size};

    struct Host {
        diff: Entity<DiffView>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.diff.clone())
        }
    }

    fn init(cx: &mut App) {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
    }

    fn repo_with(head: &str, worktree: &str) -> (std::path::PathBuf, GitRepository) {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!(
            "deepterm-diff-visual-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let _ = Command::new("git").args(args).current_dir(&dir).output();
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("a.rs"), head).unwrap();
        run(&["add", "a.rs"]);
        run(&["commit", "-m", "init", "--quiet"]);
        std::fs::write(dir.join("a.rs"), worktree).unwrap();
        let repo = GitRepository::find(&dir, true, None)
            .unwrap()
            .expect("repo");
        (dir, repo)
    }

    #[cfg(not(target_os = "macos"))]
    #[gpui::test]
    fn renders_a_split_diff_to_an_image(cx: &mut gpui::TestAppContext) {
        // Headless rasterization needs a platform renderer; only macOS' test platform has one.
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn paints_the_expected_row_colours(cx: &mut gpui::TestAppContext) {
        let old = "fn main() {\n    let a = 1;\n}\n";
        let new = "fn main() {\n    let a = 2;\n}\n";
        cx.update(init);
        let (dir, repo) = repo_with(old, new);
        let (host, cx) = cx.add_window_view(|window, cx| Host {
            diff: cx.new(|cx| {
                DiffView::new_loading(
                    "a.rs".into(),
                    repo,
                    false,
                    WeakEntity::new_invalid(),
                    window,
                    cx,
                )
            }),
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        let diff = host.read_with(cx, |h, _| h.diff.clone());
        for _ in 0..30 {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            if diff.read_with(cx, |v, _| !v.loading) {
                break;
            }
        }
        diff.update(cx, |v, cx| v.set_preference(LayoutPreference::Split, cx));
        for _ in 0..5 {
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        let (add, del) = cx.update(|_, cx| {
            let t = DiffTheme::new(cx);
            (t.add.line, t.del.line)
        });
        // The scene holds a solid quad per painted row: find the ones covering the removed
        // and added lines and check their colours.
        let removed = cx.debug_bounds("diff-old-df0.o2").expect("removed half");
        let added = cx.debug_bounds("diff-new-df0.o2").expect("added half");
        let quads = cx.update(|window, _| window.painted_quads());
        let covers = |q: &gpui::Quad, b: &gpui::Bounds<Pixels>| {
            let x = q.bounds.origin.x.0 / 2.0;
            let y = q.bounds.origin.y.0 / 2.0;
            let w = q.bounds.size.width.0 / 2.0;
            let h = q.bounds.size.height.0 / 2.0;
            let (bx, by) = (b.origin.x.as_f32(), b.origin.y.as_f32());
            let (bw, bh) = (b.size.width.as_f32(), b.size.height.as_f32());
            x <= bx + 1.0 && (x + w) >= bx + bw - 1.0 && y <= by + 1.0 && (y + h) >= by + bh - 1.0
        };
        let colour_at = |b: &gpui::Bounds<Pixels>| {
            quads
                .iter()
                .filter(|q| covers(q, b))
                .filter_map(|q| q.background.as_solid())
                .next_back()
        };
        let close = |got: Hsla, want: Hsla| {
            let (got, want) = (gpui::Rgba::from(got), gpui::Rgba::from(want));
            (got.r - want.r).abs() < 0.05
                && (got.g - want.g).abs() < 0.05
                && (got.b - want.b).abs() < 0.05
        };
        let got_del = colour_at(&removed).expect("a fill on the removed row");
        let got_add = colour_at(&added).expect("a fill on the added row");
        assert!(close(got_del, del), "removed row: {got_del:?} vs {del:?}");
        assert!(close(got_add, add), "added row: {got_add:?} vs {add:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
