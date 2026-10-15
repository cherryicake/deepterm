use std::{ops::Range, rc::Rc};

use gpui::{
    App, AvailableSpace, Bounds, Corners, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, GlobalElementId, Half, HighlightStyle, Hitbox, HitboxBehavior, Hsla, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Path, Pixels, Point,
    ShapedLine, SharedString, Size, Style, TextAlign, TextRun, TextStyle, TextStyleRefinement,
    UnderlineStyle, Window, fill, point, px, relative, size,
};
use ropey::Rope;
use smallvec::SmallVec;

use crate::{
    components::input::{
        DiffLineKind, RopeExt as _,
        blink_cursor::CURSOR_WIDTH,
        cursor::{CursorLayout, CursorShape},
        display_map::LineLayout,
    },
    editor::markdown_live::scrollbar::{
        BAR_THICKNESS, GRAB_SLOP, THUMB_RADIUS, scroll_for_drag, thumb_metrics,
    },
    sidebar_view::font_settings::FontState,
    theme::ActiveTheme as _,
};

use super::{
    BracketSpan, GitLineMark, InputState, LastLayout, RainbowCache, ScrollbarGeometry,
    WhitespaceIndicators,
    mode::InputMode,
    state::{
        RAINBOW_BRACKETS_MAX_TEXT_LEN, is_code_bracket, scan_bracket_spans_text,
        scan_bracket_spans_tree,
    },
};

/// Deterministic replacement for `gpui::combine_highlights`.
/// The original uses `FxHashSet` iteration, which is non-deterministic and
/// caused paired brackets (`{}`, `()`) to get different colors. `overlay`
/// is applied after `base` and wins on overlap.
fn combine_highlights_stable(
    base: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    overlay: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut highlights = Vec::new();
    let mut endpoints = Vec::new(); // (position, is_start, id)
    for (range, style) in base.into_iter().chain(overlay) {
        if range.is_empty() {
            continue;
        }
        let id = highlights.len();
        endpoints.push((range.start, true, id));
        endpoints.push((range.end, false, id));
        highlights.push(style);
    }
    if endpoints.is_empty() {
        return Vec::new();
    }
    // Ends (false) before starts (true) at the same position.
    endpoints.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut active = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut i = 0;
    while i < endpoints.len() {
        let pos = endpoints[i].0;
        if pos > cursor && !active.is_empty() {
            let style = active.iter().fold(HighlightStyle::default(), |acc, &id| {
                acc.highlight(highlights[id])
            });
            out.push((cursor..pos, style));
        }
        while i < endpoints.len() && endpoints[i].0 == pos {
            let (_, is_start, id) = endpoints[i];
            if is_start {
                active.insert(id);
            } else {
                active.remove(&id);
            }
            i += 1;
        }
        cursor = pos;
    }
    out
}

const BOTTOM_MARGIN_ROWS: usize = 3;
/// 正文右侧留白：行尾最多能滚到「视口右缘 − RIGHT_MARGIN」处。必须 **≥
/// 滚动条厚度**（`markdown_live::scrollbar::BAR_THICKNESS`，8px），否则滚到
/// 行尾时最后几像素会钻到右侧纵向滚动条底下看不见。
pub(super) const RIGHT_MARGIN: Pixels = px(10.);
pub(super) const LINE_NUMBER_RIGHT_MARGIN: Pixels = px(10.);
const FOLD_ICON_HITBOX_WIDTH: Pixels = px(18.);
const MAX_HIGHLIGHT_LINE_LENGTH: usize = 10_000;

use super::MASK_CHAR;

/// Convert a byte offset in the original text to a byte offset in the masked display string.
///
/// The masked string consists of `MASK_CHAR` repeated once per character in the original text.
/// Since `MASK_CHAR` may be multi-byte in UTF-8, the byte offset in the masked string is
/// `char_index * MASK_CHAR.len_utf8()`.
fn masked_display_offset(text: &Rope, original_offset: usize) -> usize {
    text.offset_to_char_index(original_offset) * MASK_CHAR.len_utf8()
}

/// Zed's fold disclosure icons (`assets/icons/chevron_down.svg` /
/// `chevron_right.svg` in the zed repo, copied verbatim into DeepTerm's
/// assets). Rendered via `window.paint_svg` with the muted text color,
/// matching zed's `ui::Disclosure` (IconButton + `IconName::ChevronDown` /
/// `ChevronRight`, `IconSize::Small`, `Color::Muted`).
const FOLD_ICON_OPEN: &str = "icons/chevron_down.svg";
const FOLD_ICON_CLOSED: &str = "icons/chevron_right.svg";

/// Size of the fold disclosure icon, matching zed's `IconSize::Small`
/// (14px) constrained by the crease-toggle available height
/// (`line_height * 0.55`, see zed's `prepaint_crease_toggles`).
fn fold_icon_size(line_height: Pixels) -> Pixels {
    px(14.).min(line_height * 0.55)
}

/// Horizontal padding inside the `⋯` placeholder pill, approximating zed's
/// `FoldPlaceholder::fold_element` (`size_full` rounded background around
/// the ellipsis).
const FOLD_PLACEHOLDER_PADDING: Pixels = px(2.);

/// Layout information for a gutter fold disclosure icon (zed's crease toggle).
struct FoldGutterIcon {
    /// The buffer row the icon toggles.
    row: usize,
    is_folded: bool,
    bounds: Bounds<Pixels>,
    hitbox: Hitbox,
}

/// A shaped `⋯` placeholder plus the fold's trailing text (e.g. `}`),
/// painted inline at the end of the fold's header line (zed's fold
/// placeholder + the text after the fold range on the same display row).
struct FoldPlaceholder {
    /// The buffer row of the fold header.
    row: usize,
    /// Bounds of the `⋯` pill (window space), flush with the end of the
    /// header line's last wrapped row; computed after the cursor layout.
    pill_bounds: Option<Bounds<Pixels>>,
    /// Hitbox of the pill (pointing-hand cursor + click to unfold).
    hitbox: Option<Hitbox>,
    /// Shaped `⋯` (U+22EF).
    ellipsis: ShapedLine,
    /// The text after the fold end on the closing row (e.g. `}`), with syntax
    /// highlighting; empty when the fold has no closing row tail.
    tail: Option<ShapedLine>,
}

impl FoldPlaceholder {
    /// Total width of the placeholder pill (background + `⋯`).
    fn pill_width(&self) -> Pixels {
        self.ellipsis.width + FOLD_PLACEHOLDER_PADDING * 2.
    }
}

/// Fold-related layout data for one frame (gutter icons + inline placeholders).
struct FoldIconLayout {
    /// Hitbox for the gutter area (used for hover detection, zed's `gutter_hovered`).
    line_number_hitbox: Hitbox,
    /// Fold disclosure icons for every visible foldable row.
    icons: Vec<FoldGutterIcon>,
}

pub(crate) struct TextElement {
    pub(crate) state: Entity<InputState>,
    placeholder: SharedString,
}

/// 悬浮浮层与锚点行之间的间隙（Zed 的 `HOVER_POPOVER_GAP`）。
const HOVER_POPOVER_GAP: Pixels = px(4.);

impl TextElement {
    pub(super) fn new(state: Entity<InputState>) -> Self {
        Self {
            state,
            placeholder: SharedString::default(),
        }
    }

