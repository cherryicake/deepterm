//! 侧边栏「搜索」面板：按文件名筛选项目文件。
//!
//! 只匹配文件名，不搜索文件内容。遍历层复用 [`crate::file_manager::walk`]：
//! 首次查询时在后台建一次文件索引，之后每次击键只在内存里过滤——每轮击键
//! 重新遍历在大仓库上要几百毫秒，索引化后省掉了输入防抖与逐轮取消。
//!
//! 结果列表沿用文件树中文件行的样式（图标 + 文件名，24px 行高），
//! 点击即在编辑器中打开。

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, IntoElement, MouseButton, Render, Subscription,
    Task, UniformListScrollHandle, Window, div, px, svg, uniform_list,
};

use crate::assets::{Icon, file_icon_or_default};
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::tooltip::Tooltip;
use crate::file_manager::walk::{FileEntry, walk};
use crate::sidebar_view::file_tree::LIST_ROW_HEIGHT;
use crate::theme::ActiveTheme;

/// 命中上限。上千条时列表已无参考价值，继续输入更长的词收敛更快。
const MAX_RESULTS: usize = 1000;

/// 搜索面板事件。
pub enum FileSearchEvent {
    /// 打开命中的文件。
    OpenFile { path: PathBuf },
}

impl EventEmitter<FileSearchEvent> for FileSearchPanel {}

pub struct FileSearchPanel {
    /// 查询输入框。懒建：`ensure_file_search_panel` 的调用点没有 window
    /// （与 `ensure_git_panel` 同一批），而 `InputState::new` 需要 window。
    input: Option<Entity<InputState>>,
    _input_subscription: Option<Subscription>,

    /// 搜索根（当前文件树的根）。`None` 表示没有可遍历的本地根（远程树）。
    root: Option<PathBuf>,
    /// 文件名索引：懒建，根变化即作废。
    index: Option<Arc<Vec<FileEntry>>>,
    /// 索引构建任务。持有句柄才能在根变化时丢弃旧根的遍历。
    index_task: Option<Task<()>>,
    /// 索引代际。每开一轮 +1，结果回来时比对代际决定是否采纳。
    index_epoch: u64,
    /// 索引是否因 [`crate::file_manager::walk::MAX_WALK_ENTRIES`] 提前结束。
    index_truncated: bool,
    /// 索引正在后台构建。
    loading: bool,

    /// 当前查询词（原样保留大小写，匹配时统一小写）。
    query: String,
    /// 命中结果在索引中的下标。
    results: Vec<usize>,
    /// 命中数超过 [`MAX_RESULTS`] 被截断。
    results_truncated: bool,

    /// 切换到本面板时请求聚焦输入框，由 `render` 落地。
    focus_pending: bool,
    scroll_handle: UniformListScrollHandle,
}

impl Default for FileSearchPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSearchPanel {
    pub fn new() -> Self {
        Self {
            input: None,
            _input_subscription: None,
            root: None,
            index: None,
            index_task: None,
            index_epoch: 0,
            index_truncated: false,
            loading: false,
            query: String::new(),
            results: Vec::new(),
            results_truncated: false,
            focus_pending: false,
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    /// 设置搜索根（当前文件树的根）。远程树没有本地根，传 `None`。
    ///
    /// 不接收 `window`：调用点（文件树 set/refresh 路径）没有 window，
    /// 与 `ensure_git_panel` 同一批。
    pub fn set_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.root == root {
            return;
        }
        self.root = root;
        // 旧索引属于旧根：命中结果带着绝对路径，留着会打开别的项目的文件。
        self.index = None;
        self.index_task.take();
        self.index_epoch = self.index_epoch.wrapping_add(1);
        self.index_truncated = false;
        self.loading = false;
        self.results.clear();
        self.results_truncated = false;
        if !self.query.trim().is_empty() {
            self.ensure_index(cx);
        }
        cx.notify();
    }

    /// 切换到本面板时请求聚焦输入框（输入框懒建，聚焦由 `render` 落地）。
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        cx.notify();
    }

