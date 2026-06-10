use std::{io, io::IsTerminal, path::Path};

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
pub(super) fn shell_command_args(shell: &Path, command: &str) -> Vec<String> {
    let command_flag = if loads_interactive_startup_files(shell) {
        "-ic"
    } else {
        "-c"
    };

    vec![command_flag.to_string(), command.to_string()]
}

#[cfg(windows)]
pub(super) fn shell_command_args(_shell: &Path, command: &str) -> Vec<String> {
    vec!["/C".to_string(), command.to_string()]
}

#[cfg(unix)]
fn loads_interactive_startup_files(shell: &Path) -> bool {
    let shell_name = shell
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    matches!(shell_name, "bash" | "zsh")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(unix)]
    #[test]
    fn zsh_commands_run_as_interactive_shell_commands() {
        assert_eq!(
            shell_command_args(Path::new("/bin/zsh"), "ll"),
            vec!["-ic".to_string(), "ll".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_commands_run_as_interactive_shell_commands() {
        assert_eq!(
            shell_command_args(Path::new("/bin/bash"), "ll"),
            vec!["-ic".to_string(), "ll".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn plain_sh_commands_stay_non_interactive() {
        assert_eq!(
            shell_command_args(Path::new("/bin/sh"), "ll"),
            vec!["-c".to_string(), "ll".to_string()]
        );
    }
}
