use super::*;
use crate::{
    agent::{AgentConfig, completion_output::Presentation, messages::ChatMessage},
    common::{
        cancellation::CancellationEvent,
        events::{EventSink, OutputEvent},
    },
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::Instant,
};

const TIMEOUT: Duration = Duration::from_secs(3);
const SSE_HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\n\r\n";
fn delta(content: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":content}}]})
    )
}
fn done() -> &'static str {
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
}

fn server(
    count: usize,
    mut respond: impl FnMut(usize, &mut TcpStream, Value) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/chat", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        for attempt in 0..count {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "request {attempt} did not arrive"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TIMEOUT)).unwrap();
            let request = super::tests::read_http_json_request(&mut stream);
            respond(attempt, &mut stream, request);
        }
    });
    (url, worker)
}
fn agent(url: String) -> Agent {
    let mut config = AgentConfig::default_empty();
    config.llm_request_settings.base_url = url;
    config.llm_request_settings.retries = 1;
    config
        .llm_request_settings
        .body
        .insert("stream".into(), json!(true));
    Agent::new(config)
}
fn assert_closed(stream: &mut TcpStream) {
    match stream.read(&mut [0; 1]) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("HTTP connection survived completion/cancel: {other:?}"),
    }
}

#[test]
fn public_string_api_keeps_unknown_usage_separate_from_last_context_estimate() {
    let (url, server) = server(3, move |attempt, stream, request| {
        assert_eq!(request["stream"], true);
        stream.write_all(SSE_HEADERS).unwrap();
        stream
            .write_all(delta(if attempt == 0 { "FIRST" } else { "SECOND" }).as_bytes())
            .unwrap();
        if attempt == 0 {
            stream.write_all(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\n").unwrap();
        }
        if attempt == 2 {
            stream
                .write_all(b"data: {\"error\":{\"code\":503,\"message\":\"after prefix\"}}\n\n")
                .unwrap();
        } else {
            stream.write_all(done().as_bytes()).unwrap();
        }
        assert_closed(stream);
    });
    let mut agent = agent(url);
    agent
        .header
        .insert("Authorization".into(), "Bearer fixture".into());
    let first: String = agent.run("first").unwrap();
    assert_eq!(first, "FIRST\n");
    assert!(agent.status_text().contains("| **context tokens** | 9 |"));
    let second: String = agent
        .run_with_context("second", crate::agent::AgentRunContext::default())
        .unwrap();
    assert_eq!(second, "SECOND\n");
    let status = agent.status_text();
    assert!(status.contains("| **context tokens** | n/a |"), "{status}");
    assert!(
        status.contains("| **last known context tokens** | 9 |"),
        "{status}"
    );
    assert!(
        status.contains("| **completion tokens** | 2 (partial) |"),
        "{status}"
    );
    assert_eq!(agent.latest_context_tokens(), Some(9));
    assert!(agent.run("failing request").is_err());
    assert_eq!(agent.latest_context_tokens(), Some(9));
    assert!(agent.latest_request_usage.get().is_none());
    server.join().unwrap();
}

#[test]
fn real_http_prefix_is_emitted_before_done_and_cancel_closes_socket() {
    for cancel in [false, true] {
        let (release, released) = mpsc::channel();
        let (url, server) = server(1, move |_, stream, request| {
            assert_eq!(request["stream"], true);
            stream.write_all(SSE_HEADERS).unwrap();
            stream
                .write_all(delta("**LIVE_PREFIX**").as_bytes())
                .unwrap();
            released.recv_timeout(TIMEOUT).unwrap();
            if !cancel {
                stream.write_all(done().as_bytes()).unwrap();
            }
            assert_closed(stream);
        });
        let cancellation = CancellationEvent::new();
        let (sink, events) = EventSink::channel(cancellation.clone());
        let mut agent = agent(url);
        agent.output = Some(sink);
        let worker_cancel = cancellation.clone();
        let worker = thread::spawn(move || agent.request_completion(&worker_cancel));
        let mut accepted = Vec::new();
        loop {
            let event = events.recv_timeout(TIMEOUT).unwrap();
            let prefix = matches!(&event.event, OutputEvent::TextAppended { text, .. } if text == "**LIVE_PREFIX**");
            assert!(!matches!(event.event, OutputEvent::BlockFinished { .. }));
            accepted.push(event);
            if prefix {
                break;
            }
        }
        if cancel {
            cancellation.cancel();
        }
        release.send(()).unwrap();
        let result = worker.join().unwrap();
        accepted.extend(events.try_iter());
        server.join().unwrap();
        if cancel {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        } else {
            let result = result.unwrap();
            assert_eq!(result.presentation, Presentation::Emitted);
            assert_eq!(
                result.trajectory.message().unwrap().content_text().unwrap(),
                "**LIVE_PREFIX**"
            );
        }
        assert_eq!(
            accepted
                .iter()
                .filter(|e| matches!(e.event, OutputEvent::TextAppended { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn idle_deadline_covers_headers_and_stalled_body() {
    for stage in ["headers", "body", "after_prefix"] {
        let (url, server) = server(1, move |_, stream, _| {
            if stage != "headers" {
                stream.write_all(SSE_HEADERS).unwrap();
            }
            if stage == "after_prefix" {
                stream.write_all(delta("PREFIX").as_bytes()).unwrap();
            }
            assert_closed(stream);
        });
        let mut agent = agent(url);
        agent.stream_idle_timeout = Duration::from_millis(40);
        let started = Instant::now();
        let error = agent
            .request_completion(&CancellationEvent::new())
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("idle"));
        assert!(started.elapsed() < Duration::from_millis(500));
        if stage == "after_prefix" {
            assert!(!is_retryable_llm_error(&error));
        }
    }
}

#[test]
fn active_streaming_requests_outlive_the_configured_total_timeout() {
    for mode in ["heartbeat", "content", "json_fallback"] {
        let (url, server) = server(1, move |_, stream, request| {
            assert_eq!(request["stream"], true);
            if mode == "json_fallback" {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                stream.write_all(SSE_HEADERS).unwrap();
            }
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(1150) {
                let chunk = match mode {
                    "heartbeat" => ": heartbeat\n\n".to_owned(),
                    "content" => delta("tick "),
                    _ => " ".to_owned(),
                };
                stream.write_all(chunk.as_bytes()).unwrap();
                // Controlled network pacing exercises the idle reset while the
                // whole exchange crosses the configured one-second deadline.
                thread::sleep(Duration::from_millis(20));
            }
            if mode == "json_fallback" {
                stream.write_all(br#"{"choices":[{"message":{"role":"assistant","content":"COMPLETE"},"finish_reason":"stop"}]}"#).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
            } else {
                stream.write_all(delta("COMPLETE").as_bytes()).unwrap();
                stream.write_all(done().as_bytes()).unwrap();
            }
            assert_closed(stream);
        });
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.base_url = url;
        config.llm_request_settings.retries = 1;
        config.llm_request_settings.request_timeout_seconds = 1;
        config
            .llm_request_settings
            .body
            .insert("stream".into(), json!(true));
        // Construct the real client with this config too: changing only the
        // coordinator's field would miss a hidden reqwest-wide total timeout.
        let mut agent = Agent::new(config);
        agent.stream_idle_timeout = Duration::from_millis(200);
        let started = Instant::now();
        let result = agent.request_completion(&CancellationEvent::new());
        server.join().unwrap();
        let result = result.unwrap();
        assert!(started.elapsed() > agent.llm_request_timeout);
        let content = result.trajectory.message().unwrap().content_text().unwrap();
        assert!(content.ends_with("COMPLETE"), "{content}");
        if mode == "content" {
            assert!(content.starts_with("tick "));
        }
    }
}

#[test]
fn json_requests_keep_total_deadline_even_while_body_bytes_arrive() {
    for explicit_false in [false, true] {
        let (url, server) = server(1, |_, stream, request| {
            assert_ne!(request["stream"], true);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n").unwrap();
            for _ in 0..60 {
                if stream.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert_closed(stream);
        });
        let mut agent = agent(url);
        agent.body.remove("stream");
        if explicit_false {
            agent.body.insert("stream".into(), json!(false));
        }
        agent.llm_request_timeout = Duration::from_millis(60);
        let error = agent
            .request_completion(&CancellationEvent::new())
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("deadline"));
    }
}

#[test]
fn retry_reuses_tools_and_messages_before_semantic_start() {
    let (url, server) = server(2, {
        let mut first = None;
        move |attempt, stream, request| {
            if attempt == 0 {
                first = Some(request.clone());
            } else {
                assert_eq!(first.as_ref().unwrap(), &request);
            }
            assert!(
                request["tools"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty())
            );
            stream.write_all(SSE_HEADERS).unwrap();
            if attempt == 0 {
                stream.write_all(b": heartbeat\n\ndata: {\"error\":{\"code\":503,\"message\":\"retry\"}}\n\n").unwrap();
            } else {
                stream.write_all(delta("retried").as_bytes()).unwrap();
                stream.write_all(done().as_bytes()).unwrap();
            }
        }
    });
    let mut agent = agent(url);
    agent.llm_request_retries = 2;
    let result = agent.request_completion(&CancellationEvent::new()).unwrap();
    server.join().unwrap();
    assert_eq!(
        result.trajectory.message().unwrap().content_text().unwrap(),
        "retried"
    );
}

#[test]
fn no_retry_after_content_opaque_reasoning_or_tool_fragments_even_for_compact() {
    for prefix in [
        json!({"role":"assistant","content":"prefix"}),
        json!({"role":"assistant","reasoning_details":[{"type":"reasoning.encrypted","data":"opaque"}]}),
        json!({"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"{"}}]}),
    ] {
        let (url, server) = server(1, move |_, stream, _| {
            stream.write_all(SSE_HEADERS).unwrap();
            write!(stream, "data: {}\n\ndata: {{\"error\":{{\"code\":503,\"message\":\"partial failure\"}}}}\n\n", json!({"choices":[{"index":0,"delta":prefix}]})).unwrap();
        });
        let mut agent = agent(url);
        agent.llm_request_retries = 3;
        let error = agent
            .request_completion_for_messages(
                vec![ChatMessage::user("compact")],
                false,
                "compact",
                &CancellationEvent::new(),
            )
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.to_string(), "partial failure");
        assert!(!is_retryable_llm_error(&error));
        let error = error
            .get_ref()
            .unwrap()
            .downcast_ref::<AttemptError>()
            .unwrap();
        assert_eq!(error.attempt, 1);
        assert!(error.semantic_started);
    }
}

#[test]
fn json_fallback_uses_same_presentation_and_unknown_content_type_fails() {
    for mime in ["application/json; charset=utf-8", "text/html"] {
        let (url, server) = server(1, move |_, stream, _| {
            let body = json!({"choices":[{"message":{"role":"assistant","content":"JSON_FALLBACK", "reasoning_details":[{"type":"reasoning.summary","summary":"JSON_REASON"}]}}]}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let cancellation = CancellationEvent::new();
        let (sink, events) = EventSink::channel(cancellation.clone());
        let mut agent = agent(url);
        agent.output = Some(sink);
        let result = agent.request_completion(&cancellation);
        server.join().unwrap();
        if mime.starts_with("application/json") {
            assert_eq!(result.unwrap().presentation, Presentation::Emitted);
            let text = events
                .try_iter()
                .filter_map(|e| match e.event {
                    OutputEvent::TextAppended { text, .. } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(text, ["JSON_REASON", "JSON_FALLBACK"]);
        } else {
            assert!(result.unwrap_err().to_string().contains("Content-Type"));
        }
    }
}

#[test]
fn full_streaming_ingress_waits_without_idle_error_and_cancel_closes_http_before_cleanup() {
    // Prepare the oversized SSE event before starting the idle clock. Waiting
    // for the consumer or serializing 2 MiB after PREFIX would create genuine
    // provider inactivity on a busy runner, before queue backpressure starts.
    let mut response = SSE_HEADERS.to_vec();
    response.extend_from_slice(delta("PREFIX").as_bytes());
    response.extend_from_slice(delta(&"x".repeat(2 * 1024 * 1024)).as_bytes());
    let (closed_tx, closed) = mpsc::channel();
    let (url, server) = server(1, move |_, stream, _| {
        stream.write_all(&response).unwrap();
        assert_closed(stream);
        closed_tx.send(()).unwrap();
    });
    let cancellation = CancellationEvent::new();
    let (sink, events) = EventSink::channel(cancellation.clone());
    let mut agent = agent(url);
    agent.output = Some(sink);
    agent.llm_request_timeout = Duration::from_millis(300);
    agent.stream_idle_timeout = Duration::from_millis(100);
    let worker_cancel = cancellation.clone();
    let worker = thread::spawn(move || agent.request_completion(&worker_cancel));
    loop {
        let event = events.recv_timeout(TIMEOUT).unwrap();
        if matches!(&event.event, OutputEvent::TextAppended { text, .. } if text == "PREFIX") {
            break;
        }
    }
    // Wait for assembly to reach enqueue, then hold the bounded consumer beyond
    // both configured timeouts. Queue backpressure is not provider inactivity.
    let event = events.recv_timeout(TIMEOUT).unwrap().event;
    let OutputEvent::TextAppended { text, .. } = event else {
        panic!("expected live text, got {event:?}");
    };
    let premature_close = closed.recv_timeout(Duration::from_millis(400));
    let started = Instant::now();
    cancellation.cancel();
    if premature_close.is_err() {
        // The actual socket must close before we make room for terminal events.
        closed.recv_timeout(TIMEOUT).unwrap();
    }
    let cancellation_elapsed = started.elapsed();
    let mut text_bytes = text.len();
    while let Ok(event) = events.recv_timeout(TIMEOUT) {
        if let OutputEvent::TextAppended { text, .. } = event.event {
            text_bytes += text.len();
        }
    }
    let error = worker.join().unwrap().unwrap_err();
    server.join().unwrap();
    assert_eq!(premature_close, Err(mpsc::RecvTimeoutError::Timeout));
    assert!(cancellation_elapsed < Duration::from_millis(250));
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<AttemptError>()
            .unwrap()
            .phase,
        "enqueue"
    );
    assert!(text_bytes > 0 && text_bytes < 2 * 1024 * 1024);
    assert!(!is_retryable_llm_error(&error));
}

#[test]
fn malformed_bytes_do_not_discard_a_valid_prefix_in_the_same_http_write() {
    let (url, server) = server(1, move |_, stream, _| {
        let mut response = SSE_HEADERS.to_vec();
        response.extend_from_slice(delta("KEPT_BEFORE_INVALID_UTF8").as_bytes());
        response.extend_from_slice(b"data: \xff\n\n");
        stream.write_all(&response).unwrap();
        assert_closed(stream);
    });
    let cancellation = CancellationEvent::new();
    let (sink, events) = EventSink::channel(cancellation.clone());
    let mut agent = agent(url);
    agent.output = Some(sink);
    agent.llm_request_retries = 3;
    let error = agent.request_completion(&cancellation).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!is_retryable_llm_error(&error));
    let accepted = events
        .try_iter()
        .filter_map(|event| match event.event {
            OutputEvent::TextAppended { text, .. } => Some(text),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(accepted, "KEPT_BEFORE_INVALID_UTF8");
    server.join().unwrap();
}

#[test]
fn full_ingress_cancel_and_disconnect_close_http_before_block_cleanup() {
    use crate::common::events::{OUTPUT_QUEUE_CAPACITY, Outcome};
    for disconnect in [false, true] {
        let (release, released) = mpsc::channel();
        let (sent_tx, sent) = mpsc::channel();
        let (closed_tx, closed) = mpsc::channel();
        let (url, server) = server(1, move |_, stream, _| {
            stream.write_all(SSE_HEADERS).unwrap();
            stream.write_all(delta("PREFIX").as_bytes()).unwrap();
            released.recv_timeout(TIMEOUT).unwrap();
            stream
                .write_all(delta("AFTER_FULL_QUEUE").as_bytes())
                .unwrap();
            sent_tx.send(()).unwrap();
            assert_closed(stream);
            closed_tx.send(()).unwrap();
        });
        let cancellation = CancellationEvent::new();
        let (sink, events) = EventSink::channel(cancellation.clone());
        let mut agent = agent(url);
        agent.output = Some(sink.clone());
        let worker_cancel = cancellation.clone();
        let worker = thread::spawn(move || agent.request_completion(&worker_cancel));
        while !matches!(events.recv_timeout(TIMEOUT).unwrap().event, OutputEvent::TextAppended { ref text, .. } if text == "PREFIX")
        {
        }
        for _ in 0..OUTPUT_QUEUE_CAPACITY {
            sink.emit(OutputEvent::Activity {
                phase: "Waiting for response".into(),
                detail: "".into(),
            })
            .unwrap();
        }
        release.send(()).unwrap();
        sent.recv_timeout(TIMEOUT).unwrap();
        let started = Instant::now();
        if disconnect {
            drop(events);
            closed.recv_timeout(TIMEOUT).unwrap();
        } else {
            cancellation.cancel();
            // Do not free queue slots until the socket really closes.
            closed.recv_timeout(TIMEOUT).unwrap();
            assert!(started.elapsed() < Duration::from_millis(250));
            for _ in 0..OUTPUT_QUEUE_CAPACITY {
                assert!(matches!(
                    events.recv_timeout(TIMEOUT).unwrap().event,
                    OutputEvent::Activity { .. }
                ));
            }
            let mut closed_blocks = Vec::new();
            for _ in 0..2 {
                if let OutputEvent::BlockFinished { outcome, .. } =
                    events.recv_timeout(TIMEOUT).unwrap().event
                {
                    closed_blocks.push(outcome);
                }
            }
            assert_eq!(closed_blocks, [Outcome::Completed, Outcome::Cancelled]);
        }
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::BrokenPipe
        ));
        assert!(cancellation.is_cancelled());
        server.join().unwrap();
    }
}

#[test]
fn cancel_after_done_during_block_cleanup_prevents_trajectory_commit_and_tools() {
    use crate::common::events::OUTPUT_QUEUE_CAPACITY;
    let cancellation = CancellationEvent::new();
    let (sink, events) = EventSink::channel(cancellation.clone());
    let directory = std::env::temp_dir().join(format!(
        "theseus-done-cancel-{}-{}",
        std::process::id(),
        next_request_id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let effect = directory.join("must-not-run");
    let command = format!("printf forbidden > '{}'", effect.display());
    let (release, released) = mpsc::channel();
    let (closed_tx, closed) = mpsc::channel();
    let (url, server) = server(1, move |_, stream, _| {
        stream.write_all(SSE_HEADERS).unwrap();
        stream
            .write_all(delta("VALIDATED_PREFIX").as_bytes())
            .unwrap();
        released.recv_timeout(TIMEOUT).unwrap();
        write!(stream, "data: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"do_not_run","type":"function","function":{"name":"bash","arguments":json!({"command":command}).to_string()}}]},"finish_reason":"tool_calls"}]})).unwrap();
        assert_closed(stream);
        closed_tx.send(()).unwrap();
    });
    let mut agent = agent(url);
    agent
        .header
        .insert("Authorization".into(), "Bearer fixture".into());
    let context = crate::agent::AgentRunContext {
        output: Some(sink.clone()),
        cancellation: cancellation.clone(),
        ..Default::default()
    };
    let worker = thread::spawn(move || {
        let result = agent.run_with_context("test race", context);
        (agent, result)
    });
    while !matches!(events.recv_timeout(TIMEOUT).unwrap().event, OutputEvent::TextAppended { ref text, .. } if text == "VALIDATED_PREFIX")
    {
    }
    for _ in 0..OUTPUT_QUEUE_CAPACITY {
        sink.emit(OutputEvent::Activity {
            phase: "Waiting for response".into(),
            detail: "".into(),
        })
        .unwrap();
    }
    release.send(()).unwrap();
    // This proves DONE was validated and the runtime closed while block finishes
    // are held by a full queue. Cancel before allowing control back to the loop.
    closed.recv_timeout(TIMEOUT).unwrap();
    cancellation.cancel();
    for _ in 0..OUTPUT_QUEUE_CAPACITY + 2 {
        events.recv_timeout(TIMEOUT).unwrap();
    }
    let (agent, result) = worker.join().unwrap();
    assert_eq!(result.unwrap(), "Agent request interrupted.\n");
    assert!(!effect.exists());
    assert!(
        agent
            .trajectory
            .iter()
            .filter_map(|entry| entry.message())
            .all(|message| message.role != "assistant")
    );
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn compact_trim_retry_preserves_streaming_and_rejects_semantic_or_protocol_retries() {
    for mode in ["trim", "semantic", "protocol"] {
        let mut first_count = 0;
        let (url, server) = server(
            if mode == "trim" { 2 } else { 1 },
            move |attempt, stream, request| {
                assert_eq!(request["stream"], true);
                assert!(request.get("tools").is_none());
                let messages = request["messages"].as_array().unwrap();
                if attempt == 0 {
                    first_count = messages.len();
                }
                if attempt == 1 {
                    assert_eq!(messages.len(), first_count - 1);
                    assert!(!request["messages"].to_string().contains("OLDEST_USER"));
                    assert!(request["messages"].to_string().contains("RECENT_USER"));
                    stream.write_all(SSE_HEADERS).unwrap();
                    stream
                        .write_all(delta("COMPACTED_SUMMARY").as_bytes())
                        .unwrap();
                    stream.write_all(done().as_bytes()).unwrap();
                } else if mode == "trim" {
                    let body = r#"{"error":{"message":"maximum context length exceeded"}}"#;
                    write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                } else {
                    stream.write_all(SSE_HEADERS).unwrap();
                    if mode == "semantic" {
                        stream
                            .write_all(delta("PARTIAL_PRIVATE_SUMMARY").as_bytes())
                            .unwrap();
                        stream.write_all(b"data: {\"error\":{\"code\":400,\"message\":\"maximum context length exceeded\"}}\n\n").unwrap();
                    } else {
                        stream.write_all(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":\"invalid_context_reason\"}]}\n\n").unwrap();
                    }
                }
                assert_closed(stream);
            },
        );
        let mut agent = agent(url);
        agent
            .header
            .insert("Authorization".into(), "Bearer fixture".into());
        agent.push_message(ChatMessage::user("OLDEST_USER"));
        agent.push_message(ChatMessage::user("RECENT_USER"));
        let original = serde_json::to_value(&agent.trajectory).unwrap();
        let result = agent.compact_context();
        if mode == "trim" {
            let crate::agent::compact::CompactOutcome::Compacted(result) = result.unwrap() else {
                panic!("expected compact commit")
            };
            assert_eq!(result.compact_trim_retries, 1);
            assert!(
                serde_json::to_string(&agent.trajectory)
                    .unwrap()
                    .contains("COMPACTED_SUMMARY")
            );
        } else {
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains(if mode == "semantic" {
                    "maximum context"
                } else {
                    "invalid_context_reason"
                }),
                "{error}"
            );
            assert!(!allows_context_trim(&error));
            assert_eq!(serde_json::to_value(&agent.trajectory).unwrap(), original);
        }
        server.join().unwrap();
    }
}
