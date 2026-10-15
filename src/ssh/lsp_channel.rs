//! 远端语言服务器：把 LSP 服务器的 stdio 架在一条 SSH channel 上。
//!
//! # 为什么这样做
//!
//! 语言服务器是有状态的常驻进程，它必须跑在**源码所在的那台机器**上：
//! rust-analyzer 要读 `Cargo.toml`、跑 `cargo metadata`、找 sysroot；
//! gopls 要读 `go.mod` 与 vendor 目录。服务器跑在本地而文件在远端时，
//! 这些它一样都看不到，诊断与补全会退化成「半残」甚至完全不工作。
//!
//! 于是这里的做法是：经 SSH 在远端 `exec` 服务器进程，把它的
//! stdin/stdout 接到 channel 的 writer/reader 上。本地侧看到的仍然是一条
//! 普通的「读写端」，帧编解码、请求配对、通知广播全部复用本地那一条路径
//! （对照 Zed `lsp/src/lsp.rs:465` 的 `new_internal` 对 stdio 泛型）。
//!
//! 与 Zed 的差别：Zed 往远端上传一个完整的 `zed` 二进制并跑 `proxy`，
//! 用 protobuf RPC 把整套 project 状态同步过去（`remote/src/transport/ssh.rs:439`）。
//! 那套方案要求远端有一份完整的编辑器运行时；本项目没有 RPC 层，
//! 直接把 channel 当管道用是同一目的下的轻量解法。
//!
//! # trait 家族桥接（关键实现细节）
//!
//! russh 的 `Channel::make_reader()` / `make_writer()` 产出的是
//! **`tokio::io::AsyncRead` / `AsyncWrite`**，而 LSP 的帧循环用的是
//! **`futures` 的同名 trait**。两者不是一套 trait，直接传会编译不过。
//!
//! 桥接靠 `tokio-util` 的 `compat` feature（`TokioAsyncReadCompatExt::compat()`）。
//! 该 crate 已在依赖树里（`russh-sftp` 传递引入），只需在 `Cargo.toml`
//! 显式打开 feature。
//!
//! 已核对 `russh` 源码确认可以在 smol 里 poll：`ChannelRx`（reader 的实现）
//! 内部只用 `tokio::sync::mpsc::Receiver::poll_recv`，**不取 tokio runtime
//! 句柄**。因此「tokio 驱动 SSH、smol 驱动 LSP 帧循环」的分工成立，
//! 不需要为了让 LSP 跑起来而引入 tokio runtime 上下文。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use futures::{AsyncRead, AsyncWrite};
use russh::client::Handle;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::ssh::SshConnectionConfig;
use crate::ssh::russh_terminal::SshClientHandler;

