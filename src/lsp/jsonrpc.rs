//! JSON-RPC 2.0 over a language server's stdio pipes.
//!
//! 这一层刻意与 LSP 语义解耦：它只知道「怎么把一条 JSON 消息写进 stdin、
//! 怎么从 stdout 里切出一条消息、以及怎么把响应配对回挂起的请求」。
//! LSP 的方法名与参数类型留给 [`crate::lsp::client`]。
//!
//! 设计要点：
//! - **帧格式**是 LSP 的固定约定：`Content-Length: N\r\n\r\n` + N 字节 UTF-8
//!   载荷。头字段大小写不敏感，但真实服务器（尤其 Windows 上的）偶发
//!   `content-length`；解析时统一小写比较。
//! - **粘包/半包是常态**：一次 `read` 可能只拿到半个头，也可能一次拿到
//!   三条消息。所以读循环必须累积缓冲区、按已解析出的长度反复切分。
//! - **请求配对**用自增 id + `HashMap<id, oneshot>`。服务器发来的
//!   `id` 若不在表里（过期响应/服务器主动乱发），只记日志不 panic。
//! - **服务器主动通知**（如 `publishDiagnostics`）走独立 channel，
//!   与请求-响应流分离，避免消费者必须区分「响应」和「通知」。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context as _, Result, anyhow};
use async_channel::{Receiver, Sender, bounded};
use async_process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use futures::io::BufReader;
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use async_lock::Mutex as AsyncMutex;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一条 JSON-RPC 消息。
///
/// 请求/响应/通知三态共用 `id` 字段：`id` 为 `Some` 且带 `method` 是
/// 入站请求；`Some` 且带 `result`/`error` 是响应；`None` 是通知。
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// 服务器向客户端发起的请求（如 `workspace/configuration`、
    /// `window/workDoneProgress/create`）。必须回一个响应，否则部分服务器
    /// 会一直等下去。
    Request { id: u32, method: String, params: Value },
    /// 对客户端请求的应答。`result` 与 `error` 互斥，由 LSP 保证；
    /// 解析时两者都缺视为协议错误。
    Response {
        id: u32,
        result: Option<Value>,
        error: Option<ResponseError>,
    },
    /// 单向通知，无需应答（`publishDiagnostics` 等）。
    Notification { method: String, params: Value },
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ResponseError {}

/// 出站请求的原始报文：序列化后可直接写进 stdin。
#[derive(Debug, Serialize)]
struct OutgoingRequest<'a> {
    jsonrpc: &'static str,
    id: u32,
    method: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
}

/// 出站响应的原始报文（用于应答服务器的 `Request`）。
#[derive(Debug, Serialize)]
struct OutgoingResponse<'a> {
    jsonrpc: &'static str,
    id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ResponseError>,
}

#[derive(Debug, Serialize)]
struct OutgoingNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
}

/// 入站报文的判别结构。
///
/// 先反序列化成这个「扁平」结构判断种类，再由调用方把 `params`/
/// `result` 转成具体类型——避免为每种方法写一个枚举变体。
///
/// `result` / `error` 收成 `Value` 而不是 `Option<Value>`：LSP 里
/// `{"result": null}` 是**合法响应**（方法成功但无返回值，如
/// `textDocument/completion` 没有候选时 rust-analyzer 就这么回），
/// 而 `Option<Value>` 会把「字段不存在」和「字段是 null」混为一谈，
/// 导致把正常响应当成协议错误。是否存在由 `has_result` / `has_error`
/// 两个显式标志记录。
/// 入站报文的判别结构。
///
/// 先反序列化成这个「扁平」结构判断种类，再由调用方把 `params`/
/// `result` 转成具体类型——避免为每种方法写一个枚举变体。
///
/// `result` / `error` 的**存在性**不能用 `Option<T>` 表达：serde 会把
/// `{"result": null}` 和「没有 result 字段」都解析成 `None`，而前者是合法
/// 响应（方法成功但无返回值——`textDocument/completion` 没有候选时
/// rust-analyzer 就这么回）。所以直接看原始 JSON 对象的键。
#[derive(Debug, Deserialize)]
struct IncomingMessage {
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    /// 服务器可能用字符串 id（少见但不违规）；统一拒绝并记日志，
    /// 因为本实现只发数值 id。
    #[serde(default)]
    #[allow(dead_code)]
    jsonrpc: Option<String>,
}

/// 解析一帧：`Content-Length` 头 + 空行 + 载荷。
///
/// 返回 `(消息, 剩余未消费字节)`；缓冲区里不足一整帧时返回
/// `Ok(None)`，调用方继续读。
///
/// 头部字段按 `:` 切分并 `trim`，值大小写不敏感。载荷长度以头声明为准，
/// 缓冲区不足则判定为「半包」——不能按 UTF-8 提前截断。
fn parse_frame(buf: &[u8]) -> Result<Option<(Message, usize)>> {
    // 找头部结束的空行。LSP 规定用 \r\n，但容忍裸 \n。
    let header_end = find_header_end(buf).ok_or_else(|| {
        // 头部本身还不完整（连空行都没到）——不是错误，是数据不够。
        anyhow!("incomplete header")
    });

    let header_end = match header_end {
        Ok(pos) => pos,
        // 头部未结束有两种可能：数据不够，或头长到不合理（服务器出 bug
        // 狂喷无换行数据）。超过 64KB 还没见到空行就判定为协议错误，
        // 避免缓冲区无限增长。
        Err(_) if buf.len() > 64 * 1024 => {
            return Err(anyhow!("header exceeds 64KB without a blank line"));
        }
        Err(_) => return Ok(None),
    };

    let header = std::str::from_utf8(&buf[..header_end])
        .context("header is not valid UTF-8")?;
    let mut content_length: Option<usize> = None;
    for line in header.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0 || true);
        }
    }

    let len = content_length.context("missing or invalid Content-Length header")?;
    let body_start = header_end + header_terminator_len(buf, header_end);
    if buf.len() - body_start < len {
        // 半包：头完整但载荷没到齐。
        return Ok(None);
    }
    let body = &buf[body_start..body_start + len];
    let raw: Value = serde_json::from_slice(body)
        .with_context(|| format!("invalid JSON body: {}", String::from_utf8_lossy(body)))?;
    let msg: IncomingMessage = serde_json::from_value(raw.clone())
        .with_context(|| format!("invalid JSON-RPC message: {}", String::from_utf8_lossy(body)))?;

    // `result` / `error` 是否存在只能看键——见 `IncomingMessage` 的说明。
    let has_result = raw.get("result").is_some();
    let error = raw
        .get("error")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value::<ResponseError>(v.clone()).ok());

    let message = match (msg.id, msg.method) {
        (Some(id), Some(method)) => Message::Request {
            id,
            method,
            params: msg.params.unwrap_or(Value::Null),
        },
        (Some(id), None) => {
            if !has_result && error.is_none() {
                return Err(anyhow!("response {id} has neither result nor error"));
            }
            Message::Response {
                id,
                // 显式 null → `Value::Null`，调用方按「无返回值」处理。
                result: if has_result {
                    Some(raw.get("result").cloned().unwrap_or(Value::Null))
                } else {
                    None
                },
                error,
            }
        }
        (None, Some(method)) => Message::Notification {
            method,
            params: msg.params.unwrap_or(Value::Null),
        },
        (None, None) => return Err(anyhow!("message has neither id nor method")),
    };

    Ok(Some((message, body_start + len)))
}

