use chrono::{DateTime, Local};
use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Div, Entity, MouseButton, Stateful, Window, div, px};

use crate::theme::{ActiveTheme, UiTheme};

pub const COLUMN_MIN_WIDTH: f32 = 140.0;
pub const COLUMN_MAX_WIDTH: f32 = 720.0;
pub const COLUMN_HANDLE_HIT_WIDTH: f32 = 6.0;
pub const COLUMN_HANDLE_VISUAL_WIDTH: f32 = 1.0;
pub const PREVIEW_MIN_WIDTH: f32 = 560.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileBrowserViewMode {
    Columns,
    List,
}

#[derive(Clone, Copy, Debug)]
pub struct ColumnResizeState {
    index: usize,
    start_mouse_x: gpui::Pixels,
    start_width: gpui::Pixels,
}

impl ColumnResizeState {
    pub fn index(&self) -> usize {
        self.index
    }
}

#[derive(Clone, Debug)]
pub struct ColumnLayoutState {
    default_width: gpui::Pixels,
    widths: Vec<gpui::Pixels>,
    resizing: Option<ColumnResizeState>,
}

impl ColumnLayoutState {
    pub fn new(default_width: gpui::Pixels) -> Self {
        Self {
            default_width,
            widths: Vec::new(),
            resizing: None,
        }
    }

    pub fn width_at(&self, index: usize) -> gpui::Pixels {
        self.widths
            .get(index)
            .copied()
            .unwrap_or(self.default_width)
    }

    pub fn is_resizing(&self) -> bool {
        self.resizing.is_some()
    }

    pub fn is_resizing_column(&self, index: usize) -> bool {
        self.resizing
            .as_ref()
            .map_or(false, |state| state.index() == index)
    }

    pub fn clear_resize(&mut self) {
        self.resizing = None;
    }

    pub fn start_resize(&mut self, index: usize, mouse_x: gpui::Pixels) {
        self.ensure_widths(index + 1);
        self.resizing = Some(ColumnResizeState {
            index,
            start_mouse_x: mouse_x,
            start_width: self.width_at(index),
        });
    }

    pub fn resize_to(&mut self, mouse_x: gpui::Pixels) -> bool {
        let Some(state) = self.resizing else {
            return false;
        };
        let delta = mouse_x - state.start_mouse_x;
        self.set_width(state.index, state.start_width + delta)
    }

    pub fn finish_resize(&mut self) -> bool {
        self.resizing.take().is_some()
    }

    fn ensure_widths(&mut self, count: usize) {
        if self.widths.len() < count {
            self.widths.resize(count, self.default_width);
        }
    }

    fn set_width(&mut self, index: usize, width: gpui::Pixels) -> bool {
        let clamped = width.max(px(COLUMN_MIN_WIDTH)).min(px(COLUMN_MAX_WIDTH));
        self.ensure_widths(index + 1);
        if self.widths[index] == clamped {
            return false;
        }
        self.widths[index] = clamped;
        true
    }
}

#[derive(Clone, Debug)]
pub struct FileBrowserLayoutState {
    available_width: Option<gpui::Pixels>,
    columns: ColumnLayoutState,
}

impl FileBrowserLayoutState {
    pub fn new(column_default_width: gpui::Pixels) -> Self {
        Self {
            available_width: None,
            columns: ColumnLayoutState::new(column_default_width),
        }
    }

    pub fn available_width(&self) -> Option<gpui::Pixels> {
        self.available_width
    }

    pub fn set_available_width(&mut self, width: Option<gpui::Pixels>) -> bool {
        if self.available_width == width {
            return false;
        }
        self.available_width = width;
        true
    }

    pub fn should_show_preview(&self, has_preview: bool) -> bool {
        has_preview
            && self
                .available_width
                .map_or(false, |width| width >= px(PREVIEW_MIN_WIDTH))
    }

    pub fn columns(&self) -> &ColumnLayoutState {
        &self.columns
    }

    pub fn columns_mut(&mut self) -> &mut ColumnLayoutState {
        &mut self.columns
    }
}

pub trait TracksFileBrowserWidth: Sized + 'static {
    fn set_tracked_width(&mut self, width: Option<gpui::Pixels>, cx: &mut Context<Self>);
}

pub struct FileBrowserWidthTracker<T: TracksFileBrowserWidth> {
    id: &'static str,
    view: Entity<T>,
    inner: AnyElement,
}

