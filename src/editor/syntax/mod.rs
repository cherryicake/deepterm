mod simple;
pub use simple::*;

mod registry;
pub use registry::*;

// 主题类型已下沉到 theme::syntax_theme（修复 theme→editor 层次倒置），
// 此处 re-export 保持 `editor::syntax::*` 旧路径兼容。
pub use crate::theme::syntax_theme::{
    FontStyle, FontWeightContent, HIGHLIGHT_NAMES, HighlightTheme, HighlightThemeStyle,
    StatusColors, SyntaxColors, ThemeStyle,
};

pub mod languages;

use std::ops::Range;

use gpui::{HighlightStyle, Hsla, UnderlineStyle, px};
use ropey::Rope;

use crate::theme::ActiveTheme as _;

/// 诊断严重级别。
///
/// 排序即优先级：`Error > Warning > Info > Hint`。gutter 标记取一行内
/// 最高级别，因此 derive 的 `Ord` 顺序必须与此一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticSeverity {
    Hint,
    Info,
    Warning,
    Error,
}

impl DiagnosticSeverity {
    /// 语义色。下划线与 gutter 标记共用，保证两处颜色一致。
    pub fn color(self, cx: &gpui::App) -> Hsla {
        let ui = cx.theme().ui;
        match self {
            Self::Error => ui.error,
            Self::Warning => ui.warning,
            Self::Info => ui.accent,
            Self::Hint => ui.text_subtle,
        }
    }

    /// 是否用波浪线。错误/警告用波浪线，信息/提示用直线，避免满屏噪点。
    fn is_wavy(self) -> bool {
        matches!(self, Self::Error | Self::Warning)
    }
}

/// 一条诊断。
///
/// `range` 是 UTF-8 字节区间，与编辑器其余部分的偏移口径一致。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range<usize>,
    pub severity: DiagnosticSeverity,
    /// 面向用户的说明（hover/状态栏用）。语法诊断场景下是节点名。
    pub message: String,
    /// 产出方标识，如 `"tree-sitter"`、`"lsp:rust-analyzer"`。
    pub source: Option<String>,
}

impl Diagnostic {
    pub fn new(
        range: Range<usize>,
        severity: DiagnosticSeverity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            range,
            severity,
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }
}

/// 偏移处命中的诊断（`for_offset` 的返回值）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticItem {
    pub range: Range<usize>,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// 编辑器诊断集合。
///
/// 不变式：
/// - `items` 按 `range.start` 升序，便于二分查找；
/// - 所有 range 已按当前文本长度钳制，并落在字符边界上。
///
/// 该结构目前由**语法诊断**（tree-sitter 的 `ERROR`/`MISSING` 节点）填充，
/// 后续 LSP 接入后可直接 `set()` 覆盖，渲染侧无需改动。
#[derive(Clone, Default)]
pub struct DiagnosticSet {
    items: Vec<Diagnostic>,
    /// `max_end_prefix[i]` = `items[0..=i]` 中最大的 `range.end`。
    ///
    /// 让 [`Self::max_severity_for_range`] 能提前终止：诊断按 `start` 排序，
    /// 但长区间（一个跨几十行的 `ERROR` 节点）起点可能远在查询区间之前，
    /// 只按 `start` 剪枝会退化成全表扫描。前缀最大 `end` 提供「更早的条目
    /// 全部不可能相交」的判据。
    max_end_prefix: Vec<usize>,
    /// 是否有任何诊断，避免渲染热路径反复 `is_empty()` 穿透 Vec。
    non_empty: bool,
}

impl DiagnosticSet {
    pub fn new(_text: &Rope) -> Self {
        Self::default()
    }

    /// 文本整体替换后调用：诊断基于旧文本，必须清空而不是留下错位的区间。
    pub fn reset(&mut self, _text: &Rope) {
        self.clear();
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.max_end_prefix.clear();
        self.non_empty = false;
    }

