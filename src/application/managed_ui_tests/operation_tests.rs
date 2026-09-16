use super::*;

#[test]
fn caught_worker_panic_finishes_block_without_independent_terminal_output() {
    let mut ui = UiPty::start();
    ui.command("append", "**PANIC_PREFIX**");
    ui.wait(|screen| screen.contents().contains("PANIC_PREFIX"));
    ui.command("panic", "");
    ui.wait(|screen| {
        screen.contents().contains("agent worker panicked")
            && !screen.contents().contains("Fixture waiting")
    });
    let history = ui.history().join("\n");
    assert_eq!(history.matches("PANIC_PREFIX").count(), 1, "{history}");
    assert!(
        !history.contains("thread 'theseus-agent' panicked"),
        "{history}"
    );
    ui.write("printf RECOVERED\r");
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row.trim() == "RECOVERED")
    });
}

#[test]
fn failure_controls_leave_partial_answer_and_draft_on_the_primary_screen() {
    let mut ui = UiPty::start();
    ui.command("append", "**SAFE_PREFIX**");
    ui.wait(|screen| screen.contents().contains("SAFE_PREFIX"));
    ui.write("SAFE_DRAFT");
    ui.wait(|screen| screen.contents().contains("SAFE_DRAFT"));
    ui.command(
        "fail",
        "\x1b[?1049h\x1b[2J\x1b[31mREMOTE_FAILURE\x1b[0m\x07",
    );
    ui.wait(|screen| {
        screen.contents().contains("REMOTE_FAILURE")
            && !screen.contents().contains("Fixture waiting")
    });
    let parser = ui.parser.lock().unwrap();
    assert!(!parser.screen().alternate_screen());
    assert!(parser.screen().contents().contains("SAFE_PREFIX"));
    assert!(parser.screen().contents().contains("SAFE_DRAFT"));
    drop(parser);
    ui.write("_EDITED");
    ui.wait(|screen| screen.contents().contains("SAFE_DRAFT_EDITED"));
}

#[cfg(unix)]
#[test]
fn exiting_after_worker_failure_restores_cooked_terminal() {
    let mut ui = UiPty::start();
    let fd = ui.master.as_raw_fd().unwrap();
    let flags = || {
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) }, 0);
        unsafe { attributes.assume_init().c_lflag }
    };
    assert_eq!(flags() & libc::ICANON, 0);
    ui.command("append", "before failure");
    ui.wait(|screen| screen.contents().contains("before failure"));
    ui.command("fail", "");
    ui.wait(|screen| {
        screen.contents().contains("injected failure")
            && !screen.contents().contains("Fixture waiting")
    });
    ui.write("/exit\r");
    let deadline = Instant::now() + TIMEOUT;
    while ui.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "application failed to exit");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        flags() & (libc::ICANON | libc::ECHO | libc::ISIG),
        libc::ICANON | libc::ECHO | libc::ISIG
    );
}

#[test]
fn reset_acknowledges_new_status_and_does_not_auto_submit_the_following_draft() {
    fn message_count(screen: &vt100::Screen) -> Option<usize> {
        screen
            .rows(0, screen.size().1)
            .filter_map(|row| {
                let fields = row.split('│').map(str::trim).collect::<Vec<_>>();
                (fields.get(1) == Some(&"messages"))
                    .then(|| fields.get(2)?.parse().ok())
                    .flatten()
            })
            .last()
    }
    let mut ui = UiPty::start();
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    ui.resize(26, 90);
    fs::write(
        ui.directory
            .join(".theseus/logs/9999-01-01-00-00-00_trajectory.json"),
        serde_json::to_vec(&json!({"messages":[
            {"role":"system","content":"fixture system"},
            {"role":"user","content":"RESUMED_QUESTION"},
            {"role":"assistant","content":"prior answer"}
        ]}))
        .unwrap(),
    )
    .unwrap();
    ui.write("/resume\r");
    ui.wait(|screen| screen.contents().contains("RESUMED_QUESTION"));
    ui.write("\r");
    ui.wait(|screen| screen.contents().contains("Resumed session from"));
    ui.write("/status\r");
    ui.wait(|screen| message_count(screen) == Some(3));
    fs::write(ui.directory.join("hold-configuration"), b"hold").unwrap();
    ui.write("/reset\r/status\r");
    ui.wait(|screen| {
        let row = screen.cursor_position().0 as usize;
        screen.contents().contains("Applying configuration")
            && screen
                .rows(0, screen.size().1)
                .nth(row)
                .is_some_and(|line| line.ends_with("/status"))
    });
    assert!(
        !ui.parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("Agent context has been reset")
    );
    // This local commit is already persisted; cancellation cannot leave the
    // running Agent using a different configuration from the saved file.
    ui.write("\x03");
    ui.wait(|screen| screen.contents().contains("Cancelling"));
    fs::write(ui.directory.join("release-configuration"), b"ok").unwrap();
    ui.wait(|screen| {
        screen.contents().contains("Agent context has been reset")
            && !screen.contents().contains("Cancelling")
    });
    assert_ne!(
        message_count(ui.parser.lock().unwrap().screen()),
        Some(1),
        "draft was submitted automatically"
    );
    ui.write("\r");
    ui.wait(|screen| message_count(screen) == Some(1));
    let history = ui.history().join("\n");
    assert_eq!(history.matches("Agent context has been reset").count(), 1);
}

