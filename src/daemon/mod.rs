//! 终端守护进程：让本地/远程终端在 deepterm 关闭后继续运行。
//!
//! 设计见 `docs/DESIGN_TERMINAL_PERSISTENCE.md`。这里只放与 UI 无关的部分：
//! 协议、socket 路径、终端登记。

pub mod bootstrap;
pub mod client;
pub mod connect;
pub mod connection;
pub mod core_holder;
pub mod deploy;
pub mod holder;
pub mod identity;
#[cfg(unix)]
pub mod launcher;
pub mod listen;
pub mod machines;
pub mod platform;
pub mod protocol;
pub mod pump;
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
            // SAFETY: `getuid` 无参数、无副作用，任何线程调用都安全。
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

/// `/tmp` 兜底目录是否可信：必须是本人拥有的真实目录且不可被他人写入。
///
/// # 为什么必须校验
///
/// `/tmp` 是 `01777`：任何用户都能预先建出 `/tmp/deepterm-<uid>` 或放一个
/// 指向自己目录的软链。之后守护进程会把 token 写进那个目录，而同机其他
/// 用户就能读到它——token 拿到即拿到该用户的所有终端。`set_permissions`
/// 在别人的目录上还会 `EPERM`，而这个错误很容易被 `let _ =` 吞掉。
///
/// # Errors
///
/// 目录不存在、不是目录、是软链、属主不是当前用户，或组/其他用户有任意
/// 权限位时返回错误。
#[cfg(unix)]
pub fn verify_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} 不是安全的目录（软链或非目录）", dir.display()),
        ));
    }
    // SAFETY: `getuid` 无参数、无副作用，任何线程调用都安全（只读内核凭证表）。
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} 属于 uid {}，当前用户是 {}",
                dir.display(),
                metadata.uid(),
                uid
            ),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} 权限过宽（{:o}），组/其他用户可访问",
                dir.display(),
                metadata.mode() & 0o777
            ),
        ));
    }
    Ok(())
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

/// 新 holder 的 socket 前缀：`holder.<id>.sock`。
pub const HOLDER_SOCKET_PREFIX: &str = "holder.";

/// holder 的 socket 路径。
pub fn holder_socket_path() -> PathBuf {
    runtime_dir().join(HOLDER_SOCKET_FILE_NAME)
}

/// 为一个新 holder 生成唯一 socket 路径。
///
/// 与 herdr 的 `<session>.holder.<id>.sock` 同构：core 不再假设“全世界只有
/// 一个 holder”。旧 holder 继续用 `holder.sock`，新 holder 各用各的路径，
/// 升级时互不打扰。
pub fn new_holder_socket_path() -> PathBuf {
    loop {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = runtime_dir().join(format!("{HOLDER_SOCKET_PREFIX}{}.sock", &id[..8]));
        if !path.exists() {
            return path;
        }
    }
}

/// 当前运行时目录里所有可能属于 holder 的 socket。
///
/// 同时接受旧版固定的 `holder.sock` 与新版 `holder.<id>.sock`。排序保证
/// 接管顺序稳定，排障日志可复现。
pub fn holder_socket_candidates() -> Vec<PathBuf> {
    let dir = runtime_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![dir.join(HOLDER_SOCKET_FILE_NAME)];
    };
    let mut sockets: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_holder = name == HOLDER_SOCKET_FILE_NAME
                || (name.starts_with(HOLDER_SOCKET_PREFIX) && name.ends_with(".sock"));
            is_holder.then(|| entry.path())
        })
        .collect();
    if sockets.is_empty() {
        sockets.push(dir.join(HOLDER_SOCKET_FILE_NAME));
    }
    sockets.sort();
    sockets
}

/// 校验远端独立运行时目录名（`sessions/<session>`）。
///
/// # 为什么必须在这里、且必须拒绝 `.` / `..`
///
/// 这个值会**拼进远端 shell 命令**（`machines::remote_connect_command`、
/// `deploy::remote_core_path`）和文件路径。只在 `MachineBook::ensure`
/// 校验是不够的：`machines.json` 是普通文件，回滚、手改、从别处拷一份
/// 过来都会绕过那次调用。而 `.`/`..` 虽然字符集合法，却会让
/// `sessions/..` 解析成安装根目录——"沙箱化安装"于是覆盖生产 core。
///
/// # Errors
///
/// 为空、超过 64 字节、含字母数字与 `-_.` 之外的字符，或恰为 `.`/`..`。
pub fn validate_session(session: &str) -> Result<(), String> {
    if session.is_empty()
        || session.len() > 64
        || matches!(session, "." | "..")
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(
            "session 只能包含字母、数字、'-'、'_'、'.'（最长 64 字节，且不能是 . 或 ..）".into(),
        );
    }
    Ok(())
}

/// 鉴权 token 的文件名。
pub const TOKEN_FILE_NAME: &str = "token";

