//! 表格结构操作（Typora 右键菜单/工具栏语义）：增删行列、移动行列、对齐。
//!
//! 全部是纯函数（`EditorModel` + 行文本），可单测、不依赖 GPUI。约定：
//! - 表格块 = 连续的 `Block::Table` 行；分隔行是**结构**而不是数据行，
//!   所以"第 N 行"计数会跳过它（与 Typora 一致：表头是真实行，分隔行不是）。
//! - 改动落在 `EditorModel` 上，因此撤销/重做、`Change` 事件、搜索面板同步
//!   都沿用既有事务机制，无需额外接线。
//! - 未编辑过的表格保持字节原样：这些函数只在用户显式操作时才重写受影响的
//!   行，不做全局 prettify（Typora 同款）。

use crate::editor::markdown_live::block::{
    is_delimiter_cell, split_table_row, table_cell_spans,
};
use crate::editor::markdown_live::model::{EditorModel, Pos};

/// 表格块的范围：`(起始行, 结束行)`，闭区间。`None` = 该行不是表格。
pub fn table_block_range(lines: &[String], line: usize) -> Option<(usize, usize)> {
    if line >= lines.len() || split_table_row(&lines[line]).is_empty() {
        return None;
    }
    // 往上/下扩到连续的可解析为表格行的范围（分隔行也算）。
    let is_row = |i: usize| -> bool {
        lines
            .get(i)
            .is_some_and(|l| l.trim_start().starts_with('|') || l.contains('|'))
    };
    if !is_row(line) {
        return None;
    }
    let mut start = line;
    while start > 0 && is_row(start - 1) {
        start -= 1;
    }
    let mut end = line;
    while end + 1 < lines.len() && is_row(end + 1) {
        end += 1;
    }
    // 至少要有一个分隔行才算表格。
    if !(start..=end).any(|i| is_delimiter_row(&lines[i])) {
        return None;
    }
    Some((start, end))
}

/// 该行是否是分隔行（`|---|---|`）。
pub fn is_delimiter_row(line: &str) -> bool {
    let cells = split_table_row(line);
    !cells.is_empty() && cells.iter().all(|c| is_delimiter_cell(c))
}

/// 分隔行在块内的行号（表头行 + 1 的常规形态；找不到返回 `None`）。
pub fn delimiter_row(lines: &[String], block: (usize, usize)) -> Option<usize> {
    (block.0..=block.1).find(|i| is_delimiter_row(&lines[*i]))
}

/// 数据行（含表头，跳过其他分隔行）的行号列表。
pub fn table_data_rows(lines: &[String], block: (usize, usize)) -> Vec<usize> {
    (block.0..=block.1)
        .filter(|i| !is_delimiter_row(&lines[*i]))
        .collect()
}

/// 列数（以分隔行为准；没有分隔行时用第一行）。
pub fn table_col_count(lines: &[String], block: (usize, usize)) -> usize {
    let probe = delimiter_row(lines, block).unwrap_or(block.0);
    table_cell_spans(lines.get(probe).map(String::as_str).unwrap_or("")).len()
}




/// 生成分隔行（`align` 与 Typora 的生成规则一致：最小宽度 4）。
pub fn delimiter_for(cols: usize, aligns: &[Align]) -> String {
    let cells: Vec<String> = (0..cols)
        .map(|i| {
            let a = aligns.get(i).copied().unwrap_or(Align::None);
            match a {
                Align::None => "----".to_string(),
                Align::Left => ":---".to_string(),
                Align::Center => ":--:".to_string(),
                Align::Right => "---:".to_string(),
            }
        })
        .collect();
    format!("| {} |", cells.join(" | "))
}

/// 列对齐（分隔行里的冒号形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

impl Align {
    /// 从分隔单元格文本解析（`:-:` 等）。
    pub fn from_cell(cell: &str) -> Self {
        let c = cell.trim();
        match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => Align::Center,
            (true, false) => Align::Left,
            (false, true) => Align::Right,
            _ => Align::None,
        }
    }

    /// Typora 的切换语义：点同一个对齐按钮两次会回到默认（无对齐）。
    pub fn toggled(self, target: Align) -> Align {
        if self == target { Align::None } else { target }
    }
}

/// 读当前各列对齐。
pub fn table_aligns(lines: &[String], block: (usize, usize)) -> Vec<Align> {
    let Some(d) = delimiter_row(lines, block) else {
        return vec![Align::None; table_col_count(lines, block)];
    };
    split_table_row(&lines[d])
        .iter()
        .map(|c| Align::from_cell(c))
        .collect()
}

