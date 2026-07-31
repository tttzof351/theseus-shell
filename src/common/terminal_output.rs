use std::{
    io::{self, Write},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

type CaptureBuffer = Arc<Mutex<Vec<u8>>>;

static STDOUT_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
static STDOUT_CAPTURE: OnceLock<Mutex<Option<CaptureBuffer>>> = OnceLock::new();

#[doc(hidden)]
pub struct StdoutGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    stdout: io::Stdout,
    capture: Option<CaptureBuffer>,
}

pub(crate) struct StdoutCapture {
    bytes: CaptureBuffer,
    active: bool,
}

pub(crate) fn begin_stdout_capture() -> io::Result<StdoutCapture> {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut capture = STDOUT_CAPTURE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| io::Error::other("stdout capture lock poisoned"))?;
    if capture.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "stdout capture is already active",
        ));
    }
    *capture = Some(Arc::clone(&bytes));
    Ok(StdoutCapture {
        bytes,
        active: true,
    })
}

impl StdoutCapture {
    pub(crate) fn finish(mut self) -> io::Result<Vec<u8>> {
        self.deactivate()?;
        self.bytes
            .lock()
            .map(|bytes| bytes.clone())
            .map_err(|_| io::Error::other("stdout capture buffer lock poisoned"))
    }

    fn deactivate(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut capture = STDOUT_CAPTURE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| io::Error::other("stdout capture lock poisoned"))?;
        if capture
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.bytes))
        {
            *capture = None;
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for StdoutCapture {
    fn drop(&mut self) {
        let _ = self.deactivate();
    }
}

#[doc(hidden)]
pub fn stdout() -> StdoutGuard<'static> {
    stdout_guard(true)
}

fn stdout_guard(record: bool) -> StdoutGuard<'static> {
    let guard = STDOUT_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let capture = record.then(|| {
        STDOUT_CAPTURE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    });
    StdoutGuard {
        _guard: guard,
        stdout: io::stdout(),
        capture: capture.flatten(),
    }
}

#[doc(hidden)]
pub fn with_stdout<T>(f: impl FnOnce(&mut StdoutGuard<'_>) -> io::Result<T>) -> io::Result<T> {
    let mut stdout = stdout();
    f(&mut stdout)
}

pub(crate) fn with_transient_stdout<T>(
    f: impl FnOnce(&mut StdoutGuard<'_>) -> io::Result<T>,
) -> io::Result<T> {
    let mut stdout = stdout_guard(false);
    f(&mut stdout)
}

#[doc(hidden)]
pub fn with_locked_writer<T, W: Write>(
    lock: &Mutex<()>,
    writer: &Mutex<W>,
    f: impl FnOnce(&mut W) -> io::Result<T>,
) -> io::Result<T> {
    let _guard = lock.lock().unwrap_or_else(|err| err.into_inner());
    let mut writer = writer.lock().unwrap_or_else(|err| err.into_inner());
    f(&mut writer)
}

impl Write for StdoutGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.stdout.write(buf)?;
        if let Some(capture) = &self.capture {
            capture
                .lock()
                .map_err(|_| io::Error::other("stdout capture buffer lock poisoned"))?
                .extend_from_slice(&buf[..written]);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }
}
