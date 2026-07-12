//! Title bar component for window management
//!
//! Supports Linux, Windows, and macOS with platform-specific window controls
//!
//! Layout: [TabBar][Draggable Area][Bell Button][Close Button]

use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    AnimationExt, App, Context, Decorations, ElementId, Entity, IntoElement, MouseButton,
    ParentElement, Pixels, Render, TitlebarOptions, WeakEntity, Window, WindowControlArea, div, px,
};

use crate::components::tab_bar::TabBar;
use crate::file_manager::{
    TransferHoverAction, TransferTaskHandler, TransferTaskStatus, render_transfer_panel,
};
use crate::theme::ActiveTheme;
use crate::workspace::workspace::Workspace;

pub const TITLE_BAR_HEIGHT: Pixels = px(44.);

#[cfg(target_os = "macos")]
const TITLE_BAR_LEFT_PADDING: Pixels = px(8.);

#[cfg(not(target_os = "macos"))]
const TITLE_BAR_LEFT_PADDING: Pixels = px(6.);

const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(400);

/// 通知面板进入动画时长（毫秒）
const BELL_PANEL_ENTER_MS: u64 = 280;
/// 通知面板退出动画时长（毫秒）
const BELL_PANEL_EXIT_MS: u64 = 200;
/// 进入 / 退出动画的垂直位移（从按钮下方向下滑入），像素。
const BELL_PANEL_SLIDE_PX: f32 = 8.0;

/// TitleBar used to customize the appearance of the title bar.
/// Layout: [TabBar][Draggable Area][Bell Button][Close Button]
pub struct TitleBar {
    id: ElementId,
    last_click_time: Option<Instant>,
    tab_bar: Option<Entity<TabBar>>,
    /// 用于跟踪是否应该移动窗口（macOS 手动拖拽实现）
    should_move: bool,
    /// Workspace 句柄，用于读取任务并触发取消 / 删除回调。
    workspace: Option<WeakEntity<Workspace>>,
    /// 铃铛面板（popover）是否展开
    bell_panel_open: bool,
    /// 弹窗正在播放关闭动画：保持 popover 渲染直到动画结束。
    bell_panel_animating_out: bool,
    /// popover 因"外部 mouse_down_out"关闭的时间点。GPUI 里 mouse_down_out 早于
    /// 铃铛按钮的 on_mouse_down 触发，因此点击铃铛时 popover 会先关闭，随后
    /// 铃铛处理器看到 `bell_panel_open == false` 便误打开。用 ~300ms 窗口抑制。
    bell_panel_dismissed_at: Option<Instant>,
    /// 每次"打开/关闭"切换递增，作为动画元素 id 的一部分，
    /// 确保动画从 delta=0 开始重新播放。
    bell_anim_epoch: u32,
    /// 已经处理过的最大任务 id，用于检测"新任务"并自动展开弹窗。
    last_seen_task_id: Option<usize>,
    /// 当前悬停的按钮（任务 id + 动作），用于把状态副文本临时替换为功能说明
    bell_hover: Option<(usize, TransferHoverAction)>,
    /// workspace observer / 定时器 subscription，随 TitleBar 释放。
    _subscriptions: Vec<gpui::Subscription>,
    /// 定时刷新任务句柄
    _progress_refresh_task: Option<gpui::Task<()>>,
    /// 关闭动画结束后清除 `bell_panel_animating_out` 的延迟任务。
    _bell_hide_task: Option<gpui::Task<()>>,
    /// 覆盖 TitleBar 默认左侧内边距。
    /// 当编辑器打开时设为 `3px`，使终端 TabBar 与编辑器分割线保持 3px 间距。
    left_padding: Pixels,
}

