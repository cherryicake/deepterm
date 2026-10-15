pub mod completion;
pub mod diff_model;
pub mod diff_view;
pub mod editor;
pub mod encoding;
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
pub mod rename;
pub mod symbols;
pub mod syntax;
pub use editor::*;

#[cfg(test)]
mod hover_sticky_tests;
#[cfg(test)]
mod lsp_jump_tests;
