use std::{env, io};

use super::{
    core::TheseusShell,
    prompt::{default_prompt, expand_home},
    pty::{PersistentShellConfig, PersistentShellSession, PtyCommandConfig, run_pty_command},
    render::render_markdown,
    resume::select_resume_session,
};
use crate::{
    agent::{Agent, AgentConfig, CompactOutcome, default_config_path},
    common::info::render_info,
    common::output::CommandOutput,
    feature_flags,
    input::{MultiLineConfig, colorize_nested, read_multi_line_input},
    logging::AppLogger,
};

impl TheseusShell {
    pub(super) fn agent_status(&self) -> String {
        self.agent
            .as_ref()
            .map(|agent| render_markdown(&agent.status_text()))
            .unwrap_or_else(|| "Agent is not configured.\n".to_string())
    }

    pub(super) fn mcp_status(&self) -> String {
        let Some(agent) = self.agent.as_ref() else {
            return "Agent is not configured.\n".to_string();
        };

        let statuses = agent.mcp_status_text();
        if statuses.is_empty() {
            return "No MCP servers configured.\n".to_string();
        }

        render_markdown(&statuses)
    }

    pub(super) fn change_directory(&mut self, command: &str) -> CommandOutput {
        let mut parts = command.split_whitespace();
        parts.next();

        let Some(path) = parts.next() else {
            return CommandOutput::failure("cd: missing path\n");
        };

        if parts.next().is_some() {
            return CommandOutput::failure("cd: too many arguments\n");
        }

        let path = expand_home(path);

        match env::set_current_dir(&path) {
            Ok(()) => {
                self.config.working_dir = env::current_dir().ok();
                self.config.prompt = default_prompt(self.config.working_dir.as_ref());
                CommandOutput::success("")
            }
            Err(err) => CommandOutput::failure(format!("cd: {err}\n")),
        }
    }

    pub(super) fn run_external_command(&mut self, command: &str) -> io::Result<CommandOutput> {
        if feature_flags::PERSISTENT_SHELL_SESSION {
            if self.shell_session.is_none() {
                self.shell_session = Some(PersistentShellSession::start(PersistentShellConfig {
                    shell: self.config.executable.clone(),
                    env_vars: self.config.env_vars.clone(),
                    working_dir: self.config.working_dir.clone(),
                })?);
            }

            let session = self
                .shell_session
                .as_mut()
                .ok_or_else(|| io::Error::other("persistent shell session was not initialized"))?;
            let output = session.run_command(command)?;
            self.sync_working_dir_from_shell();

            return Ok(output);
        }

        run_pty_command(PtyCommandConfig {
            shell: self.config.executable.clone(),
            command: command.to_string(),
            env_vars: self.config.env_vars.clone(),
            working_dir: self.config.working_dir.clone(),
            cancellation: None,
        })
    }

    fn sync_working_dir_from_shell(&mut self) {
        let Some(session) = self.shell_session.as_mut() else {
            return;
        };
        let Ok(working_dir) = session.current_working_dir() else {
            return;
        };

        if env::set_current_dir(&working_dir).is_ok() {
            self.config.working_dir = Some(working_dir);
            self.config.prompt = default_prompt(self.config.working_dir.as_ref());
        }
    }

    pub(super) fn handle_ask_command(&mut self, command: &str) -> io::Result<CommandOutput> {
        let prompt = command.strip_prefix("/ask").unwrap_or_default().trim();

        if prompt.is_empty() {
            return self.read_ask_input();
        }

        Ok(self.agent_output(prompt))
    }

    pub(super) fn handle_help_command(&self) -> CommandOutput {
        // Render the same boxed info screen that `run_shell_command` prints
        // at shell startup so the two outputs can never diverge.
        CommandOutput::success(render_info())
    }

