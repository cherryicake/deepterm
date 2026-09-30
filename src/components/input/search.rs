//! Search panel for code editors: find / replace UI plus match navigation.
//!
//! Ported from gpui-component's `input/search.rs`, adapted to deepterm's
//! component set (no `Label` / `IconName` / flex helpers).

use std::{ops::Range, rc::Rc};

use aho_corasick::AhoCorasick;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, Styled, Subscription, Window, actions, div,
    px, relative,
};
use ropey::Rope;

use crate::components::button::{
    Button, ButtonCustomVariant, ButtonVariants, Disableable, Icon, Sizable, Size,
};
use crate::theme::ActiveTheme;

use super::{
    EditorInput, Enter, Escape, IndentInline, InputEvent, InputState, Search,
    movement::MoveDirection,
};

const CONTEXT: &str = "SearchPanel";

actions!(input, [Tab]);

/// 计算跳转目标匹配索引。
///
/// `caret` 为编辑器光标字节偏移；返回 None 表示无匹配。
/// - dir = 1（下一个）：光标所在/之后的第一个匹配；若光标已在该匹配内，则取再下一个（循环）。
/// - dir = -1（上一个）：光标之前的最后一个匹配；若光标在该匹配内，则取它的前一个（循环）。
///
/// 注意：跳转后选区落在匹配起点上，因此「上一个」不能用
/// 「start < caret」判断是否在匹配内，否则会永远选中同一个匹配。
pub(crate) fn jump_target(ranges: &[Range<usize>], caret: usize, dir: i32) -> Option<usize> {
    let len = ranges.len();
    if len == 0 {
        return None;
    }
    // ix0：第一个结束位置在 caret 之后的匹配（即 caret 所在或之后的第一个）。
    let ix0 = ranges.iter().position(|r| r.end > caret);
    let inside = |i: usize| ranges[i].start <= caret; // ix0 匹配是否包含 caret 起点
    Some(match dir {
        -1 => match ix0 {
            Some(i) if inside(i) => (i + len - 1) % len,
            Some(0) | None => len - 1, // caret 在首个匹配之前 / 所有匹配之后：回绕到最后一个
            Some(i) => i - 1,
        },
        _ => match ix0 {
            Some(i) if inside(i) => (i + 1) % len,
            Some(i) => i,
            None => 0, // caret 越过所有匹配：回绕到第一个
        },
    })
}

/// 替换时按匹配文本的大小写形态变换替换文本：
/// - 全大写匹配 → 全大写替换（FOO → BAR）
/// - 首字母大写匹配 → 首字母大写替换（Foo → Bar）
/// - 其他 / 无字母 → 原样
fn apply_case_preservation(template: &str, matched: &str, preserve: bool) -> String {
    if !preserve || template.is_empty() || matched.is_empty() {
        return template.to_string();
    }
    let letters: Vec<char> = matched.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() || !letters.iter().any(|c| c.is_uppercase()) {
        return template.to_string();
    }
    if letters.iter().all(|c| c.is_uppercase()) {
        template.to_uppercase()
    } else if letters[0].is_uppercase() {
        let mut chars = template.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => template.to_string(),
        }
    } else {
        template.to_string()
    }
}

/// The active search query: a literal (aho-corasick) or a regex.
#[derive(Debug, Clone)]
enum SearchQuery {
    Literal(AhoCorasick),
    Regex(regex::Regex),
}

/// 搜索扫描上限：超大文件跳过匹配（避免每次按键全量扫描卡死）；
/// 单次匹配数封顶（避免 `a` 这类查询在几 MB 文件里撑爆 Vec）。
const SEARCH_MAX_TEXT_LEN: usize = 5_000_000;
const SEARCH_MAX_MATCHES: usize = 10_000;

#[derive(Debug, Clone)]
pub struct SearchMatcher {
    text: Rope,
    query: Option<SearchQuery>,
    /// 字面量全字匹配：保留 Aho-Corasick 字面扫描，按字符判定边界过滤
    ///（比转正则更快，且与同词高亮共用 Unicode 词定义，CJK 行为一致）。
    literal_whole_word: bool,

    pub(super) matched_ranges: Rc<Vec<Range<usize>>>,
    pub(super) current_match_ix: usize,
    /// Is in replacing mode, if true, the next update will update the current
    /// match index based on matched ranges.
    replacing: bool,
}

impl Default for SearchMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchMatcher {
    pub fn new() -> Self {
        Self {
            text: "".into(),
            query: None,
            literal_whole_word: false,
            matched_ranges: Rc::new(Vec::new()),
            current_match_ix: 0,
            replacing: false,
        }
    }

    /// Update source text and re-match.
    pub(crate) fn update(&mut self, text: &Rope) {
        if self.text.eq(text) {
            return;
        }

        self.text = text.clone();
        self.update_matches();
    }

