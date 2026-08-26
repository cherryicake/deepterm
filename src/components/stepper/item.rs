use gpui::{
    AnyElement, App, Axis, ClickEvent, InteractiveElement as _, IntoElement, ParentElement,
    RenderOnce, StyleRefinement, Styled, Window, div, prelude::FluentBuilder as _, px, relative,
};

use crate::components::stepper::StepperSize;
use crate::components::stepper::stepper::StepperStatus;
use crate::components::stepper::trigger::StepperTrigger;
use crate::theme::ActiveTheme;

/// A step item within a [`Stepper`].
#[derive(IntoElement)]
pub struct StepperItem {
    step: usize,
    checked_step: usize,
    style: StyleRefinement,
    children: Vec<AnyElement>,
    layout: Axis,
    disabled: bool,
    is_last: bool,
    text_center: bool,
    status: StepperStatus,
    size: StepperSize,
    on_click: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
}

impl StepperItem {
    pub fn new() -> Self {
        Self {
            step: 0,
            checked_step: 0,
            style: StyleRefinement::default(),
            layout: Axis::Horizontal,
            disabled: false,
            is_last: false,
            text_center: false,
            status: StepperStatus::default(),
            size: StepperSize::default(),
            children: Vec::new(),
            on_click: Box::new(|_, _, _| {}),
        }
    }

    /// Set disabled state of the stepper item.
    ///
    /// Will override the stepper's disabled state if set to true.
    ///
    /// Default is false.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub(crate) fn text_center(mut self, center: bool) -> Self {
        self.text_center = center;
        self
    }

    pub(crate) fn step(mut self, ix: usize) -> Self {
        self.step = ix;
        self
    }

    pub(crate) fn checked_step(mut self, checked_step: usize) -> Self {
        self.checked_step = checked_step;
        self
    }

    pub(crate) fn layout(mut self, layout: Axis) -> Self {
        self.layout = layout;
        self
    }

    pub(crate) fn is_last(mut self, is_last: bool) -> Self {
        self.is_last = is_last;
        self
    }

    pub(crate) fn status(mut self, status: StepperStatus) -> Self {
        self.status = status;
        self
    }

    pub(crate) fn size(mut self, size: StepperSize) -> Self {
        self.size = size;
        self
    }

    pub(crate) fn on_click<F>(mut self, f: F) -> Self
    where
        F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    {
        self.on_click = Box::new(f);
        self
    }
}

impl ParentElement for StepperItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for StepperItem {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for StepperItem {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let is_passed = self.step < self.checked_step;
        let icon_size = self.size.icon_size();

        // 调用方 refinement 应用到根元素（仅 StepperItem 公开使用，效果同 Stepper）。
        let mut root = div().id(("stepper-item", self.step));
        *root.style() = self.style;
        root.relative()
            .when(self.layout == Axis::Horizontal, |this| {
                this.flex().flex_row().items_start()
            })
            .when(self.layout == Axis::Vertical, |this| {
                this.flex().flex_col().items_start()
            })
            .when(!self.is_last, |this| this.flex_1())
            .when(self.text_center, |this| this.flex_1().justify_center())
            .child(
                StepperTrigger::new()
                    .size(self.size)
                    .step(self.step)
                    .checked_step(self.checked_step)
                    .text_center(self.text_center)
                    .layout(self.layout)
                    .disabled(self.disabled)
                    .show_spinner(self.step == self.checked_step)
                    .status(self.status)
                    .children(self.children)
                    .on_click({
                        let on_click = self.on_click;
                        move |e, window, cx| {
                            on_click(e, window, cx);
                        }
                    }),
            )
            .when(!self.is_last, |this| {
                this.child(
                    StepperSeparator::new()
                        .size(self.size)
                        .layout(self.layout)
                        .text_center(self.text_center)
                        .icon_size(icon_size)
                        .checked(is_passed),
                )
            })
    }
}

/// A separator between stepper items.
///
/// Default is `absolute` positioned.
#[derive(IntoElement)]
struct StepperSeparator {
    size: StepperSize,
    checked: bool,
    icon_size: gpui::Pixels,
    layout: Axis,
    text_center: bool,
}

impl StepperSeparator {
    fn new() -> Self {
        Self {
            size: StepperSize::default(),
            checked: false,
            icon_size: px(24.),
            layout: Axis::Horizontal,
            text_center: false,
        }
    }

    fn size(mut self, size: StepperSize) -> Self {
        self.size = size;
        self
    }

    fn text_center(mut self, center: bool) -> Self {
        self.text_center = center;
        self
    }

    fn layout(mut self, layout: Axis) -> Self {
        self.layout = layout;
        self
    }

    fn icon_size(mut self, size: gpui::Pixels) -> Self {
        self.icon_size = size;
        self
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

impl RenderOnce for StepperSeparator {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().ui;
        let icon_size = self.icon_size;
        let text_center = self.text_center;
        let separator_wide = px(2.);
        let gap = px(4.);

        div()
            .absolute()
            .flex_1()
            .when(self.layout == Axis::Horizontal, |this| {
                this.h(separator_wide).mt(icon_size / 2.).map(|this| {
                    if !text_center {
                        this.ml(icon_size + gap).mr(gap).left_0().right_0()
                    } else {
                        this.mx(icon_size / 2. + gap)
                            .left(relative(0.5))
                            .right(relative(-0.5))
                    }
                })
            })
            .when(self.layout == Axis::Vertical, |this| {
                this.w(separator_wide).ml(icon_size / 2.).map(|this| {
                    if !text_center {
                        this.mt(icon_size + gap).mb(gap).top_0().bottom_0()
                    } else {
                        this.my(icon_size / 2. + gap)
                            .top(relative(0.5))
                            .bottom(relative(-0.5))
                    }
                })
            })
            .bg(theme.border_strong)
            .when(self.checked, |this| this.bg(theme.success))
    }
}
