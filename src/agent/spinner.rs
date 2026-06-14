use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::{
    cursor::{Hide, Show},
    execute,
};

use crate::common::terminal_output;

pub(super) struct Spinner {
    stop: Option<Arc<AtomicBool>>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub(super) fn start() -> Self {
        if !io::stdout().is_terminal() {
            return Self {
                stop: None,
                handle: None,
            };
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let _ = terminal_output::with_stdout(|stdout| execute!(stdout, Hide));
        let handle = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut index = 0;

            while !thread_stop.load(Ordering::Relaxed) {
                let _ = terminal_output::with_stdout(|stdout| {
                    write_spinner_frame(stdout, frames[index % frames.len()])
                });
                index += 1;
                thread::sleep(Duration::from_millis(120));
            }
        });

        Self {
            stop: Some(stop),
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }

        if self.stop.is_some() {
            let _ = terminal_output::with_stdout(|stdout| {
                clear_spinner(stdout)?;
                execute!(stdout, Show)?;
                stdout.flush()
            });
        }
    }
}

fn write_spinner_frame(stdout: &mut impl Write, frame: &str) -> io::Result<()> {
    write!(stdout, "\r{frame}")?;
    stdout.flush()
}

fn clear_spinner(stdout: &mut impl Write) -> io::Result<()> {
    write!(stdout, "\r{:<96}\r", "")?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frame_is_carriage_returned_and_flushed() {
        let mut output = Vec::new();

        write_spinner_frame(&mut output, "x").unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "\rx");
    }

    #[test]
    fn spinner_clear_returns_to_column_zero() {
        let mut output = Vec::new();

        clear_spinner(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with('\r'));
        assert!(output.ends_with('\r'));
        assert_eq!(output.chars().count(), 98);
    }
}
