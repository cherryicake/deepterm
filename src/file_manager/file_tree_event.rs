use std::path::PathBuf;

use crate::file_manager::TransferRequest;
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
    /// 发起一次文件传输（下载到本地 / 拖拽上传等），由 workspace 的
    /// 传输协调器执行（进度、冲突、取消均复用统一链路）。
    /// `TransferRequest` 较大（含多源 endpoint），装箱保持事件枚举小体积。
    StartTransfer {
        request: Box<TransferRequest>,
    },
    /// 树内重命名已完成。编辑器据此改写已打开标签的路径，
    /// 否则保存时会把内容写回旧路径，把刚删掉的旧文件又复活出来。
    PathRenamed {
        from: PathBuf,
        to: PathBuf,
        ssh_config: Option<Box<SshConnectionConfig>>,
    },
    /// 树内删除已完成。编辑器据此关掉对应的标签。
    PathRemoved {
        path: PathBuf,
        ssh_config: Option<Box<SshConnectionConfig>>,
    },
}
