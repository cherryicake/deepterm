//! Diff data model for the git diff view.
//!
//! Line-level and word-level diff computation is ported from zed's
//! `crates/language/src/text_diff.rs` (imara-diff, Histogram algorithm).
//! The word tokenizer is simplified: word characters are alphanumeric + `_`,
//! language-scope-aware classification is not needed here.

use imara_diff::{
    Algorithm, diff,
    intern::{InternedInput, Token},
    sources::lines_with_terminator,
};
use std::iter;
use std::ops::Range;

/// Same limits as zed: only perform word-level diff inside small hunks.
const MAX_WORD_DIFF_LEN: usize = 512;
const MAX_WORD_DIFF_LINE_COUNT: usize = 8;

/// 为单个 hunk 生成可被 `git apply --cached` 消费的最小 unified 补丁
///（`--unidiff-zero`，零上下文行）。
///
/// 行号为 1-based；空侧按 git 惯例取插入点行号（文件头为 0）。
/// 缺尾换行的末行追加 `\ No newline at end of file` 标记，
/// 否则 apply 会补换行导致暂存内容与工作区不一致。
///
/// `is_new`：路径尚不在 index 中（未跟踪文件）。此时 preimage 必须是
/// `/dev/null` 并声明 `new file mode`，否则 `git apply --cached` 会因
/// 「a/{path} 不在 index 中」拒绝应用——未跟踪文件的 hunk 暂存就走不通。
pub fn build_hunk_patch(
    path: &str,
    old_text: &str,
    new_text: &str,
    hunk: &DiffHunk,
    is_new: bool,
) -> String {
    let old_starts = line_starts(old_text);
    let new_starts = line_starts(new_text);
    let old_count = hunk.old_rows.end.saturating_sub(hunk.old_rows.start);
    let new_count = hunk.new_rows.end.saturating_sub(hunk.new_rows.start);
    // 空侧取 0-based 插入点（文件头为 0），非空侧转 1-based。
    let old_start = if old_count == 0 {
        hunk.old_rows.start
    } else {
        hunk.old_rows.start + 1
    };
    let new_start = if new_count == 0 {
        hunk.new_rows.start
    } else {
        hunk.new_rows.start + 1
    };

    let mut patch = String::new();
    patch.push_str(&format!("diff --git a/{path} b/{path}\n"));
    if is_new {
        patch.push_str("new file mode 100644\n");
        patch.push_str("--- /dev/null\n");
    } else {
        patch.push_str(&format!("--- a/{path}\n"));
    }
    patch.push_str(&format!("+++ b/{path}\n"));
    patch.push_str(&format!(
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
    ));

    let old_no_nl = !old_text.is_empty() && !old_text.ends_with('\n');
    let new_no_nl = !new_text.is_empty() && !new_text.ends_with('\n');
    let old_last = line_count(old_text, &old_starts).saturating_sub(1);
    let new_last = line_count(new_text, &new_starts).saturating_sub(1);

    for row in hunk.old_rows.start..hunk.old_rows.end {
        let range = line_range(old_text, &old_starts, row);
        patch.push('-');
        patch.push_str(&old_text[range]);
        patch.push('\n');
        if old_no_nl && row == old_last {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
    for row in hunk.new_rows.start..hunk.new_rows.end {
        let range = line_range(new_text, &new_starts, row);
        patch.push('+');
        patch.push_str(&new_text[range]);
        patch.push('\n');
        if new_no_nl && row == new_last {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
    patch
}

/// One changed hunk between old and new text, with word-level detail.
#[derive(Clone, Debug)]
pub struct DiffHunk {
    pub old_rows: Range<u32>,
    pub new_rows: Range<u32>,
    /// Word-level changed byte ranges, absolute in the old text.
    pub old_word_ranges: Vec<Range<usize>>,
    /// Word-level changed byte ranges, absolute in the new text.
    pub new_word_ranges: Vec<Range<usize>>,
}

impl DiffHunk {
    pub fn deletions(&self) -> usize {
        (self.old_rows.end - self.old_rows.start) as usize
    }

    pub fn additions(&self) -> usize {
        (self.new_rows.end - self.new_rows.start) as usize
    }
}

/// Compute line-level hunks plus word-level ranges (for small hunks).
pub fn compute_hunks(old_text: &str, new_text: &str) -> Vec<DiffHunk> {
    let old_starts = line_starts(old_text);
    let new_starts = line_starts(new_text);

    line_diff(old_text, new_text)
        .into_iter()
        .map(|(old_rows, new_rows)| {
            let old_byte_range = if old_rows.start < old_rows.end {
                let s = old_starts[old_rows.start as usize];
                let e = line_range(old_text, &old_starts, old_rows.end - 1).end;
                s..e
            } else {
                0..0
            };
            let new_byte_range = if new_rows.start < new_rows.end {
                let s = new_starts[new_rows.start as usize];
                let e = line_range(new_text, &new_starts, new_rows.end - 1).end;
                s..e
            } else {
                0..0
            };

            let (old_word_ranges, new_word_ranges) =
                if should_word_diff(&old_rows, &old_byte_range, &new_rows, &new_byte_range) {
                    let (mut o, mut n) = word_diff_ranges(
                        &old_text[old_byte_range.clone()],
                        &new_text[new_byte_range.clone()],
                    );
                    for r in &mut o {
                        r.start += old_byte_range.start;
                        r.end += old_byte_range.start;
                    }
                    for r in &mut n {
                        r.start += new_byte_range.start;
                        r.end += new_byte_range.start;
                    }
                    (o, n)
                } else {
                    (Vec::new(), Vec::new())
                };

            DiffHunk {
                old_rows,
                new_rows,
                old_word_ranges,
                new_word_ranges,
            }
        })
        .collect()
}

/// Line kind in the merged unified display text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Context,
    Add,
    Del,
    /// 折叠占位行：未更改区被收起后留下的一行空行（可点击展开）。
    /// 不是文件内容，写回工作区时必须丢弃（与 Del 同等对待）。
    Separator,
}

impl LineKind {
    /// 该行是否对应工作区的真实行（写回时保留）；Del/Separator 丢弃。
    pub(crate) fn is_worktree_line(&self) -> bool {
        matches!(self, LineKind::Context | LineKind::Add)
    }
}

/// One line of the merged unified display text.
#[derive(Clone, Debug)]
pub struct UnifiedLine {
    pub kind: LineKind,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    /// Byte range of this line in the merged text (no trailing newline).
    pub merged_range: Range<usize>,
    /// Word-level changed byte ranges in the merged text.
    pub word_ranges: Vec<Range<usize>>,
    /// Separator 行隐藏的 HEAD 侧行区间（unchanged gap，与工作区内容一致）。
    /// 写回时用它回填被折叠的行；非 Separator 行为 None。
    pub hidden_old_rows: Option<Range<u32>>,
}

/// Merged old+new text for the unified diff editor, with per-line metadata.
#[derive(Clone, Debug)]
pub struct UnifiedSource {
    pub text: String,
    pub lines: Vec<UnifiedLine>,
    /// Merged row of each hunk's first changed line (hunk 导航与初始定位用，
    /// 与 hunks 入参一一对应）。
    pub hunk_rows: Vec<u32>,
}

/// Build the merged unified display: context lines interleaved with deleted
/// (old) and added (new) lines, hunks in file order, nothing collapsed.
///
/// unified/inline 视图直接编辑这份合并文本（VSCode 行为）；[`worktree_from_merged`]
/// 负责把编辑写回工作区。合并文本的末尾换行与工作区一致（无则不补），
/// 否则“删除文末换行”这类编辑无法往返（写回会凭空多出 `\n`）。
pub fn build_unified(old_text: &str, new_text: &str) -> UnifiedSource {
    let hunks = compute_hunks(old_text, new_text);
    let old_starts = line_starts(old_text);
    let new_starts = line_starts(new_text);
    let new_line_count = line_count(new_text, &new_starts);

    let mut text = String::new();
    let mut lines = Vec::new();
    let mut hunk_rows = Vec::with_capacity(hunks.len());

    let push_line = |kind: LineKind,
                     old_lineno: Option<u32>,
                     new_lineno: Option<u32>,
                     source: &str,
                     source_range: Range<usize>,
                     word_ranges: &[Range<usize>],
                     text: &mut String,
                     lines: &mut Vec<UnifiedLine>| {
        let merged_start = text.len();
        text.push_str(&source[source_range.clone()]);
        let merged_end = text.len();
        text.push('\n');
        let word_ranges = word_ranges
            .iter()
            .filter(|r| r.start < source_range.end && r.end > source_range.start)
            .map(|r| {
                let s = r.start.max(source_range.start) - source_range.start + merged_start;
                let e = r.end.min(source_range.end) - source_range.start + merged_start;
                s..e
            })
            .collect();
        lines.push(UnifiedLine {
            kind,
            old_lineno,
            new_lineno,
            merged_range: merged_start..merged_end,
            word_ranges,
            hidden_old_rows: None,
        });
    };

    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    for hunk in &hunks {
        // Context before this hunk (identical in both texts, use new).
        for row in new_cursor..hunk.new_rows.start {
            let range = line_range(new_text, &new_starts, row);
            push_line(
                LineKind::Context,
                Some(old_cursor + (row - new_cursor)),
                Some(row),
                new_text,
                range,
                &[],
                &mut text,
                &mut lines,
            );
        }
        // hunk 首个变更行的合并行号（Del 块在前，首行即 Del 首行或 Add 首行）。
        hunk_rows.push(lines.len() as u32);
        for row in hunk.old_rows.start..hunk.old_rows.end {
            let range = line_range(old_text, &old_starts, row);
            push_line(
                LineKind::Del,
                Some(row),
                None,
                old_text,
                range,
                &hunk.old_word_ranges,
                &mut text,
                &mut lines,
            );
        }
        for row in hunk.new_rows.start..hunk.new_rows.end {
            let range = line_range(new_text, &new_starts, row);
            push_line(
                LineKind::Add,
                None,
                Some(row),
                new_text,
                range,
                &hunk.new_word_ranges,
                &mut text,
                &mut lines,
            );
        }
        old_cursor = hunk.old_rows.end;
        new_cursor = hunk.new_rows.end;
    }
    // Trailing context.
    for row in new_cursor..new_line_count {
        let range = line_range(new_text, &new_starts, row);
        push_line(
            LineKind::Context,
            Some(old_cursor + (row - new_cursor)),
            Some(row),
            new_text,
            range,
            &[],
            &mut text,
            &mut lines,
        );
    }

    // 末尾换行与工作区保持一致：工作区没有尾换行时不补，否则“删尾换行”
    // 的编辑写回时会凭空多出 `\n`，往返不一致（set_text 跳过逻辑依赖此性质）。
    if !new_text.ends_with('\n') && text.ends_with('\n') {
        text.pop();
    }

    UnifiedSource {
        text,
        lines,
        hunk_rows,
    }
}

/// 把合并文本按行切分（与 [`build_unified`] 的行划分一致）：
/// 空文本 → 0 行；尾换行不产生额外空行；`\r` 保留在行内（CRLF 安全）。
pub(crate) fn split_merged_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        parts.pop();
    }
    parts
}

/// 把用户编辑后的合并文本写回工作区文本（unified/inline 可编辑的核心）。
///
/// 对齐基准是旧合并文本的快照行（`old_lines` 与 `old_merged` 逐行对应，
/// 不得用 HEAD 文本行代替——Del/Add 错位处行号完全对不上）。
///
/// 规则（VSCode 语义）：
/// - 用 [`line_diff`] 对齐旧合并文本与新合并文本。Equal 间隙按旧元数据归属：
///   Context/Add 行的内容（编辑后的）进入工作区；Del 行丢弃（HEAD 内容，
///   从不是文件的一部分）；Separator 行回填其隐藏的 HEAD 行
///   （unchanged gap 新旧一致，见 `hidden_old_rows`）。
/// - 变更区旧侧全是 Context/Add：新区所有行都是工作区内容（打字/换行拆分/
///   整行删除都精确成立）。
/// - 变更区旧侧含 Del/Separator（用户碰了删除行或横幅行）：对区内再做一次
///   行 diff，只保留与旧 Context/Add 行对齐的新行；Separator 配对处回填
///   隐藏行，其余丢弃。这等价于 VSCode“删除行不可选中”——Del 配对的行自然
///   脱落，与 VSCode 的结果一致；差异只在于编辑瞬间能看见、200ms 后对账消失。
/// - 新文本全空（含 banner 行被全选删掉的手势）→ 空工作区（清空文件，
///   与 VSCode 全选删除含折叠区一致）。无 Separator 时的纯空白文件走
///   常规路径（删剩一个空行仍保留一个 `\n`）。
///
/// 纯函数，可单元测试。
pub fn worktree_from_merged<'a>(
    old_lines: &[UnifiedLine],
    old_merged: &'a str,
    new_merged: &'a str,
    head_text: &'a str,
) -> String {
    if new_merged.trim().is_empty() && old_lines.iter().any(|l| l.kind == LineKind::Separator) {
        return String::new();
    }
    if new_merged.is_empty() {
        return String::new();
    }

    let new_lines = split_merged_lines(new_merged);
    // 快照行内容（与 old_lines 一一对应）：区内二次对齐的基准。
    let snap_rows = split_merged_lines(old_merged);
    // 隐藏区回填源：unchanged gap 新旧内容一致，从 HEAD 切片。
    let head_rows = split_merged_lines(head_text);

    let mut out: Vec<&str> = Vec::new();

    // line_diff 只给出变更块；块之间的 Equal 间隙按行号一一对应。
    // 行号口径：imara 行切分与 split_merged_lines/构造行数一致
    //（`\n` 终止段 + 末尾片段；空文本 0 行），索引用 get 防御。
    let mut old_idx = 0usize;
    let mut new_idx = 0usize;
    for (old_rows, new_rows) in line_diff(old_merged, new_merged) {
        let (os, oe) = (old_rows.start as usize, old_rows.end as usize);
        let (ns, ne) = (new_rows.start as usize, new_rows.end as usize);
        emit_aligned(
            &mut out,
            old_lines.get(old_idx..os).unwrap_or(&[]),
            new_lines.get(new_idx..ns).unwrap_or(&[]),
            &head_rows,
            true,
        );
        // 变更块：旧侧干净（全是 Context/Add）→ 新区全收；否则区内二次对齐。
        let touched_hidden =
            (os..oe).any(|i| old_lines.get(i).is_some_and(|l| !l.kind.is_worktree_line()));
        if !touched_hidden {
            for line in new_lines.get(ns..ne).unwrap_or(&[]) {
                out.push(line);
            }
        } else {
            // 先回填本区内所有 Separator 的隐藏行：空 banner 行在子 diff
            // 里是零 token（纯插入/删除），子对齐永远碰不到它们，不先回填
            // 就会丢代码。隐藏行与新区内容无关，可安全预置。
            for i in os..oe {
                if let Some(m) = old_lines.get(i)
                    && m.kind == LineKind::Separator
                {
                    push_hidden(&mut out, m, &head_rows);
                }
            }
            // 再对齐新区：只保留与旧 Context/Add 行对齐的新行
            //（Separator 已预回填，此处跳过，避免重复）。
            let sub_old: Vec<&str> = snap_rows
                .get(os..oe.min(snap_rows.len()))
                .map_or_else(Vec::new, |s| s.to_vec());
            let sub_old_text = sub_old.join("\n");
            let sub_new: Vec<&str> = new_lines.get(ns..ne).unwrap_or(&[]).to_vec();
            let sub_new_text = sub_new.join("\n");
            let mut s_old = 0usize;
            let mut s_new = 0usize;
            for (s_os, s_ns) in line_diff(&sub_old_text, &sub_new_text) {
                let (ss, se) = (s_os.start as usize, s_os.end as usize);
                let sn_e = s_ns.end as usize;
                emit_aligned(
                    &mut out,
                    old_lines.get(os + s_old..os + ss).unwrap_or(&[]),
                    sub_new.get(s_new..s_new + (ss - s_old)).unwrap_or(&[]),
                    &head_rows,
                    false,
                );
                // 子变更块的新行无法判定归属，一律丢弃。
                s_old = se;
                s_new = sn_e;
            }
            // 子尾部 Equal 间隙（新旧行数相等）。
            let sub_old_len = split_merged_lines(&sub_old_text).len();
            emit_aligned(
                &mut out,
                old_lines.get(os + s_old..os + sub_old_len).unwrap_or(&[]),
                sub_new.get(s_new..).unwrap_or(&[]),
                &head_rows,
                false,
            );
        }
        old_idx = oe;
        new_idx = ne;
    }
    // 尾部 Equal 间隙。
    emit_aligned(
        &mut out,
        old_lines.get(old_idx..).unwrap_or(&[]),
        new_lines.get(new_idx..).unwrap_or(&[]),
        &head_rows,
        true,
    );

    // 尾换行规则（精确性依赖构建器的 tail 预留不变式）：
    // out 以 buffer 行结尾时，看该行在 buffer 中是否有终止符；
    // out 以 hidden 回填结尾时，buffer 必以横幅结尾——而构建器保证
    // “无尾换行文件永不以横幅结尾”，故此时文件必有尾换行。
    // 两种情况统一为：buffer 文本以换行结尾 ⟺ 追加换行。
    let mut worktree = out.join("\n");
    if !out.is_empty() && new_merged.ends_with('\n') {
        worktree.push('\n');
    }
    worktree
}

