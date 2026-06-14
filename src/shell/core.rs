use std::{env, io, path::PathBuf};

use super::pty::PersistentShellSession;
use super::{
    command_routing::{CommandRoute, classify_command},
    history::{CommandRecord, format_history, load_string_history, push_string_history},
    input_syntax::should_read_shell_continuation,
    prompt::{default_prompt, default_shell},
    render::print_command_output,
    terminal::discard_pending_terminal_input,
};
use crate::agent::{Agent, AgentConfig, AgentRunContext, ShellCommandContext};
use crate::commands::{SlashCommand, parse_slash_command};
use crate::common::output::CommandOutput;
use crate::common::{
    cancellation::{CancellationEvent, clear_sigint_request, install_sigint_handler},
    text::{TruncatePosition, truncate_utf8_to_bytes},
};
use crate::input::{CommandInputConfig, read_command_input};
use crate::logging::AppLogger;

#[cfg(not(test))]
use super::history::{default_ask_history_path, default_command_history_path};

const MAX_AGENT_SHELL_CONTEXT_OUTPUT_BYTES: usize = 32 * 1024;
const INTERRUPTED_EXIT_HINT: &str = "Interrupted. Type /exit to exit the shell.";
const SHELL_CONTINUATION_PROMPT: &str = "> ";

#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env_vars: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    pub prompt: String,
    pub agent_config: Option<AgentConfig>,
    pub agent_config_path: Option<PathBuf>,
    pub agent: Option<Agent>,
    pub logger: Option<AppLogger>,
    pub command_history_path: Option<PathBuf>,
    pub ask_history_path: Option<PathBuf>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        let working_dir = env::current_dir().ok();
        let prompt = default_prompt(working_dir.as_ref());

        #[cfg(not(test))]
        let command_history_path = default_command_history_path().ok();
        #[cfg(test)]
        let command_history_path = None;
        #[cfg(not(test))]
        let ask_history_path = default_ask_history_path().ok();
        #[cfg(test)]
        let ask_history_path = None;

        Self {
            executable: default_shell(),
            args: Vec::new(),
            env_vars: vec![("THESEUS_ACTIVE".to_string(), "1".to_string())],
            working_dir,
            prompt,
            agent_config: None,
            agent_config_path: None,
            agent: None,
            logger: None,
            command_history_path,
            ask_history_path,
        }
    }
}

pub struct TheseusShell {
    pub(super) config: ShellConfig,
    pub(super) agent: Option<Agent>,
    pub(super) shell_session: Option<PersistentShellSession>,
    history: Vec<CommandRecord>,
    pub(super) input_history: Vec<String>,
    pub(super) ask_history: Vec<String>,
}

impl TheseusShell {
    pub fn new(config: ShellConfig) -> Self {
        let logger = config.logger.clone();
        let agent = config
            .agent
            .clone()
            .or_else(|| config.agent_config.clone().map(Agent::new))
            .map(|agent| match logger.clone() {
                Some(logger) => agent.with_logger(logger),
                None => agent,
            });

        let input_history = config
            .command_history_path
            .as_ref()
            .and_then(|path| load_string_history(path).ok())
            .unwrap_or_default();
        let ask_history = config
            .ask_history_path
            .as_ref()
            .and_then(|path| load_string_history(path).ok())
            .unwrap_or_default();

        Self {
            config,
            agent,
            shell_session: None,
            history: Vec::new(),
            input_history,
            ask_history,
        }
    }

    pub fn history(&self) -> &[CommandRecord] {
        &self.history
    }

