use crate::components::button::Disableable;
use crate::theme::ActiveTheme;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement, IntoElement, MouseButton,
    ParentElement, RenderOnce, StatefulInteractiveElement as _, StyleRefinement, Styled, Window,
    div, prelude::FluentBuilder as _, px,
};

#[derive(IntoElement)]
pub struct MenuItemElement {
    id: ElementId,
    style: StyleRefinement,
    disabled: bool,
    selected: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    on_hover: Option<Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>>,
    children: Vec<AnyElement>,
}

impl MenuItemElement {
    /// Create a new MenuItem with the given ID.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            disabled: false,
            selected: false,
            on_click: None,
            on_hover: None,
            children: Vec::new(),
        }
    }

    /// Set ListItem as the selected item style.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Set the disabled state of the MenuItem.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Set a handler for when the MenuItem is clicked.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    /// Set a handler for when the mouse enters the MenuItem.
    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Box::new(handler));
        self
    }
}

impl Disableable for MenuItemElement {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Styled for MenuItemElement {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for MenuItemElement {
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for MenuItemElement {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().ui;

        // 调用方通过 `impl Styled` 设置的 refinement 应用到根元素，
        // 再叠加组件自身的菜单项默认样式（高度 / 内边距 / 布局）。
        let mut root = div().id(self.id);
        *root.style() = self.style;

        root.h(px(22.)) // Fixed height like macOS menu items
            .pl(px(8.))
            .pr(px(8.))
            .text_sm()
            .text_color(theme.text)
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .rounded(px(6.)) // Subtle rounded corners for hover effect
            .when_some(self.on_hover, |this, on_hover| {
                this.on_hover(move |hovered, window, cx| (on_hover)(hovered, window, cx))
            })
            .when(!self.disabled, |this| {
                // 悬停高亮：直接用 .hover()（此前通过为每一项生成独立
                // group 实现同等效果，属于无谓的间接层）。
                this.hover(|this| this.bg(theme.accent).text_color(theme.accent_foreground))
                    .when(self.selected, |this| {
                        this.bg(theme.accent).text_color(theme.accent_foreground)
                    })
                    .when_some(self.on_click, |this, on_click| {
                        this.on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_click(on_click)
                    })
            })
            .when(self.disabled, |this| this.text_color(theme.text_muted))
            .children(self.children)
    }
}
