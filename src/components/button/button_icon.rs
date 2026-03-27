use crate::components::spinner::Spinner;
use crate::theme::ActiveTheme;
use gpui::{App, Hsla, IntoElement, RenderOnce, Styled, Window};

/// Button icon which can be an Icon or Spinner use for `icon` method of Button.
#[derive(IntoElement)]
pub struct ButtonIcon {
    icon: Icon,
    loading: bool,
    size: Size,
}

impl From<Icon> for ButtonIcon {
    fn from(icon: Icon) -> Self {
        ButtonIcon::new(icon)
    }
}

impl ButtonIcon {
    /// Creates a new ButtonIcon with the given icon.
    pub fn new(icon: Icon) -> Self {
        Self {
            icon,
            loading: false,
            size: Size::Medium,
        }
    }

    pub(crate) fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }
}

impl Sizable for ButtonIcon {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl RenderOnce for ButtonIcon {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        if self.loading {
            Spinner::new()
                .color(cx.theme().ui.accent)
                .into_any_element()
        } else {
            self.icon.with_size(self.size).into_any_element()
        }
    }
}

/// Icon component for Button
#[derive(IntoElement, Clone)]
pub struct Icon {
    path: &'static str,
    size: Size,
    color: Option<Hsla>,
}

impl Icon {
    /// Create a new icon with the given SVG path
    pub fn new(path: &'static str) -> Self {
        Self {
            path,
            size: Size::Medium,
            color: None,
        }
    }

    /// Set the icon size
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// Set the icon color
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// Get the icon path
    pub fn path(&self) -> &'static str {
        self.path
    }
}

impl Sizable for Icon {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        use gpui::{px, svg};

        let size = match self.size {
            Size::XSmall => px(12.0),
            Size::Small => px(14.0),
            Size::Medium => px(16.0),
            Size::Large => px(20.0),
            Size::Size(pixels) => pixels,
        };

        let color = self.color.unwrap_or_else(|| cx.theme().ui.icon);

        svg().path(self.path).size(size).text_color(color)
    }
}

/// Size variants for components
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Size {
    XSmall,
    Small,
    #[default]
    Medium,
    Large,
    Size(gpui::Pixels),
}

impl From<gpui::Pixels> for Size {
    fn from(size: gpui::Pixels) -> Self {
        Size::Size(size)
    }
}

/// Trait for setting the size of an element
#[allow(patterns_in_fns_without_body)]
pub trait Sizable: Sized {
    /// Set the size of this element
    fn with_size(self, size: impl Into<Size>) -> Self;

    /// Set to Size::XSmall
    #[inline(always)]
    fn xsmall(self) -> Self {
        self.with_size(Size::XSmall)
    }

    /// Set to Size::Small
    #[inline(always)]
    fn small(self) -> Self {
        self.with_size(Size::Small)
    }

    /// Set to Size::Large
    #[inline(always)]
    fn large(self) -> Self {
        self.with_size(Size::Large)
    }
}

/// Trait for defining element that can be disabled
#[allow(patterns_in_fns_without_body)]
pub trait Disableable {
    /// Set the disabled state of the element
    fn disabled(self, disabled: bool) -> Self;
}

/// Trait for defining element that can be selected
#[allow(patterns_in_fns_without_body)]
pub trait Selectable: Sized {
    /// Set the selected state of the element
    fn selected(self, selected: bool) -> Self;

    /// Returns true if the element is selected
    fn is_selected(&self) -> bool;
}
