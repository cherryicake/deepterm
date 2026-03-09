//! Terminal element for rendering

use gpui::prelude::*;
use gpui::{
    fill, outline, point, px, rgb, size, BorderStyle, Bounds, Element, ElementId,
    ElementInputHandler, Entity, FocusHandle, Font, FontFallbacks, FontStyle, FontWeight,
    GlobalElementId, IntoElement, LayoutId, Pixels, Point, Style, TextRun, TextStyle, WhiteSpace,
    Window,
};
use itertools::Itertools;

use crate::terminal::mappings::colors::convert_alacritty_color;
use crate::terminal::terminal::{IndexedCell, Terminal, TerminalBounds};
use crate::terminal_view::terminal_view::TerminalView;

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

    fn layout_grid(
        grid: impl Iterator<Item = IndexedCell>,
        display_offset: usize,
        text_style: &TextStyle,
        font_size: Pixels,
        _visible_line_start: usize,
        _visible_line_count: usize,
    ) -> Vec<BatchedTextRun> {
        let estimated_cells = grid.size_hint().0;
        if estimated_cells == 0 {
            return Vec::new();
        }

        let estimated_runs = (estimated_cells / 10).max(16);
        let mut batched_runs = Vec::with_capacity(estimated_runs);
        let mut current_batch: Option<BatchedTextRun> = None;

        let line_groups = grid.into_iter().chunk_by(|cell| cell.point.line);

        for (line_index, (_, line_cells)) in line_groups.into_iter().enumerate() {
            let relative_row = line_index.saturating_sub(display_offset);

            if let Some(batch) = current_batch.take() {
                batched_runs.push(batch);
            }

            for cell in line_cells {
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
        usize,
        usize,
        Option<String>,
        TerminalBounds,
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

        let terminal_bounds = TerminalBounds::new(line_height, cell_width, bounds);

        // Sync terminal and handle resize
        let (display_offset, cursor) = self.terminal.update(cx, |terminal, cx| {
            terminal.sync(window, cx);

            let current_bounds = terminal.terminal_bounds();
            if current_bounds.num_columns() != terminal_bounds.num_columns()
                || current_bounds.num_lines() != terminal_bounds.num_lines()
            {
                terminal.resize(terminal_bounds, cx);
            }
            terminal.set_terminal_bounds(terminal_bounds);

            let content = terminal.last_content();
            (content.display_offset, content.cursor)
        });

        // Calculate visible viewport
        let visible_bounds = window.content_mask().bounds;
        let intersection = visible_bounds.intersect(&bounds);

        let visible_line_start = if intersection.size.height > px(0.0) {
            ((intersection.top() - bounds.top()).max(px(0.0)) / line_height).floor() as usize
        } else {
            0
        };
        let visible_line_count = if intersection.size.height > px(0.0) {
            ((intersection.size.height / line_height).ceil() as usize).max(1)
        } else {
            terminal_bounds.num_lines()
        };

        let content = self.terminal.read(cx).last_content();
        let batched_runs = Self::layout_grid(
            content.cells.iter().cloned(),
            display_offset,
            &text_style,
            font_size,
            visible_line_start,
            visible_line_count,
        );

        let cursor_row = (cursor.line.0 as usize).saturating_sub(display_offset);
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
            terminal_bounds,
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
            _selection,
            terminal_bounds,
        ) = prepaint_state;

        let origin = point(bounds.left() + px(4.0), bounds.top());

        // Paint background
        let bg_bounds = Bounds::new(
            origin,
            size(terminal_bounds.width() - px(8.0), terminal_bounds.height()),
        );
        window.paint_quad(fill(bg_bounds, rgb(0x1e1e2e)));

        // Get terminal and content for selection checking
        let (selection_cells, content, mouse_mode) = {
            let term = self.terminal.read(cx);
            let content = term.last_content();
            let mut selection_cells = Vec::new();

            // Check if terminal is in mouse mode (e.g., neovim mouse mode)
            // In mouse mode, the application handles selection itself, so we shouldn't draw our own
            let mouse_mode = content
                .mode
                .intersects(alacritty_terminal::term::TermMode::MOUSE_MODE);

            // Only draw terminal selection if not in mouse mode
            if !mouse_mode {
                for cell in content.cells.iter() {
                    if term.is_cell_selected(&cell.point) {
                        selection_cells.push(cell.point);
                    }
                }
            }
            (selection_cells, content, mouse_mode)
        };

        // Paint selection backgrounds first (only if not in mouse mode)
        if !mouse_mode {
            for cell_point in &selection_cells {
                let cell_x = bounds.left() + px(4.0) + *cell_width * cell_point.column.0 as f32;
                let cell_y = bounds.top()
                    + *line_height
                        * (cell_point.line.0 as usize).saturating_sub(content.display_offset)
                            as f32;

                let cell_bounds =
                    Bounds::new(point(cell_x, cell_y), size(*cell_width, *line_height));

                window.paint_quad(fill(cell_bounds, gpui::rgb(0x4a4a6a)));
            }
        }

        // Paint cell backgrounds first (for cells with non-default background)
        {
            let term = self.terminal.read(cx);
            let content = term.last_content();

            for cell in content.cells.iter() {
                // Check if cell has non-default background
                let bg_color = if cell.cell.bg
                    != alacritty_terminal::vte::ansi::Color::Named(
                        alacritty_terminal::vte::ansi::NamedColor::Background,
                    ) {
                    Some(convert_alacritty_color(&cell.cell.bg))
                } else {
                    None
                };

                if let Some(bg_color) = bg_color {
                    let cell_x = bounds.left() + px(4.0) + *cell_width * cell.point.column.0 as f32;
                    let cell_y = bounds.top()
                        + *line_height
                            * (cell.point.line.0 as usize).saturating_sub(content.display_offset)
                                as f32;

                    let cell_bounds =
                        Bounds::new(point(cell_x, cell_y), size(*cell_width, *line_height));

                    window.paint_quad(fill(cell_bounds, bg_color));
                }
            }
        }

        // Paint batched text runs (text will appear on top of backgrounds)
        for batch in batched_runs.iter() {
            batch.paint(origin, *cell_width, *line_height, window, cx);
        }

        // Paint cursor
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
