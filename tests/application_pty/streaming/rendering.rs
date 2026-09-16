use super::*;

#[test]
fn first_formatted_delta_precedes_done_and_keeps_short_footer_next_to_draft() -> io::Result<()> {
    for submission in ["/ask Что ты умеешь?\r", "/ask\rЧто ты умеешь?\r/end\r"]
    {
        let mut ui = Ui::start()?;
        ui.app.write("\x0c")?;
        ui.wait(|screen| !screen.contents().contains("Theseus shell wrapper"))?;
        ui.app.write(submission)?;
        ui.server.request()?;
        let first = ui.wait(|screen| spinner(screen).is_some())?;
        assert_minimal_progress(first.screen());
        let first_spinner = spinner(first.screen()).unwrap();
        let next =
            ui.wait(|screen| spinner(screen).is_some_and(|next| next.1 != first_spinner.1))?;
        assert_eq!(spinner(next.screen()).unwrap().0, first_spinner.0);
        assert_eq!(
            next.screen().cursor_position(),
            first.screen().cursor_position()
        );
        ui.server.headers()?;
        ui.server.bytes(": heartbeat\r\n\r\n")?;
        ui.app.write("NEXT_DRAFT")?;
        ui.server.content("**EARLY_MARKER**")?;
        // DONE is withheld until bold cells, footer and editable draft are visible.
        let early = ui.wait(|screen| {
            screen.contents().contains("EARLY_MARKER") && screen.contents().contains("NEXT_DRAFT")
        })?;
        assert_bold_marker(early.screen(), "EARLY_MARKER");
        assert_minimal_progress(early.screen());
        let rows = early.screen().rows(0, SIZE.cols).collect::<Vec<_>>();
        let answer = rows
            .iter()
            .position(|row| row.contains("EARLY_MARKER"))
            .unwrap();
        assert_eq!(spinner(early.screen()).unwrap().0, answer + 1, "{rows:?}");
        assert_eq!(early.screen().cursor_position().0 as usize, answer + 2);
        assert!(rows[answer + 3..].iter().all(|row| row.is_empty()));
        ui.server
            .event(json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}}))?;
        ui.server.finish("stop")?;
        ui.wait(|screen| {
            screen.contents().contains("EARLY_MARKER")
                && screen.contents().contains("NEXT_DRAFT")
                && spinner(screen).is_none()
        })?;
        ui.app.write("\x03")?;
        ui.app.write("printf 'AFTER_%s\\n' STREAM\r")?;
        ui.wait(|screen| {
            screen
                .rows(0, screen.size().1)
                .any(|row| row == "AFTER_STREAM")
        })?;
        let history = ui.history();
        assert_eq!(history.matches("EARLY_MARKER").count(), 1, "{history}");
        assert!(
            !history
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "{history}"
        );
        ui.app.exit()?;
    }
    Ok(())
}

