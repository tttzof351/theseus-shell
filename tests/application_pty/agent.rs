use super::*;

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
