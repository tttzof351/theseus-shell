mod http;
mod pty;
mod screen;

pub(crate) use http::{held_json_fixture, interrupted_agent_fixture};
pub(crate) use pty::{ApplicationPty, SIZE, WAIT_TIMEOUT, temp_home};
pub(crate) use screen::{
    find_bytes, is_waiting_spinner, output_marker_column, screen_rows, screen_text,
    settled_prompt_is_visible, terminal_history_rows, waiting_spinner_row,
};

use std::{
    io, thread,
    time::{Duration, Instant},
};

pub(crate) fn wait_cli_exit(
    mut child: std::process::Child,
    deadline: Duration,
) -> io::Result<std::process::Output> {
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "CLI failed to stop: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}