/// Separator 元数据 → 回填隐藏的 HEAD 行。
fn push_hidden<'a>(out: &mut Vec<&'a str>, meta: &UnifiedLine, head_rows: &[&'a str]) {
    if let Some(range) = meta.hidden_old_rows.clone() {
        for r in range.start as usize..range.end as usize {
            if let Some(line) = head_rows.get(r) {
                out.push(line);
            }
        }
    }
}

/// 一段 Equal 对齐（meta 与 new_seg 等长）：按元数据归属输出。
/// `refill_sep` 为 true 时 Separator 回填隐藏行；为 false 时跳过
/// （调用方已统一预回填，见变更块处理）。
/// meta 长于 new（截断边缘防御）：多余 Separator 按同一规则处理，Del 丢弃。
fn emit_aligned<'a>(
    out: &mut Vec<&'a str>,
    meta: &[UnifiedLine],
    new_seg: &[&'a str],
    head_rows: &[&'a str],
    refill_sep: bool,
) {
    for (m, l) in meta.iter().zip(new_seg.iter()) {
        match m.kind {
            LineKind::Separator if refill_sep => push_hidden(out, m, head_rows),
            _ if m.kind.is_worktree_line() => out.push(l),
            _ => {}
        }
    }
    if refill_sep && meta.len() > new_seg.len() {
        for m in &meta[new_seg.len()..] {
            if m.kind == LineKind::Separator {
                push_hidden(out, m, head_rows);
            }
        }
    }
}
/// Number of context lines kept around each change when collapsing
/// unchanged regions (reference UI convention).
pub const CONTEXT_LINES: u32 = 4;

/// One collapsed (hidden) region of unchanged lines in the unified view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeparatorRow {
    /// Merged row index of the placeholder line in the collapsed text.
    pub merged_row: u32,
    /// Expansion key: new-side start line of the hidden region
    /// (deterministic across recomputes).
    pub id: u32,
    /// Number of unchanged rows hidden between the two context strips.
    pub hidden_lines: u32,
}

