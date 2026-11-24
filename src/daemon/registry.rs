//! 守护进程的终端注册表：地址分配、spawn、查找。
//!
//! 地址形如 `t1`、`t2`……，编号只增不复用。客户端把地址记进会话文件，
//! 重开时拿它来找回同一个终端——编号复用会让"接回"变成"接错人"。
//!
//! # 为什么一个终端只用一个线程
//!
//! `tty::Pty` 的读写端点、子进程事件都挂在同一个 `Poller` 上（注册时的
//! token 由 alacritty 内部决定，调用方不能自选）。拆成"读线程 + 写线程"
//! 会让两边都要碰 `Pty`，只能加锁，而读是阻塞的——锁一加就把写饿死了。
//! 所以按 alacritty 自己的模型来：一个线程、一个 poller，读写与子进程
//! 事件都在里面轮转。

use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{OnResize as _, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;
use alacritty_terminal::tty::{ChildEvent, EventedPty as _, EventedReadWrite as _};
use alacritty_terminal::vte::ansi::{Handler as _, Processor, StdSyncHandler};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use super::terminal::{Address, Input, State, TerminalHandle, ZedListenerShim};
use crate::terminal::escape_scan::PtyScanner as _;

/// 默认 scrollback 行数。
///
/// 守护进程不知道客户端的设置（它先于任何窗口启动），取一个够用的固定值。
/// 客户端恢复时看到的回滚就是这么多——与 herdr 的 `history: 2000` 同量级。
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// 默认终端几何。客户端挂接后会立刻按真实尺寸 resize，这只是初值。
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// 读端在 poller 上的 token。
///
/// 必须与 alacritty 内部取值一致：`Pty::register` 会**覆盖**调用方传入的
/// `event.key`，所以这里不能自选。unix 上是 0/1（`tty::unix` 里
/// `PTY_READ_WRITE_TOKEN` 是 `pub(crate)`，取不到，只能照抄；
/// `local_pty.rs` 里已有同样的处理）。
#[cfg(unix)]
const PTY_READ_WRITE_TOKEN: usize = 0;
#[cfg(unix)]
const PTY_CHILD_EVENT_TOKEN: usize = 1;

#[cfg(windows)]
const PTY_READ_WRITE_TOKEN: usize = alacritty_terminal::tty::PTY_READ_WRITE_TOKEN;
#[cfg(windows)]
const PTY_CHILD_EVENT_TOKEN: usize = alacritty_terminal::tty::PTY_CHILD_EVENT_TOKEN;

/// 新建终端的参数。
#[derive(Clone, Debug)]
pub struct SpawnRequest {
    pub shell: tty::Shell,
    /// 实际执行的 shell 程序路径（`<spawn.shell>` 的程序部分）。
    ///
    /// `tty::Shell` 的字段是 `pub(crate)`，registry 读不到；而
    /// [`TerminalHandle`] 需要它来回答"这条终端的 shell 叫什么"——见
    /// [`TerminalMeta::shell`]。由 `server::handle_spawn` 在组装请求时
    /// 一并填好，**与实际执行的程序同源**，不在这里重算一次。
    pub shell_program: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: std::collections::HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    /// core 指定的全局地址。多 holder 时地址必须由 core 统一分配；holder
    /// 自己的计数器各从 `t1` 开始，继续用它会出现两个同名终端。
    ///
    /// 旧 core 不带这个字段（`None`），仍按 holder 本地计数分配。
    pub address: Option<Address>,
}

impl SpawnRequest {
    /// 用默认几何构造。
    pub fn new(shell: tty::Shell) -> Self {
        Self {
            shell,
            shell_program: None,
            cwd: None,
            env: std::collections::HashMap::new(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            address: None,
        }
    }
}

/// 守护进程的终端表。
pub struct Registry {
    terminals: Mutex<BTreeMap<Address, Arc<TerminalHandle>>>,
    /// 正在创建、尚未就绪的地址（`spawn` 的预留）。
    ///
    /// 与 `terminals` 分开两把锁会引入"预留成功、登记前 `list` 看不到"
    /// 的中间态——那与旧行为一致，但**必须**保证释放与转正都是原子的，
    /// 否则并发 `spawn` 会把同一个地址建两次。
    reserved: Mutex<std::collections::BTreeSet<Address>>,
    next_id: AtomicU64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            terminals: Mutex::new(BTreeMap::new()),
            reserved: Mutex::new(std::collections::BTreeSet::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// 按地址取终端。
    pub fn get(&self, address: &str) -> Option<Arc<TerminalHandle>> {
        self.terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(address)
            .cloned()
    }

    /// 所有终端，按地址排序。
    pub fn list(&self) -> Vec<Arc<TerminalHandle>> {
        self.terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// 终端数量。
    pub fn len(&self) -> usize {
        self.terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 移除一个已经结束的终端。
    pub fn remove(&self, address: &str) -> bool {
        let mut map = self
            .terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get(address) {
            // 还活着的不许移除：那会把程序变成孤儿。
            Some(handle) if handle.state() == State::Live => false,
            Some(_) => map.remove(address).is_some(),
            None => false,
        }
    }

    /// 结束一个终端里的程序（不管它是否还活着）。
    ///
    /// 与 [`Self::remove`] 的差别：`remove` 只清掉**已经结束**的记录，
    /// 相当于"打扫"；这个是**主动结束**一个还在跑的程序，是用户点了
    /// "关掉这个标签"的意思。两者不能合并——把 `remove` 改成能杀活终端，
    /// 一次误调用就会把用户的编译任务干掉。
    pub fn close(&self, address: &str) -> bool {
        match self.get(address) {
            Some(handle) => {
                handle.request_close();
                true
            }
            None => false,
        }
    }

    /// 回收所有已结束的终端，返回被移除的地址。
    pub fn reap(&self) -> Vec<Address> {
        let mut map = self
            .terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dead: Vec<Address> = map
            .iter()
            .filter(|(_, h)| h.state() == State::Finished)
            .map(|(a, _)| a.clone())
            .collect();
        for address in &dead {
            map.remove(address);
        }
        dead
    }

    /// 起一个新终端。
    ///
    /// 失败时返回错误字符串，调用方把它回报给客户端——**不要**让守护进程
    /// 因为一个起不来的 shell 就退出，那会把所有正在跑的终端一起带走。
    pub fn spawn(&self, request: SpawnRequest) -> Result<Arc<TerminalHandle>, String> {
        let (id, address) = match request.address.as_deref() {
            Some(address) => {
                let id = parse_address_id(address)
                    .ok_or_else(|| format!("无效的终端地址: {address}"))?;
                // **先预留地址。**
                //
                // 控制请求各自跑在自己的线程上，而地址可以来自请求。旧写法
                // 在这里只 `contains_key`，真正的 `insert` 要等到 PTY、
                // 终端线程都建好之后——两个并发 spawn 会同时通过检查，
                // 第二个 `insert` 覆盖第一个 handle，而第一个 PTY 从此对
                // `close`/`remove`/`reap` 不可达，泄漏一个活着的子进程。
                //
                // 预留与登记都发生在 `terminals` 这把锁里（`reserved` 只是
                // "还没就绪"的旁注），所以不会与 `list` 的语义打架。
                if !self.reserve_address(address) {
                    return Err(format!("终端地址已被占用: {address}"));
                }
                // 本地计数器也要越过它，避免后续未指定地址的 spawn 撞车。
                //
                // `saturating_add`：地址来自请求（`t<hex>`），对端可以发
                // `tffffffffffffffff`。`id + 1` 会在 debug 下 panic、release
                // 下回绕成 0，之后新地址全和 `t0` 撞车。
                self.next_id
                    .fetch_max(id.saturating_add(1), Ordering::Relaxed);
                (id, address.to_string())
            }
            None => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                (id, format!("t{id:x}"))
            }
        };

        // 从这里开始每一步都可能失败，失败必须把预留的地址还回去，
        // 否则这个地址会永远"被占用"而没有任何终端存在。
        let result = self.spawn_reserved(&request, id, &address);
        if result.is_err() {
            self.release_address(&address);
        }
        result
    }

    /// 为 `Some(address)` 的 spawn 预留地址；已经有人（或已经就绪）时返回 false。
    ///
    /// 预留与登记共用 `terminals` 这把锁的同一份状态：`reserved` 只记录
    /// "正在创建、还没有 handle 可用"的地址，`list`/`get` 仍然看不到它们
    /// ——与旧行为一致（旧代码也是全部准备好之后才 insert）。
    ///
    /// 加锁顺序固定为 `reserved → terminals`，与 [`Registry::spawn_reserved`]
    /// 的"转正"路径一致；别处不会同时拿这两把锁，因此不会形成 AB/BA。
    fn reserve_address(&self, address: &str) -> bool {
        let mut reserved = self
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let taken = self
            .terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(address);
        // 已登记 或 已被别人预留，都算冲突。
        !taken && reserved.insert(address.to_string())
    }

    /// 归还预留（创建失败时）。
    fn release_address(&self, address: &str) {
        self.reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(address);
    }

    /// 真正建终端；`address` 已由 [`Registry::reserve_address`] 预留。
    fn spawn_reserved(
        &self,
        request: &SpawnRequest,
        id: u64,
        address: &str,
    ) -> Result<Arc<TerminalHandle>, String> {
        let size = WindowSize {
            num_lines: request.rows,
            num_cols: request.cols,
            cell_width: 0,
            cell_height: 0,
        };
        let options = tty::Options {
            shell: Some(request.shell.clone()),
            working_directory: request.cwd.clone(),
            env: request.env.clone(),
            drain_on_exit: false,
        };

        let pty = tty::new(&options, size, 0).map_err(|e| format!("无法创建 PTY: {e}"))?;
        let (event_tx, _event_rx) = std::sync::mpsc::channel();
        let listener = ZedListenerShim(event_tx);
        let bounds = TermBounds {
            cols: request.cols,
            rows: request.rows,
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config(DEFAULT_SCROLLBACK),
            &bounds,
            listener,
        )));

        // 守护进程侧的 kitty 图像存储。
        //
        // 与客户端各存一份：客户端那份只服务于当前这次挂接，窗口关掉就没了，
        // 而程序以为图还在。守护进程留一份，attach 时才能重放（见
        // `kitty::replay`）。上限与客户端一致，避免"守护进程先淘汰、
        // 客户端还在显示"的不一致。
        let kitty_limit = crate::terminal::terminal::KITTY_IMAGE_BUDGET;
        let kitty: crate::terminal::kitty::exec::SharedStorage = std::sync::Arc::new(
            std::sync::Mutex::new(crate::terminal::kitty::storage::Storage::new(kitty_limit)),
        );

        // poller 必须先建：`TerminalHandle` 要用它做"输入到达即唤醒"
        // （见 `TerminalHandle::waker`）。它同时与终端线程同寿。
        let poller = Arc::new(Poller::new().map_err(|e| format!("无法创建 poller: {e}"))?);

        let (input_tx, input_rx) = async_channel::bounded::<Input>(256);
        let handle = Arc::new(TerminalHandle::new(
            address.to_string(),
            // holder 侧的 argv 只用于回答"这条终端的 shell 叫什么"
            // （`TerminalMeta::shell`）：客户端在 GUI 重启后拿不到它。
            request.shell_program.clone().into_iter().collect(),
            request
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            now_ms(),
            term.clone(),
            input_tx,
            id,
            kitty,
            kitty_limit > 0,
            // shell 自己的进程组：`tcgetpgrp` 与它比对才能区分
            // "前台是 shell" 和 "前台是 opencode"。
            Some(pty.child().id() as i32),
            Some(poller.clone()),
        ));

        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_handle = handle.clone();
        let thread_term = term.clone();
        let thread_poller = poller.clone();
        std::thread::Builder::new()
            .name(format!("daemon-{address}"))
            .spawn(move || {
                run_terminal(
                    pty,
                    thread_poller,
                    input_rx,
                    thread_term,
                    thread_handle,
                    shutdown,
                );
            })
            .map_err(|e| format!("无法启动终端线程: {e}"))?;

        // 预留转正与释放是同一个临界区：中间不能让 `list` 看到"既不在
        // reserved 也不在 terminals"的地址。
        let mut reserved = self
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reserved.remove(address);
        self.terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(address.to_string(), handle.clone());
        drop(reserved);
        Ok(handle)
    }
}

/// 解析 `t<hex>` 形式的终端地址。
pub fn parse_address_id(address: &str) -> Option<u64> {
    let hex = address.strip_prefix('t')?;
    if hex.is_empty() {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

/// 终端主循环：读 PTY、喂 VT、广播输出，并把子进程退出转成 `Finished`。
///
/// # 为什么写输入不能阻塞这条线程
///
/// 这曾经是一次真实冻结的根因。旧实现把「读 PTY / 喂 VT / 广播」与
/// 「把输入写进 PTY」放在同一条线程里，写用的是无限重试（`WouldBlock`
/// 时 sleep 1ms 再来）。只要终端里的程序一时不读 stdin——TUI 正在整屏
/// 重绘、`grep` 在跑、`less` 刚启动还没进读取循环——PTY 的内核写缓冲就
/// 会满，这条线程卡在写里，**连带**它本来该做的读也停了。用户看到的就是
/// "滚一下整个界面冻住"：滚轮报文被当输入写进去，程序不读，写端堵死，
/// 输出再也读不出来，屏幕停在最后一帧。
///
/// 采样证据（修复前，holder 里的 `daemon-t1` 线程）：777 个样本中 703 个
/// 停在 `nanosleep`（就是那个 1ms 重试间隔）、72 个停在 `write`，而同一
/// 时刻 PTY 里还有输出没被读走。
///
/// 现在的分工：
/// - 读循环**永远**不阻塞在写上，只 `read` + 喂 VT + 广播；
/// - 输入进 `pending_write` 缓冲，每轮循环用 `write` 尽力写一次，
///   `WouldBlock` 就留着下一轮再写（下一轮由 100ms 超时或新的 PTY 输出
///   触发，最坏延迟 100ms，对输入来说完全可接受）；
/// - 缓冲有上限：程序彻底不读时（`yes` 之类卡住的场景）按字节丢最旧的，
///   而不是把守护进程的内存吃光。丢的是用户输入而不是输出，语义上更安全
///   ——输出才决定屏幕内容。
fn run_terminal(
    mut pty: tty::Pty,
    poller: Arc<Poller>,
    input_rx: async_channel::Receiver<Input>,
    term: Arc<FairMutex<Term<ZedListenerShim>>>,
    handle: Arc<TerminalHandle>,
    shutdown: Arc<AtomicBool>,
) {
    let mut processor = Processor::<StdSyncHandler>::new();
    // 从什么时候起 VT 解析器停在同步更新（`?2026h`）里。
    //
    // 守护进程这份 `Term` 是 attach 时 `snapshot()` 的数据源：它若卡在同步
    // 块里，重连/重绘拿到的永远是旧画面。vte 的超时不会自己到期，必须由
    // 这里按时间兜底（与客户端侧 `Terminal::release_stuck_sync` 同一策略，
    // 对齐 herdr 的 `MAX_HOLD`）。
    let mut sync_held_since: Option<std::time::Instant> = None;
    // 前台进程探测：PTY 的 master fd 在守护进程手里，客户端没有——所以
    // "Tab 上显示正在跑什么"只能由这里探、再推给客户端（见 `Kind::Meta`）。
    #[cfg(unix)]
    let master_fd = {
        use std::os::unix::io::AsRawFd as _;
        Some(pty.file().as_raw_fd())
    };
    #[cfg(not(unix))]
    let master_fd: Option<i32> = None;
    let mut last_probe: Option<super::terminal::TerminalMeta> = None;
    let mut last_meta_sent: Option<super::terminal::TerminalMeta> = None;
    let mut last_hook_revision = 0;
    let mut next_meta_probe = std::time::Instant::now();
    let mut buf = vec![0u8; 64 * 1024];
    let mut events = Events::with_capacity(std::num::NonZeroUsize::new(4).expect("非零"));
    // 待写入 PTY 的字节。见本函数文档：写不能阻塞读。
    let mut pending_write: Vec<u8> = Vec::new();
    // 连续多少轮一个字节都写不进去。用来退避轮询间隔（见
    // [`input_poll_timeout`]）。
    let mut write_stalls: u32 = 0;

    // 守护进程侧的 kitty 扫描器：把 APC 从流里摘出来，执行进共享存储，
    // 这样 attach 时才能重放图像。应答的分工见 `DaemonReplySink`。
    let replies = DaemonReplySink::new();
    let mut kitty = crate::terminal::kitty::scanner::KittyGraphicsScanner::with_storage(
        handle.kitty_storage(),
        handle.kitty_enabled(),
        Box::new(replies.clone()),
    );
    let (agent_sink, agent_state) = super::terminal::agent_hook_state();
    kitty.set_osc_sink(agent_sink);
    // 跨读被截断的 APC 残尾（大图分片必然跨 read）。
    let mut apc_pending: Vec<u8> = Vec::new();

    // SAFETY:`pty` 在本函数结束前一直存活，注册到 poller 的 fd
    // 因而始终有效；退出前会显式 deregister。
    //
    // 只注册可读：PTY 主设备写侧恒可写，一并注册会在电平触发下让
    // `wait` 立即返回，形成忙循环。写入直接走 `pty.writer()`。
    //
    // SAFETY: `pty` 在本函数结束前一直存活，注册到 poller 的 fd 因而始终
    // 有效；退出前会显式 deregister。
    if unsafe {
        pty.register(
            &poller,
            PollEvent::readable(PTY_READ_WRITE_TOKEN),
            PollMode::Level,
        )
    }
    .is_err()
    {
        handle.mark_finished(None);
        return;
    }

    'outer: loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        events.clear();
        // 有输入还在等写时把超时压短：否则用户输入最坏要等 100ms 才出去。
        // 正常情况下 pending 是空的，走 100ms 长超时（省 CPU）。
        let wait = input_poll_timeout(pending_write.is_empty(), write_stalls);
        if poller.wait(&mut events, Some(wait)).is_err() {
            break;
        }

        // 解析器卡在同步更新里了吗？超过上限就强制解冻。
        //
        // 每轮循环（最长 100ms）检查一次，粒度足够。
        if processor.sync_bytes_count() > 0 {
            let since = *sync_held_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= crate::terminal::terminal::MAX_SYNC_HOLD {
                log::warn!(
                    "终端 {} 的 VT 停在同步更新（?2026h）里已超过 {:?}，强制解冻",
                    handle.address,
                    crate::terminal::terminal::MAX_SYNC_HOLD
                );
                {
                    let mut t = term.lock();
                    processor.stop_sync(&mut *t);
                }
                sync_held_since = None;
                // 积压内容已经进网格：通知挂接者重新对齐画面。
                handle.mark_repaint();
            }
        } else {
            sync_held_since = None;
        }

        // 控制指令：输入、尺寸、关停。
        while let Ok(msg) = input_rx.try_recv() {
            match msg {
                Input::Bytes(bytes) => {
                    // **只排队，不写。** 真正的写在本轮末尾用非阻塞方式
                    // 尽力写一次（见 `flush_pending_input`）。在这里同步写
                    // 就是旧的冻结根因：程序一时不读 stdin，写缓冲一满，
                    // 这条读线程就卡死，输出跟着停摆。
                    pending_write.extend_from_slice(&bytes);
                    if pending_write.len() > MAX_PENDING_INPUT {
                        // 程序彻底不读（`yes` 卡住、TUI 卡在死循环之类）。
                        // 丢**最旧**的输入而不是输出：输出决定屏幕内容，
                        // 输入丢了最多是某几个按键没生效，而且这类场景下
                        // 用户真正在意的是能看见并 Ctrl-C。
                        let excess = pending_write.len() - MAX_PENDING_INPUT;
                        pending_write.drain(..excess);
                        log::debug!(
                            "终端 {} 的输入积压超过 {} 字节，丢弃最旧的 {excess} 字节",
                            handle.address,
                            MAX_PENDING_INPUT
                        );
                    }
                }
                Input::Resize(size) => {
                    pty.on_resize(size);

                    // **把几何交给扫描器。**
                    //
                    // TUI 靠 `CSI 16 t`（cell 像素尺寸）与 in-band size
                    // reports（DECSET 2048）算图像尺寸、图片缩放、布局比例。
                    // 扫描器没有几何时对这两类查询**静默不答**——程序只能
                    // 等到超时，然后按退化值（常见是 1×1 或按 80 列估）处理，
                    // 图片和布局就会错。
                    //
                    // 之前这里只调了 `pty.on_resize`，`kitty` 扫描器的
                    // geometry 永远是 `None`，所以 `CSI 16 t` 一直没有应答。
                    if size.num_cols > 0 && size.num_lines > 0 {
                        kitty.set_geometry(crate::terminal::escape_scan::TermGeometry {
                            rows: size.num_lines,
                            cols: size.num_cols,
                            cell_width: size.cell_width,
                            cell_height: size.cell_height,
                        });
                    }
                    // **必须同时 resize 本进程里的 VT**。
                    //
                    // 只做 `pty.on_resize` 的话：子进程看到的窗口变了、会按新
                    // 尺寸重绘，但守护进程这份 `Term` 仍是旧网格。后果分两种，
                    // 都很难查：
                    // - 新尺寸更小时，程序画的内容被旧网格裁掉；
                    // - 重连时 `snapshot` 按旧网格重放，客户端按新尺寸渲染，
                    //   于是 nvim / htop 的边框、状态栏全部错位。
                    //
                    // 这正是"重启 deepterm 后 TUI 布局全乱"的根因。
                    if size.num_cols > 0 && size.num_lines > 0 {
                        {
                            let mut t = term.lock();
                            t.resize(alacritty_terminal::term::test::TermSize::new(
                                size.num_cols as usize,
                                size.num_lines as usize,
                            ));
                        }
                        // resize 之后必须整屏重画推给挂接者。
                        //
                        // reflow 会把每一行的折行位置都改掉；客户端虽然也
                        // 自己 resize 了本地 VT，但两边对"内容落在哪一行"的
                        // 推断不可能完全一致——不重画，屏幕上就留着按旧列数
                        // 折行的残影，也就是"改窗口大小后 TUI 布局乱掉"。
                        // herdr 用 `repaint_pending` + `queue_resize_paints`
                        // 做同一件事。
                        // 只标记，不立刻生成快照：拖动窗口时 resize 是连续
                        // 的，逐次整屏序列化会让 UI 明显发卡。这一批事件读
                        // 完之后由 `flush_repaint` 合并成一次。
                        handle.mark_repaint();
                    }
                }
                Input::Shutdown => break 'outer,
            }
        }

        for event in events.iter() {
            match event.key {
                PTY_CHILD_EVENT_TOKEN => {
                    if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
                        // 退出前把残余输出读完，否则最后一屏会缺一块——
                        // `echo done` 后立刻 exit 的场景非常常见。
                        for _ in 0..16 {
                            match pty.reader().read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    feed_output(
                                        &buf[..n],
                                        &mut processor,
                                        &mut kitty,
                                        &mut apc_pending,
                                        &term,
                                        &handle,
                                    );
                                }
                                Err(_) => break,
                            }
                        }
                        let code = status.and_then(|s| s.code());
                        handle.mark_finished(code);
                        let _ = pty.deregister(&poller);
                        return;
                    }
                }
                PTY_READ_WRITE_TOKEN => {
                    if event.is_interrupt() || !event.readable {
                        continue;
                    }
                    match pty.reader().read(&mut buf) {
                        Ok(0) => break 'outer,
                        Ok(n) => {
                            feed_output(
                                &buf[..n],
                                &mut processor,
                                &mut kitty,
                                &mut apc_pending,
                                &term,
                                &handle,
                            );
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(_) => break 'outer,
                    }
                }
                _ => {}
            }
        }

        // 这一批事件处理完，把攒下的 resize 重绘合并成一次发出去。
        //
        // 放在这里而不是 resize 处直接发：拖动窗口时读线程一批能收到好几条
        // `Input::Resize`，每条都整屏序列化（遍历全部单元格 + 拼 SGR）就是
        // 卡顿的来源。合并之后一批只发一帧，画面照样跟上，CPU 也不再空转。
        handle.flush_repaint();

        // 输入按"尽力而为"写出。`WouldBlock` 时保留剩余字节，等下一轮
        // （新输出唤醒读线程，或轮询超时）继续——**绝不**在这里等。
        //
        // 这是那条冻结的修复点：读循环只要能继续跑，屏幕就会继续更新，
        // 程序不读 stdin 只是它自己的事，不该让整个终端看不见。
        let before = pending_write.len();
        match flush_pending_input(pty.writer(), &mut pending_write) {
            Ok(()) => {
                // 有任何进展就重置退避，下一轮继续用短超时把剩下的推出去。
                write_stalls = if pending_write.len() < before {
                    0
                } else {
                    write_stalls.saturating_add(1)
                };
            }
            Err(e) => {
                log::debug!("终端 {} 的输入写失败，停止该终端: {e}", handle.address);
                break 'outer;
            }
        }

        // 前台进程探测：低频（500ms）且只在变化时推送。
        //
        // 500ms 的理由：毫秒级的频率会把这条读循环拖住，而用户敲下
        // `opencode` 之后标签只要能在一拍内跟上就够了。整个探测只有两次
        // syscall（`tcgetpgrp` + 读 argv），不起子进程。
        let mut probe_changed = false;
        if std::time::Instant::now() >= next_meta_probe {
            next_meta_probe = std::time::Instant::now() + std::time::Duration::from_millis(500);
            // `None` = 查不了，**保持上一份快照**。当成"前台是 shell"会让
            // 每个短命子进程退出时标签都闪一下。
            if let Some(probed) = super::terminal::probe_terminal_meta(master_fd, &handle) {
                probe_changed = last_probe.as_ref() != Some(&probed);
                last_probe = Some(probed);
            }
        }

        // hook 状态要和前台探测合成后再广播。只在探测结果或 hook 版本变化时
        // 进入，避免每个 500ms 采样都发一帧相同的 Meta。
        let hook_revision = agent_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .revision();
        if last_probe.is_some()
            && (last_meta_sent.is_none() || probe_changed || hook_revision != last_hook_revision)
        {
            let hook = agent_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let meta = super::terminal::compose_terminal_meta(
                last_probe.clone().expect("刚判过 is_some"),
                &hook,
            );
            drop(hook);
            if last_meta_sent.as_ref() != Some(&meta) {
                handle.broadcast_meta(&meta);
                last_meta_sent = Some(meta);
            }
            last_hook_revision = hook_revision;
        }

        // 扫描器攒下的探测应答（DA2 / XTVERSION / `CSI 16 t` / 尺寸上报）
        // 回写给程序。与输入共用同一套非阻塞写路径：应答积压同样不能把
        // 读循环拖住。
        //
        // 图像类应答不在其中（见 `DaemonReplySink`）：那必须由客户端回答。
        let pending_replies = replies.take();
        if !pending_replies.is_empty() {
            pending_write.extend_from_slice(&pending_replies);
            if let Err(e) = flush_pending_input(pty.writer(), &mut pending_write) {
                log::debug!("终端 {} 的应答写失败，停止该终端: {e}", handle.address);
                break 'outer;
            }
        }
    }

    let _ = pty.deregister(&poller);
    handle.mark_finished(None);
}

/// 守护进程侧扫描器的应答去向。
///
/// 规则：**只转发非图像应答，图像应答一律丢弃**。
///
/// - 图像类（`\x1b_G…`）必须**只由客户端回答**：能显示图像的是客户端，
///   守护进程不渲染任何东西。两边都答会让程序（terminal-browser 一类会
///   门控探测的）收到重复的 `OK`，状态机随之错乱。这与 herdr 的分工一致
///   （`relay.rs`：图像由 viewer 应答）。
/// - 其余探测（DA2、XTVERSION、`CSI 16 t`、`DECSET 2048` 尺寸上报）照旧
///   由守护进程回答：它们是终端自身属性，与"有没有窗口"无关。不转发的话
///   一个在无窗口期间发出的探测会一直等到超时，程序据此把终端判成不支持。
#[derive(Clone)]
struct DaemonReplySink {
    /// 攒待回写的非图像应答。由读循环取走并走非阻塞写路径。
    pending: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl DaemonReplySink {
    fn new() -> Self {
        Self {
            pending: Default::default(),
        }
    }

    /// 取走攒下的应答。
    fn take(&self) -> Vec<u8> {
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }
}

impl crate::terminal::kitty::scanner::ReplySink for DaemonReplySink {
    fn reply(&mut self, bytes: &[u8]) {
        // kitty 图形应答：`ESC _ G … ESC \`。
        if bytes.starts_with(b"\x1b_G") {
            return;
        }
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 上限：应答很小，但一个病态程序可以疯狂探测。超了就丢，避免
        // 守护进程被一条终端拖爆内存。
        if guard.len() + bytes.len() <= MAX_PENDING_REPLIES {
            guard.extend_from_slice(bytes);
        }
    }

    fn images_changed(&mut self) {}
}

/// 待回写的探测应答上限（字节）。
const MAX_PENDING_REPLIES: usize = 64 * 1024;

/// 把一块 PTY 输出喂给 VT 与 kitty 扫描器，再原样广播给挂接者。
///
/// 三个动作的顺序有讲究：
/// 1. 扫描器按位置切开普通文本与 APC，文本立刻交 vte（这样 APC 的锚点
///    反映它前面文本的效果），图像进共享存储；
/// 2. 广播的是**原始字节**，不是扫描器过滤后的结果——客户端要自己看到
///    APC 才能建立它那份图像，也要自己看到探测序列才能用真实几何应答。
fn feed_output(
    chunk: &[u8],
    processor: &mut Processor<StdSyncHandler>,
    kitty: &mut crate::terminal::kitty::scanner::KittyGraphicsScanner,
    apc_pending: &mut Vec<u8>,
    term: &Arc<FairMutex<Term<ZedListenerShim>>>,
    handle: &TerminalHandle,
) {
    let mut guard = term.lock();
    {
        // 两个闭包都借用同一份 `Term`/`Processor`。扫描器是顺序调用它们的
        // （一次 emit 或一次 grid，绝不同时），借用检查器看不出来，所以用
        // `RefCell` 表达"运行时互斥"——真发生重入会 panic 而不是 UB。
        let ctx = std::cell::RefCell::new((&mut *processor, &mut *guard));
        let mut emit = |bytes: &[u8]| {
            if !bytes.is_empty() {
                let (processor, terminal) = &mut *ctx.borrow_mut();
                processor.advance(*terminal, bytes);
            }
        };
        let mut grid = || {
            let (_, terminal) = &mut *ctx.borrow_mut();
            let grid = terminal.grid();
            let point = grid.cursor.point;
            crate::terminal::escape_scan::GridState {
                cursor: (point.line.0, point.column.0 as u32),
                visible: crate::terminal::kitty::storage::VisibleRows::new(
                    grid.display_offset(),
                    grid.screen_lines(),
                ),
            }
        };
        // Kitty `C=` 语义：显示后光标移到图像右下角。必须在 APC 处理那一刻
        // 应用，否则同一 read 里 APC 之后的文本会覆盖图像。
        let mut move_cursor = |rows: u32, target_col: u32| {
            let (_, terminal) = &mut *ctx.borrow_mut();
            for _ in 0..rows {
                terminal.linefeed();
            }
            terminal.goto_col(target_col as usize);
        };
        kitty.scan(chunk, apc_pending, &mut emit, &mut grid, &mut move_cursor);
    }
    drop(guard);
    handle.broadcast(chunk);
}

/// 能一次写出的输入上限。
///
/// PTY 内核缓冲通常 64 KiB，写超出没有意义（必然 `WouldBlock`）。这里多给
/// 一点，让正常粘贴能在第一轮就写完。
const MAX_PENDING_INPUT: usize = 256 * 1024;

/// 这一轮读循环该等多久。
///
/// - 没有积压：100ms 长超时，省 CPU（空闲终端不该定时醒来）；
/// - 有积压且**有进展**：5ms，让剩下的字节尽快出去（贴手感）；
/// - 有积压且连续多轮写不进去：退避到 50ms。程序彻底不读 stdin 时
///   （`yes` 卡住、TUI 死循环）不能每 5ms 空转一次——那会把一个本该
///   安静的终端变成 CPU 热点，而积压的输入本来也写不出去。
fn input_poll_timeout(idle: bool, stalls: u32) -> std::time::Duration {
    /// 连续多少轮零进展之后开始退避。
    const STALLS_BEFORE_BACKOFF: u32 = 20;

    if idle {
        std::time::Duration::from_millis(100)
    } else if stalls >= STALLS_BEFORE_BACKOFF {
        std::time::Duration::from_millis(50)
    } else {
        std::time::Duration::from_millis(5)
    }
}

/// 尽力把 `pending` 写进 PTY，**不等待**。
///
/// 返回后 `pending` 里只剩"现在写不进去"的部分：
/// - `WouldBlock` / `Interrupted` → 保留剩余，下一轮再试；
/// - `Ok(0)` → 视为暂时写不进（Windows 的 `UnblockedWriter` 缓冲满时就是
///   这样返回），保留剩余；
/// - 其它错误 → 向上报，让读循环退出。
///
/// 关键性质：**它从不 sleep**。旧实现在这里 sleep 重试，把 PTY 读一起
/// 卡死了（见 `run_terminal` 的文档）。
fn flush_pending_input(w: &mut impl std::io::Write, pending: &mut Vec<u8>) -> std::io::Result<()> {
    let mut written = 0;
    while written < pending.len() {
        match w.write(&pending[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                pending.drain(..written);
                return Err(e);
            }
        }
    }
    if written > 0 {
        pending.drain(..written);
    }
    Ok(())
}

/// `Term` 需要的尺寸。
struct TermBounds {
    cols: u16,
    rows: u16,
}

impl Dimensions for TermBounds {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

fn term_config(scrollback: usize) -> alacritty_terminal::term::Config {
    alacritty_terminal::term::Config {
        scrolling_history: scrollback,
        default_cursor_style: alacritty_terminal::vte::ansi::CursorStyle {
            shape: alacritty_terminal::vte::ansi::CursorShape::Block,
            blinking: false,
        },
        osc52: alacritty_terminal::term::Osc52::CopyPaste,
        kitty_keyboard: true,
        ..Default::default()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **写不进去的输入必须留在缓冲里，绝不阻塞读循环。**
    ///
    /// 回归价值：这是"滚动 TUI 时整个界面冻住"的根因。旧实现把输入写进
    /// PTY 用的是无限重试 + sleep，而写与读在同一条线程上：程序一时不读
    /// stdin（TUI 正在重绘）时写缓冲一满，读线程就卡死，输出再也读不出来。
    /// 采样证据是 `daemon-t1` 线程 777 个样本里 703 个停在 `nanosleep`、
    /// 72 个停在 `write`。
    ///
    /// 这里用一个"永远 WouldBlock"的写端模拟"程序暂时不读"：函数必须
    /// 立刻返回、不睡眠、不动 `pending` 里的字节。
    #[test]
    fn a_blocked_pty_write_never_blocks_the_reader() {
        struct Blocked;
        impl std::io::Write for Blocked {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut pending = b"wheel-report".to_vec();
        let start = std::time::Instant::now();
        flush_pending_input(&mut Blocked, &mut pending).expect("WouldBlock 不算错误");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "绝不能在这里睡眠重试，实际耗时 {:?}",
            start.elapsed()
        );
        assert_eq!(
            pending, b"wheel-report",
            "写不进去的字节必须留着，等下一轮继续"
        );
    }

    /// 部分写出时只丢掉已经写出去的那部分。
    ///
    /// 回归价值：整段丢弃会让用户输入错位（后半截按键丢失），而整段保留
    /// 会重复发送已经生效的按键（比如重复回车）。
    #[test]
    fn a_partial_write_keeps_only_the_remainder() {
        /// 只肯写前 3 个字节，之后 `WouldBlock`。
        struct Short(usize);
        impl std::io::Write for Short {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
                }
                let n = buf.len().min(self.0);
                self.0 -= n;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut pending = b"abcdef".to_vec();
        flush_pending_input(&mut Short(3), &mut pending).expect("暂时写不进去不算错误");
        assert_eq!(pending, b"def", "已经写出的前缀不能再发一次");
    }

    /// 真正的写错误要吵出来：PTY 没了继续留着输入毫无意义。
    #[test]
    fn a_broken_pty_surfaces_the_error() {
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut pending = b"x".to_vec();
        assert!(flush_pending_input(&mut Broken, &mut pending).is_err());
    }

    /// 守护进程侧扫描器只转发**非图像**应答。
    ///
    /// 回归价值：这条区分很容易写错，而两边写错的后果不同——
    /// - 什么都不转发：无窗口期间程序发的 DA2 / `CSI 16 t` 会等到超时，
    ///   程序据此把终端判成"不支持"，之后不再试；
    /// - 全都转发（含图像 `OK`）：程序会收到守护进程与客户端两份应答，
    ///   terminal-browser 一类会门控探测的实现会因此打乱状态机。
    #[test]
    fn the_daemon_sink_forwards_probes_but_not_image_replies() {
        use crate::terminal::kitty::scanner::ReplySink as _;

        let sink = DaemonReplySink::new();
        let mut writer = sink.clone();
        // kitty 图形应答：必须丢掉，交给客户端。
        writer.reply(b"\x1b_Gi=1;OK\x1b\\");
        assert!(sink.take().is_empty(), "图像应答必须只由客户端回答");

        // 探测应答：必须转发，否则程序会把终端判成不支持。
        writer.reply(b"\x1b[>0;1;0c");
        writer.reply(b"\x1b[6;18;9t");
        assert_eq!(sink.take(), b"\x1b[>0;1;0c\x1b[6;18;9t".to_vec());

        // 取走之后是空的：不能重复回写。
        assert!(sink.take().is_empty());
    }

    /// 探测应答的积压要有上限：病态程序不能靠狂发查询把守护进程撑爆。
    #[test]
    fn probe_replies_are_bounded() {
        use crate::terminal::kitty::scanner::ReplySink as _;

        let sink = DaemonReplySink::new();
        let mut writer = sink.clone();
        let reply = vec![b'x'; 4096];
        for _ in 0..(MAX_PENDING_REPLIES / reply.len() + 8) {
            writer.reply(&reply);
        }
        let total = sink.take().len();
        assert!(
            total <= MAX_PENDING_REPLIES,
            "应答积压必须封顶，实际 {total} 字节"
        );
    }

    /// **输入投递必须立刻唤醒读线程。**
    ///
    /// 回归价值：这是"滚轮/按键延迟 67ms、手感卡顿"的根因。
    ///
    /// 读线程的主体是 `poller.wait(events, timeout)`；`write_input` 往通道
    /// 里 `try_send` 之后**不唤醒**它，输入就只能等 `wait` 自己超时
    /// （空闲终端 100ms）。实测：一条"收到输入就 echo 一行"的极简程序，
    /// 端到端中位延迟 67ms，且与输出大小无关——纯粹被 poll 节拍量化。
    ///
    /// 这条用例钉住"唤醒"这件事：把 poller 的 wait 放在一条线程上，主线程
    /// 调 `write_input`，断言 wait 在远早于超时的时间内返回。
    #[test]
    fn writing_input_wakes_the_terminal_thread_immediately() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let poller = Arc::new(Poller::new().expect("poller"));
        let (input_tx, _input_rx) = async_channel::bounded::<Input>(8);
        let listener = ZedListenerShim(std::sync::mpsc::channel().0);
        let term = Arc::new(FairMutex::new(Term::new(
            term_config(1),
            &TermBounds { cols: 20, rows: 4 },
            listener,
        )));
        let handle = Arc::new(TerminalHandle::new(
            "t1".into(),
            Vec::new(),
            None,
            0,
            term,
            input_tx,
            0,
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::terminal::kitty::storage::Storage::new(0),
            )),
            false,
            // 测试用手搭的 handle 没有真实 PTY 子进程。
            None,
            Some(poller.clone()),
        ));

        let woke = Arc::new(AtomicBool::new(false));
        let woke_in_thread = woke.clone();
        let poller_in_thread = poller.clone();
        let waiter = std::thread::spawn(move || {
            let mut events = Events::with_capacity(std::num::NonZeroUsize::new(1).unwrap());
            let start = Instant::now();
            // 用 100ms 超时（与空闲终端一致）：若没有被唤醒，这次 wait 会
            // 整整睡满，测试就能看出差别。
            let _ = poller_in_thread.wait(&mut events, Some(Duration::from_millis(100)));
            woke_in_thread.store(true, Ordering::SeqCst);
            start.elapsed()
        });

        // 稍微等它真的进 wait，再投递输入。
        std::thread::sleep(Duration::from_millis(20));
        handle.write_input(b"x".to_vec());

        let elapsed = waiter.join().expect("waiter");
        assert!(woke.load(Ordering::SeqCst), "wait 应该已经返回");
        assert!(
            elapsed < Duration::from_millis(60),
            "输入必须立刻唤醒读线程，而不是等 100ms 超时；实际 {elapsed:?}"
        );
    }

    /// 输入积压时的轮询间隔必须"空闲省 CPU、有进展抢速度、长期堵住则退避"。
    /// 回归价值：只做前两条的话，一个程序彻底不读 stdin 的终端会每 5ms
    /// 醒一次并把 256 KiB 缓冲扫一遍——把"本该安静的终端"变成 CPU 热点。
    #[test]
    fn the_input_poll_timeout_backs_off_when_writes_make_no_progress() {
        use std::time::Duration;

        assert_eq!(input_poll_timeout(true, 0), Duration::from_millis(100));
        // 有积压、刚写过一点：尽快把剩下的推出去。
        assert_eq!(input_poll_timeout(false, 0), Duration::from_millis(5));
        assert_eq!(input_poll_timeout(false, 5), Duration::from_millis(5));
        // 连续多轮零进展：退避，但别退太长（程序一旦恢复读就要立刻跟上）。
        assert_eq!(input_poll_timeout(false, 20), Duration::from_millis(50));
        assert_eq!(input_poll_timeout(false, 1000), Duration::from_millis(50));
    }

    /// 积压超过上限时要丢**最旧**的输入，而不是拒绝新输入。
    ///
    /// 回归价值：丢新的会让"卡住之后用户按的 Ctrl-C"也被丢掉——而 Ctrl-C
    /// 恰恰是那种场景下唯一想按的键。
    #[test]
    fn overflow_drops_the_oldest_input_not_the_newest() {
        let mut pending: Vec<u8> = vec![b'a'; MAX_PENDING_INPUT];
        // 模拟 `run_terminal` 里的截断逻辑。
        pending.extend_from_slice(b"CTRL-C");
        if pending.len() > MAX_PENDING_INPUT {
            let excess = pending.len() - MAX_PENDING_INPUT;
            pending.drain(..excess);
        }
        assert!(
            pending.ends_with(b"CTRL-C"),
            "最新输入必须留下，否则用户按 Ctrl-C 也救不回来"
        );
    }

    /// 空闲的注册表不应有终端，且 `is_empty` 与 `len` 一致。
    #[test]
    fn a_fresh_registry_is_empty() {
        let registry = Registry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.get("t1").is_none());
    }

    /// 地址必须自增且永不复用。
    ///
    /// 复用会让会话文件里记的地址指向另一个终端——用户重开 deepterm 时
    /// 看到的是别人的屏幕，比"没恢复"糟糕得多。
    #[test]
    fn addresses_are_never_reused() {
        let registry = Registry::new();
        let mut seen = Vec::new();
        let mut previous: u64 = 0;
        for _ in 0..5 {
            let id = registry.next_id.fetch_add(1, Ordering::Relaxed);
            let address = format!("t{id:x}");
            assert!(!seen.contains(&address), "地址 {address} 被复用了");
            seen.push(address);
            assert!(id > previous);
            previous = id;
        }
    }

    /// 移除不存在的终端应返回 false 而不是 panic。
    #[test]
    fn removing_an_unknown_terminal_is_a_no_op() {
        let registry = Registry::new();
        assert!(!registry.remove("t99"));
    }

    /// 空注册表回收不报错。
    #[test]
    fn reaping_an_empty_registry_returns_nothing() {
        assert!(Registry::new().reap().is_empty());
    }

    /// **同一个显式地址不能被两个并发 spawn 同时拿走。**
    ///
    /// 回归价值：旧实现只在建 PTY 之前 `contains_key`，真正的 `insert`
    /// 要等 PTY、终端线程都就绪——两个并发 spawn 会同时通过检查，第二个
    /// `insert` 覆盖第一个 handle，第一个 PTY 从此对 `close`/`remove`/`reap`
    /// 不可达，泄漏一个活着的子进程。
    #[test]
    fn an_explicit_address_cannot_be_reserved_twice() {
        let registry = Registry::new();
        assert!(registry.reserve_address("t7"), "第一次预留应当成功");
        assert!(
            !registry.reserve_address("t7"),
            "第二次预留必须失败，而不是覆盖第一个终端"
        );
        // 失败要能归还，否则这个地址永远不可用。
        registry.release_address("t7");
        assert!(registry.reserve_address("t7"), "归还后应当可以重新预留");
    }

    /// 预留中的地址不出现在 `list`/`len` 里（与"全部就绪才 insert"一致）。
    #[test]
    fn a_reserved_address_is_not_listed_until_ready() {
        let registry = Registry::new();
        assert!(registry.reserve_address("t3"));
        assert!(registry.is_empty(), "预留不等于已登记");
        assert!(registry.get("t3").is_none());
    }
}
