//! Row planning: turn one classified source line into exactly what gets
//! drawn — the visible string, its style runs, and a source↔visible byte
//! mapping — for both preview (markers hidden) and revealed (markers dimmed)
//! modes.
//!
//! The visible string is always a concatenation of verbatim source slices
//! (never transformed text), so a [`Seg`] maps byte offsets between the two
//! by plain arithmetic. List markers are hidden in *both* modes: the editor
//! draws bullets, numbers, and checkboxes as decorations in the hanging
//! indent, which is what keeps wrapped items aligned.

use std::collections::HashMap;
use std::ops::Range;

use super::block::{indent_cols, Block};
use super::inline::{self, InlineStyle};

/// Block visuals the editor derives sizes and decorations from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
  Heading(u8),
  Paragraph,
  /// `cols` is the indent width in columns (tab = 4).
  Bullet {
    cols: usize,
  },
  Ordered {
    cols: usize,
    number: u64,
  },
  Task {
    cols: usize,
    checked: bool,
  },
  Quote {
    depth: u8,
  },
  Fence {
    open: bool,
  },
  Code {
    lang: Option<String>,
  },
  Rule,
  Table,
  /// Display-math formula line (`$$...$$` or inside a `$$` block).
  Math,
  /// A `$$` delimiter line: hidden in preview, dimmed marker when revealed.
  MathDelim { open: bool },
  /// A setext heading underline (`===`/`---`): hidden in preview (consumed
  /// into the heading), dimmed marker text when revealed.
  Setext,
  FrontMatter,
  Blank,
}

/// One visible run: `len` bytes of the visible string sharing a style.
/// `marker` runs are markdown syntax (drawn dimmed); `dim` runs are
/// de-emphasized content (a checked task's text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunAttrs {
  pub len: usize,
  pub marker: bool,
  pub dim: bool,
  pub style: InlineStyle,
}

/// A source-range → visible-range correspondence. Hidden source bytes map
/// to an empty visible range at the point they collapsed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
  pub src: Range<usize>,
  pub vis: Range<usize>,
}

/// Everything needed to draw one source line.
#[derive(Debug, Clone, PartialEq)]
pub struct RowPlan {
  pub kind: RowKind,
  pub visible: String,
  /// Covers `visible` exactly, in order.
  pub runs: Vec<RunAttrs>,
  /// Covers the source line exactly, in order.
  pub segs: Vec<Seg>,
  /// Link targets by **source** byte range (text and url bytes alike).
  pub links: Vec<(Range<usize>, String)>,
  pub revealed: bool,
}

impl RowPlan {
  /// The link target covering source byte `src`, if any.
  pub fn link_at(&self, src: usize) -> Option<&str> {
    self
      .links
      .iter()
      .find(|(r, _)| r.contains(&src))
      .map(|(_, t)| t.as_str())
  }
}

/// Plan one table cell's inline content (a sub-range of a source line).
///
/// This is [`Builder::inline`] driven over a cell instead of a whole line,
/// so a cell gets exactly the same markup treatment as a paragraph — and,
/// crucially, the same **`segs` mapping**. `segs[].src` stays absolute in
/// the line (the cell's own byte offsets), so the existing
/// [`vis_for_src`] / [`src_for_vis`] pair maps the caret and clicks
/// between the rendered cell and the source with no new machinery.
///
/// `content` is the cell's trimmed content byte range in `line`; parsing
/// stops at `content.end`, so neighbouring cells never leak in.
///
/// `expand` is Typora's `.md-expand` rule: markers stay hidden **except**
/// inside the one inline element the caret sits in (pass its byte range;
/// `None` = nothing expanded, i.e. the fully rendered look). This is what
/// makes the active cell look rendered while still letting the user edit
/// the markdown it contains — the whole cell never flips to source.
pub fn plan_cell(
  line: &str,
  content: Range<usize>,
  expand: Option<Range<usize>>,
  refs: &HashMap<String, String>,
) -> RowPlan {
  let mut b = Builder::new(line, false, refs);
  b.expand = expand;
  b.inline_range(content, false);
  b.finish(RowKind::Table)
}

