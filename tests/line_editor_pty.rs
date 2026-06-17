use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use theseus::input::{
    DEFAULT_COMMAND_CONTINUATION_PROMPT, DEFAULT_MULTILINE_PREFIX, MULTILINE_SUBMIT_COMMAND,
    strip_ansi_codes,
};

const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_millis(500);
const ENABLE_BRACKETED_PASTE: &str = "\x1b[?2004h";
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";
const KEY_UP: &str = "\x1b[A";
const KEY_DOWN: &str = "\x1b[B";
const KEY_LEFT: &str = "\x1b[D";
const KEY_TAB: &str = "\t";
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
        let home = temp_home()?;
        Self::start_with_home_and_size(
            home,
            PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    }

    fn start_with_home(home: PathBuf) -> io::Result<Self> {
        Self::start_with_home_and_size(
            home,
            PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    }

    fn start_with_size(size: PtySize) -> io::Result<Self> {
        let home = temp_home()?;
        Self::start_with_home_and_size(home, size)
    }

    fn start_with_home_and_size(home: PathBuf, size: PtySize) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size)
            .map_err(|err| io::Error::other(err.to_string()))?;

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

fn command_history_path(home: &std::path::Path) -> PathBuf {
    home.join(".theseus")
        .join("persist")
        .join("history_command_v2.json")
}

fn count_matches(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn command_continuation_row() -> String {
    format!("\r\n{DEFAULT_COMMAND_CONTINUATION_PROMPT}")
}

fn command_continuation_text(text: &str) -> String {
    format!("{DEFAULT_COMMAND_CONTINUATION_PROMPT}{text}")
}

fn multiline_prefix_text(text: &str) -> String {
    format!("{DEFAULT_MULTILINE_PREFIX}{text}")
}

fn create_path_completion_fixture(root: &Path) -> io::Result<PathBuf> {
    let dir = root.join("path-completion");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("theseus-mojo"))?;
    fs::create_dir_all(dir.join("theseus-shell"))?;
    Ok(dir)
}

fn enter_path_completion_fixture(shell: &mut PtyShell, dir: &Path) -> io::Result<()> {
    shell.write(&format!("cd {}\r", dir.display()))?;
    let prompt_name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("path-completion");
    wait_for_default_screen(shell, |screen| screen.contains(&format!("{prompt_name}>")))?;
    Ok(())
}

fn default_screen_text(shell: &PtyShell) -> String {
    VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text()
}

