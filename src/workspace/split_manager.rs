use gpui::{App, AppContext, Entity, SharedString};

use crate::workspace::panel::{Panel, PanelPlacement};
use crate::workspace::session_state::{SplitSessionDirection, SplitSessionState};

/// 修正分割比例数组。
///
/// `sizes` 由会话 JSON 反序列化得到，长度可能与 `children` 不一致（文件损坏、
/// 旧版本格式），直接索引会在拖动分割条时 panic。这里保证输出的 `sizes`
/// 满足：长度 == count、每项有限且为正、总和归一到 1。
fn normalize_sizes(sizes: &[f32], count: usize) -> Vec<f32> {
    if count == 0 {
        return Vec::new();
    }

    let shape_valid = sizes.len() == count && sizes.iter().all(|s| s.is_finite() && *s > 0.0);
    if !shape_valid {
        return vec![1.0 / count as f32; count];
    }

    let sum: f32 = sizes.iter().sum();
    if sum.is_finite() && sum > 0.0 && (sum - 1.0).abs() > 1e-6 {
        sizes.iter().map(|s| s / sum).collect()
    } else {
        sizes.to_vec()
    }
}

/// 分割方向
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitDirection {
    Horizontal, // 水平分割 - 左右
    Vertical,   // 垂直分割 - 上下
}

impl From<PanelPlacement> for SplitDirection {
    fn from(placement: PanelPlacement) -> Self {
        match placement {
            PanelPlacement::Left | PanelPlacement::Right => SplitDirection::Horizontal,
            PanelPlacement::Top | PanelPlacement::Bottom => SplitDirection::Vertical,
        }
    }
}

/// 分割容器 - 可以包含面板或其他分割容器
#[derive(Clone)]
pub enum SplitContainer {
    Panel(Entity<Panel>),
    Split {
        direction: SplitDirection,
        children: Vec<SplitContainer>,
        sizes: Vec<f32>,
    },
}

impl SplitContainer {
    pub fn new_panel(panel: Entity<Panel>) -> Self {
        SplitContainer::Panel(panel)
    }

    pub fn new_split(direction: SplitDirection, children: Vec<SplitContainer>) -> Self {
        let count = children.len();
        let sizes = if count > 0 {
            vec![1.0 / count as f32; count]
        } else {
            vec![]
        };
        SplitContainer::Split {
            direction,
            children,
            sizes,
        }
    }

    pub fn session_state(&self, cx: &App) -> Option<SplitSessionState> {
        match self {
            SplitContainer::Panel(panel) => panel
                .read(cx)
                .session_state(cx)
                .map(SplitSessionState::Panel),
            SplitContainer::Split {
                direction,
                children,
                sizes,
            } => {
                let children = children
                    .iter()
                    .filter_map(|child| child.session_state(cx))
                    .collect::<Vec<_>>();

                if children.is_empty() {
                    return None;
                }

                Some(SplitSessionState::Split {
                    direction: (*direction).into(),
                    children,
                    sizes: sizes.clone(),
                })
            }
        }
    }

    pub fn from_session_state(
        state: &SplitSessionState,
        workspace_id: &str,
        tab_id: &str,
        tab_label: &str,
        cx: &mut App,
    ) -> Self {
        match state {
            SplitSessionState::Panel(panel_state) => {
                let panel = cx.new(|cx| {
                    Panel::from_session_state(
                        panel_state,
                        workspace_id.to_string(),
                        Some(SharedString::from(tab_id.to_string())),
                        Some(SharedString::from(tab_label.to_string())),
                        cx,
                    )
                });
                SplitContainer::Panel(panel)
            }
            SplitSessionState::Split {
                direction,
                children,
                sizes,
            } => {
                let children: Vec<SplitContainer> = children
                    .iter()
                    .map(|child| {
                        Self::from_session_state(child, workspace_id, tab_id, tab_label, cx)
                    })
                    .collect();
                // sizes 来自会话 JSON，长度可能与 children 不一致（损坏/旧版本
                // 文件），拖动分割条时会越界 panic。恢复时统一修正。
                let sizes = normalize_sizes(sizes, children.len());
                SplitContainer::Split {
                    direction: (*direction).into(),
                    children,
                    sizes,
                }
            }
        }
    }

    /// 获取Panel实体
    pub fn get_panel_entity(&self, _cx: &App) -> Option<Entity<Panel>> {
        match self {
            SplitContainer::Panel(panel) => Some(panel.clone()),
            _ => None,
        }
    }

