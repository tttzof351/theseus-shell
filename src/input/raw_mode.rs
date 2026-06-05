use std::io;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

pub(crate) struct RawModeGuard;

impl RawModeGuard {
    pub(crate) fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
