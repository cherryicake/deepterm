use std::borrow::Cow;

use ropey::RopeSlice;

/// 缩进配置：缩进单元的构成与宽度。
///
/// 编辑器内所有涉及「一级缩进」的地方（Tab / Shift+Tab / Cmd+[ / Cmd+]、回车
/// 智能缩进、键入闭合括号时的自动反缩进、缩进参考线宽度）都以此为准，不再
/// 各自写死 `"\t"` 或常量 4。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TabSize {
    /// 一个缩进单元的列宽（同时是空格缩进时的空格个数）。
    pub tab_size: usize,
    /// true：缩进单元是 Tab 字符；false：缩进单元是 `tab_size` 个空格。
    pub hard_tabs: bool,
}

impl TabSize {
    /// 硬 Tab 缩进 —— 编辑器当前默认（Makefile 等依赖制表符的格式需要它）。
    pub const HARD_TABS: Self = Self {
        tab_size: 4,
        hard_tabs: true,
    };

    /// 空格缩进，单元宽度 `width` 个空格。
    pub const fn spaces(width: usize) -> Self {
        Self {
            tab_size: width,
            hard_tabs: false,
        }
    }

    fn width(self) -> usize {
        self.tab_size.clamp(1, 64)
    }

    /// 一个缩进单元的字符内容。硬 Tab 是静态借用，空格时按需构造。
    pub fn indent_unit(self) -> Cow<'static, str> {
        if self.hard_tabs {
            Cow::Borrowed("\t")
        } else {
            Cow::Owned(" ".repeat(self.width()))
        }
    }

    /// 行首反缩进一次要删掉的字节数（0 表示行首没有缩进单元）。
    ///
    /// 硬 Tab 模式下优先删掉行首的 Tab；若行首不是 Tab（混合缩进、或该行本就
    /// 用空格缩进），则退回删掉至多 `width()` 个空格 —— 否则这类行会完全无法
    /// 反缩进。空格模式只认前导空格。
    pub fn outdent_len(self, line: &str) -> usize {
        let bytes = line.as_bytes();
        if self.hard_tabs && bytes.first() == Some(&b'\t') {
            1
        } else {
            bytes
                .iter()
                .take_while(|&&b| b == b' ')
                .take(self.width())
                .count()
        }
    }

    /// 行首缩进的列数（空格记 1 列，Tab 按 `tab_size` 折算）。
    pub fn indent_count(self, line: &RopeSlice) -> usize {
        let mut count = 0;
        for ch in line.chars() {
            match ch {
                '\t' => count += self.width(),
                ' ' => count += 1,
                _ => break,
            }
        }
        count
    }
}

impl Default for TabSize {
    fn default() -> Self {
        Self::HARD_TABS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_unit_follows_config() {
        assert_eq!(TabSize::HARD_TABS.indent_unit(), "\t");
        assert_eq!(TabSize::spaces(2).indent_unit(), "  ");
        assert_eq!(TabSize::spaces(4).indent_unit(), "    ");
        // 宽度被夹在 1..=64，避免 `tab_size: 0` 让缩进单元变成空串。
        assert_eq!(TabSize::spaces(0).indent_unit(), " ");
        assert_eq!(TabSize::spaces(1000).indent_unit().len(), 64);
    }

    /// 硬 Tab 模式：优先吃行首 Tab，否则退回吃至多 1 个缩进单元宽度的空格。
    #[test]
    fn outdent_len_hard_tabs_prefers_tab_then_spaces() {
        let tab = TabSize::HARD_TABS;
        assert_eq!(tab.outdent_len("\tfoo"), 1);
        assert_eq!(tab.outdent_len("\t\tfoo"), 1); // 一次只退一级
        assert_eq!(tab.outdent_len("foo"), 0);
        assert_eq!(tab.outdent_len(""), 0);
        // 行首不是 Tab：退回空格分支，按宽度 4 封顶。
        assert_eq!(tab.outdent_len("    foo"), 4);
        assert_eq!(tab.outdent_len("  \tfoo"), 2);
        assert_eq!(tab.outdent_len("      foo"), 4);
    }

    /// 空格模式下行首的 Tab 不构成缩进单元，避免把 `\t` 当成一个空格删掉。
    #[test]
    fn outdent_len_spaces_ignores_leading_tab() {
        assert_eq!(TabSize::spaces(4).outdent_len("\tfoo"), 0);
        assert_eq!(TabSize::spaces(4).outdent_len("  \tfoo"), 2);
    }

    /// 空格配置下反缩进最多吃掉一个缩进单元的宽度，且不会越过非空格字符。
    #[test]
    fn outdent_len_spaces_caps_at_one_unit() {
        let two = TabSize::spaces(2);
        assert_eq!(two.outdent_len("  foo"), 2);
        assert_eq!(two.outdent_len("    foo"), 2);
        assert_eq!(two.outdent_len(" foo"), 1);
        assert_eq!(two.outdent_len("foo"), 0);
        assert_eq!(two.outdent_len(""), 0);
        assert_eq!(two.outdent_len("\tfoo"), 0);

        let four = TabSize::spaces(4);
        assert_eq!(four.outdent_len("    foo"), 4);
        assert_eq!(four.outdent_len("      foo"), 4);
        assert_eq!(four.outdent_len("  foo"), 2);
    }

    #[test]
    fn indent_count_converts_tabs_and_spaces() {
        let line = RopeSlice::from("  \tfoo\t bar");
        // 硬 Tab：2 空格 + 1 Tab(4 列) = 6 列；正文里的 Tab 不参与。
        assert_eq!(TabSize::HARD_TABS.indent_count(&line), 6);
        // 空格配置并不改变列折算规则，Tab 仍按 `tab_size` 列计。
        assert_eq!(TabSize::spaces(2).indent_count(&line), 4);
        assert_eq!(TabSize::spaces(8).indent_count(&line), 10);

        assert_eq!(TabSize::HARD_TABS.indent_count(&RopeSlice::from("foo")), 0);
        assert_eq!(TabSize::HARD_TABS.indent_count(&RopeSlice::from("")), 0);
        // 行首无空白但整行都是空白时算满。
        assert_eq!(TabSize::HARD_TABS.indent_count(&RopeSlice::from("\t\t")), 8);
    }
}
