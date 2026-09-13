use std::{
    fs,
    io::{self, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
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
static TEMP_HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ApplicationPty {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    transcript: Arc<Mutex<Vec<u8>>>,
    home: PathBuf,
}

impl ApplicationPty {
    fn start() -> io::Result<Self> {
        let home = temp_home()?;
        Self::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))
    }

    fn start_with_home_and_cwd(home: PathBuf, cwd: &Path) -> io::Result<Self> {
        Self::start_with_shell(home, cwd, None)
    }

    fn start_with_shell(home: PathBuf, cwd: &Path, shell: Option<&Path>) -> io::Result<Self> {
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

impl Drop for ApplicationPty {
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
    let sequence = TEMP_HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "theseus-application-pty-{}-{nanos}-{sequence}",
        std::process::id(),
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
    screen_rows(bytes).join("\n")
}

#[test]
fn shell_handoff_preserves_input_sent_in_same_write_as_command_enter() -> io::Result<()> {
    let mut app = ApplicationPty::start()?;
    app.write("read value; printf 'RECEIVED=%s\\n' \"$value\"\rimmediate-stdin\r")?;
    app.wait_until(|bytes| {
        screen_rows(bytes)
            .iter()
            .any(|row| row == "RECEIVED=immediate-stdin")
            && settled_prompt_is_visible(bytes)
    })?;
    app.exit()
}

fn is_waiting_spinner(line: &str) -> bool {
    let mut chars = line.trim().chars();
    chars
        .next()
        .is_some_and(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
        && chars.next().is_none()
}

fn waiting_spinner_row(bytes: &[u8]) -> Option<usize> {
    screen_rows(bytes)
        .iter()
        .position(|line| is_waiting_spinner(line))
}

fn screen_rows(bytes: &[u8]) -> Vec<String> {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    parser.process(bytes);
    parser
        .screen()
        .rows(0, SIZE.cols)
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn terminal_history_rows(bytes: &[u8]) -> Vec<String> {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 200);
    parser.process(bytes);
    let screen = parser.screen_mut();
    screen.set_scrollback(usize::MAX);
    let scrollback = screen.scrollback();
    let mut rows = screen
        .rows(0, SIZE.cols)
        .map(|row| row.trim_end().to_string())
        .collect::<Vec<_>>();
    for offset in (0..scrollback).rev() {
        screen.set_scrollback(offset);
        if let Some(row) = screen.rows(0, SIZE.cols).last() {
            rows.push(row.trim_end().to_string());
        }
    }
    rows
}

fn run_git(repo: &Path, arguments: &[&str]) -> io::Result<()> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn git_branch_fixture() -> io::Result<(PathBuf, PathBuf)> {
    let home = temp_home()?;
    let repo = home.join("theseus-shell");
    let pager = home.join("git-branch-pager.sh");
    fs::create_dir_all(&repo)?;
    fs::write(
        &pager,
        "#!/bin/sh\nprintf '\\r\\033[K'\ncat\nprintf '\\r\\033[K'\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&pager, fs::Permissions::from_mode(0o755))?;
    }
    run_git(&repo, &["init", "-q"])?;
    run_git(&repo, &["config", "user.email", "application@example.test"])?;
    run_git(&repo, &["config", "user.name", "Application Test"])?;
    run_git(&repo, &["config", "color.ui", "always"])?;
    fs::write(repo.join("tracked.txt"), "fixture\n")?;
    run_git(&repo, &["add", "tracked.txt"])?;
    run_git(&repo, &["commit", "-qm", "fixture"])?;
    run_git(&repo, &["branch", "-m", "master"])?;
    run_git(&repo, &["branch", "exps"])?;
    run_git(&repo, &["checkout", "-q", "exps"])?;
    Ok((home, repo))
}

fn interrupted_agent_fixture() -> io::Result<(PathBuf, thread::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 8192];
        let _ = stream.read(&mut request).unwrap();
        let body = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_interrupted",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": "{\"command\":\"printf 'AGENT_%s_BEFORE_INTERRUPT\\\\n' OUTPUT; exec sleep 30\"}"
                                }
                            }
                        ]
                    }
                }
            ]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let home = temp_home()?;
    let config_dir = home.join(".theseus");
    fs::create_dir_all(&config_dir)?;
    let config = serde_json::json!({
        "llm_request_settings": {
            "base_url": format!("http://{address}/chat"),
            "retries": 1,
            "request_timeout_seconds": 30,
            "connect_timeout_seconds": 5,
            "body": {
                "model": "test/model",
                "tool_choice": "auto"
            },
            "header": {
                "Authorization": "Bearer test",
                "Content-Type": "application/json"
            }
        },
        "agent_settings": {
            "max_turns": 2,
            "max_tool_output_bytes": 32768,
            "max_tool_bash_bytes": 8192,
            "max_context_tokens": 200000,
            "max_resume_traj": 100,
            "build_in_tools": ["bash"],
            "system_prompt": ["test"]
        },
        "mcp_servers": {}
    });
    fs::write(
        config_dir.join("config.jsonc"),
        serde_json::to_vec_pretty(&config)?,
    )?;

    Ok((home, server))
}

