use gpui::{
    div, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled,
    Window,
};

use crate::workspace::panel::{Panel, PanelPlacement};

/// 分割方向
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
