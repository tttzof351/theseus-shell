use std::io::{self, BufRead, IsTerminal, Write};

use crossterm::{
    cursor::{MoveDown, MoveRight, MoveToColumn, MoveUp},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType},
};

use super::{
    completion::{CompletionState, path_completion_state, token_before_cursor},
    is_alt_key, is_command_key, is_key_press, is_plain_text_key,
    raw_mode::RawModeGuard,
    text_length,
};

#[cfg(test)]
use super::completion::{Completion, CompletionToken};

#[derive(Debug, Clone)]
pub struct MultiLineConfig {
    pub prefix: String,
    pub exit_word: Option<String>,
}

impl Default for MultiLineConfig {
    fn default() -> Self {
        Self {
            prefix: "> ".to_string(),
            exit_word: None,
        }
    }
}
struct MultiLineEditor {
    config: MultiLineConfig,
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    goal_col: usize,
    completion: Option<CompletionState>,
    rendered_rows: u16,
    rendered_cursor_row: u16,
}

pub fn read_multi_line_input(config: MultiLineConfig) -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return read_piped_input(&config);
    }

    let _raw_mode = RawModeGuard::enable()?;
    let mut editor = MultiLineEditor::new(config);
    editor.run()
}

fn read_piped_input(config: &MultiLineConfig) -> io::Result<String> {
    let stdin = io::stdin();
    let mut lines = Vec::new();

    for line in stdin.lock().lines() {
        let line = line?;
        if config.exit_word.as_deref() == Some(line.as_str()) {
            break;
        }
        lines.push(line);
    }

    Ok(lines.join("\n"))
}

impl MultiLineEditor {
    fn new(config: MultiLineConfig) -> Self {
        Self {
            config,
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            goal_col: 0,
            completion: None,
            rendered_rows: 1,
            rendered_cursor_row: 0,
        }
    }

    fn run(&mut self) -> io::Result<String> {
        print!("{}", self.config.prefix);
        io::stdout().flush()?;

        loop {
            match event::read()? {
                Event::Key(key) if is_key_press(key) => {
                    if let Some(text) = self.handle_key(key)? {
                        return Ok(text);
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

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<Option<String>> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.finish_line()?;
                return Ok(Some(join_lines(&self.lines)));
            }
            KeyCode::Enter => {
                self.clear_completion();
                if self.is_exit_line() {
                    self.finish_line()?;
                    let text = join_lines(&self.lines[..self.lines.len() - 1]);
                    return Ok(Some(text));
                }
                self.split_line();
                self.render()?;
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
                if self.row > 0 {
                    self.clear_completion();
                    self.row -= 1;
                    self.col = self.goal_col.min(self.lines[self.row].len());
                    self.render()?;
                }
            }
            KeyCode::Down => {
                if self.row + 1 < self.lines.len() {
                    self.clear_completion();
                    self.row += 1;
                    self.col = self.goal_col.min(self.lines[self.row].len());
                    self.render()?;
                }
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

        for (index, line) in self.lines.iter().enumerate() {
            write!(
                stdout,
                "{}{}",
                self.config.prefix,
                line.iter().collect::<String>()
            )?;
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

    fn render_layout(&self) -> RenderLayout {
        let columns = terminal::size()
            .ok()
            .map(|(columns, _)| columns.max(1) as usize)
            .unwrap_or(80);
        self.render_layout_for_columns(columns)
    }

    fn render_layout_for_columns(&self, columns: usize) -> RenderLayout {
        let prefix_len = text_length(&self.config.prefix, false);
        let mut rows_before_cursor = 0usize;
        let mut total_rows = 0usize;

        for (index, line) in self.lines.iter().enumerate() {
            let line_len = prefix_len + line.len();
            let line_rows = wrapped_rows(line_len, columns);

            if index < self.row {
                rows_before_cursor += line_rows;
            }
            total_rows += line_rows;
        }

        let cursor_len = prefix_len + self.col;

        RenderLayout {
            rows: total_rows as u16,
            cursor_row: (rows_before_cursor + cursor_len / columns) as u16,
            cursor_col: (cursor_len % columns) as u16,
        }
    }

    fn finish_line(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        write!(stdout, "\r\n")?;
        stdout.flush()
    }

    fn is_exit_line(&self) -> bool {
        let current_line = self.current_line();

        self.config.exit_word.as_deref() == Some(current_line.trim())
            && self.row + 1 == self.lines.len()
    }

    fn current_line(&self) -> String {
        self.lines[self.row].iter().collect()
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
        path_completion_state(&self.current_line(), self.col)
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

    fn clear_completion(&mut self) {
        self.completion = None;
    }

    fn insert_text(&mut self, text: &str) {
        self.clear_completion();
        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => self.split_line(),
                ch => self.insert_char(ch),
            }
        }
    }

    fn insert_char(&mut self, ch: char) {
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
            self.col -= 1;
            self.lines[self.row].remove(self.col);
            self.goal_col = self.col;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.goal_col = self.col;
            self.lines[self.row].extend(current);
        }
    }

    fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_lines_with_newlines() {
        let lines = vec![vec!['a'], vec!['b', 'c']];

        assert_eq!(join_lines(&lines), "a\nbc");
    }

    #[test]
    fn wrapped_rows_includes_terminal_wrap_row() {
        assert_eq!(wrapped_rows(0, 10), 1);
        assert_eq!(wrapped_rows(9, 10), 1);
        assert_eq!(wrapped_rows(10, 10), 2);
        assert_eq!(wrapped_rows(21, 10), 3);
    }

    #[test]
    fn render_layout_counts_wrapped_lines() {
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: None,
        });
        editor.lines = vec![
            "abcdefgh".chars().collect(),
            "ijklmnopqrst".chars().collect(),
        ];
        editor.row = 1;
        editor.col = 4;

        let layout = editor.render_layout_for_columns(10);

        assert_eq!(layout.rows, 4);
        assert_eq!(layout.cursor_row, 2);
        assert_eq!(layout.cursor_col, 6);
    }

    #[test]
    fn exit_line_ignores_surrounding_whitespace() {
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
        });
        editor.lines = vec!["/end ".chars().collect()];

        assert!(editor.is_exit_line());
    }

