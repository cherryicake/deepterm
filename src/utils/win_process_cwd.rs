//! Windows：读另一个进程的当前工作目录。
//!
//! # 为什么要读 PEB
//!
//! Windows **没有**查询他进程 cwd 的公开 API——macOS 那边是 `lsof`，Linux
//! 那边是 `/proc/<pid>/cwd` 这个符号链接，Windows 两者都没有。唯一途径是走
//! ntdll 读目标进程自己的 PEB：
//!
//! ```text
//! NtQueryInformationProcess(ProcessBasicInformation) → PEB 基址
//! PEB.ProcessParameters                          → RTL_USER_PROCESS_PARAMETERS
//! RTL_USER_PROCESS_PARAMETERS.CurrentDirectory   → CURDIR { UNICODE_STRING DosPath }
//! UNICODE_STRING.Buffer/Length                   → UTF-16 路径
//! ```
//!
//! `CurrentDirectory` 是**启动时**的工作目录，内核不会随 `SetCurrentDirectory`
//! 之外的路径变化而更新它——但对交互式 shell 来说，用户敲 `cd` 就是在改进程
//! 的 cwd，而 Windows 上 `cd` 确实会调用 `SetCurrentDirectory`。注意：只有
//! 当前目录所在卷的**目录句柄**被缓存，`CurrentDirectory.DosPath` 才是权威
//! 值，两者一致；因此本实现读出来的就是 shell 当前的 cwd。
//!
//! # 取不到就返回 None
//!
//! 进程已退出、目标提权、跨用户会话时都会失败。这与 macOS 上 `lsof` 查不到
//! 同义，**不是错误**：调用方（`PathResolver::query_process_working_directory`
//! 的使用者）把 `None` 当「本次查不到」，退回上一次缓存值或启动目录。
//!
//! 代价：每次查询要 `OpenProcess` + 3 次 `ReadProcessMemory`，比 unix 的
//! `/proc` 读法重。因此调用方本来就有 TTL（2s）与 in-flight 去抖，不会高频调用。

use std::path::PathBuf;

use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Threading::{
    IsWow64Process, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_BASIC_INFORMATION,
    PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};

/// 读 PEB 所需的权限：`PROCESS_VM_READ` 是硬要求（要读目标进程内存），
/// `PROCESS_QUERY_INFORMATION` 用于拿基本信息。
const READ_PEB_ACCESS: PROCESS_ACCESS_RIGHTS =
    PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_INFORMATION.0 | PROCESS_VM_READ.0);

/// 路径长度的上限，用来挡住「读到垃圾值就巨额分配」。
/// `MAX_PATH` 之外再留一倍余量给长路径（\\?\ 前缀）。
const MAX_CWD_BYTES: usize = 64 * 1024;

/// 目标进程的位宽。
///
/// 32 位目标（WOW64）跑在 64 位宿主上时，PEB 是 **32 位布局**：偏移与指针宽度
/// 全都不同。读错会拿到野指针，所以必须先问一句。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetBits {
    X86,
    X64,
}

impl TargetBits {
    /// `PEB.ProcessParameters` 相对 PEB 基址的偏移。
    const fn process_parameters_offset(self) -> usize {
        match self {
            Self::X86 => 0x10,
            Self::X64 => 0x20,
        }
    }

    /// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory` 相对参数块基址的偏移。
    const fn current_directory_offset(self) -> usize {
        match self {
            Self::X86 => 0x24,
            Self::X64 => 0x38,
        }
    }

    /// `UNICODE_STRING` 的大小（x64 有 4 字节对齐填充）。
    const fn unicode_string_size(self) -> usize {
        match self {
            Self::X86 => 8,
            Self::X64 => 16,
        }
    }

    /// `UNICODE_STRING.Buffer` 在结构内的偏移。
    const fn unicode_buffer_offset(self) -> usize {
        match self {
            Self::X86 => 4,
            Self::X64 => 8,
        }
    }
}

