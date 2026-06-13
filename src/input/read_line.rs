use std::io::{self, IsTerminal, Write};

use crossterm::{
    cursor::{self, MoveDown, MoveTo, MoveToColumn, MoveUp},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType},
};

use super::{
    colorize_tag,
    completion::{CompletionState, completion_state, token_before_cursor},
    editor_render::{
        EditorLine, RenderLayout, render_editor_lines, render_layout_for_lines_with_cursor_wrap,
        wrapped_rows,
    },
    is_alt_key, is_command_key, is_key_press, is_plain_text_key,
    line_buffer::LineBuffer,
    raw_mode::RawModeGuard,
    text_length,
};
use crate::commands::slash_commands;

#[cfg(test)]
use super::completion::{Completion, CompletionToken};

struct LineEditor<'a> {
    prompt: &'a str,
    line: LineBuffer,
    history: &'a [String],
    history_index: Option<usize>,
    draft: String,
    completion: Option<CompletionState>,
    rendered_rows: u16,
    rendered_cursor_row: u16,
}

pub fn read_line_with_history(prompt: &str, history: &[String]) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() {
        return read_stdin_line();
    }

    let _raw_mode = RawModeGuard::enable()?;
    let mut editor = LineEditor::new(prompt, history);
    editor.run()
}

pub fn read_masked_line(prompt: &str) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() {
        return read_stdin_line();
    }

    let _raw_mode = RawModeGuard::enable()?;
    let mut editor = MaskedLineEditor::new(prompt);
    editor.run()
}

struct MaskedLineEditor<'a> {
    prompt: &'a str,
    input: String,
    start_col: u16,
    start_row: u16,
    rendered_rows: u16,
    rendered_cursor_row: u16,
}

impl<'a> MaskedLineEditor<'a> {
    fn new(prompt: &'a str) -> Self {
        Self {
            prompt,
            input: String::new(),
            start_col: 0,
            start_row: 0,
            rendered_rows: 1,
            rendered_cursor_row: 0,
        }
    }

    fn run(&mut self) -> io::Result<Option<String>> {
        let (start_col, start_row) = cursor::position()?;
        self.start_col = start_col;
        self.start_row = start_row;
        print!("{}", self.prompt);
        io::stdout().flush()?;

        loop {
            match event::read()? {
                Event::Key(key) if is_key_press(key) => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if self.input.is_empty() {
                            return Ok(None);
                        }
                    }
                    KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.clear_screen_and_render()?;
                    }
                    KeyCode::Enter => {
                        self.finish_line()?;
                        return Ok(Some(self.input.clone()));
                    }
                    KeyCode::Backspace => {
                        if self.input.pop().is_some() {
                            self.render()?;
                        }
                    }
                    KeyCode::Char(ch) if is_plain_text_key(key) => {
                        self.input.push(ch);
                        self.render()?;
                    }
                    _ => {}
                },
                Event::Paste(text) => {
                    self.input
                        .extend(text.chars().filter(|ch| !matches!(ch, '\r' | '\n')));
                    self.render()?;
                }
                _ => {}
            }
        }
    }

    fn clear_screen_and_render(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        self.start_col = 0;
        self.start_row = 0;
        self.rendered_rows = 1;
        self.rendered_cursor_row = 0;
        stdout.flush()?;
        self.render()
    }

    fn render(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        let layout = masked_render_layout(self.start_col, self.prompt, self.input.chars().count());
        self.make_room_for_rows(layout.rows)?;

        execute!(stdout, MoveTo(self.start_col, self.start_row))?;
        for row in 0..self.rendered_rows {
            execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            if row + 1 < self.rendered_rows {
                execute!(stdout, MoveDown(1))?;
            }
        }

        if self.rendered_rows > 1 {
            execute!(stdout, MoveUp(self.rendered_rows - 1))?;
        }

        execute!(stdout, MoveTo(self.start_col, self.start_row))?;
        write!(
            stdout,
            "{}{}",
            self.prompt,
            "*".repeat(self.input.chars().count())
        )?;

        execute!(
            stdout,
            MoveTo(layout.cursor_col, self.start_row + layout.cursor_row)
        )?;

        self.rendered_rows = layout.rows;
        self.rendered_cursor_row = layout.cursor_row;

        stdout.flush()
    }

    fn make_room_for_rows(&mut self, rows: u16) -> io::Result<()> {
        let terminal_rows = terminal_height();
        let required_end_row = self.start_row.saturating_add(rows);
        if required_end_row <= terminal_rows {
            return Ok(());
        }

        let scroll_rows = required_end_row - terminal_rows;
        let mut stdout = io::stdout();
        for _ in 0..scroll_rows {
            write!(stdout, "\r\n")?;
        }
        stdout.flush()?;
        self.start_row = self.start_row.saturating_sub(scroll_rows);
        Ok(())
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
}