/// How a row is sized. Both markdown renderers — the editor and the read-only
/// [`Markdown`](super::Markdown) view — read this, because a document that
/// changes size when you start editing it is a bug. They had two copies of
/// these numbers and had already drifted apart on three of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowMetrics {
  /// Font size, as a multiple of the document's base size.
  pub scale: f32,
  /// Line height, as a multiple of this row's own font size.
  pub line_height: f32,
  /// Space above and below the row, as multiples of the base size.
  pub pad_top: f32,
  pub pad_bottom: f32,
}

/// The metrics for a row kind.
pub fn metrics(kind: &RowKind) -> RowMetrics {
  let m = |scale, line_height, pad_top, pad_bottom| RowMetrics {
    scale,
    line_height,
    pad_top,
    pad_bottom,
  };
  match kind {
    RowKind::Heading(1) => m(1.6, 1.3, 0.7, 0.3),
    RowKind::Heading(2) => m(1.45, 1.3, 0.6, 0.25),
    RowKind::Heading(3) => m(1.28, 1.3, 0.5, 0.2),
    RowKind::Heading(4) => m(1.15, 1.3, 0.4, 0.15),
    RowKind::Heading(5) => m(1.05, 1.3, 0.35, 0.1),
    RowKind::Heading(_) => m(0.95, 1.3, 0.35, 0.1),
    RowKind::Code { .. } | RowKind::Fence { .. } | RowKind::FrontMatter | RowKind::Table => {
      m(0.88, 1.55, 0.0, 0.0)
    }
    // Display math reads slightly larger, with breathing room (Typora).
    RowKind::Math => m(1.12, 1.6, 0.25, 0.25),
    _ => m(1.0, 1.6, 0.0, 0.0),
  }
}

/// Plan one line. `code_lang` is the enclosing fence's info string when
/// `block` is [`Block::CodeLine`]; `reveal` shows syntax instead of hiding it
/// (the cursor/selection lines of a focused editor).
pub fn plan(line: &str, block: &Block, code_lang: Option<&str>, reveal: bool) -> RowPlan {
  plan_with_refs(line, block, code_lang, reveal, &HashMap::new())
}

/// [`plan`] with a document-wide reference table ([`inline::collect_refdefs`]).
pub fn plan_with_refs(
  line: &str,
  block: &Block,
  code_lang: Option<&str>,
  reveal: bool,
  refs: &HashMap<String, String>,
) -> RowPlan {
  let mut b = Builder::new(line, reveal, refs);
  let kind = match *block {
    Block::Blank => {
      b.hide(0..line.len());
      RowKind::Blank
    }
    Block::Rule => {
      b.syntax(0..line.len());
      RowKind::Rule
    }
    Block::FrontMatter => {
      b.mono_dim(0..line.len());
      RowKind::FrontMatter
    }
    Block::Table => {
      b.mono(0..line.len());
      RowKind::Table
    }
    Block::SetextUnderline { .. } => {
      b.syntax(0..line.len());
      RowKind::Setext
    }
    Block::MathDelim { open } => {
      b.syntax(0..line.len());
      RowKind::MathDelim { open }
    }
    Block::Math { start, end } => {
      // `$$` markers hide in preview (dim when revealed); the formula
      // itself stays visible in both modes.
      b.syntax(0..start);
      b.math(start..end);
      b.syntax(end..line.len());
      RowKind::Math
    }
    Block::Fence { open, .. } => {
      b.mono_dim(0..line.len());
      RowKind::Fence { open }
    }
    Block::CodeLine => {
      b.mono(0..line.len());
      RowKind::Code {
        lang: code_lang.map(str::to_string),
      }
    }
    Block::Heading { level, content } => {
      b.syntax(0..content);
      b.inline(content, false);
      RowKind::Heading(level)
    }
    Block::Quote { depth, content } => {
      b.syntax(0..content);
      b.inline(content, false);
      RowKind::Quote { depth }
    }
    Block::Bullet { content, .. } => {
      b.hide(0..content);
      b.inline(content, false);
      RowKind::Bullet {
        cols: indent_cols(line),
      }
    }
    Block::Ordered {
      number, content, ..
    } => {
      b.hide(0..content);
      b.inline(content, false);
      RowKind::Ordered {
        cols: indent_cols(line),
        number,
      }
    }
    Block::Task {
      checked, content, ..
    } => {
      b.hide(0..content);
      b.inline(content, checked);
      RowKind::Task {
        cols: indent_cols(line),
        checked,
      }
    }
    Block::Paragraph => {
      b.inline(0, false);
      RowKind::Paragraph
    }
    // Definition-list definition: the `: ` marker hides like a list
    // bullet; the body plans as an ordinary paragraph (preview parity:
    // pulldown strips the marker too).
    Block::Definition { content } => {
      b.hide(0..content);
      b.inline(content, false);
      RowKind::Paragraph
    }
  };
  b.finish(kind)
}

