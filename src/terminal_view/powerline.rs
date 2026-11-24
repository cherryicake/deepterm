//! 内置 Powerline 分隔符绘制（U+E0B0–U+E0B7）。
//!
//! Powerline / Starship 提示符把这几个 PUA 码位当分段分隔符（实心三角、细线
//! 箭头、圆帽）。它们在字体里是**满格图形**：设计高度约 1.25–1.26 em，而单元格
//! 高度是 `font_size * terminal_line_height`（默认 1.3 em，可调到 3）。cell 一旦
//! 比字形高，字形上下就各留一条缝；缝里露出的是单元格自己的背景色，而分隔符
//! 单元格的背景属于**下一段**，于是分段边界上出现一条跟着行高变的浅色缝——
//! 也就是调整行高时提示符上冒出来的「毛刺」。行高调小时则是另一种走样：字形
//! 高出 cell，上下尖端被 `paint_line` 的 layer 裁剪削平。
//!
//! 更隐蔽的是两套像素取整：背景 quad 由 GPUI 吸附到设备像素（`snap_bounds`），
//! 字形基线在 `paint_glyph` 里吸附到整设备像素，两套相位不同，缝宽逐行跳变，
//! 因此同一屏里有的行明显、有的行几乎看不见。
//!
//! 这里与 [`super::box_drawing`] 一样绕开字体光栅化：把字体里的字形轮廓按
//! 「设计框 → 单元格」线性映射后自绘。横向贴住**已吸附的设备像素**边界
//! （与背景 quad 同一公式），纵向铺满整个 cell。字号、行高、DPI 怎么变，
//! 分隔符都与背景块严丝合缝。
//!
//! 只接管 0xE0B0–0xE0B7 这八个「满格」分隔符。0xE0A0 区是分支/锁等图标，
//! 0xE0B8 起是斜切半格形状，都不是满格分隔符，保持字体渲染。

use gpui::{Hsla, PathBuilder, Pixels, Point, Window, point, px};

// ─── 字形轮廓 ─────────────────────────────────────────────────────────────────

/// 字体等宽 advance：所有轮廓的横向设计宽度。
const DESIGN_ADVANCE: f32 = 500.0;
/// 字形设计框的纵向范围（字体里这几个码位的对齐基准 −292…972）。
///
/// 细线字形按它归一化：它们自己的轮廓是 −285…965，映射后保留字体自带的
/// 那点内缩，视觉粗细与字体原样一致。
const DESIGN_TOP: f32 = 972.0;
const DESIGN_BOTTOM: f32 = -292.0;

/// 轮廓上的一个点。`on_curve == false` 是 TrueType 的二次贝塞尔控制点。
///
/// 相邻两个控制点之间隐含一个 on-curve 中点（TrueType 语义），
/// [`push_outline`] 按同一规则补点，所以这里可以原样照抄字体表。
#[derive(Clone, Copy, Debug)]
struct OutlinePoint {
    x: f32,
    y: f32,
    on_curve: bool,
}

const fn on(x: f32, y: f32) -> OutlinePoint {
    OutlinePoint {
        x,
        y,
        on_curve: true,
    }
}

const fn off(x: f32, y: f32) -> OutlinePoint {
    OutlinePoint {
        x,
        y,
        on_curve: false,
    }
}

/// 右向实心三角（U+E0B0）：底边铺满 cell 高，尖端落在右侧垂直中线。
const RIGHT_TRIANGLE: &[OutlinePoint] = &[
    on(-30.0, 971.0),
    on(5.0, 971.0),
    on(500.0, 341.0),
    on(5.0, -291.0),
    on(-30.0, -291.0),
];

/// 左向实心三角（U+E0B2）：底边在右，尖端落在左侧垂直中线。
const LEFT_TRIANGLE: &[OutlinePoint] = &[
    on(495.0, -291.0),
    on(0.0, 341.0),
    on(495.0, 971.0),
    on(530.0, 971.0),
    on(530.0, -291.0),
];

/// 右向细线箭头（U+E0B1）：上下各内缩约 0.5%，中间是 0.17 格宽的斜带。
const RIGHT_CHEVRON: &[OutlinePoint] = &[
    on(0.0, 912.0),
    on(43.0, 965.0),
    on(500.0, 341.0),
    on(43.0, -285.0),
    on(0.0, -231.0),
    on(417.0, 341.0),
];

/// 左向细线箭头（U+E0B3）。
const LEFT_CHEVRON: &[OutlinePoint] = &[
    on(83.0, 341.0),
    on(500.0, -231.0),
    on(458.0, -285.0),
    on(0.0, 341.0),
    on(458.0, 965.0),
    on(500.0, 912.0),
];

