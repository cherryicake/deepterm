//! 本地 PTY 读线程。
//!
//! 替代 alacritty 的 `EventLoop`，让本地终端的字节流走与 SSH 相同的
//! 「应用层扫描 → vte 解析」管线，从而能拦截 APC 序列（Kitty 图形协议）。
//!
//! 为什么必须自建：上游 `alacritty_terminal` 的 `EventLoop` 在自有线程上
//! 直接解析字节，应用层完全看不到原始数据；而 `vte-0.15` 的 APC 状态
//! (`SosPmApcString`) 会把内容直接丢弃，`EventListener` 也没有对应事件。
//!
//! 线程模型与 alacritty 保持一致：**解析在读线程**（持有 `Term` 的 lease），
//! 因此不引入额外的 UI 跃点，`cat` 大文件类负载无吞吐回归。

use std::{
    io::{Read as _, Write as _},
    sync::Arc,
};

use alacritty_terminal::{
    event::{Event as AlacTermEvent, EventListener as _, OnResize, WindowSize},
    grid::Dimensions as _,
    sync::FairMutex,
    term::Term,
    tty::{ChildEvent, EventedPty, EventedReadWrite, Pty},
    vte::ansi::{Processor, StdSyncHandler},
};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use super::terminal::ZedListener;
use crate::terminal::escape_scan;

/// 单次 read 的缓冲上限（对齐 alacritty `event_loop.rs` 的 `READ_BUFFER_SIZE`）。
const READ_BUFFER_SIZE: usize = 0x10_0000;

/// 单次持锁解析的字节上限，用于保证解析不长时间阻塞其他线程
/// （对齐 alacritty 的 `MAX_LOCKED_READ`）。
const MAX_LOCKED_READ: usize = u16::MAX as usize;

/// PTY 读写 fd 的轮询 token。
///
/// 必须与 `alacritty_terminal::tty::unix` 内部的取值一致（0/1）：该 crate 的
/// `Pty::register` 会**覆盖**调用方传入的 `event.key`，因此这里不能自选值，
/// 否则事件匹配永远落空、本地终端将完全收不到输出。
const PTY_READ_WRITE_TOKEN: usize = 0;
/// 子进程 SIGCHLD 通知 fd 的轮询 token（同上，必须与上游一致）。
const PTY_CHILD_EVENT_TOKEN: usize = 1;

/// 应用层序列处理器。
///
/// 读线程把每次读到的字节交给它做旁路识别：需要应用层消费的序列
/// （OSC 133/7/9、Kitty 图形 APC）被摘除，其余写入 `out` 交给 vte。
///
/// 为什么是「重写输出缓冲」而不是「返回已消费长度」：被摘除的序列可能
/// 位于数据中段（多个 APC 夹着普通文本），单一边界无法表达这种裁剪。
///
/// 实现运行在读线程上，**不得**依赖 GPUI 的 `Context`；需要通知 UI 的
/// 事情通过 [`LocalNotice`] 回传。
/// 扫描器在处理一条 APC 时需要的网格状态快照。
///
/// 两者都随解析推进而变化（前面的文本可能移动光标、可能触发滚动），所以由
/// 调用方在处理序列的间隙**重新查询**，而不是先切完再一次性喂——否则同一个
/// read 里常见的 `CSI <定位> + APC <贴图>` 组合会把锚点算到旧位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridState {
    /// 当前光标（行、列）。
    pub cursor: (i32, u32),
    /// 当前可见的网格行窗口（`d=a` 的「屏幕上可见」判定）。
    pub visible: crate::terminal::kitty::storage::VisibleRows,
}

pub trait PtyScanner: Send {
    /// 处理一段字节。
    ///
    /// `pending` 为跨读残尾缓冲：未完成的序列尾部留在其中，与下次读到的
    /// 字节拼接后重新扫描。
    ///
    /// `emit` 用于把「序列之前的普通文本」按序交给 vte，`grid` 用于在
    /// 交出序列时查询当前网格状态（光标与可见窗口）。两者分两步（而非先切完
    /// 再一次性喂）是为了让 APC 的锚点精确反映它前面那些文本的效果——同一个
    /// read 里常见 `CSI <定位> + APC <贴图>` 的组合，若等整块文本都喂完才处理
    /// APC，锚点会落后一个 chunk。
    ///
    /// 为什么是回调而不是「返回区段列表」：文本与需消费序列是交替的，
    /// 回调天然表达顺序，且调用方无需为每次扫描分配区段向量。
    fn scan(
        &mut self,
        data: &[u8],
        pending: &mut Vec<u8>,
        emit: &mut dyn FnMut(&[u8]),
        grid: &mut dyn FnMut() -> GridState,
    );

    /// 取走扫描器攒下的、需要回写给终端应用的应答字节。
    ///
    /// 为什么由读线程回写而不是走 UI 线程：终端应用（如 kitty 图形协议的能力
    /// 探测）发出请求后会在**极短超时**内等待应答——terminal-browser 只等
    /// 500ms。若把应答经 `LocalNotice` 绕到 UI 线程再回写，必须依赖 UI 的重绘
    /// 泵及时被唤醒；而重绘泵的触发条件是关于 vte 字节吞吐的（见 `pty_read`
    /// 末尾），被扫描器**消费掉**的 APC 不产生任何 vte 字节，于是探测类负载
    /// 恰恰是最不保证能唤醒 UI 的那一种。读线程本来就独占 `Pty` 的写端，直接
    /// 回写既省一次跨线程跃点，也不依赖重绘节奏。
    ///
    /// 默认无应答（透传类扫描器用不到）。
    fn take_replies(&mut self) -> Vec<u8> {
        Vec::new()
    }

    /// 终端几何变更（行列数 + 单元格像素尺寸）。
    ///
    /// 读线程在收到 `LocalMsg::Resize` 时转发；`spawn` 时也会先注入一次初始
    /// 几何，使得终端应用在**任何 resize 之前**发出的尺寸查询
    /// （如 terminal-browser 启动时的 `CSI 16 t`）也能被正确应答。
    ///
    /// 默认空实现：不关心几何的扫描器（透传、OSC 过滤）无需实现。
    fn set_geometry(&mut self, geometry: escape_scan::TermGeometry) {
        let _ = geometry;
    }

    /// 接管 OSC 序列的去处。
    ///
    /// 由 `LocalReader::spawn` 在启动时注入（[`NoticeOscSink`]），因为扫描器
    /// 构造时还不知道通知通道长什么样——扫描器可能在 `Terminal::new` 的很早
    /// 阶段就被建好，而通道与读线程一起创建。
    ///
    /// 默认空实现：不解析 OSC 的扫描器（透传）无需实现；未实现则所有 OSC
    /// 原样交给 vte，行为与拦截之前完全一致。
    fn set_osc_sink(&mut self, sink: Box<dyn crate::terminal::kitty::scanner::OscSink>) {
        let _ = sink;
    }
}

/// 全量透传的扫描器：不做任何识别，字节原样交给 vte。
pub struct PassThroughScanner;

impl PtyScanner for PassThroughScanner {
    fn scan(
        &mut self,
        data: &[u8],
        _pending: &mut Vec<u8>,
        emit: &mut dyn FnMut(&[u8]),
        _grid: &mut dyn FnMut() -> GridState,
    ) {
        emit(data);
    }
}

/// 把摘到的 OSC 经 [`LocalNotice`] 转给 UI 线程的 [`OscSink`]。
///
/// # 为什么读线程只"通知"、不再决定"透传还是消费"
///
/// 判定一条 OSC 该不该由应用层消费，依赖 `Context`（发事件、弹通知）与
/// `Terminal` 自己的字段（cwd 去重、前台进程名），两者都只在 UI 线程上可用。
/// 所以判定只有一处：UI 线程的 `Terminal::try_handle_osc_sequence`。
///
/// 但**字节不必等判定**：扫描器把序列按原位置喂给 vte（见
/// [`crate::terminal::kitty::scanner::KittyGraphicsScanner::scan`]），这里只是
/// 旁路通知 UI 线程。两条路由认识的前缀互不重叠，所以同一序列"既进 vte、
/// 又通知 UI"不会被处理两次。
///
/// 若在这里就地判定"属不属于本层"，就必须把 `try_handle_osc_sequence`
/// 认识的全部前缀复制一份到读线程，两处清单必然漂移。
pub struct NoticeOscSink {
    notice: async_channel::Sender<LocalNotice>,
}

impl NoticeOscSink {
    pub fn new(notice: async_channel::Sender<LocalNotice>) -> Self {
        Self { notice }
    }
}

impl crate::terminal::kitty::scanner::OscSink for NoticeOscSink {
    fn osc(&mut self, content: &[u8], cursor: (i32, u32)) {
        // 通道满或 UI 已退出：丢一条通知。字节已按位置喂给 vte，所以标题 /
        // 超链接 / 调色板这些**由 vte 处理**的序列不受影响；只有应用层的那几种
        // （cwd 上报、133 标记、hook 事件）会少一次刷新，好过阻塞读线程。
        if self
            .notice
            .try_send(LocalNotice::Osc {
                content: content.to_vec(),
                cursor,
            })
            .is_err()
        {
            log::warn!("本地 OSC 通知通道已满/关闭，丢弃一条 OSC 通知");
        }
    }
}

/// 发给读线程的控制消息。
#[derive(Debug)]
pub enum LocalMsg {
    /// 待写入 PTY 的输入字节（键盘、粘贴等）。
    Input(Vec<u8>),
    /// PTY 窗口尺寸变更。
    ///
    /// cell 字段必须是**物理像素**（由 `TerminalBounds::window_size` 按显示
    /// 缩放换算）。这一条消息同时喂给两个消费方：`pty.on_resize` 拿去算
    /// `ws_xpixel`，读线程拿去填 `TermGeometry`——同源同口径，不会出现
    /// `CSI 14 t` 与 `CSI 16 t` 互相矛盾。
    Resize(WindowSize),
    /// 关闭终端：读线程退出并释放 `Pty`（触发 SIGHUP）。
    Shutdown,
}

