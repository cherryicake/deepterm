use crate::components::callbacks::{OptionalClickCallback, OptionalHoverCallback};
use crate::components::button::button_icon::{ButtonIcon, Disableable, Selectable, Sizable, Size};
use crate::theme::ActiveTheme;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Corners, Div, Edges, ElementId, Hsla,
    InteractiveElement, Interactivity, IntoElement, MouseButton, ParentElement, Pixels, Render,
    RenderOnce, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div, px,
    relative, transparent_black,
};
use std::rc::Rc;

/// Button rounded corner variants
#[derive(Default, Clone, Copy)]
pub enum ButtonRounded {
    None,
    Small,
    #[default]
    Medium,
    Large,
    Size(Pixels),
}

impl From<Pixels> for ButtonRounded {
    fn from(px: Pixels) -> Self {
        ButtonRounded::Size(px)
    }
}

/// Custom button variant style
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ButtonCustomVariant {
    color: Hsla,
    foreground: Hsla,
    hover: Hsla,
    active: Hsla,
    shadow: bool,
}

impl ButtonCustomVariant {
    pub fn new(cx: &App) -> Self {
        let theme = cx.theme().ui;
        Self {
            color: theme.elevated_background,
            foreground: theme.text,
            hover: theme.tab_hover_background,
            active: theme.tab_active_background,
            shadow: false,
        }
    }

    /// Set background color
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }

    /// Set foreground color
    pub fn foreground(mut self, color: Hsla) -> Self {
        self.foreground = color;
        self
    }

    /// Set hover background color
    pub fn hover(mut self, color: Hsla) -> Self {
        self.hover = color;
        self
    }

    /// Set active background color
    pub fn active(mut self, color: Hsla) -> Self {
        self.active = color;
        self
    }

    /// Set shadow
    pub fn shadow(mut self, shadow: bool) -> Self {
        self.shadow = shadow;
        self
    }
}

/// Button variant types
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ButtonVariant {
    #[default]
    Default,
    Primary,
    Secondary,
    Danger,
    Info,
    Success,
    Warning,
    Ghost,
    Link,
    Text,
    Custom(ButtonCustomVariant),
}

impl ButtonVariant {
    #[inline]
    pub fn is_link(&self) -> bool {
        matches!(self, Self::Link)
    }

    #[inline]
    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text)
    }

    #[inline]
    pub fn is_ghost(&self) -> bool {
        matches!(self, Self::Ghost)
    }

    #[inline]
    fn no_padding(&self) -> bool {
        self.is_link() || self.is_text()
    }

    #[inline]
    fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

/// Button variants trait
pub trait ButtonVariants: Sized {
    fn with_variant(self, variant: ButtonVariant) -> Self;

    fn primary(self) -> Self {
        self.with_variant(ButtonVariant::Primary)
    }

    fn secondary(self) -> Self {
        self.with_variant(ButtonVariant::Secondary)
    }

    fn danger(self) -> Self {
        self.with_variant(ButtonVariant::Danger)
    }

    fn warning(self) -> Self {
        self.with_variant(ButtonVariant::Warning)
    }

    fn success(self) -> Self {
        self.with_variant(ButtonVariant::Success)
    }

    fn info(self) -> Self {
        self.with_variant(ButtonVariant::Info)
    }

    fn ghost(self) -> Self {
        self.with_variant(ButtonVariant::Ghost)
    }

    fn link(self) -> Self {
        self.with_variant(ButtonVariant::Link)
    }

    fn text(self) -> Self {
        self.with_variant(ButtonVariant::Text)
    }

    fn custom(self, style: ButtonCustomVariant) -> Self {
        self.with_variant(ButtonVariant::Custom(style))
    }
}

