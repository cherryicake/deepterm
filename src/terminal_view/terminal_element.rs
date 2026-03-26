//! Terminal element for rendering

use crate::terminal::mappings::colors::convert_alacritty_color;
use crate::terminal::terminal::{IndexedCell, Terminal, TerminalBounds};
use crate::terminal_view::box_drawing;
use crate::terminal_view::terminal_view::TerminalView;
use alacritty_terminal::index::Point as AlacPoint;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::vte::ansi::CursorShape as AlacCursorShape;
use gpui::{
    Bounds, Element, ElementId, ElementInputHandler, Entity, FocusHandle, Font, FontFallbacks,
    FontStyle, FontWeight, GlobalElementId, Hsla, IntoElement, LayoutId, Pixels, Point, Style,
    TextRun, TextStyle, WhiteSpace, Window, fill, point, px, rgb, size,
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

#[inline]
fn rendered_cell_colors(
    cell: &alacritty_terminal::term::cell::Cell,
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
fn is_point_selected(point: &AlacPoint, range: &Option<SelectionRange>) -> bool {
    let Some(range) = range else { return false };
    let point_line = point.line.0;
    let point_col = point.column.0;

    let (start, end) = if range.start.line <= range.end.line {
        (range.start, range.end)
    } else {
        (range.end, range.start)
    };

    if point_line < start.line.0 || point_line > end.line.0 {
        return false;
    }

    if start.line == end.line {
        return point_col >= start.column.0 && point_col <= end.column.0;
    }

    match point_line.cmp(&start.line.0) {
        std::cmp::Ordering::Equal => point_col >= start.column.0,
        std::cmp::Ordering::Greater if point_line == end.line.0 => point_col <= end.column.0,
        _ => true,
    }
}

/// Cached per-row layout for one visible row.
/// Stored in `TerminalView::row_render_cache` and reused across frames when the row is clean.
#[derive(Clone)]
pub struct RowLayoutEntry {
    pub runs: Vec<BatchedTextRun>,
}

#[derive(Clone, Default)]
pub struct RowRenderCache {
    pub layout: Option<RowLayoutEntry>,
    pub selection_cols: Option<Vec<usize>>,
    pub box_cells: Option<Vec<BoxDrawingCell>>,
    pub background_cells: Option<Vec<BackgroundCell>>,
}

/// 盒子绘制字符 / 块元素单元格。
///
/// 对于 U+2500–U+259F 范围内的字符，不走字体光栅化流程，
/// 而是在 `paint` 阶段用 GPUI 矩形图元直接绘制，
/// 以保证相邻单元格的线条精确对齐（不受字体字形影响）。
#[derive(Clone, Debug, PartialEq)]
pub struct BoxDrawingCell {
    pub col: usize,
    pub row: usize,
    pub c: char,
    pub color: Hsla, // 该字符的前景色（已处理 INVERSE 反色标志）
}

#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundCell {
    pub col: usize,
    pub row: usize,
    pub color: Hsla,
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
    block_text: Option<gpui::ShapedLine>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CursorShape {
    Block,
    Hollow,
    Bar,
    Underline,
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

pub struct TerminalElement {
    terminal: Entity<Terminal>,
    focus: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    view: Entity<TerminalView>,
}

impl TerminalElement {
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
    fn assert_row_consistency<'a>(
        row_idx: usize,
        row_cells: &'a [IndexedCell],
        row_runs: &[BatchedTextRun],
        row_boxes: &[BoxDrawingCell],
        row_backgrounds: &[BackgroundCell],
        row_selection_cols: &[usize],
        window: &mut Window,
        text_style: &TextStyle,
        font_size: Pixels,
        cell_width: Pixels,
        selection_range: &Option<SelectionRange>,
    ) {
        let full_entry = Self::layout_row(
            row_idx,
            row_cells.iter(),
            window,
            text_style,
            font_size,
            cell_width,
        );
        let full_boxes = Self::box_cells_for_row(row_idx, row_cells);
        let full_backgrounds = Self::background_cells_for_row(row_idx, row_cells);
        let full_selection_cols = Self::selected_cols_for_row(row_cells, selection_range);

        debug_assert_eq!(
            Self::run_signature(row_runs),
            Self::run_signature(&full_entry.runs),
            "row {row_idx} run mismatch"
        );
        debug_assert_eq!(
            row_boxes,
            full_boxes.as_slice(),
            "row {row_idx} box mismatch"
        );
        debug_assert_eq!(
            row_backgrounds,
            full_backgrounds.as_slice(),
            "row {row_idx} background mismatch"
        );
        debug_assert_eq!(
            row_selection_cols,
            full_selection_cols.as_slice(),
            "row {row_idx} selection mismatch"
        );
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
        window: &mut Window,
        text_style: &TextStyle,
        font_size: Pixels,
        cell_width: Pixels,
    ) -> RowLayoutEntry {
        let mut batched_runs: Vec<BatchedTextRun> = Vec::new();
        let mut current_batch: Option<(usize, String, TextRun, usize)> = None;

        for cell in cells {
            let ch = cell.cell.c;

            // Handle INVERSE flag: swap foreground and background colors
            let (fg, bg) = rendered_cell_colors(&cell.cell);

            if ch == '\0' || ch == ' ' {
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
                continue;
            }

            if box_drawing::is_box_drawing_char(ch) {
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
                continue;
            }

            let cell_style = TextRun {
                len: ch.len_utf8(),
                font: Font {
                    weight: if cell
                        .cell
                        .flags
                        .contains(alacritty_terminal::term::cell::Flags::BOLD)
                    {
                        FontWeight::BOLD
                    } else {
                        text_style.font_weight
                    },
                    style: if cell
                        .cell
                        .flags
                        .contains(alacritty_terminal::term::cell::Flags::ITALIC)
                    {
                        FontStyle::Italic
                    } else {
                        FontStyle::Normal
                    },
                    ..text_style.font()
                },
                color: convert_alacritty_color(fg),
                background_color: if bg
                    == &alacritty_terminal::vte::ansi::Color::Named(
                        alacritty_terminal::vte::ansi::NamedColor::Background,
                    ) {
                    None
                } else {
                    Some(convert_alacritty_color(bg))
                },
                underline: if cell
                    .cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::UNDERLINE)
                {
                    Some(gpui::UnderlineStyle {
                        color: Some(convert_alacritty_color(&cell.cell.fg)),
                        thickness: px(1.0),
                        wavy: false,
                    })
                } else {
                    None
                },
                strikethrough: None,
            };

            match current_batch.as_mut() {
                Some((start_col, text, style, cell_count))
                    if style.font == cell_style.font
                        && style.color == cell_style.color
                        && style.background_color == cell_style.background_color
                        && style.underline == cell_style.underline
                        && style.strikethrough == cell_style.strikethrough
                        && *start_col + *cell_count == cell.point.column.0 =>
                {
                    text.push(ch);
                    *cell_count += 1;
                    style.len += ch.len_utf8();
                }
                _ => {
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
                    current_batch = Some((cell.point.column.0, ch.to_string(), cell_style, 1));
                }
            }
        }

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

        RowLayoutEntry { runs: batched_runs }
    }

    fn box_cell_for_cell(row_idx: usize, cell: &IndexedCell) -> Option<BoxDrawingCell> {
        let ch = cell.cell.c;
        if !box_drawing::is_box_drawing_char(ch) {
            return None;
        }
        let (fg, _) = rendered_cell_colors(&cell.cell);
        Some(BoxDrawingCell {
            col: cell.point.column.0,
            row: row_idx,
            c: ch,
            color: convert_alacritty_color(fg),
        })
    }

    fn selected_cols_for_row(
        cells: &[IndexedCell],
        selection_range: &Option<SelectionRange>,
    ) -> Vec<usize> {
        let mut cols = Vec::new();
        for cell in cells {
            if is_point_selected(&cell.point, selection_range) {
                cols.push(cell.point.column.0);
            }
        }
        cols
    }

    fn update_selected_cols_in_range(
        cached_cols: &[usize],
        cells: &[IndexedCell],
        range: (usize, usize),
        selection_range: &Option<SelectionRange>,
    ) -> Vec<usize> {
        let (left, right) = range;
        let mut cols: Vec<usize> = cached_cols
            .iter()
            .copied()
            .filter(|col| *col < left || *col > right)
            .collect();

        for cell in cells {
            let col = cell.point.column.0;
            if col < left || col > right {
                continue;
            }
            if is_point_selected(&cell.point, selection_range) {
                cols.push(col);
            }
        }

        cols.sort_unstable();
        cols
    }

    fn background_cells_for_row(row_idx: usize, cells: &[IndexedCell]) -> Vec<BackgroundCell> {
        let mut backgrounds = Vec::new();
        for cell in cells {
            let (_, bg) = rendered_cell_colors(&cell.cell);
            if bg
                != &alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Background,
                )
            {
                backgrounds.push(BackgroundCell {
                    col: cell.point.column.0,
                    row: row_idx,
                    color: convert_alacritty_color(bg),
                });
            }
        }
        backgrounds
    }

    fn update_background_cells_in_range(
        row_idx: usize,
        cached_backgrounds: &[BackgroundCell],
        cells: &[IndexedCell],
        range: (usize, usize),
    ) -> Vec<BackgroundCell> {
        let (left, right) = range;
        let mut backgrounds: Vec<BackgroundCell> = cached_backgrounds
            .iter()
            .filter(|bg| bg.col < left || bg.col > right)
            .cloned()
            .collect();

        for cell in cells {
            let col = cell.point.column.0;
            if col < left || col > right {
                continue;
            }
            let (_, bg) = rendered_cell_colors(&cell.cell);
            if bg
                != &alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Background,
                )
            {
                backgrounds.push(BackgroundCell {
                    col,
                    row: row_idx,
                    color: convert_alacritty_color(bg),
                });
            }
        }

        backgrounds.sort_by_key(|bg| bg.col);
        backgrounds
    }

    fn box_cells_for_row(row_idx: usize, cells: &[IndexedCell]) -> Vec<BoxDrawingCell> {
        let mut box_cells = Vec::new();
        for cell in cells {
            if let Some(box_cell) = Self::box_cell_for_cell(row_idx, cell) {
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
            if let Some(box_cell) = Self::box_cell_for_cell(row_idx, cell) {
                box_cells.push(box_cell);
            }
        }

        box_cells.sort_by_key(|cell| cell.col);
        box_cells
    }

    fn try_update_runs_in_range<'a>(
        row_idx: usize,
        cached_runs: &[BatchedTextRun],
        cells: &'a [IndexedCell],
        range: (usize, usize),
        window: &mut Window,
        text_style: &TextStyle,
        font_size: Pixels,
        cell_width: Pixels,
    ) -> Option<Vec<BatchedTextRun>> {
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
            window,
            text_style,
            font_size,
            cell_width,
        );

        merged_runs.extend(dirty_entry.runs);
        merged_runs.sort_by_key(|run| run.start_col);
        Some(merged_runs)
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
    type PrepaintState = (
        Vec<BatchedTextRun>,
        Vec<BoxDrawingCell>,
        Vec<BackgroundCell>,
        Vec<(usize, usize)>,
        Pixels,
        Pixels,
        usize,
        usize,
        bool,
        TerminalBounds,
        Option<CursorLayout>,
        Option<(gpui::ShapedLine, Point<Pixels>)>,
    );

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("terminal-element".into()))
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

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
        let font_size = px(18.0);
        let line_height = font_size * 1.25;

        #[cfg(target_os = "macos")]
        let (font_family, font_fallbacks) = (
            "Menlo".into(),
            Some(FontFallbacks::from_fonts(vec![
                "Monaco".to_string(),
                "Courier New".to_string(),
            ])),
        );

        #[cfg(target_os = "windows")]
        let (font_family, font_fallbacks) = (
            "Consolas".into(),
            Some(FontFallbacks::from_fonts(vec!["Courier New".to_string()])),
        );

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let (font_family, font_fallbacks) = (
            "monospace".into(),
            Some(FontFallbacks::from_fonts(vec![
                "Noto Sans Mono".to_string(),
                "Source Code Pro".to_string(),
            ])),
        );

        let text_style = TextStyle {
            font_family,
            font_features: Default::default(),
            font_weight: FontWeight::NORMAL,
            font_fallbacks,
            font_size: font_size.into(),
            font_style: FontStyle::Normal,
            line_height: line_height.into(),
            background_color: None,
            white_space: WhiteSpace::Normal,
            color: rgb(0x9aa3b8).into(),
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

                let current_bounds = terminal.terminal_bounds();
                if current_bounds.num_columns() != terminal_bounds.num_columns()
                    || current_bounds.num_lines() != terminal_bounds.num_lines()
                {
                    terminal.resize(terminal_bounds, cx);
                }
                terminal.set_terminal_bounds(terminal_bounds);

                let content = terminal.last_content();
                (content.display_offset, content.cursor, content.cursor_shape)
            });

        // Calculate visible viewport
        let visible_bounds = window.content_mask().bounds;
        let intersection = visible_bounds.intersect(&bounds);

        let _visible_line_start = if intersection.size.height > px(0.0) {
            ((intersection.top() - bounds.top()).max(px(0.0)) / line_height).floor() as usize
        } else {
            0
        };
        let _visible_line_count = if intersection.size.height > px(0.0) {
            ((intersection.size.height / line_height).ceil() as usize).max(1)
        } else {
            terminal_bounds.num_lines()
        };

        // Get viewport info for cursor calculation
        let viewport_lines = terminal_bounds.num_lines();

        // --- Row dirty tracking + per-row layout cache ---
        // dirty_rows is populated by Terminal::sync() via alacritty's damage API.
        let (dirty_rows, dirty_row_bounds, mouse_mode, selection_range, cursor_cell) = {
            let terminal_snapshot = self.terminal.read(cx);
            let dirty_rows = terminal_snapshot.dirty_rows().to_vec();
            let dirty_row_bounds = terminal_snapshot.dirty_row_bounds().to_vec();
            let mouse_mode = terminal_snapshot
                .last_content()
                .mode
                .intersects(alacritty_terminal::term::TermMode::MOUSE_MODE);
            let selection_range = if !mouse_mode {
                terminal_snapshot.selection_range()
            } else {
                None
            };
            let cursor_cell = terminal_snapshot.last_content().cursor_cell.clone();
            (
                dirty_rows,
                dirty_row_bounds,
                mouse_mode,
                selection_range,
                cursor_cell,
            )
        };

        // Selection changes only invalidate selection slices inside row caches.
        let selection_range_for_cache = selection_range.clone();
        let selection_changed = self.view.update(cx, |view, _cx| {
            let changed = view.cached_selection_mouse_mode != mouse_mode
                || view.cached_selection_range != selection_range_for_cache;
            if changed {
                for row_cache in &mut view.row_render_cache {
                    row_cache.selection_cols = None;
                }
                view.cached_selection_mouse_mode = mouse_mode;
                view.cached_selection_range = selection_range_for_cache;
            }
            changed
        });

        // Take row render cache out of the view so we can mutate it here
        // without a borrow conflict, then write it back after processing.
        let mut row_caches: Vec<RowRenderCache> = self
            .view
            .update(cx, |view, _cx| std::mem::take(&mut view.row_render_cache));

        // Grow or shrink caches to match the current viewport height.
        row_caches.resize_with(viewport_lines, RowRenderCache::default);

        let mut batched_runs: Vec<BatchedTextRun> = Vec::new();
        let mut box_cells: Vec<BoxDrawingCell> = Vec::new();
        let mut background_cells: Vec<BackgroundCell> = Vec::new();
        let mut selection_cells: Vec<(usize, usize)> = Vec::new();
        let mut dirty_count = 0usize;
        let mut cache_hit_count = 0usize;
        let mut partial_run_update_count = 0usize;

        // Reuse row ranges produced in `Terminal::update_last_content` to avoid per-frame scans.
        let terminal = self.terminal.read(cx);
        let content = terminal.last_content();
        let content_cells = &content.cells;
        let row_ranges = &content.row_ranges;

        for row_idx in 0..viewport_lines {
            let is_dirty = dirty_rows.get(row_idx).copied().unwrap_or(true);
            let row_cells: &[IndexedCell] = if let Some((start, end)) = row_ranges.get(row_idx) {
                &content_cells[*start..*end]
            } else {
                &[]
            };

            let row_cache = &mut row_caches[row_idx];

            if is_dirty || row_cache.layout.is_none() {
                dirty_count += 1;
                let cached_entry = row_cache.layout.clone();
                let dirty_range = dirty_row_bounds.get(row_idx).copied().flatten();
                let entry = if let (Some(cached_entry), Some(range)) = (cached_entry, dirty_range) {
                    if let Some(updated_runs) = Self::try_update_runs_in_range(
                        row_idx,
                        &cached_entry.runs,
                        row_cells,
                        range,
                        window,
                        &text_style,
                        font_size,
                        cell_width,
                    ) {
                        partial_run_update_count += 1;
                        let mut entry = cached_entry;
                        entry.runs = updated_runs;
                        entry
                    } else {
                        Self::layout_row(
                            row_idx,
                            row_cells.iter(),
                            window,
                            &text_style,
                            font_size,
                            cell_width,
                        )
                    }
                } else {
                    Self::layout_row(
                        row_idx,
                        row_cells.iter(),
                        window,
                        &text_style,
                        font_size,
                        cell_width,
                    )
                };
                batched_runs.extend_from_slice(&entry.runs);
                row_cache.layout = Some(entry);
            } else {
                cache_hit_count += 1;
                let cached_entry = row_cache.layout.as_ref().unwrap();
                batched_runs.extend_from_slice(&cached_entry.runs);
            }

            if row_cache.box_cells.is_none() {
                let row_boxes = Self::box_cells_for_row(row_idx, row_cells);
                box_cells.extend_from_slice(&row_boxes);
                row_cache.box_cells = Some(row_boxes);
            } else if is_dirty {
                if let Some(range) = dirty_row_bounds.get(row_idx).copied().flatten() {
                    let cached_row_boxes = row_cache.box_cells.as_ref().unwrap();
                    let row_boxes = Self::update_box_cells_in_range(
                        row_idx,
                        cached_row_boxes,
                        row_cells,
                        range,
                    );
                    box_cells.extend_from_slice(&row_boxes);
                    row_cache.box_cells = Some(row_boxes);
                } else {
                    let cached_row_boxes = row_cache.box_cells.as_ref().unwrap();
                    box_cells.extend_from_slice(cached_row_boxes);
                }
            } else {
                let cached_row_boxes = row_cache.box_cells.as_ref().unwrap();
                box_cells.extend_from_slice(cached_row_boxes);
            }

            if row_cache.background_cells.is_none() {
                let backgrounds = Self::background_cells_for_row(row_idx, row_cells);
                background_cells.extend_from_slice(&backgrounds);
                row_cache.background_cells = Some(backgrounds);
            } else if is_dirty {
                if let Some(range) = dirty_row_bounds.get(row_idx).copied().flatten() {
                    let cached_backgrounds = row_cache.background_cells.as_ref().unwrap();
                    let backgrounds = Self::update_background_cells_in_range(
                        row_idx,
                        cached_backgrounds,
                        row_cells,
                        range,
                    );
                    background_cells.extend_from_slice(&backgrounds);
                    row_cache.background_cells = Some(backgrounds);
                } else {
                    let cached_backgrounds = row_cache.background_cells.as_ref().unwrap();
                    background_cells.extend_from_slice(cached_backgrounds);
                }
            } else {
                let cached_backgrounds = row_cache.background_cells.as_ref().unwrap();
                background_cells.extend_from_slice(cached_backgrounds);
            }

            if selection_changed || row_cache.selection_cols.is_none() {
                let selected_cols = Self::selected_cols_for_row(row_cells, &selection_range);
                selection_cells.extend(selected_cols.iter().map(|col| (row_idx, *col)));
                row_cache.selection_cols = Some(selected_cols);
            } else if is_dirty {
                if let Some(range) = dirty_row_bounds.get(row_idx).copied().flatten() {
                    let cached_selected_cols = row_cache.selection_cols.as_ref().unwrap();
                    let selected_cols = Self::update_selected_cols_in_range(
                        cached_selected_cols,
                        row_cells,
                        range,
                        &selection_range,
                    );
                    selection_cells.extend(selected_cols.iter().map(|col| (row_idx, *col)));
                    row_cache.selection_cols = Some(selected_cols);
                } else {
                    let cached_selected_cols = row_cache.selection_cols.as_ref().unwrap();
                    selection_cells.extend(cached_selected_cols.iter().map(|col| (row_idx, *col)));
                }
            } else {
                let cached_selected_cols = row_cache.selection_cols.as_ref().unwrap();
                selection_cells.extend(cached_selected_cols.iter().map(|col| (row_idx, *col)));
            }

            #[cfg(debug_assertions)]
            {
                let row_runs = &row_cache.layout.as_ref().unwrap().runs;
                let row_boxes = row_cache.box_cells.as_ref().unwrap();
                let row_backgrounds = row_cache.background_cells.as_ref().unwrap();
                let row_selection_cols = row_cache.selection_cols.as_ref().unwrap();
                Self::assert_row_consistency(
                    row_idx,
                    row_cells,
                    row_runs,
                    row_boxes,
                    row_backgrounds,
                    row_selection_cols,
                    window,
                    &text_style,
                    font_size,
                    cell_width,
                    &selection_range,
                );
            }
        }

        log::trace!("row cache: {cache_hit_count}/{viewport_lines} hits, {dirty_count} recomputed");
        log::trace!("row run partial updates: {partial_run_update_count}");

        // Write the (potentially updated) cache back into the view.
        self.view.update(cx, |view, _cx| {
            view.row_render_cache = row_caches;
        });

        // Calculate cursor position relative to viewport
        let cursor_line_in_viewport = cursor_point.line.0 + display_offset as i32;

        // Cursor is only visible if it's within the current viewport
        let cursor_in_viewport =
            cursor_line_in_viewport >= 0 && (cursor_line_in_viewport as usize) < viewport_lines;
        let cursor_row = cursor_line_in_viewport.max(0) as usize;

        let cursor_display_col = cursor_point.column.0;

        // Check if IME composition is active — hide terminal cursor during input
        let has_ime_composition = self.view.read(cx).has_ime_composition();

        // 计算光标布局 - 如果光标形状为Hidden则跳过（由应用程序自己处理光标渲染）
        // 这是光标委托机制：当应用程序（如Claude Code）设置光标为Hidden时，
        // 终端不渲染光标，让应用程序自己处理
        // IME 输入时也隐藏光标，由 preedit overlay 替代显示
        let cursor = if self.cursor_visible
            && cursor_in_viewport
            && cursor_shape != AlacCursorShape::Hidden
            && !has_ime_composition
        {
            let cursor_x = terminal_bounds.bounds.left()
                + px((cursor_display_col as f32 * cell_width.as_f32()).floor());
            let cursor_y = terminal_bounds.bounds.top()
                + px((cursor_line_in_viewport as f32 * line_height.as_f32()).floor());

            let shape = match cursor_shape {
                AlacCursorShape::Block if !self.focused => CursorShape::Hollow,
                AlacCursorShape::Block => CursorShape::Block,
                AlacCursorShape::Underline => CursorShape::Underline,
                AlacCursorShape::Beam => CursorShape::Bar,
                AlacCursorShape::HollowBlock => CursorShape::Hollow,
                AlacCursorShape::Hidden => unreachable!(),
            };

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
            let (_, cursor_bg) = rendered_cell_colors(&cursor_cell);
            let block_text_color = convert_alacritty_color(cursor_bg);

            // Create text for block cursor (inverted color effect)
            let block_text = if matches!(shape, CursorShape::Block) {
                if cursor_char != '\0' && cursor_char != ' ' {
                    let char_str = cursor_char.to_string();
                    let len = cursor_char.len_utf8();
                    Some(window.text_system().shape_line(
                        char_str.into(),
                        font_size,
                        &[TextRun {
                            len,
                            font: text_style.font(),
                            color: block_text_color,
                            ..Default::default()
                        }],
                        Some(cursor_block_width),
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            let cursor_color =
                convert_alacritty_color(&alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Cursor,
                ));

            Some(CursorLayout {
                origin: point(cursor_x, cursor_y),
                block_width: cursor_block_width,
                line_height,
                thickness: cursor_thickness(cell_width),
                color: cursor_color,
                shape,
                block_text,
            })
        } else {
            None
        };

        // Shape IME preedit (marked) text for overlay rendering.
        let ime_overlay: Option<(gpui::ShapedLine, Point<Pixels>)> = if cursor_in_viewport {
            let ime_text = self
                .view
                .read(cx)
                .ime_state()
                .map(|s| s.marked_text.clone());
            if let Some(text) = ime_text {
                if !text.is_empty() {
                    let ime_x = terminal_bounds.bounds.left()
                        + px((cursor_display_col as f32 * cell_width.as_f32()).floor());
                    let ime_y = terminal_bounds.bounds.top()
                        + px((cursor_line_in_viewport as f32 * line_height.as_f32()).floor());
                    let len = text.len();
                    let shaped = window.text_system().shape_line(
                        text.into(),
                        font_size,
                        &[TextRun {
                            len,
                            font: text_style.font(),
                            color: gpui::white(),
                            ..Default::default()
                        }],
                        None,
                    );
                    Some((shaped, point(ime_x, ime_y)))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        (
            batched_runs,
            box_cells,
            background_cells,
            selection_cells,
            cell_width,
            line_height,
            cursor_point.column.0,
            cursor_row,
            cursor_in_viewport,
            terminal_bounds,
            cursor,
            ime_overlay,
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
        let (
            batched_runs,
            box_cells,
            background_cells,
            selection_cells,
            cell_width,
            line_height,
            _cursor_col,
            _cursor_row,
            _cursor_in_viewport,
            terminal_bounds,
            cursor,
            ime_overlay,
        ) = prepaint_state;

        let origin = terminal_bounds.bounds.origin;

        for (row, col) in selection_cells.iter() {
            let cell_x = origin.x + *cell_width * *col as f32;
            let cell_y = origin.y + *line_height * *row as f32;
            window.paint_quad(fill(
                Bounds::new(point(cell_x, cell_y), size(*cell_width, *line_height)),
                gpui::rgb(0x4a4a6a),
            ));
        }

        for cell in background_cells.iter() {
            let cell_origin = point(
                origin.x + *cell_width * cell.col as f32,
                origin.y + *line_height * cell.row as f32,
            );
            window.paint_quad(fill(
                Bounds::new(cell_origin, size(*cell_width, *line_height)),
                cell.color,
            ));
        }

        // Paint batched text runs (text will appear on top of backgrounds)
        for batch in batched_runs.iter() {
            batch.paint(origin, *cell_width, *line_height, window, cx);
        }

        // 绘制内置盒子绘制字符 / 块元素。
        // 这些字符在 layout_row 阶段已从文本批次中分离出来，
        // 此处用 GPUI 矩形图元（paint_quad）按单元格坐标精确绘制，
        // 效果等同于 Alacritty 的 builtin_box_drawing = true：
        // 相邻格的边框线条精确对齐，不依赖字体字形，不会出现间隙。
        for cell in box_cells.iter() {
            let cell_origin = point(
                origin.x + *cell_width * cell.col as f32,
                origin.y + *line_height * cell.row as f32,
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

        // 使用prepaint阶段计算的光标布局进行渲染
        // 如果cursor为None（光标形状为Hidden），则不渲染光标
        // 这实现了光标委托：当应用程序自己处理光标时，终端不渲染
        if let Some(cursor_layout) = cursor {
            cursor_layout.paint(window, cx);
        }

        // Render IME preedit (marked) text overlay at cursor position.
        if let Some((ime_shaped, ime_origin)) = ime_overlay {
            let ime_background_bounds =
                Bounds::new(*ime_origin, size(ime_shaped.width, *line_height));
            window.paint_quad(fill(ime_background_bounds, gpui::rgb(0x1a1a2e)));

            let _ = ime_shaped.paint(
                *ime_origin,
                *line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );

            // Draw underline below preedit text to indicate IME composition
            let underline_y = ime_origin.y + *line_height - px(1.0);
            window.paint_quad(fill(
                Bounds::new(
                    point(ime_origin.x, underline_y),
                    size(ime_shaped.width, px(1.0)),
                ),
                gpui::rgb(0xccccff),
            ));
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
}
