use super::*;

#[test]
fn repeated_shell_leases_and_resize_preserve_one_copy_of_each_output_line() {
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    for batch in 0..3 {
        ui.write(&format!(
            "i=0; while [ $i -lt 35 ]; do printf 'LEASE_{batch}_%02d\\n' \"$i\"; i=$((i+1)); done\r"
        ));
        ui.wait(|screen| {
            let (row, _) = screen.cursor_position();
            let current = screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default();
            screen.contents().contains(&format!("LEASE_{batch}_34"))
                && current.trim_end().ends_with('>')
        });
        if batch == 0 {
            ui.resize(24, 90);
        }
    }
    ui.write("/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    for batch in 0..3 {
        for line in 0..35 {
            let marker = format!("LEASE_{batch}_{line:02}");
            assert_eq!(
                history.lines().filter(|row| row.trim() == marker).count(),
                1,
                "{marker}:\n{history}"
            );
        }
    }
}

#[test]
fn shell_scrolling_through_a_wrapped_line_does_not_republish_its_first_row() {
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    ui.write("i=0; while [ $i -lt 35 ]; do printf 'HEAD_%02d%053dTAIL_%02d\\n' \"$i\" 0 \"$i\"; i=$((i+1)); done\r");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen.contents().contains("TAIL_34")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    ui.resize(24, 90);
    ui.write("/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    for line in 0..35 {
        for prefix in ["HEAD", "TAIL"] {
            let marker = format!("{prefix}_{line:02}");
            assert_eq!(history.matches(&marker).count(), 1, "{marker}:\n{history}");
        }
    }
}

#[test]
fn shell_alternate_screen_preserves_primary_output_before_and_after_tui() {
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    ui.write("i=0; while [ $i -lt 35 ]; do printf 'BEFORE_%02d\\n' \"$i\"; i=$((i+1)); done; printf '\\033[?1049h\\033[H'; i=0; while [ $i -lt 35 ]; do printf 'TUI_%02d\\n' \"$i\"; i=$((i+1)); done; printf '\\033[?1049l'; i=0; while [ $i -lt 35 ]; do printf 'AFTER_%02d\\n' \"$i\"; i=$((i+1)); done\r");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen.contents().contains("AFTER_34")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    ui.write("/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    for line in 0..35 {
        for prefix in ["BEFORE", "AFTER"] {
            let marker = format!("{prefix}_{line:02}");
            assert_eq!(
                history.lines().filter(|row| row.trim() == marker).count(),
                1,
                "{marker}:\n{history}"
            );
        }
        assert!(!history.contains(&format!("TUI_{line:02}")), "{history}");
    }
}

#[test]
fn shell_lease_restores_main_screen_when_tui_leaves_terminal_modes_changed() {
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    ui.write(concat!(
        r#"printf '\033[?1049h\033[3;10r\033[?6h\033[?7lTUI_LEFT_OPEN'"#,
        "\r"
    ));
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        !screen.alternate_screen()
            && screen.contents().contains("TUI_LEFT_OPEN")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    ui.write("printf 'RESTORED_SCREEN\\n'\r");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        !screen.alternate_screen()
            && screen
                .rows(0, screen.size().1)
                .any(|row| row.trim() == "RESTORED_SCREEN")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    ui.write("/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    assert_eq!(
        history
            .lines()
            .filter(|row| row.trim() == "RESTORED_SCREEN")
            .count(),
        1,
        "{history}"
    );
    assert!(
        !history.lines().any(|row| row.trim() == "TUI_LEFT_OPEN"),
        "{history}"
    );
}

