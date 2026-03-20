//! 内置盒子绘制字符和块元素渲染。
//!
//! 对 Unicode 盒子绘制字符（U+2500–U+257F）和块元素（U+2580–U+259F）
//! 使用 GPUI 矩形图元直接绘制，而不依赖字体光栅化。
//!
//! 这样做的好处：无论当前字体如何，相邻单元格的边框线条都能完美对齐，
//! 不会出现字体字形带来的间隙或粗细不一致问题。
//! 对应 Alacritty 的 `builtin_box_drawing = true` 配置项。

use gpui::{Bounds, Hsla, Pixels, Point, Window, fill, point, px, size};

// ─── 线段编码方案 ─────────────────────────────────────────────────────────────
// 每个盒子绘制字符用一个 u8 位域描述其四个方向的线段粗细：
//   bits [1:0] = 左方向线段粗细
//   bits [3:2] = 右方向线段粗细
//   bits [5:4] = 上方向线段粗细
//   bits [7:6] = 下方向线段粗细
//
// 粗细取值：0=无  1=细线(Thin)  2=粗线(Heavy)  3=双线(Double)

const NONE: u8 = 0;
const THIN: u8 = 1;
const HEAVY: u8 = 2;
const DBL: u8 = 3;

const fn enc(l: u8, r: u8, u: u8, d: u8) -> u8 {
    l | (r << 2) | (u << 4) | (d << 6)
}

