use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Action, Animation, AnimationExt as _, AnyElement, App, Bounds, Context, Div, ElementId, Entity,
    FocusHandle, IntoElement, MouseButton, Pixels, Point, Render, ScrollHandle,
    SharedString, Stateful, TextRun, TextStyle, Transformation, Window, actions, div, ease_in_out,
    ease_out_quint, percentage, px, svg,
};

use crate::assets::Icon;
use crate::components::menu::{ContextMenuExt, PopupMenu};
use crate::theme::{ActiveTheme, UiTheme};
use crate::workspace::panel::DraggedPanel;

actions!(
    tab_context_menu,
    [CopyTabAddress, DuplicateTab, ReconnectTab]
);

/// 以 tab id 为参数的回调（点击、关闭、复制地址、复制标签页、重连等）。
type TabCallback = Rc<dyn Fn(SharedString, &mut Window, &mut App) + Send + Sync>;
/// 可选 [`TabCallback`]。
type OptionalTabCallback = Option<TabCallback>;
/// 无参数回调（如新建标签页）。
type NewTabCallback = Rc<dyn Fn(&mut Window, &mut App) + Send + Sync>;
/// Tab 重排回调：参数为被拖拽的 tab 与插入位（before 语义）。
type TabReorderCallback = Rc<dyn Fn(DraggedTab, usize, &mut App) + Send + Sync>;
/// Panel 拖放到 TabBar 的回调：参数为被拖放的 panel 与可选的插入位。
type PanelDropCallback =
    Rc<dyn Fn(DraggedPanel, Option<usize>, &mut Window, &mut App) + Send + Sync>;
/// 右键菜单构建回调：接收一个空 `PopupMenu`，返回填充好菜单项的 `PopupMenu`。
type ContextMenuCallback =
    Rc<dyn Fn(SharedString, PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// 拖拽的 Tab 数据
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedTab {
    pub tab_id: SharedString,
    pub from_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabReorderPreview {
    dragged_tab: DraggedTab,
    /// 插入位（before 语义）：拖拽结果为「插入到重排前的第 insert_slot 个位置」，
    /// 取值范围 [0, tabs.len()]（等于 len 表示追加到末尾）。
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
const TAB_MAX_WIDTH_PX: f32 = 180.0;
const TAB_HEIGHT_PX: f32 = 30.0;
const TAB_PADDING_X_PX: f32 = 12.0;
const TAB_RADIUS_PX: f32 = 8.0;
const ICON_SIZE_PX: f32 = 16.0;
const FILE_ICON_SIZE_PX: f32 = 22.0;
const NEW_TAB_BUTTON_SIZE_PX: f32 = 32.0;
const TAB_TOOLTIP_MAX_WIDTH_PX: f32 = 480.0;

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
    pub is_modified: bool,
    pub is_remote: bool,
    pub is_loading: bool,
}

impl TabItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            is_modified: false,
            is_remote: false,
            is_loading: false,
        }
    }

    pub fn with_loading(mut self, loading: bool) -> Self {
        self.is_loading = loading;
        self
    }

    pub fn with_modified(mut self, modified: bool) -> Self {
        self.is_modified = modified;
        self
    }

    pub fn with_icon(mut self, icon: Option<Icon>) -> Self {
        self.icon = icon;
        self
    }

    pub fn with_is_remote(mut self, is_remote: bool) -> Self {
        self.is_remote = is_remote;
        self
    }
}

/// 纯重排核心：把 `dragged_id` 对应标签移动到「重排前列表的第 `insert_slot` 个插入位」。
/// 返回拖拽标签的重排前索引；无实际移动或标签不存在时返回 None。
fn apply_reorder(tabs: &mut Vec<TabItem>, dragged_id: &str, insert_slot: usize) -> Option<usize> {
    let from_index = tabs.iter().position(|t| t.id == dragged_id)?;
    let final_index = TabBar::final_index_for_slot(from_index, insert_slot);

    if from_index == final_index || final_index >= tabs.len() {
        return None;
    }

    let tab = tabs.remove(from_index);
    tabs.insert(final_index, tab);
    Some(from_index)
}

/// 中点判定（纯函数）：`content_x` 位于哪个 tab 中点左侧，就插入到哪个索引之前。
fn insert_slot_for_content_x(content_x: Pixels, tab_bounds: &[Bounds<Pixels>]) -> usize {
    for (index, bounds) in tab_bounds.iter().enumerate() {
        let midpoint = bounds.left() + bounds.size.width / 2.0;
        if content_x < midpoint {
            return index;
        }
    }

    tab_bounds.len()
}

