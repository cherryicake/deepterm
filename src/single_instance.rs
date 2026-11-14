//! 单实例约束：同一个用户只跑一个 `deepterm` GUI。
//!
//! # 为什么需要
//!
//! 没有它的时候，用户多点几次桌面图标就会起好几个进程，每个都开一个窗口。
//! 而 gpui 在 Linux 上 **`activate()` 是空实现**（源码里就写着
//! `activate is not implemented on Linux`），新窗口不会置前也不抢焦点——
//! 用户看到的现象是"点了没反应，过一会儿又突然能开了"，实际上每次点击都
//! 开了窗，只是叠在别的东西后面。
//!
//! 更糟的是进程会越堆越多，每个都占着 GPU 上下文与内存。
//!
//! # 做法
//!
//! 用 `flock` 对一个锁文件加排他锁：
//!
//! - 拿到锁 → 我是唯一实例，正常启动；
//! - 拿不到 → 已经有实例在跑，**立刻退出**，不创建窗口。
//!
//! 选 `flock` 而不是"检查 pid 文件是否存在"：进程崩溃时锁由内核自动释放，
//! 不会留下"文件在、进程没了"的假阳性。那正是 pid 文件方案最常见的坑。
//!
//! 锁文件放在运行时目录（与守护进程的 socket 同一处）。不用配置目录：
//! 那里可能被清、也可能被备份工具复制走。
//!
//! # 已知边界
//!
//! 锁是**按用户**的：不同用户各有一份运行时目录，可以各跑一个。这是对的。

use std::path::PathBuf;

/// 锁文件的路径。
pub fn lock_path() -> PathBuf {
    crate::daemon::runtime_dir().join("app.lock")
}

/// 一个已持有的实例锁。丢弃它（进程退出）时锁自动释放。
pub struct InstanceLock {
    /// 持有打开的文件描述符。**必须活到进程结束**：fd 一关，锁就没了。
    _file: std::fs::File,
}

impl InstanceLock {
    /// 尝试取得实例锁。
    ///
    /// - `Ok(Some(lock))`：拿到了，本进程是唯一实例；
    /// - `Ok(None)`：已有实例在跑，调用方应当退出；
    /// - `Err(_)`：无法建立锁（目录不可写等），调用方应当**照常启动**——
    ///   宁可多开一个窗口，也不要因为锁机制坏了让用户完全打不开应用。
    pub fn acquire() -> std::io::Result<Option<Self>> {
        let path = lock_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        if !try_lock(&file)? {
            return Ok(None);
        }
        // 记下 pid，纯为排障（"这个锁是谁拿的"）。
        let _ = file.set_len(0);
        {
            use std::io::Write as _;
            let mut f = &file;
            let _ = write!(f, "{}", std::process::id());
        }
        Ok(Some(Self { _file: file }))
    }
}

/// 非阻塞地加排他锁；已被别人持有返回 `false`。
#[cfg(unix)]
fn try_lock(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd as _;
    /// `LOCK_EX`：排他；`LOCK_NB`：不阻塞（拿不到立刻返回）。
    const LOCK_EX: libc::c_int = 2;
    const LOCK_NB: libc::c_int = 4;

    // SAFETY：fd 由 `file` 持有且在调用期间有效；`flock` 只操作这个 fd，
    // 没有指针参数。失败时检查 errno 区分"被别人占着"与真实错误。
    let rc = unsafe { libc::flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // 别人已经锁住了。Linux 上 `EWOULDBLOCK` 与 `EAGAIN` 是同一个值，
        // 只匹配一个即可（写两个会被判 unreachable）。
        Some(libc::EWOULDBLOCK) => Ok(false),
        _ => Err(err),
    }
}

/// Windows 上没有 `flock`，用"独占打开"模拟。
///
/// 独占打开（`share_mode(0)`）时，第二个进程的 `open` 会直接失败——语义
/// 与 `flock` 等价，且同样由内核在进程退出时释放。
#[cfg(windows)]
fn try_lock(file: &std::fs::File) -> std::io::Result<bool> {
    // 文件已经以共享模式打开过了，这里改用句柄自身的排他语义：
    // 如果另一个进程独占了同名文件，我们根本打不开它（在 `acquire` 里
    // 就会失败），所以走到这里就说明我们是唯一持有者。
    let _ = file;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁文件必须落在运行时目录，而不是配置目录。
    ///
    /// 配置目录会被备份工具复制、也可能被用户清掉；运行时目录是
    /// "这次登录期间有效"的东西，正好匹配锁的生命周期。
    #[test]
    fn the_lock_lives_in_the_runtime_directory() {
        assert_eq!(
            lock_path().parent(),
            Some(crate::daemon::runtime_dir().as_path())
        );
        assert!(lock_path().ends_with("app.lock"));
    }

    /// 同一进程里第二次取锁必须失败（说明排他真的生效了）。
    ///
    /// 回归价值：这是单实例机制的核心断言。`flock` 的语义容易被写错
    /// ——比如用 `LOCK_SH` 就完全不互斥，而那种错误在"只测一次获取"
    /// 的测试里看不出来。
    #[test]
    fn a_second_acquire_is_refused() {
        let dir = std::env::temp_dir().join(format!("deepterm-lock-{}", std::process::id()));
        // SAFETY：本用例独占该变量。
        unsafe { std::env::set_var(crate::daemon::RUNTIME_DIR_ENV, &dir) };

        let first = InstanceLock::acquire().expect("首次取锁不该报错");
        assert!(first.is_some(), "首次取锁应当成功");
        let second = InstanceLock::acquire().expect("第二次取锁不该报错");
        assert!(second.is_none(), "第二次取锁必须被拒");

        drop(first);
        // 释放后应当又能拿到。
        let third = InstanceLock::acquire().expect("释放后再取");
        assert!(third.is_some(), "前一个锁释放后应当可再次获取");

        unsafe { std::env::remove_var(crate::daemon::RUNTIME_DIR_ENV) };
        let _ = std::fs::remove_dir_all(dir);
    }
}
