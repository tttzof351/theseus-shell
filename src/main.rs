use std::io::{self, Write};

use theseus::agent::{Agent, AgentConfig};
use theseus::common::info::render_info;
use theseus::common::terminal_output;
use theseus::common::tmp_files::cleanup_expired_tmp_files_async;
use theseus::input::{BoxOptions, colorize_nested, wrap_in_box};
use theseus::logging::AppLogger;
use theseus::shell::{ShellConfig, run_shell};

/// Exit code for a successful headless agent run.
const EXIT_OK: i32 = 0;
/// Exit code for an agent/runtime error (LLM failure, tool loop, etc.).
const EXIT_AGENT_ERROR: i32 = 1;
/// Exit code for configuration or CLI usage errors.
const EXIT_USAGE: i32 = 2;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match parse_cli(&raw) {
        Ok(Cli::Version) => {
            if let Err(err) = write_stdout_line(&format!("theseus {}", theseus::commands::VERSION))
            {
                eprintln!("theseus: failed to write to stdout: {err}");
                std::process::exit(EXIT_USAGE);
            }
            std::process::exit(EXIT_OK);
        }
        Ok(Cli::Help) => {
            if let Err(err) = print_help() {
                eprintln!("theseus: failed to write help: {err}");
                std::process::exit(EXIT_USAGE);
            }
            std::process::exit(EXIT_OK);
        }
        Ok(Cli::Headless(prompt)) => {
            let code = run_headless(&prompt);
            std::process::exit(code);
        }
        Ok(Cli::Shell(args)) => match run_shell_command(args) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("Shell error: {err}");
                std::process::exit(EXIT_USAGE);
            }
        },
        Err(usage_error) => {
            eprintln!("theseus: {usage_error}\n");
            if let Err(err) = print_help() {
                eprintln!("theseus: failed to write help: {err}");
            }
            std::process::exit(EXIT_USAGE);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Shell(Vec<String>),
    /// Run the agent on `prompt` and print the final answer to stdout.
    Headless(String),
    Version,
    Help,
}

/// Parse a CLI argument vector. Any flag other than `-p/--prompt`,
/// `-v/--version`, or `-h/--help` is treated as a positional argument and
/// passed through to the interactive shell.
///
/// The argument vector should NOT include the binary name (i.e. what
/// `std::env::args().skip(1)` returns).
fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut iter = args.iter();
    let mut shell_args: Vec<String> = Vec::with_capacity(args.len());

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--version" | "-v" => return Ok(Cli::Version),
            "--help" | "-h" => return Ok(Cli::Help),
            "--prompt" | "-p" => {
                // `-p` consumes exactly one following argument as the prompt.
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

fn print_help() -> io::Result<()> {
    let usage = format!(
        "<bold><green>Theseus shell wrapper (v{})</green></bold>\n\n\
         <bright-black>-p --prompt 'Say Hello'   run the agent non-interactively</bright-black>\n\
         <bright-black>-v --version              print the version and exit</bright-black>\n\
         <bright-black>-h --help                 print this help and exit</bright-black>\n\n\
         <cyan>~/.theseus/config.jsonc</cyan>\n\
         <cyan>~/.theseus/logs</cyan>",
        theseus::commands::VERSION,
    );

    let boxed = wrap_in_box(
        &usage,
        BoxOptions {
            max_width: 58,
            border_color: Some("orange".to_string()),
            has_tags: true,
        },
    );

    terminal_output::with_stdout(|stdout| {
        writeln!(stdout, "{}", colorize_nested(&boxed))?;
        writeln!(stdout)?;
        stdout.flush()
    })
}

fn run_shell_command(args: Vec<String>) -> std::io::Result<i32> {
    let agent_init = AgentConfig::load_or_create_default()?;
    cleanup_expired_tmp_files_async(agent_init.config.agent_settings.tmp_files_ttl_min);
    let logger = AppLogger::start_session()?;
    let agent = Agent::new(agent_init.config.clone()).with_logger(logger.clone());
    let config: ShellConfig = ShellConfig {
        args,
        agent_config: Some(agent_init.config),
        agent_config_path: Some(agent_init.path),
        agent: Some(agent),
        logger: Some(logger),
        ..ShellConfig::default()
    };

    terminal_output::with_stdout(|stdout| {
        write!(stdout, "{}", render_info())?;
        stdout.flush()
    })?;

    run_shell(config)
}

/// Initialize the agent and run `prompt` once, printing the final assistant
/// message to stdout. Returns the process exit code.
fn run_headless(prompt: &str) -> i32 {
    let agent_init = match AgentConfig::load_or_create_default() {
        Ok(init) => init,
        Err(err) => {
            eprintln!("theseus: failed to load agent config: {err}");
            return EXIT_USAGE;
        }
    };
    cleanup_expired_tmp_files_async(agent_init.config.agent_settings.tmp_files_ttl_min);
    let logger = match AppLogger::start_session() {
        Ok(logger) => logger,
        Err(err) => {
            eprintln!("theseus: failed to start logger: {err}");
            return EXIT_USAGE;
        }
    };

    let mut agent = Agent::new(agent_init.config.clone()).with_logger(logger.clone());
    let mut context = theseus::agent::AgentRunContext::default();
    context.logger = Some(logger.clone());

    match agent.run_with_context(prompt, context) {
        Ok(output) => {
            // `output` already ends with a trailing newline.
            if let Err(err) = terminal_output::with_stdout(|stdout| {
                stdout.write_all(output.as_bytes())?;
                stdout.flush()
            }) {
                eprintln!("theseus: failed to flush stdout: {err}");
                return EXIT_USAGE;
            }
            EXIT_OK
        }
        Err(err) => {
            eprintln!("theseus: agent run failed: {err}");
            EXIT_AGENT_ERROR
        }
    }
}

fn write_stdout_line(line: &str) -> io::Result<()> {
    terminal_output::with_stdout(|stdout| {
        writeln!(stdout, "{line}")?;
        stdout.flush()
    })
}

#[cfg(test)]
mod tests {
    use super::{Cli, parse_cli};

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parses_version_flag() {
        assert_eq!(parse_cli(&s(&["--version"])), Ok(Cli::Version));
        assert_eq!(parse_cli(&s(&["-v"])), Ok(Cli::Version));
    }

    #[test]
    fn parses_help_flag() {
        assert_eq!(parse_cli(&s(&["--help"])), Ok(Cli::Help));
        assert_eq!(parse_cli(&s(&["-h"])), Ok(Cli::Help));
    }

    #[test]
    fn parses_prompt_flag() {
        assert_eq!(
            parse_cli(&s(&["--prompt", "do thing"])),
            Ok(Cli::Headless("do thing".to_string()))
        );
        assert_eq!(
            parse_cli(&s(&["-p", "do thing"])),
            Ok(Cli::Headless("do thing".to_string()))
        );
    }

    #[test]
    fn prompt_requires_value() {
        assert!(parse_cli(&s(&["--prompt"])).is_err());
        assert!(parse_cli(&s(&["-p"])).is_err());
    }

    #[test]
    fn prompt_rejects_extra_args() {
        assert!(parse_cli(&s(&["-p", "do thing", "extra"])).is_err());
        assert!(parse_cli(&s(&["--prompt", "do thing", "extra"])).is_err());
    }

    #[test]
    fn default_is_shell() {
        assert_eq!(parse_cli(&s(&[])), Ok(Cli::Shell(vec![])));
        assert_eq!(
            parse_cli(&s(&["ls", "-la"])),
            Ok(Cli::Shell(vec!["ls".to_string(), "-la".to_string()]))
        );
    }
}
