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
//! holder 由 core 拉起，但它**不能**随 core 一起死——否则白拆。Linux 上
//! 优先交给 `systemd-run --user` 托管，没有 user manager 时退回 `setsid`；
//! core 退出时不发任何"请退出"信号。holder 自己只在这些情况下结束：
//!
//! - 收到 SIGTERM/SIGINT；
//! - 一个终端都不剩且长时间没有连接（见 `listen` 的空闲退出）。

/// holder 的命令行参数。
pub const HOLDER_FLAG: &str = "--holder";

/// 指定 holder socket 的参数。
///
/// herdr 的每个 holder 都用自己的 socket（`<base>.<id>.sock`），core 启动时
/// 扫描并接管所有还活着的 holder。固定一个 `holder.sock` 会让新 core 永远
/// 接到旧 holder 上：旧 holder 没有新能力，重启后的标签就同步不到前台程序。
pub const HOLDER_SOCKET_FLAG: &str = "--socket";

/// 当前进程是否已被新 core 要求 drain。
///
/// 只有 holder 会真的进入这个状态。core 进程的同名原子量永远为 false。
static DRAINING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// holder 是否在 drain：不再接新终端，现有终端结束后退出。
pub fn draining() -> bool {
    DRAINING.load(std::sync::atomic::Ordering::SeqCst)
}

/// 标记 drain。重复调用无副作用。
pub fn mark_draining() {
    DRAINING.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// 这个进程是不是被要求以 holder 身份运行。
pub fn is_holder_invocation(args: &[String]) -> bool {
    args.iter().any(|a| a == HOLDER_FLAG)
}

/// 从命令行取 holder 的 socket 路径。
///
/// 支持 `--socket PATH` 与 `--socket=PATH`。没给时退回旧的固定路径，保证
/// 手工启动的 holder 与旧脚本仍可用。
///
/// # Errors
///
/// 写了 `--socket` 却没给值：退回固定路径会把 holder 绑到**旧地址**上，
/// 新 core 扫不到它，用户的终端会从界面上消失。
pub fn socket_from_args(args: &[String]) -> Result<Option<std::path::PathBuf>, String> {
    let mut iter = args
        .iter()
        .skip_while(|arg| arg.as_str() != HOLDER_SOCKET_FLAG);
    if iter.next().is_some() {
        return iter
            .next()
            .map(|value| Some(std::path::PathBuf::from(value)))
            .ok_or_else(|| format!("{HOLDER_SOCKET_FLAG} 缺少路径参数"));
    }
    Ok(args
        .iter()
        .find_map(|arg| arg.strip_prefix("--socket="))
        .map(std::path::PathBuf::from))
}

/// 启动 holder 的监听循环（阻塞）。
///
/// 与 core 的 `listen::run` 几乎一样，差别是：
/// - socket 路径用 [`super::holder_socket_path`]；
/// - **不**做空闲退出（holder 要一直活着等 core 回来），除非一个终端都没有。
pub fn run() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let socket = socket_from_args(&args)
        .map_err(std::io::Error::other)?
        .unwrap_or_else(super::holder_socket_path);
    super::platform::prepare_holder();
    super::listen::run_at(
        socket,
        // holder 没有终端就该退，否则用户每次关掉 deepterm 都留一个空进程。
        true,
        // holder 自己就是 PTY 的父进程，直接建，不再转给谁。
        super::connection::Backend::Local,
        super::machines::Machines::empty(),
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

    /// 新 holder 必须能按命令行拿到自己的 socket；解析错会让第二个 holder
    /// 抢同一个地址，结果还是回到“永远复用旧 holder”。
    #[test]
    fn the_socket_argument_is_parsed_both_ways() {
        let path = "/run/user/1000/deepterm/holder.1234abcd.sock";
        assert_eq!(
            socket_from_args(&[
                "deepcore".into(),
                HOLDER_FLAG.into(),
                HOLDER_SOCKET_FLAG.into(),
                path.into(),
            ])
            .expect("应解析成功"),
            Some(std::path::PathBuf::from(path))
        );
        assert_eq!(
            socket_from_args(&["deepcore".into(), format!("--socket={path}")]).expect("应解析成功"),
            Some(std::path::PathBuf::from(path))
        );
        assert_eq!(
            socket_from_args(&["deepcore".into()]).expect("缺省不是错误"),
            None
        );
        // `--socket` 没有值：必须报错，而不是静默退回固定路径。
        assert!(socket_from_args(&["deepcore".into(), HOLDER_SOCKET_FLAG.into()]).is_err());
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
