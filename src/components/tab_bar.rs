use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, App, Bounds, Context, ElementId, Entity, FocusHandle, Focusable,
    IntoElement, Pixels, Point, Render, ScrollHandle, SharedString, Window, canvas, div,
    ease_out_quint, px, rgb,
};

use crate::assets::Icon;
use crate::workspace::panel::DraggedPanel;
use gpui::svg;

/// 特殊标签页ID常量
pub const HOSTS_HUB_TAB_ID: &str = "__hostshub__";

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
}

const TAB_REORDER_ANIMATION_DURATION_MS: u64 = 155;
const TAB_REORDER_GAP_PX: f32 = 4.0;
const TAB_REORDER_FALLBACK_WIDTH_PX: f32 = 160.0;
const TAB_REORDER_DRAGGED_TAB_OPACITY: f32 = 0.22;

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
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let preview_width = px(152.0);
        let preview_height = px(30.0);

        div()
            // 将预览中心对齐到鼠标位置
            .pl(self.drag_offset.x - preview_width / 2.0)
            .pt(self.drag_offset.y - preview_height / 2.0)
            .child(
                div()
                    .w(preview_width)
                    .h(preview_height)
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .rounded(px(7.0))
                    .bg(rgb(0x2d2d44))
                    .border_1()
                    .border_color(rgb(0x4a4a68))
                    .opacity(0.96)
                    .shadow_md()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xffffff))
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
    pub is_active: bool,
    pub is_modified: bool,
}

