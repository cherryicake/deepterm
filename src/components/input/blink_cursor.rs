//! 输入光标闪烁管理，对标 Zed `BlinkManager` + VSCode `editor.cursorBlinking`。
//!
//! - Zed (`crates/editor/src/blink_manager.rs`): `enable` 先置 `visible=false`
//!   再进入闪烁循环（下一帧即见光标，相位稳定）；`pause_blinking` 常显 500ms 后恢复；
//!   `disable` 隐藏并停止；`blink_enabled_in_settings=false`（solid 模式）时常显。
//! - VSCode: `blink`（默认约 500ms 周期）/ `solid`（常显）/ `smooth` / `phase` / `expand`；
//!   输入、移动、IME 组合期间暂停闪烁保持常显，失焦停止。

use gpui::{Context, Pixels, Task, px};
use std::time::Duration;

pub(crate) const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const PAUSE_DELAY: Duration = Duration::from_millis(500);

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
        self.blink_cursors(self.epoch, cx);
    }

    pub fn disable(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.visible = false;
        self.enabled = false;
        self.blinking_paused = false;
        cx.notify();
    }

    pub fn show_cursor(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.visible = true;
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

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.pause_blinking(cx);
    }

    pub fn pause_blinking(&mut self, cx: &mut Context<Self>) {
        self.show_cursor(cx);
        self.blinking_paused = true;
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE_DELAY).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.resume_cursor_blinking(epoch, cx));
            }
        });
    }
}
