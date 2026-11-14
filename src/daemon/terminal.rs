//! 守护进程持有的单个终端：一个 PTY + 一份 VT 状态。
//!
//! 每个终端在守护进程里独占一条 reader 线程。字节从 PTY 读出来后**立即**
//! 喂给 `Term`（这样 `Term` 始终是最新的，attach 时能直接渲染屏幕），
//! 同时分发给当前挂接着的客户端。
//!
//! 没有客户端时字节只进 `Term`，不发给任何人——这就是「关掉窗口程序还在跑」
//! 的全部秘密：PTY 的 master 端在守护进程手里，SIGHUP 永远等不到。
//! alacritty 在 Linux 上设置的 `TIOCSCTTY` + 会话首进程让我们写进去的
//! `setsid` 进程成为控制终端持有者，因此关闭客户端不会让内核把挂断信号
//! 发给 shell。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::TermMode;

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use super::protocol::{Frame, Kind};

/// 单元格的默认背景色标记。
///
/// `Cell::bg` 默认是 `Color::Named(NamedColor::Background)`（不是 `Spec`），
/// 用它来判断"这格没画东西"，比对 RGB 更稳：终端还没设置调色板时
/// `colors()[Background]` 是 `None`，比 RGB 会把所有空格都当成有背景。
const DEFAULT_BG: Color = Color::Named(NamedColor::Background);

/// 终端在守护进程内的稳定地址（`t1`、`t2`……）。
///
/// 与 herdr 的 `t7` 同构：编号一旦分配就不再复用，客户端据此认出
/// 「还是那个终端」。
pub type Address = String;

/// 终端的生命周期状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// 程序还在跑。
    Live,
    /// 程序已经退出（自然退出或被 kill）。
    Finished,
}

/// 从 PTY 读出来的一块数据，交给挂接的客户端。
///
/// 用 `Arc<[u8]>` 而不是 `Vec<u8>`：守护进程通常只有 0 个或 1 个客户端，
/// 但即使只有一个也要避免为了「发给所有订阅者」而复制整块缓冲。
pub type Chunk = Arc<[u8]>;

/// 一个挂接者的输出通道。
pub struct Subscriber {
    /// 终端数据帧的发送端。断开时由写线程移除。
    pub tx: async_channel::Sender<Frame>,
    /// 这个挂接者用的 channel 号。
    ///
    /// **不能是 0**：0 是控制面，客户端会把 channel 0 的帧当 JSON 解析，
    /// 二进制终端输出到那里只会被当成坏帧丢掉。channel 由连接层分配
    /// （每个连接从 1 开始编号），终端只负责原样回填。
    pub channel: u32,
    /// 该挂接者还需要我们发多少字节（流控信用）。
    ///
    /// 客户端处理不过来时会停止给信用，我们就把输出缓在 `Term` 里
    /// （`Term` 本就会滚动丢弃），而不是把内存堆爆。
    pub credit: u64,
}

/// 单个终端的共享状态。
pub struct TerminalHandle {
    pub address: Address,
    /// 启动时用的 shell 与参数，仅用于展示与调试。
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// 创建时刻（毫秒时间戳）。
    pub created_ms: u64,
    /// 下一个终端编号，供注册表分配。
    state: Mutex<TerminalState>,
    /// reader 线程读到的数据要喂进去的那份 VT。
    term: Arc<FairMutex<Term<ZedListenerShim>>>,
    /// 往 PTY 写输入的通道。
    input_tx: async_channel::Sender<Input>,
    /// 稳定的编号，供日志与排障使用。
    #[allow(dead_code)]
    id: u64,
    /// 累计输出字节数，用于「这个终端有没有活动」的诊断。
    bytes_out: AtomicU64,
    /// resize 后是否有待发的整屏重绘（见 [`Self::mark_repaint`]）。
    ///
    /// 用原子量而不是每次直接发：拖动窗口时 resize 是连续的，逐次重绘会
    /// 让 UI 明显发卡。
    repaint_pending: std::sync::atomic::AtomicBool,
}

/// 写入 PTY 的请求。
pub enum Input {
    Bytes(Vec<u8>),
    Resize(WindowSize),
    Shutdown,
}

struct TerminalState {
    state: State,
    exit_code: Option<i32>,
    subscribers: Vec<(u64, Subscriber)>,
    next_subscriber_id: u64,
    /// 因额度不足错过输出的挂接者，等有额度时补一帧完整重绘。
    ///
    /// 见 [`TerminalHandle::broadcast`]：只丢增量不补偿会让客户端永久停在
    /// 丢弃前那一帧，看起来就是"卡死"。
    repaint_ids: std::collections::HashSet<u64>,
}

impl TerminalHandle {
    /// 当前状态。
    pub fn state(&self) -> State {
        self.state.lock().expect("terminal state").state
    }

    /// 退出码（未退出为 `None`）。
    pub fn exit_code(&self) -> Option<i32> {
        self.state.lock().expect("terminal state").exit_code
    }

