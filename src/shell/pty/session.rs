use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use portable_pty::{Child, CommandBuilder, MasterPty, native_pty_system};

#[cfg(unix)]
use std::fs::OpenOptions;

#[cfg(unix)]
use super::platform::NonBlockingFileGuard;
use super::platform::{RawModeGuard, current_pty_size, interactive_shell_args};
use crate::common::{output::CommandOutput, terminal_output};

#[cfg(unix)]
use signal_hook::{
    consts::signal::SIGWINCH,
    iterator::{Handle as SignalHandle, Signals},
};

const POST_SENTINEL_DRAIN_TIMEOUT: Duration = Duration::from_millis(25);
const STREAM_HOLD_BACK_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub struct PersistentShellConfig {
    pub shell: PathBuf,
    pub env_vars: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
}

pub struct PersistentShellSession {
    nonce: String,
    uses_zsh_protocol: bool,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    event_rx: mpsc::Receiver<ShellEvent>,
    reader_thread: Option<thread::JoinHandle<()>>,
    #[cfg(unix)]
    resize_signal_handle: Option<SignalHandle>,
    #[cfg(unix)]
    resize_thread: Option<thread::JoinHandle<()>>,
}

impl PersistentShellSession {
    pub fn start(config: PersistentShellConfig) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(current_pty_size())
            .map_err(|err| io::Error::other(err.to_string()))?;

        let shell = config.shell;
        let uses_zsh_protocol = is_zsh_shell(&shell);
        let mut command = CommandBuilder::new(&shell);
        command.args(interactive_shell_args(&shell));

        for (key, value) in config.env_vars {
            command.env(key, value);
        }

