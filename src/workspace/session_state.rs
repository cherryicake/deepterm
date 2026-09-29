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

// 允许大枚举变体差异（中文说明）：
// `Split` 含递归 `Vec`，`Panel` 含终端会话（含 SSH 配置）。
// 会话状态仅在标签切换/持久化时克隆（低频），装箱会复杂化 `serde` 结构，
// 故显式允许。渲染热点不经过此枚举。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SplitSessionState {
    Panel(PanelSessionState),
    Split {
        direction: SplitSessionDirection,
        children: Vec<SplitSessionState>,
        sizes: Vec<f32>,
    },
}

impl SplitSessionState {
    /// 递归收集树内所有 panel id，供恢复会话时推算 id 序号，
    /// 避免新分配的 id 与恢复出来的 id 冲突。
    pub fn collect_panel_ids(&self, out: &mut Vec<String>) {
        match self {
            SplitSessionState::Panel(panel) => out.push(panel.id.clone()),
            SplitSessionState::Split { children, .. } => {
                for child in children {
                    child.collect_panel_ids(out);
                }
            }
        }
    }
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

impl PanelContentSessionState {
    /// 该面板内容的 SSH 配置（代理标记已合回；本地内容为 `None`）。
    pub fn ssh_config_with_proxy(&self) -> Option<SshConnectionConfig> {
        match self {
            Self::Terminal(state) => state.ssh_config_with_proxy(),
        }
    }
}

// 允许大枚举变体差异（中文说明）：
// `Ssh` 含完整 `SshConnectionConfig`（约 280B），`Local` 只有几个字符串。
// 该枚举仅在标签切换 / 会话持久化时克隆（低频），装箱会复杂化 `serde` 结构
// （会话文件是外部格式，不想为省几个字节改它），故与 `SplitSessionState`
// 同样显式允许。渲染热点不经过此枚举。
#[allow(clippy::large_enum_variant)]
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
        /// 该标签是否经系统代理连接（见 [`crate::ssh::system_proxy`]）。
        ///
        /// 与 `config` 分开存：`use_system_proxy` 在 `SshConnectionConfig` 上是
        /// `serde(skip)` 的（它是「本次连接」的选择，不是主机属性），因此
        /// 会话文件必须自己记住它，否则重启后海外服务器又变回直连。
        ///
        /// 缺省 `false`：旧会话文件没有这个字段，按直连处理。
        #[serde(default)]
        use_system_proxy: bool,
    },
}

impl TerminalSessionState {
    /// 构造一个 SSH 会话状态，代理标记从 `config` 取。
    ///
    /// 强制走这个构造器（而不是让调用方自己填 `use_system_proxy`）：
    /// `SshConnectionConfig::use_system_proxy` 是内存字段，而这里的字段要落盘，
    /// 两份必须一致——分开填迟早会漂移，表现为「重启后代理没了」。
    pub fn ssh(config: SshConnectionConfig, remote_cwd: Option<String>) -> Self {
        let use_system_proxy = config.use_system_proxy;
        Self::Ssh {
            config,
            remote_cwd,
            use_system_proxy,
        }
    }

    /// 该会话是否经系统代理连接。
    pub fn use_system_proxy(&self) -> bool {
        match self {
            Self::Ssh {
                use_system_proxy, ..
            } => *use_system_proxy,
            Self::Local { .. } => false,
        }
    }

    /// 远端工作目录（本地会话为 `None`）。
    pub fn remote_cwd(&self) -> Option<String> {
        match self {
            Self::Ssh { remote_cwd, .. } => remote_cwd.clone(),
            Self::Local { .. } => None,
        }
    }