/// Accumulates visible text, runs, and segments while walking a source line.
struct Builder<'a> {
  line: &'a str,
  reveal: bool,
  refs: &'a HashMap<String, String>,
  visible: String,
  runs: Vec<RunAttrs>,
  segs: Vec<Seg>,
  links: Vec<(Range<usize>, String)>,
  /// Typora `.md-expand`: reveal markers **only** inside this byte range
  /// (the inline element under the caret). `None` = hide every marker.
  expand: Option<Range<usize>>,
}

impl<'a> Builder<'a> {
  fn new(line: &'a str, reveal: bool, refs: &'a HashMap<String, String>) -> Self {
    Builder {
      line,
      reveal,
      refs,
      visible: String::new(),
      runs: Vec::new(),
      segs: Vec::new(),
      links: Vec::new(),
      expand: None,
    }
  }

  /// Map `src` to nothing — hidden in both modes.
  fn hide(&mut self, src: Range<usize>) {
    if src.is_empty() {
      return;
    }
    let at = self.visible.len();
    self.segs.push(Seg { src, vis: at..at });
  }

  /// Emit `src` verbatim with the given attributes.
  fn emit(&mut self, src: Range<usize>, marker: bool, dim: bool, style: InlineStyle) {
    if src.is_empty() {
      return;
    }
    let at = self.visible.len();
    self.visible.push_str(&self.line[src.clone()]);
    let len = src.len();
    self.segs.push(Seg {
      src,
      vis: at..at + len,
    });
    // Coalesce with the previous run when nothing changed.
    if let Some(last) = self.runs.last_mut() {
      if last.marker == marker && last.dim == dim && last.style == style {
        last.len += len;
        return;
      }
    }
    self.runs.push(RunAttrs {
      len,
      marker,
      dim,
      style,
    });
  }

  /// Block syntax: hidden in preview, dimmed marker text when revealed.
  fn syntax(&mut self, src: Range<usize>) {
    if self.reveal {
      self.emit(src, true, false, InlineStyle::default());
    } else {
      self.hide(src);
    }
  }

  /// Verbatim monospace (code lines, tables).
  fn mono(&mut self, src: Range<usize>) {
    let style = InlineStyle {
      code: true,
      ..InlineStyle::default()
    };
    self.emit(src, false, false, style);
  }

  /// Display-math formula: verbatim content with the math style.
  fn math(&mut self, src: Range<usize>) {
    let style = InlineStyle {
      math: true,
      ..InlineStyle::default()
    };
    self.emit(src, false, false, style);
  }

  /// Verbatim monospace, dimmed (fence lines, frontmatter).
  fn mono_dim(&mut self, src: Range<usize>) {
    let style = InlineStyle {
      code: true,
      ..InlineStyle::default()
    };
    self.emit(src, true, false, style);
  }

  /// Parse and emit the line's content from `at`, with inline markup.
  fn inline(&mut self, at: usize, dim: bool) {
    self.inline_range(at..self.line.len(), dim);
  }

