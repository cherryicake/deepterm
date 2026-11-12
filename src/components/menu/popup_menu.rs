use crate::components::button::button_icon::Sizable;
use crate::components::button::{Icon, Size};
use crate::components::callbacks::OptionalClickCallback;
use crate::components::menu::menu_item::MenuItemElement;
use crate::theme::ActiveTheme;
use gpui::{
    Action, App, AppContext as _, ClickEvent, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, SharedString,
    Styled, Window, actions, div, prelude::FluentBuilder, px, rems,
};
use std::rc::Rc;

const CONTEXT: &str = "PopupMenu";

/// 勾选菜单项的 check 图标路径（与 `assets::Icon::Check` 同源）。
const CHECK_ICON_PATH: &str = "icons/check.svg";

actions!(popup_menu, [Confirm, Cancel, SelectUp, SelectDown]);

pub fn init(cx: &mut App) {
    crate::keymap::bind(
        cx,
        [
            KeyBinding::new("enter", Confirm, Some(CONTEXT)),
            KeyBinding::new("escape", Cancel, Some(CONTEXT)),
            KeyBinding::new("up", SelectUp, Some(CONTEXT)),
            KeyBinding::new("down", SelectDown, Some(CONTEXT)),
        ],
    );
}

/// An menu item in a popup menu.
pub enum PopupMenuItem {
    /// A menu separator item.
    Separator,
    /// A non-interactive label item.
    Label(SharedString),
    /// A standard menu item.
    Item {
        icon: Option<Icon>,
        label: SharedString,
        disabled: bool,
        /// 勾选状态：渲染时在左侧图标槽显示 check 图标（替换原图标，与 macOS 菜单一致）。
        checked: bool,
        action: Option<Box<dyn Action>>,
        handler: OptionalClickCallback,
        /// 右对齐的快捷键提示（herdr 菜单的 `shortcut()`）。
        ///
        /// 只用于显示：真正的按键绑定在 action 系统里，这里不参与分发。
        /// 没有绑定时留空，渲染时那一列不占位。
        shortcut: Option<SharedString>,
    },
}

impl FluentBuilder for PopupMenuItem {}
impl PopupMenuItem {
    /// Create a new menu item with the given label.
    #[inline]
    pub fn new(label: impl Into<SharedString>) -> Self {
        PopupMenuItem::Item {
            icon: None,
            label: label.into(),
            disabled: false,
            checked: false,
            action: None,
            handler: None,
            shortcut: None,
        }
    }

    /// Create a separator menu item.
    #[inline]
    pub fn separator() -> Self {
        PopupMenuItem::Separator
    }

    /// Creates a label menu item.
    #[inline]
    pub fn label(label: impl Into<SharedString>) -> Self {
        PopupMenuItem::Label(label.into())
    }

    /// Set the icon for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`].
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        if let PopupMenuItem::Item { icon: i, .. } = &mut self {
            *i = Some(icon.into());
        }
        self
    }

    /// Set the disabled state for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`].
    pub fn disabled(mut self, disabled: bool) -> Self {
        if let PopupMenuItem::Item { disabled: d, .. } = &mut self {
            *d = disabled;
        }
        self
    }

    /// Set the action for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`].
    pub fn action(mut self, action: Box<dyn Action>) -> Self {
        if let PopupMenuItem::Item { action: a, .. } = &mut self {
            *a = Some(action);
        }
        self
    }

    /// 设置右对齐的快捷键提示（如 `"⌘D"`）。
    ///
    /// 纯展示：按下的键由 action 系统分发，菜单不拦截。
    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        if let PopupMenuItem::Item { shortcut: s, .. } = &mut self {
            *s = Some(shortcut.into());
        }
        self
    }

    /// Set checked state for the menu item.
    ///
    /// 勾选的菜单项在左侧图标槽位显示 check 图标；若菜单中存在勾选项或图标项，
    /// 所有条目会统一保留图标槽位以对齐标签文本。
    pub fn checked(mut self, checked: bool) -> Self {
        if let PopupMenuItem::Item { checked: c, .. } = &mut self {
            *c = checked;
        }
        self
    }

    /// Add a click handler for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`].
    pub fn on_click<F>(mut self, handler: F) -> Self
    where
        F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    {
        if let PopupMenuItem::Item { handler: h, .. } = &mut self {
            *h = Some(Rc::new(handler));
        }
        self
    }

    #[inline]
    fn is_clickable(&self) -> bool {
        !matches!(self, PopupMenuItem::Separator)
            && matches!(
                self,
                PopupMenuItem::Item {
                    disabled: false,
                    ..
                }
            )
    }

    #[inline]
    fn is_separator(&self) -> bool {
        matches!(self, PopupMenuItem::Separator)
    }
}

