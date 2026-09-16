//! PTY process, screen observation and response-budget assertions.

use super::*;

pub(super) fn assert_responsive(started: Instant, scenario: &str) {
    let elapsed = started.elapsed();
    eprintln!("{scenario}: {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(250),
        "{scenario} exceeded the 250 ms UI response budget: {elapsed:?}"
    );
}

pub(super) fn waiting_spinner_is_visible(screen: &vt100::Screen) -> bool {
    screen.rows(0, screen.size().1).any(|line| {
        let mut chars = line.trim().chars();
        chars
            .next()
            .is_some_and(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
            && chars.next().is_none()
    })
}

pub(super) struct UiPty {
    pub(super) child: Box<dyn Child + Send + Sync>,
    pub(super) master: Box<dyn MasterPty + Send>,
    input: Box<dyn Write + Send>,
    pub(super) parser: Arc<Mutex<vt100::Parser>>,
    pub(super) reader: Option<thread::JoinHandle<()>>,
    pub(super) directory: PathBuf,
    step: u64,
}
impl UiPty {
    pub(super) fn start() -> Self {
        Self::start_with_fault(None)
    }

    pub(super) fn start_with_fault(fault: Option<&str>) -> Self {
        Self::start_with_options(fault, false)
    }

    pub(super) fn start_with_options(fault: Option<&str>, hold_initial_activity: bool) -> Self {
        let directory = env::temp_dir().join(format!(
            "theseus-ui-fixture-{}-{}",
            std::process::id(),
            common::events::OperationId::next().0
        ));
        fs::create_dir_all(&directory).unwrap();
        if hold_initial_activity {
            fs::write(directory.join("hold-initial-activity"), b"").unwrap();
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 18,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env::current_exe().unwrap());
        command.args([
            "--exact",
            "application::managed_ui_tests::fixture_child",
            "--ignored",
            "--nocapture",
        ]);
        command.env("HOME", &directory);
        command.env("SHELL", "/bin/sh");
        command.env("USER", "fixture");
        command.env("TERM", "xterm-256color");
        command.env("THESEUS_UI_FIXTURE", &directory);
        if let Some(fault) = fault {
            command.env("THESEUS_CHANNEL_FAULT", fault);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let input = pair.master.take_writer().unwrap();
        let parser = Arc::new(Mutex::new(vt100::Parser::new(18, 60, 2000)));
        let reader_parser = parser.clone();
        let reader = thread::spawn(move || {
            let mut bytes = [0; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => reader_parser
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .process(&bytes[..count]),
                }
            }
        });
        let mut result = Self {
            child,
            master: pair.master,
            input,
            parser,
            reader: Some(reader),
            directory,
            step: 0,
        };
        result.wait(|screen| screen.contents().contains("fixture "));
        if fault.is_none() {
            result.write("/ask fixture\r");
        }
        if hold_initial_activity {
            result.wait(waiting_spinner_is_visible);
        } else {
            result.wait(|screen| screen.contents().contains("Fixture waiting"));
        }
        result
    }

    pub(super) fn write(&mut self, bytes: &str) {
        self.input.write_all(bytes.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }

    pub(super) fn command(&mut self, action: &str, text: &str) {
        self.step += 1;
        let temporary = self.directory.join("step.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(&json!({"action":action,"text":text})).unwrap(),
        )
        .unwrap();
        fs::rename(
            temporary,
            self.directory.join(format!("{}.json", self.step)),
        )
        .unwrap();
    }

    pub(super) fn wait(&self, predicate: impl Fn(&vt100::Screen) -> bool) {
        let start = Instant::now();
        loop {
            {
                let parser = self.parser.lock().unwrap();
                if predicate(parser.screen()) {
                    return;
                }
                if start.elapsed() >= TIMEOUT {
                    let contents = parser.screen().contents();
                    // Keep the reader draining during unwinding/child cleanup.
                    // Poisoning its mutex here can strand a PTY writer on exit.
                    drop(parser);
                    panic!("PTY timeout; screen:\n{contents}");
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn history(&self) -> Vec<String> {
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let (rows, _) = screen.size();
        screen.set_scrollback(usize::MAX);
        let scrollback = screen.scrollback();
        let mut lines = screen.rows(0, u16::MAX).collect::<Vec<_>>();
        for offset in (0..scrollback).rev() {
            screen.set_scrollback(offset);
            lines.push(screen.rows(0, u16::MAX).nth(rows as usize - 1).unwrap());
        }
        lines
    }

    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }
}

impl Drop for UiPty {
    fn drop(&mut self) {
        let _ = self.input.write_all(b"\x03\x03/exit\r");
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