/// 进程句柄的 RAII 守卫：每条提前返回的路径都必须 `CloseHandle`，否则句柄泄漏。
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    /// 打开 `pid`；失败返回 `None`（进程已退出 / 无权限）。
    fn open(pid: u32) -> Option<Self> {
        // 成功返回的 HANDLE 一定非空，`OpenProcess` 失败时返回 Error 而非空句柄。
        let handle = unsafe { OpenProcess(READ_PEB_ACCESS, false, pid) }.ok()?;
        Some(Self(handle))
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// 从目标进程读 `size` 字节到 `buf`；任何失败都返回 `false`。
///
/// 目标随时可能退出或改地址，所以每次读都要独立判定，不能假定「上次成功这次也成功」。
fn read_remote(handle: HANDLE, address: usize, buf: &mut [u8]) -> bool {
    if address == 0 {
        return false;
    }
    let mut read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            handle,
            address as *const std::ffi::c_void,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            buf.len(),
            Some(&mut read),
        )
    }
    .is_ok();
    ok && read == buf.len()
}

/// 读目标进程里的一个指针宽度整数。
fn read_remote_usize(handle: HANDLE, address: usize, bits: TargetBits) -> Option<usize> {
    let mut raw = [0u8; std::mem::size_of::<usize>()];
    if !read_remote(handle, address, &mut raw) {
        return None;
    }
    Some(match bits {
        // 32 位目标：只取低 4 字节，高 4 字节是别的字段，不能当指针用。
        TargetBits::X86 => u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize,
        TargetBits::X64 => usize::from_le_bytes(raw),
    })
}

/// 目标进程是 WOW64（32 位）还是原生 64 位。
fn detect_target_bits(handle: HANDLE) -> TargetBits {
    // 本进程是 32 位时，32 位目标无需判断；64 位目标在 32 位宿主上本来就读不了，
    // 后面任何一次读失败都会安全地返回 None。
    if cfg!(target_pointer_width = "32") {
        return TargetBits::X86;
    }
    let mut wow64 = BOOL(0);
    let ok = unsafe { IsWow64Process(handle, &mut wow64) }.is_ok();
    if ok && wow64.0 != 0 {
        TargetBits::X86
    } else {
        TargetBits::X64
    }
}

