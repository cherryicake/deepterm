mod item;
mod stepper;
mod trigger;

pub use item::*;
pub use stepper::*;

use gpui::Pixels;

/// Stepper size variants for step indicators
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StepperSize {
    /// 16px radius (32px diameter)
    Small,
    /// 24px radius (48px diameter) - default
    #[default]
    Medium,
    /// 32px radius (64px diameter)
    Large,
    /// 48px radius (96px diameter)
    XLarge,
}

impl StepperSize {
    /// Get the icon size for this stepper size
    pub fn icon_size(&self) -> Pixels {
        match self {
            StepperSize::Small => gpui::px(16.0),
            StepperSize::Medium => gpui::px(24.0),
            StepperSize::Large => gpui::px(32.0),
            StepperSize::XLarge => gpui::px(48.0),
        }
    }

    /// Get the indicator wrapper size (spinner size + padding to create gap)
    pub fn indicator_size(&self) -> Pixels {
        match self {
            StepperSize::Small => gpui::px(20.0),
            StepperSize::Medium => gpui::px(28.0),
            StepperSize::Large => gpui::px(36.0),
            StepperSize::XLarge => gpui::px(52.0),
        }
    }

    /// Get the corresponding spinner size (always larger than indicator to wrap around it)
    pub fn spinner_size(&self) -> crate::components::spinner::SpinnerSize {
        match self {
            StepperSize::Small => crate::components::spinner::SpinnerSize::Medium,
            StepperSize::Medium => crate::components::spinner::SpinnerSize::Large,
            StepperSize::Large => crate::components::spinner::SpinnerSize::XLarge,
            StepperSize::XLarge => crate::components::spinner::SpinnerSize::XLarge,
        }
    }
}
