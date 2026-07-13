use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ssh::SshConnectionConfig;

pub const SESSION_STATE_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSessionState {
    pub version: u32,
    pub active_group_id: String,
    pub groups: Vec<WorkspaceGroupSessionState>,
}

impl Default for AppSessionState {
    fn default() -> Self {
        Self {
            version: SESSION_STATE_VERSION,
            active_group_id: crate::components::macos_sidebar::DEFAULT_TAB_GROUP_ID.to_string(),
            groups: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceGroupSessionState {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub collapsed: bool,
    pub workspace: Option<WorkspaceSessionState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSessionState {
    pub workspace_id: String,
    pub active_tab_id: String,
    pub next_tab_id: usize,
    pub next_panel_id: usize,
    pub local_terminal_count: usize,
    pub workspace_index: usize,
    pub tabs: Vec<TabSessionState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabSessionState {
    pub id: String,
    pub label: String,
    pub tab_type: TabSessionType,
    pub broadcast_enabled: bool,
    pub icon_os: Option<String>,
    pub container: SplitSessionState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TabSessionType {
    Normal,
    Workspace,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum SplitSessionDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SplitSessionState {
    Panel(PanelSessionState),
    Split {
        direction: SplitSessionDirection,
        children: Vec<SplitSessionState>,
        sizes: Vec<f32>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PanelSessionState {
    pub id: String,
    pub content: PanelContentSessionState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PanelContentSessionState {
    Terminal(TerminalSessionState),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TerminalSessionState {
    Local {
        cwd: Option<String>,
        command: Option<String>,
        args: Vec<String>,
    },
    Ssh {
        config: SshConnectionConfig,
        remote_cwd: Option<String>,
    },
}

impl TerminalSessionState {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    pub fn ssh_config(&self) -> Option<&SshConnectionConfig> {
        match self {
            Self::Ssh { config, .. } => Some(config),
            _ => None,
        }
    }

    /// 提取终端 CWD 路径和文件树显示名
    pub fn file_tree_target(&self) -> (Option<PathBuf>, String) {
        let default_label = match self {
            Self::Local { .. } => "本地文件树",
            Self::Ssh { .. } => "远程文件树",
        };
        let path = match self {
            Self::Local { cwd, .. } => cwd.as_ref().map(PathBuf::from),
            Self::Ssh { remote_cwd, .. } => remote_cwd.as_ref().map(PathBuf::from),
        };
        let label = path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(default_label)
            .to_string();
        (path, label)
    }
}
