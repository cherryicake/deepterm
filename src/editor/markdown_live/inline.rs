//! Inline markdown span parsing: emphasis, code, strikethrough, highlight,
//! math, links, and wikilinks over one line (or the content part of one).
//!
//! [`parse`] returns contiguous [`InlineSpan`]s covering the input exactly.
//! Marker bytes (the `**`, backticks, `[[`, url parts, …) are flagged so a
//! renderer can hide them in preview and dim them when the line is revealed.
//! All delimiters are ASCII, so span boundaries always land on char
//! boundaries.
//!
//! The grammar is deliberately small: code spans protect their contents,
//! emphasis uses simplified flanking rules (`_` never matches inside words),
//! and unmatched delimiters stay literal text.

use std::collections::HashMap;
use std::ops::Range;

/// Normalize a reference label (CommonMark: case-insensitive, inner
/// whitespace runs collapse to a single space).
pub fn norm_label(label: &str) -> String {
  label
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase()
}

/// Collect `[label]: destination` definitions over whole lines: reference
/// links resolve document-wide (pulldown parity for the preview).
/// `skip(i)` shields fenced-code/frontmatter/table lines from registering.
pub fn collect_refdefs(lines: &[String], skip: &dyn Fn(usize) -> bool) -> HashMap<String, String> {
  let mut defs = HashMap::new();
  for (i, line) in lines.iter().enumerate() {
    if skip(i) {
      continue;
    }
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
      continue;
    }
    let rest = &line[indent..];
    if !rest.starts_with('[') || rest.starts_with("[^") {
      continue;
    }
    let Some(end) = rest.find(']') else {
      continue;
    };
    let label = norm_label(&rest[1..end]);
    if label.is_empty() {
      continue;
    }
    let after = rest[end + 1..].trim_start();
    let Some(dest) = after.strip_prefix(':') else {
      continue;
    };
    let dest = dest.trim_start();
    let url = match dest.strip_prefix('<') {
      Some(t) => t.split('>').next().unwrap_or("").to_string(),
      None => dest.split_whitespace().next().unwrap_or("").to_string(),
    };
    if !url.is_empty() {
      // First definition wins (CommonMark).
      defs.entry(label).or_insert(url);
    }
  }
  defs
}

/// Style flags accumulated on a span. `link` marks clickable link text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InlineStyle {
  pub bold: bool,
  pub italic: bool,
  pub code: bool,
  pub strike: bool,
  pub highlight: bool,
  pub link: bool,
  /// `$...$` / `$$...$$` formula content (rendered italic, KaTeX approx).
  pub math: bool,
  /// `^sup^` content (accent, delimiters hidden).
  pub sup: bool,
  /// `~sub~` content (soft accent, delimiters hidden).
  pub sub: bool,
  /// `:shortcode:` emoji token (accent, kept verbatim for byte mapping).
  pub emoji: bool,
  /// `[^label]` footnote reference (accent, brackets hidden).
  pub footnote: bool,
}

/// One run of bytes sharing a style. `marker` bytes are markdown syntax;
/// `link` indexes into [`Inline::links`] for the whole construct (text and
/// url alike), so any click inside it can resolve the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSpan {
  pub range: Range<usize>,
  pub marker: bool,
  pub style: InlineStyle,
  pub link: Option<usize>,
}

/// The parse result: spans covering the input exactly, plus link targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inline {
  pub spans: Vec<InlineSpan>,
  pub links: Vec<String>,
}

impl Inline {
  /// The link target covering byte `at`, if any.
  pub fn link_at(&self, at: usize) -> Option<&str> {
    let span = self
      .spans
      .iter()
      .find(|s| s.range.contains(&at) && s.link.is_some())?;
    Some(&self.links[span.link?])
  }

  /// Byte range of the **inline element** containing `at` (delimiters
  /// included) — the construct an expansion would apply to.
  ///
  /// The parser gives a construct's delimiters the **same style** as its
  /// content (only `marker` differs), so the element is the maximal run of
  /// spans sharing the anchor's style + link. Plain text has the default
  /// style, which is why a caret in plain text expands nothing.
  ///
  /// Nested constructs return the **outermost** element, matching Typora
  /// (`Brush.expand` walks up to the outermost expandable ancestor).
  pub fn element_at(&self, at: usize) -> Option<Range<usize>> {
    let idx = self.spans.iter().position(|s| s.range.contains(&at))?;
    // The caret's byte decides which construct it belongs to:
    //  - on content  → that content's construct;
    //  - on a **closing** delimiter (content of the same style precedes it)
    //    → the construct to its left;
    //  - on an **opening** delimiter (content of the same style follows)
    //    → the construct to its right.
    let anchor = if self.spans[idx].marker {
      let style = self.spans[idx].style.clone();
      let link = self.spans[idx].link;
      let same = |s: &InlineSpan| s.link == link && s.style == style && !s.marker;
      let left = self.spans[..idx].iter().rposition(same);
      let right = self.spans[idx + 1..]
        .iter()
        .position(same)
        .map(|off| idx + 1 + off);
      match (left, right) {
        // Prefer the content that immediately abuts this delimiter: a
        // closing marker has its content on the left, an opening one on
        // the right.
        (Some(l), Some(r)) => {
          let l_gap = self.spans[l].range.end == self.spans[idx].range.start;
          let r_gap = self.spans[idx].range.end == self.spans[r].range.start;
          if l_gap {
            l
          } else if r_gap {
            r
          } else {
            r
          }
        }
        (Some(l), None) => l,
        (None, Some(r)) => r,
        (None, None) => idx,
      }
    } else {
      idx
    };
    let style = self.spans[anchor].style.clone();
    let link = self.spans[anchor].link;
    // A link construct is identified by its `link` index (its text carries
    // `link_text` while its `[`/`](url)` delimiters do not, so style alone
    // would split the construct). Everything else groups by style.
    let belongs = |span: &InlineSpan| {
      if link.is_some() {
        span.link == link
      } else {
        span.link.is_none() && span.style == style
      }
    };
    // Plain text (default style, no link) is not an expandable element:
    // there are no markers to reveal, so a caret in it expands nothing.
    let is_default = |s: &InlineStyle| {
      !s.bold
        && !s.italic
        && !s.code
        && !s.strike
        && !s.highlight
        && !s.math
        && !s.sup
        && !s.sub
        && !s.emoji
        && !s.footnote
        && !s.link
    };
    if link.is_none() && is_default(&style) {
      return Some(self.spans[idx].range.clone());
    }
    let mut start = anchor;
    let mut end = anchor;
    while start > 0 && belongs(&self.spans[start - 1]) {
      start -= 1;
    }
    while end + 1 < self.spans.len() && belongs(&self.spans[end + 1]) {
      end += 1;
    }
    Some(self.spans[start].range.start..self.spans[end].range.end)
  }

