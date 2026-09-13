use super::*;

#[test]
fn real_vim_round_trip_preserves_primary_history_and_saved_input() {
    if !std::process::Command::new("vim")
        .arg("--version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        eprintln!("vim is unavailable; real editor check skipped");
        return;
    }
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    let file = ui.directory.join("fixture-vim.txt");
    ui.write(&format!("printf 'BEFORE_VIM\\n'; vim -u NONE -U NONE -i NONE -n -N --cmd 'set t_RV= t_u7=' '{}'; printf 'AFTER_VIM\\n'\r", file.display()));
    ui.wait(|screen| screen.alternate_screen() && screen.contents().contains("fixture-vim.txt"));
    ui.write("iVIM_SAVED_TEXT");
    ui.wait(|screen| screen.contents().contains("VIM_SAVED_TEXT"));
    ui.resize(22, 80);
    ui.write("_AFTER_RESIZE\x1b:wq\r");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        !screen.alternate_screen()
            && screen
                .rows(0, screen.size().1)
                .any(|line| line.trim() == "AFTER_VIM")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    assert_eq!(
        fs::read_to_string(file).unwrap(),
        "VIM_SAVED_TEXT_AFTER_RESIZE\n"
    );
    ui.write("printf 'NEXT_AFTER_VIM\\n'\r");
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|line| line.trim() == "NEXT_AFTER_VIM")
    });
    let history = ui.history().join("\n");
    assert!(
        !history.contains("VIM_SAVED_TEXT"),
        "alternate screen leaked into history: {history}"
    );
    for marker in ["BEFORE_VIM", "AFTER_VIM", "NEXT_AFTER_VIM"] {
        assert_eq!(
            history.lines().filter(|line| line.trim() == marker).count(),
            1,
            "{marker}: {history}"
        );
    }
}

#[test]
fn late_reference_definition_updates_in_place_and_publishes_once_after_resize() {
    for outcome in ["finish", "fail", "cancel"] {
        let mut ui = UiPty::start();
        ui.command("append", "[**LINK_TITLE**][doc]");
        ui.wait(|screen| screen.contents().contains("[doc]"));
        ui.command("append", "\n\n[doc]: https://example.test/guide\n\n");
        ui.wait(|screen| {
            screen
                .contents()
                .contains("LINK_TITLE (https://example.test/guide)")
                && !screen.contents().contains("[doc]")
        });
        ui.command(
            "append",
            &(0..220)
                .map(|i| format!("REFERENCE_ROW_{i:03}\n\n"))
                .collect::<String>(),
        );
        ui.wait(|screen| screen.contents().contains("REFERENCE_ROW_219"));
        ui.write("KEPT_DRAFT");
        if outcome == "cancel" {
            ui.write("\x03");
        } else {
            ui.command(outcome, "");
        }
        ui.wait(|screen| {
            screen.contents().contains("KEPT_DRAFT")
                && !screen.contents().contains("Fixture waiting")
        });
        ui.resize(22, 80);
        ui.write(&"\x7f".repeat("KEPT_DRAFT".len()));
        ui.write("printf 'AFTER_REFERENCE\\n'\r");
        ui.wait(|screen| {
            let (row, _) = screen.cursor_position();
            screen
                .rows(0, screen.size().1)
                .any(|line| line.trim() == "AFTER_REFERENCE")
                && screen
                    .rows(0, screen.size().1)
                    .nth(row as usize)
                    .unwrap_or_default()
                    .trim_end()
                    .ends_with('>')
        });
        let history = ui.history().join("\n");
        assert_eq!(
            history.matches("LINK_TITLE").count(),
            1,
            "{outcome}: {history}"
        );
        assert_eq!(
            history.matches("https://example.test/guide").count(),
            1,
            "{outcome}: {history}"
        );
        assert!(
            !history.contains("[doc]"),
            "unresolved preview was published: {history}"
        );
        for i in 0..220 {
            let marker = format!("REFERENCE_ROW_{i:03}");
            assert_eq!(history.matches(&marker).count(), 1, "{outcome}: {marker}");
        }
    }
}

