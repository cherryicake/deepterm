use std::collections::VecDeque;
use std::fmt::Debug;

use super::change::{Change, HistoryItem};

/// 编辑历史管理，用于 undo/redo 功能
#[derive(Debug)]
pub struct History<I: HistoryItem> {
    /// undo 栈
    undos: VecDeque<I>,
    /// redo 栈
    redos: VecDeque<I>,
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

    /// 强制开启一个新分组，下一次 `push` 必定落在新的版本号上。
    ///
    /// 用于语义边界：切换光标位置、换行、执行 undo/redo 等场景下，即便两次
    /// 编辑相隔很近也不应被合并成同一步撤销。
    pub fn break_group(&mut self) {
        self.version = self.version.wrapping_add(1);
        self.last_changed_at = std::time::Instant::now();
    }

    /// 递增版本号
    ///
    /// `last_changed_at` 记录的是**当前这一组开始的时间**，而不是上一次
    /// `push` 的时间。两者差别很大：
    ///
    /// - 若每次 `push` 都刷新锚点，判定基准就退化成「距上次按键的间隔」。
    ///   以 0.9s 的节奏持续输入、`group_interval` 为 1s 时，`elapsed()` 恒为
    ///   0.9s < 1s，版本号永不递增 —— 一整段输入会被折叠成一个 undo 组，
    ///   一次 Cmd+Z 全没了。
    /// - 锚点只在**开新组**时刷新，则累积间隔超过 `group_interval` 就会切组，
    ///   连续输入会被切成若干步，符合预期。
    fn inc_version(&mut self) -> usize {
        let now = std::time::Instant::now();
        if !self.grouping {
            if let Some(interval) = self.group_interval {
                if self.last_changed_at.elapsed() > interval {
                    // 上一组到此为止，新组从现在开始计时。
                    self.version += 1;
                    self.last_changed_at = now;
                }
                // 仍在间隔内：沿用旧锚点，让本组有确定的寿命上限。
            } else {
                self.version += 1;
                self.last_changed_at = now;
            }
        } else {
            // 显式分组期间版本号不动，但要持续刷新锚点，这样 `end_grouping`
            // 之后的下一次编辑是从「分组结束」开始计时，而不是从建组开始。
            self.last_changed_at = now;
        }
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
            self.undos.pop_front();
        }

        if self.unique {
            self.undos.retain(|c| *c != item);
            self.redos.retain(|c| *c != item);
        }

        // 清空 redo 栈（新的变更会使 redo 历史失效）
        self.redos.clear();

        let mut item = item;
        item.set_version(version);
        self.undos.push_back(item);
    }

    /// 获取 undo 栈
    pub fn undos(&self) -> &VecDeque<I> {
        &self.undos
    }

    /// 获取 redo 栈
    pub fn redos(&self) -> &VecDeque<I> {
        &self.redos
    }

    /// 清空历史
    pub fn clear(&mut self) {
        self.undos.clear();
        self.redos.clear();
    }

    /// 从 `from` 弹出一个版本组（同 version 的连续变更），整体克隆一份压回 `to`，
    /// 返回弹出的变更列表。undo / redo 的共享实现。
    fn pop_group(from: &mut VecDeque<I>, to: &mut VecDeque<I>) -> Option<Vec<I>> {
        let first_change = from.pop_back()?;
        let target_version = first_change.version();
        let mut changes = vec![first_change];
        // 收集同一版本的所有变更
        while let Some(change) = from.back() {
            if change.version() == target_version {
                changes.push(from.pop_back().unwrap());
            } else {
                break;
            }
        }
        // 将变更放入对侧栈
        to.extend(changes.iter().cloned());
        Some(changes)
    }

    /// 执行 undo，返回被撤销的变更列表
    pub fn undo(&mut self) -> Option<Vec<I>> {
        Self::pop_group(&mut self.undos, &mut self.redos)
    }

    /// 执行 redo，返回被重做的变更列表
    pub fn redo(&mut self) -> Option<Vec<I>> {
        Self::pop_group(&mut self.redos, &mut self.undos)
    }
}

impl Default for History<Change> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::change::Change;
    use super::*;

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
        let mut history = History::new().group_interval(std::time::Duration::from_secs(1));

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

    /// 回归测试：连续等速输入必须被切成多个 undo 组。
    ///
    /// 曾经的 bug：`inc_version` 每次 `push` 都刷新计时锚点，于是判定基准是
    /// 「距上次按键」而非「距本组开始」。按键间隔恒小于 `group_interval` 时
    /// 版本号永不递增，整段输入被折叠成一个 undo 组。
    #[test]
    fn test_group_interval_measures_from_group_start() {
        // 时间线（无调度延迟时）：push 依次落在 0/300/600/900/1200ms，
        // 按"距组起点"计算分组得 a b | c d | e。
        // 容错边距：分组判定要求"距上次 push < 500ms 且距组起点 ≥ 500ms"，
        // 每段 sleep 允许最多 ~200ms 的调度延迟仍保持预期分组——
        // 旧的 60ms/100ms 组合只容 40ms 延迟，在并行测试负载下是 flaky。
        let mut history = History::new().group_interval(std::time::Duration::from_millis(500));
        for (i, ch) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            history.push(Change::new(i..i, "", i..i + 1, ch));
            std::thread::sleep(std::time::Duration::from_millis(300));
        }

        // 累积间隔跨过 100ms 就应该切组：a b | c d | e，共 3 组。
        let versions: Vec<usize> = history.undos().iter().map(|c| c.version()).collect();
        assert_eq!(versions.len(), 5);
        assert_eq!(versions[0], versions[1], "a and b belong to one group");
        assert_eq!(versions[2], versions[3], "c and d belong to one group");
        assert_ne!(versions[1], versions[2], "a second group must have started");
        assert_ne!(versions[3], versions[4], "a third group must have started");

        // 一次 undo 只撤销最后一组（单个 "e"），而不是整段输入。
        let changes = history.undo().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new_text, "e");
        assert_eq!(history.undos().len(), 4);
    }

    #[test]
    fn test_break_group_starts_a_new_undo_step() {
        let mut history = History::new().group_interval(std::time::Duration::from_secs(60));
        history.push(Change::new(0..0, "", 0..1, "a"));
        history.push(Change::new(1..1, "", 1..2, "b"));

        history.break_group();
        history.push(Change::new(2..2, "", 2..3, "c"));

        // 虽然间隔远未超过 group_interval，"c" 也必须独立成组。
        let changes = history.undo().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new_text, "c");
    }

    /// 回归测试：`unique()` 曾完全是空操作 —— 去重的 `retain` 执行在
    /// `set_version` 之前，而 `Change` 的 `PartialEq` 把 `version` 也算进
    /// 相等性，于是新记录（version == 0）与栈内记录（真实版本号）永远不等。
    #[test]
    fn test_unique_deduplicates_changes() {
        let mut history: History<Change> = History::new().max_undos(100).unique();

        history.push(Change::new(0..0, "", 0..1, "a"));
        history.push(Change::new(1..1, "", 1..2, "b"));
        // 与第一条内容完全相同的重复记录必须被去重。
        history.push(Change::new(0..0, "", 0..1, "a"));

        assert_eq!(
            history.undos().len(),
            2,
            "duplicate change should replace the older one, not stack up"
        );
        assert_eq!(history.undos().back().unwrap().new_text, "a");
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
        assert_eq!(history.undos().back().unwrap().tab_index, 1);

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
