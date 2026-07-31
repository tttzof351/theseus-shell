use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EditorBuffer {
    pub(super) lines: Vec<String>,
    pub(super) cursor: VirtualCursor,
    pub(super) goal_column: Option<usize>,
}

impl EditorBuffer {
    pub(super) fn new() -> Self {
        Self::from_text("")
    }

    pub(super) fn from_text(text: &str) -> Self {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = normalized
            .split('\n')
            .map(str::to_string)
            .collect::<Vec<_>>();
        let line = lines.len().saturating_sub(1);
        let char_offset = lines.get(line).map_or(0, |line| char_len(line));
        Self {
            lines: if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            },
            cursor: VirtualCursor { line, char_offset },
            goal_column: None,
        }
    }

    pub(super) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub(super) fn current_line(&self) -> &str {
        &self.lines[self.cursor.line]
    }

    pub(super) fn replace_with(&mut self, text: &str) {
        *self = Self::from_text(text);
    }

    pub(super) fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor.line];
        let offset = byte_offset(line, self.cursor.char_offset);
        line.insert(offset, ch);
        self.cursor.char_offset += 1;
        self.goal_column = None;
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            match ch {
                '\n' => self.split_line(),
                ch if !ch.is_control() => self.insert_char(ch),
                '\t' => self.insert_char(ch),
                _ => {}
            }
        }
    }

    pub(super) fn split_line(&mut self) {
        let line = &mut self.lines[self.cursor.line];
        let offset = byte_offset(line, self.cursor.char_offset);
        let tail = line.split_off(offset);
        self.cursor.line += 1;
        self.cursor.char_offset = 0;
        self.lines.insert(self.cursor.line, tail);
        self.goal_column = None;
    }

    pub(super) fn remove_current_line(&mut self) {
        if self.lines.len() == 1 {
            self.lines[0].clear();
            self.cursor = VirtualCursor {
                line: 0,
                char_offset: 0,
            };
            return;
        }
        self.lines.remove(self.cursor.line);
        self.cursor.line = self.cursor.line.saturating_sub(1);
        self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor.char_offset > 0 {
            let line = &mut self.lines[self.cursor.line];
            let end = byte_offset(line, self.cursor.char_offset);
            let start = byte_offset(line, self.cursor.char_offset - 1);
            line.replace_range(start..end, "");
            self.cursor.char_offset -= 1;
        } else if self.cursor.line > 0 {
            let current = self.lines.remove(self.cursor.line);
            self.cursor.line -= 1;
            self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
            self.lines[self.cursor.line].push_str(&current);
        }
        self.goal_column = None;
    }

    pub(super) fn delete(&mut self) {
        let line_len = char_len(self.current_line());
        if self.cursor.char_offset < line_len {
            let line = &mut self.lines[self.cursor.line];
            let start = byte_offset(line, self.cursor.char_offset);
            let end = byte_offset(line, self.cursor.char_offset + 1);
            line.replace_range(start..end, "");
        } else if self.cursor.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
        }
        self.goal_column = None;
    }

    pub(super) fn move_left(&mut self) {
        if self.cursor.char_offset > 0 {
            self.cursor.char_offset -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
        }
        self.goal_column = None;
    }

    pub(super) fn move_right(&mut self) {
        let line_len = char_len(self.current_line());
        if self.cursor.char_offset < line_len {
            self.cursor.char_offset += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.char_offset = 0;
        }
        self.goal_column = None;
    }

    pub(super) fn move_up(&mut self) -> bool {
        if self.cursor.line == 0 {
            return false;
        }
        let goal = self.goal_column.unwrap_or(self.cursor.char_offset);
        self.cursor.line -= 1;
        self.cursor.char_offset = goal.min(char_len(self.current_line()));
        self.goal_column = Some(goal);
        true
    }

    pub(super) fn move_down(&mut self) -> bool {
        if self.cursor.line + 1 >= self.lines.len() {
            return false;
        }
        let goal = self.goal_column.unwrap_or(self.cursor.char_offset);
        self.cursor.line += 1;
        self.cursor.char_offset = goal.min(char_len(self.current_line()));
        self.goal_column = Some(goal);
        true
    }

    pub(super) fn move_home(&mut self) {
        self.cursor.char_offset = 0;
        self.goal_column = None;
    }

    pub(super) fn move_end(&mut self) {
        self.cursor.char_offset = char_len(self.current_line());
        self.goal_column = None;
    }

    pub(super) fn move_word_left(&mut self) {
        if self.cursor.char_offset == 0 {
            self.move_left();
            return;
        }
        let chars = self.current_line().chars().collect::<Vec<_>>();
        let mut index = self.cursor.char_offset;
        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !chars[index - 1].is_whitespace() {
            index -= 1;
        }
        self.cursor.char_offset = index;
        self.goal_column = None;
    }

    pub(super) fn move_word_right(&mut self) {
        let chars = self.current_line().chars().collect::<Vec<_>>();
        let mut index = self.cursor.char_offset;
        while index < chars.len() && !chars[index].is_whitespace() {
            index += 1;
        }
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        self.cursor.char_offset = index;
        self.goal_column = None;
    }

    pub(super) fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        let cursor = self.cursor.char_offset;
        if start > cursor {
            return;
        }
        let line = &mut self.lines[self.cursor.line];
        let byte_start = byte_offset(line, start);
        let byte_end = byte_offset(line, cursor);
        line.replace_range(byte_start..byte_end, replacement);
        self.cursor.char_offset = start + char_len(replacement);
        self.goal_column = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub(super) enum HistoryKind {
    Shell,
    Agent,
    Special,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub(super) enum HistoryMode {
    SingleLine,
    SingleLineAsk,
    MultiLineAsk,
    MultiLineShell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub(super) struct HistoryEntry {
    pub(super) text: String,
    pub(super) kind: HistoryKind,
    pub(super) mode: HistoryMode,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletionCandidate {
    pub(super) replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletionCycle {
    pub(super) start: usize,
    pub(super) candidates: Vec<CompletionCandidate>,
    pub(super) selected: usize,
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
        if self.mode == EditorMode::Command
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

    pub(super) fn advance_completion(&mut self) {
        let restart_directory = self.completion.as_ref().is_some_and(|cycle| {
            cycle.candidates.len() == 1
                && cycle.candidates[0].replacement.ends_with(['/', '\\'])
                && self
                    .buffer
                    .current_line()
                    .chars()
                    .skip(cycle.start)
                    .take(self.buffer.cursor.char_offset.saturating_sub(cycle.start))
                    .collect::<String>()
                    == cycle.candidates[0].replacement
        });
        if restart_directory {
            self.completion = None;
        }
        if let Some(cycle) = &mut self.completion {
            cycle.selected = (cycle.selected + 1) % cycle.candidates.len();
            let replacement = cycle.candidates[cycle.selected].replacement.clone();
            self.buffer.replace_before_cursor(cycle.start, &replacement);
            return;
        }

        let line = self.buffer.current_line().to_string();
        let cursor = self.buffer.cursor.char_offset;
        let Some((start, candidates)) = completion_candidates(
            &line,
            cursor,
            matches!(self.mode, EditorMode::Command | EditorMode::Shell)
                && self.buffer.cursor.line == 0,
        ) else {
            return;
        };
        let replacement = candidates[0].replacement.clone();
        self.buffer.replace_before_cursor(start, &replacement);
        self.completion = Some(CompletionCycle {
            start,
            candidates,
            selected: 0,
        });
    }
}

pub(super) fn completion_candidates(
    line: &str,
    cursor: usize,
    command_completion: bool,
) -> Option<(usize, Vec<CompletionCandidate>)> {
    let before_cursor = line.chars().take(cursor).collect::<String>();
    let start = completion_token_start(&before_cursor);
    let token = before_cursor.chars().skip(start).collect::<String>();
    let first_token = before_cursor[..byte_offset(&before_cursor, start)]
        .trim()
        .is_empty();

    if command_completion && first_token {
        let mut builtins = commands::slash_command_names()
            .chain(["cd", "exit"])
            .filter(|name| name.starts_with(&token))
            .map(str::to_string)
            .collect::<Vec<_>>();
        builtins.sort();
        builtins.dedup();
        if token.is_empty() || !builtins.is_empty() {
            let candidates = builtins
                .into_iter()
                .map(|replacement| CompletionCandidate { replacement })
                .collect::<Vec<_>>();
            return (!candidates.is_empty()).then_some((start, candidates));
        }
        let replacements = if looks_like_path_token(&token) {
            path_completions_with_common_prefix(&token)
        } else {
            path_executables(&token)
        };
        let candidates = replacements
            .into_iter()
            .map(|replacement| CompletionCandidate { replacement })
            .collect::<Vec<_>>();
        return (!candidates.is_empty()).then_some((start, candidates));
    }

    if command_completion && !first_token {
        let preceding_words = before_cursor[..byte_offset(&before_cursor, start)]
            .split_whitespace()
            .count();
        if preceding_words == 1 {
            let command = before_cursor.split_whitespace().next().unwrap_or_default();
            if matches!(command, "git" | "cargo") {
                let candidates = special_subcommand_completions(command, &token)
                    .into_iter()
                    .map(|replacement| CompletionCandidate { replacement })
                    .collect::<Vec<_>>();
                return (!candidates.is_empty()).then_some((start, candidates));
            }
        }
    }

    let candidates = path_completions_with_common_prefix(&token)
        .into_iter()
        .map(|replacement| CompletionCandidate { replacement })
        .collect::<Vec<_>>();
    (!candidates.is_empty()).then_some((start, candidates))
}

pub(super) fn completion_token_start(text: &str) -> usize {
    let chars = text.chars().collect::<Vec<_>>();
    for index in (0..chars.len()).rev() {
        if !chars[index].is_whitespace() {
            continue;
        }
        let backslashes = chars[..index]
            .iter()
            .rev()
            .take_while(|ch| **ch == '\\')
            .count();
        if backslashes % 2 == 0 {
            return index + 1;
        }
    }
    0
}

pub(super) fn looks_like_path_token(token: &str) -> bool {
    token.starts_with(['.', '/', '~']) || token.contains(['/', '\\'])
}

pub(super) fn path_executables(prefix: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    let Some(path) = env::var_os("PATH") else {
        return Vec::new();
    };
    for directory in env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

pub(super) fn special_subcommand_completions(command: &str, prefix: &str) -> Vec<String> {
    let values: &[&str] = match command {
        "git" => &[
            "add",
            "bisect",
            "branch",
            "checkout",
            "cherry-pick",
            "clean",
            "clone",
            "commit",
            "diff",
            "fetch",
            "grep",
            "init",
            "log",
            "merge",
            "mv",
            "pull",
            "push",
            "rebase",
            "remote",
            "reset",
            "restore",
            "revert",
            "rm",
            "show",
            "stash",
            "status",
            "switch",
            "tag",
            "worktree",
        ],
        "cargo" => &[
            "add", "bench", "build", "check", "clean", "doc", "fetch", "fix", "fmt", "install",
            "login", "metadata", "new", "package", "publish", "remove", "run", "search", "test",
            "tree", "update",
        ],
        _ => return Vec::new(),
    };
    values
        .iter()
        .filter(|value| value.starts_with(prefix))
        .map(|value| (*value).to_string())
        .collect()
}

pub(super) fn common_char_prefix(values: &[String]) -> Option<String> {
    let mut prefix = values.first()?.clone();
    for value in &values[1..] {
        let length = prefix
            .chars()
            .zip(value.chars())
            .take_while(|(left, right)| left == right)
            .count();
        prefix.truncate(byte_offset(&prefix, length));
    }
    Some(prefix)
}

pub(super) fn path_completions_with_common_prefix(token: &str) -> Vec<String> {
    let mut candidates = path_completions(token);
    if candidates.len() > 1
        && let Some(common) = common_char_prefix(&candidates)
        && char_len(&common) > char_len(token)
    {
        candidates.insert(0, common);
    }
    candidates
}

pub(super) fn path_completions(token: &str) -> Vec<String> {
    let unescaped = unescape_shell_token(token);
    let (directory_text, file_prefix) = unescaped
        .rsplit_once('/')
        .map_or(("", unescaped.as_str()), |(directory, file)| {
            (directory, file)
        });
    let lookup_directory = if directory_text.is_empty() {
        PathBuf::from(".")
    } else if directory_text == "~" {
        home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else if let Some(rest) = directory_text.strip_prefix("~/") {
        home_dir().map_or_else(|| PathBuf::from(directory_text), |home| home.join(rest))
    } else {
        PathBuf::from(directory_text)
    };
    let Ok(entries) = fs::read_dir(lookup_directory) else {
        return Vec::new();
    };
    let rendered_directory = if directory_text.is_empty() {
        String::new()
    } else {
        format!("{directory_text}/")
    };
    let mut matches = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !name.starts_with(file_prefix)
                || (name.starts_with('.') && !file_prefix.starts_with('.'))
            {
                return None;
            }
            Some((
                escape_shell_token(&format!("{rendered_directory}{name}")),
                entry.path().is_dir(),
            ))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0.cmp(&right.0));
    let single_directory = matches.len() == 1 && matches[0].1;
    let mut results = matches
        .into_iter()
        .map(|(mut replacement, _)| {
            if single_directory {
                replacement.push('/');
            }
            replacement
        })
        .collect::<Vec<_>>();
    results.sort();
    results
}

pub(super) fn unescape_shell_token(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                output.push(next);
            } else {
                output.push(ch);
            }
        } else {
            output.push(ch);
        }
    }
    output
}

pub(super) fn escape_shell_token(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '\\' | '\''
                    | '"'
                    | '$'
                    | '&'
                    | ';'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '!'
                    | '#'
            )
        {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[derive(Debug, Clone)]
pub(super) struct PickerItem {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) detail: String,
}

#[derive(Debug, Clone)]
pub(super) struct PickerState {
    pub(super) title: String,
    pub(super) query: String,
    pub(super) items: Vec<PickerItem>,
    pub(super) filtered: Vec<usize>,
    pub(super) selected: usize,
    pub(super) searchable: bool,
    pub(super) viewport_rows: usize,
}

impl PickerState {
    pub(super) fn new(title: impl Into<String>, items: Vec<PickerItem>, searchable: bool) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            title: title.into(),
            query: String::new(),
            items,
            filtered,
            selected: 0,
            searchable,
            viewport_rows: 10,
        }
    }

    pub(super) fn with_selected_id(mut self, id: Option<&str>) -> Self {
        if let Some(id) = id
            && let Some(position) = self
                .filtered
                .iter()
                .position(|index| self.items[*index].id == id)
        {
            self.selected = position;
        }
        self
    }

    pub(super) fn selected_item(&self) -> Option<&PickerItem> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    pub(super) fn move_by(&mut self, amount: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.filtered.len() - 1);
    }

    pub(super) fn refresh(&mut self) {
        let terms = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let haystack =
                    format!("{} {} {}", item.id, item.label, item.detail).to_ascii_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return PickerOutcome::Cancel;
        }
        match key.code {
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.viewport_rows as isize)),
            KeyCode::PageDown => self.move_by(self.viewport_rows as isize),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.filtered.len().saturating_sub(1),
            KeyCode::Enter => {
                return self
                    .selected_item()
                    .map(|item| PickerOutcome::Submit(item.id.clone()))
                    .unwrap_or(PickerOutcome::Changed);
            }
            KeyCode::Backspace if self.searchable => {
                self.query.pop();
                self.refresh();
            }
            KeyCode::Char(ch) if self.searchable && is_plain_text_key(key) => {
                self.query.push(ch);
                self.refresh();
            }
            _ => return PickerOutcome::Unchanged,
        }
        PickerOutcome::Changed
    }

    pub(super) fn render_lines(
        &mut self,
        viewport_rows: usize,
        width: usize,
    ) -> (Vec<RenderLine>, VirtualCursor, bool) {
        let mut lines = vec![RenderLine::plain(truncate_for_width(&self.title, width))];
        if self.searchable {
            lines.push(RenderLine::new(
                "Search: ",
                truncate_for_width(&self.query, width.saturating_sub(char_len("Search: "))),
            ));
        }
        let row_capacity = viewport_rows.max(1).min(self.items.len().max(1));
        self.viewport_rows = row_capacity;
        let mut rendered_rows = 0;
        if self.filtered.is_empty() {
            lines.push(RenderLine::plain("  No matches"));
            rendered_rows = 1;
        } else {
            let rows = row_capacity.min(self.filtered.len());
            let top = self
                .selected
                .saturating_sub(rows / 2)
                .min(self.filtered.len().saturating_sub(rows));
            for position in top..top + rows {
                let item = &self.items[self.filtered[position]];
                let marker = if position == self.selected {
                    "> "
                } else {
                    "  "
                };
                let detail = if item.detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", item.detail)
                };
                lines.push(RenderLine::plain(truncate_for_width(
                    &format!("{marker}{}{detail}", item.label),
                    width,
                )));
                rendered_rows += 1;
            }
        }
        lines.extend(
            std::iter::repeat_with(|| RenderLine::plain(""))
                .take(row_capacity.saturating_sub(rendered_rows)),
        );
        lines.push(RenderLine::plain("Enter: select · Esc/Ctrl+C: cancel"));
        // The virtual cursor doubles as the viewport anchor. Keep it on the
        // last picker row so a picker opened near the bottom of a long
        // transcript remains fully visible.
        let cursor_line = lines.len().saturating_sub(1);
        let cursor = VirtualCursor {
            line: cursor_line,
            char_offset: char_len(&lines[cursor_line].text),
        };
        (lines, cursor, false)
    }
}

pub(super) fn truncate_for_width(text: &str, width: usize) -> String {
    if char_len(text) <= width {
        return text.to_string();
    }
    match width {
        0 => String::new(),
        1..=3 => ".".repeat(width),
        _ => {
            let mut truncated = text.chars().take(width - 3).collect::<String>();
            truncated.push_str("...");
            truncated
        }
    }
}

pub(super) fn model_catalog_source_label(
    source: &model_catalog::ModelCatalogSource,
) -> &'static str {
    match source {
        model_catalog::ModelCatalogSource::Fresh => "(OpenRouter)",
        model_catalog::ModelCatalogSource::Cache => "(cache)",
        model_catalog::ModelCatalogSource::StaleCache => "(stale cache)",
        model_catalog::ModelCatalogSource::Fallback => "(fallback)",
    }
}

pub(super) fn format_context_length(context_length: u64) -> String {
    if context_length >= 1_000_000 {
        format!("{}m", context_length / 1_000_000)
    } else if context_length >= 1_000 {
        format!("{}k", context_length / 1_000)
    } else {
        context_length.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PickerOutcome {
    Changed,
    Submit(String),
    Cancel,
    Unchanged,
}
