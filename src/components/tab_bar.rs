use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Bounds, Context, ElementId, Entity, FocusHandle,
    Focusable, IntoElement, Pixels, Point, Render, ScrollHandle, SharedString, TextRun, TextStyle,
    Window, canvas, div, ease_out_quint, px,
};

use crate::assets::Icon;
use crate::theme::ActiveTheme;
use crate::workspace::panel::DraggedPanel;
use gpui::svg;

/// 拖拽的 Tab 数据
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedTab {
    pub tab_id: SharedString,
    pub from_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabReorderPreview {
    dragged_tab: DraggedTab,
    insert_slot: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelDropPreview {
    insert_index: usize,
    tab_label: SharedString,
}

const TAB_REORDER_ANIMATION_DURATION_MS: u64 = 155;
const TAB_REORDER_GAP_PX: f32 = 4.0;
const TAB_REORDER_FALLBACK_WIDTH_PX: f32 = 160.0;
const TAB_REORDER_DRAGGED_TAB_OPACITY: f32 = 0.0;
const TAB_DRAG_PREVIEW_MIN_WIDTH_PX: f32 = 48.0;
const TAB_DRAG_PREVIEW_PADDING_X_PX: f32 = 12.0;

/// Tab 放置事件
pub struct TabDropEvent {
    pub dragged_tab: DraggedTab,
    pub to_index: usize,
}

/// 边缘区域
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdgeZone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

/// 拖拽分割事件
pub struct TabSplitEvent {
    pub dragged_tab: DraggedTab,
    pub edge_zone: EdgeZone,
}

/// 拖拽预览视图
pub struct DragPreview {
    pub tab_label: SharedString,
    pub drag_offset: Point<Pixels>,
    pub preview_width: Pixels,
}

impl DragPreview {
    pub fn new(
        tab_label: SharedString,
        drag_offset: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self {
            preview_width: Self::measure_width(window, &tab_label, cx),
            tab_label,
            drag_offset,
        }
    }

    fn measure_width(window: &mut Window, label: &SharedString, cx: &mut App) -> Pixels {
        let font_size = px(14.0);

        let shaped_line = window.text_system().shape_line(
            label.clone(),
            font_size,
            &[TextRun {
                len: label.len(),
                font: TextStyle::default().font(),
                color: cx.theme().ui.text,
                ..Default::default()
            }],
            None,
        );

        px(
            (shaped_line.width.as_f32() + TAB_DRAG_PREVIEW_PADDING_X_PX * 2.0)
                .max(TAB_DRAG_PREVIEW_MIN_WIDTH_PX),
        )
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview_width = self.preview_width;
        let preview_height = px(30.0);
        let theme = cx.theme().ui;

        div()
            // 将预览中心对齐到鼠标位置
            .pl(self.drag_offset.x - preview_width / 2.0)
            .pt(self.drag_offset.y - preview_height / 2.0)
            .child(
                div()
                    .w(preview_width)
                    .h(preview_height)
                    .px(px(TAB_DRAG_PREVIEW_PADDING_X_PX))
                    .flex()
                    .items_center()
                    .rounded(px(8.0))
                    .bg(theme.elevated_background)
                    .border_1()
                    .border_color(theme.border_strong)
                    .opacity(0.96)
                    .shadow_md()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text)
                            .child(self.tab_label.clone()),
                    ),
            )
    }
}

/// 标签页项
#[derive(Clone)]
pub struct TabItem {
    pub id: SharedString,
    pub label: SharedString,
    pub icon: Option<Icon>,
    pub is_active: bool,
    pub is_modified: bool,
}

impl TabItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            is_active: false,
            is_modified: false,
        }
    }

    pub fn with_modified(mut self, modified: bool) -> Self {
        self.is_modified = modified;
        self
    }

    pub fn with_icon(mut self, icon: Option<Icon>) -> Self {
        self.icon = icon;
        self
    }
}

/// Tab合并事件
pub struct TabMergeEvent {
    pub dragged_tab: DraggedTab,
    pub target_tab_id: SharedString,
}

/// 溢出状态变化回调
/// 当 TabBar 检测到溢出状态变化时触发
pub type OnOverflowChanged = Rc<dyn Fn(bool, &mut App) + Send + Sync>;

