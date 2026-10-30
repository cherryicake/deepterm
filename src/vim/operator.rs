//! Vim 操作符与编辑命令的执行。
//!
//! 分工：[`super::motion`] 只算「移动到哪里」，本模块负责「作用在什么范围、
//! 怎么改文本」。操作符的实际范围由 [`super::state::MotionKind`] 决定——
//! `dj` 是整行、`dw` 到词首前、`d$` 含行尾字符，这个三态是 vim 兼容性的核心。
//!
//! 编辑统一走 `replace_text_in_range_silent`：它是程序化编辑入口，不触发
//! 括号自动配对（`d` 之后重放单字符不应再配对一次）。

use gpui::{App, BorrowAppContext as _, Context, Window};

use super::marks::Marks;
use super::motion;
use super::object::{self, TextObject};
use super::state::{Change, LastFind, MotionKind, Operator, PendingChar, VimMode};
use crate::components::input::RopeExt as _;
use crate::components::input::state::InputState;

/// 一次操作符作用的范围与语义。
pub struct Operation {
    pub range: std::ops::Range<usize>,
    pub kind: MotionKind,
}

impl InputState {
    /// 取走待决的操作符栈（外层操作符）。
    pub(crate) fn vim_take_operator(&mut self) -> Option<Operator> {
        let vim = self.vim.as_mut()?;
        // 栈顶可能是 `i`/`a` 对象前缀，它不算外层操作符。
        while matches!(vim.operator_stack.last(), Some(Operator::Object { .. })) {
            vim.operator_stack.pop();
        }
        vim.operator_stack.pop()
    }

    /// 待决栈里是否有对象前缀（`i`/`a`）。
    pub(crate) fn vim_object_prefix(&self) -> Option<bool> {
        let vim = self.vim.as_ref()?;
        vim.operator_stack
            .iter()
            .rev()
            .find_map(|operator| match operator {
                Operator::Object { around } => Some(*around),
                _ => None,
            })
    }

