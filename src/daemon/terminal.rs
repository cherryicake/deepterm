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

use super::protocol::Frame;

/// 终端重连时最多回放多少行回滚。
///
/// 与设计文档（`docs/DESIGN_TERMINAL_PERSISTENCE.md`）承诺的 2000 行一致，
/// 也与 herdr 的 `history: 2000` 同量级。上限在守护进程这一侧强制：客户端
/// 报再大也只回放这么多，否则一个跑了几小时的 `make` 会把快照撑到几十 MB，
/// attach 直接变成一次大传输。
pub const MAX_ATTACH_HISTORY: usize = 2000;

/// 重连时回放 kitty 图像的总预算（字节，命令文本）。
///
/// 图像重放是"锦上添花"：超预算就少恢复几张，绝不能因为图多把 attach 拖成
/// 秒级。8 MiB 足够覆盖终端里同时可见的十几张截图。
const KITTY_REPLAY_BUDGET: usize = 8 * 1024 * 1024;

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
    /// 输入到达时用来立刻唤醒终端读线程。
    ///
    /// # 为什么需要它
    ///
    /// 读线程的主体是 `poller.wait(events, timeout)` + 把 `input_rx` 里的
    /// 字节写进 PTY。而 `write_input` 只是往通道里 `try_send` 一下——**它
    /// 不会唤醒正在 poll 的那条线程**。于是输入必须等 `wait` 自己超时才被
    /// 处理，而空闲终端的超时是 100ms。
    ///
    /// 实测代价：一条"输入就立刻 echo 一行"的极简程序，端到端中位延迟
    /// **67ms**，与输出大小无关——纯粹是被 poll 节拍量化了。滚轮每次都是
    /// 几十毫秒的延迟，手感就是"卡"。
    ///
    /// `polling::Poller::notify()` 就是为这个场景设计的：唤醒 `wait` 而
    /// 不产生任何事件。跨平台，也不用自己维护 self-pipe。
    waker: Option<Arc<polling::Poller>>,
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
    /// 守护进程侧的 kitty 图像存储。
    ///
    /// 与客户端各存一份：客户端那份只服务于**当前这次**挂接（窗口关掉就
    /// 没了），而程序以为图还在。守护进程必须自己留一份，attach 时才能
    /// 把「图像 + 放置」重新编码回放（见 `kitty::replay`）。
    ///
    /// 由终端线程里的扫描器写入（同一份 `Arc`），由 `snapshot` 读取。
    kitty: crate::terminal::kitty::exec::SharedStorage,
    /// 协议是否启用（`limit_bytes == 0` 时关闭）。与 `kitty` 一起构造。
    kitty_enabled: bool,
    /// shell 自己的进程组 id（前台等于它说明没有程序在跑）。
    ///
    /// 由 `registry::spawn` 在建 PTY 后填入。`tcgetpgrp` 要与它比对才能
    /// 区分"前台是 shell"和"前台是 opencode"。
    shell_pgid: Option<i32>,
    /// 当前的前台进程快照（见 [`TerminalMeta`]）。
    ///
    /// # 为什么要在 handle 上留一份
    ///
    /// 元信息是**状态**，不是事件：新挂接者在 attach 之前不知道任何东西，
    /// 而「只在变化时推送」意味着它可以永远等不到第一帧——用户重开窗口后
    /// 标签就一直是 `bash`，正是要修的那个 bug。
    ///
    /// 所以这里存最新一份，`subscribe` 时对**新**订阅者补发。
    meta: Mutex<TerminalMeta>,
    /// 这个终端的订阅者与流控。
    ///
    /// 流控与投递的全部状态都在这里（见 `daemon::pump` 的模块文档）：
    /// 每条连接只持有一个 id，不再各自维护 credit / repaint 标记。
    hub: Arc<super::pump::Hub>,
}

/// 写入 PTY 的请求。
pub enum Input {
    /// 用户输入 / 协议应答。由写线程取走并写进 PTY。
    ///
    /// 为什么是"写线程"而不是读线程直接写：见 [`WriteRequest`] 的说明。
    Bytes(Vec<u8>),
    Resize(WindowSize),
    Shutdown,
}

/// 输入写线程与读线程之间的关停/交接约定。
///
/// 读线程只负责 `read` + 广播；所有往 PTY 的写入都投进 [`Input::Bytes`]，
/// 由独立的写线程消费。两侧用有界通道交接，写线程堵住时读线程**永远**
/// 不会被一起拖住——这正是过去"滚动一下界面冻死"的根因。
pub type InputSender = async_channel::Sender<Input>;

