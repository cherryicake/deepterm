//! 传输任务铃铛面板。
//!
//! 从 [`TitleBar`](crate::components::title_bar::TitleBar) 剥离出的业务组件：
//! TitleBar 只保留通用布局（TabBar、拖拽区、trailing 插槽），
//! 传输任务的触发按钮、popover、workspace 订阅与进度刷新定时器
//! 全部由本组件持有，通过 TitleBar 的 trailing 插槽挂载。

use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    AnimationExt as _, AnyElement, Context, ElementId, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, WeakEntity, Window, div, px,
};

use crate::components::title_bar::TITLE_BAR_HEIGHT;
use crate::file_manager::{
    TransferHoverAction, TransferTaskHandler, TransferTaskStatus, render_transfer_panel,
};
use crate::theme::ActiveTheme;
use crate::workspace::workspace::Workspace;

/// 通知面板进入动画时长（毫秒）
const BELL_PANEL_ENTER_MS: u64 = 280;
/// 通知面板退出动画时长（毫秒）
const BELL_PANEL_EXIT_MS: u64 = 200;
/// 进入 / 退出动画的垂直位移（从按钮下方向下滑入），像素。
const BELL_PANEL_SLIDE_PX: f32 = 8.0;
/// 铃铛按钮与标题栏右缘的间距（像素）。
const BELL_RIGHT_MARGIN: f32 = 14.0;
/// 铃铛按钮内整体进度条与按钮左缘的间距（像素）。
const BELL_PROGRESS_LEFT: f32 = 4.0;

/// 传输任务铃铛：触发按钮 + 传输列表面板（popover）。
pub struct TransferBellPanel {
    /// Workspace 弱引用：读取任务并触发取消 / 删除回调。
    workspace: Option<WeakEntity<Workspace>>,
    /// 面板（popover）是否展开
    panel_open: bool,
    /// popover 正在播放关闭动画：保持渲染直到动画结束。
    panel_animating_out: bool,
    /// popover 因“外部 mouse_down_out”关闭的时间点。GPUI 里 mouse_down_out 早于
    /// 铃铛按钮的 on_mouse_down 触发，因此点击铃铛时 popover 会先关闭，随后
    /// 铃铛处理器看到 `panel_open == false` 便误打开。用 ~300ms 窗口抑制。
    panel_dismissed_at: Option<Instant>,
    /// 每次“打开/关闭”切换递增，作为动画元素 id 的一部分，
    /// 确保动画从 delta=0 开始重新播放。
    anim_epoch: u32,
    /// 已经处理过的最大任务 id，用于检测“新任务”并自动展开弹窗。
    last_seen_task_id: Option<usize>,
    /// 当前悬停的按钮（任务 id + 动作），用于把状态副文本临时替换为功能说明
    hover: Option<(usize, TransferHoverAction)>,
    /// workspace observer / 定时器 subscription，随面板释放。
    _subscriptions: Vec<gpui::Subscription>,
    /// 定时刷新任务句柄
    _progress_refresh_task: Option<gpui::Task<()>>,
    /// 关闭动画结束后清除 `panel_animating_out` 的延迟任务。
    _hide_task: Option<gpui::Task<()>>,
}

impl Default for TransferBellPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferBellPanel {
    pub fn new() -> Self {
        Self {
            workspace: None,
            panel_open: false,
            panel_animating_out: false,
            panel_dismissed_at: None,
            anim_epoch: 0,
            last_seen_task_id: None,
            hover: None,
            _subscriptions: Vec::new(),
            _progress_refresh_task: None,
            _hide_task: None,
        }
    }

    /// 绑定 workspace 弱引用并启动订阅 + 定时刷新，以便实时反馈传输进度。
    /// 切换工作区时重复调用即可（旧订阅会被清空）。
    pub fn bind_workspace(&mut self, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) {
        self._subscriptions.clear();
        self.workspace = None;
        self.last_seen_task_id = None;
        if let Some(strong) = workspace.upgrade() {
            // 初始化“已见任务 id”基线，避免已有任务误触发自动展开。
            self.last_seen_task_id = strong.read(cx).transfer_tasks().iter().map(|t| t.id).max();
            self._subscriptions
                .push(cx.observe(&strong, |this, ws, cx| {
                    let tasks = ws.read(cx).transfer_tasks();
                    let max_id = tasks.iter().map(|t| t.id).max();
                    if let Some(new_id) = max_id {
                        let should_open = match this.last_seen_task_id {
                            Some(prev) => new_id > prev,
                            None => true,
                        };
                        if should_open && !this.panel_open {
                            this.open_panel(cx);
                        }
                    } else if this.panel_open || this.panel_animating_out {
                        // 任务列表清空后立即关闭 popover（而不是在 render 中修改状态）。
                        this.close_panel(cx);
                    }
                    this.last_seen_task_id = max_id;
                }));
        }
        self.workspace = Some(workspace);
        self.start_progress_refresh(cx);
    }