    /// 懒建输入框（需要 window）。
    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索文件名")
                .placeholder_font_size(px(13.0))
                .font_size(px(13.0))
                .clean_on_escape()
        });
        self._input_subscription = Some(cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    this.on_query_changed(query, cx);
                }
            },
        ));
        self.input = Some(input);
    }

    /// 输入变化：索引未建先建，已建则直接过滤。
    fn on_query_changed(&mut self, query: String, cx: &mut Context<Self>) {
        self.query = query;
        if self.query.trim().is_empty() {
            // 清空查询：结果作废，索引保留（下次输入直接过滤）。
            self.results.clear();
            self.results_truncated = false;
        } else if self.index.is_some() {
            self.apply_filter(cx);
            return;
        } else {
            self.ensure_index(cx);
        }
        cx.notify();
    }

    /// 后台建一次文件名索引。索引是「一次遍历、多次过滤」的关键。
    fn ensure_index(&mut self, cx: &mut Context<Self>) {
        if self.index.is_some() || self.loading {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        self.loading = true;
        self.index_epoch = self.index_epoch.wrapping_add(1);
        let epoch = self.index_epoch;
        self.index_task = Some(cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { walk(&root) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // 根已切换：本轮结果属于旧根，丢弃。
                if this.index_epoch != epoch {
                    return;
                }
                this.loading = false;
                this.index_truncated = outcome.truncated;
                this.index = Some(Arc::new(outcome.entries));
                this.apply_filter(cx);
            });
        }));
    }

    /// 用当前查询词过滤索引。
    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.index.as_ref() else {
            self.results.clear();
            self.results_truncated = false;
            cx.notify();
            return;
        };
        let (results, truncated) = filter_index(index, &self.query);
        self.results = results;
        self.results_truncated = truncated;
        cx.notify();
    }

    fn open_result(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        let Some(entry_ix) = self.results.get(row_ix).copied() else {
            return;
        };
        let Some(entry) = self.index.as_ref().and_then(|index| index.get(entry_ix)) else {
            return;
        };
        cx.emit(FileSearchEvent::OpenFile {
            path: entry.path.clone(),
        });
    }

    /// 渲染一行命中：与文件树中的文件行同款（图标 + 文件名 + 悬停底色）。
    fn render_result_row(
        &self,
        entry: &FileEntry,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().ui;
        let last_row_ix = self.results.len().checked_sub(1);

        let row = div()
            .w_full()
            .h_full()
            .px(px(2.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .hover(|style| style.bg(theme.tab_hover_background))
            .child(
                svg()
                    .size(px(18.0))
                    .flex_none()
                    .text_color(theme.icon_muted)
                    .path(file_icon_or_default(&entry.path).path()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .truncate()
                    .text_sm()
                    .text_color(theme.text)
                    .child(entry.file_name().to_string()),
            );

        div()
            .id(("file-search-result", row_ix))
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .px(px(4.0))
            .cursor_pointer()
            .debug_selector(move || {
                // 首/末行用固定选择器：布局回归测试按它读真实 bounds。
                if row_ix == 0 {
                    "file-search-row-first".to_string()
                } else if Some(row_ix) == last_row_ix {
                    "file-search-row-last".to_string()
                } else {
                    format!("file-search-row-{row_ix}")
                }
            })
            .tooltip(Tooltip::text(entry.path.to_string_lossy().to_string()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.open_result(row_ix, cx)),
            )
            .child(row)
            .into_any_element()
    }

    fn render_search_box(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .h(px(32.0))
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(6.0))
            .rounded(px(8.0))
            .bg(theme.tab_hover_background)
            .child(
                svg()
                    .size(px(14.0))
                    .flex_none()
                    .path(Icon::MagnifyingGlass.path())
                    .text_color(theme.text_muted),
            )
            .children(
                self.input
                    .as_ref()
                    .map(|input| EditorInput::new(input).transparent().into_any_element()),
            )
            .into_any_element()
    }

    /// 空列表时的一行提示。
    fn empty_message(&self) -> &'static str {
        if self.root.is_none() {
            return "远程项目暂不支持文件名搜索";
        }
        if self.query.trim().is_empty() {
            return "输入文件名搜索项目文件";
        }
        if self.loading {
            return "正在读取项目文件…";
        }
        "没有匹配的文件"
    }

    /// 落地 `request_focus`：聚焦放在本帧渲染之后，渲染期间改焦点会打断当前帧。
    fn flush_pending_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_pending {
            return;
        }
        self.focus_pending = false;
        let Some(input) = self.input.clone() else {
            return;
        };
        cx.defer_in(window, move |_this, window, cx| {
            input.update(cx, |state, cx| state.focus(window, cx));
        });
    }
}

/// 按文件名筛选索引，返回命中的下标与是否被 [`MAX_RESULTS`] 截断。
fn filter_index(index: &[FileEntry], query: &str) -> (Vec<usize>, bool) {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (Vec::new(), false);
    }
    let mut hits = Vec::new();
    for (ix, entry) in index.iter().enumerate() {
        if name_matches(entry.file_name(), &needle) {
            if hits.len() == MAX_RESULTS {
                return (hits, true);
            }
            hits.push(ix);
        }
    }
    (hits, false)
}