    /// 整体替换诊断集合（LSP / 语法诊断的统一入口）。
    ///
    /// 会排序、丢弃空区间、按 `text_len` 钳制并吸附到字符边界。
    ///
    /// 吸附方向是有意的：`start` 向左、`end` 向右。诊断区间落在多字节字符
    /// 中间时（LSP 给 UTF-16 偏移、或上游算错边界），向内吸附会把区间压成空
    /// 而**静默丢掉整条诊断**；向外吸附则让下划线覆盖整个字符，既不丢信息
    /// 也不会产生非法 range。
    pub fn set(&mut self, mut items: Vec<Diagnostic>, text: &Rope) {
        let len = text.len();
        items.retain_mut(|d| {
            // 先钳制再判空：越界区间可能钳制后变成合法区间，也可能收缩为空。
            d.range.start = clip_to_boundary(text, d.range.start.min(len), Bias::Left);
            d.range.end = clip_to_boundary(text, d.range.end.min(len), Bias::Right);
            if d.range.end < d.range.start {
                std::mem::swap(&mut d.range.start, &mut d.range.end);
            }
            !d.range.is_empty()
        });
        items.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then_with(|| a.range.end.cmp(&b.range.end))
        });
        // 前缀最大 end：与 items 同步重建，是 max_severity_for_range 剪枝的依据。
        self.max_end_prefix.clear();
        self.max_end_prefix.reserve(items.len());
        let mut running = 0usize;
        for d in &items {
            running = running.max(d.range.end);
            self.max_end_prefix.push(running);
        }
        self.non_empty = !items.is_empty();
        self.items = items;
    }

    pub fn is_empty(&self) -> bool {
        !self.non_empty
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    /// 偏移处命中的**最高级别**诊断。
    ///
    /// 包含区间端点：`range.end` 处的光标也应能看到该诊断（与 Zed 一致，
    /// 光标停在带诊断的 token 末尾时仍显示提示）。
    ///
    /// 扫描范围被前缀最大 `end` 收窄：从右往左走，一旦
    /// `max_end_prefix[i] < offset` 就说明 `0..=i` 的区间全部在 offset 之前
    /// 结束，可以停止。
    ///
    /// 注意是**严格小于**：`for_offset` 包含端点，`end == offset` 仍算命中，
    /// 用 `<=` 会把这个端点命中的条目一起剪掉。
    pub fn for_offset(&self, offset: usize) -> Option<DiagnosticItem> {
        if !self.non_empty {
            return None;
        }
        let mut best: Option<&Diagnostic> = None;
        for i in (0..self.items.len()).rev() {
            if self.max_end_prefix[i] < offset {
                break;
            }
            let d = &self.items[i];
            // 前缀最大 end 只保证「有区间可能相交」，单个区间仍要判 start。
            if d.range.start > offset {
                continue;
            }
            if best.is_none_or(|b| d.severity > b.severity) {
                best = Some(d);
            }
        }
        best.map(|d| DiagnosticItem {
            range: d.range.clone(),
            severity: d.severity,
            message: d.message.clone(),
        })
    }

    /// 一行内出现的最高诊断级别（gutter 标记用）。
    ///
    /// `line_range` 是该行的字节区间（不含换行符）。无诊断返回 `None`。
    pub fn max_severity_for_range(&self, line_range: &Range<usize>) -> Option<DiagnosticSeverity> {
        if !self.non_empty || line_range.is_empty() {
            return None;
        }
        let mut best: Option<DiagnosticSeverity> = None;
        for i in (0..self.items.len()).rev() {
            // 与 for_offset 同款剪枝：更早的条目都不可能与查询区间相交。
            // 这里是半开区间语义（`end > line_range.start` 才算相交），
            // 所以 `<=` 是安全的。
            if self.max_end_prefix[i] <= line_range.start {
                break;
            }
            let d = &self.items[i];
            if d.range.start >= line_range.end {
                continue;
            }
            if best.is_none_or(|b| d.severity > b) {
                best = Some(d.severity);
            }
        }
        best
    }

    /// 把诊断转成可绘制的高亮区间（波浪下划线）。
    ///
    /// 只返回与 `range` 相交的部分，且**裁剪到 `range`**——调用方传的是可视区，
    /// 返回整段会让 gpui 去 shape 视口外的文本。
    pub fn styles_for_range(
        &self,
        range: &Range<usize>,
        cx: &gpui::App,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if !self.non_empty {
            return Vec::new();
        }
        self.items
            .iter()
            .filter_map(|d| {
                let start = d.range.start.max(range.start);
                let end = d.range.end.min(range.end);
                if start >= end {
                    return None;
                }
                Some((
                    start..end,
                    HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(d.severity.color(cx)),
                            wavy: d.severity.is_wavy(),
                        }),
                        ..Default::default()
                    },
                ))
            })
            .collect()
    }
}