pub struct PopupMenu {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) menu_items: Vec<PopupMenuItem>,
    selected_index: Option<usize>,
}

impl PopupMenu {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            menu_items: Vec::new(),
            selected_index: None,
        }
    }

    pub fn build(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(Self, &mut Window, &mut Context<PopupMenu>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| f(Self::new(cx), window, cx))
    }

    /// Add Menu Item
    pub fn menu(mut self, label: impl Into<SharedString>, action: Box<dyn Action>) -> Self {
        self.add_menu_item(label, None, action, false, false);
        self
    }

    /// 带图标与快捷键提示的菜单项（herdr 菜单的行样式：图标 + 标签 + 右对齐键位）。
    pub fn menu_with_shortcut(
        mut self,
        label: impl Into<SharedString>,
        icon: impl Into<Icon>,
        shortcut: impl Into<SharedString>,
        action: Box<dyn Action>,
    ) -> Self {
        self.menu_items.push(
            PopupMenuItem::new(label)
                .icon(icon)
                .shortcut(shortcut)
                .action(action),
        );
        self
    }

    /// Add Menu Item with Icon.
    pub fn menu_with_icon(
        mut self,
        label: impl Into<SharedString>,
        icon: impl Into<Icon>,
        action: Box<dyn Action>,
    ) -> Self {
        self.add_menu_item(label, Some(icon.into()), action, false, false);
        self
    }

    /// Add label
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.menu_items.push(PopupMenuItem::label(label.into()));
        self
    }

    /// Add a separator Menu Item
    pub fn separator(mut self) -> Self {
        if self.menu_items.is_empty() {
            return self;
        }

        if let Some(PopupMenuItem::Separator) = self.menu_items.last() {
            return self;
        }

        self.menu_items.push(PopupMenuItem::separator());
        self
    }

    /// Add menu item.
    pub fn item(mut self, item: impl Into<PopupMenuItem>) -> Self {
        let item: PopupMenuItem = item.into();
        self.menu_items.push(item);
        self
    }

    fn add_menu_item(
        &mut self,
        label: impl Into<SharedString>,
        icon: Option<Icon>,
        action: Box<dyn Action>,
        disabled: bool,
        checked: bool,
    ) -> &mut Self {
        self.menu_items.push(
            PopupMenuItem::new(label)
                .when_some(icon, |item, icon| item.icon(icon))
                .disabled(disabled)
                .checked(checked)
                .action(action),
        );
        self
    }

    pub fn is_empty(&self) -> bool {
        self.menu_items.is_empty()
    }

    fn clickable_menu_items(&self) -> impl Iterator<Item = (usize, &PopupMenuItem)> {
        self.menu_items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_clickable())
    }

    fn on_click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.prevent_default();
        self.selected_index = Some(ix);
        self.confirm(&Confirm, window, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index
            && let Some(PopupMenuItem::Item {
                handler, action, ..
            }) = self.menu_items.get(index)
        {
            if let Some(handler) = handler {
                handler(&ClickEvent::default(), window, cx);
            } else if let Some(action) = action.as_ref() {
                window.dispatch_action(action.boxed_clone(), cx);
            }

            self.dismiss(&Cancel, window, cx);
        }
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.selected_index != Some(ix) {
            self.selected_index = Some(ix);
            cx.notify();
        }
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let ix = self.selected_index.unwrap_or(0);

        if let Some((prev_ix, _)) = self
            .menu_items
            .iter()
            .enumerate()
            .rev()
            .find(|(i, item)| *i < ix && item.is_clickable())
        {
            self.set_selected_index(prev_ix, cx);
            return;
        }

        let last_clickable_ix = self.clickable_menu_items().last().map(|(ix, _)| ix);
        self.set_selected_index(last_clickable_ix.unwrap_or(0), cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let Some(ix) = self.selected_index else {
            self.set_selected_index(0, cx);
            return;
        };

        if let Some((next_ix, _)) = self
            .menu_items
            .iter()
            .enumerate()
            .find(|(i, item)| *i > ix && item.is_clickable())
        {
            self.set_selected_index(next_ix, cx);
            return;
        }

        self.set_selected_index(0, cx);
    }

    /// 关闭菜单：发出 [`DismissEvent`]，由宿主（如 [`crate::components::menu::ContextMenu`]）隐藏浮层。
    fn dismiss(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn on_mouse_down_out(
        &mut self,
        _: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss(&Cancel, window, cx);
    }

    /// 菜单中是否存在图标槽位内容（图标或勾选），用于决定是否为所有条目保留图标槽位。
    fn has_icon_slots(&self) -> bool {
        self.menu_items.iter().any(|item| match item {
            PopupMenuItem::Item { icon: Some(_), .. } => true,
            PopupMenuItem::Item { checked, .. } => *checked,
            _ => false,
        })
    }

    fn render_item(
        &self,
        ix: usize,
        item: &PopupMenuItem,
        reserve_icon_slot: bool,
        cx: &mut Context<Self>,
    ) -> MenuItemElement {
        let selected = self.selected_index == Some(ix);
        let theme = cx.theme().ui;

        let this = MenuItemElement::new(ix)
            .selected(selected)
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                if *hovered {
                    this.selected_index = Some(ix);
                } else if this.selected_index == Some(ix) {
                    this.selected_index = None;
                }
                cx.notify();
            }));

        match item {
            PopupMenuItem::Separator => {
                // Separators are rendered directly in render(), not here
                this.disabled(true)
            }
            PopupMenuItem::Label(label) => this
                .h(px(18.)) // Smaller height for labels
                .disabled(true)
                .cursor_default()
                .text_color(theme.text_muted)
                .text_xs() // Smaller font size
                .child(div().cursor_default().child(label.clone())),
            PopupMenuItem::Item {
                label,
                disabled,
                icon,
                checked,
                shortcut,
                ..
            } => {
                let this = this
                    .when(!disabled, |this| {
                        this.on_click(
                            cx.listener(move |this, _, window, cx| this.on_click(ix, window, cx)),
                        )
                    })
                    .disabled(*disabled);

                let mut content = div().flex().flex_row().items_center();

                let icon_color = if *disabled {
                    theme.text_muted
                } else {
                    theme.text
                };

                // 左侧图标槽：勾选项显示 check 图标（替换原图标），否则显示条目图标。
                // 当菜单中存在任何图标/勾选内容时，统一保留槽位以对齐所有标签。
                let left_icon_path: Option<&'static str> = if *checked {
                    Some(CHECK_ICON_PATH)
                } else {
                    icon.as_ref().map(|icon| icon.path())
                };
                if let Some(path) = left_icon_path {
                    content = content.gap(px(6.)).child(
                        div()
                            .w(px(18.))
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(path)
                                    .with_size(Size::XSmall)
                                    .color(icon_color)
                                    .into_any_element(),
                            ),
                    );
                } else if reserve_icon_slot {
                    // 仅在有其他条目携带图标/勾选时占位对齐
                    content = content.gap(px(6.)).child(div().w(px(18.)).h(px(18.)));
                }

                // 标签扩展、快捷键提示右对齐（herdr 菜单的
                // `justify_between` + 尾部 dim 色小字）。
                let label_el = div().flex_1().min_w_0().truncate().child(label.clone());
                let this = this.child(content.child(label_el));
                match shortcut {
                    Some(shortcut) => this.child(
                        div()
                            .flex_none()
                            .pl(px(18.))
                            .text_size(px(11.))
                            .text_color(if *disabled {
                                theme.text_subtle
                            } else {
                                theme.text_muted
                            })
                            .child(shortcut.clone()),
                    ),
                    None => this,
                }
            }
        }
    }
}

