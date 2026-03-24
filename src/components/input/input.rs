//! Input 文本输入组件
//!
//! 参考 Zed GPUI 的 input 示例实现，支持光标定位和选择。
use crate::assets::Icon;
use gpui::prelude::*;
use gpui::{
    App, Bounds, ContentMask, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, IntoElement, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, PaintQuad, Pixels, Point, Render,
    ScrollHandle, ScrollWheelEvent, ShapedLine, SharedString, Style, TextAlign, TextRun,
    UTF16Selection, Window, actions, div, fill, point, px, rgb, rgba, size, svg,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub(super) const CONTEXT: &str = "Input";
// Action 定义
actions!(
    input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectAll,
        Home,
        End,
        Copy,
        Cut,
        Paste,
        SelectLeft,
        SelectRight,
    ]
);
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        // Input component key bindings
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
    ]);
}

/// 光标闪烁间隔（毫秒）
const CURSOR_BLINK_INTERVAL: u64 = 530;

/// 光标宽度
const CURSOR_WIDTH: Pixels = px(2.);

/// 光标可见性判断的容差
const CURSOR_VISIBILITY_PADDING: Pixels = px(2.);

/// Placeholder 文字颜色
const PLACEHOLDER_COLOR: u32 = 0x3a3a55;

/// 文本输入组件
pub struct Input {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    /// 光标位置/选择范围
    selected_range: Range<usize>,
    /// 选择是否反向
    selection_reversed: bool,
    /// 标记的文本范围（用于输入法）
    marked_range: Option<Range<usize>>,
    /// 密码模式
    masked: bool,
    /// 是否显示密码可见性切换按钮
    mask_toggle: bool,
    /// 是否禁用
    disabled: bool,
    /// 回调：内容变化
    on_change: Option<Box<dyn Fn(&str, &mut App) + Send + Sync>>,
    /// 上一次布局信息
    last_layout: Option<ShapedLine>,
    /// 上一次边界
    last_bounds: Option<Bounds<Pixels>>,
    /// 是否正在选择（鼠标拖拽）
    is_selecting: bool,
    /// 光标是否可见（用于闪烁效果）
    cursor_visible: bool,
    /// 是否已经开始闪烁（避免重复启动任务）
    blinking_started: bool,
    /// 滚动句柄
    scroll_handle: ScrollHandle,
    /// 可滚动内容的尺寸
    scroll_size: gpui::Size<Pixels>,
    /// 输入框边界
    input_bounds: Bounds<Pixels>,
    /// 透明模式（无边框/背景，嵌入自定义容器时使用）
    transparent: bool,
    /// placeholder 字体大小（不设置则与正文相同）
    placeholder_font_size: Option<Pixels>,
    /// 上一帧是否聚焦（用于检测焦点获得事件）
    was_focused: bool,
}

