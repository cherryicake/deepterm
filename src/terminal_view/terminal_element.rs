//! Terminal element for GPUI rendering
//!
//! Implements the Element trait for custom terminal rendering
//! Based on Zed's terminal implementation - uses cached last_content

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    fill, outline, point, px, rgb, size, BorderStyle, Bounds, Element, ElementId,
    ElementInputHandler, Entity, FocusHandle, Font, FontFallbacks, FontStyle, FontWeight,
    GlobalElementId, IntoElement, LayoutId, Pixels, Point, Style, TextRun, TextStyle, WhiteSpace,
    Window,
};

use crate::terminal::terminal::{IndexedCell, Terminal, TerminalContent};
use crate::terminal_view::terminal_view::TerminalView;

/// Global counter for tracking total prepaint allocations
static PREPAINT_RUN_COUNT: AtomicU64 = AtomicU64::new(0);
static PREPAINT_TOTAL_CELLS: AtomicU64 = AtomicU64::new(0);

/// A batched text run that combines multiple adjacent cells with the same style
#[derive(Debug)]
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
        let mut text = String::with_capacity(64);
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
    /// Uses cached last_content like Zed - no allocation
    fn layout_grid(
        cells: &[IndexedCell],
        display_offset: usize,
        text_style: &TextStyle,
        font_size: Pixels,
    ) -> Vec<BatchedTextRun> {
        if cells.is_empty() {
            return Vec::new();
        }

        let estimated_runs = (cells.len() / 10).max(16);
        let mut batched_runs = Vec::with_capacity(estimated_runs);
        let mut current_batch: Option<BatchedTextRun> = None;
        let mut last_row: i32 = -1;

        for cell in cells {
            // Convert absolute line to relative line using display_offset
            let relative_line = (cell.point.line.0 - display_offset as i32) as usize;
            let cell_row = relative_line as i32;

            if cell_row != last_row {
                if let Some(batch) = current_batch.take() {
                    batched_runs.push(batch);
                }
                last_row = cell_row;
            }

            let ch = cell.cell.c;
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

            match current_batch.as_mut() {
                Some(batch)
                    if batch.can_append(&cell_style)
                        && batch.start_row == relative_line
                        && batch.start_col + batch.cell_count == cell.point.column.0 =>
                {
                    batch.append_char(ch);
                }
                _ => {
                    if let Some(batch) = current_batch.take() {
                        batched_runs.push(batch);
                    }
                    current_batch = Some(BatchedTextRun::new_from_char(
                        relative_line,
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
        usize, // cursor column
        usize, // cursor row
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

        if cols > 0 && rows > 0 {
            self.terminal.update(cx, |terminal, cx| {
                let current_size = terminal.size();
                if current_size.0 != cols || current_size.1 != rows {
                    terminal.resize(cols, rows, cx);
                }
            });
        }

        // Use cached last_content - AVOID CLONE by reading only what we need
        let terminal_ref = self.terminal.read(cx);
        let content = terminal_ref.last_content();
        let cells = content.cells.clone();
        let display_offset = content.display_offset;
        let cursor = content.cursor;
        let cursor_char = content.cursor_char;

        let batched_runs = Self::layout_grid(&cells, display_offset, &text_style, font_size);

        // Update global counters
        let run_count = PREPAINT_RUN_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        let total_cells = PREPAINT_TOTAL_CELLS.fetch_add(cells.len() as u64, Ordering::Relaxed)
            + cells.len() as u64;

        // Calculate text memory (String allocations in BatchedTextRun)
        let text_memory: usize = batched_runs.iter().map(|r| r.text.capacity()).sum();

        // Log prepaint stats
        eprintln!(
            "[Prepaint #{}] cells: {} (total processed: {}), batched_runs: {}, text_memory: {} bytes, avg cells/run: {:.1}",
            run_count,
            cells.len(),
            total_cells,
            batched_runs.len(),
            text_memory,
            if batched_runs.len() > 0 { cells.len() as f32 / batched_runs.len() as f32 } else { 0.0 }
        );

        (
            batched_runs,
            text_style,
            cell_width,
            line_height,
            font_size,
            cursor.column.0,
            cursor.line.0 as usize - display_offset,
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
        ) = prepaint_state;

        let origin = point(bounds.left() + px(4.0), bounds.top());

        // Paint batched text runs
        for batch in batched_runs.iter() {
            batch.paint(origin, *cell_width, *line_height, window, cx);
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
            _ => gpui::rgb(0xc0c0c0).into(),
        },
    }
}