impl TabItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            is_active: false,
            is_modified: false,
        }
    }

    pub fn with_modified(mut self, modified: bool) -> Self {
        self.is_modified = modified;
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
    on_settingshub_click: Option<Rc<dyn Fn(&mut Window, &mut App) + Send + Sync>>,
    on_tab_reorder: Option<Rc<dyn Fn(DraggedTab, usize, &mut App) + Send + Sync>>,
    on_tab_split: Option<Rc<dyn Fn(DraggedTab, EdgeZone, &mut App) + Send + Sync>>,
    on_tab_merge: Option<Rc<dyn Fn(DraggedTab, SharedString, &mut App) + Send + Sync>>,
    /// Panel 拖拽到 TabBar 的回调
    on_panel_drop: Option<Rc<dyn Fn(DraggedPanel, Option<usize>, &mut App) + Send + Sync>>,
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
    /// 上一次 Panel 拖入 TabBar 的插入预览
    previous_panel_drop_preview: Option<PanelDropPreview>,
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
            on_settingshub_click: None,
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
            previous_panel_drop_preview: None,
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
            on_settingshub_click: None,
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
            previous_panel_drop_preview: None,
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

    pub fn on_settingshub_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_settingshub_click = Some(Rc::new(callback));
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
        F: Fn(DraggedPanel, Option<usize>, &mut App) + Send + Sync + 'static,
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

    /// 设置 HostsHub 标签为激活状态
    pub fn set_hostshub_active(&mut self, cx: &mut Context<Self>) {
        let settings_id = SharedString::from(HOSTS_HUB_TAB_ID);
        if self.active_tab_id.as_ref() != Some(&settings_id) {
            self.active_tab_id = Some(settings_id);
            cx.notify();
        }
    }

    pub fn set_tabs(&mut self, tabs: Vec<TabItem>, cx: &mut Context<Self>) {
        self.tabs = tabs;
        if self.active_tab_id.is_none() && !self.tabs.is_empty() {
            self.active_tab_id = Some(self.tabs[0].id.clone());
        }
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
    fn update_overflow_state(&mut self, cx: &mut Context<Self>) {
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

        let is_inside = position.x >= bounds.left()
            && position.x <= bounds.right()
            && position.y >= bounds.top()
            && position.y <= bounds.bottom();

        if !is_inside {
            self.clear_tab_reorder_preview(cx);
        }
    }

    fn handle_panel_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedPanel>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        let Some(bounds) = self.tab_list_bounds else {
            return;
        };

        let is_inside = position.x >= bounds.left()
            && position.x <= bounds.right()
            && position.y >= bounds.top()
            && position.y <= bounds.bottom();

        if !is_inside {
            self.clear_panel_drop_preview(cx);
            return;
        }

        let insert_index = self.tab_insert_slot_for_position(position);
        self.set_panel_drop_preview(insert_index, cx);
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
        self.previous_panel_drop_preview = None;
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

    fn set_panel_drop_preview(&mut self, insert_index: usize, cx: &mut Context<Self>) {
        let next_preview = PanelDropPreview {
            insert_index: insert_index.min(self.tabs.len()),
        };

        if self.panel_drop_preview.as_ref() == Some(&next_preview) {
            return;
        }

        self.reorder_preview = None;
        self.previous_reorder_preview = None;
        self.previous_panel_drop_preview = self.panel_drop_preview.clone();
        self.panel_drop_preview = Some(next_preview);
        self.reorder_animation_epoch += 1;
        cx.notify();
    }

    fn clear_panel_drop_preview(&mut self, cx: &mut Context<Self>) {
        if self.panel_drop_preview.is_none() {
            return;
        }

        self.previous_panel_drop_preview = self.panel_drop_preview.take();
        self.reorder_animation_epoch += 1;
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
        if self.panel_drop_preview.is_none() && self.previous_panel_drop_preview.is_none() {
            return;
        }

        self.panel_drop_preview = None;
        self.previous_panel_drop_preview = None;
        self.reorder_animation_epoch += 1;
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

    fn panel_drop_shift_width(&self, preview: &PanelDropPreview) -> f32 {
        if self.tabs.is_empty() {
            return TAB_REORDER_FALLBACK_WIDTH_PX;
        }

        self.tab_bounds
            .get(
                preview
                    .insert_index
                    .min(self.tab_bounds.len().saturating_sub(1)),
            )
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

    fn tab_offset_for_panel_drop_preview(
        &self,
        preview: Option<&PanelDropPreview>,
        index: usize,
    ) -> f32 {
        let Some(preview) = preview else {
            return 0.0;
        };

        if preview.insert_index >= self.tabs.len() {
            return 0.0;
        }

        let shift = self.panel_drop_shift_width(preview);
        if index >= preview.insert_index {
            shift
        } else {
            0.0
        }
    }

    fn animated_tab_offset(
        &self,
        tab_preview: Option<&TabReorderPreview>,
        panel_preview: Option<&PanelDropPreview>,
        index: usize,
    ) -> f32 {
        if tab_preview.is_some() {
            self.tab_offset_for_preview(tab_preview, index)
        } else {
            self.tab_offset_for_panel_drop_preview(panel_preview, index)
        }
    }

    fn animated_tab_opacity(&self, tab_preview: Option<&TabReorderPreview>, index: usize) -> f32 {
        self.tab_opacity_for_preview(tab_preview, index)
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
        let on_tab_click = self.on_tab_click.clone();
        let on_tab_close = self.on_tab_close.clone();
        let on_new_tab = self.on_new_tab.clone();
        let on_settingshub_click = self.on_settingshub_click.clone();
        let on_tab_reorder = self.on_tab_reorder.clone();
        let _on_tab_merge = self.on_tab_merge.clone();
        let on_panel_drop = self.on_panel_drop.clone();
        let animation_epoch = self.reorder_animation_epoch;
        let is_settingshub_active =
            self.active_tab_id.as_ref().map(|s| s.as_ref()) == Some(HOSTS_HUB_TAB_ID);

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
                move |this, dragged: &DraggedPanel, _window, cx| {
                    let insert_index = this.panel_drop_preview.as_ref().map(|preview| preview.insert_index);
                    this.reset_panel_drop_preview(cx);
                    if let Some(ref callback) = on_panel_drop {
                        callback(dragged.clone(), insert_index, cx);
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
                                entity.update(cx, |this, _cx| {
                                    // 只在宽度变化超过 1px 时更新
                                    if (this.measured_container_width - container_width).abs() > 1.0
                                    {
                                        this.measured_container_width = container_width;
                                    }
                                });
                            },
                        )
                        .absolute()
                        .size_full()
                    })
                    // SettingsHub 按钮 - 固定在左侧，不参与滚动
                    .child(
                        div()
                            .id("hostshub-btn")
                            .h(px(28.0))
                            .px(px(12.0))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .gap(px(6.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .bg(if is_settingshub_active {
                                rgb(0x2d2d44)
                            } else {
                                rgb(0x1e1e2e)
                            })
                            .hover(|style| {
                                if !is_settingshub_active {
                                    style.bg(rgb(0x252538))
                                } else {
                                    style
                                }
                            })
                            .child(
                                svg()
                                    .size(px(14.0))
                                    .text_color(if is_settingshub_active {
                                        rgb(0xffffff)
                                    } else {
                                        rgb(0x8888aa)
                                    })
                                    .path(Icon::SettingsHub.path()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if is_settingshub_active {
                                        rgb(0xffffff)
                                    } else {
                                        rgb(0x8888aa)
                                    })
                                    .child("HostsHub"),
                            )
                            .on_click(cx.listener(move |_, _, window, cx| {
                                if let Some(ref callback) = on_settingshub_click {
                                    callback(window, cx);
                                }
                            }))
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            }),
                    )
                    // 分割线 - 在 SettingsHub 和 Tab 列表之间
                    .child(div().h(px(20.0)).w(px(1.0)).bg(rgb(0x2a2a3c)).mx(px(4.0)))
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
                                        let tab_count = this.tabs.len();
                                        this.tab_bounds = if tab_count == 0 {
                                            Vec::new()
                                        } else {
                                            bounds.into_iter().rev().take(tab_count).collect::<Vec<_>>()
                                        };
                                        this.tab_bounds.reverse();
                                    });
                                }
                            })
                            .id("tab-list-container")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .h_full()
                            .on_drag_move::<DraggedPanel>(cx.listener(|this, event, _window, cx| {
                                this.handle_panel_drag_move(event, cx);
                            }))
                            .on_drop(cx.listener({
                                let on_tab_reorder = on_tab_reorder.clone();
                                move |this, dragged: &DraggedTab, _window, cx| {
                                    let target_index = this
                                        .reorder_preview
                                        .as_ref()
                                        .filter(|preview| preview.dragged_tab.tab_id == dragged.tab_id)
                                        .map(|preview| preview.insert_slot);

                                    if let Some(target_index) = target_index {
                                        this.apply_tab_reorder_locally(dragged, target_index);
                                        this.reset_tab_reorder_preview(cx);

                                        if let Some(ref callback) = on_tab_reorder
                                            && target_index != dragged.from_index
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
                                move |this, dragged: &DraggedPanel, _window, cx| {
                                    let insert_index = this
                                        .panel_drop_preview
                                        .as_ref()
                                        .map(|preview| preview.insert_index);

                                    this.reset_panel_drop_preview(cx);
                                    cx.stop_propagation();

                                    if let Some(ref callback) = on_panel_drop {
                                        callback(dragged.clone(), insert_index, cx);
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
                            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                                let tab_id = tab.id.clone();
                                let is_active = self.active_tab_id.as_ref() == Some(&tab.id);
                                let label = tab.label.clone();
                                let is_modified = tab.is_modified;
                                let on_panel_drop_for_tab = on_panel_drop.clone();
                                let entity = cx.entity();
                                let start_offset = self.animated_tab_offset(
                                    self.previous_reorder_preview.as_ref(),
                                    self.previous_panel_drop_preview.as_ref(),
                                    index,
                                );
                                let end_offset = self.animated_tab_offset(
                                    self.reorder_preview.as_ref(),
                                    self.panel_drop_preview.as_ref(),
                                    index,
                                );
                                let start_opacity = self
                                    .animated_tab_opacity(self.previous_reorder_preview.as_ref(), index);
                                let end_opacity = self
                                    .animated_tab_opacity(self.reorder_preview.as_ref(), index);
                                let animation_id =
                                    format!("tab-reorder-{}-{}", tab_id, animation_epoch);

                                div()
                                    .child(
                                        div()
                                            .id(tab_id.clone())
                                            .h(px(28.0))
                                            .px(px(12.0))
                                            .flex()
                                            .flex_shrink_0()
                                            .items_center()
                                            .gap(px(6.0))
                                            .rounded(px(6.0))
                                            .bg(if is_active {
                                                rgb(0x2d2d44)
                                            } else {
                                                rgb(0x1e1e2e)
                                            })
                                            .hover(|style| {
                                                if !is_active {
                                                    style.bg(rgb(0x252538))
                                                } else {
                                                    style
                                                }
                                            })
                                            .cursor_pointer()
                                            // 拖拽功能
                                            .on_drag(
                                                DraggedTab {
                                                    tab_id: tab_id.clone(),
                                                    from_index: index,
                                                },
                                                {
                                                    let label = label.clone();
                                                    move |_drag_data, position, _, cx| {
                                                        // 阻止事件冒泡，防止触发窗口拖拽
                                                        cx.stop_propagation();
                                                        // 返回拖拽时的视觉反馈 - 创建一个新的视图实体
                                                        cx.new(|_cx| DragPreview {
                                                            tab_label: label.clone(),
                                                            drag_offset: position,
                                                        })
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
                                            // 拖拽悬停效果 - DraggedPanel（显示放置指示器）
                                            .drag_over::<DraggedPanel>({
                                                let entity = entity.clone();
                                                move |tab, _dragged: &DraggedPanel, _window, cx| {
                                                    let entity = entity.clone();
                                                    cx.defer(move |cx| {
                                                        entity.update(cx, |this, cx| {
                                                            this.set_panel_drop_preview(index, cx);
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
                                                        if let Some(ref callback) = on_tab_reorder {
                                                            if let Some(to_index) = this
                                                                .tabs
                                                                .iter()
                                                                .position(|t| t.id == tab_id)
                                                            {
                                                                this.apply_tab_reorder_locally(
                                                                    dragged,
                                                                    to_index,
                                                                );
                                                                this.reset_tab_reorder_preview(cx);
                                                                callback(dragged.clone(), to_index, cx);
                                                            }
                                                        }
                                                    } else {
                                                        this.clear_tab_reorder_preview(cx);
                                                    }
                                                }
                                            }))
                                            // 放置处理 - DraggedPanel，插入到当前 tab 位置
                                            .on_drop(cx.listener({
                                                let on_panel_drop_for_tab = on_panel_drop_for_tab.clone();
                                                move |this, dragged: &DraggedPanel, _window, cx| {
                                                    cx.stop_propagation(); // 阻止事件冒泡到 TabBar 容器
                                                    this.reset_panel_drop_preview(cx);
                                                    if let Some(ref callback) = on_panel_drop_for_tab {
                                                        callback(dragged.clone(), Some(index), cx);
                                                    }
                                                }
                                            }))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(if is_active {
                                                        rgb(0xffffff)
                                                    } else {
                                                        rgb(0x8888aa)
                                                    })
                                                    .child(label),
                                            )
                                            .child(
                                                div()
                                                    .w(px(14.0))
                                                    .h(px(14.0))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded_full()
                                                    .hover(|style| style.bg(rgb(0x2d2d44)))
                                                    .cursor_pointer()
                                                    .text_color(rgb(0x666688))
                                                    .text_xs()
                                                    .child(if is_modified { "●" } else { "×" })
                                                    .on_mouse_down(
                                                        gpui::MouseButton::Left,
                                                        cx.listener({
                                                            let on_tab_close = on_tab_close.clone();
                                                            let tab_id = tab_id.clone();
                                                            move |_this, _, window, cx| {
                                                                if let Some(ref callback) = on_tab_close {
                                                                    callback(tab_id.clone(), window, cx);
                                                                }
                                                            }
                                                        }),
                                                    ),
                                            )
                                            .on_click(cx.listener({
                                                let on_tab_click = on_tab_click.clone();
                                                let tab_id = tab_id.clone();
                                                move |_this, _event: &gpui::ClickEvent, window, cx| {
                                                    // 只有在没有发生拖拽的情况下才触发点击
                                                    if let Some(ref callback) = on_tab_click {
                                                        callback(tab_id.clone(), window, cx);
                                                    }
                                                }
                                            }))
                                            // 阻止事件冒泡到 TitleBar，防止在 macOS 上触发窗口拖拽
                                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                                cx.stop_propagation();
                                            }),
                                    )
                                    .flex_shrink_0()
                                    .with_animation(
                                        animation_id,
                                        Animation::new(Duration::from_millis(
                                            TAB_REORDER_ANIMATION_DURATION_MS,
                                        ))
                                        .with_easing(ease_out_quint()),
                                        move |this, delta| {
                                            let offset = start_offset + (end_offset - start_offset) * delta;
                                            let opacity =
                                                start_opacity + (end_opacity - start_opacity) * delta;
                                            this.left(px(offset)).opacity(opacity)
                                        },
                                    )
                            })),
                    )
                    // 新建标签按钮 - 放在滚动区域外面，始终可见
                    // 使用 ml_1 保持与 Tab 标签页之间的间距一致（gap_1 = 4px）
                    .child(
                        div()
                            .id("new-tab-btn")
                            .w(px(28.0))
                            .h(px(28.0))
                            .flex()
                            .flex_shrink_0()
                            .ml_1()
                            .mr_3()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .bg(rgb(0x1e1e2e))
                            .hover(|style| style.bg(rgb(0x2d2d44)))
                            .child(
                                svg()
                                    .size(px(14.0))
                                    .text_color(rgb(0x666688))
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
