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

    pub fn is_same_device(&self, other: &FileEndpoint) -> bool {
        match (self, other) {
            (Self::Local { .. }, Self::Local { .. }) => true,
            (Self::Remote { ssh_config: a, .. }, Self::Remote { ssh_config: b, .. }) => a == b,
            _ => false,
        }
    }
}
