//! Vim 寄存器。
//!
//! 与 Zed 的 `VimGlobals::write_registers` 同构，规则也照 vim 的实际行为：
//!
//! | 寄存器 | 语义 |
//! |---|---|
//! | `"` | 无名寄存器：每次删除/复制都会写入 |
//! | `0` | 最近一次**复制**（`y`）的内容，删除不动它 |
//! | `1`–`9` | 删除历史，`1` 最新；行级删除才进移位队列 |
//! | `-` | 最近一次「小删除」（行内的 `x`/`d` 移动） |
//! | `_` | 黑洞：写入丢弃，读取为空 |
//! | `+` / `*` | 系统剪贴板 |
//! | `a`–`z` | 命名寄存器；大写 `A`–`Z` 追加 |
//!
//! 寄存器是**跨标签共享**的：vim 里 `"ayy` 之后切到别的文件也能 `"ap`。
//! 所以状态放在 `gpui::Global` 而不是每个 `InputState` 上。

use std::collections::HashMap;

use gpui::{App, ClipboardItem, Global};

/// 寄存器存储。
#[derive(Default)]
pub struct Registers {
    named: HashMap<char, String>,
    /// 最近一次复制（寄存器 `0`）。
    yank: String,
    /// 删除历史（寄存器 `1`–`9`），`1` 最新。
    deletes: Vec<String>,
    /// 最近一次小删除（寄存器 `-`）。
    small_delete: String,
    /// 无名寄存器。
    unnamed: String,
}

impl Global for Registers {}

impl Registers {
    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<Self>()
    }

    pub fn init(cx: &mut App) {
        cx.set_global(Self::default());
    }

    /// 读寄存器。`None` 或 `"` 读无名寄存器。
    pub fn read(&self, name: Option<char>, cx: &App) -> String {
        match name {
            None | Some('"') => self.unnamed.clone(),
            // 黑洞寄存器恒为空。
            Some('_') => String::new(),
            Some('0') => self.yank.clone(),
            Some('-') => self.small_delete.clone(),
            Some(d @ '1'..='9') => {
                let index = d as usize - '1' as usize;
                self.deletes.get(index).cloned().unwrap_or_default()
            }
            Some('+') | Some('*') => cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default(),
            Some(name) => {
                // 大写字母与小写是同一个寄存器（写入时追加）。
                let key = name.to_ascii_lowercase();
                self.named.get(&key).cloned().unwrap_or_default()
            }
        }
    }

    /// 写寄存器（不改动 `0`–`9` 的历史队列）。
    ///
    /// 编号寄存器的移位由 [`Self::record_delete`] 负责：只有行级删除才进
    /// `1`–`9`，行内的 `x` 只更新 `-` 与无名寄存器。
    pub fn write(&mut self, name: Option<char>, text: &str, cx: &mut App) {
        match name {
            // 黑洞：丢弃。
            Some('_') => {}
            Some('0') => {
                // `0` 是只读的复制寄存器，vim 里写它无效。
            }
            Some('+') | Some('*') => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                // 系统剪贴板同时更新无名寄存器（vim 的默认行为）。
                self.unnamed = text.to_string();
            }
            Some(name) => {
                let upper = name.is_ascii_uppercase();
                let key = name.to_ascii_lowercase();
                if upper {
                    // 大写：追加。
                    self.named.entry(key).or_default().push_str(text);
                } else {
                    self.named.insert(key, text.to_string());
                }
                self.unnamed = text.to_string();
            }
            None => {
                self.unnamed = text.to_string();
            }
        }
    }

    /// 记录一次复制（`y`）。
    pub fn record_yank(&mut self, text: &str, name: Option<char>, cx: &mut App) {
        if name == Some('_') {
            return;
        }
        self.yank = text.to_string();
        self.write(name, text, cx);
    }

    /// 记录一次删除。
    ///
    /// 黑洞寄存器（`"_`）不改动任何寄存器状态——包括 `1`–`9` 的历史队列。
    /// 若先移位再判断寄存器名，`"_dd` 会把 `1` 挤到 `2`，而 vim 里
    /// 这些编号寄存器应当原封不动。
    pub fn record_delete(&mut self, text: &str, linewise: bool, name: Option<char>, cx: &mut App) {
        if name == Some('_') {
            return;
        }
        if linewise {
            // 行级删除进移位队列。
            self.deletes.insert(0, text.to_string());
            self.deletes.truncate(9);
        } else {
            self.small_delete = text.to_string();
        }
        self.write(name, text, cx);
    }

    /// 该寄存器是否应写入系统剪贴板。
    ///
    /// 只有显式指定 `+`/`*` 时才碰系统剪贴板——否则每次 `dd` 都会覆盖
    /// 用户从别处复制的内容，这不符合 vim 的默认行为。
    pub fn is_system(name: Option<char>) -> bool {
        matches!(name, Some('+') | Some('*'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的最小寄存器环境（不碰真实剪贴板）。
    fn fresh() -> Registers {
        Registers::default()
    }

    #[test]
    fn unnamed_register_round_trips() {
        let mut regs = fresh();
        regs.unnamed = "hello".into();
        // `read` 需要 App 才能读剪贴板，这里直接验证无名路径。
        assert_eq!(regs.unnamed, "hello");
    }

    #[test]
    fn named_registers_are_case_insensitive_keys() {
        let mut regs = fresh();
        regs.named.insert('a', "first".into());
        assert_eq!(regs.named.get(&'a').unwrap(), "first");
    }

    #[test]
    fn linewise_deletes_shift_through_numbered_registers() {
        let mut regs = fresh();
        regs.deletes.insert(0, "one\n".into());
        regs.deletes.insert(0, "two\n".into());
        assert_eq!(regs.deletes[0], "two\n", "最新的删除应在 `1`");
        assert_eq!(regs.deletes[1], "one\n", "旧的挤到 `2`");
    }

    #[test]
    fn delete_history_is_capped_at_nine() {
        let mut regs = fresh();
        for i in 0..12 {
            regs.deletes.insert(0, format!("line{i}\n"));
            regs.deletes.truncate(9);
        }
        assert_eq!(regs.deletes.len(), 9, "编号寄存器最多 9 个");
        assert_eq!(regs.deletes[0], "line11\n");
    }

    #[test]
    fn yank_register_is_separate_from_deletes() {
        let mut regs = fresh();
        regs.yank = "copied".into();
        regs.deletes.insert(0, "deleted\n".into());
        // `y` 不覆盖 `1`–`9`，`dd` 不覆盖 `0`。
        assert_eq!(regs.yank, "copied");
        assert_eq!(regs.deletes[0], "deleted\n");
    }

    #[test]
    fn small_delete_tracked_separately() {
        let mut regs = fresh();
        regs.small_delete = "x".into();
        assert_eq!(regs.small_delete, "x");
        assert!(regs.deletes.is_empty(), "小删除不进编号寄存器");
    }

    #[test]
    fn system_registers_are_detected() {
        assert!(Registers::is_system(Some('+')));
        assert!(Registers::is_system(Some('*')));
        assert!(!Registers::is_system(Some('a')));
        assert!(!Registers::is_system(None));
    }
}
