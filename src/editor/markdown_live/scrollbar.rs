//! 自绘滚动条（常显）：markdown 编辑器的纵向滚动条 + 表格/结构图的横向滚动条。
//!
//! 为什么自绘：gpui 在本 rev 只为 `Overflow::Scroll` **预留布局宽度**
//! （`scrollbar_width`），不画任何滚动条。deepterm 的终端
//! （`terminal_view/terminal_scrollbar.rs`）与代码编辑器
//! （`components/input/element.rs` 的 paint 段）都是自绘，这里沿用同一套做法，
//! 并抽成轴无关的通用组件（`Axis::Horizontal` / `Axis::Vertical`）。
//!
//! 代码编辑器不整块复用 `scroll_bar`（它的 paint 与输入框绘制耦合在同一
//! Element 里），但**几何与样式取自本模块**：`thumb_metrics` /
//! `scroll_for_drag` 算位置，`BAR_THICKNESS` / `THUMB_RADIUS` /
//! `GRAB_SLOP` 定尺寸，颜色同为 `border.opacity(0.7)` + 拖拽时
//! `border_strong`。改这里请同步那边，否则两个滚动条会长得不一样。
//!
//! 为什么不用 gpui 默认滚轮：`paint_scroll_listener` 在「只开了 x 轴」时会把
//! **纵向**滚轮增量喂给 x 轴（`Overflow::Scroll if !restrict_scroll_to_axis &&
//! overflow.y != Overflow::Scroll => delta.y`），普通上下滚动会被卡片截胡。
//! 所以横向滚动容器一律加 `restrict_scroll_to_axis`，滚动条只负责画 + 拖。
//!
//! 关键约束（踩过的坑，改前先读）：
//! - thumb 轨道在**视口**坐标系里，不随内容平移。横向条因此排在滚动容器的
//!   **后面**（同一卡片的独立条带）；挂进滚动内容里会跟着一起滚走，拖到末端
//!   就摸不到 thumb 了。
//! - 几何每帧现算、不缓存：内容宽度随窗口宽度变化（表格列是 `flex_1`），
//!   缓存值在 resize 之后会撒谎。
//! - `max_offset` 由 gpui 在 prepaint 里按子元素外框回填；同帧内**排在滚动
//!   容器之后**的元素能读到本帧的最新值（`clamp_scroll_position` 先于子元素
//!   prepaint 执行）。渲染层是否画出滚动条条带则用上一帧的值判断（一帧收敛）。

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Axis, Bounds, Context, Entity, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Render, ScrollHandle, Window, canvas, div, fill, point,
    px, size,
};

/// thumb 最小长度：内容再长也要抓得住。
const MIN_THUMB: Pixels = px(28.0);
/// 轨道两端留白（thumb 不会顶到卡片边缘）。
const TRACK_INSET: Pixels = px(2.0);
/// 滚动条条带厚度（横条高 / 纵条宽，thumb 同宽），全应用统一 8px：
/// markdown 编辑器纵向条、代码编辑器纵向/横向条共用本常量，终端滚动条在
/// `terminal_view.rs` 里独立写 `.w(px(8.0))`，改这里时记得同步那边。
/// （markdown 图片/表格下面的横条带更贴内容，单独用 6px，见 `h_bar_strip`
/// 的 `thickness` 实参。）
pub(crate) const BAR_THICKNESS: Pixels = px(8.0);
/// thumb 圆角。
pub(crate) const THUMB_RADIUS: Pixels = px(4.0);
/// 命中宽容（thumb 只有 4~8px 厚，不加宽容很难按中）。
pub(crate) const GRAB_SLOP: Pixels = px(3.0);

/// 由「视口长 / 内容长 / 当前偏移」算 thumb 的 `(轨道内起点, 长度)`（轴无关）。
///
/// 内容不溢出时返回 `None`：不画滚动条，也不吃鼠标事件。
pub(crate) fn thumb_metrics(
    viewport_len: Pixels,
    content_len: Pixels,
    offset: Pixels,
) -> Option<(Pixels, Pixels)> {
    let track = viewport_len - TRACK_INSET * 2.0;
    if content_len <= viewport_len + px(0.5) || track <= px(0.) {
        return None;
    }
    // `Pixels::clamp` 落到 `Ord::clamp`，它断言 `min <= max`：窄视口（轨道比
    // MIN_THUMB 还短）必须先把下界压到轨道，否则 panic（release 也崩）。
    let thumb_len = (track * (viewport_len / content_len)).clamp(MIN_THUMB.min(track), track);
    let travel = track - thumb_len;
    let max_scroll = content_len - viewport_len;
    let frac = (-offset / max_scroll).clamp(0., 1.);
    Some((TRACK_INSET + travel * frac, thumb_len))
}

