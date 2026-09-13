//! The UI owns neither Agent locks nor the lifetime of an individual HTTP call.

use super::{Agent, AgentConfig, AgentRunContext, CompactOutcome};
use crate::{
    common::{
        cancellation::CancellationEvent,
        events::{BackendEvent, BlockKind, EventSink, Outcome, OutputEvent},
    },
    logging::AppLogger,
};
use serde_json::json;
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

pub(crate) enum Operation {
    Run {
        prompt: String,
        context: Box<AgentRunContext>,
    },
    Compact,
    Mcp,
    Resume(PathBuf),
    ModelCatalog {
        reply: Sender<super::config::model_catalog::ModelCatalog>,
    },
    Configure {
        config: Box<AgentConfig>,
        logger: AppLogger,
    },
}

enum Command {
    Execute {
        operation: Operation,
        output: EventSink,
        cancellation: CancellationEvent,
        completion: Sender<Completion>,
    },
    Stop,
}

pub(crate) struct Completion {
    pub result: io::Result<String>,
    pub outcome: Outcome,
    pub logger: Option<AppLogger>,
}

pub(crate) struct ActiveOperation {
    pub id: crate::common::events::OperationId,
    pub events: Receiver<BackendEvent>,
    pub completion: Receiver<Completion>,
    pub cancellation: CancellationEvent,
    pub finished: bool,
    pub output_error: Option<&'static str>,
    pub started: std::time::Instant,
    pub input: String,
}

impl ActiveOperation {
    pub(crate) fn output_disconnected(&mut self) -> Option<Outcome> {
        if self.finished {
            return None;
        }
        let error = "agent output channel closed before Finished";
        self.output_error = Some(error);
        self.finished = true;
        self.cancellation.cancel();
        Some(Outcome::Failed(error.into()))
    }

    pub(crate) fn checked_completion(&self, mut completion: Completion) -> Completion {
        if let Some(error) = self.output_error {
            completion.result = Err(io::Error::other(error));
            completion.outcome = Outcome::Failed(error.into());
        }
        completion
    }
}

impl Drop for ActiveOperation {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(crate) struct AgentWorker {
    tx: Sender<Command>,
    handle: Option<JoinHandle<()>>,
    status: Arc<Mutex<String>>,
    active_cancellation: Arc<Mutex<Option<CancellationEvent>>>,
}

impl AgentWorker {
    pub(crate) fn new(config: AgentConfig, logger: AppLogger) -> io::Result<Self> {
        Self::with_executor(config, logger, execute)
    }

