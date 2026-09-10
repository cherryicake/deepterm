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
    /// `[^label]` footnote reference (preview renders an accent chip).
    FootnoteRef(String),
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
    /// Definition-list term (bold paragraph).
    DefTerm,
    /// Definition-list definition (indented paragraph).
    DefItem,
    /// `^sup^` content (accent; delimiters consumed).
    Sup,
    /// `~sub~` content (soft accent; delimiters consumed).
    Sub,
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
    DefTerm,
    DefItem,
    Sup,
    Sub,
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
    options.insert(Options::ENABLE_DEFINITION_LIST);

    let events: Vec<(Range<usize>, MarkdownEvent)> = Parser::new_ext(source, options)
        .into_offset_iter()
        .map(|(event, range)| (range, convert_event(event)))
        .collect();
    split_sup_sub(linkify_bare_urls(expand_toc(events, source)))
}

/// Split `^sup^` / `~sub~` out of text runs into synthetic style events
/// (same [`inline::find_sup_sub`](crate::editor::markdown_live::inline::find_sup_sub)
/// rules as the live editor, so both renderers agree).
fn split_sup_sub(
    events: Vec<(Range<usize>, MarkdownEvent)>,
) -> Vec<(Range<usize>, MarkdownEvent)> {
    use crate::editor::markdown_live::inline::find_sup_sub;
    let mut out = Vec::with_capacity(events.len());
    let mut depth = 0usize;
    for (range, event) in events {
        match &event {
            MarkdownEvent::Start(MarkdownTag::Link { .. } | MarkdownTag::Image { .. }) => {
                depth += 1;
            }
            MarkdownEvent::End(MarkdownTagEnd::Link | MarkdownTagEnd::Image) => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        if depth == 0
            && let MarkdownEvent::Text(text) = &event
        {
            let bytes = text.as_bytes();
            let mut at = 0usize;
            let mut pushed = false;
            while at < bytes.len() {
                let rel = bytes[at..].iter().position(|&b| b == b'^' || b == b'~');
                let Some(off) = rel else { break };
                let pos = at + off;
                let Some((full, content)) = find_sup_sub(text, pos) else {
                    at = pos + 1;
                    continue;
                };
                let (tag, end) = if text.as_bytes()[pos] == b'^' {
                    (MarkdownTag::Sup, MarkdownTagEnd::Sup)
                } else {
                    (MarkdownTag::Sub, MarkdownTagEnd::Sub)
                };
                if full.start > at {
                    out.push((
                        range.start + at..range.start + full.start,
                        MarkdownEvent::Text(text[at..full.start].to_string()),
                    ));
                }
                let base = range.start + full.start;
                out.push((base..base + full.len(), MarkdownEvent::Start(tag)));
                out.push((
                    range.start + content.start..range.start + content.end,
                    MarkdownEvent::Text(text[content.clone()].to_string()),
                ));
                out.push((base..base + full.len(), MarkdownEvent::End(end)));
                at = full.end;
                pushed = true;
            }
            if pushed {
                if at < text.len() {
                    out.push((
                        range.start + at..range.end,
                        MarkdownEvent::Text(text[at..].to_string()),
                    ));
                }
                continue;
            }
        }
        out.push((range, event));
    }
    out
}

/// Headings of the document for `[TOC]` expansion: `(level, plain title)`.
fn document_headings(source: &str) -> Vec<(u8, String)> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    let mut out = Vec::new();
    let mut cur: Option<(u8, String)> = None;
    for ev in Parser::new_ext(source, opts) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                cur = Some((level as u8, String::new()));
            }
            Event::End(TagEnd::Heading { .. }) => {
                if let Some((level, title)) = cur.take() {
                    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !title.is_empty() {
                        out.push((level, title));
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, title)) = cur.as_mut() {
                    title.push_str(&t);
                }
            }
            _ => {}
        }
    }
    out
}

/// End index of a `[TOC]`-only paragraph starting at `i` (pulldown splits
/// `[TOC]` into `[` / `TOC` / `]` text events, so concatenate first).
fn toc_paragraph_end(events: &[(Range<usize>, MarkdownEvent)], i: usize) -> Option<usize> {
    if !matches!(
        events.get(i),
        Some((_, MarkdownEvent::Start(MarkdownTag::Paragraph)))
    ) {
        return None;
    }
    let mut j = i + 1;
    let mut buf = String::new();
    while let Some((_, MarkdownEvent::Text(t))) = events.get(j) {
        buf.push_str(t);
        j += 1;
    }
    if !buf.trim().eq_ignore_ascii_case("[toc]") {
        return None;
    }
    matches!(
        events.get(j),
        Some((_, MarkdownEvent::End(MarkdownTagEnd::Paragraph)))
    )
    .then_some(j)
}

