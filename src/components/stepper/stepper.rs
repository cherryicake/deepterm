use std::rc::Rc;

use gpui::{
    App, Axis, ElementId, InteractiveElement as _, IntoElement, ParentElement, RenderOnce,
    StyleRefinement, Styled, Window, div, prelude::FluentBuilder as _,
};

use crate::components::stepper::{StepperSize, item::StepperItem};

/// Stepper 整体状态
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StepperStatus {
    /// 正常进行中（默认）
    #[default]
    Normal,
    /// 全部成功完成
    Success,
    /// 失败
    Failed,
}

/// A step-by-step progress for users to navigate through a series of steps or stages.
#[derive(IntoElement)]
pub struct Stepper {
    id: ElementId,
    style: StyleRefinement,
    items: Vec<StepperItem>,
    step: usize,
    layout: Axis,
    disabled: bool,
    text_center: bool,
    status: StepperStatus,
    size: StepperSize,
    on_click: Rc<dyn Fn(&usize, &mut Window, &mut App) + 'static>,
}

impl Stepper {
    /// Creates a new stepper with the given ID.
    ///
    /// Default use is horizontal layout with step 0 selected.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            items: Vec::new(),
            step: 0,
            layout: Axis::Horizontal,
            disabled: false,
            text_center: false,
            status: StepperStatus::default(),
            size: StepperSize::default(),
            on_click: Rc::new(|_, _, _| {}),
        }
    }

    /// Set the size of the stepper indicators.
    pub fn size(mut self, size: StepperSize) -> Self {
        self.size = size;
        self
    }

    /// Set whether to center the text within each stepper item.
    pub fn text_center(mut self, center: bool) -> Self {
        self.text_center = center;
        self
    }

    /// Set the layout of the stepper, default is horizontal.
    pub fn layout(mut self, layout: Axis) -> Self {
        self.layout = layout;
        self
    }

    /// Sets the layout of the stepper to Vertical.
    pub fn vertical(mut self) -> Self {
        self.layout = Axis::Vertical;
        self
    }

    /// Sets the selected index of the stepper, default is 0.
    pub fn selected_index(mut self, index: usize) -> Self {
        self.step = index;
        self
    }

    /// Sets the status of the stepper (Normal, Success, Failed).
    pub fn status(mut self, status: StepperStatus) -> Self {
        self.status = status;
        self
    }

    /// Adds a stepper item to the stepper.
    pub fn item(mut self, item: StepperItem) -> Self {
        self.items.push(item);
        self
    }

    /// Add multiple stepper items to the stepper.
    pub fn items(mut self, items: impl IntoIterator<Item = StepperItem>) -> Self {
        self.items.extend(items);
        self
    }

    /// Set the disabled state of the stepper, default is false.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Add an on_click handler for when a step is clicked.
    ///
    /// The first parameter is the `step` of currently clicked item.
    pub fn on_click<F>(mut self, f: F) -> Self
    where
        F: Fn(&usize, &mut Window, &mut App) + 'static,
    {
        self.on_click = Rc::new(f);
        self
    }
}

impl Styled for Stepper {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Stepper {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let total_items = self.items.len();
        let layout = self.layout;
        let disabled = self.disabled;
        let text_center = self.text_center;
        let checked_step = self.step;
        let status = self.status;
        let size = self.size;
        let on_click = self.on_click.clone();

        // 调用方通过 `impl Styled` 设置的 refinement 应用到根元素，
        // 在此基础上再叠加组件自身的布局默认值。
        let mut root = div().id(self.id);
        *root.style() = self.style;
        root.w_full()
            .when(layout == Axis::Horizontal, |this| this.flex().flex_row())
            .when(layout == Axis::Vertical, |this| this.flex().flex_col())
            .children(self.items.into_iter().enumerate().map(move |(step, item)| {
                let is_last = step + 1 == total_items;
                let on_click = on_click.clone();
                item.step(step)
                    .checked_step(checked_step)
                    .layout(layout)
                    .text_center(text_center)
                    .status(status)
                    .size(size)
                    .when(disabled, |this| this.disabled(true))
                    .is_last(is_last)
                    .on_click(move |_, window, cx| {
                        on_click(&step, window, cx);
                    })
            }))
    }
}