struct HeldJsonResponse {
    ready: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
    server: Option<thread::JoinHandle<()>>,
}

impl Drop for HeldJsonResponse {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn held_json_fixture() -> io::Result<(PathBuf, HeldJsonResponse)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(WAIT_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(WAIT_TIMEOUT)).unwrap();
        let mut request = [0; 8192];
        if stream.read(&mut request).unwrap_or(0) == 0 {
            return;
        }
        let _ = ready_tx.send(());
        let _ = release_rx.recv_timeout(WAIT_TIMEOUT);
        let body = serde_json::json!({"choices":[{"message":{"role":"assistant","content":"**HELD_ANSWER**"},"finish_reason":"stop"}]}).to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    let home = temp_home()?;
    fs::create_dir_all(home.join(".theseus"))?;
    fs::write(
        home.join(".theseus/config.jsonc"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "llm_request_settings": {
                "base_url": format!("http://{address}/chat"), "retries": 1,
                "request_timeout_seconds": 30, "connect_timeout_seconds": 5,
                "body": {"model": "test/model"}, "header": {"Authorization": "Bearer test"}
            },
            "agent_settings": {"max_turns": 2, "max_tool_output_bytes": 32768, "max_tool_bash_bytes": 8192, "max_context_tokens": 200000, "max_resume_traj": 100, "build_in_tools": [], "system_prompt": ["test"]},
            "mcp_servers": {}
        }))?,
    )?;
    Ok((
        home,
        HeldJsonResponse {
            ready,
            release,
            server: Some(server),
        },
    ))
}

#[test]
fn managed_mcp_startup_and_discovery_keep_resize_draft_and_cancel_responsive() -> io::Result<()> {
    for phase in ["initialize", "tools/list"] {
        let home = temp_home()?;
        fs::create_dir_all(home.join(".theseus"))?;
        let marker = home.join("mcp-phase");
        fs::write(
            home.join(".theseus/config.jsonc"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "llm_request_settings": {
                "base_url": "http://127.0.0.1:1/chat", "retries": 1,
                "request_timeout_seconds": 30, "connect_timeout_seconds": 5,
                "body": {"model": "test/model"}, "header": {"Authorization": "Bearer fixture"}
            },
            "agent_settings": {"max_turns": 2, "max_tool_output_bytes": 32768, "max_tool_bash_bytes": 8192, "max_context_tokens": 200000, "max_resume_traj": 100, "build_in_tools": [], "system_prompt": ["test"]},
                "mcp_servers": {"held": {
                "command": "python3",
                    "args": [format!("{}/tests/fixtures/stalled_mcp.py", env!("CARGO_MANIFEST_DIR")), phase, marker],
                "tools": ["*"], "timeout": 60000
                }}
            }))?,
        )?;
        let mut app =
            ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
        app.write("/ask held discovery\r")?;
        app.wait_until(|bytes| {
            marker.exists() && screen_text(bytes).contains("Discovering MCP tools")
        })?;
        let pid: i32 = fs::read_to_string(&marker)?
            .parse()
            .map_err(io::Error::other)?;
        let resize_at = app.transcript_len();
        let current_screen = |bytes: &[u8]| {
            let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
            let boundary = resize_at.min(bytes.len());
            parser.process(&bytes[..boundary]);
            parser.screen_mut().set_size(18, 60);
            parser.process(&bytes[boundary..]);
            parser
        };
        let started = Instant::now();
        app.master
            .resize(PtySize {
                rows: 18,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;
        app.write("MCP_NEXT_DRAFT")?;
        app.wait_until(|bytes| {
            current_screen(bytes)
                .screen()
                .contents()
                .contains("MCP_NEXT_DRAFT")
        })?;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "MCP {phase} resize/input: {:?}",
            started.elapsed()
        );
        let started = Instant::now();
        app.write("\x03")?;
        app.wait_until(|bytes| {
            let text = current_screen(bytes).screen().contents();
            text.contains("Agent request interrupted") && text.contains("MCP_NEXT_DRAFT")
        })?;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "MCP {phase} cancellation: {:?}",
            started.elapsed()
        );
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "MCP peer survived UI acknowledgement"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        app.write(&"\x7f".repeat("MCP_NEXT_DRAFT".len()))?;
        app.write("printf 'MCP_RECOVERED\\n'\r")?;
        app.wait_until(|bytes| {
            current_screen(bytes)
                .screen()
                .rows(0, 60)
                .any(|line| line.trim() == "MCP_RECOVERED")
        })?;
        app.exit()?;
    }
    Ok(())
}