  /// [`Builder::inline`] confined to `range`: only bytes inside it are
  /// parsed and emitted. Used for table cells, whose content is a
  /// sub-range of the line — parsing to end-of-line would swallow the
  /// neighbouring cells (and their pipes) into this cell's plan.
  fn inline_range(&mut self, range: Range<usize>, dim: bool) {
    let at = range.start;
    let content = &self.line[range.clone()];
    let inline = inline::parse_with_refs(content, self.refs);
    for span in &inline.spans {
      let src = at + span.range.start..at + span.range.end;
      // Markers hide unless revealed, or unless they sit inside the
      // expanded element (Typora: only the caret's own inline element shows
      // its delimiters; the rest of the line stays rendered).
      let expanded = self
        .expand
        .as_ref()
        .is_some_and(|ex| ex.start <= src.start && src.end <= ex.end);
      if span.marker && !self.reveal && !expanded {
        self.hide(src);
      } else {
        self.emit(src, span.marker, dim, span.style);
      }
    }
    // Collect link targets by construct extent, in source offsets.
    for (idx, target) in inline.links.iter().enumerate() {
      let mut range: Option<Range<usize>> = None;
      for span in inline.spans.iter().filter(|s| s.link == Some(idx)) {
        let r = at + span.range.start..at + span.range.end;
        range = Some(match range {
          None => r,
          Some(acc) => acc.start.min(r.start)..acc.end.max(r.end),
        });
      }
      if let Some(range) = range {
        self.links.push((range, target.clone()));
      }
    }
  }

  fn finish(self, kind: RowKind) -> RowPlan {
    RowPlan {
      kind,
      visible: self.visible,
      runs: self.runs,
      segs: self.segs,
      links: self.links,
      revealed: self.reveal,
    }
  }
}

// ---- source ↔ visible mapping ----------------------------------------------

/// Visible byte offset for source byte `src`. Bytes inside hidden segments
/// collapse to the point they disappeared into; offsets past the last
/// segment map to the end of the visible string.
pub fn vis_for_src(segs: &[Seg], src: usize) -> usize {
  for seg in segs {
    if src < seg.src.end {
      let off = src.saturating_sub(seg.src.start);
      return (seg.vis.start + off).min(seg.vis.end);
    }
  }
  segs.last().map_or(0, |s| s.vis.end)
}

/// Source byte offset for visible byte `vis`. Offsets at or past the end of
/// the visible string map to the source end (so clicking after the rendered
/// text puts the cursor at the end of the line).
pub fn src_for_vis(segs: &[Seg], vis: usize) -> usize {
  let end = segs.last().map_or(0, |s| s.src.end);
  for seg in segs {
    if seg.vis.is_empty() {
      continue;
    }
    if vis < seg.vis.end {
      return seg.src.start + vis.saturating_sub(seg.vis.start);
    }
  }
  end
}

