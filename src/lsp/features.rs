//! LSP 响应 → 编辑器数据结构的转换。
//!
//! 这一层是「协议世界」与「编辑器世界」的边界：
//! - 偏移口径：LSP 用 (line, character)，编辑器用 UTF-8 字节 offset；
//! - 诊断形态：LSP 的 `Diagnostic` 要变成 [`DiagnosticSet`] 认识的结构；
//! - `WorkspaceEdit` 要变成可以直接打到 `Rope` 上的编辑。
//!
//! 所有转换都是纯函数（不碰 UI），所以可以单测。

use std::collections::HashMap;
use std::ops::Range;

use lsp_types::{
    DiagnosticSeverity as LspSeverity, Location, Position, TextEdit, Url, WorkspaceEdit,
};
use ropey::Rope;

use crate::components::input::RopeExt as _;
use crate::editor::syntax::{Diagnostic, DiagnosticSeverity};

// ------------------------------------------------------------------ 位置转换

/// LSP `Position` → 编辑器 UTF-8 字节 offset。
///
/// 复用了 [`RopeExt::position_to_offset`]：按 **UTF-16 code unit** 计数，
/// 与 LSP 规范的 `character` 语义（及 Zed 的 `PointUtf16`）一致。
/// 星体字符（emoji）占 2 个 code unit，换算正确。
pub fn position_to_offset(rope: &Rope, position: Position) -> usize {
    rope.position_to_offset(&crate::components::input::Position::new(
        position.line,
        position.character,
    ))
    .min(rope.len())
}

/// 编辑器 UTF-8 字节 offset → LSP `Position`。
pub fn offset_to_position(rope: &Rope, offset: usize) -> Position {
    let p = rope.offset_to_position(offset.min(rope.len()));
    Position::new(p.line, p.character)
}

/// LSP `Range` → 编辑器字节区间。
///
/// 起点大于终点时交换：少数服务器在错误恢复路径上会给出反向区间，
/// 直接 `start..end` 会得到一个空区间并静默丢掉整条诊断。
pub fn range_to_bytes(rope: &Rope, range: lsp_types::Range) -> Range<usize> {
    let start = position_to_offset(rope, range.start);
    let end = position_to_offset(rope, range.end);
    if start > end {
        end..start
    } else {
        start..end
    }
}

// ------------------------------------------------------------------ 诊断

/// LSP 诊断 → 编辑器诊断。
///
/// `source` 前缀成 `lsp:<source>`，与 tree-sitter 的 `"tree-sitter"`
/// 区分开，便于将来在 UI 上标注诊断来源。
pub fn to_diagnostics(rope: &Rope, lsp: Vec<lsp_types::Diagnostic>) -> Vec<Diagnostic> {
    lsp.into_iter()
        .filter_map(|d| {
            let range = range_to_bytes(rope, d.range);
            if range.is_empty() {
                // 零宽诊断画不出下划线也进不了 gutter，丢弃比占位更有用。
                return None;
            }
            let message = build_message(&d);
            let source = d.source.map(|s| format!("lsp:{s}"));
            let mut diag = Diagnostic::new(range, severity(d.severity), message);
            if let Some(source) = source {
                diag = diag.with_source(source);
            }
            Some(diag)
        })
        .collect()
}

/// LSP 严重级别 → 编辑器严重级别。
///
/// `None` 按 LSP 规范视为 `Error`（1）——规范里 `severity` 可选，
/// 省略时默认值是 1。
fn severity(s: Option<LspSeverity>) -> DiagnosticSeverity {
    match s.unwrap_or(LspSeverity::ERROR) {
        LspSeverity::ERROR => DiagnosticSeverity::Error,
        LspSeverity::WARNING => DiagnosticSeverity::Warning,
        LspSeverity::INFORMATION => DiagnosticSeverity::Info,
        LspSeverity::HINT => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Info,
    }
}

/// 把诊断的关联信息（`relatedInformation`）拼进消息。
///
/// rust-analyzer 的很多主诊断正文很短，真正的解释在 related 里
/// （"consider importing this trait"）。不拼的话用户只看到一句废话。
fn build_message(d: &lsp_types::Diagnostic) -> String {
    let mut msg = d.message.clone();
    if let Some(related) = &d.related_information {
        for info in related {
            let location = format_location_short(&info.location);
            msg.push_str(&format!("\n  {location}: {}", info.message));
        }
    }
    msg
}

fn format_location_short(location: &Location) -> String {
    let name = location
        .uri
        .to_file_path()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| location.uri.to_string());
    format!("{}:{}", name, location.range.start.line + 1)
}

// ------------------------------------------------------------------ 编辑

/// 一次待应用的文本编辑（已换算成字节区间，按应用顺序排好）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteEdit {
    pub range: Range<usize>,
    pub new_text: String,
}

/// `TextEdit[]` → 字节区间编辑，**按应用顺序（从后往前）排序**。
///
/// 必须逆序应用：先改后面的区间才不会让前面区间的偏移失效。
/// 调用方拿到后直接顺序 `replace` 即可，不需要自己再排。
pub fn to_byte_edits(rope: &Rope, edits: Vec<TextEdit>) -> Vec<ByteEdit> {
    let mut out: Vec<ByteEdit> = edits
        .into_iter()
        .map(|e| ByteEdit {
            range: range_to_bytes(rope, e.range),
            new_text: e.new_text,
        })
        .collect();
    // 按起点降序：后编辑的先应用。
    out.sort_by_key(|b| std::cmp::Reverse(b.range.start));
    out
}

/// 把字节编辑应用到文本上，返回新文本。
///
/// 区间已按降序排好（见 [`to_byte_edits`]），直接顺序替换。
pub fn apply_byte_edits(text: &str, edits: &[ByteEdit]) -> String {
    let mut out = text.to_string();
    for edit in edits {
        let start = edit.range.start.min(out.len());
        let end = edit.range.end.min(out.len());
        if start > end {
            continue;
        }
        out.replace_range(start..end, &edit.new_text);
    }
    out
}

