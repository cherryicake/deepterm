//! 输入光标形状与布局，对标 Zed `CursorLayout` + VSCode `editor.cursorStyle`。
//!
//! - Zed (`crates/editor/src/element.rs` `CursorLayout`):
//!   按形状计算 bounds（Bar 2px 全高 / Block-Hollow 整字宽全高 / Underline 底部 2px），
//!   `paint` 走 `pixel_snap_bounds` 防模糊，Hollow 用 `outline`，Block 额外绘制反白字。
//! - VSCode (`editor.cursorStyle` / `editor.cursorWidth` / `editor.cursorBlinking`):
//!   line / block / underline 三种样式 + 可配宽度 + blink / solid 两种闪烁模式，
//!   失焦与 IME 组合输入时降级为空心或常显，避免干扰。

use gpui::{
    App, BorderStyle, Bounds, Hsla, Pixels, Point, ShapedLine, TextAlign, Window, fill, outline,
    point, px, size,
};

use super::blink_cursor::CURSOR_WIDTH;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Bar,
    Block,
    Underline,
    Hollow,
}

impl CursorShape {
    pub fn is_block_like(self) -> bool {
        matches!(self, CursorShape::Block | CursorShape::Hollow)
    }
}

#[derive(Clone)]
pub struct CursorLayout {
    origin: Point<Pixels>,
    block_width: Pixels,
    line_height: Pixels,
    color: Hsla,
    shape: CursorShape,
    block_text: Option<ShapedLine>,
}

impl CursorLayout {
    pub fn new(
        origin: Point<Pixels>,
        block_width: Pixels,
        line_height: Pixels,
        color: Hsla,
        shape: CursorShape,
        block_text: Option<ShapedLine>,
    ) -> Self {
        Self {
            origin,
            block_width,
            line_height,
            color,
            shape,
            block_text,
        }
    }

    pub fn origin(&self) -> Point<Pixels> {
        self.origin
    }

    pub fn shape(&self) -> CursorShape {
        self.shape
    }

    pub fn bounding_rect(&self) -> Bounds<Pixels> {
        Bounds {
            origin: self.origin,
            size: size(self.block_width, self.line_height),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        match self.shape {
            CursorShape::Bar => Bounds {
                origin: self.origin,
                size: size(CURSOR_WIDTH, self.line_height),
            },
            CursorShape::Block | CursorShape::Hollow => Bounds {
                origin: self.origin,
                size: size(self.block_width, self.line_height),
            },
            CursorShape::Underline => Bounds {
                origin: self.origin + point(px(0.), self.line_height - px(2.)),
                size: size(self.block_width, px(2.)),
            },
        }
    }

    pub fn shift_y(&mut self, dy: Pixels) {
        self.origin.y += dy;
    }

    pub fn paint(&mut self, window: &mut Window, _cx: &mut App) {
        let bounds = window.pixel_snap_bounds(self.bounds());
        let quad = if matches!(self.shape, CursorShape::Hollow) {
            outline(bounds, self.color, BorderStyle::Solid)
        } else {
            fill(bounds, self.color)
        };
        window.paint_quad(quad);
        if let Some(block_text) = &self.block_text {
            let _ = block_text.paint(
                self.origin,
                self.line_height,
                TextAlign::Left,
                None,
                window,
                _cx,
            );
        }
    }
}
