use std::{
    io::{self, Write},
    sync::{Mutex, MutexGuard, OnceLock},
};

static STDOUT_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

#[doc(hidden)]
pub struct StdoutGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    stdout: io::Stdout,
}

#[doc(hidden)]
pub fn stdout() -> StdoutGuard<'static> {
    let guard = STDOUT_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    StdoutGuard {
        _guard: guard,
        stdout: io::stdout(),
    }
}

#[doc(hidden)]
pub fn with_stdout<T>(f: impl FnOnce(&mut StdoutGuard<'_>) -> io::Result<T>) -> io::Result<T> {
    let mut stdout = stdout();
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
        self.stdout.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }
}
