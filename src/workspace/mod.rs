pub mod client_decorations;
pub mod panel;
pub mod persistence;
pub mod session_state;
pub mod split_manager;
pub mod transfer_coordinator;
pub mod tab_bar;
pub mod transfer_bell;
pub mod window_manager;
pub mod workspace;
pub(crate) use workspace::init;

use gpui::actions;
actions!(workspace_actions, [ToggleSidebar]);
