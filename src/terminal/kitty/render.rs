//! 把 Kitty 图形协议存储中的图像变成 GPUI 图元。
//!
//! 分层：本模块不做协议解析（那是 [`super::command`]/[`super::exec`] 的事），
//! 只负责「存储快照 → 屏幕上的 `paint_image` 调用」。这样渲染逻辑与协议
//! 实现解耦，且能在不启动窗口的情况下单测几何计算（[`plan`]）。
//!
//! ## 坐标系
//!
//! 存储中的 `Placement` 用网格坐标（行/列 + 单元格内像素偏移），渲染层用
//! 屏幕像素。转换需要三件事：单元格尺寸、终端内容原点、以及**滚动偏移**
//! ——放置锚定在 grid 的绝对行上，滚动后要跟着文字一起移动，否则图像会
//! 「粘」在屏幕上。
//!
//! ## z 序
//!
//! kitty 规定 `z < 0` 画在文字**之下**，`z >= 0` 画在文字**之上**（注意
//! `z=0` 也是在文字之上）。因此渲染分两桶：负 z 的桶在文字之前绘制，
//! 其余在文字之后绘制。桶内按 z 升序，同 z 按插入顺序。

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, Bounds, Corners, Pixels, Point, Size, Window, px};

use crate::terminal::kitty::image::{DecodedImage, ImageId};
use crate::terminal::kitty::storage::Storage;

/// 图像像素坐标系里的一个矩形（`paint_image` 的源区域）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceRect {
    /// 左上角在图像像素中的位置。
    pub origin: Point<Pixels>,
    /// 在图像像素中的尺寸。
    pub size: Size<Pixels>,
}

impl SourceRect {
    /// 整幅图像（普通放置的情形）。
    pub fn full(image: &DecodedImage) -> Self {
        Self {
            origin: Point::new(px(0.0), px(0.0)),
            size: Size::new(px(image.width as f32), px(image.height as f32)),
        }
    }
}

/// 一个图像在屏幕上的落位（已从网格坐标换算成像素）。
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedImage {
    pub id: ImageId,
    pub placement_id: u32,
    /// 图像左上角在窗口中的位置。
    pub origin: Point<Pixels>,
    /// 缩放后的显示尺寸。
    pub size: Size<Pixels>,
    /// 要绘制图像里的哪一块（像素坐标）。
    ///
    /// 普通放置是整幅图像；Unicode 占位符的每个片段只取图像的一小片，
    /// 再把这一片铺满 [`PlacedImage::size`] 的矩形。
    pub source: SourceRect,
    /// `z < 0` 表示画在文字之下。
    pub below_text: bool,
    /// 用于丢弃被滚动出屏幕的放置（避免每帧为不可见图像做缩放）。
    pub z: i32,
}

/// 单元格度量：把网格坐标换算成像素所需的三元组。
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub cell_width: Pixels,
    pub line_height: Pixels,
}

/// 视口参数：内容原点与滚动偏移。
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    /// 终端内容区左上角（不含标签栏等外部装饰）。
    pub origin: Point<Pixels>,
    /// 向上滚动的行数（alacritty 的 `display_offset`）。
    ///
    /// 放置锚定在 grid 绝对行 `row`；该行在视口中的位置是
    /// `row + display_offset`（因为向上滚动会把更早的行拉进视口）。
    pub display_offset: usize,
    /// 视口行数，用于裁剪。
    pub rows: usize,
    /// 视口列数，用于估算未指定尺寸的图像的列数。
    pub cols: usize,
}

/// 计算一个放置的显示矩形。
///
/// 尺寸规则（对齐 kitty 规范）：
/// - `c=`/`r=` 同时给出 → 按给定单元格数铺满（图像被拉伸）；
/// - 只给 `c=` → 按图像宽高比算行数；
/// - 都不给 → 由图像像素尺寸除以单元格尺寸得到；
/// - 若结果超过视口，按比例收缩以完整可见（kitty 的 "shrink to fit"）。
fn placement_rect(
    image: &DecodedImage,
    placement: &crate::terminal::kitty::storage::Placement,
    metrics: CellMetrics,
    viewport: Viewport,
) -> (Size<Pixels>, Point<Pixels>) {
    let cw = metrics.cell_width.as_f32();
    let ch = metrics.line_height.as_f32();
    // 图像自身的默认格子尺寸（像素尺寸 / 单元格尺寸），至少 1 格。
    let natural_cols = (image.width as f32 / cw).max(1.0);
    let natural_rows = (image.height as f32 / ch).max(1.0);

    // 请求的行列数：`columns`/`rows` 为 `Some` 时是显式指定。
    let cols = placement.columns.map(|c| c as f32);
    let rows = placement.rows.map(|r| r as f32);

    let (cols, rows) = match (cols, rows) {
        (Some(c), Some(r)) => (c, r),
        (Some(c), None) => {
            let r = c * (natural_rows / natural_cols);
            (c, r)
        }
        (None, Some(r)) => {
            let c = r * (natural_cols / natural_rows);
            (c, r)
        }
        (None, None) => (natural_cols, natural_rows),
    };

    // 收缩以适应视口：把「已定位的行 + 高出的行」压回视口内。
    let mut cols = cols;
    let mut rows = rows;
    if placement.row >= 0 {
        let avail_rows =
            (viewport.rows as f32) - (placement.row as f32 - viewport.display_offset as f32);
        if avail_rows > 0.0 && rows > avail_rows {
            let scale = avail_rows / rows;
            rows = avail_rows;
            cols *= scale;
        }
    }

    // 视口内的像素位置：行要加回 scrollback 偏移。
    let viewport_row = placement.row as f32 + viewport.display_offset as f32;
    let origin = Point::new(
        viewport.origin.x + px(placement.col as f32 * cw + placement.x_offset as f32),
        viewport.origin.y + px(viewport_row * ch + placement.y_offset as f32),
    );
    let size = Size::new(px(cols * cw), px(rows * ch));
    (size, origin)
}

/// 一段占位符在屏幕上的落位与源矩形。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaceholderRect {
    /// 源矩形左上角（图像像素坐标）。
    pub source_origin: Point<Pixels>,
    /// 源矩形尺寸（图像像素）。
    pub source_size: Size<Pixels>,
    /// 目标矩形相对单元格原点的偏移（像素）。源图像按比例居中后会留下
    /// 边距，这个偏移就是被裁掉的那部分在目标里的宽度，用来把目标矩形往
    /// 左上拉回，避免片段之间出现缝隙。
    pub dest_offset: Point<Pixels>,
    /// 目标矩形尺寸（像素）。
    pub dest_size: Size<Pixels>,
}