/// Byte offset of char column `col` in `s`, clamped to the end.
pub fn byte_for_col(s: &str, col: usize) -> usize {
  s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

/// Char column of byte offset `byte` in `s` (interior offsets round down).
pub fn col_for_byte(s: &str, byte: usize) -> usize {
  s.char_indices().take_while(|&(i, _)| i < byte).count()
}

#[cfg(test)]
mod tests {
  use super::super::block::{classify, DocState};
  use super::*;

  fn plan_line(line: &str, reveal: bool) -> RowPlan {
    let mut st = DocState::default();
    // pretend mid-document so `---` is a rule
    classify("x", &mut st);
    let block = classify(line, &mut st);
    plan(line, &block, None, reveal)
  }

  fn covers(plan: &RowPlan, line: &str) {
    let mut src_at = 0;
    let mut vis_at = 0;
    for seg in &plan.segs {
      assert_eq!(seg.src.start, src_at, "segs must cover source in order");
      assert_eq!(seg.vis.start, vis_at);
      src_at = seg.src.end;
      vis_at = seg.vis.end;
    }
    assert_eq!(src_at, line.len());
    assert_eq!(vis_at, plan.visible.len());
    let run_total: usize = plan.runs.iter().map(|r| r.len).sum();
    assert_eq!(run_total, plan.visible.len(), "runs must cover visible");
  }

  #[test]
  fn heading_hides_marks_in_preview() {
    let line = "## Section **two**";
    let p = plan_line(line, false);
    assert_eq!(p.kind, RowKind::Heading(2));
    assert_eq!(p.visible, "Section two");
    covers(&p, line);
  }

  #[test]
  fn heading_reveals_marks_dimmed() {
    let line = "## Section";
    let p = plan_line(line, true);
    assert_eq!(p.visible, "## Section");
    assert!(p.runs[0].marker);
    assert!(!p.runs[1].marker);
    covers(&p, line);
  }

  #[test]
  fn list_marker_stays_hidden_even_revealed() {
    let line = "  - item with *em*";
    for reveal in [false, true] {
      let p = plan_line(line, reveal);
      assert_eq!(p.kind, RowKind::Bullet { cols: 2 });
      if reveal {
        assert_eq!(p.visible, "item with *em*");
      } else {
        assert_eq!(p.visible, "item with em");
      }
      covers(&p, line);
    }
  }

  #[test]
  fn checked_task_content_is_dim() {
    let line = "- [x] done deal";
    let p = plan_line(line, false);
    assert_eq!(
      p.kind,
      RowKind::Task {
        cols: 0,
        checked: true
      }
    );
    assert_eq!(p.visible, "done deal");
    assert!(p.runs.iter().all(|r| r.dim));
    covers(&p, line);
  }

  #[test]
  fn display_math_hides_delims_keeps_formula() {
    let p = plan_line("$$E = mc^2$$", false);
    assert_eq!(p.kind, RowKind::Math);
    assert_eq!(p.visible, "E = mc^2");
    assert!(p.runs.iter().all(|r| r.style.math));
    covers(&p, "$$E = mc^2$$");
    // Revealed: delimiters come back dimmed as markers.
    let p = plan_line("$$E = mc^2$$", true);
    assert_eq!(p.visible, "$$E = mc^2$$");
    assert!(p.runs.iter().any(|r| r.marker && !r.style.math));
  }

  #[test]
  fn math_delim_vanishes_in_preview() {
    let mut st = DocState::default();
    let block = classify("$$", &mut st);
    let p = plan("$$", &block, None, false);
    assert_eq!(p.kind, RowKind::MathDelim { open: true });
    assert_eq!(p.visible, "");
    let p = plan("$$", &block, None, true);
    assert_eq!(p.visible, "$$");
  }

  #[test]
  fn rule_vanishes_in_preview() {
    let p = plan_line("---", false);
    assert_eq!(p.kind, RowKind::Rule);
    assert_eq!(p.visible, "");
    covers(&p, "---");
    let p = plan_line("---", true);
    assert_eq!(p.visible, "---");
  }

  #[test]
  fn code_lines_are_identity_mono() {
    let mut st = DocState::default();
    classify("```rust", &mut st);
    let lang = st.fence_lang().map(str::to_string);
    let block = classify("let x = 1;", &mut st);
    let p = plan("let x = 1;", &block, lang.as_deref(), false);
    assert_eq!(
      p.kind,
      RowKind::Code {
        lang: Some("rust".into())
      }
    );
    assert_eq!(p.visible, "let x = 1;");
    assert!(p.runs[0].style.code);
    covers(&p, "let x = 1;");
  }

  #[test]
  fn link_targets_use_source_ranges() {
    let line = "see [docs](https://x.dev)";
    let p = plan_line(line, false);
    assert_eq!(p.visible, "see docs");
    assert_eq!(p.link_at(line.find("docs").unwrap()), Some("https://x.dev"));
    assert_eq!(
      p.link_at(line.find("https").unwrap()),
      Some("https://x.dev")
    );
    assert_eq!(p.link_at(0), None);
    covers(&p, line);
  }

  #[test]
  fn mapping_roundtrips_through_hidden_prefix() {
    let line = "- [ ] task";
    let p = plan_line(line, false);
    // Source col inside the hidden marker collapses to visible start.
    assert_eq!(vis_for_src(&p.segs, 0), 0);
    assert_eq!(vis_for_src(&p.segs, 5), 0);
    // Content maps 1:1 after the marker.
    let t = line.find("task").unwrap();
    assert_eq!(vis_for_src(&p.segs, t), 0);
    assert_eq!(vis_for_src(&p.segs, t + 2), 2);
    assert_eq!(src_for_vis(&p.segs, 2), t + 2);
    // Past the end lands at the line end.
    assert_eq!(src_for_vis(&p.segs, 99), line.len());
  }

  #[test]
  fn mapping_collapses_hidden_url() {
    let line = "[a](url)";
    let p = plan_line(line, false);
    assert_eq!(p.visible, "a");
    // A cursor inside the hidden url sits at the end of the visible text.
    assert_eq!(vis_for_src(&p.segs, line.find("url").unwrap()), 1);
    // Clicking the single visible char maps back into the link text.
    assert_eq!(src_for_vis(&p.segs, 0), 1);
    covers(&p, line);
  }

  #[test]
  fn revealed_line_maps_identity() {
    let line = "a **b** c";
    let p = plan_line(line, true);
    assert_eq!(p.visible, line);
    for at in 0..=line.len() {
      assert_eq!(vis_for_src(&p.segs, at), at);
      assert_eq!(src_for_vis(&p.segs, at), at);
    }
  }

  #[test]
  fn blank_line_collapses() {
    let p = plan_line("   ", false);
    assert_eq!(p.kind, RowKind::Blank);
    assert_eq!(p.visible, "");
    assert_eq!(vis_for_src(&p.segs, 2), 0);
    assert_eq!(src_for_vis(&p.segs, 0), 3);
  }

  #[test]
  fn byte_col_helpers() {
    assert_eq!(byte_for_col("日本語", 1), 3);
    assert_eq!(byte_for_col("abc", 9), 3);
    assert_eq!(col_for_byte("日本語", 3), 1);
    assert_eq!(col_for_byte("abc", 9), 3);
  }

  /// Both renderers read one table, so a document can't change size when you
  /// start editing it.
  #[test]
  fn headings_scale_down_to_body_and_code() {
    let h1 = metrics(&RowKind::Heading(1));
    let h3 = metrics(&RowKind::Heading(3));
    let body = metrics(&RowKind::Paragraph);
    let code = metrics(&RowKind::Code { lang: None });
    assert!(h1.scale > h3.scale && h3.scale > body.scale);
    assert!(code.scale < body.scale);
    // Headings carry the space above them; body copy doesn't.
    assert!(h1.pad_top > body.pad_top);
    assert_eq!(body.pad_top, 0.0);
  }

  // ---- 表格单元格规划（格内直接编辑的映射基础） --------------------------

  #[test]
  fn plan_cell_maps_cell_bytes_and_stops_at_the_boundary() {
    // 真实一行的第一格：`| **粗** | b |` 的 `**粗**` 在字节 2..9。
    let line = "| **粗** | b |";
    let content = 2..9;
    let plan = plan_cell(line, content.clone(), Some(content.clone()), &HashMap::new());
    // 揭示态：标记可见，可见文本就是这一格的内容（不含相邻格/管道）。
    assert_eq!(plan.visible, "**粗**");
    assert_eq!(plan.kind, RowKind::Table);
    // segs 必须**行内绝对**且覆盖本格内容（不越界到邻格）。
    let first = plan.segs.first().expect("有 seg");
    let last = plan.segs.last().expect("有 seg");
    assert_eq!(first.src.start, content.start);
    assert_eq!(last.src.end, content.end);
    for seg in &plan.segs {
      assert!(
        seg.src.start >= content.start && seg.src.end <= content.end,
        "segs 不得越出本格：{seg:?} vs {content:?}"
      );
    }
    // 双向映射在格内往返自洽。
    for byte in content.clone() {
      let vis = vis_for_src(&plan.segs, byte);
      assert!(
        vis <= plan.visible.len(),
        "vis 越界：byte={byte} vis={vis} visible={:?}",
        plan.visible
      );
    }
    assert_eq!(src_for_vis(&plan.segs, 0), content.start);
    assert_eq!(src_for_vis(&plan.segs, plan.visible.len()), content.end);
  }

  #[test]
  fn plan_cell_hides_markers_when_not_revealed() {
    let line = "| **粗** | b |";
    let content = 2..9;
    let plan = plan_cell(line, content.clone(), None, &HashMap::new());
    // 非揭示态：标记隐藏，只剩内容（预览观感）。
    assert_eq!(plan.visible, "粗");
    // 隐藏的标记字节塌缩到内容起点，不会跑到邻格。
    assert_eq!(vis_for_src(&plan.segs, content.start), 0);
  }

  #[test]
  fn plan_cell_handles_links_inside_the_cell() {
    let line = "| [docs](https://x.dev) | b |";
    let content = 2..23;
    let plan = plan_cell(line, content.clone(), Some(content), &HashMap::new());
    // 链接目标必须记录在**行内**绝对范围里（点击激活用）。
    assert_eq!(plan.links.len(), 1);
    let (r, target) = &plan.links[0];
    assert_eq!(target, "https://x.dev");
    assert!(r.start >= 2 && r.end <= 23, "链接范围必须在格内：{r:?}");
    assert!(line[r.clone()].contains("docs"));
  }

  #[test]
  fn plan_cell_expands_only_the_caret_element() {
    // Typora `.md-expand`：整格保持渲染，只有光标所在的那个行内元素的
    // 标记显示。这是「1:1 复刻」的核心行为。
    let line = "| plain **bold** `code` |";
    // 内容 = `plain **bold** `code``（去掉两侧 `| ` 与 ` |`）。
    let content = 2..23;
    // 与生产代码同源：在**格内容切片**上解析，再把光标列换算成切片内偏移。
    let slice = &line[content.clone()];
    let inline = inline::parse_with_refs(slice, &HashMap::new());
    let expand_at = |cursor_in_slice: usize| {
      let expand = inline.expand_at(cursor_in_slice);
      plan_cell(line, content.clone(), expand.map(|r| content.start + r.start..content.start + r.end), &HashMap::new())
        .visible
    };
    let bt = '\u{60}';
    // 光标在纯文本里 → 整格渲染，**所有**标记都隐藏（含反引号）。
    assert_eq!(expand_at(1), "plain bold code");
    assert_eq!(expand_at(4), "plain bold code");
    // spec #2：光标在词**中间**（`**bold**` 内容 = 切片 8..12）→ 不展开，保持渲染。
    assert_eq!(expand_at(9), "plain bold code");
    assert_eq!(expand_at(11), "plain bold code");
    // 光标触到词首/词尾/标记（切片 6 = 开标记、8 = 内容首、12 = 内容尾/闭标记首）→
    // 只展开粗体的 `**`，代码段的反引号仍隐藏。
    assert_eq!(expand_at(6), "plain **bold** code");
    assert_eq!(expand_at(8), "plain **bold** code");
    assert_eq!(expand_at(12), "plain **bold** code");
    // 代码段（`code` 内容 = 16..20，反引号 15 / 20）：词中间不展开，词首/词尾展开。
    assert_eq!(expand_at(18), "plain bold code");
    assert_eq!(expand_at(16), format!("plain bold {bt}code{bt}"));
    assert_eq!(expand_at(20), format!("plain bold {bt}code{bt}"));
  }

  #[test]
  fn plan_cell_of_escaped_pipe_cell_keeps_the_escape() {
    // 转义管道格：`a\|b` 是一个格，反斜杠是内容的一部分。
    let line = "| a\\|b | c |";
    let plan = plan_cell(line, 2..6, Some(2..6), &HashMap::new());
    assert_eq!(plan.visible, "a\\|b");
    assert_eq!(plan.segs.last().unwrap().src.end, 6);
  }
}
