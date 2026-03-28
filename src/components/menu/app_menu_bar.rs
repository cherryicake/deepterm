use crate::components::button::{Button, Selectable};
use crate::components::menu::PopupMenu;
use crate::theme::ActiveTheme;
use gpui::{
    actions, anchored, deferred, div, prelude::FluentBuilder, px, App, AppContext as _, ClickEvent,
    Context, DismissEvent, Entity, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
    MouseButton, OwnedMenu, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window,
};

const CONTEXT: &str = "AppMenuBar";

actions!(app_menu_bar, [Cancel, SelectLeft, SelectRight]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
        KeyBinding::new("left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("right", SelectRight, Some(CONTEXT)),
    ]);
}

/// The application menu bar, for Windows and Linux.
pub struct AppMenuBar {
    menus: Vec<Entity<AppMenu>>,
    selected_index: Option<usize>,
}

impl AppMenuBar {
    /// Create a new app menu bar.
    pub fn new(_cx: &mut App) -> Entity<Self> {
        // Note: This is a simplified implementation
        // The original uses GlobalState::global(cx).app_menus() to get menus
        // In a full implementation, you would pass menus as a parameter

        // For now, create an empty menu bar
        // Users can extend this to pass their own menus
        unimplemented!(
            "AppMenuBar requires GlobalState integration. Use PopupMenu directly for now."
        )
    }

    fn on_move_left(&mut self, _: &SelectLeft, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_index) = self.selected_index else {
            return;
        };

        let new_ix = if selected_index == 0 {
            self.menus.len().saturating_sub(1)
        } else {
            selected_index.saturating_sub(1)
        };
        self.set_selected_index(Some(new_ix), cx);
    }

    fn on_move_right(&mut self, _: &SelectRight, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_index) = self.selected_index else {
            return;
        };

        let new_ix = if selected_index + 1 >= self.menus.len() {
            0
        } else {
            selected_index + 1
        };
        self.set_selected_index(Some(new_ix), cx);
    }

    fn on_cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_selected_index(None, cx);
    }

    fn set_selected_index(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        self.selected_index = ix;
        cx.notify();
    }

    #[inline]
    fn has_activated_menu(&self) -> bool {
        self.selected_index.is_some()
    }
}

impl Render for AppMenuBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        div()
            .id("app-menu-bar")
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_cancel))
            .size_full()
            .gap_x_1()
            .overflow_x_scroll()
            .children(self.menus.clone())
    }
}

/// A menu in the menu bar.
pub(super) struct AppMenu {
    menu_bar: Entity<AppMenuBar>,
    ix: usize,
    name: SharedString,
    popup_menu: Option<Entity<PopupMenu>>,

    _subscription: Option<Subscription>,
}

impl AppMenu {
    pub(super) fn new(
        ix: usize,
        name: SharedString,
        menu_bar: Entity<AppMenuBar>,
        _cx: &mut App,
    ) -> Entity<Self> {
        _cx.new(|_| Self {
            ix,
            menu_bar,
            name,
            popup_menu: None,
            _subscription: None,
        })
    }

    fn handle_trigger_click(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_selected = self.menu_bar.read(cx).selected_index == Some(self.ix);

        _ = self.menu_bar.update(cx, |state, cx| {
            let new_ix = if is_selected { None } else { Some(self.ix) };
            state.set_selected_index(new_ix, cx);
        });
    }

    fn handle_hover(&mut self, hovered: &bool, window: &mut Window, cx: &mut Context<Self>) {
        if !*hovered {
            return;
        }

        let has_activated_menu = self.menu_bar.read(cx).has_activated_menu();
        if !has_activated_menu {
            return;
        }

        _ = self.menu_bar.update(cx, |state, cx| {
            state.set_selected_index(Some(self.ix), cx);
        });
    }
}

impl Render for AppMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_bar = self.menu_bar.read(cx);
        let is_selected = menu_bar.selected_index == Some(self.ix);
        let theme = cx.theme().ui;

        div()
            .id(self.ix)
            .relative()
            .child(
                Button::new("menu")
                    .small()
                    .py_0p5()
                    .compact()
                    .ghost()
                    .label(self.name.clone())
                    .selected(is_selected)
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        // Stop propagation to avoid dragging the window.
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(Self::handle_trigger_click)),
            )
            .on_hover(cx.listener(Self::handle_hover))
            .when(is_selected, |this| {
                // Note: Simplified - would need to show popup menu here
                this
            })
    }
}
