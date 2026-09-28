//! Cursor blinking state machine for the terminal view.
//!
//! Timing approximated after Zed's `BlinkManager` (see
//! `zed/crates/editor/src/blink_manager.rs`) and keeps Alacritty's typing
//! pause semantics (`on_typing_start`). Driven by a `gpui::Task`; each
//! transition is tagged with a monotonically increasing epoch so stale timers
//! cannot flip `visible` after `start`/`stop`/`pause` invalidated them.
//!
//! 平滑闪烁：每个相位末尾的 [`FADE`] 窗口内 alpha 线性淡出 / 淡入，等价于
//! neovide 的 `cursor-smooth-blink`，但只渐变窗口而非整个相位——整段渐变
//! 需要 500ms 全程按帧重绘，对终端这种常年常驻的视图不划算。窗口内的连续
//! 帧由视图侧依据 [`BlinkCursor::needs_repaint`] 自行续订（与 Kitty 动画
//! 同一套「按需唤醒、静止即停」的约定）。

use gpui::{Context, Task};
use std::time::{Duration, Instant};

const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const PAUSE_DELAY: Duration = Duration::from_millis(300);
/// 相位末尾的淡出 / 淡入时长。
const FADE: Duration = Duration::from_millis(100);
/// 淡出窗口开始的时刻（相位起点之后这么久）。相位切换前需要先唤醒一帧，
/// 之后窗口内的连续帧由视图续订。
const FADE_LEAD: Duration = BLINK_INTERVAL.saturating_sub(FADE);

pub struct BlinkCursor {
    visible: bool,
    paused: bool,
    enabled: bool,
    /// 当前相位（显示 / 隐藏）的起始时刻，用于计算淡入淡出进度。
    phase_start: Instant,
    epoch: usize,
    _task: Task<()>,
}

impl Default for BlinkCursor {
    fn default() -> Self {
        Self::new()
    }
}

impl BlinkCursor {
    pub fn new() -> Self {
        Self {
            visible: true,
            paused: false,
            enabled: false,
            phase_start: Instant::now(),
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 光标当前的绘制不透明度（0.0 = 完全隐藏，1.0 = 实心）。
    ///
    /// 未启用闪烁或打字暂停期间恒为 1.0；否则在相位末尾的 [`FADE`] 窗口内
    /// 线性淡出 / 淡入，其余时间为 0.0 或 1.0。
    pub fn opacity(&self) -> f32 {
        if !self.enabled || self.paused {
            return 1.0;
        }
        let fade = self.fade_progress();
        if self.visible { fade } else { 1.0 - fade }
    }

    /// 光标是否应当绘制。等价于 `opacity() > 0.0`。
    ///
    /// 注意：淡入阶段（隐藏相位末尾）不透明度已大于 0，此时返回 `true`——
    /// 「是否绘制」与「完全不透明」是两回事，生产路径请用 [`Self::opacity`]。
    pub fn visible(&self) -> bool {
        self.opacity() > 0.0
    }

    /// 是否正处于淡入 / 淡出窗口内，需要连续重绘。
    ///
    /// 视图每帧问一次：为真就请求下一帧，为假就自然停帧——静止的闪烁不会
    /// 让渲染循环空转。
    pub fn needs_repaint(&self) -> bool {
        self.enabled && !self.paused && self.phase_start.elapsed() >= FADE_LEAD
    }

    /// 相位内剩余时间对应的淡入淡出进度：1.0 = 尚未进入窗口，0.0 = 窗口结束。
    fn fade_progress(&self) -> f32 {
        let remaining = BLINK_INTERVAL.saturating_sub(self.phase_start.elapsed());
        (remaining.as_secs_f32() / FADE.as_secs_f32()).min(1.0)
    }

    /// Enable blinking. Starts the XOR timer from the current state.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        self.paused = false;
        self.visible = true;
        self.phase_start = Instant::now();
        self.blink_loop(cx);
        cx.notify();
    }

    /// Disable blinking and leave the cursor in the visible state so the next
    /// render draws it.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.enabled = false;
        self.paused = false;
        self.visible = true;
        self.phase_start = Instant::now();
        self.epoch = self.epoch.wrapping_add(1);
        self._task = Task::ready(());
        cx.notify();
    }

