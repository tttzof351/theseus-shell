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
const KEY_UP: &str = "\x1b[A";
const KEY_DOWN: &str = "\x1b[B";
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

#[derive(Debug)]
struct VtScreen {
    rows: usize,
    cols: usize,
    row: usize,
    col: usize,
    wrap_next: bool,
    cells: Vec<Vec<char>>,
}

impl VtScreen {
    fn new(size: PtySize) -> Self {
        let rows = size.rows as usize;
        let cols = size.cols as usize;
        Self {
            rows,
            cols,
            row: 0,
            col: 0,
            wrap_next: false,
            cells: vec![vec![' '; cols]; rows],
        }
    }

    fn parse(size: PtySize, transcript: &str) -> Self {
        let mut screen = Self::new(size);
        let mut chars = transcript.chars().peekable();

        while let Some(ch) = chars.next() {
            match ch {
                '\x1b' if chars.peek() == Some(&'[') => {
                    chars.next();
                    let mut csi = String::new();
                    for next in chars.by_ref() {
                        csi.push(next);
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                    screen.apply_csi(&csi);
                }
                '\r' => {
                    screen.col = 0;
                    screen.wrap_next = false;
                }
                '\n' => {
                    screen.wrap_next = false;
                    screen.line_feed();
                }
                '\x08' => {
                    screen.col = screen.col.saturating_sub(1);
                    screen.wrap_next = false;
                }
                ch if ch.is_control() => {}
                ch => screen.put(ch),
            }
        }

        screen
    }

    fn apply_csi(&mut self, csi: &str) {
        let Some(command) = csi.chars().last() else {
            return;
        };
        let params = &csi[..csi.len().saturating_sub(command.len_utf8())];
        let first_param = || {
            params
                .split(';')
                .find_map(|param| param.parse::<usize>().ok())
                .unwrap_or(1)
        };

        match command {
            'A' => self.row = self.row.saturating_sub(first_param()),
            'B' => self.row = (self.row + first_param()).min(self.rows - 1),
            'C' => self.col = (self.col + first_param()).min(self.cols - 1),
            'G' => self.col = first_param().saturating_sub(1).min(self.cols - 1),
            'H' | 'f' => {
                let mut parts = params.split(';');
                let row = parts
                    .next()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1);
                let col = parts
                    .next()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1);
                self.row = row.saturating_sub(1).min(self.rows - 1);
                self.col = col.saturating_sub(1).min(self.cols - 1);
            }
            'K' => self.clear_line(params),
            'm' | '?' | 'h' | 'l' => {}
            _ => {}
        }
        if !matches!(command, 'm' | '?' | 'h' | 'l') {
            self.wrap_next = false;
        }
    }

    fn clear_line(&mut self, params: &str) {
        match params.parse::<usize>().unwrap_or(0) {
            1 => {
                for col in 0..=self.col {
                    self.cells[self.row][col] = ' ';
                }
            }
            2 => self.cells[self.row].fill(' '),
            _ => {
                for col in self.col..self.cols {
                    self.cells[self.row][col] = ' ';
                }
            }
        }
    }

    fn put(&mut self, ch: char) {
        if self.wrap_next {
            self.col = 0;
            self.line_feed();
            self.wrap_next = false;
        }
        self.cells[self.row][self.col] = ch;
        if self.col + 1 >= self.cols {
            self.wrap_next = true;
        } else {
            self.col += 1;
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 >= self.rows {
            self.cells.remove(0);
            self.cells.push(vec![' '; self.cols]);
        } else {
            self.row += 1;
        }
    }

    fn text(&self) -> String {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
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

fn compact_pty_size() -> PtySize {
    PtySize {
        rows: 16,
        cols: 60,
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
    let transcript =
        shell.wait_for_after_with_timeout(offset, "\r\n", Duration::from_millis(200))?;

    assert!(
        transcript[offset..].contains("\r\n"),
        "command line was not finished immediately after Enter:\n{}",
        &transcript[offset..]
    );

    shell.wait_for_after(offset, "theseus-shell")?;
    shell.exit()
}

#[test]
fn history_up_replays_latest_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo HISTORY_UP_LATEST\r")?;
    shell.wait_for("HISTORY_UP_LATEST")?;
    shell.wait_for_after(shell.transcript_len(), "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.write("\r")?;
    let transcript =
        shell.wait_until_after(offset, |tail| count_matches(tail, "HISTORY_UP_LATEST") >= 2)?;

    assert!(
        count_matches(&transcript[offset..], "HISTORY_UP_LATEST") >= 2,
        "Up did not recall and execute the latest history entry:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn history_up_down_walks_between_entries() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo HISTORY_WALK_OLDER\r")?;
    shell.wait_for("HISTORY_WALK_OLDER")?;
    shell.wait_for_after(shell.transcript_len(), "theseus-shell")?;

    shell.write("echo HISTORY_WALK_NEWER\r")?;
    shell.wait_for("HISTORY_WALK_NEWER")?;
    shell.wait_for_after(shell.transcript_len(), "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.write(KEY_UP)?;
    shell.write(KEY_DOWN)?;
    shell.write("\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        count_matches(tail, "HISTORY_WALK_NEWER") >= 2
    })?;

    assert!(
        count_matches(&transcript[offset..], "HISTORY_WALK_NEWER") >= 2,
        "Up/Up/Down did not select and execute the newer history entry:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn history_down_after_latest_restores_current_draft() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo HISTORY_FOR_DRAFT\r")?;
    shell.wait_for("HISTORY_FOR_DRAFT")?;
    shell.wait_for_after(shell.transcript_len(), "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write("echo HISTORY_DRAFT_RESTORED")?;
    shell.write(KEY_UP)?;
    shell.write(KEY_DOWN)?;
    shell.write("\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        count_matches(tail, "HISTORY_DRAFT_RESTORED") >= 2
    })?;

    assert!(
        count_matches(&transcript[offset..], "HISTORY_DRAFT_RESTORED") >= 2,
        "Down after the latest history entry did not restore and execute the draft:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn ask_multiline_history_up_recalls_previous_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("/ask\r")?;
    shell.wait_for("Enter multiline input")?;
    let submit_offset = shell.transcript_len();
    shell.write("ASK_HISTORY_LINE_ONE\rASK_HISTORY_LINE_TWO\r/end\r")?;
    shell.wait_for_after(submit_offset, "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write("/ask\r")?;
    shell.wait_for_after(offset, "Enter multiline input")?;
    shell.write(KEY_UP)?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("ASK_HISTORY_LINE_ONE") && tail.contains("ASK_HISTORY_LINE_TWO")
    })?;

    assert!(
        transcript[offset..].contains("ASK_HISTORY_LINE_ONE")
            && transcript[offset..].contains("ASK_HISTORY_LINE_TWO"),
        "Up did not recall the previous multiline /ask prompt:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Ask cancelled")?;
    shell.exit()
}

#[test]
fn ask_multiline_history_preserves_cancelled_draft() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("/ask\r")?;
    shell.wait_for("Enter multiline input")?;
    shell.write("ASK_CANCELLED_DRAFT_ONE\rASK_CANCELLED_DRAFT_TWO")?;
    shell.wait_for("ASK_CANCELLED_DRAFT_TWO")?;
    let cancel_offset = shell.transcript_len();
    shell.write("\u{3}")?;
    shell.wait_for_after(cancel_offset, "Ask cancelled")?;
    shell.wait_for_after(cancel_offset, "theseus-shell")?;
    let saved = fs::read_to_string(
        shell
            .home
            .join(".theseus")
            .join("persist")
            .join("history_ask.json"),
    )?;
    assert!(
        saved.contains("ASK_CANCELLED_DRAFT_ONE") && saved.contains("ASK_CANCELLED_DRAFT_TWO"),
        "cancelled /ask draft was not persisted:\n{saved}"
    );

    let offset = shell.transcript_len();
    shell.write("/ask\r")?;
    shell.wait_for_after(offset, "Enter multiline input")?;
    shell.write(KEY_UP)?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("ASK_CANCELLED_DRAFT_ONE") && tail.contains("ASK_CANCELLED_DRAFT_TWO")
    })?;

    assert!(
        transcript[offset..].contains("ASK_CANCELLED_DRAFT_ONE")
            && transcript[offset..].contains("ASK_CANCELLED_DRAFT_TWO"),
        "Up did not recall the cancelled multiline /ask draft:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Ask cancelled")?;
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
fn wrapped_shell_input_does_not_erase_previous_output_row() -> io::Result<()> {
    let _lock = pty_test_lock();
    let size = compact_pty_size();
    let mut shell = PtyShell::start_with_size(size)?;

    let initial_screen = VtScreen::parse(size, &shell.transcript_string()).text();
    assert!(
        initial_screen.contains("╰"),
        "test setup did not render the welcome box bottom border:\n{initial_screen}"
    );

    let offset = shell.transcript_len();
    let long_input = format!("/ask {}", "x".repeat(160));
    shell.write(&long_input)?;
    let wrapped_prefix = "x".repeat(130);
    shell.wait_until_after(offset, |tail| tail.contains(&wrapped_prefix))?;

    let screen = VtScreen::parse(size, &shell.transcript_string()).text();
    assert!(
        screen.contains("╰"),
        "wrapped input erased the welcome box bottom border:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for("Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn shell_input_cursor_moves_after_inserted_character() -> io::Result<()> {
    let _lock = pty_test_lock();
    let size = PtySize {
        rows: 24,
        cols: 100,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut shell = PtyShell::start_with_size(size)?;

    let offset = shell.transcript_len();
    shell.write("a")?;
    shell.wait_for_after(offset, "> a")?;

    let screen = VtScreen::parse(size, &shell.transcript_string());
    let text = screen.text();
    let line_index = text
        .lines()
        .position(|line| line.contains("> a"))
        .ok_or_else(|| io::Error::other(format!("prompt line was not rendered:\n{text}")))?;
    let prompt_line = text
        .lines()
        .nth(line_index)
        .ok_or_else(|| io::Error::other(format!("prompt line disappeared:\n{text}")))?;
    let expected_col = prompt_line
        .find("> a")
        .map(|index| index + "> a".chars().count())
        .ok_or_else(|| io::Error::other(format!("prompt line did not contain input:\n{text}")))?;

    assert_eq!(
        (screen.row, screen.col),
        (line_index, expected_col),
        "cursor should be after the inserted character:\n{text}"
    );

    shell.write("\u{3}")?;
    shell.wait_for("Interrupted. Type /exit to exit the shell.")?;
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
