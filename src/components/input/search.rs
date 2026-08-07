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
    movement::MoveDirection,
};

const CONTEXT: &'static str = "SearchPanel";

actions!(input, [Tab]);

#[derive(Debug, Clone)]
pub struct SearchMatcher {
    text: Rope,
    pub query: Option<AhoCorasick>,

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
            let matches = query.stream_find_iter(text.as_bytes());

            for query_match in matches.into_iter() {
                let query_match = query_match.expect("query match for select all action");
                new_ranges.push(query_match.range());
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
    pub fn update_query(&mut self, query: &str, case_insensitive: bool) {
        if query.len() > 0 {
            self.query = Some(
                AhoCorasick::builder()
                    .ascii_case_insensitive(case_insensitive)
                    .build(&[query.to_string()])
                    .expect("failed to build AhoCorasick query in SearchMatcher"),
            );
        } else {
            self.query = None;
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
    }

    fn update_search_query(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value();
        let visible_range_offset = self
            .editor
            .read(cx)
            .last_layout
            .as_ref()
            .map(|l| l.visible_range_offset.clone());

        self.matcher
            .update_query(query.as_str(), self.case_insensitive);

        if let Some(visible_range_offset) = visible_range_offset {
            self.matcher
                .update_cursor_by_offset(visible_range_offset.start);
        }
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
        self.next(window, cx);
    }

    fn on_action_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.hide(window, cx);
    }

    fn on_action_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.focus_handle(cx).focus(window, cx);
    }

    fn prev(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let previous_match_ix = self.matcher.current_match_ix;
        if let Some(range) = self.matcher.next_back() {
            let direction =
                Self::prev_scroll_direction(previous_match_ix, self.matcher.current_match_ix);
            self.editor.update(cx, |state, cx| {
                state.scroll_to(range.start, direction, cx);
            });
        }
    }

    fn next(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let previous_match_ix = self.matcher.current_match_ix;
        if let Some(range) = self.matcher.next() {
            let direction =
                Self::next_scroll_direction(previous_match_ix, self.matcher.current_match_ix);
            self.editor.update(cx, |state, cx| {
                state.scroll_to(range.end, direction, cx);
            });
        }
    }

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
        let theme = cx.theme();
        let ui = cx.theme().ui;
        let matcher_label = self.matcher.label();

        // "Aa" toggle showing selected state via accent background.
        let case_toggle = Button::new("search-case-sensitive")
            .label("Aa")
            .compact()
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(if self.case_insensitive {
                        ui.elevated_background
                    } else {
                        theme.accent
                    })
                    .foreground(if self.case_insensitive {
                        theme.text
                    } else {
                        theme.accent_foreground
                    })
                    .hover(ui.tab_hover_background)
                    .active(theme.accent),
            )
            .tooltip("大小写匹配")
            .on_click(cx.listener(|this, _, _, cx| {
                this.case_insensitive = !this.case_insensitive;
                this.update_search_query(cx);
                cx.notify();
            }));

        let replace_toggle = Button::new("search-replace-mode")
            .label("替换")
            .compact()
            .custom(ButtonCustomVariant::new(cx).color(if self.replace_mode {
                theme.accent
            } else {
                ui.elevated_background
            }))
            .tooltip("切换替换模式")
            .on_click(cx.listener(|this, _, window, cx| {
                this.replace_mode = !this.replace_mode;
                if this.replace_mode {
                    this.replace_input.read(cx).focus_handle.clone().focus(window, cx);
                } else {
                    this.search_input.read(cx).focus_handle.clone().focus(window, cx);
                }
                cx.notify();
            }));

        let search_row = div()
            .id("search-panel")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::on_action_enter))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(theme.panel_background)
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex().flex_1().child(EditorInput::new(&self.search_input).transparent()))
            .when(allow_replace, |this| this.child(replace_toggle))
            .child(case_toggle)
            .child(
                Button::new("search-prev")
                    .label("‹")
                    .compact()
                    .disabled(!has_matches)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.prev(window, cx);
                    })),
            )
            .child(
                Button::new("search-next")
                    .label("›")
                    .compact()
                    .disabled(!has_matches)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.next(window, cx);
                    })),
            )
            .child(
                div()
                    .text_color(theme.text_muted)
                    .min_w(px(48.0))
                    .text_center()
                    .child(matcher_label),
            )
            .child(div().w(px(24.0)))
            .child(
                Button::new("search-close")
                    .label("✕")
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_action_escape(&Escape, window, cx);
                    })),
            );

        let replace_row = div()
            .id("search-replace-row")
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .pb_2()
            .child(div().flex().flex_1().child(EditorInput::new(&self.replace_input).transparent()))
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
            );

        div()
            .id("search-panel-root")
            .flex()
            .flex_col()
            .w_full()
            .child(search_row)
            .when(self.replace_mode && allow_replace, |this| {
                this.child(replace_row)
            })
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
        matcher.update_query("Is", true);

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

        matcher.update_query("IS", false);
        assert_eq!(matcher.len(), 0);
        assert_eq!(matcher.next(), None);
        assert_eq!(matcher.next_back(), None);
    }

    #[test]
    fn test_search_label() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true);
        assert_eq!(matcher.label(), "1/3");
        matcher.next();
        assert_eq!(matcher.label(), "2/3");
        matcher.next();
        assert_eq!(matcher.label(), "3/3");
        matcher.next();
        assert_eq!(matcher.label(), "1/3");

        matcher.update_query("IS", false);
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
        matcher.update_query("foo", true);
        matcher.current_match_ix = 2;
        matcher.replacing = true;

        matcher.update(&Rope::from("foo xoo foo"));

        assert_eq!(matcher.len(), 2);
        assert_eq!(matcher.current_match_ix, 1);
        assert_eq!(matcher.label(), "2/2");
        assert!(!matcher.replacing);
    }
}
