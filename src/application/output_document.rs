//! Persistent semantic output with revisioned, disposable rendering caches.

use std::{collections::HashMap, io, sync::Arc};

use super::{
    RenderLine, TerminalColor, ansi::markdown_lines_with_groups, ansi_decoder::AnsiDecoder,
};
use crate::common::events::{BackendEvent, BlockId, BlockKind, OperationId, Outcome, OutputEvent};
use crate::terminal_renderer::managed::{LineOrigins, PublicationUnit, RowIdentity};

#[derive(Debug, Clone)]
enum Source {
    Text { text: String, visible_from: usize },
    Ansi(AnsiDecoder),
    Prepared(Vec<RenderLine>),
}

#[derive(Debug, Clone)]
struct Block {
    identity: u64,
    groups: Vec<usize>,
    origins: Option<Vec<LineOrigins>>,
    operation: Option<OperationId>,
    id: BlockId,
    kind: BlockKind,
    source: Source,
    // Source revision only: lifecycle changes don't reformat unchanged text.
    revision: u64,
    replacement_revision: u64,
    outcome: Option<Outcome>,
    cache: Option<(u64, usize, Arc<[RenderLine]>)>,
}

impl Block {
    fn lines(
        &mut self,
        width: usize,
        check: &impl Fn() -> io::Result<()>,
    ) -> io::Result<&[RenderLine]> {
        check()?;
        if self
            .cache
            .as_ref()
            .is_none_or(|(revision, cached_width, _)| {
                *revision != self.revision || *cached_width != width
            })
        {
            let mut lines = match &self.source {
                Source::Prepared(lines) => {
                    self.groups = (0..lines.len()).collect();
                    lines.clone()
                }
                Source::Ansi(decoder) => {
                    let lines = decoder.lines();
                    self.groups = (0..lines.len()).collect();
                    lines
                }
                Source::Text { text, visible_from } => {
                    let visible_from = (*visible_from).min(text.len());
                    let visible = &text[visible_from..];
                    // Source text is never trusted to move the physical cursor.
                    let keep = |ch: &char| !ch.is_control() || matches!(ch, '\n' | '\t');
                    let source = if self.kind == BlockKind::Markdown {
                        text.as_str()
                    } else {
                        visible
                    };
                    let safe = source.chars().filter(keep).collect::<String>();
                    if self.kind == BlockKind::Markdown {
                        let visible_offset = text[..visible_from]
                            .chars()
                            .filter(keep)
                            .map(char::len_utf8)
                            .sum();
                        let (lines, groups, origins) =
                            markdown_lines_with_groups(&safe, visible_offset, width, check)?;
                        self.groups = groups;
                        self.origins = Some(origins);
                        lines
                    } else {
                        let lines = safe.lines().map(RenderLine::plain).collect::<Vec<_>>();
                        self.groups = (0..lines.len()).collect();
                        lines
                    }
                }
            };
            if self.kind == BlockKind::Reasoning {
                for line in &mut lines {
                    for style in &mut line.styles {
                        style.foreground = Some(TerminalColor::BrightBlack);
                        style.italic = true;
                    }
                }
            }
            self.cache = Some((self.revision, width, lines.into()));
        }
        Ok(&self.cache.as_ref().unwrap().2)
    }
}