/// 光标所在列（`None` = 该行不是表格行）。
pub fn column_at(lines: &[String], line: usize, col: usize) -> Option<usize> {
    let text = lines.get(line)?;
    let spans = table_cell_spans(text);
    let byte = byte_for_col(text, col);
    spans
        .iter()
        .position(|(s, e)| byte >= *s && byte <= *e)
        .or_else(|| spans.iter().position(|(s, _)| *s >= byte))
        .or(Some(spans.len().saturating_sub(1)))
}

/// char 列 → 字节（与 `layout::byte_for_col` 同义，这里避免跨模块依赖）。
fn byte_for_col(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

// ---------------------------------------------------------------------------
// 操作
// ---------------------------------------------------------------------------

/// 在当前行上方/下方插入一行（新行为空，列数对齐）。
pub fn insert_row(model: &mut EditorModel, above: bool) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let cols = table_col_count(&lines, block);
    if cols == 0 {
        return false;
    }
    // 插入位置：`above` 时在**上一行行尾**断行（新行落在光标行之前），
    // 否则在光标行行尾断行（新行落在光标行之后）。
    let fresh = format!("|{}", "  |".repeat(cols));
    if above {
        if cur.line == 0 {
            model.move_to(0, 0, false);
        } else {
            model.move_to(cur.line - 1, usize::MAX, false);
        }
    } else {
        model.move_to(cur.line, usize::MAX, false);
    }
    model.newline();
    model.insert(&fresh);
    let line = if above { cur.line } else { cur.line + 1 };
    let text = model.line(line).map(str::to_string).unwrap_or_default();
    let (cs, _) = table_cell_spans(&text).first().copied().unwrap_or((0, 0));
    model.move_to(line, char_col(&text, cs + 1), false);
    true
}

/// 删除当前行（表头/分隔行不可删——Typora 的 Delete Row 在表头时隐藏）。
pub fn delete_row(model: &mut EditorModel) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    if is_delimiter_row(&lines[cur.line]) {
        return false;
    }
    // 表头不可删（Typora 同款）。
    if table_data_rows(&lines, block).first().copied() == Some(cur.line) {
        return false;
    }
    // 选中「本行 + 行尾换行」，一次删干净（否则会留下空行）。
    let end_col = lines[cur.line].chars().count();
    model.move_to(cur.line, 0, false);
    model.move_to(cur.line + 1, 0, true);
    model.delete_selection();
    let _ = end_col;
    model.move_to(
        cur.line.min(model.line_count().saturating_sub(1)),
        0,
        false,
    );
    true
}

/// 交换 `line` 中两个单元格的**内容**（保留各格原有 padding 与管道间距）。
fn swap_cells(line: &str, a: usize, b: usize) -> String {
    let spans = table_cell_spans(line);
    let (Some((sa, ea)), Some((sb, eb))) = (spans.get(a).copied(), spans.get(b).copied()) else {
        return line.to_string();
    };
    // 把 `from` 的内容填进 `to` 的槽位，沿用 `to` 自己的空白。
    let fill = |to: (usize, usize), from: (usize, usize)| -> String {
        let slot = &line[to.0..to.1];
        let lead = slot.len() - slot.trim_start().len();
        let trail = slot.len() - slot.trim_end().len();
        let body = line[from.0..from.1].trim();
        let pad_r = if trail == 0 { 1 } else { trail };
        format!("{}{}{}", " ".repeat(lead.max(1)), body, " ".repeat(pad_r))
    };
    let mut out = String::with_capacity(line.len());
    if sa < sb {
        out.push_str(&line[..sa]);
        out.push_str(&fill((sa, ea), (sb, eb)));
        out.push_str(&line[ea..sb]);
        out.push_str(&fill((sb, eb), (sa, ea)));
        out.push_str(&line[eb..]);
    } else {
        out.push_str(&line[..sb]);
        out.push_str(&fill((sb, eb), (sa, ea)));
        out.push_str(&line[eb..sa]);
        out.push_str(&fill((sa, ea), (sb, eb)));
        out.push_str(&line[ea..]);
    }
    out
}

/// 在 `line` 的第 `col` 个单元格前插入 `text`（**只改这一处字节**，其余
/// 部分原样保留——包括各格 padding 与管道间距，满足 Typora「未编辑部分
/// 不动」的语义）。
fn splice_cell_insert(line: &str, col: usize, text: &str) -> String {
    let spans = table_cell_spans(line);
    let Some((s, _e)) = spans.get(col).copied() else {
        // 追加到行尾（补一个单元格）。
        let t = line.trim_end();
        let t = t.strip_suffix('|').unwrap_or(t);
        return format!("{t} | {text} |");
    };
    // 在第 `col` 格的**前一个管道之后**插入 ` {text} |`，保持管道间距
    //（`| a | b |` 在第 1 格前插 X → `| X | a | b |`）。
    let head = &line[..s];
    let head = head.strip_suffix('|').unwrap_or(head);
    let tail = &line[s..];
    format!("{head}| {text} |{tail}")
}

