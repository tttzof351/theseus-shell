//! Terminal ownership changes only at the managed/shell boundary.

use super::terminal_motion::{MotionSnapshot, TerminalMotion};
use crate::common::terminal_input::{SharedInput, TerminalInput};
use crossterm::{
    cursor::{MoveTo, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    style::Print,
    terminal::{self, Clear, ClearType},
};
use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub(super) struct TerminalController {
    input: SharedInput,
    leased: Arc<AtomicBool>,
    output: Arc<Mutex<TerminalOutput>>,
}

struct TerminalOutput {
    stdout: io::Stdout,
    motion: TerminalMotion,
}

pub(super) struct TerminalWriter {
    output: Arc<Mutex<TerminalOutput>>,
    leased: Arc<AtomicBool>,
    shell: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ShellDisplay {
    pub scrolled: usize,
    pub cleared: bool,
    pub published_byte: Option<usize>,
}

impl Write for TerminalWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.leased.load(Ordering::Acquire) != self.shell {
            return Err(io::Error::other("terminal writer used outside its lease"));
        }
        let mut output = self
            .output
            .lock()
            .map_err(|_| io::Error::other("terminal output poisoned"))?;
        let (cols, rows) = terminal::size()?;
        output.motion.resize(usize::from(rows), usize::from(cols));
        let count = output.stdout.write(bytes)?;
        output.motion.process(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output
            .lock()
            .map_err(|_| io::Error::other("terminal output poisoned"))?
            .stdout
            .flush()
    }
}

impl crate::shell::pty::PtyOutput for TerminalWriter {
    fn resized(&mut self, size: portable_pty::PtySize) -> io::Result<()> {
        if !self.shell || !self.leased.load(Ordering::Acquire) {
            return Err(io::Error::other("PTY resize reported outside shell lease"));
        }
        self.output
            .lock()
            .map_err(|_| io::Error::other("terminal output poisoned"))?
            .motion
            .resize(usize::from(size.rows), usize::from(size.cols));
        Ok(())
    }
}

impl TerminalController {
    pub(super) fn is_leased(&self) -> bool {
        self.leased.load(Ordering::Acquire)
    }

    pub(super) fn enter() -> io::Result<Self> {
        let input = TerminalInput::open()?;
        let (cols, rows) = terminal::size()?;
        terminal::enable_raw_mode()?;
        let controller = Self {
            input,
            leased: Arc::new(AtomicBool::new(false)),
            output: Arc::new(Mutex::new(TerminalOutput {
                stdout: io::stdout(),
                motion: TerminalMotion::new(usize::from(rows), usize::from(cols)),
            })),
        };
        execute!(
            controller.writer(),
            EnableBracketedPaste,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        Ok(controller)
    }

    pub(super) fn writer(&self) -> TerminalWriter {
        TerminalWriter {
            output: self.output.clone(),
            leased: self.leased.clone(),
            shell: false,
        }
    }

    pub(super) fn next_event(&self, timeout: Duration) -> io::Result<Option<Event>> {
        if self.leased.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "managed input requested during shell lease",
            ));
        }
        self.input
            .lock()
            .map_err(|_| io::Error::other("terminal input poisoned"))?
            .next_event(timeout)
    }

    pub(super) fn lease_shell(&self) -> io::Result<ShellLease> {
        if self.leased.swap(true, Ordering::AcqRel) {
            return Err(io::Error::other("terminal is already leased"));
        }
        let start = self
            .output
            .lock()
            .map_err(|_| io::Error::other("terminal output poisoned"))?
            .motion
            .state
            .snapshot;
        let mut lease = ShellLease {
            input: self.input.clone(),
            leased: self.leased.clone(),
            writer: TerminalWriter {
                output: self.output.clone(),
                leased: self.leased.clone(),
                shell: true,
            },
            start,
        };
        // Keep raw mode throughout hand-off: a cooked-mode interval could echo
        // or transform bytes arriving between Enter and the PTY forwarder.
        execute!(lease.writer, DisableBracketedPaste, Show, Print("\r\n"))?;
        lease.writer.flush()?;
        lease
            .writer
            .output
            .lock()
            .map_err(|_| io::Error::other("terminal output poisoned"))?
            .motion
            .begin_source();
        Ok(lease)
    }
}

impl Drop for TerminalController {
    fn drop(&mut self) {
        let mut output = io::stdout();
        let _ = execute!(output, DisableBracketedPaste, Print("\x1b[0m"), Show);
        let _ = terminal::disable_raw_mode();
        let _ = write!(output, "\r\n");
        let _ = output.flush();
    }
}

pub(super) struct ShellLease {
    pub(super) input: SharedInput,
    pub(super) writer: TerminalWriter,
    leased: Arc<AtomicBool>,
    start: MotionSnapshot,
}

impl ShellLease {
    pub(super) fn display(&self) -> ShellDisplay {
        let mut output = self.writer.output.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = output.motion.state.snapshot;
        output.motion.end_source();
        let cleared = snapshot.clears != self.start.clears;
        ShellDisplay {
            scrolled: snapshot.scrolled
                - if cleared {
                    snapshot.scroll_at_clear
                } else {
                    self.start.scrolled
                },
            cleared,
            published_byte: snapshot.published_byte,
        }
    }
}

impl Drop for ShellLease {
    fn drop(&mut self) {
        self.writer
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .motion
            .end_source();
        let alternate = self
            .writer
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .motion
            .alternate_screen();
        if alternate {
            let _ = self.writer.write_all(b"\x1b[?1049l");
        }
        // A failed TUI may leave cursor origin, margins or autowrap altered.
        // Restore the renderer's coordinate system before the managed repaint.
        let _ = execute!(
            self.writer,
            Print("\x1b[r\x1b[?6l\x1b[?7h\x1b[20l\x1b[0m"),
            EnableBracketedPaste
        );
        self.leased.store(false, Ordering::Release);
    }
}
