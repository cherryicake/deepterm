//! Workspace 的文件传输协调逻辑（impl 分文件）。
//!
//! 从 workspace.rs 拆出：传输启动（进度/取消/冲突解决三个 detached 任务）、
//! 完成收尾、任务列表操作。workspace.rs 保留 tab / 布局 / 事件分发。

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{AnyWindowHandle, Context};

use crate::file_manager::{
    ConflictPrompt, ConflictResolution, ConflictResolver, FileEndpoint, SharedConflictResolver,
    TRANSFER_CANCELLED_MSG, TransferManager, TransferOperation, TransferRequest, TransferSummary,
    TransferTaskHandler, TransferTaskState, TransferTaskStatus, quick_total_size,
    transfer_item_name, transfer_label,
};
use crate::workspace::workspace::{TaskStatusReporter, Workspace, WorkspaceEvent};

/// 一次传输会影响到的目录：目标目录，外加 Move 时源所在的目录。
/// 传输**结束**后据此刷新文件树（发起时刷新等于空转，远端没有 watcher 兜底）。
fn affected_dirs(request: &TransferRequest) -> Vec<FileEndpoint> {
    let mut dirs = vec![request.destination.clone()];
    if request.operation == TransferOperation::Move {
        for source in &request.sources {
            if let Some(parent) = source.parent_dir() {
                dirs.push(parent);
            }
        }
    }
    dirs
}