struct TerminalState {
    state: State,
    exit_code: Option<i32>,
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
        let id = self.hub.subscribe(channel, tx.clone());
        // 新订阅者没听过任何一次「变化」，必须先把当前快照给它一份。
        //
        // 顺序上它是订阅之后的第一帧，早于任何增量输出——标签先有值，
        // 画面随后才到，不会出现「先看到 bash 再跳成 opencode」。
        let meta = self
            .meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Ok(payload) = serde_json::to_vec(&meta) {
            let _ = tx.try_send(Frame::new(channel, super::protocol::Kind::Meta, payload));
        }
        // **已结束的终端要立刻收到 `Close`。**
        //
        // `mark_finished` 只通知**当时**在册的订阅者（`Hub::finish` 之后还会
        // 清空列表），此后再挂接的人永远等不到那条 Close。而记录会留在
        // registry 里直到下一次 `reap`（30 秒），这段时间内 attach 它是
        // 完全合法的请求。
        //
        // 不补这一帧的后果：客户端建出一个终端视图、拿到 `state: finished`
        // 却不做任何处理（`session::attach_with_history` 只读 `channel`），
        // 于是面板一直挂着这个已退出的终端，`list`/`get` 里也一直列着它。
        if self.state() == State::Finished {
            let _ = tx.try_send(Frame::new(
                channel,
                super::protocol::Kind::Close,
                b"{}".to_vec(),
            ));
        }
        id
    }

    /// 移除挂接者。
    pub fn unsubscribe(&self, id: u64) {
        self.hub.unsubscribe(id);
    }

    /// 补发流控信用；欠着整屏重绘的挂接者会在这里被补上。
    pub fn grant_credit(&self, id: u64, bytes: u64) {
        let term = self.term.clone();
        self.hub
            .grant_credit(id, bytes, move || render_screen(&term, 0));
    }

    /// 把一块输出分发给所有挂接者。
    pub fn broadcast(&self, chunk: &[u8]) {
        // 累计产出量：`get`/`list` 都把它报给客户端做活动判定。不在这里加，
        // 这个字段就永远停在 0（看起来像"这个终端从没输出过"）。
        self.bytes_out
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        let term = self.term.clone();
        self.hub.broadcast(chunk, move || render_screen(&term, 0));
    }

    /// 向 PTY 写输入。
    ///
    /// 投递之后**立刻唤醒读线程**：否则这条输入要等 `poller.wait` 自己
    /// 超时（空闲时 100ms）才会被搬进 PTY，滚轮/按键就会被量化成明显的
    /// 卡顿。见 `waker` 字段的说明。
    pub fn write_input(&self, bytes: Vec<u8>) {
        if self.input_tx.try_send(Input::Bytes(bytes)).is_ok() {
            self.wake();
        }
    }

    /// 唤醒本终端的读线程，让它马上处理刚投递的请求。
    fn wake(&self) {
        if let Some(waker) = &self.waker {
            // 失败只能说明 poller 已关闭（终端正在退出），忽略即可。
            let _ = waker.notify();
        }
    }

    /// 结束这个终端里的程序（用户显式关闭标签/面板时用）。
    ///
    /// 与"视图销毁"是两件事：视图销毁只是不再观看，程序该继续跑；
    /// 这个是用户明确说"我不要它了"。两者混在一起会导致要么关标签杀不掉
    /// 程序（泄漏），要么关窗口把用户的活全杀了。
    pub fn request_close(&self) {
        if self.input_tx.try_send(Input::Shutdown).is_ok() {
            self.wake();
        }
    }

    /// 请求改尺寸。
    pub fn resize(&self, size: WindowSize) {
        if self.input_tx.try_send(Input::Resize(size)).is_ok() {
            self.wake();
        }
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
        // 没有挂接者就不必付出整屏序列化的代价（resize 在拖动窗口时是
        // 连续事件，而多数终端此刻可能根本没人在看）。
        if self.hub.is_empty() {
            return;
        }
        let term = self.term.clone();
        self.hub.repaint_all(move || render_screen(&term, 0));
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

    /// 挂接时的完整重放序列：回滚历史 + 当前屏幕 + kitty 图像。
    ///
    /// 与 [`Self::snapshot`] 的区别只在"重连要不要看到断开前的历史"：
    /// - `snapshot()` 是**重绘**用的（resize 后对齐画面），必须只含可见区，
    ///   否则每拖一下窗口就会往客户端的回滚区里堆一份历史；
    /// - 这个是**挂接**用的，用户重开窗口时期望看到滚动条能拉上去、pager
    ///   退出前的输出还在，所以带上回滚。
    ///
    /// `replay_images = false` 时跳过图像回放：原地重连的客户端进程还活着，
    /// 它那份图像存储也还在，重传只会把几 MB 像素再过一遍 socket。
    ///
    /// 图像回放放在文字**之后**：`a=p` 的普通放置靠光标定位，而文字重绘
    /// 会把光标放到程序当前的位置；先摆图再画字会被随后的逐行定位覆盖掉。
    pub fn attach_paint(&self, history_rows: usize, replay_images: bool) -> Vec<u8> {
        let history_rows = history_rows.min(MAX_ATTACH_HISTORY);
        let mut out = render_screen(&self.term, history_rows);
        if !replay_images {
            return out;
        }

        // 行数取当前视口：回放只恢复看得见的放置（回滚区里的图需要把
        // 负行号换成"历史第几行"，收益不抵复杂度，见 replay 模块文档）。
        let rows = {
            let term = self.term.lock();
            term.grid().screen_lines()
        };
        let storage = crate::terminal::kitty::storage::lock_unpoisoned(&self.kitty);
        let replay = crate::terminal::kitty::replay::encode(&storage, rows, KITTY_REPLAY_BUDGET);
        if !replay.bytes.is_empty() {
            log::debug!(
                "终端 {} 重放 {} 张图像 / {} 个放置（跳过 {}），{} 字节",
                self.address,
                replay.images,
                replay.placements,
                replay.skipped,
                replay.bytes.len()
            );
            out.extend_from_slice(&replay.bytes);
            // 回放结束后把光标放回程序当前位置：`\x1b8` 恢复的是回放开始前
            // 的位置，而文字重绘最后已经把光标设成了真实的那个。
            let cursor = {
                let term = self.term.lock();
                term.grid().cursor.point
            };
            out.extend_from_slice(
                format!("\x1b[{};{}H", cursor.line.0.max(0) + 1, cursor.column.0 + 1).as_bytes(),
            );
        }
        out
    }
}

