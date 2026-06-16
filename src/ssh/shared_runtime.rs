use std::sync::OnceLock;
use tokio::runtime::Runtime;

/// 全局共享的 tokio runtime，用于 SSH 连接和 SFTP 会话。
///
/// 避免每个 SSH 连接/SFTP 会话都创建独立的 tokio runtime（每个 runtime 默认创建
/// CPU 核心数个 worker 线程），从而大幅减少线程数。
pub fn shared_ssh_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("deepterm-ssh")
            .build()
            .expect("Failed to create shared SSH runtime")
    })
}
