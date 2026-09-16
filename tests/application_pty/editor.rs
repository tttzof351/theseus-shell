use super::*;

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
        bytes.len() > clear_offset
            && !screen_text(bytes).contains(MARKER)
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
