use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use theseus::input::strip_ansi_codes;

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

struct VtScreen {
    parser: vt100::Parser,
    cols: u16,
    row: usize,
    col: usize,
}

impl VtScreen {
    fn parse(size: PtySize, transcript: &str) -> Self {
        let mut parser = vt100::Parser::new(size.rows, size.cols, 0);
        parser.process(transcript.as_bytes());
        let (row, col) = parser.screen().cursor_position();
        Self {
            parser,
            cols: size.cols,
            row: row.into(),
            col: col.into(),
        }
    }

    fn text(&self) -> String {
        self.parser
            .screen()
            .rows(0, self.cols)
            .map(|row| row.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn cursor_cell_is_wide_continuation(&self) -> bool {
        self.parser
            .screen()
            .cell(self.row as u16, self.col as u16)
            .is_some_and(vt100::Cell::is_wide_continuation)
    }
}

fn test_text_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
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
fn pasted_heredoc_command_executes_after_terminator() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let command = concat!(
        "bash <<'REMOTE'\r",
        "set -euo pipefail\r",
        "cd /tmp\r",
        "echo THESEUS_HEREDOC_RUNNING\r",
        "echo THESEUS_HEREDOC_USER: $(whoami)\r",
        "ls -la /tmp | head -5\r",
        "REMOTE\r",
    );

    let offset = shell.transcript_len();
    shell.write(command)?;
    let transcript =
        shell.wait_until_after(offset, |tail| tail.contains("THESEUS_HEREDOC_RUNNING"))?;

    assert!(
        transcript[offset..].contains("THESEUS_HEREDOC_USER:"),
        "pasted heredoc did not execute the complete command:\n{}",
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
fn ctrl_c_during_quote_continuation_returns_to_clean_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo \\\r \"test\r")?;
    shell.wait_for("\r\n> ")?;

    let prompt_offset = shell.transcript_len();
    shell.write("\u{3}")?;
    shell.wait_for_after(prompt_offset, "Interrupted. Type /exit to exit the shell.")?;
    shell.wait_for_after(prompt_offset, "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write("echo AFTER_QUOTE_CANCEL_OK\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        count_matches(tail, "AFTER_QUOTE_CANCEL_OK") >= 2
    })?;

    assert!(
        count_matches(&transcript[offset..], "AFTER_QUOTE_CANCEL_OK") >= 2,
        "shell did not recover after cancelling quote continuation:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn single_quote_continuation_executes_after_closing_quote() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("printf '%s\\n' 'THESEUS_SINGLE_QUOTE")?;
    shell.write("\r")?;
    shell.wait_for("\r\n> ")?;

    let offset = shell.transcript_len();
    shell.write("CONTINUATION_OK'\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("THESEUS_SINGLE_QUOTE\nCONTINUATION_OK")
            || tail.contains("THESEUS_SINGLE_QUOTE\r\nCONTINUATION_OK")
    })?;

    assert!(
        transcript[offset..].contains("CONTINUATION_OK"),
        "single quote continuation did not execute after closing quote:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn command_substitution_continuation_executes_after_closing_paren() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo \"$(")?;
    shell.write("\r")?;
    shell.wait_for("\r\n> ")?;

