use gpui::{App, Axis};

mod context_menu;
mod dropdown_menu;
mod menu_item;
mod popup_menu;

pub use context_menu::{ContextMenu, ContextMenuExt, ContextMenuState};
pub use dropdown_menu::{DropdownMenu, DropdownMenuPopover};
pub use menu_item::MenuItemElement;
pub use popup_menu::{PopupMenu, PopupMenuItem};

/// Anchor position for menus and popups.
///
/// This replaces the old `gpui::Anchor` enum which was removed from GPUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    /// Top-left corner
    #[default]
    TopLeft,
    /// Top-center edge
    TopCenter,
    /// Top-right corner
    TopRight,
    /// Left-center edge
    LeftCenter,
    /// Center
    Center,
    /// Right-center edge
    RightCenter,
    /// Bottom-left corner
    BottomLeft,
    /// Bottom-center edge
    BottomCenter,
    /// Bottom-right corner
    BottomRight,
}

impl Anchor {
    /// Flip the anchor along the given axis.
    pub fn other_side_along(self, axis: Axis) -> Self {
        match axis {
            Axis::Vertical => match self {
                Anchor::TopLeft => Anchor::BottomLeft,
                Anchor::TopCenter => Anchor::BottomCenter,
                Anchor::TopRight => Anchor::BottomRight,
                Anchor::LeftCenter => Anchor::LeftCenter,
                Anchor::Center => Anchor::Center,
                Anchor::RightCenter => Anchor::RightCenter,
                Anchor::BottomLeft => Anchor::TopLeft,
                Anchor::BottomCenter => Anchor::TopCenter,
                Anchor::BottomRight => Anchor::TopRight,
            },
            Axis::Horizontal => match self {
                Anchor::TopLeft => Anchor::TopRight,
                Anchor::TopCenter => Anchor::TopCenter,
                Anchor::TopRight => Anchor::TopLeft,
                Anchor::LeftCenter => Anchor::RightCenter,
                Anchor::Center => Anchor::Center,
                Anchor::RightCenter => Anchor::LeftCenter,
                Anchor::BottomLeft => Anchor::BottomRight,
                Anchor::BottomCenter => Anchor::BottomCenter,
                Anchor::BottomRight => Anchor::BottomLeft,
            },
        }
    }
}

pub(crate) fn init(cx: &mut App) {
    popup_menu::init(cx);
}
