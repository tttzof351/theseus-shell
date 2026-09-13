//! Append-only presentation for pipes: no cursor commands and no replay.

use super::ansi_decoder::AnsiDecoder;
use crate::common::events::{BackendEvent, BlockId, BlockKind, OperationId, OutputEvent};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Write},
};

pub(super) fn write_diagnostic(writer: &mut impl Write, text: &str) -> io::Result<()> {
    for line in super::ansi::ansi_render_lines(text) {
        writeln!(writer, "{}", line.text)?;
    }
    writer.flush()
}

struct Block {
    kind: BlockKind,
    text: String,
    ansi: AnsiDecoder,
    revision: u64,
    finished: bool,
}

#[derive(Default)]
pub(super) struct PlainFrontend {
    operation: Option<OperationId>,
    sequence: u64,
    finished: bool,
    blocks: HashMap<BlockId, Block>,
    order: VecDeque<BlockId>,
    seen: HashSet<BlockId>,
    pub(super) rejected_events: usize,
}

impl PlainFrontend {
    pub(super) fn apply(
        &mut self,
        envelope: BackendEvent,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<()> {
        if self.operation != Some(envelope.operation) {
            if !matches!(envelope.event, OutputEvent::Started)
                || self
                    .operation
                    .is_some_and(|previous| !self.finished || previous.0 >= envelope.operation.0)
            {
                self.rejected_events += 1;
                return Ok(());
            }
            *self = Self {
                operation: Some(envelope.operation),
                rejected_events: self.rejected_events,
                ..Self::default()
            };
        }
        if self.finished || envelope.sequence != self.sequence + 1 {
            self.rejected_events += 1;
            return Ok(());
        }
        self.sequence = envelope.sequence;
        if let Some(id) = envelope.event.identity().1 {
            if !matches!(envelope.event, OutputEvent::BlockStarted { .. })
                && self.blocks.get(&id).is_none_or(|block| block.finished)
            {
                self.rejected_events += 1;
                return Ok(());
            }
            if let Some(block) = self.blocks.get(&id) {
                let bytes = matches!(block.kind, BlockKind::ToolOutput | BlockKind::ToolPreview);
                let incompatible = match &envelope.event {
                    OutputEvent::TextAppended { .. } | OutputEvent::BlockReplaced { .. } => bytes,
                    OutputEvent::BytesAppended { .. } => !bytes,
                    _ => false,
                };
                if incompatible {
                    self.rejected_events += 1;
                    return Ok(());
                }
            }
        }
        match envelope.event {
            OutputEvent::BlockStarted { id, kind } => {
                if !self.seen.insert(id) {
                    self.rejected_events += 1;
                    return Ok(());
                }
                self.order.push_back(id);
                self.blocks.insert(
                    id,
                    Block {
                        kind,
                        text: String::new(),
                        ansi: AnsiDecoder::default(),
                        revision: 0,
                        finished: false,
                    },
                );
            }
            OutputEvent::TextAppended { id, text } => {
                if let Some(block) = self.blocks.get_mut(&id).filter(|b| !b.finished) {
                    block.text.push_str(&text);
                }
            }
            OutputEvent::BlockReplaced { id, revision, text } => {
                if let Some(block) = self.blocks.get_mut(&id).filter(|b| !b.finished) {
                    if revision > block.revision {
                        block.text = text;
                        block.revision = revision;
                    } else {
                        self.rejected_events += 1;
                    }
                }
            }
            OutputEvent::BytesAppended { id, stream, bytes } => {
                if let Some(block) = self.blocks.get_mut(&id).filter(|b| !b.finished) {
                    block.ansi.push_stream(stream, &bytes);
                }
                self.drain(stdout, stderr)?;
            }
            OutputEvent::BlockFinished { id, .. } => {
                if let Some(block) = self.blocks.get_mut(&id) {
                    block.finished = true;
                }
                self.drain(stdout, stderr)?;
            }
            OutputEvent::Finished { .. } => {
                self.finish(stdout, stderr)?;
            }
            OutputEvent::Started | OutputEvent::Activity { .. } => {}
        }
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<()> {
        for block in self.blocks.values_mut() {
            block.finished = true;
        }
        self.drain(stdout, stderr)?;
        self.finished = true;
        Ok(())
    }

    fn drain(&mut self, stdout: &mut impl Write, stderr: &mut impl Write) -> io::Result<()> {
        while let Some(id) = self.order.front().copied() {
            let block = self.blocks.get_mut(&id).expect("queued block");
            if matches!(block.kind, BlockKind::ToolOutput | BlockKind::ToolPreview) {
                for line in block.ansi.take_complete_lines() {
                    writeln!(stdout, "{}", line.text)?;
                }
                stdout.flush()?;
            }
            if !block.finished {
                break;
            }
            self.order.pop_front();
            let block = self.blocks.remove(&id).unwrap();
            Self::write_block(block, stdout, stderr)?;
        }
        Ok(())
    }

    fn write_block(
        mut block: Block,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<()> {
        if matches!(block.kind, BlockKind::ToolOutput | BlockKind::ToolPreview) {
            block.ansi.finish();
            for line in block.ansi.lines() {
                writeln!(stdout, "{}", line.text)?;
            }
            stdout.flush()
        } else {
            // A replaceable text block commits once. JSON responses already
            // arrive as a single complete block; tool lines above are live.
            block.ansi.push(block.text.as_bytes());
            block.ansi.finish();
            let writer: &mut dyn Write = if block.kind == BlockKind::Diagnostic {
                stderr
            } else {
                stdout
            };
            for line in block.ansi.lines() {
                writeln!(writer, "{}", line.text)?;
            }
            writer.flush()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{
        cancellation::CancellationEvent,
        events::{EventSink, Outcome},
    };

    #[test]
    fn completion_diagnostic_strips_controls_and_preserves_line_breaks() {
        let mut stderr = Vec::new();
        write_diagnostic(
            &mut stderr,
            "agent: \x1b[31mfailed\x1b[0m\n\x1b]0;TITLE\x07try again\x1b[?1049h\n",
        )
        .unwrap();
        assert_eq!(stderr, b"agent: failed\ntry again\n");
    }

    #[test]
    fn foreign_started_cannot_discard_pending_output_or_reopen_an_old_operation() {
        let (first, first_events) = EventSink::channel(CancellationEvent::new());
        let (second, second_events) = EventSink::channel(CancellationEvent::new());
        first.emit(OutputEvent::Started).unwrap();
        let first_started = first_events.recv().unwrap();
        let id = first.start_block(BlockKind::Markdown).unwrap();
        first.text(id, "kept").unwrap();
        second.emit(OutputEvent::Started).unwrap();
        let second_started = second_events.recv().unwrap();
        let mut plain = PlainFrontend::default();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        plain
            .apply(first_started.clone(), &mut out, &mut err)
            .unwrap();
        for event in first_events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        plain
            .apply(second_started.clone(), &mut out, &mut err)
            .unwrap();
        assert_eq!(plain.rejected_events, 1);
        first.finish(Outcome::Completed).unwrap();
        for event in first_events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert_eq!(out, b"kept\n");
        plain.apply(second_started, &mut out, &mut err).unwrap();
        second.message(BlockKind::Markdown, "next").unwrap();
        second.finish(Outcome::Completed).unwrap();
        for event in second_events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        plain.apply(first_started, &mut out, &mut err).unwrap();
        assert_eq!(plain.rejected_events, 2);
        assert_eq!(out, b"kept\nnext\n");
        assert!(err.is_empty());
    }

    #[test]
    fn plain_and_document_reject_incompatible_payload_types_consistently() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = super::super::output_document::OutputDocument::default();
        doc.start_operation(sink.operation());
        sink.emit(OutputEvent::Started).unwrap();
        let tool = sink.start_block(BlockKind::ToolOutput).unwrap();
        let text = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(tool, "invalid tool text").unwrap();
        sink.emit(OutputEvent::BlockReplaced {
            id: tool,
            revision: 1,
            text: "invalid replacement".into(),
        })
        .unwrap();
        sink.bytes(text, 0, b"invalid Markdown bytes").unwrap();
        sink.bytes(tool, 0, b"tool\n").unwrap();
        sink.text(text, "answer").unwrap();
        sink.finish(Outcome::Completed).unwrap();
        let mut plain = PlainFrontend::default();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for event in events.try_iter() {
            doc.apply(event.clone());
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert_eq!(plain.rejected_events, 3);
        assert_eq!(doc.rejected_events, plain.rejected_events);
        assert_eq!(out, b"tool\nanswer\n");
        assert!(err.is_empty());
    }

    #[test]
    fn pipe_output_is_once_only_without_ansi_and_diagnostics_use_stderr() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        sink.emit(OutputEvent::Started).unwrap();
        let id = sink.start_block(BlockKind::ToolOutput).unwrap();
        sink.bytes(id, 0, b"\x1b[31mprogress\r\x1b[2Kdone\npartial")
            .unwrap();
        sink.finish_block(id, Outcome::Completed).unwrap();
        sink.message(BlockKind::Markdown, "**answer**").unwrap();
        sink.message(BlockKind::Diagnostic, "warning").unwrap();
        sink.finish(Outcome::Completed).unwrap();
        let mut plain = PlainFrontend::default();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for event in events.try_iter() {
            plain.apply(event.clone(), &mut out, &mut err).unwrap();
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "done\npartial\n**answer**\n"
        );
        assert_eq!(String::from_utf8(err).unwrap(), "warning\n");
    }

    #[test]
    fn later_tool_output_waits_for_replaceable_text_then_streams_in_document_order() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        sink.emit(OutputEvent::Started).unwrap();
        let text = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(text, "obsolete draft").unwrap();
        let tool = sink.start_block(BlockKind::ToolOutput).unwrap();
        sink.bytes(tool, 0, b"tool line\npartial").unwrap();
        let answer = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(answer, "final answer").unwrap();
        sink.finish_block(answer, Outcome::Completed).unwrap();
        let mut plain = PlainFrontend::default();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for event in events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert!(out.is_empty(), "later blocks overtook an open text block");
        sink.emit(OutputEvent::BlockReplaced {
            id: text,
            revision: 1,
            text: "intro".into(),
        })
        .unwrap();
        sink.finish_block(text, Outcome::Completed).unwrap();
        for event in events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert_eq!(out, b"intro\ntool line\n");
        sink.bytes(tool, 0, b" tail\n").unwrap();
        sink.finish_block(tool, Outcome::Completed).unwrap();
        // A later sequence does not reopen a sealed block or reuse its id.
        sink.text(answer, "late text").unwrap();
        sink.emit(OutputEvent::BlockStarted {
            id: text,
            kind: BlockKind::Markdown,
        })
        .unwrap();
        sink.text(text, "duplicate").unwrap();
        sink.finish(Outcome::Completed).unwrap();
        for event in events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert_eq!(out, b"intro\ntool line\npartial tail\nfinal answer\n");
        assert!(err.is_empty());
    }

    #[test]
    fn disconnected_operation_flushes_partial_blocks_once_and_rejects_late_text() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        sink.emit(OutputEvent::Started).unwrap();
        let text = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(text, "partial answer").unwrap();
        let tool = sink.start_block(BlockKind::ToolOutput).unwrap();
        sink.bytes(tool, 0, b"tool prefix \xe7").unwrap();
        let mut plain = PlainFrontend::default();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for event in events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert!(out.is_empty());
        plain.finish(&mut out, &mut err).unwrap();
        plain.finish(&mut out, &mut err).unwrap();
        sink.text(text, "late payload").unwrap();
        for event in events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            "partial answer\ntool prefix �\n"
        );
        assert_eq!(plain.rejected_events, 1);
        let (next, events) = EventSink::channel(CancellationEvent::new());
        next.emit(OutputEvent::Started).unwrap();
        next.message(BlockKind::Markdown, "next answer").unwrap();
        next.finish(Outcome::Completed).unwrap();
        for event in events.try_iter() {
            plain.apply(event, &mut out, &mut err).unwrap();
        }
        assert_eq!(
            plain.rejected_events, 1,
            "a new operation must not reset rejection accounting"
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "partial answer\ntool prefix �\nnext answer\n"
        );
        assert!(err.is_empty());
    }

    #[test]
    fn writer_failure_propagates_instead_of_panicking() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        sink.emit(OutputEvent::Started).unwrap();
        sink.message(BlockKind::Markdown, "answer").unwrap();
        let mut plain = PlainFrontend::default();
        let error = events
            .try_iter()
            .find_map(|event| plain.apply(event, &mut Closed, &mut Vec::new()).err())
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
