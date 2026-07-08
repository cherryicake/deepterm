use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Instant;

use gpui::prelude::*;
use gpui::{App, ClickEvent, Context, Div, MouseButton, Window, div, px, relative, rgb};

use crate::assets::Icon;
fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", size, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}
use crate::file_manager::{FileEndpoint, TransferRequest};
use crate::theme::UiTheme;

const PANEL_WIDTH: f32 = 520.0;
const PANEL_MAX_HEIGHT: f32 = 440.0;

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
    Skipped,
}

#[derive(Clone)]
pub struct TransferTaskState {
    pub id: usize,
    pub label: String,
    /// Short name shown as the list item title (e.g. file name without path).
    pub item_name: String,
    pub(crate) status: TransferTaskStatus,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) bytes_transferred: Arc<AtomicU64>,
    pub total_bytes: u64,
    pub started_at: Instant,
    /// 实时状态消息（如 scp 执行过程中的详细状态）
    pub status_message: Arc<Mutex<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferHoverAction {
    Cancel,
    Remove,
    Reveal,
}

impl TransferHoverAction {
    fn hint_text(self) -> &'static str {
        match self {
            TransferHoverAction::Cancel => "取消传输",
            TransferHoverAction::Remove => "从列表中移除",
            TransferHoverAction::Reveal => "在文件夹中显示",
        }
    }
}

/// Render the full transfer popover panel, including the header with a
/// "clear all completed" action and the task list. Returns `None` when no
/// task exists so callers can decide whether to show the trigger button.
pub fn render_transfer_panel(
    tasks: &[TransferTaskState],
    theme: UiTheme,
    hover_hint: Option<(usize, TransferHoverAction)>,
    on_hover: impl Fn(Option<(usize, TransferHoverAction)>, &mut App) + 'static + Clone,
    on_cancel: impl Fn(usize, &mut App) + 'static + Clone,
    on_remove: impl Fn(usize, &mut App) + 'static + Clone,
    on_clear_finished: impl Fn(&mut App) + 'static + Clone,
    on_reveal: impl Fn(usize, &mut App) + 'static + Clone,
) -> Option<Div> {
    if tasks.is_empty() {
        return None;
    }

    let has_finished = tasks
        .iter()
        .any(|task| !matches!(task.status, TransferTaskStatus::Running));

    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .px(px(14.0))
        .pt(px(10.0))
        .pb(px(8.0))
        .child(
            div()
                .flex_1()
                .text_center()
                .text_sm()
                .text_color(theme.text)
                .child("传输"),
        )
        .child({
            let mut btn = div()
                .id("transfer-clear")
                .text_xs()
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(6.0))
                .text_color(if has_finished {
                    theme.text
                } else {
                    theme.text_subtle
                })
                .child("清除");
            if has_finished {
                btn = btn
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.tab_hover_background))
                    .on_click({
                        let on_clear_finished = on_clear_finished.clone();
                        move |_: &ClickEvent, _: &mut Window, app: &mut App| {
                            on_clear_finished(app);
                        }
                    });
            }
            btn
        });

    let mut list = div()
        .id("transfer-list")
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .max_h(px(PANEL_MAX_HEIGHT - 46.0));

    for (i, task) in tasks.iter().rev().enumerate() {
        let is_last = i + 1 == tasks.len();
        let task_hover = hover_hint
            .filter(|(id, _)| *id == task.id)
            .map(|(_, action)| action);
        list = list.child(render_transfer_list_item(
            task,
            theme,
            is_last,
            task_hover,
            on_hover.clone(),
            on_cancel.clone(),
            on_remove.clone(),
            on_reveal.clone(),
        ));
    }

    // 使用主题中定义的通知弹窗背景色，确保在任意主题下都能与终端背景做明显区分。
    Some(
        div()
            .w(px(PANEL_WIDTH))
            .max_h(px(PANEL_MAX_HEIGHT))
            .rounded(px(12.0))
            .bg(theme.notification_background)
            .shadow_lg()
            .flex()
            .flex_col()
            .child(header)
            .child(list),
    )
}

