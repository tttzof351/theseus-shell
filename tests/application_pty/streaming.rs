//! Real HTTP → AgentWorker → terminal checks. Every response boundary is held
//! by the test until the relevant rendered state or side effect is observed.
use super::*;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};

enum Action {
    Write(Vec<u8>),
    Close,
    ExpectClosed,
    Stop,
}
type Command = (Action, Sender<io::Result<()>>);

struct SseServer {
    commands: Sender<Command>,
    requests: Receiver<Value>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}

impl SseServer {
    fn start() -> io::Result<(PathBuf, Self)> {
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

    fn request(&self) -> io::Result<Value> {
        let request = self
            .requests
            .recv_timeout(WAIT_TIMEOUT)
            .map_err(io::Error::other)?;
        assert_eq!(request["stream"], true);
        Ok(request)
    }

    fn action(&self, action: Action) -> io::Result<()> {
        let (ack, result) = mpsc::channel();
        self.commands
            .send((action, ack))
            .map_err(io::Error::other)?;
        result
            .recv_timeout(WAIT_TIMEOUT)
            .map_err(io::Error::other)?
    }

    fn bytes(&self, text: impl AsRef<[u8]>) -> io::Result<()> {
        self.action(Action::Write(text.as_ref().to_vec()))
    }

    fn headers(&self) -> io::Result<()> {
        self.bytes("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\n\r\n")
    }

    fn event(&self, value: Value) -> io::Result<()> {
        self.bytes(format!("data: {value}\n\n"))
    }

    fn delta(&self, delta: Value) -> io::Result<()> {
        self.event(json!({"choices":[{"index":0,"delta":delta}]}))
    }

    fn content(&self, text: &str) -> io::Result<()> {
        self.delta(json!({"role":"assistant", "content":text}))
    }

    fn finish(&self, reason: &str) -> io::Result<()> {
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

struct Ui {
    app: ApplicationPty,
    server: SseServer,
    resizes: Vec<(usize, u16, u16)>,
    observer: RefCell<Observation>,
    xterm: xterm::Trace,
}

struct Observation {
    parser: vt100::Parser,
    offset: usize,
    resizes: usize,
}

struct Snapshot(vt100::Screen);
impl Snapshot {
    fn screen(&self) -> &vt100::Screen {
        &self.0
    }
}

impl Ui {
    fn start() -> io::Result<Self> {
        let (home, server) = SseServer::start()?;
        let app =
            ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
        Ok(Self {
            app,
            server,
            resizes: Vec::new(),
            observer: RefCell::new(Observation {
                parser: vt100::Parser::new(SIZE.rows, SIZE.cols, 20_000),
                offset: 0,
                resizes: 0,
            }),
            xterm: xterm::Trace::default(),
        })
    }

    fn parser(&self, bytes: &[u8], scroll_on_erase: bool) -> vt100::Parser {
        let mut terminal = terminal::Terminal::new(SIZE.rows, SIZE.cols, 20_000, scroll_on_erase);
        let mut offset = 0;
        for &(boundary, rows, cols) in &self.resizes {
            let boundary = boundary.min(bytes.len());
            terminal.process(&bytes[offset..boundary]);
            terminal.parser.screen_mut().set_size(rows, cols);
            offset = boundary;
        }
        terminal.process(&bytes[offset..]);
        terminal.parser
    }

    fn wait(&self, predicate: impl Fn(&vt100::Screen) -> bool) -> io::Result<Snapshot> {
        self.app.wait_until(|bytes| {
            let mut observer = self.observer.borrow_mut();
            let Observation {
                parser,
                offset,
                resizes,
            } = &mut *observer;
            for &(boundary, rows, cols) in &self.resizes[*resizes..] {
                let boundary = boundary.min(bytes.len());
                parser.process(&bytes[*offset..boundary]);
                parser.screen_mut().set_size(rows, cols);
                *offset = boundary;
                *resizes += 1;
            }
            parser.process(&bytes[*offset..]);
            *offset = bytes.len();
            predicate(parser.screen())
        })?;
        Ok(Snapshot(self.observer.borrow().parser.screen().clone()))
    }

    fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let offset = self.app.transcript_len();
        self.resizes.push((offset, rows, cols));
        self.xterm.resize(offset, rows, cols);
        self.app
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    fn history(&self) -> String {
        self.history_with_scroll_on_erase(true)
    }

    fn xterm_checkpoint(&self, name: &str) {
        // Freeze the byte boundary whose decoded state satisfied wait(). The
        // reader may already have appended part of a newer frame by now.
        self.xterm.checkpoint(self.observer.borrow().offset, name);
    }

    fn export_xterm(&self, name: &str) -> io::Result<()> {
        if std::env::var_os("THESEUS_XTERM_TRACE_DIR").is_none() {
            return Ok(());
        }
        // A shell marker can arrive before the lease returns to the renderer.
        // Record the final checkpoint only once the editable prompt is back.
        self.wait(|screen| {
            !screen.alternate_screen()
                && !screen.hide_cursor()
                && spinner(screen).is_none()
                && screen.cursor_position().1 > 0
                && screen
                    .rows(0, screen.size().1)
                    .nth(screen.cursor_position().0 as usize)
                    .is_some_and(|row| row.trim_end() == "tester theseus-shell>")
        })?;
        self.xterm_checkpoint("final");
        self.xterm.export(
            name,
            &self.app.transcript()[..self.observer.borrow().offset],
        )
    }

    fn history_with_scroll_on_erase(&self, scroll_on_erase: bool) -> String {
        let parser = self.parser(&self.app.transcript(), scroll_on_erase);
        normal_buffer_text(parser.screen())
    }
}

fn normal_buffer_text(screen: &vt100::Screen) -> String {
    let mut screen = screen.clone();
    let cols = screen.size().1;
    screen.set_scrollback(usize::MAX);
    let count = screen.scrollback();
    let mut rows = screen.rows(0, cols).collect::<Vec<_>>();
    for offset in (0..count).rev() {
        screen.set_scrollback(offset);
        rows.extend(screen.rows(0, cols).last());
    }
    rows.join("\n")
}

fn spinner(screen: &vt100::Screen) -> Option<(usize, String)> {
    screen
        .rows(0, screen.size().1)
        .enumerate()
        .find(|(_, row)| is_waiting_spinner(row))
}

fn assert_minimal_progress(screen: &vt100::Screen) {
    let text = screen.contents();
    for forbidden in [
        "Working",
        "Waiting for response",
        "attempt",
        "Connecting",
        "Thinking",
        "Receiving answer",
        "Receiving tool call",
        "Retry ",
    ] {
        assert!(!text.contains(forbidden), "{text}");
    }
    assert_eq!(
        screen
            .rows(0, screen.size().1)
            .filter(|row| is_waiting_spinner(row))
            .count(),
        1,
        "{text}"
    );
}

fn assert_bold_marker(screen: &vt100::Screen, marker: &str) {
    let (row, column) = screen
        .rows(0, screen.size().1)
        .enumerate()
        .find_map(|(row, line)| {
            line.find(marker)
                .map(|column| (row as u16, line[..column].chars().count() as u16))
        })
        .expect("visible Markdown marker");
    for offset in 0..marker.chars().count() as u16 {
        assert!(
            screen.cell(row, column + offset).unwrap().bold(),
            "marker is raw Markdown: {}",
            screen.contents()
        );
    }
}

fn log_events(home: &Path) -> io::Result<Vec<Value>> {
    let mut events = Vec::new();
    for file in fs::read_dir(home.join(".theseus/logs"))? {
        let file = file?;
        if file.file_name().to_string_lossy().ends_with("_log.jsonl") {
            let text = fs::read_to_string(file.path())?;
            // The final line may still be in the middle of the logger's write.
            events.extend(
                text.split_inclusive('\n')
                    .filter(|line| line.ends_with('\n'))
                    .map(serde_json::from_str)
                    .collect::<Result<Vec<Value>, _>>()?,
            );
        }
    }
    Ok(events)
}

#[test]
fn first_formatted_delta_precedes_done_and_keeps_short_footer_next_to_draft() -> io::Result<()> {
    for submission in ["/ask Что ты умеешь?\r", "/ask\rЧто ты умеешь?\r/end\r"]
    {
        let mut ui = Ui::start()?;
        ui.app.write("\x0c")?;
        ui.wait(|screen| !screen.contents().contains("Theseus shell wrapper"))?;
        ui.app.write(submission)?;
        ui.server.request()?;
        let first = ui.wait(|screen| spinner(screen).is_some())?;
        assert_minimal_progress(first.screen());
        let first_spinner = spinner(first.screen()).unwrap();
        let next =
            ui.wait(|screen| spinner(screen).is_some_and(|next| next.1 != first_spinner.1))?;
        assert_eq!(spinner(next.screen()).unwrap().0, first_spinner.0);
        assert_eq!(
            next.screen().cursor_position(),
            first.screen().cursor_position()
        );
        ui.server.headers()?;
        ui.server.bytes(": heartbeat\r\n\r\n")?;
        ui.app.write("NEXT_DRAFT")?;
        ui.server.content("**EARLY_MARKER**")?;
        // DONE is withheld until bold cells, footer and editable draft are visible.
        let early = ui.wait(|screen| {
            screen.contents().contains("EARLY_MARKER") && screen.contents().contains("NEXT_DRAFT")
        })?;
        assert_bold_marker(early.screen(), "EARLY_MARKER");
        assert_minimal_progress(early.screen());
        let rows = early.screen().rows(0, SIZE.cols).collect::<Vec<_>>();
        let answer = rows
            .iter()
            .position(|row| row.contains("EARLY_MARKER"))
            .unwrap();
        assert_eq!(spinner(early.screen()).unwrap().0, answer + 1, "{rows:?}");
        assert_eq!(early.screen().cursor_position().0 as usize, answer + 2);
        assert!(rows[answer + 3..].iter().all(|row| row.is_empty()));
        ui.server
            .event(json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}}))?;
        ui.server.finish("stop")?;
        ui.wait(|screen| {
            screen.contents().contains("EARLY_MARKER")
                && screen.contents().contains("NEXT_DRAFT")
                && spinner(screen).is_none()
        })?;
        ui.app.write("\x03")?;
        ui.app.write("printf 'AFTER_%s\\n' STREAM\r")?;
        ui.wait(|screen| {
            screen
                .rows(0, screen.size().1)
                .any(|row| row == "AFTER_STREAM")
        })?;
        let history = ui.history();
        assert_eq!(history.matches("EARLY_MARKER").count(), 1, "{history}");
        assert!(
            !history
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "{history}"
        );
        ui.app.exit()?;
    }
    Ok(())
}