  /// Typora `.md-expand`: the element whose markers should be revealed for
  /// a caret at byte `at`, or `None` to keep everything rendered.
  ///
  /// Rule: the caret is **inside** the element → that element's delimiters
  /// show, everything else in the line stays rendered. This is Typora's
  /// documented hybrid-editing behavior — "Moving the cursor to the middle
  /// of a span element will expand that element into the Markdown source"
  /// (support.typora.io, *Markdown Reference* §Span Elements).
  ///
  /// Plain text has no delimiters, so a caret there expands nothing.
  pub fn expand_at(&self, at: usize) -> Option<Range<usize>> {
    let el = self.element_at(at)?;
    let has_marker = self
      .spans
      .iter()
      .any(|s| s.marker && el.start <= s.range.start && s.range.end <= el.end);
    has_marker.then_some(el)
  }
}

const NO_LINK: usize = usize::MAX;

/// Byte-attribute working set for one parse.
struct Attrs {
  marker: Vec<bool>,
  code: Vec<bool>,
  bold: Vec<bool>,
  italic: Vec<bool>,
  strike: Vec<bool>,
  highlight: Vec<bool>,
  math: Vec<bool>,
  /// Byte shielded by a preceding backslash escape (`\*`): invisible to all
  /// later passes, rendered as literal text.
  escaped: Vec<bool>,
  /// Link *style* (underlined text), content bytes only.
  link_text: Vec<bool>,
  /// Link target index for the whole construct.
  link_idx: Vec<usize>,
  sup: Vec<bool>,
  sub: Vec<bool>,
  emoji: Vec<bool>,
  footnote: Vec<bool>,
}

/// Parse one line's inline markup (no reference definitions: `[t][r]`
/// and `[t]` shortcuts stay literal without a doc-wide table).
pub fn parse(text: &str) -> Inline {
  parse_with_refs(text, &HashMap::new())
}

/// Parse with a document-wide reference table from [`collect_refdefs`].
pub fn parse_with_refs(text: &str, refs: &HashMap<String, String>) -> Inline {
  let n = text.len();
  let mut a = Attrs {
    marker: vec![false; n],
    code: vec![false; n],
    bold: vec![false; n],
    italic: vec![false; n],
    strike: vec![false; n],
    highlight: vec![false; n],
    math: vec![false; n],
    escaped: vec![false; n],
    link_text: vec![false; n],
    link_idx: vec![NO_LINK; n],
    sup: vec![false; n],
    sub: vec![false; n],
    emoji: vec![false; n],
    footnote: vec![false; n],
  };
  let mut links = Vec::new();

  mark_escapes(text, &mut a);
  mark_code_spans(text, &mut a);
  mark_links(text, &mut a, &mut links);
  mark_reflinks(text, &mut a, &mut links, refs);
  mark_autolinks(text, &mut a, &mut links);
  mark_footnotes(text, &mut a);
  mark_sup_sub(text, &mut a);
  mark_emoji(text, &mut a);
  mark_math(text, &mut a);
  mark_emphasis(text, &mut a);

  // Coalesce per-byte attributes into spans.
  let mut spans: Vec<InlineSpan> = Vec::new();
  for i in 0..n {
    let style = InlineStyle {
      bold: a.bold[i],
      italic: a.italic[i],
      code: a.code[i],
      strike: a.strike[i],
      highlight: a.highlight[i],
      link: a.link_text[i],
      math: a.math[i],
      sup: a.sup[i],
      sub: a.sub[i],
      emoji: a.emoji[i],
      footnote: a.footnote[i],
    };
    let link = (a.link_idx[i] != NO_LINK).then_some(a.link_idx[i]);
    match spans.last_mut() {
      Some(last) if last.marker == a.marker[i] && last.style == style && last.link == link => {
        last.range.end = i + 1;
      }
      _ => spans.push(InlineSpan {
        range: i..i + 1,
        marker: a.marker[i],
        style,
        link,
      }),
    }
  }
  Inline { spans, links }
}

fn set(flags: &mut [bool], range: Range<usize>) {
  flags[range].iter_mut().for_each(|f| *f = true);
}

/// Backslash escapes: `\` + ASCII punctuation renders the punctuation
/// literally. The backslash is a marker (hidden in preview, dimmed when
/// revealed); the escaped byte is shielded from every later pass.
fn mark_escapes(text: &str, a: &mut Attrs) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'\\'
      && bytes.get(i + 1).is_some_and(|b| b.is_ascii_punctuation())
    {
      a.marker[i] = true;
      a.escaped[i + 1] = true;
      i += 2;
    } else {
      i += 1;
    }
  }
}