/// 远端语言服务器进程的持有者。
///
/// 三样东西绑在一起，因为它们的寿命必须一致：
/// - SSH `handle`：断开它则 channel 一起没；
/// - `channel`：服务器进程的 stdio 就在它上面（`take_io` 用
///   [`russh::Channel::into_stream`] 吃掉它，之后置 `None`）；
/// - `finished`：远端进程是否已退出（读循环读到 EOF 时置位）。
///
/// `handle` 包 `Arc`：关闭路径可能在任意线程被触发
/// （`Connection::shutdown` 丢给后台任务），而 `russh::client::Handle`
/// 自身不是 `Clone`。
pub struct RemoteLspProcess {
    handle: Arc<Handle<SshClientHandler>>,
    channel: Option<russh::Channel<russh::client::Msg>>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl RemoteLspProcess {
    /// 连上远端并启动语言服务器进程。
    ///
    /// `program` 是**远端的绝对路径**（由 [`crate::ssh::lsp_channel::resolve_remote_binary`]
    /// 探测得到，见该函数的 PATH 说明），`args` 是服务器自己的参数。
    ///
    /// # 为什么不请求 PTY
    ///
    /// PTY 会把远端的终端转义序列、回车换行转换混进字节流。LSP 帧是
    /// `Content-Length: N` + 裸 JSON，任何一个 `\r\n` → `\n` 的转换都会让
    /// 帧长对不上。与 `sftp_fs::execute_command` 同款处理：`exec(false, cmd)`。
    pub async fn connect(
        config: &SshConnectionConfig,
        program: &str,
        args: &[String],
    ) -> Result<Self> {
        // 复用终端的握手/认证/主机密钥校验：LSP 承载的是「在远端执行代码」，
        // 与终端同等敏感，任何一侧漏改都会造成两条连接对同一个主机的
        // 安全判定不一致。
        //
        // 端口转发表传空表：LSP 连接不请求监听，也就不会收到
        // `forwarded-tcpip`；真收到了说明不是我们的转发，按未登记忽略
        // （与终端连接同款处理，见 `SshClientHandler` 字段说明）。
        //
        // **握手与开通道必须跑在 tokio runtime 里**：russh 内部用
        // `tokio::time::timeout`（`connect_and_authenticate`）与
        // `tokio::sync` 原语，没有 runtime 上下文会直接 panic
        // （"there is no reactor running"）。而调用方（`LspManager` 的启动
        // 任务）跑在 `smol::spawn` 上——smol 有自己的 reactor，tokio 认不出来。
        // 所以整段丢给共享 SSH runtime 执行，拿到结果再回到 smol 侧继续。
        //
        // 这里 `block_on` 阻塞的是 smol 的那个 worker 线程，不是 UI 线程；
        // 与 `sftp_fs::with_cached_session` 的既有做法同源。
        let handle = crate::ssh::shared_runtime::shared_ssh_runtime().block_on(
            crate::ssh::russh_terminal::connect_and_authenticate(
                config,
                SshClientHandler {
                    host: config.host.clone(),
                    port: config.port,
                    host_key_policy: crate::ssh::host_key::policy_from_global_config(),
                    port_forwards: crate::ssh::port_forward::RemoteForwardTable::default(),
                },
            ),
        )?;

        // 开通道与 exec 同样要 tokio 上下文（`channel_open_session` 内部
        // 走 `tokio::sync::oneshot`）。
        let channel = crate::ssh::shared_runtime::shared_ssh_runtime()
            .block_on(handle.channel_open_session())
            .map_err(|e| anyhow!("打开 SSH 通道失败: {e}"))?;

        // 命令整体交给远端的 shell 执行：`exec` 在服务端是经 shell 跑的，
        // 所以参数要按 shell 规则引用。绝对路径本身不含空格（已探测），
        // 但用户配置的 args 可能含（如 `--cfg 'foo bar'` 拆开后的片段）。
        let mut command = String::with_capacity(program.len() + 32);
        command.push_str(&crate::utils::shell::shell_quote_single(program));
        for arg in args {
            command.push(' ');
            command.push_str(&crate::utils::shell::shell_quote_single(arg));
        }

        crate::ssh::shared_runtime::shared_ssh_runtime()
            .block_on(channel.exec(false, command.as_str()))
            .map_err(|e| anyhow!("在远端启动语言服务器失败: {e}"))?;

        Ok(Self {
            handle: Arc::new(handle),
            channel: Some(channel),
            finished: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// 取出桥接好的读写端，交给 LSP 的帧循环。
    ///
    /// 读端额外挂一个「读到 EOF 即远端进程结束」的标记：关闭路径要靠它
    /// 判断服务器有没有自己退掉（本地场景对应 `child.try_status()`）。
    pub(crate) fn take_io(
        &mut self,
    ) -> Result<(
        Box<dyn AsyncRead + Send + Unpin + 'static>,
        Box<dyn AsyncWrite + Send + Unpin + 'static>,
    )> {
        let channel = self
            .channel
            .take()
            .context("remote language server io already taken")?;

        // `into_stream()` 吃掉整个 channel，产出同时实现读写的 `ChannelStream`。
        //
        // **为什么必须用它而不是 `make_reader`/`make_writer`**：那两个返回的
        // reader 借着 `&mut self`，不是 `'static`，活不过这个函数；而 LSP 帧
        // 循环跑在 `smol::spawn` 的独立任务里，要求读写端 `'static`。
        // `ChannelStream` 把两个 half 的所有权都吃进去了。
        //
        // 再经 `tokio::io::split` 拆成两个 owned half（russh 自己的
        // `ssh_read.rs` 就是这么用的），读写两端才能各自独立交给帧循环。
        //
        // **为什么外面还要包一层 `CloseOnRuntime`**：`ChannelStream` 内部持有
        // `ChannelCloseOnDrop`，它的 `Drop` 直接 `tokio::spawn(...)`。而这个
        // 流最终会在 **smol** 线程上被丢弃（帧循环的任务结束、连接重建）——
        // 那里没有 tokio reactor，`tokio::spawn` 会 panic
        // （"there is no reactor running"）。panic 发生在 drop 里，测试输出里
        // 表现为一个莫名的后台线程崩溃，而功能「看起来」是好的。
        // 所以在这里就把流包起来，让它的 `Drop` 改到共享 runtime 上执行。
        let stream = CloseOnRuntime::new(channel.into_stream());
        let (reader, writer) = tokio::io::split(stream);

        // 读端包一层 EOF 标记；写端直接透传。
        // `ChannelStream` 是 `tokio::io` 的 trait，帧循环要 `futures` 的：
        // 读走 `compat()`，写走 `compat_write()`——两个扩展 trait 都定义了
        // 名为 `compat` 的方法，写方向另起名正是为了避免同时引入时的歧义。
        let reader = EofReader {
            inner: reader.compat(),
            finished: Arc::clone(&self.finished),
        };

        Ok((Box::new(reader), Box::new(writer.compat_write())))
    }

    /// 远端进程是否已退出。
    pub fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 强制关闭：断开 SSH 连接（channel 随之关闭，远端进程收到 EOF）。
    ///
    /// 关不掉也只是泄漏一个远端进程，不影响本地；所以这里不重试、不阻塞。
    ///
    /// 只断连接而**不**显式 `channel.close()`：`take_io` 已把两个 half 交给
    /// 帧循环，这里拿不到它们；而 russh 在 handle 断开时会把该连接上所有
    /// channel 一并关闭，远端进程照样会收到 EOF。
    pub fn close(&self) {
        let finished = Arc::clone(&self.finished);
        let handle = Arc::clone(&self.handle);
        crate::ssh::shared_runtime::shared_ssh_runtime().spawn(async move {
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "", "English")
                .await;
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
}

impl Drop for RemoteLspProcess {
    fn drop(&mut self) {
        if !self.is_finished() {
            self.close();
        }
    }
}

/// 读端：EOF 时置「远端进程已退出」。
///
/// russh 的 reader 在 channel 关闭时返回 `Ok(0)`，但那个信号只存在于读循环
/// 内部。关闭路径需要知道服务器退没退，所以在这里落一个标志。
struct EofReader {
    inner: tokio_util::compat::Compat<tokio::io::ReadHalf<CloseOnRuntime>>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl AsyncRead for EofReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(0)) => {
                this.finished
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Poll::Ready(Ok(0))
            }
            other => other,
        }
    }
}

/// 让 `ChannelStream` 的析构发生在 tokio runtime 上。
///
/// # 这个包装存在的唯一理由
///
/// `russh::ChannelStream` 内含 `ChannelCloseOnDrop`，其 `Drop` 实现直接调
/// `tokio::spawn(...)` 去发 `ChannelMsg::Close`。而 DeepTerm 的 LSP 帧循环跑在
/// **smol** 上（`smol::spawn`），SSH 连接跑在 tokio 上——两个 runtime 分工。
/// 于是这条流会在 smol 线程上被丢弃，`tokio::spawn` 在那里 panic：
///
/// ```text
/// thread 'smol-1' panicked at russh-0.61.1/src/channels/io/mod.rs:37:
/// there is no reactor running, must be called from the context of a Tokio 1.x runtime
/// ```
///
/// panic 在 `Drop` 里，不会让调用方看到错误，只会留下一个崩溃的后台线程，
/// 而功能看起来是好的——这是最难发现的那类 bug。
///
/// # 为什么不用「提前显式 close」绕过
///
/// 显式 `channel.close()` 只能由持有 `Channel` 的一方调，而 `into_stream()`
/// 之后 ownership 已交给帧循环；帧循环结束时仍要走 `Drop`。所以正确做法是
/// 让**析构本身**发生在有 reactor 的线程上：`Drop` 里把内层流送到共享
/// runtime 再丢。
struct CloseOnRuntime {
    inner: Option<russh::ChannelStream<russh::client::Msg>>,
}

impl CloseOnRuntime {
    fn new(inner: russh::ChannelStream<russh::client::Msg>) -> Self {
        Self { inner: Some(inner) }
    }
}

impl Drop for CloseOnRuntime {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        // 丢进 blocking 池：那些线程同样带着 tokio runtime 上下文，内层流的
        // `Drop` 里的 `tokio::spawn` 就能正常落一个 Close 消息。
        //
        // `spawn_blocking` 在 runtime 已关闭时**直接 panic**（它返回的不是
        // `Result`）。进程退出路径上确实会走到这里，所以用 `catch_unwind`
        // 兜住：那时连接本就随之消失，同步丢弃是等价的结果。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::ssh::shared_runtime::shared_ssh_runtime().spawn_blocking(move || drop(inner));
        }));
        if result.is_err() {
            log::debug!("lsp(remote): ssh runtime gone, dropping channel inline");
        }
    }
}

