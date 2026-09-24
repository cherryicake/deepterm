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
    Block, is_delimiter_cell, split_table_row, table_cell_spans,
};
use crate::editor::markdown_live::interactions::cell_content_cols;
use crate::editor::markdown_live::layout::col_for_byte as char_col;
use crate::editor::markdown_live::model::{EditorModel, Line, Pos};
use crate::editor::markdown_live::reveal::classify_document;

/// 表格块的范围：`(起始行, 结束行)`，闭区间。`None` = 该行不是表格。
///
/// 行的归属走**渲染器同一份分类**（`classify_document` 的 `Block::Table`），
/// 再要求块内至少有一个分隔行（与渲染器「没有分隔行就不算表格、按源码行
/// 渲染」一致）。此前这里用「行内出现 `|`」做邻接扩张，会把紧邻表格的、
/// 只是恰好含 `|` 的普通段落行（如 `` 用 `a | b` 表示或运算 ``）也算进块，
/// 于是删列/移列/重排会把它当表格行改写。
pub fn table_block_range(lines: &[Line], line: usize) -> Option<(usize, usize)> {
    if line >= lines.len() {
        return None;
    }
    let classified = classify_document(lines);
    let flags: Vec<bool> = classified
        .blocks
        .iter()
        .map(|b| matches!(b, Block::Table))
        .collect();
    table_block_range_flagged(lines, &flags, line)
}

