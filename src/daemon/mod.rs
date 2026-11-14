//! 终端守护进程：让本地/远程终端在 deepterm 关闭后继续运行。
//!
//! 设计见 `docs/DESIGN_TERMINAL_PERSISTENCE.md`。这里只放与 UI 无关的部分：
//! 协议、socket 路径、终端登记。

pub mod client;
pub mod connection;
pub mod core_holder;
pub mod holder;
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

/// holder 的 socket 文件名。
///
/// # 为什么要有 holder
///
/// 终端进程的父进程就是创建 PTY 的那个进程。如果 core 亲自建 PTY，
/// core 一重启（升级、崩溃恢复）终端就跟着死——实测 `ppid` 指向 core，
/// core 退出后程序立刻消失。
///
/// herdr 的做法是让**另一个进程**（holder）持有 PTY：core 只做协调，
/// 重启时 holder 还在，终端毫发无损。这里照做：`deepcore` 启动时若没有
/// holder 就拉一个，之后所有 PTY 都建在 holder 里。
pub const HOLDER_SOCKET_FILE_NAME: &str = "holder.sock";

/// holder 的 socket 路径。
pub fn holder_socket_path() -> PathBuf {
    runtime_dir().join(HOLDER_SOCKET_FILE_NAME)
}

/// 鉴权 token 的文件名。
pub const TOKEN_FILE_NAME: &str = "token";

/// 鉴权 token 的路径。
///
/// **按 socket 名区分**：core 与 holder 是两个进程，共用一个 token 文件时
/// 后启动的会把先启动的覆盖掉，于是 core 拿自己的 token 去连 holder
/// 必然被拒——表现为"spawn 说成功了，holder 里却什么都没有"。
pub fn token_path() -> PathBuf {
    runtime_dir().join(TOKEN_FILE_NAME)
}

/// 与指定 socket 配套的 token 路径。
pub fn token_path_for(socket: &std::path::Path) -> PathBuf {
    socket.with_extension(TOKEN_FILE_NAME)
}

/// 生成一个新的随机 token（32 个十六进制字符 = 128 位）。
///
/// # 为什么需要鉴权
///
/// Unix socket 靠 0700 的目录权限保护：别的用户进不去那个目录。但**远端
/// 部署**必须走 TCP（SSH 只能转发 TCP），而 `127.0.0.1:port` 对同机所有
/// 用户开放——没有 token 的话，服务器上任何一个人都能连上来，拿到你的
/// 全部终端。
///
/// token 写在 `token_path()`，文件权限 0600。它随运行时目录一起创建，
/// 不是长期凭据：守护进程重启就换一个，不落任何"用户配置"。
pub fn generate_token() -> String {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    // 优先用内核熵源。读不到就退回时间 + pid 混合——**不如前者安全**，
    // 但比"没有 token"好得多，且这种情况本身说明系统有问题。
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mixed = (nanos ^ (std::process::id() as u128)).to_le_bytes();
        bytes.copy_from_slice(&mixed[..16]);
        log::warn!("无法读取 /dev/urandom，token 退化到时间+pid 派生");
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 写入 token 文件（0600）。
pub fn write_token_for(socket: &std::path::Path, token: &str) -> std::io::Result<()> {
    let path = token_path_for(socket);
    std::fs::write(&path, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 读出与指定 socket 配套的 token。
pub fn read_token_for(socket: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(token_path_for(socket)).map(|s| s.trim().to_string())
}

/// 写入主 socket 的 token（UI 客户端用）。
pub fn write_token(token: &str) -> std::io::Result<()> {
    write_token_for(&socket_path(), token)
}

/// 读出主 socket 的 token（UI 客户端用）。
pub fn read_token() -> std::io::Result<String> {
    read_token_for(&socket_path())
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

    /// token 必须是够长的随机十六进制串。
    ///
    /// 回归价值：token 是远端部署后唯一的访问控制。长度不足或字符集太窄
    /// （比如退化成十进制）会让它可被暴力猜解，而这在本地 socket 上测不
    /// 出来——目录权限挡住了。
    #[test]
    fn generated_tokens_are_long_random_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 32, "128 位应为 32 个十六进制字符: {a}");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "必须是十六进制: {a}"
        );
        assert_ne!(a, b, "两次生成不能相同");
    }

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
