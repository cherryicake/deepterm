//! Vim 标记（`m` / `` ` `` / `'`）。
//!
//! 与寄存器一样是**跨标签共享**的全局状态：vim 里在 A 文件打 `ma`，切到
//! B 文件也能 `` `a `` 跳回去。标记存的是**字节偏移**而不是行号——行号会
//! 随编辑漂移，偏移配合 `clip_offset` 至少能保证落在字符边界上。
//!
//! 局限：偏移不随编辑自动调整（真正的锚点需要 rope 的 anchor 支持）。
//! 编辑之后标记可能指向偏离的位置，这是当前实现的已知代价。

use std::collections::HashMap;

use gpui::{App, Global};

/// 标记表：小写字母 → 字节偏移。
#[derive(Default)]
pub struct Marks {
    marks: HashMap<char, usize>,
}

impl Global for Marks {}

impl Marks {
    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<Self>()
    }

    pub fn init(cx: &mut App) {
        cx.set_global(Self::default());
    }

    /// 记录标记位置。
    ///
    /// 只接受字母：vim 里 `` `. ``/`'` 等是特殊标记（上次修改、上次跳转），
    /// 当前未实现，用字母范围排除它们可以避免误存。
    pub fn set(&mut self, name: char, offset: usize) {
        if name.is_ascii_alphabetic() {
            self.marks.insert(name.to_ascii_lowercase(), offset);
        }
    }

    pub fn get(&self, name: char) -> Option<usize> {
        self.marks.get(&name.to_ascii_lowercase()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_round_trip() {
        let mut marks = Marks::default();
        marks.set('a', 42);
        assert_eq!(marks.get('a'), Some(42));
    }

    #[test]
    fn marks_are_case_insensitive() {
        let mut marks = Marks::default();
        marks.set('A', 7);
        assert_eq!(marks.get('a'), Some(7), "大小写指向同一标记");
    }

    #[test]
    fn non_alphabetic_names_are_rejected() {
        let mut marks = Marks::default();
        marks.set('.', 1);
        marks.set('`', 2);
        marks.set('1', 3);
        assert_eq!(marks.get('.'), None, "特殊标记名不应被当成字母标记");
        assert_eq!(marks.get('1'), None);
    }

    #[test]
    fn missing_mark_is_none() {
        let marks = Marks::default();
        assert_eq!(marks.get('z'), None);
    }

    #[test]
    fn setting_same_mark_overwrites() {
        let mut marks = Marks::default();
        marks.set('a', 1);
        marks.set('a', 99);
        assert_eq!(marks.get('a'), Some(99));
    }
}
