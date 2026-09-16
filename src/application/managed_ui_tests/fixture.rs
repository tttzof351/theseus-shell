//! Scripted backend and fault injection for the managed UI subprocess.

use super::*;

pub(super) fn run_fixture() {
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
                    .map(crate::agent::worker::RunResult::from)
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
    if directory.join("hold-initial-activity").exists() {
        let deadline = Instant::now() + TIMEOUT;
        while !directory.join("release-initial-activity").exists() {
            if cancellation.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
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
