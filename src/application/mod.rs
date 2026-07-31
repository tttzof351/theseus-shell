//! Diff-rendered Theseus application.

use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

use crossterm::{
    cursor::{MoveTo, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, read,
    },
    execute,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size},
};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstRootNode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::config::model_catalog;
use crate::{
    agent::{Agent, AgentConfig, AgentRunContext, CompactOutcome, ShellCommandContext},
    commands::{self, SlashCommand, parse_slash_command},
    common::{self, tmp_files::cleanup_expired_tmp_files_async},
    input,
    logging::{AppLogger, default_logs_dir},
    shell::{
        self,
        command_routing::{CommandRoute, classify_command},
        markdown_preprocessor,
        pty::{PersistentShellConfig, PersistentShellSession},
        terminal as shell_terminal,
    },
};

const SHELL_PROMPT: &str = "user> ";
const MAX_PERSISTED_HISTORY: usize = 100;
const MAX_AGENT_SHELL_CONTEXT_OUTPUT_BYTES: usize = 32 * 1024;

mod config_edit;
use config_edit::*;

pub fn run() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let result = match parse_cli(&args) {
        Ok(Cli::Version) => {
            println!("theseus {}", commands::VERSION);
            Ok(0)
        }
        Ok(Cli::Help) => {
            print_cli_help();
            Ok(0)
        }
        Ok(Cli::Headless(prompt)) => run_headless(&prompt),
        Ok(Cli::Shell(args)) => run_application(args),
        Err(error) => {
            eprintln!("theseus: {error}\n");
            print_cli_help();
            Ok(2)
        }
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("theseus: {error}");
            std::process::exit(2);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cli {
    Shell(Vec<String>),
    Headless(String),
    Version,
    Help,
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut iter = args.iter();
    let mut shell_args = Vec::new();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-v" | "--version" => return Ok(Cli::Version),
            "-h" | "--help" => return Ok(Cli::Help),
            "-p" | "--prompt" => {
                let prompt = iter
                    .next()
                    .ok_or_else(|| format!("`{arg}` requires a prompt argument"))?
                    .clone();
                if iter.next().is_some() {
                    return Err(format!(
                        "`{arg}` does not accept additional arguments after the prompt"
                    ));
                }
                return Ok(Cli::Headless(prompt));
            }
            _ => shell_args.push(arg.clone()),
        }
    }
    Ok(Cli::Shell(shell_args))
}

fn print_cli_help() {
    println!(
        "Theseus shell wrapper (v{})\n\n\
         -p --prompt 'Say Hello'   run the agent non-interactively\n\
         -v --version              print the version and exit\n\
         -h --help                 print this help and exit\n\n\
         ~/.theseus/config.jsonc\n\
         ~/.theseus/logs",
        commands::VERSION
    );
}

fn is_key_action(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableBracketedPaste, Print("\x1b[0m"), Show);
        let _ = disable_raw_mode();
        let _ = write!(stdout, "\r\n");
        let _ = stdout.flush();
    }
}

use crate::terminal_renderer::*;

mod editor;
use editor::*;

#[derive(Debug, Clone)]
struct ResumeSession {
    path: PathBuf,
    date: String,
    question: String,
}

