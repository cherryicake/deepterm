use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, Hsla, IntoElement, KeyBinding,
    ListAlignment, ListState, Render, SharedString, StyleRefinement, TextRun, TextStyleRefinement,
    WhiteSpace, Window, actions, div, list, px, rems,
};

use crate::editor::syntax::SyntaxHighlighter;
use crate::sidebar_view::font_settings::{
    platform_default_fallbacks, platform_default_font_family,
};
use crate::theme::{ActiveTheme, AppTheme};

use super::markdown_element::MarkdownElementBuilder;
use super::markdown_parser::{MarkdownEvent, parse_markdown};

/// 代码块高亮缓存：`(代码块源码, 源码在整篇文档中的字节范围) → TextRun 列表`。
///
/// 同一篇 Markdown 里的重复代码块可直接复用，避免每次渲染都重跑语法高亮。
pub(crate) type HighlightCache = Rc<RefCell<HashMap<(String, Range<usize>), Vec<TextRun>>>>;

#[derive(Clone)]
pub struct MarkdownStyle {
    pub base_text_style: gpui::TextStyle,
    pub container_style: StyleRefinement,
    pub code_block: StyleRefinement,
    pub inline_code: TextStyleRefinement,
    pub block_quote: TextStyleRefinement,
    pub link: TextStyleRefinement,
    pub rule_color: Hsla,
    pub block_quote_border_color: Hsla,
    pub heading: StyleRefinement,
    pub heading_level_styles: [Option<TextStyleRefinement>; 6],
    pub heading_border_color: Option<Hsla>,
}