/// Backtick code spans: a run of N backticks closes on the next run of
/// exactly N. Contents are protected from every later pass.
fn mark_code_spans(text: &str, a: &mut Attrs) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if a.escaped[i] || bytes[i] != b'`' {
      i += 1;
      continue;
    }
    let open = run_len(bytes, i, b'`');
    let mut j = i + open;
    let close = loop {
      match bytes[j..].iter().position(|&b| b == b'`') {
        None => break None,
        Some(off) => {
          let at = j + off;
          if a.escaped[at] {
            j = at + 1;
            continue;
          }
          let len = run_len(bytes, at, b'`');
          if len == open {
            break Some(at);
          }
          j = at + len;
        }
      }
    };
    match close {
      Some(at) => {
        set(&mut a.code, i..at + open);
        set(&mut a.marker, i..i + open);
        set(&mut a.marker, at..at + open);
        i = at + open;
      }
      None => i += open,
    }
  }
}

/// `[[target]]`, `[[target|alias]]`, `[text](url)`, and `![alt](url)`.
fn mark_links(text: &str, a: &mut Attrs, links: &mut Vec<String>) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if a.code[i] || a.escaped[i] || bytes[i] != b'[' && !(bytes[i] == b'!' && bytes.get(i + 1) == Some(&b'[')) {
      i += 1;
      continue;
    }
    let start = i;
    let bracket = if bytes[i] == b'!' { i + 1 } else { i };

    // Wikilink.
    if bytes[i] != b'!' && bytes.get(i + 1) == Some(&b'[') {
      if let Some(end) = find_outside_code(text, a, i + 2, "]]") {
        let inner = i + 2..end;
        let pipe = text[inner.clone()].find('|').map(|p| i + 2 + p);
        let (target, vis) = match pipe {
          Some(p) => (&text[i + 2..p], p + 1..end),
          None => (&text[inner.clone()], inner.clone()),
        };
        let idx = links.len();
        links.push(target.to_string());
        set(&mut a.marker, i..vis.start);
        set(&mut a.marker, end..end + 2);
        set(&mut a.link_text, vis);
        a.link_idx[i..end + 2].iter_mut().for_each(|l| *l = idx);
        i = end + 2;
        continue;
      }
      i += 2;
      continue;
    }

    // `[text](url)` / `![alt](url)`.
    let Some(text_end) = find_outside_code(text, a, bracket + 1, "]") else {
      i = bracket + 1;
      continue;
    };
    if bytes.get(text_end + 1) != Some(&b'(') {
      i = bracket + 1;
      continue;
    }
    let Some(url_end) = find_outside_code(text, a, text_end + 2, ")") else {
      i = bracket + 1;
      continue;
    };
    let idx = links.len();
    links.push(text[text_end + 2..url_end].to_string());
    set(&mut a.marker, start..bracket + 1);
    set(&mut a.marker, text_end..url_end + 1);
    set(&mut a.link_text, bracket + 1..text_end);
    a.link_idx[start..url_end + 1]
      .iter_mut()
      .for_each(|l| *l = idx);
    i = url_end + 1;
  }
}

/// `[text][ref]`, `[text][]`, `[text]` shortcuts and `![alt][ref]`
/// (CommonMark reference links, resolved via `refs`). Runs after
/// `mark_links`, so `[text](url)` constructs are already claimed.
/// Brackets of resolved constructs become markers (hidden in preview);
/// unresolved stays literal. `[^..]` is left for the footnote pass.
fn mark_reflinks(
  text: &str,
  a: &mut Attrs,
  links: &mut Vec<String>,
  refs: &HashMap<String, String>,
) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if a.code[i] || a.escaped[i] || a.marker[i] || a.link_idx[i] != NO_LINK {
      i += 1;
      continue;
    }
    let is_image = bytes[i] == b'!';
    if is_image && bytes.get(i + 1) != Some(&b'[') {
      i += 1;
      continue;
    }
    if !is_image && bytes[i] != b'[' {
      i += 1;
      continue;
    }
    let start = i;
    let bracket = if is_image { i + 1 } else { i };
    // Wikilinks and footnotes belong to other passes.
    if !is_image && bytes.get(bracket + 1) == Some(&b'[') {
      i += 1;
      continue;
    }
    if bytes.get(bracket + 1) == Some(&b'^') {
      i = bracket + 1;
      continue;
    }
    let Some(text_end) = find_outside_code(text, a, bracket + 1, "]") else {
      i = bracket + 1;
      continue;
    };
    // `[text](url)` claimed or broken: leave to `mark_links`.
    if bytes.get(text_end + 1) == Some(&b'(') {
      i = bracket + 1;
      continue;
    }
    let label_here = &text[bracket + 1..text_end];
    // Full `[text][ref]` / collapsed `[text][]`.
    let mut ref_end = text_end;
    let lookup: Option<String> = match bytes.get(text_end + 1) {
      Some(b'[') => {
        let Some(close) = find_outside_code(text, a, text_end + 2, "]") else {
          i = bracket + 1;
          continue;
        };
        ref_end = close;
        let inner = &text[text_end + 2..close];
        Some(if inner.is_empty() {
          norm_label(label_here)
        } else {
          norm_label(inner)
        })
      }
      // Shortcut `[text]`: the text itself is the label.
      _ => Some(norm_label(label_here)),
    };
    let Some(label) = lookup else {
      i = bracket + 1;
      continue;
    };
    if label.is_empty() {
      i = bracket + 1;
      continue;
    }
    let Some(url) = refs.get(&label) else {
      i = bracket + 1;
      continue;
    };
    let idx = links.len();
    links.push(url.clone());
    set(&mut a.marker, start..bracket + 1);
    set(&mut a.marker, text_end..ref_end + 1);
    set(&mut a.link_text, bracket + 1..text_end);
    a.link_idx[start..ref_end + 1]
      .iter_mut()
      .for_each(|l| *l = idx);
    i = ref_end + 1;
  }
}

