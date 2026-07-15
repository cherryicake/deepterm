use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    Context, FocusHandle, Focusable, Hsla, IntoElement, Render, ScrollHandle, SharedString,
    StyleRefinement, TextRun, TextStyleRefinement, Window, div, px, rems,
};

use crate::syntax::SyntaxHighlighter;
use crate::theme::{ActiveTheme, AppTheme};

use super::markdown_element::MarkdownElementBuilder;
use super::markdown_parser::{MarkdownEvent, parse_markdown};

pub struct MarkdownStyle {
    pub base_text_style: gpui::TextStyle,
    pub container_style: StyleRefinement,
    pub code_block: StyleRefinement,
    pub code_block_overflow_x_scroll: bool,
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
            line_height: Some(rems(1.3).into()),
            ..Default::default()
        });

        let code_block = StyleRefinement {
            padding: gpui::EdgesRefinement {
                top: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(px(8.)))),
                left: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(px(8.)))),
                right: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(px(8.)))),
                bottom: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(px(8.)))),
            },
            margin: gpui::EdgesRefinement {
                top: Some(gpui::Length::Definite(px(8.).into())),
                left: Some(gpui::Length::Definite(px(0.).into())),
                right: Some(gpui::Length::Definite(px(0.).into())),
                bottom: Some(gpui::Length::Definite(px(12.).into())),
            },
            border_style: Some(gpui::BorderStyle::Solid),
            border_widths: gpui::EdgesRefinement {
                top: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                left: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                right: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                bottom: Some(gpui::AbsoluteLength::Pixels(px(1.))),
            },
            border_color: Some(ui.border),
            background: Some(ui.panel_background.into()),
            text: TextStyleRefinement {
                font_family: Some(SharedString::from("monospace")),
                font_size: Some(px(13.).into()),
                white_space: Some(gpui::WhiteSpace::Nowrap),
                ..Default::default()
            },
            ..Default::default()
        };

        let inline_code = TextStyleRefinement {
            font_family: Some(SharedString::from("monospace")),
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
            code_block_overflow_x_scroll: true,
            inline_code,
            block_quote: TextStyleRefinement {
                color: Some(ui.text_muted),
                ..Default::default()
            },
            link,
            rule_color: ui.border,
            block_quote_border_color: ui.border,
            heading: StyleRefinement {
                margin: gpui::EdgesRefinement {
                    top: Some(gpui::Length::Definite(px(16.).into())),
                    bottom: Some(gpui::Length::Definite(px(8.).into())),
                    left: Some(gpui::Length::Definite(px(0.).into())),
                    right: Some(gpui::Length::Definite(px(0.).into())),
                },
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
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    copied_code_blocks: std::collections::HashSet<gpui::ElementId>,
}

impl Markdown {
    pub fn new(source: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let source = source.into();
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            source: SharedString::default(),
            events: Vec::new(),
            focus_handle,
            scroll_handle: ScrollHandle::new(),
            copied_code_blocks: std::collections::HashSet::new(),
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
        self.parse();
        cx.notify();
    }

    fn parse(&mut self) {
        self.events = parse_markdown(&self.source);
    }

    pub fn source(&self) -> &SharedString {
        &self.source
    }

    pub fn events(&self) -> &[(Range<usize>, MarkdownEvent)] {
        &self.events
    }

    pub fn copied_code_blocks(&self) -> &std::collections::HashSet<gpui::ElementId> {
        &self.copied_code_blocks
    }

    pub fn mark_code_block_copied(
        &mut self,
        id: gpui::ElementId,
        cx: &mut Context<Self>,
    ) {
        self.copied_code_blocks.insert(id);
        cx.notify();
    }
}

impl Focusable for Markdown {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Markdown {
    fn render(&mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let style = MarkdownStyle::themed(window, cx);
        let elements = MarkdownElementBuilder::new(style)
            .build(self.events.clone(), cx);
        let theme = cx.theme().ui;
        let scroll_handle = self.scroll_handle.clone();

        div()
            .id("markdown")
            .key_context("Markdown")
            .track_focus(&self.focus_handle)
            .w_full()
            .h_full()
            .overflow_hidden()
            .bg(theme.app_background)
            .child(
                div()
                    .id("markdown-scroll")
                    .w_full()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll_handle)
                    .px_8()
                    .py_6()
                    .children(elements),
            )
    }
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
    let styles = highlighter.styles(&(0..content.len()),
        &theme.highlight_theme,
    );

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
