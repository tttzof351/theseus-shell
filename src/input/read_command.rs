use std::io::{self, IsTerminal, Write};

use crossterm::{
    cursor::{Hide, MoveDown, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType},
};

use super::{
    colorize_tag,
    completion::{CompletionState, completion_state, path_completion_state, token_before_cursor},
    constants::{DEFAULT_MULTILINE_PREFIX, MULTILINE_ASK_HINT, MULTILINE_SHELL_HINT},
    editor_render::{
        EditorLine, RenderLayout, cursor_visible_col, cursor_wraps_at_boundary,
        render_editor_lines, render_layout_for_lines_with_cursor_wrap,
    },
    history_browser::{
        BrowsingAction, BrowsingInput, HistoryBrowser, HistoryEntryMode, HistoryMove,
    },
    is_alt_key, is_command_key, is_key_press, is_plain_text_key,
    raw_mode::RawModeGuard,
    shell_highlight::{
        ShellHighlightPalette, default_shell_highlight_palette,
        highlight_shell_command_with_palette,
    },
    text_buffer::TextBuffer,
    text_length,
};
use crate::{commands::slash_commands, common::terminal_output};

#[cfg(test)]
use super::completion::{Completion, CompletionToken};

pub struct CommandInputConfig<'a> {
    pub prompt: &'a str,
    pub continuation_prompt: &'a str,
    pub history: &'a [CommandHistoryItem],
    pub should_continue: fn(&str) -> bool,
    pub shell_highlight: Option<&'a ShellHighlightPalette>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHistoryItem {
    pub text: String,
    pub submit: CommandHistorySubmit,
}

