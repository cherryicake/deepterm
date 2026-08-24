//! Search panel for code editors: find / replace UI plus match navigation.
//!
//! Ported from gpui-component's `input/search.rs`, adapted to deepterm's
//! component set (no `Label` / `IconName` / flex helpers).

use std::{ops::Range, rc::Rc};

use aho_corasick::AhoCorasick;
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, actions, div, px,
};
use ropey::Rope;

use crate::components::button::{Button, ButtonCustomVariant, ButtonVariants, Disableable, Sizable};
use crate::theme::ActiveTheme;

use super::{
    EditorInput, Enter, Escape, IndentInline, InputEvent, InputState, RopeExt as _, Search,
    SearchPrevious, movement::MoveDirection,
};

const CONTEXT: &'static str = "SearchPanel";

actions!(input, [Tab]);

/// The active search query: a literal (aho-corasick) or a regex.
#[derive(Debug, Clone)]
enum SearchQuery {
    Literal(AhoCorasick),
    Regex(regex::Regex),
}

#[derive(Debug, Clone)]
pub struct SearchMatcher {
    text: Rope,
    pub query: Option<SearchQuery>,

    pub(super) matched_ranges: Rc<Vec<Range<usize>>>,
    pub(super) current_match_ix: usize,
    /// Is in replacing mode, if true, the next update will update the current
    /// match index based on matched ranges.
    replacing: bool,
}

