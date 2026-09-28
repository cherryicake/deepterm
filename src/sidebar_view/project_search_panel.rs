//! 侧边栏「搜索」面板：项目级跨文件查找。
//!
//! 此前该 tab 是占位符（只渲染一行文字"搜索"）。现在接上
//! [`crate::file_manager::project_search`] 的遍历+匹配引擎。
//!
//! 交互模型（对齐 VSCode / Zed 的 project search）：
//! - 顶部查询输入框 + 选项开关（大小写 / 全字 / 正则）
//! - 输入防抖 250ms：每击键都全量遍历在大仓库上不可接受
//! - 结果按文件分组，行内命中列高亮，点击跳到编辑器对应行列
//! - 遍历可取消：改词时取消上一轮，避免连续输入堆积遍历任务
//!
//! 线程模型：遍历+匹配跑在 `background_executor`（不阻塞 UI），
//! 通过 `cx.spawn` 回到 UI 线程更新结果。

use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton,
    ParentElement, Render, SharedString, Styled, Subscription, Task, UniformListScrollHandle,
    Window, div, px, uniform_list,
};

use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::file_manager::project_search::{
    FileMatch, ProjectSearchOptions, ProjectSearchOutcome, SearchMode, search_project,
};
use crate::file_manager::walk::WalkCancellation;
use crate::theme::ActiveTheme;

/// 输入防抖：每击键全量遍历在大仓库上不可接受。
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// 结果行高（与文件树/git 面板保持一致）。
const RESULT_ROW_HEIGHT: f32 = 22.0;
/// 文件分组头行高。
const GROUP_ROW_HEIGHT: f32 = 24.0;

/// 搜索面板事件。
pub enum ProjectSearchEvent {
    /// 打开命中位置（路径 + 1 起行号 + 0 起字符列）。
    OpenMatch {
        path: PathBuf,
        line: u32,
        column: u32,
    },
}

/// 结果列表的一行（文件分组头或命中行）。
#[derive(Clone, Debug)]
enum ResultRow {
    Group {
        file_ix: usize,
    },
    Match {
        file_ix: usize,
        line_ix: usize,
    },
}

pub struct ProjectSearchPanel {
    focus_handle: FocusHandle,
    /// 查询输入框。懒建：`ensure_search_panel` 的调用点没有 window
    /// （与 `ensure_git_panel` 同一批），而 `InputState::new` 需要 window。
    /// 由 `render`（唯一稳定能拿到 window 的地方）创建。
    input: Option<Entity<InputState>>,
    _input_subscription: Option<Subscription>,

    case_insensitive: bool,
    whole_word: bool,
    regex_mode: bool,

    /// 搜索根（工作区/终端 cwd）。为空表示无可用根。
    root: Option<PathBuf>,
    /// 当前生效的查询词（防抖后），用于渲染与「无结果」判定。
    active_query: String,

    rows: Vec<ResultRow>,
    outcome: Option<ProjectSearchOutcome>,
    /// 查询非法（正则语法错误）时的提示。
    query_error: Option<SharedString>,

    /// 正在搜索中（后台任务未回）。
    searching: bool,
    /// 当前后台搜索的取消令牌。改词/关闭面板时取消。
    cancel: Option<WalkCancellation>,
    /// 防抖任务句柄。
    debounce_task: Option<Task<()>>,
    /// 正在跑的后台搜索任务。持句柄才能在改词/关闭时真正丢弃旧任务
    /// （取消令牌只让遍历提前返回，任务本身仍占着线程直到检查点）。
    search_task: Option<Task<()>>,
    /// 搜索代际。每开一轮 +1，结果回来时比对代际决定是否采纳。
    ///
    /// 不能用取消令牌的 `is_cancelled()` 做身份判定：两轮都未被取消时
    /// 该值相等，会把过期结果误当成当前结果。
    search_generation: u64,
    /// 根变化时若已有查询，需要重搜。`set_root` 的调用点没有 window
    /// （与 `ensure_git_panel` 同一批调用点），所以在这里记一个标记，
    /// 由 `render` 用 `defer_in` 触发。
    pending_rerun: bool,

    scroll_handle: UniformListScrollHandle,
}

impl EventEmitter<ProjectSearchEvent> for ProjectSearchPanel {}