/// Collapsed unified view: merged text with unchanged gaps reduced to
/// `CONTEXT_LINES` context strips around each hunk plus one [`LineKind::Separator`]
/// placeholder row per collapsed region (reference-UI style expandable bands).
///
/// `expanded` 中的 id 对应区域完整渲染；其余大于 `2 * CONTEXT_LINES` 的
/// 间隙被折叠。返回的 `UnifiedSource.hunk_rows` 与 hunks 一一对应
/// （hunk 导航与初始定位用）。
/// Row-pushing backend for [`build_unified_collapsed`].
///
/// 结构体内聚输出缓冲，一次 `&mut` 借用解决多个闭包互斥借用问题。
struct CollapsedBuilder<'a> {
    text: String,
    lines: Vec<UnifiedLine>,
    hunk_rows: Vec<u32>,
    separators: Vec<SeparatorRow>,
    old_text: &'a str,
    old_starts: Vec<usize>,
    new_text: &'a str,
    new_starts: Vec<usize>,
}

impl<'a> CollapsedBuilder<'a> {
    fn new(old_text: &'a str, new_text: &'a str) -> Self {
        Self {
            text: String::new(),
            lines: Vec::new(),
            hunk_rows: Vec::new(),
            separators: Vec::new(),
            old_text,
            old_starts: line_starts(old_text),
            new_text,
            new_starts: line_starts(new_text),
        }
    }

