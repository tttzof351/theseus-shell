use std::io::{self, IsTerminal, Write};

use crossterm::{
    cursor::{MoveDown, MoveRight, MoveTo, MoveToColumn, MoveUp},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType},
};

use super::{
    colorize_tag,
    completion::{CompletionState, completion_state, path_completion_state, token_before_cursor},
    is_alt_key, is_command_key, is_key_press, is_plain_text_key,
    raw_mode::RawModeGuard,
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
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    goal_col: usize,
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
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            goal_col: 0,
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
                self.col = 0;
                self.goal_col = 0;
                self.render()?;
            }
            KeyCode::Right if is_command_key(key) => {
                self.clear_completion();
                self.col = self.lines[self.row].len();
                self.goal_col = self.col;
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
                self.col = 0;
                self.goal_col = 0;
                self.render()?;
            }
            KeyCode::End => {
                self.clear_completion();
                self.col = self.lines[self.row].len();
                self.goal_col = self.col;
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

        if self.rendered_cursor_row > 0 {
            execute!(stdout, MoveUp(self.rendered_cursor_row))?;
        }

        for row in 0..self.rendered_rows {
            execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            if row + 1 < self.rendered_rows {
                execute!(stdout, MoveDown(1))?;
            }
        }

        if self.rendered_rows > 1 {
            execute!(stdout, MoveUp(self.rendered_rows - 1))?;
        }

        for index in 0..self.lines.len() {
            write!(stdout, "{}", self.prompt_for_row(index))?;
            if index == 0 {
                write!(stdout, "{}", highlighted_input(&self.line_text(index)))?;
            } else {
                write!(stdout, "{}", self.line_text(index))?;
            }
            if index + 1 < self.lines.len() {
                write!(stdout, "\r\n")?;
            }
        }

        let rows_up = layout.rows - 1 - layout.cursor_row;
        if rows_up > 0 {
            execute!(stdout, MoveUp(rows_up))?;
        }

        execute!(stdout, MoveToColumn(0))?;
        if layout.cursor_col > 0 {
            execute!(stdout, MoveRight(layout.cursor_col))?;
        }
        stdout.flush()?;

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
        let mut rows_before_cursor = 0usize;
        let mut total_rows = 0usize;

        for index in 0..self.lines.len() {
            let line_len = text_length(self.prompt_for_row(index), false) + self.lines[index].len();
            let line_rows = wrapped_rows(line_len, columns);

            if index < self.row {
                rows_before_cursor += line_rows;
            }
            total_rows += line_rows;
        }

        let cursor_len = text_length(self.prompt_for_row(self.row), false) + self.col;

        RenderLayout {
            rows: total_rows as u16,
            cursor_row: (rows_before_cursor + cursor_len / columns) as u16,
            cursor_col: (cursor_len % columns) as u16,
        }
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
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn current_text(&self) -> String {
        join_lines(&self.lines)
    }

    fn current_line(&self) -> String {
        self.line_text(self.row)
    }

    fn line_text(&self, row: usize) -> String {
        self.lines[row].iter().collect()
    }

    fn insert_text(&mut self, text: &str) {
        self.clear_completion();
        self.stop_history_navigation();
        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => self.split_line(),
                ch => self.insert_char(ch),
            }
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.stop_history_navigation();
        self.lines[self.row].insert(self.col, ch);
        self.col += 1;
        self.goal_col = self.col;
    }

    fn split_line(&mut self) {
        let right = self.lines[self.row].split_off(self.col);
        self.row += 1;
        self.col = 0;
        self.goal_col = 0;
        self.lines.insert(self.row, right);
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.stop_history_navigation();
            self.col -= 1;
            self.lines[self.row].remove(self.col);
            self.goal_col = self.col;
        } else if self.row > 0 {
            self.stop_history_navigation();
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.goal_col = self.col;
            self.lines[self.row].extend(current);
        }
    }

    fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.stop_history_navigation();
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len() {
            self.stop_history_navigation();
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
        self.goal_col = self.col;
    }

    fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.goal_col = self.col;
    }

    fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
            return;
        }

        while self.col > 0 && self.lines[self.row][self.col - 1].is_whitespace() {
            self.col -= 1;
        }
        while self.col > 0 && !self.lines[self.row][self.col - 1].is_whitespace() {
            self.col -= 1;
        }
        self.goal_col = self.col;
    }

    fn move_word_right(&mut self) {
        if self.col == self.lines[self.row].len() {
            self.move_right();
            return;
        }

        while self.col < self.lines[self.row].len()
            && !self.lines[self.row][self.col].is_whitespace()
        {
            self.col += 1;
        }
        while self.col < self.lines[self.row].len()
            && self.lines[self.row][self.col].is_whitespace()
        {
            self.col += 1;
        }
        self.goal_col = self.col;
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
        self.lines.len() == 1 && self.row == 0
    }

    fn set_history_index(&mut self, index: usize) {
        self.history_index = Some(index);
        self.replace_with_text(&self.config.history[index]);
    }

    fn replace_with_text(&mut self, text: &str) {
        self.lines = text
            .split('\n')
            .map(|line| line.chars().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
        self.goal_col = self.col;
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
            && token_before_cursor(&self.current_line(), self.col)
                .is_some_and(|token| token.value == completion.replacement)
    }

    fn build_completion_state(&self) -> Option<CompletionState> {
        if self.row == 0 {
            completion_state(&self.current_line(), self.col)
        } else {
            path_completion_state(&self.current_line(), self.col)
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
        let replacement_chars = replacement.chars().collect::<Vec<_>>();
        self.lines[self.row].splice(start..self.col, replacement_chars);
        self.col = start + replacement.chars().count();
        self.goal_col = self.col;
    }
}

struct RenderLayout {
    rows: u16,
    cursor_row: u16,
    cursor_col: u16,
}

fn join_lines(lines: &[Vec<char>]) -> String {
    lines
        .iter()
        .map(|line| line.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrapped_rows(visible_len: usize, columns: usize) -> usize {
    visible_len / columns + 1
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
        assert_eq!(editor.lines.len(), 2);
        assert_eq!(editor.row, 1);
        assert_eq!(editor.col, 0);
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
        editor.row = 1;
        editor.col = 2;

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
        assert_eq!(editor.row, 0);
        assert_eq!(editor.col, "second".chars().count());
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
