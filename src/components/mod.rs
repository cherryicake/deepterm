use gpui::App;
pub mod animation;
pub mod button;
pub mod drag_drop;
pub mod input;
pub mod macos_select;
pub mod macos_sidebar;
pub mod menu;
pub mod modal;
pub mod resize_handle;
pub mod notification;
pub mod spinner;
pub mod split_view;
pub mod stepper;
pub mod tab_bar;
pub mod title_bar;

/// Initialize the components.
///
/// You must initialize the components at your application's entry point.
pub fn init(cx: &mut App) {
    input::init(cx);
    menu::init(cx);
    modal::init(cx);
}