impl<T: TracksFileBrowserWidth> FileBrowserWidthTracker<T> {
    pub fn new(id: &'static str, view: Entity<T>, inner: AnyElement) -> Self {
        Self { id, view, inner }
    }
}

impl<T: TracksFileBrowserWidth> gpui::IntoElement for FileBrowserWidthTracker<T> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<T: TracksFileBrowserWidth> gpui::Element for FileBrowserWidthTracker<T> {
    type RequestLayoutState = gpui::LayoutId;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        Some(gpui::ElementId::Name(self.id.into()))
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let layout_id = self.inner.request_layout(window, cx);
        (layout_id, layout_id)
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.inner.prepaint(window, cx);

        let width = Some(bounds.size.width);
        let view = self.view.clone();
        cx.defer(move |cx| {
            view.update(cx, |view, cx| {
                view.set_tracked_width(width, cx);
            });
        });
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(window, cx);
    }
}

pub fn render_columns_container(id: &'static str, theme: &UiTheme, columns: Div) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .h_full()
        .bg(theme.app_background)
        .overflow_x_scroll()
        .overflow_y_hidden()
        .flex()
        .child(columns)
}

pub fn render_column_resize_handle<T>(
    column_index: usize,
    is_active: bool,
    id_prefix: &'static str,
    group_prefix: &'static str,
    cx: &mut Context<T>,
    on_mouse_down: impl Fn(&mut T, &gpui::MouseDownEvent, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div>
where
    T: 'static,
{
    let theme = cx.theme().ui;
    let group_name = format!("{}{}", group_prefix, column_index);

    div()
        .id((id_prefix, column_index))
        .group(group_name.clone())
        .h_full()
        .w(px(COLUMN_HANDLE_HIT_WIDTH))
        .flex_none()
        .flex()
        .items_stretch()
        .justify_center()
        .cursor_col_resize()
        .child(
            div()
                .w(px(COLUMN_HANDLE_VISUAL_WIDTH))
                .h_full()
                .bg(if is_active {
                    theme.drop_target
                } else {
                    theme.border
                })
                .group_hover(&group_name, |this| this.bg(theme.drop_target)),
        )
        .on_mouse_down(MouseButton::Left, cx.listener(on_mouse_down))
}

pub fn format_datetime(timestamp: std::time::SystemTime) -> String {
    DateTime::<Local>::from(timestamp)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

pub fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", size, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

pub fn render_icon_grid(theme: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .gap(px(2.0))
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                )
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                ),
        )
        .child(
            div()
                .flex()
                .gap(px(2.0))
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                )
                .child(
                    div()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded(px(1.0))
                        .bg(theme.text_muted),
                ),
        )
}

pub fn render_icon_list(theme: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .w(px(10.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(10.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(10.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
}

pub fn render_icon_columns(theme: &UiTheme) -> Div {
    div()
        .flex()
        .gap(px(2.0))
        .child(
            div()
                .w(px(3.0))
                .h(px(10.0))
                .rounded(px(999.0))
                .bg(theme.text),
        )
        .child(
            div()
                .w(px(3.0))
                .h(px(10.0))
                .rounded(px(999.0))
                .bg(theme.text),
        )
        .child(
            div()
                .w(px(3.0))
                .h(px(10.0))
                .rounded(px(999.0))
                .bg(theme.text),
        )
}

pub fn render_icon_gallery(theme: &UiTheme) -> Div {
    div()
        .w(px(11.0))
        .h(px(8.0))
        .rounded(px(2.0))
        .border_1()
        .border_color(theme.text_muted)
        .child(
            div()
                .w_full()
                .h(px(2.0))
                .mt(px(4.0))
                .bg(theme.text_muted.opacity(0.8)),
        )
}

pub fn render_icon_share(theme: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(1.0))
        .child(
            div()
                .w(px(2.0))
                .h(px(6.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(8.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
}

pub fn render_icon_tag(theme: &UiTheme) -> Div {
    div()
        .w(px(10.0))
        .h(px(7.0))
        .rounded(px(2.0))
        .border_1()
        .border_color(theme.text_muted)
}

pub fn render_icon_more(theme: &UiTheme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .child(
            div()
                .w(px(2.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(2.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
        .child(
            div()
                .w(px(2.0))
                .h(px(2.0))
                .rounded(px(999.0))
                .bg(theme.text_muted),
        )
}
