use std::io::{self, IsTerminal, Write};

use crossterm::{
    cursor::{MoveDown, MoveTo},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType},
};

use super::{
    colorize_tag,
    completion::{CompletionState, completion_state, path_completion_state, token_before_cursor},
    editor_render::{
        EditorLine, RenderLayout, render_editor_lines, render_layout_for_lines_with_cursor_wrap,
    },
    is_alt_key, is_command_key, is_key_press, is_plain_text_key,
    raw_mode::RawModeGuard,
    text_buffer::TextBuffer,
    text_length,
};
use crate::commands::slash_commands;

#[cfg(test)]
use super::completion::{Completion, CompletionToken};

pub struct CommandInputConfig<'a> {
    pub prompt: &'a str,
    pub continuation_prompt: &'a str,
    pub history: &'a [String],
    pub should_continue: fn(&str) -> bool,
}

struct CommandEditor<'a> {
    config: CommandInputConfig<'a>,
    buffer: TextBuffer,
    history_index: Option<usize>,
    draft: String,
    completion: Option<CompletionState>,
    rendered_rows: u16,
    rendered_cursor_row: u16,
}

pub fn read_command_input(config: CommandInputConfig<'_>) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() {
        return read_stdin_line();
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
        Self {
            config,
            buffer: TextBuffer::new(),
            history_index: None,
            draft: String::new(),
            completion: None,
            rendered_rows: 1,
            rendered_cursor_row: 0,
        }
    }

    fn run(&mut self) -> io::Result<Option<String>> {
        print!("{}", self.config.prompt);
        io::stdout().flush()?;

        loop {
            match event::read()? {
                Event::Key(key) if is_key_press(key) => {
                    if let Some(line) = self.handle_key(key)? {
                        return Ok(line);
                    }
                }
                Event::Paste(text) => {
                    self.insert_text(&text);
                    self.render()?;
                }
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<Option<Option<String>>> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
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
                if self.enter_should_continue() {
                    self.split_line();
                    self.render()?;
                } else {
                    self.finish_line()?;
                    return Ok(Some(Some(self.current_text())));
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
                self.buffer.set_col(0);
                self.render()?;
            }
            KeyCode::Right if is_command_key(key) => {
                self.clear_completion();
                self.buffer.set_col_to_line_end();
                self.render()?;
            }
            KeyCode::Char('b') if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_left();
                self.render()?;
            }
            KeyCode::Char('f') if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_right();
                self.render()?;
            }
            KeyCode::Left if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_left();
                self.render()?;
            }
            KeyCode::Right if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_right();
                self.render()?;
            }
            KeyCode::Left => {
                self.clear_completion();
                self.move_left();
                self.render()?;
            }
            KeyCode::Right => {
                self.clear_completion();
                self.move_right();
                self.render()?;
            }
            KeyCode::Up => {
                self.clear_completion();
                self.history_previous();
                self.render()?;
            }
            KeyCode::Down => {
                self.clear_completion();
                self.history_next();
                self.render()?;
            }
            KeyCode::Home => {
                self.clear_completion();
                self.buffer.set_col(0);
                self.render()?;
            }
            KeyCode::End => {
                self.clear_completion();
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

    fn render(&mut self) -> io::Result<()> {
        let layout = self.render_layout();
        let mut stdout = io::stdout();
        let lines = self.render_lines();
        render_editor_lines(
            &mut stdout,
            &lines,
            layout,
            self.rendered_rows,
            self.rendered_cursor_row,
        )?;

        self.rendered_rows = layout.rows;
        self.rendered_cursor_row = layout.cursor_row;
        Ok(())
    }

    fn clear_screen_and_render(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        self.rendered_rows = 1;
        self.rendered_cursor_row = 0;
        stdout.flush()?;
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
        (0..self.buffer.lines_len())
            .map(|index| {
                let line = self.line_text(index);
                let rendered_line = if index == 0 {
                    highlighted_input(&line)
                } else {
                    line.clone()
                };
                EditorLine::with_visible_len(
                    self.prompt_for_row(index),
                    rendered_line,
                    text_length(&line, false),
                )
            })
            .collect()
    }

    fn cursor_visible_col(&self) -> usize {
        let line = self.current_line();
        text_length(
            &line.chars().take(self.buffer.col()).collect::<String>(),
            false,
        )
    }

    fn cursor_wraps_at_boundary(&self) -> bool {
        let line = self.current_line();
        line.chars()
            .take(self.buffer.col())
            .last()
            .is_some_and(|ch| text_length(&ch.to_string(), false) > 1)
    }

    fn finish_line(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        let rows_below_cursor = self.rendered_rows - 1 - self.rendered_cursor_row;
        if rows_below_cursor > 0 {
            execute!(stdout, MoveDown(rows_below_cursor))?;
        }
        write!(stdout, "\r\n")?;
        stdout.flush()
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
            self.stop_history_navigation();
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

    fn insert_text(&mut self, text: &str) {
        self.clear_completion();
        self.stop_history_navigation();
        self.buffer.insert_text(text);
    }

    fn insert_char(&mut self, ch: char) {
        self.stop_history_navigation();
        self.buffer.insert_char(ch);
    }

    fn split_line(&mut self) {
        self.buffer.split_line();
    }

    fn backspace(&mut self) {
        if self.buffer.backspace() {
            self.stop_history_navigation();
        }
    }

    fn delete(&mut self) {
        if self.buffer.delete() {
            self.stop_history_navigation();
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

    fn history_previous(&mut self) {
        if !self.can_navigate_history() || self.config.history.is_empty() {
            return;
        }

        let next_index = match self.history_index {
            Some(0) => 0,
            Some(index) => index - 1,
            None => {
                self.draft = self.current_text();
                self.config.history.len() - 1
            }
        };

        self.set_history_index(next_index);
    }

    fn history_next(&mut self) {
        if !self.can_navigate_history() {
            return;
        }

        let Some(index) = self.history_index else {
            return;
        };

        if index + 1 < self.config.history.len() {
            self.set_history_index(index + 1);
        } else {
            self.history_index = None;
            let draft = self.draft.clone();
            self.replace_with_text(&draft);
        }
    }

    fn can_navigate_history(&self) -> bool {
        self.history_index.is_some() || (self.buffer.lines_len() == 1 && self.buffer.row() == 0)
    }

    fn set_history_index(&mut self, index: usize) {
        self.history_index = Some(index);
        self.replace_with_text(&self.config.history[index]);
    }

    fn replace_with_text(&mut self, text: &str) {
        self.buffer.replace_with_text(text);
    }

    fn stop_history_navigation(&mut self) {
        self.history_index = None;
        self.draft.clear();
    }

    fn clear_completion(&mut self) {
        self.completion = None;
    }

    fn complete(&mut self) -> io::Result<()> {
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

    fn never_continue(_: &str) -> bool {
        false
    }

    fn trailing_backslash(text: &str) -> bool {
        text.ends_with('\\')
    }

    fn config<'a>(history: &'a [String]) -> CommandInputConfig<'a> {
        CommandInputConfig {
            prompt: "main> ",
            continuation_prompt: "> ",
            history,
            should_continue: never_continue,
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
    fn history_replaces_single_line_buffer() {
        let history = vec!["first".to_string(), "second".to_string()];
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();

        assert_eq!(editor.current_text(), "second");
        assert_eq!(editor.buffer.row(), 0);
        assert_eq!(editor.buffer.col(), "second".chars().count());
    }

    #[test]
    fn history_navigation_can_leave_multiline_history_entry() {
        let history = vec![
            "first".to_string(),
            "echo \\\n \"test\"".to_string(),
            "third".to_string(),
        ];
        let mut editor = CommandEditor::new(config(&history));

        editor.history_previous();
        assert_eq!(editor.current_text(), "third");

        editor.history_previous();
        assert_eq!(editor.current_text(), "echo \\\n \"test\"");
        assert_eq!(editor.buffer.lines_len(), 2);

        editor.history_previous();
        assert_eq!(editor.current_text(), "first");

        editor.history_next();
        assert_eq!(editor.current_text(), "echo \\\n \"test\"");

        editor.history_next();
        assert_eq!(editor.current_text(), "third");
    }

    #[test]
    fn history_is_disabled_for_multiline_buffer() {
        let history = vec!["history".to_string()];
        let mut editor = CommandEditor::new(config(&history));
        editor.insert_text("echo \\\n");

        editor.history_previous();

        assert_eq!(editor.current_text(), "echo \\\n");
        assert!(editor.history_index.is_none());
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