#[test]
fn late_reference_code_table_and_unicode_publish_once_across_resize() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("/ask stream markdown\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("[**LINK_TITLE**][doc]")?;
    ui.wait(|screen| screen.contents().contains("[doc]"))?;
    ui.xterm_checkpoint("unresolved-reference");
    ui.server.content("\n\n[doc]: https://example.test/guide\n\n| Key | Value |\n| --- | --- |\n| TABLE_界 | Привет |\n")?;
    ui.wait(|screen| {
        screen
            .contents()
            .contains("LINK_TITLE (https://example.test/guide)")
            && screen.contents().contains("TABLE_界")
            && !screen.contents().contains("[doc]")
    })?;
    ui.xterm_checkpoint("resolved-reference");
    ui.server
        .content("| TABLE_🙂 | мир |\n\n```rust\nlet CODE_MARKER = \"Привет界🙂\";")?;
    ui.wait(|screen| screen.contents().contains("CODE_MARKER"))?;
    ui.resize(18, 60)?;
    ui.app.write("RESIZE_DRAFT")?;
    ui.wait(|screen| screen.contents().contains("RESIZE_DRAFT"))?;
    let data = format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"index":0,"delta":{"content":"\n```\n\n**UNICODE_界🙂**"}}]})
    );
    let boundary = data.find('界').unwrap() + 1;
    ui.server.bytes(&data.as_bytes()[..boundary])?;
    ui.server.bytes(&data.as_bytes()[boundary..])?;
    ui.wait(|screen| screen.contents().contains("UNICODE_界🙂"))?;
    ui.xterm_checkpoint("narrow-preview");
    ui.server.finish("stop")?;
    ui.wait(|screen| spinner(screen).is_none())?;
    ui.resize(18, 90)?;
    ui.app.write("\x03printf 'AFTER_%s\\n' MARKDOWN\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_MARKDOWN")
    })?;
    let history = ui.history();
    for marker in [
        "LINK_TITLE",
        "https://example.test/guide",
        "TABLE_界",
        "TABLE_🙂",
        "CODE_MARKER",
        "UNICODE_界🙂",
    ] {
        assert_eq!(history.matches(marker).count(), 1, "{marker}: {history}");
    }
    assert!(!history.contains("[doc]"), "{history}");
    ui.export_xterm("markdown-resize")?;
    ui.app.exit()
}

#[test]
fn long_bash_output_then_streamed_table_publish_once_without_preview_in_history() -> io::Result<()>
{
    let mut ui = Ui::start()?;
    ui.app.write("/ask list files\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    // More than a publication frame (128 rows), followed by a mutable table
    // taller than the viewport. Neither command text contains an output marker.
    ui.server.delta(tool_delta(
        "i=0; while [ $i -lt 180 ]; do printf 'BASH_%s_%03d\\n' ROW $i; i=$((i+1)); done",
    ))?;
    ui.server.finish("tool_calls")?;
    ui.server.request()?;
    ui.wait(|screen| screen.contents().contains("BASH_ROW_179"))?;
    ui.xterm_checkpoint("tool-tail");
    ui.server.headers()?;
    ui.server.delta(json!({"reasoning":"TABLE_REASONING"}))?;
    ui.server
        .content("TABLE_INTRO\n\n| File | Bytes |\n| --- | --- |\n")?;
    ui.app.write("KEPT_DRAFT")?;
    for i in 0..36 {
        ui.server
            .content(&format!("| TABLE_ROW_{i:03} | {} |\n", 1000 + i))?;
        ui.wait(|screen| {
            screen.contents().contains(&format!("TABLE_ROW_{i:03}"))
                && screen.contents().contains("KEPT_DRAFT")
                && spinner(screen).is_some()
        })?;
        if matches!(i, 18 | 35) {
            ui.xterm_checkpoint(&format!("table-preview-{i}"));
        }
    }
    ui.server.content("\nTABLE_SUMMARY")?;
    // Finish only after the complete mutable preview has been rendered. The
    // spinner may disappear before that preview's stable layout is installed.
    ui.wait(|screen| screen.contents().contains("TABLE_SUMMARY") && spinner(screen).is_some())?;
    ui.server.finish("stop")?;
    let markers = (0..180)
        .map(|i| format!("BASH_ROW_{i:03}"))
        .chain(["TABLE_REASONING".to_string(), "TABLE_INTRO".to_string()])
        .chain((0..36).map(|i| format!("TABLE_ROW_{i:03}")))
        .chain(["TABLE_SUMMARY".to_string()])
        .collect::<Vec<_>>();
    // Backend completion removes the spinner before the layout worker marks
    // the answer stable. Wait for actual publication without a shell command
    // forcing prepare_document(), or this checkpoint can still be a preview.
    ui.wait(|screen| {
        if !screen.contents().contains("TABLE_SUMMARY")
            || spinner(screen).is_some()
            || screen.hide_cursor()
        {
            return false;
        }
        let normal = normal_buffer_text(screen);
        markers.iter().all(|marker| normal.contains(marker))
    })?;
    ui.xterm_checkpoint("table-complete");
    // Grow native history again after completion, exposing a leaked preview
    // even if it would otherwise remain hidden above the live viewport.
    ui.app.write(&"\x7f".repeat("KEPT_DRAFT".len()))?;
    ui.app.write("printf 'AFTER_%s\\n' TABLE\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_TABLE")
    })?;
    assert_eq!(
        terminal::scroll_up_commands(&ui.app.transcript()),
        0,
        "CSI S discards published rows in xterm.js instead of saving them to scrollback"
    );
    for scroll_on_erase in [false, true] {
        let history = ui.history_with_scroll_on_erase(scroll_on_erase);
        let mut previous = 0;
        for marker in markers.iter().map(String::as_str).chain(["AFTER_TABLE"]) {
            assert_eq!(
                history.matches(marker).count(),
                1,
                "{marker}, scroll_on_erase={scroll_on_erase}: {history}"
            );
            let position = history.find(marker).unwrap();
            assert!(position >= previous, "{marker}: {history}");
            previous = position;
        }
        assert!(!history.contains("KEPT_DRAFT"), "{history}");
        assert!(
            !history
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "{history}"
        );
    }
    ui.export_xterm("long-bash-table")?;
    ui.app.exit()
}