/// `<https://..>` / `<mail@..>` autolinks plus bare `http(s)://` and `www.`
/// URLs (Typora linkifies both). Angle brackets become markers; bare URLs
/// keep their text with link style. Interiors are shielded from emphasis
/// and math so URL punctuation never triggers markup.
fn mark_autolinks(text: &str, a: &mut Attrs, links: &mut Vec<String>) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if a.code[i] || a.escaped[i] || a.marker[i] || a.link_idx[i] != NO_LINK {
      i += 1;
      continue;
    }
    if bytes[i] == b'<' {
      if let Some((range, url)) = angle_autolink(text, i) {
        if range.start > i || a.code[range.start..range.end].iter().any(|&c| c) {
          i += 1;
          continue;
        }
        let idx = links.len();
        links.push(url);
        set(&mut a.marker, range.start..range.start + 1);
        set(&mut a.marker, range.end - 1..range.end);
        set(&mut a.link_text, range.start + 1..range.end - 1);
        set(&mut a.escaped, range.start + 1..range.end - 1);
        a.link_idx[range.clone()].iter_mut().for_each(|l| *l = idx);
        i = range.end;
        continue;
      }
      i += 1;
      continue;
    }
    if is_bare_url_start(bytes, i) {
      if let Some((range, url)) = find_bare_url(text, i)
        && range.start == i
      {
        let idx = links.len();
        links.push(url);
        set(&mut a.link_text, range.clone());
        set(&mut a.escaped, range.clone());
        a.link_idx[range.clone()]
          .iter_mut()
          .for_each(|l| *l = idx);
        i = range.end;
        continue;
      }
    }
    i += 1;
  }
}

/// `<scheme:...>` or `<user@host.tld>` at byte `from` (must be `<`).
/// Returns the full `<>` range plus the link destination (`mailto:` is
/// added for bare emails, matching pulldown).
pub fn angle_autolink(text: &str, from: usize) -> Option<(Range<usize>, String)> {
  if text.as_bytes().get(from) != Some(&b'<') {
    return None;
  }
  let rest = &text[from + 1..];
  let close = rest.find('>')?;
  let inner = &rest[..close];
  if inner.is_empty() || inner.contains(['<', ' ', '\t', '\n']) {
    return None;
  }
  let url = if let Some((scheme, _)) = inner.split_once(':') {
    if scheme.len() < 2
      || scheme.len() > 32
      || !scheme
        .bytes()
        .enumerate()
        .all(|(k, b)| b.is_ascii_alphanumeric() || (k > 0 && matches!(b, b'+' | b'-' | b'.')))
      || !scheme.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
    {
      return None;
    }
    inner.to_string()
  } else {
    // Bare email: user@host.tld minimum.
    let (user, host) = inner.split_once('@')?;
    if user.is_empty() || !host.contains('.') || host.starts_with('.') || host.ends_with('.') {
      return None;
    }
    format!("mailto:{inner}")
  };
  Some((from..from + 1 + close + 1, url))
}

/// True when a bare URL may start at `i` (scheme or `www.` prefix).
fn is_bare_url_start(bytes: &[u8], i: usize) -> bool {
  let rest = &bytes[i..];
  (rest.starts_with(b"http://") || rest.starts_with(b"https://") || rest.starts_with(b"www."))
    && (i == 0 || {
      let prev = bytes[i - 1];
      !(prev.is_ascii_alphanumeric() || prev == b'/' || prev == b'@')
    })
}

