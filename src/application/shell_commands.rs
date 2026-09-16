//! Shell execution through the terminal lease and adoption of its output.
use super::{
    Application, CommandRecord, ShellPresentation,
    ansi::{ansi_render_lines, primary_screen_output, suffix_after_last_display_clear},
    terminal,
};
use crate::{
    agent::ShellCommandContext,
    common,
    shell::pty::{PersistentShellConfig, PersistentShellSession},
};
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
};

const MAX_AGENT_SHELL_CONTEXT_OUTPUT_BYTES: usize = 32 * 1024;

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

impl Application {
    pub(super) fn ensure_shell_session(&mut self) -> io::Result<()> {
        if self.shell_session.is_none() {
            self.shell_session = Some(PersistentShellSession::start(PersistentShellConfig {
                shell: self.shell_path.clone(),
                env_vars: self.shell_env.clone(),
                working_dir: self.working_dir.clone(),
            })?);
        }
        Ok(())
    }

    pub(super) fn run_shell(&mut self, command: &str) -> io::Result<()> {
        if self.terminal.is_some() {
            self.pending_shell = Some(command.into());
            return Ok(());
        }
        self.run_shell_now(command).map(|_| ())
    }

    pub(super) fn run_shell_now(&mut self, command: &str) -> io::Result<Option<ShellPresentation>> {
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
}

pub(super) fn default_shell_path() -> PathBuf {
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

pub(super) fn shell_prompt(working_dir: Option<&Path>) -> String {
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
