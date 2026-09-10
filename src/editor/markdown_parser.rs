use std::ops::Range;

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};

#[derive(Clone, Debug)]
pub enum MarkdownEvent {
    Start(MarkdownTag),
    End(MarkdownTagEnd),
    Text(String),
    Code(String),
    Html(String),
    SoftBreak,
    HardBreak,
    Rule,
    TaskListMarker(bool),
    InlineMath(String),
    DisplayMath(String),
}

#[derive(Clone, Debug)]
pub enum MarkdownTag {
    Paragraph,
    Heading(u8),
    CodeBlock(CodeBlockInfo),
    List(Option<u64>),
    Item,
    BlockQuote,
    Table(Vec<ColumnAlign>),
    TableHead,
    TableRow,
    TableCell,
    Emphasis,
    Strong,
    Strikethrough,
    Link { dest_url: String },
    Image { dest_url: String },
}

#[derive(Clone, Debug, Default)]
pub struct CodeBlockInfo {
    pub language: String,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    Left,
    Center,
    Right,
    None,
}

impl From<Alignment> for ColumnAlign {
    fn from(a: Alignment) -> Self {
        match a {
            Alignment::Left => ColumnAlign::Left,
            Alignment::Center => ColumnAlign::Center,
            Alignment::Right => ColumnAlign::Right,
            Alignment::None => ColumnAlign::None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum MarkdownTagEnd {
    Paragraph,
    Heading(u8),
    CodeBlock,
    List(bool),
    Item,
    BlockQuote,
    Table,
    TableHead,
    TableRow,
    TableCell,
    Emphasis,
    Strong,
    Strikethrough,
    Link,
    Image,
}

pub fn parse_markdown(source: &str) -> Vec<(Range<usize>, MarkdownEvent)> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    options.insert(Options::ENABLE_MATH);

    Parser::new_ext(source, options)
        .into_offset_iter()
        .map(|(event, range)| (range, convert_event(event)))
        .collect()
}

fn convert_event(event: Event<'_>) -> MarkdownEvent {
    match event {
        Event::Start(tag) => MarkdownEvent::Start(convert_tag(tag)),
        Event::End(tag_end) => MarkdownEvent::End(convert_tag_end(tag_end)),
        Event::Text(t) => MarkdownEvent::Text(t.to_string()),
        Event::Code(c) => MarkdownEvent::Code(c.to_string()),
        Event::Html(h) => MarkdownEvent::Html(h.to_string()),
        Event::SoftBreak => MarkdownEvent::SoftBreak,
        Event::HardBreak => MarkdownEvent::HardBreak,
        Event::Rule => MarkdownEvent::Rule,
        Event::TaskListMarker(b) => MarkdownEvent::TaskListMarker(b),
        Event::InlineMath(m) => MarkdownEvent::InlineMath(m.to_string()),
        Event::DisplayMath(m) => MarkdownEvent::DisplayMath(m.to_string()),
        _ => MarkdownEvent::SoftBreak,
    }
}

fn convert_tag(tag: Tag<'_>) -> MarkdownTag {
    match tag {
        Tag::Paragraph => MarkdownTag::Paragraph,
        Tag::Heading { level, .. } => MarkdownTag::Heading(level as u8),
        Tag::CodeBlock(kind) => {
            let language = match kind {
                pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                pulldown_cmark::CodeBlockKind::Indented => String::new(),
            };
            MarkdownTag::CodeBlock(CodeBlockInfo { language })
        }
        Tag::List(start) => MarkdownTag::List(start),
        Tag::Item => MarkdownTag::Item,
        Tag::BlockQuote(_) => MarkdownTag::BlockQuote,
        Tag::Table(alignments) => {
            MarkdownTag::Table(alignments.into_iter().map(ColumnAlign::from).collect())
        }
        Tag::TableHead => MarkdownTag::TableHead,
        Tag::TableRow => MarkdownTag::TableRow,
        Tag::TableCell => MarkdownTag::TableCell,
        Tag::Emphasis => MarkdownTag::Emphasis,
        Tag::Strong => MarkdownTag::Strong,
        Tag::Strikethrough => MarkdownTag::Strikethrough,
        Tag::Link { dest_url, .. } => MarkdownTag::Link {
            dest_url: dest_url.to_string(),
        },
        Tag::Image { dest_url, .. } => MarkdownTag::Image {
            dest_url: dest_url.to_string(),
        },
        _ => MarkdownTag::Paragraph,
    }
}

fn convert_tag_end(tag_end: TagEnd) -> MarkdownTagEnd {
    match tag_end {
        TagEnd::Paragraph => MarkdownTagEnd::Paragraph,
        TagEnd::Heading(level) => MarkdownTagEnd::Heading(level as u8),
        TagEnd::CodeBlock => MarkdownTagEnd::CodeBlock,
        TagEnd::List(start) => MarkdownTagEnd::List(start),
        TagEnd::Item => MarkdownTagEnd::Item,
        TagEnd::BlockQuote(_) => MarkdownTagEnd::BlockQuote,
        TagEnd::Table => MarkdownTagEnd::Table,
        TagEnd::TableHead => MarkdownTagEnd::TableHead,
        TagEnd::TableRow => MarkdownTagEnd::TableRow,
        TagEnd::TableCell => MarkdownTagEnd::TableCell,
        TagEnd::Emphasis => MarkdownTagEnd::Emphasis,
        TagEnd::Strong => MarkdownTagEnd::Strong,
        TagEnd::Strikethrough => MarkdownTagEnd::Strikethrough,
        TagEnd::Link => MarkdownTagEnd::Link,
        TagEnd::Image => MarkdownTagEnd::Image,
        TagEnd::HtmlBlock
        | TagEnd::FootnoteDefinition
        | TagEnd::DefinitionList
        | TagEnd::DefinitionListTitle
        | TagEnd::DefinitionListDefinition
        | TagEnd::MetadataBlock(_) => MarkdownTagEnd::Paragraph,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_math(source: &str) -> bool {
        parse_markdown(source).iter().any(|(_, e)| {
            matches!(
                e,
                MarkdownEvent::InlineMath(_) | MarkdownEvent::DisplayMath(_)
            )
        })
    }

    #[test]
    fn math_events_parsed() {
        assert!(has_math("质能方程 $E = mc^2$ 展示"));
        assert!(has_math("$$E = mc^2$$"));
        assert!(has_math("$$\nE = mc^2\n$$"));
    }

    #[test]
    fn currency_is_not_math() {
        // 与 live `mark_math` 的 pandoc 式规则对齐：货币写法保持原文。
        assert!(!has_math("price $5"));
        assert!(!has_math("$5 and $10"));
    }
}
