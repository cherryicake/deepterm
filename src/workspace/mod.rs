// `workspace/workspace.rs` 是本仓库既定的模块布局（外层 `mod.rs` 汇总、实现放同名子文件）。
// 改名会波及全仓 `use crate::workspace::workspace::...` 路径，收益不足，因此显式允许该 lint。
#![allow(clippy::module_inception)]

pub mod client_decorations;
pub mod panel;
pub mod persistence;
pub mod session_state;
pub mod sidebar;
pub mod split_manager;
pub mod tab_bar;
pub mod title_bar;
pub mod transfer_bell;
pub mod transfer_coordinator;
pub mod window_manager;
pub mod workspace;
pub use workspace::init;

use gpui::actions;
actions!(workspace_actions, [ToggleSidebar]);