#[cfg(unix)]
#[test]
fn exit_after_large_markdown_cancel_finishes_publication_and_restores_terminal() {
    let mut ui = UiPty::start();
    let text = (0..60000)
        .map(|i| format!("**WORD_{i:04}** "))
        .collect::<String>()
        + "EXIT_TAIL";
    ui.command("append", &text);
    ui.wait(|screen| screen.contents().contains("EXIT_TAIL"));
    ui.write("\x03");
    ui.wait(|screen| {
        screen.contents().contains("interrupted") && !screen.contents().contains("Fixture waiting")
    });
    let started = Instant::now();
    ui.write("/exit\r");
    while ui.child.try_wait().unwrap().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "large layout exit exceeded 2 s"
        );
        thread::sleep(Duration::from_millis(5));
    }
    if let Some(reader) = ui.reader.take() {
        reader.join().unwrap();
    }
    eprintln!(
        "exit/publication after 890 KB Markdown cancellation: {:?}",
        started.elapsed()
    );
    let history = ui.history().join("\n");
    assert_eq!(history.matches("EXIT_TAIL").count(), 1, "{history}");
    assert_eq!(
        history
            .lines()
            .filter(|line| line.trim() == "[interrupted]")
            .count(),
        1,
        "{history}"
    );
    assert_eq!(
        history.matches("> /exit").count(),
        1,
        "exit editor was duplicated: {history}"
    );
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(ui.master.as_raw_fd().unwrap(), attributes.as_mut_ptr()) },
        0
    );
    let flags = unsafe { attributes.assume_init().c_lflag };
    assert_eq!(
        flags & (libc::ICANON | libc::ECHO | libc::ISIG),
        libc::ICANON | libc::ECHO | libc::ISIG
    );
}

#[test]
fn reasoning_markdown_tools_and_final_answer_keep_order_on_finish_failure_and_cancel() {
    for outcome in ["finish", "fail", "cancel"] {
        let mut ui = UiPty::start();
        ui.command("next", "reasoning");
        ui.command("append", "REASONING_MARKER");
        ui.wait(|screen| screen.contents().contains("REASONING_MARKER"));
        ui.command("next", "markdown");
        ui.command("append", "**ANSWER_BEFORE_TOOL**\n");
        ui.wait(|screen| screen.contents().contains("ANSWER_BEFORE_TOOL"));
        ui.command("next", "tool");
        ui.command("bytes", "\x1b[3");
        ui.command("bytes", "1mTOOL_PROGRESS_OLD");
        ui.wait(|screen| screen.contents().contains("TOOL_PROGRESS_OLD"));
        ui.resize(22, 80);
        ui.write("KEPT_DRAFT");
        ui.command("bytes", "\r\x1b[KTOOL_LINE_界\x1b[0m\n");
        ui.command("stderr", "STDERR_LINE\n");
        ui.wait(|screen| {
            let text = screen.contents();
            text.contains("TOOL_LINE_界")
                && text.contains("STDERR_LINE")
                && text.contains("KEPT_DRAFT")
                && !text.contains("TOOL_PROGRESS_OLD")
        });
        ui.command("next", "markdown");
        ui.command(
            "append",
            "**ANSWER_AFTER_TOOL**\n\n```rust\nlet FINAL_CODE = 1;",
        );
        ui.wait(|screen| screen.contents().contains("FINAL_CODE"));
        if outcome == "cancel" {
            ui.write("\x03");
        } else {
            ui.command(outcome, "");
        }
        ui.wait(|screen| {
            let text = screen.contents();
            text.contains("KEPT_DRAFT")
                && text.contains("FINAL_CODE")
                && !text.contains("Fixture waiting")
                && match outcome {
                    "cancel" => text.contains("interrupted"),
                    "fail" => text.contains("injected failure"),
                    _ => true,
                }
        });
        ui.write(&"\x7f".repeat("KEPT_DRAFT".len()));
        ui.write("printf 'RECOVERED_MIXED\\n'\r");
        ui.wait(|screen| {
            let (row, _) = screen.cursor_position();
            screen
                .rows(0, screen.size().1)
                .any(|line| line.trim() == "RECOVERED_MIXED")
                && screen
                    .rows(0, screen.size().1)
                    .nth(row as usize)
                    .unwrap_or_default()
                    .trim_end()
                    .ends_with('>')
        });
        let history = ui.history().join("\n");
        let mut previous = 0;
        for marker in [
            "REASONING_MARKER",
            "ANSWER_BEFORE_TOOL",
            "TOOL_LINE_界",
            "STDERR_LINE",
            "ANSWER_AFTER_TOOL",
            "FINAL_CODE",
        ] {
            assert_eq!(
                history.matches(marker).count(),
                1,
                "{outcome}: {marker}\n{history}"
            );
            let position = history.find(marker).unwrap();
            assert!(
                position >= previous,
                "{outcome}: {marker} out of order\n{history}"
            );
            previous = position;
        }
        assert!(
            !history.contains("TOOL_PROGRESS_OLD"),
            "{outcome}: {history}"
        );
        assert!(!history.contains("Fixture waiting"), "{outcome}: {history}");
        assert!(
            !history.contains("command not found"),
            "draft ran automatically: {history}"
        );
    }
}