fn wait_for_default_screen<F>(shell: &PtyShell, predicate: F) -> io::Result<String>
where
    F: Fn(&str) -> bool,
{
    let start = Instant::now();
    loop {
        let screen = default_screen_text(shell);
        if predicate(&screen) {
            return Ok(screen);
        }
        if start.elapsed() > WAIT_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for screen; screen was:\n{screen}"),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn command_prompt_enables_bracketed_paste() -> io::Result<()> {
    let _lock = pty_test_lock();
    let shell = PtyShell::start()?;
    let transcript = shell.transcript_string();

    assert!(
        transcript.contains(ENABLE_BRACKETED_PASTE),
        "command prompt should enable bracketed paste so multiline paste arrives as one Event::Paste:\n{transcript:?}"
    );

    shell.exit()
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
    let continuation_row = command_continuation_row();
    shell.wait_for(&continuation_row)?;

    let offset = shell.transcript_len();
    shell.write(" THESEUS_JOINED_OK\r")?;
    let transcript =
        shell.wait_until_after(offset, |tail| count_matches(tail, "THESEUS_JOINED_OK") >= 2)?;

    assert!(
        transcript.contains(&continuation_row),
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
    let continuation_row = command_continuation_row();

    assert!(
        transcript[offset..].contains(&continuation_row),
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
fn bracketed_paste_assignment_before_heredoc_executes_as_one_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let command = concat!(
        "USER_ID=42\n",
        "cat <<JSON\n",
        "{\n",
        "  \"userId\": $USER_ID,\n",
        "  \"body\": \"raw \\$NOT_EXPANDED but \\$USER_ID works\"\n",
        "}\n",
        "JSON\n",
    );

    let offset = shell.transcript_len();
    shell.write(BRACKETED_PASTE_START)?;
    shell.write(command)?;
    shell.write(BRACKETED_PASTE_END)?;
    let transcript = shell.wait_until_after(offset, |tail| tail.contains("\"userId\": 42"))?;

    assert!(
        transcript[offset..].contains("raw $NOT_EXPANDED but $USER_ID works"),
        "bracketed pasted heredoc did not preserve escaped variables:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn bracketed_paste_renders_multiline_command_before_submit() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let command = concat!(
        "USER_ID=42\n",
        "cat <<JSON\n",
        "{\n",
        "  \"userId\": $USER_ID\n",
        "}\n",
        "JSON\n",
    );

    let offset = shell.transcript_len();
    shell.write(BRACKETED_PASTE_START)?;
    shell.write(command)?;
    shell.write(BRACKETED_PASTE_END)?;
    let transcript = shell.wait_until_after(offset, |tail| tail.contains("\"userId\": 42"))?;
    let tail = &transcript[offset..];
    let visible_tail = strip_ansi_codes(tail);

    assert!(
        visible_tail.contains("USER_ID=42") && visible_tail.contains("cat <<JSON"),
        "bracketed pasted multiline command should be rendered before execution output:\n{visible_tail}"
    );

    shell.exit()
}

#[test]
fn shell_editor_bracketed_paste_preserves_physical_newlines() -> io::Result<()> {
    let _lock = pty_test_lock();
    let size = PtySize {
        rows: 30,
        cols: 180,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut shell = PtyShell::start_with_size(size)?;

    let command = concat!(
        "USER_ID=42\r",
        "curl -sS https://jsonplaceholder.typicode.com/posts \\\r",
        "    -X POST \\\r",
        "    -H \"Content-Type: application/json\" \\\r",
        "    --data-binary @- <<JSON\r",
        "{\r",
        "  \"userId\": $USER_ID,\r",
        "  \"title\":  \"hello from $USER_ID\",\r",
        "  \"body\":   \"raw \\$NOT_EXPANDED but \\$USER_ID works\"\r",
        "}\r",
        "JSON",
    );

    shell.write("/shell\r")?;
    shell.wait_for("Enter multiline shell command")?;

    let offset = shell.transcript_len();
    shell.write(BRACKETED_PASTE_START)?;
    shell.write(command)?;
    shell.write(BRACKETED_PASTE_END)?;
    shell.wait_until_after(offset, |tail| {
        let visible = strip_ansi_codes(tail);
        visible.contains("JSON")
            && (visible.contains("raw \\$NOT_EXPANDED but \\$USER_ID works")
                || visible.contains("raw $NOT_EXPANDED but $USER_ID works"))
    })?;

    let screen = VtScreen::parse(size, &shell.transcript_string()).text();

    assert!(
        screen.contains(&multiline_prefix_text("USER_ID=42")),
        "multiline /shell paste should render the first pasted line as its own editor row:\n{screen}"
    );
    assert!(
        screen.contains(&multiline_prefix_text(
            "curl -sS https://jsonplaceholder.typicode.com/posts \\"
        )),
        "multiline /shell paste should preserve the curl row after the first newline:\n{screen}"
    );
    assert!(
        screen.contains(&multiline_prefix_text(
            "    -H \"Content-Type: application/json\" \\"
        )),
        "multiline /shell paste should preserve indented continuation rows:\n{screen}"
    );
    assert!(
        !screen.contains("USER_ID=42curl"),
        "multiline /shell paste collapsed physical newlines into a single row:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn bracketed_paste_output_without_trailing_newline_does_not_merge_with_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let command = "printf '{\\n  \"ok\": true\\n}'\n";

    let offset = shell.transcript_len();
    shell.write(BRACKETED_PASTE_START)?;
    shell.write(command)?;
    shell.write(BRACKETED_PASTE_END)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("\"ok\": true") && tail.contains("theseus-shell")
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        !screen.contains("}euclid"),
        "prompt was rendered on the same row as bracketed pasted command output without trailing newline:\n{screen}"
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
fn command_output_without_trailing_newline_does_not_merge_with_next_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("printf NO_NEWLINE_PROMPT_OK\r")?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("NO_NEWLINE_PROMPT_OK") && tail.contains("theseus-shell")
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        !screen.contains("NO_NEWLINE_PROMPT_OKeuclid"),
        "prompt was rendered on the same row as command output without trailing newline:\n{screen}"
    );

    shell.exit()
}

#[test]
fn clear_command_does_not_add_blank_line_before_next_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("clear\r")?;
    shell.wait_until_after(offset, |tail| {
        (tail.contains("\x1b[2J") || tail.contains("\x1b[H"))
            && VtScreen::parse(
                PtySize {
                    rows: 24,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                tail,
            )
            .text()
            .contains("theseus-shell>")
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();
    let first_line = screen.lines().next().unwrap_or_default();

    assert!(
        first_line.starts_with("euclid theseus-shell>"),
        "clear should render the next prompt at the top of the cleared screen without a leading blank line:\n{screen}"
    );

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
fn command_history_preserves_explicit_ask_prefix_for_single_line_agent_entries() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "а что ты умеешь делать с bash?",
    "kind": "agent",
    "mode": "single_line"
  },
  {
    "text": "а что ты умеешь делать с bash?",
    "kind": "agent",
    "mode": "single_line_ask"
  }
]
"#,
    )?;
    let size = PtySize {
        rows: 24,
        cols: 100,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut shell = PtyShell::start_with_home_and_size(home, size)?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        strip_ansi_codes(tail).contains("/ask а что ты умеешь делать")
    })?;
    let explicit_ask_screen = VtScreen::parse(size, &shell.transcript_string()).text();
    assert!(
        explicit_ask_screen.contains("theseus-shell> /ask а что ты умеешь делать с bash?"),
        "explicit /ask history entry should be recalled with /ask prefix:\n{explicit_ask_screen}"
    );

    let second_offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(second_offset, |tail| {
        let visible = strip_ansi_codes(tail);
        visible.contains("theseus-shell") && visible.contains("а что ты умеешь делать с bash?")
    })?;
    let routed_agent_screen = VtScreen::parse(size, &shell.transcript_string()).text();
    assert!(
        routed_agent_screen.contains("theseus-shell> а что ты умеешь делать с bash?"),
        "auto-routed agent history entry should be recalled without /ask prefix:\n{routed_agent_screen}"
    );
    assert!(
        !routed_agent_screen.contains("theseus-shell> /ask а что ты умеешь делать с bash?"),
        "auto-routed agent history entry should not gain /ask prefix:\n{routed_agent_screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn command_history_mode_walks_past_multiline_entry_without_cursor_repositioning() -> io::Result<()>
{
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("echo HISTORY_MULTILINE_OLDER\r")?;
    shell.wait_for("HISTORY_MULTILINE_OLDER")?;
    shell.wait_for_after(shell.transcript_len(), "theseus-shell")?;

    shell.write("/shell\r")?;
    shell.wait_for("Enter multiline shell command")?;
    let submit_offset = shell.transcript_len();
    shell.write(&format!(
        "printf '%s\\n' \\\r  HISTORY_MULTILINE_NEWER\r{MULTILINE_SUBMIT_COMMAND}\r"
    ))?;
    shell.wait_for_after(submit_offset, "HISTORY_MULTILINE_NEWER")?;
    shell.wait_for_after(submit_offset, "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| tail.contains("HISTORY_MULTILINE_NEWER"))?;
    shell.write(KEY_UP)?;
    shell.write("\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        count_matches(tail, "HISTORY_MULTILINE_OLDER") >= 2
    })?;

    assert!(
        transcript[offset..].contains("\x1b[3m"),
        "multiline command history entry was not shown in browsing style:\n{}",
        &transcript[offset..]
    );
    assert!(
        count_matches(&transcript[offset..], "HISTORY_MULTILINE_OLDER") >= 2,
        "second Up did not move past multiline history entry to the older command:\n{}",
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
    shell.write(&format!(
        "ASK_HISTORY_LINE_ONE\rASK_HISTORY_LINE_TWO\r{MULTILINE_SUBMIT_COMMAND}\r"
    ))?;
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
fn ask_inline_backslash_continuation_reads_multiline_prompt() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("/ask a ты меешь \\\rрассказывать \\\rанекдоты?\r")?;
    let transcript = shell.wait_until_after(offset, |tail| tail.contains("анекдоты?"))?;
    let continuation_text = command_continuation_text("рассказывать");

    assert!(
        transcript[offset..].contains(&continuation_text),
        "/ask backslash continuation did not show continuation prompt:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains("a ты меешь")
            && transcript[offset..].contains("рассказывать")
            && transcript[offset..].contains("анекдоты?"),
        "/ask backslash continuation did not preserve multiline prompt:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn ask_multiline_history_mode_walks_entries_without_cursor_repositioning() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("/ask\r")?;
    shell.wait_for("Enter multiline input")?;
    let first_submit_offset = shell.transcript_len();
    shell.write(&format!(
        "ASK_HISTORY_OLD_ONE\rASK_HISTORY_OLD_TWO\r{MULTILINE_SUBMIT_COMMAND}\r"
    ))?;
    shell.wait_for_after(first_submit_offset, "theseus-shell")?;

    shell.write("/ask\r")?;
    shell.wait_for("Enter multiline input")?;
    let second_submit_offset = shell.transcript_len();
    shell.write(&format!(
        "ASK_HISTORY_NEW_ONE\rASK_HISTORY_NEW_TWO\r{MULTILINE_SUBMIT_COMMAND}\r"
    ))?;
    shell.wait_for_after(second_submit_offset, "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write("/ask\r")?;
    shell.wait_for_after(offset, "Enter multiline input")?;
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("ASK_HISTORY_NEW_ONE") && tail.contains("ASK_HISTORY_NEW_TWO")
    })?;
    shell.write(KEY_UP)?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("ASK_HISTORY_OLD_ONE") && tail.contains("ASK_HISTORY_OLD_TWO")
    })?;

    assert!(
        transcript[offset..].contains("\x1b[3m"),
        "history browsing did not render recalled text as italic:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains("ASK_HISTORY_OLD_ONE")
            && transcript[offset..].contains("ASK_HISTORY_OLD_TWO"),
        "second Up did not move to the older multiline /ask prompt:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Ask cancelled")?;
    shell.exit()
}

#[test]
fn command_history_multiline_ask_recall_finishes_prompt_line_before_editor() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "A ты умеешь\nрассказывать\nанекдоты\nпро\nпиратов?",
    "kind": "agent",
    "mode": "multi_line_ask"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write("clear")?;
    shell.write(KEY_UP)?;
    shell.write("\r")?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("Enter multiline input") && tail.contains("пиратов?")
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        !screen.contains("clearEnter multiline input"),
        "multiline /ask recall was rendered on the same prompt row as the current draft:\n{screen}"
    );
    assert!(
        screen.contains("theseus-shell> /ask"),
        "multiline /ask recall did not show the recalled /ask command before opening the editor:\n{screen}"
    );
    assert!(
        screen.contains(&multiline_prefix_text("A ты умеешь"))
            && screen.contains(&multiline_prefix_text("пиратов?")),
        "multiline /ask editor did not render the recalled prompt:\n{screen}"
    );
    assert_eq!(
        count_matches(&screen, "пиратов?"),
        1,
        "multiline /ask preview was not cleared before opening the editor:\n{screen}"
    );
    let transcript = shell.transcript_string();
    let transition_tail = &transcript[offset..];
    let editor_tail = transition_tail
        .rsplit("Enter multiline input")
        .next()
        .unwrap_or(transition_tail);
    let accepted_prompt_tail = transition_tail
        .rsplit("theseus-shell")
        .next()
        .unwrap_or(transition_tail);
    assert!(
        !accepted_prompt_tail.contains("\x1b[3m\x1b[96m/ask"),
        "accepted multiline /ask command should not stay italic after leaving command-history browsing:\n{transition_tail}"
    );
    assert!(
        !editor_tail.contains("\x1b[3mпиратов?"),
        "multiline /ask editor should open in editing mode after command-history selection:\n{transition_tail}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Ask cancelled")?;
    shell.exit()
}

#[test]
fn command_history_multiline_ask_left_accepts_preview_into_editor() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "Расскажи\nанекдот\nпро врачей",
    "kind": "agent",
    "mode": "multi_line_ask"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("/ask") && tail.contains("про врачей")
    })?;

    let left_offset = shell.transcript_len();
    shell.write(KEY_LEFT)?;
    shell.wait_until_after(left_offset, |tail| {
        let visible = strip_ansi_codes(tail);
        visible.contains("theseus-shell> /ask")
            && visible.contains(&multiline_prefix_text("Расскажи"))
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        screen.contains(&format!(
            "Enter multiline input. Type {MULTILINE_SUBMIT_COMMAND} on a new line to finish."
        )),
        "Left from multiline /ask preview should keep the ask editor hint:\n{screen}"
    );
    assert!(
        screen.contains(&multiline_prefix_text("Расскажи"))
            && screen.contains(&multiline_prefix_text("про врачей")),
        "Left from multiline /ask preview should open the real multiline editor with body prompts:\n{screen}"
    );
    assert_eq!(
        count_matches(&screen, "про врачей"),
        1,
        "Left from multiline /ask preview should clear the preview before opening the editor:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Ask cancelled")?;
    shell.exit()
}