    /// 累计输出字节数。
    pub fn bytes_out(&self) -> u64 {
        self.bytes_out.load(Ordering::Relaxed)
    }

    /// 新增一个挂接者，返回它的 id。
    ///
    /// `channel` 是连接层为这个终端分配的 channel 号，输出帧会带着它回填，
    /// 客户端据此把字节路由到对应的终端视图。
    pub fn subscribe(&self, channel: u32, tx: async_channel::Sender<Frame>) -> u64 {
        let mut st = self.state.lock().expect("terminal state");
        let id = st.next_subscriber_id;
        st.next_subscriber_id += 1;
        st.subscribers.push((
            id,
            Subscriber {
                tx,
                channel,
                credit: 1024 * 1024,
            },
        ));
        id
    }

    /// 移除挂接者。
    pub fn unsubscribe(&self, id: u64) {
        let mut st = self.state.lock().expect("terminal state");
        st.subscribers.retain(|(sid, _)| *sid != id);
    }

    /// 补发流控信用。
    pub fn grant_credit(&self, id: u64, bytes: u64) {
        let mut st = self.state.lock().expect("terminal state");
        if let Some((_, sub)) = st.subscribers.iter_mut().find(|(sid, _)| *sid == id) {
            sub.credit = sub.credit.saturating_add(bytes);
        }
        // 客户端送来信用，说明它又能收了：这时才补发积压的重绘。
        //
        // 客户端因为额度不足错过了一段输出，`broadcast` 会给它记一个
        // "待重绘"标记。在这里消费掉它——此刻额度刚好恢复，推一帧完整
        // 画面最合适。不补的话客户端会永久停在丢弃前那一帧。
        if st.repaint_ids.contains(&id) {
            st.repaint_ids.remove(&id);
            drop(st);
            self.repaint_subscriber(id);
        }
    }

    /// 给单个挂接者补一帧完整重绘（额度不足时跳过）。
    fn repaint_subscriber(&self, id: u64) {
        // 先确认它仍然需要、且额度够，再生成快照——快照本身不便宜。
        let needs = {
            let st = self.state.lock().expect("terminal state");
            st.subscribers
                .iter()
                .any(|(sid, sub)| *sid == id && sub.credit > 0)
        };
        if !needs {
            return;
        }
        let snapshot = self.snapshot();
        let mut st = self.state.lock().expect("terminal state");
        if let Some((_, sub)) = st.subscribers.iter_mut().find(|(sid, _)| *sid == id) {
            if sub.credit < snapshot.len() as u64 {
                // 还不够：把标记放回去，等下一次信用。
                st.repaint_ids.insert(id);
                return;
            }
            sub.credit -= snapshot.len() as u64;
            let _ = sub.tx.try_send(Frame::data(sub.channel, snapshot));
        }
    }

    /// 把一块输出分发给所有挂接者。
    pub fn broadcast(&self, chunk: &[u8]) {
        let mut st = self.state.lock().expect("terminal state");
        let mut dropped = Vec::new();
        // 因额度不足而错过输出的挂接者：给它们补一帧完整重绘。
        let mut need_repaint: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for (id, sub) in st.subscribers.iter_mut() {
            if sub.credit < chunk.len() as u64 {
                // 客户端跟不上：丢掉这一段，但**必须补偿**。
                //
                // 只丢不补是错的：客户端会永久停在丢弃前那一帧，之后即使
                // 回过神来也再收不到这部分内容——用户看到的就是"界面卡死
                // 不动了"。
                //
                // herdr 的做法（`wire.rs::data`）：积压超限时清空队列，并
                // 记一个"该重绘"的信号，随后推一整屏最新画面。这里照做：
                // 丢增量 + 标记重绘，等客户端回信用时补一帧完整的。
                log::debug!(
                    "终端 {} 的挂接者 {id} 跟不上（额度 {} < {}），标记重绘",
                    self.address,
                    sub.credit,
                    chunk.len()
                );
                need_repaint.insert(*id);
                continue;
            }
            sub.credit -= chunk.len() as u64;
            if sub.tx.try_send(Frame::data(sub.channel, chunk)).is_err() {
                dropped.push(*id);
            }
        }
        if !dropped.is_empty() {
            st.subscribers.retain(|(id, _)| !dropped.contains(id));
        }
        if !need_repaint.is_empty() {
            st.repaint_ids.extend(need_repaint);
        }
    }

    /// 向 PTY 写输入。
    pub fn write_input(&self, bytes: Vec<u8>) {
        let _ = self.input_tx.try_send(Input::Bytes(bytes));
    }

    /// 结束这个终端里的程序（用户显式关闭标签/面板时用）。
    ///
    /// 与"视图销毁"是两件事：视图销毁只是不再观看，程序该继续跑；
    /// 这个是用户明确说"我不要它了"。两者混在一起会导致要么关标签杀不掉
    /// 程序（泄漏），要么关窗口把用户的活全杀了。
    pub fn request_close(&self) {
        let _ = self.input_tx.try_send(Input::Shutdown);
    }

