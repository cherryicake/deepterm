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

    /// 这个本地会话是不是一个 WSL 标签页。
    ///
    /// **只在 Windows 上真为 true。** 理由是代价不对称：判真的代价是给普通 shell
    /// 注入 WSLENV（见 [`crate::terminal::terminal`]）以及把一条 `/home/...`
    /// 翻成 Windows 上打不开的 `\\wsl$\...`；判假的代价只是 WSL 标签页少了
    /// 文件树换根。unix/macOS 上没有 WSL 标签页，恒为 false 就是正确行为。
    ///
    /// `args` 仅为签名对称（发行版名可能藏在参数里，见
    /// [`crate::utils::wsl::distro_from_args`]）；判定本身只看命令名。
    #[cfg(windows)]
    fn is_wsl_session(command: Option<&str>, _args: &[String]) -> bool {
        command.is_some_and(crate::utils::wsl::is_wsl_command)
    }

    #[cfg(not(windows))]
    fn is_wsl_session(_command: Option<&str>, _args: &[String]) -> bool {
        false
    }

    /// 提取终端 CWD 路径和文件树显示名
    ///
    /// 返回的 `path` 是**可直接浏览**的：WSL 标签页的 cwd 是 Linux 路径
    /// （`/home/you/project`），在 Windows 上打不开，这里翻译成
    /// `\\wsl$\<distro>\home\you\project`——同一个 `LocalFileSystem` 就能列
    /// 目录，不需要为 WSL 写第二套后端。显示名仍取 Linux 路径的最后一段。
    ///
    /// 非 Windows 平台上没有 WSL 标签页（见 [`Self::is_wsl_session`]），行为与
    /// 加 WSL 支持之前完全一致。
    pub fn file_tree_target(&self) -> (Option<PathBuf>, String) {
        let default_label = match self {
            Self::Local { .. } => "本地文件树",
            Self::Ssh { .. } => "远程文件树",
        };
        let (path, label_path) = match self {
            Self::Local { cwd, command, args } => {
                let raw = cwd.as_ref().map(PathBuf::from);
                if Self::is_wsl_session(command.as_deref(), args) {
                    // 发行版没在参数里显式给时，就是默认发行版。
                    let distro = crate::utils::wsl::distro_from_args(args)
                        .or_else(|| crate::utils::wsl::default_distro().map(str::to_string));
                    let browsable = raw
                        .as_deref()
                        .and_then(|p| crate::utils::wsl::to_browsable_path(distro.as_deref(), p));
                    (browsable, raw)
                } else {
                    (raw.clone(), raw)
                }
            }
            Self::Ssh { remote_cwd, .. } => {
                let raw = remote_cwd.as_ref().map(PathBuf::from);
                (raw.clone(), raw)
            }
        };
        let label = label_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
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

    /// WSL 标签页的文件树根必须是 **UNC** 形式，否则在 Windows 上打不开。
    ///
    /// `cwd` 是 Linux 侧路径（`/home/you/project`），而 Windows 只有
    /// `\\wsl$\<distro>\home\you\project` 能列目录。
    ///
    /// 仅 windows：非 Windows 上没有 WSL 标签页，翻译分支根本不会进（见
    /// [`TerminalSessionState::is_wsl_session`]），跨平台跑只会断言一个在那些
    /// 平台上不该发生的行为。
    #[cfg(windows)]
    #[test]
    fn wsl_local_tree_target_is_a_browsable_unc_path() {
        let state = TerminalSessionState::Local {
            cwd: Some("/home/you/project".to_string()),
            command: Some("wsl.exe".to_string()),
            args: vec!["-d".to_string(), "Ubuntu-24.04".to_string()],
        };
        let (path, label) = state.file_tree_target();
        assert_eq!(
            path.as_deref(),
            Some(std::path::Path::new(
                r"\\wsl$\Ubuntu-24.04\home\you\project"
            )),
            "WSL 的 Linux 路径必须翻成 UNC，否则文件树打不开"
        );
        assert_eq!(label, "project", "显示名取 Linux 路径最后一段");
    }

    /// **非 Windows 上叫 `wsl` 的命令不得被当成 WSL 标签页**。
    ///
    /// 这是「WSL 支持不外溢到其他平台」的回归钉子：unix 上完全可能有个可执行文件
    /// 就叫 `wsl`。一旦被误判，`/home/you/project` 会被翻成
    /// `\\wsl$\<distro>\home\you\project`——在 unix 上根本打不开，文件树直接废掉。
    #[cfg(not(windows))]
    #[test]
    fn a_command_named_wsl_is_not_a_wsl_session_off_windows() {
        let state = TerminalSessionState::Local {
            cwd: Some("/home/you/project".to_string()),
            command: Some("/usr/local/bin/wsl".to_string()),
            args: vec!["-d".to_string(), "Ubuntu-24.04".to_string()],
        };
        let (path, label) = state.file_tree_target();
        assert_eq!(
            path.as_deref(),
            Some(std::path::Path::new("/home/you/project")),
            "非 Windows 上不得翻译成 UNC 路径"
        );
        assert_eq!(label, "project");
    }

    /// 非 WSL 的本地终端不受翻译影响。
    #[test]
    fn plain_local_tree_target_is_unchanged() {
        let state = TerminalSessionState::Local {
            cwd: Some(r"C:\Users\you\project".to_string()),
            command: Some("powershell.exe".to_string()),
            args: vec![],
        };
        let (path, label) = state.file_tree_target();
        assert_eq!(
            path.as_deref(),
            Some(std::path::Path::new(r"C:\Users\you\project"))
        );
        assert_eq!(label, "project");
    }
}