    /// Push context rows [new_from..new_to) from the new side. Unchanged
    /// regions have equal line counts on both sides; `old_from` is the
    /// corresponding old row.
    fn context(&mut self, new_from: u32, new_to: u32, old_from: u32) {
        for row in new_from..new_to {
            let range = line_range(self.new_text, &self.new_starts, row);
            let merged_start = self.text.len();
            self.text.push_str(&self.new_text[range.clone()]);
            self.text.push('\n');
            self.lines.push(UnifiedLine {
                kind: LineKind::Context,
                old_lineno: Some(old_from + (row - new_from)),
                new_lineno: Some(row),
                merged_range: merged_start..self.text.len() - 1,
                word_ranges: Vec::new(),
                hidden_old_rows: None,
            });
        }
    }

    /// Push a single empty separator row representing a collapsed region.
    /// `hidden_old` is the HEAD-side row range with identical content
    /// (unchanged gap ⇒ old/new counts equal); write-back uses it to
    /// refill the hidden worktree lines.
    fn separator(&mut self, id: u32, hidden_lines: u32, hidden_old: Range<u32>) {
        let merged_row = self.lines.len() as u32;
        let start = self.text.len();
        self.text.push('\n');
        self.lines.push(UnifiedLine {
            kind: LineKind::Separator,
            old_lineno: None,
            new_lineno: None,
            merged_range: start..start,
            word_ranges: Vec::new(),
            hidden_old_rows: Some(hidden_old),
        });
        self.separators.push(SeparatorRow {
            merged_row,
            id,
            hidden_lines,
        });
    }

    /// Push one hunk's Del block then Add block, recording its merged row.
    fn hunk(&mut self, hunk: &DiffHunk) {
        self.hunk_rows.push(self.lines.len() as u32);
        for row in hunk.old_rows.start..hunk.old_rows.end {
            let range = line_range(self.old_text, &self.old_starts, row);
            let merged_start = self.text.len();
            self.text.push_str(&self.old_text[range.clone()]);
            self.text.push('\n');
            self.lines.push(UnifiedLine {
                kind: LineKind::Del,
                old_lineno: Some(row),
                new_lineno: None,
                merged_range: merged_start..self.text.len() - 1,
                word_ranges: hunk
                    .old_word_ranges
                    .iter()
                    .filter(|r| r.start < range.end && r.end > range.start)
                    .map(|r| {
                        let s = r.start.max(range.start) - range.start + merged_start;
                        let e = r.end.min(range.end) - range.start + merged_start;
                        s..e
                    })
                    .collect(),
                hidden_old_rows: None,
            });
        }
        for row in hunk.new_rows.start..hunk.new_rows.end {
            let range = line_range(self.new_text, &self.new_starts, row);
            let merged_start = self.text.len();
            self.text.push_str(&self.new_text[range.clone()]);
            self.text.push('\n');
            self.lines.push(UnifiedLine {
                kind: LineKind::Add,
                old_lineno: None,
                new_lineno: Some(row),
                merged_range: merged_start..self.text.len() - 1,
                word_ranges: hunk
                    .new_word_ranges
                    .iter()
                    .filter(|r| r.start < range.end && r.end > range.start)
                    .map(|r| {
                        let s = r.start.max(range.start) - range.start + merged_start;
                        let e = r.end.min(range.end) - range.start + merged_start;
                        s..e
                    })
                    .collect(),
                hidden_old_rows: None,
            });
        }
    }
}

