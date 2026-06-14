use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use serde_json::{Value, json};

use super::{AgentTool, ToolOutput, args::string_arg};
use crate::{
    agent::AgentRunContext,
    common::{
        output::CommandOutput,
        system_tools::{SearchToolAvailability, search_tool_availability},
        text::{TruncatePosition, truncate_utf8_to_bytes},
        tmp_files::create_tmp_log_file,
    },
    input::{DEFAULT_SHELL_PROMPT_CONTINUATION_PREFIX, highlight_shell_command_with_palette},
};

pub(super) struct BashTool;

impl AgentTool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn schema(&self) -> Value {
        let description = bash_description(search_tool_availability());

        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Command to execute with the system shell." }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        })
    }

    fn execute(&self, arguments: &Value, context: &AgentRunContext) -> io::Result<ToolOutput> {
        let command = string_arg(arguments, "command")?;
        print!("{}", format_bash_command_preview(command, context));
        io::stdout().flush()?;

        let output = run_agent_shell_command(command, context)?;
        let truncated = truncate_utf8_to_bytes(
            &output.command_output.transcript_lossy(),
            context.max_tool_bash_bytes,
            TruncatePosition::Middle,
        );

        Ok(ToolOutput::text(format!(
            "status: {}\nCommand output log: {}\nOutput:\n{}",
            output.command_output.status_code.unwrap_or(1),
            output.log_path.display(),
            truncated
        )))
    }
}

fn format_bash_command_preview(command: &str, context: &AgentRunContext) -> String {
    let highlighted = highlight_shell_command_with_palette(command, &context.shell_highlight);
    let mut result = String::new();
    for (index, line) in highlighted.iter().enumerate() {
        if index == 0 {
            result.push_str(&context.shell_prompt);
        } else {
            result.push_str(DEFAULT_SHELL_PROMPT_CONTINUATION_PREFIX);
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

fn bash_description(availability: SearchToolAvailability) -> String {
    let mut description =
        "Run a shell command and return stdout, stderr, and exit status.".to_string();

    match (availability.rg, availability.jq) {
        (true, true) => description.push_str(
            " ripgrep (rg) is available for fast text/file search; jq is available for JSON filtering and transformation.",
        ),
        (true, false) => description
            .push_str(" ripgrep (rg) is available for fast text/file search."),
        (false, true) => {
            description.push_str(" jq is available for JSON filtering and transformation.");
        }
        (false, false) => {}
    }

    description
}

struct BashCommandOutput {
    command_output: CommandOutput,
    log_path: PathBuf,
}

fn run_agent_shell_command(
    command: &str,
    context: &AgentRunContext,
) -> io::Result<BashCommandOutput> {
    let mut child_command = Command::new(&context.shell);
    child_command
        .args(shell_command_args(command))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for (key, value) in &context.env_vars {
        child_command.env(key, value);
    }

    child_command
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("LESS", "FRX");

    if let Some(working_dir) = &context.working_dir {
        child_command.current_dir(working_dir);
    }

    let (log_path, log_file) = create_bash_log_file(context)?;
    let log_file = Arc::new(Mutex::new(log_file));
    let mut child = child_command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let transcript = Arc::new(Mutex::new(Vec::new()));

    let stdout_thread = stdout
        .map(|stdout| read_command_stream(stdout, Arc::clone(&transcript), Arc::clone(&log_file)));
    let stderr_thread = stderr
        .map(|stderr| read_command_stream(stderr, Arc::clone(&transcript), Arc::clone(&log_file)));

    let status_code = loop {
        if context.cancellation.cancel_if_interrupted() {
            let _ = child.kill();
            let _ = child.wait();
            join_reader(stdout_thread)?;
            join_reader(stderr_thread)?;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "interrupted by user",
            ));
        }

        if let Some(status) = child.try_wait()? {
            break status.code();
        }

        thread::sleep(Duration::from_millis(50));
    };

    join_reader(stdout_thread)?;
    join_reader(stderr_thread)?;

    let transcript = Arc::try_unwrap(transcript)
        .map_err(|_| io::Error::other("command transcript is still shared"))?
        .into_inner()
        .map_err(|_| io::Error::other("command transcript lock poisoned"))?;

    Ok(BashCommandOutput {
        command_output: CommandOutput::streamed(transcript, status_code),
        log_path,
    })
}

fn create_bash_log_file(context: &AgentRunContext) -> io::Result<(PathBuf, fs::File)> {
    #[cfg(test)]
    if let Some(tmp_dir) = context.tmp_dir.as_deref() {
        return crate::common::tmp_files::create_tmp_log_file_in(tmp_dir);
    }
    #[cfg(not(test))]
    let _ = context;

    create_tmp_log_file()
}

fn read_command_stream<R>(
    mut reader: R,
    transcript: Arc<Mutex<Vec<u8>>>,
    log_file: Arc<Mutex<fs::File>>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        let mut stdout = io::stdout();

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut transcript) = transcript.lock() {
                        transcript.extend_from_slice(&buffer[..n]);
                    }
                    if let Ok(mut log_file) = log_file.lock() {
                        let _ = log_file.write_all(&buffer[..n]);
                        let _ = log_file.flush();
                    }
                    let _ = stdout.write_all(&buffer[..n]);
                    let _ = stdout.flush();
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    })
}