#[derive(Debug, Clone, Default)]
struct OperationState {
    sequence: u64,
    finished: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct OutputDocument {
    blocks: Vec<Block>,
    next_identity: u64,
    operations: HashMap<OperationId, OperationState>,
    pub(super) activity: Option<(String, String)>,
    pub(super) rejected_events: usize,
    dirty: bool,
    rendered_width: Option<usize>,
    version: u64,
    generation: u64,
}

pub(super) struct RenderedDocument {
    pub(super) lines: Vec<RenderLine>,
    pub(super) stable_lines: usize,
    pub(super) publication: Vec<PublicationUnit>,
}

impl OutputDocument {
    pub(super) fn version(&self) -> u64 {
        self.version
    }
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }
    pub(super) fn source_bytes(&self) -> usize {
        self.blocks
            .iter()
            .map(|block| match &block.source {
                Source::Text { text, .. } => text.len(),
                Source::Prepared(lines) => lines.iter().map(|line| line.text.len()).sum(),
                Source::Ansi(_) => 0,
            })
            .sum()
    }
    pub(super) fn reuse_caches(&mut self, previous: &Self) {
        let previous = previous
            .blocks
            .iter()
            .map(|block| (block.identity, block))
            .collect::<HashMap<_, _>>();
        for block in &mut self.blocks {
            if let Some(old) = previous
                .get(&block.identity)
                .filter(|old| old.revision == block.revision)
            {
                block.cache = old.cache.clone();
                block.groups = old.groups.clone();
                block.origins = old.origins.clone();
            }
        }
    }
    pub(super) fn append_lines(&mut self, lines: Vec<RenderLine>) -> Option<u64> {
        if lines.is_empty() {
            return None;
        }
        self.dirty = true;
        self.version += 1;
        self.next_identity += 1;
        self.blocks.push(Block {
            identity: self.next_identity,
            groups: Vec::new(),
            origins: None,
            operation: None,
            id: BlockId(0),
            kind: BlockKind::Diagnostic,
            source: Source::Prepared(lines),
            revision: 0,
            replacement_revision: 0,
            outcome: Some(Outcome::Completed),
            cache: None,
        });
        Some(self.next_identity)
    }

    pub(super) fn start_operation(&mut self, operation: OperationId) {
        self.operations.entry(operation).or_default();
    }

    pub(super) fn finish_operation(&mut self, operation: OperationId, outcome: Outcome) {
        let Some(state) = self.operations.get_mut(&operation) else {
            return;
        };
        if state.finished {
            return;
        }
        state.finished = true;
        self.version += 1;
        self.dirty = true;
        self.activity = None;
        for block in &mut self.blocks {
            if block.operation == Some(operation) && block.outcome.is_none() {
                if let Source::Ansi(decoder) = &mut block.source {
                    decoder.finish();
                    block.revision += 1;
                }
                block.outcome = Some(outcome.clone());
            }
        }
    }

    pub(super) fn apply(&mut self, envelope: BackendEvent) -> bool {
        let Some(state) = self.operations.get_mut(&envelope.operation) else {
            self.rejected_events += 1;
            return false;
        };
        if state.finished || envelope.sequence != state.sequence + 1 {
            self.rejected_events += 1;
            return false;
        }
        state.sequence = envelope.sequence;
        self.dirty = true;
        let operation = envelope.operation;
        match envelope.event {
            OutputEvent::Started => {}
            OutputEvent::Activity { phase, detail } => {
                self.activity = Some((
                    super::ansi::terminal_label(&phase),
                    super::ansi::terminal_label(&detail),
                ));
            }
            OutputEvent::Finished { outcome } => {
                self.finish_operation(operation, outcome);
            }
            OutputEvent::BlockStarted { id, kind } => {
                if self
                    .blocks
                    .iter()
                    .any(|b| b.operation == Some(operation) && b.id == id)
                {
                    self.rejected_events += 1;
                    return false;
                }
                self.next_identity += 1;
                self.blocks.push(Block {
                    identity: self.next_identity,
                    groups: Vec::new(),
                    origins: None,
                    operation: Some(operation),
                    id,
                    kind,
                    source: if matches!(kind, BlockKind::ToolOutput | BlockKind::ToolPreview) {
                        Source::Ansi(AnsiDecoder::default())
                    } else {
                        Source::Text {
                            text: String::new(),
                            visible_from: 0,
                        }
                    },
                    revision: 0,
                    replacement_revision: 0,
                    outcome: None,
                    cache: None,
                });
            }
            event => {
                let id = match &event {
                    OutputEvent::TextAppended { id, .. }
                    | OutputEvent::BytesAppended { id, .. }
                    | OutputEvent::BlockReplaced { id, .. }
                    | OutputEvent::BlockFinished { id, .. } => *id,
                    _ => unreachable!(),
                };
                let Some(block) = self
                    .blocks
                    .iter_mut()
                    .find(|b| b.operation == Some(operation) && b.id == id && b.outcome.is_none())
                else {
                    self.rejected_events += 1;
                    return false;
                };
                let source_changed = match (event, &mut block.source) {
                    (OutputEvent::TextAppended { text: delta, .. }, Source::Text { text, .. }) => {
                        text.push_str(&delta);
                        true
                    }
                    (OutputEvent::BytesAppended { bytes, stream, .. }, Source::Ansi(decoder)) => {
                        decoder.push_stream(stream, &bytes);
                        true
                    }
                    (
                        OutputEvent::BlockReplaced {
                            revision,
                            text: replacement,
                            ..
                        },
                        Source::Text { text, visible_from },
                    ) if revision > block.replacement_revision => {
                        self.generation += 1;
                        *visible_from = rebase_clear_anchor(text, &replacement, *visible_from);
                        *text = replacement;
                        block.replacement_revision = revision;
                        true
                    }
                    (OutputEvent::BlockFinished { outcome, .. }, source) => {
                        if let Source::Ansi(decoder) = source {
                            decoder.finish();
                        }
                        block.outcome = Some(outcome);
                        matches!(source, Source::Ansi(_))
                    }
                    _ => {
                        self.rejected_events += 1;
                        return false;
                    }
                };
                if source_changed {
                    block.revision += 1;
                }
            }
        }
        self.version += 1;
        true
    }