/// 纯函数：重排预览期间第 `index` 个标签的水平位移。
/// 拖拽标签右移时，[from+1, to] 区间的标签左移让位；左移时 [to, from-1] 区间右移。
fn reorder_offset(from_index: usize, to_index: usize, index: usize, shift_width: f32) -> f32 {
    if to_index > from_index {
        if index > from_index && index <= to_index {
            -shift_width
        } else {
            0.0
        }
    } else if to_index < from_index {
        if index >= to_index && index < from_index {
            shift_width
        } else {
            0.0
        }
    } else {
        0.0
    }
}

/// 标签栏组件
pub struct TabBar {
    id: ElementId,
    pub tabs: Vec<TabItem>,
    pub active_tab_id: Option<SharedString>,
    transparent_bg: bool,
    compact: bool,
    on_tab_click: OptionalTabCallback,
    on_tab_close: OptionalTabCallback,
    on_new_tab: Option<NewTabCallback>,
    /// 重排回调：`insert_slot` 为重排前列表中的插入位（before 语义）。
    /// 回调触发时 TabBar 自身的 tabs 已完成重排，消费方只需同步自己的平行结构
    /// （如 editor 的 open_files），不要再对 tab_bar.tabs 重复应用。
    on_tab_reorder: Option<TabReorderCallback>,
    /// Panel 拖拽到 TabBar 的回调
    on_panel_drop: Option<PanelDropCallback>,
    focus_handle: FocusHandle,
    /// 滚动句柄，用于控制水平滚动；子元素 bounds（内容坐标）与视口
    /// （窗口坐标）均从此句柄读取
    scroll_handle: ScrollHandle,
    /// 当前 Tab 重排预览
    reorder_preview: Option<TabReorderPreview>,
    /// 上一次 Tab 重排预览，用于做补间动画
    previous_reorder_preview: Option<TabReorderPreview>,
    /// 重排动画序号
    reorder_animation_epoch: usize,
    /// 当前 Panel 拖入 TabBar 的插入预览
    panel_drop_preview: Option<PanelDropPreview>,
    /// 当前悬停的标签 ID
    hovered_tab_id: Option<SharedString>,
    /// 右键菜单的目标标签 ID
    context_menu_tab_id: Option<SharedString>,
    /// 拷贝地址回调
    on_tab_copy_address: OptionalTabCallback,
    /// 复制标签页回调
    on_tab_duplicate: OptionalTabCallback,
    /// 重新连接回调
    on_tab_reconnect: OptionalTabCallback,
    /// 右键菜单构建回调（由调用方设置，tab_id 为右键点击的标签 ID）
    on_context_menu: Option<ContextMenuCallback>,
}

impl TabBar {
    fn build(id: ElementId, focus_handle: FocusHandle, tabs: Vec<TabItem>) -> Self {
        let active_tab_id = tabs.first().map(|tab| tab.id.clone());
        Self {
            id,
            tabs,
            active_tab_id,
            transparent_bg: false,
            compact: false,
            on_tab_click: None,
            on_tab_close: None,
            on_new_tab: None,
            on_tab_reorder: None,
            on_panel_drop: None,
            hovered_tab_id: None,
            focus_handle,
            scroll_handle: ScrollHandle::new(),
            reorder_preview: None,
            previous_reorder_preview: None,
            reorder_animation_epoch: 0,
            panel_drop_preview: None,
            context_menu_tab_id: None,
            on_tab_copy_address: None,
            on_tab_duplicate: None,
            on_tab_reconnect: None,
            on_context_menu: None,
        }
    }

