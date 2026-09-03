use std::sync::OnceLock;
use tokio::runtime::Runtime;

/// 全局共享的 tokio runtime，用于 SSH 连接和 SFTP 会话。
///
/// 避免每个 SSH 连接/SFTP 会话都创建独立的 tokio runtime（每个 runtime 默认创建
/// CPU 核心数个 worker 线程），从而大幅减少线程数。
///
/// # 致命失败说明（中文）
/// 仅当系统资源耗尽（fd/内存不足）导致 `new_multi_thread` 与 `new_current_thread`
/// 均构建失败时才会 panic。此时进程已无法执行任何 SSH/SFTP 异步任务，
/// 无降级路径，故采用显式 panic 并携带双份错误上下文，便于排障。
/// 正常运行环境下此分支不可达，已有单元测试覆盖成功路径。
pub fn shared_ssh_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("deepterm-ssh")
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("Failed to create multi-thread SSH runtime: {e}, falling back to current_thread");
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap_or_else(|fallback_err| {
                        // Last resort: panic with context (no runtime = cannot SSH)
                        panic!("Failed to create SSH runtime (multi: {e}, current_thread: {fallback_err})")
                    })
            }
        }
    })
}
