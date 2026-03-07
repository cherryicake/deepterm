//! Mouse input mappings for terminal
//!
//! Maps GPUI mouse events to terminal mouse sequences

use alacritty_terminal::index::Point;
use gpui::{MouseButton, ScrollDelta};

use super::keys::MouseMode;

/// Mouse button state
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseButtonState {
    Left,
    Middle,
    Right,
    Released,
}

/// Convert mouse button to terminal code
pub fn button_to_code(button: MouseButtonState, shift: bool) -> u8 {
    let mut code = match button {
        MouseButtonState::Left => 0,
        MouseButtonState::Middle => 1,
        MouseButtonState::Right => 2,
        MouseButtonState::Released => 3,
    };

    if shift {
        code += 4;
    }

    code
}

/// Encode mouse position for X10 mode
pub fn encode_x10(point: Point, button: u8) -> Vec<u8> {
    let mut result = vec![0x1b, b'[', b'M'];
    result.push(32 + button);
    result.push(32 + point.column.0 as u8 + 1);
    result.push(32 + point.line.0 as u8 + 1);
    result
}

/// Encode mouse position for SGR mode
pub fn encode_sgr(point: Point, button: u8, release: bool) -> Vec<u8> {
    let mut result = vec![0x1b, b'[', b'<'];
    result.extend_from_slice(button.to_string().as_bytes());
    result.push(b';');
    result.extend_from_slice((point.column.0 + 1).to_string().as_bytes());
    result.push(b';');
    result.extend_from_slice((point.line.0 + 1).to_string().as_bytes());
    result.push(if release { b'm' } else { b'M' });
    result
}

/// Encode scroll event
pub fn encode_scroll(
    point: Point,
    delta: &ScrollDelta,
    mode: MouseMode,
    shift: bool,
) -> Option<Vec<u8>> {
    use gpui::px;

    let is_positive = match delta {
        ScrollDelta::Lines(lines) => lines.y > 0.0,
        ScrollDelta::Pixels(pixels) => pixels.y > px(0.),
    };

    let button = if is_positive { 64 } else { 65 }; // 64 = scroll up, 65 = scroll down
    let button = if shift { button + 4 } else { button };

    match mode {
        MouseMode::Sgr => Some(encode_sgr(point, button, false)),
        MouseMode::Normal | MouseMode::X10 => Some(encode_x10(point, button)),
        _ => None,
    }
}

/// Convert GPUI mouse button to terminal button state
pub fn gpui_button_to_state(button: MouseButton) -> MouseButtonState {
    match button {
        MouseButton::Left => MouseButtonState::Left,
        MouseButton::Right => MouseButtonState::Right,
        MouseButton::Middle => MouseButtonState::Middle,
        _ => MouseButtonState::Released,
    }
}