/// U+2500–U+257F 盒子绘制字符的线段查找表（共 128 项）。
/// 每项用 enc(左, 右, 上, 下) 描述该字符各方向的线段粗细。
#[rustfmt::skip]
const BOX_SEGS: [u8; 128] = [
    // ─ ━ │ ┃ ┄ ┅ ┆ ┇ ┈ ┉ ┊ ┋
    enc(THIN,THIN,NONE,NONE), enc(HEAVY,HEAVY,NONE,NONE), // 2500-2501 横线（细/粗）
    enc(NONE,NONE,THIN,THIN), enc(NONE,NONE,HEAVY,HEAVY), // 2502-2503 竖线（细/粗）
    // 虚线样式：渲染时与实线相同（简化处理）
    enc(THIN,THIN,NONE,NONE), enc(HEAVY,HEAVY,NONE,NONE), // 2504-2505 ┄┅ 三段虚横线
    enc(NONE,NONE,THIN,THIN), enc(NONE,NONE,HEAVY,HEAVY), // 2506-2507 ┆┇ 三段虚竖线
    enc(THIN,THIN,NONE,NONE), enc(HEAVY,HEAVY,NONE,NONE), // 2508-2509 ┈┉ 四段虚横线
    enc(NONE,NONE,THIN,THIN), enc(NONE,NONE,HEAVY,HEAVY), // 250A-250B ┊┋ 四段虚竖线

    // ┌ ┍ ┎ ┏   ┐ ┑ ┒ ┓ 左上角/右上角（细线/粗线混合）
    enc(NONE,THIN,NONE,THIN),  enc(NONE,HEAVY,NONE,THIN),  // 250C-250D ┌┍
    enc(NONE,THIN,NONE,HEAVY), enc(NONE,HEAVY,NONE,HEAVY), // 250E-250F ┎┏
    enc(THIN,NONE,NONE,THIN),  enc(HEAVY,NONE,NONE,THIN),  // 2510-2511 ┐┑
    enc(THIN,NONE,NONE,HEAVY), enc(HEAVY,NONE,NONE,HEAVY), // 2512-2513 ┒┓

    // └ ┕ ┖ ┗   ┘ ┙ ┚ ┛ 左下角/右下角（细线/粗线混合）
    enc(NONE,THIN,THIN,NONE),  enc(NONE,HEAVY,THIN,NONE),  // 2514-2515 └┕
    enc(NONE,THIN,HEAVY,NONE), enc(NONE,HEAVY,HEAVY,NONE), // 2516-2517 ┖┗
    enc(THIN,NONE,THIN,NONE),  enc(HEAVY,NONE,THIN,NONE),  // 2518-2519 ┘┙
    enc(THIN,NONE,HEAVY,NONE), enc(HEAVY,NONE,HEAVY,NONE), // 251A-251B ┚┛

    // ├ ┝ ┞ ┟ ┠ ┡ ┢ ┣ 左侧 T 形连接（细/粗混合）
    enc(NONE,THIN,THIN,THIN),   enc(NONE,HEAVY,THIN,THIN),   // 251C-251D ├┝
    enc(NONE,THIN,HEAVY,THIN),  enc(NONE,THIN,THIN,HEAVY),   // 251E-251F ┞┟
    enc(NONE,THIN,HEAVY,HEAVY), enc(NONE,THIN,HEAVY,THIN),   // 2520-2521 ┠┡
    enc(NONE,THIN,THIN,HEAVY),  enc(NONE,HEAVY,HEAVY,HEAVY), // 2522-2523 ┢┣

    // ┤ ┥ ┦ ┧ ┨ ┩ ┪ ┫ 右侧 T 形连接（细/粗混合）
    enc(THIN,NONE,THIN,THIN),   enc(HEAVY,NONE,THIN,THIN),   // 2524-2525 ┤┥
    enc(THIN,NONE,HEAVY,THIN),  enc(THIN,NONE,THIN,HEAVY),   // 2526-2527 ┦┧
    enc(THIN,NONE,HEAVY,HEAVY), enc(THIN,NONE,HEAVY,THIN),   // 2528-2529 ┨┩
    enc(THIN,NONE,THIN,HEAVY),  enc(HEAVY,NONE,HEAVY,HEAVY), // 252A-252B ┪┫

    // ┬ ┭ ┮ ┯ ┰ ┱ ┲ ┳ 顶部 T 形连接（细/粗混合）
    enc(THIN,THIN,NONE,THIN),   enc(HEAVY,THIN,NONE,THIN),   // 252C-252D ┬┭
    enc(THIN,HEAVY,NONE,THIN),  enc(HEAVY,HEAVY,NONE,THIN),  // 252E-252F ┮┯
    enc(THIN,THIN,NONE,HEAVY),  enc(THIN,HEAVY,NONE,HEAVY),  // 2530-2531 ┰┱
    enc(HEAVY,THIN,NONE,HEAVY), enc(HEAVY,HEAVY,NONE,HEAVY), // 2532-2533 ┲┳

    // ┴ ┵ ┶ ┷ ┸ ┹ ┺ ┻ 底部 T 形连接（细/粗混合）
    enc(THIN,THIN,THIN,NONE),   enc(HEAVY,THIN,THIN,NONE),   // 2534-2535 ┴┵
    enc(THIN,HEAVY,THIN,NONE),  enc(HEAVY,HEAVY,THIN,NONE),  // 2536-2537 ┶┷
    enc(THIN,THIN,HEAVY,NONE),  enc(THIN,HEAVY,HEAVY,NONE),  // 2538-2539 ┸┹
    enc(HEAVY,THIN,HEAVY,NONE), enc(HEAVY,HEAVY,HEAVY,NONE), // 253A-253B ┺┻

    // ┼ ┽ ┾ ┿ ╀ ╁ ╂ ╃ ╄ ╅ ╆ ╇ ╈ ╉ ╊ ╋ 十字交叉（细/粗混合）
    enc(THIN,THIN,THIN,THIN),   enc(HEAVY,THIN,THIN,THIN),   // 253C-253D ┼┽
    enc(THIN,HEAVY,THIN,THIN),  enc(HEAVY,HEAVY,THIN,THIN),  // 253E-253F ┾┿
    enc(THIN,THIN,HEAVY,THIN),  enc(THIN,THIN,THIN,HEAVY),   // 2540-2541 ╀╁
    enc(THIN,THIN,HEAVY,HEAVY),  enc(HEAVY,THIN,HEAVY,THIN),  // 2542-2543 ╂╃
    enc(THIN,HEAVY,HEAVY,THIN),  enc(HEAVY,THIN,THIN,HEAVY),  // 2544-2545 ╄╅
    enc(THIN,HEAVY,THIN,HEAVY),  enc(HEAVY,HEAVY,HEAVY,THIN), // 2546-2547 ╆╇
    enc(HEAVY,HEAVY,THIN,HEAVY), enc(HEAVY,THIN,HEAVY,HEAVY), // 2548-2549 ╈╉
    enc(THIN,HEAVY,HEAVY,HEAVY), enc(HEAVY,HEAVY,HEAVY,HEAVY),// 254A-254B ╊╋

    // ╌ ╍ ╎ ╏ 双段虚线（渲染同实线）
    enc(THIN,THIN,NONE,NONE),   enc(HEAVY,HEAVY,NONE,NONE),  // 254C-254D ╌╍
    enc(NONE,NONE,THIN,THIN),   enc(NONE,NONE,HEAVY,HEAVY),  // 254E-254F ╎╏

    // ═ ║ 双横线 / 双竖线
    enc(DBL,DBL,NONE,NONE),     enc(NONE,NONE,DBL,DBL),      // 2550-2551 ═║

    // ╒ ╓ ╔ ╕ ╖ ╗ 双线角（右上/左上，混合单双线）
    enc(NONE,DBL,NONE,THIN),    enc(NONE,THIN,NONE,DBL),     // 2552-2553 ╒╓
    enc(NONE,DBL,NONE,DBL),     enc(DBL,NONE,NONE,THIN),     // 2554-2555 ╔╕
    enc(THIN,NONE,NONE,DBL),    enc(DBL,NONE,NONE,DBL),      // 2556-2557 ╖╗

    // ╘ ╙ ╚ ╛ ╜ ╝ 双线角（右下/左下，混合单双线）
    enc(NONE,DBL,THIN,NONE),    enc(NONE,THIN,DBL,NONE),     // 2558-2559 ╘╙
    enc(NONE,DBL,DBL,NONE),     enc(DBL,NONE,THIN,NONE),     // 255A-255B ╚╛
    enc(THIN,NONE,DBL,NONE),    enc(DBL,NONE,DBL,NONE),      // 255C-255D ╜╝

    // ╞ ╟ ╠ ╡ ╢ ╣ 双线侧向 T 形连接
    enc(NONE,DBL,THIN,THIN),    enc(NONE,THIN,DBL,DBL),      // 255E-255F ╞╟
    enc(NONE,DBL,DBL,DBL),      enc(DBL,NONE,THIN,THIN),     // 2560-2561 ╠╡
    enc(THIN,NONE,DBL,DBL),     enc(DBL,NONE,DBL,DBL),       // 2562-2563 ╢╣

    // ╤ ╥ ╦ ╧ ╨ ╩ 双线顶部/底部 T 形连接
    enc(DBL,DBL,NONE,THIN),     enc(THIN,THIN,NONE,DBL),     // 2564-2565 ╤╥
    enc(DBL,DBL,NONE,DBL),      enc(DBL,DBL,THIN,NONE),      // 2566-2567 ╦╧
    enc(THIN,THIN,DBL,NONE),    enc(DBL,DBL,DBL,NONE),       // 2568-2569 ╨╩

    // ╪ ╫ ╬ 双线十字交叉
    enc(DBL,DBL,THIN,THIN),     enc(THIN,THIN,DBL,DBL),      // 256A-256B ╪╫
    enc(DBL,DBL,DBL,DBL),                                    // 256C ╬

    // ╭ ╮ ╯ ╰ 圆角（渲染为普通直角，矩形图元无法表达圆弧）
    enc(NONE,THIN,NONE,THIN),   enc(THIN,NONE,NONE,THIN),    // 256D-256E ╭╮
    enc(THIN,NONE,THIN,NONE),   enc(NONE,THIN,THIN,NONE),    // 256F-2570 ╯╰

    // ╱ ╲ ╳ 对角线：矩形图元无法表达斜线，跳过
    0, 0, 0,                                                 // 2571-2573

    // ╴ ╵ ╶ ╷ ╸ ╹ ╺ ╻ 半截线（只延伸到单元格中心的单方向线段）
    enc(THIN,NONE,NONE,NONE),   enc(NONE,NONE,THIN,NONE),    // 2574-2575 ╴╵
    enc(NONE,THIN,NONE,NONE),   enc(NONE,NONE,NONE,THIN),    // 2576-2577 ╶╷
    enc(HEAVY,NONE,NONE,NONE),  enc(NONE,NONE,HEAVY,NONE),   // 2578-2579 ╸╹
    enc(NONE,HEAVY,NONE,NONE),  enc(NONE,NONE,NONE,HEAVY),   // 257A-257B ╺╻

    // ╼ ╽ ╾ ╿ 混合粗细的半截线
    enc(THIN,HEAVY,NONE,NONE),  enc(NONE,NONE,THIN,HEAVY),   // 257C-257D ╼╽
    enc(HEAVY,THIN,NONE,NONE),  enc(NONE,NONE,HEAVY,THIN),   // 257E-257F ╾╿
];

