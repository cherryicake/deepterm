//! 输入光标形状与布局，对标 Zed `CursorLayout` + VSCode `editor.cursorStyle`。
//!
//! - Zed (`crates/editor/src/element.rs` `CursorLayout`):
//!   按形状计算 bounds（Bar 2px 全高 / Block-Hollow 整字宽全高 / Underline 底部 2px），
//!   `paint` 走 `pixel_snap_bounds` 防模糊，Hollow 用 `outline`，Block 额外绘制反白字。
//! - VSCode (`editor.cursorStyle` / `editor.cursorWidth` / `editor.cursorBlinking`):
//!   line / block / underline 三种样式 + 可配宽度 + blink / solid 两种闪烁模式，
//!   失焦与 IME 组合输入时降级为空心或常显，避免干扰。
//!
//! 绘制走 [`CursorQuad`]（四角弹簧动画的输出）：静止时是轴对齐矩形，
//! 移动过程中被剪切成平行四边形——与终端光标同一套观感。

use gpui::{App, Bounds, Hsla, Pixels, Point, ShapedLine, TextAlign, Window, point, px, size};

use super::blink_cursor::CURSOR_WIDTH;
use crate::components::cursor_animation::{CursorQuad, CursorShape as AnimCursorShape};

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

impl From<CursorShape> for AnimCursorShape {
    fn from(shape: CursorShape) -> Self {
        match shape {
            CursorShape::Bar => AnimCursorShape::Bar,
            CursorShape::Block => AnimCursorShape::Block,
            CursorShape::Underline => AnimCursorShape::Underline,
            CursorShape::Hollow => AnimCursorShape::Hollow,
        }
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
    /// 本帧的动画四边形（四角弹簧输出）。
    ///
    /// `None` = 尚未推进（静态回退：按形状直接算轴对齐矩形），
    /// 便于不关心动画的调用方（如测量、IME 定位）继续用 `bounding_rect()`。
    quad: Option<CursorQuad>,
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
            quad: None,
        }
    }

    /// 带上动画四边形（由 `CursorAnimator` 算出，**屏幕坐标**）。
    ///
    /// 反白字符画在 `origin` 上，而 origin 只含 x 滚动——绘制前用四边形
    /// 的基准点与 origin 对齐，保证字形跟光标本体一起走。
    pub fn with_quad(mut self, quad: CursorQuad) -> Self {
        self.quad = Some(quad);
        self
    }

    /// 本帧四边形（没有动画时按形状合成轴对齐矩形）。
    pub fn quad(&self) -> CursorQuad {
        self.quad.unwrap_or_else(|| {
            let rect = self.bounds();
            CursorQuad {
                corners: [
                    (rect.left().as_f32(), rect.top().as_f32()),
                    (rect.right().as_f32(), rect.top().as_f32()),
                    (rect.right().as_f32(), rect.bottom().as_f32()),
                    (rect.left().as_f32(), rect.bottom().as_f32()),
                ],
                destination: point(rect.left().as_f32(), rect.top().as_f32()),
            }
        })
    }

    pub fn origin(&self) -> Point<Pixels> {
        self.origin
    }

    pub fn shape(&self) -> CursorShape {
        self.shape
    }

    pub fn block_width(&self) -> Pixels {
        self.block_width
    }

    pub fn line_height(&self) -> Pixels {
        self.line_height
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
        self.shift_by(point(px(0.), dy));
    }

    /// 平移光标（origin 与动画四边形一起），用于把内容坐标折成屏幕坐标。
    pub fn shift_by(&mut self, delta: Point<Pixels>) {
        self.origin.x += delta.x;
        self.origin.y += delta.y;
        if let Some(quad) = self.quad.as_mut() {
            for (x, y) in quad.corners.iter_mut() {
                *x += delta.x.as_f32();
                *y += delta.y.as_f32();
            }
            quad.destination.x += delta.x.as_f32();
            quad.destination.y += delta.y.as_f32();
        }
    }

    /// 动画四边形是否已就位（用于判断是否需要额外的滚动平移）。
    pub fn has_quad(&self) -> bool {
        self.quad.is_some()
    }

    pub fn paint(&mut self, window: &mut Window, cx: &mut App) {
        self.paint_with_opacity(1.0, window, cx);
    }

    /// 带不透明度绘制（闪烁淡出 / 淡入）。
    ///
    /// `opacity` 同时作用于光标本体与反白字符，避免半透明块上浮着
    /// 一个实心反白字。
    pub fn paint_with_opacity(&mut self, opacity: f32, window: &mut Window, cx: &mut App) {
        let opacity = opacity.clamp(0.0, 1.0);
        let color = self.color.opacity(opacity);
        let quad = self.quad();
        if matches!(self.shape, CursorShape::Hollow) {
            quad.paint_outline(window, 1.0, color);
        } else {
            quad.paint_fill(window, color);
        }
        if matches!(self.shape, CursorShape::Block)
            && let Some(block_text) = &self.block_text
        {
            // 反白字符：与终端一致，画在静止目标上并裁到四边形包围盒内，
            // 移动过程中字形随光标逐步显露而不是先浮现在目标位置。
            let clip = gpui::ContentMask {
                bounds: quad.bounding_box(),
            };
            window.with_content_mask(Some(clip), |window| {
                let _ = block_text.paint(
                    self.origin,
                    self.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            });
        }
    }
}
