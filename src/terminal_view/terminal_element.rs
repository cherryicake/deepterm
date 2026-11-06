//! Terminal element for rendering

use crate::components::cursor_animation::{CursorQuad, CursorShape, CursorTarget};
use crate::sidebar_view::font_settings::{FontState, platform_default_fallbacks};
use crate::terminal::mappings::colors::TerminalPalette;
use crate::terminal::terminal::{IndexedCell, Terminal, TerminalBounds};
use crate::terminal_view::box_drawing;
use crate::terminal_view::terminal_view::TerminalView;
use crate::theme::{ActiveTheme, TerminalTheme};
#[cfg(test)]
use alacritty_terminal::index::Point as AlacPoint;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::vte::ansi::CursorShape as AlacCursorShape;
use gpui::{
    Bounds, Element, ElementId, ElementInputHandler, Entity, FocusHandle, Font, FontStyle,
    FontWeight, GlobalElementId, Hsla, IntoElement, LayoutId, MouseButton, MouseUpEvent, Pixels,
    Point, Style, TextRun, TextStyle, WhiteSpace, Window, fill, point, px, size,
};
use unicode_width::UnicodeWidthChar;

const TERMINAL_HORIZONTAL_PADDING: f32 = 4.0;
const CURSOR_THICKNESS_RATIO: f32 = 0.15;

/// Returns how many terminal cells this Unicode scalar usually occupies.
/// This is used for cursor width and run-splitting math.
#[inline]
fn char_cell_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1).max(1)
}

#[inline]
fn terminal_content_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let horizontal_padding = px(TERMINAL_HORIZONTAL_PADDING);
    let padded_width = horizontal_padding * 2.0;
    let width = if bounds.size.width > padded_width {
        bounds.size.width - padded_width
    } else {
        Pixels::ZERO
    };

    Bounds {
        origin: point(bounds.left() + horizontal_padding, bounds.top()),
        size: size(width, bounds.size.height),
    }
}

#[inline]
fn cursor_thickness(cell_width: Pixels) -> Pixels {
    px((cell_width.as_f32() * CURSOR_THICKNESS_RATIO)
        .round()
        .max(1.0))
}

/// `DEEPTERM_DISABLE_KITTY` is a process-lifetime diagnostic switch; reading
/// the environment on every frame is unnecessary (and `var_os` takes a lock).
fn kitty_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("DEEPTERM_DISABLE_KITTY").is_some())
}

/// 绘制自绘下划线装饰（双线 / 点线 / 虚线）。
///
/// 底部锚定与 IME 下划线一致（`row_y + line_height - 1px`），
/// 保证与 GPUI 原生单线 / wavy 下划线视觉对齐。
fn paint_decoration(
    dec: &DecorationSpan,
    origin: Point<Pixels>,
    row_idx: usize,
    cell_width: Pixels,
    line_height: Pixels,
    window: &mut Window,
) {
    let row_y = origin.y + line_height * row_idx as f32;
    let span_w = cell_width * dec.width as f32;
    let x0 = origin.x + cell_width * dec.start_col as f32;
    let mut line = |y: Pixels, x: Pixels, w: Pixels| {
        window.paint_quad(fill(Bounds::new(point(x, y), size(w, px(1.0))), dec.color));
    };

    match dec.kind {
        DecorationKind::DoubleUnderline => {
            let y1 = row_y + line_height - px(2.0);
            let y2 = row_y + line_height - px(4.0);
            line(y1, x0, span_w);
            line(y2, x0, span_w);
        }
        DecorationKind::DottedUnderline => {
            // 点 + 间隔按字符格宽缩放（半个 cell 一个周期），
            // 否则小字号下糊成实线、大字号下散成稀点。
            let y = row_y + line_height - px(2.0);
            let step = (cell_width * 0.5).max(px(2.0)).as_f32();
            let end = (x0 + span_w).as_f32();
            let mut x = x0.as_f32();
            while x < end {
                line(y, px(x), px((step / 2.0f32).min(end - x)));
                x += step;
            }
        }
        DecorationKind::DashedUnderline => {
            // 段长约占 0.9 个 cell，间隔 0.4 个 cell
            let y = row_y + line_height - px(2.0);
            let step = (cell_width * 0.9).max(px(4.0)).as_f32();
            let dash = step * 0.6;
            let end = (x0 + span_w).as_f32();
            let mut x = x0.as_f32();
            while x < end {
                line(y, px(x), px(dash.min(end - x)));
                x += step;
            }
        }
    }
}

#[inline]
fn rendered_cell_colors(
    cell: &crate::terminal::terminal::PackedCell,
) -> (
    &alacritty_terminal::vte::ansi::Color,
    &alacritty_terminal::vte::ansi::Color,
) {
    if cell
        .flags
        .contains(alacritty_terminal::term::cell::Flags::INVERSE)
    {
        (&cell.bg, &cell.fg)
    } else {
        (&cell.fg, &cell.bg)
    }
}

/// Pure helper: check if a grid point falls within the given selection range.
/// Takes the pre-fetched range so callers avoid locking the FairMutex per cell.
#[inline]
#[cfg(test)]
fn is_point_selected(point: &AlacPoint, range: &Option<SelectionRange>) -> bool {
    range.as_ref().is_some_and(|range| range.contains(*point))
}

/// 用算术区间求交直接产出一行内的选区 span（O(1)，不逐格扫描）。
///
/// `cells` 的 point 与 `range` 的 start/end 同为网格坐标（line 可为负
/// 的 scrollback 偏移），行 line 取首格坐标。语义与逐格调用
/// `SelectionRange::contains` 完全等价（见 reference 平价测试）：
/// Simple/Semantic/Lines 选区（is_block=false）：中间行整行高亮，
/// 首行取 [start.col, 行尾]，尾行取 [0, end.col]；Block 选区：
/// 每个相交行取相同列区间 [start.col, end.col]。
fn selection_spans_for_row(
    row_idx: usize,
    row_cells: &[IndexedCell],
    selection_range: &Option<SelectionRange>,
) -> Vec<BackgroundSpan> {
    let Some(range) = selection_range else {
        return Vec::new();
    };
    let (Some(first), Some(last)) = (row_cells.first(), row_cells.last()) else {
        return Vec::new();
    };
    let line = first.point.line;
    if line < range.start.line || line > range.end.line {
        return Vec::new();
    }

    let last_col = last.point.column.0;
    let (left, right) = if range.is_block || (line == range.start.line && line == range.end.line) {
        // Block 选区：每个相交行取相同列区间；单行选区两端点即区间。
        (range.start.column.0, range.end.column.0)
    } else if line == range.start.line {
        (range.start.column.0, last_col)
    } else if line == range.end.line {
        (0, range.end.column.0)
    } else {
        (0, last_col)
    };

    // 区间与 [0, last_col] 求交：起点越界 → 该行无选区（不能截断，
    // 否则 start.column 越界的选区会伪造一个行尾 span）；终点越界
    // → 截到行尾（contains 语义：p.column <= end.column 对整行成立）。
    if left > last_col {
        return Vec::new();
    }
    let right = right.min(last_col);
    if left > right {
        return Vec::new();
    }
    vec![BackgroundSpan::for_selection(
        left,
        row_idx,
        right - left + 1,
    )]
}

/// 平价测试参考实现：逐格 `SelectionRange::contains` 扫描再合并成 span。
/// 与 [`Self::selection_spans_for_row`] 输出必须完全一致
/// （`selection_spans_parity` 随机平价测试守护）。
#[cfg(test)]
fn selection_spans_for_row_reference(
    row_idx: usize,
    row_cells: &[IndexedCell],
    selection_range: &Option<SelectionRange>,
) -> Vec<BackgroundSpan> {
    let mut cols: Vec<usize> = Vec::new();
    for cell in row_cells {
        if is_point_selected(&cell.point, selection_range) {
            cols.push(cell.point.column.0);
        }
    }
    selection_spans_for_cols(row_idx, &cols)
}

/// Coalesce a row's sorted selected column indices into contiguous spans
/// (`start_col`, `width`). Emits one `BackgroundSpan` per contiguous run,
/// collapsing what would otherwise be one quad per selected cell into one
/// quad per run. See `BackgroundSpan::for_selection`.
///
/// 仅由平价测试的参考实现使用；生产路径走
/// [`selection_spans_for_row`] 的 O(1) 算术求交。
#[cfg(test)]
fn selection_spans_for_cols(row: usize, cols: &[usize]) -> Vec<BackgroundSpan> {
    let mut spans = Vec::new();
    let mut iter = cols.iter().peekable();
    while let Some(&start) = iter.next() {
        let mut width = 1;
        while let Some(&&next) = iter.peek() {
            if next == start + width {
                width += 1;
                iter.next();
            } else {
                break;
            }
        }
        spans.push(BackgroundSpan::for_selection(start, row, width));
    }
    spans
}

/// Cached per-row layout for one visible row.
/// Stored in `TerminalView::row_render_cache` and reused across frames when the row is clean.
#[derive(Clone)]
pub struct RowLayoutEntry {
    pub runs: Vec<BatchedTextRun>,
    /// 自绘下划线装饰（double / dotted / dashed，SGR 4:3-4:5）。
    /// 删除线与单线/curly 均由 GPUI TextRun 原生渲染，不在此列。
    pub decorations: Vec<DecorationSpan>,
}

#[derive(Clone, Default)]
pub struct RowRenderCache {
    pub layout: Option<RowLayoutEntry>,
    /// 选区高亮，直接以合并后的 span 形式存储（一次 quad 一个连续段）。
    pub selection_spans: Option<Vec<BackgroundSpan>>,
    pub box_cells: Option<Vec<BoxDrawingCell>>,
    pub background_spans: Option<Vec<BackgroundSpan>>,
    /// Last hovered URL column range used to build `layout`. When this changes,
    /// the cached layout must be invalidated so underline styles are recomputed.
    pub hovered_url_cols: Option<(usize, usize)>,
    /// 生成该缓存时的行内容指纹（来自 `Terminal::row_hashes`）。
    ///
    /// 全量 damage（滚动/清屏/批量输出）时用它判断「这一行内容其实没变」，
    /// 直接复用旧缓存；滚动场景配合 `Terminal::scroll_shift` 先把整行缓存
    /// 平移，只重建真正新进入视口的行。
    pub fingerprint: Option<u64>,
    /// 生成该缓存时的调色板指纹；与 `fingerprint` 一起决定能否复用。
    pub palette_hash: u64,
}

/// 把缓存里记录的绝对行号改成当前遍历行号。
///
/// 滚动复用会把某一行的缓存整体平移到另一个视口下标；`BackgroundSpan` /
/// `BoxDrawingCell` / 选区 span 里的 `row` 只用于一致性校验和光标例外判断，
/// 平移后必须重标，否则校验/光标会认错行。
fn retarget_row_cache(cache: &mut RowRenderCache, row: usize) {
    if let Some(spans) = cache.background_spans.as_mut() {
        for span in spans {
            span.row = row;
        }
    }
    if let Some(spans) = cache.selection_spans.as_mut() {
        for span in spans {
            span.row = row;
        }
    }
    if let Some(cells) = cache.box_cells.as_mut() {
        for cell in cells {
            cell.row = row;
        }
    }
}

/// 按滚动位移平移行缓存：`new[i] = old[i + shift]`，越界行留空待重建。
fn rotate_row_caches(
    mut caches: Vec<RowRenderCache>,
    shift: isize,
    rows: usize,
) -> Vec<RowRenderCache> {
    if shift == 0 || rows == 0 {
        return caches;
    }
    if caches.len() != rows {
        caches.resize_with(rows, RowRenderCache::default);
    }
    let magnitude = shift.unsigned_abs();
    if magnitude >= rows {
        for cache in &mut caches {
            *cache = RowRenderCache::default();
        }
        return caches;
    }
    // 原地旋转，越界端重置为默认；不再分配 slots/rotated 两个 Vec。
    if shift > 0 {
        caches.rotate_left(magnitude);
        for cache in &mut caches[rows - magnitude..] {
            *cache = RowRenderCache::default();
        }
    } else {
        caches.rotate_right(magnitude);
        for cache in &mut caches[..magnitude] {
            *cache = RowRenderCache::default();
        }
    }
    caches
}

/// 盒子绘制字符 / 块元素单元格。
///
/// 对于 U+2500–U+259F 范围内的字符，不走字体光栅化流程，
/// 而是在 `paint` 阶段用 GPUI 矩形图元直接绘制，
/// 以保证相邻单元格的线条精确对齐（不受字体字形影响）。
///
/// 注意：`row` 仅供行缓存一致校验比对（`assert_row_consistency`）；
/// paint 阶段按行序遍历缓存，y 坐标取**遍历下标**而非此字段。
#[derive(Clone, Debug, PartialEq)]
pub struct BoxDrawingCell {
    pub col: usize,
    pub row: usize,
    pub c: char,
    pub color: Hsla, // 该字符的前景色（已处理 INVERSE 反色标志）
}

#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundSpan {
    pub start_col: usize,
    pub row: usize,
    pub width: usize,
    pub color: Hsla,
}

