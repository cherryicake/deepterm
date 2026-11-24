// `editor/editor.rs` 是本仓库既定的模块布局（外层 `mod.rs` 汇总、实现放同名子文件）。
// 改名会波及全仓 `use crate::editor::editor::...` 路径，收益不足，因此显式允许该 lint。
#![allow(clippy::module_inception)]

pub mod completion;
pub mod diff;

pub mod editor;
pub mod encoding;
pub mod ex_command;
pub mod git_marks;
pub mod git_repository;
pub mod git_status;
pub mod goto_line;
pub mod hover;
pub mod image_viewer;
pub mod lsp_lamp;
pub mod lsp_log_view;
pub mod lsp_menu;
pub mod markdown;
pub mod markdown_element;
pub mod markdown_live;
pub mod markdown_parser;
pub mod markdown_selectable;
pub mod md_image;
pub mod rename;
pub mod symbols;
pub mod syntax;
pub use editor::*;

#[cfg(test)]
mod hover_sticky_tests;
#[cfg(test)]
mod lsp_jump_tests;
