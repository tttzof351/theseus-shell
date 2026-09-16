#[cfg(unix)]
use super::super::platform::NonBlockingFileGuard;
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    io::{IsTerminal, Read},
    time::Duration,
};
use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[cfg(unix)]
fn spawn_input_forwarder(
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    stop: Arc<AtomicBool>,
    input: Option<crate::common::terminal_input::SharedInput>,
) -> io::Result<Option<thread::JoinHandle<()>>> {
    if let Some(input) = input {
        return Ok(Some(thread::spawn(move || {
            let mut buffer = [0; 8192];
            while !stop.load(Ordering::Acquire) {
                let Ok(mut input) = input.lock() else {
                    break;
                };
                let count = match input.read_raw(&mut buffer, Duration::from_millis(5)) {
                    Ok(count) => count,
                    Err(_) => break,
                };
                if stop.load(Ordering::Acquire) {
                    input.return_raw(&buffer[..count]);
                    break;
                }
                if count > 0 {
                    let Ok(mut writer) = writer.lock() else {
                        input.return_raw(&buffer[..count]);
                        break;
                    };
                    if stop.load(Ordering::Acquire) {
                        input.return_raw(&buffer[..count]);
                        break;
                    }
                    if writer
                        .write_all(&buffer[..count])
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })));
    }
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let tty = OpenOptions::new().read(true).open("/dev/tty")?;
    let mut tty = NonBlockingFileGuard::enable(tty)?;

    Ok(Some(thread::spawn(move || {
        let mut buffer = [0; 8192];

        while !stop.load(Ordering::Relaxed) {
            match tty.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let Ok(mut writer) = writer.lock() else {
                        break;
                    };
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
    })))
}

pub(super) struct InputForwarder {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    input: Option<crate::common::terminal_input::SharedInput>,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) thread: Option<thread::JoinHandle<()>>,
}

impl InputForwarder {
    pub(super) fn new(
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        input: Option<crate::common::terminal_input::SharedInput>,
    ) -> Self {
        Self {
            writer,
            input,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub(super) fn start(&mut self) -> io::Result<()> {
        self.thread = spawn_input_forwarder(
            Arc::clone(&self.writer),
            Arc::clone(&self.stop),
            self.input.take(),
        )?;
        Ok(())
    }

    pub(super) fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("shell input forwarder panicked"))?;
        }
        Ok(())
    }
}

impl Drop for InputForwarder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(not(unix))]
fn spawn_input_forwarder(
    _writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _stop: Arc<AtomicBool>,
    _input: Option<crate::common::terminal_input::SharedInput>,
) -> io::Result<Option<thread::JoinHandle<()>>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn input_forwarder_is_stopped_and_joined_during_unwind() {
        let (tx, rx) = mpsc::channel();
        let result = crate::common::panic_boundary::catch(|| {
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = Arc::clone(&stop);
            let worker = thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                tx.send(()).unwrap();
            });
            let _forwarding = InputForwarder {
                writer: Arc::new(Mutex::new(Box::new(io::sink()))),
                input: None,
                stop,
                thread: Some(worker),
            };
            panic!("injected terminal writer panic");
        });
        assert!(result.is_err());
        assert_eq!(rx.try_recv(), Ok(()));
    }
}