#[test]
fn command_history_multiline_ask_text_key_keeps_preview_browsing() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "Расскажи\nанекдот\nпро врачей",
    "kind": "agent",
    "mode": "multi_line_ask"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("/ask") && tail.contains("про врачей")
    })?;

    let key_offset = shell.transcript_len();
    shell.write("u")?;
    shell.wait_until_after(key_offset, |tail| {
        strip_ansi_codes(tail).contains("про врачей")
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        screen.contains(&format!(
            "Enter multiline input. Type {MULTILINE_SUBMIT_COMMAND} on a new line to finish."
        )),
        "Text key in multiline /ask preview should keep browsing preview instead of opening a partial editor:\n{screen}"
    );
    assert_eq!(
        count_matches(&screen, "про врачей"),
        1,
        "Text key in multiline /ask preview should not duplicate or edit the recalled prompt:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn command_history_down_returns_from_multiline_ask_preview_to_newer_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "A ты умеешь\nрассказывать\nанекдоты?",
    "kind": "agent",
    "mode": "multi_line_ask"
  },
  {
    "text": "clear",
    "kind": "shell",
    "mode": "single_line"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| tail.contains("clear"))?;
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("/ask")
            && tail.contains("A ты умеешь")
            && tail.contains("рассказывать")
            && tail.contains("анекдоты?")
    })?;
    let preview_screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();
    assert!(
        preview_screen.contains(&format!(
            "Enter multiline input. Type {MULTILINE_SUBMIT_COMMAND} on a new line to finish."
        )),
        "multiline /ask command-history preview should show the multiline editor hint:\n{preview_screen}"
    );
    shell.write(KEY_DOWN)?;
    let transcript = shell.wait_until_after(offset, |tail| count_matches(tail, "clear") >= 2)?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        screen.contains("theseus-shell> clear"),
        "Down did not return from multiline /ask history preview to the newer command:\n{screen}\n\ntranscript:\n{}",
        &transcript[offset..]
    );
    assert!(
        !screen.contains("Enter multiline input"),
        "Down should stay inside command history browsing instead of entering multiline /ask editor:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn command_history_multiline_shell_recall_uses_shell_preview_and_editor() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "printf '%s\\n' \\\n  SHELL_COMMAND_HISTORY_PREVIEW",
    "kind": "shell",
    "mode": "multi_line_shell"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("/shell") && tail.contains("SHELL_COMMAND_HISTORY_PREVIEW")
    })?;
    let preview_screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        preview_screen.contains("theseus-shell> /shell"),
        "multiline /shell command-history preview should show /shell:\n{preview_screen}"
    );
    assert!(
        preview_screen.contains(&format!(
            "Enter multiline shell command. Type {MULTILINE_SUBMIT_COMMAND} on a new line to run."
        )),
        "multiline /shell command-history preview should show the shell editor hint:\n{preview_screen}"
    );
    assert!(
        preview_screen.contains(&multiline_prefix_text("printf"))
            && preview_screen.contains(&multiline_prefix_text("  SHELL_COMMAND_HISTORY_PREVIEW")),
        "multiline /shell command-history preview should render the command body with continuation prompts:\n{preview_screen}"
    );

    shell.write("\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    let transcript = shell.transcript_string();
    let transition_tail = &transcript[offset..];
    let editor_tail = transition_tail
        .rsplit("Enter multiline shell command")
        .next()
        .unwrap_or(transition_tail);
    assert!(
        !editor_tail.contains("\x1b[3mSHELL_COMMAND_HISTORY_PREVIEW"),
        "multiline /shell editor should open in editing mode after command-history selection:\n{transition_tail}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn command_history_multiline_shell_left_accepts_preview_into_editor() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "curl -sS https://example.com \\\n  -H \"Content-Type: application/json\"",
    "kind": "shell",
    "mode": "multi_line_shell"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("/shell") && tail.contains("Content-Type: application/json")
    })?;

    let left_offset = shell.transcript_len();
    shell.write(KEY_LEFT)?;
    shell.wait_until_after(left_offset, |tail| {
        let visible = strip_ansi_codes(tail);
        visible.contains("theseus-shell> /shell")
            && visible.contains(&multiline_prefix_text("curl -sS https://example.com \\"))
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        screen.contains(&format!(
            "Enter multiline shell command. Type {MULTILINE_SUBMIT_COMMAND} on a new line to run."
        )),
        "Left from multiline /shell preview should keep the shell editor hint:\n{screen}"
    );
    assert!(
        screen.contains(&multiline_prefix_text("curl -sS https://example.com \\"))
            && screen.contains(&multiline_prefix_text(
                "  -H \"Content-Type: application/json\""
            )),
        "Left from multiline /shell preview should open the real multiline editor with body prompts:\n{screen}"
    );
    assert_eq!(
        count_matches(&screen, "Content-Type: application/json"),
        1,
        "Left from multiline /shell preview should clear the preview before opening the editor:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn command_history_multiline_shell_text_key_keeps_preview_browsing() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "curl -sS https://example.com \\\n  -H \"Content-Type: application/json\"",
    "kind": "shell",
    "mode": "multi_line_shell"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("/shell") && tail.contains("Content-Type: application/json")
    })?;

    let key_offset = shell.transcript_len();
    shell.write("u")?;
    shell.wait_until_after(key_offset, |tail| {
        strip_ansi_codes(tail).contains("Content-Type: application/json")
    })?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        screen.contains(&format!(
            "Enter multiline shell command. Type {MULTILINE_SUBMIT_COMMAND} on a new line to run."
        )),
        "Text key in multiline /shell preview should keep browsing preview instead of opening a partial editor:\n{screen}"
    );
    assert_eq!(
        count_matches(&screen, "Content-Type: application/json"),
        1,
        "Text key in multiline /shell preview should not duplicate or edit the recalled command:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Interrupted. Type /exit to exit the shell.")?;
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
    let saved = fs::read_to_string(command_history_path(&shell.home))?;
    assert!(
        saved.contains("ASK_CANCELLED_DRAFT_ONE")
            && saved.contains("ASK_CANCELLED_DRAFT_TWO")
            && saved.contains("\"kind\": \"agent\"")
            && saved.contains("\"mode\": \"multi_line_ask\""),
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
fn ask_multiline_persistent_draft_replaces_intermediate_edits() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    let history = (0..100)
        .map(|index| {
            serde_json::json!({
                "text": format!("prefilled-command-{index}"),
                "kind": "shell",
                "mode": "single_line",
            })
        })
        .collect::<Vec<_>>();
    let mut text = serde_json::to_string_pretty(&history).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(&history_path, text)?;
    let mut shell = PtyShell::start_with_home(home)?;

    shell.write("/ask\r")?;
    shell.wait_for("Enter multiline input")?;
    shell.write("ASK_INCREMENTAL_DRAFT")?;
    shell.wait_for("ASK_INCREMENTAL_DRAFT")?;

    let saved = fs::read_to_string(command_history_path(&shell.home))?;
    let history: Vec<serde_json::Value> = serde_json::from_str(&saved).map_err(io::Error::other)?;
    let matching_drafts = history
        .iter()
        .filter(|entry| {
            entry.get("kind").and_then(serde_json::Value::as_str) == Some("agent")
                && entry.get("mode").and_then(serde_json::Value::as_str) == Some("multi_line_ask")
                && entry
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| text.starts_with("ASK_INCREMENTAL"))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        history.len(),
        100,
        "multiline /ask persistent history should stay capped while editing:\n{saved}"
    );
    assert!(
        !history.iter().any(|entry| {
            entry.get("text").and_then(serde_json::Value::as_str) == Some("ASK_INCREMENTAL")
        }),
        "multiline /ask persistent history should not keep an intermediate draft:\n{saved}"
    );
    assert_eq!(
        matching_drafts.len(),
        1,
        "multiline /ask persistent history should keep one live draft, not every intermediate edit:\n{saved}"
    );
    assert_eq!(
        matching_drafts[0]
            .get("text")
            .and_then(serde_json::Value::as_str),
        Some("ASK_INCREMENTAL_DRAFT"),
        "multiline /ask persistent draft should contain the latest edit:\n{saved}"
    );

    shell.write("\u{3}")?;
    shell.wait_for("Ask cancelled")?;
    shell.exit()
}

