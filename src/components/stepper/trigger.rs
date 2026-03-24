use gpui::{
    div, prelude::FluentBuilder as _, AnyElement, App, Axis, ClickEvent, InteractiveElement as _,
    IntoElement, ParentElement, RenderOnce, StatefulInteractiveElement as _, StyleRefinement,
    Styled, Window,
};

use crate::components::spinner::Spinner;
use crate::components::stepper::stepper::StepperStatus;
use crate::components::stepper::StepperSize;

/// The trigger part of a stepper item.
#[derive(IntoElement)]
pub(crate) struct StepperTrigger {
    step: usize,
    checked_step: usize,
    style: StyleRefinement,
    size: StepperSize,
    children: Vec<AnyElement>,
    layout: Axis,
    disabled: bool,
    text_center: bool,
    show_spinner: bool,
    status: StepperStatus,
    on_click: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
}

impl StepperTrigger {
    pub(crate) fn new() -> Self {
        Self {
            step: 0,
            checked_step: 0,
            size: StepperSize::default(),
            layout: Axis::Horizontal,
            disabled: false,
            children: Vec::new(),
            text_center: false,
            style: StyleRefinement::default(),
            show_spinner: false,
            status: StepperStatus::default(),
            on_click: Box::new(|_, _, _| {}),
        }
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

    pub(crate) fn size(mut self, size: StepperSize) -> Self {
        self.size = size;
        self
    }

    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub(crate) fn text_center(mut self, center: bool) -> Self {
        self.text_center = center;
        self
    }

    pub(crate) fn show_spinner(mut self, show: bool) -> Self {
        self.show_spinner = show;
        self
    }

    pub(crate) fn status(mut self, status: StepperStatus) -> Self {
        self.status = status;
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

impl Styled for StepperTrigger {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for StepperTrigger {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for StepperTrigger {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let is_checked = self.step <= self.checked_step;
        let is_current = self.step == self.checked_step;
        let is_completed = self.step < self.checked_step;
        let disabled = self.disabled;
        let on_click = self.on_click;
        let layout = self.layout;
        let text_center = self.text_center;
        let size = self.size;
        let indicator_size = size.indicator_size();
        let show_spinner = self.show_spinner && is_current && !disabled;

        div()
            .id(("trigger", self.step))
            .when(layout == Axis::Horizontal, |this| {
                this.flex().flex_col().gap_1()
            })
            .when(layout == Axis::Vertical, |this| {
                this.flex().flex_row().gap_2()
            })
            .items_start()
            .when(text_center, |this| this.items_center())
            .text_sm()
            .child(
                div()
                    .id("indicator-wrapper")
                    .size(indicator_size)
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(show_spinner, |this| {
                        this.child(
                            div()
                                .absolute()
                                .size(indicator_size)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    Spinner::new()
                                        .size(size.spinner_size())
                                        .color(gpui::rgb(0x93c5fd).into()),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("indicator")
                            .size(size.icon_size())
                            .overflow_hidden()
                            .flex()
                            .rounded_full()
                            .items_center()
                            .justify_center()
                            .bg(gpui::rgb(0x3f3f46))
                            .when(!disabled && !is_checked, |this| {
                                this.hover(|this| this.bg(gpui::rgb(0x52525b)))
                                    .active(|this| this.bg(gpui::rgb(0x27272a)))
                            })
                            .text_color(gpui::rgb(0xa1a1aa))
                            .when(is_completed, |this| {
                                this.bg(gpui::rgb(0x22c55e)).text_color(gpui::rgb(0xffffff))
                            })
                            .when(is_current && self.status == StepperStatus::Failed, |this| {
                                this.bg(gpui::rgb(0xf87171)).text_color(gpui::rgb(0xffffff))
                            })
                            .when(is_current && self.status != StepperStatus::Failed, |this| {
                                this.bg(gpui::rgb(0x3b82f6)).text_color(gpui::rgb(0xffffff))
                            })
                            .child(div().child(format!("{}", self.step + 1))),
                    ),
            )
            .children(self.children)
            .when(!disabled, |this| {
                this.on_click(move |event, window, cx| {
                    (on_click)(event, window, cx);
                })
            })
    }
}