/// 吸附方向。`Left` 向左找边界（区间起点用），`Right` 向右找边界（区间终点用）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bias {
    Left,
    Right,
}

/// 把偏移吸附到最近的字符边界（`Rope` 是 UTF-8，非边界偏移会让
/// `slice`/`paint` 直接 panic）。
fn clip_to_boundary(text: &Rope, offset: usize, bias: Bias) -> usize {
    let len = text.len();
    if offset >= len {
        return len;
    }
    if text.is_char_boundary(offset) {
        return offset;
    }
    match bias {
        Bias::Left => {
            let mut o = offset;
            while o > 0 && !text.is_char_boundary(o) {
                o -= 1;
            }
            o
        }
        Bias::Right => {
            let mut o = offset;
            while o < len && !text.is_char_boundary(o) {
                o += 1;
            }
            o
        }
    }
}

/// 从 tree-sitter 语法树收集语法诊断（`ERROR` / `MISSING` 节点）。
///
/// 这是**不依赖 LSP** 的零成本产出方：语法树本来就要为高亮构建，
/// 遍历一遍是 O(节点数) 且只在解析落地后跑一次。
///
/// 设计取舍：
/// - 只报最外层的 `ERROR` 节点（`is_error()`），其内部通常还有嵌套错误节点，
///   全部上报会让一行出现几十条重叠诊断；
/// - `MISSING` 是零宽节点（插入点），展开到 1 个字符宽度，否则无法绘制下划线；
/// - 二者都跳过零宽且无法展开的区间，避免污染 gutter。
pub fn syntax_diagnostics(root: tree_sitter::Node<'_>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    // 显式栈遍历：tree-sitter 的 TreeCursor 在嵌套错误节点上递归深度不可控，
    // 大文件里递归会爆栈。
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            if let Some(d) = error_diagnostic(node, "语法错误") {
                out.push(d);
            }
            // 错误节点内部不再下探：子错误是同一个语法错误的碎片。
            continue;
        }
        if node.is_missing() {
            if let Some(d) = missing_diagnostic(node) {
                out.push(d);
            }
            continue;
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out.sort_by_key(|d| (d.range.start, d.range.end));
    out
}

/// [`syntax_diagnostics`] 的便捷包装：树缺失（尚未解析）时返回空。
pub fn syntax_diagnostics_opt(tree: Option<&tree_sitter::Tree>) -> Vec<Diagnostic> {
    match tree {
        Some(tree) => syntax_diagnostics(tree.root_node()),
        None => Vec::new(),
    }
}

fn error_diagnostic(node: tree_sitter::Node<'_>, message: &str) -> Option<Diagnostic> {
    let range = node.byte_range();
    if range.is_empty() {
        return None;
    }
    Some(Diagnostic::new(range, DiagnosticSeverity::Error, message).with_source("tree-sitter"))
}

fn missing_diagnostic(node: tree_sitter::Node<'_>) -> Option<Diagnostic> {
    let at = node.start_byte();
    // 零宽节点：给一个 1 字节的可视区间，否则无法绘制下划线也不会出现在 gutter。
    let range = at..(at + 1);
    Some(
        Diagnostic::new(
            range,
            DiagnosticSeverity::Error,
            format!("缺少 {}", node.kind()),
        )
        .with_source("tree-sitter"),
    )
}