#[cfg(unix)]
#[test]
fn large_active_sse_markdown_keeps_input_resize_cancel_and_exit_budgets() -> io::Result<()> {
    for (kind, source, expected_size) in [
        (
            "paragraph",
            (0..60000)
                .map(|i| format!("**WORD_{i:04}** "))
                .collect::<String>()
                + "EXIT_TAIL",
            890_009,
        ),
        (
            "fence",
            "```rust\n".to_owned()
                + &(0..16000)
                    .map(|i| format!("let SOURCE_{i:04} = \"Привет 界\";\n"))
                    .collect::<String>()
                + "// EXIT_TAIL",
            614_020,
        ),
    ] {
        assert_eq!(source.len(), expected_size);
        let mut ui = Ui::start()?;
        ui.resize(18, 60)?;
        ui.app.write("/ask large streaming fixture\r")?;
        ui.server.request()?;
        ui.server.headers()?;
        ui.server.content(&source)?;
        ui.wait(|screen| screen.contents().contains("EXIT_TAIL"))?;
        let started = Instant::now();
        ui.app.write("LARGE_DRAFT")?;
        ui.wait(|screen| screen.contents().contains("LARGE_DRAFT"))?;
        let input = started.elapsed();
        assert!(
            input <= Duration::from_millis(250),
            "{kind}: visible input {input:?}"
        );
        let started = Instant::now();
        ui.resize(18, 90)?;
        ui.app.write("_RESIZED")?;
        ui.wait(|screen| {
            screen.contents().contains("LARGE_DRAFT_RESIZED") && screen.cursor_position().1 > 21
        })?;
        let resize = started.elapsed();
        assert!(
            resize <= Duration::from_millis(250),
            "{kind}: visible resize {resize:?}"
        );
        let started = Instant::now();
        ui.app.write("\x03")?;
        ui.wait(|screen| {
            screen.contents().contains("interrupted")
                && screen.contents().contains("LARGE_DRAFT_RESIZED")
                && spinner(screen).is_none()
        })?;
        let cancel = started.elapsed();
        assert!(
            cancel <= Duration::from_millis(250),
            "{kind}: visible cancel {cancel:?}"
        );
        ui.server.action(Action::ExpectClosed)?;
        ui.app.write("\x03")?;
        let started = Instant::now();
        ui.app.write("/exit\r")?;
        while ui.app.child.try_wait()?.is_none() {
            assert!(
                started.elapsed() <= Duration::from_secs(2),
                "{kind}: exit/publication exceeded 2 s"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let exit = started.elapsed();
        eprintln!(
            "SSE {kind} {expected_size} bytes: input={input:?}, resize={resize:?}, cancel={cancel:?}, exit={exit:?}"
        );
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(ui.app.master.as_raw_fd().unwrap(), attributes.as_mut_ptr()) },
            0
        );
        let flags = unsafe { attributes.assume_init().c_lflag };
        assert_eq!(
            flags & (libc::ICANON | libc::ECHO | libc::ISIG),
            libc::ICANON | libc::ECHO | libc::ISIG
        );
        let history = ui.history();
        assert_eq!(history.matches("EXIT_TAIL").count(), 1);
        assert_eq!(
            history
                .lines()
                .filter(|line| line.trim() == "[interrupted]")
                .count(),
            1
        );
    }
    Ok(())
}