/// 文件名是否包含查询词（大小写不敏感）。
///
/// ASCII 走无分配的字节窗口比较：索引可达十万级，每次击键对每个文件名
/// 都做一次 `to_lowercase` 会产生同量级的临时分配。
fn name_matches(name: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if name.is_ascii() {
        let (hay, pat) = (name.as_bytes(), needle_lower.as_bytes());
        return pat.len() <= hay.len()
            && hay.windows(pat.len()).any(|w| w.eq_ignore_ascii_case(pat));
    }
    name.to_lowercase().contains(needle_lower)
}

impl Render for FileSearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        self.ensure_input(window, cx);
        self.flush_pending_focus(window, cx);

        let header = div()
            .px(px(8.0))
            .pt(px(6.0))
            .pb(px(8.0))
            .child(self.render_search_box(cx));

        let body: AnyElement = if self.results.is_empty() {
            let message = self.empty_message();
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(16.0))
                .child(div().text_sm().text_color(theme.text_subtle).child(message))
                .into_any_element()
        } else {
            let index = self.index.clone();
            let results = Arc::new(self.results.clone());
            uniform_list(
                "file-search-results",
                results.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    let Some(index) = index.as_ref() else {
                        return Vec::new();
                    };
                    range
                        .filter_map(|row_ix| {
                            let entry_ix = *results.get(row_ix)?;
                            let entry = index.get(entry_ix)?;
                            Some(this.render_result_row(entry, row_ix, cx))
                        })
                        .collect()
                }),
            )
            .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        let notice = if self.results_truncated {
            Some(format!("仅显示前 {MAX_RESULTS} 个匹配"))
        } else if self.index_truncated {
            Some("项目文件过多，索引已截断".to_string())
        } else {
            None
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .children(notice.map(|text| {
                div()
                    .px(px(8.0))
                    .pb(px(6.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_subtle)
                    .child(text)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rel_path: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(format!("/root/{rel_path}")),
            rel_path: rel_path.to_string(),
        }
    }

    fn matched_names(index: &[FileEntry], query: &str) -> Vec<String> {
        filter_index(index, query)
            .0
            .into_iter()
            .map(|ix| index[ix].file_name().to_string())
            .collect()
    }

    #[test]
    fn matches_file_name_case_insensitively() {
        let index = vec![entry("src/Main.rs"), entry("src/lib.rs")];
        assert_eq!(matched_names(&index, "main"), vec!["Main.rs"]);
        assert_eq!(matched_names(&index, "MAIN"), vec!["Main.rs"]);
        assert_eq!(matched_names(&index, ".RS"), vec!["Main.rs", "lib.rs"]);
    }

    #[test]
    fn matches_chinese_file_names() {
        let index = vec![entry("docs/设计文档.md"), entry("docs/readme.md")];
        assert_eq!(matched_names(&index, "文档"), vec!["设计文档.md"]);
    }

    /// 只认文件名：查询词命中目录名不算命中（这是「文件名搜索」与
    /// 「路径搜索」的分界）。
    #[test]
    fn ignores_directory_segments() {
        let index = vec![entry("src/main.rs"), entry("docs/src.md")];
        assert_eq!(matched_names(&index, "src"), vec!["src.md"]);
    }

    #[test]
    fn non_ascii_names_fold_case() {
        let index = vec![entry("Ünicode.txt"), entry("plain.txt")];
        assert_eq!(matched_names(&index, "ünicode"), vec!["Ünicode.txt"]);
    }

    #[test]
    fn empty_query_yields_no_hits() {
        let index = vec![entry("a.rs")];
        assert_eq!(filter_index(&index, "   ").0.len(), 0);
        assert_eq!(filter_index(&index, "").0.len(), 0);
    }

    #[test]
    fn long_query_does_not_panic_on_short_names() {
        let index = vec![entry("a.rs")];
        assert_eq!(filter_index(&index, "a-much-longer-query").0.len(), 0);
    }

    #[test]
    fn results_are_truncated_at_the_cap() {
        let index: Vec<FileEntry> = (0..MAX_RESULTS + 10)
            .map(|i| entry(&format!("hit-{i}.rs")))
            .collect();
        let (hits, truncated) = filter_index(&index, "hit-");
        assert_eq!(hits.len(), MAX_RESULTS);
        assert!(truncated);
    }

    /// 面板在窗口里的端到端行为：打字过滤 + 点击结果打开文件。
    struct SearchProbeHost {
        panel: Entity<FileSearchPanel>,
    }

    impl Render for SearchProbeHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            // 固定尺寸：查询框 46px（6 上边距 + 32 高 + 8 下边距），
            // 首条结果紧接其后占 24px。
            div().w(px(300.)).h(px(400.)).child(self.panel.clone())
        }
    }

    fn probe_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deepterm-file-search-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for rel in ["alpha.rs", "beta.txt", "sub/gamma.rs", "AlphaUpper.rs"] {
            std::fs::write(dir.join(rel), "x").unwrap();
        }
        dir
    }

    fn visible_names(panel: &FileSearchPanel) -> Vec<String> {
        let index = panel.index.as_ref().expect("索引应已建好");
        panel
            .results
            .iter()
            .map(|ix| index[*ix].file_name().to_string())
            .collect()
    }

    #[gpui::test]
    fn typing_filters_by_file_name_and_click_opens_the_file(cx: &mut gpui::TestAppContext) {
        // 主题 + 输入框键位（cmd-a / backspace 等）。
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::components::init(cx);
        });
        let root = probe_root("e2e");

        let panel = cx.update(|cx| cx.new(|_| FileSearchPanel::new()));
        panel.update(cx, |panel, cx| panel.set_root(Some(root.clone()), cx));

        let opened = std::sync::Arc::new(std::sync::Mutex::new(None::<PathBuf>));
        let opened_for_sub = opened.clone();
        panel.update(cx, |_panel, cx| {
            cx.subscribe(&cx.entity(), move |_, _, event: &FileSearchEvent, _| {
                let FileSearchEvent::OpenFile { path } = event;
                *opened_for_sub.lock().unwrap() = Some(path.clone());
            })
            .detach();
        });

        let window = cx.open_window(gpui::size(px(300.), px(400.)), {
            let panel = panel.clone();
            move |_, _| SearchProbeHost {
                panel: panel.clone(),
            }
        });
        let handle: gpui::AnyWindowHandle = window.into();
        let cx = &mut gpui::VisualTestContext::from_window(handle, cx);
        cx.run_until_parked();

        // 聚焦查询框（与切到「搜索」标签时的行为一致）。
        panel.update(cx, |panel, cx| panel.request_focus(cx));
        cx.run_until_parked();
        cx.simulate_input("alpha");
        cx.run_until_parked();

        let (names, first_hit) = panel.read_with(cx, |panel, _| {
            let mut names = visible_names(panel);
            names.sort();
            let first = panel
                .results
                .first()
                .map(|ix| panel.index.as_ref().unwrap()[*ix].path.clone());
            (names, first)
        });
        assert_eq!(
            names,
            vec!["AlphaUpper.rs".to_string(), "alpha.rs".to_string()],
            "大小写不敏感地按文件名过滤，且不匹配目录名"
        );

        // 点击首条结果（y 落在 46..70 的首行内）必须打开该行对应的文件。
        cx.simulate_click(gpui::point(px(150.), px(58.)), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            opened.lock().unwrap().clone(),
            first_hit,
            "点击命中行必须打开该行显示的文件"
        );

        // 清空查询：结果清空，但索引保留（下次输入无需重新遍历）。
        cx.simulate_keystrokes("cmd-a backspace");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.results.is_empty(), "清空查询后不应残留结果");
            assert!(panel.index.is_some(), "清空查询不应丢弃已建好的索引");
        });
    }

    /// 渲染层回归：命中行必须真的画出来（行高、行数、位置都对），
    /// 而不是只把数据塞进 `results`。
    #[gpui::test]
    fn result_rows_paint_at_file_tree_row_height(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::components::init(cx);
        });
        let root = probe_root("paint");

        let panel = cx.update(|cx| cx.new(|_| FileSearchPanel::new()));
        panel.update(cx, |panel, cx| panel.set_root(Some(root), cx));

        let window = cx.open_window(gpui::size(px(300.), px(400.)), {
            let panel = panel.clone();
            move |_, _| SearchProbeHost {
                panel: panel.clone(),
            }
        });
        let handle: gpui::AnyWindowHandle = window.into();
        let cx = &mut gpui::VisualTestContext::from_window(handle, cx);
        cx.run_until_parked();

        let row_bounds = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds(selector)
        };
        assert!(
            row_bounds(cx, "file-search-row-first").is_none(),
            "未输入查询时不应画结果行"
        );

        panel.update(cx, |panel, cx| panel.request_focus(cx));
        cx.run_until_parked();
        cx.simulate_input("a");
        cx.run_until_parked();

        let expected = panel.read_with(cx, |panel, _| panel.results.len());
        assert!(expected >= 3, "查询 a 应命中多个文件，实际 {expected}");
        let first = row_bounds(cx, "file-search-row-first").expect("首条命中必须真的渲染出来");
        assert_eq!(
            first.size.height,
            px(LIST_ROW_HEIGHT),
            "命中行行高必须与文件树一致"
        );
        let last = row_bounds(cx, "file-search-row-last").expect("末条命中必须渲染出来");
        // 行自上而下紧密排列：第 n 行的顶边 = 首行顶边 + n * 行高。
        assert_eq!(
            last.origin.y - first.origin.y,
            px(LIST_ROW_HEIGHT) * (expected - 1) as f32,
            "命中行必须按行高紧密排列"
        );
    }
}