/// `WorkspaceEdit` 里属于某个 uri 的编辑。
///
/// 重命名可能跨多个文件；编辑器一次只能改当前文件，所以按 uri 分组后
/// 由调用方决定处理哪些（首版只应用当前文件的部分）。
/// 从 `WorkspaceEdit` 取出属于某个 uri 的编辑。
///
/// LSP 允许服务器用**两种形态**表达编辑（规范里 `changes` 与
/// `documentChanges` 至少有一个）：
///
/// - `changes`：`{ uri: TextEdit[] }` 的映射；
/// - `documentChanges`：`TextDocumentEdit[]`（带版本信息）或
///   `DocumentChangeOperation[]`（含创建/重命名/删除等资源操作）。
///
/// 两种都要认。此前只读 `changes`，于是只发 `documentChanges` 的服务器
/// （rust-analyzer 的重命名、部分服务器的格式化）编辑会被**静默丢弃**。
///
/// 两种形态同时出现时（规范允许，但少见）合并结果：先 `changes`，
/// 再按顺序追加 `documentChanges` 里的文本编辑。
pub fn edits_for_uri(edit: &WorkspaceEdit, uri: &Url) -> Option<Vec<TextEdit>> {
    let mut out: Vec<TextEdit> = Vec::new();

    if let Some(changes) = edit.changes.as_ref()
        && let Some(edits) = changes.get(uri)
    {
        out.extend(edits.iter().cloned());
    }

    if let Some(document_changes) = &edit.document_changes {
        match document_changes {
            // `TextDocumentEdit[]`：每个条目属于一个 uri，带若干编辑。
            lsp_types::DocumentChanges::Edits(edits) => {
                for e in edits {
                    if &e.text_document.uri == uri {
                        // `OneOf<TextEdit, AnnotatedTextEdit>`：两种都取出
                        // 底层 `TextEdit`（我们暂不消费 annotation id）。
                        for edit in &e.edits {
                            out.push(one_of_text_edit(edit));
                        }
                    }
                }
            }
            // `DocumentChangeOperation[]`：只有 `Edit` 变体带文本编辑，
            // 资源操作（create/rename/delete）由调用方另行处理。
            lsp_types::DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(e) = op
                        && &e.text_document.uri == uri
                    {
                        for edit in &e.edits {
                            out.push(one_of_text_edit(edit));
                        }
                    }
                }
            }
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

/// 从 `OneOf<TextEdit, AnnotatedTextEdit>` 取出底层 `TextEdit`。
///
/// `AnnotatedTextEdit` 只是多带一个 `annotation_id`（用于「哪些编辑属于
/// 同一次改动」的 UI 提示），我们暂不消费它，取文本编辑部分即可。
fn one_of_text_edit(edit: &lsp_types::OneOf<TextEdit, lsp_types::AnnotatedTextEdit>) -> TextEdit {
    match edit {
        lsp_types::OneOf::Left(e) => e.clone(),
        lsp_types::OneOf::Right(e) => e.text_edit.clone(),
    }
}

/// `WorkspaceEdit` 影响的文件 uri 列表。
pub fn affected_uris(edit: &WorkspaceEdit) -> Vec<Url> {
    let mut uris: Vec<Url> = edit
        .changes
        .as_ref()
        .map(|c| c.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(ops) = &edit.document_changes {
        match ops {
            lsp_types::DocumentChanges::Edits(edits) => {
                for e in edits {
                    uris.push(e.text_document.uri.clone());
                }
            }
            lsp_types::DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        lsp_types::DocumentChangeOperation::Edit(e) => {
                            uris.push(e.text_document.uri.clone())
                        }
                        lsp_types::DocumentChangeOperation::Op(op) => match op {
                            lsp_types::ResourceOp::Create(c) => uris.push(c.uri.clone()),
                            lsp_types::ResourceOp::Rename(r) => {
                                uris.push(r.old_uri.clone());
                                uris.push(r.new_uri.clone());
                            }
                            lsp_types::ResourceOp::Delete(d) => uris.push(d.uri.clone()),
                        },
                    }
                }
            }
        }
    }
    uris.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    uris.dedup();
    uris
}

// ------------------------------------------------------------------ 跳转

/// `GotoDefinitionResponse` 的第一个位置。
///
/// 多个结果时首版跳第一个（与 Zed 在无 "split" 意图时的行为一致）；
/// 调用方要列多个结果可直接用 [`locations_of`]。
pub fn first_location(response: &lsp_types::GotoDefinitionResponse) -> Option<Location> {
    match response {
        lsp_types::GotoDefinitionResponse::Scalar(l) => Some(l.clone()),
        lsp_types::GotoDefinitionResponse::Array(v) => v.first().cloned(),
        lsp_types::GotoDefinitionResponse::Link(links) => {
            links.first().map(|l| Location {
                uri: l.target_uri.clone(),
                range: l.target_range,
            })
        }
    }
}

/// 展平 `GotoDefinitionResponse` 的全部位置。
pub fn locations_of(response: &lsp_types::GotoDefinitionResponse) -> Vec<Location> {
    match response {
        lsp_types::GotoDefinitionResponse::Scalar(l) => vec![l.clone()],
        lsp_types::GotoDefinitionResponse::Array(v) => v.clone(),
        lsp_types::GotoDefinitionResponse::Link(links) => links
            .iter()
            .map(|l| Location {
                uri: l.target_uri.clone(),
                range: l.target_range,
            })
            .collect(),
    }
}

// ------------------------------------------------------------------ 补全

/// 补全项的展示文本（`label` 优先，回退 `insertText`/`textEdit` 的新文本）。
///
/// `label` 为空的项在菜单里会显示成一条空白，直接过滤掉。
pub fn completion_label(item: &lsp_types::CompletionItem) -> String {
    if !item.label.is_empty() {
        return item.label.clone();
    }
    if let Some(insert) = &item.insert_text {
        return insert.clone();
    }
    if let Some(lsp_types::CompletionTextEdit::Edit(e)) = &item.text_edit {
        return e.new_text.clone();
    }
    String::new()
}

