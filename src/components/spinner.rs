use std::time::Duration;

use crate::assets::Icon;
use gpui::{
    Animation, AnimationExt as _, App, Hsla, IntoElement, ParentElement, RenderOnce, Styled as _,
    Transformation, Window, div, ease_in_out, percentage, prelude::FluentBuilder as _,
};

/// A cycling loading spinner.
#[derive(IntoElement)]
pub struct Spinner {
    size: SpinnerSize,
    speed: Duration,
    color: Option<Hsla>,
}

/// Spinner size variants
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpinnerSize {
    #[default]
    Small, // 16px
    Medium, // 24px
    Large,  // 32px
    XLarge, // 48px
}

impl SpinnerSize {
    pub fn px_size(&self) -> gpui::Pixels {
        match self {
            SpinnerSize::Small => gpui::px(16.0),
            SpinnerSize::Medium => gpui::px(24.0),
            SpinnerSize::Large => gpui::px(32.0),
            SpinnerSize::XLarge => gpui::px(48.0),
        }
    }
}

impl Spinner {
    /// Create a new loading spinner.
    pub fn new() -> Self {
        Self {
            size: SpinnerSize::Medium,
            speed: Duration::from_millis(800),
            color: None,
        }
    }

    /// Set the spinner size.
    pub fn size(mut self, size: SpinnerSize) -> Self {
        self.size = size;
        self
    }

    /// Set the spinner color.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// Set the animation speed.
    pub fn speed(mut self, speed: Duration) -> Self {
        self.speed = speed;
        self
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderOnce for Spinner {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let size = self.size.px_size();

        // Create an SVG element for the loader icon
        let icon_path = Icon::Loader.path();
        let svg_element = gpui::svg()
            .path(icon_path)
            .size(size)
            .when_some(self.color, |this, color| this.text_color(color));

        div()
            .child(svg_element.with_animation(
                "spinner-rotate",
                Animation::new(self.speed).repeat().with_easing(ease_in_out),
                |this, delta| this.with_transformation(Transformation::rotate(percentage(delta))),
            ))
            .into_element()
    }
}

/// Convenience function to create a new spinner
pub fn spinner() -> Spinner {
    Spinner::new()
}