/// 标签栏组件
pub struct TabBar {
    id: ElementId,
    pub tabs: Vec<TabItem>,
    pub active_tab_id: Option<SharedString>,
    on_tab_click: Option<Rc<dyn Fn(SharedString, &mut Window, &mut App) + Send + Sync>>,
    on_tab_close: Option<Rc<dyn Fn(SharedString, &mut Window, &mut App) + Send + Sync>>,
    on_new_tab: Option<Rc<dyn Fn(&mut Window, &mut App) + Send + Sync>>,
    on_tab_reorder: Option<Rc<dyn Fn(DraggedTab, usize, &mut App) + Send + Sync>>,
    on_tab_split: Option<Rc<dyn Fn(DraggedTab, EdgeZone, &mut App) + Send + Sync>>,
    on_tab_merge: Option<Rc<dyn Fn(DraggedTab, SharedString, &mut App) + Send + Sync>>,
    /// Panel 拖拽到 TabBar 的回调
    on_panel_drop:
        Option<Rc<dyn Fn(DraggedPanel, Option<usize>, &mut Window, &mut App) + Send + Sync>>,
    /// 溢出状态变化回调
    on_overflow_changed: Option<OnOverflowChanged>,
    focus_handle: FocusHandle,
    /// 滚动句柄，用于控制水平滚动
    scroll_handle: ScrollHandle,
    /// 上一次检测到的溢出状态
    last_overflow: Option<bool>,
    /// 测量到的容器宽度（通过 canvas 获取）
    measured_container_width: f32,
    /// 当前 Tab 重排预览
    reorder_preview: Option<TabReorderPreview>,
    /// 上一次 Tab 重排预览，用于做补间动画
    previous_reorder_preview: Option<TabReorderPreview>,
    /// 重排动画序号
    reorder_animation_epoch: usize,
    /// 当前渲染周期里每个 tab 的实际边界
    tab_bounds: Vec<Bounds<Pixels>>,
    /// Tab 列表容器边界，用于判断拖拽是否仍在列表区域内
    tab_list_bounds: Option<Bounds<Pixels>>,
    /// 当前 Panel 拖入 TabBar 的插入预览
    panel_drop_preview: Option<PanelDropPreview>,
}

