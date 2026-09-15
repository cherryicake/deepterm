//! Mouse input mappings for terminal

use alacritty_terminal::index::{Column as GridCol, Line as GridLine, Point as AlacPoint, Side};
use alacritty_terminal::term::TermMode;
use gpui::{Modifiers, MouseButton, Pixels, Point, ScrollWheelEvent, px};

use crate::terminal::terminal::TerminalBounds;

#[derive(Clone, Copy, Debug)]
enum MouseFormat {
    Sgr,
    /// SGR-Pixels（模式 1016）：坐标是**终端空间像素**而非单元格。
    SgrPixels,
    Normal(bool),
}

impl MouseFormat {
    /// 由终端模式推导编码格式。
    ///
    /// `sgr_pixels` 无法从 `TermMode` 得到——模式 1016 在 vte/alacritty 的
    /// 枚举里不存在，由 deepterm 自己跟踪后显式传入（见调用方的
    /// `sgr_pixels` 参数）。
    fn from_mode(mode: TermMode, sgr_pixels: bool) -> Self {
        if sgr_pixels {
            MouseFormat::SgrPixels
        } else if mode.contains(TermMode::SGR_MOUSE) {
            MouseFormat::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseFormat::Normal(true)
        } else {
            MouseFormat::Normal(false)
        }
    }
}