/// Bare URL anchored at byte `from` (must satisfy [`is_bare_url_start`]).
/// Returns the trimmed range plus destination (`https://` prepended for
/// `www.`). Trailing punctuation is trimmed and `)` balanced, GFM-style.
pub fn find_bare_url(text: &str, from: usize) -> Option<(Range<usize>, String)> {
  if !is_bare_url_start(text.as_bytes(), from) {
    return None;
  }
  let mut end = from;
  for (off, c) in text[from..].char_indices() {
    if c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`') {
      break;
    }
    end = from + off + c.len_utf8();
  }
  let mut url = &text[from..end];
  // Trim trailing punctuation runs.
  loop {
    let t = url.trim_end_matches(['?', '!', '.', ',', ';', ':', '*', '_', '~', '\'', '"', ']', '}']);
    if t.len() == url.len() {
      break;
    }
    url = t;
  }
  // Balance parentheses: drop surplus closers from the right.
  let (mut open, mut close) = (0, 0);
  for c in url.chars() {
    match c {
      '(' => open += 1,
      ')' => close += 1,
      _ => {}
    }
  }
  while close > open && url.ends_with(')') {
    url = &url[..url.len() - 1];
    close -= 1;
  }
  if url.is_empty() || url == "http://" || url == "https://" || url == "www." {
    return None;
  }
  let end = from + url.len();
  let dest = if url.starts_with("www.") {
    format!("https://{url}")
  } else {
    url.to_string()
  };
  Some((from..end, dest))
}

/// `[^label]` footnote references (Typora renders them as superscript
/// chips; here the brackets hide and the label takes the footnote style).
/// Definitions (`[^label]: ...`) are left alone. No click target: unlike
/// links there is nowhere external to open.
fn mark_footnotes(text: &str, a: &mut Attrs) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if a.code[i] || a.escaped[i] || a.marker[i] || a.link_idx[i] != NO_LINK {
      i += 1;
      continue;
    }
    if bytes[i] != b'[' || bytes.get(i + 1) != Some(&b'^') {
      i += 1;
      continue;
    }
    let Some(end) = find_outside_code(text, a, i + 2, "]") else {
      i += 2;
      continue;
    };
    if end == i + 2 || bytes.get(end + 1) == Some(&b':') {
      i += 2;
      continue;
    }
    set(&mut a.marker, i..i + 2);
    set(&mut a.marker, end..end + 1);
    set(&mut a.footnote, i + 2..end);
    i = end + 1;
  }
}

/// `^sup^` / `~sub~` (Typora): single delimiters with non-space-adjacent,
/// non-empty content become markers and the content takes the sup/sub
/// style. `^^`, `~~` (strike), code spans and URLs are never claimed.
fn mark_sup_sub(text: &str, a: &mut Attrs) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    let b = bytes[i];
    if b != b'^' && b != b'~' {
      i += 1;
      continue;
    }
    if a.code[i] || a.escaped[i] || a.marker[i] {
      i += 1;
      continue;
    }
    if let Some((full, content)) = find_sup_sub(text, i) {
      if a.code[full.clone()].iter().any(|&c| c)
        || a.escaped[full.clone()].iter().any(|&c| c)
        || a.marker[full.clone()].iter().any(|&c| c)
      {
        i += 1;
        continue;
      }
      let flags = if b == b'^' { &mut a.sup } else { &mut a.sub };
      set(&mut a.marker, full.start..content.start);
      set(&mut a.marker, content.end..full.end);
      set(flags, content.clone());
      i = full.end;
      continue;
    }
    i += 1;
  }
}

/// Single-delimiter `^..^` / `~..~` anchored at byte `from` (must be the
/// delimiter). Returns the full range plus the content range. Shared with
/// the preview's event splitter so both renderers agree.
pub fn find_sup_sub(text: &str, from: usize) -> Option<(Range<usize>, Range<usize>)> {
  let bytes = text.as_bytes();
  let b = *bytes.get(from)?;
  if b != b'^' && b != b'~' {
    return None;
  }
  // Singles only: `^^` / `~~` belong to other constructs.
  if bytes.get(from + 1) == Some(&b) || (from > 0 && bytes[from - 1] == b) {
    return None;
  }
  let after_open = from + 1;
  if text[after_open..].chars().next().is_none_or(|c| c.is_whitespace()) {
    return None;
  }
  let mut j = after_open;
  while j < bytes.len() {
    if bytes[j] == b
      && bytes.get(j + 1) != Some(&b)
      && (j == 0 || bytes[j - 1] != b)
      && !text[after_open..j].trim().is_empty()
      && !text[..j].chars().next_back().is_some_and(|c| c.is_whitespace())
    {
      return Some((from..j + 1, after_open..j));
    }
    j += 1;
  }
  None
}

/// `:shortcode:` emoji tokens (Typora swaps the glyph; the live editor
/// styles the token since its mapping is byte-exact). Unknown or malformed
/// codes stay literal text.
fn mark_emoji(text: &str, a: &mut Attrs) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] != b':' || a.code[i] || a.escaped[i] || a.marker[i] {
      i += 1;
      continue;
    }
    if let Some((range, _)) = super::emoji::find_shortcode(text, i) {
      if a.code[range.clone()].iter().any(|&c| c)
        || a.escaped[range.clone()].iter().any(|&c| c)
        || a.marker[range.clone()].iter().any(|&c| c)
      {
        i += 1;
        continue;
      }
      set(&mut a.emoji, range.clone());
      i = range.end;
      continue;
    }
    i += 1;
  }
}

/// `$...$` / `$$...$$` formulas (Typora math): delimiters become markers,
/// content gets the math style. Runs after code/escapes so `$` inside code
/// spans stays literal. Delimiter rules (pandoc-like, avoids `$5 and $10`
/// misfires): the opener must be followed by a non-space, the closer must
/// be preceded by a non-space and followed by a non-alphanumeric, and the
/// content must contain a non-space. Unmatched `$` stays literal.
fn mark_math(text: &str, a: &mut Attrs) {
  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] != b'$' || a.code[i] || a.escaped[i] {
      i += 1;
      continue;
    }
    let double = bytes.get(i + 1) == Some(&b'$');
    let open_end = if double { i + 2 } else { i + 1 };
    let next = bytes.get(open_end);
    if next.is_none_or(|b| b.is_ascii_whitespace()) {
      i = open_end;
      continue;
    }
    // Find the closer on the same line.
    let mut j = open_end;
    let mut close: Option<usize> = None;
    while j < bytes.len() {
      if bytes[j] == b'$' && !a.code[j] && !a.escaped[j] {
        if double {
          if bytes.get(j + 1) == Some(&b'$') {
            close = Some(j);
            break;
          }
          j += 1;
          continue;
        }
        // A `$` starting a `$$` pair is not a single closer.
        if bytes.get(j + 1) == Some(&b'$') {
          j += 2;
          continue;
        }
        close = Some(j);
        break;
      }
      j += 1;
    }
    let Some(c) = close else {
      i = open_end;
      continue;
    };
    let prev_ok = c > open_end && !bytes[c - 1].is_ascii_whitespace();
    let after = if double { c + 2 } else { c + 1 };
    let next_ok = bytes
      .get(after)
      .is_none_or(|b| !b.is_ascii_alphanumeric());
    let content_ok = text[open_end..c].bytes().any(|b| !b.is_ascii_whitespace());
    if !prev_ok || !next_ok || !content_ok {
      i = open_end;
      continue;
    }
    set(&mut a.marker, i..open_end);
    set(&mut a.marker, c..after);
    set(&mut a.math, open_end..c);
    i = after;
  }
}

/// `**bold**`, `*italic*`, `***both***` (and the `_` forms), `~~strike~~`,
/// `==highlight==`. Unmatched delimiters stay literal.
fn mark_emphasis(text: &str, a: &mut Attrs) {
  // Open delimiter marker ranges, per construct.
  let mut bold: Option<Range<usize>> = None;
  let mut italic: Option<Range<usize>> = None;
  let mut strike: Option<Range<usize>> = None;
  let mut highlight: Option<Range<usize>> = None;

  let bytes = text.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    let b = bytes[i];
    if a.code[i] || a.marker[i] || a.escaped[i] || !matches!(b, b'*' | b'_' | b'~' | b'=') {
      i += 1;
      continue;
    }
    let len = run_len(bytes, i, b);
    let prev = text[..i].chars().next_back();
    let next = text[i + len..].chars().next();
    let mut can_open = next.is_some_and(|c| !c.is_whitespace());
    let mut can_close = prev.is_some_and(|c| !c.is_whitespace());
    if b == b'_' {
      // No intraword emphasis for underscores (snake_case stays plain).
      can_open &= prev.is_none_or(|c| !c.is_alphanumeric());
      can_close &= next.is_none_or(|c| !c.is_alphanumeric());
    }

    match b {
      b'~' | b'=' if len == 2 => {
        let (open, flags) = if b == b'~' {
          (&mut strike, &mut a.strike)
        } else {
          (&mut highlight, &mut a.highlight)
        };
        if let Some(o) = open.take_if(|_| can_close) {
          set(flags, o.start..i + 2);
          set(&mut a.marker, o);
          set(&mut a.marker, i..i + 2);
        } else if can_open {
          *open = Some(i..i + 2);
        }
      }
      b'*' | b'_' => {
        // A run of 3+ acts as bold (outer two) plus italic (inner one).
        let (close_i, close_b) = match len {
          1 => (can_close && italic.is_some(), false),
          2 => (false, can_close && bold.is_some()),
          _ => (can_close && italic.is_some(), can_close && bold.is_some()),
        };
        let mut at = i;
        if close_i {
          let o = italic.take().unwrap();
          set(&mut a.italic, o.start..at + 1);
          set(&mut a.marker, o);
          set(&mut a.marker, at..at + 1);
          at += 1;
        }
        if close_b && at + 2 <= i + len {
          let o = bold.take().unwrap();
          set(&mut a.bold, o.start..at + 2);
          set(&mut a.marker, o);
          set(&mut a.marker, at..at + 2);
          at += 2;
        }
        if at == i && can_open {
          match len {
            1 => italic = Some(i..i + 1),
            2 => bold = Some(i..i + 2),
            _ => {
              bold = Some(i..i + 2);
              italic = Some(i + 2..i + 3);
            }
          }
        }
      }
      _ => {}
    }
    i += len;
  }
}

/// Length of the run of `b` starting at `i`.
fn run_len(bytes: &[u8], i: usize, b: u8) -> usize {
  bytes[i..].iter().take_while(|&&x| x == b).count()
}

/// The byte offset of `needle` at or after `from`, skipping code spans and
/// backslash-escaped bytes.
fn find_outside_code(text: &str, a: &Attrs, from: usize, needle: &str) -> Option<usize> {
  let mut at = from;
  while at < text.len() {
    let off = text[at..].find(needle)?;
    let pos = at + off;
    if !a.code[pos] && !a.escaped[pos] {
      return Some(pos);
    }
    at = pos + needle.len();
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  /// (text, marker) fragments, in order — the readable spine of a parse.
  fn frags(text: &str) -> Vec<(String, bool)> {
    parse(text)
      .spans
      .iter()
      .map(|s| (text[s.range.clone()].to_string(), s.marker))
      .collect()
  }

  fn style_of(text: &str, frag: &str) -> InlineStyle {
    let at = text.find(frag).unwrap();
    let inline = parse(text);
    let span = inline.spans.iter().find(|s| s.range.contains(&at)).unwrap();
    span.style
  }

  #[test]
  fn spans_cover_input_exactly() {
    for text in ["", "plain", "**b** *i* `c` ~~s~~ ==h== [t](u) [[w]]"] {
      let inline = parse(text);
      let mut at = 0;
      for s in &inline.spans {
        assert_eq!(s.range.start, at);
        at = s.range.end;
      }
      assert_eq!(at, text.len());
    }
  }

  #[test]
  fn math_delimiters_hidden_content_flagged() {
    // `$...$`: delimiters are markers, content carries math.
    assert_eq!(
      frags("$x + y$"),
      vec![("$".into(), true), ("x + y".into(), false), ("$".into(), true)]
    );
    assert!(style_of("$x + y$", "x + y").math);
    // `$$...$$` display form works inline too.
    assert!(style_of("before $$x$$ after", "x").math);
    // Unmatched `$` stays literal text.
    assert!(!style_of("price $5", "$5").math);
    // Currency-like `$5 and $10` must not parse as math.
    let inline = parse("$5 and $10");
    assert!(inline.spans.iter().all(|s| !s.style.math));
    // `$` inside code spans stays literal.
    let inline = parse("`$x$`");
    assert!(inline.spans.iter().all(|s| !s.style.math));
    // Escaped `\\$` stays literal: `\\$x$` has no math spans.
    let inline = parse("\\$x$");
    assert!(inline.spans.iter().all(|s| !s.style.math));
  }

  #[test]
  fn bold_italic_and_both() {
    assert_eq!(
      frags("**bold**"),
      vec![
        ("**".into(), true),
        ("bold".into(), false),
        ("**".into(), true)
      ]
    );
    assert!(style_of("**bold**", "bold").bold);
    assert!(style_of("*it*", "it").italic);
    assert!(style_of("_it_", "it").italic);
    assert!(style_of("__b__", "b").bold);
    let s = style_of("***both***", "both");
    assert!(s.bold && s.italic);
  }

  #[test]
  fn unmatched_delimiters_stay_literal() {
    assert_eq!(frags("a ** b"), vec![("a ** b".into(), false)]);
    assert_eq!(frags("*x"), vec![("*x".into(), false)]);
    assert_eq!(frags("2 * 3 * 4"), vec![("2 * 3 * 4".into(), false)]);
  }

  #[test]
  fn underscores_never_match_inside_words() {
    assert_eq!(
      frags("snake_case_name"),
      vec![("snake_case_name".into(), false)]
    );
    assert!(style_of("_word_", "word").italic);
  }

  #[test]
  fn code_spans_protect_contents() {
    let text = "`**not bold**`";
    assert!(!style_of(text, "not bold").bold);
    assert!(style_of(text, "not bold").code);
    // Double-backtick span containing a single backtick.
    let text = "``a ` b``";
    assert!(style_of(text, "a ` b").code);
  }

  #[test]
  fn strike_and_highlight() {
    assert!(style_of("~~gone~~", "gone").strike);
    assert!(style_of("==note==", "note").highlight);
    assert_eq!(frags("a ~ b"), vec![("a ~ b".into(), false)]);
    assert_eq!(frags("a == b"), vec![("a == b".into(), false)]);
  }

  #[test]
  fn nesting_composes() {
    let s = style_of("**bold *and italic***", "and italic");
    assert!(s.bold && s.italic);
    let s = style_of("*it with `code`*", "code");
    assert!(s.italic && s.code);
  }

  #[test]
  fn markdown_link() {
    let text = "see [docs](https://x.dev) now";
    let inline = parse(text);
    assert_eq!(inline.links, vec!["https://x.dev".to_string()]);
    assert!(style_of(text, "docs").link);
    // Url bytes are markers, and clicking anywhere in the construct
    // resolves the target.
    assert_eq!(
      frags(text),
      vec![
        ("see ".into(), false),
        ("[".into(), true),
        ("docs".into(), false),
        ("](https://x.dev)".into(), true),
        (" now".into(), false),
      ]
    );
    assert_eq!(
      inline.link_at(text.find("https").unwrap()),
      Some("https://x.dev")
    );
    assert_eq!(inline.link_at(0), None);
  }

  #[test]
  fn image_parses_like_a_link() {
    let text = "![alt](img.png)";
    let inline = parse(text);
    assert_eq!(inline.links, vec!["img.png".to_string()]);
    assert_eq!(frags(text)[0], ("![".into(), true));
  }

  fn parse_refs(text: &str) -> Inline {
    let lines = vec![text.to_string(), "[docs]: https://x.dev".to_string()];
    let refs = collect_refdefs(&lines, &|_| false);
    parse_with_refs(text, &refs)
  }

  #[test]
  fn refdefs_collect_first_wins() {
    let lines = vec![
      "[a]: /one".to_string(),
      "   [A]: /two".to_string(),
      "[^fn]: /note".to_string(),
      "    [code]: /indented".to_string(),
      "[empty]:".to_string(),
    ];
    let refs = collect_refdefs(&lines, &|i| i == 3);
    assert_eq!(refs.get("a").map(String::as_str), Some("/one"));
    assert!(refs.get("code").is_none());
    assert!(refs.get("empty").is_none());
    assert!(refs.get("fn").is_none());
  }

  #[test]
  fn reference_links_resolve() {
    let inline = parse_refs("see [docs] and [Docs][] and [d][docs] end");
    assert_eq!(
      inline.links,
      vec![
        "https://x.dev".to_string(),
        "https://x.dev".to_string(),
        "https://x.dev".to_string()
      ]
    );
    // Without the table the same text stays literal.
    assert!(parse("see [docs] end").links.is_empty());
    // Unresolved labels stay literal even with a table present.
    assert!(parse_refs("see [nope] end").links.is_empty());
  }

  #[test]
  fn autolinks_and_bare_urls() {
    let text = "go <https://x.dev/a> or https://y.dev/b! or www.z.dev/c.";
    let inline = parse(text);
    assert_eq!(
      inline.links,
      vec![
        "https://x.dev/a".to_string(),
        "https://y.dev/b".to_string(),
        "https://www.z.dev/c".to_string()
      ]
    );
    assert_eq!(inline.links.len(), 3);
    // Trailing punctuation is trimmed; dest of www. gets a scheme.
    assert_eq!(find_bare_url("see https://y.dev/b! ok", 4).unwrap().1, "https://y.dev/b");
    assert!(find_bare_url("xhttp://y.dev", 1).is_none());
    // Email autolinks gain mailto:.
    let (range, url) = angle_autolink("<a@b.cc>", 0).unwrap();
    assert_eq!(url, "mailto:a@b.cc");
    assert_eq!(range, 0..8);
    assert!(angle_autolink("<not a link>", 0).is_none());
  }

  #[test]
  fn sup_sub_markers_hide_content_flagged() {
    assert!(style_of("E=mc^2^ and H~2~O", "2^").sup || style_of("E=mc^2^", "2").sup);
    assert!(style_of("H~2~O", "2").sub);
    // Delimiters are markers (hidden in preview).
    let frags_mc = frags("E=mc^2^");
    assert!(frags_mc.iter().any(|(t, m)| t == "^" && *m));
    // Doubles belong to other constructs: strike still wins.
    let strike = parse("~~gone~~");
    assert!(style_of("~~gone~~", "gone").strike);
    assert!(!strike.spans.iter().any(|sp| sp.style.sub));
    // Spaced or empty content stays literal.
    assert!(!parse("a ^ b ^ c").spans.iter().any(|sp| sp.style.sup));
    assert!(!parse("x^^y").spans.iter().any(|sp| sp.style.sup));
  }

  #[test]
  fn emoji_tokens_flagged_not_replaced() {
    let text = "party :tada: now";
    let inline = parse(text);
    assert!(style_of(text, ":tada:").emoji);
    // Byte-exact: visible keeps the shortcode (mapping invariant).
    let total: usize = inline.spans.iter().map(|sp| sp.range.len()).sum();
    assert_eq!(total, text.len());
    assert!(parse("a :nope: b").spans.iter().all(|sp| !sp.style.emoji));
    assert!(parse("` :tada: `").spans.iter().all(|sp| !sp.style.emoji));
  }

  #[test]
  fn footnote_refs_flagged_defs_untouched() {
    let inline = parse("see [^note] here");
    assert!(style_of("see [^note] here", "note").footnote);
    assert!(inline.links.is_empty());
    // Definitions and empty labels stay literal.
    assert!(parse("[^note]: text").spans.iter().all(|sp| !sp.style.footnote));
    assert!(parse("[^]: x").spans.iter().all(|sp| !sp.style.footnote));
  }

  #[test]
  fn wikilinks() {
    let text = "go [[Home]] or [[Page|the page]]";
    let inline = parse(text);
    assert_eq!(inline.links, vec!["Home".to_string(), "Page".to_string()]);
    assert!(style_of(text, "Home").link);
    assert!(style_of(text, "the page").link);
    // The target of an aliased wikilink is hidden marker text.
    let at = text.find("Page|").unwrap();
    let span = inline.spans.iter().find(|s| s.range.contains(&at)).unwrap();
    assert!(span.marker);
  }

  #[test]
  fn bare_brackets_stay_literal() {
    assert_eq!(frags("a [b] c"), vec![("a [b] c".into(), false)]);
    assert_eq!(frags("f(x)[0]"), vec![("f(x)[0]".into(), false)]);
  }

  #[test]
  fn emphasis_inside_link_text() {
    let text = "[**bold** link](u)";
    let s = style_of(text, "bold");
    assert!(s.bold && s.link);
  }

  #[test]
  fn utf8_content_is_safe() {
    let text = "**héllo — 日本語** *ok*";
    assert!(style_of(text, "héllo").bold);
    assert!(style_of(text, "ok").italic);
    let inline = parse(text);
    let mut at = 0;
    for s in &inline.spans {
      assert!(text.is_char_boundary(s.range.start));
      assert_eq!(s.range.start, at);
      at = s.range.end;
    }
  }

  #[test]
  fn escapes_shield_markup() {
    // `\*` stays literal: no italic, backslash is a marker.
    assert_eq!(
      frags("\\*not em\\*"),
      vec![("\\".into(), true), ("*not em".into(), false), ("\\".into(), true), ("*".into(), false)]
    );
    assert!(!style_of("\\*x\\*", "*x").italic);
    // Escaped brackets never open links.
    assert_eq!(frags("a \\[b\\] c"), vec![("a ".into(), false), ("\\".into(), true), ("[b".into(), false), ("\\".into(), true), ("] c".into(), false)]);
    assert!(parse("a \\[b\\](u) c").links.is_empty());
    // Escaped backtick never opens a code span.
    assert!(!style_of("a \\`b\\` c", "b").code);
    // Double backslash: first marks, second is literal text.
    assert_eq!(frags("a \\\\ b"), vec![("a ".into(), false), ("\\".into(), true), ("\\ b".into(), false)]);
  }
}

