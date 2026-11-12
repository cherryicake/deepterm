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
pub mod terminal;

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
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("deepterm");
    }
    crate::workspace::persistence::PersistenceManager::get_config_dir()
}

/// socket（或 Named Pipe）的完整路径。
pub fn socket_path() -> PathBuf {
    runtime_dir().join(SOCKET_FILE_NAME)
}

#[cfg(unix)]
mod transport {
    pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};
}

#[cfg(windows)]
mod transport {
    // Windows 上换 Named Pipe。实现留到阶段 7。
    pub use super::windows::pipe::{Listener, Stream};
}

#[cfg(unix)]
pub use transport::{Listener, Stream};

#[cfg(test)]
mod path_tests {
    use super::*;

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
