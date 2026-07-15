use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, ClipboardItem, Div, EdgesRefinement, IntoElement, Length, ParentElement,
    SharedString, Stateful, StyleRefinement, Styled, StyledText, TextRun, TextStyleRefinement, div,
    px, rems,
};

use crate::theme::ActiveTheme;

use super::markdown::{MarkdownStyle, highlight_code_runs};
use super::markdown_parser::{
    ColumnAlign, MarkdownEvent, MarkdownTag, MarkdownTagEnd,
};

pub enum AnyDiv {
    Div(Div),
    Stateful(Stateful<Div>),
}

impl AnyDiv {
    fn into_any_element(self) -> AnyElement {
        match self {
            Self::Div(div) => div.into_any_element(),
            Self::Stateful(div) => div.into_any_element(),
        }
    }
}

impl From<Div> for AnyDiv {
    fn from(value: Div) -> Self {
        Self::Div(value)
    }
}

impl From<Stateful<Div>> for AnyDiv {
    fn from(value: Stateful<Div>) -> Self {
        Self::Stateful(value)
    }
}

impl Styled for AnyDiv {
    fn style(&mut self) -> &mut StyleRefinement {
        match self {
            Self::Div(div) => div.style(),
            Self::Stateful(div) => div.style(),
        }
    }
}

impl ParentElement for AnyDiv {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        match self {
            Self::Div(div) => div.extend(elements),
            Self::Stateful(div) => div.extend(elements),
        }
    }
}

struct DivStackEntry {
    div: AnyDiv,
    children: Vec<AnyElement>,
}

impl DivStackEntry {
    fn new(div: impl Into<AnyDiv>) -> Self {
        Self {
            div: div.into(),
            children: Vec::new(),
        }
    }

    fn push_child(&mut self, child: AnyElement) {
        self.children.push(child);
    }
}

#[derive(Default)]
struct PendingLine {
    text: String,
    runs: Vec<TextRun>,
}

struct TableState {
    alignments: Vec<ColumnAlign>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
}

pub struct MarkdownElementBuilder {
    style: MarkdownStyle,
    div_stack: Vec<DivStackEntry>,
    pending_line: PendingLine,
    text_style_stack: Vec<TextStyleRefinement>,
    code_block_language: Vec<Option<String>>,
    code_block_content: Vec<String>,
    code_block_index: usize,
    list_stack: Vec<Option<u64>>,
    table: TableState,
    blockquote_depth: usize,
    heading_level: u8,
    pending_task: Option<(bool, Range<usize>)>,
}

impl MarkdownElementBuilder {
    pub fn new(style: MarkdownStyle) -> Self {
        let mut base_div = div();
        base_div.style().refine(&style.container_style);
        Self {
            style,
            div_stack: vec![DivStackEntry::new(base_div)],
            pending_line: PendingLine::default(),
            text_style_stack: Vec::new(),
            code_block_language: Vec::new(),
            code_block_content: Vec::new(),
            code_block_index: 0,
            list_stack: Vec::new(),
            table: TableState {
                alignments: Vec::new(),
                rows: Vec::new(),
                current_row: Vec::new(),
            },
            blockquote_depth: 0,
            heading_level: 0,
            pending_task: None,
        }
    }

    pub fn build(
        mut self,
        events: Vec<(Range<usize>, MarkdownEvent)>,
        cx: &mut gpui::App,
    ) -> Vec<AnyElement> {
        let markdown_end = events.last().map(|(r, _)| r.end).unwrap_or(0);
        for (range, event) in events {
            self.process_event(range, event, markdown_end, cx);
        }
        self.flush_text();
        while self.div_stack.len() > 1 {
            self.pop_div();
        }
        self.div_stack.pop().map(|entry| entry.children).unwrap_or_default()
    }

