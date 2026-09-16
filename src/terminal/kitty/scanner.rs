//! 把字节流中的 Kitty 图形 APC 序列接到图像协议执行器上。
//!
//! 运行在**读线程**（本地）或 UI 线程（远程），因此不依赖 GPUI 的
//! `Context`：需要回送给 PTY 的响应通过 [`ReplySink`] 转达。
//!
//! 两类"分片"是正交的，必须分别处理：
//! 1. **读边界截断**：一条 APC 序列被 read 切成两半 → 由 `pending` 残尾缓冲处理；
//! 2. **协议分片**（`m=1`/`m=0`）：多条独立完整的 APC 组成一张图 →
//!    由 `ImageStore` 内部的 `LoadingImage` 累积状态处理。

use crate::terminal::escape_scan::{self, Extract};
use crate::terminal::kitty::command::Parser;
use crate::terminal::kitty::exec::ImageStore;
use crate::terminal::local_pty::PtyScanner;

/// 回写响应的通道。
///
/// 读线程不能直接写 PTY（那会与 `LocalSender` 的写入路径竞争），
/// 因此把响应交给实现方，由它决定投递方式。
pub trait ReplySink: Send {
    /// 投递需要回写给 PTY 的字节。
    fn reply(&mut self, bytes: &[u8]);
    /// 图像集合发生变化，请求重绘。
    fn images_changed(&mut self);

    /// 取走当前已排队、尚未回写的应答字节（供读线程直接写 PTY）。
    ///
    /// 默认返回空：只关心「攒起来」而不需要「立刻取走」的实现（测试用的
    /// `CollectingSink`）无需关心这个途径。
    fn take_pending(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

/// 把响应攒在内存里的实现（测试与无 PTY 场景用）。
#[derive(Debug, Default)]
pub struct CollectingSink {
    pub replies: Vec<u8>,
    pub changes: usize,
}

impl ReplySink for CollectingSink {
    fn reply(&mut self, bytes: &[u8]) {
        self.replies.extend_from_slice(bytes);
    }

    fn images_changed(&mut self) {
        self.changes += 1;
    }

    fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }
}

/// 未完成 APC 残尾的上限。
///
/// 与单条命令的负载上限（[`MAX_COMMAND_BYTES`]）一致：残尾本质是"一条尚未
/// 到齐的命令"，上限也应相同——terminal-browser 的大帧分片单条可达数 MB，
/// 64 KiB 会把合法序列当"超大"放弃（残尾被透传给 vte，图像丢失）。超限则
/// 放弃该序列（防止无终止符的 `ESC _` 无限占用内存），与 OSC 残尾的处理
/// 策略一致。
const MAX_PENDING_APC: usize = crate::terminal::kitty::command::MAX_COMMAND_BYTES;

/// Kitty 图形协议的扫描器。
pub struct KittyGraphicsScanner {
    store: ImageStore,
    sink: Box<dyn ReplySink>,
    /// 测试/内省用：最近一次光标位置。
    cursor: (i32, u32),
    /// 当前终端几何（尺寸上报用）。
    ///
    /// `None` = 尚不可知，此时对尺寸查询**保持沉默**：回一个 `6;0;0t` 比不答
    /// 更糟糕（客户端会据此算出退化/非法的宽高比）。对齐 ghostty——几何缺失时
    /// `reportMode2048` 与 `resize` 都提前返回。
    geometry: Option<escape_scan::TermGeometry>,
    /// ghostty `in_band_size_reports`（DECSET 2048）是否启用。
    ///
    /// alacritty 把 2048 视为未知模式直接丢弃（`Ignoring unknown mode 2048`），
    /// 因此无法从 `Term` 读回，只能由本扫描器在字节层自己观察。
    mode_2048: bool,
    /// ghostty `mouse_format_sgr_pixels`（DECSET 1016）是否启用。
    ///
    /// 与 `mode_2048` 同理：1016 不在 vte/alacritty 的模式枚举里
    /// （`PrivateMode::Unknown`），无法从 `Term` 读回。
    ///
    /// 与 2048 不同的是，这个状态**必须被 UI 线程读到**——鼠标报文是在
    /// UI 线程按当前格式编码的。扫描器活在读线程里，因此用共享原子量把
    /// 状态暴露出去（`Terminal::sgr_pixels_shared`），与
    /// `kitty_images_dirty` 是同一套跨线程做法。
    mode_1016: bool,
    /// 对外可见的 1016 状态（读线程写、UI 线程读）。
    mode_1016_shared: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl KittyGraphicsScanner {
    /// 新建扫描器。`limit_bytes` 为 0 表示禁用协议（但序列仍被消费，
    /// 避免 APC 内容泄漏到屏幕上）。
    pub fn new(limit_bytes: usize, sink: Box<dyn ReplySink>) -> Self {
        Self::with_storage(
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::terminal::kitty::storage::Storage::new(limit_bytes),
            )),
            limit_bytes > 0,
            sink,
        )
    }

