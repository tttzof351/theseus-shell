use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use portable_pty::{Child, CommandBuilder, MasterPty, native_pty_system};

use super::output::{LegacyOutput, PtyOutput};
use super::platform::{RawModeGuard, current_pty_size, interactive_shell_args};
use crate::common::{output::CommandOutput, terminal_output};

#[cfg(unix)]
use signal_hook::{
    consts::signal::SIGWINCH,
    iterator::{Handle as SignalHandle, Signals},
};

mod input_forwarder;
mod protocol;

use input_forwarder::InputForwarder;
use protocol::{
    CompletedCommand, is_zsh_shell, new_nonce, output_ends_with_unfinished_visible_line,
    parse_completed_command, ready_marker, shell_group_payload, streamable_prefix_len,
    strip_ready_marker,
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
        self.run_command_inner(command, None, None)
    }

    pub(crate) fn run_command_with_terminal(
        &mut self,
        command: &str,
        input: crate::common::terminal_input::SharedInput,
        output: &mut dyn PtyOutput,
    ) -> io::Result<CommandOutput> {
        self.run_command_inner(command, Some(input), Some(output))
    }

    fn run_command_inner(
        &mut self,
        command: &str,
        input: Option<crate::common::terminal_input::SharedInput>,
        output: Option<&mut dyn PtyOutput>,
    ) -> io::Result<CommandOutput> {
        self.ensure_shell_is_running()?;
        self.resize_to_current_terminal()?;

        let payload = self.command_payload(command);
        self.write_to_shell(payload.as_bytes())?;

        let stream_output = output.is_some() || io::stdout().is_terminal();
        let _raw_mode = RawModeGuard::enable_if_terminal()?;
        let mut input_forwarder = InputForwarder::new(Arc::clone(&self.writer), input);
        let completed = match output {
            Some(output) => self.read_until_sentinel_streaming(output, Some(&mut input_forwarder)),
            None => self.read_until_sentinel(stream_output, Some(&mut input_forwarder)),
        };
        input_forwarder.stop()?;
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
        let completed = self.read_until_sentinel(false, None)?;

        Ok(CommandOutput {
            transcript: completed.transcript,
            status_code: Some(completed.status_code),
            streamed: false,
        })
    }

    fn initialize_shell(&mut self) -> io::Result<()> {
        // POSIX shells and modern Bash can abandon the surrounding command
        // group on SIGINT unless the shell handles it. Foreground children
        // reset a caught signal to its default disposition, so Ctrl+C still
        // stops them while the shell can report their exit status. Zsh uses
        // an `always` block instead.
        let payload = self.command_payload(
            "stty -echo 2>/dev/null || true\n\
             if [ -n \"${BASH_VERSION-}\" ]; then\n\
               if [ -z \"$(trap -p INT)\" ]; then trap ':' INT; fi\n\
             elif [ -z \"${ZSH_VERSION-}\" ]; then\n\
               trap ':' INT\n\
             fi\n\
             bind 'set enable-bracketed-paste off' 2>/dev/null || true\n\
             unsetopt zle prompt_cr prompt_sp 2>/dev/null || true\n\
             PROMPT=''\n\
             RPROMPT=''\n\
             PS1=''\n\
             PS2=''",
        );
        self.write_to_shell(payload.as_bytes())?;
        let _ = self.read_until_sentinel(false, None)?;
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

    fn read_until_command_ready(
        &mut self,
        mut resized: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Vec<u8>> {
        let marker = ready_marker(&self.nonce);
        let mut pending = Vec::new();
        loop {
            let chunk = self.recv_shell_chunk_notifying_resize(&mut resized)?;
            if chunk.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "shell ended before command readiness marker",
                ));
            }
            pending.extend_from_slice(&chunk);
            if strip_ready_marker(&mut pending, &marker) {
                return Ok(pending);
            }
        }
    }

    fn read_until_sentinel(
        &mut self,
        stream_output: bool,
        mut input: Option<&mut InputForwarder>,
    ) -> io::Result<CompletedCommand> {
        if stream_output {
            return terminal_output::with_stdout(|output| {
                self.read_until_sentinel_streaming(&mut LegacyOutput(output), input)
            });
        }

        let mut initial = Some(self.read_until_command_ready(|| Ok(()))?);
        if let Some(input) = input.as_deref_mut() {
            input.start()?;
        }
        let mut pending = Vec::new();
        let mut transcript = Vec::new();

        loop {
            let chunk = match initial.take().filter(|chunk| !chunk.is_empty()) {
                Some(chunk) => chunk,
                None => self.recv_shell_chunk()?,
            };
            if chunk.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "shell ended before command sentinel",
                ));
            }

            pending.extend_from_slice(&chunk);
            if let Some(mut completed) = parse_completed_command(&pending, &self.nonce) {
                if let Some(input) = input {
                    input.stop()?;
                }
                transcript.extend_from_slice(&completed.transcript);
                completed.transcript = transcript;
                self.drain_pending_output();
                return Ok(completed);
            }

            if pending.len() > STREAM_HOLD_BACK_BYTES {
                let safe_len = pending.len() - STREAM_HOLD_BACK_BYTES;
                transcript.extend_from_slice(&pending[..safe_len]);
                pending.drain(..safe_len);
            }
        }
    }

    fn read_until_sentinel_streaming(
        &mut self,
        output: &mut dyn PtyOutput,
        mut input: Option<&mut InputForwarder>,
    ) -> io::Result<CompletedCommand> {
        let mut initial =
            Some(self.read_until_command_ready(|| output.resized(current_pty_size()))?);
        if let Some(input) = input.as_deref_mut() {
            input.start()?;
        }
        let mut pending = Vec::new();
        let mut transcript = Vec::new();
        loop {
            let chunk = match initial.take().filter(|chunk| !chunk.is_empty()) {
                Some(chunk) => chunk,
                None => {
                    self.recv_shell_chunk_notifying_resize(|| output.resized(current_pty_size()))?
                }
            };
            if chunk.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "shell ended before command sentinel",
                ));
            }

            pending.extend_from_slice(&chunk);
            if let Some(mut completed) = parse_completed_command(&pending, &self.nonce) {
                // Stop stdin before final rendering/draining. Bytes typed once
                // completion is known belong to the returning managed editor.
                if let Some(input) = input {
                    input.stop()?;
                }
                let needs_prompt_separator =
                    output_ends_with_unfinished_visible_line(&transcript, &completed.transcript);
                output.write_all(&completed.transcript)?;
                if needs_prompt_separator {
                    output.write_all(b"\r\n")?;
                }
                output.flush()?;
                transcript.extend_from_slice(&completed.transcript);
                completed.transcript = transcript;
                self.drain_pending_output();
                return Ok(completed);
            }

            let safe_len = streamable_prefix_len(&pending, &self.nonce);
            if safe_len > 0 {
                output.write_all(&pending[..safe_len])?;
                output.flush()?;
                transcript.extend_from_slice(&pending[..safe_len]);
                pending.drain(..safe_len);
            }
        }
    }

    fn recv_shell_chunk(&mut self) -> io::Result<Vec<u8>> {
        self.recv_shell_chunk_notifying_resize(|| Ok(()))
    }

    fn recv_shell_chunk_notifying_resize(
        &mut self,
        mut resized: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Vec<u8>> {
        loop {
            match self.event_rx.recv() {
                Ok(event) => {
                    if let Some(chunk) = handle_shell_event(event, || {
                        self.resize_to_current_terminal()?;
                        resized()
                    })? {
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::protocol::shell_single_quote;
    use super::*;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};
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

    #[cfg(unix)]
    #[test]
    fn shell_waits_for_readiness_before_forwarding_queued_stdin() {
        use crate::common::terminal_input::TerminalInput;
        use crossterm::event::{Event, KeyCode, KeyEvent};
        use std::os::{fd::OwnedFd, unix::net::UnixStream};

        struct ObservedWriter {
            writer: Arc<Mutex<Box<dyn Write + Send>>>,
            writes: mpsc::Sender<Vec<u8>>,
        }
        impl Write for ObservedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writer.lock().unwrap().write_all(bytes)?;
                let _ = self.writes.send(bytes.to_vec());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.writer.lock().unwrap().flush()
            }
        }

        for shell in available_shells() {
            let (mut session, home) = start_clean_test_session(&shell);
            let (reader, mut sender) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            let input = TerminalInput::from_test_file(OwnedFd::from(reader).into());
            sender.write_all(b"\rIMMEDIATE\r").unwrap();
            assert!(matches!(
                input.lock().unwrap().next_event(Duration::ZERO).unwrap(),
                Some(Event::Key(KeyEvent {
                    code: KeyCode::Enter,
                    ..
                }))
            ));

            // Freeze the real shell before it parses the command. The pending
            // stdin must stay in TerminalInput until the shell resumes and
            // acknowledges readiness, regardless of thread scheduling speed.
            let pid = session.child.process_id().unwrap() as libc::pid_t;
            assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
                pid
            );
            assert!(libc::WIFSTOPPED(status));
            let (write_tx, write_rx) = mpsc::channel();
            session.writer = Arc::new(Mutex::new(Box::new(ObservedWriter {
                writer: Arc::clone(&session.writer),
                writes: write_tx,
            })));
            let (result_tx, result_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let _home = home;
                let mut visible = Vec::new();
                let result = session.run_command_with_terminal(
                    "read value; printf 'RECEIVED=%s\\n' \"$value\"",
                    input,
                    &mut LegacyOutput(&mut visible),
                );
                let recovery = session.run_command("printf recovered");
                let _ = result_tx.send((result, visible, recovery));
            });

            let payload = write_rx.recv_timeout(Duration::from_secs(2));
            let early_input = write_rx.recv_timeout(Duration::from_millis(100));
            // Resume even on assertion failure, so the stopped child can exit.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
            if payload.is_err() || !matches!(early_input, Err(mpsc::RecvTimeoutError::Timeout)) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                worker.join().unwrap();
                panic!(
                    "shell {shell}: payload={payload:?}, input before readiness={early_input:?}"
                );
            }
            let result = result_rx.recv_timeout(Duration::from_secs(2));
            if result.is_err() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                worker.join().unwrap();
                panic!("queued stdin hung for shell: {shell}");
            }
            let (result, visible, recovery) = result.unwrap();
            worker.join().unwrap();
            let result = result.unwrap();
            assert_eq!(result.status_code, Some(0), "{shell}");
            assert_eq!(
                normalized_transcript(&result),
                "RECEIVED=IMMEDIATE\n",
                "{shell}"
            );
            assert_eq!(visible, result.transcript, "{shell}");
            let recovery = recovery.unwrap();
            assert_eq!(recovery.status_code, Some(0), "{shell}");
            assert_eq!(normalized_transcript(&recovery), "recovered", "{shell}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn sentinel_stops_input_before_final_output_and_preserves_next_paste() {
        use crate::common::terminal_input::TerminalInput;
        use std::os::{fd::OwnedFd, unix::net::UnixStream};
        struct BoundaryOutput {
            stop: Arc<AtomicBool>,
            sender: UnixStream,
            sent: bool,
            paste: Vec<u8>,
        }
        impl Write for BoundaryOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes == b"\r\n" && !self.sent {
                    assert!(
                        self.stop.load(Ordering::Acquire),
                        "stdin still forwarded after sentinel"
                    );
                    self.sender.write_all(&self.paste)?;
                    self.sent = true;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl PtyOutput for BoundaryOutput {
            fn resized(&mut self, _: portable_pty::PtySize) -> io::Result<()> {
                Ok(())
            }
        }
        for shell in available_shells() {
            let (mut session, _home) = start_clean_test_session(&shell);
            let (reader, sender) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            let input = TerminalInput::from_test_file(OwnedFd::from(reader).into());
            let mut forwarding =
                InputForwarder::new(Arc::clone(&session.writer), Some(Arc::clone(&input)));
            let paste = "\x1b[200~NEXT_DRAFT\r\nПривет\x03\x1b[201~Z"
                .as_bytes()
                .to_vec();
            let mut output = BoundaryOutput {
                stop: Arc::clone(&forwarding.stop),
                sender,
                sent: false,
                paste: paste.clone(),
            };
            let payload = session.command_payload("printf EDGE");
            session.write_to_shell(payload.as_bytes()).unwrap();
            let completed = session
                .read_until_sentinel_streaming(&mut output, Some(&mut forwarding))
                .unwrap();
            assert_eq!(completed.status_code, 0, "{shell}");
            assert!(
                output.sent,
                "fixture did not reach the post-sentinel separator: {shell}"
            );
            assert!(
                forwarding.thread.is_none(),
                "forwarder not joined at sentinel"
            );
            let mut bytes = vec![0; paste.len()];
            let count = input
                .lock()
                .unwrap()
                .read_raw(&mut bytes, Duration::ZERO)
                .unwrap();
            assert_eq!(
                &bytes[..count],
                paste,
                "next editor input was lost to {shell}"
            );
        }
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
        for shell in available_shells() {
            let rc_file = if shell.ends_with("/bash") {
                ".bashrc"
            } else if shell.ends_with("/zsh") {
                ".zshrc"
            } else {
                continue;
            };

            let home = TempTestDir::new();
            fs::write(
                home.path().join(rc_file),
                "alias theseus_rc_alias='printf rc-alias-ok'\ntrap 'printf rc-int-ok' INT\n",
            )
            .unwrap();

            let mut session = PersistentShellSession::start(PersistentShellConfig {
                shell: PathBuf::from(&shell),
                env_vars: clean_home_env_vars(&shell, Some(&home)),
                working_dir: None,
            })
            .unwrap();

            assert_success(&mut session, "theseus_rc_alias", "rc-alias-ok");
            let traps = session.run_command("trap").unwrap();
            assert!(traps.transcript_lossy().contains("rc-int-ok"), "{shell}");
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
            let diagnostic = normalized_transcript(&output);
            assert!(
                diagnostic.contains('`') || diagnostic.contains("backquote"),
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
            let diagnostic = normalized_transcript(&output).to_ascii_lowercase();
            assert!(
                diagnostic.contains("unmatched")
                    || diagnostic.contains("unexpected eof")
                    || diagnostic.contains("unterminated"),
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
            let directory = TempTestDir::new();
            let ready = directory.path().join("foreground-ready");
            let command = format!(
                "sh -c {}",
                shell_single_quote(&format!(
                    "printf ready > {}; exec sleep 100",
                    shell_single_quote(&ready.to_string_lossy())
                ))
            );

            thread::spawn(move || {
                let output = session.run_command(&command);
                let recovery = session.run_command("printf recovered");
                let _ = tx.send((output, recovery));
            });

            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !ready.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "foreground command did not start for shell: {shell}"
                );
                thread::sleep(Duration::from_millis(2));
            }
            {
                let mut writer = writer.lock().unwrap();
                writer.write_all(&[3]).unwrap();
                writer.flush().unwrap();
            }

            let (output, recovery) = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("Ctrl+C did not interrupt command for shell: {shell}"));
            let output = output.unwrap();
            assert_eq!(output.status_code, Some(130), "shell: {shell}");

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
                "printf '\\n\\n\\033[32m__THESEUS_READY_other__\\033[0m\\n'",
                "\n\n\x1b[32m__THESEUS_READY_other__\x1b[0m\n",
            );
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
        use std::os::unix::fs::PermissionsExt;

        let mut shells = ["/bin/sh", "/bin/dash", "/bin/bash", "/bin/zsh"]
            .into_iter()
            .filter(|shell| Path::new(shell).exists())
            .map(str::to_string)
            .collect::<Vec<_>>();
        // macOS ships Bash 3.2. Also exercise the Bash selected through PATH
        // (e.g. Homebrew's Bash 5) so Linux protocol failures reproduce locally.
        if let Some(path) = std::env::var_os("PATH")
            && let Some(bash) = std::env::split_paths(&path)
                .map(|directory| directory.join("bash"))
                .find(|candidate| {
                    candidate.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
        {
            let bash = bash.to_string_lossy().into_owned();
            if !shells.contains(&bash) {
                shells.push(bash);
            }
        }
        shells
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