/// [`table_block_range`] 的免分类版本：`flags[i]` = 第 i 行是否 `Block::Table`
/// （调用方已有分类时用它避免重跑 O(n) 分类；拖拽等高频路径）。
pub fn table_block_range_flagged(
    lines: &[Line],
    flags: &[bool],
    line: usize,
) -> Option<(usize, usize)> {
    if line >= flags.len() || !flags[line] {
        return None;
    }
    let mut start = line;
    while start > 0 && flags[start - 1] {
        start -= 1;
    }
    let mut end = line;
    while end + 1 < flags.len() && flags[end + 1] {
        end += 1;
    }
    // 至少要有一个分隔行才算表格。
    if !(start..=end).any(|i| lines.get(i).is_some_and(|l| is_delimiter_row(l))) {
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
pub fn delimiter_row(lines: &[Line], block: (usize, usize)) -> Option<usize> {
    (block.0..=block.1).find(|i| is_delimiter_row(&lines[*i]))
}

/// 数据行（含表头，跳过其他分隔行）的行号列表。
pub fn table_data_rows(lines: &[Line], block: (usize, usize)) -> Vec<usize> {
    (block.0..=block.1)
        .filter(|i| !is_delimiter_row(&lines[*i]))
        .collect()
}

/// 列数（以分隔行为准；没有分隔行时用第一行）。
pub fn table_col_count(lines: &[Line], block: (usize, usize)) -> usize {
    let probe = delimiter_row(lines, block).unwrap_or(block.0);
    table_cell_spans(lines.get(probe).map(|l| l.as_str()).unwrap_or("")).len()
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
pub fn table_aligns(lines: &[Line], block: (usize, usize)) -> Vec<Align> {
    let Some(d) = delimiter_row(lines, block) else {
        return vec![Align::None; table_col_count(lines, block)];
    };
    split_table_row(&lines[d])
        .iter()
        .map(|c| Align::from_cell(c))
        .collect()
}

/// 光标所在列（`None` = 该行不是表格行）。
pub fn column_at(lines: &[Line], line: usize, col: usize) -> Option<usize> {
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
    // 选中「本行 + 行尾换行」，一次删干净（否则会留下空行）。末行没有
    // 下一行可借（文件末尾无换行时 `line + 1` 会被夹回本行、选区为空），
    // 改为「上一行行尾 .. 本行行尾」——同样含本行前的换行。
    let end_col = lines[cur.line].chars().count();
    if cur.line + 1 < model.line_count() {
        model.move_to(cur.line, 0, false);
        model.move_to(cur.line + 1, 0, true);
    } else {
        let prev_len = lines
            .get(cur.line.saturating_sub(1))
            .map(|l| l.chars().count())
            .unwrap_or(0);
        model.move_to(cur.line.saturating_sub(1), prev_len, false);
        model.move_to(cur.line, end_col, true);
    }
    model.delete_selection();
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
    let after: Vec<Line> = model.lines().to_vec();
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

/// 把数据行数（**含表头**）调整到 `target`（Typora 尺寸弹层的行数步进器，
/// 2..=99）：不足在**末尾补空行**，超出从**末尾删行**（表头/分隔行不动）。
/// 返回是否发生了改动。
pub fn set_row_count(model: &mut EditorModel, target: usize) -> bool {
    let target = target.clamp(2, 99);
    let lines0 = model.lines().to_vec();
    let cur0 = model.cursor();
    let Some(mut block) = table_block_range(&lines0, cur0.line) else {
        return false;
    };
    let mut changed = false;
    // 每轮增减一行，直到达标；块定位始终锚在块首行（行删后光标可能
    // 落到表外，不能再靠光标重新找块）。分类每轮只跑一次，两个用途共用。
    loop {
        let lines = model.lines().to_vec();
        let flags: Vec<bool> = classify_document(&lines)
            .blocks
            .iter()
            .map(|b| matches!(b, Block::Table))
            .collect();
        let Some(b) = table_block_range_flagged(&lines, &flags, block.0) else {
            break;
        };
        block = b;
        let rows = table_data_rows(&lines, b);
        let Some(&last) = rows.last() else {
            break;
        };
        if rows.len() == target {
            break;
        }
        // 光标挪到末行，插入/删除都发生在表格末尾。
        model.move_to(last, 0, false);
        let ok = if rows.len() > target {
            delete_row(model)
        } else {
            insert_row(model, false)
        };
        if !ok {
            // 删到只剩表头（或块结构失效）：停在当前状态。
            break;
        }
        changed = true;
    }
    // 收尾：删**末行**时 `delete_row` 会把光标留在表外的下一行（表工具栏/
    // 尺寸弹层会随之消失）——有改动就统一拉回块内末行的首格内容起点。
    if changed {
        let lines = model.lines().to_vec();
        if let Some(b) = table_block_range(&lines, block.0)
            && let Some(&last) = table_data_rows(&lines, b).last()
        {
            let text = model.line(last).map(str::to_string).unwrap_or_default();
            let (cs, _) = table_cell_spans(&text).first().copied().unwrap_or((0, 0));
            model.move_to(last, char_col(&text, cs + 1), false);
        }
    }
    changed
}

/// 把列数调整到 `target`（Typora 尺寸弹层的列数步进器，1..=40）：不足在
/// **末尾补列**，超出从**末尾删列**（以分隔行为准）。返回是否发生了改动。
pub fn set_col_count(model: &mut EditorModel, target: usize) -> bool {
    let target = target.clamp(1, 40);
    let lines0 = model.lines().to_vec();
    let cur0 = model.cursor();
    let Some(mut block) = table_block_range(&lines0, cur0.line) else {
        return false;
    };
    let mut changed = false;
    loop {
        let lines = model.lines().to_vec();
        let flags: Vec<bool> = classify_document(&lines)
            .blocks
            .iter()
            .map(|b| matches!(b, Block::Table))
            .collect();
        let Some(b) = table_block_range_flagged(&lines, &flags, block.0) else {
            return changed;
        };
        block = b;
        let cols = table_col_count(&lines, b);
        if cols == 0 {
            return changed;
        }
        if cols == target {
            return changed;
        }
        // 光标挪到行尾 → `column_at` 得到末列，插/删都发生在最右。
        model.move_to(block.1, usize::MAX, false);
        let ok = if cols > target {
            delete_column(model)
        } else {
            insert_column(model, false)
        };
        if !ok {
            return changed;
        }
        changed = true;
    }
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
    let edits = vec![
        (cur.line, lines[other].to_string()),
        (other, lines[cur.line].to_string()),
    ];
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
    // 选中「块首行 .. 块末行 + 换行」，一次删干净。末行之后没有下一行可借
    //（文件末尾无换行时会被夹回本行、选区为空、残留末行源码），改为
    // 「块首行 .. 块末行行尾」。
    model.move_to(block.0, 0, false);
    if block.1 + 1 < model.line_count() {
        model.move_to(block.1 + 1, 0, true);
    } else {
        let end_col = lines
            .get(block.1)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        model.move_to(block.1, end_col, true);
    }
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
pub fn prettify_edits(lines: &[Line], block: (usize, usize)) -> Option<Vec<(usize, String)>> {
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
            .map(|c| pad(cells.get(c).map(|l| l.as_str()).unwrap_or(""), widths[c], aligns[c]))
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
    // 分类只跑一次：块判定与 `is_table` 闭包共用（闭包逐行调用，
    // 不能在里面重跑分类）。
    let flags: Vec<bool> = classify_document(&lines)
        .blocks
        .iter()
        .map(|b| matches!(b, Block::Table))
        .collect();
    let is_table = |l: usize| flags.get(l).copied().unwrap_or(false);
    if table_block_range_flagged(&lines, &flags, cur.line).is_none() {
        return false;
    }
    let Some((row, col, _)) = crate::editor::markdown_live::interactions::table_cell_at(
        &lines,
        &is_table,
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
    loop {
        let lines = model.lines().to_vec();
        let flags: Vec<bool> = classify_document(&lines)
            .blocks
            .iter()
            .map(|b| matches!(b, Block::Table))
            .collect();
        let n = table_block_range_flagged(&lines, &flags, model.cursor().line)
            .map(|b| table_data_rows(&lines, b).len())
            .unwrap_or(0);
        if n >= need_rows || guard >= 128 {
            break;
        }
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
    let after: Vec<Line> = model.lines().to_vec();
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
/// 跨格选区的结构化删除 / 覆盖输入（Typora spec #6）。
///
/// 拖拽可以产生端点落在**不同单元格**的选区；Typora 把它当格范围处理：
/// 删除/覆盖时「首格 ← 首格前缀 + `typed` + 尾格后缀」，首格之后到尾格
/// 为止的所有格（含尾格本身）清空，行/列结构不动（GFM 无 colspan，
/// 格子只清内容不合并结构）。光标落在首格的合并点，选区清除；`typed`
/// 非空即覆盖输入（合并与写入同一个撤销步）。
///
/// 返回 `false` = 不是跨格选区（无选区 / 同格 / 端点不在同一表块），
/// 调用方回退到常规文本删除 / 插入。
pub fn merge_cross_cell_selection(model: &mut EditorModel, typed: &str) -> bool {
    let Some((start, end)) = model.selection() else {
        return false;
    };
    if start == end {
        return false;
    }
    let lines = model.lines().to_vec();
    // 两端点必须落在**同一个**表块内（分类只跑一次，两个端点共用）。
    let classified = classify_document(&lines);
    let flags: Vec<bool> = classified
        .blocks
        .iter()
        .map(|b| matches!(b, Block::Table))
        .collect();
    let Some(block) = table_block_range_flagged(&lines, &flags, start.line) else {
        return false;
    };
    if table_block_range_flagged(&lines, &flags, end.line) != Some(block) {
        return false;
    }
    // 端点所在格列（字节落在管道上时归左格，随后截断会夹回内容内）。
    let cell_col = |line: usize, col: usize| -> Option<usize> {
        let text = lines.get(line)?;
        let byte = byte_for_col(text, col);
        let spans = table_cell_spans(text);
        spans
            .iter()
            .position(|(s, e)| byte >= *s && byte <= *e)
            .or_else(|| spans.iter().position(|(s, _)| *s >= byte))
            .or_else(|| spans.len().checked_sub(1))
    };
    let Some(sc) = cell_col(start.line, start.col) else {
        return false;
    };
    let Some(ec) = cell_col(end.line, end.col) else {
        return false;
    };
    if start.line == end.line && sc == ec {
        return false;
    }
    // 首/尾格的内容范围（char 列，去 padding）与端点截断。
    let start_text = &lines[start.line];
    let end_text = &lines[end.line];
    let Some((s_cs, s_ce)) = cell_content_cols(start_text, sc) else {
        return false;
    };
    let Some((e_cs, e_ce)) = cell_content_cols(end_text, ec) else {
        return false;
    };
    let off1 = start.col.clamp(s_cs, s_ce);
    let off2 = end.col.clamp(e_cs, e_ce);
    let prefix = &start_text[byte_for_col(start_text, s_cs)..byte_for_col(start_text, off1)];
    let suffix = &end_text[byte_for_col(end_text, off2)..byte_for_col(end_text, e_ce)];
    let mut merged = String::with_capacity(prefix.len() + typed.len() + suffix.len());
    merged.push_str(prefix);
    merged.push_str(typed);
    merged.push_str(suffix);
    let merge_col = s_cs + prefix.chars().count() + typed.chars().count();

    // 逐行重写（字节区间从右往左套用，前面的偏移不受影响）：
    //  - 首行：首格 ← 合并文本；首格之后的格全部清空；
    //  - 中间行：所有格清空；
    //  - 尾行：尾格及其之前的格清空（尾格后缀已并入首格），之后的不动；
    //  - 同行跨格：首尾之间清空（含尾格），首格写合并文本。
    let mut edits: Vec<(usize, String)> = Vec::new();
    let delim = delimiter_row(&lines, block);
    for l in start.line..=end.line {
        // 分隔行是结构不是数据行：不参与合并/清空。
        if Some(l) == delim {
            continue;
        }
        let text = &lines[l];
        let spans = table_cell_spans(text);
        let blank_from = if l == start.line { sc + 1 } else { 0 };
        let blank_to = if l == end.line {
            (ec + 1).min(spans.len())
        } else {
            spans.len()
        };
        // (字节起点, 字节终点, 替换文本)，按字节从大到小套用。
        let mut ops: Vec<(usize, usize, &str)> = Vec::new();
        for ci in (blank_from..blank_to).rev() {
            let Some((cs, ce)) = cell_content_cols(text, ci) else {
                continue;
            };
            ops.push((byte_for_col(text, cs), byte_for_col(text, ce), ""));
        }
        if l == start.line {
            ops.push((
                byte_for_col(text, s_cs),
                byte_for_col(text, s_ce),
                &merged,
            ));
        }
        let mut new_text = text.to_string();
        for (bs, be, repl) in ops {
            if bs <= be && be <= new_text.len() {
                new_text.replace_range(bs..be, repl);
            }
        }
        if new_text.as_str() != text.as_str() {
            edits.push((l, new_text.to_string()));
        }
    }
    if edits.is_empty() {
        return false;
    }
    // 整行批量改写：一个撤销步；随后把光标放到首格的合并点。
    let ranges: Vec<(Pos, Pos, String)> = edits
        .iter()
        .map(|(i, new)| {
            let len = lines[*i].chars().count();
            (Pos::new(*i, 0), Pos::new(*i, len), new.clone())
        })
        .collect();
    model.replace_ranges_each(&ranges);
    model.move_to(start.line, merge_col, false);
    true
}

fn apply_row_edits(model: &mut EditorModel, lines: &[Line], edits: &[(usize, String)]) {
    let mut sorted: Vec<(usize, String)> = edits.to_vec();
    sorted.sort_by_key(|(i, _)| *i);
    sorted.dedup_by_key(|(i, _)| *i);
    let saved = model.cursor();
    for (i, new) in sorted.into_iter().rev() {
        let old = lines.get(i).map(|l| l.as_str()).unwrap_or("");
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

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n\ntail\n";

    fn model_at(line: usize, col: usize) -> EditorModel {
        let mut m = EditorModel::new(DOC);
        m.move_to(line, col, false);
        m
    }

    const DOC2: &str = "| abc | def |\n|---|---|\n| ghi | jkl |\n| mno | pqr |\n";

    /// 跨格选区（拖拽成形：锚 (0,3) → 终 (2,10)）：合并首尾格、清空中间格。
    #[test]
    fn merge_cross_cell_blanks_between_and_merges_endpoints() {
        let mut m = EditorModel::new(DOC2);
        m.move_to(0, 3, false); // 首格 "abc" 的 'b'：前缀 = "a"
        m.move_to(2, 10, true); // 尾格 "jkl" 的 'l'：后缀 = "l"
        assert!(merge_cross_cell_selection(&mut m, ""));
        let lines: Vec<&str> = m.lines().iter().map(|l| l.as_str()).collect();
        // 首行：首格 ← "a"+"l"，同排其后的格清空；分隔行不动；
        // 中间数据行整行清空；尾行到尾格为止全部清空（尾格之后无列）。
        assert_eq!(lines[0], "| al |  |");
        assert_eq!(lines[1], "|---|---|");
        assert_eq!(lines[2], "|  |  |");
        // 尾行之后的行不动（选区外）。
        assert_eq!(lines[3], "| mno | pqr |");
        // 行数（结构）不变；光标在首格合并点、选区清除。
        assert_eq!(m.line_count(), 5);
        assert_eq!(m.cursor(), Pos::new(0, 3));
        assert!(m.selection().is_none());
    }

    /// 覆盖输入：合并点插入 typed（与删除同一撤销步语义）。
    #[test]
    fn merge_cross_cell_type_over_inserts_at_merge_point() {
        let mut m = EditorModel::new(DOC2);
        m.move_to(0, 3, false);
        m.move_to(2, 10, true);
        assert!(merge_cross_cell_selection(&mut m, "X"));
        assert_eq!(m.lines()[0].as_str(), "| aXl |  |");
        assert_eq!(m.cursor(), Pos::new(0, 4)); // "a" + "X" 之后
        assert!(m.selection().is_none());
    }

    /// 同格选区 / 无选区：不是跨格，回退常规路径（返回 false、零改动）。
    #[test]
    fn merge_cross_cell_ignores_same_cell_and_no_selection() {
        let mut m = EditorModel::new(DOC2);
        // 无选区。
        m.move_to(0, 3, false);
        assert!(!merge_cross_cell_selection(&mut m, ""));
        // 同格选区（cell0 内 col2..col4）。
        m.move_to(0, 2, false);
        m.move_to(0, 4, true);
        assert!(!merge_cross_cell_selection(&mut m, ""));
        assert_eq!(m.lines()[0].as_str(), "| abc | def |");
        assert!(m.selection().is_some(), "同格选区应原样保留给常规删除");
    }

    /// 整个合并 = 一个撤销步。
    #[test]
    fn merge_cross_cell_is_one_undo_step() {
        let mut m = EditorModel::new(DOC2);
        m.move_to(0, 3, false);
        m.move_to(2, 10, true);
        assert!(merge_cross_cell_selection(&mut m, ""));
        assert!(m.undo());
        let lines: Vec<&str> = m.lines().iter().map(|l| l.as_str()).collect();
        assert_eq!(lines, DOC2.split('\n').collect::<Vec<_>>());
    }

    #[test]
    fn block_range_and_rows_skip_delimiter() {
        let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(DOC);
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
        let lines: Vec<Line> = m.lines().to_vec();
        assert_eq!(table_aligns(&lines, (0, 3))[0], Align::Left);
        // 再点同一个 → 清除。
        assert!(set_column_align(&mut m, Some(Align::Left)));
        let lines: Vec<Line> = m.lines().to_vec();
        assert_eq!(table_aligns(&lines, (0, 3))[0], Align::None);
        // 右对齐。
        assert!(set_column_align(&mut m, Some(Align::Right)));
        let lines: Vec<Line> = m.lines().to_vec();
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

    /// 回归：紧邻表格、只是恰好含 `|` 的普通段落行**不属于**表格块
    ///（旧实现按「行内含 `|`」扩张，会把段落算进块，删列/移列/重排会
    /// 把它当表格行改写；表头保护也会失效）。
    ///
    /// 判定与渲染器同源（`classify_document`）：GFM 里表格体之后的非空行
    /// 会延续网格（pipeless 行），因此只有**表格之前**的段落行会被旧逻辑
    /// 误吸 —— 也正是下面断言的情形。
    #[test]
    fn block_range_does_not_absorb_pipe_paragraphs() {
        let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(
            "用 `a | b` 表示或运算\n| a | b |\n|---|---|\n| 1 | 2 |\n\n尾部 | 段落\n",
        );
        assert_eq!(table_block_range(&lines, 2), Some((1, 3)), "前导段落行不得进块");
        // 块内数据行：表头 + 1 条数据（段落行不在其中）。
        assert_eq!(table_data_rows(&lines, (1, 3)), vec![1, 3]);
        // 表头保护仍然生效：对表头行删行 = 无操作。
        let mut m = EditorModel::new(&crate::editor::markdown_live::model::join_lines(&lines));
        m.move_to(1, 2, false);
        assert!(!delete_row(&mut m), "表头不可删");
        assert_eq!(m.text(), crate::editor::markdown_live::model::join_lines(&lines), "拒绝时不得改动文本");
        // 块范围与渲染器分类一致（同源，不可能再出现「op 改了渲染器不认为
        // 是表格的行」）。
        let classified = classify_document(&lines);
        let flags: Vec<bool> = classified
            .blocks
            .iter()
            .map(|b| matches!(b, Block::Table))
            .collect();
        assert_eq!(
            table_block_range(&lines, 2),
            table_block_range_flagged(&lines, &flags, 2),
        );
    }

    /// 回归：文件末尾无换行时，删除末行/删表也要真的删掉（旧实现选中
    /// 「本行 + 下一行」会被夹回本行、选区为空，末行源码残留）。
    #[test]
    fn delete_at_eof_without_trailing_newline() {
        // 末行是数据行：删行 → 只剩表头 + 分隔行。
        let mut m = EditorModel::new("| a | b |\n|---|---|\n| 1 | 2 |");
        m.move_to(2, 2, false);
        assert!(delete_row(&mut m));
        assert_eq!(m.text(), "| a | b |\n|---|---|");

        // 末行是数据行：删表 → 整块消失。
        let mut m = EditorModel::new("| a | b |\n|---|---|\n| 1 | 2 |");
        m.move_to(2, 2, false);
        assert!(delete_table(&mut m));
        assert_eq!(m.text(), "", "末行无换行时删表不得残留源码");
    }
}
