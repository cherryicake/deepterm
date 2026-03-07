/// Terminal events emitted to the UI layer
#[derive(Clone, Debug)]
pub enum Event {
    /// Terminal title changed
    TitleChanged(String),
    /// Working directory changed
    WorkingDirectoryChanged(String),
    /// Bell notification
    Bell,
    /// Cursor visibility changed
    CursorVisibilityChanged(bool),
    /// Selection changed
    SelectionChanged,
    /// Content changed (needs redraw)
    ContentChanged,
    /// Process exited
    Exited(i32),
    /// Process exited with error
    ExitedWithError(String),
}

/// Internal events for terminal processing
#[derive(Clone, Debug)]
pub enum InternalEvent {
    /// Data received from PTY
    PtyData(Vec<u8>),
    /// Window size changed
    Resize { cols: usize, rows: usize },
    /// Write data to PTY
    Write(Vec<u8>),
    /// Scroll up/down
    Scroll(i32),
    /// Clear scrollback
    ClearScrollback,
}