// ─── 公开 API ─────────────────────────────────────────────────────────────────

/// 判断该字符是否属于内置渲染范围（盒子绘制字符或块元素）。
/// 属于该范围的字符将跳过字体渲染，改用矩形图元直接绘制，
/// 以确保相邻单元格的线条完美对齐。
pub fn is_box_drawing_char(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x259F)
}

/// 将内置盒子/块元素字符直接绘制到 `window` 上。
/// 每个字符由若干个填充矩形（颜色为前景色）拼合而成。
/// 采用回调方式驱动内部绘制函数，避免中间 Vec 堆分配。
pub fn paint_box_drawing(
    c: char,
    origin: Point<Pixels>,
    cell_w: Pixels,
    cell_h: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let mut paint = |quad: Bounds<Pixels>| window.paint_quad(fill(quad, color));
    let code = c as u32;
    match code {
        // U+2500–U+257F：盒子绘制字符，通过线段表查找各方向线段并合成矩形
        0x2500..=0x257F => {
            let segs = BOX_SEGS[(code - 0x2500) as usize];
            if segs != 0 {
                let left = (segs & 0x03) as u8;
                let right = ((segs >> 2) & 0x03) as u8;
                let up = ((segs >> 4) & 0x03) as u8;
                let down = ((segs >> 6) & 0x03) as u8;
                paint_segments(origin, cell_w, cell_h, left, right, up, down, &mut paint);
            }
        }
        // U+2580–U+259F：块元素字符，按比例填充单元格的指定区域
        0x2580..=0x259F => paint_block_element(c, origin, cell_w, cell_h, &mut paint),
        _ => {}
    }
}

