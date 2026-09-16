use super::*;
use std::{net::TcpListener, sync::mpsc};

#[test]
fn operation_without_activity_shows_only_spinner_and_preserves_draft() {
    let mut ui = UiPty::start_with_options(None, true);
    ui.write("INITIAL_ACTIVITY_DRAFT");
    ui.wait(|screen| {
        waiting_spinner_is_visible(screen) && screen.contents().contains("INITIAL_ACTIVITY_DRAFT")
    });
    let text = ui.parser.lock().unwrap().screen().contents();
    assert!(!text.contains("Working"), "{text}");
    fs::write(ui.directory.join("release-initial-activity"), b"").unwrap();
    ui.wait(|screen| screen.contents().contains("Fixture waiting"));
    ui.command("finish", "");
    ui.wait(|screen| {
        !waiting_spinner_is_visible(screen)
            && !screen.contents().contains("Fixture waiting")
            && screen.contents().contains("INITIAL_ACTIVITY_DRAFT")
    });
    let history = ui.history().join("\n");
    assert!(!history.contains("Working"), "{history}");
}

struct CompactServer {
    url: String,
    ready: mpsc::Receiver<Value>,
    release: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CompactServer {
    fn start(outcome: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/chat", listener.local_addr().unwrap());
        let (ready_tx, ready) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "compact request never arrived");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TIMEOUT)).unwrap();
            let mut request = Vec::new();
            let mut bytes = [0; 4096];
            let (header_end, length) = loop {
                let count = stream.read(&mut bytes).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
                if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while request.len() < header_end + length {
                let count = stream.read(&mut bytes).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
            }
            ready_tx
                .send(serde_json::from_slice(&request[header_end..header_end + length]).unwrap())
                .unwrap();
            if outcome == "cancel" {
                match stream.read(&mut bytes) {
                    Ok(0) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                        ) => {}
                    result => panic!("compact response survived cancellation: {result:?}"),
                }
            } else {
                released.recv_timeout(TIMEOUT).unwrap();
                let body = if outcome == "success" {
                    json!({"choices":[{"finish_reason":"stop", "message":{"role":"assistant", "content":"COMPACT_PRIVATE_SUMMARY"}}]}).to_string()
                } else {
                    "invalid JSON".into()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            ready,
            release,
            worker: Some(worker),
        }
    }
}

impl Drop for CompactServer {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn trajectories(ui: &UiPty) -> std::collections::BTreeMap<PathBuf, String> {
    fs::read_dir(ui.directory.join(".theseus/logs"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("_trajectory.json")
        })
        .map(|path| {
            let text = fs::read_to_string(&path).unwrap();
            (path, text)
        })
        .collect()
}

#[test]
fn compact_keeps_draft_and_old_context_on_cancel_or_error_and_commits_success_once() {
    for outcome in ["cancel", "error", "success"] {
        let mut server = CompactServer::start(outcome);
        let mut ui = UiPty::start();
        ui.command("finish", "");
        ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
        ui.resize(26, 90);
        let path = ui.directory.join(".theseus/config.jsonc");
        let mut config = AgentConfig::load_or_create_at(path.clone()).unwrap().config;
        config.llm_request_settings.base_url = server.url.clone();
        config.llm_request_settings.retries = 1;
        config
            .llm_request_settings
            .header
            .insert("Authorization".into(), "Bearer fixture-key".into());
        config.save_at(&path).unwrap();
        ui.write("/reset\r");
        ui.wait(|screen| screen.contents().contains("Agent context has been reset"));
        fs::write(
            ui.directory
                .join(".theseus/logs/9999-01-01-00-00-00_trajectory.json"),
            serde_json::to_vec(&json!({"messages":[
                {"role":"system","content":"fixture system"},
                {"role":"user","content":"COMPACT_RESUME_QUESTION"},
                {"role":"assistant","content":"OLD_ASSISTANT_CONTEXT"}
            ]}))
            .unwrap(),
        )
        .unwrap();
        ui.write("/resume\r");
        ui.wait(|screen| screen.contents().contains("COMPACT_RESUME_QUESTION"));
        ui.write("\r");
        ui.wait(|screen| screen.contents().contains("Resumed session from"));
        let before = trajectories(&ui);
        ui.write("/compact\r");
        let request = server.ready.recv_timeout(TIMEOUT).unwrap();
        assert!(request.get("tools").is_none());
        assert!(
            request["messages"]
                .to_string()
                .contains("OLD_ASSISTANT_CONTEXT")
        );
        ui.wait(waiting_spinner_is_visible);
        ui.write("COMPACT_DRAFT");
        ui.wait(|screen| screen.contents().contains("COMPACT_DRAFT"));
        if outcome == "cancel" {
            ui.write("\x03");
        } else {
            server.release.send(()).unwrap();
        }
        ui.wait(|screen| {
            let contents = screen.contents();
            contents.contains("COMPACT_DRAFT")
                && !waiting_spinner_is_visible(screen)
                && !contents.contains("Cancelling")
                && if outcome == "success" {
                    contents.contains("Agent context compacted")
                } else {
                    contents.contains("agent:")
                }
        });
        server.worker.take().unwrap().join().unwrap();
        assert!(!ui.history().join("\n").contains("COMPACT_PRIVATE_SUMMARY"));
        let after = trajectories(&ui);
        for (path, text) in &before {
            assert_eq!(
                after.get(path),
                Some(text),
                "{outcome} overwrote a previous trajectory: {}",
                path.display()
            );
        }
        if outcome == "success" {
            assert_eq!(
                after
                    .values()
                    .filter(|text| text.contains("COMPACT_PRIVATE_SUMMARY"))
                    .count(),
                1
            );
        } else {
            assert_eq!(after, before);
        }
        ui.write("_EDITED");
        ui.wait(|screen| screen.contents().contains("COMPACT_DRAFT_EDITED"));
    }
}
