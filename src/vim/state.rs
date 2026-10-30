//! Vim 状态。
//!
//! 关键设计：**模式与待决状态分离**。
//!
//! `VimMode` 只表达编辑语义（Normal / Insert / Replace / Visual / VisualLine），
//! 没有「操作符待决」「等计数」「等寄存器」这类变体。那些状态各自独立存放：
//!
//! | 待决状态 | 存放位置 | 例子 |
//! |---|---|---|
//! | 操作符与对象前缀 | [`VimState::operator_stack`] | `d` 压 `Delete`，`i` 压 `Object` |
//! | 计数 | [`VimState::pending_count`] | `3` 进 pre，`d` 之后的 `2` 进 post |
//! | 寄存器选择 | [`VimState::selected_register`] | `"a` |
//! | 等待一个字符 | 由 `Operator` 变体自身携带 | `f`/`r`/`m` |
//!
//! 这样 `3d2w`、`"ayy`、`ci"` 这类组合是「栈 + 计数」的自然结果，
//! 而不需要为每种前缀组合新增一个模式。

use crate::components::input::CursorShape;
use crate::settings::VimContent;

/// 编辑语义上的模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    #[default]
    Normal,
    Insert,
    Replace,
    /// 字符可视。
    Visual,
    /// 行可视（`V`）。
    VisualLine,
}

impl VimMode {
    pub fn is_visual(self) -> bool {
        matches!(self, VimMode::Visual | VimMode::VisualLine)
    }

    /// 状态栏显示文本，对齐 vim 的 `showmode`。
    pub fn indicator(self) -> &'static str {
        match self {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Replace => "REPLACE",
            VimMode::Visual => "VISUAL",
            VimMode::VisualLine => "VISUAL LINE",
        }
    }
}

/// 操作符的选区语义。
///
/// 决定「操作符 + 移动」作用的实际范围，是 vim 兼容性的核心：
/// `dj` 删两行（Linewise），`dw` 到词首前（Exclusive），`d$` 含行尾字符（Inclusive）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    /// 整行，如 `j`/`k`/`gg`。
    Linewise,
    /// 不含终点的字符，如 `w`/`b`/`0`。
    Exclusive,
    /// 含终点的字符，如 `$`/`e`/`f`。
    Inclusive,
}

/// 待决的操作符。
///
/// 本阶段只立骨架：`Delete`/`Yank`/`Change`/`Object` 用于验证上下文分发，
/// 实际执行在阶段 1 补齐。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Yank,
    Change,
    /// `i` / `a` 对象前缀。
    Object {
        around: bool,
    },
}

/// 一次可重复的修改（供 `.` 重放）。
///
/// 记录的是**修改本身**（删了什么、插了什么）而不是按键序列：
/// 重放按键需要重新解析上下文（`d` 之后的 `w` 与单独的 `w` 不同），
/// 而记录修改只需在新光标处重放同样的编辑动作。
///
/// 这是 Zed 的做法（它记 `ReplayableAction`），也是唯一能保证
/// `.` 在光标移动后仍语义正确的方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// 删除一段文本。
    Delete {
        /// 被删文本的长度。
        len: usize,
        /// 该次删除是否**跟随移动语义**（`dw`、`cw` 这类）。
        ///
        /// 跟随移动的删除在重放时必须按新光标位置重新计算范围——
        /// vim 的 `.` 重放 `cw` 是「再改一个词」，而不是「再删 3 个字符」。
        /// 只有 `x`、`dd` 这类定长删除才用记录的长度。
        follow_motion: bool,
        /// 跟随移动时的方向：`true` 表示向后（`db`、`d0`），`false` 向前。
        ///
        /// 没有它，`. ` 重放 `db` 会朝前删——方向反了。
        backward: bool,
        /// 删的是否为整行。
        linewise: bool,
    },
    /// 插入一段文本。
    Insert { text: String },
    /// 删除并插入（`cw`、`ci"`、`r` 等）。
    Replace {
        len: usize,
        follow_motion: bool,
        backward: bool,
        text: String,
    },
    /// 整行缩进 / 反缩进。
    Indent { outdent: bool, lines: usize },
    /// 连接行。
    JoinLines,
    /// 切换大小写。
    ToggleCase { len: usize },
}

/// 待决的字符输入（`f`/`t`/`F`/`T`/`r` 之后的那个字符）。
///
/// vim 里这些命令要等一个字符才能完成，等待状态由本枚举承载，而不是给
/// `VimMode` 加变体——这样「等字符」可以与操作符栈正交组合（`dfa`、`dta`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingChar {
    /// `f`：向后移动到字符上。
    FindForward,
    /// `t`：向后移动到字符前。
    FindBefore,
    /// `F`：向前移动到字符上。
    FindBackward,
    /// `T`：向前移动到字符后。
    FindAfter,
    /// `r`：把光标处字符替换为输入字符。
    Replace,
    /// `"`：等待一个寄存器名。
    Register,
    /// `m`：等待一个标记名（写入）。
    SetMark,
    /// `` ` `` / `'`：等待一个标记名（跳转）。
    JumpMark { linewise: bool },
}