#[derive(Debug)]
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
    pixel_pos: Point<Pixels>,
    scroll_lines: i32,
    e: &ScrollWheelEvent,
    mode: TermMode,
    sgr_pixels: bool,
    scale: f32,
) -> Option<impl Iterator<Item = Vec<u8>>> {
    if mode.intersects(TermMode::MOUSE_MODE) {
        mouse_report(
            point,
            pixel_pos,
            AlacMouseButton::from_scroll(e),
            true,
            e.modifiers,
            MouseFormat::from_mode(mode, sgr_pixels),
            scale,
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
    pixel_pos: Point<Pixels>,
    button: MouseButton,
    modifiers: Modifiers,
    pressed: bool,
    mode: TermMode,
    sgr_pixels: bool,
    scale: f32,
) -> Option<Vec<u8>> {
    let button = AlacMouseButton::from_button(button);
    if !button.is_other() && mode.intersects(TermMode::MOUSE_MODE) {
        mouse_report(
            point,
            pixel_pos,
            button,
            pressed,
            modifiers,
            MouseFormat::from_mode(mode, sgr_pixels),
            scale,
        )
    } else {
        None
    }
}

pub fn mouse_moved_report(
    point: AlacPoint,
    pixel_pos: Point<Pixels>,
    button: Option<MouseButton>,
    modifiers: Modifiers,
    mode: TermMode,
    sgr_pixels: bool,
    scale: f32,
) -> Option<Vec<u8>> {
    let button = AlacMouseButton::from_move_button(button);

    if !button.is_other() && mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG) {
        if mode.contains(TermMode::MOUSE_DRAG) && matches!(button, AlacMouseButton::NoneMove) {
            None
        } else {
            mouse_report(
                point,
                pixel_pos,
                button,
                true,
                modifiers,
                MouseFormat::from_mode(mode, sgr_pixels),
                scale,
            )
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
    pixel_pos: Point<Pixels>,
    button: AlacMouseButton,
    pressed: bool,
    modifiers: Modifiers,
    format: MouseFormat,
    scale: f32,
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
        // SGR-Pixels：与 SGR 同形，只把坐标换成像素。release 保持按钮标识
        // （`'m'` + 原按钮号），与 ghostty `mouse_encode.zig:207-213` 一致——
        // 那里显式豁免了 `.sgr`/`.sgr_pixels` 的「release 一律写成 3」规则。
        MouseFormat::SgrPixels => Some(sgr_pixels_report(
            pixel_pos,
            button as u8 + mods,
            pressed,
            scale,
        )),
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

/// SGR-Pixels 报文：`\x1b[<{button};{x};{y}{M|m}`，坐标为终端空间像素。
///
/// 逐字节对齐 ghostty `input/mouse_encode.zig:164-172` + `posToPixels`
/// （`:268-282`）：
/// - 坐标是**终端空间像素**（相对内容区原点），**不做单元格换算**；
/// - **不钳制**，允许为负或超出终端尺寸（ghostty 注释明示「Unlike grid
///   conversion, terminal-space coordinates are not clamped」）；
/// - 四舍五入（`@round`），不是向下取整。
///
/// `scale` 是 `Window::scale_factor()`：GPUI 的鼠标位置是**逻辑点**，而本
/// 模式要报**物理像素**，两者相差正好这个系数（Retina 上 2.0）。漏乘会让
/// 客户端拿到一半的坐标、点击落到指针左上角且偏移随距离放大。
///
/// 注意与 `sgr_mouse_report` 的差别：这里没有 `+1`。SGR 用 1-based 单元格
/// 索引，而像素坐标是 0-based 距离值，ghostty 对两者同样不加偏移。
fn sgr_pixels_report(pixel_pos: Point<Pixels>, button: u8, pressed: bool, scale: f32) -> Vec<u8> {
    let c = if pressed { 'M' } else { 'm' };
    let x = (pixel_pos.x.as_f32() * scale).round() as i32;
    let y = (pixel_pos.y.as_f32() * scale).round() as i32;

    format!("\x1b[<{};{};{}{}", button, x, y, c).into_bytes()
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

    // ===== 模式 1016：SGR-Pixels =====

    /// 锚定 ghostty `mouse_encode.zig` 的 `sgr_pixels` 契约：
    /// `\x1b[<{button};{x_px};{y_px}{M|m}`，像素坐标、无 `+1`、四舍五入。
    #[test]
    fn sgr_pixels_report_uses_pixel_coords_without_offset() {
        let out = sgr_pixels_report(relative(13.6, 47.2), 0, true, 1.0);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[<0;14;47M",
            "像素坐标应四舍五入且不加 1（与 SGR 的 1-based 单元格不同）"
        );
    }

    #[test]
    fn sgr_pixels_release_keeps_button_identity() {
        // ghostty `mouse_encode.zig:207-213`：`.sgr_pixels` 豁免「release 写成
        // 按钮 3」的规则，因此右键 release 仍是 2 而不是 3。
        let out = sgr_pixels_report(relative(5.0, 5.0), 2, false, 1.0);
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[<2;5;5m");
    }

    #[test]
    fn sgr_pixels_does_not_clamp_negative_coords() {
        // ghostty 明示像素坐标不钳制：拖出左上角应得到负值。
        let out = sgr_pixels_report(relative(-12.0, -3.0), 0, true, 1.0);
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[<0;-12;-3M");
    }

    /// GPUI 的鼠标位置是**逻辑点**，而 1016 要报**物理像素**。漏乘缩放系数会
    /// 让客户端拿到一半坐标（Retina 2x），点击落到指针左上角且偏移随距离放大。
    #[test]
    fn sgr_pixels_scales_logical_points_to_physical_pixels() {
        let out = sgr_pixels_report(relative(13.6, 47.2), 0, true, 2.0);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[<0;27;94M",
            "Retina(2x) 下 13.6pt/47.2pt 应报 27px/94px"
        );
    }

    /// 缩放要先乘再取整：13.5 * 2 = 27.0（若先取整乘缩放则不成立）。
    #[test]
    fn sgr_pixels_multiplies_before_rounding() {
        let out = sgr_pixels_report(relative(13.5, 0.0), 0, true, 2.0);
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[<0;27;0M");
    }

    /// 端到端：1016 置位时 `mouse_button_report` 报出的是缩放后的像素坐标，
    /// 而不是单元格索引。10x20 的格子在 2x 下每格 20x40 物理像素。
    #[test]
    fn mouse_button_report_sgr_pixels_end_to_end() {
        let bounds = test_bounds(80, 24);
        let pos = relative(25.0, 45.0);
        let point = grid_point(pos, bounds, 0);
        assert_eq!(point, AlacPoint::new(GridLine(2), GridCol(2)));

        let bytes = mouse_button_report(
            point,
            pos,
            MouseButton::Left,
            Modifiers::default(),
            true,
            TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE,
            true,
            2.0,
        )
        .expect("1016 激活时应产生报文");

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b[<0;50;90M",
            "应报像素(50,90)而非单元格(3,3)"
        );
    }

    #[test]
    fn sgr_pixels_takes_priority_over_sgr_mouse_mode() {
        // 1016 置位时即使 SGR_MOUSE 也在，仍走像素编码（ghostty 中 1016 是
        // 独立的 encoding 枚举值，优先级高于 sgr）。
        assert!(matches!(
            MouseFormat::from_mode(TermMode::SGR_MOUSE, true),
            MouseFormat::SgrPixels
        ));
        // 未置位时不受影响。
        assert!(matches!(
            MouseFormat::from_mode(TermMode::SGR_MOUSE, false),
            MouseFormat::Sgr
        ));
    }
}