impl CommandHistoryItem {
    pub fn command(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            submit: CommandHistorySubmit::Command(text.clone()),
            text,
        }
    }

    pub fn multiline_ask(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            text: format!("/ask\n{text}"),
            submit: CommandHistorySubmit::MultilineAsk(text),
        }
    }

    pub fn multiline_shell(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            text: format!("/shell\n{text}"),
            submit: CommandHistorySubmit::MultilineShell(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandHistorySubmit {
    Command(String),
    MultilineAsk(String),
    MultilineShell(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandInputResult {
    Command(String),
    MultilineAsk(String),
    MultilineShell(String),
}

struct CommandEditor<'a> {
    config: CommandInputConfig<'a>,
    history_texts: Vec<String>,
    buffer: TextBuffer,
    history: HistoryBrowser,
    completion: Option<CompletionState>,
    rendered_rows: u16,
    rendered_cursor_row: u16,
}

struct MultilinePreview {
    hint: &'static str,
}

pub fn read_command_input(
    config: CommandInputConfig<'_>,
) -> io::Result<Option<CommandInputResult>> {
    if !io::stdin().is_terminal() {
        return read_stdin_line().map(|line| line.map(CommandInputResult::Command));
    }

    let _raw_mode = RawModeGuard::enable()?;
    let mut editor = CommandEditor::new(config);
    editor.run()
}

fn read_stdin_line() -> io::Result<Option<String>> {
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(None);
    }

    Ok(Some(input.trim_end_matches(['\r', '\n']).to_string()))
}

impl<'a> CommandEditor<'a> {
    fn new(config: CommandInputConfig<'a>) -> Self {
        let history_texts = config
            .history
            .iter()
            .map(|item| item.text.clone())
            .collect();
        Self {
            config,
            history_texts,
            buffer: TextBuffer::new(),
            history: HistoryBrowser::default(),
            completion: None,
            rendered_rows: 1,
            rendered_cursor_row: 0,
        }
    }

    fn run(&mut self) -> io::Result<Option<CommandInputResult>> {
        terminal_output::with_stdout(|stdout| {
            write!(stdout, "{}", self.config.prompt)?;
            stdout.flush()
        })?;

        loop {
            match event::read()? {
                Event::Key(key) if is_key_press(key) => {
                    if let Some(line) = self.handle_key(key)? {
                        return Ok(line);
                    }
                }
                Event::Paste(text) => {
                    if let Some(line) = self.handle_paste(&text)? {
                        return Ok(line);
                    }
                    self.render()?;
                }
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<Option<Option<CommandInputResult>>> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_cursor()?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.is_empty() {
                    return Ok(Some(None));
                }
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.clear_screen_and_render()?;
            }
            KeyCode::Enter => {
                self.clear_completion();
                if let Some(result) = self.selected_multiline_submit() {
                    self.history.accept();
                    let command = match &result {
                        CommandInputResult::MultilineAsk(_) => "/ask",
                        CommandInputResult::MultilineShell(_) => "/shell",
                        CommandInputResult::Command(_) => unreachable!(),
                    };
                    self.buffer.replace_with_text(command);
                    self.render()?;
                    self.finish_line()?;
                    return Ok(Some(Some(result)));
                }
                if self.apply_browsing_input(BrowsingInput::Enter) == BrowsingAction::Accept {
                    self.render()?;
                    return Ok(None);
                }
                if self.enter_should_continue() {
                    self.split_line();
                    self.render()?;
                } else {
                    self.finish_line()?;
                    return Ok(Some(Some(CommandInputResult::Command(self.current_text()))));
                }
            }
            KeyCode::Backspace => {
                self.clear_completion();
                self.backspace();
                self.render()?;
            }
            KeyCode::Delete => {
                self.clear_completion();
                self.delete();
                self.render()?;
            }
            KeyCode::Left if is_command_key(key) => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::Home);
                self.buffer.set_col(0);
                self.render()?;
            }
            KeyCode::Right if is_command_key(key) => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::End);
                self.buffer.set_col_to_line_end();
                self.render()?;
            }
            KeyCode::Char('b') if is_alt_key(key) => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::MoveWordLeft);
                self.move_word_left();
                self.render()?;
            }
            KeyCode::Char('f') if is_alt_key(key) => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::MoveWordRight);
                self.move_word_right();
                self.render()?;
            }
            KeyCode::Left if is_alt_key(key) => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::MoveWordLeft);
                self.move_word_left();
                self.render()?;
            }
            KeyCode::Right if is_alt_key(key) => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::MoveWordRight);
                self.move_word_right();
                self.render()?;
            }
            KeyCode::Left => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::Left);
                self.move_left();
                self.render()?;
            }
            KeyCode::Right => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::Right);
                self.move_right();
                self.render()?;
            }
            KeyCode::Up => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::HistoryPrevious);
                if self.history.is_browsing() || self.can_navigate_history() {
                    if let Some(result) = self.history_previous() {
                        self.render()?;
                        self.finish_line()?;
                        return Ok(Some(Some(result)));
                    }
                } else {
                    self.buffer.move_up();
                }
                self.render()?;
            }
            KeyCode::Down => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::HistoryNext);
                if self.history.is_browsing() || self.can_navigate_history() {
                    if let Some(result) = self.history_next() {
                        self.render()?;
                        self.finish_line()?;
                        return Ok(Some(Some(result)));
                    }
                } else {
                    self.buffer.move_down();
                }
                self.render()?;
            }
            KeyCode::Home => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::Home);
                self.buffer.set_col(0);
                self.render()?;
            }
            KeyCode::End => {
                self.clear_completion();
                self.apply_browsing_input(BrowsingInput::End);
                self.buffer.set_col_to_line_end();
                self.render()?;
            }
            KeyCode::Tab => {
                self.complete()?;
            }
            KeyCode::Char(ch) if is_plain_text_key(key) => {
                self.clear_completion();
                self.insert_char(ch);
                self.render()?;
            }
            _ => {}
        }

        Ok(None)
    }

    fn handle_paste(&mut self, text: &str) -> io::Result<Option<Option<CommandInputResult>>> {
        self.clear_completion();
        self.apply_browsing_input(BrowsingInput::Paste);

        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for segment in normalized.split_inclusive('\n') {
            let Some(line) = segment.strip_suffix('\n') else {
                if !segment.is_empty() {
                    self.buffer.insert_text(segment);
                }
                continue;
            };

            self.buffer.insert_text(line);
            if self.enter_should_continue() {
                self.split_line();
            } else {
                self.finish_line()?;
                return Ok(Some(Some(CommandInputResult::Command(self.current_text()))));
            }
        }

        Ok(None)
    }

    fn render(&mut self) -> io::Result<()> {
        let layout = self.render_layout();
        let lines = self.render_lines();
        terminal_output::with_stdout(|stdout| {
            if self.history.is_browsing() {
                execute!(stdout, Hide)?;
            } else {
                execute!(stdout, Show)?;
            }
            render_editor_lines(
                stdout,
                &lines,
                layout,
                self.rendered_rows,
                self.rendered_cursor_row,
            )
        })?;

        self.rendered_rows = layout.rows;
        self.rendered_cursor_row = layout.cursor_row;
        Ok(())
    }

    fn clear_screen_and_render(&mut self) -> io::Result<()> {
        terminal_output::with_stdout(|stdout| {
            execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
            stdout.flush()
        })?;
        self.rendered_rows = 1;
        self.rendered_cursor_row = 0;
        self.render()
    }

    fn render_layout(&self) -> RenderLayout {
        let columns = terminal::size()
            .ok()
            .map(|(columns, _)| columns.max(1) as usize)
            .unwrap_or(80);
        self.render_layout_for_columns(columns)
    }

    fn render_layout_for_columns(&self, columns: usize) -> RenderLayout {
        render_layout_for_lines_with_cursor_wrap(
            &self.render_lines(),
            self.buffer.row(),
            self.cursor_visible_col(),
            columns,
            self.cursor_wraps_at_boundary(),
        )
    }

    fn render_lines(&self) -> Vec<EditorLine<'_>> {
        if let Some(preview) = self.selected_multiline_preview()
            && self.buffer.lines_len() > 1
        {
            return self.render_multiline_preview_lines(preview.hint);
        }

        let highlighted_shell_lines = if self.line_text(0).starts_with('/') {
            Vec::new()
        } else {
            let default_palette;
            let palette = match self.config.shell_highlight {
                Some(palette) => palette,
                None => {
                    default_palette = default_shell_highlight_palette();
                    &default_palette
                }
            };
            highlight_shell_command_with_palette(&self.current_text(), palette)
        };

        (0..self.buffer.lines_len())
            .map(|index| {
                let line = self.line_text(index);
                let rendered_line = if !highlighted_shell_lines.is_empty() {
                    highlighted_shell_lines
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| line.clone())
                } else if index == 0 {
                    highlighted_input(&line)
                } else {
                    line.clone()
                };
                let rendered_line = if self.history.is_browsing() {
                    colorize_tag("italic", &rendered_line)
                } else {
                    rendered_line
                };
                EditorLine::with_visible_len(
                    self.prompt_for_row(index),
                    rendered_line,
                    text_length(&line, false),
                )
            })
            .collect()
    }

    fn render_multiline_preview_lines(&self, hint_text: &'static str) -> Vec<EditorLine<'_>> {
        let mut lines = Vec::with_capacity(self.buffer.lines_len() + 1);
        let first_line = self.line_text(0);
        let first_line = highlighted_input(&first_line);
        let first_line = colorize_tag("italic", &first_line);
        lines.push(EditorLine::with_visible_len(
            self.config.prompt,
            first_line,
            text_length(&self.line_text(0), false),
        ));

        let hint = colorize_tag("bright-black", hint_text);
        lines.push(EditorLine::with_visible_len(
            "",
            hint,
            text_length(hint_text, false),
        ));

        for index in 1..self.buffer.lines_len() {
            let line = self.line_text(index);
            let rendered_line = colorize_tag("italic", &line);
            lines.push(EditorLine::with_visible_len(
                DEFAULT_MULTILINE_PREFIX,
                rendered_line,
                text_length(&line, false),
            ));
        }

        lines
    }

    fn cursor_visible_col(&self) -> usize {
        let line = self.current_line();
        cursor_visible_col(&line, self.buffer.col())
    }

    fn cursor_wraps_at_boundary(&self) -> bool {
        let line = self.current_line();
        cursor_wraps_at_boundary(&line, self.buffer.col())
    }

    fn finish_line(&self) -> io::Result<()> {
        terminal_output::with_stdout(|stdout| {
            execute!(stdout, Show)?;
            let rows_below_cursor = self.rendered_rows - 1 - self.rendered_cursor_row;
            if rows_below_cursor > 0 {
                execute!(stdout, MoveDown(rows_below_cursor))?;
            }
            write!(stdout, "\r\n")?;
            stdout.flush()
        })
    }

    fn show_cursor(&self) -> io::Result<()> {
        terminal_output::with_stdout(|stdout| execute!(stdout, Show))
    }

    fn prompt_for_row(&self, row: usize) -> &str {
        if row == 0 {
            self.config.prompt
        } else {
            self.config.continuation_prompt
        }
    }

    fn enter_should_continue(&mut self) -> bool {
        let text = self.current_text();
        let should_continue = (self.config.should_continue)(&text);
        if should_continue {
            self.apply_browsing_input(BrowsingInput::InsertText);
        }
        should_continue
    }

    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    fn current_text(&self) -> String {
        self.buffer.text()
    }

    fn current_line(&self) -> String {
        self.buffer.current_line_text()
    }

    fn line_text(&self, row: usize) -> String {
        self.buffer.line_text(row)
    }

    #[cfg(test)]
    fn insert_text(&mut self, text: &str) {
        self.clear_completion();
        self.apply_browsing_input(BrowsingInput::InsertText);
        self.buffer.insert_text(text);
    }

    fn insert_char(&mut self, ch: char) {
        self.apply_browsing_input(BrowsingInput::InsertText);
        self.buffer.insert_char(ch);
    }

    fn split_line(&mut self) {
        self.buffer.split_line();
    }

    fn backspace(&mut self) {
        if self.buffer.backspace() {
            self.apply_browsing_input(BrowsingInput::Backspace);
        }
    }

    fn delete(&mut self) {
        if self.buffer.delete() {
            self.apply_browsing_input(BrowsingInput::Delete);
        }
    }

    fn move_left(&mut self) {
        self.buffer.move_left();
    }

    fn move_right(&mut self) {
        self.buffer.move_right();
    }

    fn move_word_left(&mut self) {
        self.buffer.move_word_left();
    }

    fn move_word_right(&mut self) {
        self.buffer.move_word_right();
    }

    fn history_previous(&mut self) -> Option<CommandInputResult> {
        if self.history_texts.is_empty() {
            return None;
        }
        let current_text = self.current_text();
        let can_start = self.can_navigate_history();
        let selected = self.history.previous(
            &self.history_texts,
            current_text,
            can_start,
            command_history_entry_mode,
        );
        match selected {
            HistoryMove::Selected { index, text, .. } => {
                self.apply_selected_history(index, text.to_string())
            }
            HistoryMove::RestoredDraft(draft) => {
                self.buffer.replace_with_text(&draft);
                None
            }
            HistoryMove::Unchanged => None,
        }
    }

    fn history_next(&mut self) -> Option<CommandInputResult> {
        let can_start = self.can_navigate_history();
        let selected =
            self.history
                .next(&self.history_texts, can_start, command_history_entry_mode);
        match selected {
            HistoryMove::Selected { index, text, .. } => {
                self.apply_selected_history(index, text.to_string())
            }
            HistoryMove::RestoredDraft(draft) => {
                self.buffer.replace_with_text(&draft);
                None
            }
            HistoryMove::Unchanged => None,
        }
    }

    fn can_navigate_history(&self) -> bool {
        self.history.index().is_some() || (self.buffer.lines_len() == 1 && self.buffer.row() == 0)
    }

    fn apply_selected_history(
        &mut self,
        index: usize,
        display_text: String,
    ) -> Option<CommandInputResult> {
        self.buffer.replace_with_text(&display_text);
        let _ = index;
        None
    }

    fn selected_multiline_submit(&self) -> Option<CommandInputResult> {
        let index = self.history.index()?;
        match &self.config.history.get(index)?.submit {
            CommandHistorySubmit::Command(_) => None,
            CommandHistorySubmit::MultilineAsk(text) => {
                Some(CommandInputResult::MultilineAsk(text.clone()))
            }
            CommandHistorySubmit::MultilineShell(text) => {
                Some(CommandInputResult::MultilineShell(text.clone()))
            }
        }
    }

    fn selected_multiline_preview(&self) -> Option<MultilinePreview> {
        let index = self.history.index()?;
        match &self.config.history.get(index)?.submit {
            CommandHistorySubmit::Command(_) => None,
            CommandHistorySubmit::MultilineAsk(_) => Some(MultilinePreview {
                hint: MULTILINE_ASK_HINT,
            }),
            CommandHistorySubmit::MultilineShell(_) => Some(MultilinePreview {
                hint: MULTILINE_SHELL_HINT,
            }),
        }
    }

    #[cfg(test)]
    fn accept_history_browsing(&mut self) {
        self.history.accept();
    }

    fn apply_browsing_input(&mut self, input: BrowsingInput) -> BrowsingAction {
        self.history.apply_input(input)
    }

    fn clear_completion(&mut self) {
        self.completion = None;
    }

    fn complete(&mut self) -> io::Result<()> {
        self.apply_browsing_input(BrowsingInput::Completion);

        if self.should_restart_completed_directory_completion() {
            self.clear_completion();
        }

        if self.advance_completion() {
            return self.render();
        }

        let Some(state) = self.build_completion_state() else {
            return self.render();
        };
        self.completion = Some(state);
        self.advance_completion();
        self.render()
    }

    fn should_restart_completed_directory_completion(&self) -> bool {
        let Some(state) = &self.completion else {
            return false;
        };
        if state.token.is_command || state.selected.is_none() || state.completions.len() != 1 {
            return false;
        }

        let Some(completion) = state.completions.first() else {
            return false;
        };

        completion.replacement.ends_with(['/', '\\'])
            && token_before_cursor(&self.current_line(), self.buffer.col())
                .is_some_and(|token| token.value == completion.replacement)
    }

    fn build_completion_state(&self) -> Option<CompletionState> {
        if self.buffer.row() == 0 {
            completion_state(&self.current_line(), self.buffer.col())
        } else {
            path_completion_state(&self.current_line(), self.buffer.col())
        }
    }

    fn advance_completion(&mut self) -> bool {
        let Some(state) = self.completion.as_mut() else {
            return false;
        };
        if state.completions.is_empty() {
            return false;
        }

        let next_index = state
            .selected
            .map_or(0, |index| (index + 1) % state.completions.len());
        let token = state.token.clone();
        let replacement = state.completions[next_index].replacement.clone();
        state.selected = Some(next_index);

        self.replace_before_cursor(token.start, &replacement);
        true
    }

    fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        self.buffer.replace_before_cursor(start, replacement);
    }
}