fn read_stdin_line() -> io::Result<Option<String>> {
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(None);
    }

    Ok(Some(input.trim_end_matches(['\r', '\n']).to_string()))
}

impl<'a> LineEditor<'a> {
    fn new(prompt: &'a str, history: &'a [String]) -> Self {
        Self {
            prompt,
            line: LineBuffer::new(),
            history,
            history_index: None,
            draft: String::new(),
            completion: None,
            rendered_rows: 1,
            rendered_cursor_row: 0,
        }
    }

    fn run(&mut self) -> io::Result<Option<String>> {
        print!("{}", self.prompt);
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
                if self.line.is_empty() {
                    return Ok(Some(None));
                }
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.clear_screen_and_render()?;
            }
            KeyCode::Enter => {
                self.finish_line()?;
                return Ok(Some(Some(self.current_line())));
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
                self.line.set_cursor(0);
                self.render()?;
            }
            KeyCode::Right if is_command_key(key) => {
                self.clear_completion();
                self.line.set_cursor_to_end();
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
            KeyCode::Tab => {
                self.complete()?;
            }
            KeyCode::Home => {
                self.clear_completion();
                self.line.set_cursor(0);
                self.render()?;
            }
            KeyCode::End => {
                self.clear_completion();
                self.line.set_cursor_to_end();
                self.render()?;
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
        let mut stdout = io::stdout();
        let layout = self.render_layout();
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
        render_layout_for_lines_with_cursor_wrap(
            &self.render_lines(),
            0,
            self.cursor_visible_col(),
            columns,
            self.cursor_wraps_at_boundary(),
        )
    }

    fn render_lines(&self) -> Vec<EditorLine<'_>> {
        let line = self.current_line();
        vec![EditorLine::with_visible_len(
            self.prompt,
            highlighted_input(&line),
            text_length(&line, false),
        )]
    }

    fn cursor_visible_col(&self) -> usize {
        let line = self.current_line();
        text_length(
            &line.chars().take(self.line.cursor()).collect::<String>(),
            false,
        )
    }

    fn cursor_wraps_at_boundary(&self) -> bool {
        let line = self.current_line();
        line.chars()
            .take(self.line.cursor())
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

    fn current_line(&self) -> String {
        self.line.text()
    }

    fn complete(&mut self) -> io::Result<()> {
        self.stop_history_navigation();

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
            && token_before_cursor(&self.current_line(), self.line.cursor())
                .is_some_and(|token| token.value == completion.replacement)
    }

    fn build_completion_state(&self) -> Option<CompletionState> {
        completion_state(&self.current_line(), self.line.cursor())
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

        self.line.replace_before_cursor(token.start, &replacement);
        true
    }

    fn insert_text(&mut self, text: &str) {
        self.clear_completion();
        self.stop_history_navigation();
        self.line.insert_text_without_newlines(text);
    }

    fn insert_char(&mut self, ch: char) {
        self.stop_history_navigation();
        self.line.insert_char(ch);
    }

    fn backspace(&mut self) {
        if self.line.backspace() {
            self.stop_history_navigation();
        }
    }

    fn delete(&mut self) {
        if self.line.delete() {
            self.stop_history_navigation();
        }
    }

    fn move_left(&mut self) {
        self.line.move_left();
    }

    fn move_right(&mut self) {
        self.line.move_right();
    }

    fn move_word_left(&mut self) {
        self.line.move_word_left();
    }

    fn move_word_right(&mut self) {
        self.line.move_word_right();
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }

        let next_index = match self.history_index {
            Some(0) => 0,
            Some(index) => index - 1,
            None => {
                self.draft = self.current_line();
                self.history.len() - 1
            }
        };

        self.set_history_index(next_index);
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };

        if index + 1 < self.history.len() {
            self.set_history_index(index + 1);
        } else {
            self.history_index = None;
            self.line.replace_with_text(&self.draft);
        }
    }

    fn set_history_index(&mut self, index: usize) {
        self.history_index = Some(index);
        self.line.replace_with_text(&self.history[index]);
    }

    fn stop_history_navigation(&mut self) {
        self.history_index = None;
        self.draft.clear();
    }

    fn clear_completion(&mut self) {
        self.completion = None;
    }
}

fn masked_render_layout(start_col: u16, prompt: &str, input_len: usize) -> RenderLayout {
    let columns = terminal::size()
        .ok()
        .map(|(columns, _)| columns.max(1) as usize)
        .unwrap_or(80);
    masked_render_layout_for_columns(start_col, prompt, input_len, columns)
}