    pub fn new(id: impl Into<ElementId>, cx: &mut App) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self::build(id.into(), focus_handle, Vec::new()))
    }

    pub fn new_with_tabs(
        id: impl Into<ElementId>,
        cx: &mut App,
        tabs: Vec<TabItem>,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        cx.new(|_cx| Self::build(id.into(), focus_handle, tabs))
    }

    /// 编辑器悬浮面板使用透明背景以透出玻璃材质
    pub fn set_transparent_bg(&mut self, transparent: bool, cx: &mut Context<Self>) {
        if self.transparent_bg != transparent {
            self.transparent_bg = transparent;
            cx.notify();
        }
    }

    /// 编辑器紧凑模式：降低标签高度并微调内边距，使文字与终端标签对齐
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact != compact {
            self.compact = compact;
            cx.notify();
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub fn clear_tabs(&mut self, cx: &mut Context<Self>) {
        self.tabs.clear();
        self.active_tab_id = None;
        self.context_menu_tab_id = None;
        cx.notify();
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

    /// 设置重排回调。`insert_slot` 为重排前列表中的插入位（before 语义），
    /// 取值范围 [0, tabs.len()]；TabBar 自身已同步完成重排。
    pub fn on_tab_reorder<F>(&mut self, callback: F)
    where
        F: Fn(DraggedTab, usize, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_reorder = Some(Rc::new(callback));
    }

    /// 设置 Panel 拖拽到 TabBar 的回调
    pub fn on_panel_drop<F>(&mut self, callback: F)
    where
        F: Fn(DraggedPanel, Option<usize>, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_panel_drop = Some(Rc::new(callback));
    }

    /// 拷贝地址回调
    pub fn on_tab_copy_address<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_copy_address = Some(Rc::new(callback));
    }

    /// 复制标签页回调
    pub fn on_tab_duplicate<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_duplicate = Some(Rc::new(callback));
    }

    /// 重新连接回调
    pub fn on_tab_reconnect<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, &mut Window, &mut App) + Send + Sync + 'static,
    {
        self.on_tab_reconnect = Some(Rc::new(callback));
    }

    /// 右键菜单构建回调（由调用方设置）
    pub fn on_context_menu<F>(&mut self, callback: F)
    where
        F: Fn(SharedString, PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    {
        self.on_context_menu = Some(Rc::new(callback));
    }

    /// 获取右键菜单的目标标签 ID
    pub fn context_menu_tab_id(&self) -> Option<SharedString> {
        self.context_menu_tab_id.clone()
    }

    pub fn add_tab(&mut self, tab: TabItem, cx: &mut Context<Self>) {
        let is_first = self.tabs.is_empty();
        self.tabs.push(tab);
        cx.notify();
        // 滚动到最新添加的标签（最右边）
        // 首个标签不要调用 scroll_to_item(0)，避免 ScrollHandle 未初始化时触发默认偏移
        if !is_first {
            let last_index = self.tabs.len().saturating_sub(1);
            self.scroll_handle.scroll_to_item(last_index);
        }
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
        }

        // 右键菜单目标已不存在，避免后续 action 作用到过期 id
        if self.context_menu_tab_id.as_deref() == Some(id) {
            self.context_menu_tab_id = None;
        }
        cx.notify();
    }

    pub fn set_active_tab(&mut self, id: impl Into<SharedString>, cx: &mut Context<Self>) {
        let new_id: SharedString = id.into();
        let changed = self.active_tab_id.as_ref() != Some(&new_id);
        if changed {
            self.active_tab_id = Some(new_id);
            // 溢出时把新激活的标签滚动到可见区域（最小滚动策略）
            if let Some(index) = self
                .active_tab_id
                .as_ref()
                .and_then(|id| self.tabs.iter().position(|t| t.id == *id))
            {
                self.scroll_handle.scroll_to_item(index);
            }
            cx.notify();
        }
    }

    pub fn set_tabs(&mut self, tabs: Vec<TabItem>, cx: &mut Context<Self>) {
        self.tabs = tabs;
        if self.active_tab_id.is_none() && !self.tabs.is_empty() {
            self.active_tab_id = Some(self.tabs[0].id.clone());
        }
        self.context_menu_tab_id = None;
        cx.notify();
    }

    /// 原地修改单个标签，避免重建整个 tabs Vec
    pub fn modify_tab(
        &mut self,
        tab_id: &str,
        f: impl FnOnce(&mut TabItem),
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            f(tab);
            cx.notify();
        }
    }

    pub fn active_tab_id(&self) -> Option<&SharedString> {
        self.active_tab_id.as_ref()
    }

    fn handle_tab_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        if !self.tab_list_viewport().contains(&position) {
            self.clear_tab_reorder_preview(cx);
            return;
        }

        let dragged = event.drag(cx).clone();
        // 拖拽源可能来自另一个 TabBar（同窗口内终端栏与编辑器栏并存），
        // 只对本栏存在的标签生成重排预览
        if !self.tabs.iter().any(|t| t.id == dragged.tab_id) {
            self.clear_tab_reorder_preview(cx);
            return;
        }

        let insert_slot = self.tab_insert_slot_for_position(position);
        self.set_tab_reorder_preview(&dragged, insert_slot, cx);
    }

    fn handle_panel_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedPanel>,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        if !self.tab_list_viewport().contains(&position) {
            self.clear_panel_drop_preview(cx);
            return;
        }

        let dragged: &DraggedPanel = event.drag(cx);
        let insert_index = self.tab_insert_slot_for_position(position);
        self.set_panel_drop_preview(insert_index, dragged.tab_label.clone(), cx);
    }

    /// 滚动容器视口（窗口坐标），拖拽命中判定用
    fn tab_list_viewport(&self) -> Bounds<Pixels> {
        self.scroll_handle.bounds()
    }

    /// 各 tab 的布局边界（内容坐标，不含滚动偏移），
    /// 直接读取 ScrollHandle 记录的子元素 bounds。
    /// 滚动容器的子元素 = 各 tab（+ 可选的 panel 插入预览，需跳过）。
    fn current_tab_bounds(&self) -> Vec<Bounds<Pixels>> {
        let preview_index = self.panel_drop_preview.as_ref().map(|p| p.insert_index);
        let child_count = self.tabs.len() + usize::from(preview_index.is_some());
        let mut bounds = Vec::with_capacity(self.tabs.len());
        for child_index in 0..child_count {
            if Some(child_index) == preview_index {
                continue;
            }
            if let Some(tab_bounds) = self.scroll_handle.bounds_for_item(child_index) {
                bounds.push(tab_bounds);
            }
        }
        bounds
    }

    fn set_tab_reorder_preview(
        &mut self,
        dragged_tab: &DraggedTab,
        insert_slot: usize,
        cx: &mut Context<Self>,
    ) {
        let next_preview = TabReorderPreview {
            dragged_tab: dragged_tab.clone(),
            insert_slot: insert_slot.min(self.tabs.len()),
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

    /// 将「原列表插入位」换算为 remove+insert 后的最终索引。
    /// insert_slot > from 时插入位在拖拽标签之后，移除后目标元素左移一位。
    fn final_index_for_slot(from_index: usize, insert_slot: usize) -> usize {
        if insert_slot > from_index {
            insert_slot - 1
        } else {
            insert_slot
        }
    }

    /// 就地应用重排：`insert_slot` 为重排前列表中的插入位（before 语义）。
    /// 返回拖拽标签的重排前索引；无实际移动或标签不存在时返回 None。
    fn apply_tab_reorder(&mut self, dragged_tab: &DraggedTab, insert_slot: usize) -> Option<usize> {
        apply_reorder(&mut self.tabs, &dragged_tab.tab_id, insert_slot)
    }

    /// 中点判定：指针位于哪个 tab 中点左侧，就插入到哪个索引之前。
    /// 位置为窗口坐标，需换算到内容坐标后与布局 bounds 比较。
    fn tab_insert_slot_for_position(&self, position: Point<Pixels>) -> usize {
        let offset = self.scroll_handle.offset();
        let content_x = position.x - offset.x;
        insert_slot_for_content_x(content_x, &self.current_tab_bounds())
    }

    fn reorder_preview_has_effect(preview: &TabReorderPreview, tab_count: usize) -> bool {
        let from_index = preview.dragged_tab.from_index;
        if from_index >= tab_count {
            return false;
        }

        let final_index = Self::final_index_for_slot(from_index, preview.insert_slot);
        final_index < tab_count && final_index != from_index
    }

    fn dragged_tab_shift_width(&self, preview: &TabReorderPreview) -> f32 {
        self.current_tab_bounds()
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
        let to_index = Self::final_index_for_slot(from_index, preview.insert_slot);
        let shift = self.dragged_tab_shift_width(preview);
        reorder_offset(from_index, to_index, index, shift)
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

crate::impl_focusable!(TabBar);

impl TabBar {
    /// 右键菜单动作的统一监听器：取用并清除 `context_menu_tab_id`，
    /// 避免过期 id 被后续 action 误用。
    fn context_menu_action_listener<A: Action>(
        cx: &Context<Self>,
        callback: OptionalTabCallback,
    ) -> impl Fn(&A, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _: &A, window, cx| {
            let tab_id = this.context_menu_tab_id.take();
            if let (Some(callback), Some(tab_id)) = (callback.as_ref(), tab_id) {
                callback(tab_id, window, cx);
            }
        })
    }

    /// 标签列表滚动容器：拖拽/落点处理 + 标签与插入预览子元素
    fn render_tab_list(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let on_tab_reorder = self.on_tab_reorder.clone();
        let on_panel_drop = self.on_panel_drop.clone();
        let panel_drop_preview = self.panel_drop_preview.clone();
        let theme = cx.theme().ui;

        let mut children: Vec<AnyElement> =
            Vec::with_capacity(self.tabs.len() + usize::from(panel_drop_preview.is_some()));
        for index in 0..=self.tabs.len() {
            if let Some(preview) = panel_drop_preview
                .as_ref()
                .filter(|preview| preview.insert_index == index)
            {
                children.push(Self::render_panel_drop_preview(theme, preview));
            }
            if let Some(tab) = self.tabs.get(index) {
                children.push(self.render_tab(tab, index, cx));
            }
        }

        div()
            .id("tab-list-container")
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h_full()
            .on_drag_move::<DraggedPanel>(cx.listener(|this, event, _window, cx| {
                this.handle_panel_drag_move(event, cx);
            }))
            .on_drag_move::<DraggedTab>(cx.listener(|this, event, _window, cx| {
                this.handle_tab_drag_move(event, cx);
            }))
            // 拖拽重排：由 drag_move 维护的中点插入位直接应用
            .on_drop(cx.listener({
                let on_tab_reorder = on_tab_reorder.clone();
                move |this, dragged: &DraggedTab, _window, cx| {
                    cx.stop_propagation();

                    let insert_slot = this
                        .reorder_preview
                        .as_ref()
                        .filter(|preview| preview.dragged_tab.tab_id == dragged.tab_id)
                        .map(|preview| preview.insert_slot);

                    if let Some(insert_slot) = insert_slot {
                        let did_reorder = this.apply_tab_reorder(dragged, insert_slot);
                        this.reset_tab_reorder_preview(cx);

                        if let Some(ref callback) = on_tab_reorder
                            && did_reorder.is_some()
                        {
                            callback(dragged.clone(), insert_slot, cx);
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
            .flex_1()
            .overflow_x_scroll()
            .track_scroll(&self.scroll_handle)
            .children(children)
    }

    /// 渲染单个标签（含重排补间动画包装）
    fn render_tab(&self, tab: &TabItem, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().ui;
        let tab_id = tab.id.clone();
        let is_active = self.active_tab_id.as_ref() == Some(&tab.id);
        let label = tab.label.clone();

        let start_offset =
            self.tab_offset_for_preview(self.previous_reorder_preview.as_ref(), index);
        let end_offset = self.tab_offset_for_preview(self.reorder_preview.as_ref(), index);
        let start_opacity =
            self.tab_opacity_for_preview(self.previous_reorder_preview.as_ref(), index);
        let end_opacity = self.tab_opacity_for_preview(self.reorder_preview.as_ref(), index);

        let tab_height = if self.compact {
            px(24.0)
        } else {
            px(TAB_HEIGHT_PX)
        };
        let (border, bg, hover_bg) = if self.compact {
            (theme.transparent(), gpui::transparent_black(), None)
        } else if is_active {
            (theme.tab_active_border, theme.tab_active_background, None)
        } else {
            (
                theme.transparent(),
                theme.tab_inactive_background,
                Some(theme.tab_hover_background),
            )
        };
        let tab_inner = div()
            .id(tab_id.clone())
            .h(tab_height)
            .px(px(TAB_PADDING_X_PX))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.0))
            .rounded(px(TAB_RADIUS_PX))
            .border_1()
            .border_color(border)
            .bg(bg)
            .hover(move |style| match hover_bg {
                Some(hover) => style.bg(hover),
                None => style,
            })
            .on_hover(cx.listener({
                let tab_id = tab_id.clone();
                move |this, hovered: &bool, _window, cx| {
                    this.hovered_tab_id = hovered.then(|| tab_id.clone());
                    cx.notify();
                }
            }))
            .on_drag(
                DraggedTab {
                    tab_id: tab_id.clone(),
                    from_index: index,
                },
                {
                    let label = label.clone();
                    move |_drag_data, position, window, cx| {
                        cx.stop_propagation();
                        let preview = DragPreview::new(label.clone(), position, window, cx);
                        cx.new(move |_cx| preview)
                    }
                },
            )
            .child(self.render_tab_icon_slot(tab, &tab_id, is_active, cx))
            .child(Self::render_tab_label(tab, is_active, theme))
            .on_click(cx.listener({
                let on_tab_click = self.on_tab_click.clone();
                let tab_id = tab_id.clone();
                move |_this, _event: &gpui::ClickEvent, window, cx| {
                    if let Some(ref callback) = on_tab_click {
                        callback(tab_id.clone(), window, cx);
                    }
                }
            }))
            // 左键：阻止冒泡到 TitleBar 触发窗口拖拽
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            // 中键关闭标签（浏览器/终端通用习惯）
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener({
                    let on_tab_close = self.on_tab_close.clone();
                    let tab_id = tab_id.clone();
                    move |_this, _event, window, cx| {
                        if let Some(ref callback) = on_tab_close {
                            callback(tab_id.clone(), window, cx);
                        }
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let tab_id = tab_id.clone();
                    move |this, _, _, cx| {
                        this.context_menu_tab_id = Some(tab_id.clone());
                        cx.notify();
                    }
                }),
            )
            // 完整标题 tooltip（标签宽度受限被截断时尤其有用）
            .tooltip({
                let label = label.clone();
                move |_window, cx| {
                    cx.new(|_| TabTooltip {
                        label: label.clone(),
                    })
                    .into()
                }
            })
            .context_menu(format!("tab-menu-{}", tab_id), {
                let tab_id = tab_id.clone();
                let on_context_menu = self.on_context_menu.clone();
                move |menu: PopupMenu, window, cx| {
                    if let Some(ref cb) = on_context_menu {
                        cb(tab_id.clone(), menu, window, cx)
                    } else {
                        menu
                    }
                }
            });

        let needs_animation = start_offset != end_offset || start_opacity != end_opacity;
        if needs_animation {
            let animation_id = format!("tab-reorder-{}-{}", tab_id, self.reorder_animation_epoch);

            div()
                .child(tab_inner)
                .flex_shrink_0()
                .with_animation(
                    animation_id,
                    Animation::new(Duration::from_millis(TAB_REORDER_ANIMATION_DURATION_MS))
                        .with_easing(ease_out_quint()),
                    move |this, delta| {
                        let offset = start_offset + (end_offset - start_offset) * delta;
                        let opacity = start_opacity + (end_opacity - start_opacity) * delta;
                        this.left(px(offset)).opacity(opacity)
                    },
                )
                .into_any_element()
        } else {
            tab_inner.into_any_element()
        }
    }

    /// 标签的图标/关闭按钮/loading 槽位。
    /// 关闭按钮不依赖 icon 存在：hover 时统一显示关闭按钮，
    /// 其优先于 loading 动画，慢速远程连接也能从标签上直接关闭。
    fn render_tab_icon_slot(
        &self,
        tab: &TabItem,
        tab_id: &SharedString,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().ui;
        let icon = tab.icon;
        let is_hovered = self.hovered_tab_id.as_ref() == Some(tab_id);

        let icon_slot_width = if icon.is_some_and(|i| matches!(i, Icon::FileIcon(_))) {
            px(FILE_ICON_SIZE_PX)
        } else {
            px(ICON_SIZE_PX)
        };

        let content = if is_hovered {
            let on_tab_close = self.on_tab_close.clone();
            let tab_id_close = tab_id.clone();
            svg()
                .size(px(ICON_SIZE_PX))
                .cursor_pointer()
                .text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted
                })
                .path(Icon::XmarkCircleFill.path())
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |_this, _event, window, cx| {
                        if let Some(ref callback) = on_tab_close {
                            callback(tab_id_close.clone(), window, cx);
                        }
                    }),
                )
                .into_any_element()
        } else if tab.is_loading {
            svg()
                .size(px(ICON_SIZE_PX))
                .text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted
                })
                .path(Icon::Loader.path())
                .with_animation(
                    format!("tab-loading-{}", tab_id),
                    Animation::new(Duration::from_millis(800))
                        .repeat()
                        .with_easing(ease_in_out),
                    |this, delta| {
                        this.with_transformation(Transformation::rotate(percentage(delta)))
                    },
                )
                .into_any_element()
        } else if let Some(icon) = icon {
            let icon_size = if matches!(icon, Icon::FileIcon(_)) {
                px(FILE_ICON_SIZE_PX)
            } else {
                px(ICON_SIZE_PX)
            };
            svg()
                .size(icon_size)
                .text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted
                })
                .path(icon.path())
                .into_any_element()
        } else {
            div().into_any_element()
        };

        div()
            .w(icon_slot_width)
            .flex()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }

    /// 标签文本（含未保存圆点），非激活时限宽截断
    fn render_tab_label(tab: &TabItem, is_active: bool, theme: UiTheme) -> Div {
        div()
            .relative()
            .overflow_hidden()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.0))
            .when(!is_active, |this| this.max_w(px(TAB_MAX_WIDTH_PX)))
            .when(tab.is_modified, |this| {
                this.child(
                    div()
                        .size(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(if is_active {
                            theme.text_muted
                        } else {
                            theme.text_muted.opacity(0.7)
                        }),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .whitespace_nowrap()
                    .text_ellipsis()
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
                    .child(tab.label.clone()),
            )
    }

    /// Panel 拖入时的插入位置预览
    fn render_panel_drop_preview(theme: UiTheme, preview: &PanelDropPreview) -> AnyElement {
        div()
            .id(format!("panel-drop-preview-{}", preview.insert_index))
            .h(px(TAB_HEIGHT_PX))
            .px(px(TAB_PADDING_X_PX))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.0))
            .rounded(px(TAB_RADIUS_PX))
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
            .into_any_element()
    }

    /// 新建标签按钮 - 放在滚动区域外面，始终可见
    fn render_new_tab_button(theme: UiTheme, on_new_tab: NewTabCallback) -> Stateful<Div> {
        div()
            .id("new-tab-btn")
            .w(px(NEW_TAB_BUTTON_SIZE_PX))
            .h(px(NEW_TAB_BUTTON_SIZE_PX))
            .flex()
            .flex_shrink_0()
            .ml_1()
            .mr_3()
            .items_center()
            .justify_center()
            .rounded(px(999.0))
            .cursor_pointer()
            .hover(move |style| style.bg(theme.tab_hover_background))
            .child(
                svg()
                    .size(px(ICON_SIZE_PX))
                    .text_color(theme.icon_muted)
                    .path(Icon::Plus.path()),
            )
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                on_new_tab(window, cx);
            })
    }
}

