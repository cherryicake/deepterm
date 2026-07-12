use std::path::PathBuf;

use crate::ssh::SshConnectionConfig;

#[derive(Clone, Debug)]
pub enum FileTreeEvent {
    OpenFile {
        path: PathBuf,
        ssh_config: Option<SshConnectionConfig>,
    },
}