    fn update_matches(&mut self) {
        let mut new_ranges = Vec::new();
        if self.text.len() <= SEARCH_MAX_TEXT_LEN {
            if let Some(query) = &self.query {
                let text = self.text.to_string();
                match query {
                    SearchQuery::Literal(matcher) => {
                        let whole = self.literal_whole_word;
                        for query_match in matcher.stream_find_iter(text.as_bytes()) {
                            // 关键防御：流式匹配理论上恒 Ok，但渲染路径不得 expect panic，
                            // 出错时跳过该匹配，保证搜索永不崩溃。
                            let Ok(query_match) = query_match else {
                                log::warn!("搜索字面量匹配流异常，已跳过");
                                continue;
                            };
                            let range = query_match.range();
                            if whole
                                && !Self::literal_word_boundary(&text, &range)
                            {
                                continue;
                            }
                            new_ranges.push(range);
                            if new_ranges.len() >= SEARCH_MAX_MATCHES {
                                break;
                            }
                        }
                    }
                    SearchQuery::Regex(re) => {
                        for m in re.find_iter(&text) {
                            new_ranges.push(m.range());
                            if new_ranges.len() >= SEARCH_MAX_MATCHES {
                                break;
                            }
                        }
                    }
                }
            }
        }
        self.matched_ranges = Rc::new(new_ranges);
        if !self.replacing || self.matched_ranges.is_empty() {
            self.current_match_ix = 0;
        } else {
            self.current_match_ix = self.current_match_ix.min(self.matched_ranges.len() - 1);
        }
        self.replacing = false;
    }

    /// Update the search query and reset the current match index.
    pub fn update_query(&mut self, query: &str, case_insensitive: bool, use_regex: bool) {
        self.update_query_ext(query, case_insensitive, use_regex, false);
    }

    /// 字面量全字边界：左右邻字符均非词字符（与同词高亮共用 Unicode 定义）。
    fn literal_word_boundary(text: &str, range: &Range<usize>) -> bool {
        use super::state::is_word_char;
        let left_ok = range.start == 0
            || !text[..range.start].chars().next_back().is_some_and(is_word_char);
        let right_ok = range.end >= text.len()
            || !text[range.end..].chars().next().is_some_and(is_word_char);
        left_ok && right_ok
    }

    /// 扩展版：支持全字匹配。字面量全字走 Aho 扫描 + 字符边界过滤
    ///（与同词高亮同一定義，CJK 一致）；正则全字仍用 `\b` 包裹。
    /// 非法正则时 query 置 None，由调用方标记错误状态。
    pub fn update_query_ext(
        &mut self,
        query: &str,
        case_insensitive: bool,
        use_regex: bool,
        whole_word: bool,
    ) {
        if query.is_empty() {
            self.query = None;
            self.literal_whole_word = false;
        } else if use_regex {
            self.literal_whole_word = false;
            let expr = if whole_word {
                format!(r"\b(?:{})\b", query)
            } else {
                query.to_string()
            };
            let expr = if case_insensitive {
                format!("(?i){expr}")
            } else {
                expr
            };
            self.query = regex::Regex::new(&expr).ok().map(SearchQuery::Regex);
        } else if whole_word {
            // 字面量 + 全字：不转正则，按字符边界过滤（见 update_matches）。
            self.literal_whole_word = true;
            // 关键防御：用户任意输入（含超大 query）导致构建失败时降级为无查询而非 panic。
            self.query = AhoCorasick::builder()
                .ascii_case_insensitive(case_insensitive)
                .build(&[query.to_string()])
                .inspect_err(|e| log::warn!("搜索查询构建失败，已清空查询: {e}"))
                .ok()
                .map(SearchQuery::Literal);
        } else {
            // 关键防御：用户任意输入（含超大 query）导致构建失败时降级为无查询而非 panic。
            self.query = AhoCorasick::builder()
                .ascii_case_insensitive(case_insensitive)
                .build(&[query.to_string()])
                .inspect_err(|e| log::warn!("搜索查询构建失败，已清空查询: {e}"))
                .ok()
                .map(SearchQuery::Literal);
        }
        self.update_matches();
    }

    /// Returns the number of matches found.
    #[inline]
    fn len(&self) -> usize {
        self.matched_ranges.len()
    }

    /// Byte ranges of the current matches, in document order.
    ///
    /// Live-markdown find parity: the live editor matches its own text
    /// through a Rope snapshot and maps ranges back to source lines.
    pub fn match_ranges(&self) -> &[Range<usize>] {
        &self.matched_ranges
    }

    /// Number of current matches.
    pub fn match_count(&self) -> usize {
        self.matched_ranges.len()
    }

    /// Index of the current match.
    pub fn current_ix(&self) -> usize {
        self.current_match_ix
    }

    fn peek(&self) -> Option<Range<usize>> {
        let next_match_ix = self.next_ix()?;
        self.matched_ranges.get(next_match_ix).cloned()
    }

    fn next_ix(&self) -> Option<usize> {
        if self.matched_ranges.is_empty() {
            None
        } else if self.has_next_match_without_wrap() {
            Some(self.current_match_ix + 1)
        } else {
            Some(0)
        }
    }

    fn has_next_match_without_wrap(&self) -> bool {
        self.current_match_ix < self.matched_ranges.len().saturating_sub(1)
    }

    fn label(&self) -> String {
        if self.len() == 0 {
            return "无结果".to_string();
        }
        format!("第{}项，共{}项", self.current_match_ix + 1, self.len())
    }

    /// Update the current match index based on the given offset.
    fn update_cursor_by_offset(&mut self, offset: usize) {
        for (ix, range) in self.matched_ranges.iter().enumerate() {
            self.current_match_ix = ix;
            if range.contains(&offset) || range.end >= offset {
                return;
            }
        }
    }
}

impl Iterator for SearchMatcher {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let next_match_ix = self.next_ix()?;
        self.current_match_ix = next_match_ix;
        self.matched_ranges.get(next_match_ix).cloned()
    }
}

