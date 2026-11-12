//! 终端守护进程 `deepcore`。
//!
//! # 它解决什么问题
//!
//! deepterm 关掉窗口后，终端里的程序要继续跑。做法是把 PTY 从 GUI 进程里
//! 搬出来：`deepcore` 常驻并持有所有 PTY，GUI 只是挂接上去看和打字。
//! 窗口关掉 → GUI 的连接断掉 → PTY 还在 `deepcore` 手里 → 程序不知道
//! 有人走了，继续跑。
//!
//! 设计细节见 `docs/DESIGN_TERMINAL_PERSISTENCE.md`。
//!
//! # 为什么是独立二进制而不是 `deepterm __daemon`
//!
//! 1. **生命周期独立**。GUI 崩溃、被系统杀掉、升级重装，都不该牵动守护进程。
//!    同一个二进制时，进程管理器把它俩当一回事（比如 `pkill deepterm`
//!    会连守护进程一起杀掉，而这恰恰是它要活过的那件事）。
//! 2. **不链接图形栈**。作为 `deepterm` 的子命令时，守护进程和 GUI 共用
//!    同一份二进制，里面带着 GPUI、wgpu、字体栈。它永远用不到这些，
//!    却要为此付出启动时间与内存。
//! 3. **权限与打包边界清楚**。`deepcore` 单独一个文件，便于单独设
//!    capability（Windows）或放进 systemd user unit（Linux）。
//!
//! 代价是版本一致性要靠约定保证：GUI 连上时用 `ping` 拿版本号，对不上
//! 就重新拉起（见 `daemon::client`）。

use deepterm::utils::logging::{self, CappedLog};

fn main() {
    // 守护进程的日志单独一份：和 GUI 混写会互相裁尾，
    // 而排查"终端为什么没恢复"正需要两边的时间线各自完整。
    let mut builder = logging::base_log_builder(logging::default_level());
    let mut log_path = None;
    if let Some(path) = logging::core_log_path() {
        match CappedLog::open(path.clone()) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
                log_path = Some(path);
            }
            Err(e) => {
                eprintln!("deepcore: cannot open log file {}: {e}", path.display());
            }
        }
    }
    builder.init();
    // 守护进程更要装：它崩了没有窗口、没有 stderr，只剩日志这一条线索。
    logging::install_panic_hook();

    log::info!("=== deepcore 启动 ===");
    if let Some(path) = log_path {
        log::info!("日志文件: {}", path.display());
    }

    match deepterm::daemon::listen::run() {
        Ok(()) => log::info!("=== deepcore 正常退出 ==="),
        Err(e) => {
            log::error!("守护进程启动失败: {e}");
            eprintln!("deepcore: {e}");
            std::process::exit(1);
        }
    }
}