pub(crate) fn build_unified_collapsed(
    old_text: &str,
    new_text: &str,
    hunks: &[DiffHunk],
    expanded: &std::collections::HashSet<u32>,
) -> (UnifiedSource, Vec<SeparatorRow>) {
    let new_line_count = line_count(new_text, &line_starts(new_text));
    let mut b = CollapsedBuilder::new(old_text, new_text);

    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    // 主循环。unchanged gap 的 old/new 行数相等，光标成对推进。
    for (hi, hunk) in hunks.iter().enumerate() {
        // 通向本 hunk 的 gap：head 留尾部 C 行，middle 留前导 C 行
        //（middle 的前 C 行已在上次迭代作为 trailing 输出）。
        let gap_new = hunk.new_rows.start.saturating_sub(new_cursor);
        debug_assert_eq!(gap_new, hunk.old_rows.start.saturating_sub(old_cursor));
        if gap_new > 0 {
            let visible = CONTEXT_LINES.min(gap_new);
            let hidden = gap_new - visible;
            let hidden_id = new_cursor;
            if hidden == 0 || expanded.contains(&hidden_id) {
                b.context(new_cursor, hunk.new_rows.start, old_cursor);
            } else {
                b.separator(hidden_id, hidden, old_cursor..old_cursor + hidden);
                b.context(
                    hunk.new_rows.start - visible,
                    hunk.new_rows.start,
                    hunk.old_rows.start - visible,
                );
            }
        }
        b.hunk(hunk);
        old_cursor = hunk.old_rows.end;
        new_cursor = hunk.new_rows.end;
        // hunk 后的 trailing 上下文：取前 min(C, 剩余) 行；剩余部分留给
        // 下一次迭代的 leading gap（middle）或文末 tail 处理。
        let limit_new = if hi + 1 < hunks.len() {
            hunks[hi + 1].new_rows.start
        } else {
            new_line_count
        };
        let tail_len = limit_new.saturating_sub(new_cursor);
        if tail_len > 0 {
            let take = CONTEXT_LINES.min(tail_len);
            b.context(new_cursor, new_cursor + take, old_cursor);
            new_cursor += take;
            old_cursor += take;
        }
    }

    // 文末 tail 剩余：短则全出（与 middle ≤C 规则一致），否则 separator 收尾。
    // 例外：工作区没有尾换行时，末行永远可见——否则 buffer 以横幅结尾，
    // 写回的尾换行规则（见 worktree_from_merged）无法判断文件真实结尾，
    // 会凭空补一个 `\n`（git 称为 "\ No newline"  churn）。
    if new_cursor < new_line_count {
        let remaining = new_line_count - new_cursor;
        let reserve_last = !new_text.ends_with('\n') as u32;
        let collapsible = remaining.saturating_sub(reserve_last);
        if collapsible <= CONTEXT_LINES || expanded.contains(&new_cursor) {
            b.context(new_cursor, new_line_count, old_cursor);
        } else {
            // 先折叠 collapsible 行，保留的末行随后输出。
            b.separator(
                new_cursor,
                collapsible,
                old_cursor..old_cursor + collapsible,
            );
            let last_start = new_line_count - reserve_last;
            b.context(last_start, new_line_count, old_cursor + collapsible);
        }
    }

    // 空文件 diff（无 hunk）：与全量构建一致，不折叠。
    if hunks.is_empty() && b.lines.is_empty() {
        b.context(0, new_line_count, 0);
    }

    // 末尾换行与工作区保持一致（与 build_unified 同规则，写回往返依赖）。
    // 注意 separator 空行贡献的末尾 `\n` 同样处理：split 口径下它只是
    // 一个空行成分，不影响行对齐（见 split_merged_lines）。
    if !new_text.ends_with('\n') && b.text.ends_with('\n') {
        b.text.pop();
    }

    (
        UnifiedSource {
            text: b.text,
            lines: b.lines,
            hunk_rows: b.hunk_rows,
        },
        b.separators,
    )
}

/// Byte offsets of every line start in a text, plus a sentinel past the end
/// so `line_range` works for the last line.
pub(crate) fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (ix, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(ix + 1);
        }
    }
    starts.push(text.len() + 1); // sentinel
    starts
}

/// Number of content lines. A trailing newline does not start a new line.
pub(crate) fn line_count(text: &str, starts: &[usize]) -> u32 {
    let n = starts.len() - 1; // exclude sentinel
    if text.is_empty() || text.ends_with('\n') {
        n.saturating_sub(1) as u32
    } else {
        n as u32
    }
}

/// Byte range of line `row` (0-based), excluding the trailing newline.
pub(crate) fn line_range(text: &str, starts: &[usize], row: u32) -> Range<usize> {
    let start = starts[row as usize];
    let mut end = starts[row as usize + 1].min(text.len());
    if end > start && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    start..end
}

/// Computes a line-based diff, returning `(old_row_range, new_row_range)`
/// for each changed hunk. Ported from zed `text_diff.rs::line_diff`.
pub fn line_diff(old_text: &str, new_text: &str) -> Vec<(Range<u32>, Range<u32>)> {
    let mut edits = Vec::new();
    let input = InternedInput::new(
        lines_with_terminator(old_text),
        lines_with_terminator(new_text),
    );
    diff_internal(&input, &mut |_, _, old_rows, new_rows| {
        edits.push((old_rows, new_rows));
    });
    edits
}

/// Computes word-level diff ranges between two strings.
/// Returns `(old_ranges, new_ranges)` as byte ranges of changed words.
/// Ported from zed `text_diff.rs::word_diff_ranges` with a simplified
/// tokenizer (word chars = alphanumeric + `_`).
pub fn word_diff_ranges(old_text: &str, new_text: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(tokenize(old_text));
    input.update_after(tokenize(new_text));

    let mut old_ranges: Vec<Range<usize>> = Vec::new();
    let mut new_ranges: Vec<Range<usize>> = Vec::new();

    diff_internal(&input, &mut |old_byte_range, new_byte_range, _, _| {
        if !old_byte_range.is_empty() {
            if let Some(last) = old_ranges.last_mut()
                && last.end >= old_byte_range.start
            {
                last.end = old_byte_range.end;
            } else {
                old_ranges.push(old_byte_range);
            }
        }
        if !new_byte_range.is_empty() {
            if let Some(last) = new_ranges.last_mut()
                && last.end >= new_byte_range.start
            {
                last.end = new_byte_range.end;
            } else {
                new_ranges.push(new_byte_range);
            }
        }
    });

    (old_ranges, new_ranges)
}

fn should_word_diff(
    old_rows: &Range<u32>,
    old_byte_range: &Range<usize>,
    new_rows: &Range<u32>,
    new_byte_range: &Range<usize>,
) -> bool {
    !old_byte_range.is_empty()
        && !new_byte_range.is_empty()
        && old_byte_range.len() <= MAX_WORD_DIFF_LEN
        && new_byte_range.len() <= MAX_WORD_DIFF_LEN
        && old_rows.len() <= MAX_WORD_DIFF_LINE_COUNT
        && new_rows.len() <= MAX_WORD_DIFF_LINE_COUNT
}

fn diff_internal(
    input: &InternedInput<&str>,
    on_change: &mut dyn FnMut(Range<usize>, Range<usize>, Range<u32>, Range<u32>),
) {
    let mut old_offset = 0;
    let mut new_offset = 0;
    let mut old_token_ix = 0;
    let mut new_token_ix = 0;
    diff(
        Algorithm::Histogram,
        input,
        |old_tokens: Range<u32>, new_tokens: Range<u32>| {
            old_offset += token_len(
                input,
                &input.before[old_token_ix as usize..old_tokens.start as usize],
            );
            new_offset += token_len(
                input,
                &input.after[new_token_ix as usize..new_tokens.start as usize],
            );
            let old_len = token_len(
                input,
                &input.before[old_tokens.start as usize..old_tokens.end as usize],
            );
            let new_len = token_len(
                input,
                &input.after[new_tokens.start as usize..new_tokens.end as usize],
            );
            let old_byte_range = old_offset..old_offset + old_len;
            let new_byte_range = new_offset..new_offset + new_len;
            old_token_ix = old_tokens.end;
            new_token_ix = new_tokens.end;
            old_offset = old_byte_range.end;
            new_offset = new_byte_range.end;
            on_change(old_byte_range, new_byte_range, old_tokens, new_tokens);
        },
    );
}