    /// 查找面板
    pub fn find_panel(&self, panel_id: &str, cx: &App) -> Option<Entity<Panel>> {
        match self {
            SplitContainer::Panel(panel) => {
                if panel.read(cx).panel_id == panel_id {
                    Some(panel.clone())
                } else {
                    None
                }
            }
            SplitContainer::Split { children, .. } => {
                for child in children {
                    if let Some(panel) = child.find_panel(panel_id, cx) {
                        return Some(panel);
                    }
                }
                None
            }
        }
    }

    /// 收集所有面板（递归辅助函数：`collect_panels_vec` 与外部
    /// `collect_panels(&mut Vec)` 两种用法共用，避免两份完全相同的实现）
    pub fn collect_panels_into(&self, panels: &mut Vec<Entity<Panel>>) {
        match self {
            SplitContainer::Panel(panel) => panels.push(panel.clone()),
            SplitContainer::Split { children, .. } => {
                for child in children {
                    child.collect_panels_into(panels);
                }
            }
        }
    }

    /// 收集所有面板（不需要Context）
    pub fn collect_panels_vec(&self) -> Vec<Entity<Panel>> {
        let mut panels = Vec::new();
        self.collect_panels_into(&mut panels);
        panels
    }

    /// 计算容器中的panel数量
    pub fn panel_count(&self) -> usize {
        match self {
            SplitContainer::Panel(_) => 1,
            SplitContainer::Split { children, .. } => {
                children.iter().map(|c| c.panel_count()).sum()
            }
        }
    }

    /// 获取第一个panel，不收集所有panel
    /// 用于当只需要第一个panel时，避免完整遍历
    pub fn get_first_panel(&self) -> Option<Entity<Panel>> {
        match self {
            SplitContainer::Panel(panel) => Some(panel.clone()),
            SplitContainer::Split { children, .. } => {
                for child in children {
                    if let Some(panel) = child.get_first_panel() {
                        return Some(panel);
                    }
                }
                None
            }
        }
    }

    /// 获取选中状态的panel
    pub fn get_selected_panel(&self, cx: &App) -> Option<Entity<Panel>> {
        match self {
            SplitContainer::Panel(panel) => {
                if panel.read(cx).is_selected {
                    Some(panel.clone())
                } else {
                    None
                }
            }
            SplitContainer::Split { children, .. } => {
                for child in children {
                    if let Some(panel) = child.get_selected_panel(cx) {
                        return Some(panel);
                    }
                }
                None
            }
        }
    }

    /// 分割面板
    pub fn split_panel(
        &mut self,
        target_panel_id: &str,
        direction: SplitDirection,
        new_panel: Entity<Panel>,
        cx: &App,
    ) -> bool {
        match self {
            SplitContainer::Panel(panel) => {
                if panel.read(cx).panel_id == target_panel_id {
                    // 将当前面板转换为分割容器
                    let old_panel = panel.clone();
                    *self = SplitContainer::Split {
                        direction,
                        children: vec![
                            SplitContainer::Panel(old_panel),
                            SplitContainer::Panel(new_panel),
                        ],
                        sizes: vec![0.5, 0.5],
                    };
                    true
                } else {
                    false
                }
            }
            SplitContainer::Split { children, .. } => {
                for child in children {
                    if child.split_panel(target_panel_id, direction, new_panel.clone(), cx) {
                        return true;
                    }
                }
                false
            }
        }
    }

    pub fn remove_panel(&self, panel_id: &str, cx: &App) -> Option<Self> {
        match self {
            SplitContainer::Panel(panel) => {
                if panel.read(cx).panel_id == panel_id {
                    None
                } else {
                    Some(self.clone())
                }
            }
            SplitContainer::Split {
                direction,
                children,
                sizes,
            } => {
                let mut new_children = Vec::new();
                let mut new_sizes = Vec::new();

                for (i, child) in children.iter().enumerate() {
                    if let SplitContainer::Panel(p) = child {
                        if p.read(cx).panel_id == panel_id {
                            continue;
                        }
                    }

                    if let Some(new_child) = child.remove_panel(panel_id, cx) {
                        new_children.push(new_child);
                        if let Some(size) = sizes.get(i) {
                            new_sizes.push(*size);
                        }
                    }
                }

                if new_children.is_empty() {
                    return None;
                }

                if new_children.len() == 1 {
                    Some(new_children[0].clone())
                } else {
                    Some(SplitContainer::Split {
                        direction: *direction,
                        children: new_children,
                        sizes: new_sizes,
                    })
                }
            }
        }
    }