#[test]
fn managed_waiting_spinner_animates_in_place_and_does_not_enter_history() -> io::Result<()> {
    let (home, held) = held_json_fixture()?;
    let mut app =
        ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
    app.write("/ask held question\r")?;
    held.ready
        .recv_timeout(WAIT_TIMEOUT)
        .map_err(io::Error::other)?;
    let snapshot = |bytes: &[u8]| {
        let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
        parser.process(bytes);
        parser
            .screen()
            .rows(0, SIZE.cols)
            .enumerate()
            .find_map(|(row, text)| {
                let glyph = text.chars().next()?;
                is_waiting_spinner(&text).then_some((row, glyph, parser.screen().cursor_position()))
            })
    };
    let bytes = app.wait_until(|bytes| snapshot(bytes).is_some())?;
    let first = snapshot(&bytes).unwrap();
    let visible = screen_text(&bytes);
    assert!(
        !visible.contains("Waiting for response") && !visible.contains("attempt 1/"),
        "{visible}"
    );
    // The HTTP fixture is still held: observing another frame proves that the
    // UI animates without a backend event or completion waking it up.
    let bytes = app.wait_until(|bytes| snapshot(bytes).is_some_and(|next| next.1 != first.1))?;
    let second = snapshot(&bytes).unwrap();
    assert_eq!(second.0, first.0, "animation moved the status row");
    assert_eq!(second.2, first.2, "animation moved the editor cursor");
    held.release.send(()).map_err(io::Error::other)?;
    app.wait_until(|bytes| {
        let text = screen_text(bytes);
        text.contains("HELD_ANSWER") && waiting_spinner_row(bytes).is_none()
    })?;
    app.write("printf 'AFTER_SPINNER\\n'\r")?;
    let bytes =
        app.wait_until(|bytes| screen_rows(bytes).iter().any(|row| row == "AFTER_SPINNER"))?;
    let history = terminal_history_rows(&bytes).join("\n");
    assert!(
        !history
            .chars()
            .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
        "{history}"
    );
    assert!(!history.contains("Waiting for response"), "{history}");
    app.exit()
}