/// 计算一段 Unicode 占位符该显示图像的哪一片、铺到多大。
///
/// 这是 ghostty `graphics_unicode.zig::Placement::renderPlacement` 的移植。
/// 命名沿用那边的约定：
/// - `img_*` 指原图像；
/// - `p_*` 指最终的放置（placement）；
/// - `vp_*` 指这一段虚拟放置（占 `width` 列 × 1 行）。
///
/// ## 为什么要这么绕
///
/// 虚拟放置声明的是"整幅图铺进 `columns × rows` 个单元格"，而占位符是按
/// **片段**逐个请求的（一段只占 1 行、`width` 列）。图像按比例缩放后未必
/// 刚好填满格子（可能左右或上下留白），而留白区域在原图里并不存在，所以
/// 落在留白处的片段必须把源矩形往里收、目标矩形相应变矮变窄——这就是下面
/// 那一串 `if/else` 在做的事。
///
/// 返回 `None` 表示这一段完全落在图像之外（nothing to draw）。
#[allow(clippy::too_many_arguments)]
pub fn placeholder_rect(
    image_width: u32,
    image_height: u32,
    // 虚拟放置声明的网格尺寸（`c=`/`r=`，为 0 表示按图像尺寸推导）。
    placement_columns: Option<u32>,
    placement_rows: Option<u32>,
    // 这一段在图像内的起始行列与段宽。
    vp_col: u32,
    vp_row: u32,
    vp_width: u32,
    cell_width: f64,
    cell_height: f64,
) -> Option<PlaceholderRect> {
    // 虚拟放置的网格尺寸：未指定时按"图像原尺寸需要多少格"推导，保证
    // 图像以 1:1 像素铺开。不做宽高比修正——这一步只负责把图塞进格子。
    let columns = match placement_columns {
        Some(c) if c != 0 => c as f64,
        _ => (image_width as f64 / cell_width.max(1.0)).ceil().max(1.0),
    };
    let rows = match placement_rows {
        Some(r) if r != 0 => r as f64,
        _ => (image_height as f64 / cell_height.max(1.0)).ceil().max(1.0),
    };

    let img_width = image_width as f64;
    let img_height = image_height as f64;

    // 把图像按比例铺进 `columns × rows`，多余的一边居中留白。
    let p_rows_px = rows * cell_height;
    let p_cols_px = columns * cell_width;
    let (p_x_offset, p_y_offset, p_x_scale, p_y_scale) =
        if img_width * p_rows_px > img_height * p_cols_px {
            // 图像比格子"宽"：宽度铺满，高度居中留白。
            let x_scale = p_cols_px / img_width.max(1.0);
            let y_scale = x_scale;
            (
                0.0,
                (p_rows_px - img_height * y_scale) / 2.0,
                x_scale,
                y_scale,
            )
        } else {
            // 图像比格子"高"：高度铺满，宽度居中留白。
            let y_scale = p_rows_px / img_height.max(1.0);
            let x_scale = y_scale;
            (
                (p_cols_px - img_width * x_scale) / 2.0,
                0.0,
                x_scale,
                y_scale,
            )
        };

    // 把"留白"折算回图像自身的像素空间：在缩放后的坐标系里图像两侧各有
    // 这么多像素是空白。`img_scaled` 就是"图像 + 两侧留白"的整体尺寸。
    let scaled_x_offset = p_x_offset / p_x_scale.max(f64::MIN_POSITIVE);
    let scaled_y_offset = p_y_offset / p_y_scale.max(f64::MIN_POSITIVE);
    let scaled_width = img_width + scaled_x_offset * 2.0;
    let scaled_height = img_height + scaled_y_offset * 2.0;

    // 这一段在"缩放后坐标系"里占据的矩形：按片段在整幅图网格里的比例切分。
    let mut src_x = scaled_width * (vp_col as f64 / columns);
    let mut src_y = scaled_height * (vp_row as f64 / rows);
    let mut src_width = scaled_width * (vp_width as f64 / columns);
    let mut src_height = scaled_height * (1.0 / rows);

    // 目标矩形：一段就是 `vp_width` 列 × 1 行，默认铺满。
    let mut dest_x_offset = 0.0;
    let mut dest_y_offset = 0.0;
    let mut dest_width = vp_width as f64 * cell_width;
    let mut dest_height = cell_height;

    // 竖向：片段落在上留白 / 下留白 / 正常区域，处理方式不同。
    if src_y < scaled_y_offset {
        // 上留白里的部分不存在于原图：源矩形上移到底、目标矩形变矮。
        let offset = scaled_y_offset - src_y;
        src_height -= offset;
        dest_y_offset = offset * p_y_scale;
        dest_height -= offset * p_y_scale;
        src_y = 0.0;

        // 上下留白同时侵入时，高度可能被扣过头，夹回原图高度。
        if src_height > img_height {
            src_height = img_height;
            dest_height = img_height * p_y_scale;
        }
    } else if src_y + src_height > scaled_height - scaled_y_offset {
        // 下留白：同理收窄。
        src_y -= scaled_y_offset;
        src_height = scaled_height - scaled_y_offset - src_y - scaled_y_offset;
        dest_height = src_height * p_y_scale;
    } else {
        // 正常区域：只把留白的偏移减掉。
        src_y -= scaled_y_offset;
    }

    // 横向：与竖向完全对称。
    if src_x < scaled_x_offset {
        let offset = scaled_x_offset - src_x;
        src_width -= offset;
        dest_x_offset = offset * p_x_scale;
        dest_width -= offset * p_x_scale;
        src_x = 0.0;

        if src_width > img_width {
            src_width = img_width;
            dest_width = img_width * p_x_scale;
        }
    } else if src_x + src_width > scaled_width - scaled_x_offset {
        src_x -= scaled_x_offset;
        src_width = scaled_width - scaled_x_offset - src_x - scaled_x_offset;
        dest_width = src_width * p_x_scale;
    } else {
        src_x -= scaled_x_offset;
    }

    // 退化为非正尺寸说明这一段整体落在图像之外，直接不画。
    if src_width <= 0.0 || src_height <= 0.0 || dest_width <= 0.0 || dest_height <= 0.0 {
        return None;
    }

    Some(PlaceholderRect {
        source_origin: Point::new(px(src_x.round() as f32), px(src_y.round() as f32)),
        source_size: Size::new(px(src_width.round() as f32), px(src_height.round() as f32)),
        dest_offset: Point::new(
            px(dest_x_offset.round() as f32),
            px(dest_y_offset.round() as f32),
        ),
        dest_size: Size::new(
            px(dest_width.round() as f32),
            px(dest_height.round() as f32),
        ),
    })
}

/// 从存储快照算出本帧要绘制的全部放置，按绘制顺序（负 z 在前）排列。
///
/// 返回 `Vec<PlacedImage>`；调用方按 `below_text` 分两段绘制即可。
/// 已滚动出视口、或尺寸退化为 0 的放置会被丢弃。
pub fn plan(storage: &Storage, metrics: CellMetrics, viewport: Viewport) -> Vec<PlacedImage> {
    let mut out: Vec<PlacedImage> = Vec::new();

    for (id, stored) in storage.iter() {
        for (placement_id, placement) in stored.placements.iter() {
            // 虚拟放置自身不绘制：它的实际落位由网格里的 U+10EEEE 占位符
            // 决定（见 [`plan_placeholders`]）。这里跳过，否则同一张图会在
            // 创建时的光标位置多画一份。
            if placement.is_virtual() {
                continue;
            }

            // 行号可能为负（锚定在 scrollback 里）：换算到视口坐标后判断可见性。
            let viewport_row = placement.row as f32 + viewport.display_offset as f32;
            if placement.row < 0 && viewport_row < 0.0 {
                continue;
            }
            if viewport_row >= viewport.rows as f32 && placement.row >= 0 {
                continue;
            }

            let (size, origin) = placement_rect(&stored.image, placement, metrics, viewport);
            if size.width <= px(0.0) || size.height <= px(0.0) {
                continue;
            }
            // 完全落在视口左边（列号为负且宽不足）时无可见区域。
            if placement.col == 0 && placement.x_offset > 0 && origin.x < viewport.origin.x {
                continue;
            }

            out.push(PlacedImage {
                id: *id,
                placement_id: *placement_id,
                origin,
                size,
                source: SourceRect::full(&stored.image),
                below_text: placement.is_below_text(),
                z: placement.z,
            });
        }
    }

    // 稳定的绘制顺序：先比 z，再比 id/placement（保证同 z 时结果确定）。
    out.sort_by(|a, b| {
        a.z.cmp(&b.z)
            .then(a.id.cmp(&b.id))
            .then(a.placement_id.cmp(&b.placement_id))
    });
    out
}

