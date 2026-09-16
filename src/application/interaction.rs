//! Editor/picker transitions and routing of terminal input.
use super::{
    Application, Interaction,
    editor::{EditorMode, EditorOutcome, EditorSubmission, SubmissionKind, UnifiedEditor},
    history::{HistoryKind, HistoryMode},
    shell_commands::shell_prompt,
    state::ExecutionState,
};
use crate::input;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use std::io;

pub(super) fn is_key_action(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}
pub(super) fn interaction_needs_input(interaction: &Interaction) -> bool {
    !matches!(
        interaction,
        Interaction::Editor(UnifiedEditor {
            mode: EditorMode::Command,
            ..
        })
    )
}
impl Application {
    pub(super) fn handle_event(&mut self, event: Event) -> io::Result<bool> {
        if matches!(
            self.execution_state(),
            ExecutionState::Stopping | ExecutionState::ShellPassthrough
        ) {
            return Ok(false);
        }
        if let Some(active) = &self.active_operation {
            if let Event::Key(key) = &event {
                if is_key_action(key.kind)
                    && key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    active.cancellation.cancel();
                    return Ok(true);
                }
                if is_key_action(key.kind)
                    && (key.code == KeyCode::Enter
                        || (key.code == KeyCode::Char('d')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                    && let Interaction::Editor(editor) = &mut self.interaction
                    && (!editor.history_is_browsing() || key.code != KeyCode::Enter)
                {
                    if matches!(editor.mode, EditorMode::Ask | EditorMode::Shell)
                        && key.code == KeyCode::Enter
                    {
                        editor.buffer.split_line();
                    }
                    self.sync_multiline_draft();
                    return Ok(true);
                }
            }
            if let Event::Paste(text) = &event {
                if let Interaction::Editor(editor) = &mut self.interaction {
                    let outcome = editor.handle_draft_paste(text);
                    return self.finish_editor_outcome(outcome);
                }
                return Ok(true);
            }
        }
        if matches!(event, Event::Resize(_, _)) {
            return Ok(true);
        }
        let Event::Key(key) = &event else {
            let outcome = match &mut self.interaction {
                Interaction::Editor(editor) => Some(editor.handle_event(event)),
                _ => None,
            };
            if let Some(outcome) = outcome {
                return self.finish_editor_outcome(outcome);
            }
            return Ok(false);
        };
        if !is_key_action(key.kind) {
            return Ok(false);
        }

        match &mut self.interaction {
            Interaction::Editor(editor) => {
                let outcome = editor.handle_event(event);
                self.finish_editor_outcome(outcome)
            }
            Interaction::Config(picker) => {
                let outcome = picker.handle_key(*key);
                self.finish_config_picker(outcome)
            }
            Interaction::Models(picker) => {
                let outcome = picker.handle_key(*key);
                self.finish_model_picker(outcome)
            }
            Interaction::Resume(picker, _) => {
                let outcome = picker.handle_key(*key);
                self.finish_resume_picker(outcome)
            }
        }
    }

    fn finish_editor_outcome(&mut self, outcome: EditorOutcome) -> io::Result<bool> {
        match outcome {
            EditorOutcome::Submit(submission) => {
                let submission_kind = submission.kind;
                if matches!(submission_kind, SubmissionKind::Ask | SubmissionKind::Shell) {
                    // Enter may have removed /end without producing a Changed
                    // outcome. Replace the draft before storing the submission.
                    self.sync_multiline_draft();
                }
                self.commit_submission(&submission);
                self.execute_submission(submission)?;
                if submission_kind != SubmissionKind::Command
                    && self.active_operation.is_none()
                    && self.pending_shell.is_none()
                {
                    self.finish_pending_command_log();
                }
                Ok(true)
            }
            EditorOutcome::OpenMultiline(mode, text) => {
                let recalled_command = match mode {
                    EditorMode::Ask => "/ask",
                    EditorMode::Shell => "/shell",
                    _ => unreachable!("only multiline modes can be opened from history"),
                };
                self.commit_submission(&EditorSubmission {
                    kind: SubmissionKind::Command,
                    text: recalled_command.to_string(),
                });
                let (kind, history_mode, guidance) = match mode {
                    EditorMode::Ask => (
                        HistoryKind::Agent,
                        HistoryMode::MultiLineAsk,
                        format!(
                            "Enter multiline input. Type {} on a new line to finish.\n",
                            input::MULTILINE_SUBMIT_COMMAND
                        ),
                    ),
                    EditorMode::Shell => (
                        HistoryKind::Shell,
                        HistoryMode::MultiLineShell,
                        format!(
                            "Enter multiline shell command. Type {} on a new line to run.\n",
                            input::MULTILINE_SUBMIT_COMMAND
                        ),
                    ),
                    _ => unreachable!("only multiline modes can be opened from history"),
                };
                self.append_text(&guidance);
                self.interaction = Interaction::Editor(match mode {
                    EditorMode::Ask => UnifiedEditor::ask(Some(text), &self.history),
                    EditorMode::Shell => UnifiedEditor::shell(Some(text), &self.history),
                    _ => unreachable!(),
                });
                self.active_draft = Some((self.history.len(), kind, history_mode));
                Ok(true)
            }
            EditorOutcome::Cancel => {
                let cancelled_submission = match &self.interaction {
                    Interaction::Editor(editor) => Some(EditorSubmission {
                        kind: match editor.mode {
                            EditorMode::Command => SubmissionKind::Command,
                            EditorMode::Ask => SubmissionKind::Ask,
                            EditorMode::Shell => SubmissionKind::Shell,
                            EditorMode::ApiKey => SubmissionKind::ApiKey,
                        },
                        text: editor.buffer.text(),
                    }),
                    _ => None,
                };
                let cancelled_kind = cancelled_submission
                    .as_ref()
                    .map(|submission| submission.kind);
                if let Some(submission) = cancelled_submission {
                    self.commit_submission(&submission);
                }
                match cancelled_kind {
                    Some(SubmissionKind::Command) => {
                        self.append_text("Interrupted. Type /exit to exit the shell.\n");
                    }
                    Some(SubmissionKind::Ask) => self.append_text("\nAsk cancelled.\n"),
                    Some(SubmissionKind::Shell) => self.append_text("\nShell cancelled.\n"),
                    Some(SubmissionKind::ApiKey) => self.append_text("Config cancelled.\n"),
                    None => self.append_text("Cancelled.\n"),
                }
                self.return_to_command_editor();
                self.finish_pending_command_log();
                Ok(true)
            }
            EditorOutcome::Exit => {
                self.exit_requested = true;
                Ok(true)
            }
            EditorOutcome::Redraw => {
                // Ctrl+L is a logical screen clear, not merely a request to
                // repaint the same scene. Drop the committed transcript while
                // keeping the active editor (and its draft) intact. `screen`
                // notices that the cached prefix is now longer than the
                // transcript and rebuilds the VirtualScreen from line zero.
                self.clear_output();
                self.physical_invalidated = true;
                Ok(true)
            }
            EditorOutcome::Changed => {
                self.sync_multiline_draft();
                Ok(true)
            }
            EditorOutcome::Unchanged => Ok(false),
        }
    }

    pub(super) fn return_to_command_editor(&mut self) {
        self.active_draft = None;
        let prompt = shell_prompt(self.working_dir.as_deref());
        self.interaction = Interaction::Editor(
            self.saved_model_draft
                .take()
                .unwrap_or_else(|| UnifiedEditor::command(prompt, &self.history)),
        );
    }
}
