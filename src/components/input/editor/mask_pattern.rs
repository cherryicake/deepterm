use gpui::SharedString;

#[derive(Clone, Default)]
pub struct MaskPattern;

impl MaskPattern {
    pub fn is_none(&self) -> bool {
        true
    }
    pub fn placeholder(&self) -> SharedString {
        SharedString::default()
    }
    pub fn mask(&self, text: &str) -> SharedString {
        SharedString::from(text.to_string())
    }
    pub fn unmask(&self, text: &str) -> SharedString {
        SharedString::from(text.to_string())
    }
    pub fn apply(&self, text: &str) -> SharedString {
        SharedString::from(text.to_string())
    }
    pub fn is_valid(&self, _text: &str) -> bool {
        true
    }
}