/// 视口里一个待解码的占位符单元格。
///
/// 刻意是一份从扁平数据（网格行/列 + 颜色 + 变音符），而不是借用 alacritty
/// 的 `Cell`：这样几何与解码都能脱开终端类型单测，元素层只负责把快照拍平。
#[derive(Clone, Copy, Debug)]
pub struct PlaceholderCell {
    /// 网格绝对行号。
    pub row: i32,
    /// 列号。
    pub col: u32,
    /// 前景色（图像 id 低 24 位）。
    pub fg: alacritty_terminal::vte::ansi::Color,
    /// 下划线专用色（放置 id）；未设置为 `None`。
    pub underline_color: Option<alacritty_terminal::vte::ansi::Color>,
    /// 组合变音符（行/列/高 8 位）。
    pub zerowidth: Option<[char; 3]>,
}

/// 把视口里的占位符单元格变成要绘制的图像片段。
///
/// `cells` 必须按 `(row, col)` 升序（元素层遍历网格时天然满足），因为
/// [`runs_in_row`] 依赖列序来合并连续段。
///
/// 每个片段都换算成屏幕像素：网格行加上滚动偏移得到视口行，再乘单元格尺寸
/// 得到原点。`dest_offset`（图像居中留白造成的偏移）会从原点里减掉，让片段
/// 紧贴单元格上沿而不是被留白顶下去。
///
/// 没有对应虚拟放置、或片段整体落在图像之外时静默跳过——客户端可能先打
/// 占位符再传图，中间态不该画出任何东西。
///
/// `cells` 必须按 `(行, 列)` 升序，且同一行的单元格必须连续（`last_content`
/// 的快照天然满足：alacritty 的 `GridIterator` 逐行逐列递增）。下面的分组是
/// 单趟扫描，顺序错了会把不相关的行并成一组、片段落到错误位置。
pub fn plan_placeholders(
    storage: &Storage,
    cells: &[PlaceholderCell],
    metrics: CellMetrics,
    viewport: Viewport,
) -> Vec<PlacedImage> {
    debug_assert!(
        cells
            .windows(2)
            .all(|w| (w[0].row, w[0].col) <= (w[1].row, w[1].col)),
        "plan_placeholders 要求 cells 按 (行, 列) 升序，否则单趟分组会错位"
    );
    let mut out: Vec<PlacedImage> = Vec::new();
    let cw = metrics.cell_width.as_f32() as f64;
    let ch = metrics.line_height.as_f32() as f64;

    // 按行分组。`cells` 已按 (row, col) 有序，所以顺序扫一遍即可。
    let mut start = 0usize;
    while start < cells.len() {
        let row = cells[start].row;
        let mut end = start + 1;
        while end < cells.len() && cells[end].row == row {
            end += 1;
        }
        let row_cells = &cells[start..end];
        start = end;

        // 视口行：网格行 + 滚动偏移（与 `plan` 同一套换算）。
        let viewport_row = row + viewport.display_offset as i32;
        // 整行都在视口之外就跳过（上下各判一次，允许锚定在 scrollback 的行）。
        if row >= 0 && viewport_row as usize >= viewport.rows {
            continue;
        }
        if row < 0 && viewport_row < 0 {
            continue;
        }

        // 解码 + 合并成段。
        let decoded: Vec<(u32, crate::terminal::kitty::unicode::IncompletePlacement)> = row_cells
            .iter()
            .map(|cell| {
                let fg_id = crate::terminal::kitty::unicode::color_to_id(&cell.fg);
                let underline_id = cell
                    .underline_color
                    .as_ref()
                    .map_or(0, crate::terminal::kitty::unicode::color_to_id);
                // 变音符数组用 '\0' 填充；解码时会把 '\0' 查不到而当作"没写"。
                let empty = ['\0'; 3];
                let diacritics = cell.zerowidth.as_ref().unwrap_or(&empty);
                let all = *diacritics;
                (
                    cell.col,
                    crate::terminal::kitty::unicode::IncompletePlacement::decode(
                        fg_id,
                        underline_id,
                        &all[..],
                    ),
                )
            })
            .collect();

        for run in crate::terminal::kitty::unicode::runs_in_row(&decoded, row) {
            let Some(stored) = storage.get(run.image_id) else {
                continue;
            };
            let Some((_, target)) = storage.placeholder_target(run.image_id, run.placement_id)
            else {
                continue;
            };

            let Some(rect) = placeholder_rect(
                stored.image.width,
                stored.image.height,
                target.columns,
                target.rows,
                run.col,
                run.row,
                run.width,
                cw,
                ch,
            ) else {
                continue;
            };

            // 目标矩形的左上角：单元格左上角往左上各挪 dest_offset（把居中
            // 留白裁掉的那部分补回来，片段之间才不会露出缝）。
            let origin = Point::new(
                viewport.origin.x + px(run.grid_col as f32 * cw as f32) - rect.dest_offset.x,
                viewport.origin.y + px(viewport_row as f32 * ch as f32) - rect.dest_offset.y,
            );

            out.push(PlacedImage {
                id: run.image_id,
                // 占位符是"另一条"绘制路径，用放置 id 之外的高位做区分，
                // 避免与同图像的普通放置在同一 z 上排序时打架。
                placement_id: run.placement_id,
                origin,
                size: rect.dest_size,
                source: SourceRect {
                    origin: rect.source_origin,
                    size: rect.source_size,
                },
                // 占位符铺在文字上方：它本来就是"用图像替代这几个字符"。
                below_text: false,
                z: target.z,
            });
        }
    }

    out.sort_by(|a, b| {
        a.z.cmp(&b.z)
            .then(a.id.cmp(&b.id))
            .then(a.placement_id.cmp(&b.placement_id))
    });
    out
}

