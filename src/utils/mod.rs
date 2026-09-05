//! 跨模块复用的底层小工具。
//!
//! 只收零内部依赖的通用实现，避免它们散落在 `src` 顶层；
//! 业务模块（theme、workspace、editor 等）都可安全地依赖这里，
//! 这里反过来不依赖任何业务模块。

pub mod fs_atomic;
pub mod safe_open;
pub mod shell;
