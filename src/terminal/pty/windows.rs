//! Windows PTY implementation
//!
//! Uses Windows ConPTY API

use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};

use super::{Pty, PtyConfig};

/// Windows PTY implementation
pub struct WindowsPty {
    conpty: HPCON,
    input: HANDLE,
    output: HANDLE,
    process_id: u32,
}

impl Read for WindowsPty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Read from output pipe
        let mut bytes_read: u32 = 0;
        let result = unsafe {
            windows::Win32::System::IO::ReadFile(
                self.output,
                Some(buf),
                Some(&mut bytes_read),
                None,
            )
        };

        if result.is_err() {
            return Err(io::Error::last_os_error());
        }

        Ok(bytes_read as usize)
    }
}

impl Write for WindowsPty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut bytes_written: u32 = 0;
        let result = unsafe {
            windows::Win32::System::IO::WriteFile(
                self.input,
                Some(buf),
                Some(&mut bytes_written),
                None,
            )
        };

        if result.is_err() {
            return Err(io::Error::last_os_error());
        }

        Ok(bytes_written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Pty for WindowsPty {
    fn master(&self) -> &dyn io::Read {
        self
    }

    fn master_write(&self) -> &dyn io::Write {
        self
    }

    fn resize(&self, cols: usize, rows: usize) -> io::Result<()> {
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };

        unsafe {
            ResizePseudoConsole(self.conpty, size).map_err(|_| io::Error::last_os_error())?;
        }

        Ok(())
    }

    fn child_pid(&self) -> Option<u32> {
        Some(self.process_id)
    }

    fn is_alive(&self) -> bool {
        // Check if process is still running
        unsafe {
            let handle = windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION,
                false,
                self.process_id,
            );

            if let Ok(handle) = handle {
                let mut exit_code: u32 = 0;
                let result =
                    windows::Win32::System::Threading::GetExitCodeProcess(handle, &mut exit_code);
                let _ = CloseHandle(handle);

                if result.is_ok() {
                    return exit_code == 259; // STILL_ACTIVE
                }
            }
        }

        false
    }

    fn kill(&self) -> io::Result<()> {
        unsafe {
            let handle = windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_TERMINATE,
                false,
                self.process_id,
            )
            .map_err(|_| io::Error::last_os_error())?;

            windows::Win32::System::Threading::TerminateProcess(handle, 1)
                .map_err(|_| io::Error::last_os_error())?;

            let _ = CloseHandle(handle);
        }

        Ok(())
    }
}

impl Drop for WindowsPty {
    fn drop(&mut self) {
        unsafe {
            let _ = ClosePseudoConsole(self.conpty);
            let _ = CloseHandle(self.input);
            let _ = CloseHandle(self.output);
        }
    }
}

/// Create a new Windows PTY
pub fn create_pty(config: PtyConfig) -> io::Result<Box<dyn Pty>> {
    // Create pipes for input/output
    let (input_read, input_write) = create_pipe()?;
    let (output_read, output_write) = create_pipe()?;

    // Create ConPTY
    let size = COORD {
        X: config.size.0 as i16,
        Y: config.size.1 as i16,
    };

    let conpty = unsafe {
        CreatePseudoConsole(size, input_read, output_write, 0)
            .map_err(|_| io::Error::last_os_error())?
    };

    // Close handles we don't need in parent
    unsafe {
        let _ = CloseHandle(input_read);
        let _ = CloseHandle(output_write);
    }

    // Create process
    let process_id = spawn_process(conpty, &config)?;

    Ok(Box::new(WindowsPty {
        conpty,
        input: input_write,
        output: output_read,
        process_id,
    }))
}

fn create_pipe() -> io::Result<(HANDLE, HANDLE)> {
    use windows::Win32::Security::SECURITY_ATTRIBUTES;

    let mut read: HANDLE = HANDLE(std::ptr::null_mut());
    let mut write: HANDLE = HANDLE(std::ptr::null_mut());

    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };

    unsafe {
        windows::Win32::System::Pipes::CreatePipe(&mut read, &mut write, Some(&security), 0)
            .map_err(|_| io::Error::last_os_error())?;
    }

    Ok((read, write))
}

fn spawn_process(conpty: HPCON, config: &PtyConfig) -> io::Result<u32> {
    use windows::Win32::System::Pipes::PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE;
    use windows::Win32::System::Threading::{
        CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, STARTUPINFOEXW,
    };

    // This is a simplified version - full implementation would need
    // proper attribute list setup for ConPTY

    // For now, return a placeholder
    // In a full implementation, you'd:
    // 1. InitializeProcThreadAttributeList
    // 2. UpdateProcThreadAttribute with PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE
    // 3. CreateProcessW with EXTENDED_STARTUPINFO_PRESENT

    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows PTY process spawning requires additional implementation",
    ))
}