impl SearchMatcher {
    pub fn new() -> Self {
        Self {
            text: "".into(),
            query: None,
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
        if let Some(query) = &self.query {
            let text = self.text.to_string();
            match query {
                SearchQuery::Literal(matcher) => {
                    for query_match in matcher.stream_find_iter(text.as_bytes()) {
                        let query_match = query_match.expect("query match");
                        new_ranges.push(query_match.range());
                    }
                }
                SearchQuery::Regex(re) => {
                    for m in re.find_iter(&text) {
                        new_ranges.push(m.range());
                    }
                }
            }
        }
        self.matched_ranges = Rc::new(new_ranges);
        if !self.replacing {
            self.current_match_ix = 0;
        } else if self.matched_ranges.is_empty() {
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

    /// 扩展版：支持全字匹配（literal 转为 \b 包裹正则）。
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
        } else if use_regex || whole_word {
            let body = if use_regex {
                query.to_string()
            } else {
                regex::escape(query)
            };
            let expr = if whole_word {
                format!(r"\b(?:{})\b", body)
            } else {
                body
            };
            let expr = if case_insensitive {
                format!("(?i){expr}")
            } else {
                expr
            };
            self.query = regex::Regex::new(&expr).ok().map(SearchQuery::Regex);
        } else {
            self.query = Some(SearchQuery::Literal(
                AhoCorasick::builder()
                    .ascii_case_insensitive(case_insensitive)
                    .build(&[query.to_string()])
                    .expect("failed to build AhoCorasick query in SearchMatcher"),
            ));
        }
        self.update_matches();
    }

    /// Returns the number of matches found.
    #[allow(unused)]
    #[inline]
    fn len(&self) -> usize {
        self.matched_ranges.len()
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
            return "0/0".to_string();
        }
        format!("{}/{}", self.current_match_ix + 1, self.len())
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

pub(crate) struct SearchPanel {
    editor: Entity<InputState>,
    search_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    case_insensitive: bool,
    use_regex: bool,
    /// 全字匹配（仅 literal 模式；内部转为 \b 包裹的正则）。
    whole_word: bool,
    /// 当前 query 为非法正则：UI 显示红色错误提示而非计数。
    query_invalid: bool,
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

    pub(super) fn on_action_search_previous(
        &mut self,
        _: &SearchPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.searchable {
            return;
        }
        if let Some(panel) = self.search_panel.as_ref() {
            panel.update(cx, |this, cx| this.jump(-1, window, cx));
        }
    }
}

impl SearchPanel {
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

    fn prev_scroll_direction(
        previous_match_ix: usize,
        current_match_ix: usize,
    ) -> Option<MoveDirection> {
        if current_match_ix >= previous_match_ix {
            None
        } else {
            Some(MoveDirection::Up)
        }
    }

    pub fn new(editor: Entity<InputState>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let replace_input = cx.new(|cx| InputState::new(window, cx));

        cx.new(|cx| {
            let _subscriptions = vec![cx.subscribe(&search_input, |this: &mut Self, _, ev, cx| {
                match ev {
                    InputEvent::Change => {
                        this.update_search_query(cx);
                    }
                    _ => {}
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

        // 打开时定位到视口顶部之后的第一个匹配并选中。
        // 注意：on_action_search 发生在编辑器 InputState 自身的 update 借用内，
        // 此处不能同步 read/update 编辑器（重入借用会 panic），必须延迟执行。
        let editor = self.editor.clone();
        cx.defer_in(window, move |this, _window, cx| {
            let visible_start = editor
                .read(cx)
                .last_layout
                .as_ref()
                .map(|l| l.visible_range_offset.start);
            if let Some(start) = visible_start {
                this.matcher.update_cursor_by_offset(start);
            } else {
                this.matcher.current_match_ix = 0;
            }
            if let Some(range) =
                this.matcher.matched_ranges.get(this.matcher.current_match_ix).cloned()
            {
                editor.update(cx, |state, cx| {
                    state.selected_range = range.clone().into();
                    state.scroll_to(range.start, None, cx);
                });
            }
        });
    }

    fn update_search_query(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().to_string();
        let visible_range_offset = self
            .editor
            .read(cx)
            .last_layout
            .as_ref()
            .map(|l| l.visible_range_offset.clone());

        self.matcher.update_query_ext(
            &query,
            self.case_insensitive,
            self.use_regex,
            self.whole_word,
        );
        // 非法正则：query 被置空，标记错误状态供 UI 提示
        self.query_invalid = !query.is_empty() && self.matcher.query.is_none();

        if let Some(visible_range_offset) = visible_range_offset {
            self.matcher
                .update_cursor_by_offset(visible_range_offset.start);
        }
        cx.notify();
    }

    /// 将编辑器选区设置到当前匹配并滚动到位（不抢焦点）。
    ///
    /// 统一经 defer 执行：本方法可能在编辑器自身 update 借用内被调用
    /// （如 Cmd+F 打开路径），同步触碰编辑器会触发重入借用 panic。
    fn reveal_current_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.matcher.matched_ranges.get(self.matcher.current_match_ix).cloned()
        else {
            return;
        };
        let editor = self.editor.clone();
        cx.defer_in(window, move |_, _window, cx| {
            editor.update(cx, |state, cx| {
                state.selected_range = range.clone().into();
                state.scroll_to(range.start, None, cx);
            });
        });
    }

    /// 按 dir（+1 下一个 / -1 上一个）跳转：以编辑器光标为锚点对齐当前索引后 ±1（循环）。
    fn jump(&mut self, dir: i32, _window: &mut Window, cx: &mut Context<Self>) {
        if self.matcher.matched_ranges.is_empty() {
            return;
        }
        // 以光标位置为锚：确保 current 指向光标处或之后的第一个匹配
        let caret = self.editor.read(cx).selected_range.start;
        self.matcher.update_cursor_by_offset(caret);

        let len = self.matcher.matched_ranges.len();
        let cur = self.matcher.current_match_ix;
        self.matcher.current_match_ix = match dir {
            1 => {
                // 光标不在任何匹配内时，第一个命中就是「下一个」
                let at_match = self.matcher.matched_ranges[cur].contains(&caret);
                if at_match {
                    (cur + 1) % len
                } else {
                    cur
                }
            }
            -1 => {
                let at_match = self.matcher.matched_ranges[cur].start < caret;
                if at_match {
                    cur.checked_sub(1).unwrap_or(len - 1)
                } else {
                    cur
                }
            }
            _ => cur,
        };
        self.reveal_current_match(_window, cx);
        cx.notify();
    }

    fn replaceable(&self, cx: &App) -> bool {
        let editor = self.editor.read(cx);
        editor.replaceable
    }

    pub(crate) fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.editor.read(cx).focus_handle.clone().focus(window, cx);
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
        self.editor.focus_handle(cx).focus(window, cx);
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

        let new_text = self.replace_input.read(cx).value();
        self.matcher.replacing = true;
        let previous_match_ix = self.matcher.current_match_ix;
        if let Some(range) = self
            .matcher
            .matched_ranges
            .get(self.matcher.current_match_ix)
            .cloned()
        {
            let text_state = self.editor.clone();
            let next_match_ix = self.matcher.next_ix().unwrap_or(previous_match_ix);
            let next_range = self.matcher.peek().unwrap_or(range.clone());
            self.matcher.current_match_ix = next_match_ix;
            let direction = Self::next_scroll_direction(previous_match_ix, next_match_ix);
            cx.spawn_in(window, async move |_, cx| {
                cx.update(|window, cx| {
                    text_state.update(cx, |state, cx| {
                        let range_utf16 = state.range_to_utf16(&range);
                        state.scroll_to(next_range.end, direction, cx);
                        state.replace_text_in_range_silent(
                            Some(range_utf16),
                            new_text.as_str(),
                            window,
                            cx,
                        );
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

        let new_text = self.replace_input.read(cx).value();
        self.matcher.replacing = true;
        let ranges = self.matcher.matched_ranges.clone();
        if ranges.is_empty() {
            return;
        }

        let editor = self.editor.clone();
        cx.spawn_in(window, async move |_, cx| {
            cx.update(|window, cx| {
                editor.update(cx, |state, cx| {
                    // Replace from the end to avoid messing up the ranges.
                    let mut rope = state.text.clone();
                    for range in ranges.iter().rev() {
                        rope.replace(range.clone(), new_text.as_str());
                    }
                    state.replace_text_in_range_silent(
                        Some(0..state.text.len()),
                        &rope.to_string(),
                        window,
                        cx,
                    );
                    state.scroll_to(0, Some(MoveDirection::Down), cx);
                });
            })
        })
        .detach();
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.read(cx).focus_handle.clone()
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }

        let has_matches = self.matcher.len() > 0;
        let allow_replace = self.replaceable(cx);
        if !allow_replace {
            self.replace_mode = false;
        }
        // 主题色为 Copy，提前拷出避免与后续 &mut cx 借用冲突
        let c_accent = cx.theme().accent;
        let c_accent_fg = cx.theme().accent_foreground;
        let c_elevated = cx.theme().ui.elevated_background;
        let c_hover = cx.theme().ui.tab_hover_background;
        let c_border = cx.theme().border;
        let c_panel = cx.theme().panel_background;
        let c_overlay = cx.theme().ui.overlay_background;
        let c_text = cx.theme().text;
        let c_text_muted = cx.theme().text_muted;
        let c_text_subtle = cx.theme().text_subtle;
        let c_error = cx.theme().error;
        let matcher_label = self.matcher.label();

        // 开关按钮工厂：激活态 accent 底 + 前景色，非激活 ghost。
        let toggle = |
            id: &'static str,
            label: &'static str,
            active: bool,
            tooltip: &'static str,
            cx: &mut Context<Self>,| {
            Button::new(id)
                .label(label)
                .compact()
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(if active { c_accent } else { c_elevated })
                        .foreground(if active { c_accent_fg } else { c_text_muted })
                        .hover(c_hover)
                        .active(c_accent),
                )
                .tooltip(tooltip)
                .on_click(cx.listener(move |this, _, _, cx| {
                    match id {
                        "search-case" => this.case_insensitive = !this.case_insensitive,
                        "search-word" => this.whole_word = !this.whole_word,
                        "search-regex" => this.use_regex = !this.use_regex,
                        _ => {}
                    }
                    this.update_search_query(cx);
                    cx.notify();
                }))
        };

        let case_toggle = toggle("search-case", "Aa", self.case_insensitive, "区分大小写", cx);
        let word_toggle = toggle("search-word", "W", self.whole_word, "全字匹配", cx);
        let regex_toggle = toggle("search-regex", ".*", self.use_regex, "正则表达式", cx);

        // 匹配计数：非法正则显示红色错误提示
        let counter = if self.query_invalid {
            div()
                .px(px(6.0))
                .text_size(px(11.0))
                .whitespace_nowrap()
                .text_color(c_error)
                .child("正则无效")
        } else {
            div()
                .px(px(6.0))
                .min_w(px(44.0))
                .flex()
                .justify_center()
                .text_size(px(11.0))
                .text_color(if has_matches { c_text_muted } else { c_text_subtle })
                .child(matcher_label)
        };

        let nav_button = |id: &'static str, label: &'static str, cx: &mut Context<Self>| {
            Button::new(id)
                .label(label)
                .compact()
                .disabled(!has_matches)
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(c_elevated)
                        .foreground(c_text)
                        .hover(c_hover)
                        .active(c_accent),
                )
        };

        let search_row = div().flex().items_center().gap(px(6.0)).child(
            div()
                .flex_1()
                .min_w(px(120.0))
                .h(px(26.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .bg(c_overlay)
                .border_1()
                .border_color(c_border)
                .child(
                    EditorInput::new(&self.search_input)
                        .transparent()
                        .small(),
                ),
        );

        let replace_row = div()
            .when(self.replace_mode && allow_replace, |this| this.mt_1())
            .flex()
            .items_center()
            .gap(px(6.0))
            .when(self.replace_mode && allow_replace, |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_w(px(120.0))
                        .h(px(26.0))
                        .px(px(8.0))
                        .rounded(px(6.0))
                        .bg(c_overlay)
                        .border_1()
                        .border_color(c_border)
                        .child(
                            EditorInput::new(&self.replace_input)
                                .transparent()
                                .small(),
                        ),
                )
                .child(
                    Button::new("search-replace-one")
                        .label("替换")
                        .small()
                        .disabled(!has_matches)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.replace_next(window, cx);
                        })),
                )
                .child(
                    Button::new("search-replace-all")
                        .label("全部替换")
                        .small()
                        .disabled(!has_matches)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.replace_all(window, cx);
                        })),
                )
            });

        div()
            .id("search-panel")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::on_action_enter))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_search_previous))
            .w_full()
            .child(
                // 卡片主体：悬浮于编辑器右上角（定位由外层 input.rs 提供）
                div()
                    .w(px(480.0))
                    .max_w_full()
                    .rounded(px(8.0))
                    .bg(c_panel)
                    .border_1()
                    .border_color(c_border)
                    .shadow_lg()
                    .px(px(10.0))
                    .py(px(8.0))
                    .child(
                        // 第一行：替换展开箭头 + 查找输入 + 计数 + 开关 + 导航 + 关闭
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                Button::new("search-toggle-replace")
                                    .label(if self.replace_mode { "⌄" } else { "›" })
                                    .compact()
                                    .tooltip("切换替换模式")
                                    .disabled(!allow_replace)
                                    .custom(
                                        ButtonCustomVariant::new(cx)
                                            .color(c_elevated)
                                            .foreground(c_text_muted)
                                            .hover(c_hover),
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.replace_mode = !this.replace_mode;
                                        if this.replace_mode {
                                            this.replace_input
                                                .read(cx)
                                                .focus_handle
                                                .clone()
                                                .focus(window, cx);
                                        } else {
                                            this.search_input
                                                .read(cx)
                                                .focus_handle
                                                .clone()
                                                .focus(window, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(search_row)
                            .child(counter)
                            .child(case_toggle)
                            .child(word_toggle)
                            .child(regex_toggle)
                            .child(
                                nav_button("search-prev", "‹", cx).on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.jump(-1, window, cx);
                                    },
                                )),
                            )
                            .child(
                                nav_button("search-next", "›", cx).on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.jump(1, window, cx);
                                    },
                                )),
                            )
                            .child(
                                Button::new("search-close")
                                    .label("✕")
                                    .compact()
                                    .custom(
                                        ButtonCustomVariant::new(cx)
                                            .color(c_elevated)
                                            .foreground(c_text_muted)
                                            .hover(c_hover)
                                            .active(c_error),
                                    )
                                    .tooltip("关闭 (Esc)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.on_action_escape(&Escape, window, cx);
                                    })),
                            ),
                    )
                    .child(replace_row),
            )
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
        assert_eq!(matcher.label(), "1/3");
        matcher.next();
        assert_eq!(matcher.label(), "2/3");
        matcher.next();
        assert_eq!(matcher.label(), "3/3");
        matcher.next();
        assert_eq!(matcher.label(), "1/3");

        matcher.update_query("IS", false, false);
        assert_eq!(matcher.label(), "0/0");
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
        assert_eq!(matcher.label(), "2/2");
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