// ─── 线段绘制实现 ──────────────────────────────────────────────────────────────

/// 计算线段粗细：最小 1px，约为单元格宽度的 1/11。
/// 与 Alacritty 的启发式算法一致，使线条在不同字号下都有合适的视觉权重。
fn stroke(cell_w: Pixels) -> Pixels {
    px((cell_w.as_f32() / 11.0).max(1.0).round())
}

/// 将四个方向的线段描述（左/右/上/下，每个带粗细权重）逐一通过回调 `f` 输出矩形。
///
/// 绘制规则：
/// - 横向线段：以单元格垂直中线为中心，上下各延伸半个笔画粗细的水平矩形
/// - 纵向线段：以单元格水平中线为中心，左右各延伸半个笔画粗细的垂直矩形
/// - 双线：在中心两侧各偏移 1.5 倍笔画粗细处各画一条细线
fn paint_segments(
    origin: Point<Pixels>,
    w: Pixels,
    h: Pixels,
    left: u8,
    right: u8,
    up: u8,
    down: u8,
    f: &mut impl FnMut(Bounds<Pixels>),
) {
    let s = stroke(w);

    let hc = px((w.as_f32() / 2.0).floor()); // 单元格水平中心（竖线所在列）
    let vc = px((h.as_f32() / 2.0).floor()); // 单元格垂直中心（横线所在行）

    // 生成水平半段矩形：从 x0 到 x1，垂直居中于 vc，高度为 thick
    let h_seg = |x0: f32, x1: f32, thick: Pixels| -> Bounds<Pixels> {
        Bounds {
            origin: point(origin.x + px(x0), origin.y + vc - thick * 0.5),
            size: size(px(x1 - x0), thick),
        }
    };
    // 生成垂直半段矩形：从 y0 到 y1，水平居中于 hc，宽度为 thick
    let v_seg = |y0: f32, y1: f32, thick: Pixels| -> Bounds<Pixels> {
        Bounds {
            origin: point(origin.x + hc - thick * 0.5, origin.y + px(y0)),
            size: size(thick, px(y1 - y0)),
        }
    };

    // 双线偏移量：两条平行线分别在中心 ±dbl_off 处
    let dbl_off = s * 1.5;

    let hcf = hc.as_f32();
    let vcf = vc.as_f32();
    let wf = w.as_f32();
    let hf = h.as_f32();

    // 左半段：从单元格左边沿到水平中心
    // 注意：h_seg / v_seg 不捕获 f，故可与 f 的调用交替使用，避免两个闭包同时借用 f
    if left != NONE {
        match left {
            THIN => f(h_seg(0.0, hcf, s)),
            HEAVY => f(h_seg(0.0, hcf, s * 2.0)),
            DBL => {
                f(Bounds { origin: point(origin.x, origin.y + vc - dbl_off - s * 0.5), size: size(px(hcf), s) });
                f(Bounds { origin: point(origin.x, origin.y + vc + dbl_off - s * 0.5), size: size(px(hcf), s) });
            }
            _ => {}
        }
    }
    // 右半段：从水平中心到单元格右边沿
    if right != NONE {
        match right {
            THIN => f(h_seg(hcf, wf, s)),
            HEAVY => f(h_seg(hcf, wf, s * 2.0)),
            DBL => {
                f(Bounds { origin: point(origin.x + hc, origin.y + vc - dbl_off - s * 0.5), size: size(px(wf - hcf), s) });
                f(Bounds { origin: point(origin.x + hc, origin.y + vc + dbl_off - s * 0.5), size: size(px(wf - hcf), s) });
            }
            _ => {}
        }
    }
    // 上半段：从单元格顶边沿到垂直中心
    if up != NONE {
        match up {
            THIN => f(v_seg(0.0, vcf, s)),
            HEAVY => f(v_seg(0.0, vcf, s * 2.0)),
            DBL => {
                f(Bounds { origin: point(origin.x + hc - dbl_off - s * 0.5, origin.y), size: size(s, px(vcf)) });
                f(Bounds { origin: point(origin.x + hc + dbl_off - s * 0.5, origin.y), size: size(s, px(vcf)) });
            }
            _ => {}
        }
    }
    // 下半段：从垂直中心到单元格底边沿
    if down != NONE {
        match down {
            THIN => f(v_seg(vcf, hf, s)),
            HEAVY => f(v_seg(vcf, hf, s * 2.0)),
            DBL => {
                f(Bounds { origin: point(origin.x + hc - dbl_off - s * 0.5, origin.y + vc), size: size(s, px(hf - vcf)) });
                f(Bounds { origin: point(origin.x + hc + dbl_off - s * 0.5, origin.y + vc), size: size(s, px(hf - vcf)) });
            }
            _ => {}
        }
    }
}

