/// 记录文本变更
#[derive(Debug, Clone)]
pub struct Change {
    /// 变更前的文本范围 (byte offset)
    pub old_range: std::ops::Range<usize>,
    /// 变更前的文本内容
    pub old_text: String,
    /// 变更后的文本范围
    pub new_range: std::ops::Range<usize>,
    /// 变更后的文本内容
    pub new_text: String,
    /// 版本号（用于分组）
    version: usize,
    /// 编辑前的选区 `(start, end)`，用于 undo 时恢复光标位置。
    /// `None` 表示未知（例如调用方只构造了裸 `Change`），恢复时跳过。
    pub old_selection: Option<(usize, usize)>,
    /// 编辑后的选区 `(start, end)`，用于 redo 时恢复光标位置。
    pub new_selection: Option<(usize, usize)>,
}

impl Change {
    pub fn new(
        old_range: std::ops::Range<usize>,
        old_text: &str,
        new_range: std::ops::Range<usize>,
        new_text: &str,
    ) -> Self {
        Self {
            old_range,
            old_text: old_text.to_string(),
            new_range,
            new_text: new_text.to_string(),
            version: 0,
            old_selection: None,
            new_selection: None,
        }
    }

    /// 记录编辑前后的选区，使 undo / redo 能把光标放回它当时所在的地方。
    pub fn with_selections(
        mut self,
        old_selection: std::ops::Range<usize>,
        new_selection: std::ops::Range<usize>,
    ) -> Self {
        self.old_selection = Some((old_selection.start, old_selection.end));
        self.new_selection = Some((new_selection.start, new_selection.end));
        self
    }
}

impl PartialEq for Change {
    /// 只比较**内容**，不含 `version`。
    ///
    /// 这个相等性是 `History::unique()` 去重的判据：去重发生在 `set_version`
    /// 之前，若把 `version` 也算进来，新变更的 `version` 恒为 0 而栈内记录的
    /// 是真实版本号，比较永远不等，去重就完全失效了。
    ///
    /// 选区同样不参与比较：两段「从同一处插入同一个词」的编辑应当视为同一个
    /// 变更，即便光标落点不同。
    fn eq(&self, other: &Self) -> bool {
        self.old_range == other.old_range
            && self.old_text == other.old_text
            && self.new_range == other.new_range
            && self.new_text == other.new_text
    }
}

/// HistoryItem trait，用于 History 结构体
pub trait HistoryItem: Clone + PartialEq {
    fn version(&self) -> usize;
    fn set_version(&mut self, version: usize);
}

impl HistoryItem for Change {
    fn version(&self) -> usize {
        self.version
    }

    fn set_version(&mut self, version: usize) {
        self.version = version;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unique()` 依赖内容相等：版本号不同的同一段编辑必须判为相等。
    #[test]
    fn test_change_equality_ignores_version() {
        let a = Change::new(0..0, "", 0..5, "hello");
        let mut b = Change::new(0..0, "", 0..5, "hello");
        b.set_version(7);
        assert_eq!(a, b, "version must not participate in equality");

        let c = Change::new(0..0, "", 0..5, "world");
        assert_ne!(a, c);
    }

    #[test]
    fn test_change_equality_ignores_selection() {
        let a = Change::new(0..0, "", 0..5, "hello").with_selections(0..0, 5..5);
        let b = Change::new(0..0, "", 0..5, "hello").with_selections(3..3, 8..8);
        assert_eq!(a, b);
    }

    #[test]
    fn test_with_selections_round_trips() {
        let c = Change::new(1..3, "ab", 1..4, "xyz").with_selections(1..3, 4..4);
        assert_eq!(c.old_selection, Some((1, 3)));
        assert_eq!(c.new_selection, Some((4, 4)));
    }
}