impl Render for TabBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let on_panel_drop = self.on_panel_drop.clone();

        let new_tab_button = self
            .on_new_tab
            .clone()
            .map(|on_new_tab| Self::render_new_tab_button(theme, on_new_tab));

        div()
            .id(self.id.clone())
            .h_full()
            .w_full()
            .flex()
            .items_center()
            // 标签栏背景：编辑器悬浮面板内需透明以透出玻璃材质，其余场景保持 app 背景避免脱节
            .bg(if self.transparent_bg {
                gpui::transparent_black()
            } else {
                theme.app_background
            })
            .overflow_hidden()
            .pt(if self.compact {
                px(2.5)
            } else if self.transparent_bg {
                px(4.)
            } else {
                px(5.)
            })
            .pb(if self.compact {
                px(3.5)
            } else if self.transparent_bg {
                px(4.)
            } else {
                px(3.)
            })
            // 阻止 TabBar 区域的鼠标事件冒泡到 TitleBar，防止触发窗口拖拽
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.stop_propagation();
                }),
            )
            // TabBar 整体接受 DraggedPanel drop（落点在标签列表之外时的兜底）
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
            .on_action(Self::context_menu_action_listener::<CopyTabAddress>(
                cx,
                self.on_tab_copy_address.clone(),
            ))
            .on_action(Self::context_menu_action_listener::<DuplicateTab>(
                cx,
                self.on_tab_duplicate.clone(),
            ))
            .on_action(Self::context_menu_action_listener::<ReconnectTab>(
                cx,
                self.on_tab_reconnect.clone(),
            ))
            .child(
                // 外层容器：横向布局滚动区 + 新建按钮
                div()
                    .id("tab-bar-outer")
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .w_full()
                    .child(self.render_tab_list(cx))
                    .children(new_tab_button),
            )
    }
}

