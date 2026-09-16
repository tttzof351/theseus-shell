use super::*;

#[test]
fn late_reasoning_and_tools_keep_order_and_execute_once_only_after_done() -> io::Result<()> {
    let mut ui = Ui::start()?;
    let effect = ui.app.home.join("effect");
    ui.app.write("/ask ordered response\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("**CONTENT_BEFORE_TOOL**")?;
    ui.wait(|screen| screen.contents().contains("CONTENT_BEFORE_TOOL"))?;
    ui.server.delta(json!({"reasoning":"LATE_REASONING"}))?;
    let preview = ui.wait(|screen| screen.contents().contains("LATE_REASONING"))?;
    let text = preview.screen().contents();
    assert!(
        text.find("LATE_REASONING") < text.find("CONTENT_BEFORE_TOOL"),
        "{text}"
    );
    ui.server.delta(tool_delta(&format!(
        "printf x >> '{}'; printf 'TOOL_%s\\n' RESULT",
        effect.display()
    )))?;
    ui.server
        .event(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}))?;
    // Even complete JSON arguments and finish_reason are insufficient without DONE.
    ui.app.write("TOOL_DRAFT")?;
    ui.wait(|screen| screen.contents().contains("TOOL_DRAFT"))?;
    assert!(!effect.exists());
    assert!(!ui.history().contains("arguments"));
    ui.server.bytes("data: [DONE]\n\n")?;
    ui.server.action(Action::ExpectClosed)?;
    let continuation = ui.server.request()?;
    assert_eq!(fs::read_to_string(&effect)?, "x");
    let messages = continuation["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["tool_calls"].is_array())
        .unwrap();
    assert_eq!(assistant["reasoning"], "LATE_REASONING");
    assert_eq!(assistant["content"], "**CONTENT_BEFORE_TOOL**");
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        1
    );
    ui.wait(|screen| screen.contents().contains("TOOL_RESULT"))?;
    ui.server.headers()?;
    ui.server.content("**CONTENT_AFTER_TOOL**")?;
    ui.wait(|screen| screen.contents().contains("CONTENT_AFTER_TOOL"))?;
    ui.server.finish("stop")?;
    ui.wait(|screen| spinner(screen).is_none())?;
    let history = ui.history();
    let mut previous = 0;
    for marker in [
        "LATE_REASONING",
        "CONTENT_BEFORE_TOOL",
        "TOOL_RESULT",
        "CONTENT_AFTER_TOOL",
    ] {
        assert_eq!(history.matches(marker).count(), 1, "{history}");
        let position = history.find(marker).unwrap();
        assert!(position >= previous, "{history}");
        previous = position;
    }
    assert_eq!(fs::read_to_string(effect)?, "x");
    ui.app.write("\x03")?;
    ui.app.exit()
}

#[test]
fn tool_only_stream_never_executes_cancelled_truncated_or_invalid_calls() -> io::Result<()> {
    for outcome in ["cancel", "length", "invalid"] {
        let mut ui = Ui::start()?;
        let effect = ui.app.home.join("forbidden-effect");
        ui.app.write("/ask held tool\r")?;
        ui.server.request()?;
        ui.server.headers()?;
        let mut delta = tool_delta(&format!("printf forbidden > '{}'", effect.display()));
        if outcome == "invalid" {
            delta["tool_calls"][0]["function"]["arguments"] = json!("{\"command\":");
        }
        ui.server.delta(delta)?;
        let before = ui.wait(|screen| spinner(screen).is_some())?;
        assert_minimal_progress(before.screen());
        assert!(!effect.exists());
        assert!(!before.screen().contents().contains("forbidden-effect"));
        if outcome == "cancel" {
            ui.app.write("\x03")?;
            ui.server.action(Action::ExpectClosed)?;
        } else {
            ui.server.finish(if outcome == "length" {
                "length"
            } else {
                "tool_calls"
            })?;
        }
        ui.wait(|screen| {
            spinner(screen).is_none()
                && (screen.contents().contains("interrupted")
                    || screen.contents().contains("agent:"))
        })?;
        assert!(
            !effect.exists(),
            "{outcome} executed partial tool arguments"
        );
        assert!(!ui.history().contains("forbidden-effect"));
        ui.app.exit()?;
    }
    Ok(())
}

