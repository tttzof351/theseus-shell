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
        let pair = native_pty_system()
            .openpty(SIZE)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_theseus"));
        command.cwd(cwd);
        command.env("HOME", &home);
        command.env("USER", "tester");
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
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
    let command_offset = shell.transcript_len();
    shell.write("/ask run interruption fixture\r")?;
    shell.wait_until(|bytes| {
        bytes
            .get(command_offset..)
            .is_some_and(|tail| find_bytes(tail, MARKER.as_bytes()).is_some())
    })?;

    let interrupt_offset = shell.transcript_len();
    shell.write("\x03")?;
    let after_interrupt = shell.wait_until(|bytes| {
        let tail = bytes.get(interrupt_offset..).unwrap_or_default();
        find_bytes(tail, b"Agent tool execution interrupted.").is_some()
            && find_bytes(tail, b"\x1b[2J").is_some()
            && settled_prompt_is_visible(bytes)
    })?;
    let screen = screen_text(&after_interrupt);

    assert!(
        screen.contains(MARKER),
        "the recovery diff discarded Agent output that was visible before Ctrl+C:\n{screen}"
    );

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