/// 找头部结束位置（空行起点）。同时接受 `\r\n\r\n` 与 `\n\n`。
fn find_header_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        if buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && i + 3 < buf.len()
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some(i);
        }
    }
    None
}

/// 空行终止符的实际长度（`\r\n\r\n` 是 4，`\n\n` 是 2）。
fn header_terminator_len(buf: &[u8], header_end: usize) -> usize {
    if buf[header_end] == b'\r' { 4 } else { 2 }
}

/// 把一条消息编码成带头的帧字节。
fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(b"Content-Length: ");
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(payload);
    out
}

/// 挂起请求的应答通道。
type Pending = Arc<Mutex<HashMap<u32, Sender<Result<Value>>>>>;

/// 单个请求的最长等待时间。
///
/// 服务器可能死锁、卡在 IO、或干脆不实现某个方法。没有上限的话调用方的
/// future 永远不返回，pending 表也会一直留着那个条目。
///
/// 取 30s：足够覆盖大项目上的冷启动重活（rust-analyzer 首次
/// `cargo metadata` 可能十几秒），又不至于让卡死的请求挂太久。
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 关闭握手的等待上限（Zed 的 `SERVER_SHUTDOWN_TIMEOUT` 同为 5s）。
pub const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 一条到语言服务器的连接。
///
/// 内部结构全部 `Arc`，因为读写循环跑在后台任务里、而 `Connection`
/// 本身要被多个 UI 任务共享（每个请求都可能来自不同的 tab）。
pub struct Connection {
    /// 写端在异步 `Mutex` 里：多条消息必须整体写入，交错会破坏帧。
    ///
    /// 必须是**异步**锁而不是 `parking_lot` 的：写帧要跨 `await`，
    /// 而 `parking_lot::MutexGuard` 不是 `Send`，会让整个 future 失去
    /// `Send`，从而无法交给 `smol::spawn`。
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU32,
    /// 服务器主动通知的**订阅者列表**（广播）。
    ///
    /// 每个订阅者一份独立队列，`dispatch` 向**全部**订阅者各投一份。
    ///
    /// 为什么必须是广播而不是共享队列：一个服务器进程服务整个项目
    /// （`LspManager` 按「项目根 + 语言」复用），而每个打开的标签都要收
    /// 诊断。共享一个 `async_channel` 队列是 MPMC——每条消息只到**一个**
    /// 接收者，于是 A 文件的诊断会被 B 标签的监听器抢走，而那个监听器只认
    /// 自己的 uri，于是 `continue` 掉，**没有任何人处理这条诊断**。实测三个
    /// 标签里有一个的诊断被静默丢弃，表现为「诊断时有时无、多开几个文件后
    /// 基本失效」。
    ///
    /// 对照 VSCode：它的语言服务器通知走 `Emitter.fire()`，每个订阅者都拿到
    /// 一份；「这条 uri 不是我的」由订阅者自己判断，而不是由队列决定谁该收到。
    ///
    /// 存 `(Sender, Receiver)` 对而非只存 `Sender`：队列满时要丢**最旧**的一
    /// 条，而 `Sender` 没有出队接口，只有 `Receiver` 能 `try_recv`。持有
    /// Receiver 不会额外消费——只有分发路径在用它。
    subscribers: Arc<Mutex<Vec<Subscription>>>,
    /// 子进程句柄：drop 时用来 `kill`，避免僵尸进程。
    child: Arc<Mutex<Option<Child>>>,
    /// 读循环是否已经退出（进程关闭/崩溃）。置位后所有请求立即失败，
    /// 不必等到超时。
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// 服务器通过 `client/registerCapability` 动态注册的方法名集合。
    ///
    /// 某些能力（尤其 `workspace/didChangeWatchedFiles`）不在静态
    /// `InitializeResult.capabilities` 里，而是启动后由服务器动态注册。
    /// 不记录就无法判断「该不该发这个通知」。
    registered_methods: Arc<Mutex<std::collections::HashSet<String>>>,
    /// 服务器通过 `client/registerCapability` 注册的
    /// `workspace/didChangeWatchedFiles` **glob 模式**。
    ///
    /// 只记方法名是不够的：`didChangeWatchedFiles` 通知的是「你关心哪些路径
    /// 变了」，而服务器关心哪些路径由它的 `watchers` 列表给出。不按这个列表
    /// 过滤就会把工作区里**每一个**文件改动都推给服务器。
    ///
    /// 代价不是浪费流量，而是**服务器被迫重载整个工程**：rust-analyzer 收到
    /// `didChangeWatchedFiles` 会触发一次 workspace reload，而它自己在跑的
    /// `cargo check` / `cargo metadata` 不断写 `target/`，于是形成自持循环 ——
    /// 实测 19 分钟内 reload 了 401 次，期间所有请求都被回
    /// `-32801 content modified`，补全与 hover 因此全部失效，状态栏也在
    /// Ready 与 Fetching 之间反复翻转。
    ///
    /// 对照 Zed `lsp_store.rs:12289`：它用 `DidChangeWatchedFilesRegistrationOptions`
    /// 建出 `GlobSet`，发通知前逐个 `is_match()`，不匹配的直接丢弃。
    watched_globs: Arc<Mutex<Vec<String>>>,
}

