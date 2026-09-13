//! Backend output is data. Managed producers never receive a terminal writer.

use std::{
    io,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};

use super::cancellation::CancellationEvent;

pub(crate) const OUTPUT_QUEUE_CAPACITY: usize = 64;
pub(crate) const MAX_EVENT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct OperationId(pub u64);

impl OperationId {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BlockId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Markdown,
    Reasoning,
    ToolPreview,
    ToolOutput,
    Diagnostic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutputEvent {
    Started,
    BlockStarted {
        id: BlockId,
        kind: BlockKind,
    },
    TextAppended {
        id: BlockId,
        text: String,
    },
    BytesAppended {
        id: BlockId,
        stream: u8,
        bytes: Vec<u8>,
    },
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Replacement is part of the tested UI contract; current JSON producers only append"
        )
    )]
    BlockReplaced {
        id: BlockId,
        revision: u64,
        text: String,
    },
    BlockFinished {
        id: BlockId,
        outcome: Outcome,
    },
    Activity {
        phase: String,
        detail: String,
    },
    Finished {
        outcome: Outcome,
    },
}

impl OutputEvent {
    /// Payload-free metadata for lifecycle diagnostics.
    pub(crate) fn identity(&self) -> (&'static str, Option<BlockId>) {
        match self {
            Self::Started => ("started", None),
            Self::BlockStarted { id, .. } => ("block_started", Some(*id)),
            Self::TextAppended { id, .. } => ("text_appended", Some(*id)),
            Self::BytesAppended { id, .. } => ("bytes_appended", Some(*id)),
            Self::BlockReplaced { id, .. } => ("block_replaced", Some(*id)),
            Self::BlockFinished { id, .. } => ("block_finished", Some(*id)),
            Self::Activity { .. } => ("activity", None),
            Self::Finished { .. } => ("finished", None),
        }
    }