/// 删除 `line` 的第 `col` 个单元格（含其右侧的管道分隔，保留其余字节）。
fn splice_cell_remove(line: &str, col: usize) -> String {
    let spans = table_cell_spans(line);
    if spans.len() <= 1 {
        return line.to_string();
    }
    let Some((s, e)) = spans.get(col).copied() else {
        return line.to_string();
    };
    // 连同「该格 + 右侧一个管道」一起删：`| a | b | c |` 删 b → `| a | c |`。
    // 末尾格则连左侧管道一起删（`| a | b |` 删 b → `| a |`）。
    // 直接切掉中间那段（含管道），两侧字节原样保留 —— 不做 trim，避免
    // 破坏用户表格的对齐空格。
    let is_last = col + 1 >= spans.len();
    let (from, to) = if is_last {
        let prev_pipe = line[..s].rfind('|').unwrap_or(s);
        (prev_pipe, e)
    } else {
        let after_pipe = line[e..].find('|').map(|i| e + i + 1).unwrap_or(e);
        (s, after_pipe)
    };
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..from]);
    out.push_str(&line[to..]);
    // 接缝可能变成 `||` 或缺空格：统一成 `| `（只在接缝处）。
    let trimmed = out.trim_end();
    if trimmed.ends_with('|') && !out[trimmed.len()..].contains(' ') {
        // 末尾管道后有空格，保持原样。
    }
    if out.contains("||") {
        out = out.replace("||", "| |");
    }
    out
}

/// 在光标列的左侧/右侧插入一列（空单元格）。
pub fn insert_column(model: &mut EditorModel, before: bool) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let cols = table_col_count(&lines, block);
    let Some(col) = column_at(&lines, cur.line, cur.col) else {
        return false;
    };
    let at = if before { col } else { col + 1 };
    let mut edits: Vec<(usize, String)> = Vec::new();
    for i in block.0..=block.1 {
        let line = &lines[i];
        // 分隔行用 `----`（Typora 的最小宽度），数据/表头行为空。
        let fill = if is_delimiter_row(line) { "----" } else { "" };
        // `at == cols` 表示追加到末尾；否则插在第 `at` 格之前。
        let new_line = if at >= cols {
            let t = line.trim_end();
            let t = t.strip_suffix('|').unwrap_or(t);
            format!("{} | {} |", t.trim_end(), fill)
        } else {
            splice_cell_insert(line, at, fill)
        };
        edits.push((i, new_line));
    }
    apply_row_edits(model, &lines, &edits);
    // 光标落到新列的第一个数据格。
    let after: Vec<String> = model.lines().to_vec();
    let rows = table_block_range(&after, cur.line)
        .map(|b| table_data_rows(&after, b))
        .unwrap_or_default();
    let first = rows
        .get(1)
        .copied()
        .or_else(|| rows.first().copied())
        .unwrap_or(cur.line);
    let text = model.line(first).map(str::to_string).unwrap_or_default();
    let spans = table_cell_spans(&text);
    if let Some((s, e)) = spans.get(at).copied() {
        let raw = &text[s..e];
        let lead = raw.len() - raw.trim_start().len();
        model.move_to(first, char_col(&text, s + lead), false);
    }
    true
}

/// 删除光标所在列。最后一列不可删（Typora 同款）。
pub fn delete_column(model: &mut EditorModel) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let cols = table_col_count(&lines, block);
    if cols <= 1 {
        return false;
    }
    let Some(col) = column_at(&lines, cur.line, cur.col) else {
        return false;
    };
    let edits: Vec<(usize, String)> = (block.0..=block.1)
        .map(|i| (i, splice_cell_remove(&lines[i], col)))
        .collect();
    apply_row_edits(model, &lines, &edits);
    let text = model.line(cur.line).map(str::to_string).unwrap_or_default();
    let spans = table_cell_spans(&text);
    let target = col.min(spans.len().saturating_sub(1));
    if let Some((s, e)) = spans.get(target).copied() {
        let raw = &text[s..e];
        let lead = raw.len() - raw.trim_start().len();
        model.move_to(cur.line, char_col(&text, s + lead), false);
    }
    true
}

