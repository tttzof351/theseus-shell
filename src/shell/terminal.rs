use std::{io, io::IsTerminal};

#[cfg(unix)]
use std::{fs::File, fs::OpenOptions, io::Read};

#[cfg(unix)]
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

#[cfg(unix)]
struct NonBlockingFileGuard {
    file: File,
    previous_flags: OFlags,
}

#[cfg(unix)]
impl NonBlockingFileGuard {
    fn enable(file: File) -> io::Result<Self> {
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

#[cfg(unix)]
pub(super) fn discard_pending_terminal_input() -> io::Result<()> {
    if !io::stdin().is_terminal() {
        return Ok(());
    }

    let mut tty = NonBlockingFileGuard::enable(OpenOptions::new().read(true).open("/dev/tty")?)?;
    let mut buffer = [0; 1024];

    loop {
        match tty.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err),
        }
    }
}

#[cfg(not(unix))]
pub(super) fn discard_pending_terminal_input() -> io::Result<()> {
    Ok(())
}
