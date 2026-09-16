//! Agent operation lifecycle, backend events and completion acknowledgement.
use super::{
    Application, CommandRecord, Interaction,
    ansi::{style_prompt, terminal_styled_text},
    plain,
    shell_commands::shell_prompt,
};
use crate::{
    agent::{AgentConfig, AgentRunContext},
    common,
    logging::AppLogger,
    terminal_renderer::RenderLine,
};
use serde_json::json;
use std::io::{self, IsTerminal};

impl Application {
    pub(super) fn run_agent(&mut self, prompt: &str) -> io::Result<()> {
        common::cancellation::clear_sigint_request();
        let mut tool_prompt = RenderLine::new(shell_prompt(self.working_dir.as_deref()), "");
        style_prompt(&mut tool_prompt);
        let context = AgentRunContext {
            shell: self.shell_path.clone(),
            shell_prompt: terminal_styled_text(&tool_prompt.prefix, &tool_prompt.prefix_styles),
            shell_highlight: self.config.shell_settings.shell_highlight.clone(),
            env_vars: self.shell_env.clone(),
            working_dir: self.working_dir.clone(),
            last_shell_command: self.last_shell_command.take(),
            logger: Some(self.logger.clone()),
            ..AgentRunContext::default()
        };
        self.start_operation(
            crate::agent::worker::Operation::Run {
                prompt: prompt.into(),
                context: Box::new(context),
            },
            prompt.into(),
        )
    }

    pub(super) fn start_operation(
        &mut self,
        operation: crate::agent::worker::Operation,
        input: String,
    ) -> io::Result<()> {
        if self.active_operation.is_some() {
            return Err(io::Error::other("an operation is already running"));
        }
        let active = self.agent.start(operation, input)?;
        self.document.start_operation(active.id);
        self.active_operation = Some(active);
        Ok(())
    }