/// 设置光标列的对齐（`target` 为 `None` 时清除对齐）。
pub fn set_column_align(model: &mut EditorModel, target: Option<Align>) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let Some(d) = delimiter_row(&lines, block) else {
        return false;
    };
    let Some(col) = column_at(&lines, cur.line, cur.col) else {
        return false;
    };
    let mut aligns = table_aligns(&lines, block);
    if col >= aligns.len() {
        return false;
    }
    aligns[col] = match target {
        Some(t) => aligns[col].toggled(t),
        None => Align::None,
    };
    let cols = table_col_count(&lines, block);
    let new_delim = delimiter_for(cols, &aligns);
    let edits = vec![(d, new_delim)];
    apply_row_edits(model, &lines, &edits);
    true
}

/// 把当前行/列上移、下移、左移、右移。
pub fn move_row(model: &mut EditorModel, up: bool) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let rows = table_data_rows(&lines, block);
    let Some(pos) = rows.iter().position(|l| *l == cur.line) else {
        return false;
    };
    // 表头不可移动（Typora 的 Move Row 对表头无效）。
    if pos == 0 {
        return false;
    }
    let other = if up {
        if pos == 1 {
            return false;
        }
        rows[pos - 1]
    } else {
        match rows.get(pos + 1) {
            Some(l) => *l,
            None => return false,
        }
    };
    // 与相邻数据行交换文本。
    let edits = vec![(cur.line, lines[other].clone()), (other, lines[cur.line].clone())];
    apply_row_edits(model, &lines, &edits);
    model.move_to(other, cur.col, false);
    true
}

/// 列移动：把整列内容与相邻列交换（分隔行的对齐也跟着换）。
pub fn move_column(model: &mut EditorModel, left: bool) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let cols = table_col_count(&lines, block);
    let Some(col) = column_at(&lines, cur.line, cur.col) else {
        return false;
    };
    if (left && col == 0) || (!left && col + 1 >= cols) {
        return false;
    }
    let other = if left { col - 1 } else { col + 1 };
    let edits: Vec<(usize, String)> = (block.0..=block.1)
        .map(|i| (i, swap_cells(&lines[i], col, other)))
        .collect();
    apply_row_edits(model, &lines, &edits);
    // 光标跟到新列位置。
    let text = model.line(cur.line).map(str::to_string).unwrap_or_default();
    let spans = table_cell_spans(&text);
    if let Some((s, e)) = spans.get(other).copied() {
        let raw = &text[s..e];
        let lead = raw.len() - raw.trim_start().len();
        model.move_to(cur.line, char_col(&text, s + lead), false);
    }
    true
}

/// 删除整个表格块（连同块后那个空行，避免留下双空行）。
pub fn delete_table(model: &mut EditorModel) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    // 选中「块首行 .. 块末行 + 换行」，一次删干净。
    model.move_to(block.0, 0, false);
    model.move_to(block.1 + 1, 0, true);
    model.delete_selection();
    model.move_to(
        block.0.min(model.line_count().saturating_sub(1)),
        0,
        false,
    );
    true
}

/// 计算 `block` 的重排行改写（数据行含表头 + 分隔行），纯函数、不动 model。
/// 列宽 = 各列最宽内容（CJK 算 2 列），上限 60、下限 4（Typora 同款）。
pub fn prettify_edits(lines: &[String], block: (usize, usize)) -> Option<Vec<(usize, String)>> {
    let cols = table_col_count(lines, block);
    if cols == 0 {
        return None;
    }
    let aligns = table_aligns(lines, block);
    // 显示宽度（CJK 算 2 列），上限 60（Typora 同款）。
    let width = |s: &str| -> usize {
        s.chars()
            .map(|c| if (c as u32) >= 0x1100 { 2 } else { 1 })
            .sum()
    };
    let mut widths = vec![4usize; cols];
    let rows = table_data_rows(lines, block);
    for &i in &rows {
        for (c, cell) in split_table_row(&lines[i]).iter().enumerate().take(cols) {
            widths[c] = widths[c].max(width(cell).min(60));
        }
    }
    let pad = |s: &str, w: usize, a: Align| -> String {
        let sw = width(s);
        let fill = w.saturating_sub(sw);
        match a {
            Align::Right => format!("{}{}", " ".repeat(fill), s),
            Align::Center => {
                let l = fill / 2;
                format!("{}{}{}", " ".repeat(l), s, " ".repeat(fill - l))
            }
            _ => format!("{}{}", s, " ".repeat(fill)),
        }
    };
    let mut edits: Vec<(usize, String)> = Vec::new();
    for &i in &rows {
        let cells = split_table_row(&lines[i]);
        let out: Vec<String> = (0..cols)
            .map(|c| pad(cells.get(c).map(String::as_str).unwrap_or(""), widths[c], aligns[c]))
            .collect();
        edits.push((i, format!("| {} |", out.join(" | "))));
    }
    if let Some(d) = delimiter_row(lines, block) {
        let cells: Vec<String> = (0..cols)
            .map(|c| {
                let a = aligns[c];
                let w = widths[c].max(4);
                match a {
                    Align::None => "-".repeat(w),
                    Align::Left => format!(":{}", "-".repeat(w - 1)),
                    Align::Center => format!(":{}:", "-".repeat(w.saturating_sub(2))),
                    Align::Right => format!("{}:", "-".repeat(w - 1)),
                }
            })
            .collect();
        edits.push((d, format!("| {} |", cells.join(" | "))));
    }
    Some(edits)
}