impl DoubleEndedIterator for SearchMatcher {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.matched_ranges.is_empty() {
            return None;
        }

        if self.current_match_ix == 0 {
            self.current_match_ix = self.matched_ranges.len();
        }

        self.current_match_ix -= 1;
        let item = self.matched_ranges[self.current_match_ix].clone();

        Some(item)
    }
}

/// `SearchPanel` 能挂载的文档（代码编辑器与 md 实时编辑器共用同一套
/// 查找/替换面板，不再各自实现一遍）。
///
/// - `InputState`（代码编辑器）：`Rope` 直驱，视口顶部锚定。
/// - `LiveMarkdownView`（md 实时编辑）：行列缓冲经字节偏移映射，锚定光标。
pub(crate) trait SearchTarget: 'static + Sized {
    /// 全文（匹配器输入）。
    fn search_text(&self) -> Rope;
    /// 查询框预填（通常取选区首行）。
    fn search_prefill(&self) -> Rope;
    /// 光标字节偏移（跳转锚点）。
    fn search_caret(&self) -> usize;
    /// `show` 的初始锚点（代码编辑器取视口顶部，live 取光标）；
    /// `None` 则从第 0 个匹配开始。
    fn anchor_offset(&self) -> Option<usize>;
    /// 选中一个匹配并滚入视野（不抢焦点）。
    fn reveal_match(&mut self, range: Range<usize>, window: &mut Window, cx: &mut Context<Self>);
    /// 替换一个匹配（单步 undo）。
    fn replace_match(
        &mut self,
        range: Range<usize>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    );
    /// 按 `(byte range, 替换串)` 逐个替换当前全部匹配（单步 undo；
    /// 替换串已逐匹配做好大小写保持）。
    fn replace_all_matches(
        &mut self,
        pairs: &[(Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    );
    /// 替换后滚动（代码编辑器滚向下一匹配，live 滚向编辑处）。
    fn scroll_to_offset(
        &mut self,
        offset: usize,
        direction: Option<MoveDirection>,
        cx: &mut Context<Self>,
    );
    /// 是否允许替换（只读宿主隐藏替换行）。
    fn search_replaceable(&self) -> bool;
    /// 文档焦点（面板关闭 / Tab 时聚焦回去；返回 owned handle 以便
    /// 调用方先结束 `editor.read` 借用再聚焦）。
    fn target_focus_handle(&self, cx: &App) -> FocusHandle;
}

pub(crate) struct SearchPanel<T: SearchTarget = InputState> {
    editor: Entity<T>,
    search_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    case_insensitive: bool,
    use_regex: bool,
    /// 全字匹配（仅 literal 模式；内部转为 \b 包裹的正则）。
    whole_word: bool,
    /// 当前 query 为非法正则：UI 显示红色错误提示而非计数。
    query_invalid: bool,
    /// 替换时保留大小写：按每个匹配的形态变换替换文本（Foo→Bar / FOO→BAR）。
    preserve_case: bool,
    replace_mode: bool,
    matcher: SearchMatcher,

    open: bool,
    _subscriptions: Vec<Subscription>,
}

impl InputState {
    /// Update the search matcher when text changes.
    pub(super) fn update_search(&mut self, cx: &mut App) {
        let Some(search_panel) = self.search_panel.as_ref() else {
            return;
        };

        let text = self.text.clone();
        search_panel.update(cx, |this, _| {
            this.matcher.update(&text);
        });
    }

    pub(super) fn on_action_search(
        &mut self,
        _: &Search,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.searchable {
            return;
        }

        let search_panel = match self.search_panel.as_ref() {
            Some(panel) => panel.clone(),
            None => SearchPanel::new(cx.entity(), window, cx),
        };

        let text = self.text.clone();
        let editor = cx.entity();
        let selected_text = Rope::from(self.selected_text());
        search_panel.update(cx, |this, cx| {
            this.editor = editor;
            this.matcher.update(&text);
            this.show(&selected_text, window, cx);
        });
        self.search_panel = Some(search_panel);
        cx.notify();
    }

    // NOTE: `SearchPrevious` 由 `SearchPanel::on_action_search_previous` 处理
    // （焦点在搜索面板内时派发）。这里不需要重复实现。
}

impl SearchTarget for InputState {
    fn search_text(&self) -> Rope {
        self.text.clone()
    }

    fn search_prefill(&self) -> Rope {
        Rope::from(self.selected_text())
    }

    fn search_caret(&self) -> usize {
        self.selected_range.start
    }

    fn anchor_offset(&self) -> Option<usize> {
        self.last_layout
            .as_ref()
            .map(|l| l.visible_range_offset.start)
    }

    fn reveal_match(&mut self, range: Range<usize>, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = range.clone().into();
        self.scroll_to_center(range.start, cx);
    }

    fn replace_match(
        &mut self,
        range: Range<usize>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range_utf16 = self.range_to_utf16(&range);
        self.replace_text_in_range_silent(Some(range_utf16), text, window, cx);
    }

    fn replace_all_matches(
        &mut self,
        pairs: &[(Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 逐个匹配拼接替换结果（逐匹配保留大小写已由调用方算好）。
        let content = self.text.clone();
        let mut out = String::new();
        let mut last = 0usize;
        for (range, replacement) in pairs {
            out.push_str(&content.slice(last..range.start).to_string());
            out.push_str(replacement);
            last = range.end;
        }
        out.push_str(&content.slice(last..).to_string());

        let full_range_utf16 = 0..self.offset_to_utf16(self.text.len());
        self.replace_text_in_range_silent(Some(full_range_utf16), out.as_str(), window, cx);
        self.scroll_to(0, Some(MoveDirection::Down), cx);
    }

    fn scroll_to_offset(
        &mut self,
        offset: usize,
        direction: Option<MoveDirection>,
        cx: &mut Context<Self>,
    ) {
        self.scroll_to(offset, direction, cx);
    }

    fn search_replaceable(&self) -> bool {
        self.replaceable
    }

    fn target_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<T: SearchTarget> SearchPanel<T> {
    fn next_scroll_direction(
        previous_match_ix: usize,
        current_match_ix: usize,
    ) -> Option<MoveDirection> {
        if current_match_ix <= previous_match_ix {
            None
        } else {
            Some(MoveDirection::Down)
        }
    }

    // 没有「替换并跳到上一个」的入口，`prev_scroll_direction` 因此没有调用点。

    pub fn new(editor: Entity<T>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let replace_input = cx.new(|cx| InputState::new(window, cx).placeholder("替换"));

        cx.new(|cx| {
            let _subscriptions = vec![cx.subscribe(&search_input, |this: &mut Self, _, ev, cx| {
                if let InputEvent::Change = ev {
                    this.update_search_query(cx);
                }
            })];

            Self {
                editor,
                search_input,
                replace_input,
                case_insensitive: true,
                use_regex: false,
                whole_word: false,
                query_invalid: false,
                preserve_case: false,
                // 替换行默认收起，通过左侧 ⌄ 折叠开关展开。
                replace_mode: false,
                matcher: SearchMatcher::new(),
                open: true,
                _subscriptions,
            }
        })
    }

    pub(crate) fn show(
        &mut self,
        selected_text: &Rope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open = true;
        self.search_input
            .read(cx)
            .focus_handle
            .clone()
            .focus(window, cx);

        self.search_input.update(cx, |this, cx| {
            if selected_text.len() > 0 {
                // Set value will emit to update_search_query
                this.set_value(selected_text.to_string(), window, cx);
            }
            this.select_all(&super::SelectAll, window, cx);
        });

        // 打开时定位到锚点（视口顶部 / 光标）之后的第一个匹配并选中。
        // 注意：on_action_search 发生在编辑器 InputState 自身的 update 借用内，
        // 此处不能同步 read/update 编辑器（重入借用会 panic），必须延迟执行。
        let editor = self.editor.clone();
        cx.defer_in(window, move |this, window, cx| {
            let start = editor.read(cx).anchor_offset();
            if let Some(start) = start {
                this.matcher.update_cursor_by_offset(start);
            } else {
                this.matcher.current_match_ix = 0;
            }
            if let Some(range) = this
                .matcher
                .matched_ranges
                .get(this.matcher.current_match_ix)
                .cloned()
            {
                editor.update(cx, |state, cx| {
                    state.reveal_match(range, window, cx);
                });
            }
        });
    }

    fn update_search_query(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().to_string();
        let visible_range_offset = self.editor.read(cx).anchor_offset();

        self.matcher.update_query_ext(
            &query,
            self.case_insensitive,
            self.use_regex,
            self.whole_word,
        );
        // 非法正则：query 被置空，标记错误状态供 UI 提示
        self.query_invalid = !query.is_empty() && self.matcher.query.is_none();

        if let Some(start) = visible_range_offset {
            self.matcher.update_cursor_by_offset(start);
        }
        cx.notify();
    }

    /// 将编辑器选区设置到当前匹配并滚动到位（不抢焦点）。
    ///
    /// 统一经 defer 执行：本方法可能在编辑器自身 update 借用内被调用
    /// （如 Cmd+F 打开路径），同步触碰编辑器会触发重入借用 panic。
    /// 将编辑器选区设置到当前匹配并滚动到视口中部（不抢焦点）。
    ///
    /// 同步执行：调用方（‹/› 按钮、Enter/Shift+Enter 动作）都在事件分发
    /// 阶段运行，不存在编辑器 update 借用；延迟一帧反而让跳转看起来迟钝。
    /// （Cmd+F 打开路径的首次定位在 show() 自身的 defer 中处理。）
    fn reveal_current_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self
            .matcher
            .matched_ranges
            .get(self.matcher.current_match_ix)
            .cloned()
        else {
            return;
        };
        let editor = self.editor.clone();
        editor.update(cx, |state, cx| {
            state.reveal_match(range, window, cx);
        });
    }

    /// 按 dir（+1 下一个 / -1 上一个）跳转：以编辑器光标为锚点对齐当前索引后 ±1（循环）。
    pub(crate) fn jump(&mut self, dir: i32, window: &mut Window, cx: &mut Context<Self>) {
        let caret = self.editor.read(cx).search_caret();
        if let Some(target) = jump_target(&self.matcher.matched_ranges, caret, dir) {
            self.matcher.current_match_ix = target;
            self.reveal_current_match(window, cx);
            cx.notify();
        }
    }

    /// 文档变化时同步全文（宿主每次编辑后调用，`InputState::update_search` parity）。
    pub(crate) fn sync_text(&mut self, text: &Rope) {
        self.matcher.update(text);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// 查找/替换输入框是否持有焦点（此时宿主回车等快捷键应让行）。
    pub(crate) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.open
            && [&self.search_input, &self.replace_input]
                .into_iter()
                .any(|input| input.read(cx).focus_handle.is_focused(window))
    }

    fn replaceable(&self, cx: &App) -> bool {
        self.editor.read(cx).search_replaceable()
    }

    pub(crate) fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        let handle = self.editor.read(cx).target_focus_handle(cx);
        handle.focus(window, cx);
        cx.notify();
    }

    fn on_action_enter(&mut self, _action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        self.jump(1, window, cx);
    }

    pub(super) fn on_action_search_previous(
        &mut self,
        _: &super::SearchPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.jump(-1, window, cx);
    }

    fn on_action_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.hide(window, cx);
    }

    fn on_action_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.editor.read(cx).target_focus_handle(cx);
        handle.focus(window, cx);
    }

    // prev/next 已由 jump(±1) 统一实现（见 on_action_enter / 按钮回调）

    pub(crate) fn matcher(&self) -> Option<&SearchMatcher> {
        if !self.open {
            return None;
        }

        Some(&self.matcher)
    }

    fn replace_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.replace_mode = false;
            cx.notify();
            return;
        }

        let new_text = self.replace_input.read(cx).value().to_string();
        let preserve_case = self.preserve_case;
        self.matcher.replacing = true;
        let previous_match_ix = self.matcher.current_match_ix;
        if let Some(range) = self
            .matcher
            .matched_ranges
            .get(self.matcher.current_match_ix)
            .cloned()
        {
            let content = self.editor.read(cx).search_text();
            let matched = content.slice(range.clone()).to_string();
            let replacement = apply_case_preservation(&new_text, &matched, preserve_case);
            let text_state = self.editor.clone();
            let next_match_ix = self.matcher.next_ix().unwrap_or(previous_match_ix);
            let next_range = self.matcher.peek().unwrap_or(range.clone());
            self.matcher.current_match_ix = next_match_ix;
            let direction = Self::next_scroll_direction(previous_match_ix, next_match_ix);
            cx.spawn_in(window, async move |_, cx| {
                cx.update(|window, cx| {
                    text_state.update(cx, |state, cx| {
                        state.replace_match(range, replacement.as_str(), window, cx);
                        state.scroll_to_offset(next_range.end, direction, cx);
                    });
                })
            })
            .detach();
        }
    }

    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.replace_mode = false;
            cx.notify();
            return;
        }

        let new_text = self.replace_input.read(cx).value().to_string();
        let preserve_case = self.preserve_case;
        self.matcher.replacing = true;
        let ranges = self.matcher.matched_ranges.clone();
        if ranges.is_empty() {
            return;
        }
        // 逐匹配算好替换串（含保留大小写），宿主只做一次批量替换。
        let content = self.editor.read(cx).search_text();
        let pairs: Vec<(Range<usize>, String)> = ranges
            .iter()
            .map(|range| {
                let matched = content.slice(range.clone()).to_string();
                (
                    range.clone(),
                    apply_case_preservation(&new_text, &matched, preserve_case),
                )
            })
            .collect();

        let editor = self.editor.clone();
        cx.spawn_in(window, async move |_, cx| {
            cx.update(|window, cx| {
                editor.update(cx, |state, cx| {
                    state.replace_all_matches(&pairs, window, cx);
                });
            })
        })
        .detach();
    }
}

