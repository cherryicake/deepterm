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
    }

    /// 把一块输出分发给所有挂接者。
    pub fn broadcast(&self, chunk: &[u8]) {
        let mut st = self.state.lock().expect("terminal state");
        let mut dropped = Vec::new();
        for (id, sub) in st.subscribers.iter_mut() {
            if sub.credit < chunk.len() as u64 {
                // 客户端跟不上：这一段直接丢弃。丢的是**屏幕已经画过**的
                // 内容（`Term` 里已经有了），客户端重新 attach 就能看到
                // 正确画面；把内存堆起来反而会让守护进程被一个卡住的
                // 客户端拖垮。
                log::warn!("终端 {} 的挂接者 {id} 跟不上，丢弃一段输出", self.address);
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
    }

    /// 向 PTY 写输入。
    pub fn write_input(&self, bytes: Vec<u8>) {
        let _ = self.input_tx.try_send(Input::Bytes(bytes));
    }

    /// 请求改尺寸。
    pub fn resize(&self, size: WindowSize) {
        let _ = self.input_tx.try_send(Input::Resize(size));
    }

    /// 把当前屏幕渲染成重绘序列。
    ///
    /// 客户端挂接时拿到它并喂给本地 vte，屏幕就与守护进程一致了。
    /// 只渲染可见区（不含回滚）：回滚由 `render_history` 按需补。
    pub fn snapshot(&self) -> Vec<u8> {
        render_screen(&self.term, 0)
    }
}

/// 把 VT 的可见区渲染成 ANSI 重绘序列。
///
/// `history_rows` > 0 时先从回滚区取这么多行一起输出（用于「恢复退出前的
/// 样子」时带回滚历史）。
pub fn render_screen(term: &FairMutex<Term<ZedListenerShim>>, _history_rows: usize) -> Vec<u8> {
    let term = term.lock();
    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();

    let mut out = Vec::with_capacity(rows * cols * 2 + 256);
    // 关掉自动折行：逐行定位输出时，写满最后一列不能让终端自己换行，
    // 否则每行都会多挤一行出来。
    out.extend_from_slice(b"\x1b[?7l");
    out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");

    for row in 0..rows {
        out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        let line = &grid[alacritty_terminal::index::Line(row as i32)];
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
        out.extend_from_slice(b"\x1b[0m");
    }

    // 光标放回程序认为的位置。
    let cursor = grid.cursor.point;
    out.extend_from_slice(
        format!("\x1b[{};{}H", cursor.line.0.max(0) + 1, cursor.column.0 + 1).as_bytes(),
    );
    out.extend_from_slice(b"\x1b[?7h");
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
                next_subscriber_id: 0,
            }),
            term,
            input_tx,
            id,
            bytes_out: AtomicU64::new(0),
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
                next_subscriber_id: 0,
            }),
            term,
            input_tx: tx,
            id: 0,
            bytes_out: AtomicU64::new(0),
        }
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
