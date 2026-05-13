//! Cursor blinking state machine for the terminal view.
//!
//! Timing approximated after Zed's `BlinkManager` (see
//! `zed/crates/editor/src/blink_manager.rs`) and keeps Alacritty's typing
//! pause semantics (`on_typing_start`). Driven by a `gpui::Task`; each
//! transition is tagged with a monotonically increasing epoch so stale timers
//! cannot flip `visible` after `start`/`stop`/`pause` invalidated them.

use gpui::{Context, Task};
use std::time::Duration;

const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const PAUSE_DELAY: Duration = Duration::from_millis(300);

pub struct BlinkCursor {
    visible: bool,
    paused: bool,
    enabled: bool,
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
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns whether the cursor should currently be rendered.
    /// If blinking is disabled the cursor is always visible.
    pub fn visible(&self) -> bool {
        if !self.enabled {
            return true;
        }
        self.paused || self.visible
    }

    /// Enable blinking. Starts the XOR timer from the current state.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        self.paused = false;
        self.visible = true;
        let epoch = self.next_epoch();
        self.blink(epoch, cx);
        cx.notify();
    }

    /// Disable blinking and leave the cursor in the visible state so the next
    /// render draws it.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.enabled = false;
        self.paused = false;
        self.visible = true;
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
                    this.blink(epoch, cx);
                });
            }
        });
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    fn blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if !self.enabled || self.paused || epoch != self.epoch {
            return;
        }

        self.visible = !self.visible;
        cx.notify();

        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BLINK_INTERVAL).await;
            if let Some(this) = this.upgrade() {
                #[allow(clippy::let_unit_value)]
                let _ = this.update(cx, |this, cx| this.blink(epoch, cx));
            }
        });
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
    }

    #[test]
    fn visible_when_paused_even_if_internal_visible_is_false() {
        let mut blink = new_cursor();
        blink.enabled = true;
        blink.visible = false;
        blink.paused = true;
        assert!(blink.visible());
    }

    #[test]
    fn hidden_phase_only_returns_false_when_enabled_and_not_paused() {
        let mut blink = new_cursor();
        blink.enabled = true;
        blink.paused = false;
        blink.visible = false;
        assert!(!blink.visible());
    }
}

