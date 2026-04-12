/// 记录文本变更
#[derive(Debug, PartialEq, Clone)]
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
        }
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
