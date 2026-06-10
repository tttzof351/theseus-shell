use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_millis(500);
static PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

struct PtyShell {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    transcript: Arc<Mutex<Vec<u8>>>,
    home: PathBuf,
}

impl PtyShell {
    fn start() -> io::Result<Self> {
        Self::start_with_size(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn start_with_size(size: PtySize) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size)
            .map_err(|err| io::Error::other(err.to_string()))?;

        let home = temp_home()?;
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_theseus"));
        command.cwd(env!("CARGO_MANIFEST_DIR"));
        command.env("HOME", &home);
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|err| io::Error::other(err.to_string()))?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let transcript = Arc::new(Mutex::new(Vec::new()));
        spawn_reader(reader, Arc::clone(&transcript));

        let shell = Self {
            child,
            master: pair.master,
            writer,
            transcript,
            home,
        };
        shell.wait_for("theseus-shell")?;
        Ok(shell)
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()
    }

    fn wait_for(&self, needle: &str) -> io::Result<String> {
        self.wait_for_after(0, needle)
    }

    fn wait_for_after(&self, offset: usize, needle: &str) -> io::Result<String> {
        self.wait_for_after_with_timeout(offset, needle, WAIT_TIMEOUT)
    }

    fn wait_for_after_with_timeout(
        &self,
        offset: usize,
        needle: &str,
        timeout: Duration,
    ) -> io::Result<String> {
        self.wait_until_after_with_timeout(offset, timeout, |text| text.contains(needle))
    }

    fn wait_until_after<F>(&self, offset: usize, predicate: F) -> io::Result<String>
    where
        F: Fn(&str) -> bool,
    {
        self.wait_until_after_with_timeout(offset, WAIT_TIMEOUT, predicate)
    }

    fn wait_until_after_with_timeout<F>(
        &self,
        offset: usize,
        timeout: Duration,
        predicate: F,
    ) -> io::Result<String>
    where
        F: Fn(&str) -> bool,
    {
        let start = Instant::now();
        loop {
            let text = self.transcript_string();
            let tail = text.get(offset..).unwrap_or("");
            if predicate(tail) {
                return Ok(text);
            }
            if start.elapsed() > timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for terminal output; tail was:\n{tail}"),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn transcript_len(&self) -> usize {
        self.transcript.lock().unwrap().len()
    }

    fn transcript_string(&self) -> String {
        String::from_utf8_lossy(&self.transcript.lock().unwrap()).into_owned()
    }

    fn exit(mut self) -> io::Result<()> {
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
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = fs::remove_dir_all(&self.home);
        let _ = self.master.resize(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, transcript: Arc<Mutex<Vec<u8>>>) {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => transcript.lock().unwrap().extend_from_slice(&buffer[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
}

fn temp_home() -> io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "theseus-line-editor-pty-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn count_matches(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn pty_test_lock() -> MutexGuard<'static, ()> {
    PTY_TEST_LOCK.lock().unwrap()
}

fn narrow_pty_size() -> PtySize {
    PtySize {
        rows: 24,
        cols: 34,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[test]
fn backslash_enter_shows_continuation_prompt_and_executes_joined_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo \\\r")?;
    shell.wait_for("\r\n> ")?;

    let offset = shell.transcript_len();
    shell.write(" THESEUS_JOINED_OK\r")?;
    let transcript =
        shell.wait_until_after(offset, |tail| count_matches(tail, "THESEUS_JOINED_OK") >= 2)?;

    assert!(
        transcript.contains("\r\n> "),
        "continuation prompt was not rendered on a new terminal row:\n{transcript}"
    );
    assert!(
        count_matches(&transcript[offset..], "THESEUS_JOINED_OK") >= 2,
        "joined command output was not observed:\n{transcript}"
    );

    shell.exit()
}

#[test]
fn pasted_multiline_command_executes_as_one_shell_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("echo \\\r THESEUS_PASTE_OK\r")?;
    let transcript =
        shell.wait_until_after(offset, |tail| count_matches(tail, "THESEUS_PASTE_OK") >= 2)?;

    assert!(
        transcript[offset..].contains("\r\n> "),
        "pasted multiline command did not render continuation prompt:\n{}",
        &transcript[offset..]
    );
    assert!(
        count_matches(&transcript[offset..], "THESEUS_PASTE_OK") >= 2,
        "pasted multiline command did not execute as one joined command:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn long_running_command_moves_to_next_line_immediately_after_enter() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("sleep 2\r")?;
    let transcript = shell.wait_for_after_with_timeout(offset, "\r\n", Duration::from_millis(200))?;

    assert!(
        transcript[offset..].contains("\r\n"),
        "command line was not finished immediately after Enter:\n{}",
        &transcript[offset..]
    );

    shell.wait_for_after(offset, "theseus-shell")?;
    shell.exit()
}

#[test]
fn unmatched_quote_after_continuation_returns_to_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo \\\r \"test\r")?;
    shell.wait_for("unmatched")?;
    let prompt_offset = shell.transcript_len();
    shell.wait_for_after(prompt_offset, "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write("echo AFTER_UNMATCHED_QUOTE_OK\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        count_matches(tail, "AFTER_UNMATCHED_QUOTE_OK") >= 2
    })?;

    assert!(
        count_matches(&transcript[offset..], "AFTER_UNMATCHED_QUOTE_OK") >= 2,
        "shell did not recover after unmatched quote:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn wrapped_first_line_keeps_continuation_prompt_on_next_logical_line() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start_with_size(narrow_pty_size())?;

    let offset = shell.transcript_len();
    shell.write("echo WRAP_MARKER_1234567890 \\\r")?;
    let transcript = shell.wait_for_after(offset, "\r\n> ")?;

    assert!(
        transcript[offset..].contains("WRAP_MARKER_1234567890"),
        "long first line was not rendered:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains("\r\n> "),
        "continuation prompt was not rendered after wrapped first line:\n{}",
        &transcript[offset..]
    );

    let output_offset = shell.transcript_len();
    shell.write(" WRAPPED_CONTINUATION_OK\r")?;
    shell.wait_until_after(output_offset, |tail| {
        count_matches(tail, "WRAP_MARKER_1234567890") >= 1
            && count_matches(tail, "WRAPPED_CONTINUATION_OK") >= 2
    })?;

    shell.exit()
}

#[test]
fn ctrl_c_during_continuation_returns_to_clean_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo \\\rpartial")?;
    shell.wait_for("> partial")?;

    shell.write("\u{3}")?;
    shell.wait_for("Interrupted. Type /exit to exit the shell.")?;

    let offset = shell.transcript_len();
    shell.write("echo AFTER_CTRL_C_OK\r")?;
    let transcript =
        shell.wait_until_after(offset, |tail| count_matches(tail, "AFTER_CTRL_C_OK") >= 2)?;

    assert!(
        !transcript[offset..].contains("partial"),
        "partial continuation input leaked after Ctrl+C:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn ctrl_l_during_continuation_rerenders_full_multiline_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo \\\r CTRL_L_OK")?;
    shell.wait_for(">  CTRL_L_OK")?;

    let offset = shell.transcript_len();
    shell.write("\u{c}")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("echo \\") && tail.contains(">  CTRL_L_OK")
    })?;

    assert!(
        transcript[offset..].contains("echo \\"),
        "first line was not re-rendered after Ctrl+L:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains(">  CTRL_L_OK"),
        "continuation line was not re-rendered after Ctrl+L:\n{}",
        &transcript[offset..]
    );

    shell.write("\r")?;
    shell.wait_until_after(offset, |tail| count_matches(tail, "CTRL_L_OK") >= 2)?;
    shell.exit()
}
