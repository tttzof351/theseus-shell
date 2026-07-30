use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_millis(500);
const SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

struct RenderV2Pty {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    transcript: Arc<Mutex<Vec<u8>>>,
    home: PathBuf,
}

impl RenderV2Pty {
    fn start() -> io::Result<Self> {
        let home = temp_home()?;
        let pair = native_pty_system()
            .openpty(SIZE)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_render_v2"));
        command.cwd(env!("CARGO_MANIFEST_DIR"));
        command.env("HOME", &home);
        command.env("USER", "tester");
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");

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
        shell.wait_until(|bytes| settled_prompt_is_visible(bytes))?;
        Ok(shell)
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()
    }

    fn transcript_len(&self) -> usize {
        self.transcript.lock().unwrap().len()
    }

    fn transcript(&self) -> Vec<u8> {
        self.transcript.lock().unwrap().clone()
    }

    fn wait_until(&self, predicate: impl Fn(&[u8]) -> bool) -> io::Result<Vec<u8>> {
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
                        "timed out waiting for render_v2 terminal output; tail was:\n{:?}",
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
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for RenderV2Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = fs::remove_dir_all(&self.home);
        let _ = self.master.resize(SIZE);
    }
}

fn temp_home() -> io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let home = std::env::temp_dir().join(format!(
        "theseus-render-v2-pty-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&home)?;
    Ok(home)
}

fn settled_prompt_is_visible(bytes: &[u8]) -> bool {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    parser.process(bytes);
    let screen = parser.screen();
    let (row, _) = screen.cursor_position();
    screen
        .rows(0, SIZE.cols)
        .nth(usize::from(row))
        .is_some_and(|line| line.starts_with("tester theseus-shell> "))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn output_marker_column(bytes: &[u8]) -> Option<(usize, String)> {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    parser.process(bytes);
    parser
        .screen()
        .rows(0, SIZE.cols)
        .find(|row| row.trim_start().starts_with("X") && row.contains("TAB_RIGHT"))
        .and_then(|row| {
            let column = row.find("TAB_RIGHT")?;
            Some((column, row.trim_end().to_string()))
        })
}

fn screen_text(bytes: &[u8]) -> String {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    parser.process(bytes);
    parser
        .screen()
        .rows(0, SIZE.cols)
        .map(|row| row.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn streamed_shell_output_does_not_move_when_diff_renderer_resumes() -> io::Result<()> {
    let mut shell = RenderV2Pty::start()?;
    let offset = shell.transcript_len();

    // The external terminal expands this tab using its native eight-column tab
    // stops. Once the command finishes, render_v2 rebuilds the same visible
    // transcript from VirtualScreen. That hand-off must not move existing text.
    shell.write("printf 'X\\tTAB_RIGHT\\n'\r")?;
    let transcript = shell.wait_until(|bytes| {
        let tail = bytes.get(offset..).unwrap_or_default();
        find_bytes(tail, b"X\tTAB_RIGHT").is_some() && settled_prompt_is_visible(bytes)
    })?;

    let tail = &transcript[offset..];
    let streamed_marker = find_bytes(tail, b"X\tTAB_RIGHT").expect("streamed marker");
    let streamed_marker_end = offset + streamed_marker + b"X\tTAB_RIGHT".len();
    let before_renderer_resumes = &transcript[..streamed_marker_end];
    let (streamed_column, streamed_row) =
        output_marker_column(before_renderer_resumes).expect("marker in streamed terminal state");
    let (settled_column, settled_row) =
        output_marker_column(&transcript).expect("marker in settled diff-rendered state");

    assert_eq!(
        streamed_column, 8,
        "the control state should reproduce the terminal's native tab stop: {streamed_row:?}"
    );
    assert_eq!(
        settled_column, streamed_column,
        "shell output moved while control returned to the diff renderer; before={streamed_row:?}, after={settled_row:?}"
    );

    shell.exit()
}

#[test]
fn clear_removes_existing_virtual_transcript() -> io::Result<()> {
    const MARKER: &str = "VISIBLE_BEFORE_CLEAR";

    let mut shell = RenderV2Pty::start()?;
    let marker_offset = shell.transcript_len();
    shell.write("printf 'VISIBLE_BEFORE_CLEAR\\n'\r")?;
    let before_clear = shell.wait_until(|bytes| {
        bytes
            .get(marker_offset..)
            .is_some_and(|tail| find_bytes(tail, MARKER.as_bytes()).is_some())
            && settled_prompt_is_visible(bytes)
    })?;
    assert!(
        screen_text(&before_clear).contains(MARKER),
        "test fixture did not place its marker on screen"
    );

    let clear_offset = shell.transcript_len();
    shell.write("clear\r")?;
    let after_clear = shell.wait_until(|bytes| {
        bytes
            .get(clear_offset..)
            .is_some_and(|tail| find_bytes(tail, b"\x1b[2J").is_some())
            && settled_prompt_is_visible(bytes)
    })?;
    let screen = screen_text(&after_clear);

    assert!(
        !screen.contains(MARKER),
        "clear briefly cleared the physical terminal, but the next diff frame restored the old VirtualScreen transcript:\n{screen}"
    );

    shell.exit()
}