/// 补全项的排序键：sortText 优先，否则 label。
///
/// 服务器给的 `sortText` 才是它期望的顺序（比如 `_` 前缀的私有项排后面），
/// 直接用 label 排会把服务器精心安排的顺序打乱。
pub fn completion_sort_key(item: &lsp_types::CompletionItem) -> &str {
    item.sort_text.as_deref().unwrap_or(&item.label)
}

/// 补全项要插入的文本。
///
/// 优先 `textEdit.newText`（服务器精确指定了替换区间），
/// 否则 `insertText`，最后回退 `label`。
pub fn completion_insert_text(item: &lsp_types::CompletionItem) -> String {
    if let Some(lsp_types::CompletionTextEdit::Edit(e)) = &item.text_edit {
        return e.new_text.clone();
    }
    if let Some(insert) = &item.insert_text {
        return insert.clone();
    }
    item.label.clone()
}

/// 补全项替换的字节区间。
///
/// `textEdit` 带了区间就用它（服务器会吃掉用户已输入的前缀），
/// 否则替换光标前的整个当前词——这是「无 textEdit」时的通用做法。
pub fn completion_replace_range(
    rope: &Rope,
    offset: usize,
    item: &lsp_types::CompletionItem,
) -> Range<usize> {
    if let Some(lsp_types::CompletionTextEdit::Edit(e)) = &item.text_edit {
        return range_to_bytes(rope, e.range);
    }
    word_range_before(rope, offset)
}

/// 字符在「词」中的角色。对照 Zed 的 `language::CharKind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharKind {
    Whitespace,
    Punctuation,
    Word,
}

/// 判断字符是否属于标识符（Zed 的 `CharScopeContext`）。
///
/// 默认与 Zed 的 `kind_with` 完全一致：
/// ```text
/// c.is_alphanumeric() || c == '_'  -> Word
/// c.is_whitespace()                -> Whitespace
/// 其余                             -> Punctuation
/// ```
///
/// `extra_word_chars` 对应 Zed 的 `scope.completion_query_characters()`：
/// 某些语言把 `.`（import 路径 `foo.ts`）或 `-`（Tailwind `bg-yellow-100`）
/// 也算词字符。**不能无条件把 `.` 当词字符**——在 Rust 里 `foo.` 的点是
/// 标点，把它算进词会让补全替换区间多吞一个点。
pub fn char_kind(c: char, extra_word_chars: &[char]) -> CharKind {
    if c.is_alphanumeric() || c == '_' {
        return CharKind::Word;
    }
    if extra_word_chars.contains(&c) {
        return CharKind::Word;
    }
    if c.is_whitespace() {
        return CharKind::Whitespace;
    }
    CharKind::Punctuation
}

/// 默认的补全查询字符集（无语言作用域信息时）。
///
/// 空集：只有字母数字与 `_` 算词字符。语言相关的额外字符由调用方通过
/// [`char_kind`] 的 `extra_word_chars` 传入。
pub const DEFAULT_EXTRA_WORD_CHARS: &[char] = &[];

/// 光标前连续「词字符」的区间（Zed 的 `surrounding_word` 左半部分）。
///
/// 词字符的判定见 [`char_kind`]：字母数字与 `_`，外加调用方给的
/// `extra_word_chars`（如 import 路径里的 `.`）。
///
/// **必须走 `RopeExt` 的边界吸附原语**：`offset` 可能落在多字节字符内部
/// （上游换算偏差、或 LSP 给的列号），而 `Rope::char` 在非字符边界上会
/// panic。这里先把 offset 向左吸附，再**按字符下标**回退。
pub fn word_range_before(rope: &Rope, offset: usize) -> Range<usize> {
    word_range_before_with(rope, offset, DEFAULT_EXTRA_WORD_CHARS)
}

/// 带语言额外词字符的 [`word_range_before`]。
pub fn word_range_before_with(
    rope: &Rope,
    offset: usize,
    extra_word_chars: &[char],
) -> Range<usize> {
    use crate::components::input::rope_ext::RopeExt as _;

    let end = offset.min(rope.len());
    if end == 0 {
        return 0..0;
    }
    // 先吸附：越界偏移钳制到文本长度，落在字符中间则向左对齐。
    let end = rope.clip_offset(end, sum_tree::Bias::Left);
    let mut ci = rope.offset_to_char_index(end);
    while ci > 0 {
        let byte_off = rope.char_index_to_offset(ci - 1);
        let ch = rope.char(byte_off);
        if char_kind(ch, extra_word_chars) == CharKind::Word {
            ci -= 1;
        } else {
            break;
        }
    }
    let start = rope.char_index_to_offset(ci);
    start..end
}

// ------------------------------------------------------------------ 悬浮

