use super::{
    ansi::{char_len, is_plain_text_key},
    history::{HistoryEntry, HistoryKind, HistoryMode},
    interaction::is_key_action,
};
use crate::{
    input, shell,
    terminal_renderer::{RenderLine, VirtualCursor},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

mod buffer;
mod completion;
use buffer::EditorBuffer;
use completion::CompletionCycle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditorMode {
    Command,
    Ask,
    Shell,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubmissionKind {
    Command,
    Ask,
    Shell,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EditorSubmission {
    pub(super) kind: SubmissionKind,
    pub(super) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EditorOutcome {
    Changed,
    Redraw,
    Submit(EditorSubmission),
    OpenMultiline(EditorMode, String),
    Cancel,
    Exit,
    Unchanged,
}

#[derive(Debug, Clone)]
pub(super) struct UnifiedEditor {
    pub(super) mode: EditorMode,
    pub(super) buffer: EditorBuffer,
    pub(super) prompt: String,
    pub(super) continuation_prompt: String,
    pub(super) history: Vec<HistoryEntry>,
    pub(super) history_indices: Vec<usize>,
    pub(super) history_position: Option<usize>,
    pub(super) history_draft: String,
    pub(super) recalled_mode: Option<HistoryMode>,
    pub(super) completion: Option<CompletionCycle>,
}

impl UnifiedEditor {
    pub(super) fn command(prompt: String, history: &[HistoryEntry]) -> Self {
        Self::new(EditorMode::Command, prompt, "· ", history)
    }

    pub(super) fn ask(initial: Option<String>, history: &[HistoryEntry]) -> Self {
        let mut editor = Self::new(EditorMode::Ask, "· ".to_string(), "· ", history);
        if let Some(initial) = initial {
            editor.buffer.replace_with(&initial);
        }
        editor
    }

    pub(super) fn shell(initial: Option<String>, history: &[HistoryEntry]) -> Self {
        let mut editor = Self::new(EditorMode::Shell, "· ".to_string(), "· ", history);
        if let Some(initial) = initial {
            editor.buffer.replace_with(&initial);
        }
        editor
    }

    pub(super) fn api_key() -> Self {
        Self::new(
            EditorMode::ApiKey,
            "Openrouter API key: ".to_string(),
            "",
            &[],
        )
    }

    pub(super) fn new(
        mode: EditorMode,
        prompt: String,
        continuation_prompt: &str,
        history: &[HistoryEntry],
    ) -> Self {
        let history_indices = history
            .iter()
            .enumerate()
            .filter(|(_, entry)| match mode {
                EditorMode::Command => true,
                EditorMode::Ask => entry.kind == HistoryKind::Agent,
                EditorMode::Shell => entry.kind == HistoryKind::Shell,
                EditorMode::ApiKey => false,
            })
            .map(|(index, _)| index)
            .collect();
        Self {
            mode,
            buffer: EditorBuffer::new(),
            prompt,
            continuation_prompt: continuation_prompt.to_string(),
            history: history.to_vec(),
            history_indices,
            history_position: None,
            history_draft: String::new(),
            recalled_mode: None,
            completion: None,
        }
    }

    pub(super) fn render_lines(&self) -> (Vec<RenderLine>, VirtualCursor) {
        if self.mode == EditorMode::Command
            && self.history_is_browsing()
            && matches!(
                self.recalled_mode,
                Some(HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell)
            )
        {
            let (command, hint) = match self.recalled_mode {
                Some(HistoryMode::MultiLineAsk) => (
                    "/ask",
                    format!(
                        "Enter multiline input. Type {} on a new line to finish.",
                        input::MULTILINE_SUBMIT_COMMAND
                    ),
                ),
                Some(HistoryMode::MultiLineShell) => (
                    "/shell",
                    format!(
                        "Enter multiline shell command. Type {} on a new line to run.",
                        input::MULTILINE_SUBMIT_COMMAND
                    ),
                ),
                _ => unreachable!(),
            };
            let mut lines = vec![
                RenderLine::new(self.prompt.clone(), command),
                RenderLine::plain(hint),
            ];
            lines.extend(
                self.buffer
                    .lines
                    .iter()
                    .map(|text| RenderLine::new(self.continuation_prompt.clone(), text.clone())),
            );
            return (
                lines,
                VirtualCursor {
                    line: self.buffer.cursor.line + 2,
                    char_offset: self.buffer.cursor.char_offset,
                },
            );
        }

        let lines = self
            .buffer
            .lines
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let prefix = if index == 0 {
                    self.prompt.clone()
                } else {
                    self.continuation_prompt.clone()
                };
                let text = if self.mode == EditorMode::ApiKey {
                    "*".repeat(char_len(text))
                } else {
                    text.clone()
                };
                RenderLine::new(prefix, text)
            })
            .collect();
        (lines, self.buffer.cursor)
    }

    pub(super) fn handle_event(&mut self, event: Event) -> EditorOutcome {
        match event {
            Event::Paste(text) => self.handle_paste(&text),
            Event::Key(key) if is_key_action(key.kind) => self.handle_key(key),
            _ => EditorOutcome::Unchanged,
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> EditorOutcome {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return EditorOutcome::Cancel;
        }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return match self.mode {
                EditorMode::Command if self.buffer.is_empty() => EditorOutcome::Exit,
                EditorMode::Ask => self.submit(SubmissionKind::Ask),
                EditorMode::Shell => self.submit(SubmissionKind::Shell),
                EditorMode::ApiKey if self.buffer.is_empty() => EditorOutcome::Cancel,
                _ => EditorOutcome::Unchanged,
            };
        }
        if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return EditorOutcome::Redraw;
        }
        if key.code == KeyCode::Esc {
            return EditorOutcome::Unchanged;
        }

        if self.history_is_browsing()
            && matches!(
                key.code,
                KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab | KeyCode::Char(_)
            )
        {
            // Recalled history is a preview until Enter or a navigation key
            // explicitly accepts it. Destructive/text keys cannot
            // accidentally modify a history entry while it is still a
            // browsing selection.
            return EditorOutcome::Changed;
        }

        if self.history_is_browsing() {
            let keeps_preview = matches!(key.code, KeyCode::Home | KeyCode::End)
                || matches!(key.code, KeyCode::Left | KeyCode::Right)
                    && key.modifiers.intersects(
                        KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
                    );
            if keeps_preview {
                return EditorOutcome::Changed;
            }
            if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                if let Some(mode) = self.recalled_multiline_editor_mode() {
                    return EditorOutcome::OpenMultiline(mode, self.buffer.text());
                }
                self.stop_history();
            }
        }

        match key.code {
            KeyCode::Enter => return self.handle_enter(),
            KeyCode::Backspace => {
                self.prepare_edit();
                self.buffer.backspace();
            }
            KeyCode::Delete => {
                self.prepare_edit();
                self.buffer.delete();
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.buffer.move_home();
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.buffer.move_end();
            }
            KeyCode::Left
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                self.buffer.move_word_left();
            }
            KeyCode::Right
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                self.buffer.move_word_right();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.buffer.move_word_left();
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.buffer.move_word_right();
            }
            KeyCode::Left => {
                self.buffer.move_left();
            }
            KeyCode::Right => {
                self.buffer.move_right();
            }
            KeyCode::Up => {
                if self.history_position.is_some()
                    || (self.mode == EditorMode::Command && self.buffer.lines.len() == 1)
                    || self.buffer.is_empty()
                {
                    self.history_previous();
                } else {
                    self.buffer.move_up();
                }
            }
            KeyCode::Down => {
                if self.history_position.is_some()
                    || (self.mode == EditorMode::Command && self.buffer.lines.len() == 1)
                    || (self.buffer.cursor.line + 1 == self.buffer.lines.len()
                        && self.buffer.cursor.char_offset == char_len(self.buffer.current_line()))
                {
                    self.history_next();
                } else {
                    self.buffer.move_down();
                }
            }
            KeyCode::Home => {
                self.buffer.move_home();
            }
            KeyCode::End => {
                self.buffer.move_end();
            }
            KeyCode::Tab if self.mode != EditorMode::ApiKey => {
                self.advance_completion();
                return EditorOutcome::Changed;
            }
            KeyCode::Tab => return EditorOutcome::Unchanged,
            KeyCode::Char(ch) if is_plain_text_key(key) && !ch.is_control() => {
                self.prepare_edit();
                self.buffer.insert_char(ch);
            }
            _ => return EditorOutcome::Unchanged,
        }
        self.completion = None;
        EditorOutcome::Changed
    }

    pub(super) fn handle_enter(&mut self) -> EditorOutcome {
        self.completion = None;
        if self.history_is_browsing() {
            if let Some(mode) = self.recalled_multiline_editor_mode() {
                return EditorOutcome::OpenMultiline(mode, self.buffer.text());
            }
            self.stop_history();
            return EditorOutcome::Changed;
        }

        match self.mode {
            EditorMode::Command => {
                if shell::input_syntax::should_read_shell_continuation(&self.buffer.text()) {
                    self.buffer.split_line();
                    EditorOutcome::Changed
                } else {
                    self.submit(SubmissionKind::Command)
                }
            }
            EditorMode::Ask | EditorMode::Shell => {
                if self.buffer.current_line().trim() == input::MULTILINE_SUBMIT_COMMAND
                    && self.buffer.cursor.line + 1 == self.buffer.lines.len()
                {
                    self.buffer.remove_current_line();
                    let kind = if self.mode == EditorMode::Ask {
                        SubmissionKind::Ask
                    } else {
                        SubmissionKind::Shell
                    };
                    self.submit(kind)
                } else {
                    self.buffer.split_line();
                    EditorOutcome::Changed
                }
            }
            EditorMode::ApiKey => self.submit(SubmissionKind::ApiKey),
        }
    }

    pub(super) fn handle_paste(&mut self, text: &str) -> EditorOutcome {
        self.paste(text, true)
    }

    pub(super) fn handle_draft_paste(&mut self, text: &str) -> EditorOutcome {
        self.paste(text, false)
    }

    fn paste(&mut self, text: &str, allow_submit: bool) -> EditorOutcome {
        if self.history_is_browsing() {
            return EditorOutcome::Changed;
        }
        self.prepare_edit();
        if self.mode == EditorMode::ApiKey {
            let filtered = text
                .chars()
                .filter(|ch| !matches!(ch, '\r' | '\n'))
                .collect::<String>();
            self.buffer.insert_text(&filtered);
            return EditorOutcome::Changed;
        }
        self.buffer.insert_text(text);
        self.completion = None;
        let ended_with_newline = text.ends_with(['\r', '\n']);
        if allow_submit
            && self.mode == EditorMode::Command
            && ended_with_newline
            && !shell::input_syntax::should_read_shell_continuation(&self.buffer.text())
        {
            while self.buffer.lines.last().is_some_and(String::is_empty)
                && self.buffer.lines.len() > 1
            {
                self.buffer.lines.pop();
            }
            self.buffer.cursor.line = self.buffer.lines.len() - 1;
            self.buffer.cursor.char_offset = char_len(self.buffer.current_line());
            return self.submit(SubmissionKind::Command);
        }
        EditorOutcome::Changed
    }

    pub(super) fn submit(&self, kind: SubmissionKind) -> EditorOutcome {
        EditorOutcome::Submit(EditorSubmission {
            kind,
            text: self.buffer.text(),
        })
    }

    pub(super) fn prepare_edit(&mut self) {
        self.completion = None;
    }

    pub(super) fn history_is_browsing(&self) -> bool {
        self.history_position.is_some()
            && (matches!(self.mode, EditorMode::Ask | EditorMode::Shell)
                || self.buffer.lines.len() > 1
                || matches!(
                    self.recalled_mode,
                    Some(HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell)
                ))
    }

    pub(super) fn recalled_multiline_editor_mode(&self) -> Option<EditorMode> {
        if self.mode != EditorMode::Command {
            return None;
        }
        match self.recalled_mode {
            Some(HistoryMode::MultiLineAsk) => Some(EditorMode::Ask),
            Some(HistoryMode::MultiLineShell) => Some(EditorMode::Shell),
            _ => None,
        }
    }

    pub(super) fn stop_history(&mut self) {
        self.history_position = None;
        self.recalled_mode = None;
    }

    pub(super) fn history_previous(&mut self) {
        if self.history_indices.is_empty() {
            return;
        }
        let next_position = match self.history_position {
            None => {
                self.history_draft = self.buffer.text();
                self.history_indices.len() - 1
            }
            Some(position) => position.saturating_sub(1),
        };
        self.restore_history(next_position);
    }

    pub(super) fn history_next(&mut self) {
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 >= self.history_indices.len() {
            self.buffer.replace_with(&self.history_draft);
            self.stop_history();
        } else {
            self.restore_history(position + 1);
        }
    }

    pub(super) fn restore_history(&mut self, position: usize) {
        let Some(entry) = self
            .history_indices
            .get(position)
            .and_then(|index| self.history.get(*index))
        else {
            return;
        };
        let text = if self.mode == EditorMode::Command && entry.mode == HistoryMode::SingleLineAsk {
            format!("/ask {}", entry.text)
        } else {
            entry.text.clone()
        };
        self.buffer.replace_with(&text);
        self.history_position = Some(position);
        self.recalled_mode = Some(entry.mode);
        self.completion = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_history_can_be_recalled_from_non_empty_draft() {
        let history = vec![HistoryEntry {
            text: "git status".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.buffer.replace_with("draft");

        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(editor.buffer.text(), "git status");
        assert_eq!(editor.history_draft, "draft");
    }

    #[test]
    fn recalled_single_line_history_is_immediately_editable() {
        let history = vec![HistoryEntry {
            text: "git status".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        editor.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(editor.buffer.text(), "git statusx");
        assert!(editor.history_position.is_some());
        assert!(!editor.history_is_browsing());
    }

    #[test]
    fn recalled_multiline_history_reopens_unified_multiline_editor() {
        let history = vec![HistoryEntry {
            text: "echo one\necho two".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::MultiLineShell,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::OpenMultiline(EditorMode::Shell, "echo one\necho two".to_string())
        );
    }

    #[test]
    fn recalled_multiline_history_renders_command_hint_and_hidden_cursor() {
        let history = vec![HistoryEntry {
            text: "line one\nline two".to_string(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::MultiLineAsk,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        let (lines, cursor) = editor.render_lines();

        assert_eq!(lines[0].prefix, "user> ");
        assert_eq!(lines[0].text, "/ask");
        assert!(lines[1].text.contains(input::MULTILINE_SUBMIT_COMMAND));
        assert_eq!(lines[2].prefix, input::DEFAULT_COMMAND_CONTINUATION_PROMPT);
        assert_eq!(lines[2].text, "line one");
        assert_eq!(cursor.line, 3);
        assert!(editor.history_is_browsing());
    }

    #[test]
    fn plain_arrow_reopens_recalled_multiline_special_editor() {
        let history = vec![HistoryEntry {
            text: "echo one\necho two".to_string(),
            kind: HistoryKind::Shell,
            mode: HistoryMode::MultiLineShell,
        }];
        let mut editor = UnifiedEditor::command("user> ".to_string(), &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            EditorOutcome::OpenMultiline(EditorMode::Shell, "echo one\necho two".to_string())
        );
    }

    #[test]
    fn multiline_editor_history_is_preview_until_enter() {
        let history = vec![HistoryEntry {
            text: "previous question".to_string(),
            kind: HistoryKind::Agent,
            mode: HistoryMode::SingleLine,
        }];
        let mut editor = UnifiedEditor::ask(None, &history);
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert!(editor.history_is_browsing());
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.buffer.text(), "previous question");
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::Changed
        );
        assert!(!editor.history_is_browsing());
    }

    #[test]
    fn api_key_editor_masks_rendered_text_but_submits_original() {
        let mut editor = UnifiedEditor::api_key();
        editor.buffer.replace_with("secret");
        let (lines, _) = editor.render_lines();

        assert_eq!(lines[0].text, "******");
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::Submit(EditorSubmission {
                kind: SubmissionKind::ApiKey,
                text: "secret".to_string(),
            })
        );
    }

    #[test]
    fn api_key_paste_discards_line_breaks() {
        let mut editor = UnifiedEditor::api_key();

        editor.handle_event(Event::Paste("first\r\nsecond\n".to_string()));

        assert_eq!(editor.buffer.text(), "firstsecond");
        assert_eq!(editor.buffer.lines.len(), 1);
    }

    #[test]
    fn multiline_ctrl_d_submits_without_end_marker() {
        let mut editor = UnifiedEditor::shell(None, &[]);
        editor.buffer.replace_with("echo one\necho two");

        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            EditorOutcome::Submit(EditorSubmission {
                kind: SubmissionKind::Shell,
                text: "echo one\necho two".to_string(),
            })
        );
    }
}