/// 终端元信息：Tab 标签需要、但客户端自己算不出来的那些。
///
/// 挂接式终端的 PTY 在守护进程里，客户端**没有 fd**，因此：
/// - `tcgetpgrp` 拿不到前台进程组 → 不知道自己该显示 `opencode` 还是 `bash`；
/// - 也就没法画对应的程序图标。
///
/// 这两件事只有守护进程能算，所以它探完用 [`Kind::Meta`] 推过来。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalMeta {
    /// 前台进程名（`None` = 前台就是 shell）。
    pub foreground: Option<String>,
    /// 前台进程的完整 argv，供 agent 识别与图标匹配。
    pub argv: Option<Vec<String>>,
}

/// 探测一个终端的前台进程。
///
/// 返回值把三种情况分开，**不能压成一个 `TerminalMeta`**：
///
/// - `Some(meta)` 且 `foreground` 为 `None`：前台确实是 shell，没程序在跑
///   ——这是**有效结论**，标签该显示 shell 名；
/// - `Some(meta)` 且 `foreground` 为 `Some(_)`：有个程序在前台；
/// - `None`：**查不了**（`tcgetpgrp` 失败、进程刚退、读不到 argv）。
///   这不是结论——调用方必须保持上一份快照，否则每次短命的子进程退出
///   都会让标签闪回 `bash` 再跳回来（本地探测的
///   [`crate::terminal::LocalAgentOutcome::Unavailable`] 是同一个道理）。
///
/// 只在 unix 上有实现（`tcgetpgrp` 需要 master fd）；其它平台恒为 `None`
/// ——那些平台的 ConPTY 没有「前台进程组」这个概念。
#[cfg(unix)]
pub fn probe_terminal_meta(
    master_fd: Option<i32>,
    handle: &TerminalHandle,
) -> Option<TerminalMeta> {
    let fd = master_fd?;
    // SAFETY：`tcgetpgrp` 只读查询，fd 由调用方保证是存活的 PTY master。
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    if pgid < 0 {
        return None;
    }
    // 前台就是 shell 自己：标签该显示 shell 名，不算"在跑程序"。
    if handle.shell_pgid() == Some(pgid) {
        return Some(TerminalMeta::default());
    }
    // 一次读到 argv，名字从同一份里取——**不要读两遍**。
    //
    // 两次读之间进程可能已经 exec 成别的程序，于是名字说 `opencode`、
    // argv 说 `bash`；标签的文字与图标就会自相矛盾。这条路径每 500ms
    // 跑一遍，重复读的代价也是白花的。
    let Some(argv) = crate::terminal::foreground_argv::foreground_argv(pgid as u32) else {
        // 进程刚退 / 读不到：**不是**「前台就是 shell」这个结论。
        return None;
    };
    let foreground = argv.first().map(|argv0| {
        let base = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
        base.trim_start_matches('-').to_string()
    });
    Some(TerminalMeta {
        foreground,
        argv: Some(argv),
    })
}

