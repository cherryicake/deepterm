// `terminal_view/terminal_view.rs` 是本仓库既定的模块布局（外层 `mod.rs` 汇总、实现放同名子文件）。
// 改名会波及全仓 `use crate::terminal_view::terminal_view::...` 路径，收益不足，因此显式允许该 lint。
#![allow(clippy::module_inception)]

pub mod blink_cursor;
pub mod box_drawing;
pub mod overlay_wrap;
pub mod terminal_element;
pub mod terminal_scrollbar;
pub mod terminal_view;

pub use terminal_element::TerminalElement;
pub use terminal_scrollbar::TerminalScrollbar;
pub use terminal_view::init;
pub use terminal_view::{BroadcastMsg, TerminalBlink, TerminalView};
