//! Kitty 图形协议的 Unicode 占位符（U+10EEEE）解码。
//!
//! 规范：<https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders>
//!
//! 思路：客户端先建一个**虚拟放置**（`a=p,U=1`，不带实际位置），然后在网格里
//! 打出 U+10EEEE 字符，用**颜色**和**组合变音符**把"这块该显示图像的哪一片"
//! 编码进字符本身。于是图像能跟着文字一起滚动、换行、被 TUI 重排——这正是
//! 实际放置做不到的。
//!
//! ## 编码位置
//!
//! | 信息 | 载体 |
//! | --- | --- |
//! | 图像 id 低 24 位 | 前景色（调色板索引或 RGB 的 24 位） |
//! | 图像 id 高 8 位 | 第 3 个组合变音符（可选） |
//! | 放置 id | 下划线专用色（SGR 58，可选） |
//! | 图像内的行 | 第 1 个组合变音符 |
//! | 图像内的列 | 第 2 个组合变音符 |
//!
//! 组合变音符按下标查 [`DIACRITICS`] 表得到数值，因此这张表就是编码本。
//!
//! ## 连续段（run）
//!
//! 一行内相邻的占位符若兼容（见 `IncompletePlacement::can_append`）会合并成
//! 一段，段宽即占用的列数。合并规则直接照搬 kitty 的判据——它看起来有点
//! 反直觉（显式写出的列号也允许"续上"），但这就是终端实际的行为，改了会
//! 与真实客户端不一致。

/// 占位符字符（U+10EEEE）。
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// 把一个终端颜色换算成协议里的 24 位 id（图像 id 或放置 id）。
///
/// 移植 ghostty `graphics_unicode.zig::colorToId`：
/// - `Spec`（真彩）→ `(r << 16) | (g << 8) | b`；
/// - `Indexed`（256 色索引）→ 索引本身；
/// - `Named`（16 基础色的语义名）→ 名字的编号。
///
/// 刻意**不**去查调色板/主题：这是协议层的编码，客户端写进去的是什么数值，
/// 这里就必须还原成什么数值。查询默认色或 `None` 时返回 0（图像 id 0 不是
/// 合法图像，自然什么也画不出来）。
pub fn color_to_id(color: &alacritty_terminal::vte::ansi::Color) -> u32 {
    use alacritty_terminal::vte::ansi::Color;
    match color {
        Color::Spec(rgb) => ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
        Color::Indexed(idx) => *idx as u32,
        Color::Named(named) => *named as u32,
    }
}

/// 组合变音符 → 行/列下标。**必须保持升序**（用二分查找）。
///
/// 表本身来自 kitty 的
/// `rowcolumn-diacritics.txt`，下标即编码值。
const DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// 查组合变音符对应的行/列下标。不在表内返回 `None`（调用方按"没写"处理）。
fn diacritic_index(cp: char) -> Option<u32> {
    let cp = cp as u32;
    DIACRITICS.binary_search(&cp).ok().map(|i| i as u32)
}

/// 一个 cell 上的占位符信息。
///
/// "不完整"是因为协议允许省略变音符：省略的行/列可以从前一个占位符续上，
/// 只有在开始新的一段时才需要各自的默认值。因此解码与合并要分两步走。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompletePlacement {
    /// 图像 id 低 24 位（来自前景色），恒存在。
    pub image_id_low: u32,
    /// 图像 id 高 8 位（来自第 3 个变音符），可选。
    pub image_id_high: Option<u8>,
    /// 放置 id（来自下划线色），可选；`0` 视为未指定。
    pub placement_id: Option<u32>,
    /// 图像内的行下标（第 1 个变音符），可选。
    pub row: Option<u32>,
    /// 图像内的列下标（第 2 个变音符），可选。
    pub col: Option<u32>,
    /// 到目前为止这一段占了多少列。
    pub width: u32,
}