    pub fn run(&mut self) -> io::Result<i32> {
        install_sigint_handler();

        if !self.config.args.is_empty() {
            let command = self.config.args.join(" ");
            let output = self.handle_command(&command)?;
            print_command_output(&output)?;
            return Ok(output.status_code.unwrap_or(1));
        }

        loop {
            let shell_highlight = self
                .config
                .agent_config
                .as_ref()
                .map(|config| &config.shell_settings.shell_highlight);
            let input = match read_command_input(CommandInputConfig {
                prompt: &self.config.prompt,
                continuation_prompt: SHELL_CONTINUATION_PROMPT,
                history: &self.input_history,
                should_continue: should_read_shell_continuation,
                shell_highlight,
            }) {
                Ok(Some(input)) => input,
                Ok(None) => {
                    println!();
                    return Ok(0);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    println!("\n{INTERRUPTED_EXIT_HINT}");
                    clear_sigint_request();
                    continue;
                }
                Err(err) => return Err(err),
            };
            if input.trim().is_empty() {
                continue;
            }

            let trimmed_input = input.trim();
            let input_was_ask =
                matches!(parse_slash_command(trimmed_input), Some(SlashCommand::Ask));
            let output = match self.handle_command(&input) {
                Ok(output) => output,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    println!("\n{INTERRUPTED_EXIT_HINT}");
                    clear_sigint_request();
                    continue;
                }
                Err(err) => return Err(err),
            };
            print_command_output(&output)?;

            if input_was_ask {
                discard_pending_terminal_input()?;
            }

            if is_exit_command(trimmed_input) {
                return Ok(0);
            }
        }
    }

    pub fn handle_command(&mut self, input: &str) -> io::Result<CommandOutput> {
        let mut history_input = Some(input.to_string());
        self.log_event(
            "info",
            "command_start",
            serde_json::json!({
                "input": input,
            }),
        );
        let trimmed = input.trim();
        let output = match parse_slash_command(trimmed) {
            Some(SlashCommand::Exit) => CommandOutput::success(""),
            Some(SlashCommand::History) => CommandOutput::success(format_history(&self.history)),
            Some(SlashCommand::Status) => CommandOutput::success(self.agent_status()),
            Some(SlashCommand::Mcp) => CommandOutput::success(self.mcp_status()),
            Some(SlashCommand::Reset) => self.handle_reset_command()?,
            Some(SlashCommand::Compact) => self.handle_compact_command()?,
            Some(SlashCommand::Resume) => self.handle_resume_command()?,
            Some(SlashCommand::Config) => self.handle_config_command()?,
            Some(SlashCommand::Ask) => self.handle_ask_command(trimmed)?,
            Some(SlashCommand::Shell) => {
                self.handle_shell_editor_command(trimmed, &mut history_input)?
            }
            Some(SlashCommand::Help) => self.handle_help_command(),
            None => match trimmed {
                command if is_exit_command(command) => CommandOutput::success(""),
                command
                    if classify_command(command, self.config.working_dir.as_deref())
                        == CommandRoute::Agent =>
                {
                    self.agent_output(command)
                }
                command
                    if !crate::feature_flags::PERSISTENT_SHELL_SESSION
                        && is_cd_command(command) =>
                {
                    self.change_directory(command)
                }
                command => self.run_external_command(command)?,
            },
        };

        if let Some(history_input) = history_input {
            self.history.push(CommandRecord {
                input: history_input.clone(),
                output: output.clone(),
            });
            self.store_input_history(&history_input);
        }

        self.log_event(
            if output.status_code == Some(0) {
                "info"
            } else {
                "error"
            },
            "command_finish",
            serde_json::json!({
                "input": input,
                "status_code": output.status_code,
                "streamed": output.streamed,
                "transcript_bytes": output.transcript.len(),
            }),
        );

        Ok(output)
    }

    fn store_input_history(&mut self, input: &str) {
        push_string_history(&mut self.input_history, input);
        self.save_input_history();
    }

    pub(super) fn run_agent(&mut self, prompt: &str) -> io::Result<String> {
        let last_shell_command = self.last_shell_command_context();
        let Some(agent) = self.agent.as_mut() else {
            return Ok(format!("{prompt}\n"));
        };
        clear_sigint_request();

        agent.run_with_context(
            prompt,
            AgentRunContext {
                shell: self.config.executable.clone(),
                shell_prompt: self.config.prompt.clone(),
                shell_highlight: self
                    .config
                    .agent_config
                    .as_ref()
                    .map(|config| config.shell_settings.shell_highlight.clone())
                    .unwrap_or_default(),
                env_vars: self.config.env_vars.clone(),
                working_dir: self.config.working_dir.clone(),
                last_shell_command,
                logger: self.config.logger.clone(),
                cancellation: CancellationEvent::new(),
                ..AgentRunContext::default()
            },
        )
    }

    fn last_shell_command_context(&self) -> Option<ShellCommandContext> {
        let record = self.history.last()?;
        let command = record.input.trim();

        if !self.is_shell_context_command(command) {
            return None;
        }

        Some(ShellCommandContext {
            command: command.to_string(),
            output: truncate_utf8_to_bytes(
                &record.output.transcript_lossy(),
                MAX_AGENT_SHELL_CONTEXT_OUTPUT_BYTES,
                TruncatePosition::End,
            ),
        })
    }

    pub(super) fn is_shell_context_command(&self, command: &str) -> bool {
        !is_special_command(command)
            && classify_command(command, self.config.working_dir.as_deref()) == CommandRoute::Shell
    }

    pub(super) fn log_event(&self, level: &str, event: &str, fields: serde_json::Value) {
        if let Some(logger) = &self.config.logger {
            let _ = logger.event(level, event, fields);
        }
    }
}