/// 构造启动语言服务器的命令（不真正 spawn）。
///
/// 抽出来是为了让「子进程拿到了什么环境」可断言：`PATH` 拼错时
/// `Connection::spawn` 只会返回一个泛泛的 `NotFound`，看不出是缺二进制还是
/// 环境没给对（见 [`spawn_command_environment`] 的说明）。
pub fn spawn_command(program: &str, args: &[String]) -> Command {
    let mut command = Command::new(program);
    // 语言服务器是**常驻**子进程（不是跑一下就退的那种），所以 windows 上
    // 若不抑制控制台，用户会看到编辑器旁边长期挂着一个跑 rust-analyzer 的
    // 黑框。`CREATE_NO_WINDOW` 只影响是否分配可见控制台，管道照常。
    crate::utils::console::hide_console_window_async(&mut command);
    spawn_command_environment(&mut command);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// 给语言服务器子进程设上自拼的 `PATH`。
///
/// # 为什么必须自己拼
///
/// 从 Finder / Dock 启动时，本进程继承的是 launchd 的最小 `PATH`
/// （`/usr/bin:/bin:/usr/sbin:/sbin`），`~/.cargo/bin` 与 Homebrew 都不在其中。
/// rustup 装的 `rust-analyzer` 只在 `~/.cargo/bin`，于是按名字
/// `Command::new("rust-analyzer")` 直接 `NotFound`——现场就是状态栏的
/// 「LSP 失败: rust 服务器启动失败」，而同一台机器上用户在终端里敲
/// `rust-analyzer --version` 一切正常。差别只在启动方式。
///
/// 这份 `PATH` 也会被服务器自己的子进程继承：rust-analyzer 要调 `cargo`，
/// 拿不到 `~/.cargo/bin` 的话索引会失败（表现为「有诊断但补全不来」这类
/// 半残状态，比启动失败更难排查）。
///
/// 取不到主目录时保持继承行为：设一个残缺的 `PATH` 比不设更糟。
fn spawn_command_environment(command: &mut Command) {
    if let Some(path) = crate::utils::path::local_spawn_path() {
        command.env("PATH", path);
    }
}

impl Connection {
    /// 启动一个语言服务器进程并完成握手。
    ///
    /// `spawn` 失败（命令不存在）会以 `Err` 返回，调用方降级为「无 LSP」，
    /// 不阻塞编辑器。
    pub async fn spawn(program: &str, args: &[String]) -> Result<Self> {
        let mut command = spawn_command(program, args);

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn language server `{program}`"))?;

        let stdin = child.stdin.take().context("language server has no stdin")?;
        let stdout = child.stdout.take().context("language server has no stdout")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        // 通知走**广播**：订阅者列表初始为空，由每个打开的标签经
        // `Connection::subscribe` 注册（理由见字段文档）。
        let subscribers: Arc<Mutex<Vec<Subscription>>> = Arc::new(Mutex::new(Vec::new()));

        let conn = Self {
            stdin: Arc::new(AsyncMutex::new(stdin)),
            pending: pending.clone(),
            next_id: AtomicU32::new(1),
            subscribers: subscribers.clone(),
            child: Arc::new(Mutex::new(Some(child))),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            registered_methods: Arc::new(Mutex::new(std::collections::HashSet::new())),
            watched_globs: Arc::new(Mutex::new(Vec::new())),
        };

        // 读循环：独立于任何请求，寿命与连接相同。
        let pending2 = pending.clone();
        let closed2 = conn.closed.clone();
        let stdin2 = Arc::clone(&conn.stdin);
        let registered2 = Arc::clone(&conn.registered_methods);
        let globs2 = Arc::clone(&conn.watched_globs);
        smol::spawn(async move {
            let result = read_loop(stdout, stdin2, pending2, subscribers, registered2, globs2).await;
            if let Err(e) = result {
                log::error!("lsp read loop terminated: {e}");
            }
            closed2.store(true, Ordering::SeqCst);
            // 唤醒所有挂起请求，否则它们会一直挂到进程被回收。
            let mut pending = pending.lock();
            for (_, tx) in pending.drain() {
                let _ = tx.try_send(Err(anyhow!("language server closed")));
            }
        })
        .detach();

        Ok(conn)
    }

    /// 是否已断开（进程退出或读循环终止）。
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 子进程 pid（测试用来独立确认进程已被回收）。
    pub fn child_pid(&self) -> Option<u32> {
        self.child.lock().as_ref().map(|c| c.id())
    }

    /// 仍在等待应答的请求数（测试用来确认超时后没有泄漏）。
    pub fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }

    /// 服务器是否动态注册过某个方法（`client/registerCapability`）。
    pub fn is_registered(&self, method: &str) -> bool {
        self.registered_methods.lock().contains(method)
    }

    /// 服务器注册过的 `didChangeWatchedFiles` glob（空 = 没注册过）。
    pub fn watched_globs(&self) -> Vec<String> {
        self.watched_globs.lock().clone()
    }

    /// 这个路径变化是否值得通知服务器。
    ///
    /// 两道闸门，缺一不可：
    ///
    /// 1. **服务器注册过 glob 就按 glob 过滤**（对照 Zed `lsp_store.rs:12289`
    ///    的 `watched_paths.is_match()`）。服务器只关心 `Cargo.toml`、
    ///    `.cargo/config*`、`rust-project.json` 这些清单文件，把源码改动也
    ///    推过去只是浪费——而每个多余的清单文件事件都会让 rust-analyzer
    ///    重载整个工程。
    /// 2. **没注册过 glob 就退回排除构建产物目录**。rust-analyzer 在我们
    ///    声明 `didChangeWatchedFiles` 之前不会发注册项，但仍然会读我们推的
    ///    通知；此时若不过滤，`target/` 里 `cargo check` 写的每个中间文件都会
    ///    触发一次 reload，自持循环就是这么来的。
    pub fn should_notify_watched_file(&self, path: &std::path::Path) -> bool {
        let globs = self.watched_globs.lock().clone();
        if !globs.is_empty() {
            return globs
                .iter()
                .any(|g| crate::lsp::glob::glob_match(g, path));
        }
        !crate::lsp::glob::is_build_artifact(path)
    }

    /// 订阅服务器主动通知，返回本订阅者**专属**的句柄。
    ///
    /// 广播语义：每条通知都会投给**每一个**订阅者一份。调用方按自己的 uri
    /// 过滤即可——被过滤掉的只是「这份副本」，不影响别的订阅者。
    ///
    /// 句柄 drop 即退订（见 [`NotificationSub`]），标签关闭时必须让它离开
    /// 作用域，否则订阅者会泄漏。
    pub fn subscribe(&self) -> NotificationSub {
        let (tx, rx) = bounded(NOTIFY_QUEUE_CAPACITY);
        let mut subs = self.subscribers.lock();
        let id = subs.len() as u64 + 1;
        subs.push(Subscription {
            id,
            tx,
            rx: rx.clone(),
        });
        NotificationSub {
            rx,
            registry: Arc::clone(&self.subscribers),
            id,
        }
    }

    /// 发送一个请求并等待应答。
    ///
    /// `params` 是已经序列化好的 `serde_json::Value`；返回原始 `Value`
    /// 交给调用方转具体类型——这样新增一个方法不需要动这一层。
    ///
    /// **有界等待**：服务器不答（死锁、挂起、实现有 bug）时按
    /// [`REQUEST_TIMEOUT`] 超时返回 `Err`，并把自己的条目从 pending 表摘掉
    /// ——否则该条目会永远留在表里泄漏。
    pub async fn request(&self, method: &str, params: Option<&Value>) -> Result<Value> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT).await
    }

    /// [`Self::request`] 的可指定超时版本（shutdown 用更短的上限）。
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<&Value>,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err(anyhow!("language server is closed"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // 每个挂起请求一个 slot：响应到达后即关闭，因此 `recv` 拿到的一定
        // 是本 id 的应答。
        let (tx, rx) = bounded(1);
        self.pending.lock().insert(id, tx);

        let outgoing = OutgoingRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let payload = serde_json::to_vec(&outgoing).context("failed to serialize request")?;

        // 写入失败要从 pending 表摘掉，否则请求会挂死。
        if let Err(e) = self.write_frame(&payload).await {
            self.pending.lock().remove(&id);
            return Err(e);
        }

        // 有界等待：`or` 返回先完成的那一支（应答，或超时标记）。
        let result = match smol::future::or(
            async { Some(rx.recv().await) },
            async {
                smol::Timer::after(timeout).await;
                None
            },
        )
        .await
        {
            Some(Ok(value)) => value,
            // slot 被 drop（读循环退出时唤醒）→ 连接已断。
            Some(Err(_)) => Err(anyhow!("language server closed before response")),
            None => {
                // 超时：必须摘掉自己的条目，否则 pending 表永久泄漏。
                self.pending.lock().remove(&id);
                return Err(anyhow!(
                    "timed out after {timeout:?} waiting for `{method}` (id {id})"
                ));
            }
        };

        // `result` 为 `Value::Null` 是合法的（方法没有返回值），
        // 与「出错」由 `Message::Response` 的 `error` 字段区分。
        result
    }

    /// LSP 关闭握手：`shutdown` 请求 → `exit` 通知 → 回收子进程。
    ///
    /// 顺序与 Zed 的 `LanguageServer::shutdown` 一致：
    /// 1. 发 `shutdown` 请求并**有界等待**应答（服务器可能在忙，不能无限等）；
    /// 2. 无论应答与否都发 `exit` 通知（规范要求 exit 才算完成关闭）；
    /// 3. 关 stdin，宽限后 `kill` 并 `status()` 回收，避免留下孤儿进程。
    ///
    /// 幂等：重复调用只做一次。
    pub async fn shutdown_gracefully(&self) {
        // 已经关过就直接返回（`child` 已被取走）。
        if self.child.lock().is_none() {
            self.closed.store(true, Ordering::SeqCst);
            return;
        }

        // 1) shutdown 请求。失败（超时/连接断）不阻断后续步骤。
        let started = std::time::Instant::now();
        match self.request_with_timeout("shutdown", None, SHUTDOWN_TIMEOUT).await {
            Ok(_) => log::debug!("lsp: shutdown request acknowledged"),
            Err(e) => log::warn!("lsp: shutdown request did not complete cleanly: {e}"),
        }

        // 2) exit 通知。规范：服务器收到 exit 后应自行退出。
        if let Err(e) = self.notify("exit", None).await {
            log::debug!("lsp: exit notification not delivered: {e}");
        }

        // 3) 关 stdin + 回收子进程。
        self.closed.store(true, Ordering::SeqCst);
        // 先把子进程句柄**取出来**（释放锁），再 await：`parking_lot` 的
        // guard 不是 `Send`，跨 await 持着它会让整个 future 无法 spawn。
        let child = self.child.lock().take();
        if let Some(mut child) = child {
            drop(child.stdin.take());
            // 等服务器自行退出的预算与请求超时**共享**同一个上限：
            // 否则最坏情况是「请求等 5s + 回收再等 5s」= 2× 常量，
            // 退出路径被拖成 10s。
            let step = std::time::Duration::from_millis(50);
            while started.elapsed() < SHUTDOWN_TIMEOUT {
                match child.try_status() {
                    // 已退出：`status()` 收尸，避免僵尸。
                    Ok(Some(_)) => {
                        let _ = child.status().await;
                        log::debug!("lsp: language server exited after shutdown handshake");
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::debug!("lsp: error polling language server status: {e}");
                        break;
                    }
                }
                smol::Timer::after(step).await;
            }
            // 没自己退：强杀并收尸。
            let _ = child.kill();
            let _ = child.status().await;
            log::debug!("lsp: language server killed after shutdown timeout");
        }
    }

    /// 发送一个单向通知。
    pub async fn notify(&self, method: &str, params: Option<&Value>) -> Result<()> {
        if self.is_closed() {
            return Err(anyhow!("language server is closed"));
        }
        let outgoing = OutgoingNotification {
            jsonrpc: "2.0",
            method,
            params,
        };
        let payload = serde_json::to_vec(&outgoing).context("failed to serialize notification")?;
        self.write_frame(&payload).await
    }

    /// 应答服务器发来的请求。
    pub async fn respond(
        &self,
        id: u32,
        result: Option<&Value>,
        error: Option<&ResponseError>,
    ) -> Result<()> {
        let outgoing = OutgoingResponse {
            jsonrpc: "2.0",
            id,
            result,
            error,
        };
        let payload = serde_json::to_vec(&outgoing).context("failed to serialize response")?;
        self.write_frame(&payload).await
    }

    /// 写一帧：持锁整体写出并 flush。
    ///
    /// 不加 flush 的话，消息可能留在 `BufWriter` 里直到下一条——
    /// 表现为「服务器收不到最后的 didChange」。
    async fn write_frame(&self, payload: &[u8]) -> Result<()> {
        let frame = encode_frame(payload);
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&frame)
            .await
            .context("failed to write to language server stdin")?;
        stdin
            .flush()
            .await
            .context("failed to flush language server stdin")?;
        Ok(())
    }

    /// 关闭连接：执行 LSP 关闭握手（`shutdown` → `exit`）并回收子进程。
    ///
    /// 同步入口（可在 `Drop` 里调用）：握手本身是异步的，所以丢给后台任务
    /// 执行，本函数立刻返回。真正的收尸在 [`Self::shutdown_gracefully`] 里。
    pub fn shutdown(&self) {
        let conn = self.clone_for_shutdown();
        smol::spawn(async move {
            conn.shutdown_gracefully().await;
        })
        .detach();
    }

    /// 取一份只含关闭所需字段的轻量句柄。
    ///
    /// 关闭路径只需要写端、pending 表与子进程句柄，所以单独复制这几项。
    ///
    /// 订阅者列表是 `Arc`，共享即可——关闭时不需要再收通知。
    fn clone_for_shutdown(&self) -> Connection {
        Connection {
            stdin: Arc::clone(&self.stdin),
            pending: Arc::clone(&self.pending),
            next_id: AtomicU32::new(self.next_id.load(Ordering::SeqCst)),
            subscribers: Arc::clone(&self.subscribers),
            child: Arc::clone(&self.child),
            closed: Arc::clone(&self.closed),
            registered_methods: Arc::clone(&self.registered_methods),
            watched_globs: Arc::clone(&self.watched_globs),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self.child.lock().is_some() {
            self.shutdown();
        }
    }
}