    pub(super) fn poll_operation(&mut self) -> io::Result<bool> {
        let Some(active) = &mut self.active_operation else {
            return Ok(false);
        };
        if common::cancellation::take_sigint_request() {
            active.cancellation.cancel();
        }
        let mut changed = false;
        let mut output_bytes = 0;
        for _ in 0..64 {
            match active.events.try_recv() {
                Ok(event) => {
                    output_bytes += event.event.payload_bytes();
                    let terminal =
                        matches!(event.event, common::events::OutputEvent::Finished { .. });
                    let operation = event.operation;
                    let sequence = event.sequence;
                    let (kind, block) = event.event.identity();
                    let accepted = if operation != active.id {
                        false
                    } else if let Some(plain) = &mut self.plain {
                        let rejected = plain.rejected_events;
                        plain.apply(event, &mut io::stdout(), &mut io::stderr())?;
                        plain.rejected_events == rejected
                    } else {
                        self.document.apply(event)
                    };
                    if accepted {
                        active.finished |= terminal;
                        changed = true;
                    } else {
                        log_rejected_backend_event(
                            &self.logger,
                            active.id,
                            operation,
                            sequence,
                            kind,
                            block,
                        );
                    }
                    if output_bytes >= 32 * 1024 && !active.cancellation.is_cancelled() {
                        break;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if let Some(outcome) = active.output_disconnected() {
                        log_output_disconnect(&self.logger, active.id);
                        if let Some(plain) = &mut self.plain {
                            plain.finish(&mut io::stdout(), &mut io::stderr())?;
                        } else {
                            self.document.finish_operation(active.id, outcome);
                        }
                        changed = true;
                    }
                    break;
                }
            }
        }
        if !active.finished {
            return Ok(changed);
        }
        let completion = match active.completion.try_recv() {
            Ok(completion) => completion,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(changed),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => crate::agent::worker::Completion {
                result: Err(io::Error::other("agent worker stopped")),
                outcome: common::events::Outcome::Failed("agent worker stopped".into()),
                logger: None,
            },
        };
        let mut completion = active.checked_completion(completion);
        let catalog = self.pending_models.take().and_then(|models| {
            if completion.result.is_err()
                || completion.outcome != common::events::Outcome::Completed
            {
                return None;
            }
            match models.try_recv() {
                Ok(catalog) => Some(catalog),
                Err(_) => {
                    let error = "model catalog task finished without a result";
                    completion.result = Err(io::Error::other(error));
                    completion.outcome = common::events::Outcome::Failed(error.into());
                    None
                }
            }
        });
        let active = self.active_operation.take().unwrap();
        if let Some(logger) = completion.logger {
            self.logger = logger;
        }
        if let Some(confirmation) = self.pending_config_confirmation.take()
            && completion.result.is_ok()
        {
            if self.plain.is_some() {
                plain::write_diagnostic(&mut io::stdout(), &confirmation)?;
            } else {
                self.append_text(&confirmation);
            }
            completion.result = Ok(confirmation);
        }
        let outcome = completion.outcome.clone();
        let interrupted = outcome == common::events::Outcome::Cancelled;
        let (output, status_code) = match completion.result {
            Ok(text) => {
                if interrupted {
                    if self.plain.is_some() {
                        plain::write_diagnostic(&mut io::stderr(), text.trim_end())?;
                    } else {
                        self.append_text(&text);
                    }
                }
                (text, if interrupted { 130 } else { 0 })
            }
            Err(error) => {
                let text = format!("agent: {error}\n");
                if self.plain.is_some() {
                    plain::write_diagnostic(&mut io::stderr(), &text)?;
                } else {
                    self.append_text(&text);
                }
                (text, if interrupted { 130 } else { 1 })
            }
        };
        self.last_command_status = status_code;
        if self.layout_worker.is_some() && outcome != common::events::Outcome::Completed {
            self.layout_feedback = Some((self.document.version(), outcome));
        }
        self.command_records.push(CommandRecord {
            input: active.input.clone(),
            output,
            status_code: Some(status_code),
        });
        if let Some(catalog) = catalog {
            if let Interaction::Editor(editor) = &self.interaction {
                self.saved_model_draft = Some(editor.clone());
            }
            self.show_model_catalog(catalog);
            // /config completes when the picker is submitted or cancelled.
            return Ok(true);
        }
        self.finish_pending_command_log();
        Ok(true)
    }

    pub(super) fn wait_for_operation(&mut self) -> io::Result<()> {
        if self.terminal.is_none() && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
            self.plain.get_or_insert_with(plain::PlainFrontend::default);
        }
        while self.active_operation.is_some() {
            self.poll_operation()?;
            if self.active_operation.is_some() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        self.prepare_document(80)?;
        Ok(())
    }

    pub(super) fn reset_agent(&mut self) -> io::Result<()> {
        let init = AgentConfig::load_or_create_at(self.config_path.clone())?;
        self.config = init.config;
        self.logger = AppLogger::start_session()?;
        self.apply_configuration("Agent context has been reset.\n".into(), "/reset")?;
        Ok(())
    }

    pub(super) fn compact_agent(&mut self) -> io::Result<()> {
        self.start_operation(crate::agent::worker::Operation::Compact, "/compact".into())
    }
}

pub(super) fn log_rejected_backend_event(
    logger: &AppLogger,
    active: common::events::OperationId,
    operation: common::events::OperationId,
    sequence: u64,
    kind: &str,
    block: Option<common::events::BlockId>,
) {
    let _ = logger.event(
        "warn",
        "backend_event_rejected",
        json!({
            "active_operation_id": active.0,
            "operation_id": operation.0,
            "sequence": sequence,
            "kind": kind,
            "block_id": block.map(|id| id.0),
        }),
    );
}

pub(super) fn log_output_disconnect(logger: &AppLogger, operation: common::events::OperationId) {
    let _ = logger.event(
        "error",
        "backend_output_disconnected",
        json!({
            "operation_id": operation.0,
            "reason": "event channel closed before Finished",
        }),
    );
}