    pub(super) fn handle_reset_command(&mut self) -> io::Result<CommandOutput> {
        if self.agent.is_none() {
            return Ok(CommandOutput::failure("Agent is not configured.\n"));
        }

        let path = self
            .config
            .agent_config_path
            .clone()
            .map(Ok)
            .unwrap_or_else(default_config_path)?;
        let agent_init = AgentConfig::load_or_create_at(path)?;

        let new_logger = match self.config.logger.as_ref() {
            Some(_) => AppLogger::start_session().ok(),
            None => None,
        };

        let agent = match new_logger.clone() {
            Some(logger) => Agent::new(agent_init.config.clone()).with_logger(logger),
            None => Agent::new(agent_init.config.clone()),
        };

        self.agent = Some(agent);
        self.config.agent_config = Some(agent_init.config);
        self.config.agent_config_path = Some(agent_init.path);
        if let Some(logger) = new_logger {
            self.config.logger = Some(logger);
        }

        Ok(CommandOutput::success("Agent context has been reset.\n"))
    }

    pub(super) fn handle_compact_command(&mut self) -> io::Result<CommandOutput> {
        let Some(agent) = self.agent.as_mut() else {
            return Ok(CommandOutput::failure("Agent is not configured.\n"));
        };

        let outcome = match agent.compact_context() {
            Ok(outcome) => outcome,
            Err(err) => {
                return Ok(CommandOutput::failure(format!("agent: {err}\n")));
            }
        };

        let result = match outcome {
            CompactOutcome::AlreadyMinimal => {
                return Ok(CommandOutput::success(
                    "Agent context is already minimal.\n",
                ));
            }
            CompactOutcome::MissingAuthorization => {
                return Ok(CommandOutput::success(
                    "LLM Authorization header is empty. Run /config first.\n",
                ));
            }
            CompactOutcome::Compacted(result) => result,
        };

        let new_logger = match self.config.logger.as_ref() {
            Some(_) => Some(AppLogger::start_session()?),
            None => None,
        };
        if let Some(logger) = new_logger.as_ref() {
            agent.set_logger(logger.clone());
            agent.log_event(
                "info",
                "agent_compact_finish",
                serde_json::json!({
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
            self.config.logger = Some(logger.clone());
        }

        let new_trajectory = new_logger
            .as_ref()
            .map(|logger| format!(" New trajectory: {}.", logger.trajectory_path().display()))
            .unwrap_or_default();
        Ok(CommandOutput::success(format!(
            "Agent context compacted: {} -> {} messages.{new_trajectory}\n",
            result.before_messages, result.after_messages
        )))
    }

    pub(super) fn handle_resume_command(&mut self) -> io::Result<CommandOutput> {
        let Some(agent) = self.agent.as_mut() else {
            return Ok(CommandOutput::failure("Agent is not configured.\n"));
        };

        let Some(session) = (match select_resume_session(agent.max_resume_traj()) {
            Ok(session) => session,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                return Ok(CommandOutput::success("Resume cancelled.\n"));
            }
            Err(err) => return Err(err),
        }) else {
            return Ok(CommandOutput::success("No resumable sessions found.\n"));
        };

        let message_count = agent.resume_trajectory_from_path(&session.path)?;

        Ok(CommandOutput::success(format!(
            "Resumed session from {} ({message_count} messages).\n",
            session.path.display()
        )))
    }

    pub(super) fn handle_config_command(&mut self) -> io::Result<CommandOutput> {
        let path = self
            .config
            .agent_config_path
            .clone()
            .map(Ok)
            .unwrap_or_else(default_config_path)?;
        let previous_model = self
            .config
            .agent_config
            .as_ref()
            .and_then(config_model_name)
            .map(str::to_string);
        let config =
            match AgentConfig::configure_interactive_at(self.config.agent_config.as_ref(), &path) {
                Ok(config) => config,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    return Ok(CommandOutput::success("Config cancelled.\n"));
                }
                Err(err) => return Err(err),
            };
        let next_model = config_model_name(&config).map(str::to_string);
        let model_changed = previous_model != next_model;
        let logger = if model_changed && self.config.logger.is_some() {
            Some(AppLogger::start_session()?)
        } else {
            self.config.logger.clone()
        };
        let agent = match logger.clone() {
            Some(logger) => Agent::new(config.clone()).with_logger(logger),
            None => Agent::new(config.clone()),
        };
        self.agent = Some(agent);
        self.config.agent_config = Some(config);
        self.config.agent_config_path = Some(path.clone());
        self.config.logger = logger;

        Ok(CommandOutput::success(format!(
            "Config saved to {}\n",
            path.display()
        )))
    }

    fn read_ask_input(&mut self) -> io::Result<CommandOutput> {
        println!(
            "{}",
            colorize_nested(
                "<bright-black>Enter multiline input. Type <bold>/end</bold> on a new line to finish.</bright-black>"
            )
        );

        let text = match read_multi_line_input(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
        }) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                return Ok(CommandOutput::success("\nAsk cancelled.\n"));
            }
            Err(err) => return Err(err),
        };

        Ok(self.agent_output(&text))
    }

    pub(super) fn agent_output(&mut self, prompt: &str) -> CommandOutput {
        let has_agent = self.agent.is_some();
        match self.run_agent(prompt) {
            Ok(output) => {
                let rendered = render_markdown(&output);
                CommandOutput::success(wrap_agent_answer(&rendered, has_agent))
            }
            Err(err) => {
                self.log_event(
                    "error",
                    "agent_failed",
                    serde_json::json!({
                        "prompt": prompt,
                        "error": err.to_string(),
                    }),
                );
                let body = format!("agent: {err}\n");
                CommandOutput::failure(wrap_agent_answer(&body, has_agent))
            }
        }
    }
}