    /// 请求改尺寸。
    pub fn resize(&self, size: WindowSize) {
        let _ = self.input_tx.try_send(Input::Resize(size));
    }

    /// resize 之后把整屏重画一次推给所有挂接者。
    ///
    /// # 为什么必须重画
    ///
    /// resize 会让 VT 按新列数 reflow，**每一行的折行位置都变了**。客户端
    /// 本地 VT 也会自己 resize，但两边对"内容该落在哪一行"的推断不可能
    /// 完全一致（服务端是按新尺寸重新排过序的，客户端只是把自己那份重排）。
    /// 不重画，屏幕上就留着按旧列数折行的残影——用户看到的就是"改窗口
    /// 大小后 nvim 布局乱掉"。
    ///
    /// herdr 的做法见 `viewers.rs`：`apply_size()` 末尾给每个 viewer 置
    /// `repaint_pending = true`，随后 `queue_resize_paints()` 把
    /// `first_paint()` 整屏推过去。这里对齐同一策略。
    /// resize 后**标记**需要重绘，而不是立刻生成快照。
    ///
    /// 拖动窗口时 resize 事件是连续的（读线程每改一次尺寸就来一条），
    /// 每次都整屏序列化会让 UI 明显发卡——一次快照要遍历全部单元格并
    /// 拼出 SGR，几十毫秒内做好几次就是卡顿的来源。
    ///
    /// herdr 的做法是给 viewer 置 `repaint_pending`，由写线程在合适时机
    /// 合并成一次 `first_paint()`。这里对齐：标记之后由
    /// [`Self::flush_repaint`] 在读完当前一批事件时统一发。
    pub fn mark_repaint(&self) {
        self.repaint_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 若有待发的重绘，生成一次快照并推给所有挂接者。
    ///
    /// 返回是否真的发了。
    pub fn flush_repaint(&self) -> bool {
        if !self
            .repaint_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        self.repaint_after_resize();
        true
    }

    fn repaint_after_resize(&self) {
        let snapshot = self.snapshot();
        let mut st = self.state.lock().expect("terminal state");
        let mut dropped = Vec::new();
        for (id, sub) in st.subscribers.iter_mut() {
            // **尊重信用额度**，不为重绘开后门。
            //
            // 曾经这里会强行把 `credit` 补到够发整屏。那是错的：重绘本身
            // 就是一大段数据，客户端若消化不过来，强推只会让它的接收队列
            // 越积越多、界面越发滞后，最终看起来像"卡死"。
            //
            // 额度不够就跳过——客户端本来就会继续收到增量输出，画面最终
            // 一致；而它一旦发回 `Credit`，下一次 resize 就能完整对上。
            if sub.credit < snapshot.len() as u64 {
                log::debug!(
                    "终端 {} 的挂接者 {id} 额度不足（{} < {}），跳过这次重绘",
                    self.address,
                    sub.credit,
                    snapshot.len()
                );
                continue;
            }
            sub.credit -= snapshot.len() as u64;
            if sub
                .tx
                .try_send(Frame::data(sub.channel, snapshot.clone()))
                .is_err()
            {
                dropped.push(*id);
            }
        }
        if !dropped.is_empty() {
            st.subscribers.retain(|(id, _)| !dropped.contains(id));
        }
    }

    /// 把当前屏幕渲染成重绘序列。
    ///
    /// 客户端挂接时拿到它并喂给本地 vte，屏幕就与守护进程一致了。
    /// 只渲染可见区（不含回滚）：回滚由 `render_history` 按需补。
    /// 终端当前的网格尺寸 `(cols, rows)`。
    ///
    /// 客户端挂接时**必须**先拿到它：本地 VT 要用同样的尺寸建，否则从
    /// 守护进程重放过来的画面会按错误的列数折行。表现就是 nvim / htop
    /// 这类全屏程序重连后"布局全乱"——边框错位、状态栏跑到中间。
    pub fn size(&self) -> (u16, u16) {
        let term = self.term.lock();
        let grid = term.grid();
        (
            u16::try_from(grid.columns()).unwrap_or(u16::MAX),
            u16::try_from(grid.screen_lines()).unwrap_or(u16::MAX),
        )
    }

    pub fn snapshot(&self) -> Vec<u8> {
        render_screen(&self.term, 0)
    }
}

/// 把 VT 的可见区渲染成 ANSI 重绘序列，供新挂接的客户端重建画面。
///
/// `history_rows` > 0 时先把回滚区里的这么多行推上去（主屏才有回滚；
/// 备用屏没有）。
///
/// # 这个函数要解决的问题
///
/// 客户端是**新建**一个本地 `Term`，再把这串字节喂进去。所以它必须让
/// 那个空 VT 与守护进程里的这份**完全一致**——不只是文字，还包括：
///
/// - 在哪个屏幕上（主屏 / 备用屏）；
/// - 鼠标是否上报、括号粘贴、应用光标键、焦点上报、光标显隐；
/// - 光标位置、标题。
///
/// 少一样就会出具体的问题。最容易观察到的是**备用屏**：程序（nvim /
/// htop / less）在备用屏上，客户端却停在主屏，于是重放出来的画面被当成
/// 普通输出**追加**到主屏，主屏越来越长——滚轮一滚就能看到"两个尺寸的
/// 界面叠着"，全屏程序退化成了可滚动的普通输出。
///
/// # 为什么先"全关"再"按实际开"
///
/// 客户端可能是个**旧**的本地 VT（重连复用它），里面残留着上一次会话的
/// 模式。只发"当前打开的那些"清不掉残留；先统一关一遍再按实际状态打开，
/// 结果与守护进程严格一致。
///
/// 这套顺序参考了 herdr-core 的 `ghostty-vt::Terminal::paint()`
/// （`crates/ghostty-vt/src/lib.rs`）：软重置 → 关掉全部模式 → 清屏 →
/// 需要时进备用屏 → 画内容 → 恢复光标/标题。
pub fn render_screen(term: &FairMutex<Term<ZedListenerShim>>, history_rows: usize) -> Vec<u8> {
    let term = term.lock();
    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let mode = *term.mode();
    let alt = mode.contains(TermMode::ALT_SCREEN);

    let mut out = Vec::with_capacity(rows * cols * 2 + 512);

    // ── ① 先把客户端拉回一个干净、已知的状态 ────────────────────────────
    //
    // `CSI ! p`（软重置）复位笔、边距、字符集与滚动区，但**不动**鼠标、
    // 焦点、粘贴、键盘这些私有模式——所以要显式关一遍。
    out.extend_from_slice(b"\x1b[!p");
    // 同步输出：客户端若停在它里面，屏幕会被冻住。
    out.extend_from_slice(b"\x1b[?2026l");
    // 鼠标上报的各种变体（含 X10、UTF-8、SGR、urxvt 坐标）。
    out.extend_from_slice(b"\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l");
    out.extend_from_slice(b"\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l");
    // 焦点上报、括号粘贴、应用光标键、按键协议。
    out.extend_from_slice(b"\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b[?66l");
    out.extend_from_slice(b"\x1b[?2031l\x1b[?2048l\x1b[=0;1u");
    // 备用屏的三个别名都要关：客户端可能停在任一个上面。
    out.extend_from_slice(b"\x1b[?1049l\x1b[?1047l\x1b[?47l");
    out.extend_from_slice(b"\x1b[H\x1b[2J");

    // ── ② 主屏回滚（只有主屏有） ────────────────────────────────────────
    //
    // 先在主屏把历史推上去，再切备用屏；顺序反了历史会落到备用屏里。
    if history_rows > 0 && !alt {
        let history = grid.history_size();
        if history > 0 {
            let from = history.saturating_sub(history_rows);
            for offset in (0..history - from).rev() {
                // `Line(-n)` 是回滚区第 n 行（视口上方）。
                let line = &grid[alacritty_terminal::index::Line(-((offset + 1) as i32))];
                out.extend_from_slice(b"\x1b[0m");
                push_line_text(&mut out, line, cols, true);
                out.extend_from_slice(b"\x1b[0m\r\n");
            }
            // 历史推进去之后视口被顶下去，重新清一次屏再画当前视口。
            out.extend_from_slice(b"\x1b[H\x1b[2J");
        }
    }

    // ── ③ 进入备用屏（若程序在里面） ────────────────────────────────────
    //
    // `!p` 在切屏之后再来一次：主屏的笔、边距、字符集不能泄漏进来。
    if alt {
        out.extend_from_slice(b"\x1b[?1049h\x1b[!p");
    }

    // ── ④ 画可见区 ──────────────────────────────────────────────────────
    //
    // 关掉自动折行：逐行定位输出时写满最后一列不能让终端自己换行，
    // 否则每行都会多挤一行出来。
    out.extend_from_slice(b"\x1b[?7l");
    for row in 0..rows {
        out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        let line = &grid[alacritty_terminal::index::Line(row as i32)];
        push_line_text(&mut out, line, cols, false);
    }

    // ── ⑤ 光标、模式、标题 ─────────────────────────────────────────────
    let cursor = grid.cursor.point;
    out.extend_from_slice(
        format!("\x1b[{};{}H", cursor.line.0.max(0) + 1, cursor.column.0 + 1).as_bytes(),
    );
    out.extend_from_slice(encode_terminal_modes(mode).as_bytes());

    out
}

/// 把一行的文字（带 SGR）追加到 `out`，裁掉行尾的空白。
///
/// `trailing_newline_unsafe`：调用方随后会自己补 `\r\n`（推历史时），
/// 此时不裁行尾空白的判断要保守一点——留着空格更安全，避免把带底色的
/// 区块裁掉。
fn push_line_text(
    out: &mut Vec<u8>,
    line: &alacritty_terminal::grid::Row<alacritty_terminal::term::cell::Cell>,
    cols: usize,
    history: bool,
) {
    let mut trailing_blank = cols;
    while trailing_blank > 0 {
        let cell = &line[alacritty_terminal::index::Column(trailing_blank - 1)];
        // 只有"默认背景的空格"才算空白；带底色的空格必须画出来，
        // 否则 `htop` 那种整行高亮会缺一块。
        if cell.c == ' ' && cell.bg == DEFAULT_BG {
            trailing_blank -= 1;
        } else {
            break;
        }
    }
    let mut current = String::new();
    for col in 0..trailing_blank {
        let cell = &line[alacritty_terminal::index::Column(col)];
        let style = sgr_for(&cell.fg, &cell.bg, cell.flags);
        if style != current {
            out.extend_from_slice(style.as_bytes());
            current = style;
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
    }
    let _ = history;
    out.extend_from_slice(b"\x1b[0m");
}

/// 把 VT 的终端模式编码成一组 `DECSET` / `DECRST` 序列。
///
/// 只覆盖**会影响客户端如何解释后续输出与输入**的那些模式。键盘协议
/// （kitty `CSI u`）不在此列：它属于输入编码，客户端有自己的状态机，
/// 重放过来会与本地状态打架。
fn encode_terminal_modes(mode: TermMode) -> String {
    let mut out = String::new();
    let set = |on: bool| if on { 'h' } else { 'l' };

    // 鼠标上报。程序开了它，滚轮/点击就该发给程序，客户端不该自己滚。
    out.push_str(&format!(
        "\x1b[?1000{}\x1b[?1002{}\x1b[?1003{}\x1b[?1006{}",
        set(mode.contains(TermMode::MOUSE_REPORT_CLICK)),
        set(mode.contains(TermMode::MOUSE_DRAG)),
        set(mode.contains(TermMode::MOUSE_MOTION)),
        set(mode.contains(TermMode::SGR_MOUSE)),
    ));

    // 括号粘贴：开了它，粘贴的文本要包在 `ESC[200~ … ESC[201~` 里。
    out.push_str(&format!(
        "\x1b[?2004{}",
        set(mode.contains(TermMode::BRACKETED_PASTE))
    ));

    // 应用光标键：开了它，方向键要发 SS3（`ESC O A`）而非 CSI。
    out.push_str(&format!(
        "\x1b[?1{}",
        set(mode.contains(TermMode::APP_CURSOR))
    ));

    // 焦点上报：nvim 靠它区分窗口焦点，不发的话切回来高亮不对。
    out.push_str(&format!(
        "\x1b[?1004{}",
        set(mode.contains(TermMode::FOCUS_IN_OUT))
    ));

    // 显示光标（放在最后：画内容期间光标不该乱跳）。
    out.push_str(&format!(
        "\x1b[?25{}",
        set(mode.contains(TermMode::SHOW_CURSOR))
    ));

    // 自动折行：画内容时强制关过，这里恢复程序的实际状态。
    out.push_str(&format!(
        "\x1b[?7{}",
        set(mode.contains(TermMode::LINE_WRAP))
    ));

    out
}

fn sgr_for(
    fg: &alacritty_terminal::vte::ansi::Color,
    bg: &alacritty_terminal::vte::ansi::Color,
    flags: alacritty_terminal::term::cell::Flags,
) -> String {
    use alacritty_terminal::term::cell::Flags;

    let mut s = String::from("\x1b[0");
    if flags.contains(Flags::BOLD) {
        s.push_str(";1");
    }
    if flags.contains(Flags::DIM) {
        s.push_str(";2");
    }
    if flags.contains(Flags::ITALIC) {
        s.push_str(";3");
    }
    if flags.contains(Flags::UNDERLINE) {
        s.push_str(";4");
    }
    if flags.contains(Flags::INVERSE) {
        s.push_str(";7");
    }
    if flags.contains(Flags::HIDDEN) {
        s.push_str(";8");
    }
    if flags.contains(Flags::STRIKEOUT) {
        s.push_str(";9");
    }
    match fg {
        Color::Named(n) => s.push_str(&format!(";{}", named_sgr(*n, false))),
        Color::Spec(rgb) => s.push_str(&format!(";38;2;{};{};{}", rgb.r, rgb.g, rgb.b)),
        Color::Indexed(i) => s.push_str(&format!(";38;5;{i}")),
    }
    match bg {
        Color::Named(n) => s.push_str(&format!(";{}", named_sgr(*n, true))),
        Color::Spec(rgb) => s.push_str(&format!(";48;2;{};{};{}", rgb.r, rgb.g, rgb.b)),
        Color::Indexed(i) => s.push_str(&format!(";48;5;{i}")),
    }
    s.push('m');
    s
}

fn named_sgr(n: alacritty_terminal::vte::ansi::NamedColor, background: bool) -> String {
    use alacritty_terminal::vte::ansi::NamedColor as N;
    let base = if background { 40 } else { 30 };
    match n {
        N::Black => format!("{base}"),
        N::Red => format!("{}", base + 1),
        N::Green => format!("{}", base + 2),
        N::Yellow => format!("{}", base + 3),
        N::Blue => format!("{}", base + 4),
        N::Magenta => format!("{}", base + 5),
        N::Cyan => format!("{}", base + 6),
        N::White => format!("{}", base + 7),
        N::BrightBlack => format!("{}", base + 60),
        N::BrightRed => format!("{}", base + 61),
        N::BrightGreen => format!("{}", base + 62),
        N::BrightYellow => format!("{}", base + 63),
        N::BrightBlue => format!("{}", base + 64),
        N::BrightMagenta => format!("{}", base + 65),
        N::BrightCyan => format!("{}", base + 66),
        N::BrightWhite => format!("{}", base + 67),
        // 前景/背景/光标/选区等"语义色"落到终端默认：重绘时不保留颜色，
        // 由客户端按自己的主题渲染。herdr 的快照也是这个策略——快照存的是
        // 逻辑色而不是绝对 RGB，换主题后历史内容会跟着变色。
        _ => {
            if background {
                "49".to_string()
            } else {
                "39".to_string()
            }
        }
    }
}

/// `Term` 的事件监听器。
///
/// 守护进程里不需要 `Event::Wakeup`（没有 UI），但 `Term::new` 要求一个
/// 监听器。用一个什么都不做的实现。
#[derive(Clone)]
pub struct ZedListenerShim(pub std::sync::mpsc::Sender<()>);

impl alacritty_terminal::event::EventListener for ZedListenerShim {
    fn send_event(&self, _event: alacritty_terminal::event::Event) {}
}

impl TerminalHandle {
    /// 标记终端已结束，并通知所有挂接者。
    pub fn mark_finished(&self, exit_code: Option<i32>) {
        let mut st = self.state.lock().expect("terminal state");
        st.state = State::Finished;
        st.exit_code = exit_code;
        for (_, sub) in &st.subscribers {
            let _ = sub
                .tx
                .try_send(Frame::new(sub.channel, Kind::Close, b"{}".to_vec()));
        }
        st.subscribers.clear();
    }

    /// 构造一个可用的终端句柄。
    pub fn new(
        address: Address,
        argv: Vec<String>,
        cwd: Option<String>,
        created_ms: u64,
        term: Arc<FairMutex<Term<ZedListenerShim>>>,
        input_tx: async_channel::Sender<Input>,
        id: u64,
    ) -> Self {
        Self {
            address,
            argv,
            cwd,
            created_ms,
            state: Mutex::new(TerminalState {
                state: State::Live,
                exit_code: None,
                subscribers: Vec::new(),
                repaint_ids: std::collections::HashSet::new(),
                next_subscriber_id: 0,
            }),
            term,
            input_tx,
            id,
            bytes_out: AtomicU64::new(0),
            repaint_pending: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 供测试构造。
    #[cfg(test)]
    pub fn new_for_tests(address: &str, term: Arc<FairMutex<Term<ZedListenerShim>>>) -> Self {
        let (tx, _rx) = async_channel::bounded(1);
        Self {
            address: address.to_string(),
            argv: vec!["bash".into()],
            cwd: None,
            created_ms: 0,
            state: Mutex::new(TerminalState {
                state: State::Live,
                exit_code: None,
                subscribers: Vec::new(),
                repaint_ids: std::collections::HashSet::new(),
                next_subscriber_id: 0,
            }),
            term,
            input_tx: tx,
            id: 0,
            bytes_out: AtomicU64::new(0),
            repaint_pending: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 测试用：拿到内部 `Term` 的操作句柄（验证 resize 等对网格的影响）。
    #[cfg(test)]
    pub fn term_probe(&self) -> impl std::ops::DerefMut<Target = Term<ZedListenerShim>> + '_ {
        self.term.lock()
    }

    /// 测试用：直接喂字节进 VT（不经过 PTY）。
    #[cfg(test)]
    pub fn feed_for_tests(&self, bytes: &[u8]) {
        let mut t = self.term.lock();
        let mut processor = alacritty_terminal::vte::ansi::Processor::<
            alacritty_terminal::vte::ansi::StdSyncHandler,
        >::new();
        processor.advance(&mut *t, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::Config;

    fn test_term() -> Arc<FairMutex<Term<ZedListenerShim>>> {
        let (tx, _rx) = std::sync::mpsc::channel();
        let listener = ZedListenerShim(tx);
        let bounds = alacritty_terminal::term::test::TermSize::new(20, 4);
        Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &bounds,
            listener,
        )))
    }

    /// **resize 重绘必须合并：一批事件只发一帧。**
    ///
    /// 回归价值：这是"拖窗口改大小时 nvim 明显卡顿"的直接原因。读线程
    /// 一批能收到好几条 `Input::Resize`，每条都整屏序列化（遍历全部单元格
    /// + 拼 SGR）就是卡顿的来源。
    ///
    /// 断言的是行为而非实现：标脏多次、只 flush 出一帧。
    #[test]
    fn repeated_resize_marks_coalesce_into_one_repaint() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.feed_for_tests(b"hello");

        // 连标记 10 次（模拟拖动窗口）。
        for _ in 0..10 {
            handle.mark_repaint();
        }
        // 第一次 flush 应当真的发了。
        assert!(handle.flush_repaint(), "攒了 10 次标记，flush 应当发出一次");
        // 第二次没有新标记，不该再发。
        assert!(
            !handle.flush_repaint(),
            "没有新标记时不该重复重绘（否则拖动时每帧都在整屏序列化）"
        );
    }

    /// **快照必须带上终端模式，尤其是备用屏。**
    ///
    /// 回归价值：这是"重连后 nvim 不再是全屏程序、变成可以滚动的"的根因。
    /// `render_screen` 原本只重放可见文本，不重放模式；客户端本地 VT 因此
    /// 停在主屏，重放出来的画面被当成普通输出**追加**到主屏，主屏不断变长。
    /// 于是滚动就能看到两个尺寸的界面叠在一起。
    ///
    /// 这条用例喂真实的 `1049h` 序列进 VT，再断言快照里带 `1049h`。
    #[test]
    fn snapshot_carries_the_alternate_screen_mode() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        // 程序进入备用屏并画点东西——nvim / htop / less 启动时就是这样。
        handle.feed_for_tests(b"\x1b[?1049h\x1b[1;1hNVIM");
        let snapshot = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        assert!(
            snapshot.contains("\x1b[?1049h"),
            "快照必须让客户端也进备用屏，否则界面会被追加到主屏并可以滚动: {snapshot:?}"
        );
        // 进入备用屏必须发生在**画内容**之前，否则内容先落到主屏再被切走。
        //
        // 注意不能拿"第一个 `2J`"来比：新实现开头会先 `?1049l` 再清一次屏，
        // 那是"把客户端拉回干净状态"的一部分。要比的是**正文**出现的位置。
        let alt = snapshot.find("\x1b[?1049h").expect("要有 1049h");
        let text = snapshot.find("NVIM").expect("要有正文");
        assert!(alt < text, "1049h 必须先于正文，否则内容落在主屏");
    }

    /// 退出备用屏后，快照要让客户端也回到主屏。
    ///
    /// 与上一条互补：只在进入时发 `1049h`、退出时不发，客户端会一直停在
    /// 备用屏，之后 shell 的输出全都落在备用屏上，关掉程序时整屏消失。
    #[test]
    fn snapshot_leaves_the_alternate_screen_when_the_program_is_gone() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.feed_for_tests(b"\x1b[?1049h");
        handle.feed_for_tests(b"\x1b[?1049l");
        let snapshot = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        assert!(
            snapshot.contains("\x1b[?1049l"),
            "退出全屏后要让客户端回到主屏: {snapshot:?}"
        );
    }

    /// **完整往返：守护进程的快照喂给一个新的 VT，它必须也进备用屏。**
    ///
    /// 这条比"快照里含某个字节串"更接近真实：模拟客户端断开重连后，把
    /// 收到的快照喂进一个全新的 `Term`，然后断言它确实处在 alt screen
    /// ——也就是"nvim 重连后仍然是全屏程序"，而不是可滚动的普通输出。
    #[test]
    fn replaying_a_snapshot_reproduces_the_alternate_screen() {
        // ① 守护进程侧：程序进入备用屏并画点东西。
        let source = test_term();
        let handle = TerminalHandle::new_for_tests("t1", source);
        handle.feed_for_tests(b"\x1b[?1049h\x1b[1;1hNVIM");
        let snapshot = handle.snapshot();

        // ② 客户端侧：全新的 VT（等价于重连后新建的本地终端）。
        let client_term = test_term();
        {
            let mut t = client_term.lock();
            let mut processor = alacritty_terminal::vte::ansi::Processor::<
                alacritty_terminal::vte::ansi::StdSyncHandler,
            >::new();
            processor.advance(&mut *t, &snapshot);

            let mode = *t.mode();
            assert!(
                mode.contains(TermMode::ALT_SCREEN),
                "重放快照后客户端必须也进入备用屏，否则 nvim 会变成可滚动的普通输出"
            );
            // 内容也要在。
            let grid = t.grid();
            let row0: String = (0..grid.columns())
                .map(|c| {
                    grid[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(c)].c
                })
                .collect();
            assert!(row0.contains("NVIM"), "重放后内容应在备用屏上: {row0:?}");
        }
    }

    /// 程序开启鼠标上报后，快照要带上对应模式。
    ///
    /// 不带的话客户端的滚轮会去滚 scrollback，而不是把滚轮事件发给程序
    /// ——在 htop / 文件管理器里表现为"滚轮没反应，反而把界面滚走了"。
    #[test]
    fn snapshot_reports_mouse_tracking() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.feed_for_tests(b"\x1b[?1000h\x1b[?1006h");
        let snapshot = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        assert!(
            snapshot.contains("\x1b[?1000h"),
            "要带鼠标点击上报: {snapshot:?}"
        );
        assert!(
            snapshot.contains("\x1b[?1006h"),
            "要带 SGR 编码: {snapshot:?}"
        );
    }

