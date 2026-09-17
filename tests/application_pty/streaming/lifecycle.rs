use super::*;
use std::sync::mpsc;

#[test]
fn cancel_provider_error_and_eof_preserve_prefix_draft_and_next_operation() -> io::Result<()> {
    for outcome in ["cancel", "provider", "eof"] {
        let mut ui = Ui::start()?;
        ui.app.write("/ask interrupted stream\r")?;
        ui.server.request()?;
        ui.server.headers()?;
        ui.server.content("**PRESERVED_PREFIX**")?;
        ui.wait(|screen| screen.contents().contains("PRESERVED_PREFIX"))?;
        ui.app.write("KEPT_DRAFT")?;
        ui.wait(|screen| screen.contents().contains("KEPT_DRAFT"))?;
        ui.xterm_checkpoint("busy");
        match outcome {
            "cancel" => {
                let started = Instant::now();
                ui.app.write("\x03")?;
                ui.wait(|screen| {
                    screen.contents().contains("Agent request interrupted")
                        && spinner(screen).is_none()
                })?;
                assert!(
                    started.elapsed() <= Duration::from_millis(250),
                    "cancel acknowledgement: {:?}",
                    started.elapsed()
                );
                ui.server.action(Action::ExpectClosed)?;
            }
            "provider" => {
                ui.server
                    .event(json!({"error":{"code":503,"message":"provider fixture failure"}}))?;
                ui.server.action(Action::ExpectClosed)?;
            }
            "eof" => {
                // Incomplete event is discarded, including its marker.
                ui.server.bytes(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"LATE_REJECTED\"}}]}",
                )?;
                ui.server.action(Action::Close)?;
            }
            _ => unreachable!(),
        }
        ui.wait(|screen| {
            let text = screen.contents();
            text.contains("PRESERVED_PREFIX")
                && text.contains("KEPT_DRAFT")
                && spinner(screen).is_none()
                && text.contains(if outcome == "cancel" {
                    "[interrupted]"
                } else {
                    "[failed:"
                })
        })?;
        ui.xterm_checkpoint("interrupted");
        assert!(
            matches!(
                ui.server.requests.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ),
            "semantic prefix was retried"
        );
        ui.app.write("\x03/ask next operation\r")?;
        let request = ui.server.request()?;
        assert!(
            !request.to_string().contains("PRESERVED_PREFIX"),
            "partial response committed to trajectory"
        );
        ui.server.headers()?;
        ui.server.content("**NEXT_RESPONSE**")?;
        ui.server.finish("stop")?;
        ui.wait(|screen| screen.contents().contains("NEXT_RESPONSE") && spinner(screen).is_none())?;
        let history = ui.history();
        assert_eq!(
            history.matches("PRESERVED_PREFIX").count(),
            1,
            "{outcome}: {history}"
        );
        assert_eq!(
            history.matches("NEXT_RESPONSE").count(),
            1,
            "{outcome}: {history}"
        );
        assert!(!history.contains("LATE_REJECTED"), "{history}");
        ui.export_xterm(&format!("interruption-{outcome}"))?;
        ui.app.exit()?;
    }
    Ok(())
}