#[test]
fn late_reference_code_table_and_unicode_publish_once_across_resize() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("/ask stream markdown\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("[**LINK_TITLE**][doc]")?;
    ui.wait(|screen| screen.contents().contains("[doc]"))?;
    ui.xterm_checkpoint("unresolved-reference");
    ui.server.content("\n\n[doc]: https://example.test/guide\n\n| Key | Value |\n| --- | --- |\n| TABLE_界 | Привет |\n")?;
    ui.wait(|screen| {
        screen
            .contents()
            .contains("LINK_TITLE (https://example.test/guide)")
            && screen.contents().contains("TABLE_界")
            && !screen.contents().contains("[doc]")
    })?;
    ui.xterm_checkpoint("resolved-reference");
    ui.server
        .content("| TABLE_🙂 | мир |\n\n```rust\nlet CODE_MARKER = \"Привет界🙂\";")?;
    ui.wait(|screen| screen.contents().contains("CODE_MARKER"))?;
    ui.resize(18, 60)?;
    ui.app.write("RESIZE_DRAFT")?;
    ui.wait(|screen| screen.contents().contains("RESIZE_DRAFT"))?;
    let data = format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"index":0,"delta":{"content":"\n```\n\n**UNICODE_界🙂**"}}]})
    );
    let boundary = data.find('界').unwrap() + 1;
    ui.server.bytes(&data.as_bytes()[..boundary])?;
    ui.server.bytes(&data.as_bytes()[boundary..])?;
    ui.wait(|screen| screen.contents().contains("UNICODE_界🙂"))?;
    ui.xterm_checkpoint("narrow-preview");
    ui.server.finish("stop")?;
    ui.wait(|screen| spinner(screen).is_none())?;
    ui.resize(18, 90)?;
    ui.app.write("\x03printf 'AFTER_%s\\n' MARKDOWN\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_MARKDOWN")
    })?;
    let history = ui.history();
    for marker in [
        "LINK_TITLE",
        "https://example.test/guide",
        "TABLE_界",
        "TABLE_🙂",
        "CODE_MARKER",
        "UNICODE_界🙂",
    ] {
        assert_eq!(history.matches(marker).count(), 1, "{marker}: {history}");
    }
    assert!(!history.contains("[doc]"), "{history}");
    ui.export_xterm("markdown-resize")?;
    ui.app.exit()
}

