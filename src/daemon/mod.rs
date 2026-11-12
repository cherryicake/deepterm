//! 终端守护进程：让本地/远程终端在 deepterm 关闭后继续运行。
//!
//! 设计见 `docs/DESIGN_TERMINAL_PERSISTENCE.md`。这里只放与 UI 无关的部分：
//! 协议、socket 路径、终端登记。

pub mod client;
pub mod connection;
pub mod listen;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod session;
pub mod terminal;
#[cfg(windows)]
pub mod windows;

use std::path::PathBuf;

/// 守护进程的 socket 文件名（Unix）。Windows 上是 Named Pipe 名。
pub const SOCKET_FILE_NAME: &str = "daemon.sock";

/// 运行时目录的环境变量覆盖。
///
/// 测试与多套并行安装需要能换目录，否则 `cargo test` 的多个进程会抢同一个
/// socket。
pub const RUNTIME_DIR_ENV: &str = "DEEPTERM_RUNTIME_DIR";

/// 守护进程的运行时目录：放 socket 与 pid 文件。
///
/// 优先 `$DEEPTERM_RUNTIME_DIR`，其次 `$XDG_RUNTIME_DIR/deepterm`，
/// 最后退到配置目录。选 `XDG_RUNTIME_DIR` 的理由：它在 Linux 上是
/// tmpfs 且权限为 0700，socket 天然不会被别的用户拿到；配置目录在
/// 某些发行版上是 0755，把 socket 放那里等于把终端交给同机所有用户。
/// Unix socket 的路径上限（`sun_path`），macOS 与 Linux 都是 104 字节。
///
/// 超了这个长度 `bind` 直接失败，而且错误信息是 "path too long"，很容易
/// 被当成权限问题查半天。macOS 没有 `XDG_RUNTIME_DIR`，默认会落到
/// `~/Library/Application Support/deepterm/`——用户名或目录名长一点就可能
/// 越界，所以要主动兜底。
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("deepterm");
    }

    let config = crate::workspace::persistence::PersistenceManager::get_config_dir();

    // Unix 上先算一下 socket 路径会不会超限；超了就退到临时目录下按 uid
    // 隔离的短路径。
    //
    // 注意**必须按解析软链之后的路径算**：macOS 上 `/tmp` 是指向
    // `/private/tmp` 的软链，`bind` 时内核看到的是解析后的路径，比用户写的
    // 多 8 个字节。只按字面量长度检查会在边界上放行、然后在小核那里失败。
    #[cfg(unix)]
    {
        let would_be = config.join(SOCKET_FILE_NAME);
        let resolved_len = std::fs::canonicalize(&config)
            .map(|dir| dir.join(SOCKET_FILE_NAME).to_string_lossy().len())
            .unwrap_or_else(|_| would_be.to_string_lossy().len());
        if resolved_len > MAX_SOCKET_PATH {
            let uid = unsafe { libc::getuid() };
            let short = PathBuf::from(format!("/tmp/deepterm-{uid}"));
            log::warn!(
                "配置目录下的 socket 路径过长（{}），改用 {}",
                would_be.display(),
                short.display()
            );
            return short;
        }
    }

    config
}

/// socket（或 Named Pipe）的完整路径。
pub fn socket_path() -> PathBuf {
    runtime_dir().join(SOCKET_FILE_NAME)
}

/// 跨平台的传输层抽象。
///
/// Unix 用 `UnixStream`/`UnixListener`（`$XDG_RUNTIME_DIR` 下的 socket，
/// 目录权限 0700）；Windows 用命名管道（可按用户 SID 设 ACL）。
/// 两端都实现 `Read + Write`，所以 `protocol.rs` 的分帧代码完全共用——
/// 跨平台一致才不会有"Linux 上好好的、Windows 上粘包"这类只在一边出现的问题。
#[cfg(unix)]
pub mod transport {
    pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

    /// 连接一个已有 socket。
    pub fn connect(path: &std::path::Path) -> std::io::Result<Stream> {
        Stream::connect(path)
    }

    /// 绑定监听。调用方负责先清理残留的 socket 文件。
    pub fn bind(path: &std::path::Path) -> std::io::Result<Listener> {
        Listener::bind(path)
    }

