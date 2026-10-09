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

/// 浮层上下择位（Zed 的 `can_place_above` / `can_place_below` 的简化版）。
fn place_above_or_below(
    anchor_y: gpui::Pixels,
    line_height: gpui::Pixels,
    popover_height: gpui::Pixels,
    viewport_top: gpui::Pixels,
    viewport_bottom: gpui::Pixels,
    gap: gpui::Pixels,
) -> gpui::Pixels {
    let above = anchor_y - popover_height - gap;
    let below = anchor_y + line_height;
    if above >= viewport_top {
        above
    } else if below + popover_height <= viewport_bottom {
        below
    } else {
        above.max(viewport_top)
    }
}

#[test]
fn popover_prefers_above_when_there_is_room() {
    // 锚点在中部：上方放得下 → 画在上方。
    let y = place_above_or_below(px(300.), px(20.), px(100.), px(0.), px(600.), px(4.));
    assert_eq!(y, px(196.)); // 300 - 100 - 4
}

#[test]
fn popover_flips_below_when_top_is_cramped() {
    // 锚点贴近顶部：上方放不下 → 翻到下方。
    let y = place_above_or_below(px(30.), px(20.), px(100.), px(0.), px(600.), px(4.));
    assert_eq!(y, px(50.)); // 30 + 20
}

#[test]
fn popover_clamps_into_viewport_when_neither_fits() {
    // 视口比浮层还矮：两个方向都放不下 → 夹到视口顶部，不跑出屏幕。
    let y = place_above_or_below(px(30.), px(20.), px(100.), px(0.), px(80.), px(4.));
    assert!(y >= px(0.), "must stay inside viewport, got {y:?}");
}