#[test]
fn submitting_short_request_keeps_status_and_draft_next_to_the_prompt() -> io::Result<()> {
    for submission in ["/ask Что ты умеешь?\r", "/ask\rЧто ты умеешь?\r/end\r"]
    {
        let (home, held) = held_json_fixture()?;
        let mut app =
            ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
        app.write("\x0c")?;
        app.wait_until(|bytes| !screen_text(bytes).contains("Theseus shell wrapper"))?;
        app.write(submission)?;
        held.ready
            .recv_timeout(WAIT_TIMEOUT)
            .map_err(io::Error::other)?;
        let bytes = app.wait_until(|bytes| waiting_spinner_row(bytes).is_some())?;
        let rows = screen_rows(&bytes);
        let question = rows
            .iter()
            .position(|line| line.contains("Что ты умеешь?"))
            .unwrap();
        let status = rows
            .iter()
            .position(|line| is_waiting_spinner(line))
            .unwrap();
        assert_eq!(
            status,
            question + 1,
            "status jumped away from submitted text: {rows:?}"
        );
        assert!(rows[status + 1].trim_end().ends_with('>'), "{rows:?}");
        assert!(
            rows[status + 2..].iter().all(|line| line.trim().is_empty()),
            "{rows:?}"
        );
        app.write("NEXT_DRAFT")?;
        let bytes = app.wait_until(|bytes| screen_text(bytes).contains("NEXT_DRAFT"))?;
        let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
        parser.process(&bytes);
        assert_eq!(usize::from(parser.screen().cursor_position().0), status + 1);
        held.release.send(()).map_err(io::Error::other)?;
        app.wait_until(|bytes| {
            let text = screen_text(bytes);
            text.contains("HELD_ANSWER")
                && text.contains("NEXT_DRAFT")
                && waiting_spinner_row(bytes).is_none()
        })?;
        app.write("\x03")?;
        app.exit()?;
    }
    Ok(())
}

#[test]
fn managed_request_keeps_pasted_draft_and_requires_explicit_submit_after_completion()
-> io::Result<()> {
    let (home, held) = held_json_fixture()?;
    let marker = home.join("draft-executed");
    let mut app =
        ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
    app.write("/ask held question\r")?;
    held.ready
        .recv_timeout(WAIT_TIMEOUT)
        .map_err(io::Error::other)?;
    app.wait_until(|bytes| waiting_spinner_row(bytes).is_some())?;
    let draft = format!("printf done > '{}'", marker.display());
    app.write(&format!("\x1b[200~{draft}\n\x1b[201~\r"))?;
    app.wait_until(|bytes| screen_text(bytes).contains("draft-executed"))?;
    assert!(!marker.exists(), "busy paste or Enter executed a command");
    held.release.send(()).map_err(io::Error::other)?;
    app.wait_until(|bytes| {
        let text = screen_text(bytes);
        text.contains("HELD_ANSWER")
            && text.contains("draft-executed")
            && waiting_spinner_row(bytes).is_none()
    })?;
    assert!(
        !marker.exists(),
        "completion executed the draft automatically"
    );
    app.write("\r")?;
    app.wait_until(|bytes| marker.exists() && settled_prompt_is_visible(bytes))?;
    assert_eq!(fs::read_to_string(marker)?, "done");
    app.exit()
}

#[test]
fn managed_cancel_keeps_next_draft_and_clear_does_not_cancel_request() -> io::Result<()> {
    let (home, held) = held_json_fixture()?;
    let mut app =
        ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
    app.write("/ask held question\r")?;
    held.ready
        .recv_timeout(WAIT_TIMEOUT)
        .map_err(io::Error::other)?;
    app.write("NEXT_DRAFT\x0c")?;
    app.wait_until(|bytes| {
        let text = screen_text(bytes);
        text.contains("NEXT_DRAFT")
            && waiting_spinner_row(bytes).is_some()
            && !text.contains("held question")
    })?;
    app.write("\x03")?;
    app.wait_until(|bytes| {
        let text = screen_text(bytes);
        text.contains("Agent request interrupted")
            && text.contains("NEXT_DRAFT")
            && waiting_spinner_row(bytes).is_none()
    })?;
    // Cancel the editor draft before using /exit.
    app.write("\x03")?;
    app.exit()
}

