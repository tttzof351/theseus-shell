//! Real application child process and raw PTY transcript.
use super::screen::settled_prompt_is_visible;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

const EXIT_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) const SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

static TEMP_HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ApplicationPty {
    pub(crate) child: Box<dyn Child + Send + Sync>,
    pub(crate) master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    transcript: Arc<Mutex<Vec<u8>>>,
    pub(crate) home: PathBuf,
}
impl ApplicationPty {
    pub(crate) fn start() -> io::Result<Self> {
        let home = temp_home()?;
        Self::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))
    }

    pub(crate) fn start_with_home_and_cwd(home: PathBuf, cwd: &Path) -> io::Result<Self> {
        Self::start_with_shell(home, cwd, None)
    }

    pub(crate) fn start_with_shell(
        home: PathBuf,
        cwd: &Path,
        shell: Option<&Path>,
    ) -> io::Result<Self> {
        let pair = native_pty_system()
            .openpty(SIZE)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_theseus"));
        command.cwd(cwd);
        command.env("HOME", &home);
        command.env("USER", "tester");
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        if let Some(shell) = shell {
            command.env("SHELL", shell);
        }
        let git_pager = home.join("git-branch-pager.sh");
        command.env(
            "GIT_PAGER",
            if git_pager.exists() {
                git_pager.as_os_str()
            } else {
                std::ffi::OsStr::new("cat")
            },
        );
        command.env("PAGER", "cat");

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| io::Error::other(error.to_string()))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let reader_transcript = Arc::clone(&transcript);
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => reader_transcript
                        .lock()
                        .unwrap()
                        .extend_from_slice(&buffer[..length]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

        let shell = Self {
            child,
            master: pair.master,
            writer,
            transcript,
            home,
        };
        shell.wait_until(settled_prompt_is_visible)?;
        Ok(shell)
    }

    pub(crate) fn write(&mut self, text: &str) -> io::Result<()> {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()
    }

    pub(crate) fn transcript_len(&self) -> usize {
        self.transcript.lock().unwrap().len()
    }

    pub(crate) fn transcript(&self) -> Vec<u8> {
        self.transcript.lock().unwrap().clone()
    }

    pub(crate) fn wait_until(&self, predicate: impl Fn(&[u8]) -> bool) -> io::Result<Vec<u8>> {
        let start = Instant::now();
        loop {
            let transcript = self.transcript();
            if predicate(&transcript) {
                return Ok(transcript);
            }
            if start.elapsed() > WAIT_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for theseus terminal output; tail was:\n{:?}",
                        String::from_utf8_lossy(
                            transcript
                                .get(transcript.len().saturating_sub(2_000)..)
                                .unwrap_or(&[])
                        )
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn exit(mut self) -> io::Result<()> {
        self.write("/exit\r")?;
        let start = Instant::now();
        loop {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            if start.elapsed() > EXIT_TIMEOUT {
                let _ = self.child.kill();
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ApplicationPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = fs::remove_dir_all(&self.home);
        let _ = self.master.resize(SIZE);
    }
}

pub(crate) fn temp_home() -> io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = TEMP_HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "theseus-application-pty-{}-{nanos}-{sequence}",
        std::process::id(),
    ));
    fs::create_dir_all(&home)?;
    Ok(home)
}
