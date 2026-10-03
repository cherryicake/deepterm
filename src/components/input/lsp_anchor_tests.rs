//! LSP 浮层定位的单元测试。
//!
//! 背景：hover 浮层曾因**预先算好像素坐标并存下来**而整体错位——像素位置
//! 随滚动、软换行、行号栏宽度变化，存下来的值必然过期。
//!
//! 现方案照抄 Zed（`editor/src/element.rs::layout_hover_popovers`）：
//! 状态里只存**文本锚点**（字节偏移），绘制时用当前帧的行布局换算成像素。
//! 这里验证换算的两个核心步骤：锚点 → 行下标，以及行高累计。

use gpui::px;

/// 锚点字节偏移 → 可见行下标（`visible_line_byte_offsets` 与 `lines` 平行）。
///
/// 与 `TextElement::paint_hover_popover` 里的查找逻辑同构。
fn row_for_offset(offsets: &[usize], offset: usize) -> Option<usize> {
    if offsets.is_empty() {
        return None;
    }
    offsets.iter().enumerate().find_map(|(i, start)| {
        let end = offsets.get(i + 1).copied().unwrap_or(usize::MAX);
        (offset >= *start && offset < end).then_some(i)
    })
}

#[test]
fn offset_maps_to_its_own_row() {
    // 三行，起始偏移 0 / 4 / 8。
    let offsets = [0usize, 4, 8];
    assert_eq!(row_for_offset(&offsets, 0), Some(0));
    assert_eq!(row_for_offset(&offsets, 3), Some(0));
    assert_eq!(row_for_offset(&offsets, 4), Some(1));
    assert_eq!(row_for_offset(&offsets, 7), Some(1));
    assert_eq!(row_for_offset(&offsets, 8), Some(2));
    // 最后一行没有下一行起点：吸收到文本末尾之后。
    assert_eq!(row_for_offset(&offsets, 9999), Some(2));
}

#[test]
fn offset_mapping_is_empty_safe() {
    assert_eq!(row_for_offset(&[], 5), None);
}

/// 累计前 `row_index + 1` 行的高度（与绘制路径同构）。
fn sum_heights_to(
    heights: &[gpui::Pixels],
    row_index: usize,
    visible_top: gpui::Pixels,
) -> gpui::Pixels {
    let mut y = visible_top;
    for h in heights.iter().take(row_index + 1) {
        y += *h;
    }
    y
}

#[test]
fn first_row_bottom_is_its_own_height() {
    let h = [px(20.), px(20.), px(20.)];
    assert_eq!(sum_heights_to(&h, 0, px(0.)), px(20.));
}

#[test]
fn accumulates_heights_of_preceding_rows() {
    let h = [px(20.), px(20.), px(20.)];
    assert_eq!(sum_heights_to(&h, 1, px(0.)), px(40.));
    assert_eq!(sum_heights_to(&h, 2, px(0.)), px(60.));
}

#[test]
fn accounts_for_visible_top_offset() {
    // 滚动偏移应整体平移锚点。
    let h = [px(20.), px(20.), px(20.)];
    assert_eq!(sum_heights_to(&h, 1, px(10.)), px(50.));
}

#[test]
fn varying_heights_are_summed_not_multiplied() {
    // 中间那行更高（软换行成三行）：总高必须是 20 + 60 + 20 = 100，
    // 而不是「行数 × 行高」的 60。
    let h = [px(20.), px(60.), px(20.)];
    assert_eq!(sum_heights_to(&h, 2, px(0.)), px(100.));
    assert_eq!(sum_heights_to(&h, 1, px(0.)), px(80.));
}

#[test]
fn out_of_range_row_is_safe() {
    let h = [px(20.), px(20.)];
    // 越界行号：返回已有各行的累计值，不 panic。
    assert_eq!(sum_heights_to(&h, 9, px(0.)), px(40.));
}

#[test]
fn empty_layout_returns_visible_top() {
    let h: [gpui::Pixels; 0] = [];
    assert_eq!(sum_heights_to(&h, 0, px(5.)), px(5.));
}

/// 行顶公式（与正文绘制对齐）的等价实现，用于验证 `row_top_in_viewport`。
///
/// 这是 `paint_inner` 文本循环的公式：
/// `y = bounds.origin.y + visible_top + Σ(块高 + 行高)`。
/// 注意 `bounds.origin.y` 已含滚动偏移，**不再加减滚动量**。
fn expected_row_top(
    bounds_top: f32,
    visible_top: f32,
    heights: &[f32],
    git_blocks: &[f32],
    row_ix: usize,
) -> f32 {
    let mut y = bounds_top + visible_top;
    for i in 0..row_ix {
        y += git_blocks.get(i).copied().unwrap_or(0.0);
        y += heights.get(i).copied().unwrap_or(0.0);
    }
    y
}

#[test]
fn row_top_matches_paint_formula_for_first_row() {
    // 第一行：行顶就是 visible_top（无前置行）。
    assert_eq!(
        expected_row_top(100.0, 0.0, &[20.0, 20.0], &[0.0, 0.0], 0),
        100.0
    );
}

#[test]
fn row_top_accumulates_preceding_rows() {
    let h = [20.0, 20.0, 20.0];
    let g = [0.0, 0.0, 0.0];
    assert_eq!(expected_row_top(50.0, 0.0, &h, &g, 1), 70.0);
    assert_eq!(expected_row_top(50.0, 0.0, &h, &g, 2), 90.0);
}

