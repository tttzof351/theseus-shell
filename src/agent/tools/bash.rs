use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
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
        terminal_output,
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
        if let Some(output) = &context.output {
            output.message(
                crate::common::events::BlockKind::ToolPreview,
                &format_bash_command_preview(command, context),
            )?;
        } else {
            terminal_output::with_stdout(|stdout| {
                write!(stdout, "{}", format_bash_command_preview(command, context))?;
                stdout.flush()
            })?;
        }

        let output = run_agent_shell_command(command, context)?;
        let truncated = output.command_output.transcript_lossy();

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

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child_command.process_group(0);
    }
    if context.cancellation.cancel_if_interrupted() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted by user",
        ));
    }
    let output_block = context
        .output
        .as_ref()
        .map(|output| output.start_block(crate::common::events::BlockKind::ToolOutput))
        .transpose()?;
    let (log_path, log_file) = create_bash_log_file(context)?;
    let log_file = Arc::new(Mutex::new(BashStreamLog {
        file: log_file,
        path: log_path.clone(),
        displayed: 0,
        truncation_reported: false,
    }));
    let mut child = CommandProcess {
        child: child_command.spawn()?,
        readers: Vec::new(),
    };
    let stdout = child.child.stdout.take();
    let stderr = child.child.stderr.take();
    let event_output = context.output.clone().zip(output_block);

    for (stream, reader) in [
        stdout.map(|reader| Box::new(reader) as Box<dyn Read + Send>),
        stderr.map(|reader| Box::new(reader) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(reader) = reader {
            child.readers.push(read_command_stream(
                reader,
                Arc::clone(&log_file),
                event_output.clone(),
                context.cancellation.clone(),
                stream as u8,
            ));
        }
    }

    let mut exit_status = None;
    let status_code = loop {
        if context.cancellation.cancel_if_interrupted() {
            child.terminate();
            let _ = child.child.wait();
            child.join_readers()?;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "interrupted by user",
            ));
        }

        if exit_status.is_none() {
            exit_status = child.child.try_wait()?;
        }
        // A shell can exit while its descendants still hold the pipes. Keep
        // servicing cancellation until both readers have actually reached EOF.
        if let Some(status) = exit_status
            && child.readers.iter().all(thread::JoinHandle::is_finished)
        {
            break status.code();
        }

        thread::sleep(Duration::from_millis(50));
    };

    child.join_readers()?;
    if let Some((output, block)) = event_output {
        output.finish_block(block, crate::common::events::Outcome::Completed)?;
    }

    let transcript = read_log_preview(&log_path, context.max_tool_bash_bytes)?.into_bytes();

    Ok(BashCommandOutput {
        command_output: CommandOutput::streamed(transcript, status_code),
        log_path,
    })
}

/// Own the command's process group even on an early error or unwinding path.
struct CommandProcess {
    child: std::process::Child,
    readers: Vec<thread::JoinHandle<io::Result<()>>>,
}