/// 重排整张表的格式（右键菜单「重排」= Typora 的 "Prettify Source Code"）。
/// 编辑后的**自动**重排（spec #13）在视图层走 [`prettify_edits`] +
/// `replace_rows_and_move`（带光标重映射与撤销合并）。
pub fn prettify_table(model: &mut EditorModel) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let Some(edits) = prettify_edits(&lines, block) else {
        return false;
    };
    apply_row_edits(model, &lines, &edits);
    true
}

/// Typora 的表格粘贴：把含 `|` 的文本按行列写进表格（从光标格开始），
/// 不够的行/列自动补齐。不含 `|` 时返回 `false`（调用方走普通插入）。
pub fn paste_into_table(model: &mut EditorModel, text: &str) -> bool {
    if !text.contains('|') {
        return false;
    }
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let Some((row, col, _)) = crate::editor::markdown_live::interactions::table_cell_at(
        &lines,
        &|l| table_block_range(&lines, l).is_some(),
        cur.line,
        cur.col,
    ) else {
        return false;
    };
    // 解析粘贴内容：跳过纯分隔行，去掉首尾管道。
    let is_delim = |s: &str| {
        let t = s.trim();
        !t.is_empty()
            && t.contains('-')
            && t.chars().all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
    };
    let pasted: Vec<Vec<String>> = text
        .lines()
        .filter(|l| !is_delim(l))
        .map(|l| {
            let t = l.trim();
            let t = t.strip_prefix('|').unwrap_or(t);
            let t = t.strip_suffix('|').unwrap_or(t);
            t.split('|').map(|c| c.trim().to_string()).collect::<Vec<_>>()
        })
        .filter(|r| !r.is_empty())
        .collect();
    if pasted.is_empty() {
        return false;
    }
    let need_rows = row + pasted.len();
    let need_cols = col + pasted.iter().map(Vec::len).max().unwrap_or(0);
    // 补足行数：先在块尾追加空行（用 insert_row 保证分隔行/表头结构正确）。
    let mut guard = 0;
    while table_data_rows(model.lines(), table_block_range(model.lines(), model.cursor().line).unwrap_or(block)).len() < need_rows
        && guard < 128
    {
        if !insert_row(model, false) {
            break;
        }
        guard += 1;
    }
    // 重新取一次（行数已变）。
    let lines = model.lines().to_vec();
    let Some(block) = table_block_range(&lines, cur.line.min(lines.len().saturating_sub(1))) else {
        return false;
    };
    let rows = table_data_rows(&lines, block);
    let cols = table_col_count(&lines, block);
    let mut grid: Vec<Vec<String>> = rows
        .iter()
        .map(|&i| {
            let mut c = split_table_row(&lines[i]);
            c.resize(cols, String::new());
            c
        })
        .collect();
    // 补列。
    if need_cols > cols {
        for r in grid.iter_mut() {
            r.resize(need_cols, String::new());
        }
    }
    // 写入粘贴内容。
    for (dr, cells) in pasted.iter().enumerate() {
        for (dc, cell) in cells.iter().enumerate() {
            if let Some(slot) = grid
                .get_mut(row + dr)
                .and_then(|r| r.get_mut(col + dc))
            {
                *slot = cell.clone();
            }
        }
    }
    // 重建整块（表头 + 数据行）。分隔行只在**列数变化**时重写，否则保留
    // 原样（Typora：未编辑的部分不动）。
    let new_cols = grid.first().map(Vec::len).unwrap_or(cols);
    let mut edits: Vec<(usize, String)> = Vec::new();
    for (i, cells) in grid.iter().enumerate() {
        if let Some(&line) = rows.get(i) {
            edits.push((line, format!("| {} |", cells.join(" | "))));
        }
    }
    if new_cols != cols
        && let Some(d) = delimiter_row(&lines, block)
    {
        let aligns = table_aligns(&lines, block);
        edits.push((d, delimiter_for(new_cols, &aligns)));
    }
    apply_row_edits(model, &lines, &edits);
    // 光标落在粘贴区域右下角。
    let after: Vec<String> = model.lines().to_vec();
    let is_t = |l: usize| table_block_range(&after, l).is_some();
    let last_row = row + pasted.len() - 1;
    let last_col = col + pasted.last().map(Vec::len).unwrap_or(1).saturating_sub(1);
    crate::editor::markdown_live::interactions::table_goto_cell(
        model,
        &after,
        &is_t,
        cur.line,
        last_row,
        last_col,
        true,
        false,
    );
    true
}

