//! End-to-end PTY tests of a replaceable backend, without a production test mode.

use super::*;
use crate::{
    agent::worker::AgentWorker,
    common::{
        cancellation::CancellationEvent,
        events::{BlockKind, EventSink, Outcome, OutputEvent},
    },
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::Read,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(5);

mod lifecycle_tests;
mod mixed_output_tests;

fn assert_responsive(started: Instant, scenario: &str) {
    let elapsed = started.elapsed();
    eprintln!("{scenario}: {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(250),
        "{scenario} exceeded the 250 ms UI response budget: {elapsed:?}"
    );
}

#[test]
#[ignore = "subprocess entry point for the managed UI PTY tests"]
fn fixture_child() {
    let directory = PathBuf::from(env::var_os("THESEUS_UI_FIXTURE").expect("fixture directory"));
    let mut app = Application::new().unwrap();
    let script_directory = directory.clone();
    app.agent = AgentWorker::with_executor(
        app.config.clone(),
        app.logger.clone(),
        move |agent, operation, output, cancellation| match operation {
            operation @ crate::agent::worker::Operation::Configure { .. } => {
                if script_directory.join("hold-configuration").exists() {
                    let _ = output.activity("Applying configuration", "held fixture");
                    let deadline = Instant::now() + TIMEOUT;
                    while !script_directory.join("release-configuration").exists() {
                        if Instant::now() >= deadline {
                            return Err(io::ErrorKind::TimedOut.into());
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                crate::agent::worker::execute(agent, operation, output, cancellation)
            }
            crate::agent::worker::Operation::Run { .. } => {
                run_script(&script_directory, output, cancellation)
            }
            operation @ crate::agent::worker::Operation::ModelCatalog { .. } => {
                if script_directory.join("hold-model-catalog").exists() {
                    output.activity("Loading models", "held fixture")?;
                    let deadline = Instant::now() + TIMEOUT;
                    while !script_directory.join("release-model-catalog").exists() {
                        if cancellation.is_cancelled() {
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        if Instant::now() >= deadline {
                            return Err(io::ErrorKind::TimedOut.into());
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                crate::agent::worker::execute(agent, operation, output, cancellation)
            }
            operation => crate::agent::worker::execute(agent, operation, output, cancellation),
        },
    )
    .unwrap();
    app.clear_output();
    let fault = env::var("THESEUS_CHANNEL_FAULT")
        .ok()
        .map(|mode| install_channel_fault(&mut app, directory, mode == "drop-completion"));
    let result = run_interactive_application(app, false);
    if let Some(worker) = fault {
        worker.join().unwrap();
    }
    result.unwrap();
}

fn install_channel_fault(
    app: &mut Application,
    directory: PathBuf,
    drop_completion: bool,
) -> thread::JoinHandle<()> {
    use crate::agent::worker::{ActiveOperation, Completion};
    let cancellation = CancellationEvent::new();
    let (output, events) = EventSink::channel(cancellation.clone());
    let id = output.operation();
    let (completion_tx, completion) = std::sync::mpsc::channel();
    app.document.start_operation(id);
    app.active_operation = Some(ActiveOperation {
        id,
        events,
        completion,
        cancellation: cancellation.clone(),
        finished: false,
        output_error: None,
        started: Instant::now(),
        input: "/ask channel fault".into(),
    });
    thread::spawn(move || {
        output.emit(OutputEvent::Started).unwrap();
        let block = output.start_block(BlockKind::Markdown).unwrap();
        output.text(block, "**CHANNEL_PREFIX**").unwrap();
        output.activity("Fixture waiting", "").unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while !directory.join("1.json").exists() && !cancellation.is_cancelled() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        drop(output); // No Finished event; the executor is still alive.
        while !cancellation.is_cancelled() {
            assert!(
                Instant::now() < deadline,
                "UI did not cancel the disconnected producer"
            );
            thread::sleep(Duration::from_millis(2));
        }
        fs::write(directory.join("fault-cancelled"), b"ok").unwrap();
        while !directory.join("2.json").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        if !drop_completion {
            let _ = completion_tx.send(Completion {
                result: Ok("must not turn protocol failure into success".into()),
                outcome: Outcome::Completed,
                logger: None,
            });
        }
    })
}

fn run_script(
    directory: &Path,
    output: &EventSink,
    cancellation: &CancellationEvent,
) -> io::Result<String> {
    let mut block = output.start_block(BlockKind::Markdown)?;
    output.activity("Fixture waiting", "")?;
    for step in 1.. {
        let path = directory.join(format!("{step}.json"));
        let deadline = Instant::now() + TIMEOUT;
        while !path.exists() {
            if cancellation.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture step did not arrive",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
        let command: Value = serde_json::from_slice(&fs::read(path)?)?;
        match command["action"].as_str().unwrap() {
            "next" => {
                output.finish_block(block, Outcome::Completed)?;
                block = output.start_block(match command["text"].as_str().unwrap() {
                    "reasoning" => BlockKind::Reasoning,
                    "markdown" => BlockKind::Markdown,
                    "tool" => BlockKind::ToolOutput,
                    kind => panic!("unknown fixture block kind {kind}"),
                })?;
            }
            "bytes" | "stderr" => output.bytes(
                block,
                u8::from(command["action"] == "stderr"),
                command["text"].as_str().unwrap().as_bytes(),
            )?,
            "append" => output.text(block, command["text"].as_str().unwrap())?,
            "replace" => output.emit(OutputEvent::BlockReplaced {
                id: block,
                revision: step,
                text: command["text"].as_str().unwrap().into(),
            })?,
            "finish" => {
                output.finish_block(block, Outcome::Completed)?;
                return Ok(String::new());
            }
            "late" => {
                output.finish_block(block, Outcome::Completed)?;
                output.text(block, command["text"].as_str().unwrap())?;
                return Ok(String::new());
            }
            "fail" => {
                let message = command["text"].as_str().unwrap();
                return Err(io::Error::other(
                    if message.is_empty() {
                        "injected failure"
                    } else {
                        message
                    }
                    .to_owned(),
                ));
            }
            "panic" => panic!("injected worker panic"),
            "flood" => {
                output.text(block, "```rust\n")?;
                let chunk = (0..48)
                    .map(|i| format!("let FLOW_{i:02} = \"界\";\n"))
                    .collect::<String>();
                let deadline = Instant::now() + TIMEOUT;
                while Instant::now() < deadline {
                    output.text(block, &chunk)?;
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "flood was not cancelled",
                ));
            }
            other => panic!("unknown fixture action {other}"),
        }
        output.activity("Fixture waiting", &format!("step {step}"))?;
        fs::write(directory.join(format!("{step}.accepted")), b"ok")?;
    }
    unreachable!()
}

struct UiPty {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    input: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    reader: Option<thread::JoinHandle<()>>,
    directory: PathBuf,
    step: u64,
}

impl UiPty {
    fn start() -> Self {
        Self::start_with_fault(None)
    }

    fn start_with_fault(fault: Option<&str>) -> Self {
        let directory = env::temp_dir().join(format!(
            "theseus-ui-fixture-{}-{}",
            std::process::id(),
            common::events::OperationId::next().0
        ));
        fs::create_dir_all(&directory).unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 18,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env::current_exe().unwrap());
        command.args([
            "--exact",
            "application::managed_ui_tests::fixture_child",
            "--ignored",
            "--nocapture",
        ]);
        command.env("HOME", &directory);
        command.env("SHELL", "/bin/sh");
        command.env("USER", "fixture");
        command.env("TERM", "xterm-256color");
        command.env("THESEUS_UI_FIXTURE", &directory);
        if let Some(fault) = fault {
            command.env("THESEUS_CHANNEL_FAULT", fault);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let input = pair.master.take_writer().unwrap();
        let parser = Arc::new(Mutex::new(vt100::Parser::new(18, 60, 2000)));
        let reader_parser = parser.clone();
        let reader = thread::spawn(move || {
            let mut bytes = [0; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => reader_parser
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .process(&bytes[..count]),
                }
            }
        });
        let mut result = Self {
            child,
            master: pair.master,
            input,
            parser,
            reader: Some(reader),
            directory,
            step: 0,
        };
        result.wait(|screen| screen.contents().contains("fixture "));
        if fault.is_none() {
            result.write("/ask fixture\r");
        }
        result.wait(|screen| screen.contents().contains("Fixture waiting"));
        result
    }

    fn write(&mut self, bytes: &str) {
        self.input.write_all(bytes.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }

    fn command(&mut self, action: &str, text: &str) {
        self.step += 1;
        let temporary = self.directory.join("step.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(&json!({"action":action,"text":text})).unwrap(),
        )
        .unwrap();
        fs::rename(
            temporary,
            self.directory.join(format!("{}.json", self.step)),
        )
        .unwrap();
    }

    fn wait(&self, predicate: impl Fn(&vt100::Screen) -> bool) {
        let start = Instant::now();
        loop {
            {
                let parser = self.parser.lock().unwrap();
                if predicate(parser.screen()) {
                    return;
                }
                if start.elapsed() >= TIMEOUT {
                    let contents = parser.screen().contents();
                    // Keep the reader draining during unwinding/child cleanup.
                    // Poisoning its mutex here can strand a PTY writer on exit.
                    drop(parser);
                    panic!("PTY timeout; screen:\n{contents}");
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn history(&self) -> Vec<String> {
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let (rows, _) = screen.size();
        screen.set_scrollback(usize::MAX);
        let scrollback = screen.scrollback();
        let mut lines = screen.rows(0, u16::MAX).collect::<Vec<_>>();
        for offset in (0..scrollback).rev() {
            screen.set_scrollback(offset);
            lines.push(screen.rows(0, u16::MAX).nth(rows as usize - 1).unwrap());
        }
        lines
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }
}

impl Drop for UiPty {
    fn drop(&mut self) {
        let _ = self.input.write_all(b"\x03\x03/exit\r");
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn live_markdown_is_formatted_before_finish_and_final_block_is_not_duplicated() {
    let mut ui = UiPty::start();
    ui.command("append", "**FIRST_MARKER**\n\n```rust\nlet answer = 42;");
    ui.wait(|screen| {
        let (rows, cols) = screen.size();
        (0..rows).any(|row| {
            (0..cols).any(|col| {
                screen
                    .cell(row, col)
                    .is_some_and(|cell| cell.contents() == "F" && cell.bold())
            })
        }) && screen.contents().contains("let answer = 42;")
    });
    // The producer is still blocked waiting for the next command file. Seeing
    // styled cells above is the handshake proof of early Markdown presentation.
    ui.command("append", "\n```\n\nLAST_MARKER");
    ui.wait(|screen| screen.contents().contains("LAST_MARKER"));
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    let history = ui.history().join("\n");
    assert_eq!(history.matches("FIRST_MARKER").count(), 1, "{history}");
    assert_eq!(history.matches("LAST_MARKER").count(), 1, "{history}");
}

#[test]
fn replacing_long_preview_then_shrinking_and_finishing_does_not_publish_drafts() {
    let mut ui = UiPty::start();
    ui.command(
        "replace",
        &(0..65)
            .map(|i| format!("DRAFT_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("DRAFT_64"));
    assert!(
        !ui.history()
            .iter()
            .take(20)
            .any(|line| line.contains("DRAFT_00"))
    );
    ui.resize(12, 40);
    ui.command("replace", "**SHORT_PREVIEW**");
    ui.wait(|screen| {
        screen.contents().contains("SHORT_PREVIEW") && !screen.contents().contains("DRAFT_64")
    });
    ui.command(
        "replace",
        &(0..45)
            .map(|i| format!("FINAL_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("FINAL_44"));
    ui.command("finish", "");
    ui.wait(|screen| {
        let (row, _) = screen.cursor_position();
        screen.contents().contains("FINAL_44")
            && !screen.contents().contains("Fixture waiting")
            && screen
                .rows(0, screen.size().1)
                .nth(row as usize)
                .unwrap_or_default()
                .trim_end()
                .ends_with('>')
    });
    let history = ui.history().join("\n");
    for i in 0..45 {
        assert_eq!(
            history.matches(&format!("FINAL_{i:02}")).count(),
            1,
            "{history}"
        );
    }
    assert!(
        !history.contains("DRAFT_") && !history.contains("SHORT_PREVIEW"),
        "{history}"
    );
}

#[test]
fn failure_after_partial_markdown_keeps_prefix_and_editor_usable() {
    let mut ui = UiPty::start();
    ui.command("append", "**KEPT_PREFIX**");
    ui.wait(|screen| screen.contents().contains("KEPT_PREFIX"));
    ui.write("NEXT_DRAFT");
    ui.command("fail", "");
    ui.wait(|screen| {
        let text = screen.contents();
        text.contains("KEPT_PREFIX")
            && text.contains("failed: injected failure")
            && text.contains("NEXT_DRAFT")
            && !text.contains("Fixture waiting")
    });
}

#[test]
fn clear_then_replace_does_not_resurrect_hidden_prefix() {
    let mut ui = UiPty::start();
    ui.command("append", "HIDDEN_OLD\n\n");
    ui.wait(|screen| screen.contents().contains("HIDDEN_OLD"));
    ui.write("NEXT_DRAFT\x0c");
    ui.wait(|screen| {
        screen.contents().contains("NEXT_DRAFT") && !screen.contents().contains("HIDDEN_OLD")
    });
    ui.command("replace", "INSERTED_BEFORE_HIDDEN_OLD\n\nVISIBLE_NEW");
    ui.wait(|screen| screen.contents().contains("VISIBLE_NEW"));
    assert!(
        !ui.parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("HIDDEN_OLD")
    );
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    let text = ui.parser.lock().unwrap().screen().contents();
    assert!(text.contains("VISIBLE_NEW") && text.contains("NEXT_DRAFT"));
    assert!(!text.contains("HIDDEN_OLD"));
}

#[test]
fn resize_after_publication_does_not_republish_wrapped_markdown_source() {
    let mut ui = UiPty::start();
    let source = (0..24).map(|i| format!("COMMITTED_{i:02} a paragraph with words that wraps when the terminal is narrow.\n\n")).collect::<String>();
    ui.command("append", &source);
    ui.wait(|screen| screen.contents().contains("COMMITTED_23"));
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    ui.resize(30, 100);
    ui.write("RESIZE_ACK");
    ui.wait(|screen| screen.contents().contains("RESIZE_ACK"));
    // Generate more stable output after the reflow so that remaining source
    // groups also cross the publication boundary at the new width.
    ui.write("\x03/help\r");
    ui.wait(|screen| screen.contents().contains("/compact"));
    let history = ui.history().join("\n");
    for i in 0..24 {
        assert_eq!(
            history.matches(&format!("COMMITTED_{i:02}")).count(),
            1,
            "{history}"
        );
    }
}

#[test]
fn browsing_keeps_output_anchor_and_editor_visible_while_backend_appends() {
    let mut ui = UiPty::start();
    ui.command(
        "append",
        &(0..65)
            .map(|i| format!("ROW_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("ROW_64"));
    ui.write("\x1b[5~\x1b[5~DRAFT_ANCHOR");
    ui.wait(|screen| {
        screen.contents().contains("DRAFT_ANCHOR") && !screen.contents().contains("ROW_64")
    });
    let first = ui
        .parser
        .lock()
        .unwrap()
        .screen()
        .rows(0, 60)
        .find(|row| row.contains("ROW_"))
        .unwrap();
    ui.command(
        "append",
        &(65..85)
            .map(|i| format!("ROW_{i:02}\n\n"))
            .collect::<String>(),
    );
    ui.wait(|screen| screen.contents().contains("step 2"));
    let parser = ui.parser.lock().unwrap();
    let screen = parser.screen();
    assert_eq!(
        screen.rows(0, 60).find(|row| row.contains("ROW_")).unwrap(),
        first
    );
    assert!(screen.contents().contains("DRAFT_ANCHOR"));
    drop(parser);
    ui.write("\x1b[F");
    ui.wait(|screen| {
        screen.contents().contains("ROW_84") && screen.contents().contains("DRAFT_ANCHOR")
    });
    ui.command("finish", "");
    ui.wait(|screen| !screen.contents().contains("Fixture waiting"));
    assert!(
        !ui.history()
            .iter()
            .any(|row| row.contains("Fixture waiting"))
    );
}

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