impl CommandProcess {
    fn join_readers(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        for handle in self.readers.drain(..) {
            let reader = handle
                .join()
                .map_err(|_| io::Error::other("command reader thread panicked"))
                .and_then(|result| result);
            if result.is_ok() {
                result = reader;
            }
        }
        result
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        unsafe {
            // This group was created for this command by process_group(0).
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = self.child.kill();
    }
}

impl Drop for CommandProcess {
    fn drop(&mut self) {
        // On normal completion all readers and the child have finished already.
        if self.child.try_wait().ok().flatten().is_none()
            || self.readers.iter().any(|reader| !reader.is_finished())
        {
            self.terminate();
            let _ = self.child.wait();
        }
        let _ = self.join_readers();
    }
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

const MAX_TOOL_PREVIEW_BYTES: usize = 256 * 1024;

struct BashStreamLog {
    file: fs::File,
    path: PathBuf,
    displayed: usize,
    truncation_reported: bool,
}

/// Read a bounded head/tail from the full spool instead of retaining the full
/// tool transcript in memory and truncating it only after the process exits.
fn read_log_preview(path: &std::path::Path, budget: usize) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length <= budget as u64 {
        let mut bytes = Vec::with_capacity(length as usize);
        file.read_to_end(&mut bytes)?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let mut prefix = vec![0; budget / 2];
    file.read_exact(&mut prefix)?;
    // An incomplete scalar at the cut belongs to the omitted middle.
    if let Err(err) = std::str::from_utf8(&prefix)
        && err.error_len().is_none()
    {
        prefix.truncate(err.valid_up_to());
    }
    let suffix_budget = budget - budget / 2;
    file.seek(SeekFrom::End(-(suffix_budget as i64)))?;
    let mut suffix = vec![0; suffix_budget];
    file.read_exact(&mut suffix)?;
    let cut = suffix
        .iter()
        .take_while(|byte| **byte & 0xc0 == 0x80)
        .count();
    let suffix = &suffix[cut..];
    let removed = length - prefix.len() as u64 - suffix.len() as u64;
    if budget == 0 {
        return Ok(format!("[truncated {removed} bytes]"));
    }
    Ok(format!(
        "{}\n[truncated {removed} bytes]\n{}",
        String::from_utf8_lossy(&prefix),
        String::from_utf8_lossy(suffix)
    ))
}

fn read_command_stream<R>(
    mut reader: R,
    log_file: Arc<Mutex<BashStreamLog>>,
    output: Option<(
        crate::common::events::EventSink,
        crate::common::events::BlockId,
    )>,
    cancellation: crate::common::cancellation::CancellationEvent,
    stream: u8,
) -> thread::JoinHandle<io::Result<()>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let result = crate::common::panic_boundary::catch(|| -> io::Result<()> {
            let mut buffer = [0; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        let mut log = log_file
                            .lock()
                            .map_err(|_| io::Error::other("tool spool poisoned"))?;
                        log.file.write_all(&buffer[..n])?;
                        log.file.flush()?;
                        if let Some((output, block)) = &output {
                            // Spool and UI share the observed order of both pipes.
                            let visible =
                                n.min(MAX_TOOL_PREVIEW_BYTES.saturating_sub(log.displayed));
                            if visible > 0 {
                                output.bytes(*block, stream, &buffer[..visible])?;
                            }
                            log.displayed += visible;
                            if visible < n && !log.truncation_reported {
                                log.truncation_reported = true;
                                output.message(
                                    crate::common::events::BlockKind::Diagnostic,
                                    &format!(
                                        "[tool preview limited to {} KiB; full output: {}]",
                                        MAX_TOOL_PREVIEW_BYTES / 1024,
                                        log.path.display()
                                    ),
                                )?;
                            }
                        } else {
                            terminal_output::with_stdout(|stdout| {
                                stdout.write_all(&buffer[..n])?;
                                stdout.flush()
                            })?;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => return Err(err),
                }
            }
        })
        .unwrap_or_else(|message| {
            Err(io::Error::other(format!(
                "command reader panicked: {message}"
            )))
        });
        if result.is_err() {
            cancellation.cancel();
        }
        result
    })
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

    #[cfg(unix)]
    #[test]
    fn command_scope_reaps_child_and_joins_reader_when_unwinding() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec sleep 30"])
            .process_group(0)
            .stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let pid = child.id() as i32;
        let mut stdout = child.stdout.take().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.read_to_end(&mut bytes).map(|_| ());
            done_tx.send(()).unwrap();
            result
        });
        let started = std::time::Instant::now();
        let result = crate::common::panic_boundary::catch(|| {
            let _scope = CommandProcess {
                child,
                readers: vec![reader],
            };
            panic!("unwind tool scope");
        });
        assert!(result.is_err());
        done_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

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
    fn cancellation_kills_descendant_holding_pipe_after_shell_exits() {
        let mut context = test_context();
        context.shell = "/bin/sh".into();
        let cancellation = context.cancellation.clone();
        let (output, events) = crate::common::events::EventSink::channel(cancellation.clone());
        context.output = Some(output);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let result =
                run_agent_shell_command("sleep 30 & printf 'descendant:%s\\n' $!", &context);
            let _ = done_tx.send(result);
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut text = String::new();
        while !text.contains('\n') && std::time::Instant::now() < deadline {
            if let Ok(event) = events.recv_timeout(Duration::from_millis(50))
                && let crate::common::events::OutputEvent::BytesAppended { bytes, .. } = event.event
            {
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        cancellation.cancel();
        let result = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("descendant kept reader alive");
        worker.join().unwrap();
        assert!(matches!(result, Err(err) if err.kind() == io::ErrorKind::Interrupted));
        assert!(
            text.starts_with("descendant:"),
            "child did not start: {text:?}"
        );
        // Completion proves both pipe readers reached EOF, not merely that the
        // parent shell exited (which happened before cancellation).
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

    #[cfg(unix)]
    #[test]
    fn large_tool_output_has_bounded_events_and_complete_spool() {
        let mut context = test_context();
        context.shell = "/bin/sh".into();
        context.max_tool_bash_bytes = 64;
        let (sink, events) =
            crate::common::events::EventSink::channel(context.cancellation.clone());
        context.output = Some(sink);
        let worker = thread::spawn(move || {
            run_agent_shell_command("head -c 524288 /dev/zero | tr '\\000' x", &context)
        });
        let mut visible_bytes = 0;
        let mut diagnostic = String::new();
        for event in events {
            match event.event {
                crate::common::events::OutputEvent::BytesAppended { bytes, .. } => {
                    visible_bytes += bytes.len()
                }
                crate::common::events::OutputEvent::TextAppended { text, .. } => {
                    diagnostic.push_str(&text)
                }
                _ => {}
            }
        }
        let result = worker.join().unwrap().unwrap();
        assert_eq!(visible_bytes, MAX_TOOL_PREVIEW_BYTES);
        assert!(diagnostic.contains("full output:"));
        assert_eq!(fs::metadata(&result.log_path).unwrap().len(), 524288);
        assert!(result.command_output.transcript.len() < 128);
        assert!(
            result
                .command_output
                .transcript_lossy()
                .contains("[truncated 524224 bytes]")
        );
        fs::remove_file(result.log_path).unwrap();
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
