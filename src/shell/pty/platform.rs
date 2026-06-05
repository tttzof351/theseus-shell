use std::{io, io::IsTerminal};

#[cfg(unix)]
use std::{fs::File, io::Read};

use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use portable_pty::PtySize;
#[cfg(unix)]
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

pub(super) struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    pub(super) fn enable_if_terminal() -> io::Result<Self> {
        if std::io::stdin().is_terminal() {
            enable_raw_mode()?;
            Ok(Self { enabled: true })
        } else {
            Ok(Self { enabled: false })
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = disable_raw_mode();
        }
    }
}

pub(super) fn current_pty_size() -> PtySize {
    let (cols, rows) = size().unwrap_or((80, 24));

    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(unix)]
pub(super) fn shell_command_args(command: &str) -> [&str; 2] {
    ["-c", command]
}

#[cfg(windows)]
pub(super) fn shell_command_args(command: &str) -> [&str; 2] {
    ["/C", command]
}

#[cfg(unix)]
pub(super) struct NonBlockingFileGuard {
    file: File,
    previous_flags: OFlags,
}

#[cfg(unix)]
impl NonBlockingFileGuard {
    pub(super) fn enable(file: File) -> io::Result<Self> {
        let previous_flags = fcntl_getfl(&file)?;
        fcntl_setfl(&file, previous_flags | OFlags::NONBLOCK)?;

        Ok(Self {
            file,
            previous_flags,
        })
    }
}

#[cfg(unix)]
impl Drop for NonBlockingFileGuard {
    fn drop(&mut self) {
        let _ = fcntl_setfl(&self.file, self.previous_flags);
    }
}

#[cfg(unix)]
impl Read for NonBlockingFileGuard {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}