#[test]
fn ask_multiline_history_deduplicates_entries_after_filtering() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "ASK_FILTERED_DUPLICATE_PROMPT",
    "kind": "agent",
    "mode": "multi_line_ask"
  },
  {
    "text": "clear",
    "kind": "shell",
    "mode": "single_line"
  },
  {
    "text": "ASK_FILTERED_DUPLICATE_PROMPT",
    "kind": "agent",
    "mode": "multi_line_ask"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write("/ask\r")?;
    shell.wait_for_after(offset, "Enter multiline input")?;
    shell.write(KEY_UP)?;
    shell.wait_until_after(offset, |tail| {
        tail.contains("ASK_FILTERED_DUPLICATE_PROMPT")
    })?;
    shell.write(KEY_UP)?;
    let down_offset = shell.transcript_len();
    shell.write(KEY_DOWN)?;
    shell.wait_until_after(down_offset, |tail| tail.contains("\x1b[2K"))?;
    let screen = VtScreen::parse(
        PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        },
        &shell.transcript_string(),
    )
    .text();

    assert!(
        !screen.contains("ASK_FILTERED_DUPLICATE_PROMPT"),
        "filtered multiline /ask history still exposed duplicate entries:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Ask cancelled")?;
    shell.exit()
}

#[test]
fn shell_multiline_mode_executes_command_after_end() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("/shell\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    shell.write(&format!(
        "printf '%s\\n' \\\r  SHELL_MODE_OK\r{MULTILINE_SUBMIT_COMMAND}\r"
    ))?;
    let transcript = shell.wait_until_after(offset, |tail| tail.contains("SHELL_MODE_OK"))?;

    assert!(
        transcript[offset..].contains("SHELL_MODE_OK"),
        "/shell multiline command did not execute after {MULTILINE_SUBMIT_COMMAND}:\n{}",
        &transcript[offset..]
    );

    shell.exit()
}