    pub(super) fn clear_visible(&mut self) {
        self.dirty = true;
        self.version += 1;
        self.generation += 1;
        self.operations.retain(|_, operation| !operation.finished);
        self.blocks.retain(|b| b.outcome.is_none());
        for block in &mut self.blocks {
            match &mut block.source {
                Source::Text { text, visible_from } => *visible_from = text.len(),
                Source::Ansi(decoder) => decoder.clear_visible(),
                Source::Prepared(lines) => lines.clear(),
            }
            block.revision += 1;
        }
    }

    pub(super) fn needs_render(&self, width: usize) -> bool {
        self.dirty || self.rendered_width != Some(width)
    }

    pub(super) fn render(&mut self, width: usize) -> RenderedDocument {
        self.render_checked(width, &|| Ok(()))
            .expect("uncancelled layout")
    }

    pub(super) fn render_checked(
        &mut self,
        width: usize,
        check: &impl Fn() -> io::Result<()>,
    ) -> io::Result<RenderedDocument> {
        check()?;
        let mut lines = Vec::new();
        let mut stable_lines = 0;
        let mut publication = Vec::new();
        let mut stable_prefix = true;
        for block in &mut self.blocks {
            stable_prefix &= block.outcome.is_some();
            let start = lines.len();
            lines.extend_from_slice(block.lines(width, check)?);
            let mut group_start = 0;
            for (index, group) in block.groups.iter().enumerate() {
                check()?;
                if block.groups.get(index + 1) != Some(group) {
                    publication.push(PublicationUnit {
                        id: RowIdentity {
                            block: block.identity,
                            group: *group,
                        },
                        end: start + index + 1,
                        stable: stable_prefix,
                        origins: block
                            .origins
                            .as_ref()
                            .map(|origins| origins[group_start..=index].to_vec().into()),
                    });
                    group_start = index + 1;
                }
            }
            // Outcome presentation has its own publication group. Keep it out
            // of the source cache so finishing doesn't invalidate Markdown.
            let diagnostic = match &block.outcome {
                Some(Outcome::Cancelled) => Some(RenderLine::plain("[interrupted]")),
                Some(Outcome::Failed(error)) => Some(RenderLine::plain(format!(
                    "[failed: {}]",
                    super::ansi::terminal_label(error)
                ))),
                _ => None,
            };
            if let Some(diagnostic) = diagnostic {
                lines.push(diagnostic);
                publication.push(PublicationUnit {
                    id: RowIdentity {
                        block: block.identity,
                        group: block.groups.last().copied().unwrap_or(0) + 1,
                    },
                    end: lines.len(),
                    stable: stable_prefix,
                    origins: None,
                });
            }
            if stable_prefix {
                stable_lines = lines.len();
            }
        }
        self.dirty = false;
        self.rendered_width = Some(width);
        Ok(RenderedDocument {
            lines,
            stable_lines,
            publication,
        })
    }
}

