use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 面板类型
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PanelType {
    Terminal,
    FileManager,
    LogViewer,
    Settings,
}

/// 分割方向
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// 面板配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PanelConfig {
    pub id: String,
    pub panel_type: PanelType,
    pub title: String,
    pub content: Option<String>,
}

/// 分割配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SplitConfig {
    pub direction: SplitDirection,
    pub panels: Vec<PanelConfig>,
    pub sizes: Vec<f32>,
}

/// 工作区模板
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tabs: Vec<TabConfig>,
    pub active_tab_index: usize,
}

/// 标签配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabConfig {
    pub id: String,
    pub label: String,
    pub split_config: Option<SplitConfig>,
    pub panel: Option<PanelConfig>,
}

/// 工作区状态
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub templates: HashMap<String, WorkspaceTemplate>,
    pub current_template_id: Option<String>,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            templates: HashMap::new(),
            current_template_id: None,
            window_width: 1400,
            window_height: 900,
        }
    }
}

impl WorkspaceTemplate {
    /// 创建默认模板
    pub fn default_template() -> Self {
        Self {
            id: "default".to_string(),
            name: "Default".to_string(),
            description: "Default workspace with single terminal".to_string(),
            tabs: vec![TabConfig {
                id: "tab-1".to_string(),
                label: "Terminal".to_string(),
                split_config: None,
                panel: Some(PanelConfig {
                    id: "panel-1".to_string(),
                    panel_type: PanelType::Terminal,
                    title: "Terminal".to_string(),
                    content: None,
                }),
            }],
            active_tab_index: 0,
        }
    }

    /// 创建开发工作区模板
    pub fn dev_workspace() -> Self {
        Self {
            id: "dev".to_string(),
            name: "Development".to_string(),
            description: "Development workspace with split view".to_string(),
            tabs: vec![
                TabConfig {
                    id: "tab-1".to_string(),
                    label: "Server".to_string(),
                    split_config: None,
                    panel: Some(PanelConfig {
                        id: "panel-1".to_string(),
                        panel_type: PanelType::Terminal,
                        title: "Server".to_string(),
                        content: None,
                    }),
                },
                TabConfig {
                    id: "tab-2".to_string(),
                    label: "Logs".to_string(),
                    split_config: None,
                    panel: Some(PanelConfig {
                        id: "panel-2".to_string(),
                        panel_type: PanelType::LogViewer,
                        title: "Logs".to_string(),
                        content: None,
                    }),
                },
            ],
            active_tab_index: 0,
        }
    }

    /// 创建分屏工作区模板
    pub fn split_workspace() -> Self {
        Self {
            id: "split".to_string(),
            name: "Split View".to_string(),
            description: "Split view with multiple terminals".to_string(),
            tabs: vec![TabConfig {
                id: "tab-1".to_string(),
                label: "Multi-Terminal".to_string(),
                split_config: Some(SplitConfig {
                    direction: SplitDirection::Horizontal,
                    panels: vec![
                        PanelConfig {
                            id: "panel-1".to_string(),
                            panel_type: PanelType::Terminal,
                            title: "Terminal 1".to_string(),
                            content: None,
                        },
                        PanelConfig {
                            id: "panel-2".to_string(),
                            panel_type: PanelType::Terminal,
                            title: "Terminal 2".to_string(),
                            content: None,
                        },
                    ],
                    sizes: vec![0.5, 0.5],
                }),
                panel: None,
            }],
            active_tab_index: 0,
        }
    }
}

/// 模板管理器
pub struct TemplateManager {
    state: WorkspaceState,
}

impl TemplateManager {
    pub fn new() -> Self {
        let mut manager = Self {
            state: WorkspaceState::default(),
        };

        // 注册默认模板
        manager.register_template(WorkspaceTemplate::default_template());
        manager.register_template(WorkspaceTemplate::dev_workspace());
        manager.register_template(WorkspaceTemplate::split_workspace());

        manager
    }

    pub fn register_template(&mut self, template: WorkspaceTemplate) {
        self.state.templates.insert(template.id.clone(), template);
    }

    pub fn get_template(&self, id: &str) -> Option<&WorkspaceTemplate> {
        self.state.templates.get(id)
    }

    pub fn get_all_templates(&self) -> Vec<&WorkspaceTemplate> {
        self.state.templates.values().collect()
    }

    pub fn set_current_template(&mut self, id: &str) {
        if self.state.templates.contains_key(id) {
            self.state.current_template_id = Some(id.to_string());
        }
    }

    pub fn current_template(&self) -> Option<&WorkspaceTemplate> {
        self.state
            .current_template_id
            .as_ref()
            .and_then(|id| self.state.templates.get(id))
    }

    pub fn remove_template(&mut self, id: &str) -> Option<WorkspaceTemplate> {
        self.state.templates.remove(id)
    }

    pub fn state(&self) -> &WorkspaceState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut WorkspaceState {
        &mut self.state
    }
}

impl Default for TemplateManager {
    fn default() -> Self {
        Self::new()
    }
}