fn tool_delta(command: &str) -> Value {
    json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"bash","arguments":json!({"command":command}).to_string()}}]})
}

#[test]
fn long_bash_output_then_streamed_table_publish_once_without_preview_in_history() -> io::Result<()>
{
    let mut ui = Ui::start()?;
    ui.app.write("/ask list files\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    // More than a publication frame (128 rows), followed by a mutable table
    // taller than the viewport. Neither command text contains an output marker.
    ui.server.delta(tool_delta(
        "i=0; while [ $i -lt 180 ]; do printf 'BASH_%s_%03d\\n' ROW $i; i=$((i+1)); done",
    ))?;
    ui.server.finish("tool_calls")?;
    ui.server.request()?;
    ui.wait(|screen| screen.contents().contains("BASH_ROW_179"))?;
    ui.xterm_checkpoint("tool-tail");
    ui.server.headers()?;
    ui.server.delta(json!({"reasoning":"TABLE_REASONING"}))?;
    ui.server
        .content("TABLE_INTRO\n\n| File | Bytes |\n| --- | --- |\n")?;
    ui.app.write("KEPT_DRAFT")?;
    for i in 0..36 {
        ui.server
            .content(&format!("| TABLE_ROW_{i:03} | {} |\n", 1000 + i))?;
        ui.wait(|screen| {
            screen.contents().contains(&format!("TABLE_ROW_{i:03}"))
                && screen.contents().contains("KEPT_DRAFT")
                && spinner(screen).is_some()
        })?;
        if matches!(i, 18 | 35) {
            ui.xterm_checkpoint(&format!("table-preview-{i}"));
        }
    }
    ui.server.content("\nTABLE_SUMMARY")?;
    // Finish only after the complete mutable preview has been rendered. The
    // spinner may disappear before that preview's stable layout is installed.
    ui.wait(|screen| screen.contents().contains("TABLE_SUMMARY") && spinner(screen).is_some())?;
    ui.server.finish("stop")?;
    let markers = (0..180)
        .map(|i| format!("BASH_ROW_{i:03}"))
        .chain(["TABLE_REASONING".to_string(), "TABLE_INTRO".to_string()])
        .chain((0..36).map(|i| format!("TABLE_ROW_{i:03}")))
        .chain(["TABLE_SUMMARY".to_string()])
        .collect::<Vec<_>>();
    // Backend completion removes the spinner before the layout worker marks
    // the answer stable. Wait for actual publication without a shell command
    // forcing prepare_document(), or this checkpoint can still be a preview.
    ui.wait(|screen| {
        if !screen.contents().contains("TABLE_SUMMARY")
            || spinner(screen).is_some()
            || screen.hide_cursor()
        {
            return false;
        }
        let normal = normal_buffer_text(screen);
        markers.iter().all(|marker| normal.contains(marker))
    })?;
    ui.xterm_checkpoint("table-complete");
    // Grow native history again after completion, exposing a leaked preview
    // even if it would otherwise remain hidden above the live viewport.
    ui.app.write(&"\x7f".repeat("KEPT_DRAFT".len()))?;
    ui.app.write("printf 'AFTER_%s\\n' TABLE\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_TABLE")
    })?;
    assert_eq!(
        terminal::scroll_up_commands(&ui.app.transcript()),
        0,
        "CSI S discards published rows in xterm.js instead of saving them to scrollback"
    );
    for scroll_on_erase in [false, true] {
        let history = ui.history_with_scroll_on_erase(scroll_on_erase);
        let mut previous = 0;
        for marker in markers.iter().map(String::as_str).chain(["AFTER_TABLE"]) {
            assert_eq!(
                history.matches(marker).count(),
                1,
                "{marker}, scroll_on_erase={scroll_on_erase}: {history}"
            );
            let position = history.find(marker).unwrap();
            assert!(position >= previous, "{marker}: {history}");
            previous = position;
        }
        assert!(!history.contains("KEPT_DRAFT"), "{history}");
        assert!(
            !history
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "{history}"
        );
    }
    ui.export_xterm("long-bash-table")?;
    ui.app.exit()
}