fn masked_render_layout_for_columns(
    start_col: u16,
    prompt: &str,
    input_len: usize,
    columns: usize,
) -> RenderLayout {
    let visible_len = text_length(prompt, false) + input_len;
    let total_len = start_col as usize + visible_len;
    let rows = wrapped_rows(total_len, columns);

    RenderLayout {
        rows: rows as u16,
        cursor_row: (total_len / columns) as u16,
        cursor_col: (total_len % columns) as u16,
        cursor_wraps_at_boundary: false,
    }
}

fn terminal_height() -> u16 {
    terminal::size()
        .ok()
        .map(|(_, rows)| rows.max(1))
        .unwrap_or(24)
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

    #[test]
    fn history_previous_selects_latest_command() {
        let history = vec!["first".to_string(), "second".to_string()];
        let mut editor = LineEditor::new("> ", &history);

        editor.history_previous();

        assert_eq!(editor.current_line(), "second");
        assert_eq!(editor.line.cursor(), "second".chars().count());
    }

    #[test]
    fn history_navigation_restores_draft_after_last_entry() {
        let history = vec!["first".to_string()];
        let mut editor = LineEditor::new("> ", &history);
        editor.insert_text("draft");

        editor.history_previous();
        editor.history_next();

        assert_eq!(editor.current_line(), "draft");
    }

    #[test]
    fn editing_history_entry_stops_history_navigation() {
        let history = vec!["first".to_string()];
        let mut editor = LineEditor::new("> ", &history);

        editor.history_previous();
        editor.insert_char('!');
        editor.history_next();

        assert_eq!(editor.current_line(), "first!");
    }

    #[test]
    fn repeated_completion_cycles_through_candidates() {
        let history = Vec::new();
        let mut editor = LineEditor::new("> ", &history);
        editor.insert_text("ex");
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "ex".to_string(),
                start: 0,
                is_command: true,
            },
            completions: vec![
                Completion {
                    replacement: "exit".to_string(),
                    display: "exit".to_string(),
                },
                Completion {
                    replacement: "explain".to_string(),
                    display: "explain".to_string(),
                },
            ],
            selected: None,
        });

        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "exit");
        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "explain");
        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "exit");
    }

    #[test]
    fn repeated_path_completion_replaces_same_token() {
        let history = Vec::new();
        let mut editor = LineEditor::new("> ", &history);
        editor.insert_text("vim src/");
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "src/".to_string(),
                start: 4,
                is_command: false,
            },
            completions: vec![
                Completion {
                    replacement: "src/agent/".to_string(),
                    display: "agent/".to_string(),
                },
                Completion {
                    replacement: "src/input/".to_string(),
                    display: "input/".to_string(),
                },
            ],
            selected: None,
        });

        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "vim src/agent/");
        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "vim src/input/");
    }

    #[test]
    fn completed_single_directory_match_restarts_completion_session() {
        let history = Vec::new();
        let mut editor = LineEditor::new("> ", &history);
        editor.insert_text("sr");
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

    #[test]
    fn wrapped_rows_uses_pending_terminal_wrap() {
        assert_eq!(wrapped_rows(0, 10), 1);
        assert_eq!(wrapped_rows(9, 10), 1);
        assert_eq!(wrapped_rows(10, 10), 1);
        assert_eq!(wrapped_rows(21, 10), 3);
    }

    #[test]
    fn masked_render_layout_wraps_long_secret() {
        let layout = masked_render_layout_for_columns(0, "Openrouter API key: ", 25, 20);

        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cursor_row, 2);
        assert_eq!(layout.cursor_col, 5);
    }

    #[test]
    fn masked_render_layout_accounts_for_start_column() {
        let layout = masked_render_layout_for_columns(5, "key: ", 10, 12);

        assert_eq!(layout.rows, 2);
        assert_eq!(layout.cursor_row, 1);
        assert_eq!(layout.cursor_col, 8);
    }

    #[test]
    fn highlights_exact_slash_command() {
        assert_eq!(highlighted_input("/config"), "\x1b[96m/config\x1b[0m");
    }

    #[test]
    fn highlights_slash_command_with_trailing_input() {
        assert_eq!(
            highlighted_input("/ask explain this"),
            "\x1b[96m/ask\x1b[0m explain this"
        );
    }

    #[test]
    fn does_not_highlight_unknown_slash_command() {
        assert_eq!(highlighted_input("/unknown"), "/unknown");
    }

    #[test]
    fn does_not_highlight_slash_command_prefix_inside_word() {
        assert_eq!(highlighted_input("/configure"), "/configure");
    }
}