/// Replace `[TOC]`-only paragraphs with a static heading outline (Typora
/// inserts a live TOC; the preview renders the same list statically while
/// the live editor's outline sidebar covers the interactive side).
/// Entries reuse the Sup accent style with per-level indent.
fn expand_toc(
    events: Vec<(Range<usize>, MarkdownEvent)>,
    source: &str,
) -> Vec<(Range<usize>, MarkdownEvent)> {
    if !source.to_ascii_lowercase().contains("[toc]") {
        return events;
    }
    let headings = document_headings(source);
    if headings.is_empty() {
        return events;
    }
    let mut out = Vec::with_capacity(events.len());
    let mut i = 0;
    while i < events.len() {
        let Some(toc_end) = toc_paragraph_end(&events, i) else {
            out.push(events[i].clone());
            i += 1;
            continue;
        };
        let (range, _) = events[i].clone();
        out.push((range.clone(), MarkdownEvent::Start(MarkdownTag::Paragraph)));
        for (n, (level, title)) in headings.iter().enumerate() {
            let indent = "  ".repeat((level - 1).min(5) as usize);
            let label = format!("{indent}{title}");
            out.push((range.clone(), MarkdownEvent::Start(MarkdownTag::Sup)));
            out.push((range.clone(), MarkdownEvent::Text(label)));
            out.push((range.clone(), MarkdownEvent::End(MarkdownTagEnd::Sup)));
            if n + 1 < headings.len() {
                out.push((range.clone(), MarkdownEvent::HardBreak));
            }
        }
        out.push((range, MarkdownEvent::End(MarkdownTagEnd::Paragraph)));
        i = toc_end + 1;
    }
    out
}