impl IncompletePlacement {
    /// 从单元格的样式与组合变音符解码。
    ///
    /// `fg_id` 是前景色换算出的 24 位 id，`underline_id` 是下划线色换算出的
    /// id（`0` 表示未设置），`diacritics` 是该单元格的组合变音符序列。做成
    /// 参数而不是直接吃 alacritty 的 `Cell`，是为了让解码逻辑不依赖网格类型、
    /// 可以纯函数单测。
    pub fn decode(fg_id: u32, underline_id: u32, diacritics: &[char]) -> Self {
        let mut result = Self {
            image_id_low: fg_id & 0xFF_FFFF,
            image_id_high: None,
            placement_id: if underline_id != 0 {
                Some(underline_id & 0xFF_FFFF)
            } else {
                None
            },
            row: None,
            col: None,
            width: 1,
        };

        // 任何非法变音符都当作"没写"——与 kitty 的实际行为一致，不是规范
        // 明文规定的。
        result.row = diacritics.first().and_then(|c| diacritic_index(*c));

        if let Some(second) = diacritics.get(1) {
            result.col = diacritic_index(*second);

            if let Some(third) = diacritics.get(2) {
                // 高 8 位：表里查到的值超出 u8 就当作没写。第三个之后的
                // 变音符一律忽略。
                result.image_id_high = diacritic_index(*third).and_then(|v| u8::try_from(v).ok());
            }
        }

        result
    }

    /// 能否与后面的同一行占位符合并成一段。
    ///
    /// 判据照搬 kitty：图像 id（低 24 位 + 高 8 位）、放置 id 必须一致；行号
    /// 要么没写要么与本段相同；列号要么没写、要么正好接在本段之后。
    fn can_append(&self, other: &Self) -> bool {
        self.image_id_low == other.image_id_low
            && self.placement_id == other.placement_id
            && other.row.is_none_or(|row| Some(row) == self.row)
            && other
                .col
                .is_none_or(|col| self.col.is_some_and(|start| col == start + self.width))
            && other
                .image_id_high
                .is_none_or(|high| Some(high) == self.image_id_high)
    }

    /// 尝试合并后一个占位符。不兼容时返回 `false` 且自身不变。
    pub fn append(&mut self, other: &Self) -> bool {
        if !self.can_append(other) {
            return false;
        }
        self.width += 1;
        true
    }

    /// 收尾成一段完整的虚拟放置。省略的行/列一律补 0。
    pub fn complete(&self, row: i32, col: u32) -> VirtualPlacement {
        let high = u32::from(self.image_id_high.unwrap_or(0));
        VirtualPlacement {
            image_id: self.image_id_low | (high << 24),
            placement_id: self.placement_id.unwrap_or(0),
            row: self.row.unwrap_or(0),
            col: self.col.unwrap_or(0),
            width: self.width,
            grid_row: row,
            grid_col: col,
        }
    }
}

/// 一段合并完成的占位符：指向图像的某一片，落在网格的某个位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualPlacement {
    /// 目标图像 id。
    pub image_id: u32,
    /// 目标放置 id（`0` 表示"该图像第一个虚拟放置"）。
    pub placement_id: u32,
    /// 图像内的起始行下标。
    pub row: u32,
    /// 图像内的起始列下标。
    pub col: u32,
    /// 段宽（占用的列数）。
    pub width: u32,
    /// 段所在网格的绝对行号。
    pub grid_row: i32,
    /// 段起始的列号。
    pub grid_col: u32,
}

/// 把一行里的占位符合并成若干段。
///
/// `cells` 是同一行内**按列升序**的 `(列号, 解码结果)`，其中只包含占位符
/// 单元格。返回值里每段的 `grid_col` 是段首的列号。
pub fn runs_in_row(cells: &[(u32, IncompletePlacement)], grid_row: i32) -> Vec<VirtualPlacement> {
    let mut out = Vec::new();
    let mut run: Option<(u32, IncompletePlacement)> = None;

    for (col, incomplete) in cells {
        // 起一段时省略的行/列补 0：段首是唯一能确定这两个值的地方。
        let fresh = start_run(*col, incomplete);

        let Some((start_col, current)) = run.as_mut() else {
            run = Some(fresh);
            continue;
        };

        // 只在两种情况下续得上：列号正好接在本段之后，且两个占位符兼容。
        // 列号跳号（中间被别的字符隔开）必然要断开成新段。
        if *col == *start_col + current.width && current.append(incomplete) {
            continue;
        }

        out.push(current.complete(grid_row, *start_col));
        run = Some(fresh);
    }

    if let Some((start_col, current)) = run {
        out.push(current.complete(grid_row, start_col));
    }

    out
}