/// 最近一次字符查找，供 `;` / `,` 重复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastFind {
    pub target: char,
    pub kind: PendingChar,
}

#[derive(Debug, Clone)]
pub struct VimState {
    pub mode: VimMode,
    /// 上一个模式，供 `gv` 之类的「回到上次可视区」使用。
    pub last_mode: VimMode,
    /// 操作符栈。空表示没有待决操作符。
    pub operator_stack: Vec<Operator>,
    /// 计数前缀。操作符出现前记在 `pre_count`，出现后记在 `post_count`。
    pub pre_count: Option<usize>,
    pub post_count: Option<usize>,
    /// 待写入/读取的寄存器名。
    pub selected_register: Option<char>,
    /// 等待一个字符的待决命令（`f`/`t`/`r`）。
    pub pending_char: Option<PendingChar>,
    /// 最近一次字符查找，供 `;`/`,` 重复。
    pub last_find: Option<LastFind>,
    /// Visual 模式的锚点（进入可视时的光标位置）。
    ///
    /// vim 的选区是「锚点 + 光标」，移动光标时锚点不动。
    pub visual_anchor: Option<usize>,
    /// Visual 模式的光标位置。
    ///
    /// 必须与锚点分开存：`cursor()` 从 `selected_range.end` 派生，而可视区
    /// 要**包含**光标所在字符（所以 `selected_range.end` 比光标大一格）。
    /// 若直接用 `cursor()` 当可视头，每次移动都会把「含尾」的那一格
    /// 当成新起点，位置会逐次累加跑偏。
    pub visual_head: Option<usize>,
    /// 上次退出的 Visual 选区（锚点, 光标, 模式），供 `gv` 恢复。
    pub last_visual: Option<(usize, usize, VimMode)>,
    /// 等待一个寄存器的写入/读取（`"a` 之后）。
    pub pending_register: bool,
    /// 上次修改，供 `.` 重放。
    pub last_change: Option<Change>,
    /// 上次修改是否是「删 + 插入」型的半成品（`cw`/`s`/`S` 等）。
    ///
    /// 这些命令先删再进 Insert，退出 Insert 时把键入文本合并成 `Replace`。
    /// 只有它们才该被合并——否则 `x` 之后 `i`+文本+`Esc` 会把无关的删除
    /// 也升级成替换，`.` 会多删一个字符。
    pub last_change_awaits_insert: bool,
    /// Normal 模式的光标形状（可配）。
    pub normal_cursor_shape: CursorShape,
    /// 是否随模式切换相对行号。
    pub relative_line_numbers: bool,
    /// `:set ignorecase`：搜索/替换是否忽略大小写。
    pub ignore_case: bool,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            mode: VimMode::Normal,
            last_mode: VimMode::Normal,
            operator_stack: Vec::new(),
            pre_count: None,
            post_count: None,
            selected_register: None,
            pending_char: None,
            last_find: None,
            visual_anchor: None,
            visual_head: None,
            last_visual: None,
            pending_register: false,
            last_change: None,
            last_change_awaits_insert: false,
            normal_cursor_shape: CursorShape::Block,
            relative_line_numbers: false,
            ignore_case: false,
        }
    }
}

impl VimState {
    pub fn from_settings(settings: &VimContent) -> Self {
        let mode = match settings.default_mode.as_str() {
            "insert" => VimMode::Insert,
            // 未知值回落到 Normal：vim 的默认起点，也是唯一不会让用户
            // 「以为坏了」的选项。
            _ => VimMode::Normal,
        };

        Self {
            mode,
            last_mode: mode,
            normal_cursor_shape: parse_cursor_shape(&settings.normal_cursor_shape),
            relative_line_numbers: settings.relative_line_numbers,
            ..Self::default()
        }
    }

    /// 切换模式，保留上一个模式。
    ///
    /// 切换会清空待决状态——vim 里 `i` 或 `Esc` 都会丢弃未完成的操作符与计数，
    /// 否则 `d` 之后误按 `i` 会留下一个悬空的操作符。
    /// 已在目标模式时只清待决状态，不覆盖 `last_mode`。
    pub fn switch_mode(&mut self, mode: VimMode) {
        if self.mode != mode {
            self.last_mode = self.mode;
            self.mode = mode;
        }
        self.clear_pending();
    }

    /// 清空待决状态（操作符、计数、寄存器选择）。
    pub fn clear_pending(&mut self) {
        self.operator_stack.clear();
        self.pre_count = None;
        self.post_count = None;
        self.selected_register = None;
        self.pending_char = None;
    }

    pub fn push_operator(&mut self, operator: Operator) {
        self.operator_stack.push(operator);
    }