#[cfg(not(unix))]
pub fn probe_terminal_meta(
    _master_fd: Option<i32>,
    _handle: &TerminalHandle,
) -> Option<TerminalMeta> {
    None
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

    // ── ⓪ 先把客户端从"可能卡住的解析状态"里救出来 ──────────────────────
    //
    // 这两条的**顺序不能反**，理由都很具体：
    //
    // **第一，`CSI ?2026l` 必须最前。**
    // 客户端可能停在同步更新里（`?2026h` 之后没等到 `?2026l`）。那正是
    // TUI 滚动的常态：opencode 每次滚轮重绘会发 36 个 `?2026h…?2026l`
    // 块，我们的流控在此刻丢一段、正好丢掉收尾的 ESU，客户端就永远出不
    // 来了。vte 在同步块里**不解析任何字节**，只把它们囤进 sync 缓冲，
    // 而且它的 `StdSyncHandler::pending_timeout()` 判的是"超时是否被设置"
    // 而不是"是否已到期"——**这个超时永远不会自己结束**。唯一的出口是
    // 缓冲里出现 ESU，或缓冲涨到 2 MiB（`SYNC_BUFFER_SIZE`）被动溢出。
    // 所以用户看到的是：界面完全不动、后续增量也全看不见，最多几十秒后
    // 突然跳一下。必须先发 `?2026l` 把积压冲出来。
    //
    // **第二，`CAN`(0x18) 必须紧随其后。**
    // 上面那次冲刷会把旧的半帧内容重新解析一遍，而它很可能**切在转义序列
    // 中间**（丢帧是按字节量丢的）。解析器于是停在 CsiParam 之类的状态，
    // 吃掉我们随后的字节：`ESC[3` + `ESC[!p` 会被拼成一条非法序列，
    // 整条被吞，重绘就错位/残缺——这就是用户说的"内容界面错乱"。
    // vte 的 `anywhere` 里只有 `CAN`/`SUB` 会无条件把状态机拉回 Ground
    //（`ESC` 只是进入 Escape，救不了半截参数）。herdr 的
    // `paint_state`（`viewers.rs:496`）出于同样理由以 `vec![0x18]` 开头。
    //
    // 放在冲刷**之后**也是必须的：放在之前，CAN 只会跟着一起进 sync 缓冲，
    // 冲刷时半截序列照样会污染后面的字节。
    out.extend_from_slice(b"\x1b[?2026l\x18");

    // ── ① 再把客户端拉回一个干净、已知的状态 ────────────────────────────
    //
    // `CSI ! p`（软重置）复位笔、边距、字符集与滚动区，但**不动**鼠标、
    // 焦点、粘贴、键盘这些私有模式——所以要显式关一遍。
    out.extend_from_slice(b"\x1b[!p");
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
    // **只在样式变化时生成 SGR。**
    //
    // 这曾经是挂接/重绘的头号热点：旧实现每格都调一次 `sgr_for`，而它返回
    // `String` —— 一屏 181×43 就是近 8000 次堆分配。采样证明它有多贵：
    // holder 的连接线程 33% 的时间在 `render_screen`，其中大半是
    // `format_inner`/`String::write_str`（见 `daemon/terminal.rs` 的历史）。
    //
    // 而真实屏幕上样式是成片重复的（一段正文一个颜色），比较
    // `(fg, bg, flags)` 三元组即可，命中时零分配。同时把 `current` 从
    // `String` 换成这个可 Copy 的三元组——旧写法还要为每次比较复制字符串。
    //
    // 初值刻意用一个"不会与真实单元格相等"的哨兵：`Color` 没有"无"这个
    // 取值，所以用 `Option` 包一层，第一格必然触发一次 SGR（行的起始状态
    // 必须是明确的，不能继承上一行）。
    let mut current: Option<(
        alacritty_terminal::vte::ansi::Color,
        alacritty_terminal::vte::ansi::Color,
        alacritty_terminal::term::cell::Flags,
    )> = None;
    for col in 0..trailing_blank {
        let cell = &line[alacritty_terminal::index::Column(col)];
        if current != Some((cell.fg, cell.bg, cell.flags)) {
            sgr_into(out, &cell.fg, &cell.bg, cell.flags);
            current = Some((cell.fg, cell.bg, cell.flags));
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

/// 把一格的 SGR 直接写进输出缓冲，**不分配**。
///
/// 旧实现返回 `String`，于是每格一次堆分配 + 一次 `format!`。挂接/重绘
/// 一次要遍历整屏（181×43 ≈ 7800 格），在连接线程上就是毫秒级的纯开销，
/// 而那时滚轮输入正等着被读——分配器抖动直接变成"滚动卡顿"。
fn sgr_into(
    out: &mut Vec<u8>,
    fg: &alacritty_terminal::vte::ansi::Color,
    bg: &alacritty_terminal::vte::ansi::Color,
    flags: alacritty_terminal::term::cell::Flags,
) {
    use alacritty_terminal::term::cell::Flags;

    out.extend_from_slice(b"\x1b[0");
    for (flag, code) in [
        (Flags::BOLD, ";1"),
        (Flags::DIM, ";2"),
        (Flags::ITALIC, ";3"),
        (Flags::UNDERLINE, ";4"),
        (Flags::INVERSE, ";7"),
        (Flags::HIDDEN, ";8"),
        (Flags::STRIKEOUT, ";9"),
    ] {
        if flags.contains(flag) {
            out.extend_from_slice(code.as_bytes());
        }
    }
    match fg {
        Color::Named(n) => push_named_sgr(out, *n, false),
        Color::Spec(rgb) => {
            out.extend_from_slice(b";38;2;");
            push_u8(out, rgb.r);
            out.push(b';');
            push_u8(out, rgb.g);
            out.push(b';');
            push_u8(out, rgb.b);
        }
        Color::Indexed(i) => {
            out.extend_from_slice(b";38;5;");
            push_u8(out, *i);
        }
    }
    match bg {
        Color::Named(n) => push_named_sgr(out, *n, true),
        Color::Spec(rgb) => {
            out.extend_from_slice(b";48;2;");
            push_u8(out, rgb.r);
            out.push(b';');
            push_u8(out, rgb.g);
            out.push(b';');
            push_u8(out, rgb.b);
        }
        Color::Indexed(i) => {
            out.extend_from_slice(b";48;5;");
            push_u8(out, *i);
        }
    }
    out.push(b'm');
}

/// 十进制写一个 `u8`，不分配。最多三位，够用且比 `format!` 快一个数量级。
fn push_u8(out: &mut Vec<u8>, value: u8) {
    if value >= 100 {
        out.push(b'0' + value / 100);
    }
    if value >= 10 {
        out.push(b'0' + (value / 10) % 10);
    }
    out.push(b'0' + value % 10);
}

fn push_named_sgr(
    out: &mut Vec<u8>,
    n: alacritty_terminal::vte::ansi::NamedColor,
    background: bool,
) {
    let code = named_sgr_code(n, background);
    out.push(b';');
    push_u8(out, code);
}

/// 命名色 → SGR 编号。
fn named_sgr_code(n: alacritty_terminal::vte::ansi::NamedColor, background: bool) -> u8 {
    use alacritty_terminal::vte::ansi::NamedColor as N;
    let base = if background { 40 } else { 30 };
    match n {
        N::Black => base,
        N::Red => base + 1,
        N::Green => base + 2,
        N::Yellow => base + 3,
        N::Blue => base + 4,
        N::Magenta => base + 5,
        N::Cyan => base + 6,
        N::White => base + 7,
        N::BrightBlack => base + 60,
        N::BrightRed => base + 61,
        N::BrightGreen => base + 62,
        N::BrightYellow => base + 63,
        N::BrightBlue => base + 64,
        N::BrightMagenta => base + 65,
        N::BrightCyan => base + 66,
        N::BrightWhite => base + 67,
        // 前景/背景/光标/选区等"语义色"落到终端默认：重绘时不保留颜色，
        // 由客户端按自己的主题渲染。herdr 的快照也是这个策略——快照存的是
        // 逻辑色而不是绝对 RGB，换主题后历史内容会跟着变色。
        _ => {
            if background {
                49
            } else {
                39
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
        {
            let mut st = self.state.lock().expect("terminal state");
            st.state = State::Finished;
            st.exit_code = exit_code;
        }
        // 通知所有订阅者"程序结束了"，然后释放它们的通道。
        self.hub.finish();
    }

    /// 构造一个可用的终端句柄。
    ///
    /// 参数较多但都是"创建这个终端所必需、且创建后不再变化"的东西；
    /// 再包一层 builder 只是把同一批字段搬来搬去，不值得。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Address,
        argv: Vec<String>,
        cwd: Option<String>,
        created_ms: u64,
        term: Arc<FairMutex<Term<ZedListenerShim>>>,
        input_tx: async_channel::Sender<Input>,
        id: u64,
        kitty: crate::terminal::kitty::exec::SharedStorage,
        kitty_enabled: bool,
        shell_pgid: Option<i32>,
        waker: Option<Arc<polling::Poller>>,
    ) -> Self {
        Self {
            address,
            argv,
            cwd,
            created_ms,
            state: Mutex::new(TerminalState {
                state: State::Live,
                exit_code: None,
            }),
            term,
            input_tx,
            id,
            bytes_out: AtomicU64::new(0),
            repaint_pending: std::sync::atomic::AtomicBool::new(false),
            kitty,
            kitty_enabled,
            shell_pgid,
            meta: Mutex::new(TerminalMeta::default()),
            hub: Arc::new(super::pump::Hub::new()),
            waker,
        }
    }

    /// shell 自己的进程组 id（见 [`Self::shell_pgid`]）。
    pub fn shell_pgid(&self) -> Option<i32> {
        self.shell_pgid
    }

    /// 把元信息推给所有挂接者。
    ///
    /// 元信息不进流控：它很小（几十字节）且低频（只在变化时发）。走 `Meta`
    /// 帧而不是混进终端数据流，客户端才能把它当成"标签状态"而不是屏幕内容。
    pub fn broadcast_meta(&self, meta: &TerminalMeta) {
        let payload = match serde_json::to_vec(meta) {
            Ok(p) => p,
            Err(_) => return,
        };
        // 先存快照再广播：后到的订阅者在 `subscribe` 里读它。
        *self.meta.lock().unwrap_or_else(|e| e.into_inner()) = meta.clone();
        self.hub.meta(&payload);
    }

    /// 共享的 kitty 存储（终端线程里的扫描器要用同一个）。
    pub fn kitty_storage(&self) -> crate::terminal::kitty::exec::SharedStorage {
        self.kitty.clone()
    }

    /// 协议是否启用（与存储一起交给扫描器）。
    pub fn kitty_enabled(&self) -> bool {
        self.kitty_enabled
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
            }),
            term,
            input_tx: tx,
            waker: None,
            id: 0,
            bytes_out: AtomicU64::new(0),
            repaint_pending: std::sync::atomic::AtomicBool::new(false),
            kitty: std::sync::Arc::new(std::sync::Mutex::new(
                crate::terminal::kitty::storage::Storage::new(1 << 20),
            )),
            kitty_enabled: true,
            shell_pgid: None,
            meta: Mutex::new(TerminalMeta::default()),
            hub: Arc::new(super::pump::Hub::new()),
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

    /// 测试用：把一段字节喂给 kitty 扫描器（不经过 PTY）。
    ///
    /// 生产路径里这由终端读线程完成；测试要验证"挂接能重放出图像"，
    /// 只需要让共享存储里有这张图。
    #[cfg(test)]
    pub fn scan_kitty_for_tests(&self, bytes: &[u8]) {
        use crate::terminal::escape_scan::PtyScanner as _;
        let mut scanner = crate::terminal::kitty::scanner::KittyGraphicsScanner::with_storage(
            self.kitty_storage(),
            self.kitty_enabled(),
            Box::new(crate::terminal::kitty::scanner::CollectingSink::default()),
        );
        let mut pending = Vec::new();
        let mut emit = |_: &[u8]| {};
        let mut grid = || crate::terminal::escape_scan::GridState {
            cursor: (0, 0),
            visible: crate::terminal::kitty::storage::VisibleRows::new(0, 4),
        };
        let mut move_cursor = |_: u32, _: u32| {};
        scanner.scan(bytes, &mut pending, &mut emit, &mut grid, &mut move_cursor);
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

    /// **重绘序列必须能把卡在同步更新/半截序列里的客户端救出来。**
    ///
    /// 回归价值：这是"滚动 TUI 界面完全卡死 + 内容错乱"的直接成因。
    ///
    /// - opencode 每次滚轮重绘发 36 个 `?2026h…?2026l` 块；我们的流控一旦
    ///   丢掉含 ESU 的那一段，客户端就停在同步更新里。vte 在同步块内**不解析
    ///   任何字节**，而且它的 `pending_timeout()` 判的是"超时是否被设置"而
    ///   不是"是否到期"——那个超时永远不会自己结束，只能靠缓冲涨到 2 MiB
    ///   被动溢出。用户看到的就是界面完全不动。
    /// - 丢帧是按字节量丢的，可能正好切在 `ESC[3` 与 `81m` 中间。之后补发的
    ///   快照若不先复位，解析器会把 `ESC[3` + `ESC[!p` 拼成一条非法序列吞掉，
    ///   于是画面错位。
    ///
    /// 所以这段序列必须满足三条，顺序也有讲究：先 `?2026l`（把同步块里的
    /// 积压冲出来），再 `CAN`（把冲刷后可能残留的半截序列拉回 Ground），
    /// 然后才是软重置/清屏。
    #[test]
    fn a_repaint_can_rescue_a_client_stuck_mid_sequence() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.feed_for_tests(b"hello");
        let paint = handle.snapshot();

        assert!(
            paint.starts_with(b"\x1b[?2026l\x18"),
            "重绘必须以 ?2026l + CAN 开头，否则卡住的客户端救不回来: {:?}",
            String::from_utf8_lossy(&paint[..paint.len().min(40)])
        );
        // 顺序：先结束同步更新，再复位解析器。
        let esu = paint
            .windows(8)
            .position(|w| w == b"\x1b[?2026l")
            .expect("要有 ?2026l");
        let can = paint.iter().position(|b| *b == 0x18).expect("要有 CAN");
        assert!(
            esu < can,
            "CAN 必须在 ?2026l 之后：冲刷后残留的半截序列才需要复位"
        );
    }

    /// **真的能把两种坏状态救回来**（不只是断言字节顺序）。
    ///
    /// 用一个真实 `Term` + `Processor` 扮演客户端：先喂半帧把它卡住，再喂
    /// 重绘序列，断言画面恢复成快照内容而不是错位/空白。
    #[test]
    fn the_repaint_recovers_both_stuck_states() {
        use alacritty_terminal::vte::ansi::Processor;

        // ① 卡在同步更新里：后续字节全被囤住，屏幕停住。
        {
            let source = test_term();
            let handle = TerminalHandle::new_for_tests("t1", source);
            handle.feed_for_tests(b"BEFORE\r\nAFTER");
            let paint = handle.snapshot();

            let client = test_term();
            let mut processor = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
            {
                let mut t = client.lock();
                // 半帧：只有 BSU，ESU 在"丢失的那一段"里。
                processor.advance(&mut *t, b"\x1b[?2026hSTUCK-FRAME");
                assert!(processor.sync_bytes_count() > 0, "应停在同步更新里");
                // 客户端此刻还看不到任何东西（vte 不解析同步块内容）。
                assert!(!screen_text(&t).contains("STUCK"));
            }
            // 补发重绘。
            {
                let mut t = client.lock();
                processor.advance(&mut *t, &paint);
                let text = screen_text(&t);
                assert!(
                    text.contains("AFTER"),
                    "重绘必须把卡在同步更新里的客户端救回来: {text:?}"
                );
                assert!(
                    processor.sync_bytes_count() == 0,
                    "重绘之后不应还留在同步更新里（还剩 {} 字节）",
                    processor.sync_bytes_count()
                );
            }
        }

        // ② 卡在半截 CSI 里：参数与后续字节拼在一起。
        {
            let source = test_term();
            let handle = TerminalHandle::new_for_tests("t1", source);
            handle.feed_for_tests(b"AFTER");
            let paint = handle.snapshot();

            let client = test_term();
            let mut processor = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
            {
                let mut t = client.lock();
                processor.advance(&mut *t, b"\x1b[3"); // 参数"没写完"
            }
            {
                let mut t = client.lock();
                processor.advance(&mut *t, &paint);
                let text = screen_text(&t);
                assert!(
                    text.contains("AFTER"),
                    "重绘必须能把卡在半截转义里的客户端救回来: {text:?}"
                );
            }
        }
    }

    /// 测试用：把整屏文字拼出来（不含样式）。
    fn screen_text(term: &Term<ZedListenerShim>) -> String {
        let grid = term.grid();
        let mut out = String::new();
        for row in 0..grid.screen_lines() {
            for col in 0..grid.columns() {
                out.push(
                    grid[alacritty_terminal::index::Line(row as i32)]
                        [alacritty_terminal::index::Column(col)]
                    .c,
                );
            }
            out.push('\n');
        }
        out
    }

    /// **挂接要带回滚，重绘不带。**
    ///
    /// 回归价值：`snapshot()` 一直传 `history_rows = 0`，于是"关掉窗口再
    /// 打开"只能看到最后一屏，`less` 退出前的输出、编译日志全都丢了——
    /// 而设计文档明确承诺重开窗口能看到回滚。这里把两条路径的差异钉死：
    /// 挂接（`attach_paint`）带回滚，重绘（`snapshot`）不带。
    ///
    /// 回滚不能出现在重绘里的理由很具体：resize 时重绘会被反复触发，
    /// 每触发一次就往客户端的回滚区里再堆一份历史。
    #[test]
    fn attach_paint_replays_history_but_repaint_does_not() {
        let term = test_term(); // 20x4
        let handle = TerminalHandle::new_for_tests("t1", term);
        // 推足够多的行，把最早的内容挤出可见区、进入回滚区。
        for i in 0..40 {
            handle.feed_for_tests(format!("line-{i}\r\n").as_bytes());
        }

        let repaint = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        assert!(
            !repaint.contains("line-0"),
            "重绘不该带历史，否则每次 resize 都会往客户端回滚区里再堆一份: {repaint:?}"
        );

        let paint = String::from_utf8_lossy(&handle.attach_paint(2000, false)).into_owned();
        assert!(
            paint.contains("line-0"),
            "挂接必须回滚到最早的内容，否则重开窗口只看得到最后一屏: {paint:?}"
        );
        // 顺序：历史在前、当前屏幕在后。
        let history = paint.find("line-0").expect("要有历史");
        let visible = paint.find("line-39").expect("要有当前屏");
        assert!(history < visible, "历史应当排在当前屏幕之前");
    }

    /// `history = 0` 时挂接与重绘完全一致：测试与"不要历史"的调用方
    /// 不该被强制多收一份回滚。
    #[test]
    fn attach_paint_without_history_matches_the_repaint() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        for i in 0..20 {
            handle.feed_for_tests(format!("row-{i}\r\n").as_bytes());
        }
        assert_eq!(handle.attach_paint(0, false), handle.snapshot());
    }

    /// **挂接要重放 kitty 图像。**
    ///
    /// 回归价值：守护进程过去完全不懂图形协议，窗口关掉后客户端的图像存储
    /// 随之消失，重开窗口时截图/图表变成空白，而程序以为自己还显示着。
    /// 现在守护进程自己扫描 APC 并留一份存储，挂接时重新编码回去。
    #[test]
    fn attach_paint_replays_kitty_images() {
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);

        // 一条 2x2 RGBA 图像 + 在光标处显示（`a=T` 传完就摆）。
        // 像素数据全 0 不合法也没关系：解码只校验长度。
        let payload = {
            use crate::terminal::kitty::command::encode_base64;
            encode_base64(&[0u8; 16])
        };
        let apc = format!("\x1b_Ga=T,f=32,s=2,v=2,i=9,q=2;{payload}\x1b\\");
        handle.scan_kitty_for_tests(apc.as_bytes());

        let paint = String::from_utf8_lossy(&handle.attach_paint(0, true)).into_owned();
        assert!(
            paint.contains("a=t,f=32") && paint.contains("i=9"),
            "挂接必须重新传一遍像素，否则重开窗口后图像消失: {paint:?}"
        );
        assert!(paint.contains("a=p,i=9"), "像素传完还要摆回原位: {paint:?}");
        // 重绘（resize）不带图像：客户端自己那份还在，重复传只会放大快照。
        let repaint = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        assert!(
            !repaint.contains("a=t,f=32"),
            "重绘不该重传图像（每次 resize 都传一遍会明显卡顿）"
        );
        // 原地重连也是同一个道理：客户端进程还活着，图像存储还在。
        let reconnect = String::from_utf8_lossy(&handle.attach_paint(0, false)).into_owned();
        assert!(
            !reconnect.contains("a=t,f=32"),
            "原地重连不该重传图像（几 MB 像素白过一遍 socket）"
        );
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
        use super::super::protocol::Kind;
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        let (tx, rx) = async_channel::bounded(4);
        handle.subscribe(1, tx);
        handle.mark_finished(Some(0));

        // 订阅时先补了一帧 Meta 快照（见 `a_late_subscriber_gets_the_current_meta_snapshot`），
        // 它与"程序退出"无关，这里要的是紧随其后的关闭帧。
        let snapshot = rx.try_recv().expect("订阅时应补一帧 meta 快照");
        assert_eq!(snapshot.kind, Kind::Meta);
        let frame = rx.try_recv().expect("订阅者应收到关闭帧");
        assert_eq!(frame.kind, Kind::Close);
        assert_eq!(handle.state(), State::Finished);
        assert_eq!(handle.exit_code(), Some(0));
    }

    /// **新挂接者必须立刻拿到一次 meta 快照。**
    ///
    /// 回归价值：这是"重启后 Tab 标签变成 bash、图标也跟着错"的根因之一。
    ///
    /// 元信息是**状态**而不是事件：只有变化时才广播，于是 attach 之后
    /// 才开始收数据的新订阅者可能永远等不到第一帧——前台明明是 opencode，
    /// 标签却一直显示 shell 名。所以 `broadcast_meta` 要存一份，
    /// `subscribe` 时对它补发。
    #[test]
    fn a_late_subscriber_gets_the_current_meta_snapshot() {
        use super::super::protocol::Kind;
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        let meta = TerminalMeta {
            foreground: Some("opencode".into()),
            argv: Some(vec!["opencode".into()]),
        };
        handle.broadcast_meta(&meta);

        // 广播之后才挂接：它没经历过任何一次"变化"。
        let (tx, rx) = async_channel::bounded(4);
        handle.subscribe(1, tx);
        let frame = rx.try_recv().expect("新订阅者必须收到当前 meta");
        assert_eq!(frame.kind, Kind::Meta, "必须是 Meta 帧，不能混成终端数据");
        let got: TerminalMeta = serde_json::from_slice(&frame.payload).expect("meta 应是合法 JSON");
        assert_eq!(got, meta);
    }

    /// **attach 一个已结束的终端时，必须立刻推 `Close`。**
    ///
    /// 回归价值：`mark_finished` 只通知**当时**在册的订阅者，之后
    /// `Hub::finish` 会把列表清空。此后再挂接的人永远等不到那条 Close——
    /// 而记录要留到下一次 `reap`（30 秒）才消失，这段时间内 attach 它是
    /// 完全合法的请求。
    ///
    /// 不补这一帧的后果：客户端建出一个终端视图，却不做任何处理
    /// （`session::attach_with_history` 只读 `channel`，不看 `state`），
    /// 面板一直挂着这个已退出的终端。
    #[test]
    fn subscribing_to_a_finished_terminal_immediately_reports_close() {
        use super::super::protocol::Kind;
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        handle.mark_finished(Some(0));

        let (tx, rx) = async_channel::bounded(4);
        handle.subscribe(1, tx);
        let kinds: Vec<Kind> = (0..2).filter_map(|_| rx.try_recv().ok().map(|f| f.kind)).collect();
        assert!(
            kinds.contains(&Kind::Close),
            "已结束的终端必须在订阅时补一帧 Close，否则客户端永远挂着它: {kinds:?}"
        );
    }

    /// 活着的终端**不该**收到 Close（否则面板会立刻消失）。
    #[test]
    fn subscribing_to_a_live_terminal_does_not_report_close() {
        use super::super::protocol::Kind;
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        let (tx, rx) = async_channel::bounded(4);
        handle.subscribe(1, tx);
        let kinds: Vec<Kind> = (0..2).filter_map(|_| rx.try_recv().ok().map(|f| f.kind)).collect();
        assert!(!kinds.contains(&Kind::Close), "活着的终端不该收到 Close: {kinds:?}");
    }

    /// 从未探测过时补发的是空快照（前台是 shell），而不是空白帧。
    #[test]
    fn a_late_subscriber_without_probe_gets_an_empty_snapshot() {
        use super::super::protocol::Kind;
        let term = test_term();
        let handle = TerminalHandle::new_for_tests("t1", term);
        let (tx, rx) = async_channel::bounded(4);
        handle.subscribe(1, tx);
        let frame = rx.try_recv().expect("仍应补一帧，表明前台是 shell");
        assert_eq!(frame.kind, Kind::Meta);
        assert_eq!(
            serde_json::from_slice::<TerminalMeta>(&frame.payload).expect("合法 JSON"),
            TerminalMeta::default()
        );
    }
}