        if let Some(working_dir) = config.working_dir {
            command.cwd(working_dir);
        }

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|err| io::Error::other(err.to_string()))?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let (event_tx, event_rx) = mpsc::channel();
        let reader_thread = spawn_reader_thread(reader, event_tx.clone());
        #[cfg(unix)]
        let (resize_signal_handle, resize_thread) = spawn_resize_thread(event_tx)?;

        let mut session = Self {
            nonce: new_nonce(),
            uses_zsh_protocol,
            child,
            master: pair.master,
            writer: Arc::new(Mutex::new(writer)),
            event_rx,
            reader_thread: Some(reader_thread),
            #[cfg(unix)]
            resize_signal_handle: Some(resize_signal_handle),
            #[cfg(unix)]
            resize_thread: Some(resize_thread),
        };
        session.initialize_shell()?;

        Ok(session)
    }

    pub fn run_command(&mut self, command: &str) -> io::Result<CommandOutput> {
        self.ensure_shell_is_running()?;
        self.resize_to_current_terminal()?;

        let payload = self.command_payload(command);
        self.write_to_shell(payload.as_bytes())?;

        let stream_output = io::stdout().is_terminal();
        let _raw_mode = RawModeGuard::enable_if_terminal()?;
        let stop_input = Arc::new(AtomicBool::new(false));
        let input_thread =
            spawn_input_forwarder(Arc::clone(&self.writer), Arc::clone(&stop_input))?;
        let completed = self.read_until_sentinel(&payload, stream_output);
        stop_input.store(true, Ordering::Relaxed);
        let _ = input_thread.map(|thread| thread.join());
        let completed = completed?;

        Ok(CommandOutput {
            transcript: completed.transcript,
            status_code: Some(completed.status_code),
            streamed: stream_output,
        })
    }

    pub fn current_working_dir(&mut self) -> io::Result<PathBuf> {
        let output = self.run_internal_command("pwd")?;
        let cwd = output
            .transcript_lossy()
            .trim_end_matches(['\r', '\n'])
            .to_string();

        if cwd.is_empty() {
            return Err(io::Error::other("persistent shell returned empty cwd"));
        }

        Ok(PathBuf::from(cwd))
    }

    fn ensure_shell_is_running(&mut self) -> io::Result<()> {
        if let Some(status) = self.child.try_wait()? {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("shell exited with status {}", status.exit_code()),
            ));
        }

        Ok(())
    }

    fn resize_to_current_terminal(&mut self) -> io::Result<()> {
        self.master
            .resize(current_pty_size())
            .map_err(|err| io::Error::other(err.to_string()))
    }

    fn run_internal_command(&mut self, command: &str) -> io::Result<CommandOutput> {
        self.ensure_shell_is_running()?;
        self.resize_to_current_terminal()?;
        let payload = self.command_payload(command);
        self.write_to_shell(payload.as_bytes())?;
        let completed = self.read_until_sentinel(&payload, false)?;

        Ok(CommandOutput {
            transcript: completed.transcript,
            status_code: Some(completed.status_code),
            streamed: false,
        })
    }

    fn initialize_shell(&mut self) -> io::Result<()> {
        let payload = self.command_payload(
            "stty -echo 2>/dev/null || true\n\
             unsetopt zle prompt_cr prompt_sp 2>/dev/null || true\n\
             PROMPT=''\n\
             RPROMPT=''\n\
             PS1=''\n\
             PS2=''",
        );
        self.write_to_shell(payload.as_bytes())?;
        let _ = self.read_until_sentinel(&payload, false)?;
        self.drain_pending_output();

        Ok(())
    }

    fn write_to_shell(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("persistent shell writer lock poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn command_payload(&self, command: &str) -> String {
        let payload = shell_group_payload(command, &self.nonce, self.uses_zsh_protocol);

        payload.replace('\n', "\r")
    }

    fn read_until_sentinel(
        &mut self,
        payload: &str,
        stream_output: bool,
    ) -> io::Result<CompletedCommand> {
        if stream_output {
            return self.read_until_sentinel_streaming(payload);
        }

        let mut pending = Vec::new();
        let mut transcript = Vec::new();

        loop {
            let chunk = self.recv_shell_chunk()?;
            if chunk.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "shell ended before command sentinel",
                ));
            }

            pending.extend_from_slice(&chunk);
            if let Some(mut completed) = parse_completed_command(&pending, &self.nonce) {
                strip_echoed_payload(&mut completed.transcript, payload);
                transcript.extend_from_slice(&completed.transcript);
                completed.transcript = transcript;
                self.drain_pending_output();
                return Ok(completed);
            }

            strip_echoed_payload_prefix(&mut pending, payload);
            if is_partial_echoed_payload_prefix(&pending, payload) {
                continue;
            }
            if pending.len() > STREAM_HOLD_BACK_BYTES {
                let safe_len = pending.len() - STREAM_HOLD_BACK_BYTES;
                transcript.extend_from_slice(&pending[..safe_len]);
                pending.drain(..safe_len);
            }
        }
    }

    fn read_until_sentinel_streaming(&mut self, payload: &str) -> io::Result<CompletedCommand> {
        let mut pending = Vec::new();
        let mut transcript = Vec::new();
        loop {
            let chunk = self.recv_shell_chunk()?;
            if chunk.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "shell ended before command sentinel",
                ));
            }

            pending.extend_from_slice(&chunk);
            if let Some(mut completed) = parse_completed_command(&pending, &self.nonce) {
                strip_echoed_payload(&mut completed.transcript, payload);
                let needs_prompt_separator =
                    output_ends_with_unfinished_visible_line(&transcript, &completed.transcript);
                terminal_output::with_stdout(|stdout| {
                    stdout.write_all(&completed.transcript)?;
                    if needs_prompt_separator {
                        stdout.write_all(b"\r\n")?;
                    }
                    stdout.flush()
                })?;
                transcript.extend_from_slice(&completed.transcript);
                completed.transcript = transcript;
                self.drain_pending_output();
                return Ok(completed);
            }

            strip_echoed_payload_prefix(&mut pending, payload);
            if is_partial_echoed_payload_prefix(&pending, payload) {
                continue;
            }
            let safe_len = streamable_prefix_len(&pending, &self.nonce);
            if safe_len > 0 {
                terminal_output::with_stdout(|stdout| {
                    stdout.write_all(&pending[..safe_len])?;
                    stdout.flush()
                })?;
                transcript.extend_from_slice(&pending[..safe_len]);
                pending.drain(..safe_len);
            }
        }
    }

    fn recv_shell_chunk(&mut self) -> io::Result<Vec<u8>> {
        loop {
            match self.event_rx.recv() {
                Ok(event) => {
                    if let Some(chunk) =
                        handle_shell_event(event, || self.resize_to_current_terminal())?
                    {
                        return Ok(chunk);
                    }
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "shell reader ended before command sentinel",
                    ));
                }
            }
        }
    }

    fn drain_pending_output(&mut self) {
        while let Ok(event) = self.event_rx.recv_timeout(POST_SENTINEL_DRAIN_TIMEOUT) {
            let _ = handle_shell_event(event, || self.resize_to_current_terminal());
        }
    }
}

impl Drop for PersistentShellSession {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(handle) = self.resize_signal_handle.take() {
            // A final SIGWINCH may be dropped while the resize watcher exits; the next
            // command resizes the PTY before writing user input.
            handle.close();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }
        #[cfg(unix)]
        if let Some(resize_thread) = self.resize_thread.take() {
            let _ = resize_thread.join();
        }
    }
}

enum ShellEvent {
    Chunk(io::Result<Vec<u8>>),
    Resize,
}

#[cfg(test)]
fn recv_shell_chunk_from(
    rx: &mpsc::Receiver<ShellEvent>,
    mut resize: impl FnMut() -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    loop {
        match rx.recv() {
            Ok(event) => {
                if let Some(chunk) = handle_shell_event(event, &mut resize)? {
                    return Ok(chunk);
                }
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "shell reader ended before command sentinel",
                ));
            }
        }
    }
}

fn handle_shell_event(
    event: ShellEvent,
    mut resize: impl FnMut() -> io::Result<()>,
) -> io::Result<Option<Vec<u8>>> {
    match event {
        ShellEvent::Chunk(chunk) => chunk.map(Some),
        ShellEvent::Resize => {
            // A resize failure means the active PTY is no longer in a trustworthy
            // state, so fail the current command instead of letting a TUI keep
            // rendering into stale geometry.
            resize()?;
            Ok(None)
        }
    }
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<ShellEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0; 8192];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(ShellEvent::Chunk(Ok(Vec::new())));
                    break;
                }
                Ok(n) => {
                    if tx
                        .send(ShellEvent::Chunk(Ok(buffer[..n].to_vec())))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    let _ = tx.send(ShellEvent::Chunk(Err(err)));
                    break;
                }
            }
        }
    })
}