    /// Force the cursor visible for `PAUSE_DELAY`, then resume blinking if
    /// enabled. Called on keystrokes so the caret never blinks mid-typing.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        self.paused = true;
        self.visible = true;
        self.phase_start = Instant::now();
        cx.notify();

        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE_DELAY).await;
            if let Some(this) = this.upgrade() {
                #[allow(clippy::let_unit_value)]
                let _ = this.update(cx, |this, cx| {
                    if this.epoch != epoch || !this.enabled {
                        return;
                    }
                    this.paused = false;
                    // 打字停止后先完整显示一个相位再淡出，避免从「暂停实心」
                    // 直接硬切到隐藏相位。
                    this.visible = true;
                    this.phase_start = Instant::now();
                    this.blink_loop(cx);
                });
            }
        });
    }

    /// 启动相位循环：每个相位在 [`FADE_LEAD`] 处唤醒一次（进入淡出 / 淡入
    /// 窗口），再在 [`FADE`] 之后翻转相位。
    fn blink_loop(&mut self, cx: &mut Context<Self>) {
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FADE_LEAD).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.epoch != epoch || !this.enabled || this.paused {
                            return false;
                        }
                        // 进入渐变窗口：唤醒一帧，窗口内的连续帧由视图续订。
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }

                cx.background_executor().timer(FADE).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.epoch != epoch || !this.enabled || this.paused {
                            return false;
                        }
                        this.visible = !this.visible;
                        this.phase_start = Instant::now();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        });
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_cursor() -> BlinkCursor {
        BlinkCursor::new()
    }

    #[test]
    fn disabled_cursor_is_always_visible() {
        let blink = new_cursor();
        assert!(!blink.enabled());
        assert!(blink.visible());
        assert_eq!(blink.opacity(), 1.0);
        assert!(!blink.needs_repaint());
    }

    #[test]
    fn visible_when_paused_even_if_internal_visible_is_false() {
        let mut blink = new_cursor();
        blink.enabled = true;
        blink.visible = false;
        blink.paused = true;
        assert!(blink.visible());
        assert_eq!(blink.opacity(), 1.0);
        assert!(!blink.needs_repaint());
    }

    #[test]
    fn hidden_phase_only_returns_false_when_enabled_and_not_paused() {
        let mut blink = new_cursor();
        blink.enabled = true;
        blink.paused = false;
        blink.visible = false;
        assert!(!blink.visible());
        // 相位刚开始：完全隐藏、且不在渐变窗口内。
        assert_eq!(blink.opacity(), 0.0);
        assert!(!blink.needs_repaint());
    }

    #[test]
    fn fade_out_at_end_of_visible_phase() {
        let mut blink = new_cursor();
        blink.enabled = true;
        blink.visible = true;
        // 相位已过 450ms：剩 50ms，处于 100ms 淡出窗口中点。
        blink.phase_start = Instant::now() - Duration::from_millis(450);
        let opacity = blink.opacity();
        assert!((opacity - 0.5).abs() < 0.1, "淡出中点应约 0.5: {opacity}");
        assert!(blink.needs_repaint());
    }

    #[test]
    fn fade_in_at_end_of_hidden_phase() {
        let mut blink = new_cursor();
        blink.enabled = true;
        blink.visible = false;
        blink.phase_start = Instant::now() - Duration::from_millis(450);
        let opacity = blink.opacity();
        assert!((opacity - 0.5).abs() < 0.1, "淡入中点应约 0.5: {opacity}");
        assert!(blink.needs_repaint());
    }

    #[test]
    fn steady_phases_do_not_request_repaint() {
        let mut blink = new_cursor();
        blink.enabled = true;
        blink.visible = true;
        blink.phase_start = Instant::now() - Duration::from_millis(100);
        assert_eq!(blink.opacity(), 1.0);
        assert!(!blink.needs_repaint());

        blink.visible = false;
        blink.phase_start = Instant::now() - Duration::from_millis(100);
        assert_eq!(blink.opacity(), 0.0);
        assert!(!blink.needs_repaint());
    }
}