fn config_model_name(config: &AgentConfig) -> Option<&str> {
    config
        .llm_request_settings
        .body
        .get("model")
        .and_then(serde_json::Value::as_str)
}

fn wrap_agent_answer(text: &str, add_padding: bool) -> String {
    if !add_padding {
        return text.to_string();
    }

    // Prepend/append at most one blank line. If the answer already
    // starts or ends with `\n`, the leading/trailing whitespace is
    // left untouched — we never strip it and never duplicate it.
    let leading = !text.starts_with('\n');
    let trailing = !text.ends_with('\n');
    let extra = (leading as usize) + (trailing as usize);
    let mut result = String::with_capacity(text.len() + extra);
    if leading {
        result.push('\n');
    }
    result.push_str(text);
    if trailing {
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::wrap_agent_answer;

    #[test]
    fn wrap_without_padding_returns_input_unchanged() {
        assert_eq!(wrap_agent_answer("hello\n", false), "hello\n");
    }

    #[test]
    fn wrap_with_padding_adds_blank_line_around_text_without_newlines() {
        assert_eq!(wrap_agent_answer("hello", true), "\nhello\n");
    }

    #[test]
    fn wrap_with_padding_adds_only_leading_blank_line_when_answer_ends_with_newline() {
        assert_eq!(wrap_agent_answer("hello\n", true), "\nhello\n");
    }

    #[test]
    fn wrap_with_padding_adds_only_trailing_blank_line_when_answer_starts_with_newline() {
        assert_eq!(wrap_agent_answer("\nhello", true), "\nhello\n");
    }

    #[test]
    fn wrap_with_padding_does_not_modify_already_padded_answer() {
        assert_eq!(wrap_agent_answer("\nhello\n", true), "\nhello\n");
    }

    #[test]
    fn wrap_with_padding_preserves_extra_trailing_blank_lines() {
        assert_eq!(wrap_agent_answer("hello\n\n\n", true), "\nhello\n\n\n");
    }

    #[test]
    fn wrap_with_padding_handles_empty_answer() {
        assert_eq!(wrap_agent_answer("", true), "\n\n");
    }

    #[test]
    fn wrap_with_padding_handles_answer_that_is_only_newlines() {
        assert_eq!(wrap_agent_answer("\n\n\n", true), "\n\n\n");
    }
}
