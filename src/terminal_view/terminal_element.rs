//! Terminal element for GPUI rendering
//!
//! Implements the Element trait for custom terminal rendering
//! Based on Zed's terminal implementation

use gpui::{
    BorderStyle, Bounds, Element, ElementId, ElementInputHandler, Entity, FocusHandle, Font,
    FontFallbacks, FontStyle, FontWeight, GlobalElementId, IntoElement, LayoutId, Pixels, Point,
    Style, TextRun, TextStyle, WhiteSpace, Window, fill, outline, point, px, rgb, size,
};
use itertools::Itertools;

#[macro_export]
macro_rules! render_log {
    ($($arg:tt)*) => {
        eprintln!($($arg)*)
    };
}

use crate::terminal::terminal::{IndexedCell, Terminal};
use crate::terminal_view::terminal_view::TerminalView;

/// A batched text run that combines multiple adjacent cells with the same style
#[derive(Debug, Clone)]
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

/// Terminal element for GPUI
pub struct TerminalElement {
    terminal: Entity<Terminal>,
    focus: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    view: Entity<TerminalView>,
}

impl TerminalElement {
    /// Create a new terminal element
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

    /// Layout the terminal grid into batched text runs
    /// Based on Zed's implementation - only processes visible cells
    fn layout_grid(
        grid: impl Iterator<Item = IndexedCell>,
        display_offset: usize,
        text_style: &TextStyle,
        font_size: Pixels,
        visible_line_start: usize,
        visible_line_count: usize,
    ) -> Vec<BatchedTextRun> {
        let estimated_cells = grid.size_hint().0;
        if estimated_cells == 0 {
            return Vec::new();
        }

        static RENDER_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let call_count = RENDER_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if call_count % 60 == 0 {
            render_log!(
                "[Render] layout_grid called: estimated_cells={}, visible_line_start={}, visible_line_count={}",
                estimated_cells,
                visible_line_start,
                visible_line_count
            );
        }

        let estimated_runs = (estimated_cells / 10).max(16);
        let mut batched_runs = Vec::with_capacity(estimated_runs);
        let mut current_batch: Option<BatchedTextRun> = None;

        // Group cells by line and filter to only visible rows
        // This matches Zed's approach using itertools::chunk_by
        let line_groups = grid.into_iter().chunk_by(|cell| cell.point.line);

        for (line_index, (_, line_cells)) in line_groups.into_iter().enumerate() {
            // Skip lines outside the visible viewport
            if line_index < visible_line_start
                || line_index >= visible_line_start + visible_line_count
            {
                continue;
            }

            // Calculate relative row position for rendering
            let relative_row = line_index.saturating_sub(display_offset);

            // Flush any existing batch at line boundaries
            if let Some(batch) = current_batch.take() {
                batched_runs.push(batch);
            }

            for cell in line_cells {
                let ch = cell.cell.c;

                // Skip null chars and spaces (like Zed does)
                if ch == '\0' || ch == ' ' {
                    if let Some(batch) = current_batch.take() {
                        batched_runs.push(batch);
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
                    color: convert_alacritty_color(&cell.cell.fg),
                    background_color: if cell.cell.bg
                        == alacritty_terminal::vte::ansi::Color::Named(
                            alacritty_terminal::vte::ansi::NamedColor::Background,
                        ) {
                        None
                    } else {
                        Some(convert_alacritty_color(&cell.cell.bg))
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

                // Try to batch with existing run
                match current_batch.as_mut() {
                    Some(batch)
                        if batch.can_append(&cell_style)
                            && batch.start_row == relative_row
                            && batch.start_col + batch.cell_count == cell.point.column.0 =>
                    {
                        batch.append_char(ch);
                    }
                    _ => {
                        if let Some(batch) = current_batch.take() {
                            batched_runs.push(batch);
                        }
                        current_batch = Some(BatchedTextRun::new_from_char(
                            relative_row,
                            cell.point.column.0,
                            ch,
                            cell_style,
                            font_size,
                        ));
                    }
                }
            }
        }

        // Flush any remaining batch
        if let Some(batch) = current_batch {
            batched_runs.push(batch);
        }

        batched_runs
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
        TextStyle,
        Pixels,
        Pixels,
        Pixels,
        usize,          // cursor column
        usize,          // cursor row
        Option<String>, // selection text
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
        let font_size = px(13.0);
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
            color: rgb(0xcdd6f4).into(),
            ..Default::default()
        };

        let font_id = window.text_system().resolve_font(&text_style.font());
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|a| a.width)
            .unwrap_or(px(9.0));

        let cols = ((bounds.size.width - px(8.0)) / cell_width).floor() as usize;
        let rows = (bounds.size.height / line_height).floor() as usize;

        // Sync terminal and get content info (like Zed does)
        let (display_offset, cursor) = self.terminal.update(cx, |terminal, cx| {
            // Sync terminal - process events and update last_content
            terminal.sync(window, cx);

            // Handle resize if needed
            if cols > 0 && rows > 0 {
                let current_size = terminal.size();
                if current_size.0 != cols || current_size.1 != rows {
                    terminal.resize(cols, rows, cx);
                }
            }

            let content = terminal.last_content();
            let cursor = content.cursor;
            let display_offset = content.display_offset;

            (display_offset, cursor)
        });

        // Calculate visible viewport for clipping (like Zed does)
        let visible_bounds = window.content_mask().bounds;
        let intersection = visible_bounds.intersect(&bounds);

        // Calculate which rows are visible
        let visible_line_start = if intersection.size.height > px(0.0) {
            ((intersection.top() - bounds.top()).max(px(0.0)) / line_height).floor() as usize
        } else {
            0
        };
        let visible_line_count = if intersection.size.height > px(0.0) {
            ((intersection.size.height / line_height).ceil() as usize).max(1)
        } else {
            rows
        };

        // Generate batched runs for visible cells only (no caching - like Zed)
        let content = self.terminal.read(cx).last_content();
        let batched_runs = Self::layout_grid(
            content.cells.iter().cloned(),
            display_offset,
            &text_style,
            font_size,
            visible_line_start,
            visible_line_count,
        );

        static PREPAINT_COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let prepaint_count = PREPAINT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if prepaint_count % 60 == 0 {
            render_log!(
                "[Render] prepaint: cells={}, batched_runs={}, cols={}, rows={}, visible_line_start={}, visible_line_count={}",
                content.cells.len(),
                batched_runs.len(),
                cols,
                rows,
                visible_line_start,
                visible_line_count
            );
        }

        let cursor_row = (cursor.line.0 as usize).saturating_sub(display_offset);

        // Get selection from terminal
        let selection = self.terminal.read(cx).selection_text();

        (
            batched_runs,
            text_style,
            cell_width,
            line_height,
            font_size,
            cursor.column.0,
            cursor_row,
            selection,
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
            _text_style,
            cell_width,
            line_height,
            _font_size,
            cursor_col,
            cursor_row,
            selection,
        ) = prepaint_state;

        let origin = point(bounds.left() + px(4.0), bounds.top());

        // Paint batched text runs
        for batch in batched_runs.iter() {
            batch.paint(origin, *cell_width, *line_height, window, cx);
        }

        // Draw selection highlight if any
        if let Some(ref sel_text) = *selection {
            // Get selection range from terminal
            let term = self.terminal.read(cx);
            let content = term.last_content();

            // Paint selection background for selected cells
            for cell in content.cells.iter() {
                // Simple check: if cell is part of selection
                // This is a basic implementation - can be improved
                if sel_text.contains(cell.cell.c) {
                    let cell_x = bounds.left() + px(4.0) + *cell_width * cell.point.column.0 as f32;
                    let cell_y = bounds.top()
                        + *line_height
                            * (cell.point.line.0 as usize).saturating_sub(content.display_offset)
                                as f32;

                    let cell_bounds =
                        Bounds::new(point(cell_x, cell_y), size(*cell_width, *line_height));

                    window.paint_quad(fill(cell_bounds, gpui::rgb(0x4a4a6a)));
                }
            }
        }

        // Draw cursor
        if self.cursor_visible {
            let cursor_x = bounds.left() + px(4.0) + *cell_width * *cursor_col as f32;
            let cursor_y = bounds.top() + *line_height * *cursor_row as f32;

            let cursor_bounds =
                Bounds::new(point(cursor_x, cursor_y), size(*cell_width, *line_height));

            if self.focused {
                window.paint_quad(fill(cursor_bounds, rgb(0xcba6f7)));
            } else {
                window.paint_quad(outline(
                    cursor_bounds,
                    rgb(0xcba6f7),
                    BorderStyle::default(),
                ));
            }
        }

        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
    }
}

fn convert_alacritty_color(color: &alacritty_terminal::vte::ansi::Color) -> gpui::Hsla {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    match color {
        Color::Named(named) => match named {
            NamedColor::Black => gpui::rgb(0x181825).into(),
            NamedColor::Red => gpui::rgb(0xff728c).into(),
            NamedColor::Green => gpui::rgb(0x7bf797).into(),
            NamedColor::Yellow => gpui::rgb(0xffdb72).into(),
            NamedColor::Blue => gpui::rgb(0xb88eea).into(),
            NamedColor::Magenta => gpui::rgb(0x4c2d72).into(),
            NamedColor::Cyan => gpui::rgb(0xcba6f7).into(),
            NamedColor::White => gpui::rgb(0xf6efff).into(),
            NamedColor::BrightBlack => gpui::rgb(0x9994a0).into(),
            NamedColor::BrightRed => gpui::rgb(0xff9fb1).into(),
            NamedColor::BrightGreen => gpui::rgb(0xa5ffba).into(),
            NamedColor::BrightYellow => gpui::rgb(0xffe8a5).into(),
            NamedColor::BrightBlue => gpui::rgb(0xd8b7ff).into(),
            NamedColor::BrightMagenta => gpui::rgb(0xdfc4ff).into(),
            NamedColor::BrightCyan => gpui::rgb(0xead9ff).into(),
            NamedColor::BrightWhite => gpui::rgb(0xfbf8ff).into(),
            NamedColor::Foreground => gpui::rgb(0xcdd6f4).into(),
            NamedColor::Background => gpui::rgb(0x1e1e2e).into(),
            NamedColor::Cursor => gpui::rgb(0xcba6f7).into(),
            _ => gpui::rgb(0xcdd6f4).into(),
        },
        Color::Spec(rgb) => {
            gpui::rgb((rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32).into()
        }
        Color::Indexed(idx) => match idx {
            0..=15 => {
                let named = match idx {
                    0 => NamedColor::Black,
                    1 => NamedColor::Red,
                    2 => NamedColor::Green,
                    3 => NamedColor::Yellow,
                    4 => NamedColor::Blue,
                    5 => NamedColor::Magenta,
                    6 => NamedColor::Cyan,
                    7 => NamedColor::White,
                    8 => NamedColor::BrightBlack,
                    9 => NamedColor::BrightRed,
                    10 => NamedColor::BrightGreen,
                    11 => NamedColor::BrightYellow,
                    12 => NamedColor::BrightBlue,
                    13 => NamedColor::BrightMagenta,
                    14 => NamedColor::BrightCyan,
                    15 => NamedColor::BrightWhite,
                    _ => NamedColor::Foreground,
                };
                convert_alacritty_color(&Color::Named(named))
            }
            16..=231 => {
                let idx = *idx - 16;
                let r = (idx / 36) as u8;
                let g = ((idx % 36) / 6) as u8;
                let b = (idx % 6) as u8;
                let r = if r == 0 { 0 } else { r * 40 + 55 };
                let g = if g == 0 { 0 } else { g * 40 + 55 };
                let b = if b == 0 { 0 } else { b * 40 + 55 };
                gpui::rgb((r as u32) << 16 | (g as u32) << 8 | b as u32).into()
            }
            232..=255 => {
                let gray = 8 + (*idx - 232) as u8 * 10;
                gpui::rgb((gray as u32) << 16 | (gray as u32) << 8 | gray as u32).into()
            }
        },
    }
}