    /// 在指定位置分割容器
    /// 在target_panel_id对应的panel处，按照placement方向插入new_container
    pub fn split_at(
        &self,
        target_panel_id: &str,
        placement: PanelPlacement,
        new_container: Self,
        cx: &App,
    ) -> Self {
        let direction = SplitDirection::from(placement);

        match self {
            SplitContainer::Panel(panel) => {
                if panel.read(cx).panel_id == target_panel_id {
                    // 找到目标panel，创建新的split
                    let children = match placement {
                        PanelPlacement::Left | PanelPlacement::Top => {
                            vec![new_container, self.clone()]
                        }
                        PanelPlacement::Right | PanelPlacement::Bottom => {
                            vec![self.clone(), new_container]
                        }
                    };

                    SplitContainer::Split {
                        direction,
                        children,
                        sizes: vec![1.0, 1.0],
                    }
                } else {
                    self.clone()
                }
            }
            SplitContainer::Split {
                direction: dir,
                children,
                sizes,
            } => {
                // 在子容器中递归查找
                let new_children: Vec<SplitContainer> = children
                    .iter()
                    .map(|child| {
                        child.split_at(target_panel_id, placement, new_container.clone(), cx)
                    })
                    .collect();

                SplitContainer::Split {
                    direction: *dir,
                    children: new_children,
                    sizes: sizes.clone(),
                }
            }
        }
    }

    /// 检查容器是否包含指定的panel
    pub fn contains_panel(&self, panel_id: &str, cx: &App) -> bool {
        match self {
            SplitContainer::Panel(panel) => panel.read(cx).panel_id == panel_id,
            SplitContainer::Split { children, .. } => {
                children.iter().any(|c| c.contains_panel(panel_id, cx))
            }
        }
    }

    /// 调整子容器的尺寸比例
    pub fn resize_child(&mut self, child_index: usize, delta: f32, min_size: f32) -> bool {
        match self {
            SplitContainer::Panel(_) => false,
            SplitContainer::Split {
                sizes, children, ..
            } => {
                if children.len() < 2 || child_index + 1 >= children.len() {
                    return false;
                }
                // 兜底：sizes 与 children 不一致时先修正，避免索引越界 panic
                // （正常路径下 from_session_state 已归一化）
                if sizes.len() != children.len() {
                    *sizes = normalize_sizes(sizes, children.len());
                }

                let current_left = sizes.get(child_index).copied().unwrap_or(1.0);
                let current_right = sizes.get(child_index + 1).copied().unwrap_or(1.0);

                let new_left = current_left + delta;
                let new_right = current_right - delta;

                if new_left < min_size || new_right < min_size {
                    return false;
                }

                sizes[child_index] = new_left;
                sizes[child_index + 1] = new_right;

                true
            }
        }
    }

    /// 通过路径递归调整子容器尺寸
    pub fn resize_at_path(
        &mut self,
        path: &[usize],
        child_index: usize,
        delta: f32,
        min_size: f32,
    ) -> bool {
        if path.is_empty() {
            return self.resize_child(child_index, delta, min_size);
        }

        match self {
            SplitContainer::Panel(_) => false,
            SplitContainer::Split { children, .. } => {
                let idx = path[0];
                if idx >= children.len() {
                    return false;
                }
                children[idx].resize_at_path(&path[1..], child_index, delta, min_size)
            }
        }
    }
}

impl From<SplitDirection> for SplitSessionDirection {
    fn from(direction: SplitDirection) -> Self {
        match direction {
            SplitDirection::Horizontal => SplitSessionDirection::Horizontal,
            SplitDirection::Vertical => SplitSessionDirection::Vertical,
        }
    }
}

impl From<SplitSessionDirection> for SplitDirection {
    fn from(direction: SplitSessionDirection) -> Self {
        match direction {
            SplitSessionDirection::Horizontal => SplitDirection::Horizontal,
            SplitSessionDirection::Vertical => SplitDirection::Vertical,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_sizes;

    #[test]
    fn keeps_valid_sizes() {
        assert_eq!(normalize_sizes(&[0.5, 0.5], 2), vec![0.5, 0.5]);
    }

    #[test]
    fn rescales_unnormalized_but_valid_sizes() {
        let out = normalize_sizes(&[2.0, 2.0], 2);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn repairs_length_mismatch() {
        // 会话文件损坏时的典型形态：children 有 2 个，sizes 只有 1 个
        assert_eq!(normalize_sizes(&[1.0], 2), vec![0.5, 0.5]);
        assert_eq!(normalize_sizes(&[], 3), vec![1.0 / 3.0; 3]);
    }

    #[test]
    fn repairs_invalid_values() {
        assert_eq!(normalize_sizes(&[f32::NAN, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(normalize_sizes(&[0.0, 0.0], 2), vec![0.5, 0.5]);
    }

    #[test]
    fn empty_children_yields_empty_sizes() {
        assert!(normalize_sizes(&[0.5], 0).is_empty());
    }
}