#[test]
fn resize_during_shell_maps_publication_to_source_instead_of_one_capture_width() {
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    ui.write("i=0; while [ $i -lt 35 ]; do printf 'OLD_HEAD_%02d%049dOLD_TAIL_%02d\\n' \"$i\" 0 \"$i\"; i=$((i+1)); done; printf 'RESIZE_READY\\n'; read answer; i=0; while [ $i -lt 35 ]; do printf 'NEW_HEAD_%02d%049dNEW_TAIL_%02d\\n' \"$i\" 0 \"$i\"; i=$((i+1)); done\r");
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row.trim() == "RESIZE_READY")
    });
    ui.resize(18, 90);
    ui.write("continue\n");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen.contents().contains("NEW_TAIL_34")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    ui.write("/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    for line in 0..35 {
        for prefix in ["OLD_HEAD", "OLD_TAIL", "NEW_HEAD", "NEW_TAIL"] {
            let marker = format!("{prefix}_{line:02}");
            assert_eq!(history.matches(&marker).count(), 1, "{marker}:\n{history}");
        }
    }
}

#[test]
fn shell_source_anchor_survives_narrower_and_shorter_terminal() {
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    let ready = ui.directory.join("shell-resize-ready");
    let quoted_ready = format!("'{}'", ready.to_string_lossy().replace('\'', "'\\''"));
    // Move all complete OLD lines into native history before shrinking; real
    // terminal resize may clip cells still on the physical screen. The source
    // anchor must still distinguish the OLD and NEW wrapping widths.
    ui.write(&format!("i=0; while [ $i -lt 35 ]; do printf 'OLD_HEAD_%02d%049dOLD_TAIL_%02d\\n' \"$i\" 0 \"$i\"; i=$((i+1)); done; i=0; while [ $i -lt 19 ]; do printf '\\n'; i=$((i+1)); done; printf ready > {quoted_ready}; read answer; i=0; while [ $i -lt 35 ]; do printf 'NEW_HEAD_%02d%049dNEW_TAIL_%02d\\n' \"$i\" 0 \"$i\"; i=$((i+1)); done\r"));
    let deadline = Instant::now() + TIMEOUT;
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "shell resize handshake did not arrive"
        );
        thread::sleep(Duration::from_millis(5));
    }
    ui.wait(|screen| screen.contents().trim().is_empty());
    ui.resize(12, 45);
    ui.write("continue\n");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen.contents().contains("NEW_TAIL_34")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    ui.write("/help\r");
    ui.wait(|screen| screen.contents().contains("~/.theseus/logs"));
    let history = ui.history().join("\n");
    for line in 0..35 {
        for prefix in ["OLD_HEAD", "OLD_TAIL", "NEW_HEAD", "NEW_TAIL"] {
            let marker = format!("{prefix}_{line:02}");
            assert_eq!(history.matches(&marker).count(), 1, "{marker}:\n{history}");
        }
    }
}

#[test]
fn resize_during_quiet_shell_preserves_partly_scrolled_markdown_from_before_lease() {
    let mut ui = UiPty::start();
    ui.command(
        "append",
        &(0..30)
            .map(|i| format!("PRE_{i:02} {} END_{i:02}\n\n", "a".repeat(70)))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("END_29"));
    ui.command("finish", "");
    ui.wait(|screen| {
        screen.contents().contains("END_29") && !screen.contents().contains("Fixture waiting")
    });
    let ready = ui.directory.join("quiet-shell-ready");
    let quoted_ready = format!("'{}'", ready.to_string_lossy().replace('\'', "'\\''"));
    ui.write(&format!(
        "printf ready > {quoted_ready}; read answer; printf 'SMALL_RESULT\\n'\r"
    ));
    let deadline = Instant::now() + TIMEOUT;
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "quiet shell handshake did not arrive"
        );
        thread::sleep(Duration::from_millis(5));
    }
    ui.wait(|screen| {
        screen.cursor_position().1 == 0
            && screen.contents().replace('\n', "").contains("SMALL_RESULT")
    });
    ui.resize(18, 90);
    ui.write("continue\n");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen.contents().contains("SMALL_RESULT")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    ui.write("/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    for line in 0..30 {
        for prefix in ["PRE", "END"] {
            let marker = format!("{prefix}_{line:02}");
            assert_eq!(history.matches(&marker).count(), 1, "{marker}:\n{history}");
        }
    }
}