// ─── 块元素绘制实现 ────────────────────────────────────────────────────────────

/// 将块元素字符（U+2580–U+259F）通过回调 `f` 输出填充矩形。
/// 每个块元素字符表示对单元格某个区域的像素填充，例如：
///   ▀ = 上半格  ▄ = 下半格  █ = 整格  ▌ = 左半格  ░ = 浅阴影
fn paint_block_element(
    c: char,
    origin: Point<Pixels>,
    w: Pixels,
    h: Pixels,
    f: &mut impl FnMut(Bounds<Pixels>),
) {
    let code = c as u32;

    // 辅助闭包：计算单元格宽/高的 n/d 比例对应的像素数
    let frac_w = |n: f32, d: f32| px((w.as_f32() * n / d).round());
    let frac_h = |n: f32, d: f32| px((h.as_f32() * n / d).round());

    match code {
        // ▀ 上半块：填充单元格上半部分
        0x2580 => f(Bounds { origin, size: size(w, frac_h(1.0, 2.0)) }),

        // ▁▂▃▄▅▆▇ 下 N/8 块：从单元格底部向上填充 N/8 高度
        0x2581..=0x2587 => {
            let n = (code - 0x2580) as f32; // N 取值 1–7
            let bh = frac_h(n, 8.0);
            f(Bounds { origin: point(origin.x, origin.y + h - bh), size: size(w, bh) });
        }

        // █ 全块：填充整个单元格
        0x2588 => f(Bounds { origin, size: size(w, h) }),

        // ▉▊▋▌▍▎▏ 左 N/8 块：从单元格左侧向右填充 N/8 宽度（7→1 递减）
        0x2589..=0x258F => {
            let n = (0x2590 - code) as f32; // N 从 7 递减到 1
            f(Bounds { origin, size: size(frac_w(n, 8.0), h) });
        }

        // ▐ 右半块：填充单元格右半部分
        0x2590 => {
            let half = frac_w(1.0, 2.0);
            f(Bounds { origin: point(origin.x + w - half, origin.y), size: size(half, h) });
        }

        // ░ 浅阴影：约 25% 覆盖率，用棋盘格小矩形近似
        0x2591 => shade_quads(origin, w, h, 1, 4, f),

        // ▒ 中阴影：约 50% 覆盖率
        0x2592 => shade_quads(origin, w, h, 1, 2, f),

        // ▓ 深阴影：约 75% 覆盖率
        0x2593 => shade_quads(origin, w, h, 3, 4, f),

        // ▔ 上 1/8 块：填充单元格顶部 1/8 高度的细条
        0x2594 => f(Bounds { origin, size: size(w, frac_h(1.0, 8.0)) }),

        // ▕ 右 1/8 块：填充单元格最右侧 1/8 宽度的细条
        0x2595 => {
            let bw = frac_w(1.0, 8.0);
            f(Bounds { origin: point(origin.x + w - bw, origin.y), size: size(bw, h) });
        }

        // ▖▗▘▙▚▛▜▝▞▟ 四分之一块组合：每个字符是若干象限的填充组合
        0x2596 => quadrant(origin, w, h, false, false, true, false, f),   // ▖ 左下象限
        0x2597 => quadrant(origin, w, h, false, false, false, true, f),   // ▗ 右下象限
        0x2598 => quadrant(origin, w, h, true, false, false, false, f),   // ▘ 左上象限
        0x2599 => quadrant(origin, w, h, true, false, true, true, f),     // ▙ 左上 + 左下 + 右下
        0x259A => quadrant(origin, w, h, true, false, false, true, f),    // ▚ 左上 + 右下（对角）
        0x259B => quadrant(origin, w, h, true, true, true, false, f),     // ▛ 左上 + 右上 + 左下
        0x259C => quadrant(origin, w, h, true, true, false, true, f),     // ▜ 左上 + 右上 + 右下
        0x259D => quadrant(origin, w, h, false, true, false, false, f),   // ▝ 右上象限
        0x259E => quadrant(origin, w, h, false, true, true, false, f),    // ▞ 右上 + 左下（对角）
        0x259F => quadrant(origin, w, h, false, true, true, true, f),     // ▟ 右上 + 左下 + 右下

        _ => {}
    }
}