pub fn run_shell(config: ShellConfig) -> io::Result<i32> {
    TheseusShell::new(config).run()
}

pub(super) fn is_cd_command(command: &str) -> bool {
    matches!(command.split_whitespace().next(), Some("cd"))
        && !contains_shell_control_syntax(command)
}

fn contains_shell_control_syntax(command: &str) -> bool {
    command
        .chars()
        .any(|ch| matches!(ch, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}'))
}

fn is_exit_command(command: &str) -> bool {
    matches!(command, "exit") || matches!(parse_slash_command(command), Some(SlashCommand::Exit))
}

fn is_special_command(command: &str) -> bool {
    command.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::colorize_nested;
    use std::sync::{Mutex, MutexGuard};
    use std::time::{SystemTime, UNIX_EPOCH};

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn cwd_lock() -> MutexGuard<'static, ()> {
        CWD_LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn unique_test_suffix() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    struct CurrentDirGuard {
        original: PathBuf,
    }

    impl CurrentDirGuard {
        fn new() -> Self {
            Self {
                original: env::current_dir().unwrap(),
            }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn default_config_marks_shell_as_active() {
        let config = ShellConfig::default();

        assert!(
            config
                .env_vars
                .iter()
                .any(|(key, value)| key == "THESEUS_ACTIVE" && value == "1")
        );
    }

    #[test]
    fn default_shell_is_not_empty() {
        assert!(!default_shell().as_os_str().is_empty());
    }

    #[test]
    fn default_config_uses_current_dir() {
        let _guard = cwd_lock();
        let config = ShellConfig::default();

        assert_eq!(config.working_dir, env::current_dir().ok());
    }

    #[test]
    fn default_prompt_uses_current_dir_name() {
        let working_dir = PathBuf::from("/tmp/example-project");
        let user_name = env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "user".to_string());

        assert_eq!(
            default_prompt(Some(&working_dir)),
            colorize_nested(&format!(
                "<bold><cyan>{user_name}</cyan></bold> <bold><magenta>example-project</magenta></bold>> "
            ))
        );
    }

    #[test]
    fn stores_external_command_input_and_status() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("true").unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(shell.history()[0].input, "true");
        assert_eq!(shell.history()[0].output.status_code, Some(0));
    }

    #[test]
    fn intercepts_slash_history_command() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        shell.handle_command("true").unwrap();
        let output = shell.handle_command("/history").unwrap();

        // /history is not a registered slash command so it runs as an external command
        assert_ne!(output.status_code, Some(0));
    }

    #[test]
    fn intercepts_status_command() {
        let mut agent_config = AgentConfig::default_empty();
        agent_config.llm_request_settings.base_url = "https://example.test/chat".to_string();
        agent_config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer secret".to_string());
        let mut shell = TheseusShell::new(ShellConfig {
            agent_config: Some(agent_config),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("/status").unwrap();
        let text = output.transcript_lossy();

        assert_eq!(output.status_code, Some(0));
        assert!(!text.contains("Agent status"));
        assert!(text.contains("context tokens"));
        assert!(text.contains("n/a"));
        assert_eq!(shell.history()[0].input, "/status");
    }

    #[test]
    fn intercepts_mcp_command() {
        let mut shell = TheseusShell::new(ShellConfig {
            agent_config: Some(AgentConfig::default_empty()),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("/mcp").unwrap();
        let text = output.transcript_lossy();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(text, "No MCP servers configured.\n");
        assert_eq!(shell.history()[0].input, "/mcp");
    }

    #[test]
    fn reset_reloads_agent_config_from_disk() {
        let path = env::temp_dir().join(format!(
            "theseus-reset-config-{}-{}.jsonc",
            std::process::id(),
            unique_test_suffix()
        ));
        let mut initial_config = AgentConfig::default_empty();
        initial_config
            .llm_request_settings
            .body
            .insert("model".to_string(), serde_json::json!("test/old"));
        initial_config.save_at(&path).unwrap();

        let mut shell = TheseusShell::new(ShellConfig {
            agent_config: Some(initial_config),
            agent_config_path: Some(path.clone()),
            ..ShellConfig::default()
        });

        let mut updated_config = AgentConfig::default_empty();
        updated_config
            .llm_request_settings
            .body
            .insert("model".to_string(), serde_json::json!("test/new"));
        updated_config.save_at(&path).unwrap();

        let output = shell.handle_command("/reset").unwrap();
        let status = shell.agent.as_ref().unwrap().status_text();

        assert_eq!(output.status_code, Some(0));
        assert!(status.contains("| **model** | test/new |"));
        assert_eq!(shell.config.agent_config, Some(updated_config));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ask_command_accepts_single_line_prompt() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("false"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("/ask say hello").unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(output.transcript_lossy(), "say hello\n");
        assert_eq!(shell.history()[0].input, "/ask say hello");
    }

    #[test]
    fn loads_persisted_ask_history() {
        let path = env::temp_dir().join(format!(
            "theseus-ask-history-load-{}-{}.json",
            std::process::id(),
            unique_test_suffix()
        ));
        std::fs::write(&path, "[\"old ask\"]\n").unwrap();

        let shell = TheseusShell::new(ShellConfig {
            ask_history_path: Some(path.clone()),
            ..ShellConfig::default()
        });

        assert_eq!(shell.ask_history, vec!["old ask"]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ask_command_persists_single_line_prompt() {
        let path = env::temp_dir().join(format!(
            "theseus-ask-history-save-{}-{}.json",
            std::process::id(),
            unique_test_suffix()
        ));
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("false"),
            ask_history_path: Some(path.clone()),
            ..ShellConfig::default()
        });

        shell.handle_command("/ask say hello").unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();

        assert_eq!(saved, "[\n  \"say hello\"\n]\n");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ask_command_uses_agent_config_when_available() {
        let mut agent_config = AgentConfig::default_empty();
        agent_config.llm_request_settings.base_url = "https://example.test/chat".to_string();
        agent_config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer ".to_string());
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("false"),
            agent_config: Some(agent_config),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("/ask say hello").unwrap();
        let text = output.transcript_lossy();

        assert!(text.contains("LLM Authorization header is empty"));
    }

    #[test]
    fn shell_command_inline_executes_without_shell_prefix_in_history() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command("/shell printf SHELL_INLINE_OK")
            .unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(
            output.transcript_lossy().replace("\r\n", "\n"),
            "SHELL_INLINE_OK"
        );
        assert_eq!(shell.history()[0].input, "printf SHELL_INLINE_OK");
        assert_eq!(
            shell.input_history,
            vec!["printf SHELL_INLINE_OK".to_string()]
        );
    }

    #[test]
    fn last_shell_command_context_uses_latest_shell_command() {
        let mut shell = TheseusShell::new(ShellConfig::default());
        shell.history.push(CommandRecord {
            input: "ls src".to_string(),
            output: CommandOutput::success("agent\nshell\n"),
        });

        let context = shell.last_shell_command_context().unwrap();

        assert_eq!(context.command, "ls src");
        assert_eq!(context.output, "agent\nshell\n");
    }

    #[test]
    fn last_shell_command_context_skips_latest_special_command() {
        let mut shell = TheseusShell::new(ShellConfig::default());
        shell.history.push(CommandRecord {
            input: "ls src".to_string(),
            output: CommandOutput::success("agent\nshell\n"),
        });
        shell.history.push(CommandRecord {
            input: "/status".to_string(),
            output: CommandOutput::success("status\n"),
        });

        assert_eq!(shell.last_shell_command_context(), None);
    }

    #[test]
    fn last_shell_command_context_skips_agent_natural_language() {
        let mut shell = TheseusShell::new(ShellConfig::default());
        shell.history.push(CommandRecord {
            input: "что умеешь?".to_string(),
            output: CommandOutput::success("answer\n"),
        });

        assert_eq!(shell.last_shell_command_context(), None);
    }

    #[test]
    fn non_english_input_is_agent_prompt() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("false"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("что умеешь?").unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(output.transcript_lossy(), "что умеешь?\n");
    }

    #[test]
    fn shell_command_with_non_english_path_stays_external() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("cat файл.txt").unwrap();

        assert_eq!(shell.history()[0].input, "cat файл.txt");
        assert_ne!(output.status_code, Some(0));
    }

    #[test]
    fn english_agent_prompt_is_routed_to_agent() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("false"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("find biggest file").unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(output.transcript_lossy(), "find biggest file\n");
    }

    #[test]
    fn english_question_prompt_is_routed_to_agent() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("false"),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command("ok, could you find mp3 files in this dir?")
            .unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(
            output.transcript_lossy(),
            "ok, could you find mp3 files in this dir?\n"
        );
    }

    #[test]
    fn english_plain_phrase_with_unknown_command_is_routed_to_agent() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("false"),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command("theseusunknownword files is here?")
            .unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(
            output.transcript_lossy(),
            "theseusunknownword files is here?\n"
        );
    }

    #[test]
    fn english_plain_command_found_in_path_stays_external() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("ls downloads").unwrap();

        assert_ne!(output.status_code, Some(0));
    }

    #[test]
    fn english_shell_command_with_shell_arguments_stays_external() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command(r#"find /definitely-missing-theseus-path -name "*.jpg""#)
            .unwrap();

        assert_eq!(
            shell.history()[0].input,
            r#"find /definitely-missing-theseus-path -name "*.jpg""#
        );
        assert_ne!(output.status_code, Some(0));
    }

    #[test]
    fn english_shell_command_with_question_mark_glob_stays_external() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command(r#"find /definitely-missing-theseus-path -name "file?.mp3""#)
            .unwrap();

        assert_eq!(
            shell.history()[0].input,
            r#"find /definitely-missing-theseus-path -name "file?.mp3""#
        );
        assert_ne!(output.status_code, Some(0));
    }

    #[test]
    fn english_find_with_existing_path_stays_external() {
        let _guard = cwd_lock();
        let current_dir = env::current_dir().unwrap();
        let temp_dir = env::temp_dir();
        env::set_current_dir(&temp_dir).unwrap();
        let existing_dir = temp_dir.join(format!("theseus-shell-find-test-{}", std::process::id()));
        std::fs::create_dir_all(&existing_dir).unwrap();

        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            working_dir: Some(temp_dir.clone()),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command(&format!(
                "find {}",
                existing_dir.file_name().unwrap().to_string_lossy()
            ))
            .unwrap();

        assert!(shell.shell_session.is_some());
        assert_eq!(output.status_code, Some(0));
        std::fs::remove_dir_all(existing_dir).unwrap();
        env::set_current_dir(current_dir).unwrap();
    }

    #[test]
    fn plain_history_is_external_command() {
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command("history").unwrap();

        assert_eq!(shell.history()[0].input, "history");
        assert!(shell.shell_session.is_some());
        assert_eq!(output.status_code, Some(0));
    }

    #[test]
    fn detects_cd_command() {
        assert!(is_cd_command("cd /tmp"));
        assert!(is_cd_command("cd"));
        assert!(!is_cd_command("cdd /tmp"));
        assert!(!is_cd_command(r#"cd ~ && echo "test""#));
        assert!(!is_cd_command("cd /tmp; pwd"));
    }

    #[test]
    fn detects_exit_command() {
        assert!(is_exit_command("exit"));
        assert!(is_exit_command("/exit"));
        assert!(!is_exit_command("/exit now"));
    }

    #[test]
    fn cd_updates_prompt() {
        let _guard = cwd_lock();
        let current_dir = env::current_dir().unwrap();
        let temp_dir = env::temp_dir();
        let mut shell = TheseusShell::new(ShellConfig::default());

        shell
            .handle_command(&format!("cd {}", temp_dir.display()))
            .unwrap();

        assert_eq!(shell.config.prompt, default_prompt(Some(&temp_dir)));
        env::set_current_dir(current_dir).unwrap();
    }

    #[test]
    fn compound_cd_command_runs_in_shell() {
        let _guard = cwd_lock();
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell.handle_command(r#"cd ~ && echo "test""#).unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(output.transcript_lossy().replace("\r\n", "\n"), "test\n");
    }

    #[test]
    fn compound_cd_command_syncs_working_dir_after_output() {
        let _guard = cwd_lock();
        let _cwd = CurrentDirGuard::new();
        let temp_dir = env::temp_dir();
        let expected_dir = temp_dir.canonicalize().unwrap_or_else(|_| temp_dir.clone());
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command(&format!(r#"echo "test" && cd "{}""#, temp_dir.display()))
            .unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(output.transcript_lossy().replace("\r\n", "\n"), "test\n");
        assert_eq!(
            shell
                .config
                .working_dir
                .as_ref()
                .and_then(|path| path.canonicalize().ok())
                .as_deref(),
            Some(expected_dir.as_path())
        );
        assert_eq!(
            shell.config.prompt,
            default_prompt(shell.config.working_dir.as_ref())
        );
        assert_eq!(env::current_dir().unwrap(), expected_dir);
    }

    #[test]
    fn builtin_cd_syncs_existing_persistent_shell_session() {
        let _guard = cwd_lock();
        let _cwd = CurrentDirGuard::new();
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME must be set for cd ~ test");
        let expected_home = home.canonicalize().unwrap_or(home);
        let temp_dir = env::temp_dir();
        let mut shell = TheseusShell::new(ShellConfig {
            executable: PathBuf::from("/bin/sh"),
            ..ShellConfig::default()
        });

        let output = shell
            .handle_command(&format!(r#"echo "test" && cd "{}""#, temp_dir.display()))
            .unwrap();
        assert_eq!(output.status_code, Some(0));
        assert_eq!(output.transcript_lossy().replace("\r\n", "\n"), "test\n");

        let output = shell.handle_command("cd ~").unwrap();
        assert_eq!(output.status_code, Some(0));
        assert_eq!(
            shell
                .config
                .working_dir
                .as_ref()
                .and_then(|path| path.canonicalize().ok())
                .as_deref(),
            Some(expected_home.as_path())
        );

        let output = shell.handle_command("pwd").unwrap();
        let pwd = PathBuf::from(output.transcript_lossy().trim_end_matches(['\r', '\n']));

        assert_eq!(output.status_code, Some(0));
        assert_eq!(
            pwd.canonicalize().ok().as_deref(),
            Some(expected_home.as_path())
        );
    }
}