    #[test]
    fn moves_left_by_word_on_current_line() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.lines = vec!["find biggest file".chars().collect()];
        editor.col = editor.lines[0].len();

        editor.move_word_left();

        assert_eq!(editor.col, "find biggest ".chars().count());
    }

    #[test]
    fn moves_right_by_word_on_current_line() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.lines = vec!["find biggest file".chars().collect()];
        editor.col = 0;

        editor.move_word_right();

        assert_eq!(editor.col, "find ".chars().count());
    }

    #[test]
    fn repeated_completion_cycles_through_candidates() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.lines = vec!["open sr".chars().collect()];
        editor.col = "open sr".chars().count();
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
        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "open src/");
    }

    #[test]
    fn completion_replaces_only_current_line_token() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.lines = vec!["before sr".chars().collect(), "after sr".chars().collect()];
        editor.row = 1;
        editor.col = "after sr".chars().count();
        editor.goal_col = editor.col;
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "sr".to_string(),
                start: "after ".chars().count(),
                is_command: false,
            },
            completions: vec![Completion {
                replacement: "src/".to_string(),
                display: "src/".to_string(),
            }],
            selected: None,
        });

        assert!(editor.advance_completion());

        assert_eq!(join_lines(&editor.lines), "before sr\nafter src/");
        assert_eq!(editor.col, "after src/".chars().count());
        assert_eq!(editor.goal_col, editor.col);
    }

    #[test]
    fn completed_single_directory_match_restarts_completion_session() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.lines = vec!["sr".chars().collect()];
        editor.col = "sr".chars().count();
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "sr".to_string(),
                start: 0,
                is_command: false,
            },
            completions: vec![Completion {
                replacement: "src/".to_string(),
                display: "src/".to_string(),
            }],
            selected: None,
        });

        assert!(editor.advance_completion());

        assert!(editor.should_restart_completed_directory_completion());
    }
}