impl<T: SearchTarget> Focusable for SearchPanel<T> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.read(cx).focus_handle.clone()
    }
}

impl<T: SearchTarget> Render for SearchPanel<T> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }

        let has_matches = self.matcher.len() > 0;
        let allow_replace = self.replaceable(cx);
        let replace_visible = self.replace_mode && allow_replace;
        let query_empty = self.search_input.read(cx).value().is_empty();

        // ---- 主题色（Copy，提前拷出避免后续 &mut cx 借用冲突）。
        // 卡片/输入框底色从编辑器背景推导：卡片比编辑器略深、输入框比卡片
        // 略浅，保证在任何主题下都有截图中的层次感。
        let dark = cx.theme().appearance.is_dark();
        let shade = |mut c: Hsla, dl: f32| -> Hsla {
            c.l = (c.l + dl).clamp(0.0, 1.0);
            c
        };
        let editor_bg = cx.theme().editor_background();
        let c_card = shade(editor_bg, if dark { -0.035 } else { 0.04 });
        let c_field = shade(c_card, if dark { 0.06 } else { -0.05 });
        let c_accent = cx.theme().accent;
        let c_accent_hover = cx.theme().accent_hover;
        let c_accent_fg = cx.theme().accent_foreground;
        let c_hover = cx.theme().ui.tab_hover_background;
        let c_border = cx.theme().border;
        let c_text_muted = cx.theme().text_muted;
        let c_text_subtle = cx.theme().text_subtle;
        let c_error = cx.theme().error;

        // 输入框聚焦态：accent 描边（对应截图中紫色边框的查找输入框）。
        let find_focused = self.search_input.read(cx).focus_handle.is_focused(window);
        let replace_focused = self.replace_input.read(cx).focus_handle.is_focused(window);

        // 开关按钮工厂：激活态 accent 实心底 + 前景色，非激活透明底灰字。
        // 标签用 child 而非 label，以便控制字号（xsmall 档默认只有 10px）。
        let toggle = |id: &'static str,
                      label: &'static str,
                      active: bool,
                      tooltip: &'static str,
                      cx: &mut Context<Self>| {
            Button::new(id)
                .child(div().text_size(px(12.0)).child(label))
                .xsmall()
                .tooltip(tooltip)
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(if active {
                            c_accent
                        } else {
                            gpui::transparent_black()
                        })
                        .foreground(if active { c_accent_fg } else { c_text_muted })
                        // 激活态悬停时保持选中色（用 accent_hover 提供反馈），
                        // 而不是被灰色悬停背景覆盖。
                        .hover(if active { c_accent_hover } else { c_hover })
                        .active(c_accent),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    match id {
                        "search-case" => this.case_insensitive = !this.case_insensitive,
                        "search-word" => this.whole_word = !this.whole_word,
                        "search-regex" => this.use_regex = !this.use_regex,
                        "search-preserve-case" => this.preserve_case = !this.preserve_case,
                        _ => {}
                    }
                    if id != "search-preserve-case" {
                        this.update_search_query(cx);
                    }
                    // 焦点回到开关所在行的输入框（保留大小写位于替换行）。
                    let focus_handle = match id {
                        "search-preserve-case" => this.replace_input.read(cx).focus_handle.clone(),
                        _ => this.search_input.read(cx).focus_handle.clone(),
                    };
                    window.focus(&focus_handle, cx);
                    cx.notify();
                }))
        };

        // 图标按钮统一规格：24×24 按钮 + 13.5px 图标（16 网格、描边 2），
        // 全套图标同网格同比重，视觉大小一致；颜色用主题默认图标色。
        let icon_btn = |id: &'static str,
                        path: &'static str,
                        tooltip: &'static str,
                        cx: &mut Context<Self>| {
            Button::new(id)
                .icon(Icon::new(path))
                .with_size(Size::Size(px(18.0)))
                .height(px(24.0))
                .tooltip(tooltip)
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(gpui::transparent_black())
                        .hover(c_hover),
                )
        };

        // ---- 计数标签：非法正则红色提示 / 无结果 / 第X项，共Y项。
        let counter: AnyElement = if self.query_invalid && !query_empty {
            div()
                .px(px(4.0))
                .text_size(px(12.0))
                .whitespace_nowrap()
                .text_color(c_error)
                .child("正则无效")
                .into_any_element()
        } else if query_empty {
            div().into_any_element()
        } else if !has_matches {
            div()
                .px(px(4.0))
                .text_size(px(12.0))
                .whitespace_nowrap()
                .text_color(c_text_subtle)
                .child(self.matcher.label())
                .into_any_element()
        } else {
            div()
                .px(px(4.0))
                .text_size(px(12.0))
                .whitespace_nowrap()
                .text_color(c_text_muted)
                .child(self.matcher.label())
                .into_any_element()
        };

        // 两行输入框共用的外壳样式：同一固定宽度（空间不足时等比收缩）、
        // 圆角底色、聚焦 accent 描边，保证上下两行右对齐。
        let input_shell = |focused: bool| {
            div()
                .w(px(344.0))
                .flex_shrink(1.0)
                .min_w(px(200.0))
                .h(px(28.0))
                .pl(px(8.0))
                .pr(px(3.0))
                .flex()
                .items_center()
                .gap(px(1.0))
                .rounded(px(7.0))
                .bg(c_field)
                .border_1()
                .border_color(if focused { c_accent } else { c_border })
                .overflow_hidden()
        };

        // ---- 查找行：内嵌开关的查找输入框 + 计数 + 导航 + 关闭。
        let find_field = input_shell(find_focused)
            .child(EditorInput::new(&self.search_input).transparent().small())
            .child(toggle(
                "search-case",
                "Aa",
                !self.case_insensitive,
                "区分大小写",
                cx,
            ))
            .child(toggle("search-word", "ab", self.whole_word, "全字匹配", cx))
            .child(toggle(
                "search-regex",
                ".*",
                self.use_regex,
                "正则表达式",
                cx,
            ));

        let find_row = div()
            .flex()
            .items_center()
            .w_full()
            .gap(px(4.0))
            .child(find_field)
            .child(counter)
            // 弹性空白：把导航/关闭按钮推到卡片右侧。
            .child(div().flex_1().min_w(px(2.0)))
            .child(
                icon_btn(
                    "search-prev",
                    "icons/search-chevron-left.svg",
                    "上一个 (Shift+Enter)",
                    cx,
                )
                .disabled(!has_matches)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.jump(-1, window, cx);
                })),
            )
            .child(
                icon_btn(
                    "search-next",
                    "icons/search-chevron-right.svg",
                    "下一个 (Enter)",
                    cx,
                )
                .disabled(!has_matches)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.jump(1, window, cx);
                })),
            )
            .child(
                icon_btn("search-close", "icons/search-xmark.svg", "关闭 (Esc)", cx).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.on_action_escape(&Escape, window, cx);
                    }),
                ),
            );

        // ---- 替换行：内嵌保留大小写开关的替换输入框 + 替换当前/全部替换。
        let preserve_toggle = toggle(
            "search-preserve-case",
            "Aa",
            self.preserve_case,
            "替换时保留大小写",
            cx,
        );
        let replace_field = input_shell(replace_focused)
            .child(EditorInput::new(&self.replace_input).transparent().small())
            .child(preserve_toggle);

        let replace_row = div()
            .flex()
            .items_center()
            .w_full()
            .gap(px(4.0))
            .child(replace_field)
            .child(
                icon_btn(
                    "search-replace-one",
                    "icons/search-replace.svg",
                    "替换当前",
                    cx,
                )
                .disabled(!has_matches)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.replace_next(window, cx);
                })),
            )
            .child(
                icon_btn(
                    "search-replace-all",
                    "icons/search-replace-all.svg",
                    "全部替换",
                    cx,
                )
                .disabled(!has_matches)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.replace_all(window, cx);
                })),
            );

        // ---- 左侧折叠开关（⌄ / ›）：细长竖条，高度为两行整体的 90%，
        // 垂直居中；不用 Button（其尺寸表会强制方形），直接用 div 实现。
        let disclosure = div()
            .id("search-toggle-replace")
            .w(px(24.0))
            .h(relative(0.9))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .when(allow_replace, |this| {
                this.hover(|this| this.bg(c_hover))
                    .active(|this| this.bg(c_hover))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.replace_mode = !this.replace_mode;
                        let focus_handle = if this.replace_mode {
                            this.replace_input.read(cx).focus_handle.clone()
                        } else {
                            this.search_input.read(cx).focus_handle.clone()
                        };
                        window.focus(&focus_handle, cx);
                        cx.notify();
                    }))
            })
            .when(!allow_replace, |this| this.opacity(0.4))
            .child(
                Icon::new(if replace_visible {
                    "icons/search-chevron-down.svg"
                } else {
                    "icons/search-chevron-right.svg"
                })
                .size(Size::Size(px(13.5))),
            );

        let rows = div()
            .flex()
            .w_full()
            // 折叠开关沟槽：从卡片左边界到输入框左边界（负外边距抵消
            // 卡片内边距），开关在此区间内严格水平居中。
            .child(
                div()
                    .ml(px(-10.0))
                    .w(px(33.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(disclosure),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap(px(6.0))
                    .child(find_row)
                    .when(replace_visible, |this| this.child(replace_row)),
            );

        div()
            .id("search-panel")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            // 阻断鼠标事件冒泡：面板悬浮在编辑器内部，若不拦截，点击面板
            // （含 ‹/› 按钮）会同时触发编辑器的 on_mouse_down，把编辑器光标
            // 移到面板对应的坐标处并引发竞争滚动，导致跳转错乱/延迟。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_action(cx.listener(Self::on_action_enter))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_search_previous))
            .w(px(550.0))
            .max_w_full()
            .rounded(px(8.0))
            .bg(c_card)
            .shadow_lg()
            .px(px(10.0))
            .py(px(8.0))
            .child(rows)
            .into_any_element()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true, false);

        assert_eq!(matcher.len(), 3);
        let mut matches = matcher.clone();
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next(), Some(18..20));
        assert_eq!(matches.next(), Some(23..25));
        assert_eq!(matches.current_match_ix, 2);
        assert_eq!(matches.next(), Some(15..17));
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next_back(), Some(23..25));
        assert_eq!(matches.current_match_ix, 2);
        assert_eq!(matches.next_back(), Some(18..20));
        assert_eq!(matches.current_match_ix, 1);
        assert_eq!(matches.next_back(), Some(15..17));
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next_back(), Some(23..25));

        matcher.update_query("IS", false, false);
        assert_eq!(matcher.len(), 0);
        assert_eq!(matcher.next(), None);
        assert_eq!(matcher.next_back(), None);
    }

    #[test]
    fn test_search_label() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true, false);
        assert_eq!(matcher.label(), "第1项，共3项");
        matcher.next();
        assert_eq!(matcher.label(), "第2项，共3项");
        matcher.next();
        assert_eq!(matcher.label(), "第3项，共3项");
        matcher.next();
        assert_eq!(matcher.label(), "第1项，共3项");

        matcher.update_query("IS", false, false);
        assert_eq!(matcher.label(), "无结果");
    }

    #[test]
    fn test_select_range_start() {
        let mut matcher = SearchMatcher::new();
        matcher.matched_ranges = Rc::new(vec![5..10, 15..20, 25..30]);
        matcher.update_cursor_by_offset(0);
        assert_eq!(matcher.current_match_ix, 0);

        matcher.update_cursor_by_offset(5);
        assert_eq!(matcher.current_match_ix, 0);

        matcher.update_cursor_by_offset(12);
        assert_eq!(matcher.current_match_ix, 1);

        matcher.update_cursor_by_offset(16);
        assert_eq!(matcher.current_match_ix, 1);

        matcher.update_cursor_by_offset(30);
        assert_eq!(matcher.current_match_ix, 2);

        matcher.update_cursor_by_offset(31);
        assert_eq!(matcher.current_match_ix, 2);
    }

    #[test]
    fn test_next_ix_wraps_to_start() {
        let mut matcher = SearchMatcher::new();
        matcher.matched_ranges = Rc::new(vec![5..10, 15..20, 25..30]);
        matcher.current_match_ix = 2;

        assert_eq!(matcher.next_ix(), Some(0));
    }

    #[test]
    fn test_update_matches_clamps_current_match_index_while_replacing() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo foo foo"));
        matcher.update_query("foo", true, false);
        matcher.current_match_ix = 2;
        matcher.replacing = true;

        matcher.update(&Rope::from("foo xoo foo"));

        assert_eq!(matcher.len(), 2);
        assert_eq!(matcher.current_match_ix, 1);
        assert_eq!(matcher.label(), "第2项，共2项");
        assert!(!matcher.replacing);
    }
}

