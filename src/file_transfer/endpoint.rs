use std::path::PathBuf;

use crate::ssh::SshConnectionConfig;

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
}
