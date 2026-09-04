//! 进程内通用的原子写文件工具：先写同目录临时文件，再 rename 覆盖目标。
//!
//! 统一 `workspace::persistence`、`theme` 等所有落盘点的写入标准，
//! 避免各处自写一套导致行为漂移（此前主题偏好是直写 `fs::write`，
//! 崩溃时可能留下截断 JSON）。

use std::io;
use std::path::Path;

/// 原子写入：临时文件名含 pid + 纳秒时间戳，同进程快速连续写、
/// 以及多窗口（不同 pid）并发写都不会碰撞同一个 tmp 路径。
pub fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), nanos));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}