    pub(crate) fn with_executor(
        config: AgentConfig,
        logger: AppLogger,
        mut executor: impl FnMut(
            &mut Agent,
            Operation,
            &EventSink,
            &CancellationEvent,
        ) -> io::Result<String>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let agent = Agent::new(config).with_logger(logger);
        let status = Arc::new(Mutex::new(agent.status_text()));
        let worker_status = Arc::clone(&status);
        let active_cancellation = Arc::new(Mutex::new(None));
        let worker_cancel = Arc::clone(&active_cancellation);
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("theseus-agent".into())
            .spawn(move || {
                let mut agent = agent;
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::Stop => break,
                        Command::Execute {
                            operation,
                            output,
                            cancellation,
                            completion,
                        } => {
                            // Configuration has already been persisted by the UI.
                            // Applying it is a commit: acknowledge only after the
                            // worker and its status snapshot agree with that file.
                            let configuration_commit =
                                matches!(operation, Operation::Configure { .. });
                            *worker_cancel.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(cancellation.clone());
                            agent.mcp.set_cancellation(cancellation.clone());
                            agent.output = Some(output.clone());
                            agent.mcp.set_output(Some(output.clone()));
                            let result = crate::common::panic_boundary::catch(|| {
                                let started = output.emit(OutputEvent::Started);
                                if !configuration_commit {
                                    started?;
                                }
                                if cancellation.is_cancelled() && !configuration_commit {
                                    return Err(io::ErrorKind::Interrupted.into());
                                }
                                executor(&mut agent, operation, &output, &cancellation)
                            })
                            .unwrap_or_else(|message| {
                                Err(io::Error::other(format!(
                                    "agent worker panicked: {message}"
                                )))
                            });
                            let outcome = match &result {
                                Ok(_) if configuration_commit => Outcome::Completed,
                                Err(error) if error.kind() != io::ErrorKind::Interrupted => {
                                    Outcome::Failed(error.to_string())
                                }
                                _ if cancellation.is_cancelled() => Outcome::Cancelled,
                                Err(_) => Outcome::Cancelled,
                                Ok(_) => Outcome::Completed,
                            };
                            match &result {
                                Ok(text)
                                    if !text.trim().is_empty() && !cancellation.is_cancelled() =>
                                {
                                    let _ = output.message(BlockKind::Markdown, text);
                                }
                                _ => {}
                            }
                            let _ = output.finish(outcome.clone());
                            agent.output = None;
                            agent.mcp.set_output(None);
                            *worker_status.lock().unwrap_or_else(|e| e.into_inner()) =
                                agent.status_text();
                            *worker_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            let _ = completion.send(Completion {
                                result,
                                outcome,
                                logger: agent.logger.clone(),
                            });
                        }
                    }
                }
            })?;
        Ok(Self {
            tx,
            handle: Some(handle),
            status,
            active_cancellation,
        })
    }

    pub(crate) fn start(&self, operation: Operation, input: String) -> io::Result<ActiveOperation> {
        let cancellation = CancellationEvent::new();
        let (output, events) = EventSink::channel(cancellation.clone());
        let id = output.operation();
        let (tx, completion) = mpsc::channel();
        {
            let mut active = self
                .active_cancellation
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if active.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "agent operation is already running",
                ));
            }
            // Register before enqueueing, so shutdown can cancel a command even
            // if the worker has not picked it up yet.
            *active = Some(cancellation.clone());
        }
        if self
            .tx
            .send(Command::Execute {
                operation,
                output,
                cancellation: cancellation.clone(),
                completion: tx,
            })
            .is_err()
        {
            *self
                .active_cancellation
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "agent worker stopped",
            ));
        }
        Ok(ActiveOperation {
            id,
            events,
            completion,
            cancellation,
            finished: false,
            output_error: None,
            started: std::time::Instant::now(),
            input,
        })
    }

    pub(crate) fn status_text(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for AgentWorker {
    fn drop(&mut self) {
        if let Some(cancellation) = self
            .active_cancellation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            cancellation.cancel();
        }
        let _ = self.tx.send(Command::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) fn execute(
    agent: &mut Agent,
    operation: Operation,
    output: &EventSink,
    cancellation: &CancellationEvent,
) -> io::Result<String> {
    match operation {
        Operation::Configure { config, logger } => {
            let _ = output.activity("Applying configuration", "");
            *agent = Agent::new(*config).with_logger(logger);
            Ok(String::new())
        }
        Operation::ModelCatalog { reply } => {
            output.activity("Loading models", "OpenRouter catalog")?;
            let catalog = super::config::model_catalog::load_openrouter_models(cancellation)?;
            if cancellation.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            reply
                .send(catalog)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "model picker closed"))?;
            Ok(String::new())
        }
        Operation::Run {
            prompt,
            mut context,
        } => {
            context.output = Some(output.clone());
            context.cancellation = cancellation.clone();
            agent.run_with_context(&prompt, *context)
        }
        Operation::Mcp => {
            let text = agent.mcp_status_text();
            Ok(if text.is_empty() {
                "No MCP servers configured.\n".into()
            } else {
                text
            })
        }
        Operation::Resume(path) => {
            let count = agent.resume_trajectory_from_path(&path, cancellation)?;
            Ok(format!(
                "Resumed session from {} ({count} messages).\n",
                path.display()
            ))
        }
        Operation::Compact => match agent.compact_context_cancellable(cancellation)? {
            CompactOutcome::AlreadyMinimal => Ok("Agent context is already minimal.\n".into()),
            CompactOutcome::MissingAuthorization => {
                Ok("LLM Authorization header is empty. Run /config first.\n".into())
            }
            CompactOutcome::Compacted(result) => {
                let logger = AppLogger::start_session()?;
                agent.set_logger(logger.clone());
                agent.log_event(
                    "info",
                    "agent_compact_finish",
                    json!({
                        "previous_log_path": result.previous_log_path,
                        "previous_trajectory_path": result.previous_trajectory_path,
                        "new_log_path": logger.log_path(),
                        "new_trajectory_path": logger.trajectory_path(),
                        "messages_before": result.before_messages,
                        "messages_after": result.after_messages,
                        "compact_trim_retries": result.compact_trim_retries,
                        "recent_user_messages": result.recent_user_messages,
                    }),
                );
                Ok(format!(
                    "Agent context compacted: {} -> {} messages. New trajectory: {}.\n",
                    result.before_messages,
                    result.after_messages,
                    logger.trajectory_path().display()
                ))
            }
        },
    }
}
