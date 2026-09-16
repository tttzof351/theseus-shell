//! CLI dispatch and non-interactive application entry points.
use super::{
    Application, Interaction,
    ansi::{ansi_render_lines, terminal_styled_text},
    editor::{EditorMode, EditorSubmission, SubmissionKind},
    event_loop::run_interactive_application,
    interaction::interaction_needs_input,
    operations::{log_output_disconnect, log_rejected_backend_event},
    output_document,
    picker::PickerOutcome,
    plain,
};
use crate::{
    agent::{AgentConfig, AgentRunContext},
    commands,
    common::{self, tmp_files::cleanup_expired_tmp_files_async},
    input,
    logging::AppLogger,
    terminal_renderer::RenderLine,
};
use std::{
    env,
    io::{self, BufRead, IsTerminal, Write},
};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
