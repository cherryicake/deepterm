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
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use super::terminal::{Address, Input, State, TerminalHandle, ZedListenerShim};

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
    pub cwd: Option<std::path::PathBuf>,
    pub env: std::collections::HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

impl SpawnRequest {
    /// 用默认几何构造。
    pub fn new(shell: tty::Shell) -> Self {
        Self {
            shell,
            cwd: None,
            env: std::collections::HashMap::new(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }
}

/// 守护进程的终端表。
pub struct Registry {
    terminals: Mutex<BTreeMap<Address, Arc<TerminalHandle>>>,
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
            next_id: AtomicU64::new(1),
        }
    }

    /// 按地址取终端。
    pub fn get(&self, address: &str) -> Option<Arc<TerminalHandle>> {
        self.terminals
            .lock()
            .expect("registry")
            .get(address)
            .cloned()
    }

    /// 所有终端，按地址排序。
    pub fn list(&self) -> Vec<Arc<TerminalHandle>> {
        self.terminals
            .lock()
            .expect("registry")
            .values()
            .cloned()
            .collect()
    }

    /// 终端数量。
    pub fn len(&self) -> usize {
        self.terminals.lock().expect("registry").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 移除一个已经结束的终端。
    pub fn remove(&self, address: &str) -> bool {
        let mut map = self.terminals.lock().expect("registry");
        match map.get(address) {
            // 还活着的不许移除：那会把程序变成孤儿。
            Some(handle) if handle.state() == State::Live => false,
            Some(_) => map.remove(address).is_some(),
            None => false,
        }
    }

    /// 回收所有已结束的终端，返回被移除的地址。
    pub fn reap(&self) -> Vec<Address> {
        let mut map = self.terminals.lock().expect("registry");
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
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let address = format!("t{id:x}");

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
            ..Default::default()
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

        let (input_tx, input_rx) = async_channel::bounded::<Input>(256);
        let handle = Arc::new(TerminalHandle::new(
            address.clone(),
            Vec::new(),
            request
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            now_ms(),
            term.clone(),
            input_tx,
            id,
        ));

        // poller 必须在 PTY 之前建好，且与线程同寿。
        let poller = Arc::new(Poller::new().map_err(|e| format!("无法创建 poller: {e}"))?);
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

        self.terminals
            .lock()
            .expect("registry")
            .insert(address, handle.clone());
        Ok(handle)
    }
}

/// 终端主循环：读写 PTY、喂 VT、广播输出，并把子进程退出转成 `Finished`。
fn run_terminal(
    mut pty: tty::Pty,
    poller: Arc<Poller>,
    input_rx: async_channel::Receiver<Input>,
    term: Arc<FairMutex<Term<ZedListenerShim>>>,
    handle: Arc<TerminalHandle>,
    shutdown: Arc<AtomicBool>,
) {
    let mut processor = Processor::<StdSyncHandler>::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut events = Events::with_capacity(std::num::NonZeroUsize::new(4).expect("非零"));

    // SAFETY（中文）：`pty` 在本函数结束前一直存活，注册到 poller 的 fd
    // 因而始终有效；退出前会显式 deregister。
    //
    // 只注册可读：PTY 主设备写侧恒可写，一并注册会在电平触发下让
    // `wait` 立即返回，形成忙循环。写入直接走 `pty.writer()`。
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
        // 100ms 超时让关停信号与输入消息有机会被看到，而不是死等 PTY。
        if poller
            .wait(&mut events, Some(std::time::Duration::from_millis(100)))
            .is_err()
        {
            break;
        }

        // 控制指令：输入、尺寸、关停。
        while let Ok(msg) = input_rx.try_recv() {
            match msg {
                Input::Bytes(bytes) => {
                    if write_all_retry(pty.writer(), &bytes).is_err() {
                        break 'outer;
                    }
                }
                Input::Resize(size) => {
                    pty.on_resize(size);
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
                                    let chunk = &buf[..n];
                                    {
                                        let mut t = term.lock();
                                        processor.advance(&mut *t, chunk);
                                    }
                                    handle.broadcast(chunk);
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
                            let chunk = &buf[..n];
                            {
                                let mut t = term.lock();
                                processor.advance(&mut *t, chunk);
                            }
                            handle.broadcast(chunk);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(_) => break 'outer,
                    }
                }
                _ => {}
            }
        }
    }

    let _ = pty.deregister(&poller);
    handle.mark_finished(None);
}

/// 写满整块；被信号打断时重试。
fn write_all_retry(w: &mut impl std::io::Write, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match w.write(bytes) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "PTY 写入返回 0",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(e) => return Err(e),
        }
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
}
