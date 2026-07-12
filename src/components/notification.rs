use std::sync::atomic::{AtomicU64, Ordering};

use gpui::SharedString;

static NEXT_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotificationKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone)]
pub struct NotificationItem {
    pub id: u64,
    pub message: SharedString,
    pub kind: NotificationKind,
    pub hovered: bool,
}

/// Single-toast notification state. Only the latest item is kept.
pub struct NotificationManager {
    current: Option<NotificationItem>,
}

impl NotificationManager {
    pub fn new() -> Self {
        Self { current: None }
    }

    /// Replace any existing toast with a new one. Returns the new notification id.
    pub fn add(&mut self, message: impl Into<SharedString>, kind: NotificationKind) -> u64 {
        let id = NEXT_NOTIFICATION_ID.fetch_add(1, Ordering::SeqCst);
        self.current = Some(NotificationItem {
            id,
            message: message.into(),
            kind,
            hovered: false,
        });
        id
    }

    pub fn dismiss(&mut self, id: u64) {
        if self.current.as_ref().is_some_and(|n| n.id == id) {
            self.current = None;
        }
    }

    pub fn current(&self) -> Option<&NotificationItem> {
        self.current.as_ref()
    }

    pub fn set_hovered(&mut self, id: u64, hovered: bool) {
        if let Some(item) = self.current.as_mut().filter(|n| n.id == id) {
            item.hovered = hovered;
        }
    }

    /// Returns whether the notification with `id` should auto-dismiss.
    /// Missing id means already dismissed.
    pub fn should_auto_dismiss(&self, id: u64) -> bool {
        match self.current.as_ref() {
            Some(item) if item.id == id => !item.hovered,
            _ => true,
        }
    }
}
