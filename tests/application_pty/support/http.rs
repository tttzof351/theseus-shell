//! Explicitly controlled JSON responses for agent scenarios.
use super::pty::{WAIT_TIMEOUT, temp_home};
use std::{
    fs,
    io::{self, Read, Write},
    net::TcpListener,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

pub(crate) fn interrupted_agent_fixture() -> io::Result<(PathBuf, thread::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 8192];
        let _ = stream.read(&mut request).unwrap();
        let body = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_interrupted",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": "{\"command\":\"printf 'AGENT_%s_BEFORE_INTERRUPT\\\\n' OUTPUT; exec sleep 30\"}"
                                }
                            }
                        ]
                    }
                }
            ]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let home = temp_home()?;
    let config_dir = home.join(".theseus");
    fs::create_dir_all(&config_dir)?;
    let config = serde_json::json!({
        "llm_request_settings": {
            "base_url": format!("http://{address}/chat"),
            "retries": 1,
            "request_timeout_seconds": 30,
            "connect_timeout_seconds": 5,
            "body": {
                "model": "test/model",
                "tool_choice": "auto"
            },
            "header": {
                "Authorization": "Bearer test",
                "Content-Type": "application/json"
            }
        },
        "agent_settings": {
            "max_turns": 2,
            "max_tool_output_bytes": 32768,
            "max_tool_bash_bytes": 8192,
            "max_context_tokens": 200000,
            "max_resume_traj": 100,
            "build_in_tools": ["bash"],
            "system_prompt": ["test"]
        },
        "mcp_servers": {}
    });
    fs::write(
        config_dir.join("config.jsonc"),
        serde_json::to_vec_pretty(&config)?,
    )?;

    Ok((home, server))
}

pub(crate) struct HeldJsonResponse {
    pub(crate) ready: std::sync::mpsc::Receiver<()>,
    pub(crate) release: std::sync::mpsc::Sender<()>,
    server: Option<thread::JoinHandle<()>>,
}
impl Drop for HeldJsonResponse {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}
pub(crate) fn held_json_fixture() -> io::Result<(PathBuf, HeldJsonResponse)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(WAIT_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(WAIT_TIMEOUT)).unwrap();
        let mut request = [0; 8192];
        if stream.read(&mut request).unwrap_or(0) == 0 {
            return;
        }
        let _ = ready_tx.send(());
        let _ = release_rx.recv_timeout(WAIT_TIMEOUT);
        let body = serde_json::json!({"choices":[{"message":{"role":"assistant","content":"**HELD_ANSWER**"},"finish_reason":"stop"}]}).to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    let home = temp_home()?;
    fs::create_dir_all(home.join(".theseus"))?;
    fs::write(
        home.join(".theseus/config.jsonc"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "llm_request_settings": {
                "base_url": format!("http://{address}/chat"), "retries": 1,
                "request_timeout_seconds": 30, "connect_timeout_seconds": 5,
                "body": {"model": "test/model"}, "header": {"Authorization": "Bearer test"}
            },
            "agent_settings": {"max_turns": 2, "max_tool_output_bytes": 32768, "max_tool_bash_bytes": 8192, "max_context_tokens": 200000, "max_resume_traj": 100, "build_in_tools": [], "system_prompt": ["test"]},
            "mcp_servers": {}
        }))?,
    )?;
    Ok((
        home,
        HeldJsonResponse {
            ready,
            release,
            server: Some(server),
        },
    ))
}