/// 本地 PTY 读线程的发送端。
///
/// 只保留输入侧能力：输出字节由读线程直接写进 `Term`，不经过这里。
///
/// 持有 `Poller` 是必需的：读线程阻塞在 `Poller::wait` 上，若只把消息压进
/// 通道而不唤醒，PTY 空闲时输入/尺寸变更会一直滞留到下次 PTY 输出为止
/// （alacritty 的 `EventLoopSender` 同理持有 poller）。
#[derive(Clone)]
pub struct LocalSender {
    tx: async_channel::Sender<LocalMsg>,
    poller: Arc<Poller>,
}

impl LocalSender {
    /// 写入 PTY。通道满/已关闭时静默丢弃（终端正在销毁或输入过快）。
    pub fn input(&self, bytes: Vec<u8>) {
        self.send(LocalMsg::Input(bytes));
    }

    /// 通知 PTY 尺寸变更。`size` 的 cell 字段须已是物理像素。
    pub fn resize(&self, size: WindowSize) {
        self.send(LocalMsg::Resize(size));
    }

    /// 请求读线程退出。
    pub fn shutdown(&self) {
        self.send(LocalMsg::Shutdown);
    }

    /// 压入消息并唤醒读线程，避免消息在 poller 阻塞期间滞留。
    fn send(&self, msg: LocalMsg) {
        if self.tx.try_send(msg).is_ok() {
            let _ = self.poller.notify();
        }
    }
}

/// 读线程向 UI 线程发送的通知。
///
/// 只承载「需要 `Context` 才能完成的副作用」——vte 解析本身在读线程完成，
/// 所以这里不含终端输出数据，通道天然是小流量、有界的。
#[derive(Debug)]
pub enum LocalNotice {
    /// 有字节被解析，UI 需要重绘。
    Wakeup,
    /// 读线程摘到一条**完整**的 OSC 序列（内容不含 `ESC ]` 与终止符），
    /// 附该序列出现时刻的网格光标 `(行, 列)`。
    ///
    /// 为什么要绕到 UI 线程：应用层判定一条 OSC 是否有意义依赖两类状态——
    /// `Context`（发事件、弹通知）与 `Terminal` 自己的字段（cwd 去重、
    /// 前台进程名）。两者都只在 UI 线程上可访问，读线程不能碰。
    ///
    /// 为什么要带光标：`OSC 133;B` 的语义是「此刻光标即输入区起点」。通知
    /// 到达 UI 线程时同一 chunk 后续字节早已解析完，现读光标只能拿到行尾，
    /// 所以必须由读线程在序列位置就地取样。
    ///
    /// 通道容量足够：hook 事件与提示符标记都是"每命令几条"的量级，不像
    /// PTY 输出那样连续成兆；`try_send` 失败时该条被丢弃（丢一条 hook 事件
    /// 只影响一次状态刷新，好过阻塞读线程）。字节本身已按位置喂给 vte，
    /// 丢通知不会影响 vte 侧的那几类 OSC。
    Osc {
        content: Vec<u8>,
        cursor: (i32, u32),
    },
    /// 子进程退出，附退出状态。
    ChildExit(Option<std::process::ExitStatus>),
    /// 读线程出错（PTY 读取失败等）。
    Error(String),
}

/// 本地 PTY 读线程。
///
/// 负责：从 PTY 读取字节 → 应用层扫描 → vte 解析 → 通知 UI；
/// 以及接收输入/尺寸/关闭指令。`Pty` 的所有权在线程内，直到退出才释放。
///
/// `spawn` 返回「发送端 + 通知接收端」：通知必须由 UI 线程消费，
/// 因此接收端不留在本结构内（避免读线程之外还持有一个消费方）。
pub struct LocalReader {
    sender: LocalSender,
    /// 读线程句柄：仅用于保持线程存活（`LocalReader` drop 即 join）,
    /// 字段本身不被读取，故以下划线前缀标记。
    _thread: Option<std::thread::JoinHandle<()>>,
}

/// `LocalReader::spawn` 的返回值。
pub struct LocalReaderParts {
    pub reader: LocalReader,
    /// 读线程通知的接收端，由 UI 线程的泵任务消费。
    pub notices: async_channel::Receiver<LocalNotice>,
}

impl LocalReader {
    /// 启动读线程。
    ///
    /// `term` 与最终 `Terminal` 上持有的 `term` 是同一个 `Arc`（alacritty 的
    /// `Term` 本就设计为跨线程共享）；`listener` 用于发送 `Event::Wakeup` /
    /// `Event::ChildExit` 等 alacritty 事件。
    ///
    /// `initial_geometry` 是创建 PTY 时用的初始几何：先于任何 `LocalMsg::Resize`
    /// 注入扫描器，保证终端应用在启动瞬间（resize 尚未到达时）发出的尺寸查询
    /// 也能拿到真实值而非空值。
    pub fn spawn(
        mut pty: Pty,
        term: Arc<FairMutex<Term<ZedListener>>>,
        listener: ZedListener,
        mut scanner: Box<dyn PtyScanner>,
        initial_geometry: escape_scan::TermGeometry,
    ) -> std::io::Result<LocalReaderParts> {
        scanner.set_geometry(initial_geometry);

        let (tx, rx) = async_channel::bounded::<LocalMsg>(256);
        let (notice_tx, notice_rx) = async_channel::bounded::<LocalNotice>(256);

        // 读线程摘到的 OSC 要经通知通道交给 UI 线程判定（见 [`NoticeOscSink`]）。
        // 通道发送端克隆一份给扫描器；`notice_tx` 本体仍归 `run_loop`
        // （重绘/退出/错误通知走那条）。
        scanner.set_osc_sink(Box::new(NoticeOscSink::new(notice_tx.clone())));

        let poller = Arc::new(Poller::new()?);

        // SAFETY（中文）：`pty` 被 move 进读线程闭包并存活至线程结束，
        // 注册到 poller 的 file/signals 因此始终有效；线程退出前会 deregister。
        //
        // 只注册可读：PTY 主设备写侧恒可写，若一并注册可写会在电平触发下
        // 每次 `wait` 立即返回，形成忙循环。写入走 `pty.writer()` 直接写。
        unsafe {
            pty.register(
                &poller,
                PollEvent::readable(PTY_READ_WRITE_TOKEN),
                PollMode::Level,
            )?;
        }

        // 发送端需要 poller 来唤醒阻塞中的读线程，先克隆一份。
        let sender_poller = poller.clone();

        let thread = std::thread::Builder::new()
            .name("local-pty-reader".to_string())
            .spawn(move || {
                run_loop(pty, poller, term, listener, notice_tx, rx, scanner);
            })?;

        Ok(LocalReaderParts {
            reader: Self {
                sender: LocalSender {
                    tx,
                    poller: sender_poller,
                },
                _thread: Some(thread),
            },
            notices: notice_rx,
        })
    }

    /// 输入/尺寸/关闭的发送端。
    pub fn sender(&self) -> LocalSender {
        self.sender.clone()
    }
}

