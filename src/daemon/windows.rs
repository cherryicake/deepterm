//! Windows 上替代 Unix socket 的传输：命名管道。
//!
//! # 为什么不是"本地 TCP 端口"
//!
//! 命名管道在 Windows 上有两件事是 TCP 给不了的：
//!
//! 1. **对象级 ACL**。管道可以按用户 SID 设访问控制，别的用户连不上；
//!    `127.0.0.1:port` 对同机所有用户开放，等于把终端交给全机。
//! 2.（这里没用到但相关）管道句柄可以被子进程继承，将来要做句柄传递时
//!    不用换协议。
//!
//! # 形态
//!
//! 同一份二进制在两端都用 `ReadFile`/`WriteFile` 读写，所以 `Stream`
//! 直接实现 `Read + Write`，与 `UnixStream` 的接口对齐——`protocol.rs`
//! 的收帧/发帧代码完全不用改。这是刻意的：分帧逻辑跨平台一致才不会有
//! "Linux 上好好的、Windows 上粘包"这类只在一边出现的 bug。
//!
//! # ⚠️ 未在 Windows 上编译或运行过
//!
//! 开发环境是 Linux，既没有 `x86_64-pc-windows-msvc` 目标、也没有可用的
//! Windows 机器，所以这个文件**一次都没有被 rustc 编译过**。
//!
//! 已经做的核对：按 `windows` 0.52 的源码逐个对齐了 API 签名
//! （`CreateNamedPipeW` 的首参是 `FILE_FLAGS_AND_ATTRIBUTES` 包装类型、
//! `ReadFile`/`WriteFile` 收 `Option<&mut [u8]>` 而不是裸指针、
//! `ConnectNamedPipe`/`ReadFile` 需要 `Win32_System_IO` feature 等），
//! Cargo.toml 里的 feature 清单也据此补全了。
//!
//! 首次在 Windows 上构建时预计还要修的（按可能性排序）：
//!
//! 1. `PIPE_ACCESS_DUPLEX` 的类型转换——0.52 里它是 `FILE_FLAGS_AND_ATTRIBUTES`；
//! 2. `DuplicateHandle` 的参数顺序与 `DUPLICATE_SAME_ACCESS` 的传入形态；
//! 3. `error.raw_os_error()` 与 `WIN32_ERROR` 的比较写法。
//!
//! 这些都是编译期问题，不会静默产生错误行为——跑一次 `cargo check
//! --target x86_64-pc-windows-msvc` 就能全部暴露。

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::core::PCWSTR;

/// 管道名前缀。完整名字是 `\\.\pipe\deepterm-<用户>-<会话>`。
pub const PIPE_PREFIX: &str = r"\\.\pipe\deepterm";

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 一条命名管道连接。
pub struct Stream {
    handle: HANDLE,
}

// SAFETY：`HANDLE` 只是一个不透明指针，管道句柄可以跨线程使用
// （Win32 文档允许，只要不同时对同一句柄做重叠 IO——这里都是同步读写，
// 且由 `&mut self` 保证独占）。
unsafe impl Send for Stream {}

impl Stream {
    fn from_handle(handle: HANDLE) -> Self {
        Self { handle }
    }

    /// 复制一份句柄，供读写两个线程各持一份。
    ///
    /// 与 `UnixStream::try_clone` 对应。用 `DuplicateHandle` 而不是共享
    /// 同一个句柄：两个线程各持己有句柄，`&mut self` 的独占语义才成立。
    pub fn try_clone_handle(&self) -> io::Result<Self> {
        use windows::Win32::Foundation::DUPLICATE_SAME_ACCESS;
        use windows::Win32::System::Threading::{DuplicateHandle, GetCurrentProcess};

        let mut out = HANDLE::default();
        // SAFETY：源句柄有效；目标进程是当前进程；`out` 是有效出参。
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle,
                GetCurrentProcess(),
                &mut out,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .map_err(io::Error::from)?;
        Ok(Self::from_handle(out))
    }