    /// 在既有共享存储上新建扫描器。
    ///
    /// `enabled` 必须显式传入（`Storage` 已存在，无法从其推导协议是否启用）：
    /// 渲染层需要在自己的执行器被禁用时仍能读到图像。
    pub fn with_storage(
        storage: crate::terminal::kitty::exec::SharedStorage,
        enabled: bool,
        sink: Box<dyn ReplySink>,
    ) -> Self {
        Self::build(
            storage,
            enabled,
            sink,
            None,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    /// 在既有共享存储上新建扫描器，并带上初始几何。
    ///
    /// 用创建 PTY 时那份 bounds 传入即可：终端应用可能在**任何 resize 到达
    /// 之前**就发出尺寸查询（terminal-browser 启动时即如此），此时若几何为
    /// `None` 就只能沉默，客户端拿不到尺寸。
    pub fn with_storage_and_geometry(
        storage: crate::terminal::kitty::exec::SharedStorage,
        enabled: bool,
        sink: Box<dyn ReplySink>,
        geometry: escape_scan::TermGeometry,
    ) -> Self {
        Self::build(
            storage,
            enabled,
            sink,
            Some(geometry),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    fn build(
        storage: crate::terminal::kitty::exec::SharedStorage,
        enabled: bool,
        sink: Box<dyn ReplySink>,
        geometry: Option<escape_scan::TermGeometry>,
        mode_1016_shared: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let mut store = ImageStore::from_shared(storage);
        store.set_enabled(enabled);
        Self {
            store,
            sink,
            cursor: (0, 0),
            geometry,
            mode_2048: false,
            mode_1016: false,
            mode_1016_shared,
        }
    }

    /// 接管外部的 1016 状态句柄，使 UI 线程能读到读线程观察到的模式状态。
    ///
    /// 本地终端的字节由读线程消费、鼠标报文却在 UI 线程编码，两边必须共享
    /// 同一份 1016 开关状态；远程终端没有读线程，直接用
    /// `Terminal::remote_mode_1016`，不会调用这里。
    pub fn with_mode_1016_shared(
        mut self,
        shared: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.mode_1016_shared = shared;
        self
    }

    /// 只读访问存储（渲染层/测试用）。
    pub fn with_store<R>(&self, f: impl FnOnce(&ImageStore) -> R) -> R {
        f(&self.store)
    }

    /// 当前光标位置。
    pub fn cursor(&self) -> (i32, u32) {
        self.cursor
    }

    /// 设置光标位置（`a=T`/`a=p`/`d=c` 的锚点）。
    ///
    /// 由调用方在解析字节前同步——读线程无法在扫描时安全读取 `Term`
    /// （会与正在进行的解析争锁）。
    pub fn set_cursor(&mut self, row: i32, col: u32) {
        self.cursor = (row, col);
    }

    /// 告知扫描器当前可见的网格行窗口（`d=a`/`d=A` 的"屏幕上可见"判定）。
    ///
    /// 与光标一样由调用方在解析前同步：窗口随滚动变化，扫描器读不到 `Term`
    /// 的滚动位置，只能每次送字节时一并告知。
    pub fn set_visible_rows(&mut self, rows: super::storage::VisibleRows) {
        self.store.set_visible_rows(Some(rows));
    }

    /// 处理一条**完整**的 APC 内容（远程路径：UI 线程已在
    /// `process_pty_data` 里完成序列切分，无需再扫描字节流）。
    pub fn scan_apc(&mut self, content: &[u8]) {
        self.handle_apc(content);
    }

    /// 更新终端几何。
    ///
    /// 若 in-band size reports（DECSET 2048）已启用，**同时重新上报一次**
    /// ——对齐 ghostty `stream_terminal.zig` 的 `resize()`：模式启用期间每次
    /// 尺寸变化都要主动告知客户端，否则它只能靠 `CSI 14 t` 轮询。
    pub fn set_geometry(&mut self, geometry: escape_scan::TermGeometry) {
        self.geometry = Some(geometry);
        // 同步给存储：删除类命令要把放置的像素尺寸换算成网格矩形，需要单元格
        // 尺寸与终端列数（ghostty 同样现取 `t.width_px / t.cols`）。
        self.store.set_geometry(
            super::storage::CellGeometry {
                cell_width: geometry.cell_width as u32,
                cell_height: geometry.cell_height as u32,
            },
            geometry.cols as u32,
        );
        if self.mode_2048 {
            let report =
                escape_scan::encode_size_report(escape_scan::SizeReport::Mode2048, geometry);
            self.sink.reply(report.as_bytes());
        }
    }

    /// 处理字节流里的尺寸上报类序列。
    ///
    /// 返回需要从字节流中**摘除**的区间（`(起始偏移, 长度)`，按偏移升序、
    /// 互不重叠）。调用方负责把其余片段按序 `emit` 给 vte。
    ///
    /// 涵盖两类序列（都由本层实现，因为 vte 完全不支持）：
    /// - `CSI 16 t`：cell 像素尺寸查询 → 以 `\x1b[6;{cell_height};{cell_width}t` 应答。
    /// - `DECSET/DECRST ?2048`：in-band size reports 开关 → 置位时立即上报几何。
    ///
    /// 两者都必须**摘除**：`CSI 16 t` 在 vte 里会落到 `unhandled!()`（日志噪音），
    /// 而 2048 在 alacritty 里是未知模式。ghostty 自己实现了这两条分支，因此
    /// 摘除正是对齐 ghostty 的行为。
    fn handle_size_sequences(&mut self, data: &[u8]) -> Vec<(usize, usize)> {
        let mut removals = Vec::new();

        // `CSI 16 t`：可能有多条，逐条摘除并应答。
        let mut search_from = 0;
        while let Some((offset, len)) = escape_scan::find_csi_16_t(&data[search_from..]) {
            let start = search_from + offset;
            if let Some(geometry) = self.geometry {
                let report =
                    escape_scan::encode_size_report(escape_scan::SizeReport::CellSize, geometry);
                self.sink.reply(report.as_bytes());
            }
            // 几何未知时静默（见 `geometry` 字段的注释）。
            removals.push((start, len));
            search_from = start + len;
        }

        // `DECSET/DECRST ?2048`。
        let mut search_from = 0;
        while let Some(hit) = escape_scan::find_private_mode_2048(&data[search_from..]) {
            self.mode_2048 = hit.enabled;
            // 对齐 ghostty：每次置位都重发（即使已处于置位状态）——幂等，
            // 且省掉「状态是否真变化」的分支。复位时静默。
            if hit.enabled
                && let Some(geometry) = self.geometry
            {
                let report =
                    escape_scan::encode_size_report(escape_scan::SizeReport::Mode2048, geometry);
                self.sink.reply(report.as_bytes());
            }
            // 仅单参数序列可整条摘除；多参数时保留原文，让 vte 处理同一
            // 序列里其它必须生效的模式（如 bracketed paste `2004`）。
            if let Some(r) = hit.removable {
                removals.push(r);
            }
            search_from = search_from + hit.start + hit.len;
        }

        // `DECSET/DECRST ?1016`：SGR-Pixels 鼠标编码（ghostty
        // `modes.zig:318` 的 `mouse_format_sgr_pixels`）。
        //
        // 必须摘除：1016 在 vte/alacritty 里是未知模式，不摘除则既不生效、
        // 后续 `DECRQM ?1016$p` 也会被答成 0（不识别），客户端据此判定像素
        // 级鼠标不可用而回退到单元格命中测试。模式本身不产生应答。
        let mut search_from = 0;
        while let Some(hit) = escape_scan::find_private_mode_1016(&data[search_from..]) {
            self.mode_1016 = hit.enabled;
            self.mode_1016_shared
                .store(hit.enabled, std::sync::atomic::Ordering::Relaxed);
            if let Some(r) = hit.removable {
                removals.push(r);
            }
            search_from = search_from + hit.start + hit.len;
        }

        // `DECRQM ?1016$p`：按 ghostty 回 `1`（复位）/`2`（置位）。
        let mut search_from = 0;
        while let Some((offset, len)) =
            escape_scan::find_dec_rqm_private(&data[search_from..], b"1016")
        {
            let start = search_from + offset;
            let state = if self.mode_1016 { 2 } else { 1 };
            self.sink.reply(format!("\x1b[?1016;{state}$y").as_bytes());
            removals.push((start, len));
            search_from = start + len;
        }

        removals.sort_unstable();
        removals
    }

    /// 把 `data` 中位于 `removals` 之外的片段按序 `emit` 出去。
    fn emit_without_removals(
        data: &[u8],
        removals: &[(usize, usize)],
        emit: &mut dyn FnMut(&[u8]),
        from: usize,
        to: usize,
    ) {
        let mut pos = from;
        for &(start, len) in removals {
            if start >= to || start + len <= from {
                continue;
            }
            let cut_start = start.max(from);
            let cut_end = (start + len).min(to);
            if cut_start > pos {
                emit(&data[pos..cut_start]);
            }
            pos = cut_end.max(pos);
        }
        if pos < to {
            emit(&data[pos..to]);
        }
    }

    /// 处理一条完整的 APC 内容。
    fn handle_apc(&mut self, content: &[u8]) {
        let Some(payload) = escape_scan::kitty_graphics_payload(content) else {
            // 非图形协议的 APC：直接忽略。
            return;
        };

        // 解析失败**一律静默**，与 ghostty 一致。
        //
        // ghostty 在 `stream_terminal.zig:1834` 把解析失败归入 `.unknown` 分支，
        // 交给 `unknownSequence()` 处理——它不产生任何应答。只有**解析成功**
        // 而执行失败时才回包，且被 `q=` 门控（`graphics_exec.zig:122-128`）。
        //
        // 这里无法「按 q 静默」：解析都失败了，`q=` 本身就没解析出来。ghostty
        // 的选择是无条件丢弃，我们照抄。这一点对 terminal-browser 尤其重要——
        // 它对所有图形命令用 `q=2`，若此处回包就会往终端输出流里塞入客户端
        // 没预期的字节。
        let mut parser = Parser::new();
        if parser.feed_slice(payload).is_err() {
            return;
        }
        let Ok(cmd) = parser.finish() else {
            return;
        };

        match self.store.execute(cmd, self.cursor) {
            Ok(response) => {
                if !response.reply.is_empty() {
                    self.sink.reply(&response.reply);
                }
                if response.images_changed {
                    self.sink.images_changed();
                }
            }
            Err(err) => {
                let msg = err.message();
                self.sink.reply(format!("\x1b_G;{msg}\x1b\\").as_bytes());
            }
        }
    }
}

impl PtyScanner for KittyGraphicsScanner {
    fn scan(
        &mut self,
        data: &[u8],
        pending: &mut Vec<u8>,
        emit: &mut dyn FnMut(&[u8]),
        grid: &mut dyn FnMut() -> crate::terminal::local_pty::GridState,
    ) {
        // 有跨读残尾时先拼接（仅在确有残尾时拷贝一次）。
        let joined: Vec<u8>;
        let data: &[u8] = if pending.is_empty() {
            data
        } else {
            let mut v = std::mem::take(pending);
            v.extend_from_slice(data);
            joined = v;
            joined.as_slice()
        };

        // 尺寸上报类序列（`CSI 16 t` / `DECSET ?2048`）先处理并摘除。
        //
        // 必须在这里、且覆盖 fast path：终端应用的探测通常单独成块、附近没有
        // APC（terminal-browser 启动时正是如此），而 fast path 会直接整段
        // `emit` 返回——只在慢路径嗅探会完全漏掉探测。
        //
        // 在**整块** `data` 上一次性扫描（而非按切片），因此不存在「序列被两个
        // emit 切片切开」的连续性隐患。
        let removals = self.handle_size_sequences(data);

        // 无 APC 起始符时整段透传（摘下尺寸序列后），零额外开销。
        if memchr::memmem::find(data, escape_scan::APC_START).is_none() {
            Self::emit_without_removals(data, &removals, emit, 0, data.len());
            return;
        }

        let mut pos = 0;
        while let Some(offset) = memchr::memmem::find(&data[pos..], escape_scan::APC_START) {
            let apc_pos = pos + offset;
            // APC 之前的普通文本原样交给 vte——必须在处理 APC **之前**交出，
            // 这样 `cursor()` 才能反映这些文本（如 `CSI 3;5H` 定位）的效果。
            // 尺寸序列从这一段里摘除。
            Self::emit_without_removals(data, &removals, emit, pos, apc_pos);

            match escape_scan::extract_apc(&data[apc_pos..]) {
                Extract::Complete { len, content } => {
                    // 瞄准此刻的网格状态：前面的文本已经喂给 vte 了。
                    let state = grid();
                    self.cursor = state.cursor;
                    self.store.set_visible_rows(Some(state.visible));
                    // 图形 APC 被消费，不进入 vte（vte 会丢弃内容，但序列
                    // 本身仍会走一遍状态机——提前摘除更干净）。
                    self.handle_apc(content);
                    pos = apc_pos + len;
                }
                Extract::Incomplete => {
                    if data.len() - apc_pos > MAX_PENDING_APC {
                        // 残尾过大：放弃整条，避免无限占用内存。
                        // 已 `emit` 的前缀保持原样；剩余部分透传给 vte。
                        Self::emit_without_removals(data, &removals, emit, apc_pos, data.len());
                        return;
                    }
                    *pending = data[apc_pos..].to_vec();
                    return;
                }
                Extract::NotStart => {
                    pos = apc_pos + escape_scan::APC_START.len();
                }
            }
        }

        Self::emit_without_removals(data, &removals, emit, pos, data.len());
    }

    /// 取出待回写的协议应答，交给读线程直接写 PTY。
    ///
    /// 与 `ReplySink::reply` 的分工：`reply` 负责"攒"，这里负责"取"。
    /// 本地路径下 `sink` 是 `KittyReplySink`（共享 `Mutex<Vec<u8>>`），
    /// 取走后 UI 线程的 `flush_kitty_replies` 自然看到空缓冲、不会重复回写。
    fn take_replies(&mut self) -> Vec<u8> {
        self.sink.take_pending()
    }

    /// 读线程在 resize 时转发新几何。
    ///
    /// 转发到 [`KittyGraphicsScanner::set_geometry`]，后者在 in-band size
    /// reports 启用时会顺带重发一次上报。
    fn set_geometry(&mut self, geometry: escape_scan::TermGeometry) {
        KittyGraphicsScanner::set_geometry(self, geometry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ghostty `size_report.zig` 测试用的同一组数字：
    /// cell 9x18、24 行、80 列。
    fn geometry_fixture() -> escape_scan::TermGeometry {
        escape_scan::TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        }
    }

    fn shared_storage() -> crate::terminal::kitty::exec::SharedStorage {
        std::sync::Arc::new(std::sync::Mutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ))
    }

    /// 构造一个带初始几何、应答攒在 `CollectingSink` 里的扫描器。
    fn scanner_with_geometry() -> KittyGraphicsScanner {
        KittyGraphicsScanner::with_storage_and_geometry(
            shared_storage(),
            true,
            Box::new(CollectingSink::default()),
            geometry_fixture(),
        )
    }

    /// 测试用的网格状态：光标在左上角，可见窗口覆盖 0..=23 行（24 行视口、
    /// 未滚动）。需要别的窗口的用例自行构造 `GridState`。
    fn grid_state_fixture() -> crate::terminal::local_pty::GridState {
        crate::terminal::local_pty::GridState {
            cursor: (0, 0),
            visible: crate::terminal::kitty::storage::VisibleRows::new(0, 24),
        }
    }

    /// 跑一遍 `scan`（即 `PtyScanner` 入口），返回 `(emit 出去的字节, 应答)`。
    ///
    /// 强调：必须走 `scan` 而不是 `scan_apc`——`scan_apc` 只接收已经切分好的
    /// **APC 内容**，而尺寸查询根本不是 APC（附近通常没有任何 APC）。用
    /// `scan_apc` 写这类测试会「空过」：它永远不会看到 `\x1b[16t`。
    fn run_scan(scanner: &mut KittyGraphicsScanner, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut pending = Vec::new();
        let mut emitted = Vec::new();
        scanner.scan(
            input,
            &mut pending,
            &mut |bytes| emitted.extend_from_slice(bytes),
            &mut grid_state_fixture,
        );
        assert!(pending.is_empty(), "测试输入不应留下跨读残尾");
        (emitted, scanner.take_replies())
    }

    #[test]
    fn csi_16_t_replies_with_cell_pixels() {
        let mut scanner = scanner_with_geometry();
        let (emitted, replies) = run_scan(&mut scanner, b"\x1b[16t");

        assert_eq!(replies, b"\x1b[6;18;9t", "应答必须是 高;宽 顺序");
        // 序列被摘除，不再进 vte（vte 会把它记为 unhandled）。
        assert!(
            emitted.is_empty(),
            "CSI 16 t 应被吞掉，实际透传了 {:?}",
            String::from_utf8_lossy(&emitted)
        );
    }

    /// 关键回归：终端应用的探测通常单独成块、附近没有 APC，走的正是
    /// `scan` 的 fast path。若只在慢路径嗅探，这条会失败。
    #[test]
    fn csi_16_t_answered_on_fast_path_without_apc() {
        let mut scanner = scanner_with_geometry();
        // 前后都是普通文本，没有任何 APC 起始符。
        let (emitted, replies) = run_scan(&mut scanner, b"before\x1b[16tafter");

        assert_eq!(replies, b"\x1b[6;18;9t");
        assert_eq!(
            emitted, b"beforeafter",
            "只有 CSI 16 t 该被摘除，两侧文本必须原样透传"
        );
    }

    /// 与上一条对称：序列与 APC 混在同一个 chunk 里（慢路径）也要应答。
    #[test]
    fn csi_16_t_answered_on_slow_path_with_apc() {
        let mut scanner = scanner_with_geometry();
        let (emitted, replies) = run_scan(
            &mut scanner,
            b"\x1b[16t\x1b_Gi=999,a=q,t=d,f=24,s=1,v=1;AAAA\x1b\\",
        );

        // 两条应答都会出现：尺寸查询的 `CSI 6;..t` 与 kitty 探测的 `Gi=999;OK`。
        assert!(
            replies.starts_with(b"\x1b[6;18;9t"),
            "尺寸查询应答应排在前面，实际 {:?}",
            String::from_utf8_lossy(&replies)
        );
        assert!(
            emitted.is_empty(),
            "尺寸序列与图形 APC 都由本层消费，不该有任何字节进 vte"
        );
    }

    /// 几何未知时**保持沉默**：回一个 `6;0;0t` 会让客户端算出退化宽高比。
    #[test]
    fn csi_16_t_without_geometry_is_silent() {
        let mut scanner = KittyGraphicsScanner::with_storage(
            shared_storage(),
            true,
            Box::new(CollectingSink::default()),
        );
        let (_emitted, replies) = run_scan(&mut scanner, b"\x1b[16t");

        assert!(replies.is_empty(), "几何未知时不应作答");
    }

    /// 回归护栏：`14`/`18` 已被 vte 正确处理，绝不能在这里被吞掉或改写。
    #[test]
    fn csi_14_and_18_still_forwarded_to_vte() {
        let mut scanner = scanner_with_geometry();
        let input = b"\x1b[14t\x1b[18t";
        let (emitted, replies) = run_scan(&mut scanner, input);

        assert_eq!(emitted, input, "14/18 必须原样透传给 vte");
        assert!(replies.is_empty(), "14/18 的应答由 vte 负责，本层不得插手");
    }

    #[test]
    fn unrelated_csi_passes_through() {
        let mut scanner = scanner_with_geometry();
        let input = b"\x1b[6;9H\x1b[?25h";
        let (emitted, replies) = run_scan(&mut scanner, input);

        assert_eq!(emitted, input);
        assert!(replies.is_empty());
    }

    #[test]
    fn mode_2048_enable_emits_report_then_rereports_on_resize() {
        let mut scanner = scanner_with_geometry();

        // 置位：立即上报当前几何（对齐 ghostty `reportMode2048`）。
        let (emitted, replies) = run_scan(&mut scanner, b"\x1b[?2048h");
        assert_eq!(replies, b"\x1b[48;24;80;432;720t");
        assert!(emitted.is_empty(), "DECSET 序列应被吞掉");

        // resize：启用期间每次几何变化都重新上报（对齐 ghostty `resize()`）。
        scanner.set_geometry(escape_scan::TermGeometry {
            rows: 30,
            cols: 100,
            cell_width: 9,
            cell_height: 18,
        });
        assert_eq!(
            scanner.take_replies(),
            b"\x1b[48;30;100;540;900t",
            "启用期间 resize 必须重新上报"
        );

        // 复位后静默，且不再随 resize 上报。
        let (_emitted, replies) = run_scan(&mut scanner, b"\x1b[?2048l");
        assert!(replies.is_empty(), "复位本身不应产生上报");

        scanner.set_geometry(escape_scan::TermGeometry {
            rows: 40,
            cols: 120,
            cell_width: 9,
            cell_height: 18,
        });
        assert!(
            scanner.take_replies().is_empty(),
            "复位后 resize 不应再上报"
        );
    }

    #[test]
    fn mode_2048_without_geometry_enable_is_silent() {
        let mut scanner = KittyGraphicsScanner::with_storage(
            shared_storage(),
            true,
            Box::new(CollectingSink::default()),
        );
        let (_emitted, replies) = run_scan(&mut scanner, b"\x1b[?2048h");
        assert!(replies.is_empty(), "几何未知时置位不应上报");

        // 但要记住状态：几何一到位就应开始上报。
        scanner.set_geometry(geometry_fixture());
        assert_eq!(scanner.take_replies(), b"\x1b[48;24;80;432;720t");
    }

    #[test]
    fn mode_2048_multi_param_form_is_recognised() {
        let mut scanner = scanner_with_geometry();
        let (emitted, replies) = run_scan(&mut scanner, b"\x1b[?2048;2004h");
        // 2048 被识别并上报几何。
        assert_eq!(replies, b"\x1b[48;24;80;432;720t");
        // 但整条序列**必须原样透传**：同一序列里的 2004（bracketed paste）
        // 要交给 vte 处理。整条摘除会让粘贴保护静默失效。
        assert_eq!(
            emitted, b"\x1b[?2048;2004h",
            "多参数序列必须透传给 vte（否则 2004 等模式丢失）"
        );
        assert!(scanner.mode_2048, "2048 本身仍须被识别并置位");
    }

    /// 未启用 2048 时，resize 不应产生任何上报（与启用态形成对照）。
    #[test]
    fn resize_without_mode_2048_is_silent() {
        let mut scanner = scanner_with_geometry();
        scanner.set_geometry(geometry_fixture());
        assert!(scanner.take_replies().is_empty());
    }

    #[test]
    fn set_geometry_through_trait_updates_state() {
        // 走 trait 方法（读线程的调用路径），确认与固有方法行为一致。
        let mut scanner = KittyGraphicsScanner::with_storage(
            shared_storage(),
            true,
            Box::new(CollectingSink::default()),
        );
        PtyScanner::set_geometry(&mut scanner, geometry_fixture());

        let (_emitted, replies) = run_scan(&mut scanner, b"\x1b[16t");
        assert_eq!(replies, b"\x1b[6;18;9t");
    }
}