/// 把第 `from` 行移动到第 `to` 行位置（拖拽重排用；表头不可动）。
pub fn drag_move_row(model: &mut EditorModel, from: usize, to: usize) -> bool {
    let lines = model.lines().to_vec();
    let Some(block) = table_block_range(&lines, from) else {
        return false;
    };
    let rows = table_data_rows(&lines, block);
    let (Some(fp), Some(tp)) = (
        rows.iter().position(|l| *l == from),
        rows.iter().position(|l| *l == to),
    ) else {
        return false;
    };
    // 表头（索引 0）不可移动，也不能被覆盖。
    if fp == 0 || tp == 0 || fp == tp {
        return false;
    }
    // 逐个相邻交换，直到到位（复用 move_row，保证 undo 粒度一致）。
    let cur = model.cursor();
    let mut pos = fp;
    while pos > tp {
        model.move_to(rows[pos], cur.col, false);
        if !move_row(model, true) {
            break;
        }
        pos -= 1;
    }
    while pos < tp {
        model.move_to(rows[pos], cur.col, false);
        if !move_row(model, false) {
            break;
        }
        pos += 1;
    }
    model.move_to(rows[tp], cur.col, false);
    true
}

/// 把第 `from` 列移动到第 `to` 列位置（拖拽重排用）。
pub fn drag_move_column(model: &mut EditorModel, from: usize, to: usize) -> bool {
    if from == to {
        return false;
    }
    let cur = model.cursor();
    let mut pos = from;
    while pos > to {
        model.move_to(cur.line, 0, false);
        if !move_column_at(model, pos, pos - 1) {
            break;
        }
        pos -= 1;
    }
    while pos < to {
        model.move_to(cur.line, 0, false);
        if !move_column_at(model, pos, pos + 1) {
            break;
        }
        pos += 1;
    }
    true
}

/// 交换任意相邻两列（`move_column` 的按索引版本，供拖拽重排复用）。
fn move_column_at(model: &mut EditorModel, a: usize, b: usize) -> bool {
    let lines = model.lines().to_vec();
    let cur = model.cursor();
    let Some(block) = table_block_range(&lines, cur.line) else {
        return false;
    };
    let cols = table_col_count(&lines, block);
    if a >= cols || b >= cols || a == b {
        return false;
    }
    let edits: Vec<(usize, String)> = (block.0..=block.1)
        .map(|i| (i, swap_cells(&lines[i], a, b)))
        .collect();
    apply_row_edits(model, &lines, &edits);
    true
}

/// 把 `edits`（行号 → 新文本）写回模型。从后往前替换，避免行号漂移。
/// **保持光标不动**（否则连续操作会作用到错误的行列上）。
fn apply_row_edits(model: &mut EditorModel, lines: &[String], edits: &[(usize, String)]) {
    let mut sorted: Vec<(usize, String)> = edits.to_vec();
    sorted.sort_by_key(|(i, _)| *i);
    sorted.dedup_by_key(|(i, _)| *i);
    let saved = model.cursor();
    for (i, new) in sorted.into_iter().rev() {
        let old = lines.get(i).map(String::as_str).unwrap_or("");
        if old == new {
            continue;
        }
        let end_col = old.chars().count();
        model.replace_range(Pos::new(i, 0), Pos::new(i, end_col), &new);
    }
    // 恢复光标（行号可能因增删行而漂移，这里夹到合法范围）。
    let line = saved.line.min(model.line_count().saturating_sub(1));
    model.move_to(line, saved.col, false);
}

