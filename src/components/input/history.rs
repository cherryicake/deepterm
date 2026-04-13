use std::fmt::Debug;

use super::change::{Change, HistoryItem};

/// 编辑历史管理，用于 undo/redo 功能
#[derive(Debug)]
pub struct History<I: HistoryItem> {
    /// undo 栈
    undos: Vec<I>,
    /// redo 栈
    redos: Vec<I>,
    /// 当前版本号
    version: usize,
    /// 是否忽略变更记录（undo/redo 操作时使用）
    pub ignore: bool,
    /// 最大 undo 次数
    max_undos: usize,
    /// 最后变更时间（用于分组）
    last_changed_at: std::time::Instant,
    /// 分组间隔，在此间隔内的变更会被分到同一组
    group_interval: Option<std::time::Duration>,
    /// 是否正在分组
    grouping: bool,
    /// 是否去重，开启后相同变更不会重复记录
    unique: bool,
}

impl<I> History<I>
where
    I: HistoryItem,
{
    pub fn new() -> Self {
        Self {
            undos: Default::default(),
            redos: Default::default(),
            version: 0,
            ignore: false,
            max_undos: 1000,
            last_changed_at: std::time::Instant::now(),
            group_interval: None,
            grouping: false,
            unique: false,
        }
    }

    /// 设置最大 undo 次数
    pub fn max_undos(mut self, max_undos: usize) -> Self {
        self.max_undos = max_undos;
        self
    }

    /// 设置去重，开启后相同变更不会重复记录
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// 设置分组间隔，在此间隔内的变更会被分到同一组
    pub fn group_interval(mut self, interval: std::time::Duration) -> Self {
        self.group_interval = Some(interval);
        self
    }

    /// 开始分组，在此期间的所有变更会被分到同一组
    pub fn start_grouping(&mut self) {
        self.grouping = true;
    }

    /// 结束分组
    pub fn end_grouping(&mut self) {
        self.grouping = false;
    }

    /// 递增版本号
    fn inc_version(&mut self) -> usize {
        let now = std::time::Instant::now();
        if !self.grouping {
            if let Some(interval) = self.group_interval {
                if self.last_changed_at.elapsed() > interval {
                    self.version += 1;
                }
            } else {
                self.version += 1;
            }
        }
        self.last_changed_at = now;
        self.version
    }

    /// 获取当前版本号
    pub fn version(&self) -> usize {
        self.version
    }

    /// 压入一个变更记录
    pub fn push(&mut self, item: I) {
        let version = self.inc_version();

        // 限制 undo 栈大小
        if self.undos.len() >= self.max_undos {
            self.undos.remove(0);
        }

        if self.unique {
            self.undos.retain(|c| *c != item);
            self.redos.retain(|c| *c != item);
        }

        // 清空 redo 栈（新的变更会使 redo 历史失效）
        self.redos.clear();

        let mut item = item;
        item.set_version(version);
        self.undos.push(item);
    }

    /// 获取 undo 栈
    pub fn undos(&self) -> &Vec<I> {
        &self.undos
    }

    /// 获取 redo 栈
    pub fn redos(&self) -> &Vec<I> {
        &self.redos
    }

    /// 清空历史
    pub fn clear(&mut self) {
        self.undos.clear();
        self.redos.clear();
    }

    /// 执行 undo，返回被撤销的变更列表
    pub fn undo(&mut self) -> Option<Vec<I>> {
        if let Some(first_change) = self.undos.pop() {
            let mut changes = vec![first_change.clone()];
            // 收集同一版本的所有变更
            while self
                .undos
                .iter()
                .filter(|c| c.version() == first_change.version())
                .count()
                > 0
            {
                changes.push(self.undos.pop().unwrap());
            }
            // 将变更放入 redo 栈
            self.redos.extend(changes.clone());
            Some(changes)
        } else {
            None
        }
    }

    /// 执行 redo，返回被重做的变更列表
    pub fn redo(&mut self) -> Option<Vec<I>> {
        if let Some(first_change) = self.redos.pop() {
            let mut changes = vec![first_change.clone()];
            // 收集同一版本的所有变更
            while self
                .redos
                .iter()
                .filter(|c| c.version() == first_change.version())
                .count()
                > 0
            {
                changes.push(self.redos.pop().unwrap());
            }
            // 将变更放入 undo 栈
            self.undos.extend(changes.clone());
            Some(changes)
        } else {
            None
        }
    }
}