#[test]
fn shell_multiline_history_mode_recalls_previous_command() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("/shell\r")?;
    shell.wait_for("Enter multiline shell command")?;
    let submit_offset = shell.transcript_len();
    shell.write(&format!(
        "printf '%s\\n' \\\r  SHELL_HISTORY_OK\r{MULTILINE_SUBMIT_COMMAND}\r"
    ))?;
    shell.wait_for_after(submit_offset, "SHELL_HISTORY_OK")?;
    shell.wait_for_after(submit_offset, "theseus-shell")?;

    let offset = shell.transcript_len();
    shell.write("/shell\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    shell.write(KEY_UP)?;
    let transcript = shell.wait_until_after(offset, |tail| tail.contains("SHELL_HISTORY_OK"))?;

    assert!(
        transcript[offset..].contains("\x1b[3m"),
        "/shell history browsing did not render recalled command as italic:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains("SHELL_HISTORY_OK"),
        "Up did not recall the previous multiline /shell command:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn shell_multiline_history_filters_non_shell_command_history_entries() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    fs::write(
        &history_path,
        r#"[
  {
    "text": "printf SHELL_FILTER_OK",
    "kind": "shell",
    "mode": "single_line"
  },
  {
    "text": "explain this command",
    "kind": "agent",
    "mode": "single_line"
  },
  {
    "text": "what is wrong with this code?",
    "kind": "agent",
    "mode": "single_line"
  }
]
"#,
    )?;
    let mut shell = PtyShell::start_with_home(home)?;

    let offset = shell.transcript_len();
    shell.write("/shell\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    shell.write(KEY_UP)?;
    let transcript = shell.wait_until_after(offset, |tail| tail.contains("SHELL_FILTER_OK"))?;

    assert!(
        !transcript[offset..].contains("what is wrong with this code?"),
        "/shell recalled an agent prompt from command history:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains("SHELL_FILTER_OK"),
        "/shell did not recall the shell command from command history:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn shell_multiline_success_normalizes_draft_before_history_append() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("/shell\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    shell.write(&format!(
        "\rprintf SHELL_TRIM_DUP_OK\r{MULTILINE_SUBMIT_COMMAND}\r"
    ))?;
    shell.wait_for_after(offset, "SHELL_TRIM_DUP_OK")?;
    shell.wait_for_after(offset, "theseus-shell")?;

    let saved = fs::read_to_string(command_history_path(&shell.home))?;
    assert_eq!(
        count_matches(&saved, "SHELL_TRIM_DUP_OK"),
        1,
        "/shell submit stored both the raw draft and trimmed command:\n{saved}"
    );

    shell.exit()
}

#[test]
fn shell_multiline_persistent_draft_replaces_intermediate_edits() -> io::Result<()> {
    let _lock = pty_test_lock();
    let home = temp_home()?;
    let history_path = command_history_path(&home);
    fs::create_dir_all(history_path.parent().unwrap())?;
    let history = (0..100)
        .map(|index| {
            serde_json::json!({
                "text": format!("prefilled-command-{index}"),
                "kind": "shell",
                "mode": "single_line",
            })
        })
        .collect::<Vec<_>>();
    let mut text = serde_json::to_string_pretty(&history).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(&history_path, text)?;
    let mut shell = PtyShell::start_with_home(home)?;

    shell.write("/shell\r")?;
    shell.wait_for("Enter multiline shell command")?;
    shell.write("printf SHELL_INCREMENTAL_DRAFT")?;
    shell.wait_for("SHELL_INCREMENTAL_DRAFT")?;

    let saved = fs::read_to_string(command_history_path(&shell.home))?;
    let history: Vec<serde_json::Value> = serde_json::from_str(&saved).map_err(io::Error::other)?;
    let matching_drafts = history
        .iter()
        .filter(|entry| {
            entry.get("kind").and_then(serde_json::Value::as_str) == Some("shell")
                && entry.get("mode").and_then(serde_json::Value::as_str) == Some("multi_line_shell")
                && entry
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| text.starts_with("printf SHELL_INCREMENTAL"))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        history.len(),
        100,
        "multiline /shell persistent history should stay capped while editing:\n{saved}"
    );
    assert!(
        !history.iter().any(|entry| {
            entry.get("text").and_then(serde_json::Value::as_str)
                == Some("printf SHELL_INCREMENTAL")
        }),
        "multiline /shell persistent history should not keep an intermediate draft:\n{saved}"
    );
    assert_eq!(
        matching_drafts.len(),
        1,
        "multiline /shell persistent history should keep one live draft, not every intermediate edit:\n{saved}"
    );
    assert_eq!(
        matching_drafts[0]
            .get("text")
            .and_then(serde_json::Value::as_str),
        Some("printf SHELL_INCREMENTAL_DRAFT"),
        "multiline /shell persistent draft should contain the latest edit:\n{saved}"
    );

    shell.write("\u{3}")?;
    shell.wait_for("Shell cancelled")?;
    shell.exit()
}

#[test]
fn shell_multiline_history_preserves_cancelled_draft() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    shell.write("/shell\r")?;
    shell.wait_for("Enter multiline shell command")?;
    shell.write("printf '%s\\n' \\\r  SHELL_CANCELLED_DRAFT_TWO")?;
    shell.wait_for("SHELL_CANCELLED_DRAFT_TWO")?;
    let cancel_offset = shell.transcript_len();
    shell.write("\u{3}")?;
    shell.wait_for_after(cancel_offset, "Shell cancelled")?;
    shell.wait_for_after(cancel_offset, "theseus-shell")?;
    let saved = fs::read_to_string(command_history_path(&shell.home))?;
    assert!(
        saved.contains("printf")
            && saved.contains("SHELL_CANCELLED_DRAFT_TWO")
            && saved.contains("\"kind\": \"shell\"")
            && saved.contains("\"mode\": \"multi_line_shell\""),
        "cancelled /shell draft was not persisted:\n{saved}"
    );

    let offset = shell.transcript_len();
    shell.write("/shell\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    shell.write(KEY_UP)?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("printf") && tail.contains("SHELL_CANCELLED_DRAFT_TWO")
    })?;

    assert!(
        transcript[offset..].contains("printf")
            && transcript[offset..].contains("SHELL_CANCELLED_DRAFT_TWO"),
        "Up did not recall the cancelled multiline /shell draft:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn shell_multiline_mode_completes_commands_on_first_row() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;

    let offset = shell.transcript_len();
    shell.write("/shell\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    shell.write("/he\t")?;
    let transcript = shell.wait_until_after(offset, |tail| tail.contains("/help"))?;

    assert!(
        transcript[offset..].contains("/help"),
        "/shell did not complete commands on the first row:\n{}",
        &transcript[offset..]
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn path_completion_uses_common_prefix_in_single_line_mode() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;
    let fixture = create_path_completion_fixture(&shell.home)?;
    enter_path_completion_fixture(&mut shell, &fixture)?;

    let offset = shell.transcript_len();
    shell.write("printf '%s\\n' th")?;
    shell.write(KEY_TAB)?;
    let screen = wait_for_default_screen(&shell, |screen| {
        screen.contains("printf '%s\\n' theseus-") || screen.contains("printf '%s\\n' theseus-mojo")
    })?;

    assert!(
        screen.contains("printf '%s\\n' theseus-"),
        "single-line path completion should first apply only the shared path prefix:\n{screen}"
    );
    assert!(
        !screen.contains("printf '%s\\n' theseus-mojo")
            && !screen.contains("printf '%s\\n' theseus-shell"),
        "single-line path completion should not select a full candidate on first Tab:\n{screen}"
    );

    let cycle_offset = shell.transcript_len();
    shell.write(KEY_TAB)?;
    shell.wait_for_after(cycle_offset, "theseus-mojo")?;
    let screen = wait_for_default_screen(&shell, |screen| {
        screen.contains("printf '%s\\n' theseus-mojo")
    })?;
    assert!(
        screen.contains("printf '%s\\n' theseus-mojo"),
        "second Tab should move from the shared prefix to the first concrete path candidate:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn path_completion_escapes_spaces_in_single_line_mode() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;
    let fixture = shell.home.join("path-space-completion");
    let _ = fs::remove_dir_all(&fixture);
    fs::create_dir_all(&fixture)?;
    fs::write(fixture.join("Hello, World!"), "hello\n")?;
    enter_path_completion_fixture(&mut shell, &fixture)?;

    let offset = shell.transcript_len();
    shell.write("du -h He")?;
    shell.write(KEY_TAB)?;
    let screen =
        wait_for_default_screen(&shell, |screen| screen.contains("du -h Hello,\\ World!"))?;

    assert!(
        screen.contains("du -h Hello,\\ World!"),
        "path completion should escape spaces before inserting a shell path:\n{screen}"
    );

    shell.write("\r")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        tail.contains("Hello, World!") && tail.contains("path-space-completion")
    })?;
    let visible = strip_ansi_codes(&transcript[offset..]);

    assert!(
        !visible.contains("du: Hello,: No such file or directory")
            && !visible.contains("du: World!: No such file or directory"),
        "completed path with spaces should execute as one shell argument:\n{visible}"
    );

    shell.exit()
}

#[test]
fn path_completion_uses_common_prefix_in_command_multiline_mode() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;
    let fixture = create_path_completion_fixture(&shell.home)?;
    enter_path_completion_fixture(&mut shell, &fixture)?;

    shell.write("printf '%s\\n' \\\r")?;
    shell.wait_for(&command_continuation_row())?;

    let offset = shell.transcript_len();
    shell.write("th")?;
    shell.write(KEY_TAB)?;
    let screen = wait_for_default_screen(&shell, |screen| {
        screen.contains(&command_continuation_text("theseus-"))
            || screen.contains(&command_continuation_text("theseus-mojo"))
    })?;

    assert!(
        screen.contains(&command_continuation_text("theseus-")),
        "command multiline path completion should first apply only the shared path prefix:\n{screen}"
    );
    assert!(
        !screen.contains(&command_continuation_text("theseus-mojo"))
            && !screen.contains(&command_continuation_text("theseus-shell")),
        "command multiline path completion should not select a full candidate on first Tab:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Interrupted. Type /exit to exit the shell.")?;
    shell.exit()
}