#[test]
fn retry_headers_heartbeat_and_tool_only_response_show_only_one_spinner() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("\x0c/ask retry fixture\r")?;
    let first_request = ui.server.request()?;
    let first = ui.wait(|screen| spinner(screen).is_some())?;
    assert_minimal_progress(first.screen());
    ui.server
        .bytes("HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    ui.server.action(Action::ExpectClosed)?;
    assert_eq!(first_request, ui.server.request()?);
    let retry = ui.wait(|screen| spinner(screen).is_some())?;
    assert_minimal_progress(retry.screen());
    ui.server.headers()?;
    ui.server.bytes(": keep-alive\n\n")?;
    ui.server.delta(json!({"role":"assistant", "reasoning_details":[{"index":0,"type":"reasoning.encrypted","data":"DO_NOT_LOG_ENCRYPTED","signature":"DO_NOT_LOG_SIGNATURE"}]}))?;
    ui.server.delta(json!({"role":"assistant", "tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]}))?;
    let glyph = spinner(retry.screen()).unwrap().1;
    let held = ui.wait(|screen| spinner(screen).is_some_and(|current| current.1 != glyph))?;
    assert_minimal_progress(held.screen());
    let rows = held
        .screen()
        .rows(0, held.screen().size().1)
        .collect::<Vec<_>>();
    let question = rows
        .iter()
        .position(|row| row.contains("retry fixture"))
        .unwrap();
    assert_eq!(
        spinner(held.screen()).unwrap().0,
        question + 1,
        "empty tool-only assistant block: {rows:?}"
    );
    ui.server.delta(json!({"tool_calls":[{"index":0,"id":"tool_late_id","type":"function","function":{"name":"bash","arguments":format!("{}}}", json!("printf 'RETRY_%s\\n' TOOL"))}}]}))?;
    ui.server.finish("tool_calls")?;
    ui.server.request()?;
    ui.wait(|screen| screen.contents().contains("RETRY_TOOL"))?;
    ui.server.headers()?;
    ui.server.content("RETRY_")?;
    ui.server.content("COMPLETE")?;
    ui.server.finish("stop")?;
    ui.wait(|screen| screen.contents().contains("RETRY_COMPLETE") && spinner(screen).is_none())?;
    let history = ui.history();
    assert_eq!(history.matches("RETRY_COMPLETE").count(), 1, "{history}");
    assert!(
        !history.contains("Working")
            && !history.contains("Waiting for response")
            && !history.contains("attempt"),
        "{history}"
    );
    assert!(
        !history
            .chars()
            .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
        "{history}"
    );
    let events = log_events(&ui.app.home)?;
    let starts = events
        .iter()
        .filter(|event| event["event"] == "llm_request_start")
        .map(|event| &event["fields"])
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[0]["request_id"], starts[1]["request_id"]);
    assert_ne!(starts[0]["attempt_id"], starts[1]["attempt_id"]);
    assert_ne!(starts[1]["request_id"], starts[2]["request_id"]);
    for name in [
        "llm_stream_started",
        "llm_first_semantic_delta",
        "llm_stream_finished",
    ] {
        assert_eq!(
            events.iter().filter(|event| event["event"] == name).count(),
            2,
            "{name}: {events:?}"
        );
    }
    // Opaque reasoning/tool fragments count as semantic receipt, but only the
    // later text response is enqueued. Multiple text deltas log this once.
    let enqueued = events
        .iter()
        .filter(|event| event["event"] == "llm_first_text_enqueued")
        .collect::<Vec<_>>();
    assert_eq!(enqueued.len(), 1);
    assert_eq!(enqueued[0]["fields"]["request_id"], starts[2]["request_id"]);
    assert_eq!(enqueued[0]["fields"]["phase"], "enqueue");
    let telemetry = serde_json::to_string(&events)?;
    for secret in [
        "DO_NOT_LOG_ENCRYPTED",
        "DO_NOT_LOG_SIGNATURE",
        "Bearer fixture",
    ] {
        assert!(
            !telemetry.contains(secret),
            "secret payload in event log: {secret}"
        );
    }
    ui.app.exit()
}

#[test]
fn compact_sse_keeps_summary_private_and_commits_context_only_on_success() -> io::Result<()> {
    for outcome in ["finish", "cancel", "provider"] {
        let mut ui = Ui::start()?;
        ui.app.write("/ask context seed\r")?;
        ui.server.request()?;
        ui.server.headers()?;
        ui.server.content("OLD_CONTEXT")?;
        ui.server.finish("stop")?;
        ui.wait(|screen| screen.contents().contains("OLD_CONTEXT") && spinner(screen).is_none())?;
        ui.app.write("/compact\r")?;
        let compact = ui.server.request()?;
        assert!(compact.get("tools").is_none());
        ui.server.headers()?;
        ui.server.content("PRIVATE_SUMMARY")?;
        ui.app.write("COMPACT_DRAFT")?;
        let held = ui.wait(|screen| {
            screen.contents().contains("COMPACT_DRAFT") && spinner(screen).is_some()
        })?;
        assert!(!held.screen().contents().contains("PRIVATE_SUMMARY"));
        match outcome {
            "finish" => ui.server.finish("stop")?,
            "cancel" => {
                ui.app.write("\x03")?;
                ui.server.action(Action::ExpectClosed)?;
            }
            "provider" => {
                ui.server
                    .event(json!({"error":{"code":503,"message":"compact fixture failure"}}))?;
                ui.server.action(Action::ExpectClosed)?;
            }
            _ => unreachable!(),
        }
        ui.wait(|screen| screen.contents().contains("COMPACT_DRAFT") && spinner(screen).is_none())?;
        assert!(
            !ui.history().contains("PRIVATE_SUMMARY"),
            "service summary leaked into presentation"
        );
        ui.app.write("\x03/ask after compact\r")?;
        let next = ui.server.request()?;
        let messages = next["messages"].to_string();
        assert_eq!(
            messages.contains("OLD_CONTEXT"),
            outcome != "finish",
            "{outcome}: {messages}"
        );
        assert_eq!(
            messages.contains("PRIVATE_SUMMARY"),
            outcome == "finish",
            "{outcome}: {messages}"
        );
        ui.server.headers()?;
        ui.server.content("COMPACT_CONTINUATION")?;
        ui.server.finish("stop")?;
        ui.wait(|screen| {
            screen.contents().contains("COMPACT_CONTINUATION") && spinner(screen).is_none()
        })?;
        ui.app.exit()?;
    }
    Ok(())
}

#[test]
fn streaming_paste_clear_and_multiline_history_keep_draft_until_explicit_enter() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("/ask\rExplain FIRST\r\rSECOND\r/end\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("**BEFORE_CLEAR**")?;
    ui.wait(|screen| screen.contents().contains("BEFORE_CLEAR"))?;
    let marker = ui.app.home.join("draft-executed");
    let draft = format!("printf done > '{}'", marker.display());
    ui.app.write(&format!("\x1b[200~{draft}\n\x1b[201~\r"))?;
    ui.wait(|screen| screen.contents().contains("draft-executed"))?;
    assert!(!marker.exists());
    ui.app.write("\x0c")?;
    ui.wait(|screen| {
        !screen.contents().contains("Explain FIRST")
            && screen.contents().contains("draft-executed")
            && spinner(screen).is_some()
    })?;
    ui.server.content("\n\n**AFTER_CLEAR**")?;
    ui.wait(|screen| {
        screen.contents().contains("AFTER_CLEAR") && !screen.contents().contains("BEFORE_CLEAR")
    })?;
    ui.server.finish("stop")?;
    ui.wait(|screen| screen.contents().contains("draft-executed") && spinner(screen).is_none())?;
    assert!(!marker.exists(), "completion submitted the pasted draft");
    ui.app.write("\r")?;
    ui.wait(|screen| {
        marker.exists()
            && screen
                .rows(0, screen.size().1)
                .nth(screen.cursor_position().0 as usize)
                .is_some_and(|row| row.trim_end().ends_with('>'))
    })?;
    assert_eq!(fs::read_to_string(marker)?, "done");
    let history: Value = serde_json::from_slice(&fs::read(
        ui.app.home.join(".theseus/persist/history_command_v2.json"),
    )?)?;
    let prompts = history
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == "agent")
        .collect::<Vec<_>>();
    assert_eq!(
        prompts,
        vec![&json!({"text":"Explain FIRST\n\nSECOND", "kind":"agent", "mode":"multi_line_ask"})]
    );
    // First Up is the shell command, second Up restores the multiline prompt.
    ui.app.write("\x1b[A\x1b[A")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .filter(|line| !line.is_empty())
            .last()
            .is_some_and(|row| row.trim() == "· SECOND")
    })?;
    ui.app.write("\x1b[B\x1b[B")?;
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen
            .rows(0, screen.size().1)
            .nth(row as usize)
            .is_some_and(|line| line.trim_end().ends_with('>'))
    })?;
    ui.app.write("/ask after clear\r")?;
    let continuation = ui.server.request()?;
    assert!(
        continuation["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"] == "**BEFORE_CLEAR**\n\n**AFTER_CLEAR**"),
        "clear lost the source in trajectory: {continuation}"
    );
    ui.server.headers()?;
    ui.server.content("CLEAR_CONTINUATION")?;
    ui.server.finish("stop")?;
    ui.wait(|screen| {
        spinner(screen).is_none() && screen.contents().contains("CLEAR_CONTINUATION")
    })?;
    ui.app.exit()
}