/// 与指定 socket 配套的 token 路径。
///
/// **按 socket 名区分**（而不是用 `runtime_dir().join(...)` 那个固定路径）：
/// core 与 holder 是两个进程，共用一个 token 文件时后启动的会把先启动的
/// 覆盖掉，于是 core 拿自己的 token 去连 holder 必然被拒——表现为"spawn
/// 说成功了，holder 里却什么都没有"。
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
/// token 写在 `token_path_for(socket)`，文件权限 0600。它随运行时目录一起创建，
/// 不是长期凭据：守护进程重启就换一个，不落任何"用户配置"。
pub fn generate_token() -> String {
    // 用 UUIDv4 的 128 位随机数：它在各平台都走操作系统的 CSPRNG
    // （Unix 是 `/dev/urandom`，Windows 是 `BCryptGenRandom`），一次取满
    // 16 字节。
    //
    // # 不能退回"时间 + pid"
    //
    // 旧实现在读不到 `/dev/urandom` 时用 `nanos ^ pid` 兜底，而 Windows
    // 上 `/dev/urandom` **永远**打不开——那条兜底就是 Windows 的默认路径。
    // 更糟的是 `nanos` 装得进 `u64`，`to_le_bytes()[..16]` 的后 8 字节
    // 恒为零：32 个十六进制字符里有一半是常量，实际只有 64 位熵，而
    // token 是同机任何进程都能尝试连接的命名管道的唯一凭据。
    uuid::Uuid::new_v4().simple().to_string()
}

