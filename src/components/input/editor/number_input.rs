use gpui::{App, SharedString};

pub fn format_number(value: f64, _decimals: Option<usize>) -> SharedString {
    value.to_string().into()
}

pub fn init(_cx: &mut App) {}
