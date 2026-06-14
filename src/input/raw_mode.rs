use std::io::{self, Write};

use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};

use crate::common::terminal_output;

pub(crate) struct RawModeGuard;

impl RawModeGuard {
    pub(crate) fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        terminal_output::with_stdout(|stdout| {
            execute!(stdout, EnableBracketedPaste)?;
            stdout.flush()
        })?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal_output::with_stdout(|stdout| {
            execute!(stdout, DisableBracketedPaste)?;
            stdout.flush()
        });
        let _ = disable_raw_mode();
    }
}