/// 写入 token 文件（0600）。
///
/// **创建时就带 0600**，不能 `fs::write` 之后补 `chmod`：那会在两步之间
/// 留下一个组/其他用户可读的窗口，而 `chmod` 本身失败时（FAT、特殊挂载、
/// 未来的便携构建）宽权限会永久保留。事后那次 `set_permissions` 只用于
/// 收紧历史版本留下的宽权限文件，不承担首建语义。
pub fn write_token_for(socket: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let path = token_path_for(socket);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(token.as_bytes())?;
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

    /// 半关闭写端。`deepcore connect` 的 stdin 结束时用它通知 core。
    pub fn shutdown_write(stream: &Stream) -> std::io::Result<()> {
        stream.shutdown(std::net::Shutdown::Write)
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

    /// Windows 命名管道没有与 Unix 对应的半关闭；连接由进程结束时关闭。
    pub fn shutdown_write(_stream: &Stream) -> std::io::Result<()> {
        Ok(())
    }

    /// 运行时目录 + 固定文件名 → `\\.\pipe\deepterm-<user>-<dir>`。
    ///
    /// 用用户名而不是路径：管道名是全局命名空间，塞进完整路径既没必要
    /// 也更容易撞上非法字符。
    ///
    /// # 必须把 `path` 也编进名字
    ///
    /// 旧实现直接丢弃 `path`，于是 `DEEPTERM_RUNTIME_DIR` 在 Windows 上
    /// **完全不生效**：两套并行安装、两个测试分片、一个残留的旧守护进程
    /// 全都落在同一个管道上互相串话。这里取路径的稳定哈希后缀区分。
    pub fn address(path: &std::path::Path) -> String {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        // 只用文件名的部分没有意义（都是 `daemon.sock`）；对**整个路径**
        // 取一个短哈希，保证不同运行时目录互不干扰。
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        path.hash(&mut hasher);
        let suffix = hasher.finish();
        format!("{PIPE_PREFIX}-{user}-{suffix:016x}")
    }
}

pub use transport::{Listener, Stream};

/// 串行化所有会改动 `DEEPTERM_RUNTIME_DIR` / `DEEPTERM_DEEPCORE` 的测试。
///
/// # 为什么需要这把锁
///
/// 这两个环境变量是**进程全局**的，而 `cargo test` 默认多线程并行。四个
/// 模块的用例（`daemon::mod`、`daemon::client`、`daemon::session`、
/// `single_instance`）各自 `set_var` 一个临时目录再 `remove_var`，交错执行
/// 时会出现：
///
/// - A 设了 `/tmp/a`，B 紧接着设成 `/tmp/b`，A 的断言读到 `/tmp/b` → 假失败；
/// - A 正在用它自己的目录，B 提前 `remove_var` → A 掉回真实配置目录，
///   于是**去连用户真实的守护进程**（最坏情况：测试把用户的终端列表搅了）。
///
/// 用一把全局互斥锁把所有这类用例串起来。锁中毒不影响测试：一个用例 panic
/// 之后其它用例仍应继续跑（`into_inner`），否则一个失败会把整批测试拖黑。
///
/// 注意：这把锁只解决**同一进程内**的竞争。跨进程（`cargo test` 多开）仍需
/// 各自给不同的目录，见各用例里的路径构造。
#[cfg(test)]
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 在测试期间把运行时目录指到 `dir`，作用域结束时恢复原值。
///
/// 比手写 `set_var`/`remove_var` 安全：用例中途 panic 时也会恢复，不会把
/// 污染留给后续用例。返回值必须绑定到变量（`let _guard = ...`），否则会
/// 立刻析构。
#[cfg(test)]
pub struct RuntimeDirGuard {
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl RuntimeDirGuard {
    pub fn new(dir: impl AsRef<std::path::Path>) -> Self {
        let lock = env_lock();
        let previous = std::env::var_os(RUNTIME_DIR_ENV);
        // SAFETY:调用方持有 `env_lock()`，本进程内没有别人同时改这个变量。
        unsafe { std::env::set_var(RUNTIME_DIR_ENV, dir.as_ref()) };
        Self {
            previous,
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for RuntimeDirGuard {
    fn drop(&mut self) {
        // SAFETY:仍持有 `env_lock()`。
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(RUNTIME_DIR_ENV, value),
                None => std::env::remove_var(RUNTIME_DIR_ENV),
            }
        }
    }
}

/// 在测试期间把某个环境变量设成指定值，作用域结束时恢复原值。
///
/// 与 [`RuntimeDirGuard`] 同源，只是变量名可指定（`DEEPTERM_DEEPCORE` 之类）。
/// 同样持 [`env_lock`]，因此**不能**与 `RuntimeDirGuard` 同时使用——那会
/// 在同一线程上重入同一把锁并死锁。需要同时改两个变量的用例请手动持有
/// [`env_lock`]。
#[cfg(test)]
pub struct EnvGuard {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl EnvGuard {
    pub fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let lock = env_lock();
        let previous = std::env::var_os(name);
        // SAFETY:调用方持有 `env_lock()`。
        unsafe { std::env::set_var(name, value.as_ref()) };
        Self {
            name,
            previous,
            _lock: lock,
        }
    }
}

/// 同时隔离**运行时目录**与**守护进程二进制**的测试环境。
///
/// 单独用 [`RuntimeDirGuard`] 不够：`session::spawn_terminal` 在连不上时会
/// 去拉起守护进程，而查找路径的兜底是 PATH——见
/// `session::disable_daemon_spawn` 的说明。两件事一起做才能保证"没有
/// 守护进程"这条断言是确定性的。
#[cfg(test)]
pub struct IsolatedTestEnv {
    name: &'static str,
    previous_daemon: Option<std::ffi::OsString>,
    previous_dir: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl IsolatedTestEnv {
    /// 运行时目录指向 `dir`，且禁止自动拉起守护进程。
    pub fn new(dir: impl AsRef<std::path::Path>) -> Self {
        let lock = env_lock();
        let previous_daemon = std::env::var_os(client::DAEMON_ENV);
        let previous_dir = std::env::var_os(RUNTIME_DIR_ENV);
        // SAFETY:持有 `env_lock()`，本进程内没有别人同时改这两个变量。
        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV, dir.as_ref());
            std::env::set_var(client::DAEMON_ENV, "/nonexistent/deepcore-for-tests");
        }
        Self {
            name: client::DAEMON_ENV,
            previous_daemon,
            previous_dir,
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for IsolatedTestEnv {
    fn drop(&mut self) {
        // SAFETY:仍持有 `_lock`。
        unsafe {
            match self.previous_dir.take() {
                Some(value) => std::env::set_var(RUNTIME_DIR_ENV, value),
                None => std::env::remove_var(RUNTIME_DIR_ENV),
            }
            match self.previous_daemon.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY:仍持有 `_lock`。
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

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
        let _guard = super::RuntimeDirGuard::new("/tmp/deepterm-test-rt");
        assert_eq!(runtime_dir(), PathBuf::from("/tmp/deepterm-test-rt"));
        assert_eq!(
            socket_path(),
            PathBuf::from("/tmp/deepterm-test-rt/daemon.sock")
        );
    }

    /// 每个新 holder 必须有自己的 socket。
    ///
    /// 固定复用 `holder.sock` 会让升级后的 core 永远接到旧 holder 上：旧
    /// holder 不懂 `Meta`，GUI 重启后远程 Tab 只会显示 shell，程序图标与
    /// agent 状态都丢了。Herdr 用 `<session>.holder.<id>.sock` 的原因相同。
    #[test]
    fn new_holders_get_unique_socket_names() {
        let dir = format!(
            "/tmp/deepterm-holder-name-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let _guard = super::RuntimeDirGuard::new(&dir);
        let first = new_holder_socket_path();
        let second = new_holder_socket_path();
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(PathBuf::from(&dir).as_path()));
        assert!(
            first
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(
                    |name| name.starts_with(HOLDER_SOCKET_PREFIX) && name.ends_with(".sock")
                )
        );
    }
}
