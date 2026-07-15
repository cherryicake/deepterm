use gpui::prelude::*;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton, ParentElement,
    Pixels, Render, Stateful, Styled, Subscription, UniformListScrollHandle, Window, div, px, svg,
    uniform_list,
};

use crate::assets::Icon;
use crate::editor::git_repository::GitRepository;
use crate::editor::git_status::{DiffStat, FileStatus, Section, StatusCode, TrackedStatus};
use crate::file_manager::watcher::{FsChangeEvent, WatcherContext};
use crate::theme::ActiveTheme;

const LIST_ROW_HEIGHT: f32 = 28.0;

/// Git panel events
pub enum GitPanelEvent {
    OpenDiff { path: String, diff_content: String },
    StageFile { path: String },
    UnstageFile { path: String },
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
    pub diff_stat: Option<DiffStat>,
}

impl GitFileEntry {
    pub fn status_icon(&self) -> Icon {
        if self.status.is_conflicted() {
            Icon::Warning
        } else if self.status.is_created() {
            Icon::SquarePlus
        } else if self.status.is_deleted() {
            Icon::SquareMinus
        } else {
            Icon::SquareDot
        }
    }

    pub fn status_color(&self, theme: &crate::theme::UiTheme) -> gpui::Hsla {
        if self.status.is_conflicted() {
            theme.status_conflict
        } else if self.status.is_created() {
            theme.status_added
        } else if self.status.is_deleted() {
            theme.status_deleted
        } else {
            theme.status_modified
        }
    }

    pub fn status_label(&self) -> &'static str {
        if self.status.is_conflicted() {
            "!"
        } else if self.status.is_created() {
            "+"
        } else if self.status.is_deleted() {
            "-"
        } else {
            "M"
        }
    }

    pub fn should_stage(&self) -> bool {
        matches!(
            self.status,
            FileStatus::Tracked(TrackedStatus {
                worktree_status: StatusCode::Modified,
                ..
            }) | FileStatus::Tracked(TrackedStatus {
                worktree_status: StatusCode::Deleted,
                ..
            }) | FileStatus::Tracked(TrackedStatus {
                worktree_status: StatusCode::Added,
                ..
            })
        )
    }
}

/// Git panel component
pub struct GitPanel {
    focus_handle: FocusHandle,
    workspace_path: Option<std::path::PathBuf>,
    repo: Option<GitRepository>,
    entries: Vec<GitListEntry>,
    file_entries: Vec<GitFileEntry>,
    scroll_handle: UniformListScrollHandle,
    loading: bool,
    hovered_index: Option<usize>,
    watcher_subscription: Option<Subscription>,
}