/// 读循环：切帧 → 配对响应 / 转发通知。
async fn read_loop(
    stdout: ChildStdout,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: Pending,
    subscribers: Arc<Mutex<Vec<Subscription>>>,
    registered_methods: Arc<Mutex<std::collections::HashSet<String>>>,
    watched_globs: Arc<Mutex<Vec<String>>>,
) -> Result<()> {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);

    loop {
        // 先把缓冲区里能切出的帧全部消费掉，再读新数据——
        // 一次 read 拿到多条消息时避免反复进 read 系统调用。
        while let Some((msg, consumed)) = parse_frame(&buf)? {
            buf.drain(..consumed);
            // 记录/清除服务器动态注册的能力（在应答之前处理，保证
            // 「注册成功」与「我们记住了」是同一时刻）。
            if let Message::Request { method, params, .. } = &msg {
                record_dynamic_registration(&registered_methods, &watched_globs, method, params);
            }
            if let Some((id, reply)) = dispatch(msg, &pending, &subscribers) {
                // 就地应答服务器的主动请求：不应答它就会一直等。
                let payload = match &reply {
                    ResolvedReply::Value(v) => serde_json::to_vec(&OutgoingResponse {
                        jsonrpc: "2.0",
                        id,
                        result: Some(v),
                        error: None,
                    }),
                    ResolvedReply::Unimplemented => {
                        let err = ResponseError {
                            code: -32601,
                            message: "method not implemented".to_string(),
                            data: None,
                        };
                        serde_json::to_vec(&OutgoingResponse {
                            jsonrpc: "2.0",
                            id,
                            result: None,
                            error: Some(&err),
                        })
                    }
                };
                match payload {
                    Ok(payload) => {
                        let frame = encode_frame(&payload);
                        let t0 = std::time::Instant::now();
                        let mut guard = stdin.lock().await;
                        let waited = t0.elapsed();
                        if waited > std::time::Duration::from_millis(200) {
                            log::warn!(
                                "lsp: 应答服务器请求 id {id} 前等锁 {waited:?}（pending={}）",
                                pending.lock().len()
                            );
                        }
                        // 写入失败只记日志：服务器请求应答丢了不会损坏本地
                        // 状态，下一次请求照常。
                        if let Err(e) = guard.write_all(&frame).await {
                            log::warn!("lsp: failed to reply to server request {id}: {e}");
                        } else if let Err(e) = guard.flush().await {
                            log::warn!("lsp: failed to flush reply {id}: {e}");
                        } else {
                            log::debug!("lsp: replied to server request id {id}");
                        }
                    }
                    Err(e) => log::warn!("lsp: failed to serialize reply {id}: {e}"),
                }
            }
        }

        let mut chunk = [0u8; 8192];
        let n = reader
            .read(&mut chunk)
            .await
            .context("failed to read from language server stdout")?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 服务器主动请求的处理结果。
#[derive(Debug)]
enum ServerRequestReply {
    /// 直接回这个值（多数请求回 `null` 即可）。
    Value(Value),
    /// 回一个「方法未实现」错误。用于我们确实不该假装支持的请求，
    /// 让服务器立刻降级而不是等着。
    Unimplemented,
    /// 需要看请求参数才能决定回什么（`workspace/configuration`）。
    ///
    /// 规范要求**按请求项的顺序**逐项返回配置；回一个空数组会让服务器在
    /// 取第 i 项时越界，回单个 `null` 则类型不符。
    NeedsParams,
}

/// 服务器向我们发起的请求该怎么应答。
///
/// **必须应答**：JSON-RPC 的服务器会一直等到响应才继续。早期版本只记日志
/// 不应答，rust-analyzer 就反复重发 `window/workDoneProgress/create` 并卡住
/// 进度上报（日志里表现为几十条同样的请求）。
fn server_request_reply(method: &str) -> ServerRequestReply {
    match method {
        // 服务器想注册额外能力：接受（回 null 即表示成功）。
        "client/registerCapability" | "client/unregisterCapability" => {
            ServerRequestReply::Value(Value::Null)
        }
        // 进度令牌：接受，我们只是不显示进度 UI。
        "window/workDoneProgress/create" => ServerRequestReply::Value(Value::Null),
        // 拉取配置项：必须**按请求项逐项**应答（见 `NeedsParams`）。
        "workspace/configuration" => ServerRequestReply::NeedsParams,
        // 服务器要求写入文件（格式化到文件等）：我们不支持，明确报错，
        // 让服务器退回「通过响应体返回编辑」的路径。
        "workspace/applyEdit" => ServerRequestReply::Unimplemented,
        // 其余（自定义请求等）：回 null，让服务器继续。
        _ => ServerRequestReply::Value(Value::Null),
    }
}

/// 记录 `client/registerCapability` / `unregisterCapability` 的注册项。
///
/// `params.registrations[].method` 是要注册的方法名（如
/// `workspace/didChangeWatchedFiles`）。只有注册过的方法才应该发通知。
///
/// `workspace/didChangeWatchedFiles` 的注册项里还带 `registerOptions.watchers`
/// ——服务器**关心哪些路径**的 glob 列表。必须一并记下：发通知前要按它过滤，
/// 否则工作区里任何一个文件改动都会被推给服务器，而 rust-analyzer 收到就
/// 重载整个工程（理由见 [`Connection::watched_globs`]）。
fn record_dynamic_registration(
    set: &Mutex<std::collections::HashSet<String>>,
    watched_globs: &Mutex<Vec<String>>,
    request_method: &str,
    params: &Value,
) {
    let Some(items) = params.get("registrations").and_then(|r| r.as_array()) else {
        return;
    };
    let mut guard = set.lock();
    let mut globs = watched_globs.lock();
    for item in items {
        let Some(method) = item.get("method").and_then(|m| m.as_str()) else {
            continue;
        };
        match request_method {
            "client/registerCapability" => {
                guard.insert(method.to_string());
                if method == "workspace/didChangeWatchedFiles" {
                    // 累加而非覆盖：服务器可能分多次注册不同 glob，
                    // 而 `unregisterCapability` 按 id 撤销（见下）。
                    collect_watcher_globs(item, &mut globs);
                }
            }
            "client/unregisterCapability" => {
                guard.remove(method);
                if method == "workspace/didChangeWatchedFiles" {
                    // 注册项按 `id` 撤销。这里保守处理：清空全部 glob，
                    // 让过滤退回「按构建产物目录排除」的兜底（宁可多发，
                    // 也不能因为漏记而永远不发）。下一轮 register 会补回来。
                    globs.clear();
                }
            }
            _ => {}
        }
    }
}

/// 从一个 `workspace/didChangeWatchedFiles` 注册项里取出所有 watcher 的 glob。
///
/// 兼容两种写法（服务器实现不一）：
/// - `watchers[].globPattern` 为字符串（3.16 之前的形状）
/// - `watchers[].globPattern` 为 `{ baseUri, pattern }`（3.17 的相对模式）
fn collect_watcher_globs(item: &Value, out: &mut Vec<String>) {
    let Some(watchers) = item
        .get("registerOptions")
        .and_then(|o| o.get("watchers"))
        .and_then(|w| w.as_array())
    else {
        return;
    };
    for watcher in watchers {
        let Some(pattern) = watcher.get("globPattern") else {
            continue;
        };
        match pattern {
            Value::String(s) => out.push(s.clone()),
            Value::Object(o) => {
                if let Some(p) = o.get("pattern").and_then(|p| p.as_str()) {
                    out.push(p.to_string());
                }
            }
            _ => {}
        }
    }
}

/// 已解析、可直接发送的应答（写帧的一方只见到这两态）。
#[derive(Debug)]
enum ResolvedReply {
    Value(Value),
    Unimplemented,
}

/// 需要看参数才能决定的应答（目前只有 `workspace/configuration`）。
///
/// LSP 规范：`workspace/configuration` 的响应是**与请求 `items` 等长、按顺序
/// 对应**的数组；没有配置的项回 `null`。对照 Zed 的 `lsp_store.rs`：它遍历
/// `params.items` 逐项查配置（查不到的项留空，序列化后即 `null`）。
///
/// 之前恒回空数组：服务器若请求 N 项，取第 0 项就越界（Rust 之外的语言多会
/// 读到 `undefined`），拿到的不是「默认值」而是「没有这项」。
fn reply_for_params(method: &str, params: &Value) -> ServerRequestReply {
    match method {
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(|items| items.as_array())
                .map(|items| items.len())
                .unwrap_or(0);
            // 逐项回 `null` = 「没有额外配置，用你的默认值」。
            ServerRequestReply::Value(Value::Array(vec![Value::Null; count]))
        }
        _ => ServerRequestReply::Value(Value::Null),
    }
}

