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

/// 为 GPUI `Element` 实现补齐无定位信息的 `source_location`（恒 `None`）。
///
/// GPUI 的 `Element` trait 未给 `source_location` 提供默认实现，全仓 10 处
/// 自定义元素都被迫写同一个三行 `None` 方法体；此宏将其收敛为一行。
#[macro_export]
macro_rules! impl_source_location_none {
    () => {
        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
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
