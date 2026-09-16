use super::*;

#[test]
fn live_markdown_is_formatted_before_finish_and_final_block_is_not_duplicated() {
    let mut ui = UiPty::start();
    ui.command("append", "**FIRST_MARKER**\n\n```rust\nlet answer = 42;");
    ui.wait(|screen| {
        let (rows, cols) = screen.size();
        (0..rows).any(|row| {
            (0..cols).any(|col| {
                screen
                    .cell(row, col)
                    .is_some_and(|cell| cell.contents() == "F" && cell.bold())
            })
        }) && screen.contents().contains("let answer = 42;")
    });
    // The producer is still blocked waiting for the next command file. Seeing
    // styled cells above is the handshake proof of early Markdown presentation.
    ui.command("append", "\n```\n\nLAST_MARKER");
    ui.wait(|screen| screen.contents().contains("LAST_MARKER"));
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    let history = ui.history().join("\n");
    assert_eq!(history.matches("FIRST_MARKER").count(), 1, "{history}");
    assert_eq!(history.matches("LAST_MARKER").count(), 1, "{history}");
}

#[test]
fn replacing_long_preview_then_shrinking_and_finishing_does_not_publish_drafts() {
    let mut ui = UiPty::start();
    ui.command(
        "replace",
        &(0..65)
            .map(|i| format!("DRAFT_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("DRAFT_64"));
    assert!(
        !ui.history()
            .iter()
            .take(20)
            .any(|line| line.contains("DRAFT_00"))
    );
    ui.resize(12, 40);
    ui.command("replace", "**SHORT_PREVIEW**");
    ui.wait(|screen| {
        screen.contents().contains("SHORT_PREVIEW") && !screen.contents().contains("DRAFT_64")
    });
    ui.command(
        "replace",
        &(0..45)
            .map(|i| format!("FINAL_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("FINAL_44"));
    ui.command("finish", "");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen.contents().contains("FINAL_44")
            && !screen.contents().contains("Fixture waiting")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    // Backend completion can precede the final background layout/publication.
    // Wait for all rows to reach history, then check that none were duplicated.
    let deadline = Instant::now() + TIMEOUT;
    let history = loop {
        let history = ui.history().join("\n");
        if (0..45).all(|i| history.contains(&format!("FINAL_{i:02}"))) {
            break history;
        }
        assert!(
            Instant::now() < deadline,
            "final rows were not published:\n{history}"
        );
        thread::sleep(Duration::from_millis(5));
    };
    for i in 0..45 {
        assert_eq!(
            history.matches(&format!("FINAL_{i:02}")).count(),
            1,
            "{history}"
        );
    }
    assert!(
        !history.contains("DRAFT_") && !history.contains("SHORT_PREVIEW"),
        "{history}"
    );
}

#[test]
fn failure_after_partial_markdown_keeps_prefix_and_editor_usable() {
    let mut ui = UiPty::start();
    ui.command("append", "**KEPT_PREFIX**");
    ui.wait(|screen| screen.contents().contains("KEPT_PREFIX"));
    ui.write("NEXT_DRAFT");
    ui.command("fail", "");
    ui.wait(|screen| {
        let text = screen.contents();
        text.contains("KEPT_PREFIX")
            && text.contains("failed: injected failure")
            && text.contains("NEXT_DRAFT")
            && !text.contains("Fixture waiting")
    });
}

#[test]
fn clear_then_replace_does_not_resurrect_hidden_prefix() {
    let mut ui = UiPty::start();
    ui.command("append", "HIDDEN_OLD\n\n");
    ui.wait(|screen| screen.contents().contains("HIDDEN_OLD"));
    ui.write("NEXT_DRAFT\x0c");
    ui.wait(|screen| {
        screen.contents().contains("NEXT_DRAFT") && !screen.contents().contains("HIDDEN_OLD")
    });
    ui.command("replace", "INSERTED_BEFORE_HIDDEN_OLD\n\nVISIBLE_NEW");
    ui.wait(|screen| screen.contents().contains("VISIBLE_NEW"));
    assert!(
        !ui.parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("HIDDEN_OLD")
    );
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    let text = ui.parser.lock().unwrap().screen().contents();
    assert!(text.contains("VISIBLE_NEW") && text.contains("NEXT_DRAFT"));
    assert!(!text.contains("HIDDEN_OLD"));
}

#[test]
fn resize_after_publication_does_not_republish_wrapped_markdown_source() {
    let mut ui = UiPty::start();
    let source = (0..24).map(|i| format!("COMMITTED_{i:02} a paragraph with words that wraps when the terminal is narrow.\n\n")).collect::<String>();
    ui.command("append", &source);
    ui.wait(|screen| screen.contents().contains("COMMITTED_23"));
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    ui.resize(30, 100);
    ui.write("RESIZE_ACK");
    ui.wait(|screen| screen.contents().contains("RESIZE_ACK"));
    // Generate more stable output after the reflow so that remaining source
    // groups also cross the publication boundary at the new width.
    ui.write("\x03/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    for i in 0..24 {
        assert_eq!(
            history.matches(&format!("COMMITTED_{i:02}")).count(),
            1,
            "{history}"
        );
    }
}

#[test]
fn browsing_keeps_output_anchor_and_editor_visible_while_backend_appends() {
    let mut ui = UiPty::start();
    ui.command(
        "append",
        &(0..65)
            .map(|i| format!("ROW_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("ROW_64"));
    ui.write("\x1b[5~\x1b[5~DRAFT_ANCHOR");
    ui.wait(|screen| {
        screen.contents().contains("DRAFT_ANCHOR") && !screen.contents().contains("ROW_64")
    });
    let first = ui
        .parser
        .lock()
        .unwrap()
        .screen()
        .rows(0, 60)
        .find(|row| row.contains("ROW_"))
        .unwrap();
    ui.command(
        "append",
        &(65..85)
            .map(|i| format!("ROW_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("step 2"));
    let parser = ui.parser.lock().unwrap();
    let screen = parser.screen();
    assert_eq!(
        screen.rows(0, 60).find(|row| row.contains("ROW_")).unwrap(),
        first
    );
    assert!(screen.contents().contains("DRAFT_ANCHOR"));
    drop(parser);
    ui.write("\x1b[F");
    ui.wait(|screen| {
        screen.contents().contains("ROW_84") && screen.contents().contains("DRAFT_ANCHOR")
    });
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    assert!(
        !ui.history()
            .iter()
            .any(|row| row.contains("Fixture waiting"))
    );
}
