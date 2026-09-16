use super::*;

#[test]
fn extended_keyboard_edits_busy_draft_and_broken_escape_does_not_swallow_cancel() {
    let mut ui = UiPty::start();
    ui.command("append", "**KEYBOARD_PREFIX**");
    ui.wait(|screen| screen.contents().contains("KEYBOARD_PREFIX"));
    ui.write("ab\x1b[H\x1b[49:33;2u\x1b[120;1:3u");
    ui.wait(|screen| screen.contents().contains("!ab"));
    let started = Instant::now();
    ui.write("\x1b[1;\x03");
    ui.wait(|screen| {
        screen.contents().contains("interrupted") && !screen.contents().contains("Fixture waiting")
    });
    assert_responsive(started, "Ctrl+C after incomplete CSI");
    let parser = ui.parser.lock().unwrap();
    assert!(parser.screen().contents().contains("!ab"));
    assert!(!parser.screen().contents().contains("!xab"));
    assert!(parser.screen().contents().contains("KEYBOARD_PREFIX"));
}

#[test]
fn subthreshold_markdown_tail_does_not_delay_cancellation_frame() {
    let mut ui = UiPty::start();
    let prefix = format!("```rust\n{}", "let PREFIX = \"界\";\n".repeat(1000));
    let tail = "let ACCEPTED_TAIL = \"界\";\n".repeat(2000);
    assert!(prefix.len() + tail.len() < 128 * 1024);
    ui.command("append", &prefix);
    ui.wait(|screen| screen.contents().contains("PREFIX"));
    ui.command("append", &tail);
    let deadline = Instant::now() + TIMEOUT;
    while !ui.directory.join("2.accepted").exists() {
        assert!(Instant::now() < deadline, "tail was not queued");
        thread::sleep(Duration::from_millis(2));
    }
    let started = Instant::now();
    ui.write("\x03");
    ui.wait(|screen| {
        screen.contents().contains("interrupted") && !screen.contents().contains("Fixture waiting")
    });
    assert_responsive(started, "subthreshold Markdown cancel-to-frame");
    // Immediate feedback must not discard text already accepted before Ctrl+C.
    ui.wait(|screen| screen.contents().contains("ACCEPTED_TAIL"));
}

#[test]
fn busy_markdown_producer_does_not_starve_cancellation() {
    let mut ui = UiPty::start();
    ui.command("flood", "");
    ui.wait(|screen| screen.contents().contains("FLOW_"));
    let started = Instant::now();
    ui.write("\x03");
    ui.wait(|screen| {
        screen.contents().contains("interrupted") && !screen.contents().contains("Fixture waiting")
    });
    let elapsed = started.elapsed();
    eprintln!("busy Markdown cancel-to-frame: {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(250),
        "cancellation frame exceeded 250 ms: {elapsed:?}"
    );
}

#[test]
fn resize_and_cancel_remain_responsive_for_large_open_markdown() {
    let paragraph = (0..60000)
        .map(|i| format!("**WORD_{i:04}** "))
        .collect::<String>()
        + "OPEN_TAIL";
    let code = "```rust\n".to_owned()
        + &(0..16000)
            .map(|i| format!("let SOURCE_{i:04} = \"Привет 界\";\n"))
            .collect::<String>()
        + "// OPEN_TAIL";
    for (kind, text) in [("paragraph", paragraph), ("open code fence", code)] {
        let mut ui = UiPty::start();
        ui.command("append", &text);
        ui.wait(|screen| screen.contents().contains("OPEN_TAIL"));
        let started = Instant::now();
        ui.resize(18, 90);
        ui.write("ACTIVE_DRAFT");
        ui.wait(|screen| {
            screen.contents().contains("OPEN_TAIL") && screen.contents().contains("ACTIVE_DRAFT")
        });
        assert_responsive(
            started,
            &format!("open {kind}, {} bytes, resize 60→90 and draft", text.len()),
        );
        let started = Instant::now();
        ui.write("\x03");
        ui.wait(|screen| {
            screen.contents().contains("interrupted")
                && screen.contents().contains("ACTIVE_DRAFT")
                && !screen.contents().contains("Fixture waiting")
        });
        assert_responsive(
            started,
            &format!("open {kind}, {} bytes, cancellation", text.len()),
        );
    }
}

