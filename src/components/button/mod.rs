// `button/button.rs` 是本仓库既定的模块布局（外层 `mod.rs` 汇总、实现放同名子文件）。
// 改名会波及全仓 `use crate::components::button::button::...` 路径，收益不足，因此显式允许该 lint。
#![allow(clippy::module_inception)]

pub mod button;
pub mod button_icon;

pub use button::{Button, ButtonCustomVariant, ButtonRounded, ButtonVariant, ButtonVariants};
pub use button_icon::{ButtonIcon, Disableable, Icon, Selectable, Sizable, Size};