/// Stub SyntaxHighlighter alias pointing to our SimpleHighlighter
pub use simple::SimpleHighlighter as SyntaxHighlighter;

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from(s)
    }

    #[test]
    fn set_sorts_clips_and_drops_empty() {
        let text = rope("hello world");
        let mut set = DiagnosticSet::new(&text);
        set.set(
            vec![
                Diagnostic::new(6..11, DiagnosticSeverity::Warning, "w"),
                Diagnostic::new(0..5, DiagnosticSeverity::Error, "e"),
                Diagnostic::new(3..3, DiagnosticSeverity::Info, "empty"),
                Diagnostic::new(8..999, DiagnosticSeverity::Hint, "past-end"),
            ],
            &text,
        );
        // 空区间被丢弃；越界被钳制到文本长度。
        let items = set.items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].range, 0..5);
        assert_eq!(items[1].range, 6..11);
        assert_eq!(items[2].range, 8..11);
        assert!(!set.is_empty());
    }

    #[test]
    fn set_snaps_to_char_boundary_by_expanding() {
        // "中" 占 3 字节；偏移 1/2 都不是字符边界。
        let text = rope("中x");
        let mut set = DiagnosticSet::new(&text);
        set.set(
            vec![Diagnostic::new(1..2, DiagnosticSeverity::Error, "e")],
            &text,
        );
        // 向外吸附 → 覆盖整个「中」，而不是被压成空区间后静默丢掉。
        assert_eq!(set.items().len(), 1);
        assert_eq!(set.items()[0].range, 0..3);
        assert!(text.is_char_boundary(set.items()[0].range.start));
        assert!(text.is_char_boundary(set.items()[0].range.end));
    }

    #[test]
    fn set_keeps_out_of_range_diagnostic_after_clipping() {
        let text = rope("abc");
        let mut set = DiagnosticSet::new(&text);
        set.set(
            vec![Diagnostic::new(1..999, DiagnosticSeverity::Warning, "w")],
            &text,
        );
        assert_eq!(set.items()[0].range, 1..3);
    }

    #[test]
    fn for_offset_picks_highest_severity_and_includes_endpoints() {
        let text = rope("abcdefgh");
        let mut set = DiagnosticSet::new(&text);
        set.set(
            vec![
                Diagnostic::new(0..4, DiagnosticSeverity::Warning, "w"),
                Diagnostic::new(0..2, DiagnosticSeverity::Error, "e"),
            ],
            &text,
        );
        assert_eq!(
            set.for_offset(1).map(|d| d.severity),
            Some(DiagnosticSeverity::Error)
        );
        // 端点包含：offset 2 同时落在两条诊断的闭区间上，取更高级别。
        assert_eq!(
            set.for_offset(2).map(|d| d.severity),
            Some(DiagnosticSeverity::Error)
        );
        // 4 是 warning 的 end，仍应命中。
        assert_eq!(
            set.for_offset(4).map(|d| d.severity),
            Some(DiagnosticSeverity::Warning)
        );
        assert!(set.for_offset(5).is_none());
    }

    #[test]
    fn reset_clears_everything() {
        let text = rope("abc");
        let mut set = DiagnosticSet::new(&text);
        set.set(
            vec![Diagnostic::new(0..1, DiagnosticSeverity::Error, "e")],
            &text,
        );
        assert!(!set.is_empty());
        set.reset(&text);
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert!(set.for_offset(0).is_none());
    }

    #[test]
    fn max_severity_for_range_reports_per_line_worst() {
        let text = rope("aaa\nbbb\nccc\n");
        let mut set = DiagnosticSet::new(&text);
        set.set(
            vec![
                Diagnostic::new(0..1, DiagnosticSeverity::Hint, "h"),
                Diagnostic::new(1..2, DiagnosticSeverity::Error, "e"),
                Diagnostic::new(8..9, DiagnosticSeverity::Warning, "w"),
            ],
            &text,
        );
        // 第一行（0..3）同时有 Hint 和 Error → 取 Error。
        assert_eq!(
            set.max_severity_for_range(&(0..3)),
            Some(DiagnosticSeverity::Error)
        );
        // 第二行（4..7）无诊断。
        assert_eq!(set.max_severity_for_range(&(4..7)), None);
        // 第三行（8..11）只有 Warning。
        assert_eq!(
            set.max_severity_for_range(&(8..11)),
            Some(DiagnosticSeverity::Warning)
        );
    }

    #[test]
    fn syntax_diagnostics_finds_error_and_missing() {
        // 未闭合的括号 → tree-sitter 产出 ERROR 或 MISSING 节点。
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_json::LANGUAGE.into())
            .expect("json grammar");
        let src = r#"{"a": }"#;
        let tree = parser.parse(src, None).expect("parse");
        let diags = syntax_diagnostics(tree.root_node());
        assert!(
            !diags.is_empty(),
            "expected at least one syntax diagnostic for {src:?}"
        );
        assert!(
            diags
                .iter()
                .all(|d| d.severity == DiagnosticSeverity::Error),
            "syntax diagnostics are errors"
        );
        assert!(
            diags.iter().all(|d| d.range.start <= src.len()),
            "diagnostic ranges must stay in bounds"
        );
    }

    #[test]
    fn syntax_diagnostics_clean_source_is_empty() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_json::LANGUAGE.into())
            .expect("json grammar");
        let tree = parser.parse(r#"{"a": 1}"#, None).expect("parse");
        assert!(syntax_diagnostics(tree.root_node()).is_empty());
    }
}