    /// 取出配置并还原代理标记（克隆版）。
    ///
    /// `config` 本身是 `serde(skip)` 的，从会话文件读回来恒为 `false`；标记存在
    /// 同级字段里，因此**必须**在这里合回去，否则恢复出来的终端是直连。
    ///
    /// 返回克隆而不是 `&self`：调用方（重连 / 恢复 / 复制标签）需要把配置交给
    /// 新终端，借用形态会在 `match` 里与 `remote_cwd` 的移动冲突。
    pub fn ssh_config_with_proxy(&self) -> Option<SshConnectionConfig> {
        match self {
            Self::Ssh {
                config,
                use_system_proxy,
                ..
            } => {
                let mut config = config.clone();
                config.use_system_proxy = *use_system_proxy;
                Some(config)
            }
            Self::Local { .. } => None,
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::config::AuthMethod;

    /// 代理标记必须经会话文件往返。
    ///
    /// 回归用例：`SshConnectionConfig::use_system_proxy` 是 `serde(skip)` 的
    /// （它是「本次连接」的选择，不是主机属性），因此会话文件必须自己存一份。
    /// 不存的话，海外服务器标签重启后静默变回直连——用户点了「使用系统代理
    /// 连接」，下次打开却在直连，且没有任何提示。
    #[test]
    fn proxy_choice_round_trips_through_the_session_file() {
        let mut live = SshConnectionConfig::new("overseas.example.com", "root", AuthMethod::Agent);
        live.use_system_proxy = true;

        let saved = TerminalSessionState::ssh(live.clone(), Some("/home/root".to_string()));
        let json = serde_json::to_string(&saved).expect("会话状态应可序列化");
        assert!(
            json.contains("\"use_system_proxy\":true"),
            "会话文件必须记住代理标记: {json}"
        );

        let restored: TerminalSessionState = serde_json::from_str(&json).expect("应可反序列化");
        assert!(restored.use_system_proxy(), "读回后标记应为 true");
        let config = restored.ssh_config_with_proxy().expect("Ssh 分支应有配置");
        assert!(
            config.use_system_proxy,
            "恢复出的配置必须带代理标记（否则重启后退回直连）"
        );
        assert_eq!(config.host, "overseas.example.com", "其余字段不受影响");
        assert_eq!(restored.remote_cwd().as_deref(), Some("/home/root"));
    }

    /// 旧会话文件（没有 `use_system_proxy` 字段）必须照常加载并默认直连。
    #[test]
    fn legacy_session_file_without_the_proxy_field_loads_as_direct() {
        let legacy = r#"{"Ssh":{"config":{"host":"old.example.com","port":22,"username":"root","auth_type":"agent","nickname":null,"extra_args":null,"timeout":30,"save_config":false,"os":null,"port_forwards":[],"group_id":null,"sort_order":null,"created_at":"2026-01-01T00:00:00Z"},"remote_cwd":null}}"#;

        let restored: TerminalSessionState =
            serde_json::from_str(legacy).expect("旧会话文件必须可读");
        assert!(!restored.use_system_proxy(), "缺字段时按直连处理");
        let config = restored.ssh_config_with_proxy().expect("Ssh 分支应有配置");
        assert!(!config.use_system_proxy, "旧文件恢复出的配置是直连");
        assert_eq!(config.host, "old.example.com");
    }

    /// 直连的会话不会凭空带上代理。
    #[test]
    fn direct_session_does_not_gain_the_proxy_flag() {
        let plain = SshConnectionConfig::new("direct.example.com", "root", AuthMethod::Agent);
        let state = TerminalSessionState::ssh(plain, None);
        assert!(!state.use_system_proxy());
        assert!(!state.ssh_config_with_proxy().unwrap().use_system_proxy);
    }

    /// 本地会话恒为直连，且没有配置可取。
    #[test]
    fn local_session_is_never_proxied() {
        let state = TerminalSessionState::Local {
            cwd: Some("/tmp".to_string()),
            command: None,
            args: Vec::new(),
        };
        assert!(!state.use_system_proxy());
        assert!(state.ssh_config_with_proxy().is_none());
    }
}