impl Default for History<Change> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::change::Change;

    #[derive(Clone)]
    struct TabIndex {
        tab_index: usize,
        version: usize,
    }

    impl PartialEq for TabIndex {
        fn eq(&self, other: &Self) -> bool {
            self.tab_index == other.tab_index
        }
    }

    impl From<usize> for TabIndex {
        fn from(value: usize) -> Self {
            TabIndex {
                tab_index: value,
                version: 0,
            }
        }
    }

    impl HistoryItem for TabIndex {
        fn version(&self) -> usize {
            self.version
        }
        fn set_version(&mut self, version: usize) {
            self.version = version;
        }
    }

    #[test]
    fn test_history_push_and_undo() {
        let mut history = History::new();

        // 模拟三次文本变更
        history.push(Change::new(0..0, "", 0..5, "hello"));
        history.push(Change::new(5..5, "", 5..7, " w"));
        history.push(Change::new(7..7, "", 7..11, "orld"));

        assert_eq!(history.undos().len(), 3);
        assert_eq!(history.redos().len(), 0);

        // undo 一次
        let changes = history.undo().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new_text, "orld");
        assert_eq!(history.undos().len(), 2);
        assert_eq!(history.redos().len(), 1);

        // redo
        let changes = history.redo().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new_text, "orld");
        assert_eq!(history.undos().len(), 3);
        assert_eq!(history.redos().len(), 0);
    }

    #[test]
    fn test_history_grouping() {
        let mut history = History::new()
            .group_interval(std::time::Duration::from_secs(1));

        // 开始分组
        history.start_grouping();
        history.push(Change::new(0..0, "", 0..1, "a"));
        history.push(Change::new(1..1, "", 1..2, "b"));
        history.push(Change::new(2..2, "", 2..3, "c"));
        history.end_grouping();

        // 所有变更应该在同一版本
        assert_eq!(history.undos().len(), 3);

        // undo 应该一次性撤销同一版本的所有变更
        let changes = history.undo().unwrap();
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn test_new_change_clears_redo() {
        let mut history = History::new();

        history.push(Change::new(0..0, "", 0..5, "hello"));
        history.push(Change::new(5..5, "", 5..7, " w"));

        // undo 一次
        history.undo();
        assert_eq!(history.redos().len(), 1);

        // 新的变更会清空 redo 栈
        history.push(Change::new(7..7, "", 7..11, "orld"));
        assert_eq!(history.redos().len(), 0);
    }

    #[test]
    fn test_unique_history() {
        let mut history: History<TabIndex> = History::new().max_undos(100).unique();

        // Push some items
        history.push(0.into());
        history.push(1.into());
        history.push(1.into()); // Duplicate, should be ignored
        history.push(2.into());
        history.push(1.into()); // Duplicate, should remove old, and add new

        // Check the version and undo stack
        assert_eq!(history.undos().len(), 3);
        assert_eq!(history.undos().last().unwrap().tab_index, 1);

        // Undo the last change
        let changes = history.undo().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].tab_index, 1);

        assert_eq!(history.redos().len(), 1);
        // Push duplicate, should clear redo and be ignored in undos
        history.push(2.into());

        assert_eq!(history.undos().len(), 2);
        assert_eq!(history.redos().len(), 0); // redos cleared by push

        // Redo should fail because redos was cleared
        assert!(history.redo().is_none());

        // Push another item
        history.push(3.into());

        // Check the version and undo stack
        assert_eq!(history.undos().len(), 3);

        // Undo all changes
        for _ in 0..3 {
            history.undo();
        }

        // Check the undo stack is empty and redo stack has all changes
        assert_eq!(history.undos().len(), 0);
        assert_eq!(history.redos().len(), 3);
    }
}