/// 分发一条入站消息：有 id 且在 pending 表里 → 应答；否则按通知转发。
///
/// 每个订阅者的通知队列上限。
///
/// 服务器狂推诊断时不能无限堆积内存；满了丢最旧的一条（见 [`broadcast`]）。
const NOTIFY_QUEUE_CAPACITY: usize = 256;

/// 把一条通知**广播**给所有订阅者。
///
/// 逐个投递到各自的队列。某个订阅者队列满时只丢它自己最旧的一条——**不能**
/// 因为某个订阅者积压就跳过它，那正是「A 文件的诊断被 B 的监听器抢走」这类
/// 错投的根源。
fn broadcast(msg: &Message, subscribers: &Mutex<Vec<Subscription>>) {
    let subs = subscribers.lock();
    for sub in subs.iter() {
        if sub.tx.is_full() {
            // 队列满：丢最旧的一条再塞新的。诊断是幂等的全量快照，保留最新的
            // 一份才是对的（丢新的会让 UI 停在过期诊断上）。
            let _ = sub.rx.try_recv();
        }
        let _ = sub.tx.try_send(msg.clone());
    }
}

/// 订阅者列表里的一项：id + 收发两端。
///
/// 之所以把 `Receiver` 也存在表里：队列满时要丢**最旧**的一条，而
/// `async_channel::Sender` 没有出队接口，只有 `Receiver` 能 `try_recv`。
struct Subscription {
    id: u64,
    tx: Sender<Message>,
    rx: Receiver<Message>,
}

/// 一个标签的通知订阅句柄。**drop 即退订**。
///
/// 退订必须由句柄的 `Drop` 显式做，不能靠「接收端是否 closed」判断：表里持有的
/// 那个 `Receiver` 克隆本身就让通道永远不会 closed（实测 `is_closed()` 恒为
/// false），订阅者会永久泄漏，每来一条通知都往一个没人消费的队列里塞一份。
pub struct NotificationSub {
    rx: Receiver<Message>,
    registry: Arc<Mutex<Vec<Subscription>>>,
    id: u64,
}