/// 标签完整标题的悬停提示
struct TabTooltip {
    label: SharedString,
}

impl Render for TabTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .max_w(px(TAB_TOOLTIP_MAX_WIDTH_PX))
            .px_2()
            .py_1()
            .rounded(px(TAB_RADIUS_PX))
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .text_sm()
            .text_color(theme.text)
            .child(self.label.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn make_tabs(ids: &[&str]) -> Vec<TabItem> {
        ids.iter().map(|id| TabItem::new(*id, *id)).collect()
    }

    fn ids(tabs: &[TabItem]) -> Vec<String> {
        tabs.iter().map(|t| t.id.to_string()).collect()
    }

    fn preview(from_index: usize, insert_slot: usize) -> TabReorderPreview {
        TabReorderPreview {
            dragged_tab: DraggedTab {
                tab_id: format!("tab-{from_index}").into(),
                from_index,
            },
            insert_slot,
        }
    }

    #[test]
    fn final_index_for_slot_uses_before_semantics() {
        // slot == from 或 from+1：插入位仍在自身位置，等价无移动
        assert_eq!(TabBar::final_index_for_slot(0, 0), 0);
        assert_eq!(TabBar::final_index_for_slot(0, 1), 0);
        // slot > from+1：移除自身后目标位左移一位
        assert_eq!(TabBar::final_index_for_slot(0, 2), 1);
        assert_eq!(TabBar::final_index_for_slot(0, 3), 2);
        assert_eq!(TabBar::final_index_for_slot(1, 3), 2);
        // slot < from：插入位即最终索引
        assert_eq!(TabBar::final_index_for_slot(2, 0), 0);
        assert_eq!(TabBar::final_index_for_slot(2, 3), 2);
    }

    #[test]
    fn apply_reorder_moves_tabs_to_insert_slot() {
        // 右移：A 拖到末尾 [A,B,C] -> [B,C,A]
        let mut tabs = make_tabs(&["a", "b", "c"]);
        assert_eq!(apply_reorder(&mut tabs, "a", 3), Some(0));
        assert_eq!(ids(&tabs), ["b", "c", "a"]);

        // 左移：C 拖到开头 [A,B,C] -> [C,A,B]
        let mut tabs = make_tabs(&["a", "b", "c"]);
        assert_eq!(apply_reorder(&mut tabs, "c", 0), Some(2));
        assert_eq!(ids(&tabs), ["c", "a", "b"]);

        // 跨一位右移：B 拖到 C 之后 [A,B,C] -> [A,C,B]
        let mut tabs = make_tabs(&["a", "b", "c"]);
        assert_eq!(apply_reorder(&mut tabs, "b", 3), Some(1));
        assert_eq!(ids(&tabs), ["a", "c", "b"]);
    }

    #[test]
    fn apply_reorder_no_op_cases() {
        let mut tabs = make_tabs(&["a", "b", "c"]);

        // slot == from：无移动
        assert_eq!(apply_reorder(&mut tabs, "b", 1), None);
        // slot == from + 1：插入位在自身之后，等价无移动
        assert_eq!(apply_reorder(&mut tabs, "b", 2), None);
        assert_eq!(apply_reorder(&mut tabs, "a", 1), None);
        assert_eq!(ids(&tabs), ["a", "b", "c"]);

        // 标签不存在
        assert_eq!(apply_reorder(&mut tabs, "zzz", 0), None);
        // 空列表
        let mut empty: Vec<TabItem> = Vec::new();
        assert_eq!(apply_reorder(&mut empty, "a", 0), None);
    }

    #[test]
    fn reorder_preview_has_effect_matches_final_position_change() {
        assert!(!TabBar::reorder_preview_has_effect(&preview(1, 1), 3));
        assert!(!TabBar::reorder_preview_has_effect(&preview(1, 2), 3));
        assert!(TabBar::reorder_preview_has_effect(&preview(1, 3), 3));
        assert!(TabBar::reorder_preview_has_effect(&preview(1, 0), 3));
        // from 越界
        assert!(!TabBar::reorder_preview_has_effect(&preview(3, 0), 3));
        // 最终索引越界
        assert!(!TabBar::reorder_preview_has_effect(&preview(0, 5), 3));
    }

    #[test]
    fn insert_slot_uses_tab_midpoints() {
        let bounds: Vec<Bounds<Pixels>> = (0..3)
            .map(|i| Bounds {
                origin: point(px(i as f32 * 100.0), px(0.0)),
                size: size(px(100.0), px(30.0)),
            })
            .collect();

        assert_eq!(insert_slot_for_content_x(px(-10.0), &bounds), 0);
        assert_eq!(insert_slot_for_content_x(px(49.0), &bounds), 0);
        // 恰在中点上：视为越过中点，插入到下一个位置
        assert_eq!(insert_slot_for_content_x(px(50.0), &bounds), 1);
        assert_eq!(insert_slot_for_content_x(px(149.0), &bounds), 1);
        assert_eq!(insert_slot_for_content_x(px(150.0), &bounds), 2);
        assert_eq!(insert_slot_for_content_x(px(249.0), &bounds), 2);
        // 最后一个中点之后：追加到末尾
        assert_eq!(insert_slot_for_content_x(px(250.0), &bounds), 3);
        assert_eq!(insert_slot_for_content_x(px(1000.0), &bounds), 3);
        // 无标签：只能插入到 0
        assert_eq!(insert_slot_for_content_x(px(10.0), &[]), 0);
    }

    #[test]
    fn reorder_offset_shifts_only_moved_interval() {
        // 右移 A(0)->2：区间 (0, 2] 左移让位，拖拽标签自身不动
        assert_eq!(reorder_offset(0, 2, 0, 10.0), 0.0);
        assert_eq!(reorder_offset(0, 2, 1, 10.0), -10.0);
        assert_eq!(reorder_offset(0, 2, 2, 10.0), -10.0);
        assert_eq!(reorder_offset(0, 2, 5, 10.0), 0.0);

        // 左移 C(2)->0：区间 [0, 2) 右移让位
        assert_eq!(reorder_offset(2, 0, 0, 10.0), 10.0);
        assert_eq!(reorder_offset(2, 0, 1, 10.0), 10.0);
        assert_eq!(reorder_offset(2, 0, 2, 10.0), 0.0);

        // 无移动：全部为 0
        assert_eq!(reorder_offset(1, 1, 0, 10.0), 0.0);
        assert_eq!(reorder_offset(1, 1, 1, 10.0), 0.0);
    }
}
