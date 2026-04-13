use gpui::SharedString;

pub fn format_number(value: f64, _decimals: Option<usize>) -> SharedString {
    value.to_string().into()
}
