//! PTY (Pseudo Terminal) management
//!
//! Cross-platform PTY creation and management

use std::io;
use std::process::Command;

/// PTY configuration
#[derive(Clone, Debug)]
pub struct PtyConfig {
    /// Shell to spawn (defaults to $SHELL or /bin/sh)
    pub shell: String,
    /// Working directory
    pub working_directory: Option<String>,
    /// Environment variables
    pub env: Vec<(String, String)>,
    /// Terminal size (cols, rows)
    pub size: (usize, usize),
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
            working_directory: None,
            env: vec![],
            size: (80, 24),
        }
    }
}

/// Platform-specific PTY implementation
#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod windows;

/// PTY handle trait
pub trait Pty: Send + Sync {
    /// Get the master file descriptor/handle for reading/writing
    fn master(&self) -> &dyn io::Read;

    /// Get the master file descriptor/handle for writing
    fn master_write(&self) -> &dyn io::Write;

    /// Resize the PTY
    fn resize(&self, cols: usize, rows: usize) -> io::Result<()>;

    /// Get the child process ID
    fn child_pid(&self) -> Option<u32>;

    /// Check if the child process is still running
    fn is_alive(&self) -> bool;

    /// Kill the child process
    fn kill(&self) -> io::Result<()>;

    /// Try to read data from PTY (non-blocking)
    fn try_read(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Try to write data to PTY
    fn try_write(&self, buf: &[u8]) -> io::Result<usize>;
}

/// Create a new PTY with the given configuration
#[cfg(unix)]
pub fn create_pty(config: PtyConfig) -> io::Result<Box<dyn Pty>> {
    unix::create_pty(config)
}

#[cfg(windows)]
pub fn create_pty(config: PtyConfig) -> io::Result<Box<dyn Pty>> {
    windows::create_pty(config)
}
