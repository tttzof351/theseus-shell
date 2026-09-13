//! Backend output is data. Managed producers never receive a terminal writer.

use std::{
    io,
    sync::{
        Arc, Mutex,
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
    next_block: u64,
    finished: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct EventSink {
    operation: OperationId,
    ingress: Arc<Mutex<Ingress>>,
    cancellation: CancellationEvent,
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
                    next_block: 0,
                    finished: false,
                })),
                cancellation,
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

    fn send(&self, event: OutputEvent, terminal: bool) -> io::Result<()> {
        let payload_bytes = event.payload_bytes();
        if payload_bytes > MAX_EVENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output event exceeds payload budget",
            ));
        }
        // This mutex orders concurrent stdout/stderr producers. Cancellation
        // itself is an independent atomic, never queued behind output.
        let mut ingress = self
            .ingress
            .lock()
            .map_err(|_| io::Error::other("output ingress poisoned"))?;
        if ingress.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "operation output is finished",
            ));
        }
        let is_finish = matches!(event, OutputEvent::Finished { .. });
        let mut message = BackendEvent {
            operation: self.operation,
            sequence: ingress.sequence + 1,
            event,
        };
        loop {
            if !terminal && self.cancellation.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "output cancelled",
                ));
            }
            match ingress.tx.try_send(message) {
                Ok(()) => {
                    ingress.sequence += 1;
                    ingress.finished = is_finish;
                    return Ok(());
                }
                Err(TrySendError::Full(pending)) => {
                    message = pending;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.cancellation.cancel();
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "output consumer stopped",
                    ));
                }
            }
        }
    }

    pub(crate) fn start_block(&self, kind: BlockKind) -> io::Result<BlockId> {
        let id = {
            let mut ingress = self
                .ingress
                .lock()
                .map_err(|_| io::Error::other("output ingress poisoned"))?;
            ingress.next_block += 1;
            BlockId(ingress.next_block)
        };
        self.emit(OutputEvent::BlockStarted { id, kind })?;
        Ok(id)
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