    /// 设为非阻塞（`listen` 的接受循环要用它做超时轮询）。
    pub fn set_nonblocking(listener: &Listener, on: bool) -> std::io::Result<()> {
        listener.set_nonblocking(on)
    }

    /// 让一条已接受的连接回到阻塞模式。
    pub fn set_blocking(stream: &Stream, on: bool) -> std::io::Result<()> {
        stream.set_nonblocking(!on)
    }

    /// 复制一份流（一条连接要同时被读线程和写线程持有）。
    pub fn try_clone(stream: &Stream) -> std::io::Result<Stream> {
        stream.try_clone()
    }

    /// socket 路径 → 传输层能用的名字。
    pub fn address(path: &std::path::Path) -> String {
        path.to_string_lossy().into_owned()
    }
}

#[cfg(windows)]
pub mod transport {
    pub use super::windows::{Listener, Stream};

    pub fn connect(path: &std::path::Path) -> std::io::Result<Stream> {
        Stream::connect(&address(path))
    }

    pub fn bind(path: &std::path::Path) -> std::io::Result<Listener> {
        Listener::bind(&address(path))
    }

    /// 命名管道没有"非阻塞 accept"这回事；接受循环靠 `accept` 自身的
    /// 阻塞 + 外部关停信号退出（见 `listen::run`）。
    pub fn set_nonblocking(_listener: &Listener, _on: bool) -> std::io::Result<()> {
        Ok(())
    }

    pub fn set_blocking(_stream: &Stream, _on: bool) -> std::io::Result<()> {
        Ok(())
    }

    /// 命名管道句柄可以再复制一份（`DuplicateHandle`），供读写两个线程用。
    pub fn try_clone(stream: &Stream) -> std::io::Result<Stream> {
        stream.try_clone_handle()
    }

    /// 运行时目录 + 固定文件名 → `\\.\pipe\deepterm-<user>`。
    ///
    /// 用用户名而不是路径：管道名是全局命名空间，塞进完整路径既没必要
    /// 也更容易撞上非法字符。
    pub fn address(path: &std::path::Path) -> String {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        // 文件名部分（`daemon.sock`）在管道下没有意义，只用目录区分会话。
        let _ = path;
        format!("{PIPE_PREFIX}-{user}")
    }
}

pub use transport::{Listener, Stream};

#[cfg(test)]
mod path_tests {
    use super::*;

    /// socket 路径必须落在 `sun_path` 的长度上限内。
    ///
    /// 回归价值：macOS 没有 `XDG_RUNTIME_DIR`，默认路径是
    /// `~/Library/Application Support/deepterm/daemon.sock`。用户名或目录名
    /// 长一点就越过 104 字节，`bind` 报 "path too long"——那个错误看起来像
    /// 权限问题，很容易查错方向。这条把上限钉住，改路径拼接时会立刻发现。
    #[cfg(unix)]
    #[test]
    fn the_socket_path_fits_in_sun_path() {
        let path = socket_path();
        let len = path.to_string_lossy().len();
        assert!(
            len <= MAX_SOCKET_PATH,
            "socket 路径 {len} 字节，超过上限 {MAX_SOCKET_PATH}: {}",
            path.display()
        );
    }

    /// 环境变量覆盖必须生效，且要拼上固定的文件名。
    ///
    /// 这条同时保护测试隔离：`cargo test` 并行跑时，没有覆盖就会
    /// 全部去连用户的真实守护进程。
    #[test]
    fn runtime_dir_honours_the_override() {
        // SAFETY：测试内独占该变量名，且不与其他用例并发读写同一变量。
        unsafe { std::env::set_var(RUNTIME_DIR_ENV, "/tmp/deepterm-test-rt") };
        assert_eq!(runtime_dir(), PathBuf::from("/tmp/deepterm-test-rt"));
        assert_eq!(
            socket_path(),
            PathBuf::from("/tmp/deepterm-test-rt/daemon.sock")
        );
        unsafe { std::env::remove_var(RUNTIME_DIR_ENV) };
    }
}