impl TabBar {
    pub fn new(id: impl Into<ElementId>, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self {
            id: id.into(),
            tabs: Vec::new(),
            active_tab_id: None,
            on_tab_click: None,
            on_tab_close: None,
            on_new_tab: None,
            on_tab_reorder: None,
            on_tab_split: None,
            on_tab_merge: None,
            on_panel_drop: None,
            on_overflow_changed: None,
            focus_handle,
            scroll_handle: ScrollHandle::new(),
            last_overflow: None,
            measured_container_width: 0.0,
            reorder_preview: None,
            previous_reorder_preview: None,
            reorder_animation_epoch: 0,
            tab_bounds: Vec::new(),
            tab_list_bounds: None,
            panel_drop_preview: None,
        })
    }

    pub fn new_with_tabs(
        id: impl Into<ElementId>,
        cx: &mut App,
        tabs: Vec<TabItem>,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        let active_tab_id = if tabs.is_empty() {
            None
        } else {
            Some(tabs[0].id.clone())
        };

        cx.new(|_cx| Self {
            id: id.into(),
            tabs,
            active_tab_id,
            on_tab_click: None,
            on_tab_close: None,
            on_new_tab: None,
            on_tab_reorder: None,
            on_tab_split: None,
            on_tab_merge: None,
            on_panel_drop: None,
            on_overflow_changed: None,
            focus_handle,
            scroll_handle: ScrollHandle::new(),
            last_overflow: None,
            measured_container_width: 0.0,
            reorder_preview: None,
            previous_reorder_preview: None,
            reorder_animation_epoch: 0,
            tab_bounds: Vec::new(),
            tab_list_bounds: None,
            panel_drop_preview: None,
        })
    }

    pub fn on_tab_click<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_click = Some(Rc::new(callback));
    }

    pub fn on_tab_close<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_close = Some(Rc::new(callback));
    }

    pub fn on_new_tab<F>(&mut self, callback: F)
    where
        F: Fn(&mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_new_tab = Some(Rc::new(callback));
    }

    pub fn on_tab_reorder<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, usize, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_reorder = Some(Rc::new(callback));
    }

    pub fn on_tab_split<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, EdgeZone, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_split = Some(Rc::new(callback));
    }

    pub fn on_tab_merge<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, SharedString, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_merge = Some(Rc::new(callback));
    }

    /// 设置 Panel 拖拽到 TabBar 的回调
    pub fn on_panel_drop<F>(&mut self, callback: F)
    where
        F: Fn(DraggedPanel, Option<usize>, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_panel_drop = Some(Rc::new(callback));
    }

    /// 设置溢出状态变化回调
    /// 当 TabBar 检测到溢出状态变化时触发
    /// `is_overflowing` 为 true 表示内容已溢出需要滚动
    pub fn on_overflow_changed<F>(&mut self, callback: F)
    where
        F: Fn(bool, &mut App) + Send + Sync + 'static,
    {
        self.on_overflow_changed = Some(Rc::new(callback));
    }

    pub fn add_tab(&mut self, tab: TabItem, cx: &mut Context<Self>) {
        self.tabs.push(tab);
        // 提前检查溢出状态，避免 UI 上出现溢出后再切换
        self.update_overflow_state(cx);
        cx.notify();
        // 滚动到最新添加的标签（最右边）
        let last_index = self.tabs.len().saturating_sub(1);
        self.scroll_handle.scroll_to_item(last_index);
    }

    pub fn remove_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|t| t.id.as_ref() == id) {
            self.tabs.remove(index);

            // 更新活动标签
            if self.active_tab_id.as_ref().map(|s| s.as_ref()) == Some(id) {
                self.active_tab_id = if index < self.tabs.len() {
                    Some(self.tabs[index].id.clone())
                } else if !self.tabs.is_empty() {
                    Some(self.tabs[self.tabs.len() - 1].id.clone())
                } else {
                    None
                };
            }

            // 检查溢出状态是否改变（移除标签后可能不再溢出）
            self.update_overflow_state(cx);
        }
        cx.notify();
    }

    pub fn set_active_tab(&mut self, id: impl Into<SharedString>, cx: &mut Context<Self>) {
        let new_id: SharedString = id.into();
        let changed = self.active_tab_id.as_ref() != Some(&new_id);
        if changed {
            self.active_tab_id = Some(new_id);
            cx.notify();
        }
    }

    pub fn set_tabs(&mut self, tabs: Vec<TabItem>, cx: &mut Context<Self>) {
        self.tabs = tabs;
        if self.active_tab_id.is_none() && !self.tabs.is_empty() {
            self.active_tab_id = Some(self.tabs[0].id.clone());
        }
        self.update_overflow_state(cx);
        cx.notify();
    }

    pub fn active_tab_id(&self) -> Option<&SharedString> {
        self.active_tab_id.as_ref()
    }

    pub fn tabs_count(&self) -> usize {
        self.tabs.len()
    }

    /// 获取滚动句柄
    pub fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll_handle
    }

    /// 滚动到指定索引的标签
    pub fn scroll_to_tab(&self, index: usize) {
        self.scroll_handle.scroll_to_item(index);
    }

    /// 检查并更新溢出状态
    /// 当溢出状态变化时触发回调
    pub(crate) fn update_overflow_state(&mut self, cx: &mut Context<Self>) {
        if self.measured_container_width <= 0.0 {
            return;
        }

        // 预估内容宽度：每个 tab 约 180px（包含 gap），加上 "+" 按钮 40px
        const ESTIMATED_TAB_WIDTH: f32 = 180.0;
        const NEW_TAB_BUTTON_WIDTH: f32 = 40.0;

        let estimated_content_width =
            self.tabs.len() as f32 * ESTIMATED_TAB_WIDTH + NEW_TAB_BUTTON_WIDTH;
        let would_overflow = estimated_content_width > self.measured_container_width;

        if self.last_overflow != Some(would_overflow) {
            self.last_overflow = Some(would_overflow);
            if let Some(ref callback) = self.on_overflow_changed {
                callback(would_overflow, cx);
            }
        }
    }

    fn handle_tab_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        let Some(bounds) = self.tab_list_bounds else {
            return;
        };

        if !Self::bounds_contains_position(bounds, position) {
            self.clear_tab_reorder_preview(cx);
        }
    }

    fn handle_panel_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedPanel>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        let dragged: &DraggedPanel = event.drag(cx);
        let Some(bounds) = self.tab_list_bounds else {
            return;
        };

        if !Self::bounds_contains_position(bounds, position) {
            self.clear_panel_drop_preview(cx);
            return;
        }

        let insert_index = self.tab_insert_slot_for_position(position);
        self.set_panel_drop_preview(insert_index, dragged.tab_label.clone(), cx);
    }

    fn bounds_contains_position(bounds: Bounds<Pixels>, position: Point<Pixels>) -> bool {
        position.x >= bounds.left()
            && position.x <= bounds.right()
            && position.y >= bounds.top()
            && position.y <= bounds.bottom()
    }

    fn set_tab_reorder_preview(
        &mut self,
        dragged_tab: &DraggedTab,
        to_index: usize,
        cx: &mut Context<Self>,
    ) {
        let next_preview = TabReorderPreview {
            dragged_tab: dragged_tab.clone(),
            insert_slot: to_index.min(self.tabs.len().saturating_sub(1)),
        };

        if self.reorder_preview.as_ref() == Some(&next_preview) {
            return;
        }

        self.panel_drop_preview = None;
        self.previous_reorder_preview = self.reorder_preview.clone();
        self.reorder_preview = Some(next_preview);
        self.reorder_animation_epoch += 1;
        cx.notify();
    }

    fn clear_tab_reorder_preview(&mut self, cx: &mut Context<Self>) {
        if self.reorder_preview.is_none() {
            return;
        }

        self.previous_reorder_preview = self.reorder_preview.take();
        self.reorder_animation_epoch += 1;
        cx.notify();
    }

    fn set_panel_drop_preview(
        &mut self,
        insert_index: usize,
        tab_label: SharedString,
        cx: &mut Context<Self>,
    ) {
        let next_preview = PanelDropPreview {
            insert_index: insert_index.min(self.tabs.len()),
            tab_label,
        };

        if self.panel_drop_preview.as_ref() == Some(&next_preview) {
            return;
        }

        self.reorder_preview = None;
        self.previous_reorder_preview = None;
        self.panel_drop_preview = Some(next_preview);
        cx.notify();
    }

    fn clear_panel_drop_preview(&mut self, cx: &mut Context<Self>) {
        if self.panel_drop_preview.is_none() {
            return;
        }

        self.panel_drop_preview = None;
        cx.notify();
    }

    fn reset_tab_reorder_preview(&mut self, cx: &mut Context<Self>) {
        if self.reorder_preview.is_none() && self.previous_reorder_preview.is_none() {
            return;
        }

        self.reorder_preview = None;
        self.previous_reorder_preview = None;
        self.reorder_animation_epoch += 1;
        cx.notify();
    }

    fn reset_panel_drop_preview(&mut self, cx: &mut Context<Self>) {
        if self.panel_drop_preview.is_none() {
            return;
        }

        self.panel_drop_preview = None;
        cx.notify();
    }

    fn apply_tab_reorder_locally(&mut self, dragged_tab: &DraggedTab, to_index: usize) -> bool {
        let Some(from_index) = self.tabs.iter().position(|t| t.id == dragged_tab.tab_id) else {
            return false;
        };

        if from_index == to_index || to_index >= self.tabs.len() {
            return false;
        }

        let tab = self.tabs.remove(from_index);
        self.tabs.insert(to_index, tab);
        true
    }

    fn tab_insert_slot_for_position(&self, position: Point<Pixels>) -> usize {
        for (index, bounds) in self.tab_bounds.iter().enumerate() {
            let midpoint = bounds.left() + bounds.size.width / 2.0;
            if position.x < midpoint {
                return index;
            }
        }

        self.tabs.len()
    }

    fn reorder_preview_has_effect(preview: &TabReorderPreview, tab_count: usize) -> bool {
        if preview.dragged_tab.from_index >= tab_count {
            return false;
        }

        preview.insert_slot < tab_count && preview.insert_slot != preview.dragged_tab.from_index
    }

    fn dragged_tab_shift_width(&self, preview: &TabReorderPreview) -> f32 {
        self.tab_bounds
            .get(preview.dragged_tab.from_index)
            .map(|bounds| {
                let width: f32 = bounds.size.width.into();
                width + TAB_REORDER_GAP_PX
            })
            .unwrap_or(TAB_REORDER_FALLBACK_WIDTH_PX)
    }

    fn tab_offset_for_preview(&self, preview: Option<&TabReorderPreview>, index: usize) -> f32 {
        let Some(preview) = preview else {
            return 0.0;
        };

        if !Self::reorder_preview_has_effect(preview, self.tabs.len()) {
            return 0.0;
        }

        let from_index = preview.dragged_tab.from_index;
        let to_index = preview.insert_slot.min(self.tabs.len().saturating_sub(1));
        let shift = self.dragged_tab_shift_width(preview);

        if to_index > from_index {
            if index > from_index && index <= to_index {
                -shift
            } else {
                0.0
            }
        } else if to_index < from_index {
            if index >= to_index && index < from_index {
                shift
            } else {
                0.0
            }
        } else {
            0.0
        }
    }

    fn tab_opacity_for_preview(&self, preview: Option<&TabReorderPreview>, index: usize) -> f32 {
        let Some(preview) = preview else {
            return 1.0;
        };

        if Self::reorder_preview_has_effect(preview, self.tabs.len())
            && index == preview.dragged_tab.from_index
        {
            TAB_REORDER_DRAGGED_TAB_OPACITY
        } else {
            1.0
        }
    }
}