/// Simplified word tokenizer: word chars are alphanumeric + `_`;
/// runs of whitespace are single tokens; each punctuation char is its own
/// token (same splitting policy as zed's CharClassifier-based tokenizer).
fn tokenize(text: &str) -> impl Iterator<Item = &str> {
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Word,
        Whitespace,
        Punctuation,
    }

    fn kind(c: char) -> Kind {
        if c.is_alphanumeric() || c == '_' {
            Kind::Word
        } else if c.is_whitespace() {
            Kind::Whitespace
        } else {
            Kind::Punctuation
        }
    }

    let mut chars = text.char_indices();
    let mut prev: Option<(char, Kind)> = None;
    let mut start_ix = 0;
    iter::from_fn(move || {
        for (ix, c) in chars.by_ref() {
            let mut token = None;
            let k = kind(c);
            if let Some((prev_char, prev_kind)) = prev
                && (k != prev_kind || (k == Kind::Punctuation && c != prev_char))
            {
                token = Some(&text[start_ix..ix]);
                start_ix = ix;
            }
            prev = Some((c, k));
            if token.is_some() {
                return token;
            }
        }
        if start_ix < text.len() {
            let token = &text[start_ix..];
            start_ix = text.len();
            return Some(token);
        }
        None
    })
}

fn token_len(input: &InternedInput<&str>, tokens: &[Token]) -> usize {
    tokens
        .iter()
        .map(|token| input.interner[*token].len())
        .sum()
}

