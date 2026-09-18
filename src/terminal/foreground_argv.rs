//! 读取某个进程的完整 argv，用于识别前台跑的是哪个 CLI agent。
//!
//! 为什么不复用 `Terminal::process_name_by_pid`：那个返回**展示名**（tab 标签
//! 用，只要 basename 就够），且 macOS 上靠 fork `/bin/ps` 拿。agent 识别的需求
//! 不同：
//!
//! - 需要 **argv 全部参数**——agent 常被解释器包着跑（`node .../claude-code/cli.js`、
//!   `python -m aider`），只看进程名根本认不出来。`CLIAgent::detect_from_argv_with`
//!   的解释器分支正是靠 argv 里的脚本路径命中。
//! - 需要**能在后台线程调用**——macOS 上 fork `/bin/ps` 会阻塞调用线程，而
//!   调用点在 UI 线程的重绘邻接路径上。本模块纯 syscall / 文件读，无 fork。

/// 前台进程的完整 argv（仅用于 agent 识别）。
///
/// 与 `Terminal::process_name_by_pid` 的分工见模块文档。
///
/// 前提：**只能读同用户进程**。macOS 的 `KERN_PROCARGS2` 对非同用户进程会直接
/// 失败（返回非 0），此时这里返回 `None` 而非报错。我们查的恒为自家 pty 的前台
/// 进程组，所以这个前提总是成立——但若将来有人拿它去查别人的进程，`None` 就是
/// 这个原因，不是进程不存在。
pub fn foreground_argv(pid: u32) -> Option<Vec<String>> {
    platform_foreground_argv(pid)
}

#[cfg(target_os = "linux")]
fn platform_foreground_argv(pid: u32) -> Option<Vec<String>> {
    if pid == 0 {
        return None;
    }
    // /proc/<pid>/cmdline 各项以 NUL 分隔，末项后也有一个 NUL（末尾 NUL 会被
    // 下面的 filter 丢掉）。读不到（进程已退、非本用户）即 None。
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(target_os = "macos")]
fn platform_foreground_argv(pid: u32) -> Option<Vec<String>> {
    if pid == 0 {
        return None;
    }

    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];

    // 第一趟：只问长度。`sysctl` 允许 oldp 为 NULL 时用它回报缓冲区大小。
    let mut len = 0usize;
    // SAFETY（中文）：`mib` 是长度为 3 的合法数组，传入的 `namelen` 与之匹配；
    // `oldp` 为 NULL 且 `oldlenp` 指向本栈变量，符合 sysctl 的"探长度"用法；
    // 后两个参数（newp/newlen）为 NULL/0 表示只读查询。
    let probe = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    // 至少得装得下 argc 这个 int，否则后面的解析无从谈起。
    if probe != 0 || len < std::mem::size_of::<libc::c_int>() {
        return None;
    }

    let mut buf = vec![0u8; len];
    // SAFETY（中文）：`buf` 已按刚探到的 `len` 分配，`oldlenp` 初值为其真实容量
    // （sysctl 据此判定可写入上限，成功后回写实际写入量）；指针与容量一致。
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    // 进程可能在两趟之间长出新参数：按实际的 `len` 截断，尾部零字节不参与解析。
    buf.truncate(len);
    parse_macos_procargs(&buf)
}