impl MarkdownStyle {
    pub fn themed(window: &Window, cx: &gpui::App) -> Self {
        let theme = cx.theme();
        let ui = &theme.ui;
        let is_dark = theme.appearance.is_dark();

        // Typora / Notion 阅读体验：正文使用系统无衬线栈（非等宽），
        // 15px / 1.75 行高，段间距靠容器 margin 统一控制，
        // 参考 github-markdown-css + Notion 的柔和留白。
        let mut base_text_style = window.text_style();
        base_text_style.refine(&TextStyleRefinement {
            color: Some(ui.text),
            font_size: Some(px(15.).into()),
            line_height: Some(rems(1.75).into()),
            white_space: Some(WhiteSpace::Normal),
            ..Default::default()
        });

        // 代码块卡片：editor_background 背景，淡边框，圆角 8px
        let code_border = ui.border.opacity(if is_dark { 0.28 } else { 0.20 });
        let code_block = StyleRefinement {
            padding: gpui::EdgesRefinement {
                top: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(14.)),
                )),
                left: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(16.)),
                )),
                right: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(16.)),
                )),
                bottom: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(14.)),
                )),
            },
            margin: gpui::EdgesRefinement {
                top: Some(gpui::Length::Definite(px(16.).into())),
                left: Some(gpui::Length::Definite(px(0.).into())),
                right: Some(gpui::Length::Definite(px(0.).into())),
                bottom: Some(gpui::Length::Definite(px(20.).into())),
            },
            border_style: Some(gpui::BorderStyle::Solid),
            border_widths: gpui::EdgesRefinement {
                top: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                left: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                right: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                bottom: Some(gpui::AbsoluteLength::Pixels(px(1.))),
            },
            border_color: Some(code_border),
            background: Some(theme.editor_background().into()),
            text: TextStyleRefinement {
                font_family: Some(platform_default_font_family()),
                font_fallbacks: Some(platform_default_fallbacks()),
                font_size: Some(px(13.5).into()),
                line_height: Some(rems(1.6).into()),
                ..Default::default()
            },
            ..Default::default()
        };

        // 行内代码：淡底胶囊，亮 6% / 暗 12%
        let inline_code = TextStyleRefinement {
            font_family: Some(platform_default_font_family()),
            font_fallbacks: Some(platform_default_fallbacks()),
            font_size: Some(px(13.).into()),
            background_color: Some(ui.text.opacity(if is_dark { 0.12 } else { 0.06 })),
            ..Default::default()
        };

        let link = TextStyleRefinement {
            color: Some(ui.accent),
            underline: Some(gpui::UnderlineStyle {
                color: Some(ui.accent.opacity(0.35)),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 标题层级：逐级缩小，H1-H4 加粗，H5-H6 600，H1/H2 带分割线（行高 > 字号，避免重叠）
        let heading = |size: f32, weight: gpui::FontWeight, height: f32| TextStyleRefinement {
            font_size: Some(rems(size).into()),
            font_weight: Some(weight),
            line_height: Some(rems(height).into()),
            ..Default::default()
        };
        let mut h6 = heading(0.85, gpui::FontWeight(600.), 1.55);
        h6.color = Some(ui.text_muted);
        let heading_level_styles = [
            Some(heading(1.875, gpui::FontWeight::BOLD, 2.35)),
            Some(heading(1.5, gpui::FontWeight::BOLD, 1.95)),
            Some(heading(1.25, gpui::FontWeight::BOLD, 1.75)),
            Some(heading(1.08, gpui::FontWeight::BOLD, 1.55)),
            Some(heading(0.95, gpui::FontWeight(600.), 1.5)),
            Some(h6),
        ];

        // 引用边框：左重边框
        let block_quote_border_color = ui.border.opacity(if is_dark { 0.9 } else { 0.65 });
        Self {
            base_text_style,
            container_style: StyleRefinement::default(),
            code_block,
            inline_code,
            block_quote: TextStyleRefinement {
                color: Some(ui.text.opacity(0.78)),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
            link,
            rule_color: ui.border.opacity(if is_dark { 0.5 } else { 0.45 }),
            block_quote_border_color,
            heading: StyleRefinement {
                ..Default::default()
            },
            heading_level_styles,
            heading_border_color: Some(ui.border.opacity(if is_dark { 0.35 } else { 0.22 })),
        }
    }
}

actions!(markdown, [Copy, SelectAll]);

#[derive(Clone, Debug, Default)]
pub struct MarkdownSelection {
    pub anchor_fragment: usize,
    pub anchor_offset: usize,
    pub focus_fragment: usize,
    pub focus_offset: usize,
    pub pending: bool,
    pub has_selection: bool,
}

impl MarkdownSelection {
    pub fn is_empty(&self) -> bool {
        !self.has_selection
            || (self.anchor_fragment == self.focus_fragment
                && self.anchor_offset == self.focus_offset)
    }

    pub fn set_anchor(&mut self, fragment: usize, offset: usize) {
        self.anchor_fragment = fragment;
        self.anchor_offset = offset;
        self.has_selection = true;
    }

    pub fn set_focus(&mut self, fragment: usize, offset: usize) {
        self.focus_fragment = fragment;
        self.focus_offset = offset;
        self.has_selection = true;
    }

    pub fn clear(&mut self) {
        self.has_selection = false;
        self.pending = false;
    }

    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.anchor_fragment, self.anchor_offset);
        let b = (self.focus_fragment, self.focus_offset);
        if a <= b { (a, b) } else { (b, a) }
    }
}

pub struct Markdown {
    source: SharedString,
    events: Vec<(Range<usize>, MarkdownEvent)>,
    item_ranges: Vec<Range<usize>>,
    pub(crate) focus_handle: FocusHandle,
    copied_code_blocks: std::collections::HashSet<usize>,
    highlight_cache: HighlightCache,
    list_state: ListState,
    pub(crate) selection: MarkdownSelection,
    /// fragment_id -> text, populated lazily during render for copy. Wrapped in Rc<RefCell> so builder can write without Entity borrow during render.
    pub(crate) fragment_texts: Rc<RefCell<std::collections::HashMap<usize, String>>>,
}

impl Markdown {
    pub fn init(cx: &mut gpui::App) {
        cx.bind_keys([
            KeyBinding::new("cmd-c", Copy, Some("Markdown")),
            KeyBinding::new("ctrl-c", Copy, Some("Markdown")),
            KeyBinding::new("cmd-a", SelectAll, Some("Markdown")),
            KeyBinding::new("ctrl-a", SelectAll, Some("Markdown")),
        ]);
    }