impl tokio::io::AsyncRead for CloseOnRuntime {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let inner = self.get_mut().inner.as_mut().expect("stream present");
        std::pin::Pin::new(inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for CloseOnRuntime {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let inner = self.get_mut().inner.as_mut().expect("stream present");
        std::pin::Pin::new(inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let inner = self.get_mut().inner.as_mut().expect("stream present");
        std::pin::Pin::new(inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let inner = self.get_mut().inner.as_mut().expect("stream present");
        std::pin::Pin::new(inner).poll_shutdown(cx)
    }
}

/// 远端探测的三种结论，必须区分「没装」与「问不到」。
///
/// 存在的理由是 [`crate::lsp::registry::command_available_on`] 的缓存策略：
/// 候选链挑中哪个、状态栏显示哪个名字，都按 (host, command) 缓存探测结果。
/// 「这台机器上确实没有 pyright」值得缓存一整个会话；「刚才那条 SSH 连接
/// 还没起来」缓存下来就会**永久**把远端有 pylsp 的主机判成「什么也没有」——
/// 用户看到的症状是点了启动后报「远端找不到 pyright-langserver」，而那台机器
/// 上明明有服务器，且没有任何线索指向「第一次探测赶上连接未就绪」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBinary {
    /// 找到了，附绝对路径。
    Found(PathBuf),
    /// shell 正常执行并答复「没有这个命令」——确定性结论，可长期缓存。
    NotInstalled,
    /// 探测本身没跑成（连不上 / 认证失败 / 超时）——不是关于软件的事实。
    Unreachable,
}

/// 在远端解析语言服务器的可执行路径。
///
/// `pub(crate)` 而不是公开 API：这是 SSH 层的实现细节，对外只经
/// [`crate::lsp::registry`] 的候选链与 [`crate::lsp::client::LspClient::start_remote`]
/// 使用——挑候选（`command_available_on`）与起进程必须走同一个解析器，
/// 否则 PATH 策略一改两处就漂移。
///
/// # 为什么必须专门探测
///
/// `channel.exec` 在服务端经 **非登录、非交互** shell 执行，`PATH` 与用户
/// 在终端里敲命令时看到的**不一样**。实测本项目的测试机：
///
/// ```text
/// 非登录 PATH: /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
/// 登录 PATH:   /home/zhousong/.cargo/bin:/home/zhousong/.local/bin:...
/// ```
///
/// rustup 装的 `rust-analyzer` 只在 `~/.cargo/bin`，于是直接按名字 exec
/// 会以 127（command not found）失败，而同一台机器上用户手敲
/// `rust-analyzer --version` 完全正常——差别只在启动方式。
///
/// # 探测策略
///
/// 两级，都经同一条 SFTP 会话执行（不新开连接）：
///
/// 1. `command -v <name>`：覆盖系统级安装（clangd、gopls 的 apt 版等）。
/// 2. `bash -lc 'command -v <name>'`：覆盖只在登录 shell 里可见的路径
///    （rustup 的 `~/.cargo/bin`、nvm、pyenv、`uv tool` 的 `~/.local/bin` 之类）。
///
/// 两级都失败才报错，且错误信息要指明是「远端找不到这个命令」，
/// 而不是笼统的「启动失败」——两者对用户的行动指引完全不同
/// （前者要去远端装服务器，后者是本地配置问题）。
pub fn resolve_remote_binary_detailed(config: &SshConnectionConfig, program: &str) -> RemoteBinary {
    // 已经是绝对路径：无需探测，直接交出去。
    if program.starts_with('/') {
        return RemoteBinary::Found(PathBuf::from(program));
    }

    let candidates = [
        format!(
            "command -v {}",
            crate::utils::shell::shell_quote_single(program)
        ),
        format!(
            "bash -lc {}",
            crate::utils::shell::shell_quote_single(&format!(
                "command -v {}",
                crate::utils::shell::shell_quote_single(program)
            ))
        ),
    ];

    // 至少有一级 shell 正常执行过（无论答案是什么）→ 这台机器可以问、
    // 且答复是「没有」。一级都没跑起来 → 只是问不到。
    let mut reached_any_shell = false;

    for command in &candidates {
        // 复用 SFTP 会话池：不新开一条 SSH 连接（探测是启动路径上的热操作，
        // 每次新建连接要多一次完整的 TCP+SSH 握手）。
        let result = crate::ssh::sftp_fs::with_cached_session(config, |session| {
            session.execute_command_bytes(command, 15)
        });
        let Ok(raw) = result else { continue };
        reached_any_shell = true;
        if !raw.success() {
            continue;
        }
        let Some(first) = first_path_line(&raw.stdout) else {
            continue;
        };
        log::debug!("lsp(remote): resolved `{program}` → `{first}`");
        return RemoteBinary::Found(PathBuf::from(first));
    }

    if reached_any_shell {
        RemoteBinary::NotInstalled
    } else {
        RemoteBinary::Unreachable
    }
}

/// 远端命令的 stdout 里第一条「看着像路径」的行。
///
/// `command -v` 与项目根探测脚本都可能回多行，而第二级走的是 `bash -lc`，
/// 它把 rc 文件里的 `echo`、模块加载提示之类的噪声一起带到 stdout。
/// 取「第一条非空行」会在噪声先出现时把真答案丢掉——表现为「远端明明装了
/// pylsp 却报找不到」。
///
/// 只认以 `/` 开头的行：两条探测脚本的确定答案都是绝对路径。
pub fn first_path_line(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('/'))
        .map(str::to_string)
}

/// 同 [`resolve_remote_binary_detailed`]，只要不要路径的那一面。
///
/// 启动路径用它：那里关心的是「拿什么路径去 exec」，探不到一律是错误，
/// 且错误信息要指名道姓到哪台机器缺哪个命令。
///
/// # 两种失败必须是两种错误
///
/// 「这台机器上没装这个命令」是**事实**，可以永久记下来（`ServerTable::failed`）
/// 直到用户改配置或按重启；「问不到这台机器」只是**这次没问成**（连不上、
/// 认证失败、探测超时），把它写成前者会让一次 SSH 抖动永久拉黑这个项目的
/// 服务器——之后每次打开文件都直接返回「启动失败」，重启应用之前再也起不来。
/// 后者用 [`RemoteUnreachable`] 承载，启动路径据此选择「不拉黑」的收尾。
pub fn resolve_remote_binary(config: &SshConnectionConfig, program: &str) -> Result<PathBuf> {
    match resolve_remote_binary_detailed(config, program) {
        RemoteBinary::Found(path) => Ok(path),
        RemoteBinary::NotInstalled => Err(anyhow!(
            "远端 {} 找不到语言服务器命令 `{program}`。\n\
             请在该服务器上安装它，或在 settings.json 的 lsp.servers 里为这个语言\
             指定绝对路径。",
            config.connection_string()
        )),
        RemoteBinary::Unreachable => Err(anyhow!(RemoteUnreachable::new(config, program))),
    }
}

/// 「问不到那台机器」——不是关于软件安装的事实，不该被当成事实记账。
///
/// 存在的唯一理由：让 LSP 启动路径能把这种失败与「这台机器确实没装」区分开
/// （见 [`resolve_remote_binary`]）。
#[derive(Debug, Clone)]
pub struct RemoteUnreachable {
    connection: String,
    program: String,
}

impl RemoteUnreachable {
    pub fn new(config: &SshConnectionConfig, program: &str) -> Self {
        Self {
            connection: config.connection_string(),
            program: program.to_string(),
        }
    }
}

impl std::fmt::Display for RemoteUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "连不上 {connection}，无法确认是否装了 `{program}`；\
             这不代表那台机器上没有，稍后会自动重试。",
            connection = self.connection,
            program = self.program,
        )
    }
}

impl std::error::Error for RemoteUnreachable {}