#[test]
fn late_reasoning_and_tools_keep_order_and_execute_once_only_after_done() -> io::Result<()> {
    let mut ui = Ui::start()?;
    let effect = ui.app.home.join("effect");
    ui.app.write("/ask ordered response\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("**CONTENT_BEFORE_TOOL**")?;
    ui.wait(|screen| screen.contents().contains("CONTENT_BEFORE_TOOL"))?;
    ui.server.delta(json!({"reasoning":"LATE_REASONING"}))?;
    let preview = ui.wait(|screen| screen.contents().contains("LATE_REASONING"))?;
    let text = preview.screen().contents();
    assert!(
        text.find("LATE_REASONING") < text.find("CONTENT_BEFORE_TOOL"),
        "{text}"
    );
    ui.server.delta(tool_delta(&format!(
        "printf x >> '{}'; printf 'TOOL_%s\\n' RESULT",
        effect.display()
    )))?;
    ui.server
        .event(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}))?;
    // Even complete JSON arguments and finish_reason are insufficient without DONE.
    ui.app.write("TOOL_DRAFT")?;
    ui.wait(|screen| screen.contents().contains("TOOL_DRAFT"))?;
    assert!(!effect.exists());
    assert!(!ui.history().contains("arguments"));
    ui.server.bytes("data: [DONE]\n\n")?;
    ui.server.action(Action::ExpectClosed)?;
    let continuation = ui.server.request()?;
    assert_eq!(fs::read_to_string(&effect)?, "x");
    let messages = continuation["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["tool_calls"].is_array())
        .unwrap();
    assert_eq!(assistant["reasoning"], "LATE_REASONING");
    assert_eq!(assistant["content"], "**CONTENT_BEFORE_TOOL**");
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        1
    );
    ui.wait(|screen| screen.contents().contains("TOOL_RESULT"))?;
    ui.server.headers()?;
    ui.server.content("**CONTENT_AFTER_TOOL**")?;
    ui.wait(|screen| screen.contents().contains("CONTENT_AFTER_TOOL"))?;
    ui.server.finish("stop")?;
    ui.wait(|screen| spinner(screen).is_none())?;
    let history = ui.history();
    let mut previous = 0;
    for marker in [
        "LATE_REASONING",
        "CONTENT_BEFORE_TOOL",
        "TOOL_RESULT",
        "CONTENT_AFTER_TOOL",
    ] {
        assert_eq!(history.matches(marker).count(), 1, "{history}");
        let position = history.find(marker).unwrap();
        assert!(position >= previous, "{history}");
        previous = position;
    }
    assert_eq!(fs::read_to_string(effect)?, "x");
    ui.app.write("\x03")?;
    ui.app.exit()
}

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
fn tool_only_stream_never_executes_cancelled_truncated_or_invalid_calls() -> io::Result<()> {
    for outcome in ["cancel", "length", "invalid"] {
        let mut ui = Ui::start()?;
        let effect = ui.app.home.join("forbidden-effect");
        ui.app.write("/ask held tool\r")?;
        ui.server.request()?;
        ui.server.headers()?;
        let mut delta = tool_delta(&format!("printf forbidden > '{}'", effect.display()));
        if outcome == "invalid" {
            delta["tool_calls"][0]["function"]["arguments"] = json!("{\"command\":");
        }
        ui.server.delta(delta)?;
        let before = ui.wait(|screen| spinner(screen).is_some())?;
        assert_minimal_progress(before.screen());
        assert!(!effect.exists());
        assert!(!before.screen().contents().contains("forbidden-effect"));
        if outcome == "cancel" {
            ui.app.write("\x03")?;
            ui.server.action(Action::ExpectClosed)?;
        } else {
            ui.server.finish(if outcome == "length" {
                "length"
            } else {
                "tool_calls"
            })?;
        }
        ui.wait(|screen| {
            spinner(screen).is_none()
                && (screen.contents().contains("interrupted")
                    || screen.contents().contains("agent:"))
        })?;
        assert!(
            !effect.exists(),
            "{outcome} executed partial tool arguments"
        );
        assert!(!ui.history().contains("forbidden-effect"));
        ui.app.exit()?;
    }
    Ok(())
}