#[derive(Debug, Clone)]
enum Interaction {
    Editor(UnifiedEditor),
    Config(PickerState),
    Models(PickerState),
    Resume(PickerState, Vec<ResumeSession>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandRecord {
    input: String,
    output: String,
    status_code: Option<i32>,
}

struct Application {
    transcript: Vec<RenderLine>,
    screen_cache: VirtualScreen,
    cached_transcript_lines: usize,
    interaction: Interaction,
    history: Vec<HistoryEntry>,
    history_path: Option<PathBuf>,
    command_records: Vec<CommandRecord>,
    config: AgentConfig,
    config_path: PathBuf,
    agent: Agent,
    logger: AppLogger,
    shell_path: PathBuf,
    shell_env: Vec<(String, String)>,
    working_dir: Option<PathBuf>,
    shell_session: Option<PersistentShellSession>,
    last_shell_command: Option<ShellCommandContext>,
    active_draft: Option<(usize, HistoryKind, HistoryMode)>,
    exit_requested: bool,
    physical_invalidated: bool,
    last_output_streamed: bool,
    last_command_status: i32,
    pending_command_log: Option<String>,
}

impl Application {
    fn new() -> io::Result<Self> {
        let init = AgentConfig::load_or_create_default()?;
        cleanup_expired_tmp_files_async(init.config.agent_settings.tmp_files_ttl_min);
        let logger = AppLogger::start_session()?;
        let agent = Agent::new(init.config.clone()).with_logger(logger.clone());
        let working_dir = env::current_dir().ok();
        let history_path = home_dir().map(|home| {
            home.join(".theseus")
                .join("persist")
                .join("history_command_v2.json")
        });
        let history = history_path
            .as_deref()
            .and_then(|path| load_history(path).ok())
            .unwrap_or_default();
        let prompt = shell_prompt(working_dir.as_deref());
        let interaction = Interaction::Editor(UnifiedEditor::command(prompt, &history));
        let shell_path = default_shell_path();
        let mut app = Self {
            transcript: Vec::new(),
            screen_cache: VirtualScreen::new(SHELL_PROMPT),
            cached_transcript_lines: 0,
            interaction,
            history,
            history_path,
            command_records: Vec::new(),
            config: init.config,
            config_path: init.path,
            agent,
            logger,
            shell_path,
            shell_env: vec![("THESEUS_ACTIVE".to_string(), "1".to_string())],
            working_dir,
            shell_session: None,
            last_shell_command: None,
            active_draft: None,
            exit_requested: false,
            physical_invalidated: false,
            last_output_streamed: false,
            last_command_status: 0,
            pending_command_log: None,
        };
        app.append_text(&common::info::render_info());
        Ok(app)
    }

    fn screen(&mut self, terminal_size: TerminalSize) -> &VirtualScreen {
        let mut dirty_from = self.cached_transcript_lines.min(self.transcript.len());
        if self.cached_transcript_lines > self.transcript.len() {
            self.screen_cache.truncate(0);
            self.cached_transcript_lines = 0;
            dirty_from = 0;
        } else {
            self.screen_cache.truncate(self.cached_transcript_lines);
        }
        for line in &self.transcript[self.cached_transcript_lines..] {
            self.screen_cache.push_render_line(line);
        }
        self.cached_transcript_lines = self.transcript.len();
        let base = self.cached_transcript_lines;
        let (mut active_lines, cursor, cursor_visible) = match &mut self.interaction {
            Interaction::Editor(editor) => {
                let (lines, cursor) = editor.render_lines();
                (lines, cursor, !editor.history_is_browsing())
            }
            Interaction::Config(picker)
            | Interaction::Models(picker)
            | Interaction::Resume(picker, _) => picker.render_lines(
                terminal_size.height.saturating_sub(5).min(16),
                terminal_size.width,
            ),
        };
        style_interaction_lines(
            &self.interaction,
            &mut active_lines,
            &self.config.shell_settings.shell_highlight,
        );
        for line in &active_lines {
            self.screen_cache.push_render_line(line);
        }
        if self.screen_cache.lines.is_empty() {
            self.screen_cache.push_render_line(&RenderLine::plain(""));
        }
        self.screen_cache.cursor = VirtualCursor {
            line: base + cursor.line,
            char_offset: cursor.char_offset,
        };
        self.screen_cache.cursor_visible = cursor_visible;
        self.screen_cache.dirty_from = dirty_from;
        &self.screen_cache
    }

    fn handle_event(&mut self, event: Event) -> io::Result<bool> {
        if matches!(event, Event::Resize(_, _)) {
            return Ok(true);
        }
        let Event::Key(key) = &event else {
            let outcome = match &mut self.interaction {
                Interaction::Editor(editor) => Some(editor.handle_event(event)),
                _ => None,
            };
            if let Some(outcome) = outcome {
                return self.finish_editor_outcome(outcome);
            }
            return Ok(false);
        };
        if !is_key_action(key.kind) {
            return Ok(false);
        }

        match &mut self.interaction {
            Interaction::Editor(editor) => {
                let outcome = editor.handle_event(event);
                self.finish_editor_outcome(outcome)
            }
            Interaction::Config(picker) => {
                let outcome = picker.handle_key(*key);
                self.finish_config_picker(outcome)
            }
            Interaction::Models(picker) => {
                let outcome = picker.handle_key(*key);
                self.finish_model_picker(outcome)
            }
            Interaction::Resume(picker, _) => {
                let outcome = picker.handle_key(*key);
                self.finish_resume_picker(outcome)
            }
        }
    }

    fn finish_editor_outcome(&mut self, outcome: EditorOutcome) -> io::Result<bool> {
        match outcome {
            EditorOutcome::Submit(submission) => {
                let submission_kind = submission.kind;
                self.commit_submission(&submission);
                self.execute_submission(submission)?;
                if submission_kind != SubmissionKind::Command {
                    self.finish_pending_command_log();
                }
                Ok(true)
            }
            EditorOutcome::OpenMultiline(mode, text) => {
                let recalled_command = match mode {
                    EditorMode::Ask => "/ask",
                    EditorMode::Shell => "/shell",
                    _ => unreachable!("only multiline modes can be opened from history"),
                };
                self.commit_submission(&EditorSubmission {
                    kind: SubmissionKind::Command,
                    text: recalled_command.to_string(),
                });
                let (kind, history_mode, guidance) = match mode {
                    EditorMode::Ask => (
                        HistoryKind::Agent,
                        HistoryMode::MultiLineAsk,
                        format!(
                            "Enter multiline input. Type {} on a new line to finish.\n",
                            input::MULTILINE_SUBMIT_COMMAND
                        ),
                    ),
                    EditorMode::Shell => (
                        HistoryKind::Shell,
                        HistoryMode::MultiLineShell,
                        format!(
                            "Enter multiline shell command. Type {} on a new line to run.\n",
                            input::MULTILINE_SUBMIT_COMMAND
                        ),
                    ),
                    _ => unreachable!("only multiline modes can be opened from history"),
                };
                self.append_text(&guidance);
                self.interaction = Interaction::Editor(match mode {
                    EditorMode::Ask => UnifiedEditor::ask(Some(text), &self.history),
                    EditorMode::Shell => UnifiedEditor::shell(Some(text), &self.history),
                    _ => unreachable!(),
                });
                self.active_draft = Some((self.history.len(), kind, history_mode));
                Ok(true)
            }
            EditorOutcome::Cancel => {
                let cancelled_submission = match &self.interaction {
                    Interaction::Editor(editor) => Some(EditorSubmission {
                        kind: match editor.mode {
                            EditorMode::Command => SubmissionKind::Command,
                            EditorMode::Ask => SubmissionKind::Ask,
                            EditorMode::Shell => SubmissionKind::Shell,
                            EditorMode::ApiKey => SubmissionKind::ApiKey,
                        },
                        text: editor.buffer.text(),
                    }),
                    _ => None,
                };
                let cancelled_kind = cancelled_submission
                    .as_ref()
                    .map(|submission| submission.kind);
                if let Some(submission) = cancelled_submission {
                    self.commit_submission(&submission);
                }
                match cancelled_kind {
                    Some(SubmissionKind::Command) => {
                        self.append_text("Interrupted. Type /exit to exit the shell.\n");
                    }
                    Some(SubmissionKind::Ask) => self.append_text("\nAsk cancelled.\n"),
                    Some(SubmissionKind::Shell) => self.append_text("\nShell cancelled.\n"),
                    Some(SubmissionKind::ApiKey) => self.append_text("Config cancelled.\n"),
                    None => self.append_text("Cancelled.\n"),
                }
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            EditorOutcome::Exit => {
                self.exit_requested = true;
                Ok(true)
            }
            EditorOutcome::Redraw => {
                self.physical_invalidated = true;
                Ok(true)
            }
            EditorOutcome::Changed => {
                self.sync_multiline_draft();
                Ok(true)
            }
            EditorOutcome::Unchanged => Ok(false),
        }
    }

    fn commit_submission(&mut self, submission: &EditorSubmission) {
        let (prompt, continuation) = match submission.kind {
            SubmissionKind::Command => (
                shell_prompt(self.working_dir.as_deref()),
                input::DEFAULT_COMMAND_CONTINUATION_PROMPT.to_string(),
            ),
            SubmissionKind::Ask | SubmissionKind::Shell => (
                input::DEFAULT_MULTILINE_PREFIX.to_string(),
                input::DEFAULT_MULTILINE_PREFIX.to_string(),
            ),
            SubmissionKind::ApiKey => ("Openrouter API key: ".to_string(), String::new()),
        };
        let mut committed = Vec::new();
        for (index, line) in submission.text.split('\n').enumerate() {
            let shown = if submission.kind == SubmissionKind::ApiKey {
                "*".repeat(char_len(line))
            } else {
                line.to_string()
            };
            let render_line = RenderLine::new(
                if index == 0 {
                    prompt.clone()
                } else {
                    continuation.clone()
                },
                shown,
            );
            committed.push(render_line);
        }
        if matches!(
            submission.kind,
            SubmissionKind::Command | SubmissionKind::Shell
        ) {
            style_shell_lines(
                &mut committed,
                &submission.text,
                &self.config.shell_settings.shell_highlight,
            );
        }
        for line in &mut committed {
            style_prompt(line);
        }
        self.transcript.extend(committed);
    }

    fn execute_submission(&mut self, submission: EditorSubmission) -> io::Result<()> {
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
                self.append_text(&format!("Config saved to {}\n", self.config_path.display()));
                self.return_to_command_editor();
                Ok(())
            }
        }
    }

    fn execute_command(&mut self, input: &str) -> io::Result<()> {
        let _ = self.logger.event(
            "info",
            "command_start",
            json!({
                "input": input,
                "renderer": "diff",
            }),
        );
        let result = self.execute_command_inner(input);
        if result.is_ok() && interaction_needs_input(&self.interaction) {
            self.pending_command_log = Some(input.to_string());
        } else {
            self.log_command_finish(input, result.as_ref().err());
        }
        result
    }

    fn log_command_finish(&self, input: &str, error: Option<&io::Error>) {
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

    fn finish_pending_command_log(&mut self) {
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
                let status = self.agent.mcp_status_text();
                if status.is_empty() {
                    self.append_text("No MCP servers configured.\n");
                } else {
                    self.append_markdown(&status);
                }
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
                shell_terminal::discard_pending_terminal_input()?;
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

    fn run_shell(&mut self, command: &str) -> io::Result<()> {
        if self.shell_session.is_none() {
            self.shell_session = Some(PersistentShellSession::start(PersistentShellConfig {
                shell: self.shell_path.clone(),
                env_vars: self.shell_env.clone(),
                working_dir: self.working_dir.clone(),
            })?);
        }
        let _external = ExternalTerminalGuard::enter()?;
        self.physical_invalidated = true;
        let session = self.shell_session.as_mut().expect("shell initialized");
        let output = session.run_command(command)?;
        self.last_output_streamed = output.streamed;
        self.last_command_status = output.status_code.unwrap_or(1);
        if let Ok(working_dir) = session.current_working_dir()
            && env::set_current_dir(&working_dir).is_ok()
        {
            self.working_dir = Some(working_dir);
        }
        let text = output.transcript_lossy();
        if !output.streamed || !uses_alternate_screen(&text) {
            if let Some(visible_suffix) = suffix_after_last_display_clear(&text) {
                // The streamed terminal has already discarded everything that
                // preceded ED 2. Mirror that state transition in the
                // persistent virtual scene so the recovery frame cannot bring
                // the old transcript back.
                self.transcript.clear();
                self.append_text(visible_suffix);
            } else {
                self.append_text(&text);
            }
        }
        self.last_shell_command = Some(ShellCommandContext {
            command: command.to_string(),
            output: common::text::truncate_utf8_to_bytes(
                &text,
                MAX_AGENT_SHELL_CONTEXT_OUTPUT_BYTES,
                common::text::TruncatePosition::End,
            ),
        });
        self.command_records.push(CommandRecord {
            input: command.to_string(),
            output: text,
            status_code: output.status_code,
        });
        Ok(())
    }

    fn run_agent(&mut self, prompt: &str) -> io::Result<()> {
        let _external = ExternalTerminalGuard::enter()?;
        self.physical_invalidated = true;
        let last_shell_command = self.last_shell_command.take();
        common::cancellation::clear_sigint_request();
        let output = self.agent.run_with_context(
            prompt,
            AgentRunContext {
                shell: self.shell_path.clone(),
                shell_prompt: shell_prompt(self.working_dir.as_deref()),
                shell_highlight: self.config.shell_settings.shell_highlight.clone(),
                env_vars: self.shell_env.clone(),
                working_dir: self.working_dir.clone(),
                last_shell_command,
                logger: Some(self.logger.clone()),
                ..AgentRunContext::default()
            },
        );
        match output {
            Ok(output) => {
                self.last_command_status = 0;
                let rendered = render_markdown(&output);
                self.append_text(&pad_agent_answer(&rendered));
                self.command_records.push(CommandRecord {
                    input: prompt.to_string(),
                    output,
                    status_code: Some(0),
                });
            }
            Err(error) => {
                self.last_command_status = 1;
                self.append_text(&pad_agent_answer(&format!("agent: {error}\n")));
                self.command_records.push(CommandRecord {
                    input: prompt.to_string(),
                    output: error.to_string(),
                    status_code: Some(1),
                });
            }
        }
        Ok(())
    }

    fn reset_agent(&mut self) -> io::Result<()> {
        let init = AgentConfig::load_or_create_at(self.config_path.clone())?;
        self.config = init.config;
        self.logger = AppLogger::start_session()?;
        self.agent = Agent::new(self.config.clone()).with_logger(self.logger.clone());
        self.append_text("Agent context has been reset.\n");
        Ok(())
    }

    fn compact_agent(&mut self) -> io::Result<()> {
        let _external = ExternalTerminalGuard::enter()?;
        self.physical_invalidated = true;
        let message = match self.agent.compact_context() {
            Ok(CompactOutcome::AlreadyMinimal) => "Agent context is already minimal.\n".to_string(),
            Ok(CompactOutcome::MissingAuthorization) => {
                "LLM Authorization header is empty. Run /config first.\n".to_string()
            }
            Ok(CompactOutcome::Compacted(result)) => {
                self.logger = AppLogger::start_session()?;
                self.agent.set_logger(self.logger.clone());
                self.agent.log_event(
                    "info",
                    "agent_compact_finish",
                    json!({
                        "previous_log_path": result.previous_log_path,
                        "previous_trajectory_path": result.previous_trajectory_path,
                        "new_log_path": self.logger.log_path(),
                        "new_trajectory_path": self.logger.trajectory_path(),
                        "messages_before": result.before_messages,
                        "messages_after": result.after_messages,
                        "compact_trim_retries": result.compact_trim_retries,
                        "recent_user_messages": result.recent_user_messages,
                    }),
                );
                format!(
                    "Agent context compacted: {} -> {} messages. New trajectory: {}.\n",
                    result.before_messages,
                    result.after_messages,
                    self.logger.trajectory_path().display()
                )
            }
            Err(error) => {
                self.last_command_status = 1;
                format!("agent: {error}\n")
            }
        };
        self.append_text(&message);
        Ok(())
    }

    fn open_config(&mut self) {
        if !self
            .config
            .llm_request_settings
            .base_url
            .contains("openrouter.ai")
        {
            self.append_text(
                "Warning: /config updates OpenRouter-like fields, but base_url is not an OpenRouter endpoint.\n",
            );
        }
        let items = vec![
            PickerItem {
                id: "model".to_string(),
                label: "1. Change model".to_string(),
                detail: "Select a different model".to_string(),
            },
            PickerItem {
                id: "api_key".to_string(),
                label: "2. Set OpenRouter API key".to_string(),
                detail: "Update the API key".to_string(),
            },
        ];
        self.interaction = Interaction::Config(PickerState::new(
            "What would you like to configure?",
            items,
            false,
        ));
    }

    fn finish_config_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(id) if id == "model" => {
                let catalog = model_catalog::load_openrouter_models();
                let title = format!(
                    "Select model {}",
                    model_catalog_source_label(&catalog.source)
                );
                let current = self
                    .config
                    .llm_request_settings
                    .body
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let items = catalog
                    .models
                    .into_iter()
                    .map(|model| {
                        let is_current = current.as_deref() == Some(model.id.as_str());
                        let context = model
                            .context_length
                            .map(format_context_length)
                            .unwrap_or_else(|| "n/a".to_string());
                        PickerItem {
                            label: format!(
                                "{}{}",
                                model.id,
                                if is_current { " (current)" } else { "" }
                            ),
                            id: model.id,
                            detail: model.name.map_or_else(
                                || format!("ctx: {context}"),
                                |name| format!("ctx: {context}  {name}"),
                            ),
                        }
                    })
                    .collect();
                self.interaction = Interaction::Models(
                    PickerState::new(title, items, true).with_selected_id(current.as_deref()),
                );
                Ok(true)
            }
            PickerOutcome::Submit(id) if id == "api_key" => {
                self.interaction = Interaction::Editor(UnifiedEditor::api_key());
                Ok(true)
            }
            PickerOutcome::Cancel => {
                self.append_text("Config cancelled.\n");
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Changed => Ok(true),
            _ => Ok(false),
        }
    }

    fn finish_model_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(model) => {
                let model_changed = self
                    .config
                    .llm_request_settings
                    .body
                    .get("model")
                    .and_then(Value::as_str)
                    != Some(model.as_str());
                self.save_config_patch(ConfigPatch::SetModel(model), model_changed)?;
                self.append_text(&format!("Config saved to {}\n", self.config_path.display()));
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Cancel => {
                self.append_text("Config cancelled.\n");
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Changed => Ok(true),
            _ => Ok(false),
        }
    }

    fn save_config_patch(&mut self, patch: ConfigPatch, model_changed: bool) -> io::Result<()> {
        if self.config_path.exists() {
            self.config = patch_config_jsonc_file(&self.config_path, patch)?;
        } else {
            match patch {
                ConfigPatch::SetModel(model) => {
                    self.config
                        .llm_request_settings
                        .body
                        .insert("model".to_string(), json!(model));
                }
                ConfigPatch::SetAuthorization(authorization) => {
                    self.config
                        .llm_request_settings
                        .header
                        .insert("Authorization".to_string(), authorization);
                }
            }
            self.config.save_at(&self.config_path)?;
        }
        if model_changed {
            self.logger = AppLogger::start_session()?;
        }
        self.agent = Agent::new(self.config.clone()).with_logger(self.logger.clone());
        Ok(())
    }

    fn open_resume(&mut self) -> io::Result<()> {
        let sessions = resume_sessions(self.agent.max_resume_traj())?;
        if sessions.is_empty() {
            self.append_text("No resumable sessions found.\n");
            self.return_to_command_editor();
            return Ok(());
        }
        let items = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| PickerItem {
                id: index.to_string(),
                label: session.date.clone(),
                detail: truncate_for_width(
                    &session
                        .question
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    96,
                ),
            })
            .collect();
        self.interaction =
            Interaction::Resume(PickerState::new("Resume session", items, true), sessions);
        Ok(())
    }

    fn finish_resume_picker(&mut self, outcome: PickerOutcome) -> io::Result<bool> {
        match outcome {
            PickerOutcome::Submit(id) => {
                let index = id.parse::<usize>().ok();
                let session = match (&self.interaction, index) {
                    (Interaction::Resume(_, sessions), Some(index)) => sessions.get(index).cloned(),
                    _ => None,
                };
                if let Some(session) = session {
                    let count = self.agent.resume_trajectory_from_path(&session.path)?;
                    self.append_text(&format!(
                        "Resumed session from {} ({count} messages).\n",
                        session.path.display()
                    ));
                }
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Cancel => {
                self.append_text("Resume cancelled.\n");
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            PickerOutcome::Changed => Ok(true),
            _ => Ok(false),
        }
    }

    fn return_to_command_editor(&mut self) {
        self.active_draft = None;
        let prompt = shell_prompt(self.working_dir.as_deref());
        self.interaction = Interaction::Editor(UnifiedEditor::command(prompt, &self.history));
    }

    fn sync_multiline_draft(&mut self) {
        let Some((mut slot, kind, mode)) = self.active_draft else {
            return;
        };
        let text = match &self.interaction {
            Interaction::Editor(editor)
                if matches!(editor.mode, EditorMode::Ask | EditorMode::Shell) =>
            {
                editor.buffer.text().trim().to_string()
            }
            _ => return,
        };

        self.history.truncate(slot.min(self.history.len()));
        if text.is_empty() {
            self.active_draft = Some((self.history.len(), kind, mode));
        } else {
            let entry = HistoryEntry { text, kind, mode };
            self.history.retain(|existing| existing != &entry);
            self.history.push(entry);
            if self.history.len() > MAX_PERSISTED_HISTORY {
                let removed = self.history.len() - MAX_PERSISTED_HISTORY;
                self.history.drain(..removed);
            }
            slot = self.history.len().saturating_sub(1);
            self.active_draft = Some((slot, kind, mode));
        }
        if let Some(path) = &self.history_path {
            let _ = save_history(path, &self.history);
        }
    }

    fn append_text(&mut self, text: &str) {
        self.transcript.extend(ansi_render_lines(text));
    }

    fn append_markdown(&mut self, text: &str) {
        self.transcript
            .extend(ansi_render_lines(&render_markdown(text)));
    }

    fn store_special_history_if_needed(&mut self, input: &str) {
        if parse_slash_command(input).is_some() {
            self.store_special_history(input);
        }
    }

    fn store_special_history(&mut self, input: &str) {
        self.store_history(HistoryEntry {
            text: input.to_string(),
            kind: HistoryKind::Special,
            mode: HistoryMode::SingleLine,
        });
    }

    fn store_history(&mut self, mut entry: HistoryEntry) {
        entry.text = entry.text.trim().to_string();
        if entry.text.is_empty() {
            return;
        }
        self.history.retain(|existing| existing != &entry);
        self.history.push(entry);
        if self.history.len() > MAX_PERSISTED_HISTORY {
            self.history
                .drain(..self.history.len() - MAX_PERSISTED_HISTORY);
        }
        if let Some(path) = &self.history_path {
            let _ = save_history(path, &self.history);
        }
    }

    fn formatted_history(&self) -> String {
        if self.command_records.is_empty() {
            return String::new();
        }
        self.command_records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let status = record
                    .status_code
                    .map_or_else(|| "signal".to_string(), |code| code.to_string());
                format!(
                    "{}  {}  [status: {}]\noutput:\n{}",
                    index + 1,
                    record.input,
                    status,
                    record.output
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn take_physical_invalidation(&mut self) -> bool {
        std::mem::take(&mut self.physical_invalidated)
    }
}

struct ExternalTerminalGuard {
    was_raw: bool,
}

impl ExternalTerminalGuard {
    fn enter() -> io::Result<Self> {
        let was_raw = crossterm::terminal::is_raw_mode_enabled()?;
        if !was_raw {
            return Ok(Self { was_raw });
        }
        let mut stdout = io::stdout();
        execute!(stdout, DisableBracketedPaste, Show)?;
        write!(stdout, "\r\n")?;
        stdout.flush()?;
        disable_raw_mode()?;
        Ok(Self { was_raw })
    }
}

impl Drop for ExternalTerminalGuard {
    fn drop(&mut self) {
        if !self.was_raw {
            return;
        }
        let _ = enable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, EnableBracketedPaste);
    }
}

fn run_application(args: Vec<String>) -> io::Result<i32> {
    common::cancellation::install_sigint_handler();
    let mut app = Application::new()?;
    if !args.is_empty() {
        let command = args.join(" ");
        print!("{}", common::info::render_info());
        io::stdout().flush()?;
        app.transcript.clear();
        app.execute_command(&command)?;
        if interaction_needs_input(&app.interaction) {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                let mut transcript = ansi_render_lines(&common::info::render_info());
                transcript.append(&mut app.transcript);
                app.transcript = transcript;
                return run_interactive_application(app, true);
            }
            print_plain_transcript(&app.transcript, true);
            io::stdout().flush()?;
            app.transcript.clear();
            let stdin = io::stdin();
            let mut input = stdin.lock();
            finish_plain_interactions(&mut app, &mut input)?;
        }
        if !app.last_output_streamed {
            print_plain_transcript(&app.transcript, true);
        }
        return Ok(app.last_command_status);
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return run_plain_application(app);
    }

    run_interactive_application(app, false)
}

fn run_interactive_application(
    mut app: Application,
    exit_when_command_editor_returns: bool,
) -> io::Result<i32> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnableBracketedPaste,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    let mut renderer = DiffRenderer::new();
    let mut handled_event = false;

    loop {
        if app.exit_requested {
            return Ok(app.last_command_status);
        }
        let (width, height) = size()?;
        let terminal_size = TerminalSize::new(width, height);
        renderer.render(&mut stdout, app.screen(terminal_size), terminal_size)?;
        if exit_when_command_editor_returns
            && handled_event
            && !interaction_needs_input(&app.interaction)
        {
            return Ok(app.last_command_status);
        }
        let event = read()?;
        handled_event = true;
        if app.handle_event(event)? && app.take_physical_invalidation() {
            renderer.invalidate();
        }
    }
}

fn run_plain_application(mut app: Application) -> io::Result<i32> {
    print_plain_transcript(&app.transcript, false);
    io::stdout().flush()?;
    app.transcript.clear();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        if interaction_needs_input(&app.interaction) {
            finish_plain_interactions(&mut app, &mut input)?;
            flush_plain_application_transcript(&mut app)?;
            continue;
        }
        let Some(line) = read_plain_line(&mut input)? else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        app.execute_command(&line)?;
        flush_plain_application_transcript(&mut app)?;
        if app.exit_requested {
            break;
        }
    }
    Ok(app.last_command_status)
}

fn flush_plain_application_transcript(app: &mut Application) -> io::Result<()> {
    print_plain_transcript(&app.transcript, false);
    io::stdout().flush()?;
    app.transcript.clear();
    Ok(())
}

fn interaction_needs_input(interaction: &Interaction) -> bool {
    !matches!(
        interaction,
        Interaction::Editor(UnifiedEditor {
            mode: EditorMode::Command,
            ..
        })
    )
}

fn finish_plain_interactions(app: &mut Application, input: &mut impl BufRead) -> io::Result<()> {
    while interaction_needs_input(&app.interaction) {
        match &app.interaction {
            Interaction::Config(picker) => {
                println!("{}", picker.title);
                for item in &picker.items {
                    println!("{} — {}", item.label, item.detail);
                }
                let selected = read_plain_line(input)?
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .and_then(|index| picker.items.get(index.saturating_sub(1)))
                    .or_else(|| picker.items.first())
                    .map(|item| item.id.clone());
                app.finish_config_picker(
                    selected.map_or(PickerOutcome::Cancel, PickerOutcome::Submit),
                )?;
            }
            Interaction::Models(picker) => {
                let query = read_plain_line(input)?.unwrap_or_default();
                let selected = picker
                    .items
                    .iter()
                    .find(|item| item.id == query.trim())
                    .or_else(|| {
                        let terms = query
                            .split_whitespace()
                            .map(str::to_ascii_lowercase)
                            .collect::<Vec<_>>();
                        picker.items.iter().find(|item| {
                            let haystack =
                                format!("{} {}", item.id, item.label).to_ascii_lowercase();
                            terms.iter().all(|term| haystack.contains(term))
                        })
                    })
                    .or_else(|| picker.selected_item())
                    .map(|item| item.id.clone());
                app.finish_model_picker(
                    selected.map_or(PickerOutcome::Cancel, PickerOutcome::Submit),
                )?;
            }
            Interaction::Resume(picker, _) => {
                println!("{}", picker.title);
                for (index, item) in picker.items.iter().enumerate() {
                    println!("{}. {} — {}", index + 1, item.label, item.detail);
                }
                let selected = read_plain_line(input)?
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .and_then(|index| picker.items.get(index.saturating_sub(1)))
                    .or_else(|| picker.items.first())
                    .map(|item| item.id.clone());
                app.finish_resume_picker(
                    selected.map_or(PickerOutcome::Cancel, PickerOutcome::Submit),
                )?;
            }
            Interaction::Editor(editor) => match editor.mode {
                EditorMode::ApiKey => {
                    if let Some(text) = read_plain_line(input)? {
                        app.execute_submission(EditorSubmission {
                            kind: SubmissionKind::ApiKey,
                            text,
                        })?;
                    } else {
                        app.append_text("Config cancelled.\n");
                        app.return_to_command_editor();
                        app.finish_pending_command_log();
                    }
                }
                EditorMode::Ask | EditorMode::Shell => {
                    let mut lines = Vec::new();
                    while let Some(line) = read_plain_line(input)? {
                        if line.trim() == input::MULTILINE_SUBMIT_COMMAND {
                            break;
                        }
                        lines.push(line);
                    }
                    app.execute_submission(EditorSubmission {
                        kind: if editor.mode == EditorMode::Ask {
                            SubmissionKind::Ask
                        } else {
                            SubmissionKind::Shell
                        },
                        text: lines.join("\n"),
                    })?;
                }
                EditorMode::Command => break,
            },
        }
    }
    Ok(())
}

fn read_plain_line(input: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

fn print_plain_transcript(lines: &[RenderLine], only_unprefixed: bool) {
    for line in lines {
        if !only_unprefixed || line.prefix.is_empty() {
            println!(
                "{}{}",
                if only_unprefixed {
                    String::new()
                } else {
                    terminal_styled_text(&line.prefix, &line.prefix_styles)
                },
                terminal_styled_text(&line.text, &line.styles)
            );
        }
    }
}

fn terminal_styled_text(text: &str, styles: &[CellStyle]) -> String {
    let mut output = String::new();
    let mut current = CellStyle::default();
    for (index, ch) in text.chars().enumerate() {
        let style = styles.get(index).copied().unwrap_or_default();
        if style != current {
            push_style_escape(&mut output, style);
            current = style;
        }
        output.push(ch);
    }
    if current != CellStyle::default() {
        push_style_escape(&mut output, CellStyle::default());
    }
    output
}

fn run_headless(prompt: &str) -> io::Result<i32> {
    let init = AgentConfig::load_or_create_default()?;
    cleanup_expired_tmp_files_async(init.config.agent_settings.tmp_files_ttl_min);
    let logger = AppLogger::start_session()?;
    let mut agent = Agent::new(init.config).with_logger(logger.clone());
    match agent.run_with_context(
        prompt,
        AgentRunContext {
            logger: Some(logger),
            ..AgentRunContext::default()
        },
    ) {
        Ok(output) => {
            print!("{output}");
            io::stdout().flush()?;
            Ok(0)
        }
        Err(error) => {
            eprintln!("theseus: agent run failed: {error}");
            Ok(1)
        }
    }
}

fn default_shell_path() -> PathBuf {
    #[cfg(unix)]
    {
        env::var_os("SHELL")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    }
    #[cfg(windows)]
    {
        env::var_os("COMSPEC")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cmd.exe"))
    }
}

fn shell_prompt(working_dir: Option<&Path>) -> String {
    let user = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "user".to_string());
    let directory = working_dir
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("theseus");
    format!("{user} {directory}> ")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn load_history(path: &Path) -> io::Result<Vec<HistoryEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let history: Vec<HistoryEntry> =
        serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    Ok(normalize_history(history))
}

fn normalize_history(history: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
    let mut history = history
        .into_iter()
        .filter_map(|mut entry| {
            entry.text = entry.text.trim().to_string();
            (!entry.text.is_empty()).then_some(entry)
        })
        .collect::<Vec<_>>();
    let mut seen = HashSet::with_capacity(history.len());
    history.reverse();
    history.retain(|entry| seen.insert(entry.clone()));
    history.reverse();
    if history.len() > MAX_PERSISTED_HISTORY {
        history.drain(..history.len() - MAX_PERSISTED_HISTORY);
    }
    history
}

fn save_history(path: &Path, history: &[HistoryEntry]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let normalized = normalize_history(history.to_vec());
    let mut text = serde_json::to_string_pretty(&normalized).map_err(io::Error::other)?;
    text.push('\n');
    fs::write(path, text)
}

fn resume_sessions(limit: usize) -> io::Result<Vec<ResumeSession>> {
    let directory = default_logs_dir()?;
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(directory)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_trajectory.json"))
        })
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    Ok(paths
        .into_iter()
        .take(limit)
        .filter_map(|path| resume_session_from_path(&path).ok())
        .collect())
}

