//! Windows 控制台子进程的窗口抑制。
//!
//! deepterm 在 release 下是 `windows_subsystem = "windows"` 的 GUI 进程，
//! 本身不弹控制台；但它要**探测/调用** `powershell.exe`、`wsl.exe` 这类
//! 控制台子进程（例如为新标签页挑默认 shell）。Windows 会为这样的子进程
//! 新分配一个控制台并把窗口显示出来，用户看到的就是「标签页还没出来，
//! 先闪一个黑框」。
//!
//! `CREATE_NO_WINDOW` 让子进程照常跑（stdout/stderr 仍走管道），只是不再
//! 分配可见控制台。它是 Windows 终端类程序消除该闪烁的标准做法。
//!
//! 非 Windows 平台是空操作：unix 下子进程本就没有窗口概念，且
//! `creation_flags` 不是 unix 上的 API。

use std::process::Command;

/// 让 `command` 启动的控制台子进程不弹出控制台窗口（仅 windows 生效）。
///
/// 用法：
/// ```ignore
/// let out = hide_console_window(&mut std::process::Command::new("powershell.exe"))
///     .args(["-NoProfile", "-Command", "exit 0"])
///     .output()?;
/// ```
///
/// # 为什么不用 `CREATE_NEW_CONSOLE` / `DETACHED_PROCESS`
///
/// 后两者会让子进程脱离本进程的控制台与进程组，键盘焦点、`CTRL_C` 行为与
/// 退出回收都随之改变；这里只需要「不显示窗口」，`CREATE_NO_WINDOW` 是
/// 影响面最小的一个。
pub fn hide_console_window(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// 不为控制台子进程分配可见窗口（`CREATE_NO_WINDOW`）。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// `hide_console_window` 的「建好并抑制」便捷版，省掉调用点的可变借用。
pub fn hidden_command(program: &str) -> Command {
    let mut command = Command::new(program);
    hide_console_window(&mut command);
    command
}