/// 拖动换算：thumb 走 `delta`，内容滚 `delta * (max_scroll / travel)`（轴无关）。
pub(crate) fn scroll_for_drag(
    start_scroll: Pixels,
    delta: Pixels,
    viewport_len: Pixels,
    content_len: Pixels,
    thumb_len: Pixels,
) -> Pixels {
    let max_scroll = (content_len - viewport_len).max(px(0.));
    let travel = (viewport_len - TRACK_INSET * 2.0 - thumb_len).max(px(0.));
    if travel <= px(0.) || max_scroll <= px(0.) {
        return px(0.);
    }
    (start_scroll + delta * (max_scroll / travel)).clamp(px(0.), max_scroll)
}

/// 拖动中的滚动条状态（每个滚动条一个实体，避免线程局部量在多视图下串味）。
#[derive(Default)]
pub(crate) struct ScrollBarState {
    /// `(按下时的指针坐标, 按下时的滚动偏移)`，沿本条的轴。
    grab: Option<(Pixels, Pixels)>,
}

impl ScrollBarState {
    pub(crate) fn is_dragging(&self) -> bool {
        self.grab.is_some()
    }

    /// 收尾拖拽（窗口失活时用，见 [`ScrollBarMap::clear_drags`]）。
    pub(crate) fn clear_drag(&mut self) {
        self.grab = None;
    }

    /// 测试用：直接置位拖拽状态（等价于按下 thumb）。
    #[cfg(test)]
    pub(crate) fn set_drag_for_test(&mut self, at: Pixels, scroll: Pixels) {
        self.grab = Some((at, scroll));
    }
}

impl Render for ScrollBarState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 一帧的滚动条几何（paint 与拖拽监听共用同一份）。
#[derive(Clone, Copy)]
pub(crate) struct ScrollBarGeom {
    thumb: Bounds<Pixels>,
    track: Bounds<Pixels>,
    viewport_len: Pixels,
    content_len: Pixels,
    thumb_len: Pixels,
}

fn scroll_of(handle: &ScrollHandle, axis: Axis) -> Pixels {
    match axis {
        Axis::Horizontal => handle.offset().x,
        Axis::Vertical => handle.offset().y,
    }
}

fn set_scroll(handle: &ScrollHandle, axis: Axis, scroll: Pixels) {
    let mut off = handle.offset();
    match axis {
        Axis::Horizontal => off.x = -scroll,
        Axis::Vertical => off.y = -scroll,
    }
    handle.set_offset(off);
}

fn along(axis: Axis, p: gpui::Point<Pixels>) -> Pixels {
    match axis {
        Axis::Horizontal => p.x,
        Axis::Vertical => p.y,
    }
}