#[cfg(test)]
mod extended_tests {
    use super::*;

    #[test]
    fn whole_word_literal_matches_boundaries_only() {
        let mut m = SearchMatcher::new();
        let text = Rope::from("cat catalog concat cat");
        m.update(&text);
        m.update_query_ext("cat", false, false, true);
        // 仅独立单词 "cat"（首尾两处），不含 catalog/concat
        assert_eq!(m.len(), 2);
        assert_eq!(m.matched_ranges.as_ref()[0], 0..3);
        assert_eq!(m.matched_ranges.as_ref()[1], 19..22);
    }

    #[test]
    fn whole_word_case_insensitive() {
        let mut m = SearchMatcher::new();
        m.update(&Rope::from("Foo foo FOO foobar"));
        m.update_query_ext("foo", true, false, true);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn invalid_regex_yields_no_query() {
        let mut m = SearchMatcher::new();
        m.update(&Rope::from("hello"));
        m.update_query_ext("(unclosed", false, true, false);
        assert!(m.query.is_none());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn update_cursor_by_offset_finds_match_at_or_after() {
        let mut m = SearchMatcher::new();
        m.matched_ranges = Rc::new(vec![5..10, 15..20]);
        m.update_cursor_by_offset(12); // 落在两匹配之间 → 指向下一个
        assert_eq!(m.current_match_ix, 1);
        m.update_cursor_by_offset(6); // 命中第一个匹配内部
        assert_eq!(m.current_match_ix, 0);
    }
}

#[cfg(test)]
mod case_preservation_tests {
    use super::apply_case_preservation;

    #[test]
    fn disabled_passthrough() {
        assert_eq!(apply_case_preservation("bar", "Foo", false), "bar");
    }

    #[test]
    fn all_uppercase_match_yields_uppercase_replacement() {
        assert_eq!(apply_case_preservation("bar", "FOO", true), "BAR");
        // 无大写字母的匹配保持原样
        assert_eq!(apply_case_preservation("bar", "foo", true), "bar");
    }

    #[test]
    fn capitalized_match_capitalizes_first_letter() {
        assert_eq!(apply_case_preservation("bar", "Foo", true), "Bar");
        assert_eq!(apply_case_preservation("bar_baz", "Foo", true), "Bar_baz");
    }

    #[test]
    fn mixed_or_non_alpha_match_passthrough() {
        assert_eq!(apply_case_preservation("bar", "fOo", true), "bar");
        assert_eq!(apply_case_preservation("bar", "123", true), "bar");
    }

    #[test]
    fn empty_inputs_passthrough() {
        assert_eq!(apply_case_preservation("", "FOO", true), "");
        assert_eq!(apply_case_preservation("bar", "", true), "bar");
    }

    #[test]
    fn multibyte_replacement() {
        assert_eq!(apply_case_preservation("héllo", "WÖRD", true), "HÉLLO");
    }
}

#[cfg(test)]
mod jump_target_tests {
    use super::jump_target;

    // 匹配：M0=0..2, M1=5..7, M2=10..12, M3=15..17
    const RANGES: &[std::ops::Range<usize>] = &[0..2, 5..7, 10..12, 15..17];

    #[test]
    fn forward_walks_all_matches_then_wraps() {
        // caret=0 在 M0 内 → 「下一个」是 M1；依次走到末尾后回绕到 M0
        assert_eq!(jump_target(RANGES, 0, 1), Some(1));
        assert_eq!(jump_target(RANGES, 5, 1), Some(2));
        assert_eq!(jump_target(RANGES, 10, 1), Some(3));
        assert_eq!(jump_target(RANGES, 15, 1), Some(0)); // 回绕
    }

    #[test]
    fn backward_from_match_start_goes_to_previous_match() {
        // 关键回归用例：跳到某匹配后选区在其起点，此时按上一个必须
        // 前移而不是停留在原匹配（旧实现会卡死在同一项）。
        assert_eq!(jump_target(RANGES, 5, -1), Some(0)); // 在 M1 起点 → M0
        assert_eq!(jump_target(RANGES, 10, -1), Some(1));
        assert_eq!(jump_target(RANGES, 15, -1), Some(2));
    }

    #[test]
    fn backward_wraps_from_first_to_last() {
        assert_eq!(jump_target(RANGES, 0, -1), Some(3)); // 在 M0 起点 → 最后一个
        assert_eq!(jump_target(RANGES, 1, -1), Some(3)); // 在 M0 内部任意位置同理
    }

    #[test]
    fn anchors_by_caret_when_outside_any_match() {
        // caret 位于两个匹配之间：下一个 = 后面那个；上一个 = 前面那个
        assert_eq!(jump_target(RANGES, 8, 1), Some(2));
        assert_eq!(jump_target(RANGES, 8, -1), Some(1));
        // caret 在所有匹配之前 / 之后
        assert_eq!(jump_target(RANGES, 20, 1), Some(0));
        assert_eq!(jump_target(RANGES, 20, -1), Some(3));
    }

    #[test]
    fn empty_ranges_yields_none() {
        assert_eq!(jump_target(&[], 0, 1), None);
        assert_eq!(jump_target(&[], 0, -1), None);
    }
}