#[cfg(unix)]
fn spawn_resize_thread(
    tx: mpsc::Sender<ShellEvent>,
) -> io::Result<(SignalHandle, thread::JoinHandle<()>)> {
    let mut signals = Signals::new([SIGWINCH])?;
    let handle = signals.handle();
    let thread = thread::spawn(move || {
        for _ in &mut signals {
            if tx.send(ShellEvent::Resize).is_err() {
                break;
            }
        }
    });

    Ok((handle, thread))
}

#[cfg(unix)]
fn spawn_input_forwarder(
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    stop: Arc<AtomicBool>,
) -> io::Result<Option<thread::JoinHandle<()>>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }

    let tty = OpenOptions::new().read(true).open("/dev/tty")?;
    let mut tty = NonBlockingFileGuard::enable(tty)?;

    Ok(Some(thread::spawn(move || {
        let mut buffer = [0; 8192];

        while !stop.load(Ordering::Relaxed) {
            match tty.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let Ok(mut writer) = writer.lock() else {
                        break;
                    };
                    if writer.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    })))
}

#[cfg(not(unix))]
fn spawn_input_forwarder(
    _writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _stop: Arc<AtomicBool>,
) -> io::Result<Option<thread::JoinHandle<()>>> {
    Ok(None)
}

struct CompletedCommand {
    transcript: Vec<u8>,
    status_code: i32,
}

fn parse_completed_command(bytes: &[u8], nonce: &str) -> Option<CompletedCommand> {
    let marker = sentinel_marker(nonce);
    let marker = marker.as_slice();
    let mut search_from = 0;

    while search_from < bytes.len() {
        let marker_start = find_subslice(&bytes[search_from..], marker)? + search_from;
        let status_start = marker_start + marker.len();
        let status_end = find_subslice(&bytes[status_start..], b"__")? + status_start;

        if let Ok(status_text) = std::str::from_utf8(&bytes[status_start..status_end])
            && let Ok(status) = if status_text.is_empty() {
                Ok(1)
            } else {
                status_text.parse::<i32>()
            }
        {
            let mut transcript = bytes[..marker_start].to_vec();
            strip_sentinel_separator(&mut transcript);

            return Some(CompletedCommand {
                transcript,
                status_code: status,
            });
        }

        search_from = status_end + 2;
    }

    None
}

fn streamable_prefix_len(bytes: &[u8], nonce: &str) -> usize {
    let marker = sentinel_marker(nonce);
    let hold = marker
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, _)| {
            let prefix_len = index + 1;
            (bytes.len() >= prefix_len && bytes[bytes.len() - prefix_len..] == marker[..prefix_len])
                .then_some(prefix_len)
        })
        .unwrap_or(0);

    if hold == 0 {
        return bytes.len().saturating_sub(trailing_line_break_len(bytes));
    }

    let marker_start = bytes.len() - hold;
    if marker_start >= 2 && &bytes[marker_start - 2..marker_start] == b"\r\n" {
        marker_start - 2
    } else if marker_start >= 1 && bytes[marker_start - 1] == b'\n' {
        marker_start - 1
    } else {
        marker_start
    }
}

fn trailing_line_break_len(bytes: &[u8]) -> usize {
    if bytes.ends_with(b"\r\n") {
        2
    } else if bytes.ends_with(b"\n") || bytes.ends_with(b"\r") {
        1
    } else {
        0
    }
}

fn output_ends_with_unfinished_visible_line(streamed: &[u8], final_chunk: &[u8]) -> bool {
    let mut state = VisibleLineState::default();
    state.consume(streamed);
    state.consume(final_chunk);
    state.has_unfinished_visible_text
}

#[derive(Default)]
struct VisibleLineState {
    has_unfinished_visible_text: bool,
    escape: EscapeState,
}

#[derive(Default)]
enum EscapeState {
    #[default]
    None,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

impl VisibleLineState {
    fn consume(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.consume_byte(byte);
        }
    }

    fn consume_byte(&mut self, byte: u8) {
        match self.escape {
            EscapeState::None => self.consume_plain_byte(byte),
            EscapeState::Esc => self.consume_esc_byte(byte),
            EscapeState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = EscapeState::None;
                }
            }
            EscapeState::Osc => {
                if byte == 0x07 {
                    self.escape = EscapeState::None;
                } else if byte == 0x1b {
                    self.escape = EscapeState::OscEsc;
                }
            }
            EscapeState::OscEsc => {
                self.escape = if byte == b'\\' {
                    EscapeState::None
                } else {
                    EscapeState::Osc
                };
            }
        }
    }

    fn consume_plain_byte(&mut self, byte: u8) {
        match byte {
            b'\n' | b'\r' => self.has_unfinished_visible_text = false,
            0x1b => self.escape = EscapeState::Esc,
            0x08 => self.has_unfinished_visible_text = false,
            0x00..=0x1f | 0x7f => {}
            _ => self.has_unfinished_visible_text = true,
        }
    }

    fn consume_esc_byte(&mut self, byte: u8) {
        self.escape = match byte {
            b'[' => EscapeState::Csi,
            b']' => EscapeState::Osc,
            0x40..=0x5f => EscapeState::None,
            _ => EscapeState::None,
        };
    }
}

