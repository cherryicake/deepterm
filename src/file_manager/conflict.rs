use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use super::TRANSFER_CANCELLED_MSG;

/// 用户针对单个文件冲突做出的决策。
#[derive(Clone, Debug, PartialEq)]
pub enum ConflictResolution {
    /// 跳过当前文件
    Skip,
    /// 覆盖目标文件
    Overwrite,
    /// 自动重命名（保留两者）
    AutoRename,
    /// 停止整个传输任务
    Stop,
}

/// 发送给 UI 层的冲突提示请求。
pub struct ConflictPrompt {
    /// 冲突文件名
    pub file_name: String,
    /// 目标路径
    pub destination_path: String,
    /// 用于向后台线程回传用户决策的通道
    pub response: async_channel::Sender<ConflictResolution>,
}

/// 冲突解决器 trait。
/// 由后台线程调用，将冲突提示发送到 UI 层并等待用户决策。
pub trait ConflictResolver: Send + Sync {
    /// 发送冲突提示。实现方应通过 `prompt.response` 通道回传决策。
    fn prompt(&self, prompt: ConflictPrompt);
}

/// 共享的冲突解决状态，支持 "应用到全部" 功能。
pub struct SharedConflictResolver {
    sender: async_channel::Sender<ConflictPrompt>,
    apply_to_all: AtomicBool,
    cached_resolution: Mutex<Option<ConflictResolution>>,
}

impl SharedConflictResolver {
    pub fn new(sender: async_channel::Sender<ConflictPrompt>) -> Self {
        Self {
            sender,
            apply_to_all: AtomicBool::new(false),
            cached_resolution: Mutex::new(None),
        }
    }

    /// 重置 "应用到全部" 状态，用于新的传输任务。
    pub fn reset(&self) {
        self.apply_to_all.store(false, Ordering::Relaxed);
        *self.cached_resolution.lock() = None;
    }

    /// 检查是否已缓存了 "应用到全部" 的决策。
    pub fn has_cached_resolution(&self) -> bool {
        self.cached_resolution.lock().is_some()
    }

    /// 获取缓存的决策（如果有）。
    pub fn get_cached_resolution(&self) -> Option<ConflictResolution> {
        self.cached_resolution.lock().clone()
    }

    /// 设置 "应用到全部" 决策。
    pub fn set_cached_resolution(&self, resolution: ConflictResolution) {
        *self.cached_resolution.lock() = Some(resolution);
    }
}

impl ConflictResolver for SharedConflictResolver {
    fn prompt(&self, prompt: ConflictPrompt) {
        // 如果用户已选择 "应用到全部"，直接返回缓存的决策
        if self.apply_to_all.load(Ordering::Relaxed) {
            if let Some(resolution) = self.cached_resolution.lock().as_ref() {
                let _ = prompt.response.try_send(resolution.clone());
                return;
            }
        }

        // 否则将请求发送到 UI 层
        let _ = self.sender.try_send(prompt);
    }
}

/// 阻塞等待用户决策。
/// 在后台线程中调用，通过通道同步等待 UI 层的响应。
/// 每 100ms 检查一次 `cancelled`，为 true 时立即返回取消错误。
pub fn wait_for_resolution(
    rx: &async_channel::Receiver<ConflictResolution>,
    cancelled: &AtomicBool,
) -> anyhow::Result<ConflictResolution> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!(TRANSFER_CANCELLED_MSG));
        }
        match rx.try_recv() {
            Ok(resolution) => return Ok(resolution),
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(async_channel::TryRecvError::Closed) => {
                return Err(anyhow::anyhow!("conflict resolver closed"));
            }
        }
    }
}

/// 设置 "应用到全部" 决策。
pub fn set_apply_to_all(resolver: &SharedConflictResolver, resolution: ConflictResolution) {
    resolver.apply_to_all.store(true, Ordering::Relaxed);
    *resolver.cached_resolution.lock() = Some(resolution);
}

/// 检查是否已启用 "应用到全部"。
pub fn is_apply_to_all(resolver: &SharedConflictResolver) -> bool {
    resolver.apply_to_all.load(Ordering::Relaxed)
}