#[test]
fn model_catalog_loading_keeps_ui_live_and_preserves_draft_on_cancel_or_selection() {
    for cancel in [true, false] {
        let mut ui = UiPty::start();
        ui.command("finish", "");
        ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
        fs::write(ui.directory.join("hold-model-catalog"), b"hold").unwrap();
        let cache = ui.directory.join(".theseus/persist/openrouter_models.json");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, serde_json::to_vec(&json!({
            "fetched_at_unix": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
            "models": [{"id":"fixture/model-a","name":"First","context_length":8000},
                       {"id":"fixture/model-b","name":"Second","context_length":16000}]
        })).unwrap()).unwrap();
        ui.write("/config\r");
        ui.wait(|screen| screen.contents().contains("Change model"));
        ui.write("\r");
        ui.wait(|screen| screen.contents().contains("Loading models"));
        let started = Instant::now();
        ui.resize(22, 90);
        ui.write("MODEL_DRAFT");
        ui.wait(|screen| {
            screen.contents().contains("MODEL_DRAFT")
                && screen.contents().contains("Loading models")
        });
        assert_responsive(started, "draft and resize during catalog loading");
        if cancel {
            let started = Instant::now();
            ui.write("\x03");
            ui.wait(|screen| {
                screen.contents().contains("MODEL_DRAFT")
                    && !screen.contents().contains("Loading models")
            });
            assert_responsive(started, "model catalog cancellation");
            assert!(
                !ui.parser
                    .lock()
                    .unwrap()
                    .screen()
                    .contents()
                    .contains("Select model")
            );
        } else {
            fs::write(ui.directory.join("release-model-catalog"), b"ok").unwrap();
            ui.wait(|screen| {
                screen.contents().contains("Select model")
                    && screen.contents().contains("fixture/model-b")
            });
            ui.write("\x1b[B\r");
            ui.wait(|screen| {
                screen.contents().contains("Config saved")
                    && screen.contents().contains("MODEL_DRAFT")
            });
            let init =
                AgentConfig::load_or_create_at(ui.directory.join(".theseus/config.jsonc")).unwrap();
            assert_eq!(
                init.config.llm_request_settings.body["model"],
                "fixture/model-b"
            );
        }
        ui.write("_EDITED");
        ui.wait(|screen| screen.contents().contains("MODEL_DRAFT_EDITED"));
    }
}

#[test]
fn disconnected_output_closes_blocks_cancels_producer_and_waits_for_cleanup() {
    for fault in ["late-completion", "drop-completion"] {
        let mut ui = UiPty::start_with_fault(Some(fault));
        ui.wait(|screen| screen.contents().contains("CHANNEL_PREFIX"));
        ui.command("disconnect", "");
        ui.wait(|screen| screen.contents().contains("channel closed before Finished"));
        ui.write("CHANNEL_DRAFT");
        ui.wait(|screen| {
            screen.contents().contains("CHANNEL_DRAFT") && screen.contents().contains("Cancelling")
        });
        let deadline = Instant::now() + TIMEOUT;
        while !ui.directory.join("fault-cancelled").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        // Cleanup acknowledgement is deliberately held until the UI has drawn
        // the error and kept accepting edits. It must not declare success early.
        ui.command("acknowledge", "");
        ui.wait(|screen| {
            screen.contents().contains("CHANNEL_DRAFT") && !screen.contents().contains("Cancelling")
        });
        ui.write(&("\x7f".repeat("CHANNEL_DRAFT".len()) + "printf CHANNEL_RECOVERED\r"));
        ui.wait(|screen| {
            screen
                .rows(0, screen.size().1)
                .any(|row| row.trim() == "CHANNEL_RECOVERED")
        });
        let history = ui.history().join("\n");
        assert_eq!(history.matches("CHANNEL_PREFIX").count(), 1);
        assert!(!history.contains("must not turn protocol failure into success"));
        let logs = fs::read_dir(ui.directory.join(".theseus/logs"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "jsonl"))
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect::<String>();
        assert_eq!(logs.matches("backend_output_disconnected").count(), 1);
    }
}

#[test]
fn late_block_event_is_rejected_and_logged_without_its_payload() {
    let mut ui = UiPty::start();
    ui.command("append", "ACCEPTED_PREFIX");
    ui.wait(|screen| screen.contents().contains("ACCEPTED_PREFIX"));
    ui.command("late", "DO_NOT_LOG_OR_RENDER_PAYLOAD");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    let rendered = ui.history().join("\n");
    assert_eq!(rendered.matches("ACCEPTED_PREFIX").count(), 1);
    assert!(!rendered.contains("DO_NOT_LOG_OR_RENDER_PAYLOAD"));
    let mut rejected = Vec::new();
    for entry in fs::read_dir(ui.directory.join(".theseus/logs")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("DO_NOT_LOG_OR_RENDER_PAYLOAD"));
        for line in text.lines() {
            let event: Value = serde_json::from_str(line).unwrap();
            if event["event"] == "backend_event_rejected" {
                rejected.push(event);
            }
        }
    }
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    let fields = &rejected[0]["fields"];
    assert_eq!(fields["kind"], "text_appended");
    assert_eq!(fields["block_id"], 1);
    assert_eq!(fields["active_operation_id"], fields["operation_id"]);
    assert!(fields["sequence"].as_u64().unwrap() > 0);
    ui.write("NEXT_DRAFT");
    ui.wait(|screen| screen.contents().contains("NEXT_DRAFT"));
}
