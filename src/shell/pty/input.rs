use std::{
    io::{self, Write},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::{
    fs::OpenOptions,
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(not(unix))]
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
#[cfg(not(unix))]
use portable_pty::PtySize;

use crate::common::cancellation::CancellationEvent;

#[cfg(unix)]
use super::platform::{NonBlockingFileGuard, current_pty_size};

#[cfg(unix)]
pub(super) fn forward_terminal_input_until_exit(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: &(dyn portable_pty::MasterPty + Send),
    cancellation: Option<CancellationEvent>,
) -> io::Result<Option<i32>> {
    let stop_input = Arc::new(AtomicBool::new(false));
    let input_thread = spawn_input_thread(writer, Arc::clone(&stop_input), cancellation)?;
    let mut last_size = current_pty_size();

    loop {
        if let Some(status) = child.try_wait()? {
            stop_input.store(true, Ordering::Relaxed);
            let _ = input_thread.join();
            return Ok(Some(status.exit_code() as i32));
        }

        let size = current_pty_size();
        if size != last_size {
            master
                .resize(size)
                .map_err(|err| io::Error::other(err.to_string()))?;
            last_size = size;
        }

        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn spawn_input_thread(
    mut writer: Box<dyn Write + Send>,
    stop: Arc<AtomicBool>,
    cancellation: Option<CancellationEvent>,
) -> io::Result<thread::JoinHandle<()>> {
    let tty = OpenOptions::new().read(true).open("/dev/tty")?;
    let mut tty = NonBlockingFileGuard::enable(tty)?;

    Ok(thread::spawn(move || {
        let mut buffer = [0; 8192];

        while !stop.load(Ordering::Relaxed) {
            match tty.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if buffer[..n].contains(&3)
                        && let Some(cancellation) = &cancellation
                    {
                        cancellation.cancel();
                    }
                    if writer.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    }))
}

#[cfg(not(unix))]
pub(super) fn forward_terminal_input_until_exit(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    mut writer: Box<dyn Write + Send>,
    master: &(dyn portable_pty::MasterPty + Send),
    cancellation: Option<CancellationEvent>,
) -> io::Result<Option<i32>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status.exit_code() as i32));
        }

        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
                Event::Key(key) => {
                    if matches!(key.code, KeyCode::Char('c'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && let Some(cancellation) = &cancellation
                    {
                        cancellation.cancel();
                    }
                    if let Some(bytes) = key_to_bytes(key) {
                        writer.write_all(&bytes)?;
                        writer.flush()?;
                    }
                }
                Event::Paste(text) => {
                    writer.write_all(text.as_bytes())?;
                    writer.flush()?;
                }
                Event::Resize(cols, rows) => {
                    master
                        .resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .map_err(|err| io::Error::other(err.to_string()))?;
                }
                _ => {}
            }
        }
    }
}

#[cfg(not(unix))]
fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            control_char(ch).map(|byte| vec![byte])
        }
        KeyCode::Char(ch) => Some(ch.to_string().into_bytes()),
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

#[cfg(not(unix))]
fn control_char(ch: char) -> Option<u8> {
    let upper = ch.to_ascii_uppercase();
    if upper.is_ascii_alphabetic() {
        Some((upper as u8) - b'A' + 1)
    } else if ch == '[' {
        Some(0x1b)
    } else {
        None
    }
}

#[cfg(all(test, not(unix)))]
mod tests {
    use super::*;

    #[test]
    fn maps_basic_keys_to_terminal_bytes() {
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(b"a".to_vec())
        );
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(b"\r".to_vec())
        );
    }
}