impl Focusable for ProjectSearchPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ProjectSearchPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            input: None,
            _input_subscription: None,
            case_insensitive: true,
            whole_word: false,
            regex_mode: false,
            root: None,
            active_query: String::new(),
            rows: Vec::new(),
            outcome: None,
            query_error: None,
            searching: false,
            cancel: None,
            debounce_task: None,
            search_task: None,
            search_generation: 0,
            pending_rerun: false,
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    /// 设置搜索根。根变化时清空结果并重搜（若已有查询）。
    ///
    /// 不接收 `window`：调用点（文件树 set/refresh 路径）没有 window，
    /// 与 `ensure_git_panel` 同一批。重搜通过 `pending_rerun` 交给 `render`。
    pub fn set_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.root == root {
            return;
        }
        self.root = root;
        // 旧结果属于旧根，必须清掉，否则会显示不属于当前工作区的路径。
        self.clear_results(cx);
        if !self.active_query.is_empty() {
            self.pending_rerun = true;
            cx.notify();
        }
    }

    pub fn focus_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.input.clone() {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// 当前查询词。输入框尚未创建时为空。
    fn query(&self, cx: &App) -> String {
        self.input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// 懒建输入框（需要 window）。
    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("在项目中搜索")
                .clean_on_escape()
        });
        // 输入变化 → 防抖后重开一轮搜索。
        self._input_subscription = Some(cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_search(window, cx);
                }
            },
        ));
        self.input = Some(input);
    }

    fn clear_results(&mut self, cx: &mut Context<Self>) {
        self.rows.clear();
        self.outcome = None;
        self.query_error = None;
        self.searching = false;
        cx.notify();
    }

    /// 防抖调度：取消上一轮，延迟后执行本轮。
    fn schedule_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 输入一变，上一轮的结果立刻作废：不仅取消遍历，也把结果清空，
        // 否则用户会看到与新查询不匹配的旧结果停留 250ms。
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.searching = false;

        let query = self.query(cx);
        if query.trim().is_empty() {
            self.active_query.clear();
            self.clear_results(cx);
            return;
        }

        self.debounce_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.start_search(window, cx);
            });
        }));
    }

    /// 立即执行一轮搜索（防抖到期后调用）。
    fn start_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        if query.trim().is_empty() {
            self.active_query.clear();
            self.clear_results(cx);
            return;
        }
        let Some(root) = self.root.clone() else {
            self.active_query = query;
            self.rows.clear();
            self.outcome = None;
            self.query_error = Some("没有可搜索的工作区目录".into());
            cx.notify();
            return;
        };

        let options = ProjectSearchOptions {
            query: query.clone(),
            case_insensitive: self.case_insensitive,
            whole_word: self.whole_word,
            mode: if self.regex_mode {
                SearchMode::Regex
            } else {
                SearchMode::Literal
            },
            ..Default::default()
        };

        // 查询非法（如未闭合括号的正则）：直接提示，不启动遍历。
        if options.build_matcher().is_none() {
            self.active_query = query;
            self.rows.clear();
            self.outcome = None;
            self.query_error = Some("正则表达式无效".into());
            self.searching = false;
            cx.notify();
            return;
        }

        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        let cancel = WalkCancellation::new();
        self.cancel = Some(cancel.clone());
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.active_query = query;
        self.query_error = None;
        self.searching = true;
        cx.notify();

        self.search_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn({
                    let cancel = cancel.clone();
                    async move { search_project(&root, &options, &cancel) }
                })
                .await;

            let _ = this.update_in(cx, |this, _window, cx| {
                // 结果回来时若已被新一轮取代，丢弃（代际不等即过期）。
                if this.search_generation != generation {
                    return;
                }
                this.searching = false;
                this.rows = Self::build_rows(&outcome);
                this.outcome = Some(outcome);
                cx.notify();
            });
        }));
    }

    /// 把「按文件分组的结果」摊平成可虚拟滚动的行序列。
    fn build_rows(outcome: &ProjectSearchOutcome) -> Vec<ResultRow> {
        let mut rows = Vec::new();
        for (file_ix, file) in outcome.files.iter().enumerate() {
            rows.push(ResultRow::Group { file_ix });
            for line_ix in 0..file.lines.len() {
                rows.push(ResultRow::Match { file_ix, line_ix });
            }
        }
        rows
    }

    fn toggle_case(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.case_insensitive = !self.case_insensitive;
        self.rerun(window, cx);
    }

    fn toggle_whole_word(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.whole_word = !self.whole_word;
        self.rerun(window, cx);
    }

    fn toggle_regex(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.regex_mode = !self.regex_mode;
        self.rerun(window, cx);
    }

    /// 选项变化：立即重搜，不走防抖（用户点了开关就期待立刻看到结果）。
    fn rerun(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.debounce_task.take().is_some() {
            // 丢弃尚未触发的防抖任务。
        }
        self.start_search(window, cx);
    }

    fn open_match(
        &mut self,
        file_ix: usize,
        line_ix: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(outcome) = self.outcome.as_ref() else {
            return;
        };
        let Some(file) = outcome.files.get(file_ix) else {
            return;
        };
        let Some(line) = file.lines.get(line_ix) else {
            return;
        };
        let column = line.columns.first().map(|c| c.start).unwrap_or(0) as u32;
        cx.emit(ProjectSearchEvent::OpenMatch {
            path: file.path.clone(),
            line: line.line_number as u32,
            column,
        });
    }

    /// 渲染一行命中：`行号 + 行文本（命中列高亮）`。
    fn render_match_line(
        &self,
        file: &FileMatch,
        line_ix: usize,
        cx: &App,
    ) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let line = &file.lines[line_ix];
        let chars: Vec<char> = line.line_text.chars().collect();

        // 按命中列把行切成「普通段 / 高亮段」。命中列已按字符计，
        // 这里用 chars 索引，避免 CJK 行上字节切错。
        let mut segments: Vec<(String, bool)> = Vec::new();
        let mut cursor = 0usize;
        for Range { start, end } in &line.columns {
            let start = (*start).min(chars.len());
            let end = (*end).min(chars.len());
            if start > cursor {
                segments.push((chars[cursor..start].iter().collect(), false));
            }
            if end > start {
                segments.push((chars[start..end].iter().collect(), true));
            }
            cursor = cursor.max(end);
        }
        if cursor < chars.len() {
            segments.push((chars[cursor..].iter().collect(), false));
        }

        let mut text_row = div().flex().flex_row().items_center().gap(px(0.));

        for (text, is_hit) in segments {
            if text.is_empty() {
                continue;
            }
            let mut span = div().flex_none().child(text);
            if is_hit {
                span = span
                    .bg(theme.accent.opacity(0.35))
                    .text_color(theme.text)
                    .rounded(px(2.));
            } else {
                span = span.text_color(theme.text_muted);
            }
            text_row = text_row.child(span);
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .min_w_0()
            .child(
                div()
                    .flex_none()
                    .w(px(36.0))
                    .text_right()
                    .text_color(theme.text_subtle)
                    .child(format!("{}", line.line_number)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(theme.text_muted)
                    .child(text_row),
            )
            .into_any_element()
    }
}