/// 阴影字符（░▒▓）近似渲染：用棋盘格小矩形模拟指定覆盖率。
/// `num/den` 表示覆盖率，例如 1/4 = 25%（浅阴影），3/4 = 75%（深阴影）。
fn shade_quads(
    origin: Point<Pixels>,
    w: Pixels,
    h: Pixels,
    num: u32,
    den: u32,
    f: &mut impl FnMut(Bounds<Pixels>),
) {
    // 将单元格划分为小网格，每个格子大小约 2×2 px
    let cols = (w.as_f32() / 2.0).max(1.0) as u32;
    let rows = (h.as_f32() / 2.0).max(1.0) as u32;
    let dot_w = px(w.as_f32() / cols as f32);
    let dot_h = px(h.as_f32() / rows as f32);

    for row in 0..rows {
        for col in 0..cols {
            // 取模棋盘格模式控制覆盖密度（比线性分布更均匀）
            if (row + col) % den < num {
                f(Bounds {
                    origin: point(
                        origin.x + dot_w * col as f32,
                        origin.y + dot_h * row as f32,
                    ),
                    size: size(dot_w, dot_h),
                });
            }
        }
    }
}

/// 象限块渲染：将单元格均分为四个象限，按参数填充指定象限。
/// 用于渲染 ▖▗▘▙▚▛▜▝▞▟ 等四分之一块组合字符。
fn quadrant(
    origin: Point<Pixels>,
    w: Pixels,
    h: Pixels,
    upper_left: bool,
    upper_right: bool,
    lower_left: bool,
    lower_right: bool,
    f: &mut impl FnMut(Bounds<Pixels>),
) {
    // 象限宽/高 = 单元格宽/高的一半（取整）
    let hw = px((w.as_f32() / 2.0).round());
    let hh = px((h.as_f32() / 2.0).round());

    if upper_left {
        f(Bounds { origin, size: size(hw, hh) });
    }
    if upper_right {
        f(Bounds {
            origin: point(origin.x + w - hw, origin.y),
            size: size(hw, hh),
        });
    }
    if lower_left {
        f(Bounds {
            origin: point(origin.x, origin.y + h - hh),
            size: size(hw, hh),
        });
    }
    if lower_right {
        f(Bounds {
            origin: point(origin.x + w - hw, origin.y + h - hh),
            size: size(hw, hh),
        });
    }
}
