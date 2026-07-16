use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, ListAlignment, ListState, Render,
    SharedString, StyleRefinement, TextRun, TextStyleRefinement, WhiteSpace, Window, div, list, px,
    rems,
};

use crate::sidebar_view::font_settings::{
    platform_default_fallbacks, platform_default_font_family,
};
use crate::syntax::SyntaxHighlighter;
use crate::theme::{ActiveTheme, AppTheme};

use super::markdown_element::MarkdownElementBuilder;
use super::markdown_parser::{MarkdownEvent, parse_markdown};

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

        let mut base_text_style = window.text_style();
        base_text_style.refine(&TextStyleRefinement {
            color: Some(ui.text),
            line_height: Some(rems(1.75).into()),
            white_space: Some(WhiteSpace::Normal),
            ..Default::default()
        });

        let code_block = StyleRefinement {
            padding: gpui::EdgesRefinement {
                top: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(8.)),
                )),
                left: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(8.)),
                )),
                right: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(8.)),
                )),
                bottom: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(8.)),
                )),
            },
            margin: gpui::EdgesRefinement {
                top: Some(gpui::Length::Definite(px(8.).into())),
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
            border_color: Some(ui.border),
            background: Some(theme.editor_background().into()),
            text: TextStyleRefinement {
                font_family: Some(platform_default_font_family()),
                font_fallbacks: Some(platform_default_fallbacks()),
                ..Default::default()
            },
            ..Default::default()
        };

        let inline_code = TextStyleRefinement {
            font_family: Some(platform_default_font_family()),
            font_fallbacks: Some(platform_default_fallbacks()),
            font_size: Some(px(13.).into()),
            background_color: Some(ui.text.opacity(0.08)),
            ..Default::default()
        };

        let link = TextStyleRefinement {
            color: Some(ui.accent),
            underline: Some(gpui::UnderlineStyle {
                color: Some(ui.accent.opacity(0.5)),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        };

        let heading_level_styles = [
            Some(TextStyleRefinement {
                font_size: Some(rems(1.45).into()),
                ..Default::default()
            }),
            Some(TextStyleRefinement {
                font_size: Some(rems(1.3).into()),
                ..Default::default()
            }),
            Some(TextStyleRefinement {
                font_size: Some(rems(1.1).into()),
                ..Default::default()
            }),
            Some(TextStyleRefinement {
                font_size: Some(rems(1.01).into()),
                ..Default::default()
            }),
            Some(TextStyleRefinement {
                font_size: Some(rems(0.95).into()),
                ..Default::default()
            }),
            Some(TextStyleRefinement {
                font_size: Some(rems(0.85).into()),
                ..Default::default()
            }),
        ];

        Self {
            base_text_style,
            container_style: StyleRefinement::default(),
            code_block,
            inline_code,
            block_quote: TextStyleRefinement {
                color: Some(ui.text_muted),
                ..Default::default()
            },
            link,
            rule_color: ui.border,
            block_quote_border_color: ui.border,
            heading: StyleRefinement {
                ..Default::default()
            },
            heading_level_styles,
            heading_border_color: Some(ui.border),
        }
    }
}

pub struct Markdown {
    source: SharedString,
    events: Vec<(Range<usize>, MarkdownEvent)>,
    item_ranges: Vec<Range<usize>>,
    focus_handle: FocusHandle,
    wrapped_code_blocks: std::collections::HashSet<usize>,
    copied_code_blocks: std::collections::HashSet<usize>,
    highlight_cache: Rc<RefCell<HashMap<(String, Range<usize>), Vec<TextRun>>>>,
    list_state: ListState,
}

impl Markdown {
    pub fn new(source: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let source = source.into();
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            source: SharedString::default(),
            events: Vec::new(),
            item_ranges: Vec::new(),
            focus_handle,
            wrapped_code_blocks: std::collections::HashSet::new(),
            copied_code_blocks: std::collections::HashSet::new(),
            highlight_cache: Rc::new(RefCell::new(HashMap::new())),
            list_state: ListState::new(0, ListAlignment::Top, px(200.0)),
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
        self.wrapped_code_blocks.clear();
        self.copied_code_blocks.clear();
        self.highlight_cache.borrow_mut().clear();
        self.parse();
        cx.notify();
    }

    fn parse(&mut self) {
        self.events = parse_markdown(&self.source);
        self.item_ranges = compute_item_ranges(&self.events);
        self.list_state.reset(self.item_ranges.len());
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

    pub fn toggle_code_block_wrap(&mut self, source_offset: usize, cx: &mut Context<Self>) {
        if self.wrapped_code_blocks.contains(&source_offset) {
            self.wrapped_code_blocks.remove(&source_offset);
        } else {
            self.wrapped_code_blocks.insert(source_offset);
        }
        self.list_state.reset(self.item_ranges.len());
        cx.notify();
    }
}

impl Focusable for Markdown {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Markdown {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let markdown = cx.entity();
        let list_state = self.list_state.clone();
        let style = MarkdownStyle::themed(_window, cx);

        div()
            .id("markdown")
            .key_context("Markdown")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(theme.app_background)
            .child(
                div().flex().flex_col().px_8().py_6().size_full().child(
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
        let (wrapped, copied, events, highlight_cache) = markdown.update(cx, |markdown, _cx| {
            let range = markdown.item_ranges[ix].clone();
            (
                markdown.wrapped_code_blocks.clone(),
                markdown.copied_code_blocks.clone(),
                markdown.events[range].to_vec(),
                markdown.highlight_cache.clone(),
            )
        });

        let elements =
            MarkdownElementBuilder::new(style, wrapped, copied, markdown, highlight_cache)
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
            MarkdownEvent::Rule => {
                if depth == 0 {
                    ranges.push(i..i + 1);
                }
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
