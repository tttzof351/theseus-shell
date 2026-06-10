use std::{
    io::{self, IsTerminal, Read, Write},
    sync::mpsc,
    thread,
};

use portable_pty::{CommandBuilder, native_pty_system};

use super::{
    PtyCommandConfig,
    input::forward_terminal_input_until_exit,
    platform::{RawModeGuard, current_pty_size, shell_command_args},
};
use crate::common::output::CommandOutput;

pub fn run_pty_command(config: PtyCommandConfig) -> io::Result<CommandOutput> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(current_pty_size())
        .map_err(|err| io::Error::other(err.to_string()))?;

    let mut command = CommandBuilder::new(&config.shell);
    command.args(shell_command_args(&config.shell, &config.command));

    for (key, value) in config.env_vars {
        command.env(key, value);
    }

    if let Some(working_dir) = config.working_dir {
        command.cwd(working_dir);
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|err| io::Error::other(err.to_string()))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| io::Error::other(err.to_string()))?;
    let (tx, rx) = mpsc::channel();

    let reader_thread = thread::spawn(move || {
        let mut transcript = Vec::new();
        let mut stdout = io::stdout();
        let mut buffer = [0; 8192];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    transcript.extend_from_slice(&buffer[..n]);
                    if stdout.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                    let _ = stdout.flush();
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }

        let _ = tx.send(transcript);
    });

    if io::stdin().is_terminal() {
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let _raw_mode = RawModeGuard::enable_if_terminal()?;

        let status_code = forward_terminal_input_until_exit(
            &mut child,
            writer,
            &*pair.master,
            config.cancellation,
        )?;
        drop(pair.master);
        let transcript = finish_reader(reader_thread, rx)?;
        return Ok(CommandOutput::streamed(transcript, status_code));
    }

    let status = child.wait()?;
    drop(pair.master);
    let transcript = finish_reader(reader_thread, rx)?;

    Ok(CommandOutput::streamed(
        transcript,
        Some(status.exit_code() as i32),
    ))
}

fn finish_reader(
    reader_thread: thread::JoinHandle<()>,
    rx: mpsc::Receiver<Vec<u8>>,
) -> io::Result<Vec<u8>> {
    reader_thread
        .join()
        .map_err(|_| io::Error::other("pty reader thread panicked"))?;
    rx.recv()
        .map_err(|_| io::Error::other("pty reader thread did not return transcript"))
}
