//! Terminal element for rendering

use gpui::{
    BorderStyle, Bounds, Element, ElementId, ElementInputHandler, Entity, FocusHandle, Font,
    FontFallbacks, FontStyle, FontWeight, GlobalElementId, Hsla, IntoElement, LayoutId, Pixels,
    Point, Style, TextRun, TextStyle, WhiteSpace, Window, fill, outline, point, px, rgb, size,
};
use itertools::Itertools;

use crate::terminal::mappings::colors::convert_alacritty_color;
use crate::terminal::terminal::{IndexedCell, Terminal, TerminalBounds};
use crate::terminal_view::box_drawing;
use crate::terminal_view::terminal_view::TerminalView;
use alacritty_terminal::index::Point as AlacPoint;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::vte::ansi::CursorShape as AlacCursorShape;

/// Check if a character is a wide (CJK/double-width) character.
/// In terminals, wide characters occupy 2 cells instead of 1.
#[inline]
fn is_wide_char(c: char) -> bool {
    // CJK and other East Asian Wide characters
    matches!(c,
        '\u{1100}'..='\u{11FF}' |    // Hangul Jamo
        '\u{2E80}'..='\u{2EFF}' |    // CJK Radicals Supplement
        '\u{2F00}'..='\u{2FDF}' |    // Kangxi Radicals
        '\u{3000}'..='\u{303F}' |    // CJK Punctuation
        '\u{3040}'..='\u{309F}' |    // Hiragana
        '\u{30A0}'..='\u{30FF}' |    // Katakana
        '\u{3100}'..='\u{312F}' |    // Bopomofo
        '\u{3130}'..='\u{318F}' |    // Hangul Compatibility Jamo
        '\u{3190}'..='\u{319F}' |    // Kanbun
        '\u{31A0}'..='\u{31BF}' |    // Bopomofo Extended
        '\u{31C0}'..='\u{31EF}' |    // CJK Strokes
        '\u{31F0}'..='\u{31FF}' |    // Katakana Phonetic Extensions
        '\u{3200}'..='\u{32FF}' |    // Enclosed CJK Letters and Months
        '\u{3300}'..='\u{33FF}' |    // CJK Compatibility
        '\u{3400}'..='\u{4DBF}' |    // CJK Ideographs Extension A
        '\u{4E00}'..='\u{9FFF}' |    // CJK Ideographs
        '\u{A960}'..='\u{A97F}' |    // Hangul Jamo Extended-A
        '\u{AC00}'..='\u{D7AF}' |    // Hangul Syllables
        '\u{D7B0}'..='\u{D7FF}' |    // Hangul Jamo Extended-B
        '\u{F900}'..='\u{FAFF}' |    // CJK Compatibility Ideographs
        '\u{FF01}'..='\u{FF5E}' |    // Fullwidth ASCII variants
        '\u{FFE0}'..='\u{FFE6}'      // Fullwidth symbols
    )
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

/// Cached per-row layout: (text runs, box-drawing cells) for one visible row.
/// Stored in `TerminalView::row_layout_cache` and reused across frames when the row is clean.
pub type RowLayoutEntry = (Vec<BatchedTextRun>, Vec<BoxDrawingCell>);

/// 盒子绘制字符 / 块元素单元格。
///
/// 对于 U+2500–U+259F 范围内的字符，不走字体光栅化流程，
/// 而是在 `paint` 阶段用 GPUI 矩形图元直接绘制，
/// 以保证相邻单元格的线条精确对齐（不受字体字形影响）。
#[derive(Clone)]
pub struct BoxDrawingCell {
    pub col: usize,
    pub row: usize,
    pub c: char,
    pub color: Hsla, // 该字符的前景色（已处理 INVERSE 反色标志）
}

/// CursorLayout - simplified implementation inspired by Zed's CursorLayout
/// Computed during prepaint phase, rendered during paint phase
pub struct CursorLayout {
    origin: Point<Pixels>,
    block_width: Pixels,
    line_height: Pixels,
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
        let bounds = match self.shape {
            CursorShape::Bar => Bounds {
                origin: self.origin,
                size: size(px(2.0), self.line_height),
            },
            CursorShape::Block | CursorShape::Hollow => Bounds {
                origin: self.origin,
                size: size(self.block_width, self.line_height),
            },
            CursorShape::Underline => Bounds {
                origin: self.origin + point(px(0.0), self.line_height - px(2.0)),
                size: size(self.block_width, px(2.0)),
            },
        };

        let cursor_quad = if matches!(self.shape, CursorShape::Hollow) {
            outline(bounds, self.color, BorderStyle::Solid)
        } else {
            fill(bounds, self.color)
        };

        window.paint_quad(cursor_quad);

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
    pub text: String,
    pub cell_count: usize,
    pub style: TextRun,
    pub font_size: Pixels,
}

impl BatchedTextRun {
    fn new_from_char(row: usize, col: usize, c: char, style: TextRun, font_size: Pixels) -> Self {
        let mut text = String::with_capacity(100);
        text.push(c);
        BatchedTextRun {
            start_col: col,
            start_row: row,
            text,
            cell_count: 1,
            style,
            font_size,
        }
    }

    fn can_append(&self, other_style: &TextRun) -> bool {
        self.style.font == other_style.font
            && self.style.color == other_style.color
            && self.style.background_color == other_style.background_color
            && self.style.underline == other_style.underline
            && self.style.strikethrough == other_style.strikethrough
    }

    fn append_char(&mut self, c: char) {
        self.text.push(c);
        self.cell_count += 1;
        self.style.len += c.len_utf8();
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

        let shaped_line = window.text_system().shape_line(
            self.text.clone().into(),
            self.font_size,
            std::slice::from_ref(&self.style),
            Some(cell_width),
        );

        let _ = shaped_line.paint(pos, line_height, gpui::TextAlign::Left, None, window, cx);
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
    fn layout_row(
        row_idx: usize,
        cells: impl IntoIterator<Item = IndexedCell>,
        text_style: &TextStyle,
        font_size: Pixels,
    ) -> RowLayoutEntry {
        let mut batched_runs: Vec<BatchedTextRun> = Vec::new();
        let mut box_cells: Vec<BoxDrawingCell> = Vec::new();
        let mut current_batch: Option<BatchedTextRun> = None;

        for cell in cells {
            let ch = cell.cell.c;

            if ch == '\0' || ch == ' ' {
                if let Some(batch) = current_batch.take() {
                    batched_runs.push(batch);
                }
                continue;
            }

            // Handle INVERSE flag: swap foreground and background colors
            let (fg, bg) = if cell
                .cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::INVERSE)
            {
                (&cell.cell.bg, &cell.cell.fg)
            } else {
                (&cell.cell.fg, &cell.cell.bg)
            };

            // 盒子绘制字符 / 块元素（U+2500–U+259F）：
            // 跳过字体渲染，收集到 box_cells 中，稍后在 paint 阶段
            // 用 GPUI 矩形图元直接绘制，确保相邻单元格边框线条无缝对接。
            if box_drawing::is_box_drawing_char(ch) {
                if let Some(batch) = current_batch.take() {
                    batched_runs.push(batch);
                }
                box_cells.push(BoxDrawingCell {
                    col: cell.point.column.0,
                    row: row_idx,
                    c: ch,
                    color: convert_alacritty_color(fg),
                });
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
                Some(batch)
                    if batch.can_append(&cell_style)
                        && batch.start_col + batch.cell_count == cell.point.column.0 =>
                {
                    batch.append_char(ch);
                }
                _ => {
                    if let Some(batch) = current_batch.take() {
                        batched_runs.push(batch);
                    }
                    current_batch = Some(BatchedTextRun::new_from_char(
                        row_idx,
                        cell.point.column.0,
                        ch,
                        cell_style,
                        font_size,
                    ));
                }
            }
        }

        if let Some(batch) = current_batch {
            batched_runs.push(batch);
        }

        (batched_runs, box_cells)
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
        TextStyle,
        Pixels,
        Pixels,
        Pixels,
        usize,
        usize,
        bool,
        Option<String>,
        TerminalBounds,
        Option<CursorLayout>,
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

        let terminal_bounds = TerminalBounds::new(line_height, cell_width, bounds);

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
                (
                    content.display_offset,
                    content.cursor,
                    content.cursor_shape,
                )
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
        let dirty_rows = self.terminal.read(cx).dirty_rows().to_vec();

        // Collect cells grouped by sequential row (0 = topmost visible row).
        // The borrow on terminal content ends when this block exits.
        let cells_by_row: Vec<Vec<IndexedCell>> = {
            let content = self.terminal.read(cx).last_content();
            let line_groups = content.cells.iter().cloned().chunk_by(|c| c.point.line);
            let mut by_row: Vec<Vec<IndexedCell>> = Vec::with_capacity(viewport_lines);
            for (_, (_, row_cells)) in line_groups.into_iter().enumerate() {
                by_row.push(row_cells.collect());
            }
            by_row
        };

        // Take the row layout cache out of the view so we can mutate it here
        // without a borrow conflict, then write it back after processing.
        let mut row_cache: Vec<Option<RowLayoutEntry>> =
            self.view.update(cx, |view, _cx| std::mem::take(&mut view.row_layout_cache));

        // Grow or shrink cache to match the current viewport height.
        row_cache.resize_with(viewport_lines, || None);

        let mut batched_runs: Vec<BatchedTextRun> = Vec::new();
        let mut box_cells: Vec<BoxDrawingCell> = Vec::new();
        let mut dirty_count = 0usize;
        let mut cache_hit_count = 0usize;

        for row_idx in 0..viewport_lines {
            let is_dirty = dirty_rows.get(row_idx).copied().unwrap_or(true);

            if is_dirty || row_cache[row_idx].is_none() {
                dirty_count += 1;
                let row_cells = cells_by_row.get(row_idx).cloned().unwrap_or_default();
                let (runs, boxes) =
                    Self::layout_row(row_idx, row_cells, &text_style, font_size);
                batched_runs.extend_from_slice(&runs);
                box_cells.extend_from_slice(&boxes);
                row_cache[row_idx] = Some((runs, boxes));
            } else {
                cache_hit_count += 1;
                let (cached_runs, cached_boxes) = row_cache[row_idx].as_ref().unwrap();
                batched_runs.extend_from_slice(cached_runs);
                box_cells.extend_from_slice(cached_boxes);
            }
        }

        log::trace!(
            "row cache: {cache_hit_count}/{viewport_lines} hits, {dirty_count} recomputed"
        );

        // Write the (potentially updated) cache back into the view.
        self.view.update(cx, |view, _cx| {
            view.row_layout_cache = row_cache;
        });

        // Calculate cursor position relative to viewport
        let cursor_line_in_viewport = cursor_point.line.0 + display_offset as i32;

        // Cursor is only visible if it's within the current viewport
        let cursor_in_viewport =
            cursor_line_in_viewport >= 0 && (cursor_line_in_viewport as usize) < viewport_lines;
        let cursor_row = cursor_line_in_viewport.max(0) as usize;

        let selection = self.terminal.read(cx).selection_text();

        // 计算光标布局 - 如果光标形状为Hidden则跳过（由应用程序自己处理光标渲染）
        // 这是光标委托机制：当应用程序（如Claude Code）设置光标为Hidden时，
        // 终端不渲染光标，让应用程序自己处理
        let cursor = if self.cursor_visible
            && cursor_in_viewport
            && cursor_shape != AlacCursorShape::Hidden
        {
            let cursor_display_line = cursor_point.line.0 + display_offset as i32;
            let cursor_display_col = cursor_point.column.0;

            let cursor_x =
                bounds.left() + px(4.0) + (cursor_display_col as f32 * cell_width).floor();
            let cursor_y =
                bounds.top() + (cursor_display_line as f32 * line_height).floor() + px(1.0);

            let shape = match cursor_shape {
                AlacCursorShape::Block if !self.focused => CursorShape::Hollow,
                AlacCursorShape::Block => CursorShape::Block,
                AlacCursorShape::Underline => CursorShape::Underline,
                AlacCursorShape::Beam => CursorShape::Bar,
                AlacCursorShape::HollowBlock => CursorShape::Hollow,
                AlacCursorShape::Hidden => unreachable!(),
            };

            // Get cursor character and check if it's a wide character
            let cursor_char = self.terminal.read(cx).last_content().cursor_char;
            let cursor_is_wide = cursor_char != '\0' && is_wide_char(cursor_char);
            let cursor_block_width = if cursor_is_wide { cell_width * 2.0 } else { cell_width };

            // Create text for block cursor (inverted color effect)
            let block_text = if matches!(shape, CursorShape::Block) {
                if cursor_char != '\0' && cursor_char != ' ' {
                    let char_str = cursor_char.to_string();
                    let len = cursor_char.len_utf8();
                    let bg_color = convert_alacritty_color(&alacritty_terminal::vte::ansi::Color::Named(
                        alacritty_terminal::vte::ansi::NamedColor::Background,
                    ));
                    Some(window.text_system().shape_line(
                        char_str.into(),
                        font_size,
                        &[TextRun {
                            len,
                            font: text_style.font(),
                            color: bg_color,
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

            let cursor_color = convert_alacritty_color(&alacritty_terminal::vte::ansi::Color::Named(
                alacritty_terminal::vte::ansi::NamedColor::Cursor,
            ));

            Some(CursorLayout {
                origin: point(cursor_x, cursor_y),
                block_width: cursor_block_width,
                line_height,
                color: cursor_color,
                shape,
                block_text,
            })
        } else {
            None
        };

        (
            batched_runs,
            box_cells,
            text_style,
            cell_width,
            line_height,
            font_size,
            cursor_point.column.0,
            cursor_row,
            cursor_in_viewport,
            selection,
            terminal_bounds,
            cursor,
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
            _text_style,
            cell_width,
            line_height,
            _font_size,
            _cursor_col,
            _cursor_row,
            _cursor_in_viewport,
            _selection,
            _terminal_bounds,
            cursor,
        ) = prepaint_state;

        let origin = point(bounds.left() + px(4.0), bounds.top());

        // Get terminal content and selection range once.
        // Previously: selection_range() was called once per cell inside the loop (O(N) mutex locks).
        // Now: one lock for the whole frame; is_point_selected() is a pure function.
        let term = self.terminal.read(cx);
        let content = term.last_content();
        let mouse_mode = content
            .mode
            .intersects(alacritty_terminal::term::TermMode::MOUSE_MODE);
        let selection_range = if !mouse_mode {
            term.selection_range()
        } else {
            None
        };

        // Single pass over cells (previously 3 separate iterations + a HashMap):
        // For each display line we compute the y-offset from the enumerate index, then
        // in one scan handle both selection backgrounds and non-default cell backgrounds.
        {
            let line_groups = content.cells.iter().chunk_by(|cell| cell.point.line);
            for (visible_row, (_, line_cells)) in line_groups.into_iter().enumerate() {
                let cell_y = bounds.top() + *line_height * visible_row as f32;

                for cell in line_cells {
                    // Selection background
                    if is_point_selected(&cell.point, &selection_range) {
                        let cell_x =
                            bounds.left() + px(4.0) + *cell_width * cell.point.column.0 as f32;
                        window.paint_quad(fill(
                            Bounds::new(point(cell_x, cell_y), size(*cell_width, *line_height)),
                            gpui::rgb(0x4a4a6a),
                        ));
                    }

                    // Non-default cell background (handle INVERSE flag)
                    let is_inverse = cell
                        .cell
                        .flags
                        .contains(alacritty_terminal::term::cell::Flags::INVERSE);
                    let bg = if is_inverse { &cell.cell.fg } else { &cell.cell.bg };
                    if bg
                        != &alacritty_terminal::vte::ansi::Color::Named(
                            alacritty_terminal::vte::ansi::NamedColor::Background,
                        )
                    {
                        let cell_x =
                            bounds.left() + px(4.0) + *cell_width * cell.point.column.0 as f32;
                        window.paint_quad(fill(
                            Bounds::new(point(cell_x, cell_y), size(*cell_width, *line_height)),
                            convert_alacritty_color(bg),
                        ));
                    }
                }
            }
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

        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
    }
}