impl EventEmitter<DismissEvent> for PopupMenu {}
crate::impl_focusable!(PopupMenu);

impl Render for PopupMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let items_count = self.menu_items.len();
        let reserve_icon_slot = self.has_icon_slots();

        div()
            .id("popup-menu")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            .rounded(px(12.))
            .occlude()
            // Menu container
            .child(
                div()
                    .id("menu-container")
                    .bg(theme.menu_background)
                    .text_color(theme.text)
                    .border(px(0.5))
                    .border_color(theme.menu_border)
                    .rounded(px(12.))
                    .p(px(6.))
                    .relative()
                    .occlude()
                    .child(
                        div()
                            .id("items")
                            .flex()
                            .flex_col()
                            .gap_y(px(0.))
                            .min_w(rems(10.))
                            .max_w(px(500.))
                            .children(
                                self.menu_items
                                    .iter()
                                    .enumerate()
                                    .filter(|(ix, item)| {
                                        !(*ix + 1 == items_count && item.is_separator())
                                    })
                                    .map(|(ix, item)| {
                                        if item.is_separator() {
                                            div()
                                                .h(px(9.))
                                                .flex()
                                                .items_center()
                                                .child(
                                                    div()
                                                        .h(px(1.))
                                                        .mx(px(8.))
                                                        .bg(theme.menu_separator.opacity(0.6))
                                                        .w_full(),
                                                )
                                                .into_any_element()
                                        } else {
                                            self.render_item(ix, item, reserve_icon_slot, cx)
                                                .into_any_element()
                                        }
                                    }),
                            ),
                    ),
            )
    }
}