#[test]
fn path_completion_uses_common_prefix_in_shell_multiline_mode() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;
    let fixture = create_path_completion_fixture(&shell.home)?;
    enter_path_completion_fixture(&mut shell, &fixture)?;

    let offset = shell.transcript_len();
    shell.write("/shell\r")?;
    shell.wait_for_after(offset, "Enter multiline shell command")?;
    shell.write("printf '%s\\n' th")?;
    shell.write(KEY_TAB)?;
    let screen = wait_for_default_screen(&shell, |screen| {
        screen.contains(&multiline_prefix_text("printf '%s\\n' theseus-"))
            || screen.contains(&multiline_prefix_text("printf '%s\\n' theseus-mojo"))
    })?;

    assert!(
        screen.contains(&multiline_prefix_text("printf '%s\\n' theseus-")),
        "/shell path completion should first apply only the shared path prefix:\n{screen}"
    );
    assert!(
        !screen.contains(&multiline_prefix_text("printf '%s\\n' theseus-mojo"))
            && !screen.contains(&multiline_prefix_text("printf '%s\\n' theseus-shell")),
        "/shell path completion should not select a full candidate on first Tab:\n{screen}"
    );

    shell.write("\u{3}")?;
    shell.wait_for_after(offset, "Shell cancelled")?;
    shell.exit()
}