    /// 清空待决状态（操作符执行完或放弃时调用）。
    pub(crate) fn vim_clear_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(vim) = self.vim.as_mut() {
            vim.clear_pending();
        }
        cx.notify();
    }

    // ==================== 操作符 + 移动 ====================

    /// 把「操作符 + 移动」落到文本上。
    ///
    /// `target` 是移动的落点，`kind` 是该移动的选区语义。
    pub(crate) fn vim_apply_operator(
        &mut self,
        operator: Operator,
        target: usize,
        kind: MotionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut target = target;
        let mut kind = kind;

        // vim 的 `cw` 特例：当光标在**词中间**时，`cw` 只替换到词尾
        // （等价于 `ce`），而不是删到下一个词首。这是 vim 唯一一处
        // 「操作符改变移动语义」的地方，且被广泛依赖（`cw` 是最高频的
        // 改词命令，若按 `dw` 语义会吃掉分隔空格）。
        if matches!(operator, Operator::Change)
            && kind == MotionKind::Exclusive
            && !self.vim_cursor_on_whitespace()
        {
            let word_end = motion::next_word_end(&self.text, self.cursor());
            if word_end >= self.cursor() && word_end <= target {
                target = word_end;
                kind = MotionKind::Inclusive;
            }
        }

        // `cw` 在空白（空行、行尾）上：vim 只进入 Insert，不删任何东西。
        // 否则独占范围会跨过换行删掉下一行的开头。
        if matches!(operator, Operator::Change) && self.vim_cursor_on_whitespace() {
            self.vim_switch_mode(VimMode::Insert, cx);
            self.vim_clear_pending(cx);
            return;
        }

        let Some(range) = self.vim_operator_range(target, kind) else {
            return;
        };
        self.vim_execute(operator, range, kind, window, cx);
    }

    /// 光标是否落在空白上（`cw` 特例的判据）。
    fn vim_cursor_on_whitespace(&self) -> bool {
        let cursor = self.vim_cursor();
        self.text
            .get_char(cursor)
            .ok()
            .is_none_or(|ch| ch.is_whitespace())
    }

    /// 把「操作符 + 文本对象」落到文本上。
    pub(crate) fn vim_apply_operator_object(
        &mut self,
        operator: Operator,
        object: TextObject,
        around: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let Some((range, kind)) = object::find(&self.text, cursor, object, around) else {
            // 找不到对象：vim 保持光标与文本不变，但丢弃待决操作符。
            self.vim_clear_pending(cx);
            return;
        };
        self.vim_execute(operator, range, kind, window, cx);
    }

    /// 依据 `MotionKind` 把「光标 → 目标」翻译成实际编辑范围。
    ///
    /// 三态语义（对齐 vim / Neovim 的 `ops.c`）：
    ///
    /// - `Linewise`：整行，含行尾换行。
    /// - `Exclusive`：到目标之前，不含目标字符。
    /// - `Inclusive`：含目标字符。
    ///
    /// 正向与反向（目标在光标之前）都要处理，否则 `db` 与 `dw` 会不对称。
    fn vim_operator_range(
        &self,
        target: usize,
        kind: MotionKind,
    ) -> Option<std::ops::Range<usize>> {
        let cursor = self.cursor();
        let (start, end) = if target >= cursor {
            (cursor, target)
        } else {
            (target, cursor)
        };

        match kind {
            MotionKind::Linewise => {
                let text = &self.text;
                let start_row = text.offset_to_point(start).row;
                let end_row = text.offset_to_point(end).row;
                let line_start = text.line_start_offset(start_row);
                // 含末行：取下一行的行首（即吃掉换行）。
                let line_end = if end_row + 1 < text.lines_len() {
                    text.line_start_offset(end_row + 1)
                } else {
                    text.len()
                };
                Some(line_start..line_end)
            }
            MotionKind::Inclusive => {
                // 含目标字符：终点后移一个字符。
                let text = &self.text;
                if end >= text.len() {
                    Some(start..text.len())
                } else {
                    let next = text.clip_offset(end + 1, sum_tree::Bias::Right);
                    Some(start..next)
                }
            }
            MotionKind::Exclusive => {
                if start == end {
                    // 原地不动（如行尾按 `w`）：按 vim 语义退化为整行，
                    // 这样空行上的 `dw` 仍会删掉换行。
                    let text = &self.text;
                    let row = text.offset_to_point(start).row;
                    let line_start = text.line_start_offset(row);
                    let line_end = if row + 1 < text.lines_len() {
                        text.line_start_offset(row + 1)
                    } else {
                        text.len()
                    };
                    Some(line_start..line_end)
                } else {
                    // 独占移动在行尾**钳住**：`dw` 删到行尾就停，不吃换行。
                    // 不钳的话 `dw` 会把两行合并——vim 里 `dw` 在行尾最后一个
                    // 词上只删词，保留换行。
                    let text = &self.text;
                    let start_row = text.offset_to_point(start).row;
                    let line_end = text.line_content_end_offset(start_row);
                    let clamped_end = end.min(line_end);
                    if clamped_end <= start {
                        // 钳完为空：说明起点已在行尾，退化为整行（同上面的分支）。
                        let line_start = text.line_start_offset(start_row);
                        let next_line_start = if start_row + 1 < text.lines_len() {
                            text.line_start_offset(start_row + 1)
                        } else {
                            text.len()
                        };
                        Some(line_start..next_line_start)
                    } else {
                        Some(start..clamped_end)
                    }
                }
            }
        }
    }

    // ==================== 执行 ====================

    fn vim_execute(
        &mut self,
        operator: Operator,
        range: std::ops::Range<usize>,
        kind: MotionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if range.is_empty() {
            self.vim_clear_pending(cx);
            return;
        }

        // 记录本次修改供 `.` 重放。只有真正改文本的操作才记录——
        // `y` 不改文本，vim 里也不会被 `.` 重放。
        let recorded = match operator {
            Operator::Delete | Operator::Change => Some(Change::Delete {
                len: range.len(),
                // 行级删除（`dd`）与单字符删除（`x`）是定长的；
                // 其余来自移动，重放时要按新位置重算范围。
                follow_motion: kind != MotionKind::Linewise && range.len() > 1,
                // 光标在范围末尾说明是向后移动（`db`），重放也要向后。
                backward: range.end <= self.vim_cursor(),
                linewise: kind == MotionKind::Linewise,
            }),
            _ => None,
        };

        match operator {
            Operator::Delete | Operator::Change => {
                self.vim_yank_to_clipboard(range.clone(), kind, cx);
                // 删掉范围；Change 之后进入 Insert。
                let start = range.start;
                self.vim_replace_range(range, "", window, cx);
                self.move_to(start, None, cx);
                if matches!(operator, Operator::Change) {
                    self.vim_switch_mode(VimMode::Insert, cx);
                }
            }
            Operator::Yank => {
                self.vim_copy_to_clipboard(range.clone(), kind, cx);
                // 光标回到范围起点（vim 的 `y` 不移动光标到别处）。
                self.move_to(range.start, None, cx);
            }
            // 对象前缀本身不是操作符，不应执行到这里。
            Operator::Object { .. } => {}
        }

        if let Some(change) = recorded {
            if matches!(operator, Operator::Change) {
                // `cw`/`cc` 会进入 Insert：标记为待插入，退出时升级成 Replace。
                self.vim_record_change_awaiting_insert(change);
            } else {
                self.vim_record_change(change);
            }
        }
        self.vim_clear_pending(cx);
    }

    /// 从 `start` 起，`start` 所在**词**的末尾之后一格（即替换范围的上界）。
    ///
    /// 与 `motion::next_word_end` 的区别：后者会跳过空白继续找下一个词，
    /// 本函数只走完当前这段非空白字符就停。`cw` 的替换范围需要后者语义。
    fn vim_current_word_end(&self, start: usize) -> usize {
        let text = &self.text;
        let mut offset = start;
        while offset < text.len() {
            match text.get_char(offset).ok() {
                Some(ch) if !ch.is_whitespace() => {
                    offset = text.clip_offset(offset + 1, sum_tree::Bias::Right);
                }
                _ => break,
            }
        }
        offset
    }

    /// `m{a-z}`：在当前光标处打标记。
    pub(crate) fn vim_do_set_mark(&mut self, name: char, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let marks = Marks::global_mut(cx);
        marks.set(name, cursor);
        cx.notify();
    }

    /// `` `{a-z} `` / `'{a-z}`：跳到标记。
    ///
    /// `` ` `` 跳到精确位置，`'` 跳到该行首个非空白字符（vim 语义）。
    pub(crate) fn vim_do_jump_mark(&mut self, name: char, linewise: bool, cx: &mut Context<Self>) {
        let Some(offset) = Marks::global(cx).get(name) else {
            return;
        };
        // 标记存的是绝对字节偏移，不随编辑调整：文本被删短后可能指向
        // 末尾之后。钳到字符边界，并在落到末尾时退回上一字符，
        // 否则光标停在文本末尾之外，`x`/`~`/`s` 全都无效。
        let offset = self
            .text
            .clip_offset(offset.min(self.text.len()), sum_tree::Bias::Left);
        let offset = if offset >= self.text.len() && self.text.len() > 0 {
            self.previous_boundary(self.text.len())
        } else {
            offset
        };
        let target = if linewise {
            motion::first_non_whitespace(&self.text, offset)
        } else {
            offset
        };
        self.vim_move_in_mode(target, cx);
    }

    /// `m`：等待一个标记名（写入）。
    pub(crate) fn vim_do_push_set_mark(&mut self, cx: &mut Context<Self>) {
        self.vim_do_push_char(PendingChar::SetMark, cx);
    }

    /// `` ` `` / `'`：等待一个标记名（跳转）。
    pub(crate) fn vim_do_push_jump_mark(&mut self, linewise: bool, cx: &mut Context<Self>) {
        self.vim_do_push_char(PendingChar::JumpMark { linewise }, cx);
    }

    /// 可视删除的执行入口：与 [`Self::vim_execute`] 的区别是记录方式。
    ///
    /// 可视删除按**宽度**重放（vim 里 `v4l` 之后 `.` 再删同样多的字符），
    /// 所以记录成定长删除（`follow_motion: false`），并把方向写进记录。
    /// 普通 `dw` 则是「跟随移动」，重放时按新位置重算范围——两者语义不同。
    fn vim_execute_visual(
        &mut self,
        operator: Operator,
        range: std::ops::Range<usize>,
        kind: MotionKind,
        backward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if range.is_empty() {
            return;
        }
        let len = range.len();
        let linewise = kind == MotionKind::Linewise;

        match operator {
            Operator::Delete | Operator::Change => {
                self.vim_yank_to_clipboard(range.clone(), kind, cx);
                let start = range.start;
                self.vim_replace_range(range, "", window, cx);
                self.move_to(start, None, cx);
                self.vim_record_change(Change::Delete {
                    len,
                    follow_motion: false,
                    backward,
                    linewise,
                });
                if matches!(operator, Operator::Change) {
                    self.vim_switch_mode(VimMode::Insert, cx);
                }
            }
            Operator::Yank => {
                self.vim_copy_to_clipboard(range.clone(), kind, cx);
                self.move_to(range.start, None, cx);
            }
            Operator::Object { .. } => {}
        }
    }

    /// `.`：在当前光标处重放上次修改。
    ///
    /// 重放的是**修改本身**而不是按键：`dw` 之后把光标移到别处再按 `.`，
    /// 应按新位置重新计算范围（vim 的 `.` 语义）。范围长度取自记录值，
    /// 但起点用当前光标。
    pub(crate) fn vim_do_repeat_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(change) = self.vim.as_ref().and_then(|vim| vim.last_change.clone()) else {
            return;
        };
        let cursor = self.cursor();
        match change {
            Change::Delete {
                len,
                follow_motion,
                backward,
                linewise,
            } => {
                let range = if linewise {
                    // 行级删除：重放时同样作用于当前行。
                    let row = self.text.offset_to_point(cursor).row;
                    let start = self.text.line_start_offset(row);
                    let end = if row + 1 < self.text.lines_len() {
                        self.text.line_start_offset(row + 1)
                    } else {
                        self.text.len()
                    };
                    start..end
                } else if follow_motion {
                    // 跟随移动的删除：按新光标位置重算到词边界，
                    // 这样 `.` 重放 `dw` 是「再删一个词」而非「再删 N 字符」。
                    // 方向必须沿用记录值，否则 `.` 重放 `db` 会朝前删。
                    if backward {
                        let target = motion::previous_word_start(&self.text, cursor);
                        target..cursor
                    } else {
                        let target = motion::next_word_start(&self.text, cursor);
                        cursor..target
                    }
                } else {
                    let end = (cursor + len).min(self.text.len());
                    cursor..self.text.clip_offset(end, sum_tree::Bias::Left)
                };
                if range.is_empty() {
                    return;
                }
                self.vim_yank_to_clipboard(range.clone(), MotionKind::Exclusive, cx);
                self.vim_replace_range(range, "", window, cx);
                self.move_to(cursor, None, cx);
            }
            Change::Insert { text } => {
                self.vim_replace_range(cursor..cursor, &text, window, cx);
                self.move_to(cursor + text.len(), None, cx);
            }
            Change::Replace {
                len,
                follow_motion,
                backward,
                text,
            } => {
                let range = if follow_motion && backward {
                    // 向后替换（`cb`）：从上一个词首删到光标处。
                    let start = motion::previous_word_start(&self.text, cursor);
                    start..cursor
                } else if follow_motion {
                    // `cw` 的替换：重算「光标所在的词」到词尾（不含分隔空格）。
                    //
                    // 关键是先跳过空白再取**当前**词的词尾，而不是用
                    // `next_word_end` 一路扫到下一个词——后者在光标已停在
                    // 目标词上时会把后面的词也吞掉。
                    let start = {
                        let mut offset = cursor;
                        while offset < self.text.len()
                            && self
                                .text
                                .get_char(offset)
                                .ok()
                                .is_some_and(char::is_whitespace)
                        {
                            offset = self.text.clip_offset(offset + 1, sum_tree::Bias::Right);
                        }
                        offset
                    };
                    let end = self.vim_current_word_end(start);
                    start..end
                } else {
                    let end = (cursor + len).min(self.text.len());
                    cursor..self.text.clip_offset(end, sum_tree::Bias::Left)
                };
                let range_start = range.start;
                self.vim_replace_range(range, &text, window, cx);
                self.move_to(range_start, None, cx);
            }
            Change::Indent { outdent, lines } => {
                // 记录的 `outdent` 是意图；`vim_do_indent_line` 的形参是
                // `indent`（true = 缩进），所以要取反。
                for _ in 0..lines.max(1) {
                    self.vim_do_indent_line(!outdent, window, cx);
                }
            }
            Change::JoinLines => {
                self.vim_do_join_lines(window, cx);
            }
            Change::ToggleCase { len } => {
                for _ in 0..len.max(1) {
                    self.vim_do_toggle_case(1, window, cx);
                }
            }
        }

        // 重放后光标位置可能变化，确保在可见区。
        self.scroll_to(self.cursor(), None, cx);
        cx.notify();
    }

    /// 把范围写进寄存器（以及需要时的系统剪贴板）。
    ///
    /// 行级操作补一个换行，这样 `yy` + `p` 能整行粘贴（与 vim 一致）。
    /// 是否写系统剪贴板由寄存器名决定：只有 `"+`/`"*` 才碰剪贴板，
    /// 否则每次 `dd` 都会覆盖用户从别处复制的内容。
    fn vim_yank_to_clipboard(
        &mut self,
        range: std::ops::Range<usize>,
        kind: MotionKind,
        cx: &mut Context<Self>,
    ) {
        let mut text = self.text.slice(range).to_string();
        let linewise = kind == MotionKind::Linewise;
        if linewise && !text.ends_with('\n') {
            text.push('\n');
        }
        let register = self.vim.as_ref().and_then(|vim| vim.selected_register);

        cx.update_global::<crate::vim::Registers, _>(|regs, cx| {
            regs.record_delete(&text, linewise, register, cx);
        });
    }

    /// 复制（`y`）：写 `0` 与无名寄存器。
    fn vim_copy_to_clipboard(
        &mut self,
        range: std::ops::Range<usize>,
        kind: MotionKind,
        cx: &mut Context<Self>,
    ) {
        let mut text = self.text.slice(range).to_string();
        if kind == MotionKind::Linewise && !text.ends_with('\n') {
            text.push('\n');
        }
        let register = self.vim.as_ref().and_then(|vim| vim.selected_register);

        cx.update_global::<crate::vim::Registers, _>(|regs, cx| {
            regs.record_yank(&text, register, cx);
        });
    }

    /// 读寄存器内容（`p` 的来源）。
    pub(crate) fn vim_read_register(&self, cx: &App) -> String {
        let register = self.vim.as_ref().and_then(|vim| vim.selected_register);
        crate::vim::Registers::global(cx).read(register, cx)
    }

    /// 程序化替换一个范围。
    ///
    /// 所有 Normal 模式的编辑都经过这里，所以**在这里断组**：vim 里每个
    /// Normal 命令是独立的一步撤销，而编辑器的历史按 1 秒间隔分组，连续两次
    /// `x` 会被并成一步。编辑完成后断组，下一次编辑即落入新的版本号。
    /// Insert 模式的输入不走这里（走平台输入处理器），因此不受影响。
    fn vim_replace_range(
        &mut self,
        range: std::ops::Range<usize>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range_utf16 = self.range_to_utf16(&range);
        self.replace_text_in_range_silent(Some(range_utf16), new_text, window, cx);
        self.history.break_group();
    }

    // ==================== 单键命令 ====================

    /// `x`：删除光标处字符。
    pub(crate) fn vim_do_delete_char(
        &mut self,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        let line_end = self.text.line_content_end_offset(row);
        if cursor >= line_end {
            // 行尾：vim 的 `x` 不跨行删除。
            return;
        }
        // 计数按字符数作用，但**不跨行**（vim 的 `3x` 在行尾会提前停）。
        let mut end = cursor;
        for _ in 0..count {
            if end >= line_end {
                break;
            }
            end = self.text.clip_offset(end + 1, sum_tree::Bias::Right);
        }
        self.vim_yank_to_clipboard(cursor..end, MotionKind::Exclusive, cx);
        self.vim_replace_range(cursor..end, "", window, cx);
        self.move_to(cursor, None, cx);
        self.vim_record_change(Change::Delete {
            len: end - cursor,
            follow_motion: false,
            backward: false,
            linewise: false,
        });
    }

    /// 记录一次可重复的修改（供 `.` 重放）。
    pub(crate) fn vim_record_change(&mut self, change: Change) {
        if let Some(vim) = self.vim.as_mut() {
            vim.last_change = Some(change);
            vim.last_change_awaits_insert = false;
        }
    }

    /// 记录一次「删 + 待插入」的半成品修改（`cw`/`s`/`S` 等）。
    ///
    /// 退出 Insert 时 [`InputState::vim_record_insert`] 会把它升级成
    /// `Replace`，把键入文本一并记进去。
    pub(crate) fn vim_record_change_awaiting_insert(&mut self, change: Change) {
        if let Some(vim) = self.vim.as_mut() {
            vim.last_change = Some(change);
            vim.last_change_awaits_insert = true;
        }
    }

    /// `dd` / `yy` / `cc`：整行操作。`count` 是行数（`2dd` 删两行）。
    pub(crate) fn vim_line_operator(
        &mut self,
        operator: Operator,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        // 行级计数作用在行数上：把终点推到第 `count` 行的行首，
        // 这样 `vim_operator_range` 的 Linewise 分支会覆盖整段。
        let text = &self.text;
        let start_row = text.offset_to_point(cursor).row;
        let last_row =
            (start_row + count.saturating_sub(1)).min(text.lines_len().saturating_sub(1));
        let target = text.line_start_offset(last_row);
        self.vim_apply_operator(operator, target, MotionKind::Linewise, window, cx);
    }

    /// `p`：在光标后粘贴。
    ///
    /// 行级内容插到下一行，字符级内容插到光标之后。
    pub(crate) fn vim_do_paste(
        &mut self,
        after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_paste(after, window, cx);
    }

    /// `P`：在光标前粘贴。
    fn vim_paste(&mut self, after: bool, window: &mut Window, cx: &mut Context<Self>) {
        // 内容取自寄存器：无指定寄存器时是无名寄存器（`dd`/`yy` 都会写它），
        // 只有显式 `"+p` 才从系统剪贴板读。
        let content = self.vim_read_register(cx);
        if content.is_empty() {
            return;
        }
        let content = content.as_str();

        let cursor = self.cursor();
        if content.ends_with('\n') {
            // 行级粘贴：`p` 插到当前行**之后**，`P` 插到当前行之前。
            //
            // 内容自带结尾换行，所以插到「下一行的行首」就等于插在当行之后；
            // 末行没有下一行时插到文本末尾，需要补一个换行把内容隔到新行上。
            let row = self.text.offset_to_point(cursor).row;
            let is_last_line = row + 1 >= self.text.lines_len();
            let (insert_at, payload) = if after {
                if is_last_line {
                    (self.text.len(), format!("\n{content}"))
                } else {
                    (self.text.line_start_offset(row + 1), content.to_string())
                }
            } else {
                (self.text.line_start_offset(row), content.to_string())
            };

            self.vim_replace_range(insert_at..insert_at, &payload, window, cx);
            // 光标落到粘贴内容的行首（vim 语义）。
            let target_row = if after { row + 1 } else { row };
            let target = self
                .text
                .line_start_offset(target_row.min(self.text.lines_len().saturating_sub(1)));
            self.move_to(target, None, cx);
        } else {
            let insert_at = if after {
                let row = self.text.offset_to_point(cursor).row;
                let line_end = self.text.line_content_end_offset(row);
                // 光标在行尾字符上时，`p` 插到它之后。
                self.text
                    .clip_offset((cursor + 1).min(line_end), sum_tree::Bias::Right)
            } else {
                cursor
            };
            self.vim_replace_range(insert_at..insert_at, content, window, cx);
            let target = insert_at + content.len().saturating_sub(1);
            self.move_to(
                self.text.clip_offset(target, sum_tree::Bias::Left),
                None,
                cx,
            );
        }
    }

    /// `J`：把下一行接到当前行。
    pub(crate) fn vim_do_join_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        if row + 1 >= self.text.lines_len() {
            return;
        }
        let line_end = self.text.line_content_end_offset(row);
        let next_start = self.text.line_start_offset(row + 1);
        // 删掉换行，并在接缝处补一个空格（vim 的 `J` 语义），
        // 但下一行是空行或当前行以空白结尾时不补。
        let next_first = self.text.get_char(next_start).ok();
        let sep = match next_first {
            None | Some('\n') => "",
            _ if self
                .text
                .get_char(line_end.saturating_sub(1))
                .ok()
                .is_some_and(char::is_whitespace) =>
            {
                ""
            }
            _ => " ",
        };
        let replacement = sep.to_string();
        self.vim_replace_range(line_end..next_start, &replacement, window, cx);
        self.move_to(line_end, None, cx);
        self.vim_record_change(Change::JoinLines);
    }

    /// `>>` / `<<`：整行缩进/反缩进。
    /// 直接改行首空白，不走 `indent_inline`：后者在空光标时是「插入一个缩进
    /// 单元」而非「整行缩进」，与 vim 的 `>>` 语义不同。
    pub(crate) fn vim_do_indent_line(
        &mut self,
        indent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        let line_start = self.text.line_start_offset(row);
        let column = cursor - line_start;

        let tab = self.tab_size;
        let unit = if tab.hard_tabs {
            "\t".to_string()
        } else {
            " ".repeat(tab.tab_size.max(1))
        };

        if indent {
            self.vim_replace_range(line_start..line_start, &unit, window, cx);
            self.move_to(line_start + unit.len() + column, None, cx);
        } else {
            // 反缩进：删掉行首至多一个缩进单元（若干空格，或一个 Tab）。
            let line_end = self.text.line_content_end_offset(row);
            let mut remove = 0;
            let mut offset = line_start;
            while offset < line_end && remove < unit.len() {
                match self.text.get_char(offset).ok() {
                    Some(' ') => {
                        remove += 1;
                        offset += 1;
                    }
                    Some('\t') if remove == 0 => {
                        remove = 1;
                        break;
                    }
                    _ => break,
                }
            }
            if remove > 0 {
                self.vim_replace_range(line_start..line_start + remove, "", window, cx);
            }
            let target = line_start + column.saturating_sub(remove);
            self.move_to(target.min(self.text.len()), None, cx);
        }
        self.vim_record_change(Change::Indent {
            outdent: !indent,
            lines: 1,
        });
    }
}