// Re-export nothing; keep the module self-contained.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_diff_basic() {
        let old = "one\ntwo\nthree\n";
        let new = "one\nTWO\nthree\n";
        let hunks = line_diff(old, new);
        assert_eq!(hunks, vec![(1..2, 1..2)]);
    }

    #[test]
    fn test_line_diff_insert() {
        let old = "one\nthree\n";
        let new = "one\ntwo\nthree\n";
        let hunks = line_diff(old, new);
        assert_eq!(hunks, vec![(1..1, 1..2)]);
    }

    #[test]
    fn test_word_diff_ranges() {
        let old = "let x = 1;";
        let new = "let x = 42;";
        let (old_ranges, new_ranges) = word_diff_ranges(old, new);
        assert_eq!(old_ranges, vec![8..9]);
        assert_eq!(new_ranges, vec![8..10]);
    }

    #[test]
    fn test_tokenize() {
        let text = "one.two(three)";
        assert_eq!(
            tokenize(text).collect::<Vec<_>>(),
            vec!["one", ".", "two", "(", "three", ")"]
        );
    }

    #[test]
    fn test_build_unified() {
        let old = "fn main() {\n    let x = 1;\n}\n".to_string();
        let new = "fn main() {\n    let x = 2;\n    dbg!(x);\n}\n".to_string();
        let unified = build_unified(&old, &new);
        // context: "fn main() {" ; del x=1 ; add x=2 ; add dbg! ; context "}"
        assert_eq!(unified.lines.len(), 5);
        assert_eq!(unified.lines[0].kind, LineKind::Context);
        assert_eq!(unified.lines[1].kind, LineKind::Del);
        assert_eq!(unified.lines[2].kind, LineKind::Add);
        assert_eq!(unified.lines[3].kind, LineKind::Add);
        assert_eq!(unified.lines[4].kind, LineKind::Context);

        let del = &unified.lines[1];
        assert_eq!(&unified.text[del.merged_range.clone()], "    let x = 1;");
        // word range should point at `1` in the merged text
        assert_eq!(del.word_ranges.len(), 1);
        assert_eq!(&unified.text[del.word_ranges[0].clone()], "1");

        let add = &unified.lines[2];
        assert_eq!(add.word_ranges.len(), 1);
        // Histogram alignment may include the trailing `;` in the changed run
        // (same merging behavior as zed); the range must start at `2`.
        assert!(unified.text[add.word_ranges[0].clone()].starts_with('2'));

        // Merged text reconstructs all lines
        assert_eq!(unified.text.lines().count(), 5);
    }

    #[test]
    fn test_compute_hunks_pure_addition() {
        let old = "a\nb\n".to_string();
        let new = "a\nx\nb\n".to_string();
        let hunks = compute_hunks(&old, &new);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_rows, 1..1);
        assert_eq!(hunks[0].new_rows, 1..2);
        assert!(hunks[0].old_word_ranges.is_empty());
    }

    /// 往返恒等：未编辑的合并文本写回 == 工作区原文。
    #[test]
    fn test_writeback_identity() {
        for (old, new) in [
            ("a\nb\nc\n", "a\nX\nc\n"),
            (
                "fn main() {\n    let x = 1;\n}\n",
                "fn main() {\n    let x = 2;\n    dbg!(x);\n}\n",
            ),
            ("", "a\nb\n"),
            ("a\nb\n", ""),
            ("a", "a\nb"),
            ("a\nb", "a"),
            ("same\n", "same\n"),
        ] {
            let unified = build_unified(old, new);
            assert_eq!(
                worktree_from_merged(&unified.lines, &unified.text, &unified.text, old),
                new,
                "identity round-trip failed for {old:?} -> {new:?}"
            );
        }
    }

    /// 在 Context/Add 行打字：精确进入工作区。
    #[test]
    fn test_writeback_edit_context_line() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let unified = build_unified(old, new);
        // 合并文本含 Del 行："a / b(Del) / B(Add) / c"
        assert_eq!(unified.text, "a\nb\nB\nc\n");
        // 用户把 Add 行 B 改成 BB（Del 行不动）
        let edited = "a\nb\nBB\nc\n";
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, edited, old),
            "a\nBB\nc\n"
        );
    }

    /// 换行拆分 Add 行：两行都进入工作区。
    #[test]
    fn test_writeback_split_line() {
        let old = "a\nb\n";
        let new = "a\nx\nb\n";
        let unified = build_unified(old, new);
        let edited = "a\nx\ny\nb\n";
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, edited, old),
            "a\nx\ny\nb\n"
        );
    }

    /// 删除整行 Add 行：工作区同步删除。
    #[test]
    fn test_writeback_delete_added_line() {
        let old = "a\nb\n";
        let new = "a\nx\nb\n";
        let unified = build_unified(old, new);
        let edited = "a\nb\n";
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, edited, old),
            "a\nb\n"
        );
    }

    /// 在 Del 行里打字：丢弃（VSCode 里根本选不中删除行），其余保留。
    #[test]
    fn test_writeback_typing_in_deleted_line_dropped() {
        let old = "a\nb\nc\n";
        let new = "a\nc\n";
        let unified = build_unified(old, new);
        assert_eq!(unified.text, "a\nb\nc\n");
        // 用户在 Del 行 "b" 后面加了 XYZ（Add 行没有，这是纯删除）
        let edited = "a\nbXYZ\nc\n";
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, edited, old),
            "a\nc\n"
        );
    }

    /// 删除 Del 行本身：工作区不变。
    #[test]
    fn test_writeback_delete_deleted_line_noop() {
        let old = "a\nb\nc\n";
        let new = "a\nc\n";
        let unified = build_unified(old, new);
        assert_eq!(unified.text, "a\nb\nc\n");
        // 用户把 Del 行 "b" 删掉（以为在删文件内容）
        let edited = "a\nc\n";
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, edited, old),
            "a\nc\n"
        );
    }

    /// 跨 Del+Add 选区删除：工作区对应行被删，Del 行本来就不在工作区。
    #[test]
    fn test_writeback_delete_across_del_and_add() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\n";
        let unified = build_unified(old, new);
        assert_eq!(unified.text, "a\nb\nB\nc\nd\n");
        // 用户把 "b\nB" 两行整个删掉（想删掉这一块）
        let edited = "a\nc\nd\n";
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, edited, old),
            "a\nc\nd\n"
        );
    }

    /// 清空文件：buffer 全空 → 工作区清空。
    #[test]
    fn test_writeback_clear_file() {
        let old = "a\nb\n";
        let new = "a\nB\n";
        let unified = build_unified(old, new);
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, "", old),
            ""
        );
    }

    /// 尾换行往返：删尾换行不再凭空补回；加尾换行正常落盘。
    #[test]
    fn test_writeback_trailing_newline_roundtrip() {
        // 工作区无尾换行：合并文本 = Del "a" + Add "a"（换行符差异本身即变更）。
        let old = "a\n";
        let new = "a";
        let unified = build_unified(old, new);
        assert_eq!(unified.text, "a\na");
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, &unified.text, old),
            "a"
        );
        // 用户补上尾换行 → 工作区补上。
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, "a\na\n", old),
            "a\n"
        );
        // 用户删掉尾换行 → 工作区去掉。
        let old2 = "a\nb\n";
        let new2 = "a\nb\n";
        let unified2 = build_unified(old2, new2);
        assert_eq!(unified2.text, "a\nb\n");
        assert_eq!(
            worktree_from_merged(&unified2.lines, &unified2.text, "a\nb", old2),
            "a\nb"
        );
    }

    /// CRLF：`\r` 保留在行内，写回不破坏。
    #[test]
    fn test_writeback_crlf() {
        let old = "a\r\nb\r\n";
        let new = "a\r\nB\r\n";
        let unified = build_unified(old, new);
        assert_eq!(unified.text, "a\r\nb\r\nB\r\n");
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, &unified.text, old),
            new
        );
        let edited = "a\r\nb\r\nBB\r\n";
        assert_eq!(
            worktree_from_merged(&unified.lines, &unified.text, edited, old),
            "a\r\nBB\r\n"
        );
    }

    /// split 口径：空文本 0 行，尾换行不产生空行。
    #[test]
    fn test_split_merged_lines() {
        assert!(split_merged_lines("").is_empty());
        assert_eq!(split_merged_lines("a"), vec!["a"]);
        assert_eq!(split_merged_lines("a\n"), vec!["a"]);
        assert_eq!(split_merged_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_merged_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_merged_lines("\n"), vec![""]);
    }

    /// 不动点：写回→重建→再写回收敛（编辑循环终止的模型层保证，
    /// 含重复行歧义与 Del 触碰场景）。
    #[test]
    fn test_writeback_rebuild_fixpoint() {
        let cases = [
            ("a\nb\nc\n", "a\nB\nc\n", "a\nb\nBB\nc\n"),
            ("f\nf\n", "f\n", "f\n"),
            ("a\nb\nc\n", "a\nc\n", "a\nc\n"),
            ("a\nb\nc\n", "a\nc\n", "a\nbXYZ\nc\n"),
            ("a\n", "a", "a\na\n"),
            ("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nc\nd\n"),
        ];
        for (old, new, edited) in cases {
            let u1 = build_unified(old, new);
            let w1 = worktree_from_merged(&u1.lines, &u1.text, edited, old);
            let u2 = build_unified(old, &w1);
            let w2 = worktree_from_merged(&u2.lines, &u2.text, &u2.text, old);
            assert_eq!(
                w1, w2,
                "fixpoint failed for {old:?}->{new:?} edit {edited:?}"
            );
            let u3 = build_unified(old, &w2);
            assert_eq!(u2.text, u3.text, "merged not stable for {old:?}->{new:?}");
        }
    }
}

#[cfg(test)]
mod collapsed_ui_tests {
    use super::*;
    use std::collections::HashSet;

    /// old: 20 行相同 + 改动 + 20 行相同；new 同结构（+1 行）。
    fn sample_texts() -> (String, String) {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..20 {
            old.push_str(&format!("ctx{i}\n"));
            new.push_str(&format!("ctx{i}\n"));
        }
        old.push_str("old line\n");
        new.push_str("new line A\n");
        new.push_str("new line B\n");
        for i in 20..40 {
            old.push_str(&format!("tail{i}\n"));
            new.push_str(&format!("tail{i}\n"));
        }
        (old, new)
    }

    fn collapsed(old: &str, new: &str) -> (UnifiedSource, Vec<SeparatorRow>) {
        let hunks = compute_hunks(old, new);
        build_unified_collapsed(old, new, &hunks, &HashSet::new())
    }