/// char 列（`table_cell_spans` 返回字节，这里换算）。
fn char_col(line: &str, byte: usize) -> usize {
    line.get(..byte.min(line.len()))
        .unwrap_or("")
        .chars()
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n\ntail\n";

    fn model_at(line: usize, col: usize) -> EditorModel {
        let mut m = EditorModel::new(DOC);
        m.move_to(line, col, false);
        m
    }

    #[test]
    fn block_range_and_rows_skip_delimiter() {
        let lines: Vec<String> = DOC.split('\n').map(str::to_string).collect();
        let block = table_block_range(&lines, 2).expect("是表格");
        assert_eq!(block, (0, 3));
        assert_eq!(delimiter_row(&lines, block), Some(1));
        assert_eq!(table_data_rows(&lines, block), vec![0, 2, 3]);
        assert_eq!(table_col_count(&lines, block), 3);
        // 非表格行。
        assert!(table_block_range(&lines, 4).is_none());
        assert!(table_block_range(&lines, 5).is_none());
    }

    #[test]
    fn insert_row_below_and_above() {
        let mut m = model_at(2, 2);
        assert!(insert_row(&mut m, false));
        let text = m.text();
        assert_eq!(
            text,
            "| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n|  |  |  |\n| 4 | 5 | 6 |\n\ntail\n",
            "下方插行"
        );
        let mut m = model_at(2, 2);
        assert!(insert_row(&mut m, true));
        assert_eq!(
            m.text(),
            "| a | b | c |\n|---|---|---|\n|  |  |  |\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n\ntail\n",
            "上方插行"
        );
    }

    #[test]
    fn insert_column_left_and_right() {
        let mut m = model_at(2, 2); // 第 1 格（a 列）
        assert!(insert_column(&mut m, true));
        assert_eq!(
            m.text(),
            "|  | a | b | c |\n| ---- |---|---|---|\n|  | 1 | 2 | 3 |\n|  | 4 | 5 | 6 |\n\ntail\n",
            "左插列（分隔行跟着长）"
        );
        let mut m = model_at(2, 6); // 第 2 格（b 列）
        assert!(insert_column(&mut m, false));
        assert_eq!(
            m.text(),
            "| a | b |  | c |\n|---|---| ---- |---|\n| 1 | 2 |  | 3 |\n| 4 | 5 |  | 6 |\n\ntail\n",
            "右插列"
        );
    }

    #[test]
    fn delete_row_refuses_header_and_delimiter() {
        // 表头不可删。
        let mut m = model_at(0, 2);
        assert!(!delete_row(&mut m));
        assert_eq!(m.text(), DOC);
        // 分隔行不可删。
        let mut m = model_at(1, 2);
        assert!(!delete_row(&mut m));
        // 数据行可删。
        let mut m = model_at(2, 2);
        assert!(delete_row(&mut m));
        assert_eq!(
            m.text(),
            "| a | b | c |\n|---|---|---|\n| 4 | 5 | 6 |\n\ntail\n"
        );
    }

    #[test]
    fn delete_column_refuses_last_column() {
        let one_col = "| a |\n|---|\n| 1 |\n";
        let mut m = EditorModel::new(one_col);
        m.move_to(0, 2, false);
        assert!(!delete_column(&mut m));
        assert_eq!(m.text(), one_col);
        // 多列可删。
        let mut m = model_at(2, 6); // b 列
        assert!(delete_column(&mut m));
        assert_eq!(
            m.text(),
            "| a | c |\n|---|---|\n| 1 | 3 |\n| 4 | 6 |\n\ntail\n"
        );
    }

    #[test]
    fn set_column_align_toggles_and_clears() {
        let mut m = model_at(2, 2); // a 列
        assert!(set_column_align(&mut m, Some(Align::Left)));
        assert!(m.text().contains("|:---|---:|---:|") || m.text().contains(":---"));
        let lines: Vec<String> = m.lines().to_vec();
        assert_eq!(table_aligns(&lines, (0, 3))[0], Align::Left);
        // 再点同一个 → 清除。
        assert!(set_column_align(&mut m, Some(Align::Left)));
        let lines: Vec<String> = m.lines().to_vec();
        assert_eq!(table_aligns(&lines, (0, 3))[0], Align::None);
        // 右对齐。
        assert!(set_column_align(&mut m, Some(Align::Right)));
        let lines: Vec<String> = m.lines().to_vec();
        assert_eq!(table_aligns(&lines, (0, 3))[0], Align::Right);
    }

    #[test]
    fn move_row_swaps_data_rows_and_refuses_header() {
        let mut m = model_at(2, 2); // 第 1 条数据行
        assert!(move_row(&mut m, false));
        assert_eq!(
            m.text(),
            "| a | b | c |\n|---|---|---|\n| 4 | 5 | 6 |\n| 1 | 2 | 3 |\n\ntail\n"
        );
        // 表头不可移动。
        let mut m = model_at(0, 2);
        assert!(!move_row(&mut m, false));
        assert!(!move_row(&mut m, true));
    }

    #[test]
    fn move_column_swaps_and_refuses_edges() {
        let mut m = model_at(2, 6); // b 列 → 左移
        assert!(move_column(&mut m, true));
        assert_eq!(
            m.text(),
            "| b | a | c |\n| --- | --- |---|\n| 2 | 1 | 3 |\n| 5 | 4 | 6 |\n\ntail\n"
        );
        // 第一列不能再左移。
        let mut m = model_at(2, 2);
        assert!(!move_column(&mut m, true));
        // 最后一列不能再右移。
        let mut m = model_at(2, 14);
        assert!(!move_column(&mut m, false));
    }

    #[test]
    fn delete_table_removes_block() {
        let mut m = model_at(2, 2);
        assert!(delete_table(&mut m));
        // 表格块整体消失；块后原有的空行保留（不额外吞行）。
        assert_eq!(m.text(), "\ntail\n");
    }

    #[test]
    fn prettify_aligns_columns_without_touching_short_tables() {
        let ragged = "|a|bb|\n|-|-|\n|ccc|d|\n";
        let mut m = EditorModel::new(ragged);
        m.move_to(0, 1, false);
        assert!(prettify_table(&mut m));
        // 各列按最宽内容补齐（CJK 按 2 列宽）。
        assert_eq!(m.text(), "| a    | bb   |\n| ---- | ---- |\n| ccc  | d    |\n");
    }

    #[test]
    fn prettify_is_not_automatic() {
        // model 层：insert 只改字符、绝不动格式——自动重排是**视图层** hook
        //（spec #13，集成见 live_table_auto_prettify_after_typing_spec13）。
        let mut m = EditorModel::new("| a | b |\n|---|---|\n| 1 | 2 |\n");
        m.move_to(2, 2, false);
        m.insert("X");
        assert_eq!(m.text(), "| a | b |\n|---|---|\n| X1 | 2 |\n");
    }

    #[test]
    fn paste_pipe_table_distributes_into_cells() {
        // 从第 1 条数据行第 1 格粘贴 2×2 → 覆盖现有两行两列。
        let mut m = model_at(2, 2);
        assert!(paste_into_table(&mut m, "| x | y |\n| z | w |\n"));
        assert_eq!(
            m.text(),
            "| a | b | c |\n|---|---|---|\n| x | y | 3 |\n| z | w | 6 |\n\ntail\n"
        );
    }

    #[test]
    fn paste_pipe_table_grows_rows_and_columns() {
        // 3×4 粘进 3 列表：自动补列；3 行数据能放下（表头 + 2 条数据行 → 补 1 行）。
        let mut m = model_at(2, 2);
        assert!(paste_into_table(
            &mut m,
            "| 1 | 2 | 3 | 4 |\n| 5 | 6 | 7 | 8 |\n| 9 | a | b | c |\n"
        ));
        let text = m.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "| a | b | c |  |", "表头补到 4 列");
        assert_eq!(lines[2], "| 1 | 2 | 3 | 4 |");
        assert_eq!(lines[3], "| 5 | 6 | 7 | 8 |");
        assert_eq!(lines[4], "| 9 | a | b | c |", "自动补出的第 3 条数据行");
    }

    #[test]
    fn paste_skips_delimiter_lines_and_refuses_plain_text() {
        // 分隔行跳过。
        let mut m = model_at(2, 2);
        assert!(paste_into_table(&mut m, "| p | q |\n|---|---|\n| r | s |\n"));
        assert_eq!(
            m.text(),
            "| a | b | c |\n|---|---|---|\n| p | q | 3 |\n| r | s | 6 |\n\ntail\n"
        );
        // 不含 `|` → 交给普通插入。
        let mut m = model_at(2, 2);
        assert!(!paste_into_table(&mut m, "plain text"));
        assert_eq!(m.text(), DOC);
    }

    #[test]
    fn operations_refuse_non_table_lines() {
        let mut m = EditorModel::new("plain text\n");
        m.move_to(0, 2, false);
        assert!(!insert_row(&mut m, false));
        assert!(!delete_row(&mut m));
        assert!(!insert_column(&mut m, true));
        assert!(!delete_column(&mut m));
        assert!(!delete_table(&mut m));
        assert!(!prettify_table(&mut m));
        assert!(!move_row(&mut m, false));
        assert!(!move_column(&mut m, true));
    }
}
