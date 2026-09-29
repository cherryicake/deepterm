//! 输入光标闪烁管理，对标 Zed `BlinkManager` + VSCode `editor.cursorBlinking`。
//!
//! - Zed (`crates/editor/src/blink_manager.rs`): `enable` 先置 `visible=false`
//!   再进入闪烁循环（下一帧即见光标，相位稳定）；`pause_blinking` 常显 500ms 后恢复；
//!   `disable` 隐藏并停止；`blink_enabled_in_settings=false`（solid 模式）时常显。
//! - VSCode: `blink`（默认约 500ms 周期）/ `solid`（常显）/ `smooth` / `phase` / `expand`；
//!   输入、移动、IME 组合期间暂停闪烁保持常显，失焦停止。

use gpui::{Context, Pixels, Task, px};
use std::time::{Duration, Instant};

pub(crate) const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const PAUSE_DELAY: Duration = Duration::from_millis(500);
/// 相位末尾的淡出 / 淡入时长。
const FADE: Duration = Duration::from_millis(100);
/// 淡出窗口开始的时刻（相位起点之后这么久）。
const FADE_LEAD: Duration = BLINK_INTERVAL.saturating_sub(FADE);

// On Windows, Linux, we should use integer to avoid blurry cursor.
#[cfg(not(target_os = "macos"))]
pub(crate) const CURSOR_WIDTH: Pixels = px(2.);
#[cfg(target_os = "macos")]
pub(crate) const CURSOR_WIDTH: Pixels = px(1.5);

pub(crate) struct BlinkCursor {
    visible: bool,
    blinking_paused: bool,
    enabled: bool,
    blink_enabled: bool,
    /// 当前相位（显示 / 隐藏）的起始时刻，用于计算淡入淡出进度。
    phase_start: Instant,
    epoch: usize,
    _task: Task<()>,
}

impl BlinkCursor {
    pub fn new() -> Self {
        Self {
            visible: true,
            blinking_paused: false,
            enabled: false,
            blink_enabled: true,
            phase_start: Instant::now(),
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.enable(cx);
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.disable(cx);
    }

    pub fn enable(&mut self, cx: &mut Context<Self>) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        self.visible = false;
        self.phase_start = Instant::now();
        self.blink_cursors(self.epoch, cx);
    }

    pub fn disable(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.visible = false;
        self.enabled = false;
        self.blinking_paused = false;
        self.phase_start = Instant::now();
        cx.notify();
    }

    pub fn show_cursor(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.visible = true;
            self.phase_start = Instant::now();
            cx.notify();
        }
    }

    pub fn set_blink_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.blink_enabled = enabled;
        if !enabled {
            self.show_cursor(cx);
        } else if self.enabled {
            self.visible = false;
            self.blink_cursors(self.epoch, cx);
        }
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    fn blink_cursors(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if !self.blink_enabled {
            self.show_cursor(cx);
            return;
        }
        if epoch == self.epoch && self.enabled && !self.blinking_paused {
            self.visible = !self.visible;
            self.phase_start = Instant::now();
            cx.notify();
            let epoch = self.next_epoch();
            let interval = BLINK_INTERVAL;
            self._task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(interval).await;
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| this.blink_cursors(epoch, cx));
                }
            });
        }
    }

    fn resume_cursor_blinking(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if epoch == self.epoch {
            self.blinking_paused = false;
            self.blink_cursors(epoch, cx);
        }
    }

    pub fn visible(&self) -> bool {
        if !self.blink_enabled {
            return true;
        }
        self.blinking_paused || self.visible
    }

    /// 光标当前的绘制不透明度（0.0 = 完全隐藏，1.0 = 实心）。
    ///
    /// 与终端一致：相位末尾的 [`FADE`] 窗口内线性淡出 / 淡入，其余时间为
    /// 0.0 或 1.0。相位中间不需要连续帧，窗口内由元素侧依据
    /// [`Self::needs_repaint`] 续订。
    pub fn opacity(&self) -> f32 {
        if !self.blink_enabled || self.blinking_paused {
            return 1.0;
        }
        let fade = self.fade_progress();
        if self.visible { fade } else { 1.0 - fade }
    }

    /// 是否正处于淡入 / 淡出窗口内，需要连续重绘。
    ///
    /// 元素每帧问一次：为真就请求下一帧，为假就自然停帧——静止的闪烁
    /// 不会让渲染循环空转。
    pub fn needs_repaint(&self) -> bool {
        self.enabled
            && self.blink_enabled
            && !self.blinking_paused
            && self.phase_start.elapsed() >= FADE_LEAD
    }

    /// 相位内剩余时间对应的淡入淡出进度：1.0 = 尚未进入窗口，0.0 = 窗口结束。
    fn fade_progress(&self) -> f32 {
        let remaining = BLINK_INTERVAL.saturating_sub(self.phase_start.elapsed());
        (remaining.as_secs_f32() / FADE.as_secs_f32()).min(1.0)
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.pause_blinking(cx);
    }

    pub fn pause_blinking(&mut self, cx: &mut Context<Self>) {
        self.show_cursor(cx);
        self.blinking_paused = true;
        self.phase_start = Instant::now();
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE_DELAY).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.resume_cursor_blinking(epoch, cx));
            }
        });
    }
}
