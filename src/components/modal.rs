use crate::theme::ActiveTheme;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, KeyBinding, ParentElement, Pixels,
    RenderOnce, StatefulInteractiveElement, StyleRefinement, Styled, Window, actions, deferred,
    div, px, point, BoxShadow,
};
use std::rc::Rc;

const CONTEXT: &str = "Modal";

actions!(modal, [Cancel]);

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Cancel, Some(CONTEXT))]);
}

/// A modal dialog component that renders a centered panel over a backdrop.
///
/// The modal provides a customizable container with a backdrop overlay.
/// It contains no default elements — all content is provided by the caller.
///
/// # Example
///
/// ```rust
/// // In your Render implementation:
/// .when(self.show_settings, |this| {
///     this.child(
///         Modal::new("settings-modal")
///             .width(px(480.))
///             .max_height(px(600.))
///             .on_close(cx.listener(|this, _window, cx| {
///                 this.show_settings = false;
///                 cx.notify();
///             }))
///             .child(my_custom_content),
///     )
/// })
/// ```
#[derive(IntoElement)]
pub struct Modal {
    id: ElementId,
    style: StyleRefinement,
    width: Option<Pixels>,
    height: Option<Pixels>,
    max_width: Option<Pixels>,
    max_height: Option<Pixels>,
    min_width: Option<Pixels>,
    min_height: Option<Pixels>,
    padding: Option<Pixels>,
    rounded: Option<Pixels>,
    top_padding: Option<Pixels>,
    children: Vec<AnyElement>,
    close_on_backdrop: bool,
    close_on_escape: bool,
    on_close: Option<Rc<dyn Fn(&mut Window, &mut App) + 'static>>,
}

impl Modal {
    /// Create a new modal with the given element id.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            width: None,
            height: None,
            max_width: None,
            max_height: None,
            min_width: None,
            min_height: None,
            padding: Some(px(24.0)),
            rounded: Some(px(16.0)),
            top_padding: None,
            children: Vec::new(),
            close_on_backdrop: true,
            close_on_escape: true,
            on_close: None,
        }
    }

    /// Set the width of the modal panel.
    pub fn width(mut self, width: impl Into<Pixels>) -> Self {
        self.width = Some(width.into());
        self
    }

    /// Set the height of the modal panel.
    pub fn height(mut self, height: impl Into<Pixels>) -> Self {
        self.height = Some(height.into());
        self
    }

    /// Set the max width of the modal panel.
    pub fn max_width(mut self, max_width: impl Into<Pixels>) -> Self {
        self.max_width = Some(max_width.into());
        self
    }

    /// Set the max height of the modal panel.
    pub fn max_height(mut self, max_height: impl Into<Pixels>) -> Self {
        self.max_height = Some(max_height.into());
        self
    }

    /// Set the min width of the modal panel.
    pub fn min_width(mut self, min_width: impl Into<Pixels>) -> Self {
        self.min_width = Some(min_width.into());
        self
    }

    /// Set the min height of the modal panel.
    pub fn min_height(mut self, min_height: impl Into<Pixels>) -> Self {
        self.min_height = Some(min_height.into());
        self
    }

    /// Set the padding of the modal panel. Default is `px(24.0)`.
    pub fn padding(mut self, padding: impl Into<Pixels>) -> Self {
        self.padding = Some(padding.into());
        self
    }

    /// Set the border radius of the modal panel. Default is `px(16.0)`.
    pub fn rounded(mut self, rounded: impl Into<Pixels>) -> Self {
        self.rounded = Some(rounded.into());
        self
    }

    /// Set the top padding of the modal panel to position it lower from the top.
    /// When set, the modal will be aligned to the top with this padding instead of centered.
    pub fn top_padding(mut self, top: impl Into<Pixels>) -> Self {
        self.top_padding = Some(top.into());
        self
    }

    /// Set whether clicking the backdrop closes the modal. Default is `true`.
    pub fn close_on_backdrop(mut self, close: bool) -> Self {
        self.close_on_backdrop = close;
        self
    }

    /// Set whether pressing Escape closes the modal. Default is `true`.
    pub fn close_on_escape(mut self, close: bool) -> Self {
        self.close_on_escape = close;
        self
    }

    /// Set a callback that is invoked when the modal is closed
    /// (either by backdrop click, Escape, or programmatically).
    pub fn on_close(
        mut self,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_close = Some(Rc::new(handler));
        self
    }
}

impl ParentElement for Modal {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for Modal {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Modal {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().ui;
        let on_close = self.on_close.clone();
        let close_on_backdrop = self.close_on_backdrop;
        let close_on_escape = self.close_on_escape;

        let has_max_h = self.max_height.is_some();
        let panel = div()
            .id("modal-panel")
            .flex()
            .flex_col()
            .items_center()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .text_color(theme.text)
            .when_some(self.rounded, |this, r| this.rounded(r))
            .when_some(self.width, |this, w| this.w(w))
            .when_some(self.height, |this, h| this.h(h))
            .when_some(self.max_width, |this, w| this.max_w(w))
            .when_some(self.max_height, |this, h| this.max_h(h))
            .when_some(self.min_width, |this, w| this.min_w(w))
            .when_some(self.min_height, |this, h| this.min_h(h))
            .when_some(self.padding, |this, p| this.p(p))
            .when(has_max_h, |this| this.overflow_y_scroll())
            .shadow(vec![BoxShadow {
                color: theme.shadow.opacity(if cx.theme().appearance.is_dark() {
                    0.48
                } else {
                    0.24
                }),
                offset: point(px(0.), px(8.)),
                blur_radius: px(32.),
                spread_radius: px(-8.),
            }])
            .occlude()
            .children(self.children);

        let has_top_padding = self.top_padding.is_some();
        deferred(
            div()
                .id(self.id)
                .absolute()
                .inset_0()
                .size_full()
                .flex()
                .justify_center()
                .when(has_top_padding, |this| {
                    this.items_start().when_some(self.top_padding, |this, t| this.pt(t))
                })
                .when(!has_top_padding, |this| this.items_center())
                .child(
                    // Backdrop layer
                    div()
                        .id("modal-backdrop")
                        .absolute()
                        .inset_0()
                        .size_full()
                        .bg(theme.overlay_background.opacity(0.5))
                        .when(close_on_backdrop, |this| {
                            this.on_click(move |_, window, cx| {
                                if let Some(on_close) = &on_close {
                                    (on_close)(window, cx);
                                }
                            })
                        }),
                )
                .child(
                    // Content layer - centered panel
                    div()
                        .id("modal-content")
                        .relative()
                        .flex()
                        .items_center()
                        .justify_center()
                        .max_w(window.window_bounds().get_bounds().size.width * 0.9)
                        .max_h(window.window_bounds().get_bounds().size.height * 0.9)
                        .child(panel),
                )
                .when(close_on_escape, |this| {
                    this.key_context(CONTEXT)
                        .on_action({
                            let on_close = self.on_close.clone();
                            move |_: &Cancel, window: &mut Window, cx: &mut App| {
                                if let Some(on_close) = &on_close {
                                    (on_close)(window, cx);
                                }
                            }
                        })
                }),
        )
        .with_priority(100)
    }
}