fn resume_session_from_path(path: &Path) -> io::Result<ResumeSession> {
    #[derive(Deserialize)]
    struct Snapshot {
        messages: Vec<ResumeMessage>,
    }
    #[derive(Deserialize)]
    struct ResumeMessage {
        role: Option<String>,
        content: Option<Value>,
    }
    let snapshot: Snapshot =
        serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    let question = snapshot
        .messages
        .iter()
        .filter(|message| message.role.as_deref() == Some("user"))
        .filter_map(|message| message.content.as_ref())
        .filter_map(content_value_to_string)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && !text.starts_with("Last shell command:"))
        .next_back()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no user question"))?;
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-date");
    let timestamp = file.strip_suffix("_trajectory.json").unwrap_or(file);
    let parts = timestamp.split('-').collect::<Vec<_>>();
    let date = if parts.len() == 6 {
        format!(
            "{}-{}-{} {}:{}:{}",
            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]
        )
    } else {
        timestamp.to_string()
    };
    Ok(ResumeSession {
        path: path.to_path_buf(),
        date,
        question,
    })
}

fn content_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(content_value_to_string)
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(object) => object
            .get("text")
            .and_then(content_value_to_string)
            .or_else(|| object.get("content").and_then(content_value_to_string)),
        other => Some(other.to_string()),
    }
}