impl Input {
    /// 创建新的输入框
    pub fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: SharedString::default(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            masked: false,
            mask_toggle: false,
            disabled: false,
            on_change: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            cursor_visible: true,
            blinking_started: false,
            scroll_handle: ScrollHandle::new(),
            scroll_size: gpui::size(px(0.), px(0.)),
            input_bounds: Bounds::default(),
            transparent: false,
            placeholder_font_size: None,
            was_focused: false,
        }
    }

    /// 启动光标闪烁
    fn start_blinking(&mut self, cx: &mut Context<Self>) {
        if self.disabled || self.blinking_started {
            return;
        }
        self.blinking_started = true;
        self.cursor_visible = true;
        self.blink_cursors(cx);
    }

    /// 停止光标闪烁
    fn stop_blinking(&mut self, cx: &mut Context<Self>) {
        self.blinking_started = false;
        self.cursor_visible = false;
        cx.notify();
    }

    /// 闪烁光标（递归调用）
    fn blink_cursors(&mut self, cx: &mut Context<Self>) {
        if !self.blinking_started {
            return;
        }

        self.cursor_visible = !self.cursor_visible;
        cx.notify();

        let interval = std::time::Duration::from_millis(CURSOR_BLINK_INTERVAL);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.blink_cursors(cx);
                });
            }
        })
        .detach();
    }

    /// 重置闪烁（在输入时调用，使光标立即显示）
    fn reset_blinking(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        cx.notify();
    }

    /// 设置占位符文本
    pub fn placeholder(mut self, text: impl Into<SharedString>) -> Self {
        self.placeholder = text.into();
        self
    }

    /// 设置初始值
    pub fn value(mut self, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        self.content = text.clone();
        self.selected_range = text.len()..text.len();
        self.cursor_visible = true;
        self.blinking_started = false;
        self
    }

    /// 设置为密码模式
    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    /// 设置是否显示密码可见性切换按钮
    pub fn mask_toggle(mut self, toggle: bool) -> Self {
        self.mask_toggle = toggle;
        self
    }

    /// 设置密码掩码状态
    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }

    /// 设置禁用状态
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// 透明模式：无边框/背景，嵌入自定义容器时使用
    pub fn transparent(mut self) -> Self {
        self.transparent = true;
        self
    }

    /// 设置 placeholder 字体大小
    pub fn placeholder_font_size(mut self, size: Pixels) -> Self {
        self.placeholder_font_size = Some(size);
        self
    }

    /// 设置内容变化回调
    pub fn on_change<F>(mut self, callback: F) -> Self
    where
        F: Fn(&str, &mut App) + Send + Sync + 'static,
    {
        self.on_change = Some(Box::new(callback));
        self
    }

    /// 获取当前文本
    pub fn text(&self) -> &str {
        &self.content
    }

    /// 设置文本
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        self.content = text.clone();
        self.selected_range = text.len()..text.len();
        self.selection_reversed = false;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// 清空输入
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content = SharedString::default();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// 获取光标位置
    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// 移动到指定位置
    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    /// 选择到指定位置
    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    /// 获取前一个字符边界
    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    /// 获取后一个字符边界
    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    /// 根据鼠标位置获取字符索引
    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        // 考虑滚动偏移：鼠标位置需要加上滚动偏移量（scroll_offset.x 是负数）
        let scroll_offset = self.scroll_handle.offset();
        let x = position.x - bounds.left() - scroll_offset.x;
        line.closest_index_for_x(x)
    }

    /// UTF-16 偏移量转 UTF-8
    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    /// UTF-8 偏移量转 UTF-16
    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    /// 范围转换为 UTF-16
    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    /// 范围从 UTF-16 转换
    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// 处理向左移动
    fn left(&mut self, _: &Left, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    /// 处理向右移动
    fn right(&mut self, _: &Right, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    /// 处理选择向左移动
    fn select_left(&mut self, _: &SelectLeft, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    /// 处理选择向右移动
    fn select_right(&mut self, _: &SelectRight, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    /// 触发变更回调
    fn notify_change(&self, cx: &mut Context<Self>) {
        if let Some(ref callback) = self.on_change {
            callback(&self.content, cx);
        }
    }

    /// 完成编辑操作（重置闪烁）
    fn finish_edit(&mut self, cx: &mut Context<Self>) {
        self.reset_blinking(cx);
    }

    /// 处理退格键
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
        self.finish_edit(cx);
    }

    /// 处理删除键
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
        self.finish_edit(cx);
    }

    /// 将当前选中文本复制到剪贴板
    fn copy_to_clipboard(&self, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            let selected_text = &self.content[self.selected_range.clone()];
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(selected_text.to_string()));
        }
    }

    /// 处理复制
    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.copy_to_clipboard(cx);
    }

    /// 处理剪切
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled || self.selected_range.is_empty() {
            return;
        }

        self.copy_to_clipboard(cx);
        self.replace_text_in_range(None, "", window, cx);
        self.finish_edit(cx);
    }

    /// 处理粘贴
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }

        if let Some(clipboard) = cx.read_from_clipboard() {
            if let Some(mut text) = clipboard.text() {
                text.retain(|c| c != '\n' && c != '\r');

                if !text.is_empty() {
                    self.replace_text_in_range(None, &text, window, cx);
                    self.finish_edit(cx);
                }
            }
        }
    }

    /// 处理全选
    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    /// 处理 Home 键
    fn home(&mut self, _: &Home, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.move_to(0, cx);
    }

    /// 处理 End 键
    fn end(&mut self, _: &End, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.move_to(self.content.len(), cx);
    }

    /// 处理鼠标按下
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        self.is_selecting = true;

        let offset = self.index_for_mouse_position(event.position);

        // 三击选中整行（全部文本）
        if event.button == MouseButton::Left && event.click_count >= 3 {
            self.select_all_line(cx);
            self.start_blinking(cx);
            return;
        }

        // 双击选中单词
        if event.button == MouseButton::Left && event.click_count == 2 {
            self.select_word_at(offset, cx);
            self.start_blinking(cx);
            return;
        }

        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }

        self.start_blinking(cx);
    }

    /// 处理鼠标移动（拖拽选择）
    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled || !self.is_selecting {
            return;
        }

        let offset = self.index_for_mouse_position(event.position);
        self.select_to(offset, cx);
    }

    /// 处理滚轮事件（水平滚动）
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }

        let line_height = window.line_height();
        let delta = event.delta.pixel_delta(line_height);

        let new_offset = self.scroll_handle.offset() + delta;
        let clamped_offset = self.clamp_scroll_offset(
            new_offset,
            self.scroll_size.width,
            self.input_bounds.size.width,
        );

        self.scroll_handle.set_offset(clamped_offset);
        cx.notify();
        cx.stop_propagation();
    }

    /// 限制滚动偏移量在安全范围内
    fn clamp_scroll_offset(
        &self,
        scroll_offset: Point<Pixels>,
        content_width: Pixels,
        visible_width: Pixels,
    ) -> Point<Pixels> {
        let mut offset = scroll_offset;
        let max_scroll = if content_width > visible_width {
            visible_width - content_width
        } else {
            px(0.)
        };
        offset.x = offset.x.clamp(max_scroll, px(0.));
        offset
    }

    /// 选中光标位置的单词
    fn select_word_at(&mut self, offset: usize, cx: &mut Context<Self>) {
        let word_range = self.word_range_at(offset);
        self.selected_range = word_range.clone();
        self.selection_reversed = false;
        cx.notify();
    }

    /// 获取指定位置所在单词的范围
    fn word_range_at(&self, offset: usize) -> Range<usize> {
        if self.content.is_empty() {
            return 0..0;
        }

        let offset = offset.min(self.content.len());

        let is_word_char = |c: char| c.is_ascii_alphanumeric() || c == '_';

        let chars: Vec<(usize, char)> = self.content.char_indices().collect();
        if let Some(pos) = chars.iter().position(|(idx, _)| *idx == offset) {
            let char_type = chars.get(pos).map(|(_, c)| is_word_char(*c));

            let mut start = offset;
            for i in (0..pos).rev() {
                if let Some((idx, c)) = chars.get(i) {
                    if is_word_char(*c) == char_type.unwrap_or(false) {
                        start = *idx;
                    } else {
                        break;
                    }
                }
            }

            let mut end = offset;
            for i in pos..chars.len() {
                if let Some((idx, c)) = chars.get(i) {
                    if is_word_char(*c) == char_type.unwrap_or(false) {
                        end = idx + c.len_utf8();
                    } else {
                        break;
                    }
                }
            }

            start..end
        } else {
            offset..offset
        }
    }

    /// 选中整行（对于单行输入框就是全部文本）
    fn select_all_line(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    /// 处理鼠标释放
    fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }
}