    #[test]
    fn collapsed_structure_matches_reference_ui() {
        let (old, new) = sample_texts();
        let hunks = compute_hunks(&old, &new);
        assert_eq!(hunks.len(), 1);
        let (out, seps) = build_unified_collapsed(&old, &new, &hunks, &HashSet::new());
        // 头部横幅 + 4 行上下文 + Del + 2×Add + 4 行上下文 + 尾部横幅
        assert_eq!(seps.len(), 2);
        assert_eq!(seps[0].id, 0);
        assert_eq!(seps[0].hidden_lines, 20 - CONTEXT_LINES);
        assert_eq!(seps[1].id, 22 + CONTEXT_LINES);
        assert_eq!(seps[1].hidden_lines, 20 - CONTEXT_LINES);
        assert_eq!(out.lines.len(), 1 + 4 + 1 + 2 + 4 + 1);
        // 横幅行是空 Separator 行，无行号。
        for sep in &seps {
            let line = &out.lines[sep.merged_row as usize];
            assert_eq!(line.kind, LineKind::Separator);
            assert_eq!(line.merged_range.start, line.merged_range.end);
            assert!(line.old_lineno.is_none() && line.new_lineno.is_none());
            assert!(line.hidden_old_rows.is_some());
        }
        // hunk 首行指向 Del 行。
        assert_eq!(out.hunk_rows.len(), 1);
        assert!(matches!(
            out.lines[out.hunk_rows[0] as usize].kind,
            LineKind::Del | LineKind::Add
        ));
    }

    #[test]
    fn small_gap_never_collapses() {
        let (out, seps) = collapsed("a\nb\nc\n", "a\nX\nc\n");
        assert!(seps.is_empty());
        // 合并文本含 Del + Add 两行：a / b(Del) / X(Add) / c
        assert_eq!(out.lines.len(), 4);
    }

    #[test]
    fn expanded_region_renders_in_full() {
        let (old, new) = sample_texts();
        let hunks = compute_hunks(&old, &new);
        let mut expanded = HashSet::new();
        expanded.insert(0u32);
        let (out, seps) = build_unified_collapsed(&old, &new, &hunks, &expanded);
        assert_eq!(seps.len(), 1, "头部展开后只剩尾部横幅");
        // 头部 20 行全出 + Del + 2×Add + 尾部 4 行 + 横幅
        assert_eq!(out.lines.len(), 20 + 1 + 2 + 4 + 1);
    }

    /// 折叠写回恒等：隐藏行必须从 HEAD 回填，一行不少。
    #[test]
    fn collapsed_writeback_identity_refills_hidden() {
        let (old, new) = sample_texts();
        let (out, _) = collapsed(&old, &new);
        assert_eq!(
            worktree_from_merged(&out.lines, &out.text, &out.text, &old),
            new
        );
    }

    /// 可见区打字：隐藏行保留，编辑精确落盘。
    #[test]
    fn collapsed_writeback_edit_visible_keeps_hidden() {
        let (old, new) = sample_texts();
        let (out, _) = collapsed(&old, &new);
        // 把 Add 行 "new line A" 改成 "new line A!"（其它不动）
        let edited = out.text.replacen("new line A\n", "new line A!\n", 1);
        let got = worktree_from_merged(&out.lines, &out.text, &edited, &old);
        let expect = new.replacen("new line A\n", "new line A!\n", 1);
        assert_eq!(got, expect);
    }

    /// 在横幅行上打字：丢弃输入但保留隐藏行（VSCode 删除行不可选中语义）。
    #[test]
    fn collapsed_writeback_typing_on_banner_dropped_keeps_hidden() {
        let (old, new) = sample_texts();
        let (out, seps) = collapsed(&old, &new);
        assert!(!seps.is_empty());
        // 第一个横幅行（空行）被填上内容
        let mut edited_lines: Vec<&str> = split_merged_lines(&out.text);
        let banner_row = seps[0].merged_row as usize;
        edited_lines[banner_row] = "oops";
        let edited = edited_lines.join("\n") + "\n";
        assert_eq!(
            worktree_from_merged(&out.lines, &out.text, &edited, &old),
            new
        );
    }

    /// 删除横幅行：隐藏行保留（删横幅删不掉代码）。
    #[test]
    fn collapsed_writeback_banner_deleted_keeps_hidden() {
        let (old, new) = sample_texts();
        let (out, seps) = collapsed(&old, &new);
        let banner_row = seps[0].merged_row as usize;
        let mut edited_lines: Vec<&str> = split_merged_lines(&out.text);
        edited_lines.remove(banner_row);
        let edited = edited_lines.join("\n") + "\n";
        assert_eq!(
            worktree_from_merged(&out.lines, &out.text, &edited, &old),
            new
        );
    }

    /// 无尾换行文件的尾部折叠：末行永远可见（tail 预留），写回不补换行。
    #[test]
    fn tail_reservation_for_unterminated_file() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..30 {
            old.push_str(&format!("l{i}\n"));
            new.push_str(&format!("l{i}\n"));
        }
        // 首行改动 + 尾部无换行
        let new = new.replacen("l0\n", "L0\n", 1);
        let new = new.trim_end_matches('\n').to_string();
        let hunks = compute_hunks(&old, &new);
        let (out, seps) = build_unified_collapsed(&old, &new, &hunks, &HashSet::new());
        assert!(!seps.is_empty());
        // 最后一行必须是真实行（末行 l29），不能是横幅
        let last = out.lines.last().unwrap();
        assert_ne!(last.kind, LineKind::Separator);
        assert_eq!(split_merged_lines(&out.text).last().copied(), Some("l29"));
        // 恒等写回：无幻影尾换行
        assert_eq!(
            worktree_from_merged(&out.lines, &out.text, &out.text, &old),
            new
        );
    }

    /// 折叠写回不动点：写回→按新工作区重建→再写回收敛。
    #[test]
    fn collapsed_writeback_fixpoint() {
        let (old, new) = sample_texts();
        let (out, _) = collapsed(&old, &new);
        let edited = out
            .text
            .replacen("new line B\n", "new line B2\nnew line B3\n", 1);
        let w1 = worktree_from_merged(&out.lines, &out.text, &edited, &old);
        assert!(w1.contains("new line B2\nnew line B3\n"));
        assert!(w1.contains("ctx0\n"), "隐藏头部必须保留");
        assert!(w1.contains("tail39\n"), "隐藏尾部必须保留");
        // 用新工作区重建（折叠位置重算）后再写回恒等
        let hunks2 = compute_hunks(&old, &w1);
        let (out2, _) = build_unified_collapsed(&old, &w1, &hunks2, &HashSet::new());
        assert_eq!(
            worktree_from_merged(&out2.lines, &out2.text, &out2.text, &old),
            w1
        );
    }
}