/// Preserve the boundary between hidden and visible source across edits. Inserts
/// before it belong to the hidden prefix; inserts at it belong to new output.
fn rebase_clear_anchor(previous: &str, next: &str, anchor: usize) -> usize {
    if anchor == 0 {
        return 0;
    }
    let mut old_offset = 0;
    let mut new_offset = 0;
    for change in similar::TextDiff::from_chars(previous, next).iter_all_changes() {
        if old_offset >= anchor {
            break;
        }
        let bytes = change.value().len();
        match change.tag() {
            similar::ChangeTag::Equal => {
                let before_anchor = bytes.min(anchor - old_offset);
                old_offset += before_anchor;
                new_offset += before_anchor;
            }
            similar::ChangeTag::Delete => old_offset += bytes,
            similar::ChangeTag::Insert => new_offset += bytes,
        }
    }
    new_offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{cancellation::CancellationEvent, events::EventSink};

    #[test]
    fn finishing_text_reuses_preview_cache_and_adds_outcome_once() {
        for outcome in [
            Outcome::Completed,
            Outcome::Cancelled,
            Outcome::Failed("oops".into()),
        ] {
            for finish_operation in [false, true] {
                let (sink, events) = EventSink::channel(CancellationEvent::new());
                let mut doc = OutputDocument::default();
                doc.start_operation(sink.operation());
                let id = sink.start_block(BlockKind::Markdown).unwrap();
                sink.text(id, "**FIRST**\n\n```rust\nlet value = 1;")
                    .unwrap();
                for event in events.try_iter() {
                    assert!(doc.apply(event));
                }
                // Model UI snapshots: the previous worker owns the cache, but
                // the new document snapshot was made before it was available.
                let mut previous = doc.clone();
                let preview = previous.render(40);
                let cache = previous.blocks[0].cache.as_ref().unwrap().2.clone();
                assert!(preview.publication.iter().all(|unit| !unit.stable));
                if finish_operation {
                    sink.finish(outcome.clone()).unwrap();
                } else {
                    sink.finish_block(id, outcome.clone()).unwrap();
                }
                for event in events.try_iter() {
                    assert!(doc.apply(event));
                }
                doc.reuse_caches(&previous);
                let completed = doc.render(40);
                assert!(Arc::ptr_eq(
                    &cache,
                    &doc.blocks[0].cache.as_ref().unwrap().2
                ));
                assert_eq!(&completed.lines[..preview.lines.len()], preview.lines);
                assert_eq!(completed.stable_lines, completed.lines.len());
                assert!(completed.publication.iter().all(|unit| unit.stable));
                assert!(
                    completed
                        .publication
                        .iter()
                        .any(|unit| unit.origins.is_some())
                );
                let marker = match outcome {
                    Outcome::Completed => None,
                    Outcome::Cancelled => Some("[interrupted]"),
                    Outcome::Failed(_) => Some("[failed: oops]"),
                };
                assert_eq!(
                    completed.lines.len(),
                    preview.lines.len() + usize::from(marker.is_some())
                );
                if let Some(marker) = marker {
                    assert_eq!(completed.lines.last().unwrap().text, marker);
                }
                assert_eq!(doc.render(40).lines, completed.lines);
                assert_eq!(previous.render(40).lines, preview.lines);
                assert_eq!(previous.render(40).stable_lines, 0);
                // A width change must still invalidate the source cache.
                doc.render(20);
                assert!(!Arc::ptr_eq(
                    &cache,
                    &doc.blocks[0].cache.as_ref().unwrap().2
                ));
            }
        }
    }

    #[test]
    fn finishing_ansi_invalidates_cache_to_flush_incomplete_utf8() {
        for finish_operation in [false, true] {
            let (sink, events) = EventSink::channel(CancellationEvent::new());
            let mut doc = OutputDocument::default();
            doc.start_operation(sink.operation());
            let id = sink.start_block(BlockKind::ToolOutput).unwrap();
            sink.emit(OutputEvent::BytesAppended {
                id,
                stream: 0,
                bytes: b"TOOL \xe2".to_vec(),
            })
            .unwrap();
            for event in events.try_iter() {
                assert!(doc.apply(event));
            }
            doc.render(40);
            let cache = doc.blocks[0].cache.as_ref().unwrap().2.clone();
            if finish_operation {
                sink.finish(Outcome::Cancelled).unwrap();
            } else {
                sink.finish_block(id, Outcome::Cancelled).unwrap();
            }
            for event in events.try_iter() {
                assert!(doc.apply(event));
            }
            let finished = doc.render(40);
            assert!(
                finished
                    .lines
                    .iter()
                    .any(|line| line.text == "TOOL \u{fffd}")
            );
            assert_eq!(finished.lines.last().unwrap().text, "[interrupted]");
            assert!(!Arc::ptr_eq(
                &cache,
                &doc.blocks[0].cache.as_ref().unwrap().2
            ));
        }
    }

    #[test]
    fn backend_activity_and_failure_cannot_inject_terminal_controls() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = OutputDocument::default();
        doc.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "preserved answer").unwrap();
        sink.activity("\x1b[2JWaiting", "\x1b]0;TITLE\x07remote\nstep\t2")
            .unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        assert_eq!(
            doc.activity,
            Some(("Waiting".into(), "remote step 2".into()))
        );
        sink.finish(Outcome::Failed(
            "\x1b[?1049h\x1b[31mremote failed\x1b[0m\nretry later\x07".into(),
        ))
        .unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let rendered = doc.render(60);
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.text == "preserved answer")
        );
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.text == "[failed: remote failed retry later]")
        );
        assert!(
            rendered
                .lines
                .iter()
                .all(|line| !line.text.chars().any(char::is_control))
        );
        assert!(doc.activity.is_none());
    }

    #[test]
    fn clear_anchor_follows_hidden_source_edits_and_leaves_new_suffix_visible() {
        for (previous, next, anchor, visible) in [
            ("OLD\n", "PREFIX OLD\nNEW", 4, "NEW"),
            ("OLD\nNEW", "PREFIX OLD\nNEW", 4, "NEW"),
            ("前文\n", "追加前文\n新文", "前文\n".len(), "新文"),
            ("REMOVED OLD\nNEW", "OLD\nNEW", "REMOVED OLD\n".len(), "NEW"),
        ] {
            let offset = rebase_clear_anchor(previous, next, anchor);
            assert_eq!(&next[offset..], visible);
        }
    }

    #[test]
    fn live_markdown_replaces_same_block_and_finish_preserves_source_and_style() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = OutputDocument::default();
        doc.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "**Hello**\n\n```rust\nlet n = 1;").unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let open = doc.render(40);
        assert_eq!(open.stable_lines, 0);
        assert!(
            open.lines
                .iter()
                .any(|line| line.text.contains("Hello")
                    && line.styles.iter().any(|style| style.bold))
        );
        sink.text(id, "\n```\n\nWorld").unwrap();
        sink.finish_block(id, Outcome::Completed).unwrap();
        sink.finish(Outcome::Completed).unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let completed = doc.render(40);
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(completed.stable_lines, completed.lines.len());
        assert_eq!(
            completed
                .lines
                .iter()
                .filter(|line| line.text.contains("Hello"))
                .count(),
            1
        );
        assert!(
            completed
                .lines
                .iter()
                .any(|line| line.text.contains("World"))
        );
    }

    #[test]
    fn growing_nested_list_keeps_indentation_styles_and_source_across_widths() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = OutputDocument::default();
        doc.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "* **PARENT**\n * CHILD").unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let first = doc.render(40);
        assert!(first.publication.iter().all(|unit| !unit.stable));
        sink.text(id, "\n  * `LEAF`").unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let source = "* **PARENT**\n * CHILD\n  * `LEAF`";
        for width in [40, 20, 60] {
            let rendered = doc.render(width);
            let offsets = ["PARENT", "CHILD", "LEAF"].map(|marker| {
                let matching = rendered
                    .lines
                    .iter()
                    .filter(|line| line.text.contains(marker))
                    .collect::<Vec<_>>();
                assert_eq!(matching.len(), 1, "width {width}, {marker}");
                matching[0].text.find(marker).unwrap()
            });
            assert!(
                offsets[0] < offsets[1] && offsets[1] < offsets[2],
                "width {width}: {offsets:?}"
            );
            assert!(
                rendered
                    .lines
                    .iter()
                    .any(|line| line.text.contains("PARENT")
                        && line.styles.iter().any(|style| style.bold))
            );
            assert_eq!(rendered.stable_lines, 0);
            assert!(matches!(&doc.blocks[0].source, Source::Text { text, .. } if text == source));
        }
        sink.finish_block(id, Outcome::Completed).unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let finished = doc.render(20);
        assert_eq!(finished.stable_lines, finished.lines.len());
        assert!(matches!(&doc.blocks[0].source, Source::Text { text, .. } if text == source));
    }

    #[test]
    fn late_reference_definition_reformats_open_block_without_changing_source() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = OutputDocument::default();
        doc.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        let source = "[**GUIDE**][doc]\n\n[doc]: https://example.test/guide\n";
        sink.text(id, "[**GUIDE**][doc]").unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        assert!(
            doc.render(60)
                .lines
                .iter()
                .any(|line| line.text.contains("[doc]"))
        );
        sink.text(id, "\n\n[doc]: https://example.test/guide\n")
            .unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        for width in [60, 20, 80] {
            let rendered = doc.render(width);
            let visible = rendered
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<String>();
            assert!(
                visible.contains("GUIDE") && visible.contains("https://example.test/guide"),
                "{visible}"
            );
            assert!(!visible.contains("[doc]"), "{visible}");
            assert!(
                rendered.lines.iter().any(|line| line.text.contains("GUIDE")
                    && line.styles.iter().any(|style| style.bold))
            );
            assert_eq!(rendered.stable_lines, 0);
            assert!(matches!(&doc.blocks[0].source, Source::Text { text, .. } if text == source));
        }
        sink.finish_block(id, Outcome::Completed).unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let finished = doc.render(60);
        assert_eq!(finished.stable_lines, finished.lines.len());
        assert_eq!(
            finished
                .lines
                .iter()
                .filter(|line| line.text.contains("GUIDE"))
                .count(),
            1
        );
        doc.clear_visible();
        assert!(doc.render(60).lines.is_empty());
    }

    #[test]
    fn clear_retains_reference_context_without_restoring_hidden_output() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = OutputDocument::default();
        doc.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "[doc]: https://example.test/guide\n\nHIDDEN\n\n")
            .unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        doc.clear_visible();
        sink.text(id, "[VISIBLE][doc]").unwrap();
        for event in events.try_iter() {
            assert!(doc.apply(event));
        }
        let rendered = doc
            .render(80)
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<String>();
        assert!(
            rendered.contains("VISIBLE (https://example.test/guide)"),
            "{rendered}"
        );
        assert!(!rendered.contains("HIDDEN") && !rendered.contains("[doc]:"));
    }

    #[test]
    fn cancel_preserves_prefix_and_rejects_late_or_duplicate_events() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = OutputDocument::default();
        doc.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "before cancel").unwrap();
        sink.finish(Outcome::Cancelled).unwrap();
        let events = events.try_iter().collect::<Vec<_>>();
        for event in &events {
            assert!(doc.apply(event.clone()));
        }
        assert!(!doc.apply(events[1].clone()));
        let lines = doc.render(40).lines;
        assert!(lines.iter().any(|line| line.text.contains("before cancel")));
        assert!(lines.iter().any(|line| line.text.contains("interrupted")));
    }

    #[test]
    fn clear_during_update_hides_old_source_without_sealing_open_block() {
        let (sink, events) = EventSink::channel(CancellationEvent::new());
        let mut doc = OutputDocument::default();
        doc.start_operation(sink.operation());
        let id = sink.start_block(BlockKind::Markdown).unwrap();
        sink.text(id, "OLD\n").unwrap();
        for event in events.try_iter() {
            doc.apply(event);
        }
        doc.clear_visible();
        sink.text(id, "NEW").unwrap();
        for event in events.try_iter() {
            doc.apply(event);
        }
        let rendered = doc.render(40);
        assert!(!rendered.lines.iter().any(|line| line.text.contains("OLD")));
        assert!(rendered.lines.iter().any(|line| line.text.contains("NEW")));
        assert_eq!(rendered.stable_lines, 0);
    }
}