fn wait_cli_exit(
    mut child: std::process::Child,
    deadline: Duration,
) -> io::Result<std::process::Output> {
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "CLI failed to stop: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn headless_and_piped_commands_emit_answer_once_without_ansi() -> io::Result<()> {
    for args in [vec!["-p", "held question"], vec!["/ask", "held question"]] {
        let (home, held) = held_json_fixture()?;
        let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
            .args(args)
            .env("HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        held.ready
            .recv_timeout(WAIT_TIMEOUT)
            .map_err(io::Error::other)?;
        held.release.send(()).map_err(io::Error::other)?;
        let output = wait_cli_exit(child, Duration::from_secs(2))?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.contains(&0x1b));
        assert!(!output.stderr.contains(&0x1b));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .matches("HELD_ANSWER")
                .count(),
            1
        );
        fs::remove_dir_all(home)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn headless_sigint_cancels_waiting_http_and_exits_130() -> io::Result<()> {
    let (home, held) = held_json_fixture()?;
    let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["-p", "held question"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    held.ready
        .recv_timeout(WAIT_TIMEOUT)
        .map_err(io::Error::other)?;
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let output = wait_cli_exit(child, Duration::from_secs(2))?;
    assert_eq!(output.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(home)?;
    Ok(())
}

#[test]
fn closing_headless_stdout_cancels_tool_producer_without_hanging() -> io::Result<()> {
    let (home, server) = interrupted_agent_fixture()?;
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["-p", "run tool"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    drop(child.stdout.take());
    let output = wait_cli_exit(child, Duration::from_secs(3))?;
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    fs::remove_dir_all(home)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn shell_is_ready_before_first_prompt_and_reused_for_commands() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let home = temp_home()?;
    let startup_log = home.join("shell-starts");
    let wrapper = home.join("test-shell");
    fs::write(
        &wrapper,
        "#!/bin/sh\nsleep 0.2\nprintf 'started\\n' >> \"$HOME/shell-starts\"\nexec /bin/sh -i\n",
    )?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
    let mut shell = ApplicationPty::start_with_shell(
        home,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        Some(&wrapper),
    )?;

    // start_with_shell returns as soon as the initial prompt is visible.
    // Check readiness directly instead of asserting a machine-dependent latency.
    assert_eq!(fs::read_to_string(&startup_log)?, "started\n");
    for marker in ["FIRST_READY", "SECOND_READY"] {
        let offset = shell.transcript_len();
        shell.write(&format!(
            "printf '%s%s\\n' '{}' '{}'\r",
            &marker[..5],
            &marker[5..]
        ))?;
        shell.wait_until(|bytes| {
            find_bytes(bytes.get(offset..).unwrap_or_default(), marker.as_bytes()).is_some()
                && settled_prompt_is_visible(bytes)
        })?;
    }
    assert_eq!(fs::read_to_string(&startup_log)?, "started\n");
    shell.exit()
}

#[test]
fn multiline_history_keeps_one_prompt_without_end_on_submit_or_cancel() -> io::Result<()> {
    for (command, kind, mode, text) in [
        ("/ask", "agent", "multi_line_ask", "Explain FIRST\n\nSECOND"),
        (
            "/shell",
            "shell",
            "multi_line_shell",
            "printf FIRST\n\nfalse",
        ),
    ] {
        for finish in ["\r", "\x03"] {
            let mut shell = ApplicationPty::start()?;
            let history_path = shell.home.join(".theseus/persist/history_command_v2.json");
            let expected = serde_json::json!([{"text": text, "kind": kind, "mode": mode}]);
            shell.write(&format!("{command}\r"))?;
            shell.write(&format!("\x1b[200~{text}\n/end\x1b[201~"))?;
            shell.wait_until(|bytes| {
                screen_rows(bytes)
                    .iter()
                    .rfind(|row| !row.is_empty())
                    .is_some_and(|row| row.trim() == "· /end")
            })?;
            let history: serde_json::Value = serde_json::from_slice(&fs::read(&history_path)?)?;
            assert_eq!(history, expected, "draft for {command}");

            shell.write(finish)?;
            shell.wait_until(settled_prompt_is_visible)?;
            let history: serde_json::Value = serde_json::from_slice(&fs::read(&history_path)?)?;
            assert_eq!(history, expected, "finished history for {command}");

            // Up restores the prompt body; Down returns to the empty input.
            shell.write("\x1b[A")?;
            let last_line = format!("· {}", text.lines().last().unwrap());
            shell.wait_until(|bytes| {
                screen_rows(bytes)
                    .iter()
                    .rfind(|row| !row.is_empty())
                    .is_some_and(|row| row.trim() == last_line)
            })?;
            shell.write("\x1b[B")?;
            shell.wait_until(settled_prompt_is_visible)?;
            shell.exit()?;
        }
    }
    Ok(())
}

#[test]
fn streamed_shell_output_does_not_move_when_diff_renderer_resumes() -> io::Result<()> {
    let mut shell = ApplicationPty::start()?;
    let offset = shell.transcript_len();

    // The external terminal expands this tab using its native eight-column tab
    // stops. Once the command finishes, the application rebuilds the same visible
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
fn interrupted_agent_preserves_output_written_before_ctrl_c() -> io::Result<()> {
    const MARKER: &str = "AGENT_OUTPUT_BEFORE_INTERRUPT";

    let (home, server) = interrupted_agent_fixture()?;
    let mut shell =
        ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
    shell.write("\x0c")?;
    let before = shell.wait_until(|bytes| {
        !screen_text(bytes).contains("Theseus shell wrapper") && settled_prompt_is_visible(bytes)
    })?;
    let mut prompt_screen = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    prompt_screen.process(&before);
    let prompt_row = prompt_screen.screen().cursor_position().0;
    let command_offset = shell.transcript_len();
    shell.write("/ask run interruption fixture\r")?;
    let preview = shell.wait_until(|bytes| {
        bytes
            .get(command_offset..)
            .is_some_and(|tail| find_bytes(tail, MARKER.as_bytes()).is_some())
    })?;

    let assert_prompt_style = |bytes: &[u8]| {
        let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
        parser.process(bytes);
        let prefix = "tester theseus-shell> ";
        let row = parser
            .screen()
            .rows(0, SIZE.cols)
            .position(|line| line.starts_with(&format!("{prefix}printf ")))
            .expect("agent bash command preview is visible") as u16;
        for column in 0..prefix.chars().count() as u16 {
            let editor_cell = prompt_screen.screen().cell(prompt_row, column).unwrap();
            let tool_cell = parser.screen().cell(row, column).unwrap();
            assert_eq!(tool_cell.contents(), editor_cell.contents());
            assert_eq!(
                tool_cell.fgcolor(),
                editor_cell.fgcolor(),
                "prompt color at column {column}"
            );
            assert_eq!(
                tool_cell.bold(),
                editor_cell.bold(),
                "prompt weight at column {column}"
            );
        }
        let marker = parser
            .screen()
            .cell(row, (prefix.chars().count() - 2) as u16)
            .unwrap();
        assert!(!marker.bold(), "prompt style leaked onto >");
        assert_eq!(marker.fgcolor(), vt100::Color::Default);
    };
    assert_prompt_style(&preview);

    let interrupt_offset = shell.transcript_len();
    shell.write("\x03")?;
    let after_interrupt = shell.wait_until(|bytes| {
        let tail = bytes.get(interrupt_offset..).unwrap_or_default();
        find_bytes(tail, b"Agent tool execution interrupted.").is_some()
            && settled_prompt_is_visible(bytes)
    })?;
    let screen = screen_text(&after_interrupt);

    assert!(
        screen.contains(MARKER),
        "the recovery diff discarded Agent output that was visible before Ctrl+C:\n{screen}"
    );
    assert_prompt_style(&after_interrupt);

    server.join().unwrap();
    shell.exit()
}

#[test]
fn ctrl_l_clears_virtual_transcript_and_preserves_current_input() -> io::Result<()> {
    const MARKER: &str = "VISIBLE_BEFORE_CTRL_L";
    const DRAFT: &str = "kept-draft";

    let mut shell = ApplicationPty::start()?;
    let marker_offset = shell.transcript_len();
    shell.write("printf 'VISIBLE_BEFORE_CTRL_L\\n'\r")?;
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

    shell.write(DRAFT)?;
    shell.wait_until(|bytes| {
        screen_text(bytes).contains(&format!("tester theseus-shell> {DRAFT}"))
    })?;

    let clear_offset = shell.transcript_len();
    shell.write("\x0c")?;
    let after_clear = shell.wait_until(|bytes| {
        bytes
            .get(clear_offset..)
            .is_some_and(|tail| find_bytes(tail, b"\x1b[2J").is_some())
            && settled_prompt_is_visible(bytes)
    })?;
    let screen = screen_text(&after_clear);

    assert!(
        !screen.contains(MARKER),
        "Ctrl+L cleared the physical terminal but the redraw restored the old VirtualScreen transcript:\n{screen}"
    );
    assert!(
        screen.contains(&format!("tester theseus-shell> {DRAFT}")),
        "Ctrl+L lost the current editor input:\n{screen}"
    );

    shell.exit()
}

#[test]
fn clear_removes_existing_virtual_transcript() -> io::Result<()> {
    const MARKER: &str = "VISIBLE_BEFORE_CLEAR";

    let mut shell = ApplicationPty::start()?;
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

#[test]
fn git_branch_output_has_no_extra_rows_around_streamed_output() -> io::Result<()> {
    const COMMAND: &str = "git branch";

    let (home, repo) = git_branch_fixture()?;
    let mut shell = ApplicationPty::start_with_home_and_cwd(home, &repo)?;
    let fill_offset = shell.transcript_len();
    shell.write(
        "printf 'FILL01\\nFILL02\\nFILL03\\nFILL04\\nFILL05\\nFILL06\\nFILL07\\nFILL08\\n'\r",
    )?;
    shell.wait_until(|bytes| {
        bytes
            .get(fill_offset..)
            .is_some_and(|tail| find_bytes(tail, b"FILL08").is_some())
            && settled_prompt_is_visible(bytes)
    })?;

    let offset = shell.transcript_len();
    shell.write(&format!("{COMMAND}\r"))?;
    let transcript = shell.wait_until(|bytes| {
        let rows = screen_rows(bytes);
        bytes.get(offset..).is_some_and(|tail| {
            find_bytes(tail, b"exps").is_some() && find_bytes(tail, b"master").is_some()
        }) && rows.iter().any(|row| row.trim() == "* exps")
            && rows.iter().any(|row| row.trim() == "master")
            && settled_prompt_is_visible(bytes)
    })?;
    let tail = &transcript[offset..];
    let streamed_output_end = find_bytes(tail, b"master").expect("streamed branch output");
    let renderer_resume = streamed_output_end
        + find_bytes(&tail[streamed_output_end..], b"\x1b[2J")
            .expect("renderer recovery frame after streamed branch output");
    let streamed_rows = terminal_history_rows(&transcript[..offset + renderer_resume]);
    let streamed_command_row = streamed_rows
        .iter()
        .position(|row| row.contains(COMMAND))
        .expect("submitted git branch command in streamed frame");
    let streamed_active_branch_row = streamed_rows
        .iter()
        .position(|row| row.trim() == "* exps")
        .expect("active branch in streamed frame");
    let streamed_master_branch_row = streamed_rows
        .iter()
        .position(|row| row.trim() == "master")
        .expect("master branch in streamed frame");

    assert_eq!(
        [streamed_active_branch_row, streamed_master_branch_row],
        [streamed_command_row + 1, streamed_command_row + 2],
        "streamed git output introduced empty physical rows before the renderer resumed:\n{}",
        streamed_rows.join("\n")
    );

    let settled_rows = screen_rows(&transcript);
    let command_row = settled_rows
        .iter()
        .position(|row| row.contains(COMMAND))
        .expect("submitted git branch command");
    let active_branch_row = settled_rows
        .iter()
        .position(|row| row.trim() == "* exps")
        .expect("active branch output");
    let master_branch_row = settled_rows
        .iter()
        .position(|row| row.trim() == "master")
        .expect("master branch output");
    let next_prompt_row = settled_rows
        .iter()
        .position(|row| row == "tester theseus-shell>")
        .expect("next command prompt");

    assert_eq!(
        [active_branch_row, master_branch_row, next_prompt_row,],
        [command_row + 1, command_row + 2, command_row + 3],
        "settled git output contains empty physical rows:\n{}",
        settled_rows.join("\n")
    );

    shell.exit()
}
