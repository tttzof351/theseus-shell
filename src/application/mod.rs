//! Diff-rendered Theseus application.

use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstRootNode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::config::model_catalog;
use crate::{
    agent::{AgentConfig, AgentRunContext, ShellCommandContext},
    commands::{self, SlashCommand, parse_slash_command},
    common::{self, tmp_files::cleanup_expired_tmp_files_async},
    input,
    logging::{AppLogger, default_logs_dir},
    shell::{
        self,
        command_routing::{CommandRoute, classify_command},
        markdown_preprocessor,
        pty::{PersistentShellConfig, PersistentShellSession},
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

use crate::terminal_renderer::*;

mod document_layout;
mod editor;
mod event_loop;
use event_loop::run_interactive_application;
mod markdown_references;
mod markdown_source;
mod state;
use state::ExecutionState;
mod plain;
mod terminal;
mod terminal_motion;
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

struct ShellPresentation {
    display: terminal::ShellDisplay,
    published: Option<crate::terminal_renderer::managed::PublicationAnchor>,
}

struct Application {
    document: output_document::OutputDocument,
    rendered_stable_lines: usize,
    publication: Vec<crate::terminal_renderer::managed::PublicationUnit>,
    active_operation: Option<crate::agent::worker::ActiveOperation>,
    pending_shell: Option<String>,
    transcript: Vec<RenderLine>,
    screen_cache: VirtualScreen,
    cached_transcript_lines: usize,
    interaction: Interaction,
    history: Vec<HistoryEntry>,
    history_path: Option<PathBuf>,
    command_records: Vec<CommandRecord>,
    config: AgentConfig,
    config_path: PathBuf,
    agent: crate::agent::worker::AgentWorker,
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
    terminal: Option<terminal::TerminalController>,
    plain: Option<plain::PlainFrontend>,
    pending_models: Option<std::sync::mpsc::Receiver<model_catalog::ModelCatalog>>,
    saved_model_draft: Option<UnifiedEditor>,
    pending_config_confirmation: Option<String>,
    layout_worker: Option<document_layout::Worker>,
    prepared_layout: Option<IndexedPhysicalLayout>,
    layout_feedback: Option<(u64, common::events::Outcome)>,
}

impl Application {
    fn new() -> io::Result<Self> {
        let init = AgentConfig::load_or_create_default()?;
        cleanup_expired_tmp_files_async(init.config.agent_settings.tmp_files_ttl_min);
        let logger = AppLogger::start_session()?;
        let agent = crate::agent::worker::AgentWorker::new(init.config.clone(), logger.clone())?;
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
            document: output_document::OutputDocument::default(),
            rendered_stable_lines: 0,
            publication: Vec::new(),
            active_operation: None,
            pending_shell: None,
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
            terminal: None,
            plain: None,
            pending_models: None,
            saved_model_draft: None,
            pending_config_confirmation: None,
            layout_worker: None,
            prepared_layout: None,
            layout_feedback: None,
        };
        app.append_text(&common::info::render_info());
        Ok(app)
    }

    fn screen(&mut self, terminal_size: TerminalSize) -> &VirtualScreen {
        let dirty_from = self
            .transcript
            .iter()
            .enumerate()
            .position(|(index, line)| {
                self.screen_cache.lines.get(index) != Some(&line.text)
                    || self.screen_cache.prefixes.get(index) != Some(&line.prefix)
                    || self.screen_cache.line_styles.get(index) != Some(&line.styles)
                    || self.screen_cache.prefix_styles.get(index) != Some(&line.prefix_styles)
            })
            .unwrap_or(self.transcript.len());
        // Preserve stable text/styles in the virtual screen; replace only the
        // changed suffix, including the status and editor from the last frame.
        self.screen_cache.truncate(dirty_from);
        for line in &self.transcript[dirty_from..] {
            self.screen_cache.push_render_line(line);
        }
        self.cached_transcript_lines = self.transcript.len();
        if let Some((_, outcome)) = &self.layout_feedback {
            let text = match outcome {
                common::events::Outcome::Cancelled => "[interrupted]".into(),
                common::events::Outcome::Failed(error) => {
                    format!("[failed: {}]", terminal_label(error))
                }
                common::events::Outcome::Completed => String::new(),
            };
            self.screen_cache.push_render_line(&RenderLine::plain(text));
        }
        if let Some(active) = &self.active_operation {
            let (phase, detail) = self
                .document
                .activity
                .clone()
                .unwrap_or_else(|| ("Working".into(), String::new()));
            let phase = if self.execution_state() == ExecutionState::Cancelling {
                "Cancelling"
            } else {
                &phase
            };
            self.screen_cache
                .push_render_line(&RenderLine::plain(format!(
                    "{phase} · {}s {detail}",
                    active.started.elapsed().as_secs()
                )));
        }
        let base = self.screen_cache.lines.len();
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
        if matches!(
            self.execution_state(),
            ExecutionState::Stopping | ExecutionState::ShellPassthrough
        ) {
            return Ok(false);
        }
        if let Some(active) = &self.active_operation {
            if let Event::Key(key) = &event {
                if is_key_action(key.kind)
                    && key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    active.cancellation.cancel();
                    return Ok(true);
                }
                if is_key_action(key.kind)
                    && (key.code == KeyCode::Enter
                        || (key.code == KeyCode::Char('d')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                    && let Interaction::Editor(editor) = &mut self.interaction
                    && (!editor.history_is_browsing() || key.code != KeyCode::Enter)
                {
                    if matches!(editor.mode, EditorMode::Ask | EditorMode::Shell)
                        && key.code == KeyCode::Enter
                    {
                        editor.buffer.split_line();
                    }
                    self.sync_multiline_draft();
                    return Ok(true);
                }
            }
            if let Event::Paste(text) = &event {
                if let Interaction::Editor(editor) = &mut self.interaction {
                    let outcome = editor.handle_draft_paste(text);
                    return self.finish_editor_outcome(outcome);
                }
                return Ok(true);
            }
        }
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
                if matches!(submission_kind, SubmissionKind::Ask | SubmissionKind::Shell) {
                    // Enter may have removed /end without producing a Changed
                    // outcome. Replace the draft before storing the submission.
                    self.sync_multiline_draft();
                }
                self.commit_submission(&submission);
                self.execute_submission(submission)?;
                if submission_kind != SubmissionKind::Command
                    && self.active_operation.is_none()
                    && self.pending_shell.is_none()
                {
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
                // Ctrl+L is a logical screen clear, not merely a request to
                // repaint the same scene. Drop the committed transcript while
                // keeping the active editor (and its draft) intact. `screen`
                // notices that the cached prefix is now longer than the
                // transcript and rebuilds the VirtualScreen from line zero.
                self.clear_output();
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
        self.document.append_lines(committed);
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

    fn ensure_shell_session(&mut self) -> io::Result<()> {
        if self.shell_session.is_none() {
            self.shell_session = Some(PersistentShellSession::start(PersistentShellConfig {
                shell: self.shell_path.clone(),
                env_vars: self.shell_env.clone(),
                working_dir: self.working_dir.clone(),
            })?);
        }
        Ok(())
    }

    fn run_shell(&mut self, command: &str) -> io::Result<()> {
        if self.terminal.is_some() {
            self.pending_shell = Some(command.into());
            return Ok(());
        }
        self.run_shell_now(command).map(|_| ())
    }

    fn run_shell_now(&mut self, command: &str) -> io::Result<Option<ShellPresentation>> {
        self.ensure_shell_session()?;
        let mut lease = self
            .terminal
            .as_ref()
            .map(terminal::TerminalController::lease_shell)
            .transpose()?;
        let _external = if lease.is_none() {
            Some(ExternalTerminalGuard::enter()?)
        } else {
            None
        };
        self.physical_invalidated = true;
        let session = self.shell_session.as_mut().expect("shell initialized");
        let output = match lease.as_mut() {
            Some(lease) => session.run_command_with_terminal(
                command,
                lease.input.clone(),
                &mut lease.writer,
            )?,
            None => session.run_command(command)?,
        };
        self.last_output_streamed = output.streamed;
        self.last_command_status = output.status_code.unwrap_or(1);
        if let Ok(working_dir) = session.current_working_dir()
            && env::set_current_dir(&working_dir).is_ok()
        {
            self.working_dir = Some(working_dir);
        }
        let text = output.transcript_lossy();
        let primary = primary_screen_output(&text);
        let display = lease.as_ref().map(|lease| lease.display());
        let visible = if let Some(visible_suffix) = suffix_after_last_display_clear(&primary) {
            // The streamed terminal has already discarded everything that
            // preceded ED 2. Mirror that state transition in the
            // persistent virtual scene so the recovery frame cannot bring
            // the old transcript back.
            self.clear_output();
            visible_suffix
        } else {
            &primary
        };
        let shell_block = self.document.append_lines(ansi_render_lines(visible));
        let published = shell_block.and_then(|block| {
            let end = display?.published_byte?.min(output.transcript.len());
            let prefix = primary_screen_output(&String::from_utf8_lossy(&output.transcript[..end]));
            let prefix = suffix_after_last_display_clear(&prefix).unwrap_or(&prefix);
            let lines = ansi_render_lines(prefix);
            let last = lines.last()?;
            Some(crate::terminal_renderer::managed::PublicationAnchor {
                id: crate::terminal_renderer::managed::RowIdentity {
                    block,
                    group: lines.len() - 1,
                },
                characters: last.text.chars().count(),
            })
        });
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
        Ok(display.map(|display| ShellPresentation { display, published }))
    }

    fn run_agent(&mut self, prompt: &str) -> io::Result<()> {
        common::cancellation::clear_sigint_request();
        let context = AgentRunContext {
            shell: self.shell_path.clone(),
            shell_prompt: shell_prompt(self.working_dir.as_deref()),
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

    fn start_operation(
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

    fn poll_operation(&mut self) -> io::Result<bool> {
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

    fn wait_for_operation(&mut self) -> io::Result<()> {
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

    fn reset_agent(&mut self) -> io::Result<()> {
        let init = AgentConfig::load_or_create_at(self.config_path.clone())?;
        self.config = init.config;
        self.logger = AppLogger::start_session()?;
        self.apply_configuration("Agent context has been reset.\n".into(), "/reset")?;
        Ok(())
    }

    fn compact_agent(&mut self) -> io::Result<()> {
        self.start_operation(crate::agent::worker::Operation::Compact, "/compact".into())
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
                let (reply, models) = std::sync::mpsc::channel();
                self.start_operation(
                    crate::agent::worker::Operation::ModelCatalog { reply },
                    "/config".into(),
                )?;
                self.pending_models = Some(models);
                self.return_to_command_editor();
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

    fn show_model_catalog(&mut self, catalog: model_catalog::ModelCatalog) {
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
                    label: format!("{}{}", model.id, if is_current { " (current)" } else { "" }),
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
                self.return_to_command_editor();
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
        self.apply_configuration(
            format!("Config saved to {}\n", self.config_path.display()),
            "/config",
        )?;
        Ok(())
    }

    fn apply_configuration(&mut self, confirmation: String, command: &str) -> io::Result<()> {
        self.start_operation(
            crate::agent::worker::Operation::Configure {
                config: Box::new(self.config.clone()),
                logger: self.logger.clone(),
            },
            command.into(),
        )?;
        self.pending_config_confirmation = Some(confirmation);
        Ok(())
    }

    fn open_resume(&mut self) -> io::Result<()> {
        let sessions = resume_sessions(self.config.agent_settings.max_resume_traj)?;
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
                    self.start_operation(
                        crate::agent::worker::Operation::Resume(session.path),
                        "/resume".into(),
                    )?;
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
        self.interaction = Interaction::Editor(
            self.saved_model_draft
                .take()
                .unwrap_or_else(|| UnifiedEditor::command(prompt, &self.history)),
        );
    }

    fn sync_multiline_draft(&mut self) {
        let Some((mut slot, kind, mode)) = self.active_draft else {
            return;
        };
        let text = match &self.interaction {
            Interaction::Editor(editor)
                if matches!(editor.mode, EditorMode::Ask | EditorMode::Shell) =>
            {
                multiline_history_text(&editor.buffer.text()).to_string()
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

    fn refresh_document(&mut self, width: usize) -> io::Result<()> {
        if self.terminal.is_some()
            && (self.layout_worker.is_some() || self.document.source_bytes() >= 128 * 1024)
        {
            if self.layout_worker.is_none() {
                self.layout_worker = Some(document_layout::Worker::new()?);
            }
            let worker = self.layout_worker.as_mut().unwrap();
            let key = document_layout::Key::of(&self.document, width);
            if let Some(prepared) = worker.take(key)? {
                self.rendered_stable_lines = prepared.rendered.stable_lines;
                self.publication = prepared.rendered.publication;
                self.transcript = prepared.rendered.lines;
                self.screen_cache = prepared.screen;
                self.prepared_layout = Some(prepared.layout);
                if self
                    .layout_feedback
                    .as_ref()
                    .is_some_and(|(version, _)| *version <= prepared.key.version)
                {
                    self.layout_feedback = None;
                }
            }
            worker.request(key, &self.document);
            return Ok(());
        }
        if !self.document.needs_render(width) {
            return Ok(());
        }
        let rendered = self.document.render(width);
        self.rendered_stable_lines = rendered.stable_lines;
        self.publication = rendered.publication;
        self.transcript = rendered.lines;
        Ok(())
    }

    fn layout_pending(&self, width: usize) -> bool {
        self.layout_worker
            .as_ref()
            .is_some_and(|worker| worker.pending(document_layout::Key::of(&self.document, width)))
    }

    fn prepare_document(&mut self, width: usize) -> io::Result<()> {
        loop {
            self.refresh_document(width)?;
            if !self.layout_pending(width) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn clear_output(&mut self) {
        self.document.clear_visible();
        self.transcript.clear();
        self.cached_transcript_lines = 0;
        self.prepared_layout = None;
        self.layout_feedback = None;
    }

    fn append_text(&mut self, text: &str) {
        self.document.append_lines(ansi_render_lines(text));
    }

    fn append_markdown(&mut self, text: &str) {
        self.document
            .append_lines(ansi_render_lines(&render_markdown(text)));
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
        print_plain_transcript(&ansi_render_lines(&common::info::render_info()), false)?;
        app.clear_output();
        app.execute_command(&command)?;
        if app.active_operation.is_some() && io::stdin().is_terminal() && io::stdout().is_terminal()
        {
            return run_interactive_application(app, true);
        }
        app.wait_for_operation()?;
        if interaction_needs_input(&app.interaction) {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                let mut transcript = ansi_render_lines(&common::info::render_info());
                app.refresh_document(80)?;
                transcript.append(&mut app.transcript);
                app.document = output_document::OutputDocument::default();
                app.document.append_lines(transcript);
                return run_interactive_application(app, true);
            }
            print_plain_transcript(&app.transcript, true)?;
            io::stdout().flush()?;
            app.clear_output();
            let stdin = io::stdin();
            let mut input = stdin.lock();
            finish_plain_interactions(&mut app, &mut input)?;
        }
        if !app.last_output_streamed {
            print_plain_transcript(&app.transcript, true)?;
        }
        return Ok(app.last_command_status);
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return run_plain_application(app);
    }

    run_interactive_application(app, false)
}

fn run_plain_application(mut app: Application) -> io::Result<i32> {
    app.wait_for_operation()?;
    print_plain_transcript(&app.transcript, false)?;
    io::stdout().flush()?;
    app.clear_output();
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
        app.wait_for_operation()?;
        flush_plain_application_transcript(&mut app)?;
        if app.exit_requested {
            break;
        }
    }
    Ok(app.last_command_status)
}

fn flush_plain_application_transcript(app: &mut Application) -> io::Result<()> {
    app.wait_for_operation()?;
    print_plain_transcript(&app.transcript, false)?;
    io::stdout().flush()?;
    app.clear_output();
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

fn print_plain_transcript(lines: &[RenderLine], only_unprefixed: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    let terminal = stdout.is_terminal();
    for line in lines {
        if !only_unprefixed || line.prefix.is_empty() {
            let prefix = if only_unprefixed {
                String::new()
            } else if terminal {
                terminal_styled_text(&line.prefix, &line.prefix_styles)
            } else {
                line.prefix.clone()
            };
            let text = if terminal {
                terminal_styled_text(&line.text, &line.styles)
            } else {
                line.text.clone()
            };
            writeln!(stdout, "{prefix}{text}")?;
        }
    }
    stdout.flush()
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
    common::cancellation::install_sigint_handler();
    let init = AgentConfig::load_or_create_default()?;
    cleanup_expired_tmp_files_async(init.config.agent_settings.tmp_files_ttl_min);
    let logger = AppLogger::start_session()?;
    let worker = crate::agent::worker::AgentWorker::new(init.config, logger.clone())?;
    // Declared after worker: consumer drops (and cancels) before the worker joins.
    let mut active = worker.start(
        crate::agent::worker::Operation::Run {
            prompt: prompt.into(),
            context: Box::new(AgentRunContext {
                logger: Some(logger.clone()),
                ..AgentRunContext::default()
            }),
        },
        prompt.into(),
    )?;
    let mut plain = plain::PlainFrontend::default();
    while !active.finished {
        active.cancellation.cancel_if_interrupted();
        match active
            .events
            .recv_timeout(std::time::Duration::from_millis(10))
        {
            Ok(event) => {
                let terminal = matches!(event.event, common::events::OutputEvent::Finished { .. });
                let operation = event.operation;
                let sequence = event.sequence;
                let (kind, block) = event.event.identity();
                let rejected = plain.rejected_events;
                if operation == active.id {
                    plain.apply(event, &mut io::stdout(), &mut io::stderr())?;
                }
                if operation == active.id && rejected == plain.rejected_events {
                    active.finished |= terminal;
                } else {
                    log_rejected_backend_event(
                        &logger, active.id, operation, sequence, kind, block,
                    );
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if active.output_disconnected().is_some() {
                    log_output_disconnect(&logger, active.id);
                    plain.finish(&mut io::stdout(), &mut io::stderr())?;
                }
            }
        }
    }
    let completion = active
        .completion
        .recv()
        .map_err(|_| io::Error::other("agent worker stopped"))?;
    let completion = active.checked_completion(completion);
    let cancelled = completion.outcome == common::events::Outcome::Cancelled;
    match completion.result {
        Ok(text) => {
            if cancelled {
                plain::write_diagnostic(&mut io::stderr(), text.trim_end())?;
            }
            Ok(if cancelled { 130 } else { 0 })
        }
        Err(error) => {
            plain::write_diagnostic(
                &mut io::stderr(),
                &format!("theseus: agent run failed: {error}"),
            )?;
            Ok(if cancelled { 130 } else { 1 })
        }
    }
}

fn log_rejected_backend_event(
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

fn log_output_disconnect(logger: &AppLogger, operation: common::events::OperationId) {
    let _ = logger.event(
        "error",
        "backend_output_disconnected",
        json!({
            "operation_id": operation.0,
            "reason": "event channel closed before Finished",
        }),
    );
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

fn multiline_history_text(text: &str) -> &str {
    let text = text.trim();
    let (body, last_line) = text.rsplit_once('\n').unwrap_or(("", text));
    if last_line.trim() == input::MULTILINE_SUBMIT_COMMAND {
        body.trim()
    } else {
        text
    }
}

fn normalize_history(history: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
    let mut history = history
        .into_iter()
        .filter_map(|mut entry| {
            entry.text = match entry.mode {
                HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell => {
                    multiline_history_text(&entry.text).to_string()
                }
                _ => entry.text.trim().to_string(),
            };
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
    resume_sessions_in(&directory, limit)
}

fn resume_sessions_in(directory: &Path, limit: usize) -> io::Result<Vec<ResumeSession>> {
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
    paths.sort_by_cached_key(|path| std::cmp::Reverse(resume_sort_key(path)));
    Ok(paths
        .into_iter()
        .take(limit)
        .filter_map(|path| resume_session_from_path(&path).ok())
        .collect())
}

fn resume_sort_key(path: &Path) -> (String, u64) {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let timestamp = name.strip_suffix("_trajectory.json").unwrap_or(&name);
    if let Some((base, suffix)) = timestamp.rsplit_once('-')
        && base.split('-').count() == 6
        && let Ok(sequence) = suffix.parse()
    {
        return (base.into(), sequence);
    }
    (timestamp.into(), 0)
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
        .rfind(|text| !text.is_empty() && !text.starts_with("Last shell command:"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no user question"))?;
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-date");
    let timestamp = file.strip_suffix("_trajectory.json").unwrap_or(file);
    let parts = timestamp.split('-').collect::<Vec<_>>();
    let date = if parts.len() >= 6 {
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

mod ansi;
use ansi::*;
mod ansi_decoder;
mod output_document;

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
    fn resume_orders_same_second_sessions_numerically_and_keeps_legacy_names() {
        let directory = temporary_test_path("resume-order");
        fs::create_dir_all(&directory).unwrap();
        for (suffix, question) in [("", "legacy"), ("-000002", "second"), ("-000010", "tenth")] {
            fs::write(
                directory.join(format!("2026-09-13-01-02-03{suffix}_trajectory.json")),
                serde_json::to_vec(&json!({"messages":[{"role":"user","content":question}]}))
                    .unwrap(),
            )
            .unwrap();
        }
        let sessions = resume_sessions_in(&directory, 3).unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.question.as_str())
                .collect::<Vec<_>>(),
            ["tenth", "second", "legacy"]
        );
        assert!(sessions.iter().all(|s| s.date == "2026-09-13 01:02:03"));
        assert_eq!(
            resume_sessions_in(&directory, 1).unwrap()[0].question,
            "tenth"
        );
        fs::remove_dir_all(directory).unwrap();
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
    fn multiline_history_normalization_removes_legacy_submit_markers_and_duplicates() {
        for (kind, mode) in [
            (HistoryKind::Agent, HistoryMode::MultiLineAsk),
            (HistoryKind::Shell, HistoryMode::MultiLineShell),
        ] {
            let entry = |text: &str| HistoryEntry {
                text: text.into(),
                kind,
                mode,
            };
            assert_eq!(
                normalize_history(vec![
                    entry("first\n\nsecond\n /end \n"),
                    entry("another"),
                    entry("first\n\nsecond"),
                    entry("/end"),
                ]),
                vec![entry("another"), entry("first\n\nsecond")]
            );
            for text in [
                "explain /end",
                "/end\nmore text",
                "echo /end",
                "question\n/en",
            ] {
                assert_eq!(normalize_history(vec![entry(text)]), vec![entry(text)]);
            }
        }
        let entry = HistoryEntry {
            text: "/end".into(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::SingleLineAsk,
        };
        assert_eq!(normalize_history(vec![entry.clone()]), vec![entry]);
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

#[cfg(test)]
mod managed_ui_tests;