#[test]
fn sse_bash_preview_keeps_editor_prompt_style_before_and_after_tool_cancel() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("\x0c")?;
    let editor = ui.wait(|screen| !screen.contents().contains("Theseus shell wrapper"))?;
    let editor_row = editor.screen().cursor_position().0;
    ui.app.write("/ask bash preview\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    // Explicit spaces reproduce the trailing blank cells left when the
    // renderer overwrites the longer editor prompt with this output in CI.
    ui.server.delta(tool_delta(
        "printf 'BASH_%s            \\n' READY; exec sleep 30",
    ))?;
    ui.server.finish("tool_calls")?;
    let assert_style = |screen: &vt100::Screen| {
        let prefix = "tester theseus-shell> ";
        let row = screen
            .rows(0, screen.size().1)
            .position(|line| line.starts_with(&format!("{prefix}printf ")))
            .expect("agent preview") as u16;
        for column in 0..prefix.chars().count() as u16 {
            let expected = editor.screen().cell(editor_row, column).unwrap();
            let actual = screen.cell(row, column).unwrap();
            assert_eq!(actual.contents(), expected.contents());
            assert_eq!(actual.fgcolor(), expected.fgcolor(), "color at {column}");
            assert_eq!(actual.bold(), expected.bold(), "weight at {column}");
        }
        let marker = screen
            .cell(row, (prefix.chars().count() - 2) as u16)
            .unwrap();
        assert_eq!(marker.fgcolor(), vt100::Color::Default);
        assert!(!marker.bold());
    };
    let before = ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row.trim_end() == "BASH_READY")
    })?;
    assert_style(before.screen());
    ui.app.write("\x03")?;
    let after = ui.wait(|screen| {
        screen
            .contents()
            .contains("Agent tool execution interrupted")
    })?;
    assert_style(after.screen());
    assert!(after.screen().contents().contains("BASH_READY"));
    ui.app.exit()
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
    ui.server.content("RETRY_COMPLETE")?;
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
fn plain_sse_cli_commits_text_once_and_routes_truncation_to_stderr() -> io::Result<()> {
    for args in [vec!["-p", "plain stream"], vec!["/ask", "plain stream"]] {
        let (home, server) = SseServer::start()?;
        let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
            .args(args)
            .env("HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        server.request()?;
        server.headers()?;
        server.content("**PLAIN_")?;
        server.delta(json!({"content":"ANSWER**", "reasoning":"PLAIN_REASONING"}))?;
        server.finish("length")?;
        let output = wait_cli_exit(child, Duration::from_secs(2))?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.ends_with("PLAIN_REASONING\n**PLAIN_ANSWER**\n"),
            "{stdout}"
        );
        assert_eq!(stdout.matches("PLAIN_REASONING").count(), 1);
        assert_eq!(stdout.matches("PLAIN_ANSWER").count(), 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("Response truncated"));
        assert!(!output.stdout.contains(&0x1b) && !output.stderr.contains(&0x1b));
        fs::remove_dir_all(home)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn plain_sse_sigint_preserves_prefix_closes_http_and_exits_130() -> io::Result<()> {
    for prefix in [false, true] {
        let (home, server) = SseServer::start()?;
        let child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
            .args(["-p", "interrupt stream"])
            .env("HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        server.request()?;
        if prefix {
            server.headers()?;
            server.content("PLAIN_PREFIX")?;
            let started = Instant::now();
            while !log_events(&home)?
                .iter()
                .any(|event| event["event"] == "llm_first_semantic_delta")
            {
                assert!(started.elapsed() < WAIT_TIMEOUT);
                thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        server.action(Action::ExpectClosed)?;
        let output = wait_cli_exit(child, Duration::from_secs(2))?;
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            if prefix { "PLAIN_PREFIX\n" } else { "" }
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
        assert!(!output.stdout.contains(&0x1b) && !output.stderr.contains(&0x1b));
        fs::remove_dir_all(home)?;
    }
    Ok(())
}

#[test]
fn plain_sse_broken_pipe_cancels_live_tool_output() -> io::Result<()> {
    let (home, server) = SseServer::start()?;
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["-p", "tool stream"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    drop(child.stdout.take());
    server.request()?;
    server.headers()?;
    server.delta(tool_delta("printf 'PIPE_%s\\n' READY; exec sleep 30"))?;
    server.finish("tool_calls")?;
    let output = wait_cli_exit(child, Duration::from_secs(2))?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    fs::remove_dir_all(home)?;
    Ok(())
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
fn sse_shell_leases_stdin_and_real_vim_return_to_streaming_without_replay() -> io::Result<()> {
    assert!(
        ProcessCommand::new("vim")
            .arg("--version")
            .output()?
            .status
            .success(),
        "real Vim fixture requires Vim"
    );
    let mut ui = Ui::start()?;
    ui.app.write("/ask before leases\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("**SSE_BEFORE_LEASES**")?;
    ui.server.finish("stop")?;
    ui.wait(|screen| screen.contents().contains("SSE_BEFORE_LEASES") && spinner(screen).is_none())?;
    ui.app
        .write("read value; printf 'INPUT_%s\\n' \"$value\"\rIMMEDIATE\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "INPUT_IMMEDIATE")
    })?;
    ui.app.write(&format!(
        "printf 'WRAPPED_%s {} WRAPPED_%s\\n' START END\r",
        "x".repeat(125)
    ))?;
    ui.wait(|screen| {
        screen.contents().contains("WRAPPED_START") && screen.contents().contains("WRAPPED_END")
    })?;
    ui.resize(18, 60)?;
    let file = ui.app.home.join("sse-vim.txt");
    ui.app.write(&format!(
        "vim -u NONE -U NONE -i NONE -n -N --cmd 'set t_RV= t_u7=' '{}'\r",
        file.display()
    ))?;
    ui.wait(|screen| screen.alternate_screen() && screen.contents().contains("sse-vim.txt"))?;
    ui.app.write("iVIM_SSE_TEXT")?;
    ui.wait(|screen| screen.contents().contains("VIM_SSE_TEXT"))?;
    ui.xterm_checkpoint("vim-edit");
    ui.resize(18, 90)?;
    ui.app.write("_RESIZED\x1b:wq\r")?;
    ui.wait(|screen| {
        !screen.alternate_screen()
            && screen
                .rows(0, screen.size().1)
                .nth(screen.cursor_position().0 as usize)
                .is_some_and(|row| row.trim_end().ends_with('>'))
    })?;
    assert_eq!(fs::read_to_string(file)?, "VIM_SSE_TEXT_RESIZED\n");
    ui.xterm_checkpoint("after-vim");
    ui.app.write("printf 'AFTER_%s\\n' VIM\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_VIM")
    })?;
    ui.app.write("/ask after leases\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server.content("**SSE_AFTER_LEASES**")?;
    let early = ui.wait(|screen| screen.contents().contains("SSE_AFTER_LEASES"))?;
    assert_bold_marker(early.screen(), "SSE_AFTER_LEASES");
    ui.server.finish("stop")?;
    ui.wait(|screen| spinner(screen).is_none())?;
    let history = ui.history();
    for marker in [
        "SSE_BEFORE_LEASES",
        "SSE_AFTER_LEASES",
        "INPUT_IMMEDIATE",
        "WRAPPED_START",
        "WRAPPED_END",
    ] {
        assert_eq!(history.matches(marker).count(), 1, "{marker}: {history}");
    }
    assert!(
        !history.contains("VIM_SSE_TEXT"),
        "alternate buffer leaked: {history}"
    );
    ui.export_xterm("vim-handoff")?;
    ui.app.exit()
}

#[cfg(unix)]
#[test]
fn large_active_sse_markdown_keeps_input_resize_cancel_and_exit_budgets() -> io::Result<()> {
    for (kind, source, expected_size) in [
        (
            "paragraph",
            (0..60000)
                .map(|i| format!("**WORD_{i:04}** "))
                .collect::<String>()
                + "EXIT_TAIL",
            890_009,
        ),
        (
            "fence",
            "```rust\n".to_owned()
                + &(0..16000)
                    .map(|i| format!("let SOURCE_{i:04} = \"Привет 界\";\n"))
                    .collect::<String>()
                + "// EXIT_TAIL",
            614_020,
        ),
    ] {
        assert_eq!(source.len(), expected_size);
        let mut ui = Ui::start()?;
        ui.resize(18, 60)?;
        ui.app.write("/ask large streaming fixture\r")?;
        ui.server.request()?;
        ui.server.headers()?;
        ui.server.content(&source)?;
        ui.wait(|screen| screen.contents().contains("EXIT_TAIL"))?;
        let started = Instant::now();
        ui.app.write("LARGE_DRAFT")?;
        ui.wait(|screen| screen.contents().contains("LARGE_DRAFT"))?;
        let input = started.elapsed();
        assert!(
            input <= Duration::from_millis(250),
            "{kind}: visible input {input:?}"
        );
        let started = Instant::now();
        ui.resize(18, 90)?;
        ui.app.write("_RESIZED")?;
        ui.wait(|screen| {
            screen.contents().contains("LARGE_DRAFT_RESIZED") && screen.cursor_position().1 > 21
        })?;
        let resize = started.elapsed();
        assert!(
            resize <= Duration::from_millis(250),
            "{kind}: visible resize {resize:?}"
        );
        let started = Instant::now();
        ui.app.write("\x03")?;
        ui.wait(|screen| {
            screen.contents().contains("interrupted")
                && screen.contents().contains("LARGE_DRAFT_RESIZED")
                && spinner(screen).is_none()
        })?;
        let cancel = started.elapsed();
        assert!(
            cancel <= Duration::from_millis(250),
            "{kind}: visible cancel {cancel:?}"
        );
        ui.server.action(Action::ExpectClosed)?;
        ui.app.write("\x03")?;
        let started = Instant::now();
        ui.app.write("/exit\r")?;
        while ui.app.child.try_wait()?.is_none() {
            assert!(
                started.elapsed() <= Duration::from_secs(2),
                "{kind}: exit/publication exceeded 2 s"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let exit = started.elapsed();
        eprintln!(
            "SSE {kind} {expected_size} bytes: input={input:?}, resize={resize:?}, cancel={cancel:?}, exit={exit:?}"
        );
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(ui.app.master.as_raw_fd().unwrap(), attributes.as_mut_ptr()) },
            0
        );
        let flags = unsafe { attributes.assume_init().c_lflag };
        assert_eq!(
            flags & (libc::ICANON | libc::ECHO | libc::ISIG),
            libc::ICANON | libc::ECHO | libc::ISIG
        );
        let history = ui.history();
        assert_eq!(history.matches("EXIT_TAIL").count(), 1);
        assert_eq!(
            history
                .lines()
                .filter(|line| line.trim() == "[interrupted]")
                .count(),
            1
        );
    }
    Ok(())
}

#[test]
fn a_single_long_code_line_does_not_stall_streaming_layout() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.resize(18, 60)?;
    ui.app.write("/ask unbroken code\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    ui.server
        .content(&("```text\n".to_owned() + &"x".repeat(614_002) + "\nLONG_CODE_TAIL"))?;
    ui.wait(|screen| screen.contents().contains("LONG_CODE_TAIL"))?;
    ui.app.write("\x03")?;
    ui.server.action(Action::ExpectClosed)?;
    ui.wait(|screen| spinner(screen).is_none() && screen.contents().contains("interrupted"))?;
    ui.app.exit()
}

#[test]
fn late_reference_shrinks_footer_after_clear_and_background_resize() -> io::Result<()> {
    let mut ui = Ui::start()?;
    ui.app.write("/ask footer resize\r")?;
    ui.server.request()?;
    ui.server.headers()?;
    // Keep the real background layout worker active even after clearing the
    // visible prefix. The retained source is above its 128 KiB threshold.
    ui.server
        .content(&("**HIDDEN_ROW** word\n\n".repeat(8000) + "HIDDEN_TAIL"))?;
    ui.wait(|screen| screen.contents().contains("HIDDEN_TAIL"))?;
    ui.app.write("\x0cFOOTER_DRAFT")?;
    ui.wait(|screen| {
        screen.contents().contains("FOOTER_DRAFT") && !screen.contents().contains("HIDDEN_TAIL")
    })?;
    let reference = "long_reference_identifier_".repeat(7);
    ui.server
        .content(&format!("\n\n[**SHRINK_MARKER**][{reference}]"))?;
    ui.wait(|screen| screen.contents().contains("SHRINK_MARKER"))?;
    let started = Instant::now();
    ui.resize(18, 60)?;
    ui.app.write("_RESIZED")?;
    let before = ui.wait(|screen| {
        screen.contents().contains("FOOTER_DRAFT_RESIZED")
            && screen.contents().contains("SHRINK_MARKER")
    })?;
    assert!(started.elapsed() <= Duration::from_millis(250));
    let before_row = spinner(before.screen()).unwrap().0;
    assert_eq!(before.screen().cursor_position().0 as usize, before_row + 1);
    assert!(
        before_row < 16,
        "short cleared output was pushed to the bottom: {}",
        before.screen().contents()
    );
    ui.server.content(&format!("\n\n[{reference}]: x"))?;
    let after = ui.wait(|screen| {
        screen.contents().contains("SHRINK_MARKER (x)")
            && !screen.contents().contains("long_reference")
    })?;
    let after_row = spinner(after.screen()).unwrap().0;
    assert!(
        after_row < before_row,
        "footer did not follow the shrinking preview"
    );
    assert_eq!(after.screen().cursor_position().0 as usize, after_row + 1);
    assert!(
        after
            .screen()
            .rows(0, 60)
            .skip(after_row + 2)
            .all(|row| row.is_empty())
    );
    assert_minimal_progress(after.screen());
    ui.server.finish("stop")?;
    ui.wait(|screen| {
        spinner(screen).is_none() && screen.contents().contains("FOOTER_DRAFT_RESIZED")
    })?;
    ui.app.write("\x03printf 'AFTER_%s\\n' SHRINK\r")?;
    ui.wait(|screen| {
        screen
            .rows(0, screen.size().1)
            .any(|row| row == "AFTER_SHRINK")
    })?;
    let history = ui.history();
    assert_eq!(history.matches("SHRINK_MARKER").count(), 1);
    assert!(
        !history.contains("HIDDEN_ROW") && !history.contains("long_reference"),
        "cleared or superseded source was published"
    );
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