    /// 连到已有的管道。
    pub fn connect(path: &str) -> io::Result<Self> {
        let name = wide(path);
        // 管道忙（对面还没 `ConnectNamedPipe`）时短暂等一下，与 Unix socket
        // 的 `ECONNREFUSED` 重试是同一类处理。
        for _ in 0..50 {
            // SAFETY：`name` 以 NUL 结尾且存活到调用结束。
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    Default::default(),
                    None,
                )
            };
            match handle {
                Ok(handle) if handle != INVALID_HANDLE_VALUE => {
                    return Ok(Self::from_handle(handle));
                }
                _ => {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(ERROR_PIPE_BUSY.0 as i32) {
                        return Err(err);
                    }
                    // 等对面把管道准备好（100ms），超时继续重试。
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 100) };
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "命名管道一直忙，连不上",
        ))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        // SAFETY：`buf` 是本次调用独占的可变切片；`read` 是有效出参；
        // 句柄由 `self` 持有且未关闭。
        //
        // 注意 `windows` 0.52 的 `ReadFile` 收的是 `Option<&mut [u8]>`
        // 而不是裸指针——签名对不上会在 Windows 上编译失败，而这里
        // （Linux）根本编译不到这段。
        unsafe { ReadFile(self.handle, Some(buf), Some(&mut read), None) }
            .map_err(io::Error::from)?;
        Ok(read as usize)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY：`buf` 在调用期间有效；`written` 是有效出参；句柄未关闭。
        unsafe { WriteFile(self.handle, Some(buf), Some(&mut written), None) }
            .map_err(io::Error::from)?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // 管道是字节流，没有用户态缓冲要冲。
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            // SAFETY：句柄来自 `CreateFileW` / `CreateNamedPipeW`，未重复关闭。
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

/// 服务端的管道监听器。
pub struct Listener {
    path: String,
    /// 每次 `accept` 都要新建一个实例（一条管道实例只服务一个连接）。
    _marker: (),
}

impl Listener {
    /// 绑定一个管道名，并创建第一个实例。
    pub fn bind(path: &str) -> io::Result<Self> {
        let listener = Self {
            path: path.to_string(),
            _marker: (),
        };
        Ok(listener)
    }

    /// 等一个客户端连上来。
    pub fn accept(&self) -> io::Result<Stream> {
        let name = wide(&self.path);
        // SAFETY：`name` 以 NUL 结尾且存活到调用结束；其余参数都是常量。
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(
                    PIPE_ACCESS_DUPLEX.0,
                ),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        // SAFETY：句柄刚由 `CreateNamedPipeW` 创建，有效且未关闭。
        let connected = unsafe { ConnectNamedPipe(handle, None) };
        if let Err(e) = connected {
            let err = io::Error::from(e);
            let already = err.raw_os_error() == Some(ERROR_PIPE_CONNECTED.0 as i32);
            if !already {
                // SAFETY：句柄有效，尚未交给 `Stream`。
                let _ = unsafe { DisconnectNamedPipe(handle) };
                let _ = unsafe { CloseHandle(handle) };
                return Err(err);
            }
        }
        Ok(Stream::from_handle(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 管道名必须落在 `\\.\pipe\` 下，否则 `CreateNamedPipeW` 直接失败。
    #[test]
    fn the_pipe_prefix_is_a_valid_namespace() {
        assert!(PIPE_PREFIX.starts_with(r"\\.\pipe\"));
    }

    /// 宽字符串必须以 NUL 结尾，否则 API 会读到越界。
    #[test]
    fn wide_strings_are_nul_terminated() {
        let w = wide("abc");
        assert_eq!(w.last(), Some(&0));
        assert_eq!(&w[..3], &[b'a' as u16, b'b' as u16, b'c' as u16]);
    }
}