    fn start_progress_refresh(&mut self, cx: &mut Context<Self>) {
        if self._progress_refresh_task.is_some() {
            return;
        }
        // 仅在确实有需要刷新的对象时才运行：存在运行中的任务，或者 popover 可见。
        // 一旦没有活儿干就退出，observer 会在新任务到来时重新调用本函数。
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let Some(this) = this.upgrade() else {
                    break;
                };
                let keep = this.update(cx, |this, cx| {
                    let Some(ws) = this.workspace.as_ref().and_then(|weak| weak.upgrade()) else {
                        return false;
                    };
                    let has_running = ws
                        .read(cx)
                        .transfer_tasks()
                        .iter()
                        .any(|t| matches!(t.status, TransferTaskStatus::Running));
                    let popover_visible = this.panel_open || this.panel_animating_out;
                    if !has_running && !popover_visible {
                        // 没有运行中任务也没打开 popover，空转没意义；退出循环。
                        this._progress_refresh_task = None;
                        return false;
                    }
                    cx.notify();
                    true
                });
                if !keep {
                    break;
                }
            }
        });
        self._progress_refresh_task = Some(task);
    }

    fn open_panel(&mut self, cx: &mut Context<Self>) {
        if self.panel_open {
            return;
        }
        // 取消尚未完成的退出动画，避免内容被提前卸载。
        self.panel_animating_out = false;
        self._hide_task = None;
        self.panel_dismissed_at = None;
        self.panel_open = true;
        self.anim_epoch = self.anim_epoch.wrapping_add(1);
        cx.notify();
    }

    fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        // 点击铃铛时若 popover 刚被 on_mouse_down_out 关掉（几十毫秒内），
        // 说明这是同一次鼠标下压事件：此时不要再打开，让“点击铃铛关闭”
        // 生效，而不是“关→开”抖动。
        if !self.panel_open
            && let Some(dismissed_at) = self.panel_dismissed_at
                && dismissed_at.elapsed() < Duration::from_millis(300)
            {
                self.panel_dismissed_at = None;
                return;
            }
        if self.panel_open {
            self.close_panel(cx);
        } else {
            self.open_panel(cx);
        }
    }

    fn close_panel(&mut self, cx: &mut Context<Self>) {
        if !self.panel_open {
            return;
        }
        self.panel_open = false;
        // 进入退出动画阶段：保持 popover 渲染，但以“closing”方向绘制。
        self.panel_animating_out = true;
        self.anim_epoch = self.anim_epoch.wrapping_add(1);
        cx.notify();
        // 退出动画结束后卸载 popover。略多留几毫秒以容忍帧调度抖动。
        let entity = cx.entity();
        self._hide_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(BELL_PANEL_EXIT_MS + 40))
                .await;
            entity.update(cx, |this, cx| {
                this.panel_animating_out = false;
                cx.notify();
            });
        }));
    }

    fn render_trigger(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let workspace = self.workspace.as_ref()?.upgrade()?;
        let ws = workspace.read(cx);
        if ws.transfer_tasks().is_empty() {
            // popover 的关闭改由 workspace observer 处理，这里只是不渲染铃铛。
            return None;
        }
        // 聚合所有运行中任务：用于按钮下方的整体进度条
        let (running_count, aggregate_total, aggregate_done) =
            ws.transfer_tasks()
                .iter()
                .fold((0u32, 0u64, 0u64), |(count, total, done), task| {
                    if matches!(task.status, TransferTaskStatus::Running) {
                        (
                            count + 1,
                            total.saturating_add(task.total_bytes),
                            done.saturating_add(
                                task.bytes_transferred
                                    .load(std::sync::atomic::Ordering::Relaxed),
                            ),
                        )
                    } else {
                        (count, total, done)
                    }
                });
        let has_running = running_count > 0;
        let overall_ratio = if has_running && aggregate_total > 0 {
            (aggregate_done as f64 / aggregate_total as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let theme = cx.theme().ui;

        let button = div()
            .id("bell-trigger")
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .size(px(32.0))
            .mr(px(BELL_RIGHT_MARGIN))
            .rounded(px(999.0))
            .text_color(theme.text)
            .hover(|style| style.bg(theme.title_bar_hover))
            .cursor_pointer()
            // 在 mouse_down 阶段就完成 toggle，并 stop_propagation：这样打开状态下
            // 点击铃铛只触发本处理 (close)，不会让 popover 的 on_mouse_down_out 再
            // 次生效导致“close→open”的视觉抖动。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_panel(cx);
                }),
            )
            .child(
                gpui::svg()
                    .size(px(16.0))
                    .text_color(theme.text)
                    .path(crate::assets::Icon::Bell.path()),
            )
            // 按钮底部的整体进度条：仅在有运行中任务时显示
            .when(has_running, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(1.0))
                        .left(px(BELL_PROGRESS_LEFT))
                        .w(px(20.0))
                        .h(px(3.0))
                        .rounded_md()
                        .bg(theme.border_strong)
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(overall_ratio))
                                .rounded_md()
                                // 整体进度条颜色与主题强调色保持一致。
                                .bg(theme.accent),
                        ),
                )
            });

        let popover = if self.panel_open || self.panel_animating_out {
            self.render_popover(&workspace, cx)
        } else {
            None
        };

        Some(
            div()
                .relative()
                .flex()
                .items_center()
                .flex_shrink_0()
                .child(button)
                .when_some(popover, |this, popover| this.child(popover)),
        )
    }

    fn render_popover(
        &self,
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = cx.theme().ui;
        let tasks = workspace.read(cx).transfer_tasks().to_vec();

        let weak_cancel = workspace.downgrade();
        let weak_remove = workspace.downgrade();
        let weak_clear = workspace.downgrade();
        let weak_self_hover = cx.entity().downgrade();

        let panel = render_transfer_panel(
            &tasks,
            theme,
            self.hover,
            move |hover, app| {
                if let Some(this) = weak_self_hover.upgrade() {
                    this.update(app, |this, cx| {
                        if this.hover != hover {
                            this.hover = hover;
                            cx.notify();
                        }
                    });
                }
            },
            move |task_id, app| {
                if let Some(ws) = weak_cancel.upgrade() {
                    ws.update(app, |this, cx| this.cancel_transfer_task(task_id, cx));
                }
            },
            move |task_id, app| {
                if let Some(ws) = weak_remove.upgrade() {
                    ws.update(app, |this, cx| this.remove_transfer_task(task_id, cx));
                }
            },
            move |app| {
                if let Some(ws) = weak_clear.upgrade() {
                    ws.update(app, |this, cx| this.clear_finished_transfer_tasks(cx));
                }
            },
            |_task_id, _app| {
                // 暂未接入真实定位逻辑；保留按钮以匹配截图设计。
            },
        )?;

        // 为了模仿 macOS 系统通知的“轻柔滑入、收紧滑出”手感：
        // - 进入：从按钮正下方略微上方位置（top-8px）以 0 不透明度开始，
        //   以带轻微回弹的 ease-out 曲线滑入并淡入。
        // - 退出：沿相反方向略微向上滑动并淡出，采用较快的 ease-in。
        let is_exit = !self.panel_open && self.panel_animating_out;
        let duration_ms = if is_exit {
            BELL_PANEL_EXIT_MS
        } else {
            BELL_PANEL_ENTER_MS
        };
        let epoch = self.anim_epoch;
        let animation_id: ElementId = if is_exit {
            ("bell-popover-exit", epoch).into()
        } else {
            ("bell-popover-enter", epoch).into()
        };

        let animated_panel = div()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                // 标记这是“外部点击”触发的关闭；若这次下压的目标其实是铃铛本身，
                // 紧接着的 toggle_panel 会凭这个时间戳跳过重新打开。
                this.panel_dismissed_at = Some(Instant::now());
                this.close_panel(cx);
            }))
            .child(panel)
            .with_animation(
                animation_id,
                gpui::Animation::new(Duration::from_millis(duration_ms)).with_easing(
                    move |t: f32| {
                        if is_exit {
                            bell_exit_easing(t)
                        } else {
                            bell_enter_easing(t)
                        }
                    },
                ),
                move |this, delta| {
                    let (offset_y, opacity) = if is_exit {
                        (-BELL_PANEL_SLIDE_PX * delta, 1.0 - delta)
                    } else {
                        (-BELL_PANEL_SLIDE_PX * (1.0 - delta), delta)
                    };
                    this.mt(px(offset_y)).opacity(opacity.clamp(0.0, 1.0))
                },
            );

        // 用 `deferred` 把绘制推迟到所有祖先之后，确保 popover 画在
        // workspace 区域之上（仅布局保留在当前 title bar 的局部坐标里）。
        let popover = gpui::deferred(
            div()
                .absolute()
                // 面板出现在标题栏正下方（TITLE_BAR_HEIGHT），右缘对齐铃铛按钮。
                .top(TITLE_BAR_HEIGHT)
                .right(px(BELL_RIGHT_MARGIN))
                .child(animated_panel),
        )
        .with_priority(1);
        Some(popover.into_any_element())
    }
}

/// 进入动画缓动：ease-out-quint，模拟 macOS 通知由上方快速下落、
/// 末段优雅减速的感觉。保留返回值严格在 [0, 1] 内以满足 GPUI 约束。
fn bell_enter_easing(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(5)
}

/// 退出动画缓动：ease-in-cubic，开始较慢而后快速消失，
/// 贴合 macOS 通知上移收拢的“快收”节奏。
fn bell_exit_easing(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

impl Render for TransferBellPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 没有传输任务时渲染零尺寸占位，不占用标题栏布局空间。
        match self.render_trigger(cx) {
            Some(trigger) => trigger.into_any_element(),
            None => div().into_any_element(),
        }
    }
}