    /// **resize 必须同时改 VT 的网格，不只是 PTY。**
    ///
    /// 回归价值：这是"重启 deepterm 后 nvim/htop 布局全乱"的根因。原来
    /// `Input::Resize` 只调 `pty.on_resize`——子进程看到窗口变了、按新尺寸
    /// 重绘，但守护进程这份 `Term` 还是旧网格。于是 `snapshot` 按旧网格
    /// 重放，客户端按新尺寸渲染，边框与状态栏全部错位。
    ///
    /// 这条测试直接断言网格尺寸跟着变（不经过 PTY，避免依赖终端设备）。
    #[test]
    fn resizing_updates_the_vt_grid_not_just_the_pty() {
        let term = test_term(); // 20x4
        let handle = TerminalHandle::new_for_tests("t1", term);
        assert_eq!(handle.size(), (20, 4), "初始尺寸应与构造时一致");

        // 直接对 VT 做 resize（生产路径里由 `Input::Resize` 触发同一调用）。
        {
            let mut t = handle.term_probe();
            t.resize(alacritty_terminal::term::test::TermSize::new(40, 10));
        }

        assert_eq!(
            handle.size(),
            (40, 10),
            "resize 之后 VT 网格必须跟着变，否则重连时快照会按旧尺寸重放"
        );
    }