/// 自绘滚动条：内容溢出时**常显**（不是悬停才出现）。
///
/// 返回一个铺满父级的 canvas —— 调用方负责给出条带矩形（横向 = 卡片底部的
/// 独立条带；纵向 = 内容区右缘的覆盖层），并保证它排在滚动容器**之后**。
///
/// `paging`：命中轨道空白时是否翻一屏。横向条在独立条带里、不与内容抢点击，
/// 所以开启；纵向条压在内容右缘（与代码编辑器同款），只认 thumb，否则会把
/// 「点行尾」的点击吃掉。
///
/// 元素不插入命中框：监听是窗口级的（`window.on_mouse_event`），命中判定由
/// 本组件按 thumb / 轨道矩形自己做，因此不会遮挡下层内容的悬停与点击。
pub(crate) fn scroll_bar(
    axis: Axis,
    state: Entity<ScrollBarState>,
    handle: ScrollHandle,
    color: Hsla,
    active: Hsla,
    paging: bool,
) -> AnyElement {
    let prepaint_handle = handle.clone();
    canvas(
        move |bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut App| {
            let viewport_len = match axis {
                Axis::Horizontal => bounds.size.width,
                Axis::Vertical => bounds.size.height,
            };
            let over = match axis {
                Axis::Horizontal => prepaint_handle.max_offset().x,
                Axis::Vertical => prepaint_handle.max_offset().y,
            };
            let content_len = viewport_len + over.max(px(0.));
            let offset = scroll_of(&prepaint_handle, axis);
            thumb_metrics(viewport_len, content_len, offset).map(|(start, thumb_len)| {
                let thumb = match axis {
                    Axis::Horizontal => Bounds::new(
                        point(bounds.origin.x + start, bounds.origin.y),
                        size(thumb_len, bounds.size.height),
                    ),
                    Axis::Vertical => Bounds::new(
                        point(bounds.origin.x, bounds.origin.y + start),
                        size(bounds.size.width, thumb_len),
                    ),
                };
                ScrollBarGeom {
                    thumb,
                    track: bounds,
                    viewport_len,
                    content_len,
                    thumb_len,
                }
            })
        },
        move |_bounds: Bounds<Pixels>,
              geom: Option<ScrollBarGeom>,
              window: &mut Window,
              cx: &mut App| {
            let Some(geom) = geom else {
                return;
            };
            let dragging = state.read(cx).is_dragging();
            let paint_color = if dragging { active } else { color };
            window.paint_quad(fill(geom.thumb, paint_color).corner_radii(THUMB_RADIUS));

            // 按下：命中 thumb（含宽容）→ 开始拖；`paging` 时命中轨道空白 →
            // 翻一屏；其余一律不认领（卡片内容的点击/选择不受影响）。
            //
            // 本监听注册在滚动内容之后（paint 顺序），冒泡阶段先于行/单元格的
            // 监听执行，`stop_propagation` 因此能挡住「拖滚动条顺带选中文字」。
            //
            // 重绘走 `window.refresh()` 而不是 `cx.notify(state)`：`state` 只是
            // 状态容器，从未作为元素渲染，对它 notify 找不到 window validator，
            // 不会触发重绘（滚动条就会拖了不动）。刷新窗口才是这里要的效果。
            let down_handle = handle.clone();
            let down_state = state.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                let thumb_hit = Bounds::new(
                    point(
                        geom.thumb.origin.x - GRAB_SLOP,
                        geom.thumb.origin.y - GRAB_SLOP,
                    ),
                    size(
                        geom.thumb.size.width + GRAB_SLOP * 2.0,
                        geom.thumb.size.height + GRAB_SLOP * 2.0,
                    ),
                );
                if thumb_hit.contains(&event.position) {
                    let grab = (along(axis, event.position), -scroll_of(&down_handle, axis));
                    down_state.update(cx, |s, _| s.grab = Some(grab));
                } else if paging && geom.track.contains(&event.position) {
                    // 轨道空白：翻一屏（点前半段向前、后半段向后）。
                    let page = (geom.viewport_len * 0.9).max(px(40.));
                    let cur = -scroll_of(&down_handle, axis);
                    let max = (geom.content_len - geom.viewport_len).max(px(0.));
                    let thumb_at = along(axis, geom.thumb.origin);
                    let want = if along(axis, event.position) < thumb_at {
                        (cur - page).max(px(0.))
                    } else {
                        (cur + page).min(max)
                    };
                    set_scroll(&down_handle, axis, want);
                } else {
                    return;
                }
                window.refresh();
                cx.stop_propagation();
            });

            let move_handle = handle.clone();
            let move_state = state.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() {
                    return;
                }
                let Some((start_at, start_scroll)) = move_state.read(cx).grab else {
                    return;
                };
                if !event.dragging() {
                    return;
                }
                let new_scroll = scroll_for_drag(
                    start_scroll,
                    along(axis, event.position) - start_at,
                    geom.viewport_len,
                    geom.content_len,
                    geom.thumb_len,
                );
                set_scroll(&move_handle, axis, new_scroll);
                window.refresh();
                cx.stop_propagation();
            });

            let up_state = state.clone();
            window.on_mouse_event(move |_event: &MouseUpEvent, phase, window, cx| {
                if !phase.bubble() {
                    return;
                }
                if up_state.read(cx).is_dragging() {
                    up_state.update(cx, |s, _| s.grab = None);
                    window.refresh();
                }
            });
        },
    )
    .size_full()
    .into_any_element()
}

/// 横向滚动条的**独立条带**：卡片底部一整条（`thickness` 高），内容溢出时
/// 常显。
///
/// 条带在卡片布局里占位（不是覆盖层），所以永远不会压住最后一行文字。
///
/// `visible` 决定本帧是否占位。调用方传「上一帧内容是否溢出」——溢出与否由
/// gpui 在 prepaint 里按子元素外框回填（`max_offset`），元素构造阶段读不到
/// 本帧值；一帧收敛，且不构成反馈环（条带只影响高度、不影响宽度）。
///
/// 不溢出时保留**零高度**占位（而不是不返回元素）：元素树跨帧稳定，调试
/// 选择器始终可查，高度变化就是唯一可见的差异。
pub(crate) fn h_bar_strip(
    id: impl Into<gpui::ElementId>,
    state: Entity<ScrollBarState>,
    handle: ScrollHandle,
    thickness: Pixels,
    color: Hsla,
    active: Hsla,
    visible: bool,
) -> AnyElement {
    let bar = scroll_bar(Axis::Horizontal, state, handle, color, active, true);
    div()
        .id(id)
        .debug_selector(|| "live-hscroll-bar".to_string())
        .w_full()
        .flex_shrink_0()
        .h(if visible { thickness } else { px(0.) })
        .child(bar)
        .into_any_element()
}