    pub fn new(source: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let source = source.into();
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            source: SharedString::default(),
            events: Vec::new(),
            item_ranges: Vec::new(),
            focus_handle,
            copied_code_blocks: std::collections::HashSet::new(),
            highlight_cache: Rc::new(RefCell::new(HashMap::new())),
            list_state: ListState::new(0, ListAlignment::Top, px(200.0)),
            selection: MarkdownSelection::default(),
            fragment_texts: Default::default(),
        };
        this.set_text(source, cx);
        this
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        if self.source == text {
            return;
        }
        self.source = text;
        self.copied_code_blocks.clear();
        self.highlight_cache.borrow_mut().clear();
        self.selection.clear();
        self.fragment_texts.borrow_mut().clear();
        self.parse();
        cx.notify();
    }

    fn parse(&mut self) {
        self.events = parse_markdown(&self.source);
        self.item_ranges = compute_item_ranges(&self.events);
        self.list_state.reset(self.item_ranges.len());
    }

    pub(crate) fn local_selection_for_fragment(
        &self,
        fragment_id: usize,
        fragment_len: usize,
    ) -> Option<Range<usize>> {
        if self.selection.is_empty() {
            return None;
        }
        let ((s_frag, s_off), (e_frag, e_off)) = self.selection.ordered();
        if fragment_id < s_frag || fragment_id > e_frag {
            return None;
        }
        if s_frag == e_frag && s_frag == fragment_id {
            let start = s_off.min(e_off).min(fragment_len);
            let end = s_off.max(e_off).min(fragment_len);
            if start < end {
                return Some(start..end);
            } else {
                return None;
            }
        }
        if fragment_id == s_frag {
            let start = s_off.min(fragment_len);
            if start < fragment_len {
                return Some(start..fragment_len);
            } else {
                return None;
            }
        }
        if fragment_id == e_frag {
            let end = e_off.min(fragment_len);
            if end > 0 {
                return Some(0..end);
            } else {
                return None;
            }
        }
        // Fully inside selection
        Some(0..fragment_len)
    }

    pub fn selected_plain_text(&self, _cx: &App) -> String {
        if self.selection.is_empty() {
            return String::new();
        }
        let ((s_frag, s_off), (e_frag, e_off)) = self.selection.ordered();
        // Collect fragments in order
        let mut ids: Vec<usize> = self.fragment_texts.borrow().keys().cloned().collect();
        ids.sort_unstable();

        fn snap_forward(text: &str, mut offset: usize) -> usize {
            while offset < text.len() && !text.is_char_boundary(offset) {
                offset += 1;
            }
            offset.min(text.len())
        }
        fn snap_backward(text: &str, mut offset: usize, min: usize) -> usize {
            while offset > min && !text.is_char_boundary(offset) {
                offset -= 1;
            }
            offset
        }

        let mut out = String::new();
        let mut first = true;
        for &frag_id in &ids {
            if frag_id < s_frag || frag_id > e_frag {
                continue;
            }
            let Some(text) = self.fragment_texts.borrow().get(&frag_id).cloned() else {
                continue;
            };
            let slice = if s_frag == e_frag {
                let start = snap_forward(&text, s_off.min(e_off).min(text.len()));
                let end = snap_backward(&text, s_off.max(e_off).min(text.len()), start);
                &text[start..end]
            } else if frag_id == s_frag {
                let start = snap_forward(&text, s_off.min(text.len()));
                &text[start..]
            } else if frag_id == e_frag {
                let end = snap_backward(&text, e_off.min(text.len()), 0);
                &text[..end]
            } else {
                text.as_str()
            };
            if slice.is_empty() {
                continue;
            }
            if !first {
                let prev_item = (frag_id - 1) >> 20;
                let cur_item = frag_id >> 20;
                let sep = if prev_item != cur_item { "\n\n" } else { "\n" };
                out.push_str(sep);
            }
            out.push_str(slice);
            first = false;
        }
        out
    }

    pub fn copy_selection(&self, _window: &mut Window, cx: &mut App) {
        let text = self.selected_plain_text(cx);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.fragment_texts.borrow().is_empty() {
            return;
        }
        let mut ids: Vec<usize> = self.fragment_texts.borrow().keys().cloned().collect();
        ids.sort_unstable();
        if let (Some(first), Some(last)) = (ids.first(), ids.last()) {
            let last_len = self
                .fragment_texts
                .borrow()
                .get(last)
                .map(|s| s.len())
                .unwrap_or(0);
            self.selection.anchor_fragment = *first;
            self.selection.anchor_offset = 0;
            self.selection.focus_fragment = *last;
            self.selection.focus_offset = last_len;
            self.selection.has_selection = true;
            self.selection.pending = false;
            cx.notify();
        }
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.has_selection {
            self.selection.clear();
            cx.notify();
        }
    }

    pub(crate) fn register_fragment(&self, fragment_id: usize, text: String) {
        self.fragment_texts.borrow_mut().insert(fragment_id, text);
    }

    pub fn source(&self) -> &SharedString {
        &self.source
    }

    pub fn events(&self) -> &[(Range<usize>, MarkdownEvent)] {
        &self.events
    }

    pub fn mark_code_block_copied(&mut self, id: usize, cx: &mut Context<Self>) {
        self.copied_code_blocks.insert(id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update(cx, |this, cx| {
                this.copied_code_blocks.remove(&id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

crate::impl_focusable!(Markdown);

impl Render for Markdown {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let markdown = cx.entity();
        let list_state = self.list_state.clone();
        let style = MarkdownStyle::themed(_window, cx);
        // Register actions for copy/select-all when focused
        let markdown_for_copy = markdown.clone();
        let markdown_for_select_all = markdown.clone();

        // Notion/Typora 版式：居中窄栏，最大 780px (≈72ch)，
        // 外层负责背景与居中，内层窄栏承载 list，实现优雅的阅读宽度。
        div()
            .id("markdown")
            .key_context("Markdown")
            .track_focus(&self.focus_handle)
            .on_action({
                let markdown = markdown_for_copy.clone();
                move |_: &Copy, window, cx| {
                    markdown.update(cx, |m, cx| m.copy_selection(window, cx));
                }
            })
            .on_action({
                let markdown = markdown_for_select_all.clone();
                move |_: &SelectAll, _window, cx| {
                    markdown.update(cx, |markdown, cx| {
                        markdown.select_all(cx);
                    });
                }
            })
            .flex()
            .flex_col()
            .items_center()
            .size_full()
            .overflow_hidden()
            .bg(theme.app_background)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    if !this.selection.pending && this.selection.has_selection {
                        this.clear_selection(cx);
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(780.))
                    .px_8()
                    .py_8()
                    .size_full()
                    .overflow_hidden()
                    .child(
                        list(list_state, move |ix, window, cx| {
                            Self::render_item(markdown.clone(), ix, window, cx, style.clone())
                        })
                        .w_full()
                        .h_full(),
                    ),
            )
    }
}

impl Markdown {
    fn render_item(
        markdown: Entity<Markdown>,
        ix: usize,
        _window: &mut Window,
        cx: &mut gpui::App,
        style: MarkdownStyle,
    ) -> gpui::AnyElement {
        let (copied, events, highlight_cache) = markdown.update(cx, |markdown, _cx| {
            let range = markdown.item_ranges[ix].clone();
            (
                markdown.copied_code_blocks.clone(),
                markdown.events[range].to_vec(),
                markdown.highlight_cache.clone(),
            )
        });

        let elements = MarkdownElementBuilder::new_with_item(
            style,
            copied,
            markdown,
            highlight_cache,
            ix,
        )
        .build(&events, cx);

        div().w_full().children(elements).into_any_element()
    }
}

fn compute_item_ranges(events: &[(Range<usize>, MarkdownEvent)]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut depth: usize = 0;
    let mut start = 0;
    for (i, (_, event)) in events.iter().enumerate() {
        match event {
            MarkdownEvent::Start(_) => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            MarkdownEvent::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    ranges.push(start..i + 1);
                }
            }
            MarkdownEvent::Rule if depth == 0 => {
                ranges.push(i..i + 1);
            }
            _ => {}
        }
    }
    ranges
}

pub fn highlight_code_runs(
    content: &str,
    language: &str,
    base_style: &gpui::TextStyle,
    theme: &AppTheme,
) -> Vec<TextRun> {
    let mut highlighter = SyntaxHighlighter::new(language);
    let rope = ropey::Rope::from_str(content);
    highlighter.update(None, &rope, None);
    let styles = highlighter.styles(&(0..content.len()), &theme.highlight_theme);

    let mut runs = Vec::new();
    let mut offset = 0;
    for (range, style) in &styles {
        if offset < range.start {
            runs.push(base_style.to_run(range.start - offset));
        }
        let mut run_style = base_style.clone();
        run_style = run_style.highlight(*style);
        runs.push(run_style.to_run(range.len()));
        offset = range.end;
    }
    if offset < content.len() {
        runs.push(base_style.to_run(content.len() - offset));
    }
    runs
}
