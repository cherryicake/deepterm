//! 转义序列的旁路抽取：在字节流进入 vte 之前，把应用层关心的
//! OSC / APC 序列切出来。
//!
//! 为什么需要：上游 `vte` 会把 APC 内容直接丢弃（`SosPmApcString` 状态
//! 对内容 `_ => ()`），且 `alacritty_terminal` 的 `EventListener` 没有
//! 对应事件。因此想拦截 APC（Kitty 图形协议）只能在解析器之前自己切分。
//!
//! 本模块**不含任何状态**——跨读残尾由调用方持有，这样同一套抽取逻辑
//! 可以同时服务本地读线程与远程 UI 线程两条不同的字节管线。

/// OSC 起始符 `ESC ]`。
pub const OSC_START: &[u8] = b"\x1b]";
/// APC 起始符（Kitty 图形协议为 `ESC _ G`）。
pub const APC_START: &[u8] = b"\x1b_";

/// 序列种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceKind {
    Osc,
    /// APC（`ESC _`），负载为 `G` 开头时即 Kitty 图形协议。
    Apc,
}

/// 抽取结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extract<'a> {
    /// 找到一个完整序列。
    Complete {
        /// 序列总长度（含起始符与终止符），用于推进游标。
        len: usize,
        /// 起始符与终止符之间的内容。
        content: &'a [u8],
    },
    /// 序列尚未结束（跨读被截断），调用方应缓存残尾。
    Incomplete,
    /// 起始符不匹配。
    NotStart,
}

/// 从 `data` 开头抽取一个 OSC 序列。
///
/// 终止符为 BEL (`0x07`) 或 ST (`ESC \`)。两者都支持是因为 OSC 历史上
/// 允许 BEL 作为简写。
pub fn extract_osc(data: &[u8]) -> Extract<'_> {
    extract(data, OSC_START, true)
}

/// 从 `data` 开头抽取一个 APC 序列。
///
/// APC 只能由 ST (`ESC \`) 终止——BEL 在 APC 中不是终止符，因此这里
/// 不复用 OSC 的宽松规则。
pub fn extract_apc(data: &[u8]) -> Extract<'_> {
    extract(data, APC_START, false)
}

fn extract<'a>(data: &'a [u8], start: &[u8], allow_bel: bool) -> Extract<'a> {
    if !data.starts_with(start) {
        return Extract::NotStart;
    }

    let body_start = start.len();
    let mut i = body_start;
    while i < data.len() {
        if allow_bel && data[i] == 0x07 {
            return Extract::Complete {
                len: i + 1,
                content: &data[body_start..i],
            };
        }
        // ST = ESC \
        if data[i] == 0x1b {
            if i + 1 >= data.len() {
                // `ESC` 是最后一个字节：终止符跨读被截断。
                return Extract::Incomplete;
            }
            if data[i + 1] == b'\\' {
                return Extract::Complete {
                    len: i + 2,
                    content: &data[body_start..i],
                };
            }
        }
        i += 1;
    }

    Extract::Incomplete
}

/// 判断一个 APC 内容是否是 Kitty 图形协议（`G` 开头）。
pub fn is_kitty_graphics(content: &[u8]) -> bool {
    content.first() == Some(&b'G')
}

/// 去掉 Kitty 图形协议的前导 `G`。
pub fn kitty_graphics_payload(content: &[u8]) -> Option<&[u8]> {
    content.strip_prefix(b"G")
}

/// 终端几何：行列数 + 单元格像素尺寸。
///
/// 与 ghostty `terminal/size_report.zig` 的 `Size` 同构，是尺寸上报
/// （XTWINOPS / in-band size reports）所需的全部量化值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermGeometry {
    pub rows: u16,
    pub cols: u16,
    /// 单个单元格宽度（像素）。
    pub cell_width: u16,
    /// 单个单元格高度（像素）。
    pub cell_height: u16,
}

impl TermGeometry {
    /// 文本区宽度（像素）。
    pub fn width_pixels(&self) -> u32 {
        self.cols as u32 * self.cell_width as u32
    }