impl BackgroundSpan {
    /// Construct a selection-highlight span over a contiguous run of cells.
    ///
    /// `BackgroundSpan.color` carries the cell foreground for *background* spans
    /// painted later, but the selection quad is painted with
    /// `terminal_theme.selection_background`, so the `color` here is a dummy
    /// that is never read. Centralizing it keeps the dummy out of every
    /// call site and makes "why black" self-documenting.
    #[inline]
    pub fn for_selection(start_col: usize, row: usize, width: usize) -> Self {
        Self {
            start_col,
            row,
            width,
            color: gpui::black(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderMetricsSignature {
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub cell_width: Pixels,
    pub viewport_lines: usize,
    pub viewport_cols: usize,
    /// Resolved font id (family + weight + style). The signature must cover
    /// this so that changing the terminal font in settings invalidates the
    /// cached `ShapedLine`s even when the resulting cell metrics (size, line
    /// height, cell width) happen to be unchanged. `FontState::update` only
    /// refreshes windows; it does not recreate the view or clear caches.
    pub font_id: usize,
}

/// 跨帧渲染缓存签名（O2 优化后不再保存渲染数据的第二份拷贝）。
///
/// paint 阶段直接按行序遍历 `RowRenderCache`（见 `TerminalElement::paint`），
/// 本结构只保留「缓存是否仍有效」的判定信号：排版度量签名。
///
/// 这里曾有一个 `hovered_link` 字段，用来「记住产生本缓存的悬停状态」——
/// 但悬停失效早已改由 `RowRenderCache::hovered_url_cols` 逐行比对（见
/// `prepaint` 的行循环），该字段写入后从未被读过。它每帧被 `clone` 一次
/// （`HoveredLink` 344 字节 + 其 `String`/`SharedString` 堆分配），是一处
/// 纯浪费，已删除。`Copy` 让签名比对不再有克隆成本。
#[derive(Clone, Copy, Default)]
pub struct FlattenedRenderCache {
    pub metrics: Option<RenderMetricsSignature>,
}

/// 按形状把光标画进窗口。
///
/// 全部形状都基于四角四边形绘制（与 neovide 相同，移动时剪切形变）：
/// Block / Bar / Underline 走路径填充，Hollow 走空心描边
/// （对应 neovide 的 `draw_rectangle` / `draw_rectangular_outline`）。
fn paint_cursor_shape(
    window: &mut Window,
    shape: CursorShape,
    quad: &CursorQuad,
    thickness: Pixels,
    color: Hsla,
) {
    match shape {
        CursorShape::Hollow => {
            quad.paint_outline(window, thickness.as_f32(), color);
        }
        _ => {
            quad.paint_fill(window, color);
        }
    }
}

/// CursorLayout - simplified implementation inspired by Zed's CursorLayout
/// Computed during prepaint phase, rendered during paint phase
pub struct CursorLayout {
    /// 本帧绘制四边形（四角，已含弹簧动画的位置与形状形变）。
    ///
    /// 与 neovide 相同：四个角各跑各的弹簧，移动过程中被剪切成平行四边形。
    /// 静止时退化为轴对齐矩形。
    quad: CursorQuad,
    /// 静止目标（格子左上角，像素）：反白字符与盒字符按它定位
    /// （neovide 同样在静止 `destination` 上画字符，动画只影响光标背景）。
    destination: Point<Pixels>,
    /// 静止宽度（未动画），用于判断形状 morph 是否收敛。
    settled_width: Pixels,
    settled_height: Pixels,
    thickness: Pixels,
    color: gpui::Hsla,
    shape: CursorShape,
    /// Shaped glyph painted over a Block cursor in the background color to
    /// produce the inverted look. `None` when the underlying cell is empty
    /// or the char is handled by `block_box_drawing` below.
    block_text: Option<gpui::ShapedLine>,
    /// When the character under a Block cursor is a U+2500–U+259F box-drawing
    /// character, we bypass the font rasterizer and re-draw the same glyph
    /// using the inverted color via `box_drawing::paint_box_drawing`.
    /// (char, cell_width) is enough to re-run the existing paint routine.
    block_box_drawing: Option<(char, Hsla)>,
}

impl CursorLayout {
    fn paint(&self, window: &mut Window, cx: &mut gpui::App) {
        paint_cursor_shape(window, self.shape, &self.quad, self.thickness, self.color);

        // 反白字符：与 neovide 一致，画在**静止目标**上（不是动画中的四边形），
        // 并裁剪到四边形的包围盒内——移动过程中字形随光标逐步显露，
        // 而不是先于光标浮现在目标位置。
        //
        // neovide 用 `clip_path` 做精确的平行四边形裁剪；GPUI 只有矩形
        // content mask，这里用四边形的包围盒近似（略宽松，不会漏画）。
        if self.block_text.is_some() || self.block_box_drawing.is_some() {
            let clip = gpui::ContentMask {
                bounds: self.quad.bounding_box(),
            };
            window.with_content_mask(Some(clip), |window| {
                // Paint character on block cursor with inverted colors
                if let Some(block_text) = &self.block_text {
                    let _ = block_text.paint(
                        self.destination,
                        self.settled_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }

                // Paint inverted box-drawing glyph on top of a Block cursor.
                // `block_width` may be wider than `cell_width` for wide chars, but
                // box-drawing characters are never wide (U+2500–U+259F); use the
                // cell_width implicit from `block_width` when it equals 1 cell.
                if let Some((ch, color)) = self.block_box_drawing {
                    box_drawing::paint_box_drawing(
                        ch,
                        self.destination,
                        self.settled_width,
                        self.settled_height,
                        color,
                        window,
                    );
                }
            });
        }
    }
}

pub struct LocalOverlayLayout {
    /// 多行视觉行布局；每行单独 shape 出 ShapedLine。
    rows: Vec<OverlayRow>,
    /// 起始视觉行的左上角（即 PTY 光标行的原点）。
    origin: Point<Pixels>,
    /// 视觉光标位置（行/列均相对起始行 / 起始列 0）。
    cursor_row: usize,
    cursor_col: usize,
    cursor_shape: CursorShape,
    /// 本帧覆盖层光标的绘制四边形（已含弹簧动画位置/形状）。
    /// 由 prepaint 的动画器算出；`None` = 本帧不绘制（闪烁灭相）。
    cursor_quad: Option<CursorQuad>,
    cursor_thickness: Pixels,
    cursor_color: Hsla,
}

pub struct OverlayRow {
    /// 段集合：每段独立 shape，按起始 grid 列定位。
    /// 宽字符（width≥2）独占一段以避免 GPUI `force_width` 把宽字符压成 1 cell。
    segments: Vec<ShapedSegment>,
    /// 行内 IME 段在 grid 列上的 `[start, end)`，用于绘制 IME 背景 / 下划线。
    ime_col_range: Option<(usize, usize)>,
}

pub struct ShapedSegment {
    shaped_line: gpui::ShapedLine,
    /// 段起始列（grid 列）。
    start_col: usize,
}

/// 一行里的一段同款式文本，带已排版好的 `ShapedLine`。
///
/// `shaped_line` 走 `Arc`：`ShapedLine` 本体约 3 KB（内联 32 条
/// `DecorationRun` 的 `SmallVec`），而 `BatchedTextRun` 会被成行成批地
/// `clone`（增量重算里保留未变动的 run、整行布局回写前的暂存），按值克隆
/// 会把这几 KB 逐 run 拷一遍。`Arc` 之后克隆是一次引用计数自增，结构体
/// 从 3008 字节降到 32 字节。
#[derive(Clone, Debug)]
pub struct BatchedTextRun {
    pub start_col: usize,
    pub cell_count: usize,
    pub shaped_line: std::sync::Arc<gpui::ShapedLine>,
}

impl BatchedTextRun {
    fn new(col: usize, cell_count: usize, shaped_line: gpui::ShapedLine) -> Self {
        BatchedTextRun {
            start_col: col,
            cell_count,
            shaped_line: std::sync::Arc::new(shaped_line),
        }
    }
}

/// 文本 run 的绘制原点。`row` 必须由调用方按当前遍历下标传入。
fn run_paint_origin(
    origin: Point<Pixels>,
    start_col: usize,
    row: usize,
    cell_width: Pixels,
    line_height: Pixels,
) -> Point<Pixels> {
    Point::new(
        origin.x + start_col as f32 * cell_width,
        origin.y + row as f32 * line_height,
    )
}

impl BatchedTextRun {
    /// `row` 由调用方按遍历下标显式传入，不能缓存在 run 里。
    ///
    /// 行缓存会在滚动时整行平移到另一个视口下标；若 run 自带旧行号，
    /// paint 会把这一行画回旧 y 坐标，表现为“同一个提示符出现两次”。
    pub fn paint(
        &self,
        origin: Point<Pixels>,
        row: usize,
        cell_width: Pixels,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let pos = run_paint_origin(origin, self.start_col, row, cell_width, line_height);

        let _ = self
            .shaped_line
            .paint(pos, line_height, gpui::TextAlign::Left, None, window, cx);
    }
}

/// 下划线装饰种类（由 cell flags 派生，含 URL 悬停）。
///
/// - `Single` / `Curly` / `HoverLink` 由 GPUI `TextRun.underline` 直接渲染；
/// - `Double` / `Dotted` / `Dashed` 需自绘（GPUI 下划线仅支持单线 / wavy），
///   在 [`TerminalElement::layout_row`] 归集为 [`DecorationSpan`] 后
///   于 paint 阶段以矩形图元绘制。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnderlineKind {
    None,
    Single,
    Curly,
    Double,
    Dotted,
    Dashed,
    /// URL 悬停高亮：蓝色单下划线（非 cell flag）。
    HoverLink,
}

/// 自绘文本装饰（双下划线 / 点线 / 虚线 / 删除线），
/// 在行内聚为连续 span 后于 paint 阶段绘制。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecorationSpan {
    start_col: usize,
    /// 行内连续 cell 数
    width: usize,
    kind: DecorationKind,
    color: gpui::Hsla,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecorationKind {
    DoubleUnderline,
    DottedUnderline,
    DashedUnderline,
}

/// 从 cell flags 派生下划线种类（不含 URL 悬停状态）。
#[inline]
fn underline_kind_from_flags(flags: &alacritty_terminal::term::cell::Flags) -> UnderlineKind {
    use alacritty_terminal::term::cell::Flags;
    if flags.contains(Flags::UNDERLINE) {
        UnderlineKind::Single
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineKind::Double
    } else if flags.contains(Flags::UNDERCURL) {
        UnderlineKind::Curly
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineKind::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineKind::Dashed
    } else {
        UnderlineKind::None
    }
}

/// Cheap pre-style key that fully determines every field of the `TextRun`
/// produced by [`TerminalElement::text_run_for_cell`] for a given cell:
/// the rendered foreground (after the INVERSE swap) selects `color`, the
/// BOLD/ITALIC bits select `font`, and the underline kind (none / cell flag /
/// hovered URL) selects `underline` (the flag variant's color derives from the
/// raw `fg`, which is also part of the key).
///
/// During row layout the hot path is *extending* the current batch, which
/// only needs an equality check against the batch's key. Constructing the
/// full `TextRun` (including the theme color conversion) is then done once
/// per batch start instead of once per cell — for typical output (a whole
/// row sharing one style) this removes ~cols conversions per row.
#[derive(Clone, PartialEq)]
struct BatchStyleKey {
    rendered_fg: alacritty_terminal::vte::ansi::Color,
    raw_fg: alacritty_terminal::vte::ansi::Color,
    bold: bool,
    italic: bool,
    /// 下划线种类（none/single/curly/hover-link 与自绘种类）。
    underline: UnderlineKind,
    /// 已解析的下划线色（SGR 58 显式色，否则前景色）——与种类一起决定
    /// 最终下划线绘制，参与批次相等判定。
    underline_color: gpui::Hsla,
    /// 删除线（SGR 9）：走 GPUI TextRun.strikethrough。
    strikethrough: bool,
}

impl BatchStyleKey {
    #[inline]
    fn for_cell(
        cell: &IndexedCell,
        is_hovered_url: bool,
        palette: &TerminalPalette,
        theme: &TerminalTheme,
    ) -> Self {
        use alacritty_terminal::term::cell::Flags;
        let flags = cell.cell.flags;
        let underline = if is_hovered_url {
            UnderlineKind::HoverLink
        } else {
            underline_kind_from_flags(&flags)
        };
        // 下划线色：SGR 58 显式色优先，否则用渲染前景色（xterm 语义）。
        let rendered_fg = if flags.contains(Flags::INVERSE) {
            cell.cell.bg
        } else {
            cell.cell.fg
        };
        let fg_hsla = palette.resolve(&rendered_fg, theme);
        let underline_color = match cell.cell.underline_color {
            Some(color) => palette.resolve(&color, theme),
            None => fg_hsla,
        };
        Self {
            rendered_fg,
            raw_fg: cell.cell.fg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline,
            underline_color,
            strikethrough: flags.contains(Flags::STRIKEOUT),
        }
    }
}

pub struct TerminalElement {
    terminal: Entity<Terminal>,
    focus: FocusHandle,
    focused: bool,
    /// 闪烁不透明度：1.0 = 实心，0.0 = 不绘制。
    /// 淡入淡出窗口内由 `BlinkCursor::opacity` 给出中间值。
    cursor_opacity: f32,
    view: Entity<TerminalView>,
}

impl TerminalElement {
    fn flush_text_batch(
        batched_runs: &mut Vec<BatchedTextRun>,
        current_batch: &mut Option<(usize, String, TextRun, usize)>,
        window: &mut Window,
        font_size: Pixels,
        cell_width: Pixels,
    ) {
        if let Some((start_col, text, style, cell_count)) = current_batch.take() {
            let shaped_line = window.text_system().shape_line(
                text.into(),
                font_size,
                std::slice::from_ref(&style),
                Some(cell_width),
            );
            batched_runs.push(BatchedTextRun::new(start_col, cell_count, shaped_line));
        }
    }

    /// 从 BatchStyleKey 构造 GPUI TextRun。装饰性下划线（double/dotted/dashed）
    /// 不进 TextRun（GPUI 仅支持单线/wavy），由调用方归集 [`DecorationSpan`] 自绘。
    #[inline]
    fn text_run_for_key(
        key: &BatchStyleKey,
        resolved_fg: gpui::Hsla,
        text_style: &TextStyle,
        theme: &TerminalTheme,
    ) -> TextRun {
        let underline = match key.underline {
            UnderlineKind::Single => Some(gpui::UnderlineStyle {
                color: Some(key.underline_color),
                thickness: px(1.0),
                wavy: false,
            }),
            UnderlineKind::Curly => Some(gpui::UnderlineStyle {
                color: Some(key.underline_color),
                thickness: px(1.0),
                wavy: true,
            }),
            // URL 悬停：主题蓝色单下划线（颜色不取 cell 前景，保持链接语义）
            UnderlineKind::HoverLink => Some(gpui::UnderlineStyle {
                color: Some(theme.named_color(alacritty_terminal::vte::ansi::NamedColor::Blue)),
                thickness: px(1.0),
                wavy: false,
            }),
            // Double/Dotted/Dashed 走自绘；None 无装饰。
            UnderlineKind::Double
            | UnderlineKind::Dotted
            | UnderlineKind::Dashed
            | UnderlineKind::None => None,
        };
        TextRun {
            // 调用方补 `len`；此处用占位 0，因为构造后立即被覆盖。
            len: 0,
            font: Font {
                weight: if key.bold {
                    FontWeight::BOLD
                } else {
                    text_style.font_weight
                },
                style: if key.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                },
                ..text_style.font()
            },
            color: resolved_fg,
            // Backgrounds are painted separately as spans; keeping this None avoids
            // duplicate background work inside text painting.
            background_color: None,
            underline,
            strikethrough: key.strikethrough.then(|| gpui::StrikethroughStyle {
                color: Some(resolved_fg),
                thickness: px(1.0),
            }),
        }
    }

    #[cfg(debug_assertions)]
    fn run_signature(runs: &[BatchedTextRun]) -> Vec<(usize, usize, String)> {
        runs.iter()
            .map(|run| {
                (
                    run.start_col,
                    run.cell_count,
                    run.shaped_line.text.to_string(),
                )
            })
            .collect()
    }

    #[cfg(debug_assertions)]
    fn assert_row_consistency(
        row_idx: usize,
        row_cells: &[IndexedCell],
        row_runs: &[BatchedTextRun],
        row_boxes: &[BoxDrawingCell],
        row_backgrounds: &[BackgroundSpan],
        row_selection_spans: &[BackgroundSpan],
        window: &mut Window,
        text_style: &TextStyle,
        font_size: Pixels,
        cell_width: Pixels,
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
        selection_range: &Option<SelectionRange>,
    ) {
        let full_entry = Self::layout_row(
            row_cells.iter(),
            None,
            window,
            text_style,
            font_size,
            cell_width,
            terminal_theme,
            palette,
        );
        let full_boxes = Self::box_cells_for_row(row_idx, row_cells, terminal_theme, palette);
        let full_backgrounds =
            Self::background_spans_for_row(row_idx, row_cells, terminal_theme, palette);
        let full_selection_spans = selection_spans_for_row(row_idx, row_cells, selection_range);

        if Self::run_signature(row_runs) != Self::run_signature(&full_entry.runs) {
            log::warn!("row {row_idx} run mismatch");
        }
        if row_boxes != full_boxes.as_slice() {
            log::warn!("row {row_idx} box mismatch");
        }
        if row_backgrounds != full_backgrounds.as_slice() {
            log::warn!("row {row_idx} background mismatch");
        }
        if row_selection_spans != full_selection_spans.as_slice() {
            log::warn!("row {row_idx} selection mismatch");
        }
    }

    pub fn new(
        terminal: Entity<Terminal>,
        focus: FocusHandle,
        focused: bool,
        cursor_opacity: f32,
        view: Entity<TerminalView>,
    ) -> Self {
        Self {
            terminal,
            focus,
            focused,
            cursor_opacity,
            view,
        }
    }

    /// Compute the layout for a single visible row.
    fn layout_row<'a>(
        cells: impl IntoIterator<Item = &'a IndexedCell>,
        hovered_url_cols: Option<(usize, usize)>,
        window: &mut Window,
        text_style: &TextStyle,
        font_size: Pixels,
        cell_width: Pixels,
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> RowLayoutEntry {
        let mut batched_runs: Vec<BatchedTextRun> = Vec::new();
        let mut current_batch: Option<(usize, String, TextRun, usize)> = None;
        // Style key of the cell that started `current_batch`. Kept in sync so
        // the per-cell hot path only compares cheap enum/bitfield values.
        let mut current_key: Option<BatchStyleKey> = None;
        // 自绘下划线装饰（double/dotted/dashed）：同类型且列连续时合并 span。
        let mut decorations: Vec<DecorationSpan> = Vec::new();

        for cell in cells {
            let ch = cell.cell.c;
            let col = cell.point.column.0;
            let is_hovered_url = hovered_url_cols
                .map(|(start, end)| col >= start && col < end)
                .unwrap_or(false);

            // 空格/制表/方框绘制字符不进文字批次。Unicode 占位符
            // （U+10EEEE）也走这一支：它本身不该被当作字画出来，图案由
            // Kitty 图像在上面覆盖（见 prepaint 里的 `plan_placeholders`）。
            if ch == '\0'
                || ch == ' '
                || ch == crate::terminal::kitty::unicode::PLACEHOLDER
                || box_drawing::is_box_drawing_char(ch)
            {
                Self::flush_text_batch(
                    &mut batched_runs,
                    &mut current_batch,
                    window,
                    font_size,
                    cell_width,
                );
                current_key = None;
                // 空格同样参与装饰（下划线拖尾跨越空格是 SGR 的正常语义）。
                Self::collect_decoration(
                    &mut decorations,
                    cell,
                    is_hovered_url,
                    palette,
                    terminal_theme,
                );
                continue;
            }

            let key = BatchStyleKey::for_cell(cell, is_hovered_url, palette, terminal_theme);
            // 关键防御：用 let 链替代 unwrap，逻辑错乱时降级为断开批次而非整帧 panic。
            let batch_extends = match (current_batch.as_ref(), current_key.as_ref()) {
                (Some((start_col, _, _, cell_count)), Some(cur_key)) => {
                    cur_key == &key && *start_col + *cell_count == cell.point.column.0
                }
                _ => false,
            };

            if batch_extends {
                // 此处 current_batch 必为 Some（由 batch_extends 保证），
                // 仍用 if-let 防御，避免理论外的 panic 拖垮渲染。
                if let Some((_, text, style, cell_count)) = current_batch.as_mut() {
                    text.push(ch);
                    *cell_count += 1;
                    style.len += ch.len_utf8();
                }
            } else {
                Self::flush_text_batch(
                    &mut batched_runs,
                    &mut current_batch,
                    window,
                    font_size,
                    cell_width,
                );
                current_key = Some(key.clone());
                // 前景色在此处一次性解析（调色板覆盖 + 主题回退）。
                let resolved_fg = palette.resolve(&key.rendered_fg, terminal_theme);
                let mut cell_style =
                    Self::text_run_for_key(&key, resolved_fg, text_style, terminal_theme);
                cell_style.len = ch.len_utf8();
                current_batch = Some((cell.point.column.0, ch.to_string(), cell_style, 1));
            }

            Self::collect_decoration(
                &mut decorations,
                cell,
                is_hovered_url,
                palette,
                terminal_theme,
            );
        }

        Self::flush_text_batch(
            &mut batched_runs,
            &mut current_batch,
            window,
            font_size,
            cell_width,
        );

        RowLayoutEntry {
            runs: batched_runs,
            decorations,
        }
    }

    /// 归集一个 cell 的自绘下划线装饰（double / dotted / dashed）。
    /// 同类型、同色且列连续时与上一个 span 合并；其余情况开新 span。
    /// 删除线走 TextRun.strikethrough（见 [`Self::text_run_for_key`]）。
    #[inline]
    fn collect_decoration(
        decorations: &mut Vec<DecorationSpan>,
        cell: &IndexedCell,
        is_hovered_url: bool,
        palette: &TerminalPalette,
        theme: &TerminalTheme,
    ) {
        use alacritty_terminal::term::cell::Flags;
        let kind = match underline_kind_from_flags(&cell.cell.flags) {
            UnderlineKind::Double => Some(DecorationKind::DoubleUnderline),
            UnderlineKind::Dotted => Some(DecorationKind::DottedUnderline),
            UnderlineKind::Dashed => Some(DecorationKind::DashedUnderline),
            _ => None,
        };
        let Some(kind) = kind else {
            return;
        };
        // URL 悬停优先展示链接样式，跳过自绘装饰。
        if is_hovered_url {
            return;
        }
        let color = match cell.cell.underline_color {
            Some(color) => palette.resolve(&color, theme),
            None => palette.resolve(
                &if cell.cell.flags.contains(Flags::INVERSE) {
                    cell.cell.bg
                } else {
                    cell.cell.fg
                },
                theme,
            ),
        };
        let col = cell.point.column.0;
        match decorations.last_mut() {
            Some(span)
                if span.kind == kind
                    && span.color == color
                    && span.start_col + span.width == col =>
            {
                span.width += 1;
            }
            _ => decorations.push(DecorationSpan {
                start_col: col,
                width: 1,
                kind,
                color,
            }),
        }
    }

    fn box_cell_for_cell(
        row_idx: usize,
        cell: &IndexedCell,
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> Option<BoxDrawingCell> {
        let ch = cell.cell.c;
        if !box_drawing::is_box_drawing_char(ch) {
            return None;
        }
        let (fg, _) = rendered_cell_colors(&cell.cell);
        Some(BoxDrawingCell {
            col: cell.point.column.0,
            row: row_idx,
            c: ch,
            color: palette.resolve(fg, terminal_theme),
        })
    }

    fn background_spans_for_row(
        row_idx: usize,
        cells: &[IndexedCell],
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> Vec<BackgroundSpan> {
        Self::background_spans_for_range(row_idx, cells, (0, usize::MAX), terminal_theme, palette)
    }

    fn background_spans_for_range(
        row_idx: usize,
        cells: &[IndexedCell],
        range: (usize, usize),
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> Vec<BackgroundSpan> {
        let (left, right) = range;
        let mut backgrounds = Vec::new();
        let mut current_span: Option<BackgroundSpan> = None;

        for cell in cells {
            let col = cell.point.column.0;
            if col < left || col > right {
                continue;
            }

            let (_, bg) = rendered_cell_colors(&cell.cell);
            let color = if bg
                == &alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Background,
                ) {
                None
            } else {
                Some(palette.resolve(bg, terminal_theme))
            };

            match (current_span.as_mut(), color) {
                (Some(span), Some(color))
                    if span.color == color && span.start_col + span.width == col =>
                {
                    span.width += 1;
                }
                (Some(span), Some(color)) => {
                    backgrounds.push(span.clone());
                    *span = BackgroundSpan {
                        start_col: col,
                        row: row_idx,
                        width: 1,
                        color,
                    };
                }
                (None, Some(color)) => {
                    current_span = Some(BackgroundSpan {
                        start_col: col,
                        row: row_idx,
                        width: 1,
                        color,
                    });
                }
                (Some(span), None) => {
                    backgrounds.push(span.clone());
                    current_span = None;
                }
                (None, None) => {}
            }
        }

        if let Some(span) = current_span {
            backgrounds.push(span);
        }
        backgrounds
    }

    fn merge_background_spans(spans: &mut Vec<BackgroundSpan>) {
        spans.sort_by_key(|span| span.start_col);

        let mut merged: Vec<BackgroundSpan> = Vec::with_capacity(spans.len());
        for span in spans.drain(..) {
            if span.width == 0 {
                continue;
            }

            if let Some(last) = merged.last_mut()
                && last.row == span.row
                && last.color == span.color
                && last.start_col + last.width == span.start_col
            {
                last.width += span.width;
                continue;
            }

            merged.push(span);
        }

        *spans = merged;
    }

    fn split_cached_background_spans(
        cached_spans: &[BackgroundSpan],
        range: (usize, usize),
    ) -> Vec<BackgroundSpan> {
        let (left, right) = range;
        let mut backgrounds = Vec::new();

        for span in cached_spans {
            let span_left = span.start_col;
            let span_right = span.start_col + span.width.saturating_sub(1);

            if span_right < left || span_left > right {
                backgrounds.push(span.clone());
                continue;
            }

            if span_left < left {
                backgrounds.push(BackgroundSpan {
                    start_col: span_left,
                    row: span.row,
                    width: left - span_left,
                    color: span.color,
                });
            }

            if span_right > right {
                backgrounds.push(BackgroundSpan {
                    start_col: right + 1,
                    row: span.row,
                    width: span_right - right,
                    color: span.color,
                });
            }
        }

        backgrounds
    }

    fn update_background_spans_in_range(
        row_idx: usize,
        cached_spans: &[BackgroundSpan],
        cells: &[IndexedCell],
        range: (usize, usize),
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> Vec<BackgroundSpan> {
        let mut backgrounds = Self::split_cached_background_spans(cached_spans, range);
        backgrounds.extend(Self::background_spans_for_range(
            row_idx,
            cells,
            range,
            terminal_theme,
            palette,
        ));
        Self::merge_background_spans(&mut backgrounds);
        backgrounds
    }

    fn box_cells_for_row(
        row_idx: usize,
        cells: &[IndexedCell],
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> Vec<BoxDrawingCell> {
        let mut box_cells = Vec::new();
        for cell in cells {
            if let Some(box_cell) = Self::box_cell_for_cell(row_idx, cell, terminal_theme, palette)
            {
                box_cells.push(box_cell);
            }
        }
        box_cells
    }

    fn update_box_cells_in_range(
        row_idx: usize,
        cached_box_cells: &[BoxDrawingCell],
        cells: &[IndexedCell],
        range: (usize, usize),
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> Vec<BoxDrawingCell> {
        let (left, right) = range;
        let mut box_cells: Vec<BoxDrawingCell> = cached_box_cells
            .iter()
            .filter(|cell| cell.col < left || cell.col > right)
            .cloned()
            .collect();

        for cell in cells {
            let col = cell.point.column.0;
            if col < left || col > right {
                continue;
            }
            if let Some(box_cell) = Self::box_cell_for_cell(row_idx, cell, terminal_theme, palette)
            {
                box_cells.push(box_cell);
            }
        }

        box_cells.sort_by_key(|cell| cell.col);
        box_cells
    }

    /// 增量重算脏区间的文本 run 与自绘下划线装饰。
    ///
    /// 返回 `(rebuild_left, rebuild_right, runs, decorations)`：
    /// 前两项是**扩展后**的实际重建区间（函数内部会把区间拓到完整 run 段），
    /// 调用方据此丢弃该区间内的旧装饰 span——只回写 runs 会让装饰停留在旧状态
    /// （新增下划线画不出、撤掉的还在画）。
    fn try_update_runs_in_range(
        cached_runs: &[BatchedTextRun],
        cells: &[IndexedCell],
        range: (usize, usize),
        hovered_url_cols: Option<(usize, usize)>,
        window: &mut Window,
        text_style: &TextStyle,
        font_size: Pixels,
        cell_width: Pixels,
        terminal_theme: &TerminalTheme,
        palette: &TerminalPalette,
    ) -> Option<(usize, usize, Vec<BatchedTextRun>, Vec<DecorationSpan>)> {
        // `cached_runs` is emitted in cell order and kept sorted by `start_col`
        // by every merge below, so the connected segment around the dirty
        // range can be found with two monotonic cursors. The previous
        // fixed-point loop rescanned the whole run list once per expansion,
        // which is O(runs²) on long styled lines (coloured progress bars,
        // dense syntax highlighting) and sits on the per-frame path.
        debug_assert!(
            cached_runs
                .windows(2)
                .all(|pair| pair[0].start_col <= pair[1].start_col),
            "cached terminal runs must stay sorted by start_col"
        );

        let mut lo = cached_runs.partition_point(|run| {
            let run_end = run.start_col + run.cell_count.saturating_sub(1);
            run_end.saturating_add(1) < range.0
        });
        let mut hi = lo;
        let mut rebuild_left = range.0;
        let mut rebuild_right = range.1;

        // Expand to the whole contiguous (gap ≤ 1) run segment. Cursors only
        // move forward/backward and each run is visited at most once, so the
        // total work is linear even when left expansion enables more right
        // expansion.
        loop {
            let before = (lo, hi, rebuild_left, rebuild_right);

            while hi < cached_runs.len() {
                let run = &cached_runs[hi];
                if run.start_col > rebuild_right.saturating_add(1) {
                    break;
                }
                let run_end = run.start_col + run.cell_count.saturating_sub(1);
                rebuild_left = rebuild_left.min(run.start_col);
                rebuild_right = rebuild_right.max(run_end);
                hi += 1;
            }

            while lo > 0 {
                let run = &cached_runs[lo - 1];
                let run_end = run.start_col + run.cell_count.saturating_sub(1);
                if run_end.saturating_add(1) < rebuild_left {
                    break;
                }
                rebuild_left = rebuild_left.min(run.start_col);
                rebuild_right = rebuild_right.max(run_end);
                lo -= 1;
            }

            if before == (lo, hi, rebuild_left, rebuild_right) {
                break;
            }
        }

        let dirty_cells = cells
            .iter()
            .filter(|cell| (rebuild_left..=rebuild_right).contains(&cell.point.column.0));
        let dirty_entry = Self::layout_row(
            dirty_cells,
            hovered_url_cols,
            window,
            text_style,
            font_size,
            cell_width,
            terminal_theme,
            palette,
        );

        // Reuse the untouched prefix/suffix instead of filtering every cached
        // run into a fresh, repeatedly grown vec.
        let mut merged_runs =
            Vec::with_capacity(cached_runs.len() - (hi - lo) + dirty_entry.runs.len());
        merged_runs.extend_from_slice(&cached_runs[..lo]);
        merged_runs.extend(dirty_entry.runs);
        merged_runs.extend_from_slice(&cached_runs[hi..]);
        merged_runs.sort_by_key(|run| run.start_col);
        Some((
            rebuild_left,
            rebuild_right,
            merged_runs,
            dirty_entry.decorations,
        ))
    }

    /// 将 builtin / IME 文本经过 wrap + 切段后逐段 shape，得到 GPUI 渲染所需的多行布局。
    ///
    /// 返回 `None` 表示无内容可渲染（builtin 与 IME 都为空）。
    fn shape_overlay_layout(
        &self,
        params: OverlayShapeParams<'_>,
        window: &mut Window,
    ) -> Option<LocalOverlayLayout> {
        let OverlayShapeParams {
            builtin_text,
            ime_text,
            builtin_cursor,
            cursor_display_col,
            cursor_line_in_viewport,
            terminal_bounds,
            cell_width,
            line_height,
            font_size,
            text_style,
            builtin_color,
            ime_color,
            cursor_shape,
            cursor_color,
        } = params;
        if builtin_text.is_empty() && ime_text.is_empty() {
            return None;
        }

        let cols = terminal_bounds.num_columns();
        let wrapped = crate::terminal_view::overlay_wrap::wrap_overlay_text(
            builtin_text,
            ime_text,
            builtin_cursor,
            cursor_display_col,
            cols,
        );

        let overlay_x = terminal_bounds.bounds.left();
        let overlay_y = terminal_bounds.bounds.top()
            + px((cursor_line_in_viewport as f32 * line_height.as_f32()).floor());

        let mut rows: Vec<OverlayRow> = Vec::with_capacity(wrapped.lines.len());
        for line in &wrapped.lines {
            let ime_col_range = if line.ime_cells > 0 {
                let ime_start_col = line.start_col + line.builtin_cells;
                Some((ime_start_col, ime_start_col + line.ime_cells))
            } else {
                None
            };

            let mut segments: Vec<ShapedSegment> = Vec::with_capacity(line.segments.len());
            for seg in &line.segments {
                let color = if seg.is_ime { ime_color } else { builtin_color };
                let runs = [TextRun {
                    len: seg.text.len(),
                    font: text_style.font(),
                    color,
                    ..Default::default()
                }];
                let shaped = window.text_system().shape_line(
                    seg.text.clone().into(),
                    font_size,
                    &runs,
                    Some(cell_width),
                );
                segments.push(ShapedSegment {
                    shaped_line: shaped,
                    start_col: seg.start_col,
                });
            }

            rows.push(OverlayRow {
                segments,
                ime_col_range,
            });
        }

        Some(LocalOverlayLayout {
            rows,
            origin: point(overlay_x, overlay_y),
            cursor_row: wrapped.cursor_row,
            cursor_col: wrapped.cursor_col,
            cursor_shape,
            cursor_quad: None,
            cursor_thickness: cursor_thickness(cell_width),
            cursor_color,
        })
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// `TerminalElement` 一帧 prepaint 的产物，交给 paint 直接消费。
///
/// 旧实现是一个 11 元位置元组：构造点（prepaint 末尾）与解构点（paint
/// 开头）相隔数百行，字段顺序改一次就要两处同步核对，新增字段还必须数
/// 下标。换成命名字段后由编译器对齐，字段的语义也写在类型上。
///
/// 增加了两个**具名**的结构字段（`row_caches` / `search_spans`），却没有
/// 增加任何每帧开销：仍是一次 move 或引用。
pub struct TerminalPrepaintState {
    /// 视口行缓存：prepaint 从 view `take` 出来，paint 用完归还。
    ///
    /// 不变量：prepaint 从 view `take` 出行缓存到 paint 归还期间，view
    /// 上的 `row_render_cache` 为空。gpui 帧周期保证 prepaint×1 →
    /// paint×1 配对执行，paint 体内无提前 return，归还必然到达；若同一
    /// 帧意外出现第二次 prepaint（当前框架不会），会触发该次全量重排
    /// ——行为正确、仅性能回退。
    row_caches: Vec<RowRenderCache>,
    cell_width: Pixels,
    line_height: Pixels,
    /// 光标所在 grid 列（block 光标反白字符用）。
    cursor_col: usize,
    terminal_bounds: TerminalBounds,
    /// 真实光标布局（弹簧动画后的四边形）；None = 本帧不画光标。
    cursor: Option<CursorLayout>,
    /// builtin / IME 覆盖层布局。
    input_overlay: Option<LocalOverlayLayout>,
    /// 搜索命中高亮，按视口行分桶（含当前命中配色）。
    search_spans: Vec<Vec<BackgroundSpan>>,
    /// 本帧 Kitty 图像落位（已换算成像素，paint 直接使用）。
    placed_images: Vec<crate::terminal::kitty::render::PlacedImage>,
}

/// `shape_overlay_layout` 的入参。
///
/// 原先 15 个位置参数里同时混着「文本 + 光标偏移」「网格几何」「配色」
/// 三组语义，调用点只能靠数位置对齐；换成分组命名字段后，新增一项不必再
/// 动调用点以外的地方，也不会出现「行高传成列宽」这类静默错位。
#[derive(Clone, Copy)]
struct OverlayShapeParams<'a> {
    /// builtin command 文本与光标字节偏移（`builtin_cursor`）。
    builtin_text: &'a str,
    builtin_cursor: usize,
    /// IME 组字中的文本。
    ime_text: &'a str,
    /// 光标在 grid 上的显示列 / 视口行。
    cursor_display_col: usize,
    cursor_line_in_viewport: i32,
    /// 终端网格与单元格度量。
    terminal_bounds: TerminalBounds,
    cell_width: Pixels,
    line_height: Pixels,
    font_size: Pixels,
    text_style: &'a TextStyle,
    /// builtin / IME 前景色与覆盖层光标色。
    builtin_color: Hsla,
    ime_color: Hsla,
    cursor_shape: CursorShape,
    cursor_color: Hsla,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaintState;

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("terminal-element".into()))
    }

    crate::impl_source_location_none!();

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _window: &mut Window,
        _cx: &mut gpui::App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        let layout_id = _window.request_layout(style, [], _cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> Self::PrepaintState {
        let theme = cx.theme();
        let ui_theme = theme.ui;
        let terminal_theme = theme.terminal;
        // Compare by &str and clone only on the (rare) theme switch; the
        // steady-state frame must not allocate a String just to compare ids.
        let theme_changed =
            self.view.read(cx).cached_theme_id.as_deref() != Some(theme.id.as_str());
        let new_theme_id = theme_changed.then(|| theme.id.clone());
        let settings = &FontState::global(cx).settings;
        let font_size = px(settings.terminal_font_size);
        let line_height = font_size * settings.terminal_line_height;

        let font_family =
            crate::sidebar_view::font_settings::clean_font_choice(&settings.terminal_font_family)
                .to_string()
                .into();
        let font_fallbacks = Some(platform_default_fallbacks());

        let text_style = TextStyle {
            font_family,
            font_features: Default::default(),
            font_weight: FontWeight(settings.terminal_font_weight),
            font_fallbacks,
            font_size: font_size.into(),
            font_style: FontStyle::Normal,
            line_height: line_height.into(),
            background_color: None,
            white_space: WhiteSpace::Normal,
            color: terminal_theme.foreground,
            ..Default::default()
        };

        let font_id = window.text_system().resolve_font(&text_style.font());
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|a| a.width)
            .unwrap_or(px(9.0));

        let content_bounds = terminal_content_bounds(bounds);

        // 每帧上报请求的 bounds；内部合并器决定何时把「网格 reflow + PTY/SSH
        // 通知」原子应用。渲染始终使用已应用的 `terminal_bounds`，保证内容
        // 与网格尺寸一致、不会把 TUI 内容装进它没画过尺寸的网格而割裂。
        let requested = TerminalBounds::new(line_height, cell_width, content_bounds);

        let (terminal_bounds, display_offset, cursor_point, cursor_shape, kitty_dirty) =
            self.terminal.update(cx, |terminal, cx| {
                terminal.sync(window, cx);
                // 缩放随窗口而来：尺寸上报（CSI 14 t / CSI 16 t）与 PTY 的
                // `ws_xpixel` 需要物理像素，而 `requested` 是逻辑点。
                terminal.resize(requested, window.scale_factor(), cx);

                let content = terminal.last_content();
                (
                    terminal.terminal_bounds,
                    content.display_offset,
                    content.cursor,
                    content.cursor_shape,
                    // 图像集合变化不体现为网格 damage（例：删除一条不占格的
                    // 图像），必须单独取一次，否则残影留在屏幕上。
                    terminal.take_kitty_damage(),
                )
            });

        // Get viewport info for cursor calculation
        let viewport_lines = terminal_bounds.num_lines();

        // --- Row dirty tracking + per-row layout cache ---
        // dirty_rows is populated by Terminal::sync() via alacritty's damage API.
        //
        // The dirty-row snapshots are filled into reusable scratch buffers that
        // live on the view (instead of allocating fresh `Vec`s every frame via
        // `to_vec()`). We take them out of the view so we can hold owned locals
        // across the many interleaved `cx` calls below (a borrowed slice from
        // the view/terminal cannot survive those), then put them back at the end
        // so the capacity is reused on the next frame.
        let (
            mut dirty_rows,
            mut dirty_row_bounds,
            mouse_mode,
            selection_range,
            cursor_cell,
            palette,
        ) = {
            let (mut dirty_rows, mut dirty_row_bounds) = self.view.update(cx, |view, _cx| {
                (
                    std::mem::take(&mut view.scratch_dirty_rows),
                    std::mem::take(&mut view.scratch_dirty_row_bounds),
                )
            });
            let terminal_snapshot = self.terminal.read(cx);
            dirty_rows.clear();
            dirty_rows.extend_from_slice(terminal_snapshot.dirty_rows());
            dirty_row_bounds.clear();
            dirty_row_bounds.extend_from_slice(terminal_snapshot.dirty_row_bounds());
            // 图像集合变化时把整个 viewport 标脏：图像可能覆盖任意行，
            // 且添加/移除都不一定伴随字符变化，无法用行的 damage 区间表达。
            if kitty_dirty {
                dirty_rows.iter_mut().for_each(|row| *row = true);
                dirty_row_bounds
                    .iter_mut()
                    .for_each(|bounds| *bounds = Some((0, usize::MAX)));
            }
            let content = terminal_snapshot.last_content();
            let palette = content.palette;
            let mouse_mode = content
                .mode
                .intersects(alacritty_terminal::term::TermMode::MOUSE_MODE);
            // Always render local selection, even in mouse mode, so that
            // double/triple-click selections are visible inside tmux/vim.
            // Read the cached selection range from `last_content` (resolved while
            // the term lock was held in `update_last_content`) instead of calling
            // `selection_range()`, which would acquire the lock a second time.
            let selection_range = content.selection_range;
            let cursor_cell = content.cursor_cell;
            (
                dirty_rows,
                dirty_row_bounds,
                mouse_mode,
                selection_range,
                cursor_cell,
                palette,
            )
        };

        // Selection changes only invalidate selection slices inside row caches.
        // This is merged with the row-cache take below into a single
        // `view.update` to keep per-frame entity-borrow overhead low.
        let selection_range_for_cache = selection_range;
        let (mut row_caches, cached_flattened, selection_changed) =
            self.view.update(cx, |view, _cx| {
                let selection_changed = view.cached_selection_mouse_mode != mouse_mode
                    || view.cached_selection_range != selection_range_for_cache;
                if selection_changed {
                    for row_cache in &mut view.row_render_cache {
                        row_cache.selection_spans = None;
                    }
                    view.cached_selection_mouse_mode = mouse_mode;
                    view.cached_selection_range = selection_range_for_cache;
                }

                // Take row render cache out of the view so we can mutate it here
                // without a borrow conflict, then write it back after processing.
                if theme_changed {
                    // 主题更改清空缓存，全量刷新，避免终端字符渲染颜色与主题不一致
                    view.cached_theme_id = new_theme_id;
                    view.flattened_render_cache = FlattenedRenderCache::default();
                    (
                        Vec::new(),
                        FlattenedRenderCache::default(),
                        selection_changed,
                    )
                } else {
                    (
                        std::mem::take(&mut view.row_render_cache),
                        view.flattened_render_cache,
                        selection_changed,
                    )
                }
            });

        let metrics_signature = RenderMetricsSignature {
            font_size,
            line_height,
            cell_width,
            viewport_lines,
            viewport_cols: terminal_bounds.num_columns(),
            font_id: font_id.0,
        };
        let metrics_changed = cached_flattened.metrics != Some(metrics_signature);

        // Match Ghostty's grid-size-diff behavior: when the grid or text metrics
        // change, row-local shaped text and span caches are no longer valid.
        if metrics_changed {
            row_caches.clear();
        }
        row_caches.resize_with(viewport_lines, RowRenderCache::default);

        let mut dirty_count = 0usize;
        let mut cache_hit_count = 0usize;
        let mut partial_run_update_count = 0usize;
        // 只取悬停链接在本行的列区间：`HoveredLink` 有 344 字节 + 若干堆
        // 分配（`String`/`SharedString`），而行循环用到的只有 (row, 起止列)。
        // 整份 `clone` 每帧都白付一次堆拷贝。
        let hovered_span = self
            .view
            .read(cx)
            .hovered_link
            .as_ref()
            .map(|link| (link.row, link.start_col, link.end_col));

        // O2：行缓存本身就是渲染数据源（paint 直接逐行遍历），不再有
        // flattened 拷贝。任何输入变化（主题/resize/选区/悬停/damage/度量）
        // 只让相应行缓存失效，循环内每行按需重算 —— 单行 dirty 的帧不再
        // 触发全视口重新聚合。
        // 借用贯穿行缓存循环（NLL：guard 的最后一次使用在循环内，
        // 循环后的 `view.update` 不与该借用冲突），零拷贝。
        let terminal = self.terminal.read(cx);
        let content = terminal.last_content();
        let content_cells = &content.cells;
        let row_ranges = &content.row_ranges;
        let row_hashes = &content.row_hashes;

        // 全量 damage 时若识别出整体滚动，先把旧行缓存平移到新下标；
        // 之后逐行用内容指纹判断是否还需要重排。metrics 变化时缓存已清空，
        // 不做平移。
        if !metrics_changed && content.scroll_shift != 0 {
            row_caches = rotate_row_caches(row_caches, content.scroll_shift, viewport_lines);
        }

        #[cfg(debug_assertions)]
        let validate_row_cache = std::env::var_os("DEEPTERM_VALIDATE_ROW_CACHE").is_some();

        // `row_caches` 在上面已 resize 到 `viewport_lines`，逐行迭代等价于按下标遍历。
        for (row_idx, row_cache) in row_caches.iter_mut().enumerate() {
            // 发生整体滚动时，即使 damage 没把这一行标脏，平移到新下标的
            // 缓存也必须重新过一遍指纹；调色板变化同样要强制重建（cell 的
            // Color 枚举没变，最终颜色却变了）。
            let mut is_dirty = dirty_rows.get(row_idx).copied().unwrap_or(true)
                || content.scroll_shift != 0
                || row_cache.palette_hash != content.palette_hash;
            let row_cells: &[IndexedCell] = if let Some((start, end)) = row_ranges.get(row_idx) {
                &content_cells[*start..*end]
            } else {
                &[]
            };
            let hovered_url_cols = hovered_span.and_then(|(row, start_col, end_col)| {
                (row == row_idx).then_some((start_col, end_col))
            });

            // Invalidate cached layout when the hovered URL range on this row changes,
            // so that underline styles are recomputed even if terminal content is clean.
            if row_cache.hovered_url_cols != hovered_url_cols {
                row_cache.layout = None;
                row_cache.fingerprint = None;
                row_cache.hovered_url_cols = hovered_url_cols;
            }

            // 全量 damage（滚动/清屏/批量输出）时，行内容可能完全没变。
            // 指纹命中就跳过重排，只把 span 里的行号重标到当前下标。
            let row_hash = row_hashes.get(row_idx).copied();
            if is_dirty
                && row_hash.is_some()
                && row_cache.fingerprint == row_hash
                && row_cache.palette_hash == content.palette_hash
                && row_cache.layout.is_some()
            {
                retarget_row_cache(row_cache, row_idx);
                is_dirty = false;
            }

            if is_dirty || row_cache.layout.is_none() {
                dirty_count += 1;
                // Take ownership instead of cloning the run/decorations vecs:
                // on a full-damage frame the cached entry is about to be
                // replaced, and on a partial update it is consumed in place.
                let cached_entry = row_cache.layout.take();
                let dirty_range = dirty_row_bounds.get(row_idx).copied().flatten();
                let entry = if let (Some(mut cached_entry), Some(range)) =
                    (cached_entry, dirty_range)
                {
                    if let Some((rebuilt_left, rebuilt_right, updated_runs, updated_decorations)) =
                        Self::try_update_runs_in_range(
                            &cached_entry.runs,
                            row_cells,
                            range,
                            hovered_url_cols,
                            window,
                            &text_style,
                            font_size,
                            cell_width,
                            &terminal_theme,
                            &palette,
                        )
                    {
                        partial_run_update_count += 1;
                        cached_entry.runs = updated_runs;
                        // 装饰与文本 run 必须一起增量刷新：丢弃落在重建区间内的旧 span
                        // （区间已扩展到完整 run 段），再并入新 span 并按列排序。
                        cached_entry.decorations.retain(|span| {
                            span.start_col + span.width <= rebuilt_left
                                || span.start_col > rebuilt_right
                        });
                        cached_entry.decorations.extend(updated_decorations);
                        cached_entry.decorations.sort_by_key(|span| span.start_col);
                        cached_entry
                    } else {
                        Self::layout_row(
                            row_cells.iter(),
                            hovered_url_cols,
                            window,
                            &text_style,
                            font_size,
                            cell_width,
                            &terminal_theme,
                            &palette,
                        )
                    }
                } else {
                    Self::layout_row(
                        row_cells.iter(),
                        hovered_url_cols,
                        window,
                        &text_style,
                        font_size,
                        cell_width,
                        &terminal_theme,
                        &palette,
                    )
                };
                row_cache.layout = Some(entry);
                row_cache.fingerprint = row_hash;
                row_cache.palette_hash = content.palette_hash;
            } else {
                cache_hit_count += 1;
                // 缓存本来就命中时也补齐指纹，下一帧滚动检测才能用上。
                if row_cache.layout.is_some() {
                    row_cache.fingerprint = row_hash;
                    row_cache.palette_hash = content.palette_hash;
                }
            }

            if row_cache.box_cells.is_none() {
                let row_boxes =
                    Self::box_cells_for_row(row_idx, row_cells, &terminal_theme, &palette);
                row_cache.box_cells = Some(row_boxes);
            } else if is_dirty {
                if let Some(range) = dirty_row_bounds.get(row_idx).copied().flatten() {
                    // 关键防御：缓存理论上为 Some，异常时全量重算而非 panic。
                    let row_boxes = match row_cache.box_cells.as_ref() {
                        Some(cached_row_boxes) => Self::update_box_cells_in_range(
                            row_idx,
                            cached_row_boxes,
                            row_cells,
                            range,
                            &terminal_theme,
                            &palette,
                        ),
                        None => {
                            Self::box_cells_for_row(row_idx, row_cells, &terminal_theme, &palette)
                        }
                    };
                    row_cache.box_cells = Some(row_boxes);
                } else {
                    let row_boxes =
                        Self::box_cells_for_row(row_idx, row_cells, &terminal_theme, &palette);
                    row_cache.box_cells = Some(row_boxes);
                }
            }

            if row_cache.background_spans.is_none() {
                let backgrounds =
                    Self::background_spans_for_row(row_idx, row_cells, &terminal_theme, &palette);
                row_cache.background_spans = Some(backgrounds);
            } else if is_dirty {
                if let Some(range) = dirty_row_bounds.get(row_idx).copied().flatten() {
                    // 关键防御：缓存缺失时全量重算，保证渲染永不 panic。
                    let backgrounds = match row_cache.background_spans.as_ref() {
                        Some(cached_backgrounds) => Self::update_background_spans_in_range(
                            row_idx,
                            cached_backgrounds,
                            row_cells,
                            range,
                            &terminal_theme,
                            &palette,
                        ),
                        None => Self::background_spans_for_row(
                            row_idx,
                            row_cells,
                            &terminal_theme,
                            &palette,
                        ),
                    };
                    row_cache.background_spans = Some(backgrounds);
                } else {
                    let backgrounds = Self::background_spans_for_row(
                        row_idx,
                        row_cells,
                        &terminal_theme,
                        &palette,
                    );
                    row_cache.background_spans = Some(backgrounds);
                }
            }

            // O1：选区 span 直接以 O(1) 算术区间求交计算（逐格扫描的
            // 增量更新路径已删除）。重算触发条件：选区变化、行内容
            // 变化、或缓存缺失；稳态帧直接复用缓存。
            if selection_changed || is_dirty || row_cache.selection_spans.is_none() {
                let spans = selection_spans_for_row(row_idx, row_cells, &selection_range);
                row_cache.selection_spans = Some(spans);
            }

            #[cfg(debug_assertions)]
            if validate_row_cache {
                // 调试校验：缓存缺失时跳过校验而非 panic，避免 debug 模式误崩。
                if let (
                    Some(layout),
                    Some(row_boxes),
                    Some(row_backgrounds),
                    Some(row_selection_spans),
                ) = (
                    row_cache.layout.as_ref(),
                    row_cache.box_cells.as_ref(),
                    row_cache.background_spans.as_ref(),
                    row_cache.selection_spans.as_ref(),
                ) {
                    Self::assert_row_consistency(
                        row_idx,
                        row_cells,
                        &layout.runs,
                        row_boxes,
                        row_backgrounds,
                        row_selection_spans,
                        window,
                        &text_style,
                        font_size,
                        cell_width,
                        &terminal_theme,
                        &palette,
                        &selection_range,
                    );
                }
            }
        }

        let new_signature = FlattenedRenderCache {
            metrics: Some(metrics_signature),
        };
        // Single write-back: row caches, signature and the returned scratch
        // buffers (merged so the frame performs one entity update instead
        // of two). O2 后行缓存在 paint 结束前始终以局部变量存在，写回
        // 时机移到 paint 阶段之后由 `PrepaintState` 归还（见 paint 尾部）。
        self.view.update(cx, |view, _cx| {
            view.flattened_render_cache = new_signature;
            view.scratch_dirty_rows = std::mem::take(&mut dirty_rows);
            view.scratch_dirty_row_bounds = std::mem::take(&mut dirty_row_bounds);
        });

        log::trace!("row cache: {cache_hit_count}/{viewport_lines} hits, {dirty_count} recomputed");
        log::trace!("row run partial updates: {partial_run_update_count}");

        // Calculate cursor position relative to viewport
        let cursor_line_in_viewport = cursor_point.line.0 + display_offset as i32;

        // Cursor is only visible if it's within the current viewport
        let cursor_in_viewport =
            cursor_line_in_viewport >= 0 && (cursor_line_in_viewport as usize) < viewport_lines;

        let cursor_display_col = cursor_point.column.0;

        // 本地覆盖输入存在时隐藏终端光标，由覆盖层负责显示
        let has_local_overlay_input = self.view.read(cx).has_local_overlay_input();

        // 预先计算光标形状和颜色，供终端光标和覆盖层光标共用
        let overlay_cursor_shape = match cursor_shape {
            AlacCursorShape::Block if !self.focused => CursorShape::Hollow,
            AlacCursorShape::Block => CursorShape::Block,
            AlacCursorShape::Underline => CursorShape::Underline,
            AlacCursorShape::Beam => CursorShape::Bar,
            AlacCursorShape::HollowBlock => CursorShape::Hollow,
            AlacCursorShape::Hidden => CursorShape::Bar,
        };
        // beam / underline 的粗细比例（对应 neovide 的 `cell_percentage`）。
        //
        // nvim 通过 `mode_info_set` 的 `cell_percentage` 给出该值，alacritty
        // 没有这个概念；这里按 DeepTerm 原有的 `cursor_thickness`（cell_width
        // 的 15%，至少 1px）反推，保持改动前的视觉：
        // - Bar 的宽度按 **cell 宽**缩放 → 比例 = thickness / cell_width；
        // - Underline 的高度按 **行高**缩放 → 比例 = thickness / line_height；
        //   两者分母不同，所以必须分别算，否则下划线会粗一倍。
        let cursor_thickness_px = cursor_thickness(cell_width);
        let overlay_cell_percentage = match overlay_cursor_shape {
            CursorShape::Bar => {
                (cursor_thickness_px.as_f32() / cell_width.as_f32()).clamp(0.0, 1.0)
            }
            CursorShape::Underline => {
                (cursor_thickness_px.as_f32() / line_height.as_f32()).clamp(0.0, 1.0)
            }
            _ => CURSOR_THICKNESS_RATIO,
        };

        // 光标色走调色板（OSC 12 改色 / 主题切换实时生效）。
        let cursor_color = palette.resolve(
            &alacritty_terminal::vte::ansi::Color::Named(
                alacritty_terminal::vte::ansi::NamedColor::Cursor,
            ),
            &terminal_theme,
        );

        // 闪烁不透明度：1.0 = 实心；淡入淡出窗口内线性变化。
        // 同一因子同时作用于光标矩形与反白字符，避免半透明块上浮着
        // 一个实心反白字。
        let cursor_opacity = self.cursor_opacity.clamp(0.0, 1.0);
        let cursor_color = cursor_color.opacity(cursor_opacity);

        // Shape local overlay text for rendering at the shell prompt position.
        let mut input_overlay: Option<LocalOverlayLayout> = if cursor_in_viewport {
            let (builtin_text, ime_text, builtin_cursor) = {
                let view = self.view.read(cx);
                // 借用而非拷贝：空 builtin/IME 是绝大多数帧的常态，
                // `to_string()` 会为两个空串各付一次分配。
                (
                    view.builtin_command_text(),
                    view.ime_text(),
                    view.builtin_command_cursor(),
                )
            };
            self.shape_overlay_layout(
                OverlayShapeParams {
                    builtin_text,
                    builtin_cursor,
                    ime_text,
                    cursor_display_col,
                    cursor_line_in_viewport,
                    terminal_bounds,
                    cell_width,
                    line_height,
                    font_size,
                    text_style: &text_style,
                    builtin_color: ui_theme.text,
                    ime_color: terminal_theme.foreground,
                    cursor_shape: overlay_cursor_shape,
                    cursor_color,
                },
                window,
            )
        } else {
            None
        };

        // ── 光标弹簧动画 ────────────────────────────────────────────────────
        // 四个角各跑各的弹簧（与 neovide 相同），移动过程中光标被剪切成
        // 平行四边形；滚动（display_offset 变化）由动画器整体平移，不产生动画。
        let cursor_thickness_px = cursor_thickness(cell_width);
        let layout_origin = terminal_bounds.bounds.origin;

        // 主光标目标。形状为 Hidden（应用自绘光标）或视口外 / 覆盖层接管
        // 时为 None —— 动画器据此复位，下次出现直接就位而不是从中途飞入。
        let main_target: Option<CursorTarget> = if cursor_in_viewport
            && cursor_shape != AlacCursorShape::Hidden
            && !has_local_overlay_input
        {
            // 宽字符只在 Block 形状下加宽（与 neovide `double_width` 一致：
            // 那里同样只在 `shape == Block` 时把 `cursor_width` 翻倍）。
            // Bar / Underline 的尺寸由自身定义决定，与字符宽度无关。
            let wide_scale = if cursor_cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR)
            {
                2.0
            } else {
                char_cell_width(cursor_cell.c) as f32
            };
            let cursor_cells_wide = if overlay_cursor_shape == CursorShape::Block {
                wide_scale
            } else {
                1.0
            };
            let x = terminal_bounds.bounds.left()
                + px((cursor_display_col as f32 * cell_width.as_f32()).floor());
            let y = terminal_bounds.bounds.top()
                + px((cursor_line_in_viewport as f32 * line_height.as_f32()).floor());

            Some(CursorTarget {
                destination: Point::new(x.as_f32(), y.as_f32()),
                cell: (
                    cell_width.as_f32() * cursor_cells_wide,
                    line_height.as_f32(),
                ),
                // beam / underline 的粗细取「单格」比例，与 cell 是否加宽无关。
                shape_span: (cell_width.as_f32(), line_height.as_f32()),
                // 终端只有纵向滚动；换算成像素偏移交给动画器做平移。
                scroll_offset: Point::new(0.0, line_height.as_f32() * display_offset as f32),
                shape: overlay_cursor_shape,
                cell_percentage: overlay_cell_percentage,
                metrics: (cell_width, line_height),
                layout_origin,
            })
        } else {
            None
        };

        // 覆盖层光标目标：与主光标互斥，坐标同为窗口绝对像素。
        let overlay_target: Option<CursorTarget> = input_overlay.as_ref().map(|overlay| {
            let x = overlay.origin.x + cell_width * overlay.cursor_col as f32;
            let y = overlay.origin.y + line_height * overlay.cursor_row as f32;
            CursorTarget {
                destination: Point::new(x.as_f32(), y.as_f32()),
                cell: (cell_width.as_f32(), line_height.as_f32()),
                shape_span: (cell_width.as_f32(), line_height.as_f32()),
                // 终端只有纵向滚动；换算成像素偏移交给动画器做平移。
                scroll_offset: Point::new(0.0, line_height.as_f32() * display_offset as f32),
                shape: overlay.cursor_shape,
                cell_percentage: overlay_cell_percentage,
                metrics: (cell_width, line_height),
                layout_origin,
            }
        });

        let animated_quad = {
            let target = if cursor_opacity > 0.0 {
                overlay_target.or(main_target)
            } else {
                None
            };
            let reduce_motion = cx.reduce_motion();
            // 用 GPUI 调度器的时钟：测试里 `advance_clock` 能推进它，
            // 生产里与 `std::time::Instant` 同为单调墙钟。
            let now = cx.background_executor().now();
            let (quad, animating) = self.view.update(cx, |view, _cx| {
                // 「减少动态效果」下不做光标动画：每帧复位即每帧瞬移。
                if reduce_motion {
                    view.cursor_animator.reset();
                }
                let quad = view.cursor_animator.step(target, now);
                (quad, view.cursor_animator.is_animating())
            });
            // 仍在运动 → 请求下一帧。
            //
            // 走 `Window::on_next_frame`（平台帧源驱动：macOS 上是
            // CVDisplayLink，与显示器刷新率同步，ProMotion 下就是 120Hz），
            // 而不是 `background_executor` 的定时器——定时器不参与 vsync
            // 对齐，逐帧抖动会让弹簧的连续运动看起来发涩。
            //
            // 不用 `Window::request_animation_frame`：它内部按
            // `window.current_view()` 找要 notify 的实体，而 element 的
            // prepaint 不一定在 view 的 render 栈里（测试/复用路径下会 panic）。
            // 这里显式指定本 view 的实体，语义相同且更稳。
            if animating {
                let view_id = self.view.entity_id();
                window.on_next_frame(move |_, cx| cx.notify(view_id));
            }
            quad
        };

        if let Some(overlay) = input_overlay.as_mut() {
            overlay.cursor_quad = animated_quad;
        }

        // 计算光标布局 - 如果光标形状为Hidden则跳过（由应用程序自己处理光标渲染）
        // 这是光标委托机制：当应用程序（如Claude Code）设置光标为Hidden时，
        // 终端不渲染光标，让应用程序自己处理
        // IME 输入时也隐藏光标，由 preedit overlay 替代显示
        let cursor = match (main_target, animated_quad) {
            (Some(target), Some(quad)) => {
                let cursor_char = cursor_cell.c;
                let block_text_color = terminal_theme.background.opacity(cursor_opacity);

                // Preserve bold/italic so the inverted glyph matches the cell.
                let cursor_font = {
                    let mut f = text_style.font();
                    if cursor_cell
                        .flags
                        .contains(alacritty_terminal::term::cell::Flags::BOLD)
                    {
                        f.weight = FontWeight::BOLD;
                    }
                    if cursor_cell
                        .flags
                        .contains(alacritty_terminal::term::cell::Flags::ITALIC)
                    {
                        f.style = FontStyle::Italic;
                    }
                    f
                };

                // 与 neovide 一致：反白字符**始终**绘制，由 `paint` 里的
                // content mask 裁剪到当前四边形内。neovide 用 `clip_path`
                // 做同样的裁剪，字符不会溢出、也不会在动画中消失。
                let settled_width = px(target.cell.0);
                let settled_height = px(target.cell.1);
                let (block_text, block_box_drawing) =
                    if matches!(overlay_cursor_shape, CursorShape::Block) {
                        if box_drawing::is_box_drawing_char(cursor_char) {
                            (None, Some((cursor_char, block_text_color)))
                        } else if cursor_char != '\0' && cursor_char != ' ' {
                            let char_str = cursor_char.to_string();
                            let len = cursor_char.len_utf8();
                            let shaped = window.text_system().shape_line(
                                char_str.into(),
                                font_size,
                                &[TextRun {
                                    len,
                                    font: cursor_font,
                                    color: block_text_color,
                                    ..Default::default()
                                }],
                                Some(settled_width),
                            );
                            (Some(shaped), None)
                        } else {
                            (None, None)
                        }
                    } else {
                        (None, None)
                    };

                Some(CursorLayout {
                    quad,
                    destination: point(px(target.destination.x), px(target.destination.y)),
                    settled_width,
                    settled_height,
                    thickness: cursor_thickness_px,
                    color: cursor_color,
                    shape: overlay_cursor_shape,
                    block_text,
                    block_box_drawing,
                })
            }
            _ => None,
        };

        // ── Search highlight spans ─────────────────────────────────────────────────
        // 把 grid 坐标命中映射到当前视口行（每帧重算，滚动自然跟随），
        // 非当前命中用主题 warning 低透明度，当前命中加倍醒目。
        // 按视口行分桶。命中集可能是整个 scrollback 的数千条，旧实现
        // 每帧全量映射一遍、paint 时又对每一行过滤全部 span（O(rows×matches)）。
        // 匹配按 `start.line` 有序，因此这里先二分出视口区间，再按行归档。
        let search_spans: Vec<Vec<BackgroundSpan>> = {
            let view = self.view.read(cx);
            let search = &view.search;
            if !search.active || search.matches.is_empty() {
                Vec::new()
            } else {
                let match_color = ui_theme.warning.opacity(0.28);
                let active_color = ui_theme.warning.opacity(0.55);
                let offset = display_offset as i32;
                let first = search
                    .matches
                    .partition_point(|m| m.start.line.0 + offset < 0);
                let last = search
                    .matches
                    .partition_point(|m| m.start.line.0 + offset < viewport_lines as i32);
                let mut by_row: Vec<Vec<BackgroundSpan>> =
                    (0..viewport_lines).map(|_| Vec::new()).collect();
                for (idx, m) in search
                    .matches
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(last.saturating_sub(first))
                {
                    let viewport_row = (m.start.line.0 + offset) as usize;
                    let start_col = m.start.column.0;
                    let end_col = m.end.column.0;
                    by_row[viewport_row].push(BackgroundSpan {
                        start_col,
                        row: viewport_row,
                        width: end_col.saturating_sub(start_col),
                        color: if idx == search.active_index {
                            active_color
                        } else {
                            match_color
                        },
                    });
                }
                by_row
            }
        };

        // Kitty 图像的本帧落位。`plan` 只做几何计算（网格 → 像素），
        // 纹理的创建推迟到 paint（那里才有 `&mut Window`/`App` 可用，
        // 且只有真正要画的图像才值得做 RGBA→BGRA 转换）。
        let placed_images: Vec<crate::terminal::kitty::render::PlacedImage> = if kitty_disabled() {
            Vec::new()
        } else {
            // 绝大多数终端没有任何 Kitty 图像。旧实现仍然每帧扫描全部
            // 视口 cell 找 U+10EEEE 占位符，再跑一遍 plan/sort；空存储
            // 时直接跳过这三步。
            let has_images = self
                .terminal
                .read(cx)
                .with_kitty_storage(|storage| !storage.is_empty());
            if !has_images {
                Vec::new()
            } else {
                let metrics = crate::terminal::kitty::render::CellMetrics {
                    cell_width,
                    line_height,
                };
                let viewport = crate::terminal::kitty::render::Viewport {
                    origin: terminal_bounds.bounds.origin,
                    display_offset,
                    rows: viewport_lines,
                    cols: terminal_bounds.num_columns(),
                };
                // 占位符的源数据（网格里的 U+10EEEE 单元格）在终端实体上；先拍平
                // 出来，避免带着实体借用去做图像存储的几何计算。
                let placeholder_cells = self.terminal.read(cx).kitty_placeholder_cells();
                self.terminal.read(cx).with_kitty_storage(|storage| {
                    // 普通放置 + Unicode 占位符走两条路径，合成同一份绘制清单：
                    // 两者都用 `PlacedImage` 表达，paint 时按 z 序统一绘制。
                    let mut placed =
                        crate::terminal::kitty::render::plan(storage, metrics, viewport);
                    placed.extend(crate::terminal::kitty::render::plan_placeholders(
                        storage,
                        &placeholder_cells,
                        metrics,
                        viewport,
                    ));
                    placed.sort_by(|a, b| {
                        a.z.cmp(&b.z)
                            .then(a.id.cmp(&b.id))
                            .then(a.placement_id.cmp(&b.placement_id))
                    });
                    placed
                })
            }
        };

        TerminalPrepaintState {
            row_caches,
            cell_width,
            line_height,
            cursor_col: cursor_point.column.0,
            terminal_bounds,
            cursor,
            input_overlay,
            search_spans,
            placed_images,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint_state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let terminal_theme = cx.theme().terminal;
        let TerminalPrepaintState {
            row_caches,
            cell_width,
            line_height,
            cursor_col,
            terminal_bounds,
            cursor,
            input_overlay,
            search_spans,
            placed_images,
            ..
        } = prepaint_state;

        let origin = terminal_bounds.bounds.origin;
        window.paint_quad(fill(bounds, terminal_theme.background));

        // Kitty 图像分两层绘制：`z < 0` 在文字之下、`z >= 0` 在文字之上。
        // 先画下层（此时文字还没上屏），待文字画完后再画上层。
        //
        // 纹理缓存从 view 里 `take` 出来、绘制完归还（与行缓存同一套做法）：
        // `paint` 需要在遍历中同时持有缓存可变借用与 view 的读取，两者不能
        // 共存于一次 `view.update`。
        let (mut kitty_cache, last_prune_generation) = self.view.update(cx, |view, _cx| {
            (
                std::mem::take(&mut view.kitty_image_cache),
                view.kitty_prune_generation,
            )
        });
        let kitty_generation = self.terminal.read(cx).kitty_generation();
        let kitty_storage = self.terminal.read(cx).images_handle();
        crate::terminal::kitty::render::paint_images(
            placed_images,
            true,
            &kitty_storage,
            &mut kitty_cache,
            window,
            cx,
        );

        // O2：paint 直接按行序遍历行缓存，替代先前的 flattened 拷贝。
        // 同一行内绘制顺序保持 Zed 终端的层级：背景 span → 选区 → 文本
        // runs → 盒字符；跨行按行序输出（各行区域不重叠，先后无视觉差异）。
        //
        // Exception: when the Block cursor overlays a box-drawing cell we skip
        // the foreground-colored paint here and let `CursorLayout::paint`
        // redraw that single cell inverted. It is loop-invariant.
        let skip_cursor_box = cursor
            .as_ref()
            .map(|c| c.block_box_drawing.is_some())
            .unwrap_or(false);
        for (row_idx, row_cache) in row_caches.iter().enumerate() {
            // Cell background spans are painted *under* the selection so that a
            // non-default cell background (colored prompt segments, vim splits,
            // syntax highlighting) can never cover the selection highlight. This
            // mirrors Zed's terminal element ordering: rects -> selection -> text.
            if let Some(backgrounds) = row_cache.background_spans.as_ref() {
                for span in backgrounds {
                    let span_origin = point(
                        origin.x + *cell_width * span.start_col as f32,
                        origin.y + *line_height * row_idx as f32,
                    );
                    window.paint_quad(fill(
                        Bounds::new(
                            span_origin,
                            size(*cell_width * span.width as f32, *line_height),
                        ),
                        span.color,
                    ));
                }
            }

            // 搜索命中高亮：绘制在 cell 背景之上、选区之下（选区拖过命中时
            // 选区视觉优先，符合编辑器惯例）。
            if let Some(row_spans) = search_spans.get(row_idx) {
                for span in row_spans {
                    let span_origin = point(
                        origin.x + *cell_width * span.start_col as f32,
                        origin.y + *line_height * row_idx as f32,
                    );
                    window.paint_quad(fill(
                        Bounds::new(
                            span_origin,
                            size(*cell_width * span.width as f32, *line_height),
                        ),
                        span.color,
                    ));
                }
            }

            if let Some(selections) = row_cache.selection_spans.as_ref() {
                for span in selections {
                    let span_origin = point(
                        origin.x + *cell_width * span.start_col as f32,
                        origin.y + *line_height * row_idx as f32,
                    );
                    window.paint_quad(fill(
                        Bounds::new(
                            span_origin,
                            size(*cell_width * span.width as f32, *line_height),
                        ),
                        terminal_theme.selection_background,
                    ));
                }
            }

            if let Some(layout) = row_cache.layout.as_ref() {
                for batch in &layout.runs {
                    batch.paint(origin, row_idx, *cell_width, *line_height, window, cx);
                }

                // 自绘下划线装饰（SGR 4:3 / 4:4 / 4:5：双下划线、点线、虚线）。
                // GPUI TextRun 仅支持单线 / wavy，这三种在 layout 阶段归集为
                // DecorationSpan，这里以矩形图元绘制。
                for dec in &layout.decorations {
                    paint_decoration(dec, origin, row_idx, *cell_width, *line_height, window);
                }
            }

            // 绘制内置盒子绘制字符 / 块元素。
            // 这些字符在 layout_row 阶段已从文本批次中分离出来，
            // 此处用 GPUI 矩形图元（paint_quad）按单元格坐标精确绘制，
            // 效果等同于 Alacritty 的 builtin_box_drawing = true。
            if let Some(box_cells) = row_cache.box_cells.as_ref() {
                for cell in box_cells {
                    if skip_cursor_box && cell.col == *cursor_col && cell.row == row_idx {
                        continue;
                    }
                    let cell_origin = point(
                        origin.x + *cell_width * cell.col as f32,
                        origin.y + *line_height * row_idx as f32,
                    );
                    box_drawing::paint_box_drawing(
                        cell.c,
                        cell_origin,
                        *cell_width,
                        *line_height,
                        cell.color,
                        window,
                    );
                }
            }
        }

        // 行缓存用完归还给视图（行缓存里 `row` 字段在 O2 遍历下已不参与
        // 定位，见 `BoxDrawingCell` 注释；span 行号取遍历下标）。
        let mut row_caches = std::mem::take(row_caches);
        self.view.update(cx, |view, _cx| {
            view.row_render_cache = std::mem::take(&mut row_caches);
        });

        // 文字已上屏：绘制 z >= 0 的 Kitty 图像（它们在文字之上）。
        crate::terminal::kitty::render::paint_images(
            placed_images,
            false,
            &kitty_storage,
            &mut kitty_cache,
            window,
            cx,
        );

        // 归还纹理缓存：CPU 侧裁剪 + GPU 图集释放，二者缺一不可。
        // `RenderImage` 每次构造拿新的全局图集 id，而 GPUI atlas 不会自动回收——
        // 只 prune HashMap 的话，被替换/删除/换帧的图像会在 Metal 图集里永久残留
        // tile（统一内存上直接计进程内存，持续刷新的页面几分钟即达 GB 级）。
        if kitty_generation != last_prune_generation {
            let guard = crate::terminal::kitty::storage::lock_unpoisoned(&kitty_storage);
            let freed = kitty_cache.prune(&guard);
            if !freed.is_empty() {
                log::debug!(
                    "kitty 图像缓存裁剪 {} 张纹理（存储总量 {} 字节）",
                    freed.len(),
                    guard.total_bytes()
                );
            }
            for texture in freed {
                if let Err(err) = window.drop_image(texture) {
                    log::warn!("释放 Kitty 图像纹理失败: {err}");
                }
            }
        }
        self.view.update(cx, |view, _cx| {
            view.kitty_image_cache = std::mem::take(&mut kitty_cache);
            if kitty_generation != last_prune_generation {
                view.kitty_prune_generation = kitty_generation;
            }
        });

        // 使用prepaint阶段计算的光标布局进行渲染
        // 如果cursor为None（光标形状为Hidden），则不渲染光标
        // 这实现了光标委托：当应用程序自己处理光标时，终端不渲染
        if let Some(cursor_layout) = cursor {
            cursor_layout.paint(window, cx);
        }

        // Render local buffered input at cursor position.
        if let Some(input_overlay) = input_overlay {
            // 先画 IME 背景
            for (idx, row) in input_overlay.rows.iter().enumerate() {
                if let Some((col_lo, col_hi)) = row.ime_col_range {
                    let row_y = input_overlay.origin.y + *line_height * idx as f32;
                    let x0 = input_overlay.origin.x + *cell_width * col_lo as f32;
                    let x1 = input_overlay.origin.x + *cell_width * col_hi as f32;
                    window.paint_quad(fill(
                        Bounds::new(point(x0, row_y), size(x1 - x0, *line_height)),
                        terminal_theme.ime_background,
                    ));
                }
            }

            // 逐行画各段 ShapedLine
            for (idx, row) in input_overlay.rows.iter().enumerate() {
                let row_y = input_overlay.origin.y + *line_height * idx as f32;
                for seg in row.segments.iter() {
                    let seg_origin = point(
                        input_overlay.origin.x + *cell_width * seg.start_col as f32,
                        row_y,
                    );
                    let _ = seg.shaped_line.paint(
                        seg_origin,
                        *line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }

            // 在视觉光标位置画光标（与主光标同一份形状绘制实现；
            // 四边形来自动画器，覆盖层出现 / 消失直接就位）。
            if let Some(quad) = input_overlay.cursor_quad.as_ref() {
                paint_cursor_shape(
                    window,
                    input_overlay.cursor_shape,
                    quad,
                    input_overlay.cursor_thickness,
                    input_overlay.cursor_color,
                );
            }

            // IME 下划线（每行单独一段）
            for (idx, row) in input_overlay.rows.iter().enumerate() {
                if let Some((col_lo, col_hi)) = row.ime_col_range {
                    let row_y = input_overlay.origin.y + *line_height * idx as f32;
                    let x0 = input_overlay.origin.x + *cell_width * col_lo as f32;
                    let x1 = input_overlay.origin.x + *cell_width * col_hi as f32;
                    let underline_y = row_y + *line_height - px(1.0);
                    window.paint_quad(fill(
                        Bounds::new(point(x0, underline_y), size(x1 - x0, px(1.0))),
                        terminal_theme.ime_underline,
                    ));
                }
            }
        }

        // 全局左键释放监听器：修复“视图外释放”泄漏。
        //
        // 元素级 `on_mouse_up` 仅在指针位于元素内时触发——若用户把选区
        // 拖到终端视图外（侧边栏、标签栏、另一个面板）再松开左键，释放
        // 事件不会送达：拖拽状态泄漏，之后的悬停移动会继续扩展选区
        // （幽灵拖拽）；mouse mode 应用的 release 报文也会丢失。
        //
        // 此处用窗口级监听器补上：仅在视图正在选择（`selecting`）时处理。
        // 本元素在外层 div 之后 paint，注册顺序在后，bubble 阶段先于
        // 元素级 handler 触发；处理完置 `selecting = false`，元素级
        // handler 随后因守卫早退，两者不会重复调用 `mouse_up`。
        {
            let view = self.view.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                view.update(cx, |view, cx| {
                    view.finish_mouse_selection(event, cx);
                });
            });
        }

        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_bounds_reserve_padding_on_both_sides() {
        let bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(120.0), px(50.0)));
        let content_bounds = terminal_content_bounds(bounds);

        assert_eq!(content_bounds.origin.x, px(14.0));
        assert_eq!(content_bounds.origin.y, px(20.0));
        assert_eq!(content_bounds.size.width, px(112.0));
        assert_eq!(content_bounds.size.height, px(50.0));
    }

    #[test]
    fn cursor_thickness_matches_alacritty_scaling() {
        assert_eq!(cursor_thickness(px(9.0)), px(1.0));
        assert_eq!(cursor_thickness(px(20.0)), px(3.0));
    }

    #[test]
    fn run_paint_origin_uses_passed_row_not_cached_row() {
        let origin = point(px(10.0), px(20.0));
        let pos = run_paint_origin(origin, 2, 7, px(8.0), px(16.0));
        assert_eq!(pos.x, px(10.0 + 2.0 * 8.0));
        assert_eq!(pos.y, px(20.0 + 7.0 * 16.0));
        // 同一个 run 换一个行下标，y 必须跟着变；这是滚动复用行缓存时
        // 「提示符出现两次」的直接回归点。
        let moved = run_paint_origin(origin, 2, 3, px(8.0), px(16.0));
        assert_ne!(pos.y, moved.y);
        assert_eq!(moved.y, px(20.0 + 3.0 * 16.0));
    }

    #[test]
    fn rotate_row_caches_moves_content_and_retargets_spans() {
        let mut caches = vec![
            RowRenderCache {
                fingerprint: Some(1),
                background_spans: Some(vec![BackgroundSpan {
                    start_col: 0,
                    row: 0,
                    width: 1,
                    color: gpui::black(),
                }]),
                ..Default::default()
            },
            RowRenderCache {
                fingerprint: Some(2),
                ..Default::default()
            },
            RowRenderCache {
                fingerprint: Some(3),
                ..Default::default()
            },
        ];
        // 上滚一行：新 0/1 行来自旧 1/2 行，新 2 行是新内容。
        caches = rotate_row_caches(caches, 1, 3);
        assert_eq!(caches[0].fingerprint, Some(2));
        assert_eq!(caches[1].fingerprint, Some(3));
        assert_eq!(caches[2].fingerprint, None);

        // 下滚一行：新 0 行是新内容，新 1/2 行来自旧 0/1 行。
        caches = rotate_row_caches(caches, -1, 3);
        assert_eq!(caches[0].fingerprint, None);
        assert_eq!(caches[1].fingerprint, Some(2));
        assert_eq!(caches[2].fingerprint, Some(3));

        // 平移后必须把 span 的绝对行号重标到新下标。
        let mut cache = RowRenderCache {
            background_spans: Some(vec![BackgroundSpan {
                start_col: 0,
                row: 0,
                width: 1,
                color: gpui::black(),
            }]),
            ..Default::default()
        };
        retarget_row_cache(&mut cache, 7);
        assert_eq!(cache.background_spans.unwrap()[0].row, 7);
    }

    fn test_color(l: f32) -> Hsla {
        Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        }
    }

    #[test]
    fn merge_background_spans_combines_adjacent_same_color() {
        let color = test_color(0.4);
        let mut spans = vec![
            BackgroundSpan {
                start_col: 4,
                row: 0,
                width: 2,
                color,
            },
            BackgroundSpan {
                start_col: 0,
                row: 0,
                width: 4,
                color,
            },
        ];

        TerminalElement::merge_background_spans(&mut spans);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start_col, 0);
        assert_eq!(spans[0].width, 6);
        assert_eq!(spans[0].color, color);
    }

    #[test]
    fn merge_background_spans_keeps_gaps_between_same_color_spans() {
        let color = test_color(0.4);
        let mut spans = vec![
            BackgroundSpan {
                start_col: 0,
                row: 0,
                width: 2,
                color,
            },
            BackgroundSpan {
                start_col: 4,
                row: 0,
                width: 2,
                color,
            },
        ];

        TerminalElement::merge_background_spans(&mut spans);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].start_col, 0);
        assert_eq!(spans[0].width, 2);
        assert_eq!(spans[1].start_col, 4);
        assert_eq!(spans[1].width, 2);
    }

    // ── O1 选区快速路径平价测试 ─────────────────────────────────────────

    /// 确定性 xorshift64* PRNG（避免为测试引入 rand 依赖）。
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// O1 核心平价：随机网格 × 随机选区（含 block 与非 block、单行与跨行、
    /// 越界列），算术区间求交的输出必须与逐格 `contains` 扫描完全一致。
    #[test]
    fn selection_spans_parity_fast_path_matches_per_cell_scan() {
        use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
        use alacritty_terminal::selection::SelectionRange;

        const ROWS_PER_CASE: i32 = 8;
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);

        for case in 0..600u64 {
            let cols = 1 + (rng.below(80) as usize).max(1);

            // 网格行 line ∈ [-4, 4]（覆盖 scrollback 负坐标）。
            let base_line = -(rng.below(5) as i32);
            let rows: Vec<Vec<IndexedCell>> = (0..ROWS_PER_CASE)
                .map(|i| {
                    let line = base_line + i;
                    (0..cols)
                        .map(|col| IndexedCell {
                            point: AlacPoint::new(Line(line), Column(col)),
                            cell: crate::terminal::terminal::PackedCell::from(
                                &alacritty_terminal::term::cell::Cell::default(),
                            ),
                            zerowidth: None,
                            hyperlink: None,
                        })
                        .collect()
                })
                .collect();

            // 随机选区端点（网格坐标，先归一化为 start <= end）。
            let start = AlacPoint::new(
                Line(base_line + rng.below(ROWS_PER_CASE as u64) as i32),
                Column(rng.below(cols as u64 * 2) as usize), // 允许越界列
            );
            let end = AlacPoint::new(
                Line(base_line + rng.below(ROWS_PER_CASE as u64) as i32),
                Column(rng.below(cols as u64 * 2) as usize),
            );
            let (start, end) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            let is_block = rng.below(2) == 0;

            let range = SelectionRange::new(start, end, is_block);
            let sel = Some(range);

            for (row_idx, row_cells) in rows.iter().enumerate() {
                let fast = selection_spans_for_row(row_idx, row_cells, &sel);
                let reference = selection_spans_for_row_reference(row_idx, row_cells, &sel);
                assert_eq!(
                    fast, reference,
                    "case {case} row {row_idx} (cols={cols}, block={is_block}) 不平价: fast={fast:?} ref={reference:?}"
                );

                // 无选区时两者都必须为空。
                let fast_none = selection_spans_for_row(row_idx, row_cells, &None);
                assert!(fast_none.is_empty());
            }
        }
    }

    #[test]
    fn split_cached_background_spans_removes_dirty_middle() {
        let color = test_color(0.4);
        let cached = vec![BackgroundSpan {
            start_col: 0,
            row: 0,
            width: 6,
            color,
        }];

        let updated = TerminalElement::split_cached_background_spans(&cached, (2, 3));

        assert_eq!(updated.len(), 2);
        assert_eq!(updated[0].start_col, 0);
        assert_eq!(updated[0].width, 2);
        assert_eq!(updated[1].start_col, 4);
        assert_eq!(updated[1].width, 2);
        assert!(updated.iter().all(|span| span.color == color));
    }

    // ── 光标动画接线（端到端） ────────────────────────────────────────────

    /// 通过真实的 `TerminalElement::prepaint` 驱动，验证：
    /// 1. 首帧光标直接就位（不从中途飞入）；
    /// 2. 同行一格移动产生动画，本帧矩形落在起止之间；
    /// 3. 动画收敛后精确落在目标格。
    #[gpui::test]
    fn cursor_animation_through_element_prepaint(cx: &mut gpui::TestAppContext) {
        use crate::terminal::terminal::Terminal;
        use crate::terminal_view::TerminalView;
        use gpui::AppContext as _;
        use std::time::Duration;

        struct Host {
            view: gpui::Entity<TerminalView>,
            terminal: gpui::Entity<Terminal>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|_window, cx| {
            let terminal = Terminal::new_headless(cx, 1_000);
            terminal.update(cx, |terminal, cx| {
                terminal.handle_remote_data(b"abc", cx);
            });
            let view = cx.new(|cx| TerminalView::build_headless(cx, terminal.clone()));
            Host { view, terminal }
        });

        let (view, terminal) = host.read_with(cx, |h, _| (h.view.clone(), h.terminal.clone()));
        let focus_handle = view.read_with(cx, |v, _| v.focus_handle_for_test());
        let bounds = size(px(800.0), px(600.0));

        let draw_once = |cx: &mut gpui::VisualTestContext| {
            // 推进一步模拟时钟：`prepaint` 用 `background_executor().now()`
            // 取帧间隔，GPUI 测试时钟不推进的话弹簧会一直看到 dt = 0。
            cx.executor().advance_clock(Duration::from_millis(16));
            let terminal = terminal.clone();
            let focus_handle = focus_handle.clone();
            let view = view.clone();
            cx.draw(point(px(0.0), px(0.0)), bounds, move |_, _| {
                TerminalElement::new(terminal, focus_handle, true, 1.0, view)
            })
            .1
        };

        // 首帧：光标直接就位，无动画。
        let first = draw_once(cx);
        let cursor = first.cursor.as_ref().expect("首帧应绘制光标");
        let origin = cursor.quad.corners[0];
        assert!(!view.read_with(cx, |v, _| v.cursor_animator.is_animating()));

        // 光标右移一格（打字）：产生动画，矩形整体平滑右移。
        terminal.update(cx, |terminal, cx| {
            terminal.handle_remote_data(b"\x1b[C", cx);
        });
        let moved = draw_once(cx);
        let moved_cursor = moved.cursor.as_ref().expect("移动后应绘制光标");
        assert!(
            view.read_with(cx, |v, _| v.cursor_animator.is_animating()),
            "一格水平移动应产生动画"
        );
        let moving = moved_cursor.quad.corners;
        assert!(
            moving[0].0 > origin.0,
            "动画中四边形应已离开起点: {:?} -> {:?}",
            origin.0,
            moving[0].0
        );
        // 与 neovide 一致：≤2 格的「打字跳」四角时长相同 → 保持规整矩形
        // （形状不变、整体平移），宽度恒为一个 cell。
        let width = moving[1].0 - moving[0].0;
        let cell_width = cursor.quad.corners[1].0 - cursor.quad.corners[0].0;
        assert!(
            (width - cell_width).abs() < 0.5,
            "打字跳不应改变光标宽度: {:?} (cell={cell_width})",
            moving
        );

        // 反复重绘直至收敛：最终精确落在目标格且恢复规整矩形。
        let mut settled = moved_cursor.quad.corners;
        for _ in 0..120 {
            if !view.read_with(cx, |v, _| v.cursor_animator.is_animating()) {
                break;
            }
            let state = draw_once(cx);
            settled = state
                .cursor
                .as_ref()
                .expect("收敛帧应绘制光标")
                .quad
                .corners;
        }
        assert!(
            !view.read_with(cx, |v, _| v.cursor_animator.is_animating()),
            "动画应在合理帧数内收敛"
        );
        // 目标格 x = 起点 + 1 个 cell 宽（行内位移，y 不变）。
        let cell = settled[1].0 - settled[0].0;
        assert!(
            (settled[0].0 - origin.0 - cell).abs() < 0.5,
            "应收敛到右移一格: origin={:?} settled={:?} cell={:?}",
            origin.0,
            settled[0].0,
            cell
        );
        assert!(
            (settled[0].1 - origin.1).abs() < 0.01,
            "行内移动不应改变 y: {:?} -> {:?}",
            origin.1,
            settled[0].1
        );
    }

    /// 端到端复现用户场景：**停顿后打字时光标应滑动**，而不是瞬移。
    ///
    /// 终端只在内容变化时重绘，两次重绘之间可能隔着任意长的空闲。若把空闲
    /// 时长算作已流逝的动画时间，首帧就会把 40ms 的短动画一次性播完。
    #[gpui::test]
    fn typing_after_idle_glides_through_element_prepaint(cx: &mut gpui::TestAppContext) {
        use crate::terminal::terminal::Terminal;
        use crate::terminal_view::TerminalView;
        use gpui::AppContext as _;
        use std::time::Duration;

        struct Host {
            view: gpui::Entity<TerminalView>,
            terminal: gpui::Entity<Terminal>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|_window, cx| {
            let terminal = Terminal::new_headless(cx, 1_000);
            let view = cx.new(|cx| TerminalView::build_headless(cx, terminal.clone()));
            Host { view, terminal }
        });

        let (view, terminal) = host.read_with(cx, |h, _| (h.view.clone(), h.terminal.clone()));
        let focus_handle = view.read_with(cx, |v, _| v.focus_handle_for_test());
        let bounds = size(px(800.0), px(600.0));

        // `step_clock` 控制本帧推进多少模拟时间，用于复现「长时间空闲」。
        let draw_after = |cx: &mut gpui::VisualTestContext, step_clock: Duration| {
            cx.executor().advance_clock(step_clock);
            let terminal = terminal.clone();
            let focus_handle = focus_handle.clone();
            let view = view.clone();
            cx.draw(point(px(0.0), px(0.0)), bounds, move |_, _| {
                TerminalElement::new(terminal, focus_handle, true, 1.0, view)
            })
            .1
        };

        // 首帧就位。
        let first = draw_after(cx, Duration::from_millis(16));
        let origin = first.cursor.as_ref().expect("首帧应绘制光标").quad.corners[0];

        // 用户停顿 3 秒后敲第一个字符。
        terminal.update(cx, |terminal, cx| {
            terminal.handle_remote_data(b"a", cx);
        });
        let typed = draw_after(cx, Duration::from_secs(3));
        let cursor = typed.cursor.as_ref().expect("打字后应绘制光标");
        let x = cursor.quad.corners[0].0;

        assert!(
            view.read_with(cx, |v, _| v.cursor_animator.is_animating()),
            "停顿后打字应起动画"
        );
        // 光标从第 0 列滑向第 1 列。起始帧应落在**前半程**——若把 3 秒空闲
        // 算作已流逝的动画时间，首帧就会推进到 ~96%，落在末段甚至到位。
        let cell_width = origin_width(&cursor.quad);
        let progress = (x - origin.0) / cell_width;
        assert!(
            (0.2..0.8).contains(&progress),
            "首帧进度应在前半程（滑动而非瞬移）: origin={} x={} cell={} progress={:.0}%",
            origin.0,
            x,
            cell_width,
            progress * 100.0
        );
    }

    /// 帧驱动回路：动画进行中必须**自己**续订下一帧（`on_next_frame` →
    /// `notify`），否则弹簧只走一帧就停住。
    ///
    /// 这条覆盖「驱动」而不是「插值」——弹簧算得再对，没有下一帧也不会动。
    #[gpui::test]
    fn animation_requests_follow_up_frames(cx: &mut gpui::TestAppContext) {
        use crate::terminal::terminal::Terminal;
        use crate::terminal_view::TerminalView;
        use gpui::AppContext as _;
        use std::time::Duration;

        struct Host {
            view: gpui::Entity<TerminalView>,
            terminal: gpui::Entity<Terminal>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|_window, cx| {
            let terminal = Terminal::new_headless(cx, 1_000);
            let view = cx.new(|cx| TerminalView::build_headless(cx, terminal.clone()));
            Host { view, terminal }
        });

        let (view, terminal) = host.read_with(cx, |h, _| (h.view.clone(), h.terminal.clone()));
        let focus_handle = view.read_with(cx, |v, _| v.focus_handle_for_test());
        let bounds = size(px(800.0), px(600.0));

        let draw = |cx: &mut gpui::VisualTestContext, step: Duration| {
            cx.executor().advance_clock(step);
            let terminal = terminal.clone();
            let focus_handle = focus_handle.clone();
            let view = view.clone();
            cx.draw(point(px(0.0), px(0.0)), bounds, move |_, _| {
                TerminalElement::new(terminal, focus_handle, true, 1.0, view)
            })
            .1
        };

        // 首帧就位，此时不应有待处理帧。
        draw(cx, Duration::from_millis(16));
        let pending_idle = cx.update(|window, app| window.simulate_next_frame(app));
        assert_eq!(pending_idle, 0, "静止时不应请求下一帧");

        // 打字起动画：必须留下一个待处理的 next-frame 回调。
        terminal.update(cx, |terminal, cx| {
            terminal.handle_remote_data(b"a", cx);
        });
        draw(cx, Duration::from_millis(16));
        assert!(
            view.read_with(cx, |v, _| v.cursor_animator.is_animating()),
            "应起动画"
        );
        let pending = cx.update(|window, app| window.simulate_next_frame(app));
        assert!(pending > 0, "动画中必须请求下一帧（否则只走一帧就停）");

        // 一路推进到收敛。
        for _ in 0..200 {
            if !view.read_with(cx, |v, _| v.cursor_animator.is_animating()) {
                break;
            }
            draw(cx, Duration::from_millis(16));
        }
        assert!(
            !view.read_with(cx, |v, _| v.cursor_animator.is_animating()),
            "动画应收敛"
        );

        // 排空动画期间累积的待处理帧（生产环境由平台帧循环每帧消费，
        // 测试里需要手动 `simulate_next_frame`）。
        while cx.update(|window, app| window.simulate_next_frame(app)) > 0 {}

        // 收敛后再画一帧：不应再请求新帧（静止即停，不空转）。
        draw(cx, Duration::from_millis(16));
        let pending_after = cx.update(|window, app| window.simulate_next_frame(app));
        assert_eq!(pending_after, 0, "收敛后不应再请求帧（避免空转）");
    }

    /// 端到端：形状切换（Block → Bar / Underline）时，四角从旧形状平滑
    /// 过渡到新形状，且最终几何与静止布局一致。
    #[gpui::test]
    fn shape_switch_morphs_through_element_prepaint(cx: &mut gpui::TestAppContext) {
        use crate::terminal::terminal::Terminal;
        use crate::terminal_view::TerminalView;
        use gpui::AppContext as _;
        use std::time::Duration;

        struct Host {
            view: gpui::Entity<TerminalView>,
            terminal: gpui::Entity<Terminal>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|_window, cx| {
            let terminal = Terminal::new_headless(cx, 1_000);
            let view = cx.new(|cx| TerminalView::build_headless(cx, terminal.clone()));
            Host { view, terminal }
        });

        let (view, terminal) = host.read_with(cx, |h, _| (h.view.clone(), h.terminal.clone()));
        let focus_handle = view.read_with(cx, |v, _| v.focus_handle_for_test());
        let bounds = size(px(800.0), px(600.0));

        let draw = |cx: &mut gpui::VisualTestContext, step: Duration| {
            cx.executor().advance_clock(step);
            let terminal = terminal.clone();
            let focus_handle = focus_handle.clone();
            let view = view.clone();
            cx.draw(point(px(0.0), px(0.0)), bounds, move |_, _| {
                TerminalElement::new(terminal, focus_handle, true, 1.0, view)
            })
            .1
        };

        // Block 就位。
        let block_state = draw(cx, Duration::from_millis(16));
        let block = block_state
            .cursor
            .as_ref()
            .expect("应绘制光标")
            .quad
            .corners;
        let block_w = block[1].0 - block[0].0;
        assert!(block_w > 0.0);
        // 静止的 beam 宽度：`cursor_thickness`（cell 宽的 15%，至少 1px）。
        let beam_target = (block_w * 0.15).round().max(1.0);

        // DECSCUSR 切到 beam（CSI 5 SP q）：应产生 morph，宽度收窄。
        terminal.update(cx, |terminal, cx| {
            terminal.handle_remote_data(b"\x1b[5 q", cx);
        });
        let morph = draw(cx, Duration::from_millis(16));
        let morph_quad = morph
            .cursor
            .as_ref()
            .expect("morph 中应绘制光标")
            .quad
            .corners;
        let morph_w = morph_quad[1].0 - morph_quad[0].0;
        assert!(
            morph_w < block_w,
            "beam 应比 block 窄: {morph_w} vs {block_w}"
        );
        assert!(
            view.read_with(cx, |v, _| v.cursor_animator.is_animating()),
            "形状切换应起 morph 动画: block={block:?} morph={morph_quad:?}"
        );

        // 收敛后宽度应等于静止 beam 宽度（约 cell 宽的 15%，至少 1px）。
        for _ in 0..200 {
            if !view.read_with(cx, |v, _| v.cursor_animator.is_animating()) {
                break;
            }
            draw(cx, Duration::from_millis(16));
        }
        let settled_state = draw(cx, Duration::from_millis(16));
        let settled = settled_state
            .cursor
            .as_ref()
            .expect("收敛后应绘制光标")
            .quad
            .corners;
        let settled_w = settled[1].0 - settled[0].0;
        assert!(
            (settled_w - beam_target).abs() < 0.6,
            "beam 宽度应收窄到 {beam_target}px，实际 {settled_w}"
        );
        // 高度不变（beam 占满行高）。
        let settled_h = settled[2].1 - settled[1].1;
        assert!(
            (settled_h - (block[2].1 - block[1].1)).abs() < 0.5,
            "beam 高度应与 block 相同"
        );
    }

    /// 端到端：宽字符 Block 的宽度翻倍，但切到 Bar 后粗细恢复单格比例。
    #[gpui::test]
    fn wide_char_bar_keeps_thin_through_element_prepaint(cx: &mut gpui::TestAppContext) {
        use crate::terminal::terminal::Terminal;
        use crate::terminal_view::TerminalView;
        use gpui::AppContext as _;
        use std::time::Duration;

        struct Host {
            view: gpui::Entity<TerminalView>,
            terminal: gpui::Entity<Terminal>,
        }
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let (host, cx) = cx.add_window_view(|_window, cx| {
            let terminal = Terminal::new_headless(cx, 1_000);
            // "a中" 然后回到第 2 列（1-based）——即宽字符所在格，让光标的
            // 单元格带 WIDE_CHAR 标志。
            terminal.update(cx, |terminal, cx| {
                terminal.handle_remote_data("a中\x1b[2G".as_bytes(), cx);
            });
            let view = cx.new(|cx| TerminalView::build_headless(cx, terminal.clone()));
            Host { view, terminal }
        });

        let (view, terminal) = host.read_with(cx, |h, _| (h.view.clone(), h.terminal.clone()));
        let focus_handle = view.read_with(cx, |v, _| v.focus_handle_for_test());
        let bounds = size(px(800.0), px(600.0));

        let draw = |cx: &mut gpui::VisualTestContext, step: Duration| {
            cx.executor().advance_clock(step);
            let terminal = terminal.clone();
            let focus_handle = focus_handle.clone();
            let view = view.clone();
            cx.draw(point(px(0.0), px(0.0)), bounds, move |_, _| {
                TerminalElement::new(terminal, focus_handle, true, 1.0, view)
            })
            .1
        };

        // 宽字符 Block：宽度应为 2 个 cell。
        let block_state = draw(cx, Duration::from_millis(16));
        let block = block_state
            .cursor
            .as_ref()
            .expect("应绘制光标")
            .quad
            .corners;
        let block_w = block[1].0 - block[0].0;

        // 先切到 beam 拿到「单格粗细」基准，再切回 block 比较——两者用
        // 同一个 `cell_width`，避免测试对绝对像素值的假设。
        terminal.update(cx, |terminal, cx| {
            terminal.handle_remote_data(b"\x1b[5 q", cx);
        });
        // 先画一帧让动画器看到新形状（起 morph），再推进到收敛。
        draw(cx, Duration::from_millis(16));
        for _ in 0..200 {
            if !view.read_with(cx, |v, _| v.cursor_animator.is_animating()) {
                break;
            }
            draw(cx, Duration::from_millis(16));
        }
        let beam_state = draw(cx, Duration::from_millis(16));
        let beam = beam_state.cursor.as_ref().expect("应绘制光标").quad.corners;
        let beam_w = beam[1].0 - beam[0].0;

        // 宽字符 Block 应是单格粗细基准的 2 倍左右（宽度翻倍）；
        // beam 则保持单格粗细，不应跟着翻倍。
        let single_cell_thickness = (block_w / 2.0 * 0.15).round().max(1.0);
        assert!(
            (beam_w - single_cell_thickness).abs() < 0.6,
            "宽字符下 beam 粗细应为单格比例（{single_cell_thickness}px），\
             实际 {beam_w}px（block={block_w}px）; block={block:?} beam={beam:?}"
        );
        // 高度都占满行高。
        let block_h = block[2].1 - block[1].1;
        let beam_h = beam[2].1 - beam[1].1;
        assert!(
            (block_h - beam_h).abs() < 0.5,
            "beam 高度应与 block 相同: {beam_h} vs {block_h}"
        );
    }
}

/// 光标四边形的静止宽度（右缘 − 左缘）。
#[cfg(test)]
fn origin_width(quad: &crate::components::cursor_animation::CursorQuad) -> f32 {
    quad.corners[1].0 - quad.corners[0].0
}
