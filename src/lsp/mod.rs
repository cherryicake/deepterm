//! 语言服务器协议（LSP）客户端。
//!
//! 分层（参照 Zed 的 `lsp` / `project::lsp_store`，按本项目体量收敛）：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`jsonrpc`] | JSON-RPC 帧编解码 + 子进程 stdio 传输，不含 LSP 语义 |
//! | [`registry`] | 语言 → 服务器命令的内置默认表 + `settings.json` 覆盖 |
//! | [`client`] | `initialize` 握手、文档同步、各类 `textDocument/*` 请求 |
//! | [`project`] | 项目根探测 + `LspManager`（按 root 复用服务器实例） |
//! | [`features`] | LSP 响应 → 编辑器数据结构的转换 |
//!
//! 约定：
//! - **只服务本地文件**。远程（SSH）标签不启动服务器：服务器进程跑在本地，
//!   看到的路径与远程文件不一致，语义上不成立。
//! - **失败一律降级**。服务器缺失、启动失败、请求出错都只记日志，
//!   编辑器退回「纯 tree-sitter」状态，绝不弹窗或阻塞输入。

pub mod client;
pub mod features;
pub mod glob;
pub mod jsonrpc;
pub mod project;
pub mod registry;
pub mod session;
pub mod status;

#[cfg(test)]
mod feature_tests;
#[cfg(test)]
mod integration_tests;
// 协议层用例用一个 `python3` 脚本当替身服务器，存活探测走 `libc::kill`。
// 两者都是 POSIX 专有（`libc` 只在 `cfg(unix)` 下依赖），故整模块按 unix 裁剪。
// 被验的协议行为本身与平台无关——换掉替身实现即可在 Windows 上跑，
// 但那属于另写一份用例，不在「让本机能编译」的范围内。
#[cfg(all(test, unix))]
mod protocol_tests;

pub use client::LspClient;
pub use features::*;
pub use project::{LspManager, project_root};
pub use registry::{ServerConfig, server_for_language};
pub use session::LspSession;
pub use status::{LspStatus, ProgressTracker};

use gpui::App;

/// 初始化 LSP 子系统：注册全局管理器。
pub fn init(cx: &mut App) {
    LspManager::init(cx);
}