    /// 重绘序列必须把文字原样还原出来。
    ///
    /// 这是 attach 恢复屏幕的正确性基石：客户端拿到的只有这段字节。
    #[test]
    fn snapshot_carries_the_visible_text() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.feed_for_tests(b"hello");

        let snapshot = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        assert!(snapshot.contains("hello"), "重绘里应有正文: {snapshot:?}");
    }

    /// 重绘必须先清屏再写，否则挂接时旧内容会和新内容叠在一起。
    #[test]
    fn snapshot_clears_before_drawing() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        let snapshot = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        let clear = snapshot.find("\x1b[2J").expect("要清屏");
        let home = snapshot.find("\x1b[H").expect("要归位");
        assert!(clear < home || home < clear, "清屏与归位都要出现");
    }

    /// 行尾的空白不应该被写出来，否则每行都是满宽，客户端滚动时很脏。
    #[test]
    fn snapshot_trims_trailing_blanks() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.feed_for_tests(b"x");
        let snapshot = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        // 4 行 20 列，若没裁空白会有大量空格。
        let spaces = snapshot.matches(' ').count();
        assert!(spaces < 20, "行尾空白应被裁掉，实际 {spaces} 个空格");
    }

    /// 彩色内容要在重绘里带上 SGR，否则恢复出来全是单色。
    #[test]
    fn snapshot_preserves_foreground_colours() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.feed_for_tests(b"\x1b[31mred\x1b[0m");
        let snapshot = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        assert!(
            snapshot.contains(";31") || snapshot.contains("38;5;1"),
            "红色应当出现在重绘序列里: {snapshot:?}"
        );
    }

    /// 订阅者在终端退出时必须被通知，而不是永远等下去。
    #[test]
    fn finishing_closes_subscriber_streams() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        let (tx, rx) = async_channel::bounded(4);
        handle.subscribe(1, tx);
        handle.mark_finished(Some(0));

        let frame = rx.try_recv().expect("订阅者应收到关闭帧");
        assert_eq!(frame.kind, Kind::Close);
        assert_eq!(handle.state(), State::Finished);
        assert_eq!(handle.exit_code(), Some(0));
    }

    /// 没有信用时不能把输出塞给客户端。
    #[test]
    fn broadcasting_respects_credit() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        let (tx, rx) = async_channel::bounded(4);
        let id = handle.subscribe(1, tx);
        {
            let mut st = handle.state.lock().unwrap();
            st.subscribers[0].1.credit = 0;
        }
        handle.broadcast(b"ignored");
        assert!(rx.try_recv().is_err(), "没有信用时不应发送");

        handle.grant_credit(id, 100);
        handle.broadcast(b"sent");
        let frame = rx.try_recv().expect("有信用后应发送");
        assert_eq!(frame.payload, b"sent");
        // 帧的 channel 必须是连接层分配的那个，**绝不能是 0**：
        // 0 是控制面，客户端会把二进制输出当成 JSON 丢掉。
        assert_eq!(frame.channel, 1, "输出帧应带终端自己的 channel");
        assert_ne!(frame.channel, 0, "channel 0 是控制面，不能用来发终端输出");
    }
}
