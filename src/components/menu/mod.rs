use gpui::App;

mod context_menu;
mod menu_item;
mod popup_menu;

pub use context_menu::{ContextMenu, ContextMenuExt, ContextMenuState};
pub use menu_item::MenuItemElement;
pub use popup_menu::{PopupMenu, PopupMenuItem};

pub(crate) fn init(cx: &mut App) {
    popup_menu::init(cx);
}