#[test]
fn failed_stream_telemetry_retains_finish_usage_and_semantic_timing() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("/ask telemetry fixture\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("TELEMETRY_PREFIX")?;
    ui.wait(|screen| screen.contents().contains("TELEMETRY_PREFIX"))?;
    ui.server.event(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9}}))?;
    ui.server
        .event(json!({"error":{"code":503,"message":"telemetry fixture failure"}}))?;
    ui.server.action(Action::ExpectClosed)?;
    ui.wait(|screen| {
        spinner(screen).is_none() && screen.contents().contains("telemetry fixture failure")
    })?;
    let events = log_events(&ui.app.home)?;
    let failures = events
        .iter()
        .filter(|event| event["event"] == "llm_stream_failed")
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 1);
    let fields = &failures[0]["fields"];
    assert_eq!(fields["finish_reason"], "stop");
    assert_eq!(fields["usage_known"], true);
    assert_eq!(fields["semantic_started"], true);
    assert_eq!(fields["retryable"], false);
    assert_eq!(fields["format"], "sse");
    assert_eq!(fields["purpose"], "chat");
    assert!(fields["bytes"].as_u64().unwrap() > 0);
    assert!(fields["chunks"].as_u64().unwrap() > 0);
    assert!(fields["last_network_ms"].as_u64() >= fields["last_semantic_ms"].as_u64());
    assert!(fields["ttft_ms"].is_number());
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "llm_first_semantic_delta")
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "llm_stream_finished"
                || event["event"] == "llm_request_retry")
    );
    ui.app.exit()
}