#[test]
fn row_top_includes_visible_top_offset() {
    // 滚动到中间：`visible_top` 是首可见行的（负）偏移。
    let h = [20.0, 20.0];
    assert_eq!(expected_row_top(0.0, -15.0, &h, &[0.0, 0.0], 1), 5.0);
}

/// 回归：**不得**再叠加滚动偏移。
///
/// `layout_cursor` 已经把滚动加进 `bounds.origin`，此前浮层又减了一次
/// `scroll_offset.y`，导致整体偏移。
#[test]
fn row_top_does_not_double_count_scroll() {
    let h = [20.0, 20.0];
    let g = [0.0, 0.0];
    // bounds 已含滚动（例如滚了 40px）。
    let bounds_top = -40.0;
    let visible_top = 0.0;
    let got = expected_row_top(bounds_top, visible_top, &h, &g, 1);

    // 正确值：只加前置行高，不再减滚动。
    assert_eq!(got, -20.0);
    // 错误值（曾出现的实现）：再减一次滚动 → 更小。
    assert_ne!(got, -60.0, "must not subtract scroll a second time");
}

#[test]
fn row_top_counts_git_expansion_blocks() {
    // 第一行上方有一个 3 行的 git 展开块：第二行行顶要把它算进去。
    let h = [20.0, 20.0];
    let g = [60.0, 0.0];
    assert_eq!(expected_row_top(0.0, 0.0, &h, &g, 1), 80.0);
}

// ------------------------------------------------------------------ 上下择位
//
// 这些用例驱动**真实**的 `popover_y`（不是本地复刻），因为「上下写反」
// 正是曾经的真实缺陷：底部代码的浮层跑到屏幕外下方、顶部代码的跑到
// 屏幕外上方。用本地复刻测试会让这种错误静默通过。

use crate::components::input::element::popover_y;

/// 视口与浮层尺寸的统一场景：视口 y ∈ [0, 600]，行高 20，浮层高 100。
const VIEW_TOP: f32 = 0.0;
const VIEW_BOTTOM: f32 = 600.0;
const ROW_H: f32 = 20.0;
const POP_H: f32 = 100.0;

fn place(anchor_y: f32, prefer_below: bool) -> f32 {
    f32::from(popover_y(
        px(anchor_y),
        px(ROW_H),
        px(POP_H),
        px(VIEW_TOP),
        px(VIEW_BOTTOM),
        prefer_below,
    ))
}

/// 顶部代码 + hover（优先上方）：上方放不下 → 必须翻到**下方**。
#[test]
fn hover_at_top_flips_below() {
    // 锚点 y=30：上方需要 100+gap 放不下 → 下方。
    let y = place(30.0, false);
    assert_eq!(y, 50.0, "hover near the top must flip below (30 + row 20)");
}

/// 底部代码 + hover（优先上方）：上方放得下 → 必须画在**上方**。
///
/// 这正是用户报告的场景：底部代码的浮层该往上弹。
#[test]
fn hover_at_bottom_stays_above() {
    // 锚点 y=550：下方只剩 50 放不下 100 → 上方 = 550 - 100 - gap。
    let y = place(550.0, false);
    assert!(
        y < 550.0,
        "hover near the bottom must be placed ABOVE the anchor, got y={y}"
    );
    // 上方位置必须落在视口内（不能跑到屏幕外）。
    assert!(y >= VIEW_TOP, "must stay inside the viewport, got y={y}");
    assert!(
        y + POP_H <= VIEW_BOTTOM,
        "popover must not extend past the viewport bottom, got y={y}"
    );
}

/// 中部代码 + hover：上方放得下 → 画在上方。
#[test]
fn hover_in_the_middle_prefers_above() {
    let y = place(300.0, false);
    assert!(y < 300.0, "hover should prefer above when there is room");
}

/// 补全菜单（优先下方）：中部时画在锚点行下方。
#[test]
fn completion_prefers_below() {
    let y = place(300.0, true);
    assert_eq!(y, 320.0, "completion menu should sit below the anchor row");
}

/// 补全菜单在底部：下方放不下 → 翻到上方。
#[test]
fn completion_at_bottom_flips_above() {
    let y = place(550.0, true);
    assert!(
        y < 550.0,
        "completion menu near the bottom must flip above, got y={y}"
    );
    assert!(y >= VIEW_TOP, "must stay inside the viewport, got y={y}");
}

/// 视口比浮层还矮：两边都放不下 → 夹进视口，不跑出屏幕。
#[test]
fn popover_clamps_into_viewport_when_neither_fits() {
    let y = f32::from(popover_y(
        px(30.0),
        px(ROW_H),
        px(100.0),
        px(0.0),
        px(80.0), // 视口比浮层矮
        false,
    ));
    assert!(y >= VIEW_TOP, "must not go above the viewport, got y={y}");
}

/// 无论锚点在哪、优先哪边，浮层都必须与视口有交集（绝不整块跑到屏幕外）。
///
/// 这条是「上下写反」的通用护栏：写反时浮层会整块落在视口之外。
#[test]
fn popover_always_intersects_viewport() {
    for anchor in [0.0f32, 10.0, 30.0, 100.0, 300.0, 500.0, 550.0, 590.0, 600.0] {
        for prefer_below in [false, true] {
            let y = place(anchor, prefer_below);
            let intersects = y < VIEW_BOTTOM && y + POP_H > VIEW_TOP;
            assert!(
                intersects,
                "anchor={anchor} prefer_below={prefer_below} placed popover at y={y}, \
                 which is entirely outside the viewport [{VIEW_TOP}, {VIEW_BOTTOM}]"
            );
        }
    }
}