impl GitPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace_path: None,
            repo: None,
            entries: Vec::new(),
            file_entries: Vec::new(),
            scroll_handle: UniformListScrollHandle::new(),
            loading: false,
            hovered_index: None,
            watcher_subscription: None,
        }
    }

    /// Set the workspace path and refresh git status
    pub fn set_workspace_path(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        // Skip if same path
        if self.workspace_path.as_ref() == Some(&path) {
            return;
        }

        // Unwatch old path if exists
        if let Some(ref old_path) = self.workspace_path {
            self.watcher_subscription.take();
            cx.unwatch_recursive_path(old_path);
        }

        self.workspace_path = Some(path.clone());
        self.repo = GitRepository::find(&path);

        // Watch the workspace directory recursively
        cx.watch_recursive_path(&path);

        // Set up file watching for auto-refresh
        // Use a separate subscription that checks if we're already loading
        let watcher = cx.fs_watcher();
        let weak = cx.entity().downgrade();
        self.watcher_subscription = Some(cx.subscribe(
            &watcher,
            move |_, _, _event: &FsChangeEvent, cx| {
                // Use cx.defer to avoid re-entrancy issues
                let weak = weak.clone();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if !this.loading {
                            this.refresh(cx);
                        }
                    });
                });
            },
        ));

        self.refresh(cx);
    }

    /// Refresh git status
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            self.entries.clear();
            self.file_entries.clear();
            cx.notify();
            return;
        };

        self.loading = true;
        cx.notify();

        let repo_root = repo.root().to_path_buf();
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let status_result = {
                let repo = GitRepository {
                    root: repo_root.clone(),
                };
                cx.background_executor()
                    .spawn(async move { repo.status() })
                    .await
            };

            let diff_stat_result = {
                let repo = GitRepository {
                    root: repo_root.clone(),
                };
                cx.background_executor()
                    .spawn(async move { repo.diff_stat() })
                    .await
            };

            let _ = weak.update(cx, |this, cx| {
                this.loading = false;

                match status_result {
                    Ok(status_entries) => {
                        let diff_stats = diff_stat_result.unwrap_or_default();
                        let diff_stat_map: std::collections::HashMap<String, DiffStat> =
                            diff_stats.into_iter().collect();

                        let mut file_entries: Vec<GitFileEntry> = status_entries
                            .into_iter()
                            .map(|entry| {
                                let diff_stat = diff_stat_map.get(&entry.path).copied();
                                GitFileEntry {
                                    path: entry.path,
                                    status: entry.status,
                                    section: entry.section,
                                    diff_stat,
                                }
                            })
                            .collect();

                        // Sort by section order (Conflict < Tracked < New), then by path
                        file_entries.sort_by(|a, b| {
                            let section_order = |s: &Section| match s {
                                Section::Conflict => 0,
                                Section::Tracked => 1,
                                Section::New => 2,
                            };
                            section_order(&a.section)
                                .cmp(&section_order(&b.section))
                                .then_with(|| a.path.cmp(&b.path))
                        });

                        this.file_entries = file_entries;

                        // Build list entries with section headers
                        this.entries.clear();
                        let mut current_section: Option<Section> = None;
                        for entry in &this.file_entries {
                            if current_section != Some(entry.section) {
                                current_section = Some(entry.section);
                                this.entries.push(GitListEntry::Header(entry.section));
                            }
                            this.entries.push(GitListEntry::File(entry.clone()));
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to get git status: {e}");
                        this.entries.clear();
                        this.file_entries.clear();
                    }
                }

                cx.notify();
            });
        })
        .detach();
    }

    /// Open diff for a file
    fn open_diff(&self, entry: &GitFileEntry, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };

        let path = entry.path.clone();
        let path_clone = path.clone();
        let repo_root = repo.root().to_path_buf();
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let diff_result = {
                let repo = GitRepository {
                    root: repo_root.clone(),
                };
                cx.background_executor()
                    .spawn(async move { repo.diff_file(&path_clone) })
                    .await
            };

            if let Ok(diff_content) = diff_result {
                let _ = weak.update(cx, |_, cx| {
                    cx.emit(GitPanelEvent::OpenDiff { path, diff_content });
                });
            }
        })
        .detach();
    }

    /// Stage a file
    fn stage_file(&self, path: &str, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };

        let path = path.to_string();
        let repo_root = repo.root().to_path_buf();
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let result = {
                let repo = GitRepository {
                    root: repo_root.clone(),
                };
                let path_ref = path.clone();
                cx.background_executor()
                    .spawn(async move { repo.stage(&[&path_ref]) })
                    .await
            };

            if result.is_ok() {
                let _ = weak.update(cx, |this, cx| {
                    cx.emit(GitPanelEvent::StageFile { path });
                    this.refresh(cx);
                });
            }
        })
        .detach();
    }

    /// Unstage a file
    fn unstage_file(&self, path: &str, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };

        let path = path.to_string();
        let repo_root = repo.root().to_path_buf();
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let result = {
                let repo = GitRepository {
                    root: repo_root.clone(),
                };
                let path_ref = path.clone();
                cx.background_executor()
                    .spawn(async move { repo.unstage(&[&path_ref]) })
                    .await
            };

            if result.is_ok() {
                let _ = weak.update(cx, |this, cx| {
                    cx.emit(GitPanelEvent::UnstageFile { path });
                    this.refresh(cx);
                });
            }
        })
        .detach();
    }

    fn render_header(&self, section: Section, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();

        let (title, color) = match section {
            Section::Conflict => ("Conflicts", theme.status_conflict),
            Section::Tracked => ("Tracked", theme.text),
            Section::New => ("Untracked", theme.status_added),
        };

        let count = self
            .file_entries
            .iter()
            .filter(|e| e.section == section)
            .count();

        div()
            .id(format!("git-header-{section:?}"))
            .w_full()
            .h(px(28.0))
            .flex()
            .items_center()
            .px(px(8.0))
            .gap(px(4.0))
            .text_sm()
            .text_color(color)
            .child(format!("{title} ({count})"))
    }

    fn render_file_entry(
        &self,
        entry: &GitFileEntry,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let theme = cx.theme().ui;
        let is_hovered = self.hovered_index == Some(index);
        let status_color = entry.status_color(&theme);
        let status_icon = entry.status_icon();
        let path = entry.path.clone();
        let path_clone = path.clone();
        let entry_clone = entry.clone();

        let row_background = if is_hovered {
            theme.tab_hover_background
        } else {
            theme.elevated_background.opacity(0.0)
        };

        div()
            .id(format!("git-entry-{index}"))
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .px(px(4.0))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                if *hovered {
                    this.hovered_index = Some(index);
                } else if this.hovered_index == Some(index) {
                    this.hovered_index = None;
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    this.open_diff(&entry_clone, cx);
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
                        // Status icon
                        svg()
                            .size(px(14.0))
                            .text_color(status_color)
                            .path(status_icon.path()),
                    )
                    .child(
                        // File name
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .truncate()
                            .text_sm()
                            .text_color(theme.text)
                            .child(path_clone),
                    )
                    .when_some(entry.diff_stat.as_ref(), |this, stat| {
                        this.child(
                            div()
                                .flex_none()
                                .flex()
                                .gap(px(2.0))
                                .text_xs()
                                .child(
                                    div()
                                        .text_color(theme.status_added)
                                        .child(format!("+{}", stat.added)),
                                )
                                .child(
                                    div()
                                        .text_color(theme.status_deleted)
                                        .child(format!("-{}", stat.deleted)),
                                ),
                        )
                    })
                    .child({
                        // Stage/Unstage button
                        let path_for_action = path.clone();
                        let entry_for_action = entry.clone();
                        div()
                            .flex_none()
                            .w(px(20.0))
                            .h(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .hover(|style| style.bg(theme.tab_hover_background))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _window, cx| {
                                    if entry_for_action.should_stage() {
                                        this.stage_file(&path_for_action, cx);
                                    } else {
                                        this.unstage_file(&path_for_action, cx);
                                    }
                                }),
                            )
                            .child(svg().size(px(12.0)).text_color(theme.text_subtle).path(
                                if entry.should_stage() {
                                    Icon::SquarePlus.path()
                                } else {
                                    Icon::SquareMinus.path()
                                },
                            ))
                    }),
            )
    }
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

impl Focusable for GitPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();

        if self.repo.is_none() {
            return div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_subtle)
                .child("Not a git repository")
                .into_any_element();
        }

        if self.loading && self.entries.is_empty() {
            return div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_subtle)
                .child("Loading...")
                .into_any_element();
        }

        if self.entries.is_empty() {
            return div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_subtle)
                .child("No changes")
                .into_any_element();
        }

        let entry_count = self.entries.len();
        let scroll_handle = self.scroll_handle.clone();

        let list = uniform_list(
            "git-panel-list",
            entry_count,
            cx.processor(|this, range: std::ops::Range<usize>, _window, _cx| {
                range
                    .filter_map(|ix| this.entries.get(ix).cloned())
                    .enumerate()
                    .map(|(i, entry)| match entry {
                        GitListEntry::Header(section) => {
                            this.render_header(section, _cx).into_any_element()
                        }
                        GitListEntry::File(file_entry) => this
                            .render_file_entry(&file_entry, i, _cx)
                            .into_any_element(),
                    })
                    .collect()
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
        .track_scroll(&scroll_handle);

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .child(list.flex_1().min_h_0())
            .into_any_element()
    }
}
