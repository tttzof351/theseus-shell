use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(super) struct Spinner {
    stop: Option<Arc<AtomicBool>>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub(super) fn start() -> Self {
        if !io::stdout().is_terminal() {
            return Self {
                stop: None,
                handle: None,
            };
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut index = 0;

            while !thread_stop.load(Ordering::Relaxed) {
                print!("\r{}", frames[index % frames.len()]);
                let _ = io::stdout().flush();
                index += 1;
                thread::sleep(Duration::from_millis(120));
            }
        });

        Self {
            stop: Some(stop),
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }

        if self.stop.is_some() {
            print!("\r{:<96}\r", "");
            let _ = io::stdout().flush();
        }
    }
}