/// 起一段新的连续段：补上省略的行/列默认值。
fn start_run(col: u32, incomplete: &IncompletePlacement) -> (u32, IncompletePlacement) {
    let mut fresh = *incomplete;
    if fresh.row.is_none() {
        fresh.row = Some(0);
    }
    if fresh.col.is_none() {
        fresh.col = Some(0);
    }
    (col, fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表必须有序，否则二分查找会静默失效。
    #[test]
    fn diacritics_table_is_sorted() {
        assert!(
            DIACRITICS.windows(2).all(|w| w[0] < w[1]),
            "DIACRITICS 必须严格升序"
        );
    }

    /// 抽查两个下标，锁住表的取值范围（来自 kitty 的实际行为）。
    #[test]
    fn diacritic_indices_match_kitty() {
        assert_eq!(diacritic_index('\u{0305}'), Some(0));
        assert_eq!(diacritic_index('\u{0483}'), Some(30));
        assert_eq!(diacritic_index('\u{1D242}'), Some(294));
        assert_eq!(
            diacritic_index('\u{1D244}'),
            Some(DIACRITICS.len() as u32 - 1)
        );
    }

    #[test]
    fn unknown_diacritic_is_none() {
        // 'A' 显然不是组合变音符，也不在表里。
        assert_eq!(diacritic_index('A'), None);
        assert_eq!(diacritic_index('\u{0B57}'), None);
    }

    /// 空格 + 行/列变音符：单个 cell 的段，行列都是 0。
    #[test]
    fn single_cell_row_and_col_zero() {
        let p = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{0305}']);
        assert_eq!(p.image_id_low, 0);
        assert_eq!(p.placement_id, None);
        assert_eq!(p.row, Some(0));
        assert_eq!(p.col, Some(0));
        assert_eq!(p.width, 1);

        let done = p.complete(0, 0);
        assert_eq!(done.image_id, 0);
        assert_eq!(done.placement_id, 0);
        assert_eq!(done.row, 0);
        assert_eq!(done.col, 0);
        assert_eq!(done.width, 1);
        assert_eq!(done.grid_row, 0);
        assert_eq!(done.grid_col, 0);
    }

    /// 列号跳号（第 2 个 cell 的列变音符是 2 而非 1）→ 断成两段。
    #[test]
    fn continuation_break_on_column_jump() {
        let first = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{0305}']);
        let second = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{030E}']);
        let runs = runs_in_row(&[(0, first), (1, second)], 0);

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].width, 1);
        assert_eq!(runs[1].col, 2, "第二个 cell 声明自己在图像的第 2 列");
        assert_eq!(runs[1].width, 1);
    }

    /// 显式写出的列号正好接在本段之后 → 仍然合并（kitty 的实际行为）。
    #[test]
    fn continuation_with_explicit_contiguous_columns() {
        let a = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{0305}']);
        let b = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{030D}']);
        let c = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{030E}']);
        let runs = runs_in_row(&[(0, a), (1, b), (2, c)], 0);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].width, 3);
    }

    /// 后续 cell 不写列号 → 从本段续上。
    #[test]
    fn continuation_without_col_diacritic() {
        let a = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{0305}']);
        let b = IncompletePlacement::decode(0, 0, &['\u{0305}']);
        let c = IncompletePlacement::decode(0, 0, &['\u{0305}']);
        let runs = runs_in_row(&[(0, a), (1, b), (2, c)], 0);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].width, 3);
    }

    /// 一个变音符都不写 → 仍然能起一段并续上。
    #[test]
    fn continuation_without_any_diacritics() {
        let p = IncompletePlacement::decode(0, 0, &[]);
        let runs = runs_in_row(&[(0, p), (1, p), (2, p)], 0);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].row, 0);
        assert_eq!(runs[0].width, 3);
    }

    /// 段被非占位符字符终止：只有真正相邻的两个占位符合并。
    #[test]
    fn run_ends_before_non_placeholder() {
        let a = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{0305}']);
        let b = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{030D}']);
        // 'ABC' 占掉列 2..5，第二个占位符落在列 5。
        let runs = runs_in_row(&[(0, a), (1, b)], 0);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].width, 2);
    }

    /// 段从行中间开始：段首列号跟着走。
    #[test]
    fn run_starting_mid_row() {
        let a = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{0305}']);
        let b = IncompletePlacement::decode(0, 0, &['\u{0305}', '\u{030D}']);
        let runs = runs_in_row(&[(3, a), (4, b)], 7);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].grid_col, 3);
        assert_eq!(runs[0].grid_row, 7);
        assert_eq!(runs[0].width, 2);
    }

    /// 图像 id 可以来自调色板索引。
    #[test]
    fn image_id_from_palette() {
        let p = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{0305}']);
        assert_eq!(p.image_id_low, 42);
        assert_eq!(p.complete(0, 0).image_id, 42);
    }

    /// 第 3 个变音符提供图像 id 的高 8 位（0x030E 的索引是 2）。
    #[test]
    fn image_id_high_bits_from_third_diacritic() {
        let p = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{0305}', '\u{030E}']);
        assert_eq!(p.image_id_high, Some(2));
        // 42 | (2 << 24) = 33554474
        assert_eq!(p.complete(0, 0).image_id, 33_554_474);
    }

    /// 放置 id 来自下划线色。
    #[test]
    fn placement_id_from_underline_color() {
        let p = IncompletePlacement::decode(42, 21, &['\u{0305}', '\u{0305}']);
        assert_eq!(p.placement_id, Some(21));
        assert_eq!(p.complete(0, 0).placement_id, 21);
    }

    /// 下划线色为 0 视为没写。
    #[test]
    fn zero_underline_color_means_no_placement_id() {
        let p = IncompletePlacement::decode(42, 0, &[]);
        assert_eq!(p.placement_id, None);
        assert_eq!(p.complete(0, 0).placement_id, 0);
    }

    /// 高 8 位不一致的占位符不能合并。
    #[test]
    fn mismatched_high_bits_break_the_run() {
        let a = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{0305}']);
        let b = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{030D}', '\u{030E}']);
        let runs = runs_in_row(&[(0, a), (1, b)], 0);

        assert_eq!(runs.len(), 2, "高 8 位不同应断段");
    }

    /// 行号不一致的占位符不能合并。
    #[test]
    fn mismatched_row_breaks_the_run() {
        let a = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{0305}']);
        // 第 2 个 cell 声称自己是图像第 1 行。
        let b = IncompletePlacement::decode(42, 0, &['\u{030D}', '\u{030D}']);
        let runs = runs_in_row(&[(0, a), (1, b)], 0);

        assert_eq!(runs.len(), 2, "行号不同应断段");
    }

    /// 图像 id 不同的占位符不能合并。
    #[test]
    fn mismatched_image_id_breaks_the_run() {
        let a = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{0305}']);
        let b = IncompletePlacement::decode(43, 0, &['\u{0305}', '\u{030D}']);
        let runs = runs_in_row(&[(0, a), (1, b)], 0);

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].image_id, 42);
        assert_eq!(runs[1].image_id, 43);
    }

    /// 空输入不产生任何段。
    #[test]
    fn empty_row_has_no_runs() {
        assert!(runs_in_row(&[], 0).is_empty());
    }

    /// 非法变音符按"没写"处理，而不是让整个占位符失效。
    #[test]
    fn invalid_diacritic_treated_as_missing() {
        // 'A' 不在表里 → row 与 col 都是 None → complete 补 0。
        let p = IncompletePlacement::decode(7, 0, &['A']);
        assert_eq!(p.row, None);
        assert_eq!(p.col, None);
        let done = p.complete(0, 0);
        assert_eq!(done.row, 0);
        assert_eq!(done.col, 0);
        assert_eq!(done.image_id, 7);
    }

    /// 第 3 个变音符查到的值超出 u8 → 高 8 位当作没写。
    #[test]
    fn high_diacritic_value_over_u8_is_ignored() {
        // 索引 294（0x1D242）远超 u8 上限。
        let p = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{0305}', '\u{1D242}']);
        assert_eq!(p.image_id_high, None);
        assert_eq!(p.complete(0, 0).image_id, 42);
    }

    /// 第 3 个之后的变音符一律忽略。
    #[test]
    fn extra_diacritics_are_ignored() {
        let p = IncompletePlacement::decode(42, 0, &['\u{0305}', '\u{0305}', '\u{030D}', 'A']);
        assert_eq!(p.image_id_high, Some(1));
        assert_eq!(p.width, 1);
    }

    /// 真彩颜色按 `(r << 16) | (g << 8) | b` 编码。
    #[test]
    fn color_to_id_encodes_truecolor() {
        use alacritty_terminal::vte::ansi::{Color, Rgb};
        let rgb = Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        };
        assert_eq!(color_to_id(&Color::Spec(rgb)), 0x123456);

        // 然后再折回占位符解码：低 24 位就是图像 id。
        let p = IncompletePlacement::decode(color_to_id(&Color::Spec(rgb)), 0, &[]);
        assert_eq!(p.image_id_low, 0x123456);
    }

    /// 256 色索引直接用索引本身。
    #[test]
    fn color_to_id_uses_index_for_indexed() {
        use alacritty_terminal::vte::ansi::Color;
        assert_eq!(color_to_id(&Color::Indexed(42)), 42);
    }

    /// 命名色用名字的编号。
    #[test]
    fn color_to_id_uses_named_index() {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        assert_eq!(
            color_to_id(&Color::Named(NamedColor::Red)),
            NamedColor::Red as u32
        );
    }
}
