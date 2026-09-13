//! The PTY transport reports geometry through the same owner as its bytes.

use std::io::{self, Write};

pub(crate) trait PtyOutput: Write {
    fn resized(&mut self, size: portable_pty::PtySize) -> io::Result<()>;
}

pub(super) struct LegacyOutput<'a>(pub &'a mut dyn Write);

impl Write for LegacyOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl PtyOutput for LegacyOutput<'_> {
    fn resized(&mut self, _: portable_pty::PtySize) -> io::Result<()> {
        Ok(())
    }
}