impl EntityInputHandler for Input {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();

        self.reset_blinking(cx);
        self.notify_change(cx);

        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            Point::new(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            Point::new(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;

        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

/// 文本元素，用于渲染输入框内容
struct InputTextElement {
    input: Entity<Input>,
}

struct InputPrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    text_bounds: Bounds<Pixels>,
    scroll_size: gpui::Size<Pixels>,
    cursor_scroll_offset: Point<Pixels>,
}

impl IntoElement for InputTextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputTextElement {
    type RequestLayoutState = ();
    type PrepaintState = InputPrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], _cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();
        let masked = input.masked;
        let placeholder = input.placeholder.clone();
        let placeholder_font_size = input.placeholder_font_size;
        let is_focused = input.focus_handle.is_focused(window);

        let is_empty = content.is_empty();
        let style_font_size = style.font_size.to_pixels(window.rem_size());
        let font_size = if is_empty {
            placeholder_font_size.unwrap_or(style_font_size)
        } else {
            style_font_size
        };

        // 处理 placeholder
        let placeholder_text = if is_empty {
            let ellipsis = "...";
            let max_width = bounds.size.width - px(20.0);

            let placeholder_run = TextRun {
                len: placeholder.len(),
                font: style.font(),
                color: rgb(PLACEHOLDER_COLOR).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let placeholder_line = window.text_system().shape_line(
                placeholder.clone(),
                font_size,
                &[placeholder_run],
                None,
            );

            if placeholder_line.width > max_width {
                let mut truncated = placeholder.to_string();
                while !truncated.is_empty() {
                    let test_text = format!("{}{}", truncated, ellipsis);
                    let test_run = TextRun {
                        len: test_text.len(),
                        font: style.font(),
                        color: rgb(PLACEHOLDER_COLOR).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let test_line = window.text_system().shape_line(
                        test_text.clone().into(),
                        font_size,
                        &[test_run],
                        None,
                    );
                    if test_line.width <= max_width {
                        truncated.push_str(ellipsis);
                        break;
                    }
                    truncated.pop();
                }
                truncated.into()
            } else {
                placeholder
            }
        } else {
            placeholder
        };

        let display_text: SharedString = if is_empty {
            placeholder_text
        } else if masked {
            "*".repeat(content.len()).into()
        } else {
            content.clone()
        };

        let text_color = if is_empty {
            rgb(PLACEHOLDER_COLOR).into()
        } else {
            rgb(0xcccccc).into()
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = input.marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(gpui::UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let line = window
            .text_system()
            .shape_line(display_text.clone(), font_size, &runs, None);

        let text_width = line.width;
        let scroll_size = gpui::Size {
            width: text_width,
            height: window.line_height(),
        };

        let cursor_visible = input.cursor_visible;

        // 计算光标位置和滚动偏移
        let (cursor_bounds, cursor_scroll_offset) = if selected_range.is_empty()
            && is_focused
            && cursor_visible
        {
            let cursor_x = line.x_for_index(cursor);
            let cursor_height = font_size * 0.85;
            let cursor_y = bounds.top() + (bounds.size.height - cursor_height) / 2.0;
            let visible_width = bounds.size.width;

            // 获取滚动偏移并限制在安全范围内
            let scroll_offset =
                input.clamp_scroll_offset(input.scroll_handle.offset(), text_width, visible_width);

            // 计算光标边界
            // 光标位置 = 文本原点 + 光标在文本中的位置
            // 文本原点 = bounds.left() + scroll_offset.x (scroll_offset.x 是负数)
            // 光标在文本中的位置 = cursor_x
            let quad = fill(
                Bounds::new(
                    point(bounds.left() + scroll_offset.x + cursor_x, cursor_y),
                    size(CURSOR_WIDTH, cursor_height),
                ),
                rgb(0x3b82f6),
            );
            (Some(quad), scroll_offset)
        } else {
            (None, input.scroll_handle.offset())
        };

        // 计算选择区域
        // 选择区域也在同样的坐标系中
        let selection = if !selected_range.is_empty() {
            Some(fill(
                Bounds::from_corners(
                    Point::new(
                        bounds.left()
                            + cursor_scroll_offset.x
                            + line.x_for_index(selected_range.start),
                        bounds.top(),
                    ),
                    Point::new(
                        bounds.left()
                            + cursor_scroll_offset.x
                            + line.x_for_index(selected_range.end),
                        bounds.bottom(),
                    ),
                ),
                rgba(0x3b82f640),
            ))
        } else {
            None
        };

        // 计算文本边界（应用滚动后的）
        let text_bounds = Bounds::new(
            Point::new(bounds.left() + cursor_scroll_offset.x, bounds.top()),
            bounds.size,
        );

        InputPrepaintState {
            line: Some(line),
            cursor: cursor_bounds,
            selection,
            text_bounds,
            scroll_size,
            cursor_scroll_offset,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        // 绘制选择背景
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        let line = prepaint.line.take().unwrap();

        // 使用 content mask 裁剪文本
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            line.paint(
                prepaint.text_bounds.origin,
                window.line_height(),
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .unwrap();
        });

        // 绘制光标（在 content mask 外绘制，确保光标不会被裁剪）
        if let Some(cursor) = prepaint.cursor.take() {
            let cursor_right = cursor.bounds.right();
            let cursor_left = cursor.bounds.left();
            if cursor_left >= bounds.left() - CURSOR_VISIBILITY_PADDING
                && cursor_right <= bounds.right() + CURSOR_VISIBILITY_PADDING
            {
                window.paint_quad(cursor);
            }
        }

        // 保存布局信息
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll_size = prepaint.scroll_size;
            input.input_bounds = bounds;
            // 直接使用 prepaint 中计算出的滚动偏移来更新 scroll_handle
            input
                .scroll_handle
                .set_offset(prepaint.cursor_scroll_offset);
        });
    }
}

impl Focusable for Input {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Input {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_focused = self.focus_handle.is_focused(_window);

        if is_focused && !self.was_focused {
            self.start_blinking(cx);
        } else if !is_focused && self.blinking_started {
            self.stop_blinking(cx);
        }
        self.was_focused = is_focused;

        div()
            .w_full()
            .flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Input::backspace))
            .on_action(cx.listener(Input::delete))
            .on_action(cx.listener(Input::left))
            .on_action(cx.listener(Input::right))
            .on_action(cx.listener(Input::select_left))
            .on_action(cx.listener(Input::select_right))
            .on_action(cx.listener(Input::select_all))
            .on_action(cx.listener(Input::home))
            .on_action(cx.listener(Input::end))
            .on_action(cx.listener(Input::copy))
            .on_action(cx.listener(Input::cut))
            .on_action(cx.listener(Input::paste))
            .on_mouse_down(MouseButton::Left, cx.listener(Input::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Input::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Input::on_mouse_up))
            .on_mouse_move(cx.listener(Input::on_mouse_move))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .when(!self.transparent, |this| {
                        this.h(px(36.0))
                            .px(px(10.0))
                            .gap(px(8.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(if is_focused {
                                rgb(0x4a4a6a)
                            } else {
                                rgb(0x2d2d44)
                            })
                            .bg(rgb(0x13131f))
                    })
                    .when(self.transparent, |this| this.h_full())
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .on_scroll_wheel(cx.listener(Input::on_scroll_wheel))
                    .child(InputTextElement { input: cx.entity() })
                    .when(self.mask_toggle, |this| {
                        let input_entity = cx.entity();
                        let input1 = input_entity.clone();
                        let input2 = input_entity.clone();
                        this.child(
                            div()
                                .id("toggle-mask")
                                .cursor(CursorStyle::Arrow)
                                .child(
                                    svg()
                                        .size(px(16.0))
                                        .path(if self.masked {
                                            Icon::Eye.path()
                                        } else {
                                            Icon::EyeOff.path()
                                        })
                                        .text_color(rgb(0x8888aa)),
                                )
                                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                    input1.update(cx, |state, cx| {
                                        state.set_masked(false, cx);
                                    })
                                })
                                .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                                    input2.update(cx, |state, cx| {
                                        state.set_masked(true, cx);
                                    })
                                }),
                        )
                    }),
            )
    }
}

