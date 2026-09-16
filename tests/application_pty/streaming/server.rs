//! Controlled SSE server and acknowledgement boundaries.
use super::*;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};

pub(super) enum Action {
    Write(Vec<u8>),
    Close,
    ExpectClosed,
    Stop,
}

type Command = (Action, Sender<io::Result<()>>);

pub(super) struct SseServer {
    commands: Sender<Command>,
    pub(super) requests: Receiver<Value>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}
impl SseServer {
    pub(super) fn start() -> io::Result<(PathBuf, Self)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let (commands, incoming) = mpsc::channel::<Command>();
        let (requests_tx, requests) = mpsc::channel();
        let thread = thread::spawn(move || {
            loop {
                let deadline = Instant::now() + WAIT_TIMEOUT;
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) => return Err(error),
                    }
                    match incoming.recv_timeout(Duration::from_millis(5)) {
                        Ok((Action::Stop, ack)) => {
                            let _ = ack.send(Ok(()));
                            return Ok(());
                        }
                        Ok((_, ack)) => {
                            let _ = ack.send(Err(io::Error::other("no HTTP connection")));
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                        Err(_) => {}
                    }
                    if Instant::now() > deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "SSE accept"));
                    }
                };
                stream.set_nonblocking(false)?;
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(WAIT_TIMEOUT))?;
                stream.set_write_timeout(Some(WAIT_TIMEOUT))?;
                requests_tx
                    .send(read_request(&mut stream)?)
                    .map_err(io::Error::other)?;
                loop {
                    let (action, ack) = match incoming.recv_timeout(WAIT_TIMEOUT) {
                        Ok(command) => command,
                        Err(_) => return Ok(()),
                    };
                    match action {
                        Action::Write(bytes) => {
                            let result = stream.write_all(&bytes).and_then(|_| stream.flush());
                            let _ = ack.send(result);
                        }
                        Action::Close => {
                            drop(stream);
                            let _ = ack.send(Ok(()));
                            break;
                        }
                        Action::ExpectClosed => {
                            let result = match stream.read(&mut [0; 1]) {
                                Ok(0) => Ok(()),
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        io::ErrorKind::ConnectionReset
                                            | io::ErrorKind::ConnectionAborted
                                    ) =>
                                {
                                    Ok(())
                                }
                                other => Err(io::Error::other(format!(
                                    "HTTP connection survived completion/cancel: {other:?}"
                                ))),
                            };
                            let _ = ack.send(result);
                            break;
                        }
                        Action::Stop => {
                            let _ = ack.send(Ok(()));
                            return Ok(());
                        }
                    }
                }
            }
        });
        let home = temp_home()?;
        fs::create_dir_all(home.join(".theseus"))?;
        fs::write(
            home.join(".theseus/config.jsonc"),
            serde_json::to_vec_pretty(&json!({
                "llm_request_settings": {
                    "base_url": format!("http://{address}/chat"), "retries": 2,
                    "request_timeout_seconds": 30, "connect_timeout_seconds": 5,
                    "stream_idle_timeout_seconds": 10,
                    "body": {"model": "test/model", "stream": true},
                    "header": {"Authorization": "Bearer fixture"}
                },
                "agent_settings": {"max_turns": 5, "max_tool_output_bytes": 32768,
                    "max_tool_bash_bytes": 8192, "max_context_tokens": 200000,
                    "max_resume_traj": 100, "build_in_tools": ["bash"], "system_prompt": ["test"]},
                "mcp_servers": {}
            }))?,
        )?;
        Ok((
            home,
            Self {
                commands,
                requests,
                thread: Some(thread),
            },
        ))
    }

    pub(super) fn request(&self) -> io::Result<Value> {
        let request = self
            .requests
            .recv_timeout(WAIT_TIMEOUT)
            .map_err(io::Error::other)?;
        assert_eq!(request["stream"], true);
        Ok(request)
    }

    pub(super) fn action(&self, action: Action) -> io::Result<()> {
        let (ack, result) = mpsc::channel();
        self.commands
            .send((action, ack))
            .map_err(io::Error::other)?;
        result
            .recv_timeout(WAIT_TIMEOUT)
            .map_err(io::Error::other)?
    }

    pub(super) fn bytes(&self, text: impl AsRef<[u8]>) -> io::Result<()> {
        self.action(Action::Write(text.as_ref().to_vec()))
    }

    pub(super) fn headers(&self) -> io::Result<()> {
        self.bytes("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\n\r\n")
    }

    pub(super) fn event(&self, value: Value) -> io::Result<()> {
        self.bytes(format!("data: {value}\n\n"))
    }

    pub(super) fn delta(&self, delta: Value) -> io::Result<()> {
        self.event(json!({"choices":[{"index":0,"delta":delta}]}))
    }

    pub(super) fn content(&self, text: &str) -> io::Result<()> {
        self.delta(json!({"role":"assistant", "content":text}))
    }

    pub(super) fn finish(&self, reason: &str) -> io::Result<()> {
        self.event(json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}))?;
        self.bytes("data: [DONE]\n\n")?;
        self.action(Action::ExpectClosed)
    }
}

impl Drop for SseServer {
    fn drop(&mut self) {
        let _ = self.commands.send((Action::Stop, mpsc::channel().0));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<Value> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = find_bytes(&bytes, b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .ok_or_else(|| io::Error::other("missing request Content-Length"))?;
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length])
                    .map_err(io::Error::other);
            }
        }
        if bytes.len() > 40 * 1024 * 1024 {
            return Err(io::Error::other("oversized fixture request"));
        }
    }
}