fn run_loop(
    mut pty: Pty,
    poller: Arc<Poller>,
    term: Arc<FairMutex<Term<ZedListener>>>,
    listener: ZedListener,
    notice: async_channel::Sender<LocalNotice>,
    rx: async_channel::Receiver<LocalMsg>,
    mut scanner: Box<dyn PtyScanner>,
) {
    let mut processor = Processor::<StdSyncHandler>::new();
    let mut buf = vec![0u8; READ_BUFFER_SIZE];
    // 跨读残尾（未完成的 OSC/APC 序列）。
    let mut pending: Vec<u8> = Vec::new();
    // 事件容量：单次最多 2 个源（PTY 读写 fd + 子进程信号 fd）。
    let mut events =
        Events::with_capacity(std::num::NonZeroUsize::new(4).expect("事件容量恒为非零"));

    'event_loop: loop {
        events.clear();
        if let Err(err) = poller.wait(&mut events, None) {
            let _ = notice.try_send(LocalNotice::Error(format!("poll failed: {err}")));
            break;
        }
        // 先处理控制指令（输入/尺寸/退出）。
        while let Ok(msg) = rx.try_recv() {
            match msg {
                LocalMsg::Input(bytes) => {
                    if let Err(err) = pty.writer().write_all(&bytes) {
                        let _ =
                            notice.try_send(LocalNotice::Error(format!("pty write failed: {err}")));
                    }
                }
                LocalMsg::Resize(size) => {
                    pty.on_resize(size);
                    // 几何也要告知扫描器：尺寸上报（`CSI 16 t` / in-band
                    // size reports）需要单元格**物理像素**尺寸。这两处同源自
                    // `TerminalBounds::window_size`，所以口径天然一致。
                    scanner.set_geometry(escape_scan::TermGeometry {
                        rows: size.num_lines,
                        cols: size.num_cols,
                        cell_width: size.cell_width,
                        cell_height: size.cell_height,
                    });
                }
                LocalMsg::Shutdown => break 'event_loop,
            }
        }

        for event in events.iter() {
            match event.key {
                PTY_CHILD_EVENT_TOKEN => {
                    if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
                        // `Option` 为 None 表示进程被信号杀死（try_wait 拿不到状态）。
                        if let Some(status) = status {
                            listener.send_event(AlacTermEvent::ChildExit(status));
                        }
                        // drain_on_exit：退出前读完残余输出，
                        // 否则最后一行提示符可能丢失。
                        let _ = pty_read(
                            &mut pty,
                            &mut processor,
                            &term,
                            &mut buf,
                            &mut pending,
                            &mut *scanner,
                            None,
                        );
                        term.lock().exit();
                        listener.send_event(AlacTermEvent::Wakeup);
                        let _ = notice.try_send(LocalNotice::ChildExit(status));
                        break 'event_loop;
                    }
                }
                PTY_READ_WRITE_TOKEN => {
                    if event.is_interrupt() {
                        continue;
                    }
                    if event.readable {
                        match pty_read(
                            &mut pty,
                            &mut processor,
                            &term,
                            &mut buf,
                            &mut pending,
                            &mut *scanner,
                            Some(&notice),
                        ) {
                            Ok(()) => {}
                            Err(err) => {
                                let _ = notice.try_send(LocalNotice::Error(format!(
                                    "pty read failed: {err}"
                                )));
                                break 'event_loop;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // 应答回写：放在事件循环尾部的**每轮**执行，而不是嵌在"可读"分支里。
        //
        // 扫描器的应答不一定由"读到字节"产生——resize 转发的几何变化也会产生
        // （in-band size reports 在启用期间每次尺寸变化都要重新上报）。那种情况
        // 下 poller 被 `notify()` 唤醒、控制消息被排空，但**没有读事件**，若把
        // 回写挂在 `if event.readable` 内就会一直攒着不发。
        let replies = scanner.take_replies();
        if !replies.is_empty()
            && let Err(err) = pty.writer().write_all(&replies)
        {
            let _ = notice.try_send(LocalNotice::Error(format!("pty reply write failed: {err}")));
            break 'event_loop;
        }
    }

    // 显式注销，避免 poller 持有已释放 fd；随后 drop(pty) 触发 SIGHUP。
    let _ = pty.deregister(&poller);
}

/// 扫描回调的借用上下文。
///
/// `PtyScanner::scan` 需要「把文本喂给 vte」与「读当前网格状态」两种能力，两者
/// 都作用于同一份 `Term`/`Processor`。用结构体统一持有，避免在调用点为两个
/// 闭包同时借用同一变量而无法通过借用检查。
struct ScanCtx<'a> {
    processor: &'a mut Processor<StdSyncHandler>,
    terminal: &'a mut Term<ZedListener>,
}

impl ScanCtx<'_> {
    /// 把一段普通文本喂给 vte。空段跳过（`advance` 对空输入无意义）。
    fn emit(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.processor.advance(self.terminal, bytes);
        }
    }

    /// 取网格状态前强制回灌同步更新（`CSI ?2026h`…`CSI ?2026l`）的缓冲。
    ///
    /// vte 0.15 在同步块期间**不解析**收到的字节，全部攒进
    /// `sync_state.buffer`，直到 ESU 或超时/超限才回灌——期间 `grid`
    /// 完全停在块开始前的状态。grok 的帧恰好是
    /// `2026h + 绘制 + CSI 定位 + 图像分片 + 2026l`：分片 APC 在本层立即
    /// 执行，可它前面的 CSI 还在缓冲里，锚点会取到**旧光标**（实测取到
    /// 帧首的 prompt 行，图像因此渲染到输入框上）。与 kitty/ghostty 的
    /// 语义（序列立即生效、同步只推迟**渲染**）对齐：取锚点前把缓冲解析掉。
    ///
    /// 副作用：本帧剩余部分失去 vte 的「同步块内不通知重绘」保护——仅
    /// 在同步块里出现图形 APC（即传图）时发生一次，换来锚点正确。
    /// grok 随后的 ESU 到达时同步态已被清掉，按普通序列忽略，无副作用。
    fn flush_sync_update(&mut self) {
        if self.processor.sync_bytes_count() > 0 {
            self.processor.stop_sync(self.terminal);
        }
    }

    /// 当前网格状态：光标绝对位置 + 可见行窗口。
    ///
    /// 一次取全（而不是分成两个闭包）是因为两者必须来自**同一时刻**的 `grid`
    /// 快照：`d=a` 的可见性判定要基于与锚点相同的那次滚动位置。
    fn grid_state(&self) -> GridState {
        let grid = self.terminal.grid();
        let point = grid.cursor.point;
        GridState {
            cursor: (point.line.0, point.column.0 as u32),
            visible: crate::terminal::kitty::storage::VisibleRows::new(
                grid.display_offset(),
                grid.screen_lines(),
            ),
        }
    }
}

/// 从 PTY 读取并解析，复刻 alacritty `EventLoop::pty_read` 的语义：
/// 持锁解析、按 `MAX_LOCKED_READ` 让出。
///
/// `notice` 为 `None` 时（退出前的 drain）不发重绘通知。
fn pty_read(
    pty: &mut Pty,
    processor: &mut Processor<StdSyncHandler>,
    term: &Arc<FairMutex<Term<ZedListener>>>,
    buf: &mut [u8],
    pending: &mut Vec<u8>,
    scanner: &mut dyn PtyScanner,
    notice: Option<&async_channel::Sender<LocalNotice>>,
) -> std::io::Result<()> {
    let mut unprocessed = 0;
    let mut processed = 0;

    let _terminal_lease = Some(term.lease());
    let mut locked = None;

    loop {
        match pty.reader().read(&mut buf[unprocessed..]) {
            Ok(0) if unprocessed == 0 => break,
            Ok(got) => unprocessed += got,
            Err(err) => match err.kind() {
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock => {
                    if unprocessed == 0 {
                        break;
                    }
                }
                _ => return Err(err),
            },
        }

        let terminal = match &mut locked {
            Some(terminal) => terminal,
            None => locked.insert(match term.try_lock_unfair() {
                // 缓冲满时必须阻塞等待，不能丢字节。
                None if unprocessed >= READ_BUFFER_SIZE => term.lock_unfair(),
                None => continue,
                Some(terminal) => terminal,
            }),
        };

        // 应用层扫描：普通文本经 `emit` 即时喂给 vte，需消费的序列（OSC/APC）
        // 被摘除，未完成的尾部留在 `pending` 等下次拼接。
        //
        // 扫描器不能自己碰 `Term`（锁已被本函数持有），所以文本喂入与网格
        // 状态查询都以闭包形式由这里提供。这样 APC 处理时看到的网格状态已经
        // 包含了同一 chunk 里在它之前的文本（如 `CSI 3;5H` 定位）。
        {
            // 两个闭包都借用同一份 `Term`/`Processor`，而借用检查器无法
            // 证明 `scan` 不会并发调用它们（实际上 `scan` 是顺序调用的：
            // 一次 `emit` 或一次 `grid`，绝不同时）。
            //
            // 因此把上下文放进 `RefCell`，闭包各自 `borrow_mut` 一小段；
            // 若真的发生重入会 panic（而不是未定义行为），比裸指针安全。
            let ctx = std::cell::RefCell::new(ScanCtx {
                processor: &mut *processor,
                terminal,
            });
            let mut emit = |bytes: &[u8]| ctx.borrow_mut().emit(bytes);
            let mut grid = || {
                let mut ctx = ctx.borrow_mut();
                ctx.flush_sync_update();
                ctx.grid_state()
            };
            scanner.scan(&buf[..unprocessed], pending, &mut emit, &mut grid);
        }

        processed += unprocessed;
        unprocessed = 0;

        if processed >= MAX_LOCKED_READ {
            break;
        }
    }

    // 仅在解析到非「同步更新」字节时通知重绘（对齐 alacritty：同步更新
    // 期间不应重绘，直到收到结束标记）。
    if processed > 0
        && processor.sync_bytes_count() < processed
        && let Some(notice) = notice
    {
        let _ = notice.try_send(LocalNotice::Wakeup);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Point, px};
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    /// 记录扫描器收到的全部字节，用于断言 APC 确实到达应用层。
    struct RecordingScanner {
        seen: Arc<StdMutex<Vec<u8>>>,
    }

    impl PtyScanner for RecordingScanner {
        fn scan(
            &mut self,
            data: &[u8],
            _pending: &mut Vec<u8>,
            emit: &mut dyn FnMut(&[u8]),
            _grid: &mut dyn FnMut() -> GridState,
        ) {
            self.seen.lock().unwrap().extend_from_slice(data);
            emit(data);
        }
    }

    /// 轮询等待条件成立，避免固定 sleep 造成的偶发失败。
    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    /// 端到端验证：本地 PTY 中 `printf` 出来的 APC 序列能到达扫描器。
    ///
    /// 这是整个 Kitty 图形协议实现的前提——若 APC 无法到达应用层，
    /// 后续解析与渲染都没有意义。
    #[test]
    fn apc_bytes_from_local_pty_reach_scanner() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen_for_scanner = seen.clone();

        let bounds = crate::terminal::TerminalBounds::default();
        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec![
                    "-c".to_string(),
                    // 输出 APC（Kitty graphics 的载体），再输出一个哨兵便于判定完成。
                    r"printf '\033_Gi=1,a=q;\033\\'; printf SENTINEL".to_string(),
                ],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let parts = LocalReader::spawn(
            pty,
            term,
            listener,
            Box::new(RecordingScanner {
                seen: seen_for_scanner,
            }),
            bounds.into(),
        )
        .expect("启动读线程失败");

        let got_sentinel = wait_until(
            || {
                let s = seen.lock().unwrap();
                memchr::memmem::find(&s, b"SENTINEL").is_some()
            },
            Duration::from_secs(10),
        );
        assert!(got_sentinel, "未在超时内收到哨兵输出，扫描器未收到数据");

        let bytes = seen.lock().unwrap().clone();
        assert!(
            memchr::memmem::find(&bytes, b"\x1b_G").is_some(),
            "APC 起始序列 \\x1b_G 未到达扫描器；实际收到: {:?}",
            String::from_utf8_lossy(&bytes)
        );

        // 收尾：请求关闭，避免读线程泄漏。
        parts.reader.sender().shutdown();
    }

    /// 输入经发送端写入 PTY，并被回显读回——验证 `LocalMsg::Input` 通路。
    #[test]
    fn input_round_trips_through_pty() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen_for_scanner = seen.clone();

        let bounds = crate::terminal::TerminalBounds::default();
        let options = alacritty_terminal::tty::Options {
            // `cat` 会把 stdin 原样回显，形成可断言的往返。
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/cat".to_string(),
                Vec::new(),
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let parts = LocalReader::spawn(
            pty,
            term,
            listener,
            Box::new(RecordingScanner {
                seen: seen_for_scanner,
            }),
            bounds.into(),
        )
        .expect("启动读线程失败");

        parts.reader.sender().input(b"ROUNDTRIP\n".to_vec());

        let got = wait_until(
            || {
                let s = seen.lock().unwrap();
                memchr::memmem::find(&s, b"ROUNDTRIP").is_some()
            },
            Duration::from_secs(10),
        );
        assert!(got, "输入未通过 PTY 回显回来");

        parts.reader.sender().shutdown();
    }

    /// 端到端验证：真实 Kitty 扫描器在 PTY 字节流上完成一次完整的
    /// 「传输 + 显示」，图像落进共享存储且应答被投递。
    ///
    /// 这是从 PTY 字节到图像存储的整条链路验证——上面的
    /// `apc_bytes_from_local_pty_reach_scanner` 只证明字节到了扫描器，
    /// 这里证明扫描器真的解析并执行了协议。
    #[test]
    fn kitty_transmit_and_display_through_local_pty() {
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let bounds = crate::terminal::TerminalBounds::default();

        // 1×1 RGB 红点，基64 后为 "/wAA"（3 字节 → 4 个 base64 字符）。
        // 分两个 APC 输出：先 `a=t,f=24,s=1,v=1` 传输，再 `a=p,i=1` 显示。
        // 先把光标移到第 3 行第 5 列（`\033[3;5H`），验证放置的锚点确实
        // 取自 term 的实时光标而非默认 (0,0)。
        let script = concat!(
            r"printf '\033[3;5H';",
            r"printf '\033_Gi=1,f=24,s=1,v=1,a=t;/wAA\033\\';",
            r"printf '\033_Gi=1,a=p\033\\';",
            r"printf SENTINEL",
        );

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        // 扫描器与测试之间共享存储与应答缓冲，便于读线程写入后断言。
        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));
        let replies = Arc::new(StdMutex::new(Vec::new()));
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        /// 把应答与变更计数写进共享缓冲的 sink（跨线程断言用）。
        struct SharedSink {
            replies: Arc<StdMutex<Vec<u8>>>,
            changes: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl crate::terminal::kitty::scanner::ReplySink for SharedSink {
            fn reply(&mut self, bytes: &[u8]) {
                self.replies.lock().unwrap().extend_from_slice(bytes);
            }
            fn images_changed(&mut self) {
                self.changes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let scanner = KittyGraphicsScanner::with_storage(
            storage.clone(),
            true,
            Box::new(SharedSink {
                replies: replies.clone(),
                changes: changes.clone(),
            }),
        );

        let parts = LocalReader::spawn(pty, term, listener, Box::new(scanner), bounds.into())
            .expect("启动读线程失败");

        let done = wait_until(
            || {
                let s = storage.lock().unwrap();
                // 传输 + 显示都完成：图像存在且有 1 个放置。
                s.get(1).is_some_and(|img| img.placements.len() == 1)
            },
            Duration::from_secs(10),
        );

        {
            let s = storage.lock().unwrap();
            let stored = s.get(1).expect("图像 1 未落进共享存储");
            assert_eq!(stored.image.width, 1);
            assert_eq!(stored.image.height, 1);
            assert_eq!(
                &stored.image.rgba[..4],
                &[255, 0, 0, 255],
                "1×1 红点应解出红色 RGBA"
            );
            assert_eq!(stored.placements.len(), 1, "a=p 应建立一个放置");
            // 锚点来自 term 的实时光标：CSI 3;5H 是 1-based，落到 0-based 的
            // (2, 4)。若锚定失效（恒为 0,0）这条断言会失败。
            let (_, placement) = stored.placements.iter().next().expect("应有放置");
            assert_eq!(
                (placement.row, placement.col),
                (2, 4),
                "放置锚点应为 CSI 3;5H 定位后的光标位置"
            );
        }
        assert!(done, "超时：图像未在 PTY 字节流上完成传输与显示");

        // 协议应答（两次 OK）应已投递到 sink。
        let replies = replies.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&replies);
        let ok_count = memchr::memmem::find_iter(&replies, b"OK").count();
        assert!(
            ok_count >= 2,
            "应有至少 2 条 OK 应答，实际 {ok_count} 条：{text:?}"
        );
        // 应答必须回显请求里的 `i=1`——只数 `OK` 会漏掉"回了但 id 是 0"这种
        // 客户端认不出的情况（能力探测就是按 `Gi=<请求 id>;` 匹配的）。
        assert!(
            text.contains("i=1;OK"),
            "应答应回显请求 id（i=1;OK），实际：{text:?}"
        );
        assert!(
            changes.load(std::sync::atomic::Ordering::Relaxed) >= 2,
            "传输与显示都应标记图像已变化"
        );

        parts.reader.sender().shutdown();
    }

    /// 覆盖 terminal-browser 实际发出的**那一种**负载形态，并一路验证到
    /// 「渲染层能画出它」。
    ///
    /// 之前的测试各覆盖了一段，但没有任何一个走完从 PTY 字节到
    /// `render::plan()` 的整条路。这中间有一类失败模式是查不出来的：
    /// 图像进了存储、但几何/锚点算不出可见矩形，`plan()` 把它丢掉——终端
    /// 上就是「探测通过、却什么都不显示」。
    ///
    /// 负载形态取自 terminal-browser 的原生模块 `pixel.node` 里的字符串
    /// 常量，逐条对齐：
    /// - `_Ga=T,f=32,s=` → **`a=T`**（传输并立即显示）、**`f=32`**（RGBA）、
    ///   显式 `s=`/`v=` 像素尺寸（不是 PNG，这是最容易漏的一条）
    /// - `,q=2` → **`q=2`**（静默：不应答）
    /// - `a=T,f=32,o=z,s=` → **`o=z`**（zlib 压缩负载）
    /// - `,q=2,m=` / `_G~_6` → 分片续传
    ///
    /// `q=2` 与 `a=T` 组合意味着：这条路径上**不能靠应答判断是否成功**，
    /// 只能看图像是否真的进了存储并被 `plan()` 认作可绘制。
    #[test]
    fn browser_shaped_rgba_zlib_payload_reaches_render_plan() {
        use crate::terminal::kitty::render::{CellMetrics, Viewport, plan};
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let bounds = crate::terminal::TerminalBounds::default();

        // 4×4 纯红 RGBA（f=32）原始像素，zlib 压缩后 base64。
        // 256 字节的原始数据足以跨越 zlib 的最小压缩单元，避免退化成
        // 「压缩后反而更长」的边角情形。
        let mut raw = Vec::with_capacity(4 * 4 * 4);
        for _ in 0..16 {
            raw.extend_from_slice(&[255, 0, 0, 255]);
        }
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &raw).expect("压缩失败");
        let compressed = enc.finish().expect("压缩收尾失败");
        let b64 = crate::terminal::kitty::command::encode_base64(&compressed);

        // 逐字对齐 `_Ga=T,f=32,o=z,s=...,q=2`：q=2 静默，故断言只能落在
        // 存储与渲染计划上。
        let script = format!(
            "printf '\\033[4;7H'; \
             printf '\\033_Ga=T,f=32,o=z,s=4,v=4,i=42,q=2;{b64}\\033\\\\'; \
             printf SENTINEL"
        );
        assert!(
            !script.contains("a=q"),
            "本测试的负载必须是显示类动作，不能混入探测"
        );

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));
        let scanner = KittyGraphicsScanner::with_storage(
            storage.clone(),
            true,
            Box::new(crate::terminal::kitty::CollectingSink::default()),
        );

        let parts = LocalReader::spawn(pty, term, listener, Box::new(scanner), bounds.into())
            .expect("启动读线程失败");

        let done = wait_until(
            || {
                let s = storage.lock().unwrap();
                // `a=T` 必须同时完成「解码 + 建立放置」，只有两者都成立才算
                // 走通；少了放置就是渲染层拿不到东西，屏幕必然空白。
                s.get(42).is_some_and(|img| img.placements.len() == 1)
            },
            Duration::from_secs(10),
        );

        {
            let s = storage.lock().unwrap();
            let stored = s.get(42).expect("a=T,f=32,o=z 的图像 42 未能落进存储");

            assert_eq!(
                (stored.image.width, stored.image.height),
                (4, 4),
                "显式 s=4,v=4 的 RGBA 应解出 4×4"
            );
            // 原始负载是纯红：解开 o=z 后首像素必须是 (255,0,0,255)。
            // 这条同时证明了「zlib 解压」与「RGBA 布局」两件事都正确。
            assert_eq!(
                &stored.image.rgba[..4],
                &[255, 0, 0, 255],
                "o=z 解压后应还原出纯红 RGBA 像素"
            );
        }
        assert!(done, "超时：a=T,f=32,o=z 负载未在 10s 内完成传输+显示");

        // 关键：把存储喂给渲染层的 plan()，确认它**真的算出一个可绘制的
        // 矩形**。这是本测试存在的理由——前面所有测试到存储就停了，
        // 而「存储有图像但屏幕上没有」正是用户看到的现象。
        let placed = {
            let s = storage.lock().unwrap();
            plan(
                &s,
                CellMetrics {
                    cell_width: px(9.0),
                    line_height: px(18.0),
                },
                Viewport {
                    origin: Point::new(px(0.0), px(0.0)),
                    display_offset: 0,
                    rows: 24,
                    cols: 80,
                },
            )
        };

        assert_eq!(
            placed.len(),
            1,
            "plan() 应产出恰好 1 个绘制项，实际 {}",
            placed.len()
        );
        let img = &placed[0];
        assert_eq!(img.id, 42, "绘制项应指向图像 42");
        assert!(
            img.size.width > px(0.0) && img.size.height > px(0.0),
            "绘制尺寸不能退化（退化会被 plan 丢弃），实际 {:?}",
            img.size
        );
        // 锚点取自 `CSI 4;7H` 后的光标：1-based (4,7) → 0-based (3,6)。
        // 列方向 = 6 个单元格宽；行方向 = 3 个行高。
        assert_eq!(
            img.origin,
            Point::new(px(6.0 * 9.0), px(3.0 * 18.0)),
            "绘制原点应为 CSI 4;7H 定位后的光标位置"
        );

        parts.reader.sender().shutdown();
    }

    /// 端到端验证：真实 PNG 经协议分片（`m=1`）跨多次 `printf` 传输，
    /// 在 PTY 上被正确重组并解码。
    ///
    /// 覆盖两条正交的「分片」路径里更难的一条：**协议级分片**（多个完整
    /// APC 拼成一张图）。`apc_bytes_from_local_pty_reach_scanner` 覆盖的是
    /// 读边界截断，这里覆盖 `m=1`/`m=0`。
    #[test]
    fn kitty_chunked_png_transmission_through_local_pty() {
        use crate::terminal::kitty::scanner::{KittyGraphicsScanner, ReplySink};

        // 8×8 纯蓝 RGBA PNG，base64 后 100 字符；切成两半模拟分片。
        const B64: &str = concat!(
            "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAYAAADED76LAAAAEUlEQVR4nGNg",
            "YPj/Hz8eEQoAQ1d/gZtbdTsAAAAASUVORK5CYII=",
        );
        assert!(B64.len() >= 8, "分片需要至少 8 个字符");
        let (head, tail) = B64.split_at(B64.len() / 2);
        assert!(!head.is_empty() && !tail.is_empty());

        let bounds = crate::terminal::TerminalBounds::default();

        // 第一片 `m=1`（后续还有），第二片 `m=0`（结束）。
        // 两次 printf 独立成块，形成两条独立的 APC。
        let script = format!(
            "printf '\\033_Gi=7,f=100,a=t,m=1;{head}\\033\\\\'; \
             printf '\\033_Gi=7,m=0;{tail}\\033\\\\'; \
             printf SENTINEL"
        );

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));
        let replies = Arc::new(StdMutex::new(Vec::new()));

        struct Sink(Arc<StdMutex<Vec<u8>>>);
        impl ReplySink for Sink {
            fn reply(&mut self, bytes: &[u8]) {
                self.0.lock().unwrap().extend_from_slice(bytes);
            }
            fn images_changed(&mut self) {}
        }

        let scanner = KittyGraphicsScanner::with_storage(
            storage.clone(),
            true,
            Box::new(Sink(replies.clone())),
        );

        let parts = LocalReader::spawn(pty, term, listener, Box::new(scanner), bounds.into())
            .expect("启动读线程失败");

        let decoded = wait_until(
            || storage.lock().unwrap().get(7).is_some(),
            Duration::from_secs(10),
        );
        assert!(
            decoded,
            "分片传输的 PNG 未在超时内解码完成；应答: {:?}",
            String::from_utf8_lossy(&replies.lock().unwrap())
        );

        {
            let s = storage.lock().unwrap();
            let stored = s.get(7).expect("图像 7 应存在");
            assert_eq!(
                (stored.image.width, stored.image.height),
                (8, 8),
                "应解出 8×8 的 PNG"
            );
            // 纯蓝：首像素 RGBA = (0, 0, 255, 255)。
            assert_eq!(
                &stored.image.rgba[..4],
                &[0, 0, 255, 255],
                "PNG 应解出纯蓝像素"
            );
        }

        // 末片才产生应答（中间片按协议静默）。
        let replies = replies.lock().unwrap().clone();
        assert!(
            memchr::memmem::find(&replies, b"OK").is_some(),
            "末片应产生 OK 应答：{:?}",
            String::from_utf8_lossy(&replies)
        );

        parts.reader.sender().shutdown();
    }

    /// 复刻 grok 悬浮预览发出的**完整字节流**，一路验证到渲染计划。
    ///
    /// grok 的形态与 terminal-browser 有几处关键差异，此前没有测试覆盖：
    /// - 每帧先 `a=d,d=i,i=1`（删同 id 旧图）再 `a=T`；
    /// - 显式 `p=1`（放置 id）、`z=1`（画在文字之上）、`C=1`（不动光标）；
    /// - 负载是 **PNG**（`f=100`，不带 `s=`/`v=`），base64 按 **4096 字符**
    ///   分片（grok 的 445 KB 截图约 146 片）；
    /// - 全程 `q=2`：**零应答**——出错也无声，失败只表现为「屏幕上什么
    ///   都没有」，正是用户报的「一直 Loading」。
    ///
    /// 因此断言落在「图像进存储 + 放置锚点/尺寸/层级正确 + `plan()` 真的
    /// 算出可见矩形」，并额外钉住「`q=2` 不产生任何应答」。
    #[test]
    fn grok_overlay_shaped_stream_reaches_render_plan() {
        use crate::terminal::kitty::render::{CellMetrics, Viewport, plan};
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let bounds = crate::terminal::TerminalBounds::default();

        // 160×90 噪声 RGBA → PNG：噪声压不动，base64 必然超过单片 4096，
        // 逼出 grok 的多分片路径。
        const W: u32 = 160;
        const H: u32 = 90;
        let mut rgba = Vec::with_capacity((W * H * 4) as usize);
        let mut lcg: u32 = 0x9E37_79B9;
        for _ in 0..W * H {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let v = (lcg >> 24) as u8;
            rgba.extend_from_slice(&[v, v.wrapping_mul(3), v.wrapping_mul(7), 255]);
        }
        let img = image::RgbaImage::from_raw(W, H, rgba.clone()).expect("构造 RGBA 失败");
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("PNG 编码失败");
        let b64 = crate::terminal::kitty::command::encode_base64(&png);
        assert!(b64.len() > 4096, "需要多分片，实际 {} 字符", b64.len());

        // 光标定位到 0-based (row=6, col=9)：CSI 7;10H（grok 的
        // `\x1b[{y+1};{x+1}H` 同形）。c=12,r=6 是 grok 的「按格铺满」语义。
        const COLS: u32 = 12;
        const ROWS: u32 = 6;
        let chunks: Vec<&str> = b64
            .as_bytes()
            .chunks(4096)
            .map(|c| std::str::from_utf8(c).expect("base64 分片应是 ASCII"))
            .collect();

        let mut script = String::new();
        script.push_str(r"printf '\033_Ga=d,d=i,i=1,q=2\033\\'; ");
        script.push_str(r"printf '\033[7;10H'; ");
        for (i, chunk) in chunks.iter().enumerate() {
            let m = usize::from(i + 1 < chunks.len());
            if i == 0 {
                script.push_str(&format!(
                    "printf '\\033_Ga=T,f=100,t=d,q=2,C=1,z=1,i=1,p=1,c={COLS},r={ROWS},m={m};{chunk}\\033\\\\'; "
                ));
            } else {
                script.push_str(&format!("printf '\\033_Gq=2,m={m};{chunk}\\033\\\\'; "));
            }
        }
        script.push_str("printf SENTINEL");
        let chunk_count = b64.len().div_ceil(4096);

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));
        let replies = Arc::new(StdMutex::new(Vec::new()));
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        struct SharedSink {
            replies: Arc<StdMutex<Vec<u8>>>,
            changes: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl crate::terminal::kitty::scanner::ReplySink for SharedSink {
            fn reply(&mut self, bytes: &[u8]) {
                self.replies.lock().unwrap().extend_from_slice(bytes);
            }
            fn images_changed(&mut self) {
                self.changes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let scanner = KittyGraphicsScanner::with_storage(
            storage.clone(),
            true,
            Box::new(SharedSink {
                replies: replies.clone(),
                changes: changes.clone(),
            }),
        );

        let parts = LocalReader::spawn(
            pty,
            term.clone(),
            listener,
            Box::new(scanner),
            bounds.into(),
        )
        .expect("启动读线程失败");

        // 先确认脚本真的跑完（脚本本身失败与协议失败是两回事）。
        let ran = wait_until(
            || grid_text(&term).contains("SENTINEL"),
            Duration::from_secs(5),
        );
        assert!(
            ran,
            "子进程未跑到 SENTINEL：脚本本身失败（分片 {} 片）",
            chunk_count
        );

        let done = wait_until(
            || {
                let s = storage.lock().unwrap();
                s.get(1).is_some_and(|img| img.placements.len() == 1)
            },
            Duration::from_secs(15),
        );
        assert!(
            done,
            "超时：grok 形态的 a=T 分片流未完成传输+放置；应答: {:?}",
            String::from_utf8_lossy(&replies.lock().unwrap())
        );

        {
            let s = storage.lock().unwrap();
            let stored = s.get(1).expect("图像 1 应落进存储");
            assert_eq!(
                (stored.image.width, stored.image.height),
                (W, H),
                "f=100 不带 s=/v= 时应按 PNG 头解出尺寸"
            );
            assert_eq!(
                stored.image.rgba, rgba,
                "解出的像素应与送进 PNG 的原图一致（分片重组不能错位）"
            );

            // 放置：`p=1` 的放置 id、锚点取自 CSI 7;10H 的光标、c=/r=/z= 落位。
            assert!(
                stored.placements.contains_key(&1),
                "p=1 的放置应以放置 id 1 记账，实际 {:?}",
                stored.placements.keys().collect::<Vec<_>>()
            );
            let placement = &stored.placements[&1];
            assert_eq!(
                (placement.row, placement.col),
                (6, 9),
                "放置锚点应为 CSI 7;10H 定位后的光标位置"
            );
            assert_eq!(
                (placement.columns, placement.rows),
                (Some(COLS), Some(ROWS)),
                "c=/r= 应记为显式行列数"
            );
            assert_eq!(placement.z, 1, "z=1 应记为文字之上");
        }

        // `q=2` = 全部静默：连 OK 都不该有。若这里出现字节，说明静默级别
        // 被错误降级，客户端会收到它没请求的应答。
        assert!(
            replies.lock().unwrap().is_empty(),
            "q=2 不应产生任何应答，实际: {:?}",
            String::from_utf8_lossy(&replies.lock().unwrap())
        );
        assert!(
            changes.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "传输应标记图像已变化（否则渲染层不知道要重画）"
        );

        // 关键：把存储喂给渲染层，确认真的算出可见矩形——「进存储但屏幕
        // 空白」正是用户看到的现象，只测到存储是查不出来的。
        let placed = {
            let s = storage.lock().unwrap();
            plan(
                &s,
                CellMetrics {
                    cell_width: px(9.0),
                    line_height: px(18.0),
                },
                Viewport {
                    origin: Point::new(px(0.0), px(0.0)),
                    display_offset: 0,
                    rows: 24,
                    cols: 80,
                },
            )
        };
        assert_eq!(placed.len(), 1, "plan() 应产出恰好 1 个绘制项");
        let item = &placed[0];
        assert_eq!(item.id, 1, "绘制项应指向图像 1");
        assert!(
            !item.below_text,
            "z=1 必须画在文字之上（画在下面会被不透明的盒子背景盖住）"
        );
        assert_eq!(
            item.origin,
            Point::new(px(9.0 * 9.0), px(6.0 * 18.0)),
            "绘制原点应为 CSI 7;10H 定位后的光标位置"
        );
        assert_eq!(
            item.size,
            gpui::Size::new(px(COLS as f32 * 9.0), px(ROWS as f32 * 18.0)),
            "c=12,r=6 应按格子铺满（视口内不触发 shrink-to-fit）"
        );

        parts.reader.sender().shutdown();
    }

    /// 回放**真实抓取的 grok 1.0.41 原始输出**（`GROK_CAPTURE` 指向的
    /// 字节文件），断言放置锚点落在 CSI 定位处。
    ///
    /// 合成流（上一个测试）只能证明「我猜的字节顺序」没问题；本测试用
    /// 真机抓的完整会话——含帧重绘、17 次 `a=d`、`CSI 5;19H` 定位、
    /// 146 片 594KB 分片、以及所有探测查询——排除「真实字节形态与合成
    /// 假设不一致」这一整类可能。环境变量缺失时直接跳过（CI 无此文件）。
    #[test]
    fn grok_real_capture_stream_places_at_csi_anchor() {
        use crate::terminal::kitty::render::{CellMetrics, Viewport, plan};
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let Ok(capture) = std::env::var("GROK_CAPTURE") else {
            return;
        };
        let bounds = crate::terminal::TerminalBounds::new(
            px(16.0),
            px(9.0),
            gpui::Bounds {
                origin: Point::default(),
                size: gpui::Size::new(px(1620.0), px(720.0)),
            },
        );

        // 真实流的锚点：CSI 5;19H → 0-based (4, 18)；首片 c=143,r=32,z=1。
        let script = format!("cat '{}'; printf SENTINEL", capture.replace('\'', "'\\''"));
        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        // 真实 PNG 445KB → RGBA ~8.3MB，缓存必须放得下，否则解码即淘汰。
        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(64 * 1024 * 1024),
        ));
        let replies = Arc::new(StdMutex::new(Vec::new()));
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        struct SharedSink {
            replies: Arc<StdMutex<Vec<u8>>>,
            changes: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl crate::terminal::kitty::scanner::ReplySink for SharedSink {
            fn reply(&mut self, bytes: &[u8]) {
                self.replies.lock().unwrap().extend_from_slice(bytes);
            }
            fn images_changed(&mut self) {
                self.changes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let scanner = KittyGraphicsScanner::with_storage(
            storage.clone(),
            true,
            Box::new(SharedSink {
                replies: replies.clone(),
                changes: changes.clone(),
            }),
        );

        let parts = LocalReader::spawn(
            pty,
            term.clone(),
            listener,
            Box::new(scanner),
            bounds.into(),
        )
        .expect("启动读线程失败");

        let ran = wait_until(
            || grid_text(&term).contains("SENTINEL"),
            Duration::from_secs(10),
        );
        assert!(ran, "回放脚本未跑到 SENTINEL");

        let done = wait_until(
            || {
                let s = storage.lock().unwrap();
                s.get(1).is_some_and(|img| img.placements.len() == 1)
            },
            Duration::from_secs(20),
        );
        assert!(
            done,
            "超时：真实 grok 流未完成传输+放置；应答: {:?}",
            String::from_utf8_lossy(&replies.lock().unwrap())
        );

        {
            let s = storage.lock().unwrap();
            let stored = s.get(1).expect("图像 1 应落进存储");
            assert_eq!(
                (stored.image.width, stored.image.height),
                (2148, 962),
                "应解出抓取时所用 PNG 的真实尺寸"
            );
            let placement = &stored.placements[&1];
            assert_eq!(
                (placement.row, placement.col),
                (4, 18),
                "放置锚点必须来自 CSI 5;19H（框内），实际落在 {:?}",
                (placement.row, placement.col)
            );
            assert_eq!(
                (placement.columns, placement.rows),
                (Some(143), Some(32)),
                "首片 c=/r= 应被末片合并保留"
            );
            assert_eq!(placement.z, 1, "z=1");
        }

        assert!(
            changes.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "传输应标记图像已变化"
        );

        let placed = {
            let s = storage.lock().unwrap();
            plan(
                &s,
                CellMetrics {
                    cell_width: px(9.0),
                    line_height: px(24.0),
                },
                Viewport {
                    origin: Point::new(px(0.0), px(0.0)),
                    display_offset: 0,
                    rows: 45,
                    cols: 180,
                },
            )
        };
        assert_eq!(placed.len(), 1, "plan() 应产出恰好 1 个绘制项");
        let item = &placed[0];
        assert_eq!(
            item.origin,
            Point::new(px(18.0 * 9.0), px(4.0 * 24.0)),
            "绘制原点必须在框内 (row=4, col=18)，实际 {:?}",
            item.origin
        );

        parts.reader.sender().shutdown();
    }

    /// 同步更新块（`CSI ?2026h`…`CSI ?2026l`）内的「CSI 定位 + 图像
    /// 传输」必须仍按 CSI 锚定。
    ///
    /// 回归自 grok 真机形态：vte 0.15 在同步块期间不解析字节（攒进
    /// `sync_state.buffer`，ESU 才回灌），而 grok 的帧恰是
    /// `2026h + 绘制 + CSI + 分片 + 2026l`——分片 APC 在读线程立即执行，
    /// 它前面的 CSI 却还在缓冲里，修复前锚点取到块开始前的旧光标
    ///（prompt 行），图像被渲染到输入框上。取锚点前必须强制回灌
    /// （`ScanCtx::flush_sync_update`）。
    #[test]
    fn sync_update_block_keeps_csi_anchor_for_image_transmit() {
        use crate::terminal::kitty::render::{CellMetrics, Viewport, plan};
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let bounds = crate::terminal::TerminalBounds::default();

        // 8×8 灰度 PNG（约百字节，单分片即可：锚点问题与分片数无关）。
        const PNG_8X8_GRAY: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x00, 0x00, 0x00,
            0x00, 0xe1, 0x64, 0xe1, 0x57, 0x00, 0x00, 0x00, 0x0e, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x68, 0x80, 0x02, 0x06, 0xca, 0x18, 0x00, 0x80, 0x84, 0x20, 0x01, 0x0d,
            0x80, 0x24, 0x61, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
            0x82,
        ];
        let b64 = crate::terminal::kitty::command::encode_base64(PNG_8X8_GRAY);

        const COLS: u32 = 12;
        const ROWS: u32 = 6;

        // 真实 grok 帧形态：同步块开始 → 帧文本把光标停在 prompt 行 →
        // 删除旧放置 → CSI 定位到框内 → 分片传输（m=0 单片）→ 同步块结束。
        let mut script = String::new();
        script.push_str(
            r"printf '\033[?2026h\033[41;23HPROMPT\033_Ga=d,d=i,i=1,q=2\033\\\033[7;10H'; ",
        );
        script.push_str(&format!(
            "printf '\\033_Ga=T,f=100,t=d,q=2,C=1,z=1,i=1,p=1,c={COLS},r={ROWS},m=0;{b64}\\033\\\\'; "
        ));
        script.push_str(r"printf '\033[?2026l'; printf SENTINEL");

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));
        let replies = Arc::new(StdMutex::new(Vec::new()));
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        struct SharedSink {
            replies: Arc<StdMutex<Vec<u8>>>,
            changes: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl crate::terminal::kitty::scanner::ReplySink for SharedSink {
            fn reply(&mut self, bytes: &[u8]) {
                self.replies.lock().unwrap().extend_from_slice(bytes);
            }
            fn images_changed(&mut self) {
                self.changes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let scanner = KittyGraphicsScanner::with_storage(
            storage.clone(),
            true,
            Box::new(SharedSink {
                replies: replies.clone(),
                changes: changes.clone(),
            }),
        );

        let parts = LocalReader::spawn(
            pty,
            term.clone(),
            listener,
            Box::new(scanner),
            bounds.into(),
        )
        .expect("启动读线程失败");

        let ran = wait_until(
            || grid_text(&term).contains("SENTINEL"),
            Duration::from_secs(5),
        );
        assert!(ran, "子进程未跑到 SENTINEL");

        let done = wait_until(
            || {
                let s = storage.lock().unwrap();
                s.get(1).is_some_and(|img| img.placements.len() == 1)
            },
            Duration::from_secs(10),
        );
        assert!(
            done,
            "超时：同步块内的 a=T 未完成传输+放置；应答: {:?}",
            String::from_utf8_lossy(&replies.lock().unwrap())
        );

        {
            let s = storage.lock().unwrap();
            let stored = s.get(1).expect("图像 1 应落进存储");
            let placement = &stored.placements[&1];
            assert_eq!(
                (placement.row, placement.col),
                (6, 9),
                "同步块内 CSI 7;10H 必须先于锚点生效（修复前取到 prompt 行 40;22），实际 {:?}",
                (placement.row, placement.col)
            );
            assert_eq!(
                (placement.columns, placement.rows),
                (Some(COLS), Some(ROWS)),
                "c=/r= 应落位"
            );
            assert_eq!(placement.z, 1, "z=1");
        }
        assert!(
            changes.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "传输应标记图像已变化"
        );

        let placed = {
            let s = storage.lock().unwrap();
            plan(
                &s,
                CellMetrics {
                    cell_width: px(9.0),
                    line_height: px(18.0),
                },
                Viewport {
                    origin: Point::new(px(0.0), px(0.0)),
                    display_offset: 0,
                    rows: 24,
                    cols: 80,
                },
            )
        };
        assert_eq!(placed.len(), 1, "plan() 应产出恰好 1 个绘制项");
        assert_eq!(
            placed[0].origin,
            Point::new(px(9.0 * 9.0), px(6.0 * 18.0)),
            "绘制原点应为 CSI 7;10H 定位后的光标位置"
        );

        parts.reader.sender().shutdown();
    }

    /// 复刻 terminal-browser 能力探测的**完整往返**。
    ///
    /// 这是之前所有测试都没覆盖的一环：既有测试把应答收进测试自己的 sink，
    /// 只证明"执行器产出了正确的字节"，**没有**证明这些字节真的写回了子进程。
    /// terminal-browser 的探测是在子进程侧读 stdin 的——它发
    /// `ESC_Gi=4207,a=q,...ESC\` 后 500ms 内读不到含 `Gi=4207;OK` 的回复，
    /// 就判定"此终端不支持图像"并拒绝渲染任何图片。
    ///
    /// 因此本测试让子进程把回复**读回来再断言**，覆盖"应答回写 PTY"整条链路。
    #[test]
    fn kitty_query_probe_reply_round_trips_to_child() {
        use crate::terminal::kitty::CollectingSink;
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let bounds = crate::terminal::TerminalBounds::default();

        // 与 terminal-browser 的 probeSequence 逐字一致（PROBE_ID = 4207）。
        //
        // 子进程把探测序列打印到自己的 stdout（= PTY 主设备的读方向），
        // 之后**保持存活**：应答写回 PTY 后，行规程会把裸字节回显到子进程的
        // tty 上，从而出现在 `term` 的网格里。这与 terminal-browser 在自己的
        // stdin 上收到那份字节是同一个数据流。
        //
        // 为什么不让子进程自己 `read` 应答：PTY 上 `dd` 会因缓冲语义干等、
        // `$(...)` 又会剥掉裸 ESC，脚手架噪声远大于收益。让 tty 回显即可
        // 证伪「应答没写回 PTY」这个缺陷——修复前网格里根本不会有这串字节。
        //
        // `cat` 让子进程挂住并回显输入，`sleep` 给应答留出到达窗口。
        let script = concat!(
            r"printf '\033_Gi=4207,a=q,t=d,f=24,s=1,v=1;AAAA\033\\';",
            r"sleep 3",
        );

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));

        // 用 `CollectingSink` 而不是测试自造 sink：它实现了 `take_pending`，
        // 因而反映生产路径——读线程能把应答取出并写回 PTY。
        let scanner =
            KittyGraphicsScanner::with_storage(storage, true, Box::new(CollectingSink::default()));

        // 读线程拿走了 `term`，断言侧自己留一份 `Arc` 句柄。
        let term_for_assert = term.clone();
        let parts = LocalReader::spawn(pty, term, listener, Box::new(scanner), bounds.into())
            .expect("启动读线程失败");

        // 应答必须真的落到子进程：只有写回 PTY，子进程的 read 才能拿到。
        let read_back = || -> String {
            let t = term_for_assert.lock();
            let mut out = String::new();
            for cell in t.grid().display_iter() {
                out.push(cell.cell.c);
            }
            out
        };

        // 等应答的字节回显到网格上（子进程在 sleep 期间 tty 仍回显）。
        let done = wait_until(
            || read_back().contains("Gi=4207;OK"),
            Duration::from_secs(10),
        );
        assert!(
            done,
            "应答未写回 PTY：网格里应出现 `Gi=4207;OK`（terminal-browser 的探测判据），实际：{:?}",
            read_back()
        );

        // 关键断言：应答的字节**到达了子进程的 tty**。
        //
        // `Gi=4207;OK` 是 terminal-browser `graphicsReply()` 用来判定
        // "supported" 的关键子串，钉死它就钉死了兼容性契约：
        // 修复前（应答只在 `KittyReplySink` 缓冲里等 UI 线程 flush）
        // 网格里永远不会有这串，探测必然超时判 unsupported。
        let grid = read_back();
        assert!(
            grid.contains("Gi=4207;OK"),
            "应答未回显到子进程 tty，实际：{grid:?}"
        );

        parts.reader.sender().shutdown();
    }

    /// 本地路径下模式 1016 的探测应答必须回写 PTY。
    ///
    /// 这是 terminal-browser 探测像素级鼠标的判据：置位后 `\x1b[?1016$p`
    /// 必须答 `\x1b[?1016;2$y`（已识别/置位）。修复前 1016 不在 vte 的模式
    /// 枚举里，既不会真正置位、探测也被答成 `0`（不识别），客户端据此判定
    /// 像素级命中测试不可用而退回单元格粒度。
    ///
    /// 断言方式与 `kitty_query_probe_reply_round_trips_to_child` 相同：让
    /// 子进程打印探测序列后挂住，靠 tty 回显把应答字节送回网格。
    ///
    /// 有意的顺序：先探测（应答 1）→ 再置位 → 再探测（应答 2）。这覆盖了
    /// "状态真的被跟踪"而不只是"回了固定值"。
    #[test]
    fn mode_1016_probe_reply_round_trips_to_child() {
        use crate::terminal::kitty::CollectingSink;
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let bounds = crate::terminal::TerminalBounds::default();

        let script = concat!(
            r"printf '\033[?1016$p';",
            r"sleep 0.5;",
            r"printf '\033[?1016h';",
            r"sleep 0.5;",
            r"printf '\033[?1016$p';",
            r"sleep 3",
        );

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));
        // 共享状态句柄：UI 线程侧的 `Terminal::sgr_pixels_active()` 读的就是
        // 这一份。这里手动持有它，用来断言「读线程写的」和「UI 线程读的」
        // 确实是同一个 `Arc`——`build_local_terminal` 曾经在这里错误地新建
        // 了一个 `Arc`，导致模式置位永远传不到鼠标报文编码处。
        let shared_1016 = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scanner =
            KittyGraphicsScanner::with_storage(storage, true, Box::new(CollectingSink::default()))
                .with_mode_1016_shared(shared_1016.clone());

        let term_for_assert = term.clone();
        let parts = LocalReader::spawn(pty, term, listener, Box::new(scanner), bounds.into())
            .expect("启动读线程失败");

        let read_back = || -> String {
            let t = term_for_assert.lock();
            let mut out = String::new();
            for cell in t.grid().display_iter() {
                out.push(cell.cell.c);
            }
            out
        };

        // 置位后的那次探测应答 `2` 是关键判据。它出现即证明：
        // (1) `?1016h` 被扫描器看见并置位；(2) 探测应答写回了 PTY。
        let done = wait_until(
            || read_back().contains("?1016;2$y"),
            Duration::from_secs(10),
        );
        let grid = read_back();
        assert!(
            done,
            "置位后的探测应应答 `?1016;2$y` 并回显到子进程 tty，实际：{grid:?}"
        );

        // 跨线程可见性：读线程观察到置位后，UI 线程读到的共享标志必须为真，
        // 否则鼠标报文仍会按单元格坐标编码（模式生效但行为不生效）。
        assert!(
            shared_1016.load(std::sync::atomic::Ordering::Relaxed),
            "读线程置位 1016 后，UI 线程侧的共享标志也必须为真"
        );

        // 反向断言：绝不出现「不识别」的 `0`（修复前的行为）。
        assert!(
            !grid.contains("?1016;0$y"),
            "1016 是已识别模式，绝不能答 `0`（不识别），实际：{grid:?}"
        );

        parts.reader.sender().shutdown();
    }

    /// 启动一个"打印探测序列后挂住"的本地终端，返回断言所需的句柄。
    ///
    /// 与 `kitty_query_probe_reply_round_trips_to_child` 同一手法：子进程把 探测序列打印到自己的 stdout，之后保持存活，让行规程把应答回显到子进程
    /// 的 tty 上（= `term` 网格），从而证明应答真的写回了 PTY。
    fn spawn_probe_child(
        script: &str,
        initial_geometry: escape_scan::TermGeometry,
    ) -> (
        crate::terminal::local_pty::LocalReader,
        Arc<FairMutex<Term<ZedListener>>>,
    ) {
        use crate::terminal::kitty::CollectingSink;
        use crate::terminal::kitty::scanner::KittyGraphicsScanner;

        let bounds = crate::terminal::TerminalBounds::default();

        let options = alacritty_terminal::tty::Options {
            shell: Some(alacritty_terminal::tty::Shell::new(
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )),
            working_directory: None,
            drain_on_exit: true,
            env: std::collections::HashMap::new(),
        };

        let pty =
            alacritty_terminal::tty::new(&options, bounds.into(), 0).expect("创建本地 PTY 失败");
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let listener = ZedListener(events_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &bounds,
            listener.clone(),
        )));

        let storage = Arc::new(StdMutex::new(
            crate::terminal::kitty::storage::Storage::new(1024 * 1024),
        ));
        let scanner =
            KittyGraphicsScanner::with_storage(storage, true, Box::new(CollectingSink::default()));

        let term_for_assert = term.clone();
        let parts = LocalReader::spawn(pty, term, listener, Box::new(scanner), initial_geometry)
            .expect("启动读线程失败");

        (parts.reader, term_for_assert)
    }

    /// 读回网格上的可见字符（与 `kitty_query_probe_reply_round_trips_to_child`
    /// 的 `read_back` 同构）。
    fn grid_text(term: &Arc<FairMutex<Term<ZedListener>>>) -> String {
        let t = term.lock();
        let mut out = String::new();
        for cell in t.grid().display_iter() {
            out.push(cell.cell.c);
        }
        out
    }

    /// `CSI 16 t`（cell 像素尺寸查询）的完整往返。
    ///
    /// terminal-browser 的原生模块启动时发 `ESC[16t`，并据此推算宽高比；收不到
    /// 应答就退回假定的比例，画面被水平压扁。格式必须严格是 ghostty 的
    /// `\x1b[6;{cell_height};{cell_width}t` —— **高在前**。
    #[test]
    fn csi_16_t_probe_reply_round_trips_to_child() {
        // 用非方形 cell（9x18）以便区分字段顺序：顺序反了会回显成 `6;9;18t`。
        let geometry = escape_scan::TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        };

        let script = concat!(r"printf '\033[16t';", r"sleep 3");
        let (reader, term) = spawn_probe_child(script, geometry);

        let done = wait_until(
            || grid_text(&term).contains("6;18;9t"),
            Duration::from_secs(10),
        );

        let grid = grid_text(&term);
        assert!(
            done,
            "cell 尺寸应答未写回 PTY：网格里应出现 `6;18;9t`，实际：{grid:?}"
        );
        assert!(
            !grid.contains("6;9;18t"),
            "字段顺序反了（宽在前）：画面会被压扁，实际：{grid:?}"
        );

        reader.sender().shutdown();
    }

    /// `CSI > 0 q`（XTVERSION）的完整往返。
    ///
    /// grok pager 启动即发这条（其 `xtversion.rs`，探测发的是 `\x1b[>0q`），
    /// 而 vte-0.15 的 CSI `q` 分派只有 DECSCUSR（`'q'` 配 SP 中间字节）——
    /// 这条会落进 `unhandled!()` 且**一个字节都不回**，探测方只能等到超时、
    /// 把 deepterm 当成不认识的终端。
    ///
    /// 断言取应答里不含 ESC 的中段：回显走行规程，ESC 被 ECHOCTL 编成 `^[`。
    #[test]
    fn xtversion_probe_reply_round_trips_to_child() {
        let geometry = escape_scan::TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        };

        let script = concat!(r"printf '\033[>0q';", r"sleep 3");
        let (reader, term) = spawn_probe_child(script, geometry);

        let reply = escape_scan::xtversion_reply();
        assert!(
            reply.starts_with("\x1bP>|deepterm ") && reply.ends_with("\x1b\\"),
            "XTVERSION 应答须是 `DCS > | deepterm <版本> ST`，实际 {reply:?}"
        );
        // 只留可打印中段：回显走行规程，ESC 被 ECHOCTL 编成 `^[`。
        // `P>|` 是 DCS 头，留在断言里才能钉死「回的是 DCS 载荷」而非别的文本。
        let printable = reply
            .trim_start_matches('\x1b')
            .trim_end_matches("\x1b\\")
            .to_string();

        let done = wait_until(
            || grid_text(&term).contains(printable.as_str()),
            Duration::from_secs(10),
        );
        let grid = grid_text(&term);
        assert!(
            done,
            "XTVERSION 应答未写回 PTY：网格里应出现 `{printable}`，实际：{grid:?}"
        );

        reader.sender().shutdown();
    }

    /// DA2（`CSI > 0 c`）的完整往返。
    ///
    /// 不在这一层拦截的话，应答由 alacritty 的 `identify_terminal` 产出，版本号
    /// 是 `alacritty_terminal` 库的（`\x1b[>0;2600;1c`）——客户端会据此把
    /// deepterm 报成 alacritty。这里必须回 deepterm 自己的打包版本。
    #[test]
    fn da2_probe_reply_round_trips_to_child() {
        let geometry = escape_scan::TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        };

        let script = concat!(r"printf '\033[>0c';", r"sleep 3");
        let (reader, term) = spawn_probe_child(script, geometry);

        // 回显后 ESC 变 `^[`，取 `>0;{Pv};1c` 这段（Pv 见 `escape_scan`）。
        let printable = escape_scan::da2_reply()
            .trim_start_matches('\x1b')
            .to_string();

        let done = wait_until(
            || grid_text(&term).contains(printable.as_str()),
            Duration::from_secs(10),
        );
        let grid = grid_text(&term);
        assert!(
            done,
            "DA2 应答未写回 PTY：网格里应出现 `{printable}`，实际：{grid:?}"
        );
        assert!(
            !grid.contains(";2600;"),
            "应答里泄漏了 alacritty_terminal 的库版本：{grid:?}"
        );

        reader.sender().shutdown();
    }

    /// 几何在**任何 resize 之前**就可用：terminal-browser 的探测往往早于首次
    /// layout。这条测试全程不发 `LocalMsg::Resize`，几何完全来自构造时的注入。
    #[test]
    fn csi_16_t_answered_before_any_resize() {
        let geometry = escape_scan::TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        };

        let script = concat!(r"printf '\033[16t';", r"sleep 3");
        let (reader, term) = spawn_probe_child(script, geometry);

        let done = wait_until(
            || grid_text(&term).contains("6;18;9t"),
            Duration::from_secs(10),
        );
        assert!(
            done,
            "未发送任何 resize 时也应能应答（初始几何注入失效），实际：{:?}",
            grid_text(&term)
        );

        reader.sender().shutdown();
    }

    /// in-band size reports（DECSET 2048）：置位时立即上报，此后每次几何变化
    /// 都重新上报（对齐 ghostty `stream_terminal.zig` 的 `resize()`）。
    #[test]
    fn mode_2048_enable_reports_then_resize_rereports() {
        let geometry = escape_scan::TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        };

        let script = concat!(r"printf '\033[?2048h';", r"sleep 3");
        let (reader, term) = spawn_probe_child(script, geometry);

        // 阶段一：置位时立即上报当前几何（24x80, 432x720 像素）。
        let done = wait_until(
            || grid_text(&term).contains("48;24;80;432;720t"),
            Duration::from_secs(10),
        );
        assert!(
            done,
            "开启 2048 后应立即收到几何上报，实际：{:?}",
            grid_text(&term)
        );

        // 阶段二：resize 后重新上报（30x100, 540x900 像素）。
        reader.sender().resize(WindowSize {
            num_lines: 30,
            num_cols: 100,
            cell_width: 9,
            cell_height: 18,
        });

        let done = wait_until(
            || grid_text(&term).contains("48;30;100;540;900t"),
            Duration::from_secs(10),
        );
        assert!(
            done,
            "启用期间 resize 必须重新上报，实际：{:?}",
            grid_text(&term)
        );

        reader.sender().shutdown();
    }

    /// resize 下发的 cell 尺寸是**物理像素**，扫描器必须原样上报。
    ///
    /// Retina 上 `TerminalBounds::window_size(2.0)` 会把 9x18 的 cell 放大成
    /// 18x36，`CSI 16 t` 的应答也必须是 18x36 —— 若读线程还按逻辑点算
    /// （或把 scale 丢了），终端应用就会把 surface 建小一半，画面糊。
    #[test]
    fn csi_16_t_reports_physical_cells_after_scaled_resize() {
        // 初始几何故意用逻辑点（1x）：只有 resize 生效后才会变物理像素。
        let geometry = escape_scan::TermGeometry {
            rows: 24,
            cols: 80,
            cell_width: 9,
            cell_height: 18,
        };

        let script = concat!(r"sleep 1;", r"printf '\033[16t';", r"sleep 3");
        let (reader, term) = spawn_probe_child(script, geometry);

        // 模拟 UI 线程在 2× 显示下的 resize：cell 已被换算成 18x36。
        reader.sender().resize(WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 18,
            cell_height: 36,
        });

        let done = wait_until(
            || grid_text(&term).contains("6;36;18t"),
            Duration::from_secs(10),
        );
        assert!(
            done,
            "缩放后的 cell 应以物理像素 36x18 上报，实际：{:?}",
            grid_text(&term)
        );
        assert!(
            !grid_text(&term).contains("6;18;9t"),
            "上报的仍是逻辑点（缩放被丢掉）：{:?}",
            grid_text(&term)
        );

        reader.sender().shutdown();
    }

    // ===== NoticeOscSink：读线程把 OSC 旁路通知给 UI 线程 =====

    /// sink 必须把内容与**序列时刻光标**原样送进通知通道。
    ///
    /// 光标是这里唯一需要从读线程带走的上下文：`133;B` 的语义是"此刻光标即
    /// 输入区起点"，等 UI 线程处理通知时同一 chunk 的后续字节早已解析完。
    #[test]
    fn notice_osc_sink_forwards_payload_and_cursor() {
        use crate::terminal::kitty::scanner::OscSink;

        let (tx, rx) = async_channel::bounded::<LocalNotice>(4);
        let mut sink = NoticeOscSink::new(tx);

        sink.osc(b"777;notify;deepterm://cli-agent;{}", (3, 7));

        match rx.try_recv().expect("应收到一条 Osc 通知") {
            LocalNotice::Osc { content, cursor } => {
                assert_eq!(content, b"777;notify;deepterm://cli-agent;{}");
                assert_eq!(cursor, (3, 7), "序列时刻光标必须原样带上来");
            }
            other => panic!("通知类型不对：{other:?}"),
        }
    }

    /// 通道满时 sink 不 panic、不阻塞：丢一条通知即可。
    ///
    /// 字节本身已由扫描器按位置喂给 vte，所以丢通知不影响 vte 侧的那几类
    /// OSC（标题 / 超链接 / 调色板）；只有应用层的那几种少一次刷新。
    #[test]
    fn notice_osc_sink_drops_when_channel_is_full() {
        use crate::terminal::kitty::scanner::OscSink;

        // 容量 1，且先塞满。
        let (tx, rx) = async_channel::bounded::<LocalNotice>(1);
        let mut sink = NoticeOscSink::new(tx.clone());
        sink.osc(b"0;first", (0, 0));
        // 此时第二条必然 `try_send` 失败——只要求不 panic、不阻塞。
        sink.osc(b"0;second", (0, 0));

        let first = rx.try_recv().expect("第一条应已入队");
        assert!(matches!(first, LocalNotice::Osc { .. }));
        assert!(
            rx.try_recv().is_err(),
            "第二条应被丢弃，而不是挤掉已入队的通知"
        );
    }
}