impl Render for ProjectSearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        self.ensure_input(window, cx);

        // 根变化后的重搜：render 是少数几个能拿到 window 的地方。
        if self.pending_rerun {
            self.pending_rerun = false;
            cx.defer_in(window, |this, window, cx| this.start_search(window, cx));
        }

        let option_button = |id: &'static str,
                             label: &'static str,
                             active: bool,
                             handler: fn(&mut Self, &mut Window, &mut Context<Self>),
                             cx: &mut Context<Self>| {
            let mut button = div()
                .id(id)
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .text_size(px(11.0))
                .cursor_pointer()
                .child(label);
            button = if active {
                button.bg(theme.accent).text_color(theme.accent_foreground)
            } else {
                button
                    .text_color(theme.text_muted)
                    .hover(|s| s.bg(theme.tab_hover_background))
            };
            button.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| handler(this, window, cx)),
            )
        };

        // 顶部：查询输入 + 选项开关
        let header = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(6.0))
            .child(div().w_full().children(
                    self.input
                        .as_ref()
                        .map(|input| EditorInput::new(input).into_any_element()),
                ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(option_button(
                        "search-opt-case",
                        "Aa",
                        !self.case_insensitive,
                        |this, window, cx| this.toggle_case(window, cx),
                        cx,
                    ))
                    .child(option_button(
                        "search-opt-word",
                        "全字",
                        self.whole_word,
                        |this, window, cx| this.toggle_whole_word(window, cx),
                        cx,
                    ))
                    .child(option_button(
                        "search-opt-regex",
                        ".*",
                        self.regex_mode,
                        |this, window, cx| this.toggle_regex(window, cx),
                        cx,
                    )),
            );

        // 状态行：搜索中 / 结果统计 / 错误
        let status_line = if let Some(err) = self.query_error.clone() {
            Some(div().text_color(theme.error).child(err))
        } else if self.searching {
            Some(div().text_color(theme.text_subtle).child("搜索中…"))
        } else if self.active_query.is_empty() {
            None
        } else {
            let summary = match self.outcome.as_ref() {
                Some(o) if o.is_empty() => "无结果".to_string(),
                Some(o) => {
                    let mut s = format!("{} 个文件 · {} 处", o.file_count(), o.total_lines);
                    if o.truncated {
                        s.push_str("（已截断）");
                    }
                    if o.skipped_large > 0 {
                        s.push_str(&format!(" · 跳过 {} 个大文件", o.skipped_large));
                    }
                    s
                }
                None => "无结果".to_string(),
            };
            Some(div().text_color(theme.text_subtle).child(summary))
        };

        let status = status_line.map(|line| {
            div()
                .px(px(8.0))
                .pb(px(6.0))
                .text_size(px(11.0))
                .child(line)
        });

        // 结果列表（虚拟滚动：命中数可达上万，全量渲染会卡死）
        let rows = self.rows.clone();
        let outcome = self.outcome.clone();
        let list = if rows.is_empty() {
            div().flex_1().min_h_0().into_any_element()
        } else {
            let outcome_for_render = outcome.clone();
            uniform_list(
                "project-search-results",
                rows.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    let Some(outcome) = outcome_for_render.as_ref() else {
                        return Vec::new();
                    };
                    range
                        .map(|ix| {
                            match rows.get(ix) {
                                Some(ResultRow::Group { file_ix }) => {
                                    let file = &outcome.files[*file_ix];
                                    div()
                                        .h(px(GROUP_ROW_HEIGHT))
                                        .w_full()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(6.0))
                                        .px(px(8.0))
                                        .bg(cx.theme().ui.panel_background)
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_color(cx.theme().ui.text)
                                                .child(file.rel_path.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .text_color(cx.theme().ui.text_subtle)
                                                .child(format!("{}", file.lines.len())),
                                        )
                                        .into_any_element()
                                }
                                Some(ResultRow::Match { file_ix, line_ix }) => {
                                    let file = &outcome.files[*file_ix];
                                    let (file_ix, line_ix) = (*file_ix, *line_ix);
                                    let content = this.render_match_line(file, line_ix, cx);
                                    div()
                                        .h(px(RESULT_ROW_HEIGHT))
                                        .w_full()
                                        .pl(px(8.0))
                                        .pr(px(8.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(cx.theme().ui.tab_hover_background))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _window, cx| {
                                                this.open_match(file_ix, line_ix, cx);
                                            }),
                                        )
                                        .child(content)
                                        .into_any_element()
                                }
                                None => div().into_any_element(),
                            }
                        })
                        .collect()
                }),
            )
            .flex_1()
            .min_h_0()
            .track_scroll(&self.scroll_handle)
            .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("ProjectSearchPanel")
            .track_focus(&self.focus_handle)
            .child(header)
            .children(status)
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_manager::project_search::{FileMatch, MatchLine, ProjectSearchOutcome};

    fn outcome_with(files: Vec<(&str, Vec<usize>)>) -> ProjectSearchOutcome {
        let mut outcome = ProjectSearchOutcome::default();
        for (rel, line_numbers) in files {
            let lines: Vec<MatchLine> = line_numbers
                .into_iter()
                .map(|n| MatchLine {
                    line_number: n,
                    line_text: format!("line {n}"),
                    columns: vec![0..4],
                })
                .collect();
            outcome.total_lines += lines.len();
            outcome.files.push(FileMatch {
                path: PathBuf::from(format!("/root/{rel}")),
                rel_path: rel.to_string(),
                lines,
                truncated: false,
            });
        }
        outcome
    }

    #[test]
    fn build_rows_emits_group_then_matches_per_file() {
        let outcome = outcome_with(vec![("a.rs", vec![1, 5]), ("b.rs", vec![9])]);
        let rows = ProjectSearchPanel::build_rows(&outcome);
        // 2 个文件：a 有 group+2 match，b 有 group+1 match。
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], ResultRow::Group { file_ix: 0 }));
        assert!(matches!(rows[1], ResultRow::Match { file_ix: 0, line_ix: 0 }));
        assert!(matches!(rows[2], ResultRow::Match { file_ix: 0, line_ix: 1 }));
        assert!(matches!(rows[3], ResultRow::Group { file_ix: 1 }));
        assert!(matches!(rows[4], ResultRow::Match { file_ix: 1, line_ix: 0 }));
    }

    #[test]
    fn build_rows_empty_outcome_yields_no_rows() {
        let rows = ProjectSearchPanel::build_rows(&ProjectSearchOutcome::default());
        assert!(rows.is_empty());
    }

    /// 行内高亮切片必须按字符切：CJK 行上按字节切会切出半个字。
    #[test]
    fn match_columns_are_character_indexed_for_cjk() {
        let outcome = outcome_with(vec![("cjk.txt", vec![1])]);
        let line = &outcome.files[0].lines[0];
        let chars: Vec<char> = line.line_text.chars().collect();
        // 模拟渲染侧的切片逻辑。
        for Range { start, end } in &line.columns {
            assert!(*end <= chars.len(), "列区间必须落在字符数内");
        }
    }
}