/// 光标所在的词范围。
///
/// 复用编辑器的词判定（`edit_primitives::word_range_at`），与双击选词、
/// 同词高亮同一口径——vim 的 `iw` 与它们的「词」定义一致。
#[cfg(test)]
fn word_range_at(text: &ropey::Rope, cursor: usize) -> Option<std::ops::Range<usize>> {
    crate::components::input::word_range_at(text, cursor)
}

/// 撤销 / 重做、行内快捷命令等基础编辑操作。
///
/// 这些是 neovim 日常使用的最低要求：没有 `u` 就没有可用的编辑循环。
impl InputState {
    /// `u`：撤销一步。
    ///
    /// 复用编辑器自己的 undo（它会整组回放并恢复光标），但**先断开分组**：
    /// vim 里每次 `u` 撤销的是上一次完整编辑，而编辑器的 1 秒分组窗口会把
    /// 连续的 `x` 合并成一步，与 vim 语义不符。
    pub(crate) fn vim_do_undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 一次 Insert 会话已由 `vim_switch_mode` 分组；这里确保
        // 「Normal 下连续单键编辑」也各自成组。
        self.history.break_group();
        self.undo(&crate::components::input::Undo, window, cx);
    }

    /// `ctrl-r`：重做一步。
    pub(crate) fn vim_do_redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.redo(&crate::components::input::Redo, window, cx);
    }

    /// `D`：删到行尾（`d$` 的等价单键形式）。
    pub(crate) fn vim_do_delete_to_end_of_line(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let target = motion::end_of_line(&self.text, cursor);
        self.vim_apply_operator(Operator::Delete, target, MotionKind::Inclusive, window, cx);
    }

    /// `C`：改到行尾并进入 Insert（`c$` 的等价单键形式）。
    pub(crate) fn vim_do_change_to_end_of_line(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let target = motion::end_of_line(&self.text, cursor);
        self.vim_apply_operator(Operator::Change, target, MotionKind::Inclusive, window, cx);
    }

    /// `Y`：复制整行。
    pub(crate) fn vim_do_yank_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        self.vim_apply_operator(Operator::Yank, cursor, MotionKind::Linewise, window, cx);
    }

    /// `s`：删掉光标处字符并进入 Insert。
    pub(crate) fn vim_do_substitute_char(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        let line_end = self.text.line_content_end_offset(row);
        if cursor >= line_end {
            // 行尾：`s` 退化为直接进入 Insert（与 vim 一致）。
            self.vim_switch_mode(VimMode::Insert, cx);
            return;
        }
        let end = self.text.clip_offset(cursor + 1, sum_tree::Bias::Right);
        self.vim_yank_to_clipboard(cursor..end, MotionKind::Exclusive, cx);
        self.vim_replace_range(cursor..end, "", window, cx);
        self.move_to(cursor, None, cx);
        // 记录删除，退出 Insert 时会被升级成 `Replace`（删 + 插入的文本），
        // 这样 `.` 能重放整次替换而不只是插入。
        self.vim_record_change_awaiting_insert(Change::Delete {
            len: end - cursor,
            follow_motion: false,
            backward: false,
            linewise: false,
        });
        self.vim_switch_mode(VimMode::Insert, cx);
    }

    /// `S`：清空整行内容并进入 Insert（保留换行）。
    pub(crate) fn vim_do_substitute_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        let start = self.text.line_start_offset(row);
        let end = self.text.line_content_end_offset(row);
        if end > start {
            self.vim_yank_to_clipboard(start..end, MotionKind::Exclusive, cx);
            self.vim_replace_range(start..end, "", window, cx);
            self.vim_record_change_awaiting_insert(Change::Delete {
                len: end - start,
                follow_motion: false,
                backward: false,
                linewise: false,
            });
        }
        self.move_to(start, None, cx);
        self.vim_switch_mode(VimMode::Insert, cx);
    }

    /// `o` / `O`：在上方或下方开新行并进入 Insert。
    ///
    /// 新行沿用当前行的缩进（vim 的 `autoindent` 行为）。
    pub(crate) fn vim_do_open_line(
        &mut self,
        below: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        let line_start = self.text.line_start_offset(row);

        // 取当前行缩进作为新行缩进。
        let indent_end = motion::first_non_whitespace(&self.text, line_start);
        let indent = self.text.slice(line_start..indent_end).to_string();

        let insert_at = if below {
            self.text.line_end_offset(row)
        } else {
            line_start
        };
        let payload = if below {
            format!("\n{indent}")
        } else {
            format!("{indent}\n")
        };
        self.vim_replace_range(insert_at..insert_at, &payload, window, cx);

        let target = if below {
            insert_at + 1 + indent.len()
        } else {
            line_start + indent.len()
        };
        self.move_to(target.min(self.text.len()), None, cx);
        self.vim_switch_mode(VimMode::Insert, cx);
    }

    /// `~`：切换光标处字符的大小写，并把光标右移一格。
    pub(crate) fn vim_do_toggle_case(
        &mut self,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let row = self.text.offset_to_point(cursor).row;
        let line_end = self.text.line_content_end_offset(row);
        if cursor >= line_end {
            return;
        }

        // 计数按字符数作用，但**不跨行**（vim 的 `~` 在行尾提前停）。
        let mut swapped = String::new();
        let mut end = cursor;
        let mut toggled = 0usize;
        while toggled < count && end < line_end {
            let Some(ch) = self.text.get_char(end).ok() else {
                break;
            };
            if ch.is_uppercase() {
                swapped.extend(ch.to_lowercase());
            } else {
                swapped.extend(ch.to_uppercase());
            }
            end = self.text.clip_offset(end + 1, sum_tree::Bias::Right);
            toggled += 1;
        }
        if toggled == 0 {
            return;
        }

        self.vim_replace_range(cursor..end, &swapped, window, cx);
        let target = cursor + swapped.len();
        self.move_to(
            self.text.clip_offset(target, sum_tree::Bias::Left),
            None,
            cx,
        );
        self.vim_record_change(Change::ToggleCase { len: toggled });
    }

    /// 可视模式下的 `~`：切换整个选区的大小写并退出可视。
    pub(crate) fn vim_do_visual_toggle_case(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = self.vim_visual_range() else {
            self.vim_do_leave_visual(cx);
            return;
        };
        if range.is_empty() {
            self.vim_do_leave_visual(cx);
            return;
        }

        // 逐字符切换；结果长度可能与原文不同（如 `ß` → `SS`），
        // 所以整段替换而不是逐字符替换。
        let original = self.text.slice(range.clone()).to_string();
        let swapped: String = original
            .chars()
            .map(|ch| {
                if ch.is_uppercase() {
                    ch.to_lowercase().collect::<String>()
                } else {
                    ch.to_uppercase().collect::<String>()
                }
            })
            .collect();
        let len = range.len();

        self.vim_do_leave_visual(cx);
        self.vim_replace_range(range, &swapped, window, cx);
        self.vim_record_change(Change::ToggleCase { len });
    }

    /// `%`：跳到匹配的括号。
    ///
    /// 复用编辑器的括号匹配（与高亮同一套判定），但**优先看光标所在字符**：
    /// `matching_bracket` 会先检查 `cursor - 1`，当光标停在 `)` 上时会先命中
    /// 它左边那个 `(`，于是「跳转」变成原地不动。vim 的 `%` 以光标下的字符
    /// 为准，所以这里自己定目标字符，再取配对位置。
    pub(crate) fn vim_do_matching_bracket(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let under = self.text.get_char(cursor).ok();

        let Some((open, close)) = InputState::matching_bracket(&self.text, cursor) else {
            return;
        };

        // 依据光标下的字符决定跳到哪一端。
        let target = match under {
            Some('(') | Some('[') | Some('{') => close,
            Some(')') | Some(']') | Some('}') => open,
            // 光标不在括号上：`matching_bracket` 已用 `cursor - 1` 找到了一对，
            // 此时跳到闭合端（vim 在非括号字符上按 `%` 也是跳到前方括号）。
            _ => close,
        };

        if target != cursor {
            // 走可视感知的移动入口，否则可视模式下选区会塌缩而 visual_head 变陈旧。
            self.vim_move_in_mode(target, cx);
        }
    }

    /// 屏幕顶部/中部/底部的行首（`H` / `M` / `L`）。
    ///
    /// `fraction` 为 0.0 / 0.5 / 1.0。
    pub(crate) fn vim_do_window_line(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return;
        };
        let visible_rows = last_layout.lines.len();
        if visible_rows == 0 {
            return;
        }
        // 可见区第一行对应的 buffer 行。
        let first_offset = last_layout
            .visible_line_byte_offsets
            .first()
            .copied()
            .unwrap_or(0);
        let first_row = self.text.offset_to_point(first_offset).row;
        let offset_rows = ((visible_rows.saturating_sub(1)) as f32 * fraction).round() as usize;

        let target_row = (first_row + offset_rows).min(self.text.lines_len().saturating_sub(1));
        let line_start = self.text.line_start_offset(target_row);
        let target = motion::first_non_whitespace(&self.text, line_start);
        self.move_to(target, None, cx);
    }

    /// `H`：可见区第一行的首个非空白字符。
    pub(crate) fn vim_do_window_top(&mut self, cx: &mut Context<Self>) {
        self.vim_do_window_line(0.0, cx);
    }

    /// `M`：可见区中间行。
    pub(crate) fn vim_do_window_middle(&mut self, cx: &mut Context<Self>) {
        self.vim_do_window_line(0.5, cx);
    }

    /// `L`：可见区最后一行。
    pub(crate) fn vim_do_window_bottom(&mut self, cx: &mut Context<Self>) {
        self.vim_do_window_line(1.0, cx);
    }

    /// `ctrl-d` / `ctrl-u`：半页滚动，光标随之移动。
    pub(crate) fn vim_do_half_page(
        &mut self,
        down: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return;
        };
        let half = (last_layout.lines.len() / 2).max(1);
        let delta = if down {
            half as isize
        } else {
            -(half as isize)
        };

        // 可视模式下必须走可视感知的纵向移动，否则选区塌缩、visual_head 陈旧。
        if self.vim_mode_is_visual() {
            let target = self.vim_visual_vertical_target(delta);
            if let Some(vim) = self.vim.as_mut() {
                vim.visual_head = Some(target);
            }
            self.move_to(target, None, cx);
            InputState::vim_sync_visual_selection(self, cx);
            return;
        }
        self.move_vertical(delta, window, cx);
    }

    /// `*` / `#`：查找光标下的词。
    /// 复用编辑器的查找面板：先把光标下的词放进选区，再打开面板——面板的
    /// `show` 会用选区内容作为初始查询。这样 `n`/`N` 与面板内的查找共享
    /// 同一套匹配状态，不需要另造一套搜索。
    ///
    /// `forward` 目前只影响打开面板前的光标落点（面板自身总是从光标向后找）；
    /// 反向查找的完整语义（含 `n`/`N` 方向）留待搜索整合时统一处理。
    pub(crate) fn vim_do_search_word(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor();
        let Some(range) = crate::components::input::word_range_at(&self.text, cursor) else {
            return;
        };
        if range.is_empty() {
            return;
        }

        // 把词放进选区，供面板作为初始查询。
        self.selected_range = (range.start..range.end).into();
        self.selection_reversed = false;

        if !forward {
            // 反向查找：从词首开始，面板会向后找同一个词的后续出现；
            // 先退到词首让匹配顺序与 vim 的 `#` 一致。
            self.move_to(range.start, None, cx);
        }

        self.on_action_search(&crate::components::input::Search, window, cx);
        // `*`/`#` 的搜索面板默认大小写不敏感；vim 的 `ignorecase` 打开时才
        // 保持该行为，否则要显式改成大小写敏感。
        let ignore_case = self.vim.as_ref().is_some_and(|vim| vim.ignore_case);
        if let Some(panel) = self.search_panel.clone() {
            panel.update(cx, |panel, cx| {
                panel.set_case_insensitive(ignore_case, cx);
            });
        }
    }

    // ==================== 等待一个字符的命令 ====================

    /// 进入「等待一个字符」状态（`f`/`t`/`F`/`T`/`r`）。
    ///
    /// 等待状态存在 `VimState::pending_char` 而不是 `VimMode` 里：这样
    /// 「等字符」能与操作符栈正交组合（`dfa`、`dta` 都能工作）。
    pub(crate) fn vim_do_push_char(&mut self, kind: PendingChar, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_mut() else {
            cx.propagate();
            return;
        };
        vim.pending_char = Some(kind);
        cx.notify();
    }

    /// 收到待决字符时执行对应命令。返回 `true` 表示已消费。
    pub(crate) fn vim_do_apply_pending_char(
        &mut self,
        ch: char,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(kind) = self.vim.as_ref().and_then(|vim| vim.pending_char) else {
            return false;
        };
        if let Some(vim) = self.vim.as_mut() {
            vim.pending_char = None;
        }

        let cursor = self.cursor();
        match kind {
            PendingChar::Register => {
                // `"` 之后的字符是寄存器名。
                if let Some(vim) = self.vim.as_mut() {
                    vim.selected_register = Some(ch);
                }
                cx.notify();
                true
            }
            PendingChar::SetMark => {
                self.vim_do_set_mark(ch, cx);
                true
            }
            PendingChar::JumpMark { linewise } => {
                self.vim_do_jump_mark(ch, linewise, cx);
                true
            }
            PendingChar::Replace => {
                // `r`：把光标处字符换成输入的字符，光标留在原处。
                let row = self.text.offset_to_point(cursor).row;
                let line_end = self.text.line_content_end_offset(row);
                if cursor >= line_end {
                    cx.notify();
                    return true;
                }
                let end = self.text.clip_offset(cursor + 1, sum_tree::Bias::Right);
                let replacement = ch.to_string();
                self.vim_replace_range(cursor..end, &replacement, window, cx);
                // `replace_text_in_range_silent` 会把光标移到替换内容之后；
                // vim 的 `r` 不移动光标，所以显式移回原偏移。
                self.move_to(cursor, None, cx);
                self.vim_record_change(Change::Replace {
                    len: 1,
                    follow_motion: false,
                    backward: false,
                    text: replacement,
                });
                cx.notify();
                true
            }
            _ => {
                let forward = matches!(kind, PendingChar::FindForward | PendingChar::FindBefore);
                let before = matches!(kind, PendingChar::FindBefore | PendingChar::FindAfter);
                let Some(target) = motion::find_char(&self.text, cursor, ch, forward, before)
                else {
                    // 找不到：vim 里光标不动，但记住这次查找供 `;` 重复。
                    if let Some(vim) = self.vim.as_mut() {
                        vim.last_find = Some(LastFind { target: ch, kind });
                    }
                    cx.notify();
                    return true;
                };

                if let Some(vim) = self.vim.as_mut() {
                    vim.last_find = Some(LastFind { target: ch, kind });
                }

                // 有操作符待决时，`f`/`t` 也是操作符的作用范围。
                let motion_kind = if before {
                    MotionKind::Exclusive
                } else {
                    MotionKind::Inclusive
                };
                // `vim_try_operator` 只在有操作符时执行；返回 false 说明
                // 这是纯移动（`fo`），此时把光标移到目标。
                if !self.vim_try_operator(target, motion_kind, window, cx) {
                    self.move_to(target, None, cx);
                }
                true
            }
        }
    }

    /// `;` / `,`：重复上次字符查找（`,` 反向）。
    pub(crate) fn vim_do_repeat_find(
        &mut self,
        reversed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(last) = self.vim.as_ref().and_then(|vim| vim.last_find) else {
            return;
        };

        let forward = match last.kind {
            PendingChar::FindForward | PendingChar::FindBefore => !reversed,
            PendingChar::FindBackward | PendingChar::FindAfter => reversed,
            // `r`/`"`/`m`/`` ` `` 不进 `last_find`（见 `vim_do_apply_pending_char`），
            // 这里只是防御性分支。
            PendingChar::Replace
            | PendingChar::Register
            | PendingChar::SetMark
            | PendingChar::JumpMark { .. } => return,
        };
        let before = matches!(last.kind, PendingChar::FindBefore | PendingChar::FindAfter);

        let cursor = self.cursor();
        let Some(target) = motion::find_char(&self.text, cursor, last.target, forward, before)
        else {
            return;
        };

        let motion_kind = if before {
            MotionKind::Exclusive
        } else {
            MotionKind::Inclusive
        };
        if self.vim_try_operator(target, motion_kind, window, cx) {
            return;
        }
        self.move_to(target, None, cx);
    }

    /// `gc`：切换注释。
    pub(crate) fn vim_do_toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 复用编辑器的行注释切换（按语言选注释符）。
        self.toggle_comment(&crate::components::input::ToggleComment, window, cx);
    }

    // ==================== Visual 模式 ====================

    /// 进入可视模式。`linewise` 决定字符可视还是行可视。
    ///
    /// 锚点记在 `VimState::visual_anchor`，选区由「锚点 + 光标」表达：
    /// 编辑器的 `selected_range` 每帧从这两个值重算，所以移动光标就能
    /// 自然扩展选区，不需要在每次移动时手动改 `selected_range`。
    pub(crate) fn vim_do_enter_visual(&mut self, linewise: bool, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let mode = if linewise {
            VimMode::VisualLine
        } else {
            VimMode::Visual
        };

        if let Some(vim) = self.vim.as_mut() {
            // 已在可视模式时再次按 `v`/`V` 退出（vim 的行为）。
            if vim.mode == mode {
                // 记住 (锚点, 可视头, 模式) 供 `gv`。这里必须用 `visual_head`
                // 而不是 `cursor()`——后者是 `head + 1`，会让 `gv` 多恢复一格。
                let anchor = vim.visual_anchor.take();
                let head = vim.visual_head.take().unwrap_or(cursor);
                vim.switch_mode(VimMode::Normal);
                vim.last_visual = anchor.map(|a| (a, head, mode));
                self.selected_range = (head..head).into();
                self.selection_reversed = false;
                cx.notify();
                return;
            }
            vim.visual_anchor = Some(cursor);
            vim.visual_head = Some(cursor);
            vim.switch_mode(mode);
        }
        self.vim_sync_visual_selection(cx);
        cx.notify();
    }

    /// `gv`：重选上次的可视区。
    pub(crate) fn vim_do_reselect_visual(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((anchor, head, mode)) = self.vim.as_ref().and_then(|vim| vim.last_visual) else {
            return;
        };
        if let Some(vim) = self.vim.as_mut() {
            vim.visual_anchor = Some(anchor);
            vim.visual_head = Some(head);
            vim.switch_mode(mode);
        }
        // 光标落到可视头，再同步选区。
        self.move_to(head.min(self.text.len()), None, cx);
        let _ = window;
        self.vim_sync_visual_selection(cx);
        cx.notify();
    }

    /// `o`（Visual 下）：把光标移到选区另一端。
    pub(crate) fn vim_do_visual_swap_ends(&mut self, cx: &mut Context<Self>) {
        let Some((anchor, head)) = self
            .vim
            .as_ref()
            .and_then(|vim| Some((vim.visual_anchor?, vim.visual_head?)))
        else {
            return;
        };
        if let Some(vim) = self.vim.as_mut() {
            vim.visual_anchor = Some(head);
            vim.visual_head = Some(anchor);
        }
        self.move_to(anchor, None, cx);
        self.vim_sync_visual_selection(cx);
        cx.notify();
    }

    /// 依据锚点与光标重算编辑器的选区。
    ///
    /// 行可视（`V`）扩展到整行；字符可视是「锚点 → 光标」并**含光标字符**。
    ///
    /// 光标位置取自 [`VimState::visual_head`] 而不是 `self.cursor()`：
    /// `cursor()` 派生自 `selected_range.end`，而可视区为了「含尾」会把
    /// `end` 设成光标 +1，用 `cursor()` 当可视头会让每次移动累加一格。
    pub(crate) fn vim_sync_visual_selection(&mut self, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_ref() else {
            return;
        };
        let (Some(anchor), Some(head)) = (vim.visual_anchor, vim.visual_head) else {
            return;
        };
        let mode = vim.mode;
        if !mode.is_visual() {
            return;
        }

        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };

        let range = if mode == VimMode::VisualLine {
            let text = &self.text;
            let start_row = text.offset_to_point(start).row;
            let end_row = text.offset_to_point(end).row;
            let line_start = text.line_start_offset(start_row);
            let line_end = if end_row + 1 < text.lines_len() {
                text.line_start_offset(end_row + 1)
            } else {
                text.len()
            };
            line_start..line_end
        } else {
            // 字符可视：含光标所在字符。
            let text = &self.text;
            let end_inclusive = if end >= text.len() {
                text.len()
            } else {
                text.clip_offset(end + 1, sum_tree::Bias::Right)
            };
            start..end_inclusive
        };

        self.selected_range = (range.start..range.end).into();
        self.selection_reversed = anchor > head;
        cx.notify();
    }

    /// 退出可视模式并记住选区（供 `gv`）。
    pub(crate) fn vim_do_leave_visual(&mut self, cx: &mut Context<Self>) {
        // 折叠到**可视头**而不是 `cursor()`：可视模式下 `cursor()` 等于
        // `selected_range.end`，也就是 `head + 1`（选区含尾），
        // 直接用它会让光标落在最后选中字符的右侧一格。
        let head = self.vim_cursor();
        if let Some(vim) = self.vim.as_mut() {
            if let (Some(anchor), Some(head)) = (vim.visual_anchor.take(), vim.visual_head.take()) {
                let mode = vim.mode;
                vim.last_visual = Some((anchor, head, mode));
            }
            vim.switch_mode(VimMode::Normal);
        }
        // 折叠选区到光标处（vim 退出可视后不留选区）。
        self.selected_range = (head..head).into();
        self.selection_reversed = false;
        cx.notify();
    }

    /// 当前可视区的范围（供操作符使用）。
    ///
    /// 字符可视含头所在字符；行可视扩到整行。返回 `None` 表示不在可视模式
    /// 或没有锚点。
    pub(crate) fn vim_visual_range(&self) -> Option<std::ops::Range<usize>> {
        let vim = self.vim.as_ref()?;
        let (anchor, head) = (vim.visual_anchor?, vim.visual_head?);
        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };

        let text = &self.text;
        Some(if vim.mode == VimMode::VisualLine {
            let start_row = text.offset_to_point(start).row;
            let end_row = text.offset_to_point(end).row;
            let line_start = text.line_start_offset(start_row);
            let line_end = if end_row + 1 < text.lines_len() {
                text.line_start_offset(end_row + 1)
            } else {
                text.len()
            };
            line_start..line_end
        } else {
            let end_inclusive = if end >= text.len() {
                text.len()
            } else {
                text.clip_offset(end + 1, sum_tree::Bias::Right)
            };
            start..end_inclusive
        })
    }

    /// 可视模式下把当前选区交给操作符执行。
    ///
    /// 这是 `v` 之后按 `d`/`y`/`c` 的路径：选区已由
    /// [`Self::vim_sync_visual_selection`] 维护，这里直接取用。
    pub(crate) fn vim_do_visual_operator(
        &mut self,
        operator: Operator,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let linewise = self
            .vim
            .as_ref()
            .is_some_and(|vim| vim.mode == VimMode::VisualLine);
        // 范围取自「锚点 → 可视头」，含头所在字符（行可视则扩到整行）。
        let range = self.vim_visual_range().unwrap_or(0..0);
        if range.is_empty() {
            self.vim_do_leave_visual(cx);
            return;
        }

        let kind = if linewise {
            MotionKind::Linewise
        } else {
            MotionKind::Exclusive
        };

        // 可视删除按**宽度**重放（`4l` 之后 `.` 再删同样多的字符），
        // 方向在退出可视前算好：退出后 `cursor()` 会被折叠成 head，
        // 那时再比较就分不出方向了。
        let backward = self.vim.as_ref().is_some_and(
            |vim| matches!((vim.visual_anchor, vim.visual_head), (Some(a), Some(h)) if h < a),
        );

        // 先退出可视，再执行——`vim_execute` 会依据范围改文本并重设光标。
        self.vim_do_leave_visual(cx);
        self.vim_execute_visual(operator, range, kind, backward, window, cx);
    }

    /// 可视模式下 `>`/`<`：缩进或反缩进选中的行。
    ///
    /// 从**最后一行往前**逐行处理：前面的插入会平移后面行的偏移，倒序处理
    /// 则每行的行首位置在改动时仍然有效。
    pub(crate) fn vim_do_visual_indent(
        &mut self,
        indent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 范围取自锚点与可视头（含尾），行可视则扩到整行。
        let Some(range) = self.vim_visual_range() else {
            self.vim_do_leave_visual(cx);
            return;
        };
        if range.is_empty() {
            self.vim_do_leave_visual(cx);
            return;
        }

        let text = &self.text;
        let start_row = text.offset_to_point(range.start).row;
        // 范围末端若正好落在行首，说明那是下一行的开始，不属于选区。
        let end_point = text.offset_to_point(range.end);
        let mut end_row = end_point.row;
        if end_row > start_row && text.line_start_offset(end_row) == range.end {
            end_row -= 1;
        }

        let tab = self.tab_size;
        let unit = if tab.hard_tabs {
            "\t".to_string()
        } else {
            " ".repeat(tab.tab_size.max(1))
        };

        // 先退出可视，避免 `vim_replace_range` 期间的选区同步干扰。
        let (anchor_row, head_row) = (start_row, end_row);
        self.vim_do_leave_visual(cx);

        for row in (start_row..=end_row).rev() {
            let row_start = self.text.line_start_offset(row);
            if indent {
                self.vim_replace_range(row_start..row_start, &unit, window, cx);
            } else {
                let row_end = self.text.line_content_end_offset(row);
                let mut remove = 0;
                let mut offset = row_start;
                while offset < row_end && remove < unit.len() {
                    match self.text.get_char(offset).ok() {
                        Some(' ') => {
                            remove += 1;
                            offset += 1;
                        }
                        Some('\t') if remove == 0 => {
                            remove = 1;
                            break;
                        }
                        _ => break,
                    }
                }
                if remove > 0 {
                    self.vim_replace_range(row_start..row_start + remove, "", window, cx);
                }
            }
        }

        // 重新落回整行可视区，便于连续缩进（vim 里 `>` 后仍保持选中）。
        let last_row = self.text.lines_len().saturating_sub(1);
        let anchor = self.text.line_start_offset(anchor_row.min(last_row));
        let head = self.text.line_start_offset(head_row.min(last_row));
        if let Some(vim) = self.vim.as_mut() {
            vim.mode = VimMode::VisualLine;
            vim.visual_anchor = Some(anchor);
            vim.visual_head = Some(head);
        }
        self.move_to(head, None, cx);
        self.vim_sync_visual_selection(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_range_covers_identifier() {
        let text = ropey::Rope::from_str("let foo = 1;");
        assert_eq!(word_range_at(&text, 4), Some(4..7), "光标在词首");
        assert_eq!(word_range_at(&text, 6), Some(4..7), "光标在词中");
    }

    #[test]
    fn word_range_is_none_on_punctuation() {
        let text = ropey::Rope::from_str("a = b");
        assert_eq!(word_range_at(&text, 2), None, "`=` 不是词字符");
    }
}
