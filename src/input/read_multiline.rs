use std::io::{self, BufRead, IsTerminal, Write};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal,
};

use super::{
    completion::{CompletionState, path_completion_state, token_before_cursor},
    editor_render::{EditorLine, RenderLayout, render_editor_lines, render_layout_for_lines},
    is_alt_key, is_command_key, is_key_press, is_plain_text_key,
    raw_mode::RawModeGuard,
    text_buffer::TextBuffer,
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
    buffer: TextBuffer,
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
            buffer: TextBuffer::new(),
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
                return Ok(Some(self.buffer.text()));
            }
            KeyCode::Enter => {
                self.clear_completion();
                if self.is_exit_line() {
                    self.finish_line()?;
                    let text = self.buffer.text_before_last_line();
                    return Ok(Some(text));
                }
                self.buffer.split_line();
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
                if self.buffer.move_up() {
                    self.clear_completion();
                    self.render()?;
                }
            }
            KeyCode::Down => {
                if self.buffer.move_down() {
                    self.clear_completion();
                    self.render()?;
                }
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
                self.buffer.insert_char(ch);
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

    fn render_layout(&self) -> RenderLayout {
        let columns = terminal::size()
            .ok()
            .map(|(columns, _)| columns.max(1) as usize)
            .unwrap_or(80);
        self.render_layout_for_columns(columns)
    }

    fn render_layout_for_columns(&self, columns: usize) -> RenderLayout {
        render_layout_for_lines(
            &self.render_lines(),
            self.buffer.row(),
            self.buffer.col(),
            columns,
        )
    }

    fn render_lines(&self) -> Vec<EditorLine<'_>> {
        self.buffer
            .lines()
            .iter()
            .map(|line| EditorLine::new(&self.config.prefix, line.iter().collect::<String>()))
            .collect()
    }

    fn finish_line(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        write!(stdout, "\r\n")?;
        stdout.flush()
    }

    fn is_exit_line(&self) -> bool {
        let current_line = self.current_line();

        self.config.exit_word.as_deref() == Some(current_line.trim())
            && self.buffer.row() + 1 == self.buffer.lines_len()
    }

    fn current_line(&self) -> String {
        self.buffer.current_line_text()
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
        path_completion_state(&self.current_line(), self.buffer.col())
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

    fn clear_completion(&mut self) {
        self.completion = None;
    }

    fn insert_text(&mut self, text: &str) {
        self.clear_completion();
        self.buffer.insert_text(text);
    }

    fn backspace(&mut self) {
        self.buffer.backspace();
    }

    fn delete(&mut self) {
        self.buffer.delete();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::editor_render::wrapped_rows;

    #[test]
    fn text_buffer_joins_lines_with_newlines() {
        let buffer = TextBuffer::from_text("a\nbc");

        assert_eq!(buffer.text(), "a\nbc");
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
        editor.buffer = TextBuffer::from_text("abcdefgh\nijklmnopqrst");
        editor.buffer.set_position(1, 4);

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
        editor.buffer = TextBuffer::from_text("/end ");

        assert!(editor.is_exit_line());
    }

    #[test]
    fn moves_left_by_word_on_current_line() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("find biggest file");

        editor.move_word_left();

        assert_eq!(editor.buffer.col(), "find biggest ".chars().count());
    }

    #[test]
    fn moves_right_by_word_on_current_line() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("find biggest file");
        editor.buffer.set_position(0, 0);

        editor.move_word_right();

        assert_eq!(editor.buffer.col(), "find ".chars().count());
    }

    #[test]
    fn repeated_completion_cycles_through_candidates() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("open sr");
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
        editor.buffer = TextBuffer::from_text("before sr\nafter sr");
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

        assert_eq!(editor.buffer.text(), "before sr\nafter src/");
        assert_eq!(editor.buffer.col(), "after src/".chars().count());
    }

    #[test]
    fn completed_single_directory_match_restarts_completion_session() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("sr");
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
