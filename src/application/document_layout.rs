//! Latest requested source snapshot and one completed layout, never terminal I/O.
use super::output_document::{OutputDocument, RenderedDocument};
use crate::common::cancellation::CancellationEvent;
use crate::terminal_renderer::{
    IndexedPhysicalLayout, RenderLine, TerminalSize, VirtualCursor, VirtualScreen,
};
use std::{
    io,
    sync::{Arc, Condvar, Mutex},
    thread,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Key {
    pub version: u64,
    pub generation: u64,
    pub width: usize,
}

impl Key {
    pub fn of(document: &OutputDocument, width: usize) -> Self {
        Self {
            version: document.version(),
            generation: document.generation(),
            width,
        }
    }
}

pub(super) struct Prepared {
    pub key: Key,
    pub rendered: RenderedDocument,
    pub screen: VirtualScreen,
    pub layout: IndexedPhysicalLayout,
}

#[derive(Default)]
struct Queue {
    pending: Option<(Key, OutputDocument)>,
    ready: Option<io::Result<Prepared>>,
    stopped: bool,
}

pub(super) struct Worker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
    submitted: Option<Key>,
    pub applied: Option<Key>,
    stop: CancellationEvent,
}

impl Worker {
    pub fn new() -> io::Result<Self> {
        Self::with_checkpoint(|| {})
    }

    fn with_checkpoint(checkpoint: impl Fn() + Send + 'static) -> io::Result<Self> {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker_queue = Arc::clone(&queue);
        let stop = CancellationEvent::new();
        let worker_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("theseus-document-layout".into())
            .spawn(move || {
                let check = || {
                    checkpoint();
                    if worker_stop.is_cancelled() {
                        Err(io::ErrorKind::Interrupted.into())
                    } else {
                        Ok(())
                    }
                };
                let mut previous = None;
                let mut previous_layout = IndexedPhysicalLayout::default();
                loop {
                    let (key, mut document) = {
                        let (lock, ready) = &*worker_queue;
                        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while state.pending.is_none() && !state.stopped {
                            state = ready.wait(state).unwrap_or_else(|e| e.into_inner());
                        }
                        if state.stopped {
                            break;
                        }
                        state.pending.take().unwrap()
                    };
                    let result = crate::common::panic_boundary::catch(|| {
                        check()?;
                        if let Some(previous) = &previous {
                            document.reuse_caches(previous);
                        }
                        let rendered = document.render_checked(key.width, &check)?;
                        check()?;
                        let footer = rendered.lines.len();
                        let mut lines = rendered.lines.clone();
                        lines.push(RenderLine::plain(""));
                        let mut screen = VirtualScreen::from_render_lines(
                            lines,
                            VirtualCursor {
                                line: footer,
                                char_offset: 0,
                            },
                            true,
                        );
                        // Cloning shares immutable line layouts with the UI.
                        // Reflow replaces only changed lines, preserving older
                        // prepared frames and their publication metadata.
                        let mut layout = previous_layout.clone();
                        layout.layout_checked(
                            &screen,
                            TerminalSize::new(key.width as u16, 1),
                            &check,
                        )?;
                        screen.truncate(footer);
                        screen.dirty_from = footer;
                        previous_layout = layout.clone();
                        Ok(Prepared {
                            key,
                            rendered,
                            screen,
                            layout,
                        })
                    })
                    .map_err(|message| {
                        io::Error::other(format!("document layout panicked: {message}"))
                    })
                    .and_then(|result| result);
                    previous = Some(document);
                    let old = {
                        let mut state = worker_queue.0.lock().unwrap_or_else(|e| e.into_inner());
                        if state.stopped {
                            break;
                        }
                        state.ready.replace(result)
                    };
                    drop(old); // Release superseded layouts outside the queue lock.
                }
            })?;
        Ok(Self {
            queue,
            thread: Some(thread),
            submitted: None,
            applied: None,
            stop,
        })
    }

    pub fn request(&mut self, key: Key, document: &OutputDocument) {
        if self.submitted == Some(key) {
            return;
        }
        let snapshot = document.clone();
        let old = self
            .queue
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .replace((key, snapshot));
        self.submitted = Some(key);
        self.queue.1.notify_one();
        drop(old);
    }