/// `Hover` 的展示文本。
///
/// `MarkupContent`（markdown，可能有代码块）优先；旧服务器返回
/// `MarkedString` 数组，逐个拼接。`{language:..., value:...}` 形态的
/// `MarkedString` 需要带上语言标记才能正确高亮，这里把它包成代码块。
pub fn hover_contents(hover: &lsp_types::Hover) -> String {
    match &hover.contents {
        lsp_types::HoverContents::Scalar(s) => marked_string_to_text(s),
        lsp_types::HoverContents::Markup(m) => m.value.clone(),
        lsp_types::HoverContents::Array(items) => items
            .iter()
            .map(marked_string_to_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn marked_string_to_text(s: &lsp_types::MarkedString) -> String {
    match s {
        lsp_types::MarkedString::String(text) => text.clone(),
        lsp_types::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

/// `Hover` 覆盖的源区间（用于给 hover 位置加高亮）。
pub fn hover_range(rope: &Rope, hover: &lsp_types::Hover) -> Option<Range<usize>> {
    hover.range.map(|r| range_to_bytes(rope, r))
}

// ------------------------------------------------------------------ 语义 token

/// 语义 token 的扁平数组 → (行, 起始列, 长度, token 类型索引) 列表。
///
/// LSP 的编码是：每 5 个整数一组
/// `[delta_line, delta_start_char, length, token_type, token_modifiers]`。
/// `delta_line` 是相对上一 token 的行差，`delta_start_char` 在换行的
/// 情况下是绝对列、否则是相对上一 token 的列差——这个「换行重置」的
/// 规则是最容易写错的地方。
pub fn decode_semantic_tokens(data: &[u32]) -> Vec<(u32, u32, u32, u32)> {
    let mut out = Vec::with_capacity(data.len() / 5);
    let mut line = 0u32;
    let mut start = 0u32;
    for chunk in data.chunks_exact(5) {
        let delta_line = chunk[0];
        let delta_start = chunk[1];
        let length = chunk[2];
        let token_type = chunk[3];

        line += delta_line;
        if delta_line == 0 {
            start += delta_start;
        } else {
            start = delta_start;
        }
        out.push((line, start, length, token_type));
    }
    out
}

/// 语义 token 类型 → tree-sitter 高亮名。
///
/// 只有一部分 token 类型能映射到现有高亮名；其余返回 `None`，
/// 调用方跳过（保留 tree-sitter 已有的着色，比套一个错的类别好）。
///
/// 用 `if` 链而不是 `match`：`SemanticTokenType` 内部是
/// `Cow<'static, str>`，不是结构化常量，不能出现在模式位置。
pub fn semantic_token_highlight(token_type: &lsp_types::SemanticTokenType) -> Option<&'static str> {
    let t = token_type.as_str();
    let name = match t {
        "namespace" => "namespace",
        "type" => "type",
        "class" => "type",
        "enum" => "type",
        "interface" => "type",
        "struct" => "type",
        "typeParameter" => "type",
        "parameter" => "variable.parameter",
        "variable" => "variable",
        "property" => "property",
        "function" => "function",
        "method" => "function.method",
        "macro" => "function.macro",
        "keyword" => "keyword",
        "comment" => "comment",
        "string" => "string",
        "number" => "number",
        "operator" => "operator",
        _ => return None,
    };
    Some(name)
}

// ------------------------------------------------------------------ 符号

/// 扁平化后的文档符号（含嵌套层级）。
#[derive(Debug, Clone)]
pub struct FlatSymbol {
    pub name: String,
    /// 符号种类（`function` / `class` / …），用于选图标。
    pub kind: lsp_types::SymbolKind,
    /// 定义位置的字节区间。
    pub range: Range<usize>,
    /// 嵌套深度，0 为顶层。
    pub depth: usize,
}

/// `DocumentSymbolResponse` → 扁平符号列表。
///
/// 嵌套符号（`DocumentSymbol.children`）递归展开成带 `depth` 的平表，
/// 这样 UI 只渲染一个列表，缩进由 depth 决定。
pub fn flatten_symbols(rope: &Rope, response: &lsp_types::DocumentSymbolResponse) -> Vec<FlatSymbol> {
    let mut out = Vec::new();
    match response {
        lsp_types::DocumentSymbolResponse::Nested(nested) => {
            for symbol in nested {
                collect_nested(rope, symbol, 0, &mut out);
            }
        }
        lsp_types::DocumentSymbolResponse::Flat(flat) => {
            for symbol in flat {
                // 扁平形态（`SymbolInformation`）：位置在 `location.range`。
                out.push(FlatSymbol {
                    name: symbol.name.clone(),
                    kind: symbol.kind,
                    range: range_to_bytes(rope, symbol.location.range),
                    depth: 0,
                });
            }
        }
    }
    out
}

fn collect_nested(
    rope: &Rope,
    symbol: &lsp_types::DocumentSymbol,
    depth: usize,
    out: &mut Vec<FlatSymbol>,
) {
    // `selection_range` 是标识符本身，`range` 是整个定义体。
    // 跳转要用前者——后者会选中整个函数体。
    let range = range_to_bytes(rope, symbol.selection_range);
    out.push(FlatSymbol {
        name: symbol.name.clone(),
        kind: symbol.kind,
        range,
        depth,
    });
    if let Some(children) = &symbol.children {
        for child in children {
            collect_nested(rope, child, depth + 1, out);
        }
    }
}

/// 符号种类 → 展示用的单字符标记。
///
/// 图标资源有限，用文本标记代替（也能在纯文本面板里正常工作）。
pub fn symbol_kind_marker(kind: lsp_types::SymbolKind) -> &'static str {
    match kind {
        lsp_types::SymbolKind::FUNCTION | lsp_types::SymbolKind::METHOD => "ƒ",
        lsp_types::SymbolKind::CLASS => "C",
        lsp_types::SymbolKind::STRUCT => "S",
        lsp_types::SymbolKind::INTERFACE => "I",
        lsp_types::SymbolKind::ENUM => "E",
        lsp_types::SymbolKind::CONSTANT => "c",
        lsp_types::SymbolKind::VARIABLE => "v",
        lsp_types::SymbolKind::PROPERTY | lsp_types::SymbolKind::FIELD => "p",
        lsp_types::SymbolKind::MODULE | lsp_types::SymbolKind::NAMESPACE => "M",
        lsp_types::SymbolKind::TYPE_PARAMETER => "T",
        _ => "•",
    }
}

// ------------------------------------------------------------------ 折叠

/// 折叠范围（字节区间）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldRange {
    /// 起始行（0 基，含）。
    pub start_line: u32,
    /// 结束行（0 基，含）。
    pub end_line: u32,
}

/// `FoldingRange[]` → 折叠范围。
///
/// `start_line`/`end_line` 是规范里的主字段；`start_character`/
/// `end_character` 只做行内精确定位，行级折叠用不上。
pub fn to_fold_ranges(ranges: Vec<lsp_types::FoldingRange>) -> Vec<FoldRange> {
    ranges
        .into_iter()
        .filter_map(|r| {
            if r.end_line <= r.start_line {
                // 单行或反向：折叠它没有意义。
                return None;
            }
            Some(FoldRange {
                start_line: r.start_line,
                end_line: r.end_line,
            })
        })
        .collect()
}

// ------------------------------------------------------------------ Inlay hint

/// 一个 inlay hint（已换算成字节偏移）。
#[derive(Debug, Clone)]
pub struct InlayHintData {
    /// 插入位置（字节偏移）。
    pub offset: usize,
    pub text: String,
    /// 是否放在插入点之后（参数名 hint 通常是 true）。
    pub after: bool,
}

/// `InlayHint[]` → 编辑器 hint。
pub fn to_inlay_hints(rope: &Rope, hints: Vec<lsp_types::InlayHint>) -> Vec<InlayHintData> {
    hints
        .into_iter()
        .filter_map(|h| {
            let text = match &h.label {
                lsp_types::InlayHintLabel::String(s) => s.clone(),
                lsp_types::InlayHintLabel::LabelParts(parts) => {
                    parts.iter().map(|p| p.value.clone()).collect()
                }
            };
            if text.is_empty() {
                return None;
            }
            Some(InlayHintData {
                offset: position_to_offset(rope, h.position),
                text,
                // `padding_left` 为真表示 hint 应出现在插入点之后
                // （如参数名），否则在前（如类型标注）。
                after: h.padding_left.unwrap_or(false),
            })
        })
        .collect()
}

// ------------------------------------------------------------------ 签名帮助

/// 签名帮助的展示文本。
///
/// 把参数列表里当前参数高亮出来：普通文本面板无法做富文本，
/// 用 `>` 前缀标记当前参数。
pub fn signature_text(help: &lsp_types::SignatureHelp) -> String {
    let signature = match help.signatures.get(help.active_signature.unwrap_or(0) as usize) {
        Some(s) => s,
        None => return String::new(),
    };
    let mut out = signature.label.clone();
    // `let_chains` 在 edition 2024 可用，但这里保持嵌套以兼容更老的工具链。
    if let Some(active) = help.active_parameter.or(signature.active_parameter)
        && let Some(param) = signature
            .parameters
            .as_ref()
            .and_then(|ps| ps.get(active as usize))
    {
        // 参数标签有两种形态：直接字符串，或相对整个签名的 [start, end] 偏移。
        let label = match &param.label {
            lsp_types::ParameterLabel::Simple(s) => s.clone(),
            lsp_types::ParameterLabel::LabelOffsets(offs) => signature
                .label
                .chars()
                .skip(offs[0] as usize)
                .take((offs[1] - offs[0]) as usize)
                .collect(),
        };
        out.push_str(&format!("\n\n> {label}"));
    }
    out
}

/// Code Action → `(标题, 是否首选)`。
pub fn code_action_label(action: &lsp_types::CodeActionOrCommand) -> Option<(String, bool)> {
    match action {
        lsp_types::CodeActionOrCommand::CodeAction(a) => {
            Some((a.title.clone(), a.is_preferred.unwrap_or(false)))
        }
        lsp_types::CodeActionOrCommand::Command(c) => Some((c.title.clone(), false)),
    }
}

/// 文档高亮 → 字节区间。
pub fn to_highlight_ranges(rope: &Rope, highlights: Vec<lsp_types::DocumentHighlight>) -> Vec<Range<usize>> {
    highlights
        .into_iter()
        .map(|h| range_to_bytes(rope, h.range))
        .collect()
}

/// 诊断计数（按严重级别），供状态栏显示。
pub fn severity_counts(diagnostics: &[lsp_types::Diagnostic]) -> HashMap<DiagnosticSeverity, usize> {
    let mut map = HashMap::new();
    for d in diagnostics {
        *map.entry(severity(d.severity)).or_insert(0) += 1;
    }
    map
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use lsp_types::Position;

    fn rope(s: &str) -> Rope {
        Rope::from(s)
    }

    fn pos(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lsp_types::Range {
        lsp_types::Range::new(pos(sl, sc), pos(el, ec))
    }

    #[test]
    fn position_roundtrip() {
        let text = rope("hello world\nsecond line\n");
        let offset = position_to_offset(&text, pos(1, 7));
        assert_eq!(&text.slice(offset..offset + 4).to_string(), "line");
        assert_eq!(offset_to_position(&text, offset), pos(1, 7));
    }

    #[test]
    fn position_clamps_past_end() {
        // 服务器可能给越界位置（旧版本诊断），不能 panic。
        let text = rope("ab");
        assert_eq!(position_to_offset(&text, pos(99, 0)), text.len());
    }

    #[test]
    fn range_swaps_reversed() {
        let text = rope("abcdef");
        // 反向区间应被交换，而不是塌陷成空区间。
        assert_eq!(range_to_bytes(&text, range(0, 4, 0, 1)), 1..4);
    }

    #[test]
    fn diagnostics_drop_empty_ranges() {
        let text = rope("abc");
        let d = lsp_types::Diagnostic {
            range: range(0, 1, 0, 1),
            message: "empty".into(),
            severity: Some(LspSeverity::ERROR),
            source: Some("rustc".into()),
            ..Default::default()
        };
        assert!(to_diagnostics(&text, vec![d]).is_empty());
    }

    #[test]
    fn diagnostics_map_severity_and_source() {
        let text = rope("let x = 1;");
        let d = lsp_types::Diagnostic {
            range: range(0, 0, 0, 3),
            message: "unused".into(),
            severity: Some(LspSeverity::WARNING),
            source: Some("rust-analyzer".into()),
            ..Default::default()
        };
        let out = to_diagnostics(&text, vec![d]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, DiagnosticSeverity::Warning);
        assert_eq!(out[0].source.as_deref(), Some("lsp:rust-analyzer"));
        assert_eq!(out[0].range, 0..3);
    }

    #[test]
    fn missing_severity_defaults_to_error() {
        let text = rope("abc");
        let d = lsp_types::Diagnostic {
            range: range(0, 0, 0, 1),
            message: "x".into(),
            severity: None,
            ..Default::default()
        };
        assert_eq!(
            to_diagnostics(&text, vec![d])[0].severity,
            DiagnosticSeverity::Error
        );
    }

    #[test]
    fn related_information_is_appended() {
        let text = rope("abc");
        let d = lsp_types::Diagnostic {
            range: range(0, 0, 0, 1),
            message: "main".into(),
            related_information: Some(vec![lsp_types::DiagnosticRelatedInformation {
                location: Location {
                    uri: Url::parse("file:///lib.rs").unwrap(),
                    range: range(10, 0, 10, 5),
                },
                message: "defined here".into(),
            }]),
            ..Default::default()
        };
        let out = to_diagnostics(&text, vec![d]);
        assert!(out[0].message.contains("main"));
        assert!(out[0].message.contains("defined here"));
        // 行号对用户是 1 基。
        assert!(out[0].message.contains("lib.rs:11"));
    }

    #[test]
    fn byte_edits_sorted_back_to_front() {
        let text = rope("aaa bbb ccc");
        let edits = vec![
            TextEdit {
                range: range(0, 0, 0, 3),
                new_text: "X".into(),
            },
            TextEdit {
                range: range(0, 8, 0, 11),
                new_text: "Z".into(),
            },
        ];
        let out = to_byte_edits(&text, edits);
        // 逆序：后面的先。
        assert!(out[0].range.start > out[1].range.start);
        assert_eq!(apply_byte_edits("aaa bbb ccc", &out), "X bbb Z");
    }

    #[test]
    fn apply_edits_tolerates_stale_ranges() {
        // 服务器基于旧文本给的区间可能越界（文本被删短了）。
        let edits = vec![ByteEdit {
            range: 5..100,
            new_text: "".into(),
        }];
        assert_eq!(apply_byte_edits("hello", &edits), "hello");
    }

    /// 回归：多字节文本不能让回退 panic。
    ///
    /// 曾经按字节回退并调 `Rope::char`，落在多字节字符内部会 panic——
    /// 打开含中文注释的文件、光标停在注释后即崩溃。
    #[test]
    fn word_range_multibyte_does_not_panic() {
        let text = rope("中文abc");
        // 光标在末尾：CJK 也是 alphanumeric，所以整段 `中文abc` 是一个词。
        assert_eq!(word_range_before(&text, text.len()), 0..9);
        // 光标落在多字节字符**中间**：必须安全返回，不能 panic。
        let r = word_range_before(&text, 1);
        assert!(r.start <= r.end);
        assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
    }

    #[test]
    fn word_range_after_cjk_word() {
        // CJK 本身也是 alphanumeric，所以光标在 CJK 后应把整段 CJK 取进来。
        let text = rope("变量 x");
        assert_eq!(word_range_before(&text, text.len()), text.len() - 1..text.len());
    }

    #[test]
    fn word_range_offset_past_end_is_clamped() {
        let text = rope("ab");
        // 越界偏移：钳制到文本长度，不 panic。
        let r = word_range_before(&text, 999);
        assert_eq!(r, 0..2);
    }

    #[test]
    fn word_range_stops_at_punctuation() {
        let text = rope("foo.bar baz");
        // 点在偏移 3：光标在 8（"ba" 之后）时应取 "baz"? 这里 offset=8
        // 处前面是空格，取空区间。
        assert_eq!(word_range_before(&text, 8), 8..8);
        // offset=6 → 前面是 "ba"？
        let text2 = rope("ab cd");
        assert_eq!(word_range_before(&text2, 5), 3..5);
        // 点号不被当作词字符：foo.|bar 取空。
        let text3 = rope("foo.bar");
        assert_eq!(word_range_before(&text3, 4), 4..4);
    }

    #[test]
    fn completion_prefers_text_edit_range() {
        let item = lsp_types::CompletionItem {
            label: "println".into(),
            text_edit: Some(lsp_types::CompletionTextEdit::Edit(TextEdit {
                range: range(0, 0, 0, 3),
                new_text: "println!".into(),
            })),
            ..Default::default()
        };
        let text = rope("pri");
        assert_eq!(completion_replace_range(&text, 3, &item), 0..3);
        assert_eq!(completion_insert_text(&item), "println!");
        assert_eq!(completion_label(&item), "println");
    }

    #[test]
    fn completion_falls_back_to_word_range() {
        let item = lsp_types::CompletionItem {
            label: "foo".into(),
            ..Default::default()
        };
        let text = rope("ab cd");
        assert_eq!(completion_replace_range(&text, 5, &item), 3..5);
    }

    #[test]
    fn hover_plaintext_content() {
        let hover = lsp_types::Hover {
            contents: lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(
                "docs".into(),
            )),
            range: Some(range(0, 0, 0, 3)),
        };
        assert_eq!(hover_contents(&hover), "docs");
        assert_eq!(hover_range(&rope("abc"), &hover), Some(0..3));
    }

    #[test]
    fn hover_language_string_becomes_code_block() {
        let hover = lsp_types::Hover {
            contents: lsp_types::HoverContents::Scalar(
                lsp_types::MarkedString::LanguageString(lsp_types::LanguageString {
                    language: "rust".into(),
                    value: "fn f()".into(),
                }),
            ),
            range: None,
        };
        let text = hover_contents(&hover);
        assert!(text.starts_with("```rust"), "got {text:?}");
        assert!(text.contains("fn f()"));
    }

    #[test]
    fn semantic_tokens_decode_resets_column_on_new_line() {
        // 两个 token：第 0 行第 5 列长 3，第 2 行第 1 列长 4。
        let data = vec![0u32, 5, 3, 1, 0, 2, 1, 4, 2, 0];
        let out = decode_semantic_tokens(&data);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], (0, 5, 3, 1));
        // 换行后 delta_start 是绝对列，不是累加。
        assert_eq!(out[1], (2, 1, 4, 2));
    }

    #[test]
    fn semantic_tokens_same_line_accumulates_column() {
        let data = vec![0u32, 2, 1, 0, 0, 0, 3, 2, 0, 0];
        let out = decode_semantic_tokens(&data);
        assert_eq!(out[1], (0, 5, 2, 0));
    }

    #[test]
    fn symbols_flatten_with_depth() {
        let rope = rope("fn a() {}\nfn b() {}");
        let nested = lsp_types::DocumentSymbol {
            name: "outer".into(),
            detail: None,
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range: range(0, 0, 1, 9),
            selection_range: range(0, 3, 0, 4),
            children: Some(vec![lsp_types::DocumentSymbol {
                name: "inner".into(),
                detail: None,
                kind: lsp_types::SymbolKind::VARIABLE,
                tags: None,
                deprecated: None,
                range: range(0, 9, 0, 10),
                selection_range: range(0, 9, 0, 10),
                children: None,
            }]),
        };
        let response = lsp_types::DocumentSymbolResponse::Nested(vec![nested]);
        let flat = flatten_symbols(&rope, &response);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].name, "outer");
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[1].name, "inner");
        assert_eq!(flat[1].depth, 1);
        // 跳转位置用 selection_range（标识符本身），不是整个函数体。
        assert_eq!(flat[0].range, 3..4);
    }

    #[test]
    fn fold_ranges_drop_single_line() {
        let ranges = vec![lsp_types::FoldingRange {
            start_line: 3,
            end_line: 3,
            ..Default::default()
        }];
        assert!(to_fold_ranges(ranges).is_empty());
    }

    #[test]
    fn fold_ranges_keep_valid() {
        let ranges = vec![lsp_types::FoldingRange {
            start_line: 1,
            end_line: 5,
            ..Default::default()
        }];
        let out = to_fold_ranges(ranges);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].start_line, out[0].end_line), (1, 5));
    }

    #[test]
    fn inlay_hints_convert_and_drop_empty() {
        let rope = rope("let x = 1;");
        let hints = vec![
            lsp_types::InlayHint {
                position: pos(0, 4),
                label: lsp_types::InlayHintLabel::String(": i32".into()),
                kind: None,
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: None,
                data: None,
            },
            lsp_types::InlayHint {
                position: pos(0, 0),
                label: lsp_types::InlayHintLabel::String(String::new()),
                kind: None,
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            },
        ];
        let out = to_inlay_hints(&rope, hints);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, ": i32");
        assert!(out[0].after);
        assert_eq!(out[0].offset, 4);
    }

    #[test]
    fn signature_marks_active_parameter() {
        let help = lsp_types::SignatureHelp {
            signatures: vec![lsp_types::SignatureInformation {
                label: "f(a: i32, b: i32)".into(),
                documentation: None,
                parameters: Some(vec![
                    lsp_types::ParameterInformation {
                        label: lsp_types::ParameterLabel::Simple("a: i32".into()),
                        documentation: None,
                    },
                    lsp_types::ParameterInformation {
                        label: lsp_types::ParameterLabel::Simple("b: i32".into()),
                        documentation: None,
                    },
                ]),
                active_parameter: None,
            }],
            active_signature: None,
            active_parameter: Some(1),
        };
        let text = signature_text(&help);
        assert!(text.starts_with("f(a: i32, b: i32)"));
        assert!(text.contains("> b: i32"));
    }

    #[test]
    fn affected_uris_deduplicates() {
        let mut changes = HashMap::new();
        let uri = Url::parse("file:///a.rs").unwrap();
        changes.insert(uri.clone(), vec![]);
        changes.insert(uri.clone(), vec![]);
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        // HashMap 的键天然唯一，这里验证的是「重复插入不会留下两份」。
        assert_eq!(affected_uris(&edit).len(), 1);
    }

    // ------------------------------------------------ WorkspaceEdit 两种形态

    fn uri(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn text_edit(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> TextEdit {
        TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(sl, sc),
                lsp_types::Position::new(el, ec),
            ),
            new_text: text.to_string(),
        }
    }

    /// `changes` 形态：能取到编辑。
    #[test]
    fn workspace_edit_reads_changes_shape() {
        let mut changes = HashMap::new();
        changes.insert(uri("file:///a.rs"), vec![text_edit(0, 0, 0, 3, "foo")]);
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        let edits = edits_for_uri(&edit, &uri("file:///a.rs")).expect("changes shape must work");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "foo");
    }

    /// `documentChanges`（`TextDocumentEdit[]`）形态：也必须能取到编辑。
    ///
    /// 回归：早期只读 `changes`，只发 `documentChanges` 的服务器
    /// （rust-analyzer 重命名、部分格式化）编辑被静默丢弃。
    #[test]
    fn workspace_edit_reads_document_changes_edits_shape() {
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp_types::DocumentChanges::Edits(vec![
                lsp_types::TextDocumentEdit {
                    text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                        uri: uri("file:///a.rs"),
                        version: Some(7),
                    },
                    edits: vec![lsp_types::OneOf::Left(text_edit(0, 0, 0, 3, "bar"))],
                },
            ])),
            change_annotations: None,
        };
        let edits =
            edits_for_uri(&edit, &uri("file:///a.rs")).expect("documentChanges must work");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "bar");
    }

    /// `documentChanges`（`DocumentChangeOperation[]`）形态：同样能取到编辑。
    #[test]
    fn workspace_edit_reads_document_changes_operations_shape() {
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp_types::DocumentChanges::Operations(vec![
                lsp_types::DocumentChangeOperation::Edit(lsp_types::TextDocumentEdit {
                    text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                        uri: uri("file:///a.rs"),
                        version: None,
                    },
                    edits: vec![lsp_types::OneOf::Right(lsp_types::AnnotatedTextEdit {
                        text_edit: text_edit(1, 0, 1, 2, "baz"),
                        annotation_id: "ann-1".to_string(),
                    })],
                }),
            ])),
            change_annotations: None,
        };
        let edits =
            edits_for_uri(&edit, &uri("file:///a.rs")).expect("operations shape must work");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "baz");
    }

    /// 只返回**目标 uri** 的编辑，别的文件不掺进来。
    #[test]
    fn workspace_edit_filters_by_uri() {
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp_types::DocumentChanges::Edits(vec![
                lsp_types::TextDocumentEdit {
                    text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                        uri: uri("file:///other.rs"),
                        version: None,
                    },
                    edits: vec![lsp_types::OneOf::Left(text_edit(0, 0, 0, 1, "other"))],
                },
                lsp_types::TextDocumentEdit {
                    text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                        uri: uri("file:///a.rs"),
                        version: None,
                    },
                    edits: vec![lsp_types::OneOf::Left(text_edit(0, 0, 0, 1, "mine"))],
                },
            ])),
            change_annotations: None,
        };
        let edits = edits_for_uri(&edit, &uri("file:///a.rs")).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "mine", "must not leak other files' edits");
    }

    /// 目标 uri 没有任何编辑 → `None`（调用方据此跳过应用）。
    #[test]
    fn workspace_edit_absent_uri_is_none() {
        let edit = WorkspaceEdit {
            changes: Some(HashMap::new()),
            document_changes: None,
            change_annotations: None,
        };
        assert!(edits_for_uri(&edit, &uri("file:///a.rs")).is_none());
    }

    /// 两种形态同时出现：都要收进来（规范允许，合并处理）。
    #[test]
    fn workspace_edit_merges_both_shapes() {
        let mut changes = HashMap::new();
        changes.insert(uri("file:///a.rs"), vec![text_edit(0, 0, 0, 1, "from-changes")]);
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: Some(lsp_types::DocumentChanges::Edits(vec![
                lsp_types::TextDocumentEdit {
                    text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                        uri: uri("file:///a.rs"),
                        version: None,
                    },
                    edits: vec![lsp_types::OneOf::Left(text_edit(1, 0, 1, 1, "from-doc-changes"))],
                },
            ])),
            change_annotations: None,
        };
        let edits = edits_for_uri(&edit, &uri("file:///a.rs")).unwrap();
        assert_eq!(edits.len(), 2, "both shapes must contribute");
        let texts: Vec<&str> = edits.iter().map(|e| e.new_text.as_str()).collect();
        assert!(texts.contains(&"from-changes"));
        assert!(texts.contains(&"from-doc-changes"));
    }

    /// 端到端：把 `documentChanges` 形态的编辑真正打到文本上。
    #[test]
    fn document_changes_edit_reaches_buffer_text() {
        let text = rope("fn main() { old_name(); }\n");
        // 替换 `old_name`（字节 12..20）。
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp_types::DocumentChanges::Edits(vec![
                lsp_types::TextDocumentEdit {
                    text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                        uri: uri("file:///a.rs"),
                        version: None,
                    },
                    edits: vec![lsp_types::OneOf::Left(TextEdit {
                        range: lsp_types::Range::new(
                            lsp_types::Position::new(0, 12),
                            lsp_types::Position::new(0, 20),
                        ),
                        new_text: "new_name".to_string(),
                    })],
                },
            ])),
            change_annotations: None,
        };

        let edits = edits_for_uri(&edit, &uri("file:///a.rs"))
            .expect("documentChanges must yield edits");
        let byte_edits = to_byte_edits(&text, edits);
        let out = apply_byte_edits(&text.to_string(), &byte_edits);
        assert_eq!(out, "fn main() { new_name(); }\n");
    }

    // ------------------------------------------------ 字符分类（对照 Zed CharKind）

    /// `.` 默认是**标点**，不是词字符。
    ///
    /// 回归：补全触发条件曾硬编码 `c == '.'` 算词字符，于是 Rust 里打完
    /// `foo.` 也会触发补全、且替换区间多吞一个点——「输入语法匹配不准确」。
    /// Zed 只在语言作用域声明了 `completion_query_characters` 时才把 `.`
    /// 当词字符（如 import 路径 `foo.ts`）。
    #[test]
    fn dot_is_punctuation_by_default() {
        assert_eq!(char_kind('.', DEFAULT_EXTRA_WORD_CHARS), CharKind::Punctuation);
        // 显式声明后才是词字符（对应 import 路径场景）。
        assert_eq!(char_kind('.', &['.']), CharKind::Word);
    }

    /// 字母、数字、下划线是词字符。
    #[test]
    fn alphanumeric_and_underscore_are_word_chars() {
        for c in ['a', 'Z', '0', '9', '_', '中', 'é'] {
            assert_eq!(
                char_kind(c, DEFAULT_EXTRA_WORD_CHARS),
                CharKind::Word,
                "{c:?} should be a word char"
            );
        }
    }

    /// 空白与其它标点的分类。
    #[test]
    fn whitespace_and_punctuation_are_classified() {
        for c in [' ', '\t', '\n'] {
            assert_eq!(char_kind(c, DEFAULT_EXTRA_WORD_CHARS), CharKind::Whitespace);
        }
        for c in ['-', '(', ')', '{', '}', ':', ',', ';', '+', '"', '\''] {
            assert_eq!(
                char_kind(c, DEFAULT_EXTRA_WORD_CHARS),
                CharKind::Punctuation,
                "{c:?} should be punctuation"
            );
        }
    }

    /// `-` 在 Tailwind 场景（显式声明）下是词字符。
    #[test]
    fn hyphen_is_word_char_when_scope_declares_it() {
        assert_eq!(char_kind('-', DEFAULT_EXTRA_WORD_CHARS), CharKind::Punctuation);
        assert_eq!(char_kind('-', &['-']), CharKind::Word);
    }

    /// 替换区间：`foo.` 后不该把点吞进去（Rust 成员访问）。
    #[test]
    fn word_range_does_not_swallow_dot() {
        let text = rope("foo.");
        // 光标在末尾：`.` 是标点 → 替换区间为空（服务器会给 textEdit 指定范围）。
        assert_eq!(word_range_before(&text, 4), 4..4);
        // 光标在 `foo` 之后：取到 `foo`。
        assert_eq!(word_range_before(&text, 3), 0..3);
    }

    /// 带额外词字符时，`.` 会被并入词（import 路径场景）。
    #[test]
    fn word_range_includes_dot_with_extra_chars() {
        let text = rope("./foo.ts");
        // 显式声明 `.` 为词字符：`foo.ts` 整体算一个词（前导 `/` 仍是标点，
        // 因为路径分隔符不属于标识符）。
        assert_eq!(word_range_before_with(&text, text.len(), &['.']), 2..8);
        // 不声明 `.` 时只取 `ts`（点当标点）。
        assert_eq!(word_range_before(&text, text.len()), 6..8);
    }

    /// 普通标识符照常取全。
    #[test]
    fn word_range_takes_whole_identifier() {
        let text = rope("let value = 1;");
        assert_eq!(word_range_before(&text, 9), 4..9);
    }
}