fn pad_agent_answer(text: &str) -> String {
    let mut output = String::with_capacity(text.len() + 2);
    if !text.starts_with('\n') {
        output.push('\n');
    }
    output.push_str(text);
    if !text.ends_with('\n') {
        output.push('\n');
    }
    output
}

mod ansi;
use ansi::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_size(width: u16, height: u16) -> TerminalSize {
        TerminalSize::new(width, height)
    }

    fn visible_row_text(row: &PhysicalRow) -> String {
        row_text(row, 0, row.cells.len())
            .trim_end_matches(' ')
            .to_string()
    }

    fn temporary_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "theseus-application-{name}-{}-{}.jsonc",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn cli_matches_main_prompt_and_shell_argument_rules() {
        assert_eq!(
            parse_cli(&["--prompt".to_string(), "hello".to_string()]),
            Ok(Cli::Headless("hello".to_string()))
        );
        assert!(parse_cli(&["--prompt".to_string()]).is_err());
        assert!(
            parse_cli(&[
                "--prompt".to_string(),
                "hello".to_string(),
                "extra".to_string()
            ])
            .is_err()
        );
        assert_eq!(
            parse_cli(&["printf".to_string(), "ok".to_string()]),
            Ok(Cli::Shell(vec!["printf".to_string(), "ok".to_string()]))
        );
    }

    #[test]
    fn initial_cursor_is_after_prompt() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(SHELL_PROMPT), test_size(20, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 6 }
        );
    }

    #[test]
    fn long_virtual_line_wraps_without_repeating_prompt() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "abcdefghij", 10);
        let terminal = layout_virtual_screen(&screen, test_size(10, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(visible_row_text(&terminal.rows[1]), "efghij");
        assert_eq!(
            terminal
                .rows
                .iter()
                .map(visible_row_text)
                .filter(|row| row.contains(SHELL_PROMPT))
                .count(),
            1
        );
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 6 }
        );
    }

    #[test]
    fn logical_tab_is_preserved_and_maps_to_physical_spaces() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "a\tb", 3);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.lines[0], "a\tb");
        assert_eq!(visible_row_text(&terminal.rows[0]), "user> a b");
        assert_eq!(terminal.cursor.position.column, 9);
    }

    #[test]
    fn config_patch_preserves_jsonc_comments_and_validates_result() {
        let path = temporary_test_path("config-patch");
        let init = AgentConfig::load_or_create_at(path.clone()).unwrap();
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, original.replacen("{\n", "{\n  // keep me\n", 1)).unwrap();

        let config = patch_config_jsonc_file(
            &path,
            ConfigPatch::SetModel("example/new-model".to_string()),
        )
        .unwrap();
        let patched = fs::read_to_string(&path).unwrap();

        assert_eq!(init.path, path);
        assert!(patched.contains("// keep me"));
        assert!(patched.contains(r#""model": "example/new-model""#));
        assert_eq!(
            config
                .llm_request_settings
                .body
                .get("model")
                .and_then(Value::as_str),
            Some("example/new-model")
        );
        let validation_prefix = format!(
            ".{}.application-",
            path.file_name().unwrap().to_string_lossy()
        );
        assert!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with(&validation_prefix) || !name.ends_with(".tmp")
                })
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enter_creates_new_prompt_and_moves_virtual_cursor() {
        let screen = VirtualScreen::from_render_lines(
            vec![
                RenderLine::new(SHELL_PROMPT, "hello"),
                RenderLine::new(SHELL_PROMPT, ""),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 0,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.lines, ["hello", ""]);
        assert_eq!(
            screen.cursor,
            VirtualCursor {
                line: 1,
                char_offset: 0
            }
        );
        assert_eq!(visible_row_text(&terminal.rows[0]), "user> hello");
        assert_eq!(visible_row_text(&terminal.rows[1]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 6 }
        );
    }

    #[test]
    fn cursor_navigation_never_enters_prompt() {
        let screen = VirtualScreen::new(SHELL_PROMPT);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.cursor.char_offset, 0);
        assert_eq!(terminal.cursor.position.column, SHELL_PROMPT.len());
    }

    #[test]
    fn paste_is_split_into_virtual_lines() {
        let mut buffer = EditorBuffer::new();
        buffer.insert_text("one\r\ntwo\nthree");

        assert_eq!(buffer.lines, ["one", "two", "three"]);
        assert_eq!(
            buffer.cursor,
            VirtualCursor {
                line: 2,
                char_offset: 5
            }
        );
    }

    #[test]
    fn resize_reflows_virtual_line() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "abcdefghij", 10);
        let wide = layout_virtual_screen(&screen, test_size(10, 6));
        let narrow = layout_virtual_screen(&screen, test_size(7, 6));

        assert_eq!(visible_row_text(&wide.rows[0]), "user> abcd");
        assert_eq!(visible_row_text(&wide.rows[1]), "efghij");
        assert_eq!(visible_row_text(&narrow.rows[0]), "user> a");
        assert_eq!(visible_row_text(&narrow.rows[1]), "bcdefgh");
        assert_eq!(visible_row_text(&narrow.rows[2]), "ij");
    }

    #[test]
    fn exact_terminal_boundary_places_cursor_on_next_row() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "abcd", 4);
        let terminal = layout_virtual_screen(&screen, test_size(10, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 0 }
        );
    }

    #[test]
    fn wide_character_uses_leading_and_continuation_cells() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "界", 1);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(
            terminal.rows[0].cells[6],
            PhysicalCell::Glyph {
                text: "界".to_string(),
                width: 2,
                style: CellStyle::default(),
            }
        );
        assert_eq!(
            terminal.rows[0].cells[7],
            PhysicalCell::Continuation { leading_column: 6 }
        );
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 8 }
        );
    }

    #[test]
    fn unchanged_frame_produces_no_operations() {
        let terminal = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(20, 5),
        );
        let diff = diff_physical_terminal(Some(&terminal), &terminal);

        assert!(diff.operations.is_empty());
    }

    #[test]
    fn full_redraw_does_not_write_empty_row_tail() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(SHELL_PROMPT), test_size(20, 5));
        let diff = diff_physical_terminal(None, &terminal);

        let writes = diff
            .operations
            .iter()
            .filter_map(|operation| match operation {
                PhysicalOperation::Write(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(writes, ["user> "]);
    }

    #[test]
    fn changed_character_produces_incremental_row_write() {
        let previous = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(20, 5),
        );
        let next = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hallo", 5),
            test_size(20, 5),
        );
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(
            !diff.operations.contains(&PhysicalOperation::ClearAll),
            "same-size frames should not trigger a full redraw"
        );
        assert!(
            diff.operations.iter().any(
                |operation| matches!(operation, PhysicalOperation::Write(text) if text == "a")
            )
        );
    }

    #[test]
    fn geometry_change_triggers_full_redraw() {
        let previous = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(20, 5),
        );
        let next = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(10, 5),
        );
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn viewport_keeps_cursor_visible_for_tall_content() {
        let screen = VirtualScreen::from_render_lines(
            ["one", "two", "three", "four"]
                .into_iter()
                .map(|line| RenderLine::new(SHELL_PROMPT, line))
                .collect(),
            VirtualCursor {
                line: 3,
                char_offset: 4,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(20, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> three");
        assert_eq!(visible_row_text(&terminal.rows[1]), "user> four");
        assert_eq!(terminal.cursor.position.row, 1);
    }

    #[test]
    fn advancing_viewport_uses_terminal_scroll_and_preserves_scrollback() {
        let previous_screen = VirtualScreen::from_render_lines(
            ["one", "two", "three"]
                .into_iter()
                .map(RenderLine::plain)
                .collect(),
            VirtualCursor {
                line: 2,
                char_offset: 5,
            },
            true,
        );
        let next_screen = VirtualScreen::from_render_lines(
            ["one", "two", "three", "four"]
                .into_iter()
                .map(RenderLine::plain)
                .collect(),
            VirtualCursor {
                line: 3,
                char_offset: 4,
            },
            true,
        );
        let size = test_size(20, 2);
        let previous = layout_virtual_screen(&previous_screen, size);
        let next = layout_virtual_screen(&next_screen, size);

        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.contains(&PhysicalOperation::ScrollUp(1)));
        assert!(!diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn indexed_layout_matches_reference_layout() {
        let screen = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("first line"),
                RenderLine::new("prompt> ", "wide 界 value that wraps"),
                RenderLine::plain("tail"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 7,
            },
            true,
        );
        let size = test_size(12, 4);
        let reference = layout_virtual_screen(&screen, size);
        let indexed = IndexedPhysicalLayout::default().layout(&screen, size);

        assert_eq!(indexed, reference);
    }

    #[test]
    fn indexed_layout_reflows_only_edited_cursor_line() {
        let mut layout = IndexedPhysicalLayout::default();
        let size = test_size(30, 8);
        let initial = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("stable one"),
                RenderLine::new("user> ", "draft"),
                RenderLine::plain("stable two"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 5,
            },
            true,
        );
        layout.layout(&initial, size);
        assert_eq!(layout.reflowed_last_frame, 3);

        let mut edited = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("stable one"),
                RenderLine::new("user> ", "draft!"),
                RenderLine::plain("stable two"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 6,
            },
            true,
        );
        edited.dirty_from = 1;
        layout.layout(&edited, size);

        assert_eq!(layout.reflowed_last_frame, 1);
    }

    #[test]
    fn indexed_layout_keeps_stable_prefix_when_lines_are_appended() {
        let mut layout = IndexedPhysicalLayout::default();
        let size = test_size(30, 8);
        let initial = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("transcript one"),
                RenderLine::plain("transcript two"),
                RenderLine::new("user> ", "draft"),
            ],
            VirtualCursor {
                line: 2,
                char_offset: 5,
            },
            true,
        );
        layout.layout(&initial, size);

        let mut appended = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("transcript one"),
                RenderLine::plain("transcript two"),
                RenderLine::new("user> ", "submitted"),
                RenderLine::plain("command output"),
                RenderLine::new("user> ", ""),
            ],
            VirtualCursor {
                line: 4,
                char_offset: 0,
            },
            true,
        );
        appended.dirty_from = 2;
        let indexed = layout.layout(&appended, size);

        assert_eq!(layout.reflowed_last_frame, 3);
        assert_eq!(indexed, layout_virtual_screen(&appended, size));
    }

    #[test]
    fn virtual_screen_tail_can_be_replaced_without_cloning_prefix() {
        let mut screen = VirtualScreen::from_render_lines(
            vec![RenderLine::plain("stable"), RenderLine::plain("old tail")],
            VirtualCursor {
                line: 1,
                char_offset: 8,
            },
            true,
        );
        let stable_pointer = screen.lines[0].as_ptr();

        screen.truncate(1);
        screen.push_render_line(&RenderLine::plain("new tail"));

        assert_eq!(screen.lines, ["stable", "new tail"]);
        assert_eq!(screen.lines[0].as_ptr(), stable_pointer);
    }

    #[test]
    fn command_history_can_be_recalled_from_non_empty_draft() {
        let history = vec![HistoryEntry {
            text: "git status".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.buffer.replace_with("draft");

        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(editor.buffer.text(), "git status");
        assert_eq!(editor.history_draft, "draft");
    }

    #[test]
    fn typed_history_normalization_keeps_latest_trimmed_duplicate() {
        let shell = |text: &str| HistoryEntry {
            text: text.to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        };

        let normalized = normalize_history(vec![
            shell(" first "),
            shell("second"),
            shell("first"),
            shell("  "),
        ]);

        assert_eq!(normalized, vec![shell("second"), shell("first")]);
    }

    #[test]
    fn recalled_single_line_history_is_immediately_editable() {
        let history = vec![HistoryEntry {
            text: "git status".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        editor.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(editor.buffer.text(), "git statusx");
        assert!(editor.history_position.is_some());
        assert!(!editor.history_is_browsing());
    }

    #[test]
    fn recalled_multiline_history_reopens_unified_multiline_editor() {
        let history = vec![HistoryEntry {
            text: "echo one\necho two".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::MultiLineShell,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::OpenMultiline(EditorMode::Shell, "echo one\necho two".to_string())
        );
    }

    #[test]
    fn recalled_multiline_history_renders_command_hint_and_hidden_cursor() {
        let history = vec![HistoryEntry {
            text: "line one\nline two".to_string(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::MultiLineAsk,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        let (lines, cursor) = editor.render_lines();

        assert_eq!(lines[0].prefix, "user> ");
        assert_eq!(lines[0].text, "/ask");
        assert!(lines[1].text.contains(input::MULTILINE_SUBMIT_COMMAND));
        assert_eq!(lines[2].prefix, input::DEFAULT_COMMAND_CONTINUATION_PROMPT);
        assert_eq!(lines[2].text, "line one");
        assert_eq!(cursor.line, 3);
        assert!(editor.history_is_browsing());
    }

    #[test]
    fn plain_arrow_reopens_recalled_multiline_special_editor() {
        let history = vec![HistoryEntry {
            text: "echo one\necho two".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::MultiLineShell,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            EditorOutcome::OpenMultiline(EditorMode::Shell, "echo one\necho two".to_string())
        );
    }

    #[test]
    fn multiline_editor_history_is_preview_until_enter() {
        let history = vec![HistoryEntry {
            text: "previous question".to_string(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::ask(None, &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert!(editor.history_is_browsing());
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.buffer.text(), "previous question");
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::Changed
        );
        assert!(!editor.history_is_browsing());
    }

    #[test]
    fn api_key_editor_masks_rendered_text_but_submits_original() {
        let mut editor = UnifiedEditor::api_key();
        editor.buffer.replace_with("secret");
        let (lines, _) = editor.render_lines();

        assert_eq!(lines[0].text, "******");
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::Submit(EditorSubmission {
                kind: SubmissionKind::ApiKey,
                text: "secret".to_string(),
            })
        );
    }

    #[test]
    fn api_key_paste_discards_line_breaks() {
        let mut editor = UnifiedEditor::api_key();

        editor.handle_event(Event::Paste("first\r\nsecond\n".to_string()));

        assert_eq!(editor.buffer.text(), "firstsecond");
        assert_eq!(editor.buffer.lines.len(), 1);
    }

    #[test]
    fn multiline_ctrl_d_submits_without_end_marker() {
        let mut editor = UnifiedEditor::shell(None, &[]);
        editor.buffer.replace_with("echo one\necho two");

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            EditorOutcome::Submit(EditorSubmission {
                kind: SubmissionKind::Shell,
                text: "echo one\necho two".to_string(),
            })
        );
    }

    #[test]
    fn completion_includes_git_subcommands() {
        let (_, candidates) = completion_candidates("git che", 7, true).unwrap();

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.replacement == "checkout")
        );
    }

    #[test]
    fn ansi_styles_are_cells_not_layout_characters() {
        let lines = ansi_render_lines("\x1b[1;32mgreen\x1b[0m");
        let screen = VirtualScreen::from_render_lines(
            lines,
            VirtualCursor {
                line: 0,
                char_offset: 5,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(10, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "green");
        assert_eq!(terminal.cursor.position.column, 5);
        assert!(matches!(
            terminal.rows[0].cells[0],
            PhysicalCell::Glyph {
                style: CellStyle {
                    foreground: Some(TerminalColor::Green),
                    bold: true,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn ansi_tab_is_preserved_and_uses_native_terminal_tab_stop() {
        let lines = ansi_render_lines("X\tTAB_RIGHT\n");

        assert_eq!(lines[0].text, "X\tTAB_RIGHT");
        assert_eq!(lines[0].styles.len(), char_len("X\tTAB_RIGHT"));

        let screen = VirtualScreen::from_render_lines(
            lines,
            VirtualCursor {
                line: 0,
                char_offset: char_len("X\tTAB_RIGHT"),
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(80, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "X       TAB_RIGHT");
        assert_eq!(terminal.cursor.position.column, 17);
    }

    #[test]
    fn ansi_carriage_return_and_erase_line_do_not_create_logical_rows() {
        let lines = ansi_render_lines("\r\x1b[K* exps\r\n  master\r\n\r\x1b[K");

        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["* exps", "  master"]
        );
    }

    #[test]
    fn ansi_carriage_return_overwrites_current_logical_line() {
        let lines = ansi_render_lines("progress 10%\rprogress 20%");

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "progress 20%");
    }

    #[test]
    fn terminal_clear_keeps_only_output_after_last_display_clear() {
        assert_eq!(suffix_after_last_display_clear("plain text"), None);
        assert_eq!(
            suffix_after_last_display_clear("discard\x1b[H\x1b[2Jkeep"),
            Some("keep")
        );
        assert_eq!(
            suffix_after_last_display_clear("discard\x1b[2Jmiddle\x1b[2Jfinal"),
            Some("final")
        );
        assert_eq!(
            suffix_after_last_display_clear("keep\x1b[0Jtoo"),
            None,
            "ED 0 only clears below the cursor and must not discard the transcript"
        );
        assert_eq!(
            suffix_after_last_display_clear("keep\x1b[3Jtoo"),
            None,
            "ED 3 clears scrollback, not the visible display"
        );
    }

    #[test]
    fn ansi_true_color_indexed_background_and_attributes_round_trip() {
        let lines = ansi_render_lines("\x1b[38;2;1;2;3;48;5;208;7;9mX\x1b[0m");
        let style = lines[0].styles[0];

        assert_eq!(style.foreground, Some(TerminalColor::Rgb(1, 2, 3)));
        assert_eq!(style.background, Some(TerminalColor::Indexed(208)));
        assert!(style.reverse);
        assert!(style.strikethrough);

        let rendered = terminal_styled_text(&lines[0].text, &lines[0].styles);
        assert!(rendered.contains("\x1b[38;2;1;2;3m"));
        assert!(rendered.contains("\x1b[48;5;208m"));
        assert!(rendered.contains("\x1b[7m"));
        assert!(rendered.contains("\x1b[9m"));
    }

    #[test]
    fn style_only_change_is_emitted_as_physical_diff() {
        let plain = VirtualScreen::from_render_lines(
            vec![RenderLine::plain("x")],
            VirtualCursor {
                line: 0,
                char_offset: 1,
            },
            true,
        );
        let styled = VirtualScreen::from_render_lines(
            vec![RenderLine::styled(
                "x",
                vec![CellStyle {
                    foreground: Some(TerminalColor::Red),
                    ..CellStyle::default()
                }],
            )],
            VirtualCursor {
                line: 0,
                char_offset: 1,
            },
            true,
        );
        let previous = layout_virtual_screen(&plain, test_size(10, 2));
        let next = layout_virtual_screen(&styled, test_size(10, 2));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.iter().any(|operation| {
            matches!(operation, PhysicalOperation::Write(text) if text.contains("\x1b[31m") && text.contains('x'))
        }));
        assert!(!diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn shell_styles_follow_configured_palette() {
        let mut palette = input::default_shell_highlight_palette();
        palette.insert(
            "keyword".to_string(),
            Some(input::ShellHighlightStyle::tags(vec![
                "bold".to_string(),
                "red".to_string(),
            ])),
        );
        let mut lines = vec![RenderLine::plain("if true; then")];

        style_shell_lines(&mut lines, "if true; then", &palette);

        assert_eq!(lines[0].styles[0].foreground, Some(TerminalColor::Red));
        assert!(lines[0].styles[0].bold);
    }

    #[test]
    fn slash_command_only_styles_command_name_in_command_editor() {
        let mut lines = vec![RenderLine::new("user> ", "/ask echo \"$USER\"")];

        style_shell_lines(
            &mut lines,
            "/ask echo \"$USER\"",
            &input::default_shell_highlight_palette(),
        );

        assert_eq!(
            lines[0].styles[0].foreground,
            Some(TerminalColor::BrightCyan)
        );
        assert_eq!(lines[0].styles[5], CellStyle::default());
        assert_eq!(lines[0].styles.last(), Some(&CellStyle::default()));
    }

    #[test]
    fn picker_anchors_viewport_after_its_last_row() {
        let mut picker = PickerState::new(
            "Choose",
            vec![
                PickerItem {
                    id: "one".to_string(),
                    label: "One".to_string(),
                    detail: String::new(),
                },
                PickerItem {
                    id: "two".to_string(),
                    label: "Two".to_string(),
                    detail: String::new(),
                },
            ],
            false,
        );
        let (lines, cursor, visible) = picker.render_lines(10, 80);

        assert_eq!(cursor.line, lines.len() - 1);
        assert!(!visible);
    }
}