#[test]
fn background_layout_clear_replace_and_shell_preserve_only_final_output() {
    let mut ui = UiPty::start();
    let text = "```rust\n".to_owned()
        + &(0..16000)
            .map(|i| format!("let HIDDEN_{i:05} = \"Привет 界\";\n"))
            .collect::<String>()
        + "// HIDDEN_TAIL";
    ui.command("append", &text);
    ui.wait(|screen| screen.contents().contains("HIDDEN_TAIL"));
    ui.resize(18, 90);
    ui.write("\x0cKEPT_DRAFT");
    ui.wait(|screen| {
        screen.contents().contains("KEPT_DRAFT") && !screen.contents().contains("HIDDEN_")
    });
    // A completely replaced source after clear remains hidden up to the rebased
    // boundary; only text appended after that replacement becomes visible.
    ui.command("replace", "");
    ui.command("append", "**VISIBLE_FINAL**\n");
    ui.command("finish", "");
    ui.wait(|screen| {
        let text = screen.contents();
        text.contains("VISIBLE_FINAL")
            && text.contains("KEPT_DRAFT")
            && !text.contains("Fixture waiting")
    });
    ui.write(&"\x7f".repeat("KEPT_DRAFT".len()));
    for marker in ["SHELL_AFTER_LAYOUT", "SHELL_SECOND_LAYOUT"] {
        ui.write(&format!("printf '{marker}\\n'\r"));
        ui.wait(|screen| {
            let (row, _) = screen.cursor_position();
            screen
                .rows(0, screen.size().1)
                .any(|line| line.trim() == marker)
                && screen
                    .rows(0, screen.size().1)
                    .nth(row as usize)
                    .unwrap_or_default()
                    .trim_end()
                    .ends_with('>')
        });
    }
    let history = ui.history().join("\n");
    assert!(!history.contains("HIDDEN_"), "{history}");
    assert_eq!(history.matches("VISIBLE_FINAL").count(), 1, "{history}");
    assert_eq!(
        history
            .lines()
            .filter(|line| line.trim() == "SHELL_AFTER_LAYOUT")
            .count(),
        1,
        "{history}"
    );
    assert_eq!(
        history
            .lines()
            .filter(|line| line.trim() == "SHELL_SECOND_LAYOUT")
            .count(),
        1,
        "{history}"
    );
}

#[test]
fn large_single_markdown_paragraph_publishes_across_resize_without_losing_tail_or_draft() {
    let mut ui = UiPty::start();
    let text = (0..3000)
        .map(|i| format!("**WORD_{i:04}** "))
        .collect::<String>()
        + "PARAGRAPH_END";
    ui.command("append", &text);
    ui.wait(|screen| screen.contents().contains("PARAGRAPH_END"));
    ui.command("finish", "");
    ui.wait(|screen| {
        screen.contents().contains("PARAGRAPH_END")
            && !screen.contents().contains("Fixture waiting")
    });
    ui.resize(18, 90);
    let draft_started = Instant::now();
    ui.write("KEPT_DRAFT");
    ui.wait(|screen| {
        screen.contents().contains("KEPT_DRAFT") && screen.contents().contains("PARAGRAPH_END")
    });
    assert_responsive(draft_started, "large paragraph resize-to-draft frame");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = ui.history().join("\n");
        let words = text
            .split_whitespace()
            .filter(|word| word.starts_with("WORD_"))
            .collect::<std::collections::BTreeSet<_>>();
        if words.len() == 3000 {
            for word in words {
                assert_eq!(text.matches(word).count(), 1, "duplicate {word}");
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native publication stalled at {} / 3000 words",
            words.len()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn large_table_row_keeps_each_cell_word_once_when_resized_during_publication() {
    fn columns(rows: impl Iterator<Item = String>) -> [String; 2] {
        let mut result = [String::new(), String::new()];
        for row in rows {
            let cells = row.split('│').collect::<Vec<_>>();
            if cells.len() >= 4 {
                for index in 0..2 {
                    result[index].extend(cells[index + 1].chars().filter(|c| !c.is_whitespace()));
                }
            }
        }
        result
    }
    let mut ui = UiPty::start();
    let left = (0..500)
        .map(|i| format!("LEFT_{i:04} "))
        .collect::<String>();
    let right = (0..500)
        .map(|i| format!("RIGHT_{i:04} "))
        .collect::<String>();
    ui.command(
        "append",
        &format!("| Left | Right |\n|---|---|\n| {left} | {right} |\n"),
    );
    ui.wait(|screen| columns(screen.rows(0, screen.size().1))[1].contains("RIGHT_0499"));
    ui.command("finish", "");
    ui.wait(|screen| {
        columns(screen.rows(0, screen.size().1))[1].contains("RIGHT_0499")
            && !screen.contents().contains("Fixture waiting")
    });
    ui.resize(18, 90);
    let draft_started = Instant::now();
    ui.write("TABLE_DRAFT");
    ui.wait(|screen| {
        screen.contents().contains("TABLE_DRAFT")
            && columns(screen.rows(0, screen.size().1))[1].contains("RIGHT_0499")
    });
    assert_responsive(draft_started, "large table resize-to-draft frame");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let cells = columns(ui.history().into_iter());
        let mut words = std::collections::BTreeMap::new();
        for (column, prefix) in ["LEFT_", "RIGHT_"].iter().enumerate() {
            for suffix in cells[column].split(prefix).skip(1) {
                if let Some(index) = suffix.get(..4).and_then(|s| s.parse::<usize>().ok()) {
                    *words.entry((column, index)).or_insert(0) += 1;
                }
            }
        }
        if words.len() == 1000 {
            for column in 0..2 {
                for index in 0..500 {
                    assert_eq!(
                        words.get(&(column, index)),
                        Some(&1),
                        "column {column}, word {index}"
                    );
                }
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "table publication stalled at {} / 1000 words",
            words.len()
        );
        thread::sleep(Duration::from_millis(5));
    }
}