#[cfg(test)]
mod element_at_tests {
  use super::*;

  fn el(text: &str, at: usize) -> String {
    let inline = parse(text);
    let r = inline.element_at(at).expect("有元素");
    text[r].to_string()
  }

  #[test]
  fn element_at_recovers_emphasis_constructs() {
    let t = "a **bold** b";
    // `**bold**` 在字节 2..10；光标在内容里 → 整个构造。
    assert_eq!(el(t, 5), "**bold**");
    // 光标在开标记上 → 同一个构造。
    assert_eq!(el(t, 2), "**bold**");
    // 光标在闭标记上 → 同一个构造。
    assert_eq!(el(t, 9), "**bold**");
  }

  #[test]
  fn element_at_keeps_plain_text_separate() {
    // 光标在纯文本里 → 只有纯文本 span，不该吞掉后面的粗体。
    let t = "plain **bold**";
    let inline = parse(t);
    let r = inline.element_at(2).expect("有元素");
    assert_eq!(&t[r], "plain ");
  }

  #[test]
  fn element_at_handles_code_spans() {
    let t = "x `code` y";
    assert_eq!(el(t, 5), "`code`");
  }

  #[test]
  fn element_at_handles_links() {
    let t = "see [docs](https://x.dev) now";
    let inline = parse(t);
    let r = inline.element_at(6).expect("有元素");
    assert_eq!(&t[r], "[docs](https://x.dev)");
  }

