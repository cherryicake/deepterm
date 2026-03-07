//! Terminal element for GPUI rendering
//!
//! Implements the Element trait for custom terminal rendering
//! Based on Zed's terminal implementation

use gpui::{
    fill, outline, point, px, rgb, size, BorderStyle, Bounds, Element, ElementId, Entity,
    FocusHandle, Font, FontFallbacks, FontStyle, FontWeight, GlobalElementId, IntoElement,
    LayoutId, Pixels, Point, Style, TextRun, TextStyle, WhiteSpace, Window,
};

use crate::terminal::terminal::{IndexedCell, Terminal, TerminalContent};

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

        // Try to shape the line
        let shaped_line = window.text_system().shape_line(
            self.text.clone().into(),
            self.font_size,
            std::slice::from_ref(&self.style),
            Some(cell_width),
        );

        // Paint the shaped line
        let _ = shaped_line.paint(pos, line_height, gpui::TextAlign::Left, None, window, cx);
    }
}

/// Terminal element for GPUI
pub struct TerminalElement {
    terminal: Entity<Terminal>,
    focus: FocusHandle,
    focused: bool,
    cursor_visible: bool,
}

impl TerminalElement {
    /// Create a new terminal element
    pub fn new(
        terminal: Entity<Terminal>,
        focus: FocusHandle,
        focused: bool,
        cursor_visible: bool,
    ) -> Self {
        Self {
            terminal,
            focus,
            focused,
            cursor_visible,
        }
    }

    /// Layout the terminal grid into batched text runs
    fn layout_grid(
        cells: &[IndexedCell],
        text_style: &TextStyle,
        font_size: Pixels,
    ) -> Vec<BatchedTextRun> {
        let mut batched_runs = Vec::new();
        let mut current_batch: Option<BatchedTextRun> = None;

        // Group cells by row
        let mut row_cells: Vec<Vec<&IndexedCell>> = Vec::new();
        for cell in cells {
            if cell.row >= row_cells.len() {
                row_cells.resize(cell.row + 1, Vec::new());
            }
            row_cells[cell.row].push(cell);
        }

        for (row_idx, row) in row_cells.iter().enumerate() {
            // Flush any existing batch at line boundaries
            if let Some(batch) = current_batch.take() {
                batched_runs.push(batch);
            }

            for cell in row {
                if cell.ch == ' ' || cell.ch == '\0' {
                    // Skip blank cells but flush current batch
                    if let Some(batch) = current_batch.take() {
                        batched_runs.push(batch);
                    }
                    continue;
                }

                let cell_style = TextRun {
                    len: cell.ch.len_utf8(),
                    font: Font {
                        weight: if cell.bold {
                            FontWeight::BOLD
                        } else {
                            text_style.font_weight
                        },
                        style: if cell.italic {
                            FontStyle::Italic
                        } else {
                            FontStyle::Normal
                        },
                        ..text_style.font()
                    },
                    color: cell.fg_color,
                    background_color: cell.bg_color,
                    underline: if cell.underline {
                        Some(gpui::UnderlineStyle {
                            color: Some(cell.fg_color),
                            thickness: px(1.0),
                            wavy: false,
                        })
                    } else {
                        None
                    },
                    strikethrough: None,
                };

                // Try to batch with existing run
                if let Some(ref mut batch) = current_batch {
                    if batch.can_append(&cell_style)
                        && batch.start_row == cell.row
                        && batch.start_col + batch.cell_count == cell.col
                    {
                        batch.append_char(cell.ch);
                    } else {
                        // Flush current batch and start new one
                        let old_batch = current_batch.take().unwrap();
                        batched_runs.push(old_batch);
                        let new_batch = BatchedTextRun::new_from_char(
                            cell.row, cell.col, cell.ch, cell_style, font_size,
                        );
                        current_batch = Some(new_batch);
                    }
                } else {
                    // Start new batch
                    let new_batch = BatchedTextRun::new_from_char(
                        cell.row, cell.col, cell.ch, cell_style, font_size,
                    );
                    current_batch = Some(new_batch);
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
        TerminalContent,
        Vec<BatchedTextRun>,
        TextStyle,
        Pixels,
        Pixels,
        Pixels,
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
        // Request full size like Zed does
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
        // Calculate terminal dimensions based on bounds
        let _rem_size = window.rem_size();
        // Slightly larger font size than Ghostty (13px)
        let font_size = px(13.0);
        // Slightly more spacing for better readability
        let line_height = font_size * 1.25;

        // Use Noto Sans Mono CJK SC - same as system default monospace font
        let text_style = TextStyle {
            font_family: "Noto Sans Mono CJK SC".into(),
            font_features: Default::default(),
            font_weight: FontWeight::NORMAL,
            font_fallbacks: Some(FontFallbacks::from_fonts(vec![
                "Noto Sans Mono".to_string(),
                "Source Code Pro".to_string(),
                "Adwaita Mono".to_string(),
            ])),
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

        // Resize terminal if dimensions changed
        if cols > 0 && rows > 0 {
            self.terminal.update(cx, |terminal, cx| {
                let current_size = terminal.size();
                if current_size.0 != cols || current_size.1 != rows {
                    terminal.resize(cols, rows, cx);
                }
            });
        }

        // Get terminal content and cells
        let (content, cells) = self.terminal.update(cx, |terminal, _ctx| {
            (terminal.renderable_content(), terminal.renderable_cells())
        });

        // Layout grid into batched runs
        let batched_runs = Self::layout_grid(&cells, &text_style, font_size);

        (
            content,
            batched_runs,
            text_style,
            cell_width,
            line_height,
            font_size,
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
        let (content, batched_runs, _text_style, cell_width, line_height, _font_size) =
            prepaint_state;

        // Note: Terminal background is now handled by the Panel, not drawn here
        // This allows the Panel's background color to show through

        let origin = point(bounds.left() + px(4.0), bounds.top());

        // Paint batched text runs
        for batch in batched_runs {
            batch.paint(origin, *cell_width, *line_height, window, cx);
        }

        // Draw cursor
        if self.cursor_visible {
            let cursor_x = bounds.left() + px(4.0) + *cell_width * content.cursor.0 as f32;
            let cursor_y = bounds.top() + *line_height * content.cursor.1 as f32;

            let cursor_bounds =
                Bounds::new(point(cursor_x, cursor_y), size(*cell_width, *line_height));

            if self.focused {
                // Focused: solid cursor (Ghostty cursor color)
                window.paint_quad(fill(cursor_bounds, rgb(0xcba6f7)));
            } else {
                // Unfocused: hollow cursor (outline)
                window.paint_quad(outline(
                    cursor_bounds,
                    rgb(0xcba6f7),
                    BorderStyle::default(),
                ));
            }
        }
    }
}