fn render_transfer_list_item(
    task: &TransferTaskState,
    theme: UiTheme,
    is_last: bool,
    hover: Option<TransferHoverAction>,
    on_hover: impl Fn(Option<(usize, TransferHoverAction)>, &mut App) + 'static + Clone,
    on_cancel: impl Fn(usize, &mut App) + 'static + Clone,
    on_remove: impl Fn(usize, &mut App) + 'static + Clone,
    on_reveal: impl Fn(usize, &mut App) + 'static + Clone,
) -> Div {
    let task_id = task.id;
    let transferred = task.bytes_transferred.load(Ordering::Relaxed);
    let is_running = matches!(task.status, TransferTaskStatus::Running);
    let total_bytes = task.total_bytes;

    let status_color = match &task.status {
        TransferTaskStatus::Failed(_) => theme.error,
        TransferTaskStatus::Cancelled => theme.warning,
        _ => theme.text_subtle,
    };

    let is_transferring = is_running && transferred > 0;

    // 进度条：运行中始终显示（确定或不确定）
    let is_completed = matches!(task.status, TransferTaskStatus::Completed { .. });
    let ratio = if is_completed {
        1.0
    } else if total_bytes > 0 {
        (transferred as f64 / total_bytes as f64).clamp(0.0, 0.98)
    } else {
        0.0
    };
    let progress_bar = div()
        .w_full()
        .h(px(4.0))
        .rounded(px(2.0))
        .bg(theme.border.opacity(0.5))
        .child(
            div()
                .h_full()
                .w(relative(ratio as f32))
                .rounded(px(2.0))
                .bg(rgb(0x3b82f6)),
        );

    // 单行状态文本：连接提示 / 网速 / 结果 在同一行位置切换
    let status_text = if let Some(action) = hover {
        action.hint_text().to_string()
    } else if is_running && !is_transferring {
        // 连接尝试阶段：显示策略提示（如 "直连(agent forwarding)..."）
        let msg = task.status_message.lock();
        if msg.is_empty() {
            "准备中...".to_string()
        } else {
            msg.clone()
        }
    } else {
        match &task.status {
            TransferTaskStatus::Running => {
                let elapsed = task.started_at.elapsed().as_secs_f64().max(0.001);
                let speed = (transferred as f64) / elapsed;
                let mut parts = Vec::with_capacity(3);
                if total_bytes > 0 {
                    parts.push(format!(
                        "{}/{}",
                        format_size(transferred),
                        format_size(total_bytes)
                    ));
                } else {
                    parts.push(format_size(transferred));
                }
                if speed > 0.0 {
                    parts.push(format!("({}/秒)", format_size(speed as u64)));
                }
                if total_bytes > transferred && speed > 1.0 {
                    let remaining_secs = ((total_bytes - transferred) as f64 / speed) as u64;
                    parts.push(format!(
                        "— 剩余 {}",
                        format_duration_remaining(remaining_secs)
                    ));
                }
                parts.join(" ")
            }
            TransferTaskStatus::Completed { bytes, .. } => {
                format!("传输完成 {}", format_size(*bytes))
            }
            TransferTaskStatus::Failed(error) => format!("失败: {}", trim_error(error)),
            TransferTaskStatus::Cancelled => "已取消".to_string(),
            TransferTaskStatus::Skipped => "已跳过".to_string(),
        }
    };

    let display_name = truncate_middle_preserving_suffix(&task.item_name, 56);

    let text_column = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .truncate()
                .text_xs()
                .text_color(theme.text)
                .child(display_name),
        )
        .child(progress_bar)
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .truncate()
                .text_xs()
                .text_color(status_color)
                .child(status_text),
        );

    let cancel_button = div()
        .id(("transfer-cancel", task_id))
        .size(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(theme.text_subtle)
        .hover(|style| style.text_color(theme.text).bg(theme.tab_hover_background))
        .cursor_pointer()
        .child(
            gpui::svg()
                .size(px(12.0))
                .text_color(theme.text_subtle)
                .path(Icon::WindowClose.path()),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_hover({
            let on_hover = on_hover.clone();
            move |hovered: &bool, _: &mut Window, app: &mut App| {
                on_hover(
                    if *hovered {
                        Some((task_id, TransferHoverAction::Cancel))
                    } else {
                        None
                    },
                    app,
                );
            }
        })
        .on_click({
            let on_cancel = on_cancel.clone();
            move |_: &ClickEvent, _: &mut Window, app: &mut App| {
                on_cancel(task_id, app);
            }
        });

    let reveal_button = div()
        .id(("transfer-reveal", task_id))
        .size(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(theme.text_subtle)
        .hover(|style| style.text_color(theme.text).bg(theme.tab_hover_background))
        .cursor_pointer()
        .child(
            gpui::svg()
                .size(px(14.0))
                .text_color(theme.text_subtle)
                .path(Icon::Reveal.path()),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_hover({
            let on_hover = on_hover.clone();
            move |hovered: &bool, _: &mut Window, app: &mut App| {
                on_hover(
                    if *hovered {
                        Some((task_id, TransferHoverAction::Reveal))
                    } else {
                        None
                    },
                    app,
                );
            }
        })
        .on_click({
            let on_reveal = on_reveal.clone();
            move |_: &ClickEvent, _: &mut Window, app: &mut App| {
                on_reveal(task_id, app);
            }
        });

    let remove_button = div()
        .id(("transfer-remove", task_id))
        .size(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(theme.text_subtle)
        .hover(|style| style.text_color(theme.text).bg(theme.tab_hover_background))
        .cursor_pointer()
        .child(
            gpui::svg()
                .size(px(12.0))
                .text_color(theme.text_subtle)
                .path(Icon::WindowClose.path()),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_hover({
            let on_hover = on_hover.clone();
            move |hovered: &bool, _: &mut Window, app: &mut App| {
                on_hover(
                    if *hovered {
                        Some((task_id, TransferHoverAction::Remove))
                    } else {
                        None
                    },
                    app,
                );
            }
        })
        .on_click({
            let on_remove = on_remove.clone();
            move |_: &ClickEvent, _: &mut Window, app: &mut App| {
                on_remove(task_id, app);
            }
        });

    let actions = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .when(is_running, |this| this.child(cancel_button))
        .when(!is_running, |this| this.child(remove_button))
        .child(reveal_button);

    let icon_column = div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .w(px(44.0))
        .h_full()
        .text_color(theme.text_subtle)
        .child(
            gpui::svg()
                .size(px(34.0))
                .text_color(theme.text_subtle)
                .path(Icon::File.path()),
        );

    let mut row = div()
        .relative()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(8.0))
        .child(icon_column)
        .child(text_column)
        .child(actions);

    if !is_last {
        // 分割线：起始位置与文件名左对齐，结束位置距弹窗右边界 4px
        row = row.child(
            div()
                .absolute()
                .bottom(px(0.0))
                .left(px(64.0))
                .right(px(12.0))
                .h(px(1.0))
                .bg(theme.border.opacity(0.35)),
        );
    }
    row
}

fn format_duration_remaining(secs: u64) -> String {
    if secs < 60 {
        format!("{} 秒", secs)
    } else if secs < 3600 {
        format!("{} 分", (secs + 30) / 60)
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600 + 30) / 60;
        if mins == 0 {
            format!("{} 小时", hours)
        } else {
            format!("{} 小时 {} 分", hours, mins)
        }
    }
}

fn trim_error(error: &str) -> String {
    const MAX: usize = 120;
    if error.chars().count() <= MAX {
        error.to_string()
    } else {
        let truncated: String = error.chars().take(MAX).collect();
        format!("{}…", truncated)
    }
}

/// 截断长文件名：优先在主文件名（扩展名前）部分做 "…" 省略，
/// 始终保留扩展名。若没有扩展名（或扩展名本身过长），退化为普通前缀省略。
fn truncate_middle_preserving_suffix(name: &str, max_chars: usize) -> String {
    let total = name.chars().count();
    if total <= max_chars {
        return name.to_string();
    }

    // 最后一个 '.' 视为扩展分隔（不含首字符的点文件情况）
    let dot_index = name
        .char_indices()
        .rev()
        .find(|(i, c)| *c == '.' && *i > 0)
        .map(|(i, _)| i);

    if let Some(dot) = dot_index {
        let (stem, ext_with_dot) = name.split_at(dot);
        let ext_chars = ext_with_dot.chars().count();
        // 若扩展名本身太长（如无点后缀、随机字符串），按字符总额一半上限兜底
        if ext_chars + 4 >= max_chars {
            let head: String = name.chars().take(max_chars.saturating_sub(1)).collect();
            return format!("{}…", head);
        }
        let keep_stem = max_chars.saturating_sub(ext_chars + 1); // 预留 "…"
        let stem_prefix: String = stem.chars().take(keep_stem).collect();
        format!("{}…{}", stem_prefix, ext_with_dot)
    } else {
        let head: String = name.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", head)
    }
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

/// Short name for the list-item title, shown prominently above the progress
/// bar. For a single source we use its file name; for multi-selection we
/// summarize as "N 项".
pub fn transfer_item_name(request: &TransferRequest) -> String {
    match request.sources.as_slice() {
        [] => "传输".to_string(),
        [source] => endpoint_label(source),
        sources => format!("{} 项", sources.len()),
    }
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

pub fn quick_total_size(request: &TransferRequest) -> u64 {
    request
        .sources
        .iter()
        .map(|source| match source {
            FileEndpoint::Local { path } => local_total_size(path),
            FileEndpoint::Remote { ssh_config, path } => remote_total_size(ssh_config, path),
        })
        .sum()
}

fn local_total_size(path: &std::path::Path) -> u64 {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if metadata.is_file() {
        metadata.len()
    } else if metadata.is_dir() {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                total += local_total_size(&entry.path());
            }
        }
        total
    } else {
        0
    }
}

fn remote_total_size(ssh_config: &crate::ssh::SshConnectionConfig, path: &str) -> u64 {
    // 创建独立连接，避免与正在传输的任务竞争 cached session
    let mut session = match crate::ssh::sftp_fs::BlockingSftpSession::connect(ssh_config) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let cmd = format!(
        "du -sb '{}' 2>/dev/null || echo 0",
        crate::file_manager::utils::shell_single_quote(path)
    );
    match session.execute_command(&cmd, 30) {
        Ok(result) if result.success() => result
            .stdout
            .trim()
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
        _ => 0,
    }
}
