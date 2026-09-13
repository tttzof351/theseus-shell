use std::time::Duration;

/// Shared animation for managed rendering and the legacy terminal spinner.
pub(crate) fn spinner_frame(elapsed: Duration) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[((elapsed.as_millis() / 120) % FRAMES.len() as u128) as usize]
}
