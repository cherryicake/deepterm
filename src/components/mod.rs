use gpui::App;
pub mod animation;
pub mod callbacks;
pub mod button;
pub mod input;
pub mod macos_select;
pub mod macos_sidebar;
pub mod menu;
pub mod modal;
pub mod notification;
pub mod spinner;
pub mod stepper;
pub mod title_bar;

/// 为持有 `focus_handle: FocusHandle` 字段的实体实现 Focusable。
///
/// 全仓 21 处 `impl Focusable for T { fn focus_handle(&self, _, _) { self.focus_handle.clone() } }`
/// 样板的统一收敛；形如 `search_input.read(cx).focus_handle` 的非标准实现仍手写。
#[macro_export]
macro_rules! impl_focusable {
    ($ty:ty) => {
        impl gpui::Focusable for $ty {
            fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
                self.focus_handle.clone()
            }
        }
    };
}

/// Initialize the components.
///
/// You must initialize the components at your application's entry point.
pub fn init(cx: &mut App) {
    input::init(cx);
    menu::init(cx);
    modal::init(cx);
}
