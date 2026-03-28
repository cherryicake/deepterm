use gpui::{
    Context, Corner, ElementId, InteractiveElement, IntoElement, RenderOnce, SharedString, Window,
};

use crate::components::button::Selectable;
use crate::components::menu::PopupMenu;

/// A dropdown menu trait for buttons and other interactive elements
pub trait DropdownMenu: Selectable + InteractiveElement + IntoElement + 'static {
    /// Create a dropdown menu with the given items, anchored to the TopLeft corner
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        self.dropdown_menu_with_anchor(Corner::TopLeft, f)
    }

    /// Create a dropdown menu with the given items, anchored to the given corner
    fn dropdown_menu_with_anchor(
        self,
        anchor: impl Into<Corner>,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        DropdownMenuPopover::new(0.into(), anchor, self, f)
    }
}

pub struct DropdownMenuPopover<T: Selectable + IntoElement + 'static> {
    id: ElementId,
    anchor: Corner,
    trigger: T,
    builder: std::rc::Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>,
}

impl<T> DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    fn new(
        id: ElementId,
        anchor: impl Into<Corner>,
        trigger: T,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        Self {
            id: SharedString::from(format!("dropdown-menu:{:?}", id)).into(),
            anchor: anchor.into(),
            trigger,
            builder: std::rc::Rc::new(builder),
        }
    }

    /// Set the anchor corner for the dropdown menu popover.
    pub fn anchor(mut self, anchor: impl Into<Corner>) -> Self {
        self.anchor = anchor.into();
        self
    }
}

impl<T> RenderOnce for DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        use gpui::{ParentElement, div};

        // Simplified implementation - just render the trigger
        // In a full implementation, this would use a Popover component
        div().child(self.trigger)
    }
}