/// Split bare `http(s)://` / `www.` URLs out of text runs into synthetic
/// link events (Typora linkifies them; pulldown only handles `<..>`
/// autolinks). Runs inside links/images stay untouched.
fn linkify_bare_urls(
    events: Vec<(Range<usize>, MarkdownEvent)>,
) -> Vec<(Range<usize>, MarkdownEvent)> {
    use crate::editor::markdown_live::inline::find_bare_url;
    let mut out = Vec::with_capacity(events.len());
    let mut depth = 0usize;
    for (range, event) in events {
        match &event {
            MarkdownEvent::Start(MarkdownTag::Link { .. } | MarkdownTag::Image { .. }) => {
                depth += 1;
            }
            MarkdownEvent::End(MarkdownTagEnd::Link | MarkdownTagEnd::Image) => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        if depth == 0
            && let MarkdownEvent::Text(text) = &event
        {
            let mut at = 0usize;
            let mut pushed = false;
            while at < text.len() {
                let Some(rel) = text[at..].find("http").or_else(|| text[at..].find("www.")) else {
                    break;
                };
                let abs = at + rel;
                let Some((span, url)) = find_bare_url(text, abs) else {
                    at = abs + 1;
                    continue;
                };
                if span.start > at {
                    out.push((
                        range.start + at..range.start + span.start,
                        MarkdownEvent::Text(text[at..span.start].to_string()),
                    ));
                }
                let base = range.start + span.start;
                out.push((
                    base..base + span.len(),
                    MarkdownEvent::Start(MarkdownTag::Link { dest_url: url.clone() }),
                ));
                out.push((
                    base..base + span.len(),
                    MarkdownEvent::Text(text[span.clone()].to_string()),
                ));
                out.push((
                    base..base + span.len(),
                    MarkdownEvent::End(MarkdownTagEnd::Link),
                ));
                at = span.end;
                pushed = true;
            }
            if pushed {
                if at < text.len() {
                    out.push((
                        range.start + at..range.end,
                        MarkdownEvent::Text(text[at..].to_string()),
                    ));
                }
                continue;
            }
        }
        out.push((range, event));
    }
    out
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
        Event::FootnoteReference(name) => MarkdownEvent::FootnoteRef(name.to_string()),
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
        Tag::DefinitionList => MarkdownTag::Paragraph,
        Tag::DefinitionListTitle => MarkdownTag::DefTerm,
        Tag::DefinitionListDefinition => MarkdownTag::DefItem,
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
        TagEnd::DefinitionList => MarkdownTagEnd::Paragraph,
        TagEnd::DefinitionListTitle => MarkdownTagEnd::DefTerm,
        TagEnd::DefinitionListDefinition => MarkdownTagEnd::DefItem,
        TagEnd::HtmlBlock | TagEnd::FootnoteDefinition | TagEnd::MetadataBlock(_) => {
            MarkdownTagEnd::Paragraph
        }
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

    fn link_urls(source: &str) -> Vec<String> {
        parse_markdown(source)
            .into_iter()
            .filter_map(|(_, e)| match e {
                MarkdownEvent::Start(MarkdownTag::Link { dest_url }) => Some(dest_url),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn bare_urls_linkify_outside_links() {
        assert_eq!(
            link_urls("go https://x.dev/a! end"),
            vec!["https://x.dev/a".to_string()]
        );
        assert_eq!(
            link_urls("go www.x.dev/a. end"),
            vec!["https://www.x.dev/a".to_string()]
        );
        // Link text is not re-split; autolinks still produce one link.
        assert_eq!(
            link_urls("go [https://x.dev](https://y.dev) end"),
            vec!["https://y.dev".to_string()]
        );
        assert_eq!(
            link_urls("go <https://x.dev> end"),
            vec!["https://x.dev".to_string()]
        );
    }

    #[test]
    fn sup_sub_split_into_style_events() {
        let tags: Vec<String> = parse_markdown("E=mc^2^ and H~2~O")
            .into_iter()
            .map(|(_, e)| format!("{e:?}"))
            .collect();
        assert!(tags.iter().any(|t| t == "Start(Sup)"));
        assert!(tags.iter().any(|t| t == "Start(Sub)"));
        let texts: Vec<String> = parse_markdown("E=mc^2^")
            .into_iter()
            .filter_map(|(_, e)| match e {
                MarkdownEvent::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["E=mc".to_string(), "2".to_string()]);
    }

    #[test]
    fn definition_lists_structure() {
        let tags: Vec<String> = parse_markdown("Term\n:  Def\n")
            .into_iter()
            .map(|(_, e)| format!("{e:?}"))
            .collect();
        assert!(tags.iter().any(|t| t == "Start(DefTerm)"));
        assert!(tags.iter().any(|t| t == "Start(DefItem)"));
        let texts: Vec<String> = parse_markdown("Term\n:  Def\n")
            .into_iter()
            .filter_map(|(_, e)| match e {
                MarkdownEvent::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        // The `: ` marker is consumed by pulldown on both sides.
        assert_eq!(texts, vec!["Term".to_string(), "Def".to_string()]);
    }

    #[test]
    fn toc_expands_to_heading_outline() {
        let src = "# A\n\ntext\n\n[TOC]\n\n## B\n";
        let texts: Vec<String> = parse_markdown(src)
            .into_iter()
            .filter_map(|(_, e)| match e {
                MarkdownEvent::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t == "A"));
        assert!(texts.iter().any(|t| t == "  B"));
        assert!(!texts.iter().any(|t| t == "[TOC]"));
        // No headings: left alone.
        let texts: Vec<String> = parse_markdown("[TOC]\n")
            .into_iter()
            .filter_map(|(_, e)| match e {
                MarkdownEvent::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["[".to_string(), "TOC".to_string(), "]".to_string()]);
    }

    #[test]
    fn html_blocks_survive_as_events() {
        let html: Vec<String> = parse_markdown("<div>\nhi\n</div>\n")
            .into_iter()
            .filter_map(|(_, e)| match e {
                MarkdownEvent::Html(h) => Some(h),
                _ => None,
            })
            .collect();
        assert!(!html.is_empty());
        assert!(html.concat().contains("hi"));
    }

    #[test]
    fn footnote_refs_become_events() {
        let refs: Vec<String> = parse_markdown("see [^note]\n\n[^note]: text\n")
            .into_iter()
            .filter_map(|(_, e)| match e {
                MarkdownEvent::FootnoteRef(name) => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(refs, vec!["note".to_string()]);
    }

    #[test]
    fn reference_links_resolve_in_preview() {
        let src = "see [docs][]\n\n[docs]: https://x.dev\n";
        assert_eq!(link_urls(src), vec!["https://x.dev".to_string()]);
    }

    #[test]
    fn currency_is_not_math() {
        // 与 live `mark_math` 的 pandoc 式规则对齐：货币写法保持原文。
        assert!(!has_math("price $5"));
        assert!(!has_math("$5 and $10"));
    }
}