  #[test]
  fn element_at_handles_two_adjacent_bolds() {
    // 两个相邻粗体是**两个**元素（Typora 也是分开展开）。
    let t = "**a** **b**";
    let inline = parse(t);
    let first = inline.element_at(3).expect("有元素");
    assert_eq!(&t[first], "**a**");
    let second = inline.element_at(7).expect("有元素");
    assert_eq!(&t[second], "**b**");
  }

  #[test]
  fn expand_at_only_for_elements_with_markers() {
    // Typora `.md-expand`：只有「有标记可展开」的元素才展开。
    // 纯文本没有标记 → 不展开（整行保持渲染）。
    let t = "plain **bold** tail";
    let inline = parse(t);
    assert_eq!(inline.expand_at(2), None, "纯文本不该展开");
    assert_eq!(
      inline.expand_at(9).map(|r| &t[r]),
      Some("**bold**"),
      "粗体内部应展开"
    );
    // 标记上也算「在里面」（用户正在编辑那些字符）。
    assert_eq!(
      inline.expand_at(6).map(|r| &t[r]),
      Some("**bold**"),
      "开标记上应展开"
    );
  }

  #[test]
  fn expand_at_is_none_for_plain_lines() {
    let t = "just words here";
    assert!(parse(t).expand_at(4).is_none());
  }
}
