use gpui::SharedString;

#[derive(Clone)]
pub struct MaskPattern;

impl MaskPattern {
    pub fn apply(&self, text: &str) -> SharedString {
        text.into()
    }
}