/// 右向实心圆帽（U+E0B4）：底边铺满 cell 高，向右侧鼓到 cell 边界。
///
/// 尖端不是半圆：字体用两段二次贝塞尔在 `(500, 339)` 收口，控制点都在
/// x = 500 上，所以远边接近竖直、只在垂直中线处收成一个点。
const RIGHT_CAP: &[OutlinePoint] = &[
    on(-30.0, 971.0),
    on(-30.0, -291.0),
    on(5.0, -291.0),
    off(126.0, -291.0),
    on(252.0, -199.0),
    off(500.0, -20.0),
    on(500.0, 339.0),
    off(500.0, 701.0),
    on(252.0, 881.0),
    off(127.0, 971.0),
    on(5.0, 971.0),
];

/// 左向实心圆帽（U+E0B6）。
const LEFT_CAP: &[OutlinePoint] = &[
    on(495.0, 971.0),
    off(373.0, 971.0),
    on(248.0, 881.0),
    off(0.0, 701.0),
    off(0.0, -20.0),
    on(248.0, -199.0),
    off(374.0, -291.0),
    on(495.0, -291.0),
    on(530.0, -291.0),
    on(530.0, 971.0),
];

/// 右向细线圆帽（U+E0B5）：内外两条弧构成的弧形带，两端平头封口。
const RIGHT_THIN_CAP: &[OutlinePoint] = &[
    on(100.0, -285.0),
    off(228.0, -229.0),
    off(414.0, -20.0),
    off(500.0, 216.0),
    on(500.0, 339.0),
    off(500.0, 529.0),
    off(290.0, 881.0),
    on(100.0, 965.0),
    on(0.0, 965.0),
    off(190.0, 881.0),
    off(400.0, 529.0),
    on(400.0, 339.0),
    off(400.0, 150.0),
    off(190.0, -199.0),
    on(0.0, -285.0),
];

/// 左向细线圆帽（U+E0B7）。
const LEFT_THIN_CAP: &[OutlinePoint] = &[
    on(400.0, -285.0),
    off(272.0, -229.0),
    off(86.0, -20.0),
    off(0.0, 216.0),
    on(0.0, 339.0),
    off(0.0, 529.0),
    off(210.0, 881.0),
    on(400.0, 965.0),
    on(500.0, 965.0),
    off(310.0, 881.0),
    off(100.0, 529.0),
    on(100.0, 339.0),
    off(100.0, 150.0),
    off(310.0, -199.0),
    on(500.0, -285.0),
];

/// 该码位的纵向归一化范围 `(top, bottom)`。
///
/// 四个实心分隔符按**自身轮廓极值**归一化：它们的设计意图就是铺满整格，
/// 轮廓里的 971 / −291 与设计框 972 / −292 差 1 个单位只是字体取整残留，
/// 按设计框缩放会每侧留下 ~0.02px 的缝。细线字形按设计框归一化，保留
/// 字体自带的 7 单位内缩（相邻行的细箭头不该在视觉上连成竖线）。
fn v_extent(c: char) -> (f32, f32) {
    match c {
        '\u{E0B0}' | '\u{E0B2}' | '\u{E0B4}' | '\u{E0B6}' => (971.0, -291.0),
        _ => (DESIGN_TOP, DESIGN_BOTTOM),
    }
}

/// 八个受管码位的轮廓表。
fn outline_for(c: char) -> Option<&'static [OutlinePoint]> {
    match c {
        '\u{E0B0}' => Some(RIGHT_TRIANGLE),
        '\u{E0B1}' => Some(RIGHT_CHEVRON),
        '\u{E0B2}' => Some(LEFT_TRIANGLE),
        '\u{E0B3}' => Some(LEFT_CHEVRON),
        '\u{E0B4}' => Some(RIGHT_CAP),
        '\u{E0B5}' => Some(RIGHT_THIN_CAP),
        '\u{E0B6}' => Some(LEFT_CAP),
        '\u{E0B7}' => Some(LEFT_THIN_CAP),
        _ => None,
    }
}

/// 判断是否由本模块内置绘制。
pub fn is_powerline_char(c: char) -> bool {
    matches!(c as u32, 0xE0B0..=0xE0B7)
}