    let offset = shell.transcript_len();
    shell.write("printf THESEUS_CMD_SUB_OK)\"\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        count_matches(tail, "THESEUS_CMD_SUB_OK") >= 2
    })?;

    assert!(
        count_matches(&transcript[offset..], "THESEUS_CMD_SUB_OK") >= 2,
        "command substitution continuation did not execute after closing paren:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn if_block_continuation_executes_after_fi() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("if true; then\r")?;
    shell.wait_for("\r\n> ")?;
    shell.write("echo THESEUS_IF_BLOCK_OK\r")?;
    shell.wait_for("\r\n> ")?;

    let offset = shell.transcript_len();
    shell.write("fi\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        count_matches(tail, "THESEUS_IF_BLOCK_OK") >= 1
    })?;

    assert!(
        transcript[offset..].contains("THESEUS_IF_BLOCK_OK"),
        "if block continuation did not execute after fi:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn pasted_if_block_executes_as_one_shell_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("if true; then\recho THESEUS_PASTED_IF_OK\rfi\r")?;
    let transcript =
        shell.wait_until_after(offset, |tail| tail.contains("THESEUS_PASTED_IF_OK"))?;

    assert!(
        !transcript[offset..].contains("parse error")
            && !transcript[offset..].contains("syntax error"),
        "pasted if block produced a syntax error:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains("THESEUS_PASTED_IF_OK"),
        "pasted if block did not execute:\n{}",
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
fn long_emoji_continuation_input_does_not_panic() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start_with_size(narrow_pty_size())?;

    shell.write("echo \\\r")?;
    shell.wait_for("\r\n> ")?;

    let offset = shell.transcript_len();
    shell.write(&"🤿".repeat(80))?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("panicked at") || tail.contains(&"🤿".repeat(20))
    })?;

    assert!(
        !transcript[offset..].contains("panicked at"),
        "long emoji continuation input panicked:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for("Interrupted. Type /exit to exit the shell.")?;
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
    shell.wait_until_after(offset, |tail| strip_ansi_codes(tail).contains("> a"))?;

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
fn shell_input_cursor_moves_after_wide_character() -> io::Result<()> {
    let _lock = pty_test_lock();
    let size = PtySize {
        rows: 24,
        cols: 100,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut shell = PtyShell::start_with_size(size)?;

    let offset = shell.transcript_len();
    shell.write("界")?;
    shell.wait_for_after(offset, "界")?;

    let screen = VtScreen::parse(size, &shell.transcript_string());
    let text = screen.text();
    let line_index = text
        .lines()
        .position(|line| line.contains("> 界"))
        .ok_or_else(|| io::Error::other(format!("prompt line was not rendered:\n{text}")))?;
    let prompt_line = text
        .lines()
        .nth(line_index)
        .ok_or_else(|| io::Error::other(format!("prompt line disappeared:\n{text}")))?;
    let marker_index = prompt_line
        .find("> 界")
        .ok_or_else(|| io::Error::other(format!("prompt line did not contain input:\n{text}")))?;
    let expected_col = test_text_width(&prompt_line[..marker_index]) + test_text_width("> 界");

    assert_eq!(
        (screen.row, screen.col),
        (line_index, expected_col),
        "cursor should be after the wide inserted character:\n{text}"
    );
    assert!(
        !screen.cursor_cell_is_wide_continuation(),
        "cursor should not be on the trailing cell of a wide character:\n{text}"
    );

    shell.write("\u{3}")?;
    shell.wait_for("Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn wrapped_emoji_shell_input_cursor_stays_after_last_emoji() -> io::Result<()> {
    let _lock = pty_test_lock();
    let size = narrow_pty_size();
    let mut shell = PtyShell::start_with_size(size)?;

    let emoji_input = "🤿".repeat(20);
    let offset = shell.transcript_len();
    shell.write(&emoji_input)?;
    let rendered_suffix = "🤿".repeat(12);
    shell.wait_until_after(offset, |tail| tail.contains(&rendered_suffix))?;

    let screen = VtScreen::parse(size, &shell.transcript_string());
    let text = screen.text();
    let prompt_line_index = text
        .lines()
        .position(|line| line.contains("> 🤿"))
        .ok_or_else(|| io::Error::other(format!("prompt line was not rendered:\n{text}")))?;
    let prompt_line = text
        .lines()
        .nth(prompt_line_index)
        .ok_or_else(|| io::Error::other(format!("prompt line disappeared:\n{text}")))?;
    let input_start = prompt_line
        .find("🤿")
        .ok_or_else(|| io::Error::other(format!("prompt line did not contain emoji:\n{text}")))?;
    let prompt_width = test_text_width(&prompt_line[..input_start]);
    let total_width = prompt_width + test_text_width(&emoji_input);
    let expected_row = prompt_line_index + (total_width / size.cols as usize);
    let expected_col = total_width % size.cols as usize;

    assert_eq!(
        (screen.row, screen.col),
        (expected_row, expected_col),
        "cursor should be after the final wrapped emoji:\n{text}"
    );
    assert!(
        !screen.cursor_cell_is_wide_continuation(),
        "cursor should not be on the trailing cell of a wide emoji:\n{text}"
    );

    shell.write("\u{3}")?;
    shell.wait_for("Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn wrapped_emoji_shell_input_cursor_moves_to_next_row_after_terminal_edge() -> io::Result<()> {
    let _lock = pty_test_lock();
    let size = narrow_pty_size();
    let mut shell = PtyShell::start_with_size(size)?;

    let initial_screen = VtScreen::parse(size, &shell.transcript_string());
    let initial_text = initial_screen.text();
    let prompt_line = initial_text
        .lines()
        .find(|line| line.contains("theseus-shell>"))
        .ok_or_else(|| {
            io::Error::other(format!("prompt line was not rendered:\n{initial_text}"))
        })?;
    let prompt_width = test_text_width(prompt_line) + 1;
    let remaining_cols = size.cols as usize - prompt_width;
    assert_eq!(
        remaining_cols % test_text_width("🤿"),
        0,
        "test setup should leave an even number of columns for wide emoji:\n{initial_text}"
    );
    let emoji_input = "🤿".repeat(remaining_cols / test_text_width("🤿"));
    let offset = shell.transcript_len();
    shell.write(&emoji_input)?;
    shell.wait_for_after(offset, &emoji_input)?;

    let screen = VtScreen::parse(size, &shell.transcript_string());
    let text = screen.text();
    let prompt_line_index = text
        .lines()
        .position(|line| line.contains("> 🤿"))
        .ok_or_else(|| io::Error::other(format!("prompt line was not rendered:\n{text}")))?;
    let prompt_line = text
        .lines()
        .nth(prompt_line_index)
        .ok_or_else(|| io::Error::other(format!("prompt line disappeared:\n{text}")))?;
    let input_start = prompt_line
        .find("🤿")
        .ok_or_else(|| io::Error::other(format!("prompt line did not contain emoji:\n{text}")))?;
    let prompt_width = test_text_width(&prompt_line[..input_start]);
    let total_width = prompt_width + test_text_width(&emoji_input);
    assert_eq!(
        total_width, size.cols as usize,
        "test setup should end exactly at the terminal edge:\n{text}"
    );

    assert_eq!(
        (screen.row, screen.col),
        (prompt_line_index + 1, 0),
        "cursor should move after the final emoji at the terminal edge:\n{text}"
    );
    assert!(
        !screen.cursor_cell_is_wide_continuation(),
        "cursor should not be on the trailing cell of the final emoji:\n{text}"
    );

    shell.write("\u{3}")?;
    shell.wait_for("Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn wrapped_wide_shell_input_does_not_erase_previous_output_row() -> io::Result<()> {
    let _lock = pty_test_lock();
    let size = compact_pty_size();
    let mut shell = PtyShell::start_with_size(size)?;

    let initial_screen = VtScreen::parse(size, &shell.transcript_string()).text();
    assert!(
        initial_screen.contains("╰"),
        "test setup did not render the welcome box bottom border:\n{initial_screen}"
    );

    let offset = shell.transcript_len();
    let long_input = format!("/ask {}", "界".repeat(30));
    shell.write(&long_input)?;
    let wrapped_prefix = "界".repeat(20);
    shell.wait_until_after(offset, |tail| tail.contains(&wrapped_prefix))?;

    let screen = VtScreen::parse(size, &shell.transcript_string()).text();
    assert!(
        screen.contains("╰"),
        "wrapped wide input erased the welcome box bottom border:\n{screen}"
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
    shell.wait_until_after(0, |tail| strip_ansi_codes(tail).contains("> partial"))?;

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
    shell.wait_until_after(0, |tail| strip_ansi_codes(tail).contains(">  CTRL_L_OK"))?;

    let offset = shell.transcript_len();
    shell.write("\u{c}")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        let visible = strip_ansi_codes(tail);
        visible.contains("echo \\") && visible.contains(">  CTRL_L_OK")
    })?;
    let visible_transcript = strip_ansi_codes(&transcript[offset..]);

    assert!(
        visible_transcript.contains("echo \\"),
        "first line was not re-rendered after Ctrl+L:\n{}",
        &transcript[offset..]
    );
    assert!(
        visible_transcript.contains(">  CTRL_L_OK"),
        "continuation line was not re-rendered after Ctrl+L:\n{}",
        &transcript[offset..]
    );

    shell.write("\r")?;
    shell.wait_until_after(offset, |tail| count_matches(tail, "CTRL_L_OK") >= 2)?;
    shell.exit()
}