    /// 取走计数并相乘（`3d2w` = 6）。
    ///
    /// vim 的计数乘法：操作符前的数字与操作符后的数字相乘，缺省为 1。
    pub fn take_count(&mut self) -> usize {
        let pre = self.pre_count.take().unwrap_or(1);
        let post = self.post_count.take().unwrap_or(1);
        pre.saturating_mul(post)
    }

    /// 追加一个数字到当前计数。
    ///
    /// 操作符待决时进 `post_count`，否则进 `pre_count`。`0` 由调用方决定
    /// 是否作为计数（vim 里 `0` 在无计数时是行首移动）。
    pub fn push_count_digit(&mut self, digit: usize) {
        let slot = if self.operator_stack.is_empty() {
            &mut self.pre_count
        } else {
            &mut self.post_count
        };
        let next = slot.unwrap_or(0).saturating_mul(10).saturating_add(digit);
        *slot = Some(next);
    }

    /// 当前光标形状：Normal/Visual 用配置的形状，Insert 用编辑器默认。
    ///
    /// `editor_default` 是编辑器自身的光标形状设置，Insert 模式下沿用，
    /// 这样「用户把编辑器光标设成 bar」与 vim 的 Insert 语义天然一致。
    pub fn cursor_shape(&self, editor_default: CursorShape) -> CursorShape {
        match self.mode {
            VimMode::Normal | VimMode::Visual | VimMode::VisualLine => self.normal_cursor_shape,
            VimMode::Insert | VimMode::Replace => editor_default,
        }
    }
}

fn parse_cursor_shape(value: &str) -> CursorShape {
    match value {
        "bar" => CursorShape::Bar,
        "underline" => CursorShape::Underline,
        "hollow" => CursorShape::Hollow,
        // 未知值回落到 Block：Normal 模式下块状光标是 vim 的默认观感。
        _ => CursorShape::Block,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_mode_clears_pending_state() {
        let mut state = VimState::default();
        state.push_operator(Operator::Delete);
        state.push_count_digit(3);
        state.selected_register = Some('a');

        state.switch_mode(VimMode::Insert);

        assert_eq!(state.mode, VimMode::Insert);
        assert_eq!(state.last_mode, VimMode::Normal);
        assert!(
            state.operator_stack.is_empty(),
            "切换模式必须丢弃待决操作符"
        );
        assert_eq!(state.pre_count, None);
        assert_eq!(state.selected_register, None);
    }

    #[test]
    fn switching_to_same_mode_still_clears_pending() {
        // `Esc` 在 Normal 模式下的作用就是丢弃未完成的 `d`/`y`/计数。
        let mut state = VimState::default();
        state.push_operator(Operator::Yank);
        state.push_count_digit(2);
        state.switch_mode(VimMode::Normal);

        assert!(
            state.operator_stack.is_empty(),
            "同模式切换应清空待决操作符"
        );
        assert_eq!(state.pre_count, None);
        assert_eq!(state.last_mode, VimMode::Normal, "last_mode 不应被覆盖");
    }

    #[test]
    fn counts_multiply_across_operator() {
        let mut state = VimState::default();
        state.push_count_digit(3); // 3
        state.push_operator(Operator::Delete);
        state.push_count_digit(2); // 2
        assert_eq!(state.take_count(), 6, "3d2w 应作用 6 个词");

        // 取走后归零，下次计数从 1 起算。
        assert_eq!(state.take_count(), 1);
    }

    #[test]
    fn count_digits_accumulate_by_ten() {
        let mut state = VimState::default();
        for digit in [1, 2, 3] {
            state.push_count_digit(digit);
        }
        assert_eq!(state.pre_count, Some(123));
    }

    #[test]
    fn count_digits_go_to_post_after_operator() {
        let mut state = VimState::default();
        state.push_count_digit(2);
        state.push_operator(Operator::Change);
        state.push_count_digit(3);
        assert_eq!(state.pre_count, Some(2));
        assert_eq!(state.post_count, Some(3));
    }

    #[test]
    fn cursor_shape_follows_mode() {
        let state = VimState {
            normal_cursor_shape: CursorShape::Block,
            ..VimState::default()
        };
        assert_eq!(
            state.cursor_shape(CursorShape::Bar),
            CursorShape::Block,
            "Normal 用 vim 配置的形状"
        );

        let insert = VimState {
            mode: VimMode::Insert,
            normal_cursor_shape: CursorShape::Block,
            ..VimState::default()
        };
        assert_eq!(
            insert.cursor_shape(CursorShape::Bar),
            CursorShape::Bar,
            "Insert 沿用编辑器自身的光标形状"
        );
    }

    #[test]
    fn mode_indicators_match_vim_showmode() {
        assert_eq!(VimMode::Normal.indicator(), "NORMAL");
        assert_eq!(VimMode::Insert.indicator(), "INSERT");
        assert_eq!(VimMode::VisualLine.indicator(), "VISUAL LINE");
    }
}