/// 把内置分隔符画到 `window` 上。
///
/// `origin` 是单元格左上角，`cell_w` / `cell_h` 是该终端的单元格尺寸（逻辑点）。
/// 形状只覆盖单元格自身，不越界——相邻 cell 的背景块不会被字形盖住。
pub fn paint_powerline(
    c: char,
    origin: Point<Pixels>,
    cell_w: Pixels,
    cell_h: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let Some(outline) = outline_for(c) else {
        return;
    };
    let frame = Frame::new(origin, cell_w, cell_h, window.scale_factor(), v_extent(c));
    let mut builder = PathBuilder::fill();
    push_outline(&mut builder, outline, &frame);

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// 一个分隔符单元格吸附到设备像素后的四边（逻辑点），外加该字形的纵向设计框。
///
/// 背景 quad 由 GPUI 在插入图元前用 `Window::snap_bounds` 吸附（四舍五入到
/// 设备像素、.5 向零取整）；自绘形状必须用同一公式。两边各按各的规则取整时，
/// 半个像素的差就会在拼接处露出底色——这是「毛刺」的另一半成因。
#[derive(Clone, Copy, Debug)]
struct Frame {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    design_top: f32,
    design_bottom: f32,
}

impl Frame {
    fn new(
        origin: Point<Pixels>,
        cell_w: Pixels,
        cell_h: Pixels,
        scale: f32,
        design_v_extent: (f32, f32),
    ) -> Self {
        let left = snap_to_device_pixel(origin.x.as_f32(), scale);
        let top = snap_to_device_pixel(origin.y.as_f32(), scale);
        // 与 GPUI 一样保证 far 边不小于 near 边（极窄单元格下 round 可能翻转）。
        let right = snap_to_device_pixel(origin.x.as_f32() + cell_w.as_f32(), scale).max(left);
        let bottom = snap_to_device_pixel(origin.y.as_f32() + cell_h.as_f32(), scale).max(top);
        Self {
            left,
            right,
            top,
            bottom,
            design_top: design_v_extent.0,
            design_bottom: design_v_extent.1,
        }
    }

    #[inline]
    fn width(&self) -> f32 {
        self.right - self.left
    }

    #[inline]
    fn height(&self) -> f32 {
        self.bottom - self.top
    }

    /// 把设计框坐标映射到吸附后的 cell 坐标。
    ///
    /// 横向按字体 advance 归一化并夹到 `[0, 1]`：字体把实心底边画到 −30 / 530
    /// （越界 6%）来压住相邻分段藏接缝，自绘不需要越界（设备像素吸附后本来
    /// 就没有缝），也不该盖到隔壁单元格。
    #[inline]
    fn map(&self, p: OutlinePoint) -> Point<Pixels> {
        let tx = (p.x / DESIGN_ADVANCE).clamp(0.0, 1.0);
        let ty = ((self.design_top - p.y) / (self.design_top - self.design_bottom)).clamp(0.0, 1.0);
        point(
            px(self.left + self.width() * tx),
            px(self.top + self.height() * ty),
        )
    }
}

/// 与 `round_to_device_pixel` 同一公式：四舍五入到设备像素，`.5` 向零取整。
#[inline]
fn snap_to_device_pixel(logical: f32, scale: f32) -> f32 {
    let scaled = logical * scale;
    (scaled.abs() - 0.5).ceil().copysign(scaled) / scale
}

/// 把一条 TrueType 轮廓按 cell 坐标铺进路径。
///
/// TrueType 允许连续出现控制点，此时相邻控制点的中点隐含为 on-curve 点；
/// 0xE0B5 的内外两段弧就用了这种写法，所以不能假设 on/off 交替。
fn push_outline(builder: &mut PathBuilder, points: &[OutlinePoint], frame: &Frame) {
    let Some(start_ix) = points.iter().position(|p| p.on_curve) else {
        return;
    };
    let start = frame.map(points[start_ix]);
    builder.move_to(start);

    let mut ctrl: Option<Point<Pixels>> = None;
    for step in 1..points.len() {
        let p = points[(start_ix + step) % points.len()];
        let mapped = frame.map(p);
        if p.on_curve {
            match ctrl.take() {
                Some(ctrl) => builder.curve_to(mapped, ctrl),
                None => builder.line_to(mapped),
            }
        } else if let Some(previous_ctrl) = ctrl.replace(mapped) {
            // 连续控制点：中点隐含为曲线终点。
            let mid = point(
                px((previous_ctrl.x.as_f32() + mapped.x.as_f32()) * 0.5),
                px((previous_ctrl.y.as_f32() + mapped.y.as_f32()) * 0.5),
            );
            builder.curve_to(mid, previous_ctrl);
        }
    }

    if let Some(ctrl) = ctrl {
        builder.curve_to(start, ctrl);
    }
    builder.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_for(c: char, origin: (f32, f32), cell_w: f32, cell_h: f32, scale: f32) -> Frame {
        Frame::new(
            point(px(origin.0), px(origin.1)),
            px(cell_w),
            px(cell_h),
            scale,
            v_extent(c),
        )
    }

    fn build(c: char, f: &Frame) -> gpui::Path<Pixels> {
        let mut builder = PathBuilder::fill();
        push_outline(&mut builder, outline_for(c).expect("受管码位"), f);
        builder.build().expect("轮廓应可填充")
    }

    #[test]
    fn covers_exactly_the_eight_separator_codepoints() {
        for code in 0xE0B0..=0xE0B7 {
            let c = char::from_u32(code).expect("PUA 码位合法");
            assert!(is_powerline_char(c), "{code:#x} 应内置绘制");
            assert!(outline_for(c).is_some(), "{code:#x} 缺少轮廓数据");
        }
        // 分支/锁等图标不是满格形状，必须留给字体；斜切半格同理。
        for code in [0xE0A0u32, 0xE0A1, 0xE0A2, 0xE0A3, 0xE0B8, 0xE0BC, 0xE0C0] {
            let c = char::from_u32(code).expect("PUA 码位合法");
            assert!(!is_powerline_char(c), "{code:#x} 不应内置绘制");
            assert!(outline_for(c).is_none());
        }
        assert!(!is_powerline_char('\u{2500}'));
    }

    /// 回归：行高 1.29 × 18px = 23.22pt，@2x 是 46.44 设备像素——正是毛刺出现
    /// 的非整数行高。四个实心分隔符必须**铺满吸附后的整个 cell**（四条边都贴着
    /// 背景 quad 的边），这是毛刺消失的充要条件。
    #[test]
    fn solid_separators_fill_the_snapped_cell() {
        let origin = (10.13, 20.13);
        let cell_w = 9.0;
        let cell_h = 23.22;
        let scale = 2.0;
        // 左边 10.13 → 20.26 → 20 → 10.0；右边 19.13 → 38.26 → 38 → 19.0。
        // 上边 20.13 → 40.26 → 40 → 20.0；下边 43.35 → 86.7 → 87 → 43.5。
        let (left, top, right, bottom) = (px(10.0), px(20.0), px(19.0), px(43.5));

        for &c in &['\u{E0B0}', '\u{E0B2}', '\u{E0B4}', '\u{E0B6}'] {
            let f = frame_for(c, origin, cell_w, cell_h, scale);
            let path = build(c, &f);
            assert_eq!(path.bounds.left(), left, "{c:?} 左边没贴住");
            assert_eq!(path.bounds.top(), top, "{c:?} 上边没贴住");
            assert_eq!(path.bounds.right(), right, "{c:?} 右边没贴住");
            assert_eq!(path.bounds.bottom(), bottom, "{c:?} 下边没贴住");
        }
    }

    /// 细线字形按字体设计内缩，必须完全落在 cell 内、且上下不贴边（贴边会让
    /// 相邻行的箭头在视觉上连成一条竖线）。
    #[test]
    fn thin_separators_stay_inside_the_cell() {
        for &c in &['\u{E0B1}', '\u{E0B3}', '\u{E0B5}', '\u{E0B7}'] {
            let f = frame_for(c, (0.0, 0.0), 9.0, 24.0, 2.0);
            let path = build(c, &f);
            assert!(
                path.bounds.left() >= px(0.0),
                "{c:?} 越界: {:?}",
                path.bounds
            );
            assert!(
                path.bounds.right() <= px(9.0),
                "{c:?} 越界: {:?}",
                path.bounds
            );
            assert!(path.bounds.top() > px(0.0), "{c:?} 越界: {:?}", path.bounds);
            assert!(
                path.bounds.bottom() < px(24.0),
                "{c:?} 越界: {:?}",
                path.bounds
            );
        }
    }

    /// 左向分隔符与右向是严格镜像：包围盒相同。
    #[test]
    fn left_facing_separators_mirror_right_facing_ones() {
        for pair in [
            ('\u{E0B0}', '\u{E0B2}'),
            ('\u{E0B1}', '\u{E0B3}'),
            ('\u{E0B4}', '\u{E0B6}'),
            ('\u{E0B5}', '\u{E0B7}'),
        ] {
            let (right, left) = pair;
            let right_path = build(right, &frame_for(right, (0.0, 0.0), 9.0, 24.0, 2.0));
            let left_path = build(left, &frame_for(left, (0.0, 0.0), 9.0, 24.0, 2.0));
            assert_eq!(left_path.bounds, right_path.bounds, "{left:?} 不镜像");
        }
    }

    /// 吸附公式与 GPUI 的 `round_to_device_pixel` 一致：.5 向零取整。
    #[test]
    fn snapping_matches_gpui_rounding() {
        assert_eq!(snap_to_device_pixel(1.2, 2.0), 1.0);
        // 1.25 * 2 = 2.5 → 向零取整到 2 → 1.0（不是 1.5）。
        assert_eq!(snap_to_device_pixel(1.25, 2.0), 1.0);
        // -0.76 * 2 = -1.52 → -2 → -1.0。
        assert_eq!(snap_to_device_pixel(-0.76, 2.0), -1.0);
        assert_eq!(snap_to_device_pixel(10.0, 1.0), 10.0);
    }
}
