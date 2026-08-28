use std::path::PathBuf;

use crate::ssh::SshConnectionConfig;

/// `SshConnectionConfig` 有 184 字节；装箱后整个枚举从 208 字节降到 32 字节，
/// `RootChanged` 这类高频小事件不必再跟着付拷贝代价。
#[derive(Clone, Debug)]
pub enum FileTreeEvent {
    OpenFile {
        path: PathBuf,
        ssh_config: Option<Box<SshConnectionConfig>>,
    },
    RootChanged,
}