    /// Set the placeholder text of the input field.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    fn paint_mouse_listeners(&mut self, window: &mut Window, _: &mut App) {
        // Sticky scroll 点击：命中钉性行 → 滚动到该定义行
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseDownEvent, _, _window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }
                let targets = state.read(cx).sticky_click_targets.clone();
                if let Some((buffer_line, _)) = targets
                    .iter()
                    .find(|(_, bounds)| bounds.contains(&event.position))
                {
                    let buffer_line = *buffer_line;
                    state.update(cx, |state, cx| {
                        let offset = state.text.line_start_offset(buffer_line);
                        state.scroll_to_center(offset, cx);
                    });
                    cx.stop_propagation();
                }
            }
        });

        // 滚动条拖拽：按下命中 thumb → 拖动改 scroll offset → 松开结束。
        // 纵向（右缘）与横向（底缘）两条各自一套状态，几何/换算共用。
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseDownEvent, _, window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }
                // 纵向条。
                if let Some(geometry) = state.read(cx).scrollbar_geometry {
                    // 命中宽容与 markdown 编辑器一致（`GRAB_SLOP`）。
                    let hit = Bounds::new(
                        geometry.thumb.origin - point(GRAB_SLOP, GRAB_SLOP),
                        geometry.thumb.size + size(GRAB_SLOP * 2., GRAB_SLOP * 2.),
                    );
                    if hit.contains(&event.position) {
                        state.update(cx, |state, cx| {
                            state.scrollbar_drag =
                                Some((event.position.y, -state.scroll_handle.offset().y));
                            cx.notify();
                        });
                        window.prevent_default();
                        cx.stop_propagation();
                        return;
                    }
                }
                // 横向条。
                if let Some(geometry) = state.read(cx).scrollbar_geometry_h {
                    let hit = Bounds::new(
                        geometry.thumb.origin - point(GRAB_SLOP, GRAB_SLOP),
                        geometry.thumb.size + size(GRAB_SLOP * 2., GRAB_SLOP * 2.),
                    );
                    if hit.contains(&event.position) {
                        state.update(cx, |state, cx| {
                            state.scrollbar_drag_h =
                                Some((event.position.x, -state.scroll_handle.offset().x));
                            cx.notify();
                        });
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }
            }
        });
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseMoveEvent, _, _window, cx| {
                // 纵向拖拽。
                if let Some((start_y, start_scroll)) = state.read(cx).scrollbar_drag {
                    let Some(geometry) = state.read(cx).scrollbar_geometry else {
                        return;
                    };
                    // 内容不溢出时几何是退化的，`scroll_for_drag` 会算出 0 ——
                    // 直接不认，免得把滚动位置拍回顶端。
                    if (geometry.content_len - geometry.viewport_len).max(px(0.)) <= px(0.) {
                        return;
                    }
                    let delta = event.position.y - start_y;
                    // 与 markdown 编辑器共用同一份拖拽换算（同样扣掉轨道两端留白）。
                    let new_scroll = scroll_for_drag(
                        start_scroll,
                        delta,
                        geometry.viewport_len,
                        geometry.content_len,
                        geometry.thumb_len,
                    );
                    if new_scroll == start_scroll {
                        return;
                    }
                    state.update(cx, |state, cx| {
                        let mut offset = state.scroll_handle.offset();
                        offset.y = -new_scroll;
                        state.scroll_handle.set_offset(offset);
                        cx.notify();
                    });
                    return;
                }
                // 横向拖拽：换算同一套，坐标轴换成 x。
                let Some((start_x, start_scroll)) = state.read(cx).scrollbar_drag_h else {
                    return;
                };
                let Some(geometry) = state.read(cx).scrollbar_geometry_h else {
                    return;
                };
                if (geometry.content_len - geometry.viewport_len).max(px(0.)) <= px(0.) {
                    return;
                }
                let delta = event.position.x - start_x;
                let new_scroll = scroll_for_drag(
                    start_scroll,
                    delta,
                    geometry.viewport_len,
                    geometry.content_len,
                    geometry.thumb_len,
                );
                if new_scroll == start_scroll {
                    return;
                }
                state.update(cx, |state, cx| {
                    let mut offset = state.scroll_handle.offset();
                    offset.x = -new_scroll;
                    state.scroll_handle.set_offset(offset);
                    cx.notify();
                });
            }
        });
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseUpEvent, _, _window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }
                if state.read(cx).scrollbar_drag.is_some()
                    || state.read(cx).scrollbar_drag_h.is_some()
                {
                    state.update(cx, |state, cx| {
                        state.scrollbar_drag = None;
                        state.scrollbar_drag_h = None;
                        cx.notify();
                    });
                }
            }
        });

        window.on_mouse_event({
            let state = self.state.clone();

            move |event: &MouseMoveEvent, _, window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    state.update(cx, |state, cx| {
                        state.on_drag_move(event, window, cx);
                    });
                }
            }
        });
    }

    /// Prepare the state before layout.
    ///
    /// Handles pending updates (moved from InputState::render) and ensures
    /// the display map is ready for layout.
    fn prepare_if_needed(&mut self, window: &mut Window, cx: &mut App) {
        let editor_font = self.editor_font_settings(cx);
        self.state.update(cx, |state, cx| {
            if state._pending_update {
                let bg = state
                    .mode
                    .update_highlighter(&(0..0), &state.text, "", false, cx);
                if let Some(bg) = bg {
                    InputState::dispatch_background_parse(bg, state.text_version, window, cx);
                }
                state._pending_update = false;
            }
            // 字体解析：request_layout 阶段不能 push 文本样式栈（gpui 只允许
            // prepaint/paint 阶段调用 `with_text_style`），所以这里显式解析。
            // 代码编辑器用独立的「编辑器字体」，与容器文本样式解耦；非代码
            // 编辑器维持原语义（`state.font_size` 覆盖，否则用环境样式）。
            let ambient = window.text_style();
            let size = state.font_size.unwrap_or_else(|| match &editor_font {
                Some((_, size)) => *size,
                None => ambient.font_size.to_pixels(window.rem_size()),
            });
            let mut font = ambient.font();
            if let Some((family, _)) = &editor_font {
                font.family = family.clone();
            }
            state.display_map.set_font(font, size, cx);
            state.display_map.ensure_text_prepared(&state.text, cx);
        });
    }

    /// 代码编辑器的独立字体（`font.json` 的编辑器字体设置）：`(family, size)`。
    ///
    /// 非代码编辑器、或设置尚未初始化（`FontState` 全局未注册）时返回
    /// `None`，调用方回落到环境文本样式。
    fn editor_font_settings(&self, cx: &App) -> Option<(SharedString, Pixels)> {
        if !self.state.read(cx).mode.is_code_editor() {
            return None;
        }
        let font_state = cx.try_global::<FontState>()?;
        Some((
            font_state.settings.editor_font_family.clone().into(),
            px(font_state.settings.editor_font_size),
        ))
    }

    /// prepaint/paint 阶段的文本样式 refinement：把编辑器字体压到当前环境
    /// 样式之上。
    ///
    /// 关键约束：同一 `InputState` 可能从不同容器渲染（生产里是 `EditorInput`
    /// 的样式容器，测试里还有裸 `TextElement` 直绘）。若字体跟随容器，两条
    /// 路径会交替往 display map 写不同的字体，行高/装订线宽度逐帧抖动，
    /// 光标动画器会把它判成「布局变化」而瞬移收敛。字体由编辑器自身决定后，
    /// 无论从哪条路径渲染度量都一致。
    fn editor_font_refinement(&self, cx: &App) -> Option<TextStyleRefinement> {
        self.editor_font_settings(cx)
            .map(|(family, size)| TextStyleRefinement {
                font_family: Some(family),
                font_size: Some(size.into()),
                ..Default::default()
            })
    }

    /// Returns the:
    ///
    /// - cursor bounds
    /// - scroll offset
    /// - current row index (No only the visible lines, but all lines)
    ///
    /// This method also will update for track scroll to cursor.
    fn layout_cursor(
        &self,
        last_layout: &LastLayout,
        fold_placeholders: &[FoldPlaceholder],
        bounds: &mut Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> (Option<CursorLayout>, Point<Pixels>, Option<usize>) {
        let state = self.state.read(cx);

        let line_height = last_layout.line_height;
        let visible_range = &last_layout.visible_range;
        let lines = &last_layout.lines;
        let line_number_width = last_layout.line_number_width;

        let mut selected_range = state.selected_range;

        if let Some(ime_marked_range) = &state.ime_marked_range {
            selected_range = (ime_marked_range.end..ime_marked_range.end).into();
        }
        let is_selected_all = selected_range.len() == state.text.len();

        let mut cursor = state.cursor();
        if state.masked {
            selected_range.start = masked_display_offset(&state.text, selected_range.start);
            selected_range.end = masked_display_offset(&state.text, selected_range.end);
            cursor = masked_display_offset(&state.text, cursor);
        }

        let mut current_row = None;
        let mut scroll_offset = state.scroll_handle.offset();
        let mut cursor_bounds = None;

        // If the input has a fixed height (Otherwise is auto-grow), we need to add a bottom margin to the input.
        let top_bottom_margin =
            if state.mode.is_auto_grow() || visible_range.len() < BOTTOM_MARGIN_ROWS * 8 {
                line_height
            } else {
                BOTTOM_MARGIN_ROWS * line_height
            };

        // The cursor corresponds to the current cursor position in the text no only the line.
        let mut cursor_pos = None;
        let mut cursor_start = None;
        let mut cursor_end = None;

        let mut prev_lines_offset = 0;
        let mut offset_y = px(0.);
        // git 展开区：绝对坐标累计（从第 0 行起），锚点行上的块高先行。
        let insert_total = state
            .display_map
            .git_insertions()
            .iter()
            .map(|ins| ins.lines.len())
            .sum::<usize>();
        let mut prev_insert_rows = 0usize;
        let buffer_lines = state.display_map.lines();
        let visible_buffer_lines = &last_layout.visible_buffer_lines;
        let mut vi = 0; // index into visible_buffer_lines / lines
        for (ix, wrap_line) in buffer_lines.iter().enumerate() {
            let row = ix;
            // git 展开区：锚在本行上的块高先计入（line_origin 依赖它）。
            if insert_total > 0 {
                let rows_through = state.display_map.git_insertion_rows_before(ix + 1);
                offset_y += line_height * (rows_through.saturating_sub(prev_insert_rows)) as f32;
                prev_insert_rows = rows_through;
            }
            let line_origin = point(px(0.), offset_y);

            // break loop if all cursor positions are found
            if cursor_pos.is_some() && cursor_start.is_some() && cursor_end.is_some() {
                break;
            }

            // Check if this buffer line has a LineLayout in the compact lines vec
            let line_layout = if vi < visible_buffer_lines.len() && visible_buffer_lines[vi] == ix {
                let l = &lines[vi];
                vi += 1;
                Some(l)
            } else {
                None
            };

            if let Some(line) = line_layout {
                if cursor_pos.is_none() {
                    let offset = cursor.saturating_sub(prev_lines_offset);
                    if let Some(pos) =
                        line.position_for_index(offset, last_layout, state.cursor_line_end_affinity)
                    {
                        current_row = Some(row);
                        cursor_pos = Some(line_origin + pos);
                    }
                }
                if cursor_start.is_none() {
                    let offset = selected_range.start.saturating_sub(prev_lines_offset);
                    if let Some(pos) = line.position_for_index(offset, last_layout, false) {
                        cursor_start = Some(line_origin + pos);
                    }
                }
                if cursor_end.is_none() {
                    let offset = selected_range.end.saturating_sub(prev_lines_offset);
                    if let Some(pos) = line.position_for_index(offset, last_layout, false) {
                        cursor_end = Some(line_origin + pos);
                    }
                }

                offset_y += line.size(line_height).height;
                // +1 for the last `\n`
                prev_lines_offset += wrap_line.len() + 1;
            } else {
                // Not visible (before visible range or hidden/folded).
                // Just increase the offset_y and prev_lines_offset for scroll tracking.
                //
                // 本行"覆盖"某个偏移，当且仅当它落在
                // `[行首, 行尾含换行符)` 之内。此前判据写成
                // `prev_lines_offset >= target`（取第一个行首 ≥ 目标的行），
                // 命中的其实是光标所在行的**下一行**；更糟的是当光标落在折叠
                // 的最后一个隐藏行时，那一行不满足判据，循环会一直走到折叠后
                // 的第一个可见行，而可见分支用的是 `saturating_sub`，任何早于
                // 该行的光标都会被算成它的第 0 列 —— 当前行高亮因此跳出折叠，
                // 落在折叠之后那一行上。
                let contains_offset = |target: usize| {
                    target >= prev_lines_offset && target < prev_lines_offset + wrap_line.len() + 1
                };
                if contains_offset(cursor) && cursor_pos.is_none() {
                    current_row = Some(row);
                    // A cursor inside a fold — or at its end (the visible tail,
                    // e.g. `}`) — is drawn on the fold header's display row:
                    // at the `⋯` placeholder, or right after the placeholder
                    // and tail when at the fold end (zed maps positions inside
                    // a fold to the placeholder).
                    let display_fold = state
                        .display_map
                        .fold_containing(cursor)
                        .or_else(|| state.display_map.fold_ending_at(cursor));
                    cursor_pos = display_fold.and_then(|fold| {
                        let line = last_layout.line(fold.start.row)?;
                        let placeholder =
                            fold_placeholders.iter().find(|p| p.row == fold.start.row)?;
                        let last_y = offset_y - line_height;
                        let last_width =
                            line.wrapped_lines.last().map(|l| l.width).unwrap_or(px(0.));
                        let x = if cursor >= fold.end_offset {
                            let mut x = last_width
                                + placeholder.ellipsis.width
                                + FOLD_PLACEHOLDER_PADDING * 2.;
                            if let Some(tail) = placeholder.tail.as_ref() {
                                x += tail.width;
                            }
                            x
                        } else {
                            last_width
                        };
                        Some(point(x, last_y))
                    });
                    if cursor_pos.is_none() {
                        cursor_pos = Some(line_origin);
                    }
                }
                // 与 `cursor` 同一套包含判据，选区两端才不会各自偏到下一行。
                if contains_offset(selected_range.start) && cursor_start.is_none() {
                    cursor_start = Some(line_origin);
                }
                if contains_offset(selected_range.end) && cursor_end.is_none() {
                    cursor_end = Some(line_origin);
                }

                let visible_wrap_rows =
                    state.display_map.visible_wrap_row_count_for_buffer_line(ix);
                offset_y += line_height * visible_wrap_rows;
                // +1 for the last `\n`
                prev_lines_offset += wrap_line.len() + 1;
            }
        }

        if let (Some(cursor_pos), Some(cursor_start), Some(cursor_end)) =
            (cursor_pos, cursor_start, cursor_end)
        {
            let selection_changed = state.last_selected_range != Some(selected_range);
            if selection_changed && !is_selected_all {
                // Apart from left alignment, just leave enough space for the cursor size on the right side.
                let safety_margin = if last_layout.text_align == TextAlign::Left {
                    RIGHT_MARGIN
                } else {
                    CURSOR_WIDTH
                };

                scroll_offset.x = if scroll_offset.x + cursor_pos.x
                    > (bounds.size.width - line_number_width - safety_margin)
                {
                    // cursor is out of right
                    bounds.size.width - line_number_width - safety_margin - cursor_pos.x
                } else if scroll_offset.x + cursor_pos.x < px(0.) {
                    // cursor is out of left
                    scroll_offset.x - cursor_pos.x
                } else {
                    scroll_offset.x
                };

                // If we change the scroll_offset.y, GPUI will render and trigger the next run loop.
                // So, here we just adjust offset by `line_height` for move smooth.
                scroll_offset.y =
                    if scroll_offset.y + cursor_pos.y > bounds.size.height - top_bottom_margin {
                        // cursor is out of bottom
                        scroll_offset.y - line_height
                    } else if scroll_offset.y + cursor_pos.y < top_bottom_margin {
                        // cursor is out of top
                        (scroll_offset.y + line_height).min(px(0.))
                    } else {
                        scroll_offset.y
                    };

                // For selection to move scroll
                if state.selection_reversed {
                    if scroll_offset.x + cursor_start.x < px(0.) {
                        // selection start is out of left
                        scroll_offset.x = -cursor_start.x;
                    }
                    if scroll_offset.y + cursor_start.y < px(0.) {
                        // selection start is out of top
                        scroll_offset.y = -cursor_start.y;
                    }
                } else {
                    // TODO: Consider to remove this part,
                    // maybe is not necessary (But selection_reversed is needed).
                    if scroll_offset.x + cursor_end.x <= px(0.) {
                        // selection end is out of left
                        scroll_offset.x = -cursor_end.x;
                    }
                    if scroll_offset.y + cursor_end.y <= px(0.) {
                        // selection end is out of top
                        scroll_offset.y = -cursor_end.y;
                    }
                }
            }

            let shape = state.effective_cursor_shape();
            let masked = state.masked;
            let font_size_opt = state.font_size;
            let cursor_char: Option<char> = if masked {
                Some(MASK_CHAR)
            } else if cursor < state.text.len() {
                state.text.char_at(cursor)
            } else {
                None
            };
            let deferred = state.deferred_scroll_offset;
            let mapped_row =
                current_row.map(|row| state.display_map.fold_header_for_row(row).unwrap_or(row));
            if let Some(deferred_offset) = deferred {
                scroll_offset = deferred_offset;
            }
            current_row = mapped_row;
            let cursor_origin = point(
                bounds.left() + cursor_pos.x + line_number_width + scroll_offset.x,
                bounds.top() + cursor_pos.y,
            );
            // `state` borrow ends here (NLL); block shaping uses `window`/`cx` mutably.
            let (block_width, block_text) = Self::cursor_block_layout_static(
                shape,
                cursor_char,
                font_size_opt,
                line_height,
                window,
                cx,
            );
            let cursor_color = cx.theme().accent;
            cursor_bounds = Some(CursorLayout::new(
                cursor_origin,
                block_width,
                line_height,
                cursor_color,
                shape,
                block_text,
            ));
        } else {
            if let Some(deferred_scroll_offset) = state.deferred_scroll_offset {
                scroll_offset = deferred_scroll_offset;
            }
            // zed tracks the "active row" in display space: when the cursor sits
            // on a row hidden by a fold (e.g. at the fold end, on the closing
            // bracket row), the fold's header row is the active one — it receives
            // the active-line highlight and shows the fold disclosure icon.
            current_row =
                current_row.map(|row| state.display_map.fold_header_for_row(row).unwrap_or(row));
        }

        // Non-cursor path already handled fold mapping above; cursor path did it
        // before shaping to end the `state` borrow (see NLL comment).

        bounds.origin += scroll_offset;

        (cursor_bounds, scroll_offset, current_row)
    }

    fn cursor_block_layout_static(
        shape: CursorShape,
        cursor_char: Option<char>,
        font_size_opt: Option<Pixels>,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> (Pixels, Option<ShapedLine>) {
        if !matches!(
            shape,
            CursorShape::Block | CursorShape::Hollow | CursorShape::Underline
        ) {
            return (CURSOR_WIDTH, None);
        }

        let style = window.text_style();
        let font_size =
            font_size_opt.unwrap_or_else(|| style.font_size.to_pixels(window.rem_size()));
        // 一个等宽字符格的宽度。
        //
        // 用字体的 `ch` 前进宽度而不是 `line_height * 0.6`：后者是随行高估算的
        // 经验值，在常见的 1.6 倍行高下约为单元格的 1.6 倍，块状光标会溢出到
        // 下一个字符上。取不到字体度量时才退回估算值。
        let font_id = window.text_system().resolve_font(&style.font());
        let cell_width = window
            .text_system()
            .ch_advance(font_id, font_size)
            .ok()
            .filter(|width| *width > px(0.))
            .unwrap_or_else(|| (line_height * 0.6).max(CURSOR_WIDTH));

        // 行尾 / 空行没有可覆盖的字符，按一格宽画。
        let Some(ch) = cursor_char else {
            return (cell_width, None);
        };
        if ch == '\n' || ch == '\r' {
            return (cell_width, None);
        }

        let editor_bg = cx.theme().editor_background();
        let text_color = if editor_bg.a < 0.75 {
            if cx.theme().appearance.is_dark() {
                gpui::black()
            } else {
                gpui::white()
            }
        } else {
            editor_bg
        };
        let s: SharedString = ch.to_string().into();
        let shaped = window.text_system().shape_line(
            s.clone(),
            font_size,
            &[TextRun {
                len: s.len(),
                font: style.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        // 块状光标必须正好盖住**字符自身的格宽**。
        //
        // 格宽由 `unicode-width` 的字符宽度决定，而不是靠字体度量：
        // 主字体没有该字形时（如 Menlo 遇到中文）`advance` 会失败，
        // `shaped.width` 在无回退字体的环境里也只有一格，两者都不可靠。
        // 终端侧（`terminal_element.rs`）用的是同一个判定，这里保持一致。
        let cells = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1) as f32;
        let width = if ch.is_ascii_whitespace() {
            cell_width
        } else {
            cell_width * cells
        };
        (width, Some(shaped))
    }

    /// Layout the match range to a Path.
    pub(crate) fn layout_match_range(
        range: Range<usize>,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
    ) -> Option<Path<Pixels>> {
        if range.is_empty() {
            return None;
        }

        if range.start < last_layout.visible_range_offset.start
            || range.end > last_layout.visible_range_offset.end
        {
            return None;
        }

        let line_height = last_layout.line_height;
        let visible_top = last_layout.visible_top;
        let lines = &last_layout.lines;
        let line_number_width = last_layout.line_number_width;

        let start_ix = range.start;
        let end_ix = range.end;

        // Start from visible_top (which already accounts for all lines before visible range)
        let mut offset_y = visible_top;
        let mut line_corners = vec![];

        // Iterate only over visible (non-hidden) buffer lines
        for (vi, (prev_lines_offset, line)) in last_layout
            .visible_line_byte_offsets
            .iter()
            .zip(lines.iter())
            .enumerate()
        {
            // git 展开区：锚在当前行上的块高先行（与 paint 阶段一致）。
            offset_y += line_height * last_layout.git_insert_delta(vi) as f32;

            let prev_lines_offset = *prev_lines_offset;
            let line_size = line.size(line_height);
            let line_wrap_width = line_size.width;

            let line_origin = point(px(0.), offset_y);

            let line_cursor_start = line.position_for_index(
                start_ix.saturating_sub(prev_lines_offset),
                last_layout,
                false,
            );
            let line_cursor_end = line.position_for_index(
                end_ix.saturating_sub(prev_lines_offset),
                last_layout,
                false,
            );

            if line_cursor_start.is_some() || line_cursor_end.is_some() {
                let start = line_cursor_start.unwrap_or_else(|| {
                    line.position_for_index(0, last_layout, false)
                        .unwrap_or(point(px(0.), px(0.)))
                });

                let end = line_cursor_end.unwrap_or_else(|| {
                    line.position_for_index(line.len(), last_layout, false)
                        .unwrap_or(point(px(0.), px(0.)))
                });

                // Split the selection into multiple items
                let wrapped_lines =
                    (end.y / line_height).ceil() as usize - (start.y / line_height).ceil() as usize;

                let mut end_x = end.x;
                if wrapped_lines > 0 {
                    end_x = line_wrap_width;
                }

                // Ensure at least 6px width for the selection for empty lines.
                end_x = end_x.max(start.x + px(6.));

                line_corners.push(Corners {
                    top_left: line_origin + point(start.x, start.y),
                    top_right: line_origin + point(end_x, start.y),
                    bottom_left: line_origin + point(start.x, start.y + line_height),
                    bottom_right: line_origin + point(end_x, start.y + line_height),
                });

                // wrapped lines
                for i in 1..=wrapped_lines {
                    let start = point(px(0.), start.y + i as f32 * line_height);
                    let mut end = point(end.x, end.y + i as f32 * line_height);
                    if i < wrapped_lines {
                        end.x = line_size.width;
                    }

                    line_corners.push(Corners {
                        top_left: line_origin + point(start.x, start.y),
                        top_right: line_origin + point(end.x, start.y),
                        bottom_left: line_origin + point(start.x, start.y + line_height),
                        bottom_right: line_origin + point(end.x, start.y + line_height),
                    });
                }
            }

            if line_cursor_start.is_some() && line_cursor_end.is_some() {
                break;
            }

            offset_y += line_size.height;
        }

        let mut points = vec![];
        if line_corners.is_empty() {
            return None;
        }

        // Fix corners to make sure the left to right direction
        for corners in &mut line_corners {
            if corners.top_left.x > corners.top_right.x {
                std::mem::swap(&mut corners.top_left, &mut corners.top_right);
                std::mem::swap(&mut corners.bottom_left, &mut corners.bottom_right);
            }
        }

        for corners in &line_corners {
            points.push(corners.top_right);
            points.push(corners.bottom_right);
            points.push(corners.bottom_left);
        }

        let mut rev_line_corners = line_corners.iter().rev().peekable();
        while let Some(corners) = rev_line_corners.next() {
            points.push(corners.top_left);
            if let Some(next) = rev_line_corners.peek() {
                if next.top_left.x > corners.top_left.x {
                    points.push(point(next.top_left.x, corners.top_left.y));
                }
            }
        }

        let path_origin = bounds.origin + point(line_number_width, px(0.));
        let n = points.len();
        if n < 3 {
            return None;
        }

        let corner_radius = px(4.);
        let r = f32::from(corner_radius);

        // Determine which vertices are convex (exposed outer corners) using cross product.
        // The polygon is counterclockwise, so convex corners have positive cross product.
        let is_convex: Vec<bool> = (0..n)
            .map(|i| {
                let prev = points[(i + n - 1) % n];
                let curr = points[i];
                let next = points[(i + 1) % n];
                let cross = (f32::from(curr.x) - f32::from(prev.x))
                    * (f32::from(next.y) - f32::from(prev.y))
                    - (f32::from(curr.y) - f32::from(prev.y))
                        * (f32::from(next.x) - f32::from(prev.x));
                cross > 0.
            })
            .collect();

        // Build path with rounded corners at convex vertices
        let mut builder = gpui::PathBuilder::fill();

        // For vertex[0], compute the arrive point and move there directly
        // so that close() draws the closing edge correctly.
        if is_convex[0] {
            let prev = points[n - 1];
            let curr = points[0];
            let dx = f32::from(curr.x) - f32::from(prev.x);
            let dy = f32::from(curr.y) - f32::from(prev.y);
            let len = (dx * dx + dy * dy).sqrt();
            if len > 0. {
                let ux = dx / len;
                let uy = dy / len;
                builder.move_to(path_origin + point(curr.x - px(ux * r), curr.y - px(uy * r)));
            } else {
                builder.move_to(path_origin + curr);
            }
        } else {
            builder.move_to(path_origin + points[0]);
        }

        for i in 0..n {
            let curr = points[i];
            let next = points[(i + 1) % n];

            if is_convex[i] {
                let prev = points[(i + n - 1) % n];
                let dx_in = f32::from(curr.x) - f32::from(prev.x);
                let dy_in = f32::from(curr.y) - f32::from(prev.y);
                let len_in = (dx_in * dx_in + dy_in * dy_in).sqrt();
                if len_in > 0. {
                    let ux_in = dx_in / len_in;
                    let uy_in = dy_in / len_in;
                    let start = point(curr.x - px(ux_in * r), curr.y - px(uy_in * r));

                    let dx_out = f32::from(next.x) - f32::from(curr.x);
                    let dy_out = f32::from(next.y) - f32::from(curr.y);
                    let len_out = (dx_out * dx_out + dy_out * dy_out).sqrt();
                    if len_out > 0. {
                        let ux_out = dx_out / len_out;
                        let uy_out = dy_out / len_out;
                        let end = point(curr.x + px(ux_out * r), curr.y + px(uy_out * r));

                        // Skip line_to for vertex[0] since move_to already placed us at start
                        if i > 0 {
                            builder.line_to(path_origin + start);
                        }
                        builder.curve_to(path_origin + end, path_origin + curr);
                    } else {
                        if i > 0 {
                            builder.line_to(path_origin + curr);
                        }
                    }
                } else if i > 0 {
                    builder.line_to(path_origin + curr);
                }
            } else if i > 0 {
                builder.line_to(path_origin + curr);
            }
        }

        builder.build().ok()
    }

    fn layout_selections(
        &self,
        last_layout: &LastLayout,
        bounds: &mut Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Path<Pixels>> {
        let state = self.state.read(cx);
        if !state.focus_handle.is_focused(window) {
            return None;
        }

        let mut selected_range = state.selected_range;
        if let Some(ime_marked_range) = &state.ime_marked_range {
            if !ime_marked_range.is_empty() {
                selected_range = (ime_marked_range.end..ime_marked_range.end).into();
            }
        }
        if selected_range.is_empty() {
            return None;
        }

        if state.masked {
            selected_range.start = masked_display_offset(&state.text, selected_range.start);
            selected_range.end = masked_display_offset(&state.text, selected_range.end);
        }

        let (start_ix, end_ix) = if selected_range.start < selected_range.end {
            (selected_range.start, selected_range.end)
        } else {
            (selected_range.end, selected_range.start)
        };

        let range = start_ix.max(last_layout.visible_range_offset.start)
            ..end_ix.min(last_layout.visible_range_offset.end);

        Self::layout_match_range(range, last_layout, bounds)
    }

    /// Layout highlight paths for every search match in the visible range.
    /// Returns (path, is_current_match); the current match is painted with
    /// the selection color on top of the weaker match color.
    fn layout_search_matches(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &mut App,
    ) -> Vec<(Path<Pixels>, bool)> {
        let state = self.state.read(cx);
        let Some(search_panel) = state.search_panel.clone() else {
            return Vec::new();
        };

        let Some(matcher) = search_panel.read(cx).matcher() else {
            return Vec::new();
        };
        let ranges = matcher.matched_ranges.clone();
        let current_match_ix = matcher.current_match_ix;

        let mut paths = Vec::with_capacity(ranges.as_ref().len());
        for (index, range) in ranges.as_ref().iter().enumerate() {
            if let Some(path) = Self::layout_match_range(range.clone(), last_layout, bounds) {
                paths.push((path, current_match_ix == index));
            }
        }
        paths
    }

    /// 括号着色调色板：按嵌套层级循环取色，暗/亮主题各一套。
    fn rainbow_bracket_color(depth: usize, dark: bool) -> Hsla {
        let hex: u32 = if dark {
            match depth % 6 {
                0 => 0xE2B93D, // 金
                1 => 0x4FC1FF, // 蓝
                2 => 0xE06FB8, // 粉
                3 => 0x61D4B8, // 青
                4 => 0xC88B48, // 橙
                _ => 0xA78BFA, // 紫
            }
        } else {
            match depth % 6 {
                0 => 0xB8860B,
                1 => 0x0366D6,
                2 => 0xC4529E,
                3 => 0x0F9B8E,
                4 => 0xB26A1B,
                _ => 0x7C5CBF,
            }
        };
        gpui::rgb(hex).into()
    }

    /// Sticky scroll 布局：首可见行的外围定义行单独 shape（带语法色），
    /// 返回外→内序的钉性行。点击区域同时计算好供 paint 阶段注册。
    #[allow(clippy::too_many_arguments)]
    fn layout_sticky_lines(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        base_style: &TextStyle,
        text_size: Pixels,
        line_height: Pixels,
        line_number_len: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<StickyLine> {
        let (sticky_rows, text, highlighter, highlight_theme) = {
            let state = self.state.read(cx);
            let sticky_rows = state.sticky_lines(last_layout.visible_range.start);
            (
                sticky_rows,
                state.text.clone(),
                match &state.mode {
                    InputMode::CodeEditor { highlighter, .. } => highlighter.clone(),
                    _ => return Vec::new(),
                },
                cx.theme().highlight_theme.clone(),
            )
        };
        if sticky_rows.is_empty() {
            return Vec::new();
        }

        // 高度限制：至少留出一行内容可见
        let available = bounds.size.height - line_height;
        let fit_rows = ((available / line_height).floor().max(0.)) as usize;
        let sticky_rows = if sticky_rows.len() > fit_rows {
            // 保留最内侧（最靠近可见区）的行
            let keep = sticky_rows.len() - fit_rows;
            sticky_rows[keep..].to_vec()
        } else {
            sticky_rows
        };
        if sticky_rows.is_empty() {
            return Vec::new();
        }

        let number_runs = |color: Hsla| {
            vec![TextRun {
                len: line_number_len,
                font: base_style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }]
        };
        let number_muted_runs = number_runs(cx.theme().text_muted);

        let mut sticky_lines = Vec::with_capacity(sticky_rows.len());
        for (ix, &buffer_line) in sticky_rows.iter().enumerate() {
            let line_start = text.line_start_offset(buffer_line);
            let line_end = text.line_end_offset(buffer_line);
            let mut line_text: String = text.slice(line_start..line_end).into();
            // 过长截断（按字符，避免拆 UTF-8）
            if line_text.chars().count() > 200 {
                line_text = line_text.chars().take(200).collect();
            }
            let text_bytes = line_text.len();

            // 语法高亮 runs（绝对范围 → 行内相对）
            let mut runs: Vec<TextRun> = Vec::new();
            {
                let mut hl = highlighter.borrow_mut();
                if let Some(hl) = hl.as_mut() {
                    for (range, style) in hl.styles(&(line_start..line_end), &highlight_theme) {
                        let start = range.start.max(line_start) - line_start;
                        let end = (range.end.min(line_start + text_bytes)) - line_start;
                        if end > start {
                            runs.push(base_style.clone().highlight(style).to_run(end - start));
                        }
                    }
                }
            }
            if runs.is_empty() {
                runs.push(base_style.clone().to_run(text_bytes));
            }
            let runs_len: usize = runs.iter().map(|r| r.len).sum();
            if runs_len < text_bytes {
                runs.push(base_style.clone().to_run(text_bytes - runs_len));
            }

            let shaped_text = window.text_system().shape_line(
                SharedString::from(line_text),
                text_size,
                &runs,
                None,
            );
            let number: SharedString =
                format!("{:>width$}", buffer_line + 1, width = line_number_len).into();
            let shaped_number =
                window
                    .text_system()
                    .shape_line(number, text_size, &number_muted_runs, None);

            let row_bounds = Bounds::new(
                point(bounds.origin.x, bounds.origin.y + line_height * ix as f32),
                size(bounds.size.width, line_height),
            );
            sticky_lines.push(StickyLine {
                buffer_line,
                text: shaped_text,
                number: shaped_number,
                bounds: row_bounds,
            });
        }
        sticky_lines
    }

    /// 括号着色：把可见范围内的括号字符按嵌套层级覆盖为 rainbow 色。
    ///
    /// 优先用 tree-sitter 树扫描（字符串/注释内的括号不参与计数，避免深度被污染），
    /// 无树时退回纯文本扫描；配对算法一致：类型配对 + 交错容忍，未配对标错误色。
    /// 结果按 (text_version, 树指纹) 缓存，树后台重解析落地后自动失效。
    /// 彩虹括号 span 的共享获取（命中缓存或重算并写回）。
    ///
    /// 可见行合并（[`Self::merge_rainbow_brackets`]）与折叠 tail（收尾行
    /// `}`，见 `layout_fold_placeholders`）共用同一份 span 与层级色，保证
    /// 折叠占位符两侧的括号颜色一致。返回 `None` 表示彩虹括号未启用
    /// （关闭 / 单行 / masked / diff 视图 / 超长文本），调用方保持原语法色。
    ///
    /// 注意：命中失败时会写回缓存（`state.update`），调用点不得持有
    /// `self.state.read(cx)` 的借用。
    fn rainbow_bracket_spans(&self, cx: &mut App) -> Option<Rc<Vec<BracketSpan>>> {
        let (enabled, text, version, highlighter) = {
            let state = self.state.read(cx);
            (
                state.rainbow_brackets
                    && state.mode.is_multi_line()
                    && !state.masked
                    && state.external_highlight_runs.is_none(),
                state.text.clone(),
                state.text_version,
                match &state.mode {
                    InputMode::CodeEditor { highlighter, .. } => Some(highlighter.clone()),
                    _ => None,
                },
            )
        };
        if !enabled || text.len() > RAINBOW_BRACKETS_MAX_TEXT_LEN {
            return None;
        }

        // 树指纹：树被替换（后台解析落地/换语言）时变化，缓存随之失效
        let tree_key = highlighter.as_ref().and_then(|h| {
            h.borrow()
                .as_ref()
                .and_then(|h| h.tree().map(|t| t.root_node().id()))
        });
        let cached = {
            let state = self.state.read(cx);
            match &state.rainbow_cache {
                Some(cache)
                    if cache.text_version == version && cache.tree_fingerprint == tree_key =>
                {
                    Some(cache.spans.clone())
                }
                _ => None,
            }
        };
        match cached {
            Some(spans) => Some(spans),
            None => {
                let computed = match highlighter
                    .as_ref()
                    .and_then(|h| h.borrow().as_ref().and_then(|h| h.tree().cloned()))
                {
                    Some(tree) => scan_bracket_spans_tree(&tree),
                    None => scan_bracket_spans_text(&text),
                };
                let computed = Rc::new(computed);
                self.state.update(cx, |state, _| {
                    state.rainbow_cache = Some(RainbowCache {
                        text_version: version,
                        tree_fingerprint: tree_key,
                        spans: computed.clone(),
                    });
                });
                Some(computed)
            }
        }
    }

    /// 把彩虹括号 span 中与 `range` 重叠的部分转成覆盖样式
    ///（配对/未闭合括号取层级色，游离闭括号标错误色）。
    fn rainbow_overrides_in_range(
        spans: &[BracketSpan],
        range: &Range<usize>,
        dark: bool,
        error_color: Hsla,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        spans
            .iter()
            .filter(|(r, _, _, _)| r.start < range.end && r.end > range.start)
            .map(|(r, d, is_open, matched)| {
                let color = if *matched || *is_open {
                    Self::rainbow_bracket_color(*d, dark)
                } else {
                    error_color
                };
                (
                    r.clone(),
                    HighlightStyle {
                        color: Some(color),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    fn merge_rainbow_brackets(
        &self,
        styles: Option<Vec<(Range<usize>, HighlightStyle)>>,
        visible: &Range<usize>,
        cx: &mut App,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        let mut styles = styles?;
        let Some(spans) = self.rainbow_bracket_spans(cx) else {
            return Some(styles);
        };

        let dark = cx.theme().appearance.is_dark();
        let error_color = cx.theme().error;
        let rainbow =
            Self::rainbow_overrides_in_range(spans.iter().as_slice(), visible, dark, error_color);
        if rainbow.is_empty() {
            return Some(styles);
        }
        styles = combine_highlights_stable(styles, rainbow);
        Some(styles)
    }

    /// 同词高亮：光标处词的全文件出现，返回可见范围内的背景路径。
    fn layout_occurrences(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<Path<Pixels>> {
        let focused = self.state.read(cx).focus_handle.is_focused(window);
        if !focused {
            return Vec::new();
        }
        let ranges = self.state.update(cx, |state, _| state.occurrence_ranges());
        let Some(ranges) = ranges else {
            return Vec::new();
        };
        let mut paths = Vec::with_capacity(ranges.len());
        for range in ranges.iter() {
            if let Some(path) = Self::layout_match_range(range.clone(), last_layout, bounds) {
                paths.push(path);
            }
        }
        paths
    }

    /// 布局括号匹配高亮：光标紧邻括号时，为开/闭括号各生成一个背景路径。
    fn layout_bracket_matches(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        window: &Window,
        cx: &mut App,
    ) -> Vec<Path<Pixels>> {
        let (is_single, is_masked, has_sel, focused, cursor, text, tree) = {
            let state = self.state.read(cx);
            let tree = match &state.mode {
                InputMode::CodeEditor { highlighter, .. } => highlighter
                    .borrow()
                    .as_ref()
                    .and_then(|h| h.tree().cloned()),
                _ => None,
            };
            (
                state.mode.is_single_line(),
                state.masked,
                !state.selected_range.is_empty(),
                state.focus_handle.is_focused(window),
                state.cursor(),
                state.text.clone(),
                tree,
            )
        };
        if is_single || is_masked {
            return Vec::new();
        }
        if has_sel {
            return Vec::new();
        }
        if !focused {
            return Vec::new();
        }
        let Some((open, close)) = InputState::matching_bracket(&text, cursor) else {
            return Vec::new();
        };
        // 有语法树时过滤字符串/注释内的误匹配（纯字节扫描无法区分）。
        if let Some(tree) = tree.as_ref() {
            let ok = [open, close].iter().all(|off| {
                text.get_byte(*off)
                    .is_some_and(|b| is_code_bracket(tree, *off, b))
            });
            if !ok {
                return Vec::new();
            }
        }
        let mut paths = Vec::with_capacity(2);
        for off in [open, close] {
            if let Some(path) = Self::layout_match_range(off..off + 1, last_layout, bounds) {
                paths.push(path);
            }
        }
        paths
    }

    /// Calculate the visible range of lines in the viewport.
    ///
    /// Returns
    ///
    /// - visible_range: The visible range is based on unwrapped lines (Zero based).
    /// - visible_buffer_lines: Indices of non-hidden buffer lines within the visible range.
    /// - visible_top: The top position of the first visible line in the scroll viewport.
    fn calculate_visible_range(
        &self,
        state: &InputState,
        line_height: Pixels,
        input_height: Pixels,
    ) -> (Range<usize>, Vec<usize>, Pixels) {
        // Add extra rows to avoid showing empty space when scroll to bottom.
        let extra_rows = 1;
        let mut visible_top = px(0.);
        if state.mode.is_single_line() {
            return (0..1, vec![0], visible_top);
        }

        let total_lines = state.display_map.wrap_row_count();
        let scroll_top = if let Some(deferred_scroll_offset) = state.deferred_scroll_offset {
            deferred_scroll_offset.y
        } else {
            state.scroll_handle.offset().y
        };

        let mut visible_range = 0..total_lines;
        let mut line_bottom = px(0.);
        // git 展开区：块显示在锚点行上方（间隙归属锚点行），累计时先加块高
        // 再加行高；`rows_through(ix)` 计锚点 ≤ ix 的块行。
        let has_insertions = !state.display_map.git_insertions().is_empty();
        let mut prev_insertion_rows = 0usize;
        for (ix, _line) in state.display_map.lines().iter().enumerate() {
            let visible_wrap_rows = state.display_map.visible_wrap_row_count_for_buffer_line(ix);

            if visible_wrap_rows == 0 {
                continue;
            }

            let rows_through = if has_insertions {
                state.display_map.git_insertion_rows_before(ix + 1)
            } else {
                0
            };
            let block_height =
                line_height * (rows_through.saturating_sub(prev_insertion_rows)) as f32;
            prev_insertion_rows = rows_through;

            let wrapped_height = line_height * visible_wrap_rows;
            line_bottom += block_height + wrapped_height;

            if line_bottom < -scroll_top {
                // 行顶（不含锚在本行上的块——块从更上方开始画，超界行由
                // 绘制阶段按视口裁剪）。
                visible_top = line_bottom - wrapped_height - block_height;
                visible_range.start = ix;
            }

            if line_bottom + scroll_top >= input_height {
                visible_range.end = (ix + extra_rows).min(total_lines);
                break;
            }
        }

        // Collect non-hidden buffer lines within the visible range
        let mut visible_buffer_lines = Vec::with_capacity(visible_range.len());
        for ix in visible_range.start..visible_range.end {
            let visible_wrap_rows = state.display_map.visible_wrap_row_count_for_buffer_line(ix);
            if visible_wrap_rows > 0 {
                visible_buffer_lines.push(ix);
            }
        }

        (visible_range, visible_buffer_lines, visible_top)
    }

    /// Return (line_number_width, line_number_len)
    fn layout_line_numbers(
        state: &InputState,
        text: &Rope,
        font_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
    ) -> (Pixels, usize) {
        let total_lines = text.lines_len();
        let line_number_len = match total_lines {
            0..=9999 => 5,
            10000..=99999 => 6,
            100000..=999999 => 7,
            _ => 8,
        };

        let mut line_number_width = if state.mode.line_number() {
            let empty_line_number = window.text_system().shape_line(
                "+".repeat(line_number_len).into(),
                font_size,
                &[TextRun {
                    len: line_number_len,
                    font: style.font(),
                    color: style.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );

            empty_line_number.width + LINE_NUMBER_RIGHT_MARGIN
        } else if state.mode.is_code_editor() && state.mode.is_multi_line() {
            LINE_NUMBER_RIGHT_MARGIN
        } else {
            px(0.)
        };

        if state.mode.is_folding() {
            // Add extra space for fold icons
            line_number_width += FOLD_ICON_HITBOX_WIDTH
        }

        (line_number_width, line_number_len)
    }

    /// Layout shaped lines for whitespace indicators (space and tab).
    ///
    /// Returns `WhitespaceIndicators` with shaped lines for space and tab characters.
    fn layout_whitespace_indicators(
        state: &InputState,
        text_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
        cx: &App,
    ) -> Option<WhitespaceIndicators> {
        if !state.show_whitespaces {
            return None;
        }

        let invisible_color = cx
            .theme()
            .highlight_theme
            .style
            .editor_invisible
            .unwrap_or(cx.theme().text_muted);

        let space_font_size = text_size.half();
        let tab_font_size = text_size;

        let space_text = SharedString::new_static("•");
        let space = window.text_system().shape_line(
            space_text.clone(),
            space_font_size,
            &[TextRun {
                len: space_text.len(),
                font: style.font(),
                color: invisible_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );

        let tab_text = SharedString::new_static("→");
        let tab = window.text_system().shape_line(
            tab_text.clone(),
            tab_font_size,
            &[TextRun {
                len: tab_text.len(),
                font: style.font(),
                color: invisible_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );

        Some(WhitespaceIndicators { space, tab })
    }

    /// Layout the gutter fold disclosure icons (zed's crease toggles).
    ///
    /// Mirrors zed's `EditorElement::layout_crease_toggles`: every visible
    /// row that is folded or starts an indent gets an icon hitbox in the
    /// fold area at the right of the line numbers; whether the icon is
    /// painted is decided in `paint_fold_icons` following zed's
    /// `render_crease_toggle` visibility rule.
    fn layout_fold_icons(
        &self,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        window: &mut Window,
        cx: &mut App,
    ) -> FoldIconLayout {
        let line_number_hitbox = window.insert_hitbox(
            Bounds::new(
                bounds.origin + point(px(0.), last_layout.visible_top),
                size(last_layout.line_number_width, bounds.size.height),
            ),
            HitboxBehavior::Normal,
        );

        let mut icon_layout = FoldIconLayout {
            line_number_hitbox,
            icons: vec![],
        };

        let state = self.state.read(cx);
        if !state.mode.is_folding() {
            return icon_layout;
        }

        let line_height = last_layout.line_height;
        let icon_size = fold_icon_size(line_height);
        let mut offset_y = last_layout.visible_top;
        for (i, &buffer_line) in last_layout.visible_buffer_lines.iter().enumerate() {
            let height = last_layout
                .lines
                .get(i)
                .map(|line| line.size(line_height).height)
                .unwrap_or(line_height);

            // git 展开区：锚点行上的块高先行（与正文累计一致）。
            offset_y += line_height * last_layout.git_insert_delta(i) as f32;

            // zed: `is_foldable |= !use_lsp_folding_ranges && starts_indent(row)`.
            let folded = state.display_map.is_line_folded(buffer_line);
            let is_foldable = folded || state.display_map.starts_indent(&state.text, buffer_line);
            if is_foldable {
                // Center the icon in the fold area (zed centers the crease
                // toggle within `gutter_dimensions.fold_area_width()`), on the
                // row's first display line (zed's `prepaint_crease_toggles`
                // positions toggles at `line_height * display_row`).
                let icon_x = bounds.origin.x + last_layout.line_number_width
                    - FOLD_ICON_HITBOX_WIDTH
                    + (FOLD_ICON_HITBOX_WIDTH - icon_size).half();
                let icon_y = bounds.origin.y + offset_y + (line_height - icon_size).half();
                let icon_bounds = Bounds::new(point(icon_x, icon_y), size(icon_size, icon_size));
                let hitbox = window.insert_hitbox(
                    Bounds::new(
                        point(
                            bounds.origin.x + last_layout.line_number_width
                                - FOLD_ICON_HITBOX_WIDTH,
                            bounds.origin.y + offset_y,
                        ),
                        size(FOLD_ICON_HITBOX_WIDTH, line_height),
                    ),
                    HitboxBehavior::Normal,
                );
                icon_layout.icons.push(FoldGutterIcon {
                    row: buffer_line,
                    is_folded: folded,
                    bounds: icon_bounds,
                    hitbox,
                });
            }
            offset_y += height;
        }

        icon_layout
    }

    /// Shape the `⋯` placeholders and trailing texts for every visible folded
    /// row (zed's fold placeholder is rendered inline where the folded range
    /// was, and the text after the fold end — e.g. the closing `}` — stays
    /// visible on the same display line).
    fn layout_fold_placeholders(
        &self,
        last_layout: &LastLayout,
        text_style: &TextStyle,
        font_size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<FoldPlaceholder> {
        // 先在独立作用域里收集折叠表头行（借用随块结束），彩虹 span 获取
        // 可能写回缓存（state.update），不能与读借用并存。
        let (fold_header_rows, text) = {
            let state = self.state.read(cx);
            if !state.mode.is_folding() || state.text.len() == 0 {
                return Vec::new();
            }
            let rows: Vec<usize> = last_layout
                .visible_buffer_lines
                .iter()
                .copied()
                .filter(|&buffer_line| state.display_map.fold_for_row(buffer_line).is_some())
                .collect();
            (rows, state.text.clone())
        };

        // 彩虹括号 span：折叠 tail（收尾行的 `}`）与表头行的 `{` 共用同一份
        // 层级色，否则占位符两侧括号一白一灰。
        let rainbow_spans = self.rainbow_bracket_spans(cx);

        let state = self.state.read(cx);
        let mut placeholders = Vec::new();

        let placeholder_color = cx.theme().text_muted;
        let ellipsis_text: SharedString = "⋯".into();
        let ellipsis = window.text_system().shape_line(
            ellipsis_text.clone(),
            font_size,
            &[TextRun {
                len: ellipsis_text.len(),
                font: text_style.font(),
                color: placeholder_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );

        let mut highlighter = state.mode.highlighter().map(|h| h.borrow_mut());
        let highlight_theme = cx.theme().highlight_theme.clone();
        let dark = cx.theme().appearance.is_dark();
        let error_color = cx.theme().error;

        for &buffer_line in fold_header_rows.iter() {
            let Some(fold) = state.display_map.fold_for_row(buffer_line) else {
                continue;
            };

            // The text after the fold end on the closing row (usually a `}`).
            //
            // `fold.end_offset` is `line_start(end.row) + end.column`, and the
            // old tail_end recomputed exactly that value — so the two were
            // always equal and the tail was never painted (folds rendered as
            // `fn main() {⋯` with the closing brace missing). Take the end of
            // the closing row's *content* instead: that trailing text is what
            // has to stay visible next to the `⋯` placeholder.
            let tail_start = fold.end_offset;
            let tail_end = text.line_content_end_offset(fold.end.row);
            let tail = if tail_end > tail_start {
                let tail_text: SharedString = text.slice(tail_start..tail_end).to_string().into();
                let tail_runs: Vec<TextRun> = if let Some(highlighter) =
                    highlighter.as_deref_mut().and_then(|h| h.as_mut())
                {
                    // 语法色 + 彩虹括号覆盖（与可见行同源，保证折叠两侧
                    // 括号同色），再转 TextRun。
                    let mut tail_styles =
                        highlighter.styles(&(tail_start..tail_end), &highlight_theme);
                    if let Some(spans) = &rainbow_spans {
                        let rainbow = Self::rainbow_overrides_in_range(
                            spans,
                            &(tail_start..tail_end),
                            dark,
                            error_color,
                        );
                        if !rainbow.is_empty() {
                            tail_styles = combine_highlights_stable(tail_styles, rainbow);
                        }
                    }
                    tail_styles
                        .into_iter()
                        .map(|(range, style)| {
                            let len = range.len().min(tail_text.len());
                            text_style.clone().highlight(style).to_run(len)
                        })
                        .filter(|run| run.len > 0)
                        .collect()
                } else {
                    Vec::new()
                };
                let runs = if tail_runs.is_empty() {
                    vec![TextRun {
                        len: tail_text.len(),
                        font: text_style.font(),
                        color: text_style.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }]
                } else {
                    tail_runs
                };
                Some(
                    window
                        .text_system()
                        .shape_line(tail_text, font_size, &runs, None),
                )
            } else {
                None
            };

            placeholders.push(FoldPlaceholder {
                row: buffer_line,
                pill_bounds: None,
                hitbox: None,
                ellipsis: ellipsis.clone(),
                tail,
            });
        }

        placeholders
    }

    /// Compute each placeholder pill's bounds and hitbox. Must run after the
    /// cursor layout (which scrolls `bounds`) so the pills land where the
    /// header line is painted. The pill sits at the end of the header line's
    /// last wrapped row, flush with the line's text (zed renders the
    /// placeholder inline exactly where the folded range was).
    fn layout_fold_placeholder_bounds(
        &self,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        placeholders: &mut [FoldPlaceholder],
        window: &mut Window,
        cx: &App,
    ) {
        let state = self.state.read(cx);
        if !state.mode.is_folding() {
            return;
        }

        let line_height = last_layout.line_height;
        let mut offset_y = last_layout.visible_top;
        for (i, &buffer_line) in last_layout.visible_buffer_lines.iter().enumerate() {
            let height = last_layout
                .lines
                .get(i)
                .map(|line| line.size(line_height).height)
                .unwrap_or(line_height);

            offset_y += line_height * last_layout.git_insert_delta(i) as f32;

            if state.display_map.is_line_folded(buffer_line) {
                if let Some(placeholder) = placeholders.iter_mut().find(|p| p.row == buffer_line) {
                    let line = last_layout.lines.get(i);
                    let wrap_count = line.map(|l| l.wrapped_lines.len()).unwrap_or(1).max(1);
                    let last_width = line
                        .and_then(|l| l.wrapped_lines.last())
                        .map(|l| l.width)
                        .unwrap_or(px(0.));
                    let pill_bounds = Bounds::new(
                        point(
                            bounds.origin.x + last_layout.line_number_width + last_width,
                            bounds.origin.y + offset_y + line_height * (wrap_count as f32 - 1.),
                        ),
                        size(placeholder.pill_width(), line_height),
                    );
                    let hitbox = window.insert_hitbox(pill_bounds, HitboxBehavior::Normal);
                    placeholder.pill_bounds = Some(pill_bounds);
                    placeholder.hitbox = Some(hitbox);
                }
            }

            offset_y += height;
        }
    }

    /// Paint the gutter fold disclosure icons (zed's crease toggles).
    ///
    /// Follows zed's `EditorSnapshot::render_crease_toggle` visibility rule:
    /// the icon is shown when the row is folded, or when it is foldable and
    /// either contains the cursor or the gutter is hovered. The icon is a
    /// chevron (right when folded, down when expanded) in the muted text
    /// color with a pointing-hand cursor, exactly like zed's `ui::Disclosure`.
    fn paint_fold_icons(
        &mut self,
        fold_icon_layout: &mut FoldIconLayout,
        current_row: Option<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<(usize, Bounds<Pixels>)> {
        let is_hovered = fold_icon_layout.line_number_hitbox.is_hovered(window);
        let icon_color = cx.theme().text_muted;
        let mut click_targets = Vec::new();
        for icon in &fold_icon_layout.icons {
            let is_current_line = current_row == Some(icon.row);
            if !icon.is_folded && !is_hovered && !is_current_line {
                // zed: `if folded || (is_foldable && (row_contains_cursor ||
                // gutter_hovered))` — otherwise no icon is rendered.
                continue;
            }

            click_targets.push((icon.row, icon.hitbox.bounds));
            let path = if icon.is_folded {
                FOLD_ICON_CLOSED
            } else {
                FOLD_ICON_OPEN
            };
            let _ = window.paint_svg(
                icon.bounds,
                SharedString::new_static(path),
                None,
                gpui::TransformationMatrix::default(),
                icon_color,
                cx,
            );
            window.set_cursor_style(CursorStyle::PointingHand, &icon.hitbox);
        }
        click_targets
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_lines(
        state: &InputState,
        display_text: &Rope,
        last_layout: &LastLayout,
        font_size: Pixels,
        runs: &[TextRun],
        _default_color: Hsla,
        whitespace_indicators: Option<WhitespaceIndicators>,
        window: &mut Window,
    ) -> Vec<LineLayout> {
        let is_single_line = state.mode.is_single_line();
        let buffer_lines = state.display_map.lines();

        if is_single_line {
            let shaped_line = window.text_system().shape_line(
                display_text.to_string().into(),
                font_size,
                runs,
                None,
            );

            let line_layout = LineLayout::new()
                .lines(smallvec::smallvec![shaped_line])
                .with_whitespaces(whitespace_indicators);
            return vec![line_layout];
        }

        // Empty to use placeholder, the placeholder is not in the wrapper map.
        if state.text.len() == 0 {
            let text = display_text.to_string();
            return text
                .split("\n")
                .map(|line| {
                    let shaped_line = window.text_system().shape_line(
                        SharedString::from(line.to_string()),
                        font_size,
                        runs,
                        None,
                    );
                    LineLayout::new()
                        .lines(smallvec::smallvec![shaped_line])
                        .with_whitespaces(whitespace_indicators.clone())
                })
                .collect();
        }

        let mut lines = Vec::with_capacity(last_layout.visible_buffer_lines.len());
        // run_offset is this buffer line's byte offset relative to the first
        // visible line, in the *global* text coordinate space (folded lines
        // count too). `highlight_lines` now returns a gap-free runs list over
        // the whole visible span (folded lines included), so matching global
        // offsets keep the two in sync even when lines are folded — without
        // this, every run after a fold is shifted and words get painted
        // half-highlighted / half-white.
        let first_line_start =
            display_text.line_start_offset(*last_layout.visible_buffer_lines.first().unwrap_or(&0));

        for &buffer_line in last_layout.visible_buffer_lines.iter() {
            let line_text: String = display_text.slice_line(buffer_line).into();
            // `buffer_lines` 与 `display_text` 偶尔会失同步（折叠/编辑时序），
            // 用 `expect` 会让整个编辑器在绘制期 panic。降级为跳过该行：少画
            // 一行远好过整个应用崩溃。
            let Some(line_item) = buffer_lines.get(buffer_line) else {
                log::error!(
                    "layout_lines: buffer_line {buffer_line} missing from wrapper \
                     (wrapper_len={}, visible={:?}, text_len={})",
                    buffer_lines.len(),
                    last_layout.visible_buffer_lines,
                    display_text.len()
                );
                continue;
            };

            debug_assert_eq!(line_item.len(), line_text.len());

            let run_offset = display_text.line_start_offset(buffer_line) - first_line_start;
            let mut wrapped_lines = SmallVec::with_capacity(1);

            for range in &line_item.wrapped_lines {
                let line_runs = runs_for_range(runs, run_offset, range);

                // Defensive checks to avoid panics from out-of-sync display map
                if range.start > line_text.len()
                    || range.end > line_text.len()
                    || !line_text.is_char_boundary(range.start)
                    || !line_text.is_char_boundary(range.end)
                {
                    log::error!(
                        "layout_lines invalid range: line_text.len={}, range={:?}, buffer_line={}",
                        line_text.len(),
                        range,
                        buffer_line
                    );
                    continue;
                }

                let sub_line: SharedString = line_text[range.clone()].to_string().into();
                let runs_len: usize = line_runs.iter().map(|r| r.len).sum();
                if runs_len != sub_line.len() {
                    let runs_total: usize = runs.iter().map(|r| r.len).sum();
                    let vis = last_layout.visible_buffer_lines.clone();
                    let first_line_start =
                        display_text.line_start_offset(*vis.first().unwrap_or(&0));
                    log::error!(
                        "layout_lines run length mismatch: sub_line.len={}, runs_len={}, buffer_line={}, range={:?}, run_offset={}, runs_total={}, visible={:?}, text_len={}, first_line_start={}",
                        sub_line.len(),
                        runs_len,
                        buffer_line,
                        range,
                        run_offset,
                        runs_total,
                        vis,
                        display_text.len(),
                        first_line_start
                    );
                }
                let shaped_line = window
                    .text_system()
                    .shape_line(sub_line, font_size, &line_runs, None);

                wrapped_lines.push(shaped_line);
            }

            let line_layout = LineLayout::new()
                .lines(wrapped_lines)
                .with_whitespaces(whitespace_indicators.clone());
            lines.push(line_layout);
        }

        lines
    }

    /// First usize is the offset of skipped.
    fn highlight_lines(
        &mut self,
        visible_buffer_lines: &[usize],
        _visible_top: Pixels,
        visible_byte_range: Range<usize>,
        cx: &mut App,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        let state = self.state.read(cx);
        let text = &state.text;
        let is_multi_line = state.mode.is_multi_line();

        let (mut highlighter_ref, diagnostics) = match &state.mode {
            InputMode::CodeEditor {
                highlighter,
                diagnostics,
                ..
            } => (highlighter.borrow_mut(), diagnostics.clone()),
            _ => return None,
        };

        let mut styles = Vec::with_capacity(visible_buffer_lines.len());

        if let Some(external) = &state.external_highlight_runs {
            // Externally computed runs (unified diff view): the list covers
            // the whole buffer gap-free; clip it to the visible span. The
            // internal tree-sitter highlighter is bypassed because the merged
            // diff text is not valid source code.
            let first = *visible_buffer_lines.first()?;
            let last = *visible_buffer_lines.last()?;
            let byte_start = text.line_start_offset(first);
            let byte_end = if is_multi_line {
                // +1 for `\n`, same as flush_range below
                text.line_start_offset(last + 1)
            } else {
                text.line_end_offset(last)
            };
            let mut cursor = byte_start;
            for (range, style) in external {
                if range.end <= byte_start || range.start >= byte_end {
                    continue;
                }
                let start = range.start.max(byte_start);
                let end = range.end.min(byte_end);
                if start > cursor {
                    styles.push((cursor..start, HighlightStyle::default()));
                }
                styles.push((start..end, *style));
                cursor = end.max(cursor);
            }
            if cursor < byte_end {
                styles.push((cursor..byte_end, HighlightStyle::default()));
            }
        } else {
            let highlighter = highlighter_ref.as_mut()?;

            let highlight_theme = cx.theme().highlight_theme.clone();

            // Helper to flush a contiguous range of lines
            let mut flush_range =
                |start_line: usize, end_line: usize, skip: bool, styles: &mut Vec<_>| {
                    let byte_start = text.line_start_offset(start_line);
                    let byte_end = if is_multi_line {
                        // +1 for `\n`
                        text.line_start_offset(end_line + 1)
                    } else {
                        text.line_end_offset(end_line)
                    };
                    let range_styles = if skip {
                        vec![(byte_start..byte_end, HighlightStyle::default())]
                    } else {
                        highlighter.styles(&(byte_start..byte_end), &highlight_theme)
                    };

                    // 各可见段严格递增且互不重叠（超长行与折叠会切段），
                    // 所以无需把已累积的样式一起重排 —— 只需归并本段内部
                    // （tree-sitter 的 captures 天然嵌套重叠），再追加到尾部。
                    // 此前写的是 `combine(styles.clone(), range_styles)`，
                    // 每段都对全部累积样式排序一次，段数一多就退化成
                    // O(K · N log N)。
                    styles.extend(combine_highlights_stable(
                        Vec::<(Range<usize>, HighlightStyle)>::new(),
                        range_styles,
                    ));
                };

            // Group contiguous visible lines into ranges and call styles() once per range
            let mut visible_iter = visible_buffer_lines.iter().peekable();
            let mut range_start: Option<usize> = None;

            while let Some(&line) = visible_iter.next() {
                // Check if this line is too long for highlighting; use content length without terminator
                let line_len = text.line_content(line).len();
                if line_len > MAX_HIGHLIGHT_LINE_LENGTH {
                    // Flush any accumulated range first
                    if let Some(start) = range_start.take() {
                        flush_range(start, line - 1, false, &mut styles);
                    }

                    flush_range(line, line, true, &mut styles);
                    continue;
                }

                range_start.get_or_insert(line);

                // Check if next line is contiguous, if so keep accumulating
                if visible_iter
                    .peek()
                    .map(|&&next| next == line + 1)
                    .unwrap_or(false)
                {
                    continue;
                }

                // Flush the contiguous range
                // 关键防御：range_start 理论上恒为 Some（上一行刚 get_or_insert），
                // 但 paint 路径不得 unwrap，异常时跳过该段高亮而非崩溃整帧。
                let Some(start_line) = range_start.take() else {
                    continue;
                };
                flush_range(start_line, line, false, &mut styles);
            }

            let diagnostic_styles = diagnostics.styles_for_range(&visible_byte_range, cx);
            styles = combine_highlights_stable(diagnostic_styles, styles);
        }
        drop(highlighter_ref);

        // Merge diff word-level decorations (e.g. inline changed-word
        // backgrounds in the git diff view).
        if !state.diff_range_colors.is_empty() {
            let diff_styles = state.diff_range_colors.iter().map(|(range, color)| {
                (
                    range.clone(),
                    HighlightStyle {
                        background_color: Some(*color),
                        ..Default::default()
                    },
                )
            });
            styles = combine_highlights_stable(styles, diff_styles);
        }

        // The internal tree-sitter branch flushes one gap-free range per
        // contiguous visible run, and folded (hidden) lines belong to no
        // range, so `combine_highlights` leaves byte gaps between runs.
        // Prepaint drops offsets (runs carry only lengths) and `layout_lines`
        // walks a contiguous offset from the first visible line — any missing
        // bytes shift every later run, painting words half-highlighted /
        // half-white past a fold. Fill the visible span so the runs list
        // always covers it exactly (the external branch is already gap-free,
        // so this is a no-op there).
        if let (Some(first), Some(last)) = (
            visible_buffer_lines.first().copied(),
            visible_buffer_lines.last().copied(),
        ) {
            let fill_start = text.line_start_offset(first);
            let fill_end = if is_multi_line {
                text.line_start_offset(last + 1)
            } else {
                text.line_end_offset(last)
            };
            if fill_end > fill_start {
                styles.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.0.end.cmp(&b.0.end)));
                let input = std::mem::take(&mut styles);
                let mut filled: Vec<(Range<usize>, HighlightStyle)> =
                    Vec::with_capacity(input.len() * 2 + 1);
                let mut cursor = fill_start;
                for (range, style) in input {
                    if range.end <= fill_start || range.start >= fill_end {
                        continue;
                    }
                    let s = range.start.max(fill_start);
                    let e = range.end.min(fill_end);
                    if e <= cursor {
                        continue;
                    }
                    if s > cursor {
                        filled.push((cursor..s, HighlightStyle::default()));
                    }
                    filled.push((s..e, style));
                    cursor = e;
                }
                if cursor < fill_end {
                    filled.push((cursor..fill_end, HighlightStyle::default()));
                }
                styles = filled;
            }
        }

        if styles.is_empty() {
            None
        } else {
            Some(styles)
        }
    }
}

/// Sticky scroll 单行：顶部钉住的定义行。
pub(crate) struct StickyLine {
    /// buffer 行号（0 起）。
    pub(crate) buffer_line: usize,
    /// 已 shape 的文本（含语法色）。
    pub(crate) text: ShapedLine,
    /// 已 shape 的行号。
    pub(crate) number: ShapedLine,
    /// 整行命中区域（窗口坐标，用于点击跳转）。
    pub(crate) bounds: Bounds<Pixels>,
}

/// git 展开区一行的几何（相对视口内容坐标的 y；paint 时加 origin.y）。
struct GitInsertRowPaint {
    hunk_ix: usize,
    row_ix: usize,
    y: Pixels,
}

pub(crate) struct PrepaintState {
    /// The lines of entire lines.
    last_layout: LastLayout,
    /// The lines only contains the visible lines in the viewport, based on `visible_range`.
    ///
    /// The child is the soft lines.
    line_numbers: Option<Vec<SmallVec<[ShapedLine; 1]>>>,
    /// Size of the scrollable area by entire lines.
    scroll_size: Size<Pixels>,
    cursor_bounds: Option<CursorLayout>,
    cursor_scroll_offset: Point<Pixels>,
    /// row index (zero based), no wrap, same line as the cursor.
    current_row: Option<usize>,
    selection_path: Option<Path<Pixels>>,
    /// Search-match highlight paths: (path, is_current_match). Painted below
    /// selections so both remain visible.
    search_match_paths: Vec<(Path<Pixels>, bool)>,
    /// 括号匹配高亮路径（光标紧邻括号时的开/闭括号背景）。
    bracket_paths: Vec<Path<Pixels>>,
    /// 同词高亮路径（光标处词的全文件出现）。
    occurrence_paths: Vec<Path<Pixels>>,
    /// Sticky scroll 行：(buffer 行号， 文本布局， 点击区域)。
    sticky_lines: Vec<StickyLine>,
    bounds: Bounds<Pixels>,
    /// Fold icon layout data (gutter icons + inline placeholders).
    fold_icon_layout: FoldIconLayout,
    /// Shaped `⋯` placeholders + trailing texts for visible folded rows.
    fold_placeholders: Vec<FoldPlaceholder>,
    /// 可见 buffer 行几何：(buffer 行, 相对 y, 高度)。prepaint 计算，
    /// paint 与 hitbox 创建共用。
    line_geometry: Vec<(usize, Pixels, Pixels)>,
    /// git 展开区行几何（相对 y）。
    git_insert_rows: Vec<GitInsertRowPaint>,
    /// 行首色条点击目标（prepaint 创建 hitbox，paint 落到 state）。
    git_bar_hitboxes: Vec<(u32, Bounds<Pixels>, Hitbox)>,
    /// 文本命中区（未滚动的元素 bounds，窗口坐标）：paint 阶段据此把
    /// IBeam 光标限定在文本上，而不是整个编辑器（含行号栏、滚动条）。
    text_hitbox: Hitbox,
    /// 行号栏命中区：paint 阶段显式设为箭头，压掉文本区的 IBeam。
    gutter_hitbox: Hitbox,
    /// 纵向滚动条条带命中区（内容不溢出时为 None）。
    v_scrollbar_hitbox: Option<Hitbox>,
    /// 横向滚动条条带命中区（内容不横向溢出时为 None）。
    h_scrollbar_hitbox: Option<Hitbox>,
}

impl PrepaintState {
    /// 把光标折成「屏幕坐标」。
    ///
    /// 有动画四边形时它**已经是屏幕坐标**（动画目标即屏幕位置，滚动由
    /// 动画器内部平移吸收），不能再叠加滚动偏移；无动画时按原有约定
    /// 补 y 方向滚动（x 已含在 origin 里）。
    fn cursor_bounds_with_scroll(&self) -> Option<CursorLayout> {
        self.cursor_bounds.clone().map(|mut layout| {
            if !layout.has_quad() {
                layout.shift_y(self.cursor_scroll_offset.y);
            }
            layout
        })
    }
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    crate::impl_source_location_none!();

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.prepare_if_needed(window, cx);

        let state = self.state.read(cx);
        let line_height = window.line_height();

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        if state.mode.is_multi_line() {
            style.flex_grow = 1.0;
            style.size.height = relative(1.).into();
            if state.mode.is_auto_grow() {
                // Auto grow to let height match to rows, but not exceed max rows.
                let rows = state.mode.rows();
                style.min_size.height = (rows * line_height).into();
            } else {
                style.min_size.height = line_height.into();
            }
        } else {
            // For single-line inputs, the minimum height should be the line height
            style.size.height = line_height.into();
        };

        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // 编辑器字体收敛到 TextElement 自身（见 editor_font_refinement）：
        // 同一 InputState 无论从哪条渲染路径画出来，字体/行高必须一致。
        let refinement = self.editor_font_refinement(cx);
        window.with_text_style(refinement, |window| {
            self.prepaint_inner(bounds, _request_layout, window, cx)
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        input_bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 同 prepaint：把编辑器字体压到环境样式上，保证与其它渲染路径一致。
        let refinement = self.editor_font_refinement(cx);
        window.with_text_style(refinement, |window| {
            self.paint_inner(input_bounds, _request_layout, prepaint, window, cx)
        })
    }
}

impl TextElement {
    fn prepaint_inner(
        &mut self,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let style = window.text_style();

        let state = self.state.read(cx);
        let line_height = window.line_height();

        let (visible_range, visible_buffer_lines, visible_top) =
            self.calculate_visible_range(state, line_height, bounds.size.height);
        let visible_start_offset = state.text.line_start_offset(visible_range.start);
        let visible_end_offset = state
            .text
            .line_end_offset(visible_range.end.saturating_sub(1));

        let highlight_styles = self.highlight_lines(
            &visible_buffer_lines,
            visible_top,
            visible_start_offset..visible_end_offset,
            cx,
        );
        // 括号着色：按嵌套层级给可见括号分色（覆盖语法色，最后合并）
        let highlight_styles = self.merge_rainbow_brackets(
            highlight_styles,
            &(visible_start_offset..visible_end_offset),
            cx,
        );

        let state = self.state.read(cx);
        let multi_line = state.mode.is_multi_line();
        let text = state.text.clone();
        let is_empty = text.len() == 0;
        let placeholder = self.placeholder.clone();

        let text_size = match (is_empty, state.placeholder_font_size, state.font_size) {
            (true, Some(placeholder_font_size), _) => placeholder_font_size,
            (_, _, Some(font_size)) => font_size,
            _ => style.font_size.to_pixels(window.rem_size()),
        };

        let mut text_style = window.text_style();
        let fg = cx
            .theme()
            .highlight_theme
            .style
            .editor_foreground
            .unwrap_or(text_style.color);
        text_style.color = fg;
        let (display_text, text_color) = if is_empty {
            (&Rope::from(placeholder.as_str()), cx.theme().text_muted)
        } else if state.masked {
            (
                &Rope::from(MASK_CHAR.to_string().repeat(text.chars().count())),
                fg,
            )
        } else {
            (&text, fg)
        };

        // Calculate the width of the line numbers
        let (line_number_width, line_number_len) =
            Self::layout_line_numbers(state, &text, text_size, &text_style, window);

        let input_bounds = bounds; // 未滚动的元素 bounds（sticky scroll 锚定用）
        let mut bounds = bounds;

        let wrap_width = if multi_line && state.soft_wrap {
            Some(bounds.size.width - line_number_width - RIGHT_MARGIN)
        } else {
            None
        };

        let visible_line_byte_offsets: Vec<usize> = visible_buffer_lines
            .iter()
            .map(|&bl| state.text.line_start_offset(bl))
            .collect();

        // For password input (masked: true), convert byte offsets to masked display byte offsets so that
        // layout_match_range and position_for_index work in the correct coordinate space.
        let (visible_line_byte_offsets, visible_range_offset) = if state.masked {
            let offsets = visible_line_byte_offsets
                .iter()
                .map(|&o| masked_display_offset(&text, o))
                .collect();
            let range_offset = masked_display_offset(&text, visible_start_offset)
                ..masked_display_offset(&text, visible_end_offset);
            (offsets, range_offset)
        } else {
            (
                visible_line_byte_offsets,
                visible_start_offset..visible_end_offset,
            )
        };

        // git 展开区行数前缀表：所有遍历可见行的 y 累计点统一查表。
        let (git_insert_shift, git_insert_baseline) =
            if state.display_map.git_insertions().is_empty() {
                (Rc::new(Vec::new()), 0)
            } else {
                let shift: Vec<u32> = visible_buffer_lines
                    .iter()
                    .map(|&line| state.display_map.git_insertion_rows_before(line + 1) as u32)
                    .collect();
                let baseline = visible_buffer_lines
                    .first()
                    .map(|&first| state.display_map.git_insertion_rows_before(first))
                    .unwrap_or(0);
                (Rc::new(shift), baseline)
            };

        let mut last_layout = LastLayout {
            visible_range,
            visible_buffer_lines,
            visible_line_byte_offsets,
            visible_top,
            visible_range_offset,
            line_height,
            line_number_width,
            line_number_len,
            git_insert_shift,
            git_insert_baseline,
            lines: Rc::new(vec![]),
            cursor_bounds: None,
            text_align: state.text_align,
            content_width: bounds.size.width,
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked_run = TextRun {
            len: 0,
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(text_color),
                wavy: false,
            }),
            strikethrough: None,
        };

        let runs = if !is_empty {
            if let Some(highlight_styles) = highlight_styles {
                let mut runs = Vec::with_capacity(highlight_styles.len());

                runs.extend(highlight_styles.iter().map(|(range, style)| {
                    let mut run = text_style.clone().highlight(*style).to_run(range.len());
                    if let Some(ime_marked_range) = &state.ime_marked_range {
                        if range.start >= ime_marked_range.start
                            && range.end <= ime_marked_range.end
                        {
                            run.color = marked_run.color;
                            run.strikethrough = marked_run.strikethrough;
                            run.underline = marked_run.underline;
                        }
                    }

                    run
                }));

                runs.into_iter().filter(|run| run.len > 0).collect()
            } else {
                vec![run]
            }
        } else if let Some(ime_marked_range) = &state.ime_marked_range {
            // IME marked text
            vec![
                TextRun {
                    len: ime_marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: ime_marked_range.end - ime_marked_range.start,
                    underline: marked_run.underline,
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - ime_marked_range.end,
                    ..run.clone()
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        // Create shaped lines for whitespace indicators before layout
        let whitespace_indicators =
            Self::layout_whitespace_indicators(state, text_size, &text_style, window, cx);

        let lines = Self::layout_lines(
            state,
            display_text,
            &last_layout,
            text_size,
            &runs,
            fg,
            whitespace_indicators,
            window,
        );

        let mut longest_line_width = wrap_width.unwrap_or(px(0.));
        // 1. Single line
        // 2. Multi-line with soft wrap disabled.
        if state.mode.is_single_line() || !state.soft_wrap {
            let longest_row = state.display_map.longest_row();
            let longest_line: SharedString =
                state.text.line_content(longest_row).to_string().into();
            longest_line_width = window
                .text_system()
                .shape_line(
                    longest_line.clone(),
                    text_size,
                    &[TextRun {
                        len: longest_line.len(),
                        font: style.font(),
                        color: fg,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    wrap_width,
                )
                .width;
        }
        last_layout.lines = Rc::new(lines);

        // 可见行几何（相对 y）：paint 阶段与色条 hitbox 创建共用。
        // git 展开区：锚点行上的块高经 `git_insert_delta` 查表补入，
        // 展开行（相对 y）与 buffer 行在同一累计中产出。
        let (line_geometry, git_insert_rows) = {
            let rows_total = state
                .display_map
                .git_insertions()
                .iter()
                .map(|ins| ins.lines.len())
                .sum::<usize>();
            let mut y = last_layout.visible_top;
            let mut out = Vec::with_capacity(last_layout.visible_buffer_lines.len());
            let mut insert_rows: Vec<GitInsertRowPaint> = Vec::with_capacity(rows_total);
            for (i, (line, &buffer_line)) in last_layout
                .lines
                .iter()
                .zip(last_layout.visible_buffer_lines.iter())
                .enumerate()
            {
                let mut delta = last_layout.git_insert_delta(i) as usize;
                for ins in state.display_map.git_insertions_at(buffer_line) {
                    for (row_ix, _) in ins.lines.iter().enumerate() {
                        insert_rows.push(GitInsertRowPaint {
                            hunk_ix: ins.hunk_ix,
                            row_ix,
                            y,
                        });
                        y += line_height;
                        delta = delta.saturating_sub(1);
                        if delta == 0 {
                            break;
                        }
                    }
                    if delta == 0 {
                        break;
                    }
                }
                let height = line_height * line.wrapped_lines.len().max(1) as f32;
                out.push((buffer_line, y, height));
                y += height;
            }
            (out, insert_rows)
        };

        // The scrollable height counts *display* rows (folded rows hidden,
        // git 展开区行计入), matching what is actually painted.
        let total_display_lines = state.display_map.display_row_count_with_insertions();
        let empty_bottom_height = if state.mode.is_code_editor() {
            let base = bounds
                .size
                .height
                .half()
                .max(BOTTOM_MARGIN_ROWS * line_height);
            // `scroll_beyond_last_line`: allow scrolling extra empty rows
            // past the last line.
            match state.scroll_beyond_last_line {
                Some(rows) => base.max(rows as f32 * line_height),
                None => base,
            }
        } else {
            px(0.)
        };

        let mut scroll_size = size(
            if longest_line_width + line_number_width + RIGHT_MARGIN > bounds.size.width {
                longest_line_width + line_number_width + RIGHT_MARGIN
            } else {
                longest_line_width
            },
            (total_display_lines as f32 * line_height + empty_bottom_height)
                .max(bounds.size.height),
        );

        // TODO: should be add some gap to right, to convenient to focus on boundary position
        if last_layout.text_align == TextAlign::Right || last_layout.text_align == TextAlign::Center
        {
            scroll_size.width = longest_line_width + line_number_width;
        }

        // `position_for_index` for example
        //
        // #### text
        //
        // Hello 世界，this is GPUI component.
        // The GPUI Component is a collection of UI components for
        // GPUI framework, including Button, Input, Checkbox, Radio,
        // Dropdown, Tab, and more...
        //
        // wrap_width: 444px, line_height: 20px
        //
        // #### lines[0]
        //
        // | index | pos              | line |
        // |-------|------------------|------|
        // | 5     | (37 px, 0.0)     | 0    |
        // | 38    | (261.7 px, 20.0) | 0    |
        // | 40    | None             | -    |
        //
        // #### lines[1]
        //
        // | index | position              | line |
        // |-------|-----------------------|------|
        // | 5     | (43.578125 px, 0.0)   | 0    |
        // | 56    | (422.21094 px, 0.0)   | 0    |
        // | 57    | (11.6328125 px, 20.0) | 1    |
        // | 114   | (429.85938 px, 20.0)  | 1    |
        // | 115   | (11.3125 px, 40.0)    | 2    |

        // Calculate the scroll offset to keep the cursor in view.
        // The fold placeholders are shaped first so the cursor can be placed
        // after the `⋯` when it sits at a fold boundary (zed maps positions
        // inside a fold to the placeholder).
        let mut fold_placeholders =
            self.layout_fold_placeholders(&last_layout, &text_style, text_size, window, cx);

        let (cursor_bounds, cursor_scroll_offset, current_row) =
            self.layout_cursor(&last_layout, &fold_placeholders, &mut bounds, window, cx);
        last_layout.cursor_bounds = cursor_bounds.clone().map(|layout| layout.bounding_rect());

        // ── 光标弹簧动画 ────────────────────────────────────────────────────
        // 推进四角弹簧（位置平滑 + 形状 morph），把结果四边形挂到 cursor_bounds。
        // 滚动不算移动：这里把滚动偏移单独传给动画器，它会把四角与目标一起
        // 平移，光标钉在内容上、不产生动画。
        let cursor_bounds = cursor_bounds.map(|layout| {
            let shape: crate::components::cursor_animation::CursorShape = layout.shape().into();
            // 形状基准尺寸：Bar 用固定宽度、Underline 用固定高度（编辑器两者
            // 都与字号无关），Block/Hollow 用整块宽高。`cell` 决定 Block 宽度，
            // `shape_span` 决定 beam/underline 的粗细比例，两者在编辑器里相同。
            let block_w = layout.block_width().as_f32();
            let line_h = layout.line_height().as_f32();
            let (span_w, span_h) = match layout.shape() {
                CursorShape::Bar => (block_w.max(1.0), line_h),
                _ => (block_w, line_h),
            };
            let cell_percentage = match layout.shape() {
                CursorShape::Bar => (CURSOR_WIDTH.as_f32() / span_w).clamp(0.0, 1.0),
                CursorShape::Underline => (2.0 / span_h).clamp(0.0, 1.0),
                _ => 1.0,
            };
            let origin = layout.origin();
            // ── 坐标空间（与 neovide 一致：目标即屏幕坐标） ────────────────
            // `layout_cursor` 产出的 origin.x 含横向滚动、y 不含；这里补齐 y，
            // 得到完整的屏幕坐标。滚动时 `scroll_offset` 会变化，动画器把
            // 「四角 + 静止目标」一起平移，所以滚动不算移动、不产生动画。
            let screen_x = origin.x;
            let screen_y = origin.y + cursor_scroll_offset.y;
            let target = crate::components::cursor_animation::CursorTarget {
                destination: point(screen_x.as_f32(), screen_y.as_f32()),
                cell: (block_w, line_h),
                shape_span: (span_w, span_h),
                scroll_offset: point(
                    cursor_scroll_offset.x.as_f32(),
                    cursor_scroll_offset.y.as_f32(),
                ),
                shape,
                cell_percentage,
                metrics: (layout.block_width(), layout.line_height()),
                // 用**未滚动的**元素 bounds：`bounds` 在 layout_cursor 里
                // 已被 `+= scroll_offset`，滚动时它跟着变，会被误判成
                // 「布局变化」而触发瞬移（光标本该跟着内容走）。
                layout_origin: point(input_bounds.left(), input_bounds.top()),
            };
            let now = cx.background_executor().now();
            let reduce_motion = cx.reduce_motion();
            let (quad, animating) = self.state.update(cx, |state, _cx| {
                if reduce_motion {
                    state.cursor_animator.reset();
                }
                let quad = state.cursor_animator.step(Some(target), now);
                (quad, state.cursor_animator.is_animating())
            });
            // 仍在运动 → 请求下一帧（走平台帧源，与 vsync 对齐）。
            if animating {
                let state_id = self.state.entity_id();
                window.on_next_frame(move |_, cx| cx.notify(state_id));
            }
            match quad {
                Some(quad) => layout.with_quad(quad),
                None => layout,
            }
        });

        // 闪烁淡入淡出窗口内需要连续帧：与光标动画同一套「按需续帧」约定。
        // 相位中间 `needs_repaint()` 为 false，静止即停，不会让渲染循环空转。
        if self.state.read(cx).blink_cursor.read(cx).needs_repaint() {
            let state_id = self.state.entity_id();
            window.on_next_frame(move |_, cx| cx.notify(state_id));
        }

        // Pills are positioned where the header lines are painted, so this
        // must run after the cursor layout has scrolled `bounds`.
        self.layout_fold_placeholder_bounds(
            &bounds,
            &last_layout,
            &mut fold_placeholders,
            window,
            cx,
        );

        let selection_path = self.layout_selections(&last_layout, &mut bounds, window, cx);
        let search_match_paths = self.layout_search_matches(&last_layout, &bounds, cx);
        let bracket_paths = self.layout_bracket_matches(&last_layout, &bounds, window, cx);
        let occurrence_paths = self.layout_occurrences(&last_layout, &bounds, window, cx);

        let state = self.state.read(cx);
        let line_numbers = if state.mode.line_number() {
            let mut line_numbers = Vec::with_capacity(last_layout.visible_buffer_lines.len());
            let other_line_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().text_muted,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let current_line_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().text,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            // Diff 行号着色（参考 diff UI：新增行绿号、删除行红号）。
            let diff_added_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().status_added,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let diff_deleted_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().status_deleted,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];

            // build line numbers
            for (line, &buffer_line) in last_layout
                .lines
                .iter()
                .zip(last_layout.visible_buffer_lines.iter())
            {
                let line_no: SharedString = if let Some(labels) = &state.diff_line_labels {
                    match labels.get(buffer_line).copied().flatten() {
                        Some(n) => format!("{:>width$}", n + 1, width = line_number_len).into(),
                        None => " ".repeat(line_number_len).into(),
                    }
                } else {
                    format!("{:>width$}", buffer_line + 1, width = line_number_len).into()
                };

                let runs = if current_row == Some(buffer_line) {
                    &current_line_runs
                } else {
                    match state.diff_line_colors.get(&(buffer_line as u32)) {
                        Some((_, crate::components::input::DiffLineKind::Added)) => {
                            &diff_added_runs
                        }
                        Some((_, crate::components::input::DiffLineKind::Deleted)) => {
                            &diff_deleted_runs
                        }
                        _ => &other_line_runs,
                    }
                };

                let mut sub_lines: SmallVec<[ShapedLine; 1]> = SmallVec::new();
                sub_lines.push(
                    window
                        .text_system()
                        .shape_line(line_no, text_size, runs, None),
                );
                for _ in 0..line.wrapped_lines.len().saturating_sub(1) {
                    sub_lines.push(ShapedLine::default());
                }
                line_numbers.push(sub_lines);
            }
            Some(line_numbers)
        } else {
            None
        };

        let fold_icon_layout = self.layout_fold_icons(&bounds, &last_layout, window, cx);

        // git 行首色条 hitbox：光标布局调整过 bounds 之后创建（y 为绝对坐标），
        // 与 fold 图标同一时机；paint 阶段据此设手型光标并注册点击目标。
        let git_bar_hitboxes = {
            // 短作用域重读（外层 state 守卫不能跨 layout_fold_icons 的 &mut cx）。
            let state = self.state.read(cx);
            let mut targets: Vec<(u32, Bounds<Pixels>, Hitbox)> = Vec::new();
            if state.git_gutter && state.git_expandable {
                for &(buffer_line, y, height) in line_geometry.iter() {
                    let expandable = state.git_hunk_index_for_line(buffer_line as u32).is_some();
                    if !expandable {
                        continue;
                    }
                    let hit_bounds = Bounds::new(
                        point(bounds.origin.x, bounds.origin.y + y),
                        size(px(10.), height),
                    );
                    let hitbox = window.insert_hitbox(hit_bounds, HitboxBehavior::Normal);
                    targets.push((buffer_line as u32, hit_bounds, hitbox));
                }
            }
            targets
        };

        // Sticky scroll：外围定义行（首可见行之上）钉在顶部
        let sticky_lines = self.layout_sticky_lines(
            &last_layout,
            &input_bounds,
            &text_style,
            text_size,
            window.line_height(),
            line_number_len,
            window,
            cx,
        );

        // 鼠标光标命中区（Zed `EditorElement` 同款拆分）：
        // - `gutter_hitbox` 覆盖行号栏（含折叠图标列），paint 里显式设 Arrow；
        // - `text_hitbox` 覆盖文本区，paint 里设 IBeam。
        // 两者都是**未滚动**的窗口坐标：命中判定用的是屏幕上的静态矩形，
        // 内容滚动不改变「哪块区域是文本」。文本区从行号栏右缘起，横向滚动
        // 只平移文字本身，命中区不动（与 Zed 的 `text_hitbox` 一致）。
        let text_hitbox = window.insert_hitbox(
            Bounds {
                origin: point(
                    input_bounds.origin.x + line_number_width,
                    input_bounds.origin.y,
                ),
                size: size(
                    (input_bounds.size.width - line_number_width).max(px(0.)),
                    input_bounds.size.height,
                ),
            },
            HitboxBehavior::Normal,
        );
        let gutter_hitbox = window.insert_hitbox(
            Bounds {
                origin: input_bounds.origin,
                size: size(line_number_width, input_bounds.size.height),
            },
            HitboxBehavior::Normal,
        );

        // 滚动条条带命中区：thumb 是拖拽目标，指针停在条带上要显示箭头
        // （Zed `paint_scrollbars` 对 scrollbar hitbox 设 Arrow，VSCode 的
        // `.monaco-scrollable-element > .scrollbar` 同样是 `cursor:default`）。
        // 只在内容真的溢出、条带会画出来时才建：否则会把文本右缘/底缘的
        // IBeam 变成箭头，反而误导。
        //
        // 溢出判定与 paint 阶段同一套 `thumb_metrics` 入参（视口/内容同减
        // `BAR_THICKNESS`），保证「建了 hitbox」与「画出条带」一一对应。
        // 传 0 作 offset：`thumb_metrics` 是否返回 `Some` 只由两个长度决定，
        // offset 仅影响 thumb 位置（那是 paint 的事）。
        let scrollbar_enabled = {
            let state = self.state.read(cx);
            state.scrollbar && state.mode.is_multi_line()
        };
        let v_scrollbar_hitbox = (scrollbar_enabled
            && thumb_metrics(input_bounds.size.height, scroll_size.height, px(0.)).is_some())
        .then(|| {
            window.insert_hitbox(
                Bounds {
                    origin: point(input_bounds.right() - BAR_THICKNESS, input_bounds.origin.y),
                    size: size(BAR_THICKNESS, input_bounds.size.height),
                },
                HitboxBehavior::Normal,
            )
        });
        let h_scrollbar_hitbox = (scrollbar_enabled
            && thumb_metrics(
                input_bounds.size.width - BAR_THICKNESS,
                scroll_size.width - BAR_THICKNESS,
                px(0.),
            )
            .is_some())
        .then(|| {
            window.insert_hitbox(
                Bounds {
                    origin: point(input_bounds.origin.x, input_bounds.bottom() - BAR_THICKNESS),
                    size: size(input_bounds.size.width - BAR_THICKNESS, BAR_THICKNESS),
                },
                HitboxBehavior::Normal,
            )
        });

        // LSP 浮层/菜单的布局必须在这里做：`layout_as_root` 只允许在
        // request_layout / prepaint 调用，放到 paint 会 panic。
        // 用 prepaint 结束时算好的行布局与滚动偏移（Zed 同款）。
        // 只传**未滚动**的元素矩形（屏幕上的可视区）+ 滚动偏移：
        // 滚过的内容原点由 `layout_lsp_popovers` 内部推导。
        //
        // 之所以不直接传 `bounds`：它在 `layout_cursor` 里已被
        // `bounds.origin += scroll_offset` 改过，与「可视区」是两个不同的
        // 矩形。曾经把滚过的它当视口，导致 `viewport_top` 少了 scroll_y，
        // 顶部代码被误判成「上方放得下」而往上弹。
        // 现在函数只收未滚动的视口，两者无法再被搞混。
        self.layout_lsp_popovers(
            input_bounds,
            cursor_scroll_offset,
            &last_layout,
            last_layout.line_height,
            window,
            cx,
        );

        PrepaintState {
            bounds,
            last_layout,
            scroll_size,
            line_numbers,
            cursor_bounds,
            cursor_scroll_offset,
            current_row,
            selection_path,
            search_match_paths,
            bracket_paths,
            occurrence_paths,
            sticky_lines,
            fold_icon_layout,
            fold_placeholders,
            line_geometry,
            git_insert_rows,
            git_bar_hitboxes,
            text_hitbox,
            gutter_hitbox,
            v_scrollbar_hitbox,
            h_scrollbar_hitbox,
        }
    }

    fn paint_inner(
        &mut self,
        input_bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let state = self.state.read(cx);
        // 滚动条相关值提前取出（避免 state 借用跨 line.paint 的 &mut cx）
        let scrollbar_enabled = state.scrollbar && state.mode.is_multi_line();
        let scrollbar_dragging = state.scrollbar_drag.is_some();
        let scrollbar_dragging_h = state.scrollbar_drag_h.is_some();
        let scrollbar_scroll_y = (-state.scroll_handle.offset().y).max(px(0.));
        // 横向滚动偏移（右滚为负）：后面 `state` 的读锁要跨一次 `&mut cx`
        // 的绘制段，这里与纵向一样提前取出来。
        let scrollbar_scroll_x = state.scroll_handle.offset().x;
        let focus_handle = state.focus_handle.clone();
        // 光标绘制条件与不透明度提前取出（同上：后面要用 `&mut cx` 绘制）。
        // `should_paint_cursor` 覆盖「淡入窗口内 visible() 仍为 false」的情况。
        let paint_cursor = state.should_paint_cursor(window, cx);
        let cursor_opacity = state.cursor_opacity(window, cx);
        let focused = focus_handle.is_focused(window);
        let bounds = prepaint.bounds;
        let selected_range = state.selected_range;
        let text_align = prepaint.last_layout.text_align;

        // 鼠标光标（Zed `EditorElement::paint_text` + `paint_line_numbers`，
        // VSCode `viewLines.css` 的 `cursor:text` + `lineNumbers.css` 的
        // `cursor:default`）：文本区 IBeam、行号栏与滚动条箭头。
        //
        // 只读/disabled 的编辑器同样给 IBeam：两条参考实现里只读编辑器都能
        // 划选复制（diff 视图左栏、只读预览），箭头会误导用户以为不能选。
        //
        // 命中区用 prepaint 里按**未滚动** bounds 建的 hitbox：光标样式判定的是
        // 屏幕上哪块矩形属于文本，内容滚动不改变这一点（Zed 的 `text_hitbox`
        // 同样在滚动之外）。
        //
        // 滚动条是**悬浮**在文本右缘/底缘之上的，与文本命中区有意重叠；这里
        // 把箭头请求放在文本之后（gpui 取最后一条命中项，`NextFrame::
        // cursor_style`），条带上就压掉 IBeam。
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.text_hitbox);
        window.set_cursor_style(CursorStyle::Arrow, &prepaint.gutter_hitbox);
        if let Some(hitbox) = prepaint.v_scrollbar_hitbox.as_ref() {
            window.set_cursor_style(CursorStyle::Arrow, hitbox);
        }
        if let Some(hitbox) = prepaint.h_scrollbar_hitbox.as_ref() {
            window.set_cursor_style(CursorStyle::Arrow, hitbox);
        }
        #[cfg(test)]
        {
            record_cursor_request(CursorStyle::IBeam, prepaint.text_hitbox.bounds);
            record_cursor_request(CursorStyle::Arrow, prepaint.gutter_hitbox.bounds);
            if let Some(hitbox) = prepaint.v_scrollbar_hitbox.as_ref() {
                record_cursor_request(CursorStyle::Arrow, hitbox.bounds);
            }
            if let Some(hitbox) = prepaint.h_scrollbar_hitbox.as_ref() {
                record_cursor_request(CursorStyle::Arrow, hitbox.bounds);
            }
        }

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.state.clone()),
            cx,
        );

        // Paint multi line text
        let line_height = window.line_height();
        let origin = bounds.origin;

        let invisible_top_padding = prepaint.last_layout.visible_top;
        let active_line_color = cx.theme().highlight_theme.style.editor_active_line;

        // Diff rows (git diff views only): opaque deeper versions of the
        // added/deleted tints, composited over the editor background. Used
        // for the active row and for rows covered by the selection, so the
        // active-line gray / selection blue never hide the row's hue.
        // Owned values only: the `state` read guard must not stay alive
        // across the `&mut cx` paint calls below.
        let (deep_added, deep_deleted) = if !state.diff_line_colors.is_empty() {
            let deep = |status: Hsla| {
                // 两倍于普通行 tint（0.14）的不透明度：肉眼明显加深，且不透明
                // 合成后盖在选区/激活行底色上不会透出原色。
                Hsla::from(
                    gpui::Rgba::from(cx.theme().editor_background())
                        .blend(gpui::Rgba::from(status.opacity(0.28))),
                )
            };
            (
                deep(cx.theme().status_added),
                deep(cx.theme().status_deleted),
            )
        } else {
            (Hsla::default(), Hsla::default())
        };
        let selected_byte_range =
            if window.is_window_active() && !state.masked && !selected_range.is_empty() {
                Some(
                    selected_range.start.min(selected_range.end)
                        ..selected_range.start.max(selected_range.end),
                )
            } else {
                None
            };

        // Paint the current line background.
        //
        // This used to live inside the `line_numbers` block below, so hiding
        // line numbers also silently disabled the active-line highlight.
        //
        // At most one visible line can be the current row, so a `find` is
        // enough — no need to scan the rest of the geometry.
        //
        // 仅在多行编辑器中获得焦点时绘制（与 Zed 一致）：
        // - 失焦不画 —— 失焦的输入框不应有任何行高亮；
        // - 单行输入框（搜索框、表单）永不画 —— active line 的色值接近
        //   编辑器底色，铺在 `tab_hover_background` 等更亮的输入框容器上
        //   会形成一块色差明显的矩形，导致输入区背景色不一致。
        let single_line = !state.mode.is_multi_line();
        if focused && !single_line && selected_range.is_empty() {
            if let Some(&(buffer_line, y, height)) = prepaint
                .line_geometry
                .iter()
                .find(|(buffer_line, _, _)| prepaint.current_row == Some(*buffer_line))
            {
                // diff 行的激活底色（加深红/绿）由下方 diff 行背景循环统一
                // 绘制，这里只处理普通行 —— 同一行不能先后叠两种底色，
                // 否则激活 diff 行的行首（gutter）与正文深浅不一致。
                if !state.diff_line_colors.contains_key(&(buffer_line as u32)) {
                    if let Some(bg_color) = active_line_color {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(input_bounds.origin.x, origin.y + y),
                                size(bounds.size.width, height),
                            ),
                            bg_color,
                        ));
                    }
                }
            }
        }

        // git 色条点击目标（paint 阶段填充，paint 结束写入 state）。
        let mut git_bar_click_targets: Vec<(u32, Bounds<Pixels>)> = Vec::new();

        if let Some(line_numbers) = prepaint.line_numbers.as_ref() {
            let diff_line_colors = &self.state.read(cx).diff_line_colors;

            // Each item is the normal lines; `line_geometry` is parallel to
            // `line_numbers`, so the height comes from the geometry above.
            for (&(buffer_line, y, height), _) in prepaint.line_geometry.iter().zip(line_numbers) {
                let p = point(input_bounds.origin.x, origin.y + y);
                // Paint diff line background (added/deleted) below everything
                // else. Selected/active rows use the deep tint — the same rule
                // as the gutter cell below, so one row is one uniform color.
                if let Some((bg_color, line_kind)) = diff_line_colors.get(&(buffer_line as u32)) {
                    let row_deep = (focused
                        && !single_line
                        && selected_range.is_empty()
                        && prepaint.current_row == Some(buffer_line))
                        || selected_byte_range.as_ref().is_some_and(|sel| {
                            let state = self.state.read(cx);
                            let row_start = state.text.line_start_offset(buffer_line);
                            let row_end = state.text.line_start_offset(buffer_line + 1);
                            row_start < sel.end && sel.start < row_end
                        });
                    let bg = if row_deep {
                        match line_kind {
                            DiffLineKind::Added => deep_added,
                            DiffLineKind::Deleted => deep_deleted,
                        }
                    } else {
                        *bg_color
                    };
                    window.paint_quad(fill(Bounds::new(p, size(bounds.size.width, height)), bg));
                }
                // 已展开 hunk 的新行：整行绿底（与 diff view 新增行同色）。
                // 行首色条在下方「Paint line numbers」段里绘制 —— 行号区背景
                // 全高后涂，画在它之前的色条会被盖住。
                let expanded_tinted = self
                    .state
                    .read(cx)
                    .line_in_expanded_hunk(buffer_line as u32);
                if expanded_tinted && !diff_line_colors.contains_key(&(buffer_line as u32)) {
                    window.paint_quad(fill(
                        Bounds::new(p, size(bounds.size.width, height)),
                        cx.theme().status_added.opacity(0.14),
                    ));
                }
            }
        }

        // Paint selections
        if window.is_window_active() {
            if let Some(path) = prepaint.selection_path.take() {
                window.paint_path(path, cx.theme().selection);
            }
        }

        // Selected diff rows keep their identity: overpaint the selection
        // with the deeper diff tint instead of letting the selection blue
        // cover the added/deleted row background.
        if let Some(sel) = selected_byte_range.as_ref() {
            for (buffer_line, y, height) in &prepaint.line_geometry {
                if let Some((_, kind)) = state.diff_line_colors.get(&(*buffer_line as u32)) {
                    let row_start = state.text.line_start_offset(*buffer_line);
                    let row_end = state.text.line_start_offset(*buffer_line + 1);
                    if row_start < sel.end && sel.start < row_end {
                        let bg = match kind {
                            DiffLineKind::Added => deep_added,
                            DiffLineKind::Deleted => deep_deleted,
                        };
                        window.paint_quad(fill(
                            Bounds::new(
                                point(input_bounds.origin.x, origin.y + *y),
                                size(bounds.size.width, *height),
                            ),
                            bg,
                        ));
                    }
                }
            }
        }

        // Paint search matches (below the text, above the background): every
        // match gets a faint selection color, the current one a stronger one.
        for (path, is_active) in prepaint.search_match_paths.iter() {
            window.paint_path(path.clone(), cx.theme().selection.opacity(0.45));
            if *is_active {
                window.paint_path(path.clone(), cx.theme().selection);
            }
        }

        // Paint matching brackets：柔和的 accent 底色标记配对括号
        let bracket_fill = cx.theme().accent.opacity(0.22);
        for path in prepaint.bracket_paths.drain(..) {
            window.paint_path(path, bracket_fill);
        }

        // Paint word occurrences：光标处词的全文件出现，淡选区色
        let occurrence_fill = cx.theme().selection.opacity(0.24);
        for path in prepaint.occurrence_paths.drain(..) {
            window.paint_path(path, occurrence_fill);
        }

        // Paint indent guides — VS Code style: grid-aligned, whitespace-interpolated, active-block highlighted
        if state.indent_guides
            && state.mode.is_multi_line()
            && !state.masked
            && state.text.len() > 0
        {
            // 层级宽度跟随缩进配置（`TabSize`，默认 4 空格），而不是
            // 写死常量 —— 否则把缩进切成 2 空格后参考线仍按 4 列画。
            let indent_size = state.indent_width();
            let style = window.text_style();
            let font_size = state
                .font_size
                .unwrap_or_else(|| style.font_size.to_pixels(window.rem_size()));
            let space_width = window
                .text_system()
                .shape_line(
                    SharedString::from(" "),
                    font_size,
                    &[TextRun {
                        len: 1,
                        font: style.font(),
                        color: style.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
                .width;
            if space_width > px(0.) {
                // Tab 的真实推进宽度：默认缩进单元已是 4 空格，但打开的历史
                // 文件里仍可能有 Tab 缩进；若字体给 Tab 的推进量不等于
                // `indent_size` 个空格，按空格宽度线性外推算出来的参考线 x
                // 就会与正文整体错位。
                let tab_width = window
                    .text_system()
                    .shape_line(
                        SharedString::from("\t"),
                        font_size,
                        &[TextRun {
                            len: 1,
                            font: style.font(),
                            color: style.color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    )
                    .width
                    .max(space_width);

                let total_lines = state.text.lines_len();
                // 行首缩进列数（空格记 1 列，Tab 向上取整到 indent_size 的倍数）。
                let leading_columns = |line: &str| -> i32 {
                    let mut cols = 0;
                    for ch in line.chars() {
                        match ch {
                            ' ' => cols += 1,
                            '\t' => cols = cols - cols % indent_size as i32 + indent_size as i32,
                            _ => break,
                        }
                    }
                    cols
                };
                // 每级参考线的实际 x（相对正文左缘）：按前缀里空格 / Tab 的
                // 真实推进量累加，而不是 `space_width * 列数`。
                let guide_xs = |line: &str| -> Vec<Pixels> {
                    measure_guide_xs(line, indent_size, space_width, tab_width)
                };
                // 空行返回 -1：它们的参考线数量由邻近非空行插值得到。
                let line_indent = |row: usize| -> i32 {
                    let line = state.text.line_content(row).to_string();
                    if line.trim().is_empty() {
                        -1
                    } else {
                        leading_columns(&line)
                    }
                };

                // 只有可视区内的行会被真正画出来，所以把整段计算裁到这个窗口上。
                // 此前每一行都要 `slice_line` 成 String、再跑三趟 O(总行数) 的
                // 前缀/后缀扫描，10 万行文件每帧就是 10 万次堆分配 + 40 万次迭代。
                let cursor_line = state.cursor_position().line as usize;
                let mut win_start = prepaint
                    .last_layout
                    .visible_buffer_lines
                    .iter()
                    .copied()
                    .min()
                    .unwrap_or(0);
                let mut win_end = prepaint
                    .last_layout
                    .visible_buffer_lines
                    .iter()
                    .copied()
                    .max()
                    .map(|m| m + 1)
                    .unwrap_or(0);
                // 活跃块判定要从光标所在行出发，把光标行也纳入窗口。
                win_start = win_start.min(cursor_line).min(total_lines);
                win_end = win_end.max(cursor_line + 1).min(total_lines);
                win_end = win_end.max(win_start);
                let win_lines = win_end - win_start;

                let mut indents = Vec::with_capacity(win_lines);
                let mut indent_xs: Vec<Vec<Pixels>> = Vec::with_capacity(win_lines);
                for idx in win_start..win_end {
                    let line = state.text.line_content(idx).to_string();
                    if line.trim().is_empty() {
                        indents.push(-1);
                        indent_xs.push(Vec::new());
                    } else {
                        indents.push(leading_columns(&line));
                        indent_xs.push(guide_xs(&line));
                    }
                }

                // 窗口外紧邻的非空行缩进：空行要沿用邻近非空行的缩进，而
                // 位于窗口边缘的空行需要向窗口外看一眼。两个循环都在遇到
                // 第一个非空行时停下，正常情况下只走几行。
                let above_window = (0..win_start)
                    .rev()
                    .map(line_indent)
                    .find(|indent| *indent >= 0)
                    .unwrap_or(-1);
                let below_window = (win_end..total_lines)
                    .map(line_indent)
                    .find(|indent| *indent >= 0)
                    .unwrap_or(-1);

                // Guides per line with VS Code whitespace interpolation
                let off_side = matches!(state.language().as_deref(), Some("python") | Some("yaml"));
                let guides_counts = {
                    // 最近非空行的缩进与行号：空行的层数与参考线 x 都以它
                    // 为准。窗口边缘之外只能拿到缩进（行号 None），取不到
                    // 实测 xs，参考线 x 退回外推公式。
                    let mut prev_indent = vec![-1i32; win_lines];
                    let mut prev_row: Vec<Option<usize>> = vec![None; win_lines];
                    let mut last = above_window;
                    let mut last_row: Option<usize> = None;
                    for i in 0..win_lines {
                        if indents[i] >= 0 {
                            last = indents[i];
                            last_row = Some(win_start + i);
                        }
                        prev_indent[i] = last;
                        prev_row[i] = last_row;
                    }
                    let mut next_indent = vec![-1i32; win_lines];
                    let mut next_row: Vec<Option<usize>> = vec![None; win_lines];
                    let mut nxt = below_window;
                    let mut next_r: Option<usize> = None;
                    for i in (0..win_lines).rev() {
                        if indents[i] >= 0 {
                            nxt = indents[i];
                            next_r = Some(win_start + i);
                        }
                        next_indent[i] = nxt;
                        next_row[i] = next_r;
                    }
                    let mut counts = vec![0; win_lines];
                    for i in 0..win_lines {
                        let cur = indents[i];
                        counts[i] = if cur >= 0 {
                            (cur as usize).div_ceil(indent_size)
                        } else {
                            let above = prev_indent[i];
                            let below = next_indent[i];
                            match (above, below) {
                                (-1, _) | (_, -1) => 0,
                                (a, b) if a < b => (a as usize / indent_size) + 1,
                                (a, b) if a == b => (b as usize).div_ceil(indent_size),
                                (_, b) if off_side => (b as usize).div_ceil(indent_size),
                                (_, b) => (b as usize / indent_size) + 1,
                            }
                        };
                        // 空行的参考线 x 沿用决定层数的那个邻居的实测值。
                        // 原来按空格宽度外推：邻居是 Tab 缩进（或任何混合
                        // 缩进）时外推值必然偏离邻居实测值，空行上的参考线
                        // 整体偏移，竖线看起来断开/错位。邻居取缩进更大的
                        // 一侧，其 xs 覆盖插值出的全部层级。
                        if cur < 0 && counts[i] > 0 {
                            let src = if prev_indent[i] > next_indent[i] {
                                prev_row[i]
                            } else {
                                next_row[i]
                            };
                            if let Some(row) = src {
                                indent_xs[i] = indent_xs[row - win_start].clone();
                            }
                        }
                    }
                    counts
                };
                // 窗口外的行一律当作 0 层：活跃块的向外扩张因此会停在窗口边缘，
                // 而窗口本来就覆盖了所有可见行，绘制结果完全一致。
                let count_at = |row: usize| -> usize {
                    if row >= win_start && row < win_end {
                        guides_counts[row - win_start]
                    } else {
                        0
                    }
                };
                // Active indent block for the cursor line.
                //
                // 区间统一用 **0-based 半开区间** `[active_start, active_end)`，
                // 旧实现里 `start` 是 1-based 闭区间、`end` 却是 0-based 开区间，
                // 两种约定混在一处，边界极易差一行。
                let (active_start, active_end, active_level) = if cursor_line >= total_lines {
                    (0, 0, 0)
                } else {
                    let mut level = count_at(cursor_line);
                    // 0 行的半开区间：只含光标行本身。
                    let mut start = cursor_line;
                    let mut end = cursor_line + 1;
                    if level == 0 && cursor_line + 1 < total_lines && count_at(cursor_line + 1) == 1
                    {
                        // 光标停在块的第一行（缩进尚未展开）：整块从下一行起算。
                        level = 1;
                        start = cursor_line + 1;
                        end = start + 1;
                    }
                    if level == 0 {
                        (0, 0, 0)
                    } else {
                        // 向上并吞所有缩进不低于本层的行（含首行）。
                        while start > 0 && count_at(start - 1) >= level {
                            start -= 1;
                        }
                        while end < total_lines && count_at(end) >= level {
                            end += 1;
                        }
                        (start, end, level)
                    }
                };
                let base_color = cx.theme().border.opacity(0.28);
                let active_color = cx.theme().border.opacity(0.85);
                let gutter_width = prepaint.last_layout.line_number_width;
                let mut gy = invisible_top_padding;
                for (vi, &buffer_line) in
                    prepaint.last_layout.visible_buffer_lines.iter().enumerate()
                {
                    // `lines` 与 `visible_buffer_lines` 正常情况下一一对应，
                    // 但空文本分支是按 placeholder 生成的，长度可能不等价 ——
                    // 直接下标会在绘制期 panic。
                    let Some(line) = prepaint.last_layout.lines.get(vi) else {
                        break;
                    };
                    // git 展开区：锚点行上的块高先行（与正文累计一致）。
                    gy += line_height * prepaint.last_layout.git_insert_delta(vi) as f32;
                    let line_h = line.size(line_height).height;
                    let line_y = origin.y + gy;
                    let count = count_at(buffer_line);
                    let xs = if buffer_line >= win_start && buffer_line < win_end {
                        indent_xs[buffer_line - win_start].as_slice()
                    } else {
                        &[]
                    };
                    for level in 1..=count {
                        // 与 Zed 一致：第 k 层参考线画在缩进单元的起始列
                        // （0、indent_size、2*indent_size…，即
                        // `single_indent_width * depth`），不偏移。
                        let visible_column = (level - 1) * indent_size;
                        // 优先用实测推进量；层级超出缩进前缀（窗口边缘的
                        // 空行插值）时退回按空格宽度外推。外推基准是
                        // `visible_column` 本身——列 0 处 x 为 0，空格缩进
                        // 时实测值恰好是 `space_width * visible_column`。
                        let offset = match xs.get(level - 1) {
                            Some(x) => *x,
                            None => space_width * visible_column as f32,
                        };
                        let x = origin.x + gutter_width + offset;
                        if x >= input_bounds.right() {
                            break;
                        }
                        // 半开区间：[active_start, active_end)
                        let is_active = level == active_level
                            && active_level != 0
                            && buffer_line >= active_start
                            && buffer_line < active_end;
                        let color = if is_active { active_color } else { base_color };
                        window.paint_quad(fill(
                            Bounds::new(point(x, line_y), size(px(1.), line_h)),
                            color,
                        ));
                    }
                    gy += line_h;
                }
            }
        }

        // Paint text
        let mut offset_y = invisible_top_padding;

        // Keep scrollbar offset always be positive，Start from the left position
        let scroll_offset = if text_align == TextAlign::Right {
            (prepaint.scroll_size.width - prepaint.bounds.size.width).max(px(0.))
        } else if text_align == TextAlign::Center {
            (prepaint.scroll_size.width - prepaint.bounds.size.width)
                .half()
                .max(px(0.))
        } else {
            px(0.)
        };

        // Fold-related click targets (placeholder pills + gutter icons).
        let mut fold_click_targets: Vec<(usize, Bounds<Pixels>)> = Vec::new();
        let fold_placeholder_bg = cx.theme().text.opacity(0.055);

        // Inlay Hint 的渲染素材：在循环外取一次，避免每行都读一次实体锁、
        // 也不必每行都重算字体。空时不进入绘制分支。
        // 克隆一份：hint 只在可见行上，量很小；而下面的绘制循环要反复
        // 可变借用 `cx`，持着实体读锁过不去借用检查。
        let inlay_hints_by_line = self.state.read(cx).inlay_hints_by_line().clone();
        let inlay_style = window.text_style();
        let inlay_font = inlay_style.font();
        let inlay_font_size = self
            .state
            .read(cx)
            .font_size
            .unwrap_or_else(|| inlay_style.font_size.to_pixels(window.rem_size()));

        for (i, (line, buffer_line)) in prepaint
            .last_layout
            .lines
            .iter()
            .zip(prepaint.last_layout.visible_buffer_lines.iter())
            .enumerate()
        {
            offset_y += line_height * prepaint.last_layout.git_insert_delta(i) as f32;
            let line_y = origin.y + offset_y;
            let p = point(
                origin.x + prepaint.last_layout.line_number_width + (scroll_offset),
                line_y,
            );

            // Paint the actual line
            line.paint(
                p,
                line_height,
                text_align,
                Some(prepaint.last_layout.content_width),
                window,
                cx,
            );

            // LSP Inlay Hint：画在该行正文之后（行尾追加式）。
            //
            // 真正的 inlay 插在字符之间，需要把一行切成两段分别 shape 再拼
            // 回去；首版用「行尾追加」这个更简单的形态——类型标注类 hint 的
            // 语义基本保留，而实现成本与改动风险都低一个量级。
            if let Some(hints) = inlay_hints_by_line.get(&buffer_line) {
                let mut hint_x = p.x + line.longest_width;
                for hint in hints {
                    let run = window.text_system().shape_line(
                        gpui::SharedString::from(hint.text.clone()),
                        inlay_font_size,
                        &[TextRun {
                            len: hint.text.chars().count(),
                            font: inlay_font.clone(),
                            color: cx.theme().ui.text_muted,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    _ = run.paint(
                        point(hint_x, line_y),
                        line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    hint_x += run.width;
                }
            }

            offset_y += line.size(line_height).height;
        }

        // LSP 悬浮浮层：按 Zed 的 `layout_hover_popovers` 定位。
        //
        // 关键点：**在绘制时**用当前帧的行布局把文本锚点换算成像素，而不是
        // 事先存一个像素坐标。滚动/软换行/行号栏宽度变化都会让事先算好的
        // 坐标失效，这正是之前浮层位置错乱的原因。
        // LSP 浮层/菜单不在这里画：它们在 prepaint 阶段已通过
        // `window.defer_draw(...)` 交给 gpui，由其 deferred 队列统一
        // prepaint + paint。在 paint 里再调 `defer_draw` 会入队一个
        // 从未 prepaint 的元素，触发 "must call prepaint before paint"。

        // Paint the `⋯` pills and their trailing texts (e.g. `}`) inline at the
        // end of each folded header line — zed's fold placeholder replaces the
        // folded range, keeping the closing delimiter visible on the same
        // display row (`fn main() {⋯}`).
        for placeholder in &prepaint.fold_placeholders {
            let Some(pill_bounds) = placeholder.pill_bounds else {
                continue;
            };
            window.paint_quad(fill(pill_bounds, fold_placeholder_bg).corner_radii(px(2.)));
            _ = placeholder.ellipsis.paint(
                point(
                    pill_bounds.origin.x + FOLD_PLACEHOLDER_PADDING,
                    pill_bounds.origin.y,
                ),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
            // Clicking the placeholder unfolds this fold (zed's
            // `FoldPlaceholder::fold_element` on_click → unfold_ranges).
            fold_click_targets.push((placeholder.row, pill_bounds));
            if let Some(hitbox) = placeholder.hitbox.as_ref() {
                window.set_cursor_style(CursorStyle::PointingHand, hitbox);
            }

            if let Some(tail) = placeholder.tail.as_ref() {
                _ = tail.paint(
                    point(pill_bounds.right(), pill_bounds.origin.y),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }

        // Paint blinking cursor (Zed `CursorLayout::paint` + VSCode cursorStyle).
        // 闪烁不透明度由 `cursor_opacity` 给出（相位末尾淡出/淡入）；透明度
        // 为 0 时跳过绘制，避免画一个全透明的光标。
        //
        // 用 `should_paint_cursor` 而非单纯的 `blink.visible()`：淡入窗口内
        // 后者仍为 false，但透明度已大于 0，必须绘制才不会「到点突然出现」。
        if paint_cursor && cursor_opacity > 0.0 {
            if let Some(mut cursor_layout) = prepaint.cursor_bounds_with_scroll() {
                cursor_layout.paint_with_opacity(cursor_opacity, window, cx);
            }
        }

        // Paint line numbers
        let mut offset_y = px(0.);
        if let Some(line_numbers) = prepaint.line_numbers.as_ref() {
            offset_y += invisible_top_padding;

            // 诊断 gutter 标记的数据源。在循环外取一次：循环里每行都去
            // `self.state.read(cx)` 会把渲染路径变成 O(可见行) 次实体读锁。
            let diagnostics_for_gutter = self
                .state
                .read(cx)
                .diagnostics()
                .filter(|d| !d.is_empty())
                .cloned();

            window.paint_quad(fill(
                Bounds {
                    origin: input_bounds.origin,
                    size: size(
                        prepaint.last_layout.line_number_width - LINE_NUMBER_RIGHT_MARGIN,
                        input_bounds.size.height,
                    ),
                },
                cx.theme().editor_background(),
            ));

            // Each item is the normal lines.
            for (i, (lines, &buffer_line)) in line_numbers
                .iter()
                .zip(prepaint.last_layout.visible_buffer_lines.iter())
                .enumerate()
            {
                // git 展开区：与正文累计保持一致（查表）。
                offset_y += line_height * prepaint.last_layout.git_insert_delta(i) as f32;
                let p = point(input_bounds.origin.x, origin.y + offset_y);
                let is_active = prepaint.current_row == Some(buffer_line);

                let height = line_height * lines.len() as f32;
                // Gutter cell background: diff rows always carry their tint —
                // deepened when the row is selected or active, so the
                // active-line gray / selection never hide the hue — and the
                // kind bar paints last, on top of it all.
                let diff_entry = self
                    .state
                    .read(cx)
                    .diff_line_colors
                    .get(&(buffer_line as u32))
                    .copied();
                // 该行的最高诊断级别（gutter 标记）。行区间用**内容区间**
                // （不含换行符）：否则行尾诊断会把下一行的换行符也算进来。
                let diagnostic_severity_for_line = diagnostics_for_gutter.as_ref().and_then(|d| {
                    let state = self.state.read(cx);
                    let line_range = state.text.line_start_offset(buffer_line)
                        ..state.text.line_content_end_offset(buffer_line);
                    d.max_severity_for_range(&line_range)
                });
                let row_selected = selected_byte_range.as_ref().is_some_and(|sel| {
                    let state = self.state.read(cx);
                    let row_start = state.text.line_start_offset(buffer_line);
                    let row_end = state.text.line_start_offset(buffer_line + 1);
                    row_start < sel.end && sel.start < row_end
                });
                // 与上方正文区激活行同一组条件（含焦点）：保证 gutter 与
                // 正文要么都画加深色、要么都不画。
                let active_plain =
                    focused && !single_line && is_active && selected_range.is_empty();
                let cell_bg = match diff_entry {
                    Some((tint, kind)) => {
                        let bg = if row_selected || active_plain {
                            match kind {
                                DiffLineKind::Added => deep_added,
                                DiffLineKind::Deleted => deep_deleted,
                            }
                        } else {
                            tint
                        };
                        Some(bg)
                    }
                    None => {
                        if self
                            .state
                            .read(cx)
                            .line_in_expanded_hunk(buffer_line as u32)
                        {
                            // 已展开 hunk 的新行：行号区同绿底，与正文一致。
                            Some(cx.theme().status_added.opacity(0.14))
                        } else {
                            active_plain.then_some(active_line_color).flatten()
                        }
                    }
                };
                if let Some(bg) = cell_bg {
                    let gutter_w =
                        prepaint.last_layout.line_number_width - LINE_NUMBER_RIGHT_MARGIN;
                    window.paint_quad(fill(Bounds::new(p, size(gutter_w, height)), bg));
                }
                if let Some((_, line_kind)) = diff_entry {
                    let edge_color = match line_kind {
                        crate::components::input::DiffLineKind::Added => cx.theme().status_added,
                        crate::components::input::DiffLineKind::Deleted => {
                            cx.theme().status_deleted
                        }
                    };
                    match line_kind {
                        crate::components::input::DiffLineKind::Added => {
                            window
                                .paint_quad(fill(Bounds::new(p, size(px(3.), height)), edge_color));
                        }
                        crate::components::input::DiffLineKind::Deleted => {
                            // 条纹短划（2px 划 + 2px 空）铺满行高，参考 diff UI
                            // 用纹理区分"此行在另一侧不存在"。
                            let (bar_w, dash, gap) = (px(3.), px(2.), px(2.));
                            let end = p.y + height;
                            let mut dash_y = p.y;
                            while dash_y < end {
                                let h = (end - dash_y).min(dash);
                                window.paint_quad(fill(
                                    Bounds::new(point(p.x, dash_y), size(bar_w, h)),
                                    edge_color,
                                ));
                                dash_y += dash + gap;
                            }
                        }
                    }
                }

                // 诊断 gutter 标记：贴行号区**右缘**，与左缘的 git 色条分离，
                // 两者可同时存在（改一行且该行有语法错误）。
                if let Some(severity) = diagnostic_severity_for_line {
                    let gutter_w =
                        prepaint.last_layout.line_number_width - LINE_NUMBER_RIGHT_MARGIN;
                    let mark_w = px(2.);
                    window.paint_quad(fill(
                        Bounds::new(point(p.x + gutter_w - mark_w, p.y), size(mark_w, height)),
                        severity.color(cx),
                    ));
                }

                for line in lines {
                    _ = line.paint(p, line_height, TextAlign::Left, None, window, cx);
                    offset_y += line_height;
                }

                // Paint git 行首色条：新增绿 / 修改蓝 / 删除红（行顶小短条）。
                // 必须画在行号区背景之后（背景全高后涂会盖住先画的色条），
                // 因此放在行号绘制循环内。
                let git_gutter_enabled = self.state.read(cx).git_gutter;
                if git_gutter_enabled {
                    let (mark, expandable_here, deletion_expanded, hunk_expanded) = {
                        let state = self.state.read(cx);
                        let line_u32 = buffer_line as u32;
                        let deletion_expanded = state.expanded_git_hunks.iter().any(|&ix| {
                            state
                                .git_hunks
                                .get(ix)
                                .is_some_and(|h| h.new_rows.is_empty() && h.before_line == line_u32)
                        });
                        (
                            state.git_line_marks.get(&line_u32).copied(),
                            state
                                .git_expandable
                                .then(|| state.git_hunk_index_for_line(line_u32))
                                .flatten()
                                .is_some(),
                            deletion_expanded,
                            state.line_in_expanded_hunk(line_u32),
                        )
                    };
                    if let Some(mark) = mark {
                        // 行首竖线：贴行最左缘，与 diff view 的行首色条同款。
                        // hunk 展开时其中的新增/修改行色条转绿（此时它们扮演
                        // 「增加」一侧，蓝条只用于收起态提示「此处有修改」）。
                        let bar_color = match mark {
                            GitLineMark::Added => cx.theme().status_added,
                            GitLineMark::Modified => {
                                if hunk_expanded {
                                    cx.theme().status_added
                                } else {
                                    cx.theme().git_modified
                                }
                            }
                            GitLineMark::Deleted => cx.theme().status_deleted,
                        };
                        if mark != GitLineMark::Deleted {
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(input_bounds.origin.x, p.y),
                                    size(px(3.), height),
                                ),
                                bar_color,
                            ));
                        } else {
                            // 此处上方有删除行：行顶小短条示意（展开中则省略）
                            if !deletion_expanded {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(input_bounds.origin.x, p.y),
                                        size(px(7.), px(3.)),
                                    ),
                                    bar_color,
                                ));
                            }
                        }
                        if expandable_here {
                            // hitbox 在 prepaint 创建（paint 阶段只能引用）。
                            if let Some((target_line, hit_bounds, hitbox)) = prepaint
                                .git_bar_hitboxes
                                .iter()
                                .find(|(l, _, _)| *l == buffer_line as u32)
                            {
                                if hitbox.is_hovered(window) {
                                    window.set_cursor_style(CursorStyle::PointingHand, hitbox);
                                }
                                git_bar_click_targets.push((*target_line, *hit_bounds));
                            }
                        }
                    }
                }
            }
        }

        // Paint git 展开区：被删除的旧行以红底插入 hunk 上方（只读，
        // 与 diff view 删除行同色：0.14 行底 + 行首红条 + 红色旧行号）。
        if !prepaint.git_insert_rows.is_empty() {
            let style = window.text_style();
            let insert_font = style.font();
            let insert_font_size = self
                .state
                .read(cx)
                .font_size
                .unwrap_or_else(|| style.font_size.to_pixels(window.rem_size()));
            let number_len = prepaint.last_layout.line_number_len;
            let show_line_numbers = self.state.read(cx).mode.line_number();
            let text_x = origin.x + prepaint.last_layout.line_number_width;
            let viewport_bottom = origin.y + bounds.size.height;

            // 一次性 shape 全部插入行（与 `git_insert_rows` 一一对应；
            // 短作用域读 state，不跨 paint）。
            let shaped_rows: Vec<(ShapedLine, ShapedLine)> = prepaint
                .git_insert_rows
                .iter()
                .map(|row| {
                    let state = self.state.read(cx);
                    let Some(del) = state
                        .git_hunks
                        .get(row.hunk_ix)
                        .and_then(|hunk| hunk.deleted.get(row.row_ix))
                    else {
                        return (ShapedLine::default(), ShapedLine::default());
                    };
                    let deleted_color = cx.theme().status_deleted;
                    let text_color = cx
                        .theme()
                        .highlight_theme
                        .style
                        .editor_foreground
                        .unwrap_or_else(|| cx.theme().text);
                    let label: SharedString =
                        format!("{:>width$}", del.old_lineno, width = number_len).into();
                    let number_runs = window.text_system().shape_line(
                        label.clone(),
                        insert_font_size,
                        &[TextRun {
                            len: label.len(),
                            font: insert_font.clone(),
                            color: deleted_color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let text: SharedString = del.text.as_str().into();
                    let text_line = window.text_system().shape_line(
                        text.clone(),
                        insert_font_size,
                        &[TextRun {
                            len: text.len(),
                            font: insert_font.clone(),
                            color: text_color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    (text_line, number_runs)
                })
                .collect();

            for (row, (text_line, number_line)) in prepaint.git_insert_rows.iter().zip(shaped_rows)
            {
                // prepaint 中 y 为相对坐标，这里换算为绝对 y。
                let row_y = origin.y + row.y;
                if row_y + line_height <= origin.y || row_y >= viewport_bottom {
                    continue;
                }
                let p = point(input_bounds.origin.x, row_y);
                let deleted_color = cx.theme().status_deleted;
                // 整行红底（从行号区左缘铺到右缘，单层 0.14，与 diff view
                // 删除行同色；行号区不再单独叠一层，避免两段深浅不一）。
                window.paint_quad(fill(
                    Bounds::new(p, size(bounds.size.width, line_height)),
                    deleted_color.opacity(0.14),
                ));
                // 旧行号（红色，画在行底之上）。
                if show_line_numbers {
                    _ = number_line.paint(p, line_height, TextAlign::Left, None, window, cx);
                }
                // 行首红条。
                window.paint_quad(fill(
                    Bounds::new(p, size(px(3.), line_height)),
                    deleted_color,
                ));
                // 被删除的旧行文本（只读展示，从正文左缘起画）。
                _ = text_line.paint(
                    point(text_x, row_y),
                    line_height,
                    TextAlign::Left,
                    Some(prepaint.last_layout.content_width),
                    window,
                    cx,
                );
            }
        }
        // Paint fold icons (only visible on hover or for current line)
        fold_click_targets.extend(self.paint_fold_icons(
            &mut prepaint.fold_icon_layout,
            prepaint.current_row,
            window,
            cx,
        ));

        // Paint sticky scroll：顶部钉住外围定义行（半透明底 + 底边框，点击跳转）
        let mut sticky_click_targets: Vec<(usize, Bounds<Pixels>)> = Vec::new();
        if !prepaint.sticky_lines.is_empty() {
            let sticky_h = line_height * prepaint.sticky_lines.len() as f32;
            window.paint_quad(fill(
                Bounds::new(input_bounds.origin, size(input_bounds.size.width, sticky_h)),
                cx.theme().editor_background().opacity(0.92),
            ));
            window.paint_quad(fill(
                Bounds::new(
                    point(
                        input_bounds.origin.x,
                        input_bounds.origin.y + sticky_h - px(1.),
                    ),
                    size(input_bounds.size.width, px(1.)),
                ),
                cx.theme().border,
            ));
            for sticky in &prepaint.sticky_lines {
                let y = sticky.bounds.origin.y;
                // 行号（与主行号同位左对齐）
                _ = sticky.number.paint(
                    point(input_bounds.origin.x, y),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                // 文本（与正文同起点）
                _ = sticky.text.paint(
                    point(
                        input_bounds.origin.x + prepaint.last_layout.line_number_width,
                        y,
                    ),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                sticky_click_targets.push((sticky.buffer_line, sticky.bounds));
            }
        }

        // Paint scrollbar：右侧悬浮滚动条（内容超出视口时显示，支持拖拽）。
        // 几何/尺寸/颜色与 markdown 编辑器的纵向滚动条同款（见
        // `editor::markdown_live::scrollbar` 的模块注释），改那边时同步这里。
        let mut scrollbar_geometry: Option<ScrollbarGeometry> = None;
        if scrollbar_enabled {
            let viewport_h = input_bounds.size.height;
            let content_h = prepaint.scroll_size.height;
            // `thumb_metrics` 吃的是 gpui 的原始 offset（向下滚为负）。
            if let Some((thumb_start, thumb_h)) =
                thumb_metrics(viewport_h, content_h, -scrollbar_scroll_y)
            {
                let thumb_bounds = Bounds::new(
                    point(
                        input_bounds.right() - BAR_THICKNESS,
                        input_bounds.origin.y + thumb_start,
                    ),
                    size(BAR_THICKNESS, thumb_h),
                );
                let dragging = scrollbar_dragging;
                let theme = cx.theme();
                let thumb_color = if dragging {
                    theme.border_strong
                } else {
                    theme.border.opacity(0.7)
                };
                window.paint_quad(fill(thumb_bounds, thumb_color).corner_radii(THUMB_RADIUS));
                scrollbar_geometry = Some(ScrollbarGeometry {
                    thumb: thumb_bounds,
                    viewport_len: viewport_h,
                    thumb_len: thumb_h,
                    content_len: content_h,
                });
            }
        }

        // 底部横向滚动条（Zed `scrollbar.axes.horizontal` 默认开）：长行不软
        // 换行后靠它 + 水平滚轮/触控板查看行尾。几何/拖拽换算与纵向条共用
        // `thumb_metrics` / `scroll_for_drag`（`offset.x` 向右滚为负，正好是
        // `thumb_metrics` 要的符号）。
        let mut scrollbar_geometry_h: Option<ScrollbarGeometry> = None;
        if scrollbar_enabled {
            // 条带贴底、右端给纵向条让位。视口/内容**同减** `BAR_THICKNESS`，
            // 于是 `content' - viewport' = content - viewport`：thumb 到端点时
            // 的滚动量与 `update_scroll_offset` 的钳制完全一致，不会脱节。
            let viewport_w = input_bounds.size.width - BAR_THICKNESS;
            let content_w = prepaint.scroll_size.width - BAR_THICKNESS;
            if let Some((thumb_start, thumb_w)) =
                thumb_metrics(viewport_w, content_w, scrollbar_scroll_x)
            {
                let thumb_bounds = Bounds::new(
                    point(
                        input_bounds.origin.x + thumb_start,
                        input_bounds.origin.y + input_bounds.size.height - BAR_THICKNESS,
                    ),
                    size(thumb_w, BAR_THICKNESS),
                );
                let dragging = scrollbar_dragging_h;
                let theme = cx.theme();
                let thumb_color = if dragging {
                    theme.border_strong
                } else {
                    theme.border.opacity(0.7)
                };
                window.paint_quad(fill(thumb_bounds, thumb_color).corner_radii(THUMB_RADIUS));
                scrollbar_geometry_h = Some(ScrollbarGeometry {
                    thumb: thumb_bounds,
                    viewport_len: viewport_w,
                    thumb_len: thumb_w,
                    content_len: content_w,
                });
            }
        }

        self.state.update(cx, |state, cx| {
            state.scrollbar_geometry = scrollbar_geometry;
            state.scrollbar_geometry_h = scrollbar_geometry_h;
            state.sticky_click_targets = sticky_click_targets;
            state.fold_click_targets = fold_click_targets;
            state.git_bar_click_targets = git_bar_click_targets;
            state.last_layout = Some(prepaint.last_layout.clone());
            state.last_bounds = Some(bounds);
            state.last_cursor = Some(state.cursor());
            state.set_input_bounds(input_bounds, cx);
            state.last_selected_range = Some(selected_range);
            state.scroll_size = prepaint.scroll_size;
            let scroll_changed =
                state.update_scroll_offset(Some(prepaint.cursor_scroll_offset), cx);
            state.deferred_scroll_offset = None;

            // 这里曾经无条件 `cx.notify()`：paint 阶段标脏会让实体在下一帧
            // 重新走一遍 prepaint + paint，空闲时也是满帧空转，而 paint 里还
            // 叠着若干 O(总行数) 的扫描。
            //
            // 只有滚动位置真的动了才需要再来一帧 —— 钳制后的偏移与本次布局
            // 采用的值不同，可见区要按最终值重算。其余状态（滚动条几何、点击
            // 热区、布局缓存）都只影响后续事件处理，不需要重绘。
            // 光标闪烁走独立的 `blink_cursor` 实体定时器，不依赖这里的 notify。
            if scroll_changed {
                cx.notify();
            }
        });

        self.paint_mouse_listeners(window, cx);
    }

    /// 布局 LSP 浮层与补全菜单（照抄 Zed 的 `layout_hover_popovers`）。
    ///
    /// **必须在 prepaint 阶段调用**：`layout_as_root` 只允许在
    /// request_layout / prepaint 里跑，在 paint 里调会 panic
    /// （"this method can only be called during request_layout, or prepaint"）。
    ///
    /// 算法与 Zed 一致：
    /// 1. 锚点（字节偏移）→ 可见行下标 + 行内列；
    /// 2. `x` 由该行 `position_for_index(列)` 给出，`y` = 累计行高；
    /// 3. 减去滚动偏移、加上内容原点 → **窗口坐标**；
    /// 4. 先尝试画在锚点行上方，放不下再翻到下方（Zed 的
    ///    `can_place_above` / `can_place_below`）。
    fn layout_lsp_popovers(
        &self,
        viewport: Bounds<Pixels>,
        scroll_offset: gpui::Point<Pixels>,
        last_layout: &LastLayout,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 滚过的内容原点：把「未滚动视口」平移滚动量得到，与正文绘制
        // （`paint_inner` 用 `prepaint.bounds`）一致。
        // 这样调用方只需给一个矩形，不会再出现「传错矩形」的错误。
        let bounds = Bounds::new(viewport.origin + scroll_offset, viewport.size);
        // 可见区的**结束字节**：最后一条可见行的下一行行首（钳到文本长度）。
        // 用它界定最后一条可见行——用 `text_len` 不行：那会把视口之后的
        // 所有偏移都算到最后一行上，滚动后浮层就粘在底部不消失。
        let visible_end = {
            let state = self.state.read(cx);
            let text = state.text();
            match last_layout.visible_buffer_lines.last() {
                Some(&last_row) => text.line_start_offset(last_row + 1).min(text.len()),
                None => 0,
            }
        };

        // 补全菜单优先：它在打字时出现，比 hover 更需要即时可见。
        if let Some((anchor, mut content)) = self.state.update(cx, |state, cx| {
            let offset = state.completion_anchor()?;
            let content = crate::editor::completion::render_completion_menu_from_state(state, cx)?;
            Some((offset, content))
        }) && let Some((origin, anchor_y, size)) = self.place_at_anchor(
            anchor,
            &mut content,
            bounds,
            viewport,
            last_layout,
            line_height,
            visible_end,
            true,
            // 补全菜单不换行：行是 `whitespace_nowrap`，量出来多宽画多宽。
            None,
            window,
            cx,
        ) {
            // 补全菜单不需要记矩形：它只有几行、从不滚动，滚轮穿透反而是
            // 期望行为（在菜单上滚动应当滚动背后的代码）。
            let _ = size;
            // `defer_draw` **必须**在 prepaint 阶段调用：它把元素入队，
            // gpui 随后对队列里的元素统一跑 prepaint + paint。放到 paint
            // 阶段调用会入队一个从未 prepaint 的元素，触发
            // "must call prepaint before paint"。
            window.defer_draw(content, origin, 2, None);
            self.state.update(cx, |state, _cx| {
                state.record_popover_placement(origin, anchor_y)
            });
        }

        // hover 浮层：优先画在锚点行上方（不遮住正在看的词）。
        //
        // **必须先在 `update` 里取齐数据、出闭包再渲染**：卡片渲染要拿
        // `WeakEntity`（给鼠标回调回写 state），而同一个实体在 `update`
        // 期间再被 `read` 会 panic
        // （`entity_map.rs`: cannot read ... while it is already being updated）。
        // 借用也不能带出 `update`，所以这里克隆一份 `HoverInfo` —— 它内部的
        // markdown 实体与滚动句柄都是共享引用，克隆很便宜。
        let Some((anchor, info, weak_state)) = self.state.update(cx, |state, cx| {
            let (offset, info) = state.hover_popover()?;
            Some((offset, info.clone(), cx.entity().downgrade()))
        }) else {
            return;
        };
        let Some(mut content) =
            crate::editor::hover::render_hover_card(&info, window, weak_state, cx)
        else {
            return;
        };
        if let Some((origin, anchor_y, size)) = self.place_at_anchor(
            anchor,
            &mut content,
            bounds,
            viewport,
            last_layout,
            line_height,
            visible_end,
            false,
            // hover 卡片按 `MAX_WIDTH` **定宽**测量：文本只有拿到定宽才会换行
            //（详见 `place_at_anchor` 的 `wrap_at` 注释）。`MAX_WIDTH` 见 hover.rs。
            Some(px(crate::editor::hover::MAX_WIDTH)),
            window,
            cx,
        ) {
            window.defer_draw(content, origin, 2, None);
            self.state.update(cx, |state, _cx| {
                state.record_popover_placement(origin, anchor_y);
                // 记下卡片实际占的矩形：滚轮事件冒泡到 `on_scroll_wheel` 时
                // 靠它判断是否该放行给浮层自己滚。
                state.record_hover_popover_bounds(Bounds { origin, size });
            });
        }
    }

    /// 把浮层放在锚点行附近，返回它的绝对位置。
    ///
    /// `prefer_below` 为 true 时优先画在锚点行下方（补全菜单：跟着光标往下
    /// 铺），否则优先上方（hover：不遮住正在读的那一行）。
    #[allow(clippy::too_many_arguments)]
    fn place_at_anchor(
        &self,
        anchor_offset: usize,
        content: &mut gpui::AnyElement,
        bounds: Bounds<Pixels>,
        viewport: Bounds<Pixels>,
        last_layout: &LastLayout,
        line_height: Pixels,
        visible_end: usize,
        prefer_below: bool,
        // 宽度上限，同时表示「按它换行」；`None` = 不限宽也不换行（补全菜单的
        // 行本来就是 `whitespace_nowrap`，量出来多宽就画多宽）。
        //
        // 给了 `wrap_at` 就必须量**定宽**（`AvailableSpace::Definite`）：gpui
        // 的文本只有拿到 `Definite` 才会换行 —— `elements/text.rs::TextLayout::layout`
        // 的 `wrap_width` 只在 Definite 下才有值，用 `MinContent` / `MaxContent`
        // 量到的永远是「不换行时的自然宽度」。
        //
        // 「先 MinContent 量一遍、超宽了再补一遍 Definite」这条走不通：卡片自己
        // 的 `max_w` 已经把第一遍的结果夹成 680，`680 > 680` 恒为假，定宽那一遍
        // 永远不执行 —— 于是卡片 680 宽、里面只有一行高，长段落与代码块全溢出
        // 到卡片外面（用户报的「文本不换行 / 代码块比浮窗宽」）。
        //
        // `max_h` 的截断与滚动不受影响（`MaxContent` 只是不给高度上限）。
        wrap_at: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<(gpui::Point<Pixels>, Pixels, gpui::Size<Pixels>)> {
        // 锚点必须落在**可见区**内才画。
        //
        // `visible_line_byte_offsets` 只覆盖可见区，最后一条可见行没有
        // 「下一条起点」，所以要显式给上界；否则视口外的锚点会被算到
        // 最后一个可见行上（滚动后浮层粘在底部不消失）。
        let row_ix = visible_row_for_offset(
            &last_layout.visible_line_byte_offsets,
            anchor_offset,
            visible_end,
        )?;

        // 行顶与行高：与正文绘制**逐项对齐**（见 `row_top_in_viewport`）。
        let (row_top, row_height) =
            row_top_in_viewport(last_layout, row_ix, line_height, bounds.origin.y);

        // 行内列 → 精确 x（`position_for_index` 处理软换行与对齐偏移）。
        let col = anchor_offset.saturating_sub(last_layout.visible_line_byte_offsets[row_ix]);
        let x_in_line = last_layout
            .lines
            .get(row_ix)
            .and_then(|l| l.position_for_index(col, last_layout, false))
            .map(|p| p.x)
            .unwrap_or(px(0.));
        let x = bounds.origin.x + last_layout.line_number_width + x_in_line;
        let anchor_y = row_top;

        // 量出尺寸以决定放哪边。
        //
        // 定宽量一遍即可（为什么不能两遍，见上面 `wrap_at` 的注释）：文本按
        // `max_w` 换行，`paint` 用的就是这次 layout 的结果。
        //
        // 「定宽会不会把浮层撑满 `max_w`」—— 不会。浮层是 flex 容器，宽度取
        // 子内容的 cross size（taffy `determine_container_cross_size` 只用
        // `known_dimensions` / 各行 cross，不用可用宽度），可用宽度只作为**子内容**
        // 的约束往下传（`determine_available_space`）。所以短内容仍然只占内容宽；
        // 超宽内容由卡片自己的 `max_w` 夹到上限，同时子内容拿到 `max_w - padding`
        // 的定宽去换行。
        let size = match wrap_at {
            Some(max_w) => content.layout_as_root(
                Size {
                    width: AvailableSpace::Definite(max_w),
                    height: AvailableSpace::MaxContent,
                },
                window,
                cx,
            ),
            None => content.layout_as_root(AvailableSpace::min_size(), window, cx),
        };

        // 视口用**未滚动**的 `viewport`：`bounds` 含滚动偏移，拿它算
        // 「上方/下方放得下」会把判定整体平移一个滚动量。
        let y = popover_y(
            anchor_y,
            row_height,
            size.height,
            viewport.origin.y,
            viewport.origin.y + viewport.size.height,
            prefer_below,
        );

        // 一并返回锚点行屏幕 y：调用方把它记进 state，测试据此判断
        // 「浮层在锚点上方还是下方」，无需在测试里重算坐标公式。
        //
        // 尺寸也一并带出：hover 卡片要靠它把**实际矩形**记进 state，
        // `InputState::on_scroll_wheel` 才能判断「滚轮是不是落在卡片上」
        // （见 `InputState::record_hover_popover_bounds`）。
        Some((point(x, y), anchor_y, size))
    }
}

/// 决定浮层画在哪一边，返回它的 y（窗口坐标）。
///
/// `prefer_below` 为 true 时优先画在锚点行**下方**（补全菜单：跟着光标往下
/// 铺），为 false 时优先**上方**（hover：不遮住正在读的那一行）。
///
/// 判定顺序与 Zed 的 `can_place_above` / `can_place_below` 一致：
/// 优先方向放得下就用优先方向，放不下翻到另一边，两边都放不下则夹进视口。
///
/// 抽成纯函数是因为「上下翻转」这类错误在真实绘制里很难断言——
/// 而它恰恰是最容易写反的一步（曾经把 `prefer_below == false` 的两支写反，
/// 结果底部代码的浮层跑到屏幕外下方、顶部代码的跑到屏幕外上方）。
pub(crate) fn popover_y(
    anchor_y: Pixels,
    row_height: Pixels,
    popover_height: Pixels,
    viewport_top: Pixels,
    viewport_bottom: Pixels,
    prefer_below: bool,
) -> Pixels {
    let below = anchor_y + row_height;
    let above = anchor_y - popover_height - HOVER_POPOVER_GAP;
    let fits_below = below + popover_height <= viewport_bottom;
    let fits_above = above >= viewport_top;

    match (prefer_below, fits_below, fits_above) {
        // 优先下方：下方放得下就下方，否则翻上方。
        (true, true, _) => below,
        (true, false, true) => above,
        // 优先上方：上方放得下就上方，否则翻下方。
        (false, _, true) => above,
        (false, true, false) => below,
        // 两边都放不下：夹进视口顶部，至少不跑出屏幕。
        _ => above.max(viewport_top),
    }
}

/// 计算可见行 `row_ix` 在**视口内的行顶 y** 与它的高度。
///
/// 公式与 `paint_inner` 的正文绘制循环逐项对齐：
///
/// ```text
/// row_top = bounds.origin.y + visible_top + Σ(前面各行的 git 块高 + 行高)
/// ```
///
/// **`bounds.origin.y` 已经含滚动偏移**（`layout_cursor` 里
/// `bounds.origin += scroll_offset`），所以这里不能再加/减滚动量。
/// 之前浮层位置错乱正是因为又减了一遍 `scroll_offset.y`。
///
/// 抽成纯函数便于单测：这是「浮层落在哪一行」的唯一决定因素。
fn row_top_in_viewport(
    last_layout: &LastLayout,
    row_ix: usize,
    line_height: Pixels,
    bounds_top: Pixels,
) -> (Pixels, Pixels) {
    let mut row_top = bounds_top + last_layout.visible_top;
    for (i, prev) in last_layout.lines.iter().enumerate().take(row_ix) {
        // git 展开区的块高也要计入（用户展开过 hunk 时行间会多出这些块）。
        row_top += line_height * last_layout.git_insert_delta(i) as f32;
        row_top += prev.size(line_height).height;
    }
    let row_height = last_layout
        .lines
        .get(row_ix)
        .map(|l| l.size(line_height).height)
        .unwrap_or(line_height);
    (row_top, row_height)
}

/// 锚点字节偏移 → 可见行下标（`visible_line_byte_offsets` 与 `lines` 平行）。
///
/// 抽成自由函数便于单测：hover 与补全菜单都用它定位。
/// 锚点字节偏移 → 可见行下标；不在可见区内返回 `None`。
///
/// `offsets` 是各可见行的起始字节（与 `lines` 平行），`visible_end` 是可见区
/// 结束字节（最后一条可见行的下一行行首，钳到文本长度）。
///
/// 之前用 `usize::MAX`（或全文长度）当最后一条可见行的结束，于是任何
/// **视口之外**的锚点都会匹配到最后一条可见行：滚动到别处后浮层仍粘在
/// 底部行上，而调用方以为「不在可见区就不画」。
fn visible_row_for_offset(offsets: &[usize], offset: usize, visible_end: usize) -> Option<usize> {
    if offsets.is_empty() || offset >= visible_end {
        return None;
    }
    // 锚点在首个可见行之前：不可见。
    if offset < offsets[0] {
        return None;
    }
    offsets.iter().enumerate().find_map(|(i, start)| {
        let end = offsets.get(i + 1).copied().unwrap_or(visible_end);
        (offset >= *start && offset < end).then_some(i)
    })
}

/// Get the runs for the given range.
///
/// The range is the byte range of the wrapped line.
pub(super) fn runs_for_range(
    runs: &[TextRun],
    line_offset: usize,
    range: &Range<usize>,
) -> Vec<TextRun> {
    let mut result = vec![];
    let range = (line_offset + range.start)..(line_offset + range.end);
    let mut cursor = 0;

    for run in runs {
        let run_start = cursor;
        let run_end = cursor + run.len;

        if run_end <= range.start {
            cursor = run_end;
            continue;
        }

        if run_start >= range.end {
            break;
        }

        let start = range.start.max(run_start) - run_start;
        let end = range.end.min(run_end) - run_start;
        let len = end - start;

        if len > 0 {
            result.push(TextRun { len, ..run.clone() });
        }

        cursor = run_end;
    }

    result
}

/// 一行缩进前缀的参考线 x 坐标（相对正文左缘）：第 k 项 = 第 k 层
/// 参考线的 x，即列 `k * indent_size` 处的实测位置。
///
/// 与 Zed 一致：参考线画在缩进单元的**起始列**（0、indent_size、
/// 2*indent_size…，Zed 里是 `single_indent_width * depth`，depth 从 0 起），
/// 不再右移一格。空格缩进时恒有 `xs[k] == space_width * (k * indent_size)`，
/// 这是空行插值回退公式的对齐基准。
///
/// 层级起点列一定落在字符边界上（空格逐列推进、Tab 落在 `indent_size`
/// 的倍数上），所以每个起点都能拿到该列的精确 x；返回值可能比该行的
/// 层数（`ceil(前缀列数 / indent_size)`）多一项——多出的是前缀终点，
/// 绘制时按层数取用，不会读到。
pub(super) fn measure_guide_xs(
    line: &str,
    indent_size: usize,
    space_width: Pixels,
    tab_width: Pixels,
) -> Vec<Pixels> {
    let mut out: Vec<Pixels> = Vec::new();
    let mut cols = 0usize;
    let mut width = px(0.);
    for ch in line.chars() {
        match ch {
            ' ' => {
                cols += 1;
                width += space_width;
            }
            '\t' => {
                cols = cols - cols % indent_size + indent_size;
                width += tab_width;
            }
            _ => break,
        }
        // 第 0 层：列 0，x 恒为 0（行首一旦有缩进前缀就存在）。
        if out.is_empty() && cols > 0 {
            out.push(px(0.));
        }
        // 本轮字符刚好落在 `out.len() * indent_size` 列上：补上该层级起点。
        // Tab 跳到的是上一个 `indent_size` 倍数，不会越过任何起点列。
        if out.len() * indent_size == cols {
            out.push(width);
        }
    }
    out
}

/// 最近一帧注册的光标请求，按 paint 顺序排列，供测试断言「指针落在哪个区域
/// 会显示什么光标」。
///
/// 顺序本身就是语义：gpui 的 `NextFrame::cursor_style` 反向扫描请求列表，
/// 返回**最后一条**命中的样式；子元素在父元素之后绘制，所以子元素的请求
/// 覆盖父容器。测试据此验证「行号栏/滚动条压掉容器与文本区的 IBeam」。
///
/// 存在 thread-local 里而不是进程级静态量：`#[gpui::test]` 用例并行跑在各自
/// 线程上，进程级槽位会被别的用例的帧覆盖（`LIVE_LINE_PAINT_LOG` 就带着这个
/// 坑，其断言因此只能写成单向存在性检查）。绘制与断言在同一线程，thread-local
/// 天然按用例隔离。
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct CursorRequest {
    pub(crate) style: CursorStyle,
    pub(crate) bounds: Bounds<Pixels>,
}

#[cfg(test)]
thread_local! {
    pub(crate) static CURSOR_REQUESTS: std::cell::RefCell<Vec<CursorRequest>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn record_cursor_request(style: CursorStyle, bounds: Bounds<Pixels>) {
    CURSOR_REQUESTS.with(|slot| slot.borrow_mut().push(CursorRequest { style, bounds }));
}

/// 读取本线程最近一帧记录的光标请求（paint 顺序），并清空槽位。
#[cfg(test)]
pub(crate) fn take_cursor_requests() -> Vec<CursorRequest> {
    CURSOR_REQUESTS.with(|slot| std::mem::take(&mut *slot.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn measure_guide_xs_pure_spaces_matches_fallback_formula() {
        let s = px(8.);
        let t = px(32.);
        // 4 空格：第 1 层在列 0（x=0）；第 2 项是前缀终点，层数为 1 时读不到
        assert_eq!(measure_guide_xs("    x", 4, s, t), vec![px(0.), s * 4.]);
        // 8 空格：第 1 层在列 0、第 2 层在列 4（前缀终点列 8 只作余量）
        assert_eq!(
            measure_guide_xs("        x", 4, s, t),
            vec![px(0.), s * 4., s * 8.]
        );
        // 5 空格（不成整级的前缀）：层数 ceil(5/4)=2，读到列 0 与列 4
        assert_eq!(measure_guide_xs("     x", 4, s, t), vec![px(0.), s * 4.]);
        // 不足一级缩进（3 空格）：层数 1，只有列 0
        assert_eq!(measure_guide_xs("   x", 4, s, t), vec![px(0.)]);
        // 顶格行：无参考线
        assert!(measure_guide_xs("x", 4, s, t).is_empty());
    }

    #[test]
    fn measure_guide_xs_tabs_use_tab_advance() {
        let s = px(8.);
        let t = px(32.);
        // 两个 Tab：列 0 → 0，列 4 → 第一个 Tab 的推进量，列 8 → 两个 Tab
        assert_eq!(measure_guide_xs("\t\tx", 4, s, t), vec![px(0.), t, t * 2.]);
        // Tab + 2 空格（6 列）：层数 2，列 4 落在 Tab 终点
        assert_eq!(measure_guide_xs("\t  x", 4, s, t), vec![px(0.), t]);
        // Tab + 4 空格（8 列）：层数 2，列 4 = Tab 终点；列 8 为余量
        assert_eq!(
            measure_guide_xs("\t    x", 4, s, t),
            vec![px(0.), t, t + s * 4.]
        );
        // 单 Tab：层数 1，读到列 0
        assert_eq!(measure_guide_xs("\tx", 4, s, t), vec![px(0.), t]);
    }

    // ── 编辑器光标动画（端到端） ──────────────────────────────────────────

    /// 块状光标的宽度必须等于**一个字符格**。
    ///
    /// 回归背景：宽度曾用 `line_height * 0.6` 估算，在常见的 1.6 倍行高下
    /// 约为单元格的 1.6 倍，Normal 模式下光标会盖住下一个字符。
    /// 这里直接量 `cursor_block_layout_static` 的产出，与实际渲染同一函数。
    #[gpui::test]
    fn block_cursor_is_one_cell_wide(cx: &mut gpui::TestAppContext) {
        use crate::components::input::CursorShape;

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| {
            struct Host;
            impl gpui::Render for Host {
                fn render(
                    &mut self,
                    _: &mut gpui::Window,
                    _: &mut gpui::Context<Self>,
                ) -> impl gpui::IntoElement {
                    gpui::div()
                }
            }
            Host
        });

        let (line_height, font, font_size, cell, block_width) = cx.update(|window, cx| {
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let line_height = window.line_height();
            let font = style.font();
            let font_id = window.text_system().resolve_font(&font);
            let cell = window
                .text_system()
                .ch_advance(font_id, font_size)
                .expect("等宽字体应能取到字符前进宽度");

            // 直接量渲染用的宽度计算。
            let (block_width, _) = TextElement::cursor_block_layout_static(
                CursorShape::Block,
                Some('a'),
                Some(font_size),
                line_height,
                window,
                cx,
            );
            (line_height, font, font_size, cell, block_width)
        });

        let _ = (font, font_size);
        assert!(cell > px(0.), "字符前进宽度应为正数：{cell:?}");

        // 关键断言：块状光标宽度就是一格。
        assert_eq!(
            block_width,
            cell,
            "块状光标宽度应等于一个字符格（旧实现是 line_height*0.6 = {:?}）",
            line_height * 0.6
        );

        // 并明确它不等于旧的估算值（防止回退）。
        let old_estimate = (line_height * 0.6).max(CURSOR_WIDTH);
        assert!(
            old_estimate > cell,
            "本测试的前提是旧估算值确实大于一格（估算 {old_estimate:?} vs 实际 {cell:?}）"
        );
    }

    /// 宽字符（CJK）的块状光标应覆盖两格。
    #[gpui::test]
    fn block_cursor_covers_wide_characters(cx: &mut gpui::TestAppContext) {
        use crate::components::input::CursorShape;

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| {
            struct Host;
            impl gpui::Render for Host {
                fn render(
                    &mut self,
                    _: &mut gpui::Window,
                    _: &mut gpui::Context<Self>,
                ) -> impl gpui::IntoElement {
                    gpui::div()
                }
            }
            Host
        });

        let (ascii_width, cjk_width, cell) = cx.update(|window, cx| {
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let line_height = window.line_height();
            let font = style.font();
            let font_id = window.text_system().resolve_font(&font);
            let cell = window
                .text_system()
                .ch_advance(font_id, font_size)
                .expect("等宽字体应能取到字符前进宽度");

            let (ascii, _) = TextElement::cursor_block_layout_static(
                CursorShape::Block,
                Some('a'),
                Some(font_size),
                line_height,
                window,
                cx,
            );
            let (cjk, _) = TextElement::cursor_block_layout_static(
                CursorShape::Block,
                Some('中'),
                Some(font_size),
                line_height,
                window,
                cx,
            );
            (ascii, cjk, cell)
        });

        assert_eq!(ascii_width, cell, "ASCII 字符占一格");
        assert!(
            cjk_width > cell,
            "CJK 字符的块状光标应宽于一格（{cjk_width:?} vs {cell:?}）"
        );
    }

    /// 行尾 / 空行（没有可覆盖字符）时，块状光标也应按一格宽画。
    ///
    /// 这条路径此前用的是行高估算值，比一格宽约 1.6 倍，视觉上会盖住
    /// 本不存在的「下一个字符」位置。
    #[gpui::test]
    fn block_cursor_on_empty_line_is_one_cell_wide(cx: &mut gpui::TestAppContext) {
        use crate::components::input::CursorShape;

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| {
            struct Host;
            impl gpui::Render for Host {
                fn render(
                    &mut self,
                    _: &mut gpui::Window,
                    _: &mut gpui::Context<Self>,
                ) -> impl gpui::IntoElement {
                    gpui::div()
                }
            }
            Host
        });

        let (empty_width, newline_width, cell) = cx.update(|window, cx| {
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let line_height = window.line_height();
            let font_id = window.text_system().resolve_font(&style.font());
            let cell = window
                .text_system()
                .ch_advance(font_id, font_size)
                .expect("等宽字体应能取到字符前进宽度");

            let (empty, _) = TextElement::cursor_block_layout_static(
                CursorShape::Block,
                None,
                Some(font_size),
                line_height,
                window,
                cx,
            );
            let (newline, _) = TextElement::cursor_block_layout_static(
                CursorShape::Block,
                Some('\n'),
                Some(font_size),
                line_height,
                window,
                cx,
            );
            (empty, newline, cell)
        });

        assert_eq!(empty_width, cell, "空行的块状光标应为一格宽");
        assert_eq!(newline_width, cell, "换行符处的块状光标应为一格宽");
    }

    /// 通过真实的 `TextElement` prepaint/paint 驱动，验证：
    /// 1. 首帧光标直接就位（不从中途飞入）；
    /// 2. 光标移动产生动画，四边形落在起止之间；
    /// 3. 动画收敛后精确落在新位置。
    #[gpui::test]
    fn cursor_animation_through_text_element(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::{AppContext as _, VisualTestContext};

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx)
                    .multi_line(true)
                    .code_editor("text".to_string());
                s.set_text("hello world", cx);
                s
            });
            Host { state }
        });

        let state = host.read_with(cx, |h, _| h.state.clone());
        let bounds = size(px(600.0), px(200.0));

        let draw = |cx: &mut VisualTestContext, step: Duration| {
            cx.executor().advance_clock(step);
            cx.draw(point(px(0.0), px(0.0)), bounds, |_, _| {
                // 用固定尺寸容器包一层：生产里 TextElement 由 `relative(1.)`
                // 撑满父容器，而 `cx.draw` 直接布局元素时会按内容 auto 尺寸
                // 来回解析，导致 bounds 在帧间抖动。
                TextElement::new(state.clone())
            })
        };

        // 首帧就位。
        draw(cx, Duration::from_millis(16));
        assert!(
            !state.read_with(cx, |s, _| s.cursor_animator.is_animating()),
            "首帧不应处于动画中"
        );

        // 光标右移（End 键）：应起动画。
        state.update_in(cx, |state, window, cx| {
            state.move_to_end(&crate::components::input::MoveToEnd, window, cx);
        });
        draw(cx, Duration::from_millis(16));
        assert!(
            state.read_with(cx, |s, _| s.cursor_animator.is_animating()),
            "光标移动应产生动画"
        );

        // 推进到收敛。
        for _ in 0..200 {
            if !state.read_with(cx, |s, _| s.cursor_animator.is_animating()) {
                break;
            }
            draw(cx, Duration::from_millis(16));
        }
        assert!(
            !state.read_with(cx, |s, _| s.cursor_animator.is_animating()),
            "动画应收敛"
        );
    }

    /// 滚动不应产生光标动画（滚动是内容平移，不是光标移动）。
    #[gpui::test]
    fn scroll_does_not_animate_cursor(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::{AppContext as _, VisualTestContext};

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx)
                    .multi_line(true)
                    .code_editor("text".to_string());
                // 内容要足够长，否则滚动会被钳制到 0（测不到滚动通道）。
                let long: String = (0..60).map(|i| format!("line {i}\n")).collect();
                s.set_text(&long, cx);
                s
            });
            Host { state }
        });

        let state = host.read_with(cx, |h, _| h.state.clone());
        let bounds = size(px(600.0), px(200.0));

        let draw = |cx: &mut VisualTestContext, step: Duration| {
            cx.executor().advance_clock(step);
            cx.draw(point(px(0.0), px(0.0)), bounds, |_, _| {
                // 用固定尺寸容器包一层：生产里 TextElement 由 `relative(1.)`
                // 撑满父容器，而 `cx.draw` 直接布局元素时会按内容 auto 尺寸
                // 来回解析，导致 bounds 在帧间抖动。
                TextElement::new(state.clone())
            })
        };

        // 就位并收敛。
        draw(cx, Duration::from_millis(16));
        for _ in 0..50 {
            if !state.read_with(cx, |s, _| s.cursor_animator.is_animating()) {
                break;
            }
            draw(cx, Duration::from_millis(16));
        }

        // 记录滚动前光标的**绘制位置**（内容坐标 + 滚动）。
        let quad_before = state.read_with(cx, |s, _| s.cursor_animator.last_quad());

        // 只滚动，不移动光标。
        state.update_in(cx, |state, _window, cx| {
            state.scroll_handle.set_offset(point(px(0.0), px(-40.0)));
            cx.notify();
        });
        draw(cx, Duration::from_millis(16));
        assert!(
            !state.read_with(cx, |s, _| s.cursor_animator.is_animating()),
            "滚动不应产生光标动画（光标应钉在内容上）"
        );

        // 滚动量必须真的被动画器看到：四边形应整体上移 40px。
        let quad_after = state.read_with(cx, |s, _| s.cursor_animator.last_quad());
        match (quad_before, quad_after) {
            (Some(before), Some(after)) => {
                let dy = after.corners[0].1 - before.corners[0].1;
                assert!(
                    (dy - (-40.0)).abs() < 1.0,
                    "滚动应把光标整体平移 -40px，实际 {dy}（before={:?} after={:?}）",
                    before.corners[0],
                    after.corners[0]
                );
            }
            other => panic!("两侧都应有四边形: {other:?}"),
        }
    }

    // ── 水平滚动（长行不软换行，zed / vscode 语义） ────────────────────────

    /// 点行号区 = Zed 的「模拟三击」= 选中整行；横向滚动下也必须如此 ——
    /// gutter 是固定绘制的，若按内容坐标反推列，光标会落到藏在 gutter 底下的
    /// 那一列上。
    #[gpui::test]
    fn gutter_click_selects_line_while_horizontally_scrolled(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::{AppContext as _, Modifiers, MouseButton, MouseDownEvent, VisualTestContext};

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        // 首行足够长 → 内容横向溢出，才能真的滚起来（soft_wrap 默认已关）。
        let long = "x".repeat(400);
        let text = format!("{long}\nsecond line\nthird line\n");
        let (host, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx)
                    .multi_line(true)
                    .code_editor("text".to_string());
                s.set_text(&text, cx);
                s
            });
            Host { state }
        });
        let state = host.read_with(cx, |h, _| h.state.clone());
        let viewport = size(px(600.0), px(200.0));
        let draw = |cx: &mut VisualTestContext, step: Duration| {
            cx.executor().advance_clock(step);
            cx.draw(point(px(0.0), px(0.0)), viewport, |_, _| {
                // 固定尺寸容器：见上文 `cursor_animation_through_text_element`。
                TextElement::new(state.clone())
            })
        };

        draw(cx, Duration::from_millis(16));
        // 向右滚 300px：gutter 不动，只有正文平移。
        state.update_in(cx, |state, _window, cx| {
            state.update_scroll_offset(Some(point(px(-300.0), px(0.0))), cx);
        });
        draw(cx, Duration::from_millis(16));
        let offset_x = state.read_with(cx, |s, _| s.scroll_handle.offset().x);
        assert!(
            offset_x <= px(-1.),
            "长行内容应能横向滚动，实际 offset.x = {offset_x}"
        );

        // 点第 2 行的行号（gutter 内、正文左缘左侧）。
        let click_at = state.read_with(cx, |s, _| {
            let input = s.input_bounds;
            let line_height = s
                .last_layout
                .as_ref()
                .map(|l| l.line_height)
                .unwrap_or(px(20.));
            assert!(
                point(input.origin.x + px(4.), input.origin.y).x < s.content_left_x(),
                "点击点必须落在 gutter 内"
            );
            point(
                input.origin.x + px(4.),
                input.origin.y + line_height + px(4.),
            )
        });
        state.update_in(cx, |state, window, cx| {
            state.on_mouse_down(
                &MouseDownEvent {
                    position: click_at,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Modifiers::default(),
                    first_mouse: false,
                },
                window,
                cx,
            );
            let second_line_start = state.text.line_start_offset(1);
            let expected = state
                .text
                .line_range(second_line_start)
                .expect("第 2 行存在");
            assert_eq!(
                state.selected_range,
                expected.into(),
                "点行号应选中整行（Zed: click_count = 3），\
                 而不是把光标放到 gutter 底下的某一列"
            );
        });
    }

    /// 内容横向溢出时底部出现横向滚动条（Zed `scrollbar.axes.horizontal`），
    /// 不溢出时一条都不画。
    #[gpui::test]
    fn horizontal_scrollbar_follows_content_overflow(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::AppContext as _;

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        // 搭一个真实渲染过的编辑器（input_bounds / scroll_size 由 prepaint+paint
        // 产出），滚动条几何只在 paint 里写回 state。
        let build = |cx: &mut gpui::TestAppContext, text: String| {
            let (host, cx) = cx.add_window_view(|window, cx| {
                let state = cx.new(|cx| {
                    let mut s = InputState::new(window, cx)
                        .multi_line(true)
                        .code_editor("text".to_string());
                    s.set_text(&text, cx);
                    s
                });
                Host { state }
            });
            let state = host.read_with(cx, |h, _| h.state.clone());
            let viewport = size(px(600.0), px(200.0));
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.draw(point(px(0.0), px(0.0)), viewport, |_, _| {
                TextElement::new(state.clone())
            });
            state
        };

        // 不溢出：几何应为 None。
        let short_state = build(
            cx,
            "fn main() {}\nfn other() {}\nsecond line is not long\n".to_string(),
        );
        let short_geom = short_state.read_with(cx, |s, _| s.scrollbar_geometry_h);
        assert!(
            short_geom.is_none(),
            "内容不横向溢出时不应有横向滚动条，实际 {short_geom:?}"
        );

        // 长行溢出：几何存在，thumb 落在视口内、右端给纵向条让位。
        let long = "x".repeat(400);
        let state = build(cx, format!("{long}\nshort\n"));
        let geom = state
            .read_with(cx, |s, _| s.scrollbar_geometry_h)
            .expect("内容横向溢出时应有横向滚动条");
        let input = state.read_with(cx, |s, _| s.input_bounds);
        assert!(
            geom.thumb.left() >= input.left() && geom.thumb.right() <= input.right(),
            "横向 thumb 应在视口内：{:?} vs {:?}",
            geom.thumb,
            input
        );
        assert!(
            geom.thumb.right() <= input.right() - BAR_THICKNESS,
            "横向条右端应给纵向条让位：thumb={:?} input_right={}",
            geom.thumb,
            input.right()
        );
        assert!(
            geom.thumb.bottom() <= input.bottom(),
            "横向 thumb 不应越过视口底边"
        );
        assert!(
            geom.thumb.size.width < geom.viewport_len,
            "长行应让 thumb 短于轨道（可见部分远小于内容）"
        );
    }

    /// 编辑器文本区必须报 IBeam（Zed `paint_text` / VSCode
    /// `viewLines.css` 的 `cursor:text`），行号栏报箭头。回归用例：修复前
    /// `TextElement` 从不注册任何光标样式，指针在编辑器上始终是箭头。
    #[gpui::test]
    fn text_area_requests_ibeam_cursor(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::AppContext as _;

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx)
                    .multi_line(true)
                    .code_editor("text".to_string());
                s.set_text("fn main() {\n    println!(\"hi\");\n}\n", cx);
                s
            });
            Host { state }
        });
        let state = host.read_with(cx, |h, _| h.state.clone());

        let viewport = size(px(600.0), px(200.0));
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.draw(point(px(0.0), px(0.0)), viewport, |_, _| {
            TextElement::new(state.clone())
        });

        let requests = take_cursor_requests();
        assert!(
            !requests.is_empty(),
            "一帧绘制后应注册光标样式请求（回归：修复前 TextElement 一条都不发）"
        );

        let find = |style: CursorStyle| {
            requests
                .iter()
                .find(|r| r.style == style)
                .unwrap_or_else(|| panic!("缺少 {style:?} 请求，实际 {requests:?}"))
        };
        let text = find(CursorStyle::IBeam);
        let gutter = find(CursorStyle::Arrow);

        // 文本区在行号栏右侧，行号栏在左：两块区域不重叠。
        assert!(
            gutter.bounds.right() <= text.bounds.left(),
            "行号栏与文本区不应重叠：gutter={:?} text={:?}",
            gutter.bounds,
            text.bounds
        );
        assert!(
            gutter.bounds.size.width > px(0.),
            "代码编辑器应留出行号栏，实际 {:?}",
            gutter.bounds
        );
        assert!(
            text.bounds.size.width > px(0.),
            "文本命中区应有宽度，实际 {:?}",
            text.bounds
        );
        // 文本区必须真的落在元素内（回归：若按滚动过的 bounds 建 hitbox，
        // 滚动后命中区会整体平移出可视范围）。
        let input = state.read_with(cx, |s, _| s.input_bounds);
        assert!(
            text.bounds.left() >= input.left() && text.bounds.right() <= input.right(),
            "文本命中区应在编辑器内：text={:?} input={:?}",
            text.bounds,
            input
        );
        assert!(
            text.bounds.top() >= input.top() && text.bounds.bottom() <= input.bottom(),
            "文本命中区纵向应在编辑器内：text={:?} input={:?}",
            text.bounds,
            input
        );

        // 指针落在文本区中心 → 只命中文本命中区。
        let probe = text.bounds.center();
        assert!(
            text.bounds.contains(&probe) && !gutter.bounds.contains(&probe),
            "文本区中心的指针应只命中文本命中区"
        );

        // 指针落在行号栏 → 只命中行号栏命中区。
        let gutter_probe = gutter.bounds.center();
        assert!(
            gutter.bounds.contains(&gutter_probe) && !text.bounds.contains(&gutter_probe),
            "行号栏中心的指针应只命中行号栏命中区"
        );
    }

    /// 滚动条条带命中区必须与「真的画出来的条带」一一对应：不溢出就没有
    /// 条带（文本右缘/底缘仍是 IBeam），溢出时两者同时出现。
    #[gpui::test]
    fn scrollbar_strips_match_painted_bars(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::AppContext as _;

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let build = |cx: &mut gpui::TestAppContext, text: String| {
            let (host, cx) = cx.add_window_view(|window, cx| {
                let state = cx.new(|cx| {
                    let mut s = InputState::new(window, cx)
                        .multi_line(true)
                        .code_editor("text".to_string());
                    s.set_text(&text, cx);
                    s
                });
                Host { state }
            });
            let state = host.read_with(cx, |h, _| h.state.clone());
            let viewport = size(px(600.0), px(200.0));
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.draw(point(px(0.0), px(0.0)), viewport, |_, _| {
                TextElement::new(state.clone())
            });
            state
        };

        let check = |state: &gpui::Entity<InputState>, cx: &gpui::TestAppContext, label: &str| {
            let requests = take_cursor_requests();
            let (painted_v, painted_h) = state.read_with(cx, |s, _| {
                (
                    s.scrollbar_geometry.is_some(),
                    s.scrollbar_geometry_h.is_some(),
                )
            });
            // 条带请求排在文本请求之后，所以指针停在条带上时箭头胜出。
            // 按精确厚度区分：纵向条宽 = BAR_THICKNESS、高 = 视口高；
            // 横向条反过来。行号栏也是 Arrow，但宽度远大于 BAR_THICKNESS。
            let bar_requests: Vec<_> = requests
                .iter()
                .filter(|r| r.style == CursorStyle::Arrow)
                .collect();
            let has_v = bar_requests.iter().any(|r| {
                r.bounds.size.width <= BAR_THICKNESS + px(0.5)
                    && r.bounds.size.height > BAR_THICKNESS
            });
            let has_h = bar_requests.iter().any(|r| {
                r.bounds.size.height <= BAR_THICKNESS + px(0.5)
                    && r.bounds.size.width > BAR_THICKNESS
            });
            assert_eq!(
                has_v, painted_v,
                "{label}: 纵向条带光标请求应与画出的条带一致（requests={requests:?}）"
            );
            assert_eq!(
                has_h, painted_h,
                "{label}: 横向条带光标请求应与画出的条带一致（requests={requests:?}）"
            );
            if painted_h {
                let last = requests.last().expect("应有光标请求");
                assert_eq!(
                    last.style,
                    CursorStyle::Arrow,
                    "{label}: 条带请求必须排在文本 IBeam 之后（gpui 取最后一条命中项）"
                );
                assert!(
                    last.bounds.size.height <= BAR_THICKNESS + px(0.5),
                    "{label}: 最后一条应为横向条带请求，实际 {:?}",
                    last.bounds
                );
            }
        };

        // 内容不横向溢出：横向条带（及其命中区）都不该出现。
        let state = build(cx, "short\nlines\n".to_string());
        let h_painted = state.read_with(cx, |s, _| s.scrollbar_geometry_h.is_some());
        assert!(!h_painted, "内容不横向溢出时不应画出横向滚动条");
        check(&state, cx, "short");

        // 长行 + 行数超出视口：两条都出现。
        let long = "x".repeat(400);
        let lines: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let state = build(cx, format!("{long}\n{lines}"));
        let h_painted = state.read_with(cx, |s, _| s.scrollbar_geometry_h.is_some());
        assert!(h_painted, "长行溢出时应画出横向滚动条");
        check(&state, cx, "overflowing");
    }

    /// 走完整 `EditorInput` 层级解析「指针落在某处会显示什么光标」。
    ///
    /// `EditorInput` 根 div 统一设 IBeam（覆盖内边距与末行以下的空白），
    /// `TextElement` 再用更靠后的 hitbox 把行号栏与滚动条条带改成箭头——
    /// gpui 取的是**最后一条**命中项（`NextFrame::cursor_style` 反向扫描），
    /// 子元素在父元素之后绘制，所以子元素的请求生效。
    ///
    /// 这里按 gpui 的规则在本地重放「命中 → 取最后一条」，锁住两个语义：
    /// 1. 文本区中心解析为 IBeam（回归：修复前整块编辑器都是箭头）；
    /// 2. 行号栏中心解析为 Arrow，即箭头请求确实排在文本请求之后。
    #[gpui::test]
    fn cursor_style_resolves_per_region(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::AppContext as _;

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let long = "x".repeat(400);
        let lines: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let (host, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx)
                    .multi_line(true)
                    .code_editor("text".to_string());
                s.set_text(format!("{long}\n{lines}"), cx);
                s
            });
            Host { state }
        });

        let viewport = size(px(600.0), px(200.0));
        cx.executor().advance_clock(Duration::from_millis(16));
        // 渲染完整 `EditorInput`（含根 div 的 IBeam 与文本区的 hitbox）：
        // 走 Host 视图，`RenderOnce` 需要视图上下文。
        cx.draw(point(px(0.0), px(0.0)), viewport, |_, _| {
            host.clone().into_any_element()
        });

        let requests = take_cursor_requests();
        assert!(
            requests.len() >= 3,
            "完整 EditorInput 应至少注册 文本/行号栏/纵向条 三条请求，实际 {requests:?}"
        );
        assert!(
            requests.iter().any(|r| r.style == CursorStyle::IBeam),
            "编辑器应注册 IBeam 请求，实际 {requests:?}"
        );

        // 复刻 gpui `NextFrame::cursor_style`：反向扫描，第一条命中的即为结果。
        let resolve = |position: Point<Pixels>| {
            requests
                .iter()
                .rev()
                .find(|r| r.bounds.contains(&position))
                .map(|r| r.style)
        };

        let text = requests
            .iter()
            .find(|r| r.style == CursorStyle::IBeam)
            .expect("应有 IBeam 请求");
        assert_eq!(
            resolve(text.bounds.center()),
            Some(CursorStyle::IBeam),
            "指针在文本区中心时应显示 IBeam，实际请求 {requests:?}"
        );

        let gutter = requests
            .iter()
            .find(|r| r.style == CursorStyle::Arrow && r.bounds.right() <= text.bounds.left())
            .expect("应有行号栏的 Arrow 请求（位于文本区左侧）");
        assert_eq!(
            resolve(gutter.bounds.center()),
            Some(CursorStyle::Arrow),
            "指针在行号栏中心时应显示箭头，实际请求 {requests:?}"
        );

        // 滚动条条带：悬浮在文本右缘/底缘之上，箭头请求排在文本之后才能压住。
        let bars: Vec<_> = requests
            .iter()
            .filter(|r| r.style == CursorStyle::Arrow)
            .filter(|r| {
                r.bounds.size.height > BAR_THICKNESS
                    && r.bounds.size.width <= BAR_THICKNESS + px(0.5)
            })
            .collect();
        let v_bar = bars.first().expect("内容溢出时应有纵向条带的箭头请求");
        assert!(
            text.bounds.contains(&v_bar.bounds.center()),
            "纵向条带悬浮在文本命中区之上（有意重叠）：bar={:?} text={:?}",
            v_bar.bounds,
            text.bounds
        );
        assert_eq!(
            resolve(v_bar.bounds.center()),
            Some(CursorStyle::Arrow),
            "指针在纵向条带上时应显示箭头（条带请求必须排在文本 IBeam 之后）"
        );
    }

    /// 只读（`disabled`）编辑器仍给文本区 IBeam：diff 视图左栏、只读预览
    /// 都能划选复制，箭头会误导用户以为不能选。
    ///
    /// `disabled` 只关掉闪烁光标与文本写入（`should_paint_cursor` /
    /// `replace_text_in_range`），`on_mouse_down` 并不拦——选中逻辑照常。
    #[gpui::test]
    fn disabled_editor_still_requests_ibeam(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::AppContext as _;

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state).disabled(true)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx)
                    .multi_line(true)
                    .code_editor("text".to_string());
                s.set_text("fn main() {}\n", cx);
                s
            });
            Host { state }
        });

        let viewport = size(px(600.0), px(200.0));
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.draw(point(px(0.0), px(0.0)), viewport, |_, _| {
            host.clone().into_any_element()
        });

        let requests = take_cursor_requests();
        assert!(
            requests.iter().any(|r| r.style == CursorStyle::IBeam),
            "只读编辑器也应给文本区 IBeam（可划选复制），实际 {requests:?}"
        );
    }

    /// 单行输入框（搜索框、表单字段）没有行号栏：整块都是文本区，
    /// 行号栏命中区退化为零宽，不会误吞 IBeam。
    #[gpui::test]
    fn single_line_input_has_no_gutter_region(cx: &mut gpui::TestAppContext) {
        use crate::components::input::InputState;
        use gpui::AppContext as _;

        struct Host {
            state: gpui::Entity<InputState>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                crate::components::input::EditorInput::new(&self.state)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| {
                let mut s = InputState::new(window, cx);
                s.set_text("search query", cx);
                s
            });
            Host { state }
        });

        let viewport = size(px(300.0), px(28.0));
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.draw(point(px(0.0), px(0.0)), viewport, |_, _| {
            host.clone().into_any_element()
        });

        let requests = take_cursor_requests();
        let text = requests
            .iter()
            .find(|r| r.style == CursorStyle::IBeam)
            .unwrap_or_else(|| panic!("单行输入框应给 IBeam，实际 {requests:?}"));
        assert!(
            text.bounds.size.width > px(0.),
            "单行输入框的文本命中区应有宽度：{:?}",
            text.bounds
        );
        // 行号栏是零宽的（`layout_line_numbers` 对单行输入返回 0），
        // 所以它永远不命中——不会把文本区的 IBeam 顶掉。
        for arrow in requests.iter().filter(|r| r.style == CursorStyle::Arrow) {
            assert!(
                !arrow.bounds.contains(&text.bounds.center()),
                "零宽行号栏不应命中文本区中心：arrow={:?}",
                arrow.bounds
            );
        }
    }
}

#[cfg(test)]
mod tests_runs_for_range {
    use super::*;

    #[test]
    fn test_runs_for_range() {
        let run = TextRun {
            len: 0,
            font: gpui::font(".SystemUIFont"),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // use hello this-is-test
        let runs = vec![
            // use
            TextRun {
                len: 3,
                ..run.clone()
            },
            // \s
            TextRun {
                len: 1,
                ..run.clone()
            },
            // hello
            TextRun {
                len: 5,
                ..run.clone()
            },
            // \s
            TextRun {
                len: 1,
                ..run.clone()
            },
            // this-is-test
            TextRun {
                len: 12,
                ..run.clone()
            },
        ];

        #[track_caller]
        fn assert_runs(actual: Vec<TextRun>, expected: &[usize]) {
            let left = actual.iter().map(|run| run.len).collect::<Vec<_>>();
            assert_eq!(left, expected);
        }

        assert_runs(runs_for_range(&runs, 0, &(0..0)), &[]);
        assert_runs(runs_for_range(&runs, 0, &(0..100)), &[3, 1, 5, 1, 12]);

        assert_runs(runs_for_range(&runs, 0, &(0..6)), &[3, 1, 2]);
        assert_runs(runs_for_range(&runs, 0, &(1..6)), &[2, 1, 2]);
        assert_runs(runs_for_range(&runs, 0, &(3..10)), &[1, 5, 1]);
        assert_runs(runs_for_range(&runs, 0, &(5..8)), &[3]);
        assert_runs(runs_for_range(&runs, 3, &(0..3)), &[1, 2]);
        assert_runs(runs_for_range(&runs, 3, &(2..10)), &[4, 1, 3]);
        assert_runs(runs_for_range(&runs, 9, &(0..8)), &[1, 7]);
    }

    #[test]
    fn combine_highlights_stable_rainbow_overrides_syntax() {
        // Nested "((  ))" - overlay (rainbow) must win and paired brackets share the same color.
        // 注意：`HighlightStyle` 为 `Copy`，直接按值使用，无需 `.clone()`。
        let gray = HighlightStyle {
            color: Some(gpui::rgb(0xb2b9c6).into()),
            ..Default::default()
        };
        let gold = HighlightStyle {
            color: Some(gpui::rgb(0xE2B93D).into()),
            ..Default::default()
        };
        let blue = HighlightStyle {
            color: Some(gpui::rgb(0x4FC1FF).into()),
            ..Default::default()
        };
        let syntax = vec![(0..1, gray), (1..2, gray), (2..3, gray), (3..4, gray)];
        let rainbow = vec![(0..1, gold), (1..2, blue), (2..3, blue), (3..4, gold)];
        let merged = combine_highlights_stable(syntax, rainbow);
        assert_eq!(
            merged,
            vec![(0..1, gold), (1..2, blue), (2..3, blue), (3..4, gold)]
        );
        assert_eq!(merged[0].1.color, merged[3].1.color);
        assert_eq!(merged[1].1.color, merged[2].1.color);
    }

    #[test]
    fn combine_highlights_stable_deterministic_for_nested() {
        let base = HighlightStyle {
            color: Some(gpui::red()),
            ..Default::default()
        };
        let overlay = HighlightStyle {
            color: Some(gpui::blue()),
            ..Default::default()
        };
        let a = vec![(0..5, base), (4..10, base)];
        let b = vec![(2..6, overlay)];
        let r1 = combine_highlights_stable(a.clone(), b.clone());
        let r2 = combine_highlights_stable(a, b);
        assert_eq!(r1, r2);
        let seg = r1.iter().find(|(r, _)| r.contains(&3)).unwrap();
        assert_eq!(seg.1.color, Some(gpui::blue()));
    }

    #[test]
    fn rainbow_overrides_cover_fold_tail_bracket() {
        // 回归锁定：折叠 tail（收尾行的 `}`）必须拿到与表头行 `{` 同源的
        // 彩虹层级色。spans 由 scan_bracket_spans 产出，配对括号共享层级。
        // 场景：表头 `{` 在 offset 10，tail `}` 在 offset 40，二者同 depth。
        let dark = true;
        let error = gpui::red();
        let spans: Vec<BracketSpan> = vec![(10..11, 2, true, true), (40..41, 2, false, true)];
        // tail range 40..41 → 只有闭括号 span 命中
        let overrides = TextElement::rainbow_overrides_in_range(&spans, &(40..41), dark, error);
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].0, 40..41);
        // tail 闭括号的颜色 = 同层级开括号的颜色（即表头 `{` 实际渲染色）
        let header_overrides =
            TextElement::rainbow_overrides_in_range(&spans, &(10..11), dark, error);
        assert_eq!(overrides[0].1.color, header_overrides[0].1.color);
        // 游离闭括号（未配对）标错误色
        let unmatched: Vec<BracketSpan> = vec![(40..41, 2, false, false)];
        let overrides = TextElement::rainbow_overrides_in_range(&unmatched, &(40..41), dark, error);
        assert_eq!(overrides[0].1.color, Some(error));
        // 与 tail 不相交的 span 不产生覆盖
        let overrides = TextElement::rainbow_overrides_in_range(&spans, &(50..60), dark, error);
        assert!(overrides.is_empty());
    }
}
