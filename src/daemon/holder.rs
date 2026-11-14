//! holder：真正持有 PTY 的进程。
//!
//! # 为什么需要这一层
//!
//! 终端进程的父进程就是**创建 PTY 的那个进程**。core 亲自建 PTY 的话，
//! core 一重启（升级、崩溃恢复）终端就跟着死——Linux 上实测 `ppid` 指向
//! core，core 退出后程序立刻消失。
//!
//! herdr 用两个进程解决：core 做协调、holder 持 PTY。core 可以随便重启，
//! holder 不动，终端毫发无损（它自己的 `server restart` 就明说
//! "programs keep running"）。
//!
//! # 本实现的形态
//!
//! `deepcore`（core）启动时：
//!
//! 1. 试着连 [`holder_socket_path`](super::holder_socket_path)；
//! 2. 连不上就拉起 `deepcore --holder`，等它就位；
//! 3. 之后所有 `spawn` 都转发给 holder，输出也由 holder 推回来。
//!
//! holder 只做一件事：持有 PTY + 维护 VT + 按 channel 推字节。它不理解
//! "标签""面板""SSH"，那些是 core 与 UI 的事。
//!
//! # 进程关系
//!
//! holder 由 core 拉起，但它**不能**随 core 一起死——否则白拆。所以
//! 拉起时用 `setsid`（Unix）把它放进新会话，并且 core 退出时不发任何
//! "请退出"信号。holder 自己只在这些情况下结束：
//!
//! - 收到 SIGTERM/SIGINT；
//! - 一个终端都不剩且长时间没有连接（见 `listen` 的空闲退出）。

/// holder 的命令行参数。
pub const HOLDER_FLAG: &str = "--holder";

/// 这个进程是不是被要求以 holder 身份运行。
pub fn is_holder_invocation(args: &[String]) -> bool {
    args.iter().any(|a| a == HOLDER_FLAG)
}

/// 启动 holder 的监听循环（阻塞）。
///
/// 与 core 的 `listen::run` 几乎一样，差别是：
/// - socket 路径用 [`super::holder_socket_path`]；
/// - **不**做空闲退出（holder 要一直活着等 core 回来），除非一个终端都没有。
pub fn run() -> std::io::Result<()> {
    super::listen::run_at(
        super::holder_socket_path(),
        // holder 没有终端就该退，否则用户每次关掉 deepterm 都留一个空进程。
        true,
        // holder 自己就是 PTY 的父进程，直接建，不再转给谁。
        super::connection::Backend::Local,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参数识别必须精确：`--holder` 出现在任意位置都算。
    #[test]
    fn the_holder_flag_is_recognised() {
        assert!(is_holder_invocation(&[
            "deepcore".into(),
            "--holder".into()
        ]));
        assert!(is_holder_invocation(&[
            "deepcore".into(),
            "--holder".into(),
            "--verbose".into()
        ]));
        assert!(!is_holder_invocation(&["deepcore".into()]));
        assert!(!is_holder_invocation(&["deepcore".into(), "--hold".into()]));
    }

    /// holder 与 core 必须是两个不同的 socket，否则会互相抢地址。
    #[test]
    fn the_two_processes_use_different_sockets() {
        assert_ne!(
            super::super::socket_path(),
            super::super::holder_socket_path()
        );
    }

    /// holder 用的是与 core 同一份 `Registry` 实现。
    ///
    /// holder 不理解标签/面板，core 不理解 PTY 细节，但"注册一个终端"
    /// 这件事两边完全一样——共用一份实现才不会有第二套逻辑漂移。
    /// 这条只断言类型可用，真正的行为由 `registry` 模块自己的测试覆盖。
    #[test]
    fn the_registry_type_is_usable_from_here() {
        use super::super::registry::Registry;
        assert!(Registry::new().is_empty());
    }
}