    pub fn take(&mut self, current: Key) -> io::Result<Option<Prepared>> {
        let result = self
            .queue
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ready
            .take();
        let Some(result) = result else {
            return Ok(None);
        };
        let result = result?;
        if result.key.width != current.width
            || result.key.generation != current.generation
            || self.applied.is_some_and(|old| {
                old.version >= result.key.version && old.width == result.key.width
            })
        {
            return Ok(None);
        }
        // An append-only prefix may be shown while a newer snapshot is being
        // formatted. Replace and clear invalidate the generation entirely.
        self.applied = Some(result.key);
        Ok(Some(result))
    }

    pub fn pending(&self, current: Key) -> bool {
        self.applied != Some(current)
    }

    #[cfg(test)]
    pub(super) fn has_ready_result(&self) -> bool {
        self.queue.0.lock().unwrap().ready.is_some()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.cancel();
        self.queue
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopped = true;
        self.queue.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{
        cancellation::CancellationEvent,
        events::{BlockKind, EventSink, Outcome, OutputEvent},
    };
    use std::time::{Duration, Instant};

    fn wait_ready(worker: &Worker) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if worker.queue.0.lock().unwrap().ready.is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "layout worker did not finish");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn completion_and_new_output_share_unchanged_layout_without_mutating_preview() {
        let mut document = OutputDocument::default();
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        document.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, &"**UNCHANGED** text\n\n".repeat(100))
            .unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let mut worker = Worker::new().unwrap();
        let key = Key::of(&document, 60);
        worker.request(key, &document);
        wait_ready(&worker);
        let preview = worker.take(key).unwrap().unwrap();
        let body_lines = preview.rendered.lines.len();

        sink.finish(Outcome::Cancelled).unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let key = Key::of(&document, 60);
        worker.request(key, &document);
        wait_ready(&worker);
        let finished = worker.take(key).unwrap().unwrap();
        assert_eq!(finished.rendered.lines, document.render(60).lines);
        assert_eq!(
            finished.layout.reflowed_last_frame, 2,
            "only outcome and footer changed"
        );
        for index in 0..body_lines {
            assert!(Arc::ptr_eq(
                &preview.layout.logical_lines[index],
                &finished.layout.logical_lines[index]
            ));
        }
        assert!(preview.rendered.publication.iter().all(|unit| !unit.stable));
        assert!(
            !preview
                .screen
                .lines
                .iter()
                .any(|line| line.contains("interrupted"))
        );
        assert!(finished.rendered.publication.iter().all(|unit| unit.stable));

