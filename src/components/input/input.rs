//! Input 文本输入组件
//!
//! 参考 Zed GPUI 的 input 示例实现，支持光标定位和选择。
use crate::assets::Icon;
use gpui::{
    App, Bounds, ContentMask, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, PaintQuad, Pixels, Point, Render, ShapedLine, SharedString, Style,
    TextAlign, TextRun, UTF16Selection, Window, actions, div, fill, hsla, px, relative, rgb, rgba,
    size, svg,
};
use gpui::{KeyBinding, prelude::*};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub(super) const CONTEXT: &str = "Input";
// Action 定义
actions!(
    input,
    [
        Backspace, Delete, Left, Right, SelectAll, Home, End, Copy, Cut, Paste
    ]
);
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        // Input component key bindings
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
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
    /// 水平滚动偏移量（用于处理文本溢出）
    scroll_offset: Pixels,
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
            scroll_offset: px(0.0),
        }
    }

    /// 启动光标闪烁
    fn start_blinking(&mut self, cx: &mut Context<Self>) {
        if self.disabled || self.blinking_started {
            return;
        }
        // 标记闪烁已启动
        self.blinking_started = true;
        self.cursor_visible = true;
        // 开始闪烁循环
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
        // 如果不再处于闪烁状态，停止递归
        if !self.blinking_started {
            return;
        }

        // 切换光标可见性
        self.cursor_visible = !self.cursor_visible;
        cx.notify();

        // 安排下一次闪烁
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
        // 光标移到末尾
        self.selected_range = text.len()..text.len();
        self.selection_reversed = false;
        cx.notify();
    }

    /// 清空输入
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content = SharedString::default();
        self.selected_range = 0..0;
        self.selection_reversed = false;
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
        // 考虑滚动偏移：鼠标位置需要加上滚动偏移量才能得到文本中的实际位置
        let x = position.x - bounds.left() + self.scroll_offset;
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

    /// 触发变更回调
    fn notify_change(&self, cx: &mut Context<Self>) {
        if let Some(ref callback) = self.on_change {
            callback(&self.content, cx);
        }
    }

    /// 完成编辑操作（重置闪烁并触发回调）
    fn finish_edit(&mut self, cx: &mut Context<Self>) {
        self.reset_blinking(cx);
        self.notify_change(cx);
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
                // 单行模式：移除换行符
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

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }

        // 启动光标闪烁
        self.start_blinking(cx);
    }

    /// 处理鼠标释放
    fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    /// 处理输入的字符（用于程序化插入文本）
    #[allow(dead_code)]
    fn handle_input(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.disabled || text.is_empty() {
            return;
        }

        // 在光标位置插入文本
        let cursor = self.cursor_offset();
        let new_text = format!(
            "{}{}{}",
            &self.content[..cursor],
            text,
            &self.content[cursor..]
        );
        self.content = new_text.into();
        self.selected_range = cursor + text.len()..cursor + text.len();
        self.marked_range = None;

        // 触发回调
        if let Some(ref callback) = self.on_change {
            let text = self.content.to_string();
            callback(&text, cx);
        }

        cx.notify();
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

        // 重置闪烁
        self.reset_blinking(cx);

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
    /// 输入框边界（应用滚动后的）
    text_bounds: Bounds<Pixels>,
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
        style.size.width = relative(1.).into();
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
        let is_focused = input.focus_handle.is_focused(window);

        // 显示文本（密码模式显示圆点）
        let is_empty = content.is_empty();

        // 处理 placeholder：截断并添加省略号
        let max_width = bounds.size.width - px(20.0); // 留出 padding
        let font_size = style.font_size.to_pixels(window.rem_size());
        let placeholder_text = if is_empty {
            // 测量 placeholder 宽度，如果超出则截断
            let placeholder_run = TextRun {
                len: placeholder.len(),
                font: style.font(),
                color: rgb(0x666688).into(),
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

            if placeholder_line.width() > max_width {
                // 需要截断，尝试找到合适的截断位置
                let ellipsis = "...";
                let mut truncated = placeholder.to_string();
                while !truncated.is_empty() {
                    let test_text = format!("{}{}", truncated, ellipsis);
                    let test_run = TextRun {
                        len: test_text.len(),
                        font: style.font(),
                        color: rgb(0x666688).into(),
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
                    if test_line.width() <= max_width {
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

        let (display_text, _text_color): (SharedString, _) = if is_empty {
            (placeholder_text, hsla(0., 0., 0., 0.4))
        } else if masked {
            // Use ASCII `*` for password mask: 1 char = 1 byte
            // This ensures cursor byte offset matches character index
            ("*".repeat(content.len()).into(), style.color)
        } else {
            (content.clone(), style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: if is_empty {
                rgb(0x666688).into() // placeholder 颜色 - 更浅更淡
            } else {
                rgb(0xcccccc).into() // 正常文本颜色
            },
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

        // 读取 cursor_visible 状态
        let cursor_visible = input.cursor_visible;

        // 计算文本总宽度和光标位置
        let text_width = line.width();
        let cursor_pos = line.x_for_index(cursor);

        // 计算可视区域宽度（留出边距以便光标显示）
        let visible_width = bounds.size.width - px(8.0); // 8px 边距

        // 计算滚动偏移量，确保光标始终在可视区域内
        // scroll_offset: 文本向左滚动的距离（正数表示文本向左移动）
        let mut scroll_offset: Pixels;

        if !is_empty && text_width > visible_width {
            // 文本超出可视区域，需要滚动
            // 计算光标相对于可视区域的右侧位置
            let cursor_right = cursor_pos + px(2.0); // 2px 光标宽度

            // 基础滚动量：让光标位置在可视区域内
            // 我们希望：cursor_pos - scroll_offset 在 [0, visible_width] 范围内
            if cursor_right > visible_width {
                // 光标超出右边界，需要向左滚动（增大 scroll_offset）
                scroll_offset = (cursor_right - visible_width + px(4.0)).max(px(0.0));
            } else {
                scroll_offset = px(0.0);
            }

            // 同时检查左边界
            if cursor_pos < scroll_offset {
                // 光标超出左边界，需要向右滚动（减小 scroll_offset）
                scroll_offset = cursor_pos.max(px(0.0));
            }

            // 限制最大滚动量，确保不显示空白
            let max_scroll = (text_width - visible_width).max(px(0.0));
            scroll_offset = scroll_offset.min(max_scroll);
        } else {
            // 文本未超出区域，不需要滚动
            scroll_offset = px(0.0);
        }

        // 仅在 scroll_offset 变化时更新，避免不必要的重绘
        if input.scroll_offset != scroll_offset {
            self.input.update(cx, |input, _cx| {
                input.scroll_offset = scroll_offset;
            });
        }

        // 计算应用滚动后的文本边界
        // scroll_offset 是文本向左滚动的距离，所以文本原点是向左偏移
        let text_bounds = Bounds::new(
            Point::new(bounds.left() - scroll_offset, bounds.top()),
            bounds.size,
        );

        let (selection, cursor_quad) = if selected_range.is_empty() {
            (
                None,
                if is_focused && cursor_visible {
                    // 计算光标高度（基于字体大小，与字符高度相近）
                    let font_size = style.font_size.to_pixels(window.rem_size());
                    let cursor_height = font_size * 1.2; // 略大于字体大小
                    let cursor_y =
                        bounds.top() + (bounds.bottom() - bounds.top() - cursor_height) / 2.0; // 垂直居中
                    Some(fill(
                        Bounds::new(
                            Point::new(text_bounds.left() + cursor_pos, cursor_y),
                            size(px(2.), cursor_height),
                        ),
                        rgb(0x3b82f6), // 蓝色光标
                    ))
                } else {
                    None
                },
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        Point::new(
                            text_bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        Point::new(
                            text_bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x3b82f640), // 半透明蓝色选择背景
                )),
                None,
            )
        };

        InputPrepaintState {
            line: Some(line),
            cursor: cursor_quad,
            selection,
            text_bounds,
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

        // 注册输入处理
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        // 使用 text_bounds 绘制文本（应用滚动偏移）
        let text_bounds = prepaint.text_bounds;

        // 绘制选择背景
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        // 从 prepaint 获取 line 以便后续保存
        let line = prepaint.line.take().unwrap();

        // 使用 content mask 裁剪文本，防止溢出到输入框外
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // 绘制文本（在 text_bounds 位置，即应用了滚动偏移）
            line.paint(
                text_bounds.origin,
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
            // 确保光标在可视区域内才绘制
            let cursor_right = cursor.bounds.right();
            let cursor_left = cursor.bounds.left();
            if cursor_left >= bounds.left() - px(2.0) && cursor_right <= bounds.right() + px(2.0) {
                window.paint_quad(cursor);
            }
        }

        // 保存布局信息用于鼠标点击定位（使用原始 bounds，不包含滚动偏移）
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
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

        // 失焦时停止光标闪烁
        if !is_focused && self.blinking_started {
            self.stop_blinking(cx);
        }

        div()
            .flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            // Input 内部 actions
            .on_action(cx.listener(Input::backspace))
            .on_action(cx.listener(Input::delete))
            .on_action(cx.listener(Input::left))
            .on_action(cx.listener(Input::right))
            .on_action(cx.listener(Input::select_all))
            .on_action(cx.listener(Input::home))
            .on_action(cx.listener(Input::end))
            .on_action(cx.listener(Input::copy))
            .on_action(cx.listener(Input::cut))
            .on_action(cx.listener(Input::paste))
            .on_mouse_down(MouseButton::Left, cx.listener(Input::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Input::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Input::on_mouse_up))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .h(px(36.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(if is_focused {
                        rgb(0x4a4a6a)
                    } else {
                        rgb(0x2d2d44)
                    })
                    .bg(rgb(0x13131f))
                    .overflow_hidden()
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
