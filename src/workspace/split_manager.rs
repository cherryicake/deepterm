use gpui::prelude::*;
use gpui::{
    div, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled,
    Window,
};

use crate::workspace::panel::{Panel, PanelPlacement};

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

    /// 获取Panel实体
    pub fn get_panel_entity(&self, cx: &App) -> Option<Entity<Panel>> {
        match self {
            SplitContainer::Panel(panel) => Some(panel.clone()),
            _ => None,
        }
    }

    /// 查找面板
    pub fn find_panel(&self, panel_id: &str, cx: &App) -> Option<Entity<Panel>> {
        match self {
            SplitContainer::Panel(panel) => {
                if panel.read(cx).panel_id() == panel_id {
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

    /// 收集所有面板
    pub fn collect_panels(&self, cx: &App, panels: &mut Vec<Entity<Panel>>) {
        match self {
            SplitContainer::Panel(panel) => panels.push(panel.clone()),
            SplitContainer::Split { children, .. } => {
                for child in children {
                    child.collect_panels(cx, panels);
                }
            }
        }
    }

    /// 收集所有面板（不需要Context）
    pub fn collect_panels_vec(&self) -> Vec<Entity<Panel>> {
        let mut panels = Vec::new();
        self.collect_panels_vec_recursive(&mut panels);
        panels
    }

    pub fn collect_panels_vec_recursive(&self, panels: &mut Vec<Entity<Panel>>) {
        match self {
            SplitContainer::Panel(panel) => panels.push(panel.clone()),
            SplitContainer::Split { children, .. } => {
                for child in children {
                    child.collect_panels_vec_recursive(panels);
                }
            }
        }
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
                if panel.read(cx).panel_id() == target_panel_id {
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

    /// 从容器中移除指定的panel
    /// 返回新的容器，如果移除后只剩一个子容器，则提升该子容器
    pub fn remove_panel(&self, panel_id: &str, cx: &App) -> Self {
        match self {
            SplitContainer::Panel(panel) => {
                if panel.read(cx).panel_id() == panel_id {
                    // 找到了要移除的panel，返回克隆（调用者应该处理这种情况）
                    self.clone()
                } else {
                    self.clone()
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
                    // 检查这个child是否包含要移除的panel
                    if let SplitContainer::Panel(p) = child {
                        if p.read(cx).panel_id() == panel_id {
                            // 跳过这个panel
                            continue;
                        }
                    }

                    let new_child = child.remove_panel(panel_id, cx);
                    new_children.push(new_child);
                    if let Some(size) = sizes.get(i) {
                        new_sizes.push(*size);
                    }
                }

                // 如果只剩一个子容器，提升它
                if new_children.len() == 1 {
                    new_children[0].clone()
                } else {
                    SplitContainer::Split {
                        direction: *direction,
                        children: new_children,
                        sizes: new_sizes,
                    }
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
                if panel.read(cx).panel_id() == target_panel_id {
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
                        if child.contains_panel(target_panel_id, cx) {
                            child.split_at(target_panel_id, placement, new_container.clone(), cx)
                        } else {
                            child.clone()
                        }
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
            SplitContainer::Panel(panel) => panel.read(cx).panel_id() == panel_id,
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
                if children.len() < 2 || child_index >= children.len() - 1 {
                    return false;
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

/// 分割管理器
pub struct SplitManager {
    root: SplitContainer,
    focus_handle: FocusHandle,
    next_panel_id: usize,
}

impl SplitManager {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();

        // 创建一个临时的空面板，稍后会被替换
        let temp_panel =
            cx.new(|cx| crate::workspace::panel::Panel::new("temp", "temp-group", None, cx));

        Self {
            root: SplitContainer::new_panel(temp_panel),
            focus_handle,
            next_panel_id: 0,
        }
    }

    /// 初始化根面板
    pub fn init_root(&mut self, panel: Entity<Panel>) {
        self.root = SplitContainer::new_panel(panel);
        self.next_panel_id = 1;
    }

    /// 获取根容器
    pub fn root(&self) -> &SplitContainer {
        &self.root
    }

    /// 获取根容器（可变）
    pub fn root_mut(&mut self) -> &mut SplitContainer {
        &mut self.root
    }

    /// 生成新面板 ID
    pub fn generate_panel_id(&mut self) -> String {
        let id = format!("panel-{}", self.next_panel_id);
        self.next_panel_id += 1;
        id
    }

    /// 为指定workspace组生成panel ID
    pub fn generate_panel_id_for_workspace(&self, workspace_id: &str) -> String {
        format!("{}-panel-{}", workspace_id, self.next_panel_id)
    }

    /// 收集所有面板
    pub fn collect_panels(&self, cx: &App) -> Vec<Entity<Panel>> {
        let mut panels = Vec::new();
        self.root.collect_panels(cx, &mut panels);
        panels
    }
}

impl Focusable for SplitManager {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SplitManager {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // SplitManager 本身不渲染，由 Workspace 来渲染
        div().size_full()
    }
}