    fn process_event(
        &mut self,
        range: Range<usize>,
        event: MarkdownEvent,
        markdown_end: usize,
        cx: &mut gpui::App,
    ) {
        match event {
            MarkdownEvent::Start(tag) => match tag {
                MarkdownTag::Paragraph => {
                    self.push_div(div().mb_2().line_height(rems(1.3)), &range, markdown_end);
                    if self.blockquote_depth > 0 {
                        self.push_text_style(self.style.block_quote.clone());
                    }
                }
                MarkdownTag::Heading(level) => {
                    self.heading_level = level;
                    let mut heading = div();
                    heading.style().refine(&self.style.heading);
                    if let Some(refinement) = self.style.heading_level_styles.get(level as usize - 1).cloned().flatten() {
                        heading.style().refine(&StyleRefinement {
                            text: refinement,
                            ..Default::default()
                        });
                    }
                    if level <= 2 {
                        if let Some(color) = self.style.heading_border_color {
                            heading = heading.border_b(px(1.0)).border_color(color);
                        }
                    }
                    self.push_div(heading, &range, markdown_end);
                }
                MarkdownTag::CodeBlock(info) => {
                    self.code_block_language.push(
                        if info.language.is_empty() {
                            None
                        } else {
                            Some(info.language.clone())
                        }
                    );
                    self.code_block_content.push(String::new());
                    self.code_block_index += 1;
                    let mut code_block = div().group("code-block").relative().w_full();
                    code_block.style().refine(&self.style.code_block);
                    let code_block = if self.style.code_block_overflow_x_scroll {
                        code_block.id("code-block-scroll").overflow_x_scroll()
                    } else {
                        code_block.id("code-block")
                    };
                    self.push_div(code_block, &range, markdown_end);
                    self.push_text_style(TextStyleRefinement {
                        font_family: self.style.code_block.text.font_family.clone(),
                        font_size: self.style.code_block.text.font_size,
                        white_space: self.style.code_block.text.white_space,
                        ..Default::default()
                    });
                }
                MarkdownTag::List(start) => {
                    self.list_stack.push(start);
                    self.push_div(div().pl_2p5(), &range, markdown_end);
                }
                MarkdownTag::Item => {
                    self.push_div(
                        div().flex().flex_row().items_start().gap_1().w_full(),
                        &range,
                        markdown_end,
                    );
                    let bullet = if let Some(task) = &self.pending_task {
                        let checked = task.0;
                        if checked { "☑".to_string() } else { "☐".to_string() }
                    } else if let Some(Some(n)) = self.list_stack.last().cloned() {
                        let s = format!("{}.", n);
                        if let Some(last) = self.list_stack.last_mut() {
                            *last.as_mut().unwrap() += 1;
                        }
                        s
                    } else {
                        "•".to_string()
                    };
                    self.pending_task = None;
                    let bullet_el = div().w(px(20.0)).child(bullet).text_color(cx.theme().ui.text_muted);
                    self.push_div(bullet_el, &range, markdown_end);
                    self.pop_div();
                    self.push_div(div().flex_1().w_0(), &range, markdown_end);
                }
                MarkdownTag::BlockQuote => {
                    self.blockquote_depth += 1;
                    let mut block = div();
                    block.style().refine(
                        &StyleRefinement {
                            padding: gpui::EdgesRefinement {
                                left: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(px(16.)))),
                                ..Default::default()
                            },
                            border_widths: gpui::EdgesRefinement {
                                left: Some(gpui::AbsoluteLength::Pixels(px(4.))),
                                ..Default::default()
                            },
                            border_color: Some(self.style.block_quote_border_color),
                            ..Default::default()
                        }
                    );
                    self.push_div(block, &range, markdown_end);
                    self.push_text_style(self.style.block_quote.clone());
                }
                MarkdownTag::Table(aligns) => {
                    self.table.alignments = aligns;
                    self.table.rows.clear();
                    self.table.current_row.clear();
                    let col_count = self.table.alignments.len().max(1);
                    let grid = div()
                        .grid()
                        .grid_cols(col_count as u16)
                        .w_full()
                        .mb_2()
                        .border(px(1.0))
                        .border_color(cx.theme().ui.border)
                        .rounded_sm()
                        .overflow_hidden();
                    self.push_div(grid, &range, markdown_end);
                }
                MarkdownTag::TableHead => {
                    self.table.current_row.clear();
                }
                MarkdownTag::TableRow => {
                    self.table.current_row.clear();
                }
                MarkdownTag::TableCell => {}
                MarkdownTag::Emphasis => {
                    self.push_text_style(TextStyleRefinement {
                        font_style: Some(gpui::FontStyle::Italic),
                        ..Default::default()
                    });
                }
                MarkdownTag::Strong => {
                    self.push_text_style(TextStyleRefinement {
                        font_weight: Some(gpui::FontWeight::BOLD),
                        ..Default::default()
                    });
                }
                MarkdownTag::Strikethrough => {
                    self.push_text_style(TextStyleRefinement {
                        strikethrough: Some(gpui::StrikethroughStyle {
                            thickness: px(1.0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                }
                MarkdownTag::Link { dest_url } => {
                    self.push_text_style(self.style.link.clone());
                }
                MarkdownTag::Image { dest_url } => {
                    // Images not supported in this simplified version
                }
            },
            MarkdownEvent::End(tag_end) => match tag_end {
                MarkdownTagEnd::Paragraph => {
                    if self.blockquote_depth > 0 {
                        self.pop_text_style();
                    }
                    self.pop_div();
                }
                MarkdownTagEnd::Heading(_) => {
                    self.heading_level = 0;
                    self.pop_div();
                }
                MarkdownTagEnd::CodeBlock => {
                    let content = self.code_block_content.pop().unwrap_or_default();
                    let id = SharedString::from(format!("copy-{}", self.code_block_index));
                    let button = div()
                        .id(id.clone())
                        .absolute()
                        .top(px(8.0))
                        .right(px(8.0))
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .child("Copy")
                        .on_click(move |_: &ClickEvent, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(content.clone()));
                        })
                        .into_any_element();
                    self.append_child(button);
                    self.pop_text_style();
                    self.code_block_language.pop();
                    self.pop_div();
                }
                MarkdownTagEnd::List(_) => {
                    self.list_stack.pop();
                    self.pop_div();
                }
                MarkdownTagEnd::Item => {
                    self.pop_div();
                    self.pop_div();
                }
                MarkdownTagEnd::BlockQuote => {
                    self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                    self.pop_text_style();
                    self.pop_div();
                }
                MarkdownTagEnd::Table => {
                    self.render_table(cx);
                    self.pop_div();
                }
                MarkdownTagEnd::TableHead => {
                    self.table.rows.push(self.table.current_row.clone());
                }
                MarkdownTagEnd::TableRow => {
                    self.table.rows.push(self.table.current_row.clone());
                }
                MarkdownTagEnd::TableCell => {
                    let text = self.pending_line.text.clone();
                    self.pending_line = PendingLine::default();
                    self.table.current_row.push(text);
                }
                MarkdownTagEnd::Emphasis
                | MarkdownTagEnd::Strong
                | MarkdownTagEnd::Strikethrough
                | MarkdownTagEnd::Link => {
                    self.pop_text_style();
                }
                MarkdownTagEnd::Image => {}
            },
            MarkdownEvent::Text(text) => {
                self.push_text(&text, &range, cx);
            }
            MarkdownEvent::Code(code) => {
                self.push_text_style(self.style.inline_code.clone());
                self.push_text(&code, &range, cx);
                self.pop_text_style();
            }
            MarkdownEvent::Html(_) => {}
            MarkdownEvent::SoftBreak => {
                self.push_text(" ", &range, cx);
            }
            MarkdownEvent::HardBreak => {
                self.push_text("\n", &range, cx);
            }
            MarkdownEvent::Rule => {
                self.flush_text();
                let rule = div()
                    .my_4()
                    .h(px(1.0))
                    .bg(self.style.rule_color)
                    .into_any_element();
                self.append_child(rule);
            }
            MarkdownEvent::TaskListMarker(checked) => {
                self.pending_task = Some((checked, range));
            }
        }
    }

    fn push_text_style(&mut self, style: TextStyleRefinement) {
        self.text_style_stack.push(style);
    }

    fn pop_text_style(&mut self) {
        self.text_style_stack.pop();
    }

    fn text_style(&self) -> gpui::TextStyle {
        let mut style = self.style.base_text_style.clone();
        for refinement in &self.text_style_stack {
            style.refine(refinement);
        }
        style
    }

    fn push_text(
        &mut self, text: &str, _range: &Range<usize>, cx: &mut gpui::App) {
        if self.code_block_language.last().and_then(|l| l.as_ref()).is_some() {
            self.push_code_text(text, cx);
            return;
        }
        let style = self.text_style();
        self.pending_line.text.push_str(text);
        self.pending_line.runs.push(style.to_run(text.len()));
    }

    fn push_code_text(
        &mut self, text: &str, cx: &mut gpui::App) {
        let style = self.text_style();
        let language = self.code_block_language.last().unwrap().clone().unwrap();
        let runs = highlight_code_runs(text, &language, &style, cx.theme());
        self.pending_line.text.push_str(text);
        self.pending_line.runs.extend(runs);
        if let Some(content) = self.code_block_content.last_mut() {
            content.push_str(text);
        }
    }

    fn flush_text(&mut self) {
        if self.pending_line.text.is_empty() {
            return;
        }
        let styled_text = StyledText::new(self.pending_line.text.clone())
            .with_runs(self.pending_line.runs.clone());
        self.append_child(styled_text.into_any_element());
        self.pending_line = PendingLine::default();
    }

    fn append_child(&mut self, child: AnyElement) {
        if let Some(entry) = self.div_stack.last_mut() {
            entry.push_child(child);
        }
    }

    fn push_div(
        &mut self,
        div: impl Into<AnyDiv>,
        range: &Range<usize>,
        markdown_end: usize,
    ) {
        let mut div = div.into();
        self.flush_text();

        if range.start == 0 {
            div.style().refine(&StyleRefinement {
                margin: EdgesRefinement {
                    top: Some(Length::Definite(px(0.0).into())),
                    ..Default::default()
                },
                ..Default::default()
            });
        }

        if range.end == markdown_end {
            div.style().refine(&StyleRefinement {
                margin: EdgesRefinement {
                    bottom: Some(Length::Definite(px(0.0).into())),
                    ..Default::default()
                },
                ..Default::default()
            });
        }

        self.div_stack.push(DivStackEntry::new(div));
    }

    fn pop_div(&mut self) {
        self.flush_text();
        if let Some(entry) = self.div_stack.pop() {
            let div = match entry.div {
                AnyDiv::Div(div) => div.children(entry.children).into_any_element(),
                AnyDiv::Stateful(div) => div.children(entry.children).into_any_element(),
            };
            if let Some(parent) = self.div_stack.last_mut() {
                parent.push_child(div);
            }
        }
    }

    fn render_table(&mut self, cx: &mut gpui::App) {
        if self.table.rows.is_empty() {
            return;
        }
        let col_count = self.table.alignments.len().max(1);
        let base_style = self.style.base_text_style.clone();
        let rows_len = self.table.rows.len();
        let alignments = self.table.alignments.clone();
        let border_color = cx.theme().ui.border;
        let elevated_bg = cx.theme().ui.elevated_background;
        let panel_bg = cx.theme().ui.panel_background;

        let mut children = Vec::new();
        for (row_idx, row) in self.table.rows.iter().enumerate() {
            let is_header = row_idx == 0;
            for col in 0..col_count {
                let cell_text = row.get(col).map(|s| s.as_str()).unwrap_or("");
                let mut cell = div().px_3().py_2();
                if row_idx < rows_len - 1 {
                    cell = cell.border_b(px(1.0)).border_color(border_color);
                }
                if is_header {
                    cell = cell.bg(elevated_bg);
                } else if row_idx % 2 == 0 {
                    cell = cell.bg(panel_bg);
                }
                match alignments.get(col).unwrap_or(&ColumnAlign::None) {
                    ColumnAlign::Left => cell = cell.justify_start(),
                    ColumnAlign::Center => cell = cell.justify_center(),
                    ColumnAlign::Right => cell = cell.justify_end(),
                    ColumnAlign::None => {}
                }
                let cell_text = StyledText::new(cell_text.to_string())
                    .with_runs(vec![base_style.to_run(cell_text.len())]);
                cell = cell.child(cell_text);
                children.push(cell.into_any_element());
            }
        }

        for child in children {
            self.append_child(child);
        }
    }
}
