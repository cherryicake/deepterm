use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, ClipboardItem, Div, EdgesRefinement, Entity, IntoElement, Length,
    ParentElement, SharedString, Stateful, StyleRefinement, Styled, StyledText, TextRun,
    TextStyleRefinement, div, px, rems, svg,
};

use crate::assets::Icon;
use crate::theme::ActiveTheme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalloutKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl CalloutKind {
    fn from_marker(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "[!NOTE]" => Some(Self::Note),
            "[!TIP]" => Some(Self::Tip),
            "[!IMPORTANT]" => Some(Self::Important),
            "[!WARNING]" => Some(Self::Warning),
            "[!CAUTION]" => Some(Self::Caution),
            _ => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Note => "NOTE",
            Self::Tip => "TIP",
            Self::Important => "IMPORTANT",
            Self::Warning => "WARNING",
            Self::Caution => "CAUTION",
        }
    }

    fn accent_color(&self, cx: &gpui::App) -> gpui::Hsla {
        let ui = &cx.theme().ui;
        match self {
            Self::Note => ui.accent,
            Self::Tip => ui.success,
            Self::Important => gpui::rgb(0x8250DF).into(), // purple
            Self::Warning => ui.warning,
            Self::Caution => ui.error,
        }
    }

    fn bg_color(&self, cx: &gpui::App) -> gpui::Hsla {
        self.accent_color(cx).opacity(if cx.theme().appearance.is_dark() { 0.18 } else { 0.13 })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalloutState {
    Pending,
    NotCallout,
    Kind(CalloutKind),
}


#[derive(Clone, Debug)]
struct ImageState {
    url: String,
    alt: String,
    runs: Vec<TextRun>,
}


use super::markdown::{HighlightCache, Markdown, MarkdownStyle, highlight_code_runs};
use super::markdown_parser::{ColumnAlign, MarkdownEvent, MarkdownTag, MarkdownTagEnd};

pub enum AnyDiv {
    Div(Div),
    Stateful(Stateful<Div>),
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

#[derive(Clone, Default)]
struct PendingLine {
    text: String,
    runs: Vec<TextRun>,
}

struct TableState {
    alignments: Vec<ColumnAlign>,
    /// 每个单元格保留文本与样式 runs（加粗/斜体等），渲染时逐格重建 StyledText。
    rows: Vec<Vec<PendingLine>>,
    current_row: Vec<PendingLine>,
}

struct LinkState {
    url: String,
    text: String,
    runs: Vec<TextRun>,
}

pub struct MarkdownElementBuilder {
    style: MarkdownStyle,
    div_stack: Vec<DivStackEntry>,
    pending_line: PendingLine,
    text_style_stack: Vec<TextStyleRefinement>,
    code_block_language: Vec<Option<String>>,
    code_block_content: Vec<String>,
    code_block_ids: Vec<usize>,
    list_stack: Vec<Option<u64>>,
    table: TableState,
    blockquote_depth: usize,
    heading_level: u8,
    pending_task: Option<(bool, Range<usize>)>,
    link_state: Option<LinkState>,
    image_state: Option<ImageState>,
    blockquote_callouts: Vec<CalloutState>,
    task_done_stack: Vec<bool>,
    #[allow(dead_code)]
    wrapped_code_blocks: std::collections::HashSet<usize>,
    copied_code_blocks: std::collections::HashSet<usize>,
    markdown: Entity<Markdown>,
    highlight_cache: HighlightCache,
}

impl MarkdownElementBuilder {
    pub fn new(
        style: MarkdownStyle,
        #[allow(dead_code)]
    wrapped_code_blocks: std::collections::HashSet<usize>,
        copied_code_blocks: std::collections::HashSet<usize>,
        markdown: Entity<Markdown>,
        highlight_cache: HighlightCache,
    ) -> Self {
        let mut base_div = div();
        base_div.style().refine(&style.container_style);
        Self {
            style,
            div_stack: vec![DivStackEntry::new(base_div)],
            pending_line: PendingLine::default(),
            text_style_stack: Vec::new(),
            code_block_language: Vec::new(),
            code_block_content: Vec::new(),
            code_block_ids: Vec::new(),
            list_stack: Vec::new(),
            table: TableState {
                alignments: Vec::new(),
                rows: Vec::new(),
                current_row: Vec::new(),
            },
            blockquote_depth: 0,
            heading_level: 0,
            pending_task: None,
            link_state: None,
            image_state: None,
            blockquote_callouts: Vec::new(),
            task_done_stack: Vec::new(),
            wrapped_code_blocks,
            copied_code_blocks,
            markdown,
            highlight_cache,
        }
    }

    pub fn build(
        mut self,
        events: &[(Range<usize>, MarkdownEvent)],
        cx: &mut gpui::App,
    ) -> Vec<AnyElement> {
        let markdown_end = events.last().map(|(r, _)| r.end).unwrap_or(0);
        for (range, event) in events {
            self.process_event(range.clone(), event.clone(), markdown_end, cx);
        }
        self.flush_text();
        while self.div_stack.len() > 1 {
            self.pop_div();
        }
        self.div_stack
            .pop()
            .map(|entry| entry.children)
            .unwrap_or_default()
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
                    // 段落：flex_wrap 保留行内混排，my_3 统一间距
                    self.push_div(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .my_3()
                            .line_height(rems(1.75)),
                        &range,
                        markdown_end,
                    );
                    if self.blockquote_depth > 0 {
                        self.push_text_style(self.style.block_quote.clone());
                    }
                }
                MarkdownTag::Heading(level) => {
                    self.heading_level = level;
                    // 标题：逐级递减边距，收紧行高，H1/H2 带分割线
                    let mut heading = match level {
                        1 => div().mt_8().mb_4().pb_2(),
                        2 => div().mt_7().mb_3().pb_2(),
                        3 => div().mt_6().mb_3(),
                        4 => div().mt_6().mb_2(),
                        _ => div().mt_4().mb_2(),
                    };
                    heading = heading.w_full().flex().flex_wrap().items_start().group("heading");
                    heading.style().refine(&self.style.heading);
                    if let Some(refinement) = self
                        .style
                        .heading_level_styles
                        .get(level as usize - 1)
                        .cloned()
                        .flatten()
                    {
                        heading.style().refine(&StyleRefinement {
                            text: refinement,
                            ..Default::default()
                        });
                    }
                    if level <= 2 {
                        if let Some(color) = self.style.heading_border_color {
                            heading = heading.border_b_1().border_color(color);
                        }
                    }
                    self.push_div(heading, &range, markdown_end);
                }
                MarkdownTag::CodeBlock(info) => {
                    self.code_block_language.push(if info.language.is_empty() {
                        None
                    } else {
                        Some(info.language.clone())
                    });
                    self.code_block_content.push(String::new());
                    let code_block_id = range.start;
                    self.code_block_ids.push(code_block_id);
                    let has_language = self
                        .code_block_language
                        .last()
                        .and_then(|l| l.as_ref())
                        .is_some();
                    let mut outer = div()
                        .group("code-block")
                        .relative()
                        .w_full()
                        .rounded(px(8.0))
                        .overflow_hidden();
                    outer.style().refine(&self.style.code_block);
                    self.push_div(outer, &range, markdown_end);
                    let mut inner = div()
                        .id(format!("code-inner-{}", code_block_id))
                        .overflow_x_scroll();
                    if has_language {
                        inner = inner.pt(px(24.0));
                    }
                    self.push_div(inner, &range, markdown_end);
                    let mut code_text_style = self.style.code_block.text.clone();
                    code_text_style.white_space = Some(gpui::WhiteSpace::Nowrap);
                    self.push_text_style(code_text_style);
                }
                MarkdownTag::List(start) => {
                    self.list_stack.push(start);
                    // 列表：每级 24px 缩进
                    self.push_div(div().pl_6().my_2(), &range, markdown_end);
                }
                MarkdownTag::Item => {
                    self.push_div(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap_1()
                            .w_full()
                            .mb_2()
                            .line_height(rems(1.75)),
                        &range,
                        markdown_end,
                    );
                    let task_info = self.pending_task.clone();
                    let (bullet, is_task, is_checked) = match task_info {
                        Some((checked, _)) => (
                            if checked { "☑".to_string() } else { "☐".to_string() },
                            true,
                            checked,
                        ),
                        None => {
                            if let Some(Some(n)) = self.list_stack.last().cloned() {
                                let s = format!("{}.", n);
                                if let Some(last) = self.list_stack.last_mut() {
                                    *last.as_mut().unwrap() += 1;
                                }
                                (s, false, false)
                            } else {
                                ("•".to_string(), false, false)
                            }
                        }
                    };
                    self.pending_task = None;
                    // 记录该项是否为已完成任务，用于 Item 结束时弹出删除线样式
                    self.task_done_stack.push(is_checked);
                    if is_checked {
                        self.push_text_style(TextStyleRefinement {
                            color: Some(cx.theme().ui.text_muted),
                            strikethrough: Some(gpui::StrikethroughStyle {
                                thickness: px(1.),
                                color: Some(cx.theme().ui.text_muted.opacity(0.6)),
                                ..Default::default()
                            }),
                            ..Default::default()
                        });
                    }
                    let bullet_el: AnyDiv = if is_task {
                        let checkbox = if is_checked {
                            div()
                                .size(px(16.))
                                .rounded(px(4.))
                                .bg(cx.theme().ui.success)
                                .border_1()
                                .border_color(cx.theme().ui.success)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    svg()
                                        .size(px(10.))
                                        .text_color(gpui::white())
                                        .path(Icon::Check.path()),
                                )
                                .into_any_element()
                        } else {
                            div()
                                .size(px(16.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(cx.theme().ui.text_muted.opacity(0.35))
                                .bg(gpui::transparent_black())
                                .into_any_element()
                        };
                        div()
                            .w(px(22.))
                            .flex_shrink_0()
                            .mt(px(3.))
                            .child(checkbox)
                            .into()
                    } else if bullet == "•" {
                        div()
                            .w(px(16.))
                            .flex()
                            .justify_center()
                            .flex_shrink_0()
                            .text_size(px(22.))
                            .child(bullet)
                            .text_color(cx.theme().ui.text)
                            .into()
                    } else {
                        // 有序列表数字：右对齐，缩短与正文距离
                        div()
                            .w(px(24.))
                            .flex()
                            .justify_end()
                            .flex_shrink_0()
                            .text_size(px(14.))
                            .font_weight(gpui::FontWeight(500.))
                            .pr(px(4.))
                            .child(bullet)
                            .text_color(cx.theme().ui.text_muted)
                            .into()
                    };
                    self.push_div(bullet_el, &range, markdown_end);
                    self.pop_div();
                    self.push_div(div().flex_1().w_0().min_w_0(), &range, markdown_end);
                }
                MarkdownTag::BlockQuote => {
                    self.blockquote_depth += 1;
                    self.blockquote_callouts.push(CalloutState::Pending);
                    // 引用卡片：左 3px 边框，淡底圆角
                    let quote_bg = if cx.theme().appearance.is_dark() {
                        cx.theme().ui.text.opacity(0.05)
                    } else {
                        cx.theme().ui.text.opacity(0.03)
                    };
                    let mut block = div()
                        .my_3()
                        .py_3()
                        .pr_4()
                        .rounded(px(6.0))
                        .bg(quote_bg);
                    block.style().refine(&StyleRefinement {
                        padding: gpui::EdgesRefinement {
                            left: Some(gpui::DefiniteLength::Absolute(
                                gpui::AbsoluteLength::Pixels(px(20.)),
                            )),
                            ..Default::default()
                        },
                        border_widths: gpui::EdgesRefinement {
                            left: Some(gpui::AbsoluteLength::Pixels(px(3.))),
                            ..Default::default()
                        },
                        border_color: Some(self.style.block_quote_border_color),
                        ..Default::default()
                    });
                    self.push_div(block, &range, markdown_end);
                    self.push_text_style(self.style.block_quote.clone());
                }
                MarkdownTag::Table(aligns) => {
                    self.table.alignments = aligns;
                    self.table.rows.clear();
                    self.table.current_row.clear();
                    let col_count = self.table.alignments.len().max(1);
                    // 表格：外层滚动 + 内层 grid，圆角 8px
                    let table_border = cx.theme().ui.border.opacity(if cx.theme().appearance.is_dark() { 0.28 } else { 0.22 });
                    let outer = div()
                        .id(format!("table-outer-{}", range.start))
                        .w_full()
                        .mt_6()
                        .mb_4()
                        .overflow_x_scroll()
                        .rounded(px(8.0))
                        .border(px(1.0))
                        .border_color(table_border)
                        .bg(cx.theme().ui.panel_background.opacity(0.5));
                    let grid = div()
                        .grid()
                        .grid_cols(col_count as u16)
                        .w_full()
                        .rounded(px(8.0))
                        .overflow_hidden();
                    self.push_div(outer, &range, markdown_end);
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
                    self.flush_text();
                    self.link_state = Some(LinkState {
                        url: dest_url.clone(),
                        text: String::new(),
                        runs: Vec::new(),
                    });
                    self.push_text_style(self.style.link.clone());
                }
                MarkdownTag::Image { dest_url } => {
                    self.flush_text();
                    self.image_state = Some(ImageState {
                        url: dest_url.clone(),
                        alt: String::new(),
                        runs: Vec::new(),
                    });
                    // 图片 alt 文本走 image_state 收集，不直接进 pending_line
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
                    let anchor = div()
                        .ml_2()
                        .flex_shrink_0()
                        .text_size(px(13.))
                        .text_color(cx.theme().ui.text_muted.opacity(0.35))
                        .invisible()
                        .group_hover("heading", |s| s.visible())
                        .child("#")
                        .into_any_element();
                    self.append_child(anchor);
                    self.pop_div();
                }
                MarkdownTagEnd::CodeBlock => {
                    self.trim_trailing_newline();
                    self.pop_div();
                    let content = self.code_block_content.pop().unwrap_or_default();
                    let code_block_id = self.code_block_ids.pop().unwrap_or_default();
                    let language = self.code_block_language.last().cloned().flatten();
                    let copy_id = gpui::ElementId::Name(SharedString::from(format!(
                        "copy-{}",
                        code_block_id
                    )));

                    if let Some(lang) = language {
                        let label = div()
                            .absolute()
                            .top(px(7.0))
                            .left(px(12.0))
                            .px_1p5()
                            .py_0p5()
                            .rounded(px(4.0))
                            .bg(cx.theme().ui.text.opacity(0.06))
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight(600.))
                                                        .text_color(cx.theme().ui.text_muted)
                            .child(lang.to_uppercase())
                            .into_any_element();
                        self.append_child(label);
                    }

                    let is_copied = self.copied_code_blocks.contains(&code_block_id);
                    let copy_content = content.clone();
                    let markdown_copy = self.markdown.clone();
                    let copy_btn = div()
                        .id(copy_id)
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .invisible()
                        .group_hover("code-block", |style| style.visible())
                        .child(if is_copied {
                            svg()
                                .size(px(16.0))
                                .text_color(cx.theme().ui.success)
                                .path(Icon::Check.path())
                        } else {
                            svg()
                                .size(px(16.0))
                                .text_color(cx.theme().ui.text_muted)
                                .path(Icon::Copy.path())
                        })
                        .on_click(move |_: &ClickEvent, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_content.clone()));
                            markdown_copy.update(cx, |markdown, cx| {
                                markdown.mark_code_block_copied(code_block_id, cx);
                            });
                        })
                        .into_any_element();

                    let buttons = div()
                        .absolute()
                        .top(px(6.0))
                        .right(px(6.0))
                        .flex()
                        .rounded(px(6.0))
                        .bg(cx.theme().editor_background())
                        .border_1()
                        .border_color(cx.theme().ui.border.opacity(0.18))
                        .shadow_sm()
                        .child(copy_btn)
                        .into_any_element();
                    self.append_child(buttons);

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
                    // 若该项为已完成任务，弹出此前压入的删除线样式
                    if self.task_done_stack.pop() == Some(true) {
                        self.pop_text_style();
                    }
                }
                MarkdownTagEnd::BlockQuote => {
                    self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                    self.blockquote_callouts.pop();
                    self.pop_text_style();
                    self.pop_div();
                }
                MarkdownTagEnd::Table => {
                    self.render_table(cx);
                    self.pop_div();
                    self.pop_div();
                }
                MarkdownTagEnd::TableHead => {
                    self.table.rows.push(self.table.current_row.clone());
                }
                MarkdownTagEnd::TableRow => {
                    self.table.rows.push(self.table.current_row.clone());
                }
                MarkdownTagEnd::TableCell => {
                    // 整个 pending_line 入格（含样式 runs），避免丢失加粗/斜体等
                    let line = std::mem::take(&mut self.pending_line);
                    self.table.current_row.push(line);
                }
                MarkdownTagEnd::Emphasis
                | MarkdownTagEnd::Strong
                | MarkdownTagEnd::Strikethrough => {
                    self.pop_text_style();
                }
                MarkdownTagEnd::Link => {
                    self.pop_text_style();
                    if let Some(link) = self.link_state.take() {
                        let styled_text = StyledText::new(link.text).with_runs(link.runs);
                        let url = link.url.clone();
                        let url_for_id = SharedString::from(url.clone());
                        let is_external = url.starts_with("http://") || url.starts_with("https://");
                        let link_el = div()
                            .id(url_for_id)
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .cursor_pointer()
                            .on_click(move |_: &ClickEvent, _, _cx| {
                                let _ = open::that(&url);
                            })
                            .child(styled_text)
                            .when(is_external, |this| {
                                this.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().ui.accent.opacity(0.6))
                                        .child("↗")
                                )
                            })
                            .into_any_element();
                        self.append_child(link_el);
                    }
                }
                MarkdownTagEnd::Image => {
                    if let Some(img_state) = self.image_state.take() {
                        let alt = if img_state.alt.is_empty() {
                            img_state.url.clone()
                        } else {
                            img_state.alt.clone()
                        };
                        // 尝试构造一个 Notion 风格的图片卡片：圆角 8 + 细边框 + 失败占位
                        let border = cx.theme().ui.border.opacity(0.18);
                        let url_for_click = img_state.url.clone();
                        let url_for_id = SharedString::from(img_state.url.clone());
                        // alt 文本用已收集的 runs 保留加粗/斜体（若有）
                        let alt_text = if img_state.alt.is_empty() {
                            String::new()
                        } else {
                            img_state.alt.clone()
                        };
                        let alt_runs = if img_state.runs.is_empty() {
                            None
                        } else {
                            Some(img_state.runs.clone())
                        };
                        let maybe_image = Self::load_local_image(&img_state.url);
                        let image_card: AnyElement = {
                            // 外壳统一样式，内部按是否加载成功分支
                            let shell = div()
                                .id(url_for_id.clone())
                                .flex()
                                .flex_col()
                                .items_center()
                                .my_4()
                                .w_full()
                                .rounded(px(8.))
                                .border(px(1.))
                                .border_color(border)
                                .overflow_hidden()
                                .bg(cx.theme().ui.panel_background)
                                .cursor_pointer()
                                .on_click({
                                    let url = url_for_click.clone();
                                    move |_: &ClickEvent, _, _cx| {
                                        let _ = open::that(&url);
                                    }
                                });
                            if let Some((img_arc, (w, h))) = maybe_image {
                                let max_w = 640.0;
                                let scale = (max_w / w as f32).min(1.0);
                                shell
                                    .child(
                                        div()
                                            .w_full()
                                            .flex()
                                            .justify_center()
                                            .bg(cx.theme().ui.elevated_background)
                                            .child(
                                                gpui::img(img_arc.clone())
                                                    .w(px(w as f32 * scale))
                                                    .h(px(h as f32 * scale))
                                                    .object_fit(gpui::ObjectFit::Contain),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .px_3()
                                            .py_2()
                                            .text_size(px(11.))
                                            .text_color(cx.theme().ui.text_muted)
                                            .text_center()
                                            .child(if alt.is_empty() {
                                                img_state.url.clone()
                                            } else {
                                                alt.clone()
                                            }),
                                    )
                                    .into_any_element()
                            } else {
                                shell
                                    .child(
                                        div()
                                            .w_full()
                                            .px_4()
                                            .py_6()
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .justify_center()
                                            .gap_2()
                                            .bg(cx.theme().ui.elevated_background)
                                            .child(div().text_size(px(18.)).child("🖼"))
                                            .child({
                                                let txt = alt.clone();
                                                let mut t =
                                                    div().text_size(px(13.)).text_color(cx.theme().ui.text);
                                                if let Some(runs) = alt_runs.clone() {
                                                    t = t.child(StyledText::new(txt).with_runs(runs));
                                                } else {
                                                    t = t.child(txt);
                                                }
                                                t
                                            })
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(cx.theme().ui.text_muted)
                                                    .child(img_state.url.clone()),
                                            ),
                                    )
                                    .into_any_element()
                            }
                        };
                        // 若有 alt 标题单独一行 caption
                        let caption = if alt_text.is_empty() || alt_text == img_state.url {
                            None
                        } else {
                            Some(div().px_3().py_2().text_size(px(12.)).text_color(cx.theme().ui.text_muted).child(alt_text.clone()).into_any_element())
                        };
                        let card: AnyElement = image_card;
                        // caption 放在卡片下方外部，保持 Notion 图注风格
                        if let Some(cap) = caption {
                            // 将卡片与 caption 包一层
                            let wrapper = div().flex().flex_col().w_full().child(card).child(cap).into_any_element();
                            self.append_child(wrapper);
                        } else {
                            self.append_child(card);
                        }
                    }
                }
            },
            MarkdownEvent::Text(text) => {
                if let Some(CalloutState::Pending) = self.blockquote_callouts.last().cloned() {
                    if let Some((kind, remainder)) = Self::parse_callout(&text) {
                        *self.blockquote_callouts.last_mut().unwrap() = CalloutState::Kind(kind);
                        let accent = kind.accent_color(cx);
                        let bg = kind.bg_color(cx);
                        // 目标为 blockquote 外层容器（倒数第二层，若当前在段落内），而非当前段落
                        let bq_idx = if self.div_stack.len() >= 3 {
                            self.div_stack.len() - 2
                        } else {
                            self.div_stack.len() - 1
                        };
                        if let Some(entry) = self.div_stack.get_mut(bq_idx) {
                            entry.div.style().refine(&StyleRefinement {
                                border_color: Some(accent),
                                background: Some(bg.into()),
                                border_widths: EdgesRefinement {
                                    left: Some(gpui::AbsoluteLength::Pixels(px(4.))),
                                    ..Default::default()
                                },
                                ..Default::default()
                            });
                            let header = div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .mb_2()
                                .text_size(px(12.))
                                .font_weight(gpui::FontWeight(700.))
                                .text_color(accent)
                                .child(div().size(px(10.)).rounded_full().bg(accent))
                                .child(kind.label().to_string())
                                .into_any_element();
                            entry.push_child(header);
                        } else {
                            // 回退：追加到当前容器
                            let header = div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .mb_2()
                                .text_size(px(12.))
                                .font_weight(gpui::FontWeight(700.))
                                .text_color(accent)
                                .child(div().size(px(8.)).rounded_full().bg(accent))
                                .child(kind.label().to_string())
                                .into_any_element();
                            self.append_child(header);
                        }
                        self.pop_text_style();
                        self.push_text_style(TextStyleRefinement {
                            color: Some(cx.theme().ui.text),
                            ..Default::default()
                        });
                        if let Some(rem) = remainder {
                            self.push_text(&rem, &range, cx);
                        }
                        return;
                    } else {
                        *self.blockquote_callouts.last_mut().unwrap() = CalloutState::NotCallout;
                    }
                }
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
                // Notion 分割线：1px 柔和实线，上下 24px 留白，居中，无硬边框感。
                let rule = div()
                    .h(px(1.))
                    .w_full()
                    .my_6()
                    .bg(self.style.rule_color)
                    .rounded_full()
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

    fn push_text(&mut self, text: &str, range: &Range<usize>, cx: &mut gpui::App) {
        if self.image_state.is_some() {
            let run = self.text_style().to_run(text.len());
            if let Some(img) = self.image_state.as_mut() {
                img.alt.push_str(text);
                img.runs.push(run);
            }
            return;
        }
        if self.link_state.is_some() {
            let run = self.text_style().to_run(text.len());
            if let Some(link) = self.link_state.as_mut() {
                link.text.push_str(text);
                link.runs.push(run);
            }
            return;
        }
        if self
            .code_block_language
            .last()
            .and_then(|l| l.as_ref())
            .is_some()
        {
            self.push_code_text(text, range, cx);
            return;
        }
        if !self.code_block_language.is_empty() {
            if let Some(content) = self.code_block_content.last_mut() {
                content.push_str(text);
            }
        }
        let style = self.text_style();
        self.pending_line.text.push_str(text);
        self.pending_line.runs.push(style.to_run(text.len()));
    }

    fn push_code_text(&mut self, text: &str, range: &Range<usize>, cx: &mut gpui::App) {
        let style = self.text_style();
        let language = self.code_block_language.last().unwrap().clone().unwrap();
        let key = (language.clone(), range.clone());
        let mut cache = self.highlight_cache.borrow_mut();
        let cached = cache
            .entry(key)
            .or_insert_with(|| highlight_code_runs(text, &language, &style, cx.theme()));
        self.pending_line.runs.extend(cached.iter().cloned());
        self.pending_line.text.push_str(text);
        if let Some(content) = self.code_block_content.last_mut() {
            content.push_str(text);
        }
    }

    fn trim_trailing_newline(&mut self) {
        if self.pending_line.text.ends_with('\n') {
            self.pending_line.text.pop();
            if let Some(last_run) = self.pending_line.runs.last_mut() {
                last_run.len -= 1;
            }
        }
    }

    fn flush_text(&mut self) {
        if self.pending_line.text.is_empty() {
            return;
        }
        let styled_text = StyledText::new(self.pending_line.text.clone())
            .with_runs(self.pending_line.runs.clone());
        self.append_child(div().min_w_0().child(styled_text).into_any_element());
        self.pending_line = PendingLine::default();
    }

    fn append_child(&mut self, child: AnyElement) {
        if let Some(entry) = self.div_stack.last_mut() {
            entry.push_child(child);
        }
    }

    fn push_div(&mut self, div: impl Into<AnyDiv>, range: &Range<usize>, markdown_end: usize) {
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
        let alignments = self.table.alignments.clone();
        let border_color = cx.theme().ui.border.opacity(if cx.theme().appearance.is_dark() { 0.28 } else { 0.22 });
        let elevated_bg = cx.theme().ui.elevated_background;
        let panel_bg = cx.theme().ui.panel_background;
        // Notion 表头：略深的底色 + 加粗感（由 cell runs 的 strong 承担），斑马纹保留

        let mut children = Vec::new();
        for (row_idx, row) in self.table.rows.iter().enumerate() {
            let is_header = row_idx == 0;
            for col in 0..col_count {
                let cell = row.get(col);
                let cell_text = cell.map(|c| c.text.as_str()).unwrap_or("");
                let mut cell_div = div().px_3().py_2().text_size(px(14.));
                // 分割线画在自身元素上（同元素内先背景后边框，边框不会被覆盖）。
                // 因此用左/上边框而非右/下边框：右/下边框属于前一个单元格，
                // 会被相邻（后绘制）单元格的背景盖住（表头/隔行背景尤甚）。
                // 第一行/第一列由容器外框兜底。
                let has_left_divider = col > 0;
                let has_top_divider = row_idx > 0;
                if has_left_divider || has_top_divider {
                    cell_div = cell_div.border_color(border_color);
                    if has_left_divider {
                        cell_div = cell_div.border_l(px(1.0));
                    }
                    if has_top_divider {
                        cell_div = cell_div.border_t(px(1.0));
                    }
                }
                if is_header {
                    cell_div = cell_div.bg(elevated_bg);
                } else if row_idx % 2 == 0 {
                    cell_div = cell_div.bg(panel_bg);
                }
                match alignments.get(col).unwrap_or(&ColumnAlign::None) {
                    ColumnAlign::Left => cell_div = cell_div.justify_start(),
                    ColumnAlign::Center => cell_div = cell_div.justify_center(),
                    ColumnAlign::Right => cell_div = cell_div.justify_end(),
                    ColumnAlign::None => {}
                }
                // 优先使用单元格自身保留的样式 runs；空单元格/缺列回退到 base 样式
                let cell_runs = cell
                    .map(|c| c.runs.clone())
                    .filter(|runs| !runs.is_empty())
                    .unwrap_or_else(|| vec![base_style.to_run(cell_text.len())]);
                let cell_styled = StyledText::new(cell_text.to_string()).with_runs(cell_runs);
                cell_div = cell_div.child(cell_styled);
                children.push(cell_div.into_any_element());
            }
        }

        for child in children {
            self.append_child(child);
        }
    }

    fn load_local_image(url: &str) -> Option<(std::sync::Arc<gpui::Image>, (u32, u32))> {
        if url.starts_with("http://") || url.starts_with("https://") {
            return None;
        }
        let p = std::path::Path::new(url);
        for cand in [
            p.to_path_buf(),
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(url),
        ] {
            if let Ok(bytes) = std::fs::read(&cand) {
                if let Ok(img) = crate::editor::image_viewer::load_image_from_bytes(&bytes, &cand) {
                    return Some(img);
                }
            }
        }
        None
    }

    fn parse_callout(text: &str) -> Option<(CalloutKind, Option<String>)> {
        let trimmed = text.trim();
        if let Some(kind) = CalloutKind::from_marker(trimmed) {
            return Some((kind, None));
        }
        if trimmed.starts_with("[!") {
            if let Some(end) = trimmed.find(']') {
                let marker = &trimmed[..=end];
                if let Some(kind) = CalloutKind::from_marker(marker) {
                    let rest = trimmed[end + 1..].trim_start();
                    let remainder = if rest.is_empty() { None } else { Some(rest.to_string()) };
                    return Some((kind, remainder));
                }
            }
        }
        None
    }
}
