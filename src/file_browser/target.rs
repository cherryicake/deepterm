use std::path::PathBuf;

use crate::ssh::SshConnectionConfig;

#[derive(Clone, Debug)]
pub enum FileBrowserTarget {
    Local {
        requested_path: Option<PathBuf>,
    },
    Remote {
        ssh_config: SshConnectionConfig,
        requested_path: Option<String>,
    },
}