/// 已转成 GPUI 纹理的图像缓存。
///
/// `RenderImage` 的构造要走 RGBA→BGRA 的全量重写，对大图不便宜，因此在
/// 存储代际不变时复用同一批纹理。键是 `(图像 id, 内容代际)`——客户端用相同
/// id 重新传输（内容变了）时不能命中旧纹理，动画换帧时代际也会变
/// （见 `Storage::mark_content_changed`），因此同一个键始终对应同一份像素。
///
/// 注意 GPU 侧的生命周期：`RenderImage::new` 每次分配全局唯一的图集 id，
/// 而 GPUI 的 sprite atlas 没有引用计数、不会自动回收。裁剪缓存时必须把
/// 被释放的纹理交给 `Window::drop_image`，否则每张被替换/删除/换帧的图像
/// 都会在 Metal 图集里永久留下一块 tile（Apple Silicon 统一内存，直接计入
/// 进程内存——终端里持续刷新的页面几分钟就能堆出 GB 级常驻）。
pub struct ImageCache {
    /// `(id, storage_generation) -> 纹理`。
    textures: HashMap<(ImageId, u64), Arc<gpui::RenderImage>>,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
        }
    }
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 丢弃不再需要的纹理，返回被释放的纹理。
    ///
    /// 对齐 ghostty `rebuildKitty` 的 `markForUnload`：以下两类都只卸 GPU，
    /// 终端侧的图像数据原样保留——
    /// - 世代过期（被替换/换帧）：旧像素永远不再显示；
    /// - 图像已无任何放置（`d=` 删 placement 未释数据、或滚出可视区的虚拟
    ///   放置从未落纹理）：留着 tile 占显存，重新放置时按需重建即可。
    ///
    /// 调用方必须把返回的纹理逐个传给 `Window::drop_image`（见本类型的文档），
    /// 只 drop `Arc` 不释放图集 tile。
    fn retain_live(&mut self, storage: &Storage) -> Vec<Arc<gpui::RenderImage>> {
        let mut removed = Vec::new();
        self.textures.retain(|(id, generation), texture| {
            let live = storage.get(*id).is_some_and(|stored| {
                stored.generation == *generation && stored.is_placed()
            });
            if !live {
                removed.push(texture.clone());
            }
            live
        });
        removed
    }

    /// 取得（必要时创建）某张图像**当前显示帧**的纹理。
    pub fn get_or_insert(
        &mut self,
        id: ImageId,
        storage: &Storage,
        _cx: &mut App,
    ) -> Option<Arc<gpui::RenderImage>> {
        let stored = storage.get(id)?;
        let key = (id, stored.generation);
        if let Some(texture) = self.textures.get(&key) {
            return Some(texture.clone());
        }
        // 动画图像要上传当前帧，而不是根帧。
        let rgba = stored.render_data();
        let texture = Arc::new(to_render_image_from(
            stored.image.width,
            stored.image.height,
            rgba,
        ));
        self.textures.insert(key, texture.clone());
        Some(texture)
    }

    /// 按当前存储裁剪缓存，返回被释放的纹理。
    ///
    /// 调用方（持有 `&mut Window` 的绘制路径）必须把返回的纹理逐个
    /// `drop_image`，否则 GPU 图集侧泄漏。
    pub fn prune(&mut self, storage: &Storage) -> Vec<Arc<gpui::RenderImage>> {
        self.retain_live(storage)
    }

    /// 取出全部纹理（视图销毁时释放 GPU 图集用）。
    ///
    /// 调用方必须把返回的纹理逐个 `drop_image`；缓存清空后重绘会按需重建，
    /// 因此 undo 恢复等复活路径不受影响。
    pub fn take_all(&mut self) -> Vec<Arc<gpui::RenderImage>> {
        std::mem::take(&mut self.textures).into_values().collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.textures.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }
}

/// RGBA 像素 → GPUI 纹理（BGRA 预乘）。
///
/// GPUI 的精灵图集要求 BGRA 且颜色分量已除以 alpha（见 `swap_rgba_pa_to_bgra`），
/// 直接喂 RGBA 会得到红蓝互换的成品。
pub fn to_render_image(image: &DecodedImage) -> gpui::RenderImage {
    to_render_image_from(image.width, image.height, &image.rgba)
}

/// 与 [`to_render_image`] 相同，但直接接受一段 RGBA 缓冲（动画帧没有自己的
/// `DecodedImage`）。
pub fn to_render_image_from(width: u32, height: u32, rgba: &[u8]) -> gpui::RenderImage {
    let mut rgba = rgba.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        gpui::swap_rgba_pa_to_bgra(pixel);
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba).expect("尺寸与缓冲长度一致");
    gpui::RenderImage::new(vec![image::Frame::new(buffer)])
}

/// 推进所有正在播放的动画，并返回下一次帧切换的延迟（毫秒）。
///
/// `now_ms` 由调用方提供的单调时钟给出。返回 `None` 表示没有动画需要未来的
/// tick（没有运行中的动画，或都已停住）。调用方据此安排一次重绘唤醒。
///
/// 返回 `Some(delay)` 时**至少有一个动画的显示帧可能变了**，调用方必须重绘；
/// 但"该重绘"的真实来源是存储代际（[`Storage`] 里的 `generation`），这里只是
/// 定时器：调用方 tick 完仍需去检查代际是否变化。
///
/// 未放置的图像不推进：这是"图像真的可见"的简化近似，作用是不让一张传了却
/// 从没显示过的图像永远唤醒渲染器。
pub fn animation_tick(storage: &mut Storage, now_ms: u64) -> Option<u64> {
    storage.animation_tick(now_ms)
}

/// 动画播放的时钟与唤醒调度。
///
/// 只要还有动画在跑，屏幕上每一帧都可能不一样——但 `render` 只能在"确实该换
/// 帧了"的时候重新调度，否则就会以显示器刷新率空转。这个类型把两件事分开：
///
/// - **时钟**：一个进程内的单调起点（`Instant`），对外只暴露毫秒偏移。
///   用偏移而非 `SystemTime` 是因为动画间隔是相对量，系统时钟回拨不该让
///   动画卡住或疯狂追帧。
/// - **调度**：记住上一次 tick 得到的"距下次换帧还有多久"，只有真正到点才
///   让调用方重新渲染。调用方每帧问一次 [`AnimationClock::due`]。
///
/// 与 kitty/ghostty 的差异：那边由渲染器自身的帧循环驱动（渲染器每个 vsync
/// 都 tick），这里 GPUI 只在有变化时才重绘，所以必须自己安排唤醒。
#[derive(Debug)]
pub struct AnimationClock {
    /// 单调时钟起点。第一次 [`AnimationClock::tick`] 时惰性建立。
    epoch: Option<std::time::Instant>,
    /// 上一次 tick 排定的换帧截止时刻（毫秒，相对 `epoch`）。
    deadline_ms: Option<u64>,
}

impl Default for AnimationClock {
    fn default() -> Self {
        Self::new()
    }
}

impl AnimationClock {
    pub fn new() -> Self {
        Self {
            epoch: None,
            deadline_ms: None,
        }
    }

    /// 单调时钟起点，惰性建立；惰性意味着构造 `AnimationClock` 不需要 `Instant::now()`。
    fn epoch(&mut self) -> std::time::Instant {
        *self.epoch.get_or_insert_with(std::time::Instant::now)
    }