    /// 文本区高度（像素）。
    pub fn height_pixels(&self) -> u32 {
        self.rows as u32 * self.cell_height as u32
    }
}

/// 尺寸上报的风格，对齐 ghostty `size_report.Style`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeReport {
    /// XTWINOPS `CSI 16 t`：单元格像素尺寸。
    CellSize,
    /// in-band size report（DECSET 2048）：`CSI 48 ; r ; c ; h ; w t`。
    Mode2048,
}

/// 渲染一条尺寸上报应答。
///
/// 格式逐字节对齐 ghostty `terminal/size_report.zig` 的 `encode()`：
/// - `CellSize` → `\x1b[6;{cell_height};{cell_width}t`
/// - `Mode2048` → `\x1b[48;{rows};{cols};{height_px};{width_px}t`
///
/// 注意 `CellSize` 是**高在前、宽在后**（ghostty `.{ size.cell_height,
/// size.cell_width }`）——这是客户端据此推算宽高比的关键，顺序反了会把画面
/// 压扁。
pub fn encode_size_report(style: SizeReport, geometry: TermGeometry) -> String {
    match style {
        SizeReport::CellSize => format!("\x1b[6;{};{}t", geometry.cell_height, geometry.cell_width),
        SizeReport::Mode2048 => format!(
            "\x1b[48;{};{};{};{}t",
            geometry.rows,
            geometry.cols,
            geometry.height_pixels(),
            geometry.width_pixels()
        ),
    }
}

/// CSI 起始符 `ESC [`。
pub const CSI_START: &[u8] = b"\x1b[";

/// 在 `data` 中查找裸形态的 `CSI 16 t`（cell 像素尺寸查询）。
///
/// 返回 `(起始偏移, 总长度)`。只受理**不带参数与中间字节**的 `ESC [ 16 t`
/// ——`ESC[16;...t` 之类不在此列（ghostty 也只认裸形态）。
///
/// 为什么需要它：`vte-0.15` 的 CSI `t` 分派只覆盖 `14/18/22/23`，`16` 落到
/// `unhandled!()`，因此必须在这一层自己识别。识别后序列由调用方摘除，
/// 不进 vte。
pub fn find_csi_16_t(data: &[u8]) -> Option<(usize, usize)> {
    /// `ESC [ 1 6 t`
    const NEEDLE: &[u8] = b"\x1b[16t";

    memchr::memmem::find(data, NEEDLE).map(|offset| (offset, NEEDLE.len()))
}

/// 在 `data` 中查找 DECSET/DECRST 形式的私有模式 `?{mode}`。
///
/// 参数列表按 `;` 切分后逐项比对，因此 `ESC[?2048;2004h` 这类多参数合写
/// 也能识别。
///
/// 返回值第三项是「该序列能否整条摘除」：
/// - **单参数**（`?2048h`）：可以整条摘除。这个模式号在 vte/alacritty 里是
///   未知模式，喂进去只会产生 "Ignoring unknown mode" 日志噪音。
/// - **多参数**（`?2048;2004h`）：**不能摘**。同一序列里还带着 vte 认识且
///   必须生效的模式（`2004` bracketed paste、`1004` 焦点上报、`25` 光标
///   可见性……）。整条摘掉会让它们静默失效——粘贴保护消失、焦点上报失灵。
///   此时调用方应让原序列照常喂给 vte：vte 会正确处理它认识的那些参数、
///   忽略不认识的 2048/1016，行为与 ghostty 的逐参数分派
///   （`stream_terminal.zig:411-412`）一致。
///
/// 需要自己识别的原因：这些模式号在 `vte`/`alacritty` 的枚举里不存在，
/// 会被归为 `PrivateMode::Unknown`，既不会真正置位也不会被 `DECRQM` 正确
/// 应答。ghostty 自己实现了它们（`terminal/modes.zig`），这里照做。
pub fn find_private_mode(data: &[u8], mode: &[u8]) -> Option<PrivateModeHit> {
    let mut search_from = 0;
    while let Some(offset) = memchr::memmem::find(&data[search_from..], CSI_START) {
        let start = search_from + offset;
        let body_start = start + CSI_START.len();

        // 私有模式必须紧跟 `?`。
        if data.get(body_start) != Some(&b'?') {
            search_from = body_start;
            continue;
        }

        // 扫描参数字节（数字与分号），直到最终字节 `h`/`l`。
        let mut i = body_start + 1;
        let mut enabled = None;
        while i < data.len() {
            match data[i] {
                b'0'..=b'9' | b';' => i += 1,
                b'h' => {
                    enabled = Some(true);
                    break;
                }
                b'l' => {
                    enabled = Some(false);
                    break;
                }
                // 其它中间字节/最终字节：不是我们要找的形态，从 `?` 之后继续找。
                _ => break,
            }
        }

        let Some(enabled) = enabled else {
            search_from = body_start;
            continue;
        };

        let params = &data[body_start + 1..i];
        if !params.split(|b| *b == b';').any(|param| param == mode) {
            search_from = i + 1;
            continue;
        }

        // 参数个数决定能否整条摘除（见函数文档）。
        let single = !params.contains(&b';');
        return Some(PrivateModeHit {
            start,
            len: i + 1 - start,
            enabled,
            // 多参数时保留原文（`None` 表示不摘除）。
            removable: single.then_some((start, i + 1 - start)),
        });
    }

    None
}