impl Focusable for TabBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TabBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let on_tab_click = self.on_tab_click.clone();
        let on_tab_close = self.on_tab_close.clone();
        let on_new_tab = self.on_new_tab.clone();
        let on_tab_reorder = self.on_tab_reorder.clone();
        let _on_tab_merge = self.on_tab_merge.clone();
        let on_panel_drop = self.on_panel_drop.clone();
        let animation_epoch = self.reorder_animation_epoch;
        let panel_drop_preview = self.panel_drop_preview.clone();

        // 根据溢出状态决定布局模式
        // 如果还没测量过，根据预估宽度预判；否则使用测量结果
        let is_overflowing = match self.last_overflow {
            Some(overflow) => overflow,
            None => {
                // 未测量时，如果有多个标签，默认启用滚动以避免溢出
                // 一旦测量完成，会根据实际宽度调整
                self.tabs.len() > 1
            }
        };

        div()
            .id(self.id.clone())
            .h_full()
            .w_full()
            .flex()
            .items_center()
            .bg(theme.tab_bar_background)
            .overflow_hidden()
            .pt_1()
            .pb(px(2.))
            // 阻止 TabBar 区域的鼠标事件冒泡到 TitleBar，防止触发窗口拖拽
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.stop_propagation();
                }),
            )
            // TabBar 整体接受 DraggedPanel drop
            .on_drop(cx.listener({
                let on_panel_drop = on_panel_drop.clone();
                move |this, dragged: &DraggedPanel, window, cx| {
                    let insert_index = this
                        .panel_drop_preview
                        .as_ref()
                        .map(|preview| preview.insert_index);
                    this.reset_panel_drop_preview(cx);
                    if let Some(ref callback) = on_panel_drop {
                        callback(dragged.clone(), insert_index, window, cx);
                    }
                }
            }))
            .child(
                // 外层容器，用于测量实际可用宽度
                div()
                    .id("tab-bar-outer")
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .w_full()
                    // 使用 canvas 测量容器宽度（只在变化时更新）
                    .child({
                        let entity = cx.entity();
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, _, cx| {
                                let container_width: f32 = bounds.size.width.into();
                                entity.update(cx, |this, cx| {
                                    // 只在宽度变化超过 1px 时更新
                                    if (this.measured_container_width - container_width).abs() > 1.0
                                    {
                                        this.measured_container_width = container_width;
                                        this.update_overflow_state(cx);
                                        cx.notify();
                                    }
                                });
                            },
                        )
                        .absolute()
                        .size_full()
                    })
                    // 标签列表容器（包含所有标签和新建按钮）
                    .child(
                        div()
                            .child({
                                let entity = cx.entity();
                                canvas(
                                    |_, _, _| {},
                                    move |bounds, _, _, cx| {
                                        entity.update(cx, |this, _cx| {
                                            this.tab_list_bounds = Some(bounds);
                                        });
                                    },
                                )
                                .absolute()
                                .size_full()
                            })
                            .on_children_prepainted({
                                let entity = cx.entity();
                                move |bounds, _window, cx| {
                                    entity.update(cx, |this, _cx| {
                                        // 跳过第一个 bound（canvas 测量元素），只取 tab 和 preview 的 bounds
                                        let all_bounds: Vec<_> =
                                            bounds.into_iter().skip(1).collect();
                                        let expected_bounds = this.tabs.len()
                                            + usize::from(this.panel_drop_preview.is_some());
                                        let mut tab_bounds = if expected_bounds == 0 {
                                            Vec::new()
                                        } else {
                                            all_bounds
                                                .into_iter()
                                                .rev()
                                                .take(expected_bounds)
                                                .collect::<Vec<_>>()
                                        };
                                        tab_bounds.reverse();

                                        if let Some(preview) = this.panel_drop_preview.as_ref()
                                            && preview.insert_index < tab_bounds.len()
                                        {
                                            tab_bounds.remove(preview.insert_index);
                                        }

                                        this.tab_bounds = tab_bounds;
                                    });
                                }
                            })
                            .id("tab-list-container")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .h_full()
                            .on_drag_move::<DraggedPanel>(cx.listener(
                                |this, event, _window, cx| {
                                    this.handle_panel_drag_move(event, cx);
                                },
                            ))
                            .on_drop(cx.listener({
                                let on_tab_reorder = on_tab_reorder.clone();
                                move |this, dragged: &DraggedTab, _window, cx| {
                                    let target_index = this
                                        .reorder_preview
                                        .as_ref()
                                        .filter(|preview| {
                                            preview.dragged_tab.tab_id == dragged.tab_id
                                        })
                                        .map(|preview| preview.insert_slot);

                                    if let Some(target_index) = target_index {
                                        let did_reorder =
                                            this.apply_tab_reorder_locally(dragged, target_index);
                                        this.reset_tab_reorder_preview(cx);

                                        if let Some(ref callback) = on_tab_reorder
                                            && did_reorder
                                        {
                                            callback(dragged.clone(), target_index, cx);
                                        }
                                    } else {
                                        this.clear_tab_reorder_preview(cx);
                                    }
                                }
                            }))
                            .on_drop(cx.listener({
                                let on_panel_drop = on_panel_drop.clone();
                                move |this, dragged: &DraggedPanel, window, cx| {
                                    let insert_index = this
                                        .panel_drop_preview
                                        .as_ref()
                                        .map(|preview| preview.insert_index);

                                    this.reset_panel_drop_preview(cx);
                                    cx.stop_propagation();

                                    if let Some(ref callback) = on_panel_drop {
                                        callback(dragged.clone(), insert_index, window, cx);
                                    }
                                }
                            }))
                            .when(is_overflowing, |this| {
                                // 溢出模式：水平滚动
                                this.w_full()
                                    .overflow_x_scroll()
                                    .track_scroll(&self.scroll_handle)
                            })
                            .when(!is_overflowing, |this| {
                                // 普通模式：flex 布局，不换行
                                this.flex_1().overflow_x_hidden()
                            })
                            .children({
                                let mut children: Vec<AnyElement> = Vec::with_capacity(
                                    self.tabs.len() + usize::from(panel_drop_preview.is_some()),
                                );

                                for index in 0..=self.tabs.len() {
                                    if let Some(preview) = panel_drop_preview.as_ref()
                                        && preview.insert_index == index
                                    {
                                        children.push(
                                            div()
                                                .id(format!("panel-drop-preview-{index}"))
                                                .h(px(30.0))
                                                .px(px(12.0))
                                                .flex()
                                                .flex_shrink_0()
                                                .items_center()
                                                .gap(px(6.0))
                                                .rounded(px(8.0))
                                                .border_1()
                                                .border_color(theme.border_strong.opacity(0.55))
                                                .bg(theme.tab_hover_background.opacity(0.6))
                                                .opacity(0.75)
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(theme.text_muted)
                                                        .child(preview.tab_label.clone()),
                                                )
                                                .into_any_element(),
                                        );
                                    }

                                    let Some(tab) = self.tabs.get(index) else {
                                        continue;
                                    };

                                    let tab_id = tab.id.clone();
                                    let is_active = self.active_tab_id.as_ref() == Some(&tab.id);
                                    let label = tab.label.clone();
                                    let icon = tab.icon;
                                    let is_modified = tab.is_modified;
                                    let on_panel_drop_for_tab = on_panel_drop.clone();
                                    let entity = cx.entity();
                                    let start_offset = self.tab_offset_for_preview(
                                        self.previous_reorder_preview.as_ref(),
                                        index,
                                    );
                                    let end_offset = self.tab_offset_for_preview(
                                        self.reorder_preview.as_ref(),
                                        index,
                                    );
                                    let start_opacity = self.tab_opacity_for_preview(
                                        self.previous_reorder_preview.as_ref(),
                                        index,
                                    );
                                    let end_opacity = self.tab_opacity_for_preview(
                                        self.reorder_preview.as_ref(),
                                        index,
                                    );

                                    let tab_inner = div()
                                        .id(tab_id.clone())
                                        .h(px(30.0))
                                        .px(px(12.0))
                                        .flex()
                                        .flex_shrink_0()
                                        .items_center()
                                        .gap(px(6.0))
                                        .rounded(px(8.0))
                                        .border_1()
                                        .border_color(if is_active {
                                            theme.border
                                        } else {
                                            theme.transparent()
                                        })
                                        .bg(if is_active {
                                            theme.tab_active_background
                                        } else {
                                            theme.tab_inactive_background
                                        })
                                        .hover(move |style| {
                                            if !is_active {
                                                style.bg(theme.tab_hover_background)
                                            } else {
                                                style
                                            }
                                        })
                                        .on_drag(
                                            DraggedTab {
                                                tab_id: tab_id.clone(),
                                                from_index: index,
                                            },
                                            {
                                                let label = label.clone();
                                                move |_drag_data, position, window, cx| {
                                                    cx.stop_propagation();
                                                    let preview = DragPreview::new(
                                                        label.clone(),
                                                        position,
                                                        window,
                                                        cx,
                                                    );
                                                    cx.new(move |_cx| preview)
                                                }
                                            },
                                        )
                                        .on_drag_move::<DraggedTab>(cx.listener(
                                            |this, event, _window, cx| {
                                                this.handle_tab_drag_move(event, cx);
                                            },
                                        ))
                                        .drag_over::<DraggedTab>({
                                            let entity = entity.clone();
                                            let tab_id = tab_id.clone();
                                            move |tab, dragged: &DraggedTab, _window, cx| {
                                                let dragged = dragged.clone();
                                                let entity = entity.clone();
                                                let tab_id = tab_id.clone();
                                                cx.defer(move |cx| {
                                                    entity.update(cx, |this, cx| {
                                                        if dragged.tab_id != tab_id {
                                                            this.set_tab_reorder_preview(
                                                                &dragged, index, cx,
                                                            );
                                                        } else {
                                                            this.clear_tab_reorder_preview(cx);
                                                        }
                                                    });
                                                });
                                                tab
                                            }
                                        })
                                        .drag_over::<DraggedPanel>({
                                            let entity = entity.clone();
                                            move |tab, dragged: &DraggedPanel, _window, cx| {
                                                let entity = entity.clone();
                                                let tab_label = dragged.tab_label.clone();
                                                cx.defer(move |cx| {
                                                    entity.update(cx, |this, cx| {
                                                        this.set_panel_drop_preview(
                                                            index, tab_label, cx,
                                                        );
                                                    });
                                                });
                                                tab
                                            }
                                        })
                                        .on_drop(cx.listener({
                                            let on_tab_reorder = on_tab_reorder.clone();
                                            let tab_id = tab_id.clone();
                                            move |this, dragged: &DraggedTab, _window, cx| {
                                                cx.stop_propagation();

                                                if dragged.tab_id != tab_id {
                                                    if let Some(to_index) = this
                                                        .tabs
                                                        .iter()
                                                        .position(|t| t.id == tab_id)
                                                    {
                                                        let did_reorder = this
                                                            .apply_tab_reorder_locally(
                                                                dragged, to_index,
                                                            );
                                                        this.reset_tab_reorder_preview(cx);

                                                        if let Some(ref callback) = on_tab_reorder
                                                            && did_reorder
                                                        {
                                                            callback(dragged.clone(), to_index, cx);
                                                        }
                                                    }
                                                } else {
                                                    this.clear_tab_reorder_preview(cx);
                                                }
                                            }
                                        }))
                                        .on_drop(cx.listener({
                                            let on_panel_drop_for_tab =
                                                on_panel_drop_for_tab.clone();
                                            move |this, dragged: &DraggedPanel, window, cx| {
                                                cx.stop_propagation();
                                                this.reset_panel_drop_preview(cx);
                                                if let Some(ref callback) = on_panel_drop_for_tab {
                                                    callback(
                                                        dragged.clone(),
                                                        Some(index),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            }
                                        }))
                                        .when_some(icon, |this, icon| {
                                            this.child(
                                                svg()
                                                    .size(px(16.0))
                                                    .text_color(if is_active {
                                                        theme.text
                                                    } else {
                                                        theme.text_muted
                                                    })
                                                    .path(icon.path()),
                                            )
                                        })
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(if is_active {
                                                    gpui::FontWeight::SEMIBOLD
                                                } else {
                                                    gpui::FontWeight::NORMAL
                                                })
                                                .text_color(if is_active {
                                                    theme.text
                                                } else {
                                                    theme.text_muted
                                                })
                                                .child(label),
                                        )
                                        .child(
                                            div()
                                                .w(px(18.0))
                                                .h(px(18.0))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded_full()
                                                .hover(move |style| {
                                                    style.bg(theme.error.opacity(0.15))
                                                })
                                                .cursor_pointer()
                                                .text_color(theme.icon_muted)
                                                .child(if is_modified { "●" } else { "×" })
                                                .on_mouse_down(
                                                    gpui::MouseButton::Left,
                                                    cx.listener({
                                                        let on_tab_close = on_tab_close.clone();
                                                        let tab_id = tab_id.clone();
                                                        move |_this, _, window, cx| {
                                                            if let Some(ref callback) = on_tab_close
                                                            {
                                                                callback(
                                                                    tab_id.clone(),
                                                                    window,
                                                                    cx,
                                                                );
                                                            }
                                                        }
                                                    }),
                                                ),
                                        )
                                        .on_click(cx.listener({
                                            let on_tab_click = on_tab_click.clone();
                                            let tab_id = tab_id.clone();
                                            move |_this, _event: &gpui::ClickEvent, window, cx| {
                                                if let Some(ref callback) = on_tab_click {
                                                    callback(tab_id.clone(), window, cx);
                                                }
                                            }
                                        }))
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        });

                                    let needs_animation =
                                        start_offset != end_offset || start_opacity != end_opacity;

                                    let tab_element = if needs_animation {
                                        let animation_id =
                                            format!("tab-reorder-{}-{}", tab_id, animation_epoch);

                                        div()
                                            .child(tab_inner)
                                            .flex_shrink_0()
                                            .with_animation(
                                                animation_id,
                                                Animation::new(Duration::from_millis(
                                                    TAB_REORDER_ANIMATION_DURATION_MS,
                                                ))
                                                .with_easing(ease_out_quint()),
                                                move |this, delta| {
                                                    let offset = start_offset
                                                        + (end_offset - start_offset) * delta;
                                                    let opacity = start_opacity
                                                        + (end_opacity - start_opacity) * delta;
                                                    this.left(px(offset)).opacity(opacity)
                                                },
                                            )
                                            .into_any_element()
                                    } else {
                                        tab_inner.into_any_element()
                                    };

                                    children.push(tab_element);
                                }

                                children
                            }),
                    )
                    // 新建标签按钮 - 放在滚动区域外面，始终可见
                    // 使用 ml_1 保持与 Tab 标签页之间的间距一致（gap_1 = 4px）
                    .child(
                        div()
                            .id("new-tab-btn")
                            .w(px(32.0))
                            .h(px(32.0))
                            .flex()
                            .flex_shrink_0()
                            .ml_1()
                            .mr_3()
                            .items_center()
                            .justify_center()
                            .rounded(px(999.0))
                            .cursor_pointer()
                            // .bg(theme.tab_inactive_background)
                            .hover(move |style| style.bg(theme.tab_hover_background))
                            .child(
                                svg()
                                    .size(px(16.0))
                                    .text_color(theme.icon_muted)
                                    .path(Icon::Plus.path()),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |_, _, window, cx| {
                                    if let Some(ref callback) = on_new_tab {
                                        callback(window, cx);
                                    }
                                }),
                            ),
                    ),
            )
    }
}