    pub(crate) fn payload_bytes(&self) -> usize {
        match self {
            OutputEvent::TextAppended { text, .. } | OutputEvent::BlockReplaced { text, .. } => {
                text.len()
            }
            OutputEvent::BytesAppended { bytes, .. } => bytes.len(),
            OutputEvent::Activity { phase, detail } => phase.len() + detail.len(),
            OutputEvent::BlockFinished {
                outcome: Outcome::Failed(error),
                ..
            }
            | OutputEvent::Finished {
                outcome: Outcome::Failed(error),
            } => error.len(),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BackendEvent {
    pub operation: OperationId,
    pub sequence: u64,
    pub event: OutputEvent,
}

#[derive(Debug)]
struct Ingress {
    tx: SyncSender<BackendEvent>,
    sequence: u64,
    finished: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct EventSink {
    operation: OperationId,
    ingress: Arc<Mutex<Ingress>>,
    cancellation: CancellationEvent,
    next_block: Arc<AtomicU64>,
}

impl EventSink {
    pub(crate) fn channel(cancellation: CancellationEvent) -> (Self, Receiver<BackendEvent>) {
        Self::with_capacity(cancellation, OUTPUT_QUEUE_CAPACITY)
    }

    fn with_capacity(
        cancellation: CancellationEvent,
        capacity: usize,
    ) -> (Self, Receiver<BackendEvent>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (
            Self {
                operation: OperationId::next(),
                ingress: Arc::new(Mutex::new(Ingress {
                    tx,
                    sequence: 0,
                    finished: false,
                })),
                cancellation,
                next_block: Arc::new(AtomicU64::new(1)),
            },
            rx,
        )
    }

    pub(crate) fn operation(&self) -> OperationId {
        self.operation
    }

    pub(crate) fn emit(&self, event: OutputEvent) -> io::Result<()> {
        self.send(event, false)
    }

    // The guard covers one enqueue attempt only. Both sync and async producers
    // share this ordering point; queue pressure never holds the ingress lock.
    fn try_enqueue(&self, event: OutputEvent, terminal: bool) -> io::Result<Option<OutputEvent>> {
        if event.payload_bytes() > MAX_EVENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output event exceeds payload budget",
            ));
        }
        if !terminal && self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "output cancelled",
            ));
        }
        let mut ingress = match self.ingress.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return Ok(Some(event)),
            Err(TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("output ingress poisoned"));
            }
        };
        if ingress.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "operation output is finished",
            ));
        }
        let is_finish = matches!(event, OutputEvent::Finished { .. });
        let message = BackendEvent {
            operation: self.operation,
            sequence: ingress.sequence + 1,
            event,
        };
        match ingress.tx.try_send(message) {
            Ok(()) => {
                ingress.sequence += 1;
                ingress.finished = is_finish;
                Ok(None)
            }
            Err(TrySendError::Full(pending)) => Ok(Some(pending.event)),
            Err(TrySendError::Disconnected(_)) => {
                self.cancellation.cancel();
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "output consumer stopped",
                ))
            }
        }
    }

    fn send(&self, mut event: OutputEvent, terminal: bool) -> io::Result<()> {
        while let Some(pending) = self.try_enqueue(event, terminal)? {
            event = pending;
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    pub(crate) async fn emit_async(&self, mut event: OutputEvent) -> io::Result<()> {
        while let Some(pending) = self.try_enqueue(event, false)? {
            event = pending;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok(())
    }

    pub(crate) fn start_block(&self, kind: BlockKind) -> io::Result<BlockId> {
        let id = BlockId(self.next_block.fetch_add(1, Ordering::Relaxed));
        self.emit(OutputEvent::BlockStarted { id, kind })?;
        Ok(id)
    }

    pub(crate) async fn start_block_async(&self, kind: BlockKind) -> io::Result<BlockId> {
        let id = BlockId(self.next_block.fetch_add(1, Ordering::Relaxed));
        self.emit_async(OutputEvent::BlockStarted { id, kind })
            .await?;
        Ok(id)
    }

    pub(crate) async fn text_async(&self, id: BlockId, mut text: &str) -> io::Result<()> {
        while !text.is_empty() {
            let mut end = text.len().min(MAX_EVENT_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.emit_async(OutputEvent::TextAppended {
                id,
                text: text[..end].into(),
            })
            .await?;
            text = &text[end..];
        }
        Ok(())
    }

    pub(crate) async fn activity_async(&self, phase: &str, detail: &str) -> io::Result<()> {
        self.emit_async(OutputEvent::Activity {
            phase: bounded_diagnostic(phase, MAX_EVENT_BYTES / 2),
            detail: bounded_diagnostic(detail, MAX_EVENT_BYTES / 2),
        })
        .await
    }

    pub(crate) fn text(&self, id: BlockId, text: &str) -> io::Result<()> {
        let mut remaining = text;
        while !remaining.is_empty() {
            let mut end = remaining.len().min(MAX_EVENT_BYTES);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            self.emit(OutputEvent::TextAppended {
                id,
                text: remaining[..end].to_string(),
            })?;
            remaining = &remaining[end..];
        }
        Ok(())
    }

    pub(crate) fn bytes(&self, id: BlockId, stream: u8, bytes: &[u8]) -> io::Result<()> {
        for chunk in bytes.chunks(MAX_EVENT_BYTES) {
            self.emit(OutputEvent::BytesAppended {
                id,
                stream,
                bytes: chunk.to_vec(),
            })?;
        }
        Ok(())
    }

    pub(crate) fn finish_block(&self, id: BlockId, outcome: Outcome) -> io::Result<()> {
        self.send(
            OutputEvent::BlockFinished {
                id,
                outcome: bounded_outcome(outcome),
            },
            true,
        )
    }

    pub(crate) fn finish(&self, outcome: Outcome) -> io::Result<()> {
        self.send(
            OutputEvent::Finished {
                outcome: bounded_outcome(outcome),
            },
            true,
        )
    }

    pub(crate) fn activity(&self, phase: &str, detail: &str) -> io::Result<()> {
        self.emit(OutputEvent::Activity {
            phase: bounded_diagnostic(phase, MAX_EVENT_BYTES / 2),
            detail: bounded_diagnostic(detail, MAX_EVENT_BYTES / 2),
        })
    }

    pub(crate) fn message(&self, kind: BlockKind, text: &str) -> io::Result<()> {
        let id = self.start_block(kind)?;
        if matches!(kind, BlockKind::ToolOutput | BlockKind::ToolPreview) {
            self.bytes(id, 0, text.as_bytes())?;
        } else {
            self.text(id, text)?;
        }
        self.finish_block(id, Outcome::Completed)
    }
}

fn bounded_diagnostic(text: &str, budget: usize) -> String {
    super::text::truncate_utf8_to_bytes(
        text,
        budget.saturating_sub(64),
        super::text::TruncatePosition::End,
    )
}

fn bounded_outcome(outcome: Outcome) -> Outcome {
    match outcome {
        Outcome::Failed(error) => Outcome::Failed(bounded_diagnostic(&error, MAX_EVENT_BYTES)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn full_async_queue_yields_to_deadline_and_does_not_consume_sequence() {
        let cancellation = CancellationEvent::new();
        let (sink, rx) = EventSink::with_capacity(cancellation.clone(), 1);
        sink.emit(OutputEvent::Started).unwrap();
        // A blocking implementation is released by the watchdog, then fails
        // the latency assertion instead of stranding the test executable.
        let (done, finished) = mpsc::channel();
        let watchdog = thread::spawn(move || {
            if finished.recv_timeout(Duration::from_secs(1)).is_err() {
                cancellation.cancel();
            }
        });
        let started = std::time::Instant::now();
        let timeout = tokio::time::timeout(
            Duration::from_millis(20),
            sink.activity_async("pending", ""),
        )
        .await;
        let _ = done.send(());
        watchdog.join().unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(timeout.is_err());
        assert_eq!(rx.recv().unwrap().sequence, 1);
        sink.activity_async("accepted", "").await.unwrap();
        assert_eq!(rx.recv().unwrap().sequence, 2);
        sink.finish(Outcome::Completed).unwrap();
        assert_eq!(rx.recv().unwrap().sequence, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_queue_cancel_and_disconnect_keep_accepted_data() {
        for disconnect in [false, true] {
            let cancellation = CancellationEvent::new();
            let (sink, rx) = EventSink::with_capacity(cancellation.clone(), 1);
            sink.emit(OutputEvent::Started).unwrap();
            let started = std::time::Instant::now();
            let consumer = thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                if disconnect {
                    drop(rx);
                    None
                } else {
                    cancellation.cancel();
                    Some(rx)
                }
            });
            let error = sink.activity_async("pending", "").await.unwrap_err();
            let rx = consumer.join().unwrap();
            assert!(started.elapsed() < Duration::from_millis(250));
            assert_eq!(
                error.kind(),
                if disconnect {
                    io::ErrorKind::BrokenPipe
                } else {
                    io::ErrorKind::Interrupted
                }
            );
            if let Some(rx) = rx {
                assert_eq!(rx.recv().unwrap().event, OutputEvent::Started);
                assert!(rx.try_recv().is_err());
                sink.finish(Outcome::Cancelled).unwrap();
                assert_eq!(rx.recv().unwrap().sequence, 2);
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_sync_async_producers_share_order_and_utf8_payload_limits() {
        let (sink, rx) = EventSink::with_capacity(CancellationEvent::new(), 2);
        let consumer = thread::spawn(move || {
            let mut events = Vec::new();
            while let Ok(event) = rx.recv_timeout(Duration::from_secs(2)) {
                let finished = matches!(event.event, OutputEvent::Finished { .. });
                events.push(event);
                if finished {
                    break;
                }
            }
            events
        });
        let text_id = sink.start_block_async(BlockKind::Markdown).await.unwrap();
        let sync = sink.clone();
        let producer = thread::spawn(move || {
            let id = sync.start_block(BlockKind::ToolOutput).unwrap();
            for byte in 0..80 {
                sync.bytes(id, 0, &[byte]).unwrap();
            }
            sync.finish_block(id, Outcome::Completed).unwrap();
        });
        let text = "界🙂".repeat(MAX_EVENT_BYTES);
        sink.text_async(text_id, &text).await.unwrap();
        producer.join().unwrap();
        sink.finish_block(text_id, Outcome::Completed).unwrap();
        sink.finish(Outcome::Completed).unwrap();
        let events = consumer.join().unwrap();
        let mut rendered = String::new();
        let mut bytes = Vec::new();
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.sequence, i as u64 + 1);
            assert!(event.event.payload_bytes() <= MAX_EVENT_BYTES);
            match &event.event {
                OutputEvent::TextAppended { text, .. } => rendered.push_str(text),
                OutputEvent::BytesAppended { bytes: data, .. } => bytes.extend_from_slice(data),
                _ => {}
            }
        }
        assert_eq!(rendered, text);
        assert_eq!(bytes, (0..80u8).collect::<Vec<_>>());
        assert!(matches!(
            events.last().unwrap().event,
            OutputEvent::Finished { .. }
        ));
        assert_eq!(
            sink.activity_async("late", "").await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_ingress_lock_contention_yields_and_oversize_is_rejected() {
        let (sink, rx) = EventSink::channel(CancellationEvent::new());
        let ingress = sink.ingress.clone();
        let (held_tx, held) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let locker = thread::spawn(move || {
            let _guard = ingress.lock().unwrap();
            held_tx.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(1)).unwrap();
        });
        held.recv().unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            sink.emit_async(OutputEvent::Started),
        )
        .await;
        release.send(()).unwrap();
        locker.join().unwrap();
        assert!(result.is_err());
        assert!(rx.try_recv().is_err());
        let error = sink
            .emit_async(OutputEvent::BlockReplaced {
                id: BlockId(1),
                revision: 1,
                text: "x".repeat(MAX_EVENT_BYTES + 1),
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        sink.emit_async(OutputEvent::Started).await.unwrap();
        assert_eq!(rx.recv().unwrap().sequence, 1);
    }

    #[test]
    fn cancelling_a_full_queue_releases_the_producer_without_losing_queued_data() {
        let cancellation = CancellationEvent::new();
        let (sink, rx) = EventSink::with_capacity(cancellation.clone(), 1);
        sink.emit(OutputEvent::Started).unwrap();
        let worker = thread::spawn(move || sink.activity("Waiting", ""));
        cancellation.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(rx.recv().unwrap().event, OutputEvent::Started);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn concurrent_producers_have_one_order_and_completion_follows_all_bytes() {
        let (sink, rx) = EventSink::channel(CancellationEvent::new());
        let id = sink.start_block(BlockKind::ToolOutput).unwrap();
        let threads = (0..2)
            .map(|stream| {
                let sink = sink.clone();
                thread::spawn(move || {
                    for n in 0..10 {
                        sink.bytes(id, stream, &[n]).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in threads {
            worker.join().unwrap();
        }
        sink.finish(Outcome::Completed).unwrap();
        assert_eq!(
            sink.activity("late", "").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        let events = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(events.len(), 22);
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.sequence, i as u64 + 1);
        }
        for stream in 0..2 {
            let bytes = events
                .iter()
                .filter_map(|e| match &e.event {
                    OutputEvent::BytesAppended {
                        stream: s, bytes, ..
                    } if *s == stream => Some(bytes[0]),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(bytes, (0..10).collect::<Vec<_>>());
        }
    }
}
