//! Vim action 的 handler。
//!
//! handler 是 `InputState` 的方法，由 `EditorInput` 渲染时经
//! `window.listener_for(&state, InputState::vim_xxx)` 接线——与既有的
//! `editor::MoveLeft` 等 action 走同一条派发路径。
//!
//! 每个 handler 都先取 vim 状态；`None`（vim 未启用）时不做任何事并
//! `propagate()`，让按键回到常规处理。

use gpui::{Context, Window};

use super::actions::*;
use super::motion;
use super::object::TextObject;
use super::state::{MotionKind, Operator, PendingChar, VimMode};
use crate::components::input::RopeExt as _;
use crate::components::input::state::InputState;

impl InputState {
    /// 取当前 vim 状态；未启用时返回 `None`。
    fn vim_mut(&mut self) -> Option<&mut super::VimState> {
        self.vim.as_mut()
    }

    /// 切换模式并通知重绘（光标形状与键位上下文都依赖模式）。
    ///
    /// 已经在目标模式时仍然清空待决状态：vim 里 `Esc` 在 Normal 模式下
    /// 的作用正是「丢弃未完成的 `d`/`y`/计数」，而不是空操作。
    pub(crate) fn vim_switch_mode(&mut self, mode: VimMode, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_mut() else {
            return;
        };
        let changed = vim.mode != mode;
        let was_insert = vim.mode == VimMode::Insert;
        vim.switch_mode(mode);
        if changed {
            if mode == VimMode::Insert {
                // 进入 Insert：整段输入合成一步撤销，并开始记录输入内容
                // 供 `.` 重放（vim 的 `.` 会重放整次 `cw` 的新文本）。
                self.history.start_grouping();
                self.vim_insert_start = Some(self.cursor());
                self.vim_insert_buffer.clear();
            } else {
                if was_insert {
                    // 离开 Insert 必须**成对**结束分组，否则 `grouping` 会一直
                    // 为真，之后 Normal 模式的每次编辑都落进同一个 undo 组，
                    // 一次 `u` 会把它们全部撤掉。
                    self.history.end_grouping();

                    // 把这次 Insert 的输入并进上次修改，供 `.` 重放。
                    let typed = std::mem::take(&mut self.vim_insert_buffer);
                    if !typed.is_empty() {
                        self.vim_record_insert(&typed);
                    }
                    self.vim_insert_start = None;
                }
                self.history.break_group();
            }
        }
        cx.notify();
    }

    /// 把一次 Insert 会话的输入并入「上次修改」。
    ///
    /// `cw` 之类会先记录 `Delete`，此时升级为 `Replace`（删 + 插），
    /// 这样 `.` 才能重现「替换」而不只是「删除」。
    fn vim_record_insert(&mut self, typed: &str) {
        use crate::vim::state::Change;
        let (previous, awaits_insert) = self
            .vim
            .as_ref()
            .map(|vim| (vim.last_change.clone(), vim.last_change_awaits_insert))
            .unwrap_or((None, false));

        // 只有 `cw`/`s`/`S` 这类「先删再进 Insert」的命令才把键入文本并进
        // 上一次删除。否则 `x` 之后 `i`+文本+`Esc` 会把无关的删除也升级成
        // 替换，`.` 会多删一个字符。
        let change = match (awaits_insert, previous) {
            (
                true,
                Some(Change::Delete {
                    len,
                    follow_motion,
                    backward,
                    ..
                }),
            ) => Change::Replace {
                len,
                follow_motion,
                backward,
                text: typed.to_string(),
            },
            _ => Change::Insert {
                text: typed.to_string(),
            },
        };
        self.vim_record_change(change);
    }

    // ==================== 模式切换 ====================

