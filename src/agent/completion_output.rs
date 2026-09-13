//! One presentation owner for a completion, independent of HTTP encoding.

use std::io;

use super::messages::{ChatMessage, TrajectoryMessage};
use crate::common::events::{BlockId, BlockKind, EventSink, Outcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presentation {
    Pending,
    Emitted,
}

#[derive(Debug)]
pub(crate) struct RunResult {
    pub text: String,
    pub presentation: Presentation,
}

impl From<String> for RunResult {
    fn from(text: String) -> Self {
        Self {
            text,
            presentation: Presentation::Pending,
        }
    }
}

#[derive(Debug)]
pub(super) struct CompletionResponse {
    pub trajectory: TrajectoryMessage,
    pub presentation: Presentation,
}

pub(super) struct CompletionOutput {
    sink: Option<EventSink>,
    reasoning: Option<BlockId>,
    reasoning_received: bool,
    truncated: bool,
    content: Option<BlockId>,
    presentation: Presentation,
}

impl CompletionOutput {
    pub fn new(sink: Option<EventSink>) -> Self {
        Self {
            sink,
            reasoning: None,
            reasoning_received: false,
            truncated: false,
            content: None,
            presentation: Presentation::Pending,
        }
    }

    async fn reasoning_block(&mut self) -> io::Result<Option<BlockId>> {
        if self.reasoning.is_none()
            && let Some(sink) = &self.sink
        {
            self.reasoning = Some(sink.start_block_async(BlockKind::Reasoning).await?);
        }
        Ok(self.reasoning)
    }

    pub async fn reasoning(&mut self, text: &str) -> io::Result<()> {
        if !text.is_empty()
            && let Some(id) = self.reasoning_block().await?
        {
            self.reasoning_received = true;
            self.sink.as_ref().unwrap().text_async(id, text).await?;
        }
        Ok(())
    }

    pub async fn content(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() || self.sink.is_none() {
            return Ok(());
        }
        // Reserve the canonical position even when reasoning arrives later.
        self.reasoning_block().await?;
        let sink = self.sink.as_ref().unwrap();
        if self.content.is_none() {
            self.content = Some(sink.start_block_async(BlockKind::Markdown).await?);
        }
        sink.text_async(self.content.unwrap(), text).await?;
        self.presentation = Presentation::Emitted;
        Ok(())
    }

    pub async fn message(&mut self, message: &ChatMessage) -> io::Result<()> {
        let reasoning = message.reasoning_text();
        self.reasoning(&reasoning).await?;
        if let Some(content) = message.content_text() {
            self.content(&content).await?;
        }
        Ok(())
    }

    pub fn truncated(&mut self) {
        self.truncated = true;
    }

    // Called after the HTTP future/Response has been dropped. Terminal events
    // preserve accepted deltas even when regular enqueue was cancelled.
    pub fn finish(&mut self, outcome: Outcome) -> io::Result<Presentation> {
        if let Some(sink) = &self.sink {
            if let Some(id) = self.reasoning.take() {
                sink.finish_block(
                    id,
                    if self.reasoning_received {
                        outcome.clone()
                    } else {
                        Outcome::Completed
                    },
                )?;
            }
            if let Some(id) = self.content.take() {
                sink.finish_block(id, outcome.clone())?;
            }
        }
        if self.truncated
            && outcome == Outcome::Completed
            && let Some(sink) = &self.sink
        {
            sink.message(
                BlockKind::Diagnostic,
                "Response truncated: token limit reached.",
            )?;
        }
        Ok(self.presentation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{cancellation::CancellationEvent, events::OutputEvent};

    #[tokio::test(flavor = "current_thread")]
    async fn content_before_reasoning_keeps_canonical_order_and_finishes_same_blocks() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut output = CompletionOutput::new(Some(sink));
        output.content("first").await.unwrap();
        output.reasoning("late thought").await.unwrap();
        output.content(" second").await.unwrap();
        let preview = events.try_iter().collect::<Vec<_>>();
        assert!(matches!(
            preview[0].event,
            OutputEvent::BlockStarted {
                kind: BlockKind::Reasoning,
                ..
            }
        ));
        assert!(matches!(
            preview[1].event,
            OutputEvent::BlockStarted {
                kind: BlockKind::Markdown,
                ..
            }
        ));
        assert!(
            !preview
                .iter()
                .any(|e| matches!(e.event, OutputEvent::BlockFinished { .. }))
        );
        assert_eq!(
            output.finish(Outcome::Completed).unwrap(),
            Presentation::Emitted
        );
        let finished = events.try_iter().collect::<Vec<_>>();
        assert_eq!(finished.len(), 2);
        for (start, finish) in preview.iter().zip(&finished) {
            let OutputEvent::BlockStarted { id: start, .. } = start.event else {
                panic!()
            };
            assert!(
                matches!(finish.event, OutputEvent::BlockFinished { id, outcome: Outcome::Completed } if id == start)
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_reasoning_reservation_does_not_add_a_second_error() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut output = CompletionOutput::new(Some(sink));
        output.content("retained").await.unwrap();
        output.finish(Outcome::Cancelled).unwrap();
        let events = events.try_iter().collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e.event,
                    OutputEvent::BlockFinished {
                        outcome: Outcome::Cancelled,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(events.iter().filter(|e| matches!(&e.event, OutputEvent::TextAppended { text, .. } if text == "retained")).count(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn absent_sink_preserves_pending_result_for_public_api_and_compact() {
        let mut output = CompletionOutput::new(None);
        output
            .content("private summary or API result")
            .await
            .unwrap();
        output.reasoning("private reasoning").await.unwrap();
        assert_eq!(
            output.finish(Outcome::Completed).unwrap(),
            Presentation::Pending
        );
        assert!(output.reasoning.is_none());
        assert!(output.content.is_none());
    }
}