impl TitleBar {
    /// Create a new TitleBar.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            last_click_time: None,
            tab_bar: None,
            should_move: false,
            workspace: None,
            bell_panel_open: false,
            bell_panel_animating_out: false,
            bell_panel_dismissed_at: None,
            bell_anim_epoch: 0,
            last_seen_task_id: None,
            bell_hover: None,
            _subscriptions: Vec::new(),
            _progress_refresh_task: None,
            _bell_hide_task: None,
            left_padding: TITLE_BAR_LEFT_PADDING,
        }
    }

    /// 设置 TitleBar 左侧内边距覆盖值。用于在编辑器打开时精确控制
    /// 终端 TabBar 与编辑器分割线的间距。
    pub fn set_left_padding(&mut self, padding: Pixels) {
        self.left_padding = padding;
    }

    /// 重置 TitleBar 左侧内边距为平台默认值。
    pub fn reset_left_padding(&mut self) {
        self.left_padding = TITLE_BAR_LEFT_PADDING;
    }

    /// Create a new TitleBar as an Entity.
    pub fn new_entity(cx: &mut App) -> Entity<Self> {
        cx.new(|_cx| Self::new("title-bar"))
    }

    /// Create a new TitleBar with a TabBar.
    pub fn with_tab_bar(mut self, tab_bar: Entity<TabBar>) -> Self {
        self.tab_bar = Some(tab_bar);
        self
    }

    /// 切换 TitleBar 绑定的工作区，同时更新 TabBar 和任务观察订阅。
    pub fn set_workspace(&mut self, workspace: Entity<Workspace>, cx: &mut Context<Self>) {
        self.tab_bar = Some(workspace.read(cx).tab_bar.clone());
        self._subscriptions.clear();
        self.workspace = None;
        self.last_seen_task_id = None;
        self.bind_workspace(workspace.downgrade(), cx);
        cx.notify();
    }

    /// 绑定 workspace 弱引用并启动订阅 + 定时刷新，以便实时反馈传输进度。
    pub fn bind_workspace(&mut self, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) {
        if let Some(strong) = workspace.upgrade() {
            // 初始化"已见任务 id"基线，避免已有任务误触发自动展开。
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
                        if should_open && !this.bell_panel_open {
                            this.open_bell_panel(cx);
                        }
                    } else if this.bell_panel_open || this.bell_panel_animating_out {
                        // 任务列表清空后立即关闭 popover（而不是在 render 中修改状态）。
                        this.close_bell_panel(cx);
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
                    let popover_visible = this.bell_panel_open || this.bell_panel_animating_out;
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

    /// Returns the default title bar options for compatible with the [`TitleBar`].
    pub fn title_bar_options() -> TitlebarOptions {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(15.0), px(16.0))),
        }
    }

    fn handle_title_click(&mut self, window: &mut Window) {
        let now = Instant::now();

        if let Some(prev_time) = self.last_click_time {
            let elapsed = now.duration_since(prev_time);
            if elapsed < DOUBLE_CLICK_THRESHOLD {
                // Double-click detected, toggle maximize
                window.zoom_window();
                self.last_click_time = None;
                return;
            }
        }

        // Single click - start window move
        self.last_click_time = Some(now);
        window.start_window_move();
    }

    fn open_bell_panel(&mut self, cx: &mut Context<Self>) {
        if self.bell_panel_open {
            return;
        }
        // 取消尚未完成的退出动画，避免内容被提前卸载。
        self.bell_panel_animating_out = false;
        self._bell_hide_task = None;
        self.bell_panel_dismissed_at = None;
        self.bell_panel_open = true;
        self.bell_anim_epoch = self.bell_anim_epoch.wrapping_add(1);
        cx.notify();
    }

    fn toggle_bell_panel(&mut self, cx: &mut Context<Self>) {
        // 点击铃铛时若 popover 刚被 on_mouse_down_out 关掉（几十毫秒内），
        // 说明这是同一次鼠标下压事件：此时不要再打开，让"点击铃铛关闭"
        // 生效，而不是"关→开"抖动。
        if !self.bell_panel_open {
            if let Some(dismissed_at) = self.bell_panel_dismissed_at
                && dismissed_at.elapsed() < Duration::from_millis(300)
            {
                self.bell_panel_dismissed_at = None;
                return;
            }
        }
        if self.bell_panel_open {
            self.close_bell_panel(cx);
        } else {
            self.open_bell_panel(cx);
        }
    }

    fn close_bell_panel(&mut self, cx: &mut Context<Self>) {
        if !self.bell_panel_open {
            return;
        }
        self.bell_panel_open = false;
        // 进入退出动画阶段：保持 popover 渲染，但以"closing"方向绘制。
        self.bell_panel_animating_out = true;
        self.bell_anim_epoch = self.bell_anim_epoch.wrapping_add(1);
        cx.notify();
        // 退出动画结束后卸载 popover。略多留几毫秒以容忍帧调度抖动。
        let entity = cx.entity();
        self._bell_hide_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(BELL_PANEL_EXIT_MS + 40))
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.bell_panel_animating_out = false;
                cx.notify();
            });
        }));
    }

    fn render_bell_trigger(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
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
            .mr(px(14.0))
            .rounded(px(999.0))
            .text_color(theme.text)
            .hover(|style| style.bg(theme.title_bar_hover))
            .cursor_pointer()
            // 在 mouse_down 阶段就完成 toggle，并 stop_propagation：这样打开状态下
            // 点击铃铛只触发本处理 (close)，不会让 popover 的 on_mouse_down_out 再
            // 次生效导致"close→open"的视觉抖动。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_bell_panel(cx);
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
                        .left(px(4.0))
                        .w(px(20.0))
                        .h(px(3.0))
                        .rounded_md()
                        .bg(theme.border_strong)
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(overall_ratio))
                                .rounded_md()
                                .bg(gpui::rgb(0x3b82f6)),
                        ),
                )
            });

        let popover = if self.bell_panel_open || self.bell_panel_animating_out {
            self.render_bell_popover(&workspace, cx)
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

    fn render_bell_popover(
        &self,
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let theme = cx.theme().ui;
        let tasks = workspace.read(cx).transfer_tasks().to_vec();

        let weak_cancel = workspace.downgrade();
        let weak_remove = workspace.downgrade();
        let weak_clear = workspace.downgrade();
        let weak_self_hover = cx.entity().downgrade();

        let panel = render_transfer_panel(
            &tasks,
            theme,
            self.bell_hover,
            move |hover, app| {
                if let Some(this) = weak_self_hover.upgrade() {
                    this.update(app, |this, cx| {
                        if this.bell_hover != hover {
                            this.bell_hover = hover;
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

        // 为了模仿 macOS 系统通知的"轻柔滑入、收紧滑出"手感：
        // - 进入：从按钮正下方略微上方位置（top-8px）以 0 不透明度开始，
        //   以带轻微回弹的 ease-out 曲线滑入并淡入。
        // - 退出：沿相反方向略微向上滑动并淡出，采用较快的 ease-in。
        let is_exit = !self.bell_panel_open && self.bell_panel_animating_out;
        let duration_ms = if is_exit {
            BELL_PANEL_EXIT_MS
        } else {
            BELL_PANEL_ENTER_MS
        };
        let epoch = self.bell_anim_epoch;
        let animation_id: ElementId = if is_exit {
            ("bell-popover-exit", epoch).into()
        } else {
            ("bell-popover-enter", epoch).into()
        };

        let animated_panel = div()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                // 标记这是"外部点击"触发的关闭；若这次下压的目标其实是铃铛本身，
                // 紧接着的 toggle_bell_panel 会凭这个时间戳跳过重新打开。
                this.bell_panel_dismissed_at = Some(Instant::now());
                this.close_bell_panel(cx);
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
                .top(px(44.0))
                .right(px(14.0))
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
/// 贴合 macOS 通知上移收拢的"快收"节奏。
fn bell_exit_easing(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

impl Default for TitleBar {
    fn default() -> Self {
        Self::new("title-bar")
    }
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let is_web = cfg!(target_family = "wasm");
        let is_linux = cfg!(target_os = "linux");
        let is_macos = cfg!(target_os = "macos");
        let theme = cx.theme().ui;

        // 注意：TabBar 现在使用 ScrollHandle.max_offset() 自动检测溢出
        // 不需要从外部传入可用宽度

        // Main title bar container - 与工作空间背景色一致
        div()
            .id(self.id.clone())
            .flex()
            .flex_row()
            .items_center()
            .h(TITLE_BAR_HEIGHT)
            .pl(self.left_padding)
            .bg(theme.app_background)
            // TabBar (如果有) - 阻止事件冒泡以防止触发窗口拖拽
            .when_some(self.tab_bar.clone(), |this, tab_bar| {
                this.child(
                    div()
                        .flex_shrink(1.0)
                        .min_w_0() // 允许收缩到 0 宽度
                        // 让 TabBar 可以收缩，但不会挤压窗口控制按钮
                        .when(!is_macos && !is_web, |div| {
                            div.mr(px(0.)) // 右边距留出窗口控制按钮空间
                        })
                        // macOS: 不需要留出空间，使用系统控制按钮
                        .when(is_macos || is_web, |div| div.max_w_full())
                        // 阻止 TabBar 区域的鼠标事件冒泡到 TitleBar，防止触发窗口拖拽
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .child(tab_bar),
                )
            })
            // 可拖拽区域容器 - 只有空白区域可以拖拽窗口
            .child(
                div()
                    .flex()
                    .h_full()
                    .flex_1()
                    // Windows/Linux: 使用 window_control_area
                    // macOS: 不使用 window_control_area，改用 GPUI 手动事件处理
                    .when(!is_macos && !is_web, |this| {
                        this.window_control_area(WindowControlArea::Drag)
                    })
                    // Right-click context menu on Linux
                    .when(is_linux && is_client_decorated && !is_web, |this| {
                        this.child(
                            div()
                                .top_0()
                                .left_0()
                                .absolute()
                                .size_full()
                                .h_full()
                                .on_mouse_down(MouseButton::Right, move |ev, window, _| {
                                    window.show_window_menu(ev.position)
                                }),
                        )
                    })
                    // macOS: 手动实现窗口拖拽
                    .when(is_macos, |this| {
                        this.on_mouse_down_out(cx.listener(|this, _, _, _| {
                            this.should_move = false;
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| {
                                this.should_move = false;
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| {
                                this.should_move = true;
                            }),
                        )
                        .on_mouse_move(cx.listener(
                            |this, _, window, _| {
                                if this.should_move {
                                    this.should_move = false;
                                    window.start_window_move();
                                }
                            },
                        ))
                    })
                    // Windows/Linux: 双击最大化/恢复，单击拖拽
                    .when(!is_web, |this| {
                        this.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, _| {
                                this.handle_title_click(window);
                            }),
                        )
                    }),
            )
            // 铃铛（通知）按钮：仅当 workspace 中存在任务时显示
            .when_some(self.render_bell_trigger(cx), |this, trigger| {
                this.child(trigger)
            })
            // 窗口控制按钮组 (仅 Windows/Linux)
            .when(!is_macos && !is_web, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_row()
                        .h_full()
                        .flex_shrink_0()
                        // 最小化按钮
                        .child(
                            div()
                                .id("minimize-btn")
                                .flex()
                                .w(TITLE_BAR_HEIGHT)
                                .h_full()
                                .justify_center()
                                .items_center()
                                .hover(move |style| style.bg(theme.title_bar_hover))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(|_, _, window, _| {
                                    window.minimize_window();
                                }))
                                .child(
                                    gpui::svg()
                                        .size(px(16.))
                                        .text_color(theme.icon)
                                        .path(crate::assets::Icon::WindowMinimize.path()),
                                ),
                        )
                        // 最大化/恢复按钮
                        .child(
                            div()
                                .id("maximize-btn")
                                .flex()
                                .w(TITLE_BAR_HEIGHT)
                                .h_full()
                                .justify_center()
                                .items_center()
                                .hover(move |style| style.bg(theme.title_bar_hover))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(|_, _, window, _| {
                                    window.zoom_window();
                                }))
                                .child(gpui::svg().size(px(16.)).text_color(theme.icon).path(
                                    if window.is_maximized() {
                                        crate::assets::Icon::WindowRestore.path()
                                    } else {
                                        crate::assets::Icon::WindowMaximize.path()
                                    },
                                )),
                        )
                        // 关闭按钮
                        .child(
                            div()
                                .id("close-btn")
                                .flex()
                                .w(TITLE_BAR_HEIGHT)
                                .h_full()
                                .justify_center()
                                .items_center()
                                .hover(move |style| style.bg(theme.error))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.quit();
                                }))
                                .child(
                                    gpui::svg()
                                        .size(px(16.))
                                        .text_color(theme.icon)
                                        .path(crate::assets::Icon::WindowClose.path()),
                                ),
                        ),
                )
            })
    }
}
