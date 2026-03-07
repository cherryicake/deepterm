//! Unix PTY implementation using alacritty_terminal (same as Zed)

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Mutex;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, Options};

use super::{Pty, PtyConfig};

/// Unix PTY implementation using alacritty_terminal's PTY
pub struct UnixPty {
    pty: Mutex<InnerPty>,
}

struct InnerPty {
    pty: tty::Pty,
    master_fd: i32,
}

impl Read for InnerPty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.pty.file().read(buf)
    }
}

impl Write for InnerPty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pty.file().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pty.file().flush()
    }
}

impl Pty for UnixPty {
    fn master(&self) -> &dyn io::Read {
        // This is safe because we're returning a reference to a field that lives as long as self
        // and we know the Mutex won't be moved
        let inner = self.pty.lock().unwrap();
        let ptr = &*inner as *const InnerPty;
        unsafe { &*ptr }
    }

    fn master_write(&self) -> &dyn io::Write {
        let inner = self.pty.lock().unwrap();
        let ptr = &*inner as *const InnerPty;
        unsafe { &*ptr }
    }

    fn resize(&self, cols: usize, rows: usize) -> io::Result<()> {
        let window_size = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: 8,   // Default cell width
            cell_height: 16, // Default cell height
        };
        self.pty.lock().unwrap().pty.on_resize(window_size);
        Ok(())
    }

    fn child_pid(&self) -> Option<u32> {
        Some(self.pty.lock().unwrap().pty.child().id())
    }

    fn is_alive(&self) -> bool {
        // Use kill(0) to check if process exists without sending a signal
        unsafe { libc::kill(self.pty.lock().unwrap().pty.child().id() as i32, 0) == 0 }
    }

    fn kill(&self) -> io::Result<()> {
        unsafe {
            let result = libc::kill(
                self.pty.lock().unwrap().pty.child().id() as i32,
                libc::SIGTERM,
            );
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.pty.lock().unwrap().master_fd;

        // Set non-blocking mode temporarily
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }

        let new_flags = flags | libc::O_NONBLOCK;
        if unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) } < 0 {
            return Err(io::Error::last_os_error());
        }

        // Try to read
        let result = unsafe {
            let n = libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len());
            if n < 0 {
                let err = io::Error::last_os_error();
                // Restore flags before returning error
                libc::fcntl(fd, libc::F_SETFL, flags);
                Err(err)
            } else {
                // Restore flags
                libc::fcntl(fd, libc::F_SETFL, flags);
                Ok(n as usize)
            }
        };

        result
    }

    fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        self.pty.lock().unwrap().pty.file().write(buf)
    }
}

/// Create a new Unix PTY using alacritty_terminal
pub fn create_pty(config: PtyConfig) -> io::Result<Box<dyn Pty>> {
    let window_size = WindowSize {
        num_lines: config.size.1 as u16,
        num_cols: config.size.0 as u16,
        cell_width: 8,
        cell_height: 16,
    };

    let options = Options {
        shell: None, // Use default shell
        working_directory: config.working_directory.map(PathBuf::from),
        hold: false,
        env: config.env,
    };

    // Create PTY using alacritty_terminal (same as Zed)
    let pty =
        tty::new(&options, window_size, 0).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let master_fd = pty.file().as_raw_fd();

    Ok(Box::new(UnixPty {
        pty: Mutex::new(InnerPty { pty, master_fd }),
    }))
}