/// A Button element
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    base: Stateful<Div>,
    icon: Option<ButtonIcon>,
    label: Option<SharedString>,
    children: Vec<AnyElement>,
    disabled: bool,
    pub(crate) selected: bool,
    variant: ButtonVariant,
    rounded: ButtonRounded,
    outline: bool,
    border_corners: Corners<bool>,
    border_edges: Edges<bool>,
    size: Size,
    compact: bool,
    custom_height: Option<Pixels>,
    custom_width: Option<Pixels>,
    custom_px: Option<Pixels>,
    /// 悬停提示文本，在 render 时通过 gpui 的 tooltip 机制渲染。
    tooltip: Option<SharedString>,
    on_click: OptionalClickCallback,
    on_hover: OptionalHoverCallback,
    loading: bool,
    tab_index: isize,
    tab_stop: bool,
}

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        Self {
            id: id.clone(),
            base: div().flex_shrink_0().id(id),
            icon: None,
            label: None,
            disabled: false,
            selected: false,
            variant: ButtonVariant::default(),
            rounded: ButtonRounded::Medium,
            border_corners: Corners {
                top_left: true,
                top_right: true,
                bottom_left: true,
                bottom_right: true,
            },
            border_edges: Edges::all(true),
            size: Size::Medium,
            tooltip: None,
            on_click: None,
            on_hover: None,
            loading: false,
            compact: false,
            custom_height: None,
            custom_width: None,
            custom_px: None,
            outline: false,
            children: Vec::new(),
            tab_index: 0,
            tab_stop: true,
        }
    }

    /// Set the outline style of the Button
    pub fn outline(mut self) -> Self {
        self.outline = true;
        self
    }

    /// Set the border radius of the Button
    pub fn rounded(mut self, rounded: impl Into<ButtonRounded>) -> Self {
        self.rounded = rounded.into();
        self
    }

    /// Set label to the Button
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the icon of the button
    pub fn icon(mut self, icon: impl Into<ButtonIcon>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// Set the tooltip of the button
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// 计算焦点环样式：仅在获得焦点时显示，用外发光而不是边框，避免影响布局。
    fn focus_ring_style(theme: &crate::theme::UiTheme) -> gpui::BoxShadow {
        gpui::BoxShadow {
            color: theme.accent.opacity(0.45),
            offset: gpui::Point::default(),
            blur_radius: px(0.),
            spread_radius: px(2.),
            inset: false,
        }
    }

    /// Set true to show the loading indicator
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    /// Set the button to compact mode
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// Set a custom height for the button, overriding the size-based default.
    pub fn height(mut self, height: impl Into<Pixels>) -> Self {
        self.custom_height = Some(height.into());
        self
    }

    /// Set a custom width for the button.
    pub fn width(mut self, width: impl Into<Pixels>) -> Self {
        self.custom_width = Some(width.into());
        self
    }

    /// Set a custom horizontal padding for the button, overriding the size-based default.
    pub fn px(mut self, px: impl Into<Pixels>) -> Self {
        self.custom_px = Some(px.into());
        self
    }

    /// Add click handler
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// Add hover handler
    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Rc::new(handler));
        self
    }

    /// Set the tab index
    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = tab_index;
        self
    }

    /// Set the tab stop
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    #[inline]
    fn clickable(&self) -> bool {
        !(self.disabled || self.loading) && self.on_click.is_some()
    }

    #[inline]
    fn hoverable(&self) -> bool {
        !(self.disabled || self.loading) && self.on_hover.is_some()
    }
}

impl Disableable for Button {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Selectable for Button {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl Sizable for Button {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl ButtonVariants for Button {
    fn with_variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }
}

impl ParentElement for Button {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl InteractiveElement for Button {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let _theme = cx.theme().ui;
        let style: ButtonVariant = self.variant;
        let clickable = self.clickable();
        let is_disabled = self.disabled;
        let hoverable = self.hoverable();
        let normal_style = style.normal(self.outline, cx);
        let icon_size = match self.size {
            Size::Size(v) => Size::Size(v * 0.75),
            Size::XSmall => Size::Size(px(10.0)),
            Size::Small => Size::Size(px(12.0)),
            Size::Medium => Size::Size(px(14.0)),
            Size::Large => Size::Size(px(18.0)),
        };