    /// 距离起点多少毫秒（同一进程内单调递增）。
    ///
    /// 传给 [`animation_tick`] 的时间戳必须来自这里，保证所有调用用同一时钟。
    pub fn now_ms(&mut self) -> u64 {
        let epoch = self.epoch();
        // 单调时钟不会倒流；饱和转换只是防御 u128 → u64 的极端情况。
        u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// 登记一次 `animation_tick` 的结果。
    ///
    /// `delay` 为 `Some(d)` 时排定 `now + d` 为下次换帧的截止时刻；为 `None`
    /// （没有动画需要推进）时清掉在途计划，让调用方停止重新调度。
    pub fn schedule(&mut self, delay: Option<u64>) {
        self.deadline_ms = delay.map(|d| self.now_ms().saturating_add(d));
    }

    /// 是否已到（或已过）上次排定的换帧时刻。
    ///
    /// 没有排定截止时刻（没有动画）时恒为 `false`。
    pub fn due(&mut self) -> bool {
        match self.deadline_ms {
            Some(deadline) => self.now_ms() >= deadline,
            None => false,
        }
    }

    /// 距离下次换帧还有多久；已过期（或没有计划）时返回一个极小的非零延迟，
    /// 让定时任务立刻唤醒一轮而不是退化成忙等。
    pub fn time_until_due(&mut self) -> std::time::Duration {
        let now = self.now_ms();
        match self.deadline_ms {
            Some(deadline) if deadline > now => std::time::Duration::from_millis(deadline - now),
            _ => std::time::Duration::from_millis(1),
        }
    }

    /// 是否有一个在途的换帧计划。
    pub fn is_pending(&self) -> bool {
        self.deadline_ms.is_some()
    }
}

/// 按 [`plan`] 的结果把图像画进窗口。
///
/// `below` 为 `true` 时只画文字下方的放置，`false` 时只画文字上方的——
/// 调用方在文字前后各调一次即可得到正确的层级。
///
/// 接受 `Arc<Mutex<Storage>>` 而非 `&Storage`：纹理创建要调用 GPUI（`cx`），
/// 而调用方往往还持着终端实体的借用；自行加锁可以让借用尽早结束。
pub fn paint_images(
    placed: &[PlacedImage],
    below: bool,
    storage: &std::sync::Arc<std::sync::Mutex<Storage>>,
    cache: &mut ImageCache,
    window: &mut Window,
    cx: &mut App,
) {
    if placed.iter().all(|p| p.below_text != below) {
        return;
    }
    let guard = storage.lock().expect("kitty 图像存储锁被投毒");

    for item in placed.iter().filter(|p| p.below_text == below) {
        let Some(stored) = guard.get(item.id) else {
            continue;
        };
        // 源矩形是在「整幅图像」的坐标系里给出的，换算比例要用图像自身的
        // 像素尺寸；动画换帧不改变尺寸，所以这里用 `stored.image` 就够。
        let (image_width, image_height) = (stored.image.width as f32, stored.image.height as f32);

        let Some(texture) = cache.get_or_insert(item.id, &guard, cx) else {
            continue;
        };
        let rect = Bounds::new(item.origin, item.size);
        // `bounds` 是真正要画的区域，`image_bounds` 是「整幅图像如果照这个缩放
        // 铺开会落在哪」。两者相同即整幅图原位铺满；要只画图像的一小块时，
        // 按源矩形的比例把 `image_bounds` 反推出去，让那一小块正好落在
        // `bounds` 上——正对应 GPUI 内部用 `visible_bounds ∩ image_bounds`
        // 取子贴图的逻辑。
        //
        // 比例 = 源片段尺寸 / 整幅图像尺寸，逐轴独立（横竖缩放可以不同）。
        let scale_x = (item.source.size.width / px(image_width.max(1.0))).max(f32::MIN_POSITIVE);
        let scale_y = (item.source.size.height / px(image_height.max(1.0))).max(f32::MIN_POSITIVE);
        let image_bounds = Bounds::new(
            Point::new(
                rect.origin.x - item.source.origin.x / scale_x,
                rect.origin.y - item.source.origin.y / scale_y,
            ),
            Size::new(rect.size.width / scale_x, rect.size.height / scale_y),
        );
        if let Err(err) =
            window.paint_image(rect, image_bounds, Corners::default(), texture, 0, false)
        {
            log::warn!("绘制 Kitty 图像失败: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::kitty::storage::{Placement, Storage};

    fn image(w: u32, h: u32) -> DecodedImage {
        DecodedImage {
            width: w,
            height: h,
            rgba: vec![0u8; (w * h * 4) as usize],
        }
    }

    fn metrics() -> CellMetrics {
        CellMetrics {
            cell_width: px(10.0),
            line_height: px(20.0),
        }
    }

    fn viewport() -> Viewport {
        Viewport {
            origin: Point::new(px(100.0), px(50.0)),
            display_offset: 0,
            rows: 24,
            cols: 80,
        }
    }

    #[test]
    fn natural_size_comes_from_pixel_dimensions() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(30, 40), None, false);
        s.place(
            1,
            0,
            Placement {
                row: 0,
                col: 0,
                columns: None,
                rows: None,
                x_offset: 0,
                y_offset: 0,
                z: 0,
                ..Default::default()
            },
        );

        let plan = plan(&s, metrics(), viewport());
        assert_eq!(plan.len(), 1);
        // 30px 宽 / 10px 单元格 = 3 列；40px / 20px = 2 行。
        assert_eq!(plan[0].size.width, px(30.0));
        assert_eq!(plan[0].size.height, px(40.0));
        assert_eq!(plan[0].origin, Point::new(px(100.0), px(50.0)));
    }

    #[test]
    fn explicit_columns_stretch_and_derive_rows() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(30, 40), None, false);
        // 显式 6 列：行数按宽高比推导 → 6 * (2/3) = 4 行。
        s.place(
            1,
            0,
            Placement {
                row: 0,
                col: 0,
                columns: Some(6),
                rows: None,
                x_offset: 0,
                y_offset: 0,
                z: 0,
                ..Default::default()
            },
        );

        let plan = plan(&s, metrics(), viewport());
        assert_eq!(plan[0].size.width, px(60.0));
        assert_eq!(plan[0].size.height, px(80.0));
    }

    #[test]
    fn offsets_are_applied_in_pixels() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        s.place(
            1,
            0,
            Placement {
                row: 1,
                col: 2,
                columns: Some(1),
                rows: Some(1),
                x_offset: 3,
                y_offset: 5,
                z: 0,
                ..Default::default()
            },
        );

