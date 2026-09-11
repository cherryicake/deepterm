use std::path::PathBuf;

use crate::ssh::SshConnectionConfig;

// 允许大枚举变体差异（中文说明）：
// `Remote` 含完整 `SshConnectionConfig`（约 248B），`Local` 仅 `PathBuf`。
// 该枚举用于传输任务描述（低频克隆，非每帧热点），装箱会增加堆分配与解引用成本，
// 且需改动全部构造点，收益低于风险，故显式允许。传输热点已用 `Arc`/`引用` 优化。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum FileEndpoint {
    Local {
        path: PathBuf,
    },
    Remote {
        ssh_config: SshConnectionConfig,
        path: String,
    },
}

impl FileEndpoint {
    pub fn display_name(&self) -> String {
        match self {
            Self::Local { path } => path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string()),
            Self::Remote { path, .. } => {
                let trimmed = path.trim_end_matches('/');
                if trimmed.is_empty() {
                    "/".to_string()
                } else {
                    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
                }
            }
        }
    }

    /// endpoint 的路径（远端按原样转成 `PathBuf`，便于与文件树的路径比较）。
    pub fn path_buf(&self) -> PathBuf {
        match self {
            Self::Local { path } => path.clone(),
            Self::Remote { path, .. } => PathBuf::from(path),
        }
    }

    /// 该 endpoint 是否位于 `is_local` / `ssh_config` 描述的同一个文件系统上。
    /// 用于「传输完成后刷新目录」时筛掉与当前视图无关的那一端。
    pub fn matches_fs(&self, is_local: bool, ssh_config: Option<&SshConnectionConfig>) -> bool {
        match self {
            Self::Local { .. } => is_local,
            Self::Remote {
                ssh_config: config, ..
            } => !is_local && ssh_config == Some(config),
        }
    }

    /// 取所在目录：Local 用 `parent()`，远端按 `/` 截断最后一段。
    pub fn parent_dir(&self) -> Option<FileEndpoint> {
        match self {
            Self::Local { path } => path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| FileEndpoint::Local {
                    path: p.to_path_buf(),
                }),
            Self::Remote { ssh_config, path } => {
                let trimmed = path.trim_end_matches('/');
                let parent = trimmed.rsplit_once('/').map(|(head, _)| head);
                let parent = match parent {
                    Some("") => "/",
                    Some(p) => p,
                    None => return None,
                };
                Some(FileEndpoint::Remote {
                    ssh_config: ssh_config.clone(),
                    path: parent.to_string(),
                })
            }
        }
    }

    pub fn is_same_device(&self, other: &FileEndpoint) -> bool {
        match (self, other) {
            (Self::Local { .. }, Self::Local { .. }) => true,
            (Self::Remote { ssh_config: a, .. }, Self::Remote { ssh_config: b, .. }) => a == b,
            _ => false,
        }
    }
}