        document.append_lines(vec![RenderLine::plain("user> /exit")]);
        let key = Key::of(&document, 60);
        worker.request(key, &document);
        wait_ready(&worker);
        let appended = worker.take(key).unwrap().unwrap();
        assert_eq!(
            appended.layout.reflowed_last_frame, 2,
            "only submission and footer changed"
        );
        for index in 0..finished.rendered.lines.len() {
            assert!(Arc::ptr_eq(
                &finished.layout.logical_lines[index],
                &appended.layout.logical_lines[index]
            ));
        }
        // Keeping old Prepared frames alive must not prevent a fresh reflow.
        let key = Key::of(&document, 20);
        worker.request(key, &document);
        wait_ready(&worker);
        let resized = worker.take(key).unwrap().unwrap();
        assert!(!Arc::ptr_eq(
            &resized.layout.logical_lines[0],
            &appended.layout.logical_lines[0]
        ));
        assert_eq!(resized.rendered.lines, document.render(20).lines);
    }

    #[test]
    fn shutdown_interrupts_active_markdown_and_does_not_start_queued_layout() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };
        let mut document = OutputDocument::default();
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        document.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        let text = (0..60000)
            .map(|i| format!("**WORD_{i:04}** "))
            .collect::<String>();
        sink.text(id, &text).unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut worker = Worker::with_checkpoint(move || {
            if worker_calls.fetch_add(1, Ordering::SeqCst) == 31 {
                // Pause inside real Markdown line formatting, after parsing.
                entered_tx.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .unwrap();
        worker.request(Key::of(&document, 60), &document);
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.request(Key::of(&document, 90), &document);
        let queue = worker.queue.clone();
        let started = Instant::now();
        let shutdown = thread::spawn(move || drop(worker));
        while !queue.0.lock().unwrap().stopped {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(1));
        }
        release.send(()).unwrap();
        shutdown.join().unwrap();
        let elapsed = started.elapsed();
        eprintln!("shutdown during 890 KB Markdown formatting: {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(1),
            "layout cleanup exceeded 1 s: {elapsed:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            32,
            "queued layout ran after shutdown"
        );
        assert!(
            queue.0.lock().unwrap().ready.is_none(),
            "cancelled layout became visible"
        );
    }

    #[test]
    fn resize_discards_ready_layout_and_returns_formatted_current_width() {
        let mut document = OutputDocument::default();
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        document.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(
            id,
            "**formatted** paragraph with repeated words "
                .repeat(8)
                .as_str(),
        )
        .unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let mut worker = Worker::new().unwrap();
        worker.request(Key::of(&document, 60), &document);
        wait_ready(&worker);
        let key = Key::of(&document, 20);
        assert!(worker.take(key).unwrap().is_none());
        worker.request(key, &document);
        wait_ready(&worker);
        let prepared = worker.take(key).unwrap().unwrap();
        let expected = document.render(20);
        assert_eq!(prepared.rendered.lines, expected.lines);
        assert!(
            prepared
                .rendered
                .lines
                .iter()
                .any(|line| line.styles.iter().any(|s| s.bold))
        );
        assert!(!worker.pending(key));
    }

    #[test]
    fn clear_and_replace_discard_completed_old_generations() {
        let mut document = OutputDocument::default();
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        document.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "HIDDEN_OLD\n\n").unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let mut worker = Worker::new().unwrap();
        worker.request(Key::of(&document, 60), &document);
        wait_ready(&worker);
        document.clear_visible();
        assert!(worker.take(Key::of(&document, 60)).unwrap().is_none());
        sink.text(id, "VISIBLE_OLD").unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        worker.request(Key::of(&document, 60), &document);
        wait_ready(&worker);
        sink.emit(OutputEvent::BlockReplaced {
            id,
            revision: 1,
            text: "HIDDEN_OLD\n\n**VISIBLE_NEW**".into(),
        })
        .unwrap();
        sink.finish_block(id, Outcome::Completed).unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let key = Key::of(&document, 60);
        assert!(worker.take(key).unwrap().is_none());
        worker.request(key, &document);
        wait_ready(&worker);
        let prepared = worker.take(key).unwrap().unwrap();
        let text = prepared.screen.lines.join("\n");
        assert!(text.contains("VISIBLE_NEW"), "{text}");
        assert!(
            !text.contains("HIDDEN_OLD") && !text.contains("VISIBLE_OLD"),
            "{text}"
        );
        assert!(prepared.rendered.publication.iter().all(|unit| unit.stable));
    }

    #[test]
    fn append_prefix_remains_preview_until_finished_snapshot_arrives() {
        let mut document = OutputDocument::default();
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        document.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "**PREFIX**").unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let mut worker = Worker::new().unwrap();
        worker.request(Key::of(&document, 60), &document);
        wait_ready(&worker);
        sink.text(id, " TAIL").unwrap();
        sink.finish_block(id, Outcome::Completed).unwrap();
        for event in events.try_iter() {
            assert!(document.apply(event));
        }
        let key = Key::of(&document, 60);
        let prefix = worker.take(key).unwrap().unwrap();
        assert!(prefix.rendered.publication.iter().all(|unit| !unit.stable));
        assert!(!prefix.screen.lines.join("\n").contains("TAIL"));
        assert!(worker.pending(key));
        worker.request(key, &document);
        wait_ready(&worker);
        let finished = worker.take(key).unwrap().unwrap();
        assert!(finished.screen.lines.join("\n").contains("TAIL"));
        assert!(finished.rendered.publication.iter().all(|unit| unit.stable));
        assert!(!worker.pending(key));
    }
}
