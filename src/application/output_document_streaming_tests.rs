use super::super::document_layout::{Key, Prepared, Worker};
use super::*;
use crate::agent::{Agent, AgentConfig, AgentRunContext};
use crate::common::{cancellation::CancellationEvent, events::EventSink};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn read_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&request[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if request.len() >= end + 4 + length {
                break;
            }
        }
        assert!(request.len() < 128 * 1024);
    }
}

fn prepared(worker: &mut Worker, document: &OutputDocument, width: usize) -> Prepared {
    let key = Key::of(document, width);
    worker.request(key, document);
    let started = Instant::now();
    loop {
        if let Some(prepared) = worker.take(key).unwrap() {
            return prepared;
        }
        assert!(started.elapsed() < TIMEOUT);
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn real_sse_usage_and_outcomes_reuse_source_cache_origins_and_prepared_lines() {
    use serde_json::json;
    for outcome in ["finish", "fail", "cancel"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/chat", listener.local_addr().unwrap());
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TIMEOUT)).unwrap();
            read_request(&mut stream);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {}\n\n", json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"[**CACHE_MARKER**][doc]\n\n[doc]: https://example.test/cache\n\n```rust\nlet value = 1;"}}]})).unwrap();
            released.recv_timeout(TIMEOUT).unwrap();
            if outcome != "cancel" {
                write!(stream, "data: {}\n\n", json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":42}})).unwrap();
                if outcome == "finish" {
                    stream.write_all(b"data: [DONE]\n\n").unwrap();
                } else {
                    stream.write_all(b"data: {\"error\":{\"code\":503,\"message\":\"cache fixture failure\"}}\n\n").unwrap();
                }
            }
            match stream.read(&mut [0; 1]) {
                Ok(0) => {}
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                other => panic!("stream survived completion: {other:?}"),
            }
        });
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.base_url = url;
        config
            .llm_request_settings
            .body
            .insert("stream".into(), json!(true));
        config
            .llm_request_settings
            .header
            .insert("Authorization".into(), "Bearer fixture".into());
        config.agent_settings.build_in_tools.clear();
        let cancellation = CancellationEvent::new();
        let (sink, events) = EventSink::channel(cancellation.clone());
        let mut document = OutputDocument::default();
        document.start_operation(sink.operation());
        sink.emit(OutputEvent::Started).unwrap();
        let context = AgentRunContext {
            output: Some(sink),
            cancellation: cancellation.clone(),
            ..Default::default()
        };
        let agent =
            thread::spawn(move || Agent::new(config).run_with_context("cache fixture", context));
        loop {
            let event = events.recv_timeout(TIMEOUT).unwrap();
            let content = matches!(&event.event, OutputEvent::TextAppended { text, .. } if text.contains("CACHE_MARKER"));
            assert!(document.apply(event));
            if content {
                break;
            }
        }
        let rendered = document.render(60);
        let block = document
            .blocks
            .iter()
            .position(|block| block.kind == BlockKind::Markdown)
            .unwrap();
        let cache = document.blocks[block].cache.as_ref().unwrap().2.clone();
        let revision = document.blocks[block].revision;
        let origins = document.blocks[block].origins.clone();
        let mut worker = Worker::new().unwrap();
        let preview = prepared(&mut worker, &document, 60);
        assert_eq!(preview.rendered.lines, rendered.lines);
        assert!(preview.rendered.publication.iter().all(|unit| !unit.stable));
        if outcome == "cancel" {
            cancellation.cancel();
        }
        release.send(()).unwrap();
        for event in events {
            assert!(
                !matches!(event.event, OutputEvent::TextAppended { .. }),
                "completion/usage appended source again"
            );
            assert!(document.apply(event));
        }
        let result = agent.join().unwrap();
        assert_eq!(result.is_ok(), outcome != "fail");
        server.join().unwrap();
        let finished = document.render(60);
        assert_eq!(document.blocks[block].revision, revision);
        assert!(Arc::ptr_eq(
            &cache,
            &document.blocks[block].cache.as_ref().unwrap().2
        ));
        let current_origins = document.blocks[block].origins.as_ref().unwrap();
        let origins = origins.as_ref().unwrap();
        assert_eq!(current_origins.len(), origins.len());
        for (current, previous) in current_origins.iter().zip(origins) {
            assert!(Arc::ptr_eq(&current.characters, &previous.characters));
            assert_eq!(current.preserve_columns, previous.preserve_columns);
        }
        assert_eq!(&finished.lines[..rendered.lines.len()], rendered.lines);
        let final_frame = prepared(&mut worker, &document, 60);
        for index in 0..rendered.lines.len() {
            assert!(Arc::ptr_eq(
                &preview.layout.logical_lines[index],
                &final_frame.layout.logical_lines[index]
            ));
        }
        assert!(final_frame.layout.reflowed_last_frame <= 2);
        assert!(
            final_frame
                .rendered
                .publication
                .iter()
                .all(|unit| unit.stable)
        );
        assert!(
            preview.rendered.publication.iter().all(|unit| !unit.stable),
            "old Prepared frame was mutated"
        );
        assert_eq!(preview.rendered.lines, rendered.lines);
        assert_eq!(final_frame.rendered.lines, finished.lines);
        // Clearing an actual streamed document changes its generation. A ready
        // result for the previous source must not be accepted by the new view.
        let old_key = Key::of(&document, 40);
        worker.request(old_key, &document);
        let started = Instant::now();
        while !worker.has_ready_result() {
            assert!(started.elapsed() < TIMEOUT);
            thread::sleep(Duration::from_millis(1));
        }
        document.clear_visible();
        assert!(worker.take(Key::of(&document, 40)).unwrap().is_none());
        let cleared = prepared(&mut worker, &document, 40);
        assert_eq!(cleared.key.generation, document.generation());
        assert!(cleared.rendered.lines.is_empty());
        assert_eq!(preview.rendered.lines, rendered.lines);
    }
}
