pub mod escape_scan;
pub mod file_link_detector;
pub mod foreground_argv;
pub mod kitty;
pub mod link_detector;
#[cfg(unix)]
pub mod local_pty;
pub mod mappings;
pub mod terminal;

/// 内存占用测量测试（footprint / 分配抖动证据链，见 docs/PERF_*.md）。
#[cfg(test)]
mod terminal_memory_tests;

/// ASCII 控制字符 Ctrl+V（SYN - Synchronous Idle）。
///
/// 粘贴图像时转发该字节让终端应用自行处理（见 `Terminal::paste`）。
/// 定义在模块根：`terminal_view` 曾反持此常量导致 `terminal → terminal_view`
/// 的模块环（terminal 域常量属于 terminal 域）。
pub const CTRL_V: u8 = 0x16;

pub use file_link_detector::{FileHit, detect_files_in_line};
pub use link_detector::{HyperlinkRange, UrlRange};
pub use terminal::{
    DriverEvent, Event, IndexedCell, SelectionPhase, Terminal, TerminalBounds, TerminalContent,
    TerminalType,
};
