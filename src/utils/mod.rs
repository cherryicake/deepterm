//! 跨模块复用的底层小工具。
//!
//! 只收零内部依赖的通用实现，避免它们散落在 `src` 顶层；
//! 业务模块（theme、workspace、editor 等）都可安全地依赖这里，
//! 这里反过来不依赖任何业务模块。

pub mod console;
pub mod encoding;
pub mod fs_atomic;
pub mod logging;
pub mod path;
pub mod safe_open;
pub mod shell;

/// WSL 发行版识别与 Linux→UNC 路径翻译（WSL 标签页的文件树要用）。
pub mod wsl;

/// Windows 专用的「读他进程 cwd」（PEB）。单独成模块是因为整块都是 `unsafe`
/// 与架构相关偏移，隔离起来才好审。
#[cfg(windows)]
pub mod win_process_cwd;

/// 测试 / 基准专用的内存观测工具（phys_footprint 探针 + 分配计数器）。
/// 仅被 `#[cfg(test)]` 代码与 bench 引用，不进正式产物的任何逻辑路径。
#[cfg(any(test, feature = "alloc-probe"))]
pub mod alloc_probe;
