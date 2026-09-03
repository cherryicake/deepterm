//! Mouse input mappings for terminal

use alacritty_terminal::index::{Column as GridCol, Line as GridLine, Point as AlacPoint, Side};
use alacritty_terminal::term::TermMode;
use gpui::{Modifiers, MouseButton, Pixels, Point, ScrollWheelEvent, px};

use crate::terminal::terminal::TerminalBounds;

#[derive(Clone, Copy, Debug)]
enum MouseFormat {
    Sgr,
    Normal(bool),
}

impl MouseFormat {
    fn from_mode(mode: TermMode) -> Self {
        if mode.contains(TermMode::SGR_MOUSE) {
            MouseFormat::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseFormat::Normal(true)
        } else {
            MouseFormat::Normal(false)
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum AlacMouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

impl AlacMouseButton {
    fn from_move_button(e: Option<MouseButton>) -> Self {
        match e {
            Some(MouseButton::Left) => AlacMouseButton::LeftMove,
            Some(MouseButton::Middle) => AlacMouseButton::MiddleMove,
            Some(MouseButton::Right) => AlacMouseButton::RightMove,
            Some(MouseButton::Navigate(_)) => AlacMouseButton::Other,
            None => AlacMouseButton::NoneMove,
        }
    }

    fn from_button(e: MouseButton) -> Self {
        match e {
            MouseButton::Left => AlacMouseButton::LeftButton,
            MouseButton::Middle => AlacMouseButton::MiddleButton,
            MouseButton::Right => AlacMouseButton::RightButton,
            MouseButton::Navigate(_) => AlacMouseButton::Other,
        }
    }

    fn from_scroll(e: &ScrollWheelEvent) -> Self {
        let is_positive = match e.delta {
            gpui::ScrollDelta::Pixels(pixels) => pixels.y > px(0.),
            gpui::ScrollDelta::Lines(lines) => lines.y > 0.,
        };

        if is_positive {
            AlacMouseButton::ScrollUp
        } else {
            AlacMouseButton::ScrollDown
        }
    }

    fn is_other(&self) -> bool {
        matches!(self, AlacMouseButton::Other)
    }
}

pub fn scroll_report(
    point: AlacPoint,
    scroll_lines: i32,
    e: &ScrollWheelEvent,
    mode: TermMode,
) -> Option<impl Iterator<Item = Vec<u8>>> {
    if mode.intersects(TermMode::MOUSE_MODE) {
        mouse_report(
            point,
            AlacMouseButton::from_scroll(e),
            true,
            e.modifiers,
            MouseFormat::from_mode(mode),
        )
        .map(|report| std::iter::repeat_n(report, scroll_lines.unsigned_abs() as usize))
    } else {
        None
    }
}

pub fn alt_scroll(scroll_lines: i32) -> Vec<u8> {
    let cmd = if scroll_lines > 0 { b'A' } else { b'B' };

    let mut content = Vec::with_capacity(scroll_lines.unsigned_abs() as usize * 3);
    for _ in 0..scroll_lines.abs() {
        content.push(0x1b);
        content.push(b'O');
        content.push(cmd);
    }
    content
}

pub fn mouse_button_report(
    point: AlacPoint,
    button: MouseButton,
    modifiers: Modifiers,
    pressed: bool,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let button = AlacMouseButton::from_button(button);
    if !button.is_other() && mode.intersects(TermMode::MOUSE_MODE) {
        mouse_report(
            point,
            button,
            pressed,
            modifiers,
            MouseFormat::from_mode(mode),
        )
    } else {
        None
    }
}

pub fn mouse_moved_report(
    point: AlacPoint,
    button: Option<MouseButton>,
    modifiers: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let button = AlacMouseButton::from_move_button(button);

    if !button.is_other() && mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG) {
        if mode.contains(TermMode::MOUSE_DRAG) && matches!(button, AlacMouseButton::NoneMove) {
            None
        } else {
            mouse_report(point, button, true, modifiers, MouseFormat::from_mode(mode))
        }
    } else {
        None
    }
}

pub fn grid_point(pos: Point<Pixels>, bounds: TerminalBounds, display_offset: usize) -> AlacPoint {
    grid_point_and_side(pos, bounds, display_offset).0
}

pub fn grid_point_and_side(
    pos: Point<Pixels>,
    bounds: TerminalBounds,
    display_offset: usize,
) -> (AlacPoint, Side) {
    let cell_width = bounds.cell_width;
    let line_height = bounds.line_height;

    let mut col = GridCol((pos.x.as_f32() / cell_width.as_f32()).floor() as usize);
    let cell_x = pos.x.as_f32().max(0.0) % cell_width.as_f32();
    let half_cell_width = cell_width.as_f32() / 2.0;
    let mut side = if cell_x > half_cell_width {
        Side::Right
    } else {
        Side::Left
    };

    let last_col = GridCol(bounds.num_columns().saturating_sub(1));
    if col > last_col {
        col = last_col;
        side = Side::Right;
    }

    // Calculate line number based on mouse position
    // The display_offset represents how many lines we've scrolled up in the scrollback buffer
    // So we need to SUBTRACT it to get the actual line in the terminal's grid
    let mut line = (pos.y.as_f32() / line_height.as_f32()).floor() as i32;
    let bottommost = bounds.num_lines().saturating_sub(1) as i32;

    // Clamp line to visible area first
    if line > bottommost {
        line = bottommost;
        side = Side::Right;
    } else if line < 0 {
        line = 0;
        side = Side::Left;
    }

    // Subtract display_offset to get the actual line in the terminal grid
    // When scrolled up, display_offset > 0, so screen line 0 maps to grid line -display_offset
    let actual_line = line - display_offset as i32;

    (AlacPoint::new(GridLine(actual_line), col), side)
}

fn mouse_report(
    point: AlacPoint,
    button: AlacMouseButton,
    pressed: bool,
    modifiers: Modifiers,
    format: MouseFormat,
) -> Option<Vec<u8>> {
    if point.line.0 < 0 {
        return None;
    }

    let mut mods = 0;
    if modifiers.shift {
        mods += 4;
    }
    if modifiers.alt {
        mods += 8;
    }
    if modifiers.control {
        mods += 16;
    }

    match format {
        MouseFormat::Sgr => {
            Some(sgr_mouse_report(point, button as u8 + mods, pressed).into_bytes())
        }
        MouseFormat::Normal(utf8) => {
            if pressed {
                normal_mouse_report(point, button as u8 + mods, utf8)
            } else {
                normal_mouse_report(point, 3 + mods, utf8)
            }
        }
    }
}

fn normal_mouse_report(point: AlacPoint, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let AlacPoint { line, column } = point;
    let max_point = if utf8 { 2015 } else { 223 };

    if line.0 >= max_point || column.0 >= max_point as usize {
        return None;
    }

    let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

    let mouse_pos_encode = |pos: usize| -> Vec<u8> {
        let pos = 32 + 1 + pos;
        let first = 0xC0 + pos / 64;
        let second = 0x80 + (pos & 63);
        vec![first as u8, second as u8]
    };

    if utf8 && column.0 >= 95 {
        msg.append(&mut mouse_pos_encode(column.0));
    } else {
        msg.push(32 + 1 + column.0 as u8);
    }

    if utf8 && line.0 >= 95 {
        msg.append(&mut mouse_pos_encode(line.0 as usize));
    } else {
        msg.push(32 + 1 + line.0 as u8);
    }

    Some(msg)
}

fn sgr_mouse_report(point: AlacPoint, button: u8, pressed: bool) -> String {
    let c = if pressed { 'M' } else { 'm' };

    format!(
        "\x1b[<{};{};{}{}",
        button,
        point.column.0 + 1,
        point.line.0 + 1,
        c
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, point, px, size};

    fn test_bounds(cols: usize, lines: usize) -> TerminalBounds {
        // 10x20 px cells, origin at (100, 50) in window coordinates.
        TerminalBounds {
            cell_width: px(10.0),
            line_height: px(20.0),
            bounds: Bounds::new(
                point(px(100.0), px(50.0)),
                size(px(cols as f32 * 10.0), px(lines as f32 * 20.0)),
            ),
        }
    }

    /// 坐标是相对终端内容区域原点的，但 grid_point_and_side 直接用
    /// `pos.x / cell_width` 计算——传入相对坐标。
    fn relative(pos_x: f32, pos_y: f32) -> Point<Pixels> {
        point(px(pos_x), px(pos_y))
    }

    #[test]
    fn grid_point_maps_center_of_first_cell() {
        let bounds = test_bounds(80, 24);
        let (point, side) = grid_point_and_side(relative(4.0, 10.0), bounds, 0);
        assert_eq!(point, AlacPoint::new(GridLine(0), GridCol(0)));
        assert_eq!(side, Side::Left);
    }

    #[test]
    fn grid_point_second_half_of_cell_is_right_side() {
        let bounds = test_bounds(80, 24);
        // x=6 处于第 0 列（0..10）的右半格。
        let (point, side) = grid_point_and_side(relative(6.0, 10.0), bounds, 0);
        assert_eq!(point, AlacPoint::new(GridLine(0), GridCol(0)));
        assert_eq!(side, Side::Right);
    }

    #[test]
    fn grid_point_clamps_to_last_column() {
        let bounds = test_bounds(80, 24);
        // 超出最后一列的横向位置钳制到 79 列右侧。
        let (point, side) = grid_point_and_side(relative(2000.0, 10.0), bounds, 0);
        assert_eq!(point, AlacPoint::new(GridLine(0), GridCol(79)));
        assert_eq!(side, Side::Right);
    }

    #[test]
    fn grid_point_clamps_above_viewport() {
        let bounds = test_bounds(80, 24);
        // 视口上方（负 y）钳制到第 0 行左侧——拖拽越过上边缘时选区头部
        // 停在顶行行首。
        let (point, side) = grid_point_and_side(relative(400.0, -30.0), bounds, 0);
        assert_eq!(point, AlacPoint::new(GridLine(0), GridCol(40)));
        assert_eq!(side, Side::Left);
    }

    #[test]
    fn grid_point_clamps_below_viewport() {
        let bounds = test_bounds(80, 24);
        // 视口下方钳制到最后一行右侧——拖拽越过下边缘时选区头部停在
        // 底行行尾。
        let (point, side) = grid_point_and_side(relative(400.0, 1000.0), bounds, 0);
        assert_eq!(point, AlacPoint::new(GridLine(23), GridCol(40)));
        assert_eq!(side, Side::Right);
    }

    #[test]
    fn grid_point_applies_display_offset() {
        let bounds = test_bounds(80, 24);
        // 向上滚 5 行（display_offset=5）：视口第 0 行对应 grid 第 -5 行。
        let (point, _) = grid_point_and_side(relative(4.0, 10.0), bounds, 5);
        assert_eq!(point, AlacPoint::new(GridLine(-5), GridCol(0)));

        // 视口第 7 行对应 grid 第 2 行。
        let (point, _) = grid_point_and_side(relative(4.0, 150.0), bounds, 5);
        assert_eq!(point, AlacPoint::new(GridLine(2), GridCol(0)));
    }
}