/// 带标签的输入框组件
pub struct LabeledInput {
    input: Entity<Input>,
    label: Option<SharedString>,
}

impl LabeledInput {
    pub fn new(cx: &mut App) -> Self {
        let input = cx.new(|cx| Input::new(cx));
        Self { input, label: None }
    }

    pub fn label(mut self, text: impl Into<SharedString>) -> Self {
        self.label = Some(text.into());
        self
    }

    pub fn entity(self) -> Entity<Input> {
        self.input
    }
}

impl IntoElement for LabeledInput {
    type Element = gpui::AnyElement;
    fn into_element(self) -> Self::Element {
        use gpui::ParentElement;

        let Self { input, label } = self;

        let mut container = div().w_full().flex().flex_col().gap(px(4.0));

        if let Some(label_text) = label {
            container = container.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8888aa))
                    .child(label_text.to_string()),
            );
        }

        container.child(input).into_any_element()
    }
}

/// 简化的文本输入构造器
pub struct TextInput;

impl TextInput {
    pub fn new(cx: &mut App) -> Entity<Input> {
        cx.new(|cx| Input::new(cx))
    }

    pub fn with_placeholder(cx: &mut App, text: impl Into<SharedString>) -> Entity<Input> {
        cx.new(|cx| Input::new(cx).placeholder(text))
    }

    pub fn with_value(cx: &mut App, text: impl Into<SharedString>) -> Entity<Input> {
        cx.new(|cx| Input::new(cx).value(text))
    }
}