impl Drop for CommandEditor<'_> {
    fn drop(&mut self) {
        #[cfg(not(test))]
        if self.history.is_browsing() {
            let _ = self.show_cursor();
        }
    }
}

fn command_history_entry_mode(entry: &str) -> HistoryEntryMode {
    if entry.contains('\n') {
        HistoryEntryMode::Browsing
    } else {
        HistoryEntryMode::Editing
    }
}

fn highlighted_input(input: &str) -> String {
    let Some(spec) = slash_commands().iter().find(|spec| {
        input == spec.name
            || input
                .strip_prefix(spec.name)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    }) else {
        return input.to_string();
    };

    let rest = &input[spec.name.len()..];
    format!("{}{}", colorize_tag("bright-cyan", spec.name), rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{ShellHighlightStyle, strip_ansi_codes};

    fn never_continue(_: &str) -> bool {
        false
    }

    fn trailing_backslash(text: &str) -> bool {
        text.ends_with('\\')
    }

    fn simple_if_block(text: &str) -> bool {
        text.trim_start().starts_with("if ") && !text.trim_end().ends_with("\nfi")
    }

    fn command_history(entries: &[&str]) -> Vec<CommandHistoryItem> {
        entries
            .iter()
            .map(|entry| CommandHistoryItem::command(*entry))
            .collect()
    }

    fn config<'a>(history: &'a [CommandHistoryItem]) -> CommandInputConfig<'a> {
        CommandInputConfig {
            prompt: "main> ",
            continuation_prompt: crate::input::DEFAULT_COMMAND_CONTINUATION_PROMPT,
            history,
            should_continue: never_continue,
            shell_highlight: None,
        }
    }

    #[test]
    fn joined_text_preserves_newlines_between_continuation_lines() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("echo \\\n \"test\"");

        assert_eq!(editor.current_text(), "echo \\\n \"test\"");
    }

    #[test]
    fn enter_splits_when_should_continue() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(CommandInputConfig {
            should_continue: trailing_backslash,
            ..config(&history)
        });
        editor.insert_text("echo \\");

        let should_continue = editor.enter_should_continue();
        editor.split_line();

        assert!(should_continue);
        assert_eq!(editor.buffer.lines_len(), 2);
        assert_eq!(editor.buffer.row(), 1);
        assert_eq!(editor.buffer.col(), 0);
    }

    #[test]
    fn enter_submits_when_no_continuation() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("echo ok");

        assert!(!editor.enter_should_continue());
        assert_eq!(editor.current_text(), "echo ok");
    }

    #[test]
    fn paste_submits_complete_command_with_trailing_newline() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(config(&history));

        let submitted = editor.handle_paste("echo ok\n").unwrap();

        assert_eq!(
            submitted,
            Some(Some(CommandInputResult::Command("echo ok".to_string())))
        );
    }

    #[test]
    fn paste_continues_incomplete_block_until_closing_line() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(CommandInputConfig {
            should_continue: simple_if_block,
            ..config(&history)
        });

        let submitted = editor
            .handle_paste("if true; then\necho IF_FROM_PASTE\nfi\n")
            .unwrap();

        assert_eq!(
            submitted,
            Some(Some(CommandInputResult::Command(
                "if true; then\necho IF_FROM_PASTE\nfi".to_string()
            )))
        );
    }

    #[test]
    fn paste_keeps_incomplete_block_in_editor_without_submit() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(CommandInputConfig {
            should_continue: simple_if_block,
            ..config(&history)
        });

        let submitted = editor.handle_paste("if true; then\necho waiting").unwrap();

        assert_eq!(submitted, None);
        assert_eq!(editor.current_text(), "if true; then\necho waiting");
        assert_eq!(editor.buffer.lines_len(), 2);
    }

    #[test]
    fn layout_uses_first_prompt_for_first_line_and_continuation_after() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("abcd\nef");
        editor.buffer.set_position(1, 2);

        let layout = editor.render_layout_for_columns(8);

        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cursor_row, 2);
        assert_eq!(layout.cursor_col, 4);
    }

    #[test]
    fn render_lines_highlights_shell_continuation_lines() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("if true; then\n  echo \"$USER\" # comment\nfi");

        let lines = editor.render_lines();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].text.contains("\x1b["));
        assert!(lines[1].text.contains("\x1b["));
        assert!(lines[2].text.contains("\x1b["));
        assert_eq!(
            lines
                .iter()
                .map(|line| strip_ansi_codes(&line.text))
                .collect::<Vec<_>>(),
            vec!["if true; then", "  echo \"$USER\" # comment", "fi"]
        );
    }

    #[test]
    fn render_lines_keeps_slash_command_highlighting_separate() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("/ask echo \"$USER\"");

        let lines = editor.render_lines();

        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].text,
            format!("{} echo \"$USER\"", colorize_tag("bright-cyan", "/ask"))
        );
    }

    #[test]
    fn render_lines_uses_custom_shell_highlight_palette() {
        let history = Vec::new();
        let mut palette = default_shell_highlight_palette();
        palette.insert(
            "command".to_string(),
            Some(ShellHighlightStyle::single("yellow")),
        );
        let mut editor = CommandEditor::new(CommandInputConfig {
            shell_highlight: Some(&palette),
            ..config(&history)
        });
        editor.insert_text("echo plain");

        let lines = editor.render_lines();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "\x1b[33mecho\x1b[0m plain");
    }

    #[test]
    fn history_replaces_single_line_buffer() {
        let history = command_history(&["first", "second"]);
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();

        assert_eq!(editor.current_text(), "second");
        assert_eq!(editor.buffer.row(), 0);
        assert_eq!(editor.buffer.col(), "second".chars().count());
        assert!(!editor.history.is_browsing());
    }

    #[test]
    fn history_multiline_ask_entry_returns_multiline_ask_submit_signal() {
        let history = vec![CommandHistoryItem::multiline_ask("line one\nline two")];
        let mut editor = CommandEditor::new(config(&history));

        let selected = editor.history_previous();

        assert_eq!(selected, None);
        assert_eq!(editor.current_text(), "/ask\nline one\nline two");
        assert_eq!(
            editor.selected_multiline_submit(),
            Some(CommandInputResult::MultilineAsk(
                "line one\nline two".to_string()
            ))
        );
    }

    #[test]
    fn multiline_history_preview_uses_multiline_prefix_for_body_lines() {
        let history = vec![CommandHistoryItem::multiline_ask("line one\nline two")];
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();
        let lines = editor.render_lines();

        assert_eq!(lines[0].prompt, "main> ");
        assert_eq!(lines[1].prompt, "");
        assert_eq!(lines[2].prompt, DEFAULT_MULTILINE_PREFIX);
        assert_eq!(lines[3].prompt, DEFAULT_MULTILINE_PREFIX);
    }

    #[test]
    fn history_multiline_shell_entry_returns_multiline_shell_submit_signal() {
        let history = vec![CommandHistoryItem::multiline_shell("echo \\\n  ok")];
        let mut editor = CommandEditor::new(config(&history));

        let selected = editor.history_previous();

        assert_eq!(selected, None);
        assert_eq!(editor.current_text(), "/shell\necho \\\n  ok");
        assert_eq!(
            editor.selected_multiline_submit(),
            Some(CommandInputResult::MultilineShell(
                "echo \\\n  ok".to_string()
            ))
        );
    }

    #[test]
    fn history_navigation_can_leave_multiline_history_entry() {
        let history = command_history(&["first", "echo \\\n \"test\"", "third"]);
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();
        assert_eq!(editor.current_text(), "third");

        editor.history_previous();
        assert_eq!(editor.current_text(), "echo \\\n \"test\"");
        assert_eq!(editor.buffer.lines_len(), 2);

        editor.buffer.move_up();
        editor.history_previous();
        assert_eq!(editor.current_text(), "first");
        assert_eq!(editor.buffer.row(), 0);

        editor.history_next();
        assert_eq!(editor.current_text(), "echo \\\n \"test\"");
        assert_eq!(editor.buffer.row(), 1);

        editor.history_next();
        assert_eq!(editor.current_text(), "third");
    }

    #[test]
    fn history_browsing_requires_enter_before_multiline_cursor_navigation() {
        let history = command_history(&["echo \\\n \"test\""]);
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();
        assert_eq!(editor.buffer.row(), 1);

        assert!(editor.buffer.move_up());
        editor.history_previous();

        assert_eq!(editor.current_text(), "echo \\\n \"test\"");
        assert_eq!(editor.buffer.row(), 1);
    }

    #[test]
    fn enter_accepts_history_browsing_before_up_moves_cursor() {
        let history = command_history(&["echo \\\n \"test\""]);
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();
        assert!(editor.history.is_browsing());

        editor.accept_history_browsing();
        assert!(!editor.history.is_browsing());
        assert_eq!(editor.current_text(), "echo \\\n \"test\"");

        assert!(editor.buffer.move_up());
        assert_eq!(editor.buffer.row(), 0);
        assert!(editor.history.index().is_none());
    }

    #[test]
    fn history_browsing_render_lines_are_italic_until_editing_is_accepted() {
        let history = command_history(&["echo \\\n \"test\""]);
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();
        let lines = editor.render_lines();
        assert!(lines[0].text.contains("\x1b[3m"));
        assert!(lines[1].text.contains("\x1b[3m"));

        editor.accept_history_browsing();
        let lines = editor.render_lines();
        assert!(!lines[0].text.contains("\x1b[3m"));
        assert!(!lines[1].text.contains("\x1b[3m"));
    }

    #[test]
    fn history_is_disabled_for_multiline_buffer() {
        let history = command_history(&["history"]);
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("echo \\\n");

        editor.history_previous();

        assert_eq!(editor.current_text(), "echo \\\n");
        assert!(editor.history.index().is_none());
    }

    #[test]
    fn repeated_completion_cycles_through_candidates_on_current_line() {
        let history = Vec::new();
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("open sr");
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "sr".to_string(),
                start: "open ".chars().count(),
                is_command: false,
            },
            completions: vec![
                Completion {
                    replacement: "src/".to_string(),
                    display: "src/".to_string(),
                },
                Completion {
                    replacement: "scripts/".to_string(),
                    display: "scripts/".to_string(),
                },
            ],
            selected: None,
        });

        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "open src/");
        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "open scripts/");
    }
}