        let plan = plan(&s, metrics(), viewport());
        // 列 2 → 20px，行 1 → 20px，再加偏移。
        assert_eq!(plan[0].origin, Point::new(px(120.0 + 3.0), px(70.0 + 5.0)));
    }

    #[test]
    fn scroll_offset_moves_images_with_text() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        s.place(
            1,
            0,
            Placement {
                row: 0,
                col: 0,
                columns: Some(1),
                rows: Some(1),
                x_offset: 0,
                y_offset: 0,
                z: 0,
                ..Default::default()
            },
        );

        let mut vp = viewport();
        vp.display_offset = 5;
        let plan = plan(&s, metrics(), vp);
        // 向上滚 5 行后，锚在 row 0 的图像下移 5 行。
        assert_eq!(plan[0].origin.y, px(50.0 + 100.0));
    }

    #[test]
    fn negative_z_is_below_text() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        s.insert(2, image(10, 20), None, false);
        s.place(
            1,
            0,
            Placement {
                row: 0,
                col: 0,
                columns: Some(1),
                rows: Some(1),
                x_offset: 0,
                y_offset: 0,
                z: -1,
                ..Default::default()
            },
        );
        s.place(
            2,
            0,
            Placement {
                row: 0,
                col: 0,
                columns: Some(1),
                rows: Some(1),
                x_offset: 0,
                y_offset: 0,
                z: 0,
                ..Default::default()
            },
        );

        let plan = plan(&s, metrics(), viewport());
        assert!(plan[0].below_text, "z < 0 应画在文字之下");
        assert!(!plan[1].below_text, "z >= 0 应画在文字之上");
    }

    #[test]
    fn images_scrolled_out_of_view_are_dropped() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        // 放置在第 100 行，视口只有 24 行。
        s.place(
            1,
            0,
            Placement {
                row: 99,
                col: 0,
                columns: Some(1),
                rows: Some(1),
                x_offset: 0,
                y_offset: 0,
                z: 0,
                ..Default::default()
            },
        );

        assert!(
            plan(&s, metrics(), viewport()).is_empty(),
            "视口外的放置应被丢弃"
        );
    }

    #[test]
    fn draw_order_sorts_by_z_ascending() {
        let mut s = Storage::new(1024 * 1024);
        for id in 1..=3u32 {
            s.insert(id, image(10, 20), None, false);
        }
        // 故意使用与 z 相反的顺序插入。
        s.place(
            1,
            0,
            Placement {
                row: 0,
                col: 0,
                columns: Some(1),
                rows: Some(1),
                x_offset: 0,
                y_offset: 0,
                z: 5,
                ..Default::default()
            },
        );
        s.place(
            2,
            0,
            Placement {
                row: 0,
                col: 1,
                columns: Some(1),
                rows: Some(1),
                x_offset: 0,
                y_offset: 0,
                z: -1,
                ..Default::default()
            },
        );
        s.place(
            3,
            0,
            Placement {
                row: 0,
                col: 2,
                columns: Some(1),
                rows: Some(1),
                x_offset: 0,
                y_offset: 0,
                z: 2,
                ..Default::default()
            },
        );

        let plan = plan(&s, metrics(), viewport());
        let zs: Vec<i32> = plan.iter().map(|p| p.z).collect();
        assert_eq!(zs, vec![-1, 2, 5], "应按 z 升序绘制");
    }

    #[test]
    fn shrink_to_fit_preserves_aspect_ratio() {
        let mut s = Storage::new(1024 * 1024);
        // 10 列 × 100 行的图像（宽高比 1:10），放在最后一行。
        s.insert(1, image(10, 2000), None, false);
        s.place(
            1,
            0,
            Placement {
                row: 23,
                col: 0,
                columns: Some(1),
                rows: None,
                x_offset: 0,
                y_offset: 0,
                z: 0,
                ..Default::default()
            },
        );

        let plan = plan(&s, metrics(), viewport());
        assert_eq!(plan.len(), 1);
        // 只剩 1 行可用（第 23 行是最后一行）。自然尺寸是 1 列 × 100 行，
        // 压到 1 行后按比例收缩宽度：1 列 × (1/100) = 0.1px。
        assert_eq!(plan[0].size.height, px(20.0));
        assert!(
            (plan[0].size.width.as_f32() - 1.0).abs() < 1.0,
            "宽度应按比例同步收缩，实际 {}",
            plan[0].size.width
        );
    }

    #[test]
    fn placement_without_placement_entries_is_not_drawn() {
        let mut s = Storage::new(1024 * 1024);
        // 只传输不显示：图像存在（transient）但无放置。
        s.insert(1, image(10, 20), None, true);
        assert!(plan(&s, metrics(), viewport()).is_empty());
    }

    #[test]
    fn texture_conversion_is_bgra_premultiplied() {
        let image = DecodedImage {
            width: 1,
            height: 1,
            // 半透明红：RGBA = (255, 0, 0, 128)。
            rgba: vec![255, 0, 0, 128],
        };
        let texture = to_render_image(&image);
        let bytes = texture.as_bytes(0).expect("单帧应可取字节");
        // BGRA 且预乘：R/B 互换后 R=B=0，再除以 a≈0.502 得回 255。
        assert_eq!(bytes[0], 0, "B 分量");
        assert_eq!(bytes[1], 0, "G 分量");
        assert_eq!(bytes[2], 255, "R 分量（预乘后应还原）");
        assert_eq!(bytes[3], 128, "A 分量不变");
    }

    #[test]
    fn cache_reuses_texture_for_same_generation() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        let mut cache = ImageCache::new();
        let app = gpui::TestAppContext::single();
        app.update(|cx| {
            let a = cache.get_or_insert(1, &s, cx).expect("应生成纹理");
            let b = cache.get_or_insert(1, &s, cx).expect("应复用纹理");
            assert!(Arc::ptr_eq(&a, &b), "同代际应复用同一纹理");
            assert_eq!(cache.len(), 1);
        });
    }

    #[test]
    fn cache_drops_texture_when_image_replaced() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        let generation_before = s.get(1).unwrap().generation;

        let mut cache = ImageCache::new();
        let app = gpui::TestAppContext::single();
        app.update(|cx| {
            let old = cache.get_or_insert(1, &s, cx).expect("应生成纹理");

            // 同一 id 重新传输：代际变化，旧纹理必须失效。
            s.insert(1, image(20, 20), None, false);
            assert_ne!(s.get(1).unwrap().generation, generation_before);

            // prune 必须把旧纹理本身交出来，调用方才能 `drop_image` 释放
            // GPU 图集 tile（只 drop `Arc` 不释放 tile，图集侧会无界增长）。
            let removed = cache.prune(&s);
            assert_eq!(removed.len(), 1, "旧代际纹理应被释放");
            assert!(
                Arc::ptr_eq(&removed[0], &old),
                "返回的应是同一份纹理，否则调用方无法释放正确的图集 tile"
            );
            cache.get_or_insert(1, &s, cx).expect("应生成新纹理");
            assert_eq!(cache.len(), 1, "缓存不应无限增长");
        });
    }

    #[test]
    fn prune_removes_deleted_images() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        s.insert(2, image(10, 20), None, false);
        // 对齐 ghostty `markForUnload`：只有被放置的图像才留纹理。
        s.place(1, 0, Placement::default());
        s.place(2, 0, Placement::default());

        let mut cache = ImageCache::new();
        let app = gpui::TestAppContext::single();
        app.update(|cx| {
            cache.get_or_insert(1, &s, cx);
            cache.get_or_insert(2, &s, cx);
            assert_eq!(cache.len(), 2);

            s.remove(1);
            assert_eq!(cache.prune(&s).len(), 1, "被删除图像的纹理应释放");
            assert_eq!(cache.len(), 1);
        });
    }

    #[test]
    fn prune_unloads_images_without_placements() {
        // 对齐 ghostty `markForUnload`：placement 被删光但数据保留的图像，
        // 卸 GPU 纹理、留终端数据；重新放置后按需重建。
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(10, 20), None, false);
        s.place(1, 0, Placement::default());

        let mut cache = ImageCache::new();
        let app = gpui::TestAppContext::single();
        app.update(|cx| {
            cache.get_or_insert(1, &s, cx).expect("应生成纹理");
            assert!(cache.prune(&s).is_empty(), "有放置时不应裁剪");

            assert!(s.remove_placement(1, 0));
            assert!(s.get(1).is_some(), "数据应保留");
            assert_eq!(cache.prune(&s).len(), 1, "无放置应卸纹理");
            assert!(cache.is_empty());

            // 重新放置后纹理可重建。
            s.place(1, 0, Placement::default());
            cache.get_or_insert(1, &s, cx).expect("应重建纹理");
            assert_eq!(cache.len(), 1);
        });
    }

    // ---- AnimationClock ----

    #[test]
    fn fresh_clock_has_nothing_pending() {
        let clock = AnimationClock::new();
        assert!(!clock.is_pending(), "刚构造时不应有在途计划");
    }

    #[test]
    fn schedule_none_clears_the_plan() {
        let mut clock = AnimationClock::new();
        clock.schedule(Some(100));
        assert!(clock.is_pending());

        clock.schedule(None);
        assert!(!clock.is_pending(), "没有动画时应清掉计划");
        assert!(!clock.due(), "没有计划时永远不算到点");
    }

    #[test]
    fn schedule_some_is_not_due_immediately() {
        let mut clock = AnimationClock::new();
        clock.schedule(Some(5_000));
        assert!(!clock.due(), "刚排定的未来截止时刻不该立刻到点");
        let until = clock.time_until_due();
        assert!(
            until <= std::time::Duration::from_millis(5_000),
            "剩余等待不应超过排定的延迟"
        );
        assert!(
            until > std::time::Duration::from_millis(4_000),
            "剩余等待应接近排定的延迟"
        );
    }

    #[test]
    fn time_until_due_is_never_zero_without_a_plan() {
        let mut clock = AnimationClock::new();
        // 没有计划时返回极短延迟而不是 0：定时器才不会退化成忙等。
        assert!(clock.time_until_due() > std::time::Duration::ZERO);
    }

    #[test]
    fn zero_delay_becomes_due() {
        let mut clock = AnimationClock::new();
        clock.schedule(Some(0));
        assert!(clock.due(), "0ms 延迟应立刻到点");
    }

    // ---- Unicode 占位符几何（数值来自 ghostty 同名测试）----

    /// 一只 500x306 的狗铺进 36x80 的单元格。
    const DOG: (u32, u32) = (500, 306);
    const CW: f64 = 36.0;
    const CH: f64 = 80.0;

    #[test]
    fn placeholder_rect_dog_4x2_row1() {
        // 4 列 2 行：横向铺满（144px 宽），竖向按比例缩到 88px，上下各留 36px。
        // 第 1 个片段取上半段（源 153px 高），目标高只有 44px。
        let r = placeholder_rect(DOG.0, DOG.1, Some(4), Some(2), 0, 0, 4, CW, CH).unwrap();
        assert_eq!(r.dest_offset.x, px(0.0));
        assert_eq!(r.dest_offset.y, px(36.0));
        assert_eq!(r.source_origin.x, px(0.0));
        assert_eq!(r.source_origin.y, px(0.0));
        assert_eq!(r.source_size.width, px(500.0));
        assert_eq!(r.source_size.height, px(153.0));
        assert_eq!(r.dest_size.width, px(144.0));
        assert_eq!(r.dest_size.height, px(44.0));
    }

    #[test]
    fn placeholder_rect_dog_4x2_row2() {
        // 第 2 行：取下半段（源 y=153），目标不再有上偏移。
        let r = placeholder_rect(DOG.0, DOG.1, Some(4), Some(2), 0, 1, 4, CW, CH).unwrap();
        assert_eq!(r.dest_offset.x, px(0.0));
        assert_eq!(r.dest_offset.y, px(0.0));
        assert_eq!(r.source_origin.x, px(0.0));
        assert_eq!(r.source_origin.y, px(153.0));
        assert_eq!(r.source_size.width, px(500.0));
        assert_eq!(r.source_size.height, px(153.0));
        assert_eq!(r.dest_size.width, px(144.0));
        assert_eq!(r.dest_size.height, px(44.0));
    }

    #[test]
    fn placeholder_rect_dog_2x2_with_blank_cells() {
        // 2 列 2 行：格子只有 72x160，图像偏宽 → 宽度铺满，竖向留白更多。
        let r = placeholder_rect(DOG.0, DOG.1, Some(2), Some(2), 0, 0, 4, CW, CH).unwrap();
        assert_eq!(r.dest_offset.x, px(0.0));
        assert_eq!(r.dest_offset.y, px(58.0));
        assert_eq!(r.source_origin.y, px(0.0));
        assert_eq!(r.source_size.width, px(500.0));
        assert_eq!(r.source_size.height, px(153.0));
        assert_eq!(r.dest_size.width, px(72.0));
        assert_eq!(r.dest_size.height, px(22.0));

        let r2 = placeholder_rect(DOG.0, DOG.1, Some(2), Some(2), 0, 1, 4, CW, CH).unwrap();
        assert_eq!(r2.dest_offset.y, px(0.0));
        assert_eq!(r2.source_origin.y, px(153.0));
        assert_eq!(r2.dest_size.width, px(72.0));
        assert_eq!(r2.dest_size.height, px(22.0));
    }

    #[test]
    fn placeholder_rect_dog_1x1() {
        // 1 列 1 行：格 36x80，图像远宽于格 → 宽度铺满 36px，整幅图缩到 22px 高，
        // 上下各留 29px。单个片段就覆盖整幅图像。
        let r = placeholder_rect(DOG.0, DOG.1, Some(1), Some(1), 0, 0, 1, CW, CH).unwrap();
        assert_eq!(r.dest_offset.x, px(0.0));
        assert_eq!(r.dest_offset.y, px(29.0));
        assert_eq!(r.source_origin.x, px(0.0));
        assert_eq!(r.source_origin.y, px(0.0));
        assert_eq!(r.source_size.width, px(500.0));
        assert_eq!(r.source_size.height, px(306.0));
        assert_eq!(r.dest_size.width, px(36.0));
        assert_eq!(r.dest_size.height, px(22.0));
    }

    /// 片段完全落在图像之外的可视区域 → 不画。
    #[test]
    fn placeholder_rect_outside_image_is_none() {
        // 1x1 网格里请求第 1 行（不存在的那一行）。
        assert!(placeholder_rect(DOG.0, DOG.1, Some(1), Some(1), 0, 5, 1, CW, CH).is_none());
    }

    /// 未指定 c=/r= 时按图像像素尺寸推导格子数（1:1 铺开）。
    #[test]
    fn placeholder_rect_derives_grid_from_image_size() {
        let r = placeholder_rect(100, 50, None, None, 0, 0, 1, 10.0, 10.0).unwrap();
        // 10 列 × 5 行 → 100x50 的图正好铺满，无留白。
        // 单个片段只占 1 列 = 1/10 的图像宽（源 10px），铺进 1 个单元格。
        assert_eq!(r.dest_offset.x, px(0.0));
        assert_eq!(r.dest_offset.y, px(0.0));
        assert_eq!(r.source_size.width, px(10.0));
        assert_eq!(r.dest_size.width, px(10.0));
        // 竖向：5 行 → 每行 1/5 图像高（源 10px），铺进 10px 单元格高。
        assert_eq!(r.source_size.height, px(10.0));
        assert_eq!(r.dest_size.height, px(10.0));

        // 整行 10 段拼起来应当覆盖整幅图。
        let mut total = 0.0f32;
        for col in 0..10 {
            let seg = placeholder_rect(100, 50, None, None, col, 0, 1, 10.0, 10.0).unwrap();
            total += seg.source_size.width.as_f32();
        }
        assert_eq!(total, 100.0, "10 段源宽之和应等于图像宽");
    }

    /// 多段横排时，各段的源矩形应当首尾相接、拼出整幅图宽。
    #[test]
    fn placeholder_rect_row_segments_tile_the_image() {
        // 4 列 1 行，每段 1 列。
        let mut total_width = 0.0f32;
        let mut next_x = 0.0f32;
        for col in 0..4 {
            let r = placeholder_rect(DOG.0, DOG.1, Some(4), Some(1), col, 0, 1, CW, CH).unwrap();
            assert_eq!(
                r.source_origin.x,
                px(next_x),
                "第 {col} 段的源 x 应接上前一段"
            );
            total_width += r.source_size.width.as_f32();
            next_x = r.source_origin.x.as_f32() + r.source_size.width.as_f32();
        }
        assert_eq!(total_width, 500.0, "四段源宽之和应等于图像宽");
    }

    // ---- 占位符落位（plan_placeholders）----

    use crate::terminal::kitty::storage::PlacementLocation;
    use alacritty_terminal::vte::ansi::Color as AlacColor;

    /// 变音符表的下标 0/1/2（'\u{0305}' / '\u{030D}' / '\u{030E}'）。
    const D0: char = '\u{0305}';
    const D1: char = '\u{030D}';
    const D2: char = '\u{030E}';

    fn diacritic_for(index: u32) -> char {
        match index {
            0 => D0,
            1 => D1,
            _ => D2,
        }
    }

    /// 建一张图 + 一个覆盖 `cols x rows` 格的虚拟放置。
    fn storage_with_virtual(id: u32, cols: u32, rows: u32) -> Storage {
        let mut s = Storage::new(1024 * 1024);
        s.insert(id, image(500, 306), None, false);
        s.place(
            id,
            0,
            Placement {
                columns: Some(cols),
                rows: Some(rows),
                location: PlacementLocation::Virtual,
                ..Default::default()
            },
        );
        s
    }

    /// 一个占位符单元格：前景色给出图像 id，变音符给出图像内的行/列。
    ///
    /// 顺序按规范：第 1 个变音符是**行**、第 2 个是**列**、第 3 个是图像 id
    /// 的高 8 位（这里不写）。
    fn placeholder_cell(
        row: i32,
        col: u32,
        id: u32,
        img_row: u32,
        img_col: u32,
    ) -> PlaceholderCell {
        PlaceholderCell {
            row,
            col,
            fg: AlacColor::Indexed(id as u8),
            underline_color: None,
            zerowidth: Some([diacritic_for(img_row), diacritic_for(img_col), D0]),
        }
    }

    /// 单个占位符 → 一个片段，且原点被居中留白修正回单元格顶。
    #[test]
    fn placeholder_single_cell_produces_one_fragment() {
        let s = storage_with_virtual(1, 4, 2);
        let cells = [placeholder_cell(0, 0, 1, 0, 0)];
        let plan = plan_placeholders(&s, &cells, metrics(), viewport());

        assert_eq!(plan.len(), 1);
        let p = &plan[0];
        assert_eq!(p.id, 1);
        assert_eq!(p.source.origin.x, px(0.0));
        assert_eq!(p.source.origin.y, px(0.0));
        assert_eq!(p.size.width, px(10.0), "1 列 = 1 个单元格宽");
        // 与独立算出来的几何完全一致，避免测试里重抄一遍魔数。
        let expected = placeholder_rect(500, 306, Some(4), Some(2), 0, 0, 1, 10.0, 20.0).unwrap();
        assert_eq!(p.source.origin, expected.source_origin);
        assert_eq!(p.source.size, expected.source_size);
        assert_eq!(p.size, expected.dest_size);
        // 目标原点是单元格左上角减去居中留白的偏移。
        assert_eq!(
            p.origin,
            Point::new(
                viewport().origin.x - expected.dest_offset.x,
                viewport().origin.y - expected.dest_offset.y,
            )
        );
    }

    /// 一行里相邻的两个占位符合并成一段，目标宽翻倍。
    #[test]
    fn placeholder_run_merges_adjacent_cells() {
        let s = storage_with_virtual(1, 4, 2);
        // 第 0 格声明 col=0，第 1 格声明 col=1（正好接上）→ 合并成一段。
        let cells = [
            placeholder_cell(0, 0, 1, 0, 0),
            placeholder_cell(0, 1, 1, 0, 1),
        ];
        let plan = plan_placeholders(&s, &cells, metrics(), viewport());

        assert_eq!(plan.len(), 1, "相邻两格应合并成一段");
        let one = placeholder_rect(500, 306, Some(4), Some(2), 0, 0, 1, 10.0, 20.0).unwrap();
        let two = placeholder_rect(500, 306, Some(4), Some(2), 0, 0, 2, 10.0, 20.0).unwrap();
        assert_eq!(plan[0].size.width, two.dest_size.width);
        assert!(
            plan[0].size.width > one.dest_size.width,
            "合并后的段应比单格宽"
        );
    }

    /// 源矩形首尾相接：第二个片段的源 x 正好接上第一个的右边界。
    #[test]
    fn placeholder_fragments_tile_the_source_image() {
        let s = storage_with_virtual(1, 4, 2);
        let cells = [
            placeholder_cell(0, 0, 1, 0, 0),
            placeholder_cell(0, 1, 1, 0, 1),
        ];
        let plan = plan_placeholders(&s, &cells, metrics(), viewport());
        assert_eq!(plan.len(), 1);

        // 单段与双段的几何都独立算一遍：单段的源宽应当是双段的一半。
        let one = placeholder_rect(500, 306, Some(4), Some(2), 0, 0, 1, 10.0, 20.0).unwrap();
        let two = placeholder_rect(500, 306, Some(4), Some(2), 0, 0, 2, 10.0, 20.0).unwrap();
        assert_eq!(
            one.source_size.width.as_f32() * 2.0,
            two.source_size.width.as_f32()
        );
        assert_eq!(plan[0].source.size.width, two.source_size.width);
    }

    /// 找不到虚拟放置 → 不画（客户端可能还没传图）。
    #[test]
    fn placeholder_without_any_matching_placement_is_skipped() {
        let mut s = Storage::new(1024 * 1024);
        s.insert(1, image(500, 306), None, false);
        // 图像存在但没有任何放置（更不用说虚拟放置）：零 id 回退必须落空。
        let cells = [placeholder_cell(0, 0, 1, 0, 0)];
        assert!(plan_placeholders(&s, &cells, metrics(), viewport()).is_empty());
    }

    /// 图像不存在 → 不画。
    #[test]
    fn placeholder_with_missing_image_is_skipped() {
        let s = storage_with_virtual(1, 4, 2);
        let cells = [placeholder_cell(0, 0, 9, 0, 0)];
        assert!(plan_placeholders(&s, &cells, metrics(), viewport()).is_empty());
    }

    /// 滚动后占位符跟着文字一起移动。
    #[test]
    fn placeholder_follows_scroll_offset() {
        let s = storage_with_virtual(1, 4, 2);
        let cells = [placeholder_cell(5, 0, 1, 0, 0)];

        let at_zero = plan_placeholders(&s, &cells, metrics(), viewport());
        let mut scrolled = viewport();
        scrolled.display_offset = 3;
        let after = plan_placeholders(&s, &cells, metrics(), scrolled);

        assert_eq!(at_zero.len(), 1);
        assert_eq!(after.len(), 1);
        // display_offset +3 = 视口向上滚 3 行 = 内容整体下移 3 行 = 60px
        // （与 plan 的 scroll_offset_moves_images_with_text 同一约定）。
        assert_eq!(after[0].origin.y - at_zero[0].origin.y, px(60.0));
    }

    /// 行落在视口之外 → 不画。
    #[test]
    fn placeholder_out_of_viewport_is_skipped() {
        let s = storage_with_virtual(1, 4, 2);
        // 视口 24 行；网格行 30 已滚出下边界。
        let cells = [placeholder_cell(30, 0, 1, 0, 0)];
        assert!(plan_placeholders(&s, &cells, metrics(), viewport()).is_empty());
    }

    /// 多行的占位符各自成段，不会串行。
    #[test]
    fn placeholders_on_multiple_rows_are_separate() {
        let s = storage_with_virtual(1, 4, 2);
        let cells = [
            placeholder_cell(0, 0, 1, 0, 0),
            placeholder_cell(1, 0, 1, 1, 0),
        ];
        let plan = plan_placeholders(&s, &cells, metrics(), viewport());

        assert_eq!(plan.len(), 2, "不同行不应合并");
        assert_eq!(plan[0].source.origin.y, px(0.0), "第 1 行取源上半");
        assert_eq!(plan[1].source.origin.y, px(153.0), "第 2 行取源下半");
    }

    /// 空输入不产生任何片段。
    #[test]
    fn empty_placeholder_input_is_empty() {
        let s = storage_with_virtual(1, 4, 2);
        assert!(plan_placeholders(&s, &[], metrics(), viewport()).is_empty());
    }

    /// 乱序输入会被 debug 断言拦下（单趟分组依赖升序，否则行会串在一起）。
    #[test]
    #[should_panic(expected = "按 (行, 列) 升序")]
    fn unsorted_placeholder_input_trips_debug_assert() {
        let s = storage_with_virtual(1, 4, 2);
        let cells = [
            placeholder_cell(1, 0, 1, 1, 0),
            placeholder_cell(0, 0, 1, 0, 0),
        ];
        let _ = plan_placeholders(&s, &cells, metrics(), viewport());
    }
}