#[test]
fn a_single_long_code_line_does_not_stall_streaming_layout() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.resize(18, 60)?;
    ui.app.write("/ask unbroken code\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server
        .content(&("```text\n".to_owned() + &"x".repeat(614_002) + "\nLONG_CODE_TAIL"))?;
    ui.wait(|screen| screen.contents().contains("LONG_CODE_TAIL"))?;
    ui.app.write("\x03")?;
    ui.server.action(Action::ExpectClosed)?;
    ui.wait(|screen| spinner(screen).is_none() && screen.contents().contains("interrupted"))?;
    ui.app.exit()
}

#[test]
fn late_reference_shrinks_footer_after_clear_and_background_resize() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("/ask footer resize\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    // Keep the real background layout worker active even after clearing the
    // visible prefix. The retained source is above its 128 KiB threshold.
    ui.server
        .content(&("**HIDDEN_ROW** word\n\n".repeat(8000) + "HIDDEN_TAIL"))?;
    ui.wait(|screen| screen.contents().contains("HIDDEN_TAIL"))?;
    ui.app.write("\x0cFOOTER_DRAFT")?;
    ui.wait(|screen| {
        screen.contents().contains("FOOTER_DRAFT") && !screen.contents().contains("HIDDEN_TAIL")
    })?;
    let reference = "long_reference_identifier_".repeat(7);
    ui.server
        .content(&format!("\n\n[**SHRINK_MARKER**][{reference}]"))?;
    ui.wait(|screen| screen.contents().contains("SHRINK_MARKER"))?;
    let started = Instant::now();
    ui.resize(18, 60)?;
    ui.app.write("_RESIZED")?;
    let before = ui.wait(|screen| {
        screen.contents().contains("FOOTER_DRAFT_RESIZED")
            && screen.contents().contains("SHRINK_MARKER")
    })?;
    assert!(started.elapsed() <= Duration::from_millis(250));
    let before_row = spinner(before.screen()).unwrap().0;
    assert_eq!(before.screen().cursor_position().0 as usize, before_row + 1);
    assert!(
        before_row < 16,
        "short cleared output was pushed to the bottom: {}",
        before.screen().contents()
    );
    ui.server.content(&format!("\n\n[{reference}]: x"))?;
    let after = ui.wait(|screen| {
        screen.contents().contains("SHRINK_MARKER (x)")
            && !screen.contents().contains("long_reference")
    })?;
    let after_row = spinner(after.screen()).unwrap().0;
    assert!(
        after_row < before_row,
        "footer did not follow the shrinking preview"
    );
    assert_eq!(after.screen().cursor_position().0 as usize, after_row + 1);
    assert!(
        after
            .screen()
            .rows(0, 60)
            .skip(after_row + 2)
            .all(|row| row.is_empty())
    );
    assert_minimal_progress(after.screen());
    ui.server.finish("stop")?;
    ui.wait(|screen| {
        spinner(screen).is_none() && screen.contents().contains("FOOTER_DRAFT_RESIZED")
    })?;
    ui.app.write("\x03printf 'AFTER_%s\\n' SHRINK\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_SHRINK")
    })?;
    let history = ui.history();
    assert_eq!(history.matches("SHRINK_MARKER").count(), 1);
    assert!(
        !history.contains("HIDDEN_ROW") && !history.contains("long_reference"),
        "cleared or superseded source was published"
    );
    ui.app.exit()
}
