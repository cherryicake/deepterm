//! Unix PTY implementation using libc

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::Command;

use libc::{self, c_int, winsize};

use super::{Pty, PtyConfig};

/// Unix PTY implementation
pub struct UnixPty {
    master: UnixMaster,
    child_pid: libc::pid_t,
}

struct UnixMaster {
    fd: RawFd,
}

impl Read for UnixMaster {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

impl Write for UnixMaster {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(self.fd, buf.as_ptr() as *const _, buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UnixMaster {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl Pty for UnixPty {
    fn master(&self) -> &dyn io::Read {
        &self.master
    }

    fn master_write(&self) -> &dyn io::Write {
        // This is safe because we're just returning a reference for writing
        // The actual write operation is done through the fd directly
        unsafe { &*(&self.master as *const UnixMaster as *const dyn io::Write) }
    }

    fn resize(&self, cols: usize, rows: usize) -> io::Result<()> {
        let ws = winsize {
            ws_col: cols as u16,
            ws_row: rows as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let result = unsafe { libc::ioctl(self.master.fd, libc::TIOCSWINSZ, &ws) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn child_pid(&self) -> Option<u32> {
        Some(self.child_pid as u32)
    }

    fn is_alive(&self) -> bool {
        let mut status: c_int = 0;
        let result = unsafe { libc::waitpid(self.child_pid, &mut status, libc::WNOHANG) };
        result == 0
    }

    fn kill(&self) -> io::Result<()> {
        let result = unsafe { libc::kill(self.child_pid, libc::SIGTERM) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // Set non-blocking mode temporarily
        let flags = unsafe { libc::fcntl(self.master.fd, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }

        let new_flags = flags | libc::O_NONBLOCK;
        if unsafe { libc::fcntl(self.master.fd, libc::F_SETFL, new_flags) } < 0 {
            return Err(io::Error::last_os_error());
        }

        // Try to read directly from fd
        let result = unsafe {
            let n = libc::read(self.master.fd, buf.as_mut_ptr() as *mut _, buf.len());
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        };

        // Restore original flags
        unsafe { libc::fcntl(self.master.fd, libc::F_SETFL, flags) };

        result
    }

    fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(self.master.fd, buf.as_ptr() as *const _, buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

/// Create a new Unix PTY
pub fn create_pty(config: PtyConfig) -> io::Result<Box<dyn Pty>> {
    let mut master_fd: c_int = 0;
    let mut slave_fd: c_int = 0;

    // Open PTY
    let result = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };

    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    // Fork child process
    let pid = unsafe { libc::fork() };

    if pid < 0 {
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        return Err(io::Error::last_os_error());
    }

    if pid == 0 {
        // Child process
        unsafe {
            // Close master
            libc::close(master_fd);

            // Create new session
            if libc::setsid() < 0 {
                libc::_exit(1);
            }

            // Set controlling terminal
            // macOS requires a non-zero third argument, Linux accepts 0
            #[cfg(target_os = "macos")]
            let result = libc::ioctl(slave_fd, libc::TIOCSCTTY, 1);
            #[cfg(not(target_os = "macos"))]
            let result = libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);

            if result < 0 {
                libc::_exit(1);
            }

            // Redirect stdin/stdout/stderr to slave
            if libc::dup2(slave_fd, 0) < 0
                || libc::dup2(slave_fd, 1) < 0
                || libc::dup2(slave_fd, 2) < 0
            {
                libc::_exit(1);
            }

            // Close slave (now duplicated)
            if slave_fd > 2 {
                libc::close(slave_fd);
            }

            // Set working directory
            if let Some(ref dir) = config.working_directory {
                let c_dir = CString::new(dir.as_str()).unwrap();
                if libc::chdir(c_dir.as_ptr()) < 0 {
                    libc::_exit(1);
                }
            }

            // Set environment variables
            for (key, value) in &config.env {
                let c_key = CString::new(key.as_str()).unwrap();
                let c_value = CString::new(value.as_str()).unwrap();
                libc::setenv(c_key.as_ptr(), c_value.as_ptr(), 1);
            }

            // Execute shell
            // Extract shell name from path for argv[0]
            let shell_path = config.shell.as_str();
            let shell_name = std::path::Path::new(shell_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("sh");

            let shell = CString::new(shell_path).unwrap();
            let argv0 = CString::new(format!("-{}", shell_name)).unwrap();

            libc::execl(shell.as_ptr(), argv0.as_ptr(), std::ptr::null::<()>());

            // If execl fails, exit
            libc::_exit(1);
        }
    }

    // Parent process
    unsafe {
        libc::close(slave_fd);
    }

    // Set initial size
    let ws = winsize {
        ws_col: config.size.0 as u16,
        ws_row: config.size.1 as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    unsafe {
        libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
    }

    Ok(Box::new(UnixPty {
        master: UnixMaster { fd: master_fd },
        child_pid: pid,
    }))
}