    pub(crate) fn vim_normal_mode(
        &mut self,
        _: &NormalMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vim.is_none() {
            cx.propagate();
            return;
        }

        // 从可视模式退出：必须走 `leave_visual`——它负责记住选区（供 `gv`）
        // 并清掉编辑器的选区。只调 `vim_switch_mode` 会绕过这两件事，
        // 结果是「退出可视但选区还在」，且 `gv` 无内容可恢复。
        if self.vim_mode_is_visual() {
            InputState::vim_do_leave_visual(self, cx);
            return;
        }

        // vim 的 `Esc` 把光标左移一格（插入点回到刚输入的字符上）。
        if self
            .vim
            .as_ref()
            .is_some_and(|vim| vim.mode == VimMode::Insert)
            && self.selected_range.is_empty()
        {
            let target = self.previous_boundary(self.cursor());
            self.move_to(target, None, cx);
        }
        self.vim_switch_mode(VimMode::Normal, cx);
    }

    pub(crate) fn vim_insert_before(
        &mut self,
        _: &InsertBefore,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_switch_mode(VimMode::Insert, cx);
    }

    pub(crate) fn vim_insert_after(
        &mut self,
        _: &InsertAfter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `a` = 光标右移一格后插入。行尾时不越过换行。
        let target = self.next_boundary(self.cursor());
        let line_end = self
            .text
            .line_content_end_offset(self.text.offset_to_point(self.cursor()).row);
        self.move_to(target.min(line_end), None, cx);
        self.vim_switch_mode(VimMode::Insert, cx);
    }

    pub(crate) fn vim_insert_first_non_whitespace(
        &mut self,
        _: &InsertFirstNonWhitespace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        let target = motion::first_non_whitespace(&self.text, self.vim_cursor());
        self.move_to(target, None, cx);
        self.vim_switch_mode(VimMode::Insert, cx);
    }

    pub(crate) fn vim_insert_end_of_line(
        &mut self,
        _: &InsertEndOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `A` 停在行尾之后（插入位置），与 `$` 不同。
        let row = self.text.offset_to_point(self.cursor()).row;
        let target = self.text.line_content_end_offset(row);
        self.move_to(target, None, cx);
        self.vim_switch_mode(VimMode::Insert, cx);
    }

    // ==================== 移动 ====================
    //
    // 每个移动都先检查待决操作符：有操作符时执行「操作符 + 移动」，
    // 否则只移动光标。这样 `d` 之后的 `w` 与单独的 `w` 走同一个 action，
    // 不需要为操作符待决状态复制一套移动键位。

    pub(crate) fn vim_left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim_try_operator(self.vim_motion_left(), MotionKind::Exclusive, window, cx) {
            return;
        }
        let target = self.previous_boundary(self.cursor());
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.vim_take_count();
        let from = self.vim_cursor();
        let target = self.vim_repeat_motion(from, count, |s, at| s.vim_motion_right_from(at));
        if self.vim_try_operator(target, MotionKind::Inclusive, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.vim_take_count();
        let lines = count as isize;
        // `dj` / `dk` 是整行操作，所以先用 Linewise 语义。
        if self.vim_has_pending_operator() {
            let target = self.vim_motion_vertical(lines, window, cx);
            self.vim_try_operator(target, MotionKind::Linewise, window, cx);
            return;
        }
        self.vim_move_vertical_in_mode(lines, window, cx);
    }