#[test]
fn path_completion_uses_common_prefix_in_ask_multiline_mode() -> io::Result<()> {
    let _lock = pty_test_lock();
    let mut shell = PtyShell::start()?;
    let fixture = create_path_completion_fixture(&shell.home)?;
    enter_path_completion_fixture(&mut shell, &fixture)?;

    let offset = shell.transcript_len();
    shell.write("/ask\r")?;
    shell.wait_for_after(offset, "Enter multiline input")?;
    shell.write("th")?;
    shell.write(KEY_TAB)?;
    let screen = wait_for_default_screen(&shell, |screen| {
        screen.contains(&multiline_prefix_text("theseus-"))
            || screen.contains(&multiline_prefix_text("theseus-mojo"))
    })?;

    assert!(
        screen.contains(&multiline_prefix_text("theseus-")),
        "/ask path completion should first apply only the shared path prefix:\n{screen}"
    );
    assert!(
        !screen.contains(&multiline_prefix_text("theseus-mojo"))
            && !screen.contains(&multiline_prefix_text("theseus-shell")),
        "/ask path completion should not select a full candidate on first Tab:\n{screen}"
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
    shell.wait_for(&command_continuation_row())?;

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
    shell.wait_for(&command_continuation_row())?;

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
    shell.wait_for(&command_continuation_row())?;

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
    shell.wait_for(&command_continuation_row())?;
    shell.write("echo THESEUS_IF_BLOCK_OK\r")?;
    shell.wait_for(&command_continuation_row())?;

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
    let continuation_row = command_continuation_row();
    let transcript = shell.wait_for_after(offset, &continuation_row)?;

    assert!(
        transcript[offset..].contains("WRAP_MARKER_1234567890"),
        "long first line was not rendered:\n{}",
        &transcript[offset..]
    );
    assert!(
        transcript[offset..].contains(&continuation_row),
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
    shell.wait_for(&command_continuation_row())?;

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
    let continuation_text = command_continuation_text("partial");
    shell.wait_until_after(0, |tail| {
        strip_ansi_codes(tail).contains(&continuation_text)
    })?;

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
    let continuation_text = command_continuation_text(" CTRL_L_OK");
    shell.wait_until_after(0, |tail| {
        strip_ansi_codes(tail).contains(&continuation_text)
    })?;

    let offset = shell.transcript_len();
    shell.write("\u{c}")?;
    let transcript = shell.wait_until_after(offset, |tail| {
        let visible = strip_ansi_codes(tail);
        visible.contains("echo \\") && visible.contains(&continuation_text)
    })?;
    let visible_transcript = strip_ansi_codes(&transcript[offset..]);

    assert!(
        visible_transcript.contains("echo \\"),
        "first line was not re-rendered after Ctrl+L:\n{}",
        &transcript[offset..]
    );
    assert!(
        visible_transcript.contains(&continuation_text),
        "continuation line was not re-rendered after Ctrl+L:\n{}",
        &transcript[offset..]
    );

    shell.write("\r")?;
    shell.wait_until_after(offset, |tail| count_matches(tail, "CTRL_L_OK") >= 2)?;
    shell.exit()
}