impl Workspace {
    pub(crate) fn start_transfer_request(
        &mut self,
        panel_id: String,
        request: TransferRequest,
        window_handle: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        log::info!(
            "Starting transfer from panel {}: {:?} -> {:?}",
            panel_id,
            request.sources,
            request.destination
        );

        if request.is_noop() {
            log::info!("Transfer is noop (source == destination), skipping");
            return;
        }

        // 受影响的目录要在 request 被移进后台任务之前算好。
        let dirs_for_finish = affected_dirs(&request);
        let transfer_id = self.next_transfer_id;
        self.next_transfer_id += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = cancelled.clone();
        // 供冲突对话框监听任务感知"取消"，避免取消后仍弹出/继续弹出冲突对话框。
        let conflict_cancelled = cancelled.clone();
        let weak = cx.weak_entity();
        let bytes_transferred = Arc::new(AtomicU64::new(0));
        let progress = bytes_transferred.clone();
        let is_done = Arc::new(AtomicBool::new(false));
        let is_done_for_task = is_done.clone();

        let status_message = Arc::new(Mutex::new(String::new()));
        let total_bytes_for_reporter = Arc::new(AtomicU64::new(0));
        let reporter = TaskStatusReporter {
            status_message: status_message.clone(),
            total_bytes: total_bytes_for_reporter.clone(),
        };

        self.transfer_tasks.push(TransferTaskState {
            id: transfer_id,
            label: transfer_label(&request),
            item_name: transfer_item_name(&request),
            status: TransferTaskStatus::Running,
            cancelled,
            bytes_transferred: progress.clone(),
            total_bytes: 0,
            started_at: std::time::Instant::now(),
            status_message,
        });
        cx.notify();

        let request_for_size = request.clone();
        let weak_for_size = weak.clone();
        cx.spawn(async move |_, cx| {
            let total_bytes = cx
                .background_executor()
                .spawn(async move { quick_total_size(&request_for_size) })
                .await;
            if let Some(entity) = weak_for_size.upgrade() {
                entity.update(cx, |this, cx| {
                    if let Some(task) = this.transfer_tasks.iter_mut().find(|t| t.id == transfer_id)
                    {
                        let transferred = task.bytes_transferred.load(Ordering::Relaxed);
                        if transferred <= total_bytes || task.total_bytes == 0 {
                            task.total_bytes = total_bytes;
                        }
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        // 创建冲突解决通道和共享状态
        let (conflict_tx, conflict_rx) = async_channel::unbounded::<ConflictPrompt>();
        let shared_resolver = Arc::new(SharedConflictResolver::new(conflict_tx));
        shared_resolver.reset();
        let resolver_ref: Arc<dyn ConflictResolver> = shared_resolver.clone();

        // UI 任务：监听冲突请求并弹出对话框。
        // 持有传输的 `cancelled` 句柄，确保用户点取消后不再弹出（或继续弹出）冲突对话框 ——
        // 否则后台传输已停止，detached 监听任务却仍会把排队的 prompt 渲染成原生对话框，
        // 表现为"点取消后过段时间又冒出文件已存在弹窗"。
        //
        // 采用 100ms 轮询而非 `recv().await` 阻塞：监听任务自身持有 `shared_resolver`
        // （内含 channel 发送端），若一直阻塞在 `recv()`，则传输正常结束后发送端不会全部
        // 释放，channel 不关闭，任务永久泄漏。轮询让它能在「取消」或「传输完成」时及时退出。
        let conflict_done = is_done.clone();
        cx.spawn(async move |_, cx| {
            loop {
                // 已取消：立即退出，不再处理任何 prompt。
                if conflict_cancelled.load(Ordering::Relaxed) {
                    break;
                }

                let prompt = match conflict_rx.try_recv() {
                    Ok(prompt) => prompt,
                    Err(async_channel::TryRecvError::Empty) => {
                        // 没有待处理冲突：传输已结束则退出（释放发送端，避免任务泄漏），
                        // 否则稍后再轮询。
                        if conflict_done.load(Ordering::Relaxed) {
                            break;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(100))
                            .await;
                        continue;
                    }
                    Err(async_channel::TryRecvError::Closed) => break,
                };

                // 取出 prompt 后再确认一次：取消后绝不弹窗，直接回 Stop 让后台尽快收敛。
                if conflict_cancelled.load(Ordering::Relaxed) {
                    let _ = prompt.response.try_send(ConflictResolution::Stop);
                    break;
                }

                // 检查是否已启用 "应用到全部"
                if shared_resolver.has_cached_resolution() {
                    if let Some(resolution) = shared_resolver.get_cached_resolution() {
                        let _ = prompt.response.try_send(resolution);
                        continue;
                    }
                }

                let answer = cx.update_window(window_handle, |_view, window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        "文件已存在",
                        Some(&format!(
                            "名为 \"{}\" 的文件已存在于该位置。\n是否替换为正在移动的文件？",
                            prompt.file_name
                        )),
                        &["停止", "跳过", "替换", "保留两者"],
                        cx,
                    )
                });

                let button_index = match answer {
                    Ok(receiver) => match receiver.await {
                        Ok(idx) => idx,
                        Err(_) => break,
                    },
                    Err(_) => break,
                };

                // 对话框还开着的时候用户可能点了取消：此时把决策强制为 Stop，
                // 并跳过后续的 "应用到全部" 二次对话框。
                if conflict_cancelled.load(Ordering::Relaxed) {
                    let _ = prompt.response.try_send(ConflictResolution::Stop);
                    break;
                }

                let resolution = match button_index {
                    0 => ConflictResolution::Stop,
                    1 => ConflictResolution::Skip,
                    2 => ConflictResolution::Overwrite,
                    3 => ConflictResolution::AutoRename,
                    _ => ConflictResolution::Skip,
                };

                if resolution == ConflictResolution::Stop {
                    let _ = prompt.response.try_send(resolution);
                    break;
                }

                // 询问是否应用到全部
                let apply_all_answer = cx.update_window(window_handle, |_view, window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Info,
                        "应用到全部",
                        Some("是否将此选择应用到所有后续冲突文件？"),
                        &["仅此文件", "应用到全部"],
                        cx,
                    )
                });

                if let Ok(receiver) = apply_all_answer {
                    if let Ok(1) = receiver.await {
                        // 用户选择 "应用到全部"
                        shared_resolver.set_cached_resolution(resolution.clone());
                    }
                }

                let _ = prompt.response.try_send(resolution);
            }
        })
        .detach();

        let task = cx.background_executor().spawn(async move {
            let result = smol::unblock(move || {
                TransferManager::execute_blocking(
                    request,
                    crate::file_manager::TransferRun {
                        cancelled: Some(task_cancelled),
                        progress: Some(progress),
                        resolver: Some(resolver_ref.as_ref()),
                        reporter: Some(&reporter),
                    },
                )
            })
            .await;
            is_done_for_task.store(true, Ordering::Relaxed);
            result
        });

        let weak2 = weak.clone();
        cx.spawn(async move |_, cx| {
            let result = task.await;
            if let Some(entity) = weak2.upgrade() {
                entity.update(cx, |this, cx| {
                    this.finish_transfer_task(transfer_id, result, dirs_for_finish, cx);
                });
            }
        })
        .detach();

        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                if is_done.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(entity) = weak.upgrade() {
                    let tb = total_bytes_for_reporter.load(Ordering::Relaxed);
                    entity.update(cx, |this, cx| {
                        if let Some(task) =
                            this.transfer_tasks.iter_mut().find(|t| t.id == transfer_id)
                        {
                            if tb > 0 && task.total_bytes == 0 {
                                task.total_bytes = tb;
                            }
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
    }

    fn finish_transfer_task(
        &mut self,
        transfer_id: usize,
        result: anyhow::Result<TransferSummary>,
        dirs: Vec<FileEndpoint>,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self
            .transfer_tasks
            .iter_mut()
            .find(|task| task.id == transfer_id)
        else {
            return;
        };

        match result {
            Ok(summary) if summary.files_transferred == 0 && summary.bytes_transferred == 0 => {
                // 空操作（如拖拽到同一文件夹），标记为已跳过
                task.status = TransferTaskStatus::Skipped;
            }
            Ok(summary) => {
                log::info!(
                    "Transfer finished: {} files, {} bytes, {} skipped",
                    summary.files_transferred,
                    summary.bytes_transferred,
                    summary.skipped
                );
                // 完成时把 total 对齐到实际传输字节，保证进度条精确显示 100%
                // （quick_total_size 只是预估，可能偏大导致进度条停在 <100%）。
                task.total_bytes = summary.bytes_transferred;
                task.bytes_transferred
                    .store(summary.bytes_transferred, Ordering::Relaxed);
                task.status = TransferTaskStatus::Completed {
                    files: summary.files_transferred,
                    bytes: summary.bytes_transferred,
                };
                if summary.skipped > 0 {
                    log::warn!(
                        "传输完成但有 {} 个条目被跳过（冲突跳过 / 不支持的类型）",
                        summary.skipped
                    );
                }
            }
            Err(error) if error.to_string().contains(TRANSFER_CANCELLED_MSG) => {
                log::info!("Transfer cancelled");
                task.status = TransferTaskStatus::Cancelled;
            }
            Err(error) => {
                log::warn!("Transfer failed: {:#}", error);
                task.status = TransferTaskStatus::Failed(error.to_string());
            }
        }
        cx.notify();

        // 传输结束后才通知视图刷新：取消 / 失败也可能已经拷进了一部分文件，
        // 因此所有终态都刷新（刷新本身走防抖 + loader 去重，代价很低）。
        cx.emit(WorkspaceEvent::TransferFinished { dirs });

        // 已完成/失败/取消的任务保留在列表中，仅通过用户点击"清除"或"×"手动移除。
        let _ = transfer_id;
    }
}

impl TransferTaskHandler for Workspace {
    fn cancel_transfer_task(&mut self, transfer_id: usize, cx: &mut Context<Self>) {
        if let Some(task) = self
            .transfer_tasks
            .iter_mut()
            .find(|task| task.id == transfer_id)
        {
            task.cancelled.store(true, Ordering::Relaxed);
            task.status = TransferTaskStatus::Cancelled;
            cx.notify();
        }
    }

    fn remove_transfer_task(&mut self, transfer_id: usize, cx: &mut Context<Self>) {
        let initial_len = self.transfer_tasks.len();
        self.transfer_tasks.retain(|task| task.id != transfer_id);
        if self.transfer_tasks.len() != initial_len {
            cx.notify();
        }
    }
}

impl Workspace {
    /// 读取当前所有传输任务（只读视图）。
    pub(crate) fn transfer_tasks(&self) -> &[TransferTaskState] {
        &self.transfer_tasks
    }

    /// 清除所有已完成 / 失败 / 已取消的传输任务，保留运行中的条目。
    pub(crate) fn clear_finished_transfer_tasks(&mut self, cx: &mut Context<Self>) {
        let initial_len = self.transfer_tasks.len();
        self.transfer_tasks
            .retain(|task| matches!(task.status, TransferTaskStatus::Running));
        if self.transfer_tasks.len() != initial_len {
            cx.notify();
        }
    }
}


