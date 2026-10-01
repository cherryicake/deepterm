//! Vim action 声明与 handler。
//!
//! action 按名注册进 gpui 的 `ActionRegistry`（`actions!` 宏内部走
//! `#[derive(gpui::Action)]` + `inventory`），因此 `keymap.json` 里可以直接写
//! `"h": "vim::Left"` 这样的字符串。
//!
//! 命名空间固定为 `vim`，与 Zed 一致，便于对照。
//!
//! handler 是 `InputState` 上的方法，由 `EditorInput` 渲染时经
//! `window.listener_for(&state, InputState::vim_xxx)` 接线——与既有的
//! `editor::MoveLeft` 等 action 走同一条路径。

use gpui::actions;
use serde::Deserialize;

/// 文本对象键（`i"`、`a(`、`ip`、`it` 里的那个字符）。
///
/// 带参数而不是给每个对象声明一个 action：对象名有十来个，逐个声明会让
/// 键位表里出现 `ObjectDoubleQuote`、`ObjectParen` 这类冗长名字，而它们
/// 的语义完全一致（都是「把当前操作符作用到该对象上」）。
#[derive(Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = vim)]
pub struct ObjectKey {
    /// 对象键字符，如 `"`、`(`、`w`、`p`、`t`。
    pub key: String,
}

/// 计数前缀（`3dw` 里的 `3`）。
///
/// 单独定义而非用 `vim::Number(3)` 这样的带参 action：数字键有 10 个，
/// 每个都需要一条键位绑定，带参形式在 JSON 里更啰嗦。这里按 vim 的
/// `1`~`9`（`0` 另作行首）逐个声明，语义更直白。
///
/// 之所以不用 `actions!` 的批量形式，是因为需要在下面统一注册处理逻辑。
macro_rules! count_actions {
    ($($name:ident => $digit:expr),* $(,)?) => {
        actions!(vim, [$($name),*]);

        /// 该 action 对应的数字；不是计数 action 时返回 `None`。
        pub fn count_digit(action_name: &str) -> Option<usize> {
            match action_name {
                $(stringify!($name) => Some($digit),)*
                _ => None,
            }
        }
    };
}

count_actions!(
    Count1 => 1,
    Count2 => 2,
    Count3 => 3,
    Count4 => 4,
    Count5 => 5,
    Count6 => 6,
    Count7 => 7,
    Count8 => 8,
    Count9 => 9,
);

actions!(
    vim,
    [
        // 模式切换
        /// 进入 Normal 模式（`Esc`）。
        NormalMode,
        /// 在光标前插入（`i`）。
        InsertBefore,
        /// 在光标后插入（`a`）。
        InsertAfter,
        /// 在首个非空白字符处插入（`I`）。
        InsertFirstNonWhitespace,
        /// 在行尾插入（`A`）。
        InsertEndOfLine,
        // 移动
        /// `h`
        Left,
        /// `l`
        Right,
        /// `j`
        Down,
        /// `k`
        Up,
        /// `w`
        NextWordStart,
        /// `b`
        PreviousWordStart,
        /// `e`
        NextWordEnd,
        /// `0`
        StartOfLine,
        /// `$`
        EndOfLine,
        /// `gg`
        StartOfDocument,
        /// `G`
        EndOfDocument,
        // 操作符（骨架：只压栈与改上下文，执行在阶段 1 补齐）
        /// `d`
        PushDelete,
        /// `y`
        PushYank,
        /// `c`
        PushChange,
        /// `i` 对象前缀
        PushObjectInner,
        /// `a` 对象前缀
        PushObjectAround,
        /// 对象名 `w`（配合 `i`/`a` 前缀，如 `diw`）
        ObjectWord,
        // 单键编辑命令
        /// `x`
        DeleteChar,
        /// `p`
        PasteAfter,
        /// `P`
        PasteBefore,
        /// `J`
        JoinLines,
        /// `>>`
        IndentLine,
        /// `<<`
        OutdentLine,
        // 撤销与重做
        /// `u`
        Undo,
        /// `ctrl-r`
        Redo,
        // 行首定位与整行
        /// `^` 首个非空白字符
        FirstNonWhitespace,
        /// `_` 光标下移一行到首个非空白
        FirstNonWhitespaceDown,
        /// `S` 清空整行并进入 Insert（等价 `cc`）
        SubstituteLine,
        /// `D` 删到行尾（等价 `d$`）
        DeleteToEndOfLine,
        /// `C` 改到行尾（等价 `c$`）
        ChangeToEndOfLine,
        /// `Y` 复制整行（等价 `yy`）
        YankLine,
        /// `s` 删字符并进入 Insert
        SubstituteChar,
        // 翻页与定位
        /// `ctrl-d`
        HalfPageDown,
        /// `ctrl-u`
        HalfPageUp,
        /// `ctrl-f`
        PageDown,
        /// `ctrl-b`
        PageUp,
        /// `H` 屏幕顶部
        WindowTop,
        /// `M` 屏幕中部
        WindowMiddle,
        /// `L` 屏幕底部
        WindowBottom,
        /// `%` 匹配括号
        MatchingBracket,
        /// `o` 在下方开新行并进入 Insert
        OpenLineBelow,
        /// `O` 在上方开新行并进入 Insert
        OpenLineAbove,
        /// `r` 替换单个字符（等待一个字符）
        PushReplace,
        /// `f` 向后查找字符（等待一个字符）
        PushFindForward,
        /// `t` 向后查找字符（停在目标前）
        PushFindBefore,
        /// `F` 向前查找字符
        PushFindBackward,
        /// `T` 向前查找字符（停在目标后）
        PushFindAfter,
        /// `;` 重复上次字符查找
        RepeatFind,
        /// `,` 反向重复上次字符查找
        RepeatFindReversed,
        /// `~` 切换大小写
        ToggleCase,
        /// `gc` 注释切换（注释行）
        ToggleComment,
        /// `*` 向下查找光标下的词
        SearchWordForward,
        /// `#` 向上查找光标下的词
        SearchWordBackward,
        /// `n` 下一个匹配
        SearchNext,
        /// `N` 上一个匹配
        SearchPrev,
        /// `.` 重复上次修改
        RepeatChange,
        /// `"` 选择寄存器（等待一个寄存器名）
        PushRegister,
        /// `m` 设置标记（等待一个标记名）
        PushSetMark,
        /// `` ` `` 跳到标记（等待一个标记名）
        PushJumpMark,
        /// `'` 跳到标记所在行（等待一个标记名）
        PushJumpMarkLine,
        // Visual 模式
        /// `v` 字符可视
        VisualChar,
        /// `V` 行可视
        VisualLine,
        /// `o` 在 Visual 下切换选区端点
        VisualSwapEnds,
        /// `gv` 重选上次的可视区
        ReselectVisual,
        /// 可视模式下删除选区（`d` / `x`）
        VisualDelete,
        /// 可视模式下复制选区（`y`）
        VisualYank,
        /// 可视模式下替换选区（`c` / `s`）
        VisualChange,
        /// 可视模式下缩进（`>`）
        VisualIndent,
        /// 可视模式下反缩进（`<`）
        VisualOutdent,
    ]
);