        let focus_handle = window
            .use_keyed_state(self.id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let is_focused = focus_handle.is_focused(window);

        // Calculate corner radius based on theme
        let radius = match self.rounded {
            ButtonRounded::Small => px(3.0),
            ButtonRounded::Medium => px(6.0),
            ButtonRounded::Large => px(12.0),
            ButtonRounded::Size(px_val) => px_val,
            ButtonRounded::None => Pixels::ZERO,
        };

        self.base
            .when(!self.disabled, |this| {
                this.track_focus(
                    &focus_handle
                        .tab_index(self.tab_index)
                        .tab_stop(self.tab_stop),
                )
            })
            .cursor_default()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .when(self.variant.is_link(), |this| this.cursor_pointer())
            .when(!style.no_padding(), |this| {
                if self.label.is_none() && self.children.is_empty() {
                    // Icon Button
                    if let Some(h) = self.custom_height {
                        this.size(h)
                    } else {
                        match self.size {
                            Size::Size(px_val) => this.size(px_val),
                            Size::XSmall => this.size(px(24.0)),
                            Size::Small => this.size(px(28.0)),
                            Size::Medium => this.size(px(36.0)),
                            Size::Large => this.size(px(44.0)),
                        }
                    }
                } else {
                    // Normal Button
                    let h = self.custom_height;
                    let px_val = self.custom_px;
                    match self.size {
                        Size::Size(_) => {
                            this.h(h.unwrap_or(px(36.0))).px(px_val.unwrap_or(px(12.0)))
                        }
                        Size::XSmall => this.h(h.unwrap_or(px(24.0))).px(px_val.unwrap_or(px(6.0))),
                        Size::Small => this.h(h.unwrap_or(px(28.0))).px(px_val.unwrap_or(px(10.0))),
                        Size::Medium => {
                            this.h(h.unwrap_or(px(36.0))).px(px_val.unwrap_or(px(16.0)))
                        }
                        Size::Large => this.h(h.unwrap_or(px(44.0))).px(px_val.unwrap_or(px(20.0))),
                    }
                }
            })
            .when_some(self.custom_width, |this, w| this.w(w))
            .when(self.border_corners.top_left, |this| this.rounded_tl(radius))
            .when(self.border_corners.top_right, |this| {
                this.rounded_tr(radius)
            })
            .when(self.border_corners.bottom_left, |this| {
                this.rounded_bl(radius)
            })
            .when(self.border_corners.bottom_right, |this| {
                this.rounded_br(radius)
            })
            .when(self.variant.is_default() || self.outline, |this| {
                this.when(self.border_edges.left, |this| this.border_l_1())
                    .when(self.border_edges.right, |this| this.border_r_1())
                    .when(self.border_edges.top, |this| this.border_t_1())
                    .when(self.border_edges.bottom, |this| this.border_b_1())
            })
            .text_color(normal_style.fg)
            .when(!self.disabled, |this| {
                if self.selected {
                    // 选中态：保持选中样式，但悬停/按下时降低背景不透明度，保留可交互反馈
                    let selected_style = style.selected(self.outline, cx);
                    this.bg(selected_style.bg)
                        .border_color(selected_style.border)
                        .text_color(selected_style.fg)
                        .when(!self.loading, |this| {
                            this.hover(|this| this.bg(selected_style.bg.opacity(0.85)))
                                .active(|this| this.bg(selected_style.bg.opacity(0.7)))
                        })
                } else {
                    this.border_color(normal_style.border)
                        .bg(normal_style.bg)
                        .when(normal_style.underline, |this| this.text_decoration_1())
                        .hover(|this| {
                            let hover_style = style.hovered(self.outline, cx);
                            this.bg(hover_style.bg)
                                .border_color(hover_style.border)
                                .text_color(hover_style.fg)
                        })
                        .active(|this| {
                            let active_style = style.active(self.outline, cx);
                            this.bg(active_style.bg)
                                .border_color(active_style.border)
                                .text_color(active_style.fg)
                        })
                }
            })
            .when(self.disabled, |this| {
                let disabled_style = style.disabled(self.outline, cx);
                this.bg(disabled_style.bg)
                    .text_color(disabled_style.fg)
                    .border_color(disabled_style.border)
            })
            .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                if is_disabled {
                    cx.stop_propagation();
                }
            })
            .when_some(self.on_click, |this, on_click| {
                this.on_click(move |event, window, cx| {
                    if !clickable {
                        cx.stop_propagation();
                        return;
                    }
                    on_click(event, window, cx);
                })
            })
            .when_some(self.on_hover.filter(|_| hoverable), |this, on_hover| {
                this.on_hover(move |hovered, window, cx| {
                    on_hover(hovered, window, cx);
                })
            })
            .when(is_focused && !is_disabled, |this| {
                // 键盘焦点可见性：用 accent 外发光作为焦点环，不影响布局。
                this.shadow(vec![Self::focus_ring_style(&cx.theme().ui)])
            })
            .when_some(self.tooltip.filter(|_| !is_disabled), |this, tooltip| {
                this.tooltip(move |_window, cx| {
                    cx.new(|_| ButtonTooltip {
                        label: tooltip.clone(),
                    })
                    .into()
                })
            })
            .child({
                div()
                    .id("label")
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .when_some(self.icon, |this, icon| {
                        this.child(icon.loading(self.loading).with_size(icon_size))
                    })
                    .when_some(self.label, |this, label| {
                        let font_size = match self.size {
                            Size::XSmall => px(10.0),
                            Size::Small => px(12.0),
                            Size::Medium => px(14.0),
                            Size::Large => px(16.0),
                            Size::Size(size) => size * 0.8,
                        };
                        this.child(div().flex_none().text_size(font_size).child(label))
                    })
                    .children(self.children)
            })
            .when(self.loading && !self.disabled, |this| {
                this.bg(normal_style.bg.opacity(0.8))
                    .border_color(normal_style.border.opacity(0.8))
                    .text_color(normal_style.fg.opacity(0.8))
            })
    }
}

/// 按钮悬停提示气泡，与标签栏 Tooltip 的样式保持一致。
struct ButtonTooltip {
    label: SharedString,
}

impl Render for ButtonTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .id("button-tooltip")
            .max_w(px(320.0))
            .px_2()
            .py_1()
            .rounded(px(6.0))
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .shadow(vec![gpui::BoxShadow {
                color: theme.shadow,
                offset: gpui::point(px(0.0), px(4.0)),
                blur_radius: px(12.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .text_size(px(12.0))
            .text_color(theme.text)
            .line_height(relative(1.3))
            .child(self.label.clone())
    }
}

/// Button variant style
struct ButtonVariantStyle {
    bg: Hsla,
    border: Hsla,
    fg: Hsla,
    underline: bool,
}

impl ButtonVariant {
    fn bg_color(&self, outline: bool, cx: &App) -> Hsla {
        let theme = cx.theme().ui;
        if outline {
            return theme.app_background;
        }

        match self {
            Self::Default => theme.app_background,
            Self::Primary => theme.accent,
            Self::Secondary => theme.panel_background,
            Self::Danger => theme.error,
            Self::Warning => theme.warning,
            Self::Success => theme.success,
            Self::Info => theme.accent.opacity(0.8),
            Self::Ghost | Self::Link | Self::Text => transparent_black(),
            Self::Custom(colors) => colors.color,
        }
    }

    fn text_color(&self, _outline: bool, cx: &App) -> Hsla {
        let theme = cx.theme().ui;
        match self {
            Self::Default => theme.text,
            Self::Primary => theme.accent_foreground,
            Self::Secondary => theme.text,
            Self::Danger => theme.error_foreground,
            Self::Warning => theme.text,
            Self::Success => theme.success_foreground,
            Self::Info => theme.accent_foreground,
            Self::Ghost => theme.text,
            Self::Link => theme.accent,
            Self::Text => theme.text,
            Self::Custom(colors) => colors.foreground,
        }
    }

    fn border_color(&self, outline: bool, cx: &App) -> Hsla {
        let theme = cx.theme().ui;
        if outline {
            return match self {
                Self::Primary => theme.accent,
                Self::Danger => theme.error,
                Self::Warning => theme.warning,
                Self::Success => theme.success,
                _ => theme.border,
            };
        }

        match self {
            Self::Default => theme.border,
            Self::Secondary => theme.border,
            Self::Primary => theme.accent,
            Self::Danger => theme.error,
            Self::Info => theme.accent,
            Self::Warning => theme.warning,
            Self::Success => theme.success,
            Self::Ghost | Self::Link | Self::Text => transparent_black(),
            Self::Custom(colors) => colors.color,
        }
    }

    fn underline(&self, _: &App) -> bool {
        matches!(self, Self::Link)
    }

    fn normal(&self, outline: bool, cx: &App) -> ButtonVariantStyle {
        let bg = self.bg_color(outline, cx);
        let border = self.border_color(outline, cx);
        let fg = self.text_color(outline, cx);
        let underline = self.underline(cx);

        ButtonVariantStyle {
            bg,
            border,
            fg,
            underline,
        }
    }

    fn hovered(&self, outline: bool, cx: &App) -> ButtonVariantStyle {
        let theme = cx.theme().ui;
        let bg = match self {
            Self::Default => theme.tab_hover_background,
            Self::Primary => theme.accent_hover,
            Self::Secondary => theme.tab_hover_background,
            Self::Danger => theme.error.blend(theme.elevated_background.opacity(0.2)),
            Self::Warning => theme.warning.blend(theme.elevated_background.opacity(0.2)),
            Self::Success => theme.success.blend(theme.elevated_background.opacity(0.2)),
            Self::Info => theme.accent.blend(theme.elevated_background.opacity(0.2)),
            Self::Custom(colors) => colors.hover,
            Self::Ghost => theme.tab_hover_background,
            Self::Link | Self::Text => transparent_black(),
        };

        let border = self.border_color(outline, cx);
        let fg = self.text_color(outline, cx);
        let underline = self.underline(cx);

        ButtonVariantStyle {
            bg,
            border,
            fg,
            underline,
        }
    }

    fn active(&self, outline: bool, cx: &App) -> ButtonVariantStyle {
        let theme = cx.theme().ui;
        let bg = match self {
            Self::Default => theme.tab_active_background,
            Self::Primary => theme.accent.opacity(0.8),
            Self::Secondary => theme.tab_active_background,
            Self::Ghost => theme.tab_active_background,
            Self::Danger => theme.error.blend(theme.elevated_background.opacity(0.4)),
            Self::Warning => theme.warning.blend(theme.elevated_background.opacity(0.4)),
            Self::Success => theme.success.blend(theme.elevated_background.opacity(0.4)),
            Self::Info => theme.accent.blend(theme.elevated_background.opacity(0.4)),
            Self::Custom(colors) => colors.active,
            Self::Link | Self::Text => transparent_black(),
        };
        let border = self.border_color(outline, cx);
        let fg = match self {
            Self::Link | Self::Text => theme.text.opacity(0.7),
            _ => self.text_color(outline, cx),
        };
        let underline = self.underline(cx);

        ButtonVariantStyle {
            bg,
            border,
            fg,
            underline,
        }
    }

    fn selected(&self, outline: bool, cx: &App) -> ButtonVariantStyle {
        let theme = cx.theme().ui;
        let bg = match self {
            Self::Default => theme.tab_active_background,
            Self::Primary => theme.accent.opacity(0.9),
            Self::Secondary | Self::Ghost => theme.tab_active_background,
            Self::Danger => theme.error.opacity(0.9),
            Self::Warning => theme.warning.opacity(0.9),
            Self::Success => theme.success.opacity(0.9),
            Self::Info => theme.accent.opacity(0.9),
            Self::Link | Self::Text => transparent_black(),
            Self::Custom(colors) => colors.active,
        };

        let border = self.border_color(outline, cx);
        let fg = match self {
            Self::Link | Self::Text => theme.text.opacity(0.7),
            _ => self.text_color(outline, cx),
        };
        let underline = self.underline(cx);

        ButtonVariantStyle {
            bg,
            border,
            fg,
            underline,
        }
    }

    fn disabled(&self, outline: bool, cx: &App) -> ButtonVariantStyle {
        let theme = cx.theme().ui;
        let bg = match self {
            Self::Default | Self::Link | Self::Ghost | Self::Text => transparent_black(),
            Self::Primary => theme.accent.opacity(0.3),
            Self::Danger => theme.error.opacity(0.3),
            Self::Warning => theme.warning.opacity(0.3),
            Self::Success => theme.success.opacity(0.3),
            Self::Info => theme.accent.opacity(0.3),
            Self::Secondary => theme.panel_background.opacity(0.5),
            Self::Custom(style) => style.color.opacity(0.3),
        };
        let fg = theme.text_muted.opacity(0.5);
        let border = if outline {
            theme.border.opacity(0.5)
        } else if let Self::Default = self {
            theme.border.opacity(0.5)
        } else {
            bg
        };

        let underline = self.underline(cx);

        ButtonVariantStyle {
            bg,
            border,
            fg,
            underline,
        }
    }
}
