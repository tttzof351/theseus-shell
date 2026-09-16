//! Document layout and the managed screen's output/status/editor composition.
use super::{
    Application, Interaction,
    ansi::{
        ansi_render_lines, char_len, render_markdown, style_interaction_lines, style_prompt,
        style_shell_lines, terminal_label,
    },
    document_layout,
    editor::{EditorSubmission, SubmissionKind},
    shell_commands::shell_prompt,
    state::ExecutionState,
};
use crate::{
    common, input,
    terminal_renderer::{RenderLine, TerminalSize, VirtualCursor, VirtualScreen},
};
use std::io;

impl Application {
    pub(super) fn screen(&mut self, terminal_size: TerminalSize) -> &VirtualScreen {
        let dirty_from = self
            .transcript
            .iter()
            .enumerate()
            .position(|(index, line)| {
                self.screen_cache.lines.get(index) != Some(&line.text)
                    || self.screen_cache.prefixes.get(index) != Some(&line.prefix)
                    || self.screen_cache.line_styles.get(index) != Some(&line.styles)
                    || self.screen_cache.prefix_styles.get(index) != Some(&line.prefix_styles)
            })
            .unwrap_or(self.transcript.len());
        // Preserve stable text/styles in the virtual screen; replace only the
        // changed suffix, including the status and editor from the last frame.
        self.screen_cache.truncate(dirty_from);
        for line in &self.transcript[dirty_from..] {
            self.screen_cache.push_render_line(line);
        }
        self.cached_transcript_lines = self.transcript.len();
        if let Some((_, outcome)) = &self.layout_feedback {
            let text = match outcome {
                common::events::Outcome::Cancelled => "[interrupted]".into(),
                common::events::Outcome::Failed(error) => {
                    format!("[failed: {}]", terminal_label(error))
                }
                common::events::Outcome::Completed => String::new(),
            };
            self.screen_cache.push_render_line(&RenderLine::plain(text));
        }
        if let Some(active) = &self.active_operation {
            let (phase, detail) = self.document.activity.clone().unwrap_or_default();
            let phase = if self.execution_state() == ExecutionState::Cancelling {
                "Cancelling"
            } else {
                &phase
            };
            let elapsed = active.started.elapsed();
            let spinner = common::progress::spinner_frame(elapsed);
            let status = if matches!(phase, "" | "Waiting for response") {
                spinner.to_string()
            } else {
                format!("{spinner} {phase} · {}s {detail}", elapsed.as_secs())
            };
            self.screen_cache
                .push_render_line(&RenderLine::plain(status));
        }
        let base = self.screen_cache.lines.len();
        let (mut active_lines, cursor, cursor_visible) = match &mut self.interaction {
            Interaction::Editor(editor) => {
                let (lines, cursor) = editor.render_lines();
                (lines, cursor, !editor.history_is_browsing())
            }
            Interaction::Config(picker)
            | Interaction::Models(picker)
            | Interaction::Resume(picker, _) => picker.render_lines(
                terminal_size.height.saturating_sub(5).min(16),
                terminal_size.width,
            ),
        };
        style_interaction_lines(
            &self.interaction,
            &mut active_lines,
            &self.config.shell_settings.shell_highlight,
        );
        for line in &active_lines {
            self.screen_cache.push_render_line(line);
        }
        if self.screen_cache.lines.is_empty() {
            self.screen_cache.push_render_line(&RenderLine::plain(""));
        }
        self.screen_cache.cursor = VirtualCursor {
            line: base + cursor.line,
            char_offset: cursor.char_offset,
        };
        self.screen_cache.cursor_visible = cursor_visible;
        self.screen_cache.dirty_from = dirty_from;
        &self.screen_cache
    }

    pub(super) fn commit_submission(&mut self, submission: &EditorSubmission) {
        let (prompt, continuation) = match submission.kind {
            SubmissionKind::Command => (
                shell_prompt(self.working_dir.as_deref()),
                input::DEFAULT_COMMAND_CONTINUATION_PROMPT.to_string(),
            ),
            SubmissionKind::Ask | SubmissionKind::Shell => (
                input::DEFAULT_MULTILINE_PREFIX.to_string(),
                input::DEFAULT_MULTILINE_PREFIX.to_string(),
            ),
            SubmissionKind::ApiKey => ("Openrouter API key: ".to_string(), String::new()),
        };
        let mut committed = Vec::new();
        for (index, line) in submission.text.split('\n').enumerate() {
            let shown = if submission.kind == SubmissionKind::ApiKey {
                "*".repeat(char_len(line))
            } else {
                line.to_string()
            };
            let render_line = RenderLine::new(
                if index == 0 {
                    prompt.clone()
                } else {
                    continuation.clone()
                },
                shown,
            );
            committed.push(render_line);
        }
        if matches!(
            submission.kind,
            SubmissionKind::Command | SubmissionKind::Shell
        ) {
            style_shell_lines(
                &mut committed,
                &submission.text,
                &self.config.shell_settings.shell_highlight,
            );
        }
        for line in &mut committed {
            style_prompt(line);
        }
        self.document.append_lines(committed);
    }

    pub(super) fn refresh_document(&mut self, width: usize) -> io::Result<()> {
        if self.terminal.is_some()
            && (self.layout_worker.is_some() || self.document.requires_background_layout())
        {
            if self.layout_worker.is_none() {
                self.layout_worker = Some(document_layout::Worker::new()?);
            }
            let worker = self.layout_worker.as_mut().unwrap();
            let key = document_layout::Key::of(&self.document, width);
            if let Some(prepared) = worker.take(key)? {
                self.rendered_stable_lines = prepared.rendered.stable_lines;
                self.publication = prepared.rendered.publication;
                self.transcript = prepared.rendered.lines;
                self.screen_cache = prepared.screen;
                self.prepared_layout = Some(prepared.layout);
                if self
                    .layout_feedback
                    .as_ref()
                    .is_some_and(|(version, _)| *version <= prepared.key.version)
                {
                    self.layout_feedback = None;
                }
            }
            worker.request(key, &self.document);
            return Ok(());
        }
        if !self.document.needs_render(width) {
            return Ok(());
        }
        let rendered = self.document.render(width);
        self.rendered_stable_lines = rendered.stable_lines;
        self.publication = rendered.publication;
        self.transcript = rendered.lines;
        Ok(())
    }

    pub(super) fn layout_pending(&self, width: usize) -> bool {
        self.layout_worker
            .as_ref()
            .is_some_and(|worker| worker.pending(document_layout::Key::of(&self.document, width)))
    }

    pub(super) fn prepare_document(&mut self, width: usize) -> io::Result<()> {
        loop {
            self.refresh_document(width)?;
            if !self.layout_pending(width) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    pub(super) fn clear_output(&mut self) {
        self.document.clear_visible();
        self.transcript.clear();
        self.cached_transcript_lines = 0;
        self.prepared_layout = None;
        self.layout_feedback = None;
    }

    pub(super) fn append_text(&mut self, text: &str) {
        self.document.append_lines(ansi_render_lines(text));
    }

    pub(super) fn append_markdown(&mut self, text: &str) {
        self.document
            .append_lines(ansi_render_lines(&render_markdown(text)));
    }

    pub(super) fn take_physical_invalidation(&mut self) -> bool {
        std::mem::take(&mut self.physical_invalidated)
    }
}