#[test]
fn sse_bash_preview_keeps_editor_prompt_style_before_and_after_tool_cancel() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("\x0c")?;
    let editor = ui.wait(|screen| !screen.contents().contains("Theseus shell wrapper"))?;
    let editor_row = editor.screen().cursor_position().0;
    ui.app.write("/ask bash preview\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    // Explicit spaces reproduce the trailing blank cells left when the
    // renderer overwrites the longer editor prompt with this output in CI.
    ui.server.delta(tool_delta(
        "printf 'BASH_%s            \\n' READY; exec sleep 30",
    ))?;
    ui.server.finish("tool_calls")?;
    let assert_style = |screen: &vt100::Screen| {
        let prefix = "tester theseus-shell> ";
        let row = screen
            .rows(0, screen.size().1)
            .position(|line| line.starts_with(&format!("{prefix}printf ")))
            .expect("agent preview") as u16;
        for column in 0..prefix.chars().count() as u16 {
            let expected = editor.screen().cell(editor_row, column).unwrap();
            let actual = screen.cell(row, column).unwrap();
            assert_eq!(actual.contents(), expected.contents());
            assert_eq!(actual.fgcolor(), expected.fgcolor(), "color at {column}");
            assert_eq!(actual.bold(), expected.bold(), "weight at {column}");
        }
        let marker = screen
            .cell(row, (prefix.chars().count() - 2) as u16)
            .unwrap();
        assert_eq!(marker.fgcolor(), vt100::Color::Default);
        assert!(!marker.bold());
    };
    let before = ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row.trim_end() == "BASH_READY")
    })?;
    assert_style(before.screen());
    ui.app.write("\x03")?;
    let after = ui.wait(|screen| {
        screen
            .contents()
            .contains("Agent tool execution interrupted")
    })?;
    assert_style(after.screen());
    assert!(after.screen().contents().contains("BASH_READY"));
    ui.app.exit()
}

#[test]
fn sse_shell_leases_stdin_and_real_vim_return_to_streaming_without_replay() -> io::Result<()> {
    assert!(
        ProcessCommand::new("vim")
            .arg("--version")
            .output()?
            .status
            .success(),
        "real Vim fixture requires Vim"
    );
    let mut ui = Ui::start()?;
    ui.app.write("/ask before leases\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("**SSE_BEFORE_LEASES**")?;
    ui.server.finish("stop")?;
    ui.wait(|screen| screen.contents().contains("SSE_BEFORE_LEASES") && spinner(screen).is_none())?;
    ui.app
        .write("read value; printf 'INPUT_%s\\n' \"$value\"\rIMMEDIATE\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "INPUT_IMMEDIATE")
    })?;
    ui.app.write(&format!(
        "printf 'WRAPPED_%s {} WRAPPED_%s\\n' START END\r",
        "x".repeat(125)
    ))?;
    ui.wait(|screen| {
        screen.contents().contains("WRAPPED_START") && screen.contents().contains("WRAPPED_END")
    })?;
    ui.resize(18, 60)?;
    let file = ui.app.home.join("sse-vim.txt");
    ui.app.write(&format!(
        "vim -u NONE -U NONE -i NONE -n -N --cmd 'set t_RV= t_u7=' '{}'\r",
        file.display()
    ))?;
    ui.wait(|screen| screen.alternate_screen() && screen.contents().contains("sse-vim.txt"))?;
    ui.app.write("iVIM_SSE_TEXT")?;
    ui.wait(|screen| screen.contents().contains("VIM_SSE_TEXT"))?;
    ui.xterm_checkpoint("vim-edit");
    ui.resize(18, 90)?;
    ui.app.write("_RESIZED\x1b:wq\r")?;
    ui.wait(|screen| {
        !screen.alternate_screen()
            && screen
                .rows(0, screen.size().1)
                .nth(screen.cursor_position().0 as usize)
                .is_some_and(|row| row.trim_end().ends_with('>'))
    })?;
    assert_eq!(fs::read_to_string(file)?, "VIM_SSE_TEXT_RESIZED\n");
    ui.xterm_checkpoint("after-vim");
    ui.app.write("printf 'AFTER_%s\\n' VIM\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_VIM")
    })?;
    ui.app.write("/ask after leases\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("**SSE_AFTER_LEASES**")?;
    let early = ui.wait(|screen| screen.contents().contains("SSE_AFTER_LEASES"))?;
    assert_bold_marker(early.screen(), "SSE_AFTER_LEASES");
    ui.server.finish("stop")?;
    ui.wait(|screen| spinner(screen).is_none())?;
    let history = ui.history();
    for marker in [
        "SSE_BEFORE_LEASES",
        "SSE_AFTER_LEASES",
        "INPUT_IMMEDIATE",
        "WRAPPED_START",
        "WRAPPED_END",
    ] {
        assert_eq!(history.matches(marker).count(), 1, "{marker}: {history}");
    }
    assert!(
        !history.contains("VIM_SSE_TEXT"),
        "alternate buffer leaked: {history}"
    );
    ui.export_xterm("vim-handoff")?;
    ui.app.exit()
}