    pub(crate) fn vim_up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.vim_take_count();
        let lines = -(count as isize);
        if self.vim_has_pending_operator() {
            let target = self.vim_motion_vertical(lines, window, cx);
            self.vim_try_operator(target, MotionKind::Linewise, window, cx);
            return;
        }
        self.vim_move_vertical_in_mode(lines, window, cx);
    }

    pub(crate) fn vim_next_word_start(
        &mut self,
        _: &NextWordStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.vim_take_count();
        let from = self.vim_cursor();
        let target =
            self.vim_repeat_motion(from, count, |s, at| motion::next_word_start(&s.text, at));
        if self.vim_try_operator(target, MotionKind::Exclusive, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_previous_word_start(
        &mut self,
        _: &PreviousWordStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.vim_take_count();
        let from = self.vim_cursor();
        let target = self.vim_repeat_motion(from, count, |s, at| {
            motion::previous_word_start(&s.text, at)
        });
        if self.vim_try_operator(target, MotionKind::Exclusive, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_next_word_end(
        &mut self,
        _: &NextWordEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.vim_take_count();
        let from = self.vim_cursor();
        let target =
            self.vim_repeat_motion(from, count, |s, at| motion::next_word_end(&s.text, at));
        if self.vim_try_operator(target, MotionKind::Inclusive, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_start_of_line(
        &mut self,
        _: &StartOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        let target = motion::start_of_line(&self.text, self.vim_cursor());
        if self.vim_try_operator(target, MotionKind::Exclusive, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_end_of_line(
        &mut self,
        _: &EndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        let target = motion::end_of_line(&self.text, self.vim_cursor());
        if self.vim_try_operator(target, MotionKind::Inclusive, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_start_of_document(
        &mut self,
        _: &StartOfDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        if self.vim_try_operator(0, MotionKind::Linewise, window, cx) {
            return;
        }
        self.vim_move_in_mode(0, cx);
    }

    pub(crate) fn vim_end_of_document(
        &mut self,
        _: &EndOfDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        let target = self
            .text
            .clip_offset(self.text.len().saturating_sub(1), sum_tree::Bias::Left);
        if self.vim_try_operator(target, MotionKind::Linewise, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    // ==================== 操作符 ====================

    pub(crate) fn vim_push_delete(
        &mut self,
        _: &PushDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_push_or_repeat_line(Operator::Delete, window, cx);
    }

    pub(crate) fn vim_push_yank(
        &mut self,
        _: &PushYank,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_push_or_repeat_line(Operator::Yank, window, cx);
    }

    pub(crate) fn vim_push_change(
        &mut self,
        _: &PushChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_push_or_repeat_line(Operator::Change, window, cx);
    }

    pub(crate) fn vim_push_object_inner(
        &mut self,
        _: &PushObjectInner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_push_object(false, window, cx);
    }

    pub(crate) fn vim_push_object_around(
        &mut self,
        _: &PushObjectAround,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_push_object(true, window, cx);
    }

    /// `i`/`a` 前缀的两种归宿。
    ///
    /// 没有待决操作符时，`i` 是「插入」命令——vim 里 `i` 的双重身份正是靠
    /// 操作符栈区分，所以这里必须分流，否则 `diw` 会变成「进入 Insert」。
    fn vim_push_object(&mut self, around: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.vim_has_pending_operator() {
            // 无操作符：`i` = 光标前插入，`a` = 光标后插入。
            if around {
                self.vim_insert_after(&InsertAfter, window, cx);
            } else {
                self.vim_insert_before(&InsertBefore, window, cx);
            }
            return;
        }

        // 已有对象前缀（`di` 之后再按 `i`）：vim 把它当未完成输入，忽略。
        if self.vim_object_prefix().is_some() {
            return;
        }

        if let Some(vim) = self.vim_mut() {
            vim.push_operator(Operator::Object { around });
        }
        cx.notify();
    }

    /// 对象名（`w`）：完成 `diw` / `daw` / `ciw` 这类组合。
    ///
    /// 由键位表把对象前缀状态下的 `w` 绑定到本 action。
    pub(crate) fn vim_object_word(
        &mut self,
        _: &ObjectWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vim_object_key('w', window, cx);
    }

    /// 对象名的通用入口：`di"`、`da(`、`dip`、`cit` 等。
    ///
    /// 键位表把对象前缀状态下的每个对象键绑到 [`ObjectKey`]（带参数），
    /// 这里统一解析并执行。找不到对象时按 vim 语义「什么都不做」。
    pub(crate) fn vim_object_key_action(
        &mut self,
        action: &ObjectKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ch) = action.key.chars().next() else {
            return;
        };
        self.vim_object_key(ch, window, cx);
    }

    /// 按对象键字符执行对象操作。
    fn vim_object_key(&mut self, key: char, window: &mut Window, cx: &mut Context<Self>) {
        let Some(around) = self.vim_object_prefix() else {
            // 没有对象前缀时该键是普通命令（例如 `w` 是移动）；
            // 走到这里说明上下文与键位表不一致，按移动处理更安全。
            if key == 'w' {
                self.vim_next_word_start(&NextWordStart, window, cx);
            }
            return;
        };
        let Some(object) = TextObject::from_key(key) else {
            return;
        };
        let Some(operator) = self.vim_take_operator() else {
            return;
        };
        self.vim_apply_operator_object(operator, object, around, window, cx);
    }

    /// `x`：删除光标处字符。
    pub(crate) fn vim_delete_char(
        &mut self,
        _: &DeleteChar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.vim_take_count();
        InputState::vim_do_delete_char(self, count, window, cx);
    }

    /// 计数前缀：`3` 记入 pre_count，`d` 之后的 `2` 记入 post_count。
    ///
    /// 十个数字 action 共用一个处理函数——它们的区别只在数字本身。
    pub(crate) fn vim_count_digit(&mut self, digit: usize, cx: &mut Context<Self>) {
        let Some(vim) = self.vim_mut() else {
            cx.propagate();
            return;
        };
        vim.push_count_digit(digit);
        cx.notify();
    }

    pub(crate) fn vim_count_1(&mut self, _: &Count1, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(1, cx);
    }
    pub(crate) fn vim_count_2(&mut self, _: &Count2, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(2, cx);
    }
    pub(crate) fn vim_count_3(&mut self, _: &Count3, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(3, cx);
    }
    pub(crate) fn vim_count_4(&mut self, _: &Count4, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(4, cx);
    }
    pub(crate) fn vim_count_5(&mut self, _: &Count5, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(5, cx);
    }
    pub(crate) fn vim_count_6(&mut self, _: &Count6, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(6, cx);
    }
    pub(crate) fn vim_count_7(&mut self, _: &Count7, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(7, cx);
    }
    pub(crate) fn vim_count_8(&mut self, _: &Count8, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(8, cx);
    }
    pub(crate) fn vim_count_9(&mut self, _: &Count9, _: &mut Window, cx: &mut Context<Self>) {
        self.vim_count_digit(9, cx);
    }

    /// `p` / `P`。
    pub(crate) fn vim_paste_after(
        &mut self,
        _: &PasteAfter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_paste(self, true, window, cx);
    }

    pub(crate) fn vim_paste_before(
        &mut self,
        _: &PasteBefore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_paste(self, false, window, cx);
    }

    /// `J`：连接下一行。
    pub(crate) fn vim_join_lines(
        &mut self,
        _: &JoinLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_join_lines(self, window, cx);
    }

    /// `>>` / `<<`：整行缩进 / 反缩进。
    pub(crate) fn vim_indent(
        &mut self,
        _: &IndentLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_indent_line(self, true, window, cx);
    }

    pub(crate) fn vim_outdent(
        &mut self,
        _: &OutdentLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_indent_line(self, false, window, cx);
    }

    // ==================== 撤销 / 重做 ====================

    pub(crate) fn vim_undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        InputState::vim_do_undo(self, window, cx);
    }

    pub(crate) fn vim_redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        InputState::vim_do_redo(self, window, cx);
    }

    // ==================== 行内快捷命令 ====================

    pub(crate) fn vim_delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_delete_to_end_of_line(self, window, cx);
    }

    pub(crate) fn vim_change_to_end_of_line(
        &mut self,
        _: &ChangeToEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_change_to_end_of_line(self, window, cx);
    }

    pub(crate) fn vim_yank_line(
        &mut self,
        _: &YankLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_yank_line(self, window, cx);
    }

    pub(crate) fn vim_substitute_char(
        &mut self,
        _: &SubstituteChar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_substitute_char(self, window, cx);
    }

    pub(crate) fn vim_substitute_line(
        &mut self,
        _: &SubstituteLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_substitute_line(self, window, cx);
    }

    pub(crate) fn vim_open_line_below(
        &mut self,
        _: &OpenLineBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_open_line(self, true, window, cx);
    }

    pub(crate) fn vim_open_line_above(
        &mut self,
        _: &OpenLineAbove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_open_line(self, false, window, cx);
    }

    pub(crate) fn vim_toggle_case(
        &mut self,
        _: &ToggleCase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.vim_take_count();
        if self.vim_mode_is_visual() {
            InputState::vim_do_visual_toggle_case(self, window, cx);
            return;
        }
        InputState::vim_do_toggle_case(self, count, window, cx);
    }

    pub(crate) fn vim_matching_bracket(
        &mut self,
        _: &MatchingBracket,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_matching_bracket(self, cx);
    }

    pub(crate) fn vim_first_non_whitespace(
        &mut self,
        _: &FirstNonWhitespace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        let target = motion::first_non_whitespace(&self.text, self.vim_cursor());
        if self.vim_try_operator(target, MotionKind::Exclusive, window, cx) {
            return;
        }
        self.vim_move_in_mode(target, cx);
    }

    pub(crate) fn vim_first_non_whitespace_down(
        &mut self,
        _: &FirstNonWhitespaceDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.vim_motion_vertical(1, window, cx);
        let target = motion::first_non_whitespace(&self.text, target);
        if self.vim_try_operator(target, MotionKind::Linewise, window, cx) {
            return;
        }
        self.move_to(target, None, cx);
    }

    // ==================== 翻页与窗口定位 ====================

    pub(crate) fn vim_half_page_down(
        &mut self,
        _: &HalfPageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        InputState::vim_do_half_page(self, true, window, cx);
    }

    pub(crate) fn vim_half_page_up(
        &mut self,
        _: &HalfPageUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.vim_take_count();
        InputState::vim_do_half_page(self, false, window, cx);
    }

    pub(crate) fn vim_page_down(
        &mut self,
        _: &PageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.page_down(&crate::components::input::MovePageDown, window, cx);
    }

    pub(crate) fn vim_page_up(&mut self, _: &PageUp, window: &mut Window, cx: &mut Context<Self>) {
        self.page_up(&crate::components::input::MovePageUp, window, cx);
    }

    pub(crate) fn vim_window_top(&mut self, _: &WindowTop, _: &mut Window, cx: &mut Context<Self>) {
        InputState::vim_do_window_top(self, cx);
    }

    pub(crate) fn vim_window_middle(
        &mut self,
        _: &WindowMiddle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_window_middle(self, cx);
    }

    pub(crate) fn vim_window_bottom(
        &mut self,
        _: &WindowBottom,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_window_bottom(self, cx);
    }

    // ==================== 查找 ====================

    pub(crate) fn vim_search_word_forward(
        &mut self,
        _: &SearchWordForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_search_word(self, true, window, cx);
    }

    pub(crate) fn vim_search_word_backward(
        &mut self,
        _: &SearchWordBackward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_search_word(self, false, window, cx);
    }

    pub(crate) fn vim_search_next(
        &mut self,
        _: &SearchNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 复用查找面板的「下一个」。
        if let Some(panel) = self.search_panel.clone() {
            panel.update(cx, |panel, cx| {
                panel.on_action_enter(
                    &crate::components::input::Enter { secondary: false },
                    window,
                    cx,
                );
            });
        }
    }

    pub(crate) fn vim_search_prev(
        &mut self,
        _: &SearchPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.search_panel.clone() {
            panel.update(cx, |panel, cx| {
                panel.on_action_search_previous(
                    &crate::components::input::SearchPrevious,
                    window,
                    cx,
                );
            });
        }
    }

    // ==================== 等待一个字符的命令 ====================

    pub(crate) fn vim_push_find_forward(
        &mut self,
        _: &PushFindForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_char(self, PendingChar::FindForward, cx);
    }

    pub(crate) fn vim_push_find_before(
        &mut self,
        _: &PushFindBefore,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_char(self, PendingChar::FindBefore, cx);
    }

    pub(crate) fn vim_push_find_backward(
        &mut self,
        _: &PushFindBackward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_char(self, PendingChar::FindBackward, cx);
    }

    pub(crate) fn vim_push_find_after(
        &mut self,
        _: &PushFindAfter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_char(self, PendingChar::FindAfter, cx);
    }

    pub(crate) fn vim_push_replace(
        &mut self,
        _: &PushReplace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_char(self, PendingChar::Replace, cx);
    }

    pub(crate) fn vim_repeat_find(
        &mut self,
        _: &RepeatFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_repeat_find(self, false, window, cx);
    }

    pub(crate) fn vim_repeat_find_reversed(
        &mut self,
        _: &RepeatFindReversed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_repeat_find(self, true, window, cx);
    }

    pub(crate) fn vim_toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_toggle_comment(self, window, cx);
    }

    // ==================== Visual 模式 ====================

    pub(crate) fn vim_visual_char(
        &mut self,
        _: &VisualChar,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_enter_visual(self, false, cx);
    }

    pub(crate) fn vim_visual_line(
        &mut self,
        _: &VisualLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_enter_visual(self, true, cx);
    }

    pub(crate) fn vim_visual_swap_ends(
        &mut self,
        _: &VisualSwapEnds,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_visual_swap_ends(self, cx);
    }

    pub(crate) fn vim_reselect_visual(
        &mut self,
        _: &ReselectVisual,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_reselect_visual(self, window, cx);
    }

    /// `"`：等待一个寄存器名。
    pub(crate) fn vim_push_register(
        &mut self,
        _: &PushRegister,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_char(self, PendingChar::Register, cx);
    }

    /// `m`：等待标记名（写入）。
    pub(crate) fn vim_push_set_mark(
        &mut self,
        _: &PushSetMark,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_set_mark(self, cx);
    }

    /// `` ` ``：跳到标记的精确位置。
    pub(crate) fn vim_push_jump_mark(
        &mut self,
        _: &PushJumpMark,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_jump_mark(self, false, cx);
    }

    /// `'`：跳到标记所在行的首个非空白字符。
    pub(crate) fn vim_push_jump_mark_line(
        &mut self,
        _: &PushJumpMarkLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_push_jump_mark(self, true, cx);
    }

    /// `.`：重复上次修改。
    pub(crate) fn vim_repeat_change(
        &mut self,
        _: &RepeatChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_repeat_change(self, window, cx);
    }

    /// 可视模式下的 `d` / `y` / `c` / `x` / `s`。
    pub(crate) fn vim_visual_delete(
        &mut self,
        _: &VisualDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_visual_operator(self, Operator::Delete, window, cx);
    }

    pub(crate) fn vim_visual_yank(
        &mut self,
        _: &VisualYank,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_visual_operator(self, Operator::Yank, window, cx);
    }

    pub(crate) fn vim_visual_change(
        &mut self,
        _: &VisualChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_visual_operator(self, Operator::Change, window, cx);
    }

    /// 可视模式下的 `>` / `<`。
    pub(crate) fn vim_visual_indent(
        &mut self,
        _: &VisualIndent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_visual_indent(self, true, window, cx);
    }

    pub(crate) fn vim_visual_outdent(
        &mut self,
        _: &VisualOutdent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        InputState::vim_do_visual_indent(self, false, window, cx);
    }

    /// `dd` / `yy` / `cc`：重复按同一个操作符 = 整行。
    ///
    /// 在 `vim_push_*` 里检测「栈顶已是同一个操作符」即可实现，不需要为
    /// `dd` 单独绑定键位——这样 `yy`、`cc` 自动获得同样语义。
    fn vim_push_or_repeat_line(
        &mut self,
        operator: Operator,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repeat = self
            .vim
            .as_ref()
            .and_then(|vim| vim.operator_stack.last())
            .is_some_and(|top| *top == operator);
        if repeat {
            // `2dd`：计数按行数作用。
            let count = self.vim_take_count();
            self.vim_line_operator(operator, count, window, cx);
        } else {
            self.vim_push_operator(operator, cx);
        }
    }

    // ==================== 操作符执行辅助 ====================

    /// 移动光标；可视模式下同步扩展选区。
    ///
    /// 可视模式的选区是「锚点 + 光标」的派生值，所以每次移动后都要重算。
    /// 统一走这个入口，避免逐个移动命令各写一遍同步逻辑（漏掉任何一个都会
    /// 表现为「光标走了但选区没动」）。
    ///
    /// 可视模式下把新位置记进 `visual_head`：`selected_range.end` 为了「含尾」
    /// 比光标大一格，不能反过来当作光标位置。
    ///
    /// 调用方必须已经取走计数——残留的计数会被下一个操作符乘进去。
    pub(crate) fn vim_move_in_mode(&mut self, target: usize, cx: &mut Context<Self>) {
        if self.vim_mode_is_visual() {
            if let Some(vim) = self.vim.as_mut() {
                vim.visual_head = Some(target);
            }
            // 可视模式下光标不能停在行尾换行之后，钳到行内容末尾。
            self.move_to(target, None, cx);
            InputState::vim_sync_visual_selection(self, cx);
            return;
        }
        self.move_to(target, None, cx);
    }

    /// 纵向移动；可视模式下同步选区。
    fn vim_move_vertical_in_mode(
        &mut self,
        lines: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vim_mode_is_visual() {
            // 纵向移动按「目标行 + 原列」算，结果记进 `visual_head`。
            let target = self.vim_visual_vertical_target(lines);
            if let Some(vim) = self.vim.as_mut() {
                vim.visual_head = Some(target);
            }
            self.move_to(target, None, cx);
            InputState::vim_sync_visual_selection(self, cx);
            return;
        }
        self.move_vertical(lines, window, cx);
    }

    /// 可视模式下纵向移动的落点：同列、上/下一行，行短则钳到行尾字符。
    pub(crate) fn vim_visual_vertical_target(&self, lines: isize) -> usize {
        let text = &self.text;
        let head = self
            .vim
            .as_ref()
            .and_then(|vim| vim.visual_head)
            .unwrap_or(0);
        let row = text.offset_to_point(head).row;
        let column = head - text.line_start_offset(row);
        let total = text.lines_len();
        let target_row = if lines >= 0 {
            (row + lines as usize).min(total.saturating_sub(1))
        } else {
            row.saturating_sub((-lines) as usize)
        };
        let line_start = text.line_start_offset(target_row);
        let line_end = motion::end_of_line(text, line_start);
        (line_start + column).min(line_end)
    }

    pub(crate) fn vim_mode_is_visual(&self) -> bool {
        self.vim.as_ref().is_some_and(|vim| vim.mode.is_visual())
    }

    /// 有待决操作符时执行「操作符 + 移动」，否则返回 false 让调用方移动光标。
    pub(crate) fn vim_try_operator(
        &mut self,
        target: usize,
        kind: MotionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // 对象前缀（`i`/`a`）后面必须跟对象名，不是移动。
        let Some(operator) = self.vim_peek_operator() else {
            return false;
        };
        if matches!(operator, Operator::Object { .. }) {
            return false;
        }
        // 计数由调用方折进 `target`（见 `vim_repeat_motion`），这里不再
        // 重复应用——否则 `2dw` 会把计数作用两次，终点一路走到缓冲区末尾。
        self.vim_apply_operator(operator, target, kind, window, cx);
        true
    }

    fn vim_has_pending_operator(&self) -> bool {
        self.vim_peek_operator().is_some()
    }

    /// 读栈顶操作符但不弹出。
    fn vim_peek_operator(&self) -> Option<Operator> {
        let vim = self.vim.as_ref()?;
        vim.operator_stack
            .iter()
            .rev()
            .find(|operator| !matches!(operator, Operator::Object { .. }))
            .cloned()
    }

    /// 取走计数并清空（`3j` 里的 `3`）。
    ///
    /// 每个会用到计数的命令都必须调用它，**即使不使用返回值**：否则残留的
    /// 计数会被后续命令误用（`3j` 之后 `dw` 会删 3 个词）。
    pub(crate) fn vim_take_count(&mut self) -> usize {
        self.vim.as_mut().map(|vim| vim.take_count()).unwrap_or(1)
    }

    /// 把一次「单步移动」重复 `count` 次，返回最终落点。
    ///
    /// 计数作用在**移动**上而不是范围长度上：`2w` 与「范围翻倍」在多行时
    /// 并不等价。`count == 1` 时也走一次 `step`——它才是移动本身。
    fn vim_repeat_motion(
        &self,
        mut from: usize,
        count: usize,
        step: impl Fn(&Self, usize) -> usize,
    ) -> usize {
        for _ in 0..count.max(1) {
            let next = step(self, from);
            if next == from {
                break;
            }
            from = next;
        }
        from
    }

    // ---- 纯移动目标（不改状态，供操作符复用） ----

    /// 当前「逻辑光标」：可视模式下是 `visual_head`，否则是编辑器光标。
    ///
    /// 所有移动都以它为起点。不能用 `self.cursor()` 代替——可视区为了含尾
    /// 把 `selected_range.end` 设成 head + 1，而 `cursor()` 正是从那里派生的，
    /// 于是每次移动都会多走一格。
    /// `l` 的单步落点（供计数重复使用）。
    fn vim_motion_right_from(&self, from: usize) -> usize {
        let line_end = motion::end_of_line(&self.text, from);
        self.next_boundary(from).min(line_end)
    }

    pub(crate) fn vim_cursor(&self) -> usize {
        self.vim
            .as_ref()
            .filter(|vim| vim.mode.is_visual())
            .and_then(|vim| vim.visual_head)
            .unwrap_or_else(|| self.cursor())
    }

    fn vim_motion_left(&self) -> usize {
        self.previous_boundary(self.vim_cursor())
    }

    /// 纵向移动的落点。复用 `move_vertical` 的目标计算需要真实移动，
    /// 所以这里退化为「同列、上/下一行」——行长度不足时钳到行尾。
    fn vim_motion_vertical(
        &self,
        lines: isize,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> usize {
        let text = &self.text;
        let cursor = self.cursor();
        let row = text.offset_to_point(cursor).row;
        let column = cursor - text.line_start_offset(row);

        let total = text.lines_len();
        let target_row = if lines >= 0 {
            (row + lines as usize).min(total.saturating_sub(1))
        } else {
            row.saturating_sub((-lines) as usize)
        };

        let line_start = text.line_start_offset(target_row);
        let line_end = motion::end_of_line(text, line_start);
        (line_start + column).min(line_end)
    }

    fn vim_push_operator(&mut self, operator: Operator, cx: &mut Context<Self>) {
        let Some(vim) = self.vim_mut() else {
            cx.propagate();
            return;
        };
        vim.push_operator(operator);
        // 键位上下文里的 `vim_operator` 变了，需要重绘以刷新上下文。
        cx.notify();
    }
}