impl NotificationSub {
    /// 接收下一条通知；通道关闭返回 `Err`。
    pub async fn recv(&self) -> Result<Message, async_channel::RecvError> {
        self.rx.recv().await
    }

    /// 非阻塞取一条已到达的通知（队列空则 `Err`）。
    ///
    /// 只给「先把已到达的诊断扫干净再睡」的轮询循环用（见真机用例）：
    /// 那种循环靠「有没有待处理」决定是否再等，不能被一条通知永久阻塞。
    pub fn try_recv(&self) -> Result<Message, async_channel::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for NotificationSub {
    fn drop(&mut self) {
        self.registry.lock().retain(|s| s.id != self.id);
    }
}

/// 服务器的**主动请求**不进通知队列，而是直接就地应答——`dispatch` 拿不到
/// 写端（在 `Connection` 上），所以返回待应答项由读循环发送。
fn dispatch(
    msg: Message,
    pending: &Pending,
    subscribers: &Mutex<Vec<Subscription>>,
) -> Option<(u32, ResolvedReply)> {
    match msg {
        Message::Response { id, result, error } => {
            // 逐条记录到达的响应：这是区分「服务器根本没回」与「回了但没接上」
            // 的唯一依据。请求 id 是客户端与服务器**共用**的计数器，两侧都会
            // 自增，因此只有把到达的 id 全部打出来才能对齐。
            log::debug!(
                "lsp: ← response id={id} has_result={} has_error={} pending_before={}",
                result.is_some(),
                error.is_some(),
                pending.lock().len()
            );
            let tx = pending.lock().remove(&id);
            match tx {
                Some(tx) => {
                    let value = match error {
                        Some(e) => Err(anyhow!(e)),
                        None => Ok(result.unwrap_or(Value::Null)),
                    };
                    // 接收端已被 drop（请求被取消）时忽略即可。
                    let _ = tx.try_send(value);
                }
                None => log::warn!("lsp: response for unknown request id {id}"),
            }
        }
        Message::Request { id, method, params } => {
            // 必须应答：服务器会一直等。具体应答内容见 `server_request_reply`。
            log::debug!("lsp: server request {method} (id {id})");
            // 需要看参数的（`workspace/configuration`）在这里解析完，
            // 写帧的一方只会拿到可直接发送的两态。
            let reply = match server_request_reply(&method) {
                ServerRequestReply::NeedsParams => match reply_for_params(&method, &params) {
                    ServerRequestReply::Value(v) => ResolvedReply::Value(v),
                    ServerRequestReply::Unimplemented => ResolvedReply::Unimplemented,
                    // `reply_for_params` 不会返回 NeedsParams（不是递归的）。
                    ServerRequestReply::NeedsParams => ResolvedReply::Value(Value::Null),
                },
                ServerRequestReply::Value(v) => ResolvedReply::Value(v),
                ServerRequestReply::Unimplemented => ResolvedReply::Unimplemented,
            };
            return Some((id, reply));
        }
        Message::Notification { .. } => {
            broadcast(&msg, subscribers);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> Vec<u8> {
        encode_frame(body.as_bytes())
    }

    #[test]
    fn encode_frame_has_content_length() {
        let bytes = frame("{\"a\":1}");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("Content-Length: 7\r\n\r\n"), "got {text:?}");
        assert!(text.ends_with("{\"a\":1}"));
    }

    #[test]
    fn server_requests_get_answered_not_dropped() {
        // 回归：早期版本只记日志不应答，rust-analyzer 会反复重发
        // workDoneProgress/create 并卡住进度上报。
        for method in ["client/registerCapability", "window/workDoneProgress/create"] {
            match server_request_reply(method) {
                ServerRequestReply::Value(_) => {}
                other => panic!("{method} should be answered with a value, got {other:?}"),
            }
        }
        // `workspace/configuration` 需要看参数才能答（按项数逐项回）。
        assert!(matches!(
            server_request_reply("workspace/configuration"),
            ServerRequestReply::NeedsParams
        ));
    }

    /// `workspace/configuration` 必须**按请求项数**逐项应答。
    ///
    /// 回归：曾恒回空数组。LSP 规范要求响应与 `items` 等长且按序对应，
    /// 服务器取第 i 项时会越界（拿到「没有这项」而不是「默认值」）。
    /// 对照 Zed 的 `lsp_store.rs`：它遍历 `params.items` 逐项查配置。
    #[test]
    fn workspace_configuration_replies_one_entry_per_item() {
        let params = serde_json::json!({
            "items": [
                {"scopeUri": "file:///proj", "section": "rust-analyzer"},
                {"scopeUri": "file:///proj", "section": "editor"},
                {"section": "files"},
            ]
        });
        match reply_for_params("workspace/configuration", &params) {
            ServerRequestReply::Value(Value::Array(items)) => {
                assert_eq!(items.len(), 3, "must reply one entry per requested item");
                assert!(
                    items.iter().all(|v| v.is_null()),
                    "each entry is null = 'no config, use your defaults'"
                );
            }
            other => panic!("expected an array reply, got {other:?}"),
        }
    }

    /// 没有 `items`（或为空）时回空数组，不 panic。
    #[test]
    fn workspace_configuration_handles_missing_items() {
        for params in [serde_json::json!({}), serde_json::json!({"items": []})] {
            match reply_for_params("workspace/configuration", &params) {
                ServerRequestReply::Value(Value::Array(items)) => assert!(items.is_empty()),
                other => panic!("expected an empty array, got {other:?}"),
            }
        }
    }

    /// 未知方法的参数化应答回 `null`（不能让服务器等）。
    #[test]
    fn unknown_params_request_replies_null() {
        match reply_for_params("custom/whatever", &serde_json::json!({"a": 1})) {
            ServerRequestReply::Value(Value::Null) => {}
            other => panic!("expected null, got {other:?}"),
        }
    }

    #[test]
    fn apply_edit_is_rejected_explicitly() {
        // 我们不支持让服务器直接写文件：明确报错，让它退回响应体返回编辑。
        assert!(matches!(
            server_request_reply("workspace/applyEdit"),
            ServerRequestReply::Unimplemented
        ));
    }

    #[test]
    fn unknown_server_request_still_answered() {
        // 未知请求也要应答（回 null），否则服务器会一直等。
        assert!(matches!(
            server_request_reply("custom/someRequest"),
            ServerRequestReply::Value(_)
        ));
    }

    #[test]
    fn parses_notification() {
        let body = r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///a"}}"#;
        let (msg, consumed) = parse_frame(&frame(body)).unwrap().unwrap();
        assert_eq!(consumed, frame(body).len());
        match msg {
            Message::Notification { method, params } => {
                assert_eq!(method, "textDocument/publishDiagnostics");
                assert_eq!(params["uri"], "file:///a");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn parses_response_with_result() {
        let body = r#"{"jsonrpc":"2.0","id":7,"result":{"capabilities":{}}}"#;
        let (msg, _) = parse_frame(&frame(body)).unwrap().unwrap();
        match msg {
            Message::Response { id, result, error } => {
                assert_eq!(id, 7);
                assert!(result.is_some());
                assert!(error.is_none());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn parses_response_with_error() {
        let body = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"not found"}}"#;
        let (msg, _) = parse_frame(&frame(body)).unwrap().unwrap();
        match msg {
            Message::Response { id, error, .. } => {
                assert_eq!(id, 3);
                let e = error.unwrap();
                assert_eq!(e.code, -32601);
                assert_eq!(e.message, "not found");
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn explicit_null_result_is_a_valid_response() {
        // 回归：`{"result": null}` 是**合法**响应（方法成功但无返回值）。
        // rust-analyzer 在 `completion` 无候选时正是这么回的；曾因把
        // `Option<Value>` 的 null 当成「字段缺失」而误判为协议错误，
        // 进而关掉整条连接。
        let body = r#"{"jsonrpc":"2.0","id":3,"result":null}"#;
        let (msg, _) = parse_frame(&frame(body)).unwrap().unwrap();
        match msg {
            Message::Response { id, result, error } => {
                assert_eq!(id, 3);
                assert!(error.is_none());
                assert_eq!(result, Some(serde_json::Value::Null));
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn response_without_result_or_error_is_protocol_error() {
        // 真正的协议错误：两个字段都没有（不是「值为 null」）。
        let body = r#"{"jsonrpc":"2.0","id":4}"#;
        assert!(parse_frame(&frame(body)).is_err());
    }

    #[test]
    fn parses_server_to_client_request() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"workspace/configuration","params":{}}"#;
        let (msg, _) = parse_frame(&frame(body)).unwrap().unwrap();
        match msg {
            Message::Request { id, method, .. } => {
                assert_eq!(id, 1);
                assert_eq!(method, "workspace/configuration");
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_header_returns_none() {
        // 只有半个头：不是错误，是数据不够。
        assert!(parse_frame(b"Content-Length: 10\r\n").unwrap().is_none());
    }

    #[test]
    fn incomplete_body_returns_none() {
        // 头完整但载荷不足。
        let mut buf = frame(&"x".repeat(20));
        buf.truncate(buf.len() - 5);
        assert!(parse_frame(&buf).unwrap().is_none());
    }

    #[test]
    fn multiple_frames_in_one_buffer() {
        // 粘包：一次拿到两条消息，必须只消费第一条并报告正确偏移。
        let mut buf = frame(r#"{"jsonrpc":"2.0","method":"a"}"#);
        let second = frame(r#"{"jsonrpc":"2.0","method":"b"}"#);
        buf.extend_from_slice(&second);

        let (msg, consumed) = parse_frame(&buf).unwrap().unwrap();
        assert_eq!(consumed, frame(r#"{"jsonrpc":"2.0","method":"a"}"#).len());
        match msg {
            Message::Notification { method, .. } => assert_eq!(method, "a"),
            other => panic!("got {other:?}"),
        }
        // 剩余部分仍能解析出第二条。
        let (msg, _) = parse_frame(&buf[consumed..]).unwrap().unwrap();
        match msg {
            Message::Notification { method, .. } => assert_eq!(method, "b"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn lowercase_content_length_is_accepted() {
        // 真实服务器偶发小写头字段。
        let body = r#"{"jsonrpc":"2.0","method":"x"}"#;
        let mut buf = Vec::new();
        buf.extend_from_slice(format!("content-length: {}\r\n\r\n", body.len()).as_bytes());
        buf.extend_from_slice(body.as_bytes());
        let (msg, _) = parse_frame(&buf).unwrap().unwrap();
        match msg {
            Message::Notification { method, .. } => assert_eq!(method, "x"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn missing_content_length_is_error() {
        let buf = b"\r\n\r\n{}";
        assert!(parse_frame(buf).is_err());
    }

    #[test]
    fn dispatch_pairs_response_to_pending_request() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = bounded(1);
        pending.lock().insert(42, tx);
        let (ntx, nrx) = bounded(8);
        let subs: Mutex<Vec<Subscription>> = Mutex::new(vec![Subscription {
            id: 1,
            tx: ntx,
            rx: nrx.clone(),
        }]);

        dispatch(
            Message::Response {
                id: 42,
                result: Some(serde_json::json!({"ok": true})),
                error: None,
            },
            &pending,
            &subs,
        );

        let value = rx.try_recv().unwrap().unwrap();
        assert_eq!(value["ok"], true);
        // 配对后从表内移除。
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn dispatch_turns_error_into_err() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = bounded(1);
        pending.lock().insert(1, tx);
        let (ntx, nrx) = bounded(8);
        let subs: Mutex<Vec<Subscription>> = Mutex::new(vec![Subscription {
            id: 1,
            tx: ntx,
            rx: nrx.clone(),
        }]);

        dispatch(
            Message::Response {
                id: 1,
                result: None,
                error: Some(ResponseError {
                    code: -32602,
                    message: "bad params".into(),
                    data: None,
                }),
            },
            &pending,
            &subs,
        );

        let err = rx.try_recv().unwrap().unwrap_err();
        assert!(err.to_string().contains("bad params"));
    }

    #[test]
    fn dispatch_forwards_notification() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ntx, nrx) = bounded(8);
        let subs: Mutex<Vec<Subscription>> = Mutex::new(vec![Subscription {
            id: 1,
            tx: ntx,
            rx: nrx.clone(),
        }]);
        dispatch(
            Message::Notification {
                method: "textDocument/publishDiagnostics".into(),
                params: serde_json::json!({"uri": "file:///x"}),
            },
            &pending,
            &subs,
        );
        let msg = nrx.try_recv().unwrap();
        match msg {
            Message::Notification { method, .. } => {
                assert_eq!(method, "textDocument/publishDiagnostics")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn dispatch_drops_oldest_notification_when_full() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let subs: Mutex<Vec<Subscription>> = Mutex::new(Vec::new());
        // 容量 2 的一个订阅者
        let (ntx, nrx) = bounded(2);
        subs.lock().push(Subscription {
            id: 1,
            tx: ntx,
            rx: nrx.clone(),
        });
        for i in 0..3 {
            dispatch(
                Message::Notification {
                    method: format!("m{i}"),
                    params: Value::Null,
                },
                &pending,
                &subs,
            );
        }
        // 容量 2，第三条挤掉第一条。
        let first = nrx.try_recv().unwrap();
        match first {
            Message::Notification { method, .. } => assert_eq!(method, "m1"),
            other => panic!("got {other:?}"),
        }
    }

    /// 辅助：往订阅者列表里塞一个容量 `cap` 的订阅者，返回它的接收端。
    fn add_sub(subs: &Mutex<Vec<Subscription>>, cap: usize, id: u64) -> Receiver<Message> {
        let (tx, rx) = bounded(cap);
        subs.lock().push(Subscription { id, tx, rx: rx.clone() });
        rx
    }

    /// 通知必须**广播**给每个订阅者。
    ///
    /// 回归：一个服务器进程服务整个项目（`LspManager` 按项目根+语言复用），
    /// 每个打开的标签都要收诊断。此前所有标签共享一个 `async_channel` 队列，
    /// 而它是 MPMC——每条消息只到**一个**接收者。于是 A 文件的诊断被 B 标签
    /// 的监听器抢走，B 只认自己的 uri 便`continue` 掉，这条诊断**无人处理**。
    /// 最小复现（三个标签各认一个文件、服务器推三条诊断）实测只落地 2/3。
    #[test]
    fn dispatch_broadcasts_to_every_subscriber() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let subs: Mutex<Vec<Subscription>> = Mutex::new(Vec::new());

        // 三个订阅者，各自认一个 uri（模拟三个标签的诊断监听器）
        let receivers: Vec<_> = (1..=3).map(|i| add_sub(&subs, 8, i)).collect();

        // 服务器推 3 条诊断，分属不同文件
        for uri in ["file:///p/a.rs", "file:///p/b.rs", "file:///p/c.rs"] {
            dispatch(
                Message::Notification {
                    method: "textDocument/publishDiagnostics".into(),
                    params: serde_json::json!({ "uri": uri }),
                },
                &pending,
                &subs,
            );
        }

        // 每个订阅者都必须收到全部 3 条（各自再按 uri 过滤）
        for (i, rx) in receivers.iter().enumerate() {
            let mut n = 0;
            while rx.try_recv().is_ok() {
                n += 1;
            }
            assert_eq!(n, 3, "订阅者 {i} 应收到全部 3 条通知，实际 {n} 条");
        }
    }

    /// 订阅句柄 drop 后必须从列表里摘除，否则订阅者泄漏。
    ///
    /// 退订**不能**靠「接收端是否 closed」判断：表里持有的 `Receiver` 克隆本身
    /// 就让通道永远不会 closed（`is_closed()` 恒为 false，实测过），订阅者会永
    /// 久留下，每来一条通知都往一个没人消费的队列里塞一份。
    #[test]
    fn dropping_the_subscription_handle_unsubscribes() {
        let registry: Arc<Mutex<Vec<Subscription>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = bounded(8);
        registry.lock().push(Subscription {
            id: 7,
            tx,
            rx: rx.clone(),
        });
        assert_eq!(registry.lock().len(), 1);

        let id = 7u64;
        let sub = NotificationSub {
            rx: rx.clone(),
            registry: Arc::clone(&registry),
            id,
        };
        assert_eq!(registry.lock().len(), 1, "句柄存活期间应仍在列表里");
        drop(sub);
        assert_eq!(
            registry.lock().len(),
            0,
            "句柄 drop 后必须退订，否则订阅者泄漏"
        );
    }

    #[test]
    fn serialize_request_uses_numeric_id_and_null_params_omitted() {
        let out = OutgoingRequest {
            jsonrpc: "2.0",
            id: 5,
            method: "shutdown",
            params: None,
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 5);
        assert_eq!(v["method"], "shutdown");
        // `skip_serializing_if` 生效：多数服务器对显式 null 参数更挑剔。
        assert!(v.get("params").is_none());
    }

    // ------------------------------------------------ 动态能力注册

    /// 记录 `client/registerCapability` 里声明的方法名。
    ///
    /// 回归：某些能力（如 `workspace/didChangeWatchedFiles`）不在静态
    /// `InitializeResult.capabilities` 里，而是启动后动态注册。不记录就
    /// 无法判断该不该发通知。
    #[test]
    fn records_dynamically_registered_methods() {
        let set = Mutex::new(std::collections::HashSet::new());
        let globs = Mutex::new(Vec::new());
        let params = serde_json::json!({
            "registrations": [
                { "id": "r1", "method": "workspace/didChangeWatchedFiles" },
                { "id": "r2", "method": "workspace/didChangeConfiguration" },
            ]
        });
        record_dynamic_registration(&set, &globs, "client/registerCapability", &params);

        assert!(set.lock().contains("workspace/didChangeWatchedFiles"));
        assert!(set.lock().contains("workspace/didChangeConfiguration"));
        assert!(!set.lock().contains("workspace/symbol"));
    }

    /// `client/unregisterCapability` 要移除对应的方法名。
    #[test]
    fn unregister_removes_the_method() {
        let set = Mutex::new(std::collections::HashSet::new());
        let globs = Mutex::new(Vec::new());
        let reg = serde_json::json!({
            "registrations": [{ "id": "r1", "method": "workspace/didChangeWatchedFiles" }]
        });
        record_dynamic_registration(&set, &globs, "client/registerCapability", &reg);
        assert!(set.lock().contains("workspace/didChangeWatchedFiles"));

        record_dynamic_registration(&set, &globs, "client/unregisterCapability", &reg);
        assert!(
            !set.lock().contains("workspace/didChangeWatchedFiles"),
            "unregister must remove the method"
        );
    }

    /// 形态不对的 params（缺 registrations / 元素缺 method）不能 panic。
    #[test]
    fn malformed_registrations_are_ignored() {
        let set = Mutex::new(std::collections::HashSet::new());
        let globs = Mutex::new(Vec::new());
        for params in [
            serde_json::json!({}),
            serde_json::json!({ "registrations": [] }),
            serde_json::json!({ "registrations": [{}] }),
            serde_json::json!({ "registrations": [{ "method": 42 }] }),
            serde_json::json!("not an object"),
        ] {
            record_dynamic_registration(&set, &globs, "client/registerCapability", &params);
        }
        assert!(set.lock().is_empty(), "malformed input must not register anything");
    }

    /// 注册项里的 `watchers` glob 必须被记下来——发通知前就靠它过滤。
    ///
    /// 回归的意义很大：不记 glob 就只能对所有文件一视同仁，而 `target/` 里每次
    /// `cargo check` 的写入都会让 rust-analyzer 重载整个工程（自持循环）。
    #[test]
    fn records_watcher_globs_from_registration() {
        let set = Mutex::new(std::collections::HashSet::new());
        let globs = Mutex::new(Vec::new());
        // 两种 glob 形状都要兼容：3.16 的字符串、3.17 的 {baseUri, pattern}。
        let params = serde_json::json!({
            "registrations": [{
                "id": "r1",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": {
                    "watchers": [
                        { "globPattern": "**/Cargo.toml", "kind": 7 },
                        { "globPattern": { "baseUri": "file:///ws", "pattern": "**/.cargo/config*" } }
                    ]
                }
            }]
        });
        record_dynamic_registration(&set, &globs, "client/registerCapability", &params);

        let recorded = globs.lock().clone();
        assert!(
            recorded.contains(&"**/Cargo.toml".to_string()),
            "string globPattern must be recorded, got {recorded:?}"
        );
        assert!(
            recorded.contains(&"**/.cargo/config*".to_string()),
            "3.17 relative glob must be recorded, got {recorded:?}"
        );
    }

    /// 别的方法的注册项不能混进 glob 列表（否则会把无关文件也算成「服务器关心」）。
    #[test]
    fn globs_only_come_from_watched_files_registration() {
        let set = Mutex::new(std::collections::HashSet::new());
        let globs = Mutex::new(Vec::new());
        let params = serde_json::json!({
            "registrations": [{
                "id": "r1",
                "method": "textDocument/rename",
                "registerOptions": { "watchers": [{ "globPattern": "**/*.rs" }] }
            }]
        });
        record_dynamic_registration(&set, &globs, "client/registerCapability", &params);
        assert!(globs.lock().is_empty(), "non-watched-files registration must not add globs");
    }

    /// 撤销注册要清空 glob，让过滤退回「排除构建产物」的兜底。
    ///
    /// 保守清空是有意的：漏记某个 glob 会让服务器收不到它关心的变更，而多记
    /// 只多发一点通知。宁可多发也不能永远不发。
    #[test]
    fn unregister_clears_globs() {
        let set = Mutex::new(std::collections::HashSet::new());
        let globs = Mutex::new(Vec::new());
        let reg = serde_json::json!({
            "registrations": [{
                "id": "r1",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": { "watchers": [{ "globPattern": "**/Cargo.toml" }] }
            }]
        });
        record_dynamic_registration(&set, &globs, "client/registerCapability", &reg);
        assert_eq!(globs.lock().len(), 1);

        record_dynamic_registration(&set, &globs, "client/unregisterCapability", &reg);
        assert!(globs.lock().is_empty(), "unregister must clear globs");
    }
}
