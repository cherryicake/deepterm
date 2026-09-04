//! GPUI 回调类型别名。
//!
//! 形如 `Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>` 的回调类型嵌套
//! 很深，clippy 会报 `type_complexity`。这里把反复出现的形状收敛成具名别名：
//! 既消除告警，也让结构体字段的含义一眼可读。
//!
//! 命名约定：
//! - `Optional*` = `Option<...>` 版本
//! - `Boxed*` = `Box<dyn ...>` 版本（独占所有权，不可克隆）
//! - 其余默认 `Rc<dyn ...>` 版本（可克隆共享）
//! - `*AppCallback` = 只依赖 `&mut App`，带 `Send + Sync`，可跨线程

use std::rc::Rc;

use gpui::{App, ClickEvent, Window};

/// 点击回调。
pub type ClickCallback = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// 可选点击回调。
pub type OptionalClickCallback = Option<ClickCallback>;

/// 点击回调（`Box` 版，用于只存续一次的所有权场景）。
pub type BoxedClickCallback = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
/// 可选点击回调（`Box` 版）。
pub type OptionalBoxedClickCallback = Option<BoxedClickCallback>;

/// 悬停回调，参数为是否进入悬停。
pub type HoverCallback = Rc<dyn Fn(&bool, &mut Window, &mut App)>;
/// 可选悬停回调。
pub type OptionalHoverCallback = Option<HoverCallback>;

/// 悬停回调（`Box` 版）。
pub type BoxedHoverCallback = Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>;
/// 可选悬停回调（`Box` 版）。
pub type OptionalBoxedHoverCallback = Option<BoxedHoverCallback>;

/// 只依赖 `&mut Window` + `&mut App` 的无参回调。
pub type WindowCallback = Rc<dyn Fn(&mut Window, &mut App)>;
/// 可选 [`WindowCallback`]。
pub type OptionalWindowCallback = Option<WindowCallback>;

/// 以 `&str`（通常是 id 或 key）为参数的回调。
pub type StrCallback = Rc<dyn Fn(&str, &mut Window, &mut App)>;
/// 可选 [`StrCallback`]。
pub type OptionalStrCallback = Option<StrCallback>;

/// 只依赖 `&mut App` 的回调（可跨线程）。
pub type AppCallback = Box<dyn Fn(&mut App) + Send + Sync>;
/// 可选 [`AppCallback`]。
pub type OptionalAppCallback = Option<AppCallback>;

/// 以 `&str` 为参数的回调（可跨线程）。
pub type StrAppCallback = Box<dyn Fn(&str, &mut App) + Send + Sync>;
/// 可选 [`StrAppCallback`]。
pub type OptionalStrAppCallback = Option<StrAppCallback>;