/// 取 `pid` 的当前工作目录；查不到返回 `None`。
pub fn query_process_working_directory(pid: u32) -> Option<PathBuf> {
    let process = ProcessHandle::open(pid)?;
    let handle = process.0;

    // 1) PEB 基址
    let mut pbi: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            ProcessBasicInformation,
            &mut pbi as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    // NTSTATUS >= 0 即成功（STATUS_SUCCESS == 0）。
    if status.0 < 0 || pbi.PebBaseAddress.is_null() {
        return None;
    }
    let peb = pbi.PebBaseAddress as usize;

    let bits = detect_target_bits(handle);

    // 2) PEB.ProcessParameters
    let params = read_remote_usize(handle, peb + bits.process_parameters_offset(), bits)?;
    if params == 0 {
        return None;
    }

    // 3) RTL_USER_PROCESS_PARAMETERS.CurrentDirectory（内含 UNICODE_STRING）
    let mut us = [0u8; 16];
    if !read_remote(
        handle,
        params + bits.current_directory_offset(),
        &mut us[..bits.unicode_string_size()],
    ) {
        return None;
    }

    // 4) UNICODE_STRING { u16 Length; u16 MaximumLength; [pad]; PWSTR Buffer }
    let length_bytes = u16::from_le_bytes([us[0], us[1]]) as usize;
    if length_bytes == 0 || !length_bytes.is_multiple_of(2) || length_bytes > MAX_CWD_BYTES {
        return None;
    }
    let buffer = read_remote_usize(
        handle,
        // Buffer 是**目标进程里**的地址，读它的值也要用目标位宽解释。
        params + bits.current_directory_offset() + bits.unicode_buffer_offset(),
        bits,
    )?;

    // 5) 读 UTF-16LE 路径本体
    //
    // 直接读进字节缓冲再按小端两两合成 u16：先建 u16 缓冲再手工拼字节既多一次
    // 分配，又把「读成功」与「字节序正确」拆成两件事。
    let mut raw = vec![0u8; length_bytes];
    if !read_remote(handle, buffer, &mut raw) {
        return None;
    }
    let wide: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    let path = String::from_utf16_lossy(&wide);
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 拿自己的 cwd 打底：读**自己**的 PEB 是最省事的正例——进程一定活着、
    /// 一定有权限，不受 CI 沙箱里「杀不掉子进程」之类的干扰。
    #[test]
    fn reads_own_working_directory() {
        let pid = std::process::id();
        let got = query_process_working_directory(pid).expect("应能读到自己的 cwd");
        let want = std::env::current_dir().expect("current_dir");
        // Windows 路径大小写不敏感，且 PEB 里可能是 8.3 短名，统一规范化后比较。
        let norm = |p: &std::path::Path| {
            p.to_string_lossy()
                .to_lowercase()
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_string()
        };
        assert_eq!(norm(&got), norm(&want), "读到的 cwd 应与 current_dir 一致");
    }

    /// 子进程：起一个 cwd 已知的进程，读它的 cwd。
    ///
    /// 这是真实用例的形状（`TerminalView::local_process_cwd` 查的就是 shell
    /// 子进程）。用 `cmd.exe`（不带 `/c`，它会一直等输入）保证查询期间进程活着。
    #[test]
    fn reads_child_process_working_directory() {
        // 避开 8.3 短名：用 temp 下的固定长目录名。
        let dir = std::env::temp_dir().join("deepterm-cwd-probe-dir");
        let _ = std::fs::create_dir_all(&dir);
        let mut child = crate::utils::console::hidden_command("cmd.exe")
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("启动 cmd.exe");

        let got = query_process_working_directory(child.id());
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);

        let got = got.expect("应能读到子进程 cwd");
        let norm = |p: &std::path::Path| {
            p.to_string_lossy()
                .to_lowercase()
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_string()
        };
        assert_eq!(
            norm(&got),
            norm(&dir),
            "子进程 cwd 应等于启动时指定的 current_dir"
        );
    }

    /// **决定性用例**：shell 里 `cd` 之后，PEB 里的 `CurrentDirectory` 是否
    /// 跟着变。
    ///
    /// 这条必须实测而不是想当然——`SetCurrentDirectory` 到底会不会回写
    /// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath` 是实现细节。
    /// 若它只在进程创建时写一次，本模块读到的就永远是启动目录，整个
    /// 「文件树跟随终端 cwd」在 Windows 上就是死的。
    #[test]
    fn tracks_cd_within_a_live_shell() {
        // 目录必须走 **ASCII** 路径：cmd.exe 按 OEM 代码页读 stdin，而 Rust 写出
        // 的是 UTF-8，路径里带非 ASCII 会让 shell 报「系统找不到指定的路径」，
        // 测的就不是 PEB 了。（temp 目录含非 ASCII 用户名，所以改用 Public。）
        let base = std::env::var("PUBLIC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("C:/Windows/Temp"));
        let first = base.join("dt-cwd-probe-first");
        let second = base.join("dt-cwd-probe-second");
        let _ = std::fs::create_dir_all(&first);
        let _ = std::fs::create_dir_all(&second);
        assert!(first.is_dir() && second.is_dir(), "探测目录需可创建");

        let mut child = crate::utils::console::hidden_command("cmd.exe")
            .current_dir(&first)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("启动 cmd.exe");
        let pid = child.id();

        let norm = |p: &std::path::Path| {
            p.to_string_lossy()
                .to_lowercase()
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_string()
        };
        let before = query_process_working_directory(pid).expect("读到初始 cwd");
        assert_eq!(norm(&before), norm(&first), "初始 cwd 应为启动目录");

        // `cd /d` 换盘符也要能换（同一盘用 cd 即可）。
        {
            use std::io::Write as _;
            let stdin = child.stdin.as_mut().expect("stdin 可写");
            stdin
                .write_all(format!("cd /d \"{}\"\r\n", second.display()).as_bytes())
                .expect("写入 cd");
            stdin.flush().ok();
        }

        // 给 shell 一点时间执行；`cd` 是内建命令，不起子进程。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut after = None;
        while std::time::Instant::now() < deadline {
            if let Some(cwd) = query_process_working_directory(pid)
                && norm(&cwd) == norm(&second)
            {
                after = Some(cwd);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);

        assert_eq!(
            after.as_deref().map(norm),
            Some(norm(&second)),
            "PEB 的 CurrentDirectory 必须跟随 cd 更新，否则本模块读到的是启动目录"
        );
    }

    /// 进程已退出 / pid 不存在时必须返回 `None` 而不是崩或野指针。
    #[test]
    fn returns_none_for_a_dead_process() {
        // 0 是系统空闲进程，权限受限；用一个刚退出的子进程更贴近真实。
        let mut child = crate::utils::console::hidden_command("cmd.exe")
            .args(["/c", "exit"])
            .spawn()
            .expect("启动 cmd.exe");
        let pid = child.id();
        let _ = child.wait();
        assert!(
            query_process_working_directory(pid).is_none(),
            "已退出的进程应返回 None"
        );
    }
}