fn sentinel_marker(nonce: &str) -> Vec<u8> {
    format!("__THESEUS_DONE_{nonce}_").into_bytes()
}

fn strip_sentinel_separator(transcript: &mut Vec<u8>) {
    if transcript.ends_with(b"\r\n") {
        transcript.truncate(transcript.len() - 2);
    } else if transcript.ends_with(b"\n") {
        transcript.truncate(transcript.len() - 1);
    }
}

fn strip_echoed_payload(transcript: &mut Vec<u8>, payload: &str) {
    strip_echoed_payload_prefix(transcript, payload);

    let tail = echoed_protocol_tail(payload);
    let Some(tail) = tail.as_deref() else {
        return;
    };

    if transcript.ends_with(tail) {
        let start = transcript.len() - tail.len();
        if start >= 2 && &transcript[start - 2..start] == b"\r\n" {
            transcript.truncate(start - 2);
        } else {
            transcript.truncate(start);
        }
        return;
    }

    strip_echoed_protocol_tail_with_control_bytes(transcript, payload);
}

fn strip_echoed_payload_prefix(transcript: &mut Vec<u8>, payload: &str) {
    let echoed = echoed_payload(payload);
    let echoed = echoed.as_slice();
    if transcript.starts_with(echoed) {
        transcript.drain(..echoed.len());
    }
}

fn is_partial_echoed_payload_prefix(transcript: &[u8], payload: &str) -> bool {
    let echoed = echoed_payload(payload);
    transcript.len() < echoed.len() && echoed.starts_with(transcript)
}

fn echoed_payload(payload: &str) -> Vec<u8> {
    payload.replace('\r', "\r\n").into_bytes()
}

fn strip_echoed_protocol_tail_with_control_bytes(transcript: &mut Vec<u8>, payload: &str) {
    let Some(template) = echoed_done_template(payload) else {
        return;
    };

    let status = b"__theseus_status=$?";
    let mut search_from = 0;
    while search_from < transcript.len() {
        let Some(status_start) =
            find_subslice(&transcript[search_from..], status).map(|index| index + search_from)
        else {
            return;
        };

        if find_subslice(&transcript[status_start..], &template).is_some() {
            truncate_before_protocol_tail(transcript, status_start);
            return;
        }

        search_from = status_start + status.len();
    }
}

fn truncate_before_protocol_tail(transcript: &mut Vec<u8>, protocol_start: usize) {
    if protocol_start >= 2 && &transcript[protocol_start - 2..protocol_start] == b"\r\n" {
        transcript.truncate(protocol_start - 2);
    } else {
        transcript.truncate(protocol_start);
    }
}

fn echoed_protocol_tail(payload: &str) -> Option<Vec<u8>> {
    let lines = payload
        .trim_end_matches('\r')
        .split('\r')
        .collect::<Vec<_>>();
    let tail = lines.get(lines.len().checked_sub(2)?..)?;
    Some(tail.join("\r\n").into_bytes())
}

fn echoed_done_template(payload: &str) -> Option<Vec<u8>> {
    let bytes = payload.as_bytes();
    let start = find_subslice(bytes, b"__THESEUS_DONE_")?;
    let end = find_subslice(&bytes[start..], b"%s__")? + start + b"%s__".len();
    Some(bytes[start..end].to_vec())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn new_nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    format!("{}_{}", std::process::id(), nanos)
}

fn shell_group_payload(command: &str, nonce: &str, uses_zsh_protocol: bool) -> String {
    let command = shell_single_quote(command);
    if uses_zsh_protocol {
        return format!(
            "{{ \n\
             unset __theseus_status\n\
             {{ \n\
             eval -- {command}\n\
             __theseus_status=$?\n\
             }} always {{ \n\
             __theseus_status=${{__theseus_status:-$?}}\n\
             printf '\\n__THESEUS_DONE_{nonce}_%s__\\n' \"$__theseus_status\"\n\
             unset __theseus_status\n\
             }}\n\
             }}\n"
        );
    }

    format!(
        "{{ \n\
         eval -- {command}\n\
         __theseus_status=$?\n\
         printf '\\n__THESEUS_DONE_{nonce}_%s__\\n' \"$__theseus_status\"\n\
         }}\n"
    )
}

fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn is_zsh_shell(shell: &std::path::Path) -> bool {
    shell
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "zsh" || name.ends_with("-zsh"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{
        fs,
        path::Path,
        process::Command,
        sync::{
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            mpsc as test_mpsc,
        },
    };

    #[test]
    fn parses_completed_command_and_strips_sentinel() {
        let completed =
            parse_completed_command(b"hello\r\n__THESEUS_DONE_nonce_0__\r\n", "nonce").unwrap();

        assert_eq!(completed.transcript, b"hello");
        assert_eq!(completed.status_code, 0);
    }

    #[test]
    fn preserves_command_output_trailing_newline_before_separator() {
        let completed =
            parse_completed_command(b"hello\r\n\r\n__THESEUS_DONE_nonce_0__\r\n", "nonce").unwrap();

        assert_eq!(completed.transcript, b"hello\r\n");
        assert_eq!(completed.status_code, 0);
    }

    #[test]
    fn parses_non_zero_status() {
        let completed = parse_completed_command(b"__THESEUS_DONE_nonce_127__\n", "nonce").unwrap();

        assert_eq!(completed.transcript, b"");
        assert_eq!(completed.status_code, 127);
    }

    #[test]
    fn parses_empty_status_as_error() {
        let completed = parse_completed_command(
            b"(eval):2: unmatched \"\r\n__THESEUS_DONE_nonce___\r\n",
            "nonce",
        )
        .unwrap();

        assert_eq!(completed.transcript, b"(eval):2: unmatched \"");
        assert_eq!(completed.status_code, 1);
    }

    #[test]
    fn ignores_other_nonce() {
        assert!(parse_completed_command(b"__THESEUS_DONE_other_0__\n", "nonce").is_none());
    }

    #[test]
    fn resize_event_triggers_resize_without_returning_output() {
        let resize_calls = std::cell::Cell::new(0);

        let output = handle_shell_event(ShellEvent::Resize, || {
            resize_calls.set(resize_calls.get() + 1);
            Ok(())
        })
        .unwrap();

        assert_eq!(output, None);
        assert_eq!(resize_calls.get(), 1);
    }

    #[test]
    fn output_event_returns_shell_chunk_without_resizing() {
        let resize_calls = std::cell::Cell::new(0);

        let output = handle_shell_event(ShellEvent::Chunk(Ok(b"hello".to_vec())), || {
            resize_calls.set(resize_calls.get() + 1);
            Ok(())
        })
        .unwrap();

        assert_eq!(output, Some(b"hello".to_vec()));
        assert_eq!(resize_calls.get(), 0);
    }

    #[test]
    fn drain_event_handling_applies_resize_events() {
        let events = [ShellEvent::Chunk(Ok(b"stale".to_vec())), ShellEvent::Resize];
        let resize_calls = std::cell::Cell::new(0);

        for event in events {
            let _ = handle_shell_event(event, || {
                resize_calls.set(resize_calls.get() + 1);
                Ok(())
            })
            .unwrap();
        }

        assert_eq!(resize_calls.get(), 1);
    }

    #[test]
    fn recv_shell_chunk_applies_resize_and_returns_next_chunk() {
        let (tx, rx) = mpsc::channel();
        tx.send(ShellEvent::Resize).unwrap();
        tx.send(ShellEvent::Chunk(Ok(b"next".to_vec()))).unwrap();
        let resize_calls = std::cell::Cell::new(0);

        let chunk = recv_shell_chunk_from(&rx, || {
            resize_calls.set(resize_calls.get() + 1);
            Ok(())
        })
        .unwrap();

        assert_eq!(chunk, b"next");
        assert_eq!(resize_calls.get(), 1);
    }

    #[test]
    fn waits_for_complete_status_terminator() {
        assert!(parse_completed_command(b"hello\n__THESEUS_DONE_nonce_0", "nonce").is_none());
    }

    #[test]
    fn separator_needed_after_visible_output_without_newline() {
        assert!(output_ends_with_unfinished_visible_line(b"", b"hello"));
        assert!(output_ends_with_unfinished_visible_line(b"hel", b"lo"));
    }

    #[test]
    fn separator_not_needed_after_trailing_newline_or_carriage_return() {
        assert!(!output_ends_with_unfinished_visible_line(b"", b"hello\n"));
        assert!(!output_ends_with_unfinished_visible_line(b"", b"hello\r\n"));
        assert!(!output_ends_with_unfinished_visible_line(b"", b"hello\r"));
    }

    #[test]
    fn separator_not_needed_after_clear_screen_control_sequences() {
        assert!(!output_ends_with_unfinished_visible_line(
            b"",
            b"\x1b[H\x1b[2J\x1b[3J"
        ));
    }

    #[test]
    fn separator_needed_when_visible_output_is_followed_by_style_reset() {
        assert!(output_ends_with_unfinished_visible_line(
            b"",
            b"hello\x1b[0m"
        ));
    }

    #[test]
    fn skips_echoed_protocol_template_before_real_sentinel() {
        let bytes =
            b"printf '\\n__THESEUS_DONE_nonce_%s__\\n'\r\nok\r\n__THESEUS_DONE_nonce_0__\r\n";
        let completed = parse_completed_command(bytes, "nonce").unwrap();

        assert_eq!(
            completed.transcript,
            b"printf '\\n__THESEUS_DONE_nonce_%s__\\n'\r\nok"
        );
        assert_eq!(completed.status_code, 0);
    }

    #[test]
    fn strips_echoed_payload_from_transcript_prefix() {
        let payload = "echo ok\r__theseus_status=$?\r";
        let mut transcript = b"echo ok\r\n__theseus_status=$?\r\nok".to_vec();

        strip_echoed_payload(&mut transcript, payload);

        assert_eq!(transcript, b"ok");
    }

    #[test]
    fn waits_for_complete_long_echoed_payload_before_draining() {
        let command = format!("printf '{}'", "x".repeat(STREAM_HOLD_BACK_BYTES + 100));
        let payload = shell_group_payload(&command, "nonce", false).replace('\n', "\r");
        let echoed = echoed_payload(&payload);
        let mut partial = echoed[..STREAM_HOLD_BACK_BYTES + 1].to_vec();

        strip_echoed_payload_prefix(&mut partial, &payload);

        assert!(is_partial_echoed_payload_prefix(&partial, &payload));
        assert!(partial.len() > STREAM_HOLD_BACK_BYTES);

        let mut complete = echoed.clone();
        complete.extend_from_slice(b"ok");
        strip_echoed_payload_prefix(&mut complete, &payload);

        assert_eq!(complete, b"ok");
    }

    #[test]
    fn command_payload_groups_protocol_with_command() {
        let payload = shell_group_payload("vim", "nonce", false);

        assert_eq!(
            payload,
            "{ \neval -- 'vim'\n__theseus_status=$?\nprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\n}\n"
        );
    }

    #[test]
    fn command_payload_shell_quotes_user_command_for_eval() {
        let payload = shell_group_payload("printf '%s' 'a b'", "nonce", false);

        assert_eq!(
            payload,
            "{ \neval -- 'printf '\\''%s'\\'' '\\''a b'\\'''\n__theseus_status=$?\nprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\n}\n"
        );
    }

    #[test]
    fn zsh_command_payload_prints_sentinel_from_always_block() {
        let payload = shell_group_payload("sleep 100", "nonce", true);

        assert!(payload.contains("} always {"));
        assert!(payload.contains("eval -- 'sleep 100'"));
        assert!(payload.contains("__THESEUS_DONE_nonce_%s__"));
    }

    #[test]
    fn streaming_keeps_only_possible_sentinel_prefix() {
        assert_eq!(streamable_prefix_len(b"vim-screen-update", "nonce"), 17);
        assert_eq!(
            streamable_prefix_len(b"vim-screen-update\r\n__THESEUS_D", "nonce"),
            17
        );
    }

    #[test]
    fn streaming_holds_trailing_line_break_until_sentinel_arrives() {
        assert_eq!(streamable_prefix_len(b"a b\r\n", "nonce"), 3);
        assert_eq!(streamable_prefix_len(b"hello\r\n\r\n", "nonce"), 7);
        assert_eq!(
            streamable_prefix_len(b"hello\r\n\r\n__THESEUS_D", "nonce"),
            7
        );
    }

    #[test]
    fn streaming_does_not_hold_marker_like_user_output() {
        let bytes = b"literal __THESEUS_DONE_other_0__";

        assert_eq!(streamable_prefix_len(bytes, "nonce"), bytes.len() - 2);
    }

    #[test]
    fn strips_echoed_protocol_tail_after_command_output() {
        let payload = "git branch\r__theseus_status=$?\rprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\r";
        let mut transcript = b"* dev\r\n  master\r\n__theseus_status=$?\r\nprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"".to_vec();

        strip_echoed_payload(&mut transcript, payload);

        assert_eq!(transcript, b"* dev\r\n  master");
    }

    #[test]
    fn command_completion_strips_echoed_protocol_tail_before_real_sentinel() {
        let payload = "git branch\r__theseus_status=$?\rprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\r";
        let mut completed = parse_completed_command(
            b"* dev\r\n  master\r\n__theseus_status=$?\r\nprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\r\n__THESEUS_DONE_nonce_0__\r\n",
            "nonce",
        )
        .unwrap();

        strip_echoed_payload(&mut completed.transcript, payload);

        assert_eq!(completed.transcript, b"* dev\r\n  master");
        assert_eq!(completed.status_code, 0);
    }

    #[test]
    fn strips_echoed_protocol_tail_with_terminal_control_bytes() {
        let payload = "git branch\r__theseus_status=$?\rprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\r";
        let mut transcript = b"* dev\r\n  master\r\n__theseus_status=$?\r\n\x1b[?2004h\x1b[Kprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"".to_vec();

        strip_echoed_payload(&mut transcript, payload);

        assert_eq!(transcript, b"* dev\r\n  master");
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_runs_command_and_captures_status() {
        let mut session = start_test_session("/bin/sh");

        let output = session.run_command("printf shell-ok; false").unwrap();

        assert_eq!(output.status_code, Some(1));
        assert_eq!(output.transcript_lossy(), "shell-ok");
        assert_eq!(output.streamed, io::stdout().is_terminal());
    }

    #[cfg(unix)]
    #[test]
    fn foreground_command_does_not_receive_protocol_as_stdin() {
        if !command_exists("python3") {
            eprintln!("skipping test: python3 is not available");
            return;
        }

        let mut session = start_test_session("/bin/sh");

        let output = session
            .run_command(
                r#"python3 -c 'import os, select, sys; ready, _, _ = select.select([sys.stdin], [], [], 0); data = os.read(0, 4096).decode() if ready else ""; sys.stdout.write("stdin:" + data.replace("\n", "\\n"))'"#,
            )
            .unwrap();

        assert_eq!(output.status_code, Some(0));
        assert_eq!(output.transcript_lossy(), "stdin:");
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_state_persists_for_available_shells() {
        for shell in available_shells() {
            let temp_dir = TempTestDir::new();
            let (mut session, _home) = start_clean_test_session_in_dir(&shell, temp_dir.path());

            assert_success(&mut session, "export THESEUS_TEST_VAR=env-ok", "");
            assert_success(&mut session, "printf %s \"$THESEUS_TEST_VAR\"", "env-ok");
            assert_success(&mut session, "theseus_fn(){ printf fn-ok; }", "");
            assert_success(&mut session, "theseus_fn", "fn-ok");
            assert_success(&mut session, "cd /tmp", "");
            assert_success(&mut session, "pwd", "/tmp\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_aliases_work_in_interactive_bash_and_zsh() {
        for shell in available_shells()
            .into_iter()
            .filter(|shell| shell.ends_with("/bash"))
        {
            let (mut session, _home) = start_clean_test_session(&shell);

            assert_success(&mut session, "alias theseus_alias='printf alias-ok'", "");
            assert_success(&mut session, "theseus_alias", "alias-ok");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_loads_bash_and_zsh_startup_aliases() {
        for (shell, rc_file) in [("/bin/bash", ".bashrc"), ("/bin/zsh", ".zshrc")] {
            if !Path::new(shell).exists() {
                continue;
            }

            let home = TempTestDir::new();
            fs::write(
                home.path().join(rc_file),
                "alias theseus_rc_alias='printf rc-alias-ok'\n",
            )
            .unwrap();

            let mut session = PersistentShellSession::start(PersistentShellConfig {
                shell: PathBuf::from(shell),
                env_vars: clean_home_env_vars(shell, Some(&home)),
                working_dir: None,
            })
            .unwrap();

            assert_success(&mut session, "theseus_rc_alias", "rc-alias-ok");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_preserves_quoting_multiline_and_status_for_available_shells() {
        for shell in available_shells() {
            let (mut session, _home) = start_clean_test_session(&shell);

            assert_success(&mut session, "printf '%s' 'a b'", "a b");
            assert_success(
                &mut session,
                "cat <<'EOF'\nhello heredoc\nEOF",
                "hello heredoc\n",
            );
            assert_success(&mut session, "printf '%s' \"$(printf nested)\"", "nested");

            let output = session.run_command("sh -c 'exit 42'").unwrap();
            assert_eq!(output.status_code, Some(42), "shell: {shell}");
            assert_eq!(output.transcript_lossy(), "", "shell: {shell}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_returns_after_unmatched_backtick_syntax_error() {
        for shell in available_shells() {
            let (tx, rx) = test_mpsc::channel();
            let thread_shell = shell.clone();

            thread::spawn(move || {
                let (mut session, _home) = start_clean_test_session(&thread_shell);
                let output = session.run_command("du -h vscode-plugin` (19G)");
                let recovery = session.run_command("printf recovered");
                let _ = tx.send((output, recovery));
            });

            let (output, recovery) = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("command hung for shell: {shell}"));
            let output = output.unwrap();
            assert_ne!(output.status_code, Some(0), "shell: {shell}");
            assert!(
                normalized_transcript(&output).contains("`"),
                "shell: {shell}, output: {:?}",
                output.transcript_lossy()
            );

            let recovery = recovery.unwrap();
            assert_eq!(recovery.status_code, Some(0), "shell: {shell}");
            assert_eq!(normalized_transcript(&recovery), "recovered");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_returns_after_unmatched_quote_in_multiline_command() {
        for shell in available_shells() {
            let (tx, rx) = test_mpsc::channel();
            let thread_shell = shell.clone();

            thread::spawn(move || {
                let (mut session, _home) = start_clean_test_session(&thread_shell);
                let output = session.run_command("echo \\\n \"test");
                let recovery = session.run_command("printf recovered");
                let _ = tx.send((output, recovery));
            });

            let (output, recovery) = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("command hung for shell: {shell}"));
            let output = output.unwrap();
            assert_ne!(output.status_code, Some(0), "shell: {shell}");
            assert!(
                normalized_transcript(&output).contains("unmatched")
                    || normalized_transcript(&output).contains("unexpected EOF")
                    || normalized_transcript(&output).contains("unterminated"),
                "shell: {shell}, output: {:?}",
                output.transcript_lossy()
            );

            let recovery = recovery.unwrap();
            assert_eq!(recovery.status_code, Some(0), "shell: {shell}");
            assert_eq!(normalized_transcript(&recovery), "recovered");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_ctrl_c_interrupts_foreground_command() {
        for shell in available_shells() {
            let (mut session, _home) = start_clean_test_session(&shell);
            let writer = Arc::clone(&session.writer);
            let (tx, rx) = test_mpsc::channel();

            thread::spawn(move || {
                let output = session.run_command("sleep 100");
                let recovery = session.run_command("printf recovered");
                let _ = tx.send((output, recovery));
            });

            thread::sleep(Duration::from_millis(200));
            {
                let mut writer = writer.lock().unwrap();
                writer.write_all(&[3]).unwrap();
                writer.flush().unwrap();
            }

            let (output, recovery) = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("Ctrl+C did not interrupt command for shell: {shell}"));
            let output = output.unwrap();
            assert_ne!(output.status_code, Some(0), "shell: {shell}");

            let recovery = recovery.unwrap();
            assert_eq!(recovery.status_code, Some(0), "shell: {shell}");
            assert_eq!(normalized_transcript(&recovery), "recovered");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_does_not_leak_or_overstrip_protocol_like_output() {
        for shell in available_shells() {
            let (mut session, _home) = start_clean_test_session(&shell);

            assert_success(
                &mut session,
                "printf '%s' '__THESEUS_DONE_other_0__'",
                "__THESEUS_DONE_other_0__",
            );
            assert_success(&mut session, "printf '%s' '__THESEUS_D'", "__THESEUS_D");
            assert_success(
                &mut session,
                "printf '%s' '__THESEUS_DONE_nonce_text__'",
                "__THESEUS_DONE_nonce_text__",
            );
            assert_no_protocol_leak(&mut session, "printf '%s' normal-output", "normal-output");

            let large = session
                .run_command("printf '%01024d' 0 | tr '0' 'x'")
                .unwrap()
                .transcript_lossy()
                .replace("\r\n", "\n");
            assert_eq!(large.len(), 1024, "shell: {shell}");
            assert!(large.chars().all(|ch| ch == 'x'), "shell: {shell}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_shell_reports_pty_for_available_shells() {
        for shell in available_shells() {
            let (mut session, _home) = start_clean_test_session(&shell);

            assert_success(&mut session, "test -t 0 && printf pty-ok", "pty-ok");
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "heavy PTY smoke: launches vim and depends on local terminal behavior"]
    fn ignored_vim_smoke_starts_and_exits() {
        if !command_exists("vim") {
            eprintln!("skipping ignored smoke: vim is not available");
            return;
        }

        let mut session = start_test_session("/bin/sh");
        let output = session.run_command("vim --version | head -n 1").unwrap();

        assert_eq!(output.status_code, Some(0));
        assert!(output.transcript_lossy().contains("VIM"));
    }

    #[cfg(unix)]
    fn available_shells() -> Vec<String> {
        ["/bin/sh", "/bin/bash", "/bin/zsh"]
            .into_iter()
            .filter(|shell| Path::new(shell).exists())
            .map(str::to_string)
            .collect()
    }

    #[cfg(unix)]
    fn start_test_session(shell: &str) -> PersistentShellSession {
        PersistentShellSession::start(PersistentShellConfig {
            shell: PathBuf::from(shell),
            env_vars: Vec::new(),
            working_dir: None,
        })
        .unwrap()
    }

    #[cfg(unix)]
    fn start_clean_test_session(shell: &str) -> (PersistentShellSession, Option<TempTestDir>) {
        start_clean_test_session_with_dir(shell, None)
    }

    #[cfg(unix)]
    fn start_clean_test_session_in_dir(
        shell: &str,
        working_dir: &Path,
    ) -> (PersistentShellSession, Option<TempTestDir>) {
        start_clean_test_session_with_dir(shell, Some(working_dir))
    }

    #[cfg(unix)]
    fn start_clean_test_session_with_dir(
        shell: &str,
        working_dir: Option<&Path>,
    ) -> (PersistentShellSession, Option<TempTestDir>) {
        let home = clean_home_for_interactive_shell(shell);
        let env_vars = clean_home_env_vars(shell, home.as_ref());
        let session = PersistentShellSession::start(PersistentShellConfig {
            shell: PathBuf::from(shell),
            env_vars,
            working_dir: working_dir.map(Path::to_path_buf),
        })
        .unwrap();

        (session, home)
    }

    #[cfg(unix)]
    fn clean_home_for_interactive_shell(shell: &str) -> Option<TempTestDir> {
        (shell.ends_with("/bash") || shell.ends_with("/zsh")).then(TempTestDir::new)
    }

    #[cfg(unix)]
    fn clean_home_env_vars(shell: &str, home: Option<&TempTestDir>) -> Vec<(String, String)> {
        let Some(home) = home else {
            return Vec::new();
        };
        let home = home.path().display().to_string();
        let mut env_vars = vec![("HOME".to_string(), home.clone())];
        if shell.ends_with("/zsh") {
            env_vars.push(("ZDOTDIR".to_string(), home));
        }
        env_vars
    }

    #[cfg(unix)]
    fn assert_success(session: &mut PersistentShellSession, command: &str, expected: &str) {
        let output = session.run_command(command).unwrap();
        let transcript = normalized_transcript(&output);

        assert_eq!(output.status_code, Some(0), "command: {command}");
        assert_eq!(transcript, expected, "command: {command}");
    }

    #[cfg(unix)]
    fn assert_no_protocol_leak(
        session: &mut PersistentShellSession,
        command: &str,
        expected: &str,
    ) {
        let output = session.run_command(command).unwrap();
        let transcript = normalized_transcript(&output);

        assert_eq!(output.status_code, Some(0), "command: {command}");
        assert_eq!(transcript, expected, "command: {command}");
        assert!(
            !transcript.contains("__theseus_status"),
            "command leaked status protocol: {command}"
        );
        assert!(
            !transcript.contains("__THESEUS_DONE_"),
            "command leaked done protocol: {command}"
        );
    }

    #[cfg(unix)]
    fn normalized_transcript(output: &CommandOutput) -> String {
        output.transcript_lossy().replace("\r\n", "\n")
    }

    #[cfg(unix)]
    fn command_exists(command: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {command} >/dev/null 2>&1"))
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    struct TempTestDir {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl TempTestDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "theseus-shell-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();

            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    #[cfg(unix)]
    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
