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

                // 落盘前归一化：老会话里可能还留着 `[1.0, 1.0]` 这种
                // 总和为 2 的比例。恢复侧虽然也会归一化，但写回一份
                // 规范的 sizes 能让会话文件自洽，避免下次读取时再修。
                let sizes = normalize_sizes(sizes, children.len());

                Some(SplitSessionState::Split {
                    direction: (*direction).into(),
                    children,
                    sizes,
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
                    if let SplitContainer::Panel(p) = child
                        && p.read(cx).panel_id == panel_id
                    {
                        continue;
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
                    let child_count = new_children.len();
                    Some(SplitContainer::Split {
                        direction: *direction,
                        children: new_children,
                        // 删掉一格后剩余 sizes 的总和会小于 1。若不归一化，
                        // 剩下的面板按原比例只占容器的一部分，末尾留出空白，
                        // 重启后 `from_session_state` 又会归一化，间距/宽度
                        // 前后不一致。这里当场归一，保证运行时与恢复一致。
                        sizes: normalize_sizes(&new_sizes, child_count),
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
                        // 用归一化后的比例（总和 1）。写 [1.0, 1.0] 时子项
                        // flex-basis 总和是 200%，溢出会把中间的分隔条一起
                        // 压窄；会话恢复时 sizes 又被归一化成 1.0，分隔条
                        // 不再被压，于是「重启后间距变大」。统一用
                        // [0.5, 0.5] 让新建与恢复走同一套布局。
                        sizes: vec![0.5, 0.5],
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
    /// 从当前选中 panel 出发，向指定方向寻找相邻 panel（分屏导航）。
    ///
    /// 纯结构遍历：在每一层，若分割轴与导航方向一致，则取当前子树紧邻的兄弟：
    /// - 左/上 → 左侧/上方兄弟，并在其子树中取视觉最近的 panel（最右/最下）
    /// - 右/下 → 右侧/下方兄弟，取其子树最左/最上的 panel
    /// 若当前层轴不匹配或已到边缘，则向父层继续找。找不到返回 None。
    pub fn focus_neighbor(
        &self,
        from_panel_id: &str,
        nav: SplitNavDirection,
        cx: &App,
    ) -> Option<Entity<Panel>> {
        let (want_axis, want_before) = match nav {
            SplitNavDirection::Left => (SplitDirection::Horizontal, true),
            SplitNavDirection::Right => (SplitDirection::Horizontal, false),
            SplitNavDirection::Up => (SplitDirection::Vertical, true),
            SplitNavDirection::Down => (SplitDirection::Vertical, false),
        };

        let mut path = Vec::new();
        if !find_path_to_panel(self, from_panel_id, cx, &mut path) {
            return None;
        }

        // 从最内层向外：找第一个轴向匹配且有相邻兄弟的层
        for entry in path.iter().rev() {
            if entry.axis != want_axis {
                continue;
            }
            let neighbor_index = if want_before {
                match entry.index.checked_sub(1) {
                    Some(i) => i,
                    None => continue, // 已是最左侧兄弟，向父层继续
                }
            } else if entry.index + 1 < entry.children.len() {
                entry.index + 1
            } else {
                continue;
            };
            let neighbor = &entry.children[neighbor_index];
            return corner_panel(neighbor, want_before);
        }
        None
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

/// 分屏导航方向（与容器分割轴分离：左右/上下均可导航）
#[derive(Clone, Copy, Debug)]
pub enum SplitNavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// find_path 的路径条目：记录父分割的轴、兄弟序列、当前子树的下标
struct SplitPathEntry<'a> {
    axis: SplitDirection,
    children: &'a [SplitContainer],
    index: usize,
}

/// 找到 panel 所在的从根到叶的路径（存每一层的兄弟序列与下标）
fn find_path_to_panel<'a>(
    container: &'a SplitContainer,
    panel_id: &str,
    cx: &App,
    path: &mut Vec<SplitPathEntry<'a>>,
) -> bool {
    match container {
        SplitContainer::Panel(panel) => panel.read(cx).panel_id == panel_id,
        SplitContainer::Split {
            direction,
            children,
            ..
        } => {
            for (i, _child) in children.iter().enumerate() {
                path.push(SplitPathEntry {
                    axis: *direction,
                    children: children.as_slice(),
                    index: i,
                });
                if find_path_to_panel(&children[i], panel_id, cx, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
    }
}

/// 取子树中的角 panel：want_last=true 取最后一个（最右/最下），否则第一个（最左/最上）。
///
/// 空 `Split`（理论上不应出现，但外部构造或反序列化失误时可能发生）返回 `None`，
/// 而不是 `children.len() - 1` 下溢 panic。
fn corner_panel(container: &SplitContainer, want_last: bool) -> Option<Entity<Panel>> {
    match container {
        SplitContainer::Panel(panel) => Some(panel.clone()),
        SplitContainer::Split { children, .. } => {
            let next = if want_last {
                children.last()
            } else {
                children.first()
            }?;
            corner_panel(next, want_last)
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
    use super::{SplitContainer, SplitDirection, normalize_sizes};
    use crate::workspace::panel::{Panel, PanelPlacement};
    use gpui::AppContext;

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

    /// 新建分屏的比例必须总和为 1。
    ///
    /// 回归用例：`split_at` 曾写 `[1.0, 1.0]`（总和 2）。子项 flex-basis
    /// 总和 200% 时，固定厚度的分隔条也会被 flex-shrink 压窄（12px → 约 6px）；
    /// 而重启恢复时 `normalize_sizes` 把比例归一到总和 1，分隔条不再被压，
    /// 表现为「重启后面板间距变大」。新建与恢复必须用同一种规范。
    #[gpui::test]
    fn split_at_uses_canonical_sizes(cx: &mut gpui::TestAppContext) {
        let first = cx.new(|cx| Panel::new("panel-0", "ws", None, cx));
        let second = cx.new(|cx| Panel::new("panel-1", "ws", None, cx));

        let container = cx.update(|cx| {
            SplitContainer::Panel(first).split_at(
                "panel-0",
                PanelPlacement::Right,
                SplitContainer::Panel(second),
                cx,
            )
        });

        let SplitContainer::Split {
            children, sizes, ..
        } = container
        else {
            panic!("split_at 应产生 Split 容器");
        };
        assert_eq!(children.len(), 2);
        assert_eq!(sizes, vec![0.5, 0.5]);
        let sum: f32 = sizes.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "sizes 必须归一到总和 1，实际 {sizes:?}"
        );
    }

    /// 删掉一格后，剩余比例要重新归一化，否则剩余面板只占容器的一部分，
    /// 末尾留白；重启恢复又会归一化，前后布局不一致。
    #[gpui::test]
    fn removing_a_panel_renormalizes_sizes(cx: &mut gpui::TestAppContext) {
        let p0 = cx.new(|cx| Panel::new("panel-0", "ws", None, cx));
        let p1 = cx.new(|cx| Panel::new("panel-1", "ws", None, cx));
        let p2 = cx.new(|cx| Panel::new("panel-2", "ws", None, cx));
        let container = SplitContainer::Split {
            direction: SplitDirection::Horizontal,
            children: vec![
                SplitContainer::Panel(p0),
                SplitContainer::Panel(p1),
                SplitContainer::Panel(p2),
            ],
            sizes: vec![1.0 / 3.0; 3],
        };

        let after = cx
            .update(|cx| container.remove_panel("panel-1", cx))
            .expect("还剩两个面板");
        let SplitContainer::Split { sizes, .. } = after else {
            panic!("剩两个面板时应保持 Split 容器");
        };
        assert_eq!(sizes.len(), 2);
        let sum: f32 = sizes.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "删除后剩余 sizes 必须归一化，实际 {sizes:?}"
        );
    }
}