/// [`find_private_mode`] 的命中结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateModeHit {
    /// 整条序列在 `data` 中的起始偏移。
    pub start: usize,
    /// 整条序列的长度。
    pub len: usize,
    /// 真 = `h`（置位），假 = `l`（复位）。
    pub enabled: bool,
    /// 可整条摘除时给出 `(起始偏移, 长度)`；多参数时为 `None`（保留原文）。
    pub removable: Option<(usize, usize)>,
}

impl PrivateModeHit {
    /// 该序列是否可从字节流中整条摘除。
    pub fn is_removable(&self) -> bool {
        self.removable.is_some()
    }
}

/// in-band size reports 的模式号（DECSET/DECRST `?2048`）。
pub fn find_private_mode_2048(data: &[u8]) -> Option<PrivateModeHit> {
    find_private_mode(data, b"2048")
}

/// SGR-Pixels 鼠标编码的模式号（DECSET/DECRST `?1016`）。
///
/// 对齐 ghostty `terminal/modes.zig:318`：
/// `.{ .name = "mouse_format_sgr_pixels", .value = 1016 }`。
pub fn find_private_mode_1016(data: &[u8]) -> Option<PrivateModeHit> {
    find_private_mode(data, b"1016")
}

/// 在 `data` 中查找 DECRQM 形式的 `CSI ? {mode} $ p`（私有模式状态查询）。
///
/// 返回 `(起始偏移, 总长度, mode)`。
///
/// vte 会把未知模式交给 `alacritty` 的 `report_private_mode`，后者对
/// `PrivateMode::Unknown(_)` 一律答 `NotSupported = 0`（`term/mod.rs:2087`）。
/// ghostty 对已识别的模式答 `1`（复位）/`2`（置位），因此 1016 这类
/// 我们自己维护状态的模式必须在进 vte 前拦截并截获查询。
pub fn find_dec_rqm_private(data: &[u8], mode: &[u8]) -> Option<(usize, usize)> {
    /// `$p`：DECRQM 的中间字节 + 最终字节。
    const SUFFIX: &[u8] = b"$p";

    let mut search_from = 0;
    while let Some(offset) = memchr::memmem::find(&data[search_from..], CSI_START) {
        let start = search_from + offset;
        let body_start = start + CSI_START.len();

        // 私有模式查询必须紧跟 `?`。
        if data.get(body_start) != Some(&b'?') {
            search_from = body_start;
            continue;
        }

        // `?` 之后到 `$` 之前应当是纯数字（模式号）。允许 `;` 以便识别
        // `?1016;1$p` 这类形态，但只取第一项做匹配——DECRQM 规范上一次
        // 只查一个模式，多参数是客户端的扩展写法。
        let mut i = body_start + 1;
        while i < data.len() && matches!(data[i], b'0'..=b'9' | b';') {
            i += 1;
        }

        if data[i..].starts_with(SUFFIX) {
            let params = &data[body_start + 1..i];
            if params == mode {
                return Some((start, i + SUFFIX.len() - start));
            }
            search_from = i + SUFFIX.len();
            continue;
        }

        search_from = body_start;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_osc_terminated_by_bel() {
        let data = b"\x1b]0;title\x07rest";
        match extract_osc(data) {
            Extract::Complete { len, content } => {
                assert_eq!(content, b"0;title");
                assert_eq!(len, 10);
                assert_eq!(&data[len..], b"rest");
            }
            other => panic!("期望 Complete，得到 {other:?}"),
        }
    }

    #[test]
    fn extracts_osc_terminated_by_st() {
        let data = b"\x1b]7;file://host/tmp\x1b\\tail";
        match extract_osc(data) {
            Extract::Complete { len, content } => {
                assert_eq!(content, b"7;file://host/tmp");
                assert_eq!(&data[len..], b"tail");
            }
            other => panic!("期望 Complete，得到 {other:?}"),
        }
    }

    #[test]
    fn extracts_apc_with_st_only() {
        let data = b"\x1b_Gi=1,a=q;\x1b\\after";
        match extract_apc(data) {
            Extract::Complete { len, content } => {
                assert_eq!(content, b"Gi=1,a=q;");
                assert_eq!(&data[len..], b"after");
            }
            other => panic!("期望 Complete，得到 {other:?}"),
        }
    }

    #[test]
    fn apc_ignores_bel_as_terminator() {
        // BEL 在 APC 中不是终止符：序列应视为未完成（因为无 ST）。
        let data = b"\x1b_Gi=1\x07";
        assert_eq!(extract_apc(data), Extract::Incomplete);
    }

    #[test]
    fn incomplete_osc_is_reported() {
        assert_eq!(extract_osc(b"\x1b]0;unterminated"), Extract::Incomplete);
    }

    #[test]
    fn incomplete_when_st_split_across_reads() {
        // 终止符的 ESC 落在末尾：应报告未完成，等下一块补 `\`。
        let data = b"\x1b_Gi=1\x1b";
        assert_eq!(extract_apc(data), Extract::Incomplete);
        // 拼接后在完整数据上应成功。
        let full = b"\x1b_Gi=1\x1b\\";
        assert!(matches!(extract_apc(full), Extract::Complete { .. }));
    }

    #[test]
    fn wrong_start_is_reported() {
        assert_eq!(extract_apc(b"hello"), Extract::NotStart);
        assert_eq!(extract_osc(b"\x1b_Gx"), Extract::NotStart);
    }

    #[test]
    fn empty_content_is_valid() {
        match extract_apc(b"\x1b_\x1b\\") {
            Extract::Complete { len, content } => {
                assert!(content.is_empty());
                assert_eq!(len, 4);
            }
            other => panic!("期望 Complete，得到 {other:?}"),
        }
    }

    #[test]
    fn identifies_kitty_graphics() {
        assert!(is_kitty_graphics(b"Gi=1"));
        assert!(!is_kitty_graphics(b"i=1"));
        assert!(!is_kitty_graphics(b""));
        assert_eq!(kitty_graphics_payload(b"Gi=1"), Some(&b"i=1"[..]));
        assert_eq!(kitty_graphics_payload(b"i=1"), None);
    }

    #[test]
    fn chunked_apc_reassembles_across_reads() {
        // 模拟 Kitty 大图分片：一条 APC 被读边界切成两半。
        let head = b"\x1b_Gi=1,f=32,m=1;AAAA";
        let tail = b"\x1b\\";
        assert_eq!(extract_apc(head), Extract::Incomplete);

        let mut joined = head.to_vec();
        joined.extend_from_slice(tail);
        match extract_apc(&joined) {
            Extract::Complete { content, .. } => {
                assert_eq!(content, b"Gi=1,f=32,m=1;AAAA");
            }
            other => panic!("期望 Complete，得到 {other:?}"),
        }
    }

    // ===== 尺寸上报 =====
    //
    // 用 ghostty 自带测试（`size_report.zig`）的同一组数字锚定契约：
    // 单元格 9x18、24 行、80 列。

    fn geometry_fixture() -> TermGeometry {
        TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        }
    }

    #[test]
    fn csi_16_field_order_is_height_then_width() {
        assert_eq!(
            encode_size_report(SizeReport::CellSize, geometry_fixture()),
            "\x1b[6;18;9t"
        );
        // 负向断言：宽在前的错误形态绝不允许出现——这正是画面被压扁的成因。
        assert!(!encode_size_report(SizeReport::CellSize, geometry_fixture()).contains("6;9;18"));
    }

    #[test]
    fn encode_mode_2048_matches_ghostty() {
        assert_eq!(
            encode_size_report(SizeReport::Mode2048, geometry_fixture()),
            "\x1b[48;24;80;432;720t"
        );
    }

    #[test]
    fn pixel_extents_multiply_cell_size() {
        let g = geometry_fixture();
        assert_eq!(g.width_pixels(), 720);
        assert_eq!(g.height_pixels(), 432);
    }

    #[test]
    fn finds_bare_csi_16_t() {
        assert_eq!(find_csi_16_t(b"abc\x1b[16tdef"), Some((3, 5)));
        assert_eq!(find_csi_16_t(b"\x1b[16t"), Some((0, 5)));
    }

    #[test]
    fn ignores_csi_16_with_intermediates_or_params() {
        assert_eq!(find_csi_16_t(b"\x1b[16 t"), None);
        assert_eq!(find_csi_16_t(b"\x1b[16;1t"), None);
    }

    #[test]
    fn ignores_other_csi_t() {
        assert_eq!(find_csi_16_t(b"\x1b[14t"), None);
        assert_eq!(find_csi_16_t(b"\x1b[18t"), None);
        assert_eq!(find_csi_16_t(b"no escapes here"), None);
    }

    /// 单参数私有模式：可整条摘除。
    fn assert_single(raw: &[u8], start: usize, len: usize, enabled: bool) {
        let hit = find_private_mode_2048(raw).unwrap_or_else(|| panic!("应命中 2048：{raw:?}"));
        assert_eq!(hit.start, start, "start for {raw:?}");
        assert_eq!(hit.len, len, "len for {raw:?}");
        assert_eq!(hit.enabled, enabled, "enabled for {raw:?}");
        assert_eq!(
            hit.removable,
            Some((start, len)),
            "单参数序列应可整条摘除：{raw:?}"
        );
    }

    /// 多参数私有模式：命中但**不可摘除**（否则同序列里其它模式会静默失效）。
    fn assert_multi(raw: &[u8], start: usize, len: usize, enabled: bool) {
        let hit = find_private_mode_2048(raw).unwrap_or_else(|| panic!("应命中 2048：{raw:?}"));
        assert_eq!(hit.start, start, "start for {raw:?}");
        assert_eq!(hit.len, len, "len for {raw:?}");
        assert_eq!(hit.enabled, enabled, "enabled for {raw:?}");
        assert_eq!(
            hit.removable, None,
            "多参数序列**不得**整条摘除（会吞掉 2004 等模式）：{raw:?}"
        );
    }

    #[test]
    fn finds_mode_2048_set() {
        assert_single(b"\x1b[?2048h", 0, 8, true);
    }

    /// 多参数时必须命中但保留原文——这是「粘贴保护消失」的回归测试。
    ///
    /// `ESC[?2004;2048h` 里的 `2004`（bracketed paste）由 vte 处理，因此
    /// 整条序列必须照常喂下去；2048 只是被我们**额外**识别出来。
    #[test]
    fn finds_mode_2048_in_multi_param_list_without_stripping() {
        assert_multi(b"\x1b[?2048;2004h", 0, 13, true);
        assert_multi(b"\x1b[?2004;2048h", 0, 13, true);
        // 三个参数同样不可摘。
        assert_multi(b"\x1b[?1004;2048;2004h", 0, 18, true);
    }

    #[test]
    fn finds_mode_2048_reset() {
        assert_single(b"\x1b[?2048l", 0, 8, false);
    }

    #[test]
    fn finds_mode_2048_after_preceding_bytes() {
        assert_single(b"hello\x1b[?2048h", 5, 8, true);
    }

    #[test]
    fn ignores_unrelated_private_mode() {
        assert_eq!(find_private_mode_2048(b"\x1b[?2004h"), None);
        assert_eq!(find_private_mode_2048(b"\x1b[?2049h"), None);
        // 非私有（缺 `?`）不算。
        assert_eq!(find_private_mode_2048(b"\x1b[2048h"), None);
        // 截断：没有最终字节。
        assert_eq!(find_private_mode_2048(b"\x1b[?2048"), None);
    }

    #[test]
    fn mode_2048_scan_continues_past_non_matches() {
        // 前面有不相关的私有模式，后面才是 2048。
        assert_single(b"\x1b[?2004h\x1b[?2048h", 8, 8, true);
    }

    // ===== 模式 1016（SGR-Pixels 鼠标）=====

    #[test]
    fn finds_mode_1016_set_and_reset() {
        // `ESC [ ? 1 0 1 6 h` = 8 字节。
        let set = find_private_mode_1016(b"\x1b[?1016h").expect("应命中 1016");
        assert_eq!((set.start, set.len, set.enabled), (0, 8, true));
        assert_eq!(set.removable, Some((0, 8)));

        let reset = find_private_mode_1016(b"\x1b[?1016l").expect("应命中 1016");
        assert_eq!((reset.start, reset.len, reset.enabled), (0, 8, false));
        assert_eq!(reset.removable, Some((0, 8)));
    }

    /// 1016 与其它模式合写时同样不可整条摘除。
    #[test]
    fn mode_1016_in_multi_param_list_is_not_stripped() {
        let hit = find_private_mode_1016(b"\x1b[?1000;1016h").expect("应命中 1016");
        assert!(hit.enabled);
        assert_eq!(hit.removable, None);
    }

    #[test]
    fn mode_1016_does_not_match_1015_or_10160() {
        assert_eq!(find_private_mode_1016(b"\x1b[?1015h"), None);
        assert_eq!(find_private_mode_1016(b"\x1b[?10160h"), None);
    }

    #[test]
    fn modes_2048_and_1016_are_independent() {
        // 关键回归：泛化 finder 后两者不得互相误匹配。
        assert_eq!(find_private_mode_2048(b"\x1b[?1016h"), None);
        assert_eq!(find_private_mode_1016(b"\x1b[?2048h"), None);
    }

    #[test]
    fn finds_dec_rqm_private_1016() {
        assert_eq!(find_dec_rqm_private(b"\x1b[?1016$p", b"1016"), Some((0, 9)));
        assert_eq!(
            find_dec_rqm_private(b"ab\x1b[?1016$p", b"1016"),
            Some((2, 9))
        );
    }

    #[test]
    fn dec_rqm_private_rejects_other_modes_and_shapes() {
        assert_eq!(find_dec_rqm_private(b"\x1b[?2048$p", b"1016"), None);
        // DECSET 不是查询。
        assert_eq!(find_dec_rqm_private(b"\x1b[?1016h", b"1016"), None);
        // 公有的 DECRQM（缺 `?`）不算。
        assert_eq!(find_dec_rqm_private(b"\x1b[1016$p", b"1016"), None);
        // 截断：没有 `$p`。
        assert_eq!(find_dec_rqm_private(b"\x1b[?1016", b"1016"), None);
    }

    #[test]
    fn dec_rqm_private_scan_continues_past_non_matches() {
        assert_eq!(
            find_dec_rqm_private(b"\x1b[?9999$p\x1b[?1016$p", b"1016"),
            Some((9, 9))
        );
    }
}