/// 每个滚动块一个状态实体 + 滚动句柄，按块首行索引。
///
/// 存在视图上而不是每帧新建：滚动位置必须跨帧、跨重渲染保持。
#[derive(Default)]
pub(crate) struct ScrollBarMap {
    states: std::collections::HashMap<usize, Entity<ScrollBarState>>,
    handles: std::collections::HashMap<usize, ScrollHandle>,
    /// 本帧渲染到的块（`slot` 调用方；仅用于诊断与测试）。
    frame: Vec<usize>,
}

impl ScrollBarMap {
    pub(crate) fn begin_frame(&mut self) {
        self.frame.clear();
    }

    /// 取（必要时创建）`block_id` 的 `(拖拽状态, 滚动句柄)`。
    ///
    /// 每块每帧只应调用一次（帧内重复调用只会重复记录，无副作用）。
    pub(crate) fn slot<V: 'static>(
        &mut self,
        block_id: usize,
        cx: &mut Context<V>,
    ) -> (Entity<ScrollBarState>, ScrollHandle) {
        self.frame.push(block_id);
        let state = self
            .states
            .entry(block_id)
            .or_insert_with(|| cx.new(|_| ScrollBarState::default()))
            .clone();
        let handle = self
            .handles
            .entry(block_id)
            .or_insert_with(ScrollHandle::new)
            .clone();
        (state, handle)
    }

    /// 收帧：淘汰 `alive` 之外的块（`alive` = 本帧文档里仍存在的滚动块）。
    ///
    /// 判据是「文档里还有没有这个块」，不是「本帧画没画到」：滚出虚拟化窗口
    /// 或切到源码视图的块，句柄必须留着 —— 句柄持有滚动偏移，丢掉它等于把
    /// 横向位置归零，滚回来时卡片会跳回最左。
    pub(crate) fn end_frame(&mut self, alive: &[usize]) {
        self.states.retain(|k, _| alive.binary_search(k).is_ok());
        self.handles.retain(|k, _| alive.binary_search(k).is_ok());
    }

    /// 丢弃全部句柄与拖拽状态（换文档时用：行号只在一份文档内有意义）。
    pub(crate) fn clear(&mut self) {
        self.states.clear();
        self.handles.clear();
        self.frame.clear();
    }

    /// 收尾所有进行中的拖拽（窗口失活时用）。
    ///
    /// `grab` 只在窗口收到 MouseUp 时才清；手势在窗口收不到事件时结束
    /// （拖到一半 Cmd-Tab）会把它永久留在 `Some`，`on_fallback_drag` 之后
    /// 每次拖拽都会让行，静默禁用选区扩展。
    pub(crate) fn clear_drags(&mut self, cx: &mut App) -> bool {
        let mut cleared = false;
        for state in self.states.values() {
            if state.read(cx).is_dragging() {
                state.update(cx, |s, _| s.grab = None);
                cleared = true;
            }
        }
        cleared
    }

    /// 任意块正在拖动滚动条（供点击/选择路径让行）。
    pub(crate) fn any_dragging(&self, cx: &App) -> bool {
        self.states.values().any(|s| s.read(cx).is_dragging())
    }

    /// 某块当前的滚动句柄（不存在时 `None`）。滚动视口的实测宽度由它提供
    /// （gpui 在 prepaint 回填 `bounds`），供「适配宽度」计算使用。
    pub(crate) fn handle(&self, block_id: usize) -> Option<ScrollHandle> {
        self.handles.get(&block_id).cloned()
    }

    /// 某块当前的横向滚动句柄（测试/诊断用；不存在时 `None`）。
    #[cfg(test)]
    pub(crate) fn handle_for(&self, block_id: usize) -> Option<ScrollHandle> {
        self.handles.get(&block_id).cloned()
    }
}

/// 表格内容自然宽度下界：列数 × 每列可读最小宽 + 边框余量。
///
/// 视口比这个下界宽时列平分、不滚动；比下界窄时才横向滚动 —— 避免「窗口
/// 一窄就把每个字都竖起来」（`flex_1` 列原本没有下界，可以缩到不可读）。
pub(crate) fn table_natural_width(cols: usize) -> Pixels {
    /// 每列可读最小宽（含 `px_3` 左右内边距 24px 与一段可辨识的文字）。
    const MIN_COL: f32 = 96.0;
    /// 行把手（8px）+ 边框余量。
    const CHROME: f32 = 16.0;
    px(cols.max(1) as f32 * MIN_COL + CHROME)
}
