//! Submitted commands, slash-command dispatch and command logging.
use super::{
    Application, Interaction,
    config_edit::ConfigPatch,
    editor::{EditorSubmission, SubmissionKind, UnifiedEditor},
    history::{HistoryEntry, HistoryKind, HistoryMode},
    interaction::interaction_needs_input,
};
use crate::{
    commands::{SlashCommand, parse_slash_command},
    common, input,
    shell::command_routing::{CommandRoute, classify_command},
};
use serde_json::json;
use std::io;

impl Application {
    pub(super) fn execute_submission(&mut self, submission: EditorSubmission) -> io::Result<()> {
        match submission.kind {
            SubmissionKind::Command => self.execute_command(&submission.text),
            SubmissionKind::Ask => {
                let text = submission.text.trim();
                if !text.is_empty() {
                    self.store_history(HistoryEntry {
                        text: text.to_string(),
                        kind: HistoryKind::Agent,
                        mode: HistoryMode::MultiLineAsk,
                    });
                    self.run_agent(text)?;
                }
                self.return_to_command_editor();
                Ok(())
            }
            SubmissionKind::Shell => {
                let text = submission.text.trim();
                if !text.is_empty() {
                    self.store_history(HistoryEntry {
                        text: text.to_string(),
                        kind: HistoryKind::Shell,
                        mode: HistoryMode::MultiLineShell,
                    });
                    self.run_shell(text)?;
                }
                self.return_to_command_editor();
                Ok(())
            }
            SubmissionKind::ApiKey => {
                let key = submission.text.trim();
                let authorization = if key.starts_with("Bearer ") {
                    key.to_string()
                } else {
                    format!("Bearer {key}")
                };
                self.save_config_patch(ConfigPatch::SetAuthorization(authorization), false)?;
                self.return_to_command_editor();
                Ok(())
            }
        }
    }

    pub(super) fn execute_command(&mut self, input: &str) -> io::Result<()> {
        let _ = self.logger.event(
            "info",
            "command_start",
            json!({
                "input": input,
                "renderer": "diff",
            }),
        );
        let result = self.execute_command_inner(input);
        if result.is_ok()
            && (interaction_needs_input(&self.interaction)
                || self.active_operation.is_some()
                || self.pending_shell.is_some())
        {
            self.pending_command_log = Some(input.to_string());
        } else {
            self.log_command_finish(input, result.as_ref().err());
        }
        result
    }

    pub(super) fn log_command_finish(&self, input: &str, error: Option<&io::Error>) {
        let _ = self.logger.event(
            if error.is_none() && self.last_command_status == 0 {
                "info"
            } else {
                "error"
            },
            "command_finish",
            json!({
                "input": input,
                "renderer": "diff",
                "status_code": self.last_command_status,
                "error": error.map(ToString::to_string),
            }),
        );
    }

    pub(super) fn finish_pending_command_log(&mut self) {
        if let Some(input) = self.pending_command_log.take() {
            self.log_command_finish(&input, None);
        }
    }

    fn execute_command_inner(&mut self, input: &str) -> io::Result<()> {
        self.last_output_streamed = false;
        self.last_command_status = 0;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            self.return_to_command_editor();
            return Ok(());
        }
        match parse_slash_command(trimmed) {
            Some(SlashCommand::Exit) => self.exit_requested = true,
            Some(SlashCommand::Help) => {
                self.last_shell_command = None;
                self.append_text(&common::info::render_info());
            }
            Some(SlashCommand::Status) => {
                self.last_shell_command = None;
                self.append_markdown(&self.agent.status_text());
            }
            Some(SlashCommand::Mcp) => {
                self.last_shell_command = None;
                self.start_operation(crate::agent::worker::Operation::Mcp, "/mcp".into())?;
            }
            Some(SlashCommand::Reset) => {
                self.last_shell_command = None;
                self.reset_agent()?;
            }
            Some(SlashCommand::Compact) => {
                self.last_shell_command = None;
                self.compact_agent()?;
            }
            Some(SlashCommand::Config) => {
                self.last_shell_command = None;
                self.store_special_history(trimmed);
                self.open_config();
                return Ok(());
            }
            Some(SlashCommand::Resume) => {
                self.last_shell_command = None;
                self.store_special_history(trimmed);
                self.open_resume()?;
                return Ok(());
            }
            Some(SlashCommand::Ask) => {
                let rest = trimmed.strip_prefix("/ask").unwrap_or_default().trim();
                if rest.is_empty() {
                    self.interaction = Interaction::Editor(UnifiedEditor::ask(None, &self.history));
                    self.active_draft = Some((
                        self.history.len(),
                        HistoryKind::Agent,
                        HistoryMode::MultiLineAsk,
                    ));
                    self.append_text(&format!(
                        "Enter multiline input. Type {} on a new line to finish.\n",
                        input::MULTILINE_SUBMIT_COMMAND
                    ));
                    return Ok(());
                }
                self.store_history(HistoryEntry {
                    text: rest.to_string(),
                    kind: HistoryKind::Agent,
                    mode: HistoryMode::SingleLineAsk,
                });
                self.run_agent(rest)?;
            }
            Some(SlashCommand::Shell) => {
                let rest = trimmed.strip_prefix("/shell").unwrap_or_default().trim();
                if rest.is_empty() {
                    self.interaction =
                        Interaction::Editor(UnifiedEditor::shell(None, &self.history));
                    self.active_draft = Some((
                        self.history.len(),
                        HistoryKind::Shell,
                        HistoryMode::MultiLineShell,
                    ));
                    self.append_text(&format!(
                        "Enter multiline shell command. Type {} on a new line to run.\n",
                        input::MULTILINE_SUBMIT_COMMAND
                    ));
                    return Ok(());
                }
                self.store_history(HistoryEntry {
                    text: rest.to_string(),
                    kind: HistoryKind::Shell,
                    mode: HistoryMode::SingleLine,
                });
                self.run_shell(rest)?;
            }
            Some(SlashCommand::History) => {
                self.last_shell_command = None;
                self.append_text(&self.formatted_history());
            }
            None if trimmed == "exit" => self.exit_requested = true,
            None if classify_command(trimmed, self.working_dir.as_deref())
                == CommandRoute::Agent =>
            {
                self.store_history(HistoryEntry {
                    text: trimmed.to_string(),
                    kind: HistoryKind::Agent,
                    mode: HistoryMode::SingleLine,
                });
                self.run_agent(trimmed)?;
            }
            None => {
                self.store_history(HistoryEntry {
                    text: trimmed.to_string(),
                    kind: HistoryKind::Shell,
                    mode: HistoryMode::SingleLine,
                });
                self.run_shell(trimmed)?;
            }
        }
        if !matches!(
            parse_slash_command(trimmed),
            Some(SlashCommand::Ask | SlashCommand::Shell)
        ) {
            self.store_special_history_if_needed(trimmed);
        }
        if !self.exit_requested {
            self.return_to_command_editor();
        }
        Ok(())
    }
}