fn join_reader(handle: Option<thread::JoinHandle<()>>) -> io::Result<()> {
    if let Some(handle) = handle {
        handle
            .join()
            .map_err(|_| io::Error::other("command reader thread panicked"))?;
    }

    Ok(())
}

#[cfg(unix)]
fn shell_command_args(command: &str) -> [&str; 2] {
    ["-c", command]
}

#[cfg(windows)]
fn shell_command_args(command: &str) -> [&str; 2] {
    ["/C", command]
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    #[test]
    fn bash_tool_captures_output() {
        let context = test_context();
        let output = BashTool
            .execute(&json!({ "command": "true" }), &context)
            .unwrap();

        assert!(output.text.contains("status: 0"));
        assert!(output.text.contains("Command output log: "));
        assert!(output.text.contains("Output:\n"));
        remove_log_file(&output.text);
    }

    #[cfg(unix)]
    #[test]
    fn bash_tool_runs_without_interactive_terminal() {
        let context = test_context();
        let output = BashTool
            .execute(
                &json!({ "command": "printf '%s' \"$TERM:$PAGER:$GIT_PAGER\"; read value || printf ':no-stdin'" }),
                &context,
            )
            .unwrap();

        assert!(output.text.contains("status: 0"));
        assert!(output.text.contains("dumb:cat:cat:no-stdin"));
        remove_log_file(&output.text);
    }

    #[test]
    fn bash_description_mentions_available_search_tools() {
        let description = bash_description(SearchToolAvailability { rg: true, jq: true });

        assert!(description.contains("ripgrep (rg)"));
        assert!(description.contains("jq"));
    }

    #[test]
    fn bash_description_omits_missing_search_tools() {
        let description = bash_description(SearchToolAvailability {
            rg: false,
            jq: true,
        });

        assert!(!description.contains("ripgrep"));
        assert!(description.contains("jq"));
    }

    #[test]
    fn bash_output_includes_log_path_and_truncated_preview() {
        let context = AgentRunContext {
            max_tool_bash_bytes: 8,
            tmp_dir: Some(test_tmp_dir()),
            ..Default::default()
        };
        let output = BashTool
            .execute(
                &json!({
                    "command": "printf 'abcdefghijABCDEFGHIJ'"
                }),
                &context,
            )
            .unwrap();

        assert!(output.text.contains("status: 0\n"));
        assert!(output.text.contains("Command output log: "));
        assert!(output.text.contains("Output:\nabcd"));
        assert!(output.text.contains("[truncated 12 bytes]"));
        assert!(output.text.contains("GHIJ"));

        let log_path = output
            .text
            .lines()
            .find_map(|line| line.strip_prefix("Command output log: "))
            .unwrap();
        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "abcdefghijABCDEFGHIJ"
        );
        let _ = fs::remove_file(log_path);
    }

    #[test]
    fn bash_command_preview_uses_shell_highlighting() {
        let mut context = AgentRunContext {
            shell_prompt: "euclid theseus-shell> ".to_string(),
            ..Default::default()
        };
        context.shell_highlight.insert(
            "operator".to_string(),
            Some(crate::input::ShellHighlightStyle::single("yellow")),
        );

        let preview =
            format_bash_command_preview("find . -type f 2>/dev/null | grep -v total", &context);

        assert_eq!(
            crate::input::strip_ansi_codes(&preview),
            "euclid theseus-shell> find . -type f 2>/dev/null | grep -v total\n"
        );
        assert!(preview.contains("\x1b[33m|\x1b[0m"));
    }

    #[test]
    fn bash_command_preview_uses_shell_prompt_continuation_prefix_for_multiline_command() {
        let context = AgentRunContext {
            shell_prompt: "euclid theseus-shell> ".to_string(),
            ..Default::default()
        };

        let preview = format_bash_command_preview("echo one\necho two", &context);

        assert_eq!(
            crate::input::strip_ansi_codes(&preview),
            format!(
                "euclid theseus-shell> echo one\n{DEFAULT_SHELL_PROMPT_CONTINUATION_PREFIX}echo two\n"
            )
        );
    }

    fn test_context() -> AgentRunContext {
        AgentRunContext {
            tmp_dir: Some(test_tmp_dir()),
            ..Default::default()
        }
    }

    fn test_tmp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("theseus-bash-test-{}", std::process::id()))
    }

    fn remove_log_file(output: &str) {
        if let Some(log_path) = output
            .lines()
            .find_map(|line| line.strip_prefix("Command output log: "))
        {
            let _ = fs::remove_file(log_path);
        }
    }
}