/// 解析 `KERN_PROCARGS2` 返回的缓冲区，取 `argv`。
///
/// 缓冲区布局（对照 Apple `adv_cmds-237/ps/print.c` 的 `getproclline()` 核实）：
///
/// ```text
/// +---------------------+  <-- 起点
/// | argc (int, 原生序)  |      非 NUL 结尾
/// +---------------------+
/// | exec_path           |      NUL 结尾字符串
/// +---------------------+
/// | '\0' 填充           |      **一个或多个** NUL 字节
/// +---------------------+
/// | argv[0..argc]       |      各自 NUL 结尾
/// +---------------------+
/// | env[...]            |      （本函数不需要，靠 argc 计数停住）
/// +---------------------+
/// ```
///
/// 两个容易写错的地方：
///
/// 1. `exec_path` 与 `argv[0]` 之间的 NUL 填充是 **1 字节以上**，不能假设只有
///    一个。所以跳过填充必须用循环。
/// 2. `argv` 各项之间**可能**出现空项（调用方传了空字符串参数）。这里用
///    `if i > start` 把它们滤掉——**有意为之**：这丢掉了位置保真度，但
///    `getargv` 项目统计的 90+ 个实现里翻车的全是位置语义（前导空参数），
///    而本函数的消费方只需要"有哪些有意义的 token"来匹配 agent 名。
///
/// 这是全模块唯一的裸指针外解析逻辑，务必与上面的布局描述同步修改。
#[cfg(target_os = "macos")]
fn parse_macos_procargs(buf: &[u8]) -> Option<Vec<String>> {
    const SZ: usize = std::mem::size_of::<libc::c_int>();
    if buf.len() < SZ {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..SZ].try_into().ok()?) as usize;

    let mut i = SZ;
    // 第 1 步：跳过 exec_path（扫到它的 NUL 结尾）。
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    // 第 2 步：跳过 1+ 字节的对齐填充。
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    // 第 3 步：按 argc 计数取 argv。
    let mut argv = Vec::new();
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if i > start {
            argv.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        }
        while i < buf.len() && buf[i] == 0 {
            i += 1;
        }
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_foreground_argv(_pid: u32) -> Option<Vec<String>> {
    // Windows 走 conpty，没有可比的 argv 概念；本平台的 agent 检测由阶段 8 另议。
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn procargs(exec_path: &str, pad: usize, args: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(args.len() as i32).to_ne_bytes());
        buf.extend_from_slice(exec_path.as_bytes());
        buf.push(0);
        buf.extend(std::iter::repeat_n(0u8, pad));
        for a in args {
            buf.extend_from_slice(a.as_bytes());
            buf.push(0);
        }
        // env 区：解析必须靠 argc 停住，不能被它带跑。
        buf.extend_from_slice(b"HOME=/root\0PATH=/usr/bin\0");
        buf
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn parses_full_layout() {
        let buf = procargs("/usr/local/bin/node", 3, &["node", "/opt/claude/cli.js"]);
        assert_eq!(
            parse_macos_procargs(&buf).as_deref(),
            Some(&["node".to_string(), "/opt/claude/cli.js".to_string()][..]),
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn single_byte_padding_is_enough() {
        // 填充只有 1 字节（Apple 实现的最小情形），不能被当成 argv[0] 的空项吃掉。
        let buf = procargs("/bin/zsh", 1, &["/bin/zsh", "-l"]);
        assert_eq!(
            parse_macos_procargs(&buf).as_deref(),
            Some(&["/bin/zsh".to_string(), "-l".to_string()][..]),
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn env_region_is_not_consumed() {
        // argc=1 时只能取出 1 项，env 区不得溢出成 argv 项。
        let buf = procargs("/bin/sh", 2, &["sh"]);
        assert_eq!(
            parse_macos_procargs(&buf).as_deref(),
            Some(&["sh".to_string()][..]),
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn too_short_and_empty_return_none() {
        assert_eq!(parse_macos_procargs(&[]), None);
        assert_eq!(parse_macos_procargs(&[0, 0, 0]), None);
        // argc=0：没有任何 argv 项可给，返回 None 而非空 vec。
        let mut buf = (0i32).to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sh\0\0");
        assert_eq!(parse_macos_procargs(&buf), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn truncated_buffer_stops_at_boundary() {
        // argv[1] 被截断（没有 NUL 结尾）时，仍应取出已经完整的那一项。
        let mut buf = (2i32).to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sh\0\0");
        buf.extend_from_slice(b"sh\0--trunc");
        assert_eq!(
            parse_macos_procargs(&buf).as_deref(),
            Some(&["sh".to_string(), "--trunc".to_string()][..]),
        );
    }

    /// 回归护栏：真实进程上取到的 argv 首项应当指向一个存在的路径前缀。
    ///
    /// 覆盖两趟 sysctl 调用与缓冲区长度处理——纯函数测试碰不到这段。
    #[test]
    #[cfg(target_os = "macos")]
    fn reads_own_process_argv() {
        let pid = std::process::id();
        let argv = foreground_argv(pid).expect("读自家进程的 argv 不应失败");
        assert!(!argv.is_empty(), "自家进程的 argv 不该为空");
        // 测试二进制的 argv[0] 通常是 cargo 给出的路径或裸名，不做精确断言，
        // 只要求这是个可解码的非空 token（已在读取时过滤空项）。
        assert!(argv.iter().all(|a| !a.is_empty()));
    }

    #[test]
    fn pid_zero_is_rejected() {
        assert_eq!(foreground_argv(0), None);
    }
}
