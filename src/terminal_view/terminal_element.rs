//! Terminal element for rendering

use crate::sidebar_view::font_settings::{FontState, platform_default_fallbacks};
use crate::terminal::mappings::colors::TerminalPalette;
use crate::terminal::terminal::{IndexedCell, Terminal, TerminalBounds};
use crate::terminal_view::box_drawing;
use crate::terminal_view::terminal_view::{HoveredLink, TerminalView};
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
        window.paint_quad(fill(
            Bounds::new(point(x, y), size(w, px(1.0))),
            dec.color,
        ));
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
        let (left, right) = if range.is_block
            || (line == range.start.line && line == range.end.line)
        {
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
        vec![BackgroundSpan::for_selection(left, row_idx, right - left + 1)]
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
/// 本结构只保留「缓存是否仍有效」的判定信号：排版度量签名与悬停链接状态。
#[derive(Clone, Default)]
pub struct FlattenedRenderCache {
    pub metrics: Option<RenderMetricsSignature>,
    /// Tracks the hovered link state that produced this cache so we can
    /// detect changes and rebuild when the user moves between URLs.
    pub hovered_link: Option<HoveredLink>,
}

/// CursorLayout - simplified implementation inspired by Zed's CursorLayout
/// Computed during prepaint phase, rendered during paint phase
pub struct CursorLayout {
    origin: Point<Pixels>,
    block_width: Pixels,
    line_height: Pixels,
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum CursorShape {
    Block,
    Hollow,
    Bar,
    Underline,
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
    cursor_color: Hsla,
    cursor_thickness: Pixels,
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

impl CursorLayout {
    fn paint(&self, window: &mut Window, cx: &mut gpui::App) {
        match self.shape {
            CursorShape::Bar => {
                window.paint_quad(fill(
                    Bounds {
                        origin: self.origin,
                        size: size(self.thickness, self.line_height),
                    },
                    self.color,
                ));
            }
            CursorShape::Block => {
                window.paint_quad(fill(
                    Bounds {
                        origin: self.origin,
                        size: size(self.block_width, self.line_height),
                    },
                    self.color,
                ));
            }
            CursorShape::Underline => {
                window.paint_quad(fill(
                    Bounds {
                        origin: self.origin + point(px(0.0), self.line_height - self.thickness),
                        size: size(self.block_width, self.thickness),
                    },
                    self.color,
                ));
            }
            CursorShape::Hollow => {
                let vertical_height = if self.line_height > self.thickness * 2.0 {
                    self.line_height - self.thickness * 2.0
                } else {
                    Pixels::ZERO
                };

                let quads = [
                    Bounds {
                        origin: self.origin,
                        size: size(self.block_width, self.thickness),
                    },
                    Bounds {
                        origin: self.origin + point(px(0.0), self.line_height - self.thickness),
                        size: size(self.block_width, self.thickness),
                    },
                    Bounds {
                        origin: self.origin + point(px(0.0), self.thickness),
                        size: size(self.thickness, vertical_height),
                    },
                    Bounds {
                        origin: self.origin
                            + point(self.block_width - self.thickness, self.thickness),
                        size: size(self.thickness, vertical_height),
                    },
                ];

                for quad in quads {
                    window.paint_quad(fill(quad, self.color));
                }
            }
        }

        // Paint character on block cursor with inverted colors
        if let Some(block_text) = &self.block_text {
            let _ = block_text.paint(
                self.origin,
                self.line_height,
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
                self.origin,
                self.block_width,
                self.line_height,
                color,
                window,
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct BatchedTextRun {
    pub start_col: usize,
    pub start_row: usize,
    pub cell_count: usize,
    pub shaped_line: gpui::ShapedLine,
}

impl BatchedTextRun {
    fn new(row: usize, col: usize, cell_count: usize, shaped_line: gpui::ShapedLine) -> Self {
        BatchedTextRun {
            start_col: col,
            start_row: row,
            cell_count,
            shaped_line,
        }
    }
}

impl BatchedTextRun {
    pub fn paint(
        &self,
        origin: Point<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let pos = Point::new(
            origin.x + self.start_col as f32 * cell_width,
            origin.y + self.start_row as f32 * line_height,
        );

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
fn underline_kind_from_flags(
    flags: &alacritty_terminal::term::cell::Flags,
) -> UnderlineKind {
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
    cursor_visible: bool,
    view: Entity<TerminalView>,
}

impl TerminalElement {
    fn flush_text_batch(
        row_idx: usize,
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
            batched_runs.push(BatchedTextRun::new(
                row_idx,
                start_col,
                cell_count,
                shaped_line,
            ));
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
                color: Some(
                    theme.named_color(alacritty_terminal::vte::ansi::NamedColor::Blue),
                ),
                thickness: px(1.0),
                wavy: false,
            }),
            // Double/Dotted/Dashed 走自绘；None 无装饰。
            UnderlineKind::Double | UnderlineKind::Dotted | UnderlineKind::Dashed
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
            row_idx,
            row_cells.iter(),
            None,
            window,
            text_style,
            font_size,
            cell_width,
            terminal_theme,
            palette,
        );
        let full_boxes =
            Self::box_cells_for_row(row_idx, row_cells, terminal_theme, palette);
        let full_backgrounds =
            Self::background_spans_for_row(row_idx, row_cells, terminal_theme, palette);
        let full_selection_spans =
            selection_spans_for_row(row_idx, row_cells, selection_range);

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
        cursor_visible: bool,
        view: Entity<TerminalView>,
    ) -> Self {
        Self {
            terminal,
            focus,
            focused,
            cursor_visible,
            view,
        }
    }

    /// Compute the layout for a single visible row.
    /// `row_idx` is 0-based from the top of the visible viewport.
    fn layout_row<'a>(
        row_idx: usize,
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

            if ch == '\0' || ch == ' ' || box_drawing::is_box_drawing_char(ch) {
                Self::flush_text_batch(
                    row_idx,
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
                    row_idx,
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
            row_idx,
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
        Self::background_spans_for_range(
            row_idx, cells, (0, usize::MAX), terminal_theme, palette,
        )
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

            if let Some(last) = merged.last_mut() {
                if last.row == span.row
                    && last.color == span.color
                    && last.start_col + last.width == span.start_col
                {
                    last.width += span.width;
                    continue;
                }
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
            if let Some(box_cell) =
                Self::box_cell_for_cell(row_idx, cell, terminal_theme, palette)
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
            if let Some(box_cell) =
                Self::box_cell_for_cell(row_idx, cell, terminal_theme, palette)
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
        row_idx: usize,
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
        let (mut rebuild_left, mut rebuild_right) = range;

        // Expand the rebuild window to cover the whole contiguous cached run segment
        // around the dirty range. This prevents incremental updates from leaving
        // adjacent compatible runs split across the cache boundary.
        loop {
            let mut changed = false;
            for run in cached_runs {
                let run_start = run.start_col;
                let run_end = run.start_col + run.cell_count.saturating_sub(1);

                if run_end.saturating_add(1) < rebuild_left
                    || run_start > rebuild_right.saturating_add(1)
                {
                    continue;
                }

                let next_left = rebuild_left.min(run_start);
                let next_right = rebuild_right.max(run_end);
                if next_left != rebuild_left || next_right != rebuild_right {
                    rebuild_left = next_left;
                    rebuild_right = next_right;
                    changed = true;
                }
            }

            if !changed {
                break;
            }
        }

        let mut merged_runs: Vec<BatchedTextRun> = Vec::new();

        for run in cached_runs {
            let run_start = run.start_col;
            let run_end = run.start_col + run.cell_count.saturating_sub(1);

            if run_end < rebuild_left || run_start > rebuild_right {
                merged_runs.push(run.clone());
            }
        }

        let dirty_cells = cells
            .iter()
            .filter(|cell| (rebuild_left..=rebuild_right).contains(&cell.point.column.0));
        let dirty_entry = Self::layout_row(
            row_idx,
            dirty_cells,
            hovered_url_cols,
            window,
            text_style,
            font_size,
            cell_width,
            terminal_theme,
            palette,
        );

        merged_runs.extend(dirty_entry.runs);
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
    #[allow(clippy::too_many_arguments)]
    fn shape_overlay_layout(
        &self,
        builtin_text: &str,
        ime_text: &str,
        builtin_cursor: usize,
        cursor_display_col: usize,
        cursor_line_in_viewport: i32,
        terminal_bounds: TerminalBounds,
        cell_width: Pixels,
        line_height: Pixels,
        font_size: Pixels,
        text_style: &TextStyle,
        builtin_color: Hsla,
        ime_color: Hsla,
        cursor_shape: CursorShape,
        cursor_color: Hsla,
        window: &mut Window,
    ) -> Option<LocalOverlayLayout> {
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
            cursor_color,
            cursor_thickness: cursor_thickness(cell_width),
        })
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    /// O2：paint 直接持有行缓存（take/restore），不再经过 4×Arc 的
    /// flattened 拷贝。字段：行缓存、cell 度量、光标位置、光标布局、
    /// 本地输入覆盖层。
    ///
    /// 不变量：prepaint 从 view `take` 出行缓存到 paint 归还期间，view
    /// 上的 `row_render_cache` 为空。gpui 帧周期保证 prepaint×1 →
    /// paint×1 配对执行，paint 体内无提前 return，归还必然到达；若同一
    /// 帧意外出现第二次 prepaint（当前框架不会），会触发该次全量重排
    /// ——行为正确、仅性能回退。
    type PrepaintState = (
        Vec<RowRenderCache>,
        Pixels,
        Pixels,
        usize,
        usize,
        bool,
        TerminalBounds,
        Option<CursorLayout>,
        Option<LocalOverlayLayout>,
        // 末位为搜索命中高亮（视口行映射后的 span，含当前命中配色）
        Vec<BackgroundSpan>,
    );

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
        let active_theme_id = theme.id.clone();
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
        let terminal_bounds = TerminalBounds::new(line_height, cell_width, content_bounds);

        // Sync terminal and handle resize
        let (display_offset, cursor_point, cursor_shape) =
            self.terminal.update(cx, |terminal, cx| {
                terminal.sync(window, cx);

                let current_bounds = terminal.terminal_bounds;
                if current_bounds.num_columns() != terminal_bounds.num_columns()
                    || current_bounds.num_lines() != terminal_bounds.num_lines()
                {
                    terminal.resize(terminal_bounds, cx);
                }
                terminal.set_terminal_bounds(terminal_bounds);

                let content = terminal.last_content();
                (content.display_offset, content.cursor, content.cursor_shape)
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
        let (mut dirty_rows, mut dirty_row_bounds, mouse_mode, selection_range, cursor_cell, palette) = {
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
                match &view.cached_theme_id {
                    Some(theme_id) if active_theme_id == *theme_id => (
                        std::mem::take(&mut view.row_render_cache),
                        view.flattened_render_cache.clone(),
                        selection_changed,
                    ),
                    _ => {
                        // 主题更改清空缓存，全量刷新，避免终端字符渲染颜色与主题不一致
                        view.cached_theme_id = Some(active_theme_id.clone());
                        view.flattened_render_cache = FlattenedRenderCache::default();
                        (
                            Vec::new(),
                            FlattenedRenderCache::default(),
                            selection_changed,
                        )
                    }
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
        let hovered_link = self.view.read(cx).hovered_link.clone();


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

        #[cfg(debug_assertions)]
        let validate_row_cache = std::env::var_os("DEEPTERM_VALIDATE_ROW_CACHE").is_some();

        // `row_caches` 在上面已 resize 到 `viewport_lines`，逐行迭代等价于按下标遍历。
        for (row_idx, row_cache) in row_caches.iter_mut().enumerate() {
            let is_dirty = dirty_rows.get(row_idx).copied().unwrap_or(true);
            let row_cells: &[IndexedCell] = if let Some((start, end)) = row_ranges.get(row_idx) {
                &content_cells[*start..*end]
            } else {
                &[]
            };
            let hovered_url_cols = hovered_link.as_ref().and_then(|link| {
                if link.row == row_idx {
                    Some((link.start_col, link.end_col))
                } else {
                    None
                }
            });

            // Invalidate cached layout when the hovered URL range on this row changes,
            // so that underline styles are recomputed even if terminal content is clean.
            if row_cache.hovered_url_cols != hovered_url_cols {
                row_cache.layout = None;
                row_cache.hovered_url_cols = hovered_url_cols;
            }

            if is_dirty || row_cache.layout.is_none() {
                dirty_count += 1;
                let cached_entry = row_cache.layout.clone();
                let dirty_range = dirty_row_bounds.get(row_idx).copied().flatten();
                let entry = if let (Some(cached_entry), Some(range)) = (cached_entry, dirty_range) {
                    if let Some((rebuilt_left, rebuilt_right, updated_runs, updated_decorations)) =
                        Self::try_update_runs_in_range(
                            row_idx,
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
                        let mut entry = cached_entry;
                        entry.runs = updated_runs;
                        // 装饰与文本 run 必须一起增量刷新：丢弃落在重建区间内的旧 span
                        // （区间已扩展到完整 run 段），再并入新 span 并按列排序。
                        entry.decorations.retain(|span| {
                            span.start_col + span.width <= rebuilt_left
                                || span.start_col > rebuilt_right
                        });
                        entry.decorations.extend(updated_decorations);
                        entry.decorations.sort_by_key(|span| span.start_col);
                        entry
                    } else {
                        Self::layout_row(
                            row_idx,
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
                        row_idx,
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
            } else {
                cache_hit_count += 1;
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
            hovered_link,
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
        let cursor_row = cursor_line_in_viewport.max(0) as usize;

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

        // 光标色走调色板（OSC 12 改色 / 主题切换实时生效）。
        let cursor_color = palette.resolve(
            &alacritty_terminal::vte::ansi::Color::Named(
                alacritty_terminal::vte::ansi::NamedColor::Cursor,
            ),
            &terminal_theme,
        );

        // 计算光标布局 - 如果光标形状为Hidden则跳过（由应用程序自己处理光标渲染）
        // 这是光标委托机制：当应用程序（如Claude Code）设置光标为Hidden时，
        // 终端不渲染光标，让应用程序自己处理
        // IME 输入时也隐藏光标，由 preedit overlay 替代显示
        let cursor = if self.cursor_visible
            && cursor_in_viewport
            && cursor_shape != AlacCursorShape::Hidden
            && !has_local_overlay_input
        {
            let cursor_x = terminal_bounds.bounds.left()
                + px((cursor_display_col as f32 * cell_width.as_f32()).floor());
            let cursor_y = terminal_bounds.bounds.top()
                + px((cursor_line_in_viewport as f32 * line_height.as_f32()).floor());

            let cursor_char = cursor_cell.c;
            let cursor_cells_wide = if cursor_cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR)
            {
                2.0
            } else {
                char_cell_width(cursor_char) as f32
            };
            let cursor_block_width = cell_width * cursor_cells_wide;
            let block_text_color = terminal_theme.background;

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

            // Block cursor draws the glyph on top in the inverted (background)
            // color. For box-drawing chars we bypass the font rasterizer and
            // re-run our rect-based painter so the lines stay aligned.
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
                            Some(cursor_block_width),
                        );
                        (Some(shaped), None)
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };

            Some(CursorLayout {
                origin: point(cursor_x, cursor_y),
                block_width: cursor_block_width,
                line_height,
                thickness: cursor_thickness(cell_width),
                color: cursor_color,
                shape: overlay_cursor_shape,
                block_text,
                block_box_drawing,
            })
        } else {
            None
        };

        // Shape local overlay text for rendering at the shell prompt position.
        let input_overlay: Option<LocalOverlayLayout> = if cursor_in_viewport {
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
                builtin_text,
                ime_text,
                builtin_cursor,
                cursor_display_col,
                cursor_line_in_viewport,
                terminal_bounds,
                cell_width,
                line_height,
                font_size,
                &text_style,
                ui_theme.text,
                terminal_theme.foreground,
                overlay_cursor_shape,
                cursor_color,
                window,
            )
        } else {
            None
        };

        // ── Search highlight spans ─────────────────────────────────────────────────
        // 把 grid 坐标命中映射到当前视口行（每帧重算，滚动自然跟随），
        // 非当前命中用主题 warning 低透明度，当前命中加倍醒目。
        let search_spans: Vec<BackgroundSpan> = {
            let view = self.view.read(cx);
            let search = &view.search;
            if !search.active || search.matches.is_empty() {
                Vec::new()
            } else {
                let match_color = ui_theme.warning.opacity(0.28);
                let active_color = ui_theme.warning.opacity(0.55);
                let mut spans = Vec::with_capacity(search.matches.len());
                for (idx, m) in search.matches.iter().enumerate() {
                    // grid Line → 视口顶行索引：line.0 + display_offset
                    let viewport_row = m.start.line.0 + display_offset as i32;
                    if viewport_row < 0 {
                        continue;
                    }
                    let viewport_row = viewport_row as usize;
                    if viewport_row >= viewport_lines {
                        continue;
                    }
                    let start_col = m.start.column.0;
                    let end_col = m.end.column.0;
                    spans.push(BackgroundSpan {
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
                spans
            }
        };

        (
            row_caches,
            cell_width,
            line_height,
            cursor_point.column.0,
            cursor_row,
            cursor_in_viewport,
            terminal_bounds,
            cursor,
            input_overlay,
            search_spans,
        )
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
        let (
            row_caches,
            cell_width,
            line_height,
            cursor_col,
            _cursor_row,
            _cursor_in_viewport,
            terminal_bounds,
            cursor,
            input_overlay,
            search_spans,
        ) = prepaint_state;

        let origin = terminal_bounds.bounds.origin;
        window.paint_quad(fill(bounds, terminal_theme.background));

        // O2：paint 直接按行序遍历行缓存，替代先前的 flattened 拷贝。
        // 同一行内绘制顺序保持 Zed 终端的层级：背景 span → 选区 → 文本
        // runs → 盒字符；跨行按行序输出（各行区域不重叠，先后无视觉差异）。
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
            for span in search_spans.iter().filter(|span| span.row == row_idx) {
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
                    batch.paint(origin, *cell_width, *line_height, window, cx);
                }

                // 自绘下划线装饰（SGR 4:3 / 4:4 / 4:5：双下划线、点线、虚线）。
                // GPUI TextRun 仅支持单线 / wavy，这三种在 layout 阶段归集为
                // DecorationSpan，这里以矩形图元绘制。
                for dec in &layout.decorations {
                    paint_decoration(
                        dec,
                        origin,
                        row_idx,
                        *cell_width,
                        *line_height,
                        window,
                    );
                }
            }

            // 绘制内置盒子绘制字符 / 块元素。
            // 这些字符在 layout_row 阶段已从文本批次中分离出来，
            // 此处用 GPUI 矩形图元（paint_quad）按单元格坐标精确绘制，
            // 效果等同于 Alacritty 的 builtin_box_drawing = true。
            //
            // Exception: when the Block cursor overlays a box-drawing cell we
            // skip the foreground-colored paint here and let
            // `CursorLayout::paint` redraw that single cell inverted.
            let skip_cursor_box = cursor
                .as_ref()
                .map(|c| c.block_box_drawing.is_some())
                .unwrap_or(false);
            if let Some(box_cells) = row_cache.box_cells.as_ref() {
                for cell in box_cells {
                    if skip_cursor_box
                        && cell.col == *cursor_col
                        && cell.row == row_idx
                    {
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

            // 在视觉光标位置画光标矩形
            if self.cursor_visible {
                let cursor_origin = input_overlay.origin
                    + point(
                        *cell_width * input_overlay.cursor_col as f32,
                        *line_height * input_overlay.cursor_row as f32,
                    );
                let block_width = *cell_width;
                let thickness = input_overlay.cursor_thickness;

                match input_overlay.cursor_shape {
                    CursorShape::Bar => {
                        window.paint_quad(fill(
                            Bounds {
                                origin: cursor_origin,
                                size: size(thickness, *line_height),
                            },
                            input_overlay.cursor_color,
                        ));
                    }
                    CursorShape::Block => {
                        window.paint_quad(fill(
                            Bounds {
                                origin: cursor_origin,
                                size: size(block_width, *line_height),
                            },
                            input_overlay.cursor_color,
                        ));
                    }
                    CursorShape::Underline => {
                        window.paint_quad(fill(
                            Bounds {
                                origin: cursor_origin + point(px(0.0), *line_height - thickness),
                                size: size(block_width, thickness),
                            },
                            input_overlay.cursor_color,
                        ));
                    }
                    CursorShape::Hollow => {
                        let vertical_height = if *line_height > thickness * 2.0 {
                            *line_height - thickness * 2.0
                        } else {
                            Pixels::ZERO
                        };
                        let quads = [
                            Bounds {
                                origin: cursor_origin,
                                size: size(block_width, thickness),
                            },
                            Bounds {
                                origin: cursor_origin + point(px(0.0), *line_height - thickness),
                                size: size(block_width, thickness),
                            },
                            Bounds {
                                origin: cursor_origin + point(px(0.0), thickness),
                                size: size(thickness, vertical_height),
                            },
                            Bounds {
                                origin: cursor_origin + point(block_width - thickness, thickness),
                                size: size(thickness, vertical_height),
                            },
                        ];
                        for quad in quads {
                            window.paint_quad(fill(quad, input_overlay.cursor_color));
                        }
                    }
                }
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
            let (start, end) = if start <= end { (start, end) } else { (end, start) };
            let is_block = rng.below(2) == 0;

            let range = SelectionRange::new(start, end, is_block);
            let sel = Some(range);

            for (row_idx, row_cells) in rows.iter().enumerate() {
                let fast = selection_spans_for_row(row_idx, row_cells, &sel);
                let reference =
                    selection_spans_for_row_reference(row_idx, row_cells, &sel);
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
}
