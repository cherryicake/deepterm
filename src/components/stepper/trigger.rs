use gpui::{
    AnyElement, App, Axis, ClickEvent, InteractiveElement as _, IntoElement, ParentElement,
    RenderOnce, StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _,
};

use crate::components::spinner::Spinner;
use crate::components::stepper::StepperSize;
use crate::components::stepper::stepper::StepperStatus;
use crate::theme::ActiveTheme;

/// The trigger part of a stepper item.
#[derive(IntoElement)]
pub(crate) struct StepperTrigger {
    step: usize,
    checked_step: usize,
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

impl ParentElement for StepperTrigger {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for StepperTrigger {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_checked = self.step <= self.checked_step;
        let is_current = self.step == self.checked_step;
        let is_completed = self.step < self.checked_step;
        let disabled = self.disabled;
        let on_click = self.on_click;
        let layout = self.layout;
        let text_center = self.text_center;
        let size = self.size;
        let indicator_size = size.indicator_size();
        let is_failed = self.status == StepperStatus::Failed;
        let show_spinner = self.show_spinner && is_current && !disabled && !is_failed;

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
                                        .color(theme.accent)
                                        .path("icons/loader_fade.svg"),
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
                            .bg(theme.border)
                            .when(!disabled && !is_checked, |this| {
                                this.hover(|this| this.bg(theme.border_strong))
                                    .active(|this| this.bg(theme.app_background))
                            })
                            .text_color(theme.text_muted)
                            .when(is_completed, |this| {
                                this.bg(theme.success).text_color(theme.success_foreground)
                            })
                            .when(is_current && self.status == StepperStatus::Failed, |this| {
                                this.bg(theme.error).text_color(theme.error_foreground)
                            })
                            .when(is_current && self.status != StepperStatus::Failed, |this| {
                                this.bg(theme.accent).text_color(theme.accent_foreground)
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
