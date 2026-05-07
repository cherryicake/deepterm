use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gpui::prelude::*;
use gpui::{Context, ClickEvent, Window, App, div, px};

use crate::file_browser::shared::format_size;
use crate::file_transfer::{FileEndpoint, TransferRequest};
use crate::theme::UiTheme;

/// Handler trait for transfer task actions.
pub trait TransferTaskHandler: 'static {
    fn cancel_transfer_task(&mut self, transfer_id: usize, cx: &mut Context<Self>)
    where
        Self: Sized;
    fn remove_transfer_task(&mut self, transfer_id: usize, cx: &mut Context<Self>)
    where
        Self: Sized;
}

#[derive(Clone, Debug)]
pub enum TransferTaskStatus {
    Running,
    Completed { files: usize, bytes: u64 },
    Failed(String),
    Cancelled,
}

#[derive(Clone)]
pub struct TransferTaskState {
    pub id: usize,
    pub label: String,
    pub(crate) status: TransferTaskStatus,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) bytes_transferred: Arc<AtomicU64>,
    pub total_bytes: u64,
}

pub fn render_transfer_overlay(
    tasks: &[TransferTaskState],
    theme: UiTheme,
    on_cancel: impl Fn(usize, &mut App) + 'static + Clone,
    on_remove: impl Fn(usize, &mut App) + 'static + Clone,
) -> Option<gpui::Div> {
    if tasks.is_empty() {
        return None;
    }

    let mut panel = div()
        .absolute()
        .right(px(18.0))
        .bottom(px(18.0))
        .w(px(360.0))
        .max_h(px(280.0))
        .overflow_hidden()
        .rounded(px(12.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.elevated_background)
        .shadow_md()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(div().text_sm().text_color(theme.text).child("Transfers"));

    for task in tasks.iter().rev().take(5) {
        let task_id = task.id;
        let transferred = task.bytes_transferred.load(Ordering::Relaxed);
        let status_label = transfer_status_label(&task.status, transferred, task.total_bytes);
        let is_running = matches!(task.status, TransferTaskStatus::Running);
        let on_cancel = on_cancel.clone();
        let on_remove = on_remove.clone();
        panel = panel.child(
            div()
                .w_full()
                .rounded(px(8.0))
                .bg(theme.panel_background)
                .border_1()
                .border_color(theme.border.opacity(0.5))
                .p(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text)
                                .child(task.label.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text_subtle)
                                .child(status_label),
                        ),
                )
                .when(is_running, |this| {
                    this.child(
                        div()
                            .id(format!("transfer-cancel-{}", task_id))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .bg(theme.tab_hover_background)
                            .text_xs()
                            .text_color(theme.text)
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click({
                                let on_cancel = on_cancel.clone();
                                move |_: &ClickEvent, _: &mut Window, app: &mut App| {
                                    on_cancel(task_id, app);
                                }
                            }),
                    )
                })
                .when(!is_running, |this| {
                    this.child(
                        div()
                            .id(format!("transfer-close-{}", task_id))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .bg(theme.tab_hover_background)
                            .text_xs()
                            .text_color(theme.text)
                            .cursor_pointer()
                            .child("×")
                            .on_click({
                                let on_remove = on_remove.clone();
                                move |_: &ClickEvent, _: &mut Window, app: &mut App| {
                                    on_remove(task_id, app);
                                }
                            }),
                    )
                }),
        );
    }

    Some(panel)
}

pub fn transfer_label(request: &TransferRequest) -> String {
    let source_label = match request.sources.as_slice() {
        [] => "No files".to_string(),
        [source] => endpoint_label(source),
        sources => format!("{} items", sources.len()),
    };
    format!(
        "{} → {}",
        source_label,
        endpoint_label(&request.destination)
    )
}

pub fn endpoint_label(endpoint: &FileEndpoint) -> String {
    match endpoint {
        FileEndpoint::Local { path } => path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        FileEndpoint::Remote { ssh_config, path } => {
            let name = path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or("/");
            format!("{}:{}", ssh_config.host_label(), name)
        }
    }
}

pub fn transfer_status_label(
    status: &TransferTaskStatus,
    bytes_transferred: u64,
    total_bytes: u64,
) -> String {
    match status {
        TransferTaskStatus::Running => {
            if total_bytes > 0 {
                let percent = (bytes_transferred * 100 / total_bytes).min(100);
                format!("{}%", percent)
            } else {
                format!("Running... {}", format_size(bytes_transferred))
            }
        }
        TransferTaskStatus::Completed { files, bytes } => {
            format!("Completed: {} files, {}", files, format_size(*bytes))
        }
        TransferTaskStatus::Failed(error) => format!("Failed: {}", error),
        TransferTaskStatus::Cancelled => "Cancelled".to_string(),
    }
}

pub fn quick_total_size(request: &TransferRequest) -> u64 {
    use std::fs;
    request
        .sources
        .iter()
        .map(|source| match source {
            FileEndpoint::Local { path } => fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0),
            FileEndpoint::Remote { ssh_config, path } => {
                crate::ssh::sftp_fs::metadata(ssh_config, path)
                    .ok()
                    .map(|m| m.size)
                    .unwrap_or(0)
            }
        })
        .sum()
}
